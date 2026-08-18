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
    pub operator_id: CardOperatorId,
}

#[derive(Debug, Deserialize)]
pub struct CardOperatorId {
    #[serde(default)]
    pub open_id: Option<String>,
}

/// 解析审批按钮回调：value 形如 `{"imagent_perm":"allow|deny","conv":"feishu:…"}`。
///
/// 映射为 `text = "y"/"n"` 的普通 InboundMessage——core 的 recv 循环会把 pending
/// conv 的非斜杠消息当审批回复路由（`parse_reply`），平台无需感知权限闭环。
/// 非 imagent 按钮 / 缺 conv / 缺 open_id 返回 None。
pub fn parse_card_action_event(payload: &[u8]) -> Option<(String, InboundMessage)> {
    let evt: CardActionEvent = serde_json::from_slice(payload).ok()?;
    if evt.header.event_type != "card.action.trigger" {
        return None;
    }
    let value = evt.event.action.get("value")?;
    let act = value.get("imagent_perm")?.as_str()?;
    let conv = value.get("conv")?.as_str()?;
    let text = match act {
        "allow" => "y",
        "deny" => "n",
        _ => return None,
    };
    let open_id = evt
        .event
        .operator
        .operator_id
        .open_id
        .filter(|s| !s.is_empty())?;
    let key = evt
        .header
        .event_id
        .clone()
        .unwrap_or_else(|| format!("card_action:{open_id}:{conv}:{act}"));
    Some((
        key,
        InboundMessage {
            conv_id: ConvId(conv.to_string()),
            sender: UserId(open_id),
            text: Some(text.to_string()),
            media: vec![],
            media_errors: Vec::new(),
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
    let key = evt
        .header
        .event_id
        .clone()
        .unwrap_or_else(|| format!("comment:{}:{}", b.comment_id, text.len()));
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
/// 返回 `(dedup_key, InboundMessage, pending_media)`；以下情况返回 `None`
/// （上层丢弃）：非目标事件 / 不支持的消息类型（非 text/image/file/post）/ text 空文本
/// / image 缺 image_key / file 缺 file_key / post 无文字且无图片 / content 非法 JSON
/// / payload 非法 JSON / 缺 receive_id。`pending_media` 为待下载的图片/文件（仅解析出
/// key，实际下载落盘在 platform 层完成，回填进 `InboundMessage.media`）。
pub fn parse_message_event(payload: &[u8]) -> Option<(String, InboundMessage, Vec<PendingMedia>)> {
    let evt: FeishuEvent = serde_json::from_slice(payload).ok()?;
    if evt.header.event_type != "im.message.receive_v1" {
        return None;
    }
    let mt = evt.event.message.message_type.as_str();
    let message_id = evt.event.message.message_id.clone().unwrap_or_default();
    // 解析 content：text 提取文本（空文本丢弃），image/file 提取资源 key（缺 key 丢弃）。
    let (text, pending): (Option<String>, Vec<PendingMedia>) = match mt {
        "text" => {
            let t = extract_text(&evt.event.message.content)?;
            if t.trim().is_empty() {
                return None;
            }
            (Some(t), vec![])
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
            )
        }
        "post" => {
            let (t, mut p) = parse_post(&evt.event.message.content)?;
            for m in &mut p {
                m.message_id = message_id.clone();
            }
            // 文本与图片皆空才视为无效丢弃（防御：空 post）。
            if t.as_deref().is_none_or(|s| s.trim().is_empty()) && p.is_empty() {
                return None;
            }
            (t, p)
        }
        _ => return None, // audio/video/voice/... 暂不支持
    };

    let open_id = evt.event.sender.sender_id.open_id.clone();
    let (receive_id, _kind) = receive_target(&evt.event)?;
    // dedup 回退基准：优先正文长度，其次首个媒体 key，最后用消息类型兜底
    // （post 可能纯文字 pending 空、或纯图片 text 空，旧逻辑 pending[0] 会 panic）。
    let dedup_fallback = match (text.as_deref(), pending.first()) {
        (Some(t), _) if !t.trim().is_empty() => format!("{}:{}", receive_id, t.len()),
        (_, Some(p)) => format!("{}:{}", receive_id, p.key),
        _ => format!("{receive_id}:{mt}"),
    };
    let dedup_key = evt
        .header
        .event_id
        .clone()
        .or_else(|| evt.event.message.message_id.clone())
        .unwrap_or(dedup_fallback);
    let msg = InboundMessage {
        conv_id: ConvId(format!("feishu:{receive_id}")),
        sender: UserId(open_id),
        text,
        media: vec![],
        media_errors: Vec::new(),
        reply_hint: ReplyHint::None,
    };
    Some((dedup_key, msg, pending))
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
/// content 非法 JSON 返回 `None`。text 全空则正文为 `None`。
fn parse_post(content: &str) -> Option<(Option<String>, Vec<PendingMedia>)> {
    let post: PostContent = serde_json::from_str(content).ok()?;
    let mut texts: Vec<String> = Vec::new();
    let mut pending: Vec<PendingMedia> = Vec::new();
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
                _ => {} // at/a/mention 等暂忽略
            }
        }
    }
    let text = if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    };
    Some((text, pending))
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

/// 发消息反向解析：`feishu:<id>` → `(id, kind)`。
/// 飞书 ID 前缀约定：`ou_` = open_id（用户，私聊），其余（`oc_` = chat_id，群聊）→ ChatId。
/// 无 `feishu:` 前缀返回 `None`（非法 conv_id，上层报错）。
pub fn receive_target_from_conv(conv: &ConvId) -> Option<(String, ReceiveIdKind)> {
    let id = conv.0.strip_prefix("feishu:")?;
    let kind = if id.starts_with("ou_") {
        ReceiveIdKind::OpenId
    } else {
        ReceiveIdKind::ChatId
    };
    Some((id.to_string(), kind))
}

// ---------------------------------------------------------------------------
// 单测：纯逻辑，无网络、无真机。验收核心。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
        let (key, msg, pending) = parse_message_event(payload).expect("p2p 文本应解析成功");
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
        let (key, msg, _) = parse_message_event(payload).expect("group 文本应解析成功");
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
        let (_key, msg, _) = parse_message_event(payload).expect("group 回退 chat_id 应成功");
        assert_eq!(msg.conv_id.0, "feishu:oc_chat2");
    }

    /// 非 im.message.receive_v1 事件丢弃。
    #[test]
    fn ignore_other_event_type() {
        let payload = br#"{
            "header":{"event_id":"evt_x","event_type":"application.url.menu_v6"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"text","content":"{\"text\":\"hi\"}","chat_type":"p2p"}}
        }"#;
        assert!(parse_message_event(payload).is_none());
    }

    /// 不支持的媒体类型（audio/video/voice 等）丢弃。
    #[test]
    fn ignore_unsupported_media_type() {
        let payload = br#"{
            "header":{"event_id":"evt_i","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"audio","content":"{\"file_key\":\"k\"}","chat_type":"p2p"}}
        }"#;
        assert!(parse_message_event(payload).is_none());
    }

    /// p2p 图片：pending 含 image key，msg.text==None、media 空。
    #[test]
    fn parse_p2p_image() {
        let payload = br#"{
            "header":{"event_id":"evt_img","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_user1"}},"message":{"message_type":"image","content":"{\"image_key\":\"img_v3_00ab\"}","chat_type":"p2p"}}
        }"#;
        let (key, msg, pending) = parse_message_event(payload).expect("图片应解析成功");
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
        let (key, msg, pending) = parse_message_event(payload).expect("文件应解析成功");
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
        assert!(parse_message_event(payload).is_none());
    }

    /// image content 非法 JSON 丢弃。
    #[test]
    fn ignore_image_invalid_content_json() {
        let payload = br#"{"header":{"event_id":"e","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"image","content":"not-json","chat_type":"p2p"}}}"#;
        assert!(parse_message_event(payload).is_none());
    }

    /// image 消息缺 event_id 时 dedup 回退到 message_id，再缺回退到 receive_id:image_key。
    #[test]
    fn image_dedup_fallback() {
        // 有 message_id → 用 message_id。
        let p1 = br#"{"header":{"event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"image","content":"{\"image_key\":\"img_k1\"}","chat_type":"p2p","message_id":"om_img1"}}}"#;
        let (key, _, _) = parse_message_event(p1).expect("应解析成功");
        assert_eq!(key, "om_img1");

        // event_id 与 message_id 都缺 → 回退 receive_id:image_key。
        let p2 = br#"{"header":{"event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"image","content":"{\"image_key\":\"img_k2\"}","chat_type":"p2p"}}}"#;
        let (key2, _, _) = parse_message_event(p2).expect("应解析成功");
        assert_eq!(key2, "ou_x:img_k2");
    }

    /// 空文本（含纯空白）丢弃。
    #[test]
    fn ignore_empty_text() {
        let empty = br#"{"header":{"event_id":"e","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"text","content":"{\"text\":\"\"}","chat_type":"p2p"}}}"#;
        assert!(parse_message_event(empty).is_none());

        let ws = br#"{"header":{"event_id":"e","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"text","content":"{\"text\":\"   \"}","chat_type":"p2p"}}}"#;
        assert!(parse_message_event(ws).is_none());
    }

    /// 非法 JSON payload 丢弃。
    #[test]
    fn ignore_invalid_json() {
        assert!(parse_message_event(b"not json at all").is_none());
        assert!(parse_message_event(b"").is_none());
    }

    /// content 非法 JSON 丢弃。
    #[test]
    fn ignore_invalid_content_json() {
        let payload = br#"{"header":{"event_id":"e","event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"message_type":"text","content":"not-json","chat_type":"p2p"}}}"#;
        assert!(parse_message_event(payload).is_none());
    }

    /// dedup key 回退：缺 event_id 时用 message_id。
    #[test]
    fn dedup_key_falls_back_to_message_id() {
        let payload = br#"{
            "header":{"event_type":"im.message.receive_v1"},
            "event":{"sender":{"sender_id":{"open_id":"ou_user9"}},"message":{"message_type":"text","content":"{\"text\":\"hi\"}","chat_type":"p2p","message_id":"om_fb"}}
        }"#;
        let (key, _, _) = parse_message_event(payload).expect("应解析成功");
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
            parse_message_event(payload.as_bytes()).expect("post 图片+文字应解析成功");
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
        let (key, msg, pending) =
            parse_message_event(payload.as_bytes()).expect("纯图片 post 应解析");
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
            parse_message_event(payload.as_bytes()).expect("纯文字 post 应解析");
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
        assert!(parse_message_event(payload).is_none());
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
}
