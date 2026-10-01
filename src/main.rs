//! `imagent` 二进制入口：组装 7 个 crate（store / core / ilink / wecom / claude /
//! codex / gemini），多平台（iLink 个人微信 / WeCom 企业微信）× 多后端
//!（Claude CLI/ACP / Codex / Gemini）。
//!
//! 职责：加载配置 →（iLink 扫码登录 / WeCom 读 config 凭据）→ 前台常驻收私聊 →
//! 鉴权 → 驱动 agent → 回传。鉴权 / allowedTools 收敛 / 权限审批 / 风控全部在
//! core（`Dispatcher`）中，main 只做组装 + 运维（metrics/health/SIGHUP/优雅退出）。
//!
//! 模块拆分（批 A1）：`webhook`（入站驱动面）/ `backup`（快照备份）/ `ops`
//! （metrics/SIGHUP/优雅退出 + 启动与热载共用装配段）各成模块；本文件只留
//! CLI 定义、子命令分发与 Start 装配序列。

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

mod backup;
mod ops;
mod service;
mod setup;
mod webhook;

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
    /// 打印将写入的服务定义（launchd plist / systemd unit）到 stdout——不安装、
    /// 不写文件、不需要 root（B2 deploy 合流：deploy/ 静态模板由本命令生成，
    /// 刷新方法见 deploy/README.md；默认不附凭据环境快照，防泄漏）。
    Print {
        /// 输出格式：launchd | systemd（缺省按当前 OS）。
        #[arg(long, value_enum)]
        format: Option<service::UnitFormat>,
        /// 服务二进制路径（缺省当前二进制；刷新 deploy 模板固定传 /usr/local/bin/imagent）。
        #[arg(long, value_name = "PATH")]
        exe: Option<String>,
        /// 平台覆盖（缺省读 config.platform；无 config 的机器须传本参）。
        #[arg(long)]
        platform: Option<String>,
        /// macOS plist 日志路径（StandardOut/ErrorPath；缺省 ~/.imagent/logs/daemon.log；
        /// systemd 格式忽略——走 journal）。
        #[arg(long, value_name = "PATH")]
        log_path: Option<String>,
        /// 附带当前 shell 的凭据环境变量快照（对齐 install 写盘全貌；默认不附）。
        #[arg(long)]
        with_env: bool,
    },
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

            // Wave C（出站可靠性收敛）：未知 kind sweeper——outbox 行的 kind
            // 不属于当前在跑的 driver（如平台从 config 撤下后的历史积压、kind
            // 拼写漂移）时推后 + 告警 + 超限回收，防永久滞留撑表（v14 P3o
            // 防御的泛化形态，见 core::outbox 模块文档）。ilink 无 outbox
            // kind（不接入的决策同样记档在那里），空名单即清剿全部历史行。
            {
                let driven_kinds: Vec<&str> = match platform_name {
                    "feishu" => vec![imagent_feishu::OUTBOX_KIND],
                    "wecom" => vec![imagent_wecom::OUTBOX_KIND],
                    _ => Vec::new(),
                };
                imagent_core::outbox::spawn_unknown_kind_sweeper(store.clone(), &driven_kinds);
            }

            // T12：Bitable 数据面启用判定（claude 运行参数 --bitable 与
            // Dispatcher 注入共用同一判定）；未启用原因一次性 warn（缺一/非
            // feishu 平台——配置了但无效的场景给用户看得见的反馈）。
            ops::warn_bitable_disable_reason(&config, platform_name);
            let bitable_on = config.bitable_enabled_for(platform_name);

            // 孤儿流式卡片关流（P4_ROADMAP 第六批）：上次进程退出时滞留「生成中」的
            // 卡片按 store 登记逐张 patch 成「已中断」，失败保留登记下次再试。
            imagent_core::sweep_live_cards(&store, platform.as_ref()).await;

            // 6. backend —— permission_mode 用共享句柄，SIGHUP 热重载即时生效。
            // auto（缺省）先按后端解析成具体档：claude-cli → ask（IM 审批闭环），
            // 其余 → off（闭环未接，靠各自 sandbox 兜底）。
            let perm_resolved = ops::resolve_permission_mode(config.permission_mode, &config.agent);
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
            let initial = ops::union_config_store(
                &config.allowed_senders,
                store.list_allowed_senders().await.unwrap_or_default(),
            );
            let initial_chats = ops::union_config_store(
                &config.allowed_chats,
                store.list_allowed_chats().await.unwrap_or_default(),
            );
            let auth = imagent_core::Auth::with_chats(initial, initial_chats);
            let discovery = auth.is_discovery();

            // P7-A1：管理员 = config 种子 ∪ store 动态条目（/admin add 持久化）。
            let initial_admins = ops::union_config_store(
                &config.admin_senders,
                store.list_admin_senders().await.unwrap_or_default(),
            );

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
            ops::apply_bitable(&dispatcher, &config, platform_name);

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
            let http_token = ops::metrics_http_token();
            let metrics_listener = match metrics_addr {
                Some(addr) => match addr.parse::<SocketAddr>() {
                    Ok(socket) => {
                        // S7 fail-closed（与 B3 口径一致）：非 loopback 绑定且未配
                        // token 时拒绝启动，而不是带着「公网可裸访」的 warn 继续
                        // 运行。运维要么绑回 127.0.0.1，要么显式设置
                        // IMAGENT_HTTP_TOKEN。
                        if let Err(reason) =
                            ops::validate_metrics_bind(socket, http_token.as_deref())
                        {
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
                        if let Err(reason) =
                            webhook::validate_webhook_bind(socket, &config.webhooks)
                        {
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
                ops::spawn_metrics_server(
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
                webhook::spawn_webhook_server(
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
            ops::spawn_sighup_handler(
                dispatcher.clone(),
                backend.clone(),
                claude_cli_handle,
                config_path.clone(),
                http_store.clone(),
                platform_name.to_string(),
                ops::HotReloadSnapshot::of(&config),
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
                ops::shutdown_signal().await;
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
                ServiceAction::Print {
                    format,
                    exe,
                    platform,
                    log_path,
                    with_env,
                } => service::print(
                    cli.profile.as_deref(),
                    format.unwrap_or_else(service::UnitFormat::current_os),
                    exe.as_deref(),
                    platform.as_deref(),
                    log_path.as_deref(),
                    with_env,
                )?,
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
            let dir = backup::run_backup(
                &store,
                &data_dir,
                &backup_root,
                &profile_name,
                std::time::SystemTime::now(),
            )
            .await?;
            let pruned = backup::prune_backups(&backup_root, &profile_name, backup::BACKUP_KEEP);
            backup::print_backup_summary(&dir, &pruned);
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
            ops::apply_claude_runtime_opts(&b, config, bitable);
            Ok((b.clone(), Some(b)))
        }
    }
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
            // Wave C（出站可靠性收敛）：接入 store outbox——「明确未送达」的
            // 发送失败（出站 channel 关闭）落盘退避重发，断连/重启不再丢回复。
            Ok(Arc::new(imagent_wecom::WeComPlatform::new(
                bot_id,
                secret,
                ws_url,
                config.message_max_len,
                Some(store.clone()),
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

/// B2（deploy 合流）CLI 单测：`service print` 参数位解析——缺省可解析（格式
/// 按当前 OS、平台读 config）、全 flag 显式、`--format` 非法值拒绝、顶层
/// `--profile` 贯通。渲染内容断言见 service.rs template_shape_matches_deploy_refresh。
#[cfg(test)]
mod service_print_cli_tests {
    use super::*;

    #[test]
    fn service_print_parses_with_defaults() {
        let cli = Cli::try_parse_from(["imagent", "service", "print"]).expect("缺省形态应可解析");
        match cli.cmd {
            Cmd::Service {
                action:
                    ServiceAction::Print {
                        format,
                        exe,
                        platform,
                        log_path,
                        with_env,
                    },
            } => {
                assert_eq!(format, None, "缺省格式在运行期按当前 OS 解析");
                assert_eq!(exe, None);
                assert_eq!(platform, None);
                assert_eq!(log_path, None);
                assert!(!with_env, "默认不附凭据环境快照（防泄漏）");
            }
            _ => panic!("应解析为 Service::Print"),
        }
    }

    #[test]
    fn service_print_parses_all_flags() {
        let cli = Cli::try_parse_from([
            "imagent",
            "service",
            "print",
            "--format",
            "systemd",
            "--exe",
            "/usr/local/bin/imagent",
            "--platform",
            "feishu",
            "--log-path",
            "/usr/local/var/log/imagent.log",
            "--with-env",
        ])
        .expect("全 flag 形态应可解析");
        match cli.cmd {
            Cmd::Service {
                action:
                    ServiceAction::Print {
                        format,
                        exe,
                        platform,
                        log_path,
                        with_env,
                    },
            } => {
                assert_eq!(format, Some(service::UnitFormat::Systemd));
                assert_eq!(exe.as_deref(), Some("/usr/local/bin/imagent"));
                assert_eq!(platform.as_deref(), Some("feishu"));
                assert_eq!(log_path.as_deref(), Some("/usr/local/var/log/imagent.log"));
                assert!(with_env);
            }
            _ => panic!("应解析为 Service::Print"),
        }
    }

    #[test]
    fn service_print_rejects_unknown_format() {
        assert!(
            Cli::try_parse_from(["imagent", "service", "print", "--format", "sysv"]).is_err(),
            "--format 只接受 launchd | systemd"
        );
    }

    #[test]
    fn service_print_accepts_global_profile() {
        let cli = Cli::try_parse_from(["imagent", "--profile", "work", "service", "print"])
            .expect("--profile 全局位应贯通到 print");
        assert!(matches!(
            cli.cmd,
            Cmd::Service {
                action: ServiceAction::Print { .. }
            }
        ));
        assert_eq!(cli.profile.as_deref(), Some("work"));
    }
}
