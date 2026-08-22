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

mod service;
mod setup;

#[derive(Parser)]
#[command(name = "imagent", version, about = "IM ↔ agent gateway")]
struct Cli {
    /// 状态目录 profile（P4-10）：使用 `~/.imagent/profiles/<name>`（config / db /
    /// permission.sock / 媒体全隔离），默认 `~/.imagent`。配合 `imagent profile create`。
    #[arg(long, global = true)]
    profile: Option<String>,

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
    /// 授权一个会话/群（P4-5：写入会话白名单，conv_id 原样如 feishu:oc_xxx）。
    AllowChat {
        /// 要授权的 conv_id（如 feishu:oc_xxx）。
        conv_id: String,
    },
    /// Profile 多实例管理（P4-10）：每个 profile 独立 config / db / sock / 媒体。
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
    /// 停止（v1 前台运行模式，仅打印停止方式：前台 Ctrl-C / systemctl stop / kill <pid>）。
    Stop,
    /// 首次运行交互式向导（P6-5）：平台选择 → 飞书权限/事件清单引导 → 凭据
    /// 连通性校验 → 工作目录（过宽拒绝）→ 写 config.toml。
    Setup,
    /// 服务自管理（P6-6）：安装/卸载/查询 OS 级后台服务（macOS launchd /
    /// Linux systemd 用户单元），注册当前二进制与 --profile。
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
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

#[derive(Subcommand)]
enum ProfileAction {
    /// 列出全部 profile。
    List,
    /// 创建 profile（建目录 + 写 config 模板；不覆盖已有 config）。
    Create { name: String },
    /// 删除 profile（含其全部状态；default 不可删；需 --yes 确认）。
    Remove {
        name: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// 安装并启动后台服务（注册当前二进制 + 当前 --profile）。
    Install,
    /// 停止并卸载后台服务。
    Uninstall,
    /// 查询服务安装/运行状态。
    Status,
}

/// profile 根目录：`~/.imagent/profiles/<name>`（不受 IMAGENT_HOME 覆盖影响，
/// profile 管理本身始终锚定真实 home，防嵌套歧义）。
fn profile_root(name: &str) -> anyhow::Result<std::path::PathBuf> {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized != name || sanitized.is_empty() {
        return Err(anyhow!("非法 profile 名 {name:?}（仅限字母数字 - _）"));
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("无法定位 home 目录"))?;
    Ok(home.join(".imagent").join("profiles").join(&sanitized))
}

#[tokio::main]
async fn main() -> Result<()> {
    // rustls 0.23 breaking change：必须显式安装 process-level CryptoProvider，
    // 否则飞书 open-lark 长连接首次 TLS 握手 panic（rustls 0.23 不再隐式选 provider）。
    // 须在任何 rustls/reqwest TLS 使用前调用。ring 与 reqwest rustls-tls 一致。
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // P4-10：`--profile <name>` → 进程内把状态根目录切到 profile 目录
    // （config / db / sock / 媒体 / MCP 子进程 env 全部随之隔离）。
    // profile 管理子命令自身不切（始终操作真实 home 下的 profiles/）。
    let is_profile_mgmt = matches!(cli.cmd, Cmd::Profile { .. });
    if !is_profile_mgmt {
        if let Some(name) = &cli.profile {
            let root = profile_root(name)?;
            if !root.is_dir() {
                return Err(anyhow!(
                    "profile {name:?} 不存在（{}）。先运行 `imagent profile create {name}`",
                    root.display()
                ));
            }
            // Safety（set_var 多线程）：进程启动早期、tokio runtime 尚未跑用户代码，
            // 此处是唯一写点（Unix 上 glibc putenv 无 RSS；后续只读）。
            std::env::set_var(imagent_core::paths::IMAGENT_HOME_ENV, &root);
            println!("使用 profile：{name}（状态目录 {}）", root.display());
        }
    }

    // 数据目录（imagent_home：默认 ~/.imagent，--profile 时为 profile 目录）
    let data_dir = imagent_core::paths::imagent_home();
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
            // P5：login 写凭据也按 profile 分 keyring 键（与 start 一致）。
            store.set_keyring_scope(cli.profile.as_deref().unwrap_or(""));
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
        Cmd::AllowChat { conv_id } => {
            // P4-5：会话（群）白名单 bootstrap（与 Allow 同构）。
            let store = imagent_store::Store::open(&db_path).await?;
            store.add_allowed_chat(&conv_id, None, Some("cli")).await?;
            println!("已授权会话 {conv_id}（重启 imagent 生效；IM 内 /chat 可动态管理）");
        }
        Cmd::Profile { action } => {
            // profile 管理不切 IMAGENT_HOME（见上方 is_profile_mgmt）。
            match action {
                ProfileAction::List => {
                    let home = dirs::home_dir().ok_or_else(|| anyhow!("无法定位 home 目录"))?;
                    let dir = home.join(".imagent").join("profiles");
                    if !dir.is_dir() {
                        println!("暂无 profile（{} 不存在）", dir.display());
                        return Ok(());
                    }
                    let mut names: Vec<String> = std::fs::read_dir(&dir)?
                        .filter_map(|e| e.ok())
                        .filter(|e| e.path().is_dir())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect();
                    names.sort();
                    if names.is_empty() {
                        println!("暂无 profile（目录为空）");
                    } else {
                        println!("profiles（{} 个）：", names.len());
                        for n in names {
                            println!("  - {n}（imagent --profile {n} start）");
                        }
                    }
                }
                ProfileAction::Create { name } => {
                    let root = profile_root(&name)?;
                    std::fs::create_dir_all(&root)?;
                    let cfg = root.join("config.toml");
                    if cfg.exists() {
                        println!(
                            "profile {name} 已存在（config 保留不动）：{}",
                            root.display()
                        );
                    } else {
                        std::fs::write(&cfg, imagent_core::Config::EXAMPLE)?;
                        println!(
                            "已创建 profile {name}：{}\nconfig 模板已写入 {}（填 default_workdir 后即可运行）\n启动：imagent --profile {name} start",
                            root.display(),
                            cfg.display()
                        );
                    }
                }
                ProfileAction::Remove { name, yes } => {
                    if name == "default" {
                        return Err(anyhow!("default（默认 ~/.imagent）不可经此删除"));
                    }
                    let root = profile_root(&name)?;
                    if !root.is_dir() {
                        return Err(anyhow!("profile {name} 不存在：{}", root.display()));
                    }
                    if !yes {
                        return Err(anyhow!(
                            "将删除 {} 的全部状态（config/db/凭据/媒体），确认请加 --yes",
                            root.display()
                        ));
                    }
                    std::fs::remove_dir_all(&root)?;
                    println!("已删除 profile {name}（{}）", root.display());
                }
            }
        }
        Cmd::Start { platform } => {
            if platform != "ilink" && platform != "wecom" && platform != "feishu" {
                return Err(anyhow!(
                    "未知 platform={platform}，支持 ilink | wecom | feishu"
                ));
            }

            // 1. 配置
            let config_path = imagent_core::Config::default_path()
                .ok_or_else(|| anyhow!("无法定位 home 目录"))?;
            let config = match imagent_core::Config::load(&config_path) {
                Ok(c) => c,
                Err(e) => {
                    // P5 快赢：配置加载失败以非零退出码结束——此前 return Ok(()) 退出码
                    // 为 0，systemd/监控视为成功不重启不告警。
                    return Err(anyhow!(
                        "加载配置失败（{}）：{e}\n请创建配置文件，模板：\n{}",
                        config_path.display(),
                        imagent_core::Config::EXAMPLE
                    ));
                }
            };

            // P5-9a：单实例锁——同 IMAGENT_HOME 双实例会互劫持 permission.sock，
            // 使先启动实例的 Ask 审批闭环静默失效。锁随 _instance_lock 持有到退出。
            let _instance_lock =
                imagent_core::instance::acquire(&imagent_core::paths::imagent_home())?;

            // 2. store（多份：dispatcher / HTTP /health / SIGHUP 各持一份 Clone）
            let store = imagent_store::Store::open(&db_path).await?;
            // P1-C：据 config.require_keyring 切换凭据 fail-closed
            // （true = keyring 不可用时拒绝明文落盘；默认 false 向后兼容）。
            store.set_require_keyring(config.require_keyring);
            // P5：keyring 用户名按 profile 分段——多 profile 同机同平台不再互删
            // 凭据（读取对旧的无 profile 键 fallback，存量部署零迁移）。
            store.set_keyring_scope(cli.profile.as_deref().unwrap_or(""));

            // 3. platform —— 按 config.platform / CLI 选用 ilink 或 wecom。
            let platform_name =
                if platform == "ilink" || platform == "wecom" || platform == "feishu" {
                    platform.as_str()
                } else {
                    config.platform.as_str()
                };
            let platform = build_platform(platform_name, &config, store.clone()).await?;

            // 孤儿流式卡片关流（P4_ROADMAP 第六批）：上次进程退出时滞留「生成中」的
            // 卡片按 store 登记逐张 patch 成「已中断」，失败保留登记下次再试。
            imagent_core::sweep_live_cards(&store, platform.as_ref()).await;

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

            // 7. auth —— 白名单：config 种子 ∪ store 已有（CLI /allow 或 IM /allow 持久化）；
            //    会话（群）白名单同构（P4-5）。
            let mut initial: Vec<String> = config.allowed_senders.clone();
            let stored = store.list_allowed_senders().await.unwrap_or_default();
            for s in stored {
                if !initial.contains(&s) {
                    initial.push(s);
                }
            }
            let mut initial_chats: Vec<String> = config.allowed_chats.clone();
            let stored_chats = store.list_allowed_chats().await.unwrap_or_default();
            for c in stored_chats {
                if !initial_chats.contains(&c) {
                    initial_chats.push(c);
                }
            }
            let auth = imagent_core::Auth::with_chats(initial, initial_chats);
            let discovery = auth.is_discovery();

            // P5-7（安全）：群放行 + 管理员为空的组合 = 群内任何成员都具备管理
            // 能力（/allow /chat /config /perm）。启动期硬告警（不拒启：单用户
            // 依赖「空=全员可」的既有语义），群部署必须显式设 admin_senders。
            if config.admin_gap_with_chat_allowlist() {
                tracing::error!(
                    target: "imagent",
                    "⚠️ 安全配置告警：allowed_chats（群放行）非空但 admin_senders 为空——\
                     被授权群里的任何成员都将具备管理能力（/allow 扩权、/chat 扩群、\
                     /config /perm 改全局）。请在 config.toml 设置 \
                     admin_senders = [\"<你的 sender id>\"] 收紧（/whoami 可查 id）。"
                );
            }

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
                imagent_core::TaskBudgets::from_config(&config),
                config.cot_detail,
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
                        if !socket.ip().is_loopback() {
                            tracing::warn!(
                                target: "imagent::ops",
                                addr = %socket,
                                "metrics_addr 绑定非 loopback 地址：/metrics 与 /health 无鉴权，公网可访问（仅暴露消息计数/会话数等运营指标，不含凭据）。生产环境建议绑 127.0.0.1 或置于反向代理后"
                            );
                        }
                        spawn_metrics_server(
                            socket,
                            http_store.clone(),
                            start_at,
                            platform_name.to_string(),
                            // P5-第五批：wecom 凭据来自 config（store 里永远没有），
                            // /health 按存在性预判定——其余平台 None 走 store/env 动态查。
                            if platform_name == "wecom" {
                                Some(config.wecom_bot_id.is_some() && config.wecom_secret.is_some())
                            } else {
                                None
                            },
                        );
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

            // P5：媒体目录 TTL 清理——入站媒体只增不减会撑爆磁盘；启动跑一次 +
            // 每日循环，删 7 天前的文件（best-effort，失败仅跳过）。
            tokio::spawn(async {
                let media = imagent_core::paths::imagent_home().join("media");
                loop {
                    let ttl = std::time::Duration::from_secs(7 * 24 * 3600);
                    let cutoff = std::time::SystemTime::now()
                        .checked_sub(ttl)
                        .unwrap_or(std::time::UNIX_EPOCH);
                    let removed = imagent_core::paths::sweep_media_before(&media, cutoff);
                    if removed > 0 {
                        tracing::info!(
                            target: "imagent::ops",
                            removed,
                            "媒体 TTL 清理（7 天前，共 {} 个文件目录）",
                            media.display()
                        );
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(24 * 3600)).await;
                }
            });

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
            // R-3：清理 permission.sock（P1-5 计划 ③，原未落地）；P5-9b：握手
            // token 文件一并清理。
            #[cfg(unix)]
            if let Some(sock) = imagent_core::default_sock_path() {
                let _ = std::fs::remove_file(&sock);
                if let Some(parent) = sock.parent() {
                    let _ = std::fs::remove_file(parent.join("permission.token"));
                }
            }
        }
        Cmd::Status => {
            let store = imagent_store::Store::open(&db_path).await?;
            // P5-第五批：status 也按 profile 分 keyring 键（此前漏设——profile 模式
            // 下 scoped 键读不到，报误导性错误或显示迁移前旧凭据）。
            store.set_keyring_scope(cli.profile.as_deref().unwrap_or(""));
            // 平台以 config 为准（读不到 config 时回退 ilink；status 允许在
            // login 之前运行，config 可能尚不存在）。
            let platform_name = imagent_core::Config::default_path()
                .and_then(|p| imagent_core::Config::load(&p).ok())
                .map(|c| c.platform)
                .unwrap_or_else(|| "ilink".to_string());
            match platform_name.as_str() {
                // 非扫码平台：凭据在 config/env，不走 store。
                "wecom" => println!(
                    "platform=wecom：凭据来自 config 的 wecom_bot_id / wecom_secret"
                ),
                "feishu" => println!(
                    "platform=feishu：凭据来自 config 的 feishu_app_id + 环境变量 IMAGENT_FEISHU_APP_SECRET（当前{}）",
                    if std::env::var("IMAGENT_FEISHU_APP_SECRET")
                        .map(|s| !s.trim().is_empty())
                        .unwrap_or(false)
                    {
                        "已设置"
                    } else {
                        "未设置"
                    }
                ),
                _ => match store.first_credential("ilink").await? {
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
                },
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
        Cmd::Setup => {
            setup::run().await?;
        }
        Cmd::Service { action } => {
            // service 定义随 --profile 隔离（com.imagent[.<profile>]）。
            match action {
                ServiceAction::Install => service::install(cli.profile.as_deref())?,
                ServiceAction::Uninstall => service::uninstall(cli.profile.as_deref())?,
                ServiceAction::Status => service::status(cli.profile.as_deref())?,
            }
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
/// - `"feishu"` → [`imagent_feishu::FeishuPlatform`]：`feishu_app_id` 取自 config，
///   `app_secret` 取自环境变量 `IMAGENT_FEISHU_APP_SECRET`（keyring bootstrap 为后续 P2），
///   默认 `base_url = https://open.feishu.cn`。
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
        "feishu" => {
            let app_id = config
                .feishu_app_id
                .clone()
                .ok_or_else(|| anyhow!("platform=feishu 需在 config.toml 配置 feishu_app_id"))?;
            // MVP：app_secret 从环境变量读（keyring bootstrap 为后续 P2）。
            let app_secret = std::env::var("IMAGENT_FEISHU_APP_SECRET")
                .map_err(|_| anyhow!("platform=feishu 需设置环境变量 IMAGENT_FEISHU_APP_SECRET"))?;
            let base_url = config
                .feishu_base_url
                .clone()
                .unwrap_or_else(|| "https://open.feishu.cn".to_string());
            // P6-1：群消息 @bot 过滤策略（feishu_require_mention_in_group，默认 true）。
            Ok(Arc::new(imagent_feishu::FeishuPlatform::new(
                app_id,
                app_secret,
                base_url,
                config.feishu_require_mention_in_group,
            )?))
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
    /// 实际运行的平台名（P5：/health 的 logged_in 按平台判定）。
    platform: String,
    /// 预计算的 logged_in（P5-第五批：wecom 凭据在 config，store 查不到）。
    /// None = 按平台动态查（store / env）。
    logged_in_hint: Option<bool>,
}

/// 起 HTTP server（/metrics + /health），独立 tokio task。失败仅 warn。
fn spawn_metrics_server(
    addr: SocketAddr,
    store: imagent_store::Store,
    start_at: Instant,
    platform: String,
    logged_in_hint: Option<bool>,
) {
    let state = HttpState {
        store,
        start_at,
        platform,
        logged_in_hint,
    };
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
                    // 会话白名单：config 种子 ∪ store（P4-5）。
                    let mut chats: Vec<String> = cfg.allowed_chats.clone();
                    let stored_chats = store.list_allowed_chats().await.unwrap_or_default();
                    for c in stored_chats {
                        if !chats.contains(&c) {
                            chats.push(c);
                        }
                    }
                    dispatcher.auth().reload_chats(chats);
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
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!(target: "imagent::ops", "received Ctrl-C, shutting down");
    }
}
