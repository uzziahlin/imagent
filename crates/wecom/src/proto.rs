//! WS 帧 serde 结构 + 纯函数构造/解析。
//!
//! 所有结构按官方 SDK 的 snake_case 字段命名（SDK 用 camelCase，服务端实际
//! 传输为 snake_case——本 crate 直接对接服务端，故用 snake_case）。
//! 纯函数部分无网络、无副作用，是验收核心（见 `mod tests`）。

use std::time::{SystemTime, UNIX_EPOCH};

use imagent_core::{ConvId, CoreError, InboundMessage, ReplyHint, UserId};
use serde::{Deserialize, Serialize};

/// 企业微信智能机器人凭据。从 store 的 credential blob 反序列化得到。
///
/// - `bot_id`：智能机器人 ID（控制台分配）。
/// - `secret`：智能机器人 secret（控制台分配）。
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub bot_id: String,
    pub secret: String,
}

/// 🟡 Debug redacting：secret 是凭据，避免 `{:?}` 落日志。
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("bot_id", &self.bot_id)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// 帧头。服务端帧头至少含 `req_id`，其余键按 serde 默认忽略未知。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsHeaders {
    pub req_id: String,
}

/// 统一 WS 帧结构（对应 SDK `WsFrame<T>`）。
///
/// 收帧时 `body` 用 [`serde_json::Value`] 保留原始 JSON，由调用方按 `cmd`
/// 再解析（`aibot_msg_callback` 的 body 形如 [`BaseMessage`]）。所有可选字段
/// 序列化时省略 `None`，与服务端协议一致。
#[derive(Clone, Serialize, Deserialize)]
pub struct WsFrame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
    pub headers: WsHeaders,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errcode: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errmsg: Option<String>,
}

/// 🟡 Debug redacting：body 可能含 subscribe 的 secret（见 build_subscribe_frame），
/// 避免 `debug!(?frame)` / `{:?}` 把 secret 落日志，统一 redact body。
impl std::fmt::Debug for WsFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsFrame")
            .field("cmd", &self.cmd)
            .field("headers", &self.headers)
            .field("body", &"<redacted>")
            .field("errcode", &self.errcode)
            .field("errmsg", &self.errmsg)
            .finish()
    }
}

/// `aibot_msg_callback` 的 body（最小定义，未知字段忽略）。
///
/// `msgid`/`chattype`/`msgtype` 为协议字段，反序列化保留供扩展使用；当前解析
/// 仅消费 `from.userid` 与 `text`，整体 `#[allow(dead_code)]` 以消除 clippy 误报。
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct BaseMessage {
    pub msgid: String,
    /// `"single" | "group"`。
    pub chattype: String,
    pub from: MessageFrom,
    /// `"text" | "image" | "mixed" | "voice" | "file" | "video"`。
    pub msgtype: String,
    #[serde(default)]
    pub text: Option<TextContent>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageFrom {
    pub userid: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TextContent {
    pub content: String,
}

// ---------------------------------------------------------------------------
// 纯函数：req_id / 帧构造 / 帧解析
// ---------------------------------------------------------------------------

/// 生成 req_id：`{prefix}_{毫秒时间戳}_{8 位 hex 随机}`。
///
/// prefix 为命令名（`aibot_subscribe` / `ping` / `aibot_send_msg`）。
/// 唯一性由毫秒时间戳 + 4 字节随机保证，足够区分同毫秒内的并发帧。
pub fn generate_req_id(prefix: &str) -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // 4 字节随机 → 8 位 hex（与 SDK crypto.randomBytes(4).toString('hex') 等价）。
    let rand_hex = format!("{:08x}", rand::random::<u32>());
    format!("{prefix}_{ms}_{rand_hex}")
}

/// 构造 `aibot_subscribe` 认证帧（连接 open 后立即发）。
pub fn build_subscribe_frame(bot_id: &str, secret: &str) -> WsFrame {
    WsFrame {
        cmd: Some("aibot_subscribe".into()),
        headers: WsHeaders {
            req_id: generate_req_id("aibot_subscribe"),
        },
        body: Some(serde_json::json!({
            "bot_id": bot_id,
            "secret": secret,
        })),
        errcode: None,
        errmsg: None,
    }
}

/// 构造 `ping` 心跳帧。
pub fn build_ping_frame() -> WsFrame {
    WsFrame {
        cmd: Some("ping".into()),
        headers: WsHeaders {
            req_id: generate_req_id("ping"),
        },
        body: None,
        errcode: None,
        errmsg: None,
    }
}

/// 构造 `aibot_send_msg` 主动发送帧（markdown 承载文本）。
///
/// 单聊 `chatid = userid`；群聊 `chatid = 群 chatid`（MVP 仅单聊）。
/// SDK 无 plain text 类型，统一用 markdown（渲染纯文本正常）。
pub fn build_send_markdown_frame(chatid: &str, content: &str) -> WsFrame {
    WsFrame {
        cmd: Some("aibot_send_msg".into()),
        headers: WsHeaders {
            req_id: generate_req_id("aibot_send_msg"),
        },
        body: Some(serde_json::json!({
            "chatid": chatid,
            "msgtype": "markdown",
            "markdown": { "content": content },
        })),
        errcode: None,
        errmsg: None,
    }
}

/// 从 `aibot_msg_callback` 帧解析入站消息。
///
/// - `conv_id = wecom:<from.userid>`（单聊）。群聊应为 `wecom:group:<chatid>`，留后。
/// - text 类型取 `body.text.content`；非 text 类型 `text=None`（media 留后）。
/// - body 缺失或解析失败 → `Err(CoreError::Platform("wecom", _))`。
pub fn parse_msg_callback(frame: &WsFrame) -> imagent_core::Result<(String, InboundMessage)> {
    if frame.cmd.as_deref() != Some("aibot_msg_callback") {
        return Err(CoreError::Platform(
            "wecom",
            format!(
                "parse_msg_callback 期望 aibot_msg_callback，收到 cmd={:?}",
                frame.cmd
            ),
        ));
    }
    let body = frame
        .body
        .as_ref()
        .ok_or_else(|| CoreError::Platform("wecom", "aibot_msg_callback 帧缺少 body".into()))?;
    let msg: BaseMessage = serde_json::from_value(body.clone()).map_err(|e| {
        CoreError::Platform("wecom", format!("aibot_msg_callback body 解析失败：{e}"))
    })?;

    let msgid = msg.msgid;
    let userid = msg.from.userid;
    let conv_id = ConvId(format!("wecom:{userid}"));
    let text = msg.text.map(|t| t.content);
    let inbound = InboundMessage {
        conv_id,
        sender: UserId(userid),
        text,
        media: vec![],
        reply_hint: ReplyHint::None,
    };
    // 返回 msgid 供上层（drain task）做滑动窗口去重（P1-I）。
    Ok((msgid, inbound))
}

/// 从 `ConvId` 还原 userid（strip `wecom:` 前缀；无前缀原样返回）。
pub fn userid_from_conv(conv: &ConvId) -> String {
    conv.0
        .strip_prefix("wecom:")
        .map(|s| s.to_string())
        .unwrap_or_else(|| conv.0.clone())
}

/// 序列化帧为 JSON 字符串（出站用）。
pub fn frame_to_string(frame: &WsFrame) -> serde_json::Result<String> {
    serde_json::to_string(frame)
}

/// 解析 JSON 字符串为帧（入站用）。
pub fn parse_frame(raw: &str) -> serde_json::Result<WsFrame> {
    serde_json::from_str(raw)
}

// ---------------------------------------------------------------------------
// 单测：纯逻辑，无网络。验收核心。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn req_id_格式正确() {
        let id = generate_req_id("aibot_subscribe");
        // 形如 aibot_subscribe_<毫秒数字>_<8 位 hex>。
        let parts: Vec<&str> = id.split('_').collect();
        assert!(parts.len() >= 3, "req_id 至少 3 段：{id}");
        // 前缀完整保留（aibot_subscribe 自身含下划线，故前两段合并为前缀）。
        assert!(id.starts_with("aibot_subscribe_"), "前缀不符：{id}");
        // 最后一段为 8 位 hex。
        let last = *parts.last().unwrap();
        assert_eq!(last.len(), 8, "随机段长度应为 8：{last}");
        assert!(
            last.chars().all(|c| c.is_ascii_hexdigit()),
            "随机段应为 hex：{last}"
        );
    }

    #[test]
    fn subscribe_帧字段() {
        let f = build_subscribe_frame("bot123", "sec456");
        let s = frame_to_string(&f).unwrap();
        assert!(s.contains("\"cmd\":\"aibot_subscribe\""), "cmd 缺失：{s}");
        assert!(s.contains("\"bot_id\":\"bot123\""), "bot_id 缺失：{s}");
        assert!(s.contains("\"secret\":\"sec456\""), "secret 缺失：{s}");
        // req_id 前缀正确。
        assert!(
            f.headers.req_id.starts_with("aibot_subscribe_"),
            "req_id 前缀不符：{}",
            f.headers.req_id
        );
    }

    #[test]
    fn ping_帧字段() {
        let f = build_ping_frame();
        assert_eq!(f.cmd.as_deref(), Some("ping"));
        assert!(f.body.is_none(), "ping 帧不应有 body");
        assert!(
            f.headers.req_id.starts_with("ping_"),
            "req_id 前缀不符：{}",
            f.headers.req_id
        );
    }

    #[test]
    fn send_markdown_帧字段() {
        let f = build_send_markdown_frame("user1", "hello");
        let s = frame_to_string(&f).unwrap();
        assert!(s.contains("\"cmd\":\"aibot_send_msg\""), "cmd 缺失：{s}");
        assert!(s.contains("\"chatid\":\"user1\""), "chatid 缺失：{s}");
        assert!(s.contains("\"msgtype\":\"markdown\""), "msgtype 缺失：{s}");
        assert!(
            s.contains("\"content\":\"hello\""),
            "markdown.content 缺失：{s}"
        );
    }

    #[test]
    fn parse_msg_callback_文本帧() {
        let raw = serde_json::json!({
            "cmd": "aibot_msg_callback",
            "headers": { "req_id": "aibot_msg_callback_1_abc" },
            "body": {
                "msgid": "m1",
                "aibotid": "bot1",
                "chattype": "single",
                "from": { "userid": "u42" },
                "msgtype": "text",
                "text": { "content": "你好" }
            }
        })
        .to_string();
        let frame = parse_frame(&raw).unwrap();
        let (msgid, msg) = parse_msg_callback(&frame).unwrap();
        assert_eq!(msgid, "m1");
        assert_eq!(msg.conv_id.0, "wecom:u42");
        assert_eq!(msg.sender.0, "u42");
        assert_eq!(msg.text.as_deref(), Some("你好"));
        assert!(msg.media.is_empty());
        assert!(matches!(msg.reply_hint, ReplyHint::None));
    }

    #[test]
    fn parse_msg_callback_非文本帧_text_为_none() {
        let raw = serde_json::json!({
            "cmd": "aibot_msg_callback",
            "headers": { "req_id": "x" },
            "body": {
                "msgid": "m2",
                "aibotid": "bot1",
                "chattype": "single",
                "from": { "userid": "u9" },
                "msgtype": "image",
                "image": { "url": "http://x/a.png" }
            }
        })
        .to_string();
        let frame = parse_frame(&raw).unwrap();
        let (msgid, msg) = parse_msg_callback(&frame).unwrap();
        assert_eq!(msgid, "m2");
        assert_eq!(msg.sender.0, "u9");
        assert!(msg.text.is_none(), "非文本帧 text 应为 None");
    }

    #[test]
    fn userid_from_conv_前缀处理() {
        assert_eq!(userid_from_conv(&ConvId("wecom:foo".into())), "foo");
        assert_eq!(userid_from_conv(&ConvId("foo".into())), "foo");
    }

    #[test]
    fn parse_frame_带_errcode_的_ack() {
        let raw = serde_json::json!({
            "headers": { "req_id": "aibot_send_msg_1_abc" },
            "errcode": 0,
            "errmsg": "ok"
        })
        .to_string();
        let frame = parse_frame(&raw).unwrap();
        assert!(frame.cmd.is_none(), "ack 帧通常无 cmd");
        assert_eq!(frame.errcode, Some(0));
        assert_eq!(frame.errmsg.as_deref(), Some("ok"));
    }
}
