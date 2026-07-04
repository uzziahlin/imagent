//! Backend 共享工具：三 CLI backend（claude/codex/gemini）共用的脚手架与常量，
//! 消除 `run()` 重复（见 `docs/CODE_REVIEW.md` P1-1）。
//!
//! 设计：三 backend 的 `run()` 收缩为「构造 cmd + 适配闭包（自己的 parse_line →
//! [`CliEvent`]）+ 调 [`spawn_cli_backend`]」。各 backend 的 `stream::parse_line`
//! 保持不变（各自 `ParsedEvent`），由 backend 的适配闭包映射到统一的 [`CliEvent`]。

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};

use crate::error::{CoreError, Result};
use crate::types::{AgentChunk, RunOutcome, SessionId};

/// 三 CLI backend 的 stdout 行解析统一事件。各 backend 的适配闭包把自己的
/// `ParsedEvent` 映射到它。
#[derive(Debug, Clone)]
pub enum CliEvent {
    /// 纯 session 捕获（首次有效；如 claude Other/codex ThreadStarted/gemini Init）。
    Session(String),
    /// 中间文本（best-effort 推 IM；codex AgentMessage / gemini AssistantMessage）。
    /// 同时作为 final_text 候选（最后一次非空胜出）。
    Text(String),
    /// 工具调用。`session` 供尽早捕获 session_id（如 claude ToolUse 带 session）。
    ToolUse {
        tool: String,
        input: String,
        session: Option<String>,
    },
    /// 工具结果。
    ToolResult { tool: String, output: String },
    /// 终止 + 最终文本（claude `result` 非 error）。
    Final {
        text: String,
        session: Option<String>,
    },
    /// 终止 + 错误（claude `result` is_error / codex TurnFailed / gemini Error）。
    Error {
        text: String,
        session: Option<String>,
    },
    /// 终止信号无文本（codex TurnCompleted / gemini Result）；final 取最后 Text。
    Terminal { session: Option<String> },
    /// 非 JSON / 噪声，跳过。
    Skip,
}

/// 泛型 run 脚手架：spawn cmd → kill_on_drop + stdin null → 并发 stderr →
/// 读 stdout 循环（调 `parse` 映射到 [`CliEvent`]）→ session/final/error 收集 →
/// RunOutcome。三 CLI backend 共用，零行为差异（仅去重）。
///
/// `cmd` 由调用方构造好（cwd/args 已设；本函数统一加 stdin/stdout/stderr/kill_on_drop）。
/// `parse` 是各 backend 的「行 → CliEvent」适配闭包。`backend_name` 用于错误信息。
pub async fn spawn_cli_backend(
    mut cmd: tokio::process::Command,
    parse: impl Fn(&str) -> CliEvent,
    chunks: tokio::sync::mpsc::Sender<AgentChunk>,
    backend_name: &'static str,
) -> Result<RunOutcome> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| CoreError::Backend(backend_name, format!("failed to spawn: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CoreError::Backend(backend_name, "stdout not piped".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CoreError::Backend(backend_name, "stderr not piped".into()))?;

    // 并发读 stderr，避免子进程 stderr 写满管道缓冲（~64KB）导致死锁。
    let stderr_handle = tokio::spawn(async move { read_stderr_to_string(stderr).await });

    let mut reader = BufReader::new(stdout).lines();
    let mut session_id = String::new();
    let mut final_text = String::new();
    let mut error_text: Option<String> = None;
    let mut reached_terminal = false;

    while let Ok(Some(line)) = reader.next_line().await {
        match parse(&line) {
            CliEvent::Session(id) => {
                if session_id.is_empty() {
                    session_id = id;
                }
            }
            CliEvent::Text(t) => {
                if !t.is_empty() {
                    let _ = chunks.send(AgentChunk::Text(t.clone())).await;
                    final_text = t;
                }
            }
            CliEvent::ToolUse {
                tool,
                input,
                session,
            } => {
                if let Some(s) = session {
                    if session_id.is_empty() {
                        session_id = s;
                    }
                }
                let _ = chunks.send(AgentChunk::ToolUse { tool, input }).await;
            }
            CliEvent::ToolResult { tool, output } => {
                let _ = chunks.send(AgentChunk::ToolResult { tool, output }).await;
            }
            CliEvent::Final { text, session } => {
                if let Some(s) = session {
                    session_id = s;
                }
                final_text = text;
                break;
            }
            CliEvent::Error { text, session } => {
                if let Some(s) = session {
                    session_id = s;
                }
                error_text = Some(text);
                break;
            }
            CliEvent::Terminal { session } => {
                if let Some(s) = session {
                    session_id = s;
                }
                reached_terminal = true;
                break;
            }
            CliEvent::Skip => {}
        }
    }

    let status = child.wait().await;
    let stderr_msg = stderr_handle.await.unwrap_or_default();

    if let Some(t) = error_text {
        let _ = chunks.send(AgentChunk::Error(t.clone())).await;
        return Err(CoreError::Backend(backend_name, t));
    }

    if final_text.is_empty() {
        return Err(CoreError::Backend(
            backend_name,
            diagnose(&status, &stderr_msg, backend_name, reached_terminal),
        ));
    }

    let _ = chunks.send(AgentChunk::Final(final_text.clone())).await;
    Ok(RunOutcome {
        session_id: SessionId(session_id),
        final_text,
    })
}

/// 无最终文本时的诊断：区分「正常终止但无文本」与「未终止/异常」。
fn diagnose(
    status: &std::io::Result<std::process::ExitStatus>,
    stderr: &str,
    name: &str,
    reached_terminal: bool,
) -> String {
    let code = match status {
        Ok(s) => format!("exit {s}"),
        Err(e) => format!("wait failed: {e}"),
    };
    let stderr_trim = stderr.trim();
    let head = if reached_terminal {
        format!("{name} terminated without text ({code})")
    } else {
        format!("{name} produced no result event ({code})")
    };
    if stderr_trim.is_empty() {
        head
    } else {
        format!("{head}; stderr: {stderr_trim}")
    }
}

/// 把子进程 stderr 全量读到字符串（非阻断）。三 CLI backend 共享。
pub async fn read_stderr_to_string(stderr: tokio::process::ChildStderr) -> String {
    let mut reader = BufReader::new(stderr).lines();
    let mut buf = Vec::new();
    while let Ok(Some(line)) = reader.next_line().await {
        buf.push(line);
    }
    buf.join("\n")
}

/// 视为需要写/执行权限的工具名（codex sandbox / gemini approval 收敛用，大小写敏感）。
/// codex/gemini 原各自定义且逐字相同。
pub const WRITE_OR_EXEC: &[&str] = &[
    "Edit",
    "Write",
    "Bash",
    "MultiEdit",
    "NotebookEdit",
    "WriteQuery",
    "execute_bash",
];
