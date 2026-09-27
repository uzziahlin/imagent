//! 单轮 agent 执行状态机（批处理 runner 循环的循环体）。

use super::*;

/// P2-11（code-review v13）：非卡片平台的 Text chunk 合帧窗口。不做成配置项：
/// 与 feishu 侧 `message_fragment_interval_ms`（分片发送间隔）同族的「平台
/// 节奏」参数——两处窗口本就同量级（400ms），暴露成配置只会膨胀配置面而无人
/// 调优，跟随平台限流现状以常量维护。
const TEXT_COALESCE_WINDOW: Duration = Duration::from_millis(400);

/// P2-11：非卡片平台（ilink/wecom，或卡片平台的 reply_mode=text）Text chunk
/// 合帧缓冲。claude-acp 的 Text chunk 是 delta 级（每条几字符），逐条
/// `send_text` 会把一段长回答打成数百条几字符的 IM 消息（刷屏 + 打爆平台
/// QPS）。Text 先入缓冲，满足任一条件即 flush 为一条消息：
/// ①距上次 flush ≥ [`TEXT_COALESCE_WINDOW`]——deadline 绝对（基于上次 flush
///   时刻），由消费循环的 recv 超时驱动，chunk 连续到达不会推迟它；
/// ②非 Text chunk 到达（保序：先 flush 文本再处理该 chunk）；
/// ③Final/Error/流结束/中断退出（缓冲里未发的文本必须送达，不能丢）。
///
/// 只改发送粒度，不改既有语义：「`reply_ok` 只记成功送达前缀（P5-10 去重）+
/// 失败段落留给最终全量兜底（P5-第五批）」逐 flush 保持——用户最终仍看到
/// 完整文本。
struct TextCoalescer {
    buf: String,
    last_flush: Instant,
}

impl TextCoalescer {
    fn new() -> Self {
        Self {
            buf: String::new(),
            last_flush: Instant::now(),
        }
    }

    fn push(&mut self, delta: &str) {
        self.buf.push_str(delta);
    }

    /// 距窗口到期剩余时长（缓冲空 = None：无待发文本，无需定时）。
    fn till_deadline(&self) -> Option<Duration> {
        if self.buf.is_empty() {
            return None;
        }
        Some(TEXT_COALESCE_WINDOW.saturating_sub(self.last_flush.elapsed()))
    }

    /// flush 缓冲为一条消息；成功送达才累积进 `streamed_text` 前缀（P5-10：
    /// 失败段落留给最终全量兜底，两处皆不失）。空缓冲 no-op。
    async fn flush(
        &mut self,
        disp: &Dispatcher,
        conv: &ConvId,
        hint: &ReplyHint,
        streamed_text: &mut String,
    ) {
        if self.buf.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.buf);
        self.last_flush = Instant::now();
        // T11：观察合帧效果——本次 flush 的字节量（delta 级 chunk 应聚成较大
        // 消息；持续落在最小 bucket = 合帧失效，平台 QPS 又被逐条打爆）。
        METRICS.text_flush_bytes.observe(text.len() as f64);
        if disp.reply_ok(conv, &text, hint).await {
            streamed_text.push_str(&text);
        }
    }
}

/// P2（code-review v13）：排队深度 gauge 的 Drop guard——acquire 等待期间
/// runner task 被 abort（shutdown drain / 未来取消路径）时裸 inc/dec 会永久
/// 泄漏计数（与 feishu `token_refresh_waiters` 的 WaiterGuard 同款先例）。
struct RoundQueueGuard;

impl Drop for RoundQueueGuard {
    fn drop(&mut self) {
        crate::metrics::METRICS.round_queue_depth.dec();
    }
}

/// 构造「用户发来媒体」前置提示（run_round_inner 注入用）。
/// v13 P3（媒体 GC 与排队重放错位）：逐项检查本地文件存在性——排队消息
/// 持久化无 TTL，而媒体目录 7 天 GC（main 的 sweep_media_before），停机
/// 超过 7 天后重放的排队消息会引用已删除的死路径（agent 拿到 Read 必失败
/// 的路径且无从得知原因）；缺失项替换为过期占位（fail-soft，不阻断轮次
/// ——用户重发即可）。`MediaRef.url` 的契约是本地路径（types.rs：全平台
/// 入站媒体均落盘后填充），存在性检查即有效性检查。
pub(super) fn media_hint_for(media: &[MediaRef], media_errors: &[String]) -> String {
    if media.is_empty() && media_errors.is_empty() {
        return String::new();
    }
    let mut lines: Vec<String> = media
        .iter()
        .map(|m| {
            if std::path::Path::new(&m.url).is_file() {
                format!("- {}：{}", m.kind, m.url)
            } else {
                format!("- {}：（该媒体已过期自动清理，请让用户重发）", m.kind)
            }
        })
        .collect();
    lines.extend(
        media_errors
            .iter()
            .map(|e| format!("- ⚠️ 该媒体获取失败：{e}")),
    );
    format!("【用户发来媒体】\n{}\n\n——\n\n", lines.join("\n"))
}

/// P2（code-review v13）：全局并发护栏的轮次持票。permit 跨整轮（runner 循环
/// 单次迭代 = 一轮 + 其后的自动 compact）持有；Drop 同时归还信号量 permit 与
/// 在飞 gauge——runner task 被 abort 也经 Drop 收口，不泄漏。
pub(super) struct RoundPermit {
    /// None = 上限 0（不限制），无信号量参与（gauge 照常计数）。
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl Drop for RoundPermit {
    fn drop(&mut self) {
        // permit 先于 gauge 归还（同线程内唤醒者要等本 task 让出才可能 inc，
        // 常见路径无早退窗口；多 worker 下瞬时最多偏差 1，gauge 语义可接受）。
        drop(self.permit.take());
        crate::metrics::METRICS.running_rounds.dec();
    }
}

impl Dispatcher {
    /// P2（code-review v13）：获取全局在飞轮 permit（上限 0 = 不限制，直接放
    /// 行）。获取不到不算错误——排队等待（该 conv 后续消息照常入队等下一批，
    /// 语义自然）；等待期间持有 [`RoundQueueGuard`] 维护排队深度 gauge。
    pub(super) async fn acquire_round_permit(&self, conv: &str) -> RoundPermit {
        // 快照（limit, sem）后立即放读锁——parking_lot 守卫不跨 await。
        // 热改并发时该快照即本轮的「闸」：已在旧 sem 上排队的等待者按旧闸
        // 放行（见 reload_max_concurrent_rounds）。
        let (limit, sem) = {
            let g = self.round_gate.read();
            (g.limit, g.sem.clone())
        };
        if limit == 0 {
            crate::metrics::METRICS.running_rounds.inc();
            return RoundPermit { permit: None };
        }
        crate::metrics::METRICS.round_queue_depth.inc();
        let _queued = RoundQueueGuard;
        let running = crate::metrics::METRICS.running_rounds.get();
        debug!(
            target: "imagent::core",
            conv_id = conv,
            limit,
            running,
            "等待全局并发护栏 permit（多群/cron 齐点下的生存护栏）"
        );
        let permit = sem
            .acquire_owned()
            .await
            .expect("round semaphore 永不 close");
        drop(_queued);
        crate::metrics::METRICS.running_rounds.inc();
        RoundPermit {
            permit: Some(permit),
        }
    }

    /// 单轮 agent 执行（P4 批处理 runner 循环的循环体）：合并后的消息 → typing →
    /// 续接 session → 媒体提示 / 前情摘要注入 → 流式收集（含空闲看门狗）→ 回传 →
    /// 落库。conv 串行锁由调用方（runner 循环）持有，本函数不再管理锁。
    ///
    /// 中止语义（P4-1/P4-3）：`/stop` 或空闲看门狗 abort join task →
    /// `JoinError::is_cancelled` 分支——卡片 finalize 成 Error 终态（防流式卡片停在
    /// 「生成中」），不落 session（保留上次成功映射）。
    pub(super) async fn run_agent_round(
        &self,
        msg: InboundMessage,
        react_mids: Vec<String>,
    ) -> Option<u64> {
        let conv_key = msg.conv_id.0.clone();
        let tokens = self.run_round_inner(msg, react_mids).await;
        // 统一收尾：移除在飞注册（inner 未及注册时为幂等 no-op）。同 conv 轮次串行
        // （conv 锁），key 移除无 ABA。
        // P2（v13）：发起者锚定同步收尾——轮次结束后该 conv 无在飞发起者
        //（compact 自动轮在收尾后触发，届时审批 pending 的锚定为 None：宁漏拒
        // 不误拒，不用陈旧发起者拦人）。T18：两表已并入 ConvState 单表，同一
        // 临界区清除（with_conv 对回到全空的 entry 自动剪除）。
        self.with_conv(&conv_key, |cs| {
            cs.running = None;
            cs.round_initiator = None;
        })
        .await;
        // v1.20 崩溃轮次恢复：正常收尾（成功/失败/中断都经此）清除 inflight。
        // v1.21 review：清除失败必须 warn——残留的 inflight 行会让下次启动把
        // 已完成的轮次误判为崩溃并推 /retry（副作用类 prompt 有重复执行风险）。
        if let Err(e) = self
            .store
            .delete_config(&format!("inflight_prompt:{conv_key}"))
            .await
        {
            warn!(
                target: "imagent::core",
                conv_id = %conv_key,
                error = %e,
                "崩溃标记清除失败（下次启动可能误判崩溃轮并推 /retry）"
            );
        }
        tokens
    }

    /// 返回成功轮次的上下文水位（`usage.input_tokens`；失败/无 usage 为 None）——
    /// W2-5 自动 compact 的触发依据。
    /// 表情终态标注（best-effort）：落在轮次触发的用户消息上（merge_batch 保留
    /// 首条消息的 source_msg_id）。None 锚（合成消息/无平台 id）no-op。
    async fn react_msg(&self, conv: &ConvId, mids: &[String], done: bool) {
        let r = if done {
            crate::MsgReaction::Done
        } else {
            crate::MsgReaction::Failed
        };
        for mid in mids {
            if let Err(e) = self.platform.react_to_message(conv, mid, r).await {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "消息表情终态标注失败（不影响主流程）");
            }
        }
    }

    async fn run_round_inner(&self, msg: InboundMessage, react_mids: Vec<String>) -> Option<u64> {
        let conv = msg.conv_id.clone();
        let hint = msg.reply_hint.clone();
        let sender_id = msg.sender.0.clone();
        // v1.23 发起者锚定：本轮首条消息（merge_batch 保序首条）——平台的
        // 卡片发起者标注与按钮点击权锚定到它，不被运行中插话者漂移。
        self.platform.note_round_initiator(&conv, &sender_id).await;
        // P2（v13）：core 侧同步记录（审批 pending 的发起者锚定数据源——
        // build_im_permission_hook / permission socket 注册 pending 时读取；
        // 平台的 note_round_initiator 是平台内部状态，core 取不回来）。
        self.with_conv(&conv.0, |cs| {
            cs.round_initiator = Some(sender_id.clone());
        })
        .await;
        let base_prompt = msg.text.clone().unwrap_or_default();
        // Wave B-9：断档续接判定（base_prompt 被 move 进 prompt 载体前先算好）：
        // 无可续接会话且 prompt 命中续接词表（继续/接着/然后…，≤4 字）时，
        // 最终回复前置断档提示（见下方 reply 组装处）。
        let continuation_orphan = is_continuation_prompt(base_prompt.trim());
        // v1.23 会话可辨认：轮次 prompt 摘要（base_prompt 随后被 move 进
        // prompt 载体，此处先行截断保存）。
        let first_prompt_digest = truncate_str(base_prompt.trim(), 80);

        // P0-4 补完（v1.18 review）：本Conv 停止标记水位——下方 preamble（typing/
        // store 读/转录回放/媒体提示/表情）含多个 await，/resume 首轮可达分钟级；
        // 期间 /stop 无 running 句柄只能设标记。spawn 前按「水位之后新增」复查
        //（见 spawn 前注释），水位在此先取。
        let stop_mark_epoch = self
            .peek_conv(&conv.0, |cs| cs.and_then(|c| c.stop_requested))
            .await
            .unwrap_or(0);

        // W3-3 / P0-5（v1.17）：可重试 prompt 快照——**仅失败路径落库**（store
        // config 表，重启不丢），成功轮不覆盖——失败卡上的「重试本轮」因此
        // 永远指向失败那轮（旧内存 map 每轮覆盖：用户点按钮时重放的可能是
        // 新一批的 prompt）。
        let retry_prompt: Option<String> =
            (!base_prompt.trim().is_empty()).then(|| base_prompt.trim().to_string());

        // best-effort typing 指示（agent 处理中）；失败仅 log，不阻塞后续。
        let _ = self.platform.send_typing(&conv, &hint).await;

        // 取续接 session；store 错误仅 log 后当 None。
        // TaskList 预热（2026-09-01）：轮首任务快照判定树——②行内 task_todos
        // 非 NULL 直接用（快路径）；③NULL（/resume 的电脑端会话 / 升级首轮 /
        // 上轮中断未落库）→ backend 本地转录推导（claude 系），结果（含空）
        // 回写 DB，此后永远走②。
        let mut seed_todos: Vec<crate::types::TodoItem> = Vec::new();
        let existing: Option<SessionId> = match self.store.get_session(&conv.0).await {
            Ok(Some(row)) => {
                // 校验 agent_kind：跨后端切换时不复用旧 session_id（格式不兼容会错乱）。
                if row.agent_kind == self.backend.name() {
                    seed_todos = match &row.task_todos {
                        Some(json) => serde_json::from_str::<crate::types::TaskTodosPayload>(json)
                            .map(|p| p.items)
                            .unwrap_or_default(),
                        None => {
                            let wd = std::path::PathBuf::from(&row.workdir);
                            let sid_for_replay = row.session_id.clone();
                            // P1（v1.17）：转录回放是同步逐行 IO+解析（大会话可
                            // 达 MB 级），下放 blocking 池防阻塞 async worker。
                            let backend = self.backend.clone();
                            let derived = tokio::task::spawn_blocking(move || {
                                backend.derive_task_todos(&sid_for_replay, &wd)
                            })
                            .await
                            .unwrap_or_default()
                            .unwrap_or_default();
                            let payload = crate::types::TaskTodosPayload {
                                at: now_secs(),
                                items: derived.clone(),
                            };
                            if let Ok(s) = serde_json::to_string(&payload) {
                                if let Err(e) =
                                    self.store.set_session_todos(&conv.0, Some(&s)).await
                                {
                                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e,
                                        "转录兜底快照回写失败（不影响本轮，下轮重试）");
                                }
                            }
                            derived
                        }
                    };
                    Some(SessionId(row.session_id))
                } else {
                    warn!(
                        target: "imagent::core",
                        conv_id = %conv.0,
                        stored = %row.agent_kind,
                        current = %self.backend.name(),
                        "session 的 agent_kind 与当前后端不一致，按新建处理"
                    );
                    None
                }
            }
            Ok(None) => None,
            Err(e) => {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "get_session 失败，按新建处理");
                None
            }
        };

        // 媒体提示：把本地媒体路径前置告知 agent（claude 可 Read 本地文件）；
        // 下载失败的媒体也一并列出，让 agent 知道用户附了图但没拿到。
        // v13 P3：存在性检查与过期占位见 media_hint_for。
        let media_hint = media_hint_for(&msg.media, &msg.media_errors);

        // 新建 session（无 existing）时，一次性注入压缩摘要作为前情摘要。
        // P1-K：摘要删除推迟到 run 成功落库后——若 run 失败（session 未建成），
        // 保留摘要供下次新建注入，避免永久丢失。
        let mut prompt = base_prompt;
        let mut injected_compact_summary = false;
        if existing.is_none() {
            if let Ok(Some(summary)) = self.store.get_config(&compact_summary_key(&conv.0)).await {
                if !summary.is_empty() {
                    prompt = format!("【前情摘要】{summary}\n\n——\n\n{prompt}");
                    injected_compact_summary = true;
                }
            }
        }
        // 媒体提示置最前（在摘要之后、文本之前由上方顺序保证；此处统一前置）。
        if !media_hint.is_empty() {
            prompt = format!("{media_hint}{prompt}");
        }

        // 流式通道 + 后台执行。existing 移入 spawn（避免借用跨 'static）。
        let run_started = Instant::now();
        // 表情锚：本批全部消息的平台 id（dispatch_agent_message 收集；排队过
        // 的消息此刻从 ⏳ 翻 👀）。空 = 合成消息/命令，全程 no-op。
        // Wave B-2：本轮起点的询问登记计数快照——结束时对比，判定「本轮是否
        // 发生过审批/询问」（完成强提醒触发条件）。
        let asks_at_start = self.router.ask_count(&conv.0).await;
        // v13 P3：本轮 **Permission 类**审批登记计数快照——D3 看门狗豁免上限
        // 按本轮审批次数放大（见消费循环 D3 分支注释；Ask 类不计：终端
        // ask_via_im 的超时可到 86400s，不得放大豁免预算）。
        let perm_asks_at_start = self.router.permission_ask_count(&conv.0).await;
        let (tx, mut rx) = mpsc::channel::<AgentChunk>(32);
        let backend = self.backend.clone();
        let workdir = self.resolve_workdir(&conv.0).await;
        // Wave B-10：workdir 失效前置检查——目录不存在（被删/移动/挂载未就绪）
        // 直接回可读错误、不启动 agent：backend 在坏 cwd 上只会产出难解的
        // spawn 失败（且部分 CLI 会静默落到别的目录）。
        if !workdir.is_dir() {
            warn!(
                target: "imagent::core",
                conv_id = %conv.0,
                workdir = %workdir.display(),
                "工作目录不存在，本轮不启动 agent"
            );
            self.reply(
                &conv,
                &format!(
                    "⚠️ 工作目录 {} 不存在，本轮未执行。请 /cd 切换到有效目录，或联系管理员检查配置。",
                    workdir.display()
                ),
                &hint,
            )
            .await;
            return None;
        }
        // W4-1 per-sender 成本上限检查已挪到入队闸门（dispatch_agent_message，
        // P0-3 v1.17：逐消息检查防「排进他人批次绕过」）——此处不再重复。
        let tools = self.allowed_tools.read().clone();
        let prompt_owned = prompt.clone();
        let conv_id_owned = conv.0.clone();
        let agent_timeout = self.agent_timeout;
        // P5-5：本轮传入的 session 快照（与落库用 workdir 快照）——中断/失败分支
        // 走不到下方统一 upsert，需要它们判断「backend 是否已建立新会话」。
        let existing_sid = existing.as_ref().map(|s| s.0.clone());
        // 落库 workdir 记本轮实际使用的目录（resolve 后的 per-conv 值），而非
        // default——/cd 后两才会分叉（P5 修正，与 /resume 的记法对齐）。
        let workdir_for_row = workdir.to_string_lossy().to_string();
        // 👀「在做了」打在本批全部消息上（含排队⏳ 翻转；失败仅 warn）。
        for mid in &react_mids {
            if let Err(e) = self
                .platform
                .react_to_message(&conv, mid, crate::MsgReaction::Processing)
                .await
            {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "消息表情处理中标注失败（不影响主流程）");
            }
        }
        // v1.20 崩溃轮次恢复：轮首落 inflight 标记（prompt + 时刻）——轮次任何
        // 正常收尾（成功/失败/中断）由 run_agent_round 统一清除；进程崩溃/
        // kill -9 时残留，下次启动 recover_crashed_rounds 转成 last_prompt
        //（复用 /retry 完整机制）并通知会话。best-effort，失败不阻轮次。
        // v13 P3（双注入修复）：落 **base_prompt**（注入前，与 last_prompt 同源
        // 的 retry_prompt）而非注入后的 prompt——恢复转 /retry 重放时走完整
        // 注入管道，摘要/媒体提示由重放轮各自恰好注入一次；存注入后版本会把
        // 【前情摘要】与陈旧媒体路径提示二次带进重放轮（崩溃轮未成功落库、
        // 摘要未删且 existing=None → 再注入一次）。纯媒体轮 base 为空也照落
        //（崩溃标记语义不变），恢复侧对空 prompt 清标记不引导 /retry。
        {
            let payload = serde_json::json!({
                "prompt": retry_prompt.as_deref().unwrap_or(""),
                "at": now_secs()
            });
            if let Err(e) = self
                .store
                .set_config(&format!("inflight_prompt:{}", conv.0), &payload.to_string())
                .await
            {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "inflight 轮次标记落库失败（崩溃恢复将缺失本轮）");
            }
        }
        // steering（v1.17）：运行中转向通道——dispatcher 侧保留 sender 注册进
        // running 句柄（运行中到达的文本消息注入当轮 stdin）；receiver 随 run
        // 进入 backend（不支持的后端 drop，running 注册 steer=None 消息排队）。
        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<String>(8);
        let steer_capable = backend.supports_steering();
        // P0-4 补完：注册前停止标记复查——批循环顶部的检查到此处隔了整个
        // preamble（转录回放分钟级），期间 /stop 找不到 running 句柄、只能设
        // 标记，而标记按 60s TTL 在批循环顶部消费：起跑后即失效、用户被回
        // 「当前没有运行中的任务」但轮次照跑。以「起点水位之后新增的标记」
        // 判定（时间戳单调，免 TTL——长 preamble 也不漏），命中按中断语义收口。
        {
            let hit = self
                .with_conv(&conv.0, |cs| {
                    cs.stop_requested
                        .take()
                        .is_some_and(|ts| ts > stop_mark_epoch)
                })
                .await;
            if hit {
                self.reply(
                    &conv,
                    "⏹️ 已中断：轮次在启动前被 /stop 拦下（本批消息未执行）",
                    &hint,
                )
                .await;
                // P3（code-review v13）：上方已对本批消息打 👀（Processing），
                // 拦截 early-return 不翻回则表情恒挂「在做了」。与 /stop 中断
                // 运行中轮次同款收口（Failed 终态），保证每个 👀 都有终态。
                self.react_msg(&conv, &react_mids, false).await;
                return None;
            }
        }
        let join = tokio::spawn(async move {
            let backend_name = backend.name();
            // agent_timeout = 0 = 关闭总超时（默认 3600s=1h 硬上限）：墙钟总预算
            // 会误杀持续输出的超长任务，关闭后防挂死由空闲看门狗（idle_timeout）承担。
            if agent_timeout.is_zero() {
                return backend
                    .run(
                        &conv_id_owned,
                        &prompt_owned,
                        existing.as_ref(),
                        &workdir,
                        &tools,
                        tx,
                        &seed_todos,
                        steer_rx,
                    )
                    .await;
            }
            match tokio::time::timeout(
                agent_timeout,
                backend.run(
                    &conv_id_owned,
                    &prompt_owned,
                    existing.as_ref(),
                    &workdir,
                    &tools,
                    tx,
                    &seed_todos,
                    steer_rx,
                ),
            )
            .await
            {
                Ok(res) => res,
                Err(_elapsed) => {
                    METRICS.agent_timeouts.with_label_values(&["total"]).inc();
                    Err(crate::error::CoreError::Backend(
                        backend_name,
                        format!("agent run timed out after {agent_timeout:?}"),
                    ))
                }
            }
        });
        // T11：轮次进度共享快照——消费循环在 TodoList/ToolUse chunk 处写入，
        // /tasks 面板经 running 句柄只读（std Mutex 短临界区，零 IO）。
        let round_snap = Arc::new(std::sync::Mutex::new(RoundSnapshot::default()));
        // P4-1：注册在飞句柄（/stop 中断用）。runner 持 conv 锁跨轮，同 conv 不可能
        // 并发两轮；轮次结束由 run_agent_round 统一移除。T18：注册进 ConvState。
        self.with_conv(&conv.0, |cs| {
            cs.running = Some(RoundHandle {
                abort: join.abort_handle(),
                steer: steer_capable.then_some(steer_tx),
                started: std::time::Instant::now(),
                digest: Some(first_prompt_digest.clone()),
                snapshot: round_snap.clone(),
            });
        })
        .await;

        // 收集 chunks：Final/Error 落库，ToolUse 累积用于最终工具摘要。
        let mut final_text: Option<String> = None;
        let mut error_text: Option<String> = None;
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        // agent 产出的媒体文件路径（Write 图片）；run 结束后回传 IM。
        let mut media_out: Vec<String> = Vec::new();
        // 流式卡片：支持卡片的平台累积输出 + 节流 patch（单卡片更新），不支持则每 Text 多发文本。
        // P7-A4：reply_mode=text 用户偏好强制纯文本（不建卡，/config 可热改）。
        let card_allowed = self.platform.supports_streaming_card(&conv)
            && *self.reply_mode.read() == ReplyMode::Card;
        let mut card = if card_allowed {
            // P1-1（code-review v13）：CardSession 内部常驻 patcher 任务——chunk
            // 消费方只更新累积状态并置脏唤醒（同步、零睡眠/零平台调用），节流
            // 睡眠与 platform patch 全部移到 patcher，delta 级 chunk 流
            // （claude-acp）不再被 500ms 节流钉死消费速率。conv/hint/platform
            // 轮次内恒定，构造时一次性捕获（见 card_session.rs）。
            let s = CardSession::new(
                self.store.clone(),
                conv.clone(),
                self.platform.clone(),
                hint.clone(),
                self.conv_states.clone(),
            );
            s.set_task_digest(Some(first_prompt_digest.clone()));
            Some(s)
        } else {
            None
        };
        // 真机校准 UX：轮次开始立即发「执行中」初始卡——agent 首 chunk 前的
        // 静默期（CLI 冷启动 + 模型首 token，数秒到十几秒）用户无从得知消息
        // 已被接收。非卡片平台已有 typing / 流式文本路径，不加纯文本 ack
        //（避免与后续流式分片重复）。
        if let Some(c) = card.as_ref() {
            c.ensure_started();
        }
        // P4-3：空闲看门狗——连续 agent_idle_timeout 无任何 chunk 则 abort（杀子进程）。
        // 等权限审批期间暂停（审批有独立的 permission_ask_timeout 预算兜底）。
        let mut idle_timed_out = false;
        // P5-5：backend 提前学到的 session id（SessionStarted chunk）——中断/失败
        // 路径拿不到 RunOutcome，靠它保住已建立的会话。
        let mut learned_sid: Option<String> = None;
        // P5-10：非卡片平台已实时推送的 Text 前缀——最终回复只补差量，防重发。
        let mut streamed_text = String::new();
        // P2-11：非卡片平台的 Text 合帧缓冲（见 TextCoalescer 文档）。
        let mut text_buf = TextCoalescer::new();
        // W2-2：最新任务清单状态（纯文本平台最终回复的进度行来源）。
        let mut latest_todos: Option<Vec<crate::types::TodoItem>> = None;
        // D3 补丁（v1.18 review）：豁免总额预算（每段静默期）。router 按 conv
        // 查询会把终端侧（ask_via_im / 终端 agent 经 permission socket）挂在
        // 同一 conv 的 Permission pending 也计入——其超时可到 86400s，足以把
        // IM 轮的空闲看门狗无限豁免（agent_timeout 缺省 0 = 永不超时）。
        // 本会话审批自身预算 = permission_ask_timeout × 本轮审批次数（v13 P3
        // 起按次放大，见 D3 分支），合法豁免不会超过它；超过即照常判空闲。
        // 任何 chunk 到达（审批后 agent 复工）重置预算。
        let mut exempt_secs: u64 = 0;
        // v1.23 心跳节拍：recv 超时统一取 min(idle_timeout, HEARTBEAT_TICK)，
        // 静默期每拍做一次卡片心跳（footer 时长走动 + 审批等待态翻转）——
        // 审批等待/长静默期卡片「冻结成卡死假象」由此消除。空闲判定改为
        // since_chunk 自计数（语义与旧整段超时一致）。
        const HEARTBEAT_TICK: std::time::Duration = std::time::Duration::from_secs(30);
        let mut since_chunk = std::time::Instant::now();
        loop {
            // T11（v13 #4）：chunk channel 积压深度——每迭代刷新（背压先行信号：
            // 消费被平台 IO 钉死时该值顶到容量 32 并停留，见 metrics.rs 取舍注释）。
            METRICS.agent_channel_depth.set(rx.len() as i64);
            // P4-6：COT 档位每轮读取（/config 热改对下一轮生效；Wave B-7：
            // per-conv 覆盖优先，/config cot 白名单用户可改自己会话）。
            let cot = self.cot_for(&conv.0).await;
            let idle_timeout = self.idle_timeout_for(&conv.0).await;
            // P2-11：合帧窗口 deadline 并入 recv 超时——deadline 绝对（基于上次
            // flush 时刻），chunk 连续到达不会推迟它；到点走超时分支 flush。
            let text_due = text_buf.till_deadline();
            let chunk = if idle_timeout.is_zero() {
                // 看门狗关闭：心跳节拍仍生效（卡片可视化），但不做空闲判停。
                let tick = match text_due {
                    Some(d) => HEARTBEAT_TICK.min(d),
                    None => HEARTBEAT_TICK,
                };
                match tokio::time::timeout(tick, rx.recv()).await {
                    Ok(Some(c)) => {
                        since_chunk = std::time::Instant::now();
                        c
                    }
                    Ok(None) => break,
                    Err(_) => {
                        let waiting = self
                            .router
                            .has_pending_of_kind(&conv.0, PendingKind::Permission)
                            .await;
                        if let Some(c) = card.as_ref() {
                            c.heartbeat(waiting);
                        }
                        // P2-11：合帧窗口到点（由 recv 超时驱动）。
                        text_buf.flush(self, &conv, &hint, &mut streamed_text).await;
                        continue;
                    }
                }
            } else {
                let tick = match text_due {
                    Some(d) => idle_timeout.min(HEARTBEAT_TICK).min(d),
                    None => idle_timeout.min(HEARTBEAT_TICK),
                };
                match tokio::time::timeout(tick, rx.recv()).await {
                    Ok(Some(c)) => {
                        exempt_secs = 0;
                        since_chunk = std::time::Instant::now();
                        c
                    }
                    Ok(None) => break,
                    Err(_) => {
                        // D3：仅**权限审批**的 pending 豁免看门狗（审批预算
                        // permission_ask_timeout 独立兜底）；终端 ask_via_im 的
                        // pending 超时可到 86400s，不得无限豁免。
                        let waiting = self
                            .router
                            .has_pending_of_kind(&conv.0, PendingKind::Permission)
                            .await;
                        if since_chunk.elapsed() >= idle_timeout {
                            // v13 P3：豁免上限 = permission_ask_timeout ×
                            // max(1, 本轮 Permission 类审批登记数)。旧实现钳死
                            // 单次预算（默认 900s）：同轮 N 个连续慢审批各有
                            // 独立预算，agent 审批间隙无 chunk 时累计静默
                            // N×等待时长 会超过单次预算，第 N 个审批中途被看门狗
                            // 误杀。防挂死语义不变——豁免仍要求 Permission pending
                            // 在场（waiting），审批自身由 permission_ask_timeout
                            // 超时 fail-closed 兜底，pending 消失后按原预算照常
                            // 判停（max(1) 保底 = 无审批长静默仍按单次预算杀）。
                            let perm_asks = self.router.permission_ask_count(&conv.0).await;
                            let exempt_cap = self.permission_ask_timeout.as_secs().saturating_mul(
                                perm_asks.saturating_sub(perm_asks_at_start).max(1),
                            );
                            if exempt_secs < exempt_cap && waiting {
                                // 豁免：累计静默秒数进豁免预算，重排静默起点
                                //（语义同旧的逐段累加：审批期间逐步烧预算）。
                                exempt_secs += since_chunk.elapsed().as_secs().max(1);
                                since_chunk = std::time::Instant::now();
                            } else {
                                idle_timed_out = true;
                                METRICS.agent_timeouts.with_label_values(&["idle"]).inc();
                                warn!(
                                    target: "imagent::core",
                                    conv_id = %conv.0,
                                    idle = ?idle_timeout,
                                    "agent 空闲超时（连续无输出），终止本轮"
                                );
                                break;
                            }
                        }
                        // v1.23 心跳：静默期每拍刷新 footer（时长走动）；审批
                        // pending 时阶段翻 WaitingApproval（下个 chunk 自然翻回）。
                        if let Some(c) = card.as_ref() {
                            c.heartbeat(waiting);
                        }
                        // P2-11：合帧窗口到点（由 recv 超时驱动）。
                        text_buf.flush(self, &conv, &hint, &mut streamed_text).await;
                        continue;
                    }
                }
            };
            match chunk {
                AgentChunk::SessionStarted(sid) => {
                    // 仅记录，不产生 IM 输出；正常路径 RunOutcome 仍为权威值。
                    if learned_sid.as_deref() != Some(sid.as_str()) {
                        learned_sid = Some(sid);
                    }
                }
                AgentChunk::Final(t) => {
                    // P2-11：流收尾信号（保序）——先 flush 缓冲文本再记录终稿。
                    text_buf.flush(self, &conv, &hint, &mut streamed_text).await;
                    final_text = Some(t);
                }
                AgentChunk::Error(e) => {
                    text_buf.flush(self, &conv, &hint, &mut streamed_text).await;
                    error_text = Some(e);
                }
                AgentChunk::Thought(t) => {
                    // W2-1：思考过程仅卡片平台展示（折叠区，渲染层按 cot 档位过滤）；
                    // 纯文本平台忽略——正文流式已体现活跃，逐条思考反而刷屏。
                    if cot == CotDetail::Off {
                        continue;
                    }
                    if let Some(c) = card.as_ref() {
                        c.append_thought(&t);
                    }
                }
                AgentChunk::TodoList { items } => {
                    // W2-2：任务清单（全量替换）——卡片平台实时 checklist；
                    // 纯文本平台保留最新状态，最终回复追加进度行。
                    text_buf.flush(self, &conv, &hint, &mut streamed_text).await;
                    if let Some(c) = card.as_ref() {
                        c.set_todos(&items);
                    }
                    // T11：快照同源全量替换（/tasks 面板数据）。
                    round_snap.lock().unwrap().todos = items.clone();
                    latest_todos = Some(items);
                }
                AgentChunk::ToolUse { tool, input, id } => {
                    // P8-1：input JSON → 人可读单行摘要（Bash 取 command、Read 取
                    // file_path…）——替代此前的裸 JSON 截断。
                    let raw_summary = crate::render::tool_summary(&tool, &input);
                    // T11：工具统计快照先于 COT 档判定——/tasks 是独立查看入口，
                    // cot off 只关展示过程，不影响主动查询的面板数据；摘要截断
                    // 用固定上限（cot off 的 input_trunc=0 会把展示截断截成空）。
                    {
                        let mut s = round_snap.lock().unwrap();
                        s.tool_calls += 1;
                        s.last_tool = Some(format!("{tool} — {}", truncate_str(&raw_summary, 80)));
                    }
                    // P4-6：off 档不收集工具过程（无摘要、无卡片工具面板）。
                    if cot == CotDetail::Off {
                        continue;
                    }
                    // P2-11：非 Text chunk 到达即先 flush 缓冲文本（保序 + 提前
                    // 释放，不等窗口到期）。
                    text_buf.flush(self, &conv, &hint, &mut streamed_text).await;
                    // 展示侧摘要按 COT 档截断（卡片/最终摘要用）。
                    let summary = truncate_str(&raw_summary, cot.input_trunc());
                    tool_calls.push(ToolCall {
                        name: tool.clone(),
                        summary: summary.clone(),
                        done: false,
                        id: id.clone(),
                    });
                    if let Some(c) = card.as_ref() {
                        c.append_tool(&tool, &summary, id.as_deref());
                    }
                }
                AgentChunk::ToolResult { tool, id, .. } => {
                    // P8-1：结果到达 → 翻 ✅（W2-3：优先按 id 精确配对，无 id 回退
                    // 同名最早未完成——并行同名调用不再错配）；结果内容仍不进 IM
                    //（防止把大段输出刷进卡片）。
                    if cot != CotDetail::Off {
                        // P2-11：保序 flush（同 ToolUse）。
                        text_buf.flush(self, &conv, &hint, &mut streamed_text).await;
                        // W2-3：优先按 id 精确配对（首个借用先落地结束，再做名字
                        // 兜底——避免链式 or_else 的双重可变借用）。
                        let by_id = match id.as_deref() {
                            Some(i) => tool_calls
                                .iter_mut()
                                .find(|t| !t.done && t.id.as_deref() == Some(i)),
                            None => None,
                        };
                        let target = match by_id {
                            Some(t) => Some(t),
                            None => tool_calls.iter_mut().find(|t| !t.done && t.name == tool),
                        };
                        if let Some(t) = target {
                            t.done = true;
                        }
                        if let Some(c) = card.as_ref() {
                            c.finish_tool(&tool, id.as_deref());
                        }
                    }
                }
                AgentChunk::Media { path } => {
                    // P2-11：媒体产出与文本的先后关系在回复流里可见，先 flush。
                    text_buf.flush(self, &conv, &hint, &mut streamed_text).await;
                    media_out.push(path);
                }
                AgentChunk::Text(t) => {
                    if let Some(c) = card.as_ref() {
                        c.append_text(&t);
                    } else {
                        // P2-F：中间 Text chunk 实时推 IM（流式体验，而非全部丢弃
                        // 只发最终 Final）。P2-11：delta 级 chunk（claude-acp）逐条
                        // 发送会刷屏并打爆平台 QPS——先入合帧缓冲，由窗口/非
                        // Text chunk/流结束统一 flush（见 TextCoalescer 文档）。
                        text_buf.push(&t);
                    }
                }
            }
        }

        // T11：本轮 channel 已关闭（退出时必然为空），积压 gauge 归零——不留
        // 上一次观测的残值误导「仍有积压」。
        METRICS.agent_channel_depth.set(0);
        // P2-11：流结束/中断退出的统一收口——合帧缓冲里未发的文本必须送达
        //（不能丢），且先于终态回复/失败模板/中断标记。覆盖：channel 关闭
        //（正常收尾与 abort 后的排空——sender drop 后 recv 仍会先送完缓冲
        // chunk 再报 None）、空闲看门狗 break。
        text_buf.flush(self, &conv, &hint, &mut streamed_text).await;

        // P4-3：空闲超时 → abort join（杀子进程链路同 /stop），走下方 cancelled 分支。
        if idle_timed_out {
            join.abort();
        }

        // 等待 backend 返回 RunOutcome。
        let outcome = match join.await {
            Ok(Ok(o)) => {
                let elapsed = run_started.elapsed().as_secs_f64();
                METRICS.backend_calls.inc();
                METRICS.backend_duration.observe(elapsed);
                o
            }
            Ok(Err(e)) => {
                METRICS.backend_errors.inc();
                // S-7：统一失败文案模板（摘要 + 可续接 + 建议动作），技术细节进日志。
                let m = backend_failure_reply(self.backend.name());
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "backend.run 失败");
                if let Some(c) = card.as_mut() {
                    c.finalize(
                        Some(m.as_str()),
                        &tool_calls,
                        CardTerminal::Error(m.clone()),
                    )
                    .await;
                } else {
                    self.reply(&conv, &m, &hint).await;
                }
                // P5-5：失败路径保住已学到的 session id——部分失败轮次（如正常完成
                // 但无最终文本被 backend 判 Err）会话本身是好的，落库后下条消息
                // 续接而非静默开新会话。
                // 失败/中断路径也记 usage 事件（无 RunOutcome，tokens 记 0）。
                self.record_run_usage(&conv, None, &sender_id).await;
                self.persist_learned_session(&conv, existing_sid.as_deref(), &learned_sid)
                    .await;
                // D1：失败返回路径清理本 conv 的权限 pending（fail-closed deny +
                // 收敛询问卡），防残留 pending 把后续消息误当审批回复吞掉。
                self.cancel_pending_on_exit(&conv).await;
                // W3-3：失败后的快捷操作卡（重试/自检/新会话，仅卡片平台）。
                self.persist_retry_prompt(&conv, &retry_prompt).await;
                self.send_failure_quick_actions(&conv, &hint, retry_prompt.is_some())
                    .await;
                self.react_msg(&conv, &react_mids, false).await;
                // conv 锁由 runner 循环持有并统一释放（P1-7 防泄漏语义不变）。
                return None;
            }
            Err(e) if e.is_cancelled() => {
                // P4-1/P4-3：join task 被 abort——/stop（用户中断）或空闲看门狗。
                METRICS.backend_errors.inc();
                if idle_timed_out {
                    // S-10：时长用人读格式（「3 分钟」），不再输出 `{:?}` 的 `180s`。
                    let idle = self.idle_timeout_for(&conv.0).await;
                    let m = format!(
                        "⏱️ agent 已连续 {} 无输出，空闲超时终止本轮。已进行到的进度已保留，下条消息将续接（全新开始可 /new）。",
                        format_duration_human(idle)
                    );
                    if let Some(c) = card.as_mut() {
                        c.finalize(
                            Some(m.as_str()),
                            &tool_calls,
                            CardTerminal::Error(m.clone()),
                        )
                        .await;
                    } else {
                        self.reply(&conv, &m, &hint).await;
                    }
                } else {
                    warn!(
                        target: "imagent::core",
                        conv_id = %conv.0,
                        "agent 任务被用户 /stop 中断"
                    );
                    // /stop 命令侧已回确认，这里只把流式卡片收敛到终态（防停在「生成中」）。
                    if let Some(c) = card.as_mut() {
                        c.finalize(Some(""), &tool_calls, CardTerminal::Error("已中断".into()))
                            .await;
                    } else {
                        // S-17：纯文本平台此前中断后静默——半截流式文本后无任何标记，
                        // 用户分不清「说完了」还是「被打断」。补一条短中断标记。
                        self.reply(&conv, "⏹ 本轮已被中断", &hint).await;
                    }
                }
                // P5-5：中断路径保住已学到的 session id（与 Claude Code 自身的中断
                // 语义一致：中断留在原会话，显式 /new 才重开）。会话进度保留后，
                // 下条消息续接本轮已进行到的部分。
                // 失败/中断路径也记 usage 事件（无 RunOutcome，tokens 记 0）。
                self.record_run_usage(&conv, None, &sender_id).await;
                self.persist_learned_session(&conv, existing_sid.as_deref(), &learned_sid)
                    .await;
                // D1：中断返回路径同样清理 pending（空闲超时 abort 不经 /stop 的
                // cancel_all；/stop 已清过则此处为幂等 no-op）。
                self.cancel_pending_on_exit(&conv).await;
                // W3-3：中断后的快捷操作卡（同失败路径）。
                self.persist_retry_prompt(&conv, &retry_prompt).await;
                self.send_failure_quick_actions(&conv, &hint, retry_prompt.is_some())
                    .await;
                self.react_msg(&conv, &react_mids, false).await;
                return None;
            }
            Err(e) => {
                METRICS.backend_errors.inc();
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "backend task panic");
                // P2-5：panic 时若已收到 Final chunk，优先回传它（而非丢弃只报 panic）。
                // S-7：无 Final 时用统一失败模板（技术细节进日志）。
                let m = final_text.unwrap_or_else(|| backend_failure_reply(self.backend.name()));
                if let Some(c) = card.as_mut() {
                    c.finalize(
                        Some(m.as_str()),
                        &tool_calls,
                        CardTerminal::Error(m.clone()),
                    )
                    .await;
                } else {
                    self.reply(&conv, &m, &hint).await;
                }
                // 失败/中断路径也记 usage 事件（无 RunOutcome，tokens 记 0）。
                self.record_run_usage(&conv, None, &sender_id).await;
                self.persist_learned_session(&conv, existing_sid.as_deref(), &learned_sid)
                    .await;
                self.cancel_pending_on_exit(&conv).await;
                self.persist_retry_prompt(&conv, &retry_prompt).await;
                self.send_failure_quick_actions(&conv, &hint, retry_prompt.is_some())
                    .await;
                self.react_msg(&conv, &react_mids, false).await;
                return None;
            }
        };

        // 正常出口的表情终态：terminal=Done，非正常终止（崩溃等）=Failed。
        self.react_msg(&conv, &react_mids, outcome.terminal).await;

        // 成功路径：usage 落库 + 指标（backend 未产出 usage 时记零用量事件行）。
        self.record_run_usage(&conv, outcome.usage.as_ref(), &sender_id)
            .await;

        // 回传文本优先级：收到过的 Final > outcome.final_text > session_id 提示。
        if let Some(et) = &error_text {
            // 收到 Error chunk 也算需要提示（但 backend 正常返回，故只记录）。
            warn!(target: "imagent::core", conv_id = %conv.0, error = %et, "backend 产出 Error chunk");
        }
        let final_text_is_present = final_text.is_some();
        let outcome_has_final = !outcome.final_text.is_empty();
        let mut reply = if let Some(f) = final_text {
            f
        } else if outcome_has_final {
            outcome.final_text
        } else {
            // S-8：裸 session id 对用户无意义——只回人可读提示；session id 进日志
            //（排障仍可查到本轮会话映射）。
            info!(
                target: "imagent::core",
                conv_id = %conv.0,
                session_id = %outcome.session_id.0,
                "本轮完成但无最终文本"
            );
            "（任务已完成，未返回文本）".to_string()
        };
        // P5-10：非卡片平台已实时推送过 Text 增量——最终回复只补差量，防
        // 重发两遍（codex/gemini/ACP 中间 Text 流式 + Final 全量）。final 与
        // 已推前缀不对齐（后端语义异常）时保留全量：宁可偶发重复，不可丢内容。
        if card.is_none() && !streamed_text.is_empty() {
            if let Some(rest) = reply.strip_prefix(streamed_text.as_str()) {
                reply = rest.to_string();
            }
        }
        // Wave B-9：断档续接提示——无可续接会话但 prompt 是「继续」类词时前置
        // 说明（可能已被 /new 重置或切换后端；/resume 可找回历史）。existing
        // 已 move 进 spawn，用传入快照 existing_sid 判定。
        if continuation_orphan && existing_sid.is_none() {
            reply = format!(
                "（当前无可续接会话，可能已重置或切换后端；/resume 可恢复历史）\n\n{reply}"
            );
        }
        // 工具调用摘要：仅无卡片平台（ilink/wecom）追加文本摘要；卡片平台由 render_card
        // 的折叠面板统一渲染，避免正文与卡片块重复展示工具调用。
        if !tool_calls.is_empty() && card.is_none() && (final_text_is_present || outcome_has_final)
        {
            reply.push_str(&format_tool_summary(
                &tool_calls,
                self.cot_for(&conv.0).await,
            ));
        }
        // R1：backend 标记非正常终止（崩溃等）时，回复前置告警，让用户感知是部分输出而非正常结果。
        if !outcome.terminal {
            reply = format!("⚠️ agent 异常退出，以下为部分输出：\n\n{reply}");
        }
        // W2-4：终止原因（ACP 的 stop_reason）——非正常结束给可读提示 + 下一步。
        if let Some(sr) = outcome.stop_reason.as_deref() {
            let human = match sr {
                "max_tokens" => {
                    Some("已达到单轮输出 token 上限，回复可能被截断（可发「继续」让它接着写）")
                }
                "max_turn_requests" => {
                    Some("已达到单轮最大请求数上限，任务未完全结束（可重发消息继续）")
                }
                "refusal" => Some("agent 拒绝继续执行该请求"),
                "cancelled" => Some("本轮被取消"),
                _ => None,
            };
            if let Some(h) = human {
                reply = format!("⚠️ {h}：\n\n{reply}");
            }
        }
        // W2-2：纯文本平台追加任务清单进度（卡片平台由卡片 checklist 渲染）。
        if let Some(todos) = latest_todos
            .as_ref()
            .filter(|t| !t.is_empty() && card.is_none())
        {
            let done = todos
                .iter()
                .filter(|t| t.status == crate::types::TodoStatus::Completed)
                .count();
            reply.push_str(&format!("\n\n📋 计划进度：{}/{} 完成", done, todos.len()));
        }
        // v1.27.0：Wave B-9 的 80k 水位提示卡已随「自动压缩默认关闭」移除
        // ——默认档既不自动压缩也不主动提醒（需要者显式配置开启自动压缩）；
        // 水位仍落库（ctx_watermark）供 /status 按需查看。
        // v1.23 指令复用：成功轮 prompt 落 `last_success_prompt:<conv>`（与
        // 失败轮的 last_prompt 分键互不干扰）——/again 与失败卡的对称物。
        // v1.25 /last：同一处落 `last_round:<conv>` 快照（任务+结论+耗时+成本）
        // ——长会话回看上一轮不再滚屏。
        if outcome.terminal {
            if let Some(p) = retry_prompt.as_deref().filter(|p| !p.trim().is_empty()) {
                let payload = serde_json::json!({ "prompt": p, "at": now_secs() });
                if let Err(e) = self
                    .store
                    .set_config(
                        &format!("last_success_prompt:{}", conv.0),
                        &payload.to_string(),
                    )
                    .await
                {
                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "success prompt 落库失败（不影响本轮）");
                }
                let snapshot = serde_json::json!({
                    "prompt": p,
                    "head": super::truncate_str(reply.trim(), 400),
                    "secs": run_started.elapsed().as_secs(),
                    "usage": outcome.usage.as_ref().map(|u| u.display()).unwrap_or_default(),
                    "at": now_secs(),
                });
                if let Err(e) = self
                    .store
                    .set_config(&format!("last_round:{}", conv.0), &snapshot.to_string())
                    .await
                {
                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "last_round 快照落库失败（/last 将缺）");
                }
            }
        }
        if let Some(c) = card.as_mut() {
            let terminal = if outcome.terminal {
                CardTerminal::Done
            } else {
                CardTerminal::Error("agent 异常退出".into())
            };
            // 成本摘要（成功终态 footer 展示 `✅ 已完成 · $0.012`）。
            c.set_usage_display(outcome.usage.as_ref().map(|u| u.display()));
            c.finalize(Some(reply.as_str()), &tool_calls, terminal)
                .await;
        } else if !reply.is_empty() {
            // P5-10：流式已推完且无差量、无工具摘要时不发空消息。
            self.reply(&conv, &reply, &hint).await;
        }

        // Wave B-2：长任务/含询问轮次的完成强提醒——运行超 5 分钟或本轮发生过
        // 审批/询问时，终态额外发一条 buzz 短文本（移动端推送只露首行，短文本让
        // 「完成了」一眼可见）。仅支持 buzz 的平台发送（supports_urgent_text）——
        // 其余平台普通回复已含全部信息，再发一条只是重复噪音。best-effort。
        let elapsed = run_started.elapsed();
        let asks_delta = self
            .router
            .ask_count(&conv.0)
            .await
            .saturating_sub(asks_at_start);
        // 真机校准（2026-08）：60s 内有过审批决定 → 用户显然在线（刚批准完），
        // 跳过完成推送（实测 3m11s 轮次批准后数十秒完成仍推送，纯打扰）。
        let user_present = self
            .router
            .secs_since_decision(&conv.0)
            .await
            .is_some_and(|s| s < 60);
        if outcome.terminal
            && should_buzz_done(elapsed, asks_delta)
            && !user_present
            && self.platform.supports_urgent_text()
        {
            let text = task_done_buzz_text(
                elapsed,
                outcome.usage.as_ref().map(|u| u.display()).as_deref(),
            );
            if let Err(e) = self.platform.send_urgent_text(&conv, &text, &hint).await {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "任务完成强提醒发送失败（不影响主流程）");
            }
        }

        // agent 产图回传：run 结束文件已写完；存在才发，单个失败仅 warn 不影响其余。
        for mpath in &media_out {
            let p = std::path::Path::new(mpath);
            if !p.is_file() {
                warn!(target: "imagent::core", conv_id = %conv.0, path = %mpath, "产出的媒体文件不存在，跳过回传");
                continue;
            }
            let media = MediaRef {
                kind: "image".to_string(),
                url: mpath.clone(),
            };
            if let Err(e) = self.platform.send_media(&conv, &media, &hint).await {
                warn!(target: "imagent::core", conv_id = %conv.0, path = %mpath, error = %e, "send_media 回传失败");
            }
        }

        // 落库（upsert 内部保留 created_at；store 错误仅 log）。
        let now = now_secs();
        // 当前活动命名（不存在/空 = 默认未命名）。
        let active_name = self
            .store
            .get_config(&active_name_key(&conv.0))
            .await
            .unwrap_or(None)
            .filter(|s| !s.is_empty());
        // N8 配套：非正常终止（崩溃等）时 session_id 可能空——agent 未及分配。空 session_id
        // 无法 --resume，不入库（保留既有有效映射，避免写入无效值导致下次续接失败）。
        if outcome.session_id.0.is_empty() {
            warn!(
                target: "imagent::core",
                conv_id = %conv.0,
                "backend 返回空 session_id（疑似非正常终止），不更新 session 映射"
            );
        } else {
            // TaskList 预热：本轮最新任务快照（TodoList chunk 累积，含真实 id）
            // 随会话行落库；无任务活动的轮次保留 NULL（换绑/新会话语义正确，
            // 冷启动由转录兜底接管）。at 为将来「转录较新才重解析」预留。
            let todos_json = latest_todos
                .as_ref()
                .filter(|v| !v.is_empty())
                .and_then(|items| {
                    serde_json::to_string(&crate::types::TaskTodosPayload {
                        at: now,
                        items: items.clone(),
                    })
                    .ok()
                });
            let row = SessionRow {
                conv_id: conv.0.clone(),
                // v1.23 会话可辨认：轮次 prompt 摘要入历史副表（COALESCE——
                // 会话既有值不覆盖，仅首写/NULL 回填）。
                first_prompt: Some(first_prompt_digest.clone()),
                session_id: outcome.session_id.0.clone(),
                agent_kind: self.backend.name().to_string(),
                workdir: workdir_for_row.clone(),
                name: active_name.clone(),
                created_at: now,
                updated_at: now,
                task_todos: todos_json,
            };
            if let Err(e) = self.store.upsert_session(&row).await {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "upsert_session 失败");
            }
            // 有命名时，同步写命名侧表（可恢复/历史）。
            if let Some(name) = &active_name {
                let nrow = NamedSessionRow {
                    conv_id: conv.0.clone(),
                    name: name.clone(),
                    session_id: outcome.session_id.0.clone(),
                    agent_kind: Some(self.backend.name().to_string()),
                    workdir: Some(workdir_for_row.clone()),
                    created_at: now,
                    updated_at: now,
                };
                if let Err(e) = self.store.upsert_named_session(&nrow).await {
                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "upsert_named_session 失败");
                }
            }
        }

        // P1-K：run 成功落库后，删除已注入的 compact_summary（一次性）。
        // 失败路径已在上方 return，不会走到这里，故 summary 不会丢失。
        if injected_compact_summary {
            if let Err(e) = self
                .store
                .delete_config(&compact_summary_key(&conv.0))
                .await
            {
                warn!(
                    target: "imagent::core",
                    conv_id = %conv.0,
                    error = %e,
                    "delete_config(compact_summary) 失败（best-effort）"
                );
            }
        }
        // conv 锁由 runner 循环持有并统一释放；在飞注册由 run_agent_round 统一移除。
        // W2-5：成功轮次返回上下文水位（runner 循环据此触发自动 compact）。
        // 上下文水位可视化（v1.17）：input + cached（cache_read）≈ 上一轮完整
        // 上下文规模——缓存命中时 input_tokens 只含非缓存部分（真机 2026-09-03：
        // 长 resume 会话仅 182），不含 cached 会严重低估，/status 展示与自动
        // 压缩阈值距离双双失真。落 per-conv KV，失败仅 log。
        let ctx_tokens =
            |u: &crate::types::UsageStats| u.input_tokens + u.cached_tokens.unwrap_or(0);
        // v1.20 窗口自学习：ACP 报告的模型窗口（UsageUpdate.size）上抛校准
        // 比例档（见 note_learned_context_window；CLI 路径恒 None，no-op）。
        if let Some(w) = outcome
            .usage
            .as_ref()
            .and_then(|u| u.context_window)
            .filter(|w| *w > 0)
        {
            self.note_learned_context_window(w);
        }
        if let Some(tokens) = outcome.usage.as_ref().map(ctx_tokens) {
            // v1.27.0：水位 > 已知窗口（如 1.53M > 1M）物理上不可能来自单次
            // 请求的真实上下文——上游网关 token 口径与 CLI 本地估算分歧（中文/
            // 图片 tokenizer 差异）或缓存字段双计。数字照用（超阈压缩仍正确：
            // 上下文按网关口径确实已爆），warn 留痕便于排障。窗口来源：ACP
            // UsageUpdate.size（CLI 路径恒 None，no-op）。
            if let Some(w) = outcome
                .usage
                .as_ref()
                .and_then(|u| u.context_window)
                .filter(|w| *w > 0)
                .filter(|w| tokens > *w)
            {
                warn!(
                    target: "imagent::core",
                    conv_id = %conv.0,
                    watermark = tokens,
                    window = w,
                    "上下文水位超过模型窗口（网关 token 口径异常，仍按阈值压缩）"
                );
            }
            if let Err(e) = self
                .store
                .set_config(&format!("ctx_watermark:{}", conv.0), &tokens.to_string())
                .await
            {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "上下文水位落库失败（不影响轮次）");
            }
        }
        outcome.usage.as_ref().map(ctx_tokens)
    }

    /// P0-5（v1.17）：失败路径把可重试 prompt 落库（store config 表）——重启
    /// 不丢、成功轮不覆盖（修「失败卡重试按钮重放新一批 prompt」竞态）。
    async fn persist_retry_prompt(&self, conv: &ConvId, retry_prompt: &Option<String>) {
        if let Some(p) = retry_prompt {
            let payload = serde_json::json!({ "prompt": p, "at": now_secs() });
            if let Err(e) = self
                .store
                .set_config(&format!("last_prompt:{}", conv.0), &payload.to_string())
                .await
            {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "retry prompt 落库失败（不影响失败卡）");
            }
        }
    }

    /// W3-3：失败/中断终态后的快捷操作卡（仅卡片平台）：🔁 重试本轮（有可重试
    /// prompt 时）/ 🩺 自检 / 🆕 新会话——失败后最常用的下一步动作一键可达。
    /// 纯文本平台不发（失败文案已含 /doctor 指引；按钮卡降级为文字列表是噪音）。
    async fn send_failure_quick_actions(&self, conv: &ConvId, hint: &ReplyHint, retryable: bool) {
        if !self.platform.supports_streaming_card(conv) {
            return;
        }
        let has_retry = retryable;
        let mut buttons = Vec::new();
        if has_retry {
            buttons.push(CardButton {
                label: "🔁 重试本轮".into(),
                command: "/retry".into(),
                style: CardButtonStyle::Primary,
            });
        }
        buttons.push(CardButton {
            label: "🩺 自检".into(),
            command: "/doctor".into(),
            style: CardButtonStyle::Default,
        });
        buttons.push(CardButton {
            label: "🆕 新会话".into(),
            command: "/new".into(),
            style: CardButtonStyle::Default,
        });
        self.reply_card(
            conv,
            "🔧 下一步",
            "本轮未正常完成。可一键重试（续接会话）、自检或另起会话：",
            buttons,
            hint,
        )
        .await;
    }

    /// 每轮 usage 落库 + 指标：成功路径传 RunOutcome.usage；失败/中断路径拿不到
    /// RunOutcome，传 None（仍记一行零用量事件，保证 /stats 轮次数完整）。
    async fn record_run_usage(
        &self,
        conv: &ConvId,
        usage: Option<&crate::types::UsageStats>,
        sender: &str,
    ) {
        let backend = self.backend.name();
        if let Some(u) = usage {
            METRICS
                .token_usage
                .with_label_values(&[backend, "input"])
                .inc_by(u.input_tokens);
            METRICS
                .token_usage
                .with_label_values(&[backend, "output"])
                .inc_by(u.output_tokens);
            if let Some(c) = u.cached_tokens {
                METRICS
                    .token_usage
                    .with_label_values(&[backend, "cached"])
                    .inc_by(c);
            }
            if let Some(cost) = u.total_cost_usd {
                METRICS.cost_usd.with_label_values(&[backend]).inc_by(cost);
            }
        }
        if let Err(e) = self
            .store
            .append_run_stat(
                &conv.0,
                Some(backend),
                usage.map(|u| u.input_tokens as i64).unwrap_or(0),
                usage.map(|u| u.output_tokens as i64).unwrap_or(0),
                usage.and_then(|u| u.cached_tokens).map(|c| c as i64),
                usage.and_then(|u| u.total_cost_usd),
                Some(sender),
            )
            .await
        {
            warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "append_run_stat 失败（best-effort）");
        }
    }

    /// D1：轮次失败/超时/中断返回路径的权限 pending 清理——cancel_all 全部
    /// fail-closed deny，并按被清列表收敛 IM 侧询问卡（best-effort；无 pending
    /// 时为 no-op）。参照 `cmd_stop` 的用法。
    async fn cancel_pending_on_exit(&self, conv: &ConvId) {
        let cleared = self.router.cancel_all(&conv.0).await;
        if !cleared.is_empty() {
            if let Err(e) = self.platform.cancel_all_permission_asks(conv).await {
                warn!(
                    target: "imagent::core",
                    conv_id = %conv.0,
                    error = %e,
                    "轮次失败路径收敛权限询问卡失败（不影响 deny 结果）"
                );
            }
        }
    }
}
