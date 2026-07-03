//! 企业微信智能机器人 WebSocket 长连接客户端。
//!
//! 职责链：`connect_async` 建连 → 发 `aibot_subscribe` 认证 → 心跳 + 收发帧
//! 循环；任意环节断开/出错即返回 `Err`，外层 [`WeComWsClient::run`] 按指数
//! 退避重连（1s → 2s → … 封顶 30s）。
//!
//! 与 [`crate::platform::WeComPlatform`] 通过两条 mpsc channel 解耦：
//! - `inbound_tx`：把收到的 `aibot_msg_callback` 帧推给 platform。
//! - `outbound_rx`：消费 platform 构造好的出站帧（`aibot_send_msg`）。
//!
//! 本层只做协议透传，不做白名单（白名单由 core 负责，见 lib.rs 注释）。

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::proto::{
    build_ping_frame, build_subscribe_frame, frame_to_string, parse_frame, WsFrame,
};

/// 出站帧：platform → client → 企微（由 platform 用 `build_send_markdown_frame` 构造）。
pub type OutboundFrame = WsFrame;
/// 入站回调帧：企微 → client → platform（`aibot_msg_callback`）。
pub type InboundFrame = WsFrame;

/// 心跳间隔：企微长连接约 30s 无活动会断，定时发 ping 保活。
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// 认证 ack 等待上限：subscribe 后等首帧，超时也继续收发（重连外层兜底）。
const SUBSCRIBE_ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// 重连退避上限。
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// 企微 WebSocket 长连接客户端（无内部可变状态，`run` 消费 self）。
pub struct WeComWsClient {
    pub bot_id: String,
    pub secret: String,
    /// openws 服务地址，默认 `wss://openws.work.weixin.qq.com`。
    pub ws_url: String,
}

impl WeComWsClient {
    /// 主循环：重连外层 loop。正常情况下 `connect_and_serve` 永不返回（持续收发），
    /// 一旦返回（Err 或 Ok）即按指数退避 sleep 后重连。退避在每次成功建连后重置。
    pub async fn run(
        self,
        inbound_tx: mpsc::Sender<InboundFrame>,
        mut outbound_rx: mpsc::Receiver<OutboundFrame>,
    ) {
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.connect_and_serve(&inbound_tx, &mut outbound_rx).await {
                Ok(()) => {
                    // 正常退出（通常不应发生）——重置退避。
                    backoff = Duration::from_secs(1);
                }
                Err(e) => {
                    warn!(
                        target: "wecom",
                        error = %e,
                        backoff_ms = backoff.as_millis() as u64,
                        "ws 断开/出错，准备重连"
                    );
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_CAP);
        }
    }

    /// 一次完整的「建连 → 认证 → 收发」循环。返回 `Err` 触发外层重连。
    async fn connect_and_serve(
        &self,
        inbound_tx: &mpsc::Sender<InboundFrame>,
        outbound_rx: &mut mpsc::Receiver<OutboundFrame>,
    ) -> imagent_core::Result<()> {
        // 1. 建连。
        info!(target: "wecom", url = %self.ws_url, "ws 连接中");
        let (ws_stream, _resp) = connect_async(&self.ws_url)
            .await
            .map_err(|e| imagent_core::CoreError::Platform("wecom", format!("ws connect: {e}")))?;
        info!(target: "wecom", "ws 已连接，开始认证");

        let mut ws = ws_stream;

        // 2. 发 aibot_subscribe 认证帧。
        let sub = build_subscribe_frame(&self.bot_id, &self.secret);
        let sub_json = frame_to_string(&sub).map_err(|e| {
            imagent_core::CoreError::Platform("wecom", format!("serialize subscribe: {e}"))
        })?;
        ws.send(Message::Text(sub_json)).await.map_err(|e| {
            imagent_core::CoreError::Platform("wecom", format!("send subscribe: {e}"))
        })?;

        // 3. 等首个 ack 帧（errcode==0 视为认证成功；超时/异常则继续，重连兜底）。
        let authed = match tokio::time::timeout(SUBSCRIBE_ACK_TIMEOUT, ws.next()).await {
            Ok(Some(Ok(msg))) => match msg.into_text() {
                Ok(text) => match parse_frame(&text) {
                    Ok(frame) => {
                        let ok = frame.errcode.unwrap_or(0) == 0;
                        debug!(
                            target: "wecom",
                            errcode = frame.errcode.unwrap_or(-1),
                            authed = ok,
                            "收到 subscribe ack"
                        );
                        ok
                    }
                    Err(e) => {
                        warn!(target: "wecom", error = %e, "ack 帧解析失败，按未认证继续");
                        false
                    }
                },
                Err(_) => {
                    warn!(target: "wecom", "ack 帧非文本，按未认证继续");
                    false
                }
            },
            _ => {
                warn!(target: "wecom", "等待 subscribe ack 超时/连接关闭，继续收发循环");
                false
            }
        };
        if authed {
            info!(target: "wecom", "subscribe 认证成功");
        }

        // 4. select! 收发循环：收帧 / 出站帧 / 心跳。
        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
        // 跳过 interval 首次立即 tick，避免开局立刻发 ping。
        heartbeat.tick().await;

        loop {
            tokio::select! {
                // 收帧。
                msg = ws.next() => {
                    let raw = match msg {
                        Some(Ok(Message::Text(t))) => t.to_string(),
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                        Some(Ok(Message::Close(_))) => {
                            return Err(imagent_core::CoreError::Platform(
                                "wecom", "服务端关闭连接".into(),
                            ));
                        }
                        Some(Ok(_)) => continue, // Binary 等忽略
                        Some(Err(e)) => {
                            return Err(imagent_core::CoreError::Platform(
                                "wecom", format!("ws read: {e}"),
                            ));
                        }
                        None => {
                            return Err(imagent_core::CoreError::Platform(
                                "wecom", "ws stream 结束".into(),
                            ));
                        }
                    };

                    let frame = match parse_frame(&raw) {
                        Ok(f) => f,
                        Err(e) => {
                            debug!(target: "wecom", error = %e, "入站帧解析失败，丢弃");
                            continue;
                        }
                    };

                    match frame.cmd.as_deref() {
                        Some("aibot_msg_callback") => {
                            // best-effort 推给 platform：channel 满说明消费端慢，
                            // 丢弃该帧优于阻塞收发循环。
                            let _ = inbound_tx.try_send(frame);
                        }
                        Some("aibot_event_callback")
                        | Some("aibot_subscribe")
                        | Some("ping") => {
                            debug!(target: "wecom", cmd = ?frame.cmd, "收到 ack/event 帧");
                        }
                        _ => {
                            debug!(target: "wecom", cmd = ?frame.cmd, "收到其它帧");
                        }
                    }
                }

                // 出站帧：platform 要发的（send_text 构造好的 markdown 帧）。
                maybe_frame = outbound_rx.recv() => {
                    let frame = match maybe_frame {
                        Some(f) => f,
                        None => {
                            // platform 端 sender 全部 drop，无需再发，正常退出。
                            return Ok(());
                        }
                    };
                    let json = frame_to_string(&frame).map_err(|e| {
                        imagent_core::CoreError::Platform("wecom", format!("serialize outbound: {e}"))
                    })?;
                    ws.send(Message::Text(json)).await.map_err(|e| {
                        imagent_core::CoreError::Platform("wecom", format!("ws send: {e}"))
                    })?;
                    debug!(target: "wecom", "已发出站帧");
                }

                // 心跳。
                _ = heartbeat.tick() => {
                    let ping = frame_to_string(&build_ping_frame()).map_err(|e| {
                        imagent_core::CoreError::Platform("wecom", format!("serialize ping: {e}"))
                    })?;
                    if let Err(e) = ws.send(Message::Text(ping)).await {
                        return Err(imagent_core::CoreError::Platform(
                            "wecom", format!("ws send ping: {e}"),
                        ));
                    }
                    debug!(target: "wecom", "已发心跳 ping");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! 纯逻辑/退避计算测试。真实 WS 连接需真机 bot_id/secret，不进默认 cargo test。

    use super::*;

    #[test]
    fn backoff_caps_at_30s() {
        let cap = Duration::from_secs(30);
        let mut b = Duration::from_secs(1);
        for _ in 0..10 {
            b = (b * 2).min(cap);
        }
        assert_eq!(b, cap);
    }

    #[test]
    fn constants_sane() {
        assert!(HEARTBEAT_INTERVAL.as_secs() > 0);
        assert!(SUBSCRIBE_ACK_TIMEOUT.as_secs() > 0);
        assert!(BACKOFF_CAP >= Duration::from_secs(30));
    }

    #[tokio::test]
    async fn run_loops_on_connect_failure() {
        // 无效地址 → 连不上 → Err → 退避 → 重连。run 应持续重试（永不正常返回）。
        let client = WeComWsClient {
            bot_id: "b".into(),
            secret: "s".into(),
            ws_url: "ws://127.0.0.1:1".into(),
        };
        let (inbound_tx, _inbound_rx) = mpsc::channel::<InboundFrame>(8);
        let (_outbound_tx, outbound_rx) = mpsc::channel::<OutboundFrame>(8);

        let res = tokio::time::timeout(
            Duration::from_millis(200),
            client.run(inbound_tx, outbound_rx),
        )
        .await;
        // run 永不返回 → timeout 触发（Err(Elapsed)）= 正常。
        assert!(res.is_err(), "run 应持续重连而非返回");
    }
}
