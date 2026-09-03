//! 状态 / 环境类命令（自检、工作目录、工作空间、媒体、帮助）。

use super::*;

/// Wave B-11：审批分组聚合结果（/stats 展示用）。
struct ApprovalStats {
    total: usize,
    allow: usize,
    deny: usize,
    timeout: usize,
    always: usize,
    /// 有 waited_secs 的行数与总和（平均响应时长 = sum / n）。
    waited_n: usize,
    waited_sum: u64,
}

impl ApprovalStats {
    /// 从 permission_decision 审计行聚合（decision 词 + waited_secs 从 detail 解析）。
    fn from_audit(rows: &[imagent_store::AuditRow]) -> Self {
        let mut s = Self {
            total: 0,
            allow: 0,
            deny: 0,
            timeout: 0,
            always: 0,
            waited_n: 0,
            waited_sum: 0,
        };
        for r in rows {
            let Some(d) = r.detail.as_deref() else {
                continue;
            };
            s.total += 1;
            match audit_detail_field(d, "decision") {
                Some("allow_always") => s.always += 1,
                Some("allow") => s.allow += 1,
                Some("deny") => s.deny += 1,
                Some("timeout") => s.timeout += 1,
                _ => {}
            }
            if let Some(w) =
                audit_detail_field(d, "waited_secs").and_then(|v| v.parse::<u64>().ok())
            {
                s.waited_n += 1;
                s.waited_sum += w;
            }
        }
        s
    }

    /// 展示行：`N 次 · allow 60% · deny 30% · timeout 10% · always 5% · 平均响应 3 分钟`。
    fn summary_line(&self) -> String {
        if self.total == 0 {
            return "0 次".to_string();
        }
        let pct = |n: usize| n * 100 / self.total;
        let mut out = format!(
            "{} 次 · allow {}% · deny {}% · timeout {}% · always {}%",
            self.total,
            pct(self.allow),
            pct(self.deny),
            pct(self.timeout),
            pct(self.always)
        );
        if self.waited_n > 0 {
            let avg = self.waited_sum / self.waited_n as u64;
            out.push_str(&format!(
                " · 平均响应 {}",
                format_duration_human(Duration::from_secs(avg))
            ));
        }
        out
    }
}

/// Wave B-11：审计 detail 的空格分隔 k=v 字段提取（`decision=allow waited_secs=3`）。
fn audit_detail_field<'a>(detail: &'a str, key: &str) -> Option<&'a str> {
    detail.split_whitespace().find_map(|kv| {
        kv.split_once('=')
            .filter(|(k, _)| *k == key)
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
    })
}

impl Dispatcher {
    /// /status —— 本会话 + 全局运行状态。
    pub(super) async fn cmd_status(&self, conv: &ConvId, hint: &ReplyHint) {
        // P4-7：本会话 + 全局运行状态。
        let running_here = self.running.lock().await.contains_key(&conv.0);
        let queued_here = self
            .queues
            .lock()
            .await
            .get(&conv.0)
            .map(|q| q.len())
            .unwrap_or(0);
        let in_flight = self.running.lock().await.len();
        let wd = self.resolve_workdir(&conv.0).await;
        let name_key = active_name_key(&conv.0);
        let (sess, active) = tokio::join!(
            self.store.get_session(&conv.0),
            self.store.get_config(&name_key)
        );
        let sess_desc = match sess {
            Ok(Some(row)) => {
                let name = active.ok().flatten().unwrap_or_default();
                let label = if name.is_empty() {
                    "未命名".to_string()
                } else {
                    name
                };
                format!(
                    "{label}（{}…，{}）",
                    row.session_id.chars().take(12).collect::<String>(),
                    row.agent_kind
                )
            }
            _ => "无（下条消息新建）".to_string(),
        };
        // 上下文水位（v1.17）：上轮 usage.input_tokens + 与自动压缩阈值的距离
        //（0 = 自动压缩关闭，仅展示水位）。
        let ctx_line = match self
            .store
            .get_config(&format!("ctx_watermark:{}", conv.0))
            .await
        {
            Ok(Some(raw)) => raw.parse::<u64>().ok().map(|tokens| {
                let threshold = self.auto_compact_threshold;
                if threshold > 0 {
                    let pct = tokens * 100 / threshold.max(1);
                    format!("\n- 🧠 上下文：{tokens} tok（阈值 {threshold}，{pct}%）")
                } else {
                    format!("\n- 🧠 上下文：{tokens} tok（自动压缩已关闭）")
                }
            }),
            _ => None,
        }
        .unwrap_or_default();
        let text = format!(
                            "📊 当前状态\n- 🤖 后端：{}（{}）\n- 💬 本会话：{}，排队 {} 条\n- 🔗 会话：{sess_desc}{ctx_line}\n- 📁 工作目录：{}\n- 🏃 全局在飞：{in_flight} 个\n- ⏱️ 运行时长：{}",
                            self.backend.name(),
                            self.platform.name(),
                            if running_here { "任务在跑" } else { "无任务" },
                            queued_here,
                            wd.display(),
                            format_uptime(self.started_at.elapsed()),
                        );
        self.reply(conv, &text, hint).await;
    }

    /// /doctor —— 自检（workdir/store/在飞任务）。
    pub(super) async fn cmd_doctor(&self, conv: &ConvId, hint: &ReplyHint) {
        // P4-7：自检——workdir / store / 后端 / 在飞任务。
        let mut lines = Vec::new();
        let wd = self.resolve_workdir(&conv.0).await;
        match std::fs::metadata(&wd) {
            Ok(m) if m.is_dir() => lines.push(format!("✅ 工作目录可用：{}", wd.display())),
            Ok(_) => lines.push(format!("⚠️ 工作目录不是目录：{}", wd.display())),
            Err(e) => lines.push(format!("⚠️ 工作目录不可访问：{}（{e}）", wd.display())),
        }
        // store 写读回环（config KV）。
        let probe_key = format!("doctor_probe:{}", now_secs());
        match self.store.set_config(&probe_key, "1").await {
            Ok(()) => match self.store.get_config(&probe_key).await {
                Ok(Some(v)) if v == "1" => lines.push("✅ 存储读写正常（SQLite）".into()),
                _ => lines.push("⚠️ 存储读回异常".into()),
            },
            Err(e) => lines.push(format!("⚠️ 存储写入失败：{e}")),
        }
        let _ = self.store.delete_config(&probe_key).await;
        let n_sess = self.store.count_sessions().await.unwrap_or(-1);
        if n_sess >= 0 {
            lines.push(format!("✅ 会话映射：{n_sess} 条"));
        } else {
            lines.push("⚠️ 会话映射计数失败".into());
        }
        let in_flight = self.running.lock().await.len();
        lines.push(if in_flight == 0 {
            "✅ 无在飞任务".to_string()
        } else {
            format!("ℹ️ 在飞任务 {in_flight} 个（/stop 可中断）")
        });
        lines.push(format!(
            "ℹ️ 平台 {} / 后端 {}（{}）",
            self.platform.name(),
            self.backend.name(),
            if self.platform.supports_streaming_card(conv) {
                "支持流式卡片"
            } else {
                "纯文本"
            }
        ));
        let text = format!("🩺 自检结果：\n{}", lines.join("\n"));
        self.reply(conv, &text, hint).await;
    }

    /// /reconnect —— 强制平台重连。
    pub(super) async fn cmd_reconnect(&self, conv: &ConvId, hint: &ReplyHint) {
        // P4-7：强制平台重连（排查长连接僵死）。
        match self.platform.reconnect().await {
            Ok(()) => {
                self.reply(conv, "🔌 已触发平台重连（后台进行中，稍候生效）。", hint)
                    .await
            }
            Err(e) => {
                self.reply(
                    conv,
                    &format!("⚠️ 重连指令失败：{e}（平台可能不支持，可重启 imagent）"),
                    hint,
                )
                .await
            }
        }
    }

    /// /cd [path] —— 查看/切换 per-conv 工作目录。
    pub(super) async fn cmd_cd(&self, conv: &ConvId, hint: &ReplyHint, parts: &[&str]) {
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
        if arg.is_empty() {
            let wd = self.resolve_workdir(&conv.0).await;
            self.reply(conv, &format!("当前工作目录：{}", wd.display()), hint)
                .await;
            return;
        }
        let p = std::path::Path::new(arg);
        if !p.is_absolute() {
            self.reply(conv, "用法：/cd <绝对路径>（须绝对路径）", hint)
                .await;
            return;
        }
        if !p.is_dir() {
            self.reply(conv, &format!("目录不存在：{arg}"), hint).await;
            return;
        }
        // P6-8：过宽目录拒绝（/、home 根、系统目录等——agent 以 cwd 定位工作区）。
        if let Err(e) = crate::config::validate_workdir(p) {
            self.reply(conv, &format!("❌ {e}"), hint).await;
            return;
        }
        // 改 per-conv workdir：取 conv 锁串行，与在飞 agent task 隔离。
        let _conv_lock = self.acquire_conv_lock(&conv.0).await;
        let _conv_guard = _conv_lock.lock().await;
        match self.store.set_config(&workdir_key(&conv.0), arg).await {
            Ok(_) => {
                // P5 快赢：/resume 列表缓存随 workdir 失效——列表按
                // conv 当前目录扫描，切目录后旧序号指向的是旧目录的
                // 会话（且接管前有 cwd 校验兜底）。
                // D7：缓存 key 已改为 (conv, sender)，按 conv 前缀全量失效。
                self.resume_cache
                    .lock()
                    .await
                    .retain(|(c, _), _| c != &conv.0);
                self.reply(
                    conv,
                    &format!("✅ 工作目录已切到 {arg}（下条消息生效）"),
                    hint,
                )
                .await
            }
            Err(e) => self.reply(conv, &format!("保存失败：{e}"), hint).await,
        }
    }

    /// /ws [list|save|use|remove] —— 命名工作空间管理。
    pub(super) async fn cmd_ws(&self, conv: &ConvId, hint: &ReplyHint, parts: &[&str]) {
        let sub = parts.get(1).map(|s| s.trim()).unwrap_or("");
        let arg = parts.get(2).map(|s| s.trim()).unwrap_or("");
        match sub {
            "" | "list" => match self.store.list_config("workspace:").await {
                Ok(rows) if rows.is_empty() => self.reply(conv, "（暂无命名工作空间）", hint).await,
                Ok(rows) => {
                    // CardKit 视觉改版：/ws 列表改 markdown 表格（| 名称 | 路径 |）；
                    // 飞书卡渲染层按名称配对「使用/删除」双列按钮。
                    let mut table = String::from("| 名称 | 路径 |\n|---|---|\n");
                    for (k, v) in &rows {
                        let name = k.strip_prefix("workspace:").unwrap_or(k);
                        table.push_str(&format!("| {} | {} |\n", name, v.replace('|', "\\|")));
                    }
                    // P6-3：每个空间一个「使用」按钮（点击 = /ws use <name>）。
                    // P9-1：每个空间「使用」（primary）+「删除」（danger）两钮，
                    // 对标 lcab workspacesCard 的 切换/删除。
                    let buttons: Vec<CardButton> = rows
                        .iter()
                        .flat_map(|(k, _)| {
                            let name = k.strip_prefix("workspace:").unwrap_or(k).to_string();
                            vec![
                                CardButton {
                                    label: format!("使用 {name}"),
                                    command: format!("/ws use {name}"),
                                    style: CardButtonStyle::Primary,
                                },
                                CardButton {
                                    label: format!("删除 {name}"),
                                    command: format!("/ws remove {name}"),
                                    style: CardButtonStyle::Danger,
                                },
                            ]
                        })
                        .collect();
                    self.reply_card(conv, "📁 命名工作空间", &table, buttons, hint)
                        .await
                }
                Err(e) => self.reply(conv, &format!("列出失败：{e}"), hint).await,
            },
            "save" => {
                if arg.is_empty() {
                    self.reply(conv, "用法：/ws save <name>", hint).await;
                    return;
                }
                let wd = self.resolve_workdir(&conv.0).await;
                match self
                    .store
                    .set_config(&workspace_key(arg), &wd.to_string_lossy())
                    .await
                {
                    Ok(_) => {
                        self.reply(
                            conv,
                            &format!("✅ 已保存工作空间「{arg}」= {}", wd.display()),
                            hint,
                        )
                        .await
                    }
                    Err(e) => self.reply(conv, &format!("保存失败：{e}"), hint).await,
                }
            }
            "use" => {
                if arg.is_empty() {
                    self.reply(conv, "用法：/ws use <name>", hint).await;
                    return;
                }
                match self.store.get_config(&workspace_key(arg)).await {
                    Ok(Some(path)) => {
                        let p = std::path::Path::new(&path);
                        if !p.is_dir() {
                            self.reply(conv, &format!("目录不存在：{path}"), hint).await;
                            return;
                        }
                        // P6-8：同 /cd——存储的目录也过安全校验（历史数据可能宽泛）。
                        if let Err(e) = crate::config::validate_workdir(p) {
                            self.reply(
                                conv,
                                &format!("❌ 工作空间「{arg}」目录过宽，拒绝切换：{e}"),
                                hint,
                            )
                            .await;
                            return;
                        }
                        // 改 per-conv workdir：取 conv 锁串行，与在飞 agent task 隔离（同 /cd）。
                        let _conv_lock = self.acquire_conv_lock(&conv.0).await;
                        let _conv_guard = _conv_lock.lock().await;
                        match self.store.set_config(&workdir_key(&conv.0), &path).await {
                            Ok(_) => {
                                // P5-第五批：同 /cd——切目录后失效
                                // /resume 列表缓存（列表按当前目录扫描）。
                                // D7：缓存 key 已改为 (conv, sender)，按 conv 前缀全量失效。
                                self.resume_cache
                                    .lock()
                                    .await
                                    .retain(|(c, _), _| c != &conv.0);
                                self.reply(conv, &format!("✅ 已切到「{arg}」（{path}）"), hint)
                                    .await
                            }
                            Err(e) => self.reply(conv, &format!("切换失败：{e}"), hint).await,
                        }
                    }
                    Ok(None) => {
                        self.reply(conv, &format!("无此工作空间：{arg}"), hint)
                            .await
                    }
                    Err(e) => self.reply(conv, &format!("读取失败：{e}"), hint).await,
                }
            }
            "remove" => {
                if arg.is_empty() {
                    self.reply(conv, "用法：/ws remove <name>", hint).await;
                    return;
                }
                match self.store.delete_config(&workspace_key(arg)).await {
                    Ok(_) => {
                        self.reply(conv, &format!("✅ 已删除工作空间「{arg}」"), hint)
                            .await
                    }
                    Err(e) => self.reply(conv, &format!("删除失败：{e}"), hint).await,
                }
            }
            _ => {
                self.reply(
                    conv,
                    "用法：/ws [list|save <name>|use <name>|remove <name>]",
                    hint,
                )
                .await
            }
        }
    }

    /// /img <path> —— 发送 workdir 内图片（路径越界拒绝）。
    pub(super) async fn cmd_img(&self, conv: &ConvId, hint: &ReplyHint, parts: &[&str]) {
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
        if arg.is_empty() {
            self.reply(
                conv,
                "用法：/img <图片路径>（相对当前工作目录或绝对路径）",
                hint,
            )
            .await;
            return;
        }
        let wd = self.resolve_workdir(&conv.0).await;
        let raw = std::path::Path::new(arg);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            wd.join(raw)
        };
        // 安全校验：canonicalize 后必须仍在 workdir 内——与 agent 的
        // Read 权限对齐（能 Read 才能发），防任意路径（~/.ssh 等）外传。
        let wd_real = match wd.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                self.reply(conv, &format!("工作目录不可用：{e}"), hint)
                    .await;
                return;
            }
        };
        let real = match joined.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                self.reply(conv, &format!("文件不存在：{arg}"), hint).await;
                return;
            }
        };
        if !real.starts_with(&wd_real) {
            self.reply(
                conv,
                &format!("拒绝：{arg} 不在当前工作目录内（/cd 可切换）"),
                hint,
            )
            .await;
            return;
        }
        if !real.is_file() {
            self.reply(conv, &format!("不是文件：{arg}"), hint).await;
            return;
        }
        let ext_ok = real
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                matches!(
                    e.to_ascii_lowercase().as_str(),
                    "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
                )
            })
            .unwrap_or(false);
        if !ext_ok {
            self.reply(conv, "仅支持图片（png/jpg/jpeg/gif/webp/bmp）", hint)
                .await;
            return;
        }
        let media = MediaRef {
            kind: "image".to_string(),
            url: real.to_string_lossy().into_owned(),
        };
        match self.platform.send_media(conv, &media, hint).await {
            Ok(()) => self.reply(conv, &format!("✅ 已发送：{arg}"), hint).await,
            Err(e) => self.reply(conv, &format!("发送失败：{e}"), hint).await,
        }
    }

    /// /file <path> —— 发送 workdir 内任意文件（P6-7：路径越界拒绝，同 /img）。
    pub(super) async fn cmd_file(&self, conv: &ConvId, hint: &ReplyHint, parts: &[&str]) {
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
        if arg.is_empty() {
            self.reply(
                conv,
                "用法：/file <文件路径>（相对当前工作目录或绝对路径）",
                hint,
            )
            .await;
            return;
        }
        let wd = self.resolve_workdir(&conv.0).await;
        let raw = std::path::Path::new(arg);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            wd.join(raw)
        };
        // 安全校验：同 /img——canonicalize 后必须仍在 workdir 内（能 Read 才能发）。
        let Ok(wd_real) = wd.canonicalize() else {
            self.reply(conv, "工作目录不可用", hint).await;
            return;
        };
        let Ok(real) = joined.canonicalize() else {
            self.reply(conv, &format!("文件不存在：{arg}"), hint).await;
            return;
        };
        if !real.starts_with(&wd_real) {
            self.reply(
                conv,
                &format!("拒绝：{arg} 不在当前工作目录内（/cd 可切换）"),
                hint,
            )
            .await;
            return;
        }
        if !real.is_file() {
            self.reply(conv, &format!("不是文件：{arg}"), hint).await;
            return;
        }
        let media = MediaRef {
            kind: "file".to_string(),
            url: real.to_string_lossy().into_owned(),
        };
        match self.platform.send_media(conv, &media, hint).await {
            Ok(()) => self.reply(conv, &format!("✅ 已发送：{arg}"), hint).await,
            Err(e) => self.reply(conv, &format!("发送失败：{e}"), hint).await,
        }
    }

    /// /timeout [N|off|default] —— 会话级空闲看门狗（P6-9：分钟粒度覆盖全局
    /// agent_idle_timeout_secs；off=本会话关闭；default=清除覆盖回到全局）。
    pub(super) async fn cmd_timeout(&self, conv: &ConvId, hint: &ReplyHint, parts: &[&str]) {
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
        let global = self.agent_idle_timeout.read().as_secs();
        if arg.is_empty() {
            let cur = match self.idle_overrides.lock().await.get(&conv.0) {
                Some(d) if d.is_zero() => "已关闭（本会话覆盖）".to_string(),
                Some(d) => format!("{} 分钟（本会话覆盖）", d.as_secs() / 60),
                None => format!("跟随全局 {global} 秒（0=关）"),
            };
            self.reply(
                conv,
                &format!(
                    "当前空闲看门狗：{cur}\n用法：/timeout <分钟> | /timeout off | /timeout default"
                ),
                hint,
            )
            .await;
            return;
        }
        match arg.to_ascii_lowercase().as_str() {
            "off" => {
                self.idle_overrides
                    .lock()
                    .await
                    .insert(conv.0.clone(), Duration::ZERO);
                self.reply(conv, "✅ 本会话空闲看门狗已关闭（仅本会话）", hint)
                    .await;
            }
            "default" => {
                self.idle_overrides.lock().await.remove(&conv.0);
                self.reply(
                    conv,
                    &format!("✅ 已清除本会话覆盖，回到全局 {global} 秒"),
                    hint,
                )
                .await;
            }
            _ => match arg.parse::<u64>() {
                Ok(n) if n > 0 => {
                    // L5（code-review v8）：checked_mul 防整型溢出（debug panic /
                    // release 回绕自 DoS）+ 30 天上限（43200 分钟）防误设永关。
                    const TIMEOUT_MAX_MINUTES: u64 = 30 * 24 * 60;
                    let Some(n) = n.checked_mul(1).filter(|n| *n <= TIMEOUT_MAX_MINUTES) else {
                        self.reply(
                            conv,
                            &format!("❌ 分钟数需 ≤ {TIMEOUT_MAX_MINUTES}（30 天）"),
                            hint,
                        )
                        .await;
                        return;
                    };
                    let d = Duration::from_secs(n * 60);
                    self.idle_overrides.lock().await.insert(conv.0.clone(), d);
                    self.reply(
                        conv,
                        &format!("✅ 本会话空闲看门狗 = {n} 分钟（agent 连续无输出即终止）"),
                        hint,
                    )
                    .await;
                }
                Ok(_) => {
                    self.reply(conv, "分钟数须 ≥ 1（关闭请用 /timeout off）", hint)
                        .await
                }
                Err(_) => {
                    self.reply(
                        conv,
                        "用法：/timeout <分钟> | /timeout off | /timeout default",
                        hint,
                    )
                    .await
                }
            },
        }
    }

    /// /model [name|default] —— 查看/热切运行时模型（W1-2）。
    ///
    /// 仅支持模型选择的后端可用（claude-cli `--model` / claude-acp env）；
    /// 查看对所有白名单用户开放，**切换需 admin**——模型影响成本与行为，多人
    /// 共用网关时不宜任意成员切换。进程内生效，重启/SIGHUP 恢复 config 的
    /// `claude_model` 基准值；切换落审计。
    pub(super) async fn cmd_model(
        &self,
        conv: &ConvId,
        sender: &str,
        hint: &ReplyHint,
        parts: &[&str],
    ) {
        if !self.backend.supports_model_selection() {
            self.reply(
                conv,
                &format!(
                    "当前后端 {} 暂不支持模型选择（保持其默认模型）。",
                    self.backend.name()
                ),
                hint,
            )
            .await;
            return;
        }
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
        if arg.is_empty() {
            let cur = self
                .backend
                .model()
                .unwrap_or_else(|| "（默认，跟随 CLI/本机配置）".to_string());
            self.reply(
                conv,
                &format!(
                    "当前模型：{cur}\n用法：/model <名称>（切换，需管理员） · /model default（恢复默认）"
                ),
                hint,
            )
            .await;
            return;
        }
        if !self.is_admin(sender) {
            let msg = if self.admin_senders.read().is_empty() {
                "切换模型需要管理员（admin_senders 为空，IM 内不可用；请在 config.toml 配置 admin_senders 或运行 imagent setup）。".to_string()
            } else {
                "切换模型需要管理员（查看用 /model）。".to_string()
            };
            self.reply(conv, &msg, hint).await;
            return;
        }
        if arg.eq_ignore_ascii_case("default") {
            self.backend.set_model(None);
            self.reply(conv, "✅ 已恢复默认模型（CLI/本机配置）。", hint)
                .await;
            return;
        }
        // 模型名合理性：单个词 + 长度上限（防把整段文本当模型名传给 CLI）。
        // L13（code-review v8）：字符白名单——ACP 路径模型名会拼进命令串过
        // shell_words::split，空格/引号/`=` 前缀可拆出多余 argv 改变 spawn 行为。
        if arg.chars().count() > 64
            || arg.split_whitespace().count() != 1
            || !arg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-[]:".contains(c))
            || arg.starts_with('=')
        {
            self.reply(
                conv,
                "模型名须为单个词且不超过 64 字符（如 sonnet / opus / haiku 或完整模型 id）。",
                hint,
            )
            .await;
            return;
        }
        let model = arg.to_string();
        self.backend.set_model(Some(model.clone()));
        if let Err(e) = self
            .store
            .append_audit(
                "model_switch",
                Some(sender),
                Some(&conv.0),
                Some(&format!("model={model}")),
            )
            .await
        {
            tracing::warn!(target: "imagent::core", error = %e, "append_audit(model_switch) 失败");
        }
        self.reply(
            conv,
            &format!("✅ 模型已切换为 {model}（下一轮生效；重启后恢复 config 配置）。"),
            hint,
        )
        .await;
    }

    /// /retry —— 重发本会话**最近一次失败轮**的用户 prompt（W3-3：失败/中断后
    /// 一键续接会话再试）。走与普通消息**完全相同**的 handle 路径（鉴权/批处理/
    /// 轮次语义一致），无需独立执行逻辑。
    /// P0-5（v1.17）：数据源从内存 map 改为 store config（`last_prompt:<conv>`，
    /// 仅失败路径写入）——重启不丢、成功轮不覆盖、失败卡按钮永远指向失败那轮。
    pub(super) async fn cmd_retry(
        &self,
        conv: &ConvId,
        sender: &crate::types::UserId,
        hint: &ReplyHint,
    ) {
        let prompt = match self
            .store
            .get_config(&format!("last_prompt:{}", conv.0))
            .await
        {
            Ok(Some(raw)) => serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .and_then(|v| v.get("prompt").and_then(|p| p.as_str()).map(str::to_string))
                .filter(|p| !p.trim().is_empty()),
            _ => None,
        };
        let Some(prompt) = prompt else {
            self.reply(
                conv,
                "本会话没有可重试的历史指令（最近一轮成功，或直接重发消息即可开始）。",
                hint,
            )
            .await;
            return;
        };
        let msg = InboundMessage {
            conv_id: conv.clone(),
            sender: sender.clone(),
            text: Some(prompt),
            media: Vec::new(),
            media_errors: Vec::new(),
            mentions: Vec::new(),
            mentioned_bot: false,
            ask_req: None,
            reply_to: None,
            source_msg_id: None,
            control: None,
            reply_hint: hint.clone(),
        };
        self.dispatch_agent_message(msg).await;
    }

    /// /export —— 当前会话导出为 Markdown 文件回传（W4-2）。走 backend 的本机
    /// 会话存储转录（claude 系支持；codex/gemini 回不支持提示）。导出文件经
    /// send_media 发送后即删（media 目录，0600）。
    pub(super) async fn cmd_export(&self, conv: &ConvId, hint: &ReplyHint) {
        let Some(row) = self.store.get_session(&conv.0).await.ok().flatten() else {
            self.reply(
                conv,
                "当前无活动会话可导出（先发一条消息开启会话；/resume 可恢复历史）。",
                hint,
            )
            .await;
            return;
        };
        let wd = self.resolve_workdir(&conv.0).await;
        let Some(md) = self
            .backend
            .export_session_markdown(&wd, &row.session_id)
            .await
        else {
            self.reply(
                conv,
                &format!(
                    "当前后端 {} 暂不支持会话导出（仅 claude 系后端有本机会话转录）。",
                    self.backend.name()
                ),
                hint,
            )
            .await;
            return;
        };
        // 写临时导出文件（media 目录 0700 / 文件 0600，与入站媒体同纪律）→
        // send_media 回传 → 删除。
        let dir = crate::paths::imagent_home().join("media");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.reply(conv, &format!("导出目录创建失败：{e}"), hint)
                .await;
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let sid8: String = row.session_id.chars().take(8).collect();
        let fname = format!("session-{sid8}-{}.md", now_secs());
        let path = dir.join(&fname);
        if let Err(e) = std::fs::write(&path, md) {
            self.reply(conv, &format!("导出文件写入失败：{e}"), hint)
                .await;
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        let media = MediaRef {
            kind: "file".to_string(),
            url: path.to_string_lossy().into_owned(),
        };
        match self.platform.send_media(conv, &media, hint).await {
            Ok(()) => {
                self.reply(conv, &format!("✅ 已导出会话 {sid8}…（{fname}）"), hint)
                    .await
            }
            Err(e) => {
                self.reply(conv, &format!("导出文件发送失败：{e}"), hint)
                    .await;
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    /// /queue [drop <n>] —— 查看本会话排队中的消息（agent 运行期间到达、待下
    /// 一轮合并）；`drop <n>` 选择性丢弃（仅自己的消息或 admin）。P0-5 同批
    ///（v1.17）：此前队列是黑盒——只有 /stop 回执里的数字与整队清空。
    pub(super) async fn cmd_queue(
        &self,
        conv: &ConvId,
        sender: &str,
        hint: &ReplyHint,
        parts: &[&str],
    ) {
        // drop 子命令：先做权限与序号校验，再在同一 queues 临界区移除并刷新
        // queued_hints（count 变化需反映到卡片 footer）。
        if parts.get(1).map(|s| s.trim()) == Some("drop") {
            let Some(n) = parts.get(2).and_then(|s| s.trim().parse::<usize>().ok()) else {
                self.reply(
                    conv,
                    "用法：/queue drop <序号>（序号见 /queue 列表，1 起）",
                    hint,
                )
                .await;
                return;
            };
            if n == 0 {
                self.reply(conv, "序号从 1 开始。", hint).await;
                return;
            }
            let is_admin = self.is_admin(sender);
            let removed_sender;
            {
                let mut map = self.queues.lock().await;
                let Some(q) = map.get_mut(&conv.0) else {
                    self.reply(conv, "队列为空。", hint).await;
                    return;
                };
                if n > q.len() {
                    let len = q.len();
                    drop(map);
                    self.reply(conv, &format!("序号超出范围（当前 {len} 条）。"), hint)
                        .await;
                    return;
                }
                let idx = n - 1;
                removed_sender = q[idx].sender.0.clone();
                if removed_sender != sender && !is_admin {
                    self.reply(conv, "只能丢弃自己排队的消息（或联系管理员处理）。", hint)
                        .await;
                    return;
                }
                let removed = q.remove(idx);
                let count = q.len();
                if count == 0 {
                    // 与取批路径同语义：留空 Vec 不删 entry（runner 循环依赖）。
                    self.queued_hints.lock().await.remove(&conv.0);
                } else if let Some(last) = q.last() {
                    self.queued_hints.lock().await.insert(
                        conv.0.clone(),
                        crate::card_session::QueuedHint {
                            count,
                            latest: super::super::latest_snippet(last),
                        },
                    );
                }
                drop(map);
                let snippet = removed
                    .text
                    .as_deref()
                    .map(|t| super::super::truncate_str(t.trim(), 30))
                    .unwrap_or_else(|| "（纯媒体）".into());
                self.reply(
                    conv,
                    &format!("🗑️ 已丢弃第 {n} 条（{removed_sender}）：{snippet}"),
                    hint,
                )
                .await;
                return;
            }
        }
        // 列表视图。
        let list: Vec<(String, String)> = {
            let map = self.queues.lock().await;
            match map.get(&conv.0) {
                None => Vec::new(),
                Some(q) => q
                    .iter()
                    .map(|m| {
                        let snippet = match m.text.as_deref() {
                            Some(t) if !t.trim().is_empty() => {
                                super::super::truncate_str(t.trim(), 40)
                            }
                            _ if !m.media.is_empty() => format!("（媒体 ×{}）", m.media.len()),
                            _ => "（空）".into(),
                        };
                        (m.sender.0.clone(), snippet)
                    })
                    .collect(),
            }
        };
        if list.is_empty() {
            self.reply(conv, "📭 本会话当前没有排队中的消息。", hint)
                .await;
            return;
        }
        let mut body = String::from("📋 排队中的消息（下一轮合并执行）：");
        for (i, (s, snippet)) in list.iter().enumerate() {
            body.push_str(&format!("\n{}. 【{s}】{snippet}", i + 1));
        }
        body.push_str("\n\n丢弃某条：/queue drop <序号>（仅自己的或 admin）。");
        self.reply(conv, &body, hint).await;
    }

    /// /stats [today|7d|all] —— token 用量/成本统计（默认 7d）。全局 + 本会话
    /// 两组维度；无成本数据的 backend（codex/gemini）按 tokens 汇总展示。
    pub(super) async fn cmd_stats(
        &self,
        conv: &ConvId,
        sender: &str,
        hint: &ReplyHint,
        parts: &[&str],
    ) {
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
        let (label, since) = match arg.to_ascii_lowercase().as_str() {
            "" | "7d" => ("近 7 天".to_string(), now_secs() - 7 * 86_400),
            "today" => ("今日".to_string(), now_secs() - 86_400),
            "all" => ("累计".to_string(), 0),
            other => {
                self.reply(
                    conv,
                    &format!("未知时间范围：{other}（可用：today / 7d / all）"),
                    hint,
                )
                .await;
                return;
            }
        };
        let rows = match self.store.list_run_stats_since(since).await {
            Ok(r) => r,
            Err(e) => {
                self.reply(conv, &format!("读取用量统计失败：{e}"), hint)
                    .await;
                return;
            }
        };
        if rows.is_empty() {
            self.reply(conv, &format!("📈 {label}暂无运行记录。"), hint)
                .await;
            return;
        }
        // 聚合：全局与本会话两组（tokens 求和；cost 仅累加有值的行——无成本
        // 数据的 backend 只体现在 tokens 维度）。
        let agg = |subset: &[imagent_store::RunStatRow]| {
            let runs = subset.len();
            let input: i64 = subset.iter().map(|r| r.input_tokens).sum();
            let output: i64 = subset.iter().map(|r| r.output_tokens).sum();
            let cost: f64 = subset.iter().filter_map(|r| r.cost_usd).sum();
            (runs, input, output, cost)
        };
        let (g_runs, g_in, g_out, g_cost) = agg(&rows);
        // per-sender 成本 Top5（admin 可见——多人网关下「谁花了多少」；sender 非
        // 个人隐私但跨用户成本数据按最小披露原则收敛到管理员）。
        let per_sender_line = if self.is_admin(sender) {
            let mut by_sender: std::collections::HashMap<&str, (usize, f64)> =
                std::collections::HashMap::new();
            for r in &rows {
                if let Some(sp) = &r.sender {
                    let e = by_sender.entry(sp.as_str()).or_default();
                    e.0 += 1;
                    e.1 += r.cost_usd.unwrap_or(0.0);
                }
            }
            let mut top: Vec<(&str, usize, f64)> = by_sender
                .into_iter()
                .map(|(s, (runs, cost))| (s, runs, cost))
                .collect();
            top.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
            top.iter()
                .take(5)
                .map(|(s, runs, cost)| {
                    let sid = s.rsplit_once('_').map(|(_, t)| t).unwrap_or(s);
                    let short: String = sid.chars().take(8).collect();
                    format!("- `{short}…`：{runs} 轮 · ${cost:.4}")
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            String::new()
        };
        let mine: Vec<imagent_store::RunStatRow> =
            rows.into_iter().filter(|r| r.conv_id == conv.0).collect();
        let (m_runs, m_in, m_out, m_cost) = agg(&mine);
        let cost_line = |c: f64| {
            if c > 0.0 {
                format!("${c:.4}")
            } else {
                "（无成本数据，按 tokens 汇总）".to_string()
            }
        };
        // Wave B-11：审批分组——从 permission_decision 审计聚合（固定近 7 天，
        // 不随 /stats 的时间范围参数变化：审批统计看趋势，7 天是稳定样本窗口）。
        let appr_line = match self
            .store
            .list_audit_since("permission_decision", now_secs() - 7 * 86_400)
            .await
        {
            Ok(rows) => ApprovalStats::from_audit(&rows).summary_line(),
            Err(e) => format!("读取失败：{e}"),
        };
        let sender_section = if per_sender_line.is_empty() {
            String::new()
        } else {
            format!("\n- 👥 发起者 Top：\n{per_sender_line}")
        };
        let text = format!(
            "📈 用量统计（{label}）\n- 🌍 全局：{g_runs} 轮 · 输入 {g_in} · 输出 {g_out} tokens\n- 💰 全局成本：{}\n- 💬 本会话：{m_runs} 轮 · 输入 {m_in} · 输出 {m_out} tokens\n- 💸 本会话成本：{}{sender_section}\n- 🛡️ 审批（近 7 天）：{appr_line}\n- 用法：/stats [today|7d|all]",
            cost_line(g_cost),
            cost_line(m_cost),
        );
        // CardKit 视觉改版：卡片平台回命令卡（markdown 表格）；纯文本平台保持
        // 现有列表文本（表格只发生在卡渲染层）。
        if self.platform.supports_streaming_card(conv) {
            let sender_md = if per_sender_line.is_empty() {
                String::new()
            } else {
                format!("\n**👥 发起者 Top**\n{per_sender_line}\n")
            };
            let table = format!(
                "| 维度 | 轮数 | 输入 tokens | 输出 tokens | 成本 |\n|---|---|---|---|---|\n| 🌍 全局 | {g_runs} | {g_in} | {g_out} | {} |\n| 💬 本会话 | {m_runs} | {m_in} | {m_out} | {} |\n{sender_md}\n- 🛡️ 审批（近 7 天）：{appr_line}\n- 用法：/stats [today|7d|all]",
                cost_line(g_cost),
                cost_line(m_cost),
            );
            self.reply_card(
                conv,
                &format!("📈 用量统计（{label}）"),
                &table,
                vec![],
                hint,
            )
            .await;
        } else {
            self.reply(conv, &text, hint).await;
        }
    }

    /// /audit [n] —— 审计日志（admin 门槛同 /config 等管理命令；默认最近 10 条，
    /// 上限 50）。格式：时间 · 动作 · 操作者 · 摘要。
    pub(super) async fn cmd_audit(
        &self,
        conv: &ConvId,
        sender: &str,
        hint: &ReplyHint,
        parts: &[&str],
    ) {
        // admin 门槛：与 /allow、/config 等管理命令一致。
        if !self.is_admin(sender) {
            let msg = if self.admin_senders.read().is_empty() {
                "仅管理员（admin_senders）可查看审计日志。当前 admin_senders 为空（无人是管理员），\
                 请在本地通过 CLI（`imagent setup` 或 config.toml 的 admin_senders）配置后再使用管理命令。"
                    .to_string()
            } else {
                "仅管理员（admin_senders）可查看审计日志。".to_string()
            };
            self.reply(conv, &msg, hint).await;
            return;
        }
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
        let n: usize = if arg.is_empty() {
            10
        } else {
            match arg.parse() {
                Ok(n) if (1..=50).contains(&n) => n,
                _ => {
                    self.reply(conv, "用法：/audit [条数]（1-50，默认 10）", hint)
                        .await;
                    return;
                }
            }
        };
        let rows = match self.store.list_audit(n).await {
            Ok(r) => r,
            Err(e) => {
                self.reply(conv, &format!("读取审计日志失败：{e}"), hint)
                    .await;
                return;
            }
        };
        if rows.is_empty() {
            self.reply(conv, "📋 审计日志为空。", hint).await;
            return;
        }
        let lines: Vec<String> = rows
            .iter()
            .map(|r| {
                let actor = r.actor.as_deref().unwrap_or("-");
                // 摘要 = 目标 + 详情（有则拼），截断防刷屏。
                let mut detail = r.target.clone().unwrap_or_default();
                if let Some(d) = &r.detail {
                    if !detail.is_empty() {
                        detail.push(' ');
                    }
                    detail.push_str(d);
                }
                let detail = if detail.is_empty() {
                    String::new()
                } else {
                    format!("（{}）", truncate_str(&detail, 60))
                };
                format!(
                    "- {} · {} · {}{}",
                    format_rel_ts(r.ts),
                    r.action,
                    actor,
                    detail
                )
            })
            .collect();
        let text = format!(
            "📋 审计日志（最近 {} 条）：\n{}",
            lines.len(),
            lines.join("\n")
        );
        // CardKit 视觉改版：卡片平台回命令卡（markdown 表格）；纯文本平台保持
        // 现有列表文本。
        if self.platform.supports_streaming_card(conv) {
            let mut table = String::from("| 时间 | 动作 | 操作者 | 摘要 |\n|---|---|---|---|\n");
            for r in &rows {
                let actor = r.actor.as_deref().unwrap_or("-");
                let mut detail = r.target.clone().unwrap_or_default();
                if let Some(d) = &r.detail {
                    if !detail.is_empty() {
                        detail.push(' ');
                    }
                    detail.push_str(d);
                }
                table.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    format_rel_ts(r.ts),
                    r.action.replace('|', "\\|"),
                    actor.replace('|', "\\|"),
                    truncate_str(&detail, 60).replace('|', "\\|")
                ));
            }
            self.reply_card(conv, "📋 审计日志", &table, vec![], hint)
                .await;
        } else {
            self.reply(conv, &text, hint).await;
        }
    }

    /// /help —— 命令总表（P6-3：飞书等卡片平台带常用命令按钮）。
    pub(super) async fn cmd_help(&self, conv: &ConvId, hint: &ReplyHint) {
        let body = "🗂 会话\n- /new 重置会话\n- /switch <name> 切换/新建命名会话\n- /sessions 列出命名会话\n- /resume [n] 恢复历史/本机会话\n- /compact 压缩上下文\n- /retry 重试最近一轮（失败后一键续接）\n- /export 导出当前会话为 Markdown\n\n📁 目录与文件\n- /cd <path> 切工作目录\n- /ws save|use|remove <name> 命名工作空间\n- /img <path> 发图片 · /file <path> 发文件\n\n🛡️ 权限与运行\n- /perm <off|allow|deny|ask> 权限模式\n- /stop 中断任务（排队消息保留并自动续跑；/stop all 全部丢弃）\n- /queue [drop <n>] 查看/丢弃排队中的消息\n- /timeout <分钟|off|default> 会话级空闲看门狗\n- /model [名称|default] 查看/切换模型（切换需管理员）\n\n🧪 状态与诊断\n- /status 状态 · /doctor 自检 · /reconnect 重连\n- /config [k v] 查看/热改配置 · /audit [n] 审计日志\n\n👥 白名单与管理（管理员）\n- /allow、/disallow 授权/撤权（飞书群内可 @ 对方）\n- /chat allow|deny|allow-all|list 会话白名单\n- /admin list|add|remove 管理员\n- /list 白名单 · /whoami 我的 id\n\n💬 会话规则：群主时间线直接 @我 = 续同一会话；点消息「回复」进话题 = 开独立会话（互不共享上下文/待办）。\n\n其他内容直接发给 agent 即可（运行中发文字会实时转入当前轮次、下个工具边界生效 👀；图片/文件等媒体走排队、合并进下一轮 ⏳）。";
        let buttons = vec![
            CardButton {
                label: "📊 状态".into(),
                command: "/status".into(),
                style: CardButtonStyle::Primary,
            },
            CardButton {
                label: "🗂 会话".into(),
                command: "/sessions".into(),
                style: CardButtonStyle::Default,
            },
            CardButton {
                label: "⏪ 恢复".into(),
                command: "/resume".into(),
                style: CardButtonStyle::Default,
            },
            CardButton {
                label: "📁 空间".into(),
                command: "/ws list".into(),
                style: CardButtonStyle::Default,
            },
            CardButton {
                label: "🩺 诊断".into(),
                command: "/doctor".into(),
                style: CardButtonStyle::Default,
            },
            CardButton {
                label: "⏹ 中断".into(),
                command: "/stop".into(),
                style: CardButtonStyle::Danger,
            },
        ];
        self.reply_card(conv, "🤖 imagent 命令", body, buttons, hint)
            .await;
    }
}
