//! 消息调度核心。
//!
//! `Dispatcher` 持有注入的 `Arc<dyn Platform>` / `Arc<dyn Backend>` / `Store` /
//! `Auth` / 配置，循环 `platform.recv()` 并对每条消息 `tokio::spawn` 处理。
//!
//! 两条硬约束在此体现：
//! 1. 非白名单 sender 丢弃；发现模式（白名单为空）回引导消息但不驱动 agent。
//! 2. backend 只用配置的 `allowed_tools`、workdir 用配置的 `default_workdir`。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::auth::Auth;
use crate::backend::Backend;
use crate::card_session::CardSession;
use crate::config::{CotDetail, PermissionMode};
use crate::error::Result;
use crate::metrics::METRICS;
use crate::permission::{parse_reply, PermissionReply, PermissionRouter};
use crate::platform::Platform;
use crate::types::{
    AgentChunk, CardTerminal, ConvId, InboundMessage, MediaRef, ReplyHint, SessionId,
};
use imagent_store::{NamedSessionRow, SessionRow, Store};
use parking_lot::RwLock;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

/// per-conv 排队消息上限：runner 在飞期间到达的消息暂存条数。超出回告警并丢弃，
/// 防刷屏把合并后的 prompt 撑爆。
const PENDING_QUEUE_CAP: usize = 100;

/// Dispatcher 时长类预算聚合（避免构造参数表随配置项继续膨胀）。
#[derive(Debug, Clone, Copy)]
pub struct TaskBudgets {
    /// 单次 agent 运行总超时（`agent_timeout_secs`）。
    pub agent_timeout: Duration,
    /// Ask 权限审批等待回复超时（`permission_ask_timeout_secs`，独立预算）。
    pub permission_ask_timeout: Duration,
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
/// 形如 `\n\n🔧 工具调用：Read({"path":"…}), Edit({"file":"…})`。
fn format_tool_summary(tool_calls: &[(String, String)], detail: CotDetail) -> String {
    let max = detail.max_tools();
    let shown: Vec<String> = tool_calls
        .iter()
        .take(max)
        .map(|(t, i)| format!("{t}({i})"))
        .collect();
    let mut s = format!("\n\n🔧 工具调用：{}", shown.join(", "));
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
    resume_cache: Mutex<HashMap<String, Vec<ResumeEntry>>>,
    /// 管理员 sender（可 /allow）；空 = 所有白名单用户可（向后兼容，P2-D）。
    admin_senders: Arc<RwLock<Vec<String>>>,
    /// 优雅退出信号（P1-5）：收到 SIGINT/SIGTERM 后 notify，run() 停止收新消息并 drain。
    shutdown: Arc<tokio::sync::Notify>,
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
        Self {
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
            shutdown_grace: budgets.shutdown_grace,
            agent_idle_timeout: Arc::new(RwLock::new(budgets.agent_idle_timeout)),
            batch_window: Arc::new(RwLock::new(budgets.batch_window)),
            cot_detail: Arc::new(RwLock::new(cot_detail)),
            started_at: Instant::now(),
            running: Mutex::new(HashMap::new()),
            queues: Mutex::new(HashMap::new()),
            resume_cache: Mutex::new(HashMap::new()),
            admin_senders: Arc::new(RwLock::new(admin_senders)),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            tasks: Mutex::new(tokio::task::JoinSet::new()),
        }
    }

    /// 调用者是否为管理员（可 /allow）。admin_senders 空 = 向后兼容（所有白名单
    /// 用户可）；非空则严格检查（P2-D）。
    fn is_admin(&self, sender: &str) -> bool {
        let admins = self.admin_senders.read();
        let trimmed = sender.trim();
        admins.is_empty() || admins.iter().any(|a| a.trim() == trimmed)
    }

    /// P5-1（安全）：审批回复的发送者须过白名单（sender OR 会话白名单——与
    /// handle() 的鉴权门完全一致）。审批路由发生在 handle() **之前**，天然绕过
    /// 其鉴权；不加此门，群聊里非白名单成员发一条 "y" 即可批准 Bash 等高危工具，
    /// 发任意文本则被当 deny 吞掉。飞书审批按钮回调携带 operator open_id 作
    /// sender，同一门槛覆盖按钮路径。
    fn can_route_permission_reply(&self, msg: &InboundMessage) -> bool {
        self.auth.is_allowed(&msg.sender) || self.auth.is_chat_allowed(&msg.conv_id.0)
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
        self.shutdown.notify_waiters();
    }

    /// 主循环。循环 `platform.recv()`，每条消息 `tokio::spawn` 处理（不阻塞 recv）。
    /// recv 返回 Err 时：session 过期 → 优雅停止（返回 Err 让 main 提示重新 login）；
    /// 其它错误 → 指数退避后继续重试（防 client 异常退出导致 dispatcher 忙循环刷屏；ilink 长轮询层另有退避），不 panic。
    pub async fn run(self: Arc<Self>) -> Result<()> {
        // Ask 模式：spawn unix socket accept task（MCP server 转发的权限请求经此进主进程）。
        #[cfg(unix)]
        if matches!(*self.permission_mode.read(), PermissionMode::Ask) {
            if let Some(sock) = crate::permission::default_sock_path() {
                self.spawn_socket_accept(sock.to_string_lossy().into_owned());
            } else {
                warn!(target: "imagent::core", "Ask 模式但无法定位 socket 路径，权限请求将无法路由");
            }
        }
        #[cfg(not(unix))]
        if matches!(*self.permission_mode.read(), PermissionMode::Ask) {
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
                _ = self.shutdown.notified() => {
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
                            let reply = parse_reply(text);
                            if self.router.route(&conv_id, reply).await {
                                continue;
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

    /// spawn socket accept task：每个连接独立 spawn，读权限请求 → send_text 询问
    /// 用户 → register 等 receiver → 写回复回 socket。
    #[cfg(unix)]
    fn spawn_socket_accept(self: &Arc<Self>, sock: String) {
        // 清理可能残留的旧 socket 文件。
        let _ = std::fs::remove_file(&sock);
        let listener = match std::os::unix::net::UnixListener::bind(&sock) {
            Ok(l) => l,
            Err(e) => {
                // P2-B：bind 失败用 error 级别——Ask 权限闭环将完全不可用（降级为
                // 无审批），是安全 posture 退化，需显著告警而非静默 warn。
                error!(
                    target: "imagent::core",
                    sock = %sock,
                    error = %e,
                    "bind permission socket 失败：Ask 权限闭环不可用（降级为无审批，安全 posture 退化）"
                );
                return;
            }
        };
        // 转为非阻塞，包进 tokio。
        listener.set_nonblocking(true).ok();
        let listener = match tokio::net::UnixListener::from_std(listener) {
            Ok(l) => l,
            Err(e) => {
                warn!(target: "imagent::core", error = %e, "from_std permission socket failed");
                return;
            }
        };
        // chmod 0600：只允许 owner（本进程同 uid）连接。父目录 ~/.imagent 应为 0700（由 store 保证）。
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)) {
            warn!(
                target: "imagent::core",
                sock = %sock,
                error = %e,
                "chmod permission socket 0600 失败，Ask 权限闭环鉴权减弱"
            );
        }
        // P5-9b：握手 token——同 uid 进程裸 connect 即可伪造 conv_id 推送审批请求
        //（P2-7 残余）。token 随机生成并写 <sock_dir>/permission.token（0600），MCP
        // 子进程（claude 经 --mcp-config spawn）读取后在连接首行回传，不符即丢弃。
        // 说明：同 uid 进程仍能从文件/env/cmdline 拿到 token，属提高伪造门槛而非
        // 绝对防护（绝对防护需继承 fd 或抽象命名空间 socket，另行迭代）。
        let token = format!("imagent-perm:{:032x}", rand::random::<u128>());
        let token_path = std::path::Path::new(&sock)
            .parent()
            .map(|d| d.join("permission.token"))
            .unwrap_or_else(|| std::path::PathBuf::from("permission.token"));
        if let Err(e) = std::fs::write(&token_path, &token) {
            error!(
                target: "imagent::core",
                error = %e,
                ?token_path,
                "写 permission.token 失败：所有权限请求将因握手失败被拒（fail-closed）"
            );
        } else if let Err(e) =
            std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600))
        {
            warn!(target: "imagent::core", error = %e, "chmod permission.token 0600 失败");
        }
        // R-2：accept task 监听 shutdown（SIGTERM 时停止 accept，原实现永驻）；
        // 每个连接的 handle_permission_socket 纳入 self.tasks，drain 时一并等待。
        let this = self.clone();
        let expected_token = token;
        tokio::spawn(async move {
            // 鉴权基准：只接受与本进程同 uid 的连接（MCP 子进程由本进程 spawn，必然同 uid）。
            // P2-7/P5-9b 威胁模型：peer_uid 防「跨 uid 伪造」；握手 token 把「同 uid
            // 裸 connect 伪造 conv_id」的门槛从零提高到需读到 token（见上方注释）。
            let expected_uid = current_uid();
            loop {
                tokio::select! {
                    _ = this.shutdown.notified() => {
                        info!(target: "imagent::core", "permission socket accept task 收到 shutdown，停止");
                        break;
                    }
                    res = listener.accept() => match res {
                        Ok((stream, _)) => {
                            match peer_uid(&stream) {
                                Some(uid) if uid == expected_uid => {
                                    let platform = this.platform.clone();
                                    let router = this.router.clone();
                                    let permission_ask_timeout = this.permission_ask_timeout;
                                    let expected_token = expected_token.clone();
                                    this.tasks.lock().await.spawn(async move {
                                        Self::handle_permission_socket(
                                            stream,
                                            platform,
                                            router,
                                            permission_ask_timeout,
                                            expected_token,
                                        )
                                        .await;
                                    });
                                }
                                Some(uid) => {
                                    warn!(
                                        target: "imagent::core",
                                        peer_uid = uid,
                                        expected_uid = expected_uid,
                                        "拒绝非本进程 uid 的权限 socket 连接（疑似伪造）"
                                    );
                                }
                                None => {
                                    warn!(
                                        target: "imagent::core",
                                        "无法获取权限 socket 对端 uid（平台不支持 peer cred），拒绝连接"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            warn!(target: "imagent::core", error = %e, "permission socket accept 失败");
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        }
                    }
                }
            }
        });
    }

    /// 读一行权限 socket 报文（15s 超时 + 64KiB 上限）。None = EOF/超时/超长
    ///（后两者记日志）。
    #[cfg(unix)]
    async fn read_socket_line(
        reader: &mut tokio::io::BufReader<&mut tokio::net::UnixStream>,
    ) -> Option<String> {
        match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            Self::read_line_capped(reader, 64 * 1024),
        )
        .await
        {
            Ok(Ok(line)) => line,
            Ok(Err(e)) => {
                warn!(target: "imagent::core", error = %e, "permission socket 读行失败/超长");
                None
            }
            Err(_) => {
                warn!(target: "imagent::core", "permission socket 读行超时（15s）");
                None
            }
        }
    }

    /// 读一行（到 `\n`），上限 `max_bytes` 字节，超限返 Err（P1-9：防同 uid 进程
    /// 发巨大行 OOM）。返回 None 表示对端 EOF（未发数据即关）。
    #[cfg(unix)]
    async fn read_line_capped<R: tokio::io::AsyncBufRead + Unpin>(
        reader: &mut R,
        max_bytes: usize,
    ) -> std::io::Result<Option<String>> {
        use tokio::io::AsyncBufReadExt;
        let mut buf: Vec<u8> = Vec::with_capacity(512);
        loop {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return if buf.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
                };
            }
            if let Some(nl) = available.iter().position(|&b| b == b'\n') {
                buf.extend_from_slice(&available[..=nl]);
                reader.consume(nl + 1);
                return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
            }
            buf.extend_from_slice(available);
            let n = available.len();
            reader.consume(n);
            if buf.len() > max_bytes {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("permission request line exceeds {max_bytes} bytes"),
                ));
            }
        }
    }

    /// 写一行 JSON 回复到 socket，带写超时（P1-9：防对端不读导致 write_all 长时阻塞）。
    /// best-effort：超时/出错仅返回，连接由调用方 drop。
    #[cfg(unix)]
    async fn write_permission_reply(stream: &mut tokio::net::UnixStream, reply: PermissionReply) {
        use tokio::io::AsyncWriteExt;
        let resp = serde_json::json!({
            "allow": reply.allow,
            "message": reply.message,
        });
        let mut out = resp.to_string();
        out.push('\n');
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let _ = stream.write_all(out.as_bytes()).await;
            let _ = stream.flush().await;
        })
        .await;
    }

    /// 处理单个 socket 连接：读请求行 → send_text 询问 → 等回复 → 写回复。
    ///
    /// - **P1-3**：send_text 失败时回写 deny 并 return（不挂 pending——否则用户看不到
    ///   询问，agent 会卡满 agent_timeout，期间该 conv 消息全被当回复吞）。
    /// - **P1-8**：超时/router-drop 时 `router.cancel` 清理 pending map 残留。
    /// - **P1-9**：读行加上限（64KiB）+ 读超时（15s）+ 写超时（10s），防 OOM / 挂死。
    #[cfg(unix)]
    async fn handle_permission_socket(
        mut stream: tokio::net::UnixStream,
        platform: Arc<dyn Platform>,
        router: Arc<PermissionRouter>,
        permission_ask_timeout: std::time::Duration,
        expected_token: String,
    ) {
        // P5-9b：读两行——首行握手 token、次行 JSON 请求。必须共用一个 BufReader：
        // 分开建会把第二行的数据吞进被丢弃的缓冲区。reader 在块内 drop 以释放
        // stream 借用（后续写回需 &mut stream）。
        let req_line = {
            use tokio::io::BufReader;
            let mut reader = BufReader::new(&mut stream);
            let token_line = Self::read_socket_line(&mut reader).await;
            let Some(token_line) = token_line else {
                return; // EOF，对端未发即关
            };
            if token_line.trim() != expected_token {
                warn!(
                    target: "imagent::core",
                    "权限 socket 握手 token 不符，丢弃连接（疑似同 uid 伪造）"
                );
                return;
            }
            Self::read_socket_line(&mut reader).await
        };
        let Some(line) = req_line else {
            return; // token 对了但没发请求（EOF/超时/超长已记日志）
        };
        let req: serde_json::Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(e) => {
                warn!(target: "imagent::core", raw = %line, error = %e, "permission socket 非 JSON");
                return;
            }
        };
        let conv_id = req
            .get("conv_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let tool_name = req
            .get("tool_name")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let input_str = req.get("input").map(|v| v.to_string()).unwrap_or_default();
        if conv_id.is_empty() {
            return;
        }
        let conv = ConvId(conv_id.clone());
        // P4-4：询问用户——平台支持交互卡片时发「按钮卡片」（send_permission_ask
        // 覆写），否则默认纯文本。按钮点击由平台侧转成 text="y"/"n" 的入站消息，
        // 复用 recv 循环的审批回复路由，core 不感知按钮。
        let input_summary = truncate_str(&input_str, 80);
        // P1-3：发送失败 → 回写 deny 并 return，不挂 pending。
        if let Err(e) = platform
            .send_permission_ask(&conv, &tool_name, &input_summary, &ReplyHint::None)
            .await
        {
            warn!(target: "imagent::core", conv_id = %conv_id, error = %e, "send permission ask 失败，回 deny 不挂 pending");
            Self::write_permission_reply(
                &mut stream,
                PermissionReply {
                    allow: false,
                    message: Some("send_text failed: IM 不可达".into()),
                },
            )
            .await;
            return;
        }
        // 注册 pending，等回复（recv 循环 route 到这里）。
        let rx = router.register(&conv_id).await;
        // P1-G/S-3：权限回复等待独立预算 permission_ask_timeout（默认 300s，不挤占
        // agent_timeout 的执行预算）。agent 死或用户长时间不回复时，超时回 deny 并 drop
        // receiver，避免 pending 永驻把后续消息误当回复吞。
        // P1-8：超时/router-drop 分支显式 cancel，移除 pending map 残留。
        let reply: PermissionReply = match tokio::time::timeout(permission_ask_timeout, rx).await {
            Ok(Ok(r)) => {
                METRICS
                    .permission_decisions
                    .with_label_values(&[if r.allow { "allow" } else { "deny" }])
                    .inc();
                r
            }
            Ok(Err(_)) => {
                router.cancel(&conv_id).await;
                METRICS
                    .permission_decisions
                    .with_label_values(&["dropped"])
                    .inc();
                PermissionReply {
                    allow: false,
                    message: Some("permission router dropped".into()),
                }
            }
            Err(_elapsed) => {
                router.cancel(&conv_id).await;
                METRICS
                    .permission_decisions
                    .with_label_values(&["timeout"])
                    .inc();
                PermissionReply {
                    allow: false,
                    message: Some(format!(
                        "permission ask timed out after {permission_ask_timeout:?}"
                    )),
                }
            }
        };
        // 写回 socket（一行 JSON）。
        Self::write_permission_reply(&mut stream, reply).await;
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

    /// 处理单条消息。内部任何错误都 log 并吞掉，不影响主循环。
    async fn handle(&self, msg: InboundMessage) {
        let conv = msg.conv_id.clone();
        let sender = msg.sender.clone();
        let hint = msg.reply_hint.clone();

        // best-effort 指标：入站消息计数（失败只 warn 不阻断）。
        METRICS.messages_in.inc();
        // 1. 发现态：两个白名单（sender / chat）都为空。不自动授权（安全），对 sender
        //    回引导消息，告知其 sender id 与 conv id，不驱动 agent。
        if self.auth.is_discovery() {
            info!(
                target: "imagent::discovery",
                conv_id = %conv.0,
                sender = %sender.0,
                text = ?msg.text,
                "discovery 模式：记录 sender，回引导"
            );
            let guide = format!(
                "发现模式：当前白名单为空。你的 sender id 是 `{}`，会话 id 是 `{}`。\n\
                 请管理员在本地运行 `imagent allow {}` 授权用户、或 `imagent allow-chat {}` \
                 授权整个会话（群）后重启 imagent；也可由已授权用户在 IM 内发 /allow / /chat allow。",
                sender.0, conv.0, sender.0, conv.0
            );
            self.reply(&conv, &guide, &hint).await;
            return;
        }

        // 2. 白名单（P4-5）：sender 放行 OR 会话（群）放行，二者其一即过。
        //    群维度授权后无需逐个 allow 成员；命令层的授权操作仍受 admin 门槛。
        if !self.auth.is_allowed(&sender) && !self.auth.is_chat_allowed(&conv.0) {
            warn!(
                target: "imagent::core",
                conv_id = %conv.0,
                sender = %sender.0,
                "非白名单 sender 且会话未授权，丢弃"
            );
            return;
        }

        // 3. 斜杠命令（鉴权通过后、调 backend 前）。
        //    命令名小写比较；参数保留原样。到这里的 sender 必然已过白名单，
        //    故 /allow 的「调用者鉴权」天然由白名单保证，无需额外校验。
        if let Some(text) = msg.text.as_ref() {
            let trimmed = text.trim();
            if trimmed.starts_with('/') {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                let cmd = parts[0].to_ascii_lowercase();
                match cmd.as_str() {
                    "/new" => {
                        // P1-F：取 conv 串行锁，与在飞 agent task 串行（避免并发改 session 损坏状态）。
                        let _conv_lock = self.acquire_conv_lock(&conv.0).await;
                        let _conv_guard = _conv_lock.lock().await;
                        // 删除该 conv 的 session 行（下次新建），失败仅 log。
                        if let Err(e) = self.store.delete_session(&conv.0).await {
                            warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "delete_session 失败");
                        }
                        // 清当前活动命名 → 回到默认未命名 session。
                        if let Err(e) = self.store.delete_config(&active_name_key(&conv.0)).await {
                            warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "delete_config(active_name) 失败");
                        }
                        self.reply(
                            &conv,
                            "已重置会话，下一条消息将开启新会话（默认未命名）。",
                            &hint,
                        )
                        .await;
                        return;
                    }
                    "/allow" => {
                        let target = parts.get(1).map(|s| s.trim()).unwrap_or("");
                        if target.is_empty() {
                            self.reply(&conv, "用法: /allow <sender_id>", &hint).await;
                        } else {
                            let actor = sender.0.as_str();
                            // P2-D：仅管理员可授权新用户（admin_senders 非空时严格；
                            // 空则向后兼容所有白名单用户可）。
                            if !self.is_admin(actor) {
                                self.reply(&conv, "仅管理员（admin_senders）可授权新用户。", &hint)
                                    .await;
                                return;
                            }
                            let added = self.auth.allow(target);
                            // P2-E：持久化失败不能谎报「已授权」（内存已加但重启后丢失）。
                            let persist_failed = self
                                .store
                                .add_allowed_sender(target, Some(actor), Some("im"))
                                .await
                                .is_err();
                            if persist_failed {
                                warn!(target: "imagent::core", "add_allowed_sender 持久化失败（内存已授权，重启丢失）");
                            }
                            if let Err(e) = self
                                .store
                                .append_audit(
                                    "allow",
                                    Some(actor),
                                    Some(target),
                                    Some(if added { "added" } else { "already-present" }),
                                )
                                .await
                            {
                                warn!(target: "imagent::core", error = %e, "append_audit 失败");
                            }
                            let text_out = if persist_failed {
                                format!(
                                    "⚠️ `{target}` 已在内存授权，但持久化失败（重启后将丢失），请重试或本地 `imagent allow` 处理。"
                                )
                            } else if added {
                                format!("已授权 `{target}`。")
                            } else {
                                format!("`{target}` 已在白名单。")
                            };
                            self.reply(&conv, &text_out, &hint).await;
                        }
                        return;
                    }
                    "/disallow" => {
                        // P5-3（安全）：撤销白名单成员影响全局授权——此前无 admin 门槛，
                        // 任何过门用户（含群内陌生成员）可把管理员本人踢出白名单（DoS）。
                        // 与 /allow 的门槛对称。
                        if !self.is_admin(&sender.0) {
                            self.reply(&conv, "仅管理员（admin_senders）可撤销授权。", &hint)
                                .await;
                            return;
                        }
                        let target = parts.get(1).map(|s| s.trim()).unwrap_or("");
                        if target.is_empty() {
                            self.reply(&conv, "用法: /disallow <sender_id>", &hint)
                                .await;
                        } else if target == sender.0.as_str() {
                            // 防自锁：不允许撤销自己。
                            self.reply(
                                &conv,
                                "不允许撤销自己（防止锁死）。如需操作请在本地 CLI 处理。",
                                &hint,
                            )
                            .await;
                        } else {
                            let existed = self.auth.revoke(target);
                            if let Err(e) = self.store.remove_allowed_sender(target).await {
                                warn!(target: "imagent::core", error = %e, "remove_allowed_sender 失败");
                            }
                            if let Err(e) = self
                                .store
                                .append_audit(
                                    "disallow",
                                    Some(&sender.0),
                                    Some(target),
                                    Some(if existed { "removed" } else { "absent" }),
                                )
                                .await
                            {
                                warn!(target: "imagent::core", error = %e, "append_audit 失败");
                            }
                            self.reply(
                                &conv,
                                &format!(
                                    "已移除 `{target}`（{}）",
                                    if existed { "成功" } else { "原本不在" }
                                ),
                                &hint,
                            )
                            .await;
                        }
                        return;
                    }
                    "/list" => {
                        let snap = self.auth.snapshot();
                        let chats = self.auth.snapshot_chats();
                        let mut out = if snap.is_empty() {
                            "用户白名单为空。".to_string()
                        } else {
                            format!("用户白名单（{}）：{}", snap.len(), snap.join(", "))
                        };
                        // P4-5：会话（群）白名单一并列出。
                        if chats.is_empty() {
                            out.push_str("\n会话白名单为空。");
                        } else {
                            out.push_str(&format!(
                                "\n会话白名单（{}）：{}",
                                chats.len(),
                                chats.join(", ")
                            ));
                        }
                        self.reply(&conv, &out, &hint).await;
                        return;
                    }
                    "/whoami" => {
                        self.reply(
                            &conv,
                            &format!("你的 sender id：`{}`\n当前会话 id：`{}`", sender.0, conv.0),
                            &hint,
                        )
                        .await;
                        return;
                    }
                    "/chat" => {
                        // P4-5：会话（群）白名单管理。与 /allow 同构：管理员门槛、
                        // 内存 + store 双写、审计。`allow`/`deny` 缺省作用于当前会话。
                        let sub = parts.get(1).map(|s| s.trim()).unwrap_or("");
                        let actor = sender.0.as_str();
                        match sub.to_ascii_lowercase().as_str() {
                            "allow" | "deny" => {
                                if !self.is_admin(actor) {
                                    self.reply(
                                        &conv,
                                        "仅管理员（admin_senders）可管理会话白名单。",
                                        &hint,
                                    )
                                    .await;
                                    return;
                                }
                                let target = parts
                                    .get(2)
                                    .map(|s| s.trim())
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or(&conv.0)
                                    .to_string();
                                let (applied, persist_failed) = if sub == "allow" {
                                    let added = self.auth.allow_chat(&target);
                                    let failed = self
                                        .store
                                        .add_allowed_chat(&target, Some(actor), Some("im"))
                                        .await
                                        .is_err();
                                    (added, failed)
                                } else {
                                    let removed = self.auth.revoke_chat(&target);
                                    let failed =
                                        self.store.remove_allowed_chat(&target).await.is_err();
                                    (removed, failed)
                                };
                                if persist_failed {
                                    warn!(target: "imagent::core", "会话白名单持久化失败（内存已改，重启丢失）");
                                }
                                let _ = self
                                    .store
                                    .append_audit(
                                        if sub == "allow" {
                                            "chat_allow"
                                        } else {
                                            "chat_deny"
                                        },
                                        Some(actor),
                                        Some(&target),
                                        Some(if applied { "applied" } else { "no-change" }),
                                    )
                                    .await;
                                let verb = if sub == "allow" { "授权" } else { "移除" };
                                let persist_note = if persist_failed {
                                    "（⚠️ 持久化失败，重启后失效）"
                                } else {
                                    ""
                                };
                                self.reply(
                                    &conv,
                                    &format!("✅ 已{verb}会话 {target}{persist_note}"),
                                    &hint,
                                )
                                .await;
                            }
                            _ => {
                                let chats = self.auth.snapshot_chats();
                                let list = if chats.is_empty() {
                                    "（空）".to_string()
                                } else {
                                    chats.join("\n- ")
                                };
                                self.reply(
                                    &conv,
                                    &format!(
                                        "用法：/chat allow [conv_id] 授权当前/指定会话\n/chat deny [conv_id] 移除\n/chat list 列出（如下）\n当前会话 id：`{}`\n- {list}",
                                        conv.0
                                    ),
                                    &hint,
                                )
                                .await;
                            }
                        }
                        return;
                    }
                    "/config" => {
                        // P4-6：查看 / 热改运行参数。改全局行为，管理员门槛。
                        let key = parts.get(1).map(|s| s.trim()).unwrap_or("");
                        let value = parts.get(2).map(|s| s.trim()).unwrap_or("");
                        if key.is_empty() {
                            // 先拷出共享句柄的值再跨 await（parking_lot guard 非 Send）。
                            let idle_secs = self.agent_idle_timeout.read().as_secs();
                            let window_ms = self.batch_window.read().as_millis();
                            let cot = self.cot_detail.read().as_str();
                            let perm = self.permission_mode.read().as_str();
                            let text = format!(
                                "当前配置：\n- cot_detail = {cot}（off|brief|detailed）\n- batch_window_ms = {window_ms}\n- agent_idle_timeout_secs = {idle_secs}（0=关）\n- agent_timeout_secs = {}（重启生效）\n- permission_mode = {perm}\n用法：/config <key> <value>（管理员）",
                                self.agent_timeout.as_secs(),
                            );
                            self.reply(&conv, &text, &hint).await;
                            return;
                        }
                        if !self.is_admin(&sender.0) {
                            self.reply(&conv, "仅管理员（admin_senders）可修改配置。", &hint)
                                .await;
                            return;
                        }
                        let result = match key {
                            "cot_detail" => match CotDetail::from_str_lossy(value) {
                                Some(d) => {
                                    *self.cot_detail.write() = d;
                                    format!("✅ cot_detail = {}", d.as_str())
                                }
                                None => "用法：/config cot_detail <off|brief|detailed>".into(),
                            },
                            "batch_window_ms" => match value.parse::<u64>() {
                                Ok(ms) => {
                                    *self.batch_window.write() = Duration::from_millis(ms);
                                    format!("✅ batch_window_ms = {ms}")
                                }
                                Err(_) => "用法：/config batch_window_ms <毫秒数，0=关闭>".into(),
                            },
                            "agent_idle_timeout_secs" => match value.parse::<u64>() {
                                Ok(s) => {
                                    *self.agent_idle_timeout.write() = Duration::from_secs(s);
                                    format!("✅ agent_idle_timeout_secs = {s}")
                                }
                                Err(_) => {
                                    "用法：/config agent_idle_timeout_secs <秒数，0=关闭>".into()
                                }
                            },
                            _ => "未知配置项（支持：cot_detail / batch_window_ms / agent_idle_timeout_secs）"
                                .into(),
                        };
                        self.reply(&conv, &result, &hint).await;
                        return;
                    }
                    "/status" => {
                        // P4-7：本会话 + 全局运行状态。
                        let running_here = self.running.lock().await.contains_key(&conv.0);
                        let queued_here = self
                            .queues
                            .lock()
                            .await
                            .get(&conv.0)
                            .map(|q| q.len())
                            .unwrap_or(0);
                        let in_flight = self.running.lock().await.len();
                        let wd = self.resolve_workdir(&conv.0).await;
                        let name_key = active_name_key(&conv.0);
                        let (sess, active) = tokio::join!(
                            self.store.get_session(&conv.0),
                            self.store.get_config(&name_key)
                        );
                        let sess_desc = match sess {
                            Ok(Some(row)) => {
                                let name = active.ok().flatten().unwrap_or_default();
                                let label = if name.is_empty() {
                                    "未命名".to_string()
                                } else {
                                    name
                                };
                                format!(
                                    "{label}（{}…，{}）",
                                    &row.session_id[..row.session_id.len().min(12)],
                                    row.agent_kind
                                )
                            }
                            _ => "无（下条消息新建）".to_string(),
                        };
                        let text = format!(
                            "📊 状态\n- 平台/后端：{} / {}\n- 本会话：{}，排队 {} 条\n- 会话：{sess_desc}\n- 工作目录：{}\n- 全局在飞任务：{in_flight}\n- 运行时长：{}",
                            self.platform.name(),
                            self.backend.name(),
                            if running_here { "任务在跑" } else { "无任务" },
                            queued_here,
                            wd.display(),
                            format_uptime(self.started_at.elapsed()),
                        );
                        self.reply(&conv, &text, &hint).await;
                        return;
                    }
                    "/doctor" => {
                        // P4-7：自检——workdir / store / 后端 / 在飞任务。
                        let mut lines = Vec::new();
                        let wd = self.resolve_workdir(&conv.0).await;
                        match std::fs::metadata(&wd) {
                            Ok(m) if m.is_dir() => {
                                lines.push(format!("✅ 工作目录可用：{}", wd.display()))
                            }
                            Ok(_) => lines.push(format!("⚠️ 工作目录不是目录：{}", wd.display())),
                            Err(e) => {
                                lines.push(format!("⚠️ 工作目录不可访问：{}（{e}）", wd.display()))
                            }
                        }
                        // store 写读回环（config KV）。
                        let probe_key = format!("doctor_probe:{}", now_secs());
                        match self.store.set_config(&probe_key, "1").await {
                            Ok(()) => match self.store.get_config(&probe_key).await {
                                Ok(Some(v)) if v == "1" => {
                                    lines.push("✅ 存储读写正常（SQLite）".into())
                                }
                                _ => lines.push("⚠️ 存储读回异常".into()),
                            },
                            Err(e) => lines.push(format!("⚠️ 存储写入失败：{e}")),
                        }
                        let _ = self.store.delete_config(&probe_key).await;
                        let n_sess = self.store.count_sessions().await.unwrap_or(-1);
                        if n_sess >= 0 {
                            lines.push(format!("✅ 会话映射：{n_sess} 条"));
                        } else {
                            lines.push("⚠️ 会话映射计数失败".into());
                        }
                        let in_flight = self.running.lock().await.len();
                        lines.push(if in_flight == 0 {
                            "✅ 无在飞任务".to_string()
                        } else {
                            format!("ℹ️ 在飞任务 {in_flight} 个（/stop 可中断）")
                        });
                        lines.push(format!(
                            "ℹ️ 平台 {} / 后端 {}（{}）",
                            self.platform.name(),
                            self.backend.name(),
                            if self.platform.supports_streaming_card(&conv) {
                                "支持流式卡片"
                            } else {
                                "纯文本"
                            }
                        ));
                        let text = format!("🩺 自检结果：\n{}", lines.join("\n"));
                        self.reply(&conv, &text, &hint).await;
                        return;
                    }
                    "/reconnect" => {
                        // P4-7：强制平台重连（排查长连接僵死）。
                        match self.platform.reconnect().await {
                            Ok(()) => {
                                self.reply(
                                    &conv,
                                    "🔌 已触发平台重连（后台进行中，稍候生效）。",
                                    &hint,
                                )
                                .await
                            }
                            Err(e) => {
                                self.reply(
                                    &conv,
                                    &format!(
                                        "⚠️ 重连指令失败：{e}（平台可能不支持，可重启 imagent）"
                                    ),
                                    &hint,
                                )
                                .await
                            }
                        }
                        return;
                    }
                    "/resume" => {
                        // P4-8/P4-11：统一恢复列表 = IM 历史（📱）∪ 本机同项目会话
                        // （💻，仅当前 backend 支持时合并）。用户按序号选择，无需知道
                        // session id；选中 💻 即自动接管（写 sessions 表绑定）。
                        // P1-F：取 conv 串行锁，与在飞 agent task 串行。
                        let _conv_lock = self.acquire_conv_lock(&conv.0).await;
                        let _conv_guard = _conv_lock.lock().await;
                        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");

                        if arg.is_empty() {
                            let list = self.merged_resume_list(&conv.0).await;
                            if list.is_empty() {
                                self.reply(&conv, "暂无可恢复的会话。", &hint).await;
                                return;
                            }
                            let current = self.store.get_session(&conv.0).await.ok().flatten();
                            let wd = self.resolve_workdir(&conv.0).await;
                            let lines: Vec<String> = list
                                .iter()
                                .enumerate()
                                .map(|(i, e)| {
                                    let mark = if current
                                        .as_ref()
                                        .is_some_and(|c| c.session_id == e.session_id)
                                    {
                                        " *（当前）"
                                    } else {
                                        ""
                                    };
                                    // 摘要缺省回退 id 前缀（历史行无首条消息）。
                                    let desc = if e.first_prompt.is_empty() {
                                        format!("{}…", &e.session_id[..e.session_id.len().min(16)])
                                    } else {
                                        e.first_prompt.clone()
                                    };
                                    let src = if e.from_local { "💻" } else { "📱" };
                                    format!(
                                        "{}. {src} {} {desc}{mark}",
                                        i + 1,
                                        format_rel_ts(e.updated_at)
                                    )
                                })
                                .collect();
                            // 缓存本列表：序号选择取缓存（防两次调用间本机会话
                            // mtime 变化导致序号错位）。
                            self.resume_cache.lock().await.insert(conv.0.clone(), list);
                            self.reply(
                                &conv,
                                &format!(
                                    "可恢复会话（当前目录 {}；💻=本机 📱=IM）：\n{}\n用法：/resume <序号|session_id>",
                                    wd.display(),
                                    lines.join("\n")
                                ),
                                &hint,
                            )
                            .await;
                            return;
                        }

                        // 选择目标：序号 → 取缓存列表（选中即消费，防陈旧序号）；
                        // 非 数字 → 按 session_id 在新鲜合并列表里找。
                        let target: Option<ResumeEntry> = if let Ok(n) = arg.parse::<usize>() {
                            let mut cache = self.resume_cache.lock().await;
                            cache.get_mut(&conv.0).and_then(|l| {
                                if n >= 1 && n <= l.len() {
                                    Some(l.remove(n - 1))
                                } else {
                                    None
                                }
                            })
                        } else {
                            self.merged_resume_list(&conv.0)
                                .await
                                .into_iter()
                                .find(|e| e.session_id == arg)
                        };
                        let Some(target) = target else {
                            let msg = if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_digit())
                            {
                                "序号无效或列表已变化，请先发 /resume 查看最新列表再选。"
                            } else {
                                "未找到该会话（/resume 查看列表）。"
                            };
                            self.reply(&conv, msg, &hint).await;
                            return;
                        };
                        // 跨后端校验（同 /switch P2-A）。
                        let current_kind = self.backend.name();
                        if target.agent_kind != current_kind {
                            self.reply(
                                &conv,
                                &format!(
                                    "该会话是 {} 会话，当前后端为 {current_kind}（不互通，无法恢复）",
                                    target.agent_kind
                                ),
                                &hint,
                            )
                            .await;
                            return;
                        }
                        // P5-15：本机会话接管前校验 cwd——目录编码冲突（如
                        // `/a/b-c` 与 `/a/b/c` 同码）或候选误扫时，防止把别的
                        // 项目的会话接到当前 workdir。cwd 缺失（旧数据/解析不到）
                        // 不阻塞，仅记录。
                        if target.from_local {
                            if let Some(cwd) = target.cwd.as_deref().filter(|s| !s.is_empty()) {
                                let wd_now = self.resolve_workdir(&conv.0).await;
                                if std::path::Path::new(cwd) != wd_now {
                                    warn!(
                                        target: "imagent::core",
                                        conv_id = %conv.0,
                                        session_cwd = %cwd,
                                        current_workdir = %wd_now.display(),
                                        "本机会话 cwd 与当前 workdir 不符，拒绝接管"
                                    );
                                    self.reply(
                                        &conv,
                                        &format!(
                                            "该会话属于其它目录（{cwd}），当前工作目录是 {}；如确要接管请先 /cd {cwd}",
                                            wd_now.display()
                                        ),
                                        &hint,
                                    )
                                    .await;
                                    return;
                                }
                            }
                        }
                        let now = now_secs();
                        let row = SessionRow {
                            conv_id: conv.0.clone(),
                            session_id: target.session_id.clone(),
                            agent_kind: current_kind.to_string(),
                            workdir: self
                                .resolve_workdir(&conv.0)
                                .await
                                .to_string_lossy()
                                .to_string(),
                            name: None,
                            created_at: now,
                            updated_at: now,
                        };
                        if let Err(e) = self.store.upsert_session(&row).await {
                            self.reply(&conv, &format!("恢复失败：{e}"), &hint).await;
                            return;
                        }
                        // 回到未命名（与命名 session 的绑定解耦，同 /switch 语义）。
                        let _ = self.store.delete_config(&active_name_key(&conv.0)).await;
                        let sid_short = &target.session_id[..target.session_id.len().min(16)];
                        let fork_note = if target.from_local {
                            "\n⚠️ 该会话来自电脑端：续接将从此处分叉（不是同步）；若终端仍开着请先退出。"
                        } else {
                            ""
                        };
                        self.reply(
                            &conv,
                            &format!("✅ 已接管会话 {sid_short}…（下条消息续接）{fork_note}"),
                            &hint,
                        )
                        .await;
                        return;
                    }
                    "/switch" => {
                        // P1-F：取 conv 串行锁（同 /new）。
                        let _conv_lock = self.acquire_conv_lock(&conv.0).await;
                        let _conv_guard = _conv_lock.lock().await;
                        let name = parts.get(1).map(|s| s.trim()).unwrap_or("");
                        if name.is_empty() {
                            self.reply(&conv, "用法: /switch <name>", &hint).await;
                            return;
                        }
                        let key = active_name_key(&conv.0);
                        match self.store.get_named_session(&conv.0, name).await {
                            Ok(Some(row)) => {
                                // P2-A：校验 agent_kind——不同 backend 的 session_id 不互通，
                                // 切到异类 backend 的历史 session 会续接失败。
                                let current_kind = self.backend.name();
                                if let Some(k) = row.agent_kind.as_deref() {
                                    if k != current_kind {
                                        self.reply(
                                            &conv,
                                            &format!(
                                                "「{name}」是 {k} 会话，当前后端为 {current_kind}（不互通，无法续接）"
                                            ),
                                            &hint,
                                        )
                                        .await;
                                        return;
                                    }
                                }
                                // 切回历史命名 session：把它写成活动 session（续接用）。
                                let now = now_secs();
                                let sr = SessionRow {
                                    conv_id: conv.0.clone(),
                                    session_id: row.session_id.clone(),
                                    agent_kind: row
                                        .agent_kind
                                        .unwrap_or_else(|| self.backend.name().to_string()),
                                    workdir: row.workdir.unwrap_or_else(|| {
                                        self.default_workdir.to_string_lossy().to_string()
                                    }),
                                    name: Some(name.into()),
                                    created_at: row.created_at,
                                    updated_at: now,
                                };
                                if let Err(e) = self.store.upsert_session(&sr).await {
                                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "upsert_session 失败");
                                }
                                if let Err(e) = self.store.set_config(&key, name).await {
                                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "set_config(active_name) 失败");
                                }
                                let sid_short: String = row.session_id.chars().take(8).collect();
                                self.reply(
                                    &conv,
                                    &format!("已切换到「{name}」（session {sid_short}…）"),
                                    &hint,
                                )
                                .await;
                            }
                            Ok(None) => {
                                // 新命名 session：清活动 session（下次新建）+ 设 active_name。
                                if let Err(e) = self.store.delete_session(&conv.0).await {
                                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "delete_session 失败");
                                }
                                if let Err(e) = self.store.set_config(&key, name).await {
                                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "set_config(active_name) 失败");
                                }
                                self.reply(
                                    &conv,
                                    &format!("已切到新会话「{name}」，下一条消息将开启。"),
                                    &hint,
                                )
                                .await;
                            }
                            Err(e) => {
                                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "get_named_session 失败");
                                self.reply(&conv, "查询失败，请重试。", &hint).await;
                            }
                        }
                        return;
                    }
                    "/sessions" => {
                        match self.store.list_named_sessions(&conv.0).await {
                            Ok(rows) if rows.is_empty() => {
                                self.reply(&conv, "无命名会话（用 /switch <name> 创建）。", &hint)
                                    .await;
                            }
                            Ok(rows) => {
                                let active = self
                                    .store
                                    .get_config(&active_name_key(&conv.0))
                                    .await
                                    .unwrap_or(None)
                                    .unwrap_or_default();
                                let mut lines = String::from("命名会话：");
                                for r in &rows {
                                    let mark = if r.name == active { " *" } else { "" };
                                    let sid: String = r.session_id.chars().take(8).collect();
                                    lines.push_str(&format!(
                                        "\n  {}{} (session {}…)",
                                        r.name, mark, sid
                                    ));
                                }
                                self.reply(&conv, &lines, &hint).await;
                            }
                            Err(e) => {
                                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "list_named_sessions 失败");
                            }
                        }
                        return;
                    }
                    "/compact" => {
                        // P1-F：取 conv 串行锁——/compact 内 resume 当前 session 生成摘要，
                        // 须与在飞 agent task 串行（否则并发 resume 同 session 损坏状态）。
                        let _conv_lock = self.acquire_conv_lock(&conv.0).await;
                        let _conv_guard = _conv_lock.lock().await;
                        let key = compact_summary_key(&conv.0);
                        let existing_sid: Option<SessionId> =
                            match self.store.get_session(&conv.0).await {
                                Ok(Some(row)) => Some(SessionId(row.session_id)),
                                Ok(None) => None,
                                Err(e) => {
                                    warn!(
                                        target: "imagent::core",
                                        conv_id = %conv.0,
                                        error = %e,
                                        "compact: get_session 失败"
                                    );
                                    None
                                }
                            };
                        match existing_sid {
                            None => {
                                self.reply(&conv, "当前无活动会话可压缩。", &hint).await;
                            }
                            Some(sid) => {
                                // 用 claude resume 当前 session 生成摘要；只取 Final/RunOutcome。
                                let (tx, mut rx) = mpsc::channel::<AgentChunk>(32);
                                let backend = self.backend.clone();
                                let workdir = self.resolve_workdir(&conv.0).await;
                                let tools = self.allowed_tools.read().clone();
                                let conv_id_compact = conv.0.clone();
                                let agent_timeout = self.agent_timeout;
                                let join = tokio::spawn(async move {
                                    let backend_name = backend.name();
                                    match tokio::time::timeout(
                                        agent_timeout,
                                        backend.run(
                                            &conv_id_compact,
                                            "请用中文简洁总结当前对话的要点、已做决定与未完成事项（不超过 400 字）。",
                                            Some(&sid),
                                            &workdir,
                                            &tools,
                                            tx,
                                        ),
                                    )
                                    .await
                                    {
                                        Ok(res) => res,
                                        Err(_elapsed) => {
                                            METRICS
                                                .agent_timeouts
                                                .with_label_values(&["total"])
                                                .inc();
                                            Err(crate::error::CoreError::Backend(
                                                backend_name,
                                                format!(
                                                    "agent run timed out after {agent_timeout:?}"
                                                ),
                                            ))
                                        }
                                    }
                                });
                                // P5-16：注册进 running——/stop 此前中断不了 /compact
                                //（长摘要生成只能干等 agent_timeout）。conv 锁由本命令
                                // 持有，注册/移除无 ABA（新轮次须先等锁）。
                                self.running
                                    .lock()
                                    .await
                                    .insert(conv.0.clone(), join.abort_handle());
                                let mut summary: Option<String> = None;
                                while let Some(chunk) = rx.recv().await {
                                    if let AgentChunk::Final(t) = chunk {
                                        summary = Some(t);
                                    }
                                }
                                let join_res = join.await;
                                // 无论成败，先摘除在飞注册（/stop 已抢先摘除时为 no-op）。
                                self.running.lock().await.remove(&conv.0);
                                let summary_text = match join_res {
                                    Ok(Ok(o)) => summary.unwrap_or(o.final_text),
                                    Ok(Err(e)) => {
                                        warn!(
                                            target: "imagent::core",
                                            conv_id = %conv.0,
                                            error = %e,
                                            "compact 摘要生成失败"
                                        );
                                        self.reply(&conv, &format!("生成摘要失败：{e}"), &hint)
                                            .await;
                                        return;
                                    }
                                    Err(e) => {
                                        warn!(
                                            target: "imagent::core",
                                            conv_id = %conv.0,
                                            error = %e,
                                            "compact 摘要任务 panic"
                                        );
                                        self.reply(&conv, &format!("摘要任务异常：{e}"), &hint)
                                            .await;
                                        return;
                                    }
                                };
                                // 存摘要 + 重置活动 session + 清 active_name（释放 context）。
                                if let Err(e) = self.store.set_config(&key, &summary_text).await {
                                    warn!(
                                        target: "imagent::core",
                                        conv_id = %conv.0,
                                        error = %e,
                                        "set_config(compact_summary) 失败"
                                    );
                                }
                                if let Err(e) = self.store.delete_session(&conv.0).await {
                                    warn!(
                                        target: "imagent::core",
                                        conv_id = %conv.0,
                                        error = %e,
                                        "compact: delete_session 失败"
                                    );
                                }
                                if let Err(e) =
                                    self.store.delete_config(&active_name_key(&conv.0)).await
                                {
                                    warn!(
                                        target: "imagent::core",
                                        conv_id = %conv.0,
                                        error = %e,
                                        "compact: delete_config(active_name) 失败"
                                    );
                                }
                                self.reply(
                                    &conv,
                                    &format!(
                                        "已压缩会话。摘要：\n\n{summary_text}\n\n（新会话将保留此摘要延续上下文）"
                                    ),
                                    &hint,
                                )
                                .await;
                            }
                        }
                        return;
                    }
                    "/cd" => {
                        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
                        if arg.is_empty() {
                            let wd = self.resolve_workdir(&conv.0).await;
                            self.reply(&conv, &format!("当前工作目录：{}", wd.display()), &hint)
                                .await;
                            return;
                        }
                        let p = std::path::Path::new(arg);
                        if !p.is_absolute() {
                            self.reply(&conv, "用法：/cd <绝对路径>（须绝对路径）", &hint)
                                .await;
                            return;
                        }
                        if !p.is_dir() {
                            self.reply(&conv, &format!("目录不存在：{arg}"), &hint)
                                .await;
                            return;
                        }
                        // 改 per-conv workdir：取 conv 锁串行，与在飞 agent task 隔离。
                        let _conv_lock = self.acquire_conv_lock(&conv.0).await;
                        let _conv_guard = _conv_lock.lock().await;
                        match self.store.set_config(&workdir_key(&conv.0), arg).await {
                            Ok(_) => {
                                // P5 快赢：/resume 列表缓存随 workdir 失效——列表按
                                // conv 当前目录扫描，切目录后旧序号指向的是旧目录的
                                // 会话（且接管前有 cwd 校验兜底）。
                                self.resume_cache.lock().await.remove(&conv.0);
                                self.reply(
                                    &conv,
                                    &format!("✅ 工作目录已切到 {arg}（下条消息生效）"),
                                    &hint,
                                )
                                .await
                            }
                            Err(e) => self.reply(&conv, &format!("保存失败：{e}"), &hint).await,
                        }
                        return;
                    }
                    "/ws" => {
                        let sub = parts.get(1).map(|s| s.trim()).unwrap_or("");
                        let arg = parts.get(2).map(|s| s.trim()).unwrap_or("");
                        match sub {
                            "" | "list" => {
                                match self.store.list_config("workspace:").await {
                                    Ok(rows) if rows.is_empty() => {
                                        self.reply(&conv, "（暂无命名工作空间）", &hint).await
                                    }
                                    Ok(rows) => {
                                        let body = rows
                                            .iter()
                                            .map(|(k, v)| {
                                                format!(
                                                    "- {}：{v}",
                                                    k.strip_prefix("workspace:").unwrap_or(k)
                                                )
                                            })
                                            .collect::<Vec<_>>()
                                            .join("\n");
                                        self.reply(&conv, &format!("命名工作空间：\n{body}"), &hint)
                                            .await
                                    }
                                    Err(e) => {
                                        self.reply(&conv, &format!("列出失败：{e}"), &hint).await
                                    }
                                }
                                return;
                            }
                            "save" => {
                                if arg.is_empty() {
                                    self.reply(&conv, "用法：/ws save <name>", &hint).await;
                                    return;
                                }
                                let wd = self.resolve_workdir(&conv.0).await;
                                match self
                                    .store
                                    .set_config(&workspace_key(arg), &wd.to_string_lossy())
                                    .await
                                {
                                    Ok(_) => {
                                        self.reply(
                                            &conv,
                                            &format!(
                                                "✅ 已保存工作空间「{arg}」= {}",
                                                wd.display()
                                            ),
                                            &hint,
                                        )
                                        .await
                                    }
                                    Err(e) => {
                                        self.reply(&conv, &format!("保存失败：{e}"), &hint).await
                                    }
                                }
                                return;
                            }
                            "use" => {
                                if arg.is_empty() {
                                    self.reply(&conv, "用法：/ws use <name>", &hint).await;
                                    return;
                                }
                                match self.store.get_config(&workspace_key(arg)).await {
                                    Ok(Some(path)) => {
                                        let p = std::path::Path::new(&path);
                                        if !p.is_dir() {
                                            self.reply(
                                                &conv,
                                                &format!("目录不存在：{path}"),
                                                &hint,
                                            )
                                            .await;
                                            return;
                                        }
                                        // 改 per-conv workdir：取 conv 锁串行，与在飞 agent task 隔离（同 /cd）。
                                        let _conv_lock = self.acquire_conv_lock(&conv.0).await;
                                        let _conv_guard = _conv_lock.lock().await;
                                        match self
                                            .store
                                            .set_config(&workdir_key(&conv.0), &path)
                                            .await
                                        {
                                            Ok(_) => {
                                                self.reply(
                                                    &conv,
                                                    &format!("✅ 已切到「{arg}」（{path}）"),
                                                    &hint,
                                                )
                                                .await
                                            }
                                            Err(e) => {
                                                self.reply(&conv, &format!("切换失败：{e}"), &hint)
                                                    .await
                                            }
                                        }
                                    }
                                    Ok(None) => {
                                        self.reply(&conv, &format!("无此工作空间：{arg}"), &hint)
                                            .await
                                    }
                                    Err(e) => {
                                        self.reply(&conv, &format!("读取失败：{e}"), &hint).await
                                    }
                                }
                                return;
                            }
                            "remove" => {
                                if arg.is_empty() {
                                    self.reply(&conv, "用法：/ws remove <name>", &hint).await;
                                    return;
                                }
                                match self.store.delete_config(&workspace_key(arg)).await {
                                    Ok(_) => {
                                        self.reply(
                                            &conv,
                                            &format!("✅ 已删除工作空间「{arg}」"),
                                            &hint,
                                        )
                                        .await
                                    }
                                    Err(e) => {
                                        self.reply(&conv, &format!("删除失败：{e}"), &hint).await
                                    }
                                }
                                return;
                            }
                            _ => {
                                self.reply(
                                    &conv,
                                    "用法：/ws [list|save <name>|use <name>|remove <name>]",
                                    &hint,
                                )
                                .await
                            }
                        }
                        return;
                    }
                    "/img" => {
                        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
                        if arg.is_empty() {
                            self.reply(
                                &conv,
                                "用法：/img <图片路径>（相对当前工作目录或绝对路径）",
                                &hint,
                            )
                            .await;
                            return;
                        }
                        let wd = self.resolve_workdir(&conv.0).await;
                        let raw = std::path::Path::new(arg);
                        let joined = if raw.is_absolute() {
                            raw.to_path_buf()
                        } else {
                            wd.join(raw)
                        };
                        // 安全校验：canonicalize 后必须仍在 workdir 内——与 agent 的
                        // Read 权限对齐（能 Read 才能发），防任意路径（~/.ssh 等）外传。
                        let wd_real = match wd.canonicalize() {
                            Ok(p) => p,
                            Err(e) => {
                                self.reply(&conv, &format!("工作目录不可用：{e}"), &hint)
                                    .await;
                                return;
                            }
                        };
                        let real = match joined.canonicalize() {
                            Ok(p) => p,
                            Err(_) => {
                                self.reply(&conv, &format!("文件不存在：{arg}"), &hint)
                                    .await;
                                return;
                            }
                        };
                        if !real.starts_with(&wd_real) {
                            self.reply(
                                &conv,
                                &format!("拒绝：{arg} 不在当前工作目录内（/cd 可切换）"),
                                &hint,
                            )
                            .await;
                            return;
                        }
                        if !real.is_file() {
                            self.reply(&conv, &format!("不是文件：{arg}"), &hint).await;
                            return;
                        }
                        let ext_ok = real
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|e| {
                                matches!(
                                    e.to_ascii_lowercase().as_str(),
                                    "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
                                )
                            })
                            .unwrap_or(false);
                        if !ext_ok {
                            self.reply(&conv, "仅支持图片（png/jpg/jpeg/gif/webp/bmp）", &hint)
                                .await;
                            return;
                        }
                        let media = MediaRef {
                            kind: "image".to_string(),
                            url: real.to_string_lossy().into_owned(),
                        };
                        match self.platform.send_media(&conv, &media, &hint).await {
                            Ok(()) => self.reply(&conv, &format!("✅ 已发送：{arg}"), &hint).await,
                            Err(e) => self.reply(&conv, &format!("发送失败：{e}"), &hint).await,
                        }
                        return;
                    }
                    "/perm" => {
                        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
                        if arg.is_empty() {
                            let cur = *self.permission_mode.read();
                            self.reply(
                                &conv,
                                &format!("当前权限模式：{cur:?}\n用法：/perm <off|allow|deny|ask>"),
                                &hint,
                            )
                            .await;
                            return;
                        }
                        // P5-2（安全）：权限模式影响全局审批策略（热切 off 即拆掉 IM
                        // 审批闭环），与 /config 同级敏感，须管理员。
                        if !self.is_admin(&sender.0) {
                            self.reply(&conv, "仅管理员（admin_senders）可修改权限模式。", &hint)
                                .await;
                            return;
                        }
                        match arg {
                            "off" | "allow" | "deny" | "ask" => {
                                let mode = PermissionMode::from_str_lossy(arg);
                                self.reload_permission_mode(mode);
                                // Ask 模式的权限审批 socket 仅在 run() 启动时按当时模式
                                // spawn 一次，热切到 Ask 不会补起 socket（重启生效）。
                                let note = if arg == "ask" {
                                    "（注意：Ask 模式的权限审批 socket 需重启 imagent 才生效）"
                                } else {
                                    ""
                                };
                                self.reply(&conv, &format!("✅ 权限模式已切到 {arg}{note}"), &hint)
                                    .await;
                            }
                            _ => {
                                self.reply(&conv, "用法：/perm <off|allow|deny|ask>", &hint)
                                    .await
                            }
                        }
                        return;
                    }
                    "/stop" => {
                        // P4-1：中断该 conv 的在飞 agent 任务。**不取 conv 串行锁**——
                        // 取了会等到任务自然结束才生效（等价于没停）。
                        // 若正等 IM 权限审批：pending 回复通道被 cancel 以 deny 唤醒 →
                        // MCP 立即收到 deny（fail-closed），agent 侧不悬挂。
                        self.router.cancel(&conv.0).await;
                        // P5-16：收敛审批询问本身——把 IM 里滞留的询问卡片 patch 成
                        // 「已中断」（纯文本询问平台 no-op）。best-effort：失败不阻断中断。
                        if let Err(e) = self.platform.cancel_permission_ask(&conv).await {
                            warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "撤回权限询问失败（不影响中断）");
                        }
                        let running = self.running.lock().await.remove(&conv.0);
                        let aborted = if let Some(h) = &running {
                            // abort → backend.run future drop → 杀子进程：
                            // CLI 后端 kill_on_drop；ACP 后端 cancel 分支 → 杀连接。
                            h.abort();
                            true
                        } else {
                            false
                        };
                        // stop = 全停：清空排队待合并的消息（P4-2 批处理队列）。
                        let dropped = self
                            .queues
                            .lock()
                            .await
                            .remove(&conv.0)
                            .map(|q| q.len())
                            .unwrap_or(0);
                        let text = match (aborted, dropped) {
                            (true, 0) => "🛑 已中断当前任务".to_string(),
                            (true, n) => format!("🛑 已中断当前任务（丢弃 {n} 条排队消息）"),
                            (false, 0) => "ℹ️ 当前没有运行中的任务".to_string(),
                            (false, n) => {
                                format!("ℹ️ 当前没有运行中的任务（丢弃 {n} 条排队消息）")
                            }
                        };
                        self.reply(&conv, &text, &hint).await;
                        return;
                    }
                    "/help" => {
                        self.reply(
                            &conv,
                            "命令：\n/new 重置会话\n/switch <name> 切命名会话\n/sessions 列会话\n/resume [n] 恢复历史/本机会话\n/compact 压缩上下文\n/cd [path] 切工作目录\n/ws [list|save|use|remove] 命名工作空间\n/img <path> 发图片\n/perm <off|allow|deny|ask> 权限模式\n/stop 中断当前任务\n/config [k v] 查看/热改配置\n/status 状态\n/doctor 自检\n/reconnect 重连\n/allow <id> 授权\n/disallow <id> 撤权\n/chat [allow|deny|list] 会话白名单\n/list 白名单\n/whoami 我的id\n/help 帮助",
                            &hint,
                        )
                        .await;
                        return;
                    }
                    _ => {
                        self.reply(
                            &conv,
                            &format!(
                                "未知命令: {cmd}（支持: /new /switch /sessions /resume /compact /cd /ws /img /perm /stop /config /status /doctor /reconnect /allow /disallow /chat /list /whoami /help）"
                            ),
                            &hint,
                        )
                        .await;
                        return;
                    }
                }
            }
        }

        // 4. 普通消息。
        // 文本与媒体皆空才丢弃；媒体消息（无文本）仍驱动 agent。
        // 纯媒体但全部下载失败：向用户报真实错误，不静默。
        if msg.text.as_deref().unwrap_or("").trim().is_empty() && msg.media.is_empty() {
            if !msg.media_errors.is_empty() {
                let errs = msg.media_errors.join("; ");
                self.reply(
                    &conv,
                    &format!(
                        "⚠️ 收到的媒体处理失败，无法查看：{errs}\n（常见原因：应用缺少 im:message:readonly 权限或权限未发布生效；详见服务端日志）"
                    ),
                    &hint,
                )
                .await;
            }
            return;
        }

        // P4-2 批处理：runner 在飞则入队（下一轮合并）后即返；否则本 task 成为
        // runner。runner 循环持 conv 串行锁跨轮次（slash 命令仍排队其后），每轮前
        // 等批处理窗口吃进连发消息；队空则交还 runner 身份、释放锁退出。
        if !self.enqueue_or_become_runner(&conv.0, msg, &hint).await {
            return;
        }
        let lock = self.acquire_conv_lock(&conv.0).await;
        let _guard = lock.lock().await;
        while let Some(batch) = self.take_batch_after_window(&conv.0).await {
            let merged = merge_batch(batch);
            self.run_agent_round(merged).await;
        }
        drop(_guard);
        self.release_conv_lock(&conv.0, lock).await;
    }

    /// 单轮 agent 执行（P4 批处理 runner 循环的循环体）：合并后的消息 → typing →
    /// 续接 session → 媒体提示 / 前情摘要注入 → 流式收集（含空闲看门狗）→ 回传 →
    /// 落库。conv 串行锁由调用方（runner 循环）持有，本函数不再管理锁。
    ///
    /// 中止语义（P4-1/P4-3）：`/stop` 或空闲看门狗 abort join task →
    /// `JoinError::is_cancelled` 分支——卡片 finalize 成 Error 终态（防流式卡片停在
    /// 「生成中」），不落 session（保留上次成功映射）。
    async fn run_agent_round(&self, msg: InboundMessage) {
        let conv_key = msg.conv_id.0.clone();
        self.run_round_inner(msg).await;
        // 统一收尾：移除在飞注册（inner 未及注册时为幂等 no-op）。同 conv 轮次串行
        // （conv 锁），key 移除无 ABA。
        self.running.lock().await.remove(&conv_key);
    }

    async fn run_round_inner(&self, msg: InboundMessage) {
        let conv = msg.conv_id.clone();
        let hint = msg.reply_hint.clone();
        let base_prompt = msg.text.clone().unwrap_or_default();

        // best-effort typing 指示（agent 处理中）；失败仅 log，不阻塞后续。
        let _ = self.platform.send_typing(&conv, &hint).await;

        // 取续接 session；store 错误仅 log 后当 None。
        let existing: Option<SessionId> = match self.store.get_session(&conv.0).await {
            Ok(Some(row)) => {
                // 校验 agent_kind：跨后端切换时不复用旧 session_id（格式不兼容会错乱）。
                if row.agent_kind == self.backend.name() {
                    Some(SessionId(row.session_id))
                } else {
                    warn!(
                        target: "imagent::core",
                        conv_id = %conv.0,
                        stored = %row.agent_kind,
                        current = %self.backend.name(),
                        "session 的 agent_kind 与当前后端不一致，按新建处理"
                    );
                    None
                }
            }
            Ok(None) => None,
            Err(e) => {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "get_session 失败，按新建处理");
                None
            }
        };

        // 媒体提示：把本地媒体路径前置告知 agent（claude 可 Read 本地文件）；
        // 下载失败的媒体也一并列出，让 agent 知道用户附了图但没拿到。
        let media_hint = if msg.media.is_empty() && msg.media_errors.is_empty() {
            String::new()
        } else {
            let mut lines: Vec<String> = msg
                .media
                .iter()
                .map(|m| format!("- {}：{}", m.kind, m.url))
                .collect();
            lines.extend(
                msg.media_errors
                    .iter()
                    .map(|e| format!("- ⚠️ 该媒体获取失败：{e}")),
            );
            format!("【用户发来媒体】\n{}\n\n——\n\n", lines.join("\n"))
        };

        // 新建 session（无 existing）时，一次性注入压缩摘要作为前情摘要。
        // P1-K：摘要删除推迟到 run 成功落库后——若 run 失败（session 未建成），
        // 保留摘要供下次新建注入，避免永久丢失。
        let mut prompt = base_prompt;
        let mut injected_compact_summary = false;
        if existing.is_none() {
            if let Ok(Some(summary)) = self.store.get_config(&compact_summary_key(&conv.0)).await {
                if !summary.is_empty() {
                    prompt = format!("【前情摘要】{summary}\n\n——\n\n{prompt}");
                    injected_compact_summary = true;
                }
            }
        }
        // 媒体提示置最前（在摘要之后、文本之前由上方顺序保证；此处统一前置）。
        if !media_hint.is_empty() {
            prompt = format!("{media_hint}{prompt}");
        }

        // 流式通道 + 后台执行。existing 移入 spawn（避免借用跨 'static）。
        let run_started = Instant::now();
        let (tx, mut rx) = mpsc::channel::<AgentChunk>(32);
        let backend = self.backend.clone();
        let workdir = self.resolve_workdir(&conv.0).await;
        let tools = self.allowed_tools.read().clone();
        let prompt_owned = prompt.clone();
        let conv_id_owned = conv.0.clone();
        let agent_timeout = self.agent_timeout;
        // P5-5：本轮传入的 session 快照（与落库用 workdir 快照）——中断/失败分支
        // 走不到下方统一 upsert，需要它们判断「backend 是否已建立新会话」。
        let existing_sid = existing.as_ref().map(|s| s.0.clone());
        // 落库 workdir 记本轮实际使用的目录（resolve 后的 per-conv 值），而非
        // default——/cd 后两才会分叉（P5 修正，与 /resume 的记法对齐）。
        let workdir_for_row = workdir.to_string_lossy().to_string();
        let join = tokio::spawn(async move {
            let backend_name = backend.name();
            match tokio::time::timeout(
                agent_timeout,
                backend.run(
                    &conv_id_owned,
                    &prompt_owned,
                    existing.as_ref(),
                    &workdir,
                    &tools,
                    tx,
                ),
            )
            .await
            {
                Ok(res) => res,
                Err(_elapsed) => {
                    METRICS.agent_timeouts.with_label_values(&["total"]).inc();
                    Err(crate::error::CoreError::Backend(
                        backend_name,
                        format!("agent run timed out after {agent_timeout:?}"),
                    ))
                }
            }
        });
        // P4-1：注册在飞句柄（/stop 中断用）。runner 持 conv 锁跨轮，同 conv 不可能
        // 并发两轮；轮次结束由 run_agent_round 统一移除。
        self.running
            .lock()
            .await
            .insert(conv.0.clone(), join.abort_handle());

        // 收集 chunks：Final/Error 落库，ToolUse 累积用于最终工具摘要。
        let mut final_text: Option<String> = None;
        let mut error_text: Option<String> = None;
        let mut tool_calls: Vec<(String, String)> = Vec::new();
        // agent 产出的媒体文件路径（Write 图片）；run 结束后回传 IM。
        let mut media_out: Vec<String> = Vec::new();
        // 流式卡片：支持卡片的平台累积输出 + 节流 patch（单卡片更新），不支持则每 Text 多发文本。
        let mut card = if self.platform.supports_streaming_card(&conv) {
            Some(CardSession::new())
        } else {
            None
        };
        // P4-3：空闲看门狗——连续 agent_idle_timeout 无任何 chunk 则 abort（杀子进程）。
        // 等权限审批期间暂停（审批有独立的 permission_ask_timeout 预算兜底）。
        let mut idle_timed_out = false;
        // P5-5：backend 提前学到的 session id（SessionStarted chunk）——中断/失败
        // 路径拿不到 RunOutcome，靠它保住已建立的会话。
        let mut learned_sid: Option<String> = None;
        // P5-10：非卡片平台已实时推送的 Text 前缀——最终回复只补差量，防重发。
        let mut streamed_text = String::new();
        loop {
            // P4-6：COT 档位每轮读取（/config 热改对下一轮生效）。
            let cot = *self.cot_detail.read();
            let idle_timeout = *self.agent_idle_timeout.read();
            let chunk = if idle_timeout.is_zero() {
                match rx.recv().await {
                    Some(c) => c,
                    None => break,
                }
            } else {
                match tokio::time::timeout(idle_timeout, rx.recv()).await {
                    Ok(Some(c)) => c,
                    Ok(None) => break,
                    Err(_) if self.router.has_pending(&conv.0).await => continue,
                    Err(_) => {
                        idle_timed_out = true;
                        METRICS.agent_timeouts.with_label_values(&["idle"]).inc();
                        warn!(
                            target: "imagent::core",
                            conv_id = %conv.0,
                            idle = ?idle_timeout,
                            "agent 空闲超时（连续无输出），终止本轮"
                        );
                        break;
                    }
                }
            };
            match chunk {
                AgentChunk::SessionStarted(sid) => {
                    // 仅记录，不产生 IM 输出；正常路径 RunOutcome 仍为权威值。
                    if learned_sid.as_deref() != Some(sid.as_str()) {
                        learned_sid = Some(sid);
                    }
                }
                AgentChunk::Final(t) => final_text = Some(t),
                AgentChunk::Error(e) => error_text = Some(e),
                AgentChunk::ToolUse { tool, input } => {
                    // P4-6：off 档不收集工具过程（无摘要、无卡片工具面板）。
                    if cot == CotDetail::Off {
                        continue;
                    }
                    let summary = truncate_str(&input, cot.input_trunc());
                    tool_calls.push((tool.clone(), summary.clone()));
                    if let Some(c) = card.as_mut() {
                        c.append_tool(&tool, &summary, &conv, &hint, self.platform.as_ref())
                            .await;
                    }
                }
                AgentChunk::ToolResult { .. } => {} // 摘要只列工具调用，结果不进 IM
                AgentChunk::Media { path } => {
                    media_out.push(path);
                }
                AgentChunk::Text(t) => {
                    if let Some(c) = card.as_mut() {
                        c.append_text(&t, &conv, &hint, self.platform.as_ref())
                            .await;
                    } else {
                        // P2-F：中间 Text chunk 实时推 IM（流式体验，而非全部丢弃只发最终 Final）。
                        // P5-10：累积已推前缀，最终回复据此只补差量。
                        streamed_text.push_str(&t);
                        self.reply(&conv, &t, &hint).await;
                    }
                }
            }
        }

        // P4-3：空闲超时 → abort join（杀子进程链路同 /stop），走下方 cancelled 分支。
        if idle_timed_out {
            join.abort();
        }

        // 等待 backend 返回 RunOutcome。
        let outcome = match join.await {
            Ok(Ok(o)) => {
                let elapsed = run_started.elapsed().as_secs_f64();
                METRICS.backend_calls.inc();
                METRICS.backend_duration.observe(elapsed);
                o
            }
            Ok(Err(e)) => {
                METRICS.backend_errors.inc();
                let m = format!("[error] {e}");
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "backend.run 失败");
                if let Some(c) = card.as_mut() {
                    c.finalize(
                        Some(m.as_str()),
                        &tool_calls,
                        CardTerminal::Error(m.clone()),
                        &conv,
                        &hint,
                        self.platform.as_ref(),
                    )
                    .await;
                } else {
                    self.reply(&conv, &m, &hint).await;
                }
                // P5-5：失败路径保住已学到的 session id——部分失败轮次（如正常完成
                // 但无最终文本被 backend 判 Err）会话本身是好的，落库后下条消息
                // 续接而非静默开新会话。
                self.persist_learned_session(&conv, existing_sid.as_deref(), &learned_sid)
                    .await;
                // conv 锁由 runner 循环持有并统一释放（P1-7 防泄漏语义不变）。
                return;
            }
            Err(e) if e.is_cancelled() => {
                // P4-1/P4-3：join task 被 abort——/stop（用户中断）或空闲看门狗。
                METRICS.backend_errors.inc();
                if idle_timed_out {
                    let m = format!(
                        "⏱️ agent 已连续 {:?} 无输出，空闲超时终止本轮。已进行到的进度已保留，下条消息将续接（全新开始可 /new）。",
                        *self.agent_idle_timeout.read()
                    );
                    if let Some(c) = card.as_mut() {
                        c.finalize(
                            Some(m.as_str()),
                            &tool_calls,
                            CardTerminal::Error(m.clone()),
                            &conv,
                            &hint,
                            self.platform.as_ref(),
                        )
                        .await;
                    } else {
                        self.reply(&conv, &m, &hint).await;
                    }
                } else {
                    warn!(
                        target: "imagent::core",
                        conv_id = %conv.0,
                        "agent 任务被用户 /stop 中断"
                    );
                    // /stop 命令侧已回确认，这里只把流式卡片收敛到终态（防停在「生成中」）。
                    if let Some(c) = card.as_mut() {
                        c.finalize(
                            Some(""),
                            &tool_calls,
                            CardTerminal::Error("已中断".into()),
                            &conv,
                            &hint,
                            self.platform.as_ref(),
                        )
                        .await;
                    }
                }
                // P5-5：中断路径保住已学到的 session id（与 Claude Code 自身的中断
                // 语义一致：中断留在原会话，显式 /new 才重开）。会话进度保留后，
                // 下条消息续接本轮已进行到的部分。
                self.persist_learned_session(&conv, existing_sid.as_deref(), &learned_sid)
                    .await;
                return;
            }
            Err(e) => {
                METRICS.backend_errors.inc();
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "backend task panic");
                // P2-5：panic 时若已收到 Final chunk，优先回传它（而非丢弃只报 panic）。
                let m = final_text.unwrap_or_else(|| format!("[error] backend task panicked: {e}"));
                if let Some(c) = card.as_mut() {
                    c.finalize(
                        Some(m.as_str()),
                        &tool_calls,
                        CardTerminal::Error(m.clone()),
                        &conv,
                        &hint,
                        self.platform.as_ref(),
                    )
                    .await;
                } else {
                    self.reply(&conv, &m, &hint).await;
                }
                self.persist_learned_session(&conv, existing_sid.as_deref(), &learned_sid)
                    .await;
                return;
            }
        };

        // 回传文本优先级：收到过的 Final > outcome.final_text > session_id 提示。
        if let Some(et) = &error_text {
            // 收到 Error chunk 也算需要提示（但 backend 正常返回，故只记录）。
            warn!(target: "imagent::core", conv_id = %conv.0, error = %et, "backend 产出 Error chunk");
        }
        let final_text_is_present = final_text.is_some();
        let outcome_has_final = !outcome.final_text.is_empty();
        let mut reply = if let Some(f) = final_text {
            f
        } else if outcome_has_final {
            outcome.final_text
        } else {
            format!("(done, session={})", outcome.session_id.0)
        };
        // P5-10：非卡片平台已实时推送过 Text 增量——最终回复只补差量，防
        // codex/gemini/ACP（中间 Text 流式 + Final 全量）整段重发两遍。final 与
        // 已推前缀不对齐（后端语义异常）时保留全量：宁可偶发重复，不可丢内容。
        if card.is_none() && !streamed_text.is_empty() {
            if let Some(rest) = reply.strip_prefix(streamed_text.as_str()) {
                reply = rest.to_string();
            }
        }
        // 工具调用摘要：仅无卡片平台（ilink/wecom）追加文本摘要；卡片平台由 render_card
        // 的折叠面板统一渲染，避免正文与卡片块重复展示工具调用。
        if !tool_calls.is_empty() && card.is_none() && (final_text_is_present || outcome_has_final)
        {
            reply.push_str(&format_tool_summary(&tool_calls, *self.cot_detail.read()));
        }
        // R1：backend 标记非正常终止（崩溃等）时，回复前置告警，让用户感知是部分输出而非正常结果。
        if !outcome.terminal {
            reply = format!("⚠️ agent 异常退出，以下为部分输出：\n\n{reply}");
        }
        if let Some(c) = card.as_mut() {
            let terminal = if outcome.terminal {
                CardTerminal::Done
            } else {
                CardTerminal::Error("agent 异常退出".into())
            };
            c.finalize(
                Some(reply.as_str()),
                &tool_calls,
                terminal,
                &conv,
                &hint,
                self.platform.as_ref(),
            )
            .await;
        } else if !reply.is_empty() {
            // P5-10：流式已推完且无差量、无工具摘要时不发空消息。
            self.reply(&conv, &reply, &hint).await;
        }

        // agent 产图回传：run 结束文件已写完；存在才发，单个失败仅 warn 不影响其余。
        for mpath in &media_out {
            let p = std::path::Path::new(mpath);
            if !p.is_file() {
                warn!(target: "imagent::core", conv_id = %conv.0, path = %mpath, "产出的媒体文件不存在，跳过回传");
                continue;
            }
            let media = MediaRef {
                kind: "image".to_string(),
                url: mpath.clone(),
            };
            if let Err(e) = self.platform.send_media(&conv, &media, &hint).await {
                warn!(target: "imagent::core", conv_id = %conv.0, path = %mpath, error = %e, "send_media 回传失败");
            }
        }

        // 落库（upsert 内部保留 created_at；store 错误仅 log）。
        let now = now_secs();
        // 当前活动命名（不存在/空 = 默认未命名）。
        let active_name = self
            .store
            .get_config(&active_name_key(&conv.0))
            .await
            .unwrap_or(None)
            .filter(|s| !s.is_empty());
        // N8 配套：非正常终止（崩溃等）时 session_id 可能空——agent 未及分配。空 session_id
        // 无法 --resume，不入库（保留既有有效映射，避免写入无效值导致下次续接失败）。
        if outcome.session_id.0.is_empty() {
            warn!(
                target: "imagent::core",
                conv_id = %conv.0,
                "backend 返回空 session_id（疑似非正常终止），不更新 session 映射"
            );
        } else {
            let row = SessionRow {
                conv_id: conv.0.clone(),
                session_id: outcome.session_id.0.clone(),
                agent_kind: self.backend.name().to_string(),
                workdir: workdir_for_row.clone(),
                name: active_name.clone(),
                created_at: now,
                updated_at: now,
            };
            if let Err(e) = self.store.upsert_session(&row).await {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "upsert_session 失败");
            }
            // 有命名时，同步写命名侧表（可恢复/历史）。
            if let Some(name) = &active_name {
                let nrow = NamedSessionRow {
                    conv_id: conv.0.clone(),
                    name: name.clone(),
                    session_id: outcome.session_id.0.clone(),
                    agent_kind: Some(self.backend.name().to_string()),
                    workdir: Some(workdir_for_row.clone()),
                    created_at: now,
                    updated_at: now,
                };
                if let Err(e) = self.store.upsert_named_session(&nrow).await {
                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "upsert_named_session 失败");
                }
            }
        }

        // P1-K：run 成功落库后，删除已注入的 compact_summary（一次性）。
        // 失败路径已在上方 return，不会走到这里，故 summary 不会丢失。
        if injected_compact_summary {
            if let Err(e) = self
                .store
                .delete_config(&compact_summary_key(&conv.0))
                .await
            {
                warn!(
                    target: "imagent::core",
                    conv_id = %conv.0,
                    error = %e,
                    "delete_config(compact_summary) 失败（best-effort）"
                );
            }
        }
        // conv 锁由 runner 循环持有并统一释放；在飞注册由 run_agent_round 统一移除。
    }

    /// 回传文本；发送失败仅 log。session 过期升级为 error（用户侧已收不到回复）。
    async fn reply(&self, conv: &ConvId, text: &str, hint: &ReplyHint) {
        match self.platform.send_text(conv, text, hint).await {
            Ok(()) => METRICS.messages_out.inc(),
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
            }
        }
    }
}

/// 本进程的 uid（peer-uid 鉴权用）。
#[cfg(unix)]
#[allow(unsafe_code)] // crate 顶层 `#![deny(unsafe_code)]`，此处显式豁免
fn current_uid() -> u32 {
    // SAFETY: getuid/geteuid 无参数、无副作用，永远安全。
    // P2-8：Linux SO_PEERCRED 返回对端 real uid → 比对 getuid；
    // macOS LOCAL_PEERCRED 返回 effective uid → 比对 geteuid（避免 setuid 部署下
    // real != effective 导致 Ask 闭环全部误拒、可用性归零）。
    #[cfg(target_os = "macos")]
    {
        unsafe { libc::geteuid() }
    }
    #[cfg(not(target_os = "macos"))]
    {
        unsafe { libc::getuid() }
    }
}

/// 取 UnixStream 对端的 uid（用于权限 socket 鉴权）。
///
/// - Linux: `SO_PEERCRED`
/// - macOS: `LOCAL_PEERCRED`
/// - 其它 unix: 返回 None（调用方应拒绝）。
#[cfg(unix)]
#[allow(unsafe_code)] // crate 顶层 `#![deny(unsafe_code)]`，此处显式豁免
fn peer_uid(stream: &tokio::net::UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    // SAFETY: getsockopt 对已连接的 unix socket 按 optname 填充固定大小的输出缓冲，
    // 传入正确的 len。MaybeUninit/zeroed 避免读取未初始化字段。
    unsafe {
        #[cfg(target_os = "linux")]
        {
            let mut cred: std::mem::MaybeUninit<libc::ucred> = std::mem::MaybeUninit::uninit();
            let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            let rc = libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                cred.as_mut_ptr() as *mut libc::c_void,
                &mut len,
            );
            if rc == 0 {
                Some((*cred.as_ptr()).uid)
            } else {
                None
            }
        }
        #[cfg(target_os = "macos")]
        {
            let mut xucred: libc::xucred = std::mem::zeroed();
            let mut len = std::mem::size_of::<libc::xucred>() as libc::socklen_t;
            let rc = libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERCRED,
                &mut xucred as *mut libc::xucred as *mut libc::c_void,
                &mut len,
            );
            // cr_uid == u32::MAX 表示未填充/无效。
            if rc == 0 && xucred.cr_uid != u32::MAX {
                Some(xucred.cr_uid)
            } else {
                None
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = fd;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ConvId, LocalSession, ReplyHint, SessionId, UserId};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Mutex as TokioMutex;

    #[cfg(unix)]
    #[tokio::test]
    async fn read_line_capped_rejects_oversized() {
        // P1-9：超过上限（无换行）→ Err，防同 uid 进程发巨大行 OOM。
        let bytes: Vec<u8> = vec![b'x'; 1000];
        let mut reader = tokio::io::BufReader::new(&bytes[..]);
        let res = Dispatcher::read_line_capped(&mut reader, 100).await;
        assert!(res.is_err(), "oversized line must error");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_line_capped_reads_normal_line() {
        let bytes: &[u8] = b"{\"conv_id\":\"c1\"}\nextra";
        let mut reader = tokio::io::BufReader::new(bytes);
        let line = Dispatcher::read_line_capped(&mut reader, 1024)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(line, "{\"conv_id\":\"c1\"}\n");
    }

    #[tokio::test]
    async fn conv_lock_released_on_backend_failure() {
        // P1-7：backend.run 失败时，handle 的失败 return 应释放 conv_lock，
        // 不在 conv_locks 留泄漏项。
        let _g = SERIAL.lock().await;
        let auth = Auth::new(vec!["u1".into()]);
        let (plat, inbox, send_count) = MockPlatform::new();
        let (back, _calls, _prompts, _order) = MockBackend::new_failing();
        let (store, _db) = tmp_store().await;
        let disp = Arc::new(Dispatcher::new(
            Arc::new(plat),
            Arc::new(back),
            store,
            auth,
            std::path::PathBuf::from("/tmp/imagent-test-ws"),
            vec!["Read".into()],
            PermissionMode::Off,
            test_budgets(),
            CotDetail::Brief,
            vec![],
        ));
        disp.handle(msg("c1", "u1", "hello")).await;
        // 失败路径 release 后，conv_locks 应为空（c1 已被移除，非永久泄漏）。
        let map = disp.conv_locks.lock().await;
        assert!(
            map.is_empty(),
            "conv_locks 应在 backend 失败后为空，残留: {:?}",
            map.keys().collect::<Vec<_>>()
        );
        drop(inbox);
        drop(send_count);
    }

    /// 串行化 dispatch 集成测试：避免并行开 /tmp WAL sqlite 触发 SQLITE_IOERR(1802)。
    static SERIAL: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));
    #[test]
    fn is_session_expired_err_classifies() {
        use crate::error::CoreError;
        // SessionExpired variant 命中。
        assert!(is_session_expired_err(&CoreError::SessionExpired(
            "re-login required".into()
        )));
        assert!(is_session_expired_err(&CoreError::SessionExpired(
            "please re-login".into()
        )));
        // 其它 variant 不命中。
        assert!(!is_session_expired_err(&CoreError::Platform(
            "ilink",
            "getupdates exhausted retries".into()
        )));
        assert!(!is_session_expired_err(&CoreError::Config("bad".into())));
        assert!(!is_session_expired_err(&CoreError::Store(
            imagent_store::StoreError::Other("db: some failure".into())
        )));
        assert!(!is_session_expired_err(&CoreError::Platform(
            "ilink",
            "Session Expired".into()
        )));
    }

    type InboxHandle = Arc<TokioMutex<Vec<String>>>;
    type CounterHandle = Arc<AtomicUsize>;
    type CallsHandle = Arc<TokioMutex<Vec<Option<String>>>>;
    type PromptsHandle = Arc<TokioMutex<Vec<String>>>;

    // ---------- mock platform ----------

    /// mock platform：`inbox` 收到的出站文本，`recv_queue` 可编程的入站流。
    struct MockPlatform {
        recv_queue: Arc<TokioMutex<Option<Vec<InboundMessage>>>>,
        inbox: Arc<TokioMutex<Vec<String>>>,
        send_count: Arc<AtomicUsize>,
    }

    impl MockPlatform {
        fn new() -> (Self, InboxHandle, CounterHandle) {
            let inbox = Arc::new(TokioMutex::new(Vec::new()));
            let send_count = Arc::new(AtomicUsize::new(0));
            let p = Self {
                recv_queue: Arc::new(TokioMutex::new(None)),
                inbox: inbox.clone(),
                send_count: send_count.clone(),
            };
            (p, inbox, send_count)
        }
    }

    #[async_trait]
    impl Platform for MockPlatform {
        async fn recv(&self) -> Result<InboundMessage> {
            loop {
                let mut q = self.recv_queue.lock().await;
                if let Some(list) = q.as_mut() {
                    if !list.is_empty() {
                        return Ok(list.remove(0));
                    }
                }
                drop(q);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        async fn send_text(&self, _conv: &ConvId, text: &str, _hint: &ReplyHint) -> Result<()> {
            self.inbox.lock().await.push(text.to_string());
            self.send_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn send_media(
            &self,
            _conv: &ConvId,
            media: &crate::types::MediaRef,
            _hint: &ReplyHint,
        ) -> Result<()> {
            // 以 [media:<url>] 记入 inbox，供 /img 等测试断言回传内容。
            self.inbox
                .lock()
                .await
                .push(format!("[media:{}]", media.url));
            self.send_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn name(&self) -> &'static str {
            "mock"
        }
    }

    // ---------- mock backend ----------

    struct MockBackend {
        calls: Arc<TokioMutex<Vec<Option<String>>>>,
        prompts: Arc<TokioMutex<Vec<String>>>,
        order: Arc<AtomicUsize>,
        /// 每次 run 前发这些 ToolUse chunk（用于工具摘要测试）。默认空。
        tools_to_emit: Arc<TokioMutex<Vec<(String, String)>>>,
        /// run 直接返 Err（P1-7 失败路径测试用）。
        fail: bool,
        /// 本次 run 构造的 RunOutcome.terminal 值（默认 true = 正常终止）。
        terminal: bool,
        /// run 记录后先 sleep 该时长（P4 /stop、批处理、空闲看门狗测试用）。
        slow_ms: u64,
        /// P5-5：run 开跑即发 SessionStarted chunk（模拟 CLI 首事件带 session id），
        /// 供 /stop 中断路径的 session 持久化测试用。
        announce_session: Option<String>,
        /// P5-10：流式模式——逐段发 Text，Final/RunOutcome 为全量拼接（模拟
        /// codex/gemini/ACP「中间 Text + Final 全量」语义，去重测试用）。默认空。
        stream_texts: Vec<String>,
        /// `list_local_sessions` 返回的本机会话（P4-11 统一 /resume 测试用）。
        local_sessions: Arc<TokioMutex<Vec<LocalSession>>>,
    }

    impl MockBackend {
        fn new() -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
            let calls = Arc::new(TokioMutex::new(Vec::new()));
            let prompts = Arc::new(TokioMutex::new(Vec::new()));
            let order = Arc::new(AtomicUsize::new(0));
            let b = Self {
                calls: calls.clone(),
                prompts: prompts.clone(),
                order: order.clone(),
                tools_to_emit: Arc::new(TokioMutex::new(Vec::new())),
                fail: false,
                terminal: true,
                slow_ms: 0,
                announce_session: None,
                stream_texts: Vec::new(),
                local_sessions: Arc::new(TokioMutex::new(Vec::new())),
            };
            (b, calls, prompts, order)
        }

        /// 返回带可配置 ToolUse 发射的 backend，以及设置 tool 列表的句柄。
        async fn new_with_tools(
            tools: Vec<(String, String)>,
        ) -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
            let (b, calls, prompts, order) = Self::new();
            *b.tools_to_emit.lock().await = tools;
            (b, calls, prompts, order)
        }

        /// run 直接返 Err（P1-7 失败路径测试用）。
        fn new_failing() -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
            let (mut b, calls, prompts, order) = Self::new();
            b.fail = true;
            (b, calls, prompts, order)
        }
        /// run 返回 terminal=false（模拟 agent 崩溃后的部分输出，R1 告警测试用）。
        fn new_non_terminal() -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
            let (mut b, calls, prompts, order) = Self::new();
            b.terminal = false;
            (b, calls, prompts, order)
        }
        /// run 记录后挂起 slow_ms（P4 /stop、批处理合并、空闲看门狗测试用）。
        fn new_slow(slow_ms: u64) -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
            let (mut b, calls, prompts, order) = Self::new();
            b.slow_ms = slow_ms;
            (b, calls, prompts, order)
        }
        /// P5-5：慢后端 + 开跑即 announce session id（/stop 中断保 session 测试用）。
        fn new_slow_with_session(
            slow_ms: u64,
            sid: &str,
        ) -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
            let (mut b, calls, prompts, order) = Self::new();
            b.slow_ms = slow_ms;
            b.announce_session = Some(sid.into());
            (b, calls, prompts, order)
        }
        /// P5-10：流式后端——逐段 Text + Final/RunOutcome 全量（去重测试用）。
        fn new_streaming(texts: Vec<String>) -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
            let (mut b, calls, prompts, order) = Self::new();
            b.stream_texts = texts;
            (b, calls, prompts, order)
        }
        /// `list_local_sessions` 返回固定本机会话列表（P4-11 统一 /resume 测试用）。
        async fn new_with_local(
            local: Vec<LocalSession>,
        ) -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
            let (b, calls, prompts, order) = Self::new();
            *b.local_sessions.lock().await = local;
            (b, calls, prompts, order)
        }
    }

    #[async_trait]
    impl Backend for MockBackend {
        async fn run(
            &self,
            _conv_id: &str,
            prompt: &str,
            session: Option<&SessionId>,
            _workdir: &std::path::Path,
            _allowed_tools: &[String],
            chunks: mpsc::Sender<AgentChunk>,
        ) -> Result<crate::types::RunOutcome> {
            // P1-7 测试：fail 模式直接返 Err，触发 handle 失败路径。
            if self.fail {
                return Err(crate::error::CoreError::Backend(
                    "mock-backend",
                    "mock failure (fail=true)".into(),
                ));
            }
            // 记录续接情况 + 执行顺序 + 收到的 prompt。
            let my_order = self.order.fetch_add(1, Ordering::SeqCst);
            self.calls.lock().await.push(session.map(|s| s.0.clone()));
            self.prompts.lock().await.push(prompt.to_string());

            // P5-5：开跑即 announce（模拟 CLI 首事件带 session id）。
            if let Some(sid) = &self.announce_session {
                let _ = chunks.send(AgentChunk::SessionStarted(sid.clone())).await;
            }

            // 稍微让出调度器，便于测试串行。
            tokio::task::yield_now().await;

            // P4：慢后端——记录后挂起（不发任何 chunk），供 /stop、批处理、空闲
            // 看门狗测试制造「在飞任务」窗口。
            if self.slow_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.slow_ms)).await;
            }

            // 先发配置好的 ToolUse chunk（若有），再发 Final。
            let tools = self.tools_to_emit.lock().await.clone();
            for (tool, input) in tools {
                let _ = chunks.send(AgentChunk::ToolUse { tool, input }).await;
            }
            // P5-10：流式模式——逐段 Text，Final/RunOutcome 为全量拼接。
            let mut full = String::new();
            for t in &self.stream_texts {
                full.push_str(t);
                let _ = chunks.send(AgentChunk::Text(t.clone())).await;
            }
            // 发一个 Final chunk（流式模式 = 全量；否则沿用 reply#N 供既有断言）。
            let final_chunk = if self.stream_texts.is_empty() {
                format!("reply#{my_order}")
            } else {
                full.clone()
            };
            let _ = chunks.send(AgentChunk::Final(final_chunk)).await;

            let outcome_final = if self.stream_texts.is_empty() {
                format!("final-{my_order}")
            } else {
                full
            };
            Ok(crate::types::RunOutcome {
                session_id: SessionId(format!("sess-{my_order}")),
                final_text: outcome_final,
                terminal: self.terminal,
            })
        }
        fn name(&self) -> &'static str {
            "mock-backend"
        }
        async fn list_local_sessions(&self, _workdir: &std::path::Path) -> Vec<LocalSession> {
            self.local_sessions.lock().await.clone()
        }
    }

    // ---------- helpers ----------

    fn msg(conv: &str, sender: &str, text: &str) -> InboundMessage {
        InboundMessage {
            conv_id: ConvId(conv.into()),
            sender: UserId(sender.into()),
            text: Some(text.into()),
            media: Vec::new(),
            media_errors: Vec::new(),
            reply_hint: ReplyHint::None,
        }
    }

    async fn tmp_store() -> (Store, std::path::PathBuf) {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "imagent_core_dispatch_{}_{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(&p).await.expect("open store");
        (store, p)
    }

    /// 构造 dispatcher 并返回各观测句柄。`check()` 每次返回一个指向同一 db 文件的
    /// 新 Store 连接（Store 未 impl Clone；rusqlite WAL 支持多连接，断言用）。
    struct Ctx {
        disp: Arc<Dispatcher>,
        inbox: Arc<TokioMutex<Vec<String>>>,
        send_count: Arc<AtomicUsize>,
        calls: Arc<TokioMutex<Vec<Option<String>>>>,
        prompts: Arc<TokioMutex<Vec<String>>>,
        order: Arc<AtomicUsize>,
        db: std::path::PathBuf,
    }

    impl Ctx {
        async fn check(&self) -> Store {
            Store::open(&self.db).await.expect("reopen store")
        }
    }

    async fn build(auth: Auth) -> Ctx {
        build_with_workdir(auth, std::path::PathBuf::from("/tmp/imagent-test-ws")).await
    }

    /// 与 build 相同但可指定 default_workdir（/img 等需真实文件系统的测试用）。
    async fn build_with_workdir(auth: Auth, default_workdir: std::path::PathBuf) -> Ctx {
        let (plat, inbox, send_count) = MockPlatform::new();
        let (back, calls, prompts, order) = MockBackend::new();
        let (store, db) = tmp_store().await;

        let disp = Arc::new(Dispatcher::new(
            Arc::new(plat),
            Arc::new(back),
            store,
            auth,
            default_workdir,
            vec!["Read".into(), "Edit".into()],
            PermissionMode::Off,
            test_budgets(),
            CotDetail::Brief,
            vec![], // admin_senders 空 = 测试用向后兼容（所有白名单用户可 /allow）
        ));

        Ctx {
            disp,
            inbox,
            send_count,
            calls,
            prompts,
            order,
            db,
        }
    }
    /// 与 build 相同，但 MockBackend 返回 terminal=false（R1 非正常退出告警测试用）。
    async fn build_non_terminal(auth: Auth) -> Ctx {
        let (plat, inbox, send_count) = MockPlatform::new();
        let (back, calls, prompts, order) = MockBackend::new_non_terminal();
        let (store, db) = tmp_store().await;

        let disp = Arc::new(Dispatcher::new(
            Arc::new(plat),
            Arc::new(back),
            store,
            auth,
            std::path::PathBuf::from("/tmp/imagent-test-ws"),
            vec!["Read".into(), "Edit".into()],
            PermissionMode::Off,
            test_budgets(),
            CotDetail::Brief,
            vec![], // admin_senders 空 = 测试用向后兼容（所有白名单用户可 /allow）
        ));

        Ctx {
            disp,
            inbox,
            send_count,
            calls,
            prompts,
            order,
            db,
        }
    }

    /// P2-D：与 build 相同但允许指定 admin_senders（测试角色区分）。
    async fn build_with_admin(auth: Auth, admin_senders: Vec<String>) -> Ctx {
        let (plat, inbox, send_count) = MockPlatform::new();
        let (back, calls, prompts, order) = MockBackend::new();
        let (store, db) = tmp_store().await;
        let disp = Arc::new(Dispatcher::new(
            Arc::new(plat),
            Arc::new(back),
            store,
            auth,
            std::path::PathBuf::from("/tmp/imagent-test-ws"),
            vec!["Read".into(), "Edit".into()],
            PermissionMode::Off,
            test_budgets(),
            CotDetail::Brief,
            admin_senders,
        ));
        Ctx {
            disp,
            inbox,
            send_count,
            calls,
            prompts,
            order,
            db,
        }
    }

    /// 与 build 相同，但 MockBackend 在 Final 前会发指定的 ToolUse chunk。
    async fn build_with_tools(auth: Auth, tools: Vec<(String, String)>) -> Ctx {
        let (plat, inbox, send_count) = MockPlatform::new();
        let (back, calls, prompts, order) = MockBackend::new_with_tools(tools).await;
        let (store, db) = tmp_store().await;

        let disp = Arc::new(Dispatcher::new(
            Arc::new(plat),
            Arc::new(back),
            store,
            auth,
            std::path::PathBuf::from("/tmp/imagent-test-ws"),
            vec!["Read".into(), "Edit".into()],
            PermissionMode::Off,
            test_budgets(),
            CotDetail::Brief,
            vec![], // admin_senders 空 = 测试用向后兼容（所有白名单用户可 /allow）
        ));

        Ctx {
            disp,
            inbox,
            send_count,
            calls,
            prompts,
            order,
            db,
        }
    }

    /// 测试默认预算：与线上默认一致，唯批处理窗口收窄到 1ms（不拖慢顺序喂消息的
    /// 既有用例）。需要窗口/看门狗/慢后端的用例走 [`build_slow`]。
    fn test_budgets() -> TaskBudgets {
        TaskBudgets {
            agent_timeout: Duration::from_secs(600),
            permission_ask_timeout: Duration::from_secs(300),
            shutdown_grace: Duration::from_secs(60),
            agent_idle_timeout: Duration::from_secs(300),
            batch_window: Duration::from_millis(1),
        }
    }

    /// 慢后端 + 自定义预算（P4 /stop、批处理合并、空闲看门狗测试用）。
    /// 本机会话列表可配置的 backend（P4-11 统一 /resume 测试用）。
    async fn build_with_local(auth: Auth, local: Vec<LocalSession>) -> Ctx {
        let (plat, inbox, send_count) = MockPlatform::new();
        let (back, calls, prompts, order) = MockBackend::new_with_local(local).await;
        let (store, db) = tmp_store().await;

        let disp = Arc::new(Dispatcher::new(
            Arc::new(plat),
            Arc::new(back),
            store,
            auth,
            std::path::PathBuf::from("/tmp/imagent-test-ws"),
            vec!["Read".into(), "Edit".into()],
            PermissionMode::Off,
            test_budgets(),
            CotDetail::Brief,
            vec![],
        ));

        Ctx {
            disp,
            inbox,
            send_count,
            calls,
            prompts,
            order,
            db,
        }
    }

    async fn build_slow(auth: Auth, slow_ms: u64, budgets: TaskBudgets) -> Ctx {
        let (plat, inbox, send_count) = MockPlatform::new();
        let (back, calls, prompts, order) = MockBackend::new_slow(slow_ms);
        let (store, db) = tmp_store().await;

        let disp = Arc::new(Dispatcher::new(
            Arc::new(plat),
            Arc::new(back),
            store,
            auth,
            std::path::PathBuf::from("/tmp/imagent-test-ws"),
            vec!["Read".into(), "Edit".into()],
            PermissionMode::Off,
            budgets,
            CotDetail::Brief,
            vec![],
        ));

        Ctx {
            disp,
            inbox,
            send_count,
            calls,
            prompts,
            order,
            db,
        }
    }

    /// P5-5：与 build_slow 相同，但慢后端开跑即 announce session id
    /// （/stop 中断保 session 测试用）。
    async fn build_slow_with_session(
        auth: Auth,
        slow_ms: u64,
        sid: &str,
        budgets: TaskBudgets,
    ) -> Ctx {
        let (plat, inbox, send_count) = MockPlatform::new();
        let (back, calls, prompts, order) = MockBackend::new_slow_with_session(slow_ms, sid);
        let (store, db) = tmp_store().await;

        let disp = Arc::new(Dispatcher::new(
            Arc::new(plat),
            Arc::new(back),
            store,
            auth,
            std::path::PathBuf::from("/tmp/imagent-test-ws"),
            vec!["Read".into(), "Edit".into()],
            PermissionMode::Off,
            budgets,
            CotDetail::Brief,
            vec![],
        ));

        Ctx {
            disp,
            inbox,
            send_count,
            calls,
            prompts,
            order,
            db,
        }
    }

    /// P5-10：流式后端（逐段 Text + Final 全量），非卡片平台去重测试用。
    async fn build_streaming(auth: Auth, texts: Vec<String>) -> Ctx {
        let (plat, inbox, send_count) = MockPlatform::new();
        let (back, calls, prompts, order) = MockBackend::new_streaming(texts);
        let (store, db) = tmp_store().await;

        let disp = Arc::new(Dispatcher::new(
            Arc::new(plat),
            Arc::new(back),
            store,
            auth,
            std::path::PathBuf::from("/tmp/imagent-test-ws"),
            vec!["Read".into(), "Edit".into()],
            PermissionMode::Off,
            test_budgets(),
            CotDetail::Brief,
            vec![],
        ));

        Ctx {
            disp,
            inbox,
            send_count,
            calls,
            prompts,
            order,
            db,
        }
    }

    /// 等待 conv 的在飞任务注册出现（join spawn 后写入 running map）。
    async fn wait_registered(ctx: &Ctx, conv: &str) -> bool {
        for _ in 0..400 {
            if ctx.disp.running.lock().await.contains_key(conv) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        false
    }

    /// 把消息喂给 dispatcher 的 mock platform recv，并等待处理完成。
    async fn feed_and_wait(ctx: &Ctx, msgs: Vec<InboundMessage>, want_calls: usize) {
        // 通过 downcast 不便，这里改为直接调用 handle（绕过 run/recv）。
        for m in msgs {
            let disp = ctx.disp.clone();
            // 直接 await handle，串行执行（handle 内部已有 per-conv 锁）。
            disp.handle(m).await;
        }
        // 等待到 calls 计数达到预期。
        for _ in 0..400 {
            if ctx.order.load(Ordering::SeqCst) >= want_calls {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// /img：workdir 内图片经 send_media 回传；workdir 外拒绝；缺文件提示。
    #[tokio::test]
    async fn img_sends_rejects_and_reports() {
        let _serial = SERIAL.lock().await;
        let outer = std::env::temp_dir().join(format!("imagent-img-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outer);
        let wd = outer.join("ws");
        std::fs::create_dir_all(&wd).unwrap();
        let img = wd.join("a.png");
        std::fs::write(&img, b"png").unwrap();
        // workdir 外放一个真实文件，供逃逸拒绝分支（须存在才能过 canonicalize）。
        std::fs::write(outer.join("b.png"), b"png").unwrap();

        let ctx = build_with_workdir(Auth::new(vec!["alice".into()]), wd.clone()).await;
        feed_and_wait(
            &ctx,
            vec![
                msg("feishu:ou_t", "alice", "/img a.png"),
                msg("feishu:ou_t", "alice", "/img ../b.png"),
                msg("feishu:ou_t", "alice", "/img nope.png"),
            ],
            0,
        )
        .await;

        let inbox = ctx.inbox.lock().await.clone();
        // macOS 下 /var 是 /private/var 的 symlink，send_media 用 canonicalize 后的
        // 真实路径，断言同样 canonicalize 后比较。
        let img_real = img.canonicalize().unwrap();
        assert!(
            inbox
                .iter()
                .any(|m| *m == format!("[media:{}]", img_real.display())),
            "workdir 内图片应回传: {inbox:?}"
        );
        assert!(
            inbox.iter().any(|m| m.contains("不在当前工作目录")),
            "workdir 外应拒绝: {inbox:?}"
        );
        assert!(
            inbox.iter().any(|m| m.contains("文件不存在")),
            "缺文件应提示: {inbox:?}"
        );
        assert!(
            !inbox.iter().any(|m| m.contains("b.png]")),
            "workdir 外文件不应回传: {inbox:?}"
        );
        drop_db(ctx.db.clone()).await;
        let _ = std::fs::remove_dir_all(&outer);
    }

    async fn drop_db(p: std::path::PathBuf) {
        let _ = std::fs::remove_file(&p);
        let mut w = p.clone();
        w.set_extension("sqlite-wal");
        let _ = std::fs::remove_file(&w);
        let mut s = p.clone();
        s.set_extension("sqlite-shm");
        let _ = std::fs::remove_file(&s);
    }

    // ---------- tests ----------

    #[tokio::test]
    async fn normal_message_runs_backend_and_replies_and_persists() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        feed_and_wait(&ctx, vec![msg("c1", "alice", "hello")], 1).await;

        // 回传收到（Final 优先）。
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.starts_with("reply#")),
            "inbox={inbox:?}"
        );

        // session 落库且 id 正确。
        let row = ctx
            .check()
            .await
            .get_session("c1")
            .await
            .unwrap()
            .expect("session row");
        assert_eq!(row.session_id, "sess-0");
        assert_eq!(row.agent_kind, "mock-backend");
        drop_db(ctx.db).await;
    }

    /// 纯媒体消息且全部下载失败时，应向用户回真实错误而非静默丢弃。
    #[tokio::test]
    async fn pure_media_all_failed_replies_error() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        let m = InboundMessage {
            conv_id: ConvId("feishu:ou_t".into()),
            sender: UserId("alice".into()),
            text: None,
            media: vec![],
            media_errors: vec!["img_x: 下载失败: boom".into()],
            reply_hint: ReplyHint::None,
        };
        feed_and_wait(&ctx, vec![m], 0).await;

        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("⚠️") && t.contains("img_x")),
            "纯媒体全失败应回真实错误提示: {inbox:?}"
        );
        drop_db(ctx.db).await;
    }
    #[tokio::test]
    async fn non_terminal_outcome_prefixes_warning() {
        // R1：backend 返回 terminal=false（模拟 agent 崩溃），reply 应前置告警。
        let _serial = SERIAL.lock().await;
        let ctx = build_non_terminal(Auth::new(vec!["alice".into()])).await;
        feed_and_wait(&ctx, vec![msg("c9", "alice", "hello")], 1).await;

        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox
                .iter()
                .any(|t| t.starts_with("⚠️ agent 异常退出，以下为部分输出：\n\n")),
            "inbox={inbox:?}"
        );
        // 告警后仍应跟有 backend 的 Final 文本（reply#）。
        assert!(
            inbox.iter().any(|t| t.contains("reply#")),
            "inbox={inbox:?}"
        );
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn second_message_continues_previous_session() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        feed_and_wait(
            &ctx,
            vec![msg("c2", "alice", "first"), msg("c2", "alice", "second")],
            2,
        )
        .await;

        let calls = ctx.calls.lock().await.clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], None, "first should be new session");
        assert_eq!(calls[1].as_deref(), Some("sess-0"), "second should resume");
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn discovery_mode_skips_backend_but_replies_guide() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec![])).await; // 发现模式
        feed_and_wait(&ctx, vec![msg("c3", "anyone", "hi")], 0).await;

        // backend 未被调用。
        assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
        // 但回传了引导消息，且其中含 sender id。
        let inbox = ctx.inbox.lock().await.clone();
        assert_eq!(inbox.len(), 1, "应回一条引导，inbox={inbox:?}");
        assert!(inbox[0].contains("anyone"), "引导消息应含 sender id");
        assert!(inbox[0].contains("imagent allow"), "引导消息应含 CLI 指引");
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn non_allowlisted_sender_dropped() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["someone_else".into()])).await;
        feed_and_wait(&ctx, vec![msg("c4", "intruder", "hi")], 0).await;

        assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
        assert_eq!(ctx.send_count.load(Ordering::SeqCst), 0);
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn slash_new_resets_session() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 先发一条普通消息建立 session。
        feed_and_wait(&ctx, vec![msg("c5", "alice", "hello")], 1).await;
        assert!(ctx.check().await.get_session("c5").await.unwrap().is_some());

        // 发 /new：应删除 session 并回 IM。
        let before_sends = ctx.send_count.load(Ordering::SeqCst);
        feed_and_wait(&ctx, vec![msg("c5", "alice", "/new")], 1).await;
        // /new 不触发 backend，order 不变。
        assert_eq!(ctx.order.load(Ordering::SeqCst), 1);
        // session 已删除。
        assert!(ctx.check().await.get_session("c5").await.unwrap().is_none());
        // 回传了一条重置提示。
        let after_sends = ctx.send_count.load(Ordering::SeqCst);
        assert_eq!(after_sends, before_sends + 1);

        // 下一条普通消息 backend 收到的 session 是 None。
        feed_and_wait(&ctx, vec![msg("c5", "alice", "fresh start")], 2).await;
        let calls = ctx.calls.lock().await.clone();
        // 最后一次调用 session 应为 None（fresh start 新建）。
        assert_eq!(calls.last(), Some(&None));
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn unknown_slash_command_replies() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        let before = ctx.send_count.load(Ordering::SeqCst);
        feed_and_wait(&ctx, vec![msg("c6", "alice", "/foo bar")], 0).await;
        let after = ctx.send_count.load(Ordering::SeqCst);
        assert_eq!(after, before + 1);
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("未知命令") && t.contains("/foo")),
            "inbox={inbox:?}"
        );
        // backend 未被调用。
        assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn per_conv_serial_order() {
        // 同一 conv 连发三条；mock backend 内 fetch_add 顺序应递增。
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        feed_and_wait(
            &ctx,
            vec![
                msg("c7", "alice", "a"),
                msg("c7", "alice", "b"),
                msg("c7", "alice", "c"),
            ],
            3,
        )
        .await;

        let calls = ctx.calls.lock().await.clone();
        // 三条依次执行；session 链：None -> sess-0 -> sess-1。
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0], None);
        assert_eq!(calls[1].as_deref(), Some("sess-0"));
        assert_eq!(calls[2].as_deref(), Some("sess-1"));
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn allow_rejected_for_non_admin_when_admin_senders_set() {
        // P2-D：admin_senders 非空时，白名单内非 admin 用户 /allow 被拒绝。
        let ctx = build_with_admin(
            Auth::new(vec!["alice".into(), "bob".into()]),
            vec!["alice".into()],
        )
        .await;
        // bob（白名单但非 admin）尝试 /allow charlie → 应被拒绝。
        feed_and_wait(&ctx, vec![msg("c", "bob", "/allow charlie")], 1).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|m| m.contains("仅管理员")),
            "非 admin /allow 应被拒绝: {inbox:?}"
        );
        // charlie 未被授权。
        assert!(!ctx.disp.auth().is_allowed(&UserId("charlie".into())));
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn allow_command_grants_then_bob_can_drive() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;

        // alice 发 /allow bob：应回「已授权」。
        feed_and_wait(&ctx, vec![msg("c8", "alice", "/allow bob")], 0).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("已授权") && t.contains("bob")),
            "inbox={inbox:?}"
        );

        // 白名单持久化到 store。
        let stored = ctx.check().await.list_allowed_senders().await.unwrap();
        assert!(stored.iter().any(|s| s == "bob"), "stored={stored:?}");

        // bob（刚被授权）现在能驱动 backend。
        feed_and_wait(&ctx, vec![msg("c9", "bob", "hello")], 1).await;
        assert_eq!(ctx.order.load(Ordering::SeqCst), 1, "bob 应能驱动 backend");
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn list_command_replies_whitelist() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into(), "carol".into()])).await;
        feed_and_wait(&ctx, vec![msg("c10", "alice", "/list")], 0).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("alice") && t.contains("carol") && t.contains("白名单")),
            "inbox={inbox:?}"
        );
        // backend 未被调用。
        assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn whoami_command_replies_sender() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        feed_and_wait(&ctx, vec![msg("c11", "alice", "/whoami")], 0).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(inbox.iter().any(|t| t.contains("alice")), "inbox={inbox:?}");
        assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn disallow_cannot_revoke_self() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        feed_and_wait(&ctx, vec![msg("c12", "alice", "/disallow alice")], 0).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.contains("不允许撤销自己")),
            "inbox={inbox:?}"
        );
        // alice 仍在白名单。
        assert!(ctx.disp.auth.is_allowed(&UserId("alice".into())));
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn disallow_command_removes_target() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into(), "bob".into()])).await;
        feed_and_wait(&ctx, vec![msg("c13", "alice", "/disallow bob")], 0).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("已移除") && t.contains("bob")),
            "inbox={inbox:?}"
        );
        // bob 已被移除：后续消息被丢弃，不驱动 backend。
        feed_and_wait(&ctx, vec![msg("c14", "bob", "still here?")], 0).await;
        assert_eq!(ctx.order.load(Ordering::SeqCst), 0, "bob 应已被移出白名单");
        drop_db(ctx.db).await;
    }
    #[tokio::test]
    async fn switch_new_name_clears_session_and_sets_active() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 先建立默认 session。
        feed_and_wait(&ctx, vec![msg("s1", "alice", "hello")], 1).await;
        assert!(ctx.check().await.get_session("s1").await.unwrap().is_some());

        // /switch newtask（命名不存在）→ 活动清空 + active_name 设。
        feed_and_wait(&ctx, vec![msg("s1", "alice", "/switch newtask")], 1).await;
        assert!(
            ctx.check().await.get_session("s1").await.unwrap().is_none(),
            "switch 新命名后活动 session 应清空"
        );
        assert_eq!(
            ctx.check()
                .await
                .get_config("active_name:s1")
                .await
                .unwrap(),
            Some("newtask".to_string())
        );

        // 下一条普通消息：backend 收到 None（新建），并落 named_sessions(newtask)。
        feed_and_wait(&ctx, vec![msg("s1", "alice", "do work")], 2).await;
        let calls = ctx.calls.lock().await.clone();
        assert_eq!(calls.last(), Some(&None), "switch 后首条消息应新建 session");

        let nrow = ctx
            .check()
            .await
            .get_named_session("s1", "newtask")
            .await
            .unwrap()
            .expect("named row");
        assert_eq!(nrow.name, "newtask");
        // 活动 session 行 name 也带命名。
        let srow = ctx
            .check()
            .await
            .get_session("s1")
            .await
            .unwrap()
            .expect("session row");
        assert_eq!(srow.name.as_deref(), Some("newtask"));
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn switch_existing_name_resumes_named_session() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 建立命名 session `taskA`。
        feed_and_wait(&ctx, vec![msg("s2", "alice", "/switch taskA")], 0).await;
        feed_and_wait(&ctx, vec![msg("s2", "alice", "first taskA work")], 1).await;
        let nrow = ctx
            .check()
            .await
            .get_named_session("s2", "taskA")
            .await
            .unwrap()
            .expect("named row taskA");
        let task_a_sid = nrow.session_id.clone();

        // 切到默认（/new 清 active_name）再发消息建立另一个默认 session。
        feed_and_wait(&ctx, vec![msg("s2", "alice", "/new")], 1).await;
        feed_and_wait(&ctx, vec![msg("s2", "alice", "default work")], 2).await;

        // /switch taskA（命名已存在）→ 活动 session 被写成 taskA 的 session_id。
        feed_and_wait(&ctx, vec![msg("s2", "alice", "/switch taskA")], 2).await;
        let srow = ctx
            .check()
            .await
            .get_session("s2")
            .await
            .unwrap()
            .expect("session row");
        assert_eq!(
            srow.session_id, task_a_sid,
            "switch 已存在命名应 resume 其 session_id"
        );
        assert_eq!(srow.name.as_deref(), Some("taskA"));
        assert_eq!(
            ctx.check()
                .await
                .get_config("active_name:s2")
                .await
                .unwrap(),
            Some("taskA".to_string())
        );

        // 下一条普通消息应续接 taskA 的 session_id。
        feed_and_wait(&ctx, vec![msg("s2", "alice", "continue")], 3).await;
        let calls = ctx.calls.lock().await.clone();
        assert_eq!(
            calls.last(),
            Some(&Some(task_a_sid)),
            "switch 后续消息应续接命名 session"
        );
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn sessions_command_lists_named_with_active_mark() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 空时 /sessions。
        feed_and_wait(&ctx, vec![msg("s3", "alice", "/sessions")], 0).await;
        assert!(
            ctx.inbox
                .lock()
                .await
                .last()
                .unwrap()
                .contains("无命名会话"),
            "空命名时应提示无"
        );

        // 建两个命名 session。
        feed_and_wait(&ctx, vec![msg("s3", "alice", "/switch alpha")], 0).await;
        feed_and_wait(&ctx, vec![msg("s3", "alice", "a work")], 1).await;
        feed_and_wait(&ctx, vec![msg("s3", "alice", "/switch beta")], 1).await;
        feed_and_wait(&ctx, vec![msg("s3", "alice", "b work")], 2).await;

        // /sessions：应列出 alpha、beta，当前活动 beta 标 *。
        feed_and_wait(&ctx, vec![msg("s3", "alice", "/sessions")], 2).await;
        let inbox = ctx.inbox.lock().await.clone();
        let listing = inbox.last().unwrap();
        assert!(listing.contains("命名会话"), "listing={listing}");
        assert!(listing.contains("alpha"), "listing={listing}");
        assert!(listing.contains("beta"), "listing={listing}");
        // beta 为活动，应带 `*`；alpha 不应带。
        assert!(
            listing.contains("beta *"),
            "活动命名应带 *，listing={listing}"
        );
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn new_command_clears_active_name() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 建命名 session。
        feed_and_wait(&ctx, vec![msg("s4", "alice", "/switch build")], 0).await;
        feed_and_wait(&ctx, vec![msg("s4", "alice", "go")], 1).await;
        assert_eq!(
            ctx.check()
                .await
                .get_config("active_name:s4")
                .await
                .unwrap(),
            Some("build".to_string())
        );

        // /new 清活动 session 与 active_name。
        feed_and_wait(&ctx, vec![msg("s4", "alice", "/new")], 1).await;
        assert!(
            ctx.check()
                .await
                .get_config("active_name:s4")
                .await
                .unwrap()
                .is_none(),
            "/new 后 active_name 应被清除"
        );
        assert!(ctx.check().await.get_session("s4").await.unwrap().is_none());

        // 下一条普通消息：name 应为 None（默认未命名）。
        feed_and_wait(&ctx, vec![msg("s4", "alice", "fresh")], 2).await;
        let srow = ctx
            .check()
            .await
            .get_session("s4")
            .await
            .unwrap()
            .expect("row");
        assert!(srow.name.is_none(), "/new 后新 session 应未命名");
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn compact_with_active_session_generates_summary_and_resets() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 先建立活动 session。
        feed_and_wait(&ctx, vec![msg("k1", "alice", "hello")], 1).await;
        assert!(ctx.check().await.get_session("k1").await.unwrap().is_some());

        // /compact：应 resume 当前 session 生成摘要，order +1。
        feed_and_wait(&ctx, vec![msg("k1", "alice", "/compact")], 2).await;

        // backend 被调用，且 session 为 Some（resume）。
        let calls = ctx.calls.lock().await.clone();
        assert_eq!(calls.len(), 2, "calls={calls:?}");
        assert_eq!(
            calls[1].as_deref(),
            Some("sess-0"),
            "/compact 应 resume 当前 session"
        );

        // 回传含「摘要」字样。
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.contains("摘要")),
            "应回摘要提示，inbox={inbox:?}"
        );

        // 摘要已落库。
        assert_eq!(
            ctx.check()
                .await
                .get_config("compact_summary:k1")
                .await
                .unwrap(),
            Some("reply#1".to_string()),
            "摘要应为 Final chunk 文本"
        );

        // 活动 session 已删除。
        assert!(
            ctx.check().await.get_session("k1").await.unwrap().is_none(),
            "/compact 后活动 session 应删除"
        );
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn compact_without_active_session_replies_nothing() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 无活动 session 直接 /compact。
        feed_and_wait(&ctx, vec![msg("k2", "alice", "/compact")], 0).await;

        // backend 未被调用。
        assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
        // 回传「无活动会话」。
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.contains("无活动会话")),
            "inbox={inbox:?}"
        );
        // 摘要未落库。
        assert!(ctx
            .check()
            .await
            .get_config("compact_summary:k2")
            .await
            .unwrap()
            .is_none());
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn compact_summary_injected_once_for_new_session() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 预置摘要 + 无活动 session。
        ctx.check()
            .await
            .set_config("compact_summary:k3", "之前讨论了 X 与 Y")
            .await
            .unwrap();
        assert!(ctx.check().await.get_session("k3").await.unwrap().is_none());

        // 发普通消息（无 existing）→ 应注入摘要。
        feed_and_wait(&ctx, vec![msg("k3", "alice", "继续")], 1).await;
        let prompts = ctx.prompts.lock().await.clone();
        assert_eq!(prompts.len(), 1, "prompts={prompts:?}");
        assert!(
            prompts[0].contains("【前情摘要】"),
            "新建 session 应注入摘要，prompt={}",
            prompts[0]
        );
        assert!(
            prompts[0].ends_with("继续"),
            "原始 prompt 应在末尾，prompt={}",
            prompts[0]
        );

        // 摘要一次性注入后清除。
        assert!(
            ctx.check()
                .await
                .get_config("compact_summary:k3")
                .await
                .unwrap()
                .is_none(),
            "摘要应一次性清除"
        );

        // 再发一条（现已 existing）→ 不再注入。
        feed_and_wait(&ctx, vec![msg("k3", "alice", "more")], 2).await;
        let prompts = ctx.prompts.lock().await.clone();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[1], "more", "第二条不应含摘要");
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn compact_summary_not_injected_when_session_exists() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 先建立活动 session。
        feed_and_wait(&ctx, vec![msg("k4", "alice", "first")], 1).await;
        assert!(ctx.check().await.get_session("k4").await.unwrap().is_some());

        // 会话已存在后再预置摘要；下一条消息 existing=Some，不应注入。
        ctx.check()
            .await
            .set_config("compact_summary:k4", "遗留摘要")
            .await
            .unwrap();
        feed_and_wait(&ctx, vec![msg("k4", "alice", "second")], 2).await;

        let prompts = ctx.prompts.lock().await.clone();
        assert_eq!(
            prompts[1], "second",
            "existing 时不应误注入，prompts={prompts:?}"
        );
        // 摘要仍存在（未被消费）。
        assert_eq!(
            ctx.check()
                .await
                .get_config("compact_summary:k4")
                .await
                .unwrap(),
            Some("遗留摘要".to_string())
        );
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn normal_message_appends_tool_summary() {
        let _serial = SERIAL.lock().await;
        let tools = vec![
            ("Read".to_string(), r#"{"path":"/foo"}"#.to_string()),
            ("Edit".to_string(), r#"{"file":"/bar"}"#.to_string()),
        ];
        let ctx = build_with_tools(Auth::new(vec!["alice".into()]), tools).await;
        feed_and_wait(&ctx, vec![msg("t1", "alice", "do it")], 1).await;

        // 回传文本含工具摘要与工具名。
        let inbox = ctx.inbox.lock().await.clone();
        let reply = inbox
            .iter()
            .find(|t| t.starts_with("reply#"))
            .expect("应有 final reply");
        assert!(reply.contains("🔧 工具调用"), "应含工具摘要标题: {reply}");
        assert!(reply.contains("Read("), "应含 Read 工具: {reply}");
        assert!(reply.contains("Edit("), "应含 Edit 工具: {reply}");
        assert!(reply.contains("/foo"), "应含工具输入: {reply}");
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn tool_summary_truncates_after_five() {
        let _serial = SERIAL.lock().await;
        // 6 个工具 → 截断展示 5 个并标 …(+1)。
        let tools: Vec<(String, String)> = (0..6)
            .map(|i| (format!("Tool{i}"), format!(r#"{{"k":"{i}"}}"#)))
            .collect();
        let ctx = build_with_tools(Auth::new(vec!["alice".into()]), tools).await;
        feed_and_wait(&ctx, vec![msg("t2", "alice", "go")], 1).await;

        let inbox = ctx.inbox.lock().await.clone();
        let reply = inbox
            .iter()
            .find(|t| t.starts_with("reply#"))
            .expect("应有 final reply");
        assert!(reply.contains("…(+1)"), "6 个工具应标 …(+1): {reply}");
        assert!(reply.contains("Tool0"), "应含首个工具: {reply}");
        assert!(reply.contains("Tool4"), "应含第 5 个工具: {reply}");
        drop_db(ctx.db).await;
    }

    // ---------- P4：/stop、批处理合并、空闲看门狗 ----------

    #[test]
    fn permission_reply_candidate_classification() {
        // 非空普通文本可作审批回复；斜杠命令与空文本不消费（/stop 在等审批时
        // 也要可执行；纯媒体消息不误吞成 deny）。
        assert!(is_permission_reply_candidate("y"));
        assert!(is_permission_reply_candidate(" 可以 "));
        assert!(is_permission_reply_candidate("yes please"));
        assert!(!is_permission_reply_candidate("/stop"));
        assert!(!is_permission_reply_candidate("/new"));
        assert!(!is_permission_reply_candidate(""));
        assert!(!is_permission_reply_candidate("   "));
    }

    #[test]
    fn merge_batch_joins_and_concats() {
        let mut a = msg("c1", "u1", "first");
        a.media.push(MediaRef {
            kind: "image".into(),
            url: "/tmp/a.png".into(),
        });
        let mut b = msg("c1", "u2", "second");
        b.media.push(MediaRef {
            kind: "image".into(),
            url: "/tmp/b.png".into(),
        });
        b.media_errors.push("dl fail".into());
        let blank = msg("c1", "u3", "   "); // 空文本跳过，media 仍并入
        let mut m = merge_batch(vec![a, b, blank]);
        assert_eq!(m.text.as_deref(), Some("first\n\nsecond"));
        assert_eq!(m.sender.0, "u1", "sender 取首条");
        assert_eq!(m.media.len(), 2, "media 拼接");
        assert_eq!(m.media_errors, vec!["dl fail".to_string()]);
        m.media.clear(); // silence unusedASSIGN 风格告警（显式消费）
                         // 全空文本 + 纯媒体：text 为 None，media 保留。
        let mut media_only = msg("c1", "u1", "   ");
        media_only.media.push(MediaRef {
            kind: "image".into(),
            url: "/tmp/x.png".into(),
        });
        let merged = merge_batch(vec![media_only]);
        assert_eq!(merged.text, None);
        assert_eq!(merged.media.len(), 1);
    }

    /// P4-1：/stop 中断在飞任务——backend 被 abort（无 Final 回复）、在飞注册
    /// 清空、/stop 回确认。
    #[tokio::test]
    async fn stop_aborts_running_task() {
        let _serial = SERIAL.lock().await;
        let ctx = build_slow(
            Auth::new(vec!["alice".into()]),
            30_000,
            TaskBudgets {
                batch_window: Duration::from_millis(1),
                ..test_budgets()
            },
        )
        .await;
        let disp = ctx.disp.clone();
        let runner = tokio::spawn(async move {
            disp.handle(msg("c1", "alice", "slow task")).await;
        });
        assert!(
            wait_registered(&ctx, "c1").await,
            "在飞任务应已注册（join spawn 后）"
        );
        ctx.disp.handle(msg("c1", "alice", "/stop")).await;
        let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
        assert!(done.is_ok(), "被中断的 runner 应很快退出");
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.contains("已中断当前任务")),
            "应回中断确认: {inbox:?}"
        );
        assert!(
            !inbox.iter().any(|t| t.starts_with("reply#")),
            "中断后不应有 Final 回复: {inbox:?}"
        );
        assert!(ctx.disp.running.lock().await.is_empty(), "在飞注册应清空");
        assert_eq!(ctx.prompts.lock().await.len(), 1, "恰一轮被中断的执行");
        drop_db(ctx.db).await;
    }

    /// P4-1：无在飞任务时 /stop 友好回复（不报错）。
    #[tokio::test]
    async fn stop_without_running_task_replies() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        ctx.disp.handle(msg("c1", "alice", "/stop")).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.contains("没有运行中的任务")),
            "应回无任务提示: {inbox:?}"
        );
        drop_db(ctx.db).await;
    }

    /// P4-1/P4-2：/stop 同时丢弃排队待合并的消息，回复含丢弃条数。
    #[tokio::test]
    async fn stop_drops_queued_messages() {
        let _serial = SERIAL.lock().await;
        let ctx = build_slow(
            Auth::new(vec!["alice".into()]),
            30_000,
            TaskBudgets {
                batch_window: Duration::from_millis(1),
                ..test_budgets()
            },
        )
        .await;
        let disp = ctx.disp.clone();
        let runner = tokio::spawn(async move {
            disp.handle(msg("c1", "alice", "first round")).await;
        });
        assert!(wait_registered(&ctx, "c1").await, "在飞任务应已注册");
        ctx.disp.handle(msg("c1", "alice", "queued B")).await;
        ctx.disp.handle(msg("c1", "alice", "queued C")).await;
        // 等 2 条都入队。
        for _ in 0..400 {
            if ctx
                .disp
                .queues
                .lock()
                .await
                .get("c1")
                .is_some_and(|q| q.len() == 2)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        ctx.disp.handle(msg("c1", "alice", "/stop")).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), runner).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("已中断当前任务（丢弃 2 条排队消息）")),
            "应回丢弃条数: {inbox:?}"
        );
        let prompts = ctx.prompts.lock().await.clone();
        assert_eq!(
            prompts,
            vec!["first round".to_string()],
            "排队消息不应再执行"
        );
        drop_db(ctx.db).await;
    }

    /// P4-2：运行中到达的消息排队到下一轮，且合并为一轮（B、C 合成 "B\n\nC"）。
    #[tokio::test]
    async fn messages_during_run_merge_into_next_round() {
        let _serial = SERIAL.lock().await;
        let ctx = build_slow(
            Auth::new(vec!["alice".into()]),
            200,
            TaskBudgets {
                batch_window: Duration::from_millis(1),
                ..test_budgets()
            },
        )
        .await;
        let disp = ctx.disp.clone();
        let runner = tokio::spawn(async move {
            disp.handle(msg("c1", "alice", "round A")).await;
        });
        assert!(wait_registered(&ctx, "c1").await, "在飞任务应已注册");
        ctx.disp.handle(msg("c1", "alice", "msg B")).await;
        ctx.disp.handle(msg("c1", "alice", "msg C")).await;
        let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
        assert!(done.is_ok(), "runner 应在两轮后退出");
        let prompts = ctx.prompts.lock().await.clone();
        assert_eq!(
            prompts,
            vec!["round A".to_string(), "msg B\n\nmsg C".to_string()],
            "第二轮应为 B、C 合并后的单轮: {prompts:?}"
        );
        // 第二轮续接第一轮 session（批处理不破坏会话连续性）。
        let calls = ctx.calls.lock().await.clone();
        assert_eq!(
            calls,
            vec![None, Some("sess-0".to_string())],
            "第二轮应续接第一轮 session: {calls:?}"
        );
        drop_db(ctx.db).await;
    }

    /// P4-2：批处理窗口内连发的消息并入同一轮（而非各跑一轮）。
    #[tokio::test]
    async fn burst_messages_merge_within_window() {
        let _serial = SERIAL.lock().await;
        let ctx = build_slow(
            Auth::new(vec!["alice".into()]),
            50,
            TaskBudgets {
                batch_window: Duration::from_millis(200),
                ..test_budgets()
            },
        )
        .await;
        let disp = ctx.disp.clone();
        let runner = tokio::spawn(async move {
            disp.handle(msg("c1", "alice", "first half")).await;
        });
        // 窗口期内（200ms）并入第二条。
        tokio::time::sleep(Duration::from_millis(30)).await;
        ctx.disp.handle(msg("c1", "alice", "second half")).await;
        let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
        assert!(done.is_ok(), "runner 应退出");
        let prompts = ctx.prompts.lock().await.clone();
        assert_eq!(
            prompts,
            vec!["first half\n\nsecond half".to_string()],
            "窗口内连发应合并为单轮: {prompts:?}"
        );
        assert_eq!(ctx.order.load(Ordering::SeqCst), 1, "backend 只跑一轮");
        drop_db(ctx.db).await;
    }

    /// P4-3：空闲看门狗——agent 连续无输出超时终止本轮并告知用户。
    #[tokio::test]
    async fn idle_watchdog_terminates_silent_agent() {
        let _serial = SERIAL.lock().await;
        let ctx = build_slow(
            Auth::new(vec!["alice".into()]),
            30_000,
            TaskBudgets {
                agent_idle_timeout: Duration::from_millis(100),
                batch_window: Duration::from_millis(1),
                ..test_budgets()
            },
        )
        .await;
        // 直接 await：runner 循环内完成（窗口 → 一轮 → 看门狗 → 退出）。
        ctx.disp.handle(msg("c1", "alice", "hang please")).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("无输出") && t.contains("空闲超时")),
            "应回空闲超时提示: {inbox:?}"
        );
        assert!(!inbox.iter().any(|t| t.starts_with("reply#")));
        assert_eq!(ctx.prompts.lock().await.len(), 1);
        assert!(ctx.disp.running.lock().await.is_empty(), "在飞注册应清空");
        assert!(
            ctx.disp.queues.lock().await.is_empty(),
            "runner 退出后队列 entry 应移除"
        );
        drop_db(ctx.db).await;
    }

    /// P4-2：排队上限——超限消息回告警并丢弃，runner 不受影响。
    #[tokio::test]
    async fn pending_queue_cap_warns_and_drops() {
        let _serial = SERIAL.lock().await;
        let ctx = build_slow(
            Auth::new(vec!["alice".into()]),
            30_000,
            TaskBudgets {
                batch_window: Duration::from_millis(1),
                ..test_budgets()
            },
        )
        .await;
        let disp = ctx.disp.clone();
        let runner = tokio::spawn(async move {
            disp.handle(msg("c1", "alice", "long first round")).await;
        });
        assert!(wait_registered(&ctx, "c1").await, "在飞任务应已注册");
        for i in 0..(PENDING_QUEUE_CAP + 5) {
            ctx.disp
                .handle(msg("c1", "alice", &format!("spam {i}")))
                .await;
        }
        ctx.disp.handle(msg("c1", "alice", "/stop")).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), runner).await;
        let inbox = ctx.inbox.lock().await.clone();
        let overflow = inbox.iter().filter(|t| t.contains("已达上限")).count();
        assert!(overflow >= 5, "超限的 5 条应各回一次告警: {inbox:?}");
        let prompts = ctx.prompts.lock().await.clone();
        assert_eq!(prompts.len(), 1, "只应有首轮（已 /stop）: {prompts:?}");
        drop_db(ctx.db).await;
    }

    // ---------- P4-5：会话（群）白名单 ----------

    /// P4-5：未授权群 + 未授权 sender 丢弃；/chat allow 授权当前群后成员可驱动；
    /// /chat list 列出；/chat deny 收回。
    #[tokio::test]
    async fn chat_allowlist_gates_group_messages() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // bob 在未授权群发言：丢弃（无回复、不跑 backend）。
        ctx.disp.handle(msg("feishu:oc_g", "bob", "hi group")).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(inbox.is_empty(), "未授权群应静默丢弃: {inbox:?}");
        drop(inbox);
        // alice（白名单成员）在群里授权该群。
        ctx.disp
            .handle(msg("feishu:oc_g", "alice", "/chat allow"))
            .await;
        // 授权后 bob 的群消息驱动 agent。
        feed_and_wait(&ctx, vec![msg("feishu:oc_g", "bob", "run it")], 1).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.contains("已授权会话 feishu:oc_g")),
            "应回授权确认: {inbox:?}"
        );
        assert!(
            inbox.iter().any(|t| t.starts_with("reply#")),
            "授权后群成员消息应驱动 agent: {inbox:?}"
        );
        // /chat deny 收回后 bob 再发言被丢弃。
        ctx.disp
            .handle(msg("feishu:oc_g", "alice", "/chat deny"))
            .await;
        let before = ctx.calls.lock().await.len();
        ctx.disp.handle(msg("feishu:oc_g", "bob", "again")).await;
        assert_eq!(
            ctx.calls.lock().await.len(),
            before,
            "收回授权后群消息不应驱动 agent"
        );
        drop_db(ctx.db).await;
    }

    /// P4-5：/chat 非管理员（admin_senders 非空时）被拒。
    #[tokio::test]
    async fn chat_command_requires_admin_when_set() {
        let _serial = SERIAL.lock().await;
        let ctx = build_with_admin(Auth::new(vec!["alice".into()]), vec!["root".into()]).await;
        ctx.disp
            .handle(msg("feishu:oc_g", "alice", "/chat allow"))
            .await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.contains("仅管理员")),
            "非管理员应被拒: {inbox:?}"
        );
        drop_db(ctx.db).await;
    }

    /// P4-5：仅配置会话白名单（sender 空）不进发现模式，群成员可直接使用。
    #[tokio::test]
    async fn chats_only_config_not_discovery() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::with_chats(vec![], vec!["feishu:oc_g".into()])).await;
        feed_and_wait(&ctx, vec![msg("feishu:oc_g", "bob", "hello")], 1).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.starts_with("reply#")),
            "授权群的成员消息应驱动 agent: {inbox:?}"
        );
        drop_db(ctx.db).await;
    }

    // ---------- P4-6：/config 与 COT 档位 ----------

    /// P4-6：/config 查看显示全部键；设置 off 后工具摘要消失。
    #[tokio::test]
    async fn config_views_and_sets_cot_detail() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        ctx.disp.handle(msg("c1", "alice", "/config")).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("cot_detail = brief") && t.contains("batch_window_ms")),
            "应列出配置: {inbox:?}"
        );
        drop(inbox);
        // 切 detailed → 生效确认。
        ctx.disp
            .handle(msg("c1", "alice", "/config cot_detail detailed"))
            .await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(inbox.iter().any(|t| t.contains("✅ cot_detail = detailed")));
        drop(inbox);
        // 切 off + 发工具消息 → 无 🔧 摘要。
        ctx.disp
            .handle(msg("c1", "alice", "/config cot_detail off"))
            .await;
        let tools = vec![("Read".to_string(), r#"{"path":"/foo"}"#.to_string())];
        let (back, calls, prompts, order) = MockBackend::new_with_tools(tools).await;
        let _ = (back, calls, prompts); // 本测试复用 ctx 的 backend，仅验证 off 档行为
        let _ = order;
        // 用带工具的 backend 直接构造（build_with_tools 默认 brief；这里改共享句柄已 off）。
        drop_db(ctx.db).await;
        // 单独验证 off 档：用带工具 ctx + /config off。
        let ctx2 = build_with_tools(
            Auth::new(vec!["alice".into()]),
            vec![("Read".to_string(), r#"{"path":"/foo"}"#.to_string())],
        )
        .await;
        ctx2.disp
            .handle(msg("t1", "alice", "/config cot_detail off"))
            .await;
        feed_and_wait(&ctx2, vec![msg("t1", "alice", "go")], 1).await;
        let inbox2 = ctx2.inbox.lock().await.clone();
        let reply = inbox2
            .iter()
            .find(|t| t.starts_with("reply#"))
            .expect("应有 final reply");
        assert!(
            !reply.contains("🔧 工具调用"),
            "off 档不应有工具摘要: {reply}"
        );
        drop_db(ctx2.db).await;
    }

    /// P4-6：detailed 档展示更长输入（>40 字符的输入可见）。
    #[tokio::test]
    async fn cot_detailed_shows_longer_input() {
        let _serial = SERIAL.lock().await;
        let long_input = format!(r#"{{"path":"{}"}}"#, "x".repeat(80));
        let ctx = build_with_tools(
            Auth::new(vec!["alice".into()]),
            vec![("Read".to_string(), long_input.clone())],
        )
        .await;
        ctx.disp
            .handle(msg("t1", "alice", "/config cot_detail detailed"))
            .await;
        feed_and_wait(&ctx, vec![msg("t1", "alice", "go")], 1).await;
        let inbox = ctx.inbox.lock().await.clone();
        let reply = inbox
            .iter()
            .find(|t| t.starts_with("reply#"))
            .expect("应有 final reply");
        assert!(
            reply.contains("🔧 工具调用"),
            "detailed 档应有摘要: {reply}"
        );
        // brief 截断到 40 字符；detailed 到 200 → 80 个 x 应完整可见。
        assert!(
            reply.matches('x').count() >= 80,
            "detailed 档不应在 40 字符截断: {reply}"
        );
        drop_db(ctx.db).await;
    }

    // ---------- P4-7：/status /doctor /reconnect ----------

    #[tokio::test]
    async fn status_doctor_reconnect_reply() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        ctx.disp.handle(msg("c1", "alice", "/status")).await;
        ctx.disp.handle(msg("c1", "alice", "/doctor")).await;
        ctx.disp.handle(msg("c1", "alice", "/reconnect")).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("📊") && t.contains("mock / mock-backend")),
            "/status 应含平台/后端: {inbox:?}"
        );
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("🩺") && t.contains("存储读写正常")),
            "/doctor 应含自检结果: {inbox:?}"
        );
        // MockPlatform 未覆写 reconnect → 默认不支持，回告警而非成功。
        assert!(
            inbox.iter().any(|t| t.contains("重连指令失败")),
            "默认平台应报不支持重连: {inbox:?}"
        );
        drop_db(ctx.db).await;
    }

    // ---------- P4-8/P4-11：/resume 统一列表 ----------

    /// P4-8：两轮会话后 /resume 列出历史（当前带 *）；/resume <n> 恢复后下条消息
    /// 续接被恢复的 session。MockBackend 默认无本机会话 → 纯 📱 历史列表。
    #[tokio::test]
    async fn resume_lists_and_restores_history() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        feed_and_wait(
            &ctx,
            vec![msg("c1", "alice", "first"), msg("c1", "alice", "second")],
            2,
        )
        .await;
        // 历史应有两条（sess-0 / sess-1），当前是 sess-1。
        ctx.disp.handle(msg("c1", "alice", "/resume")).await;
        let inbox = ctx.inbox.lock().await.clone();
        let list = inbox
            .iter()
            .find(|t| t.contains("可恢复会话"))
            .expect("应列出可恢复会话");
        assert!(list.contains("sess-0"), "应含 sess-0: {list}");
        assert!(list.contains("sess-1"), "应含 sess-1: {list}");
        assert!(list.contains("*"), "当前会话应带 *: {list}");
        assert!(list.contains("📱"), "历史会话标 📱: {list}");
        drop(inbox);
        // 恢复 1 号（sess-0）→ 下条消息续接 sess-0。
        ctx.disp.handle(msg("c1", "alice", "/resume 1")).await;
        feed_and_wait(&ctx, vec![msg("c1", "alice", "after resume")], 3).await;
        let calls = ctx.calls.lock().await.clone();
        assert_eq!(
            calls.last(),
            Some(&Some("sess-0".to_string())),
            "恢复后应续接 sess-0: {calls:?}"
        );
        // 越界序号 → 提示重看列表。
        ctx.disp.handle(msg("c1", "alice", "/resume 99")).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.contains("序号无效")),
            "越界序号应提示: {inbox:?}"
        );
        drop_db(ctx.db).await;
    }

    /// P4-11：统一列表合并本机（💻）与 IM（📱）会话，按序号接管本机会话后
    /// 下条消息续接之，且回复带分叉提示。
    #[tokio::test]
    async fn resume_merges_local_and_takes_over_pc_session() {
        let _serial = SERIAL.lock().await;
        let now = now_secs();
        let ctx = build_with_local(
            Auth::new(vec!["alice".into()]),
            vec![LocalSession {
                session_id: "pc-9f86d081".to_string(),
                updated_at: now - 3_600,
                first_prompt: "修复流式卡片超时问题".to_string(),
                cwd: None,
            }],
        )
        .await;
        // 一轮 IM 会话（历史表 sess-0，updated_at=now，排在 💻 之前）。
        feed_and_wait(&ctx, vec![msg("c1", "alice", "im round")], 1).await;
        ctx.disp.handle(msg("c1", "alice", "/resume")).await;
        let inbox = ctx.inbox.lock().await.clone();
        let list = inbox
            .iter()
            .find(|t| t.contains("可恢复会话"))
            .expect("应列出可恢复会话");
        assert!(list.contains("💻"), "本机会话标 💻: {list}");
        assert!(list.contains("📱"), "IM 会话标 📱: {list}");
        assert!(
            list.contains("修复流式卡片超时问题"),
            "本机会话摘要应展示: {list}"
        );
        assert!(list.contains("sess-0…"), "IM 历史行缺摘要回退 id: {list}");
        // 列表序：sess-0（新）在前，pc-9f86d081 第 2。
        let l1 = list.lines().find(|l| l.starts_with("1.")).unwrap();
        let l2 = list.lines().find(|l| l.starts_with("2.")).unwrap();
        assert!(
            l1.contains("📱") && l1.contains("sess-0"),
            "第 1 应为 IM 当前: {l1}"
        );
        assert!(
            l2.contains("💻") && l2.contains("修复"),
            "第 2 应为本机: {l2}"
        );
        drop(inbox);
        // /resume 2 接管本机会话：确认 + 分叉提示。
        ctx.disp.handle(msg("c1", "alice", "/resume 2")).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.contains("已接管会话 pc-9f86d081")),
            "应回接管确认: {inbox:?}"
        );
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("来自电脑端") && t.contains("分叉")),
            "本机会话应附分叉提示: {inbox:?}"
        );
        // 下条消息续接被接管的本机会话。
        feed_and_wait(&ctx, vec![msg("c1", "alice", "continue on pc")], 2).await;
        let calls = ctx.calls.lock().await.clone();
        assert_eq!(
            calls.last(),
            Some(&Some("pc-9f86d081".to_string())),
            "接管后续接本机会话: {calls:?}"
        );
        drop_db(ctx.db).await;
    }

    /// P4-11：序号选择依赖先列过表（缓存）；未列直接选序号 → 引导先看列表。
    #[tokio::test]
    async fn resume_numeric_without_listing_prompts_list() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        ctx.disp.handle(msg("c1", "alice", "/resume 1")).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("序号无效") && t.contains("/resume")),
            "未列过表应引导先看列表: {inbox:?}"
        );
        drop_db(ctx.db).await;
    }

    // ---------- P5 第一批：安全 + 中断续接 ----------

    /// P5-1：审批回复候选消息的发送者须过白名单才可被路由消费——审批路由发生在
    /// handle() 鉴权之前，不过门则群聊里非白名单成员发 "y" 即可批准权限请求。
    #[tokio::test]
    async fn permission_reply_gate_checks_sender() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::with_chats(
            vec!["alice".into()],
            vec!["c-group".into()],
        ))
        .await;
        // 白名单 sender：可路由。
        assert!(ctx
            .disp
            .can_route_permission_reply(&msg("c1", "alice", "y")));
        // 非白名单 sender 且会话未授权：不得消费。
        assert!(
            !ctx.disp.can_route_permission_reply(&msg("c1", "bob", "y")),
            "非白名单 sender 的审批回复不得被路由"
        );
        // 会话（群）白名单放行：群内成员可路由（与 handle() 的鉴权门一致）。
        assert!(ctx
            .disp
            .can_route_permission_reply(&msg("c-group", "stranger", "y")));
        drop_db(ctx.db).await;
    }

    /// P5-2：/perm 修改权限模式须管理员；查看（只读）不受限。
    #[tokio::test]
    async fn perm_switch_requires_admin() {
        let _serial = SERIAL.lock().await;
        let ctx = build_with_admin(
            Auth::new(vec!["alice".into(), "bob".into()]),
            vec!["alice".into()],
        )
        .await;
        // 非管理员切换 → 拒绝且模式不变。
        ctx.disp.handle(msg("c1", "bob", "/perm allow")).await;
        assert!(
            !matches!(*ctx.disp.permission_mode.read(), PermissionMode::Allow),
            "非管理员不得切换权限模式"
        );
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.contains("仅管理员")),
            "应回拒绝提示: {inbox:?}"
        );
        drop(inbox);
        // 查看（只读）不受限。
        ctx.disp.handle(msg("c1", "bob", "/perm")).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.contains("当前权限模式")),
            "查看模式应放行: {inbox:?}"
        );
        drop(inbox);
        // 管理员切换成功。
        ctx.disp.handle(msg("c1", "alice", "/perm allow")).await;
        assert!(matches!(
            *ctx.disp.permission_mode.read(),
            PermissionMode::Allow
        ));
        drop_db(ctx.db).await;
    }

    /// P5-3：/disallow 须管理员——此前任何过门用户可把管理员本人踢出白名单。
    #[tokio::test]
    async fn disallow_requires_admin() {
        let _serial = SERIAL.lock().await;
        let ctx = build_with_admin(
            Auth::new(vec!["alice".into(), "bob".into(), "carol".into()]),
            vec!["alice".into()],
        )
        .await;
        // 非管理员 bob 踢 carol → 拒绝，carol 仍在白名单。
        ctx.disp.handle(msg("c1", "bob", "/disallow carol")).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.contains("仅管理员")),
            "应回拒绝提示: {inbox:?}"
        );
        drop(inbox);
        assert!(
            ctx.disp.auth.is_allowed(&UserId("carol".into())),
            "carol 应仍在白名单"
        );
        // 管理员 alice 撤销成功。
        ctx.disp.handle(msg("c1", "alice", "/disallow carol")).await;
        assert!(
            !ctx.disp.auth.is_allowed(&UserId("carol".into())),
            "carol 应已被移除"
        );
        drop_db(ctx.db).await;
    }

    /// P5-5：首轮任务被 /stop 中断，但 backend 已 announce 的 session id 应落库——
    /// 下条消息续接该会话，而非静默开新会话（"失忆"）。
    #[tokio::test]
    async fn stop_persists_learned_session() {
        let _serial = SERIAL.lock().await;
        let ctx = build_slow_with_session(
            Auth::new(vec!["alice".into()]),
            60_000,
            "sess-learned",
            test_budgets(),
        )
        .await;
        // 首条消息起飞（backend 记录后即挂起，但已 announce sess-learned）。
        // handle 内联等整轮，慢后端须 spawn 驱动（同 stop_aborts_running_task）。
        let disp = ctx.disp.clone();
        let runner = tokio::spawn(async move {
            disp.handle(msg("c1", "alice", "first")).await;
        });
        assert!(wait_registered(&ctx, "c1").await, "任务应注册在飞");
        // /stop 中断。
        ctx.disp.handle(msg("c1", "alice", "/stop")).await;
        let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
        assert!(done.is_ok(), "被中断的 runner 应很快退出");
        // persist 在轮次结束（running 移除）之前完成，此处应已观察到。
        assert!(
            !ctx.disp.running.lock().await.contains_key("c1"),
            "在飞注册应清空"
        );
        // 下条消息：应续接学到的 sess-learned（而非 None 开新会话）。
        let disp = ctx.disp.clone();
        let runner2 = tokio::spawn(async move {
            disp.handle(msg("c1", "alice", "after stop")).await;
        });
        for _ in 0..400 {
            if ctx.calls.lock().await.len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let calls = ctx.calls.lock().await.clone();
        assert_eq!(
            calls.last(),
            Some(&Some("sess-learned".to_string())),
            "中断后下条消息应续接学到的 session: {calls:?}"
        );
        // 收尾：中断第二个在飞任务再关库。
        ctx.disp.handle(msg("c1", "alice", "/stop")).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), runner2).await;
        drop_db(ctx.db).await;
    }

    /// P5-10：非卡片平台流式 Text 已实时推送——最终回复只补差量，不整段重发
    /// （codex/gemini/ACP 的「中间 Text + Final 全量」语义此前会推两遍）。
    #[tokio::test]
    async fn streamed_text_not_duplicated_on_plain_platform() {
        let _serial = SERIAL.lock().await;
        let ctx = build_streaming(
            Auth::new(vec!["alice".into()]),
            vec!["答案第一段。".to_string(), "答案第二段。".to_string()],
        )
        .await;
        feed_and_wait(&ctx, vec![msg("c1", "alice", "问题")], 1).await;
        let inbox = ctx.inbox.lock().await.clone();
        // 两段流式文本都应实时推送。
        assert!(
            inbox.iter().any(|t| t == "答案第一段。"),
            "应实时推送第一段: {inbox:?}"
        );
        assert!(
            inbox.iter().any(|t| t == "答案第二段。"),
            "应实时推送第二段: {inbox:?}"
        );
        // 全量文本不应作为最终回复再发一遍。
        let dup = inbox.iter().filter(|t| t.contains("答案第一段。")).count();
        assert_eq!(dup, 1, "Final 全量不应重发: {inbox:?}");
        drop_db(ctx.db).await;
    }

    /// P5-15：本机会话 cwd 与当前 workdir 不符时拒绝接管（防目录编码冲突串项目）。
    #[tokio::test]
    async fn resume_rejects_local_session_cwd_mismatch() {
        let _serial = SERIAL.lock().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let ctx = build_with_local(
            Auth::new(vec!["alice".into()]),
            vec![LocalSession {
                session_id: "pc-other".to_string(),
                updated_at: now,
                first_prompt: "别的项目".to_string(),
                cwd: Some("/other/project".to_string()),
            }],
        )
        .await;
        ctx.disp.handle(msg("c1", "alice", "/resume")).await;
        ctx.disp.handle(msg("c1", "alice", "/resume 1")).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("属于其它目录") && t.contains("/cd")),
            "cwd 不符应拒绝接管并引导 /cd: {inbox:?}"
        );
        // 未接管：session 映射不应变化。
        assert!(ctx.disp.running.lock().await.is_empty(), "无在飞任务");
        drop_db(ctx.db).await;
    }

    /// P5-9b：权限 socket 握手 token——错 token 连接被丢弃（无询问无回复）；
    /// 正确 token 的请求触发 IM 询问，cancel 立即唤醒回 deny（P5-16）。
    #[cfg(unix)]
    #[tokio::test]
    async fn permission_socket_token_handshake() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        let dir = std::env::temp_dir().join(format!("imagent-sock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("permission.sock");
        ctx.disp
            .spawn_socket_accept(sock.to_string_lossy().into_owned());
        // 等 socket 与 token 文件就绪。
        let token_path = dir.join("permission.token");
        for _ in 0..400 {
            if sock.exists() && token_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let token = std::fs::read_to_string(&token_path)
            .unwrap()
            .trim()
            .to_string();
        assert!(!token.is_empty(), "token 应已生成");

        // 错 token：连接被丢弃（不应有询问，也不应有任何回复）。
        {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let mut s = tokio::net::UnixStream::connect(&sock).await.unwrap();
            s.write_all(b"wrong-token\n{\"conv_id\":\"c1\"}\n")
                .await
                .unwrap();
            s.flush().await.unwrap();
            let _ = s.shutdown().await;
            let mut buf = String::new();
            let mut r = tokio::io::BufReader::new(s);
            let n = tokio::time::timeout(Duration::from_millis(300), r.read_line(&mut buf))
                .await
                .unwrap_or(Ok(0))
                .unwrap_or(0);
            assert_eq!(n, 0, "错 token 不应有任何回复: {buf}");
        }

        // 正确 token + 请求 → IM 询问送达；cancel 立即（而非 300s 后）回 deny。
        {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let mut s = tokio::net::UnixStream::connect(&sock).await.unwrap();
            s.write_all(format!("{token}\n").as_bytes()).await.unwrap();
            s.write_all(
                b"{\"conv_id\":\"c1\",\"tool_name\":\"Bash\",\"input\":{\"cmd\":\"ls\"}}\n",
            )
            .await
            .unwrap();
            s.flush().await.unwrap();
            let mut asked = false;
            for _ in 0..400 {
                if ctx
                    .inbox
                    .lock()
                    .await
                    .iter()
                    .any(|t| t.contains("请求执行"))
                {
                    asked = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(asked, "正确 token 的请求应触发 IM 询问");
            ctx.disp.router.cancel("c1").await;
            let mut buf = String::new();
            let mut r = tokio::io::BufReader::new(s);
            let _ = tokio::time::timeout(Duration::from_secs(2), r.read_line(&mut buf)).await;
            assert!(buf.contains("\"allow\":false"), "cancel 应回 deny: {buf}");
        }
        let _ = std::fs::remove_dir_all(&dir);
        drop_db(ctx.db).await;
    }
}

#[cfg(all(test, unix))]
mod permission_socket_tests {
    use super::peer_uid;

    #[tokio::test]
    async fn peer_uid_returns_self_for_local_pair() {
        // socketpair 两端同进程，peer_uid 必须返回本进程 uid。
        let (a, b) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        // from_std 要求非阻塞 socket（tokio issue #7172）。
        a.set_nonblocking(true).expect("set_nonblocking");
        let ta = tokio::net::UnixStream::from_std(a).expect("from_std");
        let got = peer_uid(&ta).expect("peer_uid 对本地连接应返回 Some");
        let self_uid = super::current_uid();
        assert_eq!(got, self_uid);
        drop(ta);
        drop(b);
    }
}
