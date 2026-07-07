//! [`GeminiBackend`]：基于 Google Gemini CLI 的无状态 agent 执行器。
//!
//! 与 [`imagent_codex::CodexBackend`](../../imagent_codex/backend/struct.CodexBackend.html)
//! 同构：spawn `gemini -p -o stream-json` 子进程，逐行解析 JSONL，捕获
//! `session_id` 作为 session id，流式推送 `AgentChunk`，返回 `RunOutcome`。

use async_trait::async_trait;
use imagent_core::{
    backend_common::{spawn_cli_backend, CliEvent, WRITE_OR_EXEC},
    AgentChunk, Backend, Result, RunOutcome, SessionId,
};
use tokio::process::Command;
use tracing::debug;

use crate::stream::{parse_line, ParsedEvent};

/// Google Gemini CLI 后端。
///
/// MVP 无状态、不做 IM 权限审批闭环（依赖 approval-mode + workdir 锁定兜底）。
pub struct GeminiBackend;

impl GeminiBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GeminiBackend {
    fn default() -> Self {
        Self::new()
    }
}

const NAME: &str = "gemini";

#[async_trait]
impl Backend for GeminiBackend {
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
        debug!(target: "imagent::gemini", conv_id, "gemini run start");
        // 构造命令（workdir 锁定；stdin/stdout/stderr/kill_on_drop 由 spawn_cli_backend 统一加）。
        let (approval, sandbox) = pick_approval(allowed_tools);
        let mut cmd = Command::new("gemini");
        cmd.current_dir(workdir);
        // -p：headless 模式开关。
        cmd.arg("-p").arg("-o").arg("stream-json");
        // 续接：--resume <session_id>（显式 id 有效）。
        if let Some(s) = session {
            cmd.arg("--resume").arg(&s.0);
        }
        // 权限收敛：approval-mode（绝不自动 yolo）。
        cmd.arg("--approval-mode").arg(approval);
        if sandbox {
            cmd.arg("-s");
        }
        // headless 必需：信任当前 workspace，否则 trustedFolders 拒绝。
        cmd.arg("--skip-trust");
        // prompt 绑定到 flag（防止 prompt 以 `-` 开头被误解析）。
        cmd.arg(format!("--prompt={prompt}"));
        spawn_cli_backend(
            cmd,
            gemini_parse,
            chunks,
            NAME,
            // S-2：仅透传 gemini(Google) 所需凭据（最小授权）。
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        )
        .await
    }
}

/// gemini stream-json 行 → [`CliEvent`] 适配（见 [`parse_line`]）。
fn gemini_parse(line: &str) -> CliEvent {
    match parse_line(line) {
        ParsedEvent::Init {
            session_id,
            model: _,
        } => CliEvent::Session(session_id),
        ParsedEvent::AssistantMessage { text } => {
            if text.is_empty() {
                CliEvent::Skip
            } else {
                CliEvent::Text(text)
            }
        }
        ParsedEvent::ToolUse { tool, input } => CliEvent::ToolUse {
            tool,
            input,
            session: None,
        },
        ParsedEvent::ToolResult { tool, output } => CliEvent::ToolResult { tool, output },
        ParsedEvent::Result => CliEvent::Terminal { session: None },
        ParsedEvent::Error { message } => CliEvent::Error {
            text: message,
            session: None,
        },
        ParsedEvent::Other => CliEvent::Skip,
        ParsedEvent::Skip => CliEvent::Skip,
    }
}

/// 把 imagent 的 `allowed_tools` 收敛到 gemini 的 approval-mode + sandbox。
///
/// imagent 的 `allowed_tools`（如 `["Read","Edit"]`）与 gemini 的 approval 模型
/// 非一一对应：gemini approval-mode 有 `default` / `auto_edit` / `yolo` / `plan`
/// 四档。此处 best-effort 收敛：
/// - 含写/执行类工具 → `auto_edit`（允许编辑，仍非 yolo）；
/// - 否则（仅读类）→ `plan`（只读）+ 同时加 `--sandbox` 双重收敛。
///
/// 返回 `(approval_mode, want_sandbox)`。**绝不自动选 `yolo`**（全自动=危险）。
fn pick_approval(allowed_tools: &[String]) -> (&'static str, bool) {
    // WRITE_OR_EXEC 见 imagent_core::backend_common（codex/gemini 共享）。
    let needs_write = allowed_tools
        .iter()
        .any(|t| WRITE_OR_EXEC.contains(&t.as_str()));
    if needs_write {
        ("auto_edit", false)
    } else {
        ("plan", true)
    }
}

// TODO(P?): Gemini IM 权限审批闭环——MVP 不做，依赖 approval_mode + workdir 锁定兜底；
// gemini headless 无等价的 IM 审批回调机制。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_plan_read_only_by_default() {
        assert_eq!(pick_approval(&[]), ("plan", true));
        assert_eq!(
            pick_approval(&["Read".into(), "Grep".into()]),
            ("plan", true)
        );
    }

    #[test]
    fn approval_auto_edit_when_edit_present() {
        assert_eq!(
            pick_approval(&["Read".into(), "Edit".into()]),
            ("auto_edit", false)
        );
        assert_eq!(pick_approval(&["Bash".into()]), ("auto_edit", false));
        assert_eq!(pick_approval(&["MultiEdit".into()]), ("auto_edit", false));
    }

    #[test]
    fn never_yolo() {
        // 即使有全部写工具，也不选 yolo。
        let (mode, _) = pick_approval(&["Edit".into(), "Write".into(), "Bash".into()]);
        assert_ne!(mode, "yolo");
    }

    #[test]
    fn name_is_gemini() {
        assert_eq!(GeminiBackend::new().name(), "gemini");
    }
}
