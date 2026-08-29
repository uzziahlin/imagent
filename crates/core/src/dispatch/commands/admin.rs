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
    /// S2：管理命令被拒时的提示文案——admin_senders 为空（= 无人是管理员）时
    /// 附配置引导，避免用户误以为白名单用户仍可操作。
    fn admin_denied_reply(&self, action: &str) -> String {
        if self.admin_senders.read().is_empty() {
            format!(
                "仅管理员（admin_senders）可{action}。当前 admin_senders 为空（无人是管理员），\
                 请在本地通过 CLI（`imagent setup` 或 config.toml 的 admin_senders）配置后再使用管理命令。"
            )
        } else {
            format!("仅管理员（admin_senders）可{action}。")
        }
    }

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
            // P2-D/S2：仅管理员可授权新用户（admin_senders 空 = 无人是管理员）。
            if !self.is_admin(actor) {
                let msg = self.admin_denied_reply("授权新用户");
                self.reply(conv, &msg, hint).await;
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
            let msg = self.admin_denied_reply("撤销授权");
            self.reply(conv, &msg, hint).await;
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
                    let msg = self.admin_denied_reply("批量放行群");
                    self.reply(conv, &msg, hint).await;
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
                    let msg = self.admin_denied_reply("管理会话白名单");
                    self.reply(conv, &msg, hint).await;
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
                    // S2：空列表 = 无人是管理员（收紧后 IM 内无法自助设立首位
                    // 管理员——防任意白名单成员自扩权）。
                    "管理员列表为空（= 无人是管理员，IM 内管理命令不可用）。\
                     请在本地通过 CLI（`imagent setup` 或 config.toml 的 admin_senders）配置。"
                        .to_string()
                } else {
                    format!("管理员（{}）：{}", admins.len(), admins.join(", "))
                };
                self.reply(conv, &text, hint).await;
            }
            "add" | "remove" => {
                if !self.is_admin(actor) {
                    let msg = self.admin_denied_reply("管理管理员列表");
                    self.reply(conv, &msg, hint).await;
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
                    // 防自锁（历史路径，S2 收紧后列表非空才会走到这里，保留
                    // 兜底）：设立首位管理员时把操作者一并加入（防自锁）。
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
                        // S2：清空 = 无人是管理员（IM 内管理命令将不可用）。
                        "\n⚠️ 管理员列表已清空 = 无人是管理员，IM 内管理命令将不可用（含 /allow /config /admin）；需在本地 CLI 重新配置。"
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
            let reply_mode = self.reply_mode.read().as_str();
            // Wave B-7：本会话 COT 覆盖（有则标注，无则跟随全局）。
            let cot_conv = match self.cot_overrides.lock().await.get(&conv.0) {
                Some(d) => format!("{}（本会话覆盖）", d.as_str()),
                None => format!("{cot}（跟随全局）"),
            };
            // Wave B-4：quiet_hours（config 注入原文；只可查不可热改——降级判定
            // 在平台实现侧，无热改句柄，重启生效）。
            let quiet = self
                .quiet_hours_raw
                .read()
                .clone()
                .unwrap_or_else(|| "（未设置）".to_string());
            // P6 遗留补齐：require_mention 平台侧查询（None = 平台无群聊 @ 语义）。
            let require_mention = match self.platform.require_mention_in_group().await {
                Some(true) => "on（群消息须 @bot）".to_string(),
                Some(false) => "off（群消息全收）".to_string(),
                None => "（本平台不支持）".to_string(),
            };
            let rm_current = match self.platform.require_mention_in_group().await {
                Some(true) => "on",
                Some(false) => "off",
                None => "on",
            };
            let text = format!(
                                "当前配置：\n- cot_detail = {cot}（off|brief|detailed）\n- cot（本会话）= {cot_conv}（/config cot <off|brief|detailed|default>，白名单可用）\n- batch_window_ms = {window_ms}\n- agent_idle_timeout_secs = {idle_secs}（0=关）\n- agent_timeout_secs = {}（0=关，默认；重启生效）\n- permission_mode = {perm}\n- require_mention = {require_mention}（热切换，重启回 config 值）\n- reply_mode = {reply_mode}（card|text，热切换，重启回 config 值）\n- quiet_hours = {quiet}（免打扰：时段内加急提醒降级普通消息；重启生效）\n用法：/config <key> <value>（管理员；cot 为本会话偏好，白名单可用）",
                                self.agent_timeout.as_secs(),
                            );
            // P9-2：表单卡（飞书等支持 form 的平台渲染下拉 + 提交；其余平台降级
            // 上面的纯文本）。只放已有热改键；batch/timeout 类数值键继续文本命令。
            let entries = vec![
                ConfigFormField {
                    key: "reply_mode".into(),
                    label: "回复形态".into(),
                    current: reply_mode.into(),
                    options: vec![
                        ("card".into(), "卡片（流式，默认）".into()),
                        ("text".into(), "纯文本".into()),
                    ],
                },
                ConfigFormField {
                    key: "cot_detail".into(),
                    label: "工具过程展示".into(),
                    current: cot.into(),
                    options: vec![
                        ("brief".into(), "简略（默认）".into()),
                        ("detailed".into(), "详细".into()),
                        ("off".into(), "关闭".into()),
                    ],
                },
                ConfigFormField {
                    key: "require_mention".into(),
                    label: "群消息须 @bot".into(),
                    current: rm_current.into(),
                    options: vec![
                        ("on".into(), "是（默认）".into()),
                        ("off".into(), "否（群消息全收）".into()),
                    ],
                },
            ];
            let _ = self
                .platform
                .send_config_form(conv, &entries, &text, hint)
                .await;
            return;
        }
        // P9-2：表单提交回传（/config form k=v k=v …）——逐对应用（键白名单在
        // feishu proto 侧已过滤，这里再走一遍完整校验）。表单字段全部是**全局**
        // 热改键（reply_mode/cot_detail/require_mention），与 `/config k v` 同级，
        // 必须过 admin 门槛——此前该分支在门槛之前，任意白名单用户可用表单绕过
        // admin 校验热改全局配置（安全修复）。
        if key == "form" {
            if !self.is_admin(&sender.0) {
                let msg = self.admin_denied_reply("修改配置");
                self.reply(conv, &msg, hint).await;
                return;
            }
            let pairs: Vec<&str> = parts.iter().skip(2).copied().collect();
            let mut results = Vec::new();
            for pair in pairs {
                let Some((k, v)) = pair.split_once('=') else {
                    continue;
                };
                results.push(self.apply_config_kv(k.trim(), v.trim()).await);
            }
            let text = if results.is_empty() {
                "表单未包含可识别的配置项。".to_string()
            } else {
                results.join("\n")
            };
            self.reply(conv, &text, hint).await;
            return;
        }
        // Wave B-7：/config cot <off|brief|detailed|default> —— **per-conv** COT
        // 档位（白名单用户即可改自己所在会话的展示粒度——会话级偏好与 /timeout
        // 同语义，不属全局配置，无需 admin；admin 改全局仍走 /config cot_detail）。
        // 须在下方 admin 门槛之前分流。
        if key == "cot" {
            let result = self.apply_conv_cot(&conv.0, value).await;
            self.reply(conv, &result, hint).await;
            return;
        }
        if !self.is_admin(&sender.0) {
            let msg = self.admin_denied_reply("修改配置");
            self.reply(conv, &msg, hint).await;
            return;
        }
        let result = self.apply_config_kv(key, value).await;
        self.reply(conv, &result, hint).await;
    }

    /// Wave B-7：per-conv COT 覆盖应用（`/config cot`，白名单可用；default 清除
    /// 覆盖回全局）。返回面向用户的结果文案。
    async fn apply_conv_cot(&self, conv: &str, value: &str) -> String {
        if value.eq_ignore_ascii_case("default") {
            self.cot_overrides.lock().await.remove(conv);
            let global = self.cot_detail.read().as_str();
            return format!("✅ 已清除本会话覆盖，回到全局 cot_detail = {global}");
        }
        match CotDetail::from_str_lossy(value) {
            Some(d) => {
                self.cot_overrides.lock().await.insert(conv.to_string(), d);
                format!(
                    "✅ 本会话 cot = {}（仅本会话生效；/config cot default 恢复全局）",
                    d.as_str()
                )
            }
            None => {
                "用法：/config cot <off|brief|detailed|default>（本会话偏好，无需管理员）".into()
            }
        }
    }

    /// 单个配置键的热改应用（`/config k v` 与表单提交 `/config form k=v` 共用；
    /// 返回面向用户的结果文案）。
    async fn apply_config_kv(&self, key: &str, value: &str) -> String {
        match key {
            "cot_detail" => match CotDetail::from_str_lossy(value) {
                Some(d) => {
                    *self.cot_detail.write() = d;
                    format!("✅ cot_detail = {}", d.as_str())
                }
                None => "用法：cot_detail <off|brief|detailed>".into(),
            },
            "batch_window_ms" => match value.parse::<u64>() {
                Ok(ms) => {
                    // L5（code-review v8）：热改复用启动侧上限（10s）——巨值会让
                    // runner 永睡且不在 running 注册表、/stop 救不回。
                    const BATCH_WINDOW_MAX_MS: u64 = 10_000;
                    if ms > BATCH_WINDOW_MAX_MS {
                        format!("❌ batch_window_ms 上限 {BATCH_WINDOW_MAX_MS}（当前 {ms}）")
                    } else {
                        *self.batch_window.write() = Duration::from_millis(ms);
                        format!("✅ batch_window_ms = {ms}")
                    }
                }
                Err(_) => "用法：batch_window_ms <毫秒数，0=关闭>".into(),
            },
            "agent_idle_timeout_secs" => match value.parse::<u64>() {
                Ok(s) => {
                    *self.agent_idle_timeout.write() = Duration::from_secs(s);
                    format!("✅ agent_idle_timeout_secs = {s}")
                }
                Err(_) => "用法：agent_idle_timeout_secs <秒数，0=关闭>".into(),
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
                _ => "用法：require_mention <on|off>".into(),
            },
            // P7-A4：回复形态偏好（card=流式卡片 / text=纯文本），热切换即时生效
            //（下一轮起不建卡），重启回 config 值。
            "reply_mode" => match ReplyMode::from_str_lossy(value) {
                Some(m) => {
                    *self.reply_mode.write() = m;
                    format!("✅ reply_mode = {}（下一轮生效；重启回 config 值）", m.as_str())
                }
                None => "用法：reply_mode <card|text>".into(),
            },
            _ => "未知配置项（支持：cot_detail / batch_window_ms / agent_idle_timeout_secs / require_mention / reply_mode）"
                .into(),
        }
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
            // auto-claude 是 auto 在 claude-cli 下的解析产物，附说明防「设置了
            // auto 怎么显示别的档」的困惑。
            let note = if cur == PermissionMode::AutoClaude {
                "（由 auto 按后端解析：claude 原生 auto 模式，分类器自动放行安全操作，高危操作走 IM；可用 claude_permission_mode 配置覆盖）"
            } else {
                ""
            };
            self.reply(
                conv,
                &format!(
                    "当前权限模式：{}{note}\n用法：/perm <auto|off|allow|deny|ask>",
                    cur.as_str()
                ),
                hint,
            )
            .await;
            return;
        }
        // P5-2（安全）：权限模式影响全局审批策略（热切 off 即拆掉 IM
        // 审批闭环），与 /config 同级敏感，须管理员。
        if !self.is_admin(&sender.0) {
            let msg = self.admin_denied_reply("修改权限模式");
            self.reply(conv, &msg, hint).await;
            return;
        }
        match arg {
            "auto" => {
                // auto 按当前后端解析成具体档再入运行时（claude-cli → ask，
                // 其余 → off）；回执带解析结果。
                let resolved = PermissionMode::Auto.resolve(self.backend.name());
                // B3：解析结果若为闭环类档位（claude 系 → ask/auto-claude），须
                // 后端能力为 FullLoop 才放行热切（与启动期 fail-closed 同口径）。
                if resolved.needs_socket()
                    && self.backend.permission_capability()
                        != crate::backend::PermissionCapability::FullLoop
                {
                    self.reply(
                        conv,
                        &format!(
                            "⚠️ auto 在后端 {} 解析为 {}（IM 审批闭环），但该后端不支持闭环（{}）；未切换。请改用 off/allow/deny 或换 claude 系后端",
                            self.backend.name(),
                            resolved.as_str(),
                            self.backend.permission_capability().as_str()
                        ),
                        hint,
                    )
                    .await;
                    return;
                }
                // D12：闭环类档位热切时惰性补起 socket（幂等），不再要求重启。
                // S-1：reload Result 化——能力/socket 失败统一走 Err 回执（模式保持不变）。
                match self.reload_permission_mode(resolved) {
                    Ok(()) => {
                        self.reply(
                            conv,
                            &format!(
                                "✅ 权限模式 auto → {}（按后端 {}）",
                                resolved.as_str(),
                                self.backend.name()
                            ),
                            hint,
                        )
                        .await;
                    }
                    Err(e) => {
                        self.reply(
                            conv,
                            &format!("⚠️ 权限模式热切失败：{e}（保持原模式不变）"),
                            hint,
                        )
                        .await;
                    }
                }
            }
            "off" | "allow" | "deny" | "ask" => {
                let mode = PermissionMode::from_str_lossy(arg);
                // B3：ask 是闭环类档位，非 FullLoop 后端拒绝热切（fail-closed，
                // 与启动期校验同口径）。
                if mode.needs_socket()
                    && self.backend.permission_capability()
                        != crate::backend::PermissionCapability::FullLoop
                {
                    self.reply(
                        conv,
                        &format!(
                            "⚠️ 后端 {} 不支持 IM 审批闭环（{}），无法切到 {arg}；请改用 off/allow/deny 或换 claude 系后端",
                            self.backend.name(),
                            self.backend.permission_capability().as_str()
                        ),
                        hint,
                    )
                    .await;
                    return;
                }
                // D12：热切 ask 时惰性补起 socket accept task（幂等防重复），
                // 回执不再要求重启。S-1：reload Result 化（能力/socket 失败回执）。
                match self.reload_permission_mode(mode) {
                    Ok(()) => {
                        self.reply(conv, &format!("✅ 权限模式已切到 {arg}"), hint)
                            .await;
                    }
                    Err(e) => {
                        self.reply(
                            conv,
                            &format!("⚠️ 权限模式热切失败：{e}（保持原模式不变）"),
                            hint,
                        )
                        .await;
                    }
                }
            }
            _ => {
                self.reply(conv, "用法：/perm <auto|off|allow|deny|ask>", hint)
                    .await
            }
        }
    }
}
