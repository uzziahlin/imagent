//! `codex exec --json` 行解析（纯函数，可单测）。
//!
//! Codex CLI 在 `--json` 下每行 stdout 输出一个独立 JSON 对象，顶层有 `type`
//! 字段。关心：
//! - `thread.started` 的 `thread_id`（session id）；
//! - `item.completed` 且 `item.type == "agent_message"` 的文本（final）；
//! - `item.updated|item.completed` 且 `item.type == "reasoning"` 的思考文本
//!   （💭 折叠区，v1.33；此前归 Other 丢弃）；
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
    /// v1.21：`item.*` + `item.type == "todo_list"`——任务清单全量快照
    ///（codex 的 plan/todo 事件，此前归 Other 丢弃）。字段形态宽容解析：
    /// `item.items[]` 的 id/text/status，status ∈ pending/in_progress/completed
    ///（缺失/未知 → Pending）；items 缺失或形态不符退回 Other（宁可无面板
    /// 不可错面板）。
    TodoList { items: Vec<imagent_core::TodoItem> },
    /// v1.33：`item.updated|item.completed` + `item.type == "reasoning"` 且
    /// `item.text` 非空——一段完整思考（💭 折叠区）。实证（见 [`parse_item`]）
    /// 每条 reasoning item 携带该段思考的**完整文本快照**，非增量；一轮可能
    /// 产生多条（每段一条）。空/缺失/非字符串 → Other（fail-open）。
    Reasoning { text: String },
    /// v1.33：`item.updated` + `item.type == "agent_message"` 且 `item.text`
    /// 非空——**防御性解析**。实证 codex exec --json 全版本线（0.48 / 0.75 /
    /// 0.96 / 0.120 / 0.139，见 [`parse_item`] 注释）从不发出该事件；若未来
    /// 契约变化发出，text 语义为该 item 的**累计全文快照**（协议惯例）而非
    /// 纯增量。backend 侧映射见 codex_parse。
    AgentMessageDelta { text: String },
    /// 其余有效 JSON 事件（file_change / web_search / 未识别…），
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
///     - `item.updated` + `item.type == "agent_message"`（text 非空）→
///       [`ParsedEvent::AgentMessageDelta`]（防御性，见 [`parse_item`]）；
///     - `item.updated|item.completed` + `item.type == "reasoning"`（text 非空）→
///       [`ParsedEvent::Reasoning`]（💭 折叠区）；
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
        (_, Some("todo_list")) => match parse_todo_items(item) {
            Some(items) => ParsedEvent::TodoList { items },
            None => ParsedEvent::Other { thread_id },
        },
        // v1.33：reasoning 思考文本（💭 折叠区）。实证（2026-10，codex-cli
        // 0.48/0.75/0.96/0.120/0.139 源码核查 + 本机 ~/.codex/sessions rollout
        // 扫描）：
        // - exec --json 的 reasoning 只以 item.completed 送达（0.48–0.96 经
        //   EventMsg::AgentReasoning、0.139 经 ServerNotification::ItemCompleted，
        //   0.139 的 item.started 对 reasoning 显式抑制不发出）——每段思考一条
        //   事件，text = 该段**完整文本快照**（0.48 为 AgentReasoningEvent.text，
        //   0.139 为 reasoning summary 按 \n join），非增量，一轮可多条；
        // - rollout（TUI 持久化格式，response_item/event_msg）不含 item.* 信封，
        //   其 event_msg/agent_reasoning 同为逐段完整文本，与上述互证。
        // 仅 completed/updated 且 text 非空才上卡；started（无文本）与空/缺失/
        // 非字符串 text 一律 Other（fail-open，绝不 panic）。
        (et, Some("reasoning")) if et == "item.completed" || et == "item.updated" => {
            match item.get("text").and_then(Value::as_str) {
                Some(t) if !t.trim().is_empty() => ParsedEvent::Reasoning {
                    text: t.to_string(),
                },
                _ => ParsedEvent::Other { thread_id },
            }
        }
        // v1.33：item.updated + agent_message——防御性解析。实证 codex exec
        // --json 全版本线（0.48/0.75/0.96/0.120/0.139 源码核查）**从不发出**
        // 该事件：中间 delta（AgentMessageDeltaNotification.delta 等）在 CLI
        // exec 输出层被丢弃（event_processor 落 `_` 兜底臂），上网事件恒为
        // item 全量快照——协议惯例：ItemUpdatedEvent { item: ThreadItem } 携带
        // item 完整状态，纯增量在协议里是显式命名的 `delta` 字段且同样被输出
        // 层丢弃；item.updated 现网仅属于 todo_list（PlanUpdate 全量快照）。
        // 因此若未来契约变化开始发出，text 语义为「该 item 累计全文（快照）」
        // 而非纯增量：无状态逐行解析器无法 diff 出新增部分，直接透传 append
        // 会把快照重复拼进正文——且 codex 的 completed → CliEvent::Text 按 B9
        // 累积、轮末 AgentChunk::Final 覆盖的正是同一份累积，重复无法自愈。
        // 故 backend 侧映射为 Skip（宁可无流式不可错正文，对齐 todo_list 的
        // 「宁可无面板不可错面板」纪律）；权威文本由 item.completed 的
        // agent_message 兜底——codex 本就以 completed 逐段送达正文（含中间
        // 叙述段），流式体验不受损。text 空/缺失/非字符串 → Other（fail-open）。
        ("item.updated", Some("agent_message")) => match item.get("text").and_then(Value::as_str) {
            Some(t) if !t.trim().is_empty() => ParsedEvent::AgentMessageDelta {
                text: t.to_string(),
            },
            _ => ParsedEvent::Other { thread_id },
        },
        _ => ParsedEvent::Other { thread_id },
    }
}

/// todo_list item → TodoItem 列表（items 缺失/空/形态不符返回 None 走 Other）。
fn parse_todo_items(item: &Value) -> Option<Vec<imagent_core::TodoItem>> {
    let arr = item.get("items")?.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(arr.len());
    for e in arr {
        let text = e.get("text").and_then(Value::as_str)?.trim().to_string();
        if text.is_empty() {
            return None;
        }
        let status = match e.get("status").and_then(Value::as_str) {
            Some("in_progress") | Some("InProgress") => imagent_core::TodoStatus::InProgress,
            Some("completed") | Some("Completed") | Some("done") => {
                imagent_core::TodoStatus::Completed
            }
            _ => imagent_core::TodoStatus::Pending,
        };
        out.push(imagent_core::TodoItem {
            id: e.get("id").and_then(Value::as_str).map(str::to_string),
            text,
            status,
        });
    }
    Some(out)
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
        // codex/gemini CLI 不回传窗口（v1.20 窗口自学习仅 ACP 路径）。
        context_window: None,
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
            parse_line(
                r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2,"cached_input_tokens":7}}"#
            ),
            ParsedEvent::TurnCompleted {
                usage: Some(UsageStats {
                    input_tokens: 1,
                    output_tokens: 2,
                    cached_tokens: Some(7),
                    total_cost_usd: None,
                    context_window: None,
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
        // 未识别 item 也落到 Other，并保留 thread_id（v1.33 起 reasoning/
        // agent_message updated 已有专属分支，此处改用未知 item 类型）。
        let web_search = r#"{"type":"item.completed","thread_id":"tid-9","item":{"id":"w1","type":"web_search","query":"rust serde"}}"#;
        assert!(matches!(
            parse_line(web_search),
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

    // ---- v1.33：reasoning 上卡 / agent_message updated 防御解析 ----
    // fixture 形态对齐 codex-cli 0.139.0 exec --json 线上格式（exec_events.rs：
    // ThreadItem { id, #[serde(flatten)] details { type, text } }，信封
    // {"type":"item.completed","item":{...}} 已由本机实跑抓流验证）；文本内容
    // 取自本机 rollout 真实 reasoning 段脱敏缩写。

    /// reasoning item.completed（0.139 真实送达路径）→ Reasoning。
    #[test]
    fn reasoning_completed_maps_to_reasoning() {
        let line = r#"{"type":"item.completed","item":{"id":"item_3","type":"reasoning","text":"用户在问 MCP 启动失败。先看配置文件。\n再实际运行服务器验证握手。"}}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Reasoning {
                text: "用户在问 MCP 启动失败。先看配置文件。\n再实际运行服务器验证握手。".into()
            }
        );
    }

    /// reasoning item.updated：0.48–0.139 实证不发出（started 亦被抑制）；
    /// 若未来发出，按同一条思考的快照语义上卡。
    #[test]
    fn reasoning_updated_with_text_maps_to_reasoning() {
        let line = r#"{"type":"item.updated","item":{"id":"item_3","type":"reasoning","text":"先看配置文件。"}}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::Reasoning {
                text: "先看配置文件。".into()
            }
        );
    }

    /// 空白/缺失 text 与 item.started 一律 Other（fail-open 纪律）。
    #[test]
    fn reasoning_without_text_falls_back_to_other() {
        let started = r#"{"type":"item.started","item":{"id":"item_3","type":"reasoning","text":"will fill later"}}"#;
        assert!(matches!(parse_line(started), ParsedEvent::Other { .. }));
        let blank =
            r#"{"type":"item.completed","item":{"id":"item_3","type":"reasoning","text":"  "}}"#;
        assert!(matches!(parse_line(blank), ParsedEvent::Other { .. }));
        let missing = r#"{"type":"item.completed","item":{"id":"item_3","type":"reasoning"}}"#;
        assert!(matches!(parse_line(missing), ParsedEvent::Other { .. }));
    }

    /// item.updated + agent_message（text 非空）→ AgentMessageDelta。
    /// 实证：0.48–0.139 从不发出；若未来发出，text 为累计全文快照（非纯
    /// 增量，见 parse_item 注释）——backend 侧 Skip，不得当增量 append。
    #[test]
    fn agent_message_updated_maps_to_delta_defensively() {
        let line = r#"{"type":"item.updated","item":{"id":"item_1","type":"agent_message","text":"正在生成的部分回答"}}"#;
        assert_eq!(
            parse_line(line),
            ParsedEvent::AgentMessageDelta {
                text: "正在生成的部分回答".into()
            }
        );
        // 空 text → Other。
        let empty =
            r#"{"type":"item.updated","item":{"id":"item_1","type":"agent_message","text":""}}"#;
        assert!(matches!(parse_line(empty), ParsedEvent::Other { .. }));
    }

    /// 关键序列回归：思考 → 正文逐段（completed）→ 终止，既有 completed →
    /// AgentMessage 行为不变（/resume、幽灵预检、落盘依赖的权威文本路径）。
    #[test]
    fn reasoning_then_agent_message_sequence() {
        let lines = [
            r#"{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"用户要一句流式测试。"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"流式测试通过。"}}"#,
        ];
        assert_eq!(
            parse_line(lines[0]),
            ParsedEvent::Reasoning {
                text: "用户要一句流式测试。".into()
            }
        );
        assert_eq!(
            parse_line(lines[1]),
            ParsedEvent::AgentMessage {
                text: "流式测试通过。".into()
            }
        );
    }
}
