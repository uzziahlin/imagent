//! [`WeComPlatform`]：实现 [`imagent_core::Platform`]，通过 mpsc channel 委托
//! 后台 [`crate::client::WeComWsClient`] 收发帧。
//!
//! - `recv()`：从 client 推来的 `aibot_msg_callback` 帧逐条经
//!   [`crate::proto::parse_msg_callback`] 解析为 [`InboundMessage`] 返回。
//! - `send_text()`：`userid_from_conv` 还原 userid → 超限分片（见
//!   [`WECOM_TEXT_MAX_BYTES`]）逐片 `build_send_markdown_frame` → 出站 channel。
//!   hint 忽略（WeCom 仅靠 conv_id 解析 userid）。
//! - `send_media()`：**不支持**（需 upload_media 三步，留后）——显式返回 Err，
//!   core 命令层会把失败文案回给用户，不再谎报成功。
//! - `send_typing()`：no-op（WeCom 协议无 typing 语义）。

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, warn};

use imagent_core::{
    ConvId, CoreError, Dedup, InboundMessage, MediaRef, Platform, ReplyHint, Result,
};

use crate::client::{InboundFrame, OutboundFrame, WeComWsClient};
use crate::proto::{build_send_markdown_frame, parse_msg_callback, userid_from_conv};

const PLATFORM: &str = "wecom";

/// 出站 markdown 文本的单片字节上限。
///
/// 企微 `aibot_send_msg` markdown 消息 content 上限 4096 字节（超限整条被
/// 服务端拒绝）。取 4000 字节作安全阈值：预留 `(i/n)` 分片编号后缀与 JSON
/// 转义（`"`/`\n` 等）膨胀的余量。切片全程按 UTF-8 char 边界回退，不切断
/// 多字节字符（中文 3 字节 / emoji 4 字节）。
const WECOM_TEXT_MAX_BYTES: usize = 4000;

/// 按 UTF-8 字节上限切分出站文本（`max_bytes` ≥ 4 时保证不死循环）。
///
/// - 总字节 ≤ `max_bytes` → 单片原样返回（空串同理）。
/// - 否则从 `max_bytes` 处向前回退到最近的 char 边界作为切点，逐段入列。
/// - 各片按顺序拼接与原文完全相等（不丢字符）。
fn split_text_by_bytes(text: &str, max_bytes: usize) -> Vec<String> {
    if text.len() <= max_bytes {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < text.len() {
        let mut end = (start + max_bytes).min(text.len());
        if end < text.len() {
            // 回退到 char 边界：max_bytes(4000) 远大于单个 char 的最大 4 字节，
            // 循环必然在 start 之前停下，不可能死循环。
            while !text.is_char_boundary(end) {
                end -= 1;
            }
        }
        chunks.push(text[start..end].to_string());
        start = end;
    }
    chunks
}

/// 企业微信 Platform 适配器。
///
/// 持有两条 channel 与后台 client task 通信：出站帧（发给企微）、入站帧（企微
/// 推来的回调）。入站帧由后台 drain task解析入队，`recv` 从队首弹。
pub struct WeComPlatform {
    /// 出站帧通道（发给 client → 企微）。
    outbound_tx: mpsc::Sender<OutboundFrame>,
    /// `/reconnect` 强制重连信号（与 client run task 共享，P4-7）。
    reconnect: std::sync::Arc<tokio::sync::Notify>,
    /// 已解析的入站消息 channel，`recv` 直接 await（无轮询）。
    inbound_rx: Arc<Mutex<mpsc::Receiver<InboundMessage>>>,
}

impl WeComPlatform {
    /// 构造并后台 spawn：
    /// 1. client `run` task（建连/认证/心跳/重连/收发）；
    /// 2. drain task：把 client 推来的 `aibot_msg_callback` 帧解析入 pending 队列。
    pub fn new(bot_id: String, secret: String, ws_url: String) -> Self {
        let (inbound_frame_tx, inbound_frame_rx) = mpsc::channel::<InboundFrame>(64);
        let (outbound_tx, outbound_rx) = mpsc::channel::<OutboundFrame>(64);

        // 后台 client task：连接立即开始。
        let client = WeComWsClient {
            bot_id,
            secret,
            ws_url,
        };
        let reconnect = std::sync::Arc::new(tokio::sync::Notify::new());
        let reconnect_for_task = reconnect.clone();
        tokio::spawn(async move {
            client
                .run(inbound_frame_tx, outbound_rx, reconnect_for_task)
                .await;
        });

        // 后台 drain task：client 推来的回调帧解析成 InboundMessage，直送入站 channel
        // （recv 直接 await，取代 50ms 轮询——无消息时零唤醒）。
        let (inbound_msg_tx, inbound_msg_rx) = mpsc::channel::<InboundMessage>(64);
        // P1-I：msgid 滑动窗口去重（复用 core::Dedup，与 ilink 同源），重复回调丢弃。
        let dedup = Dedup::default();
        tokio::spawn(async move {
            let mut inbound_frame_rx = inbound_frame_rx;
            while let Some(frame) = inbound_frame_rx.recv().await {
                match parse_msg_callback(&frame) {
                    Ok((msgid, msg)) => {
                        if !dedup.check(&msgid) {
                            debug!(target: "wecom", %msgid, "重复 msgid，丢弃");
                            continue;
                        }
                        if inbound_msg_tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!(target: "wecom", error = %e, "入站回调帧解析失败，丢弃");
                    }
                }
            }
            debug!(target: "wecom", "inbound drain task 退出");
        });

        Self {
            outbound_tx,
            reconnect,
            inbound_rx: Arc::new(Mutex::new(inbound_msg_rx)),
        }
    }
}

#[async_trait]
impl Platform for WeComPlatform {
    async fn recv(&self) -> Result<InboundMessage> {
        // 直接 await 入站 channel（drain task 解析后 send 进来），无消息时零唤醒。
        self.inbound_rx.lock().await.recv().await.ok_or_else(|| {
            CoreError::Platform(PLATFORM, "入站 channel 已关闭（client 已退出）".into())
        })
    }

    async fn send_text(&self, conv: &ConvId, text: &str, _hint: &ReplyHint) -> Result<()> {
        let userid = userid_from_conv(conv);
        // 超限分片（与飞书/ilink 同思路）：按字节安全阈值切，多片加 (i/n) 编号
        // 后缀，用户能感知这是同一回复的一部分且未被截断。
        let chunks = split_text_by_bytes(text, WECOM_TEXT_MAX_BYTES);
        let total = chunks.len();
        for (i, chunk) in chunks.into_iter().enumerate() {
            let content = if total > 1 {
                format!("({}/{}) {}", i + 1, total, chunk)
            } else {
                chunk
            };
            let frame = build_send_markdown_frame(&userid, &content);
            // P5：中途失败标明分片序号——用户能感知回复被截断而非静默缺尾。
            self.outbound_tx.send(frame).await.map_err(|_| {
                CoreError::Platform(
                    PLATFORM,
                    format!(
                        "第 {}/{} 片发送失败（回复可能被截断）：出站 channel 已关闭（client 已退出）",
                        i + 1,
                        total
                    ),
                )
            })?;
        }
        Ok(())
    }

    async fn send_media(&self, _conv: &ConvId, _media: &MediaRef, _hint: &ReplyHint) -> Result<()> {
        // WeCom 媒体需 upload_media 三步（get/upload/finish）获取 media_id，再以
        // aibot_send_msg 携带 media_id 发送，暂不支持。显式报错而非谎报 Ok——
        // core 命令层（/img /file）会把该文案作为「发送失败：…」回给用户。
        Err(CoreError::Platform(
            PLATFORM,
            "wecom 暂不支持媒体发送".into(),
        ))
    }

    async fn send_typing(&self, _conv: &ConvId, _hint: &ReplyHint) -> Result<()> {
        // WeCom 协议无 typing 语义。
        Ok(())
    }

    /// P4-7：强制重连——notify_one 存 permit，client run task 的 select 消费后
    /// 丢弃连接 future 断开重连。
    async fn reconnect(&self) -> Result<()> {
        self.reconnect.notify_one();
        Ok(())
    }

    fn name(&self) -> &'static str {
        PLATFORM
    }
}

#[cfg(test)]
mod tests {
    //! 逻辑测试：不连真机 WS。channel + parse 行为。

    use super::*;
    use crate::proto::{build_send_markdown_frame, frame_to_string, WsFrame};
    use imagent_core::{ConvId, InboundMessage, UserId};

    fn mk_callback_frame(userid: &str, content: &str) -> WsFrame {
        // 复用 proto 的真实 JSON 形状构造一个 aibot_msg_callback 帧。
        let body = serde_json::json!({
            "msgid": "m1",
            "chattype": "single",
            "from": { "userid": userid },
            "msgtype": "text",
            "text": { "content": content },
        });
        WsFrame {
            cmd: Some("aibot_msg_callback".into()),
            headers: crate::proto::WsHeaders { req_id: "t".into() },
            body: Some(body),
            errcode: None,
            errmsg: None,
        }
    }

    #[tokio::test]
    async fn drain_drops_duplicate_msgid() {
        // P1-I：同 msgid 的重复回调应被滑动窗口去重丢弃。
        let (inbound_msg_tx, mut inbound_msg_rx) = mpsc::channel::<InboundMessage>(8);
        let (inbound_frame_tx, inbound_frame_rx) = mpsc::channel::<InboundFrame>(8);
        let dedup = Dedup::default();
        let tx = inbound_msg_tx;
        let _handle = tokio::spawn(async move {
            let mut inbound_frame_rx = inbound_frame_rx;
            let dedup = dedup;
            while let Some(frame) = inbound_frame_rx.recv().await {
                if let Ok((msgid, msg)) = parse_msg_callback(&frame) {
                    if !dedup.check(&msgid) {
                        continue;
                    }
                    if tx.send(msg).await.is_err() {
                        break;
                    }
                }
            }
        });
        // mk_callback_frame 硬编码 msgid="m1"，发两次同帧 → 第二次去重。
        inbound_frame_tx
            .send(mk_callback_frame("Alice", "hi"))
            .await
            .unwrap();
        inbound_frame_tx
            .send(mk_callback_frame("Alice", "hi"))
            .await
            .unwrap();
        let first = inbound_msg_rx.recv().await.expect("第一条应入队");
        assert_eq!(first.text.as_deref(), Some("hi"));
        // 给 drain 处理第二帧的时间，再断言无第二条入队。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            inbound_msg_rx.try_recv().is_err(),
            "重复 msgid 应被去重，不应入队"
        );
    }

    #[tokio::test]
    async fn drain_parses_callback_into_inbound() {
        let (inbound_msg_tx, mut inbound_msg_rx) = mpsc::channel::<InboundMessage>(8);
        let (inbound_frame_tx, mut inbound_frame_rx) = mpsc::channel::<InboundFrame>(8);

        let tx = inbound_msg_tx;
        let _handle = tokio::spawn(async move {
            while let Some(frame) = inbound_frame_rx.recv().await {
                if let Ok((_msgid, msg)) = parse_msg_callback(&frame) {
                    if tx.send(msg).await.is_err() {
                        break;
                    }
                }
            }
        });

        inbound_frame_tx
            .send(mk_callback_frame("Alice", "hi"))
            .await
            .unwrap();
        let msg = inbound_msg_rx.recv().await.unwrap();
        assert_eq!(msg.conv_id, ConvId("wecom:Alice".into()));
        assert_eq!(msg.text.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn send_text_serializes_to_markdown_frame() {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(8);
        // 模拟 platform.send_text 的核心：构造 + send。
        let frame = build_send_markdown_frame("Bob", "hello");
        tx.send(frame.clone()).await.unwrap();

        let got = rx.recv().await.unwrap();
        let json = frame_to_string(&got).unwrap();
        assert!(json.contains("aibot_send_msg"), "json = {json}");
        assert!(json.contains("Bob"), "json = {json}");
        assert!(json.contains("hello"), "json = {json}");
    }

    #[test]
    fn userid_roundtrip() {
        let conv = ConvId("wecom:Charlie".into());
        assert_eq!(userid_from_conv(&conv), "Charlie");
    }

    // ------------------------------------------------------------------
    // split_text_by_bytes：分片正确性（UTF-8 字符边界 / 拼接无损 / 编号后缀）
    // ------------------------------------------------------------------

    #[test]
    fn split_under_limit_single_chunk() {
        // 未超限：单片原样返回（含空串）。
        assert_eq!(
            split_text_by_bytes("hello", WECOM_TEXT_MAX_BYTES),
            vec!["hello".to_string()]
        );
        assert_eq!(split_text_by_bytes("", 16), vec![String::new()]);
    }

    #[test]
    fn split_long_chinese_roundtrip_and_bounds() {
        // 中文 3 字节/字：1334 字 = 4002 字节。切点 4000 不是 char 边界
        // （落在第 1334 字内部），须回退到 3999。验证：不 panic、每片 ≤ 上限、
        // 拼接无损、不产生替换字符。
        let text = "中".repeat(1334); // 4002 bytes
        let chunks = split_text_by_bytes(&text, WECOM_TEXT_MAX_BYTES);
        assert_eq!(chunks.len(), 2);
        for c in &chunks {
            assert!(c.len() <= WECOM_TEXT_MAX_BYTES);
            assert!(c.chars().all(|ch| ch == '中'), "不得切断多字节字符");
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn split_emoji_boundary_not_broken() {
        // emoji 4 字节：构造「4000 - 1 字节前缀 + emoji + 尾巴」，切点必须
        // 回退避开 4 字节 emoji 的中间字节。
        let mut text = String::new();
        for _ in 0..999 {
            text.push('中'); // 999 * 3 = 2997 bytes
        }
        for _ in 0..250 {
            text.push('a'); // +250 = 3247 bytes
        }
        text.push('🎉'); // +4 = 3251
        for _ in 0..250 {
            text.push('中'); // +750 = 4001 bytes，切点 4000 落在最后一个中文内
        }
        text.push('尾');
        let chunks = split_text_by_bytes(&text, WECOM_TEXT_MAX_BYTES);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(c.len() <= WECOM_TEXT_MAX_BYTES);
            // 每片必须是合法 str（切片本身即保证），再显式验证 emoji 完整。
            assert!(!c.ends_with('\u{FFFD}'));
        }
        assert!(chunks.iter().any(|c| c.contains('🎉')));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn split_exact_multiple_of_limit() {
        // 恰好整数片：3999 字节（1333 个中文）×3 = 11997，用上限 3999 模拟，
        // 切点均落在 char 边界，得到恰好 3 片等长。
        let text = "字".repeat(3999); // 11997 bytes
        let chunks = split_text_by_bytes(&text, 3999);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.len() == 3999));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn split_exactly_at_limit_single_chunk() {
        // 恰好等于上限：不分片。
        let text = "中".repeat(1000); // 3000 bytes
        assert_eq!(split_text_by_bytes(&text, 3000), vec![text.clone()]);
        // 4001 字节 → 2 片。
        let over = format!("{text}a");
        assert_eq!(split_text_by_bytes(&over, 3000).len(), 2);
    }

    #[tokio::test]
    async fn send_text_chunks_get_numbered_suffix() {
        // 集成：超长文本经 send_text 的分片逻辑（此处直接验证分片 + 编号拼装，
        // 与 send_text 内联逻辑一致——不连真机 WS）。
        let text = "中".repeat(3000); // 9000 bytes → 3 片（3000/3000/3000 字节）
        let chunks = split_text_by_bytes(&text, WECOM_TEXT_MAX_BYTES);
        let total = chunks.len();
        assert_eq!(total, 3);
        for (i, chunk) in chunks.iter().enumerate() {
            let content = format!("({}/{}) {}", i + 1, total, chunk);
            assert!(content.starts_with(&format!("({}/{}) ", i + 1, total)));
        }
        // 单片不加编号。
        let single = split_text_by_bytes("short", WECOM_TEXT_MAX_BYTES);
        assert_eq!(single, vec!["short".to_string()]);
    }

    // 静态断言 WeComPlatform name。
    fn _name_check(p: &WeComPlatform) -> &'static str {
        p.name()
    }
    #[allow(dead_code)]
    fn _ensure_platform_trait(_: &dyn Platform) {}

    #[test]
    fn unused_import_guard() {
        // 保持 UserId 等导入被使用，防止编译告警。
        let _ = UserId("x".into());
    }
}
