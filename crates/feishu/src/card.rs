//! 飞书交互卡片渲染：把平台无关的 [`OutboundCard`] 渲染成飞书 CardKit 2.0 JSON。

use imagent_core::{CardButton, CardTerminal, OutboundCard};

/// 渲染 [`OutboundCard`] 为飞书 interactive 卡片的 content JSON 字符串
/// （配合 `msg_type = "interactive"` 发送 / patch）。
///
/// markdown 文本块 + 工具调用折叠面板 + 状态 footer。
/// 这是**降级路径**的渲染（managed 真流式路径见 [`render_stream_init_card`]）。
pub fn render_card(card: &OutboundCard) -> String {
    let (footer, streaming) = match &card.terminal {
        CardTerminal::Running => ("🧠 思考中…", true),
        CardTerminal::Done => ("✅ 完成", false),
        CardTerminal::Error(e) => return render_error_card(e),
    };
    let text = if card.text.is_empty() {
        "…"
    } else {
        &card.text
    };
    let mut elements = vec![serde_json::json!({ "tag": "markdown", "content": text })];
    // 工具调用：折叠面板（默认收起，省卡片高度让 footer 可见；对标 lcab）。
    // 只用 tag/expanded/header.title/elements 最简字段——CardKit 2.0 对未知属性会报错
    // （非静默忽略），故避开 background_color/background_style 等易因版本改名的字段。
    if !card.tool_calls.is_empty() {
        let tools_md = card
            .tool_calls
            .iter()
            .map(|(t, inp)| format!("- `{t}`：{}", truncate_str(inp, 60)))
            .collect::<Vec<_>>()
            .join("\n");
        let n = card.tool_calls.len();
        elements.push(serde_json::json!({
            "tag": "collapsible_panel",
            "expanded": false,
            "header": {
                "title": {
                    "tag": "markdown",
                    "content": format!("🔧 工具调用（{n}）")
                }
            },
            "elements": [{ "tag": "markdown", "content": tools_md }]
        }));
    }
    // 状态 footer（note 行体现终态 / 流式中）
    elements.push(serde_json::json!({ "tag": "markdown", "content": footer }));

    // Running 态带自定义 summary（卡片列表预览/通知处显示，默认「生成中」）；
    // Done 态 streaming=false 不需要 summary。
    let config = if streaming {
        serde_json::json!({
            "streaming_mode": true,
            "summary": { "content": "🧠 正在执行任务…" }
        })
    } else {
        serde_json::json!({ "streaming_mode": false })
    };
    serde_json::json!({
        "schema": "2.0",
        "config": config,
        "body": { "elements": elements }
    })
    .to_string()
}

/// managed 流式卡片的**初始**卡片 JSON（创建 CardKit 实体用）。
///
/// 正文 markdown 组件带固定 `element_id = md_body`（后续 element PATCH 的锚点），
/// 初始内容为空；footer 独立组件体现执行中。`config` 开启流式模式 + 自定义摘要。
pub fn render_stream_init_card() -> String {
    serde_json::json!({
        "schema": "2.0",
        "config": {
            "streaming_mode": true,
            "summary": { "content": "🧠 正在执行任务…" }
        },
        "body": { "elements": [
            { "tag": "markdown", "element_id": "md_body", "content": "…" },
            { "tag": "markdown", "element_id": "md_footer", "content": "🧠 执行中" }
        ] }
    })
    .to_string()
}

/// Running 期间 `md_body` 的流式内容：累积正文 + 工具调用紧凑列表。
///
/// 工具与正文同置一个 markdown 组件——CardKit 的 element 流式 PATCH 仅支持
/// markdown 组件（折叠面板不可流式更新），故 managed 路径下工具以列表进正文。
pub fn stream_body_md(text: &str, tool_calls: &[(String, String)]) -> String {
    let mut out = String::new();
    if !text.is_empty() {
        out.push_str(text);
    }
    if !tool_calls.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let tools: Vec<String> = tool_calls
            .iter()
            .map(|(t, inp)| format!("- 🔧 `{t}`：{}", truncate_str(inp, 40)))
            .collect();
        out.push_str(&tools.join("\n"));
    }
    if out.is_empty() {
        out.push('…');
    }
    out
}

/// 终态（Done/Error）时 `md_body` 的最终内容：正文 + 工具统计行 + 完成行。
///
/// 终态工具用**统计**（按工具名计数）而非全列——卡片正文保持简洁，
/// 明细在降级路径的折叠面板里（managed 路径不重复罗列）。
pub fn stream_body_final(text: &str, tool_calls: &[(String, String)], err: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(e) = err {
        out.push_str(&format!("❌ 出错：{e}\n\n"));
    }
    if !text.is_empty() {
        out.push_str(text);
    }
    if !tool_calls.is_empty() {
        // 按工具名计数：Bash(5) Read(3)。
        let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
        for (t, _) in tool_calls {
            *counts.entry(t.as_str()).or_default() += 1;
        }
        let stats: Vec<String> = counts.iter().map(|(t, n)| format!("{t}({n})")).collect();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("🔧 工具：{}", stats.join(" ")));
    }
    out.push_str("\n\n✅ 完成");
    out
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

/// 审批询问卡片（P4-4）：markdown 说明 + 允许/拒绝按钮。
///
/// 按钮 `behaviors` 走 callback：点击后飞书推 `card.action.trigger` 事件，value 原样
/// 带回（我们编码 conv + 动作），proto 侧解析成 `text="y"/"n"` 的入站消息复用审批
/// 回复路由。`conv` 必须编码进 value——回调事件本身不含目标会话。
///
/// 真机校准（2026-08）：schema V2 卡片已**废弃 `action` 元素**（200861 "cards of
/// schema V2 no longer support this capability; unsupported tag action"）。按钮迁到
/// `column_set` → `column` → `button`（button 组件本身 + behaviors 保留），两列等宽。
pub fn render_permission_card(tool_name: &str, input_summary: &str, conv_id: &str) -> String {
    serde_json::json!({
        "schema": "2.0",
        "body": { "elements": [
            { "tag": "markdown", "content": format!("🔐 请求执行 `{tool_name}`：{input_summary}") },
            { "tag": "column_set", "columns": [
                { "tag": "column", "width": "weighted", "weight": 1,
                  "elements": [
                    { "tag": "button", "text": { "tag": "plain_text", "content": "✅ 允许" },
                      "type": "primary",
                      "behaviors": [{ "type": "callback", "value": {
                        "imagent_perm": "allow", "conv": conv_id
                      } }] }
                  ] },
                { "tag": "column", "width": "weighted", "weight": 1,
                  "elements": [
                    { "tag": "button", "text": { "tag": "plain_text", "content": "⛔ 拒绝" },
                      "type": "danger",
                      "behaviors": [{ "type": "callback", "value": {
                        "imagent_perm": "deny", "conv": conv_id
                      } }] }
                  ] }
            ]}
        ]}
    })
    .to_string()
}

/// 审批询问的「已中断」终态卡（P5-16：`/stop` 中断任务时把滞留的询问卡 patch 成
/// 此内容——移除按钮，防止用户对一个已死的任务做审批）。
pub fn render_permission_card_cancelled(tool_name: &str) -> String {
    serde_json::json!({
        "schema": "2.0",
        "body": { "elements": [
            { "tag": "markdown", "content": format!("⏹️ `{tool_name}` 的执行询问已随任务中断，无需处理。") }
        ]}
    })
    .to_string()
}

/// 审批询问的「已处理」终态卡（真机校准 2026-08 UX：用户点按钮后卡片立即收敛，
/// 而非保持可点的询问态直到任务结束才见反馈）。
pub fn render_permission_card_resolved(tool_name: &str, allowed: bool) -> String {
    let mark = if allowed { "✅" } else { "⛔" };
    let verb = if allowed { "已批准" } else { "已拒绝" };
    serde_json::json!({
        "schema": "2.0",
        "body": { "elements": [
            { "tag": "markdown", "content": format!("{mark} `{tool_name}` 的执行询问{verb}，任务继续处理中。") }
        ]}
    })
    .to_string()
}

/// 命令交互卡片（P6-3）：markdown 正文 + 按钮组（点击 = 注入 `imagent_cmd` 命令，
/// 走与手打命令相同的鉴权/分派路径）。
///
/// 按钮挂 `column_set`（V2 已废弃 `action` 元素，同审批卡），每行至多 3 列防挤压；
/// 超出换行。`conv` 编码进 value——`card.action.trigger` 回调不含目标会话。
pub fn render_command_card(
    title: &str,
    body_md: &str,
    buttons: &[CardButton],
    conv_id: &str,
) -> String {
    let mut content = String::new();
    if !title.trim().is_empty() {
        content.push_str(&format!("**{title}**\n\n"));
    }
    content.push_str(body_md);
    let mut elements = vec![serde_json::json!({ "tag": "markdown", "content": content })];
    // 每行 3 个按钮，多余换行（列等宽 weighted）。
    for chunk in buttons.chunks(3) {
        let columns: Vec<serde_json::Value> = chunk
            .iter()
            .map(|b| {
                serde_json::json!({
                    "tag": "column", "width": "weighted", "weight": 1,
                    "elements": [{
                        "tag": "button",
                        "text": { "tag": "plain_text", "content": b.label },
                        "behaviors": [{ "type": "callback", "value": {
                            "imagent_cmd": b.command, "conv": conv_id
                        } }]
                    }]
                })
            })
            .collect();
        elements.push(serde_json::json!({
            "tag": "column_set", "columns": columns
        }));
    }
    serde_json::json!({
        "schema": "2.0",
        "body": { "elements": elements }
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
        assert!(
            json.contains("正在执行任务"),
            "Running 态应含自定义 summary: {json}"
        );
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

    /// P6-3：命令卡片——标题/正文 + 按钮（column_set 挂载、value 编码命令与 conv、
    /// 超过 3 个换行）。
    #[test]
    fn render_command_card_buttons_and_layout() {
        let buttons = vec![
            CardButton {
                label: "使用 main".into(),
                command: "/ws use main".into(),
            },
            CardButton {
                label: "使用 web".into(),
                command: "/ws use web".into(),
            },
            CardButton {
                label: "使用 cli".into(),
                command: "/ws use cli".into(),
            },
            CardButton {
                label: "接管 1".into(),
                command: "/resume 1".into(),
            },
        ];
        let json = render_command_card("📁 工作空间", "- main：/a/b", &buttons, "feishu:oc_g");
        assert!(json.contains("📁 工作空间"), "标题: {json}");
        assert!(json.contains("- main：/a/b"), "正文: {json}");
        assert!(
            json.contains("\"imagent_cmd\":\"/ws use main\""),
            "命令编码: {json}"
        );
        assert!(
            json.contains("\"conv\":\"feishu:oc_g\""),
            "conv 编码: {json}"
        );
        assert!(
            json.contains("\"tag\":\"column_set\""),
            "V2 按钮须挂 column_set: {json}"
        );
        assert!(
            !json.contains("\"tag\":\"action\""),
            "V2 已废弃 action 元素: {json}"
        );
        // 4 个按钮 → 2 个 column_set（每行 3 个）。
        assert_eq!(
            json.matches("\"tag\":\"column_set\"").count(),
            2,
            "超过 3 个按钮应换行: {json}"
        );
        // 空按钮：纯 markdown 卡，无 column_set。
        let no_btn = render_command_card("t", "body", &[], "feishu:oc_g");
        assert!(!no_btn.contains("column_set"));
        assert!(no_btn.contains("body"));
    }

    #[test]
    fn render_permission_card_buttons_and_conv() {
        let json = render_permission_card("Bash", r#"{"cmd":"rm -rf …"}"#, "feishu:ou_u1");
        // 两个按钮 + callback value 编码 conv 与动作。
        assert!(json.contains("✅ 允许"), "允许按钮: {json}");
        assert!(json.contains("⛔ 拒绝"), "拒绝按钮: {json}");
        assert!(
            json.contains("\"imagent_perm\":\"allow\"")
                && json.contains("\"imagent_perm\":\"deny\""),
            "两个动作都应编码: {json}"
        );
        assert!(json.contains("feishu:ou_u1"), "conv 应编码进 value: {json}");
        assert!(json.contains("\"tag\":\"button\""), "按钮 tag: {json}");
        // 真机校准（2026-08）：V2 已废弃 action 元素——按钮必须在 column_set 内，
        // 且不再出现 "tag":"action"（200861 会被拒）。
        assert!(
            json.contains("\"tag\":\"column_set\""),
            "V2 按钮须挂 column_set: {json}"
        );
        assert!(
            !json.contains("\"tag\":\"action\""),
            "V2 卡片不应再含 action 元素: {json}"
        );
        assert!(json.contains("Bash"), "工具名: {json}");
    }

    #[test]
    fn stream_init_card_has_element_id_and_streaming() {
        let json = render_stream_init_card();
        assert!(json.contains("element_id"), "初始卡应含 element_id: {json}");
        assert!(json.contains("md_body"), "正文组件锚点: {json}");
        assert!(json.contains("\"streaming_mode\":true"), "应开流式: {json}");
        assert!(json.contains("正在执行任务"), "应含自定义 summary: {json}");
    }

    #[test]
    fn stream_body_md_text_tools_and_empty() {
        // 空入参给占位。
        assert_eq!(stream_body_md("", &[]), "…");
        // 文本 + 工具都有。
        let tools = vec![("Bash".to_string(), "ls -la".to_string())];
        let md = stream_body_md("进度", &tools);
        assert!(md.contains("进度"));
        assert!(md.contains("🔧 `Bash`"), "工具列表: {md}");
        // 仅工具（无正文）。
        let only = stream_body_md("", &tools);
        assert!(only.starts_with("- 🔧"), "无正文时工具列表开头: {only}");
    }

    #[test]
    fn stream_body_final_stats_and_done() {
        let tools = vec![
            ("Bash".to_string(), "a".to_string()),
            ("Bash".to_string(), "b".to_string()),
            ("Read".to_string(), "c".to_string()),
        ];
        let out = stream_body_final("结论", &tools, None);
        assert!(out.contains("结论"));
        assert!(out.contains("Bash(2)"), "工具统计: {out}");
        assert!(out.contains("Read(1)"), "工具统计: {out}");
        assert!(out.contains("✅ 完成"));
        // Error 终态带 ❌ 前置。
        let err = stream_body_final("", &[], Some("boom"));
        assert!(err.contains("❌ 出错：boom"), "错误前置: {err}");
    }
}
