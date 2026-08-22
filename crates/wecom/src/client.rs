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
    /// `reconnect`（P4-7 `/reconnect`）：notify_one 唤醒 select 丢弃 connect_and_serve
    /// future（连接随 future drop 关闭）→ 退避后重连；退避 sleep 期间通知存 permit。
    pub async fn run(
        self,
        inbound_tx: mpsc::Sender<InboundFrame>,
        mut outbound_rx: mpsc::Receiver<OutboundFrame>,
        reconnect: std::sync::Arc<tokio::sync::Notify>,
    ) {
        let mut backoff = Duration::from_secs(1);
        loop {
            tokio::select! {
                res = self.connect_and_serve(&inbound_tx, &mut outbound_rx) => match res {
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
                },
                _ = reconnect.notified() => {
                    info!(target: "wecom", "收到 /reconnect 指令，主动断开重连");
                    backoff = Duration::from_secs(1);
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
        // 1. 建连。P2-L/P2-9：远端必须 wss://（凭据保护），ws:// 仅允许 loopback
        // （测试/本地）。用 url::Url 解析真实 host 精确比较，避免 contains 子串匹配
        // 被 `ws://localhost.evil.com` / `ws://evil.com/?to=127.0.0.1` 绕过。
        let parsed = url::Url::parse(&self.ws_url).map_err(|e| {
            imagent_core::CoreError::Platform("wecom", format!("invalid ws_url: {e}"))
        })?;
        let host = parsed.host_str().unwrap_or("");
        let is_loopback = matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1");
        let host_ok = parsed.scheme() == "wss" || (parsed.scheme() == "ws" && is_loopback);
        if !matches!(parsed.scheme(), "wss" | "ws") || !host_ok {
            return Err(imagent_core::CoreError::Platform(
                "wecom",
                format!(
                    "ws_url 必须为 wss://（或 ws:// 仅 loopback）；收到 scheme={}, host={}：{}",
                    parsed.scheme(),
                    host,
                    self.ws_url
                ),
            ));
        }
        info!(target: "wecom", host = %parsed.host_str().unwrap_or("?"), "ws 连接中");
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
        ws.send(Message::text(sub_json)).await.map_err(|e| {
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
        } else {
            // R-5：认证失败（errcode≠0 / ack 解析失败 / 超时）必须 return Err 触发外层重连。
            // 否则进入收发循环空转发心跳，进程存活但无 inbound、/health 不报错，用户消息
            // 静默丢失且运维难发现（bot_id/secret 配错或被吊销/轮换时）。
            return Err(imagent_core::CoreError::Platform(
                "wecom",
                "subscribe 认证失败（errcode≠0 或 ack 异常）——请检查 bot_id/secret 是否正确/被吊销/已轮换".into(),
            ));
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
                        Some(Ok(Message::Ping(p))) => {
                            // P3-N3：显式回 Pong（tokio-tungstenite stream API 不自动回），
                            // 否则服务端 Ping 探活失败判掉线 → 频繁重连。
                            let _ = ws.send(Message::Pong(p)).await;
                            continue;
                        }
                        Some(Ok(Message::Pong(_))) => continue,
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
                            // R-6：入站回调 best-effort 推给 platform。P5-12：裸
                            // try_send 满即丢改为 1s 有界背压——消费端短暂抖动不再丢
                            // 用户消息；但不能无限 await（会饿死 select! 其它分支含
                            // 心跳，30s 无心跳被服务端断连），持续满 1s 才丢弃 + warn。
                            match tokio::time::timeout(
                                Duration::from_secs(1),
                                inbound_tx.send(frame),
                            )
                            .await
                            {
                                Ok(Ok(())) => {}
                                Ok(Err(_)) => {
                                    return Err(imagent_core::CoreError::Platform(
                                        "wecom",
                                        "入站 channel 已关闭（platform 退出）".into(),
                                    ));
                                }
                                Err(_) => {
                                    warn!(target: "wecom", "入站 channel 持续满 1s，丢弃回调帧");
                                }
                            }
                        }
                        Some("aibot_event_callback")
                        | Some("aibot_subscribe")
                        | Some("ping") => {
                            debug!(target: "wecom", cmd = ?frame.cmd, "收到 ack/event 帧");
                        }
                        _ => {
                            // P5-12：无 cmd 的 ack 帧（如 aibot_send_msg 回执）errcode≠0
                            // = 发送被服务端拒绝（限流/chatid 非法等）——升级 warn 让
                            // 可观测（fire-and-forget 架构下 dispatch 无从感知，至少
                            // 日志可查；req_id 关联重试/报错闭环需真机验证 ack 语义后做）。
                            if frame.errcode.is_some_and(|c| c != 0) {
                                warn!(
                                    target: "wecom",
                                    req_id = %frame.headers.req_id,
                                    errcode = frame.errcode,
                                    errmsg = frame.errmsg.as_deref().unwrap_or(""),
                                    "出站请求被服务端拒绝（ack errcode≠0）"
                                );
                            } else {
                                debug!(target: "wecom", cmd = ?frame.cmd, "收到其它帧");
                            }
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
                    ws.send(Message::text(json)).await.map_err(|e| {
                        imagent_core::CoreError::Platform("wecom", format!("ws send: {e}"))
                    })?;
                    debug!(target: "wecom", "已发出站帧");
                }

                // 心跳。
                _ = heartbeat.tick() => {
                    let ping = frame_to_string(&build_ping_frame()).map_err(|e| {
                        imagent_core::CoreError::Platform("wecom", format!("serialize ping: {e}"))
                    })?;
                    if let Err(e) = ws.send(Message::text(ping)).await {
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

/// 凭据连通性探针（P6 遗留补齐，`imagent setup` 用）：建连 → 发 `aibot_subscribe`
/// → 等 ack（errcode==0 即凭据有效）→ 直接返回，连接随 drop 关闭。
/// 企微无独立 HTTP token 探针接口，WS subscribe ack 是唯一的凭据校验面——
/// bot_id/secret 配错或被吊销时 errcode≠0，超时/断开按失败处理。
pub async fn probe_credentials(
    bot_id: &str,
    secret: &str,
    ws_url: &str,
) -> imagent_core::Result<()> {
    let sub = build_subscribe_frame(bot_id, secret);
    let sub_json = frame_to_string(&sub).map_err(|e| {
        imagent_core::CoreError::Platform("wecom", format!("serialize subscribe: {e}"))
    })?;
    let (mut ws, _resp) = connect_async(ws_url)
        .await
        .map_err(|e| imagent_core::CoreError::Platform("wecom", format!("ws connect: {e}")))?;
    ws.send(Message::text(sub_json))
        .await
        .map_err(|e| imagent_core::CoreError::Platform("wecom", format!("send subscribe: {e}")))?;
    match tokio::time::timeout(SUBSCRIBE_ACK_TIMEOUT, ws.next()).await {
        Ok(Some(Ok(msg))) => {
            let text = msg
                .into_text()
                .map_err(|e| imagent_core::CoreError::Platform("wecom", format!("ack 帧: {e}")))?;
            let frame = parse_frame(&text).map_err(|e| {
                imagent_core::CoreError::Platform("wecom", format!("ack 解析: {e}"))
            })?;
            if frame.errcode.unwrap_or(-1) == 0 {
                Ok(())
            } else {
                Err(imagent_core::CoreError::Platform(
                    "wecom",
                    format!(
                        "凭据校验失败 errcode={} errmsg={:?}——请检查 bot_id/secret 是否正确/被吊销/已轮换",
                        frame.errcode.unwrap_or(-1),
                        frame.errmsg.unwrap_or_default()
                    ),
                ))
            }
        }
        _ => Err(imagent_core::CoreError::Platform(
            "wecom",
            "未收到 subscribe ack（超时/连接断开）".into(),
        )),
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
        let reconnect = std::sync::Arc::new(tokio::sync::Notify::new());

        let res = tokio::time::timeout(
            Duration::from_millis(200),
            client.run(inbound_tx, outbound_rx, reconnect),
        )
        .await;
        // run 永不返回 → timeout 触发（Err(Elapsed)）= 正常。
        assert!(res.is_err(), "run 应持续重连而非返回");
    }

    /// P6 遗留补齐：凭据探针——连不上的地址应返回 Err（而非挂起/panic）。
    #[tokio::test]
    async fn probe_credentials_fails_fast_on_unreachable() {
        let res = probe_credentials("b", "s", "ws://127.0.0.1:1").await;
        assert!(res.is_err(), "不可达地址应 Err：{res:?}");
    }
}
