//! [`FeishuPlatform`]：实现 [`imagent_core::Platform`]。
//!
//! 与 wecom 的关键差异：飞书**收发分离**——收走长连接（`FeishuWsClient`），
//! 发走独立 HTTP（`client::send_text_msg`），无需 wecom 那条 outbound channel。
//!
//! - `recv()`：drain task 已把 `InboundMessage` 推入 inbound channel，直接 await。
//! - `send_text()`：`receive_target_from_conv` → `split_message` 分片 → 每片
//!   `get_token`（lazy 刷新缓存）+ `send_text_msg`（HTTP）。
//! - `send_media()`：agent 产图回传（上传+发 image 消息）；`send_typing()`：MVP 空实现。
//! - `send_card()`/`update_card()`：managed 真流式（`card:` 前缀句柄，CardKit 实体 +
//!   element PATCH 打字机）+ 降级 raw（`msg:` 前缀句柄，整卡 im patch）句柄分流。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::warn;

use imagent_core::{
    split_message, CardTerminal, ConvId, CoreError, Dedup, InboundMessage, MediaRef, OutboundCard,
    Platform, ReplyHint, Result,
};

use open_lark::{Config, CoreConfig};

use crate::card::{
    render_card, render_permission_card, render_permission_card_cancelled, render_stream_init_card,
    stream_body_final, stream_body_md,
};
use crate::client::{
    create_card_entity, download_file, download_image, fetch_bot_open_id, fetch_token, patch_card,
    patch_card_element, patch_card_settings, reply_comment, send_card_msg, send_card_ref_msg,
    send_image_msg, send_text_msg, upload_image, FeishuWsClient,
};
use crate::proto::{
    comment_target_from_conv, is_comment_event, parse_card_action_event, parse_comment_event,
    parse_message_event, receive_target_from_conv, ReceiveIdKind, COMMENT_CONV_PREFIX,
};

/// 平台名常量。
const PLATFORM: &str = "feishu";
/// 飞书单条文本消息 content 上限（保守值，留余量；精确阈值查官方文档）。
const FEISHU_TEXT_MAX: usize = 28_000;
/// `tenant_access_token` 有效期 2h（7200s），距过期 < 10min（即 elapsed >= 110min）则刷新。
const TOKEN_TTL: Duration = Duration::from_secs(110 * 60);

/// 飞书 Platform 适配器。
///
/// 持有发消息所需的 core 配置 + 凭据 + token 缓存；收消息由后台 WS task 推入
/// inbound channel。token 走 lazy 刷新（不用后台定时 task），避免过期窗口。
pub struct FeishuPlatform {
    /// 发消息用配置（HTTP OpenAPI + 取 token）。
    core_config: Arc<CoreConfig>,
    app_id: String,
    app_secret: String,
    /// token 缓存：`(token, fetched_at)`，elapsed >= TOKEN_TTL 则刷新。
    token: Arc<RwLock<Option<(String, Instant)>>>,
    /// CardKit 卡片的 sequence 计数（element/settings PATCH 共用，per card_id 严格递增）。
    card_seqs: Arc<Mutex<HashMap<String, i64>>>,
    /// `/reconnect` 强制重连信号（与 WS run task 共享，P4-7）。
    reconnect: Arc<tokio::sync::Notify>,
    /// 已解析的入站消息 channel，`recv` 直接 await。
    inbound_rx: Arc<Mutex<mpsc::Receiver<InboundMessage>>>,
    /// P5-16：per-conv 最近一次审批询问卡的 `(message_id, tool_name)`
    /// （`/stop` 撤回用）。仅记录卡片路径的成功发送；文本询问无句柄，撤回为 no-op。
    pending_asks: Arc<Mutex<HashMap<String, (String, String)>>>,
}

impl FeishuPlatform {
    /// 构造并后台 spawn：① WS client run task（收事件 + 重连）；
    /// ② drain task（payload → `parse_message_event` → Dedup → inbound channel）。
    pub fn new(app_id: String, app_secret: String, base_url: String) -> Result<Self> {
        let ws_config = Arc::new(
            Config::builder()
                .app_id(app_id.clone())
                .app_secret(app_secret.clone())
                .base_url(base_url.clone())
                .req_timeout(Duration::from_secs(30))
                .build(),
        );
        let core_config = Arc::new(
            CoreConfig::builder()
                .app_id(app_id.clone())
                .app_secret(app_secret.clone())
                .base_url(base_url)
                .build(),
        );

        // WS 收事件 task：payload → channel。
        let (payload_tx, payload_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let ws = FeishuWsClient::new(ws_config);
        let reconnect = ws.reconnect_handle();
        tokio::spawn(async move {
            ws.run(payload_tx).await;
        });

        // drain task：payload → parse（消息 / 审批按钮回调 / 云文档评论）→ Dedup →
        // （消息类）媒体下载落盘 → inbound channel。
        let (inbound_msg_tx, inbound_msg_rx) = mpsc::channel::<InboundMessage>(64);
        let dedup = Dedup::default();
        // token Arc 须在 spawn 前创建：drain task 下载媒体需取 token（发送/接收共用
        // 同一 lazy 刷新缓存，见 fetch_cached_token）。
        let token: Arc<RwLock<Option<(String, Instant)>>> = Arc::new(RwLock::new(None));
        let core_config_for_drain = core_config.clone();
        let app_id_for_drain = app_id.clone();
        let app_secret_for_drain = app_secret.clone();
        let token_for_drain = token.clone();
        // P5-8：bot 自身 open_id 懒取缓存（@bot 过滤用；open_id 随应用固定，
        // 进程内取一次。取不到时 parse_comment_event 退化为弱过滤）。
        let bot_open_id: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
        let bot_open_id_for_drain = bot_open_id.clone();
        tokio::spawn(async move {
            let mut payload_rx = payload_rx;
            while let Some(payload) = payload_rx.recv().await {
                // 三类事件：普通消息（含媒体下载）/ 审批按钮回调 / 云文档评论。
                if let Some((msgid, mut msg, pending)) = parse_message_event(&payload) {
                    if !dedup.check(&msgid) {
                        continue;
                    }
                    // 下载落盘每个待处理媒体；单个失败只 warn 跳过，不丢整条消息。
                    for p in &pending {
                        let token = match fetch_cached_token(
                            &token_for_drain,
                            &core_config_for_drain,
                            &app_id_for_drain,
                            &app_secret_for_drain,
                        )
                        .await
                        {
                            Ok(t) => t,
                            Err(e) => {
                                warn!(target: "feishu", error = %e, "取 token 失败，跳过该媒体");
                                msg.media_errors
                                    .push(format!("{}: 取 token 失败: {e}", p.key));
                                continue;
                            }
                        };
                        let dl = match p.kind {
                            "image" => {
                                download_image(
                                    &core_config_for_drain,
                                    &token,
                                    &p.message_id,
                                    &p.key,
                                )
                                .await
                            }
                            "file" => {
                                download_file(&core_config_for_drain, &token, &p.message_id, &p.key)
                                    .await
                            }
                            _ => continue,
                        };
                        // token 失效自愈（与发送侧 with_token 同语义）：清缓存强制
                        // 刷新后再试一次；二次仍失败如实进 media_errors。
                        let dl = match dl {
                            Ok(b) => Ok(b),
                            Err(e) if crate::client::is_token_invalid_msg(&e.to_string()) => {
                                warn!(target: "feishu", error = %e, "媒体下载遇 token 失效码，清缓存刷新后重试一次");
                                *token_for_drain.write().await = None;
                                let token = match fetch_cached_token(
                                    &token_for_drain,
                                    &core_config_for_drain,
                                    &app_id_for_drain,
                                    &app_secret_for_drain,
                                )
                                .await
                                {
                                    Ok(t) => t,
                                    Err(e2) => {
                                        msg.media_errors
                                            .push(format!("{}: 重取 token 失败: {e2}", p.key));
                                        continue;
                                    }
                                };
                                match p.kind {
                                    "image" => {
                                        download_image(
                                            &core_config_for_drain,
                                            &token,
                                            &p.message_id,
                                            &p.key,
                                        )
                                        .await
                                    }
                                    "file" => {
                                        download_file(
                                            &core_config_for_drain,
                                            &token,
                                            &p.message_id,
                                            &p.key,
                                        )
                                        .await
                                    }
                                    _ => continue,
                                }
                            }
                            other => other,
                        };
                        match dl {
                            Ok(bytes) => match persist_media(p.kind, &p.key, &bytes) {
                                Ok(path) => msg.media.push(MediaRef {
                                    kind: p.kind.to_string(),
                                    url: path,
                                }),
                                Err(e) => {
                                    warn!(target: "feishu", error = %e, "媒体落盘失败，跳过");
                                    msg.media_errors.push(format!("{}: 落盘失败: {e}", p.key));
                                }
                            },
                            Err(e) => {
                                warn!(
                                    target: "feishu",
                                    error = %e,
                                    message_id = %p.message_id,
                                    file_key = %p.key,
                                    "媒体下载失败，跳过"
                                );
                                msg.media_errors.push(format!("{}: 下载失败: {e}", p.key));
                            }
                        }
                    }
                    if inbound_msg_tx.send(msg).await.is_err() {
                        break;
                    }
                    continue;
                }
                // P4-4：审批按钮回调（card.action.trigger）→ text="y"/"n" 的
                // 入站消息，core 的审批回复路由消费（parse_reply("y")=allow）。
                if let Some((key, reply_msg)) = parse_card_action_event(&payload) {
                    if dedup.check(&key) && inbound_msg_tx.send(reply_msg).await.is_err() {
                        break;
                    }
                    continue;
                }
                // P4-9：云文档评论 @bot（drive.file.comment.created_v1）→ 评论
                // 线程消息（conv = feishu:comment:<file>:<comment>，回复走
                // reply_comment；需在飞书后台订阅该事件）。
                // P5-8：仅接受 @bot 的评论——bot open_id 首次遇到评论事件时懒取
                // （GET /bot/v3/info）并缓存；取不到时退化为「至少含一个 @」的
                // 弱过滤。另过滤 bot 自身的回复（防自触发循环）。
                if is_comment_event(&payload) {
                    if bot_open_id_for_drain.read().await.is_none() {
                        let fetched = async {
                            let t = fetch_cached_token(
                                &token_for_drain,
                                &core_config_for_drain,
                                &app_id_for_drain,
                                &app_secret_for_drain,
                            )
                            .await?;
                            fetch_bot_open_id(&core_config_for_drain, &t).await
                        }
                        .await;
                        match fetched {
                            Ok(b) => *bot_open_id_for_drain.write().await = Some(b),
                            Err(e) => warn!(
                                target: "feishu",
                                error = %e,
                                "取 bot open_id 失败，评论 @bot 过滤退化为「须含 @」"
                            ),
                        }
                    }
                    let bot = bot_open_id_for_drain.read().await.clone();
                    if let Some((key, cm)) = parse_comment_event(&payload, bot.as_deref()) {
                        if dedup.check(&key) && inbound_msg_tx.send(cm).await.is_err() {
                            break;
                        }
                    } else {
                        tracing::debug!(target: "feishu", "评论未 @bot（或字段缺失/纯@），丢弃");
                    }
                    continue;
                }
                warn!(target: "feishu", "无法解析/非目标事件，丢弃");
            }
        });

        Ok(Self {
            core_config,
            app_id,
            app_secret,
            token,
            card_seqs: Arc::new(Mutex::new(HashMap::new())),
            reconnect,
            inbound_rx: Arc::new(Mutex::new(inbound_msg_rx)),
            pending_asks: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// 取当前 token：缓存命中（未过 TTL）则返回，否则 `fetch_token` 刷新并缓存。
    ///
    /// 逻辑实现在模块级 [`fetch_cached_token`]（drain task 与本方法共用同一缓存）。
    async fn get_token(&self) -> Result<String> {
        fetch_cached_token(
            &self.token,
            &self.core_config,
            &self.app_id,
            &self.app_secret,
        )
        .await
    }

    /// 清空 token 缓存（下次 `get_token` 强制刷新）。
    async fn invalidate_token(&self) {
        *self.token.write().await = None;
    }

    /// 取 token 执行 `f(token)`；遇 token 失效类错误码（99991663 等，识别见
    /// [`crate::client::is_token_invalid_msg`]）→ 清缓存重取后再试一次。
    ///
    /// 缓存 token 被服务端提前吊销（app_secret 轮换 / 后台强制失效）时，TTL 内
    /// 重用旧值永远失败；此前只能等 TTL 过期自愈。二次仍失败则如实返回错误。
    async fn with_token<T, F, Fut>(&self, f: F) -> Result<T>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let token = self.get_token().await?;
        match f(token).await {
            Err(e) if crate::client::is_token_invalid_msg(&e.to_string()) => {
                warn!(target: "feishu", error = %e, "token 失效错误码，清缓存强制刷新后重试一次");
                self.invalidate_token().await;
                let fresh = self.get_token().await?;
                f(fresh).await
            }
            other => other,
        }
    }

    /// 取该 card_id 的下一个 sequence（严格递增；element 与 settings PATCH 共用）。
    async fn next_card_seq(&self, card_id: &str) -> i64 {
        let mut m = self.card_seqs.lock().await;
        let entry = m.entry(card_id.to_string()).or_insert(0);
        *entry += 1;
        *entry
    }

    /// 降级路径：发 raw 卡片消息（content=卡片 JSON），句柄 `msg:<message_id>`。
    ///
    /// managed 路径（create entity）失败时回退到整卡 im patch——体验同旧版，
    /// 不依赖 `cardkit:card:write` 权限。
    async fn send_card_raw(
        &self,
        receive_id: &str,
        kind: ReceiveIdKind,
        card: &OutboundCard,
        token: &str,
    ) -> Result<Option<String>> {
        let card_json = render_card(card);
        let mid = send_card_msg(&self.core_config, token, receive_id, kind, &card_json).await?;
        Ok(mid.map(|m| format!("msg:{m}")))
    }
}

/// 媒体目录：`<imagent_home>/media/`（0700；P4-10：随 profile 隔离）。
fn media_dir() -> Result<std::path::PathBuf> {
    let dir = imagent_core::paths::imagent_home().join("media");
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| CoreError::Platform(PLATFORM, format!("create media dir {dir:?}: {e}")))?;
    }
    Ok(dir)
}

/// 把媒体字节落盘到 `~/.imagent/media/<key>.<ext>`，返回本地路径字符串。
///
/// 文件名用飞书的 `image_key`/`file_key`（全局唯一，天然去重覆盖）。照 ilink
/// `persist_media`：目录 0700、文件 0600（解密后的私聊媒体不暴露给同机其他用户）。
fn persist_media(kind: &str, key: &str, bytes: &[u8]) -> Result<String> {
    let dir = media_dir()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let ext = if kind == "image" { "jpg" } else { "bin" };
    let path = dir.join(format!("{key}.{ext}"));
    std::fs::write(&path, bytes)
        .map_err(|e| CoreError::Platform(PLATFORM, format!("write media {path:?}: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(path.to_string_lossy().into_owned())
}

/// 取当前 token：缓存命中（未过 TTL）则返回，否则 `fetch_token` 刷新并缓存。
///
/// 提成模块级自由函数——drain task 持有 `Arc<RwLock<…>>` 句柄而无 `&self`，无法调
/// [`FeishuPlatform::get_token`]，故抽出共用（与发送侧共享同一 lazy 缓存）。
/// P5：读锁快路径 + 写锁双检——此前每次都直接取写锁且跨网络调用（最坏 30s），
/// token 刷新期间所有发送/媒体下载被串行阻塞。
async fn fetch_cached_token(
    token_lock: &Arc<RwLock<Option<(String, Instant)>>>,
    core_config: &CoreConfig,
    app_id: &str,
    app_secret: &str,
) -> Result<String> {
    if let Some((token, fetched_at)) = token_lock.read().await.as_ref() {
        if fetched_at.elapsed() < TOKEN_TTL {
            return Ok(token.clone());
        }
    }
    let mut cache = token_lock.write().await;
    // 双检：等写锁期间可能已被并发刷新。
    if let Some((token, fetched_at)) = cache.as_ref() {
        if fetched_at.elapsed() < TOKEN_TTL {
            return Ok(token.clone());
        }
    }
    let token = fetch_token(core_config, app_id, app_secret).await?;
    *cache = Some((token.clone(), Instant::now()));
    Ok(token)
}

#[async_trait]
impl Platform for FeishuPlatform {
    async fn recv(&self) -> Result<InboundMessage> {
        self.inbound_rx.lock().await.recv().await.ok_or_else(|| {
            CoreError::Platform(PLATFORM, "入站 channel 已关闭（client 已退出）".into())
        })
    }

    async fn send_text(&self, conv: &ConvId, text: &str, _hint: &ReplyHint) -> Result<()> {
        // P4-9：评论线程 conv → 回复云文档评论（每分片一条回复）。
        if let Some((file_token, comment_id)) = comment_target_from_conv(conv) {
            let chunks: Vec<String> = split_message(text, FEISHU_TEXT_MAX);
            let total = chunks.len();
            for (i, chunk) in chunks.into_iter().enumerate() {
                // P5：中途失败标明分片序号——用户能感知回复被截断而非静默缺尾。
                // token 失效错误码由 with_token 清缓存自愈（其余错误如实上抛）。
                if let Err(e) = self
                    .with_token(|t| {
                        let file_token = file_token.clone();
                        let comment_id = comment_id.clone();
                        let chunk = chunk.clone();
                        async move {
                            reply_comment(&self.core_config, &t, &file_token, &comment_id, &chunk)
                                .await
                        }
                    })
                    .await
                {
                    return Err(CoreError::Platform(
                        PLATFORM,
                        format!("第 {}/{} 片发送失败（回复可能被截断）：{e}", i + 1, total),
                    ));
                }
            }
            return Ok(());
        }
        let (receive_id, kind) = receive_target_from_conv(conv)
            .ok_or_else(|| CoreError::Platform(PLATFORM, format!("非法 conv_id: {}", conv.0)))?;
        let chunks: Vec<String> = split_message(text, FEISHU_TEXT_MAX);
        let total = chunks.len();
        for (i, chunk) in chunks.into_iter().enumerate() {
            // P5：同上——分片失败标注序号（此前中途 ? 退出，截断无标记）。
            if let Err(e) = self
                .with_token(|t| {
                    let receive_id = receive_id.clone();
                    let chunk = chunk.clone();
                    async move {
                        send_text_msg(&self.core_config, &t, &receive_id, kind, &chunk).await
                    }
                })
                .await
            {
                return Err(CoreError::Platform(
                    PLATFORM,
                    format!("第 {}/{} 片发送失败（回复可能被截断）：{e}", i + 1, total),
                ));
            }
        }
        Ok(())
    }

    async fn send_media(&self, conv: &ConvId, media: &MediaRef, _hint: &ReplyHint) -> Result<()> {
        // agent 产出图片回传：读本地文件 → 上传拿 image_key → 发 image 消息。
        let (receive_id, kind) = receive_target_from_conv(conv)
            .ok_or_else(|| CoreError::Platform(PLATFORM, format!("非法 conv_id: {}", conv.0)))?;
        let bytes = tokio::fs::read(&media.url)
            .await
            .map_err(|e| CoreError::Platform(PLATFORM, format!("读媒体文件 {}: {e}", media.url)))?;
        let file_name = std::path::Path::new(&media.url)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "image.png".to_string());
        // 上传 + 发送共用一次 with_token：同一 token 失效只自愈重试一轮。
        // 重试要求闭包可重入，move 型捕获（bytes/receive_id/file_name）先 clone。
        self.with_token(|t| {
            let bytes = bytes.clone();
            let receive_id = receive_id.clone();
            let file_name = file_name.clone();
            async move {
                let image_key = upload_image(&self.core_config, &t, &file_name, bytes).await?;
                send_image_msg(&self.core_config, &t, &receive_id, kind, &image_key).await
            }
        })
        .await
    }

    async fn send_typing(&self, _conv: &ConvId, _hint: &ReplyHint) -> Result<()> {
        // 飞书协议无 typing 语义。
        Ok(())
    }
    fn supports_streaming_card(&self, conv: &ConvId) -> bool {
        // P4-9：评论线程无卡片语义（回复是评论文本），走纯文本流。
        !conv.0.starts_with(COMMENT_CONV_PREFIX)
    }

    /// P4-7：强制重连——notify_one 存 permit，WS run task 的 select 立即/稍后消费，
    /// 丢弃 open future 断开当前连接后重连。
    async fn reconnect(&self) -> Result<()> {
        self.reconnect.notify_one();
        Ok(())
    }

    /// P4-4：审批询问走「按钮卡片」——点击后飞书推 card.action.trigger，
    /// value 带回 conv + 动作，drain 解析成 text="y"/"n" 复用审批回复路由。
    /// 卡片发送失败（无卡片权限等）降级纯文本（文本失败才向上报错 → dispatch 回 deny）。
    /// P5-16：成功发出卡片时记录 message_id（`cancel_permission_ask` 撤回用）。
    async fn send_permission_ask(
        &self,
        conv: &ConvId,
        tool_name: &str,
        input_summary: &str,
        hint: &ReplyHint,
    ) -> Result<()> {
        // 评论线程无卡片语义，直接走文本（send_text 已路由 reply_comment）。
        if comment_target_from_conv(conv).is_some() {
            return self
                .send_permission_ask_text(conv, tool_name, input_summary, hint)
                .await;
        }
        let (receive_id, kind) = receive_target_from_conv(conv)
            .ok_or_else(|| CoreError::Platform(PLATFORM, format!("非法 conv_id: {}", conv.0)))?;
        let card_json = render_permission_card(tool_name, input_summary, &conv.0);
        match self
            .with_token(|t| {
                let receive_id = receive_id.clone();
                let card_json = card_json.clone();
                async move {
                    send_card_msg(&self.core_config, &t, &receive_id, kind, &card_json).await
                }
            })
            .await
        {
            Ok(mid) => {
                if let Some(mid) = mid {
                    self.pending_asks
                        .lock()
                        .await
                        .insert(conv.0.clone(), (mid, tool_name.to_string()));
                }
                Ok(())
            }
            Err(e) => {
                warn!(target: "feishu", error = %e, "审批卡片发送失败，降级纯文本询问");
                self.send_permission_ask_text(conv, tool_name, input_summary, hint)
                    .await
            }
        }
    }

    /// P5-16：把该 conv 最近一次审批询问卡 patch 成「已中断」终态（移除按钮，
    /// 防用户对已中断的任务做审批）。无记录（文本询问/未发过卡）时 no-op。
    async fn cancel_permission_ask(&self, conv: &ConvId) -> Result<()> {
        let Some((message_id, tool_name)) = self.pending_asks.lock().await.remove(&conv.0) else {
            return Ok(());
        };
        let card_json = render_permission_card_cancelled(&tool_name);
        self.with_token(|t| {
            let message_id = message_id.clone();
            let card_json = card_json.clone();
            async move { patch_card(&self.core_config, &t, &message_id, &card_json).await }
        })
        .await
    }

    /// 发流式卡片。**句柄前缀分流**（core 无感，两种句柄均原样透传给 update_card）：
    /// - managed（优先）：`create_card_entity` + 发 card_id 引用消息 → `card:<card_id>`，
    ///   后续 element 级 PATCH 走服务端打字机渲染（需 `cardkit:card:write` 权限）
    /// - 降级：raw 卡片消息 → `msg:<message_id>`，后续整卡 im patch（体验同旧版）
    async fn send_card(
        &self,
        conv: &ConvId,
        card: &OutboundCard,
        _hint: &ReplyHint,
    ) -> Result<Option<String>> {
        let (receive_id, kind) = receive_target_from_conv(conv)
            .ok_or_else(|| CoreError::Platform(PLATFORM, format!("非法 conv_id: {}", conv.0)))?;
        self.with_token(|t| {
            let receive_id = receive_id.clone();
            async move {
                match create_card_entity(&t, &render_stream_init_card()).await {
                    Ok(card_id) => {
                        match send_card_ref_msg(&self.core_config, &t, &receive_id, kind, &card_id)
                            .await
                        {
                            Ok(_) => Ok(Some(format!("card:{card_id}"))),
                            Err(e) => {
                                // 实体已建但消息发送失败：实体作废（14 天过期自然回收），降级 raw。
                                warn!(target: "feishu", error = %e, "发送卡片引用消息失败，降级 raw 卡片");
                                self.send_card_raw(&receive_id, kind, card, &t).await
                            }
                        }
                    }
                    Err(e) => {
                        // 权限未开（cardkit:card:write）或创建失败 → 降级 raw + 整卡 im patch。
                        warn!(target: "feishu", error = %e, "创建卡片实体失败（需 cardkit:card:write 权限），降级 raw 卡片");
                        self.send_card_raw(&receive_id, kind, card, &t).await
                    }
                }
            }
        })
        .await
    }

    /// 更新流式卡片。按 [`send_card`](Self::send_card) 返回的句柄前缀分流：
    /// - `card:<card_id>`：CardKit 真流式——Running 时 PATCH `md_body`（正文+工具，
    ///   打字机渐显）；Done/Error 时 PATCH 终态正文（含工具统计+完成行）并 PATCH
    ///   settings 关闭流式（光标消失）
    /// - `msg:<message_id>`：降级路径——整卡 im patch（现有行为，含折叠面板）
    async fn update_card(
        &self,
        _conv: &ConvId,
        handle: &str,
        card: &OutboundCard,
        _hint: &ReplyHint,
    ) -> Result<()> {
        self.with_token(|token| async move {
            if let Some(card_id) = handle.strip_prefix("card:") {
                match &card.terminal {
                    CardTerminal::Running => {
                        let content = stream_body_md(&card.text, &card.tool_calls);
                        let seq = self.next_card_seq(card_id).await;
                        match patch_card_element(&token, card_id, "md_body", &content, seq).await {
                            // 流式超时（200850）：服务端已自动关流式，长任务 Running 期会触发。
                            // 自愈：重开 streaming_mode 后重试一次（sequence 继续递增）。
                            Err(e) if e.to_string().contains("code=200850") => {
                                warn!(target: "feishu", card_id, "流式超时，重开 streaming_mode 后重试");
                                let settings = serde_json::json!({
                                    "config": { "streaming_mode": true }
                                })
                                .to_string();
                                let seq2 = self.next_card_seq(card_id).await;
                                patch_card_settings(&token, card_id, &settings, seq2).await?;
                                let seq3 = self.next_card_seq(card_id).await;
                                patch_card_element(&token, card_id, "md_body", &content, seq3).await
                            }
                            other => other,
                        }
                    }
                    CardTerminal::Done | CardTerminal::Error(_) => {
                        let err = match &card.terminal {
                            CardTerminal::Error(e) => Some(e.as_str()),
                            _ => None,
                        };
                        let content = stream_body_final(&card.text, &card.tool_calls, err);
                        let seq = self.next_card_seq(card_id).await;
                        let element =
                            patch_card_element(&token, card_id, "md_body", &content, seq).await;
                        // 关闭流式（光标消失）；sequence 与 element PATCH 共用递增。
                        let settings =
                            serde_json::json!({ "config": { "streaming_mode": false } }).to_string();
                        let seq2 = self.next_card_seq(card_id).await;
                        patch_card_settings(&token, card_id, &settings, seq2).await?;
                        element
                    }
                }
            } else if let Some(message_id) = handle.strip_prefix("msg:") {
                let card_json = render_card(card);
                patch_card(&self.core_config, &token, message_id, &card_json).await
            } else {
                Err(CoreError::Platform(
                    PLATFORM,
                    format!("非法卡片句柄: {handle}"),
                ))
            }
        })
        .await
    }

    fn name(&self) -> &'static str {
        PLATFORM
    }
}

// ---------------------------------------------------------------------------
// 单测：纯逻辑，不连真机 WS / HTTP。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::ReceiveIdKind;

    /// 构造一个 p2p 文本事件 payload bytes。
    fn mk_p2p_payload(event_id: &str, open_id: &str, text: &str) -> Vec<u8> {
        let content = format!("{{\"text\":\"{text}\"}}");
        serde_json::json!({
            "header":{"event_id":event_id,"event_type":"im.message.receive_v1"},
            "event":{
                "sender":{"sender_id":{"open_id":open_id}},
                "message":{"message_type":"text","content":content,"chat_type":"p2p"}
            }
        })
        .to_string()
        .into_bytes()
    }

    #[tokio::test]
    async fn drain_drops_duplicate_event_id() {
        // 同 event_id 的重复事件应被滑动窗口去重丢弃。
        let (inbound_msg_tx, mut inbound_msg_rx) = mpsc::channel::<InboundMessage>(8);
        let (payload_tx, payload_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let dedup = Dedup::default();
        let tx = inbound_msg_tx;
        let _handle = tokio::spawn(async move {
            let mut payload_rx = payload_rx;
            while let Some(payload) = payload_rx.recv().await {
                if let Some((msgid, msg, _)) = parse_message_event(&payload) {
                    if !dedup.check(&msgid) {
                        continue;
                    }
                    if tx.send(msg).await.is_err() {
                        break;
                    }
                }
            }
        });

        // 同 event_id 发两次 → 第二次去重。
        payload_tx
            .send(mk_p2p_payload("evt_1", "ou_alice", "hi"))
            .unwrap();
        payload_tx
            .send(mk_p2p_payload("evt_1", "ou_alice", "hi"))
            .unwrap();

        let first = inbound_msg_rx.recv().await.expect("第一条应入队");
        assert_eq!(first.conv_id.0, "feishu:ou_alice");
        assert_eq!(first.text.as_deref(), Some("hi"));
        // 给 drain 处理第二帧的时间，再断言无第二条入队。
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            inbound_msg_rx.try_recv().is_err(),
            "重复 event_id 应被去重，不应入队"
        );
    }

    #[tokio::test]
    async fn drain_parses_payload_into_inbound() {
        let (inbound_msg_tx, mut inbound_msg_rx) = mpsc::channel::<InboundMessage>(8);
        let (payload_tx, payload_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        let tx = inbound_msg_tx;
        let _handle = tokio::spawn(async move {
            let mut payload_rx = payload_rx;
            while let Some(payload) = payload_rx.recv().await {
                if let Some((_msgid, msg, _)) = parse_message_event(&payload) {
                    if tx.send(msg).await.is_err() {
                        break;
                    }
                }
            }
        });

        payload_tx
            .send(mk_p2p_payload("evt_2", "ou_bob", "hello"))
            .unwrap();
        let msg = inbound_msg_rx.recv().await.unwrap();
        assert_eq!(msg.conv_id, ConvId("feishu:ou_bob".into()));
        assert_eq!(msg.sender.0, "ou_bob");
        assert_eq!(msg.text.as_deref(), Some("hello"));
    }

    #[test]
    fn conv_roundtrip() {
        let (id, kind) = receive_target_from_conv(&ConvId("feishu:ou_abc".into())).unwrap();
        assert_eq!(id, "ou_abc");
        assert_eq!(kind, ReceiveIdKind::OpenId);
    }

    // 静态断言 FeishuPlatform 实现 Platform 且 name 正确。
    fn _name_check(p: &FeishuPlatform) -> &'static str {
        p.name()
    }
    #[allow(dead_code)]
    fn _ensure_platform_trait(_: &dyn Platform) {}

    #[test]
    fn unused_import_guard() {
        // 保持导入被使用，防止编译告警。
        let _ = ConvId("x".into());
    }
}
