//! 会话生命周期命令（重置/切换/恢复/压缩/中断）。

use super::*;
use crate::types::UserId;

impl Dispatcher {
    /// /new —— 重置会话（删活动 session + active_name）。
    pub(super) async fn cmd_new(&self, conv: &ConvId, hint: &ReplyHint) {
        // P1-F：取 conv 串行锁，与在飞 agent task 串行（避免并发改 session 损坏状态）。
        let _conv_lock = self.acquire_conv_lock(&conv.0).await;
        let _conv_guard = _conv_lock.lock().await;
        // 删除该 conv 的 session 行（下次新建），失败仅 log。
        if let Err(e) = self.store.delete_session(&conv.0).await {
            warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "delete_session 失败");
        }
        // D-记忆：新会话不继承旧的「始终允许」授权。
        self.router.clear_session_allows(&conv.0).await;
        // 清当前活动命名 → 回到默认未命名 session。
        if let Err(e) = self.store.delete_config(&active_name_key(&conv.0)).await {
            warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "delete_config(active_name) 失败");
        }
        self.reply(
            conv,
            "已重置会话，下一条消息将开启新会话（默认未命名）。",
            hint,
        )
        .await;
    }

    /// /resume [n|id] —— 统一恢复列表（IM 历史 ∪ 本机同项目）+ 接管。
    /// D7：序号缓存按 (conv, sender) 隔离——群聊多用户共用 conv，仅按 conv
    /// 缓存会互相覆盖导致错选；并带 10 分钟过期（RESUME_CACHE_TTL）。
    pub(super) async fn cmd_resume(
        &self,
        conv: &ConvId,
        sender: &UserId,
        hint: &ReplyHint,
        parts: &[&str],
    ) {
        // P4-8/P4-11：统一恢复列表 = IM 历史（📱）∪ 本机同项目会话
        // （💻，仅当前 backend 支持时合并）。用户按序号选择，无需知道
        // session id；选中 💻 即自动接管（写 sessions 表绑定）。
        // P1-F：取 conv 串行锁，与在飞 agent task 串行。
        let _conv_lock = self.acquire_conv_lock(&conv.0).await;
        let _conv_guard = _conv_lock.lock().await;
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");

        if arg.is_empty() {
            let list = self.merged_resume_list(&conv.0).await;
            if list.is_empty() {
                self.reply(conv, "暂无可恢复的会话。", hint).await;
                return;
            }
            let current = self.store.get_session(&conv.0).await.ok().flatten();
            let wd = self.resolve_workdir(&conv.0).await;
            let lines: Vec<String> = list
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    let mark = if current
                        .as_ref()
                        .is_some_and(|c| c.session_id == e.session_id)
                    {
                        " *（当前）"
                    } else {
                        ""
                    };
                    // 摘要缺省回退 id 前缀（历史行无首条消息）。
                    let desc = if e.first_prompt.is_empty() {
                        format!("{}…", &e.session_id[..e.session_id.len().min(16)])
                    } else {
                        e.first_prompt.clone()
                    };
                    let src = if e.from_local { "💻" } else { "📱" };
                    format!(
                        "{}. {src} {} {desc}{mark}",
                        i + 1,
                        format_rel_ts(e.updated_at)
                    )
                })
                .collect();
            // 缓存本列表：序号选择取缓存（防两次调用间本机会话
            // mtime 变化导致序号错位）。D7：key 按 (conv, sender) 隔离 + 带时间戳。
            self.resume_cache
                .lock()
                .await
                .insert((conv.0.clone(), sender.0.clone()), (Instant::now(), list));
            // P6-3：前 9 条各带「接管」按钮（点击 = /resume <n>；卡片按钮数克制，
            // 长列表仍以文本序号为准）。
            let buttons: Vec<CardButton> = lines
                .iter()
                .take(9)
                .enumerate()
                .map(|(i, _)| CardButton {
                    label: format!("接管 {}", i + 1),
                    command: format!("/resume {}", i + 1),
                    style: if i == 0 {
                        CardButtonStyle::Primary
                    } else {
                        CardButtonStyle::Default
                    },
                })
                .collect();
            self.reply_card(
                conv,
                &format!("⏪ 可恢复会话（当前目录 {}；💻=本机 📱=IM）", wd.display()),
                &lines.join("\n"),
                buttons,
                hint,
            )
            .await;
            return;
        }

        // 选择目标：序号 → 取缓存列表（选中即消费，防陈旧序号）；
        // 非 数字 → 按 session_id 在新鲜合并列表里找。
        let target: Option<ResumeEntry> = if let Ok(n) = arg.parse::<usize>() {
            let mut cache = self.resume_cache.lock().await;
            let key = (conv.0.clone(), sender.0.clone());
            let expired = cache
                .get(&key)
                .is_some_and(|(ts, _)| ts.elapsed() >= RESUME_CACHE_TTL);
            if expired {
                cache.remove(&key);
            }
            // D7：过期视同未列过表（列表可能已变化，引导重看）。
            // S-16：选中**不再移除**缓存条目——移除会让后续序号整体前移错位
            //（选中 1 后原 2 号变 1 号，连选即错会话）。缓存本就有 TTL（10 分钟）
            // 惰性过期防陈旧；失败路径也无需恢复缓存（条目未动）。
            cache.get(&key).and_then(|(_, l)| {
                if n >= 1 && n <= l.len() {
                    Some(l[n - 1].clone())
                } else {
                    None
                }
            })
        } else {
            self.merged_resume_list(&conv.0)
                .await
                .into_iter()
                .find(|e| e.session_id == arg)
        };
        let Some(target) = target else {
            let msg = if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_digit()) {
                "序号无效或列表已变化，请先发 /resume 查看最新列表再选。"
            } else {
                "未找到该会话（/resume 查看列表）。"
            };
            self.reply(conv, msg, hint).await;
            return;
        };
        // 跨后端校验（同 /switch P2-A）。
        let current_kind = self.backend.name();
        if target.agent_kind != current_kind {
            self.reply(
                conv,
                &format!(
                    "该会话是 {} 会话，当前后端为 {current_kind}（不互通，无法恢复）",
                    target.agent_kind
                ),
                hint,
            )
            .await;
            return;
        }
        // P5-15：本机会话接管前校验 cwd——目录编码冲突（如
        // `/a/b-c` 与 `/a/b/c` 同码）或候选误扫时，防止把别的
        // 项目的会话接到当前 workdir。cwd 缺失（旧数据/解析不到）
        // 不阻塞，仅记录。
        if target.from_local {
            if let Some(cwd) = target.cwd.as_deref().filter(|s| !s.is_empty()) {
                let wd_now = self.resolve_workdir(&conv.0).await;
                if std::path::Path::new(cwd) != wd_now {
                    warn!(
                        target: "imagent::core",
                        conv_id = %conv.0,
                        session_cwd = %cwd,
                        current_workdir = %wd_now.display(),
                        "本机会话 cwd 与当前 workdir 不符，拒绝接管"
                    );
                    self.reply(
                                        conv,
                                        &format!(
                                            "该会话属于其它目录（{cwd}），当前工作目录是 {}；如确要接管请先 /cd {cwd}",
                                            wd_now.display()
                                        ),
                                        hint,
                                    )
                                    .await;
                    return;
                }
            }
        }
        let now = now_secs();
        let row = SessionRow {
            conv_id: conv.0.clone(),
            session_id: target.session_id.clone(),
            agent_kind: current_kind.to_string(),
            workdir: self
                .resolve_workdir(&conv.0)
                .await
                .to_string_lossy()
                .to_string(),
            name: None,
            created_at: now,
            updated_at: now,
        };
        if let Err(e) = self.store.upsert_session(&row).await {
            self.reply(conv, &format!("恢复失败：{e}"), hint).await;
            return;
        }
        // 回到未命名（与命名 session 的绑定解耦，同 /switch 语义）。
        let _ = self.store.delete_config(&active_name_key(&conv.0)).await;
        let sid_short = &target.session_id[..target.session_id.len().min(16)];
        let fork_note = if target.from_local {
            "\n⚠️ 该会话来自电脑端：续接将从此处分叉（不是同步）；若终端仍开着请先退出。"
        } else {
            ""
        };
        self.reply(
            conv,
            &format!("✅ 已接管会话 {sid_short}…（下条消息续接）{fork_note}"),
            hint,
        )
        .await;
    }

    /// /switch <name> —— 切换/新建命名会话（跨后端校验）。
    pub(super) async fn cmd_switch(&self, conv: &ConvId, hint: &ReplyHint, parts: &[&str]) {
        // P1-F：取 conv 串行锁（同 /new）。
        let _conv_lock = self.acquire_conv_lock(&conv.0).await;
        let _conv_guard = _conv_lock.lock().await;
        let name = parts.get(1).map(|s| s.trim()).unwrap_or("");
        if name.is_empty() {
            // S-15：空参给可行动信息——用法 + 现有命名会话列表（可直接照名字切）。
            self.reply(conv, "用法: /switch <name>（列出 / 新建命名会话）", hint)
                .await;
            self.cmd_sessions(conv, hint).await;
            return;
        }
        match self.store.get_named_session(&conv.0, name).await {
            Ok(Some(row)) => {
                // P2-A：校验 agent_kind——不同 backend 的 session_id 不互通，
                // 切到异类 backend 的历史 session 会续接失败。
                let current_kind = self.backend.name();
                if let Some(k) = row.agent_kind.as_deref() {
                    if k != current_kind {
                        self.reply(
                                            conv,
                                            &format!(
                                                "「{name}」是 {k} 会话，当前后端为 {current_kind}（不互通，无法续接）"
                                            ),
                                            hint,
                                        )
                                        .await;
                        return;
                    }
                }
                // 切回历史命名 session：把它写成活动 session（续接用）。
                // A1 接线：store 的 switch_named_session 单事务完成「活动 session
                // 写入 + active_name + 清 compact_summary」，替代此前多次独立
                // autocommit（中间崩溃会留下 active_name 指向旧 session 的不一致）。
                let now = now_secs();
                let sr = SessionRow {
                    conv_id: conv.0.clone(),
                    session_id: row.session_id.clone(),
                    agent_kind: row
                        .agent_kind
                        .unwrap_or_else(|| self.backend.name().to_string()),
                    workdir: row
                        .workdir
                        .unwrap_or_else(|| self.default_workdir.to_string_lossy().to_string()),
                    name: Some(name.into()),
                    created_at: row.created_at,
                    updated_at: now,
                };
                if let Err(e) = self
                    .store
                    .switch_named_session(&conv.0, name, Some(&sr))
                    .await
                {
                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "switch_named_session 失败");
                }
                let sid_short: String = row.session_id.chars().take(8).collect();
                self.reply(
                    conv,
                    &format!("已切换到「{name}」（session {sid_short}…）"),
                    hint,
                )
                .await;
            }
            Ok(None) => {
                // 新命名 session：清活动 session（下次新建）+ 设 active_name。
                // A1 接线：同一原子 API（activate=None 分支）。
                if let Err(e) = self.store.switch_named_session(&conv.0, name, None).await {
                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "switch_named_session 失败");
                }
                self.reply(
                    conv,
                    &format!("已切到新会话「{name}」，下一条消息将开启。"),
                    hint,
                )
                .await;
            }
            Err(e) => {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "get_named_session 失败");
                self.reply(conv, "查询失败，请重试。", hint).await;
            }
        }
    }

    /// /sessions —— 列命名会话。
    pub(super) async fn cmd_sessions(&self, conv: &ConvId, hint: &ReplyHint) {
        match self.store.list_named_sessions(&conv.0).await {
            Ok(rows) if rows.is_empty() => {
                self.reply(conv, "无命名会话（用 /switch <name> 创建）。", hint)
                    .await;
            }
            Ok(rows) => {
                let active = self
                    .store
                    .get_config(&active_name_key(&conv.0))
                    .await
                    .unwrap_or(None)
                    .unwrap_or_default();
                let mut lines = String::from("🗂 命名会话：");
                for r in &rows {
                    let mark = if r.name == active { "（当前）" } else { "" };
                    let sid: String = r.session_id.chars().take(8).collect();
                    lines.push_str(&format!("\n- {}{}（{}…）", r.name, mark, sid));
                }
                self.reply(conv, &lines, hint).await;
            }
            Err(e) => {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "list_named_sessions 失败");
                // S-14：错误分支也要有回执——静默会让用户以为命令没送达。
                self.reply(
                    conv,
                    &format!("⚠️ 查询命名会话失败：{e}（请重试，持续失败可 /doctor 自检）"),
                    hint,
                )
                .await;
            }
        }
    }

    /// /compact —— resume 当前 session 生成摘要并重置（/stop 可中断）。
    pub(super) async fn cmd_compact(&self, conv: &ConvId, hint: &ReplyHint) {
        // P1-F：取 conv 串行锁——/compact 内 resume 当前 session 生成摘要，
        // 须与在飞 agent task 串行（否则并发 resume 同 session 损坏状态）。
        let _conv_lock = self.acquire_conv_lock(&conv.0).await;
        let _conv_guard = _conv_lock.lock().await;
        let key = compact_summary_key(&conv.0);
        let existing_sid: Option<SessionId> = match self.store.get_session(&conv.0).await {
            Ok(Some(row)) => Some(SessionId(row.session_id)),
            Ok(None) => None,
            Err(e) => {
                warn!(
                    target: "imagent::core",
                    conv_id = %conv.0,
                    error = %e,
                    "compact: get_session 失败"
                );
                None
            }
        };
        match existing_sid {
            None => {
                self.reply(conv, "当前无活动会话可压缩。", hint).await;
            }
            Some(sid) => {
                // 用 claude resume 当前 session 生成摘要；只取 Final/RunOutcome。
                let (tx, mut rx) = mpsc::channel::<AgentChunk>(32);
                let backend = self.backend.clone();
                let workdir = self.resolve_workdir(&conv.0).await;
                let tools = self.allowed_tools.read().clone();
                let conv_id_compact = conv.0.clone();
                let agent_timeout = self.agent_timeout;
                let join = tokio::spawn(async move {
                    let backend_name = backend.name();
                    // agent_timeout = 0（默认）= 关闭总超时；/stop 仍可中断本任务。
                    if agent_timeout.is_zero() {
                        return backend
                            .run(
                                &conv_id_compact,
                                "请用中文简洁总结当前对话的要点、已做决定与未完成事项（不超过 400 字）。",
                                Some(&sid),
                                &workdir,
                                &tools,
                                tx,
                            )
                            .await;
                    }
                    match tokio::time::timeout(
                                        agent_timeout,
                                        backend.run(
                                            &conv_id_compact,
                                            "请用中文简洁总结当前对话的要点、已做决定与未完成事项（不超过 400 字）。",
                                            Some(&sid),
                                            &workdir,
                                            &tools,
                                            tx,
                                        ),
                                    )
                                    .await
                                    {
                                        Ok(res) => res,
                                        Err(_elapsed) => {
                                            METRICS
                                                .agent_timeouts
                                                .with_label_values(&["total"])
                                                .inc();
                                            Err(crate::error::CoreError::Backend(
                                                backend_name,
                                                format!(
                                                    "agent run timed out after {agent_timeout:?}"
                                                ),
                                            ))
                                        }
                                    }
                });
                // P5-16：注册进 running——/stop 此前中断不了 /compact
                //（长摘要生成只能干等 agent_timeout）。conv 锁由本命令
                // 持有，注册/移除无 ABA（新轮次须先等锁）。
                self.running
                    .lock()
                    .await
                    .insert(conv.0.clone(), join.abort_handle());
                let mut summary: Option<String> = None;
                while let Some(chunk) = rx.recv().await {
                    if let AgentChunk::Final(t) = chunk {
                        summary = Some(t);
                    }
                }
                let join_res = join.await;
                // 无论成败，先摘除在飞注册（/stop 已抢先摘除时为 no-op）。
                self.running.lock().await.remove(&conv.0);
                let summary_text = match join_res {
                    Ok(Ok(o)) => summary.unwrap_or(o.final_text),
                    Ok(Err(e)) => {
                        warn!(
                            target: "imagent::core",
                            conv_id = %conv.0,
                            error = %e,
                            "compact 摘要生成失败"
                        );
                        self.reply(conv, &format!("生成摘要失败：{e}"), hint).await;
                        return;
                    }
                    Err(e) => {
                        warn!(
                            target: "imagent::core",
                            conv_id = %conv.0,
                            error = %e,
                            "compact 摘要任务 panic"
                        );
                        self.reply(conv, &format!("摘要任务异常：{e}"), hint).await;
                        return;
                    }
                };
                // 存摘要 + 重置活动 session + 清 active_name（释放 context）。
                if let Err(e) = self.store.set_config(&key, &summary_text).await {
                    warn!(
                        target: "imagent::core",
                        conv_id = %conv.0,
                        error = %e,
                        "set_config(compact_summary) 失败"
                    );
                }
                if let Err(e) = self.store.delete_session(&conv.0).await {
                    warn!(
                        target: "imagent::core",
                        conv_id = %conv.0,
                        error = %e,
                        "compact: delete_session 失败"
                    );
                }
                if let Err(e) = self.store.delete_config(&active_name_key(&conv.0)).await {
                    warn!(
                        target: "imagent::core",
                        conv_id = %conv.0,
                        error = %e,
                        "compact: delete_config(active_name) 失败"
                    );
                }
                self.reply(
                    conv,
                    &format!(
                        "已压缩会话。摘要：\n\n{summary_text}\n\n（新会话将保留此摘要延续上下文）"
                    ),
                    hint,
                )
                .await;
            }
        }
    }

    /// /stop —— 中断在飞任务（撤 pending 审批 + 清批处理队列）。
    pub(super) async fn cmd_stop(&self, conv: &ConvId, hint: &ReplyHint) {
        // P4-1：中断该 conv 的在飞 agent 任务。**不取 conv 串行锁**——
        // 取了会等到任务自然结束才生效（等价于没停）。
        // 若正等 IM 权限审批：pending 回复通道被 cancel 以 deny 唤醒 →
        // MCP 立即收到 deny（fail-closed），agent 侧不悬挂。
        // P5-第五批：仅当确有 pending 审批才撤询问卡——否则会把该
        // conv 上一次已被正常回答的旧卡误 patch 成「已中断」。
        // D6：去掉前置 has_pending（与 cancel_all 两步锁间隙可能被 route 击穿），
        // 直接 cancel_all 并按其返回的被清列表判断是否需要收敛询问卡。
        let cleared = self.router.cancel_all(&conv.0).await;
        // D-记忆：中断即收回「始终允许」授权——/stop 语义是全停，旧授权不应
        // 在下一次任务继续生效。
        self.router.clear_session_allows(&conv.0).await;
        if !cleared.is_empty() {
            // P5-16：收敛审批询问本身——把 IM 里滞留的询问卡片 patch 成
            // 「已中断」（纯文本询问平台 no-op）。best-effort。多 pending 并存
            // 后按 conv 全量收敛（终端 ask 与 IM 审批都可能挂着）。
            if let Err(e) = self.platform.cancel_all_permission_asks(conv).await {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "撤回权限询问失败（不影响中断）");
            }
        }
        let running = self.running.lock().await.remove(&conv.0);
        let aborted = if let Some(h) = &running {
            // abort → backend.run future drop → 杀子进程：
            // CLI 后端 kill_on_drop；ACP 后端 cancel 分支 → 杀连接。
            h.abort();
            true
        } else {
            false
        };
        // stop = 全停：清空排队待合并的消息（P4-2 批处理队列）+ 排队状态（P10）。
        // S-4（原子）：hint 清理移进 queues 同一临界区——分两步会有间隙，期间
        // 新消息入队（写 hint）又被下一步清掉，hint 与实际队列错位。
        let dropped = {
            let mut map = self.queues.lock().await;
            let n = map.remove(&conv.0).map(|q| q.len()).unwrap_or(0);
            self.queued_hints.lock().await.remove(&conv.0);
            n
        };
        let text = match (aborted, dropped) {
            (true, 0) => "🛑 已中断当前任务".to_string(),
            (true, n) => format!("🛑 已中断当前任务（丢弃 {n} 条排队消息）"),
            (false, 0) => "ℹ️ 当前没有运行中的任务".to_string(),
            (false, n) => {
                format!("ℹ️ 当前没有运行中的任务（丢弃 {n} 条排队消息）")
            }
        };
        self.reply(conv, &text, hint).await;
    }
}
