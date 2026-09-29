//! 飞书长连接事件 payload 的 serde 结构 + 纯函数解析。
//!
//! 飞书 `im.message.receive_v1` 事件经 `open-lark` 长连接以原始 payload bytes
//! 推出（见 `client.rs`）。本模块只做**裁剪到关心字段的反序列化 + 纯函数映射**，
//! 无网络、无副作用，是验收核心（见 `mod tests`）。未知字段一律忽略（serde 默认）。
//!
//! 约定：
//! - conv_id = `feishu:<receive_id>`：p2p → `<open_id>`（`ou_` 前缀），
//!   group → `<chat_id>`（`oc_` 前缀）。发消息时反向 strip `feishu:` 还原。
//! - 鉴权（白名单）由 core 做，本模块只透传 sender 的 `open_id`。

use serde::Deserialize;

use imagent_core::{ConvId, InboundMessage, ReplyHint, UserId};

/// dedup 回退 key 用的内容稳定哈希（DefaultHasher，非加密强度——仅去重用途）：
/// 相同内容恒同值（跨重投可去重），不同内容不同值（等长内容不碰撞）。
fn content_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// `im.message.receive_v1` 事件顶层结构（裁剪：仅保留 header + event）。
#[derive(Debug, Deserialize)]
pub struct FeishuEvent {
    pub header: EventHeader,
    pub event: EventBody,
}

#[derive(Debug, Deserialize)]
pub struct EventHeader {
    pub event_type: String,
    /// 去重 key 首选（飞书事件 id）。
    #[serde(default)]
    pub event_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EventBody {
    pub sender: Sender,
    pub message: Message,
    /// 群消息附带；私聊可能缺省。
    #[serde(default)]
    pub chat: Option<Chat>,
}

#[derive(Debug, Deserialize)]
pub struct Sender {
    pub sender_id: SenderId,
}

/// 飞书用户标识三件套（union_id / user_id / open_id），鉴权用稳定的 open_id。
#[derive(Debug, Deserialize)]
pub struct SenderId {
    pub open_id: String,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub message_type: String,
    /// JSON 字符串，如 `{"text":"hi"}`（飞书把 content 序列化成字符串塞进事件）。
    pub content: String,
    /// `p2p`（私聊）/ `group`（群聊）。
    pub chat_type: String,
    #[serde(default)]
    pub chat_id: Option<String>,
    /// 去重 key 备选。
    #[serde(default)]
    pub message_id: Option<String>,
    /// 消息内 @ 提及列表（P6-1：正文占位 `@_user_N` 的元数据）。
    #[serde(default)]
    pub mentions: Vec<MessageMention>,
    /// 话题群（thread）消息所属话题的根消息 id（P6-4：仅话题群返回；普通群回复
    /// 只有 parent_id 不设 root_id）。
    #[serde(default)]
    pub root_id: Option<String>,
    /// 引用回复的目标消息 id（多 pending 路由锚点：命中询问卡消息 id 时，回复
    /// 路由到该询问的 request_id）。普通消息为 None。
    #[serde(default)]
    pub parent_id: Option<String>,
}

/// 群/私聊消息里的 @ 提及（`im.message.receive_v1` 的 message.mentions 元素）。
/// 兼容平铺形态：部分载荷把 open_id 直接放提及对象上（同评论事件的宽容姿态）。
#[derive(Debug, Deserialize)]
pub struct MessageMention {
    /// 正文占位 key，如 `@_user_1`（与 content.text 中的占位一一对应）。
    #[serde(default)]
    pub key: Option<String>,
    /// 被 @ 者标识（嵌套形态）。
    #[serde(default)]
    pub id: Option<MentionId>,
    /// 显示名（客户端渲染的 @ 名字）。
    #[serde(default)]
    pub name: Option<String>,
    /// 平铺形态的 open_id。
    #[serde(default)]
    pub open_id: Option<String>,
    /// 平铺形态的 user_id（历史字段名，评论事件同款宽容）。
    #[serde(default)]
    pub user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MentionId {
    #[serde(default)]
    pub open_id: Option<String>,
}

impl MessageMention {
    /// 被 @ 者的 open_id（嵌套优先，平铺回退）。
    pub fn open_id(&self) -> Option<&str> {
        self.id
            .as_ref()
            .and_then(|i| i.open_id.as_deref())
            .or(self.open_id.as_deref())
            .or(self.user_id.as_deref())
            .filter(|s| !s.is_empty())
    }
}

/// v1.18 回复即定向预检：群消息（回复形态）的 parent_id。parent 命中 bot
/// 近期消息（client::bot_sent_recently）时事件循环放宽 require_mention——
/// 群里纯图片/文件无法携带 @（手机端无富文本合成路径），回复 bot 的消息本身
/// 即显式定向。仅群（chat_type=group）且 parent 非空返回 Some。
/// v1.25 引用上下文：被引用消息正文 → 纯文本（text 取 JSON text 字段；
/// post 复用 parse_post 的文本提取）。空/不支持类型返回 None。
pub fn quoted_context_text(msg_type: &str, content: &str) -> Option<String> {
    let text = match msg_type {
        "text" => serde_json::from_str::<serde_json::Value>(content)
            .ok()?
            .get("text")?
            .as_str()?
            .trim()
            .to_string(),
        "post" => parse_post(content, None)?.0?.trim().to_string(),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// 引用的 interactive 卡片 → 文本转录。v1.27.0 修复：bot 自身回复即卡片，
/// 此前引用只剩 `[interactive]` 占位、追问整链失效。
///
/// 递归收集卡片 JSON 中带文本的组件（`markdown`/`plain_text`/`lark_md` 的
/// `content`），容器组件（form/column_set/collapsible_panel/未知 tag）自动
/// 下钻——按钮文案、下拉/多选选项、面板正文随之收齐，第三方 schema 1.0
/// 卡（div/note/action）同形态覆盖。模板卡（`type=template`，正文不在
/// content 里）与抽不到任何文本的结构返回 None，由调用方回退占位。
/// 只走 header.title 与 elements 树，config（summary 等元信息）不入正文。
pub fn card_text_transcript(content: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(content).ok()?;
    if v.get("type").and_then(|t| t.as_str()) == Some("template") {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    collect_card_text(v.pointer("/header/title"), &mut parts, 0);
    for root in ["/body/elements", "/elements"] {
        collect_card_text(v.pointer(root), &mut parts, 0);
    }
    let text = parts
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

/// 卡片文本 walker 的显式递归深度上限（P3，v13 遗留 fuzz 缺口收口）。
/// 此前栈安全**隐式**依赖 serde_json 解析期的 128 层嵌套上限——那是解析器的
/// 实现细节（换迭代式解析/调整上限即失效，且 walker 每层的栈帧开销高于解析
/// 器单层）。显式 64 层上限作双保险：合法 CardKit 卡片组件层级是个位数，
/// 超深子树按恶意/畸形输入处理——返回空 + debug 留痕，不栈溢出。
const CARD_TEXT_MAX_DEPTH: usize = 64;

/// 卡片节点 → 文本片段（`card_text_transcript` 的递归体）：文本组件取
/// content；其余对象/数组全部下钻——容器与未来新增组件不丢其中文本，
/// 无 tag 的字符串叶子（value 等业务载荷）天然不收。
/// `depth` 为显式递归深度（入口传 0），超 [`CARD_TEXT_MAX_DEPTH`] 截断该子树
/// （双保险理由见常量注释）。
fn collect_card_text(node: Option<&serde_json::Value>, out: &mut Vec<String>, depth: usize) {
    let Some(node) = node else { return };
    if depth >= CARD_TEXT_MAX_DEPTH {
        tracing::debug!(
            target: "feishu",
            depth,
            "card_text_transcript 子树深度超上限（{}），截断",
            CARD_TEXT_MAX_DEPTH
        );
        return;
    }
    match node {
        serde_json::Value::Object(map) => {
            match map.get("tag").and_then(|t| t.as_str()).unwrap_or("") {
                "markdown" | "plain_text" | "lark_md" => {
                    if let Some(c) = map.get("content").and_then(|c| c.as_str()) {
                        out.push(c.trim().to_string());
                    }
                }
                _ => {
                    for v in map.values() {
                        collect_card_text(Some(v), out, depth + 1);
                    }
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_card_text(Some(v), out, depth + 1);
            }
        }
        _ => {}
    }
}

/// v1.25.2：不限会话形态的 parent_id 提取（引用上下文用——私聊引用此前被
/// 群版 peek 跳过）。群「回复即定向」仍用 [`peek_group_reply_parent`]。
pub fn peek_reply_parent(payload: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct P {
        event: E,
    }
    #[derive(serde::Deserialize)]
    struct E {
        message: M,
    }
    #[derive(serde::Deserialize)]
    struct M {
        #[serde(default)]
        parent_id: Option<String>,
    }
    let p: P = serde_json::from_slice(payload).ok()?;
    p.event
        .message
        .parent_id
        .filter(|s| !s.is_empty())
        .filter(|s| s.starts_with("om_"))
}

/// T19 起 drain 分派改用 Value 导航（platform/drain.rs 的同名 helper），字节版
/// 保留作等价性钉子（drain 测试 value_helpers_match_proto_predicates 对照）。
pub fn peek_group_reply_parent(payload: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct P {
        event: E,
    }
    #[derive(serde::Deserialize)]
    struct E {
        message: M,
    }
    #[derive(serde::Deserialize)]
    struct M {
        chat_type: String,
        #[serde(default)]
        parent_id: Option<String>,
    }
    let p: P = serde_json::from_slice(payload).ok()?;
    (p.event.message.chat_type == "group")
        .then(|| p.event.message.parent_id.filter(|s| !s.is_empty()))
        .flatten()
}

/// mention 处理策略（P6-1）：由 platform 层注入 config，纯函数可测。
#[derive(Debug, Clone, Copy)]
pub struct MentionPolicy {
    /// 群消息必须 @bot 才处理（`feishu_require_mention_in_group`，默认 true）。
    /// p2p 不受限。bot id 未知时退化为「mentions 非空」弱过滤（同评论 P5-8）。
    pub require_mention_in_group: bool,
}

impl MentionPolicy {
    /// 全收（历史行为：过滤完全依赖事件订阅 scope）。
    pub const PERMISSIVE: Self = Self {
        require_mention_in_group: false,
    };
    /// 群消息须 @bot（config 默认）。
    pub const REQUIRE_BOT: Self = Self {
        require_mention_in_group: true,
    };
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub chat_id: String,
}

/// text 类型消息的 content 结构：`{"text":"..."}`。
#[derive(Debug, Deserialize)]
pub struct TextContent {
    pub text: String,
}

/// image 类型消息的 content 结构：`{"image_key":"..."}`。
#[derive(Debug, Deserialize)]
pub struct ImageContent {
    pub image_key: String,
}

/// file 类型消息的 content 结构：`{"file_key":"...","file_name":"..."}`。
/// file_name 为发送端原始文件名（含扩展名）；缺省为 None（旧客户端/字段缺失）。
#[derive(Debug, Deserialize)]
pub struct FileContent {
    pub file_key: String,
    #[serde(default)]
    pub file_name: Option<String>,
}
/// post 富文本消息的 content 结构：`{"title","content":[[节点...]]}`。
/// content 是行×列二维数组；未知字段（content_v2 等）由 serde 默认忽略。
#[derive(Debug, Deserialize)]
struct PostContent {
    #[serde(default)]
    title: String,
    #[serde(default)]
    content: Vec<Vec<PostNode>>,
}

/// post 富文本节点（裁剪：只取关心的 tag/字段，未知字段忽略）。
#[derive(Debug, Deserialize)]
struct PostNode {
    tag: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    image_key: Option<String>,
    /// a 节点（超链接）：目标地址。缺失/空时退化为纯文本渲染（见 parse_post）。
    #[serde(default)]
    href: Option<String>,
    /// at 节点：被 @ 者 open_id（字段名历史遗留 user_id，同评论事件）。
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    user_name: Option<String>,
}

/// 待下载的入站媒体（proto 只解析出 key，实际下载落盘在 platform 层）。
#[derive(Debug, Clone)]
pub struct PendingMedia {
    /// `"image"` | `"file"`，直接对应 `MediaRef.kind`。
    pub kind: &'static str,
    /// image_key 或 file_key（飞书下载资源标识，全局唯一）。
    pub key: String,
    /// 所属消息 id。下载「用户发来的」资源必须走 message-resource 接口，飞书要求 message_id。
    pub message_id: String,
    /// 发送端原始文件名（file 消息 content.file_name；图片消息 content 无该字段，
    /// post 图片节点亦无——均为 None）。落盘用原名+原扩展名（见 platform persist_media）。
    pub file_name: Option<String>,
}

/// 发消息时的 receive_id 类型（决定 OpenAPI `receive_id_type` 参数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveIdKind {
    /// `ou_` 前缀：用户 open_id（私聊）。
    OpenId,
    /// `oc_` 前缀：群 chat_id（群聊）。
    ChatId,
}

// ---------------------------------------------------------------------------
// card.action.trigger（P4-4 审批按钮回调）
// ---------------------------------------------------------------------------

/// `card.action.trigger` 事件（CardKit 2.0 按钮点击回调，schema 2.0 信封）。
/// 只裁剪关心的字段；`action.value` 是按钮 behaviors callback 里带的任意 JSON。
#[derive(Debug, Deserialize)]
pub struct CardActionEvent {
    pub header: EventHeader,
    pub event: CardActionBody,
}

#[derive(Debug, Deserialize)]
pub struct CardActionBody {
    /// 点击者（operator）。
    pub operator: CardOperator,
    /// 按钮 callback 带回的 value（我们编码了 conv 与动作）。
    pub action: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct CardOperator {
    /// 旧信封：operator_id 嵌套。
    #[serde(default)]
    pub operator_id: Option<CardOperatorId>,
    /// 真机校准（2026-08）：新回调信封把 open_id 平铺在 operator 上
    /// （`operator.open_id`），不再经 operator_id 嵌套。两形态兼容。
    #[serde(default)]
    pub open_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CardOperatorId {
    #[serde(default)]
    pub open_id: Option<String>,
}

/// 命令按钮（imagent_cmd）value 的有效期窗口：卡片长期滞留在 IM 里，用户数日后
/// 点「使用 xxx」之类按钮会以过期上下文执行命令（如 /ws use 指向已删除的工作
/// 空间）。24h 覆盖正常使用节奏（当天发的卡当天/次日点完），过期明确回提示。
pub const CMD_BUTTON_TTL_SECS: i64 = 24 * 3600;

/// conv 是否为私聊形态（`feishu:ou_…`）。v13-P2 起按钮 value 的发起者校验已
/// **全形态**（卡片可被转发，点击者身份与 conv 形态无关）；本函数仍用于：
/// 卡片「发起者」标注行仅群 conv 添加（card::sender_anchor_line）、审批
/// pending_asks 兜底校验（platform drain，存量无 sender 卡）。评论 conv 等
/// 多方可见形态一律按群处理。
pub fn is_private_conv(conv: &str) -> bool {
    conv.strip_prefix("feishu:").is_some_and(|rest| {
        let id = rest.split(':').next().unwrap_or(rest);
        id.starts_with("ou_")
    })
}

/// 解析按钮卡片回调（card.action.trigger），两类 value（P6-3 扩展）：
/// - 审批按钮：`{"imagent_perm":"allow|always|deny","conv":"feishu:…"}` → `text = "y"/"always"/"n"`，
///   core 的 recv 循环把 pending conv 的非斜杠消息当审批回复路由（`parse_reply`）；
/// - 命令按钮：`{"imagent_cmd":"/ws use main","conv":"feishu:…"}` → `text = <command>`，
///   走与手打命令完全相同的鉴权/分派路径（admin 门槛等不豁免）。
///
/// 返回 `(dedup_key, 入站消息, deny 提示)`：`deny = Some(文案)` 表示该点击被
/// 安全策略拒绝（过期按钮 / 非发起者点终止或命令按钮——全形态校验）——**不**
/// 产生入站消息（msg 为占位空壳，调用方只回 deny 文案），防过期/越权命令进
/// core 分派。
///
/// 非 imagent 按钮 / 缺 conv / conv 无 `feishu:` 前缀（伪造防） / 缺 open_id /
/// 命令非 `/` 开头（防伪造非命令文本）返回 None。
///
/// 审批按钮的**发起者校验**不在此处（需 pending_asks 状态，见 platform drain）；
/// 终止/命令按钮的发起者由 value.sender 自带（card 渲染时编码），此处即可校验。
pub fn parse_card_action_event(payload: &[u8]) -> Option<(String, InboundMessage, Option<String>)> {
    let evt: CardActionEvent = serde_json::from_slice(payload).ok()?;
    if evt.header.event_type != "card.action.trigger" {
        return None;
    }
    // 真机校准：新信封 action 平铺（value 直接是 action 的字段），旧信封嵌套在
    // action.value 下——两形态都认。
    let value = evt.event.action.get("value").unwrap_or(&evt.event.action);
    let conv = value.get("conv")?.as_str()?;
    // 前缀校验：conv 必须是本平台的 `feishu:` 形态——伪造/跨平台串号的 value
    // 不应被路由进飞书会话。
    if !conv.starts_with("feishu:") {
        return None;
    }
    // 多 pending：value 可携带 req（request_id）——按钮回调精确路由到发起方。
    let ask_req = value
        .get("req")
        .and_then(|r| r.as_str())
        .filter(|r| !r.is_empty())
        .map(String::from);
    // P6：问题卡选项按钮（imagent_ask）→ "ask:<选项>" 文本，走审批回复路由由
    // parse_reply 转成 deny+message（用户选择经 message 回给 agent）。
    // P6-3：命令按钮（imagent_cmd）→ 命令本体，走与手打命令相同的鉴权/分派
    //（admin 门槛等不豁免；只接受 / 开头，防伪造普通聊天文本）。
    let act = value.get("imagent_perm").and_then(|v| v.as_str());
    let cmd = value.get("imagent_cmd").and_then(|v| v.as_str());
    let ask_choice = value.get("imagent_ask").and_then(|v| v.as_str());
    let form_kind = value.get("imagent_form").and_then(|v| v.as_str());
    // 命令/审批按钮过期校验（card 渲染时编码 ts=epoch 秒）：超窗拒绝执行并回可读
    // 提示；无 ts 的旧卡兼容放行（升级前发出的存量卡，点击者本就过了鉴权）。
    let ts = value.get("ts").and_then(|v| v.as_i64());
    if let Some(ts) = ts {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(i64::MAX);
        // 卡片 UX 批（v1.24）：value 可携带自定义 ttl（安全命令 7 天，见 card
        // 的 is_safe_long_ttl_command）；缺省仍 24h。
        let ttl = value
            .get("ttl")
            .and_then(|v| v.as_i64())
            .filter(|t| *t > 0)
            .unwrap_or(CMD_BUTTON_TTL_SECS);
        if now - ts > ttl {
            let human = if ttl > 48 * 3600 {
                format!("{} 天", ttl / (24 * 3600))
            } else {
                format!("{} 小时", ttl / 3600)
            };
            return Some((
                dummy_card_action_key(&evt, conv),
                dummy_card_action_msg(&evt, conv),
                Some(format!("⏳ 该按钮已过期（超过 {human}），请重新发起。")),
            ));
        }
    }
    // 审批/问题/表单按钮发起者校验（安全：卡片转发代批）：value.sender 为询问
    // 发起者的编码，**全形态**校验 operator==sender——私聊 conv 也不豁免（卡片
    // 可被转发到任意会话，点击者身份与 conv 形态无关；v13-P2 起命令按钮同款
    // 全形态，见下方）。不符回明确提示，不注入 y/n/ask 文本。无 sender 的存量
    // 卡兼容放行（pending_asks 侧的群形态校验仍兜底）。
    let ask_like = act.is_some() || ask_choice.is_some() || form_kind.is_some();
    if ask_like {
        if let Some(owner) = value.get("sender").and_then(|v| v.as_str()) {
            let open_id = card_operator_open_id(&evt)?;
            if open_id != owner {
                return Some((
                    dummy_card_action_key(&evt, conv),
                    dummy_card_action_msg(&evt, conv),
                    Some(format!("⛔ 该询问由 {owner} 发起，仅其本人可答复。")),
                ));
            }
        }
    }
    // 命令按钮发起者校验（终止按钮等）：value.sender 为发起轮次用户的编码，
    // **全形态**校验 operator==sender——与上方询问类同理由（v13-P2 对齐）：
    // 卡片可被转发到任意会话，点击者身份与 conv 形态无关（私聊卡被转发给
    // 白名单用户 B 后，B 点击即在编码的私聊 conv 执行 /stop /new /again）。
    // 无 sender 的存量卡兼容放行（与询问类同款）。
    if cmd.is_some() {
        if let Some(owner) = value.get("sender").and_then(|v| v.as_str()) {
            let open_id = card_operator_open_id(&evt)?;
            if open_id != owner {
                return Some((
                    dummy_card_action_key(&evt, conv),
                    dummy_card_action_msg(&evt, conv),
                    Some("⛔ 该任务由他人发起，仅发起者本人可执行此操作。".to_string()),
                ));
            }
        }
    }
    // P9-2：表单提交按钮（imagent_form）——CardKit form 的用户输入值**不在**
    // action.value 里，在 action.form_value（lcab dispatcher 同款校准）。两类表单：
    // - "config"：把 (key, string value) 拼成 `/config form k=v k=v` 命令文本，
    //   走与手打命令相同的鉴权（admin 门槛）/分派；
    // - "ask"：问题卡表单（>4 选项下拉 / 多选 checkbox / **多题一次提交**）——
    //   form_value.ask_opt（单题兼容：单值下拉直通 / 数组多选「、」拼接）与
    //   form_value.ask_opt_{i}（多题：value=「题头=选项」，题间「；」拼接），
    //   回成 `ask:<选择>` 走与选项按钮相同的审批回复路由（req 编码在 value）。
    let text: String = if form_kind == Some("ask") {
        let fv = evt.event.action.get("form_value")?;
        let field_joined = |v: &serde_json::Value| -> Option<String> {
            let joined = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Array(a) => a
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join("、"),
                _ => return None,
            };
            (!joined.is_empty()).then_some(joined)
        };
        // 多题字段按题号归并：ask_opt_{i}（选项值）与 ask_opt_{i}_free（自由
        // 输入）**都是**该题的键——题号要从两类键一起收集，否则「只用自由输入
        // 作答的题」（选项键缺席 form_value）整题被漏掉（v1.17.3 真机首测即此
        // 形态：两题选项 + 两题自由输入，只回传了选项两题）。单题旧字段 ask_opt 兜底。
        let mut parts: Vec<String> = Vec::new();
        if let Some(obj) = fv.as_object() {
            let mut idxes: Vec<u32> = obj
                .keys()
                .filter_map(|k| {
                    k.strip_prefix("ask_opt_")
                        .and_then(|s| s.strip_suffix("_free").unwrap_or(s).parse::<u32>().ok())
                })
                .collect();
            idxes.sort_unstable();
            idxes.dedup();
            for i in idxes {
                // 自由输入非空则优先于选项（对齐 CLI 原生自定义回答；原文进入
                // 用户选择消息，agent 自行对应到题）。形态已校准（2026-09-03
                // 真机载荷）：值为字符串，未填回空串——空串忽略即回落选项。
                let free = obj
                    .get(&format!("ask_opt_{i}_free"))
                    .and_then(field_joined)
                    .map(|s| s.trim().to_string())
                    .filter(|t| !t.is_empty());
                if let Some(f) = free {
                    parts.push(f);
                    continue;
                }
                if let Some(p) = obj.get(&format!("ask_opt_{i}")).and_then(field_joined) {
                    parts.push(p);
                }
            }
        }
        if parts.is_empty() {
            let joined = fv.get("ask_opt").and_then(field_joined)?;
            parts.push(joined);
        }
        format!("ask:{}", parts.join("；"))
    } else if form_kind.is_some() {
        let fv = evt
            .event
            .action
            .get("form_value")
            .and_then(|v| v.as_object())?;
        let mut pairs: Vec<String> = Vec::new();
        // 键白名单校验（防伪造任意配置键——cmd_config 侧还会再验一次值）。
        for k in ["reply_mode", "cot_detail", "require_mention"] {
            if let Some(v) = fv.get(k).and_then(|v| v.as_str()) {
                pairs.push(format!("{k}={v}"));
            }
        }
        if pairs.is_empty() {
            return None;
        }
        format!("/config form {}", pairs.join(" "))
    } else if let Some(choice) = ask_choice {
        format!("ask:{choice}")
    } else {
        match (act, cmd) {
            (Some("allow"), _) => "y".to_string(),
            // D-记忆：始终允许（本会话内此工具后续审批直接放行）——core 的
            // parse_reply 命中 ALWAYS_WORDS，router 把 pending 的工具加入
            // 该 conv 的会话级 allow-set。
            (Some("always"), _) => "always".to_string(),
            (Some("deny"), _) => "n".to_string(),
            (_, Some(c)) if c.starts_with('/') => c.to_string(),
            _ => return None,
        }
    };
    let open_id = card_operator_open_id(&evt)?;
    // P3：缺 event_id 的回退 key 用 content_hash 对完整内容取哈希——与消息/
    // 评论回退同语义（S4 口径）。此前用 text 前 40 字符：>40 字符的不同文本
    // 前缀相同会被互相去重（按钮回调/长命令文本可超 40 字符）。
    let key = evt
        .header
        .event_id
        .clone()
        .unwrap_or_else(|| format!("card_action:{open_id}:{conv}:{:x}", content_hash(&text)));
    Some((
        key,
        InboundMessage {
            conv_id: ConvId(conv.to_string()),
            sender: UserId(open_id),
            sender_name: None,
            text: Some(text),
            media: vec![],
            media_errors: Vec::new(),
            mentions: Vec::new(),
            mentioned_bot: false,
            ask_req,
            reply_to: None,
            source_msg_id: None,
            control: None,
            no_steer: false,
            reply_hint: ReplyHint::None,
        },
        None,
    ))
}

/// 回调操作者 open_id（新旧信封两形态兼容）。
fn card_operator_open_id(evt: &CardActionEvent) -> Option<String> {
    operator_open_id(&evt.event.operator)
}

/// operator 结构 → open_id（card.action 与 menu 事件共用：平铺 open_id 优先，
/// 嵌套 operator_id 回退）。
fn operator_open_id(op: &CardOperator) -> Option<String> {
    op.open_id
        .clone()
        .or_else(|| op.operator_id.as_ref().and_then(|o| o.open_id.clone()))
        .filter(|s| !s.is_empty())
}

/// deny 路径的占位 dedup key / 占位消息（调用方只回 deny 文案，不消费 msg）。
/// 占位消息的 sender 填**操作者 open_id**——drain 侧据此给操作者补一条私聊
/// 反馈（deny 文案回原 conv 之外的第二触达，防转发代批场景下原 conv 无人知晓）。
fn dummy_card_action_key(evt: &CardActionEvent, conv: &str) -> String {
    evt.header.event_id.clone().unwrap_or_else(|| {
        format!(
            "card_action:deny:{conv}:{:x}",
            content_hash(&evt.event.action.to_string())
        )
    })
}

fn dummy_card_action_msg(evt: &CardActionEvent, conv: &str) -> InboundMessage {
    InboundMessage {
        conv_id: ConvId(conv.to_string()),
        sender: UserId(card_operator_open_id(evt).unwrap_or_default()),
        sender_name: None,
        text: None,
        media: vec![],
        media_errors: Vec::new(),
        mentions: Vec::new(),
        mentioned_bot: false,
        ask_req: None,
        reply_to: None,
        source_msg_id: None,
        control: None,
        no_steer: false,
        reply_hint: ReplyHint::None,
    }
}

// ---------------------------------------------------------------------------
// drive.file.comment.created_v1（P4-9 云文档评论触发）
// ---------------------------------------------------------------------------

/// 云文档评论创建事件（schema 2.0 信封；需在飞书后台订阅该事件 + `drive:comment`
/// 相关权限）。裁剪到关心字段；`content` 是「评论内容实体」数组（text/at/img 等）。
#[derive(Debug, Deserialize)]
pub struct CommentEvent {
    pub header: EventHeader,
    pub event: CommentBody,
}

#[derive(Debug, Deserialize)]
pub struct CommentBody {
    #[serde(default)]
    pub comment_id: String,
    #[serde(default)]
    pub file_token: String,
    /// 评论内容实体数组：`{"type":"text","text":"…"}` / at / img 等（未知 type 忽略）。
    #[serde(default)]
    pub content: Vec<CommentContentNode>,
    /// T13：划词评论引用的文档原文片段。字段名对齐官方评论实体——file-comment
    /// list API 的 `quote`（「局部评论的引用字段」，全文评论 `is_whole=true` 无此
    /// 值）；`quoted_text` 作形态漂移别名。事件侧字段**未经真机抓包确认**，
    /// fail-soft：缺失/空白不注入（见 [`render_comment_quote_block`]），fixture
    /// 单测钉住假设形态（tests/fixtures/comment_docx_*.json）。
    #[serde(default, alias = "quoted_text")]
    pub quote: Option<String>,
    #[serde(default)]
    pub sender: Option<Sender>,
}

#[derive(Debug, Deserialize)]
pub struct CommentContentNode {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
    /// at 节点：被 @ 的用户 id（字段名历史遗留为 `user_id`，值为 open_id）。
    #[serde(default)]
    pub user_id: Option<String>,
    /// 兼容部分载荷把被 @ 者放 `open_id` 字段。
    #[serde(default)]
    pub open_id: Option<String>,
}

/// 评论线程的 conv_id 前缀：`feishu:comment:<file_token>`（会话锚 = 文档；具体
/// 回复目标 comment_id 由消息元数据 ReplyHint::Anchor 携带，存量
/// `<file>:<comment>` 内嵌形态亦兼容，见 [`comment_target_from_conv`]）。
/// send_text 据此走「回复评论」API；同一文档的评论共享一个会话线程。
pub const COMMENT_CONV_PREFIX: &str = "feishu:comment:";

/// 廉价判定 payload 是否为云文档评论事件（drain 据此懒取 bot open_id，避免对
/// 无关事件也发起取 bot 信息的 HTTP 请求）。
pub fn is_comment_event(payload: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| {
            v.get("header")?
                .get("event_type")?
                .as_str()
                .map(|t| t == "drive.file.comment.created_v1")
        })
        .unwrap_or(false)
}

/// T13：评论引用片段注入块的截断上限（字符）——对齐引用上下文/群上下文的
/// 截断档（500 字），防超长划词挤占本轮 prompt 预算。
pub const COMMENT_QUOTE_MAX_CHARS: usize = 500;

/// T13：评论引用片段 → prompt 前置注入块（纯函数，验收核心）：
///
/// ```text
/// 【评论所在文档片段】
/// {quote 截 500 字}
///
/// {用户评论正文}
/// ```
///
/// 对齐仓内注入风格（`【…】` 头 + 空行分隔正文，见 [`render_group_context_block`]）。
/// quote 缺失/纯空白（全文评论、字段形态漂移）返回 None——fail-soft，不注入
/// 不影响主链路。
pub fn render_comment_quote_block(quote: &str) -> Option<String> {
    let trimmed = quote.trim();
    if trimmed.is_empty() {
        return None;
    }
    let body: String = trimmed.chars().take(COMMENT_QUOTE_MAX_CHARS).collect();
    Some(format!("【评论所在文档片段】\n{body}"))
}

/// 廉价判定 payload 是否为**群聊**消息事件（P6-1：drain 据此懒取 bot open_id——
/// 群消息的 @bot 过滤与 @bot 文本剥离需要；p2p 无 @bot 语义，无需 bot id）。
pub fn is_group_message_event(payload: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| {
            let et = v.get("header")?.get("event_type")?.as_str()?;
            let ct = v.get("event")?.get("message")?.get("chat_type")?.as_str()?;
            Some(et == "im.message.receive_v1" && ct == "group")
        })
        .unwrap_or(false)
}

/// 话题群近期活跃免 @（P 交互流）：从 payload 廉价提取话题 conv 键
/// `feishu:<chat_id>:<root_id>`（group + om_ 前缀 root_id 才算话题，与
/// parse_message_event 的 conv 升级口径一致）。drain 据此查活跃窗口：近
/// THREAD_ACTIVE_WINDOW 内该话题有过消息则豁免 require_mention——追问场景
/// （刚 @ 过 bot、接着追问）免于每条都 @。普通群（无话题 root）不命中，
/// 不豁免。
pub fn thread_key_of_payload(payload: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(payload).ok()?;
    if v.get("header")?.get("event_type")?.as_str()? != "im.message.receive_v1" {
        return None;
    }
    let msg = v.get("event")?.get("message")?;
    if msg.get("chat_type")?.as_str()? != "group" {
        return None;
    }
    let chat_id = msg
        .get("chat_id")
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())?;
    let root = msg
        .get("root_id")
        .and_then(|r| r.as_str())
        .filter(|r| r.starts_with("om_"))?;
    Some(format!("feishu:{chat_id}:{root}"))
}

/// 暂不支持的消息类型 → 用户可读提示（语音/分享卡片等，parse_message_event
/// 对这些类型返回 None 静默丢弃，本函数给出替代提示）。返回 `(提示文案, 去重键,
/// conv)`：
/// - 去重键 = message_id——drain 侧必须过 Dedup（事件重投/断线重连会重复触发，
///   v1.18 review 前该分支是唯一不经 dedup 的回执路径）；
/// - conv 仅 p2p 给出（群内「带 @」弱门槛会让**非白名单群**里的贴纸/分享触发
///   bot 回复——既是垃圾消息面也是 bot 存活性探针；白名单校验在 core 侧，
///   平台层无从判断，群内一律不提示）。
///
/// merged_forward（合并转发）已完整支持（drain 层拉子消息转录，见
/// [`parse_merged_forward_event`]），此处保留仅作**回退兜底**：事件缺 message_id
/// （无法拉子消息）时 parse_merged_forward_event 返回 None，走到这里给可行动
/// 提示（「解析失败回退现状提示」）。
pub fn unsupported_message_notice(
    payload: &[u8],
) -> Option<(&'static str, Option<String>, Option<ConvId>)> {
    let evt: FeishuEvent = serde_json::from_slice(payload).ok()?;
    if evt.header.event_type != "im.message.receive_v1" {
        return None;
    }
    let notice = match evt.event.message.message_type.as_str() {
        "share_chat" => "🗂 暂不支持群聊分享卡片，请直接发送文字。",
        "share_user" => "👤 暂不支持用户名片分享，请直接发送文字。",
        // 合并转发（仅回退兜底，正常路径见 parse_merged_forward_event）/
        // 表情包 / 视频（media=视频流、video=旧字段）：parse 侧静默丢弃，
        // 给可行动提示——用户改发文字或截图即可继续（截图走 image 路径可处理）。
        "merged_forward" | "sticker" | "media" | "video" => {
            "📦 暂不支持合并转发/表情包/视频消息，请直接发文字或截图。"
        }
        _ => return None,
    };
    // 仅 p2p 回提示（理由见函数文档）。去重键：message_id，缺省回退 header
    // event_id（重投事件二者都保持原值，dedup 仍有效）；两者皆缺不发（无法
    // 去重的回执在事件重投下会刷屏）。
    if evt.event.message.chat_type != "p2p" {
        return None;
    }
    let oid = evt.event.sender.sender_id.open_id;
    if oid.is_empty() {
        return None;
    }
    let key = evt
        .event
        .message
        .message_id
        .clone()
        .filter(|m| !m.is_empty())
        .or_else(|| evt.header.event_id.clone().filter(|e| !e.is_empty()));
    Some((notice, key, Some(ConvId(format!("feishu:{oid}")))))
}

/// W3-1：audio 消息 content `{"file_key":"…","duration":…}` → file_key。
pub fn extract_audio_key(content: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(content).ok()?;
    v.get("file_key")
        .and_then(|k| k.as_str())
        .filter(|k| !k.is_empty())
        .map(str::to_string)
}

/// W3-2：表情回应事件（`im.message.reaction.created_v1`）→ 快速审批回复。
///
/// 用户在**审批卡**上回应 👍（y）/ 👎（n）≈ 点允许/拒绝按钮——比点开卡片按
/// 按钮快得多的轻交互。payload 形态按**真机实测**（2026-08 校准：
/// `event.user_id.open_id` + `event.reaction_type.emoji_type` +
/// `event.message_id` 顶层；旧文档形态作回退），缺任一字段返回 None
/// （普通消息上的 emoji 回应不产生任何副作用）。
///
/// 仅映射两个保守 emoji；返回 `(dedup_key, 操作者 open_id, 被回应的消息 id, "y"/"n")`。
pub fn parse_reaction_event(payload: &[u8]) -> Option<(String, String, String, &'static str)> {
    // 弱解析（不依赖完整事件结构）：顶层 header 取 event_type/event_id，event
    // 子树里按字段名定位（reaction 可能内嵌 operator 之外的字段名差异）。
    let v: serde_json::Value = serde_json::from_slice(payload).ok()?;
    if v.get("header")
        .and_then(|h| h.get("event_type"))
        .and_then(|t| t.as_str())
        != Some("im.message.reaction.created_v1")
    {
        return None;
    }
    let event = v.get("event")?;
    // 真机校准（2026-08）：真实 payload 为 event.user_id.open_id /
    // event.reaction_type.emoji_type / event.message_id（顶层）。文档旧形态
    //（operator_id / reaction.emoji_key）作回退兼容。
    let operator = ["user_id", "operator_id"]
        .iter()
        .find_map(|k| {
            event
                .get(k)
                .and_then(|o| o.get("open_id"))
                .and_then(|o| o.as_str())
                .filter(|o| !o.is_empty())
        })
        .or_else(|| {
            event
                .get("operator")
                .and_then(|o| o.get("operator_id"))
                .and_then(|o| o.get("open_id"))
                .and_then(|o| o.as_str())
                .filter(|o| !o.is_empty())
        })?;
    let emoji = event
        .get("reaction_type")
        .and_then(|r| r.get("emoji_type"))
        .and_then(|e| e.as_str())
        .or_else(|| {
            event
                .get("reaction")
                .and_then(|r| r.get("emoji_key"))
                .and_then(|e| e.as_str())
        })?;
    let reply = match emoji {
        "THUMBSUP" => "y",
        "THUMBSDOWN" => "n",
        _ => return None,
    };
    let message_id = event
        .get("message_id")
        .and_then(|m| m.as_str())
        .filter(|m| m.starts_with("om_"))
        .or_else(|| {
            event
                .get("reaction")
                .and_then(|r| r.get("message_id"))
                .and_then(|m| m.as_str())
                .filter(|m| m.starts_with("om_"))
        })?;
    let dedup = v
        .get("header")
        .and_then(|h| h.get("event_id"))
        .and_then(|e| e.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("reaction:{operator}:{message_id}:{emoji}"));
    Some((dedup, operator.to_string(), message_id.to_string(), reply))
}

/// W3-4：bot 被加入群（`im.chat.member.bot.added_v1`）→ `(dedup_key, 群 chat_id)`。
/// payload 形态**离线按文档猜**（`event.chat_id` 或内嵌 chat 节点），缺字段 None。
pub fn parse_bot_added_event(payload: &[u8]) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_slice(payload).ok()?;
    if v.get("header")
        .and_then(|h| h.get("event_type"))
        .and_then(|t| t.as_str())
        != Some("im.chat.member.bot.added_v1")
    {
        return None;
    }
    let event = v.get("event")?;
    let chat_id = event
        .get("chat_id")
        .and_then(|c| c.as_str())
        .or_else(|| {
            event
                .get("chat")
                .and_then(|c| c.get("chat_id"))
                .and_then(|c| c.as_str())
        })
        .filter(|c| c.starts_with("oc_"))?;
    let dedup = v
        .get("header")
        .and_then(|h| h.get("event_id"))
        .and_then(|e| e.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("bot-added:{chat_id}"));
    Some((dedup, chat_id.to_string()))
}

/// 解析云文档评论事件 → `(dedup_key, comment_id, InboundMessage)`
/// （conv = `feishu:comment:<file_token>`，text = text 节点拼接）。comment_id
/// 单独返回——会话锚放宽后 conv 只锚 file_token，回复目标由调用方（drain task）
/// 登记进 platform 的锚点表，发送侧据此路由（存量 conv 内嵌形态兜底）。
///
/// P5-8：须 @bot 才触发——`bot_open_id` 已知时要求 at 节点命中 bot、且 sender
/// 不是 bot 自身（防 bot 回复再触发自己的自循环）；`bot_open_id` 未知（取 bot
/// 信息失败）时退化为「至少含一个 at 节点」的弱过滤（drain 层取到 id 后自动
/// 收紧）。缺 file_token/comment_id/sender 或纯 @ 无文字返回 None。
pub fn parse_comment_event(
    payload: &[u8],
    bot_open_id: Option<&str>,
) -> Option<(String, String, InboundMessage)> {
    let evt: CommentEvent = serde_json::from_slice(payload).ok()?;
    if evt.header.event_type != "drive.file.comment.created_v1" {
        return None;
    }
    let b = &evt.event;
    if b.file_token.is_empty() || b.comment_id.is_empty() {
        return None;
    }
    let open_id = b
        .sender
        .as_ref()
        .map(|s| s.sender_id.open_id.clone())
        .filter(|s| !s.is_empty())?;
    // P5-8：@bot 过滤。
    let at_ids: Vec<&str> = b
        .content
        .iter()
        .filter(|n| n.kind == "at")
        .filter_map(|n| n.user_id.as_deref().or(n.open_id.as_deref()))
        .filter(|s| !s.is_empty())
        .collect();
    match bot_open_id {
        Some(bot) => {
            if !at_ids.contains(&bot) {
                return None; // 未 @bot（@ 了别人或没 @）
            }
            if open_id == bot {
                return None; // bot 自身的回复（防自触发循环）
            }
        }
        None => {
            if at_ids.is_empty() {
                return None; // 弱过滤：至少要有一个 @
            }
        }
    }
    let text: Vec<String> = b
        .content
        .iter()
        .filter_map(|n| {
            n.text
                .as_ref()
                .filter(|t| !t.trim().is_empty() && n.kind == "text")
                .cloned()
        })
        .collect();
    if text.is_empty() {
        return None; // 纯 @ / 纯图片评论：MVP 不触发
    }
    let key = evt.header.event_id.clone().unwrap_or_else(|| {
        // 回退 key 用内容稳定哈希（非长度）：不同内容不同 key、相同内容同 key
        // （长度会把等长不同评论误判重复）。
        format!(
            "comment:{}:{:x}",
            b.comment_id,
            content_hash(&text.join("\n"))
        )
    });
    // T13：划词评论的引用片段前置注入——用户在评论文档哪一段，agent 此前
    // 只能盲答。quote 缺失/空白（全文评论、字段漂移）不注入（fail-soft）。
    let body = text.join("\n");
    let text = b
        .quote
        .as_deref()
        .and_then(render_comment_quote_block)
        .map(|blk| format!("{blk}\n\n{body}"))
        .unwrap_or(body);
    Some((
        key,
        b.comment_id.clone(),
        InboundMessage {
            // 会话锚点放宽到 file_token（同一文档的全部评论共享一个会话——同一
            // 文档多轮评论此前被拆成多个 conv，上下文割裂）。具体回复目标
            // comment_id 不再进 conv，单独返回由 drain 登记进 platform 锚点表。
            conv_id: ConvId(format!("{COMMENT_CONV_PREFIX}{}", b.file_token)),
            sender: UserId(open_id),
            sender_name: None,
            text: Some(text),
            media: vec![],
            media_errors: Vec::new(),
            mentions: Vec::new(),
            mentioned_bot: false,
            ask_req: None,
            reply_to: None,
            source_msg_id: None,
            control: None,
            no_steer: false,
            reply_hint: ReplyHint::None,
        },
    ))
}

/// 反解评论线程 conv_id → `(file_token, Option<comment_id>)`；非评论 conv 返回
/// None。兼容两种形态：
/// - 新（会话锚放宽后）：`feishu:comment:<file_token>`——comment_id 为 None，
///   回复目标由消息元数据（ReplyHint::Anchor）携带；
/// - 存量（旧形态）：`feishu:comment:<file_token>:<comment_id>`——comment_id
///   内嵌，发送侧可直接路由（兼容升级前的会话）。
pub fn comment_target_from_conv(conv: &ConvId) -> Option<(String, Option<String>)> {
    let rest = conv.0.strip_prefix(COMMENT_CONV_PREFIX)?;
    if rest.is_empty() {
        return None;
    }
    match rest.split_once(':') {
        Some((file_token, comment_id)) if !file_token.is_empty() && !comment_id.is_empty() => {
            Some((file_token.to_string(), Some(comment_id.to_string())))
        }
        // 新形态（无冒号）或尾段为空：整体即 file_token。
        _ => Some((rest.to_string(), None)),
    }
}

// ---------------------------------------------------------------------------
// application.url.menu_v6（自定义菜单跳转 → 合成 /help）
// ---------------------------------------------------------------------------

/// `application.url.menu_v6` 事件（后台自定义菜单点击跳转，schema 2.0 信封）。
/// 事件体字段形态**待真机校准**（离线按飞书文档常见形态实现：operator + chat_id；
/// menu_key 等字段不消费，serde 默认忽略）。
#[derive(Debug, Deserialize)]
pub struct MenuEvent {
    pub header: EventHeader,
    pub event: MenuBody,
}

#[derive(Debug, Deserialize)]
pub struct MenuBody {
    /// 点击者（与 card.action 的 operator 两形态兼容：嵌套 operator_id / 平铺 open_id）。
    #[serde(default)]
    pub operator: Option<CardOperator>,
    /// 菜单所在会话（群菜单 = 群 chat_id；私聊菜单可能缺省——回退操作者私聊 conv）。
    #[serde(default)]
    pub chat_id: Option<String>,
}

/// 解析菜单跳转事件 → `(dedup_key, text="/help" 的入站消息)`——复用 card action
/// 合成 InboundMessage 的模式：走与手打 /help 完全相同的鉴权/分派路径（未过
/// 白名单的点击照旧被 core 拒绝，无豁免）。非目标事件 / 缺 operator 返回 None。
pub fn parse_menu_event(payload: &[u8]) -> Option<(String, InboundMessage)> {
    let evt: MenuEvent = serde_json::from_slice(payload).ok()?;
    if evt.header.event_type != "application.url.menu_v6" {
        return None;
    }
    let open_id = evt.event.operator.as_ref().and_then(operator_open_id)?;
    // conv 优先事件携带的 chat_id（群/单聊均可直达），缺省回退操作者私聊 conv。
    let conv = evt
        .event
        .chat_id
        .as_deref()
        .filter(|c| !c.is_empty())
        .map(|c| format!("feishu:{c}"))
        .unwrap_or_else(|| format!("feishu:{open_id}"));
    let key = evt
        .header
        .event_id
        .clone()
        .unwrap_or_else(|| format!("menu:{open_id}:{conv}:{:x}", content_hash("menu_v6")));
    Some((
        key,
        InboundMessage {
            conv_id: ConvId(conv),
            sender: UserId(open_id),
            sender_name: None,
            text: Some("/help".to_string()),
            media: vec![],
            media_errors: Vec::new(),
            mentions: Vec::new(),
            mentioned_bot: false,
            ask_req: None,
            reply_to: None,
            source_msg_id: None,
            control: None,
            no_steer: false,
            reply_hint: ReplyHint::None,
        },
    ))
}

// ---------------------------------------------------------------------------
// im.message.recalled_v1（消息撤回，一期）
// ---------------------------------------------------------------------------

/// `im.message.recalled_v1` 事件（用户撤回消息）。事件体字段形态**待真机校准**
/// （离线按飞书文档常见形态实现：message_id + chat_id + sender；sender 在
/// 管理员撤回等形态可能缺省）。
#[derive(Debug, Deserialize)]
pub struct RecallEvent {
    pub header: EventHeader,
    pub event: RecallBody,
}

#[derive(Debug, Deserialize)]
pub struct RecallBody {
    #[serde(default)]
    pub message_id: String,
    #[serde(default)]
    pub chat_id: Option<String>,
    /// 撤回发起者（可缺省）。
    #[serde(default)]
    pub sender: Option<Sender>,
}

/// 解析撤回事件 → `(dedup_key, 控制消息)`。控制消息携带
/// `InboundControl::MessageRecalled`（source_msg_id = 被撤回消息 id）+ 回执/探测
/// 会话：notify_conv 优先事件 chat_id（群/单聊均可直达），回退撤回者私聊 conv；
/// probe_convs 汇总两种 key 形态——私聊消息的排队 key 是发送者 conv（feishu:ou_*），
/// 与事件携带的 chat_id 形态（feishu:oc_*）不同，在飞判定需两者都试。
/// 非 target 事件 / 缺 message_id / chat_id 与 sender 双缺（管理员撤回形态，
/// 无处回执——伪造 message_id conv 只会发送失败，见函数体注释）返回 None。
pub fn parse_recall_event(payload: &[u8]) -> Option<(String, InboundMessage)> {
    let evt: RecallEvent = serde_json::from_slice(payload).ok()?;
    if evt.header.event_type != "im.message.recalled_v1" {
        return None;
    }
    let mid = evt.event.message_id.clone();
    if mid.is_empty() {
        return None;
    }
    let sender_open = evt
        .event
        .sender
        .as_ref()
        .map(|s| s.sender_id.open_id.clone())
        .filter(|s| !s.is_empty());
    let chat_conv = evt
        .event
        .chat_id
        .as_deref()
        .filter(|c| !c.is_empty())
        .map(|c| ConvId(format!("feishu:{c}")));
    let sender_conv = sender_open
        .as_deref()
        .map(|o| ConvId(format!("feishu:{o}")));
    // v13-P3：chat_id 与 sender 双缺（管理员撤回形态）时不再伪造
    // `feishu:{message_id}` conv——om_ 前缀不是合法会话 id（平台 send 按
    // ou_/oc_ 前缀路由），core 侧任何经它的回执必然发送失败；该形态直接
    // 忽略（代价是牺牲此形态下「按 message_id 移除排队消息」的能力：事件
    // 字段形态本身待真机校准，fail-closed 优于带病解析）。
    let notify_conv = chat_conv.clone().or_else(|| sender_conv.clone())?;
    let mut probe_convs = Vec::new();
    if let (Some(c), Some(s)) = (&chat_conv, &sender_conv) {
        if c != s {
            probe_convs.push(c.clone());
        }
    }
    if let Some(s) = sender_conv {
        probe_convs.push(s);
    }
    let key = evt
        .header
        .event_id
        .clone()
        .unwrap_or_else(|| format!("recall:{mid}"));
    Some((
        key,
        InboundMessage {
            conv_id: notify_conv.clone(),
            sender: UserId(sender_open.unwrap_or_default()),
            sender_name: None,
            text: None,
            media: vec![],
            media_errors: Vec::new(),
            mentions: Vec::new(),
            mentioned_bot: false,
            ask_req: None,
            reply_to: None,
            source_msg_id: Some(mid),
            control: Some(imagent_core::InboundControl::MessageRecalled {
                notify_conv: Some(notify_conv),
                probe_convs,
            }),
            no_steer: false,
            reply_hint: ReplyHint::None,
        },
    ))
}

// ---------------------------------------------------------------------------
// im.chat.member.bot.deleted_v1（bot 被移出群）
// ---------------------------------------------------------------------------

/// `im.chat.member.bot.deleted_v1` 事件（bot 被移出群）。事件体字段形态**待真机
/// 校准**（离线按飞书文档常见形态实现：chat_id）。
#[derive(Debug, Deserialize)]
pub struct BotRemovedEvent {
    pub header: EventHeader,
    pub event: BotRemovedBody,
}

#[derive(Debug, Deserialize)]
pub struct BotRemovedBody {
    #[serde(default)]
    pub chat_id: String,
}

/// 解析 bot 移出群事件 → `(dedup_key, 控制消息)`（conv = `feishu:<chat_id>`，
/// 携带 `InboundControl::BotRemovedFromChat`——core 据此收回会话白名单并通知
/// 管理员）。非 target 事件 / 缺 chat_id 返回 None。
pub fn parse_bot_removed_event(payload: &[u8]) -> Option<(String, InboundMessage)> {
    let evt: BotRemovedEvent = serde_json::from_slice(payload).ok()?;
    if evt.header.event_type != "im.chat.member.bot.deleted_v1" {
        return None;
    }
    let chat_id = evt.event.chat_id.clone();
    if chat_id.is_empty() {
        return None;
    }
    let key = evt
        .header
        .event_id
        .clone()
        .unwrap_or_else(|| format!("bot_removed:{chat_id}"));
    Some((
        key,
        InboundMessage {
            conv_id: ConvId(format!("feishu:{chat_id}")),
            sender: UserId(String::new()),
            sender_name: None,
            text: None,
            media: vec![],
            media_errors: Vec::new(),
            mentions: Vec::new(),
            mentioned_bot: false,
            ask_req: None,
            reply_to: None,
            source_msg_id: None,
            control: Some(imagent_core::InboundControl::BotRemovedFromChat),
            no_steer: false,
            reply_hint: ReplyHint::None,
        },
    ))
}

// ---------------------------------------------------------------------------
// 纯函数：解析 / 映射（无网络，验收核心）
// ---------------------------------------------------------------------------

/// 解析长连接 payload。处理 `im.message.receive_v1` 的 **text / image / file / post** 消息。
///
/// P6-1：mention 处理——正文占位 `@_user_N` 替换为可读文本（@bot 剥离、@他人转
/// `@名字`），非Bot提及进 `InboundMessage.mentions`（`/allow @名字` 反解用）；
/// `policy.require_mention_in_group` 时群消息须 @bot（bot id 未知退化为弱过滤）。
///
/// 返回 `(dedup_key, InboundMessage, pending_media)`；以下情况返回 `None`
/// （上层丢弃）：非目标事件 / 不支持的消息类型（非 text/image/file/post）/ text 空文本
/// / image 缺 image_key / file 缺 file_key / post 无文字且无图片 / content 非法 JSON
/// / payload 非法 JSON / 缺 receive_id / 群消息未 @bot（按 policy）。
/// `pending_media` 为待下载的图片/文件（仅解析出 key，实际下载落盘在 platform 层
/// 完成，回填进 `InboundMessage.media`）。
pub fn parse_message_event(
    payload: &[u8],
    policy: &MentionPolicy,
    bot_open_id: Option<&str>,
) -> Option<(String, InboundMessage, Vec<PendingMedia>)> {
    let evt: FeishuEvent = serde_json::from_slice(payload).ok()?;
    if evt.header.event_type != "im.message.receive_v1" {
        return None;
    }
    let mt = evt.event.message.message_type.as_str();
    let message_id = evt.event.message.message_id.clone().unwrap_or_default();
    // 平台消息 id 透传在组装侧统一做（assemble_event_message 的 source_msg_id）。
    // 群消息 @bot 过滤（P6-1）：在正文清洗前判定，未 @bot 直接丢弃。
    // 真机校准（2026-08-30）：**斜杠命令豁免**——`/chat allow` 是群放行的
    // 引导命令（先有鸡还是先有蛋：不带 @ 的它被这里拦掉，群永远无法自助放行），
    // 命令自身的 admin/白名单门禁在 dispatch 层独立生效，豁免不放大权限面。
    // P3（v13 遗留）：豁免收敛为**已知命令白名单**——首词命中 core 分派表的
    // 命令词才豁免（判定见 [`is_known_slash_command`]）；未命中的斜杠文本
    // （`/foo 是什么意思` 一类闲聊）按普通消息走 mention 门，不再制造未知
    // 命令回执噪音。私聊不受影响（group_mention_ok 对 p2p 恒放行）。
    let is_command = mt == "text"
        && serde_json::from_str::<serde_json::Value>(&evt.event.message.content)
            .ok()
            .as_ref()
            .and_then(|v| v.get("text").and_then(|t| t.as_str()))
            .map(is_known_slash_command)
            .unwrap_or(false);
    if !is_command
        && !group_mention_ok(
            &evt.event.message.chat_type,
            &evt.event.message.mentions,
            policy,
            bot_open_id,
        )
    {
        return None;
    }
    // 解析 content：text 提取文本（空文本丢弃），image/file 提取资源 key（缺 key 丢弃）。
    let (text, pending, mentions): (
        Option<String>,
        Vec<PendingMedia>,
        Vec<imagent_core::Mention>,
    ) = match mt {
        "text" => {
            let raw = extract_text(&evt.event.message.content)?;
            let (clean, mentions) =
                apply_text_mentions(&raw, &evt.event.message.mentions, bot_open_id);
            if clean.trim().is_empty() {
                return None;
            }
            (Some(clean), vec![], mentions)
        }
        "image" => {
            let key = extract_image_key(&evt.event.message.content)?;
            (
                None,
                vec![PendingMedia {
                    kind: "image",
                    key,
                    message_id: message_id.clone(),
                    // image 消息 content 只有 image_key，无原始文件名——扩展名
                    // 落盘时按默认 png 处理（见 persist_media 取舍注释）。
                    file_name: None,
                }],
                Vec::new(),
            )
        }
        "file" => {
            let (key, file_name) = extract_file_meta(&evt.event.message.content)?;
            (
                None,
                vec![PendingMedia {
                    kind: "file",
                    key,
                    message_id: message_id.clone(),
                    file_name,
                }],
                Vec::new(),
            )
        }
        // W3-1：语音消息——下载后走 speech_to_text 转写（drain 侧），文本以
        // 【语音】前缀注入 prompt。转写失败回退媒体错误提示（fail-soft）。
        "audio" => {
            let key = extract_audio_key(&evt.event.message.content)?;
            (
                None,
                vec![PendingMedia {
                    kind: "audio",
                    key,
                    message_id: message_id.clone(),
                    file_name: None,
                }],
                Vec::new(),
            )
        }
        "post" => {
            // P6-1：post 的 @ 是独立 at 节点（正文无占位 key），mentions 由
            // parse_post 从节点提取（@bot 剔除、@他人渲染 `@名字`）。
            let (t, mut p, mentions) = parse_post(&evt.event.message.content, bot_open_id)?;
            for m in &mut p {
                m.message_id = message_id.clone();
                // post 图片节点无原始文件名字段，保持 None。
                m.file_name = None;
            }
            // 文本与图片皆空才视为无效丢弃（防御：空 post）。
            if t.as_deref().is_none_or(|s| s.trim().is_empty()) && p.is_empty() {
                return None;
            }
            (t, p, mentions)
        }
        _ => return None, // audio/video/voice/... 暂不支持
    };

    let (dedup_key, msg) = assemble_event_message(&evt, text, &pending, mentions, mt, bot_open_id)?;
    Some((dedup_key, msg, pending))
}

/// 消息事件公共组装（`parse_message_event` 与 `parse_merged_forward_event` 共用）：
/// dedup key（event_id → message_id → 内容哈希回退）、conv（话题群升级）、
/// `mentioned_bot`、`InboundMessage` 装配。`text`/`pending`/`mentions` 为类型分支
/// 已提取的产物；`mt` 仅作 dedup 回退兜底（text 与 pending 皆空的极端形态）。
fn assemble_event_message(
    evt: &FeishuEvent,
    text: Option<String>,
    pending: &[PendingMedia],
    mentions: Vec<imagent_core::Mention>,
    mt: &str,
    bot_open_id: Option<&str>,
) -> Option<(String, InboundMessage)> {
    let message_id = evt.event.message.message_id.clone().unwrap_or_default();
    // 平台消息 id 透传（撤回按此匹配 core 排队消息，见 InboundControl::MessageRecalled）。
    let source_msg_id = (!message_id.is_empty()).then(|| message_id.clone());
    let open_id = evt.event.sender.sender_id.open_id.clone();
    let (receive_id, _kind) = receive_target(&evt.event)?;
    // dedup 回退基准：优先正文内容哈希，其次首个媒体 key，最后用消息类型兜底
    // （post 可能纯文字 pending 空、或纯图片 text 空，旧逻辑 pending[0] 会 panic）。
    // 内容哈希而非长度：等长不同内容不同 key（长度会把同会话等长两条不同消息
    // 误判重复），相同内容跨重投同 key（5 分钟窗口外重投仍能去重）。
    let dedup_fallback = match (text.as_deref(), pending.first()) {
        (Some(t), _) if !t.trim().is_empty() => {
            format!("{}:{:x}", receive_id, content_hash(t))
        }
        (_, Some(p)) => format!("{}:{}", receive_id, p.key),
        _ => format!("{receive_id}:{mt}"),
    };
    let dedup_key = evt
        .header
        .event_id
        .clone()
        .or_else(|| evt.event.message.message_id.clone())
        .unwrap_or(dedup_fallback);
    // P6-4：话题群（thread）隔离——群消息带 root_id（话题根，om_ 前缀）时
    // conv 升级为 `feishu:<chat_id>:<root_id>`，每个话题独立 session/批处理；
    // 普通群回复只有 parent_id（root_id 空），不受影响。回复走 reply API 落回话题。
    let conv = match evt
        .event
        .message
        .root_id
        .as_deref()
        .filter(|r| r.starts_with("om_") && evt.event.message.chat_type == "group")
    {
        Some(root) => format!("feishu:{receive_id}:{root}"),
        None => format!("feishu:{receive_id}"),
    };
    // P7-A3：群消息是否 @ 了 bot（bot id 已知时据 mentions 元数据判定；
    // 弱过滤/无元数据为 false——陌生人提示宁可漏发不可误发）。
    let mentioned_bot = evt.event.message.chat_type == "group"
        && bot_open_id.is_some_and(|b| {
            evt.event
                .message
                .mentions
                .iter()
                .any(|m| m.open_id() == Some(b))
        });
    let msg = InboundMessage {
        conv_id: ConvId(conv),
        sender: UserId(open_id),
        sender_name: None,
        text,
        media: vec![],
        media_errors: Vec::new(),
        mentions,
        mentioned_bot,
        ask_req: None,
        reply_to: evt
            .event
            .message
            .parent_id
            .clone()
            .filter(|p| !p.is_empty()),
        source_msg_id,
        control: None,
        no_steer: false,
        reply_hint: ReplyHint::None,
    };
    Some((dedup_key, msg))
}

/// 群斜杠免检白名单判定（P3，v13 遗留「群内斜杠免检噪音」）：首词（空白
/// 分隔的首个 token）为 core 命令分派表的已知命令词才豁免 mention 门。
/// - **首词**语义与 core 分派一致（`handle` 取 `parts[0]`）：`/chat allow`、
///   `/status 查一下` 都算命中——平台层无从区分「命令参数」与「跟的闲聊」，
///   白名单挡的是**未知**命令词（`/foo 是什么意思`），已知命令带尾巴仍由
///   core 正常分派（参数被忽略，非噪音）；
/// - 命令词小写比较（core 分派同款 `to_ascii_lowercase`，`/STATUS` 命中）；
/// - 全角斜杠 `／` 不归一（core 分派归一，但平台层豁免维持 ASCII `'/'` 语义
///   不变——全角命令带 @ 照常进分派）；
/// - config `shortcuts` 的自定义 `/name` 不在清单（解析层无 config 视野），
///   群内使用需 @bot。
fn is_known_slash_command(text: &str) -> bool {
    // split_whitespace 自带前导空白跳过（clippy::trim_split_whitespace）。
    let Some(first) = text.split_whitespace().next() else {
        return false;
    };
    if !first.starts_with('/') {
        return false;
    }
    let lowered = first.to_ascii_lowercase();
    imagent_core::Dispatcher::known_command_words().contains(&lowered.as_str())
}

/// 群消息 @bot 过滤（P6-1）。
/// - p2p：一律放行（私聊无 @ 语义）；
/// - `require_mention_in_group=false`：放行（历史行为，过滤交给事件订阅 scope）；
/// - bot id 已知：mentions 含 bot 才放行；
/// - bot id 未知：弱过滤——mentions 非空即放行（与评论事件 P5-8 同语义，
///   drain 层取到 bot id 后自动收紧）。
fn group_mention_ok(
    chat_type: &str,
    mentions: &[MessageMention],
    policy: &MentionPolicy,
    bot_open_id: Option<&str>,
) -> bool {
    if chat_type != "group" || !policy.require_mention_in_group {
        return true;
    }
    match bot_open_id {
        Some(bot) => mentions.iter().any(|m| m.open_id() == Some(bot)),
        None => !mentions.is_empty(),
    }
}

/// 正文占位清洗（P6-1）：`@_user_N` → 可读文本。
/// - @bot（open_id 命中）：占位连同尾随一个空格整体剥离，不进 mentions；
/// - @他人：替换为 `@名字`（无名字时退化为剥掉占位，保留语义不炸格式）；
/// - mentions 数组外的孤儿占位原样保留（防御：飞书缺元数据时不丢字）。
///
/// 返回 (清洗后正文, 非Bot提及列表)。
fn apply_text_mentions(
    text: &str,
    mentions: &[MessageMention],
    bot_open_id: Option<&str>,
) -> (String, Vec<imagent_core::Mention>) {
    let mut out = text.to_string();
    let mut resolved: Vec<imagent_core::Mention> = Vec::new();
    for m in mentions {
        let Some(key) = m.key.as_deref().filter(|k| !k.is_empty()) else {
            continue;
        };
        let Some(open_id) = m.open_id() else {
            continue;
        };
        if bot_open_id == Some(open_id) {
            // @bot：占位 + 尾随空格一起剥（飞书渲染形态为「@bot 内容」）。
            out = out.replace(&format!("{key} "), "").replace(key, "");
            continue;
        }
        let name = m.name.as_deref().filter(|n| !n.trim().is_empty());
        out = match name {
            Some(n) => out.replace(key, &format!("@{n}")),
            None => out.replace(key, ""),
        };
        resolved.push(imagent_core::Mention {
            user_id: open_id.to_string(),
            name: name.unwrap_or_default().to_string(),
        });
    }
    (out, resolved)
}

/// 从 text 消息的 content JSON 提取文本：`{"text":"hi"}` -> `"hi"`。
/// 非法 JSON 返回 `None`。
pub fn extract_text(content: &str) -> Option<String> {
    serde_json::from_str::<TextContent>(content)
        .ok()
        .map(|c| c.text)
}

/// 从 image 消息 content 提取 image_key：`{"image_key":"..."}`。
/// 非法 JSON 或缺字段返回 `None`。
pub fn extract_image_key(content: &str) -> Option<String> {
    serde_json::from_str::<ImageContent>(content)
        .ok()
        .map(|c| c.image_key)
}

/// 从 file 消息 content 提取 `(file_key, file_name)`：`{"file_key":"...","file_name":"..."}`。
/// 非法 JSON 或缺 file_key 返回 `None`（file_name 缺省为 None）。
pub fn extract_file_meta(content: &str) -> Option<(String, Option<String>)> {
    serde_json::from_str::<FileContent>(content)
        .ok()
        .map(|c| (c.file_key, c.file_name))
}

/// 解析 post 富文本：提取所有 text 节点拼成正文 + 所有 img 节点的 image_key。
/// P6-1：at 节点——@bot 跳过（剥离），@他人渲染为 `@名字` 并进 mentions。
/// Bug 修复：a 节点（超链接）渲染 `[text](href)`（此前整段丢弃）；media /
/// emotion 节点渲染 `[视频]` / `[表情]` 占位。
/// content 非法 JSON 返回 `None`。text 全空则正文为 `None`。
fn parse_post(
    content: &str,
    bot_open_id: Option<&str>,
) -> Option<(
    Option<String>,
    Vec<PendingMedia>,
    Vec<imagent_core::Mention>,
)> {
    let post: PostContent = serde_json::from_str(content).ok()?;
    let mut texts: Vec<String> = Vec::new();
    let mut pending: Vec<PendingMedia> = Vec::new();
    let mut mentions: Vec<imagent_core::Mention> = Vec::new();
    if !post.title.trim().is_empty() {
        texts.push(post.title);
    }
    for row in &post.content {
        for node in row {
            match node.tag.as_str() {
                "text" => {
                    if let Some(t) = node.text.as_ref().filter(|s| !s.is_empty()) {
                        texts.push(t.clone());
                    }
                }
                "img" => {
                    if let Some(k) = node.image_key.as_ref().filter(|s| !s.is_empty()) {
                        pending.push(PendingMedia {
                            kind: "image",
                            key: k.clone(),
                            message_id: String::new(),
                            file_name: None,
                        });
                    }
                }
                "at" => {
                    // post 的 at 节点无占位 key，按节点剔除：@bot 跳过，
                    // @他人渲染 `@名字`（无名字只进 mentions 不占正文）。
                    let uid = node.user_id.as_deref().filter(|s| !s.is_empty());
                    if uid.is_none_or(|u| bot_open_id != Some(u)) {
                        if let Some(u) = uid {
                            let name = node
                                .user_name
                                .as_deref()
                                .filter(|n| !n.trim().is_empty())
                                .unwrap_or_default();
                            if !name.is_empty() {
                                texts.push(format!("@{name}"));
                            }
                            mentions.push(imagent_core::Mention {
                                user_id: u.to_string(),
                                name: name.to_string(),
                            });
                        }
                    }
                }
                // a 节点（超链接）：此前 `_ => {}` 整段丢弃——用户发「总结这个链接」
                // agent 收不到链接本体。渲染成 markdown 链接 `[text](href)`；无
                // text（纯 URL 分享）或 text 与 href 相同（防 `[url](url)` 冗余）时
                // 直接给 href；href 缺失退化为纯 text。
                "a" => {
                    let href = node.href.as_deref().filter(|s| !s.is_empty());
                    let text = node.text.as_deref().filter(|s| !s.trim().is_empty());
                    match (href, text) {
                        (Some(h), Some(t)) if t != h => texts.push(format!("[{t}]({h})")),
                        (Some(h), _) => texts.push(h.to_string()),
                        (None, Some(t)) => texts.push(t.to_string()),
                        (None, None) => {}
                    }
                }
                // 视频 / 表情包节点：正文无对应可下载资源（media 走 file_key 视频流、
                // emotion 走表情商店），渲染占位让 agent 知道「这里有个视频/表情」，
                // 用户补发文字或截图即可继续。
                "media" => texts.push("[视频]".to_string()),
                "emotion" => texts.push("[表情]".to_string()),
                _ => {} // 其余未知节点忽略
            }
        }
    }
    let text = if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    };
    Some((text, pending, mentions))
}

// ---------------------------------------------------------------------------
// im.message.receive_v1 · merged_forward（合并转发消息完整支持）
// ---------------------------------------------------------------------------

/// 「查询合并转发消息列表」（GET `/im/v1/messages/{message_id}/merge_forward`）
/// 返回的子消息条目（`client::list_merge_forward` 分页聚齐后交本模块转录）。
/// 字段按飞书文档公开形态建模，**待真机校准**（宽容提取见 client 侧注释：
/// message_type/msg_type 两名、时间戳字符串/数字/秒级归一等都兼容）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedForwardItem {
    /// 子消息 id（转录不直接用；保留排障与后续嵌套展开用）。
    pub message_id: String,
    /// 子消息类型（text/post/image/file/...；子消息里可能再次出现 merged_forward）。
    pub message_type: String,
    /// 子消息 content（JSON 字符串，形态同普通消息：`{"text":"…"}` 等）。
    pub content: String,
    /// 发送者标识（open_id / user_id 等，随 API 的 sender.id_type）。
    pub sender_id: String,
    /// 发送者显示名（可缺省——缺失时转录用 id 后 8 位，见 sender_label）。
    pub sender_name: Option<String>,
    /// 创建时间（毫秒 epoch；0 = 缺失/非法，转录行省略时间段）。
    pub create_time_ms: i64,
}

/// drain 层转录一条合并转发消息所需的元数据（见 [`parse_merged_forward_event`]）。
#[derive(Debug)]
pub struct MergedForwardMeta {
    /// 合并转发消息自身的 message_id（「查询合并转发消息列表」API 入参）。
    pub message_id: String,
    /// 转录头标题（事件 content JSON 可解析时给出；缺省头回退「共 N 条」）。
    pub title: Option<String>,
    /// 转录头摘要（同上，可缺省——有则作头部的第二行）。
    pub summary: Option<String>,
}

/// merged_forward 消息 content 的头元数据结构。事件侧 content 常为占位文本
/// （"Merged and Forwarded Message"），title/summary 仅在 content 恰为可解析
/// JSON 时可用——**待真机校准**（离线按飞书文档公开形态建模：title + summary，
/// 均可缺省；占位/非法 JSON 走 [`Option::unwrap_or_default`] 全空，头回退条数）。
#[derive(Debug, Default, Deserialize)]
struct MergedForwardContent {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
}

/// 合并转发转录的长度上限（**字符**而非字节——CJK 安全）。超限截断并在尾部
/// 标注「（已截断，共 N 条中前 M 条）」，防 agent prompt 被超大转发记录撑爆。
pub const MERGE_FORWARD_TRANSCRIPT_MAX: usize = 8_000;

/// 合并转发入站消息的占位正文：parse 阶段先占位，drain 拉到子消息后替换为
/// 转录文本（拉取失败不进 agent——占位不外泄，仅防御 drain 异常路径）。
pub(crate) const MERGE_FORWARD_PLACEHOLDER: &str = "[合并转发消息]";

/// 解析合并转发消息事件（message_type = `merged_forward`）→
/// `(dedup_key, 入站消息（占位正文）, 拉取元数据)`。完整支持（替换 v1.12.0 的
/// 「暂不支持」快赢）：事件只带占位 content，真实子消息由 drain 层按
/// `meta.message_id` 调 `client::list_merge_forward` 分页拉取，转录回填
/// `msg.text`（转录见 [`render_merge_forward_transcript`]，drain 见 platform）。
///
/// - 群内仍要求 @bot（`group_mention_ok` 与普通消息同门槛，无特判）；
/// - content JSON 的 title/summary **尽力解析**（占位文本/非法 JSON → None，
///   头回退「共 N 条」——占位 content 是常态，不能因此拒收）；
/// - 缺 message_id → None（无法拉子消息），走 `unsupported_message_notice`
///   的兜底提示（「解析失败回退现状提示」）；
/// - 不产生 PendingMedia（子消息的图片/文件一期只占位不下载，与媒体下载
///   管线无冲突）；dedup 走既有管线（drain 侧 `dedup.check`）。
///
/// 非 merged_forward 消息 / 非目标事件 / 非法 JSON / 群消息未 @bot（按 policy）
/// 返回 None。
pub fn parse_merged_forward_event(
    payload: &[u8],
    policy: &MentionPolicy,
    bot_open_id: Option<&str>,
) -> Option<(String, InboundMessage, MergedForwardMeta)> {
    let evt: FeishuEvent = serde_json::from_slice(payload).ok()?;
    if evt.header.event_type != "im.message.receive_v1" {
        return None;
    }
    if evt.event.message.message_type != "merged_forward" {
        return None;
    }
    let message_id = evt
        .event
        .message
        .message_id
        .clone()
        .filter(|m| !m.is_empty())?;
    if !group_mention_ok(
        &evt.event.message.chat_type,
        &evt.event.message.mentions,
        policy,
        bot_open_id,
    ) {
        return None;
    }
    // 头元数据尽力解析：占位文本/非法 JSON → 全空（头回退「共 N 条」）。
    let head: MergedForwardContent =
        serde_json::from_str(&evt.event.message.content).unwrap_or_default();
    // 占位正文：drain 拉到子消息后替换为转录文本（见 platform drain）。
    let (key, msg) = assemble_event_message(
        &evt,
        Some(MERGE_FORWARD_PLACEHOLDER.to_string()),
        &[],
        Vec::new(),
        "merged_forward",
        bot_open_id,
    )?;
    Some((
        key,
        msg,
        MergedForwardMeta {
            message_id,
            title: head.title,
            summary: head.summary,
        },
    ))
}

/// 把子消息列表转录为人可读且 agent 友好的文本块（纯函数，验收核心）：
///
/// ```text
/// 【合并转发聊天记录】{title 或 "共 N 条"}
/// {summary（可缺省，有则单独一行）}
/// [发送者标识 12:34] 文本内容
/// [发送者标识 12:35] [图片]
/// [发送者标识 12:36] [合并转发消息（嵌套）]
/// ```
///
/// - 发送者标识：name 优先（非空），否则 id 后 8 位（不足取全部；全缺 → 「未知」）；
/// - 时间 HH:MM：create_time 毫秒 → 本地时区（chrono Local）；缺/非法省略时间段；
/// - 子消息类型映射见 [`merge_forward_body`]（媒体一期不下载，占位示意）；
/// - 超过 [`MERGE_FORWARD_TRANSCRIPT_MAX`] 按字符边界截断，尾部标注条数。
pub fn render_merge_forward_transcript(
    items: &[MergedForwardItem],
    title: Option<&str>,
    summary: Option<&str>,
) -> String {
    let n = items.len();
    let title = title.map(str::trim).filter(|t| !t.is_empty());
    let mut out = match title {
        Some(t) => format!("【合并转发聊天记录】{t}"),
        None => format!("【合并转发聊天记录】共 {n} 条"),
    };
    let mut used = out.chars().count();
    if let Some(s) = summary.map(str::trim).filter(|s| !s.is_empty()) {
        out.push('\n');
        out.push_str(s);
        used += 1 + s.chars().count();
    }
    let mut included = 0usize;
    let mut truncated = false;
    // 平台限制（2026-09 官方双语文档确认）：资源下载接口不支持合并转发
    // 子消息（图片/视频/文件只可占位）。统计不可读媒体数，末尾给一句引导
    // ——agent 据此主动告知用户「单独发图即可分析」，不再各自踩一遍疑问。
    let unreadable_media = items
        .iter()
        .filter(|it| {
            matches!(
                it.message_type.as_str(),
                "image" | "media" | "file" | "sticker" | "emotion" | "video"
            )
        })
        .count();
    for item in items {
        let line = format!("\n{}", merge_forward_line(item));
        let ll = line.chars().count();
        if used + ll > MERGE_FORWARD_TRANSCRIPT_MAX {
            // 首条就超限也硬截保留一条（空转录对 agent 无信息量）；按字符边界截。
            if included == 0 {
                let room = MERGE_FORWARD_TRANSCRIPT_MAX.saturating_sub(used);
                out.push_str(&line.chars().take(room).collect::<String>());
                included = 1;
            }
            truncated = true;
            break;
        }
        out.push_str(&line);
        used += ll;
        included += 1;
    }
    if truncated {
        out.push_str(&format!("\n（已截断，共 {n} 条中前 {included} 条）"));
    }
    if unreadable_media > 0 {
        out.push_str(&format!(
            "\n（注：记录中另有 {unreadable_media} 条图片/文件，飞书 API 限制无法读取合并转发内的媒体；需要分析请单独发送该图）"
        ));
    }
    out
}

/// 单条子消息 → `[发送者标识 HH:MM] 正文` 行（时间缺省 → `[发送者标识] 正文`）。
fn merge_forward_line(item: &MergedForwardItem) -> String {
    let sender = merge_forward_sender_label(&item.sender_name, &item.sender_id);
    let body = merge_forward_body(item);
    match merge_forward_time_label(item.create_time_ms) {
        Some(t) => format!("[{sender} {t}] {body}"),
        None => format!("[{sender}] {body}"),
    }
}

/// 发送者标识：name 优先（非空），否则 id 后 8 位（不足 8 位取全部；全缺 → 未知）。
/// 后 8 位是「无名字时仍可区分不同发言人」的最短稳定形态（ou_/oc_ 前缀对人不
/// 可读，整串太长刷屏）。
fn merge_forward_sender_label(name: &Option<String>, id: &str) -> String {
    if let Some(n) = name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        return n.to_string();
    }
    if id.is_empty() {
        return "未知".to_string();
    }
    let tail: String = id
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    tail
}

/// create_time 毫秒 → 本地时区 HH:MM（缺/非法/超范围 → None，行内省略时间）。
fn merge_forward_time_label(ms: i64) -> Option<String> {
    use chrono::TimeZone;
    if ms <= 0 {
        return None;
    }
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.format("%H:%M").to_string())
}

/// 子消息类型 → 转录正文（共享映射见 [`message_body_of`]）。
fn merge_forward_body(item: &MergedForwardItem) -> String {
    message_body_of(&item.message_type, &item.content)
}

/// 消息 (类型, content) → 转录正文——合并转发子消息与 T10 群聊上下文**共用**的
/// 类型映射。媒体类（图片/表情/视频/文件）不下载，只占位让 agent 知道「这里有
/// 个媒体」（用户需要细节可截图/单发）；text/post 取文字。
fn message_body_of(mt: &str, content: &str) -> String {
    match mt {
        // text：取 content JSON 的 text；@_user_N 占位**保留原样**——列表类 API
        // 不回 mentions 元数据，正则清掉会丢「此处有 @」的语义、名字又无从
        // 还原，保留原样是最简单且不撒谎的选择。
        "text" => extract_text(content).unwrap_or_else(|| "[文本消息]".to_string()),
        // post：复用既有 post→文本逻辑（parse_post）。图片节点不下载：有文字
        // 只取文字（agent 拿不到图，占位反而误导）；纯图 post 以 [图片] 示意。
        "post" => match parse_post(content, None) {
            Some((Some(t), _, _)) if !t.trim().is_empty() => t,
            Some((_, pending, _)) if !pending.is_empty() => "[图片]".to_string(),
            _ => "[富文本消息]".to_string(),
        },
        "image" => "[图片]".to_string(),
        "sticker" | "emotion" => "[表情]".to_string(),
        // file：content JSON 有 file_name 则带上（agent 至少知道是什么文件）。
        "file" => {
            let name = serde_json::from_str::<serde_json::Value>(content)
                .ok()
                .and_then(|v| {
                    v.get("file_name")
                        .and_then(|f| f.as_str())
                        .map(str::trim)
                        .filter(|f| !f.is_empty())
                        .map(String::from)
                });
            match name {
                Some(n) => format!("[文件: {n}]"),
                None => "[文件]".to_string(),
            }
        }
        "media" | "video" => "[视频]".to_string(),
        // 卡片（含本 bot 回复卡）：抽文本正文（v1.27.0——此前只占位，聊天记录
        // 里引用 bot 回答追问同样失效）；模板卡/抽不到回退占位。
        "interactive" => card_text_transcript(content).unwrap_or_else(|| "[卡片消息]".to_string()),
        // 嵌套合并转发：**不递归**调 list_merge_forward——嵌套层数无界，每层一次
        // 分页拉取，深度 × API 配额易失控（用户「转发套转发」是常态），一期只
        // 标注占位让 agent 知道结构，用户需要细节可展开后单发。
        // 真机校准（2026-09-29）：拉取 API 的嵌套子消息类型是单数
        // merge_forward（此前只认复数 → 全部落「未知类型消息」占位）。
        "merged_forward" | "merge_forward" => "[合并转发消息（嵌套）]".to_string(),
        _ => "[未知类型消息]".to_string(),
    }
}

/// 按 chat_type 决定发回的 receive_id：
/// - p2p → sender.open_id（OpenId）
/// - group → event.chat.chat_id，回退 message.chat_id（ChatId）
fn receive_target(event: &EventBody) -> Option<(String, ReceiveIdKind)> {
    if event.message.chat_type == "p2p" {
        let oid = event.sender.sender_id.open_id.clone();
        return if oid.is_empty() {
            None
        } else {
            Some((oid, ReceiveIdKind::OpenId))
        };
    }
    if let Some(c) = &event.chat {
        return Some((c.chat_id.clone(), ReceiveIdKind::ChatId));
    }
    if let Some(cid) = &event.message.chat_id {
        return Some((cid.clone(), ReceiveIdKind::ChatId));
    }
    None
}

/// 发消息反向解析：`feishu:<id>[:<root_id>]` → `(id, kind)`。
/// 飞书 ID 前缀约定：`ou_` = open_id（用户，私聊），其余（`oc_` = chat_id，群聊）→ ChatId。
/// P6-4：话题群 conv 带 `:<root_id>` 后缀——发送目标取首段（话题内回复由
/// [`thread_target_from_conv`] 分流到 reply API）。
/// 无 `feishu:` 前缀返回 `None`（非法 conv_id，上层报错）。
pub fn receive_target_from_conv(conv: &ConvId) -> Option<(String, ReceiveIdKind)> {
    let rest = conv.0.strip_prefix("feishu:")?;
    let id = rest.split(':').next().unwrap_or(rest);
    let kind = if id.starts_with("ou_") {
        ReceiveIdKind::OpenId
    } else {
        ReceiveIdKind::ChatId
    };
    Some((id.to_string(), kind))
}

/// 话题群 conv 反解（P6-4）：`feishu:<chat_id>:<root_id>`（root 为 `om_` 前缀的
/// 话题根消息 id）→ `(chat_id, root_id)`。非话题 conv 返回 None。
/// 评论 conv（`feishu:comment:…`）第二段非 om_ 前缀，天然不命中。
pub fn thread_target_from_conv(conv: &ConvId) -> Option<(String, String)> {
    let rest = conv.0.strip_prefix("feishu:")?;
    let (chat_id, root_id) = rest.split_once(':')?;
    if chat_id.is_empty() || root_id.is_empty() || !root_id.starts_with("om_") {
        return None;
    }
    Some((chat_id.to_string(), root_id.to_string()))
}

// ---------------------------------------------------------------------------
// T10：群聊上下文注入——群消息触发轮次时拉本群最近 N 条消息前置进 prompt。
// ---------------------------------------------------------------------------

/// 群 conv → 会话历史 API 的 chat_id（T10 群上下文注入用）：普通群
/// `feishu:oc_xxx` 与话题群 `feishu:oc_xxx:om_root` 都取首段 `oc_xxx`（话题内
/// 免 @ 窗口与注入同语义，无需特判——上下文按整群拉取）。私聊（`ou_` 前缀）、
/// 评论 conv（`feishu:comment:…`）、非法前缀返回 None（不注入）。
pub fn group_chat_id_of_conv(conv: &str) -> Option<String> {
    let rest = conv.strip_prefix("feishu:")?;
    let id = rest.split(':').next().unwrap_or(rest);
    (!id.is_empty() && id.starts_with("oc_")).then(|| id.to_string())
}

/// 「获取指定消息 / 获取会话历史消息」列表类 API 单条 item 的统一提取产物。
///
/// 真机校准（2026-09-29）收口：合并转发子消息（client::merge_forward_item_of）、
/// 群历史（[`parse_group_context_items`]）、单条拉取（client::fetch_message_raw）
/// 三处此前各自手写字段名，同一批 schema 偏差打了两遍——提取口径从此单一
/// 事实源，宽容姿态（字段别名/回退）集中维护、集中测试。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RawMessageItem {
    pub message_id: String,
    /// 类型：`msg_type`（真机主形态），兼容别名 `message_type`。注意拉取 API
    /// 的合并转发是**单数** `merge_forward`，事件侧才是复数（见
    /// [`message_body_of`] 的双形态分支）。
    pub message_type: String,
    /// 正文 JSON 字符串（`{"text":"…"}` 等形态）：`body.content`（真机）优先，
    /// 顶层 `content` 兼容回退。
    pub content: String,
    pub sender_id: String,
    /// 真机 sender 无 name 字段（可缺省——转录用 id 后 8 位兜底）。
    pub sender_name: Option<String>,
    /// `sender.sender_type`（"user"/"app"/…；空 = 缺失）——群上下文 bot 判定用。
    pub sender_type: String,
    /// `sender.app_id`（bot 判定第二保险；空 = 缺失/非应用发送）。
    pub sender_app_id: String,
    /// 合并转发子消息的父级 id（None = 父消息本体或非合并转发场景）。
    pub upper_message_id: Option<String>,
    /// 毫秒 epoch（0 = 缺失/非法；秒级自动 ×1000 归一，见 [`create_time_ms_of`]）。
    pub create_time_ms: i64,
}

/// 单条列表 item → [`RawMessageItem`]（非对象返回 None；字段缺失给默认值不丢
/// 整条——转录对残缺条目有占位语义，见 [`message_body_of`]）。
pub(crate) fn parse_raw_message_item(v: &serde_json::Value) -> Option<RawMessageItem> {
    let obj = v.as_object()?;
    let str_of = |k: &str| {
        obj.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    let (sender_id, sender_name, sender_type, sender_app_id) =
        match obj.get("sender").and_then(|s| s.as_object()) {
            Some(s) => {
                let id = s
                    .get("id")
                    .and_then(|x| x.as_str())
                    .or_else(|| s.get("open_id").and_then(|x| x.as_str()))
                    .unwrap_or("")
                    .to_string();
                let name = s
                    .get("name")
                    .and_then(|x| x.as_str())
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(String::from);
                (
                    id,
                    name,
                    s.get("sender_type")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    s.get("app_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                )
            }
            None => (String::new(), None, String::new(), String::new()),
        };
    let upper = str_of("upper_message_id");
    Some(RawMessageItem {
        message_id: str_of("message_id"),
        message_type: obj
            .get("msg_type")
            .and_then(|x| x.as_str())
            .or_else(|| obj.get("message_type").and_then(|x| x.as_str()))
            .unwrap_or("")
            .to_string(),
        content: obj
            .get("body")
            .and_then(|b| b.get("content"))
            .and_then(|x| x.as_str())
            .or_else(|| obj.get("content").and_then(|x| x.as_str()))
            .unwrap_or("")
            .to_string(),
        sender_id,
        sender_name,
        sender_type,
        sender_app_id,
        upper_message_id: (!upper.is_empty()).then_some(upper),
        create_time_ms: create_time_ms_of(obj.get("create_time")),
    })
}

/// 「获取会话历史消息」（GET `/im/v1/messages`，container_id_type=chat）返回的
/// 单条消息解析产物（`parse_group_context_items` 提取后交
/// [`render_group_context_block`] 转录）。字段按飞书文档公开形态建模，
/// **待真机校准**（宽容提取：类型名 message_type/msg_type 两名、时间戳字符串/
/// 数字/秒级归一等都兼容，手法同 [`MergedForwardItem`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupContextItem {
    /// 发送者标识（user_id_type=open_id 请求下为 open_id；app 消息为 app 标识）。
    pub sender_id: String,
    /// 发送者显示名（API 可缺省——缺失时转录用 id 后 8 位，见
    /// [`merge_forward_sender_label`]）。
    pub sender_name: Option<String>,
    /// 是否 bot/app 发的消息（sender_type=app 或 sender.app_id 命中本应用）——
    /// 转录时跳过：自身回复是 agent 输出回声，其它 bot 消息是自动化噪音。
    pub from_bot: bool,
    /// 消息类型（text/post/image/…，映射见 [`message_body_of`]）。
    pub message_type: String,
    /// 消息 content（JSON 字符串，形态同普通消息：`{"text":"…"}` 等）。
    pub content: String,
    /// 创建时间（毫秒 epoch；0 = 缺失/非法）。
    pub create_time_ms: i64,
}

/// 解析「获取会话历史消息」响应的 `data.items`（纯函数，mock JSON 可测）。
/// 宽容姿态（同 [`MergedForwardItem`] 的提取取舍）：非对象条目跳过、字段缺失给
/// 默认值不丢整条；`own_app_id` 为本应用 app_id（drain 侧的构造入参，同事件
/// header.app_id 来源）——`sender.sender_type == "app"` 或 `sender.app_id` 命中
/// 即标 `from_bot`。信封 code 检查在 client 层（此处只管 data 提取）。
pub fn parse_group_context_items(v: &serde_json::Value, own_app_id: &str) -> Vec<GroupContextItem> {
    let items = v
        .pointer("/data/items")
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default();
    items
        .iter()
        .filter_map(|it| {
            // 字段提取统一走 parse_raw_message_item（单一事实源，2026-09-29
            // 真机校准收口——此处此前的手写提取漏 body.content 导致整链占位）。
            let raw = parse_raw_message_item(it)?;
            // bot 判定双保险：sender_type=app（含自身与其它应用）或 app_id
            // 命中本应用（字段形态变化时任一命中即跳过）。
            let from_bot = raw.sender_type == "app"
                || (!raw.sender_app_id.is_empty() && raw.sender_app_id == own_app_id);
            Some(GroupContextItem {
                sender_id: raw.sender_id,
                sender_name: raw.sender_name,
                from_bot,
                message_type: raw.message_type,
                content: raw.content,
                create_time_ms: raw.create_time_ms,
            })
        })
        .collect()
}

/// 群上下文注入块的总长上限（**字符**而非字节——CJK 安全，对齐引用/合并转发
/// 转录的 1500 档）：群历史是背景信息，超长会挤占本轮 prompt 预算。
pub const GROUP_CONTEXT_MAX_CHARS: usize = 1_500;
/// 单条消息正文的截断长度（字符）——群聊消息绝大多数在此之内，超长的粘贴
/// 内容截断保头。
const GROUP_CONTEXT_LINE_MAX_CHARS: usize = 200;
/// 截断标注行「（已截断，共 N 条中前 M 条）」的保守预留（n ≤ 50 时最长约
/// 22 字符，取 24）：行累计预算预先扣掉，保证标注 + 尾注后总长仍 ≤ 上限。
const GROUP_CONTEXT_TRUNC_NOTE_RESERVE: usize = 24;

/// 把群历史条目转录为前置注入块（纯函数，验收核心）：
///
/// ```text
/// 【群最近上下文（3 条，最新在最后）】
/// Alice: 上一条讨论
/// ou_xxx 后 8 位: [图片]
/// Bob: 最新这条
///
/// （以下是用户本轮消息）
/// ```
///
/// - 跳过 `from_bot` 条目（agent 输出回声/自动化噪音）；全部被跳过 → None（不注入）；
/// - 按时间**正序**排列（API 以 ByCreateTimeDesc 拉回最新 N 条，此处稳定升序
///   ——最新在最后，读感同聊天记录）；时间缺失（0）按原相对序沉底；
/// - 每条 `{发送者名或 id 后 8 位}: {正文截 200 字}`（类型映射与合并转发转录
///   共用 [`message_body_of`]：text/post 取文字，媒体给 `[图片]` 式占位）；
/// - 总长（含尾注）≤ [`GROUP_CONTEXT_MAX_CHARS`]：逐行累计预算，超限停止并
///   标注「（已截断，共 N 条中前 M 条）」；尾注「（以下是用户本轮消息）」恒保留。
pub fn render_group_context_block(items: &[GroupContextItem]) -> Option<String> {
    // 尾注恒保留：正文（头 + 行 + 截断标注）的总预算 = 上限 - 尾注长。
    let tail = "\n\n（以下是用户本轮消息）";
    let budget = GROUP_CONTEXT_MAX_CHARS - tail.chars().count();
    let mut kept: Vec<&GroupContextItem> = items.iter().filter(|it| !it.from_bot).collect();
    kept.sort_by_key(|it| it.create_time_ms);
    if kept.is_empty() {
        return None;
    }
    let n = kept.len();
    let mut out = format!("【群最近上下文（{n} 条，最新在最后）】");
    let mut used = out.chars().count();
    let mut included = 0usize;
    let mut truncated = false;
    for it in kept {
        let sender = merge_forward_sender_label(&it.sender_name, &it.sender_id);
        let body: String = message_body_of(&it.message_type, &it.content)
            .chars()
            .take(GROUP_CONTEXT_LINE_MAX_CHARS)
            .collect();
        let line = format!("\n{sender}: {body}");
        let ll = line.chars().count();
        if used + ll > budget.saturating_sub(GROUP_CONTEXT_TRUNC_NOTE_RESERVE) {
            // 首条就超限也硬截保留一条（空转录对 agent 无信息量）；按字符边界截。
            if included == 0 {
                let room = budget
                    .saturating_sub(used + GROUP_CONTEXT_TRUNC_NOTE_RESERVE + 1)
                    .min(ll.saturating_sub(1));
                out.push('\n');
                out.push_str(&line[1..].chars().take(room).collect::<String>());
                included = 1;
            }
            truncated = true;
            break;
        }
        out.push_str(&line);
        used += ll;
        included += 1;
    }
    if truncated {
        out.push_str(&format!("\n（已截断，共 {n} 条中前 {included} 条）"));
    }
    // 防御性硬截（预算预留足够时通常不触发）：保证总长恒 ≤ 上限。
    let body: String = out.chars().take(budget).collect();
    Some(format!("{body}{tail}"))
}

/// create_time 宽容解析：字符串 / 数字皆可；秒级值（0 < ts < 1e11，毫秒形态最早
/// 1973 年）自动 ×1000 归一毫秒——飞书各 API 时间戳单位不统一，按量级判别。
/// **待真机校准**：真机确认恒为毫秒后可去掉归一。
///（自 client.rs 移入——合并转发子消息与群历史消息两处共用同一语义。）
pub(crate) fn create_time_ms_of(v: Option<&serde_json::Value>) -> i64 {
    let raw = v
        .and_then(|x| {
            x.as_i64()
                .or_else(|| x.as_str().and_then(|s| s.parse::<i64>().ok()))
        })
        .unwrap_or(0);
    if raw > 0 && raw < 100_000_000_000 {
        raw * 1000
    } else {
        raw
    }
}

// ---------------------------------------------------------------------------
// 单测：纯逻辑，无网络、无真机。验收核心。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧语义（宽松策略、bot id 未知）的解析入口——历史用例断言不变。
    fn parse_permissive(payload: &[u8]) -> Option<(String, InboundMessage, Vec<PendingMedia>)> {
        parse_message_event(payload, &MentionPolicy::PERMISSIVE, None)
    }

    /// p2p 文本：conv=feishu:<open_id>、sender=open_id、text 正确、dedup=event_id。
    /// v1.25.2：peek_reply_parent 不限会话形态（私聊引用此前被群版跳过）。
    #[test]
    fn peek_reply_parent_accepts_p2p() {
        #[derive(serde::Serialize)]
        struct Mk<'a> {
            event: Msg<'a>,
        }
        #[derive(serde::Serialize)]
        struct Msg<'a> {
            message: M<'a>,
        }
        #[derive(serde::Serialize)]
        struct M<'a> {
            chat_type: &'a str,
            parent_id: Option<&'a str>,
        }
        let mk = |ct: &str, p: Option<&str>| {
            serde_json::to_vec(&Mk {
                event: Msg {
                    message: M {
                        chat_type: ct,
                        parent_id: p,
                    },
                },
            })
            .unwrap()
        };
        assert_eq!(
            peek_reply_parent(&mk("p2p", Some("om_p1"))).as_deref(),
            Some("om_p1"),
            "私聊引用也应提取 parent_id"
        );
        assert_eq!(
            peek_reply_parent(&mk("group", Some("om_p1"))).as_deref(),
            Some("om_p1")
        );
        assert!(peek_reply_parent(&mk("p2p", None)).is_none());
        assert!(
            peek_reply_parent(&mk("p2p", Some(""))).is_none(),
            "空 id 不放行"
        );
    }

    #[test]
    fn quoted_context_text_variants() {
        // v1.25 引用上下文：text 取 JSON text；post 复用 parse_post；空/不支持 None。
        assert_eq!(
            quoted_context_text("text", r#"{"text":"被引用的内容"}"#).as_deref(),
            Some("被引用的内容")
        );
        let post =
            r#"{"content":[[{"tag":"text","text":"第一行"},{"tag":"text","text":"第二行"}]]}"#;
        let got = quoted_context_text("post", post).unwrap_or_default();
        assert!(got.contains("第一行") && got.contains("第二行"), "{got}");
        assert!(quoted_context_text("text", r#"{"text":"  "}"#).is_none());
        assert!(quoted_context_text("image", "{}").is_none());
    }

    /// v1.27.0 引用卡片修复：interactive 卡抽文本正文（本 bot 回复卡的
    /// schema 2.0 形态——markdown 正文/折叠面板/按钮行/notation footer）。
    #[test]
    fn card_text_transcript_bot_schema2_card() {
        let card = r#"{
            "schema": "2.0",
            "header": { "title": { "tag": "plain_text", "content": "✅ 完成" } },
            "config": { "streaming_mode": false, "summary": { "content": "列表预览不入正文" } },
            "body": { "elements": [
                { "tag": "markdown", "content": "修复建议：升级 rustls 后重试。" },
                { "tag": "hr" },
                { "tag": "markdown", "content": "🔧 工具 2 次：Bash×2", "text_size": "notation" },
                { "tag": "collapsible_panel", "expanded": false,
                  "header": { "title": { "tag": "markdown", "content": "🔧 工具轨迹（2）" } },
                  "elements": [ { "tag": "markdown", "content": "- cargo test", "text_size": "notation" } ] },
                { "tag": "column_set", "flex_mode": "flow", "columns": [
                    { "tag": "column", "width": "auto", "elements": [
                        { "tag": "button", "text": { "tag": "plain_text", "content": "🔁 再跑一次" },
                          "type": "primary",
                          "behaviors": [ { "type": "callback", "value": { "command": "/again" } } ] }
                    ] }
                ] }
            ] }
        }"#;
        let got = card_text_transcript(card).unwrap_or_default();
        assert!(got.contains("✅ 完成"), "header 标题应收录: {got}");
        assert!(got.contains("修复建议：升级 rustls 后重试。"), "{got}");
        assert!(got.contains("🔧 工具轨迹（2）"), "面板头应收录: {got}");
        assert!(got.contains("- cargo test"), "面板正文应收录: {got}");
        assert!(got.contains("🔁 再跑一次"), "按钮文案应收录: {got}");
        assert!(
            !got.contains("列表预览不入正文"),
            "config.summary 不入正文: {got}"
        );
        assert!(!got.contains("/again"), "按钮 value 业务载荷不收: {got}");
    }

    /// 模板卡（正文不在 content）→ None 回退占位；第三方 schema 1.0 卡
    /// （div/lark_md + note + action）同 walker 覆盖；空/非法输入 None。
    #[test]
    fn card_text_transcript_template_and_foreign_schema1() {
        let template = r#"{
            "type": "template",
            "data": { "template_id": "tmpl_x", "template_variable": { "k": "v" } }
        }"#;
        assert!(card_text_transcript(template).is_none());

        let v1 = r#"{
            "config": {},
            "elements": [
                { "tag": "div",
                  "text": { "tag": "lark_md", "content": "**审批单** AP-1024" },
                  "fields": [ { "is_short": true, "text": { "tag": "lark_md", "content": "状态：待审批" } } ] },
                { "tag": "note", "elements": [ { "tag": "plain_text", "content": "来自审批中心" } ] },
                { "tag": "action", "actions": [
                    { "tag": "button", "text": { "tag": "lark_md", "content": "通过" }, "type": "primary" }
                ] }
            ]
        }"#;
        let got = card_text_transcript(v1).unwrap_or_default();
        assert!(
            got.contains("AP-1024") && got.contains("状态：待审批"),
            "{got}"
        );
        assert!(
            got.contains("来自审批中心") && got.contains("通过"),
            "{got}"
        );

        assert!(card_text_transcript("{}").is_none());
        assert!(card_text_transcript("not json").is_none());
    }

    /// P3（v13 遗留 fuzz 缺口）：深嵌套卡片（≤128 层，serde_json 合法解析域）
    /// → walker 显式深度上限截断超深子树——不栈溢出、深部文本被截、顶层
    /// 浅层文本照常收集；边界内（< 64 层）不截断。
    #[test]
    fn card_text_transcript_deep_nesting_truncated_not_crash() {
        // 100 层 wrap 嵌套 + 底部深埋 markdown（elements 子项深度 101 ≥ 64）。
        let mut deep = serde_json::json!({ "tag": "markdown", "content": "深处的文本" });
        for _ in 0..100 {
            deep = serde_json::json!({ "wrap": deep });
        }
        let card = serde_json::json!({
            "elements": [
                { "tag": "markdown", "content": "顶层文本" },
                deep,
            ]
        });
        let got = card_text_transcript(&card.to_string()).unwrap_or_default();
        assert!(got.contains("顶层文本"), "浅层文本照常收集: {got}");
        assert!(!got.contains("深处的文本"), "超深子树应截断: {got}");
        // 边界内（60 层 wrap，markdown 对象深度 61 < 64）不截断。
        let mut ok = serde_json::json!({ "tag": "markdown", "content": "边界内文本" });
        for _ in 0..60 {
            ok = serde_json::json!({ "wrap": ok });
        }
        let card2 = serde_json::json!({ "elements": [ok] });
        let got2 = card_text_transcript(&card2.to_string()).unwrap();
        assert!(got2.contains("边界内文本"), "边界内不截断: {got2}");
    }

    #[test]
    fn parse_p2p_text() {
        let payload = br#"{
            "schema":"2.0",
            "header":{"event_id":"evt_1","event_type":"im.message.receive_v1"},
            "event":{
                "sender":{"sender_id":{"open_id":"ou_user1"}},
                "message":{"message_type":"text","content":"{\"text\":\"hi there\"}","chat_type":"p2p","chat_id":"","message_id":"om_msg1"}
            }
        }"#;
        let (key, msg, pending) = parse_permissive(payload).expect("p2p 文本应解析成功");
        assert_eq!(key, "evt_1");
        assert_eq!(msg.conv_id.0, "feishu:ou_user1");
        assert_eq!(msg.sender.0, "ou_user1");
        assert_eq!(msg.text.as_deref(), Some("hi there"));
        assert!(pending.is_empty(), "文本消息不应有待下载媒体");
    }

    /// group 文本：conv=feishu:<chat_id>、sender=发言者 open_id。
    #[test]
    fn parse_group_text() {
        let payload = br#"{
            "header":{"event_id":"evt_2","event_type":"im.message.receive_v1"},
            "event":{
                "sender":{"sender_id":{"open_id":"ou_user2"}},
                "message":{"message_type":"text","content":"{\"text\":\"hello group\"}","chat_type":"group","chat_id":"oc_chat1","message_id":"om_msg2"},
                "chat":{"chat_id":"oc_chat1"}
            }
        }"#;
        let (key, msg, _) = parse_permissive(payload).expect("group 文本应解析成功");
        assert_eq!(key, "evt_2");
        assert_eq!(msg.conv_id.0, "feishu:oc_chat1");
        assert_eq!(msg.sender.0, "ou_user2");
        assert_eq!(msg.text.as_deref(), Some("hello group"));
    }

    /// 群消息缺 event.chat 时回退 message.chat_id。
    #[test]
    fn parse_group_fallback_message_chat_id() {
        let payload = br#"{
            "header":{"event_type":"im.message.receive_v1"},
            "event":{
                "sender":{"sender_id":{"open_id":"ou_user3"}},
                "message":{"message_type":"text","content":"{\"text\":\"x\"}","chat_type":"group","chat_id":"oc_chat2","message_id":"om_msg3"}
            }
        }"#;
        let (_key, msg, _) = parse_permissive(payload).expect("group 回退 chat_id 应成功");
        assert_eq!(msg.conv_id.0, "feishu:oc_chat2");
    }

    /// dedup 回退 key 用内容哈希：缺 event_id/message_id 时，同会话**等长不同**
    /// 文本必须得到不同 key（旧按长度的回退会误判重复丢第二条）。
    #[test]
    fn dedup_fallback_equal_length_distinct_texts_differ() {
        let mk = |text: &str| {
            let payload = format!(
                r#"{{"header":{{"event_type":"im.message.receive_v1"}},
                "event":{{"sender":{{"sender_id":{{"open_id":"ou_u"}}}},
                "message":{{"message_type":"text","content":"{{\"text\":\"{text}\"}}","chat_type":"p2p","chat_id":""}}}}}}"#
            );
            let (key, msg, _) = parse_permissive(payload.as_bytes()).expect("应解析成功");
            assert_eq!(msg.text.as_deref(), Some(text));
            key
        };
        let k1 = mk("hello");
        let k2 = mk("world");
        assert_ne!(k1, k2, "等长不同文本的回退 dedup key 必须不同");
        // 相同内容（模拟重投）→ 同 key。
        assert_eq!(k1, mk("hello"));
    }

    /// dedup 回退 key 稳定性：同一内容多次哈希值一致；不同内容哈希值不同。
    #[test]
    fn content_hash_stable_and_distinct() {
        assert_eq!(content_hash("abc"), content_hash("abc"));
        assert_ne!(content_hash("abc"), content_hash("abd"));
    }

    /// W3-2：表情回应事件——👍/👎 映射 y/n；其它 emoji / 缺字段 / 非目标事件
    /// 均返回 None（fail-soft）。
    /// W3-2 真机校准回归：2026-08-29 实测 payload 原样（user_id / reaction_type
    /// 嵌套 / message_id 顶层）——旧解析器按文档猜字段路径，实测全部落空、
    /// 事件被当非目标丢弃（用户点 👍 无反应）。
    #[test]
    fn parse_reaction_event_real_device_payload() {
        let payload = br#"{"schema":"2.0","header":{"event_id":"evt_real1","event_type":"im.message.reaction.created_v1","token":"","create_time":"1787969732208","tenant_key":"t","app_id":"cli_x"},"event":{"action_time":"1787969732208","message_id":"om_x100b6610454b0ca4b1fa2d9aaef7004","operator_type":"user","reaction_type":{"emoji_type":"THUMBSUP"},"user_id":{"open_id":"ou_a0c072f42e7c1b0995b7fd4841b4671b"}}}"#;
        let (dedup, operator, mid, reply) =
            parse_reaction_event(payload).expect("真机形态应解析成功");
        assert_eq!(operator, "ou_a0c072f42e7c1b0995b7fd4841b4671b");
        assert_eq!(mid, "om_x100b6610454b0ca4b1fa2d9aaef7004");
        assert_eq!(reply, "y");
        assert_eq!(dedup, "evt_real1");
        // 👎 → n；非 👍/👎 emoji → None（无副作用）。（payload 为纯 ASCII JSON，
        // 经 String 替换安全。）
        let as_str = |b: &[u8]| String::from_utf8(b.to_vec()).unwrap();
        let dn = as_str(payload).replace("THUMBSUP", "THUMBSDOWN");
        assert!(parse_reaction_event(dn.as_bytes())
            .map(|(_, _, _, r)| r == "n")
            .unwrap_or(false));
        let wave = as_str(payload).replace("THUMBSUP", "WAVE");
        assert!(parse_reaction_event(wave.as_bytes()).is_none());
    }

    #[test]
    fn parse_reaction_event_maps_thumbs() {
        let mk = |emoji: &str| {
            serde_json::json!({
                "header":{"event_id":"evt_r1","event_type":"im.message.reaction.created_v1"},
                "event":{
                    "operator_id":{"open_id":"ou_op"},
                    "reaction":{"emoji_key":emoji,"message_id":"om_card1"}
                }
            })
            .to_string()
            .into_bytes()
        };
        let (key, operator, msg_id, reply) =
            parse_reaction_event(&mk("THUMBSUP")).expect("👍 应映射 y");
        assert_eq!(key, "evt_r1");
        assert_eq!(operator, "ou_op");
        assert_eq!(msg_id, "om_card1");
        assert_eq!(reply, "y");
        let (_, _, _, reply) = parse_reaction_event(&mk("THUMBSDOWN")).expect("👎 应映射 n");
        assert_eq!(reply, "n");
        // 其它 emoji / 非 om_ 消息 id / 非目标事件 → None。
        assert!(parse_reaction_event(&mk("SMILE")).is_none());
        let bad_msg = serde_json::json!({
            "header":{"event_type":"im.message.reaction.created_v1"},
            "event":{"operator_id":{"open_id":"ou_op"},"reaction":{"emoji_key":"THUMBSUP","message_id":"not-om"}}
        })
        .to_string()
        .into_bytes();
        assert!(parse_reaction_event(&bad_msg).is_none());
        let other_evt = serde_json::json!({
            "header":{"event_type":"im.message.receive_v1"},
            "event":{"operator_id":{"open_id":"ou_op"},"reaction":{"emoji_key":"THUMBSUP","message_id":"om_1"}}
        })
        .to_string()
        .into_bytes();
        assert!(parse_reaction_event(&other_evt).is_none());
    }

    /// W3-4：bot 进群事件——chat_id（oc_ 前缀）解析；p2p 形态/缺字段 None。
    #[test]
    fn parse_bot_added_event_extracts_chat() {
        let payload = br#"{
            "header":{"event_id":"evt_add1","event_type":"im.chat.member.bot.added_v1"},
            "event":{"chat_id":"oc_group1"}
        }"#;
        let (key, chat) = parse_bot_added_event(payload).expect("进群事件应解析");
        assert_eq!(key, "evt_add1");
        assert_eq!(chat, "oc_group1");
        // 内嵌 chat 节点形态。
        let nested = br#"{
            "header":{"event_type":"im.chat.member.bot.added_v1"},
            "event":{"chat":{"chat_id":"oc_g2"}}
        }"#;
        let (_, chat) = parse_bot_added_event(nested).expect("内嵌形态应解析");
        assert_eq!(chat, "oc_g2");
        // 非 oc_ 前缀 / 非目标事件 → None。
        let bad = br#"{"header":{"event_type":"im.chat.member.bot.added_v1"},"event":{"chat_id":"ou_x"}}"#;
        assert!(parse_bot_added_event(bad).is_none());
        let other =
            br#"{"header":{"event_type":"im.message.receive_v1"},"event":{"chat_id":"oc_x"}}"#;
        assert!(parse_bot_added_event(other).is_none());
    }

    /// 非 im.message.receive_v1 事件丢弃。
    #[test]
    fn ignore_other_event_type() {
        let payload = br#"{
            "header":{"event_id":"evt_x","event_type":"application.url.menu_v6"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"text","content":"{\"text\":\"hi\"}","chat_type":"p2p"}}
        }"#;
        assert!(parse_permissive(payload).is_none());
    }

    /// W3-1：audio 现已支持（pending 含 audio key）；真正不支持的类型
    /// （video/voice）仍丢弃。
    #[test]
    fn audio_parses_and_video_drops() {
        let payload = br#"{
            "header":{"event_id":"evt_a","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"audio","content":"{\"file_key\":\"aud_k1\"}","chat_type":"p2p"}}
        }"#;
        let (_key, msg, pending) = parse_permissive(payload).expect("语音应解析（转写在 drain）");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, "audio");
        assert_eq!(pending[0].key, "aud_k1");
        assert!(msg.text.is_none(), "proto 阶段无文本（转写后回填）");
        let video = br#"{
            "header":{"event_id":"evt_v","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"video","content":"{}","chat_type":"p2p"}}
        }"#;
        assert!(parse_permissive(video).is_none(), "video 仍不支持");
    }

    /// p2p 图片：pending 含 image key，msg.text==None、media 空。
    #[test]
    fn parse_p2p_image() {
        let payload = br#"{
            "header":{"event_id":"evt_img","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_user1"}},"message":{"message_type":"image","content":"{\"image_key\":\"img_v3_00ab\"}","chat_type":"p2p"}}
        }"#;
        let (key, msg, pending) = parse_permissive(payload).expect("图片应解析成功");
        assert_eq!(key, "evt_img");
        assert_eq!(msg.conv_id.0, "feishu:ou_user1");
        assert_eq!(msg.sender.0, "ou_user1");
        assert!(msg.text.is_none(), "图片消息无文本");
        assert!(
            msg.media.is_empty(),
            "media 由 platform 层回填，proto 阶段为空"
        );
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, "image");
        assert_eq!(pending[0].key, "img_v3_00ab");
    }

    /// p2p 文件：pending 含 file key + 原始文件名（原名透传落盘用）。
    #[test]
    fn parse_p2p_file() {
        // content 须为 JSON 字符串（构造时转义，原名含非 ASCII 不能进 raw byte 串）。
        let content = serde_json::json!({ "file_key": "file_v3_001", "file_name": "报告.pdf" });
        let payload = serde_json::json!({
            "header":{"event_id":"evt_file","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_user2"}},"message":{
                "message_type":"file","content":content.to_string(),"chat_type":"p2p"}}
        })
        .to_string()
        .into_bytes();
        let (key, msg, pending) = parse_permissive(&payload).expect("文件应解析成功");
        assert_eq!(key, "evt_file");
        assert_eq!(msg.conv_id.0, "feishu:ou_user2");
        assert!(msg.text.is_none());
        assert!(msg.media.is_empty());
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, "file");
        assert_eq!(pending[0].key, "file_v3_001");
        assert_eq!(pending[0].file_name.as_deref(), Some("报告.pdf"));
    }

    /// image content 缺 image_key（字段缺失）丢弃。
    #[test]
    fn ignore_image_missing_key() {
        let payload = br#"{
            "header":{"event_id":"e","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"image","content":"{}","chat_type":"p2p"}}
        }"#;
        assert!(parse_permissive(payload).is_none());
    }

    /// image content 非法 JSON 丢弃。
    #[test]
    fn ignore_image_invalid_content_json() {
        let payload = br#"{"header":{"event_id":"e","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"image","content":"not-json","chat_type":"p2p"}}}"#;
        assert!(parse_permissive(payload).is_none());
    }

    /// image 消息缺 event_id 时 dedup 回退到 message_id，再缺回退到 receive_id:image_key。
    #[test]
    fn image_dedup_fallback() {
        // 有 message_id → 用 message_id。
        let p1 = br#"{"header":{"event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"image","content":"{\"image_key\":\"img_k1\"}","chat_type":"p2p","message_id":"om_img1"}}}"#;
        let (key, _, _) = parse_permissive(p1).expect("应解析成功");
        assert_eq!(key, "om_img1");

        // event_id 与 message_id 都缺 → 回退 receive_id:image_key。
        let p2 = br#"{"header":{"event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"image","content":"{\"image_key\":\"img_k2\"}","chat_type":"p2p"}}}"#;
        let (key2, _, _) = parse_permissive(p2).expect("应解析成功");
        assert_eq!(key2, "ou_x:img_k2");
    }

    /// 空文本（含纯空白）丢弃。
    #[test]
    fn ignore_empty_text() {
        let empty = br#"{"header":{"event_id":"e","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"text","content":"{\"text\":\"\"}","chat_type":"p2p"}}}"#;
        assert!(parse_permissive(empty).is_none());

        let ws = br#"{"header":{"event_id":"e","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"text","content":"{\"text\":\"   \"}","chat_type":"p2p"}}}"#;
        assert!(parse_permissive(ws).is_none());
    }

    /// 非法 JSON payload 丢弃。
    #[test]
    fn ignore_invalid_json() {
        assert!(parse_permissive(b"not json at all").is_none());
        assert!(parse_permissive(b"").is_none());
    }

    /// content 非法 JSON 丢弃。
    #[test]
    fn ignore_invalid_content_json() {
        let payload = br#"{"header":{"event_id":"e","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"text","content":"not-json","chat_type":"p2p"}}}"#;
        assert!(parse_permissive(payload).is_none());
    }

    /// dedup key 回退：缺 event_id 时用 message_id。
    #[test]
    fn dedup_key_falls_back_to_message_id() {
        let payload = br#"{
            "header":{"event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_user9"}},"message":{"message_type":"text","content":"{\"text\":\"hi\"}","chat_type":"p2p","message_id":"om_fb"}}
        }"#;
        let (key, _, _) = parse_permissive(payload).expect("应解析成功");
        assert_eq!(key, "om_fb");
    }

    /// receive_target_from_conv roundtrip：ou_ → OpenId，oc_ → ChatId，无前缀 → None。
    #[test]
    fn conv_roundtrip() {
        let (id, kind) = receive_target_from_conv(&ConvId("feishu:ou_abc".into())).unwrap();
        assert_eq!(id, "ou_abc");
        assert_eq!(kind, ReceiveIdKind::OpenId);

        let (id, kind) = receive_target_from_conv(&ConvId("feishu:oc_def".into())).unwrap();
        assert_eq!(id, "oc_def");
        assert_eq!(kind, ReceiveIdKind::ChatId);

        // 非 ou_ 前缀一律按 ChatId 处理。
        let (id, kind) = receive_target_from_conv(&ConvId("feishu:other".into())).unwrap();
        assert_eq!(id, "other");
        assert_eq!(kind, ReceiveIdKind::ChatId);

        // 无 feishu: 前缀 → None。
        assert!(receive_target_from_conv(&ConvId("wecom:x".into())).is_none());
    }

    /// extract_text 正常 / 非法 JSON。
    #[test]
    fn extract_text_works() {
        assert_eq!(
            extract_text(r#"{"text":"hello"}"#),
            Some("hello".to_string())
        );
        assert_eq!(extract_text("not json"), None);
        assert_eq!(extract_text(""), None);
    }

    /// post 图片+文字：提取正文 + image_key。
    #[test]
    fn parse_p2p_post_image_text() {
        let payload = r#"{
            "header":{"event_id":"evt_post","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_user1"}},"message":{"message_type":"post","content":"{\"title\":\"\",\"content\":[[{\"tag\":\"img\",\"image_key\":\"img_v3_abc\",\"width\":539,\"height\":317}],[{\"tag\":\"text\",\"text\":\"你能给我描述一下这张图片吗？\",\"style\":[]}]]}","chat_type":"p2p"}}
        }"#;
        let (key, msg, pending) =
            parse_permissive(payload.as_bytes()).expect("post 图片+文字应解析成功");
        assert_eq!(key, "evt_post");
        assert_eq!(msg.text.as_deref(), Some("你能给我描述一下这张图片吗？"));
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, "image");
        assert_eq!(pending[0].key, "img_v3_abc");
    }

    /// post 纯图片（无文字）：text=None, pending=[image]。
    #[test]
    fn parse_p2p_post_image_only() {
        let payload = r#"{
            "header":{"event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_u"}},"message":{"message_type":"post","content":"{\"content\":[[{\"tag\":\"img\",\"image_key\":\"img_only\"}]]}","chat_type":"p2p","message_id":"om_p"}}
        }"#;
        let (key, msg, pending) = parse_permissive(payload.as_bytes()).expect("纯图片 post 应解析");
        assert_eq!(key, "om_p");
        assert!(msg.text.is_none(), "纯图片 post 无正文");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].key, "img_only");
    }

    /// post 纯文字（无图）：text=..., pending=[]。
    #[test]
    fn parse_p2p_post_text_only() {
        let payload = r#"{
            "header":{"event_id":"e","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_u"}},"message":{"message_type":"post","content":"{\"content\":[[{\"tag\":\"text\",\"text\":\"hello post\"}]]}","chat_type":"p2p"}}
        }"#;
        let (_key, msg, pending) =
            parse_permissive(payload.as_bytes()).expect("纯文字 post 应解析");
        assert_eq!(msg.text.as_deref(), Some("hello post"));
        assert!(pending.is_empty(), "纯文字 post 无图片");
    }

    /// post 空内容（无文字无图）丢弃。
    #[test]
    fn ignore_empty_post() {
        let payload = br#"{
            "header":{"event_id":"e","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_u"}},"message":{"message_type":"post","content":"{\"content\":[]}","chat_type":"p2p"}}
        }"#;
        assert!(parse_permissive(payload).is_none());
    }

    // ---------- P4-4：card.action.trigger（审批按钮回调） ----------

    #[test]
    fn parse_card_action_allow_and_deny() {
        let mk = |act: &str| {
            serde_json::json!({
                "schema":"2.0",
                "header":{"event_id":"evt_btn_1","event_type":"card.action.trigger"},
                "event":{
                    "operator":{"operator_id":{"open_id":"ou_op"}},
                    "action":{"tag":"button","value":{"imagent_perm":act,"conv":"feishu:ou_op"}}
                }
            })
            .to_string()
            .into_bytes()
        };
        let (key, msg, _) = parse_card_action_event(&mk("allow")).expect("allow 应回调");
        assert_eq!(key, "evt_btn_1");
        assert_eq!(msg.conv_id.0, "feishu:ou_op");
        assert_eq!(msg.sender.0, "ou_op");
        assert_eq!(msg.text.as_deref(), Some("y"));
        let (_, msg, _) = parse_card_action_event(&mk("deny")).expect("deny 应回调");
        assert_eq!(msg.text.as_deref(), Some("n"));
    }

    /// P3：缺 event_id 的 card_action 回退 key 用 content_hash——前 40 字符相同
    /// 的不同长文本不再被互相去重，相同内容仍稳定同 key。
    #[test]
    fn card_action_dedup_fallback_uses_content_hash() {
        let mk = |cmd: &str, event_id: Option<&str>| {
            let mut header = serde_json::json!({"event_type":"card.action.trigger"});
            if let Some(id) = event_id {
                header["event_id"] = serde_json::json!(id);
            }
            serde_json::json!({
                "schema":"2.0",
                "header":header,
                "event":{
                    "operator":{"operator_id":{"open_id":"ou_op"}},
                    "action":{"tag":"button","value":{"imagent_cmd":cmd,"conv":"feishu:ou_op"}}
                }
            })
            .to_string()
            .into_bytes()
        };
        let long_a = format!("/do {} aaa", "x".repeat(60));
        let long_b = format!("/do {} bbb", "x".repeat(60));
        let (ka, _, _) = parse_card_action_event(&mk(&long_a, None)).expect("解析成功");
        let (kb, _, _) = parse_card_action_event(&mk(&long_b, None)).expect("解析成功");
        // 前 40 字符相同（"/do " + 60 个 x 覆盖前缀窗口）但内容不同 → 不同 key。
        assert_ne!(ka, kb, "前缀相同的不同长命令不应同 key: {ka} vs {kb}");
        // 相同内容（重投）→ 稳定同 key。
        let (ka2, _, _) = parse_card_action_event(&mk(&long_a, None)).expect("解析成功");
        assert_eq!(ka, ka2);
        // 有 event_id 时仍优先用 event_id。
        let (kid, _, _) = parse_card_action_event(&mk(&long_a, Some("evt_x"))).expect("解析成功");
        assert_eq!(kid, "evt_x");
    }

    /// P9-2：表单提交回调——用户输入在 action.form_value（不在 value），合成
    /// `/config form k=v …`；键白名单外的键被丢弃；无 form_value 整体丢弃。
    #[test]
    fn parse_card_action_form_submit() {
        let mk = |form_value: serde_json::Value| {
            serde_json::json!({
                "schema":"2.0",
                "header":{"event_id":"evt_form_1","event_type":"card.action.trigger"},
                "event":{
                    "operator":{"open_id":"ou_op"},
                    "action":{
                        "tag":"button",
                        "value":{"imagent_form":"config","conv":"feishu:ou_op"},
                        "form_value": form_value
                    }
                }
            })
            .to_string()
            .into_bytes()
        };
        let (_, msg, _) = parse_card_action_event(&mk(serde_json::json!({
            "reply_mode": "text", "cot_detail": "detailed", "extra_key": "evil"
        })))
        .expect("表单提交应回调");
        assert_eq!(
            msg.text.as_deref(),
            Some("/config form reply_mode=text cot_detail=detailed"),
            "白名单键按序拼接、白名单外丢弃: {:?}",
            msg.text
        );
        assert_eq!(msg.sender.0, "ou_op");
        // 空 form_value → 丢弃。
        assert!(parse_card_action_event(&mk(serde_json::json!({}))).is_none());
        // 无 form_value 字段 → 丢弃。
        let no_fv = serde_json::json!({
            "schema":"2.0",
            "header":{"event_id":"evt_form_2","event_type":"card.action.trigger"},
            "event":{
                "operator":{"open_id":"ou_op"},
                "action":{"tag":"button","value":{"imagent_form":"config","conv":"feishu:ou_op"}}
            }
        })
        .to_string()
        .into_bytes();
        assert!(parse_card_action_event(&no_fv).is_none());
    }

    /// 问题卡表单提交（imagent_form=ask）：form_value.ask_opt 单值（下拉）直通
    /// `ask:<选项>`；数组（checkbox 多选）按「、」拼接（多选语义）；ask_req 从
    /// value.req 精确路由；空选择丢弃。
    #[test]
    fn parse_card_action_ask_form_submit() {
        let mk = |form_value: serde_json::Value| {
            serde_json::json!({
                "schema":"2.0",
                "header":{"event_id":"evt_ask_form","event_type":"card.action.trigger"},
                "event":{
                    "operator":{"open_id":"ou_op"},
                    "action":{
                        "tag":"button",
                        "value":{"imagent_form":"ask","conv":"feishu:ou_op","req":"reqQ"},
                        "form_value": form_value
                    }
                }
            })
            .to_string()
            .into_bytes()
        };
        // 下拉单选：字符串直通。
        let (_, msg, _) = parse_card_action_event(&mk(serde_json::json!({"ask_opt": "方案2"})))
            .expect("单选表单应回调");
        assert_eq!(msg.text.as_deref(), Some("ask:方案2"));
        assert_eq!(msg.ask_req.as_deref(), Some("reqQ"), "req 精确路由");
        assert_eq!(msg.conv_id.0, "feishu:ou_op");
        // checkbox 多选：数组拼接。
        let (_, multi, _) = parse_card_action_event(&mk(serde_json::json!({
            "ask_opt": ["数据库迁移", "接口改造"]
        })))
        .expect("多选表单应回调");
        assert_eq!(
            multi.text.as_deref(),
            Some("ask:数据库迁移、接口改造"),
            "多选语义拼接: {:?}",
            multi.text
        );
        // 多题字段（P0-AUQ v1.17）：ask_opt_0..N 按序「；」拼接（值=题头=选项）。
        let (_, mq, _) = parse_card_action_event(&mk(serde_json::json!({
            "ask_opt_1": "是否备份=是",
            "ask_opt_0": "部署环境=测试环境"
        })))
        .expect("多题表单应回调");
        assert_eq!(
            mq.text.as_deref(),
            Some("ask:部署环境=测试环境；是否备份=是"),
            "多题拼接（题序）: {:?}",
            mq.text
        );
        // 多题含 checkbox：题内「、」、题间「；」。
        let (_, mqm, _) = parse_card_action_event(&mk(serde_json::json!({
            "ask_opt_0": "环境=测试",
            "ask_opt_1": ["范围=前端", "范围=后端"]
        })))
        .expect("多题多选应回调");
        assert_eq!(
            mqm.text.as_deref(),
            Some("ask:环境=测试；范围=前端、范围=后端")
        );
        // 自由输入（v1.17.2）：非空优先于选项、原文进消息；混合时按题序。
        let (_, mix, _) = parse_card_action_event(&mk(serde_json::json!({
            "ask_opt_0": "部署环境=测试环境",
            "ask_opt_0_free": "  ",
            "ask_opt_1": "是否备份=是",
            "ask_opt_1_free": "先备份 db 再备份配置"
        })))
        .expect("混合应回调");
        assert_eq!(
            mix.text.as_deref(),
            Some("ask:部署环境=测试环境；先备份 db 再备份配置"),
            "free 优先且空串忽略: {:?}",
            mix.text
        );
        // 仅自由输入的题（v1.17.3 真机失败形态）：选项键缺席 form_value，
        // 题号只存在于 _free 键——必须照样入列（旧实现整题漏掉）。
        let (_, fonly, _) = parse_card_action_event(&mk(serde_json::json!({
            "ask_opt_0": "备份策略=执行备份",
            "ask_opt_1": "执行时机=等待窗口期",
            "ask_opt_2_free": "目标环境用预发环境",
            "ask_opt_3_free": "团队通知发研发群"
        })))
        .expect("仅 free 的题应回调");
        assert_eq!(
            fonly.text.as_deref(),
            Some(
                "ask:备份策略=执行备份；执行时机=等待窗口期；目标环境用预发环境；团队通知发研发群"
            ),
            "free-only 题不因选项键缺席而漏: {:?}",
            fonly.text
        );
        // 真机载荷原样固化（2026-09-03 校准）：未填的 free 回空串、未选的
        // select 键整体缺席——空串忽略回落选项，题号两类键并集保证不漏题。
        let (_, real, _) = parse_card_action_event(&mk(serde_json::json!({
            "ask_opt_0_free": "我需要发布到开发环境",
            "ask_opt_1": "是否备份=执行备份",
            "ask_opt_1_free": "",
            "ask_opt_2_free": "通知给对应用户",
            "ask_opt_3": "执行时机=等待窗口期",
            "ask_opt_3_free": ""
        })))
        .expect("真机形态应回调");
        assert_eq!(
            real.text.as_deref(),
            Some(
                "ask:我需要发布到开发环境；是否备份=执行备份；通知给对应用户；执行时机=等待窗口期"
            ),
            "真机载荷回放: {:?}",
            real.text
        ); // 空数组 / 缺字段 → 丢弃。
        assert!(parse_card_action_event(&mk(serde_json::json!({"ask_opt": []}))).is_none());
        assert!(parse_card_action_event(&mk(serde_json::json!({}))).is_none());
    }

    /// 真机校准（2026-08）：新版回调信封 operator.open_id 平铺（不再嵌套
    /// operator_id），action.value 保持嵌套。按线上真实 payload 形态构造。
    #[test]
    fn parse_card_action_flat_operator_envelope() {
        let payload = serde_json::json!({
            "schema": "2.0",
            "header": {"event_id": "evt_flat_1", "event_type": "card.action.trigger",
                        "token": "t", "create_time": "1787363803096225",
                        "tenant_key": "tk", "app_id": "cli_x"},
            "event": {
                "operator": {"tenant_key": "tk", "open_id": "ou_real", "union_id": "on_x"},
                "action": {"tag": "button", "value": {"imagent_perm": "allow", "conv": "feishu:ou_real"}}
            }
        })
        .to_string()
        .into_bytes();
        let (key, msg, _) = parse_card_action_event(&payload).expect("平铺 operator 应可解析");
        assert_eq!(key, "evt_flat_1");
        assert_eq!(msg.sender.0, "ou_real");
        assert_eq!(msg.conv_id.0, "feishu:ou_real");
        assert_eq!(msg.text.as_deref(), Some("y"));
    }

    #[test]
    /// P6：问题卡选项按钮（imagent_ask）→ ask:<选项> 文本（经 parse_reply 转
    /// deny+message 回给 agent）。
    fn parse_card_action_question_option_to_ask_text() {
        let payload = serde_json::json!({
            "header": {"event_id": "evt_ask_1", "event_type": "card.action.trigger"},
            "event": {
                "operator": {"open_id": "ou_q"},
                "action": {"tag": "button", "value": {"imagent_ask": "数据库迁移", "conv": "feishu:ou_q"}}
            }
        })
        .to_string()
        .into_bytes();
        let (key, msg, _) = parse_card_action_event(&payload).expect("选项回调应可解析");
        assert_eq!(key, "evt_ask_1");
        assert_eq!(msg.text.as_deref(), Some("ask:数据库迁移"));
        assert_eq!(msg.conv_id.0, "feishu:ou_q");
    }

    /// D-记忆：审批卡「🔓 本次会话始终允许」按钮 → text = "always"（core 的
    /// parse_reply 命中 ALWAYS_WORDS，route 把 pending 工具加入会话级 allow-set）。
    #[test]
    fn card_action_always_button_maps_to_always_word() {
        let payload = br#"{
            "header": {"event_type": "card.action.trigger", "event_id": "e-always"},
            "event": {
                "action": {"tag": "button", "value": {
                    "imagent_perm": "always", "conv": "feishu:ou_a", "req": "p-1"
                }},
                "operator": {"open_id": "ou_op"}
            }
        }"#;
        let (_, msg, _) = parse_card_action_event(payload).expect("always 回调应解析");
        assert_eq!(msg.text.as_deref(), Some("always"));
        assert_eq!(msg.ask_req.as_deref(), Some("p-1"), "req 精确路由");
    }

    /// 多 pending：value 携带 req（request_id）→ ask_req 透传（无 req 时为 None，
    /// 兼容旧卡/手拼 payload）。
    #[test]
    fn parse_card_action_carries_request_id() {
        let with_req = serde_json::json!({
            "header": {"event_id": "evt_req_1", "event_type": "card.action.trigger"},
            "event": {
                "operator": {"open_id": "ou_r"},
                "action": {"tag": "button", "value": {
                    "imagent_ask": "选项A", "conv": "feishu:ou_r", "req": "t-abc123"
                }}
            }
        })
        .to_string()
        .into_bytes();
        let (_, msg, _) = parse_card_action_event(&with_req).expect("应可解析");
        assert_eq!(msg.ask_req.as_deref(), Some("t-abc123"));
        // 无 req：ask_req None（路由回落 parent/最新兜底）。
        let no_req = serde_json::json!({
            "header": {"event_id": "evt_req_2", "event_type": "card.action.trigger"},
            "event": {
                "operator": {"open_id": "ou_r"},
                "action": {"tag": "button", "value": {"imagent_perm": "allow", "conv": "feishu:ou_r"}}
            }
        })
        .to_string()
        .into_bytes();
        let (_, msg, _) = parse_card_action_event(&no_req).expect("应可解析");
        assert_eq!(msg.ask_req, None);
    }

    #[test]
    fn parse_card_action_ignores_foreign_and_missing() {
        // 非 card.action.trigger。
        let not_card = br#"{"header":{"event_type":"im.message.receive_v1"},"event":{}}"#;
        assert!(parse_card_action_event(not_card).is_none());
        // value 缺 conv。
        let no_conv = br#"{"header":{"event_id":"e","event_type":"card.action.trigger"},
            "event":{"operator":{"operator_id":{"open_id":"ou_x"}},"action":{"value":{"imagent_perm":"allow"}}}}"#;
        assert!(parse_card_action_event(no_conv).is_none());
        // 未知动作。
        let unknown = br#"{"header":{"event_id":"e","event_type":"card.action.trigger"},
            "event":{"operator":{"operator_id":{"open_id":"ou_x"}},"action":{"value":{"imagent_perm":"maybe","conv":"feishu:ou_x"}}}}"#;
        assert!(parse_card_action_event(unknown).is_none());
        // 缺 operator open_id。
        let no_op = br#"{"header":{"event_id":"e","event_type":"card.action.trigger"},
            "event":{"operator":{"operator_id":{}},"action":{"value":{"imagent_perm":"allow","conv":"feishu:ou_x"}}}}"#;
        assert!(parse_card_action_event(no_op).is_none());
    }

    /// P6-3：命令按钮回调——value 带 imagent_cmd（/ 开头）→ text = 命令本体；
    /// 非 / 开头（防伪造普通文本）→ None。
    #[test]
    fn parse_card_action_command_button() {
        let mk = |cmd: &str| {
            serde_json::json!({
                "header":{"event_id":"evt_cmd_1","event_type":"card.action.trigger"},
                "event":{
                    "operator":{"open_id":"ou_op"},
                    "action":{"tag":"button","value":{"imagent_cmd":cmd,"conv":"feishu:oc_g"}}
                }
            })
            .to_string()
            .into_bytes()
        };
        let (key, msg, _) = parse_card_action_event(&mk("/ws use main")).expect("命令按钮应回调");
        assert_eq!(key, "evt_cmd_1");
        assert_eq!(msg.conv_id.0, "feishu:oc_g");
        assert_eq!(msg.sender.0, "ou_op");
        assert_eq!(msg.text.as_deref(), Some("/ws use main"));
        // 非 / 开头 → 拒（回调不应产生普通聊天文本）。
        assert!(parse_card_action_event(&mk("rm -rf /")).is_none());
        // 旧信封（operator_id 嵌套 + action.value）同样支持命令按钮。
        let legacy = br#"{"header":{"event_id":"evt_cmd_2","event_type":"card.action.trigger"},
            "event":{"operator":{"operator_id":{"open_id":"ou_o2"}},"action":{"value":{"imagent_cmd":"/resume 3","conv":"feishu:ou_o2"}}}}"#;
        let (_, msg, _) = parse_card_action_event(legacy).expect("旧信封命令按钮应回调");
        assert_eq!(msg.text.as_deref(), Some("/resume 3"));
    }

    // ---------- P4-9：drive.file.comment.created_v1（云文档评论） ----------

    #[test]
    fn parse_comment_event_text_and_conv() {
        let payload = r#"{
            "schema":"2.0",
            "header":{"event_id":"evt_cm_1","event_type":"drive.file.comment.created_v1"},
            "event":{
                "comment_id":"7034abc",
                "file_token":"doxcnXYZ",
                "file_type":"docx",
                "content":[
                    {"type":"at","user_id":"ou_bot","user_name":"agent"},
                    {"type":"text","text":" 帮我总结这份文档"}
                ],
                "sender":{"sender_id":{"open_id":"ou_author"},"sender_type":"user"}
            }
        }"#;
        let (key, comment_id, msg) =
            parse_comment_event(payload.as_bytes(), Some("ou_bot")).expect("评论事件应解析");
        assert_eq!(key, "evt_cm_1");
        // 会话锚放宽：conv 只锚 file_token（同一文档评论共享会话）。
        assert_eq!(msg.conv_id.0, "feishu:comment:doxcnXYZ");
        assert_eq!(msg.sender.0, "ou_author");
        assert_eq!(msg.text.as_deref(), Some(" 帮我总结这份文档"));
        // 回复目标 comment_id 单独返回（drain 登记进锚点表）。
        assert_eq!(comment_id, "7034abc");
        // 新形态 conv 反解：file_token 有、内嵌 comment_id 无。
        let (ft, cid) = comment_target_from_conv(&msg.conv_id).unwrap();
        assert_eq!(ft, "doxcnXYZ");
        assert_eq!(cid, None);
        // 存量形态（内嵌 comment_id）反解兼容。
        let (ft2, cid2) =
            comment_target_from_conv(&ConvId("feishu:comment:doxcnXYZ:7034abc".into())).unwrap();
        assert_eq!(ft2, "doxcnXYZ");
        assert_eq!(cid2.as_deref(), Some("7034abc"));
        assert!(is_comment_event(payload.as_bytes()));
    }

    #[test]
    fn parse_comment_event_ignores_invalid() {
        // 纯 @ 无文字。
        let at_only = br#"{"header":{"event_id":"e","event_type":"drive.file.comment.created_v1"},
            "event":{"comment_id":"c1","file_token":"f1","content":[{"type":"at","user_id":"ou_b"}],"sender":{"sender_id":{"open_id":"ou_a"}}}}"#;
        assert!(parse_comment_event(at_only, Some("ou_bot")).is_none());
        // 缺 file_token。
        let no_token = br#"{"header":{"event_id":"e","event_type":"drive.file.comment.created_v1"},
            "event":{"comment_id":"c1","content":[{"type":"text","text":"hi"}],"sender":{"sender_id":{"open_id":"ou_a"}}}}"#;
        assert!(parse_comment_event(no_token, Some("ou_bot")).is_none());
        // 非目标事件。
        let other = br#"{"header":{"event_type":"im.message.receive_v1"}}"#;
        assert!(parse_comment_event(other, Some("ou_bot")).is_none());
        assert!(!is_comment_event(other));
        // 非评论 conv 反解 None。
        assert!(comment_target_from_conv(&ConvId("feishu:ou_x".into())).is_none());
    }

    /// P5-8：@bot 过滤——bot id 已知时须 @bot 且 sender 非 bot 自身；未知时弱过滤。
    #[test]
    fn parse_comment_event_requires_at_bot() {
        let mk = |content: &str, sender: &str| {
            format!(
                r#"{{"header":{{"event_id":"e","event_type":"drive.file.comment.created_v1"}},
                "event":{{"comment_id":"c1","file_token":"f1","content":{content},
                "sender":{{"sender_id":{{"open_id":"{sender}"}},"sender_type":"user"}}}}}}"#
            )
        };
        let text_node = r#"[{"type":"text","text":"总结一下"}]"#;
        // bot id 已知：无 at 节点 → 拒。
        assert!(parse_comment_event(mk(text_node, "ou_a").as_bytes(), Some("ou_bot")).is_none());
        // bot id 已知：@ 了别人 → 拒。
        let at_other = r#"[{"type":"at","user_id":"ou_other"},{"type":"text","text":"总结"}]"#;
        assert!(parse_comment_event(mk(at_other, "ou_a").as_bytes(), Some("ou_bot")).is_none());
        // bot id 已知：sender 是 bot 自身（自回复）→ 拒。
        let at_bot = r#"[{"type":"at","user_id":"ou_bot"},{"type":"text","text":"收到"}]"#;
        assert!(parse_comment_event(mk(at_bot, "ou_bot").as_bytes(), Some("ou_bot")).is_none());
        // 正常：@bot + 他人 sender → 过。
        assert!(parse_comment_event(mk(at_bot, "ou_a").as_bytes(), Some("ou_bot")).is_some());
        // bot id 未知（弱过滤）：无 at → 拒。
        assert!(parse_comment_event(mk(text_node, "ou_a").as_bytes(), None).is_none());
        // bot id 未知（弱过滤）：有 at（任意）→ 过。
        assert!(parse_comment_event(mk(at_other, "ou_a").as_bytes(), None).is_some());
    }

    /// T13：划词评论的引用片段前置注入——块头在位、原文与用户正文保序；
    /// quote 缺失/空白不注入；`quoted_text` 别名容错；超长截 500 字。
    #[test]
    fn parse_comment_event_quote_injection() {
        let mk = |quote: &str| {
            format!(
                r#"{{"header":{{"event_id":"e","event_type":"drive.file.comment.created_v1"}},
                "event":{{"comment_id":"c1","file_token":"f1","quote":{quote},
                "content":[{{"type":"at","user_id":"ou_bot"}},{{"type":"text","text":"这段啥情况"}}],
                "sender":{{"sender_id":{{"open_id":"ou_a"}},"sender_type":"user"}}}}}}"#
            )
        };
        // 有 quote：块在正文之前，两段都保留。
        let payload = mk(r#""营收环比下降 12%，主因续约延期""#);
        let (_, _, msg) = parse_comment_event(payload.as_bytes(), Some("ou_bot")).unwrap();
        let text = msg.text.as_deref().expect("应有正文");
        let blk = text.find("【评论所在文档片段】").expect("引用块在位");
        let quote = text.find("营收环比下降 12%").expect("引用原文在位");
        let body = text.find("这段啥情况").expect("用户正文在位");
        assert!(blk < quote && quote < body, "块头→引用→正文: {text}");

        // 空白 quote：不注入，正文原样。
        let payload = mk(r#""   ""#);
        let (_, _, msg) = parse_comment_event(payload.as_bytes(), Some("ou_bot")).unwrap();
        assert_eq!(msg.text.as_deref(), Some("这段啥情况"), "空白不注入");

        // quoted_text 别名（形态漂移容错）。
        let alias = r#"{"header":{"event_id":"e","event_type":"drive.file.comment.created_v1"},
            "event":{"comment_id":"c1","file_token":"f1","quoted_text":"别名片段",
            "content":[{"type":"at","user_id":"ou_bot"},{"type":"text","text":"看下"}],
            "sender":{"sender_id":{"open_id":"ou_a"}}}}"#;
        let (_, _, msg) = parse_comment_event(alias.as_bytes(), Some("ou_bot")).unwrap();
        assert!(
            msg.text.as_deref().unwrap_or_default().contains("别名片段"),
            "别名也注入: {:?}",
            msg.text
        );

        // 超长截断：600 字引用 → 注入体恰 500 字（char 边界安全）。
        let long = "长".repeat(600);
        let payload = mk(&format!("{long:?}"));
        let (_, _, msg) = parse_comment_event(payload.as_bytes(), Some("ou_bot")).unwrap();
        let text = msg.text.unwrap();
        let injected = text
            .split("【评论所在文档片段】\n")
            .nth(1)
            .and_then(|rest| rest.split("\n\n").next())
            .expect("应注入引用块");
        assert_eq!(
            injected.chars().count(),
            COMMENT_QUOTE_MAX_CHARS,
            "截到上限"
        );
    }

    // ---------- P6-1：mention 基础设施（@bot 过滤 / 占位剥离 / mentions 元数据） ----------

    /// 构造带 mentions 元数据的群 text 消息 payload（content 须为 JSON 字符串，
    /// 与飞书真实事件形态一致）。
    fn mk_group_mention_payload(event_id: &str, text: &str, mentions: &str) -> Vec<u8> {
        serde_json::json!({
            "header":{"event_id":event_id,"event_type":"im.message.receive_v1"},
            "event":{
                "sender":{"sender_id":{"open_id":"ou_sender"}},
                "message":{
                    "message_type":"text",
                    "content":serde_json::to_string(&serde_json::json!({"text": text})).unwrap(),
                    "chat_type":"group","chat_id":"oc_g1",
                    "mentions":serde_json::from_str::<serde_json::Value>(mentions).unwrap()
                },
                "chat":{"chat_id":"oc_g1"}
            }
        })
        .to_string()
        .into_bytes()
    }

    const BOT_AND_USER_MENTIONS: &str = r#"[
        {"key":"@_user_1","id":{"open_id":"ou_bot"},"name":"agent"},
        {"key":"@_user_2","id":{"open_id":"ou_alice"},"name":"Alice"}
    ]"#;

    /// @bot 剥离 + @他人替换 + mentions 元数据（bot id 已知）。
    #[test]
    fn mention_strip_and_metadata() {
        let p = mk_group_mention_payload(
            "evt_m1",
            "@_user_1 帮我看看 @_user_2 写的代码",
            BOT_AND_USER_MENTIONS,
        );
        let (_k, msg, _) = parse_message_event(&p, &MentionPolicy::REQUIRE_BOT, Some("ou_bot"))
            .expect("@bot 群消息应通过过滤");
        // @bot 占位连同尾随空格剥离；@他人替换为可读 @Alice。
        assert_eq!(msg.text.as_deref(), Some("帮我看看 @Alice 写的代码"));
        // mentions 只含非Bot提及（/allow @Alice 反解用）。
        assert_eq!(msg.mentions.len(), 1);
        assert_eq!(msg.mentions[0].user_id, "ou_alice");
        assert_eq!(msg.mentions[0].name, "Alice");
    }

    /// REQUIRE_BOT 策略：群消息未 @bot → 丢弃；@bot → 通过；p2p 不受限。
    #[test]
    fn group_require_mention_filter() {
        // 无 mentions 的群消息 → 丢。
        let no_at = mk_group_mention_payload("evt_m2", "普通群消息", "[]");
        assert!(parse_message_event(&no_at, &MentionPolicy::REQUIRE_BOT, Some("ou_bot")).is_none());
        // @了别人（非 bot）→ 丢。
        let at_other = mk_group_mention_payload(
            "evt_m3",
            "@_user_2 在吗",
            r#"[{"key":"@_user_2","id":{"open_id":"ou_alice"},"name":"Alice"}]"#,
        );
        assert!(
            parse_message_event(&at_other, &MentionPolicy::REQUIRE_BOT, Some("ou_bot")).is_none()
        );
        // 宽松策略（历史行为）：无 @ 群消息照常通过。
        assert!(parse_permissive(&no_at).is_some());
        // bot id 未知（弱过滤）：@ 了任意人 → 通过（正文占位照常替换）。
        assert!(parse_message_event(&at_other, &MentionPolicy::REQUIRE_BOT, None).is_some());
        // bot id 未知 + 无任何 mention → 丢（弱过滤）。
        assert!(parse_message_event(&no_at, &MentionPolicy::REQUIRE_BOT, None).is_none());
        // p2p 带 @bot 占位：不受过滤，且 bot id 已知时同样剥离。
        let p2p = br#"{
            "header":{"event_id":"evt_m4","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_u"}},
                "message":{"message_type":"text","content":"{\"text\":\"@_user_1 hi\"}","chat_type":"p2p",
                "mentions":[{"key":"@_user_1","id":{"open_id":"ou_bot"},"name":"agent"}]}}
        }"#;
        let (_k, msg, _) = parse_message_event(p2p, &MentionPolicy::REQUIRE_BOT, Some("ou_bot"))
            .expect("p2p 不受 @bot 过滤");
        assert_eq!(msg.text.as_deref(), Some("hi"));
    }

    /// 纯 @bot 无文字：剥离后空文本 → 丢弃（与空文本语义一致）。
    #[test]
    fn mention_only_bot_dropped_as_empty() {
        let p = mk_group_mention_payload(
            "evt_m5",
            "@_user_1",
            r#"[{"key":"@_user_1","id":{"open_id":"ou_bot"},"name":"agent"}]"#,
        );
        assert!(parse_message_event(&p, &MentionPolicy::REQUIRE_BOT, Some("ou_bot")).is_none());
    }

    /// post 的 at 节点：@bot 剔除、@他人渲染 @名字 并进 mentions。
    #[test]
    fn post_at_nodes() {
        // content 是 JSON 字符串，值内不得跨真实换行（非法控制字符），单行构造。
        let content = serde_json::to_string(&serde_json::json!({
            "content": [[
                {"tag":"at","user_id":"ou_bot","user_name":"agent"},
                {"tag":"at","user_id":"ou_alice","user_name":"Alice"},
                {"tag":"text","text":"看看这段"}
            ]]
        }))
        .unwrap();
        let payload = serde_json::json!({
            "header":{"event_id":"evt_m6","event_type":"im.message.receive_v1"},
            "event":{
                "sender":{"sender_id":{"open_id":"ou_u"}},
                "message":{"message_type":"post","chat_type":"p2p","content":content}
            }
        })
        .to_string()
        .into_bytes();
        let (_k, msg, pending) =
            parse_message_event(&payload, &MentionPolicy::PERMISSIVE, Some("ou_bot"))
                .expect("post at 节点应解析");
        // @bot 剔除、@Alice 渲染为文本、正文节点保留。
        assert_eq!(msg.text.as_deref(), Some("@Alice\n看看这段"));
        assert!(pending.is_empty());
        assert_eq!(msg.mentions.len(), 1);
        assert_eq!(msg.mentions[0].user_id, "ou_alice");
    }

    /// is_group_message_event 谓词：群消息 true，p2p / 评论 / 非法 JSON false。
    #[test]
    fn group_message_event_predicate() {
        let group = mk_group_mention_payload("e", "x", "[]");
        assert!(is_group_message_event(&group));
        let p2p = br#"{"header":{"event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_u"}},"message":{"message_type":"text","content":"{\"text\":\"x\"}","chat_type":"p2p"}}}"#;
        assert!(!is_group_message_event(p2p));
        assert!(!is_group_message_event(b"not json"));
        let comment = br#"{"header":{"event_type":"drive.file.comment.created_v1"}}"#;
        assert!(!is_group_message_event(comment));
    }

    // ---------- P6-4：话题群（thread）会话隔离 ----------

    /// 话题群消息（group + root_id）→ conv 升级为 `feishu:<chat>:<root>`；
    /// 普通群（无 root_id）conv 不变；send 反解取首段。
    #[test]
    fn thread_conv_isolation() {
        let mk = |root: Option<&str>| {
            let mut message = serde_json::json!({
                "message_type":"text",
                "content":"{\"text\":\"话题消息\"}",
                "chat_type":"group","chat_id":"oc_g1",
                "message_id":"om_child"
            });
            if let Some(r) = root {
                message["root_id"] = serde_json::json!(r);
            }
            serde_json::json!({
                "header":{"event_id":"evt_t","event_type":"im.message.receive_v1"},
                "event":{
                    "sender":{"sender_id":{"open_id":"ou_s"}},
                    "message":message,
                    "chat":{"chat_id":"oc_g1"}
                }
            })
            .to_string()
            .into_bytes()
        };
        // 话题群：conv = feishu:oc_g1:om_root1（独立 session 锚点）。
        let (_k, msg, _) =
            parse_message_event(&mk(Some("om_root1")), &MentionPolicy::PERMISSIVE, None)
                .expect("话题消息应解析");
        assert_eq!(msg.conv_id.0, "feishu:oc_g1:om_root1");
        // 普通群：无 root_id → conv 不变。
        let (_k, msg, _) =
            parse_message_event(&mk(None), &MentionPolicy::PERMISSIVE, None).expect("普通群应解析");
        assert_eq!(msg.conv_id.0, "feishu:oc_g1");
        // 反解 roundtrip：话题 conv → 发送目标取首段 chat_id。
        let (id, kind) = receive_target_from_conv(&ConvId("feishu:oc_g1:om_root1".into())).unwrap();
        assert_eq!(id, "oc_g1");
        assert_eq!(kind, ReceiveIdKind::ChatId);
        // 话题反解：命中 / 非 om_ 前缀不命中（评论 conv 天然排除）。
        assert_eq!(
            thread_target_from_conv(&ConvId("feishu:oc_g1:om_root1".into())),
            Some(("oc_g1".into(), "om_root1".into()))
        );
        assert!(thread_target_from_conv(&ConvId("feishu:oc_g1".into())).is_none());
        assert!(
            thread_target_from_conv(&ConvId("feishu:comment:dox:c1".into())).is_none(),
            "评论 conv 第二段非 om_ 前缀，不应误判为话题"
        );
    }

    /// P7-A3：mentioned_bot——群消息 @bot（bot id 已知）为 true；@ 他人 / p2p /
    /// bot id 未知为 false。
    #[test]
    fn mentioned_bot_flag_semantics() {
        let mk = |chat_type: &str, mentions: &str| {
            serde_json::json!({
                "header":{"event_id":"e","event_type":"im.message.receive_v1"},
                "event":{
                    "sender":{"sender_id":{"open_id":"ou_s"}},
                    "message":{
                        "message_type":"text",
                        "content":"{\"text\":\"x\"}",
                        "chat_type":chat_type,"chat_id":"oc_g1",
                        "mentions":serde_json::from_str::<serde_json::Value>(mentions).unwrap()
                    },
                    "chat":{"chat_id":"oc_g1"}
                }
            })
            .to_string()
            .into_bytes()
        };
        let at_bot = r#"[{"key":"@_user_1","id":{"open_id":"ou_bot"},"name":"agent"}]"#;
        let at_other = r#"[{"key":"@_user_1","id":{"open_id":"ou_x"},"name":"x"}]"#;
        // 群 + @bot → true。
        let (_, m, _) = parse_message_event(
            &mk("group", at_bot),
            &MentionPolicy::PERMISSIVE,
            Some("ou_bot"),
        )
        .expect("应解析");
        assert!(m.mentioned_bot, "群 @bot 应为 true");
        // 群 + @他人 → false。
        let (_, m, _) = parse_message_event(
            &mk("group", at_other),
            &MentionPolicy::PERMISSIVE,
            Some("ou_bot"),
        )
        .expect("应解析");
        assert!(!m.mentioned_bot, "群 @他人应为 false");
        // p2p（即使提及里有 bot 形态）→ false：陌生人提示仅限群。
        let (_, m, _) = parse_message_event(
            &mk("p2p", at_bot),
            &MentionPolicy::PERMISSIVE,
            Some("ou_bot"),
        )
        .expect("应解析");
        assert!(!m.mentioned_bot, "p2p 恒 false");
        // bot id 未知 → false（宁可漏发不可误发）。
        let (_, m, _) = parse_message_event(&mk("group", at_bot), &MentionPolicy::PERMISSIVE, None)
            .expect("应解析");
        assert!(!m.mentioned_bot, "bot id 未知应为 false");
    }

    // ---------- 交互流/安全批次：按钮校验、话题免 @、不支持类型提示 ----------

    /// 命令按钮过期：ts 超 24h 窗口 → deny 提示且不产生命令文本；窗口内 / 无 ts
    /// （存量卡兼容）→ 正常回调。
    #[test]
    fn command_button_expiry() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mk = |ts: Option<i64>, conv: &str| {
            let mut value = serde_json::json!({ "imagent_cmd": "/ws use main", "conv": conv });
            if let Some(t) = ts {
                value["ts"] = serde_json::json!(t);
            }
            serde_json::json!({
                "header":{"event_id":"evt_exp","event_type":"card.action.trigger"},
                "event":{"operator":{"open_id":"ou_op"},"action":{"tag":"button","value":value}}
            })
            .to_string()
            .into_bytes()
        };
        // 过期（25h 前）→ deny。
        let (_, msg, deny) =
            parse_card_action_event(&mk(Some(now - CMD_BUTTON_TTL_SECS - 3600), "feishu:ou_op"))
                .expect("应解析");
        assert!(
            deny.as_deref().is_some_and(|d| d.contains("已过期")),
            "{deny:?}"
        );
        assert!(msg.text.is_none(), "过期不得注入命令文本");
        // 窗口内（1h 前）→ 正常。
        let (_, msg, deny) =
            parse_card_action_event(&mk(Some(now - 3600), "feishu:ou_op")).expect("应解析");
        assert!(deny.is_none());
        assert_eq!(msg.text.as_deref(), Some("/ws use main"));
        // 无 ts（存量卡）→ 兼容放行。
        let (_, msg, deny) = parse_card_action_event(&mk(None, "feishu:ou_op")).expect("应解析");
        assert!(deny.is_none());
        assert_eq!(msg.text.as_deref(), Some("/ws use main"));
    }

    /// 命令按钮发起者校验（v13-P2 全形态）：operator ≠ value.sender → deny（群
    /// 与私聊 conv 同规则——卡片可被转发到任意会话，点击者身份与 conv 形态
    /// 无关）；发起者本人 / 无 sender（旧卡兼容）→ 放行。
    #[test]
    fn command_button_sender_guard() {
        let mk = |sender: Option<&str>, conv: &str, operator: &str| {
            let mut value = serde_json::json!({ "imagent_cmd": "/stop", "conv": conv });
            if let Some(s) = sender {
                value["sender"] = serde_json::json!(s);
            }
            serde_json::json!({
                "header":{"event_id":"evt_snd","event_type":"card.action.trigger"},
                "event":{"operator":{"open_id":operator},"action":{"tag":"button","value":value}}
            })
            .to_string()
            .into_bytes()
        };
        // 群 conv + 他人点击 → deny。
        let (_, _, deny) =
            parse_card_action_event(&mk(Some("ou_owner"), "feishu:oc_g", "ou_other"))
                .expect("应解析");
        assert!(
            deny.as_deref().is_some_and(|d| d.contains("仅发起者")),
            "{deny:?}"
        );
        // 群 conv + 发起者本人 → 放行。
        let (_, msg, deny) =
            parse_card_action_event(&mk(Some("ou_owner"), "feishu:oc_g", "ou_owner"))
                .expect("应解析");
        assert!(deny.is_none());
        assert_eq!(msg.text.as_deref(), Some("/stop"));
        // 私聊 conv（转发场景：卡片被转发给他人，按钮 value 仍编码原会话与
        // 发起者）+ 他人点击 → 同样 deny——v13-P2 起私聊不再免检。
        let (_, _, deny) =
            parse_card_action_event(&mk(Some("ou_owner"), "feishu:ou_owner", "ou_other"))
                .expect("应解析");
        assert!(
            deny.as_deref().is_some_and(|d| d.contains("仅发起者")),
            "私聊 conv 同样校验: {deny:?}"
        );
        // 私聊 conv + 发起者本人 → 放行（正常路径）。
        let (_, msg, deny) =
            parse_card_action_event(&mk(Some("ou_owner"), "feishu:ou_owner", "ou_owner"))
                .expect("应解析");
        assert!(deny.is_none());
        assert_eq!(msg.text.as_deref(), Some("/stop"));
        // 话题群 conv 同样属群形态（非 ou_ 前缀）。
        assert!(!is_private_conv("feishu:oc_g:om_root"));
        assert!(is_private_conv("feishu:ou_x"));
        // 无 sender（旧卡）→ 不校验（兼容放行，群/私聊同规则）。
        let (_, _, deny) =
            parse_card_action_event(&mk(None, "feishu:oc_g", "ou_any")).expect("应解析");
        assert!(deny.is_none(), "旧卡兼容: {deny:?}");
        let (_, _, deny) =
            parse_card_action_event(&mk(None, "feishu:ou_x", "ou_any")).expect("应解析");
        assert!(deny.is_none(), "私聊旧卡兼容: {deny:?}");
    }

    /// conv 前缀校验：value.conv 无 feishu: 前缀（伪造/跨平台）→ None。
    #[test]
    fn card_action_conv_prefix_guard() {
        let payload = serde_json::json!({
            "header":{"event_id":"e","event_type":"card.action.trigger"},
            "event":{"operator":{"open_id":"ou_op"},"action":{"value":{"imagent_perm":"allow","conv":"wecom:evil"}}}
        })
        .to_string()
        .into_bytes();
        assert!(parse_card_action_event(&payload).is_none());
    }

    /// 话题键提取：group + om_ root → feishu:<chat>:<root>；普通群 / p2p / 非
    /// 消息事件 → None。
    #[test]
    fn thread_key_extraction() {
        let mk = |chat_type: &str, root: Option<&str>, chat_id: &str| {
            let mut message = serde_json::json!({
                "message_type":"text","content":"{\"text\":\"x\"}",
                "chat_type":chat_type,"chat_id":chat_id
            });
            if let Some(r) = root {
                message["root_id"] = serde_json::json!(r);
            }
            serde_json::json!({
                "header":{"event_type":"im.message.receive_v1"},
                "event":{"sender":{"sender_id":{"open_id":"ou_s"}},"message":message,
                    "chat":{"chat_id":chat_id}}
            })
            .to_string()
            .into_bytes()
        };
        assert_eq!(
            thread_key_of_payload(&mk("group", Some("om_root1"), "oc_g1")).as_deref(),
            Some("feishu:oc_g1:om_root1")
        );
        // 普通群（无 root）→ None（不豁免 @）。
        assert!(thread_key_of_payload(&mk("group", None, "oc_g1")).is_none());
        // 非 om_ 前缀 root（如评论形态误投）→ None。
        assert!(thread_key_of_payload(&mk("group", Some("xx_root"), "oc_g1")).is_none());
        // p2p → None。
        assert!(thread_key_of_payload(&mk("p2p", Some("om_root1"), "")).is_none());
    }

    /// 不支持类型提示：audio / share_chat / share_user 给文案 + 去重键 + 回执目标
    ///（v1.18 收紧：仅 p2p 回执——群内一律不提示，防非白名单群触发 bot 回复；
    /// 返回 message_id 供 drain 侧 dedup，事件重投不重复回执）；其它类型 None。
    #[test]
    fn unsupported_message_notice_kinds() {
        let mk = |mt: &str, chat_type: &str, mentions: &str| {
            serde_json::json!({
                "header":{"event_type":"im.message.receive_v1"},
                "event":{
                    "sender":{"sender_id":{"open_id":"ou_u"}},
                    "message":{"message_type":mt,"content":"{}","chat_type":chat_type,
                        "message_id":"om_n1",
                        "chat_id":"oc_g","mentions":serde_json::from_str::<serde_json::Value>(mentions).unwrap()},
                    "chat":{"chat_id":"oc_g"}
                }
            })
            .to_string()
            .into_bytes()
        };
        // W3-1：audio 已支持（走转写路径），不再出现在提示清单。
        assert!(unsupported_message_notice(&mk("audio", "p2p", "[]")).is_none());
        let (notice, key, conv) =
            unsupported_message_notice(&mk("share_chat", "p2p", "[]")).expect("群名片应提示");
        assert!(notice.contains("分享卡片"), "{notice}");
        assert_eq!(conv.unwrap().0, "feishu:ou_u");
        assert_eq!(key.as_deref(), Some("om_n1"), "去重键 = message_id");
        let (notice, _, _) =
            unsupported_message_notice(&mk("share_user", "p2p", "[]")).expect("用户名片应提示");
        assert!(notice.contains("名片"), "{notice}");
        // 群 + 带 @ → 不提示（v1.18 收紧：白名单在 core 侧，群内弱门槛会打搅
        // 非白名单群）。
        assert!(unsupported_message_notice(&mk(
            "share_chat",
            "group",
            r#"[{"key":"@_user_1","id":{"open_id":"ou_bot"}}]"#
        ))
        .is_none());
        // 群 + 无 @ → 不提示（消息本不会送达处理）。
        assert!(unsupported_message_notice(&mk("share_chat", "group", "[]")).is_none());
        // 支持的类型 → None。
        assert!(unsupported_message_notice(&mk("text", "p2p", "[]")).is_none());
        // image 消息也不提示（有专门处理路径）。
        assert!(unsupported_message_notice(&mk("image", "p2p", "[]")).is_none());
    }

    /// 快赢：合并转发 / 表情包 / 视频（merged_forward / sticker / media / video）
    /// 给统一提示「暂不支持……请直接发文字或截图」。合并转发已完整支持，本提示
    /// 仅作**回退兜底**（事件缺 message_id 时，见
    /// parse_merged_forward_event_missing_id_falls_to_notice）；表情包/视频仍是
    /// 主路径提示。
    #[test]
    fn unsupported_message_notice_rich_media_kinds() {
        let mk = |mt: &str| {
            serde_json::json!({
                "header":{"event_type":"im.message.receive_v1"},
                "event":{
                    "sender":{"sender_id":{"open_id":"ou_u"}},
                    "message":{"message_type":mt,"content":"{}","chat_type":"p2p",
                        "message_id":"om_rich1"}
                }
            })
            .to_string()
            .into_bytes()
        };
        for mt in ["merged_forward", "sticker", "media", "video"] {
            let (notice, key, conv) =
                unsupported_message_notice(&mk(mt)).unwrap_or_else(|| panic!("{mt} 应提示"));
            assert!(
                notice.contains("暂不支持合并转发/表情包/视频消息"),
                "{mt}: {notice}"
            );
            assert!(notice.contains("发文字或截图"), "{mt}: {notice}");
            assert_eq!(conv.unwrap().0, "feishu:ou_u");
            assert_eq!(key.as_deref(), Some("om_rich1"));
        }
    }

    /// Bug：post 富文本 a 节点（超链接）不再丢弃——渲染 `[text](href)`；无 text
    /// 或 text==href 给裸 href；media/emotion 节点给占位（agent 知道有视频/表情）。
    #[test]
    fn post_link_and_media_nodes() {
        let content = serde_json::to_string(&serde_json::json!({
            "content": [[
                {"tag":"a","text":"总结这个链接","href":"https://example.com/doc"},
                {"tag":"text","text":"和"},
                {"tag":"a","text":"https://bare.example.com","href":"https://bare.example.com"},
                {"tag":"a","href":"https://no-text.example.com"},
                {"tag":"media","file_key":"file_v3_video"},
                {"tag":"emotion","emoji_type":"SMILE"},
                {"tag":"text","text":"完"}
            ]]
        }))
        .unwrap();
        let payload = serde_json::json!({
            "header":{"event_id":"evt_a","event_type":"im.message.receive_v1"},
            "event":{
                "sender":{"sender_id":{"open_id":"ou_u"}},
                "message":{"message_type":"post","chat_type":"p2p","content":content}
            }
        })
        .to_string()
        .into_bytes();
        let (_k, msg, pending) = parse_permissive(&payload).expect("post 带 a 节点应解析成功");
        let text = msg.text.as_deref().expect("应有正文");
        assert!(
            text.contains("[总结这个链接](https://example.com/doc)"),
            "a 节点渲染 markdown 链接: {text}"
        );
        // text==href 不重复（防 [url](url) 冗余）；无 text 用 href 本体。
        assert!(
            text.contains("https://bare.example.com")
                && !text.contains("[https://bare.example.com](https://bare.example.com)"),
            "url==text 给裸 url: {text}"
        );
        assert!(
            text.contains("https://no-text.example.com"),
            "无 text 用 href: {text}"
        );
        assert!(text.contains("[视频]"), "media 占位: {text}");
        assert!(text.contains("[表情]"), "emotion 占位: {text}");
        assert!(pending.is_empty(), "media/emotion 不进待下载图片列表");
    }

    // ---------- 合并转发消息（merged_forward）完整支持 ----------

    /// 构造 merged_forward 消息事件 payload bytes（content 为「JSON 字符串」字段，
    /// 与飞书真实事件形态一致——可能是合法 JSON，也可能是占位文本；mentions 仅
    /// 群消息需要）。
    fn mk_merged_forward_payload(
        event_id: &str,
        message_id: Option<&str>,
        content: &str,
        chat_type: &str,
        mentions: &str,
    ) -> Vec<u8> {
        let mut message = serde_json::json!({
            "message_type": "merged_forward",
            "content": content,
            "chat_type": chat_type,
            "chat_id": "oc_g1"
        });
        if let Some(m) = message_id {
            message["message_id"] = serde_json::json!(m);
        }
        if !mentions.is_empty() {
            message["mentions"] = serde_json::from_str::<serde_json::Value>(mentions).unwrap();
        }
        serde_json::json!({
            "header": {"event_id": event_id, "event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_id": {"open_id": "ou_fwd"}},
                "message": message,
                "chat": {"chat_id": "oc_g1"}
            }
        })
        .to_string()
        .into_bytes()
    }

    /// 构造子消息条目（转录测试用）。
    fn mf_item(
        mt: &str,
        content: &str,
        name: Option<&str>,
        id: &str,
        ms: i64,
    ) -> MergedForwardItem {
        MergedForwardItem {
            message_id: format!("om_sub_{mt}"),
            message_type: mt.to_string(),
            content: content.to_string(),
            sender_id: id.to_string(),
            sender_name: name.map(String::from),
            create_time_ms: ms,
        }
    }

    /// p2p merged_forward：正常产出（占位正文 + meta 带 content JSON 的
    /// title/summary）；content 为占位文本（非法 JSON）时 meta 头字段回退 None。
    #[test]
    fn parse_merged_forward_event_p2p_with_head_meta() {
        let content = r#"{"title":"周五排期讨论","summary":"3 条"}"#;
        let p = mk_merged_forward_payload("evt_mf1", Some("om_mf1"), content, "p2p", "");
        // 普通消息分支不收 merged_forward（完整支持在专用解析）。
        assert!(parse_message_event(&p, &MentionPolicy::PERMISSIVE, None).is_none());
        let (key, msg, meta) = parse_merged_forward_event(&p, &MentionPolicy::REQUIRE_BOT, None)
            .expect("p2p merged_forward 应产出");
        assert_eq!(key, "evt_mf1");
        assert_eq!(meta.message_id, "om_mf1");
        assert_eq!(meta.title.as_deref(), Some("周五排期讨论"));
        assert_eq!(meta.summary.as_deref(), Some("3 条"));
        assert_eq!(msg.conv_id.0, "feishu:ou_fwd");
        assert_eq!(msg.sender.0, "ou_fwd");
        assert_eq!(msg.text.as_deref(), Some(MERGE_FORWARD_PLACEHOLDER));
        assert_eq!(msg.source_msg_id.as_deref(), Some("om_mf1"));
        assert!(!msg.mentioned_bot, "p2p 恒 false");

        // 占位文本 content（v1.12.0 观察到的形态）：meta 头字段 None，仍正常产出。
        let p2 = mk_merged_forward_payload(
            "evt_mf2",
            Some("om_mf2"),
            "Merged and Forwarded Message",
            "p2p",
            "",
        );
        let (_, msg2, meta2) = parse_merged_forward_event(&p2, &MentionPolicy::PERMISSIVE, None)
            .expect("占位 content 应产出");
        assert_eq!(meta2.message_id, "om_mf2");
        assert_eq!(meta2.title, None);
        assert_eq!(meta2.summary, None);
        assert_eq!(msg2.text.as_deref(), Some(MERGE_FORWARD_PLACEHOLDER));
    }

    /// 群内仍要求 @bot：REQUIRE_BOT 下未 @bot → None；@bot → 通过。
    #[test]
    fn parse_merged_forward_event_group_mention_gate() {
        let content = r#"{"title":"t"}"#;
        let no_at = mk_merged_forward_payload("evt_mf3", Some("om_mf3"), content, "group", "[]");
        assert!(
            parse_merged_forward_event(&no_at, &MentionPolicy::REQUIRE_BOT, Some("ou_bot"))
                .is_none(),
            "群消息未 @bot 应丢弃"
        );
        let at_bot = mk_merged_forward_payload(
            "evt_mf3",
            Some("om_mf3"),
            content,
            "group",
            r#"[{"key":"@_user_1","id":{"open_id":"ou_bot"},"name":"agent"}]"#,
        );
        let (key, msg, meta) =
            parse_merged_forward_event(&at_bot, &MentionPolicy::REQUIRE_BOT, Some("ou_bot"))
                .expect("群 @bot 应通过");
        assert_eq!(key, "evt_mf3");
        assert_eq!(msg.conv_id.0, "feishu:oc_g1");
        assert_eq!(meta.message_id, "om_mf3");
        assert!(msg.mentioned_bot, "@bot 元数据应置位");
    }

    /// 缺 message_id（无法拉子消息）→ None，走 unsupported_message_notice 的兜底
    /// 提示（「解析失败回退现状提示」）；非 merged_forward 类型不误收。
    #[test]
    fn parse_merged_forward_event_missing_id_falls_to_notice() {
        let p = mk_merged_forward_payload("evt_mf4", None, "{}", "p2p", "");
        assert!(parse_merged_forward_event(&p, &MentionPolicy::PERMISSIVE, None).is_none());
        let (notice, key, conv) = unsupported_message_notice(&p).expect("缺 message_id 应回退提示");
        assert!(notice.contains("暂不支持合并转发"), "{notice}");
        assert_eq!(conv.unwrap().0, "feishu:ou_fwd");
        // 去重键回退到 header event_id（重投事件同 id，drain 侧 dedup 仍有效）。
        assert_eq!(key.as_deref(), Some("evt_mf4"));
        // 非目标事件 / 非 merged_forward 消息类型 → None。
        let text_payload = br#"{"header":{"event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"text","content":"{\"text\":\"hi\"}","chat_type":"p2p","message_id":"om_t"}}}"#;
        assert!(
            parse_merged_forward_event(text_payload, &MentionPolicy::PERMISSIVE, None).is_none()
        );
    }

    /// 转录：类型映射（text 原文/图片/表情/文件带名与缺名/视频/卡片/嵌套合并
    /// 转发/未知）、发送者标识（name / id 后 8 位 / 未知）、时间 HH:MM 与缺省。
    #[test]
    fn render_transcript_type_and_sender_mapping() {
        use chrono::TimeZone;
        // 本地时区构造 09:05 → 格式化必为 "09:05"（构造与渲染同为 Local，确定性）。
        let ms = chrono::Local
            .with_ymd_and_hms(2026, 8, 28, 9, 5, 0)
            .unwrap()
            .timestamp_millis();
        let items = vec![
            mf_item("text", r#"{"text":"文本内容"}"#, Some("Alice"), "ou_a", ms),
            mf_item(
                "image",
                r#"{"image_key":"img_v3_x"}"#,
                None,
                "ou_bbbbbbbb9999",
                ms + 60_000,
            ),
            mf_item("sticker", "{}", None, "ou_c", 0),
            mf_item("emotion", "{}", None, "ou_c", 0),
            mf_item(
                "file",
                r#"{"file_key":"file_v3_1","file_name":"报告.pdf"}"#,
                Some("Bob"),
                "ou_d",
                0,
            ),
            mf_item(
                "file",
                r#"{"file_key":"file_v3_2"}"#,
                Some("Bob"),
                "ou_d",
                0,
            ),
            mf_item("media", "{}", Some("Bob"), "ou_d", 0),
            mf_item("video", "{}", Some("Bob"), "ou_d", 0),
            mf_item("interactive", "{}", Some("Bob"), "ou_d", 0),
            mf_item("merged_forward", "{}", Some("Bob"), "ou_d", 0),
            // 真机校准（2026-09-29）：拉取 API 的嵌套合并转发是单数
            // merge_forward（事件侧才是复数）——两种都映射嵌套占位。
            mf_item("merge_forward", "{}", Some("Bob"), "ou_d", 0),
            mf_item("audio", "{}", Some("Bob"), "ou_d", 0),
            mf_item("unknown_kind", "{}", None, "", 0),
        ];
        let t = render_merge_forward_transcript(&items, None, None);
        assert!(t.starts_with("【合并转发聊天记录】共 13 条"), "{t}");
        assert!(t.contains("[Alice 09:05] 文本内容"), "{t}");
        // 无 name → id 后 8 位；时间正常（ms+60s → 09:06）。
        assert!(t.contains("[bbbb9999 09:06] [图片]"), "{t}");
        // create_time 缺失（0）→ 省略时间段。
        assert!(t.contains("[ou_c] [表情]"), "{t}");
        assert!(t.contains("[Bob] [文件: 报告.pdf]"), "{t}");
        assert!(t.contains("[Bob] [文件]"), "{t}");
        assert!(t.contains("[Bob] [视频]"), "{t}");
        assert!(t.contains("[Bob] [卡片消息]"), "{t}");
        assert!(t.contains("[Bob] [合并转发消息（嵌套）]"), "{t}");
        assert!(t.contains("[Bob] [未知类型消息]"), "{t}");
        assert!(t.contains("[未知] [未知类型消息]"), "id 全缺 → 未知: {t}");
        assert!(!t.contains("已截断"), "未超限不应有截断标注: {t}");

        // title/summary 头：title 优先于条数；summary 单独一行。
        let t2 = render_merge_forward_transcript(&items[..1], Some("周五排期"), Some("3 条"));
        assert!(
            t2.starts_with("【合并转发聊天记录】周五排期\n3 条\n"),
            "{t2}"
        );

        // 空列表：仅头。
        let t3 = render_merge_forward_transcript(&[], None, None);
        assert_eq!(t3, "【合并转发聊天记录】共 0 条");
    }

    /// 转录：post 子消息复用 post→文本逻辑（文字优先、纯图占位、空 post 占位）；
    /// text 子消息的 @_user_N 占位保留原样（无 mentions 元数据，见取舍注释）。
    #[test]
    fn render_transcript_post_and_at_placeholder() {
        let post_text = serde_json::to_string(&serde_json::json!({
            "title": "周报",
            "content": [[{"tag": "text", "text": "本周进展"}]]
        }))
        .unwrap();
        let post_img_only = serde_json::to_string(&serde_json::json!({
            "content": [[{"tag": "img", "image_key": "img_only"}]]
        }))
        .unwrap();
        let post_empty = serde_json::to_string(&serde_json::json!({ "content": [] })).unwrap();
        let items = vec![
            mf_item("post", &post_text, Some("Alice"), "ou_a", 0),
            mf_item("post", &post_img_only, Some("Alice"), "ou_a", 0),
            mf_item("post", &post_empty, Some("Alice"), "ou_a", 0),
            mf_item(
                "text",
                r#"{"text":"@_user_1 看这个"}"#,
                Some("Bob"),
                "ou_b",
                0,
            ),
        ];
        let t = render_merge_forward_transcript(&items, None, None);
        assert!(
            t.contains("[Alice] 周报\n[Alice] 本周进展") || t.contains("[Alice] 周报"),
            "{t}"
        );
        assert!(t.contains("本周进展"), "post 文字进转录: {t}");
        assert!(t.contains("[Alice] [图片]"), "纯图 post 占位: {t}");
        assert!(t.contains("[Alice] [富文本消息]"), "空 post 占位: {t}");
        assert!(t.contains("[Bob] @_user_1 看这个"), "占位保留原样: {t}");
    }

    /// v1.25.2：不可读媒体（图片/文件等）计数 + 末尾引导标注；纯文本记录不加。
    #[test]
    fn render_transcript_media_hint() {
        let items = vec![
            mf_item("text", r#"{"text":"看下这个设计"}"#, Some("A"), "ou_a", 0),
            mf_item("image", r#"{"image_key":"img_v3_x"}"#, Some("A"), "ou_a", 0),
            mf_item(
                "file",
                r#"{"file_key":"f1","file_name":"a.pdf"}"#,
                Some("B"),
                "ou_b",
                0,
            ),
        ];
        let t = render_merge_forward_transcript(&items, None, None);
        assert!(t.contains("[图片]"), "{t}");
        assert!(
            t.contains("另有 2 条图片/文件") && t.contains("单独发送该图"),
            "媒体引导缺失: {t}"
        );
        // 纯文本记录：无引导行。
        let text_only = vec![mf_item(
            "text",
            r#"{"text":"纯文字"}"#,
            Some("A"),
            "ou_a",
            0,
        )];
        let t2 = render_merge_forward_transcript(&text_only, None, None);
        assert!(!t2.contains("图片/文件"), "{t2}");
    }

    /// 转录截断保护：超 8000 字符按字符边界截断，尾部标注「（已截断，共 N 条中
    /// 前 M 条）」；未超限无标注；首条超长也硬截保留一条。
    #[test]
    fn render_transcript_truncation() {
        let long = "很".repeat(4_000);
        let items = vec![
            mf_item(
                "text",
                &format!(r#"{{"text":"{long}"}}"#),
                Some("A"),
                "ou_a",
                0,
            ),
            mf_item(
                "text",
                &format!(r#"{{"text":"{long}"}}"#),
                Some("B"),
                "ou_b",
                0,
            ),
        ];
        let t = render_merge_forward_transcript(&items, None, None);
        assert!(t.contains("（已截断，共 2 条中前 1 条）"), "{t}");
        // 截断后主体（去标注行）不超过上限；标注行本身是元信息不计入。
        let body = t.split("\n（已截断").next().unwrap();
        assert!(
            body.chars().count() <= MERGE_FORWARD_TRANSCRIPT_MAX,
            "{}",
            body.chars().count()
        );

        // 未超限：无标注。
        let short = vec![mf_item("text", r#"{"text":"短"}"#, Some("A"), "ou_a", 0)];
        let t2 = render_merge_forward_transcript(&short, None, None);
        assert!(!t2.contains("已截断"), "{t2}");

        // 单条超长（首条即超限）：硬截保留 1 条 + 标注。
        let huge = "长".repeat(20_000);
        let one = vec![mf_item(
            "text",
            &format!(r#"{{"text":"{huge}"}}"#),
            None,
            "ou_x",
            0,
        )];
        let t3 = render_merge_forward_transcript(&one, None, None);
        assert!(t3.contains("（已截断，共 1 条中前 1 条）"), "{t3}");
        let body3 = t3.split("\n（已截断").next().unwrap();
        assert!(
            body3.chars().count() <= MERGE_FORWARD_TRANSCRIPT_MAX,
            "{}",
            body3.chars().count()
        );
    }

    /// 安全（转发代批）：带 sender 的审批/问题/表单按钮 value，**全形态**（含私聊
    /// conv）校验 operator==sender；不符回「该询问由 X 发起，仅其本人可答复」，
    /// 不注入 y/n/ask 文本；占位消息 sender=operator（drain 据此私聊反馈）。
    #[test]
    fn ask_button_forward_proxy_denied_all_conv_forms() {
        let mk = |conv: &str, operator: &str, sender: Option<&str>| {
            let mut value =
                serde_json::json!({ "imagent_perm": "allow", "conv": conv, "req": "r1" });
            if let Some(s) = sender {
                value["sender"] = serde_json::json!(s);
            }
            serde_json::json!({
                "header":{"event_id":"evt_fwd","event_type":"card.action.trigger"},
                "event":{"operator":{"open_id":operator},"action":{"tag":"button","value":value}}
            })
            .to_string()
            .into_bytes()
        };
        // 群 conv：他人点击 → deny。
        let (_, msg, deny) =
            parse_card_action_event(&mk("feishu:oc_g", "ou_other", Some("ou_owner")))
                .expect("应解析");
        assert!(
            deny.as_deref()
                .is_some_and(|d| d.contains("仅其本人可答复")),
            "{deny:?}"
        );
        assert!(msg.text.is_none(), "代批不得注入审批文本");
        assert_eq!(
            msg.sender.0, "ou_other",
            "占位消息携带 operator（私聊反馈用）"
        );
        // 私聊 conv（转发场景）：他人形态同样 deny——全形态校验。
        let (_, _, deny) =
            parse_card_action_event(&mk("feishu:ou_owner", "ou_other", Some("ou_owner")))
                .expect("应解析");
        assert!(
            deny.as_deref()
                .is_some_and(|d| d.contains("仅其本人可答复")),
            "私聊 conv 同样校验: {deny:?}"
        );
        // 发起者本人 → 放行（正常路径）。
        let (_, msg, deny) =
            parse_card_action_event(&mk("feishu:oc_g", "ou_owner", Some("ou_owner")))
                .expect("应解析");
        assert!(deny.is_none());
        assert_eq!(msg.text.as_deref(), Some("y"));
        // 存量卡（无 sender）→ 兼容放行。
        let (_, msg, deny) =
            parse_card_action_event(&mk("feishu:oc_g", "ou_any", None)).expect("应解析");
        assert!(deny.is_none(), "存量卡兼容: {deny:?}");
        assert_eq!(msg.text.as_deref(), Some("y"));
    }

    /// 安全（转发代批）：问题卡选项按钮（imagent_ask）与表单（imagent_form=ask）
    /// 的 sender 校验同审批按钮；带 ts 的过期拒绝（24h 同命令按钮）。
    #[test]
    fn ask_button_expiry_and_question_form_guard() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        // 问题卡选项按钮：他人点击（私聊 conv）→ deny。
        let ask_btn = serde_json::json!({
            "header":{"event_id":"e1","event_type":"card.action.trigger"},
            "event":{"operator":{"open_id":"ou_x"},"action":{"tag":"button","value":{
                "imagent_ask":"方案A","conv":"feishu:ou_owner","req":"rq","sender":"ou_owner"
            }}}
        })
        .to_string()
        .into_bytes();
        let (_, _, deny) = parse_card_action_event(&ask_btn).expect("应解析");
        assert!(
            deny.as_deref()
                .is_some_and(|d| d.contains("仅其本人可答复")),
            "{deny:?}"
        );
        // 审批按钮 ts 过期（25h 前）→ deny 过期文案，不注入文本。
        let expired = serde_json::json!({
            "header":{"event_id":"e2","event_type":"card.action.trigger"},
            "event":{"operator":{"open_id":"ou_owner"},"action":{"tag":"button","value":{
                "imagent_perm":"allow","conv":"feishu:oc_g","req":"r2",
                "sender":"ou_owner","ts": now - CMD_BUTTON_TTL_SECS - 3600
            }}}
        })
        .to_string()
        .into_bytes();
        let (_, msg, deny) = parse_card_action_event(&expired).expect("应解析");
        assert!(
            deny.as_deref().is_some_and(|d| d.contains("已过期")),
            "{deny:?}"
        );
        assert!(msg.text.is_none(), "过期不得注入审批文本");
        // ts 在窗口内 → 正常回调。
        let fresh = serde_json::json!({
            "header":{"event_id":"e3","event_type":"card.action.trigger"},
            "event":{"operator":{"open_id":"ou_owner"},"action":{"tag":"button","value":{
                "imagent_perm":"allow","conv":"feishu:oc_g","req":"r3",
                "sender":"ou_owner","ts": now - 60
            }}}
        })
        .to_string()
        .into_bytes();
        let (_, msg, deny) = parse_card_action_event(&fresh).expect("应解析");
        assert!(deny.is_none());
        assert_eq!(msg.text.as_deref(), Some("y"));
    }

    /// 快赢：菜单跳转事件（application.url.menu_v6）→ 合成 text="/help" 的入站
    /// 消息（鉴权/分派与手打 /help 完全同路径）。事件体形态待真机校准。
    #[test]
    fn parse_menu_event_synthesizes_help() {
        let mk = |chat_id: Option<&str>, operator: &str| {
            let mut event = serde_json::json!({
                "operator": {"operator_id": {"open_id": operator}}
            });
            if let Some(c) = chat_id {
                event["chat_id"] = serde_json::json!(c);
            }
            serde_json::json!({
                "schema":"2.0",
                "header":{"event_id":"evt_menu","event_type":"application.url.menu_v6"},
                "event": event
            })
            .to_string()
            .into_bytes()
        };
        // 群菜单：conv = feishu:<chat_id>。
        let (key, msg) = parse_menu_event(&mk(Some("oc_g"), "ou_op")).expect("群菜单应解析");
        assert_eq!(key, "evt_menu");
        assert_eq!(msg.text.as_deref(), Some("/help"));
        assert_eq!(msg.conv_id.0, "feishu:oc_g");
        assert_eq!(msg.sender.0, "ou_op");
        // 私聊菜单（无 chat_id，平铺 operator 形态）→ conv 回退操作者私聊。
        let p2p = serde_json::json!({
            "header":{"event_id":"evt_menu2","event_type":"application.url.menu_v6"},
            "event": {"operator": {"open_id": "ou_p2p"}}
        })
        .to_string()
        .into_bytes();
        let (_, msg) = parse_menu_event(&p2p).expect("私聊菜单应解析");
        assert_eq!(msg.conv_id.0, "feishu:ou_p2p");
        assert_eq!(msg.text.as_deref(), Some("/help"));
        // 非目标事件 / 缺 operator → None。
        assert!(
            parse_menu_event(b"{\"header\":{\"event_type\":\"im.message.receive_v1\"}}").is_none()
        );
        let no_op = serde_json::json!({
            "header":{"event_type":"application.url.menu_v6"},
            "event": {"chat_id": "oc_g"}
        })
        .to_string()
        .into_bytes();
        assert!(parse_menu_event(&no_op).is_none());
    }

    /// 事件接入（一期）：消息撤回（im.message.recalled_v1）→ 控制消息
    /// （source_msg_id + MessageRecalled{notify_conv, probe_convs}）。payload 形态
    /// 待真机校准。
    #[test]
    fn parse_recall_event_to_control() {
        let mk = |chat_id: Option<&str>, sender: Option<&str>| {
            let mut event = serde_json::json!({});
            if let Some(c) = chat_id {
                event["chat_id"] = serde_json::json!(c);
            }
            if let Some(s) = sender {
                event["sender"] = serde_json::json!({ "sender_id": { "open_id": s } });
            }
            event["message_id"] = serde_json::json!("om_recalled");
            serde_json::json!({
                "schema":"2.0",
                "header":{"event_id":"evt_rec","event_type":"im.message.recalled_v1"},
                "event": event
            })
            .to_string()
            .into_bytes()
        };
        // 群撤回：notify=chat conv，probe 含 chat conv + 发送者私聊 conv 两形态。
        let (key, msg) = parse_recall_event(&mk(Some("oc_g"), Some("ou_sender"))).expect("应解析");
        assert_eq!(key, "evt_rec");
        assert_eq!(msg.source_msg_id.as_deref(), Some("om_recalled"));
        let imagent_core::InboundControl::MessageRecalled {
            notify_conv,
            probe_convs,
        } = msg.control.as_ref().expect("应为撤回控制消息")
        else {
            panic!("控制类型不符");
        };
        assert_eq!(notify_conv.as_ref().unwrap().0, "feishu:oc_g");
        assert!(
            probe_convs.iter().any(|c| c.0 == "feishu:oc_g"),
            "{probe_convs:?}"
        );
        assert!(
            probe_convs.iter().any(|c| c.0 == "feishu:ou_sender"),
            "{probe_convs:?}"
        );
        // 私聊撤回（chat_id 与 sender 同会话两形态）：notify=chat conv，probe 去重。
        let (_, msg) = parse_recall_event(&mk(Some("oc_p2p"), Some("ou_sender"))).expect("应解析");
        let imagent_core::InboundControl::MessageRecalled {
            notify_conv,
            probe_convs,
        } = msg.control.as_ref().unwrap()
        else {
            panic!("控制类型不符");
        };
        assert_eq!(notify_conv.as_ref().unwrap().0, "feishu:oc_p2p");
        assert!(probe_convs.iter().any(|c| c.0 == "feishu:ou_sender"));
        // 缺 chat_id → notify 回退撤回者私聊 conv。
        let (_, msg) = parse_recall_event(&mk(None, Some("ou_sender"))).expect("应解析");
        let imagent_core::InboundControl::MessageRecalled { notify_conv, .. } =
            msg.control.as_ref().unwrap()
        else {
            panic!("控制类型不符");
        };
        assert_eq!(notify_conv.as_ref().unwrap().0, "feishu:ou_sender");
        // 双缺（管理员撤回形态，v13-P3）：无处回执——不再伪造
        // feishu:{message_id}（om_ 前缀非合法 conv，经它回提示必然发送失败），
        // 直接忽略该事件。
        assert!(
            parse_recall_event(&mk(None, None)).is_none(),
            "chat_id 与 sender 双缺应忽略"
        );
        // 非目标事件 / 缺 message_id → None。
        assert!(
            parse_recall_event(b"{\"header\":{\"event_type\":\"im.message.receive_v1\"}}")
                .is_none()
        );
        let no_id = serde_json::json!({
            "header":{"event_type":"im.message.recalled_v1"},
            "event": {"chat_id": "oc_g"}
        })
        .to_string()
        .into_bytes();
        assert!(parse_recall_event(&no_id).is_none());
    }

    /// 事件接入：bot 被移出群（im.chat.member.bot.deleted_v1）→ 控制消息
    /// （conv=feishu:<chat_id> + BotRemovedFromChat）。payload 形态待真机校准。
    #[test]
    fn parse_bot_removed_event_to_control() {
        let payload = serde_json::json!({
            "schema":"2.0",
            "header":{"event_id":"evt_rm","event_type":"im.chat.member.bot.deleted_v1"},
            "event": {"chat_id": "oc_dead"}
        })
        .to_string()
        .into_bytes();
        let (key, msg) = parse_bot_removed_event(&payload).expect("应解析");
        assert_eq!(key, "evt_rm");
        assert_eq!(msg.conv_id.0, "feishu:oc_dead");
        assert!(matches!(
            msg.control.as_ref(),
            Some(imagent_core::InboundControl::BotRemovedFromChat)
        ));
        // 非目标事件 / 缺 chat_id → None。
        assert!(parse_bot_removed_event(
            b"{\"header\":{\"event_type\":\"im.message.receive_v1\"}}"
        )
        .is_none());
        let no_chat = serde_json::json!({
            "header":{"event_type":"im.chat.member.bot.deleted_v1"},
            "event": {}
        })
        .to_string()
        .into_bytes();
        assert!(parse_bot_removed_event(&no_chat).is_none());
    }
    /// 真机校准（2026-08-30）：群内不带 @ 的 `/chat allow` 此前在 @ 过滤层被
    /// 丢弃（引导命令死锁——群无法自助放行）。斜杠命令豁免；非命令仍拦。
    /// P3（v13 遗留）：豁免收敛为已知命令白名单——首词命中 core 分派表才放行，
    /// 未知斜杠词（`/foo 是什么意思`）按普通消息走 mention 门（要 @ 才触发）。
    #[test]
    fn group_slash_command_bypasses_mention_filter() {
        let policy = MentionPolicy::REQUIRE_BOT;
        // 已知命令（含引导命令与带参形态）：豁免 mention 门。
        for cmd in ["/chat allow", "/status", "/status 查一下", "/STATUS"] {
            let p = mk_group_mention_payload("e-cmd-1", cmd, "[]");
            assert!(
                parse_message_event(&p, &policy, None).is_some(),
                "群内已知命令 {cmd} 不应被 @ 过滤拦截"
            );
        }
        // 未知斜杠词：不再豁免——不带 @ 被拦（修复前任何 '/' 开头都豁免，
        // 群友闲聊 `/foo 是什么意思` 也进命令分派回未知命令提示，噪音）。
        for noise in ["/foo 是什么意思", "/salute!"] {
            let p = mk_group_mention_payload("e-noise-1", noise, "[]");
            assert!(
                parse_message_event(&p, &policy, None).is_none(),
                "未知斜杠词 {noise} 不带 @ 应被拦"
            );
        }
        // 同一条未知斜杠词带 @bot → mention 门照常放行（@ 即显式定向）。
        let at_bot = mk_group_mention_payload(
            "e-noise-2",
            "@_user_1 /foo 是什么意思",
            r#"[{"key":"@_user_1","id":{"open_id":"ou_bot"},"name":"agent"}]"#,
        );
        assert!(parse_message_event(&at_bot, &policy, Some("ou_bot")).is_some());
        // 对照：不带 @ 的普通文本仍被拦。
        let txt = mk_group_mention_payload("e-txt-1", "你好", "[]");
        assert!(
            parse_message_event(&txt, &policy, None).is_none(),
            "非命令群消息仍须 @"
        );
        // 私聊照旧：任何斜杠文本（含未知词）不受 mention 门影响。
        let p2p = serde_json::json!({
            "header":{"event_id":"e-p2p","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_u"}},
                "message":{"message_type":"text","content":"{\"text\":\"/foo 是什么意思\"}",
                "chat_type":"p2p"}}
        })
        .to_string()
        .into_bytes();
        assert!(
            parse_message_event(&p2p, &policy, Some("ou_bot")).is_some(),
            "私聊不受斜杠白名单影响"
        );
        // 快捷/审批短文本守卫不受影响：非斜杠（y/n）永不豁免（沿用 mention 门）。
        let y = mk_group_mention_payload("e-y", "y", "[]");
        assert!(parse_message_event(&y, &policy, None).is_none());
    }

    // ---------- T10：群聊上下文注入（mock JSON → 解析/转录纯函数） ----------

    /// create_time 量级归一（自 client.rs 随函数移入）：秒级 ×1000，毫秒原样，
    /// 非法/缺省 0。合并转发子消息与 T10 群历史消息共用。
    #[test]
    fn create_time_ms_of_normalization() {
        assert_eq!(
            create_time_ms_of(Some(&serde_json::json!(1787912340))),
            1_787_912_340_000,
            "秒级归一毫秒"
        );
        assert_eq!(
            create_time_ms_of(Some(&serde_json::json!("1787912345678"))),
            1_787_912_345_678,
            "毫秒字符串原样"
        );
        assert_eq!(
            create_time_ms_of(Some(&serde_json::json!(0))),
            0,
            "0 保持（缺失语义）"
        );
        assert_eq!(
            create_time_ms_of(Some(&serde_json::json!("abc"))),
            0,
            "非法字符串 → 0"
        );
        assert_eq!(create_time_ms_of(None), 0, "缺省 → 0");
    }

    /// 构造群历史条目（转录测试用）。
    fn gc_item(
        sender: &str,
        name: Option<&str>,
        from_bot: bool,
        mt: &str,
        content: &str,
        ms: i64,
    ) -> GroupContextItem {
        GroupContextItem {
            sender_id: sender.to_string(),
            sender_name: name.map(String::from),
            from_bot,
            message_type: mt.to_string(),
            content: content.to_string(),
            create_time_ms: ms,
        }
    }

    /// conv → 群 chat_id：普通群/话题群命中；私聊、评论 conv、非法前缀不注入。
    #[test]
    fn group_chat_id_of_conv_variants() {
        assert_eq!(
            group_chat_id_of_conv("feishu:oc_g"),
            Some("oc_g".to_string()),
            "普通群"
        );
        assert_eq!(
            group_chat_id_of_conv("feishu:oc_g:om_root"),
            Some("oc_g".to_string()),
            "话题群取首段（与免 @ 窗口同语义，无需特判）"
        );
        assert_eq!(group_chat_id_of_conv("feishu:ou_u"), None, "私聊不注入");
        assert_eq!(
            group_chat_id_of_conv("feishu:comment:doxcnXYZ"),
            None,
            "评论 conv 不注入"
        );
        assert_eq!(group_chat_id_of_conv("wecom:oc_g"), None, "跨平台串号");
        assert_eq!(group_chat_id_of_conv("feishu:"), None, "空 id");
    }

    /// 列表类 API 单条 item 统一提取（单一事实源）：真机 schema（msg_type +
    /// body.content + sender{id,sender_type} 无 name + upper_message_id +
    /// 毫秒字符串时间）与宽容回退（message_type 别名 / 顶层 content / 秒级
    /// 归一 / open_id 别名）钉住——字段名漂移在此一处红灯，不再三处各错各的。
    #[test]
    fn parse_raw_message_item_real_schema_and_tolerances() {
        // 真机抓包形态（2026-09-29，合并转发子消息；内容替换为等价文本）。
        let real = serde_json::json!({
            "message_id": "om_sub1",
            "upper_message_id": "om_parent",
            "msg_type": "text",
            "create_time": "1790680932661",
            "sender": { "id": "ou_alice", "id_type": "open_id", "sender_type": "user" },
            "body": { "content": "{\"text\":\"早上好\"}" }
        });
        let raw = parse_raw_message_item(&real).expect("对象应解析");
        assert_eq!(raw.message_type, "text");
        assert_eq!(raw.content, "{\"text\":\"早上好\"}", "body.content 优先");
        assert_eq!(raw.sender_id, "ou_alice");
        assert_eq!(raw.sender_name, None, "真机 sender 无 name");
        assert_eq!(raw.sender_type, "user");
        assert_eq!(raw.upper_message_id.as_deref(), Some("om_parent"));
        assert_eq!(raw.create_time_ms, 1_790_680_932_661);

        // 宽容回退：message_type 别名 / 顶层 content / open_id / 秒级 ×1000 /
        // upper 缺失 → None。
        let legacy = serde_json::json!({
            "message_type": "image",
            "content": "{\"image_key\":\"img_v3_x\"}",
            "create_time": 1787912340,
            "sender": { "open_id": "ou_bobxxxxxxxxxxxx", "app_id": "cli_self" }
        });
        let raw = parse_raw_message_item(&legacy).expect("对象应解析");
        assert_eq!(raw.message_type, "image");
        assert_eq!(raw.content, "{\"image_key\":\"img_v3_x\"}");
        assert_eq!(raw.sender_id, "ou_bobxxxxxxxxxxxx");
        assert_eq!(raw.sender_app_id, "cli_self");
        assert_eq!(raw.upper_message_id, None);
        assert_eq!(raw.create_time_ms, 1_787_912_340_000);

        // 非对象 / 空对象：None / 全默认值不炸。
        assert!(parse_raw_message_item(&serde_json::json!("x")).is_none());
        let raw = parse_raw_message_item(&serde_json::json!({})).expect("空对象默认值");
        assert_eq!(raw, RawMessageItem::default());
    }

    /// 「获取会话历史消息」响应 JSON → 条目：字段宽容提取（类型两名/时间秒归一/
    /// sender name、**body.content 优先 + 顶层 content 回退**——真机校准 2026-09-29）、
    /// bot 双判定（sender_type=app 或 app_id 命中本应用）。
    #[test]
    fn parse_group_context_items_tolerant_and_bot_detection() {
        let body = serde_json::json!({
            "code": 0,
            "data": { "items": [
                {
                    "message_id": "om_1", "msg_type": "text", "create_time": "1788000001",
                    "sender": { "id": "ou_alice", "id_type": "open_id", "sender_type": "user" },
                    "body": { "content": "{\"text\":\"早上好\"}" }
                },
                {
                    "message_id": "om_2", "message_type": "post", "create_time": "1788000002000",
                    "sender": { "id": "ou_bob", "id_type": "open_id", "sender_type": "app" },
                    "content": "{}"
                },
                {
                    "message_id": "om_3", "msg_type": "text", "create_time": "1788000003",
                    "sender": { "id": "ou_x", "id_type": "open_id", "sender_type": "user", "app_id": "cli_self" },
                    "content": "{}"
                },
                "not-an-object"
            ]}
        });
        let items = parse_group_context_items(&body, "cli_self");
        assert_eq!(items.len(), 3, "非对象条目跳过: {items:?}");
        // 字段提取 + 秒级时间归一毫秒；真机 schema：sender 无 name、内容在
        // body.content（此前只读顶层 content → 全空，只剩类型占位标签）。
        assert_eq!(items[0].sender_name, None, "真机 sender 无 name 字段");
        assert_eq!(items[0].message_type, "text");
        assert_eq!(
            items[0].content, "{\"text\":\"早上好\"}",
            "body.content 优先提取"
        );
        assert_eq!(items[0].create_time_ms, 1_788_000_001_000);
        // message_type/msg_type 两名兼容 + 数字毫秒原样。
        assert_eq!(items[1].message_type, "post");
        assert_eq!(items[1].create_time_ms, 1_788_000_002_000);
        // bot 双判定：sender_type=app（其它应用）与 app_id 命中本应用都算。
        assert!(items[1].from_bot, "sender_type=app 应标 bot");
        assert!(items[2].from_bot, "app_id 命中本应用应标 bot");
        assert!(!items[0].from_bot);
        // data.items 缺失（异常信封）→ 空（不 panic）。
        assert!(parse_group_context_items(&serde_json::json!({}), "cli_self").is_empty());
    }

    /// 转录块：跳过 bot、时间正序（最新在最后）、名字/短 id 标注、类型占位、
    /// 头尾格式、全 bot/空 → None。
    #[test]
    fn render_group_context_block_orders_filters_and_formats() {
        // 输入按 API 返回序（desc：最新在前），渲染应翻成正序。
        let items = vec![
            gc_item(
                "ou_b0c072f42e7c1b09",
                None,
                false,
                "text",
                &serde_json::to_string(&serde_json::json!({"text": "最新这条"})).unwrap(),
                3000,
            ),
            gc_item(
                "ou_bot",
                Some("agent"),
                true,
                "text",
                &serde_json::to_string(&serde_json::json!({"text": "bot 回声"})).unwrap(),
                2000,
            ),
            gc_item("ou_alice", Some("Alice"), false, "image", "{}", 1000),
        ];
        let block = render_group_context_block(&items).expect("应有块");
        assert!(
            block.starts_with("【群最近上下文（2 条，最新在最后）】"),
            "{block}"
        );
        assert!(!block.contains("bot 回声"), "bot 条目应跳过: {block}");
        assert!(!block.contains("agent"), "bot 名字不应出现: {block}");
        // 正序：早（Alice [图片]）在前、晚（bob 短 id 文本）在后。
        let img = block.find("Alice: [图片]").expect("图片占位 + 名字标注");
        let latest = block
            .find("2e7c1b09: 最新这条")
            .expect("缺名回退 id 后 8 位");
        assert!(img < latest, "最新在最后: {block}");
        // 尾注在块尾。
        assert!(block.ends_with("（以下是用户本轮消息）"), "{block}");
        // 单条正文截 200 字。
        let long: String = "长".repeat(300);
        let items = vec![gc_item(
            "ou_l",
            None,
            false,
            "text",
            &serde_json::to_string(&serde_json::json!({"text": long})).unwrap(),
            1,
        )];
        let block = render_group_context_block(&items).expect("应有块");
        let body_len = block.lines().nth(1).map(|l| l.chars().count()).unwrap_or(0);
        assert!(
            body_len <= "ou_l".len() + 2 + GROUP_CONTEXT_LINE_MAX_CHARS,
            "单条截 200 字: {block}"
        );
        // 空 / 全 bot → None（不注入）。
        assert!(render_group_context_block(&[]).is_none());
        let all_bot = vec![gc_item("ou_bot", None, true, "text", "{}", 1)];
        assert!(render_group_context_block(&all_bot).is_none());
    }

    /// 总长预算：多条超长消息累计超 1500 字符 → 截断标注 + 尾注保留 + 总长恒
    /// ≤ 上限（chars，CJK 安全）。
    #[test]
    fn render_group_context_block_respects_total_budget() {
        let long: String = "话".repeat(GROUP_CONTEXT_LINE_MAX_CHARS);
        let content = serde_json::to_string(&serde_json::json!({ "text": long })).unwrap();
        let items: Vec<GroupContextItem> = (0..20)
            .map(|i| gc_item(&format!("ou_{i}"), None, false, "text", &content, i + 1))
            .collect();
        let block = render_group_context_block(&items).expect("应有块");
        assert!(
            block.chars().count() <= GROUP_CONTEXT_MAX_CHARS,
            "总长 ≤ 上限: {}",
            block.chars().count()
        );
        assert!(block.contains("（已截断，共 20 条中前"), "{block}");
        assert!(
            block.ends_with("（以下是用户本轮消息）"),
            "尾注恒保留: {block}"
        );
    }
}

/// v1.18 回复即定向预检：仅群回复形态返回 parent。
#[test]
fn peek_group_reply_parent_shapes() {
    let mk = |chat_type: &str, parent: Option<&str>| {
        serde_json::json!({
            "header": {"event_type": "im.message.receive_v1"},
            "event": {"message": {"chat_type": chat_type, "parent_id": parent}}
        })
        .to_string()
        .into_bytes()
    };
    assert_eq!(
        peek_group_reply_parent(&mk("group", Some("om_p1"))).as_deref(),
        Some("om_p1")
    );
    assert_eq!(
        peek_group_reply_parent(&mk("p2p", Some("om_p1"))),
        None,
        "私聊无需豁免"
    );
    assert_eq!(
        peek_group_reply_parent(&mk("group", None)),
        None,
        "非回复形态"
    );
    assert_eq!(
        peek_group_reply_parent(&mk("group", Some(""))),
        None,
        "空 parent"
    );
}
