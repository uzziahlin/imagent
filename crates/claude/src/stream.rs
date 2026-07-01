//! stream-json 行解析（纯函数，可单测）。
//!
//! Claude Code CLI 在 `--output-format stream-json` 下每行输出一个独立 JSON
//! 对象。MVP 关心 `type == "result"` 的终止事件以拿到最终文本与
//! `session_id`；中间事件里的 `tool_use` / `tool_result` 解析为
//! [`ParsedEvent::ToolUse`] / [`ParsedEvent::ToolResult`]，由上层决定是否处理。

use serde_json::Value;

/// 单行解析后的归类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedEvent {
    /// `type == "result"` 的终止事件。
    Result {
        text: String,
        is_error: bool,
        session_id: Option<String>,
    },
    /// assistant 事件中的首个 `tool_use`：工具名 + input 的 JSON 字符串。
    ToolUse {
        tool: String,
        input: String,
        session_id: Option<String>,
    },
    /// user 事件中的首个 `tool_result`：tool 名通常缺失（只有 tool_use_id），留空串。
    ToolResult {
        tool: String,
        output: String,
    },
    /// 其余有效 JSON 事件（纯 text / system …）。
    /// 附带该行中可能出现的 `session_id`，便于上层尽早捕获。
    Other {
        session_id: Option<String>,
    },
    /// 非 JSON（空行 / 噪声），跳过。
    Skip,
}

/// 解析 stream-json 的一行。
///
/// - JSON 解析失败（含空行）→ [`ParsedEvent::Skip`]，永不 panic。
/// - `type == "result"` → 抽取 `result` / `is_error` / `session_id`。
/// - `type == "assistant"` 且 content 含 `tool_use` → [`ParsedEvent::ToolUse`]。
/// - `type == "user"` 且 content 含 `tool_result` → [`ParsedEvent::ToolResult`]。
/// - 其它 → [`ParsedEvent::Other`]，顺带抽取 `session_id`（若有）。
pub fn parse_line(line: &str) -> ParsedEvent {
    let line = line.trim();
    if line.is_empty() {
        return ParsedEvent::Skip;
    }

    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return ParsedEvent::Skip,
    };

    // 优先从顶层 `session_id` 抽取。
    let session_id = extract_session_id(&value);

    match value.get("type").and_then(Value::as_str) {
        Some("result") => {
            let text = match value.get("result") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            let is_error = value
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            ParsedEvent::Result {
                text,
                is_error,
                session_id,
            }
        }
        Some("assistant") => {
            // 在 message.content[] 里找首个 tool_use；纯 text 仍归 Other。
            if let Some((tool, input)) = extract_tool_use(&value) {
                return ParsedEvent::ToolUse {
                    tool,
                    input,
                    session_id,
                };
            }
            ParsedEvent::Other { session_id }
        }
        Some("user") => {
            // 在 message.content[] 里找首个 tool_result。
            if let Some((tool, output)) = extract_tool_result(&value) {
                return ParsedEvent::ToolResult { tool, output };
            }
            ParsedEvent::Other { session_id }
        }
        _ => ParsedEvent::Other { session_id },
    }
}

/// 从 JSON 对象抽取非空 `session_id`（顶层或常见嵌套位置）。
fn extract_session_id(value: &Value) -> Option<String> {
    if let Some(s) = value.get("session_id").and_then(Value::as_str) {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    None
}

/// 从 assistant 事件的 `message.content[]` 找首个 `tool_use`，返回 (name, input_string)。
/// input 是对象，用 `Value::to_string()` 序列化；缺 input 时为空串。
fn extract_tool_use(value: &Value) -> Option<(String, String)> {
    let content = value.get("message")?.get("content")?.as_array()?;
    for item in content {
        if item.get("type").and_then(Value::as_str) == Some("tool_use") {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let input = match item.get("input") {
                Some(v) => v.to_string(),
                None => String::new(),
            };
            return Some((name, input));
        }
    }
    None
}

/// 从 user 事件的 `message.content[]` 找首个 `tool_result`，返回 (tool, output)。
/// tool_result 事件通常只有 tool_use_id 没有 tool 名，tool 留空串。
/// content 可能是 string 或数组/对象，统一兜底为 string。
fn extract_tool_result(value: &Value) -> Option<(String, String)> {
    let content = value.get("message")?.get("content")?.as_array()?;
    for item in content {
        if item.get("type").and_then(Value::as_str) == Some("tool_result") {
            let output = match item.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            return Some((String::new(), output));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_empty_and_garbage() {
        assert_eq!(parse_line(""), ParsedEvent::Skip);
        assert_eq!(parse_line("   "), ParsedEvent::Skip);
        assert_eq!(parse_line("not json at all"), ParsedEvent::Skip);
        assert_eq!(parse_line("{"), ParsedEvent::Skip);
    }

    #[test]
    fn parses_ok_result_with_session() {
        let line = r#"{"type":"result","result":"pong","is_error":false,"session_id":"abc-123","total_cost_usd":0.001}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Result {
                text: "pong".to_string(),
                is_error: false,
                session_id: Some("abc-123".to_string()),
            }
        );
    }

    #[test]
    fn parses_error_result() {
        let line = r#"{"type":"result","result":"boom: bad prompt","is_error":true,"session_id":"s-7"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Result {
                text: "boom: bad prompt".to_string(),
                is_error: true,
                session_id: Some("s-7".to_string()),
            }
        );
    }

    #[test]
    fn result_defaults_is_error_false_when_missing() {
        let line = r#"{"type":"result","result":"ok"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Result {
                text: "ok".to_string(),
                is_error: false,
                session_id: None,
            }
        );
    }

    #[test]
    fn result_non_string_result_is_stringified() {
        let line = r#"{"type":"result","result":{"nested":1},"is_error":false}"#;
        match parse_line(line) {
            ParsedEvent::Result { text, is_error, .. } => {
                assert!(text.contains("nested"));
                assert!(!is_error);
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn assistant_text_event_is_other() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]},"session_id":"sess-1"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Other {
                session_id: Some("sess-1".to_string()),
            }
        );
    }

    #[test]
    fn tool_use_event_is_parsed() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"path":"/x"}}]},"session_id":"sess-2"}"#;
        match parse_line(line) {
            ParsedEvent::ToolUse {
                tool,
                input,
                session_id,
            } => {
                assert_eq!(tool, "Read");
                assert!(input.contains("path"), "input 应含 path 字段: {input}");
                assert_eq!(session_id, Some("sess-2".to_string()));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn tool_use_picks_first_when_multiple_content_entries() {
        // content 含 text + tool_use：取首个 tool_use。
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"thinking..."},{"type":"tool_use","name":"Edit","input":{"file":"/bar"}}]},"session_id":"sess-3"}"#;
        match parse_line(line) {
            ParsedEvent::ToolUse { tool, input, .. } => {
                assert_eq!(tool, "Edit");
                assert!(input.contains("/bar"));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn tool_use_missing_input_is_empty_string() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]},"session_id":"sess-4"}"#;
        match parse_line(line) {
            ParsedEvent::ToolUse {
                tool, input, ..
            } => {
                assert_eq!(tool, "Bash");
                assert!(input.is_empty());
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_event_is_parsed() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"done"}]}}"#;
        match parse_line(line) {
            ParsedEvent::ToolResult { tool, output } => {
                assert!(tool.is_empty(), "tool 名通常缺失，应为空串");
                assert_eq!(output, "done");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_array_content_is_stringified() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"x"}]}]}}"#;
        match parse_line(line) {
            ParsedEvent::ToolResult { output, .. } => {
                assert!(output.contains("text"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn user_event_without_tool_result_is_other() {
        let line = r#"{"type":"user","message":{"content":[{"type":"text","text":"hi"}]},"session_id":"sess-5"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Other {
                session_id: Some("sess-5".to_string()),
            }
        );
    }

    #[test]
    fn empty_session_id_treated_as_none() {
        let line = r#"{"type":"system","session_id":""}"#;
        assert_eq!(parse_line(line), ParsedEvent::Other { session_id: None });
    }
}
