//! [`WebPlatform`]：实现 [`imagent_core::Platform`]，接入 Dispatcher 全管线。
//!
//! 组织方式参照 wecom（client 任务 + 双 mpsc + 平台薄壳），但 web 无 proto
//! 加密、无 outbox、无 IM 协议——出站即 JSON 帧：
//! - `recv()`：inbound channel 前端（client 已把浏览器帧映射成
//!   [`InboundMessage`]）；断线期间无消息天然 pending（与 IM 平台断线一致）。
//! - `send_text` / `send_card` / `update_card` / `send_permission_ask` 族 /
//!   `reconnect` / `doctor_probes`：覆写为对应帧（见 lib.rs 帧药化函数），
//!   经出站 channel（容量 256）交给 client。断线期帧排队（client 重连后按序
//!   发出）；队列满（relay 长时间不可达）→ Err 上抛。
//! - sync 重发（D6 故障语义）：per-conv 缓存最近卡片（send_card / update_card
//!   更新，上限 32 conv 淘汰最旧），浏览器发 `sync` 时由独立 responder 任务
//!   全量重发 card 帧。relay 保证单浏览器连接，无定向问题。
//! - workspace 泵（v1.6 工作空间）：消费 client 转来的
//!   workspace_list/save/remove/conv_workdir_set 动作，直接读写 store KV
//!   （`workspace:`/`workdir:` 表，与终端 /ws /cd 同源互通）+
//!   [`imagent_core::validate_workdir`] 校验（复用 /cd 护栏，不经 dispatch
//!   命令链），每帧回 workspaces 全量快照（失败前置 error 帧）。构造注入
//!   store 句柄 + default_workdir（main 装配处传入）。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use imagent_store::Store;
use tokio::sync::{mpsc, Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use imagent_core::{
    validate_workdir,
    workspace::{workdir_key, workspace_key, WorkspaceRecord, WORKDIR_PREFIX, WORKSPACE_PREFIX},
    ConvId, CoreError, InboundMessage, MediaRef, OutboundCard, PermissionReply, Platform,
    PlatformCaps, ReplyHint, Result,
};

use crate::client::WebClient;
use crate::e2e::E2eIdentity;
use crate::{
    encode_approval_request_frame, encode_approval_resolved_frame, encode_card_frame,
    encode_conv_workdir_frame, encode_error_frame, encode_text_frame, encode_workspaces_frame,
    CardPayload, GatewayCredential, InboundAction, WorkspaceEntryFrame, TARGET,
};

const PLATFORM: &str = "web";

/// 出站 channel 容量（§5.1）：断线期 send_* 帧在此排队（client 重连后按序
/// 发出）；打满（relay 长时间不可达）→ send_* 返回 Err。
const OUTBOUND_CHANNEL_CAPACITY: usize = 256;
/// 入站 channel 容量（wecom 先例：dispatch 消费短暂抖动不丢消息）。
const INBOUND_CHANNEL_CAPACITY: usize = 64;
/// sync 请求 channel 容量。
const SYNC_CHANNEL_CAPACITY: usize = 8;
/// workspace 动作 channel 容量（v1.6）：SPA 点按级频率（保存/删除/切目录），
/// 16 足以吸收突发；打满由 client 侧 1s 有界背压丢弃 + SPA 重发兜底。
const WORKSPACE_CHANNEL_CAPACITY: usize = 16;
/// 卡片缓存 conv 上限（§5.4）：超限淘汰最旧（seq 最小），防长寿命进程内存
/// 无界增长。
const CARD_CACHE_MAX_CONVS: usize = 32;
/// v1.6 工作空间错误帧 code（帧契约定案字面量，SPA 据此分支渲染）。
const WORKSPACE_ERROR_CODE: &str = "workspace_invalid";
/// workspace_save 的 name 上限（帧契约：trim 后 1..=32 char）。
const WORKSPACE_NAME_MAX_CHARS: usize = 32;

// ---------------------------------------------------------------------------
// 卡片缓存（D6 sync 重发）
// ---------------------------------------------------------------------------

/// per-conv 最近卡片缓存：`conv -> (seq, message_id, card)`。seq 单调递增，
/// 兼作淘汰序（最小者最旧）与快照序（旧→新重发）。纯内存临界区用 std
/// Mutex（锁内计算 → clone 出锁，relay Hub 同款纪律）。
#[derive(Default)]
struct CardCache {
    entries: HashMap<String, (u64, String, CardPayload)>,
    next_seq: u64,
}

impl CardCache {
    fn upsert(&mut self, conv: &str, message_id: &str, card: CardPayload) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries
            .insert(conv.to_string(), (seq, message_id.to_string(), card));
        if self.entries.len() > CARD_CACHE_MAX_CONVS {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (s, _, _))| *s)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest);
            }
        }
    }

    /// 全量快照（按 seq 旧→新排序，重发顺序与卡片创建顺序一致）。
    fn snapshot(&self) -> Vec<(String, String, CardPayload)> {
        let mut v: Vec<(u64, String, String, CardPayload)> = self
            .entries
            .iter()
            .map(|(k, (s, mid, c))| (*s, k.clone(), mid.clone(), c.clone()))
            .collect();
        v.sort_by_key(|(s, _, _, _)| *s);
        v.into_iter().map(|(_, k, mid, c)| (k, mid, c)).collect()
    }
}

/// pending 审批记录（cancel_all 逐条收敛用）：`conv -> request_id 列表`。
type PendingAsks = HashMap<String, Vec<String>>;

/// 中毒（临界区 panic）时取回数据继续服务：纯内存临界区无 IO 副作用可回滚
/// （relay Hub / webhook 同款取舍）。
fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// sync responder：消费 client 转来的 sync 请求，把卡片缓存全量重发（card
/// 帧，state 依 terminal，v1.1 起带本机 machine 字段——与首发帧同形）。
/// best-effort——出站队列满即丢弃该轮重发并 warn（浏览器可再发 sync 刷新，
/// 不值得反压阻塞 client 的 sync channel）。
async fn serve_sync_requests(
    mut sync_rx: mpsc::Receiver<()>,
    machine_id: String,
    cards: Arc<Mutex<CardCache>>,
    outbound_tx: mpsc::Sender<String>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            req = sync_rx.recv() => {
                if req.is_none() {
                    break; // client 退出（platform drop 连动），无后续 sync
                }
                let snapshot = lock_or_recover(&cards).snapshot();
                debug!(target: TARGET, convs = snapshot.len(), "sync：重发缓存卡片");
                for (conv, message_id, card) in snapshot {
                    match encode_card_frame(&machine_id, "card", &conv, &message_id, &card) {
                        Ok(json) => {
                            if outbound_tx.try_send(json).is_err() {
                                warn!(target: TARGET, conv = %conv, "sync 重发出站队列满，丢弃该轮重发");
                                break;
                            }
                        }
                        Err(e) => warn!(target: TARGET, error = %e, conv = %conv, "sync 重发帧序列化失败"),
                    }
                }
            }
        }
    }
    debug!(target: TARGET, "sync responder 退出");
}

// ---------------------------------------------------------------------------
// workspace 泵（v1.6 工作空间：store KV 读写 + 校验 + 回帧）
// ---------------------------------------------------------------------------

/// workspace 泵主循环：消费 client 转来的工作空间动作（store API 是 async，
/// 在本泵内 await——不阻塞 client 收发循环与 dispatch）。`machine_id` 为
/// 本机身份（出站帧必带，v1.1 语义；与 serve_sync_requests 同款注入）。
/// 平台 Drop 时经 shutdown token 连动退出。
async fn serve_workspace_actions(
    mut workspace_rx: mpsc::Receiver<InboundAction>,
    machine_id: String,
    store: Store,
    default_workdir: PathBuf,
    outbound_tx: mpsc::Sender<String>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            act = workspace_rx.recv() => {
                let Some(act) = act else {
                    break; // client 退出（platform drop 连动），无后续动作
                };
                handle_workspace_action(&machine_id, &store, &default_workdir, act, &outbound_tx).await;
            }
        }
    }
    debug!(target: TARGET, "workspace 泵退出");
}

/// 处理一条工作空间动作并回帧。规则（协议 v1.6 语义）：
/// - **每帧处理完（无论成败）都回一条 workspaces 全量快照**（SPA 单一数据
///   来源；失败时快照不变 + 前置 error 帧）。
/// - conv_workdir_set 成功时**额外**推一条 conv_workdir 增量帧。
/// - workspace_remove 未命中幂等（store delete 对不存在键即 Ok）。
async fn handle_workspace_action(
    machine: &str,
    store: &Store,
    default_workdir: &Path,
    act: InboundAction,
    outbound_tx: &mpsc::Sender<String>,
) {
    match act {
        InboundAction::WorkspaceList => {} // 直接回快照（收尾统一发）
        InboundAction::WorkspaceSave { name, path } => {
            if let Err(msg) = validate_workspace_save(&name, &path) {
                reply_workspace_error(machine, outbound_tx, &msg);
            } else {
                // 同名覆盖：写全新记录（lastUsedAt 缺席——刚保存未被用过）。
                let key = workspace_key(name.trim());
                let value = WorkspaceRecord::new(&path).encode();
                if let Err(e) = store.set_config(&key, &value).await {
                    reply_workspace_error(machine, outbound_tx, &format!("保存失败：{e}"));
                } else {
                    info!(target: TARGET, name = %name.trim(), "workspace_save：已保存工作空间");
                }
            }
        }
        InboundAction::WorkspaceRemove { name } => {
            // 未命中幂等：delete_config 对不存在键返回 Ok，不报错。
            if let Err(e) = store.delete_config(&workspace_key(&name)).await {
                reply_workspace_error(machine, outbound_tx, &format!("删除失败：{e}"));
            }
        }
        InboundAction::ConvWorkdirSet { conv, path } => {
            if let Err(msg) = validate_workdir_str(&path) {
                reply_workspace_error(machine, outbound_tx, &msg);
            } else {
                // 与 /cd 同键（workdir:<conv>）——终端与网页互通的关键。
                match store.set_config(&workdir_key(&conv), &path).await {
                    Err(e) => {
                        reply_workspace_error(machine, outbound_tx, &format!("保存失败：{e}"))
                    }
                    Ok(()) => {
                        bump_workspace_last_used(store, &path).await;
                        // 额外增量帧（先增量后快照——快照是 SPA 单一来源，放最后）。
                        match encode_conv_workdir_frame(machine, &conv, &path) {
                            Ok(json) => enqueue_best_effort(outbound_tx, json),
                            Err(e) => warn!(
                                target: TARGET,
                                conv = %conv,
                                error = %e,
                                "conv_workdir 增量帧序列化失败（快照兜底）"
                            ),
                        }
                    }
                }
            }
        }
        // client 只转发上述四个变体进 workspace channel，其余分支不可达
        //（防御性忽略，不 panic——通道元素类型复用 InboundAction）。
        other => {
            debug!(target: TARGET, act = ?other, "workspace 泵收到非工作空间动作，忽略")
        }
    }
    // 每帧收尾：全量快照（无论成败）。
    match build_workspaces_frame(machine, store, default_workdir).await {
        Ok(json) => enqueue_best_effort(outbound_tx, json),
        Err(e) => warn!(target: TARGET, error = %e, "workspaces 快照帧序列化失败"),
    }
}

/// workspaces 全量快照组帧：default = 配置 default_workdir；workspaces =
/// 全部命名空间（name 稳定序）；convs = 全部已知 conv 的当前 workdir
///（含 local: 会话）。best-effort：单段 list 失败 → warn + 该段空，快照
/// 照发（SPA 单一数据来源，宁可缺段不可断流）。
async fn build_workspaces_frame(
    machine: &str,
    store: &Store,
    default_workdir: &Path,
) -> Result<String> {
    let mut entries: Vec<WorkspaceEntryFrame> = Vec::new();
    match store.list_config(WORKSPACE_PREFIX).await {
        Ok(rows) => {
            for (k, v) in rows {
                let Some(name) = k.strip_prefix(WORKSPACE_PREFIX) else {
                    continue;
                };
                // 旧值（裸路径）与 v1.6 JSON 值统一 decode；损坏值跳过。
                if let Some(rec) = WorkspaceRecord::decode(&v) {
                    entries.push(WorkspaceEntryFrame {
                        name: name.to_string(),
                        path: rec.path,
                        last_used_at: rec.last_used_at,
                    });
                }
            }
        }
        Err(e) => warn!(target: TARGET, error = %e, "workspaces 快照：列出工作空间失败（段空）"),
    }
    let mut convs: BTreeMap<String, String> = BTreeMap::new();
    match store.list_config(WORKDIR_PREFIX).await {
        Ok(rows) => {
            for (k, v) in rows {
                if let Some(conv) = k.strip_prefix(WORKDIR_PREFIX) {
                    // workdir KV 值恒为裸路径（/cd 与本泵同形态写入）。
                    convs.insert(conv.to_string(), v);
                }
            }
        }
        Err(e) => {
            warn!(target: TARGET, error = %e, "workspaces 快照：列出 conv workdir 失败（段空）")
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name)); // 契约：按 name 稳定序
    encode_workspaces_frame(machine, &default_workdir.to_string_lossy(), entries, convs)
}

/// conv_workdir_set 的 path 校验链（与 /cd 同链）：绝对路径 + is_dir +
/// [`validate_workdir`] 过宽目录拒绝——三者由 validate_workdir 一体覆盖
///（其内部先验绝对/存在再黑名单），错误文案即人话中文。
fn validate_workdir_str(path: &str) -> std::result::Result<(), String> {
    validate_workdir(Path::new(path))
}

/// workspace_save 校验：name trim 后 1..=32 char 非空；path 走
/// [`validate_workdir_str`] 同链。Err(中文原因) → error 帧 message。
fn validate_workspace_save(name: &str, path: &str) -> std::result::Result<(), String> {
    let chars = name.trim().chars().count();
    if chars == 0 {
        return Err("名称不能为空（trim 后 1~32 字符）".into());
    }
    if chars > WORKSPACE_NAME_MAX_CHARS {
        return Err(format!(
            "名称最长 {WORKSPACE_NAME_MAX_CHARS} 字符（当前 {chars}）"
        ));
    }
    validate_workdir_str(path)
}

/// conv_workdir_set 命中某空间 path 时 bump 其 lastUsedAt（best-effort：
/// 读写失败仅 warn——时间戳只影响 SPA「最近用过」排序，不影响功能）。
async fn bump_workspace_last_used(store: &Store, path: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let rows = match store.list_config(WORKSPACE_PREFIX).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(target: TARGET, error = %e, "bump lastUsedAt：列出工作空间失败（跳过）");
            return;
        }
    };
    for (k, v) in rows {
        let Some(name) = k.strip_prefix(WORKSPACE_PREFIX) else {
            continue;
        };
        let Some(rec) = WorkspaceRecord::decode(&v) else {
            continue;
        };
        if rec.path == path {
            let bumped = WorkspaceRecord {
                last_used_at: Some(now),
                ..rec
            };
            if let Err(e) = store
                .set_config(&workspace_key(name), &bumped.encode())
                .await
            {
                warn!(target: TARGET, name, error = %e, "bump lastUsedAt 写回失败（忽略）");
            }
        }
    }
}

/// 出站帧 best-effort 入队（workspace 路径专用：队列满/通道关闭丢帧 warn——
/// SPA 重发或下条消息触发的快照兜底，不反压泵）。
fn enqueue_best_effort(outbound_tx: &mpsc::Sender<String>, json: String) {
    if let Err(e) = outbound_tx.try_send(json) {
        warn!(target: TARGET, error = %e, "工作空间出站帧入队失败（队列满/通道关闭），丢弃");
    }
}

/// 工作空间 error 帧（code=workspace_invalid，复用既有 error 帧形态；
/// machine 为本机身份，v1.1 出站帧必带）。
fn reply_workspace_error(machine: &str, outbound_tx: &mpsc::Sender<String>, message: &str) {
    match encode_error_frame(machine, WORKSPACE_ERROR_CODE, message) {
        Ok(json) => enqueue_best_effort(outbound_tx, json),
        Err(e) => warn!(target: TARGET, error = %e, "workspace error 帧序列化失败"),
    }
}

// ---------------------------------------------------------------------------
// WebPlatform
// ---------------------------------------------------------------------------

/// Web 平台适配器（网关侧薄壳：两条 channel + 后台 client / sync responder）。
pub struct WebPlatform {
    /// relay 地址（doctor_probes 展示用）。
    relay_url: String,
    /// 本机 machine 身份（v1.1 多终端管理）：hello 自报 + 全部出站帧携带。
    machine_id: String,
    /// 出站帧通道（发给 client → relay → 浏览器）。
    outbound_tx: mpsc::Sender<String>,
    /// `/reconnect` 强制重连信号（与 client run task 共享）。
    reconnect: Arc<Notify>,
    /// 入站消息 channel 前端（`recv` 直接 await；AsyncMutex 跨 await 持锁）。
    inbound_rx: Arc<AsyncMutex<mpsc::Receiver<InboundMessage>>>,
    /// 在线标志（client 维护）——doctor_probes / 测试用。
    online: Arc<AtomicBool>,
    /// per-conv 最近卡片缓存（sync 重发）。
    cards: Arc<Mutex<CardCache>>,
    /// 合成卡片 id 计数器：`web-card-<n>` 单调递增（§4.2）。
    card_seq: Arc<AtomicU64>,
    /// pending 审批记录（cancel_all 收敛用）。
    pending_asks: Arc<Mutex<PendingAsks>>,
    /// 停机信号（Drop 时 cancel，client / sync responder 连动退出）。
    shutdown: CancellationToken,
}

impl WebPlatform {
    /// 构造并后台 spawn client 任务（建连/认证/心跳/重连/收发）与 sync
    /// responder / workspace 泵任务。`relay_url` 形如 `wss://…/imagent/gateway`；
    /// `credential` 为网关认证凭证（v1.3：key 优先 token 回退，main 经
    /// [`crate::key::load_gateway_credential`] 装配——密钥文件
    /// `<imagent_home>/web-gateway-key` 缺省自动生成；`IMAGENT_WEB_GATEWAY_TOKEN`
    /// 为兼容回退）；`machine_id` 为本机 machine 身份（v1.1，main 经
    /// [`crate::resolve_machine_id`] 解析：config 显式值 / hostname 短名）；
    /// `default_session` 为无 convId 浏览器消息的落点（缺省 "default"）；
    /// `store` / `default_workdir` 为 v1.6 工作空间泵所需（store KV 句柄 +
    /// 快照 default 字段来源，main 装配处注入）。
    ///
    /// E2E 密钥（v1.4）锚定 `<imagent_home>/web-e2e-key`（main 不感知——
    /// E2E 是内容保密增强，装配失败不阻断网关启动，仅降级明文 + warn；
    /// 测试经 [`Self::new_with_e2e`] 注入临时路径）。
    pub fn new(
        relay_url: String,
        credential: GatewayCredential,
        default_session: String,
        machine_id: String,
        store: Store,
        default_workdir: PathBuf,
    ) -> Self {
        let e2e_key = imagent_core::paths::imagent_home().join("web-e2e-key");
        Self::new_with_e2e(
            relay_url,
            credential,
            default_session,
            machine_id,
            &e2e_key,
            store,
            default_workdir,
        )
    }

    /// [`Self::new`] 的全参形态（E2E 密钥路径可注入——集成测试用临时目录，
    /// 不落真实 `~/.imagent`）。
    pub fn new_with_e2e(
        relay_url: String,
        credential: GatewayCredential,
        default_session: String,
        machine_id: String,
        e2e_key_path: &std::path::Path,
        store: Store,
        default_workdir: PathBuf,
    ) -> Self {
        let (inbound_tx, inbound_rx) = mpsc::channel::<InboundMessage>(INBOUND_CHANNEL_CAPACITY);
        let (outbound_tx, outbound_rx) = mpsc::channel::<String>(OUTBOUND_CHANNEL_CAPACITY);
        let (sync_tx, sync_rx) = mpsc::channel::<()>(SYNC_CHANNEL_CAPACITY);
        let (workspace_tx, workspace_rx) =
            mpsc::channel::<InboundAction>(WORKSPACE_CHANNEL_CAPACITY);

        // E2E 身份装配（v1.4）：e2e_ack 签名复用 v1.3 认证 ed25519 私钥——
        // 仅 key 凭证形态可用；token 回退形态无签名密钥，E2E 不可用（出站
        // 恒明文 = v1.3 行为，浏览器侧 10s fail-closed 自行提示明文选择）。
        // 密钥文件损坏同理降级不阻断（认证不受影响——两把密钥独立）。
        let e2e = match &credential {
            GatewayCredential::Key(sk) => {
                match E2eIdentity::load_or_generate(e2e_key_path, sk.clone()) {
                    Ok(id) => Some(id),
                    Err(e) => {
                        warn!(
                            target: TARGET,
                            key_path = %e2e_key_path.display(),
                            error = %e,
                            "E2E 密钥装配失败，业务帧保持明文中继（网关认证不受影响）"
                        );
                        None
                    }
                }
            }
            GatewayCredential::Token(_) => {
                debug!(
                    target: TARGET,
                    "token 凭证（v1.2 兼容路径）无 ed25519 签名密钥，E2E 加密不可用（明文）"
                );
                None
            }
        };

        let reconnect = Arc::new(Notify::new());
        let online = Arc::new(AtomicBool::new(false));
        let shutdown = CancellationToken::new();

        // client 任务：连接立即开始（断线自动重连）。
        let client = WebClient {
            relay_url: relay_url.clone(),
            credential,
            machine_id: machine_id.clone(),
            default_session,
            online: online.clone(),
            e2e,
            inbound_tx,
            sync_tx,
            workspace_tx,
        };
        tokio::spawn(client.run(outbound_rx, reconnect.clone(), shutdown.clone()));

        // sync responder（D6 会话恢复）。
        let cards: Arc<Mutex<CardCache>> = Arc::new(Mutex::new(CardCache::default()));
        tokio::spawn(serve_sync_requests(
            sync_rx,
            machine_id.clone(),
            cards.clone(),
            outbound_tx.clone(),
            shutdown.clone(),
        ));

        // workspace 泵（v1.6 工作空间；出站帧带 machine，v1.1 语义）。
        tokio::spawn(serve_workspace_actions(
            workspace_rx,
            machine_id.clone(),
            store,
            default_workdir,
            outbound_tx.clone(),
            shutdown.clone(),
        ));

        Self {
            relay_url,
            machine_id,
            outbound_tx,
            reconnect,
            inbound_rx: Arc::new(AsyncMutex::new(inbound_rx)),
            online,
            cards,
            card_seq: Arc::new(AtomicU64::new(0)),
            pending_asks: Arc::new(Mutex::new(PendingAsks::default())),
            shutdown,
        }
    }

    /// relay 连接是否在线（doctor_probes / 测试用）。
    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// 本机 machine 身份（测试用）。
    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    /// 出站帧入队（§5.1 断线语义）：try_send——断线期帧排队（Ok），队列满 /
    /// client 退出才 Err，不挂起 dispatch。
    fn enqueue(&self, json: String) -> Result<()> {
        match self.outbound_tx.try_send(json) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(CoreError::Platform(
                PLATFORM,
                "出站队列满：relay 长时间不可达".into(),
            )),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(CoreError::Platform(
                PLATFORM,
                "出站通道已关闭（client 已退出）".into(),
            )),
        }
    }

    /// 登记一条 pending 审批（cancel_all 逐条收敛用）。
    fn track_ask(&self, conv: &str, request_id: &str) {
        lock_or_recover(&self.pending_asks)
            .entry(conv.to_string())
            .or_default()
            .push(request_id.to_string());
    }

    /// 移除 pending 记录（已 resolve / 已 cancel）。
    fn untrack_ask(&self, conv: &str, request_id: &str) {
        let mut map = lock_or_recover(&self.pending_asks);
        if let Some(list) = map.get_mut(conv) {
            list.retain(|r| r != request_id);
            if list.is_empty() {
                map.remove(conv);
            }
        }
    }
}

/// 平台 drop 即 cancel：后台 client / sync responder 任务感知退出，不再无限
/// 重连（wecom P3-g 同款停机通路——测试隔离与多 profile 热替换不泄漏旧连接
/// 循环）。
impl Drop for WebPlatform {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[async_trait]
impl Platform for WebPlatform {
    /// B1 能力声明：CARDS | ASK | RECONNECT，与覆写清单逐族对照：
    /// - CARDS：send_card（card 帧 + 缓存终态供 sync 重发）/ update_card
    ///   （card_patch 帧 + 同步更新缓存）✅。（supports_streaming_card 恒 true
    ///   是 per-conv 细化方法，非能力位——core CardSession 据它分支建会话。）
    /// - ASK：send_permission_ask（approval_request）/ resolve_permission_ask /
    ///   cancel_permission_ask / cancel_all_permission_asks（approval_resolved）
    ///   ✅。（note_queued_on_ask / send_permission_ask_text 不覆写，走 default
    ///   ——web 卡无 note 行需求 / 纯文本降级路径无需平台实现。）
    /// - RECONNECT：reconnect（Notify 唤醒 client task 重连）✅。
    /// - ❌ 不声明（走 default 降级）：COMMAND_CARDS / FORMS（纯文本降级对
    ///   本平台已够——SPA 的按钮在浏览器侧由 approval/command 帧渲染）、
    ///   REACTIONS / TYPING / URGENT_TEXT / GROUP_CHATS（无对应语义）；
    ///   MEDIA_UPLOAD：send_media 覆写但显式 Err（MVP 无媒体上行）——声明
    ///   必须与实际能力一致（wecom send_media 同款处理），不声明。
    /// 一致性测试 `web_caps_match_overrides` 钉住本值。
    fn capabilities(&self) -> PlatformCaps {
        PlatformCaps::CARDS | PlatformCaps::ASK | PlatformCaps::RECONNECT
    }

    fn name(&self) -> &'static str {
        PLATFORM
    }

    async fn recv(&self) -> Result<InboundMessage> {
        self.inbound_rx.lock().await.recv().await.ok_or_else(|| {
            CoreError::Platform(PLATFORM, "入站 channel 已关闭（client 已退出）".into())
        })
    }

    async fn send_text(&self, conv: &ConvId, text: &str, _hint: &ReplyHint) -> Result<()> {
        let json = encode_text_frame(&self.machine_id, &conv.0, text)?;
        self.enqueue(json)
    }

    async fn send_media(&self, _conv: &ConvId, _media: &MediaRef, _hint: &ReplyHint) -> Result<()> {
        // MVP 无媒体上行通道（relay 帧协议 v1 只有文本帧）。显式报错而非谎报
        // Ok——core 命令层（/img /file）会把文案回给用户。
        Err(CoreError::Platform(
            PLATFORM,
            "web 暂不支持媒体发送（M2 无媒体上行）".into(),
        ))
    }

    /// web 无 per-conv 差异：SPA 恒走流式卡片路径。
    fn supports_streaming_card(&self, _conv: &ConvId) -> bool {
        true
    }

    async fn send_card(
        &self,
        conv: &ConvId,
        card: &OutboundCard,
        _hint: &ReplyHint,
    ) -> Result<Option<String>> {
        let message_id = format!(
            "web-card-{}",
            self.card_seq.fetch_add(1, Ordering::Relaxed) + 1
        );
        let payload = CardPayload::from_card(card);
        let json = encode_card_frame(&self.machine_id, "card", &conv.0, &message_id, &payload)?;
        lock_or_recover(&self.cards).upsert(&conv.0, &message_id, payload);
        self.enqueue(json)?;
        Ok(Some(message_id))
    }

    async fn update_card(
        &self,
        conv: &ConvId,
        message_id: &str,
        card: &OutboundCard,
        _hint: &ReplyHint,
    ) -> Result<()> {
        let payload = CardPayload::from_card(card);
        let json = encode_card_frame(
            &self.machine_id,
            "card_patch",
            &conv.0,
            message_id,
            &payload,
        )?;
        // 缓存语义是「该 conv 最近卡片」，patch 后直接覆盖（message_id 不匹配
        // 的陈旧 patch 也覆盖为最新——终态以最后一次为准）。
        lock_or_recover(&self.cards).upsert(&conv.0, message_id, payload);
        self.enqueue(json)
    }

    async fn send_permission_ask(
        &self,
        conv: &ConvId,
        request_id: &str,
        tool_name: &str,
        input_summary: &str,
        _hint: &ReplyHint,
    ) -> Result<Option<String>> {
        // inputSummary 压成人可读单行（P8-1，与 feishu 审批卡同源）。
        let summary = imagent_core::render::tool_summary(tool_name, input_summary);
        let json = encode_approval_request_frame(
            &self.machine_id,
            &conv.0,
            request_id,
            tool_name,
            &summary,
        )?;
        self.track_ask(&conv.0, request_id);
        self.enqueue(json)?;
        // 返回 request_id 作 card_msg_id 锚点：浏览器审批回复带 requestId
        // （ask_req）精确路由，SPA 无「引用回复」形态，锚点即请求 id 本身。
        Ok(Some(request_id.to_string()))
    }

    async fn resolve_permission_ask(
        &self,
        conv: &ConvId,
        request_id: &str,
        reply: &PermissionReply,
    ) -> Result<()> {
        let decision = if reply.always {
            "always"
        } else if reply.allow {
            "allow"
        } else {
            "deny"
        };
        let json = encode_approval_resolved_frame(
            &self.machine_id,
            &conv.0,
            request_id,
            decision,
            reply.message.as_deref(),
        )?;
        self.untrack_ask(&conv.0, request_id);
        self.enqueue(json)
    }

    async fn cancel_permission_ask(&self, conv: &ConvId, request_id: &str) -> Result<()> {
        let json = encode_approval_resolved_frame(
            &self.machine_id,
            &conv.0,
            request_id,
            "cancelled",
            Some("审批已取消（超时或任务被中断）"),
        )?;
        self.untrack_ask(&conv.0, request_id);
        self.enqueue(json)
    }

    async fn cancel_all_permission_asks(&self, conv: &ConvId) -> Result<()> {
        // 该 conv 记录中的全部 pending 逐条发 approval_resolved(cancelled)。
        let drained = lock_or_recover(&self.pending_asks)
            .remove(&conv.0)
            .unwrap_or_default();
        for request_id in drained {
            let json = encode_approval_resolved_frame(
                &self.machine_id,
                &conv.0,
                &request_id,
                "cancelled",
                Some("审批已取消（任务被 /stop 中断）"),
            )?;
            self.enqueue(json)?;
        }
        Ok(())
    }

    /// 强制重连：notify_one 存 permit，client run task 的 select 消费后丢弃
    /// 连接 future 断开重连（退避重置为 1s）。
    async fn reconnect(&self) -> Result<()> {
        self.reconnect.notify_one();
        Ok(())
    }

    async fn doctor_probes(&self) -> Vec<String> {
        let state = if self.is_online() {
            "在线"
        } else {
            "重连中"
        };
        vec![format!(
            "✅ relay 连接：{state}（{}，machine={}）",
            self.relay_url, self.machine_id
        )]
    }

    /// v1.6 工作空间：终端 `/cd` 成功后实时推 conv_workdir 增量帧（同步
    /// best-effort——try_send 入队，队列满丢帧 warn，下条消息触发的快照
    /// 兜底；trait 约定本方法不得 IO/await）。帧带本机 machine（v1.1 出站
    /// 帧必带——SPA 增量合并按来源机器）。
    fn on_workdir_changed(&self, conv: &ConvId, path: &str) {
        match encode_conv_workdir_frame(&self.machine_id, &conv.0, path) {
            Ok(json) => {
                if let Err(e) = self.enqueue(json) {
                    warn!(
                        target: TARGET,
                        conv = %conv.0,
                        error = %e,
                        "conv_workdir 增量帧入队失败，丢弃（下条快照兜底）"
                    );
                }
            }
            Err(e) => warn!(
                target: TARGET,
                conv = %conv.0,
                error = %e,
                "conv_workdir 增量帧序列化失败"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    //! 逻辑测试不连真实 relay（地址用 loopback 拒连端口，后台任务退避空转，
    //! wecom 同款手法）；端到端行为见 tests/web_loopback.rs。

    use super::*;
    use imagent_core::{CardPhase, CardTerminal};

    /// 工作空间直测的 machine 断言锚点（出站帧必带，v1.1 语义）。
    const TEST_MACHINE: &str = "test-mac";

    /// 测试用临时 store（真实 sqlite，临时目录——工作空间泵的 KV 读写走
    /// 与生产同一条路径）。
    async fn test_store() -> Store {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "imagent-web-platform-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Store::open(&p).await.expect("open store")
    }

    /// 通过 validate_workdir 的测试目录（绝对 + is_dir + 非过宽）。
    fn valid_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("imagent-web-ws-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    async fn offline_platform() -> WebPlatform {
        // Token 凭证 → E2E 身份不装配（无文件 IO），new_with_e2e 的路径仅
        // 满足签名（避免 new 的缺省路径锚定真实 ~/.imagent）。
        let e2e_key = std::env::temp_dir().join("imagent-web-platform-e2e-unused");
        WebPlatform::new_with_e2e(
            "ws://127.0.0.1:1/gateway".into(),
            GatewayCredential::Token("unused-token".into()),
            "default".into(),
            "test-mac".into(),
            &e2e_key,
            test_store().await,
            valid_dir(),
        )
    }

    /// Drop 平台必须 cancel 停机信号（后台任务连动退出）。
    #[tokio::test]
    async fn drop_platform_cancels_shutdown_token() {
        let p = offline_platform().await;
        let token = p.shutdown.clone();
        assert!(!token.is_cancelled());
        drop(p);
        assert!(token.is_cancelled(), "Drop 必须 cancel 停机信号");
    }

    /// B1：caps 声明与覆写清单一致性（钉值防漂移，对照 capabilities() 旁注释）。
    #[tokio::test]
    async fn web_caps_match_overrides() {
        let p = offline_platform().await;
        assert_eq!(
            p.capabilities(),
            PlatformCaps::CARDS | PlatformCaps::ASK | PlatformCaps::RECONNECT,
            "web 声明 cards+ask+reconnect"
        );
        assert_eq!(p.name(), "web");
        assert!(p.supports_streaming_card(&ConvId("web:x".into())));
        // send_media 覆写为显式 Err → MEDIA_UPLOAD 不声明（覆写≠支持）。
        assert!(!p.capabilities().contains(PlatformCaps::MEDIA_UPLOAD));
        drop(p);
    }

    /// doctor_probes：离线（初始未连接）显示重连中；在线标志翻转后显示在线。
    #[tokio::test]
    async fn doctor_probes_reflect_online_state() {
        let p = offline_platform().await;
        let probes = p.doctor_probes().await;
        assert_eq!(probes.len(), 1);
        assert!(
            probes[0].contains("重连中") && probes[0].contains("ws://127.0.0.1:1/gateway"),
            "{}",
            probes[0]
        );
        // 直接翻 client 维护的标志（在线翻转路径由回环测试覆盖真连接）。
        p.online.store(true, Ordering::Relaxed);
        let probes = p.doctor_probes().await;
        assert!(probes[0].contains("在线"), "{}", probes[0]);
        drop(p);
    }

    /// send_media 显式 Err（不谎报 Ok）。
    #[tokio::test]
    async fn send_media_errors_explicitly() {
        let p = offline_platform().await;
        let e = p
            .send_media(
                &ConvId("web:x".into()),
                &MediaRef {
                    kind: "image".into(),
                    url: "/tmp/a.png".into(),
                },
                &ReplyHint::None,
            )
            .await
            .unwrap_err();
        assert!(format!("{e}").contains("媒体"), "{e}");
        drop(p);
    }

    // ---- v1.6 工作空间动作处理（handle_workspace_action 直测：store + 出站
    // channel 注入，不经网络；端到端路径见 tests/web_loopback.rs——含
    // on_workdir_changed 的 /cd 通知路径，帧在真实 relay 的浏览器端观测）----

    /// 处理一条动作并取回全部出站帧（解析为 Value 便于断言）。
    /// machine 固定 TEST_MACHINE（出站帧 machine 断言锚点）。
    async fn run_action(
        store: &Store,
        default: &Path,
        act: InboundAction,
    ) -> Vec<serde_json::Value> {
        let (tx, mut rx) = mpsc::channel::<String>(32);
        handle_workspace_action(TEST_MACHINE, store, default, act, &tx).await;
        let mut out = Vec::new();
        while let Ok(json) = rx.try_recv() {
            out.push(serde_json::from_str(&json).unwrap());
        }
        out
    }

    /// conv_workdir_set 校验链（任务说明验收项）：非绝对路径 / 目录不存在 /
    /// 过宽目录（/、home 根）→ error 帧 code=workspace_invalid 且
    /// `workdir:<conv>` 未写入 + 照常回快照（快照不变）。
    #[tokio::test]
    async fn conv_workdir_set_validation_chain_rejects() {
        let store = test_store().await;
        let default = valid_dir();
        let home = std::env::var("HOME").expect("测试环境应有 HOME");
        for bad in ["relative/x", "/definitely/not/exist/dir", "/", &home] {
            let frames = run_action(
                &store,
                &default,
                InboundAction::ConvWorkdirSet {
                    conv: "web:work".into(),
                    path: bad.to_string(),
                },
            )
            .await;
            let err = frames
                .iter()
                .find(|f| f["type"] == "error")
                .unwrap_or_else(|| panic!("{bad} 应回 error 帧：{frames:?}"));
            assert_eq!(err["code"], WORKSPACE_ERROR_CODE, "{bad}：{err}");
            assert_eq!(
                err["machine"], TEST_MACHINE,
                "{bad}：error 帧带本机 machine"
            );
            assert!(
                err["message"].as_str().is_some_and(|m| !m.is_empty()),
                "message 须为非空人话：{err}"
            );
            // 快照照常回（SPA 单一数据来源），但 workdir KV 未写入。
            assert!(
                frames.iter().any(|f| f["type"] == "workspaces"),
                "{bad} 失败也应回快照：{frames:?}"
            );
            assert!(
                store
                    .get_config(&workdir_key("web:work"))
                    .await
                    .unwrap()
                    .is_none(),
                "{bad} 不得写入 workdir:<conv>"
            );
            // conv_workdir 增量帧不出现（校验失败）。
            assert!(
                !frames.iter().any(|f| f["type"] == "conv_workdir"),
                "{bad} 校验失败不得推增量帧"
            );
        }
    }

    /// conv_workdir_set 成功链（任务说明验收项）：写 `workdir:<conv>`（与
    /// /cd 同键）；命中空间 path 时 bump lastUsedAt；额外推 conv_workdir
    /// 增量帧 + workspaces 快照（快照含 local: conv、含 legacy 裸路径值的
    /// 兼容读）。
    #[tokio::test]
    async fn conv_workdir_set_success_writes_bumps_and_replies() {
        let store = test_store().await;
        let default = valid_dir();
        let dir = valid_dir();
        // 种子：一个新格式空间（path 命中 dir，无 lastUsedAt）+ 一个旧格式
        // 裸路径空间（存量部署兼容读）+ 一条 local: conv 的 workdir（/cd 写）。
        store
            .set_config(
                &workspace_key("proj"),
                &WorkspaceRecord::new(dir.to_string_lossy()).encode(),
            )
            .await
            .unwrap();
        store
            .set_config(&workspace_key("legacy"), "/tmp/imagent-legacy-ws")
            .await
            .unwrap();
        store
            .set_config(&workdir_key("local:work"), "/tmp/imagent-local-ws")
            .await
            .unwrap();

        let frames = run_action(
            &store,
            &default,
            InboundAction::ConvWorkdirSet {
                conv: "local:work".into(),
                path: dir.to_string_lossy().into_owned(),
            },
        )
        .await;

        // KV：与 /cd 同键写入。
        assert_eq!(
            store
                .get_config(&workdir_key("local:work"))
                .await
                .unwrap()
                .as_deref(),
            Some(dir.to_string_lossy().as_ref()),
            "写 workdir:<conv>（裸路径，与 /cd 同形态）"
        );
        // bump：proj 的 lastUsedAt 出现。
        let bumped = WorkspaceRecord::decode(
            &store
                .get_config(&workspace_key("proj"))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(bumped.last_used_at.is_some(), "命中空间 path 须 bump");
        // 出站：增量帧 + 快照（均带 machine，v1.1 出站帧必带——SPA 按其
        // 归属机器）。
        let delta = frames.iter().find(|f| f["type"] == "conv_workdir").unwrap();
        assert_eq!(delta["convId"], "local:work");
        assert_eq!(delta["machine"], TEST_MACHINE, "增量帧带本机 machine");
        assert_eq!(delta["workdir"], dir.to_string_lossy().as_ref());
        let snap = frames.iter().find(|f| f["type"] == "workspaces").unwrap();
        assert_eq!(snap["machine"], TEST_MACHINE, "快照帧带本机 machine");
        assert_eq!(snap["default"], default.to_string_lossy().as_ref());
        // 快照 workspaces：name 稳定序 + legacy 兼容读（裸路径 + lastUsedAt 缺席）。
        let ws = snap["workspaces"].as_array().unwrap();
        assert_eq!(ws.len(), 2);
        assert_eq!(ws[0]["name"], "legacy", "name 稳定序（legacy < proj）");
        assert!(
            ws[0].get("lastUsedAt").is_none(),
            "旧值读出 lastUsedAt 缺席"
        );
        assert_eq!(ws[0]["path"], "/tmp/imagent-legacy-ws");
        assert_eq!(ws[1]["name"], "proj");
        assert!(ws[1]["lastUsedAt"].is_u64(), "bump 后快照携带 lastUsedAt");
        // 快照 convs：含 local: 会话。
        assert_eq!(snap["convs"]["local:work"], dir.to_string_lossy().as_ref());
    }

    /// workspace_save / remove / list：非法 name（空白/超长）与非法 path →
    /// error 帧；合法保存落库新 JSON 格式；remove 未命中幂等（仅快照，无
    /// error）；list 仅回快照。每帧必有快照。
    #[tokio::test]
    async fn workspace_save_remove_list_flow() {
        let store = test_store().await;
        let default = valid_dir();
        let dir = valid_dir();

        // 非法 name：空白 / 超 32 char。
        for bad_name in ["   ", &"x".repeat(33)] {
            let frames = run_action(
                &store,
                &default,
                InboundAction::WorkspaceSave {
                    name: bad_name.to_string(),
                    path: dir.to_string_lossy().into_owned(),
                },
            )
            .await;
            let err = frames.iter().find(|f| f["type"] == "error").unwrap();
            assert_eq!(err["code"], WORKSPACE_ERROR_CODE);
            assert!(
                store
                    .list_config(WORKSPACE_PREFIX)
                    .await
                    .unwrap()
                    .is_empty(),
                "非法 name 不得落库"
            );
        }

        // 合法保存（trim 生效：` proj ` → `proj`）。
        let frames = run_action(
            &store,
            &default,
            InboundAction::WorkspaceSave {
                name: " proj ".into(),
                path: dir.to_string_lossy().into_owned(),
            },
        )
        .await;
        assert!(
            !frames.iter().any(|f| f["type"] == "error"),
            "合法保存不应有 error：{frames:?}"
        );
        let raw = store
            .get_config(&workspace_key("proj"))
            .await
            .unwrap()
            .unwrap();
        let rec = WorkspaceRecord::decode(&raw).unwrap();
        assert_eq!(rec.path, dir.to_string_lossy().as_ref());
        assert_eq!(rec.last_used_at, None, "刚保存 lastUsedAt 缺席");
        assert!(
            frames.iter().any(|f| f["type"] == "workspaces"),
            "每帧回快照"
        );

        // remove：命中删除；再次 remove（未命中）幂等——只回快照无 error。
        let frames = run_action(
            &store,
            &default,
            InboundAction::WorkspaceRemove {
                name: "proj".into(),
            },
        )
        .await;
        assert!(!frames.iter().any(|f| f["type"] == "error"));
        assert!(store
            .get_config(&workspace_key("proj"))
            .await
            .unwrap()
            .is_none());
        let frames = run_action(
            &store,
            &default,
            InboundAction::WorkspaceRemove {
                name: "proj".into(),
            },
        )
        .await;
        assert!(
            !frames.iter().any(|f| f["type"] == "error"),
            "未命中 remove 幂等：{frames:?}"
        );
        assert!(frames.iter().any(|f| f["type"] == "workspaces"));

        // list：空表也回快照（default + 空集合）。
        let frames = run_action(&store, &default, InboundAction::WorkspaceList).await;
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["type"], "workspaces");
        assert_eq!(frames[0]["workspaces"].as_array().unwrap().len(), 0);
    }

    /// 卡片缓存：上限 32 conv 淘汰最旧；快照按 seq 旧→新排序。
    #[test]
    fn card_cache_evicts_oldest_and_snapshots_in_order() {
        let mut cache = CardCache::default();
        let card = || {
            CardPayload::from_card(&OutboundCard {
                text: "t".into(),
                tool_calls: vec![],
                thoughts: vec![],
                todos: vec![],
                phase: CardPhase::Thinking,
                queued_hint: None,
                run_secs: 0,
                usage_display: None,
                task_digest: None,
                terminal: CardTerminal::Running,
            })
        };
        for i in 0..=CARD_CACHE_MAX_CONVS {
            cache.upsert(&format!("web:c{i}"), &format!("web-card-{i}"), card());
        }
        assert_eq!(cache.entries.len(), CARD_CACHE_MAX_CONVS, "超限淘汰");
        assert!(!cache.entries.contains_key("web:c0"), "最旧（c0）被淘汰");
        let snapshot = cache.snapshot();
        let convs: Vec<&str> = snapshot.iter().map(|(c, _, _)| c.as_str()).collect();
        assert_eq!(convs.len(), CARD_CACHE_MAX_CONVS);
        // seq 升序：c1 在 c2 前。
        assert!(
            convs.iter().position(|c| *c == "web:c1").unwrap()
                < convs.iter().position(|c| *c == "web:c2").unwrap()
        );
        // 同 conv 二次 upsert 覆盖（不涨条目）。
        cache.upsert("web:c1", "web-card-again", card());
        assert_eq!(cache.entries.len(), CARD_CACHE_MAX_CONVS);
        assert_eq!(cache.entries.get("web:c1").unwrap().1, "web-card-again");
    }
}
