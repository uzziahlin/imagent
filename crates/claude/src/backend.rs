//! [`ClaudeBackend`]：基于 Claude Code CLI 的无状态 agent 执行器。

use std::process::Stdio;

use async_trait::async_trait;
use imagent_core::{AgentChunk, Backend, CoreError, Result, RunOutcome, SessionId};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::debug;

use crate::stream::{parse_line, ParsedEvent};

/// Claude Code CLI 后端。
///
/// P1 保持最小：无配置字段（model / system_prompt 等 P2 再加）。
/// 通过 spawn `claude -p` 子进程执行任务，`stream-json` 逐行解析。
pub struct ClaudeBackend;

impl ClaudeBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ClaudeBackend {
    fn default() -> Self {
        Self::new()
    }
}

const NAME: &str = "claude-cli";

#[async_trait]
impl Backend for ClaudeBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn run(
        &self,
        prompt: &str,
        session: Option<&SessionId>,
        workdir: &std::path::Path,
        allowed_tools: &[String],
        chunks: tokio::sync::mpsc::Sender<AgentChunk>,
    ) -> Result<RunOutcome> {
        // 1. 构造命令。prompt 作为单个 arg 传入（不经 shell，安全）。
        let mut cmd = Command::new("claude");
        cmd.current_dir(workdir)
            .arg("-p")
            .arg(prompt)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--allowedTools")
            .arg(allowed_tools.join(","));
        if let Some(s) = session {
            cmd.arg("--resume").arg(&s.0);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            CoreError::Backend(
                NAME,
                format!("failed to spawn `claude`: {e}"),
            )
        })?;

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // 2. 逐行读 stdout，喂给 parse_line。
        let mut reader = BufReader::new(stdout).lines();

        let mut captured_session: Option<String> = None;
        let mut final_text: Option<String> = None;
        let mut error_text: Option<String> = None;

        while let Ok(Some(line)) = reader.next_line().await {
            match parse_line(&line) {
                ParsedEvent::Skip => {}
                ParsedEvent::Other { session_id } => {
                    // 尽早捕获 session_id（但 result 事件的优先级更高）。
                    if captured_session.is_none() {
                        captured_session = session_id;
                    }
                    debug!(line = %line, "stream event (ignored in MVP)");
                }
                ParsedEvent::Result {
                    text,
                    is_error,
                    session_id,
                } => {
                    // result 事件的 session_id 优先。
                    if let Some(sid) = session_id {
                        captured_session = Some(sid);
                    }
                    if is_error {
                        error_text = Some(text.clone());
                    } else {
                        final_text = Some(text);
                    }
                    // result 是终止事件，停止读取。
                    break;
                }
            }
        }

        // 进程退出码。读取循环可能因 result 提前 break，仍需 join 拿退出码。
        let output_status = child.wait().await;

        // 把 stderr 收集出来用于诊断（仅在异常路径读取）。
        let stderr_msg = read_stderr_to_string(stderr).await;

        // 3. 错误优先级：is_error result > 非零退出码。
        if let Some(text) = error_text {
            // 通知下游出错，再返回 Err。
            let _ = chunks.send(AgentChunk::Error(text.clone())).await;
            return Err(CoreError::Backend(NAME, text));
        }

        let final_text = match final_text {
            Some(t) if !t.is_empty() => t,
            _ => {
                // 没拿到 result 文本：依退出码诊断。
                let diag = diagnose(&output_status, &stderr_msg);
                return Err(CoreError::Backend(NAME, diag));
            }
        };

        let session_id = captured_session.unwrap_or_default();
        // 4. 推送 Final（与 RunOutcome.final_text 一致）。
        let _ = chunks.send(AgentChunk::Final(final_text.clone())).await;

        Ok(RunOutcome {
            session_id: SessionId(session_id),
            final_text,
        })
    }
}

/// 把子进程 stderr 全量读到字符串（不阻断）。
async fn read_stderr_to_string(
    stderr: tokio::process::ChildStderr,
) -> String {
    let mut reader = BufReader::new(stderr).lines();
    let mut buf = Vec::new();
    while let Ok(Some(line)) = reader.next_line().await {
        buf.push(line);
    }
    buf.join("\n")
}

/// 无 result 文本时的诊断信息。
fn diagnose(
    status: &std::io::Result<std::process::ExitStatus>,
    stderr: &str,
) -> String {
    let code_str = match status {
        Ok(s) => format!("exit {}", s),
        Err(e) => format!("wait failed: {e}"),
    };
    let stderr_trim = stderr.trim();
    if stderr_trim.is_empty() {
        format!("claude produced no result event ({code_str})")
    } else {
        format!("claude produced no result event ({code_str}); stderr: {stderr_trim}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_stable() {
        let b = ClaudeBackend::new();
        assert_eq!(b.name(), "claude-cli");
    }

    /// 真跑 `claude` CLI 的集成测试。标 `#[ignore]` 以免 CI 依赖 claude。
    ///
    /// 运行：`cargo test -p imagent-claude -- --ignored real_cli`
    #[tokio::test]
    #[ignore]
    async fn real_cli_replies_with_text_and_session() {
        let backend = ClaudeBackend::new();
        let workdir = std::env::current_dir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentChunk>(64);
        let allowed: Vec<String> = vec![];

        let outcome = backend
            .run(
                "reply with the single word: pong",
                None,
                &workdir,
                &allowed,
                tx,
            )
            .await
            .expect("claude run should succeed");

        assert!(!outcome.session_id.0.is_empty(), "session_id must be non-empty");
        assert!(
            !outcome.final_text.trim().is_empty(),
            "final_text must be non-empty"
        );

        // 应至少收到一个 Final chunk。
        let mut got_final = false;
        while let Ok(chunk) = rx.try_recv() {
            if let AgentChunk::Final(t) = chunk {
                assert_eq!(t, outcome.final_text);
                got_final = true;
            }
        }
        assert!(got_final, "expected a Final chunk");
    }
}
