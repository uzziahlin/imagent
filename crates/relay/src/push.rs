//! Web Push 服务端（协议 v1.10）：VAPID 密钥管理 + 订阅表持久化 + 三个
//! Bearer sessionToken 认证 HTTP 端点 + `push_notify` WS 管理帧处理与投递。
//!
//! 架构位置（共享设计 /tmp/contract_v110_push.md）：网关（事件生产者，埋点
//! 在 crates/web platform 层）→ `push_notify` WS 帧 → 本模块（聚合 + 投递）
//! → 系统 Web Push 服务（浏览器/OS 层）。**载荷只含元数据**（kind/title/
//! convId/machine，无消息内容）——维持中继盲化：即使本层读到了推送载荷，
//! 也只有「某机器有事件」这一信息。
//!
//! 路由语义（任务定案的简化且正确形态）：订阅按 session 绑定，中继不知
//! conv↔session 归属——**广播给全部有订阅的 session**（单用户多设备场景，
//! 全员可推）；「SPA 在线免打扰」按 session 维度判定：该 session_id 当前有
//! 活跃已认证 WS 连接（[`SessionStore::is_online`]）则跳过其全部订阅。
//!
//! 组成：
//! - [`VapidKeys`]：env `IMAGENT_PUSH_VAPID_PUBLIC/PRIVATE`（base64url 原始
//!   65B/32B 形态）优先；缺失则生成一次落 `<state_dir>/vapid.json`（0600）
//!   并 info 打公钥。私钥是唯一权威（公钥可从私钥派生——env/file 都只存
//!   私钥面，env 公钥仅作一致性交叉检查）。
//! - [`PushSubStore`]：`<state_dir>/push_subscriptions.json`（0600 原子写，
//!   行形状 `{session_id, endpoint, p256dh, auth, created}`——共享契约逐字；
//!   外层套 `{"v":1}` 版本壳与 sessions.json/credentials.json 同款演进纪律）。
//!   上限 32，LRU 淘汰（created 即「最近订阅时刻」，重复 subscribe 幂等
//!   覆盖并摸新——LRU 序 = created 升序）。
//! - [`PushState::handle_notify`]：帧校验 → 组装 ≤240B payload → 对无活跃
//!   WS 的 session 的订阅逐个 spawn 投递（量小不并发限流）；endpoint 回
//!   404/410 → 删订阅；加密失败（订阅密钥永久无效）同 410 处理；其余失败
//!   warn 保留（临时性——push 服务抖动不该静默掉订阅）。
//!
//! 依赖纪律（musl 交叉）：`web-push` crate 以 `default-features = false`
//! 引入——只取 VAPID 签名与 aes128gcm 加密构建器，**不用**其自带 HTTP 客户
//! 端（isahc→libcurl C 依赖 / hyper-tls→openssl native-tls，均不利 musl 静
//! 态交叉）；HTTP 投递经 [`ReqwestDeliverer`] 用工作区 reqwest（rustls）。
//! web-push 内部链 ece→openssl 与 webauthn-rs 同源，统一走 workspace
//! vendored（源码静态编译），无新增 C 依赖类别。测试经 [`WebPushDeliver`]
//! trait 注入 mock 投递器（单测不真发）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::future::BoxFuture;
use web_push::request_builder;
use web_push::{ContentEncoding, SubscriptionInfo, VapidSignatureBuilder, WebPushMessageBuilder};

use crate::sessions::{unix_now, write_atomic, SessionStore};
use crate::webauthn::{auth_session, b64url_encode, err_json};
use crate::{RelayState, TARGET};

/// 订阅表上限（共享契约）：LRU 淘汰（见 [`PushSubStore::subscribe`]）。
pub(crate) const MAX_SUBSCRIPTIONS: usize = 32;
/// 订阅表文件名（落在 state_dir 下）。
const SUBSCRIPTIONS_FILE: &str = "push_subscriptions.json";
/// VAPID 密钥文件名（落在 state_dir 下）。
const VAPID_FILE: &str = "vapid.json";
/// Web Push 加密载荷字节上限（共享契约 ≤240B——含 kind/title/convId/machine
/// 四字段的 JSON；aes128gcm 外层还有 ~113B 信封，浏览器端 4096B 上限充裕）。
const PAYLOAD_MAX_BYTES: usize = 240;
/// push_notify 帧 title 的 gateway 侧上限是 32 码点；本层 payload 超限时
/// 对 title 防御性按码点收缩（title 是唯一自由文本字段）。
const TITLE_HARD_MAX_CODEPOINTS: usize = 32;
/// 投递 HTTP 超时：push 服务（fcm/autopush/apns）正常亚秒级，15s 兜底防
/// 半开连接挂死投递任务。
const DELIVER_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// push_notify 管理帧（网关 → 中继，明文——内容无敏感信息）
// ---------------------------------------------------------------------------

/// 帧的 kind（共享契约三值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotifyKind {
    /// 回合终态（card terminal done）。
    RoundDone,
    /// 回合终态（card terminal error）。
    RoundError,
    /// 审批请求（approval_request 出站）。
    Approval,
}

impl NotifyKind {
    fn as_str(&self) -> &'static str {
        match self {
            NotifyKind::RoundDone => "round_done",
            NotifyKind::RoundError => "round_error",
            NotifyKind::Approval => "approval",
        }
    }
}

/// push_notify 帧载荷（校验后的运行时形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PushNotify {
    pub kind: NotifyKind,
    pub machine: String,
    pub conv_id: String,
    pub title: String,
}

/// 解析 push_notify 管理帧：`{"v":1,"type":"push_notify","machine":…,
/// "kind":"round_done|round_error|approval","convId":…,"title":…}`。字段
/// 缺失/非字符串、kind 越界、type 不符、畸形 JSON → None（调用方静默丢弃
/// ——管理帧族同纪律，不让畸形帧漏进业务路由）。
pub(crate) fn parse_notify_frame(text: &str) -> Option<PushNotify> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("type")?.as_str()? != "push_notify" {
        return None;
    }
    let kind = match v.get("kind")?.as_str()? {
        "round_done" => NotifyKind::RoundDone,
        "round_error" => NotifyKind::RoundError,
        "approval" => NotifyKind::Approval,
        _ => return None,
    };
    Some(PushNotify {
        kind,
        machine: v.get("machine")?.as_str()?.to_string(),
        conv_id: v.get("convId")?.as_str()?.to_string(),
        title: v.get("title")?.as_str()?.to_string(),
    })
}

/// 加密载荷的 JSON 形状（serde 字段序稳定，字节级测试钉住）。
#[derive(serde::Serialize)]
struct PushPayload<'a> {
    kind: &'a str,
    title: &'a str,
    #[serde(rename = "convId")]
    conv_id: &'a str,
    machine: &'a str,
}

/// 组装加密 payload JSON（≤[`PAYLOAD_MAX_BYTES`]）：title 超限按码点收缩
///（每轮去尾 1 码点重序列化——正常网关已截 32 码点，此循环空转零次；
/// convId/machine 是系统字段有界，收缩到空 title 仍超限时按原样返回——
/// 浏览器端 4096B 上限内，仅契约纪律性超限）。
fn payload_json(notify: &PushNotify) -> String {
    let title_chars: Vec<char> = notify
        .title
        .chars()
        .take(TITLE_HARD_MAX_CODEPOINTS)
        .collect();
    let mut end = title_chars.len();
    loop {
        let title: String = title_chars[..end].iter().collect();
        let json = serde_json::to_string(&PushPayload {
            kind: notify.kind.as_str(),
            title: &title,
            conv_id: &notify.conv_id,
            machine: &notify.machine,
        })
        .unwrap_or_default();
        if json.len() <= PAYLOAD_MAX_BYTES || end == 0 {
            return json;
        }
        end -= 1;
    }
}

// ---------------------------------------------------------------------------
// VAPID 密钥管理（env 优先 → vapid.json → 生成落盘）
// ---------------------------------------------------------------------------

/// VAPID ES256 密钥对（base64url 形态持有；公钥从私钥派生——唯一权威）。
#[derive(Clone)]
pub(crate) struct VapidKeys {
    /// 私钥原始 32B 的 base64url（无 padding）——签名构建入参。
    private_b64: String,
    /// 公钥未压缩点 65B 的 base64url——SPA `pushManager.subscribe` 的
    /// applicationServerKey 入参（`/push/status` 下发）。
    public_b64: String,
}

impl VapidKeys {
    /// 启动加载（读 env；[`Self::load_at`] 为时钟/env 注入的测试形态）。
    pub(crate) fn load(state_dir: &Path) -> Self {
        let env_pub = std::env::var("IMAGENT_PUSH_VAPID_PUBLIC").ok();
        let env_priv = std::env::var("IMAGENT_PUSH_VAPID_PRIVATE").ok();
        Self::load_at(state_dir, env_pub, env_priv)
    }

    /// 加载次序（任务定案 env 优先）：
    /// 1. env 私钥（base64url 32B）合法 → 直接用（不读不写文件）；env 公钥
    ///    同时给出且与派生值不一致 → warn（以私钥派生为准）；
    /// 2. env 缺失/非法 → `vapid.json`（私钥面，启动时已验证过合法性）；
    /// 3. 文件缺失/损坏 → 生成一次落盘（0600）+ info 打公钥。
    fn load_at(state_dir: &Path, env_public: Option<String>, env_private: Option<String>) -> Self {
        let nonempty =
            |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        if let Some(priv_key) = nonempty(env_private) {
            match Self::from_private(&priv_key) {
                Ok(keys) => {
                    if let Some(pub_key) = nonempty(env_public) {
                        if pub_key != keys.public_b64 {
                            tracing::warn!(
                                target: TARGET,
                                "IMAGENT_PUSH_VAPID_PUBLIC 与私钥派生公钥不一致（以私钥派生为准；\
                                 SPA applicationServerKey 须用 status 端点下发的公钥，否则订阅验签必失败）"
                            );
                        }
                    }
                    tracing::info!(
                        target: TARGET,
                        public = %keys.public_b64,
                        "VAPID 密钥来自 env（IMAGENT_PUSH_VAPID_PRIVATE）"
                    );
                    return keys;
                }
                Err(()) => {
                    tracing::warn!(
                        target: TARGET,
                        "IMAGENT_PUSH_VAPID_PRIVATE 非法（须为 base64url 原始 32B 私钥），\
                         回退 vapid.json / 重新生成"
                    );
                }
            }
        }
        let path = state_dir.join(VAPID_FILE);
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(file) = serde_json::from_str::<VapidFile>(&text) {
                if file.v == 1 {
                    if let Ok(keys) = Self::from_private(&file.private_key) {
                        tracing::info!(
                            target: TARGET,
                            path = %path.display(),
                            public = %keys.public_b64,
                            "VAPID 密钥加载（vapid.json 文件权威）"
                        );
                        return keys;
                    }
                }
            }
            tracing::warn!(
                target: TARGET,
                path = %path.display(),
                "vapid.json 损坏：重新生成（旧公钥订阅的浏览器需在 SPA 重新开关通知）"
            );
        }
        // 生成一次落盘：info 日志打公钥（SPA 配置/排查锚点）。
        let keys = Self::generate();
        let json = serde_json::to_string(&VapidFile {
            v: 1,
            private_key: keys.private_b64.clone(),
        })
        .unwrap_or_default();
        if let Err(e) = write_atomic(&path, &json) {
            // 写盘失败不阻断启动（内存态可用，重启后重新生成——订阅需重开）。
            tracing::warn!(
                target: TARGET,
                path = %path.display(),
                error = %e,
                "vapid.json 落盘失败（本次运行可用，重启后公钥将变化）"
            );
        } else {
            tracing::info!(
                target: TARGET,
                path = %path.display(),
                public = %keys.public_b64,
                "VAPID 密钥已生成并落盘（0600）"
            );
        }
        keys
    }

    /// 从 base64url 私钥构建（解析失败 = 非法 32B 私钥）。
    fn from_private(private_b64: &str) -> Result<Self, ()> {
        let partial = VapidSignatureBuilder::from_base64_no_sub(private_b64).map_err(|_| ())?;
        let public_b64 = b64url_encode(&partial.get_public_key());
        Ok(Self {
            private_b64: private_b64.to_string(),
            public_b64,
        })
    }

    /// 生成新密钥（32B 随机 → base64url 私钥；公钥派生）。
    fn generate() -> Self {
        use rand::RngCore;
        let mut buf = [0u8; 32];
        rand::rng().fill_bytes(&mut buf);
        Self::from_private(&b64url_encode(&buf)).expect("随机 32B 必然是合法 P-256 私钥")
    }
}

/// vapid.json 落盘形状（只存私钥面；公钥派生——单一事实源）。
#[derive(serde::Serialize, serde::Deserialize)]
struct VapidFile {
    v: u32,
    #[serde(rename = "privateKey")]
    private_key: String,
}

// ---------------------------------------------------------------------------
// 订阅表（<state_dir>/push_subscriptions.json，0600 原子写，32 LRU）
// ---------------------------------------------------------------------------

/// 单条订阅（落盘形状与共享契约逐字：`{session_id, endpoint, p256dh, auth,
/// created}`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SubscriptionRow {
    /// 订阅发起时的会话 id（在线免打扰判定键；重复 subscribe 幂等覆盖可换绑）。
    pub(crate) session_id: String,
    /// push 服务端点 URL（browser `pushSubscription.endpoint`）。
    pub(crate) endpoint: String,
    /// ECDH 公钥（base64url，浏览器 `keys.p256dh`）。
    pub(crate) p256dh: String,
    /// 认证秘密（base64url，浏览器 `keys.auth`）。
    pub(crate) auth: String,
    /// 订阅时刻（unix 秒）：LRU 序键——重复 subscribe 幂等覆盖时刷新。
    pub(crate) created: u64,
}

/// 落盘文件形状（`{"v":1,"subscriptions":[…]}`——版本壳对齐 sessions.json /
/// credentials.json 演进纪律；行形状即契约逐字）。
#[derive(serde::Serialize, serde::Deserialize)]
struct SubscriptionsFile {
    v: u32,
    subscriptions: Vec<SubscriptionRow>,
}

/// 订阅表（进程级单例，Arc 共享）。锁纪律对齐 SessionStore：std Mutex 纯
/// 内存临界区，「锁内计算 → 锁外写盘」，绝不跨 await 持锁。
pub(crate) struct PushSubStore {
    /// created 升序（LRU 序：头部最旧）。
    inner: Mutex<Vec<SubscriptionRow>>,
    path: PathBuf,
}

impl PushSubStore {
    /// 启动加载：文件缺失 = 空表；解析失败 warn 后按空表（订阅全失效只是
    /// 用户重开通知开关，不放大权限面，对齐 sessions.json 取舍）。
    fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(SUBSCRIPTIONS_FILE);
        let mut rows = Vec::new();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<SubscriptionsFile>(&text) {
                Ok(f) if f.v == 1 => {
                    rows = f.subscriptions;
                    tracing::info!(
                        target: TARGET,
                        subscriptions = rows.len(),
                        "订阅表加载（push_subscriptions.json）"
                    );
                }
                _ => tracing::warn!(
                    target: TARGET,
                    path = %path.display(),
                    "push_subscriptions.json 解析失败：按空表启动（SPA 重开通知开关即可恢复）"
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                target: TARGET,
                path = %path.display(),
                error = %e,
                "push_subscriptions.json 读取失败：按空表启动"
            ),
        }
        rows.sort_by_key(|r| r.created);
        Self {
            inner: Mutex::new(rows),
            path,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<SubscriptionRow>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 订阅（POST /push/subscribe）：endpoint 为幂等键——已存在则覆盖
    /// keys/session 并刷新 created（LRU 摸新，换浏览器/换会话重订阅同端点
    /// 的形态）；新增后超 [`MAX_SUBSCRIPTIONS`] 则从最旧开始淘汰（单用户
    /// 多设备常态 2~5 条，32 留足余量）。值完全未变 → no-op 不落盘。
    pub(crate) fn subscribe(
        &self,
        session_id: &str,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        now: u64,
    ) {
        let mut changed = false;
        {
            let mut rows = self.lock();
            if let Some(row) = rows.iter_mut().find(|r| r.endpoint == endpoint) {
                if row.session_id != session_id
                    || row.p256dh != p256dh
                    || row.auth != auth
                    || row.created != now
                {
                    *row = SubscriptionRow {
                        session_id: session_id.to_string(),
                        endpoint: endpoint.to_string(),
                        p256dh: p256dh.to_string(),
                        auth: auth.to_string(),
                        created: now,
                    };
                    changed = true;
                }
            } else {
                rows.push(SubscriptionRow {
                    session_id: session_id.to_string(),
                    endpoint: endpoint.to_string(),
                    p256dh: p256dh.to_string(),
                    auth: auth.to_string(),
                    created: now,
                });
                rows.sort_by_key(|r| r.created);
                while rows.len() > MAX_SUBSCRIPTIONS {
                    let evicted = rows.remove(0);
                    tracing::info!(
                        target: TARGET,
                        host = %endpoint_host(&evicted.endpoint),
                        "订阅表满，LRU 淘汰最旧订阅（该设备重开通知开关即可恢复）"
                    );
                }
                changed = true;
            }
        }
        if changed {
            self.persist();
        }
    }

    /// 退订 / 投递失败清理（endpoint 精确删；未知 → false no-op）。
    pub(crate) fn remove(&self, endpoint: &str) -> bool {
        let removed = {
            let mut rows = self.lock();
            let before = rows.len();
            rows.retain(|r| r.endpoint != endpoint);
            rows.len() != before
        };
        if removed {
            self.persist();
        }
        removed
    }

    /// 订阅总数（/push/status）。
    pub(crate) fn count(&self) -> usize {
        self.lock().len()
    }

    /// 投递目标快照：过滤掉 session 当前在线（活跃 WS）的订阅（SPA 在线
    /// 免打扰——判定经 `online` 闭包注入，避免存储间锁耦合）。**单次锁**
    /// 同时返回 (表总数, 离线目标行克隆)——调用方的「跳过数 = 总数 - 目标数」
    /// 不跨锁做减法（两锁间隙并发订阅会使减法下溢）；投递在锁外。
    pub(crate) fn snapshot_offline(
        &self,
        online: impl Fn(&str) -> bool,
    ) -> (usize, Vec<SubscriptionRow>) {
        let rows = self.lock();
        let total = rows.len();
        let targets = rows
            .iter()
            .filter(|r| !online(&r.session_id))
            .cloned()
            .collect();
        (total, targets)
    }

    /// 落盘（快照锁内、写在锁外；0600 原子替换复用 sessions 的
    /// `write_atomic`——端点 URL 是投递能力凭证，按敏感数据保护）。
    fn persist(&self) {
        let json = {
            let rows = self.lock();
            let file = SubscriptionsFile {
                v: 1,
                subscriptions: rows.clone(),
            };
            serde_json::to_string(&file).unwrap_or_default()
        };
        if json.is_empty() {
            return;
        }
        if let Err(e) = write_atomic(&self.path, &json) {
            tracing::warn!(
                target: TARGET,
                path = %self.path.display(),
                error = %e,
                "push_subscriptions.json 写盘失败（本次运行内存态可用，重启后丢失本次变更）"
            );
        }
    }
}

/// 日志里的端点脱敏：只取 host（endpoint 路径含投递能力 token，绝不落日志）。
fn endpoint_host(endpoint: &str) -> String {
    endpoint
        .split("://")
        .nth(1)
        .unwrap_or(endpoint)
        .split('/')
        .next()
        .unwrap_or(endpoint)
        .to_string()
}

// ---------------------------------------------------------------------------
// 投递器（web-push 加密 + reqwest rustls 发送；trait 可注入 mock）
// ---------------------------------------------------------------------------

/// 投递结果（状态机三值，调用方据此维护订阅表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryOutcome {
    /// 2xx：已受理（push 服务侧排队/下发）。
    Delivered,
    /// 端点永久失效：HTTP 404/410，或订阅密钥加密失败——删订阅。
    Gone,
    /// 临时失败（网络/push 服务 5xx/429 等）——保留订阅，warn。
    Failed,
}

/// 投递抽象（生产 [`ReqwestDeliverer`]；测试注入 mock——单测不真发）。
pub(crate) trait WebPushDeliver: Send + Sync {
    /// 加密 payload 并投递到 `sub.endpoint`。实现须自带超时、不得 panic。
    fn deliver(&self, sub: &SubscriptionRow, payload: &[u8])
        -> BoxFuture<'static, DeliveryOutcome>;
}

/// 生产投递器：web-push 构建（VAPID ES256 + RFC8291 aes128gcm 加密）→
/// `request_builder::build_request` 得到 http 0.2 请求 → 手工转 reqwest
/// （跨 http 0.2/1.x 版本，字段逐一搬运）→ 发送 + 状态映射。
pub(crate) struct ReqwestDeliverer {
    http: reqwest::Client,
    vapid_private_b64: String,
}

impl ReqwestDeliverer {
    pub(crate) fn new(vapid_private_b64: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(DELIVER_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            vapid_private_b64: vapid_private_b64.to_string(),
        }
    }
}

impl WebPushDeliver for ReqwestDeliverer {
    fn deliver(
        &self,
        sub: &SubscriptionRow,
        payload: &[u8],
    ) -> BoxFuture<'static, DeliveryOutcome> {
        let http = self.http.clone();
        let vapid = self.vapid_private_b64.clone();
        let sub = SubscriptionInfo::new(sub.endpoint.clone(), sub.p256dh.clone(), sub.auth.clone());
        let payload = payload.to_vec();
        Box::pin(async move {
            // 1) VAPID 签名（私钥问题 = 系统级，影响全部订阅 → Failed 保留）。
            let sig = match VapidSignatureBuilder::from_base64_no_sub(&vapid) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(target: TARGET, error = %e, "VAPID 签名构建失败（检查 vapid.json / env）");
                    return DeliveryOutcome::Failed;
                }
            }
            .add_sub_info(&sub)
            .build();
            let sig = match sig {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(target: TARGET, error = %e, "VAPID 签名失败");
                    return DeliveryOutcome::Failed;
                }
            };
            // 2) 载荷加密（aes128gcm）。失败 = 订阅密钥永久无效（同 410 语义）。
            let mut msg = WebPushMessageBuilder::new(&sub);
            msg.set_payload(ContentEncoding::Aes128Gcm, &payload);
            msg.set_vapid_signature(sig);
            let message = match msg.build() {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(
                        target: TARGET,
                        host = %sub.endpoint.split("://").nth(1).unwrap_or(""),
                        error = %e,
                        "push 载荷加密失败（订阅密钥无效），按端点失效清理"
                    );
                    return DeliveryOutcome::Gone;
                }
            };
            // 3) http 0.2 Request → reqwest Request（跨版本手工搬运：method/
            //    url/headers/body——header 值均为 base64url/数字等 ASCII）。
            let (parts, body) = request_builder::build_request::<Vec<u8>>(message).into_parts();
            let url = match reqwest::Url::parse(parts.uri.to_string().as_str()) {
                Ok(u) => u,
                Err(_) => {
                    tracing::warn!(target: TARGET, "push endpoint URL 非法，按端点失效清理");
                    return DeliveryOutcome::Gone;
                }
            };
            let mut out = http.post(url);
            for (name, value) in parts.headers.iter() {
                out = out.header(
                    name.as_str(),
                    String::from_utf8_lossy(value.as_bytes()).as_ref(),
                );
            }
            // 4) 发送 + 状态映射（404/410 → Gone 删订阅；5xx/429/其它 → Failed）。
            match out.body(body).send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if (200..300).contains(&status) {
                        DeliveryOutcome::Delivered
                    } else if status == 404 || status == 410 {
                        DeliveryOutcome::Gone
                    } else {
                        tracing::warn!(
                            target: TARGET,
                            status,
                            "push 投递失败（临时性，保留订阅）"
                        );
                        DeliveryOutcome::Failed
                    }
                }
                Err(e) => {
                    tracing::warn!(target: TARGET, error = %e, "push 投递网络失败（临时性，保留订阅）");
                    DeliveryOutcome::Failed
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// 进程级状态 + WS 帧处理入口
// ---------------------------------------------------------------------------

/// Web Push 层进程级单例（RelayState 持 Arc 共享）。
pub(crate) struct PushState {
    vapid: VapidKeys,
    subs: Arc<PushSubStore>,
    deliver: Arc<dyn WebPushDeliver>,
}

impl PushState {
    /// 生产装配：VAPID 加载 + 真实投递器（reqwest rustls）。
    pub(crate) fn load(state_dir: &Path) -> Self {
        let vapid = VapidKeys::load(state_dir);
        let deliver = Arc::new(ReqwestDeliverer::new(&vapid.private_b64));
        Self::assemble(state_dir, vapid, deliver)
    }

    /// 装配（投递器注入形态，测试 mock 用）。
    pub(crate) fn assemble(
        state_dir: &Path,
        vapid: VapidKeys,
        deliver: Arc<dyn WebPushDeliver>,
    ) -> Self {
        Self {
            vapid,
            subs: Arc::new(PushSubStore::load(state_dir)),
            deliver,
        }
    }

    /// 测试/状态端点读取的公钥（base64url 65B）。
    pub(crate) fn vapid_public_b64(&self) -> &str {
        &self.vapid.public_b64
    }

    /// push_notify 帧处理：组装 payload → 快照离线 session 的订阅 → 逐个
    /// spawn 投递（锁外；量小 ≤32 不做并发限流）。同步返回（不 await 网络
    /// ——WS 收发循环零阻塞），结果经投递任务落订阅表。
    pub(crate) fn handle_notify(&self, notify: PushNotify, sessions: &SessionStore) {
        let payload = payload_json(&notify);
        let (total, targets) = self.subs.snapshot_offline(|id| sessions.is_online(id));
        let online_skipped = total - targets.len();
        if targets.is_empty() {
            tracing::debug!(
                target: TARGET,
                kind = notify.kind.as_str(),
                skipped_online = online_skipped,
                "push_notify 无离线订阅目标，跳过投递"
            );
            return;
        }
        tracing::info!(
            target: TARGET,
            kind = notify.kind.as_str(),
            machine = %notify.machine,
            targets = targets.len(),
            skipped_online = online_skipped,
            payload_bytes = payload.len(),
            "push_notify 投递开始"
        );
        for row in targets {
            let deliver = self.deliver.clone();
            let subs = self.subs.clone();
            let payload = payload.clone();
            tokio::spawn(async move {
                match deliver.deliver(&row, payload.as_bytes()).await {
                    DeliveryOutcome::Delivered => {
                        tracing::debug!(
                            target: TARGET,
                            host = %endpoint_host(&row.endpoint),
                            "push 投递成功"
                        );
                    }
                    DeliveryOutcome::Gone => {
                        let removed = subs.remove(&row.endpoint);
                        tracing::info!(
                            target: TARGET,
                            host = %endpoint_host(&row.endpoint),
                            removed,
                            "push 端点失效（404/410/密钥无效），清理订阅"
                        );
                    }
                    DeliveryOutcome::Failed => {
                        // warn 已在投递器内落（带状态/错误详情），此处不重试
                        // ——下一条事件天然重试，持续失效的订阅由用户重开
                        // 通知开关兜底。
                    }
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP 端点（三枚，全部 Bearer sessionToken 认证；/imagent 前缀两形态）
// ---------------------------------------------------------------------------

/// POST /push/subscribe 请求体（浏览器 `pushSubscription` 的 JSON 传输形态）。
#[derive(serde::Deserialize)]
struct SubscribeBody {
    endpoint: String,
    keys: SubscribeKeys,
}

#[derive(serde::Deserialize)]
struct SubscribeKeys {
    p256dh: String,
    auth: String,
}

/// POST /push/unsubscribe 请求体。
#[derive(serde::Deserialize)]
struct UnsubscribeBody {
    endpoint: String,
}

/// base64url 字符集校验（无 padding；只验形态——真正的密钥合法性在投递期
/// 由加密步骤验证，坏值投递失败按 410 语义清理）。
fn is_b64url(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 512
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// URL 是否指向 loopback 主机（订阅端点 http 明文的唯一豁免面：本地测试/
/// 开发形态——真实 push 服务恒为 https）。`http://127.0.0.1:3011/…` /
/// `http://localhost/…` / `http://[::1]:3011/…`。
fn is_loopback_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.split('@').next_back().unwrap_or(authority);
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.split_once(']'))
        .map(|(h, _)| h.to_string())
        .unwrap_or_else(|| host.split(':').next().unwrap_or(host).to_string());
    matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1")
}

/// 订阅值域校验（任务定案：endpoint 须 https——本地测试/开发形态的
/// loopback http 豁免见 [`is_loopback_url`]；keys 两字段非空 base64url）。
/// Err 文案即 400 响应的 message。
fn validate_subscription(endpoint: &str, p256dh: &str, auth: &str) -> Result<(), &'static str> {
    if endpoint.len() > 2048 {
        return Err("endpoint 过长（>2048 字节）");
    }
    if !(endpoint.starts_with("https://") || is_loopback_url(endpoint)) {
        return Err("endpoint 须为 https URL");
    }
    if !is_b64url(p256dh) || !is_b64url(auth) {
        return Err("keys.p256dh / keys.auth 须为非空 base64url 字符串");
    }
    Ok(())
}

/// POST /push/subscribe：绑定当前 session；同 endpoint 幂等覆盖（session
/// 可换）→ 200 `{}`。
async fn push_subscribe(
    State(st): State<RelayState>,
    headers: HeaderMap,
    body: Option<Json<SubscribeBody>>,
) -> Response {
    let session_id = match auth_session(&st, &headers) {
        Ok(id) => id,
        Err(resp) => return *resp,
    };
    let Some(Json(b)) = body else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            Some("请求体须为 {\"endpoint\":…,\"keys\":{\"p256dh\":…,\"auth\":…}}"),
        );
    };
    if let Err(msg) = validate_subscription(&b.endpoint, &b.keys.p256dh, &b.keys.auth) {
        return err_json(StatusCode::BAD_REQUEST, "invalid_request", Some(msg));
    }
    st.push.subs.subscribe(
        &session_id,
        &b.endpoint,
        &b.keys.p256dh,
        &b.keys.auth,
        unix_now(),
    );
    tracing::info!(
        target: TARGET,
        session = %session_id,
        host = %endpoint_host(&b.endpoint),
        "push 订阅登记"
    );
    Json(serde_json::json!({})).into_response()
}

/// POST /push/unsubscribe：按 endpoint 删除（未知 endpoint 幂等 200）。
async fn push_unsubscribe(
    State(st): State<RelayState>,
    headers: HeaderMap,
    body: Option<Json<UnsubscribeBody>>,
) -> Response {
    let session_id = match auth_session(&st, &headers) {
        Ok(id) => id,
        Err(resp) => return *resp,
    };
    let Some(Json(b)) = body else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            Some("请求体须为 {\"endpoint\":…}"),
        );
    };
    let removed = if b.endpoint.is_empty() {
        false
    } else {
        st.push.subs.remove(&b.endpoint)
    };
    tracing::info!(
        target: TARGET,
        session = %session_id,
        host = %endpoint_host(&b.endpoint),
        removed,
        "push 退订"
    );
    Json(serde_json::json!({})).into_response()
}

/// GET /push/status：`{"supported":true,"subscriptions":<n>,
/// "vapidPublicKey":"<base64url 65B>"}`（SPA 订阅必需公钥来源）。
async fn push_status(State(st): State<RelayState>, headers: HeaderMap) -> Response {
    if let Err(resp) = auth_session(&st, &headers) {
        return *resp;
    }
    Json(serde_json::json!({
        "supported": true,
        "subscriptions": st.push.subs.count(),
        "vapidPublicKey": st.push.vapid_public_b64(),
    }))
    .into_response()
}

/// 路由（裸路径 + /imagent 前缀两形态，对齐 webauthn/WS 端点——公网 Caddy
/// strip 前缀与直连两种形态都可用；安全响应头由全局中间件统一覆盖）。
pub(crate) fn routes() -> Router<RelayState> {
    Router::new()
        .route("/push/subscribe", post(push_subscribe))
        .route("/push/unsubscribe", post(push_unsubscribe))
        .route("/push/status", get(push_status))
        .route("/imagent/push/subscribe", post(push_subscribe))
        .route("/imagent/push/unsubscribe", post(push_unsubscribe))
        .route("/imagent/push/status", get(push_status))
}

// ---------------------------------------------------------------------------
// 单元测试（纯函数 + mock 投递器，不真发 HTTP）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "imagent-relay-push-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn notify(kind: NotifyKind) -> PushNotify {
        PushNotify {
            kind,
            machine: "home-mac".into(),
            conv_id: "web:s-ab12".into(),
            title: "统计 Cargo 依赖数量".into(),
        }
    }

    /// mock 投递器：记录 (endpoint, payload_len) 调用并回预设结果。
    struct MockDeliverer {
        calls: mpsc::UnboundedSender<(String, usize)>,
        outcome: Mutex<DeliveryOutcome>,
    }

    impl MockDeliverer {
        fn new(outcome: DeliveryOutcome) -> (Self, mpsc::UnboundedReceiver<(String, usize)>) {
            let (tx, rx) = mpsc::unbounded_channel();
            (
                Self {
                    calls: tx,
                    outcome: Mutex::new(outcome),
                },
                rx,
            )
        }
    }

    impl WebPushDeliver for MockDeliverer {
        fn deliver(
            &self,
            sub: &SubscriptionRow,
            payload: &[u8],
        ) -> BoxFuture<'static, DeliveryOutcome> {
            let _ = self.calls.send((sub.endpoint.clone(), payload.len()));
            let outcome = *self.outcome.lock().unwrap();
            Box::pin(async move { outcome })
        }
    }

    // ---- 帧解析 ----

    #[test]
    fn parse_notify_frame_accepts_three_kinds() {
        for (kind_text, expect) in [
            ("round_done", NotifyKind::RoundDone),
            ("round_error", NotifyKind::RoundError),
            ("approval", NotifyKind::Approval),
        ] {
            let f = serde_json::json!({
                "v": 1, "type": "push_notify", "machine": "home-mac",
                "kind": kind_text, "convId": "web:s-ab12", "title": "摘要",
            })
            .to_string();
            let nf = parse_notify_frame(&f).expect("合法帧应解析");
            assert_eq!(nf.kind, expect);
            assert_eq!(nf.machine, "home-mac");
            assert_eq!(nf.conv_id, "web:s-ab12");
            assert_eq!(nf.title, "摘要");
        }
    }

    #[test]
    fn parse_notify_frame_rejects_malformed() {
        // 缺字段 / 非字符串 / kind 越界 / type 不符 / 畸形 JSON。
        let cases = [
            r#"{"v":1,"type":"push_notify","kind":"round_done","convId":"c","title":"t"}"#,
            r#"{"v":1,"type":"push_notify","machine":"m","kind":"round_done","convId":"c"}"#,
            r#"{"v":1,"type":"push_notify","machine":42,"kind":"round_done","convId":"c","title":"t"}"#,
            r#"{"v":1,"type":"push_notify","machine":"m","kind":"bogus","convId":"c","title":"t"}"#,
            r#"{"v":1,"type":"push_notify","machine":"m","kind":7,"convId":"c","title":"t"}"#,
            r#"{"v":1,"type":"message","text":"push_notify 假帧"}"#,
            r#"{"v":1,"type":"push_notify"}"#,
            "not json",
        ];
        for case in cases {
            assert!(parse_notify_frame(case).is_none(), "应拒绝：{case}");
        }
    }

    // ---- payload 组装 ----

    #[test]
    fn payload_json_byte_shape_and_cap() {
        let p = payload_json(&notify(NotifyKind::RoundDone));
        assert_eq!(
            p,
            r#"{"kind":"round_done","title":"统计 Cargo 依赖数量","convId":"web:s-ab12","machine":"home-mac"}"#
        );
        assert!(p.len() <= PAYLOAD_MAX_BYTES);

        // 超长中文 title：防御性按码点收缩到 ≤240B。
        let long = PushNotify {
            title: "长".repeat(200),
            ..notify(NotifyKind::Approval)
        };
        let p = payload_json(&long);
        assert!(p.len() <= PAYLOAD_MAX_BYTES, "len={}", p.len());
        assert!(p.contains("\"kind\":\"approval\""), "kind 恒在：{p}");
        assert!(p.contains("\"machine\":\"home-mac\""), "machine 恒在：{p}");
    }

    // ---- 订阅表 ----

    fn sub_store(dir: &Path) -> PushSubStore {
        // load 是私有构造，测试经 assemble 间接构建？不——直接复用 load 的
        // 行为：PushSubStore::load 为 crate 内私有，测试同模块可调。
        PushSubStore::load(dir)
    }

    #[test]
    fn subscribe_idempotent_overwrite_and_unsubscribe() {
        let dir = temp_dir("subs-basic");
        let store = sub_store(&dir);
        store.subscribe("s-1", "https://p.example/a", "P256DH", "AUTH", 100);
        assert_eq!(store.count(), 1);

        // 幂等：完全相同 → 不变。
        store.subscribe("s-1", "https://p.example/a", "P256DH", "AUTH", 100);
        assert_eq!(store.count(), 1);
        let (_, rows) = store.snapshot_offline(|_| false);
        assert_eq!(rows[0].created, 100);

        // 同 endpoint 覆盖：session 可换，created 摸新。
        store.subscribe("s-2", "https://p.example/a", "P256DH-2", "AUTH-2", 200);
        let (_, rows) = store.snapshot_offline(|_| false);
        assert_eq!(store.count(), 1, "endpoint 幂等键，不新增行");
        assert_eq!(rows[0].session_id, "s-2");
        assert_eq!(rows[0].p256dh, "P256DH-2");
        assert_eq!(rows[0].created, 200);

        // 退订：存在 → true；未知 → false（幂等）。
        assert!(store.remove("https://p.example/a"));
        assert!(!store.remove("https://p.example/a"));
        assert_eq!(store.count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subscribe_lru_eviction_at_cap() {
        let dir = temp_dir("subs-lru");
        let store = sub_store(&dir);
        for i in 0..MAX_SUBSCRIPTIONS {
            store.subscribe(
                "s",
                &format!("https://p.example/{i}"),
                "P",
                "A",
                100 + i as u64,
            );
        }
        assert_eq!(store.count(), MAX_SUBSCRIPTIONS);
        // 新订阅挤掉 created 最旧（0 号）。
        store.subscribe("s", "https://p.example/new", "P", "A", 999);
        assert_eq!(store.count(), MAX_SUBSCRIPTIONS);
        let eps: Vec<String> = store
            .snapshot_offline(|_| false)
            .1
            .into_iter()
            .map(|r| r.endpoint)
            .collect();
        assert!(
            !eps.iter().any(|e| e.ends_with("/0")),
            "最旧被淘汰：{eps:?}"
        );
        assert!(eps.iter().any(|e| e.ends_with("/new")));
        assert!(eps.iter().any(|e| e.ends_with("/1")), "次旧保留：{eps:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subscriptions_persist_roundtrip_and_permissions() {
        let dir = temp_dir("subs-persist");
        {
            let store = sub_store(&dir);
            store.subscribe("s-1", "https://p.example/a", "K1", "A1", 100);
            store.subscribe("s-2", "https://p.example/b", "K2", "A2", 200);
        }
        // 「重启」：同目录新实例——订阅保留、created 升序。
        let store2 = sub_store(&dir);
        let (_, rows) = store2.snapshot_offline(|_| false);
        assert_eq!(
            rows.iter().map(|r| r.endpoint.as_str()).collect::<Vec<_>>(),
            vec!["https://p.example/a", "https://p.example/b"]
        );
        assert_eq!(rows[0].p256dh, "K1");
        // 落盘形状：0600 + 无 tmp 残留 + 行字段契约逐字。
        let raw = std::fs::read_to_string(dir.join(SUBSCRIPTIONS_FILE)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let row0 = &v["subscriptions"][0];
        for key in ["session_id", "endpoint", "p256dh", "auth", "created"] {
            assert!(row0.get(key).is_some(), "行须含 {key}：{row0}");
        }
        assert!(!dir.join("push_subscriptions.json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(SUBSCRIPTIONS_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "订阅表权限 0600");
        }
        // 损坏文件 → 空表启动不 panic。
        std::fs::write(dir.join(SUBSCRIPTIONS_FILE), "not json").unwrap();
        let store3 = sub_store(&dir);
        assert_eq!(store3.count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- VAPID ----

    #[test]
    fn vapid_generate_persists_and_reloads() {
        let dir = temp_dir("vapid-gen");
        let keys = VapidKeys::load_at(&dir, None, None);
        // 公钥 = 未压缩点 65B 的 base64url（87 char）。
        assert_eq!(keys.public_b64.len(), 87, "{}", keys.public_b64);
        assert!(dir.join(VAPID_FILE).exists(), "生成须落盘");
        let raw = std::fs::read_to_string(dir.join(VAPID_FILE)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(
            v["privateKey"].as_str().unwrap().len(),
            43,
            "32B base64url = 43 char"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(VAPID_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "vapid.json 权限 0600");
        }
        // 「重启」：文件权威（无 env）→ 同一把公钥。
        let keys2 = VapidKeys::load_at(&dir, None, None);
        assert_eq!(
            keys.public_b64, keys2.public_b64,
            "文件权威，公钥跨重启稳定"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn vapid_env_overrides_file() {
        let dir = temp_dir("vapid-env");
        // 先落一份文件。
        let file_keys = VapidKeys::load_at(&dir, None, None);
        // env 私钥（另一把）优先于文件。
        let other = VapidKeys::generate();
        let env_keys = VapidKeys::load_at(&dir, None, Some(other.private_b64.clone()));
        assert_eq!(env_keys.public_b64, other.public_b64, "env 私钥优先");
        assert_ne!(env_keys.public_b64, file_keys.public_b64);
        // env 公私钥齐全且不一致 → 私钥派生为准（不 panic）。
        let mixed = VapidKeys::load_at(
            &dir,
            Some(file_keys.public_b64.clone()),
            Some(other.private_b64.clone()),
        );
        assert_eq!(mixed.public_b64, other.public_b64, "私钥派生公钥为权威");
        // env 非法 → 回退文件。
        let fallback = VapidKeys::load_at(&dir, None, Some("!!!not-b64url!!!".into()));
        assert_eq!(
            fallback.public_b64, file_keys.public_b64,
            "非法 env 回退文件"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 端点值域校验 ----

    #[test]
    fn validate_subscription_rules() {
        let ok = |e, k, a| validate_subscription(e, k, a);
        assert!(ok("https://fcm.googleapis.com/fcm/send/x", "AAAA", "bbbb").is_ok());
        // loopback http 豁免（本地测试/开发）。
        assert!(ok("http://127.0.0.1:3011/fake", "AAAA", "bbbb").is_ok());
        assert!(ok("http://localhost:3011/fake", "AAAA", "bbbb").is_ok());
        assert!(ok("http://[::1]:3011/fake", "AAAA", "bbbb").is_ok());
        // 明文 http 公网 → 拒。
        assert!(ok("http://push.example/x", "AAAA", "bbbb").is_err());
        assert!(ok("ftp://push.example/x", "AAAA", "bbbb").is_err());
        // keys 非法。
        assert!(ok("https://p.example/x", "", "bbbb").is_err());
        assert!(
            ok("https://p.example/x", "AA+AA/==", "bbbb").is_err(),
            "标准 b64 含非 url 字符"
        );
        assert!(ok("https://p.example/x", "AAAA", "").is_err());
        // endpoint 过长。
        let long = format!("https://p.example/{}", "x".repeat(2100));
        assert!(ok(&long, "AAAA", "bbbb").is_err());
    }

    #[test]
    fn endpoint_host_masks_path() {
        assert_eq!(
            endpoint_host("https://fcm.googleapis.com/fcm/send/secret-token"),
            "fcm.googleapis.com"
        );
        assert_eq!(
            endpoint_host("http://127.0.0.1:3011/fake"),
            "127.0.0.1:3011"
        );
        assert_eq!(endpoint_host("weird"), "weird");
    }

    // ---- push_notify 处理（mock 投递器：在线跳过 / 离线投递 / Gone 清理）----

    #[tokio::test]
    async fn handle_notify_skips_online_sessions_and_delivers_offline() {
        let dir = temp_dir("notify-online");
        let sessions = SessionStore::load(&dir);
        let online = sessions.issue("online-dev", unix_now(), 3_600).unwrap();
        let offline = sessions.issue("offline-dev", unix_now(), 3_600).unwrap();
        // online 会话绑定活跃连接（免打扰），offline 无连接。
        let (tx, _rx) = mpsc::channel(8);
        assert!(sessions.bind(&online.id, 1, tx));
        assert!(sessions.is_online(&online.id));
        assert!(!sessions.is_online(&offline.id));

        let (deliverer, mut calls) = MockDeliverer::new(DeliveryOutcome::Delivered);
        let state = PushState::assemble(&dir, VapidKeys::generate(), Arc::new(deliverer));
        // 两个订阅：一个挂在线 session，一个挂离线 session。
        state
            .subs
            .subscribe(&online.id, "https://p.example/online", "K", "A", 1);
        state
            .subs
            .subscribe(&offline.id, "https://p.example/offline", "K", "A", 2);

        state.handle_notify(notify(NotifyKind::RoundDone), &sessions);
        let mut got = Vec::new();
        for _ in 0..1 {
            got.push(
                tokio::time::timeout(Duration::from_secs(2), calls.recv())
                    .await
                    .expect("离线订阅应收到投递")
                    .expect("channel 开着"),
            );
        }
        assert_eq!(
            got.iter().map(|(e, _)| e.as_str()).collect::<Vec<_>>(),
            vec!["https://p.example/offline"],
            "在线 session 的订阅被跳过"
        );
        let (_, payload_len) = &got[0];
        assert!(*payload_len > 0, "载荷非空");
        // 通道已空（无多余投递）。
        assert!(calls.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn handle_notify_gone_removes_subscription() {
        let dir = temp_dir("notify-gone");
        let sessions = SessionStore::load(&dir);
        let (deliverer, mut calls) = MockDeliverer::new(DeliveryOutcome::Gone);
        let state = PushState::assemble(&dir, VapidKeys::generate(), Arc::new(deliverer));
        state
            .subs
            .subscribe("s-1", "https://p.example/dead", "K", "A", 1);
        assert_eq!(state.subs.count(), 1);

        state.handle_notify(notify(NotifyKind::Approval), &sessions);
        tokio::time::timeout(Duration::from_secs(2), calls.recv())
            .await
            .expect("应投递一次")
            .expect("channel 开着");
        // Gone → 订阅被清理（异步任务完成后的最终态，轮询等待）。
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while state.subs.count() != 0 {
            assert!(std::time::Instant::now() < deadline, "订阅应被 Gone 清理");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn handle_notify_failed_keeps_subscription() {
        let dir = temp_dir("notify-failed");
        let sessions = SessionStore::load(&dir);
        let (deliverer, mut calls) = MockDeliverer::new(DeliveryOutcome::Failed);
        let state = PushState::assemble(&dir, VapidKeys::generate(), Arc::new(deliverer));
        state
            .subs
            .subscribe("s-1", "https://p.example/flaky", "K", "A", 1);

        state.handle_notify(notify(NotifyKind::RoundError), &sessions);
        tokio::time::timeout(Duration::from_secs(2), calls.recv())
            .await
            .expect("应投递一次")
            .expect("channel 开着");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(state.subs.count(), 1, "临时失败保留订阅");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
