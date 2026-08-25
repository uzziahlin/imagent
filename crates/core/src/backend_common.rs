//! Backend 共享工具：三 CLI backend（claude/codex/gemini）共用的脚手架与常量，
//! 消除 `run()` 重复（见 `docs/internal/CODE_REVIEW.md` P1-1）。
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
/// 检测「Write 工具写图片文件」：tool 为 Write 且 input JSON 的 file_path
/// 扩展名是图片（png/jpg/jpeg/gif/webp/bmp），返回该路径；否则 None。
/// input 是 stream-json 的工具入参 JSON 字符串；非法 JSON / 缺 file_path 一律 None。
pub(crate) fn image_write_path(tool: &str, input: &str) -> Option<String> {
    if tool != "Write" {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    let path = v.get("file_path")?.as_str()?;
    let lower = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    const IMG_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];
    IMG_EXTS.contains(&lower.as_str()).then(|| path.to_string())
}

/// 泛型 run 脚手架：spawn cmd → kill_on_drop + stdin null → 并发 stderr →
/// 读 stdout 循环（调 `parse` 映射到 [`CliEvent`]）→ session/final/error 收集 →
/// RunOutcome。三 CLI backend 共用，零行为差异（仅去重）。
///
/// `cmd` 由调用方构造好（cwd/args 已设；本函数统一加 stdin/stdout/stderr/kill_on_drop）。
/// `parse` 是各 backend 的「行 → CliEvent」适配闭包。`backend_name` 用于错误信息。
/// `passthrough_env`：S-2——本函数会先 `env_clear()`，再仅透传 [`ALWAYS_PASSTHROUGH_ENV`]
/// 以及调用方声明的这些 key（各 backend 传自己的 API key，最小授权）。传 `&[]` 则只透传
/// 运行时必需变量（PATH/HOME/...）。
pub async fn spawn_cli_backend(
    mut cmd: tokio::process::Command,
    parse: impl Fn(&str) -> CliEvent,
    chunks: tokio::sync::mpsc::Sender<AgentChunk>,
    backend_name: &'static str,
    passthrough_env: &[&str],
) -> Result<RunOutcome> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // S-2：env_clear 防止 agent 子进程继承父进程全部 env（部署环境的 DATABASE_URL /
    // CI secret / 其他工具 token 等，可经 Bash env / /proc/self/environ 被读取并经
    // tool_result 回传 IM 或写入 workdir）。仅透传白名单：运行时必需变量 + 调用方
    // 声明的该后端 API key。未设置的 key 跳过（不向子进程注入空值）。
    cmd.env_clear();
    for &key in ALWAYS_PASSTHROUGH_ENV.iter().chain(passthrough_env.iter()) {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }

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

    let mut reader = BufReader::new(stdout);
    let mut session_id = String::new();
    let mut final_text = String::new();
    let mut error_text: Option<String> = None;
    let mut reached_terminal = false;

    loop {
        let line = match read_line_capped(&mut reader, MAX_STDOUT_LINE_BYTES).await {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(e) => {
                // S-5：单行超 MAX_STDOUT_LINE_BYTES（无 \n 的超长输出）跳过，防 OOM。
                tracing::warn!(
                    target: "imagent::backend",
                    backend = backend_name,
                    error = %e,
                    "stdout 行超长/读失败，跳过该行"
                );
                continue;
            }
        };
        match parse(&line) {
            CliEvent::Session(id) => {
                if session_id.is_empty() && !id.is_empty() {
                    session_id.clone_from(&id);
                    // P5-5：一经学到即通知 dispatch（中断/失败路径也能落库续接）。
                    let _ = chunks.send(AgentChunk::SessionStarted(id)).await;
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
                    if session_id.is_empty() && !s.is_empty() {
                        session_id.clone_from(&s);
                        let _ = chunks.send(AgentChunk::SessionStarted(s)).await;
                    }
                }
                if let Some(path) = image_write_path(&tool, &input) {
                    let _ = chunks.send(AgentChunk::Media { path }).await;
                }
                let _ = chunks.send(AgentChunk::ToolUse { tool, input }).await;
            }
            CliEvent::ToolResult { tool, output } => {
                let _ = chunks.send(AgentChunk::ToolResult { tool, output }).await;
            }
            CliEvent::Final { text, session } => {
                if let Some(s) = session {
                    if session_id.is_empty() && !s.is_empty() {
                        let _ = chunks.send(AgentChunk::SessionStarted(s.clone())).await;
                    }
                    session_id = s;
                }
                final_text = text;
                reached_terminal = true; // N8：标记由终止事件产出（非中间 Text 后 EOF）
                break;
            }
            CliEvent::Error { text, session } => {
                if let Some(s) = session {
                    if session_id.is_empty() && !s.is_empty() {
                        let _ = chunks.send(AgentChunk::SessionStarted(s.clone())).await;
                    }
                    session_id = s;
                }
                error_text = Some(text);
                break;
            }
            CliEvent::Terminal { session } => {
                if let Some(s) = session {
                    if session_id.is_empty() && !s.is_empty() {
                        let _ = chunks.send(AgentChunk::SessionStarted(s.clone())).await;
                    }
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
        // 真机校准：claude resume 幽灵会话等场景产出 is_error 且 result 文本缺失
        // 的终止事件（空字符串）——空文本对用户零信息量，回落到 diagnose（exit
        // code + stderr 至少可排障）。
        let t = if t.trim().is_empty() {
            diagnose(&status, &stderr_msg, backend_name, reached_terminal)
        } else {
            t
        };
        let _ = chunks.send(AgentChunk::Error(t.clone())).await;
        return Err(CoreError::Backend(backend_name, t));
    }

    if final_text.is_empty() {
        return Err(CoreError::Backend(
            backend_name,
            diagnose(&status, &stderr_msg, backend_name, reached_terminal),
        ));
    }

    // N8：final_text 非空但未由终止事件产出（仅中间 Text 后 stdout EOF）且 exit 非 0 →
    // agent 非正常终止（如 OOM / segfault）。不静默当成功：warn 标注。仍返回已收到的
    // 部分文本（IM 场景拿到结果比报错有用；session_id 可能空，由 dispatch 判空不入库）。
    if !reached_terminal {
        if let Ok(s) = &status {
            if !s.success() {
                tracing::warn!(
                    target: "imagent::backend",
                    backend = backend_name,
                    exit = %s,
                    "agent 非正常终止（未发 Final/Terminal 事件，exit 非 0），返回已收到的部分文本"
                );
            }
        }
    }

    let _ = chunks.send(AgentChunk::Final(final_text.clone())).await;
    Ok(RunOutcome {
        session_id: SessionId(session_id),
        final_text,
        terminal: reached_terminal,
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

/// 把子进程 stderr 读到字符串（非阻断）。三 CLI backend 共享。
///
/// S-5：双层上限防 OOM——
/// - 单行上限 [`MAX_STDERR_LINE_BYTES`]（按字节读行，防无 `\n` 的超长流——可被 prompt
///   injection 构造——单行全量分配撑爆内存）；
/// - 总量上限 [`MAX_STDERR_BYTES`]（超限截断 + 截断标记）。
///
/// 任一超限后继续 drain（不 break），防子进程 stderr 管道写满 ~64KB 阻塞子进程。
pub async fn read_stderr_to_string(stderr: tokio::process::ChildStderr) -> String {
    let mut reader = BufReader::new(stderr);
    let mut total = 0usize;
    let mut truncated = false;
    let mut buf = Vec::new();
    loop {
        match read_line_capped(&mut reader, MAX_STDERR_LINE_BYTES).await {
            Ok(Some(line)) => {
                if truncated {
                    continue; // 已超上限：继续 drain 防管道阻塞，但不累积。
                }
                total += line.len() + 1;
                if total > MAX_STDERR_BYTES {
                    truncated = true;
                    buf.push(format!(
                        "…[stderr 截断：超过 {MAX_STDERR_BYTES} 字节上限，丢弃后续]"
                    ));
                    continue;
                }
                buf.push(line);
            }
            Ok(None) => break, // EOF
            Err(_) => {
                // 单行超 MAX_STDERR_LINE_BYTES（无 `\n` 超长输出）：read_line_capped 已
                // consume 到上限点。push 截断标记并继续 drain 该行剩余（不累积），防 OOM。
                if !truncated {
                    truncated = true;
                    buf.push(format!(
                        "…[stderr 单行超过 {MAX_STDERR_LINE_BYTES} 字节，截断并丢弃后续]"
                    ));
                }
            }
        }
    }
    buf.join("\n")
}

/// stdout 单行字节上限（S-5）：防 agent 输出无 `\n` 的超长行（如 base64 流）
/// 被全量分配撑爆内存。
const MAX_STDOUT_LINE_BYTES: usize = 8 * 1024 * 1024;

/// stderr 累积字节上限（S-5）：长会话 stderr 膨胀，超限截断。
const MAX_STDERR_BYTES: usize = 64 * 1024;

/// stderr 单行字节上限（S-5）：防 agent 向 stderr 写无 `\n` 的超长流（可被 prompt
/// injection 构造）导致单行全量分配 OOM。与 stdout 的 [`MAX_STDOUT_LINE_BYTES`] 对称。
const MAX_STDERR_LINE_BYTES: usize = 1024 * 1024;

/// 按字节读一行，上限 `max_bytes`（S-5）。超限返回 Err（调用方跳过该行）。
/// 覆盖 `AsyncBufReadExt::lines()` 无上限的语义（一行无 `\n` 的超长输出会全量分配）。
async fn read_line_capped<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if buf.is_empty() {
                Ok(None)
            } else {
                Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
            };
        }
        if let Some(nl) = available.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&available[..=nl]);
            reader.consume(nl + 1);
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
        buf.extend_from_slice(available);
        let n = available.len();
        reader.consume(n);
        if buf.len() > max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("line exceeds {max_bytes} bytes"),
            ));
        }
    }
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

/// `allowed_tools` 是否为「不限制」（全量工具）语义：空列表（未收敛/不指定）
/// 或显式 `["*"]`。各 backend 对此统一取自己的最宽档：claude 不附加
/// `--allowedTools`（CLI 自身默认 = 全量）；codex `workspace-write`（按设计
/// 绝不自动 danger-full-access）；gemini `auto_edit`。
pub fn tools_unrestricted(tools: &[String]) -> bool {
    tools.is_empty() || tools.iter().any(|t| t == "*")
}

/// `env_clear()` 后始终透传给 agent 子进程的运行时必需变量（S-2）。
///
/// - `PATH`/`HOME`/`USER`/`LOGNAME`：子进程找可执行、读自身配置的最小必需；
/// - `LANG`/`LC_ALL`/`LC_CTYPE`/`TZ`：locale 与时区，缺 `LANG` 有的 CLI 报 UTF-8 警告；
/// - `TMPDIR`：临时目录。
///
/// 其余 env（含各类 `*_API_KEY`、`DATABASE_URL`、CI secret 等）一律不透传——
/// 由各 backend 经 `spawn_cli_backend` 的 `passthrough_env` 显式声明自己所需 key。
const ALWAYS_PASSTHROUGH_ENV: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR",
];

#[cfg(test)]
mod tests {
    use super::image_write_path;

    #[test]
    fn image_write_path_detects_write_png() {
        assert_eq!(
            image_write_path("Write", r#"{"file_path":"/tmp/a.png","content":"x"}"#),
            Some("/tmp/a.png".to_string())
        );
        assert_eq!(
            image_write_path("Write", r#"{"file_path":"out.JPG"}"#),
            Some("out.JPG".to_string())
        );
    }

    #[test]
    fn image_write_path_ignores_non_image_or_other_tool() {
        assert_eq!(image_write_path("Write", r#"{"file_path":"a.txt"}"#), None);
        assert_eq!(
            image_write_path("Bash", r#"{"command":"cp x.png y.png"}"#),
            None
        );
        assert_eq!(image_write_path("Write", "not-json"), None);
        assert_eq!(image_write_path("Write", r#"{"content":"x"}"#), None);
        assert_eq!(image_write_path("Write", r#"{"file_path":"noext"}"#), None);
    }
}
