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
use serde_json::json;
use tokio::sync::Mutex;
use tracing::{debug, error, warn};

use imagent_core::{ConvId, CoreError, InboundMessage, MediaRef, Platform, ReplyHint, Result};
use imagent_store::Store;

use crate::client::ILinkClient;
use crate::dedup::Dedup;
use crate::proto::{
    classify_send, extract_media_refs, extract_text, msg_to_inbound, GetConfigResp, Msg,
    SendMsgResp, SendOutcome, UpdatesResp,
};

const PLATFORM: &str = "ilink";
const ILINK_PREFIX: &str = "ilink:";
/// 退避上限：到达后停止重试、上报 Err（由 core 决定继续/暂停）。
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// typing_ticket 缓存 TTL（协议侧 600s，留 100s 余量提前刷新）。
const TYPING_TICKET_TTL: Duration = Duration::from_secs(500);

pub struct ILinkPlatform {
    client: Arc<ILinkClient>,
    store: Store,
    account_id: String,
    dedup: Dedup,
    /// 上次长轮询批量取到的消息，`recv` 逐条弹出。
    pending: Mutex<Vec<InboundMessage>>,
    /// 出站串行：同一 bot 同一时刻只有一条 sendmessage 在飞。
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
                1,
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

    /// 长轮询取消息：读游标 → POST → 更新游标 → 去重 + 更新 peer token → 收集。
    async fn fetch_updates(&self) -> Result<Vec<InboundMessage>> {
        let buf = self
            .store
            .get_sync_buf(PLATFORM, &self.account_id)
            .await
            .unwrap_or(None);
        let body = json!({ "get_updates_buf": buf.unwrap_or_default() });
        let resp: UpdatesResp = self
            .client
            .post_json("/ilink/bot/getupdates", &body)
            .await?;

        if let Some(new_buf) = resp.get_updates_buf.as_deref() {
            if !new_buf.is_empty() {
                let _ = self
                    .store
                    .set_sync_buf(PLATFORM, &self.account_id, new_buf)
                    .await;
            }
        }

        let mut out = Vec::with_capacity(resp.msgs.len());
        for msg in &resp.msgs {
            self.process_msg(msg, &mut out).await;
        }
        Ok(out)
    }

    async fn process_msg(&self, msg: &Msg, out: &mut Vec<InboundMessage>) {
        let key = dedup_key(msg);
        if !self.dedup.check(&key) {
            return;
        }
        // 更新该 peer 最新 context_token（发消息回传）。
        if let Some(token) = msg.context_token.as_deref() {
            if !token.is_empty() {
                let _ = self
                    .store
                    .set_context_token(PLATFORM, &self.account_id, &msg.from_user_id, token)
                    .await;
            }
        }

        let mut ib = msg_to_inbound(msg);
        // 阶段 A：下载入站媒体（图片/文件/视频），存 ~/.imagent/media/。
        // 逐个尽力而为：单个失败仅 log，不丢整条消息（文本仍可用）。
        for raw in extract_media_refs(msg) {
            match crate::media::download_media(
                self.client.http(),
                raw.encrypt_query_param.as_deref(),
                raw.aes_key.as_deref(),
                raw.full_url.as_deref(),
            )
            .await
            {
                Ok(bytes) => match persist_media(raw.kind, raw.file_name.as_deref(), &bytes) {
                    Ok(path) => ib.media.push(MediaRef {
                        kind: raw.kind.to_string(),
                        url: path,
                    }),
                    Err(e) => {
                        warn!(target: "ilink", kind = raw.kind, error = %e, "persist media 失败")
                    }
                },
                Err(e) => {
                    warn!(target: "ilink", kind = raw.kind, error = %e, "download media 失败")
                }
            }
        }
        out.push(ib);
    }

    /// 解析发送阶段 context_token：优先 hint，否则读 store。
    async fn resolve_context_token(&self, peer: &str, hint: &ReplyHint) -> String {
        match hint {
            ReplyHint::ILink { context_token } if !context_token.is_empty() => {
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
            Ok(resp) if resp.typing_ticket.as_deref().is_some_and(|t| !t.is_empty()) => {
                let ticket = resp.typing_ticket.unwrap();
                self.typing_tickets.lock().await.insert(
                    peer.to_string(),
                    (ticket.clone(), Instant::now() + TYPING_TICKET_TTL),
                );
                Some(ticket)
            }
            Ok(_) => {
                warn!(target: "ilink", peer, "getconfig 无 typing_ticket");
                None
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

        // 1. 读本地文件。
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

        // 出站串行 + 服从式退避（同 send_text）。
        let _guard = self.send_lock.lock().await;
        const MAX_RETRIES: usize = 4;
        const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(3);
        let mut attempt: usize = 0;
        loop {
            let remain = self.breaker.cooldown_remaining().await;
            if !remain.is_zero() {
                tokio::time::sleep(remain).await;
            }
            attempt += 1;
            match self
                .client
                .post_json::<SendMsgResp>("/ilink/bot/sendmessage", &body)
                .await
            {
                Ok(resp) => match classify_send(&resp) {
                    SendOutcome::Success => {
                        self.breaker.reset().await;
                        return Ok(());
                    }
                    SendOutcome::SessionExpired => {
                        return Err(CoreError::SessionExpired("re-login required".into()));
                    }
                    SendOutcome::RateLimited => {
                        self.breaker.record_event().await;
                        if attempt > MAX_RETRIES {
                            return Err(CoreError::Platform(
                                "ilink",
                                "sendmessage(media) rate-limited after retries".into(),
                            ));
                        }
                        tokio::time::sleep(RATE_LIMIT_BACKOFF).await;
                        continue;
                    }
                    SendOutcome::OtherError(s) => {
                        return Err(CoreError::Platform(
                            "ilink",
                            format!("sendmessage(media) failed: {s}"),
                        ));
                    }
                },
                Err(e) => {
                    if is_session_expired(&format!("{e}")) {
                        return Err(CoreError::SessionExpired("re-login required".into()));
                    }
                    if attempt > MAX_RETRIES {
                        return Err(e);
                    }
                    let backoff = Duration::from_secs(attempt as u64);
                    warn!(target: "ilink", err = %e, attempt, "sendmessage(media) network error, backing off");
                    tokio::time::sleep(backoff).await;
                }
            }
        }
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
        // 出站串行：同一 bot 同一时刻只有一条 sendmessage 在飞，
        // 避免并发叠加触发限流。
        let _guard = self.send_lock.lock().await;

        const MAX_RETRIES: usize = 4;
        const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(3);
        let mut attempt: usize = 0;
        loop {
            // 熔断前置闸：cooldown 未过则等待（服从式退避，不发包）。
            let remain = self.breaker.cooldown_remaining().await;
            if !remain.is_zero() {
                warn!(target: "ilink", cooldown_ms = remain.as_millis() as u64, "rate-limit circuit open, pausing sends");
                tokio::time::sleep(remain).await;
            }

            attempt += 1;
            match self
                .client
                .post_json::<SendMsgResp>("/ilink/bot/sendmessage", &body)
                .await
            {
                Ok(resp) => match classify_send(&resp) {
                    SendOutcome::Success => {
                        self.breaker.reset().await;
                        return Ok(());
                    }
                    SendOutcome::SessionExpired => {
                        return Err(CoreError::SessionExpired("re-login required".into()));
                    }
                    SendOutcome::RateLimited => {
                        let tripped = self.breaker.record_event().await;
                        if tripped {
                            warn!(target: "ilink", "rate-limit circuit opened by sendmessage");
                        }
                        if attempt > MAX_RETRIES {
                            return Err(CoreError::Platform(
                                "ilink",
                                "sendmessage rate-limited after retries".into(),
                            ));
                        }
                        warn!(target: "ilink", attempt, "sendmessage rate-limited, backing off");
                        tokio::time::sleep(RATE_LIMIT_BACKOFF).await;
                        continue;
                    }
                    SendOutcome::OtherError(s) => {
                        return Err(CoreError::Platform(
                            "ilink",
                            format!("sendmessage failed: {s}"),
                        ));
                    }
                },
                Err(e) => {
                    // HTTP/网络层错误：先判 session_expired（401/403 字样）。
                    let es = format!("{e}");
                    if is_session_expired(&es) {
                        return Err(CoreError::SessionExpired("re-login required".into()));
                    }
                    if attempt > MAX_RETRIES {
                        return Err(e);
                    }
                    // 网络异常线性退避：1s, 2s, 3s, 4s。
                    let backoff = Duration::from_secs(attempt as u64);
                    warn!(target: "ilink", err = %es, attempt, backoff_ms = backoff.as_millis() as u64, "sendmessage network error, backing off");
                    tokio::time::sleep(backoff).await;
                    continue;
                }
            }
        }
    }
}

/// 判断缓存的 typing_ticket 是否仍有效：非空 + 未过 TTL。
fn ticket_valid(ticket: &str, expiry: Instant, now: Instant) -> bool {
    !ticket.is_empty() && expiry > now
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
                        let msg_str = format!("{e}");
                        // SESSION_EXPIRED：session 失效，需重新登录。
                        if is_session_expired(&msg_str) {
                            error!(target: "ilink", "session expired, re-login required");
                            return Err(CoreError::SessionExpired("please re-login".into()));
                        }
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

/// 判定错误信息是否指示 session 失效（HTTP 401/403 或文本 SESSION_EXPIRED）。
fn is_session_expired(msg: &str) -> bool {
    msg.contains("SESSION_EXPIRED") || msg.contains("HTTP 401") || msg.contains("HTTP 403")
}

/// 媒体目录：`~/.imagent/media/`（0700）。
fn media_dir() -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| {
        CoreError::Platform("ilink", "cannot resolve home dir for media storage".into())
    })?;
    let dir = home.join(".imagent").join("media");
    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(|e| {
            CoreError::Platform("ilink", format!("create media dir {:?}: {e}", dir))
        })?;
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
    std::fs::write(&path, bytes)
        .map_err(|e| CoreError::Platform("ilink", format!("write media {:?}: {e}", path)))?;
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

    #[test]
    fn session_expired_detection() {
        assert!(is_session_expired("POST x: HTTP 401"));
        assert!(is_session_expired("SESSION_EXPIRED: token invalid"));
        assert!(!is_session_expired("POST x: HTTP 500"));
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
}
