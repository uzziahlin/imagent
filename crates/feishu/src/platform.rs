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
use tracing::{debug, warn};

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
    create_card_entity, download_file, download_image, fetch_bot_open_id, fetch_token,
    is_card_not_exist_msg, is_rate_limited_msg, list_joined_chats, list_merge_forward, patch_card,
    patch_card_element, patch_card_settings, reply_comment, reply_comment_nodes, reply_message,
    send_card_msg, send_file_msg, send_image_msg, send_text_msg, upload_file, upload_image,
    FeishuWsClient,
};
use crate::proto::{
    comment_target_from_conv, is_comment_event, is_group_message_event, is_private_conv,
    parse_bot_removed_event, parse_card_action_event, parse_comment_event, parse_menu_event,
    parse_merged_forward_event, parse_message_event, parse_recall_event, receive_target_from_conv,
    render_merge_forward_transcript, thread_key_of_payload, thread_target_from_conv,
    unsupported_message_notice, MergedForwardItem, ReceiveIdKind, COMMENT_CONV_PREFIX,
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

/// 一张 pending 询问卡的登记项：conv + 消息 id + 工具名 + **发起者**（群 conv 下
/// 按钮点击者校验用——询问由谁发起，只有其本人可答复；私聊不校验，单人）。
#[derive(Debug, Clone)]
struct PendingAskCard {
    conv_id: String,
    msg_id: String,
    tool_name: String,
    sender: String,
}

/// P8-2：审批卡复用槽（per conv）。`pending_req = None` 表示卡已收敛
/// （已批准/已拒绝/已中断）可被下一个询问**原地 patch 复用**——顺序询问
/// 不再每条刷一张新卡把流式卡顶上去；`Some(req)` = 挂着未决询问
/// （并发询问须另发新卡，防顶掉别人还没答的请求）。
struct AskSlot {
    msg_id: String,
    pending_req: Option<String>,
    /// 最近一次询问收敛的时刻。真机校准（2026-08）：跨轮次复用会把新询问
    /// patch 到早已被结果卡/后续消息顶离视口的历史卡上——用户看不到询问，
    /// 表现为「卡住」直到超时催办。复用仅在新鲜窗口（见 [`ASK_SLOT_REUSE_WINDOW`]）
    /// 内成立；None = 挂着未决询问或刚登记的新卡（不可复用）。
    resolved_at: Option<std::time::Instant>,
    /// P10-③：重渲染输入（note 联动更新时按原参数重画整卡，按钮 value 不变）。
    render: AskRender,
}

/// 询问卡的渲染输入（复用槽与新卡登记时记录）。
#[derive(Clone)]
struct AskRender {
    /// 是否 AskUserQuestion 问题卡（否则审批卡）。
    question: bool,
    tool_name: String,
    /// 审批卡=input 摘要 JSON；问题卡=AskUserQuestion 原始 input JSON。
    input: String,
    /// 询问发起者 open_id（note 联动重渲染时保持按钮 value 的 sender 编码不丢）。
    sender: String,
}

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
    /// P8-1：已上屏的 footer 文案缓存（per card_id）——分阶段 footer 只在**变化时**
    /// patch（思考中→调用工具→输出中），节流 tick 间内容相同则跳过，不浪费调用。
    card_footers: Arc<Mutex<HashMap<String, String>>>,
    /// P10-③：审批卡 note 行缓存（per conv）——排队计数不变不重画。
    ask_notes: Arc<Mutex<HashMap<String, String>>>,
    /// 每会话最新卡片的平台消息 id（真机校准 2026-08）：强提醒（urgent_app）
    /// 的加急对象——审批催办对审批卡、完成提醒对流式终态卡，卡直接弹通知而
    /// 非另发 buzz 文本。send_card / update_card / 审批卡登记时刷新。
    card_tail: Arc<Mutex<HashMap<String, String>>>,
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
    /// P8-2：审批卡复用槽（per conv，见 [`AskSlot`]）。
    ask_slots: Arc<Mutex<HashMap<String, AskSlot>>>,
    /// P8-2：本轮流式卡发送之后是否发过询问卡——终态时判定流式卡已被顶离
    /// 视口，触发「结果下沉」（流式卡收成指针 + 完整结果另发新卡落底）。
    asks_since_card: Arc<Mutex<HashMap<String, bool>>>,
    /// P6-1：群消息 @bot 过滤策略（与 drain task 共享，`/config` 热切换）。
    mention_policy: Arc<RwLock<crate::proto::MentionPolicy>>,
    /// conv → 最近一次入站消息 sender（轮次发起者近似——每 conv 轮次串行，
    /// 审批询问/流式卡发起时取最近 sender 编码进卡片 value 与 pending 登记）。
    conv_senders: Arc<Mutex<HashMap<String, String>>>,
    /// 评论 conv（`feishu:comment:<file_token>`）→ 最近评论 comment_id（回复目标
    /// 锚点表——会话锚放宽后 conv 不再内嵌 comment_id，drain 收到评论事件时登记，
    /// 发送侧据此路由回复；存量内嵌形态 conv 兜底）。
    comment_anchors: Arc<Mutex<HashMap<String, String>>>,
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
    /// Wave B-6：群 conv → 本轮发起消息 id（回复锚点表）。drain 收到普通群消息
    /// 时登记（见 [`group_reply_anchor`]），发送侧（send_text / 卡片）据此用
    /// reply API 把回复锚回发起消息。取舍：conv 级近似（与 conv_senders 同姿态
    /// ——每 conv 轮次串行，运行中他人新消息会更新锚点，误差窗口极窄）。
    reply_anchors: Arc<Mutex<HashMap<String, String>>>,
    /// W3-5：最近一条入站消息 id（锚点候选；send_typing 轮次锚定时提升）。
    last_inbound: Arc<Mutex<HashMap<String, String>>>,
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
        // P6-1：群消息 @bot 过滤策略——共享句柄（`/config require_mention`
        // 热切换对下一消息生效；重启回 config 值）。
        let mention_policy: Arc<RwLock<crate::proto::MentionPolicy>> =
            Arc::new(RwLock::new(crate::proto::MentionPolicy {
                require_mention_in_group,
            }));
        let policy_for_drain = mention_policy.clone();
        // pending 询问卡登记：drain task 也要查（过期询问的按钮点击反馈，见
        // drain 内 card.action 分支）——先建后共享给 Self。
        let pending_asks: Arc<Mutex<HashMap<String, PendingAskCard>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_asks_for_drain = pending_asks.clone();
        // conv 发起者 / 话题活跃窗口（drain task 与发送侧共享）。
        let conv_senders: Arc<Mutex<HashMap<String, String>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let conv_senders_for_drain = conv_senders.clone();
        // 评论回复目标锚点表（drain 登记、发送侧消费）。
        let comment_anchors: Arc<Mutex<HashMap<String, String>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let comment_anchors_for_drain = comment_anchors.clone();
        let thread_active: Arc<Mutex<HashMap<String, Instant>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let thread_active_for_drain = thread_active.clone();
        // Wave B-6：群 conv 回复锚点表（send_typing 轮次锚定时提升、发送侧消费）。
        let reply_anchors: Arc<Mutex<HashMap<String, String>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // W3-5：最近一条入站消息（锚点候选）——drain 登记；send_typing（core 每轮
        // 开始调用）提升为回复锚点，运行中他人新消息不再抢走本轮 reply 锚。
        let last_inbound: Arc<Mutex<HashMap<String, String>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let last_inbound_for_drain = last_inbound.clone();
        // Wave B-8：话题免 @ 窗口（config 注入；0 = 关闭）。
        let thread_active_window = thread_window_of(thread_active_window_secs);
        // W3-1：语音转文字开关（drain 侧消费）。
        let asr_enabled_for_drain = asr_enabled;
        tokio::spawn(async move {
            let mut payload_rx = payload_rx;
            while let Some(payload) = payload_rx.recv().await {
                // 三类事件：普通消息（含媒体下载）/ 审批按钮回调 / 云文档评论。
                // P6-1：群消息的 @bot 过滤与 @bot 文本剥离需要 bot open_id——
                // 首个群消息事件懒取（与评论事件共用缓存），失败退化为弱过滤。
                if is_group_message_event(&payload) {
                    ensure_bot_open_id(
                        &bot_open_id_for_drain,
                        &token_for_drain,
                        &core_config_for_drain,
                        &app_id_for_drain,
                        &app_secret_for_drain,
                    )
                    .await;
                }
                let bot = bot_open_id_for_drain.read().await.clone();
                let mut policy = *policy_for_drain.read().await;
                // 话题群近期活跃免 @：该话题 THREAD_ACTIVE_WINDOW 内有过消息则
                // 本条豁免 require_mention（追问场景免于每条 @）。普通群
                // thread_key 不命中，不豁免。Wave B-8：窗口时长改 config 注入
                //（thread_active_window，0 = 关闭豁免）。
                let thread_key = thread_key_of_payload(&payload);
                if !thread_active_window.is_zero() {
                    if let Some(tk) = &thread_key {
                        if thread_active_for_drain
                            .lock()
                            .await
                            .get(tk)
                            .is_some_and(|t| t.elapsed() < thread_active_window)
                        {
                            policy.require_mention_in_group = false;
                        }
                    }
                }
                if let Some((msgid, mut msg, pending)) =
                    parse_message_event(&payload, &policy, bot.as_deref())
                {
                    if !dedup.check(&msgid) {
                        continue;
                    }
                    // 记录 conv 发起者（审批卡/终止按钮的发起者校验锚）与话题
                    // 活跃时刻（免 @ 窗口续期；有界防泄漏）。
                    conv_senders_for_drain
                        .lock()
                        .await
                        .insert(msg.conv_id.0.clone(), msg.sender.0.clone());
                    // Wave B-6/W3-5：普通群消息登记**锚点候选**（最近一条入站消息
                    // id）——send_typing（core 每轮开始调用）提升为回复锚点后，
                    // 发送侧据此用 reply API 把回复/卡片锚回该消息（私聊/话题/
                    // 评论不登记：私聊无引用需求，话题已锚 root，评论走评论回复）。
                    if let Some((conv, anchor)) =
                        group_reply_anchor(&msg.conv_id.0, msg.source_msg_id.as_deref())
                    {
                        last_inbound_for_drain.lock().await.insert(conv, anchor);
                    }
                    if let Some(tk) = &thread_key {
                        let mut m = thread_active_for_drain.lock().await;
                        if m.len() > 512 {
                            m.clear(); // 粗上限：超量整体重置（窗口语义无损）。
                        }
                        m.insert(tk.clone(), Instant::now());
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
                            // W3-1：语音资源同 file 走 message-resource 接口。
                            "file" | "audio" => {
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
                                    "file" | "audio" => {
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
                            Ok(bytes) => {
                                // W3-1：语音 → speech_to_text 转写（不落盘），
                                // 文本以【语音】前缀进 prompt；失败回退媒体错误
                                // 提示（fail-soft——用户收到可行动反馈而非静默）。
                                if p.kind == "audio" {
                                    if asr_enabled_for_drain {
                                        match crate::client::transcribe_audio(
                                            &core_config_for_drain,
                                            &token,
                                            bytes,
                                        )
                                        .await
                                        {
                                            Ok(t) => {
                                                let text = format!("【语音】{t}");
                                                msg.text = match msg.text.take() {
                                                    Some(prev) if !prev.trim().is_empty() => {
                                                        Some(format!("{prev}\n\n{text}"))
                                                    }
                                                    _ => Some(text),
                                                };
                                            }
                                            Err(e) => {
                                                warn!(target: "feishu", error = %e, "语音转写失败");
                                                msg.media_errors.push(format!(
                                                    "语音转写失败: {e}（可改发文字）"
                                                ));
                                            }
                                        }
                                    } else {
                                        msg.media_errors.push(
                                            "语音转写已关闭（feishu_asr_enabled=false），请改发文字"
                                                .to_string(),
                                        );
                                    }
                                    continue;
                                }
                                match persist_media(p.kind, &p.key, p.file_name.as_deref(), &bytes)
                                {
                                    Ok(path) => msg.media.push(MediaRef {
                                        kind: p.kind.to_string(),
                                        url: path,
                                    }),
                                    Err(e) => {
                                        warn!(target: "feishu", error = %e, "媒体落盘失败，跳过");
                                        msg.media_errors.push(format!("{}: 落盘失败: {e}", p.key));
                                    }
                                }
                            }
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
                // 合并转发消息（merged_forward，完整支持——替换 v1.12.0 的「暂不
                // 支持」快赢）：按 meta.message_id 调「查询合并转发消息列表」API
                // 分页拉全子消息 → 转录为文本注入 agent（转录见
                // proto::render_merge_forward_transcript，拉取见 client::list_merge_forward）。
                // 走既有 dedup 管线；不产生 PendingMedia（子消息图片/文件一期只
                // 占位不下载，与媒体下载管线无冲突）；群内仍要求 @bot（parse 内
                // 沿用 group_mention_ok，无特判）。
                if let Some((key, mut mf_msg, meta)) =
                    parse_merged_forward_event(&payload, &policy, bot.as_deref())
                {
                    if !dedup.check(&key) {
                        continue;
                    }
                    // 与普通消息一致的登记（转录消息同样发起一轮 agent）：conv 发起
                    // 者（审批卡点击者校验锚）/ 锚点候选（W3-5：send_typing 轮次
                    // 锚定时提升为回复锚点）/ 话题活跃免 @ 续期。
                    conv_senders_for_drain
                        .lock()
                        .await
                        .insert(mf_msg.conv_id.0.clone(), mf_msg.sender.0.clone());
                    if let Some((conv, anchor)) =
                        group_reply_anchor(&mf_msg.conv_id.0, mf_msg.source_msg_id.as_deref())
                    {
                        last_inbound_for_drain.lock().await.insert(conv, anchor);
                    }
                    if let Some(tk) = &thread_key {
                        let mut m = thread_active_for_drain.lock().await;
                        if m.len() > 512 {
                            m.clear(); // 粗上限：超量整体重置（窗口语义无损）。
                        }
                        m.insert(tk.clone(), Instant::now());
                    }
                    // 拉子消息（token lazy 缓存 + 失效码清缓存自愈一次，与媒体
                    // 下载路径同姿态，见 fetch_merge_forward_items）。
                    let fetched = fetch_merge_forward_items(
                        &core_config_for_drain,
                        &token_for_drain,
                        &app_id_for_drain,
                        &app_secret_for_drain,
                        &meta.message_id,
                    )
                    .await;
                    match merge_forward_outcome(
                        &fetched,
                        meta.title.as_deref(),
                        meta.summary.as_deref(),
                    ) {
                        MergeForwardOutcome::Agent(text) => {
                            // 转录块作为消息文本送入 agent（占位正文在此替换）。
                            mf_msg.text = Some(text);
                            if inbound_msg_tx.send(mf_msg).await.is_err() {
                                break;
                            }
                        }
                        MergeForwardOutcome::Fallback(notice) => {
                            // 拉取失败（权限/网络/消息过期）：回可行动提示，不进
                            // agent——占位正文不外泄；dedup 已消费，事件重投不会
                            // 反复打提示。
                            warn!(
                                target: "feishu",
                                message_id = %meta.message_id,
                                "拉取合并转发子消息失败，回退提示"
                            );
                            send_drain_text(
                                &core_config_for_drain,
                                &token_for_drain,
                                &app_id_for_drain,
                                &app_secret_for_drain,
                                &mf_msg.conv_id,
                                &notice,
                            )
                            .await;
                        }
                    }
                    continue;
                }
                // P4-4：审批按钮回调（card.action.trigger）→ text="y"/"n" 的
                // 入站消息，core 的审批回复路由消费（parse_reply("y")=allow）。
                // 安全批次扩展：回调解析带第三元素 deny（命令按钮过期 / 群内他人
                // 点终止——proto 侧已判，此处回提示后丢弃，不进 core 分派）；
                // 审批按钮另做**发起者校验**（群 conv 下点击者须为登记的发起者，
                // 私聊单人免检）。
                if let Some((key, reply_msg, deny)) = parse_card_action_event(&payload) {
                    if let Some(deny_text) = deny {
                        if !dedup.check(&key) {
                            continue;
                        }
                        send_drain_text(
                            &core_config_for_drain,
                            &token_for_drain,
                            &app_id_for_drain,
                            &app_secret_for_drain,
                            &reply_msg.conv_id,
                            &deny_text,
                        )
                        .await;
                        // 安全批次（转发代批）：deny 文案回原 conv 之外，给点击者
                        // （operator）私聊补一条同文案——转发场景下原 conv 里没人
                        // 知道有人替点了按钮，第二触达让点击者明确知道被拒。
                        // 占位消息的 sender 即 operator open_id（见 dummy_card_action_msg）。
                        if !reply_msg.sender.0.is_empty()
                            && reply_msg.conv_id.0 != format!("feishu:{}", reply_msg.sender.0)
                        {
                            send_drain_text(
                                &core_config_for_drain,
                                &token_for_drain,
                                &app_id_for_drain,
                                &app_secret_for_drain,
                                &ConvId(format!("feishu:{}", reply_msg.sender.0)),
                                &deny_text,
                            )
                            .await;
                        }
                        continue;
                    }
                    if !dedup.check(&key) {
                        continue;
                    }
                    // conv 发起者更新（按钮触发的轮次由点击者发起）。
                    if !reply_msg.sender.0.is_empty() {
                        conv_senders_for_drain
                            .lock()
                            .await
                            .insert(reply_msg.conv_id.0.clone(), reply_msg.sender.0.clone());
                    }
                    // 过期反馈：req 已不在 pending_asks（询问已批准/拒绝/中断/
                    // 超时收敛，或复用槽换了新请求）→ 回一条「已过期」提示而非
                    // 静默丢进 core 的 miss 分支。无 req 的回调（命令按钮等）
                    // 不受影响。
                    if let Some(req) = reply_msg.ask_req.clone() {
                        let pending = pending_asks_for_drain.lock().await.get(&req).cloned();
                        match pending {
                            None => {
                                notify_expired_ask(
                                    &core_config_for_drain,
                                    &token_for_drain,
                                    &app_id_for_drain,
                                    &app_secret_for_drain,
                                    &reply_msg.conv_id,
                                )
                                .await;
                                continue;
                            }
                            // 发起者校验（群 conv）：询问由发起者登记，他人点击
                            // 回明确提示（防群里任何人替批高危操作）。
                            Some(card) => {
                                if !card.sender.is_empty()
                                    && !is_private_conv(&card.conv_id)
                                    && card.sender != reply_msg.sender.0
                                {
                                    send_drain_text(
                                        &core_config_for_drain,
                                        &token_for_drain,
                                        &app_id_for_drain,
                                        &app_secret_for_drain,
                                        &reply_msg.conv_id,
                                        &format!(
                                            "⛔ 该询问由 {} 发起，仅其本人可答复。",
                                            card.sender
                                        ),
                                    )
                                    .await;
                                    continue;
                                }
                            }
                        }
                    }
                    if inbound_msg_tx.send(reply_msg).await.is_err() {
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
                    ensure_bot_open_id(
                        &bot_open_id_for_drain,
                        &token_for_drain,
                        &core_config_for_drain,
                        &app_id_for_drain,
                        &app_secret_for_drain,
                    )
                    .await;
                    let bot = bot_open_id_for_drain.read().await.clone();
                    if let Some((key, comment_id, cm)) =
                        parse_comment_event(&payload, bot.as_deref())
                    {
                        // 会话锚放宽：登记回复目标锚点（conv → comment_id）——发送
                        // 侧（send_text/send_media 评论分支）据此路由回复。
                        if dedup.check(&key) {
                            comment_anchors_for_drain
                                .lock()
                                .await
                                .insert(cm.conv_id.0.clone(), comment_id);
                            if inbound_msg_tx.send(cm).await.is_err() {
                                break;
                            }
                        }
                    } else {
                        tracing::debug!(target: "feishu", "评论未 @bot（或字段缺失/纯@），丢弃");
                    }
                    continue;
                }
                // W3-2：表情回应快速审批（im.message.reaction.created_v1）——用户在
                // 审批卡上回应 👍/👎 等价点允许/拒绝按钮（比点开卡片更轻的交互）。
                // 反查 pending_asks 按被回应消息 id 定位询问；群 conv 下操作者须为
                // 发起者（与按钮同门槛，防代批）；非审批卡上的 emoji 静默忽略。
                // 合成 text="y"/"n" + reply_to 锚定的入站消息，core 三级路由精确
                // 消费（与引用回复同路径）。需在飞书后台订阅该事件（可选）。
                if let Some((key, operator, reacted_msg, reply)) =
                    crate::proto::parse_reaction_event(&payload)
                {
                    if !dedup.check(&key) {
                        continue;
                    }
                    let hit = pending_asks_for_drain
                        .lock()
                        .await
                        .iter()
                        .find(|(_, c)| c.msg_id == reacted_msg)
                        .map(|(_, c)| c.clone());
                    if let Some(card) = hit {
                        if !card.sender.is_empty()
                            && !is_private_conv(&card.conv_id)
                            && card.sender != operator
                        {
                            send_drain_text(
                                &core_config_for_drain,
                                &token_for_drain,
                                &app_id_for_drain,
                                &app_secret_for_drain,
                                &ConvId(card.conv_id.clone()),
                                &format!("⛔ 该询问由 {} 发起，仅其本人可答复。", card.sender),
                            )
                            .await;
                            continue;
                        }
                        let reaction_msg = InboundMessage {
                            conv_id: ConvId(card.conv_id.clone()),
                            sender: imagent_core::UserId(operator),
                            text: Some(reply.to_string()),
                            media: Vec::new(),
                            media_errors: Vec::new(),
                            mentions: Vec::new(),
                            mentioned_bot: false,
                            ask_req: None,
                            reply_to: Some(reacted_msg),
                            source_msg_id: None,
                            control: None,
                            reply_hint: ReplyHint::None,
                        };
                        if inbound_msg_tx.send(reaction_msg).await.is_err() {
                            break;
                        }
                    }
                    continue;
                }
                // 自定义菜单跳转（application.url.menu_v6）→ 合成 text="/help" 的
                // 入站消息（复用 card action 的合成模式）：走与手打 /help 完全相同
                // 的鉴权/分派路径。需在飞书后台订阅该事件（可选，见 README）。
                if let Some((key, menu_msg)) = parse_menu_event(&payload) {
                    if dedup.check(&key) && inbound_msg_tx.send(menu_msg).await.is_err() {
                        break;
                    }
                    continue;
                }
                // 消息撤回（im.message.recalled_v1，一期）→ 控制消息：core 据此
                // 把同 id 的排队消息移出（在飞任务不自动停，只回提示）。需订阅
                // 该事件（可选，见 README）。
                if let Some((key, recall_msg)) = parse_recall_event(&payload) {
                    if dedup.check(&key) && inbound_msg_tx.send(recall_msg).await.is_err() {
                        break;
                    }
                    continue;
                }
                // bot 被移出群（im.chat.member.bot.deleted_v1）→ 控制消息：core
                // 据此收回会话白名单并通知管理员。需订阅该事件（可选，见 README）。
                if let Some((key, removed_msg)) = parse_bot_removed_event(&payload) {
                    if dedup.check(&key) && inbound_msg_tx.send(removed_msg).await.is_err() {
                        break;
                    }
                    continue;
                }
                // W3-4：bot 被加入群（im.chat.member.bot.added_v1）→ 欢迎引导
                //（含 /chat allow 放行指引——放行前 core 白名单不会放行群消息）。
                // 需订阅该事件（可选）。欢迎语平台层直发（与移出群通知管理员同
                // 模式），无敏感信息。
                if let Some((key, chat_id)) = crate::proto::parse_bot_added_event(&payload) {
                    if dedup.check(&key) {
                        send_drain_text(
                            &core_config_for_drain,
                            &token_for_drain,
                            &app_id_for_drain,
                            &app_secret_for_drain,
                            &ConvId(format!("feishu:{chat_id}")),
                            "👋 我已加入本群！群内 @我 发消息即可驱动 agent。\n管理员可发送 /chat allow 放行本群（放行前我不会响应消息）；/help 查看全部命令。\n💬 会话规则：群主时间线直接 @我 = 续同一会话；点消息「回复」进话题 = 开独立会话（互不共享上下文/待办）。",
                        )
                        .await;
                    }
                    continue;
                }
                // 不支持类型提示：语音/分享卡片等此前静默丢弃，用户无感知——回一条
                // 可读提示（p2p 直回；群消息近似按「带 @」门槛发——白名单校验在
                // core 侧，drain 无白名单状态，见 proto 注释）。
                if let Some((notice, Some(conv))) = unsupported_message_notice(&payload) {
                    send_drain_text(
                        &core_config_for_drain,
                        &token_for_drain,
                        &app_id_for_drain,
                        &app_secret_for_drain,
                        &conv,
                        notice,
                    )
                    .await;
                    continue;
                }
                // 真机排障：兜底分类——已知「正常忽略」的事件（策略过滤的群消息、
                // 表情回执/自身回声）降 DEBUG，避免淹没真正需要排障的 WARN
                //（真机校准 2026-09-01：V2/V3 期间大量正常消息被记成 WARN 误导视线）。
                let head: String = String::from_utf8_lossy(&payload)
                    .chars()
                    .take(400)
                    .collect();
                let etype = serde_json::from_slice::<serde_json::Value>(&payload)
                    .ok()
                    .and_then(|v| {
                        v.get("header")?
                            .get("event_type")?
                            .as_str()
                            .map(str::to_string)
                    });
                match etype.as_deref() {
                    Some("im.message.receive_v1") => {
                        debug!(target: "feishu", payload_head = %head, "消息未过准入策略（如群内未@），忽略");
                    }
                    Some("im.message.reaction.created_v1") => {
                        debug!(target: "feishu", payload_head = %head, "表情事件回执（多为自身回声），忽略");
                    }
                    _ => warn!(target: "feishu", payload_head = %head, "无法解析/非目标事件，丢弃"),
                }
            }
        });

        Ok(Self {
            core_config,
            app_id,
            app_secret,
            token,
            card_seqs: Arc::new(Mutex::new(HashMap::new())),
            card_footers: Arc::new(Mutex::new(HashMap::new())),
            ask_notes: Arc::new(Mutex::new(HashMap::new())),
            card_tail: Arc::new(Mutex::new(HashMap::new())),
            msg_reactions: Arc::new(Mutex::new(HashMap::new())),
            managed_card_msgs: Arc::new(Mutex::new(HashMap::new())),
            reconnect,
            inbound_rx: Arc::new(Mutex::new(inbound_msg_rx)),
            pending_asks,
            ask_slots: Arc::new(Mutex::new(HashMap::new())),
            asks_since_card: Arc::new(Mutex::new(HashMap::new())),
            mention_policy,
            conv_senders,
            comment_anchors,
            ask_timeout_secs,
            quiet_hours,
            reply_anchors,
            last_inbound,
            text_split_max: message_max_len
                .unwrap_or(FEISHU_TEXT_MAX)
                .min(FEISHU_TEXT_MAX),
            comment_split_max: message_max_len
                .unwrap_or(FEISHU_COMMENT_TEXT_MAX)
                .min(FEISHU_COMMENT_TEXT_MAX),
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
                    Err(e) if is_rate_limited_msg(&e.to_string()) => {
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
                            if is_rate_limited_msg(&e.to_string()) {
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
    async fn patch_footer_if_changed(&self, token: &str, card_id: &str, footer: &str) {
        let changed = {
            let mut m = self.card_footers.lock().await;
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
            if is_rate_limited_msg(&e.to_string()) {
                tracing::warn!(target: "feishu", card_id, "footer patch 限流，丢帧并回滚缓存");
                self.card_footers.lock().await.remove(card_id);
            } else {
                tracing::warn!(target: "feishu", error = %e, "footer patch 失败（不影响主流程）");
            }
        }
    }
    /// 登记一张 pending 询问卡；同 request_id 的旧卡 patch 成 superseded
    ///（异常重发场景，正常路径 request_id 唯一）。best-effort。
    async fn record_pending_ask(
        &self,
        request_id: &str,
        conv_id: &str,
        msg_id: &str,
        tool_name: &str,
        sender: &str,
    ) {
        let superseded = self.pending_asks.lock().await.insert(
            request_id.to_string(),
            PendingAskCard {
                conv_id: conv_id.to_string(),
                msg_id: msg_id.to_string(),
                tool_name: tool_name.to_string(),
                sender: sender.to_string(),
            },
        );
        if let Some(old) = superseded {
            // P8-2：同卡复用重登记（复用槽换 request_id）不是「取代」——同一张卡
            // 不能 patch 成 superseded 顶掉自己刚挂上的新询问。
            if old.msg_id == msg_id {
                return;
            }
            let card_json = crate::card::render_permission_card_superseded(&old.tool_name);
            if let Err(e) = self
                .with_token(|t| {
                    let old_mid = old.msg_id.clone();
                    let card_json = card_json.clone();
                    async move { patch_card(&self.core_config, &t, &old_mid, &card_json).await }
                })
                .await
            {
                warn!(target: "feishu", error = %e, "旧询问卡取代收敛失败（无害）");
            }
        }
    }

    /// conv 最近一次入站消息的 sender（轮次发起者近似——每 conv 轮次串行，询问
    /// 登记时取之，作群 conv 下的按钮点击者校验锚；无记录为空串=不校验）。
    /// 刷新会话最新卡片记录（card_tail，强提醒加急对象）。
    async fn note_card_tail(&self, conv_id: &str, msg_id: &str) {
        if msg_id.starts_with("om_") {
            self.card_tail
                .lock()
                .await
                .insert(conv_id.to_string(), msg_id.to_string());
        }
    }

    async fn last_sender(&self, conv_id: &str) -> String {
        self.conv_senders
            .lock()
            .await
            .get(conv_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Wave B-6：该 conv 的回复锚点（最近一条普通群消息的 message_id；无则 None）。
    async fn reply_anchor(&self, conv_id: &str) -> Option<String> {
        self.reply_anchors
            .lock()
            .await
            .get(conv_id)
            .cloned()
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

    /// P8-2：登记一张**新发**的询问卡：pending 登记（request_id 路由）+ 复用槽
    /// （收敛后供下一个询问原地复用）+ 顶起标记（终态结果下沉判定）。
    /// 安全批次：发起者（最近 sender）一并登记（群 conv 点击者校验）。
    async fn register_ask_card(
        &self,
        conv_id: &str,
        msg_id: &str,
        request_id: &str,
        tool_name: &str,
        render: AskRender,
    ) {
        let sender = self.last_sender(conv_id).await;
        self.note_card_tail(conv_id, msg_id).await;
        self.record_pending_ask(request_id, conv_id, msg_id, tool_name, &sender)
            .await;
        self.ask_slots.lock().await.insert(
            conv_id.to_string(),
            AskSlot {
                msg_id: msg_id.to_string(),
                pending_req: Some(request_id.to_string()),
                resolved_at: None,
                render,
            },
        );
        self.mark_ask_sent(conv_id).await;
    }

    /// P8-2：标记本轮流式卡之后发过询问卡（终态「结果下沉」判定）。
    async fn mark_ask_sent(&self, conv_id: &str) {
        self.asks_since_card
            .lock()
            .await
            .insert(conv_id.to_string(), true);
    }

    /// P8-2：取出并清除「发过询问卡」标记（终态消费一次）。
    async fn take_asks_flag(&self, conv_id: &str) -> bool {
        self.asks_since_card
            .lock()
            .await
            .remove(conv_id)
            .unwrap_or(false)
    }

    /// P8-2：释放该 conv 的复用槽（询问收敛后调用——卡保留在 IM 里，下一个
    /// 询问原地 patch 复用，不另发新卡）。
    async fn free_ask_slot(&self, conv_id: &str, request_id: &str) {
        if let Some(slot) = self.ask_slots.lock().await.get_mut(conv_id) {
            if slot.pending_req.as_deref() == Some(request_id) {
                slot.pending_req = None;
                slot.resolved_at = Some(std::time::Instant::now());
            }
        }
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
        // 登记的锚点表（最近一条评论）；存量 conv 的内嵌形态兜底。两者皆无（进程
        // 刚重启、锚点表为空）无法定位评论线程，如实报错。
        if let Some((file_token, legacy_cid)) = comment_target_from_conv(conv) {
            let comment_id = self
                .comment_anchors
                .lock()
                .await
                .get(&conv.0)
                .cloned()
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

/// Wave B-6：群 conv 回复锚点判定（纯函数，便于单测）——**普通群**消息（conv
/// `feishu:oc_…` 且无话题 root 后缀、非评论线程、非私聊 `ou_`）带平台消息 id
/// 才登记；话题群已有 root 锚（回复天然落回话题），私聊无引用需求，评论走
/// 评论回复 API。返回 `(conv, message_id)`。
fn group_reply_anchor(conv: &str, source_msg_id: Option<&str>) -> Option<(String, String)> {
    let mid = source_msg_id.filter(|m| m.starts_with("om_"))?;
    if is_private_conv(conv) || comment_target_from_conv(&ConvId(conv.to_string())).is_some() {
        return None;
    }
    // 话题群 conv 形态 `feishu:<chat>:<root>`（两个冒号段）——不登记。
    if thread_target_from_conv(&ConvId(conv.to_string())).is_some() {
        return None;
    }
    Some((conv.to_string(), mid.to_string()))
}

/// 空串 sender → None（AskRender.sender 的 Option 形态适配渲染入参）。
fn sender_opt_of(sender: &str) -> Option<&str> {
    (!sender.is_empty()).then_some(sender)
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

/// 把媒体字节落盘到 `~/.imagent/media/`，返回本地路径字符串。
///
/// 原名透传（安全批次）：file 消息带原始 `file_name`（含扩展名）——按原名落盘，
/// agent 侧拿到的文件名/扩展名与用户发送的一致（此前统一 `<key>.bin`，下游按
/// 扩展名识别格式会失效）。文件名做净化（剥路径分隔符，防 `../` 逃逸）；缺原名的
/// file 与图片回退 `<key>.<默认扩展名>`。图片消息 content 无原始文件名，真实格式
/// 只有飞书侧知道（image_key 不带扩展信息）——默认 **png**（无损通用形态，jpg
/// 有损假设会二次压缩误导；下载字节原样落盘，仅扩展名标注取舍）。
/// 照 ilink `persist_media`：目录 0700、文件 0600（解密后的私聊媒体不暴露给同机其他用户）。
/// 取舍：原名不再天然全局唯一（同名文件后到覆盖先到）——换「agent 拿到真实文件
/// 名/扩展名」的收益，覆盖窗口极窄（同会话同名连发），可接受。
fn persist_media(kind: &str, key: &str, file_name: Option<&str>, bytes: &[u8]) -> Result<String> {
    let dir = media_dir()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    // 净化：剥路径分隔符与目录段，仅留文件名本体；空/全非法回退资源 key。
    let safe_name = file_name
        .map(|n| {
            n.rsplit(['/', '\\'])
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .filter(|n| !n.is_empty());
    let name = match (kind, safe_name) {
        // file：原名原名扩展名整体保留（无扩展名也照旧——原样最忠实）。
        ("file", Some(n)) => n,
        // image：content 无原名；若 post/file 路径带名则用其扩展名，否则默认 png。
        ("image", Some(n)) => {
            let ext = n.rsplit('.').next().unwrap_or("");
            let base = n.rsplit_once('.').map(|(b, _)| b).unwrap_or(n.as_str());
            let ext = if ext.is_empty() || ext == n {
                "png"
            } else {
                ext
            };
            format!("{base}.{ext}")
        }
        _ => {
            let ext = if kind == "image" { "png" } else { "bin" };
            format!("{key}.{ext}")
        }
    };
    // W4-3：同名媒体不覆盖——后到的文件加序号后缀（此前后到覆盖先到，用户
    // 连发同名文件时前一份丢失、agent 读到错文件）。
    let path = {
        let p = dir.join(&name);
        if !p.exists() {
            p
        } else {
            let stem = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("media")
                .to_string();
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{e}"))
                .unwrap_or_default();
            let mut i = 1u32;
            loop {
                let cand = dir.join(format!("{stem}-{i}{ext}"));
                if !cand.exists() {
                    break cand;
                }
                i += 1;
            }
        }
    };
    std::fs::write(&path, bytes)
        .map_err(|e| CoreError::Platform(PLATFORM, format!("write media {path:?}: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(path.to_string_lossy().into_owned())
}

/// P6-1：确保 bot open_id 已取到（懒取 + 缓存；群消息 @bot 过滤与评论 @bot 过滤
/// 共用）。已有缓存直接返回；取失败只 warn 不缓存失败——下次相关事件再试。
async fn ensure_bot_open_id(
    bot_open_id: &Arc<RwLock<Option<String>>>,
    token_lock: &Arc<RwLock<Option<(String, Instant)>>>,
    core_config: &CoreConfig,
    app_id: &str,
    app_secret: &str,
) {
    if bot_open_id.read().await.is_some() {
        return;
    }
    let fetched = async {
        let t = fetch_cached_token(token_lock, core_config, app_id, app_secret).await?;
        fetch_bot_open_id(core_config, &t).await
    }
    .await;
    match fetched {
        Ok(b) => *bot_open_id.write().await = Some(b),
        Err(e) => warn!(
            target: "feishu",
            error = %e,
            "取 bot open_id 失败，@bot 过滤退化为弱过滤（须含 @）"
        ),
    }
}

/// 拉取合并转发子消息（drain task 用）：token lazy 缓存取用，遇 token 失效类
/// 错误码清缓存强制刷新后重试一次（与媒体下载路径同姿态），其余错误如实上抛
/// 由调用方走回退提示。
async fn fetch_merge_forward_items(
    core_config: &CoreConfig,
    token_lock: &Arc<RwLock<Option<(String, Instant)>>>,
    app_id: &str,
    app_secret: &str,
    message_id: &str,
) -> Result<Vec<MergedForwardItem>> {
    let t = fetch_cached_token(token_lock, core_config, app_id, app_secret).await?;
    match list_merge_forward(core_config, &t, message_id).await {
        Err(e) if crate::client::is_token_invalid_msg(&e.to_string()) => {
            warn!(target: "feishu", error = %e, "合并转发拉取遇 token 失效码，清缓存刷新后重试一次");
            *token_lock.write().await = None;
            let fresh = fetch_cached_token(token_lock, core_config, app_id, app_secret).await?;
            list_merge_forward(core_config, &fresh, message_id).await
        }
        other => other,
    }
}

/// 合并转发消息的 drain 产出（纯函数，便于单测）：
/// - `Agent`：拉取成功 → 消息正文（「（以下为用户转发的聊天记录）」前缀 + 转录
///   块），drain 回填 `msg.text` 后进 agent；
/// - `Fallback`：拉取失败（权限/网络/消息过期）→ 用户可读提示文案，**不进
///   agent**——drain 回提示后丢弃消息（占位正文不外泄，见 drain 分支注释）。
#[derive(Debug)]
enum MergeForwardOutcome {
    Agent(String),
    Fallback(String),
}

/// [`MergeForwardOutcome`] 的决策函数：把 `client::list_merge_forward` 的结果映射
/// 为入站正文或回退提示（转录头元数据来自事件 content 的尽力解析）。
fn merge_forward_outcome(
    fetched: &Result<Vec<MergedForwardItem>>,
    title: Option<&str>,
    summary: Option<&str>,
) -> MergeForwardOutcome {
    match fetched {
        Ok(items) => MergeForwardOutcome::Agent(format!(
            "（以下为用户转发的聊天记录）\n\n{}",
            render_merge_forward_transcript(items, title, summary)
        )),
        Err(e) => MergeForwardOutcome::Fallback(format!(
            "⚠️ 无法读取合并转发内容（{e}），请直接复制文字发送"
        )),
    }
}

/// drain 侧 best-effort 文本发送（评论/话题/普通 conv 三路）：按钮 deny 提示、
/// 过期询问提示、不支持类型提示共用。发送失败仅 warn（提示丢失无害）。
/// 评论 conv：新形态（无内嵌 comment_id）在 drain 侧无回复锚点（锚点在消息元数据
/// 里，drain 只剩 conv）——跳过不回，仅记日志；存量内嵌形态照常回复评论。
async fn send_drain_text(
    core_config: &CoreConfig,
    token_lock: &Arc<RwLock<Option<(String, Instant)>>>,
    app_id: &str,
    app_secret: &str,
    conv: &ConvId,
    text: &str,
) {
    let send = async {
        let t = fetch_cached_token(token_lock, core_config, app_id, app_secret).await?;
        if let Some((file_token, comment_id)) = comment_target_from_conv(conv) {
            return match comment_id {
                Some(cid) => reply_comment(core_config, &t, &file_token, &cid, text)
                    .await
                    .map(|_| ()),
                // 新形态评论 conv 无锚点：无处可回，跳过（无害——提示性文案）。
                None => Ok(()),
            };
        }
        if let Some((_chat, root_id)) = thread_target_from_conv(conv) {
            reply_message(
                core_config,
                &t,
                &root_id,
                "text",
                &serde_json::json!({ "text": text }).to_string(),
            )
            .await
            .map(|_| ())
        } else if let Some((receive_id, kind)) = receive_target_from_conv(conv) {
            send_text_msg(core_config, &t, &receive_id, kind, text, false).await
        } else {
            Ok(())
        }
    };
    if let Err(e) = send.await {
        warn!(target: "feishu", error = %e, "drain 提示发送失败（无害）");
    }
}

/// 过期询问的点击反馈（drain task 用）：向该 conv 回一条「已过期」文本——
/// 询问卡收敛（批准/拒绝/中断/超时）后按钮仍在卡上，用户迟点不应静默无响应。
/// 评论 conv 的过期文案与聊天场景同句（回评论线程）。
/// best-effort：发送失败仅 warn（提示丢失无害，core 的 miss 分支照旧兜底丢弃）。
async fn notify_expired_ask(
    core_config: &CoreConfig,
    token_lock: &Arc<RwLock<Option<(String, Instant)>>>,
    app_id: &str,
    app_secret: &str,
    conv: &ConvId,
) {
    send_drain_text(
        core_config,
        token_lock,
        app_id,
        app_secret,
        conv,
        "⏳ 该询问已过期或已被处理，无需再次点击。",
    )
    .await;
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
    /// bot 对用户消息的表情标注：OnIt（在做了）→ DONE / CrossMark。
    /// emoji key 真机校准（2026-08）验证可用且**大小写敏感**（全大写报 231001）。
    /// 翻转 = 删旧表情 + 打新表情；删失败（过期/已撤回）仅 log，新表情照打。
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
        let tail = self.card_tail.lock().await.get(&conv.0).cloned();
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
            let comment_id = self
                .comment_anchors
                .lock()
                .await
                .get(&conv.0)
                .cloned()
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
        // agent 产出媒体回传（P6-7：按 kind 分流——image 走图片消息，其余走文件
        // 消息）：读本地文件 → 上传拿 key → 发消息。话题群 conv → reply API 落回话题。
        let thread = thread_target_from_conv(conv);
        let (receive_id, kind) = receive_target_from_conv(conv)
            .ok_or_else(|| CoreError::Platform(PLATFORM, format!("非法 conv_id: {}", conv.0)))?;
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
        if let Some(anchor) = self.last_inbound.lock().await.get(&conv.0).cloned() {
            self.reply_anchors
                .lock()
                .await
                .insert(conv.0.clone(), anchor);
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
        if let Some(slot) = self.ask_slots.lock().await.get_mut(&conv.0) {
            slot.pending_req = None;
            slot.resolved_at = Some(std::time::Instant::now());
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
            let slots = self.ask_slots.lock().await;
            let Some(slot) = slots.get(&conv.0) else {
                return Ok(());
            };
            let Some(req) = slot.pending_req.clone() else {
                return Ok(()); // 槽空闲（已收敛）——无未决审批可联动
            };
            (slot.msg_id.clone(), slot.render.clone(), req)
        };
        // 去重：note 不变不重画。
        {
            let mut notes = self.ask_notes.lock().await;
            if notes.get(&conv.0).map(String::as_str) == Some(note) {
                return Ok(());
            }
            notes.insert(conv.0.clone(), note.to_string());
        }
        // L17（code-review v8）：快照→网络→patch 期间终态可能已落——重渲染前
        // 复查 pending 仍是快照的 request_id，否则放弃（防终态卡被翻回带按钮
        // 的 pending 态误导点击；过期点击本有时效兜底，此处消歧义）。
        {
            let slots = self.ask_slots.lock().await;
            let still_pending = slots
                .get(&conv.0)
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
        // P8-2：新一轮流式卡——「之后发过询问卡」标记清零（conv 轮次串行，
        // 无并发覆盖问题）。
        self.asks_since_card
            .lock()
            .await
            .insert(conv.0.clone(), false);
        // 发起者（最近 sender）：编码进初始卡的 ⏹ 终止按钮 value（群 conv 下点击者
        // 校验）；Wave B-5：群 conv 初始卡顶部加「发起者」标注行。占位/未知为
        // None（旧语义，不校验）。
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
                        &render_stream_init_card(&conv_for_init, sender_opt.as_deref()),
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
        // 流式卡收成指针 stub，完整结果另发新卡落在会话最下面。标记取走即清
        // （300317 重试 / 重复终态不会二次重发）。
        let buried =
            !matches!(card.terminal, CardTerminal::Running) && self.take_asks_flag(&conv.0).await;
        let res = self
            .with_token(|token| async move {
                if let Some(card_id) = handle.strip_prefix("card:") {
                    // 真机校准（2026-08）：终态且未下沉时改**整卡 im patch**
                    // （render_card 折叠面板布局）——统一视觉：此前 managed 终态把
                    // 工具轨迹/思考过程内联进 md_body（长卡），而结果下沉新卡是折叠
                    // 面板，两形态不一致（用户反馈折叠更好）。Running 仍走 managed
                    // element 流式（打字机/节流/300317 自愈语义不变）。无映射
                    // （重启后）退回 managed 终态 patch（内联形态，可接受降级）。
                    if !buried
                        && !matches!(card.terminal, CardTerminal::Running)
                    {
                        if let Some(message_id) =
                            self.managed_card_msgs.lock().await.get(card_id).cloned()
                        {
                            let sender = self.last_sender(&conv.0).await;
                            let card_json = render_card(
                                card,
                                &conv.0,
                                (!sender.is_empty()).then_some(sender).as_deref(),
                            );
                            let res =
                                patch_card(&self.core_config, &token, &message_id, &card_json)
                                    .await;
                            if res.is_ok() {
                                // 终态清理（与 patch_managed 终态分支同语义——本分支
                                // 提前 return 会绕过那边的清理，防 per-card 状态泄漏）。
                                self.card_seqs.lock().await.remove(card_id);
                                self.card_footers.lock().await.remove(card_id);
                            }
                            return res;
                        }
                    }
                    match self.patch_managed(&token, card_id, card, buried).await {
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
                        *self.card_seqs.lock().await.entry(card_id.to_string()).or_insert(now) = now;
                        self.patch_managed(&token, card_id, card, buried).await
                    }
                    other => other,
                }
            } else if let Some(message_id) = handle.strip_prefix("msg:") {
                // Wave B-5：整卡重渲染带发起者标注行（群 conv）。
                let sender = self.last_sender(&conv.0).await;
                let card_json = if buried {
                    crate::card::render_stub_card(card)
                } else {
                    render_card(card, &conv.0, (!sender.is_empty()).then_some(sender).as_deref())
                };
                patch_card(&self.core_config, &token, message_id, &card_json).await
            } else {
                Err(CoreError::Platform(
                    PLATFORM,
                    format!("非法卡片句柄: {handle}"),
                ))
            }
            })
            .await;
        // 卡片不存在/已删除自愈（安全批次）：原卡片被用户删除/撤回后 patch 永远
        // 失败。清本平台 per-card 缓存（序列号/footer），错误附加 CARD_HANDLE_LOST
        // 哨兵——core CardSession 据此摘 live_cards 登记并把句柄置空（Running 期
        // 下帧重发新卡），启动扫描据此作废登记（终止无限重试）。
        let res = match res {
            Err(e) if is_card_not_exist_msg(&e.to_string()) => {
                warn!(target: "feishu", handle, "卡片不存在/已删除，清缓存并上报句柄丢失");
                if let Some(card_id) = handle.strip_prefix("card:") {
                    self.card_seqs.lock().await.remove(card_id);
                    self.card_footers.lock().await.remove(card_id);
                }
                Err(CoreError::Platform(
                    PLATFORM,
                    format!("{e}（{CARD_HANDLE_LOST}）"),
                ))
            }
            other => other,
        };
        // 真机校准（2026-08-30）：终态 patch 失败（超限 200860/230099 等）时原卡
        // 停在「思考中」——最小化终态卡重试一次，保证卡片必然收敛（完整内容由
        // core P5-11 纯文本兜底）。best-effort，再失败维持原错误上抛。
        let res = match res {
            Err(e)
                if !matches!(card.terminal, CardTerminal::Running)
                    && !e.to_string().contains(CARD_HANDLE_LOST) =>
            {
                let err_text = e.to_string();
                let done = matches!(card.terminal, CardTerminal::Done);
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
                        warn!(target: "feishu", error = %err_text, "终态 patch 失败，已用最小终态卡收敛");
                        Ok(())
                    }
                    Err(_) => Err(e),
                }
            }
            other => other,
        };
        // 结果下沉重发：流式卡已收敛成指针 → 完整结果另发新卡。Wave B-5：带
        // 发起者标注行。重发失败上抛 Err——core 的 P5-11 兜底会以纯文本补发全文
        //（结论不能因重发失败而丢）。
        if res.is_ok() && buried {
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
        }
        res
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
                if let Some((msgid, msg, _)) =
                    parse_message_event(&payload, &crate::proto::MentionPolicy::PERMISSIVE, None)
                {
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
                if let Some((_msgid, msg, _)) =
                    parse_message_event(&payload, &crate::proto::MentionPolicy::PERMISSIVE, None)
                {
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

    /// 原名透传落盘：file 用原始文件名（含扩展名）；图片无原名默认 png；带名图片
    /// 用其扩展名；路径穿越（../、分隔符）被净化；无名 file 回退 key.bin。
    /// W4-3：同名媒体不覆盖——第二次落盘加序号后缀。名字带进程 id + 纳秒防
    /// 历史残留（media 目录是真实 ~/.imagent/media，同机器多次跑测试会累积）。
    #[test]
    fn persist_media_same_name_gets_suffix() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let name = format!("同名{}-{nanos}.txt", std::process::id());
        let p1 = persist_media("file", "k9", Some(&name), b"first").unwrap();
        let p2 = persist_media("file", "k10", Some(&name), b"second").unwrap();
        assert_ne!(p1, p2, "同名不应覆盖: {p1} vs {p2}");
        let stem = name.strip_suffix(".txt").unwrap_or(&name);
        assert!(p2.ends_with(&format!("{stem}-1.txt")), "后缀序号: {p2}");
        assert_eq!(std::fs::read(&p1).unwrap(), b"first", "先到的文件内容保留");
        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
    }

    #[test]
    fn persist_media_original_name() {
        let dir = std::env::temp_dir().join(format!("imagent_persist_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // media_dir 固定 ~/.imagent/media，直接测同目录语义（写入该目录并清理）。
        let base = media_dir().unwrap();
        let p1 = persist_media("file", "k1", Some("报告 v2.pdf"), b"x").unwrap();
        assert!(p1.ends_with("报告 v2.pdf"), "{p1}");
        // 净化：路径分隔符段被剥。
        let p2 = persist_media("file", "k2", Some("../../evil.sh"), b"x").unwrap();
        assert!(p2.ends_with("evil.sh") && !p2.contains(".."), "{p2}");
        // 图片无原名：key.png（默认 png，取舍见函数注释）。
        let p3 = persist_media("image", "img_k3", None, b"x").unwrap();
        assert!(p3.ends_with("img_k3.png"), "{p3}");
        // 图片带名（未来路径）：用其扩展名。
        let p4 = persist_media("image", "img_k4", Some("photo.jpg"), b"x").unwrap();
        assert!(p4.ends_with("photo.jpg"), "{p4}");
        // 无名 file：key.bin。
        let p5 = persist_media("file", "k5", None, b"x").unwrap();
        assert!(p5.ends_with("k5.bin"), "{p5}");
        for f in [
            "报告 v2.pdf",
            "evil.sh",
            "img_k3.png",
            "photo.jpg",
            "k5.bin",
        ] {
            let _ = std::fs::remove_file(base.join(f));
        }
        let _ = dir;
    }

    /// P8-2：顶起标记——send_card 清零、发询问卡置位、终态取走即清。
    #[tokio::test]
    async fn asks_flag_roundtrip() {
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
        )
        .expect("构造");
        let conv = ConvId("feishu:ou_x".into());
        // send_card 的清零等价于直接写 false（方法本身需 HTTP，此处测标记语义）。
        p.asks_since_card.lock().await.insert(conv.0.clone(), false);
        assert!(!p.take_asks_flag(&conv.0).await, "未发询问 → 不下沉");
        p.mark_ask_sent(&conv.0).await;
        assert!(p.take_asks_flag(&conv.0).await, "发过询问 → 下沉");
        assert!(!p.take_asks_flag(&conv.0).await, "取走即清（不重复下沉）");
    }

    /// P8-2：同卡重登记（复用槽换 request_id）不是「取代」——同 msg_id 不走
    /// superseded patch（否则会把刚挂上的新询问顶掉）。guard 提前返回，无 HTTP。
    #[tokio::test]
    async fn record_pending_ask_same_card_not_superseded() {
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
        )
        .expect("构造");
        p.pending_asks.lock().await.insert(
            "r1".into(),
            PendingAskCard {
                conv_id: "feishu:ou_x".into(),
                msg_id: "m1".into(),
                tool_name: "Bash".into(),
                sender: "ou_owner".into(),
            },
        );
        // 同 request_id + 同卡重登记：不应触发 superseded（否则会真实发 HTTP）。
        p.record_pending_ask("r1", "feishu:ou_x", "m1", "Bash", "ou_owner")
            .await;
        let entry = p.pending_asks.lock().await.get("r1").cloned();
        assert!(
            entry.as_ref().is_some_and(|c| c.msg_id == "m1"),
            "登记保留: {entry:?}"
        );
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

    /// Wave B-6：群回复锚点判定——普通群消息（om_ 前缀）登记；私聊/话题群/
    /// 评论线程/非 om_ id 不登记。
    #[test]
    fn group_reply_anchor_only_plain_group() {
        // 普通群 + 平台消息 id：登记。
        assert_eq!(
            group_reply_anchor("feishu:oc_g", Some("om_123")),
            Some(("feishu:oc_g".to_string(), "om_123".to_string()))
        );
        // 私聊 / 话题群（带 root 后缀）/ 评论线程：不登记。
        assert!(
            group_reply_anchor("feishu:ou_u", Some("om_123")).is_none(),
            "私聊不登记"
        );
        assert!(
            group_reply_anchor("feishu:oc_g:om_root", Some("om_123")).is_none(),
            "话题群已锚 root，不登记"
        );
        assert!(
            group_reply_anchor("feishu:comment:ft", Some("om_123")).is_none(),
            "评论线程走评论回复，不登记"
        );
        // 无消息 id / 非 om_ 形态（防御）：不登记。
        assert!(group_reply_anchor("feishu:oc_g", None).is_none());
        assert!(group_reply_anchor("feishu:oc_g", Some("")).is_none());
        assert!(group_reply_anchor("feishu:oc_g", Some("xxx")).is_none());
    }

    /// Wave B-8：话题免 @ 窗口换算——0 = 关闭（ZERO），正值 = 秒。
    #[test]
    fn thread_window_of_maps_config() {
        assert_eq!(thread_window_of(0), Duration::ZERO);
        assert_eq!(thread_window_of(600), Duration::from_secs(600));
        assert_eq!(thread_window_of(1800), Duration::from_secs(30 * 60));
    }

    /// 合并转发 drain 产出：拉取成功 → 「（以下为用户转发的聊天记录）」前缀 +
    /// 转录正文（进 agent，占位正文被替换）；拉取失败 → 「⚠️ 无法读取合并转发
    /// 内容（原因），请直接复制文字发送」回退提示（不进 agent）。
    #[test]
    fn merge_forward_outcome_success_and_fallback() {
        let items = vec![MergedForwardItem {
            message_id: "om_sub1".into(),
            message_type: "text".into(),
            content: r#"{"text":"文本内容"}"#.into(),
            sender_id: "ou_a".into(),
            sender_name: Some("Alice".into()),
            create_time_ms: 0,
        }];
        let ok = Ok(items);
        match merge_forward_outcome(&ok, Some("群聊记录"), None) {
            MergeForwardOutcome::Agent(text) => {
                assert!(
                    text.starts_with("（以下为用户转发的聊天记录）\n\n"),
                    "正文前缀: {text}"
                );
                assert!(text.contains("【合并转发聊天记录】群聊记录"), "{text}");
                assert!(text.contains("[Alice] 文本内容"), "{text}");
            }
            other => panic!("成功路径应为 Agent: {other:?}"),
        }
        // 拉取失败（消息过期/权限/网络）：回退提示带原因，不产出 agent 正文。
        let err = Err(CoreError::Platform(
            PLATFORM,
            "list_merge_forward: code=230002 msg=message not exist".into(),
        ));
        match merge_forward_outcome(&err, None, None) {
            MergeForwardOutcome::Fallback(notice) => {
                assert!(notice.starts_with("⚠️ 无法读取合并转发内容（"), "{notice}");
                assert!(notice.contains("请直接复制文字发送"), "{notice}");
                assert!(notice.contains("code=230002"), "原因透传: {notice}");
            }
            other => panic!("失败路径应为 Fallback: {other:?}"),
        }
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
        )
        .expect("构造");
        assert!(p.quiet_hours.is_none(), "未配置 → None");
        assert!(!p.in_quiet_hours(), "未配置恒不在免打扰");
        // 锚点表 roundtrip。
        assert!(p.reply_anchor("feishu:oc_g").await.is_none(), "空表 → None");
        p.reply_anchors
            .lock()
            .await
            .insert("feishu:oc_g".to_string(), "om_1".to_string());
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
}
