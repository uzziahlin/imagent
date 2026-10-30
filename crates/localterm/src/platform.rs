//! [`LocalPlatform`]：`imagent claude` 的网关侧——unix socket server +
//! [`imagent_core::Platform`] 实现，接入 Dispatcher 全管线。
//!
//! # 结构（参照 web platform：双 channel + 后台任务 + 平台薄壳）
//!
//! - `recv()`：inbound channel 前端（连接任务已把终端帧映射成
//!   [`InboundMessage`]）；无连接期间无消息天然 pending（同 web 断线语义）。
//! - 出站：`send_*` 覆写为帧 → 全局 outbound channel（容量 256）→ pump 任务
//!   转发到**当前连接**的 per-conn channel → 连接任务写 socket。无连接 / 握手
//!   未完成时 pump 挂起、帧在全局 channel 排队（终端重连后按序送达，同 web
//!   的断线排队语义）。
//! - 单连接（踢旧 4001，同 relay 语义）：一个时刻只有一个终端视图——新
//!   `imagent claude`（同名或异名会话）接入即顶替旧连接。
//!
//! # 帧投递顺序与 sync 快照（无重复论证）
//!
//! 文本缓存在**写 socket 成功后**才追加（缓存 = 已送达历史）。重连握手
//! （hello → hello_ack → 首帧 sync）期间连接未对 pump 就绪，快照直接写入本
//! 连接；随后就绪的 pump 才开始转发全局队列里的**未送达**帧——终端看到
//! 「快照（旧历史）→ 队列增量（新内容）」，两者不重叠、不丢序。
//!
//! # 鉴权（为什么 hello 免 token）
//!
//! 本地 socket 的凭证链：socket 文件 **0600**（同 permission.sock 姿态，其它
//! uid 连不上）+ accept 侧 **peer uid 校验**（同 uid = 网关本主，对齐 core
//! dispatch::socket 的先例——core 侧 `peer_uid` 是 pub(crate) 不可复用，本
//! crate 自持一份等价实现）。web 的 hello token 解决的是「浏览器经公网 relay
//! 证明自己」，本地 socket 权限即凭证，无需再带 token。
//!
//! # 文本流路径与 send_card 决策（实测 core 后的定案）
//!
//! `supports_streaming_card` 恒 false → core dispatch（round.rs）不建
//! CardSession，Text chunk 经 400ms 合帧窗口逐段 send_text；ToolUse/ToolResult
//! 轮次中不出站，最终回复自动追加 `🔧 工具调用：…` / `📋 计划进度：…`——
//! 终端经 text 帧直接受益。send_card 的 trait default（card.text 当一条文本
//! 发送、update_card no-op）在文本路径**不会被重复触发**（CardSession 不建，
//! core 对非卡片平台每轮至多一条终态回复），故不覆写——若实测有重发面再
//! 覆写为增量/终态（实测结论：无）。

use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use imagent_core::{
    ConvId, CoreError, InboundMessage, MediaRef, PermissionReply, Platform, PlatformCaps,
    ReplyHint, Result,
};

use crate::{
    conv_of, encode_approval_request_frame, encode_approval_resolved_frame, encode_error_frame,
    encode_hello_ack, encode_text_frame, map_inbound_frame, resolved_decision,
    sanitize_session_name, InboundAction, TARGET,
};

const PLATFORM: &str = crate::PLATFORM_NAME;

/// 全局出站 channel 容量：无连接期 send_* 帧在此排队（终端重连后按序发出）；
/// 打满（终端长时间不接入）→ send_* 返回 Err（对齐 web 的 §5.1 语义）。
const OUTBOUND_CHANNEL_CAPACITY: usize = 256;
/// 入站 channel 容量（对齐 web）。
const INBOUND_CHANNEL_CAPACITY: usize = 64;
/// per-conn channel 容量：pump → 连接任务（写 socket）的缓冲；打满时 pump 在
/// `send().await` 处背压，天然传导到全局队列。
const CONN_CHANNEL_CAPACITY: usize = 256;
/// hello 等待上限（超时按握手失败断开，防慢速连接占坑）。
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// 握手后首帧 sync 的等待窗口（客户端重连后立即 sync 恢复上下文；首连无缓存
/// 可不发）。窗口内无 sync 也直接就绪。
const SYNC_WINDOW: Duration = Duration::from_secs(2);
/// 入站有界背压（对齐 web client：dispatcher 消费抖动不丢终端消息，持续满才丢）。
const INBOUND_BACKPRESSURE: Duration = Duration::from_secs(1);
/// 单帧写超时：客户端僵死（不读，缓冲打满）时不让连接任务无限挂起。
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// 被顶替连接的强杀兜底宽限（> WRITE_TIMEOUT：正常路径写完 4001 已自退，
/// 到这个点还在的必是僵死连接）。
const KICK_ABORT_GRACE: Duration = Duration::from_secs(15);

/// error 帧 code：新连接顶替（对齐 relay close 4001 语义）。
pub(crate) const CODE_SUPERSEDED: &str = "4001";
/// error 帧 code：hello 握手违规。
pub(crate) const CODE_BAD_HELLO: &str = "4002";
/// error 帧 code：peer uid 校验拒绝。
pub(crate) const CODE_PEER_REJECTED: &str = "4003";

/// sync 重发的文本缓存上限（行）：终端重连后恢复最近的上下文滚动。
pub(crate) const TEXT_CACHE_MAX_LINES: usize = 200;
/// 缓存覆盖的 conv 上限（超限淘汰最久未活跃，防长寿命进程内存无界增长）。
pub(crate) const TEXT_CACHE_MAX_CONVS: usize = 16;

/// 中毒（临界区 panic）时取回数据继续服务：纯内存临界区无 IO 副作用可回滚
/// （web platform / relay Hub 同款取舍）。
fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// 出站帧内部形态
// ---------------------------------------------------------------------------

/// 出站帧（序列化延迟到写 socket 时；Text 帧写成功后入缓存）。
#[derive(Debug, Clone)]
pub(crate) enum OutFrame {
    Text {
        conv: String,
        text: String,
    },
    ApprovalRequest {
        conv: String,
        request_id: String,
        tool: String,
        input_summary: String,
    },
    ApprovalResolved {
        conv: String,
        request_id: String,
        decision: String,
        message: Option<String>,
    },
    /// 协议层错误（踢旧 / 握手违规）——不走缓存。
    Error {
        code: &'static str,
        message: &'static str,
    },
}

impl OutFrame {
    fn encode(&self) -> Result<String> {
        match self {
            OutFrame::Text { conv, text } => encode_text_frame(conv, text),
            OutFrame::ApprovalRequest {
                conv,
                request_id,
                tool,
                input_summary,
            } => encode_approval_request_frame(conv, request_id, tool, input_summary),
            OutFrame::ApprovalResolved {
                conv,
                request_id,
                decision,
                message,
            } => encode_approval_resolved_frame(conv, request_id, decision, message.as_deref()),
            // 纯 &'static str 字段，encode_error_frame 不可失败。
            OutFrame::Error { code, message } => Ok(encode_error_frame(code, message)),
        }
    }
}

// ---------------------------------------------------------------------------
// 文本缓存（sync 重发）
// ---------------------------------------------------------------------------

/// per-conv 最近文本缓存：`conv -> (最近活跃 seq, 行队列)`。seq 单调递增，兼作
/// conv 淘汰序（最小者最久未活跃）。行上限 [`TEXT_CACHE_MAX_LINES`]。
#[derive(Default)]
pub(crate) struct TextCache {
    entries: HashMap<String, (u64, VecDeque<String>)>,
    next_seq: u64,
}

impl TextCache {
    /// 追加一行（一次 send_text 的载荷计一行）；超行数上限丢最旧。
    pub(crate) fn push(&mut self, conv: &str, line: String) {
        let seq = self.next_seq;
        self.next_seq += 1;
        let entry = self
            .entries
            .entry(conv.to_string())
            .or_insert_with(|| (seq, VecDeque::new()));
        entry.0 = seq;
        entry.1.push_back(line);
        while entry.1.len() > TEXT_CACHE_MAX_LINES {
            entry.1.pop_front();
        }
        if self.entries.len() > TEXT_CACHE_MAX_CONVS {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (s, _))| *s)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest);
            }
        }
    }

    /// 快照（旧→新 join 为单条文本；空缓存返回 None）。
    pub(crate) fn snapshot_joined(&self, conv: &str) -> Option<String> {
        let (_, q) = self.entries.get(conv)?;
        if q.is_empty() {
            return None;
        }
        Some(q.iter().cloned().collect::<Vec<_>>().join("\n"))
    }

    /// 当前行数（测试用）。
    #[cfg(test)]
    pub(crate) fn len(&self, conv: &str) -> usize {
        self.entries.get(conv).map(|(_, q)| q.len()).unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// 服务端状态与后台任务
// ---------------------------------------------------------------------------

/// 当前连接句柄（accept loop 装入 slot；kick 用 tx + task）。
struct ConnHandle {
    id: u64,
    /// 握手完成标志：pump 只向就绪连接投递（握手期帧仍留全局队列，保证
    /// sync 快照先于队列增量送达）。
    ready: Arc<AtomicBool>,
    tx: mpsc::Sender<OutFrame>,
    task: JoinHandle<()>,
}

/// server 共享状态（accept / pump / 连接任务三方持有）。
struct ServerState {
    /// 当前连接（单连接语义）。std Mutex：锁内无 await。
    slot: Mutex<Option<ConnHandle>>,
    /// slot 装卸 / ready 翻转的版本号（watch 保留最新值，pump 无丢失唤醒）。
    epoch: watch::Sender<u64>,
    /// 在线标志（连接任务维护）——doctor_probes / 测试用。
    online: Arc<AtomicBool>,
    inbound_tx: mpsc::Sender<InboundMessage>,
    cache: Mutex<TextCache>,
    shutdown: CancellationToken,
}

impl ServerState {
    /// 版本号 +1（slot 变更 / 就绪翻转后调用，唤醒 pump 重新探测）。
    fn bump(&self) {
        self.epoch.send_modify(|e| *e += 1);
    }
}

/// accept 循环：peer uid 校验 → 踢旧 → spawn 连接任务。
async fn accept_loop(listener: UnixListener, state: Arc<ServerState>, socket_path: PathBuf) {
    let mut next_id: u64 = 0;
    loop {
        let (stream, _) = tokio::select! {
            _ = state.shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok(s) => s,
                Err(e) => {
                    warn!(target: TARGET, error = %e, "local.sock accept 失败，1s 后重试");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            },
        };
        // peer uid 校验（socket 0600 之外的第二道门；两者共同构成「免 token」
        // 的凭证链，见模块文档）。
        match peer_uid(&stream) {
            Some(uid) if uid == current_uid() => {}
            other => {
                warn!(
                    target: TARGET,
                    peer_uid = ?other,
                    "local.sock 对端非网关同 uid 用户，拒绝连接"
                );
                let mut stream = stream;
                let _ = stream
                    .write_all(
                        format!(
                            "{}\n",
                            encode_error_frame(CODE_PEER_REJECTED, "对端 uid 非网关本主，拒绝连接")
                        )
                        .as_bytes(),
                    )
                    .await;
                continue; // stream drop 即关闭
            }
        }
        // 踢旧（单连接：同名/异名新终端都顶替旧视图）。不立即 abort——
        // 4001 错误帧在 per-conn 队列里，立即 abort 会连帧一起丢：连接任务
        // 写完 4001 自行退出（见收发循环的自终结分支），僵死客户端由写超时
        // / 下方延迟 abort 兜底。
        if let Some(old) = lock_or_recover(&state.slot).take() {
            let _ = old.tx.try_send(OutFrame::Error {
                code: CODE_SUPERSEDED,
                message: "被新连接顶替（imagent claude 单连接，本视图请退出）",
            });
            let task = old.task;
            tokio::spawn(async move {
                tokio::time::sleep(KICK_ABORT_GRACE).await;
                task.abort();
            });
        }
        // 新连接握手完成前不在线（ready 后由连接任务置回）。
        state.online.store(false, Ordering::Relaxed);
        next_id += 1;
        let (tx, rx) = mpsc::channel(CONN_CHANNEL_CAPACITY);
        let ready = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(serve_conn(
            stream,
            next_id,
            tx.clone(),
            rx,
            ready.clone(),
            state.clone(),
        ));
        lock_or_recover(&state.slot).replace(ConnHandle {
            id: next_id,
            ready,
            tx,
            task,
        });
        state.bump();
    }
    info!(
        target: TARGET,
        path = %socket_path.display(),
        "local.sock accept 循环退出（平台停机）"
    );
}

/// 解析 hello 首帧：`{"type":"hello","session":<合法会话名>}` → Some(session)。
fn parse_hello(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("hello") {
        return None;
    }
    sanitize_session_name(v.get("session").and_then(|s| s.as_str())?)
}

async fn write_line(wr: &mut OwnedWriteHalf, json: &str) -> std::io::Result<()> {
    wr.write_all(json.as_bytes()).await?;
    wr.write_all(b"\n").await
}

/// 单条入站文本帧分拣（握手完成后的业务帧；畸形 JSON debug 丢弃不断连）。
async fn handle_inbound_line(
    state: &ServerState,
    session: &str,
    line: &str,
    tx: &mpsc::Sender<OutFrame>,
) {
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            debug!(target: TARGET, error = %e, "入站帧解析失败，丢弃");
            return;
        }
    };
    match map_inbound_frame(&v, session) {
        InboundAction::Message(msg) => {
            // 有界背压（1s）：dispatcher 消费抖动不丢终端消息，持续满才丢。
            match tokio::time::timeout(INBOUND_BACKPRESSURE, state.inbound_tx.send(*msg)).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    warn!(target: TARGET, "入站 channel 已关闭（平台退出），断开连接");
                }
                Err(_) => {
                    warn!(target: TARGET, "入站 channel 持续满 1s，丢弃终端消息");
                }
            }
        }
        InboundAction::Sync => {
            // 手动补拉：快照走本连接 per-conn channel 尾部（与 pump 转发帧同队
            // 保序）；重发历史在活帧之后，属「重拉上下文」语义。
            let conv = conv_of(session);
            // 快照先取出（guard 不跨 await）。
            let snapshot = lock_or_recover(&state.cache).snapshot_joined(&conv);
            if let Some(joined) = snapshot {
                let _ = tx.send(OutFrame::Text { conv, text: joined }).await;
            }
        }
        InboundAction::Ignore => {
            debug!(target: TARGET, "未知入站帧 type，忽略（前向兼容）");
        }
        InboundAction::Warn(reason) => {
            warn!(target: TARGET, reason, "入站帧违规，丢弃");
        }
    }
}

/// 单连接生命周期：握手（hello → hello_ack → 首帧 sync 快照）→ 就绪 → 收发。
async fn serve_conn(
    stream: UnixStream,
    id: u64,
    tx: mpsc::Sender<OutFrame>,
    mut rx: mpsc::Receiver<OutFrame>,
    ready: Arc<AtomicBool>,
    state: Arc<ServerState>,
) {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    // ① 握手：hello 带会话名（免 token——peer uid 已在 accept 侧校验）。
    let session = match tokio::time::timeout(HELLO_TIMEOUT, lines.next_line()).await {
        Ok(Ok(Some(line))) => parse_hello(&line),
        Ok(Ok(None)) => None,
        Ok(Err(e)) => {
            debug!(target: TARGET, error = %e, "读 hello 失败");
            None
        }
        Err(_) => {
            debug!(target: TARGET, "等 hello 超时");
            None
        }
    };
    let Some(session) = session else {
        let _ = write_line(
            &mut wr,
            &encode_error_frame(
                CODE_BAD_HELLO,
                "hello 帧违规（须 type=hello + session，会话名 ≤64 char、不含 ':'）",
            ),
        )
        .await;
        return;
    };
    match encode_hello_ack(&session) {
        Ok(json) => {
            if write_line(&mut wr, &json).await.is_err() {
                return;
            }
        }
        Err(e) => {
            warn!(target: TARGET, error = %e, "hello_ack 序列化失败，断开");
            return;
        }
    }

    // ② 首帧 sync 窗口：重连客户端在此恢复上下文。快照直发（本连接尚不对
    //    pump 就绪 → 队列增量必在快照之后，见模块文档的顺序论证）。
    match tokio::time::timeout(SYNC_WINDOW, lines.next_line()).await {
        Ok(Ok(Some(line))) => {
            let is_sync = serde_json::from_str::<serde_json::Value>(&line)
                .ok()
                .and_then(|v| {
                    matches!(v.get("type").and_then(|t| t.as_str()), Some("sync")).then_some(())
                });
            if is_sync.is_some() {
                let conv = conv_of(&session);
                // 快照先取出（guard 不跨 await）。
                let snapshot = lock_or_recover(&state.cache).snapshot_joined(&conv);
                if let Some(joined) = snapshot {
                    if tx
                        .send(OutFrame::Text { conv, text: joined })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            } else {
                // 非 sync 首帧：按普通业务帧处理（向后兼容不发 sync 的客户端）。
                handle_inbound_line(&state, &session, &line, &tx).await;
            }
        }
        Ok(Ok(None)) => return, // EOF：客户端只探活
        Ok(Err(e)) => {
            debug!(target: TARGET, error = %e, "握手后读帧失败");
            return;
        }
        Err(_) => {} // 窗口内无 sync：直接就绪
    }

    // ③ 就绪：对 pump 开放投递。
    ready.store(true, Ordering::Relaxed);
    state.bump();
    state.online.store(true, Ordering::Relaxed);
    info!(target: TARGET, session = %session, "本地终端已连接");

    // ④ 收发循环。
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        handle_inbound_line(&state, &session, &line, &tx).await;
                    }
                    Ok(None) | Err(_) => break, // EOF / 读错误：连接结束
                }
            }
            maybe = rx.recv() => {
                let Some(frame) = maybe else { break };
                match frame.encode() {
                    Ok(json) => {
                        let wrote = tokio::time::timeout(WRITE_TIMEOUT, write_line(&mut wr, &json)).await;
                        match wrote {
                            Ok(Ok(())) => {
                                // 写成功才入缓存（缓存 = 已送达历史，sync 快照据此无重复）。
                                if let OutFrame::Text { conv, text } = &frame {
                                    lock_or_recover(&state.cache).push(conv, text.clone());
                                }
                                // 被顶替的自终结：per-conn 通道里的 Error 帧只有
                                // 顶替（CODE_SUPERSEDED）一种来源（握手/鉴权拒绝
                                // 的错误帧不经通道直写）——flush 完即退，客户端
                                // 据此退出视图。
                                if matches!(&frame, OutFrame::Error { .. }) {
                                    break;
                                }
                            }
                            Ok(Err(_)) => break, // 写失败：连接已死
                            Err(_) => break,     // 写超时：客户端僵死
                        }
                    }
                    Err(e) => warn!(target: TARGET, error = %e, "出站帧序列化失败，丢弃"),
                }
            }
        }
    }

    // ⑤ 退出清理：清 slot（id 匹配防误清顶替自己的新连接）。
    {
        let mut slot = lock_or_recover(&state.slot);
        if slot.as_ref().map(|h| h.id) == Some(id) {
            *slot = None;
            state.online.store(false, Ordering::Relaxed);
            state.bump();
        }
    }
    debug!(target: TARGET, session = %session, "本地终端连接退出");
}

/// 出站泵：全局 outbound channel → 当前就绪连接的 per-conn channel。无连接 /
/// 未就绪时帧留在手上等待（等价断线排队；全局 channel 容量即排队上限）。
async fn pump(mut outbound_rx: mpsc::Receiver<OutFrame>, state: Arc<ServerState>) {
    loop {
        let frame = tokio::select! {
            _ = state.shutdown.cancelled() => return,
            f = outbound_rx.recv() => match f {
                Some(f) => f,
                None => return, // platform 端 sender 全部 drop（平台退出）
            },
        };
        loop {
            // 先订阅再探测 slot——bump 发生在两次之间也不会错过（changed 等
            // 订阅之后的新值）。Sender 可 Clone，锁内取出、锁外 await。
            let mut epoch_rx = state.epoch.subscribe();
            let target = lock_or_recover(&state.slot)
                .as_ref()
                .filter(|h| h.ready.load(Ordering::Relaxed))
                .map(|h| h.tx.clone());
            if let Some(tx) = target {
                if tx.send(frame.clone()).await.is_ok() {
                    break;
                }
                // 发送失败：连接已死，其退出路径会清 slot + bump——掉回等待。
            }
            tokio::select! {
                _ = state.shutdown.cancelled() => return,
                r = epoch_rx.changed() => {
                    if r.is_err() {
                        return; // watch sender 全 drop（平台退出）
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// peer uid（core dispatch::socket 先例的等价自持实现——core 侧为 pub(crate)）
// ---------------------------------------------------------------------------

/// 本进程的 uid（peer-uid 鉴权基准）。
#[allow(unsafe_code)] // crate 顶层 `#![deny(unsafe_code)]`，此处显式豁免（core 先例）
fn current_uid() -> u32 {
    // SAFETY: getuid/geteuid 无参数、无副作用，永远安全。
    // macOS LOCAL_PEERCRED 返回 effective uid → 比对 geteuid；Linux
    // SO_PEERCRED 返回 real uid → 比对 getuid（与 core 同款口径）。
    #[cfg(target_os = "macos")]
    {
        unsafe { libc::geteuid() }
    }
    #[cfg(not(target_os = "macos"))]
    {
        unsafe { libc::getuid() }
    }
}

/// 取 UnixStream 对端的 uid（SO_PEERCRED / LOCAL_PEERCRED；其它 unix 返回 None
/// → 调用方拒绝）。
#[allow(unsafe_code)] // crate 顶层 `#![deny(unsafe_code)]`，此处显式豁免（core 先例）
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    // SAFETY: getsockopt 对已连接 unix socket 按 optname 填充固定大小输出缓冲，
    // 传入正确的 len；MaybeUninit/zeroed 避免读取未初始化字段。
    unsafe {
        #[cfg(target_os = "linux")]
        {
            let mut cred: std::mem::MaybeUninit<libc::ucred> = std::mem::MaybeUninit::uninit();
            let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            let rc = libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                cred.as_mut_ptr() as *mut libc::c_void,
                &mut len,
            );
            if rc == 0 {
                Some((*cred.as_ptr()).uid)
            } else {
                None
            }
        }
        #[cfg(target_os = "macos")]
        {
            let mut xucred: libc::xucred = std::mem::zeroed();
            let mut len = std::mem::size_of::<libc::xucred>() as libc::socklen_t;
            let rc = libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERCRED,
                &mut xucred as *mut libc::xucred as *mut libc::c_void,
                &mut len,
            );
            // cr_uid == u32::MAX 表示未填充/无效。
            if rc == 0 && xucred.cr_uid != u32::MAX {
                Some(xucred.cr_uid)
            } else {
                None
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = fd;
            None
        }
    }
}

// ---------------------------------------------------------------------------
// LocalPlatform
// ---------------------------------------------------------------------------

/// pending 审批记录（cancel_all 逐条收敛用）：`conv -> request_id 列表`。
type PendingAsks = HashMap<String, Vec<String>>;

/// 本地终端平台适配器（unix socket server + Platform 薄壳）。
pub struct LocalPlatform {
    socket_path: PathBuf,
    outbound_tx: mpsc::Sender<OutFrame>,
    /// 入站 channel 前端（`recv` 直接 await；AsyncMutex 跨 await 持锁，web 同款）。
    inbound_rx: Arc<AsyncMutex<mpsc::Receiver<InboundMessage>>>,
    pending_asks: Arc<Mutex<PendingAsks>>,
    online: Arc<AtomicBool>,
    shutdown: CancellationToken,
}

impl LocalPlatform {
    /// 构造并 spawn accept / pump 任务。`socket_path` 形如
    /// `<imagent_home>/local.sock`（main 装配传入）。
    ///
    /// bind 失败返回 Err（fail-closed：配置了 local 平台却起不来还带病运行，
    /// 等于「用户以为有终端、实际全部拒连」，对齐 webhook 的启动纪律）。
    pub fn new(socket_path: PathBuf) -> Result<Self> {
        // 陈旧 socket 清理：实例锁（start 侧）保证同 IMAGENT_HOME 只有本网关
        // 在跑，残留文件必来自已退出的上一实例——删除后重绑。
        if socket_path.exists() {
            std::fs::remove_file(&socket_path).map_err(|e| {
                CoreError::Platform(
                    PLATFORM,
                    format!("清理陈旧 local.sock 失败（{}）：{e}", socket_path.display()),
                )
            })?;
        }
        if let Some(parent) = socket_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let listener = UnixListener::bind(&socket_path).map_err(|e| {
            CoreError::Platform(
                PLATFORM,
                format!("bind local.sock 失败（{}）：{e}", socket_path.display()),
            )
        })?;
        // 0600：文件权限即第一道门（peer uid 是第二道），同 permission.sock 姿态。
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).map_err(
            |e| CoreError::Platform(PLATFORM, format!("local.sock chmod 0600 失败：{e}")),
        )?;

        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CHANNEL_CAPACITY);
        let (outbound_tx, outbound_rx) = mpsc::channel(OUTBOUND_CHANNEL_CAPACITY);
        let (epoch, _epoch_rx) = watch::channel(0u64);
        let online = Arc::new(AtomicBool::new(false));
        let shutdown = CancellationToken::new();
        let state = Arc::new(ServerState {
            slot: Mutex::new(None),
            epoch,
            online: online.clone(),
            inbound_tx,
            cache: Mutex::new(TextCache::default()),
            shutdown: shutdown.clone(),
        });
        tokio::spawn(accept_loop(listener, state.clone(), socket_path.clone()));
        tokio::spawn(pump(outbound_rx, state));
        info!(
            target: TARGET,
            path = %socket_path.display(),
            "本地终端平台已监听（imagent claude 接入）"
        );

        Ok(Self {
            socket_path,
            outbound_tx,
            inbound_rx: Arc::new(AsyncMutex::new(inbound_rx)),
            pending_asks: Arc::new(Mutex::new(PendingAsks::default())),
            online,
            shutdown,
        })
    }

    /// socket 路径（doctor / 测试用）。
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// 是否有已就绪的终端连接（doctor_probes / 测试用）。
    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// 出站帧入队：try_send——无连接期排队（Ok），队列满 / 任务退出才 Err，
    /// 不挂起 dispatch（对齐 web enqueue）。
    fn enqueue(&self, frame: OutFrame) -> Result<()> {
        match self.outbound_tx.try_send(frame) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(CoreError::Platform(
                PLATFORM,
                "出站队列满：终端长时间未接入（重连 imagent claude 可续收）".into(),
            )),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(CoreError::Platform(
                PLATFORM,
                "出站通道已关闭（平台后台任务已退出）".into(),
            )),
        }
    }

    /// 登记一条 pending 审批（cancel_all 逐条收敛用）。
    fn track_ask(&self, conv: &str, request_id: &str) {
        lock_or_recover(&self.pending_asks)
            .entry(conv.to_string())
            .or_default()
            .push(request_id.to_string());
    }

    /// 移除 pending 记录（已 resolve / 已 cancel）。
    fn untrack_ask(&self, conv: &str, request_id: &str) {
        let mut map = lock_or_recover(&self.pending_asks);
        if let Some(list) = map.get_mut(conv) {
            list.retain(|r| r != request_id);
            if list.is_empty() {
                map.remove(conv);
            }
        }
    }
}

/// 平台 drop：cancel 停机信号（accept / pump / 连接任务连动退出）+ 删 socket
/// 文件（防残留文件让下一实例 bind 失败 / `imagent claude` 误连死 socket）。
impl Drop for LocalPlatform {
    fn drop(&mut self) {
        self.shutdown.cancel();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[async_trait]
impl Platform for LocalPlatform {
    /// 能力声明：ASK（与覆写清单逐一对照）：
    /// - ASK：send_permission_ask（approval_request 帧）/ resolve_permission_ask /
    ///   cancel_permission_ask / cancel_all_permission_asks（approval_resolved
    ///   帧）✅。
    /// - ❌ 不声明 CARDS：supports_streaming_card 恒 false——core 据此走「文本
    ///   多发」路径（400ms 合帧流式，体感对齐裸 claude CLI）。实测 core 文本
    ///   路径（round.rs）：CardSession 不建、send_card default 只把 card.text
    ///   当**一条**文本发送、update_card default no-op——无「累积卡文本重发
    ///   多次」面，故 send_card/update_card 不覆写（default 即最优），也不
    ///   声明 CARDS 位。轮次中的工具过程不出站（core 在最终回复追加 🔧 工具
    ///   调用摘要 + 📋 计划进度，经 text 帧自然送达终端）。
    /// - ❌ 不声明（走 default 降级）：RECONNECT（本地 socket 无强制重连语义，
    ///   default Err 提示）；COMMAND_CARDS / FORMS（default 纯文本降级对终端
    ///   已够——按钮在终端就是文字命令）；REACTIONS / TYPING / URGENT_TEXT /
    ///   GROUP_CHATS（无对应语义）；MEDIA_UPLOAD（send_media 覆写为打印本地
    ///   文件路径的文本——非平台上传，不声明位，wecom/web 显式 Err 的对位物
    ///   这里是「路径即信息」）。
    /// 一致性测试 `local_caps_match_overrides` 钉住本值。
    fn capabilities(&self) -> PlatformCaps {
        PlatformCaps::ASK
    }

    fn name(&self) -> &'static str {
        PLATFORM
    }

    async fn recv(&self) -> Result<InboundMessage> {
        self.inbound_rx
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| CoreError::Platform(PLATFORM, "入站 channel 已关闭".into()))
    }

    async fn send_text(&self, conv: &ConvId, text: &str, _hint: &ReplyHint) -> Result<()> {
        self.enqueue(OutFrame::Text {
            conv: conv.0.clone(),
            text: text.to_string(),
        })
    }

    /// 终端无媒体上传通道；agent 产图的本地文件路径直接以文本回显（终端用户
    /// 拿到路径即可 cat / open——比报错有用）。与「打印路径」语义一致，不声明
    /// MEDIA_UPLOAD。
    async fn send_media(&self, conv: &ConvId, media: &MediaRef, hint: &ReplyHint) -> Result<()> {
        let text = format!("📎 {}（本地文件）：{}", media.kind, media.url);
        self.send_text(conv, &text, hint).await
    }

    /// 文本流路径（见 capabilities 注释的实测结论与模块文档）。
    fn supports_streaming_card(&self, _conv: &ConvId) -> bool {
        false
    }

    async fn send_permission_ask(
        &self,
        conv: &ConvId,
        request_id: &str,
        tool_name: &str,
        input_summary: &str,
        _hint: &ReplyHint,
    ) -> Result<Option<String>> {
        // inputSummary 压成人可读单行（与 web/feishu 审批同源）。
        let summary = imagent_core::render::tool_summary(tool_name, input_summary);
        self.track_ask(&conv.0, request_id);
        self.enqueue(OutFrame::ApprovalRequest {
            conv: conv.0.clone(),
            request_id: request_id.to_string(),
            tool: tool_name.to_string(),
            input_summary: summary,
        })?;
        // 返回 request_id 作 card_msg_id 锚点：终端审批回复带 requestId
        // （ask_req）精确路由（同 web——终端无「引用回复」形态，锚点即请求 id）。
        Ok(Some(request_id.to_string()))
    }

    async fn resolve_permission_ask(
        &self,
        conv: &ConvId,
        request_id: &str,
        reply: &PermissionReply,
    ) -> Result<()> {
        self.untrack_ask(&conv.0, request_id);
        self.enqueue(OutFrame::ApprovalResolved {
            conv: conv.0.clone(),
            request_id: request_id.to_string(),
            decision: resolved_decision(reply).to_string(),
            message: reply.message.clone(),
        })
    }

    async fn cancel_permission_ask(&self, conv: &ConvId, request_id: &str) -> Result<()> {
        self.untrack_ask(&conv.0, request_id);
        self.enqueue(OutFrame::ApprovalResolved {
            conv: conv.0.clone(),
            request_id: request_id.to_string(),
            decision: "cancelled".to_string(),
            message: Some("审批已取消（超时或任务被中断）".to_string()),
        })
    }

    async fn cancel_all_permission_asks(&self, conv: &ConvId) -> Result<()> {
        let drained = lock_or_recover(&self.pending_asks)
            .remove(&conv.0)
            .unwrap_or_default();
        for request_id in drained {
            self.enqueue(OutFrame::ApprovalResolved {
                conv: conv.0.clone(),
                request_id,
                decision: "cancelled".to_string(),
                message: Some("审批已取消（任务被 /stop 中断）".to_string()),
            })?;
        }
        Ok(())
    }

    async fn doctor_probes(&self) -> Vec<String> {
        let state = if self.is_online() {
            "终端已连接"
        } else {
            "等待 imagent claude 接入"
        };
        vec![format!(
            "✅ 本地终端 socket：{}（0600 + peer uid 校验，{state}）",
            self.socket_path.display()
        )]
    }
}

#[cfg(test)]
mod tests {
    //! 逻辑测试不接真客户端（回环行为见 tests/local_loopback.rs）。

    use super::*;

    fn tmp_socket(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "imagent-localterm-{}-{name}.sock",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn local_caps_match_overrides() {
        let p = LocalPlatform::new(tmp_socket("caps")).expect("bind 临时 socket");
        assert_eq!(p.capabilities(), PlatformCaps::ASK, "local 只声明 ask");
        assert_eq!(p.name(), "local");
        assert!(
            !p.supports_streaming_card(&ConvId("local:x".into())),
            "文本流路径：恒 false"
        );
        assert!(!p.capabilities().contains(PlatformCaps::MEDIA_UPLOAD));
        drop(p);
    }

    /// Drop 平台：cancel 停机信号 + 删除 socket 文件（防残留死 socket）。
    #[tokio::test]
    async fn drop_cancels_and_removes_socket() {
        let path = tmp_socket("drop");
        let p = LocalPlatform::new(path.clone()).expect("bind");
        let token = p.shutdown.clone();
        assert!(path.exists(), "构造后 socket 文件应存在");
        assert!(
            std::fs::metadata(&path)
                .map(|m| m.permissions().mode() & 0o777 == 0o600)
                .unwrap_or(false),
            "socket 权限 0600"
        );
        drop(p);
        assert!(token.is_cancelled(), "Drop 必须 cancel 停机信号");
        assert!(!path.exists(), "Drop 必须删除 socket 文件");
    }

    /// 无连接期 send_* 排队不报错（断线排队语义）；队列超容才 Err。
    #[tokio::test]
    async fn outbound_frames_queue_without_connection() {
        let p = LocalPlatform::new(tmp_socket("queue")).expect("bind");
        for i in 0..OUTBOUND_CHANNEL_CAPACITY {
            p.send_text(
                &ConvId("local:work".into()),
                &format!("t{i}"),
                &ReplyHint::None,
            )
            .await
            .expect("无连接期帧应排队");
        }
        // 超容即 Err（镜像 web §5.1：终端长时间不接入）。
        let e = p
            .send_text(&ConvId("local:work".into()), "overflow", &ReplyHint::None)
            .await
            .expect_err("队列满应 Err");
        assert!(format!("{e}").contains("队列满"), "{e}");
        drop(p);
    }

    /// 审批闭环帧在无连接期同样入队成功；cancel_all 幂等（空 conv 不 Err）。
    #[tokio::test]
    async fn ask_frames_lifecycle_without_connection() {
        let p = LocalPlatform::new(tmp_socket("ask")).expect("bind");
        let conv = ConvId("local:work".into());
        let anchor = p
            .send_permission_ask(&conv, "req-1", "Bash", "rm -rf /tmp/x", &ReplyHint::None)
            .await
            .expect("ask 入队");
        assert_eq!(anchor.as_deref(), Some("req-1"), "锚点 = request_id");
        p.resolve_permission_ask(
            &conv,
            "req-1",
            &PermissionReply {
                allow: true,
                always: false,
                message: None,
                raw_text: None,
                cancelled: false,
            },
        )
        .await
        .expect("resolve 入队");
        // 已 resolve（untrack）→ cancel_all 无残留也不 Err。
        p.cancel_all_permission_asks(&conv).await.expect("幂等");
        drop(p);
    }

    /// send_media 覆写为路径文本（不声明 MEDIA_UPLOAD、不报错）。
    #[tokio::test]
    async fn send_media_prints_local_path() {
        let p = LocalPlatform::new(tmp_socket("media")).expect("bind");
        p.send_media(
            &ConvId("local:work".into()),
            &MediaRef {
                kind: "image".into(),
                url: "/tmp/a.png".into(),
            },
            &ReplyHint::None,
        )
        .await
        .expect("路径文本应入队");
        drop(p);
    }

    /// 文本缓存：行数上限丢最旧；conv 淘汰最久未活跃；快照 join 保序。
    #[test]
    fn text_cache_caps_and_snapshot_order() {
        let mut c = TextCache::default();
        for i in 0..(TEXT_CACHE_MAX_LINES + 5) {
            c.push("local:a", format!("a{i}"));
        }
        assert_eq!(c.len("local:a"), TEXT_CACHE_MAX_LINES, "行数封顶");
        let snap = c.snapshot_joined("local:a").unwrap();
        assert!(snap.starts_with("a5\n"), "最旧的 a0..a4 被淘汰：{snap}");
        assert!(snap.ends_with(&format!("a{}", TEXT_CACHE_MAX_LINES + 4)));
        // 覆盖上限外的 conv：最久未活跃者淘汰（先 a 后 b，a 被淘汰）。
        for i in 0..=TEXT_CACHE_MAX_CONVS {
            c.push(&format!("local:c{i}"), format!("x{i}"));
        }
        assert!(
            c.snapshot_joined("local:a").is_none(),
            "最早插入的 a 被淘汰"
        );
        assert!(c
            .snapshot_joined(&format!("local:c{TEXT_CACHE_MAX_CONVS}"))
            .is_some());
        // 空/未知 conv：None。
        assert!(c.snapshot_joined("local:none").is_none());
    }

    /// peer uid：socketpair 两端同进程 → 返回本进程 uid（core dispatch 同款测试）。
    #[tokio::test]
    async fn peer_uid_returns_self_for_local_pair() {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        let got = peer_uid(&a).expect("本地连接应返回 uid");
        assert_eq!(got, current_uid());
    }
}
