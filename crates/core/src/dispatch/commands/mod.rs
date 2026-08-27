//! IM 斜杠命令：handle() 鉴权 + 命令分派（命令实现按主题拆在子模块）。
//!
//! [`admin`]：白名单/权限/配置（管理操作）；[`session`]：会话生命周期；
//! [`misc`]：状态/工作目录/媒体/帮助。

mod admin;
mod misc;
mod session;

use super::*;

/// S-12：全部支持的斜杠命令，按 /help 分组同构（未知命令提示竖排分组展示）。
pub(super) const COMMAND_GROUPS: &[(&str, &[&str])] = &[
    ("🗂 会话", &["/new", "/switch", "/sessions", "/resume", "/compact"]),
    ("📁 目录与文件", &["/cd", "/ws", "/img", "/file"]),
    ("🛡️ 权限与运行", &["/perm", "/stop", "/timeout"]),
    ("🧪 状态与诊断", &["/status", "/doctor", "/reconnect", "/config"]),
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
    /// 处理单条消息。内部任何错误都 log 并吞掉，不影响主循环。
    pub(super) async fn handle(&self, msg: InboundMessage) {
        let conv = msg.conv_id.clone();
        let sender = msg.sender.clone();
        let hint = msg.reply_hint.clone();

        // best-effort 指标：入站消息计数（失败只 warn 不阻断）。
        METRICS.messages_in.inc();
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
        if let Some(text) = msg.text.as_ref() {
            let trimmed = text.trim();
            if trimmed.starts_with('/') {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
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
                        self.cmd_status(&conv, &hint).await;
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
                        self.cmd_stop(&conv, &hint).await;
                        return;
                    }
                    "/help" => {
                        self.cmd_help(&conv, &hint).await;
                        return;
                    }
                    _ => {
                        // S-12：模糊匹配建议 + 分组竖排命令表（与 /help 分组同构）。
                        let text = unknown_command_reply(&cmd);
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
        if !self.enqueue_or_become_runner(&conv.0, msg, &hint).await {
            return;
        }
        let lock = self.acquire_conv_lock(&conv.0).await;
        let _guard = lock.lock().await;
        while let Some(batch) = self.take_batch_after_window(&conv.0).await {
            let merged = merge_batch(batch);
            self.run_agent_round(merged).await;
        }
        drop(_guard);
        self.release_conv_lock(&conv.0, lock).await;
    }
}
