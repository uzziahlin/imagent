//! stream-json 行解析（纯函数，可单测）。
//!
//! Claude Code CLI 在 `--output-format stream-json` 下每行输出一个独立 JSON
//! 对象。MVP 只关心 `type == "result"` 的终止事件以拿到最终文本与
//! `session_id`；中间事件（assistant 文本 / tool_use / tool_result）目前归为
//! [`ParsedEvent::Other`]，由上层决定是否处理。

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
    /// 其余有效 JSON 事件（assistant / tool_use / tool_result / system …）。
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
    fn tool_use_event_is_other() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"path":"/x"}}]},"session_id":"sess-2"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Other {
                session_id: Some("sess-2".to_string()),
            }
        );
    }

    #[test]
    fn tool_result_event_is_other_without_session() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"done"}]}}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Other { session_id: None }
        );
    }

    #[test]
    fn empty_session_id_treated_as_none() {
        let line = r#"{"type":"system","session_id":""}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Other { session_id: None }
        );
    }
}
