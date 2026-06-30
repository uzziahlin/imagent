//! `imagent` 二进制入口：组装 store / core / ilink / claude 四个 crate。
//!
//! 职责：加载配置 → 扫码登录 → 前台常驻收私聊 → 鉴权 → 驱动 `claude -p` → 回传。
//! 鉴权 / allowedTools 收敛 / 风控逻辑全部在 core（`Dispatcher`）中，main 只做组装。

use std::sync::Arc;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "imagent", about = "IM ↔ agent gateway")]
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
    /// 停止（P1 前台运行，仅提示）。
    Stop,
    /// 内部子命令：作为 claude 的 MCP 权限审批 server（stdio JSON-RPC）。
    /// 由 claude 经 --mcp-config spawn，不直接手动调用。
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
    let db_path = data_dir.join("imagent.db");

    match cli.cmd {
        Cmd::Login { platform } => {
            if platform != "ilink" {
                return Err(anyhow!("P1 仅支持 ilink 平台，收到 platform={platform}"));
            }
            let store = imagent_store::Store::open(&db_path).await?;
            println!("开始 iLink 扫码登录，请用手机微信扫描终端二维码 …");
            let creds = imagent_ilink::login_flow(&store).await?;
            println!(
                "登录成功：bot_id={}，user_id={}（凭据已落盘 {}）",
                creds.ilink_bot_id, creds.ilink_user_id, db_path.display()
            );
            println!(
                "提示：下次 `start` 前建议先用发现模式（config.toml 留空 allowed_senders）跑，\n\
                 在日志里看到你的 from_user_id，填进 allowed_senders 后重启即可驱动 agent。"
            );
        }
        Cmd::Start { platform } => {
            if platform != "ilink" {
                return Err(anyhow!("P1 仅支持 ilink 平台，收到 platform={platform}"));
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

            // 2. store
            let store = imagent_store::Store::open(&db_path).await?;

            // 3. 凭据
            let (account_id, blob) = store
                .first_credential("ilink")
                .await?
                .ok_or_else(|| anyhow!("未登录，请先 `imagent login`"))?;
            let creds: imagent_ilink::Credentials = serde_json::from_str(&blob)?;

            // 4. client
            let client = imagent_ilink::ILinkClient::new(
                Some(creds.baseurl.clone()),
                creds.bot_token.clone(),
                creds.ilink_bot_id.clone(),
                creds.ilink_user_id.clone(),
            )?;

            // 5. platform
            let platform = Arc::new(imagent_ilink::ILinkPlatform::new(
                client,
                store.clone(),
                account_id,
            ));

            // 6. backend（注入权限审批模式）
            let backend = Arc::new(imagent_claude::ClaudeBackend::with_permission_mode(
                config.permission_mode,
            ));

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

            // 8. dispatcher
            let dispatcher = Arc::new(imagent_core::Dispatcher::new(
                platform,
                backend,
                store,
                auth,
                config.default_workdir.clone(),
                config.allowed_tools.clone(),
                config.permission_mode,
            ));

            // 9. 前台运行 + Ctrl-C
            tracing::info!(
                "imagent started (platform=ilink, workdir={}, tools={:?}, discovery={})",
                config.default_workdir.display(),
                config.allowed_tools,
                discovery
            );
            tokio::select! {
                res = dispatcher.clone().run() => match res {
                    Ok(()) => tracing::info!("dispatcher 正常退出"),
                    Err(e) => {
                        if e.to_string().to_lowercase().contains("session expired") {
                            tracing::error!("dispatcher 退出：{e}");
                            println!("iLink session 已过期，请重新运行 `imagent login` 扫码登录。");
                        } else {
                            tracing::error!("dispatcher 异常退出：{e}");
                            println!("imagent 异常退出：{e}");
                        }
                    }
                },
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("received Ctrl-C, shutting down");
                }
            }
        }
        Cmd::Status => {
            let store = imagent_store::Store::open(&db_path).await?;
            match store.first_credential("ilink").await? {
                Some((account_id, blob)) => {
                    let creds: imagent_ilink::Credentials = serde_json::from_str(&blob)
                        .map_err(|e| anyhow!("凭据解析失败（{account_id}）：{e}"))?;
                    println!("已登录：bot_id={}（account_id={}）", creds.ilink_bot_id, account_id);
                }
                None => {
                    println!("未登录（无 iLink 凭据），请先 `imagent login`。");
                }
            }
            let config_path = imagent_core::Config::default_path();
            println!("配置路径：{}", config_path.map(|p| p.display().to_string()).unwrap_or_else(|| "<无法定位 home>".into()));
        }
        Cmd::Allow { platform: _, sender } => {
            // 本地操作者（最高权限）：直接写入白名单 + 审计。空白名单时的唯一 bootstrap。
            let store = imagent_store::Store::open(&db_path).await?;
            store
                .add_allowed_sender(&sender, Some("cli"), Some("manual"))
                .await?;
            store
                .append_audit("allow", Some("cli"), Some(&sender), Some("cli-bootstrap"))
                .await?;
            let all = store.list_allowed_senders().await.unwrap_or_default();
            println!("已授权 `{sender}`。当前白名单（{}）：{}", all.len(), all.join(", "));
        }
        Cmd::Stop => {
            println!("imagent P1 为前台运行模式，请在运行 `start` 的终端按 Ctrl-C 停止。");
        }
        Cmd::Mcp { conv_id, sock, mode } => {
            // 作为 claude 的 MCP 权限审批 server（stdio JSON-RPC）。
            let mode = imagent_core::PermissionMode::from_str_lossy(&mode);
            tracing::info!(
                target: "imagent::mcp",
                conv_id = %conv_id, sock = %sock, mode = mode.as_str(),
                "MCP permission server starting"
            );
            if let Err(e) = imagent_core::mcp::run_mcp_server(conv_id, sock, mode).await {
                tracing::error!(target: "imagent::mcp", error = %e, "MCP server 退出");
            }
        }
    }

    Ok(())
}
