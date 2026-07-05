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
        // P1-1：-s 必须在 `--` 之前——clap 的 `--` 之后全是 positional，codex exec
        // 用 trailing_var_arg 收集 prompt，原实现把 -s 放在 -- 之后导致 sandbox 模式
        // 被并入 prompt 字符串、从未生效（退到默认 read-only，用户配 Edit/Write 仍写不了）。
        let args = codex_args(session, sandbox_mode, prompt);
        let mut cmd = Command::new("codex");
        cmd.current_dir(workdir);
        cmd.args(args);
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

/// 构造 `codex exec` 的 arg 序列（不含 program name），抽成纯函数便于单测顺序。
///
/// - resume 续接：`exec resume <thread_id> <opts> -- <prompt>`；
/// - 新建：`exec <opts> -- <prompt>`。
///
/// **P1-1**：`-s <sandbox>` 必须在 `--` **之前**（options 区）；`--` 之后只留 prompt
/// 作纯 positional（P2-J：防 prompt 以 `-` 开头被误解析为 flag）。
fn codex_args(session: Option<&SessionId>, sandbox_mode: &str, prompt: &str) -> Vec<String> {
    let mut args: Vec<String> = vec!["exec".into()];
    if let Some(s) = session {
        args.push("resume".into());
        args.push(s.0.clone());
    }
    args.push("--json".into());
    args.push("--skip-git-repo-check".into());
    args.push("-s".into());
    args.push(sandbox_mode.into());
    args.push("--".into());
    args.push(prompt.into());
    args
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

    #[test]
    fn sandbox_flag_precedes_dashdash_new_session() {
        // P1-1：-s 必须在 -- 之前，否则被 codex 当 positional 并入 prompt。
        let args = codex_args(None, "workspace-write", "hello");
        let dashdash = args.iter().position(|a| a == "--").unwrap();
        let s_idx = args.iter().position(|a| a == "-s").unwrap();
        assert!(
            s_idx < dashdash,
            "-s must precede -- (got -s@{s_idx}, --@{dashdash})"
        );
        assert_eq!(args[s_idx + 1], "workspace-write");
        assert_eq!(args[dashdash + 1], "hello");
    }

    #[test]
    fn sandbox_flag_precedes_dashdash_resume() {
        let sid = SessionId("thread-123".into());
        let args = codex_args(Some(&sid), "read-only", "do thing");
        let dashdash = args.iter().position(|a| a == "--").unwrap();
        let s_idx = args.iter().position(|a| a == "-s").unwrap();
        assert!(s_idx < dashdash);
        // resume 分支：exec resume <thread_id>
        let resume_idx = args.iter().position(|a| a == "resume").unwrap();
        assert_eq!(args[resume_idx + 1], "thread-123");
        assert_eq!(args[dashdash + 1], "do thing");
    }
}
