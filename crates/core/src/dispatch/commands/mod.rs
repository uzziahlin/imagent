//! IM 斜杠命令：handle() 鉴权 + 命令分派（命令实现按主题拆在子模块）。
//!
//! [`admin`]：白名单/权限/配置（管理操作）；[`session`]：会话生命周期；
//! [`misc`]：状态/工作目录/媒体/帮助。

mod admin;
mod cron;
mod misc;
mod session;

use super::*;

/// T8（v13 安全批）：/doctor 安全自检的六项检查（misc 实现，`pub(crate)`
/// 定位）——re-export 到 commands 层供 `dispatch::tests` 单测（仅测试构建
/// 使用，cfg(test) 门控防非测试构建的 unused-import 告警）。
#[cfg(test)]
pub(super) use misc::{
    doctor_capability_lines, doctor_credential_line, doctor_guardrail_lines,
    doctor_platform_caps_lines, doctor_shared_workdir_lines, doctor_size_line, doctor_webhook_line,
};

/// P3（v13 遗留「群内斜杠免检噪音」）：命令分派表（[`Dispatcher::handle`] 的
/// match）的**全部**命令词（小写、含 `/` 前缀）。feishu 平台层的群 mention 门
/// 用它做「已知命令」豁免判定——只放行真实命令，`/xxx 是什么意思` 一类闲聊
/// 不再触发命令分派回未知命令提示。与 match 分派的一致性由本文件底部钉住
/// 测试看护（新命令加 match 臂而漏登记此处时测试红）。
/// 注意与 [`COMMAND_GROUPS`]（/help 展示表）职责不同：本清单以**可分派**为准
/// （含 /mcp、/export、/queue 等未入 help 分组的命令），二者不要求互相覆盖。
/// config `shortcuts` 的自定义 `/name` 不在此清单（平台解析层无 config 视野，
/// 群内使用需 @bot）。
const KNOWN_COMMAND_WORDS: &[&str] = &[
    "/admin",
    "/again",
    "/allow",
    "/audit",
    "/cd",
    "/chat",
    "/compact",
    "/config",
    "/cron",
    "/disallow",
    "/doctor",
    "/export",
    "/file",
    "/help",
    "/img",
    "/last",
    "/list",
    "/mcp",
    "/model",
    "/new",
    "/perm",
    "/queue",
    "/reconnect",
    "/resume",
    "/retry",
    "/sessions",
    "/stats",
    "/status",
    "/stop",
    "/switch",
    "/tasks",
    "/timeout",
    "/whoami",
    "/ws",
];

/// S-12：全部支持的斜杠命令，按 /help 分组同构（未知命令提示竖排分组展示）。
pub(super) const COMMAND_GROUPS: &[(&str, &[&str])] = &[
    (
        "🗂 会话",
        &[
            "/new",
            "/switch",
            "/sessions",
            "/resume",
            "/compact",
            "/retry",
            "/again",
            "/last",
        ],
    ),
    ("📁 目录与文件", &["/cd", "/ws", "/img", "/file"]),
    (
        "🛡️ 权限与运行",
        &["/perm", "/stop", "/timeout", "/model", "/cron"],
    ),
    (
        "🧪 状态与诊断",
        &[
            "/status",
            "/tasks",
            "/stats",
            "/doctor",
            "/reconnect",
            "/config",
            "/audit",
        ],
    ),
    (
        "👥 白名单与管理",
        &["/allow", "/disallow", "/chat", "/admin", "/list", "/whoami"],
    ),
    ("❓ 帮助", &["/help"]),
];

/// 编辑距离（Levenshtein；命令名都很短，朴素 DP 足够）。
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// S-12：未知命令 → 编辑距离 ≤2 的最相近已知命令（无则 None）。
pub(super) fn suggest_command(cmd: &str) -> Option<&'static str> {
    COMMAND_GROUPS
        .iter()
        .flat_map(|(_, cs)| cs.iter())
        .filter(|c| **c != cmd)
        .min_by_key(|c| edit_distance(cmd, c))
        .filter(|c| edit_distance(cmd, c) <= 2)
        .copied()
}

/// S-12：未知命令回复文案——模糊建议（有近邻时）+ 分组竖排命令表。
pub(super) fn unknown_command_reply(cmd: &str) -> String {
    let mut out = match suggest_command(cmd) {
        Some(s) => format!("未知命令 {cmd}，你是想找 {s} 吗？"),
        None => format!("未知命令 {cmd}"),
    };
    out.push_str("\n支持的命令：");
    for (group, cmds) in COMMAND_GROUPS {
        out.push_str(&format!("\n{group}"));
        for c in *cmds {
            out.push_str(&format!("\n- {c}"));
        }
    }
    out.push_str("\n完整说明见 /help");
    out
}

impl Dispatcher {
    /// 已知斜杠命令词表（[`KNOWN_COMMAND_WORDS`] 的公开出口）。关联函数形态
    /// （而非模块级 `pub fn`）：`commands` 是 dispatch 下的私有模块，模块级
    /// pub fn 无法穿透 `dispatch`/`commands` 私有模块链被平台 crate 引用；
    /// `Dispatcher` 已在 crate 根 re-export，本文件本就持有其 impl 块——零
    /// 额外导出链。feishu 的群 mention 门消费（见 proto 的 is_known_slash_command）。
    pub fn known_command_words() -> &'static [&'static str] {
        KNOWN_COMMAND_WORDS
    }

    /// 处理单条消息。内部任何错误都 log 并吞掉，不影响主循环。
    pub(super) async fn handle(&self, msg: InboundMessage) {
        let conv = msg.conv_id.clone();
        let sender = msg.sender.clone();
        let hint = msg.reply_hint.clone();

        // best-effort 指标：入站消息计数（失败只 warn 不阻断）。
        METRICS.messages_in.inc();

        // 0. 平台控制信号（消息撤回 / bot 被移出群等系统事件）：不是用户对话
        //    输入，在白名单校验之前消费——鉴权丢弃语义不适用于系统信号。
        if msg.control.is_some() {
            self.handle_control(msg).await;
            return;
        }
        // 1. 发现态：两个白名单（sender / chat）都为空。不自动授权（安全），对 sender
        //    回引导消息，告知其 sender id 与 conv id，不驱动 agent。
        if self.auth.is_discovery() {
            info!(
                target: "imagent::discovery",
                conv_id = %conv.0,
                sender = %sender.0,
                text = ?msg.text,
                "discovery 模式：记录 sender，回引导"
            );
            // S-13：引导按实际 admin 状态给——admin_senders 为空（S2：无人是
            // 管理员，IM 内管理命令全部不可用）时**不得**提示「/allow」，否则
            // 用户照做只会得到权限拒绝，与 S2 语义矛盾。
            let admin_note = if self.admin_senders.read().is_empty() {
                "目前未配置管理员（admin_senders 为空），IM 内管理类命令不可用；请先在本地运行 `imagent setup` 或编辑 config.toml 配置 admin_senders。".to_string()
            } else {
                "已配置管理员：管理员也可在 IM 内发授权命令直接放行。".to_string()
            };
            let guide = format!(
                "发现模式：当前白名单为空。你的 sender id 是 `{}`，会话 id 是 `{}`。\n\
                 请管理员在本地运行 `imagent allow {}` 授权用户、或 `imagent allow-chat {}` \
                 授权整个会话（群）后重启 imagent。\n{admin_note}",
                sender.0, conv.0, sender.0, conv.0
            );
            self.reply(&conv, &guide, &hint).await;
            return;
        }

        // 2. 白名单（P4-5）：sender 放行 OR 会话（群）放行，二者其一即过。
        //    群维度授权后无需逐个 allow 成员；命令层的授权操作仍受 admin 门槛。
        if !self.auth.is_allowed(&sender) && !self.auth.is_chat_allowed(&conv.0) {
            warn!(
                target: "imagent::core",
                conv_id = %conv.0,
                sender = %sender.0,
                "非白名单 sender 且会话未授权，丢弃"
            );
            // 私聊陌生人引导（默认开）：未放行用户的私聊回一句引导——私聊是
            // 用户主动找 bot，无探测面（与群内默认静默的 stranger_mention_hint
            // 相反，见 config 注释）；含 sender id 与 /allow 指引，首次使用者
            // 可据此完成授权。
            if *self.stranger_p2p_hint.read() && is_p2p_conv(&conv.0) {
                self.reply(
                    &conv,
                    &format!(
                        "👋 你好！我尚未对你开放使用权限。你的用户 id 是 `{}`。\n\
                         请联系管理员在 IM 内发送 `/allow {}` 放行（或在本地运行 `imagent allow {}` 后重启）。\
                         授权前我不会处理你的消息。",
                        sender.0, sender.0, sender.0
                    ),
                    &hint,
                )
                .await;
                return;
            }
            // P7-A3：可选的「陌生人被 @ 提示」——仅在开启且确实 @ 了 bot 时回
            // 一句引导（私聊/弱过滤未知为 false，保持完全静默；防探测默认关）。
            if *self.stranger_mention_hint.read() && msg.mentioned_bot {
                self.reply(
                    &conv,
                    "👋 你好！我还未在此群启用。群管理员可发送 `/chat allow` 放行本群\
                     （或私聊管理员处理）；启用前我不会响应其他消息。",
                    &hint,
                )
                .await;
            }
            return;
        }

        // 3. 斜杠命令（鉴权通过后、调 backend 前）。
        //    命令名小写比较；参数保留原样。到这里的 sender 必然已过白名单，
        //    故 /allow 的「调用者鉴权」天然由白名单保证，无需额外校验。
        //    全角斜杠容错：中文输入法易打出 ／（U+FF0F）——把**前导**全角斜杠
        //    归一成半角再判定（／help → /help、／STATUS → /STATUS 后经命令名
        //    小写比较命中 /status），一处归一覆盖所有命令与未知命令提示。
        if let Some(text) = msg.text.as_ref() {
            let trimmed = text.trim();
            let normalized: std::borrow::Cow<str> = match trimmed.strip_prefix('／') {
                Some(rest) => std::borrow::Cow::Owned(format!("/{rest}")),
                None => std::borrow::Cow::Borrowed(trimmed),
            };
            if normalized.starts_with('/') {
                let parts: Vec<&str> = normalized.split_whitespace().collect();
                let cmd = parts[0].to_ascii_lowercase();
                match cmd.as_str() {
                    "/new" => {
                        self.cmd_new(&conv, &hint).await;
                        return;
                    }
                    "/allow" => {
                        self.cmd_allow(&conv, &sender, &hint, &parts, &msg.mentions)
                            .await;
                        return;
                    }
                    "/disallow" => {
                        self.cmd_disallow(&conv, &sender, &hint, &parts, &msg.mentions)
                            .await;
                        return;
                    }
                    "/list" => {
                        self.cmd_list(&conv, &hint).await;
                        return;
                    }
                    "/whoami" => {
                        self.cmd_whoami(&conv, &sender, &hint).await;
                        return;
                    }
                    "/chat" => {
                        self.cmd_chat(&conv, &sender, &hint, &parts).await;
                        return;
                    }
                    "/admin" => {
                        self.cmd_admin(&conv, &sender, &hint, &parts, &msg.mentions)
                            .await;
                        return;
                    }
                    "/config" => {
                        self.cmd_config(&conv, &sender, &hint, &parts).await;
                        return;
                    }
                    "/status" => {
                        self.cmd_status(&conv, &sender.0, &hint).await;
                        return;
                    }
                    "/tasks" => {
                        self.cmd_tasks(&conv, &hint).await;
                        return;
                    }
                    "/cron" => {
                        self.cmd_cron(&conv, &sender, &hint, &parts).await;
                        return;
                    }
                    "/stats" => {
                        self.cmd_stats(&conv, &sender.0, &hint, &parts).await;
                        return;
                    }
                    "/audit" => {
                        self.cmd_audit(&conv, &sender.0, &hint, &parts).await;
                        return;
                    }
                    "/doctor" => {
                        self.cmd_doctor(&conv, &hint).await;
                        return;
                    }
                    "/reconnect" => {
                        self.cmd_reconnect(&conv, &hint).await;
                        return;
                    }
                    "/resume" => {
                        self.cmd_resume(&conv, &sender, &hint, &parts).await;
                        return;
                    }
                    "/switch" => {
                        self.cmd_switch(&conv, &hint, &parts).await;
                        return;
                    }
                    "/sessions" => {
                        self.cmd_sessions(&conv, &hint).await;
                        return;
                    }
                    "/compact" => {
                        self.cmd_compact(&conv, &hint).await;
                        return;
                    }
                    "/mcp" => {
                        self.cmd_mcp(&conv, &sender.0, &hint, &parts).await;
                        return;
                    }
                    "/last" => {
                        self.cmd_last(&conv, &hint).await;
                        return;
                    }
                    "/again" => {
                        self.cmd_again(&conv, &sender, &hint).await;
                        return;
                    }
                    "/retry" => {
                        self.cmd_retry(&conv, &sender, &hint).await;
                        return;
                    }
                    "/export" => {
                        self.cmd_export(&conv, &sender, &hint, &parts).await;
                        return;
                    }
                    "/cd" => {
                        self.cmd_cd(&conv, &hint, &parts).await;
                        return;
                    }
                    "/ws" => {
                        self.cmd_ws(&conv, &hint, &parts).await;
                        return;
                    }
                    "/img" => {
                        self.cmd_img(&conv, &hint, &parts).await;
                        return;
                    }
                    "/file" => {
                        self.cmd_file(&conv, &hint, &parts).await;
                        return;
                    }
                    "/timeout" => {
                        self.cmd_timeout(&conv, &hint, &parts).await;
                        return;
                    }
                    "/perm" => {
                        self.cmd_perm(&conv, &sender, &hint, &parts).await;
                        return;
                    }
                    "/stop" => {
                        self.cmd_stop(&conv, &hint, &parts).await;
                        return;
                    }
                    "/queue" => {
                        self.cmd_queue(&conv, &sender.0, &hint, &parts).await;
                        return;
                    }
                    "/model" => {
                        self.cmd_model(&conv, &sender.0, &hint, &parts).await;
                        return;
                    }
                    "/help" => {
                        self.cmd_help(&conv, &hint).await;
                        return;
                    }
                    _ => {
                        // 快捷命令（v1.17）：config `shortcuts` 把 /name 映射到
                        // prompt 模板（`$args` = 命令剩余参数）——按普通 agent
                        // 消息分派（完整鉴权/批处理/steering 路径）。未命中才
                        // 走未知命令提示。
                        let name = cmd.trim_start_matches('/');
                        let template = self
                            .shortcuts
                            .read()
                            .expect("shortcuts 读锁")
                            .get(name)
                            .cloned();
                        if let Some(template) = template {
                            let args = parts[1..].join(" ");
                            let prompt = template.replace("$args", &args);
                            let mut m = msg;
                            m.text = Some(prompt);
                            m.source_msg_id = None; // 快捷展开非原文，不锚表情
                            self.dispatch_agent_message(m).await;
                            return;
                        }
                        // S-12：模糊匹配建议 + 分组竖排命令表（与 /help 分组同构）。
                        let mut text = unknown_command_reply(&cmd);
                        // v1.23：附已配置的 shortcuts（此前忘了快捷名只能翻
                        // config.toml；此处列名即可自愈）。
                        {
                            let sc = self
                                .shortcuts
                                .read()
                                .unwrap_or_else(|e| e.into_inner())
                                .clone();
                            if !sc.is_empty() {
                                let mut names: Vec<&String> = sc.keys().collect();
                                names.sort();
                                text.push_str(&format!(
                                    "\n\n⌨️ 可用快捷命令：{}",
                                    names
                                        .iter()
                                        .map(|n| format!("/{n}"))
                                        .collect::<Vec<_>>()
                                        .join(" ")
                                ));
                            }
                        }
                        self.reply(&conv, &text, &hint).await;
                        return;
                    }
                }
            }
        }

        // 4. 普通消息。
        // 文本与媒体皆空才丢弃；媒体消息（无文本）仍驱动 agent。
        // 纯媒体但全部下载失败：向用户报真实错误，不静默。
        if msg.text.as_deref().unwrap_or("").trim().is_empty() && msg.media.is_empty() {
            if !msg.media_errors.is_empty() {
                let errs = msg.media_errors.join("; ");
                self.reply(
                    &conv,
                    &format!(
                        "⚠️ 收到的媒体处理失败，无法查看：{errs}\n（常见原因：应用缺少 im:message:readonly 权限或权限未发布生效；详见服务端日志）"
                    ),
                    &hint,
                )
                .await;
            }
            return;
        }

        // P4-2 批处理：runner 在飞则入队（下一轮合并）后即返；否则本 task 成为
        // runner。runner 循环持 conv 串行锁跨轮次（slash 命令仍排队其后），每轮前
        // 等批处理窗口吃进连发消息；队空则交还 runner 身份、释放锁退出。
        self.dispatch_agent_message(msg).await;
    }

    /// 普通消息的批处理入口（handle 尾部与 /retry 共用）：入队/成为 runner →
    /// 轮次循环（含 W2-5 自动 compact）。抽独立方法避免 /retry → handle 的
    /// async 递归（handle 的命令分派对 /retry 重放的 prompt 并无意义）。
    pub(super) async fn dispatch_agent_message(&self, msg: InboundMessage) {
        let conv = msg.conv_id.clone();
        let hint = msg.reply_hint.clone();
        // W4-1 per-sender 成本上限（P0-3 v1.17：挪到入队闸门**逐消息**检查）。
        // 原在轮首只查批次首 sender——超限用户把消息排进他人批次即可绕过，
        // 且整轮花费错记到首 sender 头上。闸门覆盖所有入口（handle / /retry /
        // 排队），语义：轮成本记到发起者（首位 sender）。
        if let Some(limit) = self.sender_cost_limit {
            let since = super::now_secs() - 86_400;
            let spent = self
                .store
                .sender_cost_since(&msg.sender.0, since)
                .await
                .unwrap_or(0.0);
            if spent >= limit {
                warn!(
                    target: "imagent::core",
                    conv_id = %conv.0,
                    sender = %msg.sender.0,
                    spent,
                    limit,
                    "sender 成本上限命中，拒绝入队"
                );
                // v1.18 review：提示去重——cron 合成消息每分钟过闸，超限期间
                // 此前每条都回「已达上限」（一天 1440 条）。同 sender 1 小时至多
                // 提示一次；拒绝本身照常。
                let now = super::now_secs();
                let should_notice = {
                    let mut last = self.budget_notice_last.lock().await;
                    let hit = last.get(&msg.sender.0).copied().unwrap_or(0) + 3600 <= now;
                    if hit {
                        // 顺带清理过期条目（小 map，全扫可接受）。
                        last.retain(|_, ts| now - *ts < 7200);
                        last.insert(msg.sender.0.clone(), now);
                    }
                    hit
                };
                if should_notice {
                    self.reply(
                        &conv,
                        &format!(
                            "💰 你近 24 小时的用量已达上限（${spent:.2} / 上限 ${limit:.2}），本条未执行。\n窗口按时间滚动恢复，或请联系管理员调整 sender_daily_cost_limit_usd。"
                        ),
                        &hint,
                    )
                    .await;
                }
                return;
            }
        }
        if !self.enqueue_or_become_runner(&conv.0, msg, &hint).await {
            return;
        }
        let lock = self.acquire_conv_lock(&conv.0).await;
        let _guard = lock.lock().await;
        while let Some(batch) = self.take_batch_after_window(&conv.0).await {
            // P0-4（v1.17）：起跑前停止标记检查——/stop 在批窗口/注册间隙设置，
            // 命中（60s 内）则本批不启动（批已被取走消费，按中断语义丢弃）。
            {
                let hit = self
                    .with_conv(&conv.0, |cs| {
                        cs.stop_requested
                            .take()
                            .is_some_and(|ts| super::now_secs() - ts <= 60)
                    })
                    .await;
                if hit {
                    self.reply(
                        &conv,
                        "⏹️ 已中断：轮次在启动前被 /stop 拦下（本批消息未执行）",
                        &hint,
                    )
                    .await;
                    // v1.21 review（P1）：不能 break——take 已把 entry 留成空
                    // Vec，正常退出靠下一轮 take 的「空 → 删 entry 返回 None」
                    // 收尾；break 跳过该收尾后空 entry 悬挂，本 conv 所有后续
                    // 消息只入队不取批（死队列直到 /stop all 或重启）。continue
                    // 走完自然退出路径；/stop 之后新到的消息也不受影响（标记
                    // 已消费，下一批照常执行）。
                    continue;
                }
            }
            // 表情锚（全部批次消息的平台 id，首条=轮次触发）：排队⏳的消息随批
            // 翻 OnIt→终态；非 om_（合成消息）过滤。
            let react_mids: Vec<String> = batch
                .iter()
                .filter_map(|m| m.source_msg_id.clone().filter(|s| s.starts_with("om_")))
                .collect();
            let merged = merge_batch(batch);
            // P2（code-review v13）：全局并发护栏——持 conv 锁后、spawn agent 前
            // 取 permit（等待期间本 conv 后续消息照常入队等下一批）。permit 跨
            // 整轮持有（含轮后的自动 compact），迭代末 Drop 归还。
            let _round_permit = self.acquire_round_permit(&conv.0).await;
            // P2（code-review v14）：permit 等待期间到达的 /stop——默认 4 路闸门
            // 下 acquire 可排队分钟级（多群/cron 齐点），期间新设置的停止标记
            // 会被 round.rs 的 stop_mark_epoch（permit 之后才读取）当「起点
            // 水位基线」吞掉，而批循环顶部的检查早已过去——/stop 在整个排队
            // 窗口失效。故 permit 到手后、起跑前补一次与批循环顶部同款的无
            // 条件停止检查（round.rs 内现有的 epoch 复查保留，覆盖 preamble
            // 窗口）。命中则回复拦截文案并 continue：与 v1.21 注释同理不能
            // break——take 已把 entry 留成空 Vec，break 悬挂空 entry 成死队列，
            // continue 走完 take 收尾自然退出。
            {
                let hit = self
                    .with_conv(&conv.0, |cs| {
                        cs.stop_requested
                            .take()
                            .is_some_and(|ts| super::now_secs() - ts <= 60)
                    })
                    .await;
                if hit {
                    self.reply(
                        &conv,
                        "⏹️ 已中断：轮次在启动前被 /stop 拦下（本批消息未执行）",
                        &hint,
                    )
                    .await;
                    continue;
                }
            }
            let round_input = self.run_agent_round(merged, react_mids).await;
            // v1.18：轮末清 steering 注入计数（footer「已注入 N 条」随轮归零；
            // 保留排队字段——下一批语义仍在）。T18：hint 活在 ConvState 单表。
            self.with_conv(&conv.0, |cs| {
                if let Some(h) = cs.queued_hint.as_mut() {
                    h.steered = 0;
                }
            })
            .await;
            // W2-5：自动 compact——成功轮次的上下文水位超阈值时走既有压缩管道
            //（conv 锁仍在手，与 /compact 同串行域；无活动会话时内部跳过）。
            if let Some(in_tokens) = round_input {
                self.maybe_auto_compact(&conv, &hint, in_tokens).await;
            }
        }
        drop(_guard);
        self.release_conv_lock(&conv.0, lock).await;
    }
}

#[cfg(test)]
mod known_words_tests {
    use super::*;

    /// 钉住：[`Dispatcher::known_command_words`] 与 handle() 命令分派 match 的
    /// 臂完全一致——新命令加了 match 臂而漏登记清单时测试红（feishu 群内斜杠
    /// 免检白名单随之失效：新命令在群里不带 @ 会被 mention 门拦下）。
    /// 提取方式：扫描本文件源码中形如 `"/xxx" =>` 的 match 臂字面量（分派
    /// match 是文件内唯一该形态的字符串臂，见上方 grep 验证过的 34 臂）。
    #[test]
    fn known_command_words_match_dispatch_arms() {
        let src = include_str!("mod.rs");
        let mut arms: Vec<&str> = Vec::new();
        for line in src.lines() {
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix('"') else {
                continue;
            };
            let Some((word, _)) = rest.split_once("\" =>") else {
                continue;
            };
            if word.starts_with('/') {
                arms.push(word);
            }
        }
        arms.sort_unstable();
        arms.dedup();
        let mut listed = Dispatcher::known_command_words().to_vec();
        listed.sort_unstable();
        assert_eq!(
            arms, listed,
            "分派臂与 known_command_words 不一致：新命令须在 match 与清单两处同步登记"
        );
        assert!(!listed.is_empty());
        // 清单自身无重复（排序后相邻比较）。
        let dup = listed.windows(2).any(|w| w[0] == w[1]);
        assert!(!dup, "known_command_words 存在重复项");
    }
}
