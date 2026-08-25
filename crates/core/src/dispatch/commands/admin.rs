//! 白名单 / 权限 / 配置类命令（管理操作，多数有 admin 门槛）。

use super::*;
use crate::config::ReplyMode;
use crate::types::{Mention, UserId};

/// P6-2：`/allow @名字` 的提及反解——从**本条消息**的 mentions 元数据取 open_id
/// （平台层已把 @占位 换成 `@名字`，此处只做名字 → id 匹配）。
/// - @名精确命中 → 该用户 id；
/// - 名字未命中但 mentions 恰好一条 → 唯一性兜底（显示名可能被客户端截断/改写）；
/// - 其余（无提及 / 名字歧义）→ None，调用方回用法提示。
fn resolve_mention_target<'a>(arg: &str, mentions: &'a [Mention]) -> Option<&'a str> {
    let name = arg.strip_prefix('@')?.trim();
    if name.is_empty() || mentions.is_empty() {
        return None;
    }
    if let Some(m) = mentions.iter().find(|m| m.name == name) {
        return Some(&m.user_id);
    }
    if mentions.len() == 1 {
        return Some(&mentions[0].user_id);
    }
    None
}

impl Dispatcher {
    /// /allow <id|@名字> —— 授权新用户（admin 门槛 + 审计 + 持久化失败告警）。
    /// P6-2：@提及形态由 [`resolve_mention_target`] 从消息元数据反解 open_id。
    pub(super) async fn cmd_allow(
        &self,
        conv: &ConvId,
        sender: &UserId,
        hint: &ReplyHint,
        parts: &[&str],
        mentions: &[Mention],
    ) {
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
        // P6-2：@提及形态先反解 open_id；反解不出则提示（不误把手打的 @字串 当 id）。
        let target: String = if arg.starts_with('@') {
            match resolve_mention_target(arg, mentions) {
                Some(id) => id.to_string(),
                None => {
                    self.reply(
                        conv,
                        "无法从本条消息解析该 @提及。请在同一条命令里 @ 该用户（如 `/allow @张三`），或直接用其 open_id（`ou_` 开头，`/whoami` 可查自己的）。",
                        hint,
                    )
                    .await;
                    return;
                }
            }
        } else {
            arg.to_string()
        };
        if target.is_empty() {
            self.reply(conv, "用法: /allow <sender_id|@名字>", hint)
                .await;
        } else {
            let actor = sender.0.as_str();
            // P2-D：仅管理员可授权新用户（admin_senders 非空时严格；
            // 空则向后兼容所有白名单用户可）。
            if !self.is_admin(actor) {
                self.reply(conv, "仅管理员（admin_senders）可授权新用户。", hint)
                    .await;
                return;
            }
            let added = self.auth.allow(&target);
            // P2-E：持久化失败不能谎报「已授权」（内存已加但重启后丢失）。
            let persist_failed = self
                .store
                .add_allowed_sender(&target, Some(actor), Some("im"))
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
                    Some(&target),
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

    /// /disallow <id|@名字> —— 撤销授权（admin 门槛，防自锁）。P6-2：支持 @提及。
    pub(super) async fn cmd_disallow(
        &self,
        conv: &ConvId,
        sender: &UserId,
        hint: &ReplyHint,
        parts: &[&str],
        mentions: &[Mention],
    ) {
        // P5-3（安全）：撤销白名单成员影响全局授权——此前无 admin 门槛，
        // 任何过门用户（含群内陌生成员）可把管理员本人踢出白名单（DoS）。
        // 与 /allow 的门槛对称。
        if !self.is_admin(&sender.0) {
            self.reply(conv, "仅管理员（admin_senders）可撤销授权。", hint)
                .await;
            return;
        }
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
        // P6-2：与 /allow 同款 @提及反解。
        let target: String = if arg.starts_with('@') {
            match resolve_mention_target(arg, mentions) {
                Some(id) => id.to_string(),
                None => {
                    self.reply(
                        conv,
                        "无法从本条消息解析该 @提及。请在同一条命令里 @ 该用户，或直接用其 open_id。",
                        hint,
                    )
                    .await;
                    return;
                }
            }
        } else {
            arg.to_string()
        };
        if target.is_empty() {
            self.reply(conv, "用法: /disallow <sender_id|@名字>", hint)
                .await;
        } else if target == sender.0.as_str() {
            // 防自锁：不允许撤销自己。
            self.reply(
                conv,
                "不允许撤销自己（防止锁死）。如需操作请在本地 CLI 处理。",
                hint,
            )
            .await;
        } else {
            let existed = self.auth.revoke(&target);
            if let Err(e) = self.store.remove_allowed_sender(&target).await {
                warn!(target: "imagent::core", error = %e, "remove_allowed_sender 失败");
            }
            if let Err(e) = self
                .store
                .append_audit(
                    "disallow",
                    Some(&sender.0),
                    Some(&target),
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
            // P7-A2：批量放行 bot 已加入的全部群（首次部署 onboard；平台不支持
            // 群列表时如实报错）。逐群内存 + store 双写，汇总回执。
            "allow-all" | "allow-all-groups" => {
                if !self.is_admin(actor) {
                    self.reply(conv, "仅管理员（admin_senders）可批量放行群。", hint)
                        .await;
                    return;
                }
                match self.platform.list_joined_chats().await {
                    Ok(chats) if chats.is_empty() => {
                        self.reply(conv, "bot 尚未加入任何群（先拉 bot 进群再试）。", hint)
                            .await;
                    }
                    Ok(chats) => {
                        let total = chats.len();
                        let mut applied = 0usize;
                        for c in &chats {
                            if self.auth.allow_chat(&c.chat_id) {
                                applied += 1;
                            }
                            if let Err(e) = self
                                .store
                                .add_allowed_chat(&c.chat_id, Some(actor), Some("im"))
                                .await
                            {
                                warn!(target: "imagent::core", error = %e, "批量放行持久化失败（内存已加）");
                            }
                        }
                        let _ = self
                            .store
                            .append_audit(
                                "chat_allow_all",
                                Some(actor),
                                None,
                                Some(&format!("applied={applied}/{total}")),
                            )
                            .await;
                        let names: Vec<String> = chats
                            .iter()
                            .map(|c| {
                                if c.name.is_empty() {
                                    c.chat_id.clone()
                                } else {
                                    c.name.clone()
                                }
                            })
                            .collect();
                        self.reply(
                            conv,
                            &format!(
                                "✅ 已放行 bot 加入的全部 {total} 个群（新增 {applied}）：\n- {}",
                                names.join("\n- ")
                            ),
                            hint,
                        )
                        .await;
                    }
                    Err(e) => {
                        self.reply(conv, &format!("列出群失败：{e}"), hint).await;
                    }
                }
            }
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

    /// /admin [list|add <id|@名字>|remove <id|@名字>] —— 管理员动态管理（P7-A1）。
    /// 与 config `admin_senders` 种子取并集；`/admin add` 即时生效 + 持久化。
    /// 防自锁：不可移除自己；清空列表时显式警示「空 = 所有白名单用户具备管理权」。
    pub(super) async fn cmd_admin(
        &self,
        conv: &ConvId,
        sender: &UserId,
        hint: &ReplyHint,
        parts: &[&str],
        mentions: &[Mention],
    ) {
        let sub = parts
            .get(1)
            .map(|s| s.trim())
            .unwrap_or("")
            .to_ascii_lowercase();
        let actor = sender.0.as_str();
        match sub.as_str() {
            "" | "list" => {
                let admins = self.admin_senders.read().clone();
                let text = if admins.is_empty() {
                    "管理员列表为空（= 所有白名单用户都具备管理权，P2-D 向后兼容语义）。\
                     /admin add <id|@名字> 可收紧。"
                        .to_string()
                } else {
                    format!("管理员（{}）：{}", admins.len(), admins.join(", "))
                };
                self.reply(conv, &text, hint).await;
            }
            "add" | "remove" => {
                if !self.is_admin(actor) {
                    self.reply(conv, "仅管理员可管理管理员列表。", hint).await;
                    return;
                }
                let arg = parts.get(2).map(|s| s.trim()).unwrap_or("");
                if arg.is_empty() {
                    self.reply(
                        conv,
                        "用法：/admin add <id|@名字> | /admin remove <id|@名字>",
                        hint,
                    )
                    .await;
                    return;
                }
                let target: String = if arg.starts_with('@') {
                    match resolve_mention_target(arg, mentions) {
                        Some(id) => id.to_string(),
                        None => {
                            self.reply(
                                conv,
                                "无法从本条消息解析该 @提及。请在同一条命令里 @ 该用户，或直接用其 open_id。",
                                hint,
                            )
                            .await;
                            return;
                        }
                    }
                } else {
                    arg.to_string()
                };
                if sub == "add" {
                    // 防自锁：向后兼容模式下（列表空 = 全员可管）设立首位管理员会
                    // 立即收回操作者权限——空 → 非空转换时把操作者一并加入
                    //（对齐参考项目「创建者不可自锁」语义）。
                    let (added, auto_self) = {
                        let mut list = self.admin_senders.write();
                        let was_empty = list.is_empty();
                        let auto_self = was_empty && !list.iter().any(|a| a == actor);
                        if auto_self {
                            list.push(actor.to_string());
                        }
                        let added = if list.contains(&target) {
                            false
                        } else {
                            list.push(target.clone());
                            true
                        };
                        (added, auto_self)
                    };
                    if auto_self {
                        if let Err(e) = self
                            .store
                            .add_admin_sender(actor, Some(actor), Some("im-auto"))
                            .await
                        {
                            warn!(target: "imagent::core", error = %e, "操作者自动入管理列表持久化失败");
                        }
                    }
                    let persist_failed = self
                        .store
                        .add_admin_sender(&target, Some(actor), Some("im"))
                        .await
                        .is_err();
                    if persist_failed {
                        warn!(target: "imagent::core", "add_admin_sender 持久化失败（内存已加，重启丢失）");
                    }
                    let _ = self
                        .store
                        .append_audit(
                            "admin_add",
                            Some(actor),
                            Some(&target),
                            Some(if added { "added" } else { "already-present" }),
                        )
                        .await;
                    // 管理员不在 sender 白名单时命令门都过不了——附引导而非自动放权。
                    let note = if self.auth.is_allowed(&UserId(target.clone())) {
                        String::new()
                    } else {
                        "\n⚠️ 该用户不在 sender 白名单，先 `/allow` 才能实际使用管理命令。"
                            .to_string()
                    };
                    let persist_note = if persist_failed {
                        "（⚠️ 持久化失败，重启后失效）"
                    } else {
                        ""
                    };
                    let lock_note = if auto_self {
                        format!("\n🔒 首位管理员已设立，操作者 `{actor}` 已一并加入（防自锁）。")
                    } else {
                        String::new()
                    };
                    self.reply(
                        conv,
                        &format!("✅ 已添加管理员 `{target}`{persist_note}。{lock_note}{note}"),
                        hint,
                    )
                    .await;
                } else {
                    if target == actor {
                        self.reply(
                            conv,
                            "不允许移除自己（防止锁死）。如需操作请在本地改 config 或其他管理员操作。",
                            hint,
                        )
                        .await;
                        return;
                    }
                    let existed = {
                        let mut list = self.admin_senders.write();
                        let before = list.len();
                        list.retain(|a| a != &target);
                        before != list.len()
                    };
                    if let Err(e) = self.store.remove_admin_sender(&target).await {
                        warn!(target: "imagent::core", error = %e, "remove_admin_sender 失败");
                    }
                    let _ = self
                        .store
                        .append_audit(
                            "admin_remove",
                            Some(actor),
                            Some(&target),
                            Some(if existed { "removed" } else { "absent" }),
                        )
                        .await;
                    let empty_warn = if self.admin_senders.read().is_empty() {
                        "\n⚠️ 管理员列表已清空 = 所有白名单用户都具备管理权（含 /allow /config /admin）。"
                    } else {
                        ""
                    };
                    self.reply(
                        conv,
                        &format!(
                            "已移除管理员 `{target}`（{}）。{empty_warn}",
                            if existed { "成功" } else { "原本不在" }
                        ),
                        hint,
                    )
                    .await;
                }
            }
            _ => {
                self.reply(
                    conv,
                    "用法：/admin [list] | /admin add <id|@名字> | /admin remove <id|@名字>",
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
            // P6 遗留补齐：require_mention 平台侧查询（None = 平台无群聊 @ 语义）。
            let require_mention = match self.platform.require_mention_in_group().await {
                Some(true) => "on（群消息须 @bot）".to_string(),
                Some(false) => "off（群消息全收）".to_string(),
                None => "（本平台不支持）".to_string(),
            };
            let text = format!(
                                "当前配置：\n- cot_detail = {cot}（off|brief|detailed）\n- batch_window_ms = {window_ms}\n- agent_idle_timeout_secs = {idle_secs}（0=关）\n- agent_timeout_secs = {}（重启生效）\n- permission_mode = {perm}\n- require_mention = {require_mention}（热切换，重启回 config 值）\n- reply_mode = {}（card|text，热切换，重启回 config 值）\n用法：/config <key> <value>（管理员）",
                                self.agent_timeout.as_secs(),
                                self.reply_mode.read().as_str(),
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
            // P6 遗留补齐：群消息须 @bot 热切换（平台侧策略，对下一消息生效）。
            "require_mention" => match value.to_ascii_lowercase().as_str() {
                "on" | "true" => match self.platform.set_require_mention_in_group(true).await {
                    Ok(()) => "✅ require_mention = on（群消息须 @bot；重启回 config 值）".into(),
                    Err(e) => format!("设置失败：{e}"),
                },
                "off" | "false" => match self.platform.set_require_mention_in_group(false).await {
                    Ok(()) => "✅ require_mention = off（群消息全收；重启回 config 值）".into(),
                    Err(e) => format!("设置失败：{e}"),
                },
                _ => "用法：/config require_mention <on|off>".into(),
            },
            // P7-A4：回复形态偏好（card=流式卡片 / text=纯文本），热切换即时生效
            //（下一轮起不建卡），重启回 config 值。
            "reply_mode" => match ReplyMode::from_str_lossy(value) {
                Some(m) => {
                    *self.reply_mode.write() = m;
                    format!("✅ reply_mode = {}（下一轮生效；重启回 config 值）", m.as_str())
                }
                None => "用法：/config reply_mode <card|text>".into(),
            },
            _ => {
                "未知配置项（支持：cot_detail / batch_window_ms / agent_idle_timeout_secs / require_mention / reply_mode）".into()
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
                &format!("当前权限模式：{cur:?}\n用法：/perm <auto|off|allow|deny|ask>"),
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
            "auto" => {
                // auto 按当前后端解析成具体档再入运行时（claude-cli → ask，
                // 其余 → off）；回执带解析结果。
                let resolved = PermissionMode::Auto.resolve(self.backend.name());
                self.reload_permission_mode(resolved);
                let note = if resolved == PermissionMode::Ask {
                    "（注意：Ask 模式的权限审批 socket 需重启 imagent 才生效）"
                } else {
                    ""
                };
                self.reply(
                    conv,
                    &format!(
                        "✅ 权限模式 auto → {}（按后端 {}）{note}",
                        resolved.as_str(),
                        self.backend.name()
                    ),
                    hint,
                )
                .await;
            }
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
                self.reply(conv, "用法：/perm <auto|off|allow|deny|ask>", hint)
                    .await
            }
        }
    }
}
