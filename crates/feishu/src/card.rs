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
/// L8（code-review v8）：markdown 语义层 `<` 转义——用户可控文本经 bot 卡片
/// 渲染时 `<at id=…></at>` 可以 bot 名义 @ 任意租户用户。只转 `<`（JSON 层
/// serde 已封死结构注入；`[` 链接伪装的格式损失大于收益，v4 评估结论维持）。
/// v8-L9 校准修正（2026-08-30 真机）：卡片大小限制按**字节**计（200860
/// card over max size 实测 24K 字符 ≈ 30KB 被拒）——此前按字符截断形同虚设。
/// 字节安全地在 char 边界截取头/尾窗口，中间以省略标注连接。
fn cap_md_bytes(md: &str, head_b: usize, tail_b: usize) -> String {
    if md.len() <= head_b + tail_b {
        return md.to_string();
    }
    let head_end = {
        let mut i = head_b.min(md.len());
        while i > 0 && !md.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let tail_start = {
        let mut j = md.len().saturating_sub(tail_b);
        while j < md.len() && !md.is_char_boundary(j) {
            j += 1;
        }
        j
    };
    let omitted = md.len() - head_end - (md.len() - tail_start);
    // R2（code-review v9）：不再承诺「完整内容见文本消息」——补发与否在 core
    // 侧按正文长度决定（CARD_TEXT_FULL_THRESHOLD），此处无从得知；8KB-30KB
    // 区间曾出现「卡上承诺、文本没来」的虚假契约。标注只陈述截断事实，
    // 超阈正文由 core 主动补发全文文本（阈值已对齐卡上限之下）。
    format!(
        "{}\n\n…（已截断中段 {omitted} 字节）…\n\n{}",
        &md[..head_end],
        &md[tail_start..]
    )
}

fn escape_lt(text: &str) -> String {
    // R13（code-review v9）：围栏代码块内的 `<` 不转义——CommonMark 代码块
    // 不处理反斜杠转义，此前全串替换把 `a < b` 显示成 `a \< b`。逐行跟踪
    // ``` / ~~~ 围栏开闭，仅块外转义；行内 code span 同理不转义（见
    // escape_lt_inline）。
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
        } else if in_fence {
            out.push_str(line);
        } else {
            out.push_str(&escape_lt_inline(line));
        }
    }
    out
}

/// 行内 code span（`…`）内的 `<` 不转义（同 R13）：按反引号切分，奇数索引
/// 段是 span 内容原样保留。未配对反引号/跨行 span 的边界误差可接受——
/// 展示层少转义优于显示破坏（JSON 结构注入已由 serde 层封死，此处只防
/// `<at>` 语义注入，代码片段里没有该形态）。
fn escape_lt_inline(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for (i, seg) in line.split('`').enumerate() {
        if i % 2 == 1 {
            out.push('`');
            out.push_str(seg);
            out.push('`');
        } else {
            out.push_str(&seg.replace('<', "\\<"));
        }
    }
    out
}

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

/// 审批/问题卡的自动拒绝倒计时文案（真实值透传）：`permission_ask_timeout_secs`
/// 由 `FeishuPlatform` 构造时注入（见 platform 的 `ask_timeout_secs` 字段），
/// 不再硬编码 5 分钟——自定义了超时的部署文案与实际行为一致。
/// 换算口径：≥90s 显示分钟（四舍五入），否则显示秒（避免「1 分钟」掩盖实际
/// 只有几十秒的紧迫感）。
pub(crate) fn humanize_ask_timeout(secs: u64) -> String {
    if secs >= 90 {
        format!("{} 分钟", (secs + 30) / 60)
    } else {
        format!("{secs} 秒")
    }
}

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

/// Wave B-3：运行时长的人读短格式：`42s` / `30m` / `2h05m` / `1d3h`
///（与 Running footer 的纯秒数区分——终态展示累计耗时）。
pub(crate) fn format_run_len(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}

/// Wave B-3：成功终态 footer：`✅ 已完成 · 30m · $0.012`（无 usage 省成本段）。
/// `run_secs` 为 CardSession 终态写入的**全量**秒数（非 Running 期的 10s 量化）。
pub(crate) fn terminal_done_footer(run_secs: u64, usage_display: Option<&str>) -> String {
    let mut out = format!("✅ 已完成 · {}", format_run_len(run_secs));
    if let Some(u) = usage_display {
        out.push_str(&format!(" · {u}"));
    }
    out
}

/// Wave B-5：群 conv 卡片的「发起者」标注行（markdown 元素形态）。
///
/// 形态取舍：CardKit markdown 组件支持 `<at id=…></at>` 标签（**待真机校准**：
/// 若该租户卡片 markdown 不支持 at，标签原文会显示为文本——届时退化为
/// 「→ 发起者 <open_id>」纯文本行，只改本函数一处）。私聊不加（单人无歧义）。
pub(crate) fn sender_anchor_line(
    sender: Option<&str>,
    is_group: bool,
) -> Option<serde_json::Value> {
    let s = sender.filter(|s| !s.is_empty())?;
    if !is_group {
        return None;
    }
    Some(serde_json::json!({
        "tag": "markdown",
        "content": format!("<at id=\"{s}\"></at> 发起的任务"),
        "text_size": "notation"
    }))
}

/// 渲染 [`OutboundCard`] 为飞书 interactive 卡片的 content JSON 字符串
/// （配合 `msg_type = "interactive"` 发送 / patch）。
///
/// markdown 文本块 + 工具调用折叠面板 + 状态 footer。
/// 这是**降级路径**的渲染（managed 真流式路径见 [`render_stream_init_card`]）。
///
/// Wave B-3：成功终态 footer 带总耗时（`✅ 已完成 · 30m · $0.012`，run_secs 为
/// CardSession 终态写入的全量秒数）。Wave B-5：群 conv（conv_id 非 `ou_` 私聊
/// 形态）顶部加「发起者」标注行（sender 为最近 sender 近似，见 platform 注释）。
/// Wave B-11：失败终态卡补「🩺 /doctor」按钮（点击注入 /doctor 命令，走与手打
/// 相同的鉴权/分派）。
pub fn render_card(card: &OutboundCard, conv_id: &str, sender: Option<&str>) -> String {
    let (footer, streaming, err) = match &card.terminal {
        CardTerminal::Running => (
            running_footer(card.phase, card.queued_hint.as_deref(), card.run_secs),
            true,
            None,
        ),
        CardTerminal::Done => (
            terminal_done_footer(card.run_secs, card.usage_display.as_deref()),
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
    let mut elements = Vec::new();
    // Wave B-5：群 conv 顶部「发起者」标注行（私聊/缺 sender 不加）。
    if let Some(line) = sender_anchor_line(sender, !crate::proto::is_private_conv(conv_id)) {
        elements.push(line);
    }
    // W2-2：任务清单（checklist）置正文上方——进度条语义（render_card 是整卡
    // 路径，清单为独立 markdown 组件；managed 单组件路径在 stream_body_md 内拼）。
    let body_md: String = match todo_list_md(&card.todos) {
        Some(t) => format!("{t}\n\n{text}"),
        None => text.into_owned(),
    };
    elements.push(
        serde_json::json!({ "tag": "markdown", "content": escape_lt(&cap_md_bytes(&mask_emails(&body_md), 4_096, 4_096)) }),
    );
    if !card.tool_calls.is_empty() {
        // 长正文分段：正文与工具面板间用真 hr 组件分隔（降级路径专属——
        // managed 路径的 md_body 是单 markdown 组件，用 `---` 文本分割线，
        // 见 [`stream_body_final`]）。
        elements.push(serde_json::json!({ "tag": "hr" }));
        // 工具统计行（markdown+notation 小字）：终态整卡与流式终态同一形态。
        // 真机校准（2026-08）：原 CardKit tag 胶囊墙（裸 "tag" 组件）被整卡
        // 拒收——200621 "not support tag: tag"，结果下沉降级纯文本；V2 无等价
        // 胶囊组件，统计信息以文本行承载。Running 态不加（统计未收敛）。
        if !streaming {
            elements.push(serde_json::json!({
                "tag": "markdown",
                "content": format!(
                    "🔧 工具 {} 次：{}",
                    card.tool_calls.len(),
                    tool_stats_summary(&card.tool_calls)
                ),
                "text_size": "notation"
            }));
        }
        // 面板边框随终态：Running=blue / Done=grey / Error=red。
        let border = if streaming {
            "blue"
        } else {
            border_color_of(err)
        };
        elements.push(render_tool_panel(&card.tool_calls, border));
    }
    // W2-1：思考过程折叠面板（最近 5 条，默认收起——不占正文版面，展开回看）。
    if !card.thoughts.is_empty() {
        let border = if streaming {
            "blue"
        } else {
            border_color_of(err)
        };
        elements.push(render_thought_panel(&card.thoughts, border));
    }
    // 状态 footer：note 行（notation 小字号）体现终态 / 流式阶段。
    elements.push(serde_json::json!({
        "tag": "markdown", "content": footer, "text_size": "notation"
    }));
    // Running 态带终止按钮（终态移除——整卡 patch 每次重渲染，自然消失）。
    if streaming {
        elements.push(stop_button(conv_id, None));
    } else if err.is_some() {
        // Wave B-11：失败终态卡补「🩺 自检」按钮——一键 /doctor 排障（失败后
        // 用户最需要的下一步动作）。managed（card: 句柄）路径 element PATCH 只能
        // 更新 markdown 组件、无法追加按钮，该路径以 footer 文案指引兜底（见
        // platform::patch_managed）。非卡片平台 send_command_card 默认降级纯文本。
        elements.push(flow_button_row(&[cb_button(
            "🩺 自检 /doctor",
            "default",
            cmd_value(conv_id, "/doctor", None),
        )]));
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
        // R7（code-review v9 残留）：工具摘要可携带文件内容片段，<at id=…>
        // 注入面与正文三路径同权——escape_lt 收口（mask_emails 只掩邮箱）。
        lines.push_str(&format!(
            "- {}\n",
            escape_lt(&mask_emails(&tool_card_line(t)))
        ));
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

/// W2-1：思考过程折叠面板（与 [`render_tool_panel`] 同款形态）：最近
/// THOUGHT_PANEL_LINES 条、单条截 400 字符，默认收起。`thoughts` 由 core
/// CardSession 累积（上限 10 条），此处再截显防超长推理占满面板。
const THOUGHT_PANEL_LINES: usize = 5;
fn render_thought_panel(thoughts: &[String], border_color: &str) -> serde_json::Value {
    let n = thoughts.len();
    let start = n.saturating_sub(THOUGHT_PANEL_LINES);
    let mut lines = String::new();
    for t in &thoughts[start..] {
        lines.push_str(&format!(
            "> {}\n",
            escape_lt(&mask_emails(&truncate_chars(t.trim(), 400)))
        ));
    }
    serde_json::json!({
        "tag": "collapsible_panel",
        "expanded": false,
        "header": panel_header(&format!("💭 思考过程（{n}）")),
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

/// 提示条（元信息/警示类注释行：审批倒计时、排队提示、掩码警告等）的小字形态。
///
/// 真机校准结论：schema 2.0 **不支持** `note` 组件（API 230099 / 200861
/// "unsupported tag note"——审批卡整卡被拒、降级纯文本）。V2 的小字提示用
/// markdown + `text_size: "notation"`（流式卡 footer 同款，已真机验证可发）。
fn note_element(text: &str) -> serde_json::Value {
    serde_json::json!({
        "tag": "markdown",
        "content": mask_emails(text),
        "text_size": "notation"
    })
}

/// 按工具名计数的统计行（`Bash×2 · Read×1`）——终态整卡与流式终态（结果下沉）
/// 共用。真机校准（2026-08）后为工具统计的**唯一**形态：CardKit 无可用胶囊
/// 组件（裸 "tag" 被 200621 拒收，见 [`render_card`] 的统计行注释）。
fn tool_stats_summary(tools: &[ToolCall]) -> String {
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for t in tools {
        *counts.entry(t.name.as_str()).or_default() += 1;
    }
    counts
        .iter()
        .map(|(t, n)| format!("{t}×{n}"))
        .collect::<Vec<_>>()
        .join(" · ")
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
///
/// Wave B-5：群 conv 顶部加「发起者」标注行——**独立元素**（md_body 会被流式
/// PATCH 整体覆盖，发起者行不能放进 md_body）；element PATCH 只动带 element_id
/// 的组件，本行在整个流式期持续可见。发起者 = 最近 sender 近似（见 platform
/// conv_senders 注释）。
pub fn render_stream_init_card(conv_id: &str, sender: Option<&str>) -> String {
    let mut elements = Vec::new();
    if let Some(line) = sender_anchor_line(sender, !crate::proto::is_private_conv(conv_id)) {
        elements.push(line);
    }
    elements.extend(vec![
        serde_json::json!({ "tag": "markdown", "element_id": "md_body", "content": "🧠 已接收任务，正在处理…" }),
        serde_json::json!({ "tag": "markdown", "element_id": "md_footer", "content": "🧠 思考中…", "text_size": "notation" }),
        // P9-1：⏹ 终止按钮常驻（element PATCH 只更新 markdown，按钮不受流式
        // 影响；终态后仍在，点击回「当前没有运行中的任务」，无害）。
        stop_button(conv_id, sender)
    ]);
    serde_json::json!({
        "schema": "2.0",
        "config": {
            "streaming_mode": true,
            "summary": { "content": "🧠 正在执行任务…" }
        },
        "body": { "elements": elements }
    })
    .to_string()
}

/// Running 期间 `md_body` 的流式内容：累积正文 + 工具调用紧凑列表。
///
/// 工具与正文同置一个 markdown 组件——CardKit 的 element 流式 PATCH 仅支持
/// markdown 组件（折叠面板不可流式更新），故 managed 路径下工具以引用行进正文
/// （lcab 文本模式的 `> ⏳ **Bash** — cmd` 同款）。
///
/// W2-1/W2-2：任务清单（checklist）置于正文**上方**（进度条语义，用户优先看
/// 到进行到哪一步）；思考过程取最近 1 条置底（实时「在想什么」，历史思考终态
/// 回看）。off 档的 Thought 在 core 侧已被过滤（不进卡）。
fn stream_body_md_inner(card: &OutboundCard) -> String {
    // L9（code-review v8 + 真机校准 2026-08-30）：md_body 全量重传 O(n²) 流量，
    // 且卡片上限按字节计（字符截断实测被 200860 拒）——Running 态字节制
    // 头尾窗口（各 4KB），中段截断标注；完整内容由终态/文本兜底承载。
    cap_md_bytes(&stream_body_md_inner_full(card), 4_096, 4_096)
}

fn stream_body_md_inner_full(card: &OutboundCard) -> String {
    let mut out = String::new();
    if let Some(todos) = todo_list_md(&card.todos) {
        out.push_str(&todos);
    }
    if !card.text.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&card.text);
    }
    if !card.tool_calls.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let tool_calls = &card.tool_calls;
        let n = tool_calls.len();
        let (skipped, shown) = if n > STREAM_TOOL_LINES {
            (n - STREAM_TOOL_LINES, &tool_calls[n - STREAM_TOOL_LINES..])
        } else {
            (0, tool_calls.as_slice())
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
    if let Some(thought) = card.thoughts.last() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("> 💭 {}", truncate_chars(thought, 120)));
    }
    if out.is_empty() {
        out.push_str("🧠 已接收任务，正在处理…");
    }
    mask_emails(&out)
}

pub fn stream_body_md(card: &OutboundCard) -> String {
    escape_lt(&stream_body_md_inner(card))
}

/// W2-2：任务清单 → markdown checklist 段（`- [x]`/`- [ ]`；进行中行尾 ⏳），
/// 标题带进度（`📋 计划（2/5）`）。空清单返回 None。
fn todo_list_md(todos: &[imagent_core::TodoItem]) -> Option<String> {
    if todos.is_empty() {
        return None;
    }
    let done = todos
        .iter()
        .filter(|t| t.status == imagent_core::TodoStatus::Completed)
        .count();
    let lines: Vec<String> = todos
        .iter()
        .map(|t| {
            let mark = match t.status {
                imagent_core::TodoStatus::Completed => "[x]",
                _ => "[ ]",
            };
            let icon = if t.status == imagent_core::TodoStatus::InProgress {
                " ⏳"
            } else {
                ""
            };
            format!("- {mark} {}{icon}", t.text)
        })
        .collect();
    Some(format!(
        "**📋 计划**（{}/{}）\n{}",
        done,
        todos.len(),
        lines.join("\n")
    ))
}

/// 按字符截断（卡片防溢出用）。
fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

/// 终态（Done/Error）时 `md_body` 的最终内容：正文 + 工具统计行 + 全量工具明细。
///
/// 统计行给一眼结论（按工具名计数：Bash×2 Read×3）；其后附**全量**工具引用行
/// ——managed 流式期正文只显最近 5 条（element PATCH 限制下的防刷屏），终态
/// 在同组件里补全明细，用户终态后可回看完整工具轨迹（降级/下沉路径另有折叠
/// 面板承载，见 [`render_tool_panel`]）。
///
/// W2-1/W2-2：任务清单终态置于正文上方（完成态 checklist）；思考过程取最近
/// 5 条以「💭 思考过程」段落收尾（流式期只显 1 条，终态补足回看）。
fn stream_body_final_inner(card: &OutboundCard, err: Option<&str>) -> String {
    let text = &card.text;
    let tool_calls = &card.tool_calls;
    let mut out = String::new();
    // 错误/中断说明进正文（footer 只有一句状态，装不下具体原因）；中断单列措辞。
    if let Some(e) = err {
        if e == "已中断" {
            out.push_str("⏹ 已中断\n\n");
        } else {
            out.push_str(&format!("❌ 出错：{e}\n\n"));
        }
    }
    if let Some(todos) = todo_list_md(&card.todos) {
        out.push_str(&todos);
        out.push_str("\n\n");
    }
    if !text.is_empty() {
        out.push_str(text);
    }
    if !tool_calls.is_empty() {
        // 按工具名计数：Bash×2 Read×3（tool_stats_summary，与终态整卡共用）。
        let stats = tool_stats_summary(tool_calls);
        // 长正文分段（CardKit 视觉改版）：正文与工具统计间 `---` 分割线 +
        // 工具明细块前小标题「工具轨迹」——managed 单 markdown 组件内用文本
        // 分割线（降级/整卡路径用真 hr 组件 + 面板标题，见 [`render_card`]）。
        if !out.is_empty() {
            out.push_str("\n\n---\n\n");
        }
        out.push_str(&format!(
            "🔧 工具 {} 次：{}\n\n**工具轨迹**\n",
            tool_calls.len(),
            stats
        ));
        // 全量明细（引用行形态，与流式期一致）——终态回看用。
        let lines: Vec<String> = tool_calls
            .iter()
            .map(|t| format!("> {}", tool_card_line(t)))
            .collect();
        out.push_str(&lines.join("\n"));
    }
    // W2-1：思考过程（最近 5 条，单条截 400 字符）——引用行形态段落于最末。
    if !card.thoughts.is_empty() {
        let start = card.thoughts.len().saturating_sub(5);
        let lines: Vec<String> = card.thoughts[start..]
            .iter()
            .map(|t| format!("> {}", truncate_chars(t.trim(), 400)))
            .collect();
        out.push_str(&format!("\n\n---\n\n**💭 思考过程**\n{}", lines.join("\n")));
    }
    // 终态状态行（✅ 已完成等）由 md_footer 承载——正文不再拼一份，
    // 否则同卡出现两行「完成」（真机反馈）。
    // P9-1：空正文 + 无工具的空产出给占位（空串 patch 组件可能被拒/显示空白）。
    if out.is_empty() {
        out.push_str("（未返回内容）");
    }
    mask_emails(&out)
}

pub fn stream_body_final(card: &OutboundCard, err: Option<&str>) -> String {
    // R8（code-review v9 残留）：终态 managed md_body 此前无截断，仅靠「超限
    // patch 失败→最小卡兜底」收敛（兜底又踩 R1）。与 render_card 主正文同额
    //（4KB+4KB 头尾窗）；被截时 core 侧 CARD_TEXT_FULL_THRESHOLD 成功路径
    // 会补发全文文本，不丢内容。
    escape_lt(&cap_md_bytes(
        &stream_body_final_inner(card, err),
        4_096,
        4_096,
    ))
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
    let lang = if tool_name == "Bash" || tool_name == "shell" {
        "bash"
    } else {
        ""
    };
    // Bash 的命令由下方代码块承载（解码后原文），head 不再重复命令摘要；
    // 其余工具 head 保留单行摘要。
    // R7（code-review v9 残留）：head 摘要行用户/agent 可控，<at id=…> 注入面
    // 与正文同权——escape_lt（代码块 body 不转义：CommonMark 代码块内反斜杠
    // 转义不生效，转义反而破坏显示，见 R13）。
    let head = if summary.is_empty() || lang == "bash" {
        format!("**{tool_name}**")
    } else {
        format!("**{tool_name}** — {}", escape_lt(&summary))
    };
    // 真机校准（2026-08）：Bash 审批的代码块直接展示**命令本身**，不再裹 JSON
    // 信封——pretty JSON 会把命令里的引号转义（\"）原样暴露，且 command 在
    // 信封里重复出现（head 摘要行已解码）。用户审批的对象是命令，不是它的
    // JSON 序列化形态。其它工具保留 pretty JSON（参数即内容，P8-1）。
    let raw: String = if lang == "bash" {
        match serde_json::from_str::<serde_json::Value>(input_summary) {
            Ok(v) => v
                .get("command")
                .and_then(|c| c.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| input_summary.to_string()),
            Err(_) => input_summary.to_string(),
        }
    } else {
        match serde_json::from_str::<serde_json::Value>(input_summary) {
            Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| input_summary.into()),
            // 截断的 JSON（超长输入）：解析失败原样展示。
            Err(_) => input_summary.to_string(),
        }
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
/// 审批卡 note 行缺省文案：自动拒绝的具体倒计时（`permission_ask_timeout_secs`
/// 真实值经构造注入，见 [`humanize_ask_timeout`]）——静态「长时间未处理」让用户
/// 无从判断还剩多久。
pub(crate) fn perm_note_default(ask_timeout_secs: u64) -> String {
    format!(
        "⏱️ 将在 {}后自动拒绝 · 回复 always = 本次会话内此工具不再询问",
        humanize_ask_timeout(ask_timeout_secs)
    )
}

/// 询问类按钮（`imagent_perm` / `imagent_ask` / `imagent_form`）value 公共字段：
/// 在动作键之外补 conv + req + `ts`（epoch 秒，回调侧超 24h 拒绝——与命令按钮
/// 同窗口）+ `sender`（发起者 open_id，回调侧**全形态**校验点击者——防卡片被
/// 转发到其它会话后代批）。
fn ask_value_wrap(
    mut v: serde_json::Value,
    conv_id: &str,
    request_id: &str,
    sender: Option<&str>,
) -> serde_json::Value {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    v["conv"] = serde_json::json!(conv_id);
    v["req"] = serde_json::json!(request_id);
    v["ts"] = serde_json::json!(ts);
    if let Some(s) = sender.filter(|s| !s.is_empty()) {
        v["sender"] = serde_json::json!(s);
    }
    v
}

pub fn render_permission_card(
    tool_name: &str,
    input_summary: &str,
    conv_id: &str,
    request_id: &str,
    sender: Option<&str>,
    ask_timeout_secs: u64,
) -> String {
    render_permission_card_note(
        tool_name,
        input_summary,
        conv_id,
        request_id,
        sender,
        &perm_note_default(ask_timeout_secs),
    )
}

/// P10-③：note 行可参数化（排队联动重渲染用，见 platform 的 note_queued_on_ask）。
/// `sender` 参与按钮 value 编码——重渲染时保持原卡的发起者/时效字段不丢
/// （复用槽换询问后发起者可能变化，按当前值编码）。
pub(crate) fn render_permission_card_note(
    tool_name: &str,
    input_summary: &str,
    conv_id: &str,
    request_id: &str,
    sender: Option<&str>,
    note: &str,
) -> String {
    let (detail, detail_notes) = perm_detail_md(tool_name, input_summary);
    let mut elements = vec![
        serde_json::json!({ "tag": "markdown", "content": detail }),
        // 倒计时 / 排队提示 note（markdown+notation 小字；note 组件 V2 已移除，
        // 真机校准 2026-08）。md_footer 锚点不受影响（managed 卡约束）。
        note_element(note),
    ];
    // 截断 / 掩码警告同样 note 化（元信息类注释行）。
    for n in &detail_notes {
        elements.push(note_element(n));
    }
    elements.push(serde_json::json!({ "tag": "hr" }));
    // 按钮布局（真机校准 2026-08 第三轮）：主操作一行两枚**等宽**填充按钮
    //（weighted 1:1 列 + 按钮 width=fill 拉满列宽——flow+auto 形态按钮随内容
    // 宽度参差，emoji 差一个字符宽都显不齐）；次级动作「♾️ 本次会话始终允许」
    // 独占下一行整宽描边按钮（V2 支持按钮直接入 elements）。允许 primary_filled
    //（CardKit 的 primary 实为蓝字描边，填充档是 primary_filled）。
    let mut allow_btn = cb_button(
        "允许",
        "primary_filled",
        ask_value_wrap(
            serde_json::json!({ "imagent_perm": "allow" }),
            conv_id,
            request_id,
            sender,
        ),
    );
    allow_btn["width"] = serde_json::json!("fill");
    let mut deny_btn = cb_button(
        "⛔ 拒绝",
        "danger_filled",
        ask_value_wrap(
            serde_json::json!({ "imagent_perm": "deny" }),
            conv_id,
            request_id,
            sender,
        ),
    );
    deny_btn["width"] = serde_json::json!("fill");
    let mut always_btn = cb_button(
        "♾️ 本次会话始终允许",
        "default",
        ask_value_wrap(
            serde_json::json!({ "imagent_perm": "always" }),
            conv_id,
            request_id,
            sender,
        ),
    );
    always_btn["width"] = serde_json::json!("fill");
    elements.push(serde_json::json!({
        "tag": "column_set", "flex_mode": "bisect", "horizontal_spacing": "default",
        "columns": [
            { "tag": "column", "width": "weighted", "weight": 1, "elements": [allow_btn] },
            { "tag": "column", "width": "weighted", "weight": 1, "elements": [deny_btn] }
        ]
    }));
    elements.push(always_btn);
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
pub fn render_question_card(
    tool_input: &str,
    conv_id: &str,
    request_id: &str,
    sender: Option<&str>,
    ask_timeout_secs: u64,
) -> Option<String> {
    render_question_card_note(
        tool_input,
        conv_id,
        request_id,
        sender,
        &perm_note_default(ask_timeout_secs),
    )
}

/// P10-③：note 行可参数化（同审批卡；`sender` 参与按钮 value 编码，语义同
/// [`render_permission_card_note`]）。
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
    sender: Option<&str>,
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
    // P0-AUQ（v1.17）：多问题 → 单卡一次提交（CLI 的 AskUserQuestion 一次
    // control 交互=整个工具调用，逐题追问做不到——协议只给一次响应机会）。
    // 每题一个表单字段 ask_opt_{i}，选项 value=「{header}={label}」——回调
    // 拼接成校准过的 `用户选择：Q1=x；Q2=y` 格式一次回传。
    if n_questions > 1 {
        return render_multi_question_card(&v, conv_id, request_id, sender, note);
    }
    let use_form = multi || opts.len() > 4;
    let content = format!("❓ {question}");
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
            serde_json::json!({ "tag": "markdown", "content": escape_lt(&mask_emails(&content)) }),
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
                    "behaviors": [{ "type": "callback", "value": ask_value_wrap(
                        serde_json::json!({ "imagent_form": "ask" }),
                        conv_id, request_id, sender,
                    ) }]
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
                    ask_value_wrap(
                        serde_json::json!({ "imagent_ask": label }),
                        conv_id,
                        request_id,
                        sender,
                    ),
                )
            })
            .collect();
        vec![
            serde_json::json!({ "tag": "markdown", "content": escape_lt(&mask_emails(&content)) }),
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

/// P0-AUQ（v1.17）：多问题单卡一次提交。每题一节（题头/题面/选项描述小字）+
/// 一个表单控件（multiSelect→checkbox，否则 select_static——按钮做不了按题
/// 分组）；一次「提交」回传全部选择。选项 value=「{header}={label}」，回调侧
/// 按 `；` 拼接成校准过的多答案格式（deny+message 一次带回）。
fn render_multi_question_card(
    v: &serde_json::Value,
    conv_id: &str,
    request_id: &str,
    sender: Option<&str>,
    note: &str,
) -> Option<String> {
    let qs = v.get("questions")?.as_array()?;
    let mut fields: Vec<serde_json::Value> = Vec::new();
    let mut sections: Vec<serde_json::Value> = Vec::new();
    for (i, q) in qs.iter().enumerate() {
        let question = q.get("question")?.as_str()?.trim().to_string();
        let header = q
            .get("header")
            .and_then(|h| h.as_str())
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| question.chars().take(10).collect());
        let multi = q
            .get("multiSelect")
            .and_then(|m| m.as_bool())
            .unwrap_or(false);
        let mut lines = vec![format!("**{}. {}** — {question}", i + 1, header)];
        let mut opt_values: Vec<serde_json::Value> = Vec::new();
        for o in q.get("options")?.as_array()? {
            let label = o.get("label")?.as_str()?.trim().to_string();
            if label.is_empty() {
                continue;
            }
            if let Some(d) = o
                .get("description")
                .and_then(|d| d.as_str())
                .filter(|d| !d.trim().is_empty())
            {
                lines.push(format!("- **{label}**：{}", truncate_chars(d.trim(), 60)));
            } else {
                lines.push(format!("- **{label}**"));
            }
            // value 携带题头（回调拼接 用户选择：题=答案）。
            opt_values.push(serde_json::json!({
                "text": { "tag": "plain_text", "content": label },
                "value": format!("{header}={label}")
            }));
        }
        if opt_values.is_empty() {
            return None;
        }
        sections.push(serde_json::json!({
            "tag": "markdown", "content": mask_emails(&lines.join("\n"))
        }));
        fields.push(if multi {
            serde_json::json!({ "tag": "checkbox", "name": format!("ask_opt_{i}"), "options": opt_values })
        } else {
            serde_json::json!({ "tag": "select_static", "name": format!("ask_opt_{i}"), "options": opt_values })
        });
        // v1.17.2：每题附自由输入框（对齐 CLI 原生「用户自定义回答」）——
        // name=ask_opt_{i}_free，回调侧非空则优先于选项（placeholder 注明）。
        fields.push(serde_json::json!({
            "tag": "input",
            "name": format!("ask_opt_{i}_free"),
            "placeholder": { "tag": "plain_text", "content": "或自行输入（填写则优先于上方选项）" },
            "max_length": 300
        }));
    }
    let mut elements = vec![note_element(note), serde_json::json!({ "tag": "hr" })];
    // 题面与控件交替：题 i 的说明紧邻其控件（全说明在上/全控件在下会被滚动分离）。
    // fields 长度 = 2×题数（控件+自由输入框），与 sections 按题分组对应。
    let mut fit = fields.into_iter();
    for s in sections.into_iter() {
        elements.push(s);
        if let Some(f) = fit.next() {
            elements.push(f);
        }
        if let Some(f) = fit.next() {
            elements.push(f);
        }
    }
    elements.push(serde_json::json!({ "tag": "hr" }));
    elements.push(flow_button_row(&[serde_json::json!({
        "tag": "button",
        "name": "submit_btn",
        "text": { "tag": "plain_text", "content": "提交全部答案" },
        "type": "primary",
        "form_action_type": "submit",
        "behaviors": [{ "type": "callback", "value": ask_value_wrap(
            serde_json::json!({ "imagent_form": "ask" }),
            conv_id, request_id, sender,
        )}]
    })]));
    // checkbox 必须包在 form 里才有提交语义；select_static 同form（一次提交全部）。
    let elements = vec![serde_json::json!({
        "tag": "form", "name": "imagent_ask", "elements": elements
    })];
    Some(
        serde_json::json!({
            "schema": "2.0",
            "header": {
                "title": { "tag": "plain_text", "content": "❓ 需要你的输入（多题一次提交）" },
                "template": "blue"
            },
            "body": { "elements": elements }
        })
        .to_string(),
    )
}

/// AskUserQuestion input → 人读问题列表（文本降级路径用；v1.17.1）。
/// 解析失败返回 None（调用方回落审批文本格式）。
pub fn questions_as_text(tool_input: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(tool_input).ok()?;
    let qs = v.get("questions")?.as_array()?;
    let mut out = String::from("需要你的输入（多题请逐题作答）：");
    for (i, q) in qs.iter().enumerate() {
        let question = q.get("question")?.as_str()?.trim();
        let header = q
            .get("header")
            .and_then(|h| h.as_str())
            .filter(|h| !h.trim().is_empty())
            .unwrap_or(question);
        let labels: Vec<String> = q
            .get("options")?
            .as_array()?
            .iter()
            .filter_map(|o| o.get("label")?.as_str().map(str::to_string))
            .collect();
        if question.is_empty() || labels.is_empty() {
            return None;
        }
        out.push_str(&format!(
            "\n{}. 【{header}】{question}\n   选项：{}",
            i + 1,
            labels.join(" / ")
        ));
    }
    Some(out)
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
/// 真机校准（2026-08-30）：终态 patch 因卡片超限（200860/230099）失败时，
/// 原流式卡会永远停在「思考中」——最小化终态卡兜底重试（完整内容已由
/// core P5-11 纯文本补发，卡片只需表达终态）。
pub fn render_overflow_terminal_card(done: bool) -> String {
    let (title, template) = if done {
        ("✅ 已完成", "green")
    } else {
        ("⚠️ 出错", "red")
    };
    serde_json::json!({
        "schema": "2.0",
        "header": { "title": { "tag": "plain_text", "content": title }, "template": template },
        "body": { "elements": [
            { "tag": "markdown", "content": "输出超出卡片大小上限，完整内容已转为**文本消息**发送（见下方/相邻消息）。" }
        ]}
    })
    .to_string()
}

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
                    "tag": "markdown", "content": escape_lt(&mask_emails(&content))
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

/// /resume 行的左列元素：来源标记（💻 本机 / 📱 IM）+ 时间 · 内容。
///
/// 真机校准（2026-08）：裸 "tag" 组件被 200621 拒收（整卡降级），V2 无等价
/// 胶囊——来源并入 markdown 行首（emoji 文本承载，无组件风险）。
fn resume_row_left(cells: &[String]) -> Vec<serde_json::Value> {
    let mut md = String::new();
    if cells.len() > 1 && !cells[1].is_empty() {
        let source = match cells[1].as_str() {
            "💻" => "💻 本机",
            "📱" => "📱 IM",
            other => other,
        };
        md.push_str(source);
    }
    if cells.len() > 2 && !cells[2].is_empty() {
        if !md.is_empty() {
            md.push_str(" · ");
        }
        md.push_str(&cells[2]);
    }
    if cells.len() > 3 && !cells[3].is_empty() {
        if !md.is_empty() {
            md.push_str(" · ");
        }
        md.push_str(&cells[3]);
    }
    if md.is_empty() {
        Vec::new()
    } else {
        vec![serde_json::json!({ "tag": "markdown", "content": md })]
    }
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
            id: None,
        }
    }

    /// W2 测试辅助：最小 Running 态 OutboundCard（text + tools + thoughts）。
    /// 与既有 `card_of`（terminal 维度）区分命名。
    fn body_card_of(text: &str, tools: &[ToolCall], thoughts: &[&str]) -> OutboundCard {
        OutboundCard {
            text: text.into(),
            tool_calls: tools.to_vec(),
            thoughts: thoughts.iter().map(|s| s.to_string()).collect(),
            todos: Vec::new(),
            phase: CardPhase::Thinking,
            queued_hint: None,
            run_secs: 0,
            usage_display: None,
            terminal: CardTerminal::Running,
        }
    }

    #[test]
    fn render_running_has_markdown() {
        let card = OutboundCard {
            text: "hello".into(),
            tool_calls: vec![],
            phase: CardPhase::Thinking,
            thoughts: Vec::new(),
            todos: Vec::new(),
            queued_hint: None,
            terminal: CardTerminal::Running,
            usage_display: None,
            run_secs: 0,
        };
        let json = render_card(&card, "feishu:ou_t", None);
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
                thoughts: Vec::new(),
                todos: Vec::new(),
                queued_hint: None,
                terminal: CardTerminal::Running,
                usage_display: None,
                run_secs: 0,
            };
            assert!(
                render_card(&card, "feishu:ou_t", None).contains(mark),
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
            thoughts: Vec::new(),
            todos: Vec::new(),
            queued_hint: None,
            terminal: CardTerminal::Done,
            usage_display: None,
            run_secs: 0,
        };
        let json = render_card(&card, "feishu:ou_t", None);
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
            thoughts: Vec::new(),
            todos: Vec::new(),
            queued_hint: None,
            terminal: CardTerminal::Done,
            usage_display: None,
            run_secs: 0,
        };
        let json = render_card(&card, "feishu:ou_t", None);
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
            thoughts: Vec::new(),
            todos: Vec::new(),
            queued_hint: None,
            terminal: CardTerminal::Running,
            usage_display: None,
            run_secs: 0,
        };
        let md = stream_body_md(&running);
        assert!(md.contains("前面还有 5 个工具"), "流式折叠计数: {md}");
        assert!(!md.contains("cmd-0"), "流式不显最早: {md}");
    }

    #[test]
    fn render_error() {
        let card = OutboundCard {
            text: "".into(),
            tool_calls: vec![],
            phase: CardPhase::Thinking,
            thoughts: Vec::new(),
            todos: Vec::new(),
            queued_hint: None,
            terminal: CardTerminal::Error("boom".into()),
            usage_display: None,
            run_secs: 0,
        };
        let json = render_card(&card, "feishu:ou_t", None);
        assert!(json.contains("boom"));
        assert!(json.contains("❌ 出错"), "终态 footer: {json}");
    }

    /// v1.17.1 真机输入回归：话题内 4 问（带 150+ 字描述）曾因话题分支不走
    /// 问题渲染降级审批卡——本测试钉住真实形态的输入必须渲染出多题卡。
    #[test]
    fn question_card_real_device_4q_input() {
        let input = r#"{"questions":[{"question":"本次部署选择哪个环境？","header":"部署环境","multiSelect":false,"options":[{"label":"测试环境","description":"选择测试环境意味着新版本将首先部署到隔离的测试集群或测试服务器上，不会影响线上真实用户的访问。测试环境通常配置与生产相近，但数据为模拟或脱敏数据，适合验证新功能的正确性、接口兼容性以及性能表现。在测试环境完成冒烟测试、回归测试并通过验收标准后，再推进到生产环境，可以大幅降低发布风险，是大多数团队推荐的标准发布流程的第一步。"},{"label":"生产环境","description":"选择生产环境意味着本次变更将直接发布到线上，真实用户流量会立即接触到新版本。这种方式适用于紧急修复（如线上故障的热修复）、改动极小且风险可控的场景，或者测试环境已提前完成充分验证的情况。直接上生产虽然节省时间，但一旦存在问题会直接影响用户体验和业务指标，建议配合灰度发布、蓝绿部署或快速回滚方案，以确保出现异常时能在最短时间内恢复。"}]},{"question":"部署前是否进行备份？","header":"部署前备份","multiSelect":false,"options":[{"label":"备份","description":"部署前对数据库、配置文件、静态资源以及当前运行的程序版本进行完整备份，是保障发布安全的关键措施。一旦新版本出现数据异常、配置错误或功能回退，可以迅速通过备份回滚到上一个稳定状态，将故障影响时间和范围降到最低。备份内容应包括数据库快照、关键配置项的导出以及当前制品版本的归档，并建议在部署前验证备份的完整性和可恢复性，避免需要回滚时才发现备份不可用。"},{"label":"不备份","description":"跳过备份可以缩短部署前的准备时间，适用于本次变更不涉及数据库结构或数据变更、仅为静态资源更新或前端页面调整等低风险场景。但需要明确的是，一旦部署过程中出现意外（如配置覆盖错误、数据误写、版本损坏），将没有直接的回滚依据，只能通过重新构建旧版本或手工修复来恢复，恢复周期和不确定性都会显著增加。选择此项前请确认本次变更确实无数据风险且具备快速重建能力。"}]},{"question":"部署前是否通知团队？","header":"团队通知","multiSelect":false,"options":[{"label":"通知团队","description":"部署前通过群消息、邮件或工单系统通知研发、测试、运维及相关业务方，说明发布时间、影响范围、变更内容和回滚预案。这样做可以让相关人员在出现异常时第一时间知晓原因并协同处理，避免值班同学面对突发状况毫无头绪；同时也便于测试同学在发布后及时跟进验证，业务方也能提前向用户说明可能出现的短暂波动。透明的发布沟通是成熟团队协作的基本规范，尤其是生产环境发布更应坚持这一原则。"},{"label":"不通知","description":"不主动通知团队，适用于深夜低风险的小型变更、个人测试项目，或团队已明确约定无需通知的自动化发布流程（例如 CI/CD 流水线自动触发）。好处是减少沟通成本、流程更轻量，发布者可以完全自主掌控节奏。但风险在于：一旦部署引发线上异常，其他人对此变更毫不知情，排查问题时会缺少关键上下文，值班或接手同学可能重复排查甚至误判原因，协作效率反而下降。请确认本次变更影响面足够小再选择此项。"}]},{"question":"部署是立即执行还是等待窗口期？","header":"执行时机","multiSelect":false,"options":[{"label":"立即执行","description":"确认方案后马上开始部署，适合紧急修复线上故障、解决阻塞他人进展的问题、或已经过充分测试验证且风险很低的变更。立即执行能最快让变更生效，缩短问题暴露和修复之间的等待时间。但要注意：如果当前正处于业务高峰期（如促销活动、用户访问高峰），立即上线可能放大潜在问题的影响面。选择此项前建议确认当前流量处于安全水平、回滚方案已就绪，并且你有足够时间在场观察发布后的监控指标。"},{"label":"等待窗口期","description":"将部署安排在预先约定的低流量时间窗口（如深夜、凌晨或周末）执行，这是生产发布的行业惯例。窗口期内用户访问量低，即使出现异常，受影响的用户数量和业务损失也最小；同时此时段通常没有其他变更并行发布，出现问题更容易定位归因。代价是需要等待，紧急问题可能等不起，且深夜发布对执行者的精力也是考验，建议窗口期发布配合双人复核机制。如果变更不紧急且影响面较大，优先推荐等待窗口期执行。"}]}]}"#;
        let rendered = render_question_card(input, "feishu:oc_test", "req_test", None, 300);
        assert!(rendered.is_some(), "真机 4 问输入应渲染多题卡");
        if let Some(json) = rendered {
            assert!(json.contains("ask_opt_0"), "多题字段: {json}");
        }
    }

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
        let json = render_question_card(&input, "feishu:ou_q", "req1", Some("ou_q"), 300)
            .expect("应可渲染");
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
        assert!(render_question_card("not json", "c", "req1", None, 300).is_none());
        assert!(render_question_card("{}", "c", "req1", None, 300).is_none());
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
            Some("ou_u1"),
            300,
        );
        // 标题栏 + 主题色。
        assert!(json.contains("权限审批"), "标题栏: {json}");
        assert!(
            json.contains("\"template\":\"orange\""),
            "审批主题色: {json}"
        );
        // 签名行 + bash 代码块。真机校准（2026-08）：Bash 代码块为**命令本身**
        // （解码原文，无 JSON 信封、无 \" 转义噪声）；head 不再重复命令摘要。
        assert!(json.contains("**Bash**\\n```bash"), "签名行: {json}");
        assert!(
            json.contains("```bash\\ncargo test --all\\n```"),
            "命令原文直出: {json}"
        );
        assert!(
            !json.contains("\\\"command\\\""),
            "Bash 不应再裹 JSON 信封: {json}"
        );
        // 两个按钮 + callback value 编码 conv 与动作。允许按钮不带 ✅（primary
        // 蓝底已高亮，绿色系 emoji 冲突）；⛔ 拒绝（danger）保留。
        assert!(json.contains("\"content\":\"允许\""), "允许按钮: {json}");
        assert!(json.contains("⛔ 拒绝"), "拒绝按钮: {json}");
        // 真机校准（2026-08 第三轮）：三动作齐备——允许/拒绝等宽填充主行
        //（weighted 1:1 + 按钮 width=fill），始终允许独立整宽次级行。
        assert!(
            json.contains("\"imagent_perm\":\"allow\"")
                && json.contains("\"imagent_perm\":\"deny\"")
                && json.contains("\"imagent_perm\":\"always\""),
            "三个动作都应编码: {json}"
        );
        assert!(
            json.contains("\"primary_filled\"") && json.contains("\"danger_filled\""),
            "主行填充双按钮: {json}"
        );
        assert!(
            json.contains("\"width\":\"weighted\"") && json.contains("\"weight\":1"),
            "主行等宽列: {json}"
        );
        assert!(
            json.matches("\"width\":\"fill\"").count() >= 3,
            "三按钮均拉满列宽: {json}"
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
            None,
            300,
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
            thoughts: Vec::new(),
            todos: Vec::new(),
            queued_hint: Some("📥 排队 1 条".into()),
            terminal: CardTerminal::Running,
            usage_display: None,
            run_secs: 0,
        };
        let json = render_card(&card, "feishu:ou_t", None);
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
            Some("ou_t"),
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
        let plain = render_permission_card("Bash", r#"{"command":"ls"}"#, "c", "r", None, 300);
        assert!(
            plain.contains("将在 5 分钟后自动拒绝"),
            "默认倒计时: {plain}"
        );
    }

    /// Bug：审批卡倒计时真实值透传——`permission_ask_timeout_secs` 换算显示
    /// （≥90s 显示分钟、否则秒），不再硬编码 5 分钟。
    #[test]
    fn ask_timeout_humanize_and_note_passthrough() {
        assert_eq!(humanize_ask_timeout(300), "5 分钟");
        assert_eq!(humanize_ask_timeout(60 * 30), "30 分钟");
        assert_eq!(humanize_ask_timeout(90), "2 分钟", "90s 四舍五入到分钟");
        assert_eq!(humanize_ask_timeout(89), "89 秒", "<90s 显示秒");
        assert_eq!(humanize_ask_timeout(45), "45 秒");
        // 自定义超时（60s）的卡片 note 按实际值显示。
        let json = render_permission_card("Bash", r#"{"command":"ls"}"#, "c", "r", None, 60);
        assert!(
            json.contains("将在 60 秒后自动拒绝"),
            "短超时显示秒: {json}"
        );
        assert!(!json.contains("5 分钟"), "不得再出现硬编码 5 分钟: {json}");
        // 长超时（1800s）显示分钟。
        let json = render_permission_card("Bash", r#"{"command":"ls"}"#, "c", "r", None, 1800);
        assert!(
            json.contains("将在 30 分钟后自动拒绝"),
            "长超时显示分钟: {json}"
        );
    }

    /// 安全（转发代批）：审批/问题卡按钮 value 补 sender（发起者）与 ts（时效），
    /// 与命令按钮同款编码；无 sender（未知发起者）不编码该字段。
    #[test]
    fn ask_button_value_carries_sender_and_ts() {
        let json = render_permission_card(
            "Bash",
            r#"{"command":"ls"}"#,
            "feishu:oc_g",
            "req_s",
            Some("ou_owner"),
            300,
        );
        assert!(
            json.contains("\"sender\":\"ou_owner\""),
            "审批按钮带发起者: {json}"
        );
        assert!(json.contains("\"ts\":"), "审批按钮带时效戳: {json}");
        // 无 sender：不编码（私聊/未知发起者）。
        let plain = render_permission_card("Bash", r#"{"command":"ls"}"#, "c", "r", None, 300);
        assert!(!plain.contains("\"sender\":"), "无发起者不编码: {plain}");
        assert!(plain.contains("\"ts\":"), "时效戳恒编码: {plain}");
        // 问题卡（按钮形态与表单形态）同样编码。
        let input = serde_json::json!({
            "questions": [{"question": "选？", "options": [{"label":"A"},{"label":"B"}]}]
        })
        .to_string();
        let q = render_question_card(&input, "feishu:oc_g", "rq", Some("ou_owner"), 300).unwrap();
        assert!(
            q.contains("\"sender\":\"ou_owner\"") && q.contains("\"ts\":"),
            "问题卡按钮带 sender+ts: {q}"
        );
        let many = serde_json::json!({
            "questions": [{
                "question": "选哪个方案？",
                "options": (1..=6).map(|i| serde_json::json!({"label": format!("方案{i}")})).collect::<Vec<_>>()
            }]
        })
        .to_string();
        let form = render_question_card(&many, "feishu:oc_g", "rq", Some("ou_owner"), 300).unwrap();
        assert!(
            form.contains("\"imagent_form\":\"ask\"") && form.contains("\"sender\":\"ou_owner\""),
            "表单提交按钮同样带 sender: {form}"
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
            thoughts: Vec::new(),
            todos: Vec::new(),
            queued_hint: None,
            terminal: CardTerminal::Running,
            usage_display: None,
            run_secs: 0,
        };
        let json = render_card(&running, "feishu:ou_t", None);
        assert!(json.contains("⏹ 终止"), "Running 降级卡带终止按钮: {json}");
        let done = OutboundCard {
            text: "ok".into(),
            tool_calls: vec![],
            phase: CardPhase::Outputting,
            thoughts: Vec::new(),
            todos: Vec::new(),
            queued_hint: None,
            terminal: CardTerminal::Done,
            usage_display: None,
            run_secs: 0,
        };
        let json2 = render_card(&done, "feishu:ou_t", None);
        assert!(!json2.contains("⏹ 终止"), "终态不带终止按钮: {json2}");
    }

    /// P9-1：空产出占位（空串 patch 可能被拒/显示空白）。
    #[test]
    fn stream_body_final_empty_placeholder() {
        assert_eq!(
            stream_body_final(&body_card_of("", &[], &[]), None),
            "（未返回内容）"
        );
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

    /// W2-2：任务清单渲染——checklist 置正文上方，进度计数 + 进行中 ⏳；
    /// W2-1：思考片段置底（Running 只显最近 1 条）。
    #[test]
    fn stream_body_renders_todos_and_thoughts() {
        let todos = vec![
            imagent_core::TodoItem {
                id: None,
                text: "分析需求".into(),
                status: imagent_core::TodoStatus::Completed,
            },
            imagent_core::TodoItem {
                id: None,
                text: "写代码".into(),
                status: imagent_core::TodoStatus::InProgress,
            },
            imagent_core::TodoItem {
                id: None,
                text: "测试".into(),
                status: imagent_core::TodoStatus::Pending,
            },
        ];
        let mut card = body_card_of("正文内容", &[], &["旧思考", "最新思考"]);
        card.todos = todos;
        let md = stream_body_md(&card);
        assert!(md.contains("**📋 计划**（1/3）"), "进度计数: {md}");
        assert!(md.contains("- [x] 分析需求"), "完成项: {md}");
        assert!(md.contains("- [ ] 写代码 ⏳"), "进行中项: {md}");
        assert!(md.contains("- [ ] 测试"), "待办项: {md}");
        assert!(md.starts_with("**📋 计划**"), "清单置正文上方: {md}");
        assert!(md.contains("正文内容"), "正文仍在: {md}");
        assert!(md.contains("> 💭 最新思考"), "最新思考置底: {md}");
        assert!(!md.contains("旧思考"), "Running 只显最近 1 条思考: {md}");
    }

    /// W2-1：终态思考段落（最近 5 条）与整卡折叠面板。
    #[test]
    fn final_and_full_card_render_thoughts() {
        let thoughts: Vec<String> = (0..7).map(|i| format!("思考{i}")).collect();
        let mut card = body_card_of("结论", &[], &[]);
        card.thoughts = thoughts.clone();
        card.terminal = CardTerminal::Done;
        let out = stream_body_final(&card, None);
        assert!(out.contains("**💭 思考过程**"), "终态段落标题: {out}");
        assert!(!out.contains("思考0"), "只保留最近 5 条: {out}");
        assert!(out.contains("思考6"), "最新思考在: {out}");
        // 整卡路径：折叠面板（默认收起）承载。
        let json = render_card(&card, "feishu:ou_t", None);
        assert!(json.contains("💭 思考过程（7）"), "面板标题带计数: {json}");
        assert!(json.contains("collapsible_panel"), "面板形态: {json}");
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
        assert_eq!(
            stream_body_md(&body_card_of("", &[], &[])),
            "🧠 已接收任务，正在处理…"
        );
        // 文本 + 工具都有：引用行 + 状态图标 + 加粗工具名。
        let tools = vec![tool("Bash", "ls -la", false)];
        let md = stream_body_md(&body_card_of("进度", &tools, &[]));
        assert!(md.contains("进度"));
        assert!(md.contains("⏳ **Bash** — ls -la"), "工具引用行: {md}");
        // 仅工具（无正文）。
        let only = stream_body_md(&body_card_of("", &tools, &[]));
        assert!(only.starts_with("> ⏳"), "无正文时工具行开头: {only}");
        // 超出 5 个折叠 + 计数。
        let many: Vec<ToolCall> = (0..8)
            .map(|i| tool("Read", &format!("f{i}"), true))
            .collect();
        let md2 = stream_body_md(&body_card_of("", &many, &[]));
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
            thoughts: Vec::new(),
            todos: Vec::new(),
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
        let out = stream_body_final(&body_card_of("结论", &tools, &[]), None);
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
        let err = stream_body_final(&body_card_of("", &[], &[]), Some("boom"));
        assert!(err.contains("❌ 出错：boom"), "错误前置: {err}");
        // 中断单列（非出错）。
        let stop = stream_body_final(&body_card_of("", &[], &[]), Some("已中断"));
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
            None,
            300,
        );
        assert!(
            json.contains("已截断，仅显示前 1000 字符"),
            "截断提示: {json}"
        );
        // 短输入无提示。
        let short = render_permission_card("Bash", r#"{"command":"ls"}"#, "c", "r", None, 300);
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
            None,
            300,
        );
        assert!(json.contains("[at]"), "掩码仍生效（审计强制）: {json}");
        assert!(json.contains("邮箱已掩码显示"), "掩码提示: {json}");
        assert!(
            json.contains("原命令可直接执行"),
            "告知原命令语义不变: {json}"
        );
        // 无邮箱的命令不出现提示。
        let plain = render_permission_card("Bash", r#"{"command":"ls -la"}"#, "c", "r", None, 300);
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
        let json = render_question_card(&mk_input(false), "feishu:ou_q", "reqF", None, 300)
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
        let multi = render_question_card(&mk_input(true), "feishu:ou_q", "reqM", None, 300)
            .expect("多选应渲染");
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
        let btn = render_question_card(&few, "c", "r", None, 300).expect("少选项应渲染");
        assert!(btn.contains("\"imagent_ask\":\"A\""), "按钮形态保留: {btn}");
        assert!(!btn.contains("\"tag\":\"form\""), "少选项不用表单: {btn}");
        // 多问题（P0-AUQ v1.17）：单卡一次提交——每题一个 ask_opt_{i} 字段，
        // 选项 value=「题头=选项」，一次提交全部。
        let multi_q = serde_json::json!({
            "questions": [
                {"question": "第一问？", "header": "问一", "options": [{"label":"A"}]},
                {"question": "第二问？", "header": "问二",
                 "multiSelect": true,
                 "options": [{"label":"B"},{"label":"C","description":"说明"}]}
            ]
        })
        .to_string();
        let mq = render_question_card(&multi_q, "c", "r", None, 300).expect("应渲染");
        assert!(
            mq.contains("\"name\":\"ask_opt_0\"") && mq.contains("\"name\":\"ask_opt_1\""),
            "每题一字段: {mq}"
        );
        assert!(
            mq.contains("\"value\":\"问一=A\"") && mq.contains("\"value\":\"问二=B\""),
            "value 携带题头: {mq}"
        );
        assert!(mq.contains("提交全部答案"), "一次提交: {mq}");
        assert!(mq.contains("说明"), "选项描述保留: {mq}");
        // 第二题 multiSelect → checkbox；第一题 select_static。
        assert!(
            mq.contains("\"tag\":\"checkbox\"") && mq.contains("\"tag\":\"select_static\""),
            "控件分流: {mq}"
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
            thoughts: Vec::new(),
            todos: Vec::new(),
            queued_hint: None,
            terminal: CardTerminal::Running,
            usage_display: None,
            run_secs: 0,
        };
        let json = render_card(&card, "feishu:ou_t", None);
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
            thoughts: Vec::new(),
            todos: Vec::new(),
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
        let done = render_card(&card_of(CardTerminal::Done, vec![]), "feishu:ou_t", None);
        assert!(
            done.contains("\"template\":\"green\"") && done.contains("✅ 已完成"),
            "Done header: {done}"
        );
        let err = render_card(
            &card_of(CardTerminal::Error("boom".into()), vec![]),
            "feishu:ou_t",
            None,
        );
        assert!(
            err.contains("\"template\":\"red\"") && err.contains("❌ 出错"),
            "Error header: {err}"
        );
        let stop = render_card(
            &card_of(CardTerminal::Error("已中断".into()), vec![]),
            "feishu:ou_t",
            None,
        );
        assert!(
            stop.contains("\"template\":\"grey\"") && stop.contains("⏹ 已中断"),
            "中断 header: {stop}"
        );
        let running = render_card(&card_of(CardTerminal::Running, vec![]), "feishu:ou_t", None);
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

    /// ② 提示条：审批倒计时 / 排队提示 / 掩码警告 / 超时原因均为 markdown+notation
    /// 小字（真机校准：note 组件 schema 2.0 已移除，230099/200861）。
    #[test]
    fn note_elements_for_meta_lines() {
        let json =
            render_permission_card("Bash", r#"{"command":"ls"}"#, "feishu:ou_t", "r", None, 300);
        assert!(
            json.contains("\"text_size\":\"notation\"") && json.contains("分钟后自动拒绝"),
            "倒计时 note: {json}"
        );
        let queued = render_permission_card_note(
            "Bash",
            r#"{"command":"ls"}"#,
            "feishu:ou_t",
            "r",
            None,
            "⏳ 等待你审批 · 后面还排着 3 条消息",
        );
        assert!(
            queued.contains("\"text_size\":\"notation\"") && queued.contains("等待你审批"),
            "排队 note: {queued}"
        );
        let masked = render_permission_card(
            "Bash",
            r#"{"command":"git clone git@github.com:org/repo.git"}"#,
            "feishu:ou_t",
            "r",
            None,
            300,
        );
        assert!(
            masked.matches("\"text_size\":\"notation\"").count() >= 2,
            "倒计时 + 掩码警告两条 note: {masked}"
        );
        assert!(
            masked.contains("邮箱已掩码显示") && masked.contains("\"text_size\":\"notation\""),
            "掩码警告 note 化: {masked}"
        );
        let cancelled = render_permission_card_cancelled("Bash");
        assert!(
            cancelled.contains("\"text_size\":\"notation\"") && cancelled.contains("审批超时"),
            "超时原因 note: {cancelled}"
        );
        // 问题卡 note 同步。
        let input = serde_json::json!({
            "questions": [{"question": "选？", "options": [{"label":"A"},{"label":"B"}]}]
        })
        .to_string();
        let q = render_question_card(&input, "feishu:ou_t", "r", None, 300).unwrap();
        assert!(q.contains("\"text_size\":\"notation\""), "问题卡 note: {q}");
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
        let json = render_card(&card_of(CardTerminal::Done, tools), "feishu:ou_t", None);
        // 真机校准（2026-08）：裸 tag 组件 200621 拒收（整卡降级纯文本）——
        // 统计改为 markdown+notation 小字行，且**整卡不得再出现** tag 组件。
        assert!(
            !json.contains("\"tag\":\"tag\""),
            "不得含 tag 组件（200621）: {json}"
        );
        assert!(
            json.contains("🔧 工具 3 次：Bash×2 · Read×1"),
            "统计行: {json}"
        );
        // Running 不加统计行（统计未收敛）。
        let running = render_card(
            &card_of(CardTerminal::Running, vec![tool("Bash", "a", false)]),
            "feishu:ou_t",
            None,
        );
        assert!(!running.contains("工具 1 次"), "Running 无统计: {running}");
        // markdown 统计行兜底仍在（managed 终态正文）。
        let md = stream_body_final(&body_card_of("结论", &[tool("Bash", "a", true)], &[]), None);
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
        // 真机校准（2026-08）：来源并入行首 markdown（💻 本机 / 📱 IM 文本），
        // 裸 tag 组件 200621 拒收已移除。
        assert!(
            !json.contains("\"tag\":\"tag\"") && json.contains("💻 本机") && json.contains("📱 IM"),
            "来源文本化且无 tag 组件: {json}"
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
        let md = stream_body_md(&body_card_of("", &many, &[]));
        assert!(md.contains("⋯ 前面还有 3 个工具"), "省略号图标: {md}");
        assert!(!md.contains("☕"), "不再用咖啡杯: {md}");
        let sup = render_permission_card_superseded("Bash");
        assert!(sup.contains("🔁 已被新询问取代"), "superseded 图标: {sup}");
        assert!(!sup.contains("⏭️"), "不再用跳过图标: {sup}");
        let perm = render_permission_card("Bash", r#"{"command":"ls"}"#, "c", "r", None, 300);
        // 真机校准（2026-08 第三轮）：♾️ 始终允许回归为独立整宽次级按钮
        //（用户反馈移除后功能入口消失）。
        assert!(perm.contains("♾️ 本次会话始终允许"), "♾️ 次级按钮: {perm}");
        assert!(!perm.contains("🔓"), "不再用开锁: {perm}");
    }

    /// ⑦ primary 按钮 emoji 精简：允许（primary）无 ✅（蓝底已高亮）；
    /// ⛔ 拒绝（danger）保留。
    #[test]
    fn primary_buttons_no_green_emoji() {
        let json = render_permission_card("Bash", r#"{"command":"ls"}"#, "c", "r", None, 300);
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
        let out = stream_body_final(&body_card_of("结论", &[tool("Bash", "a", true)], &[]), None);
        assert!(out.contains("\n---\n"), "正文与统计间分割线: {out}");
        assert!(out.contains("**工具轨迹**"), "明细块小标题: {out}");
        let json = render_card(
            &card_of(CardTerminal::Done, vec![tool("Bash", "a", true)]),
            "feishu:ou_t",
            None,
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
            None,
        );
        assert!(done.contains("\"color\":\"grey\""), "Done grey: {done}");
        let err = render_card(
            &card_of(
                CardTerminal::Error("boom".into()),
                vec![tool("B", "a", true)],
            ),
            "c",
            None,
        );
        assert!(err.contains("\"color\":\"red\""), "Error red: {err}");
        let running = render_card(
            &card_of(CardTerminal::Running, vec![tool("B", "a", false)]),
            "c",
            None,
        );
        assert!(
            running.contains("\"color\":\"blue\""),
            "Running blue: {running}"
        );
    }

    /// Wave B-5：群 conv 卡片的「发起者」标注行——群形态输出 <at> 标签行，
    /// 私聊/缺 sender 不加；初始卡（managed）与整卡（render_card）同款。
    #[test]
    fn sender_anchor_line_group_only() {
        // 群 + sender：at 标签行。
        let line = sender_anchor_line(Some("ou_owner"), true).expect("群应有标注行");
        let s = line.to_string();
        // JSON 序列化后引号被转义，断言拆开查（<at id= 与 open_id 值）。
        assert!(
            s.contains("<at id=") && s.contains("ou_owner"),
            "at 标签: {s}"
        );
        assert!(s.contains("发起的任务"), "文案: {s}");
        // 私聊 / 无 sender：不加。
        assert!(
            sender_anchor_line(Some("ou_owner"), false).is_none(),
            "私聊不加"
        );
        assert!(sender_anchor_line(None, true).is_none(), "缺 sender 不加");
        assert!(
            sender_anchor_line(Some(""), true).is_none(),
            "空 sender 不加"
        );
        // 初始卡（群）含标注行且在 md_body 之前；（私聊）不含。
        let init_group = render_stream_init_card("feishu:oc_g", Some("ou_owner"));
        assert!(
            init_group.contains("<at id=") && init_group.contains("ou_owner"),
            "{init_group}"
        );
        assert!(
            init_group.find("<at").unwrap() < init_group.find("md_body").unwrap(),
            "发起者行在正文组件之前"
        );
        let init_p2p = render_stream_init_card("feishu:ou_t", Some("ou_t"));
        assert!(!init_p2p.contains("<at"), "私聊初始卡不加: {init_p2p}");
        // 整卡路径（render_card）同款：群含、私聊不含。
        let card = OutboundCard {
            text: "x".into(),
            tool_calls: vec![],
            phase: CardPhase::Thinking,
            thoughts: Vec::new(),
            todos: Vec::new(),
            queued_hint: None,
            terminal: CardTerminal::Done,
            usage_display: None,
            run_secs: 10,
        };
        let g = render_card(&card, "feishu:oc_g", Some("ou_owner"));
        assert!(g.contains("<at id=") && g.contains("ou_owner"), "{g}");
        assert!(!render_card(&card, "feishu:ou_t", Some("ou_t")).contains("<at"));
    }

    /// Wave B-3：成功终态 footer 带总耗时——`✅ 已完成 · 30m · $0.012`；
    /// 无 usage 省成本段；时长格式化分档。
    #[test]
    fn terminal_done_footer_carries_run_len() {
        assert_eq!(
            terminal_done_footer(1800, Some("$0.012")),
            "✅ 已完成 · 30m · $0.012"
        );
        assert_eq!(terminal_done_footer(42, None), "✅ 已完成 · 42s");
        assert_eq!(terminal_done_footer(0, None), "✅ 已完成 · 0s");
        assert_eq!(format_run_len(750), "12m");
        assert_eq!(format_run_len(3661), "1h01m");
        assert_eq!(format_run_len(90000), "1d1h");
        // 整卡渲染带出该 footer。
        let card = OutboundCard {
            text: "x".into(),
            tool_calls: vec![],
            phase: CardPhase::Thinking,
            thoughts: Vec::new(),
            todos: Vec::new(),
            queued_hint: None,
            terminal: CardTerminal::Done,
            usage_display: Some("$0.5".into()),
            run_secs: 1800,
        };
        let json = render_card(&card, "feishu:ou_t", None);
        assert!(json.contains("✅ 已完成 · 30m · $0.5"), "footer: {json}");
    }

    /// Wave B-11：失败终态卡补「🩺 自检」按钮——value 编码 /doctor 命令（回调
    /// 走与手打相同的鉴权/分派）；成功/Running 终态不加。
    #[test]
    fn error_terminal_card_has_doctor_button() {
        let err = render_card(
            &card_of(CardTerminal::Error("boom".into()), vec![]),
            "feishu:ou_t",
            None,
        );
        assert!(err.contains("/doctor"), "失败卡带 /doctor 按钮: {err}");
        assert!(
            err.contains("\"imagent_cmd\":\"/doctor\"") || err.contains("imagent_cmd"),
            "命令按钮 value: {err}"
        );
        let done = render_card(&card_of(CardTerminal::Done, vec![]), "feishu:ou_t", None);
        assert!(!done.contains("/doctor"), "成功卡不带: {done}");
        let running = render_card(&card_of(CardTerminal::Running, vec![]), "feishu:ou_t", None);
        assert!(!running.contains("/doctor"), "Running 卡不带: {running}");
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
    /// 真机校准（2026-08-30）：卡片上限按**字节**计——24K 字符（~30KB）实测被
    /// 200860 拒。cap_md_bytes 字节制头尾窗口 + char 边界安全。
    #[test]
    fn cap_md_bytes_truncates_by_bytes() {
        let short = "你好".repeat(100); // 600 字节
        assert_eq!(cap_md_bytes(&short, 4_096, 4_096), short, "未超限原样返回");
        let long = "https://example.com/很长的路径".repeat(2000); // 远超 8KB
        let capped = cap_md_bytes(&long, 4_096, 4_096);
        assert!(capped.len() < long.len(), "必须截断");
        assert!(capped.contains("已截断中段"), "带截断标注");
        assert!(
            capped.len() < 9_500,
            "截后总长受字节预算约束: {}",
            capped.len()
        );
        // 头尾仍是原文的前缀/后缀（char 边界安全，无乱码）。
        let head_ok = long.starts_with(&capped[..capped.find('\n').unwrap_or(0)]);
        assert!(head_ok, "头部为原文前缀");
    }
}
