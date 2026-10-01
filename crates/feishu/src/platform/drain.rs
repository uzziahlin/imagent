//! WS 事件 drain 循环（事件路由本体，T19 拆分自 platform.rs）。
//!
//! 职责：payload channel → 解析分派（消息/审批按钮/评论/表情/菜单/撤回/bot
//! 进出群等）→ Dedup →（消息类）媒体下载/转写/落盘与引用·群上下文注入 →
//! per-conv 顺序泵 → inbound channel。与发送侧共享的状态句柄（token/conv
//! 状态/pending asks/……）经 [`DrainContext`] 注入（原 new() 内联 spawn 闭包的
//! 捕获变量收敛为结构体，纯移动）。

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, Mutex, RwLock, Semaphore};
use tracing::{debug, warn};

use imagent_core::{ConvId, Dedup, InboundMessage, MediaRef, ReplyHint, Result};
use open_lark::CoreConfig;

use crate::client::{
    download_file, download_image, fetch_bot_open_id, list_merge_forward, reply_comment,
    reply_message, send_text_msg,
};
use crate::proto::{
    comment_target_from_conv, is_private_conv, parse_bot_removed_event, parse_card_action_event,
    parse_comment_event, parse_menu_event, parse_merged_forward_event, parse_message_event,
    parse_recall_event, receive_target_from_conv, render_merge_forward_transcript,
    thread_target_from_conv, unsupported_message_notice, MergedForwardItem,
};

use super::state::{persist_media, ConvState, PER_CONV_MAP_CAP};
use super::{fetch_cached_token, PendingAskCard};

/// v1.21 可观测性：drain 单事件时延 guard——构造于事件取出、drop 于处理完
///（含 continue 路径），Drop 时 observe。
struct DrainEventTimer {
    started: Instant,
}

impl Default for DrainEventTimer {
    fn default() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Drop for DrainEventTimer {
    fn drop(&mut self) {
        crate::metrics::METRICS
            .drain_event
            .observe(self.started.elapsed().as_secs_f64());
    }
}

/// P2-3（code-review v14）：媒体下载 / 合并转发拉取 / 群上下文拉取的全局
/// 并发闸（drain 三类作业此前无并发上限——事件风暴下 spawn 无界，同时打满
/// 连接与内存）。permit 在进入下载段前 acquire_owned、hold 到该段落盘完成
///（drop）；下载侧「整读进内存」的结构性改造（流式落盘）另行，本闸是并发
/// 面的止血。8 与 mock/真机常见的 50 并发连接上限留出余量。
/// Arc 包装：acquire_owned 需要 `self: Arc<Self>`（permit 'static 可跨 await
/// 移动进 spawn 的任务）。
static MEDIA_SEMAPHORE: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(8)));

/// drain task 的共享句柄集合（原 new() 内联 spawn 闭包的捕获变量收敛为
/// 结构体——字段语义与原局部变量一一对应，纯移动）。
pub(super) struct DrainContext {
    /// WS payload 无界 channel 接收端（FeishuWsClient run task 推入）。
    pub(super) payload_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    /// core 有界入站通道（drain 产出推入，`FeishuPlatform::recv` 消费）。
    pub(super) inbound_tx: mpsc::Sender<InboundMessage>,
    /// 事件级滑动窗口去重（drain 私有）。
    pub(super) dedup: Dedup,
    /// 发消息用配置（HTTP OpenAPI + 取 token）。
    pub(super) core_config: Arc<CoreConfig>,
    pub(super) app_id: String,
    pub(super) app_secret: String,
    /// token lazy 刷新缓存（发送/接收共用同一份）。
    pub(super) token: Arc<RwLock<Option<(String, Instant)>>>,
    /// v1.21 outbox：提示类发送失败落盘重试（None = 未接 store）。
    pub(super) outbox: Option<imagent_store::Store>,
    /// v1.23 说话人归属：open_id → 展示名缓存（contact 懒解析）。
    pub(super) user_names: Arc<Mutex<HashMap<String, String>>>,
    /// contact 解析失败负缓存（open_id → 失败时刻）。
    pub(super) user_name_failed: Arc<Mutex<HashMap<String, Instant>>>,
    /// P5-8：bot 自身 open_id 懒取缓存（@bot 过滤用）。
    pub(super) bot_open_id: Arc<RwLock<Option<String>>>,
    /// P6-1：群消息 @bot 过滤策略（`/config require_mention` 热切换共享句柄）。
    pub(super) mention_policy: Arc<RwLock<crate::proto::MentionPolicy>>,
    /// pending 询问卡登记（过期询问的按钮点击反馈查这里）。
    pub(super) pending_asks: Arc<Mutex<HashMap<String, PendingAskCard>>>,
    /// per-conv 会话状态单表（与发送侧共享一份）。
    pub(super) conv_states: Arc<Mutex<HashMap<String, ConvState>>>,
    /// Wave B-8：话题免 @ 窗口时长（0 = 关闭豁免）。
    pub(super) thread_active_window: Duration,
    /// T10：群聊上下文注入条数（0 = 关闭）。
    pub(super) group_context_messages: usize,
    /// W3-1：语音转文字开关（关闭时语音消息回退提示）。
    pub(super) asr_enabled: bool,
}

/// drain 主循环（事件路由本体）。行为与拆分前的内联闭包逐分支一致。
pub(super) async fn run(ctx: DrainContext) {
    let DrainContext {
        mut payload_rx,
        inbound_tx,
        dedup,
        core_config,
        app_id,
        app_secret,
        token,
        outbox,
        user_names,
        user_name_failed,
        bot_open_id,
        mention_policy,
        pending_asks,
        conv_states,
        thread_active_window,
        group_context_messages,
        asr_enabled,
    } = ctx;
    // v1.18 迭代：per-conv 顺序泵表（见 conv_pump/pump_send）——消息
    // 事件经泵发出，媒体/合并转发处理 spawn 后按 conv 入队等序。
    let mut conv_pumps: HashMap<String, mpsc::UnboundedSender<PumpJob>> = HashMap::new();
    while let Some(payload) = payload_rx.recv().await {
        // v1.21 可观测性：单事件处理时延 + channel 积压采样（停摆
        // 先行信号，见 metrics.rs 文档）。
        crate::metrics::METRICS
            .ws_backlog
            .set(payload_rx.len() as f64);
        let _drain_timer = DrainEventTimer::default();
        // T19/v13-P3：单次解析 + event_type 精确分派。入口对每个 payload 只做
        // 一次 serde_json::Value 全量解析（取 header.event_type 定分支），命中
        // 分支内再做一次强类型解析（parse_* 仍收 &[u8]，proto 纯函数与测试/
        // fuzz 面不变）。拆分前同一 payload 顺序尝试 11+ 个 parse/peek/is 谓词
        //（各自 from_slice 完整反序列化），单事件最坏 14 次解析；现恒为 2 次
        //（入口 Value + 命中分支强类型；merged_forward 经 message_type 二选一，
        // 兜底分类日志的重复解析也随之消失）。Value 导航 helper 与 proto 字节
        // 版谓词的等价性由 value_helpers_match_proto_predicates 测试钉住。
        let value: serde_json::Value = match serde_json::from_slice(&payload).ok() {
            Some(v) => v,
            // 整体非 JSON：与拆分前同结论（全部谓词/解析 None → 兜底 WARN）。
            None => {
                log_unhandled(None, &payload);
                continue;
            }
        };
        let etype = event_type_of(&value);
        let v: &serde_json::Value = &value;
        match branch_of_event_type(etype) {
            DrainBranch::Message => {
                // 三类事件：普通消息（含媒体下载）/ 审批按钮回调 / 云文档评论。
                // P6-1：群消息的 @bot 过滤与 @bot 文本剥离需要 bot open_id——
                // 首个群消息事件懒取（与评论事件共用缓存），失败退化为弱过滤。
                if is_group_message(v) {
                    ensure_bot_open_id(&bot_open_id, &token, &core_config, &app_id, &app_secret)
                        .await;
                }
                let bot = bot_open_id.read().await.clone();
                let mut policy = *mention_policy.read().await;
                // 话题群近期活跃免 @：该话题 THREAD_ACTIVE_WINDOW 内有过消息则
                // 本条豁免 require_mention（追问场景免于每条 @）。普通群
                // thread_key 不命中，不豁免。Wave B-8：窗口时长改 config 注入
                //（thread_active_window，0 = 关闭豁免）。
                // v1.18 群媒体「回复即定向」：回复 bot 近期消息的群消息（图片/
                // 文件等无法携带 @ 的形态）视为对 bot 的显式定向，豁免
                // require_mention——与 @ 等权（白名单/会话域门禁不变）。
                if policy.require_mention_in_group {
                    if let Some(parent) = peek_group_reply_parent(v) {
                        if crate::client::bot_sent_recently(&parent) {
                            policy.require_mention_in_group = false;
                        }
                    }
                }
                let thread_key = thread_key_of_value(v);
                if !thread_active_window.is_zero() {
                    if let Some(tk) = &thread_key {
                        if conv_states
                            .lock()
                            .await
                            .get(tk)
                            .and_then(|s| s.thread_active_at)
                            .is_some_and(|t| t.elapsed() < thread_active_window)
                        {
                            policy.require_mention_in_group = false;
                        }
                    }
                }

                // T19 解析预算：merged_forward 与普通消息在 message_type 上互斥
                //（parse_message_event 对 merged_forward 恒 None，反之亦然）——
                // 按入口已解析的 message_type 二选一，拆分前对同一事件的
                // 串行双解析消失。
                // 合并转发消息（merged_forward，完整支持——替换 v1.12.0 的「暂不
                // 支持」快赢）：按 meta.message_id 调「查询合并转发消息列表」API
                // 分页拉全子消息 → 转录为文本注入 agent（转录见
                // proto::render_merge_forward_transcript，拉取见 client::list_merge_forward）。
                // 走既有 dedup 管线；不产生 PendingMedia（子消息图片/文件一期只
                // 占位不下载，与媒体下载管线无冲突）；群内仍要求 @bot（parse 内
                // 沿用 group_mention_ok，无特判）。
                if message_type_of(v) == "merged_forward" {
                    if let Some((key, mf_msg, meta)) =
                        parse_merged_forward_event(&payload, &policy, bot.as_deref())
                    {
                        if !dedup.check(&key) {
                            continue;
                        }
                        // 与普通消息一致的登记（转录消息同样发起一轮 agent）：conv 发起
                        // 者（审批卡点击者校验锚）/ 锚点候选（W3-5：send_typing 轮次
                        // 锚定时提升为回复锚点）/ 话题活跃免 @ 续期。
                        {
                            let mut m = conv_states.lock().await;
                            let st = m.entry(mf_msg.conv_id.0.clone()).or_default();
                            st.sender = Some(mf_msg.sender.0.clone());
                            if let Some((conv, anchor)) = group_reply_anchor(
                                &mf_msg.conv_id.0,
                                mf_msg.source_msg_id.as_deref(),
                            ) {
                                st.last_inbound = Some(anchor);
                                let _ = conv;
                            }
                            if let Some(tk) = &thread_key {
                                let st = m.entry(tk.clone()).or_default();
                                st.thread_active_at = Some(Instant::now());
                                st.last_touched = Instant::now();
                            }
                        }
                        // v1.18 迭代（intake 解耦）：拉子消息（分页 API）与媒体下载
                        // 同理移出 drain 串行循环——转录产出经 per-conv 泵按序发出；
                        // Fallback 提示在任务内直发（终态通知，无排序语义）。
                        let conv_key = mf_msg.conv_id.0.clone();
                        let cfg = core_config.clone();
                        let token_lock = token.clone();
                        let aid = app_id.clone();
                        let sec = app_secret.clone();
                        let fallback_conv = ConvId(conv_key.clone());
                        let handle = tokio::spawn(async move {
                            // P2-3（code-review v14）：合并转发拉取与媒体下载同
                            // 走并发闸（permit 覆盖整个分页拉取段）。
                            let _permit = MEDIA_SEMAPHORE
                                .clone()
                                .acquire_owned()
                                .await
                                .expect("MEDIA_SEMAPHORE 永不关闭");
                            let fetched = fetch_merge_forward_items(
                                &cfg,
                                &token_lock,
                                &aid,
                                &sec,
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
                                    let mut m = mf_msg;
                                    m.text = Some(text);
                                    Some(m)
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
                                        &cfg,
                                        &token_lock,
                                        &aid,
                                        &sec,
                                        &fallback_conv,
                                        &notice,
                                    )
                                    .await;
                                    None
                                }
                            }
                        });
                        if inbound_tx.is_closed() {
                            break;
                        }
                        pump_send(
                            &mut conv_pumps,
                            &inbound_tx,
                            &conv_key,
                            PumpJob::Merged(handle),
                        );
                        continue;
                    }
                } else if let Some((msgid, mut msg, pending)) =
                    parse_message_event(&payload, &policy, bot.as_deref())
                {
                    if !dedup.check(&msgid) {
                        continue;
                    }
                    // 记录 conv 发起者（审批卡/终止按钮的发起者校验锚）、锚点候选
                    //（最近一条入站消息 id——send_typing 提升为回复锚点）与话题活跃
                    // 时刻（免 @ 窗口续期）。ConvState 收敛后单锁单 entry 完成；
                    // 有界性由 housekeeping 粗上限统一承担（原 512 整体重置删除）。
                    {
                        let mut m = conv_states.lock().await;
                        let st = m.entry(msg.conv_id.0.clone()).or_default();
                        st.sender = Some(msg.sender.0.clone());
                        // v1.21 LRU：入站消息 = 活跃信号。
                        st.last_touched = Instant::now();
                        if let Some((conv, anchor)) =
                            group_reply_anchor(&msg.conv_id.0, msg.source_msg_id.as_deref())
                        {
                            st.last_inbound = Some(anchor);
                            let _ = conv;
                        }
                        if let Some(tk) = &thread_key {
                            let st = m.entry(tk.clone()).or_default();
                            st.thread_active_at = Some(Instant::now());
                            st.last_touched = Instant::now();
                        }
                    }
                    // v1.18 迭代（intake 解耦）：媒体下载/转写/落盘移出 drain 串行
                    // 循环——单条媒体 IO（read_timeout 已兜底停滞，此处再解耦吞吐）
                    // 不再阻塞其它 conv 的消息/审批回调；per-conv 顺序泵保序。
                    // v1.23 说话人归属：命中缓存即带名；未命中 spawn 预热
                    //（contact API，失败 1h 负缓存）——本轮标注回退 id 短版，
                    // 下一轮起有名字。非阻塞（drain 循环不等网络）。
                    {
                        let oid = msg.sender.0.clone();
                        let known = user_names.lock().await.get(&oid).cloned();
                        let has_name = known.is_some();
                        msg.sender_name = known;
                        if !has_name {
                            let recently_failed = user_name_failed
                                .lock()
                                .await
                                .get(&oid)
                                .is_some_and(|t| t.elapsed() < Duration::from_secs(3600));
                            if !recently_failed {
                                let cfg = core_config.clone();
                                let tl = token.clone();
                                let aid = app_id.clone();
                                let sec = app_secret.clone();
                                let cache = user_names.clone();
                                let failed = user_name_failed.clone();
                                tokio::spawn(async move {
                                    let fetched = async {
                                        let t = fetch_cached_token(&tl, &cfg, &aid, &sec).await?;
                                        crate::client::fetch_user_display_name(&cfg, &t, &oid).await
                                    }
                                    .await;
                                    match fetched {
                                        Ok(name) => {
                                            let mut m = cache.lock().await;
                                            if m.len() > 4096 {
                                                m.clear();
                                            }
                                            m.insert(oid, name);
                                        }
                                        Err(e) => {
                                            debug!(target: "feishu", error = %e, "用户名解析失败（标注回退 open_id；1h 内不重试）");
                                            failed.lock().await.insert(oid, Instant::now());
                                        }
                                    }
                                });
                            }
                        }
                    }
                    let conv_key = msg.conv_id.0.clone();
                    // v1.25 引用上下文：回复（parent_id）消息拉被引用正文前置进
                    // prompt——群聊引用追问是 Slack 线程上下文的等价物。守卫：
                    // ①审批/询问/命令候选（is_explicit_reply_word 或 ask:/斜杠
                    // 前缀，见 [`quote_parent_for`]）不加引，防破坏审批路由；
                    // ②拉取失败 fail-soft 原样发送。与媒体处理合流到同一异步
                    // 作业（保序经泵）。
                    // T19：peek_reply_parent 的字节版重解析改为复用已解析消息的
                    // reply_to（= parent_id，非空过滤同源）+ om_ 前缀过滤——与原
                    // 谓词等价（peek_reply_parent 即 parent_id 非空 + om_ 前缀），
                    // 省一次反序列化。
                    // P2-1（code-review v14）：守卫改为「不是审批/询问/命令候选
                    // 才注入」——旧的 `trim().len() > 4` 是字节数，中文审批词
                    // （允许/没问题）与 always 全部漏拦：引文前置后
                    // parse_reply 全字匹配失败 → 批准变拒绝。
                    let quote_parent = quote_parent_for(&msg);
                    // T10 群聊上下文：群 conv（含话题群——免 @ 窗口语义下同样
                    // 适用，无需特判）且配置 > 0 时拉本群最近 N 条前置注入；
                    // 私聊/评论 conv 天然不命中。与引用上下文同款 fail-soft，
                    // 同一异步作业内**后于**引用注入执行（群上下文块在引用块
                    // 之前——更早的背景）。
                    // P2-4（code-review v14）：话题 conv 附带话题 root——拉取后
                    // 只保留本话题条目（话题=独立会话）。
                    let group_ctx_chat = crate::proto::group_chat_id_of_conv(&msg.conv_id.0)
                        .filter(|_| group_context_messages > 0);
                    let group_ctx_thread_root =
                        thread_target_from_conv(&ConvId(msg.conv_id.0.clone()))
                            .map(|(_, root)| root);
                    let needs_async =
                        !pending.is_empty() || quote_parent.is_some() || group_ctx_chat.is_some();
                    let job = if !needs_async {
                        PumpJob::Ready(msg)
                    } else {
                        let token_lock = token.clone();
                        let cfg = core_config.clone();
                        let aid = app_id.clone();
                        let sec = app_secret.clone();
                        let pending = pending.clone();
                        let group_ctx_limit = group_context_messages;
                        let group_ctx_thread_root = group_ctx_thread_root.clone();
                        PumpJob::Media(tokio::spawn(async move {
                            let mut msg = msg;
                            if let Some(parent_id) = quote_parent.as_deref() {
                                enrich_with_quote(
                                    &mut msg,
                                    parent_id,
                                    &token_lock,
                                    &cfg,
                                    &aid,
                                    &sec,
                                )
                                .await;
                            }
                            if let Some(chat_id) = group_ctx_chat.as_deref() {
                                enrich_with_group_context(
                                    &mut msg,
                                    chat_id,
                                    group_ctx_limit,
                                    group_ctx_thread_root.as_deref(),
                                    &token_lock,
                                    &cfg,
                                    &aid,
                                    &sec,
                                )
                                .await;
                            }
                            process_pending_media(
                                msg,
                                &pending,
                                &token_lock,
                                &cfg,
                                &aid,
                                &sec,
                                asr_enabled,
                            )
                            .await
                        }))
                    };
                    if inbound_tx.is_closed() {
                        break;
                    }
                    pump_send(&mut conv_pumps, &inbound_tx, &conv_key, job);
                    continue;
                }

                // 不支持类型提示：语音/分享卡片等此前静默丢弃，用户无感知——回一条
                // 可读提示。v1.18 review 收紧：① 过 dedup（message_id）——事件重投/
                // 断线重连不再重复回执；② 仅 p2p（proto 侧）——群内「带 @」弱门槛
                // 会让非白名单群的贴纸/分享触发 bot 回复（垃圾消息面 + 存活性探针）。
                if let Some((notice, dedup_key, Some(conv))) = unsupported_message_notice(&payload)
                {
                    if dedup_key.is_some_and(|k| dedup.check(&k)) {
                        spawn_drain_text(
                            &core_config,
                            &token,
                            &app_id,
                            &app_secret,
                            conv,
                            notice.to_string(),
                            outbox.as_ref(),
                        );
                    }
                    continue;
                }
                // 兜底：消息事件未过准入策略/字段不合法（原循环尾部分类
                // 日志的 message 分型，DEBUG）。
                log_unhandled(etype, &payload);
            }
            DrainBranch::CardAction => {
                // P4-4：审批按钮回调（card.action.trigger）→ text="y"/"n" 的
                // 入站消息，core 的审批回复路由消费（parse_reply("y")=allow）。
                // 安全批次扩展：回调解析带第三元素 deny（命令按钮过期 / 他人点
                // 终止或命令按钮——全形态校验，proto 侧已判，此处回提示后丢弃，
                // 不进 core 分派）；
                // 审批按钮另做**发起者校验**（群 conv 下点击者须为登记的发起者，
                // 私聊单人免检）。
                // v1.17.3 诊断：真机 2026-09-03 自由输入值未随 form_value 到达
                //（根因是回调侧题号收集漏 _free 键，已修）。形态已用真机载荷
                // 校准（free=字符串、未填回空串、未选 select 键缺席）；debug 级
                // 原始载荷日志留作卡片回调排障。v13-P3：截断到头 400 字符——
                // 完整载荷含 Bash 审批命令全文（可能有 secret/内网地址）与用户
                // 表单输入，全量进日志的泄露面大于排障价值（与下方兜底分类日志
                // 同款 payload_head 手法）。
                debug!(
                    target: "feishu",
                    payload_head = %payload_head(&payload, 400),
                    "card.action.trigger 原始载荷（头 400 字符）"
                );
                if let Some((key, reply_msg, deny)) = parse_card_action_event(&payload) {
                    if let Some(deny_text) = deny {
                        if !dedup.check(&key) {
                            continue;
                        }
                        spawn_drain_text(
                            &core_config,
                            &token,
                            &app_id,
                            &app_secret,
                            reply_msg.conv_id.clone(),
                            deny_text.clone(),
                            outbox.as_ref(),
                        );
                        // 安全批次（转发代批）：deny 文案回原 conv 之外，给点击者
                        // （operator）私聊补一条同文案——转发场景下原 conv 里没人
                        // 知道有人替点了按钮，第二触达让点击者明确知道被拒。
                        // 占位消息的 sender 即 operator open_id（见 dummy_card_action_msg）。
                        if !reply_msg.sender.0.is_empty()
                            && reply_msg.conv_id.0 != format!("feishu:{}", reply_msg.sender.0)
                        {
                            spawn_drain_text(
                                &core_config,
                                &token,
                                &app_id,
                                &app_secret,
                                ConvId(format!("feishu:{}", reply_msg.sender.0)),
                                deny_text.clone(),
                                outbox.as_ref(),
                            );
                        }
                        continue;
                    }
                    if !dedup.check(&key) {
                        continue;
                    }
                    // conv 发起者更新（按钮触发的轮次由点击者发起）。
                    if !reply_msg.sender.0.is_empty() {
                        conv_states
                            .lock()
                            .await
                            .entry(reply_msg.conv_id.0.clone())
                            .or_default()
                            .sender = Some(reply_msg.sender.0.clone());
                    }
                    // 过期反馈：req 已不在 pending_asks（询问已批准/拒绝/中断/
                    // 超时收敛，或复用槽换了新请求）→ 回一条「已过期」提示而非
                    // 静默丢进 core 的 miss 分支。无 req 的回调（命令按钮等）
                    // 不受影响。
                    if let Some(req) = reply_msg.ask_req.clone() {
                        let pending = pending_asks.lock().await.get(&req).cloned();
                        match pending {
                            None => {
                                spawn_drain_text(
                                    &core_config,
                                    &token,
                                    &app_id,
                                    &app_secret,
                                    reply_msg.conv_id.clone(),
                                    "⏳ 该询问已过期或已被处理，无需再次点击。".to_string(),
                                    outbox.as_ref(),
                                );
                                continue;
                            }
                            // 发起者校验（群 conv）：询问由发起者登记，他人点击
                            // 回明确提示（防群里任何人替批高危操作）。
                            Some(card) => {
                                if !card.sender.is_empty()
                                    && !is_private_conv(&card.conv_id)
                                    && card.sender != reply_msg.sender.0
                                {
                                    spawn_drain_text(
                                        &core_config,
                                        &token,
                                        &app_id,
                                        &app_secret,
                                        reply_msg.conv_id.clone(),
                                        format!(
                                            "⛔ 该询问由 {} 发起，仅其本人可答复。",
                                            card.sender
                                        ),
                                        outbox.as_ref(),
                                    );
                                    continue;
                                }
                            }
                        }
                    }
                    if inbound_tx.send(reply_msg).await.is_err() {
                        break;
                    }
                    continue;
                }
                log_unhandled(etype, &payload);
            }
            DrainBranch::Comment => {
                // P4-9：云文档评论 @bot（drive.file.comment.created_v1）→ 评论
                // 线程消息（conv = feishu:comment:<file>:<comment>，回复走
                // reply_comment；需在飞书后台订阅该事件）。
                // P5-8：仅接受 @bot 的评论——bot open_id 首次遇到评论事件时懒取
                // （GET /bot/v3/info）并缓存；取不到时退化为「至少含一个 @」的
                // 弱过滤。另过滤 bot 自身的回复（防自触发循环）。
                ensure_bot_open_id(&bot_open_id, &token, &core_config, &app_id, &app_secret).await;
                let bot = bot_open_id.read().await.clone();
                if let Some((key, comment_id, cm)) = parse_comment_event(&payload, bot.as_deref()) {
                    // 会话锚放宽：登记回复目标锚点（conv → comment_id）——发送
                    // 侧（send_text/send_media 评论分支）据此路由回复。T13：
                    // 双锚点登记（评论者本人 + 最近评论回退），发送侧按轮次
                    // 发起者解析（B 抢先评论不再截走 A 的回答）。
                    if dedup.check(&key) {
                        let mut m = conv_states.lock().await;
                        let st = m.entry(cm.conv_id.0.clone()).or_default();
                        st.note_comment(&cm.sender.0, &comment_id);
                        st.last_touched = Instant::now();
                        drop(m);
                        if inbound_tx.send(cm).await.is_err() {
                            break;
                        }
                    }
                } else {
                    tracing::debug!(target: "feishu", "评论未 @bot（或字段缺失/纯@），丢弃");
                }
                continue;
            }
            DrainBranch::Reaction => {
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
                    let hit = pending_asks
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
                            spawn_drain_text(
                                &core_config,
                                &token,
                                &app_id,
                                &app_secret,
                                ConvId(card.conv_id.clone()),
                                format!("⛔ 该询问由 {} 发起，仅其本人可答复。", card.sender),
                                outbox.as_ref(),
                            );
                            continue;
                        }
                        let reaction_msg = InboundMessage {
                            conv_id: ConvId(card.conv_id.clone()),
                            sender: imagent_core::UserId(operator),
                            sender_name: None,
                            text: Some(reply.to_string()),
                            media: Vec::new(),
                            media_errors: Vec::new(),
                            mentions: Vec::new(),
                            mentioned_bot: false,
                            ask_req: None,
                            reply_to: Some(reacted_msg),
                            source_msg_id: None,
                            control: None,
                            no_steer: false,
                            reply_hint: ReplyHint::None,
                        };
                        if inbound_tx.send(reaction_msg).await.is_err() {
                            break;
                        }
                    }
                    continue;
                }
                log_unhandled(etype, &payload);
            }
            DrainBranch::Menu => {
                // 自定义菜单跳转（application.url.menu_v6）→ 合成 text="/help" 的
                // 入站消息（复用 card action 的合成模式）：走与手打 /help 完全相同
                // 的鉴权/分派路径。需在飞书后台订阅该事件（可选，见 README）。
                if let Some((key, menu_msg)) = parse_menu_event(&payload) {
                    if dedup.check(&key) && inbound_tx.send(menu_msg).await.is_err() {
                        break;
                    }
                    continue;
                }
                log_unhandled(etype, &payload);
            }
            DrainBranch::Recall => {
                // 消息撤回（im.message.recalled_v1，一期）→ 控制消息：core 据此
                // 把同 id 的排队消息移出（在飞任务不自动停，只回提示）。需订阅
                // 该事件（可选，见 README）。
                if let Some((key, recall_msg)) = parse_recall_event(&payload) {
                    if dedup.check(&key) && inbound_tx.send(recall_msg).await.is_err() {
                        break;
                    }
                    continue;
                }
                log_unhandled(etype, &payload);
            }
            DrainBranch::BotRemoved => {
                // bot 被移出群（im.chat.member.bot.deleted_v1）→ 控制消息：core
                // 据此收回会话白名单并通知管理员。需订阅该事件（可选，见 README）。
                if let Some((key, removed_msg)) = parse_bot_removed_event(&payload) {
                    if dedup.check(&key) && inbound_tx.send(removed_msg).await.is_err() {
                        break;
                    }
                    continue;
                }
                log_unhandled(etype, &payload);
            }
            DrainBranch::BotAdded => {
                // W3-4：bot 被加入群（im.chat.member.bot.added_v1）→ 欢迎引导
                //（含 /chat allow 放行指引——放行前 core 白名单不会放行群消息）。
                // 需订阅该事件（可选）。欢迎语平台层直发（与移出群通知管理员同
                // 模式），无敏感信息。
                if let Some((key, chat_id)) = crate::proto::parse_bot_added_event(&payload) {
                    if dedup.check(&key) {
                        // v1.24 卡片 UX：进群欢迎从一段纯文本升级为命令卡——
                        // 第一印象带可点按钮（帮助/放行），不再是一堵文字墙。
                        // 失败（卡片权限缺失等）回落旧文本形态（outbox 兜底）。
                        spawn_welcome_card(
                            &core_config,
                            &token,
                            &app_id,
                            &app_secret,
                            &chat_id,
                            outbox.as_ref(),
                        );
                    }
                    continue;
                }
                log_unhandled(etype, &payload);
            }
            DrainBranch::Unknown => {
                log_unhandled(etype, &payload);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 入口单次解析的 Value 导航 helper + 分派枚举（T19/v13-P3）。
// 导航 helper 与 proto 字节版谓词（is_group_message_event / peek_group_reply_parent /
// thread_key_of_payload）语义一致——等价性由 value_helpers_match_proto_predicates
// 测试钉住；不复用字节版是为了避免每次导航都 from_slice 完整反序列化一遍
//（见 run() 头部的解析预算注释）。
// ---------------------------------------------------------------------------

/// payload → `header.event_type`（缺失/非字符串 → None）。
fn event_type_of(v: &serde_json::Value) -> Option<&str> {
    v.get("header")?.get("event_type")?.as_str()
}

/// 与 `proto::is_group_message_event` 同语义：im.message.receive_v1 且 group。
fn is_group_message(v: &serde_json::Value) -> bool {
    event_type_of(v) == Some("im.message.receive_v1")
        && v.pointer("/event/message/chat_type")
            .and_then(|c| c.as_str())
            == Some("group")
}

/// 与 `proto::peek_group_reply_parent` 同语义：群消息的 parent_id（非空）；
/// 非群/无 parent → None。
fn peek_group_reply_parent(v: &serde_json::Value) -> Option<String> {
    if v.pointer("/event/message/chat_type")
        .and_then(|c| c.as_str())
        != Some("group")
    {
        return None;
    }
    v.pointer("/event/message/parent_id")
        .and_then(|p| p.as_str())
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

/// 与 `proto::thread_key_of_payload` 同语义：话题群（group + om_ 前缀 root_id）
/// 的话题 conv 键 `feishu:<chat_id>:<root_id>`。
fn thread_key_of_value(v: &serde_json::Value) -> Option<String> {
    if event_type_of(v) != Some("im.message.receive_v1") {
        return None;
    }
    let msg = v.get("event")?.get("message")?;
    if msg.get("chat_type").and_then(|c| c.as_str()) != Some("group") {
        return None;
    }
    let chat_id = msg
        .get("chat_id")
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())?;
    let root = msg
        .get("root_id")
        .and_then(|r| r.as_str())
        .filter(|r| r.starts_with("om_"))?;
    Some(format!("feishu:{chat_id}:{root}"))
}

/// `event.message.message_type`（缺失 → 空串）。仅作 merged_forward 与普通消息
/// 的分支选择（两者在该字段上互斥）；语义判定仍由 parse_* 的强类型解析承担。
fn message_type_of(v: &serde_json::Value) -> &str {
    v.pointer("/event/message/message_type")
        .and_then(|m| m.as_str())
        .unwrap_or("")
}

/// drain 分派分支（单次解析后按 `header.event_type` 精确 match）。各 parse_*
/// 内部同样校验 event_type 且只认自己的类型——此处映射与其一一对应，由
/// branch_dispatch_covers_all_event_types 测试钉住。
#[derive(Debug, PartialEq, Eq)]
enum DrainBranch {
    /// im.message.receive_v1：普通消息 / 媒体 / 合并转发 / 不支持类型提示。
    Message,
    /// card.action.trigger：审批/命令/表单按钮回调。
    CardAction,
    /// drive.file.comment.created_v1：云文档评论 @bot。
    Comment,
    /// im.message.reaction.created_v1：表情回应快速审批。
    Reaction,
    /// application.url.menu_v6：自定义菜单跳转 → /help。
    Menu,
    /// im.message.recalled_v1：消息撤回 → 控制消息。
    Recall,
    /// im.chat.member.bot.deleted_v1：bot 被移出群。
    BotRemoved,
    /// im.chat.member.bot.added_v1：bot 被加入群 → 欢迎卡。
    BotAdded,
    /// 其余/缺 event_type：兜底分类日志（原循环尾部兜底）。
    Unknown,
}

fn branch_of_event_type(etype: Option<&str>) -> DrainBranch {
    match etype {
        Some("im.message.receive_v1") => DrainBranch::Message,
        Some("card.action.trigger") => DrainBranch::CardAction,
        Some("drive.file.comment.created_v1") => DrainBranch::Comment,
        Some("im.message.reaction.created_v1") => DrainBranch::Reaction,
        Some("application.url.menu_v6") => DrainBranch::Menu,
        Some("im.message.recalled_v1") => DrainBranch::Recall,
        Some("im.chat.member.bot.deleted_v1") => DrainBranch::BotRemoved,
        Some("im.chat.member.bot.added_v1") => DrainBranch::BotAdded,
        _ => DrainBranch::Unknown,
    }
}

/// 真机排障：兜底分类——已知「正常忽略」的事件（策略过滤的群消息、表情回执/
/// 自身回声）降 DEBUG，避免淹没真正需要排障的 WARN（真机校准 2026-09-01：
/// V2/V3 期间大量正常消息被记成 WARN 误导视线）。各分支解析未命中时统一走
/// 这里收尾（原循环尾部的兜底分类——etype 取自入口已解析的 Value，不再重析）。
fn log_unhandled(etype: Option<&str>, payload: &[u8]) {
    let head: String = payload_head(payload, 400);
    match etype {
        Some("im.message.receive_v1") => {
            debug!(target: "feishu", payload_head = %head, "消息未过准入策略（如群内未@），忽略");
        }
        Some("im.message.reaction.created_v1") => {
            debug!(target: "feishu", payload_head = %head, "表情事件回执（多为自身回声），忽略");
        }
        _ => warn!(target: "feishu", payload_head = %head, "无法解析/非目标事件，丢弃"),
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

/// P2-1（code-review v14）：审批回复词表的**本地镜像**——core 的判定函数
/// `permission::is_explicit_reply_word`（ALLOW/DENY/ALWAYS 词表全字匹配）在
/// HEAD 未公开导出（`mod permission` 私有，`pub use` 清单不含它，已核实），
/// feishu 侧只能镜像；词表演进（如 P2-12 补中文确认词）须同步此处，core
/// 导出后换用 `imagent_core::permission::is_explicit_reply_word` 消重。
const REPLY_ALLOW_WORDS: &[&str] = &[
    "y",
    "yes",
    "ye",
    "yep",
    "yeah",
    "ok",
    "okay",
    "是",
    "允许",
    "好",
    "好的",
    "可以",
    "行",
    "没问题",
    "好呀",
    "行吧",
    "可以吧",
    "嗯",
];
const REPLY_DENY_WORDS: &[&str] = &[
    "n",
    "no",
    "nope",
    "nah",
    "不",
    "否",
    "不要",
    "不行",
    "不可以",
    "不许",
    "拒绝",
    "不批",
];
const REPLY_ALWAYS_WORDS: &[&str] = &["always", "始终允许", "会话内允许"];

/// [`REPLY_*_WORDS`] 的全字匹配（trim + 小写），语义镜像 core 的
/// `is_explicit_reply_word`。
fn is_explicit_reply_word(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    REPLY_ALLOW_WORDS.contains(&lower.as_str())
        || REPLY_DENY_WORDS.contains(&lower.as_str())
        || REPLY_ALWAYS_WORDS.contains(&lower.as_str())
}

/// P2-1（code-review v14）：引用注入的准入守卫（纯函数，便于单测）。
/// 回复（reply_to = om_ 平台消息）且正文**不是**审批/询问/命令候选时才注入
/// 引文：
/// - 审批词表（上方本地镜像，全字匹配）命中 → 跳过——引文会把「允许」前置成
///   「（用户引用了…）:\n> …\n\n允许」，parse_reply 全字匹配失败，批准变拒绝；
/// - `ask:` 前缀（AskUserQuestion 选项回执）与 `/` 前缀（斜杠命令）同理跳过。
///
/// 旧守卫 `trim().len() > 4` 是**字节**数——中文审批词与 always 全部漏拦。
fn quote_parent_for(msg: &InboundMessage) -> Option<String> {
    let parent = msg.reply_to.as_deref().filter(|p| p.starts_with("om_"))?;
    let text = msg
        .text
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())?;
    if is_explicit_reply_word(text) || text.starts_with("ask:") || text.starts_with('/') {
        return None;
    }
    Some(parent.to_string())
}

/// 原始事件载荷的日志安全形态：头 `max_chars` 字符（char 边界安全，多字节
/// UTF-8 不劈开）。v13-P3 收口——`card.action.trigger` 载荷含 Bash 审批命令
/// 全文（可能有 secret/内网地址）与用户表单输入，全量进日志的泄露面大于排障
/// 价值；与兜底分类日志（原 head-400 内联写法）统一到本函数。
fn payload_head(payload: &[u8], max_chars: usize) -> String {
    String::from_utf8_lossy(payload)
        .chars()
        .take(max_chars)
        .collect()
}

/// 消息待处理媒体的下载/token 自愈/语音转写/落盘（原 drain 内联块，v1.18
/// 迭代抽离以便 spawn——drain 串行循环不再被单条媒体 IO 阻塞）。单个失败只
/// 记 media_errors，不丢整条消息（语义与原内联实现一致，含 token 失效码
/// 清缓存重试一次）。
#[allow(clippy::too_many_arguments)]
/// v1.25 引用上下文：拉被引用消息正文并前置进 prompt（fail-soft——
/// 权限缺失/网络失败原样通过，仅 debug 留痕）。
async fn enrich_with_quote(
    msg: &mut InboundMessage,
    parent_id: &str,
    token_lock: &Arc<RwLock<Option<(String, Instant)>>>,
    cfg: &CoreConfig,
    aid: &str,
    sec: &str,
) {
    let fetched = async {
        let t = fetch_cached_token(token_lock, cfg, aid, sec).await?;
        crate::client::fetch_message_raw(cfg, &t, parent_id).await
    }
    .await;
    // 真机校准（2026-09-29）：拉取 API 的 msg_type 是单数 merge_forward，
    // 事件侧才是复数 merged_forward（飞书两面命名不一致）——两个都认。
    let is_merged_forward = fetched
        .as_ref()
        .is_ok_and(|(mt, _)| matches!(mt.as_str(), "merged_forward" | "merge_forward"));
    // 引用卡片与聊天记录同权重放宽到 1500 字：卡片正文即完整 agent 回复
    //（发送侧上限 8KB），500 字截断恰好砍在用户追问的报错/结论上。
    let mut wide_quote = is_merged_forward;
    let quote = match fetched {
        Ok((mt, content)) => match mt.as_str() {
            // 引用的是合并转发消息（聊天记录卡片）：本体 content 是占位符，
            // 须再调子消息接口拉全量并转录（v1.25.2 补——此前该类型直接
            // 放弃，引用会话记录场景整链失效）。转录放宽到 1500 字（会话
            // 记录天然长于单条消息）。类型串双形态见上方 is_merged_forward
            // 校准注释（API 单数 / 事件复数）。
            "merged_forward" | "merge_forward" => {
                let token = match fetch_cached_token(token_lock, cfg, aid, sec).await {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(target: "feishu", error = %e, "引用合并转发取 token 失败（本轮引用上下文缺省，用户提问照常）");
                        return;
                    }
                };
                // P3b（code-review v14）：引用路径 expand_nested=false——嵌套
                // 展开只有直发 8000 字预算才做；引用块预算 1500 字，展开的
                // 嵌套条目随即被截掉，白付每次 1 条的拉取配额。
                match crate::client::list_merge_forward(cfg, &token, parent_id, false).await {
                    Ok(items) if items.is_empty() => {
                        // P3c（code-review v14）：空 items 不注入「共 0 条」
                        // 占位转录（可能已撤回或形态异常）——fail-soft 静默跳过
                        //（引用是辅助上下文，回提示反而是噪音）。
                        debug!(target: "feishu", parent_id, "引用合并转发子消息为空（可能已撤回），跳过引用注入");
                        return;
                    }
                    Ok(items) => Some(
                        crate::proto::render_merge_forward_transcript(&items, None, None)
                            .trim()
                            .to_string(),
                    ),
                    Err(e) => {
                        warn!(target: "feishu", error = %e, parent_id, "引用合并转发子消息拉取失败（本轮引用上下文缺省，用户提问照常）——v1.26.x 教训：此类静默失败此前 debug 级不可见，排查极难");
                        return;
                    }
                }
            }
            // v1.27.0 修复：引用卡片（bot 自身回复即 interactive 卡）此前只剩
            // [interactive] 占位、追问整链失效——抽卡片文本正文（proto::
            // card_text_transcript），模板卡/抽不到回退类型占位。
            "interactive" => {
                wide_quote = true;
                crate::proto::card_text_transcript(&content).or(Some("[卡片消息]".to_string()))
            }
            // 图片/文件等：给类型占位（agent 至少知道引用的是什么）。
            "image" | "media" | "file" | "sticker" | "emotion" => Some(format!("[{mt}]")),
            _ => crate::proto::quoted_context_text(&mt, &content),
        },
        Err(e) => {
            warn!(target: "feishu", error = %e, parent_id, "引用消息拉取失败（原样发送，引用上下文缺省）");
            return;
        }
    };
    let Some(quote) = quote.filter(|q| !q.trim().is_empty()) else {
        return;
    };
    let cap = if wide_quote { 1_500 } else { 500 };
    let total = quote.chars().count();
    // P3b（code-review v14）：截断处补标记——静默截断让 agent 把残句当全文。
    let quote: String = quote.chars().take(cap).collect();
    let quote = if total > cap {
        format!("{quote}\n（引用内容过长已截断）")
    } else {
        quote
    };
    let base = msg.text.take().unwrap_or_default();
    msg.text = Some(format!(
        "（用户引用了以下消息，针对它追问）:\n> {}\n\n{base}",
        quote.replace('\n', "\n> ")
    ));
}

/// T10 群聊上下文注入：拉本群最近 N 条消息转录为前置块进 prompt（Slack 线程
/// 上下文的飞书等价物——「帮我们看看刚才讨论的」类追问不再失忆）。
///
/// - fail-soft：token/权限/网络任一失败 `debug!` 一条后原样通过，轮次照常
///   （token 失效码自愈一次，与媒体下载同款）；
/// - 全部条目被过滤（如只有 bot 自己的消息）→ 不注入（[`render_group_context_block`]
///   返回 None）；
/// - 与引用上下文共存时**后于**其执行（调用序），群上下文块落在引用块之前——
///   更早的背景；纯媒体轮次（text 空）只注入块本身。
/// - P2-4（code-review v14）：话题 conv（`thread_root` 非 None）只保留 root_id
///   匹配本话题的条目——「话题=独立会话互不共享上下文」；响应缺 root_id 字段
///   （API 形态不含话题归属）则整轮跳过注入（fail-safe：宁缺毋跨话题）。
/// - P2-4（code-review v14）：per-chat 15s 拉取去抖（时间窗内重复拉取直接跳过
///   ——只去抖**拉取**，注入语义不变；失败也占窗，防拉取错误风暴）。
#[allow(clippy::too_many_arguments)]
async fn enrich_with_group_context(
    msg: &mut InboundMessage,
    chat_id: &str,
    limit: usize,
    thread_root: Option<&str>,
    token_lock: &Arc<RwLock<Option<(String, Instant)>>>,
    cfg: &CoreConfig,
    aid: &str,
    sec: &str,
) {
    // P2-4：per-chat 拉取去抖（15s）。std::sync::Mutex：临界区无 await。
    // 粗上限：超限整体清空（与 user_names 等表的惯例一致——去抖窗短暂失效
    // 无害，下轮重新记账）。
    static GROUP_CTX_FETCH_AT: LazyLock<std::sync::Mutex<HashMap<String, Instant>>> =
        LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
    const GROUP_CTX_DEBOUNCE: Duration = Duration::from_secs(15);
    const GROUP_CTX_CHAT_CAP: usize = 512;
    {
        let mut last = GROUP_CTX_FETCH_AT.lock().unwrap_or_else(|e| e.into_inner());
        if last.len() >= GROUP_CTX_CHAT_CAP {
            last.clear();
        }
        if last
            .get(chat_id)
            .is_some_and(|t| t.elapsed() < GROUP_CTX_DEBOUNCE)
        {
            debug!(target: "feishu", chat_id, "群上下文 15s 去抖窗口内，跳过本轮拉取");
            return;
        }
        last.insert(chat_id.to_string(), Instant::now());
    }
    // P2-3：下载段并发闸（token 失效自愈的重试也在 permit 内——同一次拉取
    // 不应并发两份）。
    let _permit = MEDIA_SEMAPHORE
        .clone()
        .acquire_owned()
        .await
        .expect("MEDIA_SEMAPHORE 永不关闭");
    let fetched = async {
        let t = fetch_cached_token(token_lock, cfg, aid, sec).await?;
        crate::client::list_chat_messages(cfg, &t, chat_id, limit, aid).await
    }
    .await;
    let mut items = match fetched {
        Ok(items) => items,
        // token 失效自愈：清缓存强制刷新后重试一次（与媒体下载同语义）。
        Err(e) if crate::client::is_token_invalid_err(&e) => {
            *token_lock.write().await = None;
            match async {
                let t = fetch_cached_token(token_lock, cfg, aid, sec).await?;
                crate::client::list_chat_messages(cfg, &t, chat_id, limit, aid).await
            }
            .await
            {
                Ok(items) => items,
                Err(e2) => {
                    warn!(
                        target: "feishu",
                        error = %e2,
                        chat_id,
                        "群上下文拉取失败（token 刷新后仍失败，跳过注入）"
                    );
                    return;
                }
            }
        }
        Err(e) => {
            warn!(
                target: "feishu",
                error = %e,
                chat_id,
                "群上下文拉取失败（权限/网络，跳过注入）——群消息历史需 im:message:readonly + im:message.group_msg"
            );
            return;
        }
    };
    // P2-4：话题过滤——只留 root_id 匹配本话题的条目；响应若整体缺 root_id
    // 字段（无任何非空值），无法区分话题归属，跳过注入（宁缺毋跨话题）。
    if let Some(root) = thread_root {
        let field_usable = items.iter().any(|it| !it.root_id.is_empty());
        if !field_usable {
            debug!(target: "feishu", chat_id, "话题群历史响应缺 root_id 字段，跳过注入（防跨话题共享上下文）");
            return;
        }
        items.retain(|it| it.root_id == root);
    }
    // 退化转录检测（真机校准教训）：text 条目正文为空 → 群上下文只剩类型
    // 占位标签，warn 附形状便于定位 schema 漂移（正常路径静默）。
    if items
        .iter()
        .any(|it| it.message_type == "text" && it.content.trim().is_empty())
    {
        let shape: Vec<String> = items
            .iter()
            .map(|it| format!("{}({}B)", it.message_type, it.content.len()))
            .collect();
        warn!(
            target: "feishu",
            items = shape.join(","),
            "群上下文含空正文的 text 条目（转录将退化占位）——疑似字段结构与已知 schema 不符"
        );
    }
    let Some(block) = crate::proto::render_group_context_block(&items) else {
        return;
    };
    let base = msg.text.take().unwrap_or_default();
    msg.text = Some(if base.trim().is_empty() {
        block
    } else {
        format!("{block}\n\n{base}")
    });
}

async fn process_pending_media(
    mut msg: InboundMessage,
    pending: &[crate::proto::PendingMedia],
    token_lock: &Arc<RwLock<Option<(String, Instant)>>>,
    core_config: &Arc<CoreConfig>,
    app_id: &str,
    app_secret: &str,
    asr_enabled: bool,
) -> InboundMessage {
    for p in pending {
        let token = match fetch_cached_token(token_lock, core_config, app_id, app_secret).await {
            Ok(t) => t,
            Err(e) => {
                warn!(target: "feishu", error = %e, "取 token 失败，跳过该媒体");
                msg.media_errors
                    .push(format!("{}: 取 token 失败: {e}", p.key));
                continue;
            }
        };
        // P2-3（code-review v14）：下载段并发闸——permit 覆盖下载+落盘/转写
        //（迭代末 drop），事件风暴下并发下载有界；非下载段（记账等）不占 permit。
        let _dl_permit = MEDIA_SEMAPHORE
            .clone()
            .acquire_owned()
            .await
            .expect("MEDIA_SEMAPHORE 永不关闭");
        let dl = match p.kind {
            "image" => download_image(core_config, &token, &p.message_id, &p.key).await,
            // W3-1：语音资源同 file 走 message-resource 接口。
            "file" | "audio" => download_file(core_config, &token, &p.message_id, &p.key).await,
            _ => continue,
        };
        // token 失效自愈（与发送侧 with_token 同语义）：清缓存强制
        // 刷新后再试一次；二次仍失败如实进 media_errors。
        let dl = match dl {
            Ok(b) => Ok(b),
            Err(e) if crate::client::is_token_invalid_err(&e) => {
                warn!(target: "feishu", error = %e, "媒体下载遇 token 失效码，清缓存刷新后重试一次");
                *token_lock.write().await = None;
                let token =
                    match fetch_cached_token(token_lock, core_config, app_id, app_secret).await {
                        Ok(t) => t,
                        Err(e2) => {
                            msg.media_errors
                                .push(format!("{}: 重取 token 失败: {e2}", p.key));
                            continue;
                        }
                    };
                match p.kind {
                    "image" => download_image(core_config, &token, &p.message_id, &p.key).await,
                    "file" | "audio" => {
                        download_file(core_config, &token, &p.message_id, &p.key).await
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
                    if asr_enabled {
                        match crate::client::transcribe_audio(core_config, &token, bytes).await {
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
                                msg.media_errors
                                    .push(format!("语音转写失败: {e}（可改发文字）"));
                            }
                        }
                    } else {
                        msg.media_errors.push(
                            "语音转写已关闭（feishu_asr_enabled=false），请改发文字".to_string(),
                        );
                    }
                    continue;
                }
                match persist_media(p.kind, &p.key, p.file_name.as_deref(), &bytes) {
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
    msg
}

// clippy：Ready 变体（InboundMessage ~200B）显著大于 JoinHandle 变体——
// 瞬态队列作业、同 conv 同时至多几条，装箱得不偿失，允许。
#[allow(clippy::large_enum_variant)]
enum PumpJob {
    Ready(InboundMessage),
    Media(tokio::task::JoinHandle<InboundMessage>),
    Merged(tokio::task::JoinHandle<Option<InboundMessage>>),
}

/// 单 conv 的顺序泵任务：按入队顺序等待作业完成并把消息发往 core 的有界
/// 入站通道（背压经 outbound.send().await 传导）。
/// v1.18 review（agent-1 #1）：**不做空闲退出**——「发送方 is_closed 检查通过
/// → 泵恰好超时退出 → 缓冲作业随 rx drop 丢失」是丢消息竞态（dedup 已消费
/// id，丢了即永久丢）。泵仅在 outbound 关闭（core 停机）或全 sender drop 时
/// 退出；map 无界增长由 pump_send 的上限驱逐兜住（驱逐 = drop sender，泵把
/// 缓冲作业处理完再干净退出，不丢）。闲置泵只是 parked task（百字节级）。
async fn conv_pump(
    mut rx: mpsc::UnboundedReceiver<PumpJob>,
    outbound: mpsc::Sender<InboundMessage>,
) {
    loop {
        let job = match rx.recv().await {
            Some(j) => j,
            None => break, // 全部 sender drop
        };
        // v1.21 可观测性：取件即减（与 pump_send 的 inc 配对）——gauge 反映
        // 「已入队未取件」的总量，媒体队头阻塞时先行增长。
        crate::metrics::METRICS.pump_pending.dec();
        let msg = match job {
            PumpJob::Ready(m) => Some(m),
            PumpJob::Media(h) => match h.await {
                Ok(m) => Some(m),
                Err(e) => {
                    warn!(target: "feishu", error = %e, "媒体处理任务 join 失败（panic？），丢弃该条消息");
                    None
                }
            },
            PumpJob::Merged(h) => match h.await {
                Ok(opt) => opt,
                Err(e) => {
                    warn!(target: "feishu", error = %e, "合并转发处理任务 join 失败（panic？），丢弃该条消息");
                    None
                }
            },
        };
        if let Some(m) = msg {
            if outbound.send(m).await.is_err() {
                break;
            }
        }
    }
}

/// 按 conv 取（或惰性建）泵通道并入队。泵空闲退出后 entry 残留 → send 失败
/// → 重建一次重发（task 不泄漏、消息不丢）。
fn pump_send(
    pumps: &mut HashMap<String, mpsc::UnboundedSender<PumpJob>>,
    outbound: &mpsc::Sender<InboundMessage>,
    conv: &str,
    job: PumpJob,
) {
    let spawn_pump = || {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(conv_pump(rx, outbound.clone()));
        tx
    };
    // v1.18 review（agent-1 #1）：上限驱逐——drop sender 后泵先把缓冲作业
    // 处理完再退出（recv None），不丢消息；被驱逐 conv 的下一条消息走重建。
    if pumps.len() >= PER_CONV_MAP_CAP {
        if let Some(oldest) = pumps.keys().next().cloned() {
            if let Some(tx) = pumps.remove(&oldest) {
                debug!(target: "feishu", conv = %oldest, "泵表超上限，驱逐最旧条目（缓冲作业由泵排空后自然退出）");
                drop(tx);
            }
        }
    }
    use std::collections::hash_map::Entry;
    let sent = match pumps.entry(conv.to_string()) {
        Entry::Occupied(mut e) => {
            // SendError 含被退回的 job：重建泵后原样重发（此前 `let _ =` 把
            // job 一并丢弃——可检测的丢失没有兜底）。
            match e.get().send(job) {
                Ok(()) => true,
                Err(undelivered) => {
                    let tx = spawn_pump();
                    let ok = tx.send(undelivered.0).is_ok();
                    e.insert(tx);
                    ok
                }
            }
        }
        Entry::Vacant(e) => {
            let tx = spawn_pump();
            let ok = tx.send(job).is_ok();
            e.insert(tx);
            ok
        }
    };
    // v1.21 可观测性：入队成功即增（与 conv_pump 取件时的 dec 配对）。
    if sent {
        crate::metrics::METRICS.pump_pending.inc();
    }
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
    // v1.21 review（负缓存）：HTTP 分区期间 drain 串行循环里每条群消息事件都
    // 内联重试一次 30s 超时的取值——全平台入站被队头阻塞。失败后 60s 内不再
    // 重试（期间维持弱过滤降级，恢复由下一窗口的首次事件触发）。
    static BOT_ID_FAIL_AT: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
    const BOT_ID_FAIL_NEG_TTL: Duration = Duration::from_secs(60);
    {
        let fail_at = BOT_ID_FAIL_AT.lock().unwrap_or_else(|e| e.into_inner());
        if fail_at.is_some_and(|t| t.elapsed() < BOT_ID_FAIL_NEG_TTL) {
            return;
        }
    }
    let fetched = async {
        let t = fetch_cached_token(token_lock, core_config, app_id, app_secret).await?;
        fetch_bot_open_id(core_config, &t).await
    }
    .await;
    match fetched {
        Ok(b) => {
            *BOT_ID_FAIL_AT.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *bot_open_id.write().await = Some(b)
        }
        Err(e) => {
            *BOT_ID_FAIL_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            warn!(
                target: "feishu",
                error = %e,
                "取 bot open_id 失败，@bot 过滤退化为弱过滤（须含 @）；60s 内不再重试"
            )
        }
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
    match list_merge_forward(core_config, &t, message_id, true).await {
        Err(e) if crate::client::is_token_invalid_err(&e) => {
            warn!(target: "feishu", error = %e, "合并转发拉取遇 token 失效码，清缓存刷新后重试一次");
            *token_lock.write().await = None;
            let fresh = fetch_cached_token(token_lock, core_config, app_id, app_secret).await?;
            list_merge_forward(core_config, &fresh, message_id, true).await
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
        // P3c（code-review v14）：空 items 不产出「共 0 条」占位转录进 agent
        //（可能已撤回或形态异常）——与拉取失败同走 Fallback 用户可读提示
        //（语义一致：直发合并转发是用户显式动作，缺内容须可感知）。
        Ok(items) if items.is_empty() => MergeForwardOutcome::Fallback(
            "⚠️ 转发记录拉取为空（可能已撤回或形态异常），请直接复制文字发送".into(),
        ),
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
    if let Err(e) =
        send_drain_text_result(core_config, token_lock, app_id, app_secret, conv, text).await
    {
        warn!(target: "feishu", error = %e, "drain 提示发送失败（无害）");
    }
}

/// [`send_drain_text`] 的 Result 形态（outbox 泵需要成败信号驱动退避）。
pub(super) async fn send_drain_text_result(
    core_config: &CoreConfig,
    token_lock: &Arc<RwLock<Option<(String, Instant)>>>,
    app_id: &str,
    app_secret: &str,
    conv: &ConvId,
    text: &str,
) -> Result<()> {
    send_drain_text_result_with_uuid(
        core_config,
        token_lock,
        app_id,
        app_secret,
        conv,
        text,
        None,
    )
    .await
}

/// P2-5（code-review v14）：[`send_drain_text_result`] 的外带幂等键版——
/// outbox 泵重发时透传落盘时的 uuid（reply/create 路径同键，飞书幂等窗口内
/// 去重，重试不再重复触达）；None = 现生成（原语义）。
pub(super) async fn send_drain_text_result_with_uuid(
    core_config: &CoreConfig,
    token_lock: &Arc<RwLock<Option<(String, Instant)>>>,
    app_id: &str,
    app_secret: &str,
    conv: &ConvId,
    text: &str,
    idempotency_uuid: Option<&str>,
) -> Result<()> {
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
            idempotency_uuid,
        )
        .await
        .map(|_| ())
    } else if let Some((receive_id, kind)) = receive_target_from_conv(conv) {
        match idempotency_uuid {
            Some(u) => {
                crate::client::send_text_msg_with_uuid(
                    core_config,
                    &t,
                    &receive_id,
                    kind,
                    text,
                    false,
                    u,
                )
                .await
            }
            None => send_text_msg(core_config, &t, &receive_id, kind, text, false).await,
        }
    } else {
        Ok(())
    }
}

/// v1.21 review（drain 解耦）：drain 串行事件循环里的 best-effort 提示改为
/// spawn 后台发送——HTTP 分区时这些内联 await（token 懒取 + 发送，最坏 30s+）
/// 会把整个入站管道（含审批回调、撤回事件）队头阻塞，payload 无界 channel
/// 随之膨胀。提示类消息无顺序要求，解耦零代价。
/// v1.24 卡片 UX：进群欢迎命令卡（按钮可点：帮助/放行），失败回落旧文本。
fn spawn_welcome_card(
    core_config: &Arc<CoreConfig>,
    token_lock: &Arc<RwLock<Option<(String, Instant)>>>,
    app_id: &str,
    app_secret: &str,
    chat_id: &str,
    outbox: Option<&imagent_store::Store>,
) {
    use imagent_core::{CardButton, CardButtonStyle};
    let body = "群内 **@我** 发消息即可驱动 agent（Claude Code 等）。\n\n**先放行**：管理员发送 `/chat allow` 放行本群（放行前我不会响应消息）。\n\n**会话规则**：群主时间线直接 @我 = 续同一会话；点消息「回复」进话题 = 开独立会话（互不共享上下文/待办）。\n\n**排队与转向**：我运行中发文字会实时转入当前任务（👀）；图片/文件排队下一轮（⏳）。";
    let conv_id = format!("feishu:{chat_id}");
    let card_json = crate::card::render_command_card(
        "👋 你好，我是 agent 网关",
        body,
        &[
            CardButton {
                label: "📖 命令帮助".into(),
                command: "/help".into(),
                style: CardButtonStyle::Primary,
            },
            CardButton {
                label: "✅ 放行本群（管理员）".into(),
                command: "/chat allow".into(),
                style: CardButtonStyle::Default,
            },
        ],
        &conv_id,
    );
    let core_config = core_config.clone();
    let token_lock = token_lock.clone();
    let app_id = app_id.to_string();
    let app_secret = app_secret.to_string();
    let chat_id = chat_id.to_string();
    let outbox = outbox.cloned();
    tokio::spawn(async move {
        let sent = match fetch_cached_token(&token_lock, &core_config, &app_id, &app_secret).await {
            Ok(t) => {
                crate::client::send_card_msg(
                    &core_config,
                    &t,
                    &chat_id,
                    crate::proto::ReceiveIdKind::ChatId,
                    &card_json,
                )
                .await
            }
            Err(e) => Err(e),
        };
        if sent.is_err() {
            warn!(target: "feishu", "欢迎卡发送失败，回落文本形态");
            let conv = ConvId(format!("feishu:{chat_id}"));
            let text = "👋 我已加入本群！群内 @我 发消息即可驱动 agent。\n管理员可发送 /chat allow 放行本群（放行前我不会响应消息）；/help 查看全部命令。".to_string();
            if let Err(e) = send_drain_text_result(
                &core_config,
                &token_lock,
                &app_id,
                &app_secret,
                &conv,
                &text,
            )
            .await
            {
                warn!(target: "feishu", error = %e, conv_id = %conv.0, "欢迎文本回落也失败（转 outbox）");
                if let Some(store) = outbox {
                    // P2-5（code-review v14）：落盘 payload 带幂等 uuid（泵重发透传）。
                    let payload = super::outbox::outbox_payload(&conv.0, &text);
                    let _ = store.enqueue_outbox(&conv.0, "feishu_text", &payload).await;
                }
            }
        }
    });
}

fn spawn_drain_text(
    core_config: &Arc<CoreConfig>,
    token_lock: &Arc<RwLock<Option<(String, Instant)>>>,
    app_id: &str,
    app_secret: &str,
    conv: ConvId,
    text: String,
    outbox: Option<&imagent_store::Store>,
) {
    let core_config = core_config.clone();
    let token_lock = token_lock.clone();
    let app_id = app_id.to_string();
    let app_secret = app_secret.to_string();
    let outbox = outbox.cloned();
    tokio::spawn(async move {
        // v1.21 outbox：失败不再无声丢失——落 outbox 表由后台泵退避重发
        //（未接 store 的部署保持旧 warn 语义）。
        if let Err(e) = send_drain_text_result(
            &core_config,
            &token_lock,
            &app_id,
            &app_secret,
            &conv,
            &text,
        )
        .await
        {
            warn!(target: "feishu", error = %e, conv_id = %conv.0, "drain 提示发送失败（转入 outbox 重试）");
            if let Some(store) = outbox {
                // P2-5（code-review v14）：落盘 payload 带幂等 uuid（泵重发透传）。
                let payload = super::outbox::outbox_payload(&conv.0, &text);
                if let Err(e2) = store.enqueue_outbox(&conv.0, "feishu_text", &payload).await {
                    warn!(target: "feishu", error = %e2, "outbox 落盘失败（提示丢失）");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::testutil::{cached_token, mock_core_config, spawn_mock_feishu};
    use crate::platform::PLATFORM;
    use imagent_core::CoreError;

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

    /// 原名透传落盘：file 用原始文件名（含扩展名）；图片无原名默认 png；带名图片
    /// 用其扩展名；路径穿越（../、分隔符）被净化；无名 file 回退 key.bin。
    /// W4-3：同名媒体不覆盖——第二次落盘加序号后缀。名字带进程 id + 纳秒防
    /// 历史残留（media 目录是真实 ~/.imagent/media，同机器多次跑测试会累积）。
    /// per-conv 顺序泵：慢媒体任务先入队、就绪消息后入队 → 输出仍按入队序
    ///（intake 解耦的保序不变量）。
    #[tokio::test]
    async fn conv_pump_preserves_order_across_await_jobs() {
        let (out_tx, mut out_rx) = mpsc::channel::<InboundMessage>(8);
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(conv_pump(rx, out_tx));
        let mk = |text: &str| InboundMessage {
            conv_id: ConvId("feishu:ou_pump".into()),
            sender: imagent_core::UserId("ou_u".into()),
            sender_name: None,
            text: Some(text.into()),
            media: vec![],
            media_errors: Vec::new(),
            mentions: Vec::new(),
            mentioned_bot: false,
            ask_req: None,
            reply_to: None,
            source_msg_id: None,
            control: None,
            no_steer: false,
            reply_hint: ReplyHint::None,
        };
        let slow = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            mk("slow-media")
        });
        tx.send(PumpJob::Media(slow)).unwrap();
        tx.send(PumpJob::Ready(mk("fast-text"))).unwrap();
        let first = out_rx.recv().await.unwrap();
        assert_eq!(
            first.text.as_deref(),
            Some("slow-media"),
            "先入队的媒体消息先出"
        );
        let second = out_rx.recv().await.unwrap();
        assert_eq!(
            second.text.as_deref(),
            Some("fast-text"),
            "后入队的就绪消息后出"
        );
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

    /// v13-P3：载荷日志截断——头 400 字符（card.action.trigger 载荷含 Bash 审批
    /// 命令全文/用户表单输入，不全文进日志）；多字节 UTF-8 不劈开；短/恰好
    /// 等长载荷原样。
    #[test]
    fn payload_head_truncates_for_logging() {
        let short = br#"{"header":{"event_type":"card.action.trigger"}}"#;
        assert_eq!(
            payload_head(short, 400),
            String::from_utf8_lossy(short).to_string()
        );
        // 多字节：500 个「好」（1500 字节）→ 截到 400 字符，char 边界安全。
        let long = "好".repeat(500);
        let head = payload_head(long.as_bytes(), 400);
        assert_eq!(head.chars().count(), 400);
        assert!(head.chars().all(|c| c == '好'), "不得劈开多字节字符");
        // 恰好等长 → 不截断。
        let exact = "a".repeat(400);
        assert_eq!(payload_head(exact.as_bytes(), 400), exact);
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

        // P3c（code-review v14）：空 items（可能已撤回/形态异常）不产出
        // 「共 0 条」占位转录进 agent——同样走 Fallback 用户可读提示。
        match merge_forward_outcome(&Ok(Vec::new()), None, None) {
            MergeForwardOutcome::Fallback(notice) => {
                assert!(notice.contains("转发记录拉取为空"), "{notice}");
                assert!(notice.contains("可能已撤回"), "{notice}");
                assert!(notice.contains("请直接复制文字发送"), "{notice}");
            }
            other => panic!("空 items 应为 Fallback: {other:?}"),
        }
    }

    /// 群文本入站消息（@bot 已剥离后的形态）。
    fn mk_group_text_msg(text: &str) -> InboundMessage {
        InboundMessage {
            conv_id: ConvId("feishu:oc_g".into()),
            sender: imagent_core::UserId("ou_sender".into()),
            sender_name: None,
            text: Some(text.into()),
            media: vec![],
            media_errors: Vec::new(),
            mentions: Vec::new(),
            mentioned_bot: true,
            ask_req: None,
            reply_to: None,
            source_msg_id: Some("om_now".into()),
            control: None,
            no_steer: false,
            reply_hint: ReplyHint::None,
        }
    }

    /// 「获取会话历史消息」mock 响应（ByCreateTimeDesc：最新在前；1 条 bot 消息
    /// 混在中间——sender_type=app 且 app_id 命中 cli_mock，双判定形态都覆盖）。
    fn group_context_list_body() -> String {
        let content = |t: &str| serde_json::to_string(&serde_json::json!({ "text": t })).unwrap();
        serde_json::json!({
            "code": 0,
            "data": { "items": [
                {
                    "message_id": "om_3", "msg_type": "text", "create_time": "1788000003",
                    "sender": { "id": "ou_b0c072f42e7c1b09", "id_type": "open_id", "sender_type": "user" },
                    "content": content("最新这条")
                },
                {
                    "message_id": "om_2", "msg_type": "image", "create_time": "1788000002",
                    "sender": { "id": "ou_b0c072f42e7c1b09", "id_type": "open_id",
                                "sender_type": "app", "app_id": "cli_mock" },
                    "content": "{}"
                },
                {
                    "message_id": "om_1", "msg_type": "text", "create_time": "1788000001",
                    "sender": { "id": "ou_alice", "id_type": "open_id", "sender_type": "user", "name": "Alice" },
                    "content": content("最早这条")
                }
            ]}
        })
        .to_string()
    }

    /// 群消息进轮次 → 本群最近 N 条（跳过 bot 消息、时间正序）前置注入 prompt，
    /// 头尾格式与正文保留都在位。
    /// P2-4（code-review v14）：chat id 用测试唯一值——群上下文拉取有 per-chat
    /// 15s 去抖（全局表），并行测试共用 chat id 会互相吃掉拉取窗口。
    #[tokio::test]
    async fn enrich_with_group_context_injects_recent_block() {
        let base = spawn_mock_feishu(std::sync::Arc::new(|path: &str| {
            assert!(
                path.starts_with("/open-apis/im/v1/messages"),
                "路径: {path}"
            );
            (200u16, group_context_list_body())
        }))
        .await;
        let cfg = mock_core_config(&base);
        let token = cached_token();
        let mut msg = mk_group_text_msg("帮我们看看刚才讨论的");
        enrich_with_group_context(
            &mut msg,
            "oc_g_inject",
            10,
            None,
            &token,
            &cfg,
            "cli_mock",
            "sec_mock",
        )
        .await;
        let text = msg.text.as_deref().expect("应注入文本");
        assert!(
            text.starts_with("【群最近上下文（2 条，最新在最后）】"),
            "{text}"
        );
        // bot 消息（om_2）被跳过：条数 2 且无 [图片] 占位。
        assert!(!text.contains("[图片]"), "bot 消息应跳过: {text}");
        // 正序：Alice（最早）在前，bob 短 id（最新）在后；名字优先、缺名回退后 8 位。
        let alice = text.find("Alice: 最早这条").expect("最早条应在前半");
        let bob = text.find("2e7c1b09: 最新这条").expect("最新条应在后半");
        assert!(alice < bob, "按时间正序（最新在最后）: {text}");
        // 尾注 + 用户正文保留。
        let tail = text.find("（以下是用户本轮消息）").expect("尾注在位");
        assert!(tail < text.find("帮我们看看刚才讨论的").unwrap(), "{text}");
        // 总长 ≤ 上限（chars）。
        assert!(text.chars().count() <= crate::proto::GROUP_CONTEXT_MAX_CHARS);
    }

    /// API 失败（权限不足 code!=0）→ fail-soft：debug 留痕，prompt 原样通过。
    #[tokio::test]
    async fn enrich_with_group_context_failsoft_on_api_error() {
        let base = spawn_mock_feishu(std::sync::Arc::new(|_path: &str| {
            (
                200u16,
                r#"{"code":230002,"msg":"no permission"}"#.to_string(),
            )
        }))
        .await;
        let cfg = mock_core_config(&base);
        let token = cached_token();
        let mut msg = mk_group_text_msg("原文不动");
        enrich_with_group_context(
            &mut msg,
            "oc_g_failsoft",
            10,
            None,
            &token,
            &cfg,
            "cli_mock",
            "sec_mock",
        )
        .await;
        assert_eq!(
            msg.text.as_deref(),
            Some("原文不动"),
            "fail-soft 应原样通过"
        );
    }

    /// 与引用上下文共存：群上下文块在引用块**之前**（更早的背景），正文最后
    ///（对齐 drain 内「先 quote 后 group」的调用序）。chat id 唯一（去抖）。
    #[tokio::test]
    async fn enrich_with_quote_then_group_context_order() {
        let quote_body = serde_json::json!({
            "code": 0,
            "data": { "items": [ {
                "msg_type": "text",
                "body": { "content":
                    serde_json::to_string(&serde_json::json!({"text": "被引用的报错内容"})).unwrap() }
            }]}
        })
        .to_string();
        let base = spawn_mock_feishu(std::sync::Arc::new(move |path: &str| {
            // 单条消息 GET（引用）与列表 GET（群上下文）按 path 分发。
            if path == "/open-apis/im/v1/messages" {
                (200u16, group_context_list_body())
            } else {
                (200u16, quote_body.clone())
            }
        }))
        .await;
        let cfg = mock_core_config(&base);
        let token = cached_token();
        let mut msg = mk_group_text_msg("这个报错怎么修");
        enrich_with_quote(&mut msg, "om_parent", &token, &cfg, "cli_mock", "sec_mock").await;
        enrich_with_group_context(
            &mut msg,
            "oc_g_order",
            10,
            None,
            &token,
            &cfg,
            "cli_mock",
            "sec_mock",
        )
        .await;
        let text = msg.text.as_deref().expect("两块都应注入");
        let group = text.find("【群最近上下文").expect("群上下文块在位");
        let quote = text.find("（用户引用了以下消息").expect("引用块在位");
        let body = text.find("这个报错怎么修").expect("正文在位");
        assert!(group < quote && quote < body, "群上下文→引用→正文: {text}");
    }

    /// T19 分派映射：各已知 event_type → 对应分支；未知/缺省 → Unknown。
    #[test]
    fn branch_dispatch_covers_all_event_types() {
        use super::DrainBranch::*;
        for (et, want) in [
            ("im.message.receive_v1", Message),
            ("card.action.trigger", CardAction),
            ("drive.file.comment.created_v1", Comment),
            ("im.message.reaction.created_v1", Reaction),
            ("application.url.menu_v6", Menu),
            ("im.message.recalled_v1", Recall),
            ("im.chat.member.bot.deleted_v1", BotRemoved),
            ("im.chat.member.bot.added_v1", BotAdded),
            ("im.other.event_v1", Unknown),
            ("", Unknown),
        ] {
            assert_eq!(branch_of_event_type(Some(et)), want, "etype={et}");
        }
        assert_eq!(
            branch_of_event_type(None),
            Unknown,
            "缺 event_type → Unknown"
        );
    }

    /// T19 等价性钉子：入口 Value 导航 helper 与 proto 字节版谓词在同一组
    /// payload 上结论一致（分派改走 Value 后，字节版谓词不再被 drain 调用，
    /// 等价契约由本测试承担）。
    #[test]
    fn value_helpers_match_proto_predicates() {
        let mk = |chat_type: &str, parent: Option<&str>, root: Option<&str>, etype: &str| {
            serde_json::json!({
                "header": {"event_id": "e1", "event_type": etype},
                "event": {"message": {
                    "chat_type": chat_type,
                    "chat_id": "oc_g",
                    "message_type": "text",
                    "parent_id": parent,
                    "root_id": root,
                }}
            })
        };
        let cases = vec![
            mk("group", Some("om_p"), None, "im.message.receive_v1"),
            mk("group", None, Some("om_root"), "im.message.receive_v1"),
            mk("p2p", Some("om_p"), None, "im.message.receive_v1"),
            mk("group", None, None, "im.message.receive_v1"),
            mk("group", None, None, "card.action.trigger"),
        ];
        for v in cases {
            let payload = serde_json::to_string(&v).unwrap().into_bytes();
            assert_eq!(
                is_group_message(&v),
                crate::proto::is_group_message_event(&payload),
                "is_group: {payload:?}"
            );
            assert_eq!(
                peek_group_reply_parent(&v),
                crate::proto::peek_group_reply_parent(&payload),
                "peek_group_reply_parent: {payload:?}"
            );
            assert_eq!(
                thread_key_of_value(&v),
                crate::proto::thread_key_of_payload(&payload),
                "thread_key: {payload:?}"
            );
            assert_eq!(event_type_of(&v).is_some(), !payload.is_empty());
        }
        // message_type 导航：命中 / 缺失（空串兜底）。
        let mf = mk("p2p", None, None, "im.message.receive_v1");
        assert_eq!(message_type_of(&mf), "text");
        let no_type: serde_json::Value = serde_json::json!({
            "header": {"event_type": "im.message.receive_v1"}, "event": {"message": {}}
        });
        assert_eq!(message_type_of(&no_type), "");
    }

    // ------------------------------------------------------------------
    // code-review v14：P2-1（引用守卫）/ P2-3（并发闸）/ P2-4（话题过滤+去抖）
    // / P3b（引用截断标记+免嵌套展开）/ P3c（空 items 不注入）
    // ------------------------------------------------------------------

    /// 带指定 reply_to 的群消息（P2-1 守卫测试用）。
    fn mk_reply_msg(text: &str, reply_to: Option<&str>) -> InboundMessage {
        let mut m = mk_group_text_msg(text);
        m.reply_to = reply_to.map(String::from);
        m
    }

    /// P2-1：引用注入守卫——审批/询问/命令候选不注入引文（原样发送）。
    /// 旧守卫 `trim().len() > 4` 是字节数：中文审批词（允许/没问题）与
    /// always（5 字节，trim 后 ≤4 的 y/n 更不必说）全部漏拦，引文前置后
    /// parse_reply 全字匹配失败 → 批准变拒绝。
    #[test]
    fn quote_guard_blocks_reply_candidates() {
        // 精确审批词（core 词表的本地镜像）：中英文全字命中 → 不注入。
        for word in [
            "y",
            "n",
            "yes",
            "no",
            "ok",
            "always",
            "允许",
            "拒绝",
            "可以",
            "没问题",
            "始终允许",
            "会话内允许",
        ] {
            let msg = mk_reply_msg(word, Some("om_quoted"));
            assert!(
                quote_parent_for(&msg).is_none(),
                "审批词「{word}」不得被前置引文污染"
            );
        }
        // ask: 回执与斜杠命令：同样不注入。
        for t in ["ask:方案A", "/help", "/config require_mention on"] {
            let msg = mk_reply_msg(t, Some("om_quoted"));
            assert!(
                quote_parent_for(&msg).is_none(),
                "「{t}」不得被前置引文污染"
            );
        }
        // 自由文本追问：照常注入（引用上下文的正常场景）。
        let free = mk_reply_msg("这个报错怎么解决", Some("om_quoted"));
        assert_eq!(quote_parent_for(&free).as_deref(), Some("om_quoted"));
        // 「允许」出现在长句中不是审批词（全字匹配语义）——照常注入。
        let free2 = mk_reply_msg("允许我补充一点背景再回答", Some("om_quoted"));
        assert!(quote_parent_for(&free2).is_some(), "自由长句照常注入");
        // 非回复 / 空 text / 非 om_ parent：不注入（原有守卫语义）。
        assert!(quote_parent_for(&mk_reply_msg("随便聊聊", None)).is_none());
        assert!(quote_parent_for(&mk_reply_msg("", Some("om_q"))).is_none());
        assert!(quote_parent_for(&mk_reply_msg("文本", Some("oc_not_msg"))).is_none());
    }

    /// P2-3：媒体/合并转发/群上下文的并发闸——permit 数恒 8，acquire/release
    /// 往返后恢复；并发任务观测到的同时在临界区的数量 ≤ 8。
    #[tokio::test]
    async fn media_semaphore_bounds_concurrency() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // 上限断言（并行测试可能瞬时持有 permit，只验「不多于 8」）。
        assert!(MEDIA_SEMAPHORE.available_permits() <= 8, "permit 总数=8");
        // 并发观测：20 个任务各占 permit 50ms，任意时刻临界区 ≤ 8
        //（其他测试若同时持 permit 只会让观测值更小，断言方向安全）。
        let inflight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..20 {
            let inflight = inflight.clone();
            let max_seen = max_seen.clone();
            handles.push(tokio::spawn(async move {
                let _p = MEDIA_SEMAPHORE
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("MEDIA_SEMAPHORE 永不关闭");
                let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                inflight.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert!(
            max_seen.load(Ordering::SeqCst) <= 8,
            "并发临界区 ≤ 8: {}",
            max_seen.load(Ordering::SeqCst)
        );
        assert_eq!(inflight.load(Ordering::SeqCst), 0, "全部释放");
    }

    /// P2-4：话题群只保留本话题条目——root_id 匹配 conv 第三段的才注入，
    /// 其它话题/主时间线条目不进 prompt（话题=独立会话互不共享上下文）。
    #[tokio::test]
    async fn group_context_thread_filter_keeps_only_own_thread() {
        let body = serde_json::json!({
            "code": 0,
            "data": { "items": [
                {
                    "message_id": "om_t1", "msg_type": "text", "create_time": "1788000001",
                    "root_id": "om_root_a",
                    "sender": { "id": "ou_a", "id_type": "open_id", "sender_type": "user" },
                    "body": { "content": "{\"text\":\"本话题的消息\"}" }
                },
                {
                    "message_id": "om_t2", "msg_type": "text", "create_time": "1788000002",
                    "root_id": "om_root_b",
                    "sender": { "id": "ou_b", "id_type": "open_id", "sender_type": "user" },
                    "body": { "content": "{\"text\":\"别的话题的消息\"}" }
                },
                {
                    "message_id": "om_main", "msg_type": "text", "create_time": "1788000003",
                    "sender": { "id": "ou_c", "id_type": "open_id", "sender_type": "user" },
                    "body": { "content": "{\"text\":\"主时间线的消息\"}" }
                }
            ]}
        })
        .to_string();
        let base = spawn_mock_feishu(std::sync::Arc::new(move |_path: &str| {
            (200u16, body.clone())
        }))
        .await;
        let cfg = mock_core_config(&base);
        let token = cached_token();
        let mut msg = mk_group_text_msg("继续这个话题");
        enrich_with_group_context(
            &mut msg,
            "oc_thread",
            10,
            Some("om_root_a"),
            &token,
            &cfg,
            "cli_mock",
            "sec_mock",
        )
        .await;
        let text = msg.text.as_deref().expect("应注入");
        assert!(text.contains("本话题的消息"), "{text}");
        assert!(!text.contains("别的话题的消息"), "跨话题不得泄漏: {text}");
        assert!(!text.contains("主时间线的消息"), "主时间线不得泄漏: {text}");
        assert!(text.ends_with("继续这个话题"), "正文保留: {text}");
        // 本话题条目被过滤后为空（窗口内无本话题消息）→ 不注入（render None）。
        let mut msg2 = mk_group_text_msg("新话题首问");
        enrich_with_group_context(
            &mut msg2,
            "oc_thread2",
            10,
            Some("om_root_none"),
            &token,
            &cfg,
            "cli_mock",
            "sec_mock",
        )
        .await;
        assert_eq!(
            msg2.text.as_deref(),
            Some("新话题首问"),
            "无本话题条目 → 不注入"
        );
    }

    /// P2-4：话题 conv 且响应缺 root_id 字段（API 形态不含话题归属）→
    /// 跳过注入（fail-safe 宁缺毋跨话题）；普通群 conv（thread_root=None）
    /// 不受影响照常注入。
    #[tokio::test]
    async fn group_context_thread_skip_when_root_id_missing() {
        let base = spawn_mock_feishu(std::sync::Arc::new(|_path: &str| {
            (200u16, group_context_list_body())
        }))
        .await;
        let cfg = mock_core_config(&base);
        let token = cached_token();
        // 话题 conv：group_context_list_body 的条目无 root_id → 跳过。
        let mut thread_msg = mk_group_text_msg("话题内追问");
        enrich_with_group_context(
            &mut thread_msg,
            "oc_thread_noroot",
            10,
            Some("om_root_x"),
            &token,
            &cfg,
            "cli_mock",
            "sec_mock",
        )
        .await;
        assert_eq!(
            thread_msg.text.as_deref(),
            Some("话题内追问"),
            "缺 root_id 字段：跳过注入（不跨话题共享）"
        );
        // 普通群（thread_root=None）：照常注入（root_id 缺失不影响）。
        let mut plain_msg = mk_group_text_msg("普通群追问");
        enrich_with_group_context(
            &mut plain_msg,
            "oc_plain_noroot",
            10,
            None,
            &token,
            &cfg,
            "cli_mock",
            "sec_mock",
        )
        .await;
        assert!(
            plain_msg
                .text
                .as_deref()
                .unwrap()
                .contains("【群最近上下文"),
            "普通群不受话题过滤影响"
        );
    }

    /// P2-4：per-chat 15s 拉取去抖——窗口内同 chat 的第二次拉取直接跳过
    ///（HTTP 不再打），注入语义不变（本轮不注入）。
    #[tokio::test]
    async fn group_context_debounce_skips_refetch_within_window() {
        let hits = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let hits_c = hits.clone();
        let base = spawn_mock_feishu(std::sync::Arc::new(move |_path: &str| {
            *hits_c.lock().unwrap() += 1;
            (200u16, group_context_list_body())
        }))
        .await;
        let cfg = mock_core_config(&base);
        let token = cached_token();
        let mut first = mk_group_text_msg("第一条");
        enrich_with_group_context(
            &mut first,
            "oc_debounce",
            10,
            None,
            &token,
            &cfg,
            "cli_mock",
            "sec_mock",
        )
        .await;
        assert!(
            first.text.as_deref().unwrap().contains("【群最近上下文"),
            "窗口外首次拉取照常注入"
        );
        let mut second = mk_group_text_msg("第二条");
        enrich_with_group_context(
            &mut second,
            "oc_debounce",
            10,
            None,
            &token,
            &cfg,
            "cli_mock",
            "sec_mock",
        )
        .await;
        assert_eq!(*hits.lock().unwrap(), 1, "窗口内不重复拉取");
        assert_eq!(
            second.text.as_deref(),
            Some("第二条"),
            "去抖轮次原样通过（不注入）"
        );
    }

    /// P3b：引用合并转发——超 1500 字截断处补「（引用内容过长已截断）」标记；
    /// 引用路径 expand_nested=false（嵌套条目不再逐条拉取，GET 次数有界）。
    #[tokio::test]
    async fn quote_merge_forward_truncation_marker_and_no_nested_expansion() {
        let long_text: String = "报错日志".repeat(800); // 转录 > 1500 字符
        let parent_body = serde_json::json!({
            "code": 0,
            "data": { "items": [ {
                "msg_type": "merge_forward",
                "body": { "content": "Merged and Forwarded Message" }
            }]}
        })
        .to_string();
        let sub_items_body = serde_json::json!({
            "code": 0,
            "data": { "items": [
                { "message_id": "om_parent", "msg_type": "merge_forward" },
                {
                    "message_id": "om_sub_long", "upper_message_id": "om_parent",
                    "msg_type": "text", "create_time": "1788000001",
                    "sender": { "id": "ou_a", "id_type": "open_id", "sender_type": "user" },
                    "body": { "content": format!("{{\"text\":\"{long_text}\"}}") }
                },
                {
                    "message_id": "om_sub_nested", "upper_message_id": "om_parent",
                    "msg_type": "merge_forward", "create_time": "1788000002",
                    "sender": { "id": "ou_b", "id_type": "open_id", "sender_type": "user" },
                    "body": { "content": "Merged and Forwarded Message" }
                }
            ]}
        })
        .to_string();
        let gets = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let gets_c = gets.clone();
        let base = spawn_mock_feishu(std::sync::Arc::new(move |_path: &str| {
            let n = {
                let mut g = gets_c.lock().unwrap();
                *g += 1;
                *g
            };
            // ① fetch_message_raw（父消息形态）；② list_merge_forward（子消息）。
            if n == 1 {
                (200u16, parent_body.clone())
            } else {
                (200u16, sub_items_body.clone())
            }
        }))
        .await;
        let cfg = mock_core_config(&base);
        let token = cached_token();
        let mut msg = mk_group_text_msg("这个转发里的报错怎么修");
        enrich_with_quote(&mut msg, "om_parent", &token, &cfg, "cli_mock", "sec_mock").await;
        let text = msg.text.as_deref().expect("应注入引用");
        assert!(
            text.contains("（引用内容过长已截断）"),
            "截断处补标记: {text}"
        );
        assert!(text.contains("这个转发里的报错怎么修"), "正文保留: {text}");
        // expand_nested=false：嵌套条目（om_sub_nested）不再拉取——GET 恰 2 次
        //（父消息 + 子消息列表），无第三次。
        tokio::time::sleep(Duration::from_millis(50)).await;
        let total_gets = *gets.lock().unwrap();
        assert_eq!(total_gets, 2, "引用路径不展开嵌套: {total_gets}");
    }

    /// P3c：引用合并转发拉回空 items（可能已撤回或形态异常）——不注入
    /// 「共 0 条」占位转录，prompt 原样通过。
    #[tokio::test]
    async fn quote_merge_forward_empty_items_skips_injection() {
        let parent_body = serde_json::json!({
            "code": 0,
            "data": { "items": [ {
                "msg_type": "merge_forward",
                "body": { "content": "Merged and Forwarded Message" }
            }]}
        })
        .to_string();
        let empty_body = serde_json::json!({
            "code": 0,
            "data": { "items": [ { "message_id": "om_parent", "msg_type": "merge_forward" } ] }
        })
        .to_string();
        let gets = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let gets_c = gets.clone();
        let base = spawn_mock_feishu(std::sync::Arc::new(move |_path: &str| {
            let n = {
                let mut g = gets_c.lock().unwrap();
                *g += 1;
                *g
            };
            if n == 1 {
                (200u16, parent_body.clone())
            } else {
                (200u16, empty_body.clone())
            }
        }))
        .await;
        let cfg = mock_core_config(&base);
        let token = cached_token();
        let mut msg = mk_group_text_msg("看看这个转发");
        enrich_with_quote(&mut msg, "om_parent", &token, &cfg, "cli_mock", "sec_mock").await;
        assert_eq!(
            msg.text.as_deref(),
            Some("看看这个转发"),
            "空 items 不注入占位转录"
        );
    }
}
