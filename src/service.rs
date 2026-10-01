//! `imagent service install|uninstall|status`（P6-6）：把 deploy/ 下的静态模板
//! 变成程序化安装——注册**当前二进制**与当前 `--profile`，凭据类环境变量
//! （IMAGENT_FEISHU_APP_SECRET / IMAGENT_HOME）随安装时的进程环境写进服务定义。
//!
//! - macOS：launchd 用户代理 `~/Library/LaunchAgents/com.imagent[.<profile>].plist`
//!   （`launchctl unload/load`；日志 `~/.imagent/logs/daemon.log`）
//! - Linux：systemd 用户单元 `~/.config/systemd/user/imagent[-<profile>].service`
//!   （`systemctl --user daemon-reload && enable --now`；日志走 journalctl --user -u）

use std::path::PathBuf;

use anyhow::{anyhow, Result};

/// 服务标识：default profile → `com.imagent`；命名 profile → `com.imagent.<name>`。
fn label(profile: Option<&str>) -> String {
    match profile {
        None | Some("") => "com.imagent".to_string(),
        Some(p) => format!("com.imagent.{p}"),
    }
}

/// 写入路径（launchd plist / systemd unit）。
fn unit_path(profile: Option<&str>) -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("无法定位 home 目录"))?;
    #[cfg(target_os = "macos")]
    {
        Ok(home
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", label(profile))))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let name = label(profile).replace("com.imagent", "imagent");
        Ok(home
            .join(".config/systemd/user")
            .join(format!("{name}.service")))
    }
    #[cfg(not(unix))]
    {
        let _ = profile;
        Err(anyhow!(
            "service 自管理仅支持 macOS（launchd）与 Linux（systemd 用户单元）"
        ))
    }
}

/// launchd plist 模板：注册当前二进制 + start + 可选 --profile；把安装进程持有的
/// 凭据环境变量快照进服务（KeepAlive 崩溃自动拉起）。
/// v1.23 review：plist/unit 模板值转义——secret 含 &/</" 时裸内插会产生
/// 非法 XML（launchd 加载失败）或注入额外 Environment= 指令（systemd）。
/// B2（deploy 合流）：xml_escape/render_plist 与 unit_escape/render_unit 均为纯
/// 字符串渲染，**不再按 OS cfg 门控**——`service print --format` 需在任意平台
/// 生成两种格式（deploy/ 模板刷新 + 双平台测试覆盖），OS 差异只留在
/// install/uninstall/status 的落盘/加载路径。
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// systemd unit 值清洗：引号/换行/分号在 Environment="k=v" 语境可断句注入
/// ——直接替换为安全字符（这些值是路径/secret，正常不含此类字符）。
fn unit_escape(s: &str) -> String {
    s.replace(['"', '\n', ';'], "_")
}

fn render_plist(
    exe: &str,
    profile: Option<&str>,
    platform: &str,
    envs: &[(String, String)],
    log: &str,
) -> String {
    let mut args = format!(
        "        <string>{}</string>\n        <string>start</string>\n        <string>--platform</string>\n        <string>{}</string>",
        xml_escape(exe),
        xml_escape(platform)
    );
    if let Some(p) = profile.filter(|p| !p.is_empty()) {
        args.push_str(&format!(
            "\n        <string>--profile</string>\n        <string>{}</string>",
            xml_escape(p)
        ));
    }
    let mut env = String::new();
    if !envs.is_empty() {
        env.push_str("    <key>EnvironmentVariables</key>\n    <dict>\n");
        for (k, v) in envs {
            env.push_str(&format!(
                "        <key>{}</key>\n        <string>{}</string>\n",
                xml_escape(k),
                xml_escape(v)
            ));
        }
        env.push_str("    </dict>\n");
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \x20   <key>Label</key>\n    <string>{label}</string>\n\n\
         \x20   <key>ProgramArguments</key>\n    <array>\n{args}\n    </array>\n\n{env}\
         \x20   <key>RunAtLoad</key>\n    <true/>\n\n\
         \x20   <key>KeepAlive</key>\n    <true/>\n\n\
         \x20   <key>StandardOutPath</key>\n    <string>{log_}</string>\n\
         \x20   <key>StandardErrorPath</key>\n    <string>{log_}</string>\n\
         </dict>\n</plist>\n",
        label = label(profile),
        log_ = xml_escape(log),
    )
}

/// systemd 用户单元模板（ExecStart 同参数；日志 journalctl）。
fn render_unit(
    exe: &str,
    profile: Option<&str>,
    platform: &str,
    envs: &[(String, String)],
) -> String {
    let mut exec = format!(
        "{} start --platform {}",
        unit_escape(exe),
        unit_escape(platform)
    );
    if let Some(p) = profile.filter(|p| !p.is_empty()) {
        exec.push_str(&format!(" --profile {}", unit_escape(p)));
    }
    let mut env = String::new();
    for (k, v) in envs {
        env.push_str(&format!(
            "Environment=\"{}={}\"\n",
            unit_escape(k),
            unit_escape(v)
        ));
    }
    let name = label(profile).replace("com.imagent", "imagent");
    format!(
        "[Unit]\nDescription=imagent — IM ↔ agent gateway ({name})\n\
         After=network-online.target\nWants=network-online.target\n\n\
         [Service]\nType=simple\nExecStart={exec}\n{env}\
         Restart=on-failure\nRestartSec=5\n\n\
         [Install]\nWantedBy=default.target\n"
    )
}

// ---------------------------------------------------------------------------
// B2（deploy 合流）：`service print` —— deploy/ 静态模板与 install 程序化路径的
// 单一事实源。此前两套内容手工分叉（静态模板缺 `--platform`，v13 记过此债：
// 复制模板装出的守护进程会误走缺省平台解析）。现约定：deploy/ 下模板 = 本
// 命令在固定参数下的输出（刷新方法见 deploy/README.md 顶部），勿手改模板。
// ---------------------------------------------------------------------------

/// 服务定义输出格式（`service print --format`）。
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitFormat {
    /// macOS launchd 用户代理 plist。
    Launchd,
    /// Linux systemd 用户单元。
    Systemd,
}

impl UnitFormat {
    /// 当前 OS 的缺省格式（print 不传 --format 时用）。
    pub fn current_os() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::Launchd
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            Self::Systemd
        }
        #[cfg(not(unix))]
        {
            Self::Systemd
        }
    }
}

/// `service print`：把 install 会写盘的服务定义渲染到 stdout。**不安装、不写
/// 任何文件、不调 launchctl/systemctl、不需要 root**——幂等可重复，退出码恒 0
///（输入非法才非 0）。与 [`install`] 的差异仅三点，均为「预览/模板」场景取舍：
/// - 平台解析：`--platform` 覆盖 > config.platform（无 config 时允许 `--platform`
///   直给——模板生成机未必初始化过 config；install 则强制读 config 防静默装错）。
/// - 凭据环境变量快照：**默认不附**（`--with-env` 才附）——print 的输出常被粘贴
///   进 issue/文档/入库存模板，凭据默认不进场；对齐 install 全貌时再显式开启。
/// - 二进制路径：`--exe` 覆盖 > 当前二进制（刷新 deploy 模板固定传
///   `/usr/local/bin/imagent`，避免开发者本机 target/ 路径入库）。
pub fn print(
    profile: Option<&str>,
    format: UnitFormat,
    exe: Option<&str>,
    platform: Option<&str>,
    log_path: Option<&str>,
    with_env: bool,
) -> Result<()> {
    let exe = match exe {
        Some(e) => e.to_string(),
        None => std::env::current_exe()
            .map_err(|e| anyhow!("定位当前二进制失败：{e}（可用 --exe 覆盖）"))?
            .to_string_lossy()
            .into_owned(),
    };
    let platform_name = match platform {
        Some(p) => p.to_string(),
        None => {
            let cfg_path = imagent_core::Config::default_path()
                .ok_or_else(|| anyhow!("无法定位 config 路径（print 可用 --platform 跳过读取）"))?;
            imagent_core::Config::load(&cfg_path)
                .map_err(|e| {
                    anyhow!(
                        "读取 {} 失败：{e}\n（print 可用 --platform 覆盖，无需先 setup）",
                        cfg_path.display()
                    )
                })?
                .platform
        }
    };
    let envs = if with_env { capture_envs() } else { Vec::new() };
    let content = match format {
        UnitFormat::Launchd => {
            // 日志路径仅 launchd 语义（systemd 走 journal）——--log-path 在
            // Systemd 格式下无对应字段，忽略（README 已注明）。
            let log = log_path
                .map(str::to_string)
                .unwrap_or_else(|| daemon_log_path().to_string_lossy().into_owned());
            render_plist(&exe, profile, &platform_name, &envs, &log)
        }
        UnitFormat::Systemd => render_unit(&exe, profile, &platform_name, &envs),
    };
    print!("{content}");
    Ok(())
}

/// 安装时应快照进服务定义的环境变量（凭据等——不快照则守护进程取不到）。
/// T15：IMAGENT_LOG_MAX_MB（daemon.log 轮转阈值）一并快照——守护进程形态下
/// 该 env 决定轮转阈值，不快照则 install 时的设置对守护进程不生效。
/// P3-e（code-review v14）：补 `IMAGENT_PASSPHRASE`（ilink 加密凭据解密口令，
/// 缺失则守护进程读凭据直接失败）、`IMAGENT_HTTP_TOKEN`（metrics Bearer 鉴权，
/// 非 loopback 部署缺失会被 S7 fail-closed 拒启）、`IMAGENT_ACP_COMMAND`
/// （agent=acp/claude-acp 的 ACP 启动命令覆盖）——此前快照漏键，装出来的
/// 守护进程与交互 shell 行为不一致。
fn capture_envs() -> Vec<(String, String)> {
    const KEYS: &[&str] = &[
        "IMAGENT_FEISHU_APP_SECRET",
        "IMAGENT_HOME",
        "RUST_LOG",
        "IMAGENT_LOG_MAX_MB",
        "IMAGENT_PASSPHRASE",
        "IMAGENT_HTTP_TOKEN",
        "IMAGENT_ACP_COMMAND",
    ];
    KEYS.iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

/// 返回 (输出文本, 是否成功退出)。v1.18 review（agent-2 #5）：此前非零退出
/// 也返回 Ok(text)、调用方无从判断——install 对 launchctl load 失败照样打印
/// 「✅ 已安装并启动」（服务实际没起来）。强制步骤用 [`run_mandatory`]。
fn run(cmd: &str, args: &[&str]) -> Result<(String, bool)> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| anyhow!("执行 {cmd} 失败：{e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok((text.trim().to_string(), out.status.success()))
}

/// 强制步骤：非零退出即 Err（区别于 unload 类 best-effort 步骤）。
fn run_mandatory(cmd: &str, args: &[&str]) -> Result<String> {
    let (text, ok) = run(cmd, args)?;
    if !ok {
        return Err(anyhow!("{cmd} {} 失败：{}", args.join(" "), text));
    }
    Ok(text)
}

/// 服务定义文件落盘后 chmod 0600（v1.18 review agent-2 #6：定义内嵌
/// IMAGENT_FEISHU_APP_SECRET 明文，此前按 umask 0644 世界可读——与
/// config.toml 0600 的既定 posture 不一致）。
fn write_unit_secret_safe(path: &std::path::Path, content: String) -> Result<()> {
    std::fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// `service install`：写定义文件 + 加载启动。
/// P3-e（code-review v14）：改为 async——安装期检查 ilink 加密凭据需要读
/// store（SQLite IO），同步签名装不下。
pub async fn install(profile: Option<&str>) -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| anyhow!("定位当前二进制失败：{e}"))?
        .to_string_lossy()
        .into_owned();
    let path = unit_path(profile)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let envs = capture_envs();
    // 平台来自 config（feishu/wecom 显式写进服务定义，自文档化且不依赖 start 的
    // 缺省解析）；config 读不到时提示先 setup（不猜默认值静默装错平台）。
    let cfg_path =
        imagent_core::Config::default_path().ok_or_else(|| anyhow!("无法定位 config 路径"))?;
    let config = imagent_core::Config::load(&cfg_path).map_err(|e| {
        anyhow!(
            "读取 {} 失败：{e}\n先跑 `imagent setup` 完成配置再装服务",
            cfg_path.display()
        )
    })?;
    let platform_name = config.platform.clone();
    println!(
        "平台：{platform_name}（凭据环境变量快照 {} 项）",
        envs.len()
    );
    if platform_name == "feishu" && !envs.iter().any(|(k, _)| k == "IMAGENT_FEISHU_APP_SECRET") {
        return Err(anyhow!(
            "platform=feishu 但当前 shell 未设置 IMAGENT_FEISHU_APP_SECRET——\n\
             守护进程取不到交互 shell 的环境变量，安装时会快照进服务定义。\n\
             请先 `export IMAGENT_FEISHU_APP_SECRET=…` 再执行本命令。"
        ));
    }
    // P3-e（code-review v14）：platform=ilink 且 store 里存在 enc 形态凭据但
    // IMAGENT_PASSPHRASE 不在快照 → 拦截安装（比照上方 feishu secret 缺省
    // 拦截）：守护进程没有 tty 可输口令，缺 passphrase 时读凭据直接失败，
    // bot 会以「凭据解密失败」反复崩溃重启（KeepAlive 循环）。明文/keyring
    // 形态不受影响。
    if platform_name == "ilink" && !envs.iter().any(|(k, _)| k == "IMAGENT_PASSPHRASE") {
        let db = imagent_core::paths::imagent_home().join("imagent.db");
        if db.is_file() {
            let store = imagent_store::Store::open(&db).await?;
            let forms = store.credential_forms().await?;
            if forms.encrypted > 0 {
                return Err(anyhow!(
                    "platform=ilink 且检测到 {} 条加密形态（enc:v1/v2）凭据，\
                     但当前 shell 未设置 IMAGENT_PASSPHRASE——\n\
                     守护进程无 tty 可输口令，缺 passphrase 将解密失败并反复崩溃重启。\n\
                     请先 `export IMAGENT_PASSPHRASE=…` 再执行本命令\
                     （安装时会快照进服务定义）。",
                    forms.encrypted
                ));
            }
        }
    }
    // 日志路径仅 launchd 用（systemd 走 journal）——随平台门控，防 Linux 下未用告警。
    #[cfg(target_os = "macos")]
    let log = {
        if let Some(dir) = daemon_log_path().parent() {
            std::fs::create_dir_all(dir)?;
        }
        daemon_log_path().to_string_lossy().into_owned()
    };

    #[cfg(target_os = "macos")]
    {
        let plist = render_plist(&exe, profile, &platform_name, &envs, &log);
        write_unit_secret_safe(&path, plist)?;
        let lbl = label(profile);
        // 先卸旧（不存在时报错可忽略）再加载（load 为强制步骤——失败如实报错）。
        let _ = run("launchctl", &["unload", &path.to_string_lossy()]);
        run_mandatory("launchctl", &["load", &path.to_string_lossy()])?;
        println!("✅ 已安装并启动 launchd 用户代理 {lbl}");
        println!("   定义：{}", path.display());
        println!("   日志：{log}");
        println!(
            "   停止：imagent service uninstall（或 launchctl unload {}）",
            path.display()
        );
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let unit = render_unit(&exe, profile, &platform_name, &envs);
        write_unit_secret_safe(&path, unit)?;
        let name = label(profile).replace("com.imagent", "imagent");
        run("systemctl", &["--user", "daemon-reload"])?;
        run_mandatory("systemctl", &["--user", "enable", "--now", &name])?;
        println!("✅ 已安装并启动 systemd 用户服务 {name}");
        println!("   定义：{}", path.display());
        println!("   日志：journalctl --user -u {name} -f");
    }
    #[cfg(not(unix))]
    {
        let _ = (exe, envs);
        return Err(anyhow!("仅支持 macOS / Linux"));
    }
    Ok(())
}

/// `service uninstall`：停止 + 删定义文件。
pub fn uninstall(profile: Option<&str>) -> Result<()> {
    let path = unit_path(profile)?;
    if !path.exists() {
        return Err(anyhow!("服务未安装（{} 不存在）", path.display()));
    }
    #[cfg(target_os = "macos")]
    {
        let _ = run("launchctl", &["unload", &path.to_string_lossy()]);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let name = label(profile).replace("com.imagent", "imagent");
        let _ = run("systemctl", &["--user", "disable", "--now", &name]);
        run("systemctl", &["--user", "daemon-reload"])?;
    }
    std::fs::remove_file(&path)?;
    println!("✅ 已卸载（{}）", path.display());
    Ok(())
}

/// `service status`：查询运行状态。
pub fn status(profile: Option<&str>) -> Result<()> {
    let path = unit_path(profile)?;
    if !path.exists() {
        println!(
            "未安装（{} 不存在）。安装：imagent service install",
            path.display()
        );
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let lbl = label(profile);
        match run("launchctl", &["list"]) {
            Ok((list, _ok)) => {
                let hit = list
                    .lines()
                    .find(|l| l.split('\t').nth(2) == Some(lbl.as_str()));
                match hit {
                    Some(l) => {
                        let mut it = l.split('\t');
                        let pid = it.next().unwrap_or("-");
                        let code = it.next().unwrap_or("-");
                        println!("{lbl}：已加载（PID {pid}，上次退出码 {code}）");
                    }
                    None => println!(
                        "{lbl}：定义存在但未加载（launchctl load {}）",
                        path.display()
                    ),
                }
            }
            Err(e) => return Err(e),
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let name = label(profile).replace("com.imagent", "imagent");
        let (out, _ok) = run("systemctl", &["--user", "is-active", &name])?;
        println!("{name}：{out}");
    }
    #[cfg(not(unix))]
    {
        println!("仅支持 macOS / Linux");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// T15：daemon.log 尺寸轮转（copytruncate）—— CODE_REVIEW_v13 P3
//「macOS daemon.log 无限增长，全仓无任何轮转机制」。
// ---------------------------------------------------------------------------

/// daemon.log 路径：`<imagent_home>/logs/daemon.log`（install 写进 plist
/// `StandardOutPath/StandardErrorPath` 的同一形态，轮转目标）。
pub fn daemon_log_path() -> PathBuf {
    imagent_core::paths::imagent_home()
        .join("logs")
        .join("daemon.log")
}

/// 轮转阈值 env 名（单位 MB）：缺省回落 [`DEFAULT_LOG_MAX_MB`]，`0` = 不限。
/// 随 install 快照进服务定义（见 [`capture_envs`]）——守护进程形态改值需
/// `export` 新值后重跑 `imagent service install`（或手改 plist 后重新 load）。
pub const LOG_MAX_MB_ENV: &str = "IMAGENT_LOG_MAX_MB";

/// 默认阈值 50MB。
pub const DEFAULT_LOG_MAX_MB: u64 = 50;

/// 保留最近 5 份轮转归档（`daemon.log.<epoch>`），超出删最旧。
pub const LOG_ROTATE_KEEP: usize = 5;

/// 解析阈值（MB；纯函数便于单测）：缺省/空白 → 默认值；`0` → `Ok(None)`
///（不限，关闭轮转）；合法正整数 → `Ok(Some(mb))`；非法 → `Err`（调用方
/// warn 后回落默认——坏 env 不应阻断守护进程启动）。
fn parse_log_max_mb(raw: Option<&str>) -> Result<Option<u64>, String> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Some(DEFAULT_LOG_MAX_MB));
    };
    let mb = raw
        .parse::<u64>()
        .map_err(|_| format!("{LOG_MAX_MB_ENV}={raw:?} 不是合法的非负整数"))?;
    Ok((mb > 0).then_some(mb))
}

/// 当前生效阈值（字节）：`None` = 不限。每次检查时现读 env（读 env 廉价，
/// 且 launchctl setenv / 手改 plist 重启后的下一节拍即可吃到新值）。
fn log_max_bytes() -> Option<u64> {
    match parse_log_max_mb(std::env::var(LOG_MAX_MB_ENV).ok().as_deref()) {
        Ok(mb) => mb.map(|m| m.saturating_mul(1024 * 1024)),
        Err(e) => {
            tracing::warn!(
                target: "imagent::ops",
                "{e}，daemon.log 轮转阈值回落默认 {DEFAULT_LOG_MAX_MB}MB"
            );
            Some(DEFAULT_LOG_MAX_MB * 1024 * 1024)
        }
    }
}

/// copytruncate 本体：把 `log` 当前内容复制为归档 `dest`，再把 `log` **原地**
/// truncate(0)。
///
/// # 为什么必须是 copytruncate 而不是 rename（plist 形态的机制约束，勿改）
///
/// launchd 对 plist `StandardOutPath/StandardErrorPath` 的实现是在 spawn 守护
/// 进程时打开该路径一次、把 **fd** 接到子进程的 stdout/stderr（O_APPEND 追加
/// 写）——它持有的是 fd（指向 inode），不是路径：
/// - 若用 rename 轮转（`daemon.log` → `daemon.log.<epoch>`），launchd 的 fd
///   仍指向旧 inode（此后挂在改名后的归档路径上），新日志继续写进归档文件，
///   而之后重建的 `daemon.log` 永远收不到输出——轮转静默失效，这是 plist
///   捕获 stdout 形态的经典坑。
/// - copytruncate 不动 inode：复制出归档后把原文件 `set_len(0)`，launchd 的
///   fd 保持有效，O_APPEND 保证下一次 write 原子定位到（已归零的）文件末尾。
///
/// # 丢日志窗口
///
/// `fs::copy` 完成到 `set_len(0)` 之间写入的日志行会被截掉——最坏丢秒级
/// 日志。这与业界 logrotate 的 copytruncate 语义一致，是无外部工具协作下
/// plist 形态的最优解，可接受。
fn copytruncate(log: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    std::fs::copy(log, dest)?;
    // write(true) + set_len(0) = 原地截断（inode 不变）；sync_all 让截断即时
    // 落盘，防崩溃后文件长度回弹。
    let f = std::fs::OpenOptions::new().write(true).open(log)?;
    f.set_len(0)?;
    f.sync_all()
}

/// 归档命名：`<base>.<epoch>`（epoch 秒）；同秒内再次轮转（正常 10 分钟节拍
/// 下不可达，测试拨钟/手动触发会撞）追加 `.<n>` 序号避免覆盖既有归档。
fn next_rotated_dest(dir: &std::path::Path, log: &std::path::Path, epoch: u64) -> PathBuf {
    let base = file_base(log);
    let primary = dir.join(format!("{base}.{epoch}"));
    if !primary.exists() {
        return primary;
    }
    for i in 1u64..1000 {
        let p = dir.join(format!("{base}.{epoch}.{i}"));
        if !p.exists() {
            return p;
        }
    }
    primary
}

/// 清理归档：按文件名中的 epoch（旧→新）排序，保留最近 `keep` 份，超出删
/// 最旧。删除 best-effort（失败跳过，下一节拍再试）；返回成功删除数。
fn prune_rotated(dir: &std::path::Path, log: &std::path::Path, keep: usize) -> usize {
    let prefix = format!("{}.", file_base(log));
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut entries: Vec<(u64, u64, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().into_owned();
            let rest = name.strip_prefix(&prefix)?;
            // 主 epoch + 可选同秒碰撞序号 ".n"：`daemon.log.1700000000[.2]`。
            let (epoch, sub) = match rest.split_once('.') {
                Some((a, b)) => (a.parse::<u64>().ok()?, b.parse::<u64>().ok()?),
                None => (rest.parse::<u64>().ok()?, 0),
            };
            Some((epoch, sub, p))
        })
        .collect();
    // (epoch, sub) 升序 = 时间旧→新（PathBuf 仅作平局比较项，不影响语义）。
    entries.sort_unstable();
    let excess = entries.len().saturating_sub(keep);
    entries
        .into_iter()
        .take(excess)
        .filter(|(_, _, p)| std::fs::remove_file(p).is_ok())
        .count()
}

fn file_base(log: &std::path::Path) -> String {
    log.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "daemon.log".to_string())
}

/// 单次轮转判定（参数化，便于单测用 tmp 目录）：文件不存在 → `Ok(false)`
///（Linux journal / 未装服务形态天然 no-op）；大小未超阈 → `Ok(false)`；
/// 超阈（严格大于）→ copytruncate + 清理超保留数的归档 → `Ok(true)`。
fn rotate_if_over(
    log: &std::path::Path,
    max_bytes: u64,
    keep: usize,
    now_epoch: u64,
) -> std::io::Result<bool> {
    let meta = match std::fs::metadata(log) {
        Ok(m) => m,
        // 不存在是常态（Linux 走 journal / 前台未装服务），静默跳过。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if !meta.is_file() || meta.len() <= max_bytes {
        return Ok(false);
    }
    let dir = log.parent().unwrap_or_else(|| std::path::Path::new("."));
    copytruncate(log, &next_rotated_dest(dir, log, now_epoch))?;
    prune_rotated(dir, log, keep);
    Ok(true)
}

/// 轮转检查入口（启动 + 每 10 分钟节拍调用；best-effort——失败仅 warn，
/// 绝不影响主流程）。
///
/// **形态判定**：`imagent start` 是前台与 launchd 守护的同一入口（plist
/// `ProgramArguments` = `<exe> start --platform …`，无 `--daemon` 标志），
/// 无法也不必区分，统一按「文件存在且超阈」判定：
/// - macOS launchd（`service install`）：plist 把 stdout/stderr 指到本文件，
///   真正的轮转目标；
/// - macOS 前台：仅装过服务时该文件才存在，顺手轮转无害且同样值得；
/// - Linux：`service install` 的 systemd unit 不写 StandardOutput（走
///   journal），本文件天然不存在 → no-op；journal 的轮转归 journald 管。
///
/// 并发安全：同 IMAGENT_HOME 双实例被 instance 锁互斥，不存在两个进程
/// 同时轮转；与 launchd 的写入方-轮转方关系见 [`copytruncate`] 注释。
pub fn rotate_daemon_log_if_needed() {
    let Some(max_bytes) = log_max_bytes() else {
        return; // 0 = 不限
    };
    let log = daemon_log_path();
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match rotate_if_over(&log, max_bytes, LOG_ROTATE_KEEP, now_epoch) {
        Ok(true) => tracing::info!(
            target: "imagent::ops",
            log = %log.display(),
            keep = LOG_ROTATE_KEEP,
            "daemon.log 超阈已轮转（copytruncate，最坏丢复制与截断之间的秒级日志）"
        ),
        Ok(false) => {}
        Err(e) => tracing::warn!(
            target: "imagent::ops",
            log = %log.display(),
            error = %e,
            "daemon.log 轮转失败（继续运行，下一节拍重试）"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_paths() {
        assert_eq!(label(None), "com.imagent");
        assert_eq!(label(Some("codex")), "com.imagent.codex");
        assert_eq!(label(Some("")), "com.imagent");
    }

    #[test]
    fn plist_shape() {
        let p = render_plist(
            "/usr/local/bin/imagent",
            Some("codex"),
            "feishu",
            &[("IMAGENT_FEISHU_APP_SECRET".into(), "s3cr3t".into())],
            "/tmp/daemon.log",
        );
        assert!(p.contains("<string>com.imagent.codex</string>"), "{p}");
        assert!(p.contains("<string>/usr/local/bin/imagent</string>"));
        assert!(p.contains("<string>--profile</string>"));
        assert!(p.contains("<string>codex</string>"));
        assert!(
            p.contains("<string>--platform</string>") && p.contains("<string>feishu</string>"),
            "平台应显式入参: {p}"
        );
        assert!(p.contains("IMAGENT_FEISHU_APP_SECRET"));
        assert!(p.contains("<string>s3cr3t</string>"));
        assert!(p.contains("KeepAlive"));
        // 无 profile 时不带 --profile 参数（平台仍显式）。
        let p2 = render_plist("/x/imagent", None, "ilink", &[], "/tmp/l");
        assert!(!p2.contains("--profile"));
        assert!(p2.contains("<string>com.imagent</string>"));
    }

    #[test]
    fn unit_shape() {
        let u = render_unit(
            "/usr/local/bin/imagent",
            Some("codex"),
            "feishu",
            &[("IMAGENT_FEISHU_APP_SECRET".into(), "s3cr3t".into())],
        );
        assert!(
            u.contains("ExecStart=/usr/local/bin/imagent start --platform feishu --profile codex"),
            "{u}"
        );
        assert!(u.contains("Environment=\"IMAGENT_FEISHU_APP_SECRET=s3cr3t\""));
        assert!(u.contains("Restart=on-failure"));
        let u2 = render_unit("/x/imagent", None, "ilink", &[]);
        assert!(u2.contains("ExecStart=/x/imagent start --platform ilink"));
        assert!(!u2.contains("--profile"));
    }

    /// B2 deploy 合流：deploy/ 静态模板 = `service print` 在固定参数下的输出
    /// （刷新命令见 deploy/README.md）。本测试固化模板关键字段——尤其
    /// `--platform` 参数位（历史债：静态模板缺失该位，复制装出的守护进程
    /// 误走缺省平台解析）。若本测试红了，先改生成器再重刷模板，勿手改模板。
    #[test]
    fn template_shape_matches_deploy_refresh() {
        // 与 deploy/README.md 刷新命令同参（launchd：--log-path 取旧模板的
        // /usr/local/var/log，README 注明该路径不在内置轮转范围）。
        let p = render_plist(
            "/usr/local/bin/imagent",
            None,
            "feishu",
            &[],
            "/usr/local/var/log/imagent.log",
        );
        assert!(p.contains("<string>/usr/local/bin/imagent</string>"), "{p}");
        assert!(p.contains("<string>start</string>"), "{p}");
        // --platform 参数位：模板必须显式带平台（缺这位的旧债正是 B2 要消除的分叉点）。
        assert!(
            p.contains("<string>--platform</string>") && p.contains("<string>feishu</string>"),
            "plist 模板必须含 --platform 入参位: {p}"
        );
        assert!(p.contains("<string>com.imagent</string>"), "{p}");
        assert!(
            p.contains("<string>/usr/local/var/log/imagent.log</string>"),
            "{p}"
        );
        assert!(p.contains("KeepAlive"), "{p}");

        let u = render_unit("/usr/local/bin/imagent", None, "feishu", &[]);
        assert!(
            u.contains("ExecStart=/usr/local/bin/imagent start --platform feishu"),
            "{u}"
        );
        assert!(u.contains("Restart=on-failure"), "{u}");
        assert!(u.contains("WantedBy=default.target"), "{u}");
        // 模板（--with-env 关闭）不应含凭据快照位。
        assert!(!p.contains("IMAGENT_FEISHU_APP_SECRET"), "{p}");
        assert!(!u.contains("Environment="), "{u}");
    }
}

/// T15 daemon.log 轮转单测：全部走 tmp 目录参数化，不碰真实 `~/.imagent`。
#[cfg(test)]
mod logrotate_tests {
    use super::*;

    /// 每测试独立 tmp 目录（tag 唯一，并行测试互不干扰）；结尾 best-effort 清理。
    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "imagent-t15-logrotate-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn log_in(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join("daemon.log")
    }

    /// 轮转判定：未超阈不动原文件；恰好等于阈值不轮转（严格大于才轮转）；
    /// 超阈 → 原文件截 0、归档 `<base>.<epoch>` 含旧内容。
    #[test]
    fn rotate_decision_over_and_under_threshold() {
        let dir = tmp_dir("decision");
        let log = log_in(&dir);
        // 未超阈（3 字节 vs 阈值 10）：不动。
        std::fs::write(&log, "abc").unwrap();
        assert!(!rotate_if_over(&log, 10, 5, 1_700_000_000).unwrap());
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "abc");

        // 恰好等于阈值：不轮转（「超过」= 严格大于）。
        std::fs::write(&log, "0123456789").unwrap();
        assert!(!rotate_if_over(&log, 10, 5, 1_700_000_000).unwrap());

        // 超阈（11 > 10）：轮转。
        std::fs::write(&log, "0123456789A").unwrap();
        assert!(rotate_if_over(&log, 10, 5, 1_700_000_000).unwrap());
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "",
            "轮转后原文件应截 0"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("daemon.log.1700000000")).unwrap(),
            "0123456789A",
            "归档应含轮转前的全部内容"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// copytruncate 全链路：写 → 轮转 → 新写入 → 归档仍是旧内容、原文件只有
    /// 新内容；且截断是**原地**的（unix 下 inode 不变——launchd 持 fd 的前提，
    /// rename 方案正是在此断裂）。
    #[test]
    fn copytruncate_write_rotate_write_old_content_preserved() {
        let dir = tmp_dir("chain");
        let log = log_in(&dir);
        std::fs::write(&log, "old-line-1\nold-line-2\n").unwrap();
        #[cfg(unix)]
        let inode_before = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&log).unwrap().ino()
        };
        assert!(rotate_if_over(&log, 8, 5, 1_700_000_000).unwrap());
        // 轮转后继续写（模拟 launchd 追加）。
        std::fs::write(&log, "new-line-1\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("daemon.log.1700000000")).unwrap(),
            "old-line-1\nold-line-2\n",
            "归档必须保留轮转前的旧内容"
        );
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "new-line-1\n",
            "原文件只应含轮转后的新写入"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                std::fs::metadata(&log).unwrap().ino(),
                inode_before,
                "copytruncate 必须原地截断（inode 不变）——rename 会换 inode，launchd 的 fd 将指向归档"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 机制级验证（模拟 launchd）：外部进程以 O_APPEND 持有 fd 的文件被本机制
    /// 轮转后，经旧 fd 继续写——写入应落到截断后的文件末尾（无 NUL 洞），
    /// 而非写到截断前的旧 offset。
    #[test]
    fn held_append_fd_survives_truncation() {
        use std::io::Write;
        let dir = tmp_dir("append-fd");
        let log = log_in(&dir);
        let mut held = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&log)
            .unwrap();
        writeln!(held, "before-rotate").unwrap();
        assert!(rotate_if_over(&log, 4, 5, 1_700_000_000).unwrap());
        // launchd 的 fd 仍打开且 O_APPEND：下一次 write 定位到（已归零的）末尾。
        writeln!(held, "after-rotate").unwrap();
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "after-rotate\n",
            "经旧 fd 的写入应落在新文件末尾，无 NUL 洞"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("daemon.log.1700000000")).unwrap(),
            "before-rotate\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 保留 N 份：7 份归档（含同秒碰撞序号档）keep=5 → 删最旧 2 份，其余保留；
    /// 排序按 epoch（而非文件名字典序——跨位数字典序会错）。
    #[test]
    fn prune_keeps_newest_n() {
        let dir = tmp_dir("prune");
        let log = log_in(&dir);
        for (ep, sub) in [
            (999_u64, 0_u64),
            (1000, 0),
            (1000, 1), // 同秒碰撞档
            (1001, 0),
            (1002, 0),
            (998, 0),
            (1003, 0),
        ] {
            let name = if sub == 0 {
                format!("daemon.log.{ep}")
            } else {
                format!("daemon.log.{ep}.{sub}")
            };
            std::fs::write(dir.join(name), "x").unwrap();
        }
        let removed = prune_rotated(&dir, &log, 5);
        assert_eq!(removed, 2, "7 份 keep 5 → 删最旧 2（epoch 998 与 999）");
        assert!(!dir.join("daemon.log.998").exists());
        assert!(!dir.join("daemon.log.999").exists());
        for keep_name in [
            "daemon.log.1000",
            "daemon.log.1000.1",
            "daemon.log.1001",
            "daemon.log.1002",
            "daemon.log.1003",
        ] {
            assert!(dir.join(keep_name).exists(), "应保留 {keep_name}");
        }
        // 不足 keep 份时全保留、不误删。
        assert_eq!(prune_rotated(&dir, &log, 5), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 同秒两次轮转（epoch 碰撞）：第二次归档拿 `.<n>` 序号，两份内容都不丢。
    #[test]
    fn same_epoch_collision_gets_suffix() {
        let dir = tmp_dir("collision");
        let log = log_in(&dir);
        std::fs::write(&log, "first-batch").unwrap();
        assert!(rotate_if_over(&log, 4, 5, 1_700_000_000).unwrap());
        std::fs::write(&log, "second-batch").unwrap();
        assert!(rotate_if_over(&log, 4, 5, 1_700_000_000).unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.join("daemon.log.1700000000")).unwrap(),
            "first-batch"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("daemon.log.1700000000.1")).unwrap(),
            "second-batch",
            "同秒第二次轮转应拿序号后缀而非覆盖"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件不存在（Linux journal / 前台未装服务形态）→ no-op 不报错。
    #[test]
    fn missing_log_is_noop() {
        let dir = tmp_dir("missing");
        assert!(!rotate_if_over(&log_in(&dir), 10, 5, 1).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 阈值 env 解析：缺省/空白 → 默认 50；"0" → None（不限）；合法值直通；
    /// 非法（非数字/负数）→ Err（调用方 warn 后回落默认）。
    #[test]
    fn parse_env_variants() {
        assert_eq!(parse_log_max_mb(None), Ok(Some(DEFAULT_LOG_MAX_MB)));
        assert_eq!(parse_log_max_mb(Some("")), Ok(Some(DEFAULT_LOG_MAX_MB)));
        assert_eq!(parse_log_max_mb(Some("  ")), Ok(Some(DEFAULT_LOG_MAX_MB)));
        assert_eq!(parse_log_max_mb(Some("0")), Ok(None), "0 = 不限");
        assert_eq!(parse_log_max_mb(Some(" 0 ")), Ok(None));
        assert_eq!(parse_log_max_mb(Some("200")), Ok(Some(200)));
        assert!(parse_log_max_mb(Some("abc")).is_err());
        assert!(parse_log_max_mb(Some("-1")).is_err());
        assert!(parse_log_max_mb(Some("1.5")).is_err());
    }
}
