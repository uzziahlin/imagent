//! `codex exec --json` 行解析（纯函数，可单测）。
//!
//! Codex CLI 在 `--json` 下每行 stdout 输出一个独立 JSON 对象，顶层有 `type`
//! 字段。MVP 关心：
//! - `thread.started` 的 `thread_id`（session id）；
//! - `item.completed` 且 `item.type == "agent_message"` 的文本（final）；
//! - `command_execution` / `mcp_tool_call` 的工具调用与结果；
//! - `turn.completed`（成功终止）/ `turn.failed`（失败终止）。
//!
//! 详见任务文档 §2 的 codex JSONL schema。

use imagent_core::UsageStats;
use serde_json::Value;

/// codex JSONL 单行解析结果。
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedEvent {
    /// `thread.started`：session id（字段 `thread_id`）。
    ThreadStarted { thread_id: String },
    /// `item.completed` 且 `item.type == "agent_message"`：agent 文本回复。
    AgentMessage { text: String },
    /// 工具调用（command_execution started / mcp_tool_call started）。
    ToolUse { tool: String, input: String },
    /// 工具结果（command_execution completed / mcp_tool_call completed）。
    ToolResult { tool: String, output: String },
    /// `turn.completed`（成功终止）。附带 usage（`usage.input_tokens` /
    /// `output_tokens` / `cached_input_tokens` / `total_cost_usd`，缺失为 None/0）。
    TurnCompleted { usage: Option<UsageStats> },
    /// `turn.failed`（失败终止）。
    TurnFailed { message: String },
    /// 顶层 `error` 事件（可能瞬时重连，上层 best-effort，不致命）。
    Error { message: String },
    /// 其余有效 JSON 事件（reasoning / file_change / todo_list / 未识别…），
    /// 附带该行中可能出现的 `thread_id`，便于上层尽早捕获。
    Other { thread_id: Option<String> },
    /// 非 JSON（空行 / 噪声），跳过。
    Skip,
}

/// 解析 `codex exec --json` 的一行。
///
/// - JSON 解析失败（含空行）→ [`ParsedEvent::Skip`]，永不 panic。
/// - `type == "thread.started"` → [`ParsedEvent::ThreadStarted`]（`thread_id`）。
/// - `type == "turn.completed"` → [`ParsedEvent::TurnCompleted`]。
/// - `type == "turn.failed"` → [`ParsedEvent::TurnFailed`]（`error.message`）。
/// - `type == "error"` → [`ParsedEvent::Error`]（`message`）。
/// - `type ∈ {item.started, item.updated, item.completed}`：
///     - `item.completed` + `item.type == "agent_message"` →
///       [`ParsedEvent::AgentMessage`]（`item.text`）；
///     - started + `item.type == "command_execution"` →
///       [`ParsedEvent::ToolUse`] `{ tool: "shell", input: item.command }`；
///     - `item.completed` + `item.type == "command_execution"` →
///       [`ParsedEvent::ToolResult`] `{ tool: "shell", output: item.aggregated_output }`；
///     - started + `item.type == "mcp_tool_call"` →
///       [`ParsedEvent::ToolUse`] `{ tool: item.tool, input: arguments JSON 字符串 }`；
///     - `item.completed` + `item.type == "mcp_tool_call"` →
///       [`ParsedEvent::ToolResult`] `{ tool: item.tool, output: result JSON / error.message }`；
///     - 其它 item → [`ParsedEvent::Other`]。
/// - 其它顶层事件 → [`ParsedEvent::Other`]（顺带抽取 `thread_id` 若有）。
pub fn parse_line(line: &str) -> ParsedEvent {
    let line = line.trim();
    if line.is_empty() {
        return ParsedEvent::Skip;
    }

    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return ParsedEvent::Skip,
    };

    // 顺带抽取顶层 thread_id（部分事件会带上），便于尽早捕获 session。
    let thread_id = extract_thread_id(&value);
    let item_type = value.get("type").and_then(Value::as_str);

    match item_type {
        Some("thread.started") => {
            let tid = value
                .get("thread_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            ParsedEvent::ThreadStarted { thread_id: tid }
        }
        Some("turn.completed") => ParsedEvent::TurnCompleted {
            usage: extract_usage(&value),
        },
        Some("turn.failed") => ParsedEvent::TurnFailed {
            message: value
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("turn failed")
                .to_string(),
        },
        Some("error") => ParsedEvent::Error {
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string(),
        },
        Some(t) if t.starts_with("item.") => parse_item(t, &value, thread_id),
        _ => ParsedEvent::Other { thread_id },
    }
}

/// 解析 item.* 事件。
fn parse_item(event_type: &str, value: &Value, thread_id: Option<String>) -> ParsedEvent {
    let item = match value.get("item") {
        Some(i) => i,
        None => return ParsedEvent::Other { thread_id },
    };
    let kind = item.get("type").and_then(Value::as_str);

    match (event_type, kind) {
        ("item.completed", Some("agent_message")) => ParsedEvent::AgentMessage {
            text: item.get("text").map(text_of).unwrap_or_default(),
        },
        (et, Some("command_execution")) if et == "item.started" || et == "item.updated" => {
            // started/upd 这类给出命令本身。
            ParsedEvent::ToolUse {
                tool: "shell".into(),
                input: item
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            }
        }
        ("item.completed", Some("command_execution")) => ParsedEvent::ToolResult {
            tool: "shell".into(),
            output: item
                .get("aggregated_output")
                .map(text_of)
                .unwrap_or_default(),
        },
        (et, Some("mcp_tool_call")) if et == "item.started" || et == "item.updated" => {
            let tool = item
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("mcp")
                .to_string();
            let input = match item.get("arguments") {
                Some(v) => v.to_string(),
                None => String::new(),
            };
            ParsedEvent::ToolUse { tool, input }
        }
        ("item.completed", Some("mcp_tool_call")) => {
            let tool = item
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("mcp")
                .to_string();
            // 优先 result（序列化为 JSON 字符串），无则用 error.message。
            let output = match item.get("result") {
                Some(v) if !v.is_null() => v.to_string(),
                _ => item
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            };
            ParsedEvent::ToolResult { tool, output }
        }
        _ => ParsedEvent::Other { thread_id },
    }
}

/// 从 turn.completed 事件抽取 usage 对象（input/output/cached tokens，
/// 部分版本还带 total_cost_usd）。全部缺失 → None。
fn extract_usage(value: &Value) -> Option<UsageStats> {
    let u = value.get("usage")?;
    let num = |k: &str| u.get(k).and_then(Value::as_u64);
    let input = num("input_tokens");
    let output = num("output_tokens");
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(UsageStats {
        input_tokens: input.unwrap_or(0),
        output_tokens: output.unwrap_or(0),
        cached_tokens: num("cached_input_tokens"),
        total_cost_usd: u.get("total_cost_usd").and_then(Value::as_f64),
    })
}

/// 从 JSON 对象抽取非空顶层 `thread_id`。
fn extract_thread_id(value: &Value) -> Option<String> {
    value
        .get("thread_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// 把 JSON 值规范化为文本：字符串取原值，其它序列化。
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_line_is_skip() {
        assert_eq!(parse_line(""), ParsedEvent::Skip);
        assert_eq!(parse_line("   \n"), ParsedEvent::Skip);
    }

    #[test]
    fn non_json_is_skip() {
        assert_eq!(parse_line("not json at all"), ParsedEvent::Skip);
        assert_eq!(parse_line("{ broken"), ParsedEvent::Skip);
    }

    #[test]
    fn thread_started() {
        let line = r#"{"type":"thread.started","thread_id":"abc-123"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::ThreadStarted {
                thread_id: "abc-123".into()
            }
        );
    }

    #[test]
    fn turn_completed() {
        assert_eq!(
            parse_line(r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2,"cached_input_tokens":7}}"#),
            ParsedEvent::TurnCompleted {
                usage: Some(UsageStats {
                    input_tokens: 1,
                    output_tokens: 2,
                    cached_tokens: Some(7),
                    total_cost_usd: None,
                }),
            }
        );
    }

    /// turn.completed 无 usage 对象 → usage 为 None。
    #[test]
    fn turn_completed_without_usage() {
        assert_eq!(
            parse_line(r#"{"type":"turn.completed"}"#),
            ParsedEvent::TurnCompleted { usage: None }
        );
    }

    #[test]
    fn turn_failed() {
        let line = r#"{"type":"turn.failed","error":{"message":"rate limit exceeded"}}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::TurnFailed {
                message: "rate limit exceeded".into()
            }
        );
    }

    #[test]
    fn top_level_error_event() {
        // 瞬时重连提示，非致命。
        let line = r#"{"type":"error","message":"Reconnecting... 1/5"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Error {
                message: "Reconnecting... 1/5".into()
            }
        );
    }

    #[test]
    fn agent_message_completed() {
        let line = r#"{"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"hello world"}}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::AgentMessage {
                text: "hello world".into()
            }
        );
    }

    #[test]
    fn command_execution_started_then_completed() {
        let started = r#"{"type":"item.started","item":{"id":"c1","type":"command_execution","command":"bash -lc ls"}}"#;
        assert_eq!(
            parse_line(started),
            ParsedEvent::ToolUse {
                tool: "shell".into(),
                input: "bash -lc ls".into()
            }
        );
        let completed = r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","aggregated_output":"file_a\nfile_b","exit_code":0,"status":"completed"}}"#;
        assert_eq!(
            parse_line(completed),
            ParsedEvent::ToolResult {
                tool: "shell".into(),
                output: "file_a\nfile_b".into()
            }
        );
    }

    #[test]
    fn mcp_tool_call_started_then_completed() {
        let started = r#"{"type":"item.started","item":{"id":"t1","type":"mcp_tool_call","server":"fs","tool":"read_file","arguments":{"path":"/tmp/x"}}}"#;
        assert_eq!(
            parse_line(started),
            ParsedEvent::ToolUse {
                tool: "read_file".into(),
                input: r#"{"path":"/tmp/x"}"#.into()
            }
        );
        let completed = r#"{"type":"item.completed","item":{"id":"t1","type":"mcp_tool_call","tool":"read_file","result":{"lines":3},"error":null}}"#;
        assert_eq!(
            parse_line(completed),
            ParsedEvent::ToolResult {
                tool: "read_file".into(),
                output: r#"{"lines":3}"#.into()
            }
        );
    }

    #[test]
    fn mcp_tool_call_completed_with_error() {
        let line = r#"{"type":"item.completed","item":{"id":"t2","type":"mcp_tool_call","tool":"read_file","result":null,"error":{"message":"not found"}}}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::ToolResult {
                tool: "read_file".into(),
                output: "not found".into()
            }
        );
    }

    #[test]
    fn other_events_carry_thread_id() {
        let line = r#"{"type":"turn.started","thread_id":"tid-9"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Other {
                thread_id: Some("tid-9".into())
            }
        );
        // 未识别 item 也落到 Other，并保留 thread_id。
        let reasoning = r#"{"type":"item.updated","thread_id":"tid-9","item":{"id":"r1","type":"reasoning","text":"..."}}"#;
        assert!(matches!(
            parse_line(reasoning),
            ParsedEvent::Other { thread_id: Some(_) }
        ));
    }

    #[test]
    fn item_updated_command_execution_is_tooluse() {
        // updated 同样可能携带 command，按 started 处理。
        let line = r#"{"type":"item.updated","item":{"id":"c2","type":"command_execution","command":"bash -lc pwd"}}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::ToolUse {
                tool: "shell".into(),
                input: "bash -lc pwd".into()
            }
        );
    }
}
