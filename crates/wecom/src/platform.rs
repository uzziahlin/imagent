//! [`WeComPlatform`]：实现 [`imagent_core::Platform`]，通过 mpsc channel 委托
//! 后台 [`crate::client::WeComWsClient`] 收发帧。
//!
//! - `recv()`：从 client 推来的 `aibot_msg_callback` 帧逐条经
//!   [`crate::proto::parse_msg_callback`] 解析为 [`InboundMessage`] 返回。
//! - `send_text()`：`userid_from_conv` 还原 userid → `build_send_markdown_frame`
//!   → 出站 channel。hint 忽略（WeCom 仅靠 conv_id 解析 userid）。
//! - `send_media()` / `send_typing()`：MVP 空实现（媒体需 upload_media 三步，留后）。

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, warn};

use imagent_core::{ConvId, CoreError, InboundMessage, MediaRef, Platform, ReplyHint, Result};

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
    /// 已解析的入站消息缓冲，`recv` 弹队首。
    pending: Arc<Mutex<VecDeque<InboundMessage>>>,
}

impl WeComPlatform {
    /// 构造并后台 spawn：
    /// 1. client `run` task（建连/认证/心跳/重连/收发）；
    /// 2. drain task：把 client 推来的 `aibot_msg_callback` 帧解析入 pending 队列。
    pub fn new(bot_id: String, secret: String, ws_url: String) -> Self {
        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundFrame>(64);
        let (outbound_tx, outbound_rx) = mpsc::channel::<OutboundFrame>(64);

        // 后台 client task：连接立即开始。
        let client = WeComWsClient {
            bot_id,
            secret,
            ws_url,
        };
        tokio::spawn(async move {
            client.run(inbound_tx, outbound_rx).await;
        });

        // 后台 drain task：inbound channel → pending 队列。
        let pending: Arc<Mutex<VecDeque<InboundMessage>>> = Arc::new(Mutex::new(VecDeque::new()));
        let pending_clone = Arc::clone(&pending);
        tokio::spawn(async move {
            while let Some(frame) = inbound_rx.recv().await {
                match parse_msg_callback(&frame) {
                    Ok(msg) => {
                        let mut q = pending_clone.lock().await;
                        q.push_back(msg);
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
            pending,
        }
    }
}

#[async_trait]
impl Platform for WeComPlatform {
    async fn recv(&self) -> Result<InboundMessage> {
        loop {
            // 1. 先弹已解析的缓冲。
            {
                let mut q = self.pending.lock().await;
                if let Some(msg) = q.pop_front() {
                    return Ok(msg);
                }
            }
            // 2. 缓冲空：等 drain task 攒入。这里短 sleep 轮询，避免 channel 已被
            //    drain task 独占持有（recv 端在 drain task 内）。延迟可忽略。
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
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
    async fn drain_parses_callback_into_inbound() {
        let pending: Arc<Mutex<VecDeque<InboundMessage>>> = Arc::new(Mutex::new(VecDeque::new()));
        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundFrame>(8);

        let pc = Arc::clone(&pending);
        let handle = tokio::spawn(async move {
            while let Some(frame) = inbound_rx.recv().await {
                if let Ok(msg) = parse_msg_callback(&frame) {
                    pc.lock().await.push_back(msg);
                }
            }
        });

        inbound_tx
            .send(mk_callback_frame("Alice", "hi"))
            .await
            .unwrap();
        // 给 drain task 处理时间。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(inbound_tx);
        let _ = handle.await;

        let msg = pending.lock().await.pop_front().unwrap();
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
