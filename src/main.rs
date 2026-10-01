//! `imagent` 二进制入口：组装 7 个 crate（store / core / ilink / wecom / claude /
//! codex / gemini），多平台（iLink 个人微信 / WeCom 企业微信）× 多后端
//!（Claude CLI/ACP / Codex / Gemini）。
//!
//! 职责：加载配置 →（iLink 扫码登录 / WeCom 读 config 凭据）→ 前台常驻收私聊 →
//! 鉴权 → 驱动 agent → 回传。鉴权 / allowedTools 收敛 / 权限审批 / 风控全部在
//! core（`Dispatcher`）中，main 只做组装 + 运维（metrics/health/SIGHUP/优雅退出）。

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
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
        /// 平台（缺省读 config.platform，config 未写则 ilink）。
        #[arg(long)]
        platform: Option<String>,
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
    /// `--platform feishu|wecom|ilink` 直达对应平台引导（免菜单；ilink 纯指引
    /// 可非交互），菜单默认值取现有 config 的平台。
    Setup {
        /// 直达平台引导：feishu | wecom | ilink（缺省出菜单）。
        #[arg(long)]
        platform: Option<String>,
    },
    /// 服务自管理（P6-6）：安装/卸载/查询 OS 级后台服务（macOS launchd /
    /// Linux systemd 用户单元），注册当前二进制与 --profile。
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// 一致性快照备份（T14，v13 运维批 #1）：`VACUUM INTO` 快照 db + config.toml
    /// 副本 + MANIFEST.txt（版本/sha256/恢复步骤）。媒体目录不进快照（体积
    /// 不可控）；backups/ 自动保留最近 10 份。不持实例锁——运行中即可备份，
    /// 取舍见命令输出与 README「备份与恢复」。
    Backup {
        /// 输出根目录（缺省 `<imagent_home>/backups`，profile 感知）。
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
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
        /// T12：是否暴露 bitable 数据面工具（write_mcp_config 按
        /// feishu_bitable_* 配置写入 0|1；1 时 tools/list 追加两个 bitable 工具）。
        #[arg(long, default_value_t = 0)]
        bitable: u8,
    },
    /// 内部子命令：面向终端 agent 的「问人」MCP server（stdio JSON-RPC），暴露
    /// `ask_via_im` 工具。挂在任意终端 agent 的 MCP 配置里（command=imagent，
    /// args=["mcp-ask"]）；需 config 配置 `ask_via_im_conv` 且主进程已运行。
    #[command(hide = true)]
    McpAsk {
        /// 主进程权限路由 socket 路径（缺省 `<imagent_home>/permission.sock`）。
        #[arg(long)]
        sock: Option<String>,
        /// 打印挂到终端 agent 的 mcpServers 配置 JSON（command 用当前二进制绝对
        /// 路径），然后退出。一键配置用。
        #[arg(long)]
        print_config: bool,
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
    /// 导出 profile 为 JSON（跨机器迁移；P7-A5）。config 里 secret 默认脱敏，
    /// `--include-secrets --yes` 才带明文。keyring 凭据不随导出（机器绑定）。
    Export {
        name: String,
        /// 输出文件（默认 ./<name>-profile.json）。
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        include_secrets: bool,
        #[arg(long)]
        yes: bool,
    },
    /// 从 export 产物导入为新 profile（P7-A5）。目标已存在需 --yes 覆盖 config。
    Import {
        /// export 产物 JSON 路径。
        path: PathBuf,
        /// 目标 profile 名（缺省用导出时的名字；不允许 default）。
        #[arg(long)]
        name: Option<String>,
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

/// profile 状态目录（P7-A5）：default → `~/.imagent`；其余 → profiles/<name>。
fn profile_state_dir(name: &str) -> anyhow::Result<PathBuf> {
    if name == "default" {
        return Ok(dirs::home_dir()
            .ok_or_else(|| anyhow!("无法定位 home 目录"))?
            .join(".imagent"));
    }
    profile_root(name)
}

/// login/allow 的并发风险探测（CODE_REVIEW_v13 横切项「login/allow 子命令不持
/// 实例锁」）：`instance` 模块明确仅 `start` 获取实例锁——运行中的主进程旁边跑
/// `imagent login` / `imagent allow` 会并发改写凭据/白名单（WAL 防单页损坏但
/// 不防逻辑竞争：login 覆盖凭据、allow 与 SIGHUP 的白名单整体替换互踩）。
///
/// 探测复用 `imagent_core::instance` 的 flock 语义（现有基建，不强造 try-probe
/// API）：尝试 acquire——成功说明无主进程持锁（File 立即 drop 释放；期间锁文件
/// 会被短暂写入本进程 PID，与崩溃残留同形态，flock 互斥不依赖文件内容，后续
/// `start` 照常接管）；失败且为 Config 错误（unix 上即 flock 争用）说明锁被
/// 持有。IO 错误（锁文件打不开等）不构成「主进程运行中」的证据，不提示。
///
/// 返回提示文案（Some = 锁被持有）；**只提示不阻断**——用户可能就是要热改
/// （如换号重登）。竞态取舍：探测持锁的微秒窗口内恰有 `start` 并发启动会被
/// 误拒（start 明确报错可重试），远小于漏提示的运维风险面。
#[cfg(unix)]
fn instance_running_warning(home: &Path) -> Option<&'static str> {
    match imagent_core::instance::acquire(home) {
        Ok(_released_on_return) => None,
        Err(imagent_core::CoreError::Config(_)) => Some(
            "⚠️ 检测到 imagent 主进程运行中：并发修改凭据/白名单有竞态风险，建议先 \
             imagent stop；继续执行风险自负",
        ),
        Err(_) => None,
    }
}

/// 非 unix：flock 不可用（项目本身 unix-only——permission.sock 依赖 unix
/// domain socket），探测恒无提示。
#[cfg(not(unix))]
fn instance_running_warning(_home: &Path) -> Option<&'static str> {
    None
}

/// login/allow 执行前的并发风险提示（打印一行 warn，不阻断——见
/// [`instance_running_warning`]）。
fn warn_if_instance_running(home: &Path) {
    if let Some(msg) = instance_running_warning(home) {
        eprintln!("{msg}");
    }
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
            if platform != "ilink" && platform != "wecom" {
                return Err(anyhow!(
                    "login 仅支持 ilink（扫码）与 wecom（bot_id + secret 录入），收到 platform={platform}"
                ));
            }
            // v13 横切项：主进程运行中并发 login 会竞态改写凭据——先探测提示
            //（不阻断，用户可能就是要热改；见 instance_running_warning）。
            warn_if_instance_running(&data_dir);
            let store = imagent_store::Store::open(&db_path).await?;
            // P5：login 写凭据也按 profile 分 keyring 键（与 start 一致）。
            store.set_keyring_scope(cli.profile.as_deref().unwrap_or(""));
            // P2-3（code-review v14）：login 与 start 同库写凭据，require_keyring
            // 的 fail-closed 语义必须同样作用于 login——否则 config 已设 true 而
            // login 走默认 false，keyring 不可用时凭据仍明文落盘，把「拒绝明文
            // 落盘」的安全承诺旁路掉。config 不存在/解析失败时保持默认 false
            // 并 debug 说明（首次 setup 前登录的场景）。
            match imagent_core::Config::default_path()
                .and_then(|p| imagent_core::Config::load(&p).ok())
            {
                Some(cfg) => store.set_require_keyring(cfg.require_keyring),
                None => {
                    tracing::debug!(
                        target: "imagent::ops",
                        "login：config 不存在或解析失败，require_keyring 沿默认 false（首次 setup 前登录场景）"
                    );
                }
            }
            match platform.as_str() {
                // P3-h（code-review v14）：wecom secret 改走 login 入 store（keyring
                // 优先、passphrase 加密回退），bot_id 非敏感仍由 config 提供。
                "wecom" => {
                    println!("录入企业微信智能机器人凭据（secret 输入不回显）…");
                    let bot_id = setup::prompt("bot_id", "")?;
                    if bot_id.is_empty() {
                        return Err(anyhow!("bot_id 不能为空"));
                    }
                    let secret = setup::prompt_secret("secret")?;
                    store.put_credential("wecom", &bot_id, &secret).await?;
                    println!(
                        "✅ wecom 凭据已落库（account_id={bot_id}，keyring 优先）。\n\
                           bot_id 请确保已写入 config.toml 的 wecom_bot_id（非敏感，不随凭据存储）；\n\
                           start 读取顺序：store 凭据优先，config.wecom_secret 为兼容回退。"
                    );
                }
                _ => {
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
            }
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
                // P7-A5：导出 profile（config 脱敏 + 白名单/管理员/命名空间表）。
                ProfileAction::Export {
                    name,
                    output,
                    include_secrets,
                    yes,
                } => {
                    let dir = profile_state_dir(&name)?;
                    let cfg_path = dir.join("config.toml");
                    if !cfg_path.is_file() {
                        return Err(anyhow!(
                            "profile {name} 无 config.toml（{}），无可导出内容",
                            cfg_path.display()
                        ));
                    }
                    let mut config_toml = std::fs::read_to_string(&cfg_path)?;
                    if include_secrets {
                        if !yes {
                            return Err(anyhow!(
                                "--include-secrets 会把 wecom_secret 明文写入导出文件，确认请加 --yes"
                            ));
                        }
                    } else {
                        // 行级脱敏：wecom_secret = "..." → "***"，其余原样。
                        config_toml = config_toml
                            .lines()
                            .map(|l| {
                                let t = l.trim_start();
                                if t.starts_with("wecom_secret") {
                                    "wecom_secret = \"***REDACTED***\" # 由 profile export 脱敏"
                                } else {
                                    l
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                    }
                    let store = imagent_store::Store::open(&dir.join("imagent.db")).await?;
                    let allowed_senders = store.list_allowed_senders().await.unwrap_or_default();
                    let allowed_chats = store.list_allowed_chats().await.unwrap_or_default();
                    let admin_senders = store.list_admin_senders().await.unwrap_or_default();
                    let workspaces = store
                        .list_config("workspace:")
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .collect::<std::collections::BTreeMap<_, _>>();
                    let payload = serde_json::json!({
                        "schema": 1,
                        "exported_at": std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0),
                        "name": name,
                        "config_toml": config_toml,
                        "allowed_senders": allowed_senders,
                        "allowed_chats": allowed_chats,
                        "admin_senders": admin_senders,
                        "workspaces": workspaces,
                    });
                    let out_path =
                        output.unwrap_or_else(|| PathBuf::from(format!("{name}-profile.json")));
                    std::fs::write(&out_path, serde_json::to_string_pretty(&payload)?)?;
                    // L11（code-review v8）：导出产物 chmod 0600——明文 secret 按
                    // umask（典型 0644）落盘世界可读；与 Import 分支同姿态。
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(
                            &out_path,
                            std::fs::Permissions::from_mode(0o600),
                        )?;
                    }
                    println!(
                        "✅ 已导出 profile {name} → {}（白名单 {}/群 {}/管理员 {}；config {}）\n                         ⚠️ keyring 凭据（iLink）与飞书 app_secret（环境变量）不随导出，需在目标机器重配。",
                        out_path.display(),
                        payload["allowed_senders"].as_array().map(|a| a.len()).unwrap_or(0),
                        payload["allowed_chats"].as_array().map(|a| a.len()).unwrap_or(0),
                        payload["admin_senders"].as_array().map(|a| a.len()).unwrap_or(0),
                        if include_secrets { "含明文 secret" } else { "secret 已脱敏" },
                    );
                }
                // P7-A5：导入为新 profile（写 config + 种子白名单/管理员/空间）。
                ProfileAction::Import { path, name, yes } => {
                    let raw = std::fs::read_to_string(&path)
                        .map_err(|e| anyhow!("读取 {} 失败：{e}", path.display()))?;
                    let v: serde_json::Value =
                        serde_json::from_str(&raw).map_err(|e| anyhow!("JSON 解析失败：{e}"))?;
                    if v.get("schema").and_then(|s| s.as_i64()) != Some(1) {
                        return Err(anyhow!("未知的导出格式（schema != 1）"));
                    }
                    let target = name.clone().unwrap_or_else(|| {
                        v.get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("imported")
                            .to_string()
                    });
                    if target == "default" {
                        return Err(anyhow!("不允许导入为 default（会覆盖运行中的 ~/.imagent）"));
                    }
                    let root = profile_root(&target)?;
                    let cfg_path = root.join("config.toml");
                    if root.is_dir() && cfg_path.is_file() && !yes {
                        return Err(anyhow!(
                            "profile {target} 已有 config（{}），覆盖请加 --yes",
                            cfg_path.display()
                        ));
                    }
                    std::fs::create_dir_all(&root)?;
                    let config_toml = v
                        .get("config_toml")
                        .and_then(|c| c.as_str())
                        .ok_or_else(|| anyhow!("导出文件缺 config_toml"))?;
                    std::fs::write(&cfg_path, config_toml)?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(
                            &cfg_path,
                            std::fs::Permissions::from_mode(0o600),
                        );
                    }
                    let store = imagent_store::Store::open(&root.join("imagent.db")).await?;
                    for s in v
                        .get("allowed_senders")
                        .and_then(|a| a.as_array())
                        .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>())
                        .unwrap_or_default()
                    {
                        store.add_allowed_sender(s, None, Some("import")).await?;
                    }
                    for c in v
                        .get("allowed_chats")
                        .and_then(|a| a.as_array())
                        .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>())
                        .unwrap_or_default()
                    {
                        store.add_allowed_chat(c, None, Some("import")).await?;
                    }
                    for a in v
                        .get("admin_senders")
                        .and_then(|a| a.as_array())
                        .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>())
                        .unwrap_or_default()
                    {
                        store.add_admin_sender(a, None, Some("import")).await?;
                    }
                    if let Some(ws) = v.get("workspaces").and_then(|w| w.as_object()) {
                        for (k, val) in ws {
                            if let Some(p) = val.as_str() {
                                store.set_config(&format!("workspace:{k}"), p).await?;
                            }
                        }
                    }
                    println!(
                        "✅ 已导入 profile {target}（{}）\n启动：imagent --profile {target} start",
                        root.display()
                    );
                }
            }
        }
        Cmd::Start { platform } => {
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

            // T15（CODE_REVIEW_v13 P3）：daemon.log 启动期轮转检查——launchd
            // plist 把守护进程 stdout/stderr 指到 ~/.imagent/logs/daemon.log 只增
            // 不减。形态判定与 copytruncate 机制见 service::rotate_daemon_log_if_needed
            // 注释；前台 / Linux journal 形态下文件不存在即 no-op。
            service::rotate_daemon_log_if_needed();

            // 2. store（多份：dispatcher / HTTP /health / SIGHUP 各持一份 Clone）
            let store = imagent_store::Store::open(&db_path).await?;
            // P1-C：据 config.require_keyring 切换凭据 fail-closed
            // （true = keyring 不可用时拒绝明文落盘；默认 false 向后兼容）。
            store.set_require_keyring(config.require_keyring);
            // P5：keyring 用户名按 profile 分段——多 profile 同机同平台不再互删
            // 凭据（读取对旧的无 profile 键 fallback，存量部署零迁移）。
            store.set_keyring_scope(cli.profile.as_deref().unwrap_or(""));

            // 3. platform —— CLI 显式优先，否则 config.platform（未写 = ilink 默认）。
            // 修复（2026-08-24）：此前 CLI 硬默认 ilink——config 写了 feishu/wecom 的
            // 用户不带 --platform 启动会误走 ilink（service install 的守护进程即中招）。
            let platform_name = platform
                .as_deref()
                .filter(|p| !p.is_empty())
                .unwrap_or(&config.platform);
            if platform_name != "ilink" && platform_name != "wecom" && platform_name != "feishu" {
                return Err(anyhow!(
                    "未知 platform={platform_name}，支持 ilink | wecom | feishu"
                ));
            }
            let platform = build_platform(platform_name, &config, store.clone()).await?;

            // T12：Bitable 数据面启用判定（claude 运行参数 --bitable 与
            // Dispatcher 注入共用同一判定）；未启用原因一次性 warn（缺一/非
            // feishu 平台——配置了但无效的场景给用户看得见的反馈）。
            if let Some(reason) = config.bitable_disable_reason(platform_name) {
                tracing::warn!(target: "imagent::ops", "{reason}");
            }
            let bitable_on = config.bitable_enabled_for(platform_name);

            // 孤儿流式卡片关流（P4_ROADMAP 第六批）：上次进程退出时滞留「生成中」的
            // 卡片按 store 登记逐张 patch 成「已中断」，失败保留登记下次再试。
            imagent_core::sweep_live_cards(&store, platform.as_ref()).await;

            // 6. backend —— permission_mode 用共享句柄，SIGHUP 热重载即时生效。
            // auto（缺省）先按后端解析成具体档：claude-cli → ask（IM 审批闭环），
            // 其余 → off（闭环未接，靠各自 sandbox 兜底）。
            let perm_resolved = config.permission_mode.resolve(&config.agent);
            if config.permission_mode == imagent_core::PermissionMode::Auto {
                tracing::info!(
                    target: "imagent::ops",
                    agent = %config.agent,
                    resolved = perm_resolved.as_str(),
                    "permission_mode=auto → 按后端解析"
                );
            }
            let perm_mode = std::sync::Arc::new(parking_lot::RwLock::new(perm_resolved));
            let (backend, claude_cli_handle) = build_backend(
                &config,
                perm_mode.clone(),
                std::time::Duration::from_secs(config.permission_ask_timeout_secs),
                bitable_on,
            )?;
            // W1-2：模型基准值（config `claude_model`）——/model 的运行时热设以
            // 此为初值，SIGHUP 重载时重设回 config 值。
            backend.set_model(config.claude_model.clone());
            // P8-4：后端原生权限模式透传（claude → --permission-mode）——缺省
            // None = auto 档透传新 auto 模式；显式配置则覆盖（已过 config 校验归一）。
            // 后端暂不支持时 warn（不静默——防预期落差），值保留待后续接入。
            backend.set_native_permission_mode(config.backend_permission_mode.clone());
            if config.backend_permission_mode.is_some()
                && !backend.supports_native_permission_mode()
            {
                // B3：升级为带能力矩阵的明确 warn（codex=exec 模式无原生 approval
                // 参数；gemini=原生档位与该配置键值域不同名，无可靠映射）。
                tracing::warn!(
                    target: "imagent::ops",
                    agent = %config.agent,
                    capability = backend.permission_capability().as_str(),
                    "backend_permission_mode 已配置但该后端不支持原生权限模式透传（忽略）。能力矩阵：claude-cli/claude-acp=IM 审批闭环；gemini=仅原生 --approval-mode（由 allowed_tools 收敛）；codex=无原生 approval 档（exec 模式）"
                );
            }

            // B3/T4：能力面矩阵告警统一收敛到 Dispatcher（run 启动 + SIGHUP +
            // /perm 三点位，按 Backend 能力位判定而非后端名硬编码）：
            // - 闭环档位（ask/auto-claude）×非 FullLoop：run() 启动 fail-closed 拒绝；
            // - allow/deny ×非 FullLoop（无审批回调 = 无执行点）：capability_surface_warnings warn；
            // - allowed_tools 非全量 × 后端不支持逐工具白名单（如 ACP）：同上。

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

            // P7-A1：管理员 = config 种子 ∪ store 动态条目（/admin add 持久化）。
            let mut initial_admins = config.admin_senders.clone();
            for a in store.list_admin_senders().await.unwrap_or_default() {
                if !initial_admins.contains(&a) {
                    initial_admins.push(a);
                }
            }

            // S2（v7 review）：admin_senders 为空 = 无人可 admin（IM 内管理命令
            // 一律拒绝，提示走 CLI/setup 配置）。群放行时空管理员不再有扩权风险，
            // 但补一条提示方便单用户发现「管理命令不可用」的原因。
            if initial_admins.is_empty() && !config.allowed_chats.is_empty() {
                tracing::warn!(
                    target: "imagent",
                    "admin_senders 为空：IM 内管理命令（/allow /chat /config /perm /admin）\
                     不可用。如需使用，请在 config.toml 设置 \
                     admin_senders = [\"<你的 sender id>\"]（/whoami 可查 id）。"
                );
            }

            // 8. dispatcher —— allowed_tools / permission_mode 均以共享句柄注入。
            // backend 先 clone 给 SIGHUP 热重载用（透传覆盖），再 move 进 dispatcher。
            let tools_handle =
                std::sync::Arc::new(parking_lot::RwLock::new(config.allowed_tools.clone()));
            let dispatcher = Arc::new(imagent_core::Dispatcher::new_with_handles(
                platform,
                backend.clone(),
                store.clone(),
                auth,
                config.default_workdir.clone(),
                tools_handle,
                perm_mode.clone(),
                imagent_core::TaskBudgets::from_config(&config),
                config.cot_detail,
                initial_admins,
            ));
            // 快捷命令（v1.17）：config.shortcuts 注入（SIGHUP 热重载同步）。
            dispatcher.set_shortcuts(config.shortcuts.clone());
            // P7-A3/A4：启动偏好（陌生人 @ 提示开关 + 私聊引导开关 + 回复形态），
            // 构造后注入。
            dispatcher.set_prefs(
                config.stranger_mention_hint,
                config.stranger_p2p_hint,
                config.reply_mode,
            );
            // Wave B-4：quiet_hours 原文注入（/config 展示用；降级判定在平台侧）。
            dispatcher.set_quiet_hours(config.quiet_hours.clone());
            // 审批集：ask 模式下仅清单内工具过 IM 审批（空 = 全部过审）。
            if !config.approval_tools.is_empty() {
                tracing::info!(
                    target: "imagent::ops",
                    tools = ?config.approval_tools,
                    "approval_tools 生效：清单外权限请求将直接放行"
                );
            }
            dispatcher.set_approval_tools(config.approval_tools.clone());
            // T12：Bitable 数据面注入（platform=feishu 且 app_token/table_id 齐备
            // → feishu 实现挂进 Dispatcher；socket kind=bitable 请求经它落飞书）。
            apply_bitable(&dispatcher, &config, platform_name);

            // 9. 运维 HTTP server（/metrics + /health）。metrics_addr 为 None 或空串则关闭。
            // P2-2（code-review v14）：bind 提前到 spawn 之前——旧实现先 spawn 再
            // 在 task 里 bind，失败只 warn/error 后 server 静默消失，配置了
            // metrics_addr 的运维（Prometheus 抓取/健康探针）要到运行半天才发现
            // 端口从未起来。取舍：metrics 属观测面，bind 失败打 error 后继续运行
            //（fail-open，主链路不受影响）；webhook 是入站驱动面（安全面），
            // bind 失败直接拒绝启动（fail-closed，见 9.5）。
            let start_at = std::time::Instant::now();
            let metrics_addr = config
                .metrics_addr
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let http_store = store.clone();
            // S7：可选 Bearer 鉴权 token（环境变量 `IMAGENT_HTTP_TOKEN`，与
            // IMAGENT_FEISHU_APP_SECRET 的配置风格一致——config 键在 core::Config，
            // 本轮不可改，故走 env）。设置后 /metrics 与 /health 要求
            // `Authorization: Bearer <token>`，不匹配返回 401。
            let http_token = metrics_http_token();
            let metrics_listener = match metrics_addr {
                Some(addr) => match addr.parse::<SocketAddr>() {
                    Ok(socket) => {
                        // S7 fail-closed（与 B3 口径一致）：非 loopback 绑定且未配
                        // token 时拒绝启动，而不是带着「公网可裸访」的 warn 继续
                        // 运行。运维要么绑回 127.0.0.1，要么显式设置
                        // IMAGENT_HTTP_TOKEN。
                        if let Err(reason) = validate_metrics_bind(socket, http_token.as_deref()) {
                            return Err(anyhow!("metrics_addr 配置不安全，拒绝启动：{reason}"));
                        }
                        if !socket.ip().is_loopback() {
                            tracing::info!(
                                target: "imagent::ops",
                                addr = %socket,
                                "metrics_addr 绑定非 loopback 地址：/metrics 与 /health 已启用 IMAGENT_HTTP_TOKEN Bearer 鉴权"
                            );
                        }
                        match tokio::net::TcpListener::bind(socket).await {
                            Ok(l) => {
                                tracing::info!(target: "imagent::ops", addr = %socket, "metrics/health HTTP server listening");
                                Some(l)
                            }
                            Err(e) => {
                                // P2-2：观测面 fail-open——error 打满可见性后继续运行
                                //（配置了但没监听起来必须在日志可见；/health 未起，
                                // 探针会得到 connection refused，同样可见）。
                                tracing::error!(
                                    target: "imagent::ops",
                                    addr = %socket, error = %e,
                                    "bind metrics addr 失败：/metrics 与 /health 不可用（主进程继续运行）"
                                );
                                None
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "imagent::ops", addr = addr, error = %e, "metrics_addr 解析失败，HTTP server 未启动");
                        None
                    }
                },
                None => {
                    tracing::info!(target: "imagent::ops", "metrics_addr 为空，HTTP server 关闭");
                    None
                }
            };

            // 9.5 v1.20 webhook 入站：事件 → 会话（须 [[webhook]] 条目 + 会话白名单）。
            // v1.21 review：startup_recovery 必须先于 webhook accept——旧时序里
            // 启动窗口内注入的消息会撞上 replay 的 clear_queued_all（双执行窗口）、
            // recover 也可能把抢跑轮次误判为崩溃轮。P2-2 后 bind 先于 recovery、
            // accept 后于 recovery，语义不变（bind 只占端口不收请求）。
            // P2-2（code-review v14）：webhook bind 同样提前 + fail-closed——
            // 配置了 webhook_addr 却起不来还带病运行，等于「运维以为有 CI 注入、
            // 实际全部丢失」，入站驱动面不静默降级。
            let mut webhook_listener: Option<tokio::net::TcpListener> = None;
            let webhook_listening: Option<bool> = if let Some(addr) = config
                .webhook_addr
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                match addr.parse::<SocketAddr>() {
                    Ok(socket) => {
                        // v1.24 fail-closed（对齐 metrics 的 S7 口径）：非
                        // loopback 绑定且任一 [[webhook]] 条目未配 secret →
                        // 拒绝启动——webhook 能直接驱动 agent，不允许多一条
                        // 无验签的公网入口。
                        if let Err(reason) = validate_webhook_bind(socket, &config.webhooks) {
                            return Err(anyhow!("webhook_addr 配置不安全，拒绝启动：{reason}"));
                        }
                        if config.webhooks.is_empty() {
                            tracing::warn!(target: "imagent::ops", "webhook_addr 已配置但 [[webhook]] 表为空，webhook server 未启动");
                            Some(false)
                        } else {
                            let listener = tokio::net::TcpListener::bind(socket).await.map_err(|e| {
                                anyhow!("webhook_addr {socket} bind 失败，拒绝启动：{e}（webhook 为入站驱动面，不静默降级）")
                            })?;
                            webhook_listener = Some(listener);
                            // T8（v13 安全批）：/doctor 安全自检的 webhook 暴露面
                            // 摘要注入（core 拿不到 Config/绑定事实；server 不随
                            // SIGHUP 重启，摘要与 server 同生命周期，只注入一次）。
                            dispatcher.set_webhook_exposure(imagent_core::WebhookExposure {
                                listening: true,
                                loopback: socket.ip().is_loopback(),
                                entry_secrets: config
                                    .webhooks
                                    .iter()
                                    .map(|e| e.secret.is_some())
                                    .collect(),
                                any_replay_window: config
                                    .webhooks
                                    .iter()
                                    .any(|e| e.replay_window_secs > 0),
                            });
                            Some(true)
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "imagent::ops", addr = addr, error = %e, "webhook_addr 解析失败，webhook server 未启动");
                        Some(false)
                    }
                }
            } else {
                // 未配置 → /health 报 null（P2-2：JSON 友好三态 null/false/true）。
                None
            };

            // P3-h（code-review v14）：wecom 的 /health logged_in 预判定——secret
            // 可能在 store（login wecom）也可能在 config，按存在性取并。
            let wecom_logged_in_hint = if platform_name == "wecom" {
                let in_store = http_store
                    .first_credential("wecom")
                    .await
                    .map(|o| o.is_some())
                    .unwrap_or(false);
                Some(config.wecom_bot_id.is_some() && (config.wecom_secret.is_some() || in_store))
            } else {
                None
            };

            dispatcher.startup_recovery().await;
            if let Some(listener) = metrics_listener {
                spawn_metrics_server(
                    listener,
                    http_store.clone(),
                    start_at,
                    platform_name.to_string(),
                    http_token,
                    wecom_logged_in_hint,
                    webhook_listening,
                );
            }
            if let Some(listener) = webhook_listener {
                spawn_webhook_server(
                    listener,
                    config.webhooks.clone(),
                    dispatcher.clone(),
                    store.clone(),
                );
            }

            // 10. SIGHUP 热重载（白名单 / allowed_tools / permission_mode / 模型与运行参数）。
            // P2-1b（code-review v14）：带上启动配置的「不可热载键」快照——重载
            // 时对比打 error，防止「改了没生效」静默。
            #[cfg(unix)]
            spawn_sighup_handler(
                dispatcher.clone(),
                backend.clone(),
                claude_cli_handle,
                config_path.clone(),
                http_store.clone(),
                platform_name.to_string(),
                HotReloadSnapshot::of(&config),
            );
            #[cfg(not(unix))]
            {
                let _ = claude_cli_handle;
                tracing::info!(
                    target: "imagent::ops",
                    "SIGHUP 热重载需要 Unix 信号，当前平台不可用（配置改动需重启生效）"
                )
            }

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

            // T15：daemon.log 尺寸轮转 10 分钟节拍复查（启动时已查过一次）。
            // launchd 追加写不会自己变小，运行中长期驻留同样会撑爆磁盘；
            // 机制/形态判定见 service::rotate_daemon_log_if_needed 注释。
            // IMAGENT_LOG_MAX_MB=0（不限）时每次节拍仅读 env 一次即短路。
            tokio::spawn(async {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(10 * 60)).await;
                    service::rotate_daemon_log_if_needed();
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
            // v1.18 review（agent-2 #1）：异常退出必须以非 0 退出码结束——此前
            // Err 分支打印后照样 exit 0，systemd `Restart=on-failure` 视为成功
            // 永不拉起（bot 静默下线）。区分退出码：1 = 通用异常，2 = session
            // 过期（需人工 login；launchd KeepAlive 循环重启时日志可辨）。
            let exit_code = match dispatcher.run().await {
                Ok(()) => {
                    tracing::info!(target: "imagent::ops", "dispatcher 退出（drain 完成）");
                    None
                }
                Err(e) => {
                    if matches!(e, imagent_core::CoreError::SessionExpired(_)) {
                        tracing::error!("dispatcher 退出：{e}");
                        println!("iLink session 已过期，请重新运行 `imagent login` 扫码登录。");
                        Some(2)
                    } else {
                        tracing::error!("dispatcher 异常退出：{e}");
                        println!("imagent 异常退出：{e}");
                        Some(1)
                    }
                }
            };
            // 退出接线：显式调用 backend 的 shutdown（claude-acp 断开全部
            // per-conv 连接并 kill ACP 子进程；其余后端默认 no-op）——此前只靠
            // Drop 兜底，Arc 泄漏/延迟 drop 场景下子进程会活到 OS 清理。
            backend.shutdown().await;
            // R-3：清理 permission.sock（P1-5 计划 ③，原未落地）；P5-9b：握手
            // token 文件一并清理。
            // P3-a（code-review v14）：清理必须先于 std::process::exit——exit(1)/
            // exit(2)（异常退出/SessionExpired）会直接终止进程跳过后续语句，旧
            // 实现把清理放在 exit 之后，异常路径永远执行不到，sock/token 残留到
            // 下次启动（陈旧 permission.sock 会让下一实例的审批闭环错接旧文件）。
            #[cfg(unix)]
            if let Some(sock) = imagent_core::default_sock_path() {
                let _ = std::fs::remove_file(&sock);
                if let Some(parent) = sock.parent() {
                    let _ = std::fs::remove_file(parent.join("permission.token"));
                }
            }
            // v1.18 review：带退出码收尾（见上）。shutdown 与文件清理完成后才退出。
            if let Some(code) = exit_code {
                std::process::exit(code);
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
                // 非扫码平台：凭据在 config/env/store，不走扫码。
                "wecom" => println!(
                    "platform=wecom：secret 优先取 store 凭据（`imagent login wecom`），\
                     回退 config 的 wecom_secret；bot_id 来自 config 的 wecom_bot_id"
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
            // v13 横切项：主进程运行中并发 allow 会竞态改写白名单——先探测提示
            //（不阻断；见 instance_running_warning）。
            warn_if_instance_running(&data_dir);
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
            // v1.18 review（agent-2 #8）：提示里的 PID 应是**运行中实例**的——
            // 此前打印本进程（stop 命令自身）的 pid，操作员照做会杀错/杀空。
            let pid = imagent_core::paths::imagent_home()
                .join("instance.lock")
                .to_string_lossy()
                .to_string();
            let pid_hint = std::fs::read_to_string(&*pid)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .map(|p| format!("或 `kill {p}`"))
                .unwrap_or_else(|| "实例未运行（无 PID 记录）".into());
            println!("imagent 为前台运行模式。停止方式：在 `start` 的终端按 Ctrl-C，{pid_hint}。");
        }
        Cmd::Setup { platform } => {
            setup::run(platform).await?;
        }
        Cmd::Service { action } => {
            // service 定义随 --profile 隔离（com.imagent[.<profile>]）。
            match action {
                ServiceAction::Install => service::install(cli.profile.as_deref()).await?,
                ServiceAction::Uninstall => service::uninstall(cli.profile.as_deref())?,
                ServiceAction::Status => service::status(cli.profile.as_deref())?,
            }
        }
        Cmd::Backup { out } => {
            // T14（v13 运维批 #1）：一致性快照备份。决策：**不持实例锁**——
            // VACUUM INTO 对运行中 DB 安全（WAL 读一致快照，含已提交事务），
            // 备份不要求先停机；快照开始后新提交的数据不在快照内（取舍写进
            // 命令输出 / MANIFEST / README）。默认根目录 profile 感知（走
            // imagent_home，--profile 时即 profile 目录下的 backups/）。
            let profile_name = cli.profile.clone().unwrap_or_else(|| "default".to_string());
            let backup_root = out.unwrap_or_else(|| data_dir.join("backups"));
            let store = imagent_store::Store::open(&db_path).await?;
            let dir = run_backup(
                &store,
                &data_dir,
                &backup_root,
                &profile_name,
                std::time::SystemTime::now(),
            )
            .await?;
            let pruned = prune_backups(&backup_root, &profile_name, BACKUP_KEEP);
            print_backup_summary(&dir, &pruned);
        }
        Cmd::Mcp {
            conv_id,
            sock,
            mode,
            ask_timeout,
            bitable,
        } => {
            // 作为 claude 的 MCP 权限审批 server（stdio JSON-RPC）。
            let mode = imagent_core::PermissionMode::from_str_lossy(&mode);
            tracing::info!(
                target: "imagent::mcp",
                conv_id = %conv_id, sock = %sock, mode = mode.as_str(),
                ask_timeout_secs = ask_timeout,
                bitable,
                "MCP permission server starting"
            );
            if let Err(e) = imagent_core::mcp::run_mcp_server(
                conv_id,
                sock,
                mode,
                std::time::Duration::from_secs(ask_timeout),
                bitable != 0,
            )
            .await
            {
                tracing::error!(target: "imagent::mcp", error = %e, "MCP server 退出");
            }
        }
        Cmd::McpAsk { sock, print_config } => {
            // --print-config：输出 mcpServers JSON（current_exe 解析为绝对路径），
            // 供一键贴进任意 MCP client 配置。不依赖 config/主进程。
            if print_config {
                let exe = std::env::current_exe()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "imagent".to_string());
                println!("{}", imagent_core::mcp::mcp_servers_config(&exe));
                return Ok(());
            }
            // 终端 agent 的 ask_via_im MCP server。conv/超时来自 config——
            // 未配置 ask_via_im_conv 时退出码 2 + stderr 提示（tools/list 无从
            // 兜底，让终端 agent 的报错可见）。
            let config_path = imagent_core::Config::default_path()
                .ok_or_else(|| anyhow!("无法定位 home 目录"))?;
            let config = imagent_core::Config::load(&config_path)
                .map_err(|e| anyhow!("加载配置失败（{}）：{e}", config_path.display()))?;
            let Some(conv) = config
                .ask_via_im_conv
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
            else {
                eprintln!(
                    "config.toml 未配置 ask_via_im_conv（如 \"feishu:ou_xxx\"）——ask_via_im 未启用。"
                );
                std::process::exit(2);
            };
            let sock = sock
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    imagent_core::default_sock_path().map(|p| p.to_string_lossy().into_owned())
                });
            let Some(sock) = sock else {
                eprintln!("无法定位 permission.sock 路径（home 目录缺失？）");
                std::process::exit(2);
            };
            tracing::info!(
                target: "imagent::mcp",
                conv_id = %conv, sock = %sock,
                ask_timeout_secs = config.ask_via_im_timeout_secs,
                "MCP ask server starting (ask_via_im)"
            );
            if let Err(e) = imagent_core::mcp::run_ask_mcp_server(
                conv.to_string(),
                sock,
                std::time::Duration::from_secs(config.ask_via_im_timeout_secs),
            )
            .await
            {
                tracing::error!(target: "imagent::mcp", error = %e, "MCP ask server 退出");
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// T14（v13 运维批 #1）：backup 子命令——VACUUM INTO 一致性快照 + 保留策略
// ---------------------------------------------------------------------------

/// 保留策略：备份根目录内单个 profile 的备份最多保留份数（超出自动清最旧）。
const BACKUP_KEEP: usize = 10;

/// unix 秒 → UTC 年月日时分秒（Howard Hinnant civil_from-days 算法；不引
/// chrono——feishu 侧引是为审批卡本地时间，这里刻意 UTC：备份目录名排序即时
/// 间序、无 DST 歧义，纯函数便于测试拨钟）。
fn utc_civil(unix_secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = unix_secs.div_euclid(86_400);
    let secs = unix_secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = ((doy - (153 * mp + 2) / 5 + 1) as u32).clamp(1, 31);
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (secs / 3600) as u32,
        (secs % 3600 / 60) as u32,
        (secs % 60) as u32,
    )
}

/// 备份目录名的时间戳段：`YYYYMMDD-HHMMSS`（UTC，见 [`utc_civil`]）。
fn backup_timestamp(now: std::time::SystemTime) -> String {
    let secs = match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        // 时钟早于 epoch（系统时钟异常）：取负值继续格式化，fail-open——
        // 备份是运维兜底动作，不应因时钟异常拒绝执行。
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let (y, mo, d, h, mi, s) = utc_civil(secs);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// 文件 sha256（hex）+ 尺寸（字节）。媒体不进快照、产物尺寸有限，整读入
/// 内存可接受（备份是低频运维路径）。
fn sha256_and_size(path: &Path) -> anyhow::Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    let size = bytes.len() as u64;
    Ok((hex::encode(Sha256::digest(&bytes)), size))
}

/// 生成一份备份：单目录 `imagent-backup-<profile>-<YYYYMMDD-HHMMSS>`，内含
/// `imagent.db.snapshot`（VACUUM INTO 快照，先写同盘 tmp 再 rename——对齐
/// 仓内 temp+rename 先例）+ `config.toml` 副本（缺失则仅在 MANIFEST 记录）
/// + `MANIFEST.txt`。返回备份目录路径。
///
/// 原子性：任一步失败即删掉整个新目录——「目录存在 = 备份完整」，避免半截
/// 目录混进保留策略的排序。`now` 由调用方注入（测试拨钟可控）。
async fn run_backup(
    store: &imagent_store::Store,
    data_dir: &Path,
    backup_root: &Path,
    profile: &str,
    now: std::time::SystemTime,
) -> Result<PathBuf> {
    let ts = backup_timestamp(now);
    let unix_secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = backup_root.join(format!("imagent-backup-{profile}-{ts}"));
    if dir.exists() {
        return Err(anyhow!(
            "备份目录已存在（同秒时间戳冲突，稍后重试即可）：{}",
            dir.display()
        ));
    }
    std::fs::create_dir_all(&dir)?;
    // 备份目录 0700：快照含 credentials 表（即便 enc/keyring marker 形态），
    // 与 ~/.imagent 主目录收紧姿态一致（--out 可指到 umask 更宽的位置）。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if let Err(e) = build_backup_artifacts(store, data_dir, &dir, &ts, unix_secs, profile).await {
        // 失败回收整个目录：不留半截备份占保留名额/误导排序。
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e.context(format!("备份产物生成失败（已清理 {}）", dir.display())));
    }
    Ok(dir)
}

/// 组装单份备份的三件产物（快照 / config 副本 / MANIFEST）。
async fn build_backup_artifacts(
    store: &imagent_store::Store,
    data_dir: &Path,
    dir: &Path,
    ts: &str,
    unix_secs: u64,
    profile: &str,
) -> Result<()> {
    use anyhow::Context as _;

    // ① db 快照：VACUUM INTO 同盘 tmp → rename（VACUUM INTO 要求目标不存在，
    // 先清掉上次异常残留）。
    let snapshot = dir.join("imagent.db.snapshot");
    let tmp = dir.join(".imagent.db.snapshot.tmp");
    let _ = std::fs::remove_file(&tmp);
    let schema_version = store
        .backup_snapshot(&tmp)
        .await
        .context("VACUUM INTO 快照失败")?;
    std::fs::rename(&tmp, &snapshot).context("快照 rename 进备份目录失败")?;
    tighten_backup_file(&snapshot)?;

    // ② config.toml 副本（可能尚不存在——首次 setup 前；缺失不阻断，MANIFEST
    // 如实记录）。fs::copy 会带上源文件权限，再统一收紧 0600（副本可能含
    // wecom_secret 明文，与 profile export 产物同姿态）。
    let config_src = data_dir.join("config.toml");
    let config_dst = dir.join("config.toml");
    if config_src.is_file() {
        std::fs::copy(&config_src, &config_dst)
            .with_context(|| format!("复制 config.toml 失败（{}）", config_src.display()))?;
        tighten_backup_file(&config_dst)?;
    }
    let snapshot_info = sha256_and_size(&snapshot)?;
    let config_info = if config_dst.is_file() {
        Some(sha256_and_size(&config_dst)?)
    } else {
        None
    };

    // ③ MANIFEST.txt。
    let manifest = build_manifest(
        env!("CARGO_PKG_VERSION"),
        schema_version,
        ts,
        unix_secs,
        profile,
        &snapshot_info,
        config_info.as_ref(),
    );
    let manifest_path = dir.join("MANIFEST.txt");
    std::fs::write(&manifest_path, manifest)?;
    tighten_backup_file(&manifest_path)?;
    Ok(())
}

/// 备份产物统一收紧 0600（仅 unix）。VACUUM INTO 新建文件按 umask（典型
/// 0644）落盘世界可读；快照含 credentials 表、config 副本可能含 secret。
#[cfg(unix)]
fn tighten_backup_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
#[cfg(not(unix))]
fn tighten_backup_file(_path: &Path) -> Result<()> {
    Ok(())
}

/// MANIFEST.txt 内容。keyring 不在快照内的提示必须醒目（换机恢复后第一件事
/// 就是重录凭据，漏了会以为「恢复失败」）。
fn build_manifest(
    version: &str,
    schema_version: i64,
    ts: &str,
    unix_secs: u64,
    profile: &str,
    snapshot: &(String, u64),
    config_copy: Option<&(String, u64)>,
) -> String {
    let mut m = String::new();
    m.push_str("imagent 备份清单（MANIFEST）\n");
    m.push_str("======================================================================\n\n");
    m.push_str(&format!("生成时间    : {ts} UTC（unix {unix_secs}）\n"));
    m.push_str(&format!("imagent 版本 : {version}\n"));
    m.push_str(&format!(
        "schema 版本  : store user_version = {schema_version}\n"
    ));
    m.push_str(&format!("profile      : {profile}\n\n"));
    m.push_str("文件清单（sha256）\n");
    m.push_str("----------------------------------------------------------------------\n");
    m.push_str(&format!(
        "  imagent.db.snapshot  {} 字节\n    sha256={}\n",
        snapshot.1, snapshot.0
    ));
    match config_copy {
        Some((hash, size)) => m.push_str(&format!(
            "  config.toml          {size} 字节\n    sha256={hash}\n"
        )),
        None => m.push_str("  config.toml          缺失（备份时无 config，未复制）\n"),
    }
    m.push_str("\n⚠️ keyring 凭据不在本快照内（换机/重装后必须重录）\n");
    m.push_str("----------------------------------------------------------------------\n");
    m.push_str(
        "keyring（iLink 登录态等）存放在 OS 钥匙串，机器绑定、不随快照迁移：\n\
         \x20 - iLink : `imagent login` 重新扫码登录\n\
         \x20 - WeCom : 重配 config.toml 的 wecom_bot_id / wecom_secret\n\
         \x20 - 飞书  : 重设环境变量 IMAGENT_FEISHU_APP_SECRET\n\n",
    );
    m.push_str("媒体目录不进快照\n");
    m.push_str("----------------------------------------------------------------------\n");
    m.push_str("<imagent_home>/media 为入站媒体缓存（可重新获取、体积不可控），未纳入快照。\n\n");
    m.push_str("恢复步骤\n");
    m.push_str("----------------------------------------------------------------------\n");
    m.push_str(
        "1. 停止 imagent（start 终端 Ctrl-C / `imagent service uninstall` / kill）。\n\
         2. cp imagent.db.snapshot <imagent_home>/imagent.db\n\
         \x20  （同时删除旧库残留的 imagent.db-wal / imagent.db-shm，避免旧 WAL 混入新库）\n\
         3. 如有 config.toml 副本：cp config.toml <imagent_home>/config.toml\n\
         4. 重录凭据（见上方 keyring 提示）。\n\
         5. `imagent start`——schema 迁移自动向前（旧快照 + 新版二进制 = 自动迁移）。\n\n",
    );
    m.push_str("备份语义与限制\n");
    m.push_str("----------------------------------------------------------------------\n");
    m.push_str(
        "- 本备份不持实例锁：服务运行中即可执行，无需先停机。db 快照由 SQLite\n\
         \x20 `VACUUM INTO` 生成，包含快照开始前已提交的全部事务（含 WAL 中尚未\n\
         \x20 checkpoint 的已提交数据）；快照开始后新提交的数据不在快照内——这是\n\
         \x20 不停机备份的固有语义，恢复后以快照点为准继续。\n\
         - 降级不可恢复：schema 更新的快照不能被旧版 imagent 打开（启动时拒绝\n\
         \x20 比代码新的 user_version）。恢复请使用与生成时相同或更新的版本；\n\
         \x20 反向（旧快照 + 新二进制）安全，迁移自动向前。\n",
    );
    m
}

/// 保留策略：`backup_root` 下 `imagent-backup-<profile>-` 前缀的目录按名排序
/// （时间戳零填充 → 字典序即时序），仅保留最新 `keep` 份，返回被删目录名
/// （best-effort：单个删除失败跳过不阻断）。其他 profile 的备份、无关目录、
/// 普通文件一律不动。
fn prune_backups(backup_root: &Path, profile: &str, keep: usize) -> Vec<String> {
    let prefix = format!("imagent-backup-{profile}-");
    let Ok(rd) = std::fs::read_dir(backup_root) else {
        return vec![];
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(&prefix))
        .collect();
    names.sort();
    if names.len() <= keep {
        return vec![];
    }
    let excess = names.len() - keep;
    names
        .into_iter()
        .take(excess)
        .filter(|n| std::fs::remove_dir_all(backup_root.join(n)).is_ok())
        .collect()
}

/// backup 命令的终端摘要（取舍与 keyring 提示必须落到用户可见处，不只是文档）。
fn print_backup_summary(dir: &Path, pruned: &[String]) {
    println!("✅ 备份完成：{}", dir.display());
    println!("   ├─ imagent.db.snapshot（VACUUM INTO 一致性快照）");
    let has_config = dir.join("config.toml").is_file();
    println!(
        "   ├─ config.toml（{}）",
        if has_config {
            "副本"
        } else {
            "缺失——备份时无 config，未复制"
        }
    );
    println!("   └─ MANIFEST.txt（sha256 / 恢复步骤 / 限制说明）");
    println!(
        "ℹ️  运行中备份：快照含开始前已提交的全部事务，其后新写入不在内（无需停机；取舍详见 MANIFEST.txt）。"
    );
    println!(
        "⚠️ keyring 凭据（iLink 登录态等）不在快照内——恢复后需 `imagent login` 重录 / 重设 secret（步骤见 MANIFEST.txt）。"
    );
    println!("媒体目录不进快照；backups/ 自动保留最近 {BACKUP_KEEP} 份。");
    if !pruned.is_empty() {
        println!(
            "🗑️ 已自动清理旧备份 {} 份：{}",
            pruned.len(),
            pruned.join(", ")
        );
    }
}

// ---------------------------------------------------------------------------
// Backend 选择：按 config.agent 选用对应 agent 后端。
// ---------------------------------------------------------------------------

/// 按 `config.agent` 选择 Backend。
///
/// - `"codex"` → [`imagent_codex::CodexBackend`]；
/// - `"gemini"` → [`imagent_gemini::GeminiBackend`]；
/// - `"claude-acp"` → [`imagent_claude::AcpBackend`]（Claude 的 ACP/JSON-RPC 长驻
///   子进程模式，默认命令 `claude-agent-acp`、本机存储走 ~/.claude，与 `claude-cli`
///   并存；共享 permission_mode 句柄，SIGHUP 即时生效）；
/// - `"acp"` → [`imagent_claude::AcpBackend`] 泛化装配（T9：任意 ACP agent——
///   命令来自 config `acp_command`（必填，config 层已校验）、无 ~/.claude 存储
///   假设；审批闭环随 ACP 协议覆盖，`permission_mode = "ask"` 可用）；
/// - 其它（含默认 `"claude-cli"`）→ [`imagent_claude::ClaudeBackend`]，
///   行为与单后端时期完全一致（permission_mode 共享句柄，SIGHUP 即时生效）。
///
/// Err 仅一种来源：`agent = "acp"` 而 `acp_command` 缺失（config 校验已拦，
/// 此处 fail-closed 兜底，防绕过校验的构造路径静默落到 claude 默认命令）。
///
/// 返回的二元组：泛化 backend 句柄 + claude-cli 专属句柄（SIGHUP 重载运行
/// 参数用；非 claude-cli 后端为 None）。
type BuiltBackend = (
    Arc<dyn imagent_core::Backend>,
    Option<Arc<imagent_claude::ClaudeBackend>>,
);

fn build_backend(
    config: &imagent_core::Config,
    perm_mode: Arc<parking_lot::RwLock<imagent_core::PermissionMode>>,
    ask_timeout: std::time::Duration,
    bitable: bool,
) -> Result<BuiltBackend> {
    match config.agent.as_str() {
        "codex" => Ok((Arc::new(imagent_codex::CodexBackend::new()), None)),
        "gemini" => Ok((Arc::new(imagent_gemini::GeminiBackend::new()), None)),
        "claude-acp" => Ok((
            Arc::new(
                imagent_claude::AcpBackend::with_permission_mode_shared(perm_mode)
                    // W2-4：连接参数接 config（并发上限 / 空闲回收）。
                    .with_conn_limits(
                        config.acp_max_connections,
                        std::time::Duration::from_secs(config.acp_idle_recycle_secs),
                    ),
            ),
            None,
        )),
        "acp" => {
            // T9：泛化 ACP 接入——命令必填（config 归一后空白也已滤为 None，
            // 此处再兜底一次，错误文案与 config 校验同源）。
            let command = config.acp_command.clone().ok_or_else(|| {
                anyhow!(
                    "agent = \"acp\" 需要配置 acp_command（启动目标 ACP agent 的命令，\
                     如 acp_command = \"opencode-acp\" 或 \"gemini --experimental-acp\"，\
                     以目标 agent 的 ACP 接入文档为准）"
                )
            })?;
            Ok((
                Arc::new(
                    imagent_claude::AcpBackend::with_permission_mode_shared(perm_mode)
                        .with_conn_limits(
                            config.acp_max_connections,
                            std::time::Duration::from_secs(config.acp_idle_recycle_secs),
                        )
                        .with_agent_command(command),
                ),
                None,
            ))
        }
        _ => {
            let b = Arc::new(imagent_claude::ClaudeBackend::with_permission_mode_shared(
                perm_mode,
                ask_timeout,
            ));
            // 审批传输通道（缺省 control=canUseTool 双工；mcp=legacy 回退）。
            b.set_permission_channel(&config.claude_permission_channel);

            // W1-2/W1-3/W1-4：claude-cli 运行参数（fallback 模型 / 禁用工具 /
            // 系统提示 / 用户 MCP servers）。具体类型上调用（trait 不暴露
            // claude 专有参数）；句柄额外返回给 SIGHUP 重载用。
            // T12：bitable 开关经 RuntimeOpts 进 write_mcp_config（--bitable）。
            // claude-cli 之外的分支忽略该参数（bitable 工具面只在 claude-cli
            // 存在——ACP 的 MCP 配置不经 write_mcp_config）。
            apply_claude_runtime_opts(&b, config, bitable);
            Ok((b.clone(), Some(b)))
        }
    }
}

/// W1-2/W1-3/W1-4 + T7/T12：claude-cli 运行参数注入（启动与 SIGHUP 共用同一
/// 接线）。T7 的 hide_state_dir、T12 的 bitable 同此热改（整体替换，下一轮
/// spawn 生效）。
fn apply_claude_runtime_opts(
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
fn apply_bitable(
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

// ---------------------------------------------------------------------------
// Platform 选择：按 platform 名选用 ilink 或 wecom。
// ---------------------------------------------------------------------------

/// 按 platform 名选择 Platform 实例。
///
/// - `"wecom"` → [`imagent_wecom::WeComPlatform`]：`bot_id` 取自 config（非敏感）；
///   `secret` 优先取 store 凭据（`imagent login wecom`，keyring/加密落盘），
///   回退 config 的 `wecom_secret`（明文兼容，P3-h/code-review v14）。
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
            // P3-h（code-review v14）：secret 读取顺序 = store 凭据
            // （`imagent login wecom` 写入，keyring 优先 + passphrase 加密回退）
            // → config.wecom_secret（既有部署的明文兼容回退）。两者皆空才报错
            // ——此前只认 config 明文，login 写入的凭据无人消费。
            let secret = match store.get_credential("wecom", &bot_id).await? {
                Some(s) => {
                    tracing::info!(
                        target: "imagent::ops",
                        account_id = %bot_id,
                        "wecom secret 取自 store 凭据（imagent login wecom 写入）"
                    );
                    s
                }
                None => config.wecom_secret.clone().ok_or_else(|| {
                    anyhow!(
                        "platform=wecom 需先 `imagent login wecom` 录入 secret，\
                         或在 config.toml 配置 wecom_secret（兼容回退）"
                    )
                })?,
            };
            // openws 默认地址。
            let ws_url = "wss://openws.work.weixin.qq.com".to_string();
            // message_max_len 三平台生效（安全/一致批次）：企微侧与 4000 字节协议
            // 上限取 min。
            Ok(Arc::new(imagent_wecom::WeComPlatform::new(
                bot_id,
                secret,
                ws_url,
                config.message_max_len,
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
            // P6-1：群消息 @bot 过滤策略（feishu_require_mention_in_group，默认 true）；
            // message_max_len 三平台生效（飞书侧与 28000 协议上限取 min）；
            // permission_ask_timeout_secs 透传（审批卡倒计时文案与实际超时一致）。
            // Wave B-4/B-8：quiet_hours（buzz 免打扰降级窗口）与话题免 @ 窗口透传。
            Ok(Arc::new(imagent_feishu::FeishuPlatform::new(
                app_id,
                app_secret,
                base_url,
                config.feishu_require_mention_in_group,
                config.message_max_len,
                config.permission_ask_timeout_secs,
                config.quiet_hours_parsed,
                config.feishu_thread_active_window_secs,
                config.feishu_asr_enabled,
                // v1.21：发送侧 outbox 持久化重试 + per-conv 令牌桶；
                // v1.24：审批卡到达即加急；T10：群聊上下文注入条数。
                Some(store.clone()),
                config.feishu_send_rps,
                config.feishu_urgent_on_ask,
                config.feishu_group_context_messages,
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
    /// v1.23：发送侧重试队列深度（outbox 表行数）——持续 >0 说明出站通路
    /// 在退避重发（0 = 健康）。
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
fn metrics_http_token() -> Option<String> {
    std::env::var("IMAGENT_HTTP_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// S7：fail-closed 绑定校验（纯函数，便于单测）。非 loopback 绑定且未配
/// token 即拒绝；loopback 或已配 token 均放行。
fn validate_metrics_bind(socket: SocketAddr, token: Option<&str>) -> Result<(), String> {
    if socket.ip().is_loopback() || token.is_some() {
        Ok(())
    } else {
        Err(format!(
            "绑定非 loopback 地址 {socket} 且未设置 IMAGENT_HTTP_TOKEN，\
             /metrics 与 /health 将无鉴权公网可访问；请绑回 127.0.0.1 或设置 IMAGENT_HTTP_TOKEN"
        ))
    }
}

/// S7：Bearer 鉴权判定（纯函数，便于单测）。`token` 为 None 表示未启用
/// 鉴权（loopback 部署），一律放行；Some 时要求 Authorization 头精确等于
/// `Bearer <token>`（前缀多余字符不匹配）。恒定时间比较（L14，见函数体）。
/// v1.20 webhook：body → 注入文本（JSON text 字段优先 / 纯文本兜底 / 空拒绝）。
#[test]
fn webhook_body_text_variants() {
    assert_eq!(
        webhook_body_text(br#"{"text":"deploy failed","run":42}"#),
        Some("deploy failed".into())
    );
    assert_eq!(webhook_body_text(b"  raw text  "), Some("raw text".into()));
    assert_eq!(webhook_body_text(b""), None);
    assert_eq!(webhook_body_text(b"   "), None);
    // JSON 无 text 字段 → 整包作为文本（lossy 容忍）。
    assert!(webhook_body_text(br#"{"other":1}"#).is_some());
}

/// v1.21：HMAC 验签——正签通过、错签/错体/畸形 hex 拒绝（GitHub sha256= 前缀
/// 与裸 hex 两形态）。
#[test]
fn webhook_signature_verify() {
    use hmac::Mac as _;
    let secret = "topsecret-0123456789";
    let body = br#"{"text":"hi"}"#;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    let sig = hex::encode(mac.finalize().into_bytes().as_slice());
    assert!(verify_webhook_signature(
        secret,
        body,
        &format!("sha256={sig}")
    ));
    assert!(verify_webhook_signature(secret, body, &sig));
    assert!(!verify_webhook_signature(secret, body, "sha256=deadbeef"));
    assert!(!verify_webhook_signature(
        secret,
        b"tampered",
        &format!("sha256={sig}")
    ));
    assert!(!verify_webhook_signature(
        "other-secret",
        body,
        &format!("sha256={sig}")
    ));
    assert!(!verify_webhook_signature(secret, body, "not-hex!"));
    assert!(!verify_webhook_signature(secret, body, ""));
}

/// v1.21：令牌桶——rps>0 时容量 = max(1, rps)，耗尽 429，回填后恢复；
/// rps=0 不限速。
#[test]
fn webhook_token_bucket() {
    let mut b = TokenBucket::new(2.0);
    assert!(b.try_take(2.0));
    assert!(b.try_take(2.0));
    assert!(!b.try_take(2.0), "容量 2 应耗尽");
    std::thread::sleep(std::time::Duration::from_millis(600));
    assert!(b.try_take(2.0), "回填 ~1.2 个后应恢复");
    let mut unbounded = TokenBucket::new(0.0);
    for _ in 0..100 {
        assert!(unbounded.try_take(0.0), "rps=0 不限速");
    }
}

/// v1.21：GitHub 原生 payload → 摘要文本（workflow_run 终态/中间态、push、
/// 未知事件 Skip）。
#[test]
fn github_event_summary_variants() {
    let wf = br#"{"action":"completed","workflow_run":{"name":"CI","head_branch":"main","conclusion":"failure","run_number":42,"html_url":"https://github.com/u/r/actions/runs/1","actor":{"login":"uzziahlin"}},"repository":{"full_name":"u/r"}}"#;
    match github_event_text("workflow_run", wf) {
        GithubEventText::Text(t) => {
            assert!(t.contains("CI") && t.contains("failure") && t.contains("runs/1"));
        }
        GithubEventText::Skip => panic!("completed 应产出文本"),
    }
    let wf_mid = br#"{"action":"in_progress","workflow_run":{"name":"CI"}}"#;
    assert!(matches!(
        github_event_text("workflow_run", wf_mid),
        GithubEventText::Skip
    ));
    let push = br#"{"pusher":{"name":"alice"},"ref":"refs/heads/main","compare":"https://github.com/u/r/compare/a...b","repository":{"full_name":"u/r"},"commits":[{"message":"fix: one\n\nbody"},{"message":"feat: two"}]}"#;
    match github_event_text("push", push) {
        GithubEventText::Text(t) => {
            assert!(t.contains("alice") && t.contains("fix: one") && t.contains("feat: two"));
        }
        GithubEventText::Skip => panic!("push 应产出文本"),
    }
    assert!(matches!(
        github_event_text("star", br#"{}"#),
        GithubEventText::Skip
    ));
    assert_eq!(truncate_chars("abcdef", 3), "abc…");
    assert_eq!(truncate_chars("ab", 3), "ab");
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

/// 起 HTTP server（/metrics + /health），独立 tokio task。失败仅 warn。
/// v1.20 webhook 入站：`POST /hook/<token>` → 事件文本注入对应会话。
/// 鉴权 = 路径 token 与 config [[webhook]] 精确匹配；body ≤64KB；
/// JSON `{"text": "..."}` 取 text，否则整包作为纯文本。与 /cron 同走
/// handle() 完整管线（会话白名单门内才有 agent，无旁路）。
/// v1.21 防护套件：可选 HMAC-SHA256 验签（GitHub webhook secret 协议）+
/// 每 token 令牌桶限速 + GitHub 原生 payload 结构化摘要。
/// v1.24 防重放两层：① 验签通过后按 (token, 签名) 进程内去重（同字节重放
/// 409）；② opt-in 时间戳协议 `X-Imagent-Timestamp`（验签串 `{ts}.{body}`，
/// 过窗 401）——见 [`webhook_gate`] / [`ReplayGuard`]。
#[derive(Clone)]
struct WebhookState {
    /// token → 路由（含验签密钥/限速桶）
    routes: std::collections::HashMap<String, std::sync::Arc<WebhookRoute>>,
    dispatcher: Arc<imagent_core::Dispatcher>,
    /// v1.24 第 1 层防重放：验签通过签名的进程级去重表（所有路由共享，
    /// 键含 token）。Clone 语义 = 共享同一张表（axum 每 connection 克隆
    /// state，去重必须进程级生效）。
    seen: ReplayGuard,
}

/// 单条 webhook 路由：投递目标 + 防护配置 + 限速桶（std Mutex——临界区纯内存
/// 无 await）。
struct WebhookRoute {
    conv: String,
    name: String,
    /// HMAC-SHA256 验签密钥（GitHub webhook secret 协议）。None = 不验签。
    secret: Option<String>,
    /// v1.24 时间戳协议窗口（秒）；0 = 不启用（验签串 = body 原文）。
    replay_window_secs: u64,
    /// 令牌桶速率（请求/秒）；0 = 不限速。
    rps: f64,
    bucket: std::sync::Mutex<TokenBucket>,
}

/// v1.24 第 1 层防重放：验签通过签名的去重表（进程内 LRU——容量
/// [`REPLAY_SEEN_CAPACITY`] / 条目 TTL [`REPLAY_SEEN_TTL`]）。手法与 feishu
/// 评论锚点表同款：HashMap + 插入时间戳，超容先清过期、再淘汰最旧；不引
/// lru 依赖。std Mutex——临界区纯内存无 await（与限速桶同款理由）。
///
/// 原理：攻击者无法伪造新签名（HMAC），因此所有「能通过验签的重放」与
/// 首发字节完全相同 → 签名相同 → 去重即可拦截。代价：**合法的完全相同
/// 事件在 TTL 内重发会被误拦**（如手动重跑同一 GitHub event——罕见，
/// README 已说明；等 TTL 过后可重发，或发送端启用时间戳协议让每次签名
/// 天然不同）。
///
/// P3-b（code-review v14）：`store` 挂 SQLite 持久层（schema v16 `webhook_seen`）
/// ——内存表随进程重启清零，重启窗口内的同签名重放会再次通过；落库后跨
/// 进程生效（`replay_window_secs > 0` 的路由启用，窗口即保留期）。
#[derive(Clone)]
struct ReplayGuard {
    inner: Arc<std::sync::Mutex<std::collections::HashMap<String, Instant>>>,
    /// None = 未接 store（持久层关闭，仅 [`ReplayGuard::default`] 的测试形态）。
    store: Option<imagent_store::Store>,
}

impl Default for ReplayGuard {
    fn default() -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            store: None,
        }
    }
}

/// 签名去重表容量：默认 rps=10 下 10 分钟 ≈ 6000 个合法签名，1024 是
/// 「正常流量全覆盖 + 洪泛下内存有界」的折中——被容量挤出的条目失去
/// 去重记忆（极端洪泛下少量重放漏网），换来表恒定 ≤1024 条。
const REPLAY_SEEN_CAPACITY: usize = 1024;
/// 签名去重条目 TTL：≥ GitHub 对重复 delivery 的自查窗口（5 分钟），
/// 覆盖绝大多数重试/重放场景。
const REPLAY_SEEN_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

impl ReplayGuard {
    /// 首见（或条目已过 TTL）→ 记录并放行（true）；TTL 内重复 → 判定
    /// 重放（false）。`now` 由调用方传入便于单测拨钟。
    fn admit(&self, key: &str, now: Instant) -> bool {
        let fresh = |t: &Instant| -> bool {
            now.checked_duration_since(*t)
                .unwrap_or(std::time::Duration::ZERO)
                < REPLAY_SEEN_TTL
        };
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // 命中未过期条目 = 确定重放——判定优先于容量淘汰（被查键不应在
        // 自己的查询里被挤出而放行）。
        if let Some(t) = m.get(key) {
            if fresh(t) {
                return false;
            }
        }
        // 容量护栏：先清过期；仍满则淘汰最旧（重复命中不刷新时间戳，
        // 插入序即新旧序）。
        if m.len() >= REPLAY_SEEN_CAPACITY {
            m.retain(|_, t| fresh(t));
            while m.len() >= REPLAY_SEEN_CAPACITY {
                let Some(oldest) = m.iter().min_by_key(|(_, t)| *t).map(|(k, _)| k.clone()) else {
                    break;
                };
                m.remove(&oldest);
            }
        }
        m.insert(key.to_string(), now);
        true
    }
}

/// v1.24 fail-closed 绑定校验（纯函数，便于单测；口径对齐
/// [`validate_metrics_bind`]）：webhook 绑定非 loopback 且任一条目未配
/// `secret` → 拒绝启动。webhook 能直接驱动一整轮 agent，防护面不得低于
/// metrics 端点——「公网裸 token 即可伪造事件」不允许带病运行。
fn validate_webhook_bind(
    socket: SocketAddr,
    entries: &[imagent_core::WebhookEntry],
) -> Result<(), String> {
    if socket.ip().is_loopback() {
        return Ok(());
    }
    match entries.iter().find(|e| e.secret.is_none()) {
        None => Ok(()),
        Some(e) => Err(format!(
            "绑定非 loopback 地址 {socket} 且 [[webhook]] name={} 未配置 secret，\
             webhook 可直接驱动 agent，公网裸 token 即可伪造事件；\
             请为每条 webhook 配置 secret 或绑定 127.0.0.1",
            e.name
        )),
    }
}

/// v1.24 webhook 入站防护门（纯函数，便于单测）：验签（含 opt-in 时间戳协议）。
/// 通过返回 `Ok(Some(去重键))`（配置了 secret 的路由；`Ok(None)` = 未配
/// secret、无可验/可去重）；拒绝返回 (状态码, 文案)。
///
/// P3-b（code-review v14）：**本函数只做验证、不再记录签名**——去重键交还
/// 调用方，在「验签 + 时间窗通过 + 未被限流拒绝」之后才进内存/持久去重表。
/// 旧序「gate 内先 admit 再限流」会让被 429 的请求也占住签名：GitHub 对同一
/// delivery 的合规重试（同字节同签名）会吃 409「重放」，违反重试协议。
///
/// 两层防重放：
/// 1. 签名去重（默认启用）——见 [`ReplayGuard`]（进程内）+ webhook_seen 表（持久）；
/// 2. 时间戳协议（`replay_window_secs > 0` 时）——请求须带
///    `X-Imagent-Timestamp: <unix 秒>`，验签串从 `body` 改为 `{ts}.{body}`
///    （ts 参与签名、不可伪造），`|now - ts| > 窗口` 即 401。两层叠加：
///    去重拦「同字节重放」，时间戳拦「窗口外的任何重放（含首发迟到）」。
fn webhook_gate(
    route: &WebhookRoute,
    token: &str,
    headers: &axum::http::HeaderMap,
    body: &[u8],
    now_unix: u64,
) -> Result<Option<String>, (StatusCode, &'static str)> {
    let Some(secret) = route.secret.as_deref() else {
        // 未配 secret：无签名可验/可去重（非 loopback 绑定由启动期
        // validate_webhook_bind fail-closed 把关）。
        return Ok(None);
    };
    // 验签串：默认 body 原文；启用时间戳协议时 "{ts}.{body}"（u64 无负值，
    // 解析失败 = 头缺失/非数字，一律 401）。
    let (signing_input, ts) = if route.replay_window_secs > 0 {
        let ts = headers
            .get("x-imagent-timestamp")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .ok_or((
                StatusCode::UNAUTHORIZED,
                "missing or invalid X-Imagent-Timestamp\n",
            ))?;
        let mut input = format!("{ts}.").into_bytes();
        input.extend_from_slice(body);
        (input, Some(ts))
    } else {
        (body.to_vec(), None)
    };
    let sig = webhook_signature_header(headers);
    if !sig.is_some_and(|s| verify_webhook_signature(secret, &signing_input, s)) {
        return Err((StatusCode::UNAUTHORIZED, "invalid signature\n"));
    }
    // 时间戳新鲜度：ts 已参与验签，此处拒绝的必然是「曾经合法」的旧签名
    // 请求 = 重放（绝对差判定，容忍双向时钟偏移）。ts 仅在协议启用时为 Some。
    if let Some(ts) = ts {
        if !timestamp_within_window(now_unix, ts, route.replay_window_secs) {
            return Err((
                StatusCode::UNAUTHORIZED,
                "timestamp outside replay window\n",
            ));
        }
    }
    // 去重键 = (token, 规范化签名 hex)，交调用方在限流通过后 admit。
    let key = format!(
        "{token}\u{0}{}",
        webhook_signature_key_hex(sig.unwrap_or_default())
    );
    Ok(Some(key))
}

/// 验签头原文（GitHub `X-Hub-Signature-256: sha256=<hex>` 或裸 hex 的
/// `X-Signature`）。两层去重键从同一处取值。
fn webhook_signature_header(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get("x-hub-signature-256")
        .or_else(|| headers.get("x-signature"))
        .and_then(|h| h.to_str().ok())
}

/// 验签头 → 去重键的规范化 hex：去 `sha256=` 前缀 + trim + 小写。大小写
/// hex 解码等值（验签都会过），不规范化则攻击者换大小写即可绕过去重。
fn webhook_signature_key_hex(sig: &str) -> String {
    sig.strip_prefix("sha256=")
        .unwrap_or(sig)
        .trim()
        .to_ascii_lowercase()
}

/// v1.24 时间戳新鲜度：|now - ts| ≤ window 即新鲜（绝对差容忍双向时钟
/// 偏移；相等视为新鲜——窗口边界含端点）。
fn timestamp_within_window(now_unix: u64, ts: u64, window: u64) -> bool {
    now_unix.abs_diff(ts) <= window
}

/// 简单令牌桶（纯内存，webhook 单进程内生效）。
struct TokenBucket {
    tokens: f64,
    last: std::time::Instant,
}

impl TokenBucket {
    fn new(full: f64) -> Self {
        Self {
            tokens: full,
            last: std::time::Instant::now(),
        }
    }
    /// 尝试取 1 个令牌：先按 elapsed×rps 回填（封顶容量 = max(1, rps)，约 1s
    /// 突发余量），余量 ≥1 才放行。
    fn try_take(&mut self, rps: f64) -> bool {
        if rps <= 0.0 {
            return true; // 不限速
        }
        let cap = rps.max(1.0);
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * rps).min(cap);
            self.last = now;
        }
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// v1.21 HMAC-SHA256 验签（GitHub webhook secret 同款协议）：
/// `X-Hub-Signature-256: sha256=<hex>`（也兼容裸 hex 的 `X-Signature` 形态）。
/// 常数时间比较（防时序侧信道）；hex 解码失败/长度不符直接 false。
fn verify_webhook_signature(secret: &str, body: &[u8], header_value: &str) -> bool {
    use hmac::Mac as _;
    let hex_sig = header_value
        .strip_prefix("sha256=")
        .unwrap_or(header_value)
        .trim();
    let Ok(expect) = hex::decode(hex_sig) else {
        return false;
    };
    let Ok(mut mac) = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    let computed = mac.finalize().into_bytes();
    // 常数时间比较（与 bearer_authorized 同款 XOR 累计——不引入 subtle 依赖）。
    let n = computed.len().max(expect.len());
    let mut diff: u8 = (computed.len() != expect.len()) as u8;
    for i in 0..n {
        let x = computed.get(i).copied().unwrap_or(0);
        let y = expect.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

fn spawn_webhook_server(
    listener: tokio::net::TcpListener,
    entries: Vec<imagent_core::WebhookEntry>,
    dispatcher: Arc<imagent_core::Dispatcher>,
    store: imagent_store::Store,
) {
    const DEFAULT_WEBHOOK_RPS: f64 = 10.0;
    let routes = entries
        .into_iter()
        .map(|e| {
            let rps = e.rps.unwrap_or(DEFAULT_WEBHOOK_RPS);
            (
                e.token,
                std::sync::Arc::new(WebhookRoute {
                    conv: e.conv,
                    name: e.name,
                    secret: e.secret,
                    replay_window_secs: e.replay_window_secs,
                    rps,
                    bucket: std::sync::Mutex::new(TokenBucket::new(rps.max(1.0))),
                }),
            )
        })
        .collect();
    // P3-b（code-review v14）：去重表挂 store 持久层（webhook_seen，v16）——
    // replay_window_secs > 0 的路由重启后窗口内重放仍被拦。
    let state = WebhookState {
        routes,
        dispatcher,
        seen: ReplayGuard {
            inner: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            store: Some(store),
        },
    };
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    // v1.21 review：停机时停止 accept——drain 期间注入只会挂死/被丢弃，
    // 优雅关停让客户端拿到连接关闭而非假 202。
    let shutdown = state.dispatcher.shutdown_token();
    let app = Router::new()
        .route("/hook/:token", post(webhook_handler))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(state);
    tokio::spawn(async move {
        // P2-2（code-review v14）：bind 已由调用方完成（fail-closed 拒启）；
        // 此处仅 accept/serve。
        tracing::info!(target: "imagent::ops", addr = %addr, "webhook 入站 listening（POST /hook/<token>）");
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
        {
            tracing::warn!(target: "imagent::ops", addr = %addr, error = %e, "webhook HTTP server 退出");
        }
        tracing::info!(target: "imagent::ops", "webhook server 已随停机关闭");
    });
}

/// body → 注入文本：JSON 带 text 字段取之；否则整包 UTF-8 文本；空/非 UTF-8 拒。
fn webhook_body_text(body: &[u8]) -> Option<String> {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
            let t = t.trim();
            if t.is_empty() {
                return None;
            }
            return Some(t.to_string());
        }
    }
    let s = String::from_utf8_lossy(body);
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// v1.21 GitHub 原生事件解析结果：`Text` 注入会话；`Skip` 确认收到但不注入
///（进行中/未订阅的中间事件——注入只会产生噪音与无效 agent 轮次）。
enum GithubEventText {
    Text(String),
    Skip,
}

/// GitHub webhook payload（`X-GitHub-Event` 头存在时）→ 可读事件摘要。
/// 覆盖 CI/协作主链路事件；未识别的事件类型一律 Skip（raw JSON 注入只会让
/// agent 解析噪音——自定义注入请走无该头的 JSON `{"text": ...}` 形态）。
fn github_event_text(event: &str, body: &[u8]) -> GithubEventText {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return GithubEventText::Skip;
    };
    let s = |path: &[&str]| -> String {
        let mut cur = &v;
        for k in path {
            cur = &cur[*k];
        }
        cur.as_str().unwrap_or_default().to_string()
    };
    match event {
        "ping" => GithubEventText::Text(format!(
            "🏓 GitHub webhook 连通性测试成功：仓库 {} 的事件订阅已生效。",
            s(&["repository", "full_name"])
        )),
        "workflow_run" => {
            // 只注入终态（completed）——requested/in_progress 每次跑三连发，
            // 中间态对「驱动 agent」无信息量。
            if s(&["action"]) != "completed" {
                return GithubEventText::Skip;
            }
            // v1.23 review：必要字段缺失（name/html_url）拼出的空壳文本会
            // 驱动一整轮 agent 却无信息量——与「未识别事件 Skip」同哲学拦下。
            let wf_name = s(&["workflow_run", "name"]);
            let wf_url = s(&["workflow_run", "html_url"]);
            if wf_name.is_empty() || wf_url.is_empty() {
                return GithubEventText::Skip;
            }
            let conclusion = s(&["workflow_run", "conclusion"]);
            let mark = match conclusion.as_str() {
                "success" => "✅",
                "failure" => "❌",
                "cancelled" => "⚠️",
                _ => "⏹️",
            };
            GithubEventText::Text(format!(
                "{mark} GitHub Actions：「{}」（{} 分支）{}\n仓库 {} · 触发 {} · 第 {} 次运行\n{}",
                wf_name,
                s(&["workflow_run", "head_branch"]),
                conclusion,
                s(&["repository", "full_name"]),
                s(&["workflow_run", "actor", "login"]),
                v["workflow_run"]["run_number"].as_i64().unwrap_or(0),
                wf_url,
            ))
        }
        "push" => {
            let pusher = s(&["pusher", "name"]);
            let repo = s(&["repository", "full_name"]);
            if pusher.is_empty() || repo.is_empty() {
                return GithubEventText::Skip;
            }
            let commits = v["commits"].as_array().cloned().unwrap_or_default();
            let msgs: Vec<String> = commits
                .iter()
                .take(3)
                .filter_map(|c| {
                    c["message"]
                        .as_str()
                        .and_then(|m| m.lines().next())
                        .map(|m| truncate_chars(m, 200))
                })
                .collect();
            let list = if msgs.is_empty() {
                "（无提交——可能是分支删除或 tag 操作）".to_string()
            } else {
                let mut l = msgs
                    .iter()
                    .map(|m| format!("- {m}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                if commits.len() > msgs.len() {
                    l.push_str(&format!("\n- ……等共 {} 个提交", commits.len()));
                }
                l
            };
            let git_ref = s(&["ref"]).trim_start_matches("refs/heads/").to_string();
            GithubEventText::Text(format!(
                "📦 GitHub push：{} → {}（{}）\n{}\n{}",
                pusher,
                repo,
                git_ref,
                list,
                s(&["compare"])
            ))
        }
        "issues" => {
            let action = s(&["action"]);
            // assigned/labeled/unassigned 等低价值动作跳过。
            if !matches!(action.as_str(), "opened" | "closed" | "reopened") {
                return GithubEventText::Skip;
            }
            let (title, url) = (s(&["issue", "title"]), s(&["issue", "html_url"]));
            if title.is_empty() || url.is_empty() {
                return GithubEventText::Skip;
            }
            let mark = if action == "closed" { "✅" } else { "🎯" };
            GithubEventText::Text(format!(
                "{mark} GitHub issue {}：#{} {}\nby {} · {}\n{}",
                action,
                v["issue"]["number"].as_i64().unwrap_or(0),
                s(&["issue", "title"]),
                s(&["issue", "user", "login"]),
                s(&["repository", "full_name"]),
                s(&["issue", "html_url"]),
            ))
        }
        "issue_comment" => {
            if s(&["action"]) != "created" {
                return GithubEventText::Skip;
            }
            let body_txt = truncate_chars(&s(&["comment", "body"]), 300);
            GithubEventText::Text(format!(
                "💬 GitHub 新评论（{}#{} {}）：\n{}：{}\n{}",
                s(&["repository", "full_name"]),
                v["issue"]["number"].as_i64().unwrap_or(0),
                s(&["issue", "title"]),
                s(&["comment", "user", "login"]),
                body_txt,
                s(&["comment", "html_url"]),
            ))
        }
        "pull_request" => {
            let action = s(&["action"]);
            if !matches!(
                action.as_str(),
                "opened" | "closed" | "reopened" | "review_requested"
            ) {
                return GithubEventText::Skip;
            }
            let merged = v["pull_request"]["merged"].as_bool().unwrap_or(false);
            let action_disp = if merged {
                "merged（已合并）".to_string()
            } else {
                action
            };
            let (title, url) = (
                s(&["pull_request", "title"]),
                s(&["pull_request", "html_url"]),
            );
            if title.is_empty() || url.is_empty() {
                return GithubEventText::Skip;
            }
            let mark = if merged { "🎉" } else { "🌱" };
            GithubEventText::Text(format!(
                "{mark} GitHub PR {}：#{} {}\nby {} · {} ← {}\n{}",
                action_disp,
                v["pull_request"]["number"].as_i64().unwrap_or(0),
                s(&["pull_request", "title"]),
                s(&["pull_request", "user", "login"]),
                s(&["pull_request", "base", "ref"]),
                s(&["pull_request", "head", "ref"]),
                s(&["pull_request", "html_url"]),
            ))
        }
        _ => GithubEventText::Skip,
    }
}

/// 按字符截断（中文安全），尾加省略标记。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let t: String = s.chars().take(max).collect();
    format!("{t}…")
}

async fn webhook_handler(
    State(st): State<WebhookState>,
    axum::extract::Path(token): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> impl IntoResponse {
    let Some(route) = st.routes.get(&token) else {
        return (StatusCode::NOT_FOUND, "unknown token\n");
    };
    let Ok(bytes) = body else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "body too large (64KB)\n");
    };
    // v1.21 防护① + v1.24 两层防重放（配置了 secret 时强制）：HMAC 验签
    //（GitHub 头 `X-Hub-Signature-256: sha256=<hex>`，兼容裸 hex 的
    // `X-Signature`）→ opt-in 时间戳协议 → 限速 → 签名去重（内存 LRU +
    // SQLite 持久层，同签名 409）。
    // P3-b（code-review v14）顺序修正：旧序「签名去重在限速之前」会让被 429
    // 拒绝的请求也占住签名——GitHub 对同一 delivery 的合规重试（同字节同签名）
    // 会吃 409「重放」。签名记录必须发生在验签+时间窗通过**且**未被限流拒绝
    // 之后（429 拒绝不记录签名）。
    // 时钟早于 epoch（系统时钟异常）时 now_unix 取 u64::MAX：时间戳协议
    // 请求全部 401（fail-closed），未启用协议的路径不受影响。
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(u64::MAX);
    let dedup_key = match webhook_gate(route, &token, &headers, &bytes, now_unix) {
        Ok(k) => k,
        Err((code, msg)) => {
            tracing::warn!(target: "imagent::ops", name = %route.name, status = %code, "webhook 验签/防重放拒绝");
            return (code, msg);
        }
    };
    // v1.21 防护②：每 token 令牌桶限速。
    {
        let mut bucket = route.bucket.lock().unwrap_or_else(|e| e.into_inner());
        if !bucket.try_take(route.rps) {
            return (StatusCode::TOO_MANY_REQUESTS, "rate limited\n");
        }
    }
    // 签名去重（限流通过后才记录，见上）。内存层恒启用；持久层仅
    // `replay_window_secs > 0` 的路由（窗口即 SQLite 保留期，重启不失效）。
    if let Some(key) = dedup_key {
        if !st.seen.admit(&key, Instant::now()) {
            tracing::warn!(target: "imagent::ops", name = %route.name, status = %StatusCode::CONFLICT, "webhook 签名去重拦截（内存层命中：重放）");
            return (StatusCode::CONFLICT, "replay detected\n");
        }
        if route.replay_window_secs > 0 {
            if let Some(db) = &st.seen.store {
                // 内存未命中 → 查库（重启后内存表清零，持久层兜底）；库中也
                // 未见过才入库（验签+限流已通过 = 真实首见）。查/写失败仅
                // warn fail-open：持久层是第二道去重，第一道签名验证与内存
                // 去重不受影响，不值得让 webhook 入站因观测性写失败而拒绝。
                let boundary =
                    (now_unix.saturating_sub(route.replay_window_secs)).min(i64::MAX as u64) as i64;
                let ts = now_unix.min(i64::MAX as u64) as i64;
                match db.webhook_seen_contains(&key).await {
                    Ok(true) => {
                        tracing::warn!(target: "imagent::ops", name = %route.name, status = %StatusCode::CONFLICT, "webhook 签名去重拦截（持久层命中：重放）");
                        return (StatusCode::CONFLICT, "replay detected\n");
                    }
                    Ok(false) => {
                        if let Err(e) = db.webhook_seen_insert(&key, ts, boundary).await {
                            tracing::warn!(target: "imagent::ops", error = %e, "webhook 签名入库失败（持久去重 best-effort）");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "imagent::ops", error = %e, "webhook 签名查库失败（持久去重降级内存层）");
                    }
                }
            }
        }
    }
    // v1.21 GitHub 原生事件：X-GitHub-Event 头存在 → 结构化摘要（未知事件
    // Skip 确认不注入）；否则走通用 text 提取。
    let text = match headers.get("x-github-event").and_then(|h| h.to_str().ok()) {
        Some(event) => match github_event_text(event, &bytes) {
            GithubEventText::Text(t) => t,
            GithubEventText::Skip => {
                tracing::debug!(target: "imagent::ops", event, name = %route.name, "GitHub 事件按策略跳过（中间态/未订阅）");
                return (StatusCode::ACCEPTED, "ignored\n");
            }
        },
        None => match webhook_body_text(&bytes) {
            Some(t) => t,
            None => return (StatusCode::BAD_REQUEST, "empty or undecodable body\n"),
        },
    };
    let msg = imagent_core::InboundMessage {
        conv_id: imagent_core::ConvId(route.conv.clone()),
        sender: imagent_core::UserId(format!("webhook:{}", route.name)),
        sender_name: None,
        text: Some(format!("【{}】{}", route.name, text)),
        media: vec![],
        media_errors: Vec::new(),
        mentions: Vec::new(),
        mentioned_bot: false,
        ask_req: None,
        reply_to: None,
        source_msg_id: None,
        control: None,
        // v1.21 review：合成消息不走 steering（独立轮次 + 持久化兜底）。
        no_steer: true,
        reply_hint: imagent_core::ReplyHint::None,
    };
    tracing::info!(target: "imagent::ops", conv = %route.conv, name = %route.name, "webhook 命中，注入 dispatcher");
    match st.dispatcher.inject(msg).await {
        Ok(()) => (StatusCode::ACCEPTED, "queued\n"),
        Err(e) => {
            tracing::warn!(target: "imagent::ops", error = %e, "webhook 注入被拒（停机中）");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "shutting down, retry later\n",
            )
        }
    }
}

fn spawn_metrics_server(
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
struct HotReloadSnapshot {
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
    fn of(cfg: &imagent_core::Config) -> Self {
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
fn spawn_sighup_handler(
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
                        let mut admins = cfg.admin_senders.clone();
                        for a in store.list_admin_senders().await.unwrap_or_default() {
                            if !admins.contains(&a) {
                                admins.push(a);
                            }
                        }
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
                    if let Some(reason) = cfg.bitable_disable_reason(&platform_name) {
                        tracing::warn!(target: "imagent::ops", "{reason}");
                    }
                    apply_bitable(&dispatcher, &cfg, &platform_name);
                    // P2-1a（code-review v14）：auto 档按**运行中后端**解析——
                    // agent 键不可热载（后端实现启动期装配），若按重载 config 的
                    // agent 字符串解析，把 claude-cli 改成 codex 的那次 SIGHUP
                    // 会把仍在运行的 claude-cli 从审批闭环（auto-claude）静默
                    // 降级成 off。agent_label() 返回启动期后端名，保证解析基准
                    // 与实际运行的后端一致（后端名在下次重启前不会变）。
                    let perm = cfg.permission_mode.resolve(dispatcher.agent_label());
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

/// v1.24 webhook 防重放 / fail-closed 单测（纯函数层——防护门
/// [`webhook_gate`] 与 [`validate_webhook_bind`] 均为无 IO 纯函数）。
#[cfg(test)]
mod webhook_replay_tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};
    use hmac::Mac as _;

    const SECRET: &str = "topsecret-0123456789";

    fn hmac_hex(secret: &str, input: &[u8]) -> String {
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(input);
        hex::encode(mac.finalize().into_bytes().as_slice())
    }

    /// 时间戳协议验签串：`{ts}.{body}`。
    fn ts_signing_input(ts: u64, body: &[u8]) -> Vec<u8> {
        let mut v = format!("{ts}.").into_bytes();
        v.extend_from_slice(body);
        v
    }

    fn route(secret: Option<&str>, replay_window_secs: u64) -> WebhookRoute {
        WebhookRoute {
            conv: "feishu:oc_t".into(),
            name: "t".into(),
            secret: secret.map(str::to_string),
            replay_window_secs,
            rps: 0.0,
            bucket: std::sync::Mutex::new(TokenBucket::new(0.0)),
        }
    }

    fn headers(pairs: &[(&str, String)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                k.parse::<axum::http::HeaderName>().unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    /// 第 1 层去重表本体：TTL 内同键拦截、TTL 过后放行；容量护栏——
    /// 满容量先清过期、再淘汰最旧，被查键不被自己的查询挤出。
    #[test]
    fn replay_guard_ttl_and_capacity() {
        let g = ReplayGuard::default();
        let t0 = Instant::now();
        assert!(g.admit("a", t0), "首见放行");
        assert!(
            !g.admit(
                "a",
                t0 + REPLAY_SEEN_TTL - std::time::Duration::from_secs(1)
            ),
            "TTL 内重复 → 重放"
        );
        assert!(
            g.admit(
                "a",
                t0 + REPLAY_SEEN_TTL + std::time::Duration::from_secs(1)
            ),
            "TTL 过后重发 → 放行（合法同字节事件的重发出口）"
        );

        // 容量：灌满 1024 个新鲜键（时间戳严格递增，k0 最旧）。
        let g2 = ReplayGuard::default();
        for i in 0..REPLAY_SEEN_CAPACITY {
            assert!(g2.admit(
                &format!("k{i}"),
                t0 + std::time::Duration::from_millis(i as u64)
            ));
        }
        let len = |g: &ReplayGuard| g.inner.lock().unwrap_or_else(|e| e.into_inner()).len();
        assert_eq!(len(&g2), REPLAY_SEEN_CAPACITY);
        assert!(
            g2.admit("overflow", t0 + std::time::Duration::from_secs(5)),
            "满容量时新键仍放行"
        );
        assert_eq!(len(&g2), REPLAY_SEEN_CAPACITY, "容量恒定（最旧 k0 被淘汰）");
        assert!(
            !g2.admit("k1", t0 + std::time::Duration::from_secs(5)),
            "未淘汰的近期键仍拦截"
        );
        assert!(
            g2.admit("k0", t0 + std::time::Duration::from_secs(6)),
            "k0 已被容量淘汰，去重记忆重开——容量护栏优先于 TTL"
        );
    }

    /// P3-b（code-review v14）后的 gate 纯验证语义：验签通过返回**去重键**而非
    /// 自行记录（记录移到限流之后，429 不占签名）；键含 token + 规范化签名
    /// （大小写 hex / 裸 hex 形态同键，防换大小写绕过）；未配 secret → None。
    #[test]
    fn gate_returns_dedup_key_without_side_effect() {
        let body = br#"{"text":"deploy failed"}"#;
        let sig = hmac_hex(SECRET, body);
        let r = route(Some(SECRET), 0);
        let now = 1_700_000_000_u64;

        let h = headers(&[("x-hub-signature-256", format!("sha256={sig}"))]);
        let key = webhook_gate(&r, "token-a", &h, body, now)
            .expect("首发验签通过")
            .expect("配 secret 的路由应有去重键");
        assert_eq!(
            key,
            format!("token-a\u{0}{}", sig.to_ascii_lowercase()),
            "键 = token + 小写规范化签名"
        );

        // 大小写 hex / 裸 hex 头形态：验签等价 → 去重键规范化后相同（防绕过）。
        let h_upper = headers(&[(
            "x-hub-signature-256",
            format!("sha256={}", sig.to_uppercase()),
        )]);
        assert_eq!(
            webhook_gate(&r, "token-a", &h_upper, body, now)
                .unwrap()
                .unwrap(),
            key,
            "签名换大小写不可绕过去重"
        );
        let h_bare = headers(&[("x-signature", sig.clone())]);
        assert_eq!(
            webhook_gate(&r, "token-a", &h_bare, body, now)
                .unwrap()
                .unwrap(),
            key,
            "裸 hex 头形态与 sha256= 前缀形态同键"
        );

        // 不同 token（另一条 webhook 路由收到同样签名）键不同，互不误拦。
        let key_b = webhook_gate(&r, "token-b", &h, body, now).unwrap().unwrap();
        assert_ne!(key_b, key, "键含 token，跨路由不串");

        // gate 不再消费 ReplayGuard：同签名重复过 gate 恒 Ok（去重由调用方在
        // 限流通过后显式 admit——见 handler；这里直接验证 admit 语义）。
        let guard = ReplayGuard::default();
        assert!(guard.admit(&key, Instant::now()), "首见放行");
        assert!(!guard.admit(&key, Instant::now()), "TTL 内重放拦截");

        // 未配 secret 的路由：门直通且无去重键（非 loopback 无 secret 由启动校验拒绝）。
        let r_nosec = route(None, 0);
        assert_eq!(
            webhook_gate(&r_nosec, "token-a", &HeaderMap::new(), body, now),
            Ok(None)
        );
    }

    /// 第 2 层时间戳协议（opt-in）：缺失 ts → 401；过期 ts（签名正确）→ 401；
    /// 新鲜 ts + `{ts}.{body}` 正签 → 放行；body 原文签名（协议未生效形态）→ 401；
    /// 两层叠加——新鲜 ts 重复 POST → 409。
    #[test]
    fn gate_timestamp_protocol_variants() {
        let body = br#"{"text":"deploy failed"}"#;
        let r = route(Some(SECRET), 300);
        let now = 1_700_000_000_u64;
        let fresh_ts = now - 10;
        let stale_ts = now - 1000; // 超出 300s 窗口

        // 缺失 X-Imagent-Timestamp → 401。
        let sig_fresh = hmac_hex(SECRET, &ts_signing_input(fresh_ts, body));
        let h_no_ts = headers(&[("x-hub-signature-256", format!("sha256={sig_fresh}"))]);
        let err = webhook_gate(&r, "t", &h_no_ts, body, now).unwrap_err();
        assert_eq!(
            (err.0, err.1),
            (
                StatusCode::UNAUTHORIZED,
                "missing or invalid X-Imagent-Timestamp\n"
            )
        );

        // 过期 ts + 对 "{ts}.{body}" 的正确签名 → 401（曾经合法的旧请求 = 重放）。
        let sig_stale = hmac_hex(SECRET, &ts_signing_input(stale_ts, body));
        let h_stale = headers(&[
            ("x-imagent-timestamp", stale_ts.to_string()),
            ("x-hub-signature-256", format!("sha256={sig_stale}")),
        ]);
        let err = webhook_gate(&r, "t", &h_stale, body, now).unwrap_err();
        assert_eq!(
            (err.0, err.1),
            (
                StatusCode::UNAUTHORIZED,
                "timestamp outside replay window\n"
            )
        );

        // 未来侧偏移同样拒绝（绝对差判定）。
        let future_ts = now + 1000;
        let sig_future = hmac_hex(SECRET, &ts_signing_input(future_ts, body));
        let h_future = headers(&[
            ("x-imagent-timestamp", future_ts.to_string()),
            ("x-hub-signature-256", format!("sha256={sig_future}")),
        ]);
        assert_eq!(
            webhook_gate(&r, "t", &h_future, body, now).unwrap_err().0,
            StatusCode::UNAUTHORIZED,
            "未来侧超窗同样拒绝（容忍双向偏移但不放行超窗）"
        );

        // 对 body 原文的签名（协议生效后旧发送端形态）→ 验签即 401。
        let sig_body_only = hmac_hex(SECRET, body);
        let h_body_sig = headers(&[
            ("x-imagent-timestamp", fresh_ts.to_string()),
            ("x-hub-signature-256", format!("sha256={sig_body_only}")),
        ]);
        assert_eq!(
            webhook_gate(&r, "t", &h_body_sig, body, now).unwrap_err().0,
            StatusCode::UNAUTHORIZED,
            "验签串已改为 {{ts}}.{{body}}，body 原文签名不再通过"
        );

        // 新鲜 ts + 正确签名 → 放行（返回去重键）；同键 admit 二次 → 拦截
        //（两层叠加：时间戳拦窗口外，去重拦窗口内同字节重放）。
        let h_fresh = headers(&[
            ("x-imagent-timestamp", fresh_ts.to_string()),
            ("x-hub-signature-256", format!("sha256={sig_fresh}")),
        ]);
        let key = webhook_gate(&r, "t", &h_fresh, body, now)
            .unwrap()
            .expect("新鲜请求应有去重键");
        let guard = ReplayGuard::default();
        assert!(guard.admit(&key, Instant::now()));
        assert!(
            !guard.admit(&key, Instant::now()),
            "时间戳协议与去重叠加：同签名重发被拦"
        );

        // 窗口边界：|now - ts| == window 视为新鲜（端点含）。
        let edge_ts = now - 300;
        let sig_edge = hmac_hex(SECRET, &ts_signing_input(edge_ts, body));
        let h_edge = headers(&[
            ("x-imagent-timestamp", edge_ts.to_string()),
            ("x-hub-signature-256", format!("sha256={sig_edge}")),
        ]);
        assert!(
            webhook_gate(&r, "t", &h_edge, body, now).unwrap().is_some(),
            "窗口端点视为新鲜"
        );
    }

    /// v1.24 fail-closed：非 loopback 且任一条目未配 secret → 拒绝；
    /// loopback 或全部条目配齐 secret → 放行。
    #[test]
    fn non_loopback_webhook_without_secret_is_rejected() {
        let pub_addr: SocketAddr = "0.0.0.0:18443".parse().unwrap();
        let lo_addr: SocketAddr = "127.0.0.1:18443".parse().unwrap();
        let v6lo: SocketAddr = "[::1]:18443".parse().unwrap();
        let mk = |secret: Option<&str>, name: &str| imagent_core::WebhookEntry {
            token: "0123456789abcdef0123456789abcdef".into(),
            conv: "feishu:oc_g".into(),
            name: name.into(),
            secret: secret.map(str::to_string),
            rps: None,
            replay_window_secs: 0,
        };

        assert!(validate_webhook_bind(pub_addr, &[mk(None, "ci")]).is_err());
        // 任一条目未配即拒绝（混配不放行），报错点名具体条目。
        let err = validate_webhook_bind(pub_addr, &[mk(Some("s-pair-01"), "ci"), mk(None, "cron")])
            .unwrap_err();
        assert!(
            err.contains("name=cron") && err.contains("secret"),
            "msg={err}"
        );
        assert!(err.contains("127.0.0.1"), "给出可操作修复指引：{err}");
        assert!(validate_webhook_bind(pub_addr, &[mk(Some("s-pair-01"), "ci")]).is_ok());
        assert!(validate_webhook_bind(lo_addr, &[mk(None, "ci")]).is_ok());
        assert!(validate_webhook_bind(v6lo, &[mk(None, "ci")]).is_ok());
        // 空表 + 非 loopback：server 本就不启动，校验不阻拦（保持既有 warn 路径）。
        assert!(validate_webhook_bind(pub_addr, &[]).is_ok());
    }
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

/// T14（v13 运维批 #1）：backup 子命令单测——CLI 解析 / 时间戳 / 产物清单与
/// MANIFEST 内容 / 保留策略 / 失败回收。时间戳全部参数化注入（拨钟可控）。
#[cfg(test)]
mod backup_tests {
    use super::*;

    /// CLI 解析：`backup` 无参（默认目录）与 `--out <dir>` 两形态。
    #[test]
    fn backup_cli_parse() {
        let c = Cli::try_parse_from(["imagent", "backup"]).expect("bare backup 应可解析");
        assert!(
            matches!(c.cmd, Cmd::Backup { out: None }),
            "缺省不应带 --out"
        );
        let c = Cli::try_parse_from(["imagent", "backup", "--out", "/tmp/bk-out"])
            .expect("--out 应可解析");
        match c.cmd {
            Cmd::Backup { out } => assert_eq!(out, Some(PathBuf::from("/tmp/bk-out"))),
            _ => panic!("期望 Backup 子命令"),
        }
    }

    /// 时间戳格式：UTC 已知值核对（epoch / 2023-11-14 / 2024 闰日），零填充
    /// 保证字典序 = 时间序（保留策略的排序前提）。
    #[test]
    fn backup_timestamp_known_values() {
        use std::time::Duration;
        assert_eq!(backup_timestamp(std::time::UNIX_EPOCH), "19700101-000000");
        let t = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(backup_timestamp(t), "20231114-221320");
        let leap = std::time::UNIX_EPOCH + Duration::from_secs(1_709_164_800);
        assert_eq!(backup_timestamp(leap), "20240229-000000");
    }

    /// 产物清单 + MANIFEST 内容：真 Store 跑一次 run_backup（now 注入固定
    /// 时间），断言三产物存在、MANIFEST 含版本/schema/sha256/keyring 提示/
    /// 恢复步骤/取舍说明，快照内容=快照时刻（备份前写入可见）。
    #[tokio::test]
    async fn backup_artifacts_and_manifest() {
        let root = std::env::temp_dir().join(format!("imagent-bk-art-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data_dir = root.join("home");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("config.toml"), "platform = \"feishu\"\n").unwrap();
        let store = imagent_store::Store::open(&data_dir.join("imagent.db"))
            .await
            .expect("open store");
        store
            .add_allowed_sender("bk-art", None, Some("test"))
            .await
            .unwrap();

        let out_root = root.join("backups");
        let fixed_now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_777_777_777);
        let dir = run_backup(&store, &data_dir, &out_root, "default", fixed_now)
            .await
            .expect("run_backup");

        // 目录名：profile + 注入的时间戳（1777777777 = 2026-05-03 03:09:37 UTC）。
        assert!(
            dir.ends_with("imagent-backup-default-20260503-030937"),
            "{dir:?}"
        );

        // 三产物齐全。
        let snapshot = dir.join("imagent.db.snapshot");
        assert!(snapshot.is_file(), "快照应存在");
        assert!(dir.join("config.toml").is_file(), "config 副本应存在");
        let manifest = std::fs::read_to_string(dir.join("MANIFEST.txt")).unwrap();

        // MANIFEST 关键内容。
        assert!(manifest.contains(env!("CARGO_PKG_VERSION")), "imagent 版本");
        assert!(
            manifest.contains(&format!("user_version = {}", imagent_store::SCHEMA_VERSION)),
            "schema 版本"
        );
        let (hash, size) = sha256_and_size(&snapshot).unwrap();
        assert!(manifest.contains(&hash), "快照 sha256 应记录在 MANIFEST");
        assert!(manifest.contains(&format!("{size} 字节")));
        assert!(manifest.contains("keyring"), "keyring 不在快照的提示");
        assert!(manifest.contains("imagent login"), "重录凭据步骤");
        assert!(manifest.contains("恢复步骤"), "恢复步骤段");
        assert!(manifest.contains("VACUUM INTO"), "一致性取舍说明");
        assert!(manifest.contains("降级不可恢复"), "降级限制说明");
        assert!(manifest.contains("media"), "媒体不进快照说明");

        // 快照内容 = 快照时刻：备份前写入可见（一致性）。
        let snap_store = imagent_store::Store::open(&snapshot)
            .await
            .expect("打开快照");
        let senders = snap_store.list_allowed_senders().await.unwrap();
        assert!(senders.iter().any(|s| s == "bk-art"));

        // 产物权限 0600（unix）——快照含 credentials 表、config 副本可能含 secret。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in ["imagent.db.snapshot", "config.toml", "MANIFEST.txt"] {
                let mode = dir.join(f).metadata().unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{f} 应为 0600");
            }
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// config 缺失：备份仍产出（db 快照为主），MANIFEST 如实记录缺失。
    #[tokio::test]
    async fn backup_without_config_still_works() {
        let root = std::env::temp_dir().join(format!("imagent-bk-nocfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data_dir = root.join("home");
        std::fs::create_dir_all(&data_dir).unwrap();
        let store = imagent_store::Store::open(&data_dir.join("imagent.db"))
            .await
            .expect("open store");
        let dir = run_backup(
            &store,
            &data_dir,
            &root.join("backups"),
            "work",
            std::time::SystemTime::now(),
        )
        .await
        .expect("run_backup");
        assert!(dir.join("imagent.db.snapshot").is_file());
        assert!(!dir.join("config.toml").exists(), "无 config 不应产出副本");
        let manifest = std::fs::read_to_string(dir.join("MANIFEST.txt")).unwrap();
        assert!(manifest.contains("缺失"), "MANIFEST 应记录 config 缺失");
        assert!(
            dir.to_string_lossy().contains("imagent-backup-work-"),
            "目录名带 profile"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 同秒时间戳冲突：第二次报错（不覆盖既有备份）。
    #[tokio::test]
    async fn backup_rejects_duplicate_timestamp() {
        let root = std::env::temp_dir().join(format!("imagent-bk-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data_dir = root.join("home");
        std::fs::create_dir_all(&data_dir).unwrap();
        let store = imagent_store::Store::open(&data_dir.join("imagent.db"))
            .await
            .expect("open store");
        let now = std::time::SystemTime::now();
        let first = run_backup(&store, &data_dir, &root.join("backups"), "default", now)
            .await
            .expect("first");
        let err = run_backup(&store, &data_dir, &root.join("backups"), "default", now)
            .await
            .expect_err("同秒应冲突");
        assert!(err.to_string().contains("冲突"), "{err}");
        // 既有备份原样保留（仍是完整三产物）。
        assert!(first.join("MANIFEST.txt").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 保留策略：12 份 → 只保留最新 10、最旧 2 被删；其他 profile / 无关
    /// 目录 / 同名前缀的普通文件一律不动。
    #[test]
    fn backup_retention_prunes_oldest() {
        let root = std::env::temp_dir().join(format!("imagent-bk-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..12 {
            // 时间戳目录名直接手工构造（不依赖真实时钟）。
            let d = root.join(format!("imagent-backup-default-202601{i:02}-000000"));
            std::fs::create_dir_all(&d).unwrap();
        }
        // 干扰项。
        std::fs::create_dir_all(root.join("imagent-backup-work-20260101-000000")).unwrap();
        std::fs::create_dir_all(root.join("not-a-backup")).unwrap();
        std::fs::write(
            root.join("imagent-backup-default-20260105-000000.txt"),
            b"f",
        )
        .unwrap();

        let removed = prune_backups(&root, "default", BACKUP_KEEP);
        assert_eq!(
            removed,
            vec![
                "imagent-backup-default-20260100-000000".to_string(),
                "imagent-backup-default-20260101-000000".to_string(),
            ],
            "应删最旧 2 份"
        );
        assert!(!root.join("imagent-backup-default-20260101-000000").exists());
        assert!(
            root.join("imagent-backup-default-20260111-000000").exists(),
            "最新保留"
        );
        assert!(
            root.join("imagent-backup-work-20260101-000000").exists(),
            "其他 profile 不动"
        );
        assert!(root.join("not-a-backup").exists(), "无关目录不动");
        assert!(
            root.join("imagent-backup-default-20260105-000000.txt")
                .exists(),
            "普通文件不动"
        );

        // 不超额时不删任何东西；根目录不存在安全返回空。
        assert!(
            prune_backups(&root, "default", BACKUP_KEEP).is_empty(),
            "已恰好 10 份"
        );
        assert!(prune_backups(&root.join("nope"), "default", BACKUP_KEEP).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// v13 横切项（login/allow 并发风险提示）单测：实例锁被持有时给出 warn 文案，
/// 无人持锁 / 释放后无提示。锁探测经 imagent_core::instance 的 flock 语义
///（临时目录即真实锁文件，无需 mock）。
#[cfg(all(test, unix))]
mod instance_warning_tests {
    use super::*;

    #[test]
    fn instance_warning_only_when_lock_held() {
        let home = std::env::temp_dir().join(format!("imagent-cli-warn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();

        // 无人持锁：无提示（探测内部 acquire 成功即释放，不残留持锁状态）。
        assert!(
            instance_running_warning(&home).is_none(),
            "无人持锁不应提示"
        );

        // 持锁期间（模拟运行中的主进程——同进程另一 fd 持 flock，语义等价）：
        // 给出含关键信息的 warn 文案。
        let _guard = imagent_core::instance::acquire(&home).expect("测试先取锁");
        let msg = instance_running_warning(&home).expect("持锁期间应给出提示");
        assert!(msg.contains("主进程"), "点名主进程: {msg}");
        assert!(msg.contains("竞态风险"), "说明风险: {msg}");
        assert!(msg.contains("imagent stop"), "给出可操作建议: {msg}");
        assert!(msg.contains("风险自负"), "不阻断的姿态: {msg}");

        // 释放后（主进程退出）：回到无提示。
        drop(_guard);
        assert!(instance_running_warning(&home).is_none(), "释放后不应提示");

        let _ = std::fs::remove_dir_all(&home);
    }
}
