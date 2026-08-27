//! 飞书交互卡片渲染：把平台无关的 [`OutboundCard`] 渲染成飞书 CardKit 2.0 JSON。
//!
//! P8-1 视觉改版（对标 lcab / lark-coding-agent-bridge 的卡片风格）：
//! - 工具行带状态图标（⏳ 执行中 → ✅ 已完成）+ 人可读摘要（`Bash — git status`）
//! - Running 卡分阶段 footer：🧠 思考中 / 🧰 调用工具 / ✍️ 输出中
//! - 审批卡/问题卡/命令卡带卡片级标题栏（header + 主题色）
//! - 折叠面板带边框/圆角/内边距/小字号（notation），lcab 生产验证过的字段集

use imagent_core::render::{tool_card_line, tool_summary};
use imagent_core::{
    CardButton, CardButtonStyle, CardPhase, CardTerminal, ConfigFormField, OutboundCard, ToolCall,
};

/// 邮箱掩码（lcab mask-email 同款）：飞书租户消息审计对含裸邮箱的出站内容回
/// 400（"contain sensitive data: EMAIL_ADDRESS"），流式卡会**静默失败**——典型
/// 触发是 git commit 的 Co-Authored-By 尾注。改写 `@` 为 `[at]`（刻意不用全角＠
/// 或零宽字符：中文审计会归一化还原后再次触发拦截；`[at]` 无法还原为合法地址）。
/// 点分 TLD 要求避开 npm scope（`@larksuite/x`）、版本号（`pkg@1.2.3`）与裸句柄；
/// SSH remote（`git@host.tld`）会被掩码——审计同样拦它，掩了才能发出去。
pub(crate) fn mask_emails(s: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"([A-Za-z0-9._%+-]+)@((?:[A-Za-z0-9-]+\.)+[A-Za-z]{2,})").unwrap()
    })
    .replace_all(s, "$1[at]$2")
    .into_owned()
}

/// 流式中工具行的展示上限：超出折叠成 `… 前面还有 N 个`（防长任务把卡片正文刷爆）。
const STREAM_TOOL_LINES: usize = 5;

/// 审批卡详情代码块上限（卡片单元素 ~30KB，留足余量）。
const PERM_DETAIL_MAX: usize = 1000;

/// 审批/问题卡的自动拒绝时长（分钟）。取 core `permission_ask_timeout_secs` 的
/// 缺省值（300s）；平台构造 API（`FeishuPlatform::new`）不接收该配置，无法逐
/// 实例感知——自定义了该配置的部署以配置为准（此文案为缺省提示）。
const ASK_AUTO_DENY_MINS: u64 = 5;

/// Running 阶段 → footer 文案（也用于 config.summary 预览）。
pub fn phase_footer(phase: CardPhase) -> &'static str {
    match phase {
        CardPhase::Thinking => "🧠 思考中…",
        CardPhase::ToolRunning => "🧰 正在调用工具…",
        CardPhase::Outputting => "✍️ 输出中…",
    }
}

/// P10：Running footer 组合——阶段文案 + 运行时长 + 排队提示
/// （`🧰 正在调用工具… · 30s · 📥 排队 2 条`）。
/// 排队状态"上卡不上消息流"：入队即被看见，不往会话里发任何确认消息。
/// 运行时长（`run_secs`，10s 粒度量化）区分「思考中」与「卡死」——长静默期
/// 用户可看到秒数仍在走；量化保证 footer 去重缓存命中（窗口内不重复 patch）。
pub fn running_footer(phase: CardPhase, queued_hint: Option<&str>, run_secs: u64) -> String {
    let mut out = phase_footer(phase).to_string();
    if run_secs > 0 {
        out.push_str(&format!(" · {run_secs}s"));
    }
    if let Some(h) = queued_hint {
        out.push_str(&format!(" · {h}"));
    }
    out
}

/// 终态 header 主题色（CardKit 视觉改版）：Done=green / Error=red / 已中断=grey，
/// Running 不加 header（保持现状——流式期正文持续变化，header 无信息量）。
/// 标题用终态状态词（与 footer 措辞一致，header 主题色 + footer 小字双锚）。
fn terminal_header(err: Option<&str>) -> serde_json::Value {
    let (title, template) = match err {
        Some("已中断") => ("⏹ 已中断", "grey"),
        Some(_) => ("❌ 出错", "red"),
        None => ("✅ 已完成", "green"),
    };
    serde_json::json!({
        "title": { "tag": "plain_text", "content": title },
        "template": template
    })
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
pub fn render_card(card: &OutboundCard, conv_id: &str) -> String {
    let (footer, streaming, err) = match &card.terminal {
        CardTerminal::Running => (
            running_footer(card.phase, card.queued_hint.as_deref(), card.run_secs),
            true,
            None,
        ),
        CardTerminal::Done => (
            match &card.usage_display {
                Some(u) => format!("✅ 已完成 · {u}"),
                None => "✅ 已完成".to_string(),
            },
            false,
            None,
        ),
        CardTerminal::Error(e) => (
            terminal_footer(Some(e)).to_string(),
            false,
            Some(e.as_str()),
        ),
    };
    let text = if card.text.is_empty() {
        // 明确状态语而非模糊的「…」：首 chunk 前的静默期（CLI 冷启动 + 模型
        // 首 token 可达十几秒）让用户确知任务已被接收处理。
        "🧠 已接收任务，正在处理…"
    } else {
        &card.text
    };
    // Error 终态：错误行前置（终态 footer 只有一句 ❌，具体原因须进正文）。
    let text: std::borrow::Cow<str> = match err {
        Some(e) => format!("❌ 出错：{e}\n\n{text}").into(),
        None => text.into(),
    };
    let mut elements =
        vec![serde_json::json!({ "tag": "markdown", "content": mask_emails(&text) })];
    if !card.tool_calls.is_empty() {
        // 长正文分段：正文与工具面板间用真 hr 组件分隔（降级路径专属——
        // managed 路径的 md_body 是单 markdown 组件，用 `---` 文本分割线，
        // 见 [`stream_body_final`]）。
        elements.push(serde_json::json!({ "tag": "hr" }));
        // tag 胶囊墙（CardKit tag 组件）：终态工具统计的胶囊化展示；markdown
        // 统计行（stream_body_final）仍是表格以外的兜底。Running 态不加
        // （统计未收敛）。
        if !streaming {
            elements.push(tool_tag_wall(&card.tool_calls));
        }
        // 面板边框随终态：Running=blue / Done=grey / Error=red。
        let border = if streaming {
            "blue"
        } else {
            border_color_of(err)
        };
        elements.push(render_tool_panel(&card.tool_calls, border));
    }
    // 状态 footer：note 行（notation 小字号）体现终态 / 流式阶段。
    elements.push(serde_json::json!({
        "tag": "markdown", "content": footer, "text_size": "notation"
    }));
    // Running 态带终止按钮（终态移除——整卡 patch 每次重渲染，自然消失）。
    if streaming {
        elements.push(stop_button(conv_id, None));
    }

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
    let mut card = serde_json::json!({
        "schema": "2.0",
        "config": config,
        "body": { "elements": elements }
    });
    // 终态 header 主题色（Done=green / Error=red / 已中断=grey）；Running 不加
    // （null header 字段不发送，保持现状）。managed 流式初始卡同样不加 header
    // （首帧后无法改，见 [`render_stream_init_card`]）。
    if let Some(h) = terminal_header_opt(streaming, err) {
        card["header"] = h;
    }
    card.to_string()
}

/// 终态 header（Running 回 None——不发送该字段）。
fn terminal_header_opt(running: bool, err: Option<&str>) -> Option<serde_json::Value> {
    if running {
        None
    } else {
        Some(terminal_header(err))
    }
}

/// 工具调用折叠面板（lcab collapsedToolSummary 同款）：边框 + 圆角 + 内边距，
/// 收起态；正文为小字号（notation）的工具行列表，行首状态图标。
///
/// 终态卡**全量罗列**（不截最近 5 条）——面板默认收起不占版面，展开即完整
/// 工具轨迹，终态后可回看明细（流式期只显最近 5 条，见 [`stream_body_md`]）。
///
/// 面板边框色随终态（CardKit 视觉改版）：Running=blue（进行中）/ Done=grey
/// （信息中性，不再抢视觉）/ Error=red（警示）。
fn render_tool_panel(tools: &[ToolCall], border_color: &str) -> serde_json::Value {
    let n = tools.len();
    let mut lines = String::new();
    for t in tools {
        lines.push_str(&format!("- {}\n", mask_emails(&tool_card_line(t))));
    }
    serde_json::json!({
        "tag": "collapsible_panel",
        "expanded": false,
        "header": panel_header(&format!("🔧 工具轨迹（{n}）")),
        "border": { "color": border_color, "corner_radius": "5px" },
        "vertical_spacing": "8px",
        "padding": "8px 8px 8px 8px",
        "elements": [{ "tag": "markdown", "content": lines, "text_size": "notation" }]
    })
}

/// 终态 → 工具面板边框色（Running=blue / Done=grey / Error=red）。
fn border_color_of(err: Option<&str>) -> &'static str {
    match err {
        Some("已中断") | None => "grey",
        Some(_) => "red",
    }
}

/// CardKit note 组件（提示条）：元信息/警示类注释行（审批倒计时、排队提示、
/// 掩码警告等）的小字提示形态。
///
/// 字段形态**待真机校准**（本项目未真机验证过 note 组件）；markdown 降级思路＝
/// 此前形态 `{ "tag": "markdown", "content": …, "text_size": "notation" }`
/// （note 不被租户卡片接受时回退该写法即可）。
fn note_element(text: &str) -> serde_json::Value {
    serde_json::json!({
        "tag": "note",
        "elements": [{ "tag": "plain_text", "content": mask_emails(text) }]
    })
}

/// CardKit tag 组件胶囊。
///
/// 字段形态**待真机校准**（本项目未真机验证过 tag 组件）；markdown 降级思路＝
/// 正文文本行（统计行 `Bash×5 · Read×3` / 列表行内 emoji 徽章）。
fn tag_pill(text: &str, color: &str) -> serde_json::Value {
    serde_json::json!({
        "tag": "tag",
        "text": { "tag": "plain_text", "content": text },
        "color": color
    })
}

/// 终态工具统计的 tag 胶囊墙（CardKit 视觉改版）：column_set flow + tag 胶囊
/// （`Bash×5` · `Read×3`，按名计数）。整卡路径（render_card / 结果下沉重发）
/// 的 elements 追加；markdown 统计行（stream_body_final / 表格以外场景）保留
/// 作兜底。
fn tool_tag_wall(tools: &[ToolCall]) -> serde_json::Value {
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for t in tools {
        *counts.entry(t.name.as_str()).or_default() += 1;
    }
    let pills: Vec<serde_json::Value> = counts
        .iter()
        .map(|(t, n)| tag_pill(&format!("{t}×{n}"), "turquoise"))
        .collect();
    serde_json::json!({
        "tag": "column_set", "flex_mode": "flow", "horizontal_spacing": "default",
        "columns": pills.into_iter()
            .map(|p| serde_json::json!({ "tag": "column", "width": "auto", "elements": [p] }))
            .collect::<Vec<_>>()
    })
}

/// 命令按钮 value 公共字段：命令 + conv + `ts`（epoch 秒，proto 回调侧超 24h 拒
/// 绝——卡片长期滞留 IM，过期上下文的命令点击应明确提示而非照旧执行）。
/// `sender`（发起轮次用户 open_id，群 conv 下校验点击者）仅终止按钮携带——命令
/// 卡按钮无「发起者」语义（命令卡由命令回执触发，非轮次锚定）。
fn cmd_value(conv_id: &str, command: &str, sender: Option<&str>) -> serde_json::Value {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut v = serde_json::json!({
        "imagent_cmd": command, "conv": conv_id, "ts": ts
    });
    if let Some(s) = sender.filter(|s| !s.is_empty()) {
        v["sender"] = serde_json::json!(s);
    }
    v
}

/// ⏹ 终止按钮（lcab stopButton 同款）：Running 态挂在卡片底部，点击回调注入
/// `/stop`（imagent_cmd 机制，走与手打命令相同的鉴权/分派）。`sender` 为发起
/// 轮次的用户 open_id——群 conv 下 proto 回调校验点击者须为发起者本人（他人
/// 点击回「仅发起者可操作」）；私聊不校验（单人）。
/// managed 卡终态后按钮无法移除（element PATCH 只能动 markdown）——点击回
/// 「当前没有运行中的任务」，无害。
fn stop_button(conv_id: &str, sender: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "tag": "column_set", "flex_mode": "flow", "horizontal_spacing": "default",
        "columns": [{
            "tag": "column", "width": "auto",
            "elements": [{
                "tag": "button",
                "text": { "tag": "plain_text", "content": "⏹ 终止" },
                "type": "danger",
                "behaviors": [{ "type": "callback", "value": cmd_value(conv_id, "/stop", sender) }]
            }]
        }]
    })
}

/// 按钮组 → flow 自适应 column_set（lcab 同款 `flex_mode: "flow"` + `width: auto`）：
/// 按内容宽度排列、自动换行，替代此前每行 3 个等宽的固定布局。
fn flow_button_row(buttons: &[serde_json::Value]) -> serde_json::Value {
    let columns: Vec<serde_json::Value> = buttons
        .iter()
        .map(|b| serde_json::json!({ "tag": "column", "width": "auto", "elements": [b] }))
        .collect();
    serde_json::json!({
        "tag": "column_set", "flex_mode": "flow", "horizontal_spacing": "default",
        "columns": columns
    })
}

/// 单个 callback 按钮的 JSON。
fn cb_button(label: &str, btn_type: &str, value: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": label },
        "type": btn_type,
        "behaviors": [{ "type": "callback", "value": value }]
    })
}

/// 带二次确认弹窗的 callback 按钮（CardKit button `confirm` 字段）：danger 类
/// 破坏性命令（/ws 删除等）点击先弹「确认执行」——按钮组件原生字段，无额外
/// 交互成本。
fn cb_button_confirm(
    label: &str,
    btn_type: &str,
    value: serde_json::Value,
    confirm_text: &str,
) -> serde_json::Value {
    let mut b = cb_button(label, btn_type, value);
    b["confirm"] = serde_json::json!({
        "title": { "tag": "plain_text", "content": "确认执行" },
        "text": { "tag": "plain_text", "content": confirm_text }
    });
    b
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
pub fn render_stream_init_card(conv_id: &str, sender: Option<&str>) -> String {
    serde_json::json!({
        "schema": "2.0",
        "config": {
            "streaming_mode": true,
            "summary": { "content": "🧠 正在执行任务…" }
        },
        "body": { "elements": [
            { "tag": "markdown", "element_id": "md_body", "content": "🧠 已接收任务，正在处理…" },
            { "tag": "markdown", "element_id": "md_footer", "content": "🧠 思考中…", "text_size": "notation" },
            // P9-1：⏹ 终止按钮常驻（element PATCH 只更新 markdown，按钮不受流式
            // 影响；终态后仍在，点击回「当前没有运行中的任务」，无害）。
            stop_button(conv_id, sender)
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
            // 图标统一（CardKit 视觉改版）：☕ → ⋯（省略号语义中性，与咖啡混淆无关）。
            out.push_str(&format!("> ⋯ 前面还有 {skipped} 个工具\n"));
        }
        let lines: Vec<String> = shown
            .iter()
            .map(|t| format!("> {}", tool_card_line(t)))
            .collect();
        out.push_str(&lines.join("\n"));
    }
    if out.is_empty() {
        out.push_str("🧠 已接收任务，正在处理…");
    }
    mask_emails(&out)
}

/// 终态（Done/Error）时 `md_body` 的最终内容：正文 + 工具统计行 + 全量工具明细。
///
/// 统计行给一眼结论（按工具名计数：Bash×2 Read×3）；其后附**全量**工具引用行
/// ——managed 流式期正文只显最近 5 条（element PATCH 限制下的防刷屏），终态
/// 在同组件里补全明细，用户终态后可回看完整工具轨迹（降级/下沉路径另有折叠
/// 面板承载，见 [`render_tool_panel`]）。
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
        // 长正文分段（CardKit 视觉改版）：正文与工具统计间 `---` 分割线 +
        // 工具明细块前小标题「工具轨迹」——managed 单 markdown 组件内用文本
        // 分割线（降级/整卡路径用真 hr 组件 + 面板标题，见 [`render_card`]）。
        if !out.is_empty() {
            out.push_str("\n\n---\n\n");
        }
        out.push_str(&format!(
            "🔧 工具 {} 次：{}\n\n**工具轨迹**\n",
            tool_calls.len(),
            stats.join(" · ")
        ));
        // 全量明细（引用行形态，与流式期一致）——终态回看用。
        let lines: Vec<String> = tool_calls
            .iter()
            .map(|t| format!("> {}", tool_card_line(t)))
            .collect();
        out.push_str(&lines.join("\n"));
    }
    // 终态状态行（✅ 已完成等）由 md_footer 承载——正文不再拼一份，
    // 否则同卡出现两行「完成」（真机反馈）。
    // P9-1：空正文 + 无工具的空产出给占位（空串 patch 组件可能被拒/显示空白）。
    if out.is_empty() {
        out.push_str("（未返回内容）");
    }
    mask_emails(&out)
}

/// 终态「结果下沉」指针正文（P8-2）：本轮发过询问卡（流式卡已被顶离视口）时，
/// 流式卡正文收成一行状态 + 指针，完整结果以**新卡**重发在下方——用户读完
/// 审批卡往下看即是结论，无需回滚翻找第一张卡。
pub fn stub_body(tool_count: usize, err: Option<&str>) -> String {
    // stub 正文自带终态状态词（✅ 任务完成 / ❌ 执行出错 / ⏹ 已中断）——stub
    // 卡常被审批卡顶到视口外、footer 小字易被忽略，正文状态词让回滚一眼辨成败；
    // 措辞刻意区别于 footer 的「✅ 已完成」（正文「任务完成」），避免同词双行。
    let status = match err {
        Some("已中断") => "⏹ 已中断",
        Some(_) => "❌ 执行出错",
        None => "✅ 任务完成",
    };
    let mut out = status.to_string();
    if err.is_none() && tool_count > 0 {
        out.push_str(&format!("\n\n🔧 工具 {tool_count} 次"));
    }
    out.push_str(&format!(
        "\n\n⬇️ {}见下方消息",
        if err.is_none() {
            "完整结果"
        } else {
            "详情"
        }
    ));
    out
}

/// 降级/话题路径（`msg:` 句柄）整卡 patch 用的 stub 卡（managed 路径用
/// [`stub_body`] patch `md_body`，语义相同）。
///
/// 终态 header 主题色（Done=green / Error=red / 已中断=grey）承载状态——
/// header 取代此前的 footer 小字状态行（managed 路径受限仍用 md_footer）。
pub fn render_stub_card(card: &OutboundCard) -> String {
    let err = match &card.terminal {
        CardTerminal::Error(e) => Some(e.as_str()),
        _ => None,
    };
    serde_json::json!({
        "schema": "2.0",
        "config": { "streaming_mode": false },
        "header": terminal_header(err),
        "body": { "elements": [
            { "tag": "markdown", "content": stub_body(card.tool_calls.len(), err) }
        ] }
    })
    .to_string()
}

/// 审批卡详情：工具签名行 + 参数代码块。
///
/// - Bash/shell → ```bash 命令
/// - 其它工具 → 解析 JSON 走 pretty 打印（解析失败回退原始串）
///
/// 返回 `(markdown 正文, note 提示条列表)`——截断/掩码警告等元信息类注释行
/// 走 CardKit note 组件（见 [`note_element`]），markdown 正文保留同文案作降级。
fn perm_detail_md(tool_name: &str, input_summary: &str) -> (String, Vec<String>) {
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
    let raw: String = match serde_json::from_str::<serde_json::Value>(input_summary) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| input_summary.into()),
        // 截断的 JSON（超长输入）：解析失败原样展示。
        Err(_) => input_summary.to_string(),
    };
    // 截断提示：静默截断会让用户误以为参数就这么多——末尾明示。
    let (body, truncated) = if raw.chars().count() > PERM_DETAIL_MAX {
        (truncate_str(&raw, PERM_DETAIL_MAX), true)
    } else {
        (raw, false)
    };
    // 邮箱掩码是平台合规强制（租户审计对含裸邮箱的卡片内容回 400，代码块
    // 同样被审计，无法豁免——见 [`mask_emails`]）。掩码处加提示，防用户复制
    // 掩码后的命令执行坏命令。
    let masked = mask_emails(&body);
    let email_masked = masked != body;
    let mut md = format!("{head}\n```{lang}\n{masked}\n```");
    // 截断提示进 note 元素（元信息类注释行 CardKit note 化，见 [`note_element`]）。
    let mut notes: Vec<String> = Vec::new();
    if truncated {
        notes.push(format!("…（已截断，仅显示前 {PERM_DETAIL_MAX} 字符）"));
    }
    if email_masked {
        notes.push("⚠️ 邮箱已掩码显示（`[at]`），原命令可直接执行，请勿复制此代码块。".into());
    }
    if !notes.is_empty() {
        md.push_str(&format!("\n\n{}", notes.join("\n\n")));
    }
    (md, notes)
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
/// 审批卡 note 行缺省文案：自动拒绝的具体倒计时（分钟数值来自
/// [`ASK_AUTO_DENY_MINS`]，即 core `permission_ask_timeout_secs` 缺省值）——
/// 静态「长时间未处理」让用户无从判断还剩多久。
pub(crate) fn perm_note_default() -> String {
    format!("⏱️ 将在 {ASK_AUTO_DENY_MINS} 分钟后自动拒绝 · 回复 always = 本次会话内此工具不再询问")
}

pub fn render_permission_card(
    tool_name: &str,
    input_summary: &str,
    conv_id: &str,
    request_id: &str,
) -> String {
    render_permission_card_note(
        tool_name,
        input_summary,
        conv_id,
        request_id,
        &perm_note_default(),
    )
}

/// P10-③：note 行可参数化（排队联动重渲染用，见 platform 的 note_queued_on_ask）。
pub(crate) fn render_permission_card_note(
    tool_name: &str,
    input_summary: &str,
    conv_id: &str,
    request_id: &str,
    note: &str,
) -> String {
    let (detail, detail_notes) = perm_detail_md(tool_name, input_summary);
    let mut elements = vec![
        serde_json::json!({ "tag": "markdown", "content": detail }),
        // 倒计时 / 排队提示 note（CardKit note 组件；此前为 markdown+notation，
        // 降级回退该形态即可）。md_footer 锚点不受影响（managed 卡约束）。
        note_element(note),
    ];
    // 截断 / 掩码警告同样 note 化（元信息类注释行）。
    for n in &detail_notes {
        elements.push(note_element(n));
    }
    elements.push(serde_json::json!({ "tag": "hr" }));
    elements.push(flow_button_row(&[
        cb_button(
            "允许",
            "primary",
            serde_json::json!({
                "imagent_perm": "allow", "conv": conv_id, "req": request_id
            }),
        ),
        cb_button(
            "♾️ 本次会话始终允许",
            "default",
            serde_json::json!({
                "imagent_perm": "always", "conv": conv_id, "req": request_id
            }),
        ),
        cb_button(
            "⛔ 拒绝",
            "danger",
            serde_json::json!({
                "imagent_perm": "deny", "conv": conv_id, "req": request_id
            }),
        ),
    ]));
    serde_json::json!({
        "schema": "2.0",
        "header": {
            "title": { "tag": "plain_text", "content": "🔐 权限审批" },
            "template": "orange"
        },
        "body": { "elements": elements }
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
            { "tag": "markdown", "content": format!("`{tool_name}` 的本次询问已结束，无需处理。") },
            // 中断/审批超时/被取代的原因说明走 note 提示条（元信息类注释行）。
            note_element("原因：任务中断 · 审批超时 · 被后续询问取代")
        ]}
    })
    .to_string()
}

/// 询问被**新询问取代**的终态（并发 permission_request 顶掉了旧的）。
pub fn render_permission_card_superseded(tool_name: &str) -> String {
    serde_json::json!({
        "schema": "2.0",
        "header": { "title": { "tag": "plain_text", "content": "🔁 已被新询问取代" }, "template": "grey" },
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
    render_question_card_note(tool_input, conv_id, request_id, &perm_note_default())
}

/// P10-③：note 行可参数化（同审批卡）。
///
/// 交互形态按选项数/多选分流（替代此前「>4 选项要求手打 `ask:选项`、多选第一
/// 次点击即收敛」的残缺交互）：
/// - 单选 ≤4 选项：选项按钮（首选项 primary）——原交互，最快路径；
/// - 单选 >4 选项：CardKit form + `select_static` 下拉（参照 /config 表单卡），
///   提交一次回传选择；
/// - 多选（multiSelect）：form + `checkbox`，勾选多项后一次提交全部——proto 侧
///   把 `form_value.ask_opt`（数组）按多选语义拼接（「、」连接）回 `ask:` 通道。
///   多问题场景（questions.len() > 1）当前只答第一问：卡片上明确标注。
pub(crate) fn render_question_card_note(
    tool_input: &str,
    conv_id: &str,
    request_id: &str,
    note: &str,
) -> Option<String> {
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
        extra.push_str(&format!(
            "\n（本次共 {n_questions} 个问题，将依次询问——此卡只答第一问）"
        ));
    }
    let use_form = multi || opts.len() > 4;
    let content = format!("❓ {question}{extra}");
    let body_elements: Vec<serde_json::Value> = if use_form {
        // 表单形态：选项 value 即 label（回传直接走 ask 通道语义，与按钮一致）。
        let options: Vec<serde_json::Value> = opts
            .iter()
            .map(|l| {
                serde_json::json!({
                    "text": { "tag": "plain_text", "content": l },
                    "value": l
                })
            })
            .collect();
        let field = if multi {
            serde_json::json!({
                "tag": "checkbox", "name": "ask_opt",
                "options": options
            })
        } else {
            serde_json::json!({
                "tag": "select_static", "name": "ask_opt",
                "options": options
            })
        };
        let submit_tip = if multi {
            "勾选后点「提交」，一次回传全部选择"
        } else {
            "下拉选择后点「提交」"
        };
        vec![
            serde_json::json!({ "tag": "markdown", "content": mask_emails(&content) }),
            note_element(note),
            serde_json::json!({ "tag": "hr" }),
            serde_json::json!({ "tag": "form", "name": "imagent_ask", "elements": [
                serde_json::json!({ "tag": "markdown", "content": submit_tip }),
                field,
                serde_json::json!({ "tag": "hr" }),
                flow_button_row(&[serde_json::json!({
                    "tag": "button",
                    "name": "submit_btn",
                    "text": { "tag": "plain_text", "content": "提交" },
                    "type": "primary",
                    "form_action_type": "submit",
                    "behaviors": [{ "type": "callback", "value": {
                        "imagent_form": "ask", "conv": conv_id, "req": request_id
                    } }]
                })])
            ]}),
        ]
    } else {
        // 按钮形态：每选项一钮（flow 自适应）；首选项 primary 高亮。
        let opt_buttons: Vec<serde_json::Value> = opts
            .iter()
            .enumerate()
            .map(|(i, label)| {
                let btn_type = if i == 0 { "primary" } else { "default" };
                cb_button(
                    &format!("{}. {}", i + 1, label),
                    btn_type,
                    serde_json::json!({
                        "imagent_ask": label, "conv": conv_id, "req": request_id
                    }),
                )
            })
            .collect();
        vec![
            serde_json::json!({ "tag": "markdown", "content": mask_emails(&content) }),
            note_element(note),
            serde_json::json!({ "tag": "hr" }),
            flow_button_row(&opt_buttons),
        ]
    };
    Some(
        serde_json::json!({
            "schema": "2.0",
            "header": {
                "title": { "tag": "plain_text", "content": "❓ 需要你的输入" },
                "template": "blue"
            },
            "body": { "elements": body_elements }
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

/// 单个 [`CardButton`] → callback 按钮 JSON（value 编码命令 + conv + ts；danger
/// 带二次确认弹窗）。
fn render_cmd_button(b: &CardButton, conv_id: &str) -> serde_json::Value {
    let value = cmd_value(conv_id, &b.command, None);
    if matches!(b.style, CardButtonStyle::Danger) {
        cb_button_confirm(
            &b.label,
            button_type(b.style),
            value,
            &format!("将执行「{}」，该操作可能删除/覆盖数据，确认吗？", b.command),
        )
    } else {
        cb_button(&b.label, button_type(b.style), value)
    }
}

/// 正文 markdown → 元素列表（/help 分组元素化）：按空行切块，**组标题独立成
/// markdown 元素**（单行、非列表/表格/标题的块用 heading-large 大字号——
/// text_size 支持性**待真机校准**，不支持时退化为独立元素+空行亦成立）；
/// 其余块原样。不再是一整块 400+ 字符 markdown。
fn body_block_elements(body_md: &str) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    for block in body_md.split("\n\n").filter(|b| !b.trim().is_empty()) {
        let mut lines = block.lines().peekable();
        while let Some(first) = lines.next() {
            let is_heading =
                !first.starts_with("- ") && !first.starts_with("|") && !first.starts_with('#');
            let rest: Vec<&str> = lines.clone().collect();
            if is_heading && !rest.is_empty() {
                // 组标题独立成 heading 元素（/help 的「🗂 会话\n- …」形态——
                // 标题与列表间无空行，块内再拆）。
                out.push(serde_json::json!({
                    "tag": "markdown", "content": first, "text_size": "heading-large"
                }));
            } else {
                // 标题后无内容（单行块）或列表/表格行：整体一个普通元素
                //（标题行并入，避免空元素）。
                let mut content = first.to_string();
                for l in rest {
                    content.push('\n');
                    content.push_str(l);
                }
                out.push(serde_json::json!({
                    "tag": "markdown", "content": mask_emails(&content)
                }));
                lines.by_ref().for_each(drop);
                break;
            }
        }
    }
    out
}

/// 解析 body_md 里的 markdown 表格为行单元格矩阵（含表头行；分隔行剔除）。
/// 无表格（< 表头 + 1 数据行）回 None——调用方走平铺降级布局。
fn table_rows(body_md: &str) -> Option<Vec<Vec<String>>> {
    let mut rows: Vec<Vec<String>> = body_md
        .lines()
        .filter(|l| l.trim().starts_with('|'))
        .map(|l| {
            l.trim()
                .trim_matches('|')
                .split('|')
                .map(|c| c.trim().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|cells| !cells.iter().all(|c| c.trim_matches([':', '-']).is_empty()))
        .collect();
    if rows.len() < 2 {
        return None;
    }
    // 兼容列数不齐（防御）：按表头列数截齐/补空。
    let n = rows[0].len();
    for r in &mut rows {
        r.resize(n, String::new());
    }
    Some(rows)
}

/// /resume 行的左列元素：来源徽章 tag 胶囊（列表行兜底文本里的 💻/📱 同步
/// tag 化——徽章用 tag，行内容保持 markdown 文本）+ 时间 · 内容。
///
/// tag 字段形态**待真机校准**；降级兜底＝表格行内 emoji 文本。
fn resume_row_left(cells: &[String]) -> Vec<serde_json::Value> {
    let mut els = Vec::new();
    if cells.len() > 1 {
        match cells[1].as_str() {
            "💻" => els.push(tag_pill("💻 本机", "blue")),
            "📱" => els.push(tag_pill("📱 IM", "green")),
            other if !other.is_empty() => els.push(tag_pill(other, "grey")),
            _ => {}
        }
    }
    let mut md = String::new();
    if cells.len() > 2 && !cells[2].is_empty() {
        md.push_str(&cells[2]);
    }
    if cells.len() > 3 && !cells[3].is_empty() {
        if !md.is_empty() {
            md.push_str(" · ");
        }
        md.push_str(&cells[3]);
    }
    if !md.is_empty() {
        els.push(serde_json::json!({ "tag": "markdown", "content": md }));
    }
    els
}

/// 按钮 label 约定配对的双列行布局（CardKit 视觉改版）：
/// - /resume：按钮「接管 N」↔ 表格行首列 N（`| # | 来源 | 时间 | 内容 |`）；
/// - /ws：按钮「使用 X」/「删除 X」↔ 表格行首列 X（`| 名称 | 路径 |`）。
///
/// 每个配对行一个 column_set 双列（左信息右按钮，weighted 4:1，weight 字段
/// **待真机校准**）；未配对的表格行（如 /resume 第 10+ 条）保留 markdown 表格
/// 片段兜底，未配对按钮收进底部 flow 行。配对不成立（无表格/约定不匹配）回
/// None，调用方走原平铺布局（markdown 降级思路）。
fn try_paired_rows(
    body_md: &str,
    buttons: &[CardButton],
    conv_id: &str,
) -> Option<Vec<serde_json::Value>> {
    let rows = table_rows(body_md)?;
    let data = &rows[1..];
    // /resume 模式：全部按钮形如「接管 N」。
    let resume_idx: Option<Vec<usize>> = buttons
        .iter()
        .map(|b| {
            b.label
                .strip_prefix("接管 ")
                .and_then(|n| n.parse::<usize>().ok())
        })
        .collect::<Option<Vec<_>>>();
    let mut elements: Vec<serde_json::Value> = Vec::new();
    let mut used_buttons: Vec<usize> = Vec::new();
    let mut unpaired_rows: Vec<&Vec<String>> = Vec::new();
    for row in data {
        let key = row.first()?.trim().to_string();
        if resume_idx.is_some() {
            if let Some(i) = resume_idx
                .as_ref()?
                .iter()
                .position(|n| n.to_string() == key)
            {
                let btn = render_cmd_button(&buttons[i], conv_id);
                elements.push(two_col_row(resume_row_left(row), vec![btn]));
                used_buttons.push(i);
                continue;
            }
        } else {
            // /ws 模式：按名称配「使用 X」/「删除 X」两钮。
            let mut row_btns: Vec<usize> = Vec::new();
            for (i, b) in buttons.iter().enumerate() {
                let name = b
                    .label
                    .strip_prefix("使用 ")
                    .or_else(|| b.label.strip_prefix("删除 "));
                if name == Some(key.as_str()) {
                    row_btns.push(i);
                }
            }
            if !row_btns.is_empty() {
                let left = vec![serde_json::json!({
                    "tag": "markdown",
                    "content": format!("**{key}**\n{}", row.get(1).map(String::as_str).unwrap_or(""))
                })];
                let btns: Vec<serde_json::Value> = row_btns
                    .iter()
                    .map(|i| render_cmd_button(&buttons[*i], conv_id))
                    .collect();
                elements.push(two_col_row(left, btns));
                used_buttons.extend(row_btns);
                continue;
            }
        }
        unpaired_rows.push(row);
    }
    if elements.is_empty() {
        return None; // 一行都没配上——约定不成立，走平铺降级。
    }
    // 未配对的表格行：markdown 表格片段兜底（含表头）。
    if !unpaired_rows.is_empty() {
        let mut md = String::from("|");
        md.push_str(&rows[0].join("|"));
        md.push_str("|\n");
        for r in &unpaired_rows {
            md.push_str(&format!("|{}|\n", r.join("|")));
        }
        md.push_str("\n（其余会话发送 /resume <序号> 接管）");
        elements.push(serde_json::json!({ "tag": "markdown", "content": md }));
    }
    // 未配对按钮：底部 flow 行。
    let leftover: Vec<serde_json::Value> = buttons
        .iter()
        .enumerate()
        .filter(|(i, _)| !used_buttons.contains(i))
        .map(|(_, b)| render_cmd_button(b, conv_id))
        .collect();
    if !leftover.is_empty() {
        elements.push(serde_json::json!({ "tag": "hr" }));
        elements.push(flow_button_row(&leftover));
    }
    Some(elements)
}

/// 双列行（column_set weighted 4:1）：左列信息、右列操作按钮。
/// `weight` 数值形态**待真机校准**（CardKit column 加权宽度）。
fn two_col_row(left: Vec<serde_json::Value>, right: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({
        "tag": "column_set",
        "flex_mode": "bisect",
        "horizontal_spacing": "default",
        "columns": [
            { "tag": "column", "width": "weighted", "weight": 4, "elements": left,
              "vertical_align": "center" },
            { "tag": "column", "width": "weighted", "weight": 1, "elements": right,
              "vertical_align": "center" }
        ]
    })
}

/// 命令交互卡片（P6-3）：标题栏 + markdown 正文 + 按钮组（点击 = 注入
/// `imagent_cmd` 命令，走与手打命令相同的鉴权/分派路径）。
///
/// CardKit 视觉改版：
/// - 正文按空行分块、组标题独立成 heading 元素（[`body_block_elements`]）；
/// - /resume、/ws 列表（markdown 表格数据）按按钮 label 约定配对成双列行
///   （[`try_paired_rows`]），配对不成立回退平铺（markdown + flow 按钮）。
/// - P8-1：标题进卡片级 header（蓝色主题），按钮按 [`CardButtonStyle`] 分层
///   （primary 高亮推荐项 / danger 示警破坏项）。按钮挂 `column_set`（V2 已废弃
///   `action` 元素，同审批卡）。`conv` 编码进 value——`card.action.trigger`
///   回调不含目标会话。
pub fn render_command_card(
    title: &str,
    body_md: &str,
    buttons: &[CardButton],
    conv_id: &str,
) -> String {
    // 优先尝试配对双列行布局（/resume「接管 N」/ /ws「使用 X」约定）。
    let elements = match try_paired_rows(body_md, buttons, conv_id) {
        Some(paired) => paired,
        None => {
            let mut els = body_block_elements(body_md);
            if !buttons.is_empty() {
                // P9-1：hr 分隔正文与按钮；flow 自适应单行布局（按内容宽度自动换行）。
                els.push(serde_json::json!({ "tag": "hr" }));
                let btns: Vec<serde_json::Value> = buttons
                    .iter()
                    .map(|b| render_cmd_button(b, conv_id))
                    .collect();
                els.push(flow_button_row(&btns));
            }
            els
        }
    };
    serde_json::json!({
        "schema": "2.0",
        "header": {
            "title": { "tag": "plain_text", "content": if title.trim().is_empty() { "imagent" } else { title } },
            "template": "blue"
        },
        "body": { "elements": elements }
    })
    .to_string()
}

/// P9-2：`/config` 偏好设置表单卡（CardKit 2.0 `form` + `select_static` 下拉 +
/// 提交按钮——lcab configFormCard 同款交互）。提交回调经 card.action.trigger 的
/// `form_value` 回传，proto 侧合成 `/config form k=v …` 命令文本（走与手打命令
/// 相同的鉴权/分派）。
pub fn render_config_form_card(entries: &[ConfigFormField], conv_id: &str) -> String {
    let mut form_elements: Vec<serde_json::Value> = Vec::new();
    for f in entries {
        let options: Vec<serde_json::Value> = f
            .options
            .iter()
            .map(|(value, label)| {
                serde_json::json!({
                    "text": { "tag": "plain_text", "content": label },
                    "value": value
                })
            })
            .collect();
        form_elements.push(serde_json::json!({
            "tag": "markdown",
            "content": format!("**{}**", f.label)
        }));
        form_elements.push(serde_json::json!({
            "tag": "select_static",
            "name": f.key,
            "initial_option": f.current,
            "options": options
        }));
    }
    form_elements.push(serde_json::json!({ "tag": "hr" }));
    form_elements.push(flow_button_row(&[serde_json::json!({
        "tag": "button",
        "name": "submit_btn",
        "text": { "tag": "plain_text", "content": "提交" },
        "type": "primary",
        "form_action_type": "submit",
        "behaviors": [{ "type": "callback", "value": {
            "imagent_form": "config", "conv": conv_id
        } }]
    })]));
    serde_json::json!({
        "schema": "2.0",
        "config": { "summary": { "content": "⚙️ 偏好设置" } },
        "header": {
            "title": { "tag": "plain_text", "content": "⚙️ 偏好设置" },
            "template": "blue"
        },
        "body": { "elements": [
            { "tag": "markdown", "content": "下拉选择后点「提交」，立即生效（重启回 config.toml 值；也可继续用 `/config <key> <value>` 文本命令）。" },
            { "tag": "hr" },
            { "tag": "form", "name": "imagent_config", "elements": form_elements }
        ] }
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
            queued_hint: None,
            terminal: CardTerminal::Running,
            usage_display: None,
            run_secs: 0,
        };
        let json = render_card(&card, "feishu:ou_t");
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
                queued_hint: None,
                terminal: CardTerminal::Running,
                usage_display: None,
                run_secs: 0,
            };
            assert!(
                render_card(&card, "feishu:ou_t").contains(mark),
                "{phase:?} → {mark}"
            );
        }
    }

    #[test]
    fn render_done_with_tools() {
        let card = OutboundCard {
            text: "done".into(),
            tool_calls: vec![tool("Read", "src/main.rs", true)],
            phase: CardPhase::Outputting,
            queued_hint: None,
            terminal: CardTerminal::Done,
            usage_display: None,
            run_secs: 0,
        };
        let json = render_card(&card, "feishu:ou_t");
        assert!(json.contains("done"));
        assert!(json.contains("Read"));
        assert!(json.contains("✅ 已完成"));
        // 工具面板：lcab 风格折叠面板（边框/内边距/小字号/状态图标）。
        assert!(json.contains("collapsible_panel"), "折叠面板: {json}");
        assert!(json.contains("corner_radius"), "面板边框: {json}");
        assert!(json.contains("notation"), "小字号: {json}");
        assert!(json.contains("✅ **Read**"), "工具状态行: {json}");
    }

    /// 终态卡折叠面板全量罗列：不丢最早工具（终态后可回看完整轨迹）；
    /// 流式期 stream_body_md 仍只显最近 5 条（防刷屏）。
    #[test]
    fn render_card_tool_panel_full_list_on_terminal() {
        let tools: Vec<ToolCall> = (0..10)
            .map(|i| tool("Bash", &format!("cmd-{i}"), true))
            .collect();
        let card = OutboundCard {
            text: "out".into(),
            tool_calls: tools,
            phase: CardPhase::ToolRunning,
            queued_hint: None,
            terminal: CardTerminal::Done,
            usage_display: None,
            run_secs: 0,
        };
        let json = render_card(&card, "feishu:ou_t");
        assert!(json.contains("cmd-0"), "终态面板含最早工具: {json}");
        assert!(json.contains("cmd-9"), "终态面板含最新工具: {json}");
        assert!(!json.contains("前面还有"), "面板不截断: {json}");
        // 流式 md 仍折叠（最近 5 条）。
        let running = OutboundCard {
            text: "out".into(),
            tool_calls: (0..10)
                .map(|i| tool("Bash", &format!("cmd-{i}"), true))
                .collect(),
            phase: CardPhase::ToolRunning,
            queued_hint: None,
            terminal: CardTerminal::Running,
            usage_display: None,
            run_secs: 0,
        };
        let md = stream_body_md(&running.text, &running.tool_calls);
        assert!(md.contains("前面还有 5 个工具"), "流式折叠计数: {md}");
        assert!(!md.contains("cmd-0"), "流式不显最早: {md}");
    }

    #[test]
    fn render_error() {
        let card = OutboundCard {
            text: "".into(),
            tool_calls: vec![],
            phase: CardPhase::Thinking,
            queued_hint: None,
            terminal: CardTerminal::Error("boom".into()),
            usage_display: None,
            run_secs: 0,
        };
        let json = render_card(&card, "feishu:ou_t");
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
        // P9-1：flow 自适应——所有按钮进单个 column_set（自动换行），并有 hr 分隔。
        assert_eq!(
            json.matches("\"tag\":\"column_set\"").count(),
            1,
            "flow 布局单 column_set: {json}"
        );
        assert!(json.contains("\"flex_mode\":\"flow\""), "flow 模式: {json}");
        assert!(json.contains("\"tag\":\"hr\""), "hr 分隔线: {json}");
        assert_eq!(
            json.matches("\"tag\":\"button\"").count(),
            4,
            "按钮数: {json}"
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
        // 两个按钮 + callback value 编码 conv 与动作。允许按钮不带 ✅（primary
        // 蓝底已高亮，绿色系 emoji 冲突）；⛔ 拒绝（danger）保留。
        assert!(json.contains("\"content\":\"允许\""), "允许按钮: {json}");
        assert!(json.contains("⛔ 拒绝"), "拒绝按钮: {json}");
        assert!(
            json.contains("\"imagent_perm\":\"allow\"")
                && json.contains("\"imagent_perm\":\"deny\"")
                && json.contains("\"imagent_perm\":\"always\""),
            "三个动作都应编码: {json}"
        );
        assert!(json.contains("♾️ 本次会话始终允许"), "始终允许按钮: {json}");
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

    /// P9-1：邮箱掩码——本地部分保留、@ 改 [at]；npm scope / 版本号 / 裸句柄不误伤。
    #[test]
    fn mask_emails_rewrites_only_real_addresses() {
        assert_eq!(
            mask_emails("联系 someone@example.com 谢谢"),
            "联系 someone[at]example.com 谢谢"
        );
        assert_eq!(
            mask_emails("Co-Authored-By: Uzziah <u@foo.dev>"),
            "Co-Authored-By: Uzziah <u[at]foo.dev>"
        );
        // 非邮箱形态不动。
        for keep in ["@larksuite/x", "pkg@1.2.3", "user@localhost", "@所有人"] {
            assert_eq!(mask_emails(keep), keep, "不应误伤: {keep}");
        }
    }

    /// P10：Running footer 组合——阶段 + 运行时长 + 排队提示；无附加纯阶段文案。
    #[test]
    fn running_footer_composes_queued_hint() {
        assert_eq!(
            running_footer(CardPhase::ToolRunning, None, 0),
            "🧰 正在调用工具…"
        );
        assert_eq!(
            running_footer(
                CardPhase::ToolRunning,
                Some("📥 排队 2 条，最新：「快一点」"),
                30
            ),
            "🧰 正在调用工具… · 30s · 📥 排队 2 条，最新：「快一点」"
        );
        // 时长 0（刚起步）不带秒数，防「0s」噪音。
        assert_eq!(running_footer(CardPhase::Thinking, None, 0), "🧠 思考中…");
        // 降级卡 footer 同样组合。
        let card = OutboundCard {
            text: "x".into(),
            tool_calls: vec![],
            phase: CardPhase::Outputting,
            queued_hint: Some("📥 排队 1 条".into()),
            terminal: CardTerminal::Running,
            usage_display: None,
            run_secs: 0,
        };
        let json = render_card(&card, "feishu:ou_t");
        assert!(
            json.contains("✍️ 输出中… · 📥 排队 1 条"),
            "降级卡组合: {json}"
        );
    }

    /// P10-③：审批卡 note 行可替换（排队联动重渲染），按钮 value 编码不变。
    #[test]
    fn permission_card_note_override() {
        let json = render_permission_card_note(
            "Bash",
            r#"{"command":"ls"}"#,
            "feishu:ou_t",
            "req9",
            "⏳ 等待你审批 · 后面还排着 3 条消息",
        );
        assert!(
            json.contains("⏳ 等待你审批 · 后面还排着 3 条消息"),
            "note 替换: {json}"
        );
        assert!(
            !json.contains("分钟后自动拒绝"),
            "默认 note 不再出现: {json}"
        );
        assert!(
            json.contains("\"imagent_perm\":\"allow\"") && json.contains("\"req\":\"req9\""),
            "按钮 value 编码不变: {json}"
        );
        // 缺省包装函数仍用默认 note（含具体分钟数值）。
        let plain = render_permission_card("Bash", r#"{"command":"ls"}"#, "c", "r");
        assert!(
            plain.contains("将在 5 分钟后自动拒绝"),
            "默认倒计时: {plain}"
        );
    }

    /// 命令按钮 value 带 ts（过期拒绝）与终止按钮的 sender（发起者校验）。
    #[test]
    fn cmd_button_value_carries_ts_and_sender() {
        let init = render_stream_init_card("feishu:oc_g", Some("ou_owner"));
        assert!(
            init.contains("\"sender\":\"ou_owner\""),
            "终止按钮带发起者: {init}"
        );

        assert!(
            init.contains("\"imagent_cmd\":\"/stop\""),
            "命令编码: {init}"
        );
        // 无发起者（私聊/未知）不编码 sender。
        let init2 = render_stream_init_card("feishu:ou_t", None);
        assert!(!init2.contains("\"sender\":"), "无 sender 不编码: {init2}");
        // 命令卡按钮带 ts。
        let json = render_command_card(
            "t",
            "b",
            &[CardButton {
                label: "x".into(),
                command: "/stop".into(),
                style: CardButtonStyle::Default,
            }],
            "feishu:oc_g",
        );
        assert!(json.contains("\"ts\":"), "命令卡 ts: {json}");
    }

    /// P9-1：流式卡终止按钮——init 卡与降级 Running 卡都带 ⏹ 终止（danger，
    /// 回调注入 /stop + conv 编码）；终态不带。
    #[test]
    fn stop_button_on_running_cards_only() {
        let init = render_stream_init_card("feishu:ou_t", None);
        assert!(init.contains("⏹ 终止"), "init 卡终止按钮: {init}");
        assert!(
            init.contains("\"imagent_cmd\":\"/stop\""),
            "命令编码: {init}"
        );
        assert!(
            init.contains("\"conv\":\"feishu:ou_t\""),
            "conv 编码: {init}"
        );

        let running = OutboundCard {
            text: "x".into(),
            tool_calls: vec![],
            phase: CardPhase::Outputting,
            queued_hint: None,
            terminal: CardTerminal::Running,
            usage_display: None,
            run_secs: 0,
        };
        let json = render_card(&running, "feishu:ou_t");
        assert!(json.contains("⏹ 终止"), "Running 降级卡带终止按钮: {json}");
        let done = OutboundCard {
            text: "ok".into(),
            tool_calls: vec![],
            phase: CardPhase::Outputting,
            queued_hint: None,
            terminal: CardTerminal::Done,
            usage_display: None,
            run_secs: 0,
        };
        let json2 = render_card(&done, "feishu:ou_t");
        assert!(!json2.contains("⏹ 终止"), "终态不带终止按钮: {json2}");
    }

    /// P9-1：空产出占位（空串 patch 可能被拒/显示空白）。
    #[test]
    fn stream_body_final_empty_placeholder() {
        assert_eq!(stream_body_final("", &[], None), "（未返回内容）");
    }

    /// P9-2：/config 表单卡——form + select_static 下拉 + 提交按钮（form_action_type）。
    #[test]
    fn config_form_card_shape() {
        let entries = vec![ConfigFormField {
            key: "reply_mode".into(),
            label: "回复形态".into(),
            current: "card".into(),
            options: vec![
                ("card".into(), "卡片（流式，默认）".into()),
                ("text".into(), "纯文本".into()),
            ],
        }];
        let json = render_config_form_card(&entries, "feishu:ou_t");
        assert!(json.contains("\"tag\":\"form\""), "form 元素: {json}");
        assert!(json.contains("select_static"), "下拉: {json}");
        assert!(json.contains("\"name\":\"reply_mode\""), "字段名: {json}");
        assert!(
            json.contains("\"form_action_type\":\"submit\""),
            "提交按钮: {json}"
        );
        assert!(
            json.contains("\"imagent_form\":\"config\""),
            "回调标记: {json}"
        );
        assert!(
            json.contains("\"conv\":\"feishu:ou_t\""),
            "conv 编码: {json}"
        );
    }

    #[test]
    fn stream_init_card_has_element_id_and_streaming() {
        let json = render_stream_init_card("feishu:ou_t", None);
        assert!(json.contains("element_id"), "初始卡应含 element_id: {json}");
        assert!(json.contains("md_body"), "正文组件锚点: {json}");
        assert!(json.contains("\"streaming_mode\":true"), "应开流式: {json}");
        assert!(json.contains("正在执行任务"), "应含自定义 summary: {json}");
        assert!(json.contains("🧠 思考中…"), "初始 footer: {json}");
    }

    #[test]
    fn stream_body_md_text_tools_and_empty() {
        // 空入参给明确状态语（首 chunk 前的静默期）。
        assert_eq!(stream_body_md("", &[]), "🧠 已接收任务，正在处理…");
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

    /// P8-2：结果下沉 stub——正文自带终态状态词（回滚一眼辨成败）+ 指针。
    #[test]
    fn stub_body_and_card() {
        assert_eq!(
            stub_body(3, None),
            "✅ 任务完成\n\n🔧 工具 3 次\n\n⬇️ 完整结果见下方消息"
        );
        assert_eq!(stub_body(0, None), "✅ 任务完成\n\n⬇️ 完整结果见下方消息");
        assert_eq!(
            stub_body(0, Some("boom")),
            "❌ 执行出错\n\n⬇️ 详情见下方消息"
        );
        assert_eq!(
            stub_body(0, Some("已中断")),
            "⏹ 已中断\n\n⬇️ 详情见下方消息"
        );
        let card = OutboundCard {
            text: "结论".into(),
            tool_calls: vec![tool("Bash", "ls", true)],
            phase: CardPhase::Outputting,
            queued_hint: None,
            terminal: CardTerminal::Done,
            usage_display: None,
            run_secs: 0,
        };
        let json = render_stub_card(&card);
        assert!(json.contains("⬇️ 完整结果见下方消息"), "指针: {json}");
        assert!(
            !json.contains("结论"),
            "stub 不含正文（正文在重发的新卡）: {json}"
        );
        assert!(json.contains("工具 1 次"), "统计行: {json}");
        assert!(json.contains("✅ 任务完成"), "正文状态词: {json}");
        // footer 的「已完成」与正文的「任务完成」措辞互异——footer 状态词仍只一次。
        assert_eq!(
            json.matches("已完成").count(),
            1,
            "footer 状态词只应出现一次: {json}"
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
        // 终态附全量工具明细（引用行）——managed 路径终态后可回看轨迹。
        assert!(out.contains("> ✅ **Bash** — a"), "全量明细: {out}");
        assert!(out.contains("> ✅ **Read** — c"), "全量明细: {out}");
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

    /// 审批详情超长截断提示：末尾明示「已截断，仅显示前 1000 字符」。
    #[test]
    fn perm_detail_truncation_notice() {
        let long = "x".repeat(1500);
        let json = render_permission_card(
            "Bash",
            &format!(r#"{{"command":"echo {long}"}}"#),
            "feishu:ou_t",
            "req1",
        );
        assert!(
            json.contains("已截断，仅显示前 1000 字符"),
            "截断提示: {json}"
        );
        // 短输入无提示。
        let short = render_permission_card("Bash", r#"{"command":"ls"}"#, "c", "r");
        assert!(!short.contains("已截断"), "短输入不提示: {short}");
    }

    /// 邮箱掩码破坏复制的缓解：掩码仍强制（审计合规），但加提示文案。
    #[test]
    fn perm_detail_email_mask_notice() {
        let json = render_permission_card(
            "Bash",
            r#"{"command":"git clone git@github.com:org/repo.git"}"#,
            "feishu:ou_t",
            "req1",
        );
        assert!(json.contains("[at]"), "掩码仍生效（审计强制）: {json}");
        assert!(json.contains("邮箱已掩码显示"), "掩码提示: {json}");
        assert!(
            json.contains("原命令可直接执行"),
            "告知原命令语义不变: {json}"
        );
        // 无邮箱的命令不出现提示。
        let plain = render_permission_card("Bash", r#"{"command":"ls -la"}"#, "c", "r");
        assert!(!plain.contains("邮箱已掩码"), "无掩码不提示: {plain}");
    }

    /// 问题卡表单化：>4 选项单选 → select_static 下拉；多选 → checkbox。
    /// 提交按钮一次回传（imagent_form=ask + req 精确路由）。
    #[test]
    fn question_card_form_for_many_options_and_multi() {
        let labels: Vec<String> = (1..=6).map(|i| format!("方案{i}")).collect();
        let mk_input = |multi: bool| {
            serde_json::json!({
                "questions": [{
                    "question": "选哪个方案？",
                    "multiSelect": multi,
                    "options": labels.iter().map(|l| serde_json::json!({"label": l})).collect::<Vec<_>>()
                }]
            })
            .to_string()
        };
        // >4 选项单选：select_static 表单，不再要求手打 ask:选项。
        let json = render_question_card(&mk_input(false), "feishu:ou_q", "reqF")
            .expect("单选多选项应渲染");
        assert!(json.contains("\"tag\":\"form\""), "form 元素: {json}");
        assert!(json.contains("select_static"), "下拉: {json}");
        assert!(json.contains("\"name\":\"ask_opt\""), "字段名: {json}");
        assert!(
            json.contains("\"imagent_form\":\"ask\""),
            "ask 表单标记: {json}"
        );
        assert!(json.contains("\"req\":\"reqF\""), "req 精确路由: {json}");
        assert!(
            json.contains("\"form_action_type\":\"submit\""),
            "提交按钮: {json}"
        );
        assert!(!json.contains("回复 `ask:选项`"), "不再要求手打: {json}");
        // 全部选项都在下拉里（无「其余选项」截断）。
        assert!(
            json.contains("方案1") && json.contains("方案6"),
            "全选项: {json}"
        );
        // 多选：checkbox 表单。
        let multi =
            render_question_card(&mk_input(true), "feishu:ou_q", "reqM").expect("多选应渲染");
        assert!(multi.contains("\"tag\":\"checkbox\""), "checkbox: {multi}");
        assert!(!multi.contains("select_static"), "多选不用下拉: {multi}");
        assert!(multi.contains("一次回传全部选择"), "多选提交提示: {multi}");
        // ≤4 选项单选仍是按钮形态（最快路径）。
        let few = serde_json::json!({
            "questions": [{
                "question": "选哪个？",
                "options": [{"label":"A"},{"label":"B"}]
            }]
        })
        .to_string();
        let btn = render_question_card(&few, "c", "r").expect("少选项应渲染");
        assert!(btn.contains("\"imagent_ask\":\"A\""), "按钮形态保留: {btn}");
        assert!(!btn.contains("\"tag\":\"form\""), "少选项不用表单: {btn}");
        // 多问题标注：只答第一问。
        let multi_q = serde_json::json!({
            "questions": [
                {"question": "第一问？", "options": [{"label":"A"}]},
                {"question": "第二问？", "options": [{"label":"B"}]}
            ]
        })
        .to_string();
        let mq = render_question_card(&multi_q, "c", "r").expect("应渲染");
        assert!(
            mq.contains("将依次询问") && mq.contains("只答第一问"),
            "多问题标注: {mq}"
        );
    }

    /// danger 按钮二次确认弹窗（confirm 字段）；非 danger 按钮不带。
    #[test]
    fn command_card_danger_button_has_confirm() {
        let buttons = vec![
            CardButton {
                label: "使用 main".into(),
                command: "/ws use main".into(),
                style: CardButtonStyle::Primary,
            },
            CardButton {
                label: "删除".into(),
                command: "/ws remove tmp".into(),
                style: CardButtonStyle::Danger,
            },
        ];
        let json = render_command_card("📁 工作空间", "- main", &buttons, "feishu:oc_g");
        assert!(json.contains("\"confirm\""), "danger 按钮带确认: {json}");
        assert!(json.contains("确认执行"), "确认弹窗标题: {json}");
        assert!(
            json.contains("/ws remove tmp"),
            "确认文案含具体命令: {json}"
        );
        // confirm 只挂在 danger 按钮上（出现一次）。
        assert_eq!(
            json.matches("\"confirm\"").count(),
            1,
            "仅 danger 按钮确认: {json}"
        );
    }

    /// 空正文占位：init 卡与流式 md 均为明确状态语（非「…」）。
    #[test]
    fn empty_body_placeholder_is_explicit() {
        let init = render_stream_init_card("feishu:ou_t", None);
        assert!(
            init.contains("🧠 已接收任务，正在处理"),
            "init 卡状态语: {init}"
        );
        let card = OutboundCard {
            text: "".into(),
            tool_calls: vec![],
            phase: CardPhase::Thinking,
            queued_hint: None,
            terminal: CardTerminal::Running,
            usage_display: None,
            run_secs: 0,
        };
        let json = render_card(&card, "feishu:ou_t");
        assert!(
            json.contains("🧠 已接收任务，正在处理"),
            "降级卡空正文状态语: {json}"
        );
    }

    // ------------------------------------------------------------------
    // CardKit 视觉改版（终态 header / note / tag / 表格双列 / 分段 …）
    // ------------------------------------------------------------------

    fn card_of(terminal: CardTerminal, tools: Vec<ToolCall>) -> OutboundCard {
        OutboundCard {
            text: "结论".into(),
            tool_calls: tools,
            phase: CardPhase::Outputting,
            queued_hint: None,
            terminal,
            usage_display: None,
            run_secs: 0,
        }
    }

    /// ① 终态 header 主题色：Done=green / Error=red / 已中断=grey；Running 与
    /// 流式初始卡不加 header（首帧后无法改）。
    #[test]
    fn terminal_header_template_colors() {
        let done = render_card(&card_of(CardTerminal::Done, vec![]), "feishu:ou_t");
        assert!(
            done.contains("\"template\":\"green\"") && done.contains("✅ 已完成"),
            "Done header: {done}"
        );
        let err = render_card(
            &card_of(CardTerminal::Error("boom".into()), vec![]),
            "feishu:ou_t",
        );
        assert!(
            err.contains("\"template\":\"red\"") && err.contains("❌ 出错"),
            "Error header: {err}"
        );
        let stop = render_card(
            &card_of(CardTerminal::Error("已中断".into()), vec![]),
            "feishu:ou_t",
        );
        assert!(
            stop.contains("\"template\":\"grey\"") && stop.contains("⏹ 已中断"),
            "中断 header: {stop}"
        );
        let running = render_card(&card_of(CardTerminal::Running, vec![]), "feishu:ou_t");
        assert!(
            !running.contains("\"header\""),
            "Running 不加 header: {running}"
        );
        let init = render_stream_init_card("feishu:ou_t", None);
        assert!(
            !init.contains("\"header\""),
            "流式初始卡不加 header: {init}"
        );
        // stub 卡（结果下沉指针）同样带终态 header；状态词只出现一次（header 承载，
        // 不再有 footer 小字状态行）。
        let stub = render_stub_card(&card_of(CardTerminal::Done, vec![tool("Bash", "ls", true)]));
        assert!(
            stub.contains("\"template\":\"green\""),
            "stub header: {stub}"
        );
        assert_eq!(stub.matches("已完成").count(), 1, "状态词单次: {stub}");
    }

    /// ② note 提示条：审批倒计时 / 排队 note / 掩码警告 / 超时原因均为 note 元素
    /// （形态待真机校准；降级＝markdown+notation）。
    #[test]
    fn note_elements_for_meta_lines() {
        let json = render_permission_card("Bash", r#"{"command":"ls"}"#, "feishu:ou_t", "r");
        assert!(
            json.contains("\"tag\":\"note\"") && json.contains("分钟后自动拒绝"),
            "倒计时 note: {json}"
        );
        let queued = render_permission_card_note(
            "Bash",
            r#"{"command":"ls"}"#,
            "feishu:ou_t",
            "r",
            "⏳ 等待你审批 · 后面还排着 3 条消息",
        );
        assert!(
            queued.contains("\"tag\":\"note\"") && queued.contains("等待你审批"),
            "排队 note: {queued}"
        );
        let masked = render_permission_card(
            "Bash",
            r#"{"command":"git clone git@github.com:org/repo.git"}"#,
            "feishu:ou_t",
            "r",
        );
        assert!(
            masked.matches("\"tag\":\"note\"").count() >= 2,
            "倒计时 + 掩码警告两条 note: {masked}"
        );
        assert!(
            masked.contains("邮箱已掩码显示") && masked.contains("\"tag\":\"note\""),
            "掩码警告 note 化: {masked}"
        );
        let cancelled = render_permission_card_cancelled("Bash");
        assert!(
            cancelled.contains("\"tag\":\"note\"") && cancelled.contains("审批超时"),
            "超时原因 note: {cancelled}"
        );
        // 问题卡 note 同步。
        let input = serde_json::json!({
            "questions": [{"question": "选？", "options": [{"label":"A"},{"label":"B"}]}]
        })
        .to_string();
        let q = render_question_card(&input, "feishu:ou_t", "r").unwrap();
        assert!(q.contains("\"tag\":\"note\""), "问题卡 note: {q}");
    }

    /// ③ tag 胶囊墙：终态整卡 elements 带 tag 组件（`Bash×2` 按名计数）；
    /// markdown 统计行（stream_body_final）保留作兜底。
    #[test]
    fn tool_tag_wall_on_terminal_card() {
        let tools = vec![
            tool("Bash", "a", true),
            tool("Bash", "b", true),
            tool("Read", "c", true),
        ];
        let json = render_card(&card_of(CardTerminal::Done, tools), "feishu:ou_t");
        assert!(json.contains("\"tag\":\"tag\""), "tag 组件: {json}");
        assert!(
            json.contains("Bash×2") && json.contains("Read×1"),
            "计数胶囊: {json}"
        );
        // Running 不加胶囊墙（统计未收敛）。
        let running = render_card(
            &card_of(CardTerminal::Running, vec![tool("Bash", "a", false)]),
            "feishu:ou_t",
        );
        assert!(
            !running.contains("\"tag\":\"tag\""),
            "Running 无胶囊: {running}"
        );
        // markdown 统计行兜底仍在（managed 终态正文）。
        let md = stream_body_final("结论", &[tool("Bash", "a", true)], None);
        assert!(
            md.contains("工具 1 次") && md.contains("Bash×1"),
            "统计行兜底: {md}"
        );
    }

    /// ④⑤ /resume 表格 + 双列配对：行「会话信息 | 接管按钮」weighted 4:1，
    /// 来源徽章 tag 化（💻 本机 / 📱 IM）；第 10+ 行（无按钮）保留表格兜底。
    #[test]
    fn resume_table_paired_two_column_rows() {
        let mut body = String::from("| # | 来源 | 时间 | 内容 |\n|---|---|---|---|\n");
        for (i, src) in ["📱", "💻", "📱"].iter().enumerate() {
            body.push_str(&format!("| {} | {src} | 3 分钟前 | 会话{i} |\n", i + 1));
        }
        body.push_str("| 10 | 💻 | 1 小时前 | 长列表第 10 条 |\n");
        let buttons: Vec<CardButton> = (1..=3)
            .map(|i| CardButton {
                label: format!("接管 {i}"),
                command: format!("/resume {i}"),
                style: if i == 1 {
                    CardButtonStyle::Primary
                } else {
                    CardButtonStyle::Default
                },
            })
            .collect();
        let json = render_command_card("⏪ 可恢复会话", &body, &buttons, "feishu:oc_g");
        // 双列配对（weighted 4:1，形态待真机校准）。
        assert!(
            json.contains("\"width\":\"weighted\"") && json.contains("\"weight\":4"),
            "左列 weighted 4: {json}"
        );
        assert!(json.contains("\"weight\":1"), "右列 weighted 1: {json}");
        // 来源徽章 tag 化（行文本兜底保留在降级表格）。
        assert!(
            json.contains("\"tag\":\"tag\"") && json.contains("💻 本机") && json.contains("📱 IM"),
            "来源 tag 胶囊: {json}"
        );
        // 按钮配对进各行（value 编码不变）。
        assert!(
            json.contains("\"imagent_cmd\":\"/resume 2\""),
            "接管命令编码: {json}"
        );
        assert!(
            json.contains("会话1") && json.contains("会话2"),
            "行内容: {json}"
        );
        // 未配对行（10 号）保留 markdown 表格兜底 + 提示。
        assert!(json.contains("长列表第 10 条"), "未配对行兜底: {json}");
        assert!(json.contains("其余会话发送"), "兜底提示: {json}");
    }

    /// ④⑤ /ws 表格 + 双列配对：「名称+路径 | 使用/删除按钮」。
    #[test]
    fn ws_table_paired_rows() {
        let body = "| 名称 | 路径 |\n|---|---|\n| main | /a/b |\n| web | /c/d |\n";
        let buttons = vec![
            CardButton {
                label: "使用 main".into(),
                command: "/ws use main".into(),
                style: CardButtonStyle::Primary,
            },
            CardButton {
                label: "删除 main".into(),
                command: "/ws remove main".into(),
                style: CardButtonStyle::Danger,
            },
            CardButton {
                label: "使用 web".into(),
                command: "/ws use web".into(),
                style: CardButtonStyle::Primary,
            },
            CardButton {
                label: "删除 web".into(),
                command: "/ws remove web".into(),
                style: CardButtonStyle::Danger,
            },
        ];
        let json = render_command_card("📁 命名工作空间", body, &buttons, "feishu:oc_g");
        assert!(json.contains("\"weight\":4"), "双列 weighted: {json}");
        assert!(
            json.contains("**main**") && json.contains("/a/b"),
            "名称+路径: {json}"
        );
        assert!(
            json.contains("\"imagent_cmd\":\"/ws use main\"")
                && json.contains("\"imagent_cmd\":\"/ws remove web\""),
            "使用/删除按钮编码: {json}"
        );
        // danger 仍带二次确认。
        assert!(json.contains("\"confirm\""), "danger 确认弹窗: {json}");
    }

    /// 配对不成立（非表格正文 / 约定外的按钮 label）回退平铺布局（markdown
    /// 降级思路）：单 column_set flow 按钮行。
    #[test]
    fn command_card_fallback_when_no_table() {
        let buttons = vec![CardButton {
            label: "📊 状态".into(),
            command: "/status".into(),
            style: CardButtonStyle::Primary,
        }];
        let json = render_command_card("t", "- main：/a/b", &buttons, "feishu:oc_g");
        assert!(json.contains("\"flex_mode\":\"flow\""), "平铺 flow: {json}");
        assert!(json.contains("- main：/a/b"), "正文原样: {json}");
        assert!(!json.contains("\"width\":\"weighted\""), "无双列: {json}");
    }

    /// ⑥ 图标统一：☕ 省略行改 ⋯；superseded ⏭️ 改 🔁；始终允许 🔓 → ♾️。
    #[test]
    fn icon_unification() {
        let many: Vec<ToolCall> = (0..8)
            .map(|i| tool("Read", &format!("f{i}"), true))
            .collect();
        let md = stream_body_md("", &many);
        assert!(md.contains("⋯ 前面还有 3 个工具"), "省略号图标: {md}");
        assert!(!md.contains("☕"), "不再用咖啡杯: {md}");
        let sup = render_permission_card_superseded("Bash");
        assert!(sup.contains("🔁 已被新询问取代"), "superseded 图标: {sup}");
        assert!(!sup.contains("⏭️"), "不再用跳过图标: {sup}");
        let perm = render_permission_card("Bash", r#"{"command":"ls"}"#, "c", "r");
        assert!(perm.contains("♾️ 本次会话始终允许"), "♾️ 徽章: {perm}");
        assert!(!perm.contains("🔓"), "不再用开锁: {perm}");
    }

    /// ⑦ primary 按钮 emoji 精简：允许（primary）无 ✅（蓝底已高亮）；
    /// ⛔ 拒绝（danger）保留。
    #[test]
    fn primary_buttons_no_green_emoji() {
        let json = render_permission_card("Bash", r#"{"command":"ls"}"#, "c", "r");
        assert!(
            json.contains("\"content\":\"允许\"") && !json.contains("✅ 允许"),
            "primary 允许无 ✅: {json}"
        );
        assert!(json.contains("⛔ 拒绝"), "danger ⛔ 保留: {json}");
    }

    /// ⑧ 长正文分段：managed 终态正文 `---` + 「**工具轨迹**」小标题；
    /// 降级整卡路径用真 hr 组件 + 面板标题（🔧 工具轨迹）。
    #[test]
    fn long_body_sectioning() {
        let out = stream_body_final("结论", &[tool("Bash", "a", true)], None);
        assert!(out.contains("\n---\n"), "正文与统计间分割线: {out}");
        assert!(out.contains("**工具轨迹**"), "明细块小标题: {out}");
        let json = render_card(
            &card_of(CardTerminal::Done, vec![tool("Bash", "a", true)]),
            "feishu:ou_t",
        );
        assert!(json.contains("\"tag\":\"hr\""), "真 hr 组件: {json}");
        assert!(json.contains("🔧 工具轨迹（1）"), "面板标题: {json}");
    }

    /// ⑨ 面板边框随终态：Done=grey / Error=red / Running=blue。
    #[test]
    fn tool_panel_border_by_terminal() {
        let done = render_card(
            &card_of(CardTerminal::Done, vec![tool("B", "a", true)]),
            "c",
        );
        assert!(done.contains("\"color\":\"grey\""), "Done grey: {done}");
        let err = render_card(
            &card_of(
                CardTerminal::Error("boom".into()),
                vec![tool("B", "a", true)],
            ),
            "c",
        );
        assert!(err.contains("\"color\":\"red\""), "Error red: {err}");
        let running = render_card(
            &card_of(CardTerminal::Running, vec![tool("B", "a", false)]),
            "c",
        );
        assert!(
            running.contains("\"color\":\"blue\""),
            "Running blue: {running}"
        );
    }

    /// ⑩ /help 分组元素化：组标题独立成 markdown 元素（heading-large，支持性
    /// 待真机校准），不再是一整块 markdown。
    #[test]
    fn help_body_split_into_group_elements() {
        let body = "🗂 会话\n- /new 重置会话\n- /resume 恢复\n\n📁 目录与文件\n- /cd 切目录";
        let buttons = vec![CardButton {
            label: "📊 状态".into(),
            command: "/status".into(),
            style: CardButtonStyle::Primary,
        }];
        let json = render_command_card("🤖 imagent 命令", body, &buttons, "feishu:ou_t");
        assert!(
            json.contains("\"text_size\":\"heading-large\""),
            "组标题 heading-large: {json}"
        );
        assert!(
            json.matches("\"tag\":\"markdown\"").count() >= 3,
            "分组独立元素（组标题×2 + 列表×2）: {json}"
        );
        assert!(
            json.contains("🗂 会话") && json.contains("/new 重置会话"),
            "内容不丢: {json}"
        );
    }
}
