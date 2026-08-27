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
        elements.push(render_tool_panel(&card.tool_calls));
    }
    // 状态 footer：note 行（notation 小字号）体现终态 / 流式阶段。
    elements.push(serde_json::json!({
        "tag": "markdown", "content": footer, "text_size": "notation"
    }));
    // Running 态带终止按钮（终态移除——整卡 patch 每次重渲染，自然消失）。
    if streaming {
        elements.push(stop_button(conv_id));
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
    serde_json::json!({
        "schema": "2.0",
        "config": config,
        "body": { "elements": elements }
    })
    .to_string()
}

/// 工具调用折叠面板（lcab collapsedToolSummary 同款）：蓝边框 + 圆角 + 内边距，
/// 收起态；正文为小字号（notation）的工具行列表，行首状态图标。
///
/// 终态卡**全量罗列**（不截最近 5 条）——面板默认收起不占版面，展开即完整
/// 工具轨迹，终态后可回看明细（流式期只显最近 5 条，见 [`stream_body_md`]）。
fn render_tool_panel(tools: &[ToolCall]) -> serde_json::Value {
    let n = tools.len();
    let mut lines = String::new();
    for t in tools {
        lines.push_str(&format!("- {}\n", mask_emails(&tool_card_line(t))));
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

/// ⏹ 终止按钮（lcab stopButton 同款）：Running 态挂在卡片底部，点击回调注入
/// `/stop`（imagent_cmd 机制，走与手打命令相同的鉴权/分派）。managed 卡终态后
/// 按钮无法移除（element PATCH 只能动 markdown）——点击回「当前没有运行中的
/// 任务」，无害。
fn stop_button(conv_id: &str) -> serde_json::Value {
    serde_json::json!({
        "tag": "column_set", "flex_mode": "flow", "horizontal_spacing": "default",
        "columns": [{
            "tag": "column", "width": "auto",
            "elements": [{
                "tag": "button",
                "text": { "tag": "plain_text", "content": "⏹ 终止" },
                "type": "danger",
                "behaviors": [{ "type": "callback", "value": {
                    "imagent_cmd": "/stop", "conv": conv_id
                } }]
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
pub fn render_stream_init_card(conv_id: &str) -> String {
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
            stop_button(conv_id)
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
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!(
            "🔧 工具 {} 次：{}\n",
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
    if truncated {
        md.push_str(&format!("\n\n…（已截断，仅显示前 {PERM_DETAIL_MAX} 字符）"));
    }
    if email_masked {
        md.push_str("\n\n⚠️ 邮箱已掩码显示（`[at]`），原命令可直接执行，请勿复制此代码块。");
    }
    md
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
    serde_json::json!({
        "schema": "2.0",
        "header": {
            "title": { "tag": "plain_text", "content": "🔐 权限审批" },
            "template": "orange"
        },
        "body": { "elements": [
            { "tag": "markdown", "content": perm_detail_md(tool_name, input_summary) },
            { "tag": "markdown", "content": note, "text_size": "notation" },
            // P9-1：hr 分割线（V2 支持，lcab 生产验证）+ flow 自适应按钮布局。
            { "tag": "hr" },
            flow_button_row(&[
                cb_button("✅ 允许", "primary", serde_json::json!({
                    "imagent_perm": "allow", "conv": conv_id, "req": request_id
                })),
                cb_button("🔓 本次会话始终允许", "default", serde_json::json!({
                    "imagent_perm": "always", "conv": conv_id, "req": request_id
                })),
                cb_button("⛔ 拒绝", "danger", serde_json::json!({
                    "imagent_perm": "deny", "conv": conv_id, "req": request_id
                })),
            ])
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
/// 多问题场景（questions.len() > 1）当前只答第一问：卡片上明确标注。
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
            serde_json::json!({ "tag": "markdown", "content": note, "text_size": "notation" }),
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
            serde_json::json!({ "tag": "markdown", "content": note, "text_size": "notation" }),
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
            { "tag": "markdown", "content": mask_emails(body_md) }
        ] }
    });
    if buttons.is_empty() {
        return card.to_string();
    }
    // P9-1：hr 分隔正文与按钮；flow 自适应单行布局（按内容宽度自动换行，
    // 替代此前每行 3 个等宽）。
    let btns: Vec<serde_json::Value> = buttons
        .iter()
        .map(|b| {
            let value = serde_json::json!({ "imagent_cmd": b.command, "conv": conv_id });
            // danger 按钮带二次确认弹窗（误触破坏性命令的最后一道闸）。
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
        })
        .collect();
    if let Some(elements) = card
        .pointer_mut("/body/elements")
        .and_then(|e| e.as_array_mut())
    {
        elements.push(serde_json::json!({ "tag": "hr" }));
        elements.push(flow_button_row(&btns));
    }
    card.to_string()
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
        // 两个按钮 + callback value 编码 conv 与动作。
        assert!(json.contains("✅ 允许"), "允许按钮: {json}");
        assert!(json.contains("⛔ 拒绝"), "拒绝按钮: {json}");
        assert!(
            json.contains("\"imagent_perm\":\"allow\"")
                && json.contains("\"imagent_perm\":\"deny\"")
                && json.contains("\"imagent_perm\":\"always\""),
            "三个动作都应编码: {json}"
        );
        assert!(json.contains("🔓 本次会话始终允许"), "始终允许按钮: {json}");
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

    /// P9-1：流式卡终止按钮——init 卡与降级 Running 卡都带 ⏹ 终止（danger，
    /// 回调注入 /stop + conv 编码）；终态不带。
    #[test]
    fn stop_button_on_running_cards_only() {
        let init = render_stream_init_card("feishu:ou_t");
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
        let json = render_stream_init_card("feishu:ou_t");
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
        let init = render_stream_init_card("feishu:ou_t");
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
}
