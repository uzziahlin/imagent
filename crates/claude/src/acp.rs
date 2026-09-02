//! [`AcpBackend`]：基于 `claude-agent-acp` 长驻子进程的 agent 执行器（ACP/JSON-RPC）。
//!
//! 与 [`crate::ClaudeBackend`]（CLI 模式 `claude -p`）同 `impl Backend`，内部换用 ACP
//! 协议：imagent 作 Client，spawn `claude-agent-acp` 作 Agent 子进程，通过 stdin/stdout
//! 的 JSON-RPC 通信。
//!
//! ## 协议映射
//!
//! - `initialize` → 协商版本/能力（crate 内部 transport 自动驱动）。
//! - `session/new`（`session=None`）/ `session/load`（`session=Some`）→ 拿到/续接
//!   `sessionId`，`cwd` 锁定为调用方传入的 `workdir`（绝对路径）。
//! - `session/prompt` → 触发一个 turn，期间 Agent 通过 `session/update` 通知流式推送
//!   文本/工具调用/工具结果，本 backend 转成 [`AgentChunk`] 推入 chunks 通道。
//! - `session/request_permission` → Agent 反向调用 Client 请求工具权限，本 backend 按
//!   [`PermissionMode`] 自动响应；Ask 档经注入的 [`ImPermissionHook`]（B3）把审批卡
//!   发进 IM 等用户 y/n，超时 deny（与 claude-cli 的 MCP 闭环同一 PermissionRouter
//!   通道）。
//!
//! ## 连接模型（B2 / roadmap P5-14：per-conv 长驻连接）
//!
//! 每个 conv（IM 会话）一条独立长驻连接（spawn 一个 `claude-agent-acp` 子进程），
//! 惰性建立（首次 run 时）、空闲 [`CONN_IDLE_RECYCLE`] 回收、并发上限
//! [`MAX_CONCURRENT_CONNS`]（超限拒绝并回可读错误）。单会话的超时 cancel /
//! LoadSession 失败只杀**该会话的**连接（旧实现全局单连接 + 串行主循环，A 的长任务
//! 让 B 排队烧 agent_timeout，A 的 cancel 殃及所有会话——P5-14）。子进程由 SDK 的
//! ChildGuard 在 connection drop 时 kill（无泄漏）。
//!
//! [`AgentChunk`]: imagent_core::AgentChunk

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, LoadSessionRequest, NewSessionRequest, PermissionOption,
    PermissionOptionId, PermissionOptionKind, PlanEntryStatus, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{AcpAgent, Client, ConnectionTo};
use async_trait::async_trait;
use imagent_core::{
    AgentChunk, Backend, CoreError, ImPermissionAsk, ImPermissionHook, LocalSession,
    PermissionCapability, PermissionMode, Result, RunOutcome, SessionId, TodoItem, TodoStatus,
    UsageStats,
};
use parking_lot::RwLock;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// ACP backend 的固定名称。
const NAME: &str = "claude-acp";

/// 默认 spawn 的 agent 命令（PATH 可见的 `claude-agent-acp`）。
const DEFAULT_AGENT_CMD: &str = "claude-agent-acp";

/// B2/P5-14：per-conv 连接的并发上限（= 同时存活的 claude-agent-acp 子进程数）。
/// 超限直接拒绝（回可读错误）而非排队——排队会把排队时长烧进 agent_timeout
/// （正是 P5-14 要修的问题），拒绝让用户侧显式重试/减少并发。
const MAX_CONCURRENT_CONNS: usize = 8;

/// B2/P5-14：连接空闲回收时长：该 conv 的连接在此窗口内没有任何新 prompt，则断开
/// 连接（子进程由 ChildGuard kill）、从 map 移除。防低频会话长期占用子进程名额
/// （泄漏回收保底；shutdown 时 [`AcpBackend::shutdown`] 全量清理）。
const CONN_IDLE_RECYCLE: std::time::Duration = std::time::Duration::from_secs(600);

/// `claude-agent-acp` 长驻子进程 Backend（ACP/JSON-RPC）。
///
/// 持有共享的 [`PermissionMode`] 句柄（与 [`crate::ClaudeBackend`] 一致，支持 SIGHUP
/// 热重载）。连接按 conv 惰性建立、长驻复用（见模块级「连接模型」）。
pub struct AcpBackend {
    permission_mode: Arc<RwLock<PermissionMode>>,
    /// B3：dispatcher 注入的 IM 审批闭环回调（run() 启动时注入一次；None =
    /// 未注入（独立使用/测试），Ask 档 fail-closed）。
    hook: RwLock<Option<ImPermissionHook>>,
    /// W2-4：运行时模型（`/model` 热设）——spawn 子进程时以 `ANTHROPIC_MODEL`
    /// 前导 env 注入（`AcpAgent::from_str` 支持 NAME=value 前导语法）。
    model: RwLock<Option<String>>,
    /// B2/P5-14：conv_id → 长驻连接 map（惰性建立；task 退出时自摘除）。
    conns: Arc<Mutex<HashMap<String, Arc<LongLivedAcp>>>>,
    /// 并发连接上限（默认 [`MAX_CONCURRENT_CONNS`]；`with_conn_limits` 可配）。
    /// W2-4：经 main 从 config（`acp_max_connections`）注入。
    max_conns: usize,
    /// 连接空闲回收时长（默认 [`CONN_IDLE_RECYCLE`]；`with_conn_limits` 可配）。
    /// W2-4：经 main 从 config（`acp_idle_recycle_secs`）注入。
    conn_idle_recycle: std::time::Duration,
    /// 测试专用：mock transport 工厂（in-process 假 agent，替代 spawn 子进程）。
    #[cfg(test)]
    mock_factory: Option<Arc<dyn Fn() -> agent_client_protocol::Channel + Send + Sync>>,
}

impl AcpBackend {
    /// 默认构造（`PermissionMode::Off`，等同 CLI 的 Off 行为）。
    pub fn new() -> Self {
        Self {
            permission_mode: Arc::new(RwLock::new(PermissionMode::Off)),
            hook: RwLock::new(None),
            model: RwLock::new(None),
            conns: Arc::new(Mutex::new(HashMap::new())),
            max_conns: MAX_CONCURRENT_CONNS,
            conn_idle_recycle: CONN_IDLE_RECYCLE,
            #[cfg(test)]
            mock_factory: None,
        }
    }

    /// builder 风格配置连接上限/空闲回收时长（默认 [`MAX_CONCURRENT_CONNS`] /
    /// [`CONN_IDLE_RECYCLE`]）。core 构造方不动，后续接 config 后由此注入。
    pub fn with_conn_limits(
        mut self,
        max_conns: usize,
        conn_idle_recycle: std::time::Duration,
    ) -> Self {
        self.max_conns = max_conns;
        self.conn_idle_recycle = conn_idle_recycle;
        self
    }

    /// 用指定权限模式构造。
    pub fn with_permission_mode(mode: PermissionMode) -> Self {
        Self {
            permission_mode: Arc::new(RwLock::new(mode)),
            hook: RwLock::new(None),
            model: RwLock::new(None),
            conns: Arc::new(Mutex::new(HashMap::new())),
            max_conns: MAX_CONCURRENT_CONNS,
            conn_idle_recycle: CONN_IDLE_RECYCLE,
            #[cfg(test)]
            mock_factory: None,
        }
    }

    /// 用外部共享句柄构造——与 `Dispatcher` 共享同一 `Arc<RwLock<PermissionMode>>`，
    /// 使 SIGHUP 热重载对 backend 即时生效（每次 `run` 取最新值）。
    pub fn with_permission_mode_shared(mode: Arc<RwLock<PermissionMode>>) -> Self {
        Self {
            permission_mode: mode,
            hook: RwLock::new(None),
            model: RwLock::new(None),
            conns: Arc::new(Mutex::new(HashMap::new())),
            max_conns: MAX_CONCURRENT_CONNS,
            conn_idle_recycle: CONN_IDLE_RECYCLE,
            #[cfg(test)]
            mock_factory: None,
        }
    }

    /// 测试专用：注入 mock transport 工厂（每个新建连接调用一次）。
    #[cfg(test)]
    fn with_mock_factory(
        mut self,
        f: Arc<dyn Fn() -> agent_client_protocol::Channel + Send + Sync>,
    ) -> Self {
        self.mock_factory = Some(f);
        self
    }

    /// 解析要 spawn 的 agent 命令字符串。
    ///
    /// 优先取环境变量 `IMAGENT_ACP_COMMAND`（便于切版本/加参数），否则用
    /// [`DEFAULT_AGENT_CMD`]。支持 shell 风格拆分（由 crate 的 `AcpAgent::from_str`
    /// 处理）。
    fn agent_command() -> String {
        std::env::var("IMAGENT_ACP_COMMAND").unwrap_or_else(|_| DEFAULT_AGENT_CMD.into())
    }

    /// B2/P5-14：取（或启动）该 conv 的长驻连接。存活（prompt_tx 未关闭）则复用；
    /// 僵尸条目顺手清理；新建受 [`MAX_CONCURRENT_CONNS`] 上限约束（超限拒绝）。
    async fn long_lived(&self, conv: &str) -> Result<Arc<LongLivedAcp>> {
        let mut g = self.conns.lock().await;
        // 顺手清理僵尸条目（task 已退出的连接），让上限计数只算活连接。
        g.retain(|_, v| !v.prompt_tx.is_closed());
        if let Some(ll) = g.get(conv) {
            return Ok(ll.clone());
        }
        if g.len() >= self.max_conns {
            return Err(CoreError::Backend(
                NAME,
                format!(
                    "ACP 并发连接已达上限 {}（每会话一条长驻子进程连接），\
                     本会话请求被拒绝；请减少并发会话，或等待空闲连接回收（约 {} 分钟）\
                     后重试",
                    self.max_conns,
                    self.conn_idle_recycle.as_secs() / 60
                ),
            ));
        }
        let hook = self.hook.read().clone();
        let idle = self.conn_idle_recycle;
        let ll = match self.spawn_transport().await? {
            Transport::Real(agent) => LongLivedAcp::spawn(
                agent,
                Arc::clone(&self.permission_mode),
                hook,
                conv.to_string(),
                Arc::clone(&self.conns),
                idle,
            )?,
            #[cfg(test)]
            Transport::Mock(ch) => LongLivedAcp::spawn(
                ch,
                Arc::clone(&self.permission_mode),
                hook,
                conv.to_string(),
                Arc::clone(&self.conns),
                idle,
            )?,
        };
        g.insert(conv.to_string(), ll.clone());
        Ok(ll)
    }

    /// 防御性清理：若 map 中该 conv 的条目与 `ll` 是**同一连接**（same_channel），
    /// 则显式移除。正常情况下 task 退出时会自摘除，且 insert 与 task spawn 同在
    /// `long_lived` 的锁临界区内（自摘除必然发生在 insert 之后），「task 先退出、
    /// insert 后到」的窗口理论上不存在——但自摘除依赖 task 侧逻辑跑到收尾，此方法
    /// 供 run 的 send-Err 重试路径兜底：确证旧连接已死时立刻让出名额，不等下一轮
    /// `long_lived` 的 retain。
    async fn remove_conn_if_same(&self, conv: &str, ll: &Arc<LongLivedAcp>) {
        let mut g = self.conns.lock().await;
        if g.get(conv)
            .is_some_and(|cur| cur.prompt_tx.same_channel(&ll.prompt_tx))
        {
            g.remove(conv);
        }
    }

    /// 建立 transport：真机走 `AcpAgent`（spawn claude-agent-acp 子进程），测试可注入。
    ///
    /// M2（code-review v8，已评估未修）：SDK 内部读子进程 stdout 用无上限
    /// `lines()`、stderr 连接生命周期内无上限累积——CLI 路径的单行 8MB/stderr
    /// 64KB 双层上限在 ACP 不生效（「cat 大文件」类超长流 OOM 面）。SDK 无
    /// 传输层注入点（`SchemaMcpServer` 是命令 spec），修复需 (a) 中继进程封装
    /// 或 (b) 上游 PR——记 follow-up（docs/CODE_REVIEW_v8 §M2），当前接受该
    /// 限制（ACP 非主链路，claude-cli 已有完整上限）。
    /// in-process mock（见 [`Self::with_mock_factory`]）。W2-4：设置了运行时模型时，
    /// 命令串前导注入 `ANTHROPIC_MODEL=<model>` env（`AcpAgent::from_str` 的
    /// NAME=value 前导语法；子进程内 claude CLI 读取该 env）。
    async fn spawn_transport(&self) -> Result<Transport> {
        #[cfg(test)]
        if let Some(f) = &self.mock_factory {
            return Ok(Transport::Mock(f()));
        }
        let cmd = Self::sanitized_agent_command(self.model.read().clone());
        let agent = AcpAgent::from_str(&cmd)
            .map_err(|e| CoreError::Backend(NAME, format!("解析 agent 命令失败: {e}")))?;
        Ok(Transport::Real(agent))
    }

    /// H2（code-review v8）：S-2 env 消毒对齐——SDK 的 `spawn_process` 只做
    /// `cmd.env()` 增量、**无 env_clear**，ACP 子进程（及它内部再 spawn 的
    /// claude CLI）会继承部署环境全部变量（DATABASE_URL / CI secret / 其它
    /// token），可经 `Bash env` 或 `/proc/self/environ` 读走回传 IM。
    ///
    /// 修法：命令前导 `/usr/bin/env -i NAME=value …`——`env` 作为被 exec 程序
    /// （SDK `from_str` 走 shell_words::split 后直接 exec argv），`-i` 清空环境
    /// 后仅注入白名单（对齐 CLI 路径 [`imagent_core::backend_common::ALWAYS_PASSTHROUGH_ENV`]
    /// 以及 claude 凭据两键）。值含空白/引号/元字符时跳过该键（赋值经 shell_words
    /// 再切分会破形；白名单键的常规值——路径/键/locale——均无此类字符）。
    /// 自定义 `IMAGENT_ACP_COMMAND` 同样经此消毒（需要额外 env 的场景可在命令
    /// 里自带 `env NAME=value` 前缀）。
    fn sanitized_agent_command(model: Option<String>) -> String {
        let base = Self::agent_command();
        let mut assignments: Vec<String> = Vec::new();
        let safe = |v: &str| {
            !v.is_empty()
                && v.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._/:=+-@[]".contains(c))
        };
        for key in [
            "PATH",
            "HOME",
            "USER",
            "LOGNAME",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "TZ",
            "TMPDIR",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
        ] {
            match std::env::var(key) {
                Ok(v) if safe(&v) => assignments.push(format!("{key}={v}")),
                Ok(_v) => tracing::warn!(
                    target: "claude-acp",
                    key,
                    "ACP env 白名单值含非常规字符，跳过注入"
                ),
                Err(_) => {}
            }
        }
        if let Some(m) = model {
            if safe(&m) {
                assignments.push(format!("ANTHROPIC_MODEL={m}"));
            }
        }
        format!("/usr/bin/env -i {} {base}", assignments.join(" "))
    }

    /// B2：shutdown 全量清理——断开全部 per-conv 连接（map 清空后最后一个 sender
    /// drop → 长驻 task 的 recv 返回 None → connect_with 闭包返回 → connection
    /// drop → SDK ChildGuard kill 子进程）。独立部署（非 dispatcher 注入）时由
    /// 持有方调用；进程退出路径由空闲回收 + OS 清理兜底。
    pub async fn shutdown(&self) {
        let conns: Vec<Arc<LongLivedAcp>> =
            self.conns.lock().await.drain().map(|(_, v)| v).collect();
        drop(conns); // 释放 map 内的 sender 克隆（task 侧 rx 随之关闭）
        info!(target: "claude-acp", "ACP 连接已全量清理（shutdown）");
    }
}

/// Drop 时兜底清理：[`AcpBackend::shutdown`] 尚未被 main/core 侧接线调用（不能改
/// 其他 crate），最后持有方 drop backend 时在此释放全部连接条目——map 内的
/// sender 克隆 drop 后，长驻 task 的 recv 返回 None / 空闲回收超时退出 →
/// connection drop → SDK ChildGuard kill 子进程。
///
/// Drop 不是 async：用 `try_lock` 非阻塞拿 map（拿不到说明并发 run 正持锁，
/// 记 warn 放弃——空闲回收 + OS 清理兜底；绝不阻塞/嵌套 runtime）。
impl Drop for AcpBackend {
    fn drop(&mut self) {
        match self.conns.try_lock() {
            Ok(mut g) => {
                let n = g.len();
                g.clear();
                if n > 0 {
                    info!(target: "claude-acp", n, "AcpBackend drop：释放 {n} 条 ACP 连接（shutdown 未被显式调用的兜底）");
                }
            }
            Err(_) => {
                warn!(
                    target: "claude-acp",
                    "AcpBackend drop 时 conns map 被并发持有，跳过兜底清理（空闲回收/OS 清理兜底）；\
                     建议 main 侧显式接线 AcpBackend::shutdown"
                );
            }
        }
    }
}

/// 长驻连接的 transport 来源（真机子进程 / 测试 in-process mock）。
enum Transport {
    Real(AcpAgent),
    #[cfg(test)]
    Mock(agent_client_protocol::Channel),
}

impl Default for AcpBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// turn 期间在 notification handler 与 `run` 主体之间共享的流式状态。
///
/// - `chunks`：core 传入的通道（克隆一份给 handler，handler 直接推 `AgentChunk`）。
/// - `agent_text`：累计 `AgentMessageChunk` 文本——ACP 的 `session/prompt` 响应只带
///   `stop_reason`，最终回复文本必须从流式 `AgentMessageChunk` 累积。
/// - `usage`：W2-4——`UsageUpdate` 通知的**最新**会话水位（ACP 语义是会话累计值，
///   替换而非求和），turn 结束时并入 RunOutcome.usage。
/// - `cost_baseline`：P0-2（v1.17 审计）——cost 累计水位记账基线（上次轮末的
///   `(session_id, 累计cost)`）。RunOutcome 落库的是**本轮增量**（累计差，负值
///   钳 0），否则每轮把会话累计当单轮成本记账、run_stats 求和严重虚高（per-
///   sender 日上限失效）。连接重建/进程重启后基线丢失，首轮保守记整段（仅此
///   一轮，旧实现是每轮都虚高）。换会话（--resume 到别处）基线不命中同理。
#[derive(Clone)]
struct StreamState {
    chunks: tokio::sync::mpsc::Sender<AgentChunk>,
    agent_text: Arc<Mutex<String>>,
    usage: Arc<Mutex<Option<UsageStats>>>,
    cost_baseline: Arc<Mutex<Option<(String, f64)>>>,
}

impl StreamState {
    fn new(chunks: tokio::sync::mpsc::Sender<AgentChunk>) -> Self {
        Self {
            chunks,
            agent_text: Arc::new(Mutex::new(String::new())),
            usage: Arc::new(Mutex::new(None)),
            cost_baseline: Arc::new(Mutex::new(None)),
        }
    }

    /// 累计 usage → 本轮增量 usage（P0-2）。cost 按基线差值；tokens 语义上是
    /// 上下文水位（ACP 不提供每轮 token），保持透传。
    async fn round_delta_usage(&self, sid: &str, cumulative: UsageStats) -> UsageStats {
        let mut bl = self.cost_baseline.lock().await;
        let round_cost = match &*bl {
            Some((bsid, base)) if bsid == sid => {
                cumulative.total_cost_usd.map(|c| (c - base).max(0.0))
            }
            _ => cumulative.total_cost_usd,
        };
        if let Some(c) = cumulative.total_cost_usd {
            *bl = Some((sid.to_string(), c));
        }
        UsageStats {
            total_cost_usd: round_cost,
            ..cumulative
        }
    }
}

/// 长驻 AcpAgent/connection：单 conv 的跨 run 复用（B2/P5-14）。
struct LongLivedAcp {
    prompt_tx: tokio::sync::mpsc::Sender<PromptReq>,
    _task: tokio::task::JoinHandle<()>,
}

/// `run()` → 长驻 task 的单次 prompt 请求。
struct PromptReq {
    prompt: String,
    session: Option<String>,
    cwd: std::path::PathBuf,
    chunks: tokio::sync::mpsc::Sender<AgentChunk>,
    resp: tokio::sync::oneshot::Sender<Result<RunOutcome>>,
    /// P1-E：cancel 信号——run future drop（dispatch 超时）时 sender drop，
    /// 长驻 task select 检测后 break → 杀**本 conv 的**连接（B2：不再殃及他 conv）。
    cancel: tokio::sync::oneshot::Receiver<()>,
}

impl LongLivedAcp {
    /// spawn 单 conv 的长驻 task：`connect_with` 建连接（spawn claude-agent-acp 子
    /// 进程；SDK `ChildGuard` 在 connection drop 时 kill，无泄漏），main_fn 内 loop
    /// 接收 prompt 跨 run 复用同一子进程 + connection。
    ///
    /// 泛型 `T`：真机为 `AcpAgent`（子进程 stdio）；测试为 in-process `Channel`
    /// （假 agent，见 tests）。
    fn spawn<T>(
        transport: T,
        perm_mode: Arc<RwLock<PermissionMode>>,
        hook: Option<ImPermissionHook>,
        conv: String,
        conns: Arc<Mutex<HashMap<String, Arc<LongLivedAcp>>>>,
        idle_recycle: std::time::Duration,
    ) -> Result<Arc<LongLivedAcp>>
    where
        T: agent_client_protocol::ConnectTo<Client> + 'static,
    {
        let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<PromptReq>(8);
        // 自摘除用：与返回的 LongLivedAcp.prompt_tx 同一 channel（same_channel 比对，
        // 防误删已被重建替代的新条目）。
        let tx_for_cleanup = prompt_tx.clone();
        let current: Arc<Mutex<Option<StreamState>>> = Arc::new(Mutex::new(None));
        let current_for_notif = current.clone();
        let current_for_main = current.clone();
        let perm_for_handler = perm_mode;
        let conv_for_handler = conv.clone();
        let _task = tokio::spawn(async move {
            let _ = Client
                .builder()
                .on_receive_notification(
                    async move |notification: SessionNotification, _cx: ConnectionTo<_>| {
                        // 先 clone StreamState 并立即 drop guard，再跨 await 转发：
                        // forward_update 内部 send 带最长 30s 超时，若持锁跨 await，
                        // 期间主循环（session 建立 / turn 收尾清 current）会在同一
                        // Mutex 上阻塞，极端时互相等。
                        let st = current_for_notif.lock().await.clone();
                        if let Some(st) = st {
                            forward_update(&st, notification.update).await;
                        }
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .on_receive_request(
                    async move |request: RequestPermissionRequest,
                                responder,
                                _cx: ConnectionTo<_>| {
                        // B3：Ask/AutoClaude 档经注入的 IM 审批闭环回调（与
                        // claude-cli 的 MCP → socket → PermissionRouter 同一通道）；
                        // 超时 deny 由 hook 内部（permission_ask_timeout）兜底。
                        let outcome = permission_outcome(
                            &request,
                            &conv_for_handler,
                            &perm_for_handler,
                            hook.as_ref(),
                        )
                        .await;
                        responder.respond(RequestPermissionResponse::new(outcome))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(transport, |connection: ConnectionTo<_>| async move {
                    connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    // P5-4：会话选择以 req.session（dispatch 从 store 读出的权威值）为
                    // 准。此前的 per-conv sessions 缓存命中即用、无视 req.session，导致
                    // /new（store 已删映射）后仍续接旧会话、/resume /switch 接管后仍跑
                    // 在旧上下文。这里仅跟踪**连接当前已 load 的 session**（同 sid 连续
                    // 轮次免重复 LoadSession 的纯优化），无 per-conv 状态。
                    let mut loaded: Option<String> = None;
                    // L7（code-review v8）：连接已 load 的 cwd——/cd 切目录后同
                    // session 的下一轮须重发 LoadSession（缓存只比 sid 不比 cwd
                    // 会让新 cwd 不传递，与「已切到 X，下条消息生效」承诺相悖）。
                    let mut loaded_cwd: Option<std::path::PathBuf> = None;
                    // B2/P5-14：每轮用 sleep_until 实现空闲回收——完成一个 turn 后
                    // 重新起算 CONN_IDLE_RECYCLE 窗口，窗口内无新 prompt 则退出
                    //（connection drop → ChildGuard kill 子进程，名额让出）。
                    loop {
                        let deadline = tokio::time::Instant::now() + idle_recycle;
                        let req = tokio::select! {
                            _ = tokio::time::sleep_until(deadline) => {
                                info!(
                                    target: "claude-acp",
                                    idle_secs = idle_recycle.as_secs(),
                                    "连接空闲回收：断开本 conv 的 ACP 连接"
                                );
                                break;
                            }
                            r = prompt_rx.recv() => match r {
                                Some(r) => r,
                                None => break, // 所有 sender drop（shutdown 清理）
                            },
                        };
                        let st = StreamState::new(req.chunks.clone());
                        *current_for_main.lock().await = Some(st.clone());
                        let cwd = req.cwd.clone();
                        let sid = match req.session.clone() {
                            Some(s)
                                if loaded.as_deref() == Some(s.as_str())
                                    && loaded_cwd.as_deref() == Some(cwd.as_path()) =>
                            {
                                s
                            }
                            Some(s) => match connection
                                .send_request(LoadSessionRequest::new(s.clone(), cwd.clone()))
                                .block_task()
                                .await
                            {
                                Ok(_) => {
                                    loaded = Some(s.clone());
                                    loaded_cwd = Some(cwd.clone());
                                    s
                                }
                                Err(e) => {
                                    // 会话续接失败必须把真实原因写回 req.resp——
                                    // 此前 `?` 直接上抛闭包错误，而外层 `let _ =` 丢弃
                                    // Err 且 resp 从不 send，run 侧只拿到笼统的
                                    // 「长驻 ACP task 无响应」。杀掉本连接（会话状态
                                    // 已不可信）但让调用方看到真实错误。
                                    let _ = req.resp.send(Err(CoreError::Backend(
                                        NAME,
                                        format!("acp load session 失败: {e}"),
                                    )));
                                    break;
                                }
                            },
                            None => match connection
                                .send_request(NewSessionRequest::new(cwd.clone()))
                                .block_task()
                                .await
                            {
                                Ok(resp) => {
                                    let sid = resp.session_id.to_string();
                                    loaded = Some(sid.clone());
                                    sid
                                }
                                Err(e) => {
                                    let _ = req.resp.send(Err(CoreError::Backend(
                                        NAME,
                                        format!("acp new session 失败: {e}"),
                                    )));
                                    break;
                                }
                            },
                        };
                        // P5-5：session 一经建立/续接即通知 dispatch——被 /stop 或超时
                        // 中断的轮次拿不到 RunOutcome，靠它落库续接。
                        let _ = st
                            .chunks
                            .send(AgentChunk::SessionStarted(sid.clone()))
                            .await;
                        let blocks = vec![ContentBlock::Text(TextContent::new(req.prompt.clone()))];
                        let prompt_fut = connection
                            .send_request(PromptRequest::new(
                                agent_client_protocol::schema::v1::SessionId::new(sid.clone()),
                                blocks,
                            ))
                            .block_task();
                        tokio::pin!(prompt_fut);
                        // P1-E：run future 被 cancel（dispatch agent_timeout 超时 drop）时，
                        // cancel sender drop → select 命中 cancel 分支 → break 退出
                        // loop → connect_with 闭包返回 → connection drop → SDK
                        // ChildGuard kill 子进程。B2：per-conv 连接，只杀本会话。
                        tokio::select! {
                            res = &mut prompt_fut => match res {
                                Ok(resp) => {
                                    let final_text = st.agent_text.lock().await.clone();
                                    // W2-4：stop_reason（MaxTokens/Refusal 等）
                                    // + usage（UsageUpdate 最新值→P0-2 本轮增量）。
                                    let stop_reason = stop_reason_str(&resp.stop_reason);
                                    let usage = match *st.usage.lock().await {
                                        Some(u) => Some(st.round_delta_usage(&sid, u).await),
                                        None => None,
                                    };
                                    let _ = st.chunks.send(AgentChunk::Final(final_text.clone())).await;
                                    let _ = req.resp.send(Ok(RunOutcome {
                                        session_id: SessionId(sid),
                                        final_text,
                                        terminal: true,
                                        usage,
                                        stop_reason,
                                    }));
                                }
                                Err(e) => {
                                    let _ = req.resp.send(Err(CoreError::Backend(
                                        NAME,
                                        format!("acp prompt 失败: {e}"),
                                    )));
                                }
                            },
                            _ = req.cancel => {
                                let _ = req.resp.send(Err(CoreError::Backend(
                                    NAME,
                                    "acp prompt 被 cancel（run 超时/drop，已杀本会话连接）".into(),
                                )));
                                break;
                            }
                        }
                        // P5-6：turn 结束即清 current。StreamState 持有 chunks sender 的
                        // 克隆，残留会让 dispatch 的 chunk 循环等不到通道关闭，挂到空闲
                        // 看门狗才退出——此前每轮回复被拖满 agent_idle_timeout。
                        // （cancel 分支 break 跳过此处，但连接随之销毁，sender 一并释放。）
                        *current_for_main.lock().await = None;
                    }
                    Ok(())
                })
                .await;
            // 连接断开（子进程退出/崩溃/空闲回收/cancel）：长驻 task 结束。B2：从
            // map 自摘除（same_channel 防误删已被重建替代的新条目）；prompt_tx 随之
            // 关闭，下次 run() 检测后重建。
            let mut g = conns.lock().await;
            let is_mine = g
                .get(&conv)
                .is_some_and(|cur| cur.prompt_tx.same_channel(&tx_for_cleanup));
            if is_mine {
                g.remove(&conv);
            }
        });
        Ok(Arc::new(LongLivedAcp { prompt_tx, _task }))
    }
}

#[async_trait]
impl Backend for AcpBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    /// W2-4：/model 热设——本会话已有连接不追改（子进程 env 已定），下一次建连
    /// （新 conv / 空闲回收后）以 `ANTHROPIC_MODEL` 前导 env 生效。
    fn set_model(&self, model: Option<String>) {
        *self.model.write() = model;
    }

    fn model(&self) -> Option<String> {
        self.model.read().clone()
    }

    fn supports_model_selection(&self) -> bool {
        true
    }

    /// B3：Ask/AutoClaude 档经注入的 ImPermissionHook 走 IM 审批闭环
    /// （session/request_permission → 审批卡 → y/n / 超时 deny）。
    fn permission_capability(&self) -> PermissionCapability {
        PermissionCapability::FullLoop
    }

    /// B3：dispatcher `run()` 启动时注入 IM 审批闭环回调（新连接 spawn 时读取）。
    fn set_im_permission_hook(&self, hook: Option<ImPermissionHook>) {
        *self.hook.write() = hook;
    }

    /// P4-11：ACP 的 LoadSession 与 CLI 的 --resume 共用同一 claude 会话存储，
    /// 扫描逻辑同 ClaudeBackend。
    async fn list_local_sessions(&self, workdir: &std::path::Path) -> Vec<LocalSession> {
        crate::sessions::scan_for_backend(workdir)
    }

    /// W4-2：会话转录导出（与 CLI 同一 ~/.claude 存储）。
    async fn export_session_markdown(
        &self,
        workdir: &std::path::Path,
        session_id: &str,
    ) -> Option<String> {
        crate::sessions::export_session_md(workdir, session_id)
    }

    /// 进程退出接线（main 在 dispatcher.run() 返回后统一调用）：断开全部
    /// per-conv 连接，长驻 task 退出 → connection drop → ChildGuard kill 子
    /// 进程。语义与固有 [`AcpBackend::shutdown`] 相同，经 trait 暴露给
    /// `Arc<dyn Backend>` 调用方。
    async fn shutdown(&self) {
        AcpBackend::shutdown(self).await
    }

    async fn run(
        &self,
        conv_id: &str,
        prompt: &str,
        session: Option<&SessionId>,
        workdir: &std::path::Path,
        allowed_tools: &[String],
        chunks: tokio::sync::mpsc::Sender<AgentChunk>,
        _initial_todos: &[imagent_core::TodoItem],
        _steer: tokio::sync::mpsc::Receiver<String>,
    ) -> Result<RunOutcome> {
        // allowed_tools 暂无 ACP 直接映射（session/new 无工具白名单字段），依赖 cwd
        // 锁定 + claude 自身工具策略 + 权限审批收敛。
        if !allowed_tools.is_empty() {
            debug!(
                target: "claude-acp",
                ?workdir,
                tools = ?allowed_tools,
                "allowed_tools 暂无 ACP 直接映射，依赖 cwd 锁定 + 权限审批收敛"
            );
        }

        // B2/P5-14：取（或启动）本 conv 的长驻连接（惰性建立、并发上限）。
        let mut ll = self.long_lived(conv_id).await?;
        for attempt in 0..2 {
            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
            // P1-E：cancel_tx 随 run future 生命周期——run 正常完成或被 drop（超时）时
            // drop，触发长驻 task select 的 cancel 分支（杀本会话连接，防子进程泄漏）。
            let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
            let send = ll.prompt_tx.send(PromptReq {
                prompt: prompt.to_string(),
                session: session.map(|s| s.0.clone()),
                cwd: workdir.to_path_buf(),
                chunks: chunks.clone(),
                resp: resp_tx,
                cancel: cancel_rx,
            });
            match send.await {
                Ok(()) => {
                    let _cancel_guard = cancel_tx;
                    return resp_rx
                        .await
                        .map_err(|_| CoreError::Backend(NAME, "长驻 ACP task 无响应".into()))?;
                }
                Err(_) if attempt == 0 => {
                    // 发送失败 = 连接刚好退出（空闲回收/崩溃竞态）：先显式移除 map 中
                    // 该 conv 的旧条目（防御性：正常自摘除/retain 已清，此处确证旧连接
                    // 已死，立即让出名额，不等下一轮 long_lived 的 retain），再重建一次。
                    warn!(
                        target: "claude-acp",
                        conv_id,
                        "长驻 ACP task 已退出（竞态），重建连接重试一次"
                    );
                    self.remove_conn_if_same(conv_id, &ll).await;
                    ll = self.long_lived(conv_id).await?;
                }
                Err(_) => {
                    return Err(CoreError::Backend(
                        NAME,
                        "长驻 ACP task 已退出（重建后仍失败，下次 run 将再重建）".into(),
                    ));
                }
            }
        }
        unreachable!("重试循环最多两轮")
    }
}

/// 把一个 [`SessionUpdate`] 转成对应的 [`AgentChunk`] 推入 core 通道，并把 agent 文本
/// 累积到共享状态（供 final_text）。
///
/// 映射策略（best-effort，未知变体忽略并 debug 记录）：
/// - `AgentMessageChunk`（文本）→ `AgentChunk::Text`（仅 Agent 文本累计）；
///   `UserMessageChunk` 是用户消息回放，忽略（M7）。
/// - `AgentThoughtChunk`（W2-1）→ `AgentChunk::Thought`（与正文分离，卡片折叠展示）。
/// - `ToolCall` → `AgentChunk::ToolUse`（title 首 token 作 tool 名，raw_input 作输入，
///   tool_call_id 作配对 id——W2-3）。
/// - `ToolCallUpdate`（带输出）→ `AgentChunk::ToolResult`（带 tool_call_id）。
/// - `Plan`（W2-2）→ `AgentChunk::TodoList`（任务清单，全量替换语义）。
/// - `UsageUpdate`（W2-4）→ 累计进共享 usage（不推 chunk；turn 结束并入 RunOutcome）。
///
/// B4：异步 send().await + 超时（替代原 `try_send` 通道满即静默丢事件）。发送点在
/// notification handler（async 闭包）里，await 不会死锁 run 主体——chunks 消费方
/// （dispatch 的 chunk 循环）独立并发收；超时兜底防 dispatch 长期不收时把 agent
/// 连接卡死。agent_text 累计仍为同步 try_lock（在 send 之前），即使推送超时，
/// final_text 也不丢。
async fn forward_update(state: &StreamState, update: SessionUpdate) {
    let chunk = match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            if let Some(text) = text_of(&chunk.content) {
                // 累计 agent 文本（同步 try_lock，不阻塞 dispatch loop）。
                if let Ok(mut buf) = state.agent_text.try_lock() {
                    buf.push_str(&text);
                }
                Some(AgentChunk::Text(text))
            } else {
                None
            }
        }
        // M7（code-review v8）：UserMessageChunk 是 agent 对用户消息的**回放**，
        // 不是 agent 产出——此前映射 AgentChunk::Text 使每轮流式卡以用户原话
        // 开头（展示层污染）。忽略（final_text 只累计 AgentMessageChunk，不受影响）。
        SessionUpdate::UserMessageChunk(_) => None,
        SessionUpdate::AgentThoughtChunk(chunk) => {
            // W2-1：推理文本独立透出（Thought）——不进 agent_text / 正文流，
            // 由卡片侧折叠展示（cot 档位控制）。
            text_of(&chunk.content).map(AgentChunk::Thought)
        }
        SessionUpdate::ToolCall(call) => {
            let input = call
                .raw_input
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_else(|_| v.to_string()))
                .unwrap_or_default();
            // W2-3：tool 名取 title 首 token（与 CLI 工具名口径一致，卡片工具行
            // 「Bash — git status」由 input 摘要补全）；id = tool_call_id。
            let tool = call
                .title
                .split_whitespace()
                .next()
                .unwrap_or(&call.title)
                .to_string();
            let id = call.tool_call_id.to_string();
            Some(AgentChunk::ToolUse {
                tool,
                input,
                id: Some(id),
            })
        }
        SessionUpdate::ToolCallUpdate(update) => {
            // 优先用 raw_output；无输出时忽略（仅状态/标题更新无信息量）。
            // W2-3：id = tool_call_id（与 ToolUse 精确配对）；tool 名取 title 首
            // token（此前用 id 冒充 tool 名，CLI/ACP 两边语义不一致）。
            let tool = tool_name_of(&update);
            let id = update.tool_call_id.to_string();
            update.fields.raw_output.as_ref().map(|output| {
                let out = serde_json::to_string(output).unwrap_or_else(|_| output.to_string());
                AgentChunk::ToolResult {
                    tool,
                    output: out,
                    id: Some(id),
                }
            })
        }
        SessionUpdate::Plan(plan) => {
            // W2-2：任务清单——协议为全量替换语义，直接映射最新状态。
            let items: Vec<TodoItem> = plan
                .entries
                .iter()
                .filter_map(|e| {
                    let text = e.content.trim();
                    if text.is_empty() {
                        return None;
                    }
                    let status = match e.status {
                        PlanEntryStatus::Completed => TodoStatus::Completed,
                        PlanEntryStatus::InProgress => TodoStatus::InProgress,
                        PlanEntryStatus::Pending => TodoStatus::Pending,
                        // non_exhaustive：未知状态按未开始。
                        _ => TodoStatus::Pending,
                    };
                    Some(TodoItem {
                        id: None,
                        text: text.to_string(),
                        status,
                    })
                })
                .collect();
            (!items.is_empty()).then_some(AgentChunk::TodoList { items })
        }
        SessionUpdate::UsageUpdate(u) => {
            // W2-4：上下文水位 + 累计成本。ACP 的 used 是**会话累计**上下文 token
            //（非本轮增量）、cost 是会话累计成本——替换而非求和。仅记入共享状态，
            // turn 结束并入 RunOutcome（不推 chunk，不是 IM 可读内容）。
            let cost = u
                .cost
                .as_ref()
                .filter(|c| c.currency.eq_ignore_ascii_case("USD"))
                .map(|c| c.amount);
            if let Ok(mut g) = state.usage.try_lock() {
                *g = Some(UsageStats {
                    input_tokens: u.used,
                    output_tokens: 0,
                    cached_tokens: None,
                    total_cost_usd: cost,
                });
            }
            None
        }
        other => {
            debug!(target: "claude-acp", ?other, "忽略 SessionUpdate 变体");
            None
        }
    };

    if let Some(chunk) = chunk {
        // B4：await send（通道满时挂起等待而非丢弃）；30s 超时兜底防消费方长期
        // 不收时卡死 notification handler。超时丢弃时 warn（agent_text 已同步
        // 累计，final_text 不受影响）。
        match tokio::time::timeout(std::time::Duration::from_secs(30), state.chunks.send(chunk))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(_)) => debug!(target: "claude-acp", "chunks 通道已关闭，停止推送"),
            Err(_) => {
                warn!(target: "claude-acp", "chunks 推送超时（消费方未收），丢弃该 chunk");
            }
        }
    }
}

/// 从 [`ContentBlock`] 中提取文本（仅 Text 变体）；其它变体返回 None。
fn text_of(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(t) => Some(t.text.clone()),
        _ => None,
    }
}

/// W2-4：`PromptResponse.stop_reason` → 人读终止原因（None = 正常结束）。
/// dispatch 据此给「输出被截断 / 被拒绝」类提示。
fn stop_reason_str(reason: &StopReason) -> Option<String> {
    match reason {
        StopReason::EndTurn => None,
        StopReason::MaxTokens => Some("max_tokens".into()),
        StopReason::MaxTurnRequests => Some("max_turn_requests".into()),
        StopReason::Refusal => Some("refusal".into()),
        StopReason::Cancelled => Some("cancelled".into()),
        // non_exhaustive：未知变体按正常处理。
        _ => None,
    }
}

/// 从权限请求的 tool_call 提取与 core `needs_approval`（工具名精确/前缀匹配）语义
/// 对齐的工具名：title 首个 token（如 "Bash git status" → "Bash"）；无 title 回退
/// tool_call_id。**安全关键**：若取 title 全串，approval_tools=["Bash"] 之类永不
/// 命中，高危工具会被自动放行。
/// M4（code-review v8）：title 缺失时不再裸回退 tool_call_id——那种形态在
/// 审批集语义下「清单外放行」= 无询问自动放行（fail-open）。哨兵前缀使
/// [`crate::permission::needs_approval`] 恒命中（fail-closed：必过 IM 审批）。
pub const UNTITLED_TOOL_SENTINEL: &str = "imagent:untitled-tool";

fn tool_name_of(tool_call: &agent_client_protocol::schema::v1::ToolCallUpdate) -> String {
    tool_call
        .fields
        .title
        .as_deref()
        .and_then(|t| t.split_whitespace().next())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{UNTITLED_TOOL_SENTINEL}:{}", tool_call.tool_call_id))
}

/// 按字符截断到 n（审批卡 input 摘要用），超出加省略号。
fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

/// 按 [`PermissionMode`] 计算 `session/request_permission` 的响应 outcome。
///
/// - `Allow` / `Off`（默认）→ 选一个 allow 类选项（让 claude 按 allowed_tools 自理，
///   等同 CLI 的 Off 行为）。
/// - `Deny` → 选一个 reject 类选项；若无则 Cancelled。
/// - `Ask` / `AutoClaude`（IM 审批闭环）→ B3：经注入的 [`ImPermissionHook`] 把审批
///   卡发进 IM、等待 y/n（超时 deny，由 hook 内部的 permission_ask_timeout 兜底），
///   按结果选 allow/reject 类选项；hook 未注入（独立使用）时 fail-closed 拒绝。
async fn permission_outcome(
    request: &RequestPermissionRequest,
    conv: &str,
    mode: &RwLock<PermissionMode>,
    hook: Option<&ImPermissionHook>,
) -> RequestPermissionOutcome {
    let mode = *mode.read();
    match mode {
        PermissionMode::Deny => reject_outcome(&request.options),
        // Auto 不会出现在运行时句柄里（main/SIGHUP//perm 均先 resolve）；
        // 防御性按未接线=放行 Off 同路处理（resolve 后 ACP 本就映射 Off）。
        PermissionMode::Allow | PermissionMode::Off | PermissionMode::Auto => {
            allow_outcome(&request.options)
        }
        PermissionMode::Ask | PermissionMode::AutoClaude => {
            let Some(hook) = hook else {
                // B3：hook 未注入（backend 被独立使用、未经 Dispatcher::run 注入）时
                // 无 IM 闭环可用，fail-closed 拒绝（绝不静默放行）。
                warn!(
                    target: "claude-acp",
                    "Ask 权限模式但 IM 审批回调未注入（backend 未挂到 Dispatcher？），fail-closed 拒绝该次权限请求"
                );
                return reject_outcome(&request.options);
            };
            // 映射到现有审批卡渲染：tool_name 用工具调用 title 的**首个 token**
            // （title 是人读全串如 "Bash git status"，取首词才与 core
            // needs_approval 的工具名精确/前缀匹配语义对齐，否则 approval_tools
            // = ["Bash"] 永不命中、高危工具被自动放行）；无 title 回退
            // tool_call_id。input 摘要取 raw_input JSON（截断 2000，与 socket
            // 闭环同口径）。
            let tool_name = tool_name_of(&request.tool_call);
            let input_summary = truncate_chars(
                &request
                    .tool_call
                    .fields
                    .raw_input
                    .as_ref()
                    .map(|v| serde_json::to_string(v).unwrap_or_else(|_| v.to_string()))
                    .unwrap_or_default(),
                2000,
            );
            let request_id = format!("acp-{}", request.tool_call.tool_call_id);
            let allow = hook(ImPermissionAsk {
                conv_id: conv.to_string(),
                request_id,
                tool_name,
                input_summary,
            })
            .await;
            if allow {
                allow_outcome(&request.options)
            } else {
                // 用户拒绝 / 超时 deny（hook 内部已收敛 pending 与询问卡）。
                reject_outcome(&request.options)
            }
        }
    }
}

/// 选一个 reject 类选项构造 outcome；无 reject 选项则 Cancelled（fail-closed）。
fn reject_outcome(options: &[PermissionOption]) -> RequestPermissionOutcome {
    select_option(options, false)
        .map(|id| RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)))
        .unwrap_or(RequestPermissionOutcome::Cancelled)
}

/// 选一个 allow 类选项构造 outcome；若无 allow 选项则取首个；都没有则 Cancelled。
fn allow_outcome(options: &[PermissionOption]) -> RequestPermissionOutcome {
    select_option(options, true)
        .or_else(|| options.first().map(|o| o.option_id.clone()))
        .map(|id| RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)))
        .unwrap_or(RequestPermissionOutcome::Cancelled)
}

/// 在权限选项中找一个目标 kind 的选项：`allow=true` 找 Allow*，否则找 Reject*。
/// 找不到返回 None（调用方决定兜底语义）。
fn select_option(options: &[PermissionOption], allow: bool) -> Option<PermissionOptionId> {
    let want = |k: PermissionOptionKind| match k {
        PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways => allow,
        PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways => !allow,
        // PermissionOptionKind 为 non_exhaustive，未知变体按非目标处理。
        _ => false,
    };
    options
        .iter()
        .find(|o| want(o.kind))
        .map(|o| o.option_id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        AgentCapabilities, ContentChunk, InitializeResponse, LoadSessionRequest,
        LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptResponse, StopReason,
        ToolCall, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
    };
    use agent_client_protocol::{Agent, Channel};
    use std::time::Duration;

    /// P0-2（v1.17）：cost 累计水位 → 本轮增量。首轮=整段；同会话后续=差值；
    /// 会话累计回落（换会话/重置）→ 钳 0；换 session id → 基线不命中重新整段。
    #[tokio::test]
    async fn acp_cost_delta_round_accounting() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentChunk>(8);
        let st = StreamState::new(tx);
        let u = |cost: f64| UsageStats {
            input_tokens: 1000,
            output_tokens: 0,
            cached_tokens: None,
            total_cost_usd: Some(cost),
        };
        // 首轮：无基线 → 整段（新会话语义正确）。
        assert_eq!(
            st.round_delta_usage("s1", u(0.10)).await.total_cost_usd,
            Some(0.10)
        );
        // 同会话第二轮：差值（浮点近似）。
        let delta2 = st.round_delta_usage("s1", u(0.35)).await.total_cost_usd;
        assert!((delta2.unwrap() - 0.25).abs() < 1e-9, "delta={delta2:?}");
        // 累计回落（水位重置）→ 钳 0，不产负成本。
        assert_eq!(
            st.round_delta_usage("s1", u(0.05)).await.total_cost_usd,
            Some(0.0)
        );
        // 换会话：基线不命中 → 重新整段。
        assert_eq!(
            st.round_delta_usage("s2", u(0.40)).await.total_cost_usd,
            Some(0.40)
        );
    }

    #[test]
    fn name_is_claude_acp() {
        assert_eq!(AcpBackend::new().name(), "claude-acp");
        assert_eq!(AcpBackend::default().name(), "claude-acp");
    }

    #[test]
    fn capability_is_full_loop() {
        // B3：ACP 接入 PermissionRouter（ImPermissionHook）后声明 FullLoop
        //（启动期 ask/auto-claude 档放行）。
        assert_eq!(
            AcpBackend::new().permission_capability(),
            PermissionCapability::FullLoop
        );
    }

    #[test]
    #[serial_test::serial]
    fn agent_command_honors_env() {
        std::env::remove_var("IMAGENT_ACP_COMMAND");
        assert_eq!(AcpBackend::agent_command(), DEFAULT_AGENT_CMD);

        std::env::set_var(
            "IMAGENT_ACP_COMMAND",
            "npx -y @zed-industries/claude-code-acp@latest",
        );
        assert_eq!(
            AcpBackend::agent_command(),
            "npx -y @zed-industries/claude-code-acp@latest"
        );
        std::env::remove_var("IMAGENT_ACP_COMMAND");
    }

    #[test]
    fn acp_agent_parses_default_command() {
        // 验证 crate 的 AcpAgent::from_str 能解析默认命令（不 spawn）。
        assert!(AcpAgent::from_str(DEFAULT_AGENT_CMD).is_ok());
    }

    #[test]
    fn text_of_extracts_text_block() {
        let t = ContentBlock::Text(TextContent::new("hello"));
        assert_eq!(text_of(&t).as_deref(), Some("hello"));
    }

    /// 【安全回归】tool_name 必须取 title 首个 token：core needs_approval 对
    /// approval_tools 做工具名精确/前缀匹配，取 title 全串（"Bash git status"）会让
    /// approval_tools=["Bash"] 永不命中、高危工具全部自动放行。
    #[test]
    fn tool_name_takes_first_token_of_title() {
        // M4（code-review v8）：title 缺失改为哨兵前缀（fail-closed），不再裸
        // 回退 tool_call_id（清单外放行 = 无询问自动放行）。
        let mut tc = ToolCallUpdate::new("tc-1", ToolCallUpdateFields::default());
        tc.fields.title = Some("Bash git status".into());
        assert_eq!(tool_name_of(&tc), "Bash");
        // 无 title：哨兵前缀 + 恒过审。
        let tc = ToolCallUpdate::new("tc-9", ToolCallUpdateFields::default());
        let name = tool_name_of(&tc);
        assert!(name.starts_with(UNTITLED_TOOL_SENTINEL), "哨兵前缀: {name}");
        assert!(
            imagent_core::needs_approval(&["Bash".to_string()], &name),
            "哨兵形态必过审（fail-closed）"
        );
    }

    /// 【可配置连接上限】with_conn_limits 覆盖默认上限。
    #[tokio::test]
    async fn conn_limits_are_configurable() {
        let backend = AcpBackend::new().with_conn_limits(1, Duration::from_secs(60));
        let (chan_a, release_a) = spawn_mock_agent("lim-a");
        let (chan_b, _release_b) = spawn_mock_agent("lim-b");
        let which = std::sync::Mutex::new(std::collections::VecDeque::from(vec![chan_a, chan_b]));
        let backend = backend.with_mock_factory(Arc::new(move || {
            which.lock().unwrap().pop_front().expect("mock 通道")
        }));
        let backend = std::sync::Arc::new(backend);
        let workdir = std::env::temp_dir();

        // conv-a 占用唯一名额（挂起，闸门不放行）。
        let (tx_a, _rx_a) = tokio::sync::mpsc::channel::<AgentChunk>(64);
        let a = backend.clone();
        let wd = workdir.clone();
        let run_a = tokio::spawn(async move {
            let _ = a
                .run("lim-a", "slow", None, &wd, &[], tx_a, &[], {
                    let (sx, rx) = tokio::sync::mpsc::channel(1);
                    drop(sx);
                    rx
                })
                .await;
        });
        tokio::time::sleep(Duration::from_millis(300)).await;

        // 上限 1：第二个 conv 被拒，且错误提示可读。
        let (tx_b, _rx_b) = tokio::sync::mpsc::channel::<AgentChunk>(64);
        let err = backend
            .run("lim-b", "fast", None, &workdir, &[], tx_b, &[], {
                let (sx, rx) = tokio::sync::mpsc::channel(1);
                drop(sx);
                rx
            })
            .await
            .expect_err("上限 1 时第二个 conv 应被拒绝");
        assert!(err.to_string().contains("上限"), "错误应说明上限: {err}");

        // 清理。
        let _ = release_a.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), run_a).await;
    }

    fn dummy_perm_request(options: Vec<PermissionOption>) -> RequestPermissionRequest {
        let mut tc = ToolCallUpdate::new("tc-1", ToolCallUpdateFields::default());
        tc.fields.title = Some("Read src/main.rs".into());
        RequestPermissionRequest::new("s", tc, options)
    }

    fn perm_option(id: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption::new(PermissionOptionId::new(id), id.to_string(), kind)
    }

    #[tokio::test]
    async fn forward_update_agent_message_accumulates_text() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentChunk>(16);
        let state = StreamState::new(tx);

        forward_update(
            &state,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new("hi "),
            ))),
        )
        .await;
        forward_update(
            &state,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new("there"),
            ))),
        )
        .await;

        // 累计文本（同步 try_lock，立即生效）。
        let acc = state.agent_text.try_lock().unwrap().clone();
        assert_eq!(acc, "hi there");

        let c1 = rx.try_recv().unwrap();
        let c2 = rx.try_recv().unwrap();
        match (c1, c2) {
            (AgentChunk::Text(a), AgentChunk::Text(b)) => {
                assert_eq!((a, b), ("hi ".to_string(), "there".to_string()))
            }
            other => panic!("期望两个 Text chunk，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn forward_update_tool_call_emits_tool_use() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentChunk>(16);
        let state = StreamState::new(tx);

        let mut call = ToolCall::new(ToolCallId::new("tc-1"), "Read".to_string());
        call.raw_input = Some(serde_json::json!({"path": "/tmp/a"}));
        forward_update(&state, SessionUpdate::ToolCall(call)).await;
        // ToolUse 已 send（通道容量足够，不 panic 即通过）。
    }

    #[tokio::test]
    async fn forward_update_tool_call_update_with_output_emits_result() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentChunk>(16);
        let state = StreamState::new(tx);

        let upd = ToolCallUpdate::new(
            "tc-2",
            ToolCallUpdateFields::default().raw_output(serde_json::json!({"lines": 3})),
        );
        forward_update(&state, SessionUpdate::ToolCallUpdate(upd)).await;

        match rx.try_recv().unwrap() {
            AgentChunk::ToolResult { tool, output, .. } => {
                assert!(tool.starts_with(UNTITLED_TOOL_SENTINEL), "M4 哨兵: {tool}");
                assert!(output.contains("lines"));
            }
            other => panic!("期望 ToolResult，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn permission_outcome_deny_picks_reject() {
        let request = dummy_perm_request(vec![
            perm_option("allow", PermissionOptionKind::AllowOnce),
            perm_option("reject", PermissionOptionKind::RejectOnce),
        ]);
        let mode = RwLock::new(PermissionMode::Deny);
        match permission_outcome(&request, "c", &mode, None).await {
            RequestPermissionOutcome::Selected(sel) => {
                assert_eq!(sel.option_id.0.as_ref(), "reject")
            }
            _ => panic!("Deny 应选中 reject 选项"),
        }
    }

    #[tokio::test]
    async fn permission_outcome_allow_picks_allow() {
        let request = dummy_perm_request(vec![
            perm_option("reject", PermissionOptionKind::RejectOnce),
            perm_option("allow", PermissionOptionKind::AllowAlways),
        ]);
        let mode = RwLock::new(PermissionMode::Allow);
        match permission_outcome(&request, "c", &mode, None).await {
            RequestPermissionOutcome::Selected(sel) => {
                assert_eq!(sel.option_id.0.as_ref(), "allow")
            }
            _ => panic!("Allow 应选中 allow 选项"),
        }
    }

    #[tokio::test]
    async fn permission_outcome_off_falls_back_to_first() {
        // 无 allow 选项时，Off 走 allow_outcome → 取首个兜底。
        let request = dummy_perm_request(vec![perm_option(
            "reject",
            PermissionOptionKind::RejectOnce,
        )]);
        let mode = RwLock::new(PermissionMode::Off);
        match permission_outcome(&request, "c", &mode, None).await {
            RequestPermissionOutcome::Selected(sel) => {
                assert_eq!(sel.option_id.0.as_ref(), "reject")
            }
            _ => panic!("Off 无 allow 选项时应取首个兜底"),
        }
    }

    #[tokio::test]
    async fn permission_outcome_empty_options_cancels() {
        let request = dummy_perm_request(vec![]);
        let mode = RwLock::new(PermissionMode::Allow);
        assert!(matches!(
            permission_outcome(&request, "c", &mode, None).await,
            RequestPermissionOutcome::Cancelled
        ));
    }

    #[tokio::test]
    async fn permission_outcome_ask_without_hook_fails_closed() {
        // Ask + hook 未注入：fail-closed——有 reject 选项时必选 reject，绝不放行。
        let request = dummy_perm_request(vec![
            perm_option("allow", PermissionOptionKind::AllowOnce),
            perm_option("reject", PermissionOptionKind::RejectOnce),
        ]);
        let mode = RwLock::new(PermissionMode::Ask);
        match permission_outcome(&request, "c", &mode, None).await {
            RequestPermissionOutcome::Selected(sel) => {
                assert_eq!(
                    sel.option_id.0.as_ref(),
                    "reject",
                    "Ask 无 hook 必须 fail-closed 选 reject"
                )
            }
            RequestPermissionOutcome::Cancelled => { /* 无 reject 时 cancel 也算 fail-closed */
            }
            _ => panic!("Ask 应 fail-closed：Selected(reject) 或 Cancelled"),
        }
    }

    #[tokio::test]
    async fn permission_outcome_ask_without_reject_cancels() {
        // 回归保护：Ask 模式 fail-closed，无 Reject* 时应 Cancelled，绝不放行。
        let request = dummy_perm_request(vec![
            perm_option("allow1", PermissionOptionKind::AllowOnce),
            perm_option("allow2", PermissionOptionKind::AllowAlways),
        ]);
        let mode = RwLock::new(PermissionMode::Ask);
        assert!(matches!(
            permission_outcome(&request, "c", &mode, None).await,
            RequestPermissionOutcome::Cancelled
        ));
    }

    #[tokio::test]
    async fn permission_outcome_deny_without_reject_cancels() {
        // 回归保护：Deny 模式下若 options 只含 Allow*（无 Reject*），
        // 修复前会被 select_option 的无条件 fallback 击穿为 Selected(Allow)。
        let request = dummy_perm_request(vec![
            perm_option("allow1", PermissionOptionKind::AllowOnce),
            perm_option("allow2", PermissionOptionKind::AllowAlways),
        ]);
        let mode = RwLock::new(PermissionMode::Deny);
        assert!(matches!(
            permission_outcome(&request, "c", &mode, None).await,
            RequestPermissionOutcome::Cancelled
        ));
    }

    /// B3：Ask + hook 注入——hook 放行 → allow 类选项；hook 拒绝/超时 → reject 类。
    #[tokio::test]
    async fn permission_outcome_ask_via_hook_allow_and_deny() {
        let request = dummy_perm_request(vec![
            perm_option("allow", PermissionOptionKind::AllowOnce),
            perm_option("reject", PermissionOptionKind::RejectOnce),
        ]);
        let mode = RwLock::new(PermissionMode::Ask);

        let allow_hook: ImPermissionHook = Arc::new(|ask| {
            Box::pin(async move {
                // 映射检查：title 首 token → tool_name，request_id 带 acp- 前缀。
                assert_eq!(ask.tool_name, "Read");
                assert!(ask.request_id.starts_with("acp-"));
                true
            })
        });
        match permission_outcome(&request, "c", &mode, Some(&allow_hook)).await {
            RequestPermissionOutcome::Selected(sel) => {
                assert_eq!(sel.option_id.0.as_ref(), "allow", "hook 放行应选 allow")
            }
            _ => panic!("hook allow 应选中 allow 选项"),
        }

        let deny_hook: ImPermissionHook = Arc::new(|_| Box::pin(async move { false }));
        match permission_outcome(&request, "c", &mode, Some(&deny_hook)).await {
            RequestPermissionOutcome::Selected(sel) => {
                assert_eq!(sel.option_id.0.as_ref(), "reject", "hook 拒绝应选 reject")
            }
            _ => panic!("hook deny 应选中 reject 选项"),
        }
    }

    // ------------------------------------------------------------------
    // B2/P5-14：per-conv 连接行为测试（in-process 假 agent，无子进程）。
    //
    // 假 agent = SDK `Agent` role 挂在 `Channel::duplex()` 一端：处理
    // initialize/session/new/session/load/session/prompt，prompt 到达后先推一条
    // AgentMessageChunk 通知，再等 release 闸门放行才回 PromptResponse——用闸门
    // 模拟「长任务」，验证两 conv 不互相阻塞、单 conv cancel 不杀另一 conv。
    // ------------------------------------------------------------------

    /// 起一个假 agent，返回 client 侧 Channel 端点 + prompt 完成闸门
    /// （watch：send(true) 放行本轮 turn）。
    fn spawn_mock_agent(
        session_prefix: &'static str,
    ) -> (Channel, tokio::sync::watch::Sender<bool>) {
        let (agent_side, client_side) = Channel::duplex();
        let (release_tx, mut release_rx) = tokio::sync::watch::channel(false);
        let sid = format!("{session_prefix}-s1");
        let sid_for_prompt = sid.clone();
        tokio::spawn(async move {
            let _ = Agent
                .builder()
                .name(format!("mock-{session_prefix}"))
                .on_receive_request(
                    async move |req: InitializeRequest, responder, _cx: ConnectionTo<_>| {
                        responder.respond(
                            InitializeResponse::new(req.protocol_version)
                                .agent_capabilities(AgentCapabilities::new()),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |_req: NewSessionRequest, responder, _cx: ConnectionTo<_>| {
                        responder.respond(NewSessionResponse::new(sid.clone()))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |_req: LoadSessionRequest, responder, _cx: ConnectionTo<_>| {
                        responder.respond(LoadSessionResponse::new())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |req: PromptRequest, responder, cx: ConnectionTo<_>| {
                        // 先推一条流式文本（走 session/update 通知路径）。
                        cx.send_notification(SessionNotification::new(
                            sid_for_prompt.clone(),
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                ContentBlock::Text(TextContent::new(format!(
                                    "echo:{}",
                                    req.prompt.first().and_then(text_of).unwrap_or_default()
                                ))),
                            )),
                        ))?;
                        // 等闸门放行（模拟长任务）。
                        while !*release_rx.borrow_and_update() {
                            if release_rx.changed().await.is_err() {
                                break;
                            }
                        }
                        responder.respond(PromptResponse::new(StopReason::EndTurn))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_to(agent_side)
                .await;
        });
        (client_side, release_tx)
    }

    /// 按注入顺序给每个新 conv 分发一条 mock 通道。
    fn backend_with_mocks(a: Channel, b: Channel) -> AcpBackend {
        let which = std::sync::Mutex::new(std::collections::VecDeque::from(vec![a, b]));
        AcpBackend::new().with_mock_factory(Arc::new(move || {
            which
                .lock()
                .unwrap()
                .pop_front()
                .expect("mock 通道按 conv 建连顺序注入")
        }))
    }

    /// P5-14：conv A 挂长任务期间，conv B 不被 head-of-line 阻塞、独立完成；
    /// 两 conv 各持一条独立连接。
    #[tokio::test]
    async fn conv_b_completes_while_conv_a_hangs() {
        let (chan_a, release_a) = spawn_mock_agent("a");
        let (chan_b, release_b) = spawn_mock_agent("b");
        let backend = std::sync::Arc::new(backend_with_mocks(chan_a, chan_b));
        let workdir = std::env::temp_dir();

        // A 先跑且挂起（闸门不放行）。注意 future 是惰性的：必须 tokio::spawn
        // 驱动，连接才会真正建立、消费工厂里的第一条 mock 通道。
        let (tx_a, _rx_a) = tokio::sync::mpsc::channel::<AgentChunk>(64);
        let a = backend.clone();
        let wd = workdir.clone();
        let run_a = tokio::spawn(async move {
            let _ = a
                .run("conv-a", "slow", None, &wd, &[], tx_a, &[], {
                    let (sx, rx) = tokio::sync::mpsc::channel(1);
                    drop(sx);
                    rx
                })
                .await;
        });
        tokio::time::sleep(Duration::from_millis(300)).await;

        // B 起跑并立即放行其闸门——旧全局单连接串行模型下 B 的 prompt 排在 A
        // 之后，即便闸门开了也拿不到响应；per-conv 模型下 B 应立即完成。
        let (tx_b, _rx_b) = tokio::sync::mpsc::channel::<AgentChunk>(64);
        let run_b = backend.run("conv-b", "fast", None, &workdir, &[], tx_b, &[], {
            let (sx, rx) = tokio::sync::mpsc::channel(1);
            drop(sx);
            rx
        });
        let mut run_b = std::pin::pin!(run_b);
        let _ = release_b.send(true);
        let out_b = tokio::time::timeout(Duration::from_secs(5), run_b.as_mut())
            .await
            .expect("B 不应被 A 的长任务阻塞（head-of-line）")
            .expect("B run 应成功");
        assert_eq!(out_b.final_text, "echo:fast");

        // 两条连接独立存在（A 挂着、B 仍存活）。
        {
            let conns = backend.conns.lock().await;
            let a = conns.get("conv-a").expect("A 连接应存在");
            let b = conns.get("conv-b").expect("B 连接应存在");
            assert_ne!(Arc::as_ptr(a), Arc::as_ptr(b), "两 conv 应各持独立连接");
        }

        // 清理：放行 A 让其完成，防 task 泄漏。
        let _ = release_a.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), run_a).await;
    }

    /// P5-14：单 conv cancel（run future drop，模拟 dispatch agent_timeout）只杀
    /// 该会话连接；另一 conv 照常完成。
    #[tokio::test]
    async fn cancel_conv_a_does_not_kill_conv_b() {
        let (chan_a, _release_a) = spawn_mock_agent("a"); // A 永不放行
        let (chan_b, release_b) = spawn_mock_agent("b");
        let backend_for_a = std::sync::Arc::new(backend_with_mocks(chan_a, chan_b));
        let workdir = std::env::temp_dir();

        // A 挂起后被 cancel（abort run task = cancel_tx drop，模拟 dispatch
        // agent_timeout drop run future）。future 惰性，须 spawn 驱动建立连接。
        let (tx_a, _rx_a) = tokio::sync::mpsc::channel::<AgentChunk>(64);
        let a = backend_for_a.clone();
        let wd = workdir.clone();
        let run_a = tokio::spawn(async move {
            let _ = a
                .run("conv-a", "slow", None, &wd, &[], tx_a, &[], {
                    let (sx, rx) = tokio::sync::mpsc::channel(1);
                    drop(sx);
                    rx
                })
                .await;
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        run_a.abort(); // 等 A 进入 prompt 后 cancel
                       // 等 A 的连接自摘除（cancel → break → task 退出 → map 自摘除）。
        for _ in 0..200 {
            if !backend_for_a.conns.lock().await.contains_key("conv-a") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !backend_for_a.conns.lock().await.contains_key("conv-a"),
            "cancel 后 A 的连接应被回收"
        );

        // B 不受影响：照常完成。
        let (tx_b, _rx_b) = tokio::sync::mpsc::channel::<AgentChunk>(64);
        let run_b = backend_for_a.run("conv-b", "fast", None, &workdir, &[], tx_b, &[], {
            let (sx, rx) = tokio::sync::mpsc::channel(1);
            drop(sx);
            rx
        });
        let mut run_b = std::pin::pin!(run_b);
        let _ = release_b.send(true);
        let out_b = tokio::time::timeout(Duration::from_secs(5), run_b.as_mut())
            .await
            .expect("B 不应受 A cancel 影响")
            .expect("B run 应成功");
        assert_eq!(out_b.final_text, "echo:fast");
    }

    /// B2：并发上限——超 [`MAX_CONCURRENT_CONNS`] 的新 conv 直接拒绝（可读错误），
    /// 已有 conv 不受影响。
    #[tokio::test]
    async fn concurrent_conn_cap_rejects() {
        let backend = AcpBackend::new().with_mock_factory(Arc::new(|| {
            // 此测试只验证 map 上限逻辑，不需要能说话的通道。
            let (client_side, _agent_side) = Channel::duplex();
            client_side
        }));
        // 预填 map 到上限：轻量连接（prompt_tx 存活的空 task，模拟活连接）。
        {
            let mut conns = backend.conns.lock().await;
            for i in 0..MAX_CONCURRENT_CONNS {
                let (tx, mut rx) = tokio::sync::mpsc::channel::<PromptReq>(8);
                tokio::spawn(async move { while rx.recv().await.is_some() {} });
                conns.insert(
                    format!("conv-{i}"),
                    Arc::new(LongLivedAcp {
                        prompt_tx: tx,
                        _task: tokio::spawn(async {}),
                    }),
                );
            }
        }
        // 超限：新 conv 被拒绝；已有 conv 复用不受影响。
        let err = match backend.long_lived("conv-extra").await {
            Err(e) => e,
            Ok(_) => panic!("超限的新 conv 应被拒绝"),
        };
        assert!(
            err.to_string().contains("上限"),
            "错误应说明并发上限: {err}"
        );
        assert!(
            backend.long_lived("conv-0").await.is_ok(),
            "已有 conv 应复用连接"
        );
    }

    /// 【错误不被吞】LoadSession 失败时，真实错误必须经 resp 写回 run 调用方
    /// （而非笼统的「长驻 ACP task 无响应」）。
    #[tokio::test]
    async fn load_session_failure_reports_real_error() {
        // 假 agent 只处理 initialize / session/new——session/load 未注册，
        // 客户端 LoadSession 请求会收到错误响应。
        let (agent_side, client_side) = Channel::duplex();
        tokio::spawn(async move {
            let _ = Agent
                .builder()
                .name("mock-no-load")
                .on_receive_request(
                    async move |req: InitializeRequest, responder, _cx: ConnectionTo<_>| {
                        responder.respond(
                            InitializeResponse::new(req.protocol_version)
                                .agent_capabilities(AgentCapabilities::new()),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |_req: NewSessionRequest, responder, _cx: ConnectionTo<_>| {
                        responder.respond(NewSessionResponse::new("never-s1".to_string()))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_to(agent_side)
                .await;
        });
        let chan = std::sync::Mutex::new(std::collections::VecDeque::from(vec![client_side]));
        let backend = AcpBackend::new().with_mock_factory(Arc::new(move || {
            chan.lock().unwrap().pop_front().expect("mock 通道")
        }));
        let workdir = std::env::temp_dir();
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentChunk>(64);

        let err = backend
            .run(
                "conv-load-fail",
                "hi",
                Some(&SessionId("old-sid".into())),
                &workdir,
                &[],
                tx,
                &[],
                {
                    let (sx, rx) = tokio::sync::mpsc::channel(1);
                    drop(sx);
                    rx
                },
            )
            .await
            .expect_err("LoadSession 失败应返回错误");
        let msg = err.to_string();
        assert!(
            msg.contains("load session"),
            "错误应携带真实原因（load session 失败）: {msg}"
        );
    }

    /// 【僵尸连接防御清理】remove_conn_if_same：仅当 map 条目与给定连接同 channel
    /// 时移除（不误删已被重建替代的新条目）。
    #[tokio::test]
    async fn remove_conn_if_same_only_removes_matching_entry() {
        let backend = AcpBackend::new();
        let (old_tx, _old_rx) = tokio::sync::mpsc::channel::<PromptReq>(8);
        let (new_tx, _new_rx) = tokio::sync::mpsc::channel::<PromptReq>(8);
        let old_ll = Arc::new(LongLivedAcp {
            prompt_tx: old_tx,
            _task: tokio::spawn(async {}),
        });

        // map 中是 old_ll：同 channel → 移除。
        backend
            .conns
            .lock()
            .await
            .insert("conv-x".into(), old_ll.clone());
        backend.remove_conn_if_same("conv-x", &old_ll).await;
        assert!(
            !backend.conns.lock().await.contains_key("conv-x"),
            "同 channel 的旧条目应被移除"
        );

        // map 中已被重建的新条目：不同 channel → 不动。
        let rebuilt = Arc::new(LongLivedAcp {
            prompt_tx: new_tx,
            _task: tokio::spawn(async {}),
        });
        backend
            .conns
            .lock()
            .await
            .insert("conv-x".into(), rebuilt.clone());
        backend.remove_conn_if_same("conv-x", &old_ll).await;
        assert!(
            backend.conns.lock().await.contains_key("conv-x"),
            "不同 channel（重建后的新条目）不应被误删"
        );
    }

    /// 【shutdown 无人调用的兜底】Drop for AcpBackend 释放 map 内全部连接条目
    /// （shutdown 未被 main 接线前的最小本 crate 方案）。
    #[tokio::test]
    async fn drop_releases_conn_entries() {
        let backend = AcpBackend::new();
        let conns = backend.conns.clone();
        let (tx, _rx) = tokio::sync::mpsc::channel::<PromptReq>(8);
        conns.lock().await.insert(
            "conv-z".into(),
            Arc::new(LongLivedAcp {
                prompt_tx: tx,
                _task: tokio::spawn(async {}),
            }),
        );
        assert!(conns.lock().await.contains_key("conv-z"));
        drop(backend);
        assert!(
            conns.lock().await.is_empty(),
            "drop backend 后连接条目应被清空"
        );
    }

    // ---------------------------------------------------------------------
    // 真机集成测试（需 claude-agent-acp 已安装 + Claude 已认证），默认跳过：
    //   cargo test --package imagent-claude -- --ignored acp_e2e
    // ---------------------------------------------------------------------
    #[tokio::test]
    #[ignore = "需 claude-agent-acp + Claude 认证；待真机校准"]
    async fn acp_e2e_say_hi() {
        // 端到端：spawn claude-agent-acp，session/new + session/prompt "say hi in one word"，
        // 断言收到非空 final_text。
        let backend = AcpBackend::new();
        let workdir = std::env::current_dir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentChunk>(64);
        let outcome = backend
            .run(
                "e2e",
                "Reply with exactly: hi",
                None,
                &workdir,
                &[],
                tx,
                &[],
                {
                    let (sx, rx) = tokio::sync::mpsc::channel(1);
                    drop(sx);
                    rx
                },
            )
            .await
            .expect("acp run 应成功");
        assert!(!outcome.final_text.is_empty(), "final_text 不应为空");
        // 至少收到一个 Final chunk。
        let mut got_final = false;
        while let Ok(chunk) = rx.try_recv() {
            if matches!(chunk, AgentChunk::Final(_)) {
                got_final = true;
            }
        }
        assert!(got_final, "应推送 Final chunk");
    }
    /// H2（code-review v8）：ACP 命令消毒——env -i + 白名单前导；model 注入；
    /// 白名单之外的继承被物理切断。
    #[test]
    fn sanitized_agent_command_env_isolation() {
        // 无 model：env -i 前导 + 基础命令殿后。
        let cmd = AcpBackend::sanitized_agent_command(None);
        assert!(cmd.starts_with("/usr/bin/env -i "), "{cmd}");
        assert!(cmd.ends_with("claude-agent-acp"), "{cmd}");
        // PATH 在真实环境中必然存在且安全 → 一定被注入。
        assert!(cmd.contains("PATH="), "{cmd}");
        // 有 model：ANTHROPIC_MODEL 注入。
        let cmd2 = AcpBackend::sanitized_agent_command(Some("glm-5.3[1M]".into()));
        assert!(cmd2.contains("ANTHROPIC_MODEL=glm-5.3[1M]"), "{cmd2}");
        // 值含空格的键在真实环境难保证存在——形态由 safe 闭包保证（间接）：
        // 命令不含未加引号的空白赋值段。
        for seg in cmd2.split_whitespace() {
            if seg.contains('=') {
                assert!(
                    seg.starts_with("/usr/bin/env")
                        || seg.starts_with("claude")
                        || seg
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || "._/:=+-@[]".contains(c)),
                    "异常赋值段: {seg}"
                );
            }
        }
    }
}
