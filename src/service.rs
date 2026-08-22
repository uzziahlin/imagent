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
fn render_plist(exe: &str, profile: Option<&str>, envs: &[(String, String)], log: &str) -> String {
    let mut args = format!("        <string>{exe}</string>\n        <string>start</string>");
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
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn render_unit(exe: &str, profile: Option<&str>, envs: &[(String, String)]) -> String {
    let mut exec = format!("{exe} start");
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

fn run(cmd: &str, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| anyhow!("执行 {cmd} 失败：{e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        // launchctl/systemctl 非零退出不一定是失败（如 unload 不存在的服务），
        // 调用方据语义判断；这里返回文本 + 状态码由调用方处理。
        return Ok(text.trim().to_string());
    }
    Ok(text.trim().to_string())
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
    let state_root = imagent_core::paths::imagent_home();
    let log_dir = state_root.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let log = log_dir.join("daemon.log").to_string_lossy().into_owned();

    #[cfg(target_os = "macos")]
    {
        let plist = render_plist(&exe, profile, &envs, &log);
        std::fs::write(&path, plist)?;
        let lbl = label(profile);
        // 先卸旧（不存在时报错可忽略）再加载。
        let _ = run("launchctl", &["unload", &path.to_string_lossy()]);
        run("launchctl", &["load", &path.to_string_lossy()])?;
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
        let unit = render_unit(&exe, profile, &envs);
        std::fs::write(&path, unit)?;
        let name = label(profile).replace("com.imagent", "imagent");
        run("systemctl", &["--user", "daemon-reload"])?;
        run("systemctl", &["--user", "enable", "--now", &name])?;
        println!("✅ 已安装并启动 systemd 用户服务 {name}");
        println!("   定义：{}", path.display());
        println!("   日志：journalctl --user -u {name} -f");
    }
    #[cfg(not(unix))]
    {
        let _ = (exe, envs, log);
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
            Ok(list) => {
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
        let out = run("systemctl", &["--user", "is-active", &name])?;
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
            &[("IMAGENT_FEISHU_APP_SECRET".into(), "s3cr3t".into())],
            "/tmp/daemon.log",
        );
        assert!(p.contains("<string>com.imagent.codex</string>"), "{p}");
        assert!(p.contains("<string>/usr/local/bin/imagent</string>"));
        assert!(p.contains("<string>--profile</string>"));
        assert!(p.contains("<string>codex</string>"));
        assert!(p.contains("IMAGENT_FEISHU_APP_SECRET"));
        assert!(p.contains("<string>s3cr3t</string>"));
        assert!(p.contains("KeepAlive"));
        // 无 profile 时不带 --profile 参数。
        let p2 = render_plist("/x/imagent", None, &[], "/tmp/l");
        assert!(!p2.contains("--profile"));
        assert!(p2.contains("<string>com.imagent</string>"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn unit_shape() {
        let u = render_unit(
            "/usr/local/bin/imagent",
            Some("codex"),
            &[("IMAGENT_FEISHU_APP_SECRET".into(), "s3cr3t".into())],
        );
        assert!(
            u.contains("ExecStart=/usr/local/bin/imagent start --profile codex"),
            "{u}"
        );
        assert!(u.contains("Environment=\"IMAGENT_FEISHU_APP_SECRET=s3cr3t\""));
        assert!(u.contains("Restart=on-failure"));
        let u2 = render_unit("/x/imagent", None, &[]);
        assert!(u2.contains("ExecStart=/x/imagent start"));
        assert!(!u2.contains("--profile"));
    }
}
