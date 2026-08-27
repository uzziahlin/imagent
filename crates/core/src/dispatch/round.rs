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
    pub(super) async fn run_agent_round(&self, msg: InboundMessage) {
        let conv_key = msg.conv_id.0.clone();
        self.run_round_inner(msg).await;
        // 统一收尾：移除在飞注册（inner 未及注册时为幂等 no-op）。同 conv 轮次串行
        // （conv 锁），key 移除无 ABA。
        self.running.lock().await.remove(&conv_key);
    }

    async fn run_round_inner(&self, msg: InboundMessage) {
        let conv = msg.conv_id.clone();
        let hint = msg.reply_hint.clone();
        let base_prompt = msg.text.clone().unwrap_or_default();

        // best-effort typing 指示（agent 处理中）；失败仅 log，不阻塞后续。
        let _ = self.platform.send_typing(&conv, &hint).await;

        // 取续接 session；store 错误仅 log 后当 None。
        let existing: Option<SessionId> = match self.store.get_session(&conv.0).await {
            Ok(Some(row)) => {
                // 校验 agent_kind：跨后端切换时不复用旧 session_id（格式不兼容会错乱）。
                if row.agent_kind == self.backend.name() {
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
        let (tx, mut rx) = mpsc::channel::<AgentChunk>(32);
        let backend = self.backend.clone();
        let workdir = self.resolve_workdir(&conv.0).await;
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
        let join = tokio::spawn(async move {
            let backend_name = backend.name();
            match tokio::time::timeout(
                agent_timeout,
                backend.run(
                    &conv_id_owned,
                    &prompt_owned,
                    existing.as_ref(),
                    &workdir,
                    &tools,
                    tx,
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
        self.running
            .lock()
            .await
            .insert(conv.0.clone(), join.abort_handle());

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
        loop {
            // P4-6：COT 档位每轮读取（/config 热改对下一轮生效）。
            let cot = *self.cot_detail.read();
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
                    Err(_) if self.router.has_pending(&conv.0).await => continue,
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
                AgentChunk::ToolUse { tool, input } => {
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
                    });
                    if let Some(c) = card.as_mut() {
                        c.append_tool(&tool, &summary, &conv, &hint, self.platform.as_ref())
                            .await;
                    }
                }
                AgentChunk::ToolResult { tool, .. } => {
                    // P8-1：结果到达 → 同名最早未完成的调用翻 ✅（卡片工具行的
                    // ⏳→✅ 反馈）；结果内容仍不进 IM（防止把大段输出刷进卡片）。
                    if cot != CotDetail::Off {
                        if let Some(t) = tool_calls.iter_mut().find(|t| !t.done && t.name == tool) {
                            t.done = true;
                        }
                        if let Some(c) = card.as_mut() {
                            c.finish_tool(&tool, &conv, &hint, self.platform.as_ref())
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
                let m = format!("[error] {e}");
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
                self.persist_learned_session(&conv, existing_sid.as_deref(), &learned_sid)
                    .await;
                // conv 锁由 runner 循环持有并统一释放（P1-7 防泄漏语义不变）。
                return;
            }
            Err(e) if e.is_cancelled() => {
                // P4-1/P4-3：join task 被 abort——/stop（用户中断）或空闲看门狗。
                METRICS.backend_errors.inc();
                if idle_timed_out {
                    let m = format!(
                        "⏱️ agent 已连续 {:?} 无输出，空闲超时终止本轮。已进行到的进度已保留，下条消息将续接（全新开始可 /new）。",
                        self.idle_timeout_for(&conv.0).await
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
                    }
                }
                // P5-5：中断路径保住已学到的 session id（与 Claude Code 自身的中断
                // 语义一致：中断留在原会话，显式 /new 才重开）。会话进度保留后，
                // 下条消息续接本轮已进行到的部分。
                self.persist_learned_session(&conv, existing_sid.as_deref(), &learned_sid)
                    .await;
                return;
            }
            Err(e) => {
                METRICS.backend_errors.inc();
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "backend task panic");
                // P2-5：panic 时若已收到 Final chunk，优先回传它（而非丢弃只报 panic）。
                let m = final_text.unwrap_or_else(|| format!("[error] backend task panicked: {e}"));
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
                self.persist_learned_session(&conv, existing_sid.as_deref(), &learned_sid)
                    .await;
                return;
            }
        };

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
            format!("(done, session={})", outcome.session_id.0)
        };
        // P5-10：非卡片平台已实时推送过 Text 增量——最终回复只补差量，防
        // codex/gemini/ACP（中间 Text 流式 + Final 全量）整段重发两遍。final 与
        // 已推前缀不对齐（后端语义异常）时保留全量：宁可偶发重复，不可丢内容。
        if card.is_none() && !streamed_text.is_empty() {
            if let Some(rest) = reply.strip_prefix(streamed_text.as_str()) {
                reply = rest.to_string();
            }
        }
        // 工具调用摘要：仅无卡片平台（ilink/wecom）追加文本摘要；卡片平台由 render_card
        // 的折叠面板统一渲染，避免正文与卡片块重复展示工具调用。
        if !tool_calls.is_empty() && card.is_none() && (final_text_is_present || outcome_has_final)
        {
            reply.push_str(&format_tool_summary(&tool_calls, *self.cot_detail.read()));
        }
        // R1：backend 标记非正常终止（崩溃等）时，回复前置告警，让用户感知是部分输出而非正常结果。
        if !outcome.terminal {
            reply = format!("⚠️ agent 异常退出，以下为部分输出：\n\n{reply}");
        }
        if let Some(c) = card.as_mut() {
            let terminal = if outcome.terminal {
                CardTerminal::Done
            } else {
                CardTerminal::Error("agent 异常退出".into())
            };
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
            let row = SessionRow {
                conv_id: conv.0.clone(),
                session_id: outcome.session_id.0.clone(),
                agent_kind: self.backend.name().to_string(),
                workdir: workdir_for_row.clone(),
                name: active_name.clone(),
                created_at: now,
                updated_at: now,
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
    }
}
