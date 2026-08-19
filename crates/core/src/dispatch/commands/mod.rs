//! IM 斜杠命令：handle() 鉴权 + 命令分派（命令实现按主题拆在子模块）。
//!
//! [`admin`]：白名单/权限/配置（管理操作）；[`session`]：会话生命周期；
//! [`misc`]：状态/工作目录/媒体/帮助。

mod admin;
mod misc;
mod session;

use super::*;

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
            let guide = format!(
                "发现模式：当前白名单为空。你的 sender id 是 `{}`，会话 id 是 `{}`。\n\
                 请管理员在本地运行 `imagent allow {}` 授权用户、或 `imagent allow-chat {}` \
                 授权整个会话（群）后重启 imagent；也可由已授权用户在 IM 内发 /allow / /chat allow。",
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
                        self.cmd_allow(&conv, &sender, &hint, &parts).await;
                        return;
                    }
                    "/disallow" => {
                        self.cmd_disallow(&conv, &sender, &hint, &parts).await;
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
                        self.cmd_resume(&conv, &hint, &parts).await;
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
                        self.reply(
                            &conv,
                            &format!(
                                "未知命令: {cmd}（支持: /new /switch /sessions /resume /compact /cd /ws /img /perm /stop /config /status /doctor /reconnect /allow /disallow /chat /list /whoami /help）"
                            ),
                            &hint,
                        )
                        .await;
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
