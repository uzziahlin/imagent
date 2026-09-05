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
#[cfg(target_os = "macos")]
fn render_plist(
    exe: &str,
    profile: Option<&str>,
    platform: &str,
    envs: &[(String, String)],
    log: &str,
) -> String {
    let mut args = format!(
        "        <string>{exe}</string>\n        <string>start</string>\n        <string>--platform</string>\n        <string>{platform}</string>"
    );
    if let Some(p) = profile.filter(|p| !p.is_empty()) {
        args.push_str(&format!(
            "\n        <string>--profile</string>\n        <string>{p}</string>"
        ));
    }
    let mut env = String::new();
    if !envs.is_empty() {
        env.push_str("    <key>EnvironmentVariables</key>\n    <dict>\n");
        for (k, v) in envs {
            env.push_str(&format!(
                "        <key>{k}</key>\n        <string>{v}</string>\n"
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
         \x20   <key>StandardOutPath</key>\n    <string>{log}</string>\n\
         \x20   <key>StandardErrorPath</key>\n    <string>{log}</string>\n\
         </dict>\n</plist>\n",
        label = label(profile),
    )
}

/// systemd 用户单元模板（ExecStart 同参数；日志 journalctl）。
#[cfg(all(unix, not(target_os = "macos")))]
fn render_unit(
    exe: &str,
    profile: Option<&str>,
    platform: &str,
    envs: &[(String, String)],
) -> String {
    let mut exec = format!("{exe} start --platform {platform}");
    if let Some(p) = profile.filter(|p| !p.is_empty()) {
        exec.push_str(&format!(" --profile {p}"));
    }
    let mut env = String::new();
    for (k, v) in envs {
        env.push_str(&format!("Environment=\"{k}={v}\"\n"));
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

/// 安装时应快照进服务定义的环境变量（凭据等——不快照则守护进程取不到）。
fn capture_envs() -> Vec<(String, String)> {
    const KEYS: &[&str] = &["IMAGENT_FEISHU_APP_SECRET", "IMAGENT_HOME", "RUST_LOG"];
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
pub fn install(profile: Option<&str>) -> Result<()> {
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
    // 日志路径仅 launchd 用（systemd 走 journal）——随平台门控，防 Linux 下未用告警。
    #[cfg(target_os = "macos")]
    let log = {
        let log_dir = imagent_core::paths::imagent_home().join("logs");
        std::fs::create_dir_all(&log_dir)?;
        log_dir.join("daemon.log").to_string_lossy().into_owned()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_paths() {
        assert_eq!(label(None), "com.imagent");
        assert_eq!(label(Some("codex")), "com.imagent.codex");
        assert_eq!(label(Some("")), "com.imagent");
    }

    #[cfg(target_os = "macos")]
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

    #[cfg(all(unix, not(target_os = "macos")))]
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
}
