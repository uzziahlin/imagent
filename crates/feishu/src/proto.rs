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

/// file 类型消息的 content 结构：`{"file_key":"..."}`。
#[derive(Debug, Deserialize)]
pub struct FileContent {
    pub file_key: String,
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

/// 解析按钮卡片回调（card.action.trigger），两类 value（P6-3 扩展）：
/// - 审批按钮：`{"imagent_perm":"allow|deny","conv":"feishu:…"}` → `text = "y"/"n"`，
///   core 的 recv 循环把 pending conv 的非斜杠消息当审批回复路由（`parse_reply`）；
/// - 命令按钮：`{"imagent_cmd":"/ws use main","conv":"feishu:…"}` → `text = <command>`，
///   走与手打命令完全相同的鉴权/分派路径（admin 门槛等不豁免）。
///
/// 非 imagent 按钮 / 缺 conv / 缺 open_id / 命令非 `/` 开头（防伪造非命令文本）
/// 返回 None。
pub fn parse_card_action_event(payload: &[u8]) -> Option<(String, InboundMessage)> {
    let evt: CardActionEvent = serde_json::from_slice(payload).ok()?;
    if evt.header.event_type != "card.action.trigger" {
        return None;
    }
    // 真机校准：新信封 action 平铺（value 直接是 action 的字段），旧信封嵌套在
    // action.value 下——两形态都认。
    let value = evt.event.action.get("value").unwrap_or(&evt.event.action);
    let conv = value.get("conv")?.as_str()?;
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
    // P9-2：表单提交按钮（imagent_form）——CardKit form 的用户输入值**不在**
    // action.value 里，在 action.form_value（lcab dispatcher 同款校准）。把
    // (key, string value) 拼成 `/config form k=v k=v` 命令文本，走与手打命令
    // 相同的鉴权（admin 门槛）/分派。
    let text: String = if value.get("imagent_form").and_then(|v| v.as_str()).is_some() {
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
    } else if let Some(choice) = value.get("imagent_ask").and_then(|c| c.as_str()) {
        format!("ask:{choice}")
    } else {
        match (act, cmd) {
            (Some("allow"), _) => "y".to_string(),
            (Some("deny"), _) => "n".to_string(),
            (_, Some(c)) if c.starts_with('/') => c.to_string(),
            _ => return None,
        }
    };
    let open_id = evt
        .event
        .operator
        .open_id
        .clone()
        .or_else(|| {
            evt.event
                .operator
                .operator_id
                .as_ref()
                .and_then(|o| o.open_id.clone())
        })
        .filter(|s| !s.is_empty())?;
    // P3：缺 event_id 的回退 key 用 content_hash 对完整内容取哈希——与消息/
    // 评论回退同语义（S4 口径）。此前用 text 前 40 字符：>40 字符的不同文本
    // 前缀相同会被互相去重（按钮回调/长命令文本可超 40 字符）。
    let key = evt.header.event_id.clone().unwrap_or_else(|| {
        format!("card_action:{open_id}:{conv}:{:x}", content_hash(&text))
    });
    Some((
        key,
        InboundMessage {
            conv_id: ConvId(conv.to_string()),
            sender: UserId(open_id),
            text: Some(text),
            media: vec![],
            media_errors: Vec::new(),
            mentions: Vec::new(),
            mentioned_bot: false,
            ask_req,
            reply_to: None,
            reply_hint: ReplyHint::None,
        },
    ))
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

/// 评论线程的 conv_id 前缀：`feishu:comment:<file_token>:<comment_id>`。
/// send_text 据此走「回复评论」API；每条评论 = 独立会话线程。
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

/// 解析云文档评论事件 → InboundMessage（conv = 评论线程，text = text 节点拼接）。
///
/// P5-8：须 @bot 才触发——`bot_open_id` 已知时要求 at 节点命中 bot、且 sender
/// 不是 bot 自身（防 bot 回复再触发自己的自循环）；`bot_open_id` 未知（取 bot
/// 信息失败）时退化为「至少含一个 at 节点」的弱过滤（drain 层取到 id 后自动
/// 收紧）。缺 file_token/comment_id/sender 或纯 @ 无文字返回 None。
pub fn parse_comment_event(
    payload: &[u8],
    bot_open_id: Option<&str>,
) -> Option<(String, InboundMessage)> {
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
    Some((
        key,
        InboundMessage {
            conv_id: ConvId(format!(
                "{COMMENT_CONV_PREFIX}{}:{}",
                b.file_token, b.comment_id
            )),
            sender: UserId(open_id),
            text: Some(text.join("\n")),
            media: vec![],
            media_errors: Vec::new(),
            mentions: Vec::new(),
            mentioned_bot: false,
            ask_req: None,
            reply_to: None,
            reply_hint: ReplyHint::None,
        },
    ))
}

/// 反解评论线程 conv_id → `(file_token, comment_id)`；非评论 conv 返回 None。
pub fn comment_target_from_conv(conv: &ConvId) -> Option<(String, String)> {
    let rest = conv.0.strip_prefix(COMMENT_CONV_PREFIX)?;
    let (file_token, comment_id) = rest.split_once(':')?;
    if file_token.is_empty() || comment_id.is_empty() {
        return None;
    }
    Some((file_token.to_string(), comment_id.to_string()))
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
    // 群消息 @bot 过滤（P6-1）：在正文清洗前判定，未 @bot 直接丢弃。
    if !group_mention_ok(
        &evt.event.message.chat_type,
        &evt.event.message.mentions,
        policy,
        bot_open_id,
    ) {
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
                }],
                Vec::new(),
            )
        }
        "file" => {
            let key = extract_file_key(&evt.event.message.content)?;
            (
                None,
                vec![PendingMedia {
                    kind: "file",
                    key,
                    message_id,
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
            }
            // 文本与图片皆空才视为无效丢弃（防御：空 post）。
            if t.as_deref().is_none_or(|s| s.trim().is_empty()) && p.is_empty() {
                return None;
            }
            (t, p, mentions)
        }
        _ => return None, // audio/video/voice/... 暂不支持
    };

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
        text,
        media: vec![],
        media_errors: Vec::new(),
        mentions,
        mentioned_bot,
        ask_req: None,
        reply_to: evt.event.message.parent_id.filter(|p| !p.is_empty()),
        reply_hint: ReplyHint::None,
    };
    Some((dedup_key, msg, pending))
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

/// 从 file 消息 content 提取 file_key：`{"file_key":"..."}`。
/// 非法 JSON 或缺字段返回 `None`。
pub fn extract_file_key(content: &str) -> Option<String> {
    serde_json::from_str::<FileContent>(content)
        .ok()
        .map(|c| c.file_key)
}

/// 解析 post 富文本：提取所有 text 节点拼成正文 + 所有 img 节点的 image_key。
/// P6-1：at 节点——@bot 跳过（剥离），@他人渲染为 `@名字` 并进 mentions。
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
                _ => {} // a/mention 等其余忽略
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

    /// 非 im.message.receive_v1 事件丢弃。
    #[test]
    fn ignore_other_event_type() {
        let payload = br#"{
            "header":{"event_id":"evt_x","event_type":"application.url.menu_v6"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"text","content":"{\"text\":\"hi\"}","chat_type":"p2p"}}
        }"#;
        assert!(parse_permissive(payload).is_none());
    }

    /// 不支持的媒体类型（audio/video/voice 等）丢弃。
    #[test]
    fn ignore_unsupported_media_type() {
        let payload = br#"{
            "header":{"event_id":"evt_i","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"audio","content":"{\"file_key\":\"k\"}","chat_type":"p2p"}}
        }"#;
        assert!(parse_permissive(payload).is_none());
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

    /// p2p 文件：pending 含 file key。
    #[test]
    fn parse_p2p_file() {
        let payload = br#"{
            "header":{"event_id":"evt_file","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_user2"}},"message":{"message_type":"file","content":"{\"file_key\":\"file_v3_001\"}","chat_type":"p2p"}}
        }"#;
        let (key, msg, pending) = parse_permissive(payload).expect("文件应解析成功");
        assert_eq!(key, "evt_file");
        assert_eq!(msg.conv_id.0, "feishu:ou_user2");
        assert!(msg.text.is_none());
        assert!(msg.media.is_empty());
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, "file");
        assert_eq!(pending[0].key, "file_v3_001");
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
        let (key, msg) = parse_card_action_event(&mk("allow")).expect("allow 应回调");
        assert_eq!(key, "evt_btn_1");
        assert_eq!(msg.conv_id.0, "feishu:ou_op");
        assert_eq!(msg.sender.0, "ou_op");
        assert_eq!(msg.text.as_deref(), Some("y"));
        let (_, msg) = parse_card_action_event(&mk("deny")).expect("deny 应回调");
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
        let (ka, _) = parse_card_action_event(&mk(&long_a, None)).expect("解析成功");
        let (kb, _) = parse_card_action_event(&mk(&long_b, None)).expect("解析成功");
        // 前 40 字符相同（"/do " + 60 个 x 覆盖前缀窗口）但内容不同 → 不同 key。
        assert_ne!(ka, kb, "前缀相同的不同长命令不应同 key: {ka} vs {kb}");
        // 相同内容（重投）→ 稳定同 key。
        let (ka2, _) = parse_card_action_event(&mk(&long_a, None)).expect("解析成功");
        assert_eq!(ka, ka2);
        // 有 event_id 时仍优先用 event_id。
        let (kid, _) = parse_card_action_event(&mk(&long_a, Some("evt_x"))).expect("解析成功");
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
        let (_, msg) = parse_card_action_event(&mk(serde_json::json!({
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
        let (key, msg) = parse_card_action_event(&payload).expect("平铺 operator 应可解析");
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
        let (key, msg) = parse_card_action_event(&payload).expect("选项回调应可解析");
        assert_eq!(key, "evt_ask_1");
        assert_eq!(msg.text.as_deref(), Some("ask:数据库迁移"));
        assert_eq!(msg.conv_id.0, "feishu:ou_q");
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
        let (_, msg) = parse_card_action_event(&with_req).expect("应可解析");
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
        let (_, msg) = parse_card_action_event(&no_req).expect("应可解析");
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
        let (key, msg) = parse_card_action_event(&mk("/ws use main")).expect("命令按钮应回调");
        assert_eq!(key, "evt_cmd_1");
        assert_eq!(msg.conv_id.0, "feishu:oc_g");
        assert_eq!(msg.sender.0, "ou_op");
        assert_eq!(msg.text.as_deref(), Some("/ws use main"));
        // 非 / 开头 → 拒（回调不应产生普通聊天文本）。
        assert!(parse_card_action_event(&mk("rm -rf /")).is_none());
        // 旧信封（operator_id 嵌套 + action.value）同样支持命令按钮。
        let legacy = br#"{"header":{"event_id":"evt_cmd_2","event_type":"card.action.trigger"},
            "event":{"operator":{"operator_id":{"open_id":"ou_o2"}},"action":{"value":{"imagent_cmd":"/resume 3","conv":"feishu:ou_o2"}}}}"#;
        let (_, msg) = parse_card_action_event(legacy).expect("旧信封命令按钮应回调");
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
        let (key, msg) =
            parse_comment_event(payload.as_bytes(), Some("ou_bot")).expect("评论事件应解析");
        assert_eq!(key, "evt_cm_1");
        assert_eq!(msg.conv_id.0, "feishu:comment:doxcnXYZ:7034abc");
        assert_eq!(msg.sender.0, "ou_author");
        assert_eq!(msg.text.as_deref(), Some(" 帮我总结这份文档"));
        // conv 反解 roundtrip。
        let (ft, cid) = comment_target_from_conv(&msg.conv_id).unwrap();
        assert_eq!(ft, "doxcnXYZ");
        assert_eq!(cid, "7034abc");
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
}
