//! 飞书交互卡片渲染：把平台无关的 [`OutboundCard`] 渲染成飞书 CardKit 2.0 JSON。
//!
//! P8-1 视觉改版（对标 lcab / lark-coding-agent-bridge 的卡片风格）：
//! - 工具行带状态图标（⏳ 执行中 → ✅ 已完成）+ 人可读摘要（`Bash — git status`）
//! - Running 卡分阶段 footer：🧠 思考中 / 🧰 调用工具 / ✍️ 输出中
//! - 审批卡/问题卡/命令卡带卡片级标题栏（header + 主题色）
//! - 折叠面板带边框/圆角/内边距/小字号（notation），lcab 生产验证过的字段集

use imagent_core::render::{tool_card_line, tool_summary};
use imagent_core::{CardButton, CardButtonStyle, CardPhase, CardTerminal, OutboundCard, ToolCall};

/// 流式中工具行的展示上限：超出折叠成 `… 前面还有 N 个`（防长任务把卡片正文刷爆）。
const STREAM_TOOL_LINES: usize = 5;

/// 审批卡详情代码块上限（卡片单元素 ~30KB，留足余量）。
const PERM_DETAIL_MAX: usize = 1000;

/// Running 阶段 → footer 文案（也用于 config.summary 预览）。
pub fn phase_footer(phase: CardPhase) -> &'static str {
    match phase {
        CardPhase::Thinking => "🧠 思考中…",
        CardPhase::ToolRunning => "🧰 正在调用工具…",
        CardPhase::Outputting => "✍️ 输出中…",
    }
}

/// 终态 footer 文案（`已中断` 单列——/stop 与卡片扫描的收敛语义，非出错）。
fn terminal_footer(err: Option<&str>) -> &'static str {
    match err {
        Some("已中断") => "⏹ 已中断",
        Some(_) => "❌ 出错",
        None => "✅ 已完成",
    }
}

/// 渲染 [`OutboundCard`] 为飞书 interactive 卡片的 content JSON 字符串
/// （配合 `msg_type = "interactive"` 发送 / patch）。
///
/// markdown 文本块 + 工具调用折叠面板 + 状态 footer。
/// 这是**降级路径**的渲染（managed 真流式路径见 [`render_stream_init_card`]）。
pub fn render_card(card: &OutboundCard) -> String {
    let (footer, streaming, err) = match &card.terminal {
        CardTerminal::Running => (phase_footer(card.phase), true, None),
        CardTerminal::Done => ("✅ 已完成", false, None),
        CardTerminal::Error(e) => (terminal_footer(Some(e)), false, Some(e.as_str())),
    };
    let text = if card.text.is_empty() {
        "…"
    } else {
        &card.text
    };
    // Error 终态：错误行前置（终态 footer 只有一句 ❌，具体原因须进正文）。
    let text: std::borrow::Cow<str> = match err {
        Some(e) => format!("❌ 出错：{e}\n\n{text}").into(),
        None => text.into(),
    };
    let mut elements = vec![serde_json::json!({ "tag": "markdown", "content": text })];
    if !card.tool_calls.is_empty() {
        elements.push(render_tool_panel(&card.tool_calls));
    }
    // 状态 footer：note 行（notation 小字号）体现终态 / 流式阶段。
    elements.push(serde_json::json!({
        "tag": "markdown", "content": footer, "text_size": "notation"
    }));

    // Running 态带自定义 summary（卡片列表预览/通知处显示，默认「生成中」）；
    // Done 态 streaming=false 不需要 summary。
    let config = if streaming {
        serde_json::json!({
            "streaming_mode": true,
            "summary": { "content": phase_footer(card.phase) }
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

/// 工具调用折叠面板（lcab collapsedToolSummary 同款）：蓝边框 + 圆角 + 内边距，
/// 收起态；正文为小字号（notation）的工具行列表，行首状态图标。
fn render_tool_panel(tools: &[ToolCall]) -> serde_json::Value {
    let n = tools.len();
    // 超长任务只保留最近 STREAM_TOOL_LINES 行（完整过程在最终统计里）。
    let (skipped, shown) = if n > STREAM_TOOL_LINES + 2 {
        (n - STREAM_TOOL_LINES, &tools[n - STREAM_TOOL_LINES..])
    } else {
        (0, tools)
    };
    let mut lines = String::new();
    if skipped > 0 {
        lines.push_str(&format!("- ☕ … 前面还有 {skipped} 个\n"));
    }
    for t in shown {
        lines.push_str(&format!("- {}\n", tool_card_line(t)));
    }
    serde_json::json!({
        "tag": "collapsible_panel",
        "expanded": false,
        "header": panel_header(&format!("🔧 工具调用（{n}）")),
        "border": { "color": "blue", "corner_radius": "5px" },
        "vertical_spacing": "8px",
        "padding": "8px 8px 8px 8px",
        "elements": [{ "tag": "markdown", "content": lines, "text_size": "notation" }]
    })
}

/// 折叠面板头（lcab panelHeader 同款）：markdown 标题 + 展开箭头图标。
fn panel_header(title_md: &str) -> serde_json::Value {
    serde_json::json!({
        "title": { "tag": "markdown", "content": title_md },
        "vertical_align": "center",
        "icon": { "tag": "standard_icon", "token": "down-small-ccm_outlined", "size": "16px 16px" },
        "icon_position": "follow_text",
        "icon_expanded_angle": -180
    })
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
            { "tag": "markdown", "element_id": "md_footer", "content": "🧠 思考中…", "text_size": "notation" }
        ] }
    })
    .to_string()
}

/// Running 期间 `md_body` 的流式内容：累积正文 + 工具调用紧凑列表。
///
/// 工具与正文同置一个 markdown 组件——CardKit 的 element 流式 PATCH 仅支持
/// markdown 组件（折叠面板不可流式更新），故 managed 路径下工具以引用行进正文
/// （lcab 文本模式的 `> ⏳ **Bash** — cmd` 同款）。
pub fn stream_body_md(text: &str, tool_calls: &[ToolCall]) -> String {
    let mut out = String::new();
    if !text.is_empty() {
        out.push_str(text);
    }
    if !tool_calls.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let n = tool_calls.len();
        let (skipped, shown) = if n > STREAM_TOOL_LINES {
            (n - STREAM_TOOL_LINES, &tool_calls[n - STREAM_TOOL_LINES..])
        } else {
            (0, tool_calls)
        };
        if skipped > 0 {
            out.push_str(&format!("> ☕ … 前面还有 {skipped} 个工具\n"));
        }
        let lines: Vec<String> = shown
            .iter()
            .map(|t| format!("> {}", tool_card_line(t)))
            .collect();
        out.push_str(&lines.join("\n"));
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
pub fn stream_body_final(text: &str, tool_calls: &[ToolCall], err: Option<&str>) -> String {
    let mut out = String::new();
    // 错误/中断说明进正文（footer 只有一句状态，装不下具体原因）；中断单列措辞。
    if let Some(e) = err {
        if e == "已中断" {
            out.push_str("⏹ 已中断\n\n");
        } else {
            out.push_str(&format!("❌ 出错：{e}\n\n"));
        }
    }
    if !text.is_empty() {
        out.push_str(text);
    }
    if !tool_calls.is_empty() {
        // 按工具名计数：Bash×2 Read×3。
        let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
        for t in tool_calls {
            *counts.entry(t.name.as_str()).or_default() += 1;
        }
        let stats: Vec<String> = counts.iter().map(|(t, n)| format!("{t}×{n}")).collect();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!(
            "🔧 工具 {} 次：{}",
            tool_calls.len(),
            stats.join(" · ")
        ));
    }
    // 终态状态行（✅ 已完成等）由 md_footer 承载——正文不再拼一份，
    // 否则同卡出现两行「完成」（真机反馈）。
    out
}

/// 终态「结果下沉」指针正文（P8-2）：本轮发过询问卡（流式卡已被顶离视口）时，
/// 流式卡正文收成一行状态 + 指针，完整结果以**新卡**重发在下方——用户读完
/// 审批卡往下看即是结论，无需回滚翻找第一张卡。
pub fn stub_body(tool_count: usize, err: Option<&str>) -> String {
    // 状态行（✅ 已完成 / ❌ 出错 / ⏹ 已中断）由 md_footer 承载，stub 正文
    // 只留统计 + 指针——不再拼状态词，防同卡双「完成」。
    match err {
        None if tool_count > 0 => format!("🔧 工具 {tool_count} 次\n\n⬇️ 完整结果见下方消息"),
        None => "⬇️ 完整结果见下方消息".to_string(),
        Some(_) => "⬇️ 详情见下方消息".to_string(),
    }
}

/// 降级/话题路径（`msg:` 句柄）整卡 patch 用的 stub 卡（managed 路径用
/// [`stub_body`] patch `md_body`，语义相同）。
pub fn render_stub_card(card: &OutboundCard) -> String {
    let err = match &card.terminal {
        CardTerminal::Error(e) => Some(e.as_str()),
        _ => None,
    };
    serde_json::json!({
        "schema": "2.0",
        "config": { "streaming_mode": false },
        "body": { "elements": [
            { "tag": "markdown", "content": stub_body(card.tool_calls.len(), err) },
            // 状态行在此卡的 footer 元素承载（managed 路径由 patch_footer 等价提供）。
            { "tag": "markdown", "content": terminal_footer(err), "text_size": "notation" }
        ] }
    })
    .to_string()
}

/// 审批卡详情：工具签名行 + 参数代码块。
///
/// - Bash/shell → ```bash 命令
/// - 其它工具 → 解析 JSON 走 pretty 打印（解析失败回退原始串）
fn perm_detail_md(tool_name: &str, input_summary: &str) -> String {
    let summary = tool_summary(tool_name, input_summary);
    let head = if summary.is_empty() {
        format!("**{tool_name}**")
    } else {
        format!("**{tool_name}** — {summary}")
    };
    let lang = if tool_name == "Bash" || tool_name == "shell" {
        "bash"
    } else {
        ""
    };
    let body: String = match serde_json::from_str::<serde_json::Value>(input_summary) {
        Ok(v) => {
            let pretty = serde_json::to_string_pretty(&v).unwrap_or_else(|_| input_summary.into());
            truncate_str(&pretty, PERM_DETAIL_MAX)
        }
        // 截断的 JSON（超长输入）：解析失败原样展示。
        Err(_) => truncate_str(input_summary, PERM_DETAIL_MAX),
    };
    format!("{head}\n```{lang}\n{body}\n```")
}

/// 审批询问卡片（P4-4）：标题栏 + 工具签名/参数详情 + 允许/拒绝按钮。
///
/// 按钮 `behaviors` 走 callback：点击后飞书推 `card.action.trigger` 事件，value 原样
/// 带回（我们编码 conv + 动作），proto 侧解析成 `text="y"/"n"` 的入站消息复用审批
/// 回复路由。`conv` 必须编码进 value——回调事件本身不含目标会话。
///
/// 真机校准（2026-08）：schema V2 卡片已**废弃 `action` 元素**（200861 "cards of
/// schema V2 no longer support this capability; unsupported tag action"）。按钮迁到
/// `column_set` → `column` → `button`（button 组件本身 + behaviors 保留），两列等宽。
pub fn render_permission_card(
    tool_name: &str,
    input_summary: &str,
    conv_id: &str,
    request_id: &str,
) -> String {
    serde_json::json!({
        "schema": "2.0",
        "header": {
            "title": { "tag": "plain_text", "content": "🔐 权限审批" },
            "template": "orange"
        },
        "body": { "elements": [
            { "tag": "markdown", "content": perm_detail_md(tool_name, input_summary) },
            { "tag": "markdown", "content": "⏱️ 长时间未处理将自动拒绝", "text_size": "notation" },
            { "tag": "column_set", "columns": [
                { "tag": "column", "width": "weighted", "weight": 1,
                  "elements": [
                    { "tag": "button", "text": { "tag": "plain_text", "content": "✅ 允许" },
                      "type": "primary",
                      "behaviors": [{ "type": "callback", "value": {
                        "imagent_perm": "allow", "conv": conv_id, "req": request_id
                      } }] }
                  ] },
                { "tag": "column", "width": "weighted", "weight": 1,
                  "elements": [
                    { "tag": "button", "text": { "tag": "plain_text", "content": "⛔ 拒绝" },
                      "type": "danger",
                      "behaviors": [{ "type": "callback", "value": {
                        "imagent_perm": "deny", "conv": conv_id, "req": request_id
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
        "header": { "title": { "tag": "plain_text", "content": "⏹ 询问已结束" }, "template": "grey" },
        "body": { "elements": [
            { "tag": "markdown", "content": format!("`{tool_name}` 的本次询问已结束（任务中断/审批超时/被后续询问取代），无需处理。") }
        ]}
    })
    .to_string()
}

/// 询问被**新询问取代**的终态（并发 permission_request 顶掉了旧的）。
pub fn render_permission_card_superseded(tool_name: &str) -> String {
    serde_json::json!({
        "schema": "2.0",
        "header": { "title": { "tag": "plain_text", "content": "⏭️ 已被新询问取代" }, "template": "grey" },
        "body": { "elements": [
            { "tag": "markdown", "content": format!("`{tool_name}` 的询问已被更新的询问取代（agent 并发请求时旧请求自动拒绝），请处理最新一张。") }
        ]}
    })
    .to_string()
}

/// agent 问题卡（P6：AskUserQuestion 透传）：标题栏 + 问题正文 + 选项按钮。
///
/// 输入是 AskUserQuestion 工具的 input JSON（`questions[0].question/options`），
/// 解析失败返回 None（调用方降级普通审批卡）。选项按钮 value 编码
/// `imagent_ask`（选项文本）+ conv，回调转成 `ask:<选项>` 走审批回复路由。
pub fn render_question_card(tool_input: &str, conv_id: &str, request_id: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(tool_input).ok()?;
    let q = v.pointer("/questions/0")?;
    let question = q.get("question")?.as_str()?.trim().to_string();
    let opts = q
        .get("options")?
        .as_array()?
        .iter()
        .filter_map(|o| o.get("label")?.as_str().map(String::from))
        .collect::<Vec<_>>();
    if question.is_empty() || opts.is_empty() {
        return None;
    }
    let multi = v
        .pointer("/questions/0/multiSelect")
        .and_then(|m| m.as_bool())
        .unwrap_or(false);
    let n_questions = v
        .pointer("/questions")
        .and_then(|q| q.as_array())
        .map(|a| a.len())
        .unwrap_or(1);
    let mut extra = String::new();
    if n_questions > 1 {
        extra.push_str(&format!("\n（本次共 {n_questions} 个问题，先答第一个）"));
    }
    if multi {
        extra.push_str("\n（多选：依次点击多个选项）");
    }
    // 每选项一列；超 4 个截断（卡片宽度），剩余以文字列出。首选项 primary 高亮。
    let shown: Vec<&String> = opts.iter().take(4).collect();
    let mut columns = Vec::new();
    for (i, label) in shown.iter().enumerate() {
        let btn_type = if i == 0 { "primary" } else { "default" };
        columns.push(serde_json::json!({
            "tag": "column", "width": "weighted", "weight": 1,
            "elements": [{
                "tag": "button",
                "text": { "tag": "plain_text", "content": format!("{}. {}", i + 1, label) },
                "type": btn_type,
                "behaviors": [{ "type": "callback", "value": {
                    "imagent_ask": label, "conv": conv_id, "req": request_id
                } }]
            }]
        }));
    }
    let mut content = format!("❓ {question}{extra}");
    if opts.len() > 4 {
        let rest: Vec<&str> = opts.iter().skip(4).map(|s| s.as_str()).collect();
        content.push_str(&format!(
            "\n其余选项（回复 `ask:选项`）：{}",
            rest.join(" / ")
        ));
    }
    Some(
        serde_json::json!({
            "schema": "2.0",
            "header": {
                "title": { "tag": "plain_text", "content": "❓ 需要你的输入" },
                "template": "blue"
            },
            "body": { "elements": [
                { "tag": "markdown", "content": content },
                { "tag": "column_set", "columns": columns }
            ]}
        })
        .to_string(),
    )
}

/// 问题卡的「已记录选择」终态（区别于审批卡的已批准/已拒绝）。
pub fn render_question_card_resolved(choice: &str) -> String {
    serde_json::json!({
        "schema": "2.0",
        "header": { "title": { "tag": "plain_text", "content": "✅ 已记录选择" }, "template": "grey" },
        "body": { "elements": [
            { "tag": "markdown", "content": format!("已记录你的选择：{choice}。任务继续处理中。") }
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
        "header": { "title": { "tag": "plain_text", "content": format!("{mark} {verb}") }, "template": "grey" },
        "body": { "elements": [
            { "tag": "markdown", "content": format!("`{tool_name}` 的执行询问{verb}，任务继续处理中。") }
        ]}
    })
    .to_string()
}

/// 按钮样式 → 飞书 button type。
fn button_type(style: CardButtonStyle) -> &'static str {
    match style {
        CardButtonStyle::Default => "default",
        CardButtonStyle::Primary => "primary",
        CardButtonStyle::Danger => "danger",
    }
}

/// 命令交互卡片（P6-3）：标题栏 + markdown 正文 + 按钮组（点击 = 注入
/// `imagent_cmd` 命令，走与手打命令相同的鉴权/分派路径）。
///
/// P8-1：标题进卡片级 header（蓝色主题），按钮按 [`CardButtonStyle`] 分层
/// （primary 高亮推荐项 / danger 示警破坏项）。按钮挂 `column_set`（V2 已废弃
/// `action` 元素，同审批卡），每行至多 3 列防挤压；超出换行。`conv` 编码进
/// value——`card.action.trigger` 回调不含目标会话。
pub fn render_command_card(
    title: &str,
    body_md: &str,
    buttons: &[CardButton],
    conv_id: &str,
) -> String {
    let mut card = serde_json::json!({
        "schema": "2.0",
        "header": {
            "title": { "tag": "plain_text", "content": if title.trim().is_empty() { "imagent" } else { title } },
            "template": "blue"
        },
        "body": { "elements": [
            { "tag": "markdown", "content": body_md }
        ] }
    });
    if buttons.is_empty() {
        return card.to_string();
    }
    // 每行 3 个按钮，多余换行（列等宽 weighted）。
    let mut rows = Vec::new();
    for chunk in buttons.chunks(3) {
        let columns: Vec<serde_json::Value> = chunk
            .iter()
            .map(|b| {
                serde_json::json!({
                    "tag": "column", "width": "weighted", "weight": 1,
                    "elements": [{
                        "tag": "button",
                        "text": { "tag": "plain_text", "content": b.label },
                        "type": button_type(b.style),
                        "behaviors": [{ "type": "callback", "value": {
                            "imagent_cmd": b.command, "conv": conv_id
                        } }]
                    }]
                })
            })
            .collect();
        rows.push(serde_json::json!({ "tag": "column_set", "columns": columns }));
    }
    if let Some(elements) = card
        .pointer_mut("/body/elements")
        .and_then(|e| e.as_array_mut())
    {
        elements.extend(rows);
    }
    card.to_string()
}

/// 按 char 截断（避免半截 UTF-8）。
fn truncate_str(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use imagent_core::CardTerminal;

    fn tool(name: &str, summary: &str, done: bool) -> ToolCall {
        ToolCall {
            name: name.into(),
            summary: summary.into(),
            done,
        }
    }

    #[test]
    fn render_running_has_markdown() {
        let card = OutboundCard {
            text: "hello".into(),
            tool_calls: vec![],
            phase: CardPhase::Thinking,
            terminal: CardTerminal::Running,
        };
        let json = render_card(&card);
        assert!(json.contains("hello"));
        assert!(json.contains("schema"));
        assert!(json.contains("思考中"), "分阶段 footer: {json}");
        assert!(
            json.contains("正在执行任务") || json.contains("思考中"),
            "Running 态应含 summary: {json}"
        );
    }

    /// P8-1：分阶段 footer——思考/调用工具/输出各有文案。
    #[test]
    fn render_running_phase_footers() {
        for (phase, mark) in [
            (CardPhase::Thinking, "🧠 思考中…"),
            (CardPhase::ToolRunning, "🧰 正在调用工具…"),
            (CardPhase::Outputting, "✍️ 输出中…"),
        ] {
            let card = OutboundCard {
                text: "x".into(),
                tool_calls: vec![],
                phase,
                terminal: CardTerminal::Running,
            };
            assert!(render_card(&card).contains(mark), "{phase:?} → {mark}");
        }
    }

    #[test]
    fn render_done_with_tools() {
        let card = OutboundCard {
            text: "done".into(),
            tool_calls: vec![tool("Read", "src/main.rs", true)],
            phase: CardPhase::Outputting,
            terminal: CardTerminal::Done,
        };
        let json = render_card(&card);
        assert!(json.contains("done"));
        assert!(json.contains("Read"));
        assert!(json.contains("✅ 已完成"));
        // 工具面板：lcab 风格折叠面板（边框/内边距/小字号/状态图标）。
        assert!(json.contains("collapsible_panel"), "折叠面板: {json}");
        assert!(json.contains("corner_radius"), "面板边框: {json}");
        assert!(json.contains("notation"), "小字号: {json}");
        assert!(json.contains("✅ **Read**"), "工具状态行: {json}");
    }

    /// 长任务工具列表折叠：仅保留最近 N 行 + 计数行。
    #[test]
    fn render_card_tool_panel_collapses_long_list() {
        let tools: Vec<ToolCall> = (0..10)
            .map(|i| tool("Bash", &format!("cmd-{i}"), true))
            .collect();
        let card = OutboundCard {
            text: "out".into(),
            tool_calls: tools,
            phase: CardPhase::ToolRunning,
            terminal: CardTerminal::Running,
        };
        let json = render_card(&card);
        assert!(json.contains("前面还有 5"), "折叠计数行: {json}");
        assert!(!json.contains("cmd-0"), "最早的不展示: {json}");
        assert!(json.contains("cmd-9"), "最新的一直可见: {json}");
    }

    #[test]
    fn render_error() {
        let card = OutboundCard {
            text: "".into(),
            tool_calls: vec![],
            phase: CardPhase::Thinking,
            terminal: CardTerminal::Error("boom".into()),
        };
        let json = render_card(&card);
        assert!(json.contains("boom"));
        assert!(json.contains("❌ 出错"), "终态 footer: {json}");
    }

    /// P6：AskUserQuestion 输入 → 问题卡（标题栏 + 选项按钮 + imagent_ask value）。
    #[test]
    fn render_question_card_options_and_fallback() {
        let input = serde_json::json!({
            "questions": [{
                "question": "先做哪一步？",
                "options": [
                    {"label": "数据库迁移"},
                    {"label": "接口改造"},
                    {"label": "直接上线"}
                ]
            }]
        })
        .to_string();
        let json = render_question_card(&input, "feishu:ou_q", "req1").expect("应可渲染");
        assert!(json.contains("先做哪一步？"), "问题正文: {json}");
        assert!(json.contains("数据库迁移"), "选项文本: {json}");
        assert!(json.contains("需要你的输入"), "标题栏: {json}");
        assert!(json.contains("\"template\":\"blue\""), "主题色: {json}");
        assert!(
            json.contains("\"imagent_ask\":\"数据库迁移\""),
            "选项 value: {json}"
        );
        assert!(
            json.contains("\"type\":\"primary\""),
            "首选项 primary: {json}"
        );
        assert!(json.contains("feishu:ou_q"), "conv 编码: {json}");
        assert!(!json.contains("\"tag\":\"action\""), "V2 无 action: {json}");
        // 非法 JSON / 缺 options → None（降级审批卡）。
        assert!(render_question_card("not json", "c", "req1").is_none());
        assert!(render_question_card("{}", "c", "req1").is_none());
    }

    /// P6-3：命令卡片——标题栏 + 正文 + 按钮样式分层（primary/danger）、
    /// column_set 挂载、value 编码命令与 conv、超过 3 个换行。
    #[test]
    fn render_command_card_buttons_and_layout() {
        let buttons = vec![
            CardButton {
                label: "使用 main".into(),
                command: "/ws use main".into(),
                style: CardButtonStyle::Primary,
            },
            CardButton {
                label: "使用 web".into(),
                command: "/ws use web".into(),
                style: CardButtonStyle::Default,
            },
            CardButton {
                label: "使用 cli".into(),
                command: "/ws use cli".into(),
                style: CardButtonStyle::Default,
            },
            CardButton {
                label: "删除".into(),
                command: "/ws remove x".into(),
                style: CardButtonStyle::Danger,
            },
        ];
        let json = render_command_card("📁 工作空间", "- main：/a/b", &buttons, "feishu:oc_g");
        assert!(json.contains("📁 工作空间"), "标题栏: {json}");
        assert!(json.contains("\"template\":\"blue\""), "主题色: {json}");
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
            json.contains("\"type\":\"primary\"") && json.contains("\"type\":\"danger\""),
            "按钮样式分层: {json}"
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
        let json = render_permission_card(
            "Bash",
            r#"{"command":"cargo test --all"}"#,
            "feishu:ou_u1",
            "req1",
        );
        // 标题栏 + 主题色。
        assert!(json.contains("权限审批"), "标题栏: {json}");
        assert!(
            json.contains("\"template\":\"orange\""),
            "审批主题色: {json}"
        );
        // 签名行 + bash 代码块。
        assert!(
            json.contains("**Bash** — cargo test --all"),
            "签名行: {json}"
        );
        assert!(json.contains("```bash"), "bash 代码块: {json}");
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

    /// 非 Bash 工具的审批详情：pretty JSON 代码块 + 人可读签名行。
    #[test]
    fn render_permission_card_non_bash_pretty_json() {
        let json = render_permission_card(
            "Write",
            r##"{"file_path":"/a/b.md","content":"# hi"}"##,
            "feishu:ou_u1",
            "req1",
        );
        assert!(json.contains("**Write** — /a/b.md"), "签名行: {json}");
        // 序列化后内嵌引号成转义形态，断言裸字段名即可。
        assert!(json.contains("file_path"), "pretty JSON: {json}");
        assert!(
            !json.contains("```bash"),
            "非 bash 不用 bash 语言标注: {json}"
        );
    }

    #[test]
    fn stream_init_card_has_element_id_and_streaming() {
        let json = render_stream_init_card();
        assert!(json.contains("element_id"), "初始卡应含 element_id: {json}");
        assert!(json.contains("md_body"), "正文组件锚点: {json}");
        assert!(json.contains("\"streaming_mode\":true"), "应开流式: {json}");
        assert!(json.contains("正在执行任务"), "应含自定义 summary: {json}");
        assert!(json.contains("🧠 思考中…"), "初始 footer: {json}");
    }

    #[test]
    fn stream_body_md_text_tools_and_empty() {
        // 空入参给占位。
        assert_eq!(stream_body_md("", &[]), "…");
        // 文本 + 工具都有：引用行 + 状态图标 + 加粗工具名。
        let tools = vec![tool("Bash", "ls -la", false)];
        let md = stream_body_md("进度", &tools);
        assert!(md.contains("进度"));
        assert!(md.contains("⏳ **Bash** — ls -la"), "工具引用行: {md}");
        // 仅工具（无正文）。
        let only = stream_body_md("", &tools);
        assert!(only.starts_with("> ⏳"), "无正文时工具行开头: {only}");
        // 超出 5 个折叠 + 计数。
        let many: Vec<ToolCall> = (0..8)
            .map(|i| tool("Read", &format!("f{i}"), true))
            .collect();
        let md2 = stream_body_md("", &many);
        assert!(md2.contains("前面还有 3 个工具"), "折叠计数: {md2}");
        assert!(!md2.contains("f0"), "最早不展示: {md2}");
        assert!(md2.contains("f7"), "最新可见: {md2}");
    }

    /// P8-2：结果下沉 stub——状态 + 指针；错误/中断各有文案。
    #[test]
    fn stub_body_and_card() {
        // 状态词（完成/出错/中断）归 footer——stub 正文只有统计 + 指针。
        assert_eq!(stub_body(3, None), "🔧 工具 3 次\n\n⬇️ 完整结果见下方消息");
        assert_eq!(stub_body(0, None), "⬇️ 完整结果见下方消息");
        assert_eq!(stub_body(0, Some("boom")), "⬇️ 详情见下方消息");
        let card = OutboundCard {
            text: "结论".into(),
            tool_calls: vec![tool("Bash", "ls", true)],
            phase: CardPhase::Outputting,
            terminal: CardTerminal::Done,
        };
        let json = render_stub_card(&card);
        assert!(json.contains("⬇️ 完整结果见下方消息"), "指针: {json}");
        assert!(
            !json.contains("结论"),
            "stub 不含正文（正文在重发的新卡）: {json}"
        );
        assert!(json.contains("工具 1 次"), "统计行: {json}");
        // 状态词恰好出现一次（footer 元素）——正文不拼第二份。
        assert_eq!(
            json.matches("已完成").count(),
            1,
            "状态词只应出现一次（footer）: {json}"
        );
    }

    #[test]
    fn stream_body_final_stats_and_done() {
        let tools = vec![
            tool("Bash", "a", true),
            tool("Bash", "b", true),
            tool("Read", "c", true),
        ];
        let out = stream_body_final("结论", &tools, None);
        assert!(out.contains("结论"));
        assert!(out.contains("工具 3 次"), "总数: {out}");
        assert!(out.contains("Bash×2"), "工具统计: {out}");
        assert!(out.contains("Read×1"), "工具统计: {out}");
        // 状态行归 md_footer——正文不得再拼「完成」（真机反馈过双行）。
        assert!(!out.contains("完成"), "正文不应含状态词: {out}");
        // Error 终态带 ❌ 前置（具体原因正文承载）。
        let err = stream_body_final("", &[], Some("boom"));
        assert!(err.contains("❌ 出错：boom"), "错误前置: {err}");
        // 中断单列（非出错）。
        let stop = stream_body_final("", &[], Some("已中断"));
        assert!(
            stop.contains("⏹ 已中断") && !stop.contains("出错"),
            "中断终态: {stop}"
        );
    }
}
