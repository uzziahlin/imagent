//! OS 级 agent 沙箱（v1.32）：把 SECURITY.md 威胁模型里的「进程面」约束下沉到
//! 内核层。覆盖与 `hide_state_dir_from_agent`（CLI 层 deny）同源但更硬——挡住
//! Bash / 任意子进程对状态目录的间接读写与「写 DB 植后门」路径。
//!
//! 两档（`config agent_sandbox`）：
//! - **Guarded（`auto` 缺省）**：`allow default` 基座 + 定点 deny——
//!   ① 状态目录（生效 home + 基座 `~/.imagent`）读+写全 deny，例外放行
//!   `permission.token`（读）/ `permission.sock`（读+写，审批闭环通道）/ 每轮
//!   `mcp_*.json`（读，claude 启动即读）；② 持久化热点写 deny（`~/.ssh`、
//!   shell rc 文件、LaunchAgents/LaunchDaemons、Linux autostart/user-units）——
//!   读保留（git push 需要 `~/.ssh` 读）。正常开发读写（包管理器缓存等）不受
//!   影响，零破坏面。
//! - **Strict**：Guarded 基础上把**写**收敛到 workdir / 临时目录 / `~/.claude`
//!   （claude 自身状态）/ `agent_sandbox_extra_write` 显式清单。包管理器缓存
//!   （`~/.npm` `~/.cargo` 等）不在缺省清单——需要者用 extra_write 显式加。
//! - **Off**：不包装（等价 v1.31 行为）。
//!
//! 平台机制：macOS = `/usr/bin/sandbox-exec`（SBPL profile）；Linux = `bwrap`
//!（`--die-with-parent`；宿主未安装降级 Off + warn）。杀进程语义兼容：包装进程
//! 进独立进程组（spawn_cli_backend 的 `process_group(0)`），`killpg` 整组收割
//! 连 claude 孙进程一并终止——与 B5 守卫既有语义一致，无新增逃逸面。
//!
//! 仅 claude-cli 后端接线（codex/gemini 有各自原生沙箱，叠加双层沙箱冲突；
//! ACP 命令行外部指定不可控——与 `hide_state_dir_from_agent` 同边界，见
//! SECURITY.md 已知限制）。
//!
//! 规则优先级与路径形态**真机校准**（2026-10-22，darwin 25.6.0 / sandbox-exec）：
//! - SBPL **后规则覆盖前规则**（先 deny subpath 再 allow literal = 例外放行；
//!   反序则 deny 胜）——本模块生成器按「基座 allow → deny → 例外 allow」排序；
//! - 路径按 **canonical 形态**匹配（`/var` → `/private/var`，`/tmp` →
//!   `/private/tmp`）——每路径注入字面 + canonicalize 双形态（对齐
//!   `state_dir_deny_settings` 的既有处理）。

use std::path::{Path, PathBuf};

/// 沙箱档位（`agent_sandbox` 配置值解析产物）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxTier {
    /// `auto`（缺省）：Guarded 档——状态目录 + 持久化热点定点 deny。
    #[default]
    Auto,
    /// `strict`：写入收敛到 workdir / 临时目录 / ~/.claude / extra_write。
    Strict,
    /// `off`：不包装。
    Off,
}

impl SandboxTier {
    /// 配置字符串解析（load 期校验用；未知值 None）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "strict" => Some(Self::Strict),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Strict => "strict",
            Self::Off => "off",
        }
    }
}

/// 平台解析后的执行计划（main 启动 / SIGHUP 时经 [`resolve`] 产出，注入
/// ClaudeBackend 的 `set_sandbox`；delegate 的 claude 副后端同注一份）。
#[derive(Debug, Clone, PartialEq)]
pub enum SandboxPlan {
    Off,
    /// macOS Seatbelt（/usr/bin/sandbox-exec 恒在，无探测失败分支）。
    Seatbelt {
        tier: SandboxTier,
    },
    /// Linux bubblewrap（`bwrap_bin` = 探测到的绝对路径）。
    Bwrap {
        bwrap_bin: PathBuf,
        tier: SandboxTier,
    },
}

impl SandboxPlan {
    /// 生效档位（Off 计划 → Off tier）。
    pub fn tier(&self) -> SandboxTier {
        match self {
            Self::Off => SandboxTier::Off,
            Self::Seatbelt { tier } | Self::Bwrap { tier, .. } => *tier,
        }
    }

    /// `/doctor` 展示的一行状态（None = 未启用）。
    pub fn status_line(&self) -> Option<String> {
        match self {
            Self::Off => None,
            Self::Seatbelt { tier } => Some(format!(
                "seatbelt({})：内核层 deny 状态目录 + 持久化热点{}",
                tier.as_str(),
                if *tier == SandboxTier::Strict {
                    "，写入收敛 workdir/临时目录/~/.claude"
                } else {
                    ""
                }
            )),
            Self::Bwrap { tier, .. } => Some(format!(
                "bwrap({})：内核层 deny 状态目录 + 持久化热点{}",
                tier.as_str(),
                if *tier == SandboxTier::Strict {
                    "，写入收敛 workdir/临时目录/~/.claude（Linux 实验性）"
                } else {
                    ""
                }
            )),
        }
    }
}

/// 按平台解析执行计划。`bwrap_bin`：Linux 上探测到的 bwrap 绝对路径（None =
/// 未安装 → 降级 Off，warn 文案由调用方打）；macOS 忽略该参数（恒 Seatbelt）。
pub fn resolve(tier: SandboxTier, bwrap_bin: Option<PathBuf>) -> SandboxPlan {
    if tier == SandboxTier::Off {
        return SandboxPlan::Off;
    }
    if cfg!(target_os = "macos") {
        SandboxPlan::Seatbelt { tier }
    } else if cfg!(target_os = "linux") {
        match bwrap_bin {
            Some(bin) => SandboxPlan::Bwrap {
                bwrap_bin: bin,
                tier,
            },
            None => SandboxPlan::Off,
        }
    } else {
        SandboxPlan::Off
    }
}

/// PATH 里找 bwrap 绝对路径（Linux 启动 / SIGHUP 探测；None = 未安装）。
pub fn detect_bwrap() -> Option<PathBuf> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let cand = Path::new(dir).join("bwrap");
        // 可执行文件判定（access X_OK 语义用 metadata 近似 + unix 扩展位）。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(md) = cand.metadata() {
                if md.is_file() && md.permissions().mode() & 0o111 != 0 {
                    return Some(cand);
                }
            }
        }
        #[cfg(not(unix))]
        {
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// 单次 run 的沙箱输入（claude backend 每 run 构造）。
#[derive(Debug, Clone, Default)]
pub struct SandboxSpec {
    /// agent 工作目录（Strict 档 RW 清单首位）。
    pub workdir: PathBuf,
    /// 本轮 mcp 配置文件路径（`mcp_<conv>.json`——claude 启动即读，须例外）。
    pub mcp_json: Option<PathBuf>,
    /// Strict 档额外可写目录（config `agent_sandbox_extra_write`）。
    pub extra_write: Vec<PathBuf>,
}

/// 状态目录根集合（生效 home + 基座 `~/.imagent`，去重——对齐
/// `state_dir_deny_settings` 的既有语义）。
fn state_dir_roots() -> Vec<PathBuf> {
    let mut roots = vec![crate::paths::imagent_home()];
    if let Some(h) = dirs::home_dir() {
        let base = h.join(".imagent");
        if !roots.contains(&base) {
            roots.push(base);
        }
    }
    roots
}

/// 路径的字面 + canonicalize 双形态（真机校准：SBPL 按 canonical 匹配，
/// `/var` 系 symlink 路径的字面规则会落空；canonicalize 失败只留字面形态）。
fn path_forms(p: &Path) -> Vec<PathBuf> {
    let mut forms = vec![p.to_path_buf()];
    if let Ok(c) = p.canonicalize() {
        if !forms.contains(&c) {
            forms.push(c);
        }
    }
    forms
}

/// SBPL 字符串字面量转义（路径里的 `\` `"`；正常路径不含，防御性处理）。
fn sb_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 持久化热点（写 deny、读保留）：提示注入后最常见的「落盘驻留」落点。
/// 平台差异：macOS LaunchAgents / Linux autostart + user units；`*_try` 语义由
/// 生成器实现（不存在的路径按 SBPL 直接 deny 该 literal 也无害）。
fn persistence_hotspots() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(h) = dirs::home_dir() {
        out.push(h.join(".ssh"));
        for rc in [
            ".zshrc",
            ".zprofile",
            ".zshenv",
            ".bashrc",
            ".bash_profile",
            ".profile",
        ] {
            out.push(h.join(rc));
        }
        if cfg!(target_os = "macos") {
            out.push(h.join("Library/LaunchAgents"));
        } else {
            out.push(h.join(".config/autostart"));
            out.push(h.join(".config/systemd/user"));
        }
    }
    if cfg!(target_os = "macos") {
        out.push(PathBuf::from("/Library/LaunchAgents"));
        out.push(PathBuf::from("/Library/LaunchDaemons"));
    }
    out
}

/// 生成 macOS Seatbelt（SBPL）profile。规则序（后规则覆盖前规则，真机校准）：
/// ① `(allow default)` 基座（零破坏面——未知 syscall 不误伤 CLI/工具链）；
/// ② 状态目录 read+write deny（subpath × 双路径形态）；
/// ③ 持久化热点 write deny（literal/subpath）；
/// ④ Strict：`(deny file-write* (subpath "/"))` 全局写 deny；
/// ⑤ 例外放行（最后置，压过前面全部 deny）：token/mcp json 读、sock 读写、
///    Strict 的 RW 清单（workdir/临时目录/~/.claude/extra/dev 节点）。
pub fn seatbelt_profile(tier: SandboxTier, spec: &SandboxSpec) -> String {
    if tier == SandboxTier::Off {
        return String::new();
    }
    let mut rules: Vec<String> = vec!["(allow default)".to_string()];

    // ② 状态目录：读 + 写全 deny（subpath × 双形态）。
    for root in state_dir_roots() {
        for form in path_forms(&root) {
            let p = sb_quote(&form.to_string_lossy());
            rules.push(format!("(deny file-read* (subpath {p}))"));
            rules.push(format!("(deny file-write* (subpath {p}))"));
        }
    }
    // ③ 持久化热点：写 deny（读保留——git push 走 ~/.ssh 读）。
    for spot in persistence_hotspots() {
        for form in path_forms(&spot) {
            let p = sb_quote(&form.to_string_lossy());
            if form.is_dir() {
                rules.push(format!("(deny file-write* (subpath {p}))"));
            } else {
                rules.push(format!("(deny file-write* (literal {p}))"));
            }
        }
    }
    // ④ Strict：全局写 deny（随后⑤的 RW 例外压回）。
    if tier == SandboxTier::Strict {
        rules.push("(deny file-write* (subpath \"/\"))".to_string());
    }

    // ⑤ 例外放行（最后置）。
    let mut allows: Vec<String> = Vec::new();
    // 状态目录例外：sock（读+写——审批闭环通道）与每轮 mcp json / token（读）。
    let home = crate::paths::imagent_home();
    for form in path_forms(&home.join("permission.sock")) {
        let p = sb_quote(&form.to_string_lossy());
        allows.push(format!("(allow file-read* file-write* (literal {p}))"));
    }
    if let Some(mcp) = &spec.mcp_json {
        for form in path_forms(mcp) {
            let p = sb_quote(&form.to_string_lossy());
            allows.push(format!("(allow file-read* (literal {p}))"));
        }
    }
    for form in path_forms(&home.join("permission.token")) {
        let p = sb_quote(&form.to_string_lossy());
        allows.push(format!("(allow file-read* (literal {p}))"));
    }
    // Strict 的 RW 清单：workdir / 临时目录 / ~/.claude / extra / dev 节点。
    if tier == SandboxTier::Strict {
        let mut rw: Vec<PathBuf> = vec![spec.workdir.clone()];
        if let Ok(tmp) = std::env::temp_dir().canonicalize() {
            rw.push(tmp);
        }
        rw.push(PathBuf::from("/tmp"));
        rw.push(PathBuf::from("/private/tmp"));
        rw.push(PathBuf::from("/private/var/folders"));
        if let Some(h) = dirs::home_dir() {
            rw.push(h.join(".claude"));
        }
        rw.extend(spec.extra_write.iter().cloned());
        for dir in rw {
            for form in path_forms(&dir) {
                let p = sb_quote(&form.to_string_lossy());
                allows.push(format!("(allow file-write* (subpath {p}))"));
            }
        }
        // node/CLI 常需的 dev 写节点（不在 subpath 例外形态内，literal 放行）。
        for dev in ["/dev/null", "/dev/tty"] {
            allows.push(format!("(allow file-write* (literal \"{dev}\"))"));
        }
    }
    rules.extend(allows);

    let body = rules.join("\n  ");
    format!("(version 1)\n  {body}\n")
}

/// 生成 Linux bwrap 参数前缀（`program 后接 -- cmd...`）。挂载序即遮蔽序：
/// 基座（`--bind / /` 或 strict 的 `--ro-bind / /`）→ 状态目录 tmpfs 遮蔽 +
/// 三文件重挂 → 热点 ro 遮蔽 →（strict）RW 目录重挂。恒带 `--die-with-parent`
///（父死即收，与 killpg 双保险）。
pub fn bwrap_args(tier: SandboxTier, spec: &SandboxSpec) -> Vec<String> {
    if tier == SandboxTier::Off {
        return Vec::new();
    }
    let mut args: Vec<String> = vec![
        "--die-with-parent".into(),
        "--dev-bind".into(),
        "/dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
    ];
    // 基座：Guarded 全 rw；strict 只读 + RW 目录重挂。
    if tier == SandboxTier::Strict {
        args.extend(["--ro-bind".into(), "/".into(), "/".into()]);
    } else {
        args.extend(["--bind".into(), "/".into(), "/".into()]);
    }
    // 状态目录：tmpfs 整体遮蔽（读+写都不可见），三文件重挂（sock rw 连接 /
    // token、mcp json ro 读）。socket 文件 bind-mount 可连接（mount(2) 对任意
    // 文件类型生效）。
    for root in state_dir_roots() {
        let r = root.to_string_lossy().into_owned();
        args.extend(["--tmpfs".into(), r.clone()]);
        let sock = root.join("permission.sock");
        let s = sock.to_string_lossy().into_owned();
        args.extend(["--bind".into(), s.clone(), s]);
        if let Some(mcp) = &spec.mcp_json {
            let m = mcp.to_string_lossy().into_owned();
            args.extend(["--ro-bind".into(), m.clone(), m]);
        }
        let token = root.join("permission.token");
        let t = token.to_string_lossy().into_owned();
        args.extend(["--ro-bind".into(), t.clone(), t]);
    }
    // 持久化热点：ro 遮蔽（写挡、读留）。
    for spot in persistence_hotspots() {
        let s = spot.to_string_lossy().into_owned();
        args.extend(["--ro-bind-try".into(), s.clone(), s]);
    }
    // Strict：RW 目录重挂（workdir / tmpfs 化 /tmp / ~/.claude / extra）。
    if tier == SandboxTier::Strict {
        args.extend(["--tmpfs".into(), "/tmp".into()]);
        let w = spec.workdir.to_string_lossy().into_owned();
        args.extend(["--bind".into(), w.clone(), w]);
        if let Some(h) = dirs::home_dir() {
            let c = h.join(".claude").to_string_lossy().into_owned();
            args.extend(["--bind".into(), c.clone(), c]);
        }
        for dir in &spec.extra_write {
            let e = dir.to_string_lossy().into_owned();
            args.extend(["--bind".into(), e.clone(), e]);
        }
    }
    args.push("--".into());
    args
}

/// 包装命令：返回 `(wrapper_program, prefix_args)`（后接原命令与参数）。
/// None = 计划 Off 或平台无机制（裸跑）。
pub fn wrap_command(plan: &SandboxPlan, spec: &SandboxSpec) -> Option<(String, Vec<String>)> {
    match plan {
        SandboxPlan::Off => None,
        SandboxPlan::Seatbelt { tier } => {
            let profile = seatbelt_profile(*tier, spec);
            Some((
                "/usr/bin/sandbox-exec".to_string(),
                vec!["-p".to_string(), profile, "--".to_string()],
            ))
        }
        SandboxPlan::Bwrap { bwrap_bin, tier } => {
            let args = bwrap_args(*tier, spec);
            if args.is_empty() {
                return None;
            }
            Some((bwrap_bin.to_string_lossy().into_owned(), args))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 档位解析往返 + 未知值拒绝。
    #[test]
    fn tier_parse_roundtrip() {
        for (raw, want) in [
            ("auto", SandboxTier::Auto),
            ("Auto", SandboxTier::Auto),
            (" strict ", SandboxTier::Strict),
            ("off", SandboxTier::Off),
        ] {
            assert_eq!(SandboxTier::parse(raw), Some(want), "{raw}");
        }
        assert_eq!(SandboxTier::parse("on"), None, "未知值应拒");
    }

    /// resolve：Off 直通；macOS 恒 Seatbelt；Linux 无 bwrap 降级 Off、有则 Bwrap。
    #[test]
    fn resolve_platform_matrix() {
        assert_eq!(
            resolve(SandboxTier::Off, Some(PathBuf::from("/usr/bin/bwrap"))),
            SandboxPlan::Off
        );
        if cfg!(target_os = "macos") {
            assert_eq!(
                resolve(SandboxTier::Auto, None),
                SandboxPlan::Seatbelt {
                    tier: SandboxTier::Auto
                },
                "macOS 无需 bwrap 探测"
            );
            assert_eq!(
                resolve(SandboxTier::Strict, Some(PathBuf::from("/x"))),
                SandboxPlan::Seatbelt {
                    tier: SandboxTier::Strict
                },
                "macOS 忽略 bwrap 参数"
            );
        }
        if cfg!(target_os = "linux") {
            assert_eq!(
                resolve(SandboxTier::Auto, None),
                SandboxPlan::Off,
                "无 bwrap 降级"
            );
            assert_eq!(
                resolve(SandboxTier::Strict, Some(PathBuf::from("/usr/bin/bwrap"))),
                SandboxPlan::Bwrap {
                    bwrap_bin: PathBuf::from("/usr/bin/bwrap"),
                    tier: SandboxTier::Strict
                }
            );
        }
    }

    /// status_line：Off 无输出；其余带机制名 + 档位。
    #[test]
    fn status_line_shapes() {
        assert_eq!(SandboxPlan::Off.status_line(), None);
        let s = SandboxPlan::Seatbelt {
            tier: SandboxTier::Auto,
        }
        .status_line()
        .unwrap();
        assert!(s.contains("seatbelt(auto)"), "{s}");
        let s = SandboxPlan::Bwrap {
            bwrap_bin: PathBuf::from("/usr/bin/bwrap"),
            tier: SandboxTier::Strict,
        }
        .status_line()
        .unwrap();
        assert!(s.contains("实验性"), "{s}");
    }

    /// Seatbelt profile：规则序 = 后规则覆盖前规则（真机校准 2026-10-22）——
    /// 断言 ①基座在最前 ②状态目录 deny 在例外 allow 之前 ③strict 的全局
    /// deny 在 RW allow 之前 ④sock/token/mcp 例外存在。
    #[test]
    fn seatbelt_profile_rule_ordering() {
        let spec = SandboxSpec {
            workdir: PathBuf::from("/tmp/ws"),
            mcp_json: Some(PathBuf::from("/home/u/.imagent/mcp_c1.json")),
            extra_write: vec![],
        };
        let p = seatbelt_profile(SandboxTier::Auto, &spec);
        assert!(p.starts_with("(version 1)"));
        let base = p.find("(allow default)").unwrap();
        let deny_state = p.find("(deny file-read*").unwrap();
        let allow_sock = p.find("(allow file-read* file-write* (literal").unwrap();
        assert!(base < deny_state, "基座在前");
        assert!(deny_state < allow_sock, "deny 在例外 allow 之前（后者胜）");
        assert!(p.contains("permission.sock"));
        assert!(p.contains("mcp_c1.json"), "每轮 mcp json 例外: {p}");
        assert!(p.contains("permission.token"));

        // Guarded 无全局写 deny；Strict 有且在 RW 例外之前。
        assert!(
            !p.contains("(deny file-write* (subpath \"/\"))"),
            "guarded 无全局写 deny"
        );
        let ps = seatbelt_profile(SandboxTier::Strict, &spec);
        let broad = ps.find("(deny file-write* (subpath \"/\"))").unwrap();
        let rw = ps
            .find(&format!(
                "(allow file-write* (subpath {})",
                sb_quote("/tmp/ws")
            ))
            .unwrap();
        assert!(broad < rw, "全局 deny 在 RW allow 之前");
        assert!(ps.contains(".claude"), "strict 放行 claude 自身状态");
        assert!(ps.contains("\"/dev/null\""), "strict 放行 dev/null");
        // 持久化热点（guarded 也有）。
        assert!(p.contains(".ssh"), "ssh 写 deny");
        assert!(p.contains(".zshrc"), "rc 文件写 deny");
    }

    /// strict 的 extra_write 进 RW allow 清单。
    #[test]
    fn seatbelt_strict_extra_write_allowed() {
        let spec = SandboxSpec {
            workdir: PathBuf::from("/tmp/ws"),
            mcp_json: None,
            extra_write: vec![PathBuf::from("/home/u/.npm")],
        };
        let p = seatbelt_profile(SandboxTier::Strict, &spec);
        assert!(p.contains(&format!(
            "(allow file-write* (subpath {}))",
            sb_quote("/home/u/.npm")
        )));
    }

    /// bwrap 参数：基座→tmpfs 遮蔽→三文件重挂→热点 ro→strict RW→`--` 收尾，
    /// 顺序敏感（挂载序即遮蔽序）。
    #[test]
    fn bwrap_args_mount_ordering() {
        let spec = SandboxSpec {
            workdir: PathBuf::from("/tmp/ws"),
            mcp_json: Some(PathBuf::from("/home/u/.imagent/mcp_c1.json")),
            extra_write: vec![],
        };
        let a = bwrap_args(SandboxTier::Auto, &spec);
        assert_eq!(a.first().unwrap(), "--die-with-parent");
        let base = a.iter().position(|x| x == "--bind").unwrap();
        let tmpfs = a.iter().position(|x| x == "--tmpfs").unwrap();
        let sock = a
            .iter()
            .position(|x| x.ends_with("permission.sock"))
            .expect("sock 应重挂（完整路径形态）");
        assert!(base < tmpfs && tmpfs < sock, "基座→遮蔽→重挂");
        assert!(a.iter().any(|x| x == "--ro-bind-try"), "热点 ro 遮蔽");
        assert_eq!(a.last().unwrap(), "--", "以 -- 收尾接原命令");

        let s = bwrap_args(SandboxTier::Strict, &spec);
        assert!(s.iter().any(|x| x == "--ro-bind"), "strict 基座只读");
        assert!(
            s.iter().position(|x| x == "--ro-bind") < s.iter().position(|x| x == "--bind"),
            "只读基座在前、RW 重挂在后"
        );
    }

    /// wrap_command：Off → None；Seatbelt → sandbox-exec -p profile --；Bwrap →
    /// 二进制路径 + args。
    #[test]
    fn wrap_command_shapes() {
        let spec = SandboxSpec::default();
        assert!(wrap_command(&SandboxPlan::Off, &spec).is_none());
        let (prog, args) = wrap_command(
            &SandboxPlan::Seatbelt {
                tier: SandboxTier::Auto,
            },
            &spec,
        )
        .unwrap();
        assert_eq!(prog, "/usr/bin/sandbox-exec");
        assert_eq!(&args[0], "-p");
        assert!(args[1].starts_with("(version 1)"));
        assert_eq!(args.last().unwrap(), "--");
    }

    /// 真机校准回归（macOS，需 `/usr/bin/sandbox-exec`；2026-10-22 校准事实的
    /// 自动化防线）：生成的 profile 直接喂 sandbox-exec——① deny 生效（状态目录
    /// 读被拒）② 例外生效（token 可读）③ 持久化热点写被拒。标记 `#[ignore]`
    /// 防 CI 依赖宿主文件系统状态；本地跑：`cargo test -p imagent-core --
    /// --ignored sandbox_real`。
    #[cfg(all(test, target_os = "macos"))]
    #[test]
    #[ignore = "真机校准测试：写宿主临时目录，本地手动跑"]
    fn sandbox_real_seatbelt_profile_guards_state_dir() {
        let dir = std::env::temp_dir().join(format!(
            "imagent_sb_real_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("secret.txt"), "SECRET").unwrap();
        // 例外文件（token 形态）。
        std::fs::write(state.join("permission.token"), "tok").unwrap();
        let mcp = state.join("mcp_c1.json");
        std::fs::write(&mcp, "{}").unwrap();
        // 持久化热点替身：用 state 下 rc 形态文件模拟（真实 ~/.ssh 不可动）。
        std::fs::write(state.join(".zshrc"), "# rc").unwrap();

        // 直接调用内部构件：把 state 当状态目录根（绕过 imagent_home——
        // 用 sb_quote/规则序手工拼等价 profile，聚焦语义而非真实 home）。
        // 路径一律 canonicalize 形态（真机校准：SBPL 按 canonical 匹配，
        // temp_dir 的 /var/folders 字面形态会落空）。
        let state = state.canonicalize().unwrap();
        let mcp = state.join("mcp_c1.json");
        let p = sb_quote(&state.to_string_lossy());
        let profile = format!(
            "(version 1)\n(allow default)\n(deny file-read* (subpath {p}))\n(deny file-write* (subpath {p}))\n(deny file-write* (literal {}))\n(allow file-read* (literal {}))\n(allow file-read* (literal {}))\n",
            sb_quote(&state.join(".zshrc").to_string_lossy()),
            sb_quote(&state.join("permission.token").to_string_lossy()),
            sb_quote(&mcp.to_string_lossy()),
        );
        let run = |args: &[&str]| {
            std::process::Command::new("/usr/bin/sandbox-exec")
                .arg("-p")
                .arg(&profile)
                .arg("--")
                .args(args)
                .output()
                .expect("sandbox-exec 应存在")
        };
        // ① 状态目录读被拒。
        let out = run(&["cat", &state.join("secret.txt").to_string_lossy()]);
        assert!(!out.status.success(), "deny 生效应拦读");
        // ② 例外文件可读（后置 allow 覆盖 deny）。
        let out = run(&["cat", &state.join("permission.token").to_string_lossy()]);
        assert!(
            out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "tok",
            "例外放行应可读: {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        // ③ 持久化热点写被拒。
        let rc = state.join(".zshrc").to_string_lossy().into_owned();
        let out = run(&["sh", "-c", &format!("echo x > '{rc}'")]);
        assert!(!out.status.success(), "热点写 deny 应拦");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真机校准（macOS）：**真实生成器输出**的 Guarded profile 可被 sandbox-exec
    /// 编译执行，且语义正确——① 正常命令（git）不受影响（零破坏面）② 真实
    /// imagent 状态目录（生效 home 的 `imagent.db`）读被内核层拦截（本批的
    /// 核心交付断言）。`#[ignore]`：依赖宿主 git 与真实 `~/.imagent` 布局。
    #[cfg(all(test, target_os = "macos"))]
    #[test]
    #[ignore = "真机校准测试：跑真实生成 profile，本地手动跑"]
    fn sandbox_real_generated_profile_guards_real_state_dir() {
        let spec = SandboxSpec {
            workdir: std::env::current_dir().unwrap(),
            mcp_json: None,
            extra_write: vec![],
        };
        let profile = seatbelt_profile(SandboxTier::Auto, &spec);
        let run = |args: &[&str]| {
            std::process::Command::new("/usr/bin/sandbox-exec")
                .arg("-p")
                .arg(&profile)
                .arg("--")
                .args(args)
                .output()
                .expect("sandbox-exec 应存在")
        };
        // ① 正常命令不受影响（profile 编译 + 执行双验证）。
        let out = run(&["/usr/bin/git", "--version"]);
        assert!(
            out.status.success(),
            "git 应正常运行（零破坏面）：{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // ② 真实状态目录读被拦（db 不存在时 cat 也应被 subpath deny 拦——
        // deny 在 resolve 之前生效，无需文件存在）。
        let db = crate::paths::imagent_home().join("imagent.db");
        let out = run(&["/bin/cat", &db.to_string_lossy()]);
        assert!(
            !out.status.success(),
            "状态目录读应被内核层拦截（本批核心交付）"
        );
    }
}
