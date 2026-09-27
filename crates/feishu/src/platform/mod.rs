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
//!
//! T19 结构拆分：本模块只留构造/后台任务编排 + Platform trait impl + 发送原语；
//! per-conv 状态与媒体落盘见 [`state`]（drain/ask/outbox 后续批次拆出）。

mod ask;
mod drain;
mod outbox;
mod state;

#[cfg(test)]
mod testutil;

use ask::{sender_opt_of, AskRender, PendingAskCard};
use state::{housekeeping_loop, ConvState, HousekeepingMaps};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::warn;

use imagent_core::{
    command_card_fallback_text, split_message, CardButton, CardTerminal, ConvId, CoreError, Dedup,
    InboundMessage, JoinedChat, MediaRef, OutboundCard, Platform, ReplyHint, Result,
    CARD_HANDLE_LOST,
};

use open_lark::{Config, CoreConfig};

use crate::card::{
    mask_emails, render_card, render_command_card, render_config_form_card, render_permission_card,
    render_permission_card_cancelled, render_stream_init_card, stream_body_final, stream_body_md,
};
use crate::client::{
    create_card_entity, fetch_token, is_card_not_exist_err, is_rate_limited_err, list_joined_chats,
    patch_card, patch_card_element, patch_card_settings, reply_comment, reply_comment_nodes,
    reply_message, send_card_msg, send_file_msg, send_image_msg, send_text_msg, upload_file,
    upload_image, FeishuWsClient,
};
use crate::proto::{
    comment_target_from_conv, receive_target_from_conv, thread_target_from_conv, ReceiveIdKind,
    COMMENT_CONV_PREFIX,
};

/// 平台名常量。
const PLATFORM: &str = "feishu";
/// 飞书单条文本消息 content 上限（保守值，留余量；精确阈值查官方文档）。
const FEISHU_TEXT_MAX: usize = 28_000;
/// 评论线程回复的分片阈值（字符）。评论回复 API 的内容上限与 im 消息不同（更小，
/// 离线无法精确确认——按评论场景普遍几千字符的量级取 3000 字符保守值，**待真机
/// 校准**：超限报错时再下调）。
const FEISHU_COMMENT_TEXT_MAX: usize = 3_000;
/// `tenant_access_token` 有效期 2h（7200s），距过期 < 10min（即 elapsed >= 110min）则刷新。
const TOKEN_TTL: Duration = Duration::from_secs(110 * 60);

pub struct FeishuPlatform {
    /// per-conv 会话状态（v1.18 review「ConvState 收敛」，见 [`ConvState`]）。
    conv_states: Arc<Mutex<HashMap<String, ConvState>>>,
    /// 发消息用配置（HTTP OpenAPI + 取 token）。
    core_config: Arc<CoreConfig>,
    app_id: String,
    app_secret: String,
    /// token 缓存：`(token, fetched_at)`，elapsed >= TOKEN_TTL 则刷新。
    token: Arc<RwLock<Option<(String, Instant)>>>,
    /// CardKit 卡片的 sequence 计数（element/settings PATCH 共用，per card_id 严格递增）。
    card_seqs: Arc<Mutex<HashMap<String, i64>>>,
    /// P8-1：已上屏的 footer 文案缓存（per card_id）——分阶段 footer 只在**变化时**
    /// patch（思考中→调用工具→输出中），节流 tick 间内容相同则跳过，不浪费调用。
    card_footers: Arc<Mutex<HashMap<String, String>>>,
    /// bot 对用户消息的表情标注状态：om_ 消息 id → 当前 reaction_id（终态翻转
    /// 时先删旧表情再打新表情；仅内存态，重启后旧表情滞留无害——新一轮会重打）。
    msg_reactions: Arc<Mutex<HashMap<String, String>>>,
    /// managed 流式卡的 card_id → 平台消息 id（om_）：终态整卡 patch 用
    /// （CardKit 句柄只有实体 id，im PATCH 需要消息 id；send 时记录）。
    managed_card_msgs: Arc<Mutex<HashMap<String, String>>>,
    /// `/reconnect` 强制重连信号（与 WS run task 共享，P4-7）。
    reconnect: Arc<tokio::sync::Notify>,
    /// 已解析的入站消息 channel，`recv` 直接 await。
    inbound_rx: Arc<Mutex<mpsc::Receiver<InboundMessage>>>,
    /// pending 询问卡登记（多卡并存）：request_id → 卡片信息。
    /// cancel/resolve 按 request_id 精确收敛；`cancel_all_permission_asks` 按 conv 遍历。
    pending_asks: Arc<Mutex<HashMap<String, PendingAskCard>>>,
    /// P6-1：群消息 @bot 过滤策略（与 drain task 共享，`/config` 热切换）。
    mention_policy: Arc<RwLock<crate::proto::MentionPolicy>>,
    /// 审批/问题卡自动拒绝倒计时的真实值（core `permission_ask_timeout_secs`，
    /// 构造注入——卡片 note 文案与实际超时行为一致，不再硬编码 5 分钟）。
    ask_timeout_secs: u64,
    /// 出站 im 文本分片上限：`min(config.message_max_len, FEISHU_TEXT_MAX)`
    /// （config 未设 = 仅协议上限）。
    text_split_max: usize,
    /// 评论回复分片上限：`min(config.message_max_len, FEISHU_COMMENT_TEXT_MAX)`。
    comment_split_max: usize,
    /// Wave B-4：免打扰时段（config `quiet_hours` 解析产物；None = 不启用）——
    /// buzz 类加急提醒（send_urgent_text）在窗口内降级为普通消息（不加 buzz
    /// 字段），只影响加急不影响内容。本地时区判定（chrono Local）。
    quiet_hours: Option<imagent_core::QuietHours>,
    /// v1.21 per-conv 发送令牌桶速率（消息创建/秒；0 = 关闭）。主动预算出站
    /// 频率——把「挨 429 再被动退避」翻转为「不触发 429」。
    send_rps: f64,
    /// v1.24 卡片 UX：审批/问题卡到达即加急（config feishu_urgent_on_ask）。
    urgent_on_ask: bool,
    /// per-conv 令牌桶状态（粗上限清理，见 [`Self::acquire_send_slot`]）。
    send_budget: Arc<Mutex<HashMap<String, SendBucket>>>,
}

/// v1.21：per-conv 发送令牌桶（令牌数 + 上次回填时刻）。
#[derive(Clone, Copy)]
struct SendBucket {
    tokens: f64,
    last: Instant,
}

impl FeishuPlatform {
    /// 构造并后台 spawn：① WS client run task（收事件 + 重连）；
    /// ② drain task（payload → `parse_message_event` → Dedup → inbound channel）。
    ///
    /// P6-1：`require_mention_in_group` = config `feishu_require_mention_in_group`
    /// （默认 true）——群消息须 @bot 才处理；p2p 不受限。
    ///
    /// - `message_max_len`：config `message_max_len`（None = 不按配置分片，仅用
    ///   平台协议上限）；send_text 各分片阈值与它取 min（见 `text_split_max`）。
    /// - `ask_timeout_secs`：config `permission_ask_timeout_secs`——审批/问题卡
    ///   倒计时 note 的真实值（与 core 的实际超时预算同源）。
    /// - `quiet_hours`（Wave B-4）：config `quiet_hours` 解析产物——buzz 加急
    ///   提醒的免打扰降级窗口。
    /// - `thread_active_window_secs`（Wave B-8）：话题免 @ 窗口（0 = 关闭）。
    /// - `asr_enabled`（W3-1）：语音转文字开关（config `feishu_asr_enabled`。
    ///   关闭时语音消息回退为提示，不调 speech_to_text）。
    /// - `group_context_messages`（T10）：群聊上下文注入条数（config
    ///   `feishu_group_context_messages`，0 = 关闭）——群消息触发轮次时拉本群
    ///   最近 N 条消息前置进 prompt（fail-soft）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        app_id: String,
        app_secret: String,
        base_url: String,
        require_mention_in_group: bool,
        message_max_len: Option<usize>,
        ask_timeout_secs: u64,
        quiet_hours: Option<imagent_core::QuietHours>,
        thread_active_window_secs: u64,
        asr_enabled: bool,
        outbox: Option<imagent_store::Store>,
        send_rps: f64,
        urgent_on_ask: bool,
        group_context_messages: usize,
    ) -> Result<Self> {
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
                // M1（code-review v8）：SDK HTTP 请求超时——缺省 None 时连接黑洞
                // 会把 token 刷新（持写锁）乃至全进程发送永久挂起。
                .req_timeout(Duration::from_secs(30))
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
        // （消息类）媒体下载落盘 → inbound channel。循环本体见 drain.rs（T19 拆分）。
        let (inbound_msg_tx, inbound_msg_rx) = mpsc::channel::<InboundMessage>(64);
        // token Arc 须在 spawn 前创建：drain task 下载媒体需取 token（发送/接收共用
        // 同一 lazy 刷新缓存，见 fetch_cached_token）。
        let token: Arc<RwLock<Option<(String, Instant)>>> = Arc::new(RwLock::new(None));
        // v1.23 说话人归属：open_id → 展示名缓存（contact 懒解析 + 失败负缓存）。
        let user_names: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
        let user_name_failed: Arc<Mutex<HashMap<String, Instant>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // P5-8：bot 自身 open_id 懒取缓存（@bot 过滤用；open_id 随应用固定，
        // 进程内取一次。取不到时 parse_comment_event 退化为弱过滤）。
        let bot_open_id: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
        // P6-1：群消息 @bot 过滤策略——共享句柄（`/config require_mention`
        // 热切换对下一消息生效；重启回 config 值）。
        let mention_policy: Arc<RwLock<crate::proto::MentionPolicy>> =
            Arc::new(RwLock::new(crate::proto::MentionPolicy {
                require_mention_in_group,
            }));
        // pending 询问卡登记：drain task 也要查（过期询问的按钮点击反馈，见
        // drain 内 card.action 分支）——先建后共享给 Self。
        let pending_asks: Arc<Mutex<HashMap<String, PendingAskCard>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // v1.18 review「ConvState 收敛」：原 9 张 conv 键表（发起者/评论锚/
        // 回复锚/最近入站/话题活跃/……）收敛为单表，drain 与发送侧共享一份。
        let conv_states: Arc<Mutex<HashMap<String, ConvState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // v1.18 迭代（housekeeping）：per-conv 状态表粗上限淘汰（ConvState
        // 收敛后只剩这一张表）。
        tokio::spawn(housekeeping_loop(HousekeepingMaps {
            conv_states: conv_states.clone(),
        }));
        // Wave B-8：话题免 @ 窗口（config 注入；0 = 关闭）。T10：群聊上下文注入
        // 条数（config 注入；0 = 关闭）。W3-1：语音转文字开关（drain 侧消费）。
        tokio::spawn(drain::run(drain::DrainContext {
            payload_rx,
            inbound_tx: inbound_msg_tx,
            dedup: Dedup::default(),
            core_config: core_config.clone(),
            app_id: app_id.clone(),
            app_secret: app_secret.clone(),
            token: token.clone(),
            outbox: outbox.clone(),
            user_names: user_names.clone(),
            user_name_failed: user_name_failed.clone(),
            bot_open_id: bot_open_id.clone(),
            mention_policy: mention_policy.clone(),
            pending_asks: pending_asks.clone(),
            conv_states: conv_states.clone(),
            thread_active_window: thread_window_of(thread_active_window_secs),
            group_context_messages,
            asr_enabled,
        }));

        let platform = Self {
            core_config,
            app_id,
            app_secret,
            token,
            card_seqs: Arc::new(Mutex::new(HashMap::new())),
            card_footers: Arc::new(Mutex::new(HashMap::new())),
            conv_states: conv_states.clone(),
            msg_reactions: Arc::new(Mutex::new(HashMap::new())),
            managed_card_msgs: Arc::new(Mutex::new(HashMap::new())),
            reconnect,
            inbound_rx: Arc::new(Mutex::new(inbound_msg_rx)),
            pending_asks,

            mention_policy,

            ask_timeout_secs,
            quiet_hours,

            text_split_max: message_max_len
                .unwrap_or(FEISHU_TEXT_MAX)
                .min(FEISHU_TEXT_MAX),
            comment_split_max: message_max_len
                .unwrap_or(FEISHU_COMMENT_TEXT_MAX)
                .min(FEISHU_COMMENT_TEXT_MAX),
            send_rps,
            urgent_on_ask,
            send_budget: Arc::new(Mutex::new(HashMap::new())),
        };
        // v1.21 outbox 泵：每 10s 拉到期行重发（feishu_text），指数退避
        // 15s→1h 封顶，成功删行、超 OUTBOX_MAX_ATTEMPTS 放弃并 error 留痕。
        if let Some(store) = outbox {
            let cfg = platform.core_config.clone();
            let token_lock = platform.token.clone();
            let aid = platform.app_id.clone();
            let sec = platform.app_secret.clone();
            tokio::spawn(async move {
                outbox::outbox_pump(store, cfg, token_lock, aid, sec).await;
            });
        }
        Ok(platform)
    }

    /// v1.21 per-conv 发送令牌桶：消息创建前取一个发送名额（速率
    /// `send_rps`/秒，容量 = max(1, rps)，约 1s 突发）。等待上限 2s——超时
    /// 放行（预算是主动平滑不是硬闸，429 自愈链路仍兜底）。0 = 关闭。
    async fn acquire_send_slot(&self, conv: &str) {
        if self.send_rps <= 0.0 {
            return;
        }
        const WAIT_CAP_MS: u64 = 2_000;
        const POLL_MS: u64 = 20;
        const BUDGET_MAP_CAP: usize = 4096;
        let deadline = Instant::now() + Duration::from_millis(WAIT_CAP_MS);
        loop {
            {
                let mut m = self.send_budget.lock().await;
                // 粗上限：桶表只随 conv 数增长，超限整体清空（桶满后按速率
                // 重建，瞬时限流无害）。
                if m.len() > BUDGET_MAP_CAP {
                    m.clear();
                }
                let now = Instant::now();
                let cap = self.send_rps.max(1.0);
                let b = m.entry(conv.to_string()).or_insert(SendBucket {
                    tokens: cap,
                    last: now,
                });
                let elapsed = now.duration_since(b.last).as_secs_f64();
                if elapsed > 0.0 {
                    b.tokens = (b.tokens + elapsed * self.send_rps).min(cap);
                    b.last = now;
                }
                if b.tokens >= 1.0 {
                    b.tokens -= 1.0;
                    return;
                }
            }
            if Instant::now() >= deadline {
                return; // 预算等待超时：放行（429 被动退避仍兜底）
            }
            tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
        }
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
    /// [`crate::client::is_token_invalid_err`]）→ 清缓存重取后再试一次。
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
            Err(e) if crate::client::is_token_invalid_err(&e) => {
                warn!(target: "feishu", error = %e, "token 失效错误码，清缓存强制刷新后重试一次");
                self.invalidate_token().await;
                let fresh = self.get_token().await?;
                f(fresh).await
            }
            other => other,
        }
    }

    /// 取该 card_id 的下一个 sequence（严格递增；element 与 settings PATCH 共用）。
    /// v1.21 review：终态清理失败路径（Failed/HandleLost 之外的残留）无上限——
    /// 加与 managed_card_msgs 同款粗上限兜底（clear 后 seq 从 0 重起有 300317
    /// 自愈兜底，可接受）。
    async fn next_card_seq(&self, card_id: &str) -> i64 {
        const CARD_SEQ_CAP: usize = 2048;
        let mut m = self.card_seqs.lock().await;
        if m.len() >= CARD_SEQ_CAP {
            m.clear();
            warn!(target: "feishu", "card_seqs 超粗上限（{CARD_SEQ_CAP}），整体清空兜底（seq 经 300317 自愈重置）");
        }
        let entry = m.entry(card_id.to_string()).or_insert(0);
        *entry += 1;
        *entry
    }

    /// 降级路径：发 raw 卡片消息（content=卡片 JSON），句柄 `msg:<message_id>`。
    ///
    /// managed 路径（create entity）失败时回退到整卡 im patch——体验同旧版，
    /// 不依赖 `cardkit:card:write` 权限。Wave B-5：群 conv 带发起者标注行；
    /// Wave B-6：有锚点时 reply 引用发起消息（见 [`Self::send_interactive_anchored`]）。
    async fn send_card_raw(
        &self,
        conv_id: &str,
        receive_id: &str,
        kind: ReceiveIdKind,
        card: &OutboundCard,
        token: &str,
        sender: Option<&str>,
    ) -> Result<Option<String>> {
        let card_json = render_card(card, conv_id, sender);
        let mid = self
            .send_interactive_anchored(
                token,
                &ConvId(conv_id.to_string()),
                receive_id,
                kind,
                &card_json,
            )
            .await?;
        Ok(mid.map(|m| format!("msg:{m}")))
    }

    /// managed（`card:` 句柄）卡片的 patch 主体，供 [`Self::update_card`] 与
    /// 300317 自愈重试共用。
    ///
    /// P8-1：Running 期 footer 按阶段（思考中/调用工具/输出中）patch，经
    /// `card_footers` 缓存去重——内容不变不发；终态收敛成 完成/出错/已中断。
    /// P8-2：`stub = true`（终态结果下沉）时终态正文用指针 stub 替代全文——
    /// 全文由调用方以新卡重发在下方。
    /// update_card 阶段 C（v1.18 review 状态机化抽离，行为与原内联块一致）：
    /// 终态 patch 失败（超限 200860/230099 等）时原卡停在「思考中」——最小化
    /// 终态卡重试一次保证卡片必然收敛。R1（code-review v9）：收敛成功也上抛
    /// 加工后的原错误（触发 core P5-11 纯文本补发）；再失败维持原错误。
    async fn minimize_terminal_card(
        &self,
        handle: &str,
        done: bool,
        original: CoreError,
    ) -> PatchOutcome {
        let err_text = original.to_string();
        let minimal = crate::card::render_overflow_terminal_card(done);
        // 重试目标：msg: 句柄直用；card: 句柄经映射表换消息 id。
        let target_mid: Option<String> = match handle.strip_prefix("msg:") {
            Some(m) => Some(m.to_string()),
            None => match handle.strip_prefix("card:") {
                Some(cid) => self.managed_card_msgs.lock().await.get(cid).cloned(),
                None => None,
            },
        };
        let retry = match target_mid {
            Some(mid) => {
                self.with_token(move |t| {
                    let minimal = minimal.clone();
                    let mid = mid.clone();
                    async move { patch_card(&self.core_config, &t, &mid, &minimal).await }
                })
                .await
            }
            None => Err(CoreError::Platform(
                PLATFORM,
                "终态重试无目标消息 id".into(),
            )),
        };
        match retry {
            Ok(()) => {
                warn!(target: "feishu", error = %err_text, "终态 patch 失败，最小卡已收敛；上抛原错误触发 core 纯文本补发");
                PatchOutcome::Minimized(CoreError::Platform(
                    PLATFORM,
                    format!("{err_text}（终态卡已收敛为最小形态，完整内容由文本消息补发）"),
                ))
            }
            Err(_) => PatchOutcome::Failed(original),
        }
    }

    /// update_card 阶段 A（token 作用域内的主 patch，v1.18 review 状态机化抽离）：
    /// card: 句柄 →〔终态且未下沉：im-patch 整卡快路径（无映射降级 managed）〕
    /// → managed element 流式 + 300317 序列重置重试；msg: 句柄 → 整卡/stub 重渲染；
    /// 其余 → 非法句柄。行为与抽离前的闭包逐分支一致。
    async fn patch_any_handle(
        &self,
        token: &str,
        conv: &ConvId,
        handle: &str,
        card: &OutboundCard,
        wants_buried: bool,
    ) -> Result<()> {
        if let Some(card_id) = handle.strip_prefix("card:") {
            // 真机校准（2026-08）：终态且未下沉时改**整卡 im patch**
            // （render_card 折叠面板布局）——统一视觉：此前 managed 终态把
            // 工具轨迹/思考过程内联进 md_body（长卡），而结果下沉新卡是折叠
            // 面板，两形态不一致（用户反馈折叠更好）。Running 仍走 managed
            // element 流式（打字机/节流/300317 自愈语义不变）。无映射
            // （重启后）退回 managed 终态 patch（内联形态，可接受降级）。
            if !wants_buried && !matches!(card.terminal, CardTerminal::Running) {
                // v1.18 review：先 let 绑定再判断——edition 2021 的 if-let
                // scrutinee 临时值（MutexGuard）存活到整个 if-let 结束，
                // 此前 managed_card_msgs 锁跨 patch 网络调用（30s 超时 +
                // 限流重试）长达半分钟，期间其它 conv 的 send_card 登记/
                // 终态查询全部排队。
                let mid = self.managed_card_msgs.lock().await.get(card_id).cloned();
                if let Some(message_id) = mid {
                    let sender = self.last_sender(&conv.0).await;
                    let card_json = render_card(
                        card,
                        &conv.0,
                        (!sender.is_empty()).then_some(sender).as_deref(),
                    );
                    let res = patch_card(&self.core_config, token, &message_id, &card_json).await;
                    if res.is_ok() {
                        // 终态清理（与 patch_managed 终态分支同语义——本分支
                        // 提前 return 会绕过那边的清理，防 per-card 状态泄漏）。
                        self.card_seqs.lock().await.remove(card_id);
                        self.card_footers.lock().await.remove(card_id);
                    }
                    return res;
                }
            }
            match self.patch_managed(token, card_id, card, wants_buried).await {
                // 300317（sequence 落后）自愈（真机校准）：重启后内存计数器归零，
                // 但旧卡片的 server 序号已推进（孤儿扫描接管、同进程异常路径）
                // ——把该卡计数器重置为时间戳级（必然大于 server 序号）整段重试。
                Err(e) if e.to_string().contains("300317") => {
                    warn!(target: "feishu", card_id, "sequence 落后（300317），重置计数器后重试");
                    // sequence 是 int32：用**秒级**时间戳（~1.8e9 < 2^31，
                    // 2038 年前安全）；毫秒会溢出被 9499 拒（真机踩过）。
                    // 秒级值必然大于服务端已用的小序号，满足严格递增。
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(1_000_000_000);
                    // CAS（v1.18 review）：原写法直接覆写——并发 300317 同时
                    // 重置会写出相同 seq（两个重试再撞 300317）。只增不降且
                    // +1，并发重置也严格递增（int32 上限 2038 年，见上）。
                    let mut seqs = self.card_seqs.lock().await;
                    let v = seqs.entry(card_id.to_string()).or_insert(now);
                    *v = (*v).max(now).saturating_add(1);
                    self.patch_managed(token, card_id, card, wants_buried).await
                }
                other => other,
            }
        } else if let Some(message_id) = handle.strip_prefix("msg:") {
            // Wave B-5：整卡重渲染带发起者标注行（群 conv）。
            let sender = self.last_sender(&conv.0).await;
            let card_json = if wants_buried {
                crate::card::render_stub_card(card)
            } else {
                render_card(
                    card,
                    &conv.0,
                    (!sender.is_empty()).then_some(sender).as_deref(),
                )
            };
            patch_card(&self.core_config, token, message_id, &card_json).await
        } else {
            Err(CoreError::Platform(
                PLATFORM,
                format!("非法卡片句柄: {handle}"),
            ))
        }
    }

    async fn patch_managed(
        &self,
        token: &str,
        card_id: &str,
        card: &OutboundCard,
        stub: bool,
    ) -> Result<()> {
        match &card.terminal {
            CardTerminal::Running => {
                let content = stream_body_md(card);
                let seq = self.next_card_seq(card_id).await;
                // 限流丢帧策略（安全批次）：element PATCH 用**不重试**变体——429 重试
                // 会 sleep 阻塞流式主循环（agent chunk 消费被卡最多 3.5s/次）；改为
                // 丢弃本帧返回 Ok（内容在累积文本里，下个节流窗整帧重发），只有非
                // 限流错误才走自愈/上抛。
                let patched = match patch_card_element(token, card_id, "md_body", &content, seq)
                    .await
                {
                    Err(e) if is_rate_limited_err(&e) => {
                        tracing::warn!(target: "feishu", card_id, "element patch 限流，丢弃本帧（下个节流窗再发）");
                        return Ok(());
                    }
                    // 流式超时（200850）：服务端已自动关流式，长任务 Running 期会触发。
                    // 自愈一级：重开 streaming_mode 后重试一次（sequence 继续递增）。
                    Err(e) if e.to_string().contains("code=200850") => {
                        warn!(target: "feishu", card_id, "流式超时，重开 streaming_mode 后重试");
                        let settings =
                            serde_json::json!({ "config": { "streaming_mode": true } }).to_string();
                        let seq2 = self.next_card_seq(card_id).await;
                        let reopen = patch_card_settings(token, card_id, &settings, seq2).await;
                        if let Err(e) = reopen {
                            if is_rate_limited_err(&e) {
                                return Ok(()); // 限流：同丢帧策略。
                            }
                            return Err(e);
                        }
                        let seq3 = self.next_card_seq(card_id).await;
                        match patch_card_element(token, card_id, "md_body", &content, seq3).await {
                            // 自愈二级（升级兜底）：重开流式后仍 200850——CardKit 无
                            // 「重建实体」API（离线确认，**待真机校准**），退化为
                            // 关流式 + 全量 raw patch 一次（无打字机但内容不丢帧）。
                            Err(e2) if e2.to_string().contains("code=200850") => {
                                warn!(target: "feishu", card_id, "重开流式仍超时，退化关闭流式后 raw patch");
                                let off = serde_json::json!({
                                    "config": { "streaming_mode": false }
                                })
                                .to_string();
                                let seq4 = self.next_card_seq(card_id).await;
                                let _ = patch_card_settings(token, card_id, &off, seq4).await;
                                let seq5 = self.next_card_seq(card_id).await;
                                patch_card_element(token, card_id, "md_body", &content, seq5).await
                            }
                            other => other,
                        }
                    }
                    other => other,
                };
                patched?;
                // 分阶段 footer（best-effort，失败不影响正文流）+ P10 排队提示
                //（入队状态由 CardSession 每次 patch 拉取，随 chunk 刷新）。
                let footer = crate::card::running_footer(
                    card.phase,
                    card.queued_hint.as_deref(),
                    card.run_secs,
                );
                self.patch_footer_if_changed(token, card_id, &footer).await;
                Ok(())
            }
            CardTerminal::Done | CardTerminal::Error(_) => {
                let err = match &card.terminal {
                    CardTerminal::Error(e) => Some(e.as_str()),
                    _ => None,
                };
                let content = if stub {
                    crate::card::stub_body(card.tool_calls.len(), err)
                } else {
                    stream_body_final(card, err)
                };
                let seq = self.next_card_seq(card_id).await;
                // 终态用不重试变体：429 不睡（上抛 Err 由 core P5-11 降级纯文本补
                // 结论——终态内容不能等下个节流窗，丢帧语义只属 Running 流式帧）。
                let element = patch_card_element(token, card_id, "md_body", &content, seq).await;
                // footer 收敛（真机校准 UX）：初始卡的「🧠 思考中…」在终态
                // 换成 完成/出错/已中断——否则任务结束后标识永远停在执行中。
                // 成功终态附本轮成本摘要 + 总耗时（Wave B-3：`✅ 已完成 · 30m ·
                // $0.012`，run_secs 为终态全量秒数）。失败终态：managed 路径
                // element PATCH 无法追加按钮组件（/doctor 按钮只在整卡渲染路径，
                // 见 render_card），以 footer 文案指引兜底（Wave B-11）。
                let footer = match err {
                    Some("已中断") => "⏹ 已中断".to_string(),
                    Some(_) => "❌ 出错 · 可发 /doctor 自检".to_string(),
                    None => crate::card::terminal_done_footer(
                        card.run_secs,
                        card.usage_display.as_deref(),
                    ),
                };
                self.patch_footer_if_changed(token, card_id, &footer).await;
                // 关闭流式（光标消失）；sequence 与 element PATCH 共用递增。
                let settings =
                    serde_json::json!({ "config": { "streaming_mode": false } }).to_string();
                let seq2 = self.next_card_seq(card_id).await;
                let res = patch_card_settings(token, card_id, &settings, seq2).await;
                // L1（code-review v8）：终态清理（与 im-patch 终态分支同语义）——
                // card_seqs/card_footers 每卡 2 条泄漏、无 cap 无过期；清理放在
                // settings patch 之后（失败也不致命：条目泄漏 ≠ 功能受损）。
                self.card_seqs.lock().await.remove(card_id);
                self.card_footers.lock().await.remove(card_id);
                res?;
                element
            }
        }
    }

    /// footer 变化才 patch（缓存命中跳过）；失败仅 warn（footer 是点缀，正文/终态
    /// 才是主流程）。同时管理 `card_footers` 缓存的写入与终态清理。
    /// v1.21 review：终态清理失败路径的 footer 条目无上限（card_seqs 同款），
    /// 写入侧加粗上限兜底。
    async fn patch_footer_if_changed(&self, token: &str, card_id: &str, footer: &str) {
        const CARD_FOOTER_CAP: usize = 2048;
        let changed = {
            let mut m = self.card_footers.lock().await;
            if m.len() >= CARD_FOOTER_CAP {
                m.clear();
                warn!(target: "feishu", "card_footers 超粗上限（{CARD_FOOTER_CAP}），整体清空兜底");
            }
            if m.get(card_id).map(String::as_str) == Some(footer) {
                false
            } else {
                m.insert(card_id.to_string(), footer.to_string());
                true
            }
        };
        if !changed {
            return;
        }
        let seq = self.next_card_seq(card_id).await;
        // 限流丢帧：footer 是点缀，不重试不阻塞；缓存条目回滚（否则本窗口内后续
        // 相同 footer 会被误判「已上屏」而跳过，内容永久丢失直到 footer 再变化）。
        if let Err(e) = patch_card_element(token, card_id, "md_footer", footer, seq).await {
            if is_rate_limited_err(&e) {
                tracing::warn!(target: "feishu", card_id, "footer patch 限流，丢帧并回滚缓存");
                self.card_footers.lock().await.remove(card_id);
            } else {
                tracing::warn!(target: "feishu", error = %e, "footer patch 失败（不影响主流程）");
            }
        }
    }
    /// conv 最近一次入站消息的 sender（轮次发起者近似——每 conv 轮次串行，询问
    /// 登记时取之，作群 conv 下的按钮点击者校验锚；无记录为空串=不校验）。
    /// 刷新会话最新卡片记录（card_tail，强提醒加急对象）。
    async fn note_card_tail(&self, conv_id: &str, msg_id: &str) {
        if msg_id.starts_with("om_") {
            let mut m = self.conv_states.lock().await;
            let st = m.entry(conv_id.to_string()).or_default();
            st.card_tail = Some(msg_id.to_string());
            // v1.23 review：发送侧活跃也是 LRU 活跃信号——长会话持续出卡但
            // 暂无入站时不该被驱逐（card_tail 丢失会把加急降级为普通文本）。
            st.last_touched = Instant::now();
        }
    }

    async fn last_sender(&self, conv_id: &str) -> String {
        // v1.23：优先本轮首条消息的 sender（dispatch 轮首锚定）——「最近
        // sender」会被运行中的插话者漂移。
        self.conv_states
            .lock()
            .await
            .get(conv_id)
            .and_then(|s| s.round_initiator.clone().or_else(|| s.sender.clone()))
            .unwrap_or_default()
    }

    /// Wave B-6：该 conv 的回复锚点（最近一条普通群消息的 message_id；无则 None）。
    async fn reply_anchor(&self, conv_id: &str) -> Option<String> {
        self.conv_states
            .lock()
            .await
            .get(conv_id)
            .and_then(|s| s.reply_anchor.clone())
            .filter(|a| !a.is_empty())
    }

    /// Wave B-4：当前是否处于免打扰时段（本地时区；未配置 = false）。
    fn in_quiet_hours(&self) -> bool {
        let Some(q) = self.quiet_hours else {
            return false;
        };
        use chrono::Timelike;
        let now = chrono::Local::now();
        let minute_of_day = now.hour() * 60 + now.minute();
        q.contains(minute_of_day)
    }

    /// Wave B-6：发 interactive 卡消息——有回复锚点（群 conv）优先 reply API
    /// 引用发起消息，失败/无锚点回退 create 到会话。返回 message_id。
    /// 两个用途：raw 卡片 JSON（降级/话题外的整卡）与 CardKit 实体引用
    /// （`{"type":"card","data":{"card_id":…}}`——reply 的 content 与 create 同构）。
    async fn send_interactive_anchored(
        &self,
        token: &str,
        conv: &ConvId,
        receive_id: &str,
        kind: ReceiveIdKind,
        content: &str,
    ) -> Result<Option<String>> {
        if let Some(anchor) = self.reply_anchor(&conv.0).await {
            match reply_message(&self.core_config, token, &anchor, "interactive", content).await {
                Ok(mid) => {
                    if let Some(m) = &mid {
                        self.note_card_tail(&conv.0, m).await;
                    }
                    return Ok(mid);
                }
                Err(e) => {
                    // 锚点消息可能已被撤回/删除：回退 create（卡片不能因此不发）。
                    warn!(
                        target: "feishu",
                        conv_id = %conv.0,
                        error = %e,
                        "reply 引用发起消息失败，回退普通发送"
                    );
                }
            }
        }
        let mid = send_card_msg(&self.core_config, token, receive_id, kind, content).await;
        if let Ok(Some(m)) = &mid {
            self.note_card_tail(&conv.0, m).await;
        }
        mid
    }

    /// P8-2：发送静态卡片（终态结果下沉用）：普通 conv 直接发，话题群 reply 进
    /// 原话题。Wave B-6：普通群优先 reply 引用发起消息（锚点）；Wave B-5：带
    /// 发起者标注行。发送失败如实上抛（调用方 warn——结果已在流式卡里兜底过一次）。
    async fn send_static_card(&self, conv: &ConvId, card_json: &str) -> Result<()> {
        if let Some((_chat, root_id)) = thread_target_from_conv(conv) {
            return self
                .with_token(|t| {
                    let root_id = root_id.clone();
                    let card_json = card_json.to_string();
                    async move {
                        reply_message(&self.core_config, &t, &root_id, "interactive", &card_json)
                            .await
                    }
                })
                .await
                .map(|_| ());
        }
        let (receive_id, kind) = receive_target_from_conv(conv)
            .ok_or_else(|| CoreError::Platform(PLATFORM, format!("非法 conv_id: {}", conv.0)))?;
        self.with_token(|t| {
            let receive_id = receive_id.clone();
            let card_json = card_json.to_string();
            let conv_clone = conv.clone();
            async move {
                self.send_interactive_anchored(&t, &conv_clone, &receive_id, kind, &card_json)
                    .await
            }
        })
        .await
        .map(|_| ())
    }

    /// send_text 实现（`buzz = true` 附加急字段；普通路径 false 与历史形态一致）。
    async fn send_text_opts(
        &self,
        conv: &ConvId,
        text: &str,
        _hint: &ReplyHint,
        buzz: bool,
    ) -> Result<()> {
        // P9-1：出站文本统一邮箱掩码——租户消息审计对裸邮箱回 400（含纯文本
        // 消息），流式/最终回复都会过这里。
        let text = &mask_emails(text);
        // P4-9：评论线程 conv → 回复云文档评论（每分片一条回复）。
        // 会话锚放宽批次：conv 只锚 file_token，回复目标 comment_id 优先取 drain
        // 登记的锚点表；存量 conv 的内嵌形态兜底。两者皆无（进程
        // 刚重启、锚点表为空）无法定位评论线程，如实报错。
        // T13：锚点按**轮次发起者**解析（A 的回答落回 A 的评论线程），发起者
        // 无记录回退最近一条评论——此前恒取「最新评论」，B 抢先评论会把 A 的
        // 回答回复到 B 的评论下（归属错乱）。
        if let Some((file_token, legacy_cid)) = comment_target_from_conv(conv) {
            let comment_id = self
                .conv_states
                .lock()
                .await
                .get(&conv.0)
                .and_then(|s| s.resolve_comment_anchor(s.round_initiator.as_deref()))
                .or(legacy_cid);
            let Some(comment_id) = comment_id else {
                return Err(CoreError::Platform(
                    PLATFORM,
                    "评论线程缺少回复目标（comment_id），无法回复".to_string(),
                ));
            };
            // 评论回复用独立更小阈值（FEISHU_COMMENT_TEXT_MAX 与 config
            // message_max_len 取 min，见 comment_split_max）；首片带「（共 N 段）」
            // 序标，长回复被拆分时用户可感知。
            let chunks: Vec<String> = split_message(text, self.comment_split_max);
            let total = chunks.len();
            for (i, chunk) in chunks.into_iter().enumerate() {
                let chunk = if i == 0 && total > 1 {
                    format!("（共 {total} 段）\n{chunk}")
                } else {
                    chunk
                };
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
        // P6-4：话题群 conv → 回复话题根消息（reply API 落回原话题，而非发新话题）。
        if let Some((_chat_id, root_id)) = thread_target_from_conv(conv) {
            let chunks: Vec<String> = split_message(text, self.text_split_max);
            let total = chunks.len();
            for (i, chunk) in chunks.into_iter().enumerate() {
                let content = serde_json::json!({ "text": chunk }).to_string();
                if let Err(e) = self
                    .with_token(|t| {
                        let root_id = root_id.clone();
                        let content = content.clone();
                        async move {
                            reply_message(&self.core_config, &t, &root_id, "text", &content).await
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
        let chunks: Vec<String> = split_message(text, self.text_split_max);
        let total = chunks.len();
        // Wave B-6：普通群 conv 有回复锚点时整段用 reply API 引用发起消息（最终
        // 回复锚回问题，多轮群聊里不再错位）；锚点失效（被撤回/删除）回退普通
        // 发送——内容不能因引用失败而丢。私聊无锚点，走原路径。
        // Wave B-4：buzz（加急）消息不走锚点——reply API 的 text content 加急
        // 字段未验证（**待真机校准**），加急走 create 路径保 buzz 字段生效。
        let anchor = if buzz {
            None
        } else {
            self.reply_anchor(&conv.0).await
        };
        for (i, chunk) in chunks.into_iter().enumerate() {
            // P5：同上——分片失败标注序号（此前中途 ? 退出，截断无标记）。
            if let Err(e) = self
                .with_token(|t| {
                    let receive_id = receive_id.clone();
                    let chunk = chunk.clone();
                    let anchor = anchor.clone();
                    async move {
                        if let Some(a) = anchor.as_deref() {
                            let content = serde_json::json!({ "text": chunk }).to_string();
                            if reply_message(&self.core_config, &t, a, "text", &content)
                                .await
                                .is_ok()
                            {
                                return Ok(());
                            }
                        }
                        send_text_msg(&self.core_config, &t, &receive_id, kind, &chunk, buzz).await
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
}

/// Wave B-8：config 秒数 → 话题免 @ 窗口时长（0 = 关闭豁免；纯函数便于单测）。
/// 默认值（30 分钟）在 core config 的 `default_feishu_thread_active_window_secs`。
fn thread_window_of(secs: u64) -> Duration {
    if secs == 0 {
        Duration::ZERO
    } else {
        Duration::from_secs(secs)
    }
}

/// per-conv 顺序泵的作业：就绪消息 / 媒体处理任务（等 join 后按序发出）/
/// 合并转发处理任务（Agent 转录分支产出消息，Fallback 分支在任务内直发提示）。
/// update_card 自愈管线的显式阶段结果（v1.18 review「update_card 状态机化」：
/// 此前四层自愈以嵌套 match + 后置 match 链叠在 160 行闭包里，层间移交只能
/// 通读推演；命名结果让每层职责与后续动作一目了然——Delivered 才走下沉重发，
/// Minimized 上抛原错触发 core 纯文本补发，HandleLost 附哨兵供 core 摘句柄）。
enum PatchOutcome {
    /// 主 patch 成功（Running 流式帧 / 终态整卡 im-patch / managed element）。
    Delivered,
    /// 原卡被删除/撤回——per-card 缓存已清，错误已附 CARD_HANDLE_LOST 哨兵。
    HandleLost(CoreError),
    /// 终态超限失败但最小终态卡已收敛——上抛加工后的原错误（R1 语义）。
    Minimized(CoreError),
    /// 未触发自愈/自愈无效的失败，原样上抛。
    Failed(CoreError),
}

impl PatchOutcome {
    fn into_result(self) -> Result<()> {
        match self {
            PatchOutcome::Delivered => Ok(()),
            PatchOutcome::HandleLost(e) | PatchOutcome::Minimized(e) | PatchOutcome::Failed(e) => {
                Err(e)
            }
        }
    }
}

/// v1.21 review：媒体上传读前大小预检——返回 Some(Err) 表示超限/不可访问，
/// 调用方在读文件前拒绝（防任意大文件整读进内存）。metadata 失败放行（交给
/// 后续 read 的真实错误路径——此处只拦「确定超限」的）。
async fn media_size_violation(url: &str) -> Option<CoreError> {
    match tokio::fs::metadata(url).await {
        Ok(meta) if meta.len() > crate::client::MEDIA_MAX_BYTES => Some(CoreError::Platform(
            PLATFORM,
            format!(
                "媒体文件 {} 大小 {}MB 超上限 {}MB，拒绝上传",
                url,
                meta.len() / (1024 * 1024),
                crate::client::MEDIA_MAX_BYTES / (1024 * 1024)
            ),
        )),
        _ => None,
    }
}

/// 过期询问的点击反馈（drain task 用）：向该 conv 回一条「已过期」文本——
/// 询问卡收敛（批准/拒绝/中断/超时）后按钮仍在卡上，用户迟点不应静默无响应。
/// 评论 conv 的过期文案与聊天场景同句（回评论线程）。
/// best-effort：发送失败仅 warn（提示丢失无害，core 的 miss 分支照旧兜底丢弃）。
/// 取当前 token：缓存命中（未过 TTL）则返回，否则 `fetch_token` 刷新并缓存。
///
/// 提成模块级自由函数——drain task 持有 `Arc<RwLock<…>>` 句柄而无 `&self`，无法调
/// [`FeishuPlatform::get_token`]，故抽出共用（与发送侧共享同一 lazy 缓存）。
/// P5：读锁快路径 + 写锁双检——此前每次都直接取写锁且跨网络调用（最坏 30s），
/// token 刷新期间所有发送/媒体下载被串行阻塞。
/// token 刷新 single-flight（v1.18 review）：抢到它的一方发网络请求，其余
/// 并发刷新者等它完成后走读锁快路径。此前写锁**跨网络**持有（tokio RwLock
/// 写偏好，fetch 30s 超时期间全平台发送/patch/下载在锁上排队，每 ~2h 刷新
/// 卡一次）；token 失效风暴时 N 个并发请求还各自独立重取（撞飞书签发频控）。
static TOKEN_REFRESH_MU: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    // v1.21 review（失败负缓存）：token 端点故障期间每个调用方在 single-flight
    // 门上排队、各自串行做一次 30s 超时的失败刷新（等待时间随人数线性放大，
    // 恢复瞬间集中重试）。失败结果短 TTL 负缓存：5s 内的后续调用直接复用上
    // 一次错误，不撞端点。
    static TOKEN_FAIL: std::sync::Mutex<Option<(String, Instant)>> = std::sync::Mutex::new(None);
    const TOKEN_FAIL_NEG_TTL: Duration = Duration::from_secs(5);
    // 刷新串行化：网络期间 token_lock 完全不被持有，发送方零阻塞。
    // v1.23 review：等待人数改 Drop guard——调用方 future 在等锁期间被取消
    //（dispatch 超时 drop 发送 future 是常态路径）时裸 inc/dec 会永久泄漏
    // 计数，指标假阳。
    struct WaiterGuard;
    impl Drop for WaiterGuard {
        fn drop(&mut self) {
            crate::metrics::METRICS.token_waiters.dec();
        }
    }
    crate::metrics::METRICS.token_waiters.inc();
    let _waiter = WaiterGuard;
    let _refresh_guard = TOKEN_REFRESH_MU.lock().await;
    drop(_waiter);
    // 双检：等刷新权期间可能已被前一个刷新者写回新 token。
    if let Some((token, fetched_at)) = token_lock.read().await.as_ref() {
        if fetched_at.elapsed() < TOKEN_TTL {
            return Ok(token.clone());
        }
    }
    {
        let fail = TOKEN_FAIL.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((err, at)) = fail.as_ref() {
            if at.elapsed() < TOKEN_FAIL_NEG_TTL {
                return Err(imagent_core::CoreError::Platform(
                    crate::client::PLATFORM,
                    format!("tenant token 刷新近期失败（{err}），负缓存窗口内直接复用错误"),
                ));
            }
        }
    }
    match fetch_token(core_config, app_id, app_secret).await {
        Ok(token) => {
            // 写回仅短暂持写锁（无网络），读锁等待者立即可见。
            *token_lock.write().await = Some((token.clone(), Instant::now()));
            *TOKEN_FAIL.lock().unwrap_or_else(|e| e.into_inner()) = None;
            Ok(token)
        }
        Err(e) => {
            *TOKEN_FAIL.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((e.to_string(), Instant::now()));
            Err(e)
        }
    }
}

#[async_trait]
impl Platform for FeishuPlatform {
    /// bot 对用户消息的表情标注：OnIt（在做了）→ DONE / CrossMark。
    /// emoji key 真机校准（2026-08）验证可用且**大小写敏感**（全大写报 231001）。
    /// 翻转 = 删旧表情 + 打新表情；删失败（过期/已撤回）仅 log，新表情照打。
    /// v1.26 权限自检：三级探测（token → bot 能力 → 消息读权限）+ 功能↔权限
    /// 对照表。消息读探测用合成 message_id 调 GET /im/v1/messages——按错误
    /// 形态分类：对象不存在类（230001/99991672/not exist）= 权限已通（API
    /// 放行了调用，只是对象没有）；无权限类 = 缺 scope。真实踩坑驱动（用户
    /// 三连问：引用/合并转发静默失败、权限概念混淆、开通后忘发布版本）。
    async fn doctor_probes(&self) -> Vec<String> {
        let mut out = Vec::new();
        // ① tenant token（凭据/网络）
        let token = match self.get_token().await {
            Ok(t) => {
                out.push("✅ 飞书 token 获取正常（凭据/网络连通）".into());
                t
            }
            Err(e) => {
                out.push(format!(
                    "⚠️ 飞书 token 获取失败：{e}（检查 app_id/secret 与网络）"
                ));
                return out;
            }
        };
        // ② bot 能力（GET /bot/v3/info）
        match crate::client::fetch_bot_open_id(&self.core_config, &token).await {
            Ok(_) => out.push("✅ 机器人能力正常（bot info 可读）".into()),
            Err(e) => out.push(format!(
                "⚠️ bot info 读取失败：{e}（开发者后台 → 应用能力 → 开启机器人）"
            )),
        }
        // ③ 消息读权限（im:message:readonly / im:message）
        match crate::client::fetch_message_raw(&self.core_config, &token, "om_doctor_probe").await {
            Ok(_) => out.push("✅ 消息读取权限正常".into()),
            Err(e) => {
                let es = e.to_string();
                let not_exist = es.contains("230001")
                    || es.contains("99991672")
                    || es.to_ascii_lowercase().contains("not exist");
                if not_exist {
                    out.push("✅ 消息读取权限正常（探测 id 不存在 = 调用被放行）".into());
                } else {
                    out.push(format!(
                        "⚠️ 消息读取疑似缺权限：{es}\n   → 开发者后台 → 权限管理 → 开通 im:message:readonly 并**创建新版本发布**（只勾选不发布不生效）。影响：引用上下文、合并转发转录"
                    ));
                }
            }
        }
        out.push("📋 功能↔权限对照：合并转发/引用上下文需 im:message:readonly；媒体下载需 im:resource；加急需 im:message.urgent:send；进群事件需订阅 im.chat.member.bot.added_v1（事件与回调页）".into());
        out
    }

    /// v1.23 发起者锚定：dispatch 在每轮首条消息分派前调用——本 conv 的
    /// 卡片发起者/按钮点击权锚定到轮次发起者（漂移修复见 last_sender）。
    async fn note_round_initiator(&self, conv: &ConvId, sender: &str) {
        if sender.is_empty() {
            return;
        }
        let mut m = self.conv_states.lock().await;
        let st = m.entry(conv.0.clone()).or_default();
        st.round_initiator = Some(sender.to_string());
        st.last_touched = Instant::now();
    }

    async fn react_to_message(
        &self,
        conv: &ConvId,
        source_msg_id: &str,
        reaction: imagent_core::MsgReaction,
    ) -> Result<()> {
        if !source_msg_id.starts_with("om_") {
            return Ok(()); // 合成消息（按钮回调等）无平台消息锚——no-op。
        }
        // L3（code-review v8）：排队 ⏳ 与 runner 👀 并发交错竞态兜底——打 ⏳
        // 前若该消息已有表情登记（runner 侧已接管），跳过（防同消息双表情、
        // ⏳ 在已完成消息上永久残留）。
        if matches!(reaction, imagent_core::MsgReaction::Queued)
            && self.msg_reactions.lock().await.contains_key(source_msg_id)
        {
            return Ok(());
        }
        let emoji = match reaction {
            imagent_core::MsgReaction::Queued => "OneSecond",
            imagent_core::MsgReaction::Processing => "OnIt",
            imagent_core::MsgReaction::Done => "DONE",
            imagent_core::MsgReaction::Failed => "CrossMark",
        };
        // 旧表情先删（翻转语义）：reaction_id 在则删，删失败不阻塞。
        let old = self.msg_reactions.lock().await.remove(source_msg_id);
        let old_del = old.map(|rid| {
            self.with_token(move |t| {
                let rid = rid.clone();
                async move {
                    crate::client::delete_reaction(&self.core_config, &t, source_msg_id, &rid).await
                }
            })
        });
        if let Some(fut) = old_del {
            if let Err(e) = fut.await {
                warn!(target: "feishu", error = %e, "旧表情删除失败（不阻塞新表情）");
            }
        }
        let rid = self
            .with_token(|t| async move {
                crate::client::create_reaction(&self.core_config, &t, source_msg_id, emoji).await
            })
            .await?;
        {
            let mut mr = self.msg_reactions.lock().await;
            if mr.len() > 1024 {
                // 粗上限（超量整体重置）：清后旧表情的 reaction_id 丢失，翻转时
                // 旧表情滞留 + 新表情叠加（视觉小瑕疵，可接受）。
                mr.clear();
            }
            mr.insert(source_msg_id.to_string(), rid);
        }
        let _ = conv; // conv 仅日志语义，保留签名对齐 trait。
        Ok(())
    }

    async fn recv(&self) -> Result<InboundMessage> {
        self.inbound_rx.lock().await.recv().await.ok_or_else(|| {
            CoreError::Platform(PLATFORM, "入站 channel 已关闭（client 已退出）".into())
        })
    }

    async fn send_text(&self, conv: &ConvId, text: &str, hint: &ReplyHint) -> Result<()> {
        // v1.21：消息创建走 per-conv 令牌桶（主动预算，防 429）。
        self.acquire_send_slot(&conv.0).await;
        self.send_text_opts(conv, text, hint, false).await
    }

    /// Wave B：加急（buzz）文本覆写——免打扰时段（quiet_hours，本地时区）降级
    /// 为普通消息（只去掉 buzz 字段，内容与投递不变，见 config 注释）。
    async fn send_urgent_text(&self, conv: &ConvId, text: &str, hint: &ReplyHint) -> Result<()> {
        if self.in_quiet_hours() {
            return self.send_text_opts(conv, text, hint, false).await;
        }
        // 真机校准（2026-08）：强提醒优先对**会话最新卡片**发应用内加急
        //（urgent_app）——卡直接弹通知、不产生额外文本消息（此前 buzz 文本
        // 与卡片流视觉割裂）。审批催办时最新卡即审批卡、完成提醒时即终态卡。
        // 无卡 / 无接收人 / 接口失败（权限缺失等）回退 buzz 文本（fail-soft）。
        let tail = self
            .conv_states
            .lock()
            .await
            .get(&conv.0)
            .and_then(|s| s.card_tail.clone());
        let sender = self.last_sender(&conv.0).await;
        if let (Some(mid), false) = (tail, sender.is_empty()) {
            match self
                .with_token(|t| {
                    let mid = mid.clone();
                    let sender = sender.clone();
                    async move {
                        crate::client::urgent_app_buzz(&self.core_config, &t, &mid, &sender).await
                    }
                })
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) => {
                    warn!(target: "feishu", error = %e, "应用内加急失败，回退 buzz 文本");
                }
            }
        }
        self.send_text_opts(conv, text, hint, true).await
    }

    /// Wave B：平台支持加急文本（text 消息体 buzz 字段）——core 据此决定长任务
    /// 完成强提醒是否发送。
    fn supports_urgent_text(&self) -> bool {
        true
    }

    async fn send_media(&self, conv: &ConvId, media: &MediaRef, _hint: &ReplyHint) -> Result<()> {
        // 评论线程分支（安全批次修复：此前评论 conv 错走普通 conv 路径，comment
        // 形 conv 被当 chat_id 发送必失败）：图片上传后以评论回复带 img 实体；
        // 文件实体评论回复不支持（drive 评论内容实体只有 text/at/img——离线确认，
        // **待真机校准**），给用户可读错误而非静默失败。
        if let Some((file_token, legacy_cid)) = comment_target_from_conv(conv) {
            // T13：锚点解析与 send_text 评论分支同款——轮次发起者优先，最近
            // 评论回退（B 抢先评论不截走 A 的回答）。
            let comment_id = self
                .conv_states
                .lock()
                .await
                .get(&conv.0)
                .and_then(|s| s.resolve_comment_anchor(s.round_initiator.as_deref()))
                .or(legacy_cid);
            let Some(comment_id) = comment_id else {
                return Err(CoreError::Platform(
                    PLATFORM,
                    "评论线程缺少回复目标（comment_id），无法发送媒体".to_string(),
                ));
            };
            if media.kind != "image" {
                return Err(CoreError::Platform(
                    PLATFORM,
                    "评论线程暂不支持发送文件，请在聊天会话中获取文件。".to_string(),
                ));
            }
            // v1.21 review：读前大小预检——50MB 上限此前在 client 上传侧才查，
            // `tokio::fs::read` 会先把任意大文件整读进内存（OOM 风险）。
            if let Some(e) = media_size_violation(&media.url).await {
                return Err(e);
            }
            let bytes = tokio::fs::read(&media.url).await.map_err(|e| {
                CoreError::Platform(PLATFORM, format!("读媒体文件 {}: {e}", media.url))
            })?;
            let file_name = std::path::Path::new(&media.url)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "image.png".to_string());
            return self
                .with_token(|t| {
                    let bytes = bytes.clone();
                    let file_name = file_name.clone();
                    let file_token = file_token.clone();
                    let comment_id = comment_id.clone();
                    async move {
                        let image_key =
                            upload_image(&self.core_config, &t, &file_name, bytes).await?;
                        // img 实体字段名（file_key vs file_token）离线无法确认，
                        // 按评论事件 content 同构的最合理形态实现——待真机校准。
                        reply_comment_nodes(
                            &self.core_config,
                            &t,
                            &file_token,
                            &comment_id,
                            serde_json::json!([{ "type": "img", "file_key": image_key }]),
                        )
                        .await
                        .map(|_| ())
                    }
                })
                .await;
        }
        // v1.23 review：媒体上传是最重的出站请求——纳入 per-conv 令牌桶
        //（此前只在 send_text/send_card，覆盖面不全）。
        self.acquire_send_slot(&conv.0).await;
        // agent 产出媒体回传（P6-7：按 kind 分流——image 走图片消息，其余走文件
        // 消息）：读本地文件 → 上传拿 key → 发消息。话题群 conv → reply API 落回话题。
        let thread = thread_target_from_conv(conv);
        let (receive_id, kind) = receive_target_from_conv(conv)
            .ok_or_else(|| CoreError::Platform(PLATFORM, format!("非法 conv_id: {}", conv.0)))?;
        // v1.21 review：读前大小预检（同上方评论分支——防大文件整读进内存）。
        if let Some(e) = media_size_violation(&media.url).await {
            return Err(e);
        }
        let bytes = tokio::fs::read(&media.url)
            .await
            .map_err(|e| CoreError::Platform(PLATFORM, format!("读媒体文件 {}: {e}", media.url)))?;
        let file_name = std::path::Path::new(&media.url)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "file.bin".to_string());
        let is_image = media.kind == "image";
        // 上传 + 发送共用一次 with_token：同一 token 失效只自愈重试一轮。
        // 重试要求闭包可重入，move 型捕获（bytes/receive_id/file_name）先 clone。
        self.with_token(|t| {
            let bytes = bytes.clone();
            let receive_id = receive_id.clone();
            let file_name = file_name.clone();
            let root_id = thread.as_ref().map(|(_, r)| r.clone());
            async move {
                let content = if is_image {
                    let image_key = upload_image(&self.core_config, &t, &file_name, bytes).await?;
                    serde_json::json!({ "image_key": image_key })
                } else {
                    let file_key = upload_file(&self.core_config, &t, &file_name, bytes).await?;
                    serde_json::json!({ "file_key": file_key })
                };
                match root_id {
                    // 话题群：与文本同路——reply API 落回原话题。
                    Some(root) => {
                        let mt = if is_image { "image" } else { "file" };
                        reply_message(&self.core_config, &t, &root, mt, &content.to_string())
                            .await
                            .map(|_| ())
                    }
                    None => {
                        if is_image {
                            send_image_msg(
                                &self.core_config,
                                &t,
                                &receive_id,
                                kind,
                                content["image_key"].as_str().unwrap_or_default(),
                            )
                            .await
                        } else {
                            send_file_msg(
                                &self.core_config,
                                &t,
                                &receive_id,
                                kind,
                                content["file_key"].as_str().unwrap_or_default(),
                            )
                            .await
                        }
                    }
                }
            }
        })
        .await
    }

    async fn send_typing(&self, conv: &ConvId, _hint: &ReplyHint) -> Result<()> {
        // 飞书协议无 typing 语义，但 core 在**每轮开始**调用本方法（round.rs）——
        // W3-5 借此作「轮次锚定」信号：把该 conv 最近一条入站消息提升为回复
        // 锚点。此后本轮运行中他人新消息只更新 last_inbound（下轮才生效），
        // 本轮的流式卡/回复不再被无关新消息抢走 reply 锚（修复 conv 级「最近
        // 一条」近似在群协作下的锚点漂移）。
        {
            let mut m = self.conv_states.lock().await;
            if let Some(s) = m.get_mut(&conv.0) {
                if let Some(a) = s.last_inbound.clone() {
                    s.reply_anchor = Some(a);
                }
            }
        }
        Ok(())
    }
    fn supports_streaming_card(&self, conv: &ConvId) -> bool {
        // P4-9：评论线程无卡片语义（回复是评论文本），走纯文本流。
        // P6 遗留补齐：话题群已支持「reply raw 卡 + 整卡 patch」流式（见 send_card）。
        !conv.0.starts_with(COMMENT_CONV_PREFIX)
    }

    /// P4-7：强制重连——notify_one 存 permit，WS run task 的 select 立即/稍后消费，
    /// 丢弃 open future 断开当前连接后重连。
    async fn reconnect(&self) -> Result<()> {
        self.reconnect.notify_one();
        Ok(())
    }

    /// P4-4：审批询问走「按钮卡片」——点击后飞书推 card.action.trigger，
    /// value 带回 conv + req（request_id）+ 动作，drain 解析成携带 ask_req 的
    /// 入站消息复用审批回复路由。卡片发送失败（无卡片权限等）降级纯文本
    /// （文本失败才向上报错 → dispatch 回 deny）。
    /// 多 pending 并存：不同 request_id 的卡片互不顶替（终端 ask 与 IM 审批共存）；
    /// 同 request_id 重复发送时旧卡 patch 成 superseded。
    /// 返回卡片 message_id（core 作为引用回复路由锚点；文本路径 None）。
    async fn send_permission_ask(
        &self,
        conv: &ConvId,
        request_id: &str,
        tool_name: &str,
        input_summary: &str,
        hint: &ReplyHint,
    ) -> Result<Option<String>> {
        // 评论线程无卡片语义，直接走文本（send_text 已路由 reply API）。
        if comment_target_from_conv(conv).is_some() {
            return self
                .send_permission_ask_text(conv, tool_name, input_summary, hint)
                .await
                .map(|_| None);
        }
        // 询问发起者（最近 sender）与真实超时值：编码进按钮 value（回调侧全形态
        // 校验点击者 + 24h 时效）与 note 倒计时文案。
        let sender = self.last_sender(&conv.0).await;
        let sender_opt = (!sender.is_empty()).then_some(sender.as_str());
        let timeout = self.ask_timeout_secs;
        // P6（AskUserQuestion 透传）：agent 的问题渲染成「问题 + 选项」卡而非
        // 允许/拒绝审批卡——**判定在话题/评论分支之前**（v1.17.1 修：原判定在
        // 主时间线分支内，话题里的问题仍降级审批卡裸显 JSON——真机 2026-09-02
        // 复现：话题内 4 问测试收到"回复 y 允许"审批文本）。
        // v1.17.2：降级时 warn 携带长度与解析失败原因——v1.17.0 曾有一例
        // 正常群内降级审批卡（body 4257 卡片形态），根因未定位，此日志让
        // 复发时可诊断（截断？解析？形状变化？）。
        let is_question = if tool_name == "AskUserQuestion" {
            let ok = crate::card::render_question_card(
                input_summary,
                &conv.0,
                request_id,
                sender_opt,
                timeout,
            )
            .is_some();
            if !ok {
                warn!(target: "feishu", conv = %conv.0,
                    len = input_summary.chars().count(),
                    parse_ok = serde_json::from_str::<serde_json::Value>(input_summary).is_ok(),
                    tail = %input_summary.chars().rev().take(8).collect::<String>(),
                    "AskUserQuestion 问题卡渲染失败，降级审批卡（tail 应为右花括号，否则被截断）");
            }
            ok
        } else {
            false
        };
        // P6 遗留补齐：话题群——reply API 把询问卡发进原话题（与流式卡同路），
        // 失败降级文本（文本经 send_text 的线程分支也落回话题）。
        // P8-2：话题群的复用槽与普通 conv 同一套（patch 话题内旧卡同样有效）。
        if let Some((_chat, root_id)) = thread_target_from_conv(conv) {
            let card_json = if is_question {
                crate::card::render_question_card(
                    input_summary,
                    &conv.0,
                    request_id,
                    sender_opt,
                    timeout,
                )
                .unwrap_or_else(|| {
                    render_permission_card(
                        tool_name,
                        input_summary,
                        &conv.0,
                        request_id,
                        sender_opt,
                        timeout,
                    )
                })
            } else {
                render_permission_card(
                    tool_name,
                    input_summary,
                    &conv.0,
                    request_id,
                    sender_opt,
                    timeout,
                )
            };
            return match self
                .with_token(|t| {
                    let root_id = root_id.clone();
                    let card_json = card_json.clone();
                    async move {
                        reply_message(&self.core_config, &t, &root_id, "interactive", &card_json)
                            .await
                    }
                })
                .await
            {
                Ok(mid) => {
                    if let Some(mid) = &mid {
                        let render = AskRender {
                            question: is_question,
                            tool_name: tool_name.to_string(),
                            input: input_summary.to_string(),
                            sender,
                        };
                        self.register_ask_card(&conv.0, mid, request_id, tool_name, render)
                            .await;
                    }
                    Ok(mid)
                }
                Err(e) => {
                    warn!(target: "feishu", error = %e, "话题内审批卡发送失败，降级纯文本询问");
                    self.mark_ask_sent(&conv.0).await;
                    self.send_permission_ask_text(conv, tool_name, input_summary, hint)
                        .await
                        .map(|_| None)
                }
            };
        }
        let (receive_id, kind) = receive_target_from_conv(conv)
            .ok_or_else(|| CoreError::Platform(PLATFORM, format!("非法 conv_id: {}", conv.0)))?;
        // is_question 已在上方话题分支前统一判定（v1.17.1）。
        let card_json = if is_question {
            crate::card::render_question_card(
                input_summary,
                &conv.0,
                request_id,
                sender_opt,
                timeout,
            )
            .unwrap_or_else(|| {
                render_permission_card(
                    tool_name,
                    input_summary,
                    &conv.0,
                    request_id,
                    sender_opt,
                    timeout,
                )
            })
        } else {
            render_permission_card(
                tool_name,
                input_summary,
                &conv.0,
                request_id,
                sender_opt,
                timeout,
            )
        };
        let render = AskRender {
            // 问题卡解析失败降级审批卡——AskRender 按实际渲染形态记（question
            // 为 false 时 input 走审批路径）。
            question: is_question,
            tool_name: tool_name.to_string(),
            input: input_summary.to_string(),
            sender,
        };
        // 真机校准（2026-08-30）：不再复用旧询问卡——跨轮复用曾致「隐形审批」，
        // 残留旧卡又让用户点错（实测两次）。每次询问都发**新卡**；顺序审批多卡
        // 的代价（顶走流式卡）远小于复用的认知负担。ask_slots 仍作「当前未决
        // 询问卡」登记（note 联动 / reaction 路由用），仅不再回收复用。
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
                if let Some(mid) = &mid {
                    self.register_ask_card(&conv.0, mid, request_id, tool_name, render).await;
                }
                Ok(mid)
            }
            Err(e) => {
                warn!(target: "feishu", error = %e, "审批卡片发送失败，降级纯文本询问");
                self.mark_ask_sent(&conv.0).await;
                self.send_permission_ask_text(conv, tool_name, input_summary, hint)
                    .await
                    .map(|_| None)
            }
        }
    }

    /// 纯文本审批询问覆写（评论场景文案批次）：评论线程无按钮卡（卡片降级文本），
    /// 「回复 y 允许」在评论里会变成对文档的新评论而非审批回复——改为指引
    /// 「回复 @bot y / @bot n」（@bot 的评论才会被当审批回复路由回 bot）。其余
    /// 场景文案与 core 默认一致。
    async fn send_permission_ask_text(
        &self,
        conv: &ConvId,
        tool_name: &str,
        input_summary: &str,
        hint: &ReplyHint,
    ) -> Result<()> {
        // v1.17.1：AskUserQuestion 的文本降级（评论线程/卡发送失败）不再裸显
        // JSON——按问题列表渲染，答案走既有的 ask: 回复通道。
        if tool_name == "AskUserQuestion" {
            if let Some(questions) = crate::card::questions_as_text(input_summary) {
                let prefix = if comment_target_from_conv(conv).is_some() {
                    "@bot "
                } else {
                    ""
                };
                let text = format!(
                    "❓ {questions}\n\n请{prefix}回复 ask:选项（多题用「；」分隔，如 ask:题一=甲；题二=乙）。"
                );
                return self.send_text(conv, &text, hint).await;
            }
        }
        let summary = imagent_core::render::tool_summary(tool_name, input_summary);
        let text = if comment_target_from_conv(conv).is_some() {
            format!("🔐 请求执行 {tool_name}：{summary}\n\n请回复 @bot y 允许 / @bot n 拒绝。")
        } else {
            format!("🔐 请求执行 {tool_name}：{summary}\n\n回复 y 允许，其它拒绝。")
        };
        self.send_text(conv, &text, hint).await
    }

    /// P5-16：把指定 request_id 的询问卡 patch 成「已中断」终态（移除按钮，
    /// 防用户对已结束的询问继续操作）。无记录（文本询问/未发过卡）时 no-op。
    async fn cancel_permission_ask(&self, _conv: &ConvId, request_id: &str) -> Result<()> {
        let Some(card) = self.pending_asks.lock().await.remove(request_id) else {
            return Ok(());
        };
        let PendingAskCard {
            conv_id,
            msg_id: message_id,
            tool_name,
            sender: _,
        } = card;
        // P8-2：释放复用槽（卡保留，下一个询问可原地复用）。
        self.free_ask_slot(&conv_id, request_id).await;
        let card_json = render_permission_card_cancelled(&tool_name);
        self.with_token(|t| {
            let message_id = message_id.clone();
            let card_json = card_json.clone();
            async move { patch_card(&self.core_config, &t, &message_id, &card_json).await }
        })
        .await
    }

    /// /stop：收敛该 conv 的**全部** pending 询问卡（多卡并存后按 conv 遍历）。
    async fn cancel_all_permission_asks(&self, conv: &ConvId) -> Result<()> {
        let mut all = self.pending_asks.lock().await;
        let mut hits: Vec<(String, String)> = Vec::new();
        all.retain(|_, card| {
            if card.conv_id == conv.0 {
                hits.push((card.msg_id.clone(), card.tool_name.clone()));
                false
            } else {
                true
            }
        });
        drop(all);
        // P8-2：conv 级复用槽一并释放（/stop 后短窗口内的下一个询问可复用末张卡）。
        if let Some(slot) = self
            .conv_states
            .lock()
            .await
            .get_mut(&conv.0)
            .and_then(|s| s.ask_slot.as_mut())
        {
            slot.pending_req = None;
        }
        for (message_id, tool_name) in hits {
            let card_json = render_permission_card_cancelled(&tool_name);
            if let Err(e) = self
                .with_token(|t| {
                    let message_id = message_id.clone();
                    let card_json = card_json.clone();
                    async move { patch_card(&self.core_config, &t, &message_id, &card_json).await }
                })
                .await
            {
                warn!(target: "feishu", error = %e, "询问卡收敛失败（不影响中断）");
            }
        }
        Ok(())
    }

    /// 真机校准 UX：决策已回（approve/deny）后把询问卡 patch 成「已批准/已拒绝」
    /// 终态——用户点击后立即有反馈，卡片不再保持可点。best-effort。
    /// P6：AskUserQuestion 的问题卡显示「已记录你的选择」（message 携带选项）。
    async fn resolve_permission_ask(
        &self,
        _conv: &ConvId,
        request_id: &str,
        reply: &imagent_core::PermissionReply,
    ) -> Result<()> {
        let Some(card) = self.pending_asks.lock().await.remove(request_id) else {
            return Ok(());
        };
        let PendingAskCard {
            conv_id,
            msg_id: message_id,
            tool_name,
            sender: _,
        } = card;
        // P8-2：释放复用槽——卡保留（显示已批准/已拒绝），下一个询问原地复用。
        self.free_ask_slot(&conv_id, request_id).await;
        let card_json = if tool_name == "AskUserQuestion" {
            let choice = reply
                .raw_text
                .as_deref()
                .or(reply.message.as_deref())
                .unwrap_or("已收到")
                .trim_start_matches("用户选择：");
            crate::card::render_question_card_resolved(choice)
        } else {
            crate::card::render_permission_card_resolved(&tool_name, reply.allow)
        };
        self.with_token(|t| {
            let message_id = message_id.clone();
            let card_json = card_json.clone();
            async move { patch_card(&self.core_config, &t, &message_id, &card_json).await }
        })
        .await
    }

    /// P10-③：排队联动——该会话挂着未决审批卡时，按原渲染输入重画整卡并把
    /// note 行换成「⏳ 等待你审批 · 后面还排着 N 条消息」（审批等待是流式卡最
    /// 静默的窗口，排队状态需要推送）。note 内容经 ask_notes 缓存去重（计数
    /// 不变不重画）；无未决槽 no-op。best-effort。
    async fn note_queued_on_ask(&self, conv: &ConvId, note: &str, _hint: &ReplyHint) -> Result<()> {
        let (msg_id, render, request_id) = {
            let states = self.conv_states.lock().await;
            let Some(slot) = states.get(&conv.0).and_then(|s| s.ask_slot.as_ref()) else {
                return Ok(());
            };
            let Some(req) = slot.pending_req.clone() else {
                return Ok(()); // 槽空闲（已收敛）——无未决审批可联动
            };
            (slot.msg_id.clone(), slot.render.clone(), req)
        };
        // 去重：note 不变不重画。
        {
            let mut states = self.conv_states.lock().await;
            let unchanged = states
                .get(&conv.0)
                .and_then(|s| s.ask_note.as_deref())
                .is_some_and(|prev| prev == note);
            if unchanged {
                return Ok(());
            }
            states.entry(conv.0.clone()).or_default().ask_note = Some(note.to_string());
        }
        // L17（code-review v8）：快照→网络→patch 期间终态可能已落——重渲染前
        // 复查 pending 仍是快照的 request_id，否则放弃（防终态卡被翻回带按钮
        // 的 pending 态误导点击；过期点击本有时效兜底，此处消歧义）。
        {
            let states = self.conv_states.lock().await;
            let still_pending = states
                .get(&conv.0)
                .and_then(|s| s.ask_slot.as_ref())
                .map(|sl| sl.pending_req.as_deref() == Some(request_id.as_str()))
                .unwrap_or(false);
            if !still_pending {
                return Ok(());
            }
        }
        let card_json = if render.question {
            crate::card::render_question_card_note(
                &render.input,
                &conv.0,
                &request_id,
                sender_opt_of(&render.sender),
                note,
            )
            .unwrap_or_else(|| {
                crate::card::render_permission_card_note(
                    &render.tool_name,
                    &render.input,
                    &conv.0,
                    &request_id,
                    sender_opt_of(&render.sender),
                    note,
                )
            })
        } else {
            crate::card::render_permission_card_note(
                &render.tool_name,
                &render.input,
                &conv.0,
                &request_id,
                sender_opt_of(&render.sender),
                note,
            )
        };
        self.with_token(|t| {
            let msg_id = msg_id.clone();
            let card_json = card_json.clone();
            async move { patch_card(&self.core_config, &t, &msg_id, &card_json).await }
        })
        .await
        .map_err(|e| {
            tracing::debug!(target: "feishu", error = %e, "审批卡排队 note 重画失败（不影响排队）");
            e
        })
    }

    /// P9-2：`/config` 表单卡（form + select_static 下拉 + 提交）。评论线程无
    /// 卡片语义 → 文本降级；话题群 → reply 进原话题；发送失败上抛由 dispatch
    /// 层统一降级（与命令卡同策略）。
    async fn send_config_form(
        &self,
        conv: &ConvId,
        entries: &[imagent_core::ConfigFormField],
        fallback: &str,
        hint: &ReplyHint,
    ) -> Result<()> {
        if comment_target_from_conv(conv).is_some() {
            return self.send_text(conv, fallback, hint).await;
        }
        let card_json = render_config_form_card(entries, &conv.0);
        if let Some((_chat, root_id)) = thread_target_from_conv(conv) {
            return self
                .with_token(|t| {
                    let root_id = root_id.clone();
                    let card_json = card_json.clone();
                    async move {
                        reply_message(&self.core_config, &t, &root_id, "interactive", &card_json)
                            .await
                    }
                })
                .await
                .map(|_| ());
        }
        let (receive_id, kind) = receive_target_from_conv(conv)
            .ok_or_else(|| CoreError::Platform(PLATFORM, format!("非法 conv_id: {}", conv.0)))?;
        let mid = self
            .with_token(|t| {
                let receive_id = receive_id.clone();
                let card_json = card_json.clone();
                async move { send_card_msg(&self.core_config, &t, &receive_id, kind, &card_json).await }
            })
            .await?;
        // 结果下沉的新卡是会话最新可见卡——完成强提醒的加急对象。
        if let Some(m) = mid {
            self.note_card_tail(&conv.0, &m).await;
        }
        Ok(())
    }

    /// P6-3：命令交互卡片（markdown 正文 + 按钮组）。按钮点击回调由 proto 解析成
    /// `text = <command>` 走手打命令同路径。评论线程无卡片语义 → 纯文本降级；
    /// 话题群 → reply API 把卡发进原话题；卡片发送失败向上返回 Err，由 dispatch
    /// 层统一降级纯文本（与审批卡策略不同：命令卡失败无紧急性，不急于平台内自救）。
    async fn send_command_card(
        &self,
        conv: &ConvId,
        title: &str,
        body_md: &str,
        buttons: &[CardButton],
        hint: &ReplyHint,
    ) -> Result<()> {
        // v1.23 review：命令卡直发（不走 send_card）——补令牌桶覆盖。
        self.acquire_send_slot(&conv.0).await;
        if comment_target_from_conv(conv).is_some() {
            return self
                .send_text(
                    conv,
                    &command_card_fallback_text(title, body_md, buttons),
                    hint,
                )
                .await;
        }
        let card_json = render_command_card(title, body_md, buttons, &conv.0);
        // P6 遗留补齐：话题群用 reply API 落卡进原话题（create 到 chat 会开新话题）。
        if let Some((_chat, root_id)) = thread_target_from_conv(conv) {
            return self
                .with_token(|t| {
                    let root_id = root_id.clone();
                    let card_json = card_json.clone();
                    async move {
                        reply_message(&self.core_config, &t, &root_id, "interactive", &card_json)
                            .await
                    }
                })
                .await
                .map(|_| ());
        }
        let (receive_id, kind) = receive_target_from_conv(conv)
            .ok_or_else(|| CoreError::Platform(PLATFORM, format!("非法 conv_id: {}", conv.0)))?;
        self.with_token(|t| {
            let receive_id = receive_id.clone();
            let card_json = card_json.clone();
            async move { send_card_msg(&self.core_config, &t, &receive_id, kind, &card_json).await }
        })
        .await
        .map(|_| ())
    }

    /// P6 遗留补齐：`/config require_mention` 热切换——drain task 每消息现读，
    /// 对下一消息生效；进程内不落盘（重启回 config 值，与 cot_detail 同姿态）。
    async fn require_mention_in_group(&self) -> Option<bool> {
        Some(self.mention_policy.read().await.require_mention_in_group)
    }

    /// P6 遗留补齐：set 侧（见 [`Self::require_mention_in_group`]）。
    async fn set_require_mention_in_group(&self, on: bool) -> Result<()> {
        self.mention_policy.write().await.require_mention_in_group = on;
        Ok(())
    }

    /// P7-A2：bot 已加入的群（conv 形态 id + 群名），`/chat allow-all` 批量放行。
    async fn list_joined_chats(&self) -> Result<Vec<JoinedChat>> {
        let token = self.get_token().await?;
        let chats = list_joined_chats(&self.core_config, &token).await?;
        Ok(chats
            .into_iter()
            .map(|(chat_id, name)| JoinedChat { chat_id, name })
            .collect())
    }

    /// 发流式卡片。**句柄前缀分流**（core 无感，两种句柄均原样透传给 update_card）：
    /// - managed（优先）：`create_card_entity` + 发 card_id 引用消息 → `card:<card_id>`，
    ///   后续 element 级 PATCH 走服务端打字机渲染（需 `cardkit:card:write` 权限）
    /// - 降级：raw 卡片消息 → `msg:<message_id>`，后续整卡 im patch（体验同旧版）
    ///
    /// P6 遗留补齐：话题群走「reply API 发 raw 卡」——managed 卡片实体无法在话题内
    /// 引用（send_card_ref_msg 到 chat 会开新话题），但 reply 的 interactive 回执是
    /// 普通消息，msg: 句柄照常整卡 patch（体验同降级路径，卡片不再缺席话题）。
    async fn send_card(
        &self,
        conv: &ConvId,
        card: &OutboundCard,
        _hint: &ReplyHint,
    ) -> Result<Option<String>> {
        // v1.21：卡片创建与文本同走 per-conv 令牌桶（update_card 的 patch
        // 不在此列——流式帧已有节流与 seq）。
        self.acquire_send_slot(&conv.0).await;
        // P8-2：新一轮流式卡——「之后发过询问卡」标记清零（conv 轮次串行，
        // 无并发覆盖问题）。
        self.conv_states
            .lock()
            .await
            .entry(conv.0.clone())
            .or_default()
            .asks_since_card = false;
        // 发起者（最近 sender）：编码进初始卡的 ⏹ 终止按钮 value（回调侧全形态
        // 校验点击者，v13-P2）；Wave B-5：群 conv 初始卡顶部加「发起者」标注行。
        // 占位/未知为 None（旧语义，不校验）。
        let sender = self.last_sender(&conv.0).await;
        let sender_opt = (!sender.is_empty()).then_some(sender);
        if let Some((_chat, root_id)) = thread_target_from_conv(conv) {
            let card_json = render_card(card, &conv.0, sender_opt.as_deref());
            return self
                .with_token(|t| {
                    let root_id = root_id.clone();
                    let card_json = card_json.clone();
                    async move {
                        reply_message(&self.core_config, &t, &root_id, "interactive", &card_json)
                            .await
                    }
                })
                .await
                .map(|mid| mid.map(|m| format!("msg:{m}")));
        }
        let (receive_id, kind) = receive_target_from_conv(conv)
            .ok_or_else(|| CoreError::Platform(PLATFORM, format!("非法 conv_id: {}", conv.0)))?;
        let res = self
            .with_token(|t| {
                let receive_id = receive_id.clone();
                let conv_for_init = conv.0.clone();
                let sender_opt = sender_opt.clone();
                let conv_for_anchor = conv.clone();
                async move {
                    match create_card_entity(
                        &t,
                        &render_stream_init_card(
                            &conv_for_init,
                            sender_opt.as_deref(),
                            card.task_digest.as_deref(),
                        ),
                    )
                    .await
                    {
                        Ok(card_id) => {
                            // Wave B-6：普通群优先 reply 引用发起消息（content 与
                            // create 同构：card 实体引用 JSON）；失败/无锚点回退
                            // create 到会话。
                            let content = serde_json::json!({
                                "type": "card", "data": { "card_id": card_id }
                            })
                            .to_string();
                            match self
                                .send_interactive_anchored(
                                    &t,
                                    &conv_for_anchor,
                                    &receive_id,
                                    kind,
                                    &content,
                                )
                                .await
                            {
                                Ok(mid) => {
                                    if let Some(m) = mid {
                                        let mut mm = self.managed_card_msgs.lock().await;
                                        if mm.len() > 1024 {
                                            // 粗上限（超量整体重置，同 thread_active
                                            // 惯例）：清后旧卡终态退回内联形态（可接受）。
                                            mm.clear();
                                        }
                                        mm.insert(card_id.clone(), m);
                                    }
                                    Ok(Some(format!("card:{card_id}")))
                                }
                                Err(e) => {
                                    // 实体已建但消息发送失败：实体作废（14 天过期自然回收），降级 raw。
                                    warn!(target: "feishu", error = %e, "发送卡片引用消息失败，降级 raw 卡片");
                                    self.send_card_raw(
                                        &conv_for_anchor.0,
                                        &receive_id,
                                        kind,
                                        card,
                                        &t,
                                        sender_opt.as_deref(),
                                    )
                                    .await
                                }
                            }
                        }
                        Err(e) => {
                            // 权限未开（cardkit:card:write）或创建失败 → 降级 raw + 整卡 im patch。
                            warn!(target: "feishu", error = %e, "创建卡片实体失败（需 cardkit:card:write 权限），降级 raw 卡片");
                            self.send_card_raw(
                                &conv_for_anchor.0,
                                &receive_id,
                                kind,
                                card,
                                &t,
                                sender_opt.as_deref(),
                            )
                            .await
                        }
                    }
                }
            })
            .await;
        // 初始 footer 预填缓存（footer 预填批次）：初始模板的 md_footer 就是
        // 「🧠 思考中…」——预填 card_footers 后首次 Running patch 若 footer 仍是
        // 思考中（未带秒数/排队）会被去重跳过，不再重复 patch 同内容。
        if let Ok(Some(handle)) = &res {
            if let Some(card_id) = handle.strip_prefix("card:") {
                self.card_footers
                    .lock()
                    .await
                    .insert(card_id.to_string(), "🧠 思考中…".to_string());
            }
        }
        res
    }

    /// 更新流式卡片。按 [`send_card`](Self::send_card) 返回的句柄前缀分流：
    /// - `card:<card_id>`：CardKit 真流式——Running 时 PATCH `md_body`（正文+工具，
    ///   打字机渐显）；Done/Error 时 PATCH 终态正文（含工具统计+完成行）并 PATCH
    ///   settings 关闭流式（光标消失）
    /// - `msg:<message_id>`：降级路径——整卡 im patch（现有行为，含折叠面板）
    async fn update_card(
        &self,
        conv: &ConvId,
        handle: &str,
        card: &OutboundCard,
        _hint: &ReplyHint,
    ) -> Result<()> {
        // P8-2：终态「结果下沉」——本轮发过询问卡（流式卡被审批卡顶离视口）时，
        // 流式卡收成指针 stub，完整结果另发新卡落在会话最下面。
        // v1.18 review：peek 而非 take——终态全链路（patch + 下沉重发）成功才
        // 清标志（见 clear_asks_flag）；失败路径保标志，孤儿扫描/重试还能走
        // 下沉形态补救（此前 patch 前消费，失败后下沉能力静默丢失）。
        let wants_buried =
            !matches!(card.terminal, CardTerminal::Running) && self.peek_asks_flag(&conv.0).await;
        let res = self
            .with_token(|token| {
                let this = self;
                async move {
                    this.patch_any_handle(&token, conv, handle, card, wants_buried)
                        .await
                }
            })
            .await;
        // 阶段 B/C（v1.18 review 状态机化）：失败路径的两层自愈收敛为显式
        // PatchOutcome。B（安全批次，原注释）：原卡片被用户删除/撤回后 patch
        // 永远失败——清 per-card 缓存（序列号/footer），错误附 CARD_HANDLE_LOST
        // 哨兵（core CardSession 据此摘 live_cards 并置空句柄，Running 期下帧
        // 重发新卡，启动扫描据此作废登记）。C：终态失败的最小卡收敛（见
        // minimize_terminal_card）。Running 失败不触发 C（流式帧失败由下帧/
        // 看门狗兜底——原守卫的语义保留在 match arm 条件里）。
        let outcome = match res {
            Ok(()) => PatchOutcome::Delivered,
            Err(e) if is_card_not_exist_err(&e) => {
                warn!(target: "feishu", handle, "卡片不存在/已删除，清缓存并上报句柄丢失");
                if let Some(card_id) = handle.strip_prefix("card:") {
                    self.card_seqs.lock().await.remove(card_id);
                    self.card_footers.lock().await.remove(card_id);
                }
                PatchOutcome::HandleLost(CoreError::Platform(
                    PLATFORM,
                    format!("{e}（{CARD_HANDLE_LOST}）"),
                ))
            }
            Err(e)
                if !matches!(card.terminal, CardTerminal::Running)
                    && !e.to_string().contains(CARD_HANDLE_LOST) =>
            {
                let done = matches!(card.terminal, CardTerminal::Done);
                self.minimize_terminal_card(handle, done, e).await
            }
            Err(e) => PatchOutcome::Failed(e),
        };
        // 阶段 D：结果下沉重发——流式卡已收敛成指针 → 完整结果另发新卡（Wave
        // B-5：带发起者标注行）。仅 Delivered 走此步；重发失败上抛 Err（core 的
        // P5-11 兜底纯文本补发全文，结论不能因重发失败而丢）。
        if matches!(outcome, PatchOutcome::Delivered) && wants_buried {
            let sender = self.last_sender(&conv.0).await;
            let full = render_card(
                card,
                &conv.0,
                (!sender.is_empty()).then_some(sender).as_deref(),
            );
            if let Err(e) = self.send_static_card(conv, &full).await {
                warn!(target: "feishu", error = %e, "结果下沉重发失败，交由 core 纯文本兜底");
                return Err(e);
            }
            // 下沉全链路成功才消费标志（失败保标志：孤儿扫描仍可下沉补救）。
            self.clear_asks_flag(&conv.0).await;
        }
        outcome.into_result()
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
    use testutil::{mk_platform_with_mock, spawn_mock_feishu};

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

    /// P6 遗留补齐：require_mention 热切换——共享句柄 get/set 往返（drain task
    /// 每消息现读同一句柄）。占位凭据，WS/drain 后台任务自然失败重试不干扰断言。
    #[tokio::test]
    async fn require_mention_hot_toggle_roundtrip() {
        let p = FeishuPlatform::new(
            "cli_test".into(),
            "secret_test".into(),
            "https://open.feishu.cn".into(),
            true,
            None,
            300,
            None,
            1800,
            true,
            // v1.21：outbox 未接 store / 发送限速关闭（测试态）；v1.24 加急关。
            None,
            0.0,
            false,
            0,
        )
        .expect("构造");
        assert_eq!(p.require_mention_in_group().await, Some(true));
        p.set_require_mention_in_group(false).await.expect("set");
        assert_eq!(p.require_mention_in_group().await, Some(false));
        p.set_require_mention_in_group(true)
            .await
            .expect("set back");
        assert_eq!(p.require_mention_in_group().await, Some(true));
    }

    /// Wave B-8：话题免 @ 窗口换算——0 = 关闭（ZERO），正值 = 秒。
    #[test]
    fn thread_window_of_maps_config() {
        assert_eq!(thread_window_of(0), Duration::ZERO);
        assert_eq!(thread_window_of(600), Duration::from_secs(600));
        assert_eq!(thread_window_of(1800), Duration::from_secs(30 * 60));
    }

    /// Wave B-4/B-6：构造注入落位——quiet_hours 存解析产物（None 恒不在免打扰）；
    /// 回复锚点表写入后可查。占位凭据（后台 WS 自然失败重试，不干扰断言）。
    #[tokio::test]
    async fn quiet_hours_and_reply_anchor_wiring() {
        let p = FeishuPlatform::new(
            "cli_test".into(),
            "secret_test".into(),
            "https://open.feishu.cn".into(),
            true,
            None,
            300,
            None,
            1800,
            true,
            // v1.21：outbox 未接 store / 发送限速关闭（测试态）；v1.24 加急关。
            None,
            0.0,
            false,
            0,
        )
        .expect("构造");
        assert!(p.quiet_hours.is_none(), "未配置 → None");
        assert!(!p.in_quiet_hours(), "未配置恒不在免打扰");
        // 锚点表 roundtrip。
        assert!(p.reply_anchor("feishu:oc_g").await.is_none(), "空表 → None");
        p.conv_states
            .lock()
            .await
            .entry("feishu:oc_g".to_string())
            .or_default()
            .reply_anchor = Some("om_1".to_string());
        assert_eq!(
            p.reply_anchor("feishu:oc_g").await.as_deref(),
            Some("om_1"),
            "登记后可查"
        );
    }

    /// Bug：message_max_len 三平台生效——飞书分片上限 = min(config, FEISHU_TEXT_MAX)；
    /// 评论路径与 FEISHU_COMMENT_TEXT_MAX 取 min；未配置回落协议上限。
    /// 占位凭据构造（WS 后台任务自然失败重试，不干扰断言）。
    #[tokio::test]
    async fn text_split_caps_respect_message_max_len() {
        let mk = |max: Option<usize>| {
            FeishuPlatform::new(
                "cli_test".into(),
                "secret_test".into(),
                "https://open.feishu.cn".into(),
                true,
                max,
                300,
                None,
                1800,
                true,
                None,
                0.0,
                false,
                0,
            )
            .expect("构造")
        };
        // 未配置：协议上限。
        let p = mk(None);
        assert_eq!(p.text_split_max, FEISHU_TEXT_MAX);
        assert_eq!(p.comment_split_max, FEISHU_COMMENT_TEXT_MAX);
        // 配置小于协议上限：生效。
        let p = mk(Some(2_000));
        assert_eq!(p.text_split_max, 2_000);
        assert_eq!(p.comment_split_max, 2_000);
        // 配置大于协议上限：钳到协议上限（不放大）。
        let p = mk(Some(1_000_000));
        assert_eq!(p.text_split_max, FEISHU_TEXT_MAX);
        assert_eq!(p.comment_split_max, FEISHU_COMMENT_TEXT_MAX);
    }

    // ---------- T13：评论回复锚定轮次发起者 + 引用片段注入 ----------

    /// 核心场景：A @bot 提问（c_a）→ B 抢先又评论（c_b，更晚登记）→ 回复的
    /// reply_comment POST 锚定 **A** 的 comment_id（此前恒取「最新评论」会把
    /// A 的回答拽到 B 的评论线程下）。mock 回环断言真实请求路径（只记录评论
    /// 回复路径——后台 WS 重连也会打到 mock，须滤除防串扰）。
    #[tokio::test]
    async fn comment_reply_anchors_round_initiator() {
        let hits: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let hits_c = hits.clone();
        let base = spawn_mock_feishu(std::sync::Arc::new(move |path: &str| {
            if path.contains("/comments/") {
                hits_c.lock().unwrap().push(path.to_string());
            }
            (200u16, r#"{"code":0,"msg":"success"}"#.to_string())
        }))
        .await;
        let p = mk_platform_with_mock(&base).await;
        {
            let mut m = p.conv_states.lock().await;
            let st = m.entry("feishu:comment:ft_doc".into()).or_default();
            st.note_comment("ou_a", "c_a");
            st.note_comment("ou_b", "c_b"); // B 抢先评论（最新登记）
            st.round_initiator = Some("ou_a".into()); // 轮次由 A 发起
        }
        p.send_text(
            &ConvId("feishu:comment:ft_doc".into()),
            "A 问题的答案",
            &ReplyHint::None,
        )
        .await
        .expect("评论回复应成功");
        let paths = hits.lock().unwrap();
        assert_eq!(
            paths.last().map(String::as_str),
            Some("/open-apis/drive/v1/files/ft_doc/comments/c_a/replies"),
            "回复锚定轮次发起者 A 的评论: {paths:?}"
        );
    }

    /// 回退链：发起者无评论记录 / 无发起者 → 回退「最近一条评论」（修复前
    /// 行为，不更差）；锚点表整空 → 存量 conv 内嵌 comment_id 兜底；发起者
    /// 同人多次评论 → 取其最新一条（仍是本人线程）。
    #[tokio::test]
    async fn comment_reply_anchor_fallbacks() {
        let hits: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let hits_c = hits.clone();
        let base = spawn_mock_feishu(std::sync::Arc::new(move |path: &str| {
            if path.contains("/comments/") {
                hits_c.lock().unwrap().push(path.to_string());
            }
            (200u16, r#"{"code":0,"msg":"success"}"#.to_string())
        }))
        .await;
        let p = mk_platform_with_mock(&base).await;
        let conv = ConvId("feishu:comment:ft_doc".into());
        // ① 发起者（ou_c）无评论记录 → 回退最近一条（c_b）。
        {
            let mut m = p.conv_states.lock().await;
            let st = m.entry(conv.0.clone()).or_default();
            st.note_comment("ou_a", "c_a");
            st.note_comment("ou_b", "c_b");
            st.round_initiator = Some("ou_c".into());
        }
        p.send_text(&conv, "1", &ReplyHint::None).await.unwrap();
        // ② 无发起者记录（重启后）→ 同样回退最近一条。
        p.conv_states
            .lock()
            .await
            .get_mut(&conv.0)
            .unwrap()
            .round_initiator = None;
        p.send_text(&conv, "2", &ReplyHint::None).await.unwrap();
        // ③ 发起者同人两次评论 → 取其最新。
        {
            let mut m = p.conv_states.lock().await;
            let st = m.get_mut(&conv.0).unwrap();
            st.note_comment("ou_a", "c_a2");
            st.round_initiator = Some("ou_a".into());
        }
        p.send_text(&conv, "3", &ReplyHint::None).await.unwrap();
        // ④ 锚点表整空 + 存量内嵌形态 conv → 内嵌 comment_id 兜底。
        p.send_text(
            &ConvId("feishu:comment:ft_legacy:c_old".into()),
            "4",
            &ReplyHint::None,
        )
        .await
        .unwrap();
        let paths = hits.lock().unwrap();
        let expected = [
            "/open-apis/drive/v1/files/ft_doc/comments/c_b/replies",
            "/open-apis/drive/v1/files/ft_doc/comments/c_b/replies",
            "/open-apis/drive/v1/files/ft_doc/comments/c_a2/replies",
            "/open-apis/drive/v1/files/ft_legacy/comments/c_old/replies",
        ];
        assert_eq!(&*paths, &expected, "回退链按序生效");
    }
}
