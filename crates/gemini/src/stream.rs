//! `gemini -o stream-json` 行解析（纯函数，可单测）。
//!
//! Gemini CLI 在 `stream-json` 下每行 stdout 输出一个独立 JSON 对象，顶层有
//! `type` 字段。MVP 关心：
//! - `init` 的 `session_id`（session id）；
//! - `message`（`role == "assistant"`）的 `content`（final 文本）；
//! - `tool_use` / `tool_result` 的工具调用与结果；
//! - `result`（成功终止）/ `error`（失败终止）。
//!
//! 详见任务文档 §2 的 gemini stream-json schema。

use serde_json::Value;

/// gemini stream-json 单行解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedEvent {
    /// `init`：session id（字段 `session_id`，可能带 `model`）。
    Init {
        session_id: String,
        model: Option<String>,
    },
    /// `message` 且 `role == "assistant"`：agent 文本回复。
    AssistantMessage { text: String },
    /// `tool_use`：工具调用。
    ToolUse { tool: String, input: String },
    /// `tool_result`：工具结果。
    ToolResult { tool: String, output: String },
    /// `result`（成功终止）。
    Result,
    /// `error`（失败终止）。
    Error { message: String },
    /// 其余有效 JSON 事件（未识别的 type），debug 记录，不 panic。
    Other,
    /// 非 JSON（空行 / 噪声），跳过。
    Skip,
}

/// 解析 `gemini -o stream-json` 的一行。
///
/// - JSON 解析失败（含空行）→ [`ParsedEvent::Skip`]，永不 panic。
/// - `type == "init"` → [`ParsedEvent::Init`]（`session_id`，可选 `model`）。
/// - `type == "message"` 且 `role == "assistant"` →
///   [`ParsedEvent::AssistantMessage`]（`content`）；`role != "assistant"`（如
///   user）→ [`ParsedEvent::Other`]。
/// - `type == "tool_use"` → [`ParsedEvent::ToolUse`] `{ tool: tool_name, input:
///   parameters 序列化为 JSON 字符串 }`。
/// - `type == "tool_result"` → [`ParsedEvent::ToolResult`] `{ tool:
///   tool_name（无则 "tool"）, output }`；`status != "success"` 时把 status 拼进 output。
/// - `type == "result"` → [`ParsedEvent::Result`]（成功终止）。
/// - `type == "error"` → [`ParsedEvent::Error`]（`message`）。
/// - 其它 → [`ParsedEvent::Other`]。
pub fn parse_line(line: &str) -> ParsedEvent {
    let line = line.trim();
    if line.is_empty() {
        return ParsedEvent::Skip;
    }

    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return ParsedEvent::Skip,
    };

    let item_type = value.get("type").and_then(Value::as_str);

    match item_type {
        Some("init") => ParsedEvent::Init {
            session_id: value
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            model: value.get("model").and_then(Value::as_str).map(String::from),
        },
        Some("message") => {
            let role = value.get("role").and_then(Value::as_str).unwrap_or("");
            if role == "assistant" {
                ParsedEvent::AssistantMessage {
                    text: value.get("content").map(text_of).unwrap_or_default(),
                }
            } else {
                // user message 等不计入 final 文本。
                ParsedEvent::Other
            }
        }
        Some("tool_use") => ParsedEvent::ToolUse {
            tool: value
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string(),
            input: match value.get("parameters") {
                Some(v) => v.to_string(),
                None => String::new(),
            },
        },
        Some("tool_result") => {
            let tool = value
                .get("tool_name")
                .and_then(Value::as_str)
                .or_else(|| value.get("tool_id").and_then(Value::as_str))
                .unwrap_or("tool")
                .to_string();
            let status = value.get("status").and_then(Value::as_str).unwrap_or("");
            let mut output = value.get("output").map(text_of).unwrap_or_default();
            // status 非 success 时把 status 拼进 output，便于上层诊断。
            if !status.is_empty() && status != "success" {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str("[status: ");
                output.push_str(status);
                output.push(']');
            }
            ParsedEvent::ToolResult { tool, output }
        }
        Some("result") => ParsedEvent::Result,
        Some("error") => ParsedEvent::Error {
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string(),
        },
        _ => ParsedEvent::Other,
    }
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
    fn init_with_model() {
        let line = r#"{"type":"init","session_id":"abc123def","model":"gemini-2.5-pro","timestamp":"..."}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Init {
                session_id: "abc123def".into(),
                model: Some("gemini-2.5-pro".into()),
            }
        );
    }

    #[test]
    fn init_without_model() {
        let line = r#"{"type":"init","session_id":"abc123def"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Init {
                session_id: "abc123def".into(),
                model: None,
            }
        );
    }

    #[test]
    fn init_missing_session_id() {
        let line = r#"{"type":"init"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Init {
                session_id: "".into(),
                model: None,
            }
        );
    }

    #[test]
    fn assistant_message() {
        let line = r#"{"type":"message","role":"assistant","content":"The output `hello`.","timestamp":"..."}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::AssistantMessage {
                text: "The output `hello`.".into()
            }
        );
    }

    #[test]
    fn assistant_message_with_delta() {
        let line = r#"{"type":"message","role":"assistant","content":"partial","delta":true}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::AssistantMessage {
                text: "partial".into()
            }
        );
    }

    #[test]
    fn user_message_is_other() {
        let line = r#"{"type":"message","role":"user","content":"do something"}"#;
        assert_eq!(parse_line(line), ParsedEvent::Other);
    }

    #[test]
    fn tool_use() {
        let line = r#"{"type":"tool_use","tool_name":"read_file","tool_id":"tool_1","parameters":{"file_path":"x"},"timestamp":"..."}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::ToolUse {
                tool: "read_file".into(),
                input: r#"{"file_path":"x"}"#.into()
            }
        );
    }

    #[test]
    fn tool_use_missing_parameters() {
        let line = r#"{"type":"tool_use","tool_name":"list_dir","tool_id":"t2"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::ToolUse {
                tool: "list_dir".into(),
                input: "".into()
            }
        );
    }

    #[test]
    fn tool_result_success_with_tool_name() {
        let line = r#"{"type":"tool_result","tool_id":"tool_1","tool_name":"read_file","status":"success","output":"hello","timestamp":"..."}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::ToolResult {
                tool: "read_file".into(),
                output: "hello".into()
            }
        );
    }

    #[test]
    fn tool_result_without_tool_name() {
        // tool_result 不一定带 tool_name，可能只有 tool_id。
        let line =
            r#"{"type":"tool_result","tool_id":"tool_1","status":"success","output":"hello"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::ToolResult {
                tool: "tool_1".into(),
                output: "hello".into()
            }
        );
    }

    #[test]
    fn tool_result_non_success_status() {
        let line = r#"{"type":"tool_result","tool_id":"tool_2","tool_name":"run_cmd","status":"error","output":"command failed"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::ToolResult {
                tool: "run_cmd".into(),
                output: "command failed\n[status: error]".into()
            }
        );
    }

    #[test]
    fn tool_result_non_success_empty_output() {
        let line = r#"{"type":"tool_result","tool_id":"tool_3","status":"error"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::ToolResult {
                tool: "tool_3".into(),
                output: "[status: error]".into()
            }
        );
    }

    #[test]
    fn tool_result_falls_back_to_default() {
        // 既无 tool_name 也无 tool_id，回退到通用 "tool"。
        let line = r#"{"type":"tool_result","status":"success","output":"x"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::ToolResult {
                tool: "tool".into(),
                output: "x".into()
            }
        );
    }

    #[test]
    fn result_success() {
        let line = r#"{"type":"result","status":"success","stats":{"input_tokens":100,"output_tokens":50},"timestamp":"..."}"#;
        assert_eq!(parse_line(line), ParsedEvent::Result);
    }

    #[test]
    fn error_event() {
        let line = r#"{"type":"error","message":"API key invalid","timestamp":"..."}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Error {
                message: "API key invalid".into()
            }
        );
    }

    #[test]
    fn error_event_missing_message() {
        let line = r#"{"type":"error"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Error {
                message: "error".into()
            }
        );
    }

    #[test]
    fn unknown_type_is_other() {
        let line = r#"{"type":"some_future_event","foo":"bar"}"#;
        assert_eq!(parse_line(line), ParsedEvent::Other);
    }
}
