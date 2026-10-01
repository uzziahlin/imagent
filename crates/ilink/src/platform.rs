//! `impl imagent_core::Platform` for iLink。
//!
//! - `recv()`：`pending` 缓存上次长轮询多条消息，逐条返回；空则长轮询
//!   `getupdates`（带游标），失败指数退避重连，SESSION_EXPIRED → Err。
//! - `send_text()`：context_token 优先取 `ReplyHint`，否则读 store 该 peer 最新；
//!   POST `sendmessage`；响应若带新 token 则更新。
//! - 媒体/typing：P1 空实现（core 不调媒体）。
//!
//! 鉴权由 core 做：本层只透传 `from_user_id`，不做白名单（DESIGN §9 硬约束①）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::future::join_all;
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::json;
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, error, warn};

use imagent_core::{
    ConvId, CoreError, InboundMessage, MediaRef, Platform, PlatformCaps, ReplyHint, Result,
};
use imagent_store::Store;

use crate::client::ILinkClient;
use crate::dedup::Dedup;
use crate::proto::{
    classify_send, extract_media_refs, extract_text, msg_to_inbound, GetConfigResp, Msg,
    RawMediaRef, SendMsgResp, SendOutcome, UpdatesResp,
};

const PLATFORM: &str = "ilink";
const ILINK_PREFIX: &str = "ilink:";
/// 退避上限：到达后停止重试、上报 Err（由 core 决定继续/暂停）。
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// typing_ticket 缓存 TTL（协议侧 600s，留 100s 余量提前刷新）。
const TYPING_TICKET_TTL: Duration = Duration::from_secs(500);
/// P2-2（code-review v14）：批内媒体下载并发上限（信号量）。此前逐条消息、
/// 逐个媒体顺序 await，单文件最长 45s（HTTP 超时）× 批内媒体数串行累加，阻塞
/// **所有**会话的入站投递。4 并发对 CDN 是礼貌值（再高易触发风控/带宽挤占）。
const MEDIA_DOWNLOAD_CONCURRENCY: usize = 4;
/// P2-2（code-review v14）：单条消息媒体处理总预算（含信号量排队与下载）。
/// 超预算即放弃该消息剩余未完成媒体项（warn 标记失败），文本照常投递——
/// 媒体是增强信息，不能让它无限期拖住整批入站。60s ≈ 允许一轮完整重试
/// （单下载上限 45s）但为批内其他消息留出余量。
const MEDIA_TOTAL_BUDGET: Duration = Duration::from_secs(60);

pub struct ILinkPlatform {
    client: Arc<ILinkClient>,
    store: Store,
    account_id: String,
    dedup: Dedup,
    /// 上次长轮询批量取到的消息，`recv` 逐条弹出。
    pending: Mutex<Vec<InboundMessage>>,
    /// 出站串行：同一 bot 同一时刻只有一条 sendmessage 在飞。
    /// P3-c（code-review v14）：锁只覆盖「读熔断状态 + 发包 + 即时分类」的
    /// 临界区；重试的冷却/退避 sleep 在锁外（见 [`ILinkPlatform::send_with_retry`]），
    /// peer A 重试等待（最坏 ~1min）不再卡死 peer B 的发送。
    send_lock: Mutex<()>,
    /// 被动限流熔断器。
    breaker: crate::ratelimit::RateBreaker,
    /// per-peer typing_ticket 缓存：peer → (ticket, expiry)。
    typing_tickets: Mutex<HashMap<String, (String, Instant)>>,
    /// 出站文本单条字符上限（Unicode char）。None = 不分片。
    max_text_len: Option<usize>,
    /// 分片间发送间隔。
    fragment_interval: Duration,
}

impl ILinkPlatform {
    pub fn new(
        client: ILinkClient,
        store: Store,
        account_id: String,
        max_text_len: Option<usize>,
        fragment_interval: Duration,
    ) -> Self {
        Self {
            client: Arc::new(client),
            store,
            account_id,
            dedup: Dedup::default(),
            send_lock: Mutex::new(()),
            breaker: crate::ratelimit::RateBreaker::new(
                Duration::from_secs(30),
                3, // P2-T：窗口内 3 次限流才熔断（threshold=1 单次即熔断，过于敏感）
                Duration::from_secs(30),
            ),
            typing_tickets: Mutex::new(HashMap::new()),
            pending: Mutex::new(Vec::new()),
            max_text_len,
            fragment_interval,
        }
    }

    /// 从 `ConvId` 提取 peer（去掉 `"ilink:"` 前缀；无前缀则原样返回）。
    fn peer_of(conv: &ConvId) -> String {
        conv.0
            .strip_prefix(ILINK_PREFIX)
            .unwrap_or(&conv.0)
            .to_string()
    }

    /// 长轮询取消息：读游标 → POST → 处理消息（去重 + 更新 peer token + 媒体）
    /// → 前进游标。游标在消息全部处理完后才更新：处理中 crash → 游标未动 →
    /// 下次重拉同批，重复消息由 dedup 吸收（at-least-once，优于丢消息）。
    async fn fetch_updates(&self) -> Result<Vec<InboundMessage>> {
        // v1.23 review：游标**读取**失败此前被静默吞掉（unwrap_or(None) → 带空
        // 游标请求，服务端可能按重置语义大批重放；DB 故障跨 dedup 窗后恢复
        // 甚至重复驱动 agent）。读失败即报错——让 recv 的退避重试接管，宁可
        // 暂停拉取也不带空游标请求（与写侧 set_sync_buf 的重试+告警对称）。
        let buf = match self.store.get_sync_buf(PLATFORM, &self.account_id).await {
            Ok(b) => b,
            Err(e) => {
                use tracing::error;
                error!(target: "ilink", error = %e, "同步游标读取失败，暂停本轮拉取（不带空游标请求）");
                return Err(e.into());
            }
        };
        let body = json!({ "get_updates_buf": buf.unwrap_or_default() });
        let resp: UpdatesResp = self
            .client
            .post_json("/ilink/bot/getupdates", &body)
            .await?;

        // 先处理所有消息（含媒体下载），再前进游标：crash 在处理中 → 游标未动 →
        // 下次重拉同批，重复消息由 dedup 吸收（at-least-once，优于丢消息）。
        //
        // P2-2（code-review v14）：媒体下载从「逐条消息逐个媒体顺序 await」改为
        // ①各消息廉价阶段（去重/token/构造 inbound）保持顺序串行；②媒体阶段批内
        // 并发（futures + 批级 Semaphore(4) 限流）且单条消息有 60s 总预算上限。
        // 此前单文件最长 45s × 批内媒体数串行累加，阻塞**所有**会话的入站。
        // 注：「文本先投递、媒体后补」的完全解耦是结构性改动（需把媒体阶段挪出
        // recv 关键路径并定义媒体后到时的消费端语义），本轮不做——本批消息仍
        // 在批处理完成后一并投递，只是总时长被并发与预算限界。
        let mut prepared: Vec<(InboundMessage, Vec<RawMediaRef>)> =
            Vec::with_capacity(resp.msgs.len());
        for msg in &resp.msgs {
            if let Some(p) = self.prepare_msg(msg).await {
                prepared.push(p);
            }
        }

        // 媒体阶段：批级信号量限流下的并发下载（每条消息独立预算兜底）。
        let semaphore = Arc::new(Semaphore::new(MEDIA_DOWNLOAD_CONCURRENCY));
        join_all(
            prepared
                .iter_mut()
                .map(|(ib, refs)| self.process_media_phase(ib, std::mem::take(refs), &semaphore)),
        )
        .await;

        // 投递顺序与批次内消息顺序一致（媒体阶段不改变排列）。
        let out: Vec<InboundMessage> = prepared.into_iter().map(|(ib, _)| ib).collect();

        if let Some(new_buf) = resp.get_updates_buf.as_deref() {
            if !new_buf.is_empty() {
                // P5-13 修正：游标推进失败原地重试数次；仍失败则**照常投递本批消息**
                // 并 error 告警。此前直接 return Err 丢弃本批——但 process_msg 已把
                // dedup 键插进去，重拉同批会被去重吸收，等于**静默丢消息**（比原来的
                // 「5min 后可能重复」更糟）。取舍：瞬态 DB 故障宁可冒重复驱动一轮的
                // 风险（dedup 5min 窗口内多数能吸收），不可丢用户消息。
                let mut advanced = false;
                for attempt in 1..=3 {
                    match self
                        .store
                        .set_sync_buf(PLATFORM, &self.account_id, new_buf)
                        .await
                    {
                        Ok(()) => {
                            advanced = true;
                            break;
                        }
                        Err(e) => {
                            warn!(
                                target: "ilink",
                                attempt,
                                error = %e,
                                "set_sync_buf 失败（游标未推进）"
                            );
                            tokio::time::sleep(Duration::from_millis(300)).await;
                        }
                    }
                }
                if !advanced {
                    tracing::error!(
                        target: "ilink",
                        "游标推进连续失败（3 次）——本批消息照常投递，但服务端会重推同批；\
                         若 DB 持续故障超过 dedup 窗口（5min），可能出现重复执行"
                    );
                }
            }
        }
        Ok(out)
    }

    /// P2-2（code-review v14）：`process_msg` 拆分的廉价阶段——去重 + 更新该
    /// peer 最新 context_token + 构造 InboundMessage（媒体引用做 owned 快照，
    /// 并发阶段不再借用原始 Msg）。返回 `None` = 去重丢弃。
    async fn prepare_msg(&self, msg: &Msg) -> Option<(InboundMessage, Vec<RawMediaRef>)> {
        let key = dedup_key(msg);
        if !self.dedup.check(&key) {
            return None;
        }
        // 更新该 peer 最新 context_token（发消息回传）。
        if let Some(token) = msg.context_token.as_deref() {
            if !token.is_empty() {
                if let Err(e) = self
                    .store
                    .set_context_token(PLATFORM, &self.account_id, &msg.from_user_id, token)
                    .await
                {
                    warn!(
                        target: "ilink",
                        peer = %msg.from_user_id,
                        error = %e,
                        "set_context_token 失败（best-effort）"
                    );
                }
            }
        }

        let ib = msg_to_inbound(msg);
        let refs = extract_media_refs(msg);
        Some((ib, refs))
    }

    /// P2-2（code-review v14）：单条消息的媒体阶段——下载 CDN（AES 解密）+
    /// 落盘。并发与总预算见 [`drain_media_with_budget`]；信号量在批级共享
    /// （fetch_updates 创建），单条多图消息不会独占全部并发槽。
    async fn process_media_phase(
        &self,
        ib: &mut InboundMessage,
        refs: Vec<RawMediaRef>,
        semaphore: &Arc<Semaphore>,
    ) {
        if refs.is_empty() {
            return; // 纯文本消息零开销直过。
        }
        let total = refs.len();
        let futs: FuturesUnordered<_> = refs
            .into_iter()
            .enumerate()
            .map(|(idx, raw)| {
                let permit = semaphore.clone().acquire_owned();
                async move {
                    // 信号量在 future 内获取：批内所有消息的所有媒体统一排队，
                    // 并发上限全局生效。
                    let _permit = permit.await.expect("媒体下载信号量随批存活，不会 close");
                    // 阶段 A：下载入站媒体（图片/文件/视频），存 ~/.imagent/media/。
                    // 单个失败仅 log，不丢整条消息（文本仍可用）。
                    let r: std::result::Result<MediaRef, String> = crate::media::download_media(
                        self.client.http(),
                        raw.encrypt_query_param.as_deref(),
                        raw.aes_key.as_deref(),
                        raw.full_url.as_deref(),
                    )
                    .await
                    .map_err(|e| format!("download media 失败: {e}"))
                    .and_then(|bytes| {
                        persist_media(raw.kind, raw.file_name.as_deref(), &bytes)
                            .map_err(|e| format!("persist media 失败: {e}"))
                    })
                    .map(|path| MediaRef {
                        kind: raw.kind.to_string(),
                        url: path,
                    });
                    (idx, raw.kind, r)
                }
            })
            .collect();
        drain_media_with_budget(&mut ib.media, futs, MEDIA_TOTAL_BUDGET, total).await;
    }

    /// 解析发送阶段 context_token：优先 hint，否则读 store。
    async fn resolve_context_token(&self, peer: &str, hint: &ReplyHint) -> String {
        match hint {
            ReplyHint::ContextToken { context_token } if !context_token.is_empty() => {
                context_token.clone()
            }
            _ => self
                .store
                .get_context_token(PLATFORM, &self.account_id, peer)
                .await
                .unwrap_or(None)
                .unwrap_or_default(),
        }
    }

    /// 取（或刷新）该 peer 的 typing_ticket。缓存命中且未过期则直接返回；
    /// 否则 POST getconfig 刷新。失败返回 None（尽力而为，不阻断主流程）。
    async fn ensure_typing_ticket(&self, peer: &str, hint: &ReplyHint) -> Option<String> {
        // 1. 缓存命中？
        {
            let cache = self.typing_tickets.lock().await;
            if let Some((t, exp)) = cache.get(peer) {
                if ticket_valid(t, *exp, Instant::now()) {
                    return Some(t.clone());
                }
            }
        }
        // 2. 过期/无 → getconfig 刷新。
        let ctx = self.resolve_context_token(peer, hint).await;
        let mut body = json!({ "ilink_user_id": peer });
        if !ctx.is_empty() {
            body["context_token"] = json!(ctx);
        }
        match self
            .client
            .post_json::<GetConfigResp>("/ilink/bot/getconfig", &body)
            .await
        {
            Ok(resp) => {
                let ticket = match resp.typing_ticket.as_deref() {
                    Some(t) if !t.is_empty() => t.to_string(),
                    _ => {
                        warn!(target: "ilink", peer, "getconfig 无 typing_ticket");
                        return None;
                    }
                };
                // v1.23 review：插入时顺带清理过期条目 + 粗上限（此前只判失效
                // 不删除，peer 无限增长）。
                {
                    let mut tickets = self.typing_tickets.lock().await;
                    let now = Instant::now();
                    tickets.retain(|_, (_, at)| now.duration_since(*at).as_secs() < 600);
                    if tickets.len() >= 1024 {
                        tickets.clear();
                    }
                }
                self.typing_tickets.lock().await.insert(
                    peer.to_string(),
                    (ticket.clone(), Instant::now() + TYPING_TICKET_TTL),
                );
                Some(ticket)
            }
            Err(e) => {
                warn!(target: "ilink", peer, error = %e, "getconfig 失败");
                None
            }
        }
    }

    /// 出站媒体发送（阶段 B）：读本地文件 → AES 加密 → getuploadurl → CDN POST →
    /// sendmessage 媒体 item。
    ///
    /// hermes 协议事实：
    /// - getuploadurl 的 `media_type`：1=img / 2=video / 3=file / 4=voice。
    /// - 出站 `aes_key` 字段 = `base64(hex_string)`（非对称编码）。
    /// - CDN 上传用 POST（PUT 404）。
    /// - sendmessage item type：image=2 / file=4 / video=5 / voice=3。
    async fn send_media_inner(
        &self,
        conv: &ConvId,
        media: &MediaRef,
        hint: &ReplyHint,
    ) -> Result<()> {
        let peer = Self::peer_of(conv);
        let token = self.resolve_context_token(&peer, hint).await;

        // 1. 读本地文件。v1.23 review：读前大小预检（出站媒体此前无上限，
        // 与入站 50MB 上限不对称——大文件整读即 OOM 尖峰）。
        if let Ok(meta) = std::fs::metadata(&media.url) {
            const OUTBOUND_MEDIA_MAX: u64 = 50 * 1024 * 1024;
            if meta.len() > OUTBOUND_MEDIA_MAX {
                return Err(CoreError::Platform(
                    "ilink",
                    format!(
                        "媒体文件 {} 大小超上限 {}MB，拒绝上传",
                        media.url,
                        OUTBOUND_MEDIA_MAX / (1024 * 1024)
                    ),
                ));
            }
        }
        let plaintext = std::fs::read(&media.url).map_err(|e| {
            CoreError::Platform("ilink", format!("read media file {:?}: {e}", media.url))
        })?;
        let raw_size = plaintext.len() as u64;
        let raw_md5_hex = format!("{:x}", md5::compute(&plaintext));

        // 2. AES 加密。
        let key = crate::media::random_aes_key();
        let ciphertext = crate::media::aes_encrypt(&plaintext, &key);
        let file_size = ciphertext.len() as u64;
        let aeskey_hex = hex::encode(key);
        let aes_key_out = crate::media::encode_aes_key_outbound(&key);

        // 3. getuploadurl。
        let (media_type, item_type) = match media.kind.as_str() {
            "image" => (1i64, 2i64),
            "file" => (3, 4),
            "video" => (2, 5),
            "voice" => (4, 3),
            other => {
                warn!(target: "ilink", kind = other, "未知媒体 kind，按 file 处理");
                (3, 4)
            }
        };
        let filekey = format!("imagent-{}", uuid::Uuid::new_v4().simple());
        let upload = crate::media::get_upload_url(
            self.client.as_ref(),
            &filekey,
            media_type,
            &peer,
            raw_size,
            &raw_md5_hex,
            file_size,
            &aeskey_hex,
        )
        .await?;

        // x-encrypted-param（上传 URL 凭证）来自 upload_param。
        let upload_param = upload
            .upload_param
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                CoreError::Platform("ilink", "getuploadurl: missing upload_param".into())
            })?;

        // 4. CDN POST 上传，响应头 x-encrypted-param = sendmessage 的 encrypt_query_param。
        let encrypt_query_param =
            crate::media::upload_cdn(self.client.http(), upload_param, &filekey, &ciphertext)
                .await?;

        // 5. sendmessage 媒体 item。
        let client_id = format!("imagent-{}", uuid::Uuid::new_v4());
        let item_obj = match media.kind.as_str() {
            "image" => serde_json::json!({
                "type": item_type,
                "image_item": {
                    "media": {
                        "encrypt_query_param": encrypt_query_param,
                        "aes_key": aes_key_out,
                        "encrypt_type": 1,
                    },
                    "mid_size": file_size,
                },
            }),
            _ => serde_json::json!({
                "type": item_type,
                "file_item": {
                    "file_name": std::path::Path::new(&media.url)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&filekey)
                        .to_string(),
                    "media": {
                        "encrypt_query_param": encrypt_query_param,
                        "aes_key": aes_key_out,
                        "encrypt_type": 1,
                    },
                },
            }),
        };

        let mut msg = serde_json::json!({
            "from_user_id": "",
            "to_user_id": peer,
            "client_id": client_id,
            "message_type": 2,
            "message_state": 2,
            "item_list": [item_obj],
        });
        if !token.is_empty() {
            msg["context_token"] = serde_json::json!(token);
        }
        let body = serde_json::json!({ "msg": msg });

        // 出站串行 + 服从式退避：与 send_text 共用 send_with_retry（P3-b，
        // code-review v14：此前两份复制的重试逻辑已分叉——media 路径缺
        // tripped / circuit-open 告警日志）。
        self.send_with_retry(&body, "(media)").await
    }
    /// 发送单条文本（body 构造 + sendmessage 重试 + 限流熔断服从）。
    /// 每条独立走出站串行锁（片间 sleep 时释放锁，不长时间阻塞出站）。
    async fn send_text_one(&self, peer: &str, token: &str, text: &str) -> Result<()> {
        let client_id = format!("imagent-{}", uuid::Uuid::new_v4());
        let mut msg = json!({
            "from_user_id": "",
            "to_user_id": peer,
            "client_id": client_id,
            "message_type": 2,
            "message_state": 2,
            "item_list": [{"type": 1, "text_item": {"text": text}}],
        });
        // context_token 仅非空时带（同 hermes）。
        if !token.is_empty() {
            msg["context_token"] = json!(token);
        }
        let body = json!({ "msg": msg });
        // 出站串行 + 服从式退避：与 send_media 共用 send_with_retry（P3-b，
        // code-review v14：双份复制的重试逻辑收口；锁语义见 P3-c）。
        self.send_with_retry(&body, "").await
    }

    /// P3-b（code-review v14）：文本/媒体 sendmessage 的共用发送入口。
    /// 重试/熔断/退避核心见 [`send_retry_loop`]；`label` 仅用于错误文案区分
    /// 路径（`""` / `"(media)"`，与旧文案逐字兼容）。
    async fn send_with_retry(&self, body: &serde_json::Value, label: &str) -> Result<()> {
        let client = self.client.clone();
        send_retry_loop(&self.send_lock, &self.breaker, label, || {
            client.post_json::<SendMsgResp>("/ilink/bot/sendmessage", body)
        })
        .await
    }
}

/// 判断缓存的 typing_ticket 是否仍有效：非空 + 未过 TTL。
fn ticket_valid(ticket: &str, expiry: Instant, now: Instant) -> bool {
    !ticket.is_empty() && expiry > now
}

/// P2-2（code-review v14）：单条消息媒体项的收集循环——deadline 预算内等
/// FuturesUnordered 逐项完成，预算耗尽即放弃剩余项（warn 记数、文本照投递）。
///
/// 泛型注入下载 future（真实路径见 [`ILinkPlatform::process_media_phase`]），
/// 使并发/预算行为可用可控 future + paused 时钟做回归测试。
///
/// 每个 future 输出 `(原序号, kind, 结果)`：成功项按原序号放回槽位，最终
/// `media_out` 保持与消息内媒体顺序一致（并发完成顺序不影响结果顺序）。
async fn drain_media_with_budget<Fut>(
    media_out: &mut Vec<MediaRef>,
    futs: FuturesUnordered<Fut>,
    budget: Duration,
    total: usize,
) where
    Fut: std::future::Future<Output = (usize, &'static str, std::result::Result<MediaRef, String>)>,
{
    let mut slots: Vec<Option<MediaRef>> = vec![None; total];
    let mut completed = 0usize;
    let mut futs = futs;
    let deadline = tokio::time::Instant::now() + budget;
    while completed < total {
        match tokio::time::timeout_at(deadline, futs.next()).await {
            Ok(Some((idx, _kind, Ok(m)))) => {
                slots[idx] = Some(m);
                completed += 1;
            }
            Ok(Some((_, kind, Err(e)))) => {
                // 单个失败仅 log，不丢整条消息（文本仍可用）——与旧行为一致。
                warn!(target: "ilink", kind, error = %e, "媒体项处理失败（仅丢该媒体）");
                completed += 1;
            }
            Ok(None) => break, // 全部完成（防御分支，正常由 completed==total 退出）
            Err(_) => {
                // P2-2：预算耗尽。未完成项（含仍在排队/在途的）放弃——媒体是
                // 增强信息，不能让它无限期拖住整批入站投递。
                let abandoned = total - completed;
                warn!(
                    target: "ilink",
                    budget_ms = budget.as_millis() as u64,
                    abandoned,
                    "单条消息媒体处理总预算耗尽：未完成媒体项标记失败，文本照常投递（code-review v14 P2-2）"
                );
                break;
            }
        }
    }
    media_out.extend(slots.into_iter().flatten());
}

/// P3-b（code-review v14）：send_retry_loop 单轮结果——包已发完，或需等待后
/// 重试（等待发生在锁外，见下）。
enum SendStep {
    Done,
    Wait(Duration),
}

/// P3-b/P3-c（code-review v14）：sendmessage「发送 + 重试」共用核心。
///
/// 此前 send_media_inner / send_text_one 各持一份复制（熔断前置闸、MAX_RETRIES、
/// 限流退避、网络退避），已分叉——media 路径漏了 `tripped` 与 circuit-open
/// 的 warn 日志（限流熔断触发时无可观测痕迹）。收口为单一实现，两路共用。
///
/// 锁语义（P3-c）：send_lock 只在「读熔断状态 + 发包 + 即时分类」的临界区内
/// 持有；熔断冷却 / 限流退避 / 网络退避的 sleep 一律在锁外。此前整个重试循环
/// 持锁（最坏 4 次重试 × 退避 ≈ 1 分钟），bot 级单锁下 peer A 的重试会卡死
/// peer B 的全部发送。串行语义不变：任一时刻至多一条 sendmessage 在飞（发包
/// 必持锁）；冷却期判断在锁内，sleep 醒来后回到循环头重新取锁、重查熔断状态
/// （等待期间他人可能已重新触发熔断，不可跳过复查）。
///
/// 泛型注入发送动作（do_send）：真实路径 = client.post_json；测试注入可控
/// 响应序列验证重试/熔断/锁释放行为（code-review v14 回归）。
async fn send_retry_loop<F, Fut>(
    send_lock: &Mutex<()>,
    breaker: &crate::ratelimit::RateBreaker,
    label: &str,
    mut do_send: F,
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<SendMsgResp>>,
{
    const MAX_RETRIES: usize = 4;
    const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(3);
    let mut attempt: usize = 0;
    loop {
        let step = {
            let _guard = send_lock.lock().await;
            // 熔断前置闸：cooldown 未过则等待（服从式退避，不发包）。
            let remain = breaker.cooldown_remaining().await;
            if !remain.is_zero() {
                warn!(
                    target: "ilink",
                    cooldown_ms = remain.as_millis() as u64,
                    "rate-limit circuit open, pausing sends"
                );
                SendStep::Wait(remain)
            } else {
                attempt += 1;
                match do_send().await {
                    Ok(resp) => match classify_send(&resp) {
                        SendOutcome::Success => {
                            breaker.reset().await;
                            SendStep::Done
                        }
                        SendOutcome::SessionExpired => {
                            return Err(CoreError::SessionExpired("re-login required".into()));
                        }
                        SendOutcome::RateLimited => {
                            // P3-b：tripped warn 此前只有 text 路径有——熔断触发
                            // 无可观测痕迹，现已两路统一。
                            let tripped = breaker.record_event().await;
                            if tripped {
                                warn!(target: "ilink", "rate-limit circuit opened by sendmessage");
                            }
                            if attempt > MAX_RETRIES {
                                return Err(CoreError::Platform(
                                    "ilink",
                                    format!("sendmessage{label} rate-limited after retries"),
                                ));
                            }
                            warn!(target: "ilink", attempt, "sendmessage rate-limited, backing off");
                            SendStep::Wait(RATE_LIMIT_BACKOFF)
                        }
                        SendOutcome::OtherError(s) => {
                            return Err(CoreError::Platform(
                                "ilink",
                                format!("sendmessage{label} failed: {s}"),
                            ));
                        }
                    },
                    Err(e) => {
                        // P3-a（code-review v14）：401/403 已由 client 层返回 typed
                        // SessionExpired，这里用 matches! 判定（不再字符串匹配）。
                        if is_session_expired(&e) {
                            return Err(e);
                        }
                        if attempt > MAX_RETRIES {
                            return Err(e);
                        }
                        // 网络异常线性退避：1s, 2s, 3s, 4s。
                        let backoff = Duration::from_secs(attempt as u64);
                        warn!(
                            target: "ilink",
                            err = %e,
                            attempt,
                            backoff_ms = backoff.as_millis() as u64,
                            "sendmessage network error, backing off"
                        );
                        SendStep::Wait(backoff)
                    }
                }
            }
        };
        match step {
            SendStep::Done => return Ok(()),
            // P3-c：退避在锁外 sleep——等待期间 send_lock 完全释放，其他 peer
            // 的发送可正常进入临界区。
            SendStep::Wait(d) => tokio::time::sleep(d).await,
        }
    }
}

/// 去重 key：优先 `message_id`，否则 `from_user_id + 文本` 组合。
fn dedup_key(msg: &Msg) -> String {
    if let Some(v) = msg.message_id.as_ref() {
        let s = v.to_string();
        if !s.is_empty() {
            return format!("id:{s}");
        }
    }
    let body = extract_text(msg);
    format!("fc:{}:{}", msg.from_user_id, body)
}

#[async_trait]
impl Platform for ILinkPlatform {
    /// B1 能力声明：媒体回传 + typing。与覆写方法清单对齐，新增覆写须同步——
    /// 逐族对照：
    /// - MEDIA_UPLOAD：send_media（下载→AES 解密→上传回传管线）✅
    /// - TYPING：send_typing（getconfig 取 ticket + sendtyping，真实语义）✅
    /// - CARDS/ASK/COMMAND_CARDS/FORMS/REACTIONS/URGENT_TEXT/GROUP_CHATS/
    ///   RECONNECT：无覆写（走 trait 纯文本/no-op/Err default）❌——注意询问
    ///   闭环本身**可用**（send_permission_ask 文本 default），只是无卡可收敛，
    ///   故不声明 ASK 位。
    /// 一致性测试 `ilink_caps_match_overrides` 钉住本值。
    fn capabilities(&self) -> PlatformCaps {
        PlatformCaps::MEDIA_UPLOAD | PlatformCaps::TYPING
    }

    async fn recv(&self) -> Result<InboundMessage> {
        loop {
            // 1. 先弹缓存
            {
                let mut pending = self.pending.lock().await;
                if !pending.is_empty() {
                    return Ok(pending.remove(0));
                }
            }

            // 2. 长轮询 + 指数退避
            let mut backoff = Duration::from_secs(1);
            loop {
                match self.fetch_updates().await {
                    Ok(msgs) if !msgs.is_empty() => {
                        let mut pending = self.pending.lock().await;
                        pending.extend(msgs);
                        break; // 回外层弹第一条
                    }
                    Ok(_) => {
                        // 长轮询正常返回空（无消息）。服务端正常会 hold ~35s 才返回空；
                        // 加最小间隔兜底，防御服务端某次立即返回空导致忙循环/触发限流。
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        break;
                    }
                    Err(e) => {
                        // SESSION_EXPIRED：session 失效，需重新登录（P3-a：typed
                        // 判定——client 层 401/403 已返回 SessionExpired variant）。
                        if is_session_expired(&e) {
                            error!(target: "ilink", "session expired, re-login required");
                            return Err(e);
                        }
                        let msg_str = format!("{e}");
                        warn!(target: "ilink", err = %msg_str, backoff_ms = backoff.as_millis() as u64, "getupdates failed, backing off");
                        if backoff >= BACKOFF_CAP {
                            return Err(CoreError::Platform(
                                "ilink",
                                format!("getupdates exhausted retries: {msg_str}"),
                            ));
                        }
                        tokio::time::sleep(backoff).await;
                        backoff *= 2;
                    }
                }
            }
        }
    }

    async fn send_text(&self, conv: &ConvId, text: &str, hint: &ReplyHint) -> Result<()> {
        let peer = Self::peer_of(conv);
        let token = self.resolve_context_token(&peer, hint).await;

        // 分片：配置了上限且 >0 才切；否则单片。
        let fragments: Vec<String> = match self.max_text_len {
            Some(n) if n > 0 => {
                let parts = imagent_core::split_message(text, n);
                if parts.len() > 1 {
                    let total = parts.len();
                    parts
                        .into_iter()
                        .enumerate()
                        .map(|(i, frag)| format!("({}/{}) {}", i + 1, total, frag))
                        .collect()
                } else {
                    parts
                }
            }
            _ => vec![text.to_string()],
        };

        let last_idx = fragments.len() - 1;
        for (i, frag) in fragments.into_iter().enumerate() {
            self.send_text_one(&peer, &token, &frag).await?;
            if i != last_idx {
                tokio::time::sleep(self.fragment_interval).await;
            }
        }
        Ok(())
    }

    async fn send_media(&self, conv: &ConvId, media: &MediaRef, hint: &ReplyHint) -> Result<()> {
        self.send_media_inner(conv, media, hint).await
    }

    /// best-effort typing 指示（agent 处理中）。先 getconfig 取 ticket，再 POST sendtyping。
    /// 全程尽力而为：失败仅 log 并返回 Ok，绝不阻断主流程。
    /// 仅发 status=1（start）——typing 时长由客户端按 ticket 自管，无需 stop。
    async fn send_typing(&self, conv: &ConvId, hint: &ReplyHint) -> Result<()> {
        let peer = Self::peer_of(conv);
        let ticket = match self.ensure_typing_ticket(&peer, hint).await {
            Some(t) => t,
            None => return Ok(()), // 无 ticket 则跳过（不阻断）。
        };
        let body = json!({
            "ilink_user_id": peer,
            "typing_ticket": ticket,
            "status": 1u32, // start
        });
        // sendtyping body 无 msg 包装（与 sendmessage 不同，照 hermes）。
        let _: serde_json::Value = match self.client.post_json("/ilink/bot/sendtyping", &body).await
        {
            Ok(v) => v,
            Err(e) => {
                warn!(target: "ilink", peer, error = %e, "sendtyping 失败（忽略）");
                return Ok(());
            }
        };
        debug!(target: "ilink", peer, "sendtyping ok");
        Ok(())
    }

    fn name(&self) -> &'static str {
        PLATFORM
    }
}

/// 判定错误是否指示 session 失效。
///
/// P3-a（code-review v14）：改 typed 判定（`matches!` SessionExpired variant）。
/// 生成路径已全部核对无遗漏：
/// - HTTP 401/403 由 `client::post_json` 直接返回 typed `CoreError::SessionExpired`；
/// - 响应体 ret/errcode 的过期形态（-14 / -2+"unknown error"）由 `classify_send`
///   判为 `SendOutcome::SessionExpired` 后同样转 typed。
///
/// 旧的 Display 字符串匹配（「HTTP 401/403」「SESSION_EXPIRED」子串）已删——
/// crate 内无任何代码再生成含这些子串的错误文案（login 的 401 文案不进本层），
/// 字符串形态文案一改即静默失配，故按 variant 判定。
fn is_session_expired(e: &CoreError) -> bool {
    matches!(e, CoreError::SessionExpired(_))
}

/// 媒体目录：`<imagent_home>/media/`（0700；随 profile 隔离——此前写死
/// `~/.imagent/media`，多 profile 会混存，P5 快赢修正）。
fn media_dir() -> Result<std::path::PathBuf> {
    let dir = imagent_core::paths::imagent_home().join("media");
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| CoreError::Platform("ilink", format!("create media dir {dir:?}: {e}")))?;
    }
    Ok(dir)
}

/// 从文件名或 kind 推断扩展名（含点）；推不出返回空串。
fn guess_ext(file_name: Option<&str>, kind: &str) -> String {
    if let Some(name) = file_name {
        if let Some(idx) = name.rfind('.') {
            let ext = &name[idx..];
            // 仅当看起来像扩展名（≤8 字符、含字母）时保留。
            if ext.len() <= 8 && ext.chars().any(|c| c.is_ascii_alphabetic()) {
                return ext.to_ascii_lowercase();
            }
        }
        return String::new();
    }
    match kind {
        "image" => ".jpg".to_string(),
        _ => ".bin".to_string(),
    }
}

/// 把媒体字节落盘到 `~/.imagent/media/<uuid>.<ext>`，返回该路径的字符串形式。
fn persist_media(kind: &str, file_name: Option<&str>, bytes: &[u8]) -> Result<String> {
    let dir = media_dir()?;
    // 0700 权限（目录私有）。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let ext = guess_ext(file_name, kind);
    let fname = format!("{}{ext}", uuid::Uuid::new_v4().simple());
    let path = dir.join(fname);
    // P2-V：媒体文件权限 0600（headless 部署隐私——解密后的私聊媒体不暴露
    // 给同机其他用户）。v1.23：改 OpenOptions 原子创建（先 write 后 chmod 有
    // umask 0644 的暴露窗口，且 set_permissions 失败被忽略）。
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| CoreError::Platform("ilink", format!("write media {path:?}: {e}")))?;
        f.write_all(bytes)
            .map_err(|e| CoreError::Platform("ilink", format!("write media {path:?}: {e}")))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, bytes)
            .map_err(|e| CoreError::Platform("ilink", format!("write media {path:?}: {e}")))?;
    }
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Item, TextItem};

    #[test]
    fn peer_strips_prefix() {
        assert_eq!(ILinkPlatform::peer_of(&ConvId("ilink:abc".into())), "abc");
        assert_eq!(ILinkPlatform::peer_of(&ConvId("naked".into())), "naked");
    }

    #[test]
    fn dedup_key_prefers_message_id() {
        let msg = Msg {
            from_user_id: "u".into(),
            message_id: Some(serde_json::Value::String("m1".into())),
            context_token: None,
            msg_type: Some(1),
            item_list: vec![],
        };
        assert_eq!(dedup_key(&msg), "id:\"m1\"");
    }

    #[test]
    fn dedup_key_falls_back_to_text() {
        // 无 message_id → from_user_id + extract_text。
        let msg = Msg {
            from_user_id: "u".into(),
            message_id: None,
            context_token: None,
            msg_type: Some(1),
            item_list: vec![Item {
                item_type: 1,
                text_item: Some(TextItem {
                    text: Some("c".into()),
                }),
                ..Default::default()
            }],
        };
        assert_eq!(dedup_key(&msg), "fc:u:c");
    }

    /// P3-a（code-review v14）：typed 判定——仅 `SessionExpired` variant 命中。
    /// Display 携带「HTTP 401」字样的 `Platform` 错误**不再**匹配（字符串匹配
    /// 已删，401/403 在 client 层就转成了 typed variant）。
    #[test]
    fn session_expired_detection_typed() {
        assert!(is_session_expired(&CoreError::SessionExpired(
            "re-login required".into()
        )));
        // 旧字符串形态（若未来有人退回 Platform+401 文案）必须**不**命中——
        // 钉住「判定只认 variant」的契约。
        assert!(!is_session_expired(&CoreError::Platform(
            "ilink",
            "POST x: HTTP 401".into()
        )));
        assert!(!is_session_expired(&CoreError::Platform(
            "ilink",
            "POST x: HTTP 500".into()
        )));
        assert!(!is_session_expired(&CoreError::Store(
            imagent_store::StoreError::Other("x".into())
        )));
    }

    #[test]
    fn ticket_valid_fresh_nonempty() {
        let now = Instant::now();
        let exp = now + Duration::from_secs(400);
        assert!(ticket_valid("tk", exp, now));
    }

    #[test]
    fn ticket_valid_expired() {
        // expiry 在 now 之前 → 已过期。
        let now = Instant::now();
        let exp = now - Duration::from_secs(1);
        assert!(!ticket_valid("tk", exp, now));
    }

    #[test]
    fn ticket_valid_empty_ticket() {
        let now = Instant::now();
        let exp = now + Duration::from_secs(400);
        // 即使未过期，空 ticket 也判无效（需刷新）。
        assert!(!ticket_valid("", exp, now));
    }

    // ------------------------------------------------------------------
    // P2-2（code-review v14）：媒体并发 + 单消息总预算
    // ------------------------------------------------------------------

    /// 预算耗尽：永不完成的下载在 60s 预算处被放弃（warn 记数），已完成的
    /// 成功项保留、顺序稳定、文本照投递（media 只含成功项）。start_paused
    /// 让 60s 预算在 mock 时钟上瞬时推进。四个 async 块形态各异，Box::pin
    /// 统一成同一 Fut 类型。
    #[tokio::test(start_paused = true)]
    async fn media_budget_abandons_stuck_downloads_keeps_finished() {
        type Item = (usize, &'static str, std::result::Result<MediaRef, String>);
        type ItemFut = std::pin::Pin<Box<dyn std::future::Future<Output = Item>>>;
        let futs: FuturesUnordered<ItemFut> = vec![
            // #0 立即成功。
            Box::pin(async {
                (
                    0usize,
                    "image",
                    Ok(MediaRef {
                        kind: "image".into(),
                        url: "/a".into(),
                    }),
                )
            }) as ItemFut,
            // #1 永不完成（模拟 CDN 挂死：单文件 45s 超时也兜不住的形态）。
            Box::pin(async { std::future::pending::<Item>().await }) as ItemFut,
            // #2 立即失败（下载错误——仅丢该媒体项）。
            Box::pin(async { (2usize, "file", Err("download media 失败: boom".into())) })
                as ItemFut,
            // #3 也立即成功——验证顺序按原序号而非完成顺序。
            Box::pin(async {
                (
                    3usize,
                    "file",
                    Ok(MediaRef {
                        kind: "file".into(),
                        url: "/d".into(),
                    }),
                )
            }) as ItemFut,
        ]
        .into_iter()
        .collect();
        let mut media = Vec::new();
        let start = tokio::time::Instant::now();
        drain_media_with_budget(&mut media, futs, Duration::from_secs(60), 4).await;
        // 预算等待确实发生（mock 时钟推进到 ≥60s）。
        assert!(
            start.elapsed() >= Duration::from_secs(60),
            "预算应耗尽：elapsed={:?}",
            start.elapsed()
        );
        // 只有 #0/#3 成功项、按原序号排列。
        let urls: Vec<&str> = media.iter().map(|m| m.url.as_str()).collect();
        assert_eq!(urls, vec!["/a", "/d"], "只保留成功项且按原序");
    }

    /// 全部顺利完成（远小于预算）→ 不等待、全量收集。
    #[tokio::test(start_paused = true)]
    async fn media_budget_completes_within_budget() {
        let futs: FuturesUnordered<_> = (0..3usize)
            .map(|i| async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                (
                    i,
                    "image",
                    Ok(MediaRef {
                        kind: "image".into(),
                        url: format!("/{i}"),
                    }),
                )
            })
            .collect();
        let mut media = Vec::new();
        drain_media_with_budget(&mut media, futs, Duration::from_secs(60), 3).await;
        assert_eq!(media.len(), 3, "预算内应全部完成");
        let urls: Vec<&str> = media.iter().map(|m| m.url.as_str()).collect();
        assert_eq!(urls, vec!["/0", "/1", "/2"], "顺序稳定");
    }

    /// 信号量限流语义（与生产同款形态）：8 个 10s 任务 × 4 并发 ≈ 20s 完成
    /// （若信号量失效则 ~10s，若串行则 ~80s 超预算）——钉住「并发=4」的档位。
    #[tokio::test(start_paused = true)]
    async fn media_downloads_throttled_by_semaphore() {
        let sem = Arc::new(Semaphore::new(MEDIA_DOWNLOAD_CONCURRENCY));
        let futs: FuturesUnordered<_> = (0..8usize)
            .map(|i| {
                let permit = sem.clone().acquire_owned();
                async move {
                    let _p = permit.await.unwrap();
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    (
                        i,
                        "image",
                        Ok(MediaRef {
                            kind: "image".into(),
                            url: format!("/{i}"),
                        }),
                    )
                }
            })
            .collect();
        let start = tokio::time::Instant::now();
        let mut media = Vec::new();
        drain_media_with_budget(&mut media, futs, Duration::from_secs(60), 8).await;
        assert_eq!(media.len(), 8, "预算内应全部完成");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_secs(19),
            "应有并发限流（两波 10s ≈ 20s）：elapsed={elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(60),
            "不应超出预算：elapsed={elapsed:?}"
        );
    }

    // ------------------------------------------------------------------
    // P3-b/P3-c（code-review v14）：sendmessage 重试核心
    // ------------------------------------------------------------------

    /// 网络错误重试后成功：第 1/2 次网络错误、第 3 次成功 → Ok。
    #[tokio::test(start_paused = true)]
    async fn send_retry_recovers_after_transient_errors() {
        let lock = Mutex::new(());
        let breaker =
            crate::ratelimit::RateBreaker::new(Duration::from_secs(30), 3, Duration::from_secs(30));
        let attempts = std::cell::Cell::new(0usize);
        let r = send_retry_loop(&lock, &breaker, "", || {
            let n = attempts.get() + 1;
            attempts.set(n);
            async move {
                if n < 3 {
                    Err(CoreError::Platform("ilink", "network".into()))
                } else {
                    // ret/errcode 均 None → classify Success。
                    Ok(SendMsgResp::default())
                }
            }
        })
        .await;
        assert!(r.is_ok(), "瞬态错误应在重试后成功：{r:?}");
        assert_eq!(attempts.get(), 3);
    }

    /// 网络错误重试耗尽（MAX_RETRIES=4 → 共 5 次尝试）→ 报原始 Err。
    #[tokio::test(start_paused = true)]
    async fn send_retry_gives_up_after_max_retries() {
        let lock = Mutex::new(());
        let breaker =
            crate::ratelimit::RateBreaker::new(Duration::from_secs(30), 3, Duration::from_secs(30));
        let attempts = std::cell::Cell::new(0usize);
        let r = send_retry_loop(&lock, &breaker, "", || {
            attempts.set(attempts.get() + 1);
            async { Err(CoreError::Platform("ilink", "boom".into())) }
        })
        .await;
        let e = r.unwrap_err();
        let msg = format!("{e}");
        assert!(msg.contains("boom"), "应保留原始错误：{msg}");
        assert_eq!(attempts.get(), 5, "4 次重试 + 首次 = 5 次尝试");
    }

    /// P3-c：退避 sleep 期间 send_lock 必须可用（旧实现整循环持锁，peer A 重试
    /// 卡死 peer B）。观察者在重试退避窗口内 try_lock 应成功。
    #[tokio::test(start_paused = true)]
    async fn send_lock_released_during_backoff_sleep() {
        let lock = Mutex::new(());
        let breaker =
            crate::ratelimit::RateBreaker::new(Duration::from_secs(30), 3, Duration::from_secs(30));
        let attempts = std::cell::Cell::new(0usize);
        let saw_free = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saw_free2 = saw_free.clone();
        let (r, _) = tokio::join!(
            send_retry_loop(&lock, &breaker, "", || {
                let n = attempts.get() + 1;
                attempts.set(n);
                async move {
                    if n < 3 {
                        Err(CoreError::Platform("ilink", "network".into()))
                    } else {
                        Ok(SendMsgResp::default())
                    }
                }
            }),
            async {
                // 与重试退避并发跑：任一时刻锁可取即证明退避不持锁。
                for _ in 0..50 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if lock.try_lock().is_ok() {
                        saw_free2.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            }
        );
        assert!(r.is_ok());
        assert!(
            saw_free.load(std::sync::atomic::Ordering::SeqCst),
            "退避窗口内 send_lock 应可被其他 peer 获取"
        );
    }

    /// 限流重试耗尽 → 「rate-limited after retries」文案（label 区分 media 路径，
    /// 与旧文案逐字兼容）。breaker 的 cooldown 用 `std::Instant`（真实钟），
    /// paused mock 时钟不驱动它——故取小 cooldown（50ms 真实时间）防测试
    /// 空转等待 30s；前置闸（circuit-open Wait 分支）仍会被覆盖。
    #[tokio::test(start_paused = true)]
    async fn send_retry_rate_limited_exhausts_with_label() {
        let lock = Mutex::new(());
        // threshold=1：单次限流即熔断。
        let breaker = crate::ratelimit::RateBreaker::new(
            Duration::from_secs(30),
            1,
            Duration::from_millis(50),
        );
        let attempts = std::cell::Cell::new(0usize);
        let r = send_retry_loop(&lock, &breaker, "(media)", || {
            attempts.set(attempts.get() + 1);
            async {
                Ok(SendMsgResp {
                    ret: Some(-2),
                    errcode: None,
                    errmsg: None,
                })
            }
        })
        .await;
        let e = r.unwrap_err();
        let msg = format!("{e}");
        assert!(
            msg.contains("sendmessage(media) rate-limited after retries"),
            "文案错误：{msg}"
        );
        assert_eq!(attempts.get(), 5, "4 次重试 + 首次 = 5 次尝试");
        // 熔断器已被限流事件触发（最后一次 record_event 刚推开 cooldown）。
        assert!(breaker.cooldown_remaining().await > Duration::ZERO);
    }

    /// B1：caps 声明与覆写清单一致性（`capabilities()` 覆写处的逐族对照注释
    /// 即静态清单，本测试钉值防漂移）。capabilities() 为纯同步声明不触网。
    #[tokio::test]
    async fn ilink_caps_match_overrides() {
        let client = ILinkClient::new(None, "tok".into(), "bot".into(), "user".into()).unwrap();
        let db =
            std::env::temp_dir().join(format!("imagent-ilink-caps-{}.db", uuid::Uuid::new_v4()));
        let store = Store::open(&db).await.expect("open store");
        let p = ILinkPlatform::new(client, store, "acct".into(), None, Duration::from_millis(0));
        let caps = p.capabilities();
        assert_eq!(
            caps,
            PlatformCaps::MEDIA_UPLOAD | PlatformCaps::TYPING,
            "ilink 仅声明媒体回传 + typing"
        );
        // URGENT_TEXT ↔ supports_urgent_text 同源（ilink 走 default false）。
        assert_eq!(
            caps.contains(PlatformCaps::URGENT_TEXT),
            p.supports_urgent_text()
        );
        drop(p);
        let _ = std::fs::remove_file(&db);
    }
}
