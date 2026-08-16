//! [`WeComPlatform`]：实现 [`imagent_core::Platform`]，通过 mpsc channel 委托
//! 后台 [`crate::client::WeComWsClient`] 收发帧。
//!
//! - `recv()`：从 client 推来的 `aibot_msg_callback` 帧逐条经
//!   [`crate::proto::parse_msg_callback`] 解析为 [`InboundMessage`] 返回。
//! - `send_text()`：`userid_from_conv` 还原 userid → `build_send_markdown_frame`
//!   → 出站 channel。hint 忽略（WeCom 仅靠 conv_id 解析 userid）。
//! - `send_media()` / `send_typing()`：MVP 空实现（媒体需 upload_media 三步，留后）。

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
        let frame = build_send_markdown_frame(&userid, text);
        self.outbound_tx.send(frame).await.map_err(|_| {
            CoreError::Platform(PLATFORM, "出站 channel 已关闭（client 已退出）".into())
        })?;
        Ok(())
    }

    async fn send_media(&self, _conv: &ConvId, _media: &MediaRef, _hint: &ReplyHint) -> Result<()> {
        // TODO: WeCom 媒体需 upload_media 三步（get/upload/finish）获取 media_id，
        // 再以 aibot_send_msg 携带 media_id 发送。MVP 暂不支持，留后。
        Ok(())
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
