//! Claude Code peer 协议层（v1.41 网关伪装实例）。
//!
//! 网关伪装成 Claude Code 实例注册为本机 peer（`~/.claude/sessions/<pid>.json`
//! 注册条目 + cc-socks 目录下的 socket），本地所有 claude 窗口即可用原生
//! `SendMessage` 语义给网关发消息（网关推驾驶舱 `peer:<窗口名>` 会话），用户在
//! 驾驶舱回复（网关投回窗口，窗口空闲时自动开新轮处理——claude 原生行为）。
//! 协议为逆向 port（无官方契约，对 Claude Code 2.1.310 双向实测验证；参考
//! pi-claude-link claude-protocol.ts，对 2.1.224 验证），claude 升级若变格式
//! 需跟进本模块薄适配。
//!
//! 模块分四块（均在本文件）：
//! - 帧纯函数（envelope 构造/剥离/转义、user 帧组帧、注册条目 JSON）；
//! - [`PeerRegistry`]：注册表扫描 / socket 探活 / 注册注销 / 孤儿清理；
//! - [`PeerSender`]：出站句柄（dispatch 持有——`peer:` 会话文本与 `/to` 命令
//!   按窗口名投递）；
//! - [`PeerServer`]：UDS server（收帧 → 回调 dispatch 直投驾驶舱）。
//!
//! 入站握手纪律（v1.41.8，硬约束——**不要补回执**）：[`PeerServer`] 收到 user 帧
//! 后**不向入站连接写任何字节**，读到对端 EOF 才关闭连接。在入站连接上回写
//! 数据会让 Claude Code 发送方把这次 SendMessage 判为失败；实证与「不反连
//! 回执」的理由见 [`handle_conn`]。
//!
//! 路径纪律（硬要求）：注册表目录与 socket 路径**一律注入**（[`PeerServerConfig`]
//! / [`PeerSender::new`] 入参），生产由 main 经 [`default_registry_dir`] +
//! socket 目录发现装配，测试用 tempdir——模块内不读 `$HOME`（仅有的两个 home
//! 访问点是 [`default_registry_dir`] 与 [`default_claude_settings_path`]，都只在
//! main / peer-hook 客户端的装配路径上调用）。
//!
//! 安全模型：socket 目录 0700 + socket 文件 0600，同 uid 即本机凭证（与
//! `local` 平台 / 权限审批 socket 同款信任边界）。入站窗口消息**不进**
//! `handle()` 鉴权管线（直接 reply 进驾驶舱）——注册表反查 + 本机 socket
//! 边界即准入。
//!
//! v1.43 本地窗口看板（同文件续两块）：
//! - 窗口快照：[`PeerServer::start`] 内的轮询任务每 3 秒读注册表（只认
//!   [`PeerEntry::is_window`] 的条目，防抖后）→ [`PeerState::apply_snapshot`]
//!   判定「进入 waiting / 跑完一轮」事件 → 回调 main 装配的平台钩子
//!   （[`crate::Platform::on_windows_changed`] / [`crate::Platform::on_window_event`]）。
//!   [`PeerState`] 是共享态：轮询任务、出站句柄 [`PeerSender`]、dispatch 的钩子
//!   上报处理、web 平台（`windows` 帧数据源）四处共用。
//! - 钩子上报：Claude Code 的 Stop 钩子经 `imagent peer-hook` 客户端把本轮结论
//!   作为 hook 帧（[`PeerFrame::Hook`]）写到本网关 socket，同样**不回写**。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::error::CoreError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// peer 会话 conv 前缀（dispatch 出站拦截与入站投递共用的事实源）。
pub const CONV_PREFIX: &str = "peer:";

/// 出站投递失败的 SPA error 帧码（驾驶舱 lastError toast 提示）。
pub const ERR_UNREACHABLE: &str = "peer_unreachable";

/// envelope 标签（claude cross-session message 的包裹标签）。
const ENVELOPE_TAG: &str = "cross-session-message";

/// 入站 JSON 行上限（claude 侧超 1MiB 无换行即断连，同口径防呆）。
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// socket 探活超时（参考实现 socketLive 同值）。
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// 出站 connect + 收尾总预算（claude 发送方等待对端关闭 5s 超时，对齐）。
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// 本网关注册条目的 `entrypoint` 标识——孤儿清理只动本家族条目。
const ENTRYPOINT: &str = "imagent";

/// 错误标签（`CoreError::Platform` 的平台槽位——peer 通道错误统一标 "peer"）。
const ERR_PLATFORM: &str = "peer";

/// 注册表目录覆盖环境变量（v1.43）。本机真实注册表是**所有** Claude Code
/// 实例共用的：集成测试 / 端到端验证需要让被测网关注册到隔离目录，既不污染
/// 真实注册表，也不受真实窗口状态变化的干扰。生产部署不设该变量，行为不变
/// （见 [`default_registry_dir`]）。peer-hook 客户端找网关条目时经同一个访问器
/// 取目录，测试里钩子帧才能投到被测网关。隔离测试不受关注名单持久化的
/// 影响：名单落在网关自己 profile 的 store（`config` 表 [`WATCH_CONFIG_KEY`]），
/// 不在注册表目录里。
pub const REGISTRY_DIR_ENV: &str = "IMAGENT_PEER_REGISTRY_DIR";

/// 「窗口」的注册来源白名单值（`entrypoint`）。为什么是白名单而不是排除法见
/// [`PeerEntry::is_window`]。
pub const WINDOW_ENTRYPOINT: &str = "cli";

/// 后台作业的 `kind`（由父会话拉起、人不直接打字）——不是窗口。
const KIND_BG: &str = "bg";

/// 网关默认注册表目录：`~/.claude/sessions`。home 不可解析 → None（调用方
/// 禁用 peer 通道并 warn，不 panic）。**仅 main / peer-hook 客户端装配用**——
/// 测试一律注入 tempdir。环境变量 [`REGISTRY_DIR_ENV`] 非空时优先返回它
/// （`PathBuf`，不要求已存在——首次启动由 `register` 创建）。
pub fn default_registry_dir() -> Option<PathBuf> {
    if let Some(dir) =
        std::env::var_os(REGISTRY_DIR_ENV).filter(|v| !v.to_string_lossy().trim().is_empty())
    {
        return Some(PathBuf::from(dir));
    }
    Some(dirs::home_dir()?.join(".claude").join("sessions"))
}

/// 网关默认的 Claude Code 用户级配置路径：`~/.claude/settings.json`。只读
/// 探测用（[`hook_installed`]——`windows` 帧的 `hookInstalled`）；home 不可
/// 解析 → None（不探测，`hookInstalled` 恒 false）。与 [`default_registry_dir`]
/// 并列的第二个 home 访问点：**不给它加环境变量覆盖**（安装/卸载/状态的
/// 单测全走注入路径，`hookInstalled` 只需纯函数单测覆盖）。
pub fn default_claude_settings_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude").join("settings.json"))
}

/// 注册表条目缺省显示名（`messagingSocketPath` 缺失的条目直接跳过，见
/// [`PeerRegistry::scan`]）。
const FALLBACK_SOCKS_DIR: &str = "cc-socks";

// ---------------------------------------------------------------------------
// 注册表条目
// ---------------------------------------------------------------------------

/// 注册表条目（`<registry_dir>/<pid>.json` 解析产物，字段与 claude 侧契约
/// 同名；`live` 为探活结果非文件内容）。
#[derive(Debug, Clone)]
pub struct PeerEntry {
    /// 实例进程 pid（注册表文件名来源）。
    pub pid: u32,
    /// 实例 sessionId（claude 侧为 uuid；本网关注册时生成 uuid）。
    pub session_id: Option<String>,
    /// 显示名（`name`；缺省回退 `pid <pid>`，与参考实现一致）。
    pub name: String,
    /// 工作目录（`cwd`）。
    pub cwd: String,
    /// 运行状态（`status`：busy / idle / waiting；缺省 "unknown"）。
    pub status: String,
    /// 实例形态（`kind`：interactive / bg / …）。
    pub kind: Option<String>,
    /// 注册来源（`entrypoint`：cli / sdk-cli / sdk-ts / imagent / …——孤儿清理按
    /// 此界家族，窗口认定见 [`Self::is_window`]）。
    pub entrypoint: Option<String>,
    /// 注册时刻（unix 毫秒；列表按此倒序展示）。
    pub started_at: Option<i64>,
    /// peer socket 路径（`messagingSocketPath`；回程路由键）。
    pub sock: String,
    /// 探活结果（connect 成功 = true）。[`PeerRegistry::scan`] 恒 false，
    /// 探活后由调用方填写。
    pub live: bool,
    /// 等待原因（`waitingFor`；`status == waiting` 时有值，已观察到的取值：
    /// `dialog open` / `input needed` / `sandbox request` / `goal proposal`）。
    pub waiting_for: Option<String>,
    /// 最近一次写状态的时刻（unix 毫秒，`statusUpdatedAt`）。Claude Code 每次
    /// 写 `status` 都会刷新它，且**只在写状态时变**（一轮 25 秒含三次工具调用的
    /// 任务，运行期间它自始至终是开始时的那一个值）——所以「idle 且它变大」
    /// 能判定刚跑完一轮，而把它的变化算进快照变化也不会在一轮运行期间制造
    /// 帧风暴。
    pub status_updated_at: Option<i64>,
}

impl PeerEntry {
    /// 是否算驾驶舱看板里的一个「窗口」（全系统唯一的窗口定义：看板轮询、
    /// 钩子上报的窗口反查、`/peers` / `/to` / 出站投递都按它判定）。
    ///
    /// **窗口 = `entrypoint == "cli"` 且 `kind != "bg"`**，白名单而非排除法。
    /// 实测依据（Claude Code 2.1.310）：无头的 `claude -p` 同样会登记注册表
    /// ——`entrypoint: "sdk-cli"`（CLI 以非交互方式运行时 Claude Code 自己把
    /// `cli` 改写成 `sdk-cli`）、`kind: "interactive"`、**带活套接字**、
    /// `status: "busy"`、名字按工作目录派生，退出才删条目。网关用 `claude-cli`
    /// 引擎跑的每一轮都是这种会话：若按「除本网关外所有带活套接字的条目」
    /// 认窗口，看板每轮都会闪出一行假窗口，这一轮的 Stop 钩子上报还会被当成
    /// 窗口结论重复注入。ACP 子进程是 `sdk-ts`（无套接字，扫描天然跳过）。
    /// `kind` 缺省 `"interactive"`，后台作业是 `"bg"`。没见过的来源宁可不列，
    /// 也不要让网关自己的会话漏进来。
    pub fn is_window(&self) -> bool {
        self.entrypoint.as_deref() == Some(WINDOW_ENTRYPOINT)
            && self.kind.as_deref() != Some(KIND_BG)
    }

    /// 是否本网关家族的注册条目（`entrypoint == "imagent"`：孤儿清理只动这一
    /// 家族；peer-hook 客户端按它找网关）。
    pub fn is_gateway(&self) -> bool {
        self.entrypoint.as_deref() == Some(ENTRYPOINT)
    }
}

/// socket 目录发现（与参考实现 ccSocksDir 同款）：扫描注册表既有条目的
/// `messagingSocketPath` 取 dirname（claude 自家 socket 所在目录——co-locate
/// 才能收到 claude 的投递回执；网关注册条目的 socket 也放这里）；无条目回退
/// `$XDG_RUNTIME_DIR/cc-socks`，再回退 `/tmp/cc-socks`。
///
/// 这里的「回执」指 claude 对**网关出站投递**的送达/暂存回执：按参考实现，
/// 它是 claude 反向连接网关 socket 送来的 control 帧（[`handle_conn`] 忽略
/// 之）——与「入站连接不回写」是两个方向，不矛盾。
async fn discover_socks_dir(registry_dir: &Path) -> PathBuf {
    for entry in PeerRegistry::new(registry_dir.to_path_buf()).scan() {
        if entry.sock.ends_with(".sock") {
            if let Some(dir) = Path::new(&entry.sock).parent() {
                return dir.to_path_buf();
            }
        }
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/tmp".to_string());
    Path::new(&base).join(FALLBACK_SOCKS_DIR)
}

/// `uds:` 前缀剥离（帧 `from` 字段 = `uds:<sock 路径>`，注册表存裸路径）。
fn strip_uds_prefix(from: &str) -> &str {
    from.strip_prefix("uds:").unwrap_or(from)
}

/// socket 文件名去 `.sock` 后缀（入站反查失败时的窗口名兜底）。
fn sock_file_stem(sock: &str) -> Option<String> {
    Path::new(sock)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// 帧纯函数（协议适配薄层，可单测；与 claude-protocol.ts 逐条对齐）
// ---------------------------------------------------------------------------

/// envelope 正文转义：`</` 后紧跟标签名（大小写不敏感）且以 `>` / 空白 / `/`
/// / 串尾收尾时，`</` → `<\/`（防正文里的伪闭合标签截断 claude 侧解析）。
pub fn escape_envelope_body(body: &str) -> String {
    let tag = ENVELOPE_TAG.as_bytes();
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut i = 0usize;
    while i < bytes.len() {
        // "</" + TAG（ASCII 大小写不敏感）+ 边界 → 转义。
        if bytes[i] == b'<'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'/'
            && i + 2 + tag.len() <= bytes.len()
            && bytes[i + 2..i + 2 + tag.len()]
                .iter()
                .zip(tag)
                .all(|(b, t)| b.to_ascii_lowercase() == *t)
        {
            let after = i + 2 + tag.len();
            let boundary = after == bytes.len()
                || bytes[after] == b'>'
                || bytes[after] == b'/'
                || bytes[after].is_ascii_whitespace();
            if boundary {
                out.push_str("<\\/");
                i += 2;
                continue;
            }
        }
        let ch = body[i..].chars().next().expect("char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// envelope 正文反转义（[`escape_envelope_body`] 的逆）：`<\/` + 标签 + 边界
/// → `</`。
pub fn unescape_envelope_body(body: &str) -> String {
    let tag = ENVELOPE_TAG.as_bytes();
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut i = 0usize;
    while i < bytes.len() {
        // "<\/" + TAG（ASCII 大小写不敏感）+ 边界 → 还原。
        if bytes[i] == b'<'
            && i + 3 < bytes.len()
            && bytes[i + 1] == b'\\'
            && bytes[i + 2] == b'/'
            && i + 3 + tag.len() <= bytes.len()
            && bytes[i + 3..i + 3 + tag.len()]
                .iter()
                .zip(tag)
                .all(|(b, t)| b.to_ascii_lowercase() == *t)
        {
            let after = i + 3 + tag.len();
            let boundary = after == bytes.len()
                || bytes[after] == b'>'
                || bytes[after] == b'/'
                || bytes[after].is_ascii_whitespace();
            if boundary {
                out.push_str("</");
                i += 3;
                continue;
            }
        }
        let ch = body[i..].chars().next().expect("char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// envelope 构造：`<cross-session-message from=".." from-name="..">\n正文\n
/// </cross-session-message>`（正文经 [`escape_envelope_body`]；from-name 剥
/// `"<>`，对齐参考实现 buildEnvelope 的属性消毒）。
pub fn build_envelope(from: &str, from_name: &str, body: &str) -> String {
    let clean_name: String = from_name
        .chars()
        .filter(|c| !matches!(c, '"' | '<' | '>'))
        .collect();
    format!(
        "<{ENVELOPE_TAG} from=\"{from}\" from-name=\"{clean_name}\">\n{}\n</{ENVELOPE_TAG}>",
        escape_envelope_body(body)
    )
}

/// envelope 剥离产物：正文 + envelope 属性（无 envelope 时正文 = 原文、属性
/// 全 None——对齐参考实现 stripEnvelope「非 envelope 原样返回」语义）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrippedEnvelope {
    pub body: String,
    pub from: Option<String>,
    pub from_name: Option<String>,
}

/// envelope 剥离：整个 content 形如
/// `<cross-session-message(\s+attr="v")*>\n…\n</cross-session-message>` 才算
/// envelope（属性名限 `[a-z-]+`、值限非 `"`，与参考实现正则同口径）；属性
/// 解析失败 / 无闭合 / 提前截断一律按非 envelope 原文返回（宁可原文透传，
/// 不吞用户正文）。
pub fn strip_envelope(content: &str) -> StrippedEnvelope {
    const FAIL: fn(&str) -> StrippedEnvelope = |c| StrippedEnvelope {
        body: c.to_string(),
        from: None,
        from_name: None,
    };
    let prefix = format!("<{ENVELOPE_TAG}");
    let Some(rest) = content.strip_prefix(&prefix) else {
        return FAIL(content);
    };
    // 属性段：`\s+[a-z-]+="[^"]*"` 重复 ≥0 次（组间空白属于组内前缀；组后必须
    // 紧跟 `>`——正则 `(?:\s+attr)* >` 同口径，残留空白/形态不符即整体失配）。
    let mut idx = 0usize;
    let bytes = rest.as_bytes();
    let mut attrs: Vec<(String, String)> = Vec::new();
    loop {
        // 一次迭代尝试吃一组 `\s+attr="value"`。
        let mut j = idx;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j == idx || j >= bytes.len() || !(bytes[j].is_ascii_lowercase() || bytes[j] == b'-') {
            break; // 无空白（属性段结束，收尾判定 `>`）或空白后非属性（失配）
        }
        let name_start = j;
        while j < bytes.len() && (bytes[j].is_ascii_lowercase() || bytes[j] == b'-') {
            j += 1;
        }
        if j + 1 >= bytes.len() || bytes[j] != b'=' || bytes[j + 1] != b'"' {
            break; // 形态不符：按非 envelope 处理
        }
        j += 2;
        let Some(value_len) = rest[j..].find('"') else {
            break; // 值未闭合
        };
        attrs.push((
            rest[name_start..j - 2].to_string(),
            rest[j..j + value_len].to_string(),
        ));
        idx = j + value_len + 1;
    }
    // 收尾：`>` + `\n` + 正文 + `\n</tag>`（串尾锚定）。
    let tail = rest.as_bytes();
    if idx >= tail.len() || tail[idx] != b'>' {
        return FAIL(content);
    }
    let body_start = idx + 1;
    let Some(after_gt) = rest.get(body_start..).and_then(|s| s.strip_prefix('\n')) else {
        return FAIL(content);
    };
    let closing = format!("\n</{ENVELOPE_TAG}>");
    let Some(body) = after_gt.strip_suffix(&closing) else {
        return FAIL(content);
    };
    let get = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    StrippedEnvelope {
        body: unescape_envelope_body(body),
        from: get("from"),
        from_name: get("from-name"),
    }
}

/// 出站 user 帧组帧（JSON 行）：`{"msgV":1,"msg_id":<uuid>,"type":"user",
/// "priority":"next","from":"uds:<sock>","message":{"role":"user","content":…}}`。
pub fn build_user_frame_json(from_sock: &str, content: &str) -> String {
    serde_json::json!({
        "msgV": 1,
        "msg_id": uuid::Uuid::new_v4().to_string(),
        "type": "user",
        "priority": "next",
        "from": format!("uds:{from_sock}"),
        "message": { "role": "user", "content": content },
    })
    .to_string()
}

/// 网关注册条目 JSON（字段与 claude 契约同名同形态；`procStart` 取自
/// `ps -o lstart=`，取不到则缺席——JSON.stringify undefined 同语义）。
fn registry_entry_json(
    pid: u32,
    session_id: &str,
    cwd: &Path,
    started_at_ms: u64,
    proc_start: Option<&str>,
    sock: &Path,
    name: &str,
) -> String {
    let mut v = serde_json::json!({
        "pid": pid,
        "sessionId": session_id,
        "cwd": cwd.to_string_lossy(),
        "startedAt": started_at_ms,
        "version": ENTRYPOINT,
        "peerProtocol": 1,
        "kind": "interactive",
        "entrypoint": ENTRYPOINT,
        "messagingSocketPath": sock.to_string_lossy(),
        "name": name,
        "nameSource": "derived",
        "status": "idle",
    });
    if let Some(ps) = proc_start {
        v["procStart"] = serde_json::Value::String(ps.to_string());
    }
    v.to_string()
}

/// `ps -o lstart= -p <pid>`（进程启动时刻人读串；claude 注册表契约字段）。
/// 阻塞一次子进程调用（~ms 级、仅注册时一次），失败/非 unix → None。
fn proc_start_of(pid: u32) -> Option<String> {
    #[cfg(unix)]
    {
        std::process::Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

/// Stop 钩子事件名（hook 帧 `hook` 字段 / Claude Code 配置 `hooks.Stop` 的键）。
/// 网关目前只消费这一种 hook 帧，其它事件名一律忽略。
pub const HOOK_EVENT_STOP: &str = "Stop";

/// hook 帧的 `message` 上限（字符）。peer-hook 客户端与网关注入侧共用同一个
/// 上限（客户端截完的总长含截断标记仍 ≤ 它，网关侧的截断只对不规范发送方生效）。
pub const HOOK_MESSAGE_MAX_CHARS: usize = 6000;

/// 正文被截断时追加的标记（用户在驾驶舱里能看出结论不完整）。
pub const HOOK_TRUNCATED_MARK: &str = "…（已截断）";

/// 入站帧解析产物：`type=="user"`（窗口回信）或 `type=="hook"`（v1.43 Stop
/// 钩子上报），其它（control 等）一律忽略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerFrame {
    User {
        msg_id: String,
        /// `from` 原文（`uds:<sock>` 形态；回程路由键）。
        from: Option<String>,
        /// `message.content` 原文（envelope 未剥）。
        content: String,
    },
    /// v1.43 钩子上报（`imagent peer-hook` 客户端写入；键序 `type`、`hook`、
    /// `sessionId`、`cwd`、`message`）。`session_id` / `message` 非空才成立。
    Hook {
        hook: String,
        session_id: String,
        cwd: Option<String>,
        message: String,
    },
    Other,
}

/// JSON 行 → [`PeerFrame`]。畸形 JSON / 缺 `type` → None（调用方跳行）。
pub fn parse_frame_line(line: &str) -> Option<PeerFrame> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let text = |k: &str| v.get(k).and_then(|x| x.as_str());
    match text("type") {
        Some("user") => Some(PeerFrame::User {
            msg_id: v
                .get("msg_id")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string(),
            from: v.get("from").and_then(|f| f.as_str()).map(str::to_string),
            content: v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .to_string(),
        }),
        // v1.43 hook 帧：`hook` / `sessionId` / `message` 须为非空字符串，`cwd`
        // 可空——不合规一律归 Other 忽略（钩子客户端任何失败都静默，网关侧
        // 同样不留半截状态）。
        Some("hook") => match (text("hook"), text("sessionId"), text("message")) {
            (Some(hook), Some(session_id), Some(message))
                if !hook.is_empty() && !session_id.is_empty() && !message.is_empty() =>
            {
                Some(PeerFrame::Hook {
                    hook: hook.to_string(),
                    session_id: session_id.to_string(),
                    cwd: text("cwd").map(str::to_string),
                    message: message.to_string(),
                })
            }
            _ => Some(PeerFrame::Other),
        },
        Some(_) => Some(PeerFrame::Other),
        None => None,
    }
}

/// hook 帧组帧（JSON 行；键序固定 `type`、`hook`、`sessionId`、`cwd`、`message`
/// ——struct 序列化保持声明序，不依赖 `json!` 宏的 Map 键序）。peer-hook
/// 客户端用；`cwd` 缺席时写空串（键不省略）。
pub fn build_hook_frame_json(hook: &str, session_id: &str, cwd: &str, message: &str) -> String {
    #[derive(serde::Serialize)]
    struct HookFrame<'a> {
        r#type: &'static str,
        hook: &'a str,
        #[serde(rename = "sessionId")]
        session_id: &'a str,
        cwd: &'a str,
        message: &'a str,
    }
    serde_json::to_string(&HookFrame {
        r#type: "hook",
        hook,
        session_id,
        cwd,
        message,
    })
    .expect("hook 帧字段均为字符串，序列化不可失败")
}

/// peer-hook 客户端侧的正文截断：总长（**含** [`HOOK_TRUNCATED_MARK`]）不超过
/// [`HOOK_MESSAGE_MAX_CHARS`]。给标记留位的原因：网关侧只对「超过上限」的正文
/// 追加标记——客户端若正好截到上限，网关无从得知它被截过，用户看到的会是
/// 悄悄断在半句的结论。
pub fn clip_hook_message(message: &str) -> String {
    if message.chars().count() <= HOOK_MESSAGE_MAX_CHARS {
        return message.to_string();
    }
    let keep = HOOK_MESSAGE_MAX_CHARS - HOOK_TRUNCATED_MARK.chars().count();
    let mut out: String = message.chars().take(keep).collect();
    out.push_str(HOOK_TRUNCATED_MARK);
    out
}

// ---------------------------------------------------------------------------
// socket 探活 / 出站帧 I/O
// ---------------------------------------------------------------------------

/// socket 探活：connect + 立即关闭（250ms 超时）。**连接成功**才算活
/// （connect 快速失败的 Err 也算死——`timeout(..).is_ok()` 只判外层，会把
/// 立即 ECONNREFUSED 误判为活，孤儿清理将失去判据）。非 unix 恒 false。
pub async fn socket_live(sock: &str) -> bool {
    #[cfg(unix)]
    {
        use tokio::net::UnixStream;
        if sock.is_empty() {
            return false;
        }
        matches!(
            tokio::time::timeout(PROBE_TIMEOUT, UnixStream::connect(sock)).await,
            Ok(Ok(_))
        )
    }
    #[cfg(not(unix))]
    {
        let _ = sock;
        false
    }
}

/// 出站单帧投递：connect 目标 socket → 写 JSON 行 → 半关闭写 → 读到对端关闭
/// （对端写来的字节一律丢弃、不解析——握手的完成信号只是「对端关闭」）。
/// 总预算 [`SEND_TIMEOUT`]（对齐 claude 发送方等待对端关闭的 5s 超时）。
pub async fn send_frame(sock_path: &str, line: &str) -> Result<(), CoreError> {
    #[cfg(unix)]
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        if sock_path.is_empty() {
            return Err(CoreError::Platform(ERR_PLATFORM, "socket 路径为空".into()));
        }
        let connect =
            tokio::time::timeout(SEND_TIMEOUT, tokio::net::UnixStream::connect(sock_path))
                .await
                .map_err(|_| {
                    CoreError::Platform(
                        ERR_PLATFORM,
                        format!("连接 {sock_path} 超时（{SEND_TIMEOUT:?}）"),
                    )
                })?
                .map_err(|e| {
                    CoreError::Platform(ERR_PLATFORM, format!("连接 {sock_path} 失败：{e}"))
                })?;
        let mut conn = connect;
        let io = async {
            conn.write_all(line.as_bytes()).await?;
            conn.write_all(b"\n").await?;
            conn.flush().await?;
            // 半关闭写（claude 窗口按「写完即 shutdown-write」语义收帧）；随后
            // 读到对端关闭——对端写来的字节丢弃不解析；对端不关（异常实现）
            // 由外层超时兜底。
            conn.shutdown().await?;
            let mut sink = Vec::new();
            let _ = conn.read_to_end(&mut sink).await;
            Ok::<(), CoreError>(())
        };
        tokio::time::timeout(SEND_TIMEOUT, io).await.map_err(|_| {
            CoreError::Platform(
                ERR_PLATFORM,
                format!("对端 {sock_path} 未在 {SEND_TIMEOUT:?} 内关闭"),
            )
        })?
    }
    #[cfg(not(unix))]
    {
        let _ = (sock_path, line);
        Err(CoreError::Platform(
            ERR_PLATFORM,
            "peer 出站需要 Unix domain socket，当前平台不支持".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// PeerRegistry：注册表扫描 / 注册注销 / 孤儿清理
// ---------------------------------------------------------------------------

/// 注册表访问器（`<dir>/<pid>.json` 文件族）。路径注入（测试 tempdir / main
/// 默认 [`default_registry_dir`]），模块内不读 `$HOME`。
#[derive(Debug, Clone)]
pub struct PeerRegistry {
    dir: PathBuf,
}

impl PeerRegistry {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 条目文件路径（`<pid>.json`）。
    fn entry_path(&self, pid: u32) -> PathBuf {
        self.dir.join(format!("{pid}.json"))
    }

    /// 扫描全部条目（文件名 `^\d+\.json$`；畸形 JSON / 缺 messagingSocketPath
    /// 的条目跳过——best-effort，单文件损坏不拖垮扫描）。不做探活（`live`
    /// 恒 false）。
    pub fn scan(&self) -> Vec<PeerEntry> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let Some(pid) = name
                .strip_suffix(".json")
                .filter(|stem| !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|stem| stem.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(raw) = std::fs::read(e.path()) else {
                continue;
            };
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) else {
                continue;
            };
            let Some(sock) = v
                .get("messagingSocketPath")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            rows.push(PeerEntry {
                pid,
                session_id: v
                    .get("sessionId")
                    .and_then(|s| s.as_str())
                    .map(str::to_string),
                name: v
                    .get("name")
                    .and_then(|n| n.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("pid {pid}")),
                cwd: v
                    .get("cwd")
                    .and_then(|c| c.as_str())
                    .unwrap_or("?")
                    .to_string(),
                status: v
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                kind: v.get("kind").and_then(|k| k.as_str()).map(str::to_string),
                entrypoint: v
                    .get("entrypoint")
                    .and_then(|k| k.as_str())
                    .map(str::to_string),
                started_at: v.get("startedAt").and_then(|s| s.as_i64()),
                sock: sock.to_string(),
                live: false,
                waiting_for: v
                    .get("waitingFor")
                    .and_then(|w| w.as_str())
                    .map(str::to_string),
                status_updated_at: v.get("statusUpdatedAt").and_then(|s| s.as_i64()),
            });
        }
        rows
    }

    /// 扫描 + 探活，返回活条目（`startedAt` 倒序，对齐参考实现
    /// listClaudeSessions 的展示序）。
    pub async fn live_entries(&self) -> Vec<PeerEntry> {
        let mut rows = self.scan();
        for r in &mut rows {
            r.live = socket_live(&r.sock).await;
        }
        rows.retain(|r| r.live);
        rows.sort_by_key(|r| std::cmp::Reverse(r.started_at.unwrap_or(0)));
        rows
    }

    /// 按 sock 路径反查条目（入站帧 from → 窗口名；**不探活**——发送方刚连上
    /// 本网关，socket 必然活跃）。命中多条取首个（注册表无唯一性约束，同名
    /// sock 极罕见，宁取首个不报错）。
    pub fn find_by_sock(&self, sock: &str) -> Option<PeerEntry> {
        self.scan().into_iter().find(|e| e.sock == sock)
    }

    /// v1.43 按 sessionId 反查**窗口**（Stop 钩子上报 → 窗口名）：
    /// `session_id` 相等、[`PeerEntry::is_window`]、套接字探活为活；多条取
    /// `startedAt` 最新。
    ///
    /// 白名单过滤是这条路径的安全闸：钩子对本机**所有** Claude Code 会话触发
    /// ——网关自己用 `claude-cli` 引擎跑的每一轮（无头 `sdk-cli`，带活套接字）、
    /// ACP 子进程（`sdk-ts`）、后台作业（`bg`）、其它工具调的 `claude -p`、已
    /// 退出的窗口（死套接字）——这些的上报一律查不到窗口，调用方据此丢弃，
    /// 不然网关自己每一轮的结论都会被当成某个窗口的回信注入驾驶舱。
    pub async fn find_by_session_id(&self, session_id: &str) -> Option<PeerEntry> {
        if session_id.is_empty() {
            return None;
        }
        let mut hits: Vec<PeerEntry> = self
            .scan()
            .into_iter()
            .filter(|e| e.session_id.as_deref() == Some(session_id) && e.is_window())
            .collect();
        // 先按 startedAt 倒序再逐条探活：首条活的即结果，不必对死条目逐个探。
        hits.sort_by_key(|e| std::cmp::Reverse(e.started_at.unwrap_or(0)));
        for mut entry in hits {
            if socket_live(&entry.sock).await {
                entry.live = true;
                return Some(entry);
            }
        }
        None
    }

    /// 写入本网关注册条目（`<pid>.json`，目录按需创建）。
    pub fn register(&self, pid: u32, name: &str, cwd: &Path, sock: &Path) -> Result<(), CoreError> {
        std::fs::create_dir_all(&self.dir).map_err(|e| {
            CoreError::Platform(
                ERR_PLATFORM,
                format!("创建注册表目录 {} 失败：{e}", self.dir.display()),
            )
        })?;
        let started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let json = registry_entry_json(
            pid,
            &uuid::Uuid::new_v4().to_string(),
            cwd,
            started_at,
            proc_start_of(pid).as_deref(),
            sock,
            name,
        );
        std::fs::write(self.entry_path(pid), json).map_err(|e| {
            CoreError::Platform(
                ERR_PLATFORM,
                format!("写注册条目 {} 失败：{e}", self.entry_path(pid).display()),
            )
        })
    }

    /// 注销条目 + unlink socket（均 best-effort——停机清理幂等）。
    pub fn deregister(&self, pid: u32, sock: Option<&Path>) {
        let _ = std::fs::remove_file(self.entry_path(pid));
        if let Some(s) = sock {
            let _ = std::fs::remove_file(s);
        }
    }

    /// 孤儿清理：`entrypoint == "imagent"`（本网关家族——崩溃/被 kill 残留的
    /// 注册条目）且 socket 探活失败 → 删条目 + unlink socket。**不碰**其它
    /// entrypoint 条目（交互窗口 / pi 等他家族条目只读）。返回清理条数。
    pub async fn cleanup_orphans(&self) -> usize {
        let orphans: Vec<PeerEntry> = self.scan().into_iter().filter(|e| e.is_gateway()).collect();
        let mut removed = 0usize;
        for e in orphans {
            if socket_live(&e.sock).await {
                continue;
            }
            debug!(
                target: "imagent::core",
                pid = e.pid,
                name = %e.name,
                sock = %e.sock,
                "peer 注册表发现本家族孤儿条目（socket 不在线），清理"
            );
            self.deregister(e.pid, Some(Path::new(&e.sock)));
            removed += 1;
        }
        removed
    }
}

// ---------------------------------------------------------------------------
// 窗口快照与共享状态（v1.43 本地窗口看板）
// ---------------------------------------------------------------------------

/// 「跑完一轮」推送节流窗口：同一窗口在此期间最多推一次（轮询判定与钩子上报
/// 可能对同一轮各判一次）。`Waiting` 不节流——进入即发，状态不变不会再判为
/// 「进入」。
pub const TURN_END_PUSH_THROTTLE: Duration = Duration::from_secs(30);

/// 窗口回信与钩子上报的去重窗口：窗口被驾驶舱驱动时，它的 SendMessage 回信与
/// 钩子的 `last_assistant_message` 可能是同一段话——回信已经到了、`peer_reply`
/// 推送也发过了，钩子上报就不再重复。
pub const INBOUND_DEDUP_WINDOW: Duration = Duration::from_secs(180);

/// 钩子上报的注入节流窗口：窗口内的上报只走推送闸门、不再往会话里注入。
pub const REPORT_INJECT_THROTTLE: Duration = Duration::from_secs(20);

/// 去重比较取正文前缀的长度（字符）。
const INBOUND_DEDUP_PREFIX_CHARS: usize = 200;

/// 关注名单在 store `config` 表里的键（值 = [`encode_watch_list`] 产出的 JSON
/// 字符串数组）。为什么由网关持久化而不是前端：关注是同一个人跨设备的全局
/// 状态——手机和电脑各存一份名单时，A 设备取消关注会被 B 设备的对账立刻补发
/// 回来，等于取消不了。所以名单归网关所有，前端只发 `window_watch`、不存
/// 名单、不对账。
pub const WATCH_CONFIG_KEY: &str = "peer_watched";

/// 关注名单变化回调（同步；[`PeerState`] 在**锁外**调用，入参 = 排序后的完整
/// 名单）。实现方只做非阻塞动作——`PeerServer::start` 装配为「把最新名单交给
/// 单写者落盘泵」。
pub type WatchChangedCallback = Arc<dyn Fn(Vec<String>) + Send + Sync>;

/// 关注名单 → 持久化形态：JSON 字符串数组，排序、去重、去掉空名。
pub fn encode_watch_list(names: &[String]) -> String {
    let sorted: BTreeSet<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|n| !n.is_empty())
        .collect();
    serde_json::to_string(&sorted.into_iter().collect::<Vec<_>>())
        .expect("字符串数组序列化不可失败")
}

/// 持久化形态 → 关注名单。必须是 JSON 字符串数组，否则**整体当空**——非 JSON /
/// 非数组 / 含非字符串项都不 panic（读坏一个 KV 不该让关注集合变成半截，更
/// 不该拖垮 peer 通道启动）。空名项丢弃，结果排序去重。
pub fn decode_watch_list(raw: &str) -> Vec<String> {
    let Ok(items) = serde_json::from_str::<Vec<String>>(raw) else {
        return Vec::new();
    };
    let mut names: Vec<String> = items.into_iter().filter(|n| !n.is_empty()).collect();
    names.sort();
    names.dedup();
    names
}

/// 窗口运行状态（注册表 `status` 的四值投影；字面量同注册表取值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowStatus {
    /// 正在跑一轮。
    Busy,
    /// 空闲。
    Idle,
    /// 卡住等人（对话框 / 输入 / 沙箱请求 / 目标确认）。
    Waiting,
    /// 注册表里的值不是前三者，或缺失。
    Unknown,
}

impl WindowStatus {
    /// 注册表 `status` → 枚举：大小写敏感精确匹配 `busy` / `idle` / `waiting`，
    /// 其余一律 `Unknown`。
    pub fn from_registry(status: &str) -> Self {
        match status {
            "busy" => Self::Busy,
            "idle" => Self::Idle,
            "waiting" => Self::Waiting,
            _ => Self::Unknown,
        }
    }
}

/// 看板里的一个窗口（`windows` 帧条目的数据源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub name: String,
    pub status: WindowStatus,
    /// 等待原因（`status == waiting` 时有值）。
    pub waiting_for: Option<String>,
    pub cwd: String,
    /// 注册时刻（unix 毫秒，`startedAt`）。
    pub started_at: Option<i64>,
    /// 最近一次写状态的时刻（unix 毫秒，`statusUpdatedAt`）。
    pub status_updated_at: Option<i64>,
    /// 当前是否被关注：**跨设备的全局状态**（任一设备点星标即全局生效），由
    /// 网关持久化在 store `config` 表的 [`WATCH_CONFIG_KEY`]。[`PeerState`]
    /// 回填——[`Self::from_entry`] 恒置 false。
    pub watched: bool,
}

impl WindowInfo {
    /// 注册表条目 → 窗口（`watched` 恒 false，关注态由 [`PeerState`] 回填）。
    pub fn from_entry(entry: &PeerEntry) -> Self {
        Self {
            name: entry.name.clone(),
            status: WindowStatus::from_registry(&entry.status),
            waiting_for: entry.waiting_for.clone(),
            cwd: entry.cwd.clone(),
            started_at: entry.started_at,
            status_updated_at: entry.status_updated_at,
            watched: false,
        }
    }
}

/// 快照排序（`windows` 帧字节可复现的前提）：`startedAt` 降序（缺失排最后），
/// 同值按 `name` 升序。[`PeerState::apply_snapshot`] 内部恒排——`latest` 只经
/// 它写入，于是无论谁来喂快照，帧序都是这一种。
pub fn sort_windows(windows: &mut [WindowInfo]) {
    windows.sort_by(|a, b| {
        let by_start = match (a.started_at, b.started_at) {
            (Some(x), Some(y)) => y.cmp(&x),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        by_start.then_with(|| a.name.cmp(&b.name))
    });
}

/// 窗口状态转换里值得通知用户的那几种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowEventKind {
    /// 跑完一轮（`busy → idle`，或 `idle → idle` 且 `statusUpdatedAt` 变大）。
    TurnEnd,
    /// 进入 waiting（卡住等人）。
    Waiting,
}

/// 一次快照判定出的窗口事件（[`PeerState::apply_snapshot`] 产物）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowEvent {
    pub name: String,
    pub kind: WindowEventKind,
    /// `Waiting` 事件的等待原因；`TurnEnd` 恒 None。
    pub waiting_for: Option<String>,
    /// 是否应推送：`Waiting` 恒 true（所有窗口都推——窗口卡住等人紧急且低频）；
    /// `TurnEnd` 仅被关注或本次运行里驾驶舱给它发过消息的窗口，且过了
    /// [`TURN_END_PUSH_THROTTLE`]。
    pub push: bool,
}

/// 窗口看板的共享状态（`Arc` 共享：PeerServer 的轮询任务、出站句柄
/// [`PeerSender`]、Dispatcher 的钩子上报处理、web 平台四处共用）。内部可变性
/// （`parking_lot::Mutex`），方法一律取 `&self`；时间相关的判定全部 `now`
/// 注入（单测拨 `Instant` 推进，不 sleep）。其中**只有关注名单是持久化的**：
/// 它是跨设备的全局状态，归网关所有、落在 store `config` 表的
/// [`WATCH_CONFIG_KEY`]（见 [`Self::set_watched`]）；其余——对话记录、各类
/// 节流 / 去重时间戳、最近快照——都是本次运行的内存态，重启清空。
pub struct PeerState {
    inner: parking_lot::Mutex<PeerStateInner>,
    /// 关注名单变化回调（[`Self::set_watched`] 改变了集合才调）。`PeerServer::start`
    /// 在加载完持久化名单**之后**才装上——加载本身不触发回写。
    on_watch_changed: parking_lot::Mutex<Option<WatchChangedCallback>>,
}

#[derive(Default)]
struct PeerStateInner {
    /// 最近一次快照（已排序；`windows` 帧的数据源）。
    latest: Vec<WindowInfo>,
    /// 上一次轮询到的状态，算转换用。
    prev: HashMap<String, PrevWindow>,
    /// 被关注的窗口名（全局：所有设备共用这一份；持久化在 `config` 表
    /// [`WATCH_CONFIG_KEY`]，`PeerServer::start` 先加载再开轮询）。
    watched: HashSet<String>,
    /// 本次网关运行里驾驶舱给它发过消息的窗口（`send_to_window` 成功即登记）。
    contacted: HashSet<String>,
    /// 「跑完一轮」推送节流：窗口名 → 上次推送时刻。
    last_turn_end_push: HashMap<String, Instant>,
    /// 窗口最近一次经 peer 通道发来的回信（归一化正文前缀，时刻），钩子上报去重用。
    last_inbound: HashMap<String, (String, Instant)>,
    /// 钩子上报注入节流：窗口名 → 上次注入时刻。
    last_report_inject: HashMap<String, Instant>,
    hook_installed: bool,
    /// 已建基线（[`PeerState::apply_snapshot`] 被调用过至少一次）。
    primed: bool,
}

/// 上一次轮询到的单窗口状态。多存一个 `started_at`：同名窗口重启（新进程、
/// 新的注册时刻）是另一个实例，没有「上一次」可言——不区分的话，新实例一
/// 上来就是 idle 且时间戳更大，会被当成「刚跑完一轮」推一条。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrevWindow {
    status: WindowStatus,
    status_updated_at: Option<i64>,
    started_at: Option<i64>,
}

impl PrevWindow {
    /// 是否同一个实例（注册时刻缺失时无从比较，按同实例处理）。
    fn is_same_instance(&self, now: &WindowInfo) -> bool {
        self.started_at.is_none() || self.started_at == now.started_at
    }
}

fn prev_map(windows: &[WindowInfo]) -> HashMap<String, PrevWindow> {
    windows
        .iter()
        .map(|w| {
            (
                w.name.clone(),
                PrevWindow {
                    status: w.status,
                    status_updated_at: w.status_updated_at,
                    started_at: w.started_at,
                },
            )
        })
        .collect()
}

/// 快照对比键（契约钉死的集合）：每个窗口的 `name / status / waitingFor /
/// watched / statusUpdatedAt`，以及窗口集合本身。两份快照都已按
/// [`sort_windows`] 排好，同下标即同窗口。`cwd` / `startedAt` 单独变化不算——
/// 它们照样更新进 `latest`，只是不为此单独推一帧。
fn same_snapshot(a: &[WindowInfo], b: &[WindowInfo]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.name == y.name
                && x.status == y.status
                && x.waiting_for == y.waiting_for
                && x.watched == y.watched
                && x.status_updated_at == y.status_updated_at
        })
}

/// 正文归一化（钩子上报去重比较用）：trim + 任意空白序列折叠为单空格。
pub fn normalize_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 去重比较键：归一化后的前 [`INBOUND_DEDUP_PREFIX_CHARS`] 字符。
fn dedup_key(text: &str) -> String {
    normalize_ws(text)
        .chars()
        .take(INBOUND_DEDUP_PREFIX_CHARS)
        .collect()
}

/// 互为前缀：钩子的 `last_assistant_message` 与窗口经 SendMessage 发来的回信
/// 可能一长一短（窗口只把结论段发了回来），相同取前缀比较而不是全等。
fn prefix_overlap(a: &str, b: &str) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

impl Default for PeerState {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerState {
    pub fn new() -> Self {
        Self {
            inner: parking_lot::Mutex::new(PeerStateInner::default()),
            on_watch_changed: parking_lot::Mutex::new(None),
        }
    }

    /// 安装 / 卸下关注名单变化回调。`PeerServer::start` 在加载完持久化名单之后
    /// 才调用：若在加载前就装上，加载的每个名字都会触发一次回写，几次并发写
    /// 的完成顺序不定，最后落盘的可能是半截名单。
    pub fn set_watch_changed_callback(&self, callback: Option<WatchChangedCallback>) {
        *self.on_watch_changed.lock() = callback;
    }

    /// 关注 / 取消关注一个窗口（驾驶舱 `window_watch` 帧）。关注态按窗口名
    /// 保留——窗口下线后再回来仍是关注的。集合是跨设备的全局状态，**变了**
    /// 才经回调落盘（入参 = 排序后的完整名单）；设成已有的值不算变化、不回调。
    /// 回调在锁外调用——实现方可以同步再读 `PeerState`。
    pub fn set_watched(&self, name: &str, on: bool) {
        let changed = {
            let mut inner = self.inner.lock();
            let before = inner.watched.len();
            if on {
                inner.watched.insert(name.to_string());
            } else {
                inner.watched.remove(name);
            }
            inner.watched.len() != before
        };
        if !changed {
            return;
        }
        let callback = self.on_watch_changed.lock().clone();
        if let Some(callback) = callback {
            callback(self.watched_names());
        }
    }

    pub fn is_watched(&self, name: &str) -> bool {
        self.inner.lock().watched.contains(name)
    }

    /// 排序后的完整关注名单（回调载荷 = 持久化内容）。
    pub fn watched_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.inner.lock().watched.iter().cloned().collect();
        names.sort();
        names
    }

    /// 登记驾驶舱给该窗口发过消息（`send_to_window` 成功后调用）。
    pub fn mark_contacted(&self, name: &str) {
        self.inner.lock().contacted.insert(name.to_string());
    }

    /// 最近一次快照（已排序，`watched` 按当前关注名单回填——`window_watch`
    /// 之后、下一轮轮询之前读到的是新关注态）。未建基线时为空。
    pub fn windows(&self) -> Vec<WindowInfo> {
        let inner = self.inner.lock();
        let mut out = inner.latest.clone();
        for w in &mut out {
            w.watched = inner.watched.contains(&w.name);
        }
        out
    }

    /// 更新钩子安装态；返回是否发生变化（变化时轮询任务补推一帧 `windows`，
    /// 用户装完钩子不必等到下一次快照变化才看到 `hookInstalled` 翻转）。
    pub fn set_hook_installed(&self, installed: bool) -> bool {
        let mut inner = self.inner.lock();
        if inner.hook_installed == installed {
            return false;
        }
        inner.hook_installed = installed;
        true
    }

    pub fn hook_installed(&self) -> bool {
        self.inner.lock().hook_installed
    }

    /// 喂入一轮窗口快照，返回（快照是否变化，本轮判定出的事件）。
    ///
    /// - **首个快照只建基线、不产生任何事件**：没有「上一次」就没有转换可言。
    ///   否则网关每次重启（升级、崩溃拉起）都会把当时正卡在等待的窗口各推一条
    ///   「窗口在等你」——本机实测就有一个窗口的对话框挂了四天，每次发版都推
    ///   一次纯属打扰。之后运行期间新出现的窗口若一上来就是 `waiting`，仍按
    ///   「进入 waiting」处理（它对用户是新信息）。首个快照的 `changed` 恒
    ///   true（从无到有）。
    /// - **进入 waiting**：上一次不是 `waiting`、这次是 → `Waiting`，`push`
    ///   恒 true。
    /// - **跑完一轮**：`busy → idle`，或 `idle → idle` 且 `statusUpdatedAt`
    ///   变大 → `TurnEnd`，`push` = 被关注或对话过，且过了
    ///   [`TURN_END_PUSH_THROTTLE`]（返回 true 即记下本次时刻）。
    /// - 其它转换不产生事件：进入 busy、窗口下线，以及 **`waiting → idle`**
    ///   （对话框是人在终端里处理掉的，人就在电脑前；若处理后还有一轮，走
    ///   `busy → idle` 或钩子上报）。
    pub fn apply_snapshot(&self, windows: &[WindowInfo], now: Instant) -> (bool, Vec<WindowEvent>) {
        let mut inner = self.inner.lock();
        let mut current: Vec<WindowInfo> = windows.to_vec();
        for w in &mut current {
            w.watched = inner.watched.contains(&w.name);
        }
        sort_windows(&mut current);

        if !inner.primed {
            inner.prev = prev_map(&current);
            inner.latest = current;
            inner.primed = true;
            return (true, Vec::new());
        }

        let changed = !same_snapshot(&inner.latest, &current);
        let mut events = Vec::new();
        for w in &current {
            // 上一次的状态；运行期间新出现的窗口（含同名但已重启的实例）没有。
            let prev = inner
                .prev
                .get(&w.name)
                .copied()
                .filter(|p| p.is_same_instance(w));
            if w.status == WindowStatus::Waiting
                && prev.is_none_or(|p| p.status != WindowStatus::Waiting)
            {
                events.push(WindowEvent {
                    name: w.name.clone(),
                    kind: WindowEventKind::Waiting,
                    waiting_for: w.waiting_for.clone(),
                    push: true,
                });
                continue;
            }
            let Some(prev) = prev else {
                continue;
            };
            let turn_end = matches!(
                (prev.status, w.status),
                (WindowStatus::Busy, WindowStatus::Idle)
            ) || (prev.status == WindowStatus::Idle
                && w.status == WindowStatus::Idle
                && w.status_updated_at > prev.status_updated_at);
            if turn_end {
                let push = inner.turn_end_push_allowed(&w.name, now);
                events.push(WindowEvent {
                    name: w.name.clone(),
                    kind: WindowEventKind::TurnEnd,
                    waiting_for: None,
                    push,
                });
            }
        }
        inner.prev = prev_map(&current);
        inner.latest = current;
        (changed, events)
    }

    /// 「跑完一轮」是否该推送：窗口被关注或对话过，且距上次推送已过
    /// [`TURN_END_PUSH_THROTTLE`]。返回 true 即记下本次时刻。钩子上报与轮询
    /// 判定共用它，所以同一轮两条路径至多推一次。
    pub fn should_push_turn_end(&self, name: &str, now: Instant) -> bool {
        self.inner.lock().turn_end_push_allowed(name, now)
    }

    /// 登记窗口经 peer 通道发来的一条回信（`handle_peer_inbound` 在投递前调用）。
    pub fn note_inbound(&self, name: &str, text: &str, now: Instant) {
        self.inner
            .lock()
            .last_inbound
            .insert(name.to_string(), (dedup_key(text), now));
    }

    /// 钩子上报是否与该窗口 [`INBOUND_DEDUP_WINDOW`] 内最近一条 peer 回信重复
    /// （归一化后互为前缀，取前 [`INBOUND_DEDUP_PREFIX_CHARS`] 字符比较）。
    pub fn inbound_duplicate(&self, name: &str, text: &str, now: Instant) -> bool {
        let inner = self.inner.lock();
        let Some((seen, at)) = inner.last_inbound.get(name) else {
            return false;
        };
        now.saturating_duration_since(*at) <= INBOUND_DEDUP_WINDOW
            && prefix_overlap(&dedup_key(text), seen)
    }

    /// 钩子上报可否注入会话（每个窗口 [`REPORT_INJECT_THROTTLE`] 内最多一次）。
    /// 纯判定，放行后由调用方 [`Self::note_report_injected`] 记时刻。
    pub fn report_inject_allowed(&self, name: &str, now: Instant) -> bool {
        let inner = self.inner.lock();
        inner
            .last_report_inject
            .get(name)
            .is_none_or(|last| now.saturating_duration_since(*last) >= REPORT_INJECT_THROTTLE)
    }

    pub fn note_report_injected(&self, name: &str, now: Instant) {
        self.inner
            .lock()
            .last_report_inject
            .insert(name.to_string(), now);
    }
}

impl PeerStateInner {
    fn turn_end_push_allowed(&mut self, name: &str, now: Instant) -> bool {
        if !self.watched.contains(name) && !self.contacted.contains(name) {
            return false;
        }
        if let Some(last) = self.last_turn_end_push.get(name) {
            if now.saturating_duration_since(*last) < TURN_END_PUSH_THROTTLE {
                return false;
            }
        }
        self.last_turn_end_push.insert(name.to_string(), now);
        true
    }
}

/// 本机 Claude Code 用户级配置里是否装了指向网关 `<peer_name>` 的 Stop 钩子
/// （`hooks.Stop[].hooks[]` 里有 [`hook_entry_matches`] 的条目）。纯函数、路径
/// 注入；文件不存在 / `None` / 解析失败 → false（探测是只读的 best-effort，
/// 配置文件写坏的瞬间不该让看板报错）。
pub fn hook_installed(settings_path: Option<&Path>, peer_name: &str) -> bool {
    let Some(path) = settings_path else {
        return false;
    };
    let Ok(raw) = std::fs::read(path) else {
        return false;
    };
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return false;
    };
    let installed = stop_hook_entries(&root).any(|hook| hook_entry_matches(hook, peer_name));
    installed
}

/// 遍历配置里的全部 Stop 钩子条目：`hooks.Stop[].hooks[]`（`hooks.Stop` 是
/// 「组」数组，每组一个内层 `hooks` 数组；形状不符的节点跳过）。
pub fn stop_hook_entries(root: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
    root.get("hooks")
        .and_then(|h| h.get(HOOK_EVENT_STOP))
        .and_then(|s| s.as_array())
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks"))
        .filter_map(|hooks| hooks.as_array())
        .flatten()
}

/// Stop 钩子条目（`hooks.Stop[].hooks[]` 的一个元素）是否是「网关 `<to>` 的
/// peer-hook 客户端」：`type == "command"` 且 [`hook_command_matches`]。网关的
/// `hookInstalled` 探测与 CLI 的安装 / 卸载 / 状态共用这一个定义。
pub fn hook_entry_matches(hook: &serde_json::Value, to: &str) -> bool {
    hook.get("type").and_then(|t| t.as_str()) == Some("command")
        && hook
            .get("command")
            .and_then(|c| c.as_str())
            .is_some_and(|c| hook_command_matches(c, to))
}

/// 钩子命令是否指向网关 `<to>`：按空白切词后，存在 `peer-hook` 词（前面必须
/// 还有可执行文件那个词——契约写作「含 `" peer-hook "`」），其后是 `--to <to>`
/// （或 `--to=<to>`）。`--to` 按整词匹配而不是子串：子串匹配下 `--to imagent-2`
/// 也含 `--to imagent`，卸载 `imagent` 的钩子会把另一台网关的条目一起删掉。
pub fn hook_command_matches(command: &str, to: &str) -> bool {
    if to.is_empty() {
        return false;
    }
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let Some(i) = tokens.iter().position(|t| *t == "peer-hook") else {
        return false;
    };
    if i == 0 {
        return false;
    }
    match tokens.get(i + 1) {
        Some(flag) if *flag == "--to" => tokens.get(i + 2) == Some(&to),
        Some(flag) if flag.strip_prefix("--to=") == Some(to) => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// PeerSender：出站句柄（dispatch 持有）
// ---------------------------------------------------------------------------

/// 出站投递句柄：按窗口名解析注册表（探活过滤 + 只认窗口条目）→ envelope
/// user 帧投递。dispatch 的 `peer:` 会话文本路由与 `/to` / `/peers` 命令经此
/// 消费。v1.43 起持有共享态 [`PeerState`]（投递成功登记 contacted；钩子上报
/// 处理经它做去重 / 节流 / 推送闸门）。
#[derive(Clone)]
pub struct PeerSender {
    registry: PeerRegistry,
    own_sock: PathBuf,
    own_name: String,
    state: Arc<PeerState>,
}

impl PeerSender {
    /// 构造（路径注入；测试直接 new）。自建一份**私有**的共享态——签名与
    /// 行为不变，投递登记 / 上报节流照常生效，只是没有轮询任务与 web 平台
    /// 与它共享。生产由 `PeerServer::start` 经 [`Self::with_state`] 产出。
    pub fn new(registry_dir: PathBuf, own_sock: PathBuf, own_name: String) -> Self {
        Self::with_state(registry_dir, own_sock, own_name, Arc::new(PeerState::new()))
    }

    /// 带共享态构造（`PeerServer::start` 用：与轮询任务、web 平台同一份
    /// `PeerState`）。
    pub fn with_state(
        registry_dir: PathBuf,
        own_sock: PathBuf,
        own_name: String,
        state: Arc<PeerState>,
    ) -> Self {
        Self {
            registry: PeerRegistry::new(registry_dir),
            own_sock,
            own_name,
            state,
        }
    }

    pub fn registry(&self) -> &PeerRegistry {
        &self.registry
    }

    /// 本网关自身 sock 路径（出站帧 from 字段与测试断言用）。
    pub fn own_sock(&self) -> &Path {
        &self.own_sock
    }

    /// 共享态（看板快照 / 关注名单 / 上报去重与节流）。
    pub fn state(&self) -> &Arc<PeerState> {
        &self.state
    }

    /// 在线窗口列表（探活过滤 + 只认 [`PeerEntry::is_window`] 的条目；
    /// `startedAt` 倒序）。
    ///
    /// v1.43 起是白名单：本网关自己（`entrypoint == "imagent"`）自然被排除，
    /// `own_sock` 过滤保留作双保险；网关用 `claude-cli` 引擎跑的每一轮都是无头
    /// 会话（`sdk-cli`、带活套接字、`status: busy`），此前会被列成一个「窗口」
    /// ——`/peers` 里每轮闪出一行假窗口、`/to` 前缀匹配也可能撞上它。
    pub async fn list_windows(&self) -> Vec<PeerEntry> {
        let own = self.own_sock.to_string_lossy().into_owned();
        self.registry
            .live_entries()
            .await
            .into_iter()
            .filter(|e| e.is_window() && e.sock != own)
            .collect()
    }

    /// 按窗口名解析在线条目（精确名优先；唯一前缀次之——对齐参考实现
    /// resolveTarget 的解析序），**只解析不投递**（`/to` 无正文的建立会话
    /// 语义复用）。失败给**可行动**错误文案（含当前在线名单，`/to` 回执直接
    /// 展示）。
    pub async fn resolve_window(&self, name: &str) -> Result<PeerEntry, CoreError> {
        let live = self.list_windows().await;
        let target = if let Some(hit) = live.iter().find(|e| e.name == name) {
            Some(hit.clone())
        } else {
            let pfx: Vec<&PeerEntry> = live.iter().filter(|e| e.name.starts_with(name)).collect();
            match pfx.len() {
                1 => Some(pfx[0].clone()),
                0 => None,
                _ => {
                    return Err(CoreError::Platform(
                        ERR_PLATFORM,
                        format!(
                            "窗口名「{name}」匹配到多个：{}（请用完整名）",
                            pfx.iter()
                                .map(|e| e.name.as_str())
                                .collect::<Vec<_>>()
                                .join("、")
                        ),
                    ))
                }
            }
        };
        let Some(entry) = target else {
            let hint = if live.is_empty() {
                "当前无在线的 Claude Code 窗口（/peers 可查看）".to_string()
            } else {
                format!(
                    "无在线窗口匹配「{name}」（当前在线：{}）",
                    live.iter()
                        .map(|e| e.name.as_str())
                        .collect::<Vec<_>>()
                        .join("、")
                )
            };
            return Err(CoreError::Platform(ERR_PLATFORM, hint));
        };
        Ok(entry)
    }

    /// 按窗口名投递（解析复用 [`Self::resolve_window`]）→ envelope user 帧投递。
    /// 成功即登记 contacted（此后该窗口跑完一轮都会推送——用户主动找过它，
    /// 它的结论对用户不再是噪音）。
    pub async fn send_to_window(&self, name: &str, body: &str) -> Result<(), CoreError> {
        let entry = self.resolve_window(name).await?;
        let own = self.own_sock.to_string_lossy().into_owned();
        let content = build_envelope(&format!("uds:{own}"), &self.own_name, body);
        send_frame(&entry.sock, &build_user_frame_json(&own, &content)).await?;
        // 登记解析后的规范窗口名（前缀匹配也归一到条目全名，与 peer: 会话同键）。
        self.state.mark_contacted(&entry.name);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PeerServer：UDS server（注册 → listen → 收帧循环）
// ---------------------------------------------------------------------------

/// 入站 user 帧回调（main 装配：`(窗口名, 正文)` → Dispatcher 直投驾驶舱
/// `peer:<窗口名>` 会话）。窗口名已由 server 解析完（注册表反查 → envelope
/// from-name → sock 文件名兜底，见 [`handle_conn`]）。
pub type PeerInboundCallback =
    Arc<dyn Fn(String, String) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// v1.43 hook 帧回调（main 装配：`(session_id, message)` → Dispatcher 的
/// `handle_peer_report`——按 session 反查窗口、去重后注入 `peer:` 会话并走推送
/// 闸门）。与 [`PeerInboundCallback`] 同构。
pub type PeerReportCallback =
    Arc<dyn Fn(String, String) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// v1.43 窗口快照回调（轮询任务每有变化 / 事件调一次：`(窗口快照, 事件)`）。
/// **同步**——实现方只做非阻塞动作（同步钩子直接调；async 的逐个 spawn），
/// 不得在回调里 await / IO，否则轮询任务被拖住。
pub type PeerWindowsCallback = Arc<dyn Fn(Vec<WindowInfo>, Vec<WindowEvent>) + Send + Sync>;

/// [`PeerServer::start`] 参数（路径全注入，测试 tempdir / main 生产路径）。
pub struct PeerServerConfig {
    /// 注册表目录（`~/.claude/sessions`；测试注入 tempdir）。
    pub registry_dir: PathBuf,
    /// 本网关 peer 名（注册条目 `name`——窗口侧看到的对端名；出站帧 envelope
    /// 的 from-name 同用它；也是 `hookInstalled` 探测的 `--to <名>`）。
    pub name: String,
    /// 注册条目 cwd（config.default_workdir）。
    pub cwd: PathBuf,
    /// Claude Code 用户级配置 `settings.json` 路径（只读探测 Stop 钩子是否已装
    /// ——`windows` 帧的 `hookInstalled`）。main 传 [`default_claude_settings_path`]；
    /// `None` = 不探测（恒 false），测试注入 tempdir 路径。
    pub settings_path: Option<PathBuf>,
    /// 窗口轮询间隔。`None` = 不启动轮询（测试用——只验收帧路径时不让后台
    /// 任务反复读注册表）；main 传 3 秒。
    pub watch_interval: Option<Duration>,
    /// 关注名单的持久化落点（store `config` 表的 [`WATCH_CONFIG_KEY`]）。main 传
    /// `Some(store.clone())`；`None` = 不持久化（名单只在内存里，重启清空——
    /// 测试用）。启动时先从这里加载名单、再开轮询；之后名单一变就落盘。
    pub watch_store: Option<Arc<imagent_store::Store>>,
    /// 停机信号（main 传 dispatcher token——drain 窗口内完成注销）。
    pub shutdown: CancellationToken,
    /// 入站回调。
    pub on_user: PeerInboundCallback,
    /// hook 帧回调。
    pub on_report: PeerReportCallback,
    /// 窗口快照回调。
    pub on_windows: PeerWindowsCallback,
}

/// peer server 句柄：`start` 后台跑 accept 循环（与窗口轮询任务）；`shutdown`
/// 幂等注销（删注册条目 + unlink socket）。
pub struct PeerServer {
    registry: PeerRegistry,
    sender: Arc<PeerSender>,
    /// 窗口看板共享态（与 `sender`、轮询任务、web 平台同一份）。
    state: Arc<PeerState>,
    cleanup: Arc<ServerCleanup>,
    task: tokio::task::JoinHandle<()>,
    /// 窗口轮询任务（`watch_interval: None` 时无）。
    watch_task: Option<tokio::task::JoinHandle<()>>,
    /// 关注名单落盘泵（`watch_store: None` 时无）。停机时等它把最后一次变化
    /// 写完再走——用 `Mutex<Option<..>>` 是因为 `shutdown(&self)` 要拿走句柄
    /// 去 await。
    persist_task: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// 注销一次性守卫（accept 循环退出与显式 shutdown 两条路收敛，幂等）。
struct ServerCleanup {
    registry: PeerRegistry,
    pid: u32,
    sock: PathBuf,
    done: AtomicBool,
}

impl ServerCleanup {
    fn run(&self) {
        if self.done.swap(true, Ordering::SeqCst) {
            return;
        }
        self.registry.deregister(self.pid, Some(&self.sock));
        info!(target: "imagent::core", "peer 注册已注销（注册条目与 socket 已清理）");
    }
}

impl PeerServer {
    /// 启动：孤儿清理 → socket 目录发现/创建 → bind → 注册 → accept 循环
    /// （后台 task，随 `config.shutdown` 退出并自注销）。bind/注册失败返回
    /// Err（调用方 fail-open 降级：peer 通道不可用不影响网关主链路）。
    #[cfg(unix)]
    pub async fn start(config: PeerServerConfig) -> Result<Self, CoreError> {
        let registry = PeerRegistry::new(config.registry_dir);
        // ① 孤儿清理（本家族崩溃残留；best-effort，失败不影响启动）。
        let orphans = registry.cleanup_orphans().await;
        if orphans > 0 {
            info!(target: "imagent::core", orphans, "peer 注册表孤儿清理完成");
        }
        // ② socket 目录：发现（注册表既有条目 dirname）→ 建目录 0700。
        let sock_dir = discover_socks_dir(registry.dir()).await;
        std::fs::create_dir_all(&sock_dir).map_err(|e| {
            CoreError::Platform(
                ERR_PLATFORM,
                format!("创建 peer socket 目录 {} 失败：{e}", sock_dir.display()),
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&sock_dir, std::fs::Permissions::from_mode(0o700));
        }
        let pid = std::process::id();
        let sock_path = sock_dir.join(format!("{pid}.sock"));
        let _ = std::fs::remove_file(&sock_path); // 残留清理（bind 前置）
                                                  // ③ bind + chmod 0600。
        let listener = tokio::net::UnixListener::bind(&sock_path).map_err(|e| {
            CoreError::Platform(
                ERR_PLATFORM,
                format!("bind peer socket {} 失败：{e}", sock_path.display()),
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600));
        }
        // ④ 注册（写 <pid>.json）。
        registry.register(pid, &config.name, &config.cwd, &sock_path)?;
        info!(
            target: "imagent::core",
            pid,
            name = %config.name,
            sock = %sock_path.display(),
            registry = %registry.dir().display(),
            "已注册为 Claude Code peer（本地窗口可 SendMessage 到网关）"
        );
        // v1.43 共享态：与出站句柄、窗口轮询任务、web 平台（`windows` 帧）同一份。
        // hookInstalled 起始值在此同步算一次（小文件）——即便不启动轮询
        // （watch_interval = None），windows_query 的应答也是对的。
        let state = Arc::new(PeerState::new());
        // 关注名单（跨设备的全局状态）：**先加载、再开轮询**——轮询的首个快照
        // 里 `watched` 就得是持久化后的真值，否则重启后第一帧会把已关注的窗口
        // 报成未关注（随后才翻转）。读失败 / 畸形只 warn、按空集继续：一个 KV
        // 读失败不该让 peer 通道起不来。
        load_watch_list(&state, config.watch_store.as_deref()).await;
        state.set_hook_installed(hook_installed(
            config.settings_path.as_deref(),
            &config.name,
        ));
        // 变更即落盘：加载完才装回调（加载不触发回写）。回调只把最新名单交给
        // 单写者泵——不直接 spawn 写库：两次快速连点会并发两次写，完成顺序
        // 不定，后完成的旧名单会盖掉新名单；单写者 + 「最新值优先」天然保序，
        // 还把连点合并成一次写。
        let persist_task = config.watch_store.clone().map(|store| {
            let (watch_tx, watch_rx) = tokio::sync::watch::channel(state.watched_names());
            state.set_watch_changed_callback(Some(Arc::new(move |names: Vec<String>| {
                // send 失败 = 泵已退出（停机收尾），此刻没有也不需要落盘方。
                let _ = watch_tx.send(names);
            })));
            tokio::spawn(run_watch_persist(store, watch_rx, config.shutdown.clone()))
        });
        // ⑤ accept 循环（后台；shutdown 信号 → 退出 + 自注销）。
        let cleanup = Arc::new(ServerCleanup {
            registry: registry.clone(),
            pid,
            sock: sock_path.clone(),
            done: AtomicBool::new(false),
        });
        let task_cleanup = cleanup.clone();
        let task_registry = registry.clone();
        let on_user = config.on_user.clone();
        let on_report = config.on_report.clone();
        let shutdown = config.shutdown.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((conn, _addr)) => {
                            handle_conn(conn, task_registry.clone(), on_user.clone(), on_report.clone());
                        }
                        Err(e) => {
                            warn!(target: "imagent::core", error = %e, "peer socket accept 失败（退避后继续）");
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                    }
                }
            }
            task_cleanup.run();
        });
        // ⑥ 窗口轮询任务（v1.43；watch_interval = None 时不启动；零间隔等同
        //    None——tokio 的 interval 遇零会 panic，坏配置不该让后台任务静默死掉）。
        let watch_task = config
            .watch_interval
            .filter(|d| !d.is_zero())
            .map(|interval| {
                tokio::spawn(run_watch(
                    WindowWatcher {
                        registry: registry.clone(),
                        state: state.clone(),
                        settings_path: config.settings_path.clone(),
                        peer_name: config.name.clone(),
                        tracked: HashMap::new(),
                        miss: HashMap::new(),
                        last_hook_check: Instant::now(),
                        hook_refresh: HOOK_STATUS_REFRESH,
                    },
                    interval,
                    config.shutdown.clone(),
                    config.on_windows.clone(),
                ))
            });
        Ok(Self {
            sender: Arc::new(PeerSender::with_state(
                registry.dir().to_path_buf(),
                sock_path.clone(),
                config.name,
                state.clone(),
            )),
            state,
            registry,
            cleanup,
            task,
            watch_task,
            persist_task: parking_lot::Mutex::new(persist_task),
        })
    }

    /// 出站句柄（main 注入 Dispatcher；与 server 共享注册表与共享态）。
    pub fn sender(&self) -> Arc<PeerSender> {
        self.sender.clone()
    }

    /// 窗口看板共享态（main 注入 web 平台：`windows` 帧数据源 + 关注名单）。
    pub fn state(&self) -> Arc<PeerState> {
        self.state.clone()
    }

    /// 注册表访问器（测试/命令诊断用）。
    pub fn registry(&self) -> &PeerRegistry {
        &self.registry
    }

    /// 幂等注销（删注册条目 + unlink socket）。accept 循环已随停机信号自注销
    /// 时此处 no-op；main 的 run() 返回后显式调用作确定性兜底（异常退出码路径
    /// 会 process::exit，必须先于 exit 执行——同 permission.sock 清理的理由）。
    pub async fn shutdown(&self) {
        self.cleanup.run();
        // 兜底终止 accept 循环与轮询任务（正常路径已由 token 驱动退出；cleanup
        // 已先行执行，abort 只是防任务卡在 accept / 定时器上不退）。
        self.task.abort();
        if let Some(t) = &self.watch_task {
            t.abort();
        }
        // 关注名单落盘泵不 abort：等它把停机前最后一次变化写完。先卸下共享态上
        // 的回调——发送端随之释放，泵的 changed() 立刻返回 Err 并在退出前补写
        // 未落盘的变化；不必等停机令牌（令牌只在收到 SIGINT/SIGTERM 时置位，
        // 会话过期等退出路径上并不置位）。上限 2s——一次卡住的写不该拖住进程
        // 退出。先把句柄取出再 await——锁卫兵不能跨 await 持有。
        self.state.set_watch_changed_callback(None);
        let persist_task = self.persist_task.lock().take();
        if let Some(t) = persist_task {
            let _ = tokio::time::timeout(Duration::from_secs(2), t).await;
        }
    }
}

/// 非 unix 平台：peer 通道不可用（UDS 缺席），start 恒 Err——调用方 fail-open
/// 降级（peer 功能仅 macOS/Linux 部署有意义，与权限审批 socket 同边界）。
#[cfg(not(unix))]
impl PeerServer {
    pub async fn start(_config: PeerServerConfig) -> Result<Self, CoreError> {
        Err(CoreError::Platform(
            ERR_PLATFORM,
            "peer 通道需要 Unix domain socket，当前平台不支持".into(),
        ))
    }

    pub fn sender(&self) -> Arc<PeerSender> {
        unreachable!("非 unix 平台 start 恒 Err，PeerServer 不可构造")
    }

    pub fn state(&self) -> Arc<PeerState> {
        unreachable!("非 unix 平台 start 恒 Err，PeerServer 不可构造")
    }

    pub async fn shutdown(&self) {
        unreachable!("非 unix 平台 start 恒 Err，PeerServer 不可构造")
    }
}

// ---------------------------------------------------------------------------
// 窗口轮询任务（v1.43；仅 unix——与 PeerServer::start 同边界）
// ---------------------------------------------------------------------------

/// 轮询防抖：解析失败的条目沿用上一轮信息的最大连续轮数（注册表文件被改写
/// 的瞬间可能读到空 / 半截内容）。
#[cfg(unix)]
const CARRY_ON_PARSE_FAIL_ROUNDS: u8 = 3;

/// 轮询防抖：上一轮有、本轮没有的窗口，连续缺席这么多轮才移除（条目文件被
/// 原子改写时会有瞬时读不到的情况）。
#[cfg(unix)]
const ABSENT_ROUNDS_BEFORE_REMOVE: u8 = 2;

/// `hookInstalled` 重算间隔（读一次 `settings.json`，小文件）。
#[cfg(unix)]
const HOOK_STATUS_REFRESH: Duration = Duration::from_secs(30);

/// 条目本轮没进快照的原因。
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissKind {
    /// 条目文件还在，但没解析出活条目（内容瞬时损坏 / 套接字探活瞬时失败）。
    Parse,
    /// 条目文件不在了（窗口退出，或改名重写的间隙）。
    Absent,
}

/// 上一轮未取到的条目：原因 + 连续轮数。
#[cfg(unix)]
struct Miss {
    kind: MissKind,
    rounds: u8,
}

/// 轮询任务的跨轮状态。
#[cfg(unix)]
struct WindowWatcher {
    registry: PeerRegistry,
    state: Arc<PeerState>,
    settings_path: Option<PathBuf>,
    peer_name: String,
    /// 上一轮进入快照的条目（按注册表 pid 键——防抖沿用的来源）。
    tracked: HashMap<u32, WindowInfo>,
    /// 上一轮未取到、正被沿用的条目及其连续轮数。
    miss: HashMap<u32, Miss>,
    last_hook_check: Instant,
    /// `hookInstalled` 重算间隔（生产 [`HOOK_STATUS_REFRESH`]；测试注入短间隔）。
    hook_refresh: Duration,
}

/// 注册表目录里现存的条目文件 pid（`<pid>.json`）——区分「文件在但解析失败」
/// 与「文件没了」两种缺席。
#[cfg(unix)]
fn existing_entry_pids(dir: &Path) -> HashSet<u32> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return HashSet::new();
    };
    rd.flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".json"))
                .filter(|stem| !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|stem| stem.parse::<u32>().ok())
        })
        .collect()
}

#[cfg(unix)]
impl WindowWatcher {
    /// 一轮轮询：探活后的窗口条目 + 防抖沿用，产出本轮快照（未排序——排序与
    /// 变化判定在 [`PeerState::apply_snapshot`]）。全 best-effort，不返回错误
    /// （注册表是本机所有 Claude Code 实例共用的，条目会被别的进程改写 / 清理，
    /// 读取要容忍瞬时缺失与解析失败）。
    async fn snapshot(&mut self) -> Vec<WindowInfo> {
        let files = existing_entry_pids(self.registry.dir());
        let live: HashMap<u32, PeerEntry> = self
            .registry
            .live_entries()
            .await
            .into_iter()
            .filter(|e| e.is_window())
            .map(|e| (e.pid, e))
            .collect();
        let mut next: HashMap<u32, WindowInfo> = live
            .iter()
            .map(|(&pid, e)| (pid, WindowInfo::from_entry(e)))
            .collect();
        let mut next_miss = HashMap::new();
        // 上一轮有、本轮没有的条目：按缺席原因沿用有限轮数。
        for (pid, info) in std::mem::take(&mut self.tracked) {
            if next.contains_key(&pid) {
                continue;
            }
            let kind = if files.contains(&pid) {
                MissKind::Parse
            } else {
                MissKind::Absent
            };
            // 同一原因连续缺席：累计轮数；原因变了从 1 重新数。
            let rounds = match self.miss.remove(&pid) {
                Some(miss) if miss.kind == kind => miss.rounds + 1,
                _ => 1,
            };
            let keep = match kind {
                // 解析失败：最多沿用 3 轮（第 3 轮仍在快照里，第 4 轮移除）。
                MissKind::Parse => rounds <= CARRY_ON_PARSE_FAIL_ROUNDS,
                // 缺席：连续 2 轮才移除（第 1 轮先保留，第 2 轮移除）。
                MissKind::Absent => rounds < ABSENT_ROUNDS_BEFORE_REMOVE,
            };
            // 同名的新条目优先（窗口重启换了 pid）：沿用的旧实例不得与新实例并列。
            if keep && !next.values().any(|w| w.name == info.name) {
                next.insert(pid, info);
                next_miss.insert(pid, Miss { kind, rounds });
            }
        }
        self.tracked = next;
        self.miss = next_miss;
        self.tracked.values().cloned().collect()
    }
}

/// 启动时把持久化的关注名单装进共享态（[`PeerServer::start`] 在开轮询任务
/// **之前** `await` 它）。读失败 / 畸形只 warn、按空集继续；此时落盘回调还没
/// 装上，加载本身不会触发回写。返回加载的条数（日志 / 诊断用）。
#[cfg(unix)]
async fn load_watch_list(state: &PeerState, store: Option<&imagent_store::Store>) -> usize {
    let Some(store) = store else {
        return 0;
    };
    let raw = match store.get_config(WATCH_CONFIG_KEY).await {
        Ok(raw) => raw,
        Err(e) => {
            warn!(
                target: "imagent::core",
                error = %e,
                "读取持久化的关注名单失败，按空集继续（不影响 peer 通道启动）"
            );
            return 0;
        }
    };
    let Some(raw) = raw else {
        return 0;
    };
    let names = decode_watch_list(&raw);
    if names.is_empty() && raw.trim() != "[]" {
        warn!(
            target: "imagent::core",
            raw_head = %raw.chars().take(80).collect::<String>(),
            "持久化的关注名单不是 JSON 字符串数组，按空集继续"
        );
    }
    for name in &names {
        state.set_watched(name, true);
    }
    if !names.is_empty() {
        info!(
            target: "imagent::core",
            count = names.len(),
            "已加载持久化的关注名单"
        );
    }
    names.len()
}

/// 关注名单落盘泵（单写者）。`PeerState` 的变化回调只把最新名单放进 watch
/// 通道；本泵串行写库，所以写序 = 变化序，连点只落最后一次（中间值合并）。
/// 随停机信号退出；退出前若还有没轮到写的变化，补写一次（写失败只 warn：
/// 名单在本次运行内仍生效，重启后回到上次落盘的那份）。
#[cfg(unix)]
async fn run_watch_persist(
    store: Arc<imagent_store::Store>,
    mut names_rx: tokio::sync::watch::Receiver<Vec<String>>,
    shutdown: CancellationToken,
) {
    // 初值 = 刚加载的名单，已是落盘状态：不因启动而重写一次。
    names_rx.mark_unchanged();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            changed = names_rx.changed() => {
                if changed.is_err() {
                    break; // 发送端（PeerState 回调）已全部释放
                }
            }
        }
        // 先把名单 clone 出来再 await：borrow 的读锁卫兵不能跨 await 持有。
        let names = names_rx.borrow_and_update().clone();
        if let Err(e) = persist_watch_list(store.as_ref(), &names).await {
            warn!(
                target: "imagent::core",
                error = %e,
                "关注名单落盘失败（本次运行内仍生效，重启后回到上次落盘的名单）"
            );
        }
    }
    if names_rx.has_changed().unwrap_or(false) {
        let names = names_rx.borrow_and_update().clone();
        if let Err(e) = persist_watch_list(store.as_ref(), &names).await {
            warn!(target: "imagent::core", error = %e, "关注名单收尾落盘失败");
        }
    }
    debug!(target: "imagent::core", "peer 关注名单落盘泵退出");
}

#[cfg(unix)]
async fn persist_watch_list(
    store: &imagent_store::Store,
    names: &[String],
) -> imagent_store::Result<()> {
    store
        .set_config(WATCH_CONFIG_KEY, &encode_watch_list(names))
        .await
}

/// 轮询主循环：首次立即执行（不先睡一个间隔），之后按间隔；随停机信号退出。
/// 每轮 = （到点则重算 `hookInstalled`）→ 快照 → [`PeerState::apply_snapshot`]
/// → 快照有变化 / 有事件 / `hookInstalled` 翻转时调回调。轮询内部没有可失败
/// 的步骤（全部 best-effort 吞掉），任务不会因单次异常退出。
#[cfg(unix)]
async fn run_watch(
    mut watcher: WindowWatcher,
    interval: Duration,
    shutdown: CancellationToken,
    on_windows: PeerWindowsCallback,
) {
    let mut ticker = tokio::time::interval(interval);
    // 一轮耗时超过间隔（机器卡顿）时不补发积压的 tick——注册表 3 秒读一次
    // 是足够新的近似，连发只会白耗 IO。
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {}
        }
        let now = Instant::now();
        let mut hook_flipped = false;
        if now.saturating_duration_since(watcher.last_hook_check) >= watcher.hook_refresh {
            watcher.last_hook_check = now;
            hook_flipped = watcher.state.set_hook_installed(hook_installed(
                watcher.settings_path.as_deref(),
                &watcher.peer_name,
            ));
        }
        let windows = watcher.snapshot().await;
        let (changed, events) = watcher.state.apply_snapshot(&windows, now);
        if changed || !events.is_empty() || hook_flipped {
            on_windows(watcher.state.windows(), events);
        }
    }
    debug!(target: "imagent::core", "peer 窗口轮询任务退出");
}

/// 单连接处理：JSON 行循环（1MiB 行上限，超限跳行）→ user 帧解析窗口名 →
/// spawn 回调；读到对端 EOF 后 drop 连接（跟随关闭——claude 发送方等对端全
/// 关闭才 resolve，参考实现 bindSocket 的 allowHalfOpen:false 语义）。
///
/// **全程不向入站连接写任何字节**。这是协议硬约束而非省略，不要「补回执」：
///
/// Claude Code 的发送方（2.1.310 实测）写完帧、半关闭后，只等对端**不回任何
/// 数据并关闭连接**；对端只要回了数据，发送方就判失败
/// （`{"success":false,"message":"Failed to send to <网关名>."}`）。
/// v1.41.0–v1.41.7 在这里回写一行 receipt（`type:control` /
/// `action:peer_message_status` / `status:delivered`）：网关每条都收到，窗口侧
/// SendMessage 却每次报 Failed → 窗口的 claude 以为没送到而重发（真机上同一
/// 问题出现连续两条回复），进而认定回程链路不通、改在自己终端里答题。
///
/// 对照实验（2026-11-24，真实 `claude --bg` 会话作发送方，同一时刻、只变
/// 接收方一个条件）：
/// - 接收方在入站连接上写一行 receipt 再关闭 →
///   `{"success":false,"message":"Failed to send to exp-sameack."}`
/// - 接收方什么都不写、直接关闭 → `{"success":true,…,"msg_id":…}`
///
/// 也不做「反连回执」（反过来连接发送方 socket 送 `peer_message_status`）：
/// 实测发送方会因此多出一条「[Cross-session delivery notice] approved and
/// released」并**多跑一轮**。网关不回执即成功，也不给窗口会话添多余的轮次。
///
/// 「不回任何字节」对入站的每种帧都成立：control 等非 user 帧一律忽略，同样
/// 不回写；v1.43 的 hook 帧（`imagent peer-hook` 客户端）同理——客户端同样
/// 「写完即半关闭、等对端关闭」。发送方发帧前还会先开一条连接、不写数据、
/// 立刻关闭（探活）——「无数据即 EOF」的连接走同一条路直接退出。回归钉：
/// `server_inbound_silent_close_matches_real_sender`、
/// `server_hook_frame_silent_and_reports_once`。
#[cfg(unix)]
fn handle_conn(
    conn: tokio::net::UnixStream,
    registry: PeerRegistry,
    on_user: PeerInboundCallback,
    on_report: PeerReportCallback,
) {
    tokio::spawn(async move {
        use crate::lineio::read_line_capped;
        use tokio::io::BufReader;
        // 只有读端、没有写端——本连接上永不写数据（见函数注释）。
        let mut lines = BufReader::new(conn);
        loop {
            match read_line_capped(&mut lines, MAX_LINE_BYTES).await {
                Ok(Some(raw)) => {
                    let line = raw.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match parse_frame_line(line) {
                        Some(PeerFrame::User {
                            msg_id,
                            from,
                            content,
                        }) => {
                            // 剥 envelope → 正文；窗口名三级解析：注册表 sock 反查 →
                            // envelope from-name → sock 文件名（再兜底 "unknown"）。
                            let stripped = strip_envelope(&content);
                            let from_sock = from.as_deref().map(strip_uds_prefix);
                            let window = from_sock
                                .and_then(|s| registry.find_by_sock(s))
                                .map(|e| e.name)
                                .or_else(|| stripped.from_name.clone())
                                .or_else(|| from_sock.and_then(sock_file_stem))
                                .unwrap_or_else(|| "unknown".to_string());
                            debug!(
                                target: "imagent::core",
                                window = %window,
                                msg_id = %msg_id,
                                bytes = stripped.body.len(),
                                "peer 入站 user 帧（不回写；对端 EOF 后关闭）"
                            );
                            let cb = on_user.clone();
                            let body = stripped.body;
                            // 回调走 send_text（可能带退避重试）——spawn 隔离，不阻塞
                            // 连接读循环（多帧连接不被单回调拖住）。
                            tokio::spawn(async move { cb(window, body).await });
                        }
                        // v1.43 Stop 钩子上报：只认 `hook == "Stop"`（其它事件名
                        // 尚无消费方，忽略）；回调要查注册表 + 投递会话，同样
                        // spawn 隔离。**绝不向窗口回发任何消息**（上报是单向的，
                        // 不存在环路）。
                        Some(PeerFrame::Hook {
                            hook,
                            session_id,
                            message,
                            ..
                        }) if hook == HOOK_EVENT_STOP => {
                            debug!(
                                target: "imagent::core",
                                session_id = %session_id,
                                bytes = message.len(),
                                "peer 入站 Stop 钩子上报（不回写）"
                            );
                            let cb = on_report.clone();
                            tokio::spawn(async move { cb(session_id, message).await });
                        }
                        Some(_) => continue, // control 等其它类型忽略
                        None => continue,    // 畸形行跳过
                    }
                }
                Ok(None) => break, // 对端 EOF（半关闭）：跳出后 drop 连接 → 全关闭
                Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                    continue; // 超长行已整体丢弃（1MiB 上限），下一行干净恢复
                }
                Err(_) => break,
            }
        }
    });
}

// ---------------------------------------------------------------------------
// 测试（协议单测 + tempdir 注入集成测试）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// 唯一短 tempdir（Unix socket 路径有 SUN_LEN ≈104 字节上限，macOS 默认
    /// temp_dir 前缀过长——固定用 /tmp 短根 + 进程 id + 纳秒；无 tempfile 依赖）。
    fn tmp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ip{}_{}_{}", tag, std::process::id(), nanos));
        std::fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    /// 写一条注册表条目（测试 helper：字段与真实 claude/imagent 条目同构）。
    /// `entrypoint` 用**真实取值**：窗口是 `cli`（交互式 `claude`）、无头会话是
    /// `sdk-cli`、网关自身是 `imagent`——窗口认定是白名单
    /// （[`PeerEntry::is_window`]），夹具写错值用例会整批转红。
    fn write_entry(dir: &Path, pid: u32, name: &str, sock: &Path, entrypoint: &str) {
        std::fs::create_dir_all(dir).expect("create registry dir");
        let v = serde_json::json!({
            "pid": pid,
            "sessionId": uuid::Uuid::new_v4().to_string(),
            "cwd": "/tmp",
            "startedAt": 1_700_000_000_000i64,
            "version": entrypoint,
            "peerProtocol": 1,
            "kind": "interactive",
            "entrypoint": entrypoint,
            "messagingSocketPath": sock.to_string_lossy(),
            "name": name,
            "nameSource": "derived",
            "status": "idle",
        });
        std::fs::write(dir.join(format!("{pid}.json")), v.to_string()).expect("write entry");
    }

    /// [`PeerServerConfig`] 的收帧测试形态：不轮询、不探测钩子（不读 home）、
    /// hook / 窗口回调为空——既有收帧用例只关心 `on_user`。
    fn server_config(
        registry_dir: &Path,
        cwd: PathBuf,
        shutdown: CancellationToken,
        on_user: PeerInboundCallback,
    ) -> PeerServerConfig {
        PeerServerConfig {
            registry_dir: registry_dir.to_path_buf(),
            name: "im-agent".to_string(),
            cwd,
            settings_path: None,
            watch_interval: None,
            watch_store: None,
            shutdown,
            on_user,
            on_report: Arc::new(|_, _| Box::pin(async {})),
            on_windows: Arc::new(|_, _| {}),
        }
    }

    // ---- 帧纯函数 ----

    #[test]
    fn escape_body_escapes_closing_tag_only() {
        assert_eq!(escape_envelope_body("hello"), "hello");
        // 伪闭合标签 → 转义。
        assert_eq!(
            escape_envelope_body("a</cross-session-message>b"),
            "a<\\/cross-session-message>b"
        );
        // 大小写不敏感 + 各边界形态。
        assert_eq!(
            escape_envelope_body("</Cross-Session-Message>"),
            "<\\/Cross-Session-Message>"
        );
        assert_eq!(
            escape_envelope_body("</cross-session-message\nnext"),
            "<\\/cross-session-message\nnext"
        );
        assert_eq!(
            escape_envelope_body("</cross-session-message/>"),
            "<\\/cross-session-message/>"
        );
        assert_eq!(
            escape_envelope_body("</cross-session-message"),
            "<\\/cross-session-message"
        );
        // 非边界（标签名不完整/带后缀）→ 原样。
        assert_eq!(
            escape_envelope_body("</cross-session-messages>"),
            "</cross-session-messages>"
        );
        assert_eq!(escape_envelope_body("</cross"), "</cross");
        assert_eq!(escape_envelope_body("</ other"), "</ other");
    }

    #[test]
    fn unescape_body_round_trips_escape() {
        let raw = "看 </cross-session-message> 这个标签";
        let escaped = escape_envelope_body(raw);
        assert_eq!(unescape_envelope_body(&escaped), raw);
        // 独立反转义。
        assert_eq!(
            unescape_envelope_body("<\\/cross-session-message>x"),
            "</cross-session-message>x"
        );
        assert_eq!(unescape_envelope_body("plain"), "plain");
    }

    #[test]
    fn envelope_build_strip_round_trip() {
        let body = "正文第一行\n含伪闭合 </cross-session-message> 的行\n尾部";
        let content = build_envelope("uds:/tmp/cc-socks/1.sock", "窗 口<A>", body);
        // from-name 消毒（剥 "<>）。
        assert!(content.contains("from-name=\"窗 口A\""), "{content}");
        assert!(content.starts_with("<cross-session-message from=\"uds:/tmp/cc-socks/1.sock\""));
        assert!(content.ends_with("</cross-session-message>"));
        let stripped = strip_envelope(&content);
        assert_eq!(stripped.body, body, "往返无损（含转义还原）");
        assert_eq!(stripped.from.as_deref(), Some("uds:/tmp/cc-socks/1.sock"));
        assert_eq!(stripped.from_name.as_deref(), Some("窗 口A"));
    }

    #[test]
    fn strip_envelope_non_envelope_returns_verbatim() {
        // 无 envelope → 原文。
        let s = strip_envelope("普通正文");
        assert_eq!(s.body, "普通正文");
        assert_eq!(s.from, None);
        // 有开无闭 → 原文（不吞正文）。
        let s = strip_envelope("<cross-session-message from=\"x\">\n正文");
        assert_eq!(s.body, "<cross-session-message from=\"x\">\n正文");
        // 属性形态不符（大写属性名）→ 原文。
        let s =
            strip_envelope("<cross-session-message From=\"x\">\n正文\n</cross-session-message>");
        assert_eq!(
            s.body,
            "<cross-session-message From=\"x\">\n正文\n</cross-session-message>"
        );
        // 空正文 envelope 合法。
        let s = strip_envelope("<cross-session-message>\n\n</cross-session-message>");
        assert_eq!(s.body, "");
        // 多属性形态。
        let s = strip_envelope(
            "<cross-session-message from=\"u1\" from-name=\"窗口\" from-mode=\"x\">\nhi\n</cross-session-message>",
        );
        assert_eq!(s.body, "hi");
        assert_eq!(s.from.as_deref(), Some("u1"));
        assert_eq!(s.from_name.as_deref(), Some("窗口"));
    }

    #[test]
    fn user_frame_json_shape() {
        let content = build_envelope("uds:/s/1.sock", "im-agent", "hi");
        let line = build_user_frame_json("/tmp/cc-socks/9.sock", &content);
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid json");
        assert_eq!(v["msgV"], 1);
        assert_eq!(v["type"], "user");
        assert_eq!(v["priority"], "next");
        assert_eq!(v["from"], "uds:/tmp/cc-socks/9.sock");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"], content);
        assert!(
            !v["msg_id"].as_str().expect("uuid").is_empty(),
            "msg_id 为 uuid 串"
        );
    }

    #[test]
    fn registry_entry_json_fields() {
        let line = registry_entry_json(
            42,
            "sess-1",
            Path::new("/work"),
            1_700_000_000_000,
            Some("Mon Nov 10 09:00:00 2026"),
            Path::new("/tmp/cc-socks/42.sock"),
            "im-agent",
        );
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid json");
        assert_eq!(v["pid"], 42);
        assert_eq!(v["sessionId"], "sess-1");
        assert_eq!(v["cwd"], "/work");
        assert_eq!(v["startedAt"], 1_700_000_000_000i64);
        assert_eq!(v["procStart"], "Mon Nov 10 09:00:00 2026");
        assert_eq!(v["version"], "imagent");
        assert_eq!(v["peerProtocol"], 1);
        assert_eq!(v["kind"], "interactive");
        assert_eq!(v["entrypoint"], "imagent");
        assert_eq!(v["messagingSocketPath"], "/tmp/cc-socks/42.sock");
        assert_eq!(v["name"], "im-agent");
        assert_eq!(v["nameSource"], "derived");
        assert_eq!(v["status"], "idle");
        // procStart 缺席形态（JSON.stringify undefined 同语义）。
        let line = registry_entry_json(
            7,
            "s",
            Path::new("/w"),
            1,
            None,
            Path::new("/tmp/7.sock"),
            "n",
        );
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(v.get("procStart").is_none(), "无 procStart 时字段缺席");
    }

    #[test]
    fn parse_frame_line_classifies() {
        // user 帧。
        let f = parse_frame_line(
            r#"{"msgV":1,"msg_id":"m1","type":"user","priority":"next","from":"uds:/s/1.sock","message":{"role":"user","content":"<cross-session-message from=\"uds:/s/1.sock\" from-name=\"w\">\n正文\n</cross-session-message>"}}"#,
        );
        assert_eq!(
            f,
            Some(PeerFrame::User {
                msg_id: "m1".into(),
                from: Some("uds:/s/1.sock".into()),
                content: "<cross-session-message from=\"uds:/s/1.sock\" from-name=\"w\">\n正文\n</cross-session-message>".into(),
            })
        );
        // control 帧 → Other（忽略类）。
        assert_eq!(
            parse_frame_line(r#"{"msgV":1,"msg_id":"x","type":"control","action":"y"}"#),
            Some(PeerFrame::Other)
        );
        // 畸形 / 缺 type → None。
        assert_eq!(parse_frame_line("not json"), None);
        assert_eq!(parse_frame_line(r#"{"msgV":1}"#), None);
    }

    #[test]
    fn strip_uds_and_sock_stem_helpers() {
        assert_eq!(strip_uds_prefix("uds:/tmp/a.sock"), "/tmp/a.sock");
        assert_eq!(strip_uds_prefix("/tmp/a.sock"), "/tmp/a.sock");
        assert_eq!(
            sock_file_stem("/tmp/cc-socks/1234.sock"),
            Some("1234".to_string())
        );
        assert_eq!(sock_file_stem(""), None);
    }

    // ---- 集成：PeerServer + mock 窗口（tempdir 注入） ----

    /// mock 窗口：写注册条目 + bind socket + 后台 accept 收帧（记录到句柄）。
    struct MockWindow {
        sock: PathBuf,
        frames: Arc<StdMutex<Vec<String>>>,
        _task: tokio::task::JoinHandle<()>,
    }

    impl MockWindow {
        async fn bind(registry_dir: &Path, sock_dir: &Path, pid: u32, name: &str) -> Self {
            let sock = sock_dir.join(format!("{pid}.sock"));
            let _ = std::fs::remove_file(&sock);
            std::fs::create_dir_all(sock_dir).unwrap();
            let listener = tokio::net::UnixListener::bind(&sock).expect("mock window bind");
            write_entry(registry_dir, pid, name, &sock, "cli");
            let frames: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
            let fh = frames.clone();
            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut conn, _)) = listener.accept().await else {
                        break;
                    };
                    let fh = fh.clone();
                    tokio::spawn(async move {
                        use tokio::io::AsyncReadExt;
                        let mut line = String::new();
                        let mut buf = vec![0u8; 65536];
                        // 循环读到出现 \n（单帧即可；窗口侧实现按行读）。
                        loop {
                            match conn.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    line.push_str(&String::from_utf8_lossy(&buf[..n]));
                                    if line.contains('\n') {
                                        break;
                                    }
                                }
                            }
                        }
                        if !line.trim().is_empty() {
                            fh.lock().unwrap().push(line.trim().to_string());
                        }
                        // 真实窗口的行为（v1.41.8 实证）：收帧后不回写任何字节，
                        // 直接关闭——conn 随作用域结束 drop，对端（网关
                        // send_frame）读到 EOF 即送达。见 handle_conn 注释。
                    });
                }
            });
            Self {
                sock,
                frames,
                _task: task,
            }
        }

        fn line_count(&self) -> usize {
            self.frames.lock().unwrap().len()
        }

        fn first_line(&self) -> Option<String> {
            self.frames.lock().unwrap().first().cloned()
        }
    }

    #[tokio::test]
    async fn server_inbound_and_outbound_round_trip() {
        let tmp = tmp_dir("roundtrip");
        let registry_dir = tmp.join("r");
        let sock_dir = tmp.join("s");
        std::fs::create_dir_all(&registry_dir).unwrap();

        // mock 窗口：注册 + bind。
        let window = MockWindow::bind(&registry_dir, &sock_dir, 4242, "窗口A").await;

        // 回调捕获。
        let got: Arc<StdMutex<Vec<(String, String)>>> = Arc::new(StdMutex::new(Vec::new()));
        let got_cb = got.clone();
        let shutdown = CancellationToken::new();
        let server = PeerServer::start(server_config(
            &registry_dir,
            tmp.clone(),
            shutdown.clone(),
            Arc::new(move |w, b| {
                let g = got_cb.clone();
                Box::pin(async move { g.lock().unwrap().push((w, b)) })
            }),
        ))
        .await
        .expect("server start");
        let gw_sock = server.sender().own_sock().to_string_lossy().into_owned();

        // ---- 窗口 → 网关：发一帧 → 半关闭，断言「0 字节 + EOF」+ 回调窗口名与正文 ----
        // 帧的 from 字段须指向窗口真实 sock（注册表反查依据）——直接用窗口
        // sock 路径组帧（build_user_frame_json 的 from 参即 sock 路径）。
        let content = build_envelope("uds:mock-window", "窗口A", "帮我看看构建");
        let frame = build_user_frame_json(&window.sock.to_string_lossy(), &content);
        let mut conn = tokio::net::UnixStream::connect(&gw_sock)
            .await
            .expect("connect gw");
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        conn.write_all(frame.as_bytes()).await.unwrap();
        conn.write_all(b"\n").await.unwrap();
        conn.shutdown().await.unwrap();
        // 读到 EOF：网关**不回写任何字节**、对端半关闭后跟随关闭（协议硬约束
        // ——回写数据会让 Claude Code 发送方判失败，见 handle_conn）。
        let mut resp = Vec::new();
        let n = tokio::time::timeout(Duration::from_secs(5), conn.read_to_end(&mut resp))
            .await
            .expect("网关应在发送方 5s 预算内关闭连接")
            .expect("read_to_end");
        assert!(
            n == 0 && resp.is_empty(),
            "网关不得在入站连接上回写任何字节：{:?}",
            String::from_utf8_lossy(&resp)
        );
        // 回调：窗口名按注册表反查（messagingSocketPath == frame.from 剥 uds:），
        // 正文为剥 envelope 后原文。
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while got.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        {
            let got_all = got.lock().unwrap();
            assert_eq!(
                got_all.as_slice(),
                &[("窗口A".to_string(), "帮我看看构建".to_string())],
                "回调拿到 (窗口名, 正文)"
            );
        } // guard 块内结束——不跨后续 await（await_holding_lock）

        // ---- 网关 → 窗口：出站投递，断言帧内容与 envelope ----
        server
            .sender()
            .send_to_window("窗口A", "出站派活正文")
            .await
            .expect("send to window");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while window.line_count() == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let out = window.first_line().expect("window received frame");
        let ov: serde_json::Value = serde_json::from_str(&out).expect("outbound json");
        assert_eq!(ov["type"], "user");
        assert_eq!(
            ov["from"],
            format!("uds:{gw_sock}"),
            "from = uds:<网关 sock 路径>"
        );
        let content = ov["message"]["content"].as_str().unwrap();
        let stripped = strip_envelope(content);
        assert_eq!(stripped.body, "出站派活正文");
        assert_eq!(
            stripped.from.as_deref(),
            Some(format!("uds:{gw_sock}").as_str())
        );
        assert_eq!(stripped.from_name.as_deref(), Some("im-agent"));

        // ---- 停机：注册条目 + socket 清理 ----
        shutdown.cancel();
        server.shutdown().await;
        assert!(
            !registry_dir
                .join(format!("{}.json", std::process::id()))
                .exists(),
            "网关注册条目已删"
        );
        assert!(!Path::new(&gw_sock).exists(), "网关 socket 已 unlink");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 回归钉（v1.41.8）：按真实 Claude Code 发送方（2.1.310 实测）的时序走一遍
    /// SendMessage——① 先开一条连接、不写数据、立刻关闭（探活）；② 另开一条
    /// 连接写 user 帧、半关闭。网关必须：回调恰好一次、两条连接上都不回任何
    /// 字节、都在对端半关闭后及时关闭（发送方只认「0 字节 + 对端关闭」为送达
    /// ——见 [`handle_conn`]。v1.41.0–v1.41.7 在入站连接回写 receipt，窗口侧
    /// SendMessage 每次被判 Failed 并重发）。
    #[tokio::test]
    async fn server_inbound_silent_close_matches_real_sender() {
        let tmp = tmp_dir("silent");
        let registry_dir = tmp.join("r");
        let sock_dir = tmp.join("s");
        std::fs::create_dir_all(&registry_dir).unwrap();
        std::fs::create_dir_all(&sock_dir).unwrap();
        // 发送方窗口的注册条目（真实发送方的 from 指向它自己已注册的 sock）。
        // sock 落在本用例自己的目录：server 的 socket 目录发现取「既有条目 sock
        // 的 dirname」——空注册表会回退到 /tmp/cc-socks/<pid>.sock，与同进程里
        // 其它走回退的用例绑到同一路径而互踩。
        let sender_sock = sock_dir.join("9001.sock");
        write_entry(&registry_dir, 9001, "probe-window", &sender_sock, "cli");
        let got: Arc<StdMutex<Vec<(String, String)>>> = Arc::new(StdMutex::new(Vec::new()));
        let got_cb = got.clone();
        let server = PeerServer::start(server_config(
            &registry_dir,
            tmp.clone(),
            CancellationToken::new(),
            Arc::new(move |w, b| {
                let g = got_cb.clone();
                Box::pin(async move { g.lock().unwrap().push((w, b)) })
            }),
        ))
        .await
        .expect("server start");
        let gw_sock = server.sender().own_sock().to_string_lossy().into_owned();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // ① 探活：连上、不写任何数据就关闭。真实发送方是直接 close；这里用半
        //    关闭写端代替——网关读侧看到的同样是「无数据即 EOF」，而测试端因此
        //    还能读到网关有没有回写。
        let mut probe = tokio::net::UnixStream::connect(&gw_sock)
            .await
            .expect("connect gw (probe)");
        probe.shutdown().await.expect("probe shutdown");

        // ② 发帧连接：不等探活连接收尾就开（真实发送方也不等），写 user 帧后
        //    半关闭。窗口名走注册表反查（envelope 的 from-name 故意写成别的
        //    名字——反查命中优先）。
        let sender = sender_sock.to_string_lossy().into_owned();
        let content = build_envelope(&format!("uds:{sender}"), "alias-name", "回程正文");
        let frame = build_user_frame_json(&sender, &content);
        let mut conn = tokio::net::UnixStream::connect(&gw_sock)
            .await
            .expect("connect gw (frame)");
        conn.write_all(frame.as_bytes()).await.unwrap();
        conn.write_all(b"\n").await.unwrap();
        conn.shutdown().await.expect("frame conn shutdown");

        // 两条连接都读到 EOF 且 0 字节；预算 = 发送方的 5s（超了发送方即报
        // Failed to send）。
        let (mut probe_sink, mut conn_sink) = (Vec::new(), Vec::new());
        let probe_n = tokio::time::timeout(SEND_TIMEOUT, probe.read_to_end(&mut probe_sink))
            .await
            .expect("探活连接：网关应在发送方预算内关闭")
            .expect("probe read_to_end");
        assert!(
            probe_n == 0 && probe_sink.is_empty(),
            "探活连接不得收到任何字节：{:?}",
            String::from_utf8_lossy(&probe_sink)
        );
        let conn_n = tokio::time::timeout(SEND_TIMEOUT, conn.read_to_end(&mut conn_sink))
            .await
            .expect("发帧连接：网关应在发送方预算内关闭")
            .expect("conn read_to_end");
        assert!(
            conn_n == 0 && conn_sink.is_empty(),
            "发帧连接不得收到任何字节：{:?}",
            String::from_utf8_lossy(&conn_sink)
        );

        // 回调恰好一次：窗口名（注册表反查，优先于 envelope from-name）+ 剥
        // envelope 正文。
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while got.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // 再留一小段静默期：探活连接与连接收尾都不得补出第二次回调。
        tokio::time::sleep(Duration::from_millis(150)).await;
        {
            let got_all = got.lock().unwrap();
            assert_eq!(
                got_all.as_slice(),
                &[("probe-window".to_string(), "回程正文".to_string())],
                "回调恰好一次，且 (窗口名, 正文) 正确"
            );
        } // guard 块内结束——不跨 shutdown await（await_holding_lock）
        server.shutdown().await;
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 出站方向的握手只看「对端是否关闭」：对端在关闭前回了数据，`send_frame`
    /// 照样成功（对端字节丢弃、不解析）。与入站纪律（永不回写）是两回事——
    /// 这里网关是发送方；「对端回数据即失败」是 Claude Code 发送方的规则。
    #[tokio::test]
    async fn send_frame_tolerates_bytes_written_by_peer() {
        let tmp = tmp_dir("tolerate");
        std::fs::create_dir_all(&tmp).unwrap();
        let sock = tmp.join("1.sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("peer bind");
        let peer = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut c, _) = listener.accept().await.unwrap();
            let mut frame = Vec::new();
            c.read_to_end(&mut frame).await.unwrap();
            // 关闭前回写一行数据（会回写的对端实现）。
            c.write_all(b"{\"type\":\"control\"}\n").await.unwrap();
            c.shutdown().await.unwrap();
            frame
        });
        let line = build_user_frame_json("/tmp/cc-socks/9.sock", "hi");
        tokio::time::timeout(SEND_TIMEOUT, send_frame(&sock.to_string_lossy(), &line))
            .await
            .expect("within budget")
            .expect("对端回写数据不应使出站投递失败");
        let frame = peer.await.unwrap();
        assert_eq!(
            String::from_utf8_lossy(&frame),
            format!("{line}\n"),
            "对端收到完整帧"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn server_inbound_name_fallbacks() {
        // 注册表反查失败 → envelope from-name 兜底 → sock 文件名兜底。
        let tmp = tmp_dir("fallback");
        let registry_dir = tmp.join("r");
        std::fs::create_dir_all(&registry_dir).unwrap();
        let got: Arc<StdMutex<Vec<(String, String)>>> = Arc::new(StdMutex::new(Vec::new()));
        let got_cb = got.clone();
        let server = PeerServer::start(server_config(
            &registry_dir,
            tmp.clone(),
            CancellationToken::new(),
            Arc::new(move |w, b| {
                let g = got_cb.clone();
                Box::pin(async move { g.lock().unwrap().push((w, b)) })
            }),
        ))
        .await
        .expect("server start");
        let gw_sock = server.sender().own_sock().to_string_lossy().into_owned();

        let send = |content: String| {
            let gw = gw_sock.clone();
            async move {
                let mut conn = tokio::net::UnixStream::connect(&gw).await.unwrap();
                use tokio::io::AsyncWriteExt;
                conn.write_all(build_user_frame_json("/nonexistent/x.sock", &content).as_bytes())
                    .await
                    .unwrap();
                conn.write_all(b"\n").await.unwrap();
                conn.shutdown().await.unwrap();
                let mut sink = Vec::new();
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    tokio::io::AsyncReadExt::read_to_end(&mut conn, &mut sink),
                )
                .await;
            }
        };

        // ① 注册表查不到（from sock 不存在）→ envelope from-name 兜底。
        send(build_envelope(
            "uds:/nonexistent/x.sock",
            "envelope-name",
            "b1",
        ))
        .await;
        // ② from-name 也缺 → sock 文件名兜底（x）。
        send("<cross-session-message>\nb2\n</cross-session-message>".to_string()).await;
        // ③ 无 envelope → sock 文件名兜底 + 原文正文。
        send("b3".to_string()).await;

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while got.lock().unwrap().len() < 3 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        {
            let got_all = got.lock().unwrap();
            assert_eq!(
                got_all.as_slice(),
                &[
                    ("envelope-name".to_string(), "b1".to_string()),
                    ("x".to_string(), "b2".to_string()),
                    ("x".to_string(), "b3".to_string()),
                ],
                "窗口名三级兜底与正文剥离"
            );
        } // guard 块内结束——不跨 shutdown await（await_holding_lock）
        server.shutdown().await;
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn cleanup_orphans_only_own_family() {
        let tmp = tmp_dir("orphans");
        let registry_dir = tmp.join("r");
        let sock_dir = tmp.join("s");
        std::fs::create_dir_all(&registry_dir).unwrap();
        std::fs::create_dir_all(&sock_dir).unwrap();
        // 本家族孤儿（imagent + 死 sock）→ 清理。
        write_entry(
            &registry_dir,
            111,
            "orphan",
            &sock_dir.join("dead.sock"),
            "imagent",
        );
        // 他家族（真实交互窗口 cli）死 sock → 保留（不得碰）。
        write_entry(
            &registry_dir,
            222,
            "cli-dead",
            &sock_dir.join("dead2.sock"),
            "cli",
        );
        // 活 sock 条目（imagent）→ 保留（用自身监听模拟活）。
        let live_listener = tokio::net::UnixListener::bind(sock_dir.join("alive.sock")).unwrap();
        write_entry(
            &registry_dir,
            333,
            "alive",
            &sock_dir.join("alive.sock"),
            "imagent",
        );

        let reg = PeerRegistry::new(registry_dir.clone());
        let removed = reg.cleanup_orphans().await;
        assert_eq!(removed, 1, "只清本家族死条目");
        assert!(!registry_dir.join("111.json").exists(), "孤儿条目已删");
        assert!(registry_dir.join("222.json").exists(), "窗口条目不碰");
        assert!(registry_dir.join("333.json").exists(), "活条目保留");
        drop(live_listener);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn sender_resolves_by_exact_then_prefix_and_reports_candidates() {
        let tmp = tmp_dir("resolve");
        let registry_dir = tmp.join("r");
        let sock_dir = tmp.join("s");
        let w1 = MockWindow::bind(&registry_dir, &sock_dir, 5001, "alpha-window").await;
        let _w2 = MockWindow::bind(&registry_dir, &sock_dir, 5002, "alpha-other").await;
        let _ = &w1; // 仅为持有 socket 存活（join 于测试尾）
        let sender = PeerSender::new(
            registry_dir.clone(),
            tmp.join("own.sock"),
            "im-agent".into(),
        );
        // 精确名。
        let live = sender.list_windows().await;
        let names: Vec<&str> = live.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            ["alpha-window", "alpha-other"],
            "startedAt 倒序无关紧要，两条都在"
        );
        // 唯一前缀。
        let hits: Vec<String> = live
            .iter()
            .filter(|e| e.name.starts_with("alpha-w"))
            .map(|e| e.name.clone())
            .collect();
        assert_eq!(hits, ["alpha-window"]);
        // 歧义前缀报错并列名。
        let err = sender
            .send_to_window("alpha", "x")
            .await
            .expect_err("ambiguous");
        assert!(err.to_string().contains("匹配到多个"), "{err}");
        // 无匹配报错并列在线名单。
        let err = sender
            .send_to_window("ghost", "x")
            .await
            .expect_err("no match");
        let msg = err.to_string();
        assert!(
            msg.contains("ghost") && msg.contains("alpha-window"),
            "{msg}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn discover_socks_dir_prefers_entries_then_fallback() {
        // 有条目：取条目 sock 的 dirname。
        let tmp = tmp_dir("socksdir");
        let registry_dir = tmp.join("r");
        let sock_dir = tmp.join("custom-socks");
        std::fs::create_dir_all(&registry_dir).unwrap();
        std::fs::create_dir_all(&sock_dir).unwrap();
        write_entry(&registry_dir, 7001, "w", &sock_dir.join("7001.sock"), "cli");
        let dir = discover_socks_dir(&registry_dir).await;
        assert_eq!(dir, sock_dir, "发现自既有条目 dirname");
        // 无条目：回退 base/cc-socks（XDG_RUNTIME_DIR 或 /tmp）。
        let empty = tmp.join("empty-registry");
        std::fs::create_dir_all(&empty).unwrap();
        let dir = discover_socks_dir(&empty).await;
        let expected = std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "/tmp".to_string());
        assert_eq!(dir, Path::new(&expected).join("cc-socks"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---- v1.43 本地窗口看板 + Stop 钩子上报 ----

    /// 可控字段的注册表条目：缺省与真实交互窗口条目同构，`extra` 覆盖同名键
    /// （值为 `null` 的键整个删掉，用来制造缺字段的条目）。`sock` 为 None =
    /// 不写 messagingSocketPath（ACP 子进程 `sdk-ts` 的形态）。
    fn write_entry_with(
        dir: &Path,
        pid: u32,
        name: &str,
        sock: Option<&Path>,
        extra: &[(&str, serde_json::Value)],
    ) {
        std::fs::create_dir_all(dir).expect("create registry dir");
        let mut v = serde_json::json!({
            "pid": pid,
            "sessionId": format!("sess-{pid}"),
            "cwd": format!("/work/{name}"),
            "startedAt": 1_700_000_000_000i64 + i64::from(pid),
            "version": "claude",
            "peerProtocol": 1,
            "kind": "interactive",
            "entrypoint": "cli",
            "name": name,
            "nameSource": "derived",
            "status": "idle",
        });
        if let Some(sock) = sock {
            v["messagingSocketPath"] = serde_json::json!(sock.to_string_lossy());
        }
        for (k, val) in extra {
            if val.is_null() {
                v.as_object_mut().expect("entry is object").remove(*k);
            } else {
                v[*k] = val.clone();
            }
        }
        std::fs::write(dir.join(format!("{pid}.json")), v.to_string()).expect("write entry");
    }

    /// 绑一个由测试持有的活 socket（探活只需内核在 backlog 里接受连接，不必
    /// accept）。
    fn live_sock(path: &Path) -> tokio::net::UnixListener {
        let _ = std::fs::remove_file(path);
        tokio::net::UnixListener::bind(path).expect("bind live sock")
    }

    /// 窗口夹具：`startedAt = 100`、`statusUpdatedAt = ts`；waiting 带原因。
    fn win(name: &str, status: WindowStatus, ts: i64) -> WindowInfo {
        WindowInfo {
            name: name.into(),
            status,
            waiting_for: (status == WindowStatus::Waiting).then(|| "dialog open".to_string()),
            cwd: format!("/work/{name}"),
            started_at: Some(100),
            status_updated_at: Some(ts),
            watched: false,
        }
    }

    fn ev(name: &str, kind: WindowEventKind, waiting_for: Option<&str>, push: bool) -> WindowEvent {
        WindowEvent {
            name: name.into(),
            kind,
            waiting_for: waiting_for.map(str::to_string),
            push,
        }
    }

    /// 轮询等待 `pred` 在 5s 内成立（带后台任务的用例的时序断言）。
    async fn until(mut pred: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pred() {
            assert!(Instant::now() < deadline, "条件在 5s 内未满足");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// 注册表条目解析 `waitingFor` / `statusUpdatedAt`（字符串 / 整数才有值；
    /// 类型不符或缺键 → None；`status` 缺省 "unknown"）。
    #[test]
    fn entry_parses_waiting_for_and_status_updated_at() {
        let tmp = tmp_dir("parse");
        let dir = tmp.join("r");
        let sock = tmp.join("1.sock");
        write_entry_with(
            &dir,
            1,
            "w",
            Some(&sock),
            &[
                ("status", serde_json::json!("waiting")),
                ("waitingFor", serde_json::json!("dialog open")),
                ("statusUpdatedAt", serde_json::json!(1_795_352_000_000i64)),
            ],
        );
        // 缺字段 / 类型不符 / 缺 status 的条目。
        write_entry_with(
            &dir,
            2,
            "bare",
            Some(&sock),
            &[("status", serde_json::Value::Null)],
        );
        write_entry_with(
            &dir,
            3,
            "odd",
            Some(&sock),
            &[
                ("waitingFor", serde_json::json!(42)),
                ("statusUpdatedAt", serde_json::json!("yesterday")),
            ],
        );
        let rows: HashMap<u32, PeerEntry> = PeerRegistry::new(dir)
            .scan()
            .into_iter()
            .map(|e| (e.pid, e))
            .collect();
        let w = &rows[&1];
        assert_eq!(w.status, "waiting");
        assert_eq!(w.waiting_for.as_deref(), Some("dialog open"));
        assert_eq!(w.status_updated_at, Some(1_795_352_000_000));
        let bare = &rows[&2];
        assert_eq!(bare.status, "unknown", "缺 status 的条目");
        assert_eq!(
            (bare.waiting_for.as_deref(), bare.status_updated_at),
            (None, None)
        );
        let odd = &rows[&3];
        assert_eq!(
            (odd.waiting_for.as_deref(), odd.status_updated_at),
            (None, None),
            "类型不符按缺失处理"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `WindowStatus::from_registry`：大小写敏感精确匹配三值，其余 Unknown。
    #[test]
    fn window_status_from_registry_four_values() {
        use WindowStatus::*;
        for (raw, want) in [
            ("busy", Busy),
            ("idle", Idle),
            ("waiting", Waiting),
            ("Busy", Unknown),
            ("IDLE", Unknown),
            ("", Unknown),
            ("running", Unknown),
            ("waiting ", Unknown),
        ] {
            assert_eq!(WindowStatus::from_registry(raw), want, "{raw:?}");
        }
        // from_entry 透传 + watched 恒 false。
        let info = WindowInfo::from_entry(&PeerEntry {
            pid: 1,
            session_id: None,
            name: "n".into(),
            cwd: "/c".into(),
            status: "waiting".into(),
            kind: None,
            entrypoint: Some("cli".into()),
            started_at: Some(7),
            sock: String::new(),
            live: true,
            waiting_for: Some("input needed".into()),
            status_updated_at: Some(9),
        });
        assert_eq!(
            info,
            WindowInfo {
                name: "n".into(),
                status: Waiting,
                waiting_for: Some("input needed".into()),
                cwd: "/c".into(),
                started_at: Some(7),
                status_updated_at: Some(9),
                watched: false,
            }
        );
    }

    /// hook 帧：组帧键序逐字（struct 序列化）+ 解析往返；不合规形态归 Other。
    #[test]
    fn hook_frame_byte_shape_and_parse_rules() {
        let line = build_hook_frame_json("Stop", "sess-42", "/work", "本轮结论");
        assert_eq!(
            line,
            r#"{"type":"hook","hook":"Stop","sessionId":"sess-42","cwd":"/work","message":"本轮结论"}"#,
            "键序固定：type、hook、sessionId、cwd、message"
        );
        assert_eq!(
            parse_frame_line(&line),
            Some(PeerFrame::Hook {
                hook: "Stop".into(),
                session_id: "sess-42".into(),
                cwd: Some("/work".into()),
                message: "本轮结论".into(),
            })
        );
        // cwd 可缺省 / 可空。
        let no_cwd = r#"{"type":"hook","hook":"Stop","sessionId":"s","message":"m"}"#;
        assert_eq!(
            parse_frame_line(no_cwd),
            Some(PeerFrame::Hook {
                hook: "Stop".into(),
                session_id: "s".into(),
                cwd: None,
                message: "m".into(),
            })
        );
        // 其它事件名照样解析成 Hook（消费与否由 handle_conn 按 hook 名判定）。
        assert!(matches!(
            parse_frame_line(r#"{"type":"hook","hook":"Notification","sessionId":"s","message":"m"}"#),
            Some(PeerFrame::Hook { hook, .. }) if hook == "Notification"
        ));
        // sessionId 空 / message 空 / 缺字段 / 类型错 / type 非 hook → 一律不是 Hook。
        for bad in [
            r#"{"type":"hook","hook":"Stop","sessionId":"","message":"m"}"#,
            r#"{"type":"hook","hook":"Stop","sessionId":"s","message":""}"#,
            r#"{"type":"hook","hook":"Stop","message":"m"}"#,
            r#"{"type":"hook","hook":"Stop","sessionId":"s"}"#,
            r#"{"type":"hook","hook":"","sessionId":"s","message":"m"}"#,
            r#"{"type":"hook","hook":"Stop","sessionId":42,"message":"m"}"#,
            r#"{"type":"user","hook":"Stop","sessionId":"s","message":"m"}"#,
            r#"{"type":"control","hook":"Stop","sessionId":"s","message":"m"}"#,
        ] {
            assert!(
                !matches!(parse_frame_line(bad), Some(PeerFrame::Hook { .. })),
                "不应解析为 Hook：{bad}"
            );
        }
        assert_eq!(parse_frame_line("not json"), None);
    }

    /// peer-hook 客户端侧截断：总长（含标记）不超过上限，不劈半个字符。
    #[test]
    fn clip_hook_message_total_length_including_mark() {
        assert_eq!(clip_hook_message("短正文"), "短正文");
        let at_limit = "长".repeat(HOOK_MESSAGE_MAX_CHARS);
        assert_eq!(clip_hook_message(&at_limit), at_limit, "恰好到上限：不截");
        let clipped = clip_hook_message(&format!("{at_limit}尾巴"));
        assert_eq!(
            clipped.chars().count(),
            HOOK_MESSAGE_MAX_CHARS,
            "总长含标记不超过上限——网关侧只对超限正文加标记，这里不能超"
        );
        assert!(clipped.ends_with(HOOK_TRUNCATED_MARK));
        assert!(clipped.starts_with(&"长".repeat(100)));
    }

    /// 窗口白名单（契约 §0 实测事实）：`entrypoint == "cli"` 且 `kind != "bg"`。
    #[test]
    fn is_window_whitelist_combinations() {
        let entry = |entrypoint: Option<&str>, kind: Option<&str>| PeerEntry {
            pid: 1,
            session_id: None,
            name: "n".into(),
            cwd: String::new(),
            status: "idle".into(),
            kind: kind.map(str::to_string),
            entrypoint: entrypoint.map(str::to_string),
            started_at: None,
            sock: String::new(),
            live: true,
            waiting_for: None,
            status_updated_at: None,
        };
        // 是窗口：终端里的交互 claude（kind 缺省即 interactive）。
        assert!(entry(Some("cli"), None).is_window());
        assert!(entry(Some("cli"), Some("interactive")).is_window());
        // 不是窗口：后台作业；无头 claude -p（sdk-cli，带活套接字）；网关自身；
        // ACP 子进程；没见过的来源；大小写不符。
        assert!(!entry(Some("cli"), Some("bg")).is_window());
        assert!(!entry(Some("sdk-cli"), Some("interactive")).is_window());
        assert!(!entry(Some("imagent"), Some("interactive")).is_window());
        assert!(!entry(Some("sdk-ts"), Some("interactive")).is_window());
        assert!(!entry(Some("claude-desktop"), None).is_window());
        assert!(!entry(Some("CLI"), Some("interactive")).is_window());
        assert!(!entry(None, None).is_window());
        // 网关家族判定与窗口判定互斥。
        assert!(entry(Some("imagent"), None).is_gateway());
        assert!(!entry(Some("cli"), None).is_gateway());
    }

    /// `list_windows`（/peers、/to、出站投递的解析源）只列窗口：带活套接字的
    /// 无头 `sdk-cli`（网关自己用 claude-cli 引擎跑的每一轮）、后台作业、网关
    /// 自身、无套接字的 ACP 子进程都不在内——此前每轮都会闪出一行假窗口。
    #[tokio::test]
    async fn list_windows_whitelists_interactive_cli_only() {
        let tmp = tmp_dir("whitelist");
        let registry_dir = tmp.join("r");
        let _win = live_sock(&tmp.join("9701.sock"));
        let _headless = live_sock(&tmp.join("9702.sock"));
        let _gateway = live_sock(&tmp.join("9703.sock"));
        let _bg = live_sock(&tmp.join("9704.sock"));
        write_entry_with(
            &registry_dir,
            9701,
            "real-window",
            Some(&tmp.join("9701.sock")),
            &[("status", serde_json::json!("busy"))],
        );
        write_entry_with(
            &registry_dir,
            9702,
            "imagent-34",
            Some(&tmp.join("9702.sock")),
            &[
                ("entrypoint", serde_json::json!("sdk-cli")),
                ("status", serde_json::json!("busy")),
            ],
        );
        write_entry_with(
            &registry_dir,
            9703,
            "imagent",
            Some(&tmp.join("9703.sock")),
            &[("entrypoint", serde_json::json!("imagent"))],
        );
        write_entry_with(
            &registry_dir,
            9704,
            "bg-job",
            Some(&tmp.join("9704.sock")),
            &[("kind", serde_json::json!("bg"))],
        );
        write_entry_with(
            &registry_dir,
            9705,
            "acp-child",
            None,
            &[("entrypoint", serde_json::json!("sdk-ts"))],
        );
        let sender = PeerSender::new(registry_dir, tmp.join("own.sock"), "im-agent".into());
        let names: Vec<String> = sender
            .list_windows()
            .await
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["real-window"]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 钩子上报的窗口反查：只认 sessionId 相等、是窗口、套接字为活的条目；
    /// 多条取 startedAt 最新。网关自己的会话（sdk-cli / imagent）、后台作业、
    /// 已退出的窗口（死套接字）都查不到——这是钩子上报不被误当窗口结论的闸。
    #[tokio::test]
    async fn find_by_session_id_only_live_interactive_windows() {
        let tmp = tmp_dir("bysess");
        let registry_dir = tmp.join("r");
        let sock = |name: &str| tmp.join(name);
        let _win = live_sock(&sock("9801.sock"));
        let _gw = live_sock(&sock("9802.sock"));
        let _headless = live_sock(&sock("9803.sock"));
        let _bg = live_sock(&sock("9804.sock"));
        let _dup_new = live_sock(&sock("9805.sock"));
        let _dup_old = live_sock(&sock("9806.sock"));
        let sid = |s: &str| ("sessionId", serde_json::json!(s));
        write_entry_with(
            &registry_dir,
            9801,
            "win",
            Some(&sock("9801.sock")),
            &[sid("s-win")],
        );
        write_entry_with(
            &registry_dir,
            9802,
            "imagent",
            Some(&sock("9802.sock")),
            &[sid("s-gw"), ("entrypoint", serde_json::json!("imagent"))],
        );
        write_entry_with(
            &registry_dir,
            9803,
            "imagent-34",
            Some(&sock("9803.sock")),
            &[
                sid("s-headless"),
                ("entrypoint", serde_json::json!("sdk-cli")),
            ],
        );
        write_entry_with(
            &registry_dir,
            9804,
            "bg-job",
            Some(&sock("9804.sock")),
            &[sid("s-bg"), ("kind", serde_json::json!("bg"))],
        );
        write_entry_with(
            &registry_dir,
            9807,
            "gone",
            Some(&sock("dead.sock")),
            &[sid("s-dead")],
        );
        // 同一 sessionId 两个活窗口：startedAt = 基准 + pid，pid 大者更新。
        write_entry_with(
            &registry_dir,
            9805,
            "dup-older",
            Some(&sock("9805.sock")),
            &[sid("s-dup")],
        );
        write_entry_with(
            &registry_dir,
            9806,
            "dup-newer",
            Some(&sock("9806.sock")),
            &[sid("s-dup")],
        );

        let reg = PeerRegistry::new(registry_dir);
        let hit = reg.find_by_session_id("s-win").await.expect("活窗口应命中");
        assert_eq!((hit.name.as_str(), hit.live), ("win", true));
        for sid in ["s-gw", "s-headless", "s-bg", "s-dead", "", "no-such"] {
            assert!(
                reg.find_by_session_id(sid).await.is_none(),
                "{sid} 不是可上报的窗口"
            );
        }
        assert_eq!(
            reg.find_by_session_id("s-dup").await.map(|e| e.name),
            Some("dup-newer".into()),
            "多条命中取 startedAt 最新"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 注册表目录的环境变量覆盖（端到端验证的隔离手段）：非空值原样返回
    /// （不要求已存在）；空白 / 未设 → `~/.claude/sessions`。
    #[test]
    fn default_registry_dir_env_override() {
        let saved = std::env::var_os(REGISTRY_DIR_ENV);
        std::env::set_var(REGISTRY_DIR_ENV, "/tmp/ip-reg-override-not-created");
        assert_eq!(
            default_registry_dir(),
            Some(PathBuf::from("/tmp/ip-reg-override-not-created"))
        );
        std::env::set_var(REGISTRY_DIR_ENV, "   ");
        let blank = default_registry_dir();
        std::env::remove_var(REGISTRY_DIR_ENV);
        let unset = default_registry_dir();
        match saved {
            Some(v) => std::env::set_var(REGISTRY_DIR_ENV, v),
            None => std::env::remove_var(REGISTRY_DIR_ENV),
        }
        assert_eq!(blank, unset, "空白值等同未设");
        assert_eq!(
            unset,
            dirs::home_dir().map(|h| h.join(".claude").join("sessions"))
        );
    }

    /// `hookInstalled` 探测（与 CLI 的安装 / 卸载 / 状态共用条目识别）：只认
    /// 「网关自己的 peer-hook 条目」——别的 `--to` 不算，名字只是前缀相同
    /// （`imagent-2`）也不算（否则卸载 `imagent` 会误删另一台网关的条目）。
    #[test]
    fn hook_installed_detects_own_entry_only() {
        let tmp = tmp_dir("hooki");
        let settings = tmp.join("settings.json");
        assert!(!hook_installed(Some(&settings), "imagent"), "文件不存在");
        assert!(!hook_installed(None, "imagent"), "不探测");
        let hook = |cmd: &str| serde_json::json!({"type": "command", "command": cmd, "timeout": 5, "async": true});
        let write = |v: serde_json::Value| {
            std::fs::write(&settings, v.to_string()).unwrap();
        };
        // 已安装：用户另有 PreToolUse 组与别的 Stop 钩子，本网关条目在第二个
        // Stop 组里；命令前后带别的 shell 片段也能认出。
        write(serde_json::json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [hook("echo pre")]}],
                "Stop": [
                    {"hooks": [hook("echo bye")]},
                    {"hooks": [hook("cd /x && /usr/local/bin/imagent peer-hook --to imagent 2>/dev/null")]}
                ]
            }
        }));
        assert!(hook_installed(Some(&settings), "imagent"));
        // 装的是别的 --to。
        write(
            serde_json::json!({"hooks": {"Stop": [{"hooks": [hook("/bin/imagent peer-hook --to other")]}]}}),
        );
        assert!(!hook_installed(Some(&settings), "imagent"));
        // 前缀碰撞：--to imagent-2 不是 imagent。
        write(
            serde_json::json!({"hooks": {"Stop": [{"hooks": [hook("/bin/imagent peer-hook --to imagent-2")]}]}}),
        );
        assert!(!hook_installed(Some(&settings), "imagent"));
        // 缺可执行文件（peer-hook 在行首）/ 条目类型不是 command。
        write(
            serde_json::json!({"hooks": {"Stop": [{"hooks": [hook("peer-hook --to imagent")]}]}}),
        );
        assert!(!hook_installed(Some(&settings), "imagent"));
        write(
            serde_json::json!({"hooks": {"Stop": [{"hooks": [{"type": "prompt", "command": "/bin/imagent peer-hook --to imagent"}]}]}}),
        );
        assert!(!hook_installed(Some(&settings), "imagent"));
        // --to=<名> 同义。
        write(
            serde_json::json!({"hooks": {"Stop": [{"hooks": [hook("/bin/imagent peer-hook --to=imagent")]}]}}),
        );
        assert!(hook_installed(Some(&settings), "imagent"));
        // 解析失败 → false（探测是 best-effort）。
        std::fs::write(&settings, "{ not json").unwrap();
        assert!(!hook_installed(Some(&settings), "imagent"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 快照排序（帧字节可复现的前提）：startedAt 降序、缺失排最后、同值按名。
    #[test]
    fn sort_windows_started_desc_missing_last_then_name() {
        let mut old = win("aaa", WindowStatus::Idle, 1);
        old.started_at = None;
        let mut new = win("zzz", WindowStatus::Busy, 1);
        new.started_at = Some(300);
        let mut mid = win("mid", WindowStatus::Idle, 1);
        mid.started_at = Some(200);
        let mut ws = vec![
            win("b", WindowStatus::Idle, 1),
            old,
            win("a", WindowStatus::Idle, 1),
            new,
            mid,
        ];
        sort_windows(&mut ws);
        let names: Vec<&str> = ws.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["zzz", "mid", "a", "b", "aaa"]);
    }

    /// 快照「变化」判定只看契约的对比键（name / status / waitingFor / watched /
    /// statusUpdatedAt + 窗口集合）；排序在 PeerState 内部恒做，输入顺序无关。
    #[test]
    fn apply_snapshot_changed_only_on_contract_keys() {
        let s = PeerState::new();
        let t0 = Instant::now();
        assert!(
            s.apply_snapshot(&[win("a", WindowStatus::Idle, 1_000)], t0)
                .0,
            "首个快照：从无到有"
        );
        assert_eq!(
            s.apply_snapshot(&[win("a", WindowStatus::Idle, 1_000)], t0),
            (false, vec![]),
            "完全相同不算变化"
        );
        // 仅 statusUpdatedAt 变 → 变（一轮短到轮询没看到 busy，也靠它发现）。
        let a1 = win("a", WindowStatus::Idle, 1_001);
        assert!(s.apply_snapshot(&[a1.clone()], t0).0);
        // 仅 cwd 变 → 不算变化（对比键不含 cwd），但 latest 已更新。
        let mut a2 = a1.clone();
        a2.cwd = "/elsewhere".into();
        assert!(!s.apply_snapshot(std::slice::from_ref(&a2), t0).0);
        assert_eq!(s.windows()[0].cwd, "/elsewhere");
        // 仅 waitingFor 变 → 变。
        let mut a3 = a2.clone();
        a3.waiting_for = Some("input needed".into());
        assert!(s.apply_snapshot(std::slice::from_ref(&a3), t0).0);
        // 关注态变 → 变（帧里的 watched 要随关注名单收敛）。
        s.set_watched("a", true);
        assert!(s.apply_snapshot(std::slice::from_ref(&a3), t0).0);
        assert!(s.windows()[0].watched);
        // windows() 实时回填：取消关注后立刻读到 false。
        s.set_watched("a", false);
        assert!(!s.windows()[0].watched);
        assert!(!s.is_watched("a"));
        // 窗口集合变 → 变；输入乱序不算变化，latest 恒按规则排序。
        let mut b = win("b", WindowStatus::Busy, 1);
        b.started_at = Some(200);
        assert!(s.apply_snapshot(&[a3.clone(), b.clone()], t0).0, "新增窗口");
        assert!(
            s.apply_snapshot(std::slice::from_ref(&a3), t0).0,
            "窗口下线"
        );
        assert!(
            s.apply_snapshot(&[b.clone(), a3.clone()], t0).0,
            "窗口重新出现：集合变回去同样是变化"
        );
        assert!(
            !s.apply_snapshot(&[a3.clone(), b.clone()], t0).0,
            "顺序无关"
        );
        let latest = s.windows();
        let names: Vec<&str> = latest.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["b", "a"], "startedAt 大的在前");
    }

    /// 首个快照只建基线、不产生任何事件（否则网关每次重启都会把当时正卡在
    /// 等待的窗口各推一条「窗口在等你」）；运行期间新出现的 waiting 窗口才推。
    #[test]
    fn first_snapshot_primes_baseline_without_events() {
        let s = PeerState::new();
        let t0 = Instant::now();
        let (changed, events) = s.apply_snapshot(&[win("stuck", WindowStatus::Waiting, 1_000)], t0);
        assert_eq!(
            (changed, events),
            (true, vec![]),
            "重启时已卡住的窗口：不推"
        );
        assert_eq!(
            s.apply_snapshot(&[win("stuck", WindowStatus::Waiting, 1_000)], t0),
            (false, vec![])
        );
        // 重启时就 idle 的窗口：其后 idle → idle 且时间戳变大才算跑完一轮。
        let mut w = win("stuck", WindowStatus::Idle, 2_000);
        assert!(
            s.apply_snapshot(std::slice::from_ref(&w), t0).1.is_empty(),
            "waiting → idle 不产生事件"
        );
        w.status_updated_at = Some(3_000);
        assert_eq!(
            s.apply_snapshot(std::slice::from_ref(&w), t0).1,
            vec![ev("stuck", WindowEventKind::TurnEnd, None, false)]
        );
        // 运行期间新出现的窗口一上来就是 waiting → 仍按「进入 waiting」推。
        let (_, events) = s.apply_snapshot(&[w, win("newcomer", WindowStatus::Waiting, 1)], t0);
        assert_eq!(
            events,
            vec![ev(
                "newcomer",
                WindowEventKind::Waiting,
                Some("dialog open"),
                true
            )]
        );
    }

    /// 进入 waiting：上一次不是 waiting、这次是 → 事件（push 恒 true）；再次
    /// waiting 不重复；`waiting → idle` 不算跑完一轮（对话框是人在终端里处理
    /// 掉的）；`busy → waiting` 同样算进入。
    #[test]
    fn waiting_entered_once_and_waiting_to_idle_is_not_turn_end() {
        let s = PeerState::new();
        let t0 = Instant::now();
        s.apply_snapshot(&[win("w", WindowStatus::Idle, 1_000)], t0);
        let (changed, events) = s.apply_snapshot(&[win("w", WindowStatus::Waiting, 2_000)], t0);
        assert!(changed);
        assert_eq!(
            events,
            vec![ev("w", WindowEventKind::Waiting, Some("dialog open"), true)],
            "未关注、未对话过的窗口进入 waiting 也推"
        );
        assert_eq!(
            s.apply_snapshot(&[win("w", WindowStatus::Waiting, 2_000)], t0),
            (false, vec![]),
            "再次 waiting 不算进入"
        );
        let (changed, events) = s.apply_snapshot(&[win("w", WindowStatus::Idle, 3_000)], t0);
        assert!(changed, "快照变了");
        assert!(events.is_empty(), "waiting → idle 不是跑完一轮：{events:?}");
        s.apply_snapshot(&[win("w", WindowStatus::Busy, 3_500)], t0);
        let (_, events) = s.apply_snapshot(&[win("w", WindowStatus::Waiting, 4_000)], t0);
        assert_eq!(
            events,
            vec![ev("w", WindowEventKind::Waiting, Some("dialog open"), true)]
        );
    }

    /// 跑完一轮的两条判定（busy → idle；idle → idle 且时间戳变大）+ 推送闸门
    /// （仅关注 / 对话过的窗口）+ 30 秒节流；其余转换与同名重启不产生事件。
    #[test]
    fn turn_end_rules_push_gate_and_throttle() {
        let s = PeerState::new();
        let t0 = Instant::now();
        s.set_watched("watched", true);
        s.mark_contacted("contacted");
        // 快照按名升序（startedAt 相同）：contacted / plain / quiet / watched。
        let snap = |status_of: &dyn Fn(&str) -> WindowStatus, ts: i64| {
            ["contacted", "plain", "quiet", "watched"]
                .iter()
                .map(|n| win(n, status_of(n), ts))
                .collect::<Vec<_>>()
        };
        let at = |ts: i64| {
            snap(
                &|n| {
                    if n == "quiet" {
                        WindowStatus::Idle
                    } else {
                        WindowStatus::Busy
                    }
                },
                ts,
            )
        };
        s.apply_snapshot(&at(1_000), t0);
        // busy → idle：三个在跑的窗口各跑完一轮；闸门只放行关注 / 对话过的。
        let idle = at(2_000)
            .into_iter()
            .map(|mut w| {
                w.status = WindowStatus::Idle;
                w
            })
            .collect::<Vec<_>>();
        // 期望事件恒按快照序（名升序）；`gate` = 关注 / 对话过的窗口这次是否放行。
        let expect = |gate: bool| {
            vec![
                ev("contacted", WindowEventKind::TurnEnd, None, gate),
                ev("plain", WindowEventKind::TurnEnd, None, false),
                ev("quiet", WindowEventKind::TurnEnd, None, false),
                ev("watched", WindowEventKind::TurnEnd, None, gate),
            ]
        };
        // quiet 本就 idle：它的时间戳也变大了——同样判「跑完一轮」（不推送）。
        let events = s.apply_snapshot(&idle, t0).1;
        assert_eq!(events, expect(true));
        // idle → idle 且时间戳变大也算跑完一轮；节流窗口内事件照判、推送挡住。
        let bump = |ws: &[WindowInfo], ts: i64| {
            ws.iter()
                .cloned()
                .map(|mut w| {
                    w.status_updated_at = Some(ts);
                    w
                })
                .collect::<Vec<_>>()
        };
        let t10 = t0 + Duration::from_secs(10);
        let events = s.apply_snapshot(&bump(&idle, 3_000), t10).1;
        assert_eq!(events, expect(false), "30 秒节流内：事件照判、不重复推");
        let t31 = t0 + Duration::from_secs(31);
        let events = s.apply_snapshot(&bump(&idle, 4_000), t31).1;
        assert_eq!(events, expect(true), "节流窗口过后：闸门重新放行");
        // 时间戳没变的 idle → idle、进入 busy：不产生事件。
        assert!(s.apply_snapshot(&bump(&idle, 4_000), t31).1.is_empty());
        let busy = bump(&idle, 5_000)
            .into_iter()
            .map(|mut w| {
                w.status = WindowStatus::Busy;
                w
            })
            .collect::<Vec<_>>();
        assert!(
            s.apply_snapshot(&busy, t31).1.is_empty(),
            "进入 busy 不产生事件"
        );
        // 同名窗口重启（注册时刻变了）：新实例一上来就 idle 且时间戳更大，不是跑完一轮。
        let restarted = bump(&idle, 9_000)
            .into_iter()
            .map(|mut w| {
                w.started_at = Some(200);
                w
            })
            .collect::<Vec<_>>();
        assert!(
            s.apply_snapshot(&restarted, t31).1.is_empty(),
            "重启后的新实例没有「上一次」"
        );
        // should_push_turn_end（钩子上报路径共用）：未关注未对话 → false 且不占
        // 节流名额；关注后立即放行，29 秒内不放行，30 秒整放行。
        assert!(!s.should_push_turn_end("stranger", t0));
        s.set_watched("stranger", true);
        assert!(s.should_push_turn_end("stranger", t0));
        assert!(!s.should_push_turn_end("stranger", t0 + Duration::from_secs(29)));
        assert!(s.should_push_turn_end("stranger", t0 + Duration::from_secs(30)));
        // contacted 同样过闸（send_to_window 成功即登记）。
        s.mark_contacted("messaged");
        assert!(s.should_push_turn_end("messaged", t0));
    }

    /// 钩子上报与窗口回信的去重：180 秒内、归一化后互为前缀（取前 200 字符）。
    #[test]
    fn inbound_duplicate_window_and_prefix_rule() {
        let s = PeerState::new();
        let t0 = Instant::now();
        assert!(!s.inbound_duplicate("w", "任何正文", t0), "没有回信记录");
        s.note_inbound("w", "  已修好\n构建，\t测试全绿  ", t0);
        assert!(
            s.inbound_duplicate("w", "已修好 构建， 测试全绿", t0),
            "空白折叠后相同"
        );
        assert!(
            s.inbound_duplicate("w", "已修好 构建， 测试全绿。详情见下", t0),
            "钩子正文更长"
        );
        assert!(
            s.inbound_duplicate("w", "已修好", t0),
            "窗口只把结论段发了回来"
        );
        assert!(!s.inbound_duplicate("w", "构建失败了", t0), "不同内容");
        assert!(
            !s.inbound_duplicate("other", "已修好 构建， 测试全绿", t0),
            "别的窗口"
        );
        assert!(
            s.inbound_duplicate("w", "已修好 构建， 测试全绿", t0 + INBOUND_DEDUP_WINDOW),
            "180 秒整仍在窗口内"
        );
        assert!(
            !s.inbound_duplicate(
                "w",
                "已修好 构建， 测试全绿",
                t0 + INBOUND_DEDUP_WINDOW + Duration::from_millis(1)
            ),
            "超过 180 秒即过期"
        );
        // 只比前 200 字符：回信 300 字符，钩子正文前 200 字符相同、其后不同 → 重复；
        // 第 200 字符内就不同 → 不重复。
        s.note_inbound("long", &"长".repeat(300), t0);
        let mut same_head = "长".repeat(200);
        same_head.push_str("后续完全不同的内容");
        assert!(s.inbound_duplicate("long", &same_head, t0));
        let mut diff_head = "长".repeat(199);
        diff_head.push('短');
        assert!(!s.inbound_duplicate("long", &diff_head, t0));
        assert_eq!(normalize_ws("  a \n b\t\tc  "), "a b c");
    }

    /// 钩子上报的注入节流：每个窗口 20 秒内最多一次（纯判定 + 显式登记）。
    #[test]
    fn report_inject_throttle_per_window() {
        let s = PeerState::new();
        let t0 = Instant::now();
        assert!(s.report_inject_allowed("w", t0));
        assert!(
            s.report_inject_allowed("w", t0),
            "只判定不登记：没注入就不占名额"
        );
        s.note_report_injected("w", t0);
        assert!(!s.report_inject_allowed("w", t0 + Duration::from_secs(19)));
        assert!(s.report_inject_allowed("w", t0 + Duration::from_secs(20)));
        assert!(s.report_inject_allowed("other", t0), "节流按窗口计");
    }

    /// hookInstalled 状态：set 返回是否翻转（轮询任务据此补推一帧）。
    #[test]
    fn hook_installed_flag_flip_is_reported_once() {
        let s = PeerState::new();
        assert!(!s.hook_installed());
        assert!(!s.set_hook_installed(false), "值没变不算翻转");
        assert!(s.set_hook_installed(true));
        assert!(s.hook_installed());
        assert!(!s.set_hook_installed(true));
        assert!(s.set_hook_installed(false));
    }

    /// 轮询防抖的单轮合并逻辑（逐轮精确断言——集成用例的时序掐不到轮）：
    /// 条目文件在但解析失败 → 沿用上一轮信息最多 3 轮；条目文件没了 → 连续
    /// 2 轮才移除；同名的新条目优先于被沿用的旧实例。
    #[cfg(unix)]
    #[tokio::test]
    async fn watcher_snapshot_debounces_parse_failure_and_absence() {
        let tmp = tmp_dir("debounce");
        let registry_dir = tmp.join("r");
        let entry_path = registry_dir.join("9603.json");
        let sock = tmp.join("9603.sock");
        let listener = live_sock(&sock);
        let write_window = |status: &str| {
            write_entry_with(
                &registry_dir,
                9603,
                "win-d",
                Some(&sock),
                &[("status", serde_json::json!(status))],
            );
        };
        let mut watcher = WindowWatcher {
            registry: PeerRegistry::new(registry_dir.clone()),
            state: Arc::new(PeerState::new()),
            settings_path: None,
            peer_name: "im-agent".into(),
            tracked: HashMap::new(),
            miss: HashMap::new(),
            last_hook_check: Instant::now(),
            hook_refresh: HOOK_STATUS_REFRESH,
        };
        // 概要形态 "名字:状态"（元组不能跨 String / &str 比较）。
        fn brief(ws: &[WindowInfo]) -> Vec<String> {
            use WindowStatus::*;
            ws.iter()
                .map(|w| {
                    let status = match w.status {
                        Busy => "busy",
                        Idle => "idle",
                        Waiting => "waiting",
                        Unknown => "unknown",
                    };
                    format!("{}:{status}", w.name)
                })
                .collect()
        }

        write_window("busy");
        assert_eq!(brief(&watcher.snapshot().await), ["win-d:busy"]);
        // 文件瞬时损坏（非法 JSON）：沿用上一轮信息，最多连续 3 轮。
        std::fs::write(&entry_path, "{ not json").unwrap();
        for round in 1..=CARRY_ON_PARSE_FAIL_ROUNDS {
            assert_eq!(
                brief(&watcher.snapshot().await),
                ["win-d:busy"],
                "解析失败第 {round} 轮：沿用"
            );
        }
        assert!(watcher.snapshot().await.is_empty(), "第 4 轮：移除");
        // 恢复后重新出现（按新状态）。
        write_window("idle");
        assert_eq!(brief(&watcher.snapshot().await), ["win-d:idle"]);
        // 文件还在但套接字探活失败：同样按解析失败沿用 3 轮。
        drop(listener);
        for _ in 0..CARRY_ON_PARSE_FAIL_ROUNDS {
            assert_eq!(brief(&watcher.snapshot().await), ["win-d:idle"]);
        }
        assert!(watcher.snapshot().await.is_empty());
        // 条目缺席（文件没了）：第 1 轮先保留，第 2 轮移除。
        let listener = live_sock(&sock);
        write_window("idle");
        watcher.snapshot().await;
        std::fs::remove_file(&entry_path).unwrap();
        drop(listener);
        assert_eq!(
            brief(&watcher.snapshot().await),
            ["win-d:idle"],
            "缺席第 1 轮：保留"
        );
        assert!(watcher.snapshot().await.is_empty(), "缺席第 2 轮：移除");
        // 同名新条目优先：旧 pid 的文件没了（正被沿用）、新 pid 已在线 → 只剩新实例。
        let listener = live_sock(&sock);
        write_window("idle");
        watcher.snapshot().await;
        let new_sock = tmp.join("9604.sock");
        let _new_listener = live_sock(&new_sock);
        write_entry_with(
            &registry_dir,
            9604,
            "win-d",
            Some(&new_sock),
            &[("status", serde_json::json!("busy"))],
        );
        std::fs::remove_file(&entry_path).unwrap();
        drop(listener);
        assert_eq!(
            brief(&watcher.snapshot().await),
            ["win-d:busy"],
            "沿用中的旧实例不得与新实例并列"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 轮询主循环（真实注册表 + 真实探活）：首个快照建基线 → 状态变化推快照与
    /// 事件 → 文件瞬时损坏一轮又恢复时快照不抖动 → 关注后 watched 随下一轮
    /// 收敛 → busy → idle 推 TurnEnd、30 秒节流内再判一次不推 → 删除条目两轮后
    /// 移除 → 装上钩子后 hookInstalled 翻转补推一帧。
    #[cfg(unix)]
    #[tokio::test]
    async fn watch_loop_snapshots_events_and_debounce() {
        let tmp = tmp_dir("watch");
        let registry_dir = tmp.join("r");
        let win_sock = tmp.join("9601.sock");
        let _win_listener = live_sock(&win_sock);
        let write_window = |status: &str, ts: i64, waiting: Option<&str>| {
            let mut extra = vec![
                ("status", serde_json::json!(status)),
                ("statusUpdatedAt", serde_json::json!(ts)),
            ];
            if let Some(w) = waiting {
                extra.push(("waitingFor", serde_json::json!(w)));
            }
            write_entry_with(&registry_dir, 9601, "win-live", Some(&win_sock), &extra);
        };
        write_window("busy", 1_000, None);

        type Snap = (Vec<WindowInfo>, Vec<WindowEvent>);
        let log: Arc<StdMutex<Vec<Snap>>> = Arc::new(StdMutex::new(Vec::new()));
        let cb_log = log.clone();
        let state = Arc::new(PeerState::new());
        let shutdown = CancellationToken::new();
        // 间隔取 100ms：下面的「损坏 120ms」至多跨 2–3 个 tick，稳落在 3 轮沿用
        // 上限之内（不会因为机器卡顿多跑一轮而误判成「移除」）。
        let interval = Duration::from_millis(100);
        let task = tokio::spawn(run_watch(
            WindowWatcher {
                registry: PeerRegistry::new(registry_dir.clone()),
                state: state.clone(),
                settings_path: Some(tmp.join("settings.json")),
                peer_name: "im-agent".into(),
                tracked: HashMap::new(),
                miss: HashMap::new(),
                last_hook_check: Instant::now(),
                hook_refresh: interval,
            },
            interval,
            shutdown.clone(),
            Arc::new(move |windows, events| cb_log.lock().unwrap().push((windows, events))),
        ));
        let count = || log.lock().unwrap().len();
        // 最近一次回调（克隆出来断言，不跨 await 持锁）。
        let last = || log.lock().unwrap().last().cloned();
        // 最近一次回调的窗口概要：(窗口名, 状态, 关注态)。
        let brief = || {
            last().map(|(ws, _)| {
                ws.iter()
                    .map(|w| (w.name.clone(), w.status, w.watched))
                    .collect::<Vec<_>>()
            })
        };
        let last_events = || last().map(|(_, events)| events).unwrap_or_default();
        let entry_path = registry_dir.join("9601.json");

        // ① 首个快照建基线：只回调一次，不产生事件。
        until(|| count() >= 1).await;
        assert_eq!(
            brief(),
            Some(vec![("win-live".into(), WindowStatus::Busy, false)])
        );
        assert_eq!(last_events(), vec![], "基线不产生事件");

        // ② busy → waiting：推变化后的快照 + Waiting 事件（未关注也推）。
        write_window("waiting", 2_000, Some("dialog open"));
        until(|| !last_events().is_empty()).await;
        assert_eq!(
            last_events(),
            vec![ev(
                "win-live",
                WindowEventKind::Waiting,
                Some("dialog open"),
                true
            )]
        );
        assert_eq!(
            last().unwrap().0[0].waiting_for.as_deref(),
            Some("dialog open")
        );

        // ③ 文件瞬时损坏一轮又恢复：快照不抖动——期间不应有任何回调。
        let before = count();
        std::fs::write(&entry_path, "{ not json").unwrap();
        tokio::time::sleep(interval * 2 + Duration::from_millis(20)).await;
        write_window("waiting", 2_000, Some("dialog open"));
        tokio::time::sleep(interval * 3).await;
        assert_eq!(count(), before, "损坏 120ms 后恢复：无回调、无事件");
        assert_eq!(state.windows().len(), 1, "窗口仍在快照里");

        // ④ 关注：下一轮快照的 watched 收敛为 true（快照变化 → 回调，无事件）。
        state.set_watched("win-live", true);
        until(|| brief().is_some_and(|b| b[0].2)).await;
        assert!(last_events().is_empty());

        // ⑤ waiting → idle：快照变、无事件（对话框是人在终端里处理掉的）。
        write_window("idle", 3_000, None);
        until(|| brief().is_some_and(|b| b[0].1 == WindowStatus::Idle)).await;
        assert!(last_events().is_empty(), "{:?}", last_events());

        // ⑥ busy → idle：TurnEnd，关注过 → push。
        write_window("busy", 3_500, None);
        until(|| brief().is_some_and(|b| b[0].1 == WindowStatus::Busy)).await;
        write_window("idle", 4_000, None);
        until(|| !last_events().is_empty()).await;
        assert_eq!(
            last_events(),
            vec![ev("win-live", WindowEventKind::TurnEnd, None, true)]
        );

        // ⑦ idle → idle 且时间戳变大：30 秒节流内再判一次——事件照判、不推。
        write_window("idle", 4_500, None);
        until(|| last_events() == vec![ev("win-live", WindowEventKind::TurnEnd, None, false)])
            .await;

        // ⑧ 删除条目：两轮后移除（空快照，无事件）。
        std::fs::remove_file(&entry_path).unwrap();
        until(|| brief().is_some_and(|b| b.is_empty())).await;
        assert!(last_events().is_empty());
        assert!(state.windows().is_empty());

        // ⑨ 装上钩子：hookInstalled 翻转 → 补推一帧（快照本身没变）。
        let before = count();
        std::fs::write(
            tmp.join("settings.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"/usr/local/bin/imagent peer-hook --to im-agent","timeout":5,"async":true}]}]}}"#,
        )
        .unwrap();
        until(|| state.hook_installed()).await;
        until(|| count() > before).await;
        assert_eq!(last(), Some((vec![], vec![])), "翻转补推：快照不变、无事件");

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("停机信号应让轮询任务退出")
            .expect("轮询任务不 panic");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// hook 帧入站：网关收 Stop 上报 → `on_report` 恰被调一次（session_id /
    /// message 原样），连接上**不回写任何字节**（与 user 帧同规——客户端同样
    /// 「写完即半关闭、等对端关闭」）；同一条连接里的非 Stop hook 帧、不合规
    /// hook 帧与探活连接都不触发回调。
    #[tokio::test]
    async fn server_hook_frame_silent_and_reports_once() {
        let tmp = tmp_dir("hook");
        let registry_dir = tmp.join("r");
        let sock_dir = tmp.join("s");
        std::fs::create_dir_all(&registry_dir).unwrap();
        std::fs::create_dir_all(&sock_dir).unwrap();
        // sock 目录发现取既有条目的 dirname（空注册表会回退到真实 /tmp/cc-socks）。
        write_entry(
            &registry_dir,
            9002,
            "probe-window",
            &sock_dir.join("9002.sock"),
            "cli",
        );
        let reports: Arc<StdMutex<Vec<(String, String)>>> = Arc::new(StdMutex::new(Vec::new()));
        let reports_cb = reports.clone();
        let server = PeerServer::start(PeerServerConfig {
            settings_path: None,
            watch_interval: None,
            on_report: Arc::new(move |session_id, message| {
                let r = reports_cb.clone();
                Box::pin(async move { r.lock().unwrap().push((session_id, message)) })
            }),
            ..server_config(
                &registry_dir,
                tmp.clone(),
                CancellationToken::new(),
                Arc::new(|_, _| Box::pin(async {})),
            )
        })
        .await
        .expect("server start");
        let gw_sock = server.sender().own_sock().to_string_lossy().into_owned();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // 发帧连接：一条连接里依次写 Stop 帧、非 Stop 的 hook 帧、不合规 hook 帧
        // （message 为空），然后半关闭。
        let mut conn = tokio::net::UnixStream::connect(&gw_sock)
            .await
            .expect("connect gw");
        let mut wire = String::new();
        for line in [
            build_hook_frame_json("Stop", "sess-42", "/work", "本轮结论"),
            build_hook_frame_json("Notification", "sess-42", "/work", "别的事件"),
            build_hook_frame_json("Stop", "sess-42", "/work", ""),
        ] {
            wire.push_str(&line);
            wire.push('\n');
        }
        conn.write_all(wire.as_bytes()).await.unwrap();
        conn.shutdown().await.expect("frame conn shutdown");
        let mut sink = Vec::new();
        let n = tokio::time::timeout(SEND_TIMEOUT, conn.read_to_end(&mut sink))
            .await
            .expect("发帧连接：网关应在客户端 5s 预算内关闭")
            .expect("conn read_to_end");
        assert!(
            n == 0 && sink.is_empty(),
            "hook 帧连接不得收到任何字节：{:?}",
            String::from_utf8_lossy(&sink)
        );

        // 探活连接：连上、不写数据、立刻关闭——同样 0 字节 + EOF。
        let mut probe = tokio::net::UnixStream::connect(&gw_sock)
            .await
            .expect("connect gw (probe)");
        probe.shutdown().await.expect("probe shutdown");
        let mut probe_sink = Vec::new();
        let probe_n = tokio::time::timeout(SEND_TIMEOUT, probe.read_to_end(&mut probe_sink))
            .await
            .expect("探活连接：网关应在客户端预算内关闭")
            .expect("probe read_to_end");
        assert!(
            probe_n == 0 && probe_sink.is_empty(),
            "探活连接不得收到任何字节"
        );

        until(|| !reports.lock().unwrap().is_empty()).await;
        // 再留一小段静默期：收尾不得补出第二次回调。
        tokio::time::sleep(Duration::from_millis(150)).await;
        {
            let all = reports.lock().unwrap();
            assert_eq!(
                all.as_slice(),
                &[("sess-42".to_string(), "本轮结论".to_string())],
                "只消费 hook == Stop 的合规帧，恰一次"
            );
        } // guard 块内结束——不跨 shutdown await（await_holding_lock）
        server.shutdown().await;
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PeerServer 装配：`state()` 与出站句柄共享同一份共享态；`hookInstalled`
    /// 起始值在启动时同步算好（不依赖轮询是否开启）；不轮询时没有轮询任务；
    /// 投递成功即登记 contacted。
    #[tokio::test]
    async fn server_shares_state_and_computes_hook_installed_at_start() {
        let tmp = tmp_dir("srvstate");
        let registry_dir = tmp.join("r");
        let sock_dir = tmp.join("s");
        std::fs::create_dir_all(&registry_dir).unwrap();
        std::fs::create_dir_all(&sock_dir).unwrap();
        write_entry(
            &registry_dir,
            9004,
            "probe-window",
            &sock_dir.join("9004.sock"),
            "cli",
        );
        let settings = tmp.join("settings.json");
        std::fs::write(
            &settings,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"/usr/local/bin/imagent peer-hook --to im-agent"}]}]}}"#,
        )
        .unwrap();
        let server = PeerServer::start(PeerServerConfig {
            settings_path: Some(settings),
            watch_interval: None,
            ..server_config(
                &registry_dir,
                tmp.clone(),
                CancellationToken::new(),
                Arc::new(|_, _| Box::pin(async {})),
            )
        })
        .await
        .expect("server start");
        assert!(
            Arc::ptr_eq(&server.state(), server.sender().state()),
            "server.state() 与 sender 持有的共享态是同一份"
        );
        assert!(server.state().hook_installed(), "启动时即探测到已安装");
        assert!(
            server.watch_task.is_none(),
            "watch_interval = None 不启动轮询"
        );
        // 投递成功即登记 contacted（共享态生效）。mock 窗口收帧后关闭连接——
        // 只绑不 accept 的 socket 会让 send_frame 等不到对端关闭而超时。
        let _window = MockWindow::bind(&registry_dir, &sock_dir, 9005, "win-e").await;
        server
            .sender()
            .send_to_window("win-e", "派活")
            .await
            .expect("send to window");
        assert_eq!(_window.line_count(), 1, "窗口收到恰一帧");
        assert!(
            server.state().should_push_turn_end("win-e", Instant::now()),
            "对话过的窗口过推送闸门"
        );
        assert!(
            !server
                .state()
                .should_push_turn_end("never-contacted", Instant::now()),
            "没对话过也没关注的窗口不过闸"
        );
        server.shutdown().await;
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---- v1.43 R2-8 关注集合由网关持久化（store config 表 peer_watched）----

    fn names_of(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// 编解码：往返、排序、去重、去空名；畸形输入（非 JSON / 非数组 / 含非字符
    /// 串项）整体当空、不 panic——读坏一个 KV 不该让关注集合变成半截。
    #[test]
    fn watch_list_codec_round_trip_and_malformed() {
        let names = names_of(&["win-b", "win-a", "win-b", ""]);
        assert_eq!(
            encode_watch_list(&names),
            r#"["win-a","win-b"]"#,
            "排序、去重、去空名"
        );
        assert_eq!(encode_watch_list(&[]), "[]");
        assert_eq!(
            decode_watch_list(r#"["win-b","win-a","win-b"]"#),
            names_of(&["win-a", "win-b"]),
            "解码结果排序去重"
        );
        assert_eq!(
            decode_watch_list(&encode_watch_list(&names)),
            names_of(&["win-a", "win-b"]),
            "往返无损"
        );
        assert_eq!(
            decode_watch_list(r#"["win-a",""]"#),
            names_of(&["win-a"]),
            "空名项丢弃"
        );
        assert_eq!(decode_watch_list("[]"), Vec::<String>::new());
        assert_eq!(decode_watch_list(" [] \n"), Vec::<String>::new());
        for bad in [
            "not json",
            "",
            "{",
            r#"{"win-a":true}"#,
            r#"["win-a",1]"#,
            r#"["win-a",null]"#,
            "[1,2]",
            r#""win-a""#,
            "null",
            "123",
        ] {
            assert_eq!(
                decode_watch_list(bad),
                Vec::<String>::new(),
                "畸形输入整体当空：{bad:?}"
            );
        }
        // 键名是契约的一部分（前端任务 / 文档都引用它）。
        assert_eq!(WATCH_CONFIG_KEY, "peer_watched");
    }

    /// `set_watched` 的回调规则：新增 / 移除各回调一次，入参是排序后的完整名单；
    /// 设成已有的值不回调；回调在锁外调用（回调里能同步再读 `PeerState`）。
    #[test]
    fn set_watched_notifies_only_real_changes_outside_lock() {
        let state = Arc::new(PeerState::new());
        let calls: Arc<StdMutex<Vec<Vec<String>>>> = Arc::new(StdMutex::new(Vec::new()));
        let weak = Arc::downgrade(&state);
        let calls_cb = calls.clone();
        state.set_watch_changed_callback(Some(Arc::new(move |names: Vec<String>| {
            let state = weak.upgrade().expect("state alive");
            // 回调若在持锁时被调，这里的 try_lock 会失败——用断言暴露，而不是
            // 让下面的 watched_names() 死锁把测试挂住。
            assert!(state.inner.try_lock().is_some(), "回调不得在持锁时调用");
            assert_eq!(names, state.watched_names(), "回调入参 = 当前完整名单");
            calls_cb.lock().unwrap().push(names);
        })));
        let recorded = || calls.lock().unwrap().clone();

        state.set_watched("win-b", true);
        state.set_watched("win-a", true);
        assert_eq!(
            recorded(),
            vec![names_of(&["win-b"]), names_of(&["win-a", "win-b"])],
            "新增各回调一次，入参为排序后的完整名单"
        );
        state.set_watched("win-b", false);
        assert_eq!(recorded().pop(), Some(names_of(&["win-a"])), "移除回调一次");

        // 设成已有的值（重复关注 / 取消关注一个没关注的名字）：不回调。
        state.set_watched("win-a", true);
        state.set_watched("win-a", true);
        state.set_watched("never-watched", false);
        assert_eq!(recorded().len(), 3, "同值不回调");
        assert!(state.is_watched("win-a") && !state.is_watched("win-b"));

        // 卸下回调后不再通知；没装回调的实例（watch_store = None / 直接 new 的
        // 出站句柄）照常工作。
        state.set_watch_changed_callback(None);
        state.set_watched("win-c", true);
        assert_eq!(recorded().len(), 3, "卸下后不回调");
        assert_eq!(state.watched_names(), names_of(&["win-a", "win-c"]));
        let plain = PeerState::new();
        plain.set_watched("x", true);
        assert!(plain.is_watched("x"));
        assert_eq!(plain.watched_names(), names_of(&["x"]));
    }

    /// R2-8 用例的 server 配置：关注名单落点 = `db`（每次调用重新打开同一个库，
    /// 即「新进程」语义），其余同收帧测试形态。
    async fn persisted_config(
        registry_dir: &Path,
        cwd: PathBuf,
        db: &Path,
        interval: Option<Duration>,
        shutdown: CancellationToken,
        on_report: PeerReportCallback,
        on_windows: PeerWindowsCallback,
    ) -> PeerServerConfig {
        PeerServerConfig {
            settings_path: None,
            watch_interval: interval,
            watch_store: Some(Arc::new(
                imagent_store::Store::open(db)
                    .await
                    .expect("open watch store"),
            )),
            on_report,
            on_windows,
            shutdown,
            ..server_config(
                registry_dir,
                cwd,
                CancellationToken::new(),
                Arc::new(|_, _| Box::pin(async {})),
            )
        }
    }

    /// 用一次全新的库连接读当前落盘的关注名单。
    async fn stored_watch_list(db: &Path) -> Option<String> {
        let store = imagent_store::Store::open(db).await.expect("reopen store");
        store
            .get_config(WATCH_CONFIG_KEY)
            .await
            .expect("read config")
    }

    /// 等落盘的名单变成 `want`（落盘泵是后台任务；5s 内不达即失败）。
    async fn wait_stored_watch_list(db: &Path, want: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let stored = stored_watch_list(db).await;
            if stored.as_deref() == Some(want) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "关注名单未在 5s 内落盘为 {want}，当前 = {stored:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// 持久化往返：关注 → 落盘 → 「新进程」第二次启动 → `start()` 返回时名单
    /// 已加载、**首个快照**里该窗口已是关注态（加载先于轮询）；取消关注与
    /// 快速连点后，落盘的是最后一次的名单。
    #[tokio::test]
    async fn server_persists_watch_list_across_restart() {
        let tmp = tmp_dir("persistw");
        let db = tmp.join("imagent.db");
        let registry_dir = tmp.join("r");
        std::fs::create_dir_all(&registry_dir).unwrap();
        let _win = live_sock(&tmp.join("9801.sock"));
        write_entry_with(
            &registry_dir,
            9801,
            "win-b",
            Some(&tmp.join("9801.sock")),
            &[],
        );
        type Snap = (Vec<WindowInfo>, Vec<WindowEvent>);
        let snapshots: Arc<StdMutex<Vec<Snap>>> = Arc::new(StdMutex::new(Vec::new()));
        let cb_snapshots = snapshots.clone();
        let on_windows: PeerWindowsCallback = Arc::new(move |windows, events| {
            cb_snapshots.lock().unwrap().push((windows, events));
        });
        let noop: PeerReportCallback = Arc::new(|_, _| Box::pin(async {}));

        // 第一次运行（不轮询）：关注 win-b → 落盘。启动本身不写库。
        let shutdown = CancellationToken::new();
        let first = PeerServer::start(
            persisted_config(
                &registry_dir,
                tmp.clone(),
                &db,
                None,
                shutdown.clone(),
                noop.clone(),
                on_windows.clone(),
            )
            .await,
        )
        .await
        .expect("first start");
        assert_eq!(
            stored_watch_list(&db).await,
            None,
            "启动只加载、不回写（初值即落盘状态）"
        );
        first.state().set_watched("win-b", true);
        // 不置位停机令牌就 shutdown：泵靠「回调被卸下」即时收尾（会话过期等退出
        // 路径上令牌并不置位），且 2s 内返回。
        first.shutdown().await;
        assert_eq!(
            stored_watch_list(&db).await.as_deref(),
            Some(r#"["win-b"]"#)
        );

        // 第二次运行（重新打开同一个库 = 新进程语义；开轮询）。
        let shutdown = CancellationToken::new();
        let second = PeerServer::start(
            persisted_config(
                &registry_dir,
                tmp.clone(),
                &db,
                Some(Duration::from_millis(40)),
                shutdown.clone(),
                noop,
                on_windows,
            )
            .await,
        )
        .await
        .expect("second start");
        assert!(
            second.state().is_watched("win-b"),
            "start() 返回时持久化的关注名单已加载"
        );
        until(|| !snapshots.lock().unwrap().is_empty()).await;
        let first_snapshot = snapshots.lock().unwrap().remove(0).0;
        assert_eq!(
            first_snapshot.len(),
            1,
            "只有 win-b 一个窗口：{first_snapshot:?}"
        );
        assert!(
            first_snapshot[0].watched,
            "加载先于轮询：首个快照里 win-b 已是关注态"
        );
        // 运行期间的正常落盘（后台泵异步写）。
        second.state().set_watched("win-z", true);
        wait_stored_watch_list(&db, r#"["win-b","win-z"]"#).await;
        // 取消关注、关注别的又取消：落盘以最后一次为准（单写者保序）。不等
        // 落盘、不置位停机令牌，立刻 shutdown——收尾必须把还没写的变化补写完。
        second.state().set_watched("win-b", false);
        second.state().set_watched("win-z", false);
        second.state().set_watched("win-c", true);
        second.state().set_watched("win-c", false);
        second.shutdown().await;
        assert_eq!(
            stored_watch_list(&db).await.as_deref(),
            Some("[]"),
            "shutdown 收尾补写未落盘的变化，且是最后一次的名单"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 加载读到畸形值：只 warn、集合按空集；轮询（首个快照照常到达、watched 为
    /// false）与握手（hook 帧照常进 on_report）都不受影响——不因一个坏 KV 让
    /// peer 通道起不来。
    #[tokio::test]
    async fn server_tolerates_malformed_persisted_watch_list() {
        for raw in ["{ not json", r#"{"win-b":true}"#, r#"["win-b",42]"#] {
            let tmp = tmp_dir("badwatch");
            let db = tmp.join("imagent.db");
            let seed = imagent_store::Store::open(&db)
                .await
                .expect("open seed store");
            seed.set_config(WATCH_CONFIG_KEY, raw).await.expect("seed");
            drop(seed);
            let registry_dir = tmp.join("r");
            std::fs::create_dir_all(&registry_dir).unwrap();
            let _win = live_sock(&tmp.join("9802.sock"));
            write_entry_with(
                &registry_dir,
                9802,
                "win-b",
                Some(&tmp.join("9802.sock")),
                &[],
            );
            let snapshots: Arc<StdMutex<usize>> = Arc::new(StdMutex::new(0));
            let cb_snapshots = snapshots.clone();
            // 首个快照里 win-b 是否被关注（畸形值 → 必须是 false）。
            let first_watched = Arc::new(AtomicBool::new(false));
            let cb_first_watched = first_watched.clone();
            let on_windows: PeerWindowsCallback = Arc::new(move |windows, _events| {
                let mut count = cb_snapshots.lock().unwrap();
                *count += 1;
                if *count == 1 {
                    cb_first_watched
                        .store(windows.first().is_some_and(|w| w.watched), Ordering::SeqCst);
                }
            });
            let reports: Arc<StdMutex<Vec<(String, String)>>> = Arc::new(StdMutex::new(Vec::new()));
            let reports_cb = reports.clone();
            let on_report: PeerReportCallback = Arc::new(move |session_id, message| {
                let r = reports_cb.clone();
                Box::pin(async move { r.lock().unwrap().push((session_id, message)) })
            });
            let shutdown = CancellationToken::new();
            let server = PeerServer::start(
                persisted_config(
                    &registry_dir,
                    tmp.clone(),
                    &db,
                    Some(Duration::from_millis(40)),
                    shutdown.clone(),
                    on_report,
                    on_windows,
                )
                .await,
            )
            .await
            .expect("server start");
            assert!(
                !server.state().is_watched("win-b"),
                "畸形值 {raw:?} 按空集处理"
            );
            until(|| *snapshots.lock().unwrap() > 0).await;
            assert!(
                !first_watched.load(Ordering::SeqCst),
                "轮询照常：首个快照里 win-b 未被关注（{raw:?}）"
            );
            // 握手照常：hook 帧照样进 on_report（连接上仍不回写）。
            let gw_sock = server.sender().own_sock().to_string_lossy().into_owned();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut conn = tokio::net::UnixStream::connect(&gw_sock)
                .await
                .expect("connect gw");
            conn.write_all(build_hook_frame_json("Stop", "sess-1", "/w", "结论").as_bytes())
                .await
                .unwrap();
            conn.write_all(b"\n").await.unwrap();
            conn.shutdown().await.unwrap();
            let mut sink = Vec::new();
            let n = tokio::time::timeout(SEND_TIMEOUT, conn.read_to_end(&mut sink))
                .await
                .expect("网关应在客户端预算内关闭")
                .expect("read_to_end");
            assert!(n == 0 && sink.is_empty(), "hook 帧连接不回写");
            until(|| !reports.lock().unwrap().is_empty()).await;
            assert_eq!(
                reports.lock().unwrap().as_slice(),
                &[("sess-1".to_string(), "结论".to_string())],
                "握手照常"
            );
            shutdown.cancel();
            server.shutdown().await;
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }

    /// 「加载先于轮询」的最强形态：单线程运行时里 `start()` 一返回（中间不让出
    /// 执行权）就读得到持久化的关注态。把加载挪成后台任务（与轮询并发）时，
    /// 这里读到的还是空集——那条路径上首个快照可能把已关注的窗口报成未关注。
    #[tokio::test(flavor = "current_thread")]
    async fn server_has_watch_list_loaded_when_start_returns() {
        let tmp = tmp_dir("loadorder");
        let db = tmp.join("imagent.db");
        let seed = imagent_store::Store::open(&db)
            .await
            .expect("open seed store");
        seed.set_config(WATCH_CONFIG_KEY, r#"["win-b"]"#)
            .await
            .expect("seed");
        drop(seed);
        let registry_dir = tmp.join("r");
        std::fs::create_dir_all(&registry_dir).unwrap();
        let _win = live_sock(&tmp.join("9803.sock"));
        write_entry_with(
            &registry_dir,
            9803,
            "win-b",
            Some(&tmp.join("9803.sock")),
            &[],
        );
        let noop: PeerReportCallback = Arc::new(|_, _| Box::pin(async {}));
        let shutdown = CancellationToken::new();
        let server = PeerServer::start(
            persisted_config(
                &registry_dir,
                tmp.clone(),
                &db,
                Some(Duration::from_millis(40)),
                shutdown.clone(),
                noop.clone(),
                Arc::new(|_, _| {}),
            )
            .await,
        )
        .await
        .expect("server start");
        assert!(
            server.state().is_watched("win-b"),
            "start() 返回时持久化的关注名单必须已加载（先加载、再开轮询）"
        );
        shutdown.cancel();
        server.shutdown().await;
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
