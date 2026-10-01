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

// ---------- T8（v13 安全批）：/doctor 安全自检 ----------
// 六项检查各为独立纯函数（数据由 cmd_doctor 采集后传入），便于单测直接构造
// 各状态断言文案。目标读者：跑公网 webhook / 多人群白名单 / 默认配置的用户
// ——把「只有读了 SECURITY.md 才知道」的部署风险变成一条命令可见。

/// 字节数 → 人读形态（体积信息行用）：`42 B` / `1.2 KB` / `3.4 MB` / `1.1 GB`。
fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// 检查 ①：凭据形态（数据源：store credentials 只读聚合
/// [`imagent_store::Store::credential_forms`]）。
/// 明文行存在且无 passphrase = ❌（明文进 SQLite 及 WAL 副本，headless 回退的
/// 真实泄漏面；补救 = 设 `IMAGENT_PASSPHRASE` 走加密回退形态）；明文行存在但
/// 已设 passphrase = ⚠️（读取时惰性迁移为加密形态）；无明文（keyring 指针 /
/// 加密）或无凭据 = ✅。
pub(crate) fn doctor_credential_line(forms: &imagent_store::CredentialForms) -> String {
    if forms.total() == 0 {
        return "✅ 凭据：无落库凭据".into();
    }
    if forms.plaintext > 0 {
        if !forms.passphrase_set {
            return format!(
                "❌ 凭据明文落盘 {} 条且未设 passphrase——设 IMAGENT_PASSPHRASE 可走加密回退形态（见 SECURITY.md 威胁模型）",
                forms.plaintext
            );
        }
        return format!(
            "⚠️ 凭据明文落盘 {} 条（已设 passphrase，读取时会惰性迁移为加密形态）",
            forms.plaintext
        );
    }
    format!(
        "✅ 凭据形态：keyring {} · 加密 {}（无明文）",
        forms.keyring, forms.encrypted
    )
}

/// 检查 ②：webhook 暴露面（数据源：main 装配时注入的 [`crate::WebhookExposure`]
/// 摘要——core 拿不到 Config 与绑定事实）。三档：非 loopback × 任一条目无
/// secret = ❌（启动期 fail-closed 应已拒绝，出现即配置漂移——复查兜底）；
/// 非 loopback × 全有 secret = ⚠️（提示重放防护层：签名去重 LRU 恒开 +
/// replay_window 建议）；loopback / 未启用 = ✅。
pub(crate) fn doctor_webhook_line(exposure: &crate::WebhookExposure) -> String {
    if !exposure.listening {
        return "✅ webhook 入站未启用（无 HTTP 暴露面）".into();
    }
    if exposure.loopback {
        return "✅ webhook 绑定 loopback（仅本机可达）".into();
    }
    if exposure.entry_secrets.iter().any(|s| !s) {
        return "❌ webhook 非 loopback 且有条目未配 secret——公网裸 token 即可伪造事件（启动期应已拒绝，出现即配置漂移，请复查 config）".into();
    }
    let replay = if exposure.any_replay_window {
        "时间戳协议已启用"
    } else {
        "建议配置 replay_window_secs 启用时间戳窗口"
    };
    format!("⚠️ webhook 暴露非 loopback 地址：验签 secret 已全配；重放防护 = 签名去重 LRU 恒开，{replay}")
}

/// 检查 ③：共享工作区。数据源：`workdir:<conv>` 覆盖键（config 表）∪ sessions
/// 表会话枚举；有效 workdir = 覆盖值否则 `default_workdir`（与 `resolve_workdir`
/// 同口径）。多个 conv 指向同一路径 = ⚠️（协作模型：成员互见产物——多用户
/// 共享 default_workdir 的后果此前只有 SECURITY.md 有写）。路径归一化仅做
/// trim + 去尾部 `/`（`/a/dir` 与 `/a/dir/` 同目录；不做 canonicalize——
/// 目录可能已不存在，巡检不应有副作用）。
pub(crate) fn doctor_shared_workdir_lines(
    overrides: &[(String, String)],
    session_convs: &[String],
    default_workdir: &std::path::Path,
) -> Vec<String> {
    let normalize = |p: &str| -> String {
        let t = p.trim();
        t.trim_end_matches('/').to_string()
    };
    // conv → 有效 workdir（覆盖键优先；会话行无覆盖则落 default）。
    let mut effective: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (k, v) in overrides {
        let conv = k.strip_prefix("workdir:").unwrap_or(k);
        effective.insert(conv.to_string(), normalize(v));
    }
    let default = normalize(&default_workdir.to_string_lossy());
    for conv in session_convs {
        effective
            .entry(conv.clone())
            .or_insert_with(|| default.clone());
    }
    // 路径 → 会话数；≥2 的组逐组提示（组大小降序、至多 3 组防刷屏）。
    let mut by_path: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for p in effective.values() {
        *by_path.entry(p.as_str()).or_default() += 1;
    }
    let mut shared: Vec<(&str, usize)> = by_path.into_iter().filter(|(_, n)| *n >= 2).collect();
    shared.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    if shared.is_empty() {
        return vec!["✅ 工作区无共享（无多会话指向同一目录）".into()];
    }
    let mut out: Vec<String> = shared
        .iter()
        .take(3)
        .map(|(path, n)| {
            format!(
                "⚠️ {n} 个会话共享工作区 {path}（协作模型：成员互见产物；如需隔离用 /cd 或 /ws）"
            )
        })
        .collect();
    if shared.len() > 3 {
        out.push(format!("ℹ️ 另有 {} 组共享工作区", shared.len() - 3));
    }
    out
}

/// 检查 ④：权限 × 能力错配（复用 T4 判定函数与文案——doctor 把启动 / SIGHUP
/// 时点的 warn 矩阵变为随时可查）：
/// 1. allowed_tools 非全量 × 后端不支持逐工具白名单（`allowlist_divergence_notice`）；
/// 2. permission_mode = allow/deny × 非 FullLoop 后端（`perm_mode_dead_notice`）。
pub(crate) fn doctor_capability_lines(
    tools: &[String],
    mode: PermissionMode,
    backend_name: &str,
    supports_allowlist: bool,
    capability: crate::PermissionCapability,
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(notice) = allowlist_divergence_notice(tools, backend_name, supports_allowlist) {
        out.push(format!("⚠️ {notice}"));
    }
    if let Some(notice) = perm_mode_dead_notice(mode, backend_name, capability) {
        out.push(format!("⚠️ {notice}"));
    }
    if out.is_empty() {
        out.push(format!(
            "✅ 权限 × 能力匹配（工具白名单与权限档位在后端 {backend_name} 均有执行点）"
        ));
    }
    out
}

/// 检查 ⑤：护栏水位。`max_concurrent_rounds = 0`（不限制）= ⚠️（多群/cron/
/// webhook 齐点时在飞轮数无界——内存与 API 配额同炸）；auto-compact 生效阈值
/// 0（比例/绝对双档全关）= ℹ️ 信息行（v1.27 起默认关闭，非风险项）。
pub(crate) fn doctor_guardrail_lines(
    max_concurrent_rounds: usize,
    auto_compact_threshold: u64,
) -> Vec<String> {
    vec![
        if max_concurrent_rounds == 0 {
            "⚠️ 全局并发护栏未设上限（max_concurrent_rounds = 0）——多群/cron/webhook 齐点时在飞轮数无界".to_string()
        } else {
            format!("✅ 全局并发护栏：{max_concurrent_rounds} 轮上限")
        },
        if auto_compact_threshold == 0 {
            "ℹ️ 自动压缩未启用（v1.27 起默认关闭，需要者配置开启）".to_string()
        } else {
            format!("✅ 自动压缩已启用（阈值 {auto_compact_threshold} tok）")
        },
    ]
}

/// 检查 ⑥：体积信息行（DB 主文件 + 媒体目录——非风险项，容量水位可观测；
/// 媒体目录有 7 天 TTL 清理，见 main 的 sweep 循环）。
pub(crate) fn doctor_size_line(db_bytes: u64, media_bytes: u64) -> String {
    format!(
        "ℹ️ 体积：DB {} · 媒体 {}",
        format_bytes(db_bytes),
        format_bytes(media_bytes)
    )
}

impl Dispatcher {
    /// /status —— 本会话 + 全局运行状态。
    /// P3-a（code-review v14）：`sender` 用于 admin 判定——在飞任务的 prompt
    /// 摘要（digest）是跨会话信息泄露面（群 A 里能看到群 B 正在跑什么指令，
    /// 可能含他群的敏感 prompt），非 admin 只见**本 conv** 的 digest 明细 +
    /// 全局在飞计数；admin（运维需要全局视野）保留完整明细。
    pub(super) async fn cmd_status(&self, conv: &ConvId, sender: &str, hint: &ReplyHint) {
        // P4-7：本会话 + 全局运行状态。T18：在飞轮次与排队队列同在 ConvState
        // 单表（原 running/queues 两表），一次锁内取齐三份数据（原子快照，
        // 输出与旧版一致）。
        let admin = self.is_admin(sender);
        let (running_here, queued_here, in_flight, running_detail) = {
            let states = self.conv_states.lock().await;
            let running_here = states.get(&conv.0).is_some_and(|cs| cs.running.is_some());
            let queued_here = states
                .get(&conv.0)
                .and_then(|cs| cs.queue.as_ref())
                .map_or(0, Vec::len);
            let running_all = states
                .iter()
                .filter_map(|(rc, cs)| cs.running.as_ref().map(|h| (rc, h)))
                .collect::<Vec<_>>();
            // P3-a：全局计数对所有人可见（生存/容量水位，无内容）；digest 明细
            // 非 admin 只保留本 conv 条目。
            let in_flight = running_all.len();
            let running_detail = running_all
                .into_iter()
                .filter(|(rc, _)| admin || rc.as_str() == conv.0.as_str())
                .map(|(rc, h)| {
                    let secs = h.started.elapsed().as_secs();
                    let run = if secs < 60 {
                        format!("{secs}s")
                    } else {
                        format!("{}m{}s", secs / 60, secs % 60)
                    };
                    let digest = h
                        .digest
                        .as_deref()
                        .filter(|d| !d.is_empty())
                        .map(|d| super::super::truncate_str(d, 30))
                        .unwrap_or_else(|| "（任务）".into());
                    let mark = if rc.as_str() == conv.0.as_str() {
                        "（本会话）"
                    } else {
                        ""
                    };
                    format!("\n- {digest} · 已跑 {run}{mark}")
                })
                .collect::<Vec<_>>();
            (running_here, queued_here, in_flight, running_detail)
        };
        // P2（code-review v13）：全局并发护栏——「在飞轮次 X/Y（上限）」；上限 0
        //（不限制）显示 X（无上限）。取实时在飞数而非 gauge（running 表同源）。
        let gate_limit = self.round_gate.read().limit;
        let in_flight_line = if gate_limit == 0 {
            format!("在飞轮次 {in_flight}（无上限）")
        } else {
            format!("在飞轮次 {in_flight}/{gate_limit}")
        };
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
                let threshold = self
                    .auto_compact_threshold
                    .load(std::sync::atomic::Ordering::Relaxed);
                if threshold > 0 {
                    let pct = tokens * 100 / threshold.max(1);
                    format!("\n- 🧠 上下文：{tokens} tok（会话累计，阈值 {threshold}，{pct}%）")
                } else {
                    format!("\n- 🧠 上下文：{tokens} tok（会话累计，自动压缩已关闭）")
                }
            }),
            _ => None,
        }
        .unwrap_or_default();
        let text = format!(
                            "📊 当前状态\n- 🤖 后端：{}（{}）\n- 💬 本会话：{}，排队 {} 条\n- 🔗 会话：{sess_desc}{ctx_line}\n- 📁 工作目录：{}\n- 🏃 全局：{in_flight_line}{}\n- ⏱️ 运行时长：{}",
                            self.backend.name(),
                            self.platform.name(),
                            if running_here { "任务在跑" } else { "无任务" },
                            queued_here,
                            wd.display(),
                            running_detail.join(""),
                            format_uptime(self.started_at.elapsed()),
                        );
        self.reply(conv, &text, hint).await;
    }

    /// /tasks —— 轮次内部进度面板（T11，v13 #4）：本 conv 在飞轮次的 checklist
    /// 进度（▓ 进度条 + 逐项 ✅/⏳/◌，格式复用卡片侧 checklist）+ 工具统计。
    /// /status 只有轮级摘要（任务 + 时长），轮次内部进度此前仅卡片平台 checklist
    /// 可见——纯文本平台（wecom/ilink）与「不想翻卡片」场景由此获得查看入口。
    /// 白名单即可用（非 admin）：查看型只读无副作用，群 conv 任何人可看。
    pub(super) async fn cmd_tasks(&self, conv: &ConvId, hint: &ReplyHint) {
        // 取句柄即 clone、锁外渲染（v1.18 纪律：不持 ConvState 表锁跨 reply 的
        // await；T18 起 running 活在单表）。
        let handle = self
            .peek_conv(&conv.0, |cs| cs.and_then(|c| c.running.clone()))
            .await;
        let Some(h) = handle else {
            // 文案对齐 /stop 无任务时的口径。
            self.reply(conv, "ℹ️ 当前没有运行中的任务", hint).await;
            return;
        };
        // 快照读（std Mutex 短临界区，不跨 await；clone 后立即放锁）。
        let snap = h.snapshot.lock().unwrap().clone();
        let mut out = String::from("📋 任务面板");
        if let Some(d) = h.digest.as_deref().filter(|d| !d.is_empty()) {
            out.push_str(&format!("：{}", truncate_str(d.trim(), 40)));
        }
        out.push_str(&format!("\n⏱️ 已跑 {}", format_uptime(h.started.elapsed())));
        // checklist：与卡片侧 todo_list_md 同款 10 段 ▓ 进度条 + 逐项状态行
        //（文本面板改用 ✅/⏳/◌ 图标承载 markdown checkbox 语义）。
        if !snap.todos.is_empty() {
            let total = snap.todos.len();
            let done = snap
                .todos
                .iter()
                .filter(|t| t.status == crate::types::TodoStatus::Completed)
                .count();
            let filled = ((done * 10) + total / 2) / total;
            out.push_str(&format!(
                "\n📋 计划 {}{} {}/{}",
                "▓".repeat(filled.min(10)),
                "░".repeat(10 - filled.min(10)),
                done,
                total
            ));
            for t in &snap.todos {
                let icon = match t.status {
                    crate::types::TodoStatus::Completed => "✅",
                    crate::types::TodoStatus::InProgress => "⏳",
                    crate::types::TodoStatus::Pending => "◌",
                };
                out.push_str(&format!("\n{icon} {}", truncate_str(t.text.trim(), 60)));
            }
        }
        out.push_str(&format!("\n🔧 工具 {} 次", snap.tool_calls));
        if let Some(last) = snap.last_tool.as_deref() {
            out.push_str(&format!("，最近：{last}"));
        }
        self.reply(conv, &out, hint).await;
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
        // T16：SQLite 版本可观测——bundled SQLite 的 CVE 不进 cargo-audit，
        // 部署侧从 /doctor 看实际内嵌版本（编译期常量 = bundled 运行时版本）。
        lines.push(format!(
            "ℹ️ SQLite {}（bundled）",
            imagent_store::sqlite_version()
        ));
        let n_sess = self.store.count_sessions().await.unwrap_or(-1);
        if n_sess >= 0 {
            lines.push(format!("✅ 会话映射：{n_sess} 条"));
        } else {
            lines.push("⚠️ 会话映射计数失败".into());
        }
        let in_flight = {
            let states = self.conv_states.lock().await;
            states.values().filter(|cs| cs.running.is_some()).count()
        };
        lines.push(if in_flight == 0 {
            "✅ 无在飞任务".to_string()
        } else {
            format!("ℹ️ 在飞任务 {in_flight} 个（/stop 可中断）")
        });
        // v1.26：平台侧 API 权限探测（token/bot 能力/消息读 + 功能对照表）。
        for l in self.platform.doctor_probes().await {
            lines.push(l);
        }
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
        // T8（v13 安全批）：追加「🛡️ 安全」分组——把「只有读了 SECURITY.md
        // 才知道」的部署风险（凭据明文/入口暴露/共享工作区/权限×能力错配/
        // 护栏水位/体积）一条命令可见。单项查询失败只降级为该行告警，不阻断
        // 其余检查。
        lines.push("🛡️ 安全：".into());
        lines.extend(self.doctor_security_lines().await);
        let text = format!("🩺 自检结果：\n{}", lines.join("\n"));
        self.reply(conv, &text, hint).await;
    }

    /// T8：/doctor 安全分组的数据采集与组装。六项检查的判定各自在独立纯函数
    /// 里（见上），此处只负责取数与容错。
    async fn doctor_security_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        // ① 凭据形态（store 只读聚合，不触发懒迁移/不接触 keychain）。
        match self.store.credential_forms().await {
            Ok(forms) => out.push(doctor_credential_line(&forms)),
            Err(e) => out.push(format!("⚠️ 凭据形态查询失败：{e}")),
        }
        // ② webhook 暴露面（main 装配时注入的摘要快照）。
        out.push(doctor_webhook_line(&self.webhook_exposure.read()));
        // ③ 共享工作区：workdir 覆盖键 ∪ sessions 会话枚举（并发取数）。
        let (overrides, convs) = tokio::join!(
            self.store.list_config("workdir:"),
            self.store.list_session_convs()
        );
        match (overrides, convs) {
            (Ok(o), Ok(c)) => {
                out.extend(doctor_shared_workdir_lines(&o, &c, &self.default_workdir))
            }
            (Err(e), _) | (_, Err(e)) => out.push(format!("⚠️ 工作区共享检查查询失败：{e}")),
        }
        // ④ 权限 × 能力错配（复用 T4 判定函数；读当前热载态）。
        out.extend(doctor_capability_lines(
            &self.allowed_tools.read(),
            *self.permission_mode.read(),
            self.backend.name(),
            self.backend.supports_tool_allowlist(),
            self.backend.permission_capability(),
        ));
        // ⑤ 护栏水位（round_gate 当前上限 + 自动压缩生效阈值）。
        out.extend(doctor_guardrail_lines(
            self.round_gate.read().limit,
            self.auto_compact_threshold
                .load(std::sync::atomic::Ordering::Relaxed),
        ));
        // ⑥ 体积信息行（DB 主文件经 PRAGMA；媒体目录平铺求和）。
        let db_bytes = self.store.db_size_bytes().await.unwrap_or(0);
        let media_bytes = crate::paths::dir_size_bytes(&crate::paths::imagent_home().join("media"));
        out.push(doctor_size_line(db_bytes, media_bytes));
        out
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
                // D7：缓存按 (conv, sender) 隔离——本 conv 全部 sender 一并失效
                //（T18 并入 ConvState 后 = 清空本 conv 字段）。
                self.with_conv(&conv.0, |cs| cs.resume_cache.clear()).await;
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
                                // D7：本 conv 全部 sender 一并失效（同 /cd）。
                                self.with_conv(&conv.0, |cs| cs.resume_cache.clear()).await;
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
            let cur_override = self
                .peek_conv(&conv.0, |cs| cs.and_then(|c| c.idle_override))
                .await;
            let cur = match cur_override {
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
                self.with_conv(&conv.0, |cs| cs.idle_override = Some(Duration::ZERO))
                    .await;
                self.reply(conv, "✅ 本会话空闲看门狗已关闭（仅本会话）", hint)
                    .await;
            }
            "default" => {
                self.with_conv(&conv.0, |cs| cs.idle_override = None).await;
                self.reply(
                    conv,
                    &format!("✅ 已清除本会话覆盖，回到全局 {global} 秒"),
                    hint,
                )
                .await;
            }
            _ => match arg.parse::<u64>() {
                Ok(n) if n > 0 => {
                    // L5（code-review v8）：30 天上限（43200 分钟）防误设永关。
                    //（v9-R14：原 checked_mul(1) 为乘 1 no-op、注释宣称防溢出
                    // 误导——n 由 u64 parse 直接而来无运算，上限过滤即全部防护。）
                    const TIMEOUT_MAX_MINUTES: u64 = 30 * 24 * 60;
                    let Some(n) = Some(n).filter(|n| *n <= TIMEOUT_MAX_MINUTES) else {
                        self.reply(
                            conv,
                            &format!("❌ 分钟数需 ≤ {TIMEOUT_MAX_MINUTES}（30 天）"),
                            hint,
                        )
                        .await;
                        return;
                    };
                    let d = Duration::from_secs(n * 60);
                    self.with_conv(&conv.0, |cs| cs.idle_override = Some(d))
                        .await;
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
    /// 仅支持模型选择的后端可用（claude-cli `--model` / claude-acp env /
    /// codex `-m` / gemini `-m`，v1.21 起全覆盖）；
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
            sender_name: None,
            text: Some(prompt),
            media: Vec::new(),
            media_errors: Vec::new(),
            mentions: Vec::new(),
            mentioned_bot: false,
            ask_req: None,
            reply_to: None,
            source_msg_id: None,
            control: None,
            // /retry 是用户显式动作，保留 steering 语义（运行中注入当轮追问）。
            no_steer: false,
            reply_hint: hint.clone(),
        };
        self.dispatch_agent_message(msg).await;
    }

    /// /again —— 重跑最近一次**成功**轮的指令（v1.23 指令复用；与 /retry 的
    /// 失败轮重试对称）。数据源 `last_success_prompt:<conv>`（成功轮落库，
    /// 与失败轮的 last_prompt 分键）。巡检/日报类「隔天再跑一次」从此不必
    /// 手打全文。
    pub(super) async fn cmd_again(
        &self,
        conv: &ConvId,
        sender: &crate::types::UserId,
        hint: &ReplyHint,
    ) {
        let prompt = match self
            .store
            .get_config(&format!("last_success_prompt:{}", conv.0))
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
                "本会话还没有成功完成的指令可复用（先跑完一轮；失败轮的重试用 /retry）。",
                hint,
            )
            .await;
            return;
        };
        let digest = truncate_str(&prompt, 60);
        let msg = InboundMessage {
            conv_id: conv.clone(),
            sender: sender.clone(),
            sender_name: None,
            text: Some(prompt),
            media: Vec::new(),
            media_errors: Vec::new(),
            mentions: Vec::new(),
            mentioned_bot: false,
            ask_req: None,
            reply_to: None,
            source_msg_id: None,
            control: None,
            no_steer: false,
            reply_hint: hint.clone(),
        };
        self.reply(conv, &format!("🔁 再跑一次：{digest}"), hint)
            .await;
        self.dispatch_agent_message(msg).await;
    }

    /// /export —— 当前会话导出为 Markdown 文件回传（W4-2）。走 backend 的本机
    /// 会话存储转录（claude 系支持；codex/gemini 回不支持提示）。导出文件经
    /// send_media 发送后即删（media 目录，0600）。
    pub(super) async fn cmd_export(
        &self,
        conv: &ConvId,
        sender: &crate::types::UserId,
        hint: &ReplyHint,
        parts: &[&str],
    ) {
        // v1.23：/export [n]——带序号时导 /resume 列表里的历史会话（此前只能
        // 导当前活动会话，而用户最想找回结论的恰是非活动历史会话）。序号取
        // resume_cache（与 /resume <n> 同源，防列表漂移错位）。
        let (sid_for_export, sid_note) = match parts
            .get(1)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            Some(n) => {
                let Ok(idx) = n.parse::<usize>() else {
                    self.reply(conv, "⚠️ 用法：/export [序号]（序号 = /resume 列表编号；无参 = 导出当前活动会话）。", hint).await;
                    return;
                };
                let cached = {
                    let key = sender.0.clone();
                    self.peek_conv(&conv.0, move |cs| {
                        cs.and_then(|c| c.resume_cache.get(&key).cloned())
                    })
                    .await
                };
                let Some((at, list)) = cached else {
                    self.reply(conv, "⚠️ 没有可用的 /resume 列表——先发 /resume 查看历史会话，再 /export <序号>。", hint).await;
                    return;
                };
                // v13 P3 还债批：TTL 不再本地重复定义——用 dispatch 模块的单一
                // 来源 RESUME_CACHE_TTL（与 /resume <n> 的过期判定同值，防两处
                // 漂移后 /export 与 /resume 对「过期」判断不一致）。
                if at.elapsed() > RESUME_CACHE_TTL {
                    self.reply(
                        conv,
                        "⚠️ /resume 列表已过期（超过 10 分钟）——重新 /resume 后再 /export <序号>。",
                        hint,
                    )
                    .await;
                    return;
                }
                let Some(entry) = list.get(idx.wrapping_sub(1)).filter(|_| idx >= 1) else {
                    self.reply(
                        conv,
                        &format!("⚠️ 序号超出范围（列表共 {} 条）。", list.len()),
                        hint,
                    )
                    .await;
                    return;
                };
                let sid8: String = entry.session_id.chars().take(8).collect();
                (
                    entry.session_id.clone(),
                    format!("（/resume #{n} · {sid8}…）"),
                )
            }
            None => {
                let Some(row) = self.store.get_session(&conv.0).await.ok().flatten() else {
                    self.reply(
                        conv,
                        "当前无活动会话可导出（先发一条消息开启会话；/resume 列历史后 /export <序号> 导任意会话）。",
                        hint,
                    )
                    .await;
                    return;
                };
                (row.session_id, String::new())
            }
        };
        let wd = self.resolve_workdir(&conv.0).await;
        let Some(md) = self
            .backend
            .export_session_markdown(&wd, &sid_for_export)
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
        let sid8: String = sid_for_export.chars().take(8).collect();
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
                self.reply(
                    conv,
                    &format!("✅ 已导出会话 {sid8}…{sid_note}（{fname}）"),
                    hint,
                )
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
        // drop 子命令：先做权限与序号校验，再在 ConvState 同一临界区移除并收缩
        // 排队提示（count 变化需反映到卡片 footer）。T18：队列与 hint 同表，
        // 校验+移除+hint 收缩一锁完成；回执与删行 IO 在锁外（reply 含平台
        // 发送——超时 + 退避重试，持全局表锁 await 会卡住所有 conv 的
        // 入队/取批/steering）。
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
            // 锁内校验 + 移除的结果（回执/删行 IO 全在锁外分支）。
            enum DropOutcome {
                NoQueue,
                OutOfRange(usize),
                NotYours,
                Dropped {
                    sender: String,
                    snippet: String,
                    rowid: i64,
                },
            }
            let outcome = {
                let mut states = self.conv_states.lock().await;
                match states.get_mut(&conv.0) {
                    None => DropOutcome::NoQueue,
                    Some(cs) => match cs.queue.as_mut() {
                        None => DropOutcome::NoQueue,
                        Some(q) => {
                            if n > q.len() {
                                DropOutcome::OutOfRange(q.len())
                            } else {
                                let idx = n - 1;
                                let removed_sender = q[idx].msg.sender.0.clone();
                                if removed_sender != sender && !is_admin {
                                    DropOutcome::NotYours
                                } else {
                                    let removed = q.remove(idx);
                                    let count = q.len();
                                    let latest = q
                                        .last()
                                        .map(|last| super::super::latest_snippet(&last.msg));
                                    // q 的借用到此为止（不相交字段路径，NLL 放行）。
                                    // 与取批路径同语义：留空 Vec 不交还 runner 身份
                                    //（runner 循环依赖）。hint：count==0 清除、否则
                                    // 合并写入保 steered（同 enqueue 路径的 v1.18
                                    // review 修正：整体替换会把「已注入 N 条」清零）。
                                    if count == 0 {
                                        cs.queued_hint = None;
                                    } else if let Some(latest) = latest {
                                        let h = cs.queued_hint.get_or_insert_default();
                                        h.count = count;
                                        h.latest = latest;
                                    }
                                    let snippet = removed
                                        .msg
                                        .text
                                        .as_deref()
                                        .map(|t| super::super::truncate_str(t.trim(), 30))
                                        .unwrap_or_else(|| "（纯媒体）".into());
                                    DropOutcome::Dropped {
                                        sender: removed_sender,
                                        snippet,
                                        rowid: removed.rowid,
                                    }
                                }
                            }
                        }
                    },
                }
            };
            match outcome {
                DropOutcome::NoQueue => {
                    self.reply(conv, "队列为空。", hint).await;
                }
                DropOutcome::OutOfRange(len) => {
                    self.reply(conv, &format!("序号超出范围（当前 {len} 条）。"), hint)
                        .await;
                }
                DropOutcome::NotYours => {
                    self.reply(conv, "只能丢弃自己排队的消息（或联系管理员处理）。", hint)
                        .await;
                }
                DropOutcome::Dropped {
                    sender: removed_sender,
                    snippet,
                    rowid,
                } => {
                    // v1.18 review（排队持久化重做）：按被丢元素的 rowid 精确删行
                    //（替代整队重写——并发入队的新行不受影响）。
                    if rowid > 0 {
                        if let Err(e) = self.store.delete_queued_rows(&[rowid]).await {
                            warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "丢弃后持久化行删除失败（重启后可能重放该条）");
                        }
                    }
                    self.reply(
                        conv,
                        &format!("🗑️ 已丢弃第 {n} 条（{removed_sender}）：{snippet}"),
                        hint,
                    )
                    .await;
                }
            }
            return;
        }
        // 列表视图。
        let list: Vec<(String, String)> = {
            let states = self.conv_states.lock().await;
            match states.get(&conv.0).and_then(|cs| cs.queue.as_ref()) {
                None => Vec::new(),
                Some(q) => q
                    .iter()
                    .map(|q| {
                        let m = &q.msg;
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
        // v1.25 卡片化：表格 + 前 9 条各带「丢弃」按钮（配对行布局；权限
        // 校验在 drop 命令层不变）。发送者用展示名（v1.23 sender_name）回退
        // 短 id。
        let mut body = String::from("| # | 发送者 | 内容 |\n|---|---|---|");
        for (i, (s, snippet)) in list.iter().enumerate() {
            let who = s.rsplit_once('_').map(|(_, t)| t).unwrap_or(s.as_str());
            let who: String = who.chars().take(8).collect();
            body.push_str(&format!(
                "\n| {} | {}… | {} |",
                i + 1,
                who,
                snippet.replace('|', "\\|")
            ));
        }
        let buttons: Vec<crate::types::CardButton> = (1..=list.len().min(9))
            .map(|n| crate::types::CardButton {
                label: format!("丢弃 {n}"),
                command: format!("/queue drop {n}"),
                style: crate::types::CardButtonStyle::Danger,
            })
            .collect();
        self.reply_card(conv, "📋 排队中的消息", &body, buttons, hint)
            .await;
    }

    /// /mcp —— 用户 MCP servers 热管理（v1.26 能力批）：list / add <名> <url> /
    /// rm <名>。store 持久化 + claude backend 内存镜像热更，下一轮 spawn 生效
    ///（write_mcp_config 现读）。admin 门槛：加 server = 给 agent 扩工具面。
    /// 形态：URL 型（streamable http / sse）。保留名 imagent 被拒（审批闭环专用）。
    pub(super) async fn cmd_mcp(
        &self,
        conv: &ConvId,
        sender: &str,
        hint: &ReplyHint,
        parts: &[&str],
    ) {
        if !self.is_admin(sender) {
            let msg = self.admin_denied_reply("管理 MCP servers");
            self.reply(conv, &msg, hint).await;
            return;
        }
        let sub = parts.get(1).map(|s| s.trim()).unwrap_or("");
        match sub {
            "list" | "" => {
                let rows = self
                    .store
                    .list_config("mcp_server:")
                    .await
                    .unwrap_or_default();
                if rows.is_empty() {
                    self.reply(conv, "📭 未配置 MCP servers（/mcp add <名> <url>）。config.toml 的 mcp_config_path 文件源不受影响、照常合并。", hint).await;
                    return;
                }
                let mut body = String::from("| 名 | URL |\n|---|---|");
                for (k, v) in &rows {
                    let name = k.strip_prefix("mcp_server:").unwrap_or(k);
                    let url = v.trim().chars().take(60).collect::<String>();
                    body.push_str(&format!("\n| {name} | {url} |"));
                }
                let buttons = rows
                    .iter()
                    .filter_map(|(k, _)| k.strip_prefix("mcp_server:"))
                    .take(9)
                    .map(|n| crate::types::CardButton {
                        label: format!("移除 {n}"),
                        command: format!("/mcp rm {n}"),
                        style: crate::types::CardButtonStyle::Danger,
                    })
                    .collect::<Vec<_>>();
                self.reply_card(conv, "🔌 MCP servers", &body, buttons, hint)
                    .await;
            }
            "add" => {
                let (Some(name), Some(url)) = (
                    parts.get(2).map(|s| s.trim()),
                    parts.get(3).map(|s| s.trim()),
                ) else {
                    self.reply(
                        conv,
                        "用法：/mcp add <名> <url>（URL 型 server，streamable http / sse）",
                        hint,
                    )
                    .await;
                    return;
                };
                if name.is_empty() || url.is_empty() || !url.starts_with("http") {
                    self.reply(conv, "⚠️ 名与 URL 必填，URL 须 http(s):// 开头。", hint)
                        .await;
                    return;
                }
                if name == "imagent" {
                    self.reply(conv, "⛔ `imagent` 是审批闭环保留名。", hint)
                        .await;
                    return;
                }
                if let Err(e) = self
                    .store
                    .set_config(&format!("mcp_server:{name}"), url)
                    .await
                {
                    self.reply(conv, &format!("⚠️ 保存失败：{e}"), hint).await;
                    return;
                }
                if let Err(e) = self.sync_mcp_to_backend().await {
                    warn!(target: "imagent::core", error = %e, "MCP 热更同步 backend 失败（下轮不生效）");
                }
                self.reply(
                    conv,
                    &format!("✅ 已添加 MCP server `{name}`，下一轮任务生效。agent 侧工具名形如 `mcp__{name}__<tool>`。"),
                    hint,
                )
                .await;
            }
            "rm" => {
                let Some(name) = parts.get(2).map(|s| s.trim()).filter(|n| !n.is_empty()) else {
                    self.reply(conv, "用法：/mcp rm <名>", hint).await;
                    return;
                };
                let key = format!("mcp_server:{name}");
                let exists = self
                    .store
                    .get_config(&key)
                    .await
                    .map(|v| v.is_some())
                    .unwrap_or(false);
                if !exists {
                    self.reply(conv, &format!("⚠️ `{name}` 不存在。"), hint)
                        .await;
                    return;
                }
                if let Err(e) = self.store.delete_config(&key).await {
                    self.reply(conv, &format!("⚠️ 删除失败：{e}"), hint).await;
                    return;
                }
                if let Err(e) = self.sync_mcp_to_backend().await {
                    warn!(target: "imagent::core", error = %e, "MCP 热更同步 backend 失败（下轮不生效）");
                }
                self.reply(
                    conv,
                    &format!("🗑️ 已移除 MCP server `{name}`（已起的轮次不受影响，下一轮生效）。"),
                    hint,
                )
                .await;
            }
            _ => {
                self.reply(
                    conv,
                    "用法：/mcp list · /mcp add <名> <url> · /mcp rm <名>",
                    hint,
                )
                .await
            }
        }
    }

    /// store 的 MCP servers → backend 热更（Backend::set_user_mcp_servers，
    /// claude 实现合并进 extra_mcp；其余后端默认 no-op）。
    async fn sync_mcp_to_backend(&self) -> anyhow::Result<()> {
        let rows = self.store.list_config("mcp_server:").await?;
        let mut servers = serde_json::Map::new();
        for (k, v) in rows {
            let name = k.strip_prefix("mcp_server:").unwrap_or(&k).to_string();
            servers.insert(name, serde_json::json!({ "url": v.trim() }));
        }
        self.backend
            .set_user_mcp_servers(serde_json::json!({ "mcpServers": servers }));
        Ok(())
    }

    /// /last —— 回看本会话最近一次成功轮（v1.25）：任务摘要 + 结论 + 耗时/
    /// 成本。长会话翻旧结论不再滚屏或全量 /export。
    pub(super) async fn cmd_last(&self, conv: &ConvId, hint: &ReplyHint) {
        let row = self
            .store
            .get_config(&format!("last_round:{}", conv.0))
            .await
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
        let Some(v) = row else {
            self.reply(
                conv,
                "本会话还没有可回看的完成轮（成功跑完一轮后可 /last 回看）。",
                hint,
            )
            .await;
            return;
        };
        let gets = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let prompt = gets("prompt");
        let head = gets("head");
        let secs = v.get("secs").and_then(|x| x.as_u64()).unwrap_or(0);
        let usage = gets("usage");
        let at = v.get("at").and_then(|x| x.as_i64()).unwrap_or(0);
        let mut body = format!(
            "**🎯 任务**\n{}\n\n**✅ 结论**\n{}",
            super::super::truncate_str(prompt.trim(), 80),
            super::super::truncate_str(head.trim(), 400)
        );
        let run = if secs < 60 {
            format!("{secs}s")
        } else {
            format!("{}m{}s", secs / 60, secs % 60)
        };
        body.push_str(&format!("\n\n⏱ {} · {}", run, super::format_rel_ts(at)));
        if !usage.is_empty() {
            body.push_str(&format!(" · {usage}"));
        }
        let buttons = vec![
            crate::types::CardButton {
                label: "🔁 再跑一次".into(),
                command: "/again".into(),
                style: crate::types::CardButtonStyle::Primary,
            },
            crate::types::CardButton {
                label: "📄 导出会话".into(),
                command: "/export".into(),
                style: crate::types::CardButtonStyle::Default,
            },
        ];
        self.reply_card(conv, "🕘 上一轮", &body, buttons, hint)
            .await;
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
        // v1.24 卡片 UX：四列表格在手机窄屏挤压严重——卡片平台与文本平台
        // 统一为逐条列表（时间 · 动作 · 操作者一行 + 摘要随行）。
        let _ = &text;
        self.reply_card(conv, "📋 审计日志", &text, vec![], hint)
            .await;
    }

    /// /help —— 命令总表（P6-3：飞书等卡片平台带常用命令按钮）。
    pub(super) async fn cmd_help(&self, conv: &ConvId, hint: &ReplyHint) {
        let mut body = "其他内容直接发给 agent 即可（运行中发文字实时转入当轮 👀；图片/文件排队下一轮 ⏳）。\n\n💬 会话规则：群主时间线直接 @我 = 续同一会话；点消息「回复」进话题 = 开独立会话。".to_string();
        // v1.25 折叠分组：卡片平台用 :::collapse 围栏（常用组展开、其余
        // 收起——30+ 命令一屏放不下）；纯文本平台用平铺段（围栏语法由
        // core 按平台选择，文本侧不见）。
        let groups = if self.platform.supports_streaming_card(conv) {
            [
                (":::collapse+ 🚀 常用", "- /last 回看上一轮 · /again 再跑成功指令\n- /resume 恢复历史 · /sessions 切换会话\n- /stop 中断 · /queue 查看排队\n- /help 本表"),
                (":::collapse 🗂 会话管理", "- /new 重置 · /switch <name> 切换/新建命名 · /compact 压缩上下文\n- /retry 重试失败轮 · /export [n] 导出 Markdown"),
                (":::collapse 📁 目录与文件", "- /cd <path> 切工作目录 · /ws save|use|remove <name> 命名空间\n- /img <path> 发图片 · /file <path> 发文件"),
                (":::collapse 🛡️ 权限与运行", "- /perm <off|allow|deny|ask> 模式 · /perm list 查看 · /perm revoke <工具> 撤销\n- /timeout <分钟|off|default> 空闲看门狗 · /model [名称|default] 模型"),
                (":::collapse ⏰ 定时任务", "- /cron add <分 时 日 月 周> <指令>（本地时区）\n- /cron list · rm <id> · enable|disable <id>"),
                (":::collapse 🧪 状态与诊断", "- /status 状态 · /tasks 轮次进度 · /doctor 自检 · /reconnect 重连\n- /config [k v] 热改 · /stats [today|7d|all] 用量 · /audit [n] 审计"),
                (":::collapse 👥 管理（管理员）", "- /allow、/disallow 授权（群内可 @ 对方）· /chat allow|deny|allow-all|list\n- /admin list|add|remove · /list 白名单 · /whoami 我的 id"),
            ]
            .iter()
            .map(|(h, c)| format!("{h}\n{c}\n:::"))
            .collect::<Vec<_>>()
            .join("\n\n")
        } else {
            "🗂 会话\n- /new 重置 · /switch 切换 · /sessions · /resume [n] · /compact · /retry · /again · /last · /export [n]\n\n📁 目录与文件\n- /cd · /ws save|use|remove · /img · /file\n\n🛡️ 权限与运行\n- /perm <off|allow|deny|ask> · /perm list · /perm revoke <工具>\n- /stop · /queue [drop n] · /timeout · /model\n\n⏰ 定时\n- /cron add <分 时 日 月 周> <指令> · list · rm · enable|disable\n\n🧪 诊断\n- /status · /tasks 轮次进度 · /doctor · /reconnect · /config · /stats · /audit\n\n👥 管理（管理员）\n- /allow · /disallow · /chat · /admin · /list · /whoami".to_string()
        };
        body = format!("{groups}\n\n{body}");
        // v1.23：动态追加 shortcuts 段——快捷命令此前零发现性（忘了名字就
        // 永久失联，只能翻 config.toml）。
        {
            let sc = self
                .shortcuts
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if !sc.is_empty() {
                let mut names: Vec<&String> = sc.keys().collect();
                names.sort();
                let lines: Vec<String> = names
                    .iter()
                    .map(|n| {
                        let tpl = sc[*n].split_whitespace().collect::<Vec<_>>().join(" ");
                        format!("- /{n} → {}", truncate_str(&tpl, 40))
                    })
                    .collect();
                body.push_str(&format!(
                    "\n\n⌨️ 自定义快捷命令\n{}\n（发送 /名称 [参数]；模板里 $args 为参数占位）",
                    lines.join("\n")
                ));
            }
        }
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
        self.reply_card(conv, "🤖 imagent 命令", &body, buttons, hint)
            .await;
    }
}
