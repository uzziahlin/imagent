//! v1.43 Claude Code Stop 钩子的两个入口（逻辑全在本模块，main 只做参数接线
//! ——先例 `join` / `service`）。
//!
//! 1. **客户端 `imagent peer-hook --to <网关 peer 名>`**（内部子命令，由 Claude
//!    Code 在每轮结束时调用，不直接手动使用）：读 stdin JSON（`session_id` /
//!    `cwd` / `last_assistant_message`）→ 在注册表里找到目标网关
//!    （`entrypoint == "imagent"`）→ 把 hook 帧写到网关的 peer socket。
//!    **任何失败都静默退出、退出码恒为 0**：Claude Code 把 Stop 钩子的非零
//!    退出当成错误（退出码 2 会把 stderr 喂回模型并阻止停止）——钩子只是
//!    best-effort 的旁路同步，不能打断窗口的正常收尾。仅当环境变量
//!    `IMAGENT_PEER_HOOK_DEBUG=1` 时向 stderr 打印原因。
//! 2. **安装管理 `imagent peer hook <install|uninstall|status>`**：向 Claude Code
//!    配置写入 / 移除 / 查看一个 Stop 钩子条目
//!    （`{"type":"command","command":"<imagent> peer-hook --to <名>","timeout":5,
//!    "async":true}`；`async` 后台执行不阻塞 Claude）。做成显式命令而不是网关
//!    启动时自动改配置——动用户的全局配置必须由用户主动决定；网关只读探测
//!    是否已装（驾驶舱 `windows` 帧的 `hookInstalled`）。
//!
//! 改写纪律（[`install_hook`] / [`uninstall_hook`]，路径注入的纯函数）：
//! - **不改用户的其它配置**：读成 `serde_json::Value` 增量修改；追加时新增一个
//!   Stop 组，不碰用户已有的组；
//! - **解析失败直接报错、不写文件**；文件不存在按 `{}` 起步；
//! - 改写前把原文件备份为 `<文件>.imagent-bak`（已存在则覆盖），再**原子替换**
//!   （同目录临时文件 → rename：写到一半被杀也不会留下半截配置）；
//! - 新建文件 0600（含敏感配置的惯例），已存在的文件保持原权限；
//! - 条目识别（[`hook_entry_matches`]）与网关的 `hookInstalled` 探测共用 core 里
//!   的同一个定义。
//!
//! 输出惯例：结果与下一步走 stdout；`peer-hook` 客户端正常情况下不输出任何
//! 字节（stderr 仅调试开关打开时）。

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use imagent_core::peer::{
    build_hook_frame_json, clip_hook_message, hook_entry_matches, send_frame, PeerEntry,
    PeerRegistry, HOOK_EVENT_STOP,
};

/// `--to` 的缺省值（= 网关 config `peer_name` 的缺省值）。
pub const DEFAULT_PEER_NAME: &str = "imagent";

/// peer 名长度上限（字符）。窗口名是注册表里的短标识，上限只为挡手滑。
const PEER_NAME_MAX_CHARS: usize = 64;

/// 钩子条目的 `timeout`（秒）。客户端只做「读 stdin → 找网关 → 写一帧」，正常
/// 毫秒级；5s 是 Claude Code 侧的兜底（网关 `send_frame` 的总预算同值）。
const HOOK_TIMEOUT_SECS: u64 = 5;

/// peer-hook 客户端 stdin 读取上限（与网关入站 JSON 行上限同口径）。
const STDIN_MAX_BYTES: usize = 1024 * 1024;

/// peer-hook 客户端的调试开关环境变量（=1 时向 stderr 打印未上报的原因）。
const DEBUG_ENV: &str = "IMAGENT_PEER_HOOK_DEBUG";

/// 备份文件后缀（`<配置文件>.imagent-bak`）。
const BACKUP_SUFFIX: &str = ".imagent-bak";

/// 原子写的临时文件后缀（同目录，写完 rename 覆盖）。
const TMP_SUFFIX: &str = ".imagent-tmp";

// ---------------------------------------------------------------------------
// 钩子写入范围
// ---------------------------------------------------------------------------

/// `--scope`：钩子写到哪一级 Claude Code 配置。
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum HookScope {
    /// 用户级：`~/.claude/settings.json`（对本机全部 Claude Code 会话生效）。
    User,
    /// 项目级：`<当前目录>/.claude/settings.json`（只对该项目的会话生效）。
    Project,
}

impl HookScope {
    fn label(self) -> &'static str {
        match self {
            HookScope::User => "user",
            HookScope::Project => "project",
        }
    }
}

/// 用户级配置路径：`~/.claude/settings.json`（home 不可解析 → Err）。
fn user_settings_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("无法定位 home 目录"))?;
    Ok(home.join(".claude").join("settings.json"))
}

/// 项目级配置路径（纯函数）：`<cwd>/.claude/settings.json`。
pub fn project_settings_path(cwd: &Path) -> PathBuf {
    cwd.join(".claude").join("settings.json")
}

/// 按 scope 解析配置文件路径（project = 当前目录；目录不存在由写入侧创建）。
pub fn settings_path(scope: HookScope) -> Result<PathBuf> {
    match scope {
        HookScope::User => user_settings_path(),
        HookScope::Project => {
            let cwd = std::env::current_dir().context("无法获取当前目录")?;
            Ok(project_settings_path(&cwd))
        }
    }
}

/// 当前二进制的绝对路径（`std::env::current_exe()`）。**不做 symlink 解析**：
/// `/usr/local/bin/imagent` 这类稳定入口若解析到带版本号的真实路径，升级后
/// 钩子就指向了被删掉的旧文件；保留入口路径则随升级自然可用（入口真的挪了
/// 时重跑 `install` 会原位更新条目，见 [`HookChange::Updated`]）。
pub fn current_exe_string() -> Result<String> {
    let exe = std::env::current_exe().context("定位当前 imagent 二进制失败")?;
    Ok(exe.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// 条目命令与校验（纯函数）
// ---------------------------------------------------------------------------

/// peer 名合法性：非空、≤ [`PEER_NAME_MAX_CHARS`] 字符、**首字符是字母数字**、
/// 其余只含字母数字与 `. _ - + @`。钩子命令 `… peer-hook --to <名>` 经 shell
/// 解析再经 clap 解析，两头都不能出岔子：名字里有空格 / 引号 / 元字符会被
/// shell 拆错；以 `-` 开头会被 clap 当成选项——两者都是非零退出（clap 用法
/// 错误是退出码 2，Claude Code 会把 stderr 喂回模型并阻止停止）；而网关侧的
/// 条目识别又要求命令里逐字出现 `--to <名>`，带引号的名字两边对不上。
fn validate_peer_name(to: &str) -> Result<()> {
    let ok = to.chars().next().is_some_and(|c| c.is_alphanumeric())
        && to.chars().count() <= PEER_NAME_MAX_CHARS
        && to
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '@'));
    if !ok {
        bail!(
            "peer 名 {to:?} 不合法：须以字母或数字开头，仅含字母、数字与 `. _ - + @`，长度 1..={PEER_NAME_MAX_CHARS}。\n\
             请把网关 config.toml 的 peer_name 改成合法值（改动需重启网关）后重试。"
        );
    }
    Ok(())
}

/// 可执行路径的 shell 安全形态：只含字母数字与 `/ . - _ + @` 时原样，否则整体
/// 用单引号包裹（内部 `'` → `'\''`）——home 目录带空格时钩子命令才不会被拆开。
/// 识别用的 `peer-hook --to <名>` 两个词不受影响。
fn shell_word(exe: &str) -> String {
    let safe = !exe.is_empty()
        && exe
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '/' | '.' | '-' | '_' | '+' | '@'));
    if safe {
        exe.to_string()
    } else {
        format!("'{}'", exe.replace('\'', r"'\''"))
    }
}

/// 钩子条目的 `command` 原文：`<imagent 绝对路径> peer-hook --to <名>`。
pub fn hook_command(exe: &str, to: &str) -> Result<String> {
    validate_peer_name(to)?;
    if exe.is_empty() {
        bail!("imagent 可执行路径为空");
    }
    Ok(format!("{} peer-hook --to {to}", shell_word(exe)))
}

/// 钩子条目的规范形态（struct 序列化：字段声明序 `type` / `command` / `timeout`
/// / `async`）。落盘键序取决于 serde_json 的 Map 实现——工作区依赖图打开
/// `preserve_order` 时保持插入序，否则字母序（见 [`json_preserves_order`]）。
#[derive(serde::Serialize)]
struct HookEntry {
    r#type: &'static str,
    command: String,
    timeout: u64,
    r#async: bool,
}

/// 本次构建的 serde_json Map 是否保插入序（特性 `preserve_order` 被依赖图里
/// 某个 crate 打开时为真）。决定「已改写的配置文件键序是否可能被重排」的
/// 提示文案，单测也据它选择逐字节期望值。
fn json_preserves_order() -> bool {
    serde_json::to_string(&serde_json::json!({"b": 1, "a": 2}))
        .is_ok_and(|s| s.starts_with("{\"b\""))
}

/// [`install_hook`] 的结果（CLI 据此打印；单测断言）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookChange {
    /// 配置文件不存在：按 `{}` 起步新建并写入。
    Created,
    /// 已有文件：追加了钩子条目（改写前已备份）。
    Added,
    /// 已有本网关的条目，但与本二进制应有的命令不一致（典型：升级后二进制
    /// 挪了位置；或手改过 / 重复多条）：首条原位换成规范条目、其余重复项删除。
    Updated,
    /// 已有等价条目：未改写文件。
    Unchanged,
}

// ---------------------------------------------------------------------------
// 配置文件读取 / 定位（增量修改的导航，全部不 panic）
// ---------------------------------------------------------------------------

/// 读配置：文件不存在 → `{}`；存在但解析失败 → Err（调用方不写文件）。
fn load_settings(path: &Path) -> Result<serde_json::Value> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let raw = std::fs::read(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    serde_json::from_slice(&raw).with_context(|| {
        format!(
            "{} 不是合法 JSON（未改动该文件）。请先修复它（或从 {}.imagent-bak 恢复）再重试",
            path.display(),
            path.display()
        )
    })
}

/// 取 `hooks.Stop` 组数组（可变；`create` = 缺键时创建）。形状不符 → Err，
/// 不猜着改。
fn stop_groups_mut(
    root: &mut serde_json::Value,
    create: bool,
) -> Result<&mut Vec<serde_json::Value>> {
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("配置文件顶层不是 JSON 对象，拒绝改写"))?;
    if obj.get("hooks").is_none() {
        if !create {
            bail!("配置文件没有 hooks.{HOOK_EVENT_STOP}");
        }
        obj.insert("hooks".into(), serde_json::json!({}));
    }
    let hooks = obj
        .get_mut("hooks")
        .ok_or_else(|| anyhow!("配置文件的 hooks 键丢失"))?;
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("配置文件的 hooks 不是对象，拒绝改写"))?;
    if hooks.get(HOOK_EVENT_STOP).is_none() {
        if !create {
            bail!("配置文件没有 hooks.{HOOK_EVENT_STOP}");
        }
        hooks.insert(HOOK_EVENT_STOP.into(), serde_json::json!([]));
    }
    hooks
        .get_mut(HOOK_EVENT_STOP)
        .ok_or_else(|| anyhow!("配置文件的 hooks.{HOOK_EVENT_STOP} 键丢失"))?
        .as_array_mut()
        .ok_or_else(|| anyhow!("配置文件的 hooks.{HOOK_EVENT_STOP} 不是数组，拒绝改写"))
}

/// 配置里全部「网关 `<to>` 的钩子条目」的位置：`(组下标, 组内条目下标)`，
/// 按文件顺序。
fn our_hook_indices(root: &serde_json::Value, to: &str) -> Vec<(usize, usize)> {
    let mut hits = Vec::new();
    let Some(groups) = root
        .get("hooks")
        .and_then(|h| h.get(HOOK_EVENT_STOP))
        .and_then(|s| s.as_array())
    else {
        return hits;
    };
    for (g, group) in groups.iter().enumerate() {
        if let Some(hooks) = group.get("hooks").and_then(|h| h.as_array()) {
            for (h, hook) in hooks.iter().enumerate() {
                if hook_entry_matches(hook, to) {
                    hits.push((g, h));
                }
            }
        }
    }
    hits
}

/// 第 `g` 个 Stop 组的第 `h` 条（只读）。
fn entry_at(root: &serde_json::Value, g: usize, h: usize) -> Option<&serde_json::Value> {
    root.get("hooks")?
        .get(HOOK_EVENT_STOP)?
        .as_array()?
        .get(g)?
        .get("hooks")?
        .as_array()?
        .get(h)
}

/// 第 `g` 个 Stop 组的内层 `hooks` 数组（组存在且形如对象 + hooks 数组）。
fn group_hooks_mut(root: &mut serde_json::Value, g: usize) -> Result<&mut Vec<serde_json::Value>> {
    let missing = || {
        anyhow!("hooks.{HOOK_EVENT_STOP}[{g}] 组不存在或没有 hooks 数组（配置在处理期间被改？）")
    };
    match root
        .get_mut("hooks")
        .and_then(|h| h.get_mut(HOOK_EVENT_STOP))
        .and_then(|s| s.as_array_mut())
    {
        Some(groups) => match groups.get_mut(g) {
            Some(group) => group
                .get_mut("hooks")
                .and_then(|h| h.as_array_mut())
                .ok_or_else(missing),
            None => Err(missing()),
        },
        None => Err(missing()),
    }
}

/// 收尾清理：`touched` 里内层 `hooks` 已空的组删掉（只删我们清空的——用户
/// 自己留的空组不动）；随后 `hooks.Stop` 空则删 `Stop` 键，`hooks` 空则删
/// `hooks` 键。
fn prune_stop_hooks(root: &mut serde_json::Value, touched: &[usize]) -> Result<()> {
    let emptied: Vec<usize> = {
        let groups = stop_groups_mut(root, false)?;
        touched
            .iter()
            .copied()
            .filter(|&g| {
                groups.get(g).is_some_and(|group| {
                    group
                        .get("hooks")
                        .and_then(|h| h.as_array())
                        .is_some_and(|h| h.is_empty())
                })
            })
            .collect()
    };
    let groups = stop_groups_mut(root, false)?;
    for g in emptied.into_iter().rev() {
        groups.remove(g);
    }
    let hooks = root
        .get_mut("hooks")
        .and_then(|h| h.as_object_mut())
        .ok_or_else(|| anyhow!("配置文件的 hooks 不是对象"))?;
    if hooks
        .get(HOOK_EVENT_STOP)
        .and_then(|s| s.as_array())
        .is_none_or(|a| a.is_empty())
    {
        hooks.remove(HOOK_EVENT_STOP);
    }
    if hooks.is_empty() {
        root.as_object_mut()
            .ok_or_else(|| anyhow!("配置文件顶层不是 JSON 对象"))?
            .remove("hooks");
    }
    Ok(())
}

fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(BACKUP_SUFFIX);
    PathBuf::from(s)
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(TMP_SUFFIX);
    PathBuf::from(s)
}

/// 原子改写：同目录临时文件写好（见 [`write_temp`]）→ `rename` 覆盖。失败路径
/// 上清掉临时文件，不留残片。
fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let tmp = tmp_path(path);
    let _ = std::fs::remove_file(&tmp);
    let attempt = write_temp(&tmp, bytes, mode).and_then(|()| std::fs::rename(&tmp, path));
    if attempt.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    attempt
}

/// 临时文件新建即 0600（内容落盘前没有「世界可读」的窗口），再按 `mode`
/// 校准——chmod 不受 umask 影响，已存在文件的权限能逐位保持。
#[cfg(unix)]
fn write_temp(tmp: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)?;
    f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    f.write_all(bytes)?;
    f.sync_all()
}

#[cfg(not(unix))]
fn write_temp(tmp: &Path, bytes: &[u8], _mode: u32) -> io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// 改写配置文件：已存在则先备份（`fs::copy` 连权限一并带走；备份已存在则
/// 覆盖）并保持原权限，新建则 0600；输出 `to_string_pretty` + 末尾换行。
fn write_settings(path: &Path, root: &serde_json::Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("创建目录 {} 失败", dir.display()))?;
    }
    let existed = path.exists();
    let mode = if existed {
        let bak = backup_path(path);
        std::fs::copy(path, &bak).with_context(|| {
            format!(
                "备份 {} → {} 失败（未改动原文件）",
                path.display(),
                bak.display()
            )
        })?;
        file_mode(path)?
    } else {
        0o600
    };
    let mut text = serde_json::to_string_pretty(root).context("配置序列化失败")?;
    text.push('\n');
    atomic_write(path, text.as_bytes(), mode)
        .with_context(|| format!("写入 {} 失败", path.display()))
}

#[cfg(unix)]
fn file_mode(path: &Path) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    let meta =
        std::fs::metadata(path).with_context(|| format!("读取 {} 的权限失败", path.display()))?;
    Ok(meta.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> Result<u32> {
    Ok(0o600)
}

// ---------------------------------------------------------------------------
// 安装 / 卸载 / 状态（路径注入的纯函数）
// ---------------------------------------------------------------------------

/// 向 Claude Code 配置写入本网关的 Stop 钩子（幂等）。返回是否 / 如何改动了
/// 文件；[`HookChange::Unchanged`] 时文件字节不动、也不重写备份。
pub fn install_hook(settings_path: &Path, exe: &str, to: &str) -> Result<HookChange> {
    let want = serde_json::to_value(HookEntry {
        r#type: "command",
        command: hook_command(exe, to)?,
        timeout: HOOK_TIMEOUT_SECS,
        r#async: true,
    })
    .context("钩子条目序列化失败")?;
    let existed = settings_path.exists();
    let mut root = load_settings(settings_path)?;
    let hits = our_hook_indices(&root, to);
    // 恰一条且与规范条目逐字段相同 → 无变化（文件字节不动，也不重写备份）。
    if let [(g, h)] = hits.as_slice() {
        if entry_at(&root, *g, *h) == Some(&want) {
            return Ok(HookChange::Unchanged);
        }
    }
    let change = match hits.as_slice() {
        // 没有本网关的条目 → 追加一个新组（不碰用户已有的 Stop 组）。
        [] => {
            stop_groups_mut(&mut root, true)?.push(serde_json::json!({ "hooks": [want] }));
            if existed {
                HookChange::Added
            } else {
                HookChange::Created
            }
        }
        // 有本网关的条目但与规范条目不同（旧路径 / 被改过 / 重复多条）：首条
        // 原位换成规范条目，其余重复项删掉（倒序删，前面的下标不受影响）。
        [first, rest @ ..] => {
            let mut touched = Vec::new();
            for &(g, h) in rest.iter().rev() {
                group_hooks_mut(&mut root, g)?.remove(h);
                touched.push(g);
            }
            let hooks = group_hooks_mut(&mut root, first.0)?;
            let slot = hooks
                .get_mut(first.1)
                .ok_or_else(|| anyhow!("本网关的钩子条目在处理期间消失（配置被并发改写？）"))?;
            *slot = want;
            prune_stop_hooks(&mut root, &touched)?;
            HookChange::Updated
        }
    };
    write_settings(settings_path, &root)?;
    Ok(change)
}

/// 移除本网关（`--to <to>`）的全部 Stop 钩子条目：别的 Stop 钩子保留；被清空
/// 的组删掉，`hooks.Stop` 空则删键、`hooks` 空则删键。返回是否有删除——文件
/// 不存在或未安装 → `Ok(false)`（不改写）。解析失败 → Err（不写文件）。
pub fn uninstall_hook(settings_path: &Path, to: &str) -> Result<bool> {
    if !settings_path.exists() {
        return Ok(false);
    }
    let mut root = load_settings(settings_path)?;
    let hits = our_hook_indices(&root, to);
    if hits.is_empty() {
        return Ok(false);
    }
    let mut touched = Vec::new();
    for &(g, h) in hits.iter().rev() {
        group_hooks_mut(&mut root, g)?.remove(h);
        touched.push(g);
    }
    prune_stop_hooks(&mut root, &touched)?;
    write_settings(settings_path, &root)?;
    Ok(true)
}

/// 只读：配置里本网关 Stop 钩子条目的 `command` 原文（文件顺序；空 = 未安装
/// 或文件不存在）。解析失败 → Err。
pub fn hook_status(settings_path: &Path, to: &str) -> Result<Vec<String>> {
    if !settings_path.exists() {
        return Ok(Vec::new());
    }
    let root = load_settings(settings_path)?;
    Ok(our_hook_indices(&root, to)
        .into_iter()
        .filter_map(|(g, h)| entry_at(&root, g, h))
        .filter_map(|hook| {
            hook.get("command")
                .and_then(|c| c.as_str())
                .map(str::to_string)
        })
        .collect())
}

// ---------------------------------------------------------------------------
// CLI 打印层（参数接线 + 人读输出）
// ---------------------------------------------------------------------------

fn print_command(path: &Path, command: &str) {
    println!("   配置文件：{}", path.display());
    println!("   命令：{command}");
}

fn print_next_step() {
    println!(
        "之后：在已登录的 Claude Code 里跑完一轮，驾驶舱的「peer:<窗口名>」会话就会出现 \
         「📋 本轮结束（自动同步）」。"
    );
    println!(
        "      随时 `imagent peer hook status` 查看安装情况；`imagent peer hook uninstall` 卸载。"
    );
}

/// `imagent peer hook install [--to] [--scope]`。
pub fn install(to: &str, scope: HookScope) -> Result<()> {
    let path = settings_path(scope)?;
    let exe = current_exe_string()?;
    let change = install_hook(&path, &exe, to)?;
    let command = hook_command(&exe, to)?;
    match change {
        HookChange::Created => {
            println!("✅ 已安装 Stop 钩子（scope：{}）", scope.label());
            print_command(&path, &command);
        }
        HookChange::Added => {
            println!("✅ 已安装 Stop 钩子（scope：{}）", scope.label());
            print_command(&path, &command);
            println!(
                "   原文件已备份：{}（配置重新排版：JSON 缩进统一为两空格{}；内容不变）",
                backup_path(&path).display(),
                if json_preserves_order() {
                    "，键序保持原样"
                } else {
                    "，键序可能被重排为字母序"
                }
            );
        }
        HookChange::Updated => {
            println!("♻️  已更新 Stop 钩子（原条目的命令与当前二进制不一致，已原位替换）");
            print_command(&path, &command);
            println!("   原文件已备份：{}", backup_path(&path).display());
        }
        HookChange::Unchanged => {
            println!("✅ Stop 钩子此前已安装，无需改动");
            print_command(&path, &command);
        }
    }
    print_next_step();
    Ok(())
}

/// `imagent peer hook uninstall [--to] [--scope]`。
pub fn uninstall(to: &str, scope: HookScope) -> Result<()> {
    let path = settings_path(scope)?;
    if uninstall_hook(&path, to)? {
        println!(
            "🗑  已卸载 Stop 钩子（scope：{}；原文件已备份：{}）",
            scope.label(),
            backup_path(&path).display()
        );
    } else {
        println!(
            "未安装（scope：{}，{}）——无需卸载",
            scope.label(),
            path.display()
        );
    }
    Ok(())
}

/// `imagent peer hook status [--to] [--scope]`（只读）。
pub fn status(to: &str, scope: HookScope) -> Result<()> {
    let path = settings_path(scope)?;
    let commands = hook_status(&path, to)?;
    println!("配置文件：{}", path.display());
    if commands.is_empty() {
        println!(
            "状态：未安装（安装：imagent peer hook install --to {to} --scope {}）",
            scope.label()
        );
    } else {
        println!("状态：已安装（{} 条）", commands.len());
        for c in commands {
            println!("   {c}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// peer-hook 客户端（由 Claude Code 的 Stop 钩子调用）
// ---------------------------------------------------------------------------

/// 一条待上报的钩子输入（stdin JSON 解析产物；`message` 已截断）。
struct HookInput {
    session_id: String,
    cwd: Option<String>,
    message: String,
}

/// 客户端入口：**任何失败都只静默返回**（调试开关打开时向 stderr 打印原因）。
/// 调用方（main）保证进程退出码为 0。
pub async fn run_client(to: Option<String>) {
    let to = to.unwrap_or_else(|| DEFAULT_PEER_NAME.to_string());
    if let Err(reason) = deliver_from_stdin(&to).await {
        if std::env::var_os(DEBUG_ENV).is_some_and(|v| v == "1") {
            eprintln!("imagent peer-hook 未上报：{reason}");
        }
    }
}

/// 读 stdin（上限 [`STDIN_MAX_BYTES`]，超限即失败）→ 解析 → 投递。
async fn deliver_from_stdin(to: &str) -> Result<(), String> {
    let input = read_stdin()
        .await
        .map_err(|e| format!("读取 stdin 失败：{e}"))?;
    let report = parse_hook_input(&input).map_err(|e| e.to_string())?;
    let Some(report) = report else {
        return Ok(()); // message 为空：不上报（契约）
    };
    let registry_dir = imagent_core::peer::default_registry_dir()
        .ok_or_else(|| "无法定位 home 目录（注册表目录不可解析）".to_string())?;
    deliver(&PeerRegistry::new(registry_dir), to, &report).await
}

async fn read_stdin() -> io::Result<String> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    tokio::io::stdin()
        .take(STDIN_MAX_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .await?;
    if buf.len() > STDIN_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stdin 超过 1 MiB 上限",
        ));
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// 解析 Claude Code 传入的 hook JSON：`session_id` 必须非空；`message` 取
/// `last_assistant_message`（兼容直接给 `message`），为空 → `Ok(None)`（不上报）；
/// 非空则经 [`clip_hook_message`] 截断。`cwd` 可缺省。
fn parse_hook_input(input: &str) -> Result<Option<HookInput>> {
    let v: serde_json::Value =
        serde_json::from_str(input).context("stdin 不是合法 JSON（非 hook 输入？）")?;
    let text = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let session_id = text("session_id")
        .filter(|s| !s.is_empty())
        .context("stdin JSON 缺少非空的 session_id")?;
    let message = text("last_assistant_message")
        .or_else(|| text("message"))
        .unwrap_or_default();
    if message.is_empty() {
        return Ok(None);
    }
    Ok(Some(HookInput {
        session_id,
        cwd: text("cwd"),
        message: clip_hook_message(&message),
    }))
}

/// 向网关 `<to>` 投递一条上报（注册表注入——单测 / 端到端用；生产由
/// [`deliver_from_stdin`] 传 [`imagent_core::peer::default_registry_dir`]）。
/// 帧经既有 [`send_frame`]：写 JSON 行 → 半关闭 → 等网关关连接。
async fn deliver(registry: &PeerRegistry, to: &str, input: &HookInput) -> Result<(), String> {
    let entry = find_gateway_entry(registry, to).await?;
    let line = build_hook_frame_json(
        HOOK_EVENT_STOP,
        &input.session_id,
        input.cwd.as_deref().unwrap_or(""),
        &input.message,
    );
    send_frame(&entry.sock, &line)
        .await
        .map_err(|e| format!("投递 hook 帧失败：{e}"))
}

/// 在注册表里找目标网关：`entrypoint == "imagent"` 且套接字探活为活；优先
/// `name == to`，找不到且**只有一个**网关条目时用它（`--to` 缺省名与实际
/// 网关名不一致时仍能投到），多于一个则无法确定目标。
async fn find_gateway_entry(registry: &PeerRegistry, to: &str) -> Result<PeerEntry, String> {
    let mut gateways: Vec<PeerEntry> = registry
        .live_entries()
        .await
        .into_iter()
        .filter(|e| e.is_gateway())
        .collect();
    if let Some(i) = gateways.iter().position(|e| e.name == to) {
        return Ok(gateways.swap_remove(i));
    }
    if let [only] = gateways.as_slice() {
        return Ok(only.clone());
    }
    Err(if gateways.is_empty() {
        format!("注册表里没有在线的网关条目（--to {to}）")
    } else {
        format!("注册表里有多个网关条目且没有名为「{to}」的，无法确定投递目标")
    })
}

#[cfg(test)]
mod tests {
    //! 全部走 tempdir（路径注入的纯函数 + 注入注册表的客户端投递）：**不碰**
    //! 真实 `~/.claude/settings.json`、不读真实注册表、不起 claude 进程。

    use super::*;
    use tokio::io::AsyncReadExt;

    /// 唯一短 tempdir（peer socket 路径有长度上限，沿用 core peer 测试的做法：
    /// /tmp 短根 + 进程 id + 原子序号；无 tempfile 依赖）。
    fn tmp_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::path::Path::new("/tmp").join(format!(
            "ipb{tag}_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read file")
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    const EXE: &str = "/usr/local/bin/imagent";
    const CMD: &str = "/usr/local/bin/imagent peer-hook --to imagent";

    /// 首次写入的完整文件（键序取决于 serde_json Map 实现，两种都逐字节钉住）。
    fn first_write(preserved: bool) -> String {
        let entry = if preserved {
            // 字段声明序：type / command / timeout / async（契约示例的顺序）。
            [
                "\"type\": \"command\",",
                "\"command\": \"/usr/local/bin/imagent peer-hook --to imagent\",",
                "\"timeout\": 5,",
                "\"async\": true",
            ]
        } else {
            // 字母序：async / command / timeout / type。
            [
                "\"async\": true,",
                "\"command\": \"/usr/local/bin/imagent peer-hook --to imagent\",",
                "\"timeout\": 5,",
                "\"type\": \"command\"",
            ]
        };
        let [a, b, c, d] = entry;
        // `to_string_pretty`：两空格缩进、数组逐元素换行、末尾补一个换行。
        [
            "{",
            "  \"hooks\": {",
            "    \"Stop\": [",
            "      {",
            "        \"hooks\": [",
            "          {",
            &format!("            {a}"),
            &format!("            {b}"),
            &format!("            {c}"),
            &format!("            {d}"),
            "          }",
            "        ]",
            "      }",
            "    ]",
            "  }",
            "}",
            "",
        ]
        .join("\n")
    }

    fn own(cmd: &str) -> serde_json::Value {
        serde_json::json!({"type": "command", "command": cmd, "timeout": 5, "async": true})
    }

    /// 一个 Stop 组：`{"hooks": [条目…]}`。
    fn group(hooks: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({ "hooks": hooks })
    }

    #[test]
    fn install_first_write_full_json_shape() {
        let tmp = tmp_dir("inst");
        // 目录不存在 → 一并创建（project scope 的形态）。
        let path = tmp.join("proj").join(".claude").join("settings.json");
        assert_eq!(
            install_hook(&path, EXE, "imagent").unwrap(),
            HookChange::Created
        );
        let raw = read(&path);
        assert_eq!(
            raw,
            first_write(json_preserves_order()),
            "完整 JSON 形态逐字节"
        );
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0],
            own(CMD),
            "字段逐一：type / command / timeout=5 / async=true；Stop 无 matcher"
        );
        assert!(v["hooks"]["Stop"][0].get("matcher").is_none());
        // 新建文件 0600；原本不存在 → 没有备份；临时文件已清理。
        assert_eq!(mode(&path), 0o600, "新建文件 0600");
        assert!(!backup_path(&path).exists(), "首次新建无备份");
        assert!(!tmp_path(&path).exists(), "临时文件已清理");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 已存在配置文件：只追加（新增一个 Stop 组），其余键原样；改写前备份；
    /// 已存在文件的权限保持不变。
    #[test]
    fn install_appends_without_touching_other_keys() {
        let tmp = tmp_dir("append");
        let path = tmp.join("settings.json");
        let original = serde_json::json!({
            "model": "opus",
            "permissions": {"allow": ["Bash(ls:*)"], "deny": ["Read(./.env)"]},
            "env": {"ANTHROPIC_MODEL": "opus"},
            "hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [own("echo pre")]}],
                "SessionStart": [group(vec![own("echo start")])]
            }
        });
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&original).unwrap()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        assert_eq!(
            install_hook(&path, EXE, "imagent").unwrap(),
            HookChange::Added
        );

        let v: serde_json::Value = serde_json::from_str(&read(&path)).unwrap();
        assert_eq!(v["model"], "opus");
        assert_eq!(
            v["permissions"], original["permissions"],
            "permissions 原样"
        );
        assert_eq!(v["env"], original["env"], "env 原样");
        assert_eq!(
            v["hooks"]["PreToolUse"], original["hooks"]["PreToolUse"],
            "别的钩子事件原样"
        );
        assert_eq!(
            v["hooks"]["SessionStart"],
            original["hooks"]["SessionStart"]
        );
        assert_eq!(
            v["hooks"]["Stop"],
            serde_json::json!([group(vec![own(CMD)])]),
            "追加一个新组"
        );
        assert_eq!(
            v.as_object().unwrap().len(),
            original.as_object().unwrap().len(),
            "不新增顶层键"
        );
        // 备份 = 改写前的原文字节；权限保持。
        assert_eq!(
            read(&backup_path(&path)),
            format!("{}\n", serde_json::to_string_pretty(&original).unwrap())
        );
        assert_eq!(mode(&path), 0o644, "已存在文件保持原权限");
        assert_eq!(mode(&backup_path(&path)), 0o644, "备份连带权限");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 重复安装幂等：返回「无变化」、文件字节不动、不重写备份。
    #[test]
    fn install_is_idempotent() {
        let tmp = tmp_dir("idem");
        let path = tmp.join("settings.json");
        install_hook(&path, EXE, "imagent").unwrap();
        // 手动追加一个别的键，证明第二次 install 根本没重写文件。
        let mut v: serde_json::Value = serde_json::from_str(&read(&path)).unwrap();
        v["model"] = serde_json::json!("sonnet");
        std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
        let before = read(&path);
        assert_eq!(
            install_hook(&path, EXE, "imagent").unwrap(),
            HookChange::Unchanged
        );
        assert_eq!(read(&path), before, "文件字节不变");
        assert!(
            !backup_path(&path).exists(),
            "无变化 → 不改写、也不产生备份"
        );
        assert_eq!(
            hook_status(&path, "imagent").unwrap(),
            vec![CMD.to_string()],
            "仍恰一条"
        );
        // 装到别的名字是另一条：已有 imagent 的条目不影响给 imagent-2 安装。
        assert_eq!(
            install_hook(&path, EXE, "imagent-2").unwrap(),
            HookChange::Added
        );
        let mut cmds = hook_status(&path, "imagent").unwrap();
        cmds.extend(hook_status(&path, "imagent-2").unwrap());
        assert_eq!(cmds.len(), 2);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 解析失败的文件：直接报错、不写文件、不留备份与临时文件。顶层不是对象
    /// 同样拒绝。
    #[test]
    fn install_rejects_broken_json_without_touching_file() {
        let tmp = tmp_dir("broken");
        let path = tmp.join("settings.json");
        for bad in ["{ not json", "[]", "\"just a string\"", "{\"hooks\": 42}"] {
            std::fs::write(&path, bad).unwrap();
            let err = install_hook(&path, EXE, "imagent")
                .err()
                .unwrap_or_else(|| panic!("应拒绝：{bad}"));
            assert!(!err.to_string().is_empty());
            assert_eq!(read(&path), bad, "文件字节不变");
            assert!(!backup_path(&path).exists(), "未改写 → 无备份");
            assert!(!tmp_path(&path).exists(), "无临时文件残留");
            // 卸载 / 状态是只读的：解析失败同样报错（不吞坏文件）。
            if bad == "{ not json" {
                assert!(
                    uninstall_hook(&path, "imagent").is_err(),
                    "uninstall：{bad}"
                );
                assert!(hook_status(&path, "imagent").is_err(), "status：{bad}");
            }
        }
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 已有本网关的条目但命令与当前二进制不一致（升级后路径变了）/ 重复多条：
    /// 首条原位换成规范条目、其余重复项删掉、清空的组删掉；别的钩子不动。
    #[test]
    fn install_updates_stale_entry_and_collapses_duplicates() {
        let tmp = tmp_dir("stale");
        let path = tmp.join("settings.json");
        let stale = "/old/location/imagent peer-hook --to imagent";
        std::fs::write(
            &path,
            serde_json::json!({
                "model": "opus",
                "hooks": {"Stop": [
                    {"hooks": [own("echo bye"), own(stale)]},
                    {"matcher": "", "hooks": [own("echo other")]},
                    group(vec![own(stale)])
                ]}
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            install_hook(&path, EXE, "imagent").unwrap(),
            HookChange::Updated
        );
        let v: serde_json::Value = serde_json::from_str(&read(&path)).unwrap();
        assert_eq!(v["model"], "opus");
        assert_eq!(
            v["hooks"]["Stop"],
            serde_json::json!([
                {"hooks": [own("echo bye"), own(CMD)]},
                {"matcher": "", "hooks": [own("echo other")]}
            ]),
            "首条原位更新、重复项与其所在的空组删除、别的钩子保留"
        );
        assert_eq!(
            hook_status(&path, "imagent").unwrap(),
            vec![CMD.to_string()]
        );
        // 再装一次回到幂等。
        assert_eq!(
            install_hook(&path, EXE, "imagent").unwrap(),
            HookChange::Unchanged
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 卸载只删本网关的条目：别的 Stop 钩子保留；被清空的组删掉；`hooks.Stop`
    /// 空则删键、`hooks` 空则删键。文件不存在 / 未安装 → Ok(false) 且不改写。
    #[test]
    fn uninstall_only_own_entry_and_prunes_emptied_containers() {
        let tmp = tmp_dir("uninst");
        let path = tmp.join("settings.json");
        // 文件不存在。
        assert!(!uninstall_hook(&path, "imagent").unwrap());
        assert!(hook_status(&path, "imagent").unwrap().is_empty());

        // 同组里还有别的钩子：只删本网关那条。
        let mut original = serde_json::json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [own("echo pre")]}],
                "Stop": [{"hooks": [own("echo bye"), own(CMD)]}, group(vec![own(CMD)])]
            }
        });
        std::fs::write(&path, original.to_string()).unwrap();
        let original_raw = read(&path);
        assert!(uninstall_hook(&path, "imagent").unwrap());
        let v: serde_json::Value = serde_json::from_str(&read(&path)).unwrap();
        original["hooks"]["Stop"] = serde_json::json!([{"hooks": [own("echo bye")]}]);
        assert_eq!(
            v, original,
            "同组别条保留、本网关独占的组整个删除、其余原样"
        );
        assert_eq!(read(&backup_path(&path)), original_raw, "卸载前同样备份");

        // 本网关独占 Stop：卸载后 Stop 键删除，其余事件保留。
        let mut original = serde_json::json!({
            "hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [own("echo pre")]}],
                "Stop": [group(vec![own(CMD)])]
            }
        });
        std::fs::write(&path, original.to_string()).unwrap();
        assert!(uninstall_hook(&path, "imagent").unwrap());
        original["hooks"].as_object_mut().unwrap().remove("Stop");
        let v: serde_json::Value = serde_json::from_str(&read(&path)).unwrap();
        assert_eq!(v, original, "hooks.Stop 空则删键");

        // 本网关独占整个 hooks：卸载后 hooks 键也删除。
        std::fs::write(
            &path,
            serde_json::json!({"model": "opus", "hooks": {"Stop": [group(vec![own(CMD)])]}})
                .to_string(),
        )
        .unwrap();
        assert!(uninstall_hook(&path, "imagent").unwrap());
        assert_eq!(
            read(&path),
            "{\n  \"model\": \"opus\"\n}\n",
            "hooks 空则删键"
        );

        // 未安装（装的是别的网关）：Ok(false)、不改写——--to imagent 不会误删 imagent-2 的条目。
        let untouched = serde_json::json!({"hooks": {"Stop": [group(vec![own("/bin/imagent peer-hook --to imagent-2")])]}}).to_string();
        std::fs::write(&path, &untouched).unwrap();
        assert!(!uninstall_hook(&path, "imagent").unwrap());
        assert_eq!(read(&path), untouched);
        // 用户自己留的空组不因卸载被顺手清掉。
        std::fs::write(
            &path,
            serde_json::json!({"hooks": {"Stop": [group(vec![own(CMD)]), group(vec![])]}})
                .to_string(),
        )
        .unwrap();
        assert!(uninstall_hook(&path, "imagent").unwrap());
        let v: serde_json::Value = serde_json::from_str(&read(&path)).unwrap();
        assert_eq!(
            v["hooks"]["Stop"],
            serde_json::json!([group(vec![])]),
            "只删我们清空的组"
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 状态查询：命令原文按文件顺序；未安装 / 文件不存在为空。
    #[test]
    fn hook_status_lists_commands_in_file_order() {
        let tmp = tmp_dir("status");
        let path = tmp.join("settings.json");
        assert!(
            hook_status(&path, "imagent").unwrap().is_empty(),
            "文件不存在 = 未安装"
        );
        install_hook(&path, EXE, "imagent").unwrap();
        assert_eq!(
            hook_status(&path, "imagent").unwrap(),
            vec![CMD.to_string()]
        );
        assert!(hook_status(&path, "other").unwrap().is_empty(), "别的 --to");
        install_hook(&path, "/opt/x/imagent", "imagent").unwrap();
        assert_eq!(
            hook_status(&path, "imagent").unwrap(),
            vec!["/opt/x/imagent peer-hook --to imagent".to_string()],
            "原位更新后仍恰一条，命令原文 = 当前二进制"
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// scope → 配置路径：user = ~/.claude/settings.json；project = 当前目录下。
    #[test]
    fn settings_path_follows_scope() {
        assert_eq!(
            project_settings_path(Path::new("/work/proj")),
            Path::new("/work/proj/.claude/settings.json")
        );
        assert_eq!(
            settings_path(HookScope::Project).unwrap(),
            std::env::current_dir()
                .unwrap()
                .join(".claude")
                .join("settings.json")
        );
        assert_eq!(
            settings_path(HookScope::User).unwrap(),
            dirs::home_dir()
                .unwrap()
                .join(".claude")
                .join("settings.json")
        );
    }

    /// 命令组装：常规路径原样；含空格的路径整体单引号包裹（识别不受影响）；
    /// peer 名只许字母数字与 `. _ - + @`（钩子命令经 shell 解析，名字里不能有
    /// 空白 / 引号 / 元字符——非零退出会打断 Claude 的收尾）。
    #[test]
    fn hook_command_validates_name_and_quotes_unsafe_exe() {
        assert_eq!(hook_command(EXE, "imagent").unwrap(), CMD);
        assert_eq!(
            hook_command(EXE, "gw-home_2.a+b@c").unwrap(),
            format!("{EXE} peer-hook --to gw-home_2.a+b@c")
        );
        let quoted = hook_command("/Users/My Name/bin/imagent", "imagent").unwrap();
        assert_eq!(
            quoted,
            "'/Users/My Name/bin/imagent' peer-hook --to imagent"
        );
        assert!(
            imagent_core::peer::hook_command_matches(&quoted, "imagent"),
            "引号包裹后仍被识别为本网关条目"
        );
        for bad in [
            "", "my gw", "imagent;", "a$b", "a\"b", "a'b", "a|b", "a\tb", "--x",
        ] {
            assert!(hook_command(EXE, bad).is_err(), "应拒绝 peer 名 {bad:?}");
        }
        assert!(hook_command(EXE, &"n".repeat(65)).is_err(), "超长名字");
        assert!(hook_command("", "imagent").is_err(), "空路径");
        let tmp = tmp_dir("badname");
        assert!(install_hook(&tmp.join("settings.json"), EXE, "my gw").is_err());
        assert!(!tmp.join("settings.json").exists(), "拒绝时不落文件");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// stdin JSON 解析：message 取 last_assistant_message（兼容直接给 message）；
    /// 为空 → 不上报；缺 session_id / 非 JSON → Err；超长正文截到上限内。
    #[test]
    fn parse_hook_input_variants() {
        let ok = parse_hook_input(
            r#"{"session_id":"s-1","cwd":"/work","hook_event_name":"Stop","last_assistant_message":"结论"}"#,
        )
        .unwrap()
        .expect("应上报");
        assert_eq!(
            (
                ok.session_id.as_str(),
                ok.cwd.as_deref(),
                ok.message.as_str()
            ),
            ("s-1", Some("/work"), "结论")
        );
        let compat = parse_hook_input(r#"{"session_id":"s-1","message":"直给"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(
            (compat.message.as_str(), compat.cwd.as_deref()),
            ("直给", None)
        );
        for empty in [
            r#"{"session_id":"s-1","last_assistant_message":""}"#,
            r#"{"session_id":"s-1"}"#,
        ] {
            assert!(
                parse_hook_input(empty).unwrap().is_none(),
                "空 message 不上报：{empty}"
            );
        }
        for bad in [
            r#"{"cwd":"/w","last_assistant_message":"m"}"#,
            r#"{"session_id":"","last_assistant_message":"m"}"#,
            "not json",
            "",
        ] {
            assert!(parse_hook_input(bad).is_err(), "应报错：{bad}");
        }
        let long = "长".repeat(imagent_core::peer::HOOK_MESSAGE_MAX_CHARS + 10);
        let clipped = parse_hook_input(
            &serde_json::json!({"session_id": "s", "last_assistant_message": long}).to_string(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            clipped.message.chars().count(),
            imagent_core::peer::HOOK_MESSAGE_MAX_CHARS
        );
        assert!(clipped
            .message
            .ends_with(imagent_core::peer::HOOK_TRUNCATED_MARK));
    }

    fn write_registry_entry(dir: &Path, pid: u32, name: &str, sock: &Path, entrypoint: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("{pid}.json")),
            serde_json::json!({
                "pid": pid,
                "sessionId": format!("sess-{pid}"),
                "cwd": "/work",
                "startedAt": 1_700_000_000_000i64 + i64::from(pid),
                "kind": "interactive",
                "entrypoint": entrypoint,
                "messagingSocketPath": sock.to_string_lossy(),
                "name": name,
                "status": "idle",
            })
            .to_string(),
        )
        .unwrap();
    }

    fn live_sock(path: &Path) -> tokio::net::UnixListener {
        let _ = std::fs::remove_file(path);
        tokio::net::UnixListener::bind(path).expect("bind live sock")
    }

    /// 找网关：只认 `entrypoint == "imagent"` 且套接字为活；优先名字相等，
    /// 找不到且只有一个网关时用它；多个又对不上名字 → 不猜。
    #[tokio::test]
    async fn find_gateway_entry_prefers_named_then_single() {
        let tmp = tmp_dir("findgw");
        let dir = tmp.join("r");
        let sock = |n: &str| tmp.join(n);
        let _a = live_sock(&sock("a.sock"));
        let _b = live_sock(&sock("b.sock"));
        let _w = live_sock(&sock("w.sock"));
        write_registry_entry(&dir, 1, "gw-a", &sock("a.sock"), "imagent");
        write_registry_entry(&dir, 2, "gw-b", &sock("b.sock"), "imagent");
        write_registry_entry(&dir, 3, "some-window", &sock("w.sock"), "cli");
        write_registry_entry(&dir, 4, "gw-dead", &sock("dead.sock"), "imagent");
        let reg = PeerRegistry::new(dir.clone());

        assert_eq!(
            find_gateway_entry(&reg, "gw-b").await.unwrap().name,
            "gw-b",
            "名字相等的优先"
        );
        assert!(
            find_gateway_entry(&reg, "imagent")
                .await
                .unwrap_err()
                .contains("多个网关"),
            "两个网关、名字都对不上 → 不猜"
        );
        // 只剩一个在线网关：名字对不上也用它（--to 缺省名与实际网关名不一致）。
        std::fs::remove_file(dir.join("1.json")).unwrap();
        assert_eq!(
            find_gateway_entry(&reg, "imagent").await.unwrap().name,
            "gw-b"
        );
        // 没有在线网关：交互窗口与死条目都不算。
        std::fs::remove_file(dir.join("2.json")).unwrap();
        assert!(
            find_gateway_entry(&reg, "imagent")
                .await
                .unwrap_err()
                .contains("没有在线的网关"),
            "交互窗口 / 死套接字的网关条目都不是投递目标"
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 客户端投递端到端（注入 tempdir 注册表；对端是测试监听的「网关」socket）：
    /// 找到网关条目 → 把契约形态的 hook 帧写到它的 socket（一行 JSON + 换行，
    /// 写完半关闭、等对端关闭）。帧字节与网关侧 `parse_frame_line` 消费的组帧
    /// 函数逐字节一致——这是 CLI 客户端与 core 网关之间唯一的帧契约锚点。
    #[tokio::test]
    async fn deliver_writes_contract_frame_to_gateway_socket() {
        let tmp = tmp_dir("deliver");
        let gw_sock = tmp.join("gw.sock");
        let listener = live_sock(&gw_sock);
        write_registry_entry(&tmp.join("r"), 7777, "imagent", &gw_sock, "imagent");
        // 与真实 PeerServer 同款：accept 循环。注册表探活会先连一次（不写数据
        // 即关），只有随后的投递连接才带帧——网关一律不回写，读完即关。
        let gateway = tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.expect("gateway accept");
                let mut buf = Vec::new();
                conn.read_to_end(&mut buf).await.expect("read frame");
                if !buf.is_empty() {
                    return String::from_utf8(buf).expect("utf8");
                }
            }
        });
        let input = HookInput {
            session_id: "sess-42".to_string(),
            cwd: Some("/work".to_string()),
            message: "本轮结论".to_string(),
        };
        deliver(&PeerRegistry::new(tmp.join("r")), "imagent", &input)
            .await
            .expect("deliver");
        let wire = gateway.await.expect("gateway task");
        let frame =
            imagent_core::peer::build_hook_frame_json("Stop", "sess-42", "/work", "本轮结论");
        assert_eq!(
            frame,
            r#"{"type":"hook","hook":"Stop","sessionId":"sess-42","cwd":"/work","message":"本轮结论"}"#,
            "键序固定：type、hook、sessionId、cwd、message"
        );
        assert_eq!(wire, format!("{frame}\n"), "一行 JSON + 换行");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 投递失败一律上抛为原因串（由 run_client 静默吞掉）：网关不在线 / 没有
    /// 网关条目 / socket 写不进去。
    #[tokio::test]
    async fn deliver_reports_failures_as_reasons() {
        let tmp = tmp_dir("delfail");
        let dir = tmp.join("r");
        let reg = PeerRegistry::new(dir.clone());
        let input = HookInput {
            session_id: "s".to_string(),
            cwd: None,
            message: "m".to_string(),
        };
        let err = deliver(&reg, "imagent", &input).await.unwrap_err();
        assert!(err.contains("没有在线的网关"), "{err}");
        write_registry_entry(&dir, 1, "imagent", &tmp.join("dead.sock"), "imagent");
        let err = deliver(&reg, "imagent", &input).await.unwrap_err();
        assert!(
            err.contains("没有在线的网关"),
            "死套接字的网关条目不算在线：{err}"
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
