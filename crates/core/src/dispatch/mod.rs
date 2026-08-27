//! 消息调度核心。
//!
//! `Dispatcher` 持有注入的 `Arc<dyn Platform>` / `Arc<dyn Backend>` / `Store` /
//! `Auth` / 配置，循环 `platform.recv()` 并对每条消息 `tokio::spawn` 处理。
//!
//! 两条硬约束在此体现：
//! 1. 非白名单 sender 丢弃；发现模式（白名单为空）回引导消息但不驱动 agent。
//! 2. backend 只用配置的 `allowed_tools`、workdir 用配置的 `default_workdir`。
//!
//! 结构（5238 行巨石拆分，见 P4_ROADMAP 第六批）：本文件保留 Dispatcher 状态与
//! 生命周期（构造 / run 主循环 / conv 锁与批处理 runner / reply 基元）；
//! [`commands`] 是斜杠命令分派；[`round`] 是单轮 agent 状态机；[`socket`] 是
//! 权限审批 Unix socket；`tests` 集中全部单测。

mod commands;
mod round;
mod socket;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::auth::Auth;
use crate::backend::Backend;
use crate::card_session::CardSession;
use crate::config::{CotDetail, PermissionMode, ReplyMode};
use crate::error::Result;
use crate::metrics::METRICS;
use crate::permission::{is_explicit_reply_word, parse_reply, PendingKind, PermissionReply, PermissionRouter};
use crate::platform::Platform;
use crate::types::{
    AgentChunk, CardButton, CardButtonStyle, CardTerminal, ConfigFormField, ConvId, InboundMessage,
    MediaRef, ReplyHint, SessionId, ToolCall,
};
use imagent_store::{NamedSessionRow, SessionRow, Store};
use parking_lot::RwLock;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

/// per-conv 排队消息上限：runner 在飞期间到达的消息暂存条数。超出回告警并丢弃，
/// 防刷屏把合并后的 prompt 撑爆。
const PENDING_QUEUE_CAP: usize = 100;

/// D2：存在待审批 pending 时，对「未被消费的自由文本」的提示去重间隔——
/// 同一 conv 在该窗口内只提示一次，避免每条消息都刷屏。
const PENDING_HINT_DEDUPE: Duration = Duration::from_secs(60);

/// D7：`/resume` 序号缓存的有效期——缓存按 (conv, sender) 隔离，过期防止
/// 陈旧序号在列表变化后错位。
const RESUME_CACHE_TTL: Duration = Duration::from_secs(600);

/// Dispatcher 时长类预算聚合（避免构造参数表随配置项继续膨胀）。
#[derive(Debug, Clone, Copy)]
pub struct TaskBudgets {
    /// 单次 agent 运行总超时（`agent_timeout_secs`）。
    pub agent_timeout: Duration,
    /// Ask 权限审批等待回复超时（`permission_ask_timeout_secs`，独立预算）。
    pub permission_ask_timeout: Duration,
    /// 终端 agent `ask_via_im` 等待回复的默认超时（`ask_via_im_timeout_secs`；
    /// 可被请求的 timeout_secs 覆盖，上限 86400）。
    pub ask_via_im_timeout: Duration,
    /// 优雅退出 drain in-flight task 宽限（`shutdown_grace_secs`）。
    pub shutdown_grace: Duration,
    /// 空闲看门狗：agent 连续无输出该时长则终止本轮（`agent_idle_timeout_secs`；
    /// 零值 = 关闭）。
    pub agent_idle_timeout: Duration,
    /// 批处理窗口：runner 起跑前等待后续消息并入同一轮的时长（`batch_window_ms`；
    /// 零值 = 关闭）。
    pub batch_window: Duration,
}

impl TaskBudgets {
    /// 从 Config 构造（单位换算集中在这一处）。
    pub fn from_config(c: &crate::config::Config) -> Self {
        Self {
            agent_timeout: Duration::from_secs(c.agent_timeout_secs),
            permission_ask_timeout: Duration::from_secs(c.permission_ask_timeout_secs),
            ask_via_im_timeout: Duration::from_secs(c.ask_via_im_timeout_secs),
            shutdown_grace: Duration::from_secs(c.shutdown_grace_secs),
            agent_idle_timeout: Duration::from_secs(c.agent_idle_timeout_secs),
            batch_window: Duration::from_millis(c.batch_window_ms),
        }
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
/// 把字符串按字符截断到 n 个字符，超出则加省略号。
fn truncate_str(s: &str, n: usize) -> String {
    let count = s.chars().count();
    let t: String = s.chars().take(n).collect();
    if count > n {
        format!("{t}…")
    } else {
        t
    }
}

/// 人读运行时长（/status 用）：`2d3h` / `4h05m` / `7m` / `42s`。
fn format_uptime(d: Duration) -> String {
    let secs = d.as_secs();
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    let s = secs % 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else if mins > 0 {
        format!("{mins}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// epoch 秒 → 相对时间（`/resume` 列表用）：`42秒前` / `5分钟前` / `3小时前` /
/// `2天前`；超 7 天回退原始时间戳（避免引日期库）。
fn format_rel_ts(ts: i64) -> String {
    let d = (now_secs() - ts).max(0);
    if d < 60 {
        format!("{d}秒前")
    } else if d < 3_600 {
        format!("{}分钟前", d / 60)
    } else if d < 86_400 {
        format!("{}小时前", d / 3_600)
    } else if d < 7 * 86_400 {
        format!("{}天前", d / 86_400)
    } else {
        format!("（{ts}）")
    }
}

/// 格式化工具调用摘要：按 COT 档位展示（P4-6），超出 `max` 标 `…(+N)`。
/// P8-1：摘要是人可读单行（`Bash — git status`），形如
/// `\n\n🔧 工具调用：Bash — git status，Read — src/main.rs …(+3)`。
fn format_tool_summary(tool_calls: &[ToolCall], detail: CotDetail) -> String {
    let max = detail.max_tools();
    let shown: Vec<String> = tool_calls
        .iter()
        .take(max)
        .map(crate::render::tool_text_line)
        .collect();
    let mut s = format!("\n\n🔧 工具调用：{}", shown.join("，"));
    if tool_calls.len() > max {
        s.push_str(&format!(" …(+{})", tool_calls.len() - max));
    }
    s
}
/// 当前活动命名 session 的 config 键：`active_name:<conv_id>`。
/// 不存在/空值表示当前会话为默认未命名 session。
fn active_name_key(conv_id: &str) -> String {
    format!("active_name:{conv_id}")
}
/// 压缩摘要的 config 键：`compact_summary:<conv_id>`。
/// 由 /compact 写入，下次新建 session 时作为前情摘要注入后清除（一次性）。
fn compact_summary_key(conv_id: &str) -> String {
    format!("compact_summary:{conv_id}")
}
/// per-conv 工作目录的 config 键：`workdir:<conv_id>`（由 /cd 设置，覆盖默认 workdir）。
fn workdir_key(conv_id: &str) -> String {
    format!("workdir:{conv_id}")
}

/// 命名工作空间的 config 键：`workspace:<name>`（全局别名，所有 conv 共享）。由 /ws 设置。
fn workspace_key(name: &str) -> String {
    format!("workspace:{name}")
}

/// 错误是否指示 iLink session 过期（需重新 login）。
///
/// 专用 `CoreError::SessionExpired` variant，靠类型判定而非 Display 子串（更鲁棒）。
fn is_session_expired_err(e: &crate::error::CoreError) -> bool {
    matches!(e, crate::error::CoreError::SessionExpired(_))
}

/// 消息是否可能作为权限审批回复被消费：非空且非斜杠命令。
/// 斜杠命令（如 `/stop`）在等待审批期间也必须可执行——否则会被当 deny 吞掉，
/// 用户将无法中断正等审批的任务；空文本（纯媒体消息）同样不消费。
fn is_permission_reply_candidate(text: &str) -> bool {
    let t = text.trim();
    !t.is_empty() && !t.starts_with('/')
}

/// 合并一批排队消息为一轮 prompt 载体：非空文本以 `\n\n` 拼接、media /
/// media_errors 拼接；sender 与 reply_hint 取首条（各消息入队前已各自过白名单）。
fn merge_batch(batch: Vec<InboundMessage>) -> InboundMessage {
    let mut it = batch.into_iter();
    let mut first = it.next().expect("merge_batch: batch 非空");
    let mut texts: Vec<String> = first
        .text
        .take()
        .filter(|t| !t.trim().is_empty())
        .into_iter()
        .collect();
    for m in it {
        if let Some(t) = m.text.filter(|t| !t.trim().is_empty()) {
            texts.push(t);
        }
        first.media.extend(m.media);
        first.media_errors.extend(m.media_errors);
    }
    first.text = if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n\n"))
    };
    first
}

/// 统一 `/resume` 列表条目（P4-11）：IM 会话历史 ∪ 本机同项目 agent 会话。
#[derive(Debug, Clone)]
struct ResumeEntry {
    session_id: String,
    /// epoch 秒。
    updated_at: i64,
    /// 产生该会话的后端类型（历史行带原始 kind；本机会话按当前后端）。
    agent_kind: String,
    /// 首条用户消息摘要（本机扫描有；纯历史行可能空，展示回退 id 前缀）。
    first_prompt: String,
    /// 本机（电脑端）会话——不在 IM 历史表里的扫描结果；接管时附分叉提示。
    from_local: bool,
    /// 本机会话记录的工作目录（jsonl cwd；P5-15 接管前校验用）。
    cwd: Option<String>,
}

pub struct Dispatcher {
    platform: Arc<dyn Platform>,
    backend: Arc<dyn Backend>,
    store: Store,
    auth: Auth,
    default_workdir: PathBuf,
    allowed_tools: Arc<RwLock<Vec<String>>>,
    /// per-conv 串行锁：同一会话的 agent 任务排队执行，避免 session 冲突。
    conv_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// IM 权限审批路由（Ask 闭环用）。
    router: Arc<PermissionRouter>,
    /// 权限审批模式。
    permission_mode: Arc<RwLock<PermissionMode>>,
    /// 单次 agent 运行超时。超时则中止该次 run（backend 的 kill_on_drop 杀子进程）。
    agent_timeout: std::time::Duration,
    /// 权限审批（Ask）等待用户回复的超时（S-3：独立预算，不挤占 agent_timeout）。
    permission_ask_timeout: std::time::Duration,
    /// 终端 agent `ask_via_im` 等待回复的默认超时（socket ask 分支用）。
    ask_via_im_timeout: std::time::Duration,
    /// 优雅退出 drain in-flight task 的宽限期（R-1：原硬编码 30s）。
    shutdown_grace: std::time::Duration,
    /// 空闲看门狗：agent 连续无输出该时长则终止本轮（零值 = 关闭）。
    /// `/config agent_idle_timeout_secs` 可热改，故共享句柄。
    agent_idle_timeout: Arc<RwLock<Duration>>,
    /// 批处理窗口：runner 起跑前等待后续消息并入同一轮的时长（零值 = 关闭）。
    /// `/config batch_window_ms` 可热改，故共享句柄。
    batch_window: Arc<RwLock<Duration>>,
    /// 工具过程（COT）展示档位（P4-6）：`/config cot_detail` 可热改。
    cot_detail: Arc<RwLock<CotDetail>>,
    /// 进程启动时刻（`/status` uptime 用）。
    started_at: Instant,
    /// per-conv 在飞 agent 任务注册表（`/stop` 中断用）：conv_id → join task 的
    /// AbortHandle。同 conv 轮次串行（conv 锁保证），key 插入/移除无 ABA。
    running: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    /// per-conv 批处理队列：runner 在飞期间到达的消息暂存（entry 存在 = runner
    /// 活跃；runner 取空交还时移除）。入队与取批共用一把锁，杜绝 lost-wakeup。
    queues: Mutex<HashMap<String, Vec<InboundMessage>>>,
    /// per-conv 最近一次 `/resume` 渲染的列表（P4-11）：序号选择取缓存，
    /// 防两次调用间本机会话 mtime 变化导致错位；选中即消费（移除）。
    /// D7：key 为 (conv, sender)——群聊多用户共用 conv，仅按 conv 缓存会互相
    /// 覆盖错位；值带写入时刻，超过 [`RESUME_CACHE_TTL`] 惰性过期。
    resume_cache: Mutex<HashMap<(String, String), (Instant, Vec<ResumeEntry>)>>,
    /// P6-9：per-conv 空闲看门狗覆盖（`/timeout`）——`Some(ZERO)` = 本会话关闭；
    /// 无条目 = 跟随全局 `agent_idle_timeout`。进程内（会话级旋钮，不落盘）。
    idle_overrides: Mutex<HashMap<String, Duration>>,
    /// P7-A3：陌生人被 @ 提示开关（config 注入，set_prefs 热设；共享句柄）。
    stranger_mention_hint: RwLock<bool>,
    /// P7-A4：回复形态偏好（card/text，/config 可热改）。
    reply_mode: Arc<RwLock<ReplyMode>>,
    /// 审批集（ask 模式下仅清单内工具过 IM 审批，其余放行；空 = 全部过审）。
    /// main 启动注入 + SIGHUP 热重载（见 [`Self::set_approval_tools`]）。
    approval_tools: Arc<RwLock<Vec<String>>>,
    /// 管理员 sender（可 /allow /config /perm /admin）。S2：空 = **无人**是
    /// 管理员（IM 内管理命令全部不可用，须通过 CLI / setup 配置 admin_senders）。
    admin_senders: Arc<RwLock<Vec<String>>>,
    /// D2：per-conv 最近一次「存在待审批项」提示的时刻（PENDING_HINT_DEDUPE 去重）。
    pending_hint_last: Mutex<HashMap<String, Instant>>,
    /// 优雅退出信号（P1-5）：收到 SIGINT/SIGTERM 后 cancel，run() 停止收新消息并
    /// drain。D4：改用 CancellationToken（持久信号）——`Notify::notify_waiters` 只
    /// 唤醒**已注册**的等待者，信号先于监听者 await 到达时存在丢失窗口。
    shutdown: Arc<tokio_util::sync::CancellationToken>,
    /// in-flight handle task 集合（P1-5）：drain 时等待其完成，避免 SIGKILL 正在
    /// 写文件的 agent 子进程导致半写。task 完成自动移除。
    tasks: Mutex<tokio::task::JoinSet<()>>,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        platform: Arc<dyn Platform>,
        backend: Arc<dyn Backend>,
        store: Store,
        auth: Auth,
        default_workdir: PathBuf,
        allowed_tools: Vec<String>,
        permission_mode: PermissionMode,
        budgets: TaskBudgets,
        cot_detail: CotDetail,
        admin_senders: Vec<String>,
    ) -> Self {
        Self::new_with_handles(
            platform,
            backend,
            store,
            auth,
            default_workdir,
            Arc::new(RwLock::new(allowed_tools)),
            Arc::new(RwLock::new(permission_mode)),
            budgets,
            cot_detail,
            admin_senders,
        )
    }

    /// 与 [`new`](Self::new) 相同，但接受外部持有的共享句柄
    /// （`allowed_tools` / `permission_mode` 的 `Arc<RwLock>`）。
    ///
    /// main 用此构造，把 `permission_mode` 句柄同时共享给 `ClaudeBackend`，
    /// 使 SIGHUP 热重载对二者同时生效。
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_handles(
        platform: Arc<dyn Platform>,
        backend: Arc<dyn Backend>,
        store: Store,
        auth: Auth,
        default_workdir: PathBuf,
        allowed_tools: Arc<RwLock<Vec<String>>>,
        permission_mode: Arc<RwLock<PermissionMode>>,
        budgets: TaskBudgets,
        cot_detail: CotDetail,
        admin_senders: Vec<String>,
    ) -> Self {
        let disp = Self {
            platform,
            backend,
            store,
            auth,
            default_workdir,
            allowed_tools,
            conv_locks: Mutex::new(HashMap::new()),
            router: Arc::new(PermissionRouter::new()),
            permission_mode,
            agent_timeout: budgets.agent_timeout,
            permission_ask_timeout: budgets.permission_ask_timeout,
            ask_via_im_timeout: budgets.ask_via_im_timeout,
            shutdown_grace: budgets.shutdown_grace,
            agent_idle_timeout: Arc::new(RwLock::new(budgets.agent_idle_timeout)),
            batch_window: Arc::new(RwLock::new(budgets.batch_window)),
            cot_detail: Arc::new(RwLock::new(cot_detail)),
            started_at: Instant::now(),
            running: Mutex::new(HashMap::new()),
            queues: Mutex::new(HashMap::new()),
            resume_cache: Mutex::new(HashMap::new()),
            pending_hint_last: Mutex::new(HashMap::new()),
            idle_overrides: Mutex::new(HashMap::new()),
            stranger_mention_hint: RwLock::new(false),
            reply_mode: Arc::new(RwLock::new(ReplyMode::Card)),
            approval_tools: Arc::new(RwLock::new(Vec::new())),
            admin_senders: Arc::new(RwLock::new(admin_senders)),
            shutdown: Arc::new(tokio_util::sync::CancellationToken::new()),
            tasks: Mutex::new(tokio::task::JoinSet::new()),
        };
        // S2：admin_senders 为空 = 无人是管理员，IM 内管理命令全部不可用——
        // 构造即显著提示（防用户以为白名单用户仍可 /allow）。
        if disp.admin_senders.read().is_empty() {
            warn!(
                target: "imagent::core",
                "admin_senders 为空，IM 内管理命令不可用；请通过 CLI（imagent setup / config.toml admin_senders）配置管理员"
            );
        }
        disp
    }

    /// 审批集注入/热重载（main 启动与 SIGHUP 调用；空 = 全部权限请求过审）。
    pub fn set_approval_tools(&self, tools: Vec<String>) {
        *self.approval_tools.write() = tools;
    }

    /// P7：启动偏好注入（main 在 run 前调一次；构造器保持零新参，测试无感）。
    pub fn set_prefs(&self, stranger_mention_hint: bool, reply_mode: ReplyMode) {
        *self.stranger_mention_hint.write() = stranger_mention_hint;
        *self.reply_mode.write() = reply_mode;
    }

    /// P6-9：该会话的空闲看门狗——`/timeout` 覆盖优先（ZERO=关），否则全局值。
    async fn idle_timeout_for(&self, conv: &str) -> Duration {
        if let Some(d) = self.idle_overrides.lock().await.get(conv) {
            return *d;
        }
        *self.agent_idle_timeout.read()
    }

    /// 调用者是否为管理员（可 /allow /config /perm /admin）。S2：admin_senders
    /// 空 = **无人**是管理员（旧「空 = 全员可」语义使群部署下任意白名单成员可
    /// 自扩权，已收紧）；非空则严格匹配（P2-D）。
    fn is_admin(&self, sender: &str) -> bool {
        let admins = self.admin_senders.read();
        let trimmed = sender.trim();
        admins.iter().any(|a| a.trim() == trimmed)
    }

    /// P5-1/S1（安全）：审批回复的发送者须过 **sender 白名单**（或为管理员）。
    /// 审批路由发生在 handle() **之前**，天然绕过其鉴权；旧「sender OR 会话白
    /// 名单」门在群被 `/chat allow` 加白后，任意群成员发 "y" 即可批准 Bash 等
    /// 高危工具——群白名单只代表「可对话」，不代表「可批高危操作」，故收紧为
    /// 仅 sender 白名单（管理员兜底）。飞书审批按钮回调携带 operator open_id
    /// 作 sender，同一门槛覆盖按钮路径。
    fn can_route_permission_reply(&self, msg: &InboundMessage) -> bool {
        self.auth.is_allowed(&msg.sender) || self.is_admin(&msg.sender.0)
    }

    /// SIGHUP 热重载：整体替换 allowed_tools。
    pub fn reload_tools(&self, tools: Vec<String>) {
        *self.allowed_tools.write() = tools;
    }

    /// SIGHUP 热重载：更新 permission_mode（与 ClaudeBackend 共享同一句柄时
    /// 二者同步生效）。注意：Ask 模式的 socket accept task 仅在 `run()` 启动时
    /// 按当时的模式 spawn 一次，热切到 Ask 不会补起 socket（重启生效）。
    pub fn reload_permission_mode(&self, mode: PermissionMode) {
        *self.permission_mode.write() = mode;
    }

    /// 暴露 auth（main 的 SIGHUP task 用其 reload）。
    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    /// 暴露 router（主进程 socket accept task 用）。
    pub fn router(&self) -> Arc<PermissionRouter> {
        self.router.clone()
    }

    /// 触发优雅退出（P1-5）：run() 收到后停止 recv 并 drain in-flight task。
    /// 由 main 的信号处理 task 调用（SIGINT/SIGTERM）。
    pub fn shutdown(&self) {
        // D4：CancellationToken 持久——先 cancel 后监听也不会丢信号。
        self.shutdown.cancel();
    }

    /// 主循环。循环 `platform.recv()`，每条消息 `tokio::spawn` 处理（不阻塞 recv）。
    /// recv 返回 Err 时：session 过期 → 优雅停止（返回 Err 让 main 提示重新 login）；
    /// 其它错误 → 指数退避后继续重试（防 client 异常退出导致 dispatcher 忙循环刷屏；ilink 长轮询层另有退避），不 panic。
    pub async fn run(self: Arc<Self>) -> Result<()> {
        // Ask 模式：spawn unix socket accept task（MCP server 转发的权限请求经此进主进程）。
        #[cfg(unix)]
        if self.permission_mode.read().needs_socket() {
            if let Some(sock) = crate::permission::default_sock_path() {
                self.spawn_socket_accept(sock.to_string_lossy().into_owned());
            } else {
                warn!(target: "imagent::core", "Ask 模式但无法定位 socket 路径，权限请求将无法路由");
            }
        }
        #[cfg(not(unix))]
        if self.permission_mode.read().needs_socket() {
            warn!(
                target: "imagent::core",
                "Ask 权限审批闭环需要 Unix domain socket，当前平台(Windows)不可用；请改用 permission_mode = allow/deny/off 或在 macOS/Linux 运行"
            );
        }

        // recv 失败退避（防 client 异常退出后 dispatcher 忙循环刷屏）。
        let mut recv_backoff = std::time::Duration::from_secs(1);
        const RECV_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(30);
        loop {
            // P1-5：监听 shutdown 信号，停止接收新消息并进入 drain。
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => {
                    info!(target: "imagent::core", "shutdown 信号到达，停止接收新消息，drain in-flight task");
                    break;
                }
                msg = self.platform.recv() => match msg {
                    Ok(msg) => {
                        recv_backoff = std::time::Duration::from_secs(1); // 成功，重置退避
                        let conv_id = msg.conv_id.0.clone();
                        // 权限闭环优先：若该 conv 正等待 approve/deny 回复，把这条消息
                        // 当作回复送达 oneshot。P2-2：直接 route（单次 lock 原子 check+
                        // remove+send），避免旧 has_pending→route 两次 lock 间隙被超时
                        // 清理（P1-8 cancel）击穿，导致 "yes" 误走 fallforward 当新 prompt。
                        // 斜杠命令不消费（/stop 在等审批时也要可执行），空文本（纯媒体）
                        // 同样不消费。P5-1：发送者须过白名单才可被消费（防群聊陌生人
                        // 用 "y" 批准权限请求）；未过门的消息落到 handle() 走正常鉴权
                        // 丢弃路径。
                        let text = msg.text.as_deref().unwrap_or("");
                        if is_permission_reply_candidate(text)
                            && self.can_route_permission_reply(&msg)
                        {
                            // D2：自由文本（无按钮回调 ask_req / 无引用回复
                            // reply_to 锚定询问卡）只有在**明确命中审批词表**
                            // （y/n 全字匹配，见 is_explicit_reply_word）时才可被
                            // 消费；否则回落正常 handle/批处理路径，不再被当
                            // deny 兜底吞掉。多 pending 并存且无锚定时无法消解
                            // 歧义，同样不消费，回一条去重提示引导回复卡片。
                            let anchored = msg.ask_req.is_some() || msg.reply_to.is_some();
                            let explicit_word = is_explicit_reply_word(text);
                            let consumable = if anchored {
                                true
                            } else if !explicit_word {
                                false
                            } else {
                                matches!(self.router.pending_count(&conv_id).await, 0 | 1)
                            };
                            let reply = parse_reply(text);
                            let reply_for_card = reply.clone();
                            // 多 pending 三级路由：按钮回调带 ask_req 精确 → 引用
                            // 回复（reply_to）命中询问卡 → 最新 pending 兜底。
                            let routed = if consumable {
                                self.router
                                    .route(
                                        &conv_id,
                                        msg.ask_req.as_deref(),
                                        msg.reply_to.as_deref(),
                                        reply,
                                    )
                                    .await
                            } else {
                                None
                            };
                            if let Some(req) = routed {
                                // 真机校准 UX：决策已达 MCP，立即把询问卡收敛成
                                // 「已批准/已拒绝」终态（best-effort，无卡 no-op）；
                                // 问题卡（P6）显示「已记录你的选择：<选项>」。
                                if let Err(e) = self
                                    .platform
                                    .resolve_permission_ask(&msg.conv_id, &req, &reply_for_card)
                                    .await
                                {
                                    tracing::warn!(
                                        target: "imagent::core",
                                        error = %e,
                                        "询问卡收敛失败（不影响审批结果）"
                                    );
                                }
                                continue;
                            }
                            // D2：未被消费但确有 pending——回一条去重提示，引导
                            // 用户回复询问卡（或 y/n），避免静默落进 agent 批处理
                            // 造成「发了没人理」的困惑；60s 窗口去重防刷屏。
                            if self.router.has_pending(&conv_id).await {
                                let now = Instant::now();
                                let mut last = self.pending_hint_last.lock().await;
                                let due = last
                                    .get(&conv_id)
                                    .is_none_or(|t| now.duration_since(*t) >= PENDING_HINT_DEDUPE);
                                if due {
                                    last.insert(conv_id.clone(), now);
                                    drop(last);
                                    let n = self.router.pending_count(&conv_id).await;
                                    self.reply(
                                        &msg.conv_id,
                                        &format!(
                                            "⚠️ 当前有 {n} 项待审批/待回答的询问，请回复对应询问卡（多待决时需引用对应卡片），或直接回复 y / n 表态。"
                                        ),
                                        &msg.reply_hint,
                                    )
                                    .await;
                                }
                            }
                        }
                        // 每条消息独立 spawn，不阻塞 recv。P1-5：入 JoinSet 以便 drain。
                        let this = self.clone();
                        self.tasks.lock().await.spawn(async move {
                            this.handle(msg).await;
                        });
                    }
                    Err(e) => {
                        if is_session_expired_err(&e) {
                            tracing::error!(
                                target: "imagent::core",
                                error = %e,
                                "session 过期，停止 dispatcher（需重新 login）"
                            );
                            return Err(e);
                        }
                        warn!(
                            target: "imagent::core",
                            error = %e,
                            backoff_secs = recv_backoff.as_secs(),
                            "platform.recv 失败，退避后继续重试（防忙循环刷屏）"
                        );
                        tokio::time::sleep(recv_backoff).await;
                        recv_backoff = (recv_backoff * 2).min(RECV_BACKOFF_CAP);
                    }
                },
            }
        }
        // P1-5/R-1：drain in-flight handle task（最多 shutdown_grace，默认 60s），超时 abort。
        // 避免 SIGKILL 正在写文件的 agent 子进程导致半写；超时兜底防无限等待。
        // R-2：handle_permission_socket 也纳入 self.tasks，drain 一并等待。
        let mut tasks = self.tasks.lock().await;
        let drain = async { while tasks.join_next().await.is_some() {} };
        match tokio::time::timeout(self.shutdown_grace, drain).await {
            Ok(_) => info!(target: "imagent::core", "drain 完成（in-flight task 已结束）"),
            Err(_) => {
                warn!(
                    target: "imagent::core",
                    grace = ?self.shutdown_grace,
                    "drain 超时，abort 剩余 in-flight task"
                );
                tasks.abort_all();
            }
        }
        Ok(())
    }

    /// 取（或创建）conv 串行锁的 Arc clone。
    /// P1-F：slash 命令复用，与普通消息 agent task 串行（避免并发改 session 损坏状态）。
    /// 回收沿用普通消息路径的 release（strong_count==1 时 remove）；slash 路径不显式
    /// release，其 lock clone drop 后由下次普通消息 release 清理（延迟回收，最终回收）。
    async fn acquire_conv_lock(&self, conv: &str) -> Arc<Mutex<()>> {
        let mut map = self.conv_locks.lock().await;
        map.entry(conv.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// 回收 conv 串行锁（P1-7：失败/正常路径统一调用，防 conv_locks HashMap 项
    /// 在 backend 失败/panic 的 return 路径永久泄漏）。调用方需先 drop guard 再传 lock。
    /// strong_count==1 表示只剩 HashMap 那份，安全移除；竞态最坏漏清（下轮再来）。
    async fn release_conv_lock(&self, conv: &str, lock: Arc<Mutex<()>>) {
        drop(lock);
        let mut map = self.conv_locks.lock().await;
        if let Some(arc) = map.get(conv) {
            if Arc::strong_count(arc) == 1 {
                map.remove(conv);
            }
        }
    }

    /// 普通消息入队（P4-2 批处理）：runner 在飞 → push pending 返回 false（本 task
    /// 即返，消息将在下一轮合并）；无 runner → 建 entry 返回 true（调用方成为
    /// runner）。入队/成为 runner 在同一把 queues 锁内原子判定——与
    /// [`take_batch_after_window`](Self::take_batch_after_window) 的取批/交还互斥，
    /// 杜绝「消息卡在无人认领的队列」（lost-wakeup）。超上限回告警并丢弃。
    async fn enqueue_or_become_runner(
        &self,
        conv: &str,
        msg: InboundMessage,
        hint: &ReplyHint,
    ) -> bool {
        let mut map = self.queues.lock().await;
        match map.get_mut(conv) {
            Some(pending) => {
                if pending.len() >= PENDING_QUEUE_CAP {
                    drop(map);
                    warn!(
                        target: "imagent::core",
                        conv_id = %conv,
                        cap = PENDING_QUEUE_CAP,
                        "排队消息超上限，丢弃本条"
                    );
                    self.reply(
                        &ConvId(conv.to_string()),
                        &format!("⚠️ 排队消息已达上限（{PENDING_QUEUE_CAP} 条），本条已丢弃；如需立即处理请发 /stop 中断当前任务后重发"),
                        hint,
                    )
                    .await;
                    return false;
                }
                info!(target: "imagent::core", conv_id = %conv, "runner 在飞，消息入队待下一轮合并");
                pending.push(msg);
                false
            }
            None => {
                map.insert(conv.to_string(), vec![msg]);
                true
            }
        }
    }

    /// runner 起跑前等批处理窗口，然后原子取批：pending 空 → 删 entry（交还 runner
    /// 身份）返回 None；非空 → drain 返回 Some（窗口期入队的消息自然并入本批）。
    async fn take_batch_after_window(&self, conv: &str) -> Option<Vec<InboundMessage>> {
        let window = *self.batch_window.read();
        if !window.is_zero() {
            tokio::time::sleep(window).await;
        }
        let mut map = self.queues.lock().await;
        let pending = map.get_mut(conv)?;
        if pending.is_empty() {
            map.remove(conv);
            return None;
        }
        Some(std::mem::take(pending))
    }

    /// 统一 `/resume` 列表（P4-11）：IM 会话历史（store `session_history`）∪
    /// 本机同项目 agent 会话（`Backend::list_local_sessions`，按 conv 当前
    /// workdir 扫描——workdir 对齐由扫描天然保证；`/cd` 切换后列表随之变化）。
    ///
    /// 归属标注：在历史表里的 id 标 📱（IM 创建，含也被扫到的），仅本机扫描出的
    /// 标 💻；历史里有但扫描没有的（其它 backend 会话/文件已删）仍列出（📱）。
    /// 按时间倒序取前 10。
    async fn merged_resume_list(&self, conv: &str) -> Vec<ResumeEntry> {
        const MAX: usize = 10;
        let history = self
            .store
            .list_session_history(conv, 50)
            .await
            .unwrap_or_default();
        let hist_kinds: HashMap<String, Option<String>> = history
            .iter()
            .map(|r| (r.session_id.clone(), r.agent_kind.clone()))
            .collect();
        let wd = self.resolve_workdir(conv).await;
        let local = self.backend.list_local_sessions(&wd).await;
        let backend_name = self.backend.name().to_string();
        let mut seen: std::collections::HashSet<String> = Default::default();
        let mut entries: Vec<ResumeEntry> = local
            .into_iter()
            .map(|l| {
                seen.insert(l.session_id.clone());
                ResumeEntry {
                    agent_kind: hist_kinds
                        .get(&l.session_id)
                        .cloned()
                        .flatten()
                        .unwrap_or_else(|| backend_name.clone()),
                    from_local: !hist_kinds.contains_key(&l.session_id),
                    session_id: l.session_id,
                    updated_at: l.updated_at,
                    first_prompt: l.first_prompt,
                    cwd: l.cwd,
                }
            })
            .collect();
        for r in history {
            if seen.insert(r.session_id.clone()) {
                entries.push(ResumeEntry {
                    session_id: r.session_id,
                    updated_at: r.updated_at,
                    agent_kind: r.agent_kind.unwrap_or_else(|| backend_name.clone()),
                    first_prompt: String::new(),
                    from_local: false,
                    cwd: None,
                });
            }
        }
        entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        entries.truncate(MAX);
        entries
    }

    /// 解析 conv 的工作目录：per-conv KV（`/cd` 设置）覆盖，否则回退 `default_workdir`。
    async fn resolve_workdir(&self, conv_id: &str) -> PathBuf {
        match self.store.get_config(&workdir_key(conv_id)).await {
            Ok(Some(p)) => PathBuf::from(p),
            _ => self.default_workdir.clone(),
        }
    }

    /// P5-5：中断/失败路径保住 backend 已学到（`SessionStarted`）的 session id。
    ///
    /// agent 可能在被 abort 前已建立新会话（如首轮任务跑了几分钟被 /stop 打断、
    /// 或正常完成但无最终文本被 backend 判 Err）——RunOutcome 拿不到，不落库则
    /// 下条消息静默开新会话，用户感知为「agent 失忆」。仅当学到的 id 非空且与本轮
    /// 传入的不同时写（相同 = 续接既有会话，映射未变）；失败仅 log 不影响回复。
    async fn persist_learned_session(
        &self,
        conv: &ConvId,
        existing: Option<&str>,
        learned: &Option<String>,
    ) {
        let Some(sid) = learned.as_deref().filter(|s| !s.is_empty()) else {
            return;
        };
        if Some(sid) == existing {
            return;
        }
        let now = now_secs();
        let active_name = self
            .store
            .get_config(&active_name_key(&conv.0))
            .await
            .unwrap_or(None)
            .filter(|s| !s.is_empty());
        let workdir = self
            .resolve_workdir(&conv.0)
            .await
            .to_string_lossy()
            .to_string();
        let row = SessionRow {
            conv_id: conv.0.clone(),
            session_id: sid.to_string(),
            agent_kind: self.backend.name().to_string(),
            workdir: workdir.clone(),
            name: active_name.clone(),
            created_at: now,
            updated_at: now,
        };
        if let Err(e) = self.store.upsert_session(&row).await {
            warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "中断路径 upsert_session 失败");
            return;
        }
        if let Some(name) = active_name {
            let nrow = NamedSessionRow {
                conv_id: conv.0.clone(),
                name,
                session_id: sid.to_string(),
                agent_kind: Some(self.backend.name().to_string()),
                workdir: Some(workdir),
                created_at: now,
                updated_at: now,
            };
            if let Err(e) = self.store.upsert_named_session(&nrow).await {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "中断路径 upsert_named_session 失败");
            }
        }
        info!(
            target: "imagent::core",
            conv_id = %conv.0,
            session_id = %sid,
            "中断/失败路径已持久化 backend 学到的 session id（下条消息续接）"
        );
    }

    /// 回传文本；发送失败仅 log（见 [`Self::reply_ok`]）。
    async fn reply(&self, conv: &ConvId, text: &str, hint: &ReplyHint) {
        let _ = self.reply_ok(conv, text, hint).await;
    }

    /// P6-3：回命令交互卡片；平台无卡片能力时默认实现已降级纯文本，卡片发送
    /// 失败（权限/网络）在此再兜一层纯文本——命令永远有回执。
    async fn reply_card(
        &self,
        conv: &ConvId,
        title: &str,
        body_md: &str,
        buttons: Vec<CardButton>,
        hint: &ReplyHint,
    ) {
        if let Err(e) = self
            .platform
            .send_command_card(conv, title, body_md, &buttons, hint)
            .await
        {
            warn!(
                target: "imagent::core",
                conv_id = %conv.0,
                error = %e,
                "命令卡片发送失败，降级纯文本"
            );
            let text = crate::platform::command_card_fallback_text(title, body_md, &buttons);
            self.reply(conv, &text, hint).await;
        }
    }

    /// 回传文本，返回是否成功送达。P5-第五批：流式前缀累积据此只记成功送达
    /// 的部分——失败段落留给最终全量兜底，而非两处皆失。session 过期升级为
    /// error（用户侧已收不到回复）。
    async fn reply_ok(&self, conv: &ConvId, text: &str, hint: &ReplyHint) -> bool {
        match self.platform.send_text(conv, text, hint).await {
            Ok(()) => {
                METRICS.messages_out.inc();
                true
            }
            Err(e) => {
                if is_session_expired_err(&e) {
                    tracing::error!(
                        target: "imagent::core",
                        conv_id = %conv.0,
                        error = %e,
                        "send_text session 过期（用户侧已收不到）"
                    );
                } else {
                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "send_text 失败");
                }
                false
            }
        }
    }
}
