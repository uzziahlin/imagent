//! 白名单 / 权限 / 配置类命令（管理操作，多数有 admin 门槛）。

use super::*;
use crate::types::UserId;

impl Dispatcher {
    /// /allow <id> —— 授权新用户（admin 门槛 + 审计 + 持久化失败告警）。
    pub(super) async fn cmd_allow(
        &self,
        conv: &ConvId,
        sender: &UserId,
        hint: &ReplyHint,
        parts: &[&str],
    ) {
        let target = parts.get(1).map(|s| s.trim()).unwrap_or("");
        if target.is_empty() {
            self.reply(conv, "用法: /allow <sender_id>", hint).await;
        } else {
            let actor = sender.0.as_str();
            // P2-D：仅管理员可授权新用户（admin_senders 非空时严格；
            // 空则向后兼容所有白名单用户可）。
            if !self.is_admin(actor) {
                self.reply(conv, "仅管理员（admin_senders）可授权新用户。", hint)
                    .await;
                return;
            }
            let added = self.auth.allow(target);
            // P2-E：持久化失败不能谎报「已授权」（内存已加但重启后丢失）。
            let persist_failed = self
                .store
                .add_allowed_sender(target, Some(actor), Some("im"))
                .await
                .is_err();
            if persist_failed {
                warn!(target: "imagent::core", "add_allowed_sender 持久化失败（内存已授权，重启丢失）");
            }
            if let Err(e) = self
                .store
                .append_audit(
                    "allow",
                    Some(actor),
                    Some(target),
                    Some(if added { "added" } else { "already-present" }),
                )
                .await
            {
                warn!(target: "imagent::core", error = %e, "append_audit 失败");
            }
            let text_out = if persist_failed {
                format!(
                                    "⚠️ `{target}` 已在内存授权，但持久化失败（重启后将丢失），请重试或本地 `imagent allow` 处理。"
                                )
            } else if added {
                format!("已授权 `{target}`。")
            } else {
                format!("`{target}` 已在白名单。")
            };
            self.reply(conv, &text_out, hint).await;
        }
    }

    /// /disallow <id> —— 撤销授权（admin 门槛，防自锁）。
    pub(super) async fn cmd_disallow(
        &self,
        conv: &ConvId,
        sender: &UserId,
        hint: &ReplyHint,
        parts: &[&str],
    ) {
        // P5-3（安全）：撤销白名单成员影响全局授权——此前无 admin 门槛，
        // 任何过门用户（含群内陌生成员）可把管理员本人踢出白名单（DoS）。
        // 与 /allow 的门槛对称。
        if !self.is_admin(&sender.0) {
            self.reply(conv, "仅管理员（admin_senders）可撤销授权。", hint)
                .await;
            return;
        }
        let target = parts.get(1).map(|s| s.trim()).unwrap_or("");
        if target.is_empty() {
            self.reply(conv, "用法: /disallow <sender_id>", hint).await;
        } else if target == sender.0.as_str() {
            // 防自锁：不允许撤销自己。
            self.reply(
                conv,
                "不允许撤销自己（防止锁死）。如需操作请在本地 CLI 处理。",
                hint,
            )
            .await;
        } else {
            let existed = self.auth.revoke(target);
            if let Err(e) = self.store.remove_allowed_sender(target).await {
                warn!(target: "imagent::core", error = %e, "remove_allowed_sender 失败");
            }
            if let Err(e) = self
                .store
                .append_audit(
                    "disallow",
                    Some(&sender.0),
                    Some(target),
                    Some(if existed { "removed" } else { "absent" }),
                )
                .await
            {
                warn!(target: "imagent::core", error = %e, "append_audit 失败");
            }
            self.reply(
                conv,
                &format!(
                    "已移除 `{target}`（{}）",
                    if existed { "成功" } else { "原本不在" }
                ),
                hint,
            )
            .await;
        }
    }

    /// /list —— 列出用户/会话白名单。
    pub(super) async fn cmd_list(&self, conv: &ConvId, hint: &ReplyHint) {
        let snap = self.auth.snapshot();
        let chats = self.auth.snapshot_chats();
        let mut out = if snap.is_empty() {
            "用户白名单为空。".to_string()
        } else {
            format!("用户白名单（{}）：{}", snap.len(), snap.join(", "))
        };
        // P4-5：会话（群）白名单一并列出。
        if chats.is_empty() {
            out.push_str("\n会话白名单为空。");
        } else {
            out.push_str(&format!(
                "\n会话白名单（{}）：{}",
                chats.len(),
                chats.join(", ")
            ));
        }
        self.reply(conv, &out, hint).await;
    }

    /// /whoami —— 报 sender/conv id。
    pub(super) async fn cmd_whoami(&self, conv: &ConvId, sender: &UserId, hint: &ReplyHint) {
        self.reply(
            conv,
            &format!("你的 sender id：`{}`\n当前会话 id：`{}`", sender.0, conv.0),
            hint,
        )
        .await;
    }

    /// /chat [allow|deny|list] —— 会话（群）白名单管理。
    pub(super) async fn cmd_chat(
        &self,
        conv: &ConvId,
        sender: &UserId,
        hint: &ReplyHint,
        parts: &[&str],
    ) {
        // P4-5：会话（群）白名单管理。与 /allow 同构：管理员门槛、
        // 内存 + store 双写、审计。`allow`/`deny` 缺省作用于当前会话。
        let sub = parts.get(1).map(|s| s.trim()).unwrap_or("");
        let actor = sender.0.as_str();
        match sub.to_ascii_lowercase().as_str() {
            "allow" | "deny" => {
                if !self.is_admin(actor) {
                    self.reply(conv, "仅管理员（admin_senders）可管理会话白名单。", hint)
                        .await;
                    return;
                }
                let target = parts
                    .get(2)
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&conv.0)
                    .to_string();
                let (applied, persist_failed) = if sub == "allow" {
                    let added = self.auth.allow_chat(&target);
                    let failed = self
                        .store
                        .add_allowed_chat(&target, Some(actor), Some("im"))
                        .await
                        .is_err();
                    (added, failed)
                } else {
                    let removed = self.auth.revoke_chat(&target);
                    let failed = self.store.remove_allowed_chat(&target).await.is_err();
                    (removed, failed)
                };
                if persist_failed {
                    warn!(target: "imagent::core", "会话白名单持久化失败（内存已改，重启丢失）");
                }
                let _ = self
                    .store
                    .append_audit(
                        if sub == "allow" {
                            "chat_allow"
                        } else {
                            "chat_deny"
                        },
                        Some(actor),
                        Some(&target),
                        Some(if applied { "applied" } else { "no-change" }),
                    )
                    .await;
                let verb = if sub == "allow" { "授权" } else { "移除" };
                let persist_note = if persist_failed {
                    "（⚠️ 持久化失败，重启后失效）"
                } else {
                    ""
                };
                self.reply(
                    conv,
                    &format!("✅ 已{verb}会话 {target}{persist_note}"),
                    hint,
                )
                .await;
            }
            _ => {
                let chats = self.auth.snapshot_chats();
                let list = if chats.is_empty() {
                    "（空）".to_string()
                } else {
                    chats.join("\n- ")
                };
                self.reply(
                                    conv,
                                    &format!(
                                        "用法：/chat allow [conv_id] 授权当前/指定会话\n/chat deny [conv_id] 移除\n/chat list 列出（如下）\n当前会话 id：`{}`\n- {list}",
                                        conv.0
                                    ),
                                    hint,
                                )
                                .await;
            }
        }
    }

    /// /config [k v] —— 查看/热改运行参数（admin 门槛）。
    pub(super) async fn cmd_config(
        &self,
        conv: &ConvId,
        sender: &UserId,
        hint: &ReplyHint,
        parts: &[&str],
    ) {
        // P4-6：查看 / 热改运行参数。改全局行为，管理员门槛。
        let key = parts.get(1).map(|s| s.trim()).unwrap_or("");
        let value = parts.get(2).map(|s| s.trim()).unwrap_or("");
        if key.is_empty() {
            // 先拷出共享句柄的值再跨 await（parking_lot guard 非 Send）。
            let idle_secs = self.agent_idle_timeout.read().as_secs();
            let window_ms = self.batch_window.read().as_millis();
            let cot = self.cot_detail.read().as_str();
            let perm = self.permission_mode.read().as_str();
            let text = format!(
                                "当前配置：\n- cot_detail = {cot}（off|brief|detailed）\n- batch_window_ms = {window_ms}\n- agent_idle_timeout_secs = {idle_secs}（0=关）\n- agent_timeout_secs = {}（重启生效）\n- permission_mode = {perm}\n用法：/config <key> <value>（管理员）",
                                self.agent_timeout.as_secs(),
                            );
            self.reply(conv, &text, hint).await;
            return;
        }
        if !self.is_admin(&sender.0) {
            self.reply(conv, "仅管理员（admin_senders）可修改配置。", hint)
                .await;
            return;
        }
        let result = match key {
            "cot_detail" => match CotDetail::from_str_lossy(value) {
                Some(d) => {
                    *self.cot_detail.write() = d;
                    format!("✅ cot_detail = {}", d.as_str())
                }
                None => "用法：/config cot_detail <off|brief|detailed>".into(),
            },
            "batch_window_ms" => match value.parse::<u64>() {
                Ok(ms) => {
                    *self.batch_window.write() = Duration::from_millis(ms);
                    format!("✅ batch_window_ms = {ms}")
                }
                Err(_) => "用法：/config batch_window_ms <毫秒数，0=关闭>".into(),
            },
            "agent_idle_timeout_secs" => match value.parse::<u64>() {
                Ok(s) => {
                    *self.agent_idle_timeout.write() = Duration::from_secs(s);
                    format!("✅ agent_idle_timeout_secs = {s}")
                }
                Err(_) => "用法：/config agent_idle_timeout_secs <秒数，0=关闭>".into(),
            },
            _ => {
                "未知配置项（支持：cot_detail / batch_window_ms / agent_idle_timeout_secs）".into()
            }
        };
        self.reply(conv, &result, hint).await;
    }

    /// /perm <off|allow|deny|ask> —— 权限模式切换（admin 门槛）。
    pub(super) async fn cmd_perm(
        &self,
        conv: &ConvId,
        sender: &UserId,
        hint: &ReplyHint,
        parts: &[&str],
    ) {
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
        if arg.is_empty() {
            let cur = *self.permission_mode.read();
            self.reply(
                conv,
                &format!("当前权限模式：{cur:?}\n用法：/perm <off|allow|deny|ask>"),
                hint,
            )
            .await;
            return;
        }
        // P5-2（安全）：权限模式影响全局审批策略（热切 off 即拆掉 IM
        // 审批闭环），与 /config 同级敏感，须管理员。
        if !self.is_admin(&sender.0) {
            self.reply(conv, "仅管理员（admin_senders）可修改权限模式。", hint)
                .await;
            return;
        }
        match arg {
            "off" | "allow" | "deny" | "ask" => {
                let mode = PermissionMode::from_str_lossy(arg);
                self.reload_permission_mode(mode);
                // Ask 模式的权限审批 socket 仅在 run() 启动时按当时模式
                // spawn 一次，热切到 Ask 不会补起 socket（重启生效）。
                let note = if arg == "ask" {
                    "（注意：Ask 模式的权限审批 socket 需重启 imagent 才生效）"
                } else {
                    ""
                };
                self.reply(conv, &format!("✅ 权限模式已切到 {arg}{note}"), hint)
                    .await;
            }
            _ => {
                self.reply(conv, "用法：/perm <off|allow|deny|ask>", hint)
                    .await
            }
        }
    }
}
