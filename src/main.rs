//! `imagent` 二进制入口：组装 7 个 crate（store / core / ilink / wecom / claude /
//! codex / gemini），多平台（iLink 个人微信 / WeCom 企业微信）× 多后端
//!（Claude CLI/ACP / Codex / Gemini）。
//!
//! 职责：加载配置 →（iLink 扫码登录 / WeCom 读 config 凭据）→ 前台常驻收私聊 →
//! 鉴权 → 驱动 agent → 回传。鉴权 / allowedTools 收敛 / 权限审批 / 风控全部在
//! core（`Dispatcher`）中，main 只做组装 + 运维（metrics/health/SIGHUP/优雅退出）。

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::{routing::get, Json, Router};
use clap::{Parser, Subcommand};
use serde::Serialize;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "imagent", version, about = "IM ↔ agent gateway")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 扫码登录（iLink），凭据落盘。
    Login {
        #[arg(long, default_value = "ilink")]
        platform: String,
    },
    /// 前台常驻：收消息 → 鉴权 → 驱动 agent → 回传（Ctrl-C 退出）。
    Start {
        #[arg(long, default_value = "ilink")]
        platform: String,
    },
    /// 查看登录状态与配置路径。
    Status,
    /// 授权一个 sender（写入白名单，本地最高权限）。空白名单时的 bootstrap 途径。
    Allow {
        #[arg(long, default_value = "ilink")]
        platform: String,
        /// 要授权的 from_user_id（如 wx_xxx@im.wechat）。
        sender: String,
    },
    /// 停止（v1 前台运行模式，仅打印停止方式：前台 Ctrl-C / systemctl stop / kill <pid>）。
    Stop,
    /// 内部子命令：作为 claude 的 MCP 权限审批 server（stdio JSON-RPC）。
    /// 由 claude 经 --mcp-config spawn，不直接手动调用。
    #[command(hide = true)]
    Mcp {
        /// 当前会话标识（路由权限回复用）。
        #[arg(long)]
        conv_id: String,
        /// 主进程权限路由 socket 路径。
        #[arg(long)]
        sock: String,
        /// 权限模式 off | allow | deny | ask。
        #[arg(long, default_value = "off")]
        mode: String,
        /// S-3：socket 读超时（秒，= config.permission_ask_timeout_secs），与 dispatcher 审批预算对齐。
        #[arg(long, default_value_t = 300)]
        ask_timeout: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // 数据目录 ~/.imagent
    let home = dirs::home_dir().ok_or_else(|| anyhow!("无法定位 home 目录"))?;
    let data_dir = home.join(".imagent");
    std::fs::create_dir_all(&data_dir)?;
    // P2-14：数据目录收紧 0700（默认 umask 常 0755，同机其他用户可 ls 看到文件名；
    // 最小权限，与 store 文件 0600 / permission.sock 0600 姿态一致）。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700));
    }
    let db_path = data_dir.join("imagent.db");

    match cli.cmd {
        Cmd::Login { platform } => {
            if platform != "ilink" {
                return Err(anyhow!(
                    "login 仅支持 ilink 平台（WeCom 用 config 的 bot_id + secret，不走扫码登录），收到 platform={platform}"
                ));
            }
            let store = imagent_store::Store::open(&db_path).await?;
            println!("开始 iLink 扫码登录，请用手机微信扫描终端二维码 …");
            let creds = imagent_ilink::login_flow(&store).await?;
            println!(
                "登录成功：bot_id={}，user_id={}（凭据已落盘 {}）",
                creds.ilink_bot_id,
                creds.ilink_user_id,
                db_path.display()
            );
            println!(
                "提示：下次 `start` 前建议先用发现模式（config.toml 留空 allowed_senders）跑，\n\
                 在日志里看到你的 from_user_id，填进 allowed_senders 后重启即可驱动 agent。"
            );
        }
        Cmd::Start { platform } => {
            if platform != "ilink" && platform != "wecom" {
                return Err(anyhow!("未知 platform={platform}，支持 ilink | wecom"));
            }

            // 1. 配置
            let config_path = imagent_core::Config::default_path()
                .ok_or_else(|| anyhow!("无法定位 home 目录"))?;
            let config = match imagent_core::Config::load(&config_path) {
                Ok(c) => c,
                Err(e) => {
                    println!("加载配置失败（{}）：{e}", config_path.display());
                    println!("请创建配置文件，模板：\n{}", imagent_core::Config::EXAMPLE);
                    return Ok(());
                }
            };

            // 2. store（多份：dispatcher / HTTP /health / SIGHUP 各持一份 Clone）
            let store = imagent_store::Store::open(&db_path).await?;
            // P1-C：据 config.require_keyring 切换凭据 fail-closed
            // （true = keyring 不可用时拒绝明文落盘；默认 false 向后兼容）。
            store.set_require_keyring(config.require_keyring);

            // 3. platform —— 按 config.platform / CLI 选用 ilink 或 wecom。
            let platform_name = if platform == "ilink" || platform == "wecom" {
                platform.as_str()
            } else {
                config.platform.as_str()
            };
            let platform = build_platform(platform_name, &config, store.clone()).await?;

            // 6. backend —— permission_mode 用共享句柄，SIGHUP 热重载即时生效。
            let perm_mode = std::sync::Arc::new(parking_lot::RwLock::new(config.permission_mode));
            let backend = build_backend(
                &config.agent,
                perm_mode.clone(),
                std::time::Duration::from_secs(config.permission_ask_timeout_secs),
            );

            // codex/gemini 后端不支持 IM 权限审批闭环：若用户开启了 permission_mode，
            // 显式 warn（不静默忽略），避免预期落差。
            if config.permission_mode.is_enabled()
                && matches!(config.agent.as_str(), "codex" | "gemini")
            {
                tracing::warn!(
                    target: "imagent::ops",
                    agent = %config.agent,
                    "后端不支持 IM 权限审批闭环，permission_mode 将不生效（仅靠 agent 自身 sandbox/approval-mode 兜底）；如需 IM approve/deny 请用 claude-cli"
                );
            }

            // S-1：ACP 后端不强制 allowed_tools（CLI 用 --allowedTools 收敛，ACP 无等价机制）。
            // 用户配置 allowed_tools 期望工具白名单时需知晓：ACP 下工具收敛只能靠
            // permission_mode=ask/deny 审批闭环兜底，否则 claude 可用其请求的任意工具。
            if config.agent.as_str() == "claude-acp" && !config.allowed_tools.is_empty() {
                tracing::warn!(
                    target: "imagent::ops",
                    agent = %config.agent,
                    "claude-acp 后端不强制 allowed_tools（--allowedTools 在 ACP 无等价机制）；\
                     工具收敛需依赖 permission_mode=ask/deny，否则 claude 可用其请求的任意工具"
                );
            }

            // 7. auth —— 白名单：config 种子 ∪ store 已有（CLI /allow 或 IM /allow 持久化）。
            let mut initial: Vec<String> = config.allowed_senders.clone();
            let stored = store.list_allowed_senders().await.unwrap_or_default();
            for s in stored {
                if !initial.contains(&s) {
                    initial.push(s);
                }
            }
            let auth = imagent_core::Auth::new(initial);
            let discovery = auth.is_discovery();

            // 8. dispatcher —— allowed_tools / permission_mode 均以共享句柄注入。
            let tools_handle =
                std::sync::Arc::new(parking_lot::RwLock::new(config.allowed_tools.clone()));
            let dispatcher = Arc::new(imagent_core::Dispatcher::new_with_handles(
                platform,
                backend,
                store.clone(),
                auth,
                config.default_workdir.clone(),
                tools_handle,
                perm_mode.clone(),
                std::time::Duration::from_secs(config.agent_timeout_secs),
                std::time::Duration::from_secs(config.permission_ask_timeout_secs),
                std::time::Duration::from_secs(config.shutdown_grace_secs),
                config.admin_senders.clone(),
            ));

            // 9. 运维 HTTP server（/metrics + /health）。metrics_addr 为 None 或空串则关闭。
            let start_at = std::time::Instant::now();
            let metrics_addr = config
                .metrics_addr
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let http_store = store.clone();
            match metrics_addr {
                Some(addr) => match addr.parse::<SocketAddr>() {
                    Ok(socket) => {
                        spawn_metrics_server(socket, http_store.clone(), start_at);
                        tracing::info!(target: "imagent::ops", addr = %socket, "metrics/health HTTP server listening");
                    }
                    Err(e) => {
                        tracing::warn!(target: "imagent::ops", addr = addr, error = %e, "metrics_addr 解析失败，HTTP server 未启动");
                    }
                },
                None => {
                    tracing::info!(target: "imagent::ops", "metrics_addr 为空，HTTP server 关闭");
                }
            }

            // 10. SIGHUP 热重载（白名单 / allowed_tools / permission_mode）。
            #[cfg(unix)]
            spawn_sighup_handler(dispatcher.clone(), config_path.clone(), http_store.clone());
            #[cfg(not(unix))]
            tracing::info!(
                target: "imagent::ops",
                "SIGHUP 热重载需要 Unix 信号，当前平台不可用（配置改动需重启生效）"
            );

            // 11. 前台运行 + Ctrl-C
            tracing::info!(
                "imagent started (platform={}, workdir={}, tools={:?}, discovery={})",
                platform_name,
                config.default_workdir.display(),
                config.allowed_tools,
                discovery
            );
            // P1-4/P1-5：信号 task 监听 SIGINT + SIGTERM，触发 dispatcher 优雅退出
            // （run() 停止 recv + drain in-flight task，避免 SIGKILL 正在写文件的
            // agent 子进程导致半写）。run() 完成 drain 后自然返回。
            let dispatcher_for_signal = dispatcher.clone();
            tokio::spawn(async move {
                shutdown_signal().await;
                dispatcher_for_signal.shutdown();
            });
            match dispatcher.run().await {
                Ok(()) => tracing::info!(target: "imagent::ops", "dispatcher 退出（drain 完成）"),
                Err(e) => {
                    if matches!(e, imagent_core::CoreError::SessionExpired(_)) {
                        tracing::error!("dispatcher 退出：{e}");
                        println!("iLink session 已过期，请重新运行 `imagent login` 扫码登录。");
                    } else {
                        tracing::error!("dispatcher 异常退出：{e}");
                        println!("imagent 异常退出：{e}");
                    }
                }
            }
            // R-3：清理 permission.sock（P1-5 计划 ③，原未落地）。
            #[cfg(unix)]
            if let Some(sock) = imagent_core::default_sock_path() {
                let _ = std::fs::remove_file(&sock);
            }
        }
        Cmd::Status => {
            let store = imagent_store::Store::open(&db_path).await?;
            match store.first_credential("ilink").await? {
                Some((account_id, blob)) => {
                    let creds: imagent_ilink::Credentials = serde_json::from_str(&blob)
                        .map_err(|e| anyhow!("凭据解析失败（{account_id}）：{e}"))?;
                    println!(
                        "已登录：bot_id={}（account_id={}）",
                        creds.ilink_bot_id, account_id
                    );
                }
                None => {
                    println!("未登录（无 iLink 凭据），请先 `imagent login`。");
                }
            }
            let config_path = imagent_core::Config::default_path();
            println!(
                "配置路径：{}",
                config_path
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<无法定位 home>".into())
            );
        }
        Cmd::Allow {
            platform: _,
            sender,
        } => {
            // 本地操作者（最高权限）：直接写入白名单 + 审计。空白名单时的唯一 bootstrap。
            let store = imagent_store::Store::open(&db_path).await?;
            store
                .add_allowed_sender(&sender, Some("cli"), Some("manual"))
                .await?;
            store
                .append_audit("allow", Some("cli"), Some(&sender), Some("cli-bootstrap"))
                .await?;
            let all = store.list_allowed_senders().await.unwrap_or_default();
            println!(
                "已授权 `{sender}`。当前白名单（{}）：{}",
                all.len(),
                all.join(", ")
            );
        }
        Cmd::Stop => {
            println!(
                "imagent 为前台运行模式。停止方式：在 `start` 的终端按 Ctrl-C，或 `kill {}`。",
                std::process::id()
            );
        }
        Cmd::Mcp {
            conv_id,
            sock,
            mode,
            ask_timeout,
        } => {
            // 作为 claude 的 MCP 权限审批 server（stdio JSON-RPC）。
            let mode = imagent_core::PermissionMode::from_str_lossy(&mode);
            tracing::info!(
                target: "imagent::mcp",
                conv_id = %conv_id, sock = %sock, mode = mode.as_str(),
                ask_timeout_secs = ask_timeout,
                "MCP permission server starting"
            );
            if let Err(e) = imagent_core::mcp::run_mcp_server(
                conv_id,
                sock,
                mode,
                std::time::Duration::from_secs(ask_timeout),
            )
            .await
            {
                tracing::error!(target: "imagent::mcp", error = %e, "MCP server 退出");
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Backend 选择：按 config.agent 选用对应 agent 后端。
// ---------------------------------------------------------------------------

/// 按 `config.agent` 选择 Backend。
///
/// - `"codex"` → [`imagent_codex::CodexBackend`]；
/// - `"gemini"` → [`imagent_gemini::GeminiBackend`]；
/// - `"claude-acp"` → [`imagent_claude::AcpBackend`]（ACP/JSON-RPC 长驻子进程模式，
///   与 `claude-cli` 并存；共享 permission_mode 句柄，SIGHUP 即时生效）；
/// - 其它（含默认 `"claude-cli"`）→ [`imagent_claude::ClaudeBackend`]，
///   行为与单后端时期完全一致（permission_mode 共享句柄，SIGHUP 即时生效）。
fn build_backend(
    agent: &str,
    perm_mode: Arc<parking_lot::RwLock<imagent_core::PermissionMode>>,
    ask_timeout: std::time::Duration,
) -> Arc<dyn imagent_core::Backend> {
    match agent {
        "codex" => Arc::new(imagent_codex::CodexBackend::new()),
        "gemini" => Arc::new(imagent_gemini::GeminiBackend::new()),
        "claude-acp" => Arc::new(imagent_claude::AcpBackend::with_permission_mode_shared(
            perm_mode,
        )),
        _ => Arc::new(imagent_claude::ClaudeBackend::with_permission_mode_shared(
            perm_mode,
            ask_timeout,
        )),
    }
}

// ---------------------------------------------------------------------------
// Platform 选择：按 platform 名选用 ilink 或 wecom。
// ---------------------------------------------------------------------------

/// 按 platform 名选择 Platform 实例。
///
/// - `"wecom"` → [`imagent_wecom::WeComPlatform`]：凭据取自 config 的
///   `wecom_bot_id` / `wecom_secret`（企业微信智能机器人不走扫码登录）。
/// - 其它（含默认 `"ilink"`）→ [`imagent_ilink::ILinkPlatform`]，
///   行为与单平台时期完全一致（读 store 的 ilink 凭据 + 扫码登录的 client）。
async fn build_platform(
    name: &str,
    config: &imagent_core::Config,
    store: imagent_store::Store,
) -> Result<Arc<dyn imagent_core::Platform>> {
    match name {
        "wecom" => {
            let bot_id = config
                .wecom_bot_id
                .clone()
                .ok_or_else(|| anyhow!("platform=wecom 需在 config.toml 配置 wecom_bot_id"))?;
            let secret = config
                .wecom_secret
                .clone()
                .ok_or_else(|| anyhow!("platform=wecom 需在 config.toml 配置 wecom_secret"))?;
            // openws 默认地址。
            let ws_url = "wss://openws.work.weixin.qq.com".to_string();
            Ok(Arc::new(imagent_wecom::WeComPlatform::new(
                bot_id, secret, ws_url,
            )))
        }
        _ => {
            // 默认 ilink：保持既有行为。
            let (account_id, blob) = store
                .first_credential("ilink")
                .await?
                .ok_or_else(|| anyhow!("未登录，请先 `imagent login`"))?;
            let creds: imagent_ilink::Credentials = serde_json::from_str(&blob)?;
            let client = imagent_ilink::ILinkClient::new(
                Some(creds.baseurl.clone()),
                creds.bot_token.clone(),
                creds.ilink_bot_id.clone(),
                creds.ilink_user_id.clone(),
            )?;
            Ok(Arc::new(imagent_ilink::ILinkPlatform::new(
                client,
                store,
                account_id,
                config.message_max_len,
                std::time::Duration::from_millis(config.message_fragment_interval_ms),
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// 运维：Prometheus 指标 / 健康检查 HTTP server + SIGHUP 热重载
// ---------------------------------------------------------------------------

/// `/health` 返回的 JSON 载荷。
#[derive(Serialize)]
struct Health {
    logged_in: bool,
    uptime_secs: u64,
    version: &'static str,
    sessions: i64,
}

/// 共享给 axum handler 的状态。
#[derive(Clone)]
struct HttpState {
    store: imagent_store::Store,
    start_at: Instant,
}

/// 起 HTTP server（/metrics + /health），独立 tokio task。失败仅 warn。
fn spawn_metrics_server(addr: SocketAddr, store: imagent_store::Store, start_at: Instant) {
    let state = HttpState { store, start_at };
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/health", get(health_handler))
        .with_state(state);
    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(target: "imagent::ops", addr = %addr, error = %e, "bind metrics addr 失败");
                return;
            }
        };
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!(target: "imagent::ops", addr = %addr, error = %e, "metrics HTTP server 退出");
        }
    });
}

async fn metrics_handler() -> (StatusCode, String) {
    (StatusCode::OK, imagent_core::metrics::render())
}

async fn health_handler(State(st): State<HttpState>) -> (StatusCode, Json<Health>) {
    let sessions = st.store.count_sessions().await.unwrap_or(-1);
    let logged_in = st
        .store
        .first_credential("ilink")
        .await
        .map(|o| o.is_some())
        .unwrap_or(false);
    let body = Health {
        logged_in,
        uptime_secs: st.start_at.elapsed().as_secs(),
        version: env!("CARGO_PKG_VERSION"),
        sessions,
    };
    (StatusCode::OK, Json(body))
}

/// SIGHUP 热重载：重读 config.toml，刷新白名单 / allowed_tools / permission_mode。
/// 解析失败只 warn 不崩，保留既有运行时配置。
#[cfg(unix)]
fn spawn_sighup_handler(
    dispatcher: Arc<imagent_core::Dispatcher>,
    config_path: PathBuf,
    store: imagent_store::Store,
) {
    tokio::spawn(async move {
        let mut sig = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "imagent::ops", error = %e, "无法注册 SIGHUP 处理器，热重载不可用");
                return;
            }
        };
        loop {
            sig.recv().await;
            tracing::info!(target: "imagent::ops", "received SIGHUP, reloading config");
            match imagent_core::Config::load(&config_path) {
                Ok(cfg) => {
                    // 白名单：config 种子 ∪ store 已有，整体替换。
                    let mut senders: Vec<String> = cfg.allowed_senders.clone();
                    let stored = store.list_allowed_senders().await.unwrap_or_default();
                    for s in stored {
                        if !senders.contains(&s) {
                            senders.push(s);
                        }
                    }
                    dispatcher.auth().reload(senders);
                    dispatcher.reload_tools(cfg.allowed_tools.clone());
                    dispatcher.reload_permission_mode(cfg.permission_mode);
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
async fn shutdown_signal() {
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
            }
            _ = term.recv() => {
                tracing::info!(target: "imagent::ops", "received SIGTERM, shutting down");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!(target: "imagent::ops", "received Ctrl-C, shutting down");
    }
}
