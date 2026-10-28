//! `imagent-relay`：Web 驾驶舱 M1——ECS 侧无状态 WS 帧中继
//! （设计见 docs/WEB_DESIGN.md §2/§3.1，规格以任务评审修订版为准）。
//!
//! 拓扑：`浏览器 WS（多连接并存）⇄ relay ⇄ 网关 WS（单连接）`，连接都是
//! 客户端模式连到本进程（Mac 网关零入站端口）。本 crate 是中继：
//! **不理解任何业务语义**，只做认证（双 token、fail-closed）、转发
//! （Hub 状态机）、网关离线缓冲（200 帧 / 1MB）、每连接限流
//! （20 帧 / 10s）。部署形态：ECS systemd 服务绑 127.0.0.1:3010，公网由
//! Caddy（8443，TLS 终结）`handle_path /imagent/*` strip 前缀后反代——
//! relay 看到的路径已是 strip 后的 `/xxx`。
//!
//! 端点语义（真机常态：手机 + 电脑 + 多标签同时在线）：
//! - **浏览器侧多连接并存 + 广播**：同一 user token 的多条 browser 连接
//!   同时注册在 Hub（并存上限 [`MAX_BROWSERS`] = 8，防滥用——满时新连接
//!   认证通过后仍被 close 4001，复用 superseded 让位语义，SPA 已有 4001
//!   终态处理：不重连、不与在线连接互踢）；gateway→browser 业务帧与
//!   machines 上/下线通知向**全部**在线 browser 广播（逐个 try_send，单个
//!   打满/关闭不影响其余）；任一 browser 的入站帧按 machine 字段路由给
//!   对应网关（或入离线缓冲），见 [`Hub::route_browser_frame`]。
//! - **网关侧多机器并存（帧协议 v1.1，M5 多终端管理）**：每条网关连接在
//!   hello 帧自报 `machine` 身份（Bearer 直通回退兼容 = `"default"` 机器），
//!   Hub 按 machine 槽位存多连接（上限 [`MAX_GATEWAYS`] = 4——满员且非同
//!   机重连的新连接 close 4001）；**同一 machine 重连仍踢旧**（每台机器
//!   单连接语义，旧连接 close 4001 superseded）。
//!
//! 分层（任务说明 §4.7）：核心状态机全部在本 lib（可单测 + 集成测试直接
//! [`spawn_relay`] 起真 server），`main.rs` 只做 env / bind / tracing 组装。

#![deny(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::header::HeaderName;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next};
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
/// 每连接出站 channel 容量。上限推导：网关侧 hello_ack + 离线缓冲全量
/// 重放（≤200 帧）、浏览器侧 hello_ack + machine/machines 双快照，必须在
/// **进入消费循环之前**一次性入队也不阻塞，故 ≥202；取 512 留余量。业务
/// 帧转发走 try_send（打满即丢，见 [`try_send`]），连接之间不存在互相
/// await 的投递环，不会死锁。
const CHANNEL_CAPACITY: usize = 512;
/// 浏览器侧最多并存连接数（防滥用）：真机常态是手机 + 电脑 + 多标签
///（≤3~4 条），取 8 留余量。满时新连接认证通过后仍被 close 4001
///（复用 superseded 让位语义，SPA 已有 4001 终态处理：不重连）。
const MAX_BROWSERS: usize = 8;
/// 网关侧最多并存机器数（v1.1 多终端管理）：每条网关连接按 hello 自报的
/// machine 占一个槽位。真机常态是 2~3 台（家/公司/便携），取 4 留余量；
/// 满员且非同机重连的新连接 close 4001（同 machine 重连不受限——踢旧
/// 顶替，每台机器单连接语义）。
const MAX_GATEWAYS: usize = 4;
/// 缺省 machine id（v1.1）：hello 不带 machine / Bearer 直通（无 machine
/// 维度）都落到 default——与单机时期的语义完全一致（旧网关/旧 SPA 回退
/// 兼容），旧 `machine` 帧也只表达 default 机器的在线状态。
const DEFAULT_MACHINE: &str = "default";
/// machine id 协议约束（v1.1）：≤32 char，仅 ASCII 字母数字与 `-_`。
const MACHINE_ID_MAX_CHARS: usize = 32;

/// WS close code（RFC 6455 + 私有码，§4.2/§4.3/§4.5）。
const CLOSE_POLICY: u16 = 1008; // 认证失败 / 协议违规（binary、非 hello 首帧）/ 限流
const CLOSE_NORMAL: u16 = 1000; // idle 超时正常关闭
const CLOSE_GOING_AWAY: u16 = 1001; // 服务停机
const CLOSE_SUPERSEDED: u16 = 4001; // 网关新连接顶替旧连接（踢旧）；browser 并存数达上限

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

/// hello 首帧反序列化形状：`{"v":1,"type":"hello","token":"...","machine":"..."}`
///（v1.1：machine 可选，网关侧自报身份；浏览器侧无 machine 概念，不发送）。
#[derive(Deserialize)]
struct HelloFrame {
    v: i64,
    #[serde(rename = "type")]
    kind: String,
    token: String,
    #[serde(default)]
    machine: Option<String>,
}

/// hello 解析失败原因（只为日志可读；对外行为一致——close 1008）。
#[derive(Debug, PartialEq, Eq)]
enum HelloParseError {
    /// JSON 畸形 / 缺字段 / token 非字符串 / token 为空串 / machine 非法
    ///（空串、trim 后为空、超 32 char、含字母数字-_ 之外的字符）。
    Malformed,
    /// 首帧 type 不是 hello。
    NotHello,
    /// 信封版本不为 1。
    BadVersion,
}

/// machine id 校验（v1.1 协议约束，纯函数便于单测）：trim、非空、
/// ≤32 char、仅 ASCII 字母数字与 `-_`。通过返回 trim 后的值。
fn validate_machine(raw: &str) -> Result<String, HelloParseError> {
    let m = raw.trim();
    if m.is_empty() {
        return Err(HelloParseError::Malformed);
    }
    if m.chars().count() > MACHINE_ID_MAX_CHARS {
        return Err(HelloParseError::Malformed);
    }
    if !m
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(HelloParseError::Malformed);
    }
    Ok(m.to_string())
}

/// 解析首帧 hello（纯函数便于单测），返回 `(token, machine)`。空 token 判
/// Malformed：空串不可能与合法配置 token 相等，提前拒绝让语义更明确。
/// machine 缺省（字段不存在）→ `"default"`；带值则经 [`validate_machine`]
/// 校验（非法拒绝——machine 是路由主键，脏值宁可断连也不静默改写）。
/// 浏览器侧调用方忽略 machine（浏览器 hello 不带该字段；带了也只校验不
/// 消费，不影响浏览器路由语义）。
fn parse_hello(text: &str) -> Result<(String, String), HelloParseError> {
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
    let machine = match h.machine.as_deref() {
        None => DEFAULT_MACHINE.to_string(),
        Some(m) => validate_machine(m)?,
    };
    Ok((h.token, machine))
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
///
/// M5 多机简化：缓冲保持 per-relay 单 FIFO，**不按 machine 分桶**。理由：
/// 帧进缓冲的唯一时机是**全部**网关离线（任一在线都不会缓冲）；缓冲帧
/// 重放给第一个上线的网关。多机并存时 SPA 本就必须显式选 machine 才能发
/// 帧（缺省路由在多机下是 ambiguous error——bootstrap 期的 sync/ping 静默
/// 豁免除外，见 [`AMBIGUOUS_SILENT_KINDS`]），「全下线」窗口内积压的帧语义
/// 上属于「当时选中的机器」，但 SPA 重连后所选机器未必恢复——按 machine
/// 分桶要么帧永久滞留（无人来取），要么协议再加清理规则，复杂度不匹配
/// 收益；单 FIFO「先上线先重放」足够覆盖主场景（单机部署 / 多机短暂全掉
/// 后任一台先恢复）。
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
/// 业务帧内容——machine/error 之外 relay 对帧是「盲」的）。hello_ack 的
/// `machine` 仅网关侧携带（v1.1 身份确认：回显 hello 自报的 machine）。
fn hello_ack_frame(role: Role, machine: Option<&str>) -> Message {
    match machine {
        Some(m) => Message::text(
            serde_json::json!({"v": 1, "type": "hello_ack", "role": role.name(), "machine": m})
                .to_string(),
        ),
        None => Message::text(
            serde_json::json!({"v": 1, "type": "hello_ack", "role": role.name()}).to_string(),
        ),
    }
}

/// 旧 `machine` 帧（v1 唯一通知形态，v1.1 保留）：语义收敛为「default
/// 机器的在线状态」——单机部署（default 机器）与旧 SPA 的行为完全不变；
/// 非 default 机器上下线不发本帧（新 SPA 看 [`machines_frame`]）。
fn machine_frame(online: bool) -> Message {
    Message::text(serde_json::json!({"v": 1, "type": "machine", "online": online}).to_string())
}

/// v1.1 `machines` 帧：全量在线机器列表（排序不敏感，此处给有序输出便于
/// 测试与日志），任一网关上/下线时向全部在线浏览器广播。
fn machines_frame(machines: &[String]) -> Message {
    Message::text(serde_json::json!({"v": 1, "type": "machines", "online": machines}).to_string())
}

fn error_frame(code: &str) -> Message {
    Message::text(serde_json::json!({"v": 1, "type": "error", "code": code}).to_string())
}

/// 出站投递项：业务帧 / 主动 close（code+reason）。
/// close 必须走 channel 而非直接操作 socket：socket 由连接任务独占，而
/// 踢旧（4001）与服务停机（1001）都发生在其它任务的上下文里。
#[derive(Debug, Clone)]
enum Outbound {
    Msg(Message),
    Close(u16, &'static str),
}

/// Hub 槽位：指向某条连接的出站 channel + 连接身份。conn_id 用于网关
/// machine 槽位防「踢旧后旧连接退出时误清新连接」；browser 并存表以
/// conn_id 为键，字段保持单一 Endpoint 形状。
struct Endpoint {
    conn_id: u64,
    tx: mpsc::Sender<Outbound>,
}

struct HubState {
    /// 网关多机器并存表（v1.1）：machine id → 连接槽位（上限
    /// [`MAX_GATEWAYS`]；同 machine 重连顶替旧连接——每台机器单连接）。
    gateways: HashMap<String, Endpoint>,
    /// 浏览器多连接并存表：conn_id → 出站投递口（上限 [`MAX_BROWSERS`]）。
    browsers: HashMap<u64, Endpoint>,
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

/// 锁内取全部在线 browser 的投递口（广播目标：machines/machine 通知 /
/// 业务帧 fan-out / 停机 close）。
fn browser_senders(st: &HubState) -> Vec<mpsc::Sender<Outbound>> {
    st.browsers.values().map(|e| e.tx.clone()).collect()
}

/// 锁内取全量在线 machine 列表（有序输出——machines 帧对 SPA 排序不敏感，
/// 有序只为测试与日志稳定）。
fn online_machines(st: &HubState) -> Vec<String> {
    let mut v: Vec<String> = st.gateways.keys().cloned().collect();
    v.sort();
    v
}

/// 网关注册结果：锁外要做的副作用（踢旧 / 广播上线通知 / 重放缓冲）。
struct GatewayRegistered {
    /// 同 machine 旧连接被顶替（每台机器单连接语义）→ 调用方对其 close 4001。
    superseded: Option<mpsc::Sender<Outbound>>,
    /// 全部在线 browser 的投递口（machines/machine 上线广播目标）。
    browsers: Vec<mpsc::Sender<Outbound>>,
    /// 注册后全量在线 machine 列表（machines 帧载荷）。
    machines: Vec<String>,
    replay: Vec<String>,
    /// 注册后在线网关数（日志）。
    gateways: usize,
    /// 本次注册的是否 default 机器（决定是否附带旧 machine 上线帧）。
    is_default: bool,
}

/// browser 注册结果（多连接并存版）：不再踢旧。`full` = 并存数已达
/// [`MAX_BROWSERS`]，本连接未注册（调用方对**自己** close 4001——复用
/// superseded 让位语义，SPA 已有终态处理）；`machine_online` 为注册时
/// **default 机器**在线快照（旧 machine 帧语义，v1.1）；`machines` 为
/// 注册时全量在线机器列表（v1.1 machines 快照帧载荷）；`browsers` 为
/// 注册后在线 browser 数（含本连接），供连接日志。
struct BrowserRegistered {
    full: bool,
    machine_online: bool,
    machines: Vec<String>,
    browsers: usize,
}

/// browser 业务帧的路向（§4.3 行为 4 + v1.1 §3 路由规则）。
#[derive(Debug)]
enum BrowserRoute {
    /// 命中网关（显式指定该机器在线 / 未指定且恰好一台在线）→ 透传
    ///（携带网关出站 channel）。
    Deliver(mpsc::Sender<Outbound>),
    /// 全部网关离线 → 已入缓冲（单 FIFO，见 [`OfflineBuffer`] 头注释）。
    Buffered,
    /// 全部网关离线且缓冲满 → 不入，调用方向该 browser 回 gateway_offline error。
    OfflineFull,
    /// 显式指定 machine 但该机器不在线 → 调用方回 machine_offline error。
    MachineOffline,
    /// 未指定 machine 且多台网关在线，但帧类型为 bootstrap 豁免类型
    ///（sync/ping，见 [`AMBIGUOUS_SILENT_KINDS`]）→ 静默丢弃：不路由、
    /// 不回错（SPA 收到 machines 帧后会补发带 machine 的 sync；ping 无
    /// 副作用）。其余帧类型仍 [`BrowserRoute::Ambiguous`]——真实用户操作
    /// 报错是对的。
    Ignored,
    /// 未指定且多台网关在线（缺省路由歧义）→ 调用方回 ambiguous_machine error。
    Ambiguous,
}

/// browser→gateway 业务帧的可选 `machine` 路由键提取（v1.1 §3）。
///
/// relay 对业务帧保持「盲」（零解析零改写零记录），唯一例外是这一个
/// 字段：`machine` 是 **transport 层路由键**（决定帧投给哪条网关连接），
/// 不是业务语义——不读它就无法多机路由；除它之外的任何字段仍然不碰。
/// JSON 畸形 / 字段缺失 / 非字符串 / trim 后为空 → None（按「未指定」
/// 路由；畸形帧本身照原样透传或入缓冲，盲转语义不变）。
fn frame_machine(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let m = v.get("machine")?.as_str()?.trim();
    if m.is_empty() {
        return None;
    }
    Some(m.to_string())
}

/// 缺省路由歧义（未指定 machine 且多台网关在线）时**静默忽略**的帧类型：
/// `sync` 与 `ping`。理由（bootstrap 时序豁免）：
/// - SPA 首连在 hello_ack 后立即发 bootstrap sync，此时 machines 列表帧
///   尚未到达、无从带 machine（鸡生蛋）；sync 幂等可补发——SPA 收到
///   machines 帧后会再发一次带 machine 的 sync，此前的无 machine sync
///   丢弃无害；
/// - ping 是无副作用探活，丢弃同样无害。
///
/// 这两类帧若照常回 `ambiguous_machine`，只会让 SPA 每次连接都弹一次
///「多台机器在线」toast（4s 噪音）。其余类型（message / approval_response
/// / control）是真实用户操作，歧义仍回错——见 [`BrowserRoute::Ambiguous`]。
const AMBIGUOUS_SILENT_KINDS: [&str; 2] = ["sync", "ping"];

/// browser→gateway 帧的 `type` 提取（仅服务于 [`AMBIGUOUS_SILENT_KINDS`]
/// 判定）。与 [`frame_machine`] 同款纪律：relay 对业务帧保持「盲」，`type`
/// 只用于歧义分支的静默豁免判定，不消费业务语义。JSON 畸形 / 字段缺失 /
/// 非字符串 → None（按未知类型处理，走既有 Ambiguous 回错路径）。
fn frame_kind(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(v.get("type")?.as_str()?.to_string())
}

/// 缺省路由歧义分支的静默豁免判定：帧 type 属于 [`AMBIGUOUS_SILENT_KINDS`]
///（解析失败 / 未知类型 → false，不豁免）。
fn is_ambiguous_silent_kind(text: &str) -> bool {
    match frame_kind(text).as_deref() {
        Some(k) => AMBIGUOUS_SILENT_KINDS.contains(&k),
        None => false,
    }
}

impl Hub {
    fn new(shutdown: watch::Receiver<bool>) -> Self {
        Self {
            state: Arc::new(Mutex::new(HubState {
                gateways: HashMap::new(),
                browsers: HashMap::new(),
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

    /// 网关注册（v1.1 多机器并存）：满员（[`MAX_GATEWAYS`]）且非同机重连
    /// → None（调用方 close 4001 自己）；否则占该 machine 的槽位——同
    /// machine 旧连接被顶替（踢旧）、取全部在线 browser 的投递口（上线
    /// 广播目标）、drain 离线缓冲（重放顺序 = 入队顺序，见
    /// [`OfflineBuffer`] 头注释的单 FIFO 简化）。
    fn register_gateway(
        &self,
        machine: &str,
        conn_id: u64,
        tx: mpsc::Sender<Outbound>,
    ) -> Option<GatewayRegistered> {
        let mut st = self.lock();
        if st.gateways.len() >= MAX_GATEWAYS && !st.gateways.contains_key(machine) {
            return None;
        }
        let superseded = st
            .gateways
            .insert(machine.to_string(), Endpoint { conn_id, tx })
            .map(|e| e.tx);
        let reg = GatewayRegistered {
            superseded,
            browsers: browser_senders(&st),
            machines: online_machines(&st),
            replay: st.buf.drain(),
            gateways: st.gateways.len(),
            is_default: machine == DEFAULT_MACHINE,
        };
        Some(reg)
    }

    /// browser 注册（多连接并存）：上限未满 → 注册新 conn_id（不踢任何旧
    /// 连接）+ default 机器快照 + 全量 machines 快照；已满 → full=true
    ///（本连接不注册，调用方 close 4001 自己）。
    fn register_browser(&self, conn_id: u64, tx: mpsc::Sender<Outbound>) -> BrowserRegistered {
        let mut st = self.lock();
        if st.browsers.len() >= MAX_BROWSERS {
            return BrowserRegistered {
                full: true,
                machine_online: st.gateways.contains_key(DEFAULT_MACHINE),
                machines: online_machines(&st),
                browsers: st.browsers.len(),
            };
        }
        st.browsers.insert(conn_id, Endpoint { conn_id, tx });
        BrowserRegistered {
            full: false,
            machine_online: st.gateways.contains_key(DEFAULT_MACHINE),
            machines: online_machines(&st),
            browsers: st.browsers.len(),
        }
    }

    /// browser→gateway 帧路向（§4.3 行为 4 + v1.1 §3 路由规则）：
    /// - 显式 machine 在线 → 透传；不在线 → [`BrowserRoute::MachineOffline`]；
    /// - 未指定：恰好一台 → 透传（单机部署 SPA 零升级无感）；多台 → sync/
    ///   ping 静默丢弃（bootstrap 时序豁免，[`BrowserRoute::Ignored`]），
    ///   其余 [`BrowserRoute::Ambiguous`]；零台 → 沿用 M1 行为入单 FIFO
    ///   缓冲（sync/ping 亦照旧入缓冲——豁免只作用于歧义分支），缓冲满
    ///   拒新（丢新保旧）。
    fn route_browser_frame(&self, text: &str) -> BrowserRoute {
        let mut st = self.lock();
        match frame_machine(text) {
            Some(m) => match st.gateways.get(&m) {
                Some(e) => BrowserRoute::Deliver(e.tx.clone()),
                None => BrowserRoute::MachineOffline,
            },
            None => match st.gateways.len() {
                1 => {
                    let e = st.gateways.values().next().expect("len==1 必有槽位");
                    BrowserRoute::Deliver(e.tx.clone())
                }
                0 => {
                    if st.buf.push(text) {
                        BrowserRoute::Buffered
                    } else {
                        BrowserRoute::OfflineFull
                    }
                }
                _ if is_ambiguous_silent_kind(text) => BrowserRoute::Ignored,
                _ => BrowserRoute::Ambiguous,
            },
        }
    }

    /// gateway→browser 帧路向（§4.3 行为 5，多 browser 广播版）：返回全部
    /// 在线 browser 的投递口，调用侧逐个 try_send——单个打满/关闭不影响
    /// 其余（已关闭的条目无需在此清理，release 路径会移除）。空 = 无在线
    /// browser，帧直接丢弃不缓存——会话恢复走网关侧重发机制，不归 relay
    /// 管（docs/WEB_DESIGN.md D6）。网关侧出站帧自带 machine 字段（v1.1），
    /// relay 透传不改、广播全部 browser。
    fn route_gateway_frame(&self) -> Vec<mpsc::Sender<Outbound>> {
        let st = self.lock();
        browser_senders(&st)
    }

    /// browser 退出只清自己的槽位（按键精确移除，天然不误伤其它并存连接），
    /// 返回剩余在线 browser 数（供断开日志）。
    fn release_browser(&self, conn_id: u64) -> usize {
        let mut st = self.lock();
        st.browsers.remove(&conn_id);
        st.browsers.len()
    }

    /// 网关退出清 machine 槽位（§4.3 行为 2，v1.1 多机版）：仅当该 machine
    /// 槽位仍指向本连接（conn_id 相同）才清——踢旧后旧连接退出不得误清新
    /// 连接；清槽位后锁外向**全部**在线 browser 广播 default 机器下线（旧
    /// machine 帧，仅 default 机器）+ 全量 machines 列表（v1.1）。
    fn release_gateway(&self, machine: &str, conn_id: u64) -> (usize, usize) {
        // 锁内：判定槽位归属（非本连接 → 提前 return，不广播）、清槽位、
        // 快照广播目标与计数。
        let (senders, machines, is_default, browsers, gateways) = {
            let mut st = self.lock();
            let is_current = st
                .gateways
                .get(machine)
                .is_some_and(|e| e.conn_id == conn_id);
            if !is_current {
                return (st.browsers.len(), st.gateways.len());
            }
            st.gateways.remove(machine);
            (
                browser_senders(&st),
                online_machines(&st),
                machine == DEFAULT_MACHINE,
                st.browsers.len(),
                st.gateways.len(),
            )
        };
        // 旧 machine 帧在前（v1 旧 SPA 看到的帧序不变），machines 帧在后。
        if is_default {
            let offline = Outbound::Msg(machine_frame(false));
            for b in &senders {
                try_send(b, offline.clone());
            }
        }
        let list = Outbound::Msg(machines_frame(&machines));
        for b in &senders {
            try_send(b, list.clone());
        }
        (browsers, gateways)
    }

    /// 服务停机：向全部网关与全部在线 browser 发 Close（§4.5 graceful
    /// shutdown，1001）。
    fn close_all(&self, code: u16, reason: &'static str) {
        let senders = {
            let st = self.lock();
            st.gateways
                .values()
                .map(|e| e.tx.clone())
                .chain(browser_senders(&st))
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

/// 安全响应头 · CSP（SPA 纵深防御）：浏览器把 user token 存在 localStorage，
/// XSS 是偷 token 的主通道——CSP 让「即使被注入 HTML，脚本也不执行」。
/// 逐项理由：
/// - `default-src 'self'`：未显式列出的加载类 directive 一律收紧到同源；
/// - `script-src 'self'`：**核心项**，不放行 inline / eval——已核实
///   `static/index.html` 与 Vite 构建产物无 inline script（唯一
///   `<script type="module" crossorigin src>` 外链，无 `%VITE%` 注入、无
///   inline 事件属性），故无需 nonce 方案；注入的内联脚本一律不执行；
/// - `style-src 'self' 'unsafe-inline'`：React 运行时 `<elem style="...">`
///   属性注入与 Tailwind 产物所需。风险可接受——CSS 注入改的是样式，
///   偷不走 token（数据外带走 connect-src，已被 'self' 封死）；
/// - `img-src 'self' data:`：内联 `data:` URI 图标/占位图所需；
/// - `connect-src 'self' wss: ws:`：fetch/XHR/WebSocket 只准同源。scheme
///   双列是部署形态决定的：公网经 Caddy（TLS 终结）是 `wss://`，本地
///   http 直连调试形态（http://127.0.0.1:3010/）是 `ws://`——都属同源
///   WebSocket，不是放行外站；
/// - `font-src 'self'`：字体同源（产物无外链字体）；
/// - `frame-ancestors 'none'`：任何页面不得嵌入本页（clickjacking）；
/// - `base-uri 'none'`：禁 `<base>` 注入改写相对 URL 的解析根（绕过
///   script-src 的相对路径技俩）；
/// - `form-action 'self'`：表单只准提交同源，堵「注入表单外带凭证」。
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' \
     'unsafe-inline'; img-src 'self' data:; connect-src 'self' wss: ws:; \
     font-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

/// 对全部响应（SPA 静态 / healthz / WS upgrade 101）统一注入安全响应头。
/// 用 `Router::layer`（见 [`spawn_relay`]）盖住所有路由与 fallback。逐项：
/// - CSP：见 [`CSP`]；
/// - `X-Content-Type-Options: nosniff`：禁 MIME 嗅探——配 `content_type`
///   白名单，防 svg/js 被浏览器「猜」成 HTML 执行；
/// - `Referrer-Policy: no-referrer`：Referer 泄漏面清零（token 本就不进
///   URL，纵深防御）；
/// - `X-Frame-Options: DENY`：与 CSP `frame-ancestors` 双保险——旧浏览器
///   不认 frame-ancestors，两者取严即可；
/// - `Cross-Origin-Opener-Policy: same-origin`：切断跨源 `window.opener`
///   句柄（reverse tabnabbing / 钓鱼跳板）。
async fn security_headers(req: Request, next: Next) -> Response {
    /// COOP：http crate 未提供标准常量，静态字符串构造（编译期校验合法）。
    const CROSS_ORIGIN_OPENER_POLICY_NAME: HeaderName =
        HeaderName::from_static("cross-origin-opener-policy");
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        CROSS_ORIGIN_OPENER_POLICY_NAME,
        HeaderValue::from_static("same-origin"),
    );
    res
}

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
    let raw = uri.path().trim_start_matches('/');
    // 兼容不经 Caddy 的直连形态（本地开发/验收直开 http://127.0.0.1:3010/）：
    // SPA 构建产物（Vite base=/imagent/）的资源引用自带 `/imagent/` 前缀，
    // 公网由 Caddy `handle_path /imagent/*` strip 后转给 relay，但直连时
    // 浏览器请求的是 /imagent/assets/...——这里剥一次前缀再查内嵌产物，
    // 带前缀与不带前缀两种 URL 都可用；缓存头按剥前缀后的路径判定
    //（/imagent/assets/... 同样享 immutable），剥完仍未命中的照旧回落
    // index.html（含 SPA 客户端路由）。
    let path = raw.strip_prefix("imagent/").unwrap_or(raw);
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
    // 认证产物：网关侧 machine 身份（hello 自报 / Bearer 回退 default）；
    // 浏览器侧无 machine 概念（恒 None）。
    let mut auth_machine: Option<String> = None;
    // Bearer 正确 → 直进转发态；Bearer 带了但不正确 → 失败路径（不回落
    // hello：发起方已表明认证意图，回落只会让探测多一种节奏）。
    match bearer_token(&headers) {
        Some(t) if constant_time_eq(t, expected) => {
            // Bearer 直通无 machine 维度（v1.1 回退兼容：旧网关版本/调试
            // 脚本）→ default 机器。
            if role == Role::Gateway {
                auth_machine = Some(DEFAULT_MACHINE.to_string());
            }
        }
        Some(_) => {
            auth_reject(&mut socket, role, "Authorization Bearer token 不匹配").await;
            return;
        }
        None => {
            // hello 首帧认证：10s 内必须是 `{"v":1,"type":"hello","token":...}`
            //（网关侧 v1.1 起带 machine 自报身份）。
            match tokio::time::timeout(AUTH_TIMEOUT, socket.next()).await {
                Err(_) => {
                    tracing::warn!(target: TARGET, role = role.name(), "认证超时（10s 无 hello），关闭");
                    let _ = socket.send(close_msg(CLOSE_POLICY, "auth timeout")).await;
                    return;
                }
                Ok(None) | Ok(Some(Err(_))) => return, // 连接已断 / 协议错误
                Ok(Some(Ok(msg))) => {
                    let authed = match msg {
                        Message::Text(t) => match parse_hello(t.as_str()) {
                            Ok((tok, machine)) => {
                                if !constant_time_eq(&tok, expected) {
                                    false
                                } else {
                                    // machine 身份仅网关侧消费（浏览器 hello 不带该
                                    // 字段，带了也只经校验不使用）。
                                    if role == Role::Gateway {
                                        auth_machine = Some(machine);
                                    }
                                    true
                                }
                            }
                            Err(_) => false,
                        },
                        _ => false, // 首帧必须是文本 hello
                    };
                    if !authed {
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
            let machine = auth_machine
                .clone()
                .expect("网关认证通过后必有 machine 身份");
            let Some(reg) = st.hub.register_gateway(&machine, conn_id, tx.clone()) else {
                // 满员（MAX_GATEWAYS 且非同机重连）：认证已通过，close 4001
                // 让位——复用 superseded 终态语义（新网关客户端按可重连错误
                // 处理，退避后重试；同 machine 槽位释放后即可再入）。
                tracing::warn!(target: TARGET, machine = %machine, "网关机器数达上限（MAX_GATEWAYS），close 4001");
                let _ = socket
                    .send(close_msg(CLOSE_SUPERSEDED, "too many gateways"))
                    .await;
                return;
            };
            let replayed = reg.replay.len();
            if let Some(old) = reg.superseded {
                tracing::info!(target: TARGET, machine = %machine, "踢旧：同 machine 新连接顶替旧连接（close 4001 superseded）");
                try_send(&old, Outbound::Close(CLOSE_SUPERSEDED, "superseded"));
            }
            // hello_ack 先于重放帧：客户端先确认角色、认证结果与 machine 身份
            //（v1.1 回显自报的 machine），再消费重放。
            let mut init = vec![Outbound::Msg(hello_ack_frame(role, Some(&machine)))];
            init.extend(
                reg.replay
                    .into_iter()
                    .map(|f| Outbound::Msg(Message::text(f))),
            );
            send_all(&tx, init).await;
            // 上线通知广播给全部在线 browser（多端并存，逐个 try_send）：
            // default 机器附带旧 machine 上线帧（v1 兼容），随后全量 machines
            // 列表帧（v1.1）。
            if reg.is_default {
                let online = Outbound::Msg(machine_frame(true));
                for b in &reg.browsers {
                    try_send(b, online.clone());
                }
            }
            let list = Outbound::Msg(machines_frame(&reg.machines));
            for b in reg.browsers {
                try_send(&b, list.clone());
            }
            tracing::info!(target: TARGET, machine = %machine, gateways = reg.gateways, replayed, "gateway 连接（认证通过）");
        }
        Role::Browser => {
            let reg = st.hub.register_browser(conn_id, tx.clone());
            if reg.full {
                // 并存数达上限（认证已通过）：本连接 close 4001 让位——复用
                // superseded 语义，SPA 已有终态处理（不重连），在线连接零影响。
                tracing::warn!(target: TARGET, browsers = reg.browsers, "browser 并存数达上限（MAX_BROWSERS），close 4001");
                let _ = socket
                    .send(close_msg(CLOSE_SUPERSEDED, "too many browsers"))
                    .await;
                return;
            }
            // hello_ack 后立即推双快照（§4.3 行为 3）：旧 machine 帧（default
            // 机器在线状态，v1 兼容——帧序不变）+ 全量 machines 列表（v1.1）。
            send_all(
                &tx,
                vec![
                    Outbound::Msg(hello_ack_frame(role, None)),
                    Outbound::Msg(machine_frame(reg.machine_online)),
                    Outbound::Msg(machines_frame(&reg.machines)),
                ],
            )
            .await;
            tracing::info!(target: TARGET, machine_online = reg.machine_online, machines = ?reg.machines, browsers = reg.browsers, "browser 连接（认证通过）");
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
                            // 定向转发（§4.3 行为 4/5 + v1.1 §3 路由）：零拷贝
                            // 转交 Utf8Bytes；try_send 不跨连接 await，杜绝
                            // 互相顶死的环。
                            match role {
                                Role::Browser => match st.hub.route_browser_frame(text.as_str()) {
                                    BrowserRoute::Deliver(gw) => {
                                        tracing::debug!(target: TARGET, bytes = text.len(), "browser → gateway 透传");
                                        try_send(&gw, Outbound::Msg(Message::Text(text)));
                                    }
                                    BrowserRoute::Buffered => {
                                        tracing::debug!(target: TARGET, bytes = text.len(), "网关全下线，帧入缓冲（单 FIFO）");
                                    }
                                    BrowserRoute::OfflineFull => {
                                        tracing::warn!(target: TARGET, bytes = text.len(), "网关离线缓冲已满，拒新帧（丢新保旧）");
                                        let _ = socket.send(error_frame("gateway_offline")).await;
                                    }
                                BrowserRoute::MachineOffline => {
                                    tracing::debug!(target: TARGET, bytes = text.len(), "目标 machine 不在线，回 machine_offline");
                                    let _ = socket.send(error_frame("machine_offline")).await;
                                }
                                BrowserRoute::Ignored => {
                                    // bootstrap 时序豁免（见 AMBIGUOUS_SILENT_KINDS）：
                                    // 静默丢弃不回错——回 ambiguous_machine 只会让 SPA
                                    // 每次连接弹一次「多台机器在线」toast。
                                    tracing::debug!(target: TARGET, bytes = text.len(), "sync/ping 未指定 machine 且多台网关在线，静默丢弃");
                                }
                                    BrowserRoute::Ambiguous => {
                                        tracing::debug!(target: TARGET, bytes = text.len(), "未指定 machine 且多台网关在线，回 ambiguous_machine");
                                        let _ = socket.send(error_frame("ambiguous_machine")).await;
                                    }
                                },
                                Role::Gateway => {
                                    let targets = st.hub.route_gateway_frame();
                                    if targets.is_empty() {
                                        tracing::debug!(target: TARGET, bytes = text.len(), "无在线 browser，帧丢弃（不缓存）");
                                    } else {
                                        tracing::debug!(target: TARGET, bytes = text.len(), browsers = targets.len(), "gateway → browser 广播");
                                        // Utf8Bytes 是 Arc 语义，逐个 clone 广播零拷贝。
                                        let msg = Outbound::Msg(Message::Text(text));
                                        for b in targets {
                                            try_send(&b, msg.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            },
        }
    }

    // ---- 退出：清槽位（browser 只清自己；网关退出清 machine 槽位并广播）----
    if role == Role::Browser {
        let browsers = st.hub.release_browser(conn_id);
        // 多端排查：断开后还剩几条 browser 在线。
        tracing::info!(target: TARGET, role = role.name(), browsers, "连接断开");
    } else {
        let machine = auth_machine.unwrap_or_default();
        let (browsers, gateways) = st.hub.release_gateway(&machine, conn_id);
        tracing::info!(target: TARGET, role = role.name(), machine = %machine, gateways, browsers, "连接断开");
    }
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
        // 前缀路由与静态前缀剥离（spa_fallback）同款理由：兼容不经 Caddy
        // 的直连形态（本地开发/验收直开 http://127.0.0.1:3010/）——SPA 按
        // Vite base=/imagent/ 把 WS/探活地址推导为 /imagent/ws 等；公网
        // Caddy `handle_path /imagent/*` strip 后走上面的原路径。同一
        // handler，两种形态都可用。
        .route("/imagent/gateway", get(gateway_ws))
        .route("/imagent/ws", get(browser_ws))
        .route("/imagent/healthz", get(healthz))
        .fallback(spa_fallback)
        // 安全响应头（SPA 纵深防御）盖住**全部**响应：路由（WS upgrade 101 /
        // healthz）+ fallback（SPA 静态）。注意 layer 只作用于此前已注册的
        // 路由与 fallback，故放在 fallback 之后。
        .layer(middleware::from_fn(security_headers))
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

    /// hello 帧解析：合法 / 缺 token / 畸形 JSON / 非 hello 首帧 / 版本 / 空
    /// token；v1.1 machine：缺省 default / 显式 / trim / 空串与空白拒绝 /
    /// 非法字符拒绝 / 超 32 char 拒绝。
    #[test]
    fn hello_frame_parsing() {
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc"}"#),
            Ok(("abc".to_string(), "default".to_string())),
            "缺 machine → default"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","machine":"home-mac"}"#),
            Ok(("abc".to_string(), "home-mac".to_string())),
            "显式 machine"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","machine":" home-mac "}"#),
            Ok(("abc".to_string(), "home-mac".to_string())),
            "machine trim"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","machine":"work_01"}"#),
            Ok(("abc".to_string(), "work_01".to_string())),
            "下划线与数字合法"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","machine":""}"#),
            Err(HelloParseError::Malformed),
            "空串 machine 拒绝"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","machine":"   "}"#),
            Err(HelloParseError::Malformed),
            "trim 后为空拒绝"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","machine":"home mac"}"#),
            Err(HelloParseError::Malformed),
            "非法字符（空格）拒绝"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","machine":"家-mac"}"#),
            Err(HelloParseError::Malformed),
            "非 ASCII 拒绝（machine 是路由主键，限 ASCII 可打印子集）"
        );
        assert_eq!(
            parse_hello(&format!(
                r#"{{"v":1,"type":"hello","token":"abc","machine":"{}"}}"#,
                "m".repeat(MACHINE_ID_MAX_CHARS + 1)
            )),
            Err(HelloParseError::Malformed),
            "超 32 char 拒绝"
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

    /// 业务帧 machine 路由键提取：缺失 / 空串 / 空白 / 非字符串 / 畸形 JSON /
    /// 畸形但含 machine（JSON 解析失败时整体 None——relay 只在解析成功后取
    /// 单字段）。
    #[test]
    fn frame_machine_extraction() {
        assert_eq!(
            frame_machine(r#"{"v":1,"type":"message","text":"hi"}"#),
            None
        );
        assert_eq!(
            frame_machine(r#"{"v":1,"type":"message","machine":"home","text":"hi"}"#),
            Some("home".to_string())
        );
        assert_eq!(
            frame_machine(r#"{"v":1,"type":"ping","machine":"  work  "}"#),
            Some("work".to_string()),
            "路由键同样 trim"
        );
        assert_eq!(frame_machine(r#"{"machine":""}"#), None, "空串按未指定");
        assert_eq!(frame_machine(r#"{"machine":"   "}"#), None, "空白按未指定");
        assert_eq!(frame_machine(r#"{"machine":42}"#), None, "非字符串按未指定");
        assert_eq!(
            frame_machine("not json"),
            None,
            "畸形 JSON 按未指定（帧本身照旧盲转）"
        );
    }

    /// 帧 type 提取（歧义静默豁免判定用）：常规 / 缺失 / 非字符串 / 畸形。
    #[test]
    fn frame_kind_extraction() {
        assert_eq!(
            frame_kind(r#"{"v":1,"type":"sync"}"#),
            Some("sync".to_string())
        );
        assert_eq!(
            frame_kind(r#"{"v":1,"type":"ping","machine":"m"}"#),
            Some("ping".to_string())
        );
        assert_eq!(frame_kind(r#"{"v":1,"text":"无类型"}"#), None, "缺 type");
        assert_eq!(frame_kind(r#"{"type":42}"#), None, "非字符串");
        assert_eq!(frame_kind("not json"), None, "畸形 JSON");
        // 豁免判定：sync/ping 命中，message / 未知 / 畸形不豁免。
        assert!(is_ambiguous_silent_kind(r#"{"v":1,"type":"sync"}"#));
        assert!(is_ambiguous_silent_kind(r#"{"v":1,"type":"ping"}"#));
        assert!(!is_ambiguous_silent_kind(r#"{"v":1,"type":"message"}"#));
        assert!(!is_ambiguous_silent_kind(
            r#"{"v":1,"type":"approval_response"}"#
        ));
        assert!(!is_ambiguous_silent_kind(r#"{"v":1,"type":"control"}"#));
        assert!(!is_ambiguous_silent_kind("not json"));
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

    /// Hub 槽位语义（v1.1 多机版）：注册/路由/释放互不误伤（无网络，纯
    /// channel）。覆盖：多机器并存、同 machine 踢旧、browser 帧 machine
    /// 路由（指定命中 / 指定离线 / 缺省单台 / 缺省多台 ambiguous / 全下线
    /// 入缓冲）、release 广播帧形态。
    #[test]
    fn hub_register_route_release() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(false); // sender 保留，避免触发「控制端丢弃=停机」
        let hub = Hub::new(shutdown_rx);

        // 网关注册（default 机器）：空环境 → 无踢旧、无在线 browser、无重放；
        // machines 列表含 default、is_default=true。
        let (gw_tx, mut gw_rx) = mpsc::channel(8);
        let reg = hub.register_gateway("default", 1, gw_tx).unwrap();
        assert!(reg.superseded.is_none() && reg.browsers.is_empty() && reg.replay.is_empty());
        assert_eq!(reg.machines, vec!["default".to_string()]);
        assert_eq!(reg.gateways, 1);
        assert!(reg.is_default);

        // browser 注册（多连接并存）：default 机器在线快照 + 全量 machines 快照。
        let (br_tx, mut br_rx) = mpsc::channel(8);
        let regb = hub.register_browser(3, br_tx);
        assert!(regb.machine_online && !regb.full && regb.browsers == 1);
        assert_eq!(regb.machines, vec!["default".to_string()]);

        // 第二台机器注册（home）：并存不踢 default；browser 快照含两台。
        let (home_tx, mut home_rx) = mpsc::channel(8);
        let reg2 = hub.register_gateway("home", 2, home_tx).unwrap();
        assert!(reg2.superseded.is_none(), "不同机器不互踢");
        assert_eq!(
            reg2.machines,
            vec!["default".to_string(), "home".to_string()]
        );
        assert_eq!(reg2.gateways, 2);
        assert!(!reg2.is_default);

        // ---- machine 路由（v1.1 §3 四分支）----
        // 显式指定且在线 → 路由到对应机器。
        match hub.route_browser_frame(r#"{"v":1,"type":"message","machine":"home","text":"a"}"#) {
            BrowserRoute::Deliver(tx) => {
                try_send(&tx, Outbound::Msg(Message::text("a")));
                assert_eq!(
                    home_rx.try_recv().ok().and_then(|o| match o {
                        Outbound::Msg(m) => Some(m.into_text().unwrap().as_str().to_string()),
                        _ => None,
                    }),
                    Some("a".to_string()),
                    "machine=home 应路由到 home 网关"
                );
                // default 机器不受影响。
                assert!(gw_rx.try_recv().is_err(), "default 网关不应收到 home 帧");
            }
            other => panic!("machine=home 在线应 Deliver：{other:?}"),
        }
        match hub.route_browser_frame(r#"{"v":1,"type":"message","machine":"default"}"#) {
            BrowserRoute::Deliver(tx) => {
                try_send(&tx, Outbound::Msg(Message::text("b")));
                match gw_rx.try_recv() {
                    Ok(Outbound::Msg(m)) => assert_eq!(m.into_text().unwrap().as_str(), "b"),
                    other => panic!("应透传到 default 网关 channel：{other:?}"),
                }
            }
            other => panic!("machine=default 在线应 Deliver：{other:?}"),
        }
        // 显式指定但不在线 → MachineOffline。
        assert!(matches!(
            hub.route_browser_frame(r#"{"v":1,"type":"message","machine":"ghost"}"#),
            BrowserRoute::MachineOffline
        ));
        // 未指定 + 两台在线 → Ambiguous。
        assert!(matches!(
            hub.route_browser_frame(r#"{"v":1,"type":"message","text":"c"}"#),
            BrowserRoute::Ambiguous
        ));
        // bootstrap 豁免（v1.1）：sync/ping 未指定 + 多台在线 → 静默忽略
        //（不路由不回错）；畸形 JSON（type 不可知）不豁免，仍 Ambiguous。
        assert!(matches!(
            hub.route_browser_frame(r#"{"v":1,"type":"sync"}"#),
            BrowserRoute::Ignored
        ));
        assert!(matches!(
            hub.route_browser_frame(r#"{"v":1,"type":"ping"}"#),
            BrowserRoute::Ignored
        ));
        assert!(matches!(
            hub.route_browser_frame("not json"),
            BrowserRoute::Ambiguous
        ));

        // 踢旧：同 machine（default）第二条注册顶替第一条，旧投递口由调用方
        // close 4001（每台机器单连接语义）。
        let (gw2_tx, _gw2_rx) = mpsc::channel(8);
        let reg3 = hub.register_gateway("default", 5, gw2_tx).unwrap();
        let old = reg3.superseded.expect("同 machine 应踢旧");
        try_send(&old, Outbound::Close(CLOSE_SUPERSEDED, "superseded"));
        assert!(matches!(
            gw_rx.try_recv(),
            Ok(Outbound::Close(CLOSE_SUPERSEDED, "superseded"))
        ));
        assert_eq!(reg3.gateways, 2, "踢旧是顶替不是新增槽位");

        // 旧连接（conn_id=1）退出不得误清新槽位：default 机器仍在线。
        hub.release_gateway("default", 1);
        assert!(matches!(
            hub.route_browser_frame(r#"{"v":1,"type":"message","machine":"default","text":"d"}"#),
            BrowserRoute::Deliver(_)
        ));

        // 现 default 网关（conn_id=5）退出 → 广播：旧 machine(false)（default
        // 机器，v1 兼容）+ machines 全量列表（剩 home）；两帧顺序 machine 在前。
        hub.release_gateway("default", 5);
        match br_rx.try_recv() {
            Ok(Outbound::Msg(m)) => {
                let s = m.into_text().unwrap();
                assert!(
                    s.contains(r#""type":"machine""#) && s.contains("false"),
                    "旧 machine 帧（default 下线）：{s}"
                );
            }
            other => panic!("应先广播旧 machine 帧：{other:?}"),
        }
        match br_rx.try_recv() {
            Ok(Outbound::Msg(m)) => {
                let s = m.into_text().unwrap();
                assert!(
                    s.contains(r#""type":"machines""#)
                        && s.contains("home")
                        && !s.contains("default"),
                    "machines 列表应只剩 home：{s}"
                );
            }
            other => panic!("应后广播 machines 帧：{other:?}"),
        }

        // 未指定 + 仅剩一台（home）→ 路由它（单机部署 SPA 零升级无感）。
        assert!(matches!(
            hub.route_browser_frame(r#"{"v":1,"type":"message","text":"e"}"#),
            BrowserRoute::Deliver(_)
        ));

        // home 网关退出：非 default 机器 → 只广播 machines，无旧 machine 帧。
        hub.release_gateway("home", 2);
        match br_rx.try_recv() {
            Ok(Outbound::Msg(m)) => {
                let s = m.into_text().unwrap();
                assert!(s.contains(r#""type":"machines""#), "machines 广播：{s}");
            }
            other => panic!("应广播 machines 帧：{other:?}"),
        }
        assert!(
            br_rx.try_recv().is_err(),
            "非 default 机器不发旧 machine 帧"
        );

        // 全下线：未指定帧入缓冲（单 FIFO，重放序 = 入队序）；sync/ping 的
        // 静默豁免只作用于歧义分支（多台在线），零台在线沿用缓冲语义。
        let frames = ["1", "2", "3", r#"{"v":1,"type":"sync"}"#];
        for f in frames {
            assert!(matches!(hub.route_browser_frame(f), BrowserRoute::Buffered));
        }
        let (gw3_tx, _gw3_rx) = mpsc::channel(8);
        let reg4 = hub.register_gateway("home", 9, gw3_tx).unwrap();
        assert_eq!(reg4.replay, frames, "重放保 FIFO 序");
        assert_eq!(reg4.browsers.len(), 1, "上线广播目标含全部在线 browser");

        // browser 退出只清自己：br（conn_id=3）退出后广播目标清零。
        assert_eq!(hub.release_browser(3), 0);
        assert!(hub.route_gateway_frame().is_empty());
    }

    /// 网关机器数上限：第 [`MAX_GATEWAYS`] 台（不同 machine）注册成功，第
    /// MAX_GATEWAYS+1 台 None（调用方 close 4001）；**同 machine 重连不受
    /// 上限约束**（踢旧顶替，槽位复用）；释放一台后新 machine 可再入。
    #[test]
    fn hub_gateway_machine_cap() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let hub = Hub::new(shutdown_rx);

        for i in 1..=MAX_GATEWAYS {
            let (tx, _rx) = mpsc::channel(8);
            let reg = hub
                .register_gateway(&format!("m{i}"), i as u64, tx)
                .unwrap_or_else(|| panic!("第 {i} 台应注册成功"));
            assert_eq!(reg.gateways, i);
        }
        // 满员 + 新 machine → None。
        let (tx, _rx) = mpsc::channel(8);
        assert!(
            hub.register_gateway("m-extra", 99, tx).is_none(),
            "满员新机器应被拒"
        );
        // 满员 + 同 machine 重连 → 踢旧顶替（不受上限约束）。
        let (tx2, _rx2) = mpsc::channel(8);
        let reg = hub.register_gateway("m2", 100, tx2).unwrap();
        assert!(reg.superseded.is_some(), "同 machine 重连踢旧");
        assert_eq!(reg.gateways, MAX_GATEWAYS);
        // 旧连接（conn_id=2）退出不误清新连接。
        hub.release_gateway("m2", 2);
        let (tx3, _rx3) = mpsc::channel(8);
        assert!(
            hub.register_gateway("m2", 101, tx3).is_some(),
            "新连接仍在槽位，同 machine 再连仍走踢旧"
        );
        // 释放一台 → 新 machine 槽位立即可复用。
        hub.release_gateway("m1", 1);
        let (tx4, _rx4) = mpsc::channel(8);
        assert!(hub.register_gateway("m-extra", 102, tx4).is_some());
    }

    /// browser 并存上限：第 [`MAX_BROWSERS`] 条注册成功，第 MAX_BROWSERS+1
    /// 条 full（未入表，调用方 close 4001 自己）；未注册 conn_id 的退出清
    /// 槽位是 no-op；释放一条后槽位立即可复用。
    #[test]
    fn hub_browser_cap() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let hub = Hub::new(shutdown_rx);

        for i in 1..=MAX_BROWSERS {
            let (tx, _rx) = mpsc::channel(8);
            let reg = hub.register_browser(i as u64, tx);
            assert!(!reg.full, "第 {i} 条应注册成功");
            assert_eq!(reg.browsers, i);
        }
        // 超上限：full=true 且不注册（该连接随后 close 4001 自己）。
        let (tx, _rx) = mpsc::channel(8);
        let reg = hub.register_browser(999, tx);
        assert!(reg.full);
        assert_eq!(reg.browsers, MAX_BROWSERS, "满员时在线数不变");

        // 未注册的 conn_id 退出是 no-op（不误伤在线连接）。
        assert_eq!(hub.release_browser(999), MAX_BROWSERS);

        // 释放一条 → 槽位立即可复用。
        assert_eq!(hub.release_browser(1), MAX_BROWSERS - 1);
        let (tx, _rx) = mpsc::channel(8);
        let reg = hub.register_browser(1000, tx);
        assert!(!reg.full && reg.browsers == MAX_BROWSERS);
    }
}
