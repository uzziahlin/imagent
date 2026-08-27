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
//!   [`PermissionMode`] 自动响应（MVP，Ask 模式留 TODO）。
//!
//! ## MVP 策略
//!
//! 当前每次 `run` spawn 一个新的 `claude-agent-acp` 子进程并在 turn 结束后随连接退出
//! （功能正确优先）。**跨 run 复用进程与 session 缓存**（性能优化的核心收益）作为后续
//! TODO：需要 `AcpBackend` 持有长驻子进程句柄 + session 缓存，复杂度高，单独迭代。
//!
//! [`AgentChunk`]: imagent_core::AgentChunk

use std::str::FromStr;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, LoadSessionRequest, NewSessionRequest, PermissionOption,
    PermissionOptionId, PermissionOptionKind, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification, SessionUpdate, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{AcpAgent, Client, ConnectionTo};
use async_trait::async_trait;
use imagent_core::{
    AgentChunk, Backend, CoreError, LocalSession, PermissionMode, Result, RunOutcome, SessionId,
};
use parking_lot::RwLock;
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// ACP backend 的固定名称。
const NAME: &str = "claude-acp";

/// 默认 spawn 的 agent 命令（PATH 可见的 `claude-agent-acp`）。
const DEFAULT_AGENT_CMD: &str = "claude-agent-acp";

/// `claude-agent-acp` 长驻子进程 Backend（ACP/JSON-RPC）。
///
/// 持有共享的 [`PermissionMode`] 句柄（与 [`crate::ClaudeBackend`] 一致，支持 SIGHUP
/// 热重载）。每次 `run` 用 [`AcpAgent`] spawn 一个子进程，跑完一个 turn 后退出。
pub struct AcpBackend {
    permission_mode: Arc<RwLock<PermissionMode>>,
    /// 长驻 AcpAgent/connection（跨 run 复用 claude-agent-acp 子进程，P1-4）。
    /// None=未启动或上次崩溃；run 时 lazy get_or_init。子进程由 SDK 的 ChildGuard 在
    /// connection drop 时 kill（无泄漏）。
    long_lived: Arc<Mutex<Option<Arc<LongLivedAcp>>>>,
}

impl AcpBackend {
    /// 默认构造（`PermissionMode::Off`，等同 CLI 的 Off 行为）。
    pub fn new() -> Self {
        Self {
            permission_mode: Arc::new(RwLock::new(PermissionMode::Off)),
            long_lived: Arc::new(Mutex::new(None)),
        }
    }

    /// 用指定权限模式构造。
    pub fn with_permission_mode(mode: PermissionMode) -> Self {
        Self {
            permission_mode: Arc::new(RwLock::new(mode)),
            long_lived: Arc::new(Mutex::new(None)),
        }
    }

    /// 用外部共享句柄构造——与 `Dispatcher` 共享同一 `Arc<RwLock<PermissionMode>>`，
    /// 使 SIGHUP 热重载对 backend 即时生效（每次 `run` 取最新值）。
    pub fn with_permission_mode_shared(mode: Arc<RwLock<PermissionMode>>) -> Self {
        Self {
            permission_mode: mode,
            long_lived: Arc::new(Mutex::new(None)),
        }
    }

    /// 解析要 spawn 的 agent 命令字符串。
    ///
    /// 优先取环境变量 `IMAGENT_ACP_COMMAND`（便于切版本/加参数），否则用
    /// [`DEFAULT_AGENT_CMD`]。支持 shell 风格拆分（由 crate 的 `AcpAgent::from_str`
    /// 处理）。
    fn agent_command() -> String {
        std::env::var("IMAGENT_ACP_COMMAND").unwrap_or_else(|_| DEFAULT_AGENT_CMD.into())
    }

    /// 取（或启动）长驻 AcpAgent task。已存活（prompt_tx 未关闭）则复用；否则重建。
    /// 子进程复用 = 性能收益（不再每次 run spawn claude-agent-acp）。
    async fn long_lived(&self) -> Result<Arc<LongLivedAcp>> {
        let mut g = self.long_lived.lock().await;
        if let Some(ll) = g.as_ref() {
            if !ll.prompt_tx.is_closed() {
                return Ok(ll.clone());
            }
        }
        let ll = LongLivedAcp::spawn(Self::agent_command(), Arc::clone(&self.permission_mode))?;
        *g = Some(ll.clone());
        Ok(ll)
    }
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
#[derive(Clone)]
struct StreamState {
    chunks: tokio::sync::mpsc::Sender<AgentChunk>,
    agent_text: Arc<Mutex<String>>,
}

impl StreamState {
    fn new(chunks: tokio::sync::mpsc::Sender<AgentChunk>) -> Self {
        Self {
            chunks,
            agent_text: Arc::new(Mutex::new(String::new())),
        }
    }
}

/// 长驻 AcpAgent/connection：跨 run 复用 claude-agent-acp 子进程（P1-4）。
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
    /// 长驻 task select 检测后 break → 杀连接，防子进程资源泄漏。
    cancel: tokio::sync::oneshot::Receiver<()>,
}

impl LongLivedAcp {
    /// spawn 长驻 task：`connect_with` 建连接（spawn claude-agent-acp 子进程；SDK
    /// `ChildGuard` 在 connection drop 时 kill，无泄漏），main_fn 内 loop 接收 prompt
    /// 跨 run 复用同一子进程 + connection。
    fn spawn(
        agent_cmd: String,
        perm_mode: Arc<RwLock<PermissionMode>>,
    ) -> Result<Arc<LongLivedAcp>> {
        let agent = AcpAgent::from_str(&agent_cmd)
            .map_err(|e| CoreError::Backend(NAME, format!("解析 agent 命令失败: {e}")))?;
        let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<PromptReq>(8);
        let current: Arc<Mutex<Option<StreamState>>> = Arc::new(Mutex::new(None));
        let current_for_notif = current.clone();
        let current_for_main = current.clone();
        let perm_for_handler = perm_mode;
        let _task = tokio::spawn(async move {
            let _ = Client
                .builder()
                .on_receive_notification(
                    async move |notification: SessionNotification, _cx: ConnectionTo<_>| {
                        if let Some(st) = current_for_notif.lock().await.as_ref() {
                            forward_update(st, notification.update).await;
                        }
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .on_receive_request(
                    async move |request: RequestPermissionRequest,
                                responder,
                                _cx: ConnectionTo<_>| {
                        let outcome = permission_outcome(&request, &perm_for_handler);
                        responder.respond(RequestPermissionResponse::new(outcome))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(agent, |connection: ConnectionTo<_>| async move {
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
                    while let Some(req) = prompt_rx.recv().await {
                        let st = StreamState::new(req.chunks.clone());
                        *current_for_main.lock().await = Some(st.clone());
                        let cwd = req.cwd.clone();
                        let sid = match req.session.clone() {
                            Some(s) if loaded.as_deref() == Some(s.as_str()) => s,
                            Some(s) => {
                                connection
                                    .send_request(LoadSessionRequest::new(
                                        s.clone(),
                                        cwd.clone(),
                                    ))
                                    .block_task()
                                    .await?;
                                loaded = Some(s.clone());
                                s
                            }
                            None => {
                                let sid = connection
                                    .send_request(NewSessionRequest::new(cwd.clone()))
                                    .block_task()
                                    .await?
                                    .session_id
                                    .to_string();
                                loaded = Some(sid.clone());
                                sid
                            }
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
                        // cancel sender drop → select 命中 cancel 分支 → break 退出 while →
                        // connect_with 闭包返回 → connection drop → SDK ChildGuard kill 子进程。
                        tokio::select! {
                            res = &mut prompt_fut => match res {
                                Ok(_) => {
                                    let final_text = st.agent_text.lock().await.clone();
                                    let _ = st.chunks.send(AgentChunk::Final(final_text.clone())).await;
                                    let _ = req.resp.send(Ok(RunOutcome {
                                        session_id: SessionId(sid),
                                        final_text,
                                        terminal: true,
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
                                    "acp prompt 被 cancel（run 超时/drop，已杀连接）".into(),
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
            // 连接断开（子进程退出/崩溃）：长驻 task 结束，prompt_tx drop。
            // 下次 run() 检测 is_closed → 重建（SDK ChildGuard 已 kill 子进程，无泄漏）。
        });
        Ok(Arc::new(LongLivedAcp { prompt_tx, _task }))
    }
}

#[async_trait]
impl Backend for AcpBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    /// P4-11：ACP 的 LoadSession 与 CLI 的 --resume 共用同一 claude 会话存储，
    /// 扫描逻辑同 ClaudeBackend。
    async fn list_local_sessions(&self, workdir: &std::path::Path) -> Vec<LocalSession> {
        crate::sessions::scan_for_backend(workdir)
    }

    async fn run(
        &self,
        _conv_id: &str,
        prompt: &str,
        session: Option<&SessionId>,
        workdir: &std::path::Path,
        allowed_tools: &[String],
        chunks: tokio::sync::mpsc::Sender<AgentChunk>,
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

        // 取（或启动）长驻 AcpAgent task，跨 run 复用子进程 + connection（P1-4）。
        let ll = self.long_lived().await?;
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        // P1-E：cancel_tx 随 run future 生命周期——run 正常完成或被 drop（超时）时
        // drop，触发长驻 task select 的 cancel 分支（杀连接，防子进程泄漏）。
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        ll.prompt_tx
            .send(PromptReq {
                prompt: prompt.to_string(),
                session: session.map(|s| s.0.clone()),
                cwd: workdir.to_path_buf(),
                chunks: chunks.clone(),
                resp: resp_tx,
                cancel: cancel_rx,
            })
            .await
            .map_err(|_| {
                CoreError::Backend(
                    NAME,
                    "长驻 ACP task 已退出（可能崩溃，下次 run 将重建）".into(),
                )
            })?;
        let _cancel_guard = cancel_tx;
        resp_rx
            .await
            .map_err(|_| CoreError::Backend(NAME, "长驻 ACP task 无响应".into()))?
    }
}

/// 把一个 [`SessionUpdate`] 转成对应的 [`AgentChunk`] 推入 core 通道，并把 agent 文本
/// 累积到共享状态（供 final_text）。
///
/// 映射策略（best-effort，未知变体忽略并 debug 记录）：
/// - `AgentMessageChunk`/`UserMessageChunk`（文本）→ `AgentChunk::Text`（仅 Agent 文本累计）。
/// - `ToolCall` → `AgentChunk::ToolUse`（title 作 tool 名，raw_input 作输入）。
/// - `ToolCallUpdate`（带输出）→ `AgentChunk::ToolResult`。
/// - 其它（Plan/UsageUpdate/...）→ 忽略。
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
        SessionUpdate::UserMessageChunk(chunk) => text_of(&chunk.content).map(AgentChunk::Text),
        SessionUpdate::AgentThoughtChunk(chunk) => {
            // 推理文本：best-effort 也走 Text，保持可见。
            text_of(&chunk.content).map(AgentChunk::Text)
        }
        SessionUpdate::ToolCall(call) => {
            let input = call
                .raw_input
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_else(|_| v.to_string()))
                .unwrap_or_default();
            Some(AgentChunk::ToolUse {
                tool: call.title,
                input,
            })
        }
        SessionUpdate::ToolCallUpdate(update) => {
            // 优先用 raw_output；无输出时忽略（仅状态/标题更新无信息量）。
            let tool = update.tool_call_id.to_string();
            update.fields.raw_output.as_ref().map(|output| {
                let out = serde_json::to_string(output).unwrap_or_else(|_| output.to_string());
                AgentChunk::ToolResult { tool, output: out }
            })
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
        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            state.chunks.send(chunk),
        )
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

/// 按 [`PermissionMode`] 计算 `session/request_permission` 的响应 outcome（MVP 自动策略）。
///
/// - `Allow` / `Off`（默认）→ 选一个 allow 类选项（让 claude 按 allowed_tools 自理，
///   等同 CLI 的 Off 行为）。
/// - `Deny` → 选一个 reject 类选项；若无则 Cancelled。
/// - `Ask`（IM 审批闭环）→ ACP 后端尚未接 PermissionRouter，**fail-closed**：选 reject
///   类选项（若无则 Cancelled），绝不静默放行。如需 IM 审批闭环请用 claude-cli 后端。
fn permission_outcome(
    request: &RequestPermissionRequest,
    mode: &RwLock<PermissionMode>,
) -> RequestPermissionOutcome {
    let mode = *mode.read();
    match mode {
        PermissionMode::Deny => select_option(&request.options, false)
            .map(|id| RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)))
            .unwrap_or(RequestPermissionOutcome::Cancelled),
        // Auto 不会出现在运行时句柄里（main/SIGHUP//perm 均先 resolve）；
        // 防御性按未接线=放行 Off 同路处理（resolve 后 ACP 本就映射 Off）。
        PermissionMode::Allow | PermissionMode::Off | PermissionMode::Auto => {
            allow_outcome(&request.options)
        }
        // AutoClaude 也进不来（resolve 只在 claude-cli 产生；防御性同 Ask fail-closed）。
        PermissionMode::Ask | PermissionMode::AutoClaude => {
            // ACP 后端尚未接入 IM 审批闭环（需把 core 的 PermissionRouter 接到 ACP 的
            // session/request_permission 通知通道，复杂度高）。为安全（fail-closed），
            // Ask 模式下拒绝每次权限请求，而非静默放行。如需 IM 审批闭环，请用
            // claude-cli 后端（config: agent = "claude-cli"）。
            warn!(
                target: "claude-acp",
                "Ask 权限模式在 ACP 后端不可用（未接 IM 审批闭环），按 fail-closed 拒绝该次权限请求"
            );
            select_option(&request.options, false)
                .map(|id| RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)))
                .unwrap_or(RequestPermissionOutcome::Cancelled)
        }
    }
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
        ContentChunk, ToolCall, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
    };

    #[test]
    fn name_is_claude_acp() {
        assert_eq!(AcpBackend::new().name(), "claude-acp");
        assert_eq!(AcpBackend::default().name(), "claude-acp");
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

    fn dummy_perm_request(options: Vec<PermissionOption>) -> RequestPermissionRequest {
        RequestPermissionRequest::new(
            "s",
            ToolCallUpdate::new("tc-1", ToolCallUpdateFields::default()),
            options,
        )
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
            AgentChunk::ToolResult { tool, output } => {
                assert_eq!(tool, "tc-2");
                assert!(output.contains("lines"));
            }
            other => panic!("期望 ToolResult，得到 {other:?}"),
        }
    }

    #[test]
    fn permission_outcome_deny_picks_reject() {
        let request = dummy_perm_request(vec![
            perm_option("allow", PermissionOptionKind::AllowOnce),
            perm_option("reject", PermissionOptionKind::RejectOnce),
        ]);
        let mode = RwLock::new(PermissionMode::Deny);
        match permission_outcome(&request, &mode) {
            RequestPermissionOutcome::Selected(sel) => {
                assert_eq!(sel.option_id.0.as_ref(), "reject")
            }
            _ => panic!("Deny 应选中 reject 选项"),
        }
    }

    #[test]
    fn permission_outcome_allow_picks_allow() {
        let request = dummy_perm_request(vec![
            perm_option("reject", PermissionOptionKind::RejectOnce),
            perm_option("allow", PermissionOptionKind::AllowAlways),
        ]);
        let mode = RwLock::new(PermissionMode::Allow);
        match permission_outcome(&request, &mode) {
            RequestPermissionOutcome::Selected(sel) => {
                assert_eq!(sel.option_id.0.as_ref(), "allow")
            }
            _ => panic!("Allow 应选中 allow 选项"),
        }
    }

    #[test]
    fn permission_outcome_off_falls_back_to_first() {
        // 无 allow 选项时，Off 走 allow_outcome → 取首个兜底。
        let request = dummy_perm_request(vec![perm_option(
            "reject",
            PermissionOptionKind::RejectOnce,
        )]);
        let mode = RwLock::new(PermissionMode::Off);
        match permission_outcome(&request, &mode) {
            RequestPermissionOutcome::Selected(sel) => {
                assert_eq!(sel.option_id.0.as_ref(), "reject")
            }
            _ => panic!("Off 无 allow 选项时应取首个兜底"),
        }
    }

    #[test]
    fn permission_outcome_empty_options_cancels() {
        let request = dummy_perm_request(vec![]);
        let mode = RwLock::new(PermissionMode::Allow);
        assert!(matches!(
            permission_outcome(&request, &mode),
            RequestPermissionOutcome::Cancelled
        ));
    }

    #[test]
    fn permission_outcome_ask_fails_closed() {
        // Ask 在 ACP 后端 fail-closed：有 reject 选项时必选 reject，绝不放行。
        let request = dummy_perm_request(vec![
            perm_option("allow", PermissionOptionKind::AllowOnce),
            perm_option("reject", PermissionOptionKind::RejectOnce),
        ]);
        let mode = RwLock::new(PermissionMode::Ask);
        match permission_outcome(&request, &mode) {
            RequestPermissionOutcome::Selected(sel) => {
                assert_eq!(
                    sel.option_id.0.as_ref(),
                    "reject",
                    "Ask 必须 fail-closed 选 reject"
                )
            }
            RequestPermissionOutcome::Cancelled => { /* 无 reject 时 cancel 也算 fail-closed */
            }
            _ => panic!("Ask 应 fail-closed：Selected(reject) 或 Cancelled"),
        }
    }

    #[test]
    fn permission_outcome_deny_without_reject_cancels() {
        // 回归保护：Deny 模式下若 options 只含 Allow*（无 Reject*），
        // 修复前会被 select_option 的无条件 fallback 击穿为 Selected(Allow)。
        let request = dummy_perm_request(vec![
            perm_option("allow1", PermissionOptionKind::AllowOnce),
            perm_option("allow2", PermissionOptionKind::AllowAlways),
        ]);
        let mode = RwLock::new(PermissionMode::Deny);
        assert!(matches!(
            permission_outcome(&request, &mode),
            RequestPermissionOutcome::Cancelled
        ));
    }

    #[test]
    fn permission_outcome_ask_without_reject_cancels() {
        // 回归保护：Ask 模式 fail-closed，无 Reject* 时应 Cancelled，绝不放行。
        let request = dummy_perm_request(vec![
            perm_option("allow1", PermissionOptionKind::AllowOnce),
            perm_option("allow2", PermissionOptionKind::AllowAlways),
        ]);
        let mode = RwLock::new(PermissionMode::Ask);
        assert!(matches!(
            permission_outcome(&request, &mode),
            RequestPermissionOutcome::Cancelled
        ));
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
            .run("e2e", "Reply with exactly: hi", None, &workdir, &[], tx)
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
}
