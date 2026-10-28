//! `imagent-relay`：Web 驾驶舱 M1——ECS 侧无状态 WS 帧中继
//! （设计见 docs/WEB_DESIGN.md §2/§3.1，规格以任务评审修订版为准）。
//!
//! 拓扑：`浏览器 WS ⇄ relay ⇄ 网关 WS`，两条连接都是客户端模式连到本进程
//! （Mac 网关零入站端口）。本 crate 是中继：**不理解任何业务语义**，
//! 只做认证（双 token、fail-closed）、转发（Hub 状态机）、网关离线缓冲
//! （200 帧 / 1MB）、每连接限流（20 帧 / 10s）。部署形态：ECS systemd
//! 服务绑 127.0.0.1:3010，公网由 Caddy（8443，TLS 终结）`handle_path
//! /imagent/*` strip 前缀后反代——relay 看到的路径已是 strip 后的 `/xxx`。
//!
//! 分层（任务说明 §4.7）：核心状态机全部在本 lib（可单测 + 集成测试直接
//! [`spawn_relay`] 起真 server），`main.rs` 只做 env / bind / tracing 组装。

#![deny(unsafe_code)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures::StreamExt;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

/// 统一 tracing target（仓库纪律）。
const TARGET: &str = "imagent::relay";

/// 认证前等待 hello 首帧的超时（§4.2）：hello 迟迟不来直接 close，
/// 防止未认证连接长期占用资源。
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// 认证失败后的关停延迟（§4.2）：给爆破加节奏（1s/次探测）。
const AUTH_FAIL_DELAY: Duration = Duration::from_secs(1);
/// 认证后空闲超时（§4.4）：90s 未收到任何 WS message（含 pong）→ close 1000。
/// 保活 ping 由客户端负责（网关 30s ping，浏览器发 ping 文本帧——端到端
/// 语义，relay 透传，自己不做应用层应答）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// 限流窗口（§4.4）：任意 10s 滑动窗口内 ≤ [`RATE_MAX_FRAMES`] 帧。
const RATE_WINDOW: Duration = Duration::from_secs(10);
const RATE_MAX_FRAMES: usize = 20;
/// 网关离线缓冲上限（§4.3）：帧数 / 字节数先到为准。
const BUF_MAX_FRAMES: usize = 200;
const BUF_MAX_BYTES: usize = 1024 * 1024;
/// 单 WS message / 单 frame 上限 256KB（§4.4）：超限 axum 自会以 1009 断开。
const MAX_WS_MESSAGE_BYTES: usize = 256 * 1024;
/// 每连接出站 channel 容量。上限推导：hello_ack + machine 快照 + 网关离线
/// 缓冲全量重放（≤200 帧）必须在**进入消费循环之前**一次性入队也不阻塞，
/// 故 ≥202；取 512 留余量。业务帧转发走 try_send（打满即丢，见 [`try_send`]），
/// 连接之间不存在互相 await 的投递环，不会死锁。
const CHANNEL_CAPACITY: usize = 512;

/// WS close code（RFC 6455 + 私有码，§4.2/§4.3/§4.5）。
const CLOSE_POLICY: u16 = 1008; // 认证失败 / 协议违规（binary、非 hello 首帧）/ 限流
const CLOSE_NORMAL: u16 = 1000; // idle 超时正常关闭
const CLOSE_GOING_AWAY: u16 = 1001; // 服务停机
const CLOSE_SUPERSEDED: u16 = 4001; // 同端点新连接顶替（踢旧）

// ---------------------------------------------------------------------------
// 配置与 fail-closed 校验
// ---------------------------------------------------------------------------

/// relay 运行配置（token 对）。main 从 env 读取；测试直接构造。
#[derive(Clone)]
pub struct RelayConfig {
    /// 网关侧认证 token（`IMAGENT_RELAY_GATEWAY_TOKEN`）。
    pub gateway_token: String,
    /// 浏览器侧认证 token（`IMAGENT_RELAY_USER_TOKEN`）。
    pub user_token: String,
}

impl RelayConfig {
    /// fail-closed 校验（§4.2，纯函数便于单测）：任一 token 缺失/空 → 拒
    /// （不提供无认证模式）；两 token 相同 → 拒（职责分离——拿到一个 token
    /// 不能同时扮演网关与浏览器两端）。
    pub fn validate(&self) -> Result<(), String> {
        if self.gateway_token.trim().is_empty() {
            return Err("网关 token 缺失或为空：必填 IMAGENT_RELAY_GATEWAY_TOKEN\
                 （openssl rand -hex 32 生成；relay 不提供无认证模式）"
                .into());
        }
        if self.user_token.trim().is_empty() {
            return Err("浏览器 token 缺失或为空：必填 IMAGENT_RELAY_USER_TOKEN\
                 （openssl rand -hex 32 生成）"
                .into());
        }
        if self.gateway_token == self.user_token {
            return Err(
                "IMAGENT_RELAY_GATEWAY_TOKEN 与 IMAGENT_RELAY_USER_TOKEN 不得相同：\
                 职责分离，防止拿到一个 token 就能扮演网关与浏览器两端"
                    .into(),
            );
        }
        Ok(())
    }
}

/// §4.5 fail-closed 绑定校验（哲学对齐 `src/webhook.rs` 的
/// `validate_webhook_bind`，纯函数便于单测）：relay 必须绑 loopback——公网
/// 入口只允许 Caddy（TLS 终结 + 统一暴露面），直接对外绑定等于绕过 TLS
/// 与 Caddy 层防护。给出可读错误说明原因。
pub fn validate_bind(addr: SocketAddr) -> Result<(), String> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    Err(format!(
        "IMAGENT_RELAY_BIND={addr} 非 loopback 地址：relay 只允许绑 127.0.0.1（或 [::1]），\
         公网入口必须由 Caddy（8443，TLS 终结）反代；请改回 loopback 绑定"
    ))
}

/// token 日志脱敏（§4.5 日志纪律：绝不 log 完整 token，只露前 4 位）。
pub fn mask_token(token: &str) -> String {
    format!("{}…", token.chars().take(4).collect::<String>())
}

/// 常数时间比较（防时序侧信道）。写法对齐 `src/webhook.rs`
/// `verify_webhook_signature` 尾部的 XOR 累计——不引入 subtle 依赖的
/// 仓库既有先例：长度差异单独进 diff，字节差异逐位 OR 累计。
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let n = a.len().max(b.len());
    let mut diff: u8 = (a.len() != b.len()) as u8;
    for i in 0..n {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// hello 首帧解析（§4.2 主认证形态）
// ---------------------------------------------------------------------------

/// hello 首帧反序列化形状：`{"v":1,"type":"hello","token":"..."}`。
#[derive(Deserialize)]
struct HelloFrame {
    v: i64,
    #[serde(rename = "type")]
    kind: String,
    token: String,
}

/// hello 解析失败原因（只为日志可读；对外行为一致——close 1008）。
#[derive(Debug, PartialEq, Eq)]
enum HelloParseError {
    /// JSON 畸形 / 缺字段 / token 非字符串 / token 为空串。
    Malformed,
    /// 首帧 type 不是 hello。
    NotHello,
    /// 信封版本不为 1。
    BadVersion,
}

/// 解析首帧 hello（纯函数便于单测）。空 token 判 Malformed：空串不可能与
/// 合法配置 token 相等，提前拒绝让语义更明确。
fn parse_hello(text: &str) -> Result<String, HelloParseError> {
    let h: HelloFrame = serde_json::from_str(text).map_err(|_| HelloParseError::Malformed)?;
    if h.v != 1 {
        return Err(HelloParseError::BadVersion);
    }
    if h.kind != "hello" {
        return Err(HelloParseError::NotHello);
    }
    if h.token.is_empty() {
        return Err(HelloParseError::Malformed);
    }
    Ok(h.token)
}

// ---------------------------------------------------------------------------
// 滑动窗口限流（§4.4，纯函数可拨钟）
// ---------------------------------------------------------------------------

/// 每连接滑动窗口限流：任意 [`RATE_WINDOW`] 内 ≤ [`RATE_MAX_FRAMES`] 帧。
/// 时间由调用方注入（单测可拨钟）；被拒的帧不记录——拒绝即断连，且不应
/// 延长窗口记忆。
#[derive(Default)]
struct RateWindow {
    events: VecDeque<Instant>,
}

impl RateWindow {
    /// 记录一帧并判定放行：先滑出窗口外（elapsed ≥ 窗口长）的旧事件再计数。
    fn check_and_record(&mut self, now: Instant) -> bool {
        while let Some(&oldest) = self.events.front() {
            if now.duration_since(oldest) < RATE_WINDOW {
                break;
            }
            self.events.pop_front();
        }
        if self.events.len() >= RATE_MAX_FRAMES {
            return false;
        }
        self.events.push_back(now);
        true
    }
}

// ---------------------------------------------------------------------------
// 网关离线缓冲（§4.3）
// ---------------------------------------------------------------------------

/// 网关离线缓冲：FIFO，[`BUF_MAX_FRAMES`] 帧 / [`BUF_MAX_BYTES`] 字节先到
/// 为准，溢出拒新（丢新保旧——保证重放顺序完整）。字节按 UTF-8 计。
#[derive(Default)]
struct OfflineBuffer {
    frames: VecDeque<String>,
    bytes: usize,
}

impl OfflineBuffer {
    /// 入缓冲：帧数已满或加入后超字节上限 → false（不入，调用方向该
    /// browser 回 gateway_offline error）。
    fn push(&mut self, text: &str) -> bool {
        if self.frames.len() >= BUF_MAX_FRAMES {
            return false;
        }
        if self.bytes + text.len() > BUF_MAX_BYTES {
            return false;
        }
        self.frames.push_back(text.to_string());
        self.bytes += text.len();
        true
    }

    /// 取走全部缓冲（网关上线按序重放用），计数清零。
    fn drain(&mut self) -> Vec<String> {
        self.bytes = 0;
        self.frames.drain(..).collect()
    }
}

// ---------------------------------------------------------------------------
// Hub 状态机（§4.3）
// ---------------------------------------------------------------------------

/// 连接角色：决定槽位、认证 token 与转发方向。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Gateway,
    Browser,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Gateway => "gateway",
            Role::Browser => "browser",
        }
    }

    fn token(self, cfg: &RelayConfig) -> &str {
        match self {
            Role::Gateway => &cfg.gateway_token,
            Role::Browser => &cfg.user_token,
        }
    }
}

/// relay 自产帧（§4.3 行为 7：除此之外 relay 不解析、不改写、不记录任何
/// 业务帧内容——machine/error 之外 relay 对帧是「盲」的）。
fn hello_ack_frame(role: Role) -> Message {
    Message::text(serde_json::json!({"v": 1, "type": "hello_ack", "role": role.name()}).to_string())
}

fn machine_frame(online: bool) -> Message {
    Message::text(serde_json::json!({"v": 1, "type": "machine", "online": online}).to_string())
}

fn error_frame(code: &str) -> Message {
    Message::text(serde_json::json!({"v": 1, "type": "error", "code": code}).to_string())
}

/// 出站投递项：业务帧 / 主动 close（code+reason）。
/// close 必须走 channel 而非直接操作 socket：socket 由连接任务独占，而
/// 踢旧（4001）与服务停机（1001）都发生在其它任务的上下文里。
#[derive(Debug)]
enum Outbound {
    Msg(Message),
    Close(u16, &'static str),
}

/// Hub 槽位：指向某条连接的出站 channel + 连接身份（用于踢旧后旧连接
/// 退出时不误清新连接的槽位）。
struct Endpoint {
    conn_id: u64,
    tx: mpsc::Sender<Outbound>,
}

struct HubState {
    gateway: Option<Endpoint>,
    browser: Option<Endpoint>,
    buf: OfflineBuffer,
}

/// 进程级单例 Hub（Arc 共享，每连接 Clone）。锁纪律（任务说明 §4.3）：
/// std Mutex 保护纯内存临界区，所有方法「锁内计算 → clone sender 出锁 →
/// 锁外投递」，绝不跨 await 持锁。
#[derive(Clone)]
struct Hub {
    state: Arc<Mutex<HubState>>,
    conn_ids: Arc<AtomicU64>,
    shutdown: watch::Receiver<bool>,
}

/// 网关注册结果：锁外要做的副作用（踢旧 / 通知 browser / 重放缓冲）。
struct GatewayRegistered {
    superseded: Option<mpsc::Sender<Outbound>>,
    browser: Option<mpsc::Sender<Outbound>>,
    replay: Vec<String>,
}

/// browser 注册结果：被顶替的旧连接 + machine 快照（online 依 gateway 槽位）。
struct BrowserRegistered {
    superseded: Option<mpsc::Sender<Outbound>>,
    machine_online: bool,
}

/// browser 业务帧的路向（§4.3 行为 4）。
#[derive(Debug)]
enum BrowserRoute {
    /// 网关在线 → 透传（携带网关出站 channel）。
    Deliver(mpsc::Sender<Outbound>),
    /// 网关离线 → 已入缓冲。
    Buffered,
    /// 网关离线且缓冲满 → 不入，调用方向该 browser 回 gateway_offline error。
    OfflineFull,
}

impl Hub {
    fn new(shutdown: watch::Receiver<bool>) -> Self {
        Self {
            state: Arc::new(Mutex::new(HubState {
                gateway: None,
                browser: None,
                buf: OfflineBuffer::default(),
            })),
            conn_ids: Arc::new(AtomicU64::new(1)),
            shutdown,
        }
    }

    /// 中毒（临界区 panic）时取回数据继续服务：纯内存临界区无 IO 副作用
    /// 可回滚，与 `src/webhook.rs` 的限速桶/去重表同款取舍。
    fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next_conn_id(&self) -> u64 {
        self.conn_ids.fetch_add(1, Ordering::Relaxed)
    }

    fn shutdown_subscribe(&self) -> watch::Receiver<bool> {
        self.shutdown.clone()
    }

    /// 网关注册（§4.3 行为 1）：顶替旧槽位（踢旧）、取在线 browser 的
    /// 投递口、drain 离线缓冲（重放顺序 = 入队顺序）。
    fn register_gateway(&self, conn_id: u64, tx: mpsc::Sender<Outbound>) -> GatewayRegistered {
        let mut st = self.lock();
        let superseded = st.gateway.replace(Endpoint { conn_id, tx }).map(|e| e.tx);
        let browser = st.browser.as_ref().map(|e| e.tx.clone());
        let replay = st.buf.drain();
        GatewayRegistered {
            superseded,
            browser,
            replay,
        }
    }

    /// browser 注册（§4.3 行为 3）：顶替旧槽位（踢旧）+ 当前 machine 快照。
    fn register_browser(&self, conn_id: u64, tx: mpsc::Sender<Outbound>) -> BrowserRegistered {
        let mut st = self.lock();
        let superseded = st.browser.replace(Endpoint { conn_id, tx }).map(|e| e.tx);
        BrowserRegistered {
            superseded,
            machine_online: st.gateway.is_some(),
        }
    }

    /// browser→gateway 帧路向（§4.3 行为 4）：在线透传；离线入缓冲；
    /// 缓冲满拒新（丢新保旧）。
    fn route_browser_frame(&self, text: &str) -> BrowserRoute {
        let mut st = self.lock();
        if let Some(g) = st.gateway.as_ref() {
            return BrowserRoute::Deliver(g.tx.clone());
        }
        if st.buf.push(text) {
            BrowserRoute::Buffered
        } else {
            BrowserRoute::OfflineFull
        }
    }

    /// gateway→browser 帧路向（§4.3 行为 5）：browser 离线直接丢弃，不缓存
    /// ——会话恢复走网关侧重发机制，不归 relay 管（docs/WEB_DESIGN.md D6）。
    fn route_gateway_frame(&self) -> Option<mpsc::Sender<Outbound>> {
        let st = self.lock();
        st.browser.as_ref().map(|e| e.tx.clone())
    }

    /// 连接退出清槽位（§4.3 行为 2）：仅当槽位仍指向本连接（conn_id 相同）
    /// 才清——踢旧后旧连接退出不得误清新连接。网关清槽位同时向 browser
    /// 推 machine offline（锁内取投递口，锁外投递）。
    fn release(&self, role: Role, conn_id: u64) {
        let notify_offline = {
            let mut st = self.lock();
            let slot = match role {
                Role::Gateway => &mut st.gateway,
                Role::Browser => &mut st.browser,
            };
            if slot.as_ref().is_none_or(|e| e.conn_id != conn_id) {
                return;
            }
            *slot = None;
            (role == Role::Gateway)
                .then(|| st.browser.as_ref().map(|e| e.tx.clone()))
                .flatten()
        };
        if let Some(b) = notify_offline {
            try_send(&b, Outbound::Msg(machine_frame(false)));
        }
    }

    /// 服务停机：向两条在线连接发 Close（§4.5 graceful shutdown，1001）。
    fn close_all(&self, code: u16, reason: &'static str) {
        let senders = {
            let st = self.lock();
            [st.gateway.as_ref(), st.browser.as_ref()]
                .into_iter()
                .flatten()
                .map(|e| e.tx.clone())
                .collect::<Vec<_>>()
        };
        for tx in senders {
            try_send(&tx, Outbound::Close(code, reason));
        }
    }
}

/// best-effort 投递：channel 打满 / 接收端已撤 = 对端消费停滞，丢弃并记
/// 日志。业务层有心跳与重发兜底（§4.3 行为 5），中继不做交付保证；
/// try_send 让连接之间不存在互相 await 的投递环，杜绝死锁。
fn try_send(tx: &mpsc::Sender<Outbound>, item: Outbound) -> bool {
    match tx.try_send(item) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Closed(_)) => false,
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!(target: TARGET, "出站 channel 打满，帧被丢弃（对端消费停滞）");
            false
        }
    }
}

/// close 帧构造（axum 的 CloseCode 即 u16 类型别名）。
fn close_msg(code: u16, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }))
}

// ---------------------------------------------------------------------------
// HTTP 路由（§4.1；relay 看到的是 Caddy strip /imagent 后的路径）
// ---------------------------------------------------------------------------

/// axum 共享状态（每连接 Clone；Arc 内部共享同一 Hub / 配置）。
#[derive(Clone)]
struct RelayState {
    hub: Hub,
    config: Arc<RelayConfig>,
}

/// SPA 静态资源（M1 占位 index.html；M3 构建产物落入 `static/` 后零改动挂载）。
#[derive(rust_embed::RustEmbed)]
#[folder = "static/"]
struct Spa;

/// GET /gateway：网关侧 WS 端点（Upgrade）。
async fn gateway_ws(
    State(st): State<RelayState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    ws_endpoint(st, Role::Gateway, headers, ws).await
}

/// GET /ws：浏览器侧 WS 端点（Upgrade）。
async fn browser_ws(
    State(st): State<RelayState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    ws_endpoint(st, Role::Browser, headers, ws).await
}

/// 两端点公共入口：套上 256KB message/frame 上限（§4.4）后升级。
async fn ws_endpoint(
    st: RelayState,
    role: Role,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    ws.max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_connection(socket, st, role, headers))
}

/// GET /healthz：健康检查，200 空体。
async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// 静态 SPA fallback（§4.1）：`/` 与所有未匹配路径。缓存头规则：`/assets/`
/// 前缀视为带 hash 资源 → 长缓存 immutable；其余（index.html 与 SPA 客户端
/// 路由回落）一律 no-store——无 hash 文件名的内容随时会变，不可缓存。
async fn spa_fallback(uri: Uri) -> Response {
    const NO_STORE: &str = "no-store";
    const IMMUTABLE: &str = "public, max-age=31536000, immutable";
    let path = uri.path().trim_start_matches('/');
    let (file, cache, ctype_of) = if path.is_empty() {
        (Spa::get("index.html"), NO_STORE, "index.html")
    } else if path.starts_with("assets/") {
        (Spa::get(path), IMMUTABLE, path)
    } else {
        match Spa::get(path) {
            Some(f) => (Some(f), NO_STORE, path),
            // 未命中的路径回落 index.html：SPA 客户端路由（/session/xxx 等）
            // 需要拿到同一个 app 入口。
            None => (Spa::get("index.html"), NO_STORE, "index.html"),
        }
    };
    match file {
        Some(f) => (
            [
                (header::CACHE_CONTROL, cache),
                (header::CONTENT_TYPE, content_type(ctype_of)),
            ],
            f.data.into_owned(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "static assets missing\n").into_response(),
    }
}

/// 静态资源 Content-Type（M1 只有 html；扩展映射为 M3 真 SPA 备好）。
fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "json" => "application/json",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------------------
// WS 连接生命周期
// ---------------------------------------------------------------------------

/// `Authorization: Bearer <token>` 提取（§4.2 形态 2，网关 Rust 客户端的
/// 便利形态；浏览器唯一可用形态是首帧 hello，但本端点不歧视来源）。
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// 认证失败路径（§4.2）：sleep 1s（防爆破节奏）后 close 1008。
/// 日志只记角色与原因，绝不记 token。
async fn auth_reject(socket: &mut WebSocket, role: Role, reason: &'static str) {
    tracing::warn!(target: TARGET, role = role.name(), reason, "认证失败，延迟关闭");
    tokio::time::sleep(AUTH_FAIL_DELAY).await;
    let _ = socket.send(close_msg(CLOSE_POLICY, "auth failed")).await;
}

/// 向自身 channel 顺序投递（连接尚未进入消费循环）。容量推导见
/// [`CHANNEL_CAPACITY`]：hello_ack + machine 快照 + ≤200 重放帧恒不阻塞。
async fn send_all(tx: &mpsc::Sender<Outbound>, items: impl IntoIterator<Item = Outbound>) {
    for item in items {
        if tx.send(item).await.is_err() {
            break;
        }
    }
}

/// 单条 WS 连接完整生命周期：认证（hello / Bearer 直通）→ 注册 Hub →
/// 转发主循环（限流 + idle 超时 + 停机响应）→ 退出清槽位。
async fn handle_connection(mut socket: WebSocket, st: RelayState, role: Role, headers: HeaderMap) {
    let expected = role.token(&st.config);

    // ---- 认证（§4.2，fail-closed）----
    // Bearer 正确 → 直进转发态；Bearer 带了但不正确 → 失败路径（不回落
    // hello：发起方已表明认证意图，回落只会让探测多一种节奏）。
    match bearer_token(&headers) {
        Some(t) if constant_time_eq(t, expected) => {}
        Some(_) => {
            auth_reject(&mut socket, role, "Authorization Bearer token 不匹配").await;
            return;
        }
        None => {
            // hello 首帧认证：10s 内必须是 `{"v":1,"type":"hello","token":...}`。
            match tokio::time::timeout(AUTH_TIMEOUT, socket.next()).await {
                Err(_) => {
                    tracing::warn!(target: TARGET, role = role.name(), "认证超时（10s 无 hello），关闭");
                    let _ = socket.send(close_msg(CLOSE_POLICY, "auth timeout")).await;
                    return;
                }
                Ok(None) | Ok(Some(Err(_))) => return, // 连接已断 / 协议错误
                Ok(Some(Ok(msg))) => {
                    let ok = match msg {
                        Message::Text(t) => match parse_hello(t.as_str()) {
                            Ok(tok) => constant_time_eq(&tok, expected),
                            Err(_) => false,
                        },
                        _ => false, // 首帧必须是文本 hello
                    };
                    if !ok {
                        auth_reject(&mut socket, role, "首帧不是合法 hello（token 不匹配/畸形）")
                            .await;
                        return;
                    }
                }
            }
        }
    }

    // ---- 注册与自产帧（§4.3 行为 1/3）----
    let conn_id = st.hub.next_conn_id();
    let (tx, mut rx) = mpsc::channel::<Outbound>(CHANNEL_CAPACITY);
    let mut shutdown = st.hub.shutdown_subscribe();

    match role {
        Role::Gateway => {
            let reg = st.hub.register_gateway(conn_id, tx.clone());
            let replayed = reg.replay.len();
            if let Some(old) = reg.superseded {
                tracing::info!(target: TARGET, "踢旧：新网关连接顶替旧连接（close 4001 superseded）");
                try_send(&old, Outbound::Close(CLOSE_SUPERSEDED, "superseded"));
            }
            // hello_ack 先于重放帧：客户端先确认角色与认证结果，再消费重放。
            let mut init = vec![Outbound::Msg(hello_ack_frame(role))];
            init.extend(
                reg.replay
                    .into_iter()
                    .map(|f| Outbound::Msg(Message::text(f))),
            );
            send_all(&tx, init).await;
            if let Some(b) = reg.browser {
                try_send(&b, Outbound::Msg(machine_frame(true)));
            }
            tracing::info!(target: TARGET, replayed, "gateway 连接（认证通过）");
        }
        Role::Browser => {
            let reg = st.hub.register_browser(conn_id, tx.clone());
            if let Some(old) = reg.superseded {
                tracing::info!(target: TARGET, "踢旧：新浏览器连接顶替旧连接（close 4001 superseded）");
                try_send(&old, Outbound::Close(CLOSE_SUPERSEDED, "superseded"));
            }
            // hello_ack 后立即推 machine 快照（§4.3 行为 3）。
            send_all(
                &tx,
                vec![
                    Outbound::Msg(hello_ack_frame(role)),
                    Outbound::Msg(machine_frame(reg.machine_online)),
                ],
            )
            .await;
            tracing::info!(target: TARGET, machine_online = reg.machine_online, "browser 连接（认证通过）");
        }
    }

    // ---- 转发主循环 ----
    // 限流只计本连接**入站**业务帧（hello_ack 之前不算；relay 自产帧是出站
    // 的，天然不计入任何一端的限流——§4.4）。
    let mut rate = RateWindow::default();
    let mut last_rx = Instant::now();
    loop {
        // idle 剩余时间每轮从 last_rx 重算：出站流量不重置 idle 时钟，
        // 「90s 未收到对端任何 message」的语义才成立。
        let remaining = IDLE_TIMEOUT.saturating_sub(last_rx.elapsed());
        tokio::select! {
            // 服务停机兜底（正常路径是 close_all 经 channel 发来 Close(1001)）。
            changed = shutdown.changed() => {
                // 虚醒（值未翻 true）继续循环；信号到或控制端丢弃（Err）→ 停机。
                if changed.is_ok() && !*shutdown.borrow() {
                    continue;
                }
                let _ = socket.send(close_msg(CLOSE_GOING_AWAY, "going away")).await;
                break;
            }
            out = rx.recv() => match out {
                None => break, // 防御路径：槽位已清且自身 sender 已丢弃
                Some(Outbound::Msg(m)) => {
                    if socket.send(m).await.is_err() {
                        break; // 对端 socket 已死
                    }
                }
                Some(Outbound::Close(code, reason)) => {
                    let _ = socket.send(close_msg(code, reason)).await;
                    break;
                }
            },
            item = tokio::time::timeout(remaining, socket.next()) => match item {
                Err(_elapsed) => {
                    tracing::info!(target: TARGET, role = role.name(), "idle 90s 超时，close 1000");
                    let _ = socket.send(close_msg(CLOSE_NORMAL, "idle timeout")).await;
                    break;
                }
                Ok(None) | Ok(Some(Err(_))) => break, // 对端关闭 / 协议错误
                Ok(Some(Ok(msg))) => {
                    last_rx = Instant::now(); // 任何 message（含 ping/pong）都重置 idle
                    match msg {
                        // ping/pong 是端到端语义（§4.4）：relay 不做应用层应答；
                        // WS 协议层 Ping 由 axum 自动回 Pong。
                        Message::Ping(_) | Message::Pong(_) => {}
                        Message::Close(_) => break,
                        Message::Binary(_) => {
                            // MVP 无二进制载荷（§4.3 行为 8）。
                            let _ = socket.send(close_msg(CLOSE_POLICY, "binary not supported")).await;
                            break;
                        }
                        Message::Text(text) => {
                            // 限流（§4.4）：窗口内第 21 帧 → error + close 1008。
                            if !rate.check_and_record(Instant::now()) {
                                tracing::warn!(target: TARGET, role = role.name(), "限流触发（10s 内超过 20 帧），close 1008");
                                let _ = socket.send(error_frame("rate_limited")).await;
                                let _ = socket.send(close_msg(CLOSE_POLICY, "rate limited")).await;
                                break;
                            }
                            // 定向转发（§4.3 行为 4/5）：零拷贝转交 Utf8Bytes；
                            // try_send 不跨连接 await，杜绝互相顶死的环。
                            match role {
                                Role::Browser => match st.hub.route_browser_frame(text.as_str()) {
                                    BrowserRoute::Deliver(gw) => {
                                        tracing::debug!(target: TARGET, bytes = text.len(), "browser → gateway 透传");
                                        try_send(&gw, Outbound::Msg(Message::Text(text)));
                                    }
                                    BrowserRoute::Buffered => {
                                        tracing::debug!(target: TARGET, bytes = text.len(), "网关离线，帧入缓冲");
                                    }
                                    BrowserRoute::OfflineFull => {
                                        tracing::warn!(target: TARGET, bytes = text.len(), "网关离线缓冲已满，拒新帧（丢新保旧）");
                                        let _ = socket.send(error_frame("gateway_offline")).await;
                                    }
                                },
                                Role::Gateway => {
                                    if let Some(b) = st.hub.route_gateway_frame() {
                                        tracing::debug!(target: TARGET, bytes = text.len(), "gateway → browser 透传");
                                        try_send(&b, Outbound::Msg(Message::Text(text)));
                                    } else {
                                        tracing::debug!(target: TARGET, bytes = text.len(), "browser 离线，帧丢弃（不缓存）");
                                    }
                                }
                            }
                        }
                    }
                }
            },
        }
    }

    // ---- 退出：清槽位（+ 网关退出时通知 browser offline，§4.3 行为 2）----
    st.hub.release(role, conn_id);
    tracing::info!(target: TARGET, role = role.name(), "连接断开");
}

// ---------------------------------------------------------------------------
// server 组装（§4.7：bind 由调用方完成，fail-closed 校验先行）
// ---------------------------------------------------------------------------

/// relay server 句柄：测试与 main 共用。
pub struct RelayHandle {
    /// 实际绑定地址（测试绑 127.0.0.1:0 后由此取随机端口）。
    pub addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl RelayHandle {
    /// 触发 graceful shutdown（幂等；停 accept → 两条 WS close 1001 → drain）。
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// 等 serve 任务退出（shutdown 后 drain 完成才返回）。
    pub async fn wait(self) {
        let _ = self.task.await;
    }
}

/// 起 relay server。必须在 tokio runtime 内调用（内部 tokio::spawn）。
/// listener 由调用方绑定（校验先行，对齐 `src/webhook.rs` P2-2 先例）；
/// 配置不合法直接 Err（fail-closed 双保险）。
pub fn spawn_relay(listener: TcpListener, config: RelayConfig) -> Result<RelayHandle, String> {
    config.validate()?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("获取监听地址失败：{e}"))?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let hub = Hub::new(shutdown_rx.clone());
    let state = RelayState {
        hub: hub.clone(),
        config: Arc::new(config),
    };
    let app = Router::new()
        .route("/gateway", get(gateway_ws))
        .route("/ws", get(browser_ws))
        .route("/healthz", get(healthz))
        .fallback(spa_fallback)
        .with_state(state);
    // graceful shutdown（§4.5）：信号到 → close 两条 WS（1001）→ 连接退出
    // → serve drain 完成退出。控制端（RelayHandle）被丢弃同样触发停机：
    // 无人再能主动停机的服务与其失控常驻，不如随控制端退出。
    let drain = {
        let hub = hub.clone();
        let mut rx = shutdown_rx;
        async move {
            loop {
                if rx.changed().await.is_err() {
                    break; // 控制端已丢弃 → 停机
                }
                if *rx.borrow() {
                    break;
                }
            }
            hub.close_all(CLOSE_GOING_AWAY, "going away");
        }
    };
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(drain)
            .await
        {
            tracing::warn!(target: TARGET, error = %e, "relay serve 退出（错误）");
        }
    });
    tracing::info!(target: TARGET, addr = %addr, "relay listening（/gateway /ws /healthz + SPA fallback）");
    Ok(RelayHandle {
        addr,
        shutdown: shutdown_tx,
        task,
    })
}

// ---------------------------------------------------------------------------
// 单元测试（纯函数拨钟，任务说明 §5）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 常数时间比较：等长相等 / 等长不等 / 不等长 / 前缀 / 空串。
    #[test]
    fn constant_time_compare_variants() {
        assert!(constant_time_eq("abcdef0123456789", "abcdef0123456789"));
        assert!(
            !constant_time_eq("abcdef0123456789", "abcdef0123456788"),
            "末位不同"
        );
        assert!(!constant_time_eq("short", "short-but-longer"), "不等长");
        assert!(!constant_time_eq("prefix-extra", "prefix"), "前缀（反向）");
        assert!(!constant_time_eq("", "x"));
        assert!(constant_time_eq("", ""));
    }

    /// 滑动窗口限流：20 帧内放行、第 21 帧拒、窗口滑出后恢复。
    #[test]
    fn sliding_window_rate_limit() {
        let mut w = RateWindow::default();
        let t0 = Instant::now();
        for _ in 0..RATE_MAX_FRAMES {
            assert!(w.check_and_record(t0), "前 20 帧放行");
        }
        assert!(!w.check_and_record(t0), "窗口内第 21 帧拒");
        assert!(
            !w.check_and_record(t0 + Duration::from_secs(9)),
            "9s 时旧帧仍在窗内，仍拒"
        );
        assert!(
            w.check_and_record(t0 + RATE_WINDOW),
            "10s 整点旧帧滑出窗口，恢复放行"
        );
        // 恢复后窗口内只有 1 帧：再来 19 帧仍放行，第 21 帧拒。
        for _ in 0..RATE_MAX_FRAMES - 1 {
            assert!(w.check_and_record(t0 + RATE_WINDOW));
        }
        assert!(!w.check_and_record(t0 + RATE_WINDOW));
    }

    /// 离线缓冲：FIFO 序、200 帧上限、1MB 字节上限、溢出拒新、drain 清零。
    #[test]
    fn offline_buffer_fifo_caps_and_drain() {
        // FIFO。
        let mut b = OfflineBuffer::default();
        for f in ["a", "b", "c"] {
            assert!(b.push(f));
        }
        assert_eq!(b.drain(), vec!["a", "b", "c"]);

        // 帧数上限：恰好 200 帧允许，第 201 帧拒。
        let mut b = OfflineBuffer::default();
        for _ in 0..BUF_MAX_FRAMES {
            assert!(b.push("f"));
        }
        assert!(!b.push("f"), "第 201 帧拒（帧数上限）");
        let drained = b.drain();
        assert_eq!(drained.len(), BUF_MAX_FRAMES, "溢出拒新保旧：旧帧完整");
        assert!(b.push("z"), "drain 后清零可再入");
        assert_eq!(b.drain(), vec!["z"]);

        // 字节上限：恰好 1MB 允许（加入后 == 上限未超），再入 1 字节拒。
        let mut b = OfflineBuffer::default();
        let frame_1m = "x".repeat(BUF_MAX_BYTES);
        assert!(b.push(&frame_1m), "恰好 1MB 允许");
        assert!(!b.push("y"), "加入后超 1MB → 拒");
    }

    /// hello 帧解析：合法 / 缺 token / 畸形 JSON / 非 hello 首帧 / 版本 / 空 token。
    #[test]
    fn hello_frame_parsing() {
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc"}"#),
            Ok("abc".to_string())
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello"}"#),
            Err(HelloParseError::Malformed),
            "缺 token"
        );
        assert_eq!(
            parse_hello("not json"),
            Err(HelloParseError::Malformed),
            "畸形 JSON"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"message","token":"x"}"#),
            Err(HelloParseError::NotHello),
            "非 hello 首帧"
        );
        assert_eq!(
            parse_hello(r#"{"v":2,"type":"hello","token":"x"}"#),
            Err(HelloParseError::BadVersion)
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":""}"#),
            Err(HelloParseError::Malformed),
            "空 token"
        );
    }

    /// fail-closed：token 缺失 / 相同 → 拒。
    #[test]
    fn fail_closed_token_validation() {
        let cfg = |g: &str, u: &str| RelayConfig {
            gateway_token: g.to_string(),
            user_token: u.to_string(),
        };
        assert!(cfg("g-token", "u-token").validate().is_ok());
        let err = cfg("", "u-token").validate().unwrap_err();
        assert!(
            err.contains("GATEWAY_TOKEN") && err.contains("无认证模式"),
            "msg={err}"
        );
        let err = cfg("g-token", "  ").validate().unwrap_err();
        assert!(err.contains("USER_TOKEN"), "msg={err}");
        let err = cfg("same", "same").validate().unwrap_err();
        assert!(err.contains("不得相同"), "msg={err}");
    }

    /// fail-closed：非 loopback bind → 拒（报错给修复指引）。
    #[test]
    fn fail_closed_bind_validation() {
        assert!(validate_bind("127.0.0.1:3010".parse().unwrap()).is_ok());
        assert!(validate_bind("[::1]:3010".parse().unwrap()).is_ok());
        for bad in ["0.0.0.0:3010", "192.168.1.10:3010", "10.0.0.1:8443"] {
            let err = validate_bind(bad.parse().unwrap()).unwrap_err();
            assert!(
                err.contains("loopback") && err.contains("Caddy"),
                "addr={bad} msg={err}"
            );
        }
    }

    /// Hub 槽位语义：注册/踢旧/路由/释放互不误伤（无网络，纯 channel）。
    #[test]
    fn hub_register_route_release() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(false); // sender 保留，避免触发「控制端丢弃=停机」
        let hub = Hub::new(shutdown_rx);

        // 网关注册：空环境 → 无踢旧、无 browser、无重放。
        let (gw_tx, mut gw_rx) = mpsc::channel(8);
        let reg = hub.register_gateway(1, gw_tx);
        assert!(reg.superseded.is_none() && reg.browser.is_none() && reg.replay.is_empty());

        // browser 注册：machine 快照 online（网关在线）。
        let (br_tx, mut br_rx) = mpsc::channel(8);
        let regb = hub.register_browser(3, br_tx);
        assert!(regb.machine_online);

        // browser 帧 → 网关在线透传。
        match hub.route_browser_frame("hello") {
            BrowserRoute::Deliver(tx) => {
                try_send(&tx, Outbound::Msg(Message::text("hello")));
                match gw_rx.try_recv() {
                    Ok(Outbound::Msg(m)) => assert_eq!(m.into_text().unwrap().as_str(), "hello"),
                    other => panic!("应透传到网关 channel：{other:?}"),
                }
            }
            other => panic!("网关在线应 Deliver：{other:?}"),
        }

        // 踢旧：第二条网关注册顶替第一条，旧投递口收到 4001。
        let (gw2_tx, _gw2_rx) = mpsc::channel(8);
        let reg2 = hub.register_gateway(2, gw2_tx);
        let old = reg2.superseded.expect("应有旧连接被顶替");
        try_send(&old, Outbound::Close(CLOSE_SUPERSEDED, "superseded"));
        assert!(matches!(
            gw_rx.try_recv(),
            Ok(Outbound::Close(CLOSE_SUPERSEDED, "superseded"))
        ));

        // 旧连接（conn_id=1）退出不得误清新槽位：网关仍在线。
        hub.release(Role::Gateway, 1);
        assert!(
            hub.route_gateway_frame().is_some(),
            "旧连接退出不清新连接的槽位"
        );

        // 现网关（conn_id=2）退出 → browser 收 machine offline。
        hub.release(Role::Gateway, 2);
        match br_rx.try_recv() {
            Ok(Outbound::Msg(m)) => {
                let s = m.into_text().unwrap();
                assert!(
                    s.contains(r#""type":"machine""#) && s.contains("false"),
                    "msg={s}"
                );
            }
            other => panic!("应通知 browser 离线：{other:?}"),
        }

        // 网关离线：browser 帧入缓冲（重放序 = 入队序）。
        for f in ["1", "2", "3"] {
            assert!(matches!(hub.route_browser_frame(f), BrowserRoute::Buffered));
        }
        let (gw3_tx, _gw3_rx) = mpsc::channel(8);
        let reg3 = hub.register_gateway(9, gw3_tx);
        assert_eq!(reg3.replay, vec!["1", "2", "3"], "重放保 FIFO 序");
    }
}
