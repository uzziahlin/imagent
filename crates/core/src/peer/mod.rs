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
//! socket 目录发现装配，测试用 tempdir——模块内不读 `$HOME`。
//!
//! 安全模型：socket 目录 0700 + socket 文件 0600，同 uid 即本机凭证（与
//! `local` 平台 / 权限审批 socket 同款信任边界）。入站窗口消息**不进**
//! `handle()` 鉴权管线（直接 reply 进驾驶舱）——注册表反查 + 本机 socket
//! 边界即准入。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    /// 运行状态（`status`：idle / busy / …；缺省 "unknown"）。
    pub status: String,
    /// 实例形态（`kind`：interactive / …）。
    pub kind: Option<String>,
    /// 注册来源（`entrypoint`：claude / pi / imagent / …——孤儿清理按此界家族）。
    pub entrypoint: Option<String>,
    /// 注册时刻（unix 毫秒；列表按此倒序展示）。
    pub started_at: Option<i64>,
    /// peer socket 路径（`messagingSocketPath`；回程路由键）。
    pub sock: String,
    /// 探活结果（connect 成功 = true）。[`PeerRegistry::scan`] 恒 false，
    /// 探活后由调用方填写。
    pub live: bool,
}

/// 网关默认注册表目录：`~/.claude/sessions`。home 不可解析 → None（调用方
/// 禁用 peer 通道并 warn，不 panic）。**仅 main 装配用**——测试一律注入
/// tempdir（模块内唯一的 home 访问点）。
pub fn default_registry_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude").join("sessions"))
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

/// 入站帧解析产物：`type=="user"`（唯一消费类型）或其它（control 等一律忽略）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerFrame {
    User {
        msg_id: String,
        /// `from` 原文（`uds:<sock>` 形态；回程路由键）。
        from: Option<String>,
        /// `message.content` 原文（envelope 未剥）。
        content: String,
    },
    Other,
}

/// JSON 行 → [`PeerFrame`]。畸形 JSON / 缺 `type` → None（调用方跳行）。
pub fn parse_frame_line(line: &str) -> Option<PeerFrame> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    match v.get("type").and_then(|t| t.as_str()) {
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
        Some(_) => Some(PeerFrame::Other),
        None => None,
    }
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
    /// entrypoint 条目（claude / pi 等他家族条目只读）。返回清理条数。
    pub async fn cleanup_orphans(&self) -> usize {
        let orphans: Vec<PeerEntry> = self
            .scan()
            .into_iter()
            .filter(|e| e.entrypoint.as_deref() == Some(ENTRYPOINT))
            .collect();
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
// PeerSender：出站句柄（dispatch 持有）
// ---------------------------------------------------------------------------

/// 出站投递句柄：按窗口名解析注册表（探活过滤 + 本网关自身条目排除）→
/// envelope user 帧投递。dispatch 的 `peer:` 会话文本路由与 `/to` / `/peers`
/// 命令经此消费。
#[derive(Clone)]
pub struct PeerSender {
    registry: PeerRegistry,
    own_sock: PathBuf,
    own_name: String,
}

impl PeerSender {
    /// 构造（路径注入；main 由 PeerServer::sender 产出，测试直接 new）。
    pub fn new(registry_dir: PathBuf, own_sock: PathBuf, own_name: String) -> Self {
        Self {
            registry: PeerRegistry::new(registry_dir),
            own_sock,
            own_name,
        }
    }

    pub fn registry(&self) -> &PeerRegistry {
        &self.registry
    }

    /// 本网关自身 sock 路径（出站帧 from 字段与测试断言用）。
    pub fn own_sock(&self) -> &Path {
        &self.own_sock
    }

    /// 在线窗口列表（探活过滤 + 排除本网关自身条目；`startedAt` 倒序）。
    pub async fn list_windows(&self) -> Vec<PeerEntry> {
        let own = self.own_sock.to_string_lossy().into_owned();
        self.registry
            .live_entries()
            .await
            .into_iter()
            .filter(|e| e.sock != own)
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
    pub async fn send_to_window(&self, name: &str, body: &str) -> Result<(), CoreError> {
        let entry = self.resolve_window(name).await?;
        let own = self.own_sock.to_string_lossy().into_owned();
        let content = build_envelope(&format!("uds:{own}"), &self.own_name, body);
        send_frame(&entry.sock, &build_user_frame_json(&own, &content)).await
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

/// [`PeerServer::start`] 参数（路径全注入，测试 tempdir / main 生产路径）。
pub struct PeerServerConfig {
    /// 注册表目录（`~/.claude/sessions`；测试注入 tempdir）。
    pub registry_dir: PathBuf,
    /// 本网关 peer 名（注册条目 `name`——窗口侧看到的对端名；出站帧 envelope
    /// 的 from-name 同用它）。
    pub name: String,
    /// 注册条目 cwd（config.default_workdir）。
    pub cwd: PathBuf,
    /// 停机信号（main 传 dispatcher token——drain 窗口内完成注销）。
    pub shutdown: CancellationToken,
    /// 入站回调。
    pub on_user: PeerInboundCallback,
}

/// peer server 句柄：`start` 后台跑 accept 循环；`shutdown` 幂等注销（删
/// 注册条目 + unlink socket）。
pub struct PeerServer {
    registry: PeerRegistry,
    sender: Arc<PeerSender>,
    cleanup: Arc<ServerCleanup>,
    task: tokio::task::JoinHandle<()>,
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
        let shutdown = config.shutdown.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((conn, _addr)) => {
                            handle_conn(conn, task_registry.clone(), on_user.clone());
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
        Ok(Self {
            sender: Arc::new(PeerSender::new(
                registry.dir().to_path_buf(),
                sock_path.clone(),
                config.name,
            )),
            registry,
            cleanup,
            task,
        })
    }

    /// 出站句柄（main 注入 Dispatcher；与 server 共享注册表）。
    pub fn sender(&self) -> Arc<PeerSender> {
        self.sender.clone()
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
        // 兜底终止 accept 循环（正常路径已由 token 驱动退出；cleanup 已先行
        // 执行，abort 只是防循环卡在 accept 上不退）。
        self.task.abort();
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

    pub async fn shutdown(&self) {
        unreachable!("非 unix 平台 start 恒 Err，PeerServer 不可构造")
    }
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
/// 不回写。发送方发帧前还会先开一条连接、不写数据、立刻关闭（探活）——
/// 「无数据即 EOF」的连接走同一条路直接退出。回归钉：
/// `server_inbound_silent_close_matches_real_sender`。
#[cfg(unix)]
fn handle_conn(conn: tokio::net::UnixStream, registry: PeerRegistry, on_user: PeerInboundCallback) {
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
                    let Some(PeerFrame::User {
                        msg_id,
                        from,
                        content,
                    }) = parse_frame_line(line)
                    else {
                        continue; // control 等其它类型忽略
                    };
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
            write_entry(registry_dir, pid, name, &sock, "claude");
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
        let server = PeerServer::start(PeerServerConfig {
            registry_dir: registry_dir.clone(),
            name: "im-agent".to_string(),
            cwd: tmp.clone(),
            shutdown: shutdown.clone(),
            on_user: Arc::new(move |w, b| {
                let g = got_cb.clone();
                Box::pin(async move { g.lock().unwrap().push((w, b)) })
            }),
        })
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
        write_entry(&registry_dir, 9001, "probe-window", &sender_sock, "claude");
        let got: Arc<StdMutex<Vec<(String, String)>>> = Arc::new(StdMutex::new(Vec::new()));
        let got_cb = got.clone();
        let server = PeerServer::start(PeerServerConfig {
            registry_dir: registry_dir.clone(),
            name: "im-agent".to_string(),
            cwd: tmp.clone(),
            shutdown: CancellationToken::new(),
            on_user: Arc::new(move |w, b| {
                let g = got_cb.clone();
                Box::pin(async move { g.lock().unwrap().push((w, b)) })
            }),
        })
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
        let server = PeerServer::start(PeerServerConfig {
            registry_dir: registry_dir.clone(),
            name: "im-agent".to_string(),
            cwd: tmp.clone(),
            shutdown: CancellationToken::new(),
            on_user: Arc::new(move |w, b| {
                let g = got_cb.clone();
                Box::pin(async move { g.lock().unwrap().push((w, b)) })
            }),
        })
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
        // 他家族（claude）死 sock → 保留（不得碰）。
        write_entry(
            &registry_dir,
            222,
            "claude-dead",
            &sock_dir.join("dead2.sock"),
            "claude",
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
        assert!(registry_dir.join("222.json").exists(), "claude 条目不碰");
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
        write_entry(
            &registry_dir,
            7001,
            "w",
            &sock_dir.join("7001.sock"),
            "claude",
        );
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
}
