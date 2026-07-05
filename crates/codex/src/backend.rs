//! [`CodexBackend`]：基于 OpenAI Codex CLI 的无状态 agent 执行器。
//!
//! 与 [`imagent_claude::ClaudeBackend`](../../imagent_claude/backend/struct.ClaudeBackend.html)
//! 同构：spawn `codex exec --json` 子进程，逐行解析 JSONL，捕获 `thread_id`
//! 作为 session id，流式推送 `AgentChunk`，返回 `RunOutcome`。

use async_trait::async_trait;
use imagent_core::{
    backend_common::{spawn_cli_backend, CliEvent, WRITE_OR_EXEC},
    AgentChunk, Backend, Result, RunOutcome, SessionId,
};
use tokio::process::Command;
use tracing::debug;

use crate::stream::{parse_line, ParsedEvent};

/// OpenAI Codex CLI 后端。
///
/// MVP 无状态、不做 IM 权限审批闭环（Codex 自身有沙箱模型兜底）。
pub struct CodexBackend;

impl CodexBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CodexBackend {
    fn default() -> Self {
        Self::new()
    }
}

const NAME: &str = "codex";

#[async_trait]
impl Backend for CodexBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn run(
        &self,
        conv_id: &str,
        prompt: &str,
        session: Option<&SessionId>,
        workdir: &std::path::Path,
        allowed_tools: &[String],
        chunks: tokio::sync::mpsc::Sender<AgentChunk>,
    ) -> Result<RunOutcome> {
        debug!(target: "imagent::codex", conv_id, "codex run start");
        // 构造命令（workdir 锁定，prompt 单 arg 不经 shell；stdin/stdout/stderr/kill_on_drop
        // 由 spawn_cli_backend 统一加）。
        let sandbox_mode = pick_sandbox(allowed_tools);
        let mut cmd = Command::new("codex");
        cmd.current_dir(workdir);
        if let Some(s) = session {
            // 续接：codex exec resume <thread_id> <prompt>。
            // P2-J：prompt 经 `--` 分隔为纯 positional，防止 prompt 以 `-` 开头
            // 被误解析为 flag（参数注入）。
            cmd.arg("exec")
                .arg("resume")
                .arg(&s.0)
                .arg("--json")
                .arg("--skip-git-repo-check")
                .arg("--")
                .arg(prompt);
        } else {
            cmd.arg("exec")
                .arg("--json")
                .arg("--skip-git-repo-check")
                .arg("--")
                .arg(prompt);
        }
        cmd.arg("-s").arg(sandbox_mode);
        spawn_cli_backend(cmd, codex_parse, chunks, NAME).await
    }
}

/// codex JSONL 行 → [`CliEvent`] 适配（见 [`parse_line`]）。
fn codex_parse(line: &str) -> CliEvent {
    match parse_line(line) {
        ParsedEvent::ThreadStarted { thread_id } => CliEvent::Session(thread_id),
        ParsedEvent::Other { thread_id } => thread_id.map_or(CliEvent::Skip, CliEvent::Session),
        ParsedEvent::AgentMessage { text } => CliEvent::Text(text),
        ParsedEvent::ToolUse { tool, input } => CliEvent::ToolUse {
            tool,
            input,
            session: None,
        },
        ParsedEvent::ToolResult { tool, output } => CliEvent::ToolResult { tool, output },
        ParsedEvent::TurnCompleted => CliEvent::Terminal { session: None },
        ParsedEvent::TurnFailed { message } => CliEvent::Error {
            text: message,
            session: None,
        },
        ParsedEvent::Error { message: _ } => CliEvent::Skip, // 顶层 error 可能瞬时重连，忽略（原 warn）
        ParsedEvent::Skip => CliEvent::Skip,
    }
}

/// 把 imagent 的 `allowed_tools` 收敛到 codex 的沙箱模式。
///
/// imagent 的 `allowed_tools`（如 `["Read","Edit"]`）与 codex 的沙箱模型
/// 非一一对应：codex 沙箱只有 `read-only` / `workspace-write` /
/// `danger-full-access` 三档。此处 best-effort 收敛：
/// - 含写/执行类工具 → `workspace-write`；
/// - 否则 → `read-only`（最安全默认）。
///
/// **绝不**自动选 `danger-full-access`。
fn pick_sandbox(allowed_tools: &[String]) -> &'static str {
    // WRITE_OR_EXEC 见 imagent_core::backend_common（codex/gemini 共享）。
    let needs_write = allowed_tools
        .iter()
        .any(|t| WRITE_OR_EXEC.contains(&t.as_str()));
    if needs_write {
        "workspace-write"
    } else {
        "read-only"
    }
}

// TODO(P?): Codex IM 权限审批闭环——当前仅依赖 codex 沙箱 + workdir 锁定兜底，
// 未实现类似 ClaudeBackend 的 MCP 回调式 IM 审批。codex exec 暂无等价的
// --permission-prompt-tool 机制。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_read_only_by_default() {
        assert_eq!(pick_sandbox(&[]), "read-only");
        assert_eq!(pick_sandbox(&["Read".into(), "Grep".into()]), "read-only");
    }

    #[test]
    fn sandbox_workspace_write_when_edit_present() {
        assert_eq!(
            pick_sandbox(&["Read".into(), "Edit".into()]),
            "workspace-write"
        );
        assert_eq!(pick_sandbox(&["Bash".into()]), "workspace-write");
        assert_eq!(pick_sandbox(&["MultiEdit".into()]), "workspace-write");
    }

    #[test]
    fn name_is_codex() {
        assert_eq!(CodexBackend::new().name(), "codex");
    }
}
