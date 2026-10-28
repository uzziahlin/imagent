//! [`WebPlatform`]：实现 [`imagent_core::Platform`]，接入 Dispatcher 全管线。
//!
//! 组织方式参照 wecom（client 任务 + 双 mpsc + 平台薄壳），但 web 无 proto
//! 加密、无 outbox、无 IM 协议——出站即 JSON 帧：
//! - `recv()`：inbound channel 前端（client 已把浏览器帧映射成
//!   [`InboundMessage`]）；断线期间无消息天然 pending（与 IM 平台断线一致）。
//! - `send_text` / `send_card` / `update_card` / `send_permission_ask` 族 /
//!   `reconnect` / `doctor_probes`：覆写为对应帧（见 lib.rs 帧药化函数），
//!   经出站 channel（容量 256）交给 client。断线期帧排队（client 重连后按序
//!   发出）；队列满（relay 长时间不可达）→ Err 上抛。
//! - sync 重发（D6 故障语义）：per-conv 缓存最近卡片（send_card / update_card
//!   更新，上限 32 conv 淘汰最旧），浏览器发 `sync` 时由独立 responder 任务
//!   全量重发 card 帧。relay 保证单浏览器连接，无定向问题。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use imagent_core::{
    ConvId, CoreError, InboundMessage, MediaRef, OutboundCard, PermissionReply, Platform,
    PlatformCaps, ReplyHint, Result,
};

use crate::client::WebClient;
use crate::{
    encode_approval_request_frame, encode_approval_resolved_frame, encode_card_frame,
    encode_text_frame, CardPayload, TARGET,
};

const PLATFORM: &str = "web";

/// 出站 channel 容量（§5.1）：断线期 send_* 帧在此排队（client 重连后按序
/// 发出）；打满（relay 长时间不可达）→ send_* 返回 Err。
const OUTBOUND_CHANNEL_CAPACITY: usize = 256;
/// 入站 channel 容量（wecom 先例：dispatch 消费短暂抖动不丢消息）。
const INBOUND_CHANNEL_CAPACITY: usize = 64;
/// sync 请求 channel 容量。
const SYNC_CHANNEL_CAPACITY: usize = 8;
/// 卡片缓存 conv 上限（§5.4）：超限淘汰最旧（seq 最小），防长寿命进程内存
/// 无界增长。
const CARD_CACHE_MAX_CONVS: usize = 32;

// ---------------------------------------------------------------------------
// 卡片缓存（D6 sync 重发）
// ---------------------------------------------------------------------------

/// per-conv 最近卡片缓存：`conv -> (seq, message_id, card)`。seq 单调递增，
/// 兼作淘汰序（最小者最旧）与快照序（旧→新重发）。纯内存临界区用 std
/// Mutex（锁内计算 → clone 出锁，relay Hub 同款纪律）。
#[derive(Default)]
struct CardCache {
    entries: HashMap<String, (u64, String, CardPayload)>,
    next_seq: u64,
}

impl CardCache {
    fn upsert(&mut self, conv: &str, message_id: &str, card: CardPayload) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries
            .insert(conv.to_string(), (seq, message_id.to_string(), card));
        if self.entries.len() > CARD_CACHE_MAX_CONVS {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (s, _, _))| *s)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest);
            }
        }
    }

    /// 全量快照（按 seq 旧→新排序，重发顺序与卡片创建顺序一致）。
    fn snapshot(&self) -> Vec<(String, String, CardPayload)> {
        let mut v: Vec<(u64, String, String, CardPayload)> = self
            .entries
            .iter()
            .map(|(k, (s, mid, c))| (*s, k.clone(), mid.clone(), c.clone()))
            .collect();
        v.sort_by_key(|(s, _, _, _)| *s);
        v.into_iter().map(|(_, k, mid, c)| (k, mid, c)).collect()
    }
}

/// pending 审批记录（cancel_all 逐条收敛用）：`conv -> request_id 列表`。
type PendingAsks = HashMap<String, Vec<String>>;

/// 中毒（临界区 panic）时取回数据继续服务：纯内存临界区无 IO 副作用可回滚
/// （relay Hub / webhook 同款取舍）。
fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// sync responder：消费 client 转来的 sync 请求，把卡片缓存全量重发（card
/// 帧，state 依 terminal，v1.1 起带本机 machine 字段——与首发帧同形）。
/// best-effort——出站队列满即丢弃该轮重发并 warn（浏览器可再发 sync 刷新，
/// 不值得反压阻塞 client 的 sync channel）。
async fn serve_sync_requests(
    mut sync_rx: mpsc::Receiver<()>,
    machine_id: String,
    cards: Arc<Mutex<CardCache>>,
    outbound_tx: mpsc::Sender<String>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            req = sync_rx.recv() => {
                if req.is_none() {
                    break; // client 退出（platform drop 连动），无后续 sync
                }
                let snapshot = lock_or_recover(&cards).snapshot();
                debug!(target: TARGET, convs = snapshot.len(), "sync：重发缓存卡片");
                for (conv, message_id, card) in snapshot {
                    match encode_card_frame(&machine_id, "card", &conv, &message_id, &card) {
                        Ok(json) => {
                            if outbound_tx.try_send(json).is_err() {
                                warn!(target: TARGET, conv = %conv, "sync 重发出站队列满，丢弃该轮重发");
                                break;
                            }
                        }
                        Err(e) => warn!(target: TARGET, error = %e, conv = %conv, "sync 重发帧序列化失败"),
                    }
                }
            }
        }
    }
    debug!(target: TARGET, "sync responder 退出");
}

// ---------------------------------------------------------------------------
// WebPlatform
// ---------------------------------------------------------------------------

/// Web 平台适配器（网关侧薄壳：两条 channel + 后台 client / sync responder）。
pub struct WebPlatform {
    /// relay 地址（doctor_probes 展示用）。
    relay_url: String,
    /// 本机 machine 身份（v1.1 多终端管理）：hello 自报 + 全部出站帧携带。
    machine_id: String,
    /// 出站帧通道（发给 client → relay → 浏览器）。
    outbound_tx: mpsc::Sender<String>,
    /// `/reconnect` 强制重连信号（与 client run task 共享）。
    reconnect: Arc<Notify>,
    /// 入站消息 channel 前端（`recv` 直接 await；AsyncMutex 跨 await 持锁）。
    inbound_rx: Arc<AsyncMutex<mpsc::Receiver<InboundMessage>>>,
    /// 在线标志（client 维护）——doctor_probes / 测试用。
    online: Arc<AtomicBool>,
    /// per-conv 最近卡片缓存（sync 重发）。
    cards: Arc<Mutex<CardCache>>,
    /// 合成卡片 id 计数器：`web-card-<n>` 单调递增（§4.2）。
    card_seq: Arc<AtomicU64>,
    /// pending 审批记录（cancel_all 收敛用）。
    pending_asks: Arc<Mutex<PendingAsks>>,
    /// 停机信号（Drop 时 cancel，client / sync responder 连动退出）。
    shutdown: CancellationToken,
}

impl WebPlatform {
    /// 构造并后台 spawn client 任务（建连/认证/心跳/重连/收发）与 sync
    /// responder 任务。`relay_url` 形如 `wss://…/imagent/gateway`；
    /// `gateway_token` 走 env `IMAGENT_WEB_GATEWAY_TOKEN`（main 装配）；
    /// `machine_id` 为本机 machine 身份（v1.1，main 经
    /// [`crate::resolve_machine_id`] 解析：config 显式值 / hostname 短名）；
    /// `default_session` 为无 convId 浏览器消息的落点（缺省 "default"）。
    pub fn new(
        relay_url: String,
        gateway_token: String,
        default_session: String,
        machine_id: String,
    ) -> Self {
        let (inbound_tx, inbound_rx) = mpsc::channel::<InboundMessage>(INBOUND_CHANNEL_CAPACITY);
        let (outbound_tx, outbound_rx) = mpsc::channel::<String>(OUTBOUND_CHANNEL_CAPACITY);
        let (sync_tx, sync_rx) = mpsc::channel::<()>(SYNC_CHANNEL_CAPACITY);

        let reconnect = Arc::new(Notify::new());
        let online = Arc::new(AtomicBool::new(false));
        let shutdown = CancellationToken::new();

        // client 任务：连接立即开始（断线自动重连）。
        let client = WebClient {
            relay_url: relay_url.clone(),
            gateway_token,
            machine_id: machine_id.clone(),
            default_session,
            online: online.clone(),
            inbound_tx,
            sync_tx,
        };
        tokio::spawn(client.run(outbound_rx, reconnect.clone(), shutdown.clone()));

        // sync responder（D6 会话恢复）。
        let cards: Arc<Mutex<CardCache>> = Arc::new(Mutex::new(CardCache::default()));
        tokio::spawn(serve_sync_requests(
            sync_rx,
            machine_id.clone(),
            cards.clone(),
            outbound_tx.clone(),
            shutdown.clone(),
        ));

        Self {
            relay_url,
            machine_id,
            outbound_tx,
            reconnect,
            inbound_rx: Arc::new(AsyncMutex::new(inbound_rx)),
            online,
            cards,
            card_seq: Arc::new(AtomicU64::new(0)),
            pending_asks: Arc::new(Mutex::new(PendingAsks::default())),
            shutdown,
        }
    }

    /// relay 连接是否在线（doctor_probes / 测试用）。
    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// 本机 machine 身份（测试用）。
    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    /// 出站帧入队（§5.1 断线语义）：try_send——断线期帧排队（Ok），队列满 /
    /// client 退出才 Err，不挂起 dispatch。
    fn enqueue(&self, json: String) -> Result<()> {
        match self.outbound_tx.try_send(json) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(CoreError::Platform(
                PLATFORM,
                "出站队列满：relay 长时间不可达".into(),
            )),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(CoreError::Platform(
                PLATFORM,
                "出站通道已关闭（client 已退出）".into(),
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

/// 平台 drop 即 cancel：后台 client / sync responder 任务感知退出，不再无限
/// 重连（wecom P3-g 同款停机通路——测试隔离与多 profile 热替换不泄漏旧连接
/// 循环）。
impl Drop for WebPlatform {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[async_trait]
impl Platform for WebPlatform {
    /// B1 能力声明：CARDS | ASK | RECONNECT，与覆写清单逐族对照：
    /// - CARDS：send_card（card 帧 + 缓存终态供 sync 重发）/ update_card
    ///   （card_patch 帧 + 同步更新缓存）✅。（supports_streaming_card 恒 true
    ///   是 per-conv 细化方法，非能力位——core CardSession 据它分支建会话。）
    /// - ASK：send_permission_ask（approval_request）/ resolve_permission_ask /
    ///   cancel_permission_ask / cancel_all_permission_asks（approval_resolved）
    ///   ✅。（note_queued_on_ask / send_permission_ask_text 不覆写，走 default
    ///   ——web 卡无 note 行需求 / 纯文本降级路径无需平台实现。）
    /// - RECONNECT：reconnect（Notify 唤醒 client task 重连）✅。
    /// - ❌ 不声明（走 default 降级）：COMMAND_CARDS / FORMS（纯文本降级对
    ///   本平台已够——SPA 的按钮在浏览器侧由 approval/command 帧渲染）、
    ///   REACTIONS / TYPING / URGENT_TEXT / GROUP_CHATS（无对应语义）；
    ///   MEDIA_UPLOAD：send_media 覆写但显式 Err（MVP 无媒体上行）——声明
    ///   必须与实际能力一致（wecom send_media 同款处理），不声明。
    /// 一致性测试 `web_caps_match_overrides` 钉住本值。
    fn capabilities(&self) -> PlatformCaps {
        PlatformCaps::CARDS | PlatformCaps::ASK | PlatformCaps::RECONNECT
    }

    fn name(&self) -> &'static str {
        PLATFORM
    }

    async fn recv(&self) -> Result<InboundMessage> {
        self.inbound_rx.lock().await.recv().await.ok_or_else(|| {
            CoreError::Platform(PLATFORM, "入站 channel 已关闭（client 已退出）".into())
        })
    }

    async fn send_text(&self, conv: &ConvId, text: &str, _hint: &ReplyHint) -> Result<()> {
        let json = encode_text_frame(&self.machine_id, &conv.0, text)?;
        self.enqueue(json)
    }

    async fn send_media(&self, _conv: &ConvId, _media: &MediaRef, _hint: &ReplyHint) -> Result<()> {
        // MVP 无媒体上行通道（relay 帧协议 v1 只有文本帧）。显式报错而非谎报
        // Ok——core 命令层（/img /file）会把文案回给用户。
        Err(CoreError::Platform(
            PLATFORM,
            "web 暂不支持媒体发送（M2 无媒体上行）".into(),
        ))
    }

    /// web 无 per-conv 差异：SPA 恒走流式卡片路径。
    fn supports_streaming_card(&self, _conv: &ConvId) -> bool {
        true
    }

    async fn send_card(
        &self,
        conv: &ConvId,
        card: &OutboundCard,
        _hint: &ReplyHint,
    ) -> Result<Option<String>> {
        let message_id = format!(
            "web-card-{}",
            self.card_seq.fetch_add(1, Ordering::Relaxed) + 1
        );
        let payload = CardPayload::from_card(card);
        let json = encode_card_frame(&self.machine_id, "card", &conv.0, &message_id, &payload)?;
        lock_or_recover(&self.cards).upsert(&conv.0, &message_id, payload);
        self.enqueue(json)?;
        Ok(Some(message_id))
    }

    async fn update_card(
        &self,
        conv: &ConvId,
        message_id: &str,
        card: &OutboundCard,
        _hint: &ReplyHint,
    ) -> Result<()> {
        let payload = CardPayload::from_card(card);
        let json = encode_card_frame(
            &self.machine_id,
            "card_patch",
            &conv.0,
            message_id,
            &payload,
        )?;
        // 缓存语义是「该 conv 最近卡片」，patch 后直接覆盖（message_id 不匹配
        // 的陈旧 patch 也覆盖为最新——终态以最后一次为准）。
        lock_or_recover(&self.cards).upsert(&conv.0, message_id, payload);
        self.enqueue(json)
    }

    async fn send_permission_ask(
        &self,
        conv: &ConvId,
        request_id: &str,
        tool_name: &str,
        input_summary: &str,
        _hint: &ReplyHint,
    ) -> Result<Option<String>> {
        // inputSummary 压成人可读单行（P8-1，与 feishu 审批卡同源）。
        let summary = imagent_core::render::tool_summary(tool_name, input_summary);
        let json = encode_approval_request_frame(
            &self.machine_id,
            &conv.0,
            request_id,
            tool_name,
            &summary,
        )?;
        self.track_ask(&conv.0, request_id);
        self.enqueue(json)?;
        // 返回 request_id 作 card_msg_id 锚点：浏览器审批回复带 requestId
        // （ask_req）精确路由，SPA 无「引用回复」形态，锚点即请求 id 本身。
        Ok(Some(request_id.to_string()))
    }

    async fn resolve_permission_ask(
        &self,
        conv: &ConvId,
        request_id: &str,
        reply: &PermissionReply,
    ) -> Result<()> {
        let decision = if reply.always {
            "always"
        } else if reply.allow {
            "allow"
        } else {
            "deny"
        };
        let json = encode_approval_resolved_frame(
            &self.machine_id,
            &conv.0,
            request_id,
            decision,
            reply.message.as_deref(),
        )?;
        self.untrack_ask(&conv.0, request_id);
        self.enqueue(json)
    }

    async fn cancel_permission_ask(&self, conv: &ConvId, request_id: &str) -> Result<()> {
        let json = encode_approval_resolved_frame(
            &self.machine_id,
            &conv.0,
            request_id,
            "cancelled",
            Some("审批已取消（超时或任务被中断）"),
        )?;
        self.untrack_ask(&conv.0, request_id);
        self.enqueue(json)
    }

    async fn cancel_all_permission_asks(&self, conv: &ConvId) -> Result<()> {
        // 该 conv 记录中的全部 pending 逐条发 approval_resolved(cancelled)。
        let drained = lock_or_recover(&self.pending_asks)
            .remove(&conv.0)
            .unwrap_or_default();
        for request_id in drained {
            let json = encode_approval_resolved_frame(
                &self.machine_id,
                &conv.0,
                &request_id,
                "cancelled",
                Some("审批已取消（任务被 /stop 中断）"),
            )?;
            self.enqueue(json)?;
        }
        Ok(())
    }

    /// 强制重连：notify_one 存 permit，client run task 的 select 消费后丢弃
    /// 连接 future 断开重连（退避重置为 1s）。
    async fn reconnect(&self) -> Result<()> {
        self.reconnect.notify_one();
        Ok(())
    }

    async fn doctor_probes(&self) -> Vec<String> {
        let state = if self.is_online() {
            "在线"
        } else {
            "重连中"
        };
        vec![format!(
            "✅ relay 连接：{state}（{}，machine={}）",
            self.relay_url, self.machine_id
        )]
    }
}

#[cfg(test)]
mod tests {
    //! 逻辑测试不连真实 relay（地址用 loopback 拒连端口，后台任务退避空转，
    //! wecom 同款手法）；端到端行为见 tests/web_loopback.rs。

    use super::*;
    use imagent_core::{CardPhase, CardTerminal};

    fn offline_platform() -> WebPlatform {
        WebPlatform::new(
            "ws://127.0.0.1:1/gateway".into(),
            "unused-token".into(),
            "default".into(),
            "test-mac".into(),
        )
    }

    /// Drop 平台必须 cancel 停机信号（后台任务连动退出）。
    #[tokio::test]
    async fn drop_platform_cancels_shutdown_token() {
        let p = offline_platform();
        let token = p.shutdown.clone();
        assert!(!token.is_cancelled());
        drop(p);
        assert!(token.is_cancelled(), "Drop 必须 cancel 停机信号");
    }

    /// B1：caps 声明与覆写清单一致性（钉值防漂移，对照 capabilities() 旁注释）。
    #[tokio::test]
    async fn web_caps_match_overrides() {
        let p = offline_platform();
        assert_eq!(
            p.capabilities(),
            PlatformCaps::CARDS | PlatformCaps::ASK | PlatformCaps::RECONNECT,
            "web 声明 cards+ask+reconnect"
        );
        assert_eq!(p.name(), "web");
        assert!(p.supports_streaming_card(&ConvId("web:x".into())));
        // send_media 覆写为显式 Err → MEDIA_UPLOAD 不声明（覆写≠支持）。
        assert!(!p.capabilities().contains(PlatformCaps::MEDIA_UPLOAD));
        drop(p);
    }

    /// doctor_probes：离线（初始未连接）显示重连中；在线标志翻转后显示在线。
    #[tokio::test]
    async fn doctor_probes_reflect_online_state() {
        let p = offline_platform();
        let probes = p.doctor_probes().await;
        assert_eq!(probes.len(), 1);
        assert!(
            probes[0].contains("重连中") && probes[0].contains("ws://127.0.0.1:1/gateway"),
            "{}",
            probes[0]
        );
        // 直接翻 client 维护的标志（在线翻转路径由回环测试覆盖真连接）。
        p.online.store(true, Ordering::Relaxed);
        let probes = p.doctor_probes().await;
        assert!(probes[0].contains("在线"), "{}", probes[0]);
        drop(p);
    }

    /// send_media 显式 Err（不谎报 Ok）。
    #[tokio::test]
    async fn send_media_errors_explicitly() {
        let p = offline_platform();
        let e = p
            .send_media(
                &ConvId("web:x".into()),
                &MediaRef {
                    kind: "image".into(),
                    url: "/tmp/a.png".into(),
                },
                &ReplyHint::None,
            )
            .await
            .unwrap_err();
        assert!(format!("{e}").contains("媒体"), "{e}");
        drop(p);
    }

    /// 卡片缓存：上限 32 conv 淘汰最旧；快照按 seq 旧→新排序。
    #[test]
    fn card_cache_evicts_oldest_and_snapshots_in_order() {
        let mut cache = CardCache::default();
        let card = || {
            CardPayload::from_card(&OutboundCard {
                text: "t".into(),
                tool_calls: vec![],
                thoughts: vec![],
                todos: vec![],
                phase: CardPhase::Thinking,
                queued_hint: None,
                run_secs: 0,
                usage_display: None,
                task_digest: None,
                terminal: CardTerminal::Running,
            })
        };
        for i in 0..=CARD_CACHE_MAX_CONVS {
            cache.upsert(&format!("web:c{i}"), &format!("web-card-{i}"), card());
        }
        assert_eq!(cache.entries.len(), CARD_CACHE_MAX_CONVS, "超限淘汰");
        assert!(!cache.entries.contains_key("web:c0"), "最旧（c0）被淘汰");
        let snapshot = cache.snapshot();
        let convs: Vec<&str> = snapshot.iter().map(|(c, _, _)| c.as_str()).collect();
        assert_eq!(convs.len(), CARD_CACHE_MAX_CONVS);
        // seq 升序：c1 在 c2 前。
        assert!(
            convs.iter().position(|c| *c == "web:c1").unwrap()
                < convs.iter().position(|c| *c == "web:c2").unwrap()
        );
        // 同 conv 二次 upsert 覆盖（不涨条目）。
        cache.upsert("web:c1", "web-card-again", card());
        assert_eq!(cache.entries.len(), CARD_CACHE_MAX_CONVS);
        assert_eq!(cache.entries.get("web:c1").unwrap().1, "web-card-again");
    }
}
