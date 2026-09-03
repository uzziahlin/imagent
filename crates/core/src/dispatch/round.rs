//! 单轮 agent 执行状态机（批处理 runner 循环的循环体）。

use super::*;

impl Dispatcher {
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
        self.running.lock().await.remove(&conv_key);
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
        let base_prompt = msg.text.clone().unwrap_or_default();
        // Wave B-9：断档续接判定（base_prompt 被 move 进 prompt 载体前先算好）：
        // 无可续接会话且 prompt 命中续接词表（继续/接着/然后…，≤4 字）时，
        // 最终回复前置断档提示（见下方 reply 组装处）。
        let continuation_orphan = is_continuation_prompt(base_prompt.trim());

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
        let media_hint = if msg.media.is_empty() && msg.media_errors.is_empty() {
            String::new()
        } else {
            let mut lines: Vec<String> = msg
                .media
                .iter()
                .map(|m| format!("- {}：{}", m.kind, m.url))
                .collect();
            lines.extend(
                msg.media_errors
                    .iter()
                    .map(|e| format!("- ⚠️ 该媒体获取失败：{e}")),
            );
            format!("【用户发来媒体】\n{}\n\n——\n\n", lines.join("\n"))
        };

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
        // steering（v1.17）：运行中转向通道——dispatcher 侧保留 sender 注册进
        // running 句柄（运行中到达的文本消息注入当轮 stdin）；receiver 随 run
        // 进入 backend（不支持的后端 drop，running 注册 steer=None 消息排队）。
        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<String>(8);
        let steer_capable = backend.supports_steering();
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
        // P4-1：注册在飞句柄（/stop 中断用）。runner 持 conv 锁跨轮，同 conv 不可能
        // 并发两轮；轮次结束由 run_agent_round 统一移除。
        self.running.lock().await.insert(
            conv.0.clone(),
            RoundHandle {
                abort: join.abort_handle(),
                steer: steer_capable.then_some(steer_tx),
            },
        );

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
            Some(CardSession::new(
                self.store.clone(),
                conv.clone(),
                self.platform.name(),
                self.queued_hints.clone(),
            ))
        } else {
            None
        };
        // 真机校准 UX：轮次开始立即发「执行中」初始卡——agent 首 chunk 前的
        // 静默期（CLI 冷启动 + 模型首 token，数秒到十几秒）用户无从得知消息
        // 已被接收。非卡片平台已有 typing / 流式文本路径，不加纯文本 ack
        //（避免与后续流式分片重复）。
        if let Some(c) = card.as_mut() {
            c.ensure_started(&conv, &hint, self.platform.as_ref()).await;
        }
        // P4-3：空闲看门狗——连续 agent_idle_timeout 无任何 chunk 则 abort（杀子进程）。
        // 等权限审批期间暂停（审批有独立的 permission_ask_timeout 预算兜底）。
        let mut idle_timed_out = false;
        // P5-5：backend 提前学到的 session id（SessionStarted chunk）——中断/失败
        // 路径拿不到 RunOutcome，靠它保住已建立的会话。
        let mut learned_sid: Option<String> = None;
        // P5-10：非卡片平台已实时推送的 Text 前缀——最终回复只补差量，防重发。
        let mut streamed_text = String::new();
        // W2-2：最新任务清单状态（纯文本平台最终回复的进度行来源）。
        let mut latest_todos: Option<Vec<crate::types::TodoItem>> = None;
        loop {
            // P4-6：COT 档位每轮读取（/config 热改对下一轮生效；Wave B-7：
            // per-conv 覆盖优先，/config cot 白名单用户可改自己会话）。
            let cot = self.cot_for(&conv.0).await;
            let idle_timeout = self.idle_timeout_for(&conv.0).await;
            let chunk = if idle_timeout.is_zero() {
                match rx.recv().await {
                    Some(c) => c,
                    None => break,
                }
            } else {
                match tokio::time::timeout(idle_timeout, rx.recv()).await {
                    Ok(Some(c)) => c,
                    Ok(None) => break,
                    // D3：仅**权限审批**的 pending 豁免看门狗（审批预算
                    // permission_ask_timeout 独立兜底）；终端 ask_via_im 的 pending
                    // 超时可到 86400s，不得无限豁免 IM 会话空闲看门狗。
                    Err(_)
                        if self
                            .router
                            .has_pending_of_kind(&conv.0, PendingKind::Permission)
                            .await =>
                    {
                        continue
                    }
                    Err(_) => {
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
            };
            match chunk {
                AgentChunk::SessionStarted(sid) => {
                    // 仅记录，不产生 IM 输出；正常路径 RunOutcome 仍为权威值。
                    if learned_sid.as_deref() != Some(sid.as_str()) {
                        learned_sid = Some(sid);
                    }
                }
                AgentChunk::Final(t) => final_text = Some(t),
                AgentChunk::Error(e) => error_text = Some(e),
                AgentChunk::Thought(t) => {
                    // W2-1：思考过程仅卡片平台展示（折叠区，渲染层按 cot 档位过滤）；
                    // 纯文本平台忽略——正文流式已体现活跃，逐条思考反而刷屏。
                    if cot == CotDetail::Off {
                        continue;
                    }
                    if let Some(c) = card.as_mut() {
                        c.append_thought(&t, &conv, &hint, self.platform.as_ref())
                            .await;
                    }
                }
                AgentChunk::TodoList { items } => {
                    // W2-2：任务清单（全量替换）——卡片平台实时 checklist；
                    // 纯文本平台保留最新状态，最终回复追加进度行。
                    if let Some(c) = card.as_mut() {
                        c.set_todos(&items, &conv, &hint, self.platform.as_ref())
                            .await;
                    }
                    latest_todos = Some(items);
                }
                AgentChunk::ToolUse { tool, input, id } => {
                    // P4-6：off 档不收集工具过程（无摘要、无卡片工具面板）。
                    if cot == CotDetail::Off {
                        continue;
                    }
                    // P8-1：input JSON → 人可读单行摘要（Bash 取 command、Read 取
                    // file_path…），再按 COT 档截断——替代此前的裸 JSON 截断。
                    let summary = truncate_str(
                        &crate::render::tool_summary(&tool, &input),
                        cot.input_trunc(),
                    );
                    tool_calls.push(ToolCall {
                        name: tool.clone(),
                        summary: summary.clone(),
                        done: false,
                        id: id.clone(),
                    });
                    if let Some(c) = card.as_mut() {
                        c.append_tool(
                            &tool,
                            &summary,
                            id.as_deref(),
                            &conv,
                            &hint,
                            self.platform.as_ref(),
                        )
                        .await;
                    }
                }
                AgentChunk::ToolResult { tool, id, .. } => {
                    // P8-1：结果到达 → 翻 ✅（W2-3：优先按 id 精确配对，无 id 回退
                    // 同名最早未完成——并行同名调用不再错配）；结果内容仍不进 IM
                    //（防止把大段输出刷进卡片）。
                    if cot != CotDetail::Off {
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
                        if let Some(c) = card.as_mut() {
                            c.finish_tool(
                                &tool,
                                id.as_deref(),
                                &conv,
                                &hint,
                                self.platform.as_ref(),
                            )
                            .await;
                        }
                    }
                }
                AgentChunk::Media { path } => {
                    media_out.push(path);
                }
                AgentChunk::Text(t) => {
                    if let Some(c) = card.as_mut() {
                        c.append_text(&t, &conv, &hint, self.platform.as_ref())
                            .await;
                    } else {
                        // P2-F：中间 Text chunk 实时推 IM（流式体验，而非全部丢弃只发最终 Final）。
                        // P5-10：累积**已成功送达**的前缀，最终回复据此只补差量；
                        // P5-第五批：失败不累积——该段留给最终全量兜底，两处皆失。
                        if self.reply_ok(&conv, &t, &hint).await {
                            streamed_text.push_str(&t);
                        }
                    }
                }
            }
        }

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
                        &conv,
                        &hint,
                        self.platform.as_ref(),
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
                            &conv,
                            &hint,
                            self.platform.as_ref(),
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
                        c.finalize(
                            Some(""),
                            &tool_calls,
                            CardTerminal::Error("已中断".into()),
                            &conv,
                            &hint,
                            self.platform.as_ref(),
                        )
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
                        &conv,
                        &hint,
                        self.platform.as_ref(),
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
        // Wave B-9：上下文水位提示——本轮输入 token 超过 80k 时提醒压缩。
        // W2-5：自动压缩开启时不重复提醒（超阈值将自动 /compact，用户无需动作）；
        // 仅在自动压缩关闭（阈值 0）或水位在 80k~阈值之间时提示。
        // 取舍：OutboundCard 的 footer 无自由文本通道，提示追加在回复正文末尾
        //（卡片/纯文本两条路径都可见，语义同为「完成后给用户的建议」）。
        let auto_threshold = self.auto_compact_threshold;
        if outcome
            .usage
            .as_ref()
            .is_some_and(|u| u.input_tokens > 80_000)
            && (auto_threshold == 0
                || outcome
                    .usage
                    .as_ref()
                    .is_some_and(|u| u.input_tokens < auto_threshold))
        {
            let n = outcome.usage.as_ref().map(|u| u.input_tokens).unwrap_or(0);
            reply.push_str(&format!(
                "\n\n📊 本轮输入约 {n} tokens，上下文较大，建议 /compact。"
            ));
        }
        if let Some(c) = card.as_mut() {
            let terminal = if outcome.terminal {
                CardTerminal::Done
            } else {
                CardTerminal::Error("agent 异常退出".into())
            };
            // 成本摘要（成功终态 footer 展示 `✅ 已完成 · $0.012`）。
            c.usage_display = outcome.usage.as_ref().map(|u| u.display());
            c.finalize(
                Some(reply.as_str()),
                &tool_calls,
                terminal,
                &conv,
                &hint,
                self.platform.as_ref(),
            )
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
        if let Some(tokens) = outcome.usage.as_ref().map(ctx_tokens) {
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
