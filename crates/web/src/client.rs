//! 网关侧 WS 客户端任务：出站连 relay `/gateway`，帧收发 + 重连/心跳/停机。
//!
//! 职责链（参照 wecom client.rs 的成熟模式）：`connect_async` 建连 →
//! 认证（v1.3 按凭证分流：**key 形态**三帧——hello 自报 machine（不带
//! token）→ 等 relay 回 `challenge{nonce}` → 发 `prove{pubkey,signature}`
//!（对 nonce 原文签名）；**token 形态**（v1.2 兼容）一步——hello 带
//! token + machine。Bearer 头直通形态仍被 relay 接受但固定为 default
//! 机器，新网关版本不走）→ 等 relay 回的 `hello_ack`（回显 machine，
//! 校验与自报一致——不一致按认证失败重连）→ 收发帧循环；任意环节断开/
//! 出错即返回 `Err`，外层 [`WebClient::run`] 按指数退避重连（1s → 2s → …
//! 封顶 30s，成功建连重置）。每步握手等待 10s 超时（不空转发——对齐
//! wecom R-5「认证失败必须重连」纪律）。
//!
//! 与 [`crate::platform::WebPlatform`] 通过三条 channel 解耦：
//! - `inbound_tx`：浏览器业务帧映射成的 [`InboundMessage`] → platform.recv；
//! - `sync_tx`：sync 请求 → platform 的 sync responder（D6 会话恢复）；
//! - `outbound_rx`：platform 序列化好的出站帧（JSON 文本）→ relay → 浏览器。
//!
//! 协议细节（§4）：relay 对浏览器侧有 90s idle 超时，网关 30s 发协议层
//! `Message::Ping` 保活；浏览器应用层 `ping` JSON 帧由本层直接回 `pong`
//! （不进 recv）；`machine` / `machines` / `error` / `hello_ack` /
//! `challenge` 是 relay 自产帧，本层分拣自理（在线标志 / warn / debug），
//! 不进业务通道。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::Signer;
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

use crate::{map_inbound_frame, GatewayCredential, InboundAction, TARGET};

/// 建连后的 WS 流类型（connect_async 的产物）。
type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 认证握手每步等待上限（hello_ack / challenge 各 10s，v1.3 三帧握手逐
/// 步计时）：relay 立即回帧，超时按认证失败重连（不空转发——对齐 wecom
/// R-5「认证失败必须重连」纪律）。
const HANDSHAKE_STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// 心跳间隔：relay idle 超时 90s，30s 协议层 Ping 保活。
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// 重连退避上限。
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// 网关侧 WS 客户端（无内部可变状态；`run` 消费 self，由 platform spawn）。
pub(crate) struct WebClient {
    /// relay 网关端点完整地址：`wss://…/imagent/gateway`（公网，Caddy strip
    /// 前缀）或 `ws://127.0.0.1:<port>/gateway`（本地/测试）。
    pub(crate) relay_url: String,
    /// 网关认证凭证（v1.3）：Key → 三帧挑战应答；Token → 一步 hello。
    pub(crate) credential: GatewayCredential,
    /// 本机 machine 身份（v1.1 多终端管理）：hello 自报 + 出站帧携带。
    pub(crate) machine_id: String,
    /// 无 convId 的浏览器消息落到的默认会话名（映射 message 帧用）。
    pub(crate) default_session: String,
    /// 在线标志（client 维护：连接成功 true，断开 false）——doctor_probes
    /// 与日志用。relay 的 machine/machines 帧只推浏览器，网关侧标志由连接
    /// 生命周期自维护；分拣到 machine 时也顺带同步（防御性，正常收不到）。
    pub(crate) online: Arc<AtomicBool>,
    pub(crate) inbound_tx: mpsc::Sender<InboundMessage>,
    pub(crate) sync_tx: mpsc::Sender<()>,
}

/// hello 首帧构造（token 形态，v1.1/v1.2 兼容路径：token + machine 自报；
/// 纯函数便于单测）。
fn hello_frame(token: &str, machine: &str) -> String {
    serde_json::json!({"v": 1, "type": "hello", "token": token, "machine": machine}).to_string()
}

/// hello 首帧构造（key 形态，v1.3 挑战应答入口：只带 machine，不带任何
/// 静态凭证——relay 据此走 challenge 分流）。
fn hello_key_frame(machine: &str) -> String {
    serde_json::json!({"v": 1, "type": "hello", "machine": machine}).to_string()
}

/// prove 帧构造（v1.3 第三帧）：公钥 hex + 对 **nonce hex 字符串原文**
/// 的 ed25519 签名（网关对收到的 nonce 原样签名——relay 侧对同一原文
/// 验签，无 hex 解码歧义）。
fn prove_frame(sk: &ed25519_dalek::SigningKey, nonce: &str) -> String {
    serde_json::json!({
        "v": 1,
        "type": "prove",
        "pubkey": hex::encode(sk.verifying_key().to_bytes()),
        "signature": hex::encode(sk.sign(nonce.as_bytes()).to_bytes()),
    })
    .to_string()
}

/// challenge 帧解析（v1.3 第二帧）：返回 nonce（hex 字符串原文）。非
/// challenge / 缺 nonce / 空串 / 畸形 JSON → Err（调用方按认证失败重连；
/// 错误文案区分「relay 未配本机公钥（未回 challenge 即关闭）」的排查方向）。
fn parse_challenge(text: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| CoreError::Platform(TARGET, format!("challenge 解析失败：{e}")))?;
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if kind != "challenge" {
        return Err(CoreError::Platform(
            TARGET,
            format!(
                "认证握手帧异常（type={kind}，期待 challenge）——relay 可能未配本机公钥\
                 （IMAGENT_RELAY_GATEWAY_KEYS）或为不支持 key 认证的旧版本，按失败重连"
            ),
        ));
    }
    v.get("nonce")
        .and_then(|n| n.as_str())
        .map(str::to_string)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| CoreError::Platform(TARGET, "challenge 帧缺 nonce 字段".into()))
}

/// hello_ack 解析（v1.1）：返回回帧的 machine 字段（relay 回显自报身份）。
/// 非 hello_ack / 缺 machine（relay < v1.1）/ 畸形 → Err（调用方按认证失败
/// 重连，不空转发）。
fn parse_hello_ack(text: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| CoreError::Platform(TARGET, format!("hello_ack 解析失败：{e}")))?;
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if kind != "hello_ack" {
        return Err(CoreError::Platform(
            TARGET,
            format!("认证确认帧异常（type={kind}），按失败重连"),
        ));
    }
    v.get("machine")
        .and_then(|m| m.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            CoreError::Platform(TARGET, "hello_ack 缺 machine 字段（relay < v1.1？）".into())
        })
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

    /// 一次完整的「建连 → 认证（v1.3：key 三帧 / token 一步 + hello_ack）
    /// → 收发」循环。返回 Err 触发外层重连。
    async fn connect_and_serve(&self, outbound_rx: &mut mpsc::Receiver<String>) -> Result<()> {
        let req = build_request(&self.relay_url)?;
        info!(target: TARGET, url = %self.relay_url, machine = %self.machine_id, "连接 relay 中");
        let (mut ws, _resp) = connect_async(req)
            .await
            .map_err(|e| CoreError::Platform(TARGET, format!("relay connect: {e}")))?;

        // v1.3 认证按凭证分流（key 优先，token 兼容——装配决策见
        // `crate::key::load_gateway_credential`）：
        // - Key：hello（只带 machine）→ challenge{nonce} → prove{pubkey,
        //   signature}（对 nonce 原文签名）；
        // - Token：hello（token + machine）一步校验。
        match &self.credential {
            GatewayCredential::Token(tok) => {
                ws.send(Message::text(hello_frame(tok, &self.machine_id)))
                    .await
                    .map_err(|e| CoreError::Platform(TARGET, format!("hello 发送失败：{e}")))?;
            }
            GatewayCredential::Key(sk) => {
                ws.send(Message::text(hello_key_frame(&self.machine_id)))
                    .await
                    .map_err(|e| CoreError::Platform(TARGET, format!("hello 发送失败：{e}")))?;
                // 等 challenge（relay 对无凭证 hello 下发一次性 nonce）。
                let nonce = match tokio::time::timeout(HANDSHAKE_STEP_TIMEOUT, ws.next()).await {
                    Ok(Some(Ok(msg))) => parse_challenge(
                        msg.into_text()
                            .map_err(|_| CoreError::Platform(TARGET, "challenge 帧非文本".into()))?
                            .as_str(),
                    )?,
                    Ok(Some(Err(e))) => {
                        return Err(CoreError::Platform(
                            TARGET,
                            format!("等 challenge 读失败：{e}"),
                        ));
                    }
                    Ok(None) => {
                        return Err(CoreError::Platform(TARGET, "等 challenge 时流结束".into()));
                    }
                    Err(_) => {
                        return Err(CoreError::Platform(
                            TARGET,
                            "等 challenge 超时（10s）——relay 可能未配本机公钥（IMAGENT_RELAY_GATEWAY_KEYS）".into(),
                        ));
                    }
                };
                ws.send(Message::text(prove_frame(sk, &nonce)))
                    .await
                    .map_err(|e| CoreError::Platform(TARGET, format!("prove 发送失败：{e}")))?;
            }
        }
        // 共同收口：等 hello_ack（回显 machine）并校验与自报一致——不一致
        // 视为 relay 侧身份错乱（如 machine 槽位被同名顶替/漂移），warn 并按
        // 认证失败重连，绝不空转发（对齐 wecom R-5：认证失败不得空转发，
        // 否则进程存活但无入站，用户消息静默丢失）。
        match tokio::time::timeout(HANDSHAKE_STEP_TIMEOUT, ws.next()).await {
            Ok(Some(Ok(msg))) => {
                let text = msg
                    .into_text()
                    .map_err(|_| CoreError::Platform(TARGET, "hello_ack 帧非文本".into()))?;
                let ack_machine = parse_hello_ack(text.as_str())?;
                if ack_machine != self.machine_id {
                    warn!(
                        target: TARGET,
                        ack_machine = %ack_machine,
                        self_machine = %self.machine_id,
                        "hello_ack 回显 machine 与自报不一致，按认证失败重连"
                    );
                    return Err(CoreError::Platform(
                        TARGET,
                        format!(
                            "hello_ack machine 不一致（ack={ack_machine}，自报={}），重连",
                            self.machine_id
                        ),
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
                    "等 hello_ack 超时（10s）——gateway 认证可能未通过".into(),
                ));
            }
        }
        self.online.store(true, Ordering::Relaxed);
        info!(target: TARGET, machine = %self.machine_id, "relay 已连接（hello_ack 确认身份），开始收发");

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

    /// 单条入站文本帧分拣：relay 自产帧（machine/machines/error/hello_ack）
    /// 本层自理（machines 只推浏览器，网关侧正常收不到，落到 Ignore 容错）；
    /// 业务帧（message/approval_response/control/sync/ping，均可带 relay
    /// 路由用的 machine 字段——网关侧无需消费）经 [`map_inbound_frame`]
    /// 映射。畸形 JSON warn 丢弃（不断连——浏览器/中继偶发杂帧不致网关
    /// 重连）。
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
                    // 应用层 ping：本层直接回 pong（§4.1，不进 recv），带本机
                    // machine（v1.1 全部出站帧约定）。
                    let _ = ws
                        .send(Message::text(crate::encode_pong_frame(&self.machine_id)))
                        .await;
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
/// 须自带 Host/Upgrade/Key 等）。认证走 hello 首帧（v1.3 key 三帧 / v1.2
/// token 一步），不再附 Bearer 头——Bearer 直通在 relay 侧固定为 default
/// 机器，带上会让 machine 身份失效。
fn build_request(relay_url: &str) -> Result<Request<()>> {
    Request::builder()
        .uri(relay_url)
        .header("host", host_of(relay_url))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", generate_key())
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
        assert!(HANDSHAKE_STEP_TIMEOUT.as_secs() > 0);
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

    /// 请求构造：URI 与握手头正确进入 Request，且**不带** Authorization 头
    ///（v1.1 认证统一走 hello 首帧——Bearer 直通固定 default 机器，不能带）。
    #[test]
    fn build_request_no_bearer_carries_uri() {
        let req = build_request("wss://ecs.example.com/imagent/gateway").unwrap();
        assert_eq!(
            req.uri().to_string().as_str(),
            "wss://ecs.example.com/imagent/gateway"
        );
        assert!(
            req.headers().get("authorization").is_none(),
            "v1.1 起认证走 hello 帧，不得附 Bearer 头"
        );
        assert_eq!(req.headers().get("upgrade").unwrap(), "websocket");
    }

    /// hello 帧形状：token + machine 自报（v1.1）。
    #[test]
    fn hello_frame_carries_token_and_machine() {
        let f = hello_frame("tok-123", "home-mac");
        let v: serde_json::Value = serde_json::from_str(&f).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "hello");
        assert_eq!(v["token"], "tok-123");
        assert_eq!(v["machine"], "home-mac");
    }

    /// v1.3 key 形态 hello：只带 machine，**不带任何静态凭证**（relay 据
    /// 此走 challenge 分流）。
    #[test]
    fn hello_key_frame_carries_no_credential() {
        let f = hello_key_frame("home-mac");
        let v: serde_json::Value = serde_json::from_str(&f).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "hello");
        assert_eq!(v["machine"], "home-mac");
        assert!(v.get("token").is_none(), "key 握手 hello 不带 token");
        assert!(v.get("session").is_none(), "网关无会话形态");
    }

    /// v1.3 prove 帧：pubkey 为本机公钥 hex，signature 为对 nonce **原文**
    /// 的 ed25519 签名（用公钥真实验签钉住签名对象约定——两侧无 hex 解码
    /// 歧义）。
    #[test]
    fn prove_frame_signature_covers_nonce_literal() {
        use ed25519_dalek::Verifier;
        let sk = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let nonce = "deadbeef".repeat(8);
        let f = prove_frame(&sk, &nonce);
        let v: serde_json::Value = serde_json::from_str(&f).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "prove");
        assert_eq!(
            v["pubkey"].as_str().unwrap(),
            hex::encode(sk.verifying_key().to_bytes())
        );
        let sig =
            <[u8; 64]>::try_from(hex::decode(v["signature"].as_str().unwrap()).unwrap()).unwrap();
        sk.verifying_key()
            .verify(
                nonce.as_bytes(),
                &ed25519_dalek::Signature::from_bytes(&sig),
            )
            .expect("签名须对 nonce 原文（hex 字符串字节）可验");
    }

    /// challenge 解析：正常 / 非 challenge（含排查指引文案）/ 缺 nonce /
    /// 空 nonce / 畸形 JSON。
    #[test]
    fn parse_challenge_variants() {
        assert_eq!(
            parse_challenge(r#"{"v":1,"type":"challenge","nonce":"abcd"}"#).unwrap(),
            "abcd"
        );
        assert!(
            parse_challenge(r#"{"v":1,"type":"hello_ack","machine":"m"}"#)
                .unwrap_err()
                .to_string()
                .contains("期待 challenge"),
            "非 challenge 报错（附排查指引）"
        );
        assert!(
            parse_challenge(r#"{"v":1,"type":"challenge"}"#)
                .unwrap_err()
                .to_string()
                .contains("nonce"),
            "缺 nonce 报错"
        );
        assert!(
            parse_challenge(r#"{"v":1,"type":"challenge","nonce":""}"#).is_err(),
            "空 nonce 报错"
        );
        assert!(parse_challenge("not json").is_err(), "畸形 JSON 报错");
    }

    /// hello_ack 解析：正常回显 machine / 非 hello_ack / 缺 machine（旧
    /// relay）/ 畸形 JSON 四分支。
    #[test]
    fn parse_hello_ack_variants() {
        assert_eq!(
            parse_hello_ack(r#"{"v":1,"type":"hello_ack","role":"gateway","machine":"home-mac"}"#)
                .unwrap(),
            "home-mac"
        );
        assert!(
            parse_hello_ack(r#"{"v":1,"type":"error","code":"boom"}"#)
                .unwrap_err()
                .to_string()
                .contains("认证确认帧异常"),
            "非 hello_ack 报错"
        );
        assert!(
            parse_hello_ack(r#"{"v":1,"type":"hello_ack","role":"gateway"}"#)
                .unwrap_err()
                .to_string()
                .contains("machine"),
            "缺 machine（relay < v1.1）报错"
        );
        assert!(parse_hello_ack("not json").is_err(), "畸形 JSON 报错");
    }

    /// 已 cancel 的 token 让 run 立即返回（停机通路，对齐 wecom P3-g 测试）。
    #[tokio::test]
    async fn run_returns_promptly_on_cancelled_token() {
        let (inbound_tx, _inbound_rx) = mpsc::channel::<InboundMessage>(8);
        let (sync_tx, _sync_rx) = mpsc::channel::<()>(8);
        let client = WebClient {
            relay_url: "ws://127.0.0.1:1/gateway".into(),
            credential: GatewayCredential::Token("t".into()),
            machine_id: "test-mac".into(),
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
