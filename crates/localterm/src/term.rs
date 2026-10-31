//! 终端客户端（`imagent claude` / `imagent codex` 子命令）：rustyline 行编辑 +
//! socket 帧收发。两个子命令同一视图实现，差异仅在 [`run`] 的 `backend_hint`：
//! codex 视图连接后自动声明 codex-app 引擎，claude 视图不声明（既有语义）。
//!
//! # 设计基线：TUI 是视图不是宿主
//!
//! 本进程**不是** agent 宿主——会话活在网关（LocalPlatform + Dispatcher 全
//! 管线）。因此：
//! - **Ctrl-C / Ctrl-D / exit / quit 只退出本视图**（关 socket 即可，不发任何
//!   停止信号），会话继续在网关跑；退出提示语明示重入方式；
//! - 被**新连接顶替**（error 4001，另一个 `imagent claude` 接入）同样只退出
//!   本视图，不重连（重连会互踢死循环）；
//! - socket 断开（网关重启）：友好提示 + **指数退避重连**（0.5s 起、封顶 8s，
//!   成功重置），断线期间的最后一行输入缓存待重连后补发。
//!
//! # 交互形态
//!
//! - 引擎声明（批 B 薄视图）：`imagent codex <会话>` 在连接成功后、交互前自动
//!   代发一条 `/backend codex-app` 命令文本（与用户手打等价——dispatcher 命令
//!   层写 per-conv 引擎覆盖 + 回显确认，不进 agent 轮次；回显「✅ 本会话引擎
//!   已切换…」经 reader 打印，正是可见反馈）。`imagent claude` 不发声明
//!   （跟随网关默认/该会话已有设置，语义完全不变）。
//! - 输入：rustyline（历史/行编辑）；`/` 命令原样透传（message 帧，走与 IM
//!   手打命令相同的鉴权/分派）；`exit`/`quit` 退出视图。
//! - 输出（stdout 追加式，ANSI 基础色）：
//!   - text 帧：直接打印（core 文本流路径的合帧流式文本，含最终回复自动
//!     追加的 🔧 工具调用摘要 / 📋 计划进度行）；
//!   - approval_request：醒目块 `🔐 请求执行 <工具>：…` + 提示行——**下一次
//!     行输入被路由为审批回复**（y/n/always → approval_response 帧，requestId
//!     锚定；斜杠命令仍透传，/stop 会经 cancel_all 收敛审批）；
//!   - approval_resolved：暗色收敛行（对端处理 / 系统取消）；
//!   - machine/machines 等多机帧：忽略（本地无 relay 多机语义）。
//! - 已知取舍：流式输出与行编辑提示符并发打印会短暂串行错位（下一击键
//!   rustyline 重绘自愈），追加式终端可接受，不引 curses 级依赖。

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

use crate::{sanitize_session_name, TARGET};

/// 首连失败（网关未运行）的退出码（main 直接 exit）。
pub const EXIT_GATEWAY_DOWN: i32 = 2;

/// hello_ack 等待上限。
const HELLO_ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// 重连退避：0.5s 起，翻倍封顶 8s，成功重置。
const BACKOFF_START: Duration = Duration::from_millis(500);
const BACKOFF_CAP: Duration = Duration::from_secs(8);
/// 重连线程空转探测节拍。
const RECONNECT_POLL: Duration = Duration::from_secs(1);

/// ANSI 基础色（无依赖；非 TTY 环境大多原样吞掉，可接受）。
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// 待回复的审批（渲染 + 下一行输入路由锚定）。
#[derive(Debug, Clone)]
struct PendingAsk {
    conv: String,
    request_id: String,
}

/// 一行输入的去向（在线直发 / 断线缓存，形态同构便于补发）。
#[derive(Debug, Clone)]
enum Outgoing {
    Message(String),
    Approval {
        conv: String,
        request_id: String,
        decision: &'static str,
    },
}

/// 客户端共享状态（主循环 / 读线程 / 重连线程三方持有）。
struct Shared {
    session: String,
    /// 写半（重连时整体替换）；读线程持独立克隆，断流自然退出。
    out: Mutex<UnixStream>,
    pending: Mutex<Option<PendingAsk>>,
    /// 连接状态（与 cv 配对）：false = 断线（主循环缓存输入、重连线程工作）。
    connected: Mutex<bool>,
    cv: Condvar,
    /// 终态（被顶替 4001）：不再重连，主循环退出视图。
    superseded: AtomicBool,
    /// 视图主动退出中：reader 的 EOF 不再报「断开重连」（关写向是我们自己）。
    closing: AtomicBool,
    /// 断线期间缓存的最后一行（重连成功后由重连线程补发并清空）。
    buffered: Mutex<Option<Outgoing>>,
}

impl Shared {
    fn new(session: String, stream: UnixStream) -> Self {
        Self {
            session,
            out: Mutex::new(stream),
            pending: Mutex::new(None),
            connected: Mutex::new(true),
            cv: Condvar::new(),
            superseded: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            buffered: Mutex::new(None),
        }
    }

    fn mark_disconnected(&self) {
        let mut c = self.connected.lock().unwrap();
        *c = false;
        self.cv.notify_all();
    }

    fn mark_connected(&self) {
        let mut c = self.connected.lock().unwrap();
        *c = true;
        self.cv.notify_all();
    }

    /// 主循环 park：等到重连成功；被顶替（终态）返回 false 立即退出。
    fn wait_connected(&self) -> bool {
        let mut c = self.connected.lock().unwrap();
        loop {
            if self.superseded.load(Ordering::Relaxed) {
                return false;
            }
            if *c {
                return true;
            }
            c = self.cv.wait(c).unwrap();
        }
    }

    /// 发送一行 JSON（带换行）。写失败 = 连接已断 → 标记断线（重连线程接管）。
    fn send_line(&self, json: &str) {
        let mut out = self.out.lock().unwrap();
        let r = out
            .write_all(json.as_bytes())
            .and_then(|_| out.write_all(b"\n"))
            .and_then(|_| out.flush());
        if r.is_err() {
            drop(out);
            self.mark_disconnected();
        }
    }

    /// 按去向发送（在线路径）。
    fn send_outgoing(&self, o: &Outgoing) {
        match o {
            Outgoing::Message(text) => self.send_line(&message_frame(text)),
            Outgoing::Approval {
                conv,
                request_id,
                decision,
            } => self.send_line(&approval_response_frame(conv, request_id, decision)),
        }
    }
}

// ---------------------------------------------------------------------------
// 帧构造 / 解析（客户端侧只用 JSON Value，帧形态见 lib.rs）
// ---------------------------------------------------------------------------

fn hello_frame(session: &str) -> String {
    serde_json::json!({"v": 1, "type": "hello", "session": session}).to_string()
}

fn sync_frame() -> &'static str {
    r#"{"v":1,"type":"sync"}"#
}

fn message_frame(text: &str) -> String {
    serde_json::json!({"v": 1, "type": "message", "text": text}).to_string()
}

fn approval_response_frame(conv: &str, request_id: &str, decision: &str) -> String {
    serde_json::json!({
        "v": 1,
        "type": "approval_response",
        "convId": conv,
        "requestId": request_id,
        "decision": decision,
    })
    .to_string()
}

/// 审批行输入 → 帧内 decision（词表宽于帧侧：y/yes/n/no/always，大小写不
/// 敏感；未命中 None——主循环提示重新输入，**不把任意文本当 deny**，防误拒）。
fn input_decision(line: &str) -> Option<&'static str> {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some("allow"),
        "n" | "no" => Some("deny"),
        "always" => Some("always"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 薄视图引擎声明（批 B：`imagent codex` → `/backend codex-app`）
// ---------------------------------------------------------------------------

/// 引擎声明命令文本：`/backend <name>`。与用户手打完全等价——dispatcher 命令
/// 层处理（写 `backend:<conv>` KV + 回显确认，不进 agent 轮次；会话已在该
/// 引擎时重复写同值 KV，幂等无副作用）。
fn backend_hint_command(hint: &str) -> String {
    format!("/backend {hint}")
}

/// 连接成功后、交互前代发引擎声明（`imagent codex` 薄视图）。经既有发送通道
/// 发 message 文本帧（与手打同形），回显确认由 reader 打印。发送失败（连接
/// 刚断）静默——重连线程接管即可继续用；只在首连发一次（每次重连都重发会在
/// 断线循环里重复写 KV，无意义；KV 未写成时用户可手打 `/backend <name>` 补）。
fn send_backend_hint(shared: &Shared, hint: &str) {
    shared.send_outgoing(&Outgoing::Message(backend_hint_command(hint)));
}

// ---------------------------------------------------------------------------
// 连接 + 握手
// ---------------------------------------------------------------------------

/// 建连并完成握手：hello（带会话名，**免 token**——本地 socket 0600 + peer
/// uid 即凭证，见 platform.rs 模块文档）→ 等 hello_ack（校验回显会话名）→
/// 发 sync（网关在就绪前回文本缓存快照，恢复上下文）。
fn connect_and_hello(path: &Path, session: &str) -> Result<UnixStream> {
    let mut stream =
        UnixStream::connect(path).with_context(|| format!("连接 {} 失败", path.display()))?;
    stream.write_all(hello_frame(session).as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    stream.set_read_timeout(Some(HELLO_ACK_TIMEOUT))?;
    let mut line = String::new();
    let n = BufReader::new(&mut stream).read_line(&mut line)?;
    if n == 0 {
        return Err(anyhow!("网关在握手期关闭了连接"));
    }
    let v: serde_json::Value =
        serde_json::from_str(line.trim()).with_context(|| format!("hello_ack 解析失败：{line}"))?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("hello_ack") => {}
        Some("error") => {
            let code = v.get("code").and_then(|c| c.as_str()).unwrap_or("?");
            let msg = v.get("message").and_then(|m| m.as_str()).unwrap_or("");
            return Err(anyhow!("网关拒绝连接（code={code}）：{msg}"));
        }
        other => return Err(anyhow!("认证确认帧异常（type={other:?}）")),
    }
    let ack_session = v.get("session").and_then(|s| s.as_str()).unwrap_or("");
    if ack_session != session {
        return Err(anyhow!(
            "hello_ack 回显会话名不一致（ack={ack_session}，自报={session}）"
        ));
    }
    stream.set_read_timeout(None)?;
    stream.write_all(sync_frame().as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(stream)
}

// ---------------------------------------------------------------------------
// 读线程：渲染出站帧
// ---------------------------------------------------------------------------

fn spawn_reader(stream: UnixStream, shared: Arc<Shared>) {
    std::thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "text" => {
                    let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    if !text.is_empty() {
                        println!("{text}");
                    }
                }
                "approval_request" => {
                    let conv = v.get("convId").and_then(|c| c.as_str()).unwrap_or("");
                    let request_id = v
                        .get("requestId")
                        .and_then(|r| r.as_str())
                        .unwrap_or("")
                        .to_string();
                    let tool = v.get("tool").and_then(|t| t.as_str()).unwrap_or("?");
                    let summary = v.get("inputSummary").and_then(|s| s.as_str()).unwrap_or("");
                    println!("\n{YELLOW}{BOLD}🔐 请求执行 {tool}：{summary}{RESET}");
                    println!("{DIM}（会话 {conv}）回复 y 允许 / n 拒绝 / always 始终允许{RESET}\n");
                    *shared.pending.lock().unwrap() = Some(PendingAsk {
                        conv: conv.to_string(),
                        request_id,
                    });
                }
                "approval_resolved" => {
                    let request_id = v
                        .get("requestId")
                        .and_then(|r| r.as_str())
                        .unwrap_or("")
                        .to_string();
                    let decision = v.get("decision").and_then(|d| d.as_str()).unwrap_or("?");
                    let msg = v.get("message").and_then(|m| m.as_str()).unwrap_or("");
                    let icon = match decision {
                        "allow" | "always" => "✅",
                        "deny" => "❌",
                        _ => "⏹",
                    };
                    let suffix = if msg.is_empty() {
                        String::new()
                    } else {
                        format!("：{msg}")
                    };
                    println!("{DIM}{icon} 审批已收敛（{decision}）{suffix}{RESET}");
                    let mut pending = shared.pending.lock().unwrap();
                    if pending.as_ref().map(|p| p.request_id.clone()) == Some(request_id) {
                        *pending = None;
                    }
                }
                "error" => {
                    let code = v.get("code").and_then(|c| c.as_str()).unwrap_or("");
                    let msg = v.get("message").and_then(|m| m.as_str()).unwrap_or("");
                    if code == "4001" {
                        println!("{YELLOW}⚠ {msg}{RESET}");
                        println!(
                            "{DIM}终端视图已退出。会话仍在 imagent 网关中运行——\
                             重入：imagent claude {session}（imagent codex {session} \
                             可用 codex 引擎接入）{RESET}",
                            session = shared.session
                        );
                        shared.superseded.store(true, Ordering::Relaxed);
                        shared.mark_disconnected();
                        return; // 被顶替是终态：不重连（互踢死循环）
                    }
                    println!("{YELLOW}⚠ 网关错误帧（code={code}）：{msg}{RESET}");
                }
                // machine/machines 等 relay 多机帧：本地无此语义，忽略。
                _ => {}
            }
        }
        // EOF / 读错误：断线 → 重连线程接管（被顶替 / 主动退出时上面已处理）。
        if !shared.superseded.load(Ordering::Relaxed) && !shared.closing.load(Ordering::Relaxed) {
            println!("{DIM}⚠ 与网关的连接已断开（网关可能重启中），自动重连中…{RESET}");
            shared.mark_disconnected();
        }
    });
}

// ---------------------------------------------------------------------------
// 重连线程
// ---------------------------------------------------------------------------

fn spawn_reconnector(socket_path: PathBuf, shared: Arc<Shared>) {
    std::thread::spawn(move || {
        let mut backoff = BACKOFF_START;
        loop {
            // 等断线（在线时按节拍醒来看标志，避免永久 wait 错过 superseded）。
            {
                let mut c = shared.connected.lock().unwrap();
                while *c && !shared.superseded.load(Ordering::Relaxed) {
                    c = shared.cv.wait_timeout(c, RECONNECT_POLL).unwrap().0;
                }
            }
            if shared.superseded.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(BACKOFF_CAP);
            if shared.superseded.load(Ordering::Relaxed) {
                return;
            }
            match connect_and_hello(&socket_path, &shared.session) {
                Ok(stream) => {
                    backoff = BACKOFF_START;
                    spawn_reader(stream.try_clone().expect("克隆读半"), shared.clone());
                    *shared.out.lock().unwrap() = stream;
                    // 补发断线期间缓存的最后一行（缓存只留一行，取走即清）。
                    let buffered = shared.buffered.lock().unwrap().take();
                    if let Some(b) = &buffered {
                        shared.send_outgoing(b);
                    }
                    shared.mark_connected();
                    println!("{DIM}✅ 已重新连接网关（上下文快照已恢复）{RESET}");
                }
                Err(e) => {
                    tracing::debug!(target: TARGET, error = %e, "重连失败，退避重试");
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// 主入口
// ---------------------------------------------------------------------------

/// 终端客户端主循环。返回进程退出码：0 = 正常退出视图；2 = 网关未运行
/// （首次连接即失败——提示先 `imagent start`）。
///
/// `backend_hint`（批 B 薄视图）：`Some(hint)` 时在连接成功后、交互前经既有
/// 发送通道代发一条 `/backend <hint>` 命令文本声明本会话引擎（`imagent codex`
/// → `"codex-app"`，与用户手打等价）；`None`（`imagent claude`）零行为，既有
/// 语义完全不变。
pub fn run(socket_path: &Path, session: Option<String>, backend_hint: Option<&str>) -> Result<i32> {
    let session = match session.as_deref() {
        None => "default".to_string(),
        Some(raw) => sanitize_session_name(raw).ok_or_else(|| {
            anyhow!("非法会话名 {raw:?}（≤64 char，不含 ':' 与控制字符；缺省 default）")
        })?,
    };
    let stream = match connect_and_hello(socket_path, &session) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "无法连接 imagent 网关（{}）：{e}\n\
                 请先在 config.toml 配置 platforms 含 \"local\" 并运行 `imagent start`，\n\
                 然后用 `imagent claude {session}` 接入（`imagent codex {session}` 可用 \
                 codex 引擎接入；--profile 与网关一致）。",
                socket_path.display()
            );
            return Ok(EXIT_GATEWAY_DOWN);
        }
    };
    println!(
        "{DIM}已接入 imagent 网关：会话 local:{session}（视图模式——Ctrl-C/exit 只退出本终端，\
         会话继续在网关运行；/help 看命令）{RESET}"
    );
    let shared = Arc::new(Shared::new(session, stream));
    spawn_reader(
        shared.out.lock().unwrap().try_clone().expect("克隆读半"),
        shared.clone(),
    );
    spawn_reconnector(socket_path.to_path_buf(), shared.clone());

    // 批 B 薄视图：连接成功后、交互前代发引擎声明（见 run 文档与
    // send_backend_hint）。经既有发送通道（与手打同形），回显确认经 reader
    // 打印；None = claude 视图，零行为。
    if let Some(hint) = backend_hint {
        send_backend_hint(&shared, hint);
    }

    let mut rl = DefaultEditor::new().context("初始化行编辑器失败")?;
    // 历史（best-effort）：socket 同目录 localterm_history。
    let history_path = socket_path
        .parent()
        .map(|d| d.join("localterm_history"))
        .unwrap_or_else(|| Path::new("localterm_history").to_path_buf());
    let _ = rl.load_history(&history_path);

    loop {
        // 纯文本提示符（rustyline 不解析 ANSI，着色会导致重绘宽度错位）。
        let pending = shared.pending.lock().unwrap().is_some();
        let prompt = if pending {
            "y/n ❯ ".to_string()
        } else {
            format!("imagent[{}] ❯ ", shared.session)
        };
        match rl.readline(&prompt) {
            Ok(raw) => {
                let line = raw.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(&line);
                let _ = rl.save_history(&history_path);
                if line == "exit" || line == "quit" {
                    print_exit_hint(&shared);
                    break;
                }
                // 行去向分类：挂审批时非命令行 → 审批回复；其余（含 / 命令）→ 消息。
                let pending = shared.pending.lock().unwrap().clone();
                let outgoing = match pending {
                    Some(ask) if !line.starts_with('/') => match input_decision(&line) {
                        Some(decision) => Some(Outgoing::Approval {
                            conv: ask.conv,
                            request_id: ask.request_id,
                            decision,
                        }),
                        None => {
                            println!(
                                "{DIM}等待审批回复：请输入 y（允许）/ n（拒绝）/ always（始终允许），\
                                 或 /stop 中断任务{RESET}"
                            );
                            None
                        }
                    },
                    _ => Some(Outgoing::Message(line)),
                };
                let Some(outgoing) = outgoing else { continue };
                // 在线直发；断线缓存最后一行并等重连（重连线程补发）。
                let online = *shared.connected.lock().unwrap();
                if online {
                    shared.send_outgoing(&outgoing);
                    if matches!(outgoing, Outgoing::Approval { .. }) {
                        // 本端即时清 pending（resolved 帧是服务端确认，双保险）。
                        *shared.pending.lock().unwrap() = None;
                    }
                } else {
                    *shared.buffered.lock().unwrap() = Some(outgoing);
                    println!("{DIM}连接断开中，输入已缓存，重连后自动发送…{RESET}");
                    if !shared.wait_connected() {
                        // 被顶替终态（提示已由读线程打印）。
                        break;
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                // Ctrl-C：只退出视图（本设计的核心语义）——不向网关发任何信号。
                print_exit_hint(&shared);
                break;
            }
            Err(ReadlineError::Eof) => {
                print_exit_hint(&shared);
                break;
            }
            Err(e) => {
                eprintln!("行读取失败，退出视图：{e}");
                print_exit_hint(&shared);
                break;
            }
        }
    }

    // 视图退出：先置 closing（reader 的 EOF 不再报断线重连），再关写向
    //（服务端读半收 EOF 清理连接；会话不受影响——活在网关）。
    shared.closing.store(true, Ordering::Relaxed);
    if let Ok(out) = shared.out.lock() {
        let _ = out.shutdown(Shutdown::Write);
    }
    Ok(0)
}

fn print_exit_hint(shared: &Shared) {
    println!(
        "{DIM}终端视图已退出。会话 local:{session} 仍在 imagent 网关中运行——\n\
         重入：imagent claude {session}（imagent codex {session} 可用 codex 引擎接入）；\n\
         停止任务：重入后发 /stop。{RESET}",
        session = shared.session
    );
}

// ---------------------------------------------------------------------------
// 测试：hint 帧注入（socketpair 最小回环，形态参照 platform.rs peer uid 测试）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! 断言薄视图引擎声明的帧形态：连接后首条为 `/backend codex-app` message
    //! 文本帧（与用户手打同形——走批 A 命令通道，零协议改动）。

    use super::*;

    /// `imagent codex` 路径：send_backend_hint 经既有发送通道（Shared::send_
    /// outgoing）发出的帧与手打 message 帧同形，text 恰为 `/backend codex-app`
    /// （dispatcher 命令层凭 `/` 前缀分派；不带 convId 落 hello 会话）。
    #[test]
    fn backend_hint_sends_backend_command_message_frame() {
        let (client, mut gateway) = UnixStream::pair().expect("socketpair");
        let shared = Arc::new(Shared::new("work".to_string(), client));
        send_backend_hint(&shared, "codex-app");

        let mut line = String::new();
        BufReader::new(&mut gateway)
            .read_line(&mut line)
            .expect("读 hint 帧");
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("hint 帧应为 JSON 行");
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "message", "与手打同形：message 文本帧");
        assert_eq!(v["text"], "/backend codex-app");
        assert!(
            v.get("convId").is_none(),
            "客户端 message 帧不带 convId（落 hello 会话）"
        );
    }

    /// 声明文本构造钉住：`/backend <hint>`（命令层按前缀 + 注册表名单精确
    /// 分派，写法漂移会落「未知引擎」回显）。
    #[test]
    fn backend_hint_command_format() {
        assert_eq!(backend_hint_command("codex-app"), "/backend codex-app");
    }
}
