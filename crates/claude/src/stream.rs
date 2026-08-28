//! stream-json 行解析（纯函数，可单测）。
//!
//! Claude Code CLI 在 `--output-format stream-json` 下每行输出一个独立 JSON
//! 对象。MVP 关心 `type == "result"` 的终止事件以拿到最终文本与
//! `session_id`；中间事件里的 `tool_use` / `tool_result` 解析为
//! [`ParsedEvent::ToolUse`] / [`ParsedEvent::ToolResult`]，由上层决定是否处理。

use imagent_core::UsageStats;
use serde_json::Value;

/// 单个 `tool_use` 项（B7：一条 assistant 消息可含多个并行 tool_use，全部收集）。
/// W2-3：携带调用 id（与 tool_result 的 tool_use_id 精确配对）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUseItem {
    pub tool: String,
    pub input: String,
    pub id: Option<String>,
}

/// 单个 `tool_result` 项（B7：一条 user 消息可含多个并行 tool_result，全部收集）。
/// W2-3：携带 tool_use_id（配对用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultItem {
    pub tool: String,
    pub output: String,
    pub id: Option<String>,
}

/// 单行解析后的归类。
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedEvent {
    /// `type == "result"` 的终止事件。附带 usage（顶层 `total_cost_usd` +
    /// `usage` 对象的 input/output/cache_read tokens；缺失字段为 None/0）。
    Result {
        text: String,
        is_error: bool,
        session_id: Option<String>,
        usage: Option<UsageStats>,
    },
    /// assistant 事件（B7/B8）：content[] 中的全部 text 块按序拼接（无则空串）+
    /// 全部 `tool_use`（并行工具调用不再只取首个）。W2-1：thinking 块单独拼接
    /// 为 `thoughts`（与正文分离透出）。
    Assistant {
        text: String,
        thoughts: String,
        tool_uses: Vec<ToolUseItem>,
        session_id: Option<String>,
    },
    /// user 事件（B7）：content[] 中的全部 `tool_result`。
    ToolResults { results: Vec<ToolResultItem> },
    /// 其余有效 JSON 事件（system / progress …）。
    /// 附带该行中可能出现的 `session_id`，便于上层尽早捕获。
    Other { session_id: Option<String> },
    /// 非 JSON（空行 / 噪声），跳过。
    Skip,
}

/// 解析 stream-json 的一行。
///
/// - JSON 解析失败（含空行）→ [`ParsedEvent::Skip`]，永不 panic。
/// - `type == "result"` → 抽取 `result` / `is_error` / `session_id`。
/// - `type == "assistant"` → [`ParsedEvent::Assistant`]（全部 text + 全部 tool_use）。
/// - `type == "user"` → [`ParsedEvent::ToolResults`]（全部 tool_result）。
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
                usage: extract_usage(&value),
            }
        }
        Some("assistant") => ParsedEvent::Assistant {
            text: extract_text_blocks(&value),
            thoughts: extract_thinking_blocks(&value),
            tool_uses: extract_tool_uses(&value),
            session_id,
        },
        Some("user") => {
            let results = extract_tool_results(&value);
            if results.is_empty() {
                ParsedEvent::Other { session_id }
            } else {
                ParsedEvent::ToolResults { results }
            }
        }
        _ => ParsedEvent::Other { session_id },
    }
}

/// 从 result 事件抽取 usage：顶层 `total_cost_usd`（Option<f64>）+ `usage`
/// 对象（input_tokens / output_tokens / cache_read_input_tokens）。
/// 三者全缺 → None；部分缺 → 缺的字段为 None / 0。
fn extract_usage(value: &Value) -> Option<UsageStats> {
    let cost = value.get("total_cost_usd").and_then(Value::as_f64);
    let u = value.get("usage");
    let num = |k: &str| u.and_then(|u| u.get(k)).and_then(Value::as_u64);
    let input = num("input_tokens");
    let output = num("output_tokens");
    let cached = u
        .and_then(|u| u.get("cache_read_input_tokens"))
        .and_then(Value::as_u64);
    if input.is_none() && output.is_none() && cost.is_none() {
        return None;
    }
    Some(UsageStats {
        input_tokens: input.unwrap_or(0),
        output_tokens: output.unwrap_or(0),
        cached_tokens: cached,
        total_cost_usd: cost,
    })
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

/// 从 assistant 事件的 `message.content[]` 收集全部 text 块文本，按序拼接（`\n` 分隔）。
fn extract_text_blocks(value: &Value) -> String {
    let mut texts = Vec::new();
    if let Some(content) = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    {
        for item in content {
            if item.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    texts.push(t.to_string());
                }
            }
        }
    }
    texts.join("\n")
}

/// W2-1：从 assistant 事件的 `message.content[]` 收集 thinking 块（扩展思考），
/// 按序拼接（`\n` 分隔；无则空串）。redacted_thinking 无文本可透出，跳过。
fn extract_thinking_blocks(value: &Value) -> String {
    let mut thoughts = Vec::new();
    if let Some(content) = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    {
        for item in content {
            if item.get("type").and_then(Value::as_str) == Some("thinking") {
                if let Some(t) = item.get("thinking").and_then(Value::as_str) {
                    if !t.trim().is_empty() {
                        thoughts.push(t.to_string());
                    }
                }
            }
        }
    }
    thoughts.join("\n")
}

/// 从 assistant 事件的 `message.content[]` 收集**全部** `tool_use`（B7：并行工具
/// 调用不再只取首个）。input 是对象，用 `Value::to_string()` 序列化；缺 input 为空串。
fn extract_tool_uses(value: &Value) -> Vec<ToolUseItem> {
    let mut uses = Vec::new();
    let Some(content) = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return uses;
    };
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
            // W2-3：调用 id（`tool_use.id`）——与 tool_result 的 tool_use_id 配对。
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            uses.push(ToolUseItem {
                tool: name,
                input,
                id,
            });
        }
    }
    uses
}

/// 从 user 事件的 `message.content[]` 收集**全部** `tool_result`（B7）。
/// tool_result 事件通常只有 tool_use_id 没有 tool 名，tool 留空串。
/// content 可能是 string 或数组/对象，统一兜底为 string。
fn extract_tool_results(value: &Value) -> Vec<ToolResultItem> {
    let mut results = Vec::new();
    let Some(content) = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return results;
    };
    for item in content {
        if item.get("type").and_then(Value::as_str) == Some("tool_result") {
            let output = match item.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            // W2-3：tool_use_id（配对锚点；与 tool_use.id 同源）。
            let id = item
                .get("tool_use_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            results.push(ToolResultItem {
                tool: String::new(),
                output,
                id,
            });
        }
    }
    results
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
        let line = r#"{"type":"result","result":"pong","is_error":false,"session_id":"abc-123","total_cost_usd":0.001,"usage":{"input_tokens":26,"output_tokens":4,"cache_read_input_tokens":120}}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Result {
                text: "pong".to_string(),
                is_error: false,
                session_id: Some("abc-123".to_string()),
                usage: Some(UsageStats {
                    input_tokens: 26,
                    output_tokens: 4,
                    cached_tokens: Some(120),
                    total_cost_usd: Some(0.001),
                }),
            }
        );
    }

    /// result 无 usage/total_cost_usd → usage 为 None。
    #[test]
    fn result_without_usage_fields_is_none() {
        let line = r#"{"type":"result","result":"x","is_error":false}"#;
        match parse_line(line) {
            ParsedEvent::Result { usage, .. } => assert_eq!(usage, None),
            other => panic!("expected Result, got {other:?}"),
        }
    }

    /// result 仅有 total_cost_usd（usage 对象缺失）→ usage 仍产出（tokens 为 0）。
    #[test]
    fn result_cost_only_usage() {
        let line = r#"{"type":"result","result":"x","total_cost_usd":0.02}"#;
        match parse_line(line) {
            ParsedEvent::Result { usage, .. } => {
                let u = usage.expect("cost-only 也应产出 usage");
                assert_eq!(u.total_cost_usd, Some(0.02));
                assert_eq!((u.input_tokens, u.output_tokens), (0, 0));
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn parses_error_result() {
        let line =
            r#"{"type":"result","result":"boom: bad prompt","is_error":true,"session_id":"s-7"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Result {
                text: "boom: bad prompt".to_string(),
                is_error: true,
                session_id: Some("s-7".to_string()),
                usage: None,
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
                usage: None,
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
    fn assistant_text_event_is_parsed() {
        // B8：assistant 纯文本归 Assistant（text 块），供上层推 Text 流。
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]},"session_id":"sess-1"}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Assistant {
                text: "hello".to_string(),
                thoughts: String::new(),
                tool_uses: vec![],
                session_id: Some("sess-1".to_string()),
            }
        );
    }

    #[test]
    fn tool_use_event_is_parsed() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"path":"/x"}}]},"session_id":"sess-2"}"#;
        match parse_line(line) {
            ParsedEvent::Assistant {
                text,
                thoughts: _,
                tool_uses,
                session_id,
            } => {
                assert!(text.is_empty());
                assert_eq!(tool_uses.len(), 1);
                assert_eq!(tool_uses[0].tool, "Read");
                assert!(tool_uses[0].input.contains("path"), "input 应含 path 字段");
                assert_eq!(session_id, Some("sess-2".to_string()));
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    /// B7：一条 assistant 消息内多个并行 tool_use 全部收集（不再只取首个），
    /// 且 text 块文本与 tool_use 共存。
    #[test]
    fn multiple_parallel_tool_uses_all_collected() {
        let line = r#"{"type":"assistant","session_id":"sess-9","message":{"content":[
            {"type":"text","text":"running two tools"},
            {"type":"tool_use","name":"Read","input":{"path":"/a"}},
            {"type":"tool_use","name":"Bash","input":{"command":"ls"}}
        ]}}"#;
        match parse_line(line) {
            ParsedEvent::Assistant {
                text,
                thoughts: _,
                tool_uses,
                session_id,
            } => {
                assert_eq!(text, "running two tools");
                assert_eq!(session_id, Some("sess-9".to_string()));
                assert_eq!(tool_uses.len(), 2, "两个并行 tool_use 都应收集");
                assert_eq!(tool_uses[0].tool, "Read");
                assert_eq!(tool_uses[1].tool, "Bash");
                assert!(tool_uses[0].input.contains("/a"));
                assert!(tool_uses[1].input.contains("ls"));
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn tool_use_with_text_before_it() {
        // content 含 text + 单个 tool_use：两者都保留。
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"thinking..."},{"type":"tool_use","name":"Edit","input":{"file":"/bar"}}]},"session_id":"sess-3"}"#;
        match parse_line(line) {
            ParsedEvent::Assistant {
                text,
                thoughts: _,
                tool_uses,
                ..
            } => {
                assert_eq!(text, "thinking...");
                assert_eq!(tool_uses.len(), 1);
                assert_eq!(tool_uses[0].tool, "Edit");
                assert!(tool_uses[0].input.contains("/bar"));
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn tool_use_missing_input_is_empty_string() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]},"session_id":"sess-4"}"#;
        match parse_line(line) {
            ParsedEvent::Assistant { tool_uses, .. } => {
                assert_eq!(tool_uses.len(), 1);
                assert_eq!(tool_uses[0].tool, "Bash");
                assert!(tool_uses[0].input.is_empty());
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_event_is_parsed() {
        let line =
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"done"}]}}"#;
        match parse_line(line) {
            ParsedEvent::ToolResults { results } => {
                assert_eq!(results.len(), 1);
                assert!(results[0].tool.is_empty(), "tool 名通常缺失，应为空串");
                assert_eq!(results[0].output, "done");
            }
            other => panic!("expected ToolResults, got {other:?}"),
        }
    }

    /// B7：一条 user 消息内多个并行 tool_result 全部收集。
    #[test]
    fn multiple_parallel_tool_results_all_collected() {
        let line = r#"{"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"t1","content":"out-a"},
            {"type":"tool_result","tool_use_id":"t2","content":"out-b"}
        ]}}"#;
        match parse_line(line) {
            ParsedEvent::ToolResults { results } => {
                assert_eq!(results.len(), 2, "两个并行 tool_result 都应收集");
                assert_eq!(results[0].output, "out-a");
                assert_eq!(results[1].output, "out-b");
            }
            other => panic!("expected ToolResults, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_array_content_is_stringified() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"x"}]}]}}"#;
        match parse_line(line) {
            ParsedEvent::ToolResults { results } => {
                assert!(results[0].output.contains("text"));
            }
            other => panic!("expected ToolResults, got {other:?}"),
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

    /// W2-1：thinking 块单独透出（thoughts 字段），不混进 text。
    #[test]
    fn thinking_blocks_parsed_separately() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"thinking","thinking":"先分析问题"},
            {"type":"text","text":"结论如下"},
            {"type":"thinking","thinking":"再验证一遍"}
        ]},"session_id":"sess-t"}"#;
        match parse_line(line) {
            ParsedEvent::Assistant {
                text,
                thoughts,
                tool_uses,
                ..
            } => {
                assert_eq!(text, "结论如下");
                assert_eq!(thoughts, "先分析问题\n再验证一遍");
                assert!(tool_uses.is_empty());
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    /// W2-3：tool_use 带 id、tool_result 带 tool_use_id（精确配对锚点）。
    #[test]
    fn tool_use_and_result_carry_ids() {
        let use_line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01","name":"Bash","input":{"command":"ls"}}]},"session_id":"s"}"#;
        match parse_line(use_line) {
            ParsedEvent::Assistant { tool_uses, .. } => {
                assert_eq!(tool_uses[0].id.as_deref(), Some("toolu_01"));
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
        let res_line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"ok"}]}}"#;
        match parse_line(res_line) {
            ParsedEvent::ToolResults { results } => {
                assert_eq!(results[0].id.as_deref(), Some("toolu_01"));
                assert_eq!(results[0].output, "ok");
            }
            other => panic!("expected ToolResults, got {other:?}"),
        }
    }

    #[test]
    fn empty_session_id_treated_as_none() {
        let line = r#"{"type":"system","session_id":""}"#;
        assert_eq!(parse_line(line), ParsedEvent::Other { session_id: None });
    }
}
