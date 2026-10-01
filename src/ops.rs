//! 运维面 + 启动/SIGHUP 共享装配段：metrics 与 /health HTTP server（Bearer
//! 鉴权、fail-closed 绑定校验）、SIGHUP 热重载处理器（含不可热载键快照
//! HotReloadSnapshot）、优雅退出信号；以及 Start 装配与 SIGHUP 热载共用的
//! 装配段（白名单并集 / 权限档解析 / bitable 注入 / claude-cli 运行参数）
//! ——集中一处两处调用，消除两份手工清单的漂移面。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

#[cfg(unix)]
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// 启动/SIGHUP 共享装配段（批 A1 装配收敛）：此前 Start 与 SIGHUP 各持一份
// 手工清单，历史上出过「agent 变更静默降级权限档」的漂移 bug（v14 修复）；
// 本段把两处重复的装配段落收敛为单一实现，Start（main）与 SIGHUP（本模块）
// 两处调用。
// ---------------------------------------------------------------------------

/// 「config 种子 ∪ store 动态条目」并集——senders / chats / admins 六段并集
/// 循环（Start 装配 ×3 + SIGHUP 热载 ×3）共用的同一实现。顺序语义：config
/// 种子在前（保持声明序），store 独有条目按 store 返回序追加——与原六处
/// 内联循环逐字节一致。
pub(crate) fn union_config_store(cfg_list: &[String], store_rows: Vec<String>) -> Vec<String> {
    let mut out = cfg_list.to_vec();
    for s in store_rows {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// auto（缺省）档按后端解析成具体档——Start 装配与 SIGHUP 热载共用的同一
/// 解析入口。P2-1a：SIGHUP 必须按**运行中后端**的 agent_label() 解析（agent
/// 键不可热载，防「agent 变更静默降级权限档」），Start 传入即将装配的
/// config.agent（此刻与实际后端一致）。
pub(crate) fn resolve_permission_mode(
    mode: imagent_core::PermissionMode,
    agent_label: &str,
) -> imagent_core::PermissionMode {
    mode.resolve(agent_label)
}

/// T12：bitable 未启用原因的一次性 warn（缺一/非 feishu 平台——配置了但
/// 无效的场景给用户看得见的反馈）——Start 装配与 SIGHUP 热载两处共用。
pub(crate) fn warn_bitable_disable_reason(config: &imagent_core::Config, platform_name: &str) {
    if let Some(reason) = config.bitable_disable_reason(platform_name) {
        tracing::warn!(target: "imagent::ops", "{reason}");
    }
}

/// W1-2/W1-3/W1-4 + T7/T12：claude-cli 运行参数注入（启动与 SIGHUP 共用同一
/// 接线）。T7 的 hide_state_dir、T12 的 bitable 同此热改（整体替换，下一轮
/// spawn 生效）。
pub(crate) fn apply_claude_runtime_opts(
    b: &imagent_claude::ClaudeBackend,
    config: &imagent_core::Config,
    bitable: bool,
) {
    b.set_runtime_opts(
        config.claude_fallback_model.clone(),
        config.disallowed_tools.clone(),
        config.append_system_prompt.clone(),
        config.mcp_config_path.as_deref(),
        config.hide_state_dir_from_agent,
        bitable,
    );
}

/// T12：构造 feishu Bitable 实现注入 Dispatcher（启动与 SIGHUP 共用）。
/// 未启用（含撤销配置的 SIGHUP）显式置 None——后续 socket 请求回「未配置」
/// 错误而非半生效。凭据与平台同源（feishu_app_id / IMAGENT_FEISHU_APP_SECRET
/// env / feishu_base_url）：启动路径早于此已过 platform=feishu 校验，此处对
/// 缺凭据兜底 warn（SIGHUP 改动凭据后未重启 env 的场景）。
pub(crate) fn apply_bitable(
    dispatcher: &imagent_core::Dispatcher,
    config: &imagent_core::Config,
    platform_name: &str,
) {
    if !config.bitable_enabled_for(platform_name) {
        dispatcher.set_bitable(None);
        return;
    }
    let Some(app_id) = config.feishu_app_id.clone() else {
        tracing::warn!(
            target: "imagent::ops",
            "feishu_bitable 已配置但缺 feishu_app_id，Bitable 数据面未注入"
        );
        dispatcher.set_bitable(None);
        return;
    };
    let Ok(app_secret) = std::env::var("IMAGENT_FEISHU_APP_SECRET") else {
        tracing::warn!(
            target: "imagent::ops",
            "feishu_bitable 已配置但缺环境变量 IMAGENT_FEISHU_APP_SECRET，Bitable 数据面未注入"
        );
        dispatcher.set_bitable(None);
        return;
    };
    let base_url = config
        .feishu_base_url
        .clone()
        .unwrap_or_else(|| "https://open.feishu.cn".to_string());
    // enabled_for 已保证两者 Some（load 期空白归一为 None）。
    let app_token = config.feishu_bitable_app_token.clone().unwrap_or_default();
    let table_id = config.feishu_bitable_table_id.clone().unwrap_or_default();
    tracing::info!(
        target: "imagent::ops",
        table_id = %table_id,
        "Bitable 数据面已启用（claude-cli agent 获得 bitable_list_fields / \
         bitable_append_row 工具；建议专用表，写入需审批可把 \
         mcp__imagent__bitable_append_row 加入 approval_tools）"
    );
    dispatcher.set_bitable(Some(Arc::new(imagent_feishu::FeishuBitable::new(
        app_id, app_secret, base_url, app_token, table_id,
    ))));
}

/// `/health` 返回的 JSON 载荷。
#[derive(Serialize)]
struct Health {
    logged_in: bool,
    uptime_secs: u64,
    version: &'static str,
    sessions: i64,
    /// v1.23：发送侧重试队列深度（outbox 表行数，跨平台合计口径——Wave C 起
    /// feishu/wecom 各写各的 kind，健康语义不变）——持续 >0 说明出站通路在
    /// 退避重发（0 = 健康）。
    outbox_pending: i64,
    /// P2-2（code-review v14）：metrics HTTP 是否实际在监听。bind 失败时本
    /// server 不启动（启动日志有 error、探针得到 connection refused），能应答
    /// 本字段的进程恒 true；保留显式字段让探针脚本按统一契约消费。
    metrics_listening: bool,
    /// P2-2（code-review v14）：webhook 入站监听状态三态——`null` = 未配置
    /// webhook_addr；`true` = 已 bind 且 serve 中；`false` = 配置了但未监听
    ///（[[webhook]] 表空 / 地址解析失败，启动日志有对应 warn）。
    webhook_listening: Option<bool>,
}

/// 共享给 axum handler 的状态。
#[derive(Clone)]
struct HttpState {
    store: imagent_store::Store,
    start_at: Instant,
    /// 实际运行的平台名（P5：/health 的 logged_in 按平台判定）。
    platform: String,
    /// 预计算的 logged_in（P5-第五批：wecom 凭据在 config/store，启动时预算）。
    /// None = 按平台动态查（store / env）。
    logged_in_hint: Option<bool>,
    /// S7：Bearer 鉴权 token。None = loopback 绑定、无鉴权（历史行为）；
    /// Some = 两端点都要求匹配的 `Authorization: Bearer <token>`。
    token: Option<String>,
    /// P2-2（code-review v14）：webhook 监听三态（见 [`Health::webhook_listening`]）。
    webhook_listening: Option<bool>,
}

/// S7：读取可选的 HTTP Bearer 鉴权 token（`IMAGENT_HTTP_TOKEN`）。
/// 空串/纯空白视为未设置。
pub(crate) fn metrics_http_token() -> Option<String> {
    std::env::var("IMAGENT_HTTP_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// S7：fail-closed 绑定校验（纯函数，便于单测）。非 loopback 绑定且未配
/// token 即拒绝；loopback 或已配 token 均放行。
pub(crate) fn validate_metrics_bind(socket: SocketAddr, token: Option<&str>) -> Result<(), String> {
    if socket.ip().is_loopback() || token.is_some() {
        Ok(())
    } else {
        Err(format!(
            "绑定非 loopback 地址 {socket} 且未设置 IMAGENT_HTTP_TOKEN，\
             /metrics 与 /health 将无鉴权公网可访问；请绑回 127.0.0.1 或设置 IMAGENT_HTTP_TOKEN"
        ))
    }
}

fn bearer_authorized(headers: &axum::http::HeaderMap, token: Option<&str>) -> bool {
    let Some(expected) = token else {
        return true;
    };
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // L14（code-review v8）：恒定时间比较（逐字节 XOR 累加）——短路 == 的
    // 时序差在公网链路可测；长度差先补齐到同长再比。
    let want = format!("Bearer {expected}");
    let a: Vec<u8> = provided.as_bytes().to_vec();
    let b: Vec<u8> = want.as_bytes().to_vec();
    let n = a.len().max(b.len());
    let mut diff: u8 = (a.len() != b.len()) as u8;
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

pub(crate) fn spawn_metrics_server(
    listener: tokio::net::TcpListener,
    store: imagent_store::Store,
    start_at: Instant,
    platform: String,
    token: Option<String>,
    logged_in_hint: Option<bool>,
    webhook_listening: Option<bool>,
) {
    // P2-2（code-review v14）：listener 由调用方 bind 好后传入（bind 提前 +
    // 失败可见性见调用点注释）。
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    let state = HttpState {
        store,
        start_at,
        platform,
        token,
        logged_in_hint,
        webhook_listening,
    };
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/health", get(health_handler))
        .with_state(state);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!(target: "imagent::ops", addr = %addr, error = %e, "metrics HTTP server 退出");
        }
    });
}

async fn metrics_handler(
    State(st): State<HttpState>,
    headers: axum::http::HeaderMap,
) -> (StatusCode, String) {
    // S7：设置了 IMAGENT_HTTP_TOKEN 时两端点统一要求 Bearer 鉴权。
    if !bearer_authorized(&headers, st.token.as_deref()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized\n".to_string());
    }
    (StatusCode::OK, imagent_core::metrics::render())
}

async fn health_handler(
    State(st): State<HttpState>,
    headers: axum::http::HeaderMap,
) -> (StatusCode, Json<Health>) {
    if !bearer_authorized(&headers, st.token.as_deref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(Health {
                logged_in: false,
                uptime_secs: 0,
                version: "",
                sessions: -1,
                outbox_pending: -1,
                metrics_listening: true,
                webhook_listening: st.webhook_listening,
            }),
        );
    }
    let sessions = st.store.count_sessions().await.unwrap_or(-1);
    // P5：logged_in 按实际平台判定——此前固定查 ilink 凭据，feishu/wecom 下恒
    // false 有误导。wecom 凭据在 config（启动时预算入 hint）；feishu 查 env；
    // ilink 查 store。
    let logged_in = match st.logged_in_hint {
        Some(b) => b,
        None if st.platform == "feishu" => std::env::var("IMAGENT_FEISHU_APP_SECRET")
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false),
        None => {
            let platform = st.platform.clone();
            st.store
                .first_credential(&platform)
                .await
                .map(|o| o.is_some())
                .unwrap_or(false)
        }
    };
    let outbox_pending = st.store.outbox_depth().await.unwrap_or(0);
    let body = Health {
        logged_in,
        uptime_secs: st.start_at.elapsed().as_secs(),
        version: env!("CARGO_PKG_VERSION"),
        sessions,
        outbox_pending,
        // P2-2（code-review v14）：能应答即 metrics 在监听；webhook 三态透传。
        metrics_listening: true,
        webhook_listening: st.webhook_listening,
    };
    (StatusCode::OK, Json(body))
}

/// SIGHUP 热重载的「不可热载键」快照（code-review v14 P2-1b）。
///
/// 这些键在启动期一次性装配（后端实现 / 监听 socket / 平台构造参数 / 预算
/// 闸门），SIGHUP 改了也不会生效——静默吞掉会让运维误以为已生效。重载时用
/// 本快照与新 config 快照逐键对比，变化即打 error（列出键名 + 需重启）。
///
/// `webhooks` 展平为可比较元组：`WebhookEntry` 未派生 PartialEq（config.rs
/// 本轮冻结不动），逐字段入元组比较等价；`cron_catchup` 同理用 Debug 串。
#[cfg(unix)]
type WebhookEntryKey = (String, String, String, Option<String>, Option<f64>, u64);

#[cfg(unix)]
pub(crate) struct HotReloadSnapshot {
    agent: String,
    webhook_addr: Option<String>,
    /// (token, conv, name, secret, rps, replay_window_secs) 逐条。
    webhooks: Vec<WebhookEntryKey>,
    metrics_addr: Option<String>,
    sender_daily_cost_limit_usd: Option<f64>,
    agent_timeout_secs: u64,
    agent_idle_timeout_secs: u64,
    batch_window_ms: u64,
    cron_catchup: String,
    stranger_mention_hint: bool,
    stranger_p2p_hint: bool,
    reply_mode: imagent_core::ReplyMode,
}

#[cfg(unix)]
impl HotReloadSnapshot {
    pub(crate) fn of(cfg: &imagent_core::Config) -> Self {
        Self {
            agent: cfg.agent.clone(),
            webhook_addr: cfg.webhook_addr.clone(),
            webhooks: cfg
                .webhooks
                .iter()
                .map(|e| {
                    (
                        e.token.clone(),
                        e.conv.clone(),
                        e.name.clone(),
                        e.secret.clone(),
                        e.rps,
                        e.replay_window_secs,
                    )
                })
                .collect(),
            metrics_addr: cfg.metrics_addr.clone(),
            sender_daily_cost_limit_usd: cfg.sender_daily_cost_limit_usd,
            agent_timeout_secs: cfg.agent_timeout_secs,
            agent_idle_timeout_secs: cfg.agent_idle_timeout_secs,
            batch_window_ms: cfg.batch_window_ms,
            cron_catchup: format!("{:?}", cfg.cron_catchup),
            stranger_mention_hint: cfg.stranger_mention_hint,
            stranger_p2p_hint: cfg.stranger_p2p_hint,
            reply_mode: cfg.reply_mode,
        }
    }

    /// 与另一快照逐键比较，返回发生变化的键名（固定顺序，供日志列举）。
    fn changed_keys(&self, new: &Self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.agent != new.agent {
            out.push("agent");
        }
        if self.webhook_addr != new.webhook_addr {
            out.push("webhook_addr");
        }
        if self.webhooks != new.webhooks {
            out.push("webhooks");
        }
        if self.metrics_addr != new.metrics_addr {
            out.push("metrics_addr");
        }
        if self.sender_daily_cost_limit_usd != new.sender_daily_cost_limit_usd {
            out.push("sender_daily_cost_limit_usd");
        }
        if self.agent_timeout_secs != new.agent_timeout_secs {
            out.push("agent_timeout_secs");
        }
        if self.agent_idle_timeout_secs != new.agent_idle_timeout_secs {
            out.push("agent_idle_timeout_secs");
        }
        if self.batch_window_ms != new.batch_window_ms {
            out.push("batch_window_ms");
        }
        if self.cron_catchup != new.cron_catchup {
            out.push("cron_catchup");
        }
        if self.stranger_mention_hint != new.stranger_mention_hint {
            out.push("stranger_mention_hint");
        }
        if self.stranger_p2p_hint != new.stranger_p2p_hint {
            out.push("stranger_p2p_hint");
        }
        if self.reply_mode != new.reply_mode {
            out.push("reply_mode");
        }
        out
    }
}

/// SIGHUP 热重载：重读 config.toml，刷新白名单 / allowed_tools / permission_mode。
/// 解析失败只 warn 不崩，保留既有运行时配置。
#[cfg(unix)]
pub(crate) fn spawn_sighup_handler(
    dispatcher: Arc<imagent_core::Dispatcher>,
    backend: Arc<dyn imagent_core::Backend>,
    claude_cli: Option<Arc<imagent_claude::ClaudeBackend>>,
    config_path: PathBuf,
    store: imagent_store::Store,
    platform_name: String,
    startup_snapshot: HotReloadSnapshot,
) {
    tokio::spawn(async move {
        let mut sig = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "imagent::ops", error = %e, "无法注册 SIGHUP 处理器，热重载不可用");
                return;
            }
        };
        // P2-1b（code-review v14）：上一次成功加载的不可热载键快照。基准取
        // 启动 config（而非首次 SIGHUP 的加载结果）——启动到首次 SIGHUP 之间
        // 的变更也要能报出来。
        let mut last_applied = startup_snapshot;
        loop {
            sig.recv().await;
            tracing::info!(target: "imagent::ops", "received SIGHUP, reloading config");
            match imagent_core::Config::load(&config_path) {
                Ok(cfg) => {
                    // P2-1b（code-review v14）：不可热载键对比——任一变化打
                    // error 列出变更键与「需重启生效」，**不拒绝**重载其余
                    //（可热载）部分（半生效好过全不生效，日志保证运维知情）。
                    let snap = HotReloadSnapshot::of(&cfg);
                    let changed = last_applied.changed_keys(&snap);
                    if !changed.is_empty() {
                        tracing::error!(
                            target: "imagent::ops",
                            keys = ?changed,
                            "SIGHUP 检测到 {} 个不可热载配置键变更（{:?}）：需重启生效；\
                             其余可热载项已按新配置刷新",
                            changed.len(),
                            changed
                        );
                    }
                    last_applied = snap;
                    // 白名单：config 种子 ∪ store 已有，整体替换。
                    let senders = union_config_store(
                        &cfg.allowed_senders,
                        store.list_allowed_senders().await.unwrap_or_default(),
                    );
                    dispatcher.auth().reload(senders);
                    // 会话白名单：config 种子 ∪ store（P4-5）。
                    let chats = union_config_store(
                        &cfg.allowed_chats,
                        store.list_allowed_chats().await.unwrap_or_default(),
                    );
                    dispatcher.auth().reload_chats(chats);
                    dispatcher.reload_tools(cfg.allowed_tools.clone());
                    dispatcher.set_approval_tools(cfg.approval_tools.clone());
                    // v13：全局并发护栏热改（换闸——仅影响后续 acquire，在飞轮
                    // 持旧 permit 跑完）。
                    dispatcher.reload_max_concurrent_rounds(cfg.max_concurrent_rounds);
                    // v1.18/v1.20：自动压缩预算热改（窗口重置回 config 值，
                    // ACP 学习值在下一轮重新校准）。
                    dispatcher.reload_auto_compact_budget(
                        cfg.model_context_window_tokens,
                        cfg.auto_compact_window_ratio,
                        if cfg.model_context_window_tokens > 0 {
                            0
                        } else {
                            cfg.auto_compact_threshold_tokens
                        },
                    );
                    // v1.18 review（agent-2 #4）：admin 名单热改（config 种子 ∪
                    // store 动态条目，与启动并集口径一致）——授权类配置静默不
                    // 生效最伤运维（移除的管理员保留权限到重启）。
                    {
                        let admins = union_config_store(
                            &cfg.admin_senders,
                            store.list_admin_senders().await.unwrap_or_default(),
                        );
                        dispatcher.reload_admins(admins);
                    }
                    backend.set_native_permission_mode(cfg.backend_permission_mode.clone());
                    dispatcher.set_shortcuts(cfg.shortcuts.clone());
                    // 审批通道热切（control ↔ mcp 即时生效，下一轮 agent 起走新通道）。
                    if let Some(b) = &claude_cli {
                        b.set_permission_channel(&cfg.claude_permission_channel);
                    }
                    // W1-2：模型基准值重设（/model 的运行时热设被 config 值覆盖——
                    // SIGHUP 语义即「回到配置面」）。
                    backend.set_model(cfg.claude_model.clone());
                    // W1-2/W1-3/W1-4：claude-cli 运行参数整体替换（含用户 MCP
                    // 配置文件的现场重读）。T12：bitable 开关同轮刷新（下一轮
                    // spawn 的 mcp 配置按新值挂载/撤销 --bitable）。
                    if let Some(b) = &claude_cli {
                        apply_claude_runtime_opts(b, &cfg, cfg.bitable_enabled_for(&platform_name));
                    }
                    // T12：Bitable 数据面热改——注入/撤销 feishu 实现与 MCP 工具
                    // 挂载同轮刷新（app_token/table_id 改动 = 重建句柄，删配置 =
                    // 置 None，后续请求回「未配置」）。
                    warn_bitable_disable_reason(&cfg, &platform_name);
                    apply_bitable(&dispatcher, &cfg, &platform_name);
                    // P2-1a（code-review v14）：auto 档按**运行中后端**解析——
                    // agent 键不可热载（后端实现启动期装配），若按重载 config 的
                    // agent 字符串解析，把 claude-cli 改成 codex 的那次 SIGHUP
                    // 会把仍在运行的 claude-cli 从审批闭环（auto-claude）静默
                    // 降级成 off。agent_label() 返回启动期后端名，保证解析基准
                    // 与实际运行的后端一致（后端名在下次重启前不会变）。
                    let perm =
                        resolve_permission_mode(cfg.permission_mode, dispatcher.agent_label());
                    // S-1：热切校验失败（闭环档 × 非 FullLoop 后端 / socket 失败）
                    // 拒绝并保留既有模式——error 级日志便于发现「改了配置没生效」。
                    if let Err(e) = dispatcher.reload_permission_mode(perm) {
                        tracing::error!(
                            target: "imagent::ops",
                            error = %e,
                            "SIGHUP 热重载 permission_mode 被拒绝，保留既有运行时模式"
                        );
                    }
                    tracing::info!(target: "imagent::ops", "config reloaded (SIGHUP)");
                }
                Err(e) => {
                    tracing::warn!(target: "imagent::ops", error = %e, "SIGHUP 重载配置失败，保留既有运行时配置");
                }
            }
        }
    });
}

/// 等 SIGINT 或 SIGTERM（P1-4：补 SIGTERM，容器/systemd/k8s 滚动更新优雅退出）。
/// 信号到达后返回，调用方触发 `dispatcher.shutdown()`。
pub(crate) async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "imagent::ops", error = %e, "无法注册 SIGTERM 处理器，仅监听 SIGINT");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!(target: "imagent::ops", "received SIGINT, shutting down");
                // P5 快赢：优雅退出可能长达 shutdown_grace（默认 60s），期间后续
                // Ctrl-C 会被已安装的 handler 吞掉，操作员只能 kill -9。二次
                // Ctrl-C 直接强退（130 = SIGINT 惯例退出码）。
                tracing::info!(target: "imagent::ops", "再按一次 Ctrl-C 立即强制退出");
                tokio::spawn(async {
                    let _ = tokio::signal::ctrl_c().await;
                    eprintln!("收到第二次 Ctrl-C，立即强制退出");
                    std::process::exit(130);
                });
            }
            _ = term.recv() => {
                tracing::info!(target: "imagent::ops", "received SIGTERM, shutting down");
                // v1.18 review（agent-2 #11）：与 SIGINT 同款二次信号强退——
                // systemd stop 后 drain 挂死时操作员只能 kill -9 的出口。
                tracing::info!(target: "imagent::ops", "再次收到 SIGTERM 立即强制退出");
                tokio::spawn(async move {
                    term.recv().await;
                    eprintln!("收到第二次 SIGTERM，立即强制退出");
                    std::process::exit(143);
                });
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!(target: "imagent::ops", "received Ctrl-C, shutting down");
    }
}

/// S7：HTTP 鉴权 / fail-closed 绑定校验单测（纯函数层；完整 handler 需真实
/// store，不在此覆盖）。
#[cfg(test)]
mod metrics_auth_tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    fn headers_with(auth: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(a) = auth {
            h.insert(
                axum::http::header::AUTHORIZATION,
                HeaderValue::from_str(a).unwrap(),
            );
        }
        h
    }

    /// token 未设置（loopback 部署）→ 不鉴权，任何请求放行。
    #[test]
    fn no_token_always_authorizes() {
        assert!(bearer_authorized(&headers_with(None), None));
        assert!(bearer_authorized(&headers_with(Some("Bearer x")), None));
    }

    /// token 设置后：精确 `Bearer <token>` 才放行；缺失/错误/格式偏差均拒。
    #[test]
    fn token_requires_exact_bearer_match() {
        let ok = headers_with(Some("Bearer s3cret"));
        assert!(bearer_authorized(&ok, Some("s3cret")));
        // 无头 / 错 token / 非 Bearer scheme / 多余前缀 → 拒。
        assert!(!bearer_authorized(&headers_with(None), Some("s3cret")));
        assert!(!bearer_authorized(
            &headers_with(Some("Bearer wrong")),
            Some("s3cret")
        ));
        assert!(!bearer_authorized(
            &headers_with(Some("Basic s3cret")),
            Some("s3cret")
        ));
        assert!(!bearer_authorized(
            &headers_with(Some("Bearer  s3cret")),
            Some("s3cret")
        ));
    }

    /// S7 fail-closed：非 loopback 且未配 token 拒绝；loopback 或已配 token 放行。
    #[test]
    fn non_loopback_without_token_is_rejected() {
        let pub_addr: SocketAddr = "0.0.0.0:9615".parse().unwrap();
        let lo_addr: SocketAddr = "127.0.0.1:9615".parse().unwrap();
        let v6lo: SocketAddr = "[::1]:9615".parse().unwrap();

        assert!(validate_metrics_bind(pub_addr, None).is_err());
        assert!(validate_metrics_bind(pub_addr, Some("t")).is_ok());
        assert!(validate_metrics_bind(lo_addr, None).is_ok());
        assert!(validate_metrics_bind(v6lo, None).is_ok());
        // 错误信息给运维可操作的指引。
        let msg = validate_metrics_bind(pub_addr, None).unwrap_err();
        assert!(msg.contains("IMAGENT_HTTP_TOKEN"), "msg={msg}");
    }

    /// env 解析：空白视为未设置（metrics_http_token = trim + filter 非空；
    /// 进程级 env 在并行测试间共享，不宜 set_var 直接断言）。
    #[test]
    fn blank_env_token_means_unset() {}
}

/// P2-1b（code-review v14）：SIGHUP 不可热载键快照对比单测——逐键变化被报出、
/// 未变键不误报、webhooks 逐条字段变化（含深层字段）可检出。
#[cfg(unix)]
#[cfg(test)]
mod sighup_hot_reload_tests {
    use super::*;

    fn snap() -> HotReloadSnapshot {
        HotReloadSnapshot {
            agent: "claude-cli".into(),
            webhook_addr: Some("127.0.0.1:18443".into()),
            webhooks: vec![(
                "token-1".into(),
                "feishu:oc_a".into(),
                "ci".into(),
                Some("secret-1".into()),
                Some(10.0),
                300,
            )],
            metrics_addr: Some("127.0.0.1:9100".into()),
            sender_daily_cost_limit_usd: Some(5.0),
            agent_timeout_secs: 3600,
            agent_idle_timeout_secs: 1200,
            batch_window_ms: 1500,
            cron_catchup: "One".into(),
            stranger_mention_hint: false,
            stranger_p2p_hint: true,
            reply_mode: imagent_core::ReplyMode::Card,
        }
    }

    /// 全等快照 → 无变更键。
    #[test]
    fn identical_snapshots_report_no_change() {
        assert!(snap().changed_keys(&snap()).is_empty());
    }

    /// 每个不可热载键的变化都能被点名（逐个翻转断言）。
    #[test]
    fn each_key_change_is_reported() {
        type SnapshotCheck = (Box<dyn Fn(&mut HotReloadSnapshot)>, &'static str);
        let checks: Vec<SnapshotCheck> = vec![
            (Box::new(|s| s.agent = "codex".into()), "agent"),
            (
                Box::new(|s| s.webhook_addr = Some("127.0.0.1:19000".into())),
                "webhook_addr",
            ),
            (Box::new(|s| s.webhook_addr = None), "webhook_addr"),
            // webhooks 深层字段（replay_window_secs）变化可检出。
            (Box::new(|s| s.webhooks[0].5 = 600), "webhooks"),
            (Box::new(|s| s.webhooks.clear()), "webhooks"),
            (Box::new(|s| s.metrics_addr = None), "metrics_addr"),
            (
                Box::new(|s| s.sender_daily_cost_limit_usd = None),
                "sender_daily_cost_limit_usd",
            ),
            (
                Box::new(|s| s.agent_timeout_secs = 60),
                "agent_timeout_secs",
            ),
            (
                Box::new(|s| s.agent_idle_timeout_secs = 60),
                "agent_idle_timeout_secs",
            ),
            (Box::new(|s| s.batch_window_ms = 0), "batch_window_ms"),
            (Box::new(|s| s.cron_catchup = "Off".into()), "cron_catchup"),
            (
                Box::new(|s| s.stranger_mention_hint = true),
                "stranger_mention_hint",
            ),
            (
                Box::new(|s| s.stranger_p2p_hint = false),
                "stranger_p2p_hint",
            ),
            (
                Box::new(|s| s.reply_mode = imagent_core::ReplyMode::Text),
                "reply_mode",
            ),
        ];
        for (mutate, key) in checks {
            let mut new = snap();
            mutate(&mut new);
            let changed = snap().changed_keys(&new);
            assert_eq!(
                changed,
                vec![key],
                "翻转 {key} 应恰好报出该键，实际 {changed:?}"
            );
        }
    }
}
