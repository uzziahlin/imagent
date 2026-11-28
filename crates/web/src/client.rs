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
//! 与 [`crate::platform::WebPlatform`] 通过六条 channel 解耦：
//! - `inbound_tx`：浏览器业务帧映射成的 [`InboundMessage`] → platform.recv；
//! - `sync_tx`：sync 通道请求（[`SyncRequest`]：`sync` 请求 → platform 的
//!   sync responder 重发卡片缓存，D6 会话恢复；v1.42 `approvals_query` →
//!   同一 responder 回待审批快照；v1.43 `windows_query` → 同一 responder 回
//!   本地窗口快照）；
//! - `workspace_tx`：v1.6 工作空间动作（workspace_list/save/remove /
//!   conv_workdir_set）→ platform 的 workspace 泵（store KV 读写）；
//! - `media_tx`：v1.7 媒体上行（message 帧带 media 段）→ platform 的媒体泵
//!   （值域校验 + 落盘 + 合成 InboundMessage / 失败回 error 帧）；
//! - `history_tx`：v1.8 会话历史重放（history_query 帧）→ platform 的
//!   history 泵（查 outbound_log + 回 history 帧）；
//! - `asr_tx`：v1.40 语音转写（asr_request 帧）→ platform 的 ASR 泵
//!   （b64 解码 + 写临时 WAV + spawn whisper-cli + 回 asr_result 帧）；
//! - `outbound_rx`：platform 序列化好的出站帧（JSON 文本）→ relay → 浏览器。
//! - `outbound_tx`（v1.44）：同一条出站通道的**发送端**——client 自发的帧
//!   （`ack{received}` / `ack{rejected,malformed}` / `gateway_info`）经它入队，
//!   与 platform 的帧走同一条 `send_business` 路径（E2E 活跃时逐把密钥加密）。
//!   best-effort：`try_send`，队列满只 debug。
//!
//! client 自发起的 v1.44 帧（详见各分支注释）：
//! - `received` 回执：带合规 `clientId` 的 `message` 帧**进入平台入站 channel
//!   之后**才发（纯文本在本层 `deliver_inbound`，带图在 platform 媒体泵）——
//!   它证明的是「到达了网关」，发早了会把一条实际被丢弃的消息报成已送达；
//! - `malformed` 回执：`map_inbound_frame` 判 `Warn` 且帧里能读出合规
//!   `clientId` 时回（消息格式无效）；
//! - `gateway_info`：`gateway_info_query` 的应答（无状态，直接在本层出站）。
//! - `e2e_close`（入站，封在 enc 里）：解开后删除解开它的那把密钥，不回帧。
//!
//! 协议细节（§4）：relay 对浏览器侧有 90s idle 超时，网关 30s 发协议层
//! `Message::Ping` 保活；浏览器应用层 `ping` JSON 帧由本层直接回 `pong`
//! （不进 recv）；`machine` / `machines` / `error` / `hello_ack` /
//! `challenge` 是 relay 自产帧，本层分拣自理（在线标志 / warn / debug），
//! 不进业务通道。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

use imagent_core::peer::PeerState;
use imagent_core::{CoreError, InboundMessage, Result};

use crate::e2e::{E2eIdentity, E2eSessions};
use crate::platform::SyncRequest;
use crate::{
    map_inbound_frame, normalize_conv, parse_client_id, AckStatus, GatewayCredential,
    InboundAction, PushEvent, TARGET,
};

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
    /// E2E 身份（协议 v1.4）：静态 X25519 + v1.3 ed25519 签名密钥。None =
    /// E2E 不可用（token 凭证无签名密钥 / 密钥文件损坏）——e2e_hello 忽略、
    /// 出站恒明文（v1.3 行为，兼容旧链路）。
    pub(crate) e2e: Option<E2eIdentity>,
    pub(crate) inbound_tx: mpsc::Sender<InboundMessage>,
    /// sync 通道（v1.42 起载荷为 [`SyncRequest`]：`sync` / `approvals_query`
    /// 两帧共用）→ platform 的 sync responder。
    pub(crate) sync_tx: mpsc::Sender<SyncRequest>,
    /// v1.6 工作空间动作通道（workspace_list/save/remove/conv_workdir_set
    /// 四帧）→ platform 的 workspace 泵（store KV 读写 + 校验 + 回快照）。
    pub(crate) workspace_tx: mpsc::Sender<InboundAction>,
    /// v1.7 媒体上行通道（message 帧带 media 段）→ platform 的媒体泵
    /// （mime/b64/大小校验 + 落盘 + 合成 InboundMessage；失败回 error 帧）。
    pub(crate) media_tx: mpsc::Sender<InboundAction>,
    /// v1.8 会话历史重放通道（history_query 帧）→ platform 的 history 泵
    /// （查 outbound_log + 回 history 帧）。
    pub(crate) history_tx: mpsc::Sender<InboundAction>,
    /// v1.40 语音转写通道（asr_request 帧）→ platform 的 ASR 泵（b64 解码
    /// + 写临时 WAV + spawn whisper-cli；失败回 error 帧）。
    pub(crate) asr_tx: mpsc::Sender<InboundAction>,
    /// v1.43 本地窗口看板的 peer 共享态槽位（与 platform 同一个 `Arc`——
    /// `PeerState` 在 `PeerServer::start` 里才创建，晚于 client 构造，由 main
    /// 经 `WebPlatform::set_peer_state` 后填；`None` = peer 通道未启用，
    /// `windows_query` / `window_watch` 一律忽略）。
    pub(crate) peer_state: Arc<std::sync::Mutex<Option<Arc<PeerState>>>>,
    /// v1.44 出站通道的发送端（与 `outbound_rx` 同一条 channel；platform 与
    /// 各泵各持一份）。client 自发的 `ack` / `gateway_info` 帧经它入队，随
    /// platform 的帧一起走 `send_business`（E2E 活跃时逐把密钥加密）。
    /// 副作用：本任务因此永远持有一个 sender，`outbound_rx.recv()` 不会再
    /// 返回 None——停机完全由 `shutdown` token 承担（平台 Drop 即 cancel，
    /// 见 [`WebClient::run`]），与此前行为一致。
    pub(crate) outbound_tx: mpsc::Sender<String>,
}

impl WebClient {
    /// peer 共享态的当前值（None = peer 通道未启用）。临界区只做一次 clone。
    fn peer_state(&self) -> Option<Arc<PeerState>> {
        crate::platform::lock_or_recover(&self.peer_state).clone()
    }
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
        mut push_rx: mpsc::Receiver<PushEvent>,
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
                res = self.connect_and_serve(&mut outbound_rx, &mut push_rx) => {
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
    async fn connect_and_serve(
        &self,
        outbound_rx: &mut mpsc::Receiver<String>,
        push_rx: &mut mpsc::Receiver<PushEvent>,
    ) -> Result<()> {
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

        // E2E 会话表（协议 v1.4）：per-relay-连接重建——网关重连即清空全部
        // 会话密钥（「断连清 session 加密器」的网关侧实现）。v1.44 起表是
        // 「每个登录会话多把密钥」：同 sid 的多个标签页各自握手、互不顶替；
        // 浏览器断连对网关不可见，密钥靠 e2e_close / 空闲回收 / 容量淘汰
        // 退场（见 e2e 模块头「多标签页」）。
        let mut e2e_sessions = E2eSessions::new();

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
                    self.handle_text(&mut ws, &raw, &mut e2e_sessions).await?;
                }

                // 出站帧：platform 序列化好的 JSON 文本，经 E2E 会话逐 sid
                // 加密后写回 relay（无任何会话时明文直发——兼容旧浏览器，
                // 见 e2e 模块头注释的渐进语义）。
                // 已知限制：帧已从 channel 取出，ws.send 失败（连接瞬断）即
                // 丢弃——MVP 无 outbox 持久重试（与 IM 平台不同），本帧不随
                // 重连重放；浏览器可发 sync 帧重拉卡片终态兜底。
                maybe_json = outbound_rx.recv() => {
                    let json = match maybe_json {
                        Some(j) => j,
                        None => {
                            // 类型完备分支，实际不可达：v1.44 起本任务自己持有
                            // 一个 outbound sender（见 WebClient::outbound_tx），
                            // channel 不会关闭。停机由 shutdown token 承担。
                            return Ok(());
                        }
                    };
                    self.send_business(&mut ws, json, &mut e2e_sessions).await?;
                }

                // v1.10 push_notify：平台埋点事件 → 明文管理帧直发。**不经
                // send_business 的 E2E seal**——该帧族与 device_list 等管理帧
                // 同路（中继需明文读取 kind/convId/machine 做订阅投递与在线
                // 免打扰判定；title 是契约约定的 ≤32 码点摘要，同样明文——
                // 见 lib.rs encode_push_notify_frame 文档）。v1.42 新增
                // kind=peer_reply（窗口回信推送）。
                maybe_event = push_rx.recv() => {
                    let ev = match maybe_event {
                        Some(ev) => ev,
                        // 平台退出（push 通道与 outbound 通道同生命周期
                        // ——都在 WebPlatform 上）→ 与 outbound None 同语义收尾。
                        None => return Ok(()),
                    };
                    let frame = crate::encode_push_notify_frame(&self.machine_id, &ev);
                    ws.send(Message::text(frame))
                        .await
                        .map_err(|e| CoreError::Platform(TARGET, format!("ws send push_notify: {e}")))?;
                    debug!(target: TARGET, kind = ev.kind.as_str(), conv = %ev.conv_id, "已发 push_notify 帧");
                }

                // 心跳：协议层 Ping（relay idle 90s，30s 保活）+ 顺带执行 E2E
                // 密钥清理（半开 60s TTL + v1.44 已确认密钥空闲 600s 回收，
                // 无需独立定时器）。
                _ = heartbeat.tick() => {
                    e2e_sessions.prune(Instant::now());
                    if let Err(e) = ws.send(Message::Ping(Vec::new().into())).await {
                        return Err(CoreError::Platform(TARGET, format!("ws send ping: {e}")));
                    }
                    debug!(target: TARGET, "已发心跳 ping");
                }
            }
        }
    }

    /// 出站业务帧发送（v1.4 E2E 包装点）：表里有密钥 → **每把密钥**各密封
    /// 一份 enc{sid}（同会话的每个标签页一份；sync 重发帧、client 自发的
    /// ack / gateway_info 同样流经此处，天然逐密钥加密）；无密钥 → 明文直发
    /// （行为与 v1.3 完全一致）。曾加密过的连接回退明文时 warn 一次（降级
    /// 观测点：正常只有「全部浏览器断开且密钥被回收」才触发，恶意中继丢弃
    /// 浏览器 enc 帧也会走到这里）。
    async fn send_business(
        &self,
        ws: &mut WsStream,
        plaintext: String,
        sessions: &mut E2eSessions,
    ) -> Result<()> {
        let frames = sessions.seal_each(&self.machine_id, &plaintext);
        if sessions.is_empty() {
            if sessions.take_plaintext_revert() {
                warn!(
                    target: TARGET,
                    "E2E 会话已全部失效，业务帧回退明文中继（若非预期：可能存在\
                     中继丢帧干扰，或浏览器握手未完成/未续期）"
                );
            }
            ws.send(Message::text(plaintext))
                .await
                .map_err(|e| CoreError::Platform(TARGET, format!("ws send: {e}")))?;
            debug!(target: TARGET, "已发出站帧（明文，无 E2E 会话）");
        } else {
            for f in frames {
                ws.send(Message::text(f))
                    .await
                    .map_err(|e| CoreError::Platform(TARGET, format!("ws send: {e}")))?;
            }
            debug!(target: TARGET, keys = sessions.len(), "已发出站 enc 帧（逐密钥各一份）");
        }
        Ok(())
    }

    /// 单条入站文本帧分拣：relay 自产帧（machine/machines/error/hello_ack）
    /// 本层自理（machines 只推浏览器，网关侧正常收不到，落到 Ignore 容错）；
    /// E2E 帧（v1.4）：`e2e_hello` → 握手应答（e2e_ack），`enc` → 试解后把
    /// 内层业务帧走既有分发（v1.44：内层是 `e2e_close` 则摘掉解开它的那把
    /// 密钥、不进分发）；业务帧（message/approval_response/control/
    /// sync/ping + v1.6 工作空间 workspace_list/workspace_save/
    /// workspace_remove/conv_workdir_set + v1.8 history_query + v1.42
    /// approvals_query + v1.43 windows_query/window_watch + v1.44
    /// gateway_info_query，均可带 relay 路由用的
    /// machine 字段——网关侧无需消费）经 [`map_inbound_frame`] 映射。
    /// 畸形 JSON warn 丢弃（不断连——浏览器/中继偶发杂帧不致网关重连）。
    async fn handle_text(
        &self,
        ws: &mut WsStream,
        raw: &str,
        e2e_sessions: &mut E2eSessions,
    ) -> Result<()> {
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
            // E2E 握手（v1.4）：建会话并回 e2e_ack（gk/ik/n2/sig）。E2E 身份
            // 不可用（token 凭证 / 密钥文件损坏）→ 忽略不回——浏览器 10s
            // 超时走 SPA 侧 fail-closed 明文选择。
            "e2e_hello" => match &self.e2e {
                Some(identity) => {
                    if let Some(ack) = identity.handle_hello(e2e_sessions, &v, &self.machine_id) {
                        let _ = ws.send(Message::text(ack)).await;
                    }
                }
                None => {
                    warn!(
                        target: TARGET,
                        "e2e_hello 到达但 E2E 身份不可用（token 凭证无 ed25519 签名密钥/密钥文件损坏），忽略"
                    );
                }
            },
            // E2E 密文帧（v1.4）：浏览器→网关方向无 sid，逐把密钥试解
            //（≤ [`E2eSessions::MAX_KEYS_TOTAL`] 次，见 E2eSessions::open_any）
            // 后走既有业务分发。
            "enc" => {
                let Some(boxed) = v.get("box").and_then(|b| b.as_str()) else {
                    warn!(target: TARGET, "enc 帧缺 box 字段，丢弃");
                    return Ok(());
                };
                match e2e_sessions.open_any(boxed, Instant::now()) {
                    Some(opened) => {
                        let inner_v: serde_json::Value = match serde_json::from_str(
                            &opened.plaintext,
                        ) {
                            Ok(v) => v,
                            Err(e) => {
                                warn!(target: TARGET, error = %e, "enc 帧解密后内层 JSON 畸形，丢弃");
                                return Ok(());
                            }
                        };
                        // v1.44 e2e_close：标签页关闭时发的「注销本密钥」——删除
                        // **解开这帧的那把**密钥（同会话其它标签页不受影响），
                        // 不回任何帧、不进业务分发。崩溃 / 被 kill 的标签页发
                        // 不出，靠 prune 的空闲回收兜底。旧网关解开后按未知
                        // type 忽略，无副作用。
                        if inner_v.get("type").and_then(|t| t.as_str()) == Some("e2e_close") {
                            e2e_sessions.remove(&opened.sid, &opened.key);
                            debug!(
                                target: TARGET,
                                sid = %crate::e2e::short_sid(&opened.sid),
                                remaining_keys = e2e_sessions.len(),
                                "e2e_close：已移除该标签页的会话密钥，不回帧"
                            );
                            return Ok(());
                        }
                        self.dispatch_business(ws, &inner_v, e2e_sessions).await?
                    }
                    None => {
                        warn!(
                            target: TARGET,
                            "enc 帧无法用任何活跃密钥解密（无握手/密钥已 e2e_close 或回收），丢弃"
                        );
                    }
                }
            }
            _ => self.dispatch_business(ws, &v, e2e_sessions).await?,
        }
        Ok(())
    }

    /// 业务帧分发（明文直收与 enc 解密后共用）。ping 是唯一要写 ws 的动作
    /// （pong 经 [`Self::send_business`]——E2E 密钥活跃时同为业务帧，逐把密钥
    /// 加密）；其余动作都不碰 ws，落在可单测的 [`Self::dispatch_action`]。
    async fn dispatch_business(
        &self,
        ws: &mut WsStream,
        v: &serde_json::Value,
        e2e_sessions: &mut E2eSessions,
    ) -> Result<()> {
        let act = map_inbound_frame(v, &self.default_session);
        if matches!(act, InboundAction::Ping) {
            // 应用层 ping：本层直接回 pong（§4.1，不进 recv），带本机
            // machine（v1.1 全部出站帧约定）。
            let pong = crate::encode_pong_frame(&self.machine_id);
            return self.send_business(ws, pong, e2e_sessions).await;
        }
        self.dispatch_action(v, act).await
    }

    /// 不碰 ws 的业务动作分发（client 单测直接驱动，不必连 relay）。`v` 是
    /// 原始帧——`Warn` 分支要回头读 `clientId` / `convId` 组 malformed 回执
    /// （[`InboundAction::Warn`] 只带原因文本）。
    async fn dispatch_action(&self, v: &serde_json::Value, act: InboundAction) -> Result<()> {
        match act {
            InboundAction::Message(msg) => self.deliver_inbound(msg).await?,
            // v1.7 媒体上行 → platform 媒体泵（b64 解码 + 落盘是文件系统
            // IO，不占 client 收发循环；失败 error 帧经泵的 outbound 通道回）。
            // 同 1s 有界背压：泵毫秒级吞吐，持续满只在异常时出现——丢弃由
            // SPA 无回执感知 + 用户重发兜底。v1.44：`received` 回执由泵在
            // 合成消息**进入入站 channel 之后**发（clientId 随动作带过去），
            // 本层只经 media channel，不算「已接下」。
            act @ InboundAction::MediaMessage { .. } => {
                if tokio::time::timeout(Duration::from_secs(1), self.media_tx.send(act))
                    .await
                    .is_err()
                {
                    warn!(target: TARGET, "media channel 持续满 1s，丢弃媒体消息");
                }
            }
            InboundAction::Sync => {
                if tokio::time::timeout(
                    Duration::from_secs(1),
                    self.sync_tx.send(SyncRequest::Sync),
                )
                .await
                .is_err()
                {
                    warn!(target: TARGET, "sync channel 持续满 1s，丢弃 sync 请求");
                }
            }
            // v1.42 待审批快照 → 同一 sync responder（按 pending 登记表回
            // approvals 帧）。同 1s 有界背压；丢弃由 SPA 重发 approvals_query
            // 兜底（重连 / 切会话时都会发）。
            InboundAction::ApprovalsQuery => {
                if tokio::time::timeout(
                    Duration::from_secs(1),
                    self.sync_tx.send(SyncRequest::ApprovalsQuery),
                )
                .await
                .is_err()
                {
                    warn!(target: TARGET, "sync channel 持续满 1s，丢弃 approvals_query 请求");
                }
            }
            // v1.43 本地窗口快照 → 同一 sync responder（按 peer 共享态回 windows
            // 帧）。peer 通道未启用时**不进泵、不回帧**——契约：前端收不到
            // windows 帧即不显示看板。同 1s 有界背压；丢弃由 SPA 重发
            // windows_query 兜底（重连 / 切会话时都会发）。
            InboundAction::WindowsQuery => {
                if self.peer_state().is_none() {
                    debug!(target: TARGET, "windows_query：peer 通道未启用，不回帧");
                    return Ok(());
                }
                if tokio::time::timeout(
                    Duration::from_secs(1),
                    self.sync_tx.send(SyncRequest::Windows),
                )
                .await
                .is_err()
                {
                    warn!(target: TARGET, "sync channel 持续满 1s，丢弃 windows_query 请求");
                }
            }
            // v1.43 关注 / 取消关注窗口：client 层直接改共享态（不经泵），再让
            // sync 泵重发一帧 windows——所有在线设备的 `watched` 以网关为准
            // 收敛。peer 通道未启用时静默忽略（看板本就不显示，无处可点）。
            InboundAction::WindowWatch { name, watch } => {
                if let Some(state) = self.peer_state() {
                    state.set_watched(&name, watch);
                    if tokio::time::timeout(
                        Duration::from_secs(1),
                        self.sync_tx.send(SyncRequest::Windows),
                    )
                    .await
                    .is_err()
                    {
                        warn!(target: TARGET, "sync channel 持续满 1s，window_watch 后的快照重发被丢弃（下条快照兜底）");
                    }
                }
            }
            // v1.6 工作空间动作 → platform 泵（store IO 在泵内 await，client
            // 收发循环不被 sqlite 阻塞）。同 sync 的 1s 有界背压：泵消费极快
            //（KV 读写毫秒级），持续满只会在 relay 断连排队时出现——此时帧
            // 本就发不出去，丢弃后 SPA 重发/下条快照兜底。v1.41.3 conv_delete
            // 同泵同通道（活跃轮探测 + 三处删除 + 快照回，见 platform 泵注释）。
            act @ (InboundAction::WorkspaceList
            | InboundAction::WorkspaceSave { .. }
            | InboundAction::WorkspaceRemove { .. }
            | InboundAction::ConvWorkdirSet { .. }
            | InboundAction::ConvDelete { .. }) => {
                if tokio::time::timeout(Duration::from_secs(1), self.workspace_tx.send(act))
                    .await
                    .is_err()
                {
                    warn!(target: TARGET, "workspace channel 持续满 1s，丢弃工作空间请求");
                }
            }
            // v1.8 会话历史重放 → platform history 泵（store IO 在泵内 await，
            // 同 workspace 泵纪律）。1s 有界背压：丢弃由 SPA 下次切会话/重连
            // 的 history_query 兜底。
            act @ InboundAction::HistoryQuery { .. } => {
                if tokio::time::timeout(Duration::from_secs(1), self.history_tx.send(act))
                    .await
                    .is_err()
                {
                    warn!(target: TARGET, "history channel 持续满 1s，丢弃 history_query 请求");
                }
            }
            // v1.40 语音转写 → platform ASR 泵（b64 解码 + spawn whisper-cli
            // 是秒级 IO，不占 client 收发循环；失败 error 帧经泵的 outbound
            // 通道回）。同 1s 有界背压：转写泵串行消费（一条 30s 封顶），连发
            // 排队到容量 4；打满丢弃由 SPA 无回执感知 + 重录重发兜底。
            act @ InboundAction::AsrRequest { .. } => {
                if tokio::time::timeout(Duration::from_secs(1), self.asr_tx.send(act))
                    .await
                    .is_err()
                {
                    warn!(target: TARGET, "asr channel 持续满 1s，丢弃 asr_request 请求");
                }
            }
            // v1.44 网关信息查询：无状态应答（版本 + 能力字面量都是编译期
            // 常量），本层直接出站，不必绕 sync 泵。best-effort：丢弃由 SPA
            // 重发兜底（重连 / 加密会话建立后各发一次）。
            InboundAction::GatewayInfoQuery => {
                debug!(target: TARGET, "gateway_info_query：回 gateway_info");
                self.enqueue_client_frame(
                    crate::encode_gateway_info_frame(&self.machine_id),
                    "gateway_info",
                );
            }
            // ping 已在 dispatch_business 处理（唯一要写 ws 的动作）。
            InboundAction::Ping => {}
            InboundAction::Ignore => {
                debug!(target: TARGET, kind = v.get("type").and_then(|t| t.as_str()).unwrap_or(""), "未知入站帧 type，忽略（前向兼容）");
            }
            InboundAction::Warn(reason) => {
                warn!(target: TARGET, kind = v.get("type").and_then(|t| t.as_str()).unwrap_or(""), reason, "入站帧违规，丢弃");
                // v1.44：违规帧若带合规 clientId，回 `rejected{malformed}`——
                // 发送方据此把那条消息标成「未送达：消息格式无效」，而不是
                // 等到超时变「未确认送达」。convId 按 message 帧既有规则归一
                //（缺席 / 非字符串 → 默认会话）：发送方没写会话时回执落默认
                // 会话，正是它那条消息被归到的会话。
                if let Some(client_id) = parse_client_id(v.get("clientId")) {
                    let conv = normalize_conv(
                        v.get("convId").and_then(|c| c.as_str()),
                        &self.default_session,
                    );
                    self.enqueue_client_frame(
                        crate::encode_ack_frame(
                            &self.machine_id,
                            &client_id,
                            &conv,
                            AckStatus::Rejected {
                                reason: crate::ACK_MALFORMED_REASON,
                                message: crate::ACK_MALFORMED_MESSAGE,
                            },
                        ),
                        "ack(malformed)",
                    );
                }
            }
        }
        Ok(())
    }

    /// 业务消息投进入站 channel（→ platform.recv）。v1.44：消息带合规
    /// `client_id` 且**投递成功之后**才出站 `ack{received}`——回执证明的是
    /// 「到达了网关」，发早了会把一条实际被丢弃的消息报成已送达。入站
    /// channel 持续满 1s 被丢弃时不发回执（浏览器超时判定为「未确认送达」）。
    async fn deliver_inbound(&self, msg: Box<InboundMessage>) -> Result<()> {
        // 投递之前先取出回执要素（msg 随 send 被 move 走）。
        let receipt = msg
            .client_id
            .as_ref()
            .map(|client_id| (msg.conv_id.0.clone(), client_id.clone()));
        // 入站 best-effort 推给 platform：1s 有界背压（消费端短暂抖动不丢
        // 用户消息；持续满才丢弃，对齐 wecom P5-12）。
        match tokio::time::timeout(Duration::from_secs(1), self.inbound_tx.send(*msg)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                return Err(CoreError::Platform(
                    TARGET,
                    "入站 channel 已关闭（platform 退出）".into(),
                ));
            }
            Err(_) => {
                warn!(target: TARGET, "入站 channel 持续满 1s，丢弃浏览器消息");
                return Ok(());
            }
        }
        // 走到这里 = 消息已在入站 channel 里。
        if let Some((conv, client_id)) = receipt {
            self.ack(&client_id, &conv, AckStatus::Received);
        }
        Ok(())
    }

    /// v1.44 送达回执出站（best-effort，见 [`Self::enqueue_client_frame`]）。
    fn ack(&self, client_id: &str, conv: &str, status: AckStatus) {
        self.enqueue_client_frame(
            crate::encode_ack_frame(&self.machine_id, client_id, conv, status),
            "ack",
        );
    }

    /// client 自发帧入队（v1.44：`ack` / `gateway_info`）——走与 platform 的
    /// 出站帧同一条通道，由本任务的 select 循环取出发送（E2E 密钥活跃时
    /// 逐把加密）。best-effort：队列满 / 通道关闭只 debug、不重试——回执
    /// 丢一条的后果只是浏览器把那条消息判成「未确认送达」，用户可重发；
    /// `gateway_info` 丢了对 SPA 只是暂时判定「不支持回执」、不显示状态。
    fn enqueue_client_frame(&self, frame: Result<String>, what: &'static str) {
        match frame {
            Ok(json) => {
                if let Err(e) = self.outbound_tx.try_send(json) {
                    debug!(
                        target: TARGET,
                        error = %e,
                        frame = what,
                        "client 自发出站帧入队失败（队列满 / 通道关闭），丢弃"
                    );
                }
            }
            Err(e) => {
                warn!(target: TARGET, error = %e, frame = what, "client 自发出站帧序列化失败");
            }
        }
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
        let (sync_tx, _sync_rx) = mpsc::channel::<SyncRequest>(8);
        let (workspace_tx, _workspace_rx) = mpsc::channel::<InboundAction>(8);
        let (media_tx, _media_rx) = mpsc::channel::<InboundAction>(8);
        let (history_tx, _history_rx) = mpsc::channel::<InboundAction>(8);
        let (asr_tx, _asr_rx) = mpsc::channel::<InboundAction>(8);
        let (_outbound_tx_for_client, outbound_rx) = mpsc::channel::<String>(8);
        let client = WebClient {
            relay_url: "ws://127.0.0.1:1/gateway".into(),
            credential: GatewayCredential::Token("t".into()),
            machine_id: "test-mac".into(),
            default_session: "default".into(),
            online: Arc::new(AtomicBool::new(false)),
            e2e: None,
            inbound_tx,
            sync_tx,
            workspace_tx,
            media_tx,
            history_tx,
            asr_tx,
            peer_state: Arc::new(std::sync::Mutex::new(None)),
            outbound_tx: _outbound_tx_for_client,
        };
        let (_push_tx, push_rx) = mpsc::channel::<PushEvent>(8);
        let token = CancellationToken::new();
        token.cancel();
        let res = tokio::time::timeout(
            Duration::from_secs(2),
            client.run(
                outbound_rx,
                push_rx,
                Arc::new(tokio::sync::Notify::new()),
                token,
            ),
        )
        .await;
        assert!(res.is_ok(), "已取消的 token 应让 run 直接退出");
    }

    // ---- v1.44 送达回执 / 网关信息（dispatch_action 直测：不连 relay；
    // 真实中继全链路见 tests/web_loopback.rs）----

    /// 不连 relay 的 WebClient + 各通道接收端。入站 / 出站通道容量可配
    /// （「入站打满不发回执」「出站打满不 panic」两条用例各要一个容量）。
    struct ClientRig {
        client: WebClient,
        inbound_rx: mpsc::Receiver<InboundMessage>,
        outbound_rx: mpsc::Receiver<String>,
        media_rx: mpsc::Receiver<InboundAction>,
        /// 保活：其余泵通道的接收端（drop 即视为泵退出，client 侧会 warn）。
        _other_rx: (
            mpsc::Receiver<SyncRequest>,
            mpsc::Receiver<InboundAction>,
            mpsc::Receiver<InboundAction>,
            mpsc::Receiver<InboundAction>,
        ),
    }

    fn rig(inbound_cap: usize, outbound_cap: usize) -> ClientRig {
        let (inbound_tx, inbound_rx) = mpsc::channel(inbound_cap);
        let (outbound_tx, outbound_rx) = mpsc::channel(outbound_cap);
        let (sync_tx, sync_rx) = mpsc::channel(8);
        let (workspace_tx, workspace_rx) = mpsc::channel(8);
        let (media_tx, media_rx) = mpsc::channel(8);
        let (history_tx, history_rx) = mpsc::channel(8);
        let (asr_tx, asr_rx) = mpsc::channel(8);
        ClientRig {
            client: WebClient {
                relay_url: "ws://127.0.0.1:1/gateway".into(),
                credential: GatewayCredential::Token("t".into()),
                machine_id: "test-mac".into(),
                default_session: "default".into(),
                online: Arc::new(AtomicBool::new(false)),
                e2e: None,
                inbound_tx,
                outbound_tx,
                sync_tx,
                workspace_tx,
                media_tx,
                history_tx,
                asr_tx,
                peer_state: Arc::new(std::sync::Mutex::new(None)),
            },
            inbound_rx,
            outbound_rx,
            media_rx,
            _other_rx: (sync_rx, workspace_rx, history_rx, asr_rx),
        }
    }

    impl ClientRig {
        /// 把一条原始入站帧经与生产相同的路径分发（映射 + dispatch_action）。
        async fn dispatch(&self, raw: &str) {
            let v: serde_json::Value = serde_json::from_str(raw).unwrap();
            let act = map_inbound_frame(&v, &self.client.default_session);
            self.client.dispatch_action(&v, act).await.unwrap();
        }
    }

    /// 取出 outbound 通道里现有的全部帧。不等待：入队是同步 `try_send`，
    /// `dispatch` 返回时帧已在通道里。
    fn drained(rx: &mut mpsc::Receiver<String>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(json) = rx.try_recv() {
            out.push(json);
        }
        out
    }

    const RECEIVED_ACK: &str = r#"{"v":1,"type":"ack","machine":"test-mac","clientId":"m_1a2b3c4d","convId":"web:s-1","status":"received"}"#;
    const MALFORMED_ACK: &str = r#"{"v":1,"type":"ack","machine":"test-mac","clientId":"m_1a2b3c4d","convId":"web:s-1","status":"rejected","reason":"malformed","message":"消息格式无效"}"#;
    const MESSAGE_WITH_CLIENT_ID: &str = r#"{"v":1,"type":"message","convId":"web:s-1","text":"列出目录","machine":"home-mac","clientId":"m_1a2b3c4d"}"#;

    /// v1.44 `received` 回执：带合规 clientId 的 message 进入入站 channel 后，
    /// 出站恰一帧 ack（字节级），消息的 client_id 随 InboundMessage 带入。
    #[tokio::test]
    async fn received_ack_sent_after_inbound_enqueue() {
        let mut r = rig(8, 8);
        r.dispatch(MESSAGE_WITH_CLIENT_ID).await;
        let m = r.inbound_rx.try_recv().expect("消息应已进入入站 channel");
        assert_eq!(m.conv_id.0, "web:s-1");
        assert_eq!(m.text.as_deref(), Some("列出目录"));
        assert_eq!(m.client_id.as_deref(), Some("m_1a2b3c4d"));
        assert_eq!(
            drained(&mut r.outbound_rx),
            vec![RECEIVED_ACK.to_string()],
            "恰一帧 received ack"
        );
    }

    /// v1.44 不带 / 不合规 clientId 的消息照旧处理、**不发任何回执**（旧 SPA
    /// 一切如旧）；带图的 message 经 media channel 转发时 clientId 随动作带到
    /// 泵——`received` 由泵在合成消息进入入站 channel 之后发，本层不发。
    #[tokio::test]
    async fn no_received_ack_without_valid_client_id() {
        let mut r = rig(8, 8);
        let over = "m".repeat(65);
        for raw in [
            r#"{"v":1,"type":"message","convId":"web:s-1","text":"旧 SPA"}"#.to_string(),
            format!(
                r#"{{"v":1,"type":"message","convId":"web:s-1","text":"超长","clientId":"{over}"}}"#
            ),
            r#"{"v":1,"type":"message","convId":"web:s-1","text":"非法字符","clientId":"a.b"}"#
                .to_string(),
            r#"{"v":1,"type":"message","convId":"web:s-1","text":"非字符串","clientId":42}"#
                .to_string(),
        ] {
            r.dispatch(&raw).await;
        }
        let mut delivered = 0;
        while let Ok(m) = r.inbound_rx.try_recv() {
            assert_eq!(m.client_id, None, "不合规 clientId 当缺席");
            delivered += 1;
        }
        assert_eq!(delivered, 4, "消息照旧进入入站 channel");
        assert!(
            drained(&mut r.outbound_rx).is_empty(),
            "无合规 clientId 不发回执"
        );

        // 媒体消息：clientId 随动作到泵，本层不发 received。
        r.dispatch(
            r#"{"v":1,"type":"message","convId":"web:s-1","text":"看图","clientId":"m_img00001","media":{"name":"a.png","mime":"image/png","b64":"aGk="}}"#,
        )
        .await;
        match r.media_rx.try_recv().expect("媒体动作应转发到泵") {
            InboundAction::MediaMessage {
                conv, client_id, ..
            } => {
                assert_eq!(conv, "web:s-1");
                assert_eq!(client_id.as_deref(), Some("m_img00001"));
            }
            other => panic!("应为 MediaMessage：{other:?}"),
        }
        assert!(
            drained(&mut r.outbound_rx).is_empty(),
            "媒体消息此刻还没进入入站 channel：不得发 received"
        );
    }

    /// v1.44 入站 channel 打满：消息等 1s 后被丢弃，**不得发 received**——回执
    /// 只在消息真正进入 channel 之后发，否则会把一条被丢弃的消息报成已送达
    /// （浏览器据超时判定为「未确认送达」）。1s 是被测代码的有界背压常量，
    /// 不是用 sleep 表达时序。
    #[tokio::test]
    async fn no_received_ack_when_inbound_channel_full() {
        let mut r = rig(1, 8);
        // 占满唯一的槽位（无人消费）。
        r.client
            .inbound_tx
            .send(crate::synth_message(
                "web:x".into(),
                "占位".into(),
                None,
                None,
            ))
            .await
            .unwrap();
        r.dispatch(MESSAGE_WITH_CLIENT_ID).await;
        assert!(
            drained(&mut r.outbound_rx).is_empty(),
            "消息没进 channel：不得发 received"
        );
        let m = r.inbound_rx.try_recv().expect("占位消息仍在");
        assert_eq!(m.text.as_deref(), Some("占位"));
        assert!(
            r.inbound_rx.try_recv().is_err(),
            "被丢弃的消息不在 channel 里"
        );
    }

    /// v1.44 出站通道打满：`received` 回执 best-effort 丢弃——不 panic、不
    /// 返回 Err，消息照常投递（回执丢一条只是浏览器判成「未确认送达」）。
    /// tokio 的 mpsc 容量必须 > 0，用容量 1 + 预占一帧造「满」。
    #[tokio::test]
    async fn received_ack_dropped_without_panic_when_outbound_full() {
        let mut r = rig(8, 1);
        r.client
            .outbound_tx
            .try_send(r#"{"v":1,"type":"占位"}"#.to_string())
            .expect("预占出站通道");
        r.dispatch(MESSAGE_WITH_CLIENT_ID).await;
        assert!(r.inbound_rx.try_recv().is_ok(), "消息照常进入入站 channel");
        assert_eq!(
            drained(&mut r.outbound_rx),
            vec![r#"{"v":1,"type":"占位"}"#.to_string()],
            "只有预占的那一帧：回执被丢弃，通道里没有新帧"
        );
    }

    /// v1.44 `malformed` 回执：帧校验失败（Warn）且带合规 clientId → 出站
    /// `ack{rejected, malformed}`（字节级）；convId 按 message 帧既有规则归一
    /// （缺席 → 默认会话、裸名补前缀）；Warn 但无合规 clientId 不回（旧 SPA
    /// 形态）。被拒的消息不进入入站 channel。
    #[tokio::test]
    async fn malformed_ack_for_warn_frame_with_client_id() {
        let mut r = rig(8, 8);
        // 缺 text。
        r.dispatch(r#"{"v":1,"type":"message","convId":"web:s-1","machine":"home-mac","clientId":"m_1a2b3c4d"}"#)
            .await;
        assert_eq!(
            drained(&mut r.outbound_rx),
            vec![MALFORMED_ACK.to_string()],
            "缺 text 的 message → malformed 回执"
        );
        // 缺 convId → 默认会话；裸 session 名 → 补 web: 前缀。
        r.dispatch(r#"{"v":1,"type":"message","clientId":"m_nocode01"}"#)
            .await;
        r.dispatch(r#"{"v":1,"type":"message","convId":"s-2","clientId":"m_bare001"}"#)
            .await;
        // media 段畸形 + 合规 clientId。
        r.dispatch(
            r#"{"v":1,"type":"message","convId":"web:s-1","text":"x","media":"png","clientId":"m_med00001"}"#,
        )
        .await;
        let frames = drained(&mut r.outbound_rx);
        assert_eq!(frames.len(), 3, "{frames:?}");
        assert_eq!(
            frames[0],
            r#"{"v":1,"type":"ack","machine":"test-mac","clientId":"m_nocode01","convId":"web:default","status":"rejected","reason":"malformed","message":"消息格式无效"}"#
        );
        assert_eq!(
            frames[1],
            r#"{"v":1,"type":"ack","machine":"test-mac","clientId":"m_bare001","convId":"web:s-2","status":"rejected","reason":"malformed","message":"消息格式无效"}"#
        );
        assert_eq!(
            frames[2],
            r#"{"v":1,"type":"ack","machine":"test-mac","clientId":"m_med00001","convId":"web:s-1","status":"rejected","reason":"malformed","message":"消息格式无效"}"#
        );

        // Warn 但无合规 clientId：不回（旧 SPA 的违规帧一切如旧）。
        r.dispatch(r#"{"v":1,"type":"message"}"#).await;
        r.dispatch(r#"{"v":1,"type":"message","clientId":"bad.id"}"#)
            .await;
        r.dispatch(r#"{"v":1,"type":"approval_response","convId":"web:s-1","requestId":"r"}"#)
            .await;
        assert!(
            drained(&mut r.outbound_rx).is_empty(),
            "无合规 clientId 不回"
        );
        assert!(
            r.inbound_rx.try_recv().is_err(),
            "被拒 / 违规的消息都不进入入站 channel"
        );
    }

    /// v1.44 `gateway_info_query` → `gateway_info`（字节级，version = 本 crate
    /// 版本）；任何机器名 / 是否带 machine 字段都回。
    #[tokio::test]
    async fn gateway_info_query_replies_byte_level() {
        let mut r = rig(8, 8);
        for raw in [
            r#"{"v":1,"type":"gateway_info_query","machine":"home-mac"}"#,
            r#"{"v":1,"type":"gateway_info_query"}"#,
        ] {
            r.dispatch(raw).await;
        }
        let want = format!(
            r#"{{"v":1,"type":"gateway_info","machine":"test-mac","version":"{}","caps":["acks"]}}"#,
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(drained(&mut r.outbound_rx), vec![want.clone(), want]);
        assert!(r.inbound_rx.try_recv().is_err(), "查询不进入入站 channel");
    }
}
