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
    SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{AcpAgent, Client, ConnectionTo};
use async_trait::async_trait;
use imagent_core::{AgentChunk, Backend, CoreError, PermissionMode, Result, RunOutcome, SessionId};
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
}

impl AcpBackend {
    /// 默认构造（`PermissionMode::Off`，等同 CLI 的 Off 行为）。
    pub fn new() -> Self {
        Self {
            permission_mode: Arc::new(RwLock::new(PermissionMode::Off)),
        }
    }

    /// 用指定权限模式构造。
    pub fn with_permission_mode(mode: PermissionMode) -> Self {
        Self {
            permission_mode: Arc::new(RwLock::new(mode)),
        }
    }

    /// 用外部共享句柄构造——与 `Dispatcher` 共享同一 `Arc<RwLock<PermissionMode>>`，
    /// 使 SIGHUP 热重载对 backend 即时生效（每次 `run` 取最新值）。
    pub fn with_permission_mode_shared(mode: Arc<RwLock<PermissionMode>>) -> Self {
        Self {
            permission_mode: mode,
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

#[async_trait]
impl Backend for AcpBackend {
    fn name(&self) -> &'static str {
        NAME
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
        // 1. 解析 agent 命令并构造 AcpAgent（实现 ConnectTo<Client>，spawn 子进程 + stdio）。
        let agent = AcpAgent::from_str(&Self::agent_command())
            .map_err(|e| CoreError::Backend(NAME, format!("解析 agent 命令失败: {e}")))?;

        // 2. 共享流式状态：handler 直接把 SessionUpdate 转 AgentChunk 推给 core，
        //    并累计 agent 文本作为 final_text 来源。
        let state = StreamState::new(chunks);
        let notif_state = state.clone();
        let perm_mode = Arc::clone(&self.permission_mode);

        // 提前把 workdir 转成绝对路径，供 session/new|load 的 cwd 锁定使用。
        let cwd = workdir.to_path_buf();

        // TODO(P3.4): allowed_tools 目前无直接 ACP 映射。session/new 无工具白名单字段；
        //   依赖 cwd 锁定 + claude 自身工具策略 + 权限审批收敛。后续若 claude-agent-acp
        //   暴露工具开关（如环境变量/配置），在此接入。
        if !allowed_tools.is_empty() {
            debug!(
                target: "claude-acp",
                ?workdir,
                tools = ?allowed_tools,
                "allowed_tools 暂无 ACP 直接映射，依赖 cwd 锁定 + 权限审批收敛"
            );
        }

        // 3. 连接 + 跑完一个 turn。connect_with 的闭包返回
        //    `Result<TurnOutcome, agent_client_protocol::Error>`。
        let turn = Client
            .builder()
            // session/update 通知 → 转 AgentChunk + 累计文本。
            .on_receive_notification(
                async move |notification: SessionNotification, _cx: ConnectionTo<_>| {
                    forward_update(&notif_state, notification.update);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            // session/request_permission 请求 → 按 PermissionMode 自动响应（MVP）。
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, _cx: ConnectionTo<_>| {
                    let outcome = permission_outcome(&request, &perm_mode);
                    responder.respond(RequestPermissionResponse::new(outcome))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<_>| async move {
                // 3a. initialize：协商协议版本（V1）。
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                // 3b. 建会话或续接。cwd 锁定为 workdir（绝对路径），构成安全边界。
                //     loadSession 响应不含 session_id，复用入参 id；newSession 返回新 id。
                let session_id = if let Some(s) = session {
                    s.0.clone()
                } else {
                    connection
                        .send_request(NewSessionRequest::new(cwd.clone()))
                        .block_task()
                        .await?
                        .session_id
                        .to_string()
                };

                // session=None 时上面已 new；session=Some 时此处 load 续接（带 cwd 锁定）。
                if session.is_some() {
                    connection
                        .send_request(LoadSessionRequest::new(session_id.clone(), cwd.clone()))
                        .block_task()
                        .await?;
                }

                // 3c. 发 prompt 触发 turn。期间 Agent 的 session/update 由上面的
                //     notification handler 处理（在 dispatch loop 的独立 task 上）。
                let prompt_blocks = vec![ContentBlock::Text(TextContent::new(prompt))];
                let prompt_resp = connection
                    .send_request(PromptRequest::new(
                        agent_client_protocol::schema::v1::SessionId::new(session_id.clone()),
                        prompt_blocks,
                    ))
                    .block_task()
                    .await?;

                Ok(TurnOutcome {
                    session_id,
                    stop_reason: prompt_resp.stop_reason,
                })
            })
            .await
            .map_err(|e| CoreError::Backend(NAME, format!("acp turn 失败: {e}")))?;

        // 4. 取累积的 agent 文本作为 final_text。
        let final_text = state.agent_text.lock().await.clone();

        // 5. 推送 Final（与 RunOutcome.final_text 一致），与 CLI backend 行为对齐。
        let _ = state
            .chunks
            .send(AgentChunk::Final(final_text.clone()))
            .await;

        Ok(RunOutcome {
            session_id: SessionId(turn.session_id),
            final_text,
        })
    }
}

/// turn 结束后从闭包返回的轻量结果（避免在闭包里构造 imagent 类型）。
struct TurnOutcome {
    session_id: String,
    #[allow(dead_code)]
    stop_reason: StopReason,
}

/// 把一个 [`SessionUpdate`] 转成对应的 [`AgentChunk`] 推入 core 通道，并把 agent 文本
/// 累积到共享状态（供 final_text）。
///
/// 映射策略（best-effort，未知变体忽略并 debug 记录）：
/// - `AgentMessageChunk`/`UserMessageChunk`（文本）→ `AgentChunk::Text`（仅 Agent 文本累计）。
/// - `ToolCall` → `AgentChunk::ToolUse`（title 作 tool 名，raw_input 作输入）。
/// - `ToolCallUpdate`（带输出）→ `AgentChunk::ToolResult`。
/// - 其它（Plan/UsageUpdate/...）→ 忽略。
fn forward_update(state: &StreamState, update: SessionUpdate) {
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
        // try_send 避免在 dispatch loop 上跨 await 阻塞；通道满则丢弃该 chunk（best-effort）。
        let _ = state.chunks.try_send(chunk);
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
/// - `Ask`（IM 审批闭环）→ **TODO**：需接 core 的 PermissionRouter 转 IM 询问，MVP 先按
///   allow 放行并 warn。
fn permission_outcome(
    request: &RequestPermissionRequest,
    mode: &RwLock<PermissionMode>,
) -> RequestPermissionOutcome {
    let mode = *mode.read();
    match mode {
        PermissionMode::Deny => select_option(&request.options, false)
            .map(|id| RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)))
            .unwrap_or(RequestPermissionOutcome::Cancelled),
        PermissionMode::Allow | PermissionMode::Off => allow_outcome(&request.options),
        PermissionMode::Ask => {
            // TODO: Ask 模式应经 PermissionRouter 发 IM 询问用户，等待回复路由回此响应。
            //   MVP 先按 allow 放行，保证功能可用。
            warn!(
                target: "claude-acp",
                "Ask 权限模式尚未接入 IM 审批闭环，MVP 按 allow 放行"
            );
            allow_outcome(&request.options)
        }
    }
}

/// 选一个 allow 类选项构造 outcome；若无 allow 选项则取首个；都没有则 Cancelled。
fn allow_outcome(options: &[PermissionOption]) -> RequestPermissionOutcome {
    select_option(options, true)
        .map(|id| RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)))
        .unwrap_or(RequestPermissionOutcome::Cancelled)
}

/// 在权限选项中挑一个：`allow=true` 时优先 Allow*，否则优先 Reject*；都找不到则取首个。
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
        .or_else(|| options.first())
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

    #[test]
    fn forward_update_agent_message_accumulates_text() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentChunk>(16);
        let state = StreamState::new(tx);

        forward_update(
            &state,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new("hi "),
            ))),
        );
        forward_update(
            &state,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new("there"),
            ))),
        );

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

    #[test]
    fn forward_update_tool_call_emits_tool_use() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentChunk>(16);
        let state = StreamState::new(tx);

        let mut call = ToolCall::new(ToolCallId::new("tc-1"), "Read".to_string());
        call.raw_input = Some(serde_json::json!({"path": "/tmp/a"}));
        forward_update(&state, SessionUpdate::ToolCall(call));
        // ToolUse 已 try_send（通道容量足够，不 panic 即通过）。
    }

    #[test]
    fn forward_update_tool_call_update_with_output_emits_result() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentChunk>(16);
        let state = StreamState::new(tx);

        let upd = ToolCallUpdate::new(
            "tc-2",
            ToolCallUpdateFields::default().raw_output(serde_json::json!({"lines": 3})),
        );
        forward_update(&state, SessionUpdate::ToolCallUpdate(upd));

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
