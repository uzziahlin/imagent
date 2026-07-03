//! [`CodexBackend`]：基于 OpenAI Codex CLI 的无状态 agent 执行器。
//!
//! 与 [`imagent_claude::ClaudeBackend`](../../imagent_claude/backend/struct.ClaudeBackend.html)
//! 同构：spawn `codex exec --json` 子进程，逐行解析 JSONL，捕获 `thread_id`
//! 作为 session id，流式推送 `AgentChunk`，返回 `RunOutcome`。

use std::process::Stdio;

use async_trait::async_trait;
use imagent_core::{AgentChunk, Backend, CoreError, Result, RunOutcome, SessionId};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{debug, warn};

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

        // 1. 构造命令。workdir 锁定（安全边界），prompt 作为单个 arg（不经 shell）。
        let sandbox_mode = pick_sandbox(allowed_tools);
        let mut cmd = Command::new("codex");
        cmd.current_dir(workdir);
        if let Some(s) = session {
            // 续接：codex exec resume <thread_id> <prompt>
            cmd.arg("exec")
                .arg("resume")
                .arg(&s.0)
                .arg("--json")
                .arg(prompt)
                .arg("--skip-git-repo-check");
        } else {
            cmd.arg("exec")
                .arg("--json")
                .arg(prompt)
                .arg("--skip-git-repo-check");
        }
        cmd.arg("-s").arg(sandbox_mode);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| CoreError::Backend(NAME, format!("failed to spawn `codex`: {e}")))?;

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
                ParsedEvent::ThreadStarted { thread_id } => {
                    if captured_session.is_none() {
                        captured_session = Some(thread_id);
                    }
                }
                ParsedEvent::Other { thread_id } => {
                    if let Some(tid) = thread_id {
                        if captured_session.is_none() {
                            captured_session = Some(tid);
                        }
                    }
                    debug!(target: "imagent::codex", line = %line, "stream event (ignored)");
                }
                ParsedEvent::AgentMessage { text } => {
                    // best-effort 推中间文本；final 取最后一条 agent_message。
                    let _ = chunks.send(AgentChunk::Text(text.clone())).await;
                    final_text = Some(text);
                }
                ParsedEvent::ToolUse { tool, input } => {
                    let _ = chunks.send(AgentChunk::ToolUse { tool, input }).await;
                }
                ParsedEvent::ToolResult { tool, output } => {
                    let _ = chunks.send(AgentChunk::ToolResult { tool, output }).await;
                }
                ParsedEvent::TurnCompleted => {
                    turn_completed = true;
                    break;
                }
                ParsedEvent::TurnFailed { message } => {
                    let _ = chunks.send(AgentChunk::Error(message.clone())).await;
                    error_text = Some(message);
                    break;
                }
                ParsedEvent::Error { message } => {
                    // 顶层 error 可能是瞬时重连（如 "Reconnecting... 1/5"），
                    // 非致命，不中断；仅 warn。
                    warn!(
                        target: "imagent::codex",
                        %message, "codex error event (may be transient)"
                    );
                }
            }
        }

        // 3. 进程退出码。读取循环可能因终止事件提前 break，仍需 join。
        let output_status = child.wait().await;
        let stderr_msg = read_stderr_to_string(stderr).await;

        // 4. 错误优先级：TurnFailed message > 非零退出码诊断。
        if let Some(message) = error_text {
            return Err(CoreError::Backend(NAME, message));
        }

        let final_text = match final_text {
            Some(t) if !t.is_empty() => t,
            _ => {
                // 没拿到 agent_message 文本：依终止状态诊断。
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
/// - `turn_completed`：turn 正常结束但无 agent_message（罕见，如纯工具调用 turn）。
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
        return format!("codex turn completed but produced no agent_message ({code_str})");
    }
    if stderr_trim.is_empty() {
        format!("codex produced no turn result ({code_str})")
    } else {
        format!("codex produced no turn result ({code_str}); stderr: {stderr_trim}")
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
