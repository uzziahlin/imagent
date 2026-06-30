//! `impl imagent_core::Platform` for iLink。
//!
//! - `recv()`：`pending` 缓存上次长轮询多条消息，逐条返回；空则长轮询
//!   `getupdates`（带游标），失败指数退避重连，SESSION_EXPIRED → Err。
//! - `send_text()`：context_token 优先取 `ReplyHint`，否则读 store 该 peer 最新；
//!   POST `sendmessage`；响应若带新 token 则更新。
//! - 媒体/typing：P1 空实现（core 不调媒体）。
//!
//! 鉴权由 core 做：本层只透传 `from_user_id`，不做白名单（DESIGN §9 硬约束①）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;
use tracing::{error, warn};

use imagent_core::{ConvId, CoreError, InboundMessage, MediaRef, Platform, ReplyHint, Result};
use imagent_store::Store;

use crate::client::ILinkClient;
use crate::dedup::Dedup;
use crate::proto::{classify_send, extract_text, msg_to_inbound, Msg, SendMsgResp, SendOutcome, UpdatesResp};

const PLATFORM: &str = "ilink";
const ILINK_PREFIX: &str = "ilink:";
/// 退避上限：到达后停止重试、上报 Err（由 core 决定继续/暂停）。
const BACKOFF_CAP: Duration = Duration::from_secs(30);

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
}

impl ILinkPlatform {
    pub fn new(client: ILinkClient, store: Store, account_id: String) -> Self {
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
            pending: Mutex::new(Vec::new()),
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
        out.push(msg_to_inbound(msg));
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
                        // 长轮询正常返回空（无消息），立即再次轮询。
                        break;
                    }
                    Err(e) => {
                        let msg_str = format!("{e}");
                        // SESSION_EXPIRED：session 失效，需重新登录。
                        if is_session_expired(&msg_str) {
                            error!(target: "ilink", "session expired, re-login required");
                            return Err(CoreError::Platform(
                                "ilink",
                                "session expired, please re-login".into(),
                            ));
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
                        return Err(CoreError::Platform(
                            "ilink",
                            "session expired: re-login required".into(),
                        ));
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
                        return Err(CoreError::Platform(
                            "ilink",
                            "session expired: re-login required".into(),
                        ));
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

    async fn send_media(
        &self,
        _conv: &ConvId,
        _media: &MediaRef,
        _hint: &ReplyHint,
    ) -> Result<()> {
        // P1 不实现媒体（core 不调用）。
        Ok(())
    }

    async fn send_typing(&self, _conv: &ConvId, _hint: &ReplyHint) -> Result<()> {
        // P1 不实现 typing。
        Ok(())
    }

    fn name(&self) -> &'static str {
        PLATFORM
    }
}

/// 判定错误信息是否指示 session 失效（HTTP 401/403 或文本 SESSION_EXPIRED）。
fn is_session_expired(msg: &str) -> bool {
    msg.contains("SESSION_EXPIRED")
        || msg.contains("HTTP 401")
        || msg.contains("HTTP 403")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Item, TextItem};

    #[test]
    fn peer_strips_prefix() {
        assert_eq!(
            ILinkPlatform::peer_of(&ConvId("ilink:abc".into())),
            "abc"
        );
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
                text_item: Some(TextItem { text: Some("c".into()) }),
                voice_item: None,
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
}
