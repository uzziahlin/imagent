//! 网关侧 WS 客户端任务：出站连 relay `/gateway`，帧收发 + 重连/心跳/停机。
//!
//! 职责链（参照 wecom client.rs 的成熟模式）：`connect_async` 建连（`Authorization:
//! Bearer <token>` 头直通认证——Rust 客户端无需 hello 帧）→ 等 relay 推来的
//! `hello_ack` 确认 → 收发帧循环；任意环节断开/出错即返回 `Err`，外层
//! [`WebClient::run`] 按指数退避重连（1s → 2s → … 封顶 30s，成功建连重置）。
//!
//! 与 [`crate::platform::WebPlatform`] 通过三条 channel 解耦：
//! - `inbound_tx`：浏览器业务帧映射成的 [`InboundMessage`] → platform.recv；
//! - `sync_tx`：sync 请求 → platform 的 sync responder（D6 会话恢复）；
//! - `outbound_rx`：platform 序列化好的出站帧（JSON 文本）→ relay → 浏览器。
//!
//! 协议细节（§4）：relay 对浏览器侧有 90s idle 超时，网关 30s 发协议层
//! `Message::Ping` 保活；浏览器应用层 `ping` JSON 帧由本层直接回 `pong`
//! （不进 recv）；`machine` / `error` / `hello_ack` 是 relay 自产帧，本层分拣
//! 自理（在线标志 / warn / debug），不进业务通道。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use imagent_core::{CoreError, InboundMessage, Result};

use crate::{map_inbound_frame, InboundAction, TARGET};

/// 建连后的 WS 流类型（connect_async 的产物）。
type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// hello_ack 等待上限：Bearer 认证通过后 relay 立即推 ack，超时按认证失败
/// 重连（不空转发——对齐 wecom R-5「认证失败必须重连」纪律）。
const HELLO_ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// 心跳间隔：relay idle 超时 90s，30s 协议层 Ping 保活。
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// 重连退避上限。
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// 网关侧 WS 客户端（无内部可变状态；`run` 消费 self，由 platform spawn）。
pub(crate) struct WebClient {
    /// relay 网关端点完整地址：`wss://…/imagent/gateway`（公网，Caddy strip
    /// 前缀）或 `ws://127.0.0.1:<port>/gateway`（本地/测试）。
    pub(crate) relay_url: String,
    pub(crate) gateway_token: String,
    /// 无 convId 的浏览器消息落到的默认会话名（映射 message 帧用）。
    pub(crate) default_session: String,
    /// 在线标志（client 维护：连接成功 true，断开 false）——doctor_probes
    /// 与日志用。relay 的 machine 帧只推浏览器，网关侧标志由连接生命周期
    /// 自维护；分拣到 machine 时也顺带同步（防御性，正常收不到）。
    pub(crate) online: Arc<AtomicBool>,
    pub(crate) inbound_tx: mpsc::Sender<InboundMessage>,
    pub(crate) sync_tx: mpsc::Sender<()>,
}

impl WebClient {
    /// 主循环：重连外层 loop。`connect_and_serve` 正常态永不返回，一旦返回
    /// （Err 或 Ok）即按指数退避 sleep 后重连；退避在成功建连后重置。
    /// `reconnect`（`/reconnect`）：notify_one 唤醒 select 丢弃
    /// connect_and_serve future（连接随 future drop 关闭）→ 退避后重连。
    /// `shutdown`（平台 Drop 时 cancel）：迭代开头 / 连接保持态 / 退避 sleep
    /// 三处均可打断退出，不留孤儿重连循环。
    pub(crate) async fn run(
        self,
        mut outbound_rx: mpsc::Receiver<String>,
        reconnect: Arc<tokio::sync::Notify>,
        shutdown: CancellationToken,
    ) {
        let mut backoff = Duration::from_secs(1);
        loop {
            if shutdown.is_cancelled() {
                info!(target: TARGET, "停机信号已置位，client run 退出");
                return;
            }
            tokio::select! {
                res = self.connect_and_serve(&mut outbound_rx) => {
                    // connect_and_sserve 被 select drop（reconnect/shutdown 分支）
                    // 不会走到这里；正常返回即连接已断，在线标志统一在这里
                    // 熄灭（无论 Ok/Err 路径）。
                    self.online.store(false, Ordering::Relaxed);
                    match res {
                        Ok(()) => {
                            // 出站 channel 关闭（platform 退出）——正常收尾。
                            backoff = Duration::from_secs(1);
                        }
                        Err(e) => {
                            warn!(
                                target: TARGET,
                                error = %e,
                                backoff_ms = backoff.as_millis() as u64,
                                "relay 连接断开/出错，准备重连"
                            );
                        }
                    }
                }
                _ = reconnect.notified() => {
                    self.online.store(false, Ordering::Relaxed);
                    info!(target: TARGET, "收到 /reconnect 指令，主动断开重连");
                    backoff = Duration::from_secs(1);
                }
                _ = shutdown.cancelled() => {
                    self.online.store(false, Ordering::Relaxed);
                    info!(target: TARGET, "收到停机信号，client run 退出");
                    return;
                }
            }
            // 退避 sleep 同样可被停机信号打断——最坏 30s 退避不拖住停机。
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = shutdown.cancelled() => {
                    info!(target: TARGET, "停机信号打断退避等待，client run 退出");
                    return;
                }
            }
            backoff = (backoff * 2).min(BACKOFF_CAP);
        }
    }

    /// 一次完整的「建连 → 认证（Bearer + hello_ack）→ 收发」循环。返回 Err
    /// 触发外层重连。
    async fn connect_and_serve(&self, outbound_rx: &mut mpsc::Receiver<String>) -> Result<()> {
        let req = build_request(&self.relay_url, &self.gateway_token)?;
        info!(target: TARGET, url = %self.relay_url, "连接 relay 中");
        let (mut ws, _resp) = connect_async(req)
            .await
            .map_err(|e| CoreError::Platform(TARGET, format!("relay connect: {e}")))?;

        // Bearer 认证通过后 relay 立即推 hello_ack——等首帧确认认证结果，
        // 非 hello_ack（error/close）或超时都按认证失败重连（对齐 wecom R-5：
        // 认证失败不得空转发，否则进程存活但无入站，用户消息静默丢失）。
        match tokio::time::timeout(HELLO_ACK_TIMEOUT, ws.next()).await {
            Ok(Some(Ok(msg))) => {
                let text = msg
                    .into_text()
                    .map_err(|_| CoreError::Platform(TARGET, "hello_ack 帧非文本".into()))?;
                let kind = serde_json::from_str::<serde_json::Value>(text.as_str())
                    .ok()
                    .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
                    .unwrap_or_default();
                if kind != "hello_ack" {
                    return Err(CoreError::Platform(
                        TARGET,
                        format!("认证确认帧异常（type={kind}），按失败重连"),
                    ));
                }
            }
            Ok(Some(Err(e))) => {
                return Err(CoreError::Platform(
                    TARGET,
                    format!("等 hello_ack 读失败：{e}"),
                ));
            }
            Ok(None) => {
                return Err(CoreError::Platform(TARGET, "等 hello_ack 时流结束".into()));
            }
            Err(_) => {
                return Err(CoreError::Platform(
                    TARGET,
                    "等 hello_ack 超时（10s）——Bearer token 可能不匹配".into(),
                ));
            }
        }
        self.online.store(true, Ordering::Relaxed);
        info!(target: TARGET, "relay 已连接（hello_ack 确认），开始收发");

        // 收发循环。interval 首 tick 立即触发，先吃掉避免开局立刻发 ping。
        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
        heartbeat.tick().await;

        loop {
            tokio::select! {
                msg = ws.next() => {
                    let raw = match msg {
                        Some(Ok(Message::Text(t))) => t.to_string(),
                        // WS 协议层 Ping：显式回 Pong（tungstenite stream API
                        // 不自动回，对齐 wecom P3-N3）。
                        Some(Ok(Message::Ping(p))) => {
                            let _ = ws.send(Message::Pong(p)).await;
                            continue;
                        }
                        Some(Ok(Message::Pong(_))) => continue,
                        Some(Ok(Message::Close(_))) => {
                            return Err(CoreError::Platform(TARGET, "relay 关闭连接".into()));
                        }
                        Some(Ok(_)) => continue, // Binary 等忽略（协议无二进制载荷）
                        Some(Err(e)) => {
                            return Err(CoreError::Platform(TARGET, format!("ws read: {e}")));
                        }
                        None => {
                            return Err(CoreError::Platform(TARGET, "ws stream 结束".into()));
                        }
                    };
                    self.handle_text(&mut ws, &raw).await?;
                }

                // 出站帧：platform 序列化好的 JSON 文本，原样写回 relay。
                // 已知限制：帧已从 channel 取出，ws.send 失败（连接瞬断）即
                // 丢弃——MVP 无 outbox 持久重试（与 IM 平台不同），本帧不随
                // 重连重放；浏览器可发 sync 帧重拉卡片终态兜底。
                maybe_json = outbound_rx.recv() => {
                    let json = match maybe_json {
                        Some(j) => j,
                        None => {
                            // platform 端 sender 全部 drop（平台退出），正常收尾。
                            return Ok(());
                        }
                    };
                    ws.send(Message::text(json)).await.map_err(|e| {
                        CoreError::Platform(TARGET, format!("ws send: {e}"))
                    })?;
                    debug!(target: TARGET, "已发出站帧");
                }

                // 心跳：协议层 Ping（relay idle 90s，30s 保活）。
                _ = heartbeat.tick() => {
                    if let Err(e) = ws.send(Message::Ping(Vec::new().into())).await {
                        return Err(CoreError::Platform(TARGET, format!("ws send ping: {e}")));
                    }
                    debug!(target: TARGET, "已发心跳 ping");
                }
            }
        }
    }

    /// 单条入站文本帧分拣：relay 自产帧（machine/error/hello_ack）本层自理；
    /// 业务帧（message/approval_response/control/sync/ping）经
    /// [`map_inbound_frame`] 映射。畸形 JSON warn 丢弃（不断连——浏览器/中继
    /// 偶发杂帧不致网关重连）。
    async fn handle_text(&self, ws: &mut WsStream, raw: &str) -> Result<()> {
        let v: serde_json::Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                warn!(target: TARGET, error = %e, "入站帧解析失败，丢弃");
                return Ok(());
            }
        };
        let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match kind {
            "machine" => {
                if let Some(online) = v.get("online").and_then(|o| o.as_bool()) {
                    self.online.store(online, Ordering::Relaxed);
                    debug!(target: TARGET, online, "machine 帧更新在线标志");
                }
            }
            "error" => {
                warn!(target: TARGET, code = v.get("code").and_then(|c| c.as_str()).unwrap_or("?"), "relay error 帧");
            }
            "hello_ack" => {
                // 首帧 ack 已在 connect_and_serve 消费；重连顶替等场景的后续
                // ack 容错忽略。
                debug!(target: TARGET, "收到后续 hello_ack，忽略");
            }
            _ => match map_inbound_frame(&v, &self.default_session) {
                InboundAction::Message(msg) => {
                    // 入站 best-effort 推给 platform：1s 有界背压（消费端短暂
                    // 抖动不丢用户消息；持续满才丢弃，对齐 wecom P5-12）。
                    match tokio::time::timeout(Duration::from_secs(1), self.inbound_tx.send(*msg))
                        .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) => {
                            return Err(CoreError::Platform(
                                TARGET,
                                "入站 channel 已关闭（platform 退出）".into(),
                            ));
                        }
                        Err(_) => {
                            warn!(target: TARGET, "入站 channel 持续满 1s，丢弃浏览器消息");
                        }
                    }
                }
                InboundAction::Sync => {
                    if tokio::time::timeout(Duration::from_secs(1), self.sync_tx.send(()))
                        .await
                        .is_err()
                    {
                        warn!(target: TARGET, "sync channel 持续满 1s，丢弃 sync 请求");
                    }
                }
                InboundAction::Ping => {
                    // 应用层 ping：本层直接回 pong（§4.1，不进 recv）。
                    let _ = ws.send(Message::text(crate::encode_pong_frame())).await;
                }
                InboundAction::Ignore => {
                    debug!(target: TARGET, kind, "未知入站帧 type，忽略（前向兼容）");
                }
                InboundAction::Warn(reason) => {
                    warn!(target: TARGET, kind, reason, "入站帧违规，丢弃");
                }
            },
        }
        Ok(())
    }
}

/// 自建 WS Upgrade 请求（tungstenite 0.29 对自建 Request 不自动补握手头，
/// 须自带 Host/Upgrade/Key 等；Bearer 头走 relay 的直通认证形态 §4.2）。
fn build_request(relay_url: &str, gateway_token: &str) -> Result<Request<()>> {
    Request::builder()
        .uri(relay_url)
        .header("host", host_of(relay_url))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", generate_key())
        // relay §4.2 形态 2：`Authorization: Bearer <token>` 直通认证。
        .header("authorization", format!("Bearer {gateway_token}"))
        .body(())
        .map_err(|e| CoreError::Platform(TARGET, format!("构造 relay 请求失败：{e}")))
}

/// 从 URL 提取 host[:port]（Host 头用；不引入 url crate——web 依赖清单固定）。
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    rest.split('/').next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 常量一致性。
    #[test]
    fn constants_sane() {
        assert!(HEARTBEAT_INTERVAL.as_secs() > 0);
        assert!(BACKOFF_CAP >= Duration::from_secs(30));
        assert!(HELLO_ACK_TIMEOUT.as_secs() > 0);
    }

    /// host 提取：带路径 / 不带 / 无 scheme 均不 panic。
    #[test]
    fn host_of_variants() {
        assert_eq!(
            host_of("wss://ecs.example.com:8443/imagent/gateway"),
            "ecs.example.com:8443"
        );
        assert_eq!(host_of("ws://127.0.0.1:3010/gateway"), "127.0.0.1:3010");
        assert_eq!(host_of("wss://example.com"), "example.com");
        assert_eq!(host_of("not-a-url"), "not-a-url");
    }

    /// 请求构造：Bearer 头与目标 URI 正确进入 Request。
    #[test]
    fn build_request_carries_bearer_and_uri() {
        let req = build_request("wss://ecs.example.com/imagent/gateway", "tok-123").unwrap();
        assert_eq!(
            req.uri().to_string().as_str(),
            "wss://ecs.example.com/imagent/gateway"
        );
        assert_eq!(
            req.headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer tok-123"
        );
        assert_eq!(req.headers().get("upgrade").unwrap(), "websocket");
    }

    /// 已 cancel 的 token 让 run 立即返回（停机通路，对齐 wecom P3-g 测试）。
    #[tokio::test]
    async fn run_returns_promptly_on_cancelled_token() {
        let (inbound_tx, _inbound_rx) = mpsc::channel::<InboundMessage>(8);
        let (sync_tx, _sync_rx) = mpsc::channel::<()>(8);
        let client = WebClient {
            relay_url: "ws://127.0.0.1:1/gateway".into(),
            gateway_token: "t".into(),
            default_session: "default".into(),
            online: Arc::new(AtomicBool::new(false)),
            inbound_tx,
            sync_tx,
        };
        let (_outbound_tx, outbound_rx) = mpsc::channel::<String>(8);
        let token = CancellationToken::new();
        token.cancel();
        let res = tokio::time::timeout(
            Duration::from_secs(2),
            client.run(outbound_rx, Arc::new(tokio::sync::Notify::new()), token),
        )
        .await;
        assert!(res.is_ok(), "已取消的 token 应让 run 直接退出");
    }
}
