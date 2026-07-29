//! 飞书交互卡片渲染：把平台无关的 [`OutboundCard`] 渲染成飞书 CardKit 2.0 JSON。

use imagent_core::{CardTerminal, OutboundCard};

/// 渲染 [`OutboundCard`] 为飞书 interactive 卡片的 content JSON 字符串
/// （配合 `msg_type = "interactive"` 发送 / patch）。
///
/// MVP：markdown 文本块 + 工具调用摘要 + 状态 footer。折叠面板/流式动效留批2-3。
pub fn render_card(card: &OutboundCard) -> String {
    let (footer, streaming) = match &card.terminal {
        CardTerminal::Running => ("🧠 思考中…", true),
        CardTerminal::Done => ("✅ 完成", false),
        CardTerminal::Error(e) => return render_error_card(e),
    };
    let text = if card.text.is_empty() { "…" } else { &card.text };
    let mut elements = vec![serde_json::json!({ "tag": "markdown", "content": text })];
    // 工具调用摘要（MVP：markdown 列表；批2-3 再做 collapsible_panel）
    if !card.tool_calls.is_empty() {
        let tools_md = card
            .tool_calls
            .iter()
            .map(|(t, inp)| format!("- `{t}`：{}", truncate_str(inp, 60)))
            .collect::<Vec<_>>()
            .join("\n");
        elements.push(serde_json::json!({
            "tag": "markdown",
            "content": format!("**工具调用**\n{tools_md}")
        }));
    }
    // 状态 footer（note 行体现终态 / 流式中）
    elements.push(serde_json::json!({ "tag": "markdown", "content": footer }));

    serde_json::json!({
        "schema": "2.0",
        "config": { "streaming_mode": streaming },
        "body": { "elements": elements }
    })
    .to_string()
    // 注：飞书 CardKit 2.0 结构以「能发出去 + 能 patch」为准。MVP 用最简 markdown 块
    //    + streaming_mode；若 streaming_mode/config 字段致飞书 400，退回最简
    //    {"schema":"2.0","body":{"elements":[{tag:markdown,content:text}]}}。但本批无真机，
    //    单测只验含关键文本 + schema，不改结构。
}

/// 错误终态卡片。
fn render_error_card(err: &str) -> String {
    serde_json::json!({
        "schema": "2.0",
        "body": {
            "elements": [{ "tag": "markdown", "content": format!("❌ 出错：{err}") }]
        }
    })
    .to_string()
}

/// 按 char 截断（避免半截 UTF-8）。
fn truncate_str(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use imagent_core::CardTerminal;

    #[test]
    fn render_running_has_markdown() {
        let card = OutboundCard {
            text: "hello".into(),
            tool_calls: vec![],
            terminal: CardTerminal::Running,
        };
        let json = render_card(&card);
        assert!(json.contains("hello"));
        assert!(json.contains("schema"));
        assert!(json.contains("思考中"));
    }

    #[test]
    fn render_done_with_tools() {
        let card = OutboundCard {
            text: "done".into(),
            tool_calls: vec![("Read".into(), "x".into())],
            terminal: CardTerminal::Done,
        };
        let json = render_card(&card);
        assert!(json.contains("done"));
        assert!(json.contains("Read"));
        assert!(json.contains("完成"));
    }

    #[test]
    fn render_error() {
        let card = OutboundCard {
            text: "".into(),
            tool_calls: vec![],
            terminal: CardTerminal::Error("boom".into()),
        };
        assert!(render_card(&card).contains("boom"));
    }
}
