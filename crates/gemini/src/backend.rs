//! [`GeminiBackend`]：基于 Google Gemini CLI 的无状态 agent 执行器。
//!
//! 与 [`imagent_codex::CodexBackend`](../../imagent_codex/backend/struct.CodexBackend.html)
//! 同构：spawn `gemini -p -o stream-json` 子进程，逐行解析 JSONL，捕获
//! `session_id` 作为 session id，流式推送 `AgentChunk`，返回 `RunOutcome`。

use std::process::Stdio;

use async_trait::async_trait;
use imagent_core::{AgentChunk, Backend, CoreError, Result, RunOutcome, SessionId};
use tokio::io::{AsyncBufReadExt, BufReader};
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

        // 1. 构造命令。workdir 锁定（安全边界）。
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
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| CoreError::Backend(NAME, format!("failed to spawn `gemini`: {e}")))?;

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // 2. 逐行读 stdout，喂给 parse_line。
        let mut reader = BufReader::new(stdout).lines();

        let mut captured_session: Option<String> = None;
        let mut final_text: Option<String> = None;
        let mut error_text: Option<String> = None;
        let mut turn_completed = false;

        while let Ok(Some(line)) = reader.next_line().await {
            match parse_line(&line) {
                ParsedEvent::Skip => {}
                ParsedEvent::Init { session_id, model } => {
                    if captured_session.is_none() {
                        captured_session = Some(session_id);
                    }
                    debug!(target: "imagent::gemini", ?model, "gemini init");
                }
                ParsedEvent::AssistantMessage { text } => {
                    // best-effort 推中间文本；final 取最后一条非空 assistant message。
                    let _ = chunks.send(AgentChunk::Text(text.clone())).await;
                    if !text.is_empty() {
                        final_text = Some(text);
                    }
                }
                ParsedEvent::ToolUse { tool, input } => {
                    let _ = chunks.send(AgentChunk::ToolUse { tool, input }).await;
                }
                ParsedEvent::ToolResult { tool, output } => {
                    let _ = chunks.send(AgentChunk::ToolResult { tool, output }).await;
                }
                ParsedEvent::Result => {
                    turn_completed = true;
                    break;
                }
                ParsedEvent::Error { message } => {
                    let _ = chunks.send(AgentChunk::Error(message.clone())).await;
                    error_text = Some(message);
                    break;
                }
                ParsedEvent::Other => {
                    debug!(target: "imagent::gemini", line = %line, "stream event (ignored)");
                }
            }
        }

        // 3. 进程退出码。读取循环可能因终止事件提前 break，仍需 join。
        let output_status = child.wait().await;
        let stderr_msg = read_stderr_to_string(stderr).await;

        // 4. 错误优先级：error 事件 message > 无 final 文本诊断。
        if let Some(message) = error_text {
            return Err(CoreError::Backend(NAME, message));
        }

        let final_text = match final_text {
            Some(t) if !t.is_empty() => t,
            _ => {
                // 没拿到 assistant message 文本：依终止状态诊断。
                let diag = diagnose(&output_status, &stderr_msg, turn_completed);
                return Err(CoreError::Backend(NAME, diag));
            }
        };

        let session_id = captured_session.unwrap_or_default();
        // 5. 推送 Final（与 RunOutcome.final_text 一致）。
        let _ = chunks.send(AgentChunk::Final(final_text.clone())).await;

        Ok(RunOutcome {
            session_id: SessionId(session_id),
            final_text,
        })
    }
}

/// 把子进程 stderr 全量读到字符串（不阻断）。
async fn read_stderr_to_string(stderr: tokio::process::ChildStderr) -> String {
    let mut reader = BufReader::new(stderr).lines();
    let mut buf = Vec::new();
    while let Ok(Some(line)) = reader.next_line().await {
        buf.push(line);
    }
    buf.join("\n")
}

/// 无 final 文本时的诊断信息。
///
/// - `turn_completed`：turn 正常结束但无 assistant message（罕见，如纯工具调用 turn）。
/// - 否则依退出码 + stderr 诊断。
fn diagnose(
    status: &std::io::Result<std::process::ExitStatus>,
    stderr: &str,
    turn_completed: bool,
) -> String {
    let code_str = match status {
        Ok(s) => format!("exit {}", s),
        Err(e) => format!("wait failed: {e}"),
    };
    let stderr_trim = stderr.trim();
    if turn_completed {
        return format!("gemini turn completed but produced no assistant message ({code_str})");
    }
    if stderr_trim.is_empty() {
        format!("gemini produced no turn result ({code_str})")
    } else {
        format!("gemini produced no turn result ({code_str}); stderr: {stderr_trim}")
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
    /// 视为需要写/执行权限的工具名（大小写敏感匹配 imagent 工具命名）。
    const WRITE_OR_EXEC: &[&str] = &[
        "Edit",
        "Write",
        "Bash",
        "MultiEdit",
        "NotebookEdit",
        "WriteQuery",
        "execute_bash",
    ];
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
