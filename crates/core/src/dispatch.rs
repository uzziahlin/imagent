//! 消息调度核心。
//!
//! `Dispatcher` 持有注入的 `Arc<dyn Platform>` / `Arc<dyn Backend>` / `Store` /
//! `Auth` / 配置，循环 `platform.recv()` 并对每条消息 `tokio::spawn` 处理。
//!
//! 两条硬约束在此体现：
//! 1. 非白名单 sender 丢弃；发现模式（白名单为空）回引导消息但不驱动 agent。
//! 2. backend 只用配置的 `allowed_tools`、workdir 用配置的 `default_workdir`。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::auth::Auth;
use crate::backend::Backend;
use crate::config::PermissionMode;
use crate::error::Result;
use crate::metrics::METRICS;
use crate::permission::{parse_reply, PermissionReply, PermissionRouter};
use crate::platform::Platform;
use crate::types::{AgentChunk, ConvId, InboundMessage, ReplyHint, SessionId};
use imagent_store::{NamedSessionRow, SessionRow, Store};
use parking_lot::RwLock;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
/// 把字符串按字符截断到 n 个字符，超出则加省略号。
fn truncate_str(s: &str, n: usize) -> String {
    let count = s.chars().count();
    let t: String = s.chars().take(n).collect();
    if count > n {
        format!("{t}…")
    } else {
        t
    }
}

/// 格式化工具调用摘要：最多展示 5 个，超出标 `…(+N)`。
/// 形如 `\n\n🔧 工具调用：Read({"path":"…}), Edit({"file":"…})`。
fn format_tool_summary(tool_calls: &[(String, String)]) -> String {
    let shown: Vec<String> = tool_calls
        .iter()
        .take(5)
        .map(|(t, i)| format!("{t}({i})"))
        .collect();
    let mut s = format!("\n\n🔧 工具调用：{}", shown.join(", "));
    if tool_calls.len() > 5 {
        s.push_str(&format!(" …(+{})", tool_calls.len() - 5));
    }
    s
}
/// 当前活动命名 session 的 config 键：`active_name:<conv_id>`。
/// 不存在/空值表示当前会话为默认未命名 session。
fn active_name_key(conv_id: &str) -> String {
    format!("active_name:{conv_id}")
}
/// 压缩摘要的 config 键：`compact_summary:<conv_id>`。
/// 由 /compact 写入，下次新建 session 时作为前情摘要注入后清除（一次性）。
fn compact_summary_key(conv_id: &str) -> String {
    format!("compact_summary:{conv_id}")
}

/// 错误是否指示 iLink session 过期（需重新 login）。
///
/// `CoreError::Platform` 的 `Display` 形如 `platform(ilink): session expired: ...`，
/// 故按子串匹配即可，无需 match 具体变体。
fn is_session_expired_err(e: &crate::error::CoreError) -> bool {
    e.to_string().to_lowercase().contains("session expired")
}

pub struct Dispatcher {
    platform: Arc<dyn Platform>,
    backend: Arc<dyn Backend>,
    store: Store,
    auth: Auth,
    default_workdir: PathBuf,
    allowed_tools: Arc<RwLock<Vec<String>>>,
    /// per-conv 串行锁：同一会话的 agent 任务排队执行，避免 session 冲突。
    conv_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// IM 权限审批路由（Ask 闭环用）。
    router: Arc<PermissionRouter>,
    /// 权限审批模式。
    permission_mode: Arc<RwLock<PermissionMode>>,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        platform: Arc<dyn Platform>,
        backend: Arc<dyn Backend>,
        store: Store,
        auth: Auth,
        default_workdir: PathBuf,
        allowed_tools: Vec<String>,
        permission_mode: PermissionMode,
    ) -> Self {
        Self::new_with_handles(
            platform,
            backend,
            store,
            auth,
            default_workdir,
            Arc::new(RwLock::new(allowed_tools)),
            Arc::new(RwLock::new(permission_mode)),
        )
    }

    /// 与 [`new`](Self::new) 相同，但接受外部持有的共享句柄
    /// （`allowed_tools` / `permission_mode` 的 `Arc<RwLock>`）。
    ///
    /// main 用此构造，把 `permission_mode` 句柄同时共享给 `ClaudeBackend`，
    /// 使 SIGHUP 热重载对二者同时生效。
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_handles(
        platform: Arc<dyn Platform>,
        backend: Arc<dyn Backend>,
        store: Store,
        auth: Auth,
        default_workdir: PathBuf,
        allowed_tools: Arc<RwLock<Vec<String>>>,
        permission_mode: Arc<RwLock<PermissionMode>>,
    ) -> Self {
        Self {
            platform,
            backend,
            store,
            auth,
            default_workdir,
            allowed_tools,
            conv_locks: Mutex::new(HashMap::new()),
            router: Arc::new(PermissionRouter::new()),
            permission_mode,
        }
    }

    /// SIGHUP 热重载：整体替换 allowed_tools。
    pub fn reload_tools(&self, tools: Vec<String>) {
        *self.allowed_tools.write() = tools;
    }

    /// SIGHUP 热重载：更新 permission_mode（与 ClaudeBackend 共享同一句柄时
    /// 二者同步生效）。注意：Ask 模式的 socket accept task 仅在 `run()` 启动时
    /// 按当时的模式 spawn 一次，热切到 Ask 不会补起 socket（重启生效）。
    pub fn reload_permission_mode(&self, mode: PermissionMode) {
        *self.permission_mode.write() = mode;
    }

    /// 暴露 auth（main 的 SIGHUP task 用其 reload）。
    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    /// 暴露 router（主进程 socket accept task 用）。
    pub fn router(&self) -> Arc<PermissionRouter> {
        self.router.clone()
    }

    /// 主循环。循环 `platform.recv()`，每条消息 `tokio::spawn` 处理（不阻塞 recv）。
    /// recv 返回 Err 时：session 过期 → 优雅停止（返回 Err 让 main 提示重新 login）；
    /// 其它错误 → 记录日志后继续（长轮询层自管重连/退避），不 panic。
    pub async fn run(self: Arc<Self>) -> Result<()> {
        // Ask 模式：spawn unix socket accept task（MCP server 转发的权限请求经此进主进程）。
        if matches!(*self.permission_mode.read(), PermissionMode::Ask) {
            if let Some(sock) = crate::permission::default_sock_path() {
                self.spawn_socket_accept(sock.to_string_lossy().into_owned());
            } else {
                warn!(target: "imagent::core", "Ask 模式但无法定位 socket 路径，权限请求将无法路由");
            }
        }

        loop {
            match self.platform.recv().await {
                Ok(msg) => {
                    let conv_id = msg.conv_id.0.clone();
                    // 权限闭环优先：若该 conv 正等待 approve/deny 回复，
                    // 把这条消息当作回复送达 oneshot，不走正常 handle。
                    if self.router.has_pending(&conv_id).await {
                        let reply = parse_reply(msg.text.as_deref().unwrap_or(""));
                        let routed = self.router.route(&conv_id, reply).await;
                        if routed {
                            continue;
                        }
                        // 未命中（理论上 has_pending 为 true 时应命中）：fallthrough 处理。
                    }
                    // 每条消息独立 spawn，不阻塞 recv。
                    let this = self.clone();
                    tokio::spawn(async move {
                        this.handle(msg).await;
                    });
                }
                Err(e) => {
                    if is_session_expired_err(&e) {
                        tracing::error!(
                            target: "imagent::core",
                            error = %e,
                            "session 过期，停止 dispatcher（需重新 login）"
                        );
                        return Err(e);
                    }
                    warn!(target: "imagent::core", error = %e, "platform.recv 失败，继续重试");
                }
            }
        }
    }

    /// spawn socket accept task：每个连接独立 spawn，读权限请求 → send_text 询问
    /// 用户 → register 等 receiver → 写回复回 socket。
    fn spawn_socket_accept(self: &Arc<Self>, sock: String) {
        // 清理可能残留的旧 socket 文件。
        let _ = std::fs::remove_file(&sock);
        let listener = match std::os::unix::net::UnixListener::bind(&sock) {
            Ok(l) => l,
            Err(e) => {
                warn!(target: "imagent::core", sock = %sock, error = %e, "bind permission socket failed; Ask 闭环不可用");
                return;
            }
        };
        // 转为非阻塞，包进 tokio。
        listener.set_nonblocking(true).ok();
        let listener = match tokio::net::UnixListener::from_std(listener) {
            Ok(l) => l,
            Err(e) => {
                warn!(target: "imagent::core", error = %e, "from_std permission socket failed");
                return;
            }
        };
        let platform = self.platform.clone();
        let router = self.router.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let platform = platform.clone();
                        let router = router.clone();
                        tokio::spawn(async move {
                            Self::handle_permission_socket(stream, platform, router).await;
                        });
                    }
                    Err(e) => {
                        warn!(target: "imagent::core", error = %e, "permission socket accept 失败");
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        });
    }

    /// 处理单个 socket 连接：读请求行 → send_text 询问 → 等回复 → 写回复。
    async fn handle_permission_socket(
        mut stream: tokio::net::UnixStream,
        platform: Arc<dyn Platform>,
        router: Arc<PermissionRouter>,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        if reader.read_line(&mut line).await.is_err() {
            return;
        }
        let req: serde_json::Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(e) => {
                warn!(target: "imagent::core", raw = %line, error = %e, "permission socket 非 JSON");
                return;
            }
        };
        let conv_id = req
            .get("conv_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let tool_name = req
            .get("tool_name")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let input_str = req.get("input").map(|v| v.to_string()).unwrap_or_default();
        if conv_id.is_empty() {
            return;
        }
        let conv = ConvId(conv_id.clone());
        // 询问用户。
        let prompt_text = format!(
            "🔐 Claude 请求执行 {tool_name}：{}\n回复 y 允许，其它拒绝。",
            truncate_str(&input_str, 80)
        );
        if let Err(e) = platform
            .send_text(&conv, &prompt_text, &ReplyHint::None)
            .await
        {
            warn!(target: "imagent::core", conv_id = %conv_id, error = %e, "send permission ask 失败");
        }
        // 注册 pending，等回复（recv 循环 route 到这里）。
        let rx = router.register(&conv_id).await;
        let reply: PermissionReply = match rx.await {
            Ok(r) => r,
            Err(_) => PermissionReply {
                allow: false,
                message: Some("permission router dropped".into()),
            },
        };
        // 写回 socket（一行 JSON）。
        let resp = serde_json::json!({
            "allow": reply.allow,
            "message": reply.message,
        });
        let mut out = resp.to_string();
        out.push('\n');
        let _ = stream.write_all(out.as_bytes()).await;
        let _ = stream.flush().await;
    }

    /// 处理单条消息。内部任何错误都 log 并吞掉，不影响主循环。
    async fn handle(&self, msg: InboundMessage) {
        let conv = msg.conv_id.clone();
        let sender = msg.sender.clone();
        let hint = msg.reply_hint.clone();

        // best-effort 指标：入站消息计数（失败只 warn 不阻断）。
        METRICS.messages_in.inc();
        // 1. 发现态：白名单为空。不自动授权（安全），对 sender 回引导消息，
        //    告知其 sender id 与如何联系管理员，不驱动 agent。
        if self.auth.is_discovery() {
            info!(
                target: "imagent::discovery",
                conv_id = %conv.0,
                sender = %sender.0,
                text = ?msg.text,
                "discovery 模式：记录 sender，回引导"
            );
            let guide = format!(
                "发现模式：当前白名单为空。你的 sender id 是 `{}`。\n\
                 请管理员在本地运行 `imagent allow {}` 授权后重启 imagent。",
                sender.0, sender.0
            );
            self.reply(&conv, &guide, &hint).await;
            return;
        }

        // 2. 白名单：非白名单 sender 丢弃。
        if !self.auth.is_allowed(&sender) {
            warn!(
                target: "imagent::core",
                conv_id = %conv.0,
                sender = %sender.0,
                "非白名单 sender，丢弃"
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
                        // 删除该 conv 的 session 行（下次新建），失败仅 log。
                        if let Err(e) = self.store.delete_session(&conv.0).await {
                            warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "delete_session 失败");
                        }
                        // 清当前活动命名 → 回到默认未命名 session。
                        if let Err(e) = self.store.delete_config(&active_name_key(&conv.0)).await {
                            warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "delete_config(active_name) 失败");
                        }
                        self.reply(
                            &conv,
                            "已重置会话，下一条消息将开启新会话（默认未命名）。",
                            &hint,
                        )
                        .await;
                        return;
                    }
                    "/allow" => {
                        let target = parts.get(1).map(|s| s.trim()).unwrap_or("");
                        if target.is_empty() {
                            self.reply(&conv, "用法: /allow <sender_id>", &hint).await;
                        } else {
                            let actor = sender.0.as_str();
                            let added = self.auth.allow(target);
                            if let Err(e) = self
                                .store
                                .add_allowed_sender(target, Some(actor), Some("im"))
                                .await
                            {
                                warn!(target: "imagent::core", error = %e, "add_allowed_sender 失败");
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
                            let text_out = if added {
                                format!("已授权 `{target}`。")
                            } else {
                                format!("`{target}` 已在白名单。")
                            };
                            self.reply(&conv, &text_out, &hint).await;
                        }
                        return;
                    }
                    "/disallow" => {
                        let target = parts.get(1).map(|s| s.trim()).unwrap_or("");
                        if target.is_empty() {
                            self.reply(&conv, "用法: /disallow <sender_id>", &hint)
                                .await;
                        } else if target == sender.0.as_str() {
                            // 防自锁：不允许撤销自己。
                            self.reply(
                                &conv,
                                "不允许撤销自己（防止锁死）。如需操作请在本地 CLI 处理。",
                                &hint,
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
                                &conv,
                                &format!(
                                    "已移除 `{target}`（{}）",
                                    if existed { "成功" } else { "原本不在" }
                                ),
                                &hint,
                            )
                            .await;
                        }
                        return;
                    }
                    "/list" => {
                        let snap = self.auth.snapshot();
                        let msg = if snap.is_empty() {
                            "白名单为空。".to_string()
                        } else {
                            format!("白名单（{}）：{}", snap.len(), snap.join(", "))
                        };
                        self.reply(&conv, &msg, &hint).await;
                        return;
                    }
                    "/whoami" => {
                        self.reply(&conv, &format!("你的 sender id：`{}`", sender.0), &hint)
                            .await;
                        return;
                    }
                    "/switch" => {
                        let name = parts.get(1).map(|s| s.trim()).unwrap_or("");
                        if name.is_empty() {
                            self.reply(&conv, "用法: /switch <name>", &hint).await;
                            return;
                        }
                        let key = active_name_key(&conv.0);
                        match self.store.get_named_session(&conv.0, name).await {
                            Ok(Some(row)) => {
                                // 切回历史命名 session：把它写成活动 session（续接用）。
                                let now = now_secs();
                                let sr = SessionRow {
                                    conv_id: conv.0.clone(),
                                    session_id: row.session_id.clone(),
                                    agent_kind: row
                                        .agent_kind
                                        .unwrap_or_else(|| self.backend.name().to_string()),
                                    workdir: row.workdir.unwrap_or_else(|| {
                                        self.default_workdir.to_string_lossy().to_string()
                                    }),
                                    name: Some(name.into()),
                                    created_at: row.created_at,
                                    updated_at: now,
                                };
                                if let Err(e) = self.store.upsert_session(&sr).await {
                                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "upsert_session 失败");
                                }
                                if let Err(e) = self.store.set_config(&key, name).await {
                                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "set_config(active_name) 失败");
                                }
                                let sid_short: String = row.session_id.chars().take(8).collect();
                                self.reply(
                                    &conv,
                                    &format!("已切换到「{name}」（session {sid_short}…）"),
                                    &hint,
                                )
                                .await;
                            }
                            Ok(None) => {
                                // 新命名 session：清活动 session（下次新建）+ 设 active_name。
                                if let Err(e) = self.store.delete_session(&conv.0).await {
                                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "delete_session 失败");
                                }
                                if let Err(e) = self.store.set_config(&key, name).await {
                                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "set_config(active_name) 失败");
                                }
                                self.reply(
                                    &conv,
                                    &format!("已切到新会话「{name}」，下一条消息将开启。"),
                                    &hint,
                                )
                                .await;
                            }
                            Err(e) => {
                                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "get_named_session 失败");
                                self.reply(&conv, "查询失败，请重试。", &hint).await;
                            }
                        }
                        return;
                    }
                    "/sessions" => {
                        match self.store.list_named_sessions(&conv.0).await {
                            Ok(rows) if rows.is_empty() => {
                                self.reply(&conv, "无命名会话（用 /switch <name> 创建）。", &hint)
                                    .await;
                            }
                            Ok(rows) => {
                                let active = self
                                    .store
                                    .get_config(&active_name_key(&conv.0))
                                    .await
                                    .unwrap_or(None)
                                    .unwrap_or_default();
                                let mut lines = String::from("命名会话：");
                                for r in &rows {
                                    let mark = if r.name == active { " *" } else { "" };
                                    let sid: String = r.session_id.chars().take(8).collect();
                                    lines.push_str(&format!(
                                        "\n  {}{} (session {}…)",
                                        r.name, mark, sid
                                    ));
                                }
                                self.reply(&conv, &lines, &hint).await;
                            }
                            Err(e) => {
                                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "list_named_sessions 失败");
                            }
                        }
                        return;
                    }
                    "/compact" => {
                        let key = compact_summary_key(&conv.0);
                        let existing_sid: Option<SessionId> =
                            match self.store.get_session(&conv.0).await {
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
                                self.reply(&conv, "当前无活动会话可压缩。", &hint).await;
                            }
                            Some(sid) => {
                                // 用 claude resume 当前 session 生成摘要；只取 Final/RunOutcome。
                                let (tx, mut rx) = mpsc::channel::<AgentChunk>(32);
                                let backend = self.backend.clone();
                                let workdir = self.default_workdir.clone();
                                let tools = self.allowed_tools.read().clone();
                                let conv_id_compact = conv.0.clone();
                                let join = tokio::spawn(async move {
                                    backend
                                        .run(
                                            &conv_id_compact,
                                            "请用中文简洁总结当前对话的要点、已做决定与未完成事项（不超过 400 字）。",
                                            Some(&sid),
                                            &workdir,
                                            &tools,
                                            tx,
                                        )
                                        .await
                                });
                                let mut summary: Option<String> = None;
                                while let Some(chunk) = rx.recv().await {
                                    if let AgentChunk::Final(t) = chunk {
                                        summary = Some(t);
                                    }
                                }
                                let summary_text = match join.await {
                                    Ok(Ok(o)) => summary.unwrap_or(o.final_text),
                                    Ok(Err(e)) => {
                                        warn!(
                                            target: "imagent::core",
                                            conv_id = %conv.0,
                                            error = %e,
                                            "compact 摘要生成失败"
                                        );
                                        self.reply(&conv, &format!("生成摘要失败：{e}"), &hint)
                                            .await;
                                        return;
                                    }
                                    Err(e) => {
                                        warn!(
                                            target: "imagent::core",
                                            conv_id = %conv.0,
                                            error = %e,
                                            "compact 摘要任务 panic"
                                        );
                                        self.reply(&conv, &format!("摘要任务异常：{e}"), &hint)
                                            .await;
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
                                if let Err(e) =
                                    self.store.delete_config(&active_name_key(&conv.0)).await
                                {
                                    warn!(
                                        target: "imagent::core",
                                        conv_id = %conv.0,
                                        error = %e,
                                        "compact: delete_config(active_name) 失败"
                                    );
                                }
                                self.reply(
                                    &conv,
                                    &format!(
                                        "已压缩会话。摘要：\n\n{summary_text}\n\n（新会话将保留此摘要延续上下文）"
                                    ),
                                    &hint,
                                )
                                .await;
                            }
                        }
                        return;
                    }
                    _ => {
                        self.reply(
                            &conv,
                            &format!(
                                "未知命令: {cmd}（支持: /new /allow /disallow /list /whoami /switch /sessions /compact）"
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
        let base_prompt = msg.text.clone().unwrap_or_default();
        // 文本与媒体皆空才丢弃；媒体消息（无文本）仍驱动 agent。
        if base_prompt.trim().is_empty() && msg.media.is_empty() {
            return;
        }

        // per-conv 串行锁：保证同一会话的 agent 任务串行。
        let lock = {
            let mut map = self.conv_locks.lock().await;
            map.entry(conv.0.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        // best-effort typing 指示（agent 处理中）；失败仅 log，不阻塞后续。
        let _ = self.platform.send_typing(&conv, &hint).await;

        // 取续接 session；store 错误仅 log 后当 None。
        let existing: Option<SessionId> = match self.store.get_session(&conv.0).await {
            Ok(Some(row)) => Some(SessionId(row.session_id)),
            Ok(None) => None,
            Err(e) => {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "get_session 失败，按新建处理");
                None
            }
        };

        // 媒体提示：把本地媒体路径前置告知 agent（claude 可 Read 本地文件）。
        let media_hint = if msg.media.is_empty() {
            String::new()
        } else {
            let lines: Vec<String> = msg
                .media
                .iter()
                .map(|m| format!("- {}：{}", m.kind, m.url))
                .collect();
            format!("【用户发来媒体】\n{}\n\n——\n\n", lines.join("\n"))
        };

        // 新建 session（无 existing）时，一次性注入压缩摘要作为前情摘要。
        let mut prompt = base_prompt;
        if existing.is_none() {
            if let Ok(Some(summary)) = self.store.get_config(&compact_summary_key(&conv.0)).await {
                if !summary.is_empty() {
                    prompt = format!("【前情摘要】{summary}\n\n——\n\n{prompt}");
                    let _ = self
                        .store
                        .delete_config(&compact_summary_key(&conv.0))
                        .await;
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
        let workdir = self.default_workdir.clone();
        let tools = self.allowed_tools.read().clone();
        let prompt_owned = prompt.clone();
        let conv_id_owned = conv.0.clone();
        let join = tokio::spawn(async move {
            backend
                .run(
                    &conv_id_owned,
                    &prompt_owned,
                    existing.as_ref(),
                    &workdir,
                    &tools,
                    tx,
                )
                .await
        });
        // 收集 chunks：Final/Error 落库，ToolUse 累积用于最终工具摘要。
        let mut final_text: Option<String> = None;
        let mut error_text: Option<String> = None;
        let mut tool_calls: Vec<(String, String)> = Vec::new();
        while let Some(chunk) = rx.recv().await {
            match chunk {
                AgentChunk::Final(t) => final_text = Some(t),
                AgentChunk::Error(e) => error_text = Some(e),
                AgentChunk::ToolUse { tool, input } => {
                    tool_calls.push((tool, truncate_str(&input, 40)));
                }
                AgentChunk::ToolResult { .. } => {} // 摘要只列工具调用，结果不进 IM
                AgentChunk::Text(_) => {}
            }
        }

        // 等待 backend 返回 RunOutcome。
        let outcome = match join.await {
            Ok(Ok(o)) => {
                let elapsed = run_started.elapsed().as_secs_f64();
                METRICS.claude_calls.inc();
                METRICS.claude_duration.observe(elapsed);
                o
            }
            Ok(Err(e)) => {
                METRICS.claude_errors.inc();
                let m = format!("[error] {e}");
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "backend.run 失败");
                self.reply(&conv, &m, &hint).await;
                return;
            }
            Err(e) => {
                METRICS.claude_errors.inc();
                let m = format!("[error] backend task panicked: {e}");
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "backend task panic");
                self.reply(&conv, &m, &hint).await;
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
        // 工具调用摘要：仅在正常 final 分支附加（不在 backend 错误回复上附加）。
        if !tool_calls.is_empty() && (final_text_is_present || outcome_has_final) {
            reply.push_str(&format_tool_summary(&tool_calls));
        }
        self.reply(&conv, &reply, &hint).await;

        // 落库（upsert 内部保留 created_at；store 错误仅 log）。
        let now = now_secs();
        // 当前活动命名（不存在/空 = 默认未命名）。
        let active_name = self
            .store
            .get_config(&active_name_key(&conv.0))
            .await
            .unwrap_or(None)
            .filter(|s| !s.is_empty());
        let row = SessionRow {
            conv_id: conv.0.clone(),
            session_id: outcome.session_id.0.clone(),
            agent_kind: self.backend.name().to_string(),
            workdir: self.default_workdir.to_string_lossy().to_string(),
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
                workdir: Some(self.default_workdir.to_string_lossy().to_string()),
                created_at: now,
                updated_at: now,
            };
            if let Err(e) = self.store.upsert_named_session(&nrow).await {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "upsert_named_session 失败");
            }
        }
    }

    /// 回传文本；发送失败仅 log。session 过期升级为 error（用户侧已收不到回复）。
    async fn reply(&self, conv: &ConvId, text: &str, hint: &ReplyHint) {
        match self.platform.send_text(conv, text, hint).await {
            Ok(()) => METRICS.messages_out.inc(),
            Err(e) => {
                if is_session_expired_err(&e) {
                    tracing::error!(
                        target: "imagent::core",
                        conv_id = %conv.0,
                        error = %e,
                        "send_text session 过期（用户侧已收不到）"
                    );
                } else {
                    warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "send_text 失败");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ConvId, ReplyHint, SessionId, UserId};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Mutex as TokioMutex;
    /// 串行化 dispatch 集成测试：避免并行开 /tmp WAL sqlite 触发 SQLITE_IOERR(1802)。
    static SERIAL: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));
    #[test]
    fn is_session_expired_err_classifies() {
        use crate::error::CoreError;
        assert!(is_session_expired_err(&CoreError::Platform(
            "ilink",
            "session expired: re-login required".into()
        )));
        assert!(is_session_expired_err(&CoreError::Platform(
            "ilink",
            "session expired, please re-login".into()
        )));
        assert!(!is_session_expired_err(&CoreError::Platform(
            "ilink",
            "getupdates exhausted retries".into()
        )));
        assert!(!is_session_expired_err(&CoreError::Config("bad".into())));
        assert!(!is_session_expired_err(&CoreError::Store(
            imagent_store::StoreError::Other("db: some failure".into())
        )));
        // 大小写无关
        assert!(is_session_expired_err(&CoreError::Platform(
            "ilink",
            "Session Expired".into()
        )));
    }

    type InboxHandle = Arc<TokioMutex<Vec<String>>>;
    type CounterHandle = Arc<AtomicUsize>;
    type CallsHandle = Arc<TokioMutex<Vec<Option<String>>>>;
    type PromptsHandle = Arc<TokioMutex<Vec<String>>>;

    // ---------- mock platform ----------

    /// mock platform：`inbox` 收到的出站文本，`recv_queue` 可编程的入站流。
    struct MockPlatform {
        recv_queue: Arc<TokioMutex<Option<Vec<InboundMessage>>>>,
        inbox: Arc<TokioMutex<Vec<String>>>,
        send_count: Arc<AtomicUsize>,
    }

    impl MockPlatform {
        fn new() -> (Self, InboxHandle, CounterHandle) {
            let inbox = Arc::new(TokioMutex::new(Vec::new()));
            let send_count = Arc::new(AtomicUsize::new(0));
            let p = Self {
                recv_queue: Arc::new(TokioMutex::new(None)),
                inbox: inbox.clone(),
                send_count: send_count.clone(),
            };
            (p, inbox, send_count)
        }
    }

    #[async_trait]
    impl Platform for MockPlatform {
        async fn recv(&self) -> Result<InboundMessage> {
            loop {
                let mut q = self.recv_queue.lock().await;
                if let Some(list) = q.as_mut() {
                    if !list.is_empty() {
                        return Ok(list.remove(0));
                    }
                }
                drop(q);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        async fn send_text(&self, _conv: &ConvId, text: &str, _hint: &ReplyHint) -> Result<()> {
            self.inbox.lock().await.push(text.to_string());
            self.send_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn send_media(
            &self,
            _conv: &ConvId,
            _media: &crate::types::MediaRef,
            _hint: &ReplyHint,
        ) -> Result<()> {
            Ok(())
        }
        fn name(&self) -> &'static str {
            "mock"
        }
    }

    // ---------- mock backend ----------

    struct MockBackend {
        calls: Arc<TokioMutex<Vec<Option<String>>>>,
        prompts: Arc<TokioMutex<Vec<String>>>,
        order: Arc<AtomicUsize>,
        /// 每次 run 前发这些 ToolUse chunk（用于工具摘要测试）。默认空。
        tools_to_emit: Arc<TokioMutex<Vec<(String, String)>>>,
    }

    impl MockBackend {
        fn new() -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
            let calls = Arc::new(TokioMutex::new(Vec::new()));
            let prompts = Arc::new(TokioMutex::new(Vec::new()));
            let order = Arc::new(AtomicUsize::new(0));
            let b = Self {
                calls: calls.clone(),
                prompts: prompts.clone(),
                order: order.clone(),
                tools_to_emit: Arc::new(TokioMutex::new(Vec::new())),
            };
            (b, calls, prompts, order)
        }

        /// 返回带可配置 ToolUse 发射的 backend，以及设置 tool 列表的句柄。
        async fn new_with_tools(
            tools: Vec<(String, String)>,
        ) -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
            let (b, calls, prompts, order) = Self::new();
            *b.tools_to_emit.lock().await = tools;
            (b, calls, prompts, order)
        }
    }

    #[async_trait]
    impl Backend for MockBackend {
        async fn run(
            &self,
            _conv_id: &str,
            prompt: &str,
            session: Option<&SessionId>,
            _workdir: &std::path::Path,
            _allowed_tools: &[String],
            chunks: mpsc::Sender<AgentChunk>,
        ) -> Result<crate::types::RunOutcome> {
            // 记录续接情况 + 执行顺序 + 收到的 prompt。
            let my_order = self.order.fetch_add(1, Ordering::SeqCst);
            self.calls.lock().await.push(session.map(|s| s.0.clone()));
            self.prompts.lock().await.push(prompt.to_string());

            // 稍微让出调度器，便于测试串行。
            tokio::task::yield_now().await;

            // 先发配置好的 ToolUse chunk（若有），再发 Final。
            let tools = self.tools_to_emit.lock().await.clone();
            for (tool, input) in tools {
                let _ = chunks.send(AgentChunk::ToolUse { tool, input }).await;
            }
            // 发一个 Final chunk。
            let _ = chunks
                .send(AgentChunk::Final(format!("reply#{my_order}")))
                .await;

            Ok(crate::types::RunOutcome {
                session_id: SessionId(format!("sess-{my_order}")),
                final_text: format!("final-{my_order}"),
            })
        }
        fn name(&self) -> &'static str {
            "mock-backend"
        }
    }

    // ---------- helpers ----------

    fn msg(conv: &str, sender: &str, text: &str) -> InboundMessage {
        InboundMessage {
            conv_id: ConvId(conv.into()),
            sender: UserId(sender.into()),
            text: Some(text.into()),
            media: Vec::new(),
            reply_hint: ReplyHint::None,
        }
    }

    async fn tmp_store() -> (Store, std::path::PathBuf) {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "imagent_core_dispatch_{}_{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(&p).await.expect("open store");
        (store, p)
    }

    /// 构造 dispatcher 并返回各观测句柄。`check()` 每次返回一个指向同一 db 文件的
    /// 新 Store 连接（Store 未 impl Clone；rusqlite WAL 支持多连接，断言用）。
    struct Ctx {
        disp: Arc<Dispatcher>,
        inbox: Arc<TokioMutex<Vec<String>>>,
        send_count: Arc<AtomicUsize>,
        calls: Arc<TokioMutex<Vec<Option<String>>>>,
        prompts: Arc<TokioMutex<Vec<String>>>,
        order: Arc<AtomicUsize>,
        db: std::path::PathBuf,
    }

    impl Ctx {
        async fn check(&self) -> Store {
            Store::open(&self.db).await.expect("reopen store")
        }
    }

    async fn build(auth: Auth) -> Ctx {
        let (plat, inbox, send_count) = MockPlatform::new();
        let (back, calls, prompts, order) = MockBackend::new();
        let (store, db) = tmp_store().await;

        let disp = Arc::new(Dispatcher::new(
            Arc::new(plat),
            Arc::new(back),
            store,
            auth,
            std::path::PathBuf::from("/tmp/imagent-test-ws"),
            vec!["Read".into(), "Edit".into()],
            PermissionMode::Off,
        ));

        Ctx {
            disp,
            inbox,
            send_count,
            calls,
            prompts,
            order,
            db,
        }
    }

    /// 与 build 相同，但 MockBackend 在 Final 前会发指定的 ToolUse chunk。
    async fn build_with_tools(auth: Auth, tools: Vec<(String, String)>) -> Ctx {
        let (plat, inbox, send_count) = MockPlatform::new();
        let (back, calls, prompts, order) = MockBackend::new_with_tools(tools).await;
        let (store, db) = tmp_store().await;

        let disp = Arc::new(Dispatcher::new(
            Arc::new(plat),
            Arc::new(back),
            store,
            auth,
            std::path::PathBuf::from("/tmp/imagent-test-ws"),
            vec!["Read".into(), "Edit".into()],
            PermissionMode::Off,
        ));

        Ctx {
            disp,
            inbox,
            send_count,
            calls,
            prompts,
            order,
            db,
        }
    }

    /// 把消息喂给 dispatcher 的 mock platform recv，并等待处理完成。
    async fn feed_and_wait(ctx: &Ctx, msgs: Vec<InboundMessage>, want_calls: usize) {
        // 通过 downcast 不便，这里改为直接调用 handle（绕过 run/recv）。
        for m in msgs {
            let disp = ctx.disp.clone();
            // 直接 await handle，串行执行（handle 内部已有 per-conv 锁）。
            disp.handle(m).await;
        }
        // 等待到 calls 计数达到预期。
        for _ in 0..400 {
            if ctx.order.load(Ordering::SeqCst) >= want_calls {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn drop_db(p: std::path::PathBuf) {
        let _ = std::fs::remove_file(&p);
        let mut w = p.clone();
        w.set_extension("sqlite-wal");
        let _ = std::fs::remove_file(&w);
        let mut s = p.clone();
        s.set_extension("sqlite-shm");
        let _ = std::fs::remove_file(&s);
    }

    // ---------- tests ----------

    #[tokio::test]
    async fn normal_message_runs_backend_and_replies_and_persists() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        feed_and_wait(&ctx, vec![msg("c1", "alice", "hello")], 1).await;

        // 回传收到（Final 优先）。
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.starts_with("reply#")),
            "inbox={inbox:?}"
        );

        // session 落库且 id 正确。
        let row = ctx
            .check()
            .await
            .get_session("c1")
            .await
            .unwrap()
            .expect("session row");
        assert_eq!(row.session_id, "sess-0");
        assert_eq!(row.agent_kind, "mock-backend");
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn second_message_continues_previous_session() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        feed_and_wait(
            &ctx,
            vec![msg("c2", "alice", "first"), msg("c2", "alice", "second")],
            2,
        )
        .await;

        let calls = ctx.calls.lock().await.clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], None, "first should be new session");
        assert_eq!(calls[1].as_deref(), Some("sess-0"), "second should resume");
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn discovery_mode_skips_backend_but_replies_guide() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec![])).await; // 发现模式
        feed_and_wait(&ctx, vec![msg("c3", "anyone", "hi")], 0).await;

        // backend 未被调用。
        assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
        // 但回传了引导消息，且其中含 sender id。
        let inbox = ctx.inbox.lock().await.clone();
        assert_eq!(inbox.len(), 1, "应回一条引导，inbox={inbox:?}");
        assert!(inbox[0].contains("anyone"), "引导消息应含 sender id");
        assert!(inbox[0].contains("imagent allow"), "引导消息应含 CLI 指引");
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn non_allowlisted_sender_dropped() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["someone_else".into()])).await;
        feed_and_wait(&ctx, vec![msg("c4", "intruder", "hi")], 0).await;

        assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
        assert_eq!(ctx.send_count.load(Ordering::SeqCst), 0);
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn slash_new_resets_session() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 先发一条普通消息建立 session。
        feed_and_wait(&ctx, vec![msg("c5", "alice", "hello")], 1).await;
        assert!(ctx.check().await.get_session("c5").await.unwrap().is_some());

        // 发 /new：应删除 session 并回 IM。
        let before_sends = ctx.send_count.load(Ordering::SeqCst);
        feed_and_wait(&ctx, vec![msg("c5", "alice", "/new")], 1).await;
        // /new 不触发 backend，order 不变。
        assert_eq!(ctx.order.load(Ordering::SeqCst), 1);
        // session 已删除。
        assert!(ctx.check().await.get_session("c5").await.unwrap().is_none());
        // 回传了一条重置提示。
        let after_sends = ctx.send_count.load(Ordering::SeqCst);
        assert_eq!(after_sends, before_sends + 1);

        // 下一条普通消息 backend 收到的 session 是 None。
        feed_and_wait(&ctx, vec![msg("c5", "alice", "fresh start")], 2).await;
        let calls = ctx.calls.lock().await.clone();
        // 最后一次调用 session 应为 None（fresh start 新建）。
        assert_eq!(calls.last(), Some(&None));
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn unknown_slash_command_replies() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        let before = ctx.send_count.load(Ordering::SeqCst);
        feed_and_wait(&ctx, vec![msg("c6", "alice", "/foo bar")], 0).await;
        let after = ctx.send_count.load(Ordering::SeqCst);
        assert_eq!(after, before + 1);
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("未知命令") && t.contains("/foo")),
            "inbox={inbox:?}"
        );
        // backend 未被调用。
        assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn per_conv_serial_order() {
        // 同一 conv 连发三条；mock backend 内 fetch_add 顺序应递增。
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        feed_and_wait(
            &ctx,
            vec![
                msg("c7", "alice", "a"),
                msg("c7", "alice", "b"),
                msg("c7", "alice", "c"),
            ],
            3,
        )
        .await;

        let calls = ctx.calls.lock().await.clone();
        // 三条依次执行；session 链：None -> sess-0 -> sess-1。
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0], None);
        assert_eq!(calls[1].as_deref(), Some("sess-0"));
        assert_eq!(calls[2].as_deref(), Some("sess-1"));
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn allow_command_grants_then_bob_can_drive() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;

        // alice 发 /allow bob：应回「已授权」。
        feed_and_wait(&ctx, vec![msg("c8", "alice", "/allow bob")], 0).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("已授权") && t.contains("bob")),
            "inbox={inbox:?}"
        );

        // 白名单持久化到 store。
        let stored = ctx.check().await.list_allowed_senders().await.unwrap();
        assert!(stored.iter().any(|s| s == "bob"), "stored={stored:?}");

        // bob（刚被授权）现在能驱动 backend。
        feed_and_wait(&ctx, vec![msg("c9", "bob", "hello")], 1).await;
        assert_eq!(ctx.order.load(Ordering::SeqCst), 1, "bob 应能驱动 backend");
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn list_command_replies_whitelist() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into(), "carol".into()])).await;
        feed_and_wait(&ctx, vec![msg("c10", "alice", "/list")], 0).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("alice") && t.contains("carol") && t.contains("白名单")),
            "inbox={inbox:?}"
        );
        // backend 未被调用。
        assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn whoami_command_replies_sender() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        feed_and_wait(&ctx, vec![msg("c11", "alice", "/whoami")], 0).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(inbox.iter().any(|t| t.contains("alice")), "inbox={inbox:?}");
        assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn disallow_cannot_revoke_self() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        feed_and_wait(&ctx, vec![msg("c12", "alice", "/disallow alice")], 0).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.contains("不允许撤销自己")),
            "inbox={inbox:?}"
        );
        // alice 仍在白名单。
        assert!(ctx.disp.auth.is_allowed(&UserId("alice".into())));
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn disallow_command_removes_target() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into(), "bob".into()])).await;
        feed_and_wait(&ctx, vec![msg("c13", "alice", "/disallow bob")], 0).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox
                .iter()
                .any(|t| t.contains("已移除") && t.contains("bob")),
            "inbox={inbox:?}"
        );
        // bob 已被移除：后续消息被丢弃，不驱动 backend。
        feed_and_wait(&ctx, vec![msg("c14", "bob", "still here?")], 0).await;
        assert_eq!(ctx.order.load(Ordering::SeqCst), 0, "bob 应已被移出白名单");
        drop_db(ctx.db).await;
    }
    #[tokio::test]
    async fn switch_new_name_clears_session_and_sets_active() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 先建立默认 session。
        feed_and_wait(&ctx, vec![msg("s1", "alice", "hello")], 1).await;
        assert!(ctx.check().await.get_session("s1").await.unwrap().is_some());

        // /switch newtask（命名不存在）→ 活动清空 + active_name 设。
        feed_and_wait(&ctx, vec![msg("s1", "alice", "/switch newtask")], 1).await;
        assert!(
            ctx.check().await.get_session("s1").await.unwrap().is_none(),
            "switch 新命名后活动 session 应清空"
        );
        assert_eq!(
            ctx.check()
                .await
                .get_config("active_name:s1")
                .await
                .unwrap(),
            Some("newtask".to_string())
        );

        // 下一条普通消息：backend 收到 None（新建），并落 named_sessions(newtask)。
        feed_and_wait(&ctx, vec![msg("s1", "alice", "do work")], 2).await;
        let calls = ctx.calls.lock().await.clone();
        assert_eq!(calls.last(), Some(&None), "switch 后首条消息应新建 session");

        let nrow = ctx
            .check()
            .await
            .get_named_session("s1", "newtask")
            .await
            .unwrap()
            .expect("named row");
        assert_eq!(nrow.name, "newtask");
        // 活动 session 行 name 也带命名。
        let srow = ctx
            .check()
            .await
            .get_session("s1")
            .await
            .unwrap()
            .expect("session row");
        assert_eq!(srow.name.as_deref(), Some("newtask"));
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn switch_existing_name_resumes_named_session() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 建立命名 session `taskA`。
        feed_and_wait(&ctx, vec![msg("s2", "alice", "/switch taskA")], 0).await;
        feed_and_wait(&ctx, vec![msg("s2", "alice", "first taskA work")], 1).await;
        let nrow = ctx
            .check()
            .await
            .get_named_session("s2", "taskA")
            .await
            .unwrap()
            .expect("named row taskA");
        let task_a_sid = nrow.session_id.clone();

        // 切到默认（/new 清 active_name）再发消息建立另一个默认 session。
        feed_and_wait(&ctx, vec![msg("s2", "alice", "/new")], 1).await;
        feed_and_wait(&ctx, vec![msg("s2", "alice", "default work")], 2).await;

        // /switch taskA（命名已存在）→ 活动 session 被写成 taskA 的 session_id。
        feed_and_wait(&ctx, vec![msg("s2", "alice", "/switch taskA")], 2).await;
        let srow = ctx
            .check()
            .await
            .get_session("s2")
            .await
            .unwrap()
            .expect("session row");
        assert_eq!(
            srow.session_id, task_a_sid,
            "switch 已存在命名应 resume 其 session_id"
        );
        assert_eq!(srow.name.as_deref(), Some("taskA"));
        assert_eq!(
            ctx.check()
                .await
                .get_config("active_name:s2")
                .await
                .unwrap(),
            Some("taskA".to_string())
        );

        // 下一条普通消息应续接 taskA 的 session_id。
        feed_and_wait(&ctx, vec![msg("s2", "alice", "continue")], 3).await;
        let calls = ctx.calls.lock().await.clone();
        assert_eq!(
            calls.last(),
            Some(&Some(task_a_sid)),
            "switch 后续消息应续接命名 session"
        );
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn sessions_command_lists_named_with_active_mark() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 空时 /sessions。
        feed_and_wait(&ctx, vec![msg("s3", "alice", "/sessions")], 0).await;
        assert!(
            ctx.inbox
                .lock()
                .await
                .last()
                .unwrap()
                .contains("无命名会话"),
            "空命名时应提示无"
        );

        // 建两个命名 session。
        feed_and_wait(&ctx, vec![msg("s3", "alice", "/switch alpha")], 0).await;
        feed_and_wait(&ctx, vec![msg("s3", "alice", "a work")], 1).await;
        feed_and_wait(&ctx, vec![msg("s3", "alice", "/switch beta")], 1).await;
        feed_and_wait(&ctx, vec![msg("s3", "alice", "b work")], 2).await;

        // /sessions：应列出 alpha、beta，当前活动 beta 标 *。
        feed_and_wait(&ctx, vec![msg("s3", "alice", "/sessions")], 2).await;
        let inbox = ctx.inbox.lock().await.clone();
        let listing = inbox.last().unwrap();
        assert!(listing.contains("命名会话"), "listing={listing}");
        assert!(listing.contains("alpha"), "listing={listing}");
        assert!(listing.contains("beta"), "listing={listing}");
        // beta 为活动，应带 `*`；alpha 不应带。
        assert!(
            listing.contains("beta *"),
            "活动命名应带 *，listing={listing}"
        );
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn new_command_clears_active_name() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 建命名 session。
        feed_and_wait(&ctx, vec![msg("s4", "alice", "/switch build")], 0).await;
        feed_and_wait(&ctx, vec![msg("s4", "alice", "go")], 1).await;
        assert_eq!(
            ctx.check()
                .await
                .get_config("active_name:s4")
                .await
                .unwrap(),
            Some("build".to_string())
        );

        // /new 清活动 session 与 active_name。
        feed_and_wait(&ctx, vec![msg("s4", "alice", "/new")], 1).await;
        assert!(
            ctx.check()
                .await
                .get_config("active_name:s4")
                .await
                .unwrap()
                .is_none(),
            "/new 后 active_name 应被清除"
        );
        assert!(ctx.check().await.get_session("s4").await.unwrap().is_none());

        // 下一条普通消息：name 应为 None（默认未命名）。
        feed_and_wait(&ctx, vec![msg("s4", "alice", "fresh")], 2).await;
        let srow = ctx
            .check()
            .await
            .get_session("s4")
            .await
            .unwrap()
            .expect("row");
        assert!(srow.name.is_none(), "/new 后新 session 应未命名");
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn compact_with_active_session_generates_summary_and_resets() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 先建立活动 session。
        feed_and_wait(&ctx, vec![msg("k1", "alice", "hello")], 1).await;
        assert!(ctx.check().await.get_session("k1").await.unwrap().is_some());

        // /compact：应 resume 当前 session 生成摘要，order +1。
        feed_and_wait(&ctx, vec![msg("k1", "alice", "/compact")], 2).await;

        // backend 被调用，且 session 为 Some（resume）。
        let calls = ctx.calls.lock().await.clone();
        assert_eq!(calls.len(), 2, "calls={calls:?}");
        assert_eq!(
            calls[1].as_deref(),
            Some("sess-0"),
            "/compact 应 resume 当前 session"
        );

        // 回传含「摘要」字样。
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.contains("摘要")),
            "应回摘要提示，inbox={inbox:?}"
        );

        // 摘要已落库。
        assert_eq!(
            ctx.check()
                .await
                .get_config("compact_summary:k1")
                .await
                .unwrap(),
            Some("reply#1".to_string()),
            "摘要应为 Final chunk 文本"
        );

        // 活动 session 已删除。
        assert!(
            ctx.check().await.get_session("k1").await.unwrap().is_none(),
            "/compact 后活动 session 应删除"
        );
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn compact_without_active_session_replies_nothing() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 无活动 session 直接 /compact。
        feed_and_wait(&ctx, vec![msg("k2", "alice", "/compact")], 0).await;

        // backend 未被调用。
        assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
        // 回传「无活动会话」。
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.contains("无活动会话")),
            "inbox={inbox:?}"
        );
        // 摘要未落库。
        assert!(ctx
            .check()
            .await
            .get_config("compact_summary:k2")
            .await
            .unwrap()
            .is_none());
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn compact_summary_injected_once_for_new_session() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 预置摘要 + 无活动 session。
        ctx.check()
            .await
            .set_config("compact_summary:k3", "之前讨论了 X 与 Y")
            .await
            .unwrap();
        assert!(ctx.check().await.get_session("k3").await.unwrap().is_none());

        // 发普通消息（无 existing）→ 应注入摘要。
        feed_and_wait(&ctx, vec![msg("k3", "alice", "继续")], 1).await;
        let prompts = ctx.prompts.lock().await.clone();
        assert_eq!(prompts.len(), 1, "prompts={prompts:?}");
        assert!(
            prompts[0].contains("【前情摘要】"),
            "新建 session 应注入摘要，prompt={}",
            prompts[0]
        );
        assert!(
            prompts[0].ends_with("继续"),
            "原始 prompt 应在末尾，prompt={}",
            prompts[0]
        );

        // 摘要一次性注入后清除。
        assert!(
            ctx.check()
                .await
                .get_config("compact_summary:k3")
                .await
                .unwrap()
                .is_none(),
            "摘要应一次性清除"
        );

        // 再发一条（现已 existing）→ 不再注入。
        feed_and_wait(&ctx, vec![msg("k3", "alice", "more")], 2).await;
        let prompts = ctx.prompts.lock().await.clone();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[1], "more", "第二条不应含摘要");
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn compact_summary_not_injected_when_session_exists() {
        let _serial = SERIAL.lock().await;
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        // 先建立活动 session。
        feed_and_wait(&ctx, vec![msg("k4", "alice", "first")], 1).await;
        assert!(ctx.check().await.get_session("k4").await.unwrap().is_some());

        // 会话已存在后再预置摘要；下一条消息 existing=Some，不应注入。
        ctx.check()
            .await
            .set_config("compact_summary:k4", "遗留摘要")
            .await
            .unwrap();
        feed_and_wait(&ctx, vec![msg("k4", "alice", "second")], 2).await;

        let prompts = ctx.prompts.lock().await.clone();
        assert_eq!(
            prompts[1], "second",
            "existing 时不应误注入，prompts={prompts:?}"
        );
        // 摘要仍存在（未被消费）。
        assert_eq!(
            ctx.check()
                .await
                .get_config("compact_summary:k4")
                .await
                .unwrap(),
            Some("遗留摘要".to_string())
        );
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn normal_message_appends_tool_summary() {
        let _serial = SERIAL.lock().await;
        let tools = vec![
            ("Read".to_string(), r#"{"path":"/foo"}"#.to_string()),
            ("Edit".to_string(), r#"{"file":"/bar"}"#.to_string()),
        ];
        let ctx = build_with_tools(Auth::new(vec!["alice".into()]), tools).await;
        feed_and_wait(&ctx, vec![msg("t1", "alice", "do it")], 1).await;

        // 回传文本含工具摘要与工具名。
        let inbox = ctx.inbox.lock().await.clone();
        let reply = inbox
            .iter()
            .find(|t| t.starts_with("reply#"))
            .expect("应有 final reply");
        assert!(reply.contains("🔧 工具调用"), "应含工具摘要标题: {reply}");
        assert!(reply.contains("Read("), "应含 Read 工具: {reply}");
        assert!(reply.contains("Edit("), "应含 Edit 工具: {reply}");
        assert!(reply.contains("/foo"), "应含工具输入: {reply}");
        drop_db(ctx.db).await;
    }

    #[tokio::test]
    async fn tool_summary_truncates_after_five() {
        let _serial = SERIAL.lock().await;
        // 6 个工具 → 截断展示 5 个并标 …(+1)。
        let tools: Vec<(String, String)> = (0..6)
            .map(|i| (format!("Tool{i}"), format!(r#"{{"k":"{i}"}}"#)))
            .collect();
        let ctx = build_with_tools(Auth::new(vec!["alice".into()]), tools).await;
        feed_and_wait(&ctx, vec![msg("t2", "alice", "go")], 1).await;

        let inbox = ctx.inbox.lock().await.clone();
        let reply = inbox
            .iter()
            .find(|t| t.starts_with("reply#"))
            .expect("应有 final reply");
        assert!(reply.contains("…(+1)"), "6 个工具应标 …(+1): {reply}");
        assert!(reply.contains("Tool0"), "应含首个工具: {reply}");
        assert!(reply.contains("Tool4"), "应含第 5 个工具: {reply}");
        drop_db(ctx.db).await;
    }
}
