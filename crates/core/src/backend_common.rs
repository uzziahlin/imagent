//! Backend 共享工具：三 CLI backend（claude/codex/gemini）共用的脚手架与常量，
//! 消除 `run()` 重复（见 `docs/internal/CODE_REVIEW.md` P1-1）。
//!
//! 设计：三 backend 的 `run()` 收缩为「构造 cmd + 适配闭包（自己的 parse_line →
//! [`CliEvent`]）+ 调 [`spawn_cli_backend`]」。各 backend 的 `stream::parse_line`
//! 保持不变（各自 `ParsedEvent`），由 backend 的适配闭包映射到统一的 [`CliEvent`]。

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::error::{CoreError, Result};
use crate::types::{AgentChunk, RunOutcome, SessionId, TodoItem, TodoStatus, UsageStats};

/// 三 CLI backend 的 stdout 行解析统一事件。各 backend 的适配闭包把自己的
/// `ParsedEvent` 映射到它。
#[derive(Debug, Clone)]
pub enum CliEvent {
    /// 纯 session 捕获（首次有效；如 claude Other/codex ThreadStarted/gemini Init）。
    Session(String),
    /// 中间文本（best-effort 推 IM；codex AgentMessage / gemini AssistantMessage /
    /// claude assistant 文本 B8）。同时作为 final_text 候选（按序拼接，见 B9）。
    Text(String),
    /// W2-1：思考过程（claude assistant 的 thinking 块）——与 Text 分离，
    /// 不进 final_text、不进正文流，仅推 Thought chunk（卡片折叠展示）。
    Thought(String),
    /// 工具调用。`session` 供尽早捕获 session_id（如 claude ToolUse 带 session）。
    ToolUse {
        tool: String,
        input: String,
        session: Option<String>,
        /// W2-3：调用 id（与 ToolResult 精确配对）；后端拿不到为 None。
        id: Option<String>,
    },
    /// 工具结果。
    ToolResult {
        tool: String,
        output: String,
        /// W2-3：对应调用的 id（claude `tool_result.tool_use_id`）。
        id: Option<String>,
    },
    /// W2-2：任务清单（Claude Code TodoWrite 的结构化解析产物；全量替换语义）。
    TodoList { items: Vec<TodoItem> },
    /// 终止 + 最终文本（claude `result` 非 error）。
    Final {
        text: String,
        session: Option<String>,
    },
    /// 终止 + 错误（claude `result` is_error / codex TurnFailed / gemini Error）。
    Error {
        text: String,
        session: Option<String>,
    },
    /// 终止信号无文本（codex TurnCompleted / gemini Result）；final 取最后 Text。
    Terminal { session: Option<String> },
    /// canUseTool 控制请求（claude `--input-format stream-json` 双工协议）：claude
    /// 需要工具审批时经 stdout 发出，宿主须回写 `control_response` 到 stdin（见
    /// [`ControlIo`]）。`input` 为工具入参原始 JSON；`subtype` 未知形态也透传
    /// （ responder 对非 can_use_tool 回 error，防 claude 挂起）。
    ControlRequest {
        request_id: String,
        subtype: String,
        tool_name: String,
        input: String,
    },
    /// token 用量/成本（claude result / codex turn.completed / gemini result 附带）。
    /// 由 spawn_cli_backend 累积合并进 RunOutcome.usage；多事件合并语义见
    /// [`UsageStats::merge`]（input/output 求和、cost 取最后非 None）。
    /// 注意：与终止事件同批（Multi）时应排在终止事件**之前**——终止事件会
    /// break 读取循环，排在后的 Usage 会被丢弃。
    Usage(UsageStats),
    /// 非致命 error 事件（codex 顶层 `error`，可能瞬时重连；B10）。不中断流，
    /// 仅记录内容——若最终无任何 final 文本，作为失败原因呈现。
    TransientError(String),
    /// 一行产出多个事件（B7：claude 一条 assistant/user 消息的 content[] 可含
    /// 多个并行 tool_use / tool_result 与文本，需全部产出）。由
    /// [`spawn_cli_backend`] 展开逐个处理；各 backend 适配闭包按需构造。
    Multi(Vec<CliEvent>),
    /// 非 JSON / 噪声，跳过。
    Skip,
}

/// 把 [`CliEvent::Multi`] 展平成事件序列（Multi 不嵌套，一层即可）。
fn flatten_event(ev: CliEvent, out: &mut Vec<CliEvent>) {
    match ev {
        CliEvent::Multi(evs) => out.extend(evs),
        other => out.push(other),
    }
}

/// 检测「Write 工具写图片文件」：tool 为 Write 且 input JSON 的 file_path
/// 扩展名是图片（png/jpg/jpeg/gif/webp/bmp），返回该路径；否则 None。
/// input 是 stream-json 的工具入参 JSON 字符串；非法 JSON / 缺 file_path 一律 None。
pub(crate) fn image_write_path(tool: &str, input: &str) -> Option<String> {
    if tool != "Write" {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    let path = v.get("file_path")?.as_str()?;
    let lower = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    const IMG_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];
    IMG_EXTS.contains(&lower.as_str()).then(|| path.to_string())
}

/// W2-2：解析 Claude Code 的 TodoWrite 工具入参为任务清单。input 形如
/// `{"todos":[{"content":"…","status":"pending|in_progress|completed", …}]}`。
/// 非 TodoWrite / 非法 JSON / 空 todos 返回 None（调用方按普通 ToolUse 处理）。
pub fn todo_write_items(tool: &str, input: &str) -> Option<Vec<TodoItem>> {
    if tool != "TodoWrite" {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    let todos = v.get("todos")?.as_array()?;
    if todos.is_empty() {
        return None;
    }
    let items = todos
        .iter()
        .filter_map(|t| {
            let text = t.get("content")?.as_str()?.trim().to_string();
            if text.is_empty() {
                return None;
            }
            let status = match t.get("status").and_then(|s| s.as_str()) {
                Some("completed") => TodoStatus::Completed,
                Some("in_progress") => TodoStatus::InProgress,
                _ => TodoStatus::Pending,
            };
            Some(TodoItem { text, status })
        })
        .collect::<Vec<_>>();
    (!items.is_empty()).then_some(items)
}

/// unix 进程组 kill 守卫（B5）：spawn 时 `process_group(0)` 把 CLI 子进程放进
/// 独立进程组；本守卫在 drop（run future 被 cancel / 超时 / Err 提前返回）时对
/// 整组 `killpg(SIGKILL)`，连孙进程（MCP server、Bash 工具）一并收割。正常
/// `wait` 成功后调用 [`GroupKillGuard::disarm`]（防 pid 复用误杀无关进程组）。
#[cfg(unix)]
struct GroupKillGuard {
    pgid: i32,
    armed: bool,
}

#[cfg(unix)]
#[allow(unsafe_code)] // 同上 Drop impl：libc::kill 局部豁免
impl GroupKillGuard {
    fn new(pgid: u32) -> Self {
        Self {
            pgid: pgid as i32,
            armed: true,
        }
    }

    /// 正常退出路径解除武装。
    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
#[allow(unsafe_code)] // 局部豁免，先例同 instance.rs flock / dispatch::socket::peer_uid
impl Drop for GroupKillGuard {
    fn drop(&mut self) {
        if self.armed {
            // SAFETY：libc::kill 是 POSIX 简单系统调用，负 pid = 整个进程组，
            // best-effort（组已消散时 ESRCH，忽略返回值）。
            unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
        }
    }
}

#[cfg(unix)]
#[allow(unsafe_code)] // 同 Drop impl：libc::kill 局部豁免
impl GroupKillGuard {
    /// 主动组杀 + disarm（不等 drop）：终态收尾路径用——孙进程可能持有
    /// stderr/stdout 管道，只杀直接子进程会让 `stderr_handle.await` 等一个
    /// 永不到来的 EOF（真机校准 2026-08-29：control 通道 kill 后 3 分钟挂起）。
    fn kill_now(&mut self) {
        if self.armed {
            // SAFETY：同 Drop——负 pid = 整组 SIGKILL，best-effort。
            unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
            self.armed = false;
        }
    }
}

/// 泛型 run 脚手架：spawn cmd → kill_on_drop + stdin null → 并发 stderr →
/// 读 stdout 循环（调 `parse` 映射到 [`CliEvent`]）→ session/final/error 收集 →
/// RunOutcome。三 CLI backend 共用，零行为差异（仅去重）。
///
/// `cmd` 由调用方构造好（cwd/args 已设；本函数统一加 stdin/stdout/stderr/kill_on_drop）。
/// `parse` 是各 backend 的「行 → CliEvent」适配闭包。`backend_name` 用于错误信息。
/// `passthrough_env`：S-2——本函数会先 `env_clear()`，再仅透传 [`ALWAYS_PASSTHROUGH_ENV`]
/// 以及调用方声明的这些 key（各 backend 传自己的 API key，最小授权）。传 `&[]` 则只透传
/// 运行时必需变量（PATH/HOME/...）。
/// canUseTool 控制通道上下文（claude 独有；其余 CLI 传 None）。审批决策经
/// [`crate::mcp::ask_via_socket`] 复用既有 permission.sock 路由（含 token 握手、
/// IM 审批卡、👍、always、超时 fail-closed——core 侧零改动）。
/// W2-2 扩展（真机校准 2026-08-30）：claude 2.x 用 Task* 工具族替代 TodoWrite
/// （实测形态：TaskCreate `{subject, description?, activeForm?}` 无 id——序号即
/// 创建顺序；TaskUpdate `{taskId, status?}`，status 语义与 [`TodoStatus`] 同构）。
/// 增量维护 (id, TodoItem) 列表，返回是否变化（变化即发全量 TodoList chunk）。
fn apply_task_tool(list: &mut Vec<(String, TodoItem)>, tool: &str, input_json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(input_json) else {
        return false;
    };
    match tool {
        "TaskCreate" => {
            let Some(subject) = v.get("subject").and_then(|s| s.as_str()) else {
                return false;
            };
            // 真实任务 id 是**会话级连续编号**（真机校准 2026-08-30：--resume 后
            // 新任务 #3 而非 #1），创建时未知——先占位（空 id），由创建调用的
            // 工具结果（"Task #N created"）经 tool_use id 配对回填（见
            // [`resolve_task_create_result`]）。占位期间 TaskUpdate 匹配不到
            // 是安全的：模型引用 id 必然晚于创建结果返回。
            list.push((
                String::new(),
                TodoItem {
                    text: subject.to_string(),
                    status: TodoStatus::Pending,
                },
            ));
            true
        }
        "TaskUpdate" => {
            let Some(task_id) = v.get("taskId").and_then(|t| t.as_str()) else {
                return false;
            };
            let status = v.get("status").and_then(|s| s.as_str());
            let subject = v.get("subject").and_then(|s| s.as_str());
            let Some(entry) = list.iter_mut().find(|(id, _)| id == task_id) else {
                return false;
            };
            let mut changed = false;
            if let Some(s) = status {
                let mapped = match s {
                    "completed" => TodoStatus::Completed,
                    "in_progress" => TodoStatus::InProgress,
                    _ => TodoStatus::Pending,
                };
                if entry.1.status != mapped {
                    entry.1.status = mapped;
                    changed = true;
                }
            }
            if let Some(sub) = subject {
                if entry.1.text != sub {
                    entry.1.text = sub.to_string();
                    changed = true;
                }
            }
            changed
        }
        _ => false,
    }
}

/// 从 CliEvent::ToolUse 取工具调用 id（W2-3 配对键）。
fn ev_id_of(ev: &CliEvent) -> Option<String> {
    match ev {
        CliEvent::ToolUse { id, .. } => id.clone(),
        _ => None,
    }
}

/// TaskList 工具结果 → 全量待办快照（真机校准 2026-08-30 实测形态：每行
/// `#N [status] subject`）。桌面端（Claude Code 终端）创建的任务也在此权威
/// 视图中——模型 resume 后调 TaskList 即可让面板补全历史任务。无任何可解析
/// 行返回 None（不动既有清单）。
fn parse_task_list_snapshot(output: &str) -> Option<Vec<(String, TodoItem)>> {
    let mut out = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix('#') else {
            continue;
        };
        let Some(sp) = rest.find(' ') else { continue };
        let (id, rest2) = rest.split_at(sp);
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Some(open) = rest2.find('[') else {
            continue;
        };
        let Some(close) = rest2.find(']') else {
            continue;
        };
        if open >= close {
            continue;
        }
        let status = match &rest2[open + 1..close] {
            "completed" => TodoStatus::Completed,
            "in_progress" => TodoStatus::InProgress,
            _ => TodoStatus::Pending,
        };
        let text = rest2[close + 1..].trim();
        if text.is_empty() {
            continue;
        }
        out.push((
            id.to_string(),
            TodoItem {
                text: text.to_string(),
                status,
            },
        ));
    }
    (!out.is_empty()).then_some(out)
}

/// TaskCreate 工具结果回填：解析 `Task #N created successfully`（实测形态），
/// 回填占位条目的真实 id；无该形态（失败）则移除占位行（防幽灵清单项）。
/// 返回是否变化（变化即重发快照）。
fn resolve_task_create_result(
    list: &mut Vec<(String, TodoItem)>,
    index: usize,
    output: &str,
) -> bool {
    let real_id = output
        .find("Task #")
        .and_then(|i| {
            output[i + 6..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse::<u32>()
                .ok()
        })
        .map(|n| n.to_string());
    match real_id {
        Some(id) => {
            if list.get(index).is_some_and(|(cur, _)| cur.is_empty()) {
                list[index].0 = id;
                true
            } else {
                false
            }
        }
        None => {
            // 创建失败：移除占位行（保持 display 顺序）。
            if index < list.len() {
                list.remove(index);
                true
            } else {
                false
            }
        }
    }
}

pub struct ControlIo {
    /// permission.sock 路径。
    pub sock: String,
    /// 会话 id（审批卡路由用）。
    pub conv_id: String,
    /// 审批预算（= config permission_ask_timeout_secs）。
    pub ask_timeout: std::time::Duration,
    /// 启动即写入子进程 stdin 的首条消息（SDK 式 user 消息 JSON 行——
    /// `--input-format stream-json` 模式下 prompt 经 stdin 投递）。
    pub initial_stdin_message: String,
}

/// 组装 control_response 响应行（写回子进程 stdin）。形态按 Agent SDK 双工协议
/// （**待真机校准**：字段名/大小写以实测为准，错则 claude 视为未答复挂起——
/// 兜底见 spawn 循环的 error 响应）。
fn control_response_line(request_id: &str, reply: &crate::permission::PermissionReply) -> String {
    // 真机校准（2026-08-30 实测 2.1.250）：request_id/subtype 嵌在 response 内、
    // 决定再嵌一层（顶层形态 claude 不认，表现为审批后无 tool_result）。
    // L16（code-review v8）：always 语义不回传 updatedPermissions——网关侧
    // session_allows 已短路后续同工具询问（route 路径），回传属死代码。
    let behavior = if reply.allow { "allow" } else { "deny" };
    let resp = serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {
                "behavior": behavior,
                "message": reply.message.clone().unwrap_or_default(),
            }
        }
    });
    format!("{resp}\n")
}

pub async fn spawn_cli_backend(
    mut cmd: tokio::process::Command,
    parse: impl Fn(&str) -> CliEvent,
    chunks: tokio::sync::mpsc::Sender<AgentChunk>,
    backend_name: &'static str,
    passthrough_env: &[&str],
    control: Option<ControlIo>,
) -> Result<RunOutcome> {
    if control.is_some() {
        // 控制通道：stdin 必须保持打开（control_response 回写 + SDK 式 prompt
        // 投递）。EOF 会让 claude 的 control_request 永远无人应答（已知挂死形态）。
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // B5：claude/codex/gemini 会 spawn 孙进程（MCP server、Bash 工具），kill 只打
    // 直接子进程会留孤儿。unix 上把子进程放进独立进程组（pgid = 自身 pid），
    // 读取结束后由 [`GroupKillGuard`] 对整组 killpg(SIGKILL) 兜底；kill_on_drop
    // 仍保留（非 unix / 进程组语义不可用时的直接子进程兜底）。非 unix 保持现状。
    #[cfg(unix)]
    cmd.process_group(0);

    // S-2：env_clear 防止 agent 子进程继承父进程全部 env（部署环境的 DATABASE_URL /
    // CI secret / 其他工具 token 等，可经 Bash env / /proc/self/environ 被读取并经
    // tool_result 回传 IM 或写入 workdir）。仅透传白名单：运行时必需变量 + 调用方
    // 声明的该后端 API key。未设置的 key 跳过（不向子进程注入空值）。
    cmd.env_clear();
    for &key in ALWAYS_PASSTHROUGH_ENV.iter().chain(passthrough_env.iter()) {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| CoreError::Backend(backend_name, format!("failed to spawn: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CoreError::Backend(backend_name, "stdout not piped".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CoreError::Backend(backend_name, "stderr not piped".into()))?;

    // 控制通道：取 stdin 写入首条 user 消息（SDK 式 prompt 投递），并保持写
    // 句柄供 control_response 回写。写入失败不致命（claude 侧报 EOF 可见错误）。
    let mut stdin_w = child.stdin.take();
    if let (Some(w), Some(io)) = (&mut stdin_w, &control) {
        // 诊断（control 通道真机校准）：初始写入失败必须显式可见——静默吞错
        // 会让 claude 等 stdin 空转（实测卡「输出中」排查用）。
        if let Err(e) = w.write_all(io.initial_stdin_message.as_bytes()).await {
            tracing::error!(target: "imagent::backend", error = %e, "control 通道初始 stdin 写入失败");
        }
        if let Err(e) = w.flush().await {
            tracing::error!(target: "imagent::backend", error = %e, "control 通道 stdin flush 失败");
        }
        tracing::debug!(target: "imagent::backend", bytes = io.initial_stdin_message.len(),
            "control 通道初始 stdin 消息已写入");
    }

    // B5：进程组 kill 守卫——run future 被 drop（dispatch 超时 / /stop）时对整个
    // 进程组 killpg(SIGKILL)，连孙进程（MCP server、Bash 工具）一并收割。正常
    // wait 成功后 disarm（防 pid 复用误杀无关进程组）。
    #[cfg(unix)]
    let mut group_guard = child.id().map(GroupKillGuard::new);

    // 并发读 stderr，避免子进程 stderr 写满管道缓冲（~64KB）导致死锁。
    let stderr_handle = tokio::spawn(async move { read_stderr_to_string(stderr).await });

    let mut reader = BufReader::new(stdout);
    let mut session_id = String::new();
    let mut final_text = String::new();
    let mut error_text: Option<String> = None;
    let mut reached_terminal = false;
    // B10：非致命 error 事件累积（codex 顶层 `error`，可能瞬时重连）。不中断流；
    // 若最终无 final 文本，作为失败原因呈现。
    let mut transient_errors: Vec<String> = Vec::new();
    // usage 事件累积（合并语义：input/output 求和、cost 取最后非 None）。
    let mut usage_acc: Option<UsageStats> = None;
    // B1：真实 stdout IO 错误（管道 EIO 等，持续性）记录，最终无文本时并入诊断。
    let mut read_err: Option<String> = None;
    // canUseTool 控制通道（真机校准 2026-08-29 定位）：终态事件的 break 只跳出
    // 内层 for——mcp 模式靠子进程退出（EOF）收尾外层读循环，而 control 模式下
    // claude 跑完一轮**不退出**（stream-input 等 stdin 下一条），外层会永远等
    // 下一行 stdout。终态置位后跳出外层。
    let mut terminal_seen = false;
    // Task* 工具族的待办累积（claude 2.x 的 TodoWrite 后继，见 apply_task_tool）。
    let mut task_todos: Vec<(String, TodoItem)> = Vec::new();
    // TaskCreate 的 tool_use id → 占位行 index（结果到达后回填真实任务 id）。
    let mut pending_task_creates: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    // TaskList 调用的 tool_use id（结果到达即整表替换快照）。
    let mut pending_task_lists: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    loop {
        let line = match read_line_capped(&mut reader, MAX_STDOUT_LINE_BYTES).await {
            Ok(Some(l)) => l,
            Ok(None) => break,
            // B1：read_line_capped 的 Err 有两种语义——
            // - ErrorKind::InvalidInput：单行超 MAX_STDOUT_LINE_BYTES（无 \n 的超长
            //   输出，S-5 防僵尸行 OOM）→ 可跳过语义，跳过该行继续；
            // - 其它（管道 EIO / EBADF 等真实 IO 错误）→ 持续性，continue 会忙循环
            //   空转，记录后终止读取循环（已收集的 final/error 不受影响）。
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                tracing::warn!(
                    target: "imagent::backend",
                    backend = backend_name,
                    error = %e,
                    "stdout 单行超长，跳过该行"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    target: "imagent::backend",
                    backend = backend_name,
                    error = %e,
                    "stdout 读取 IO 错误，终止读取循环"
                );
                read_err = Some(e.to_string());
                break;
            }
        };
        // 诊断（control 通道真机校准）：前 3 行与之后每 50 行记录一次收流进度。
        {
            thread_local! { static N: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
            N.with(|n| {
                let i = n.get() + 1;
                n.set(i);
                {
                    let _ = i;
                    tracing::debug!(target: "imagent::backend", line_no = i,
                        head = %line.chars().take(160).collect::<String>(),
                        "stdout 行");
                }
            });
        }
        // B7：一行可产出多个事件（Multi 展平后逐个处理）。
        let mut events = Vec::new();
        flatten_event(parse(&line), &mut events);
        for ev in events {
            match ev {
                CliEvent::Session(id) => {
                    if session_id.is_empty() && !id.is_empty() {
                        session_id.clone_from(&id);
                        // P5-5：一经学到即通知 dispatch（中断/失败路径也能落库续接）。
                        let _ = chunks.send(AgentChunk::SessionStarted(id)).await;
                    }
                }
                // canUseTool 控制请求：经 permission.sock 复用既有审批闭环。
                // M3（code-review v8）：本臂在读循环内联 await（审批预算内）——
                // claude 同一轮的多个 canUseTool 因此**串行化**为串行 IM 审批，
                // 期间子进程其它输出滞留管道（~64KB 缓冲，极端时子进程写阻塞）。
                // 取舍：并发 responder 需拆 task + 真机校准多询问并发场景，
                // 当前接受串行语义（claude 单轮并行工具罕见）；stdin 生命周期
                // 已在终态路径闭环（drop → 5s wait → 组杀，见下方 wait 阶段）。
                //（IM 卡/👍/always/超时 fail-closed），决策回写子进程 stdin。
                CliEvent::ControlRequest {
                    request_id,
                    subtype,
                    tool_name,
                    input,
                } => match &control {
                    Some(io) => {
                        tracing::debug!(target: "imagent::backend",
                            request_id = %request_id, subtype = %subtype, tool = %tool_name,
                            "收到 control_request，经 permission.sock 询问");
                        let line = if subtype == "can_use_tool" {
                            let parsed_input: serde_json::Value =
                                serde_json::from_str(&input).unwrap_or(serde_json::json!({}));
                            let reply = match crate::mcp::ask_via_socket(
                                &io.sock,
                                &io.conv_id,
                                &request_id,
                                &tool_name,
                                &parsed_input,
                                io.ask_timeout,
                            )
                            .await
                            {
                                Ok(r) => r,
                                Err(e) => crate::permission::PermissionReply {
                                    allow: false,
                                    always: false,
                                    message: Some(format!("control 通道询问失败: {e}")),
                                    raw_text: None,
                                },
                            };
                            control_response_line(&request_id, &reply)
                        } else {
                            // 未知 subtype：回 error 控制响应防挂起（清单外形态，
                            // 真机校准补充）。
                            tracing::warn!(target: "imagent::backend", subtype = %subtype,
                                "未知 control_request subtype，回 error 响应");
                            format!(
                                "{}\n",
                                serde_json::json!({
                                    "type": "control_response",
                                    "response": {
                                        "subtype": "error",
                                        "request_id": request_id,
                                        "error": format!("unsupported subtype: {subtype}"),
                                    }
                                })
                            )
                        };
                        if let Some(w) = stdin_w.as_mut() {
                            if let Err(e) = w.write_all(line.as_bytes()).await {
                                tracing::warn!(target: "imagent::backend", error = %e,
                                    "control_response 回写失败（claude 可能挂起或自行 deny）");
                            }
                            let _ = w.flush().await;
                        }
                    }
                    None => {
                        tracing::warn!(target: "imagent::backend", backend = backend_name,
                            "收到 control_request 但未启用控制通道（claude_permission_channel=control 才应答）");
                    }
                },
                CliEvent::Text(t) => {
                    if !t.is_empty() {
                        let _ = chunks.send(AgentChunk::Text(t.clone())).await;
                        // B9：多条完整 agent_message 按序拼接（\n\n 分隔）。原先
                        // 「最后一次赋值胜出」会丢多消息 turn 的前几条内容。终止事件
                        // （Final）仍整体覆盖 final_text（claude result 权威文本语义
                        // 不变）；CliEvent 无 delta 概念，各 backend 的 Text 均为
                        // 完整消息，直接拼接。
                        if !final_text.is_empty() {
                            final_text.push_str("\n\n");
                        }
                        final_text.push_str(&t);
                    }
                }
                // W2-1：思考过程——仅推 chunk（卡片折叠展示），不进 final_text
                //（正文语义保持纯净，cot 档位控制展示）。
                CliEvent::Thought(t) => {
                    if !t.is_empty() {
                        let _ = chunks.send(AgentChunk::Thought(t)).await;
                    }
                }
                // W2-2：任务清单——全量替换语义，仅推 chunk。
                CliEvent::TodoList { items } => {
                    let _ = chunks.send(AgentChunk::TodoList { items }).await;
                }
                CliEvent::ToolUse {
                    ref tool,
                    ref input,
                    ..
                } if tool == "TaskCreate" || tool == "TaskUpdate" || tool == "TaskList" => {
                    // Task* 工具族 → 待办清单（全量快照语义，与 TodoWrite 路径
                    // 同一 chunk 类型）。仍落入下方通用 ToolUse 处理（工具轨迹
                    // 面板同样展示）——此处只追加清单 chunk。
                    let before = task_todos.len();
                    if apply_task_tool(&mut task_todos, tool, input) {
                        if tool == "TaskList" {
                            // 结果即全量权威快照（含桌面端创建的历史任务）。
                            if let Some(tuid) = ev_id_of(&ev) {
                                pending_task_lists.insert(tuid);
                            }
                        }
                        if tool == "TaskCreate" && before < task_todos.len() {
                            // 占位行 index 记账，等结果回填真实 id（会话级编号，
                            // --resume 后非 1 起）。
                            if let Some(tuid) = ev_id_of(&ev) {
                                pending_task_creates.insert(tuid, before);
                            }
                        }
                        let items = task_todos.iter().map(|(_, it)| it.clone()).collect();
                        let _ = chunks.send(AgentChunk::TodoList { items }).await;
                    }
                    // 重新匹配进通用臂：构造值消费。
                    let CliEvent::ToolUse {
                        tool,
                        input,
                        session,
                        id,
                    } = ev
                    else {
                        unreachable!("已按 ToolUse 分派")
                    };
                    if let Some(s) = session {
                        if session_id.is_empty() && !s.is_empty() {
                            session_id.clone_from(&s);
                            let _ = chunks.send(AgentChunk::SessionStarted(s)).await;
                        }
                    }
                    if let Some(path) = image_write_path(&tool, &input) {
                        let _ = chunks.send(AgentChunk::Media { path }).await;
                    }
                    let _ = chunks.send(AgentChunk::ToolUse { tool, input, id }).await;
                }
                CliEvent::ToolUse {
                    tool,
                    input,
                    session,
                    id,
                } => {
                    if let Some(s) = session {
                        if session_id.is_empty() && !s.is_empty() {
                            session_id.clone_from(&s);
                            let _ = chunks.send(AgentChunk::SessionStarted(s)).await;
                        }
                    }
                    if let Some(path) = image_write_path(&tool, &input) {
                        let _ = chunks.send(AgentChunk::Media { path }).await;
                    }
                    let _ = chunks.send(AgentChunk::ToolUse { tool, input, id }).await;
                }
                CliEvent::ToolResult {
                    ref tool,
                    ref output,
                    ref id,
                } if tool == "TaskCreate"
                    && id
                        .as_deref()
                        .is_some_and(|t| pending_task_creates.contains_key(t)) =>
                {
                    // TaskCreate 结果回填真实任务 id（或失败移除占位行）。
                    let tuid = id.clone().unwrap();
                    if let Some(index) = pending_task_creates.remove(&tuid) {
                        if resolve_task_create_result(&mut task_todos, index, output) {
                            let items = task_todos.iter().map(|(_, it)| it.clone()).collect();
                            let _ = chunks.send(AgentChunk::TodoList { items }).await;
                        }
                    }
                    let CliEvent::ToolResult { tool, output, id } = ev else {
                        unreachable!("已按 ToolResult 分派")
                    };
                    let _ = chunks
                        .send(AgentChunk::ToolResult { tool, output, id })
                        .await;
                }
                CliEvent::ToolResult {
                    ref tool,
                    ref output,
                    ref id,
                } if tool == "TaskList"
                    && id.as_deref().is_some_and(|t| pending_task_lists.remove(t)) =>
                {
                    // TaskList 结果 = 全量权威快照：整表替换（含桌面端历史任务）。
                    if let Some(snapshot) = parse_task_list_snapshot(output) {
                        task_todos = snapshot;
                        let items = task_todos.iter().map(|(_, it)| it.clone()).collect();
                        let _ = chunks.send(AgentChunk::TodoList { items }).await;
                    }
                    let CliEvent::ToolResult { tool, output, id } = ev else {
                        unreachable!("已按 ToolResult 分派")
                    };
                    let _ = chunks
                        .send(AgentChunk::ToolResult { tool, output, id })
                        .await;
                }
                CliEvent::ToolResult { tool, output, id } => {
                    let _ = chunks
                        .send(AgentChunk::ToolResult { tool, output, id })
                        .await;
                }
                CliEvent::Final { text, session } => {
                    if let Some(s) = session {
                        if session_id.is_empty() && !s.is_empty() {
                            let _ = chunks.send(AgentChunk::SessionStarted(s.clone())).await;
                        }
                        session_id = s;
                    }
                    final_text = text;
                    reached_terminal = true; // N8：标记由终止事件产出（非中间 Text 后 EOF）
                    terminal_seen = true;
                    break;
                }
                CliEvent::Error { text, session } => {
                    if let Some(s) = session {
                        if session_id.is_empty() && !s.is_empty() {
                            let _ = chunks.send(AgentChunk::SessionStarted(s.clone())).await;
                        }
                        session_id = s;
                    }
                    error_text = Some(text);
                    terminal_seen = true;
                    break;
                }
                CliEvent::Terminal { session } => {
                    if let Some(s) = session {
                        if session_id.is_empty() && !s.is_empty() {
                            let _ = chunks.send(AgentChunk::SessionStarted(s.clone())).await;
                        }
                        session_id = s;
                    }
                    reached_terminal = true;
                    terminal_seen = true;
                    break;
                }
                // B10：非致命 error 事件——不中断流（保留「可能瞬时重连」考量），
                // 仅记录，待失败路径（final 为空）作为失败原因呈现。
                CliEvent::TransientError(text) => {
                    tracing::warn!(
                        target: "imagent::backend",
                        backend = backend_name,
                        error = %text,
                        "CLI error 事件（不中断流，已记录备查）"
                    );
                    transient_errors.push(text);
                }
                // token 用量累积——不中断流，不推 chunk（usage 只进 RunOutcome/
                // metrics，不是 IM 可读内容）。
                CliEvent::Usage(u) => {
                    usage_acc = Some(match usage_acc {
                        Some(acc) => acc.merge(u),
                        None => u,
                    });
                }
                // 展平阶段已处理，运行期不应到达。
                CliEvent::Multi(_) => {}
                CliEvent::Skip => {}
            }
        }
        if terminal_seen {
            break;
        }
    }

    // canUseTool 控制通道（真机校准 2026-08-29 实测）：`--input-format stream-json`
    // 下 stdin 保持打开时，claude 跑完一轮**不退出**（流式会话等下一条消息）——
    // 终态事件已收到即无需再等它：先关 stdin（EOF 后 claude 自行退出），再带
    // 超时 wait，超时则 kill（kill_on_drop/进程组守卫兜底收尾孙进程）。
    tracing::debug!(target: "imagent::backend",
        final_len = final_text.len(), terminal = reached_terminal,
        err = error_text.is_some(), read_err = read_err.is_some(),
        "stdout 读取循环退出（进入 wait 阶段）");
    if control.is_some() {
        drop(stdin_w.take());
        tracing::debug!(target: "imagent::backend", "control 通道已关 stdin，等 claude 退出（5s 超时）");
        match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
            Ok(res) => {
                tracing::debug!(target: "imagent::backend", status = ?res, "control 通道 claude 已退出");
            }
            Err(_) => {
                tracing::warn!(target: "imagent::backend",
                    "control 通道 claude 终态后未随 stdin EOF 退出，kill 整组收尾");
                #[cfg(unix)]
                if let Some(g) = group_guard.as_mut() {
                    g.kill_now();
                }
                let _ = child.kill().await;
            }
        }
    }
    let status = child.wait().await;
    // B5：正常 wait 返回 → 进程组主进程已退出，disarm 防 pid 复用误杀无关进程组。
    #[cfg(unix)]
    if let Some(g) = group_guard.as_mut() {
        g.disarm();
    }
    let stderr_msg = stderr_handle.await.unwrap_or_default();

    if let Some(t) = error_text {
        // 真机校准：claude resume 幽灵会话等场景产出 is_error 且 result 文本缺失
        // 的终止事件（空字符串）——空文本对用户零信息量，回落到 diagnose（exit
        // code + stderr 至少可排障）。
        let t = if t.trim().is_empty() {
            diagnose(&status, &stderr_msg, backend_name, reached_terminal)
        } else {
            t
        };
        let _ = chunks.send(AgentChunk::Error(t.clone())).await;
        return Err(CoreError::Backend(backend_name, t));
    }

    if final_text.is_empty() {
        // B10：final 为空但沿途收到过 error 事件（如「API key invalid」）——作为
        // 失败原因呈现，不再让致命错误被吞成无信息量的 diagnose 文案。
        if !transient_errors.is_empty() {
            let t = transient_errors.join("; ");
            let _ = chunks.send(AgentChunk::Error(t.clone())).await;
            return Err(CoreError::Backend(backend_name, t));
        }
        // B1：真实 stdout IO 错误并入诊断。
        let mut msg = diagnose(&status, &stderr_msg, backend_name, reached_terminal);
        if let Some(e) = read_err {
            msg.push_str(&format!("; stdout read failed: {e}"));
        }
        return Err(CoreError::Backend(backend_name, msg));
    }

    // N8：final_text 非空但未由终止事件产出（仅中间 Text 后 stdout EOF）且 exit 非 0 →
    // agent 非正常终止（如 OOM / segfault）。不静默当成功：warn 标注。仍返回已收到的
    // 部分文本（IM 场景拿到结果比报错有用；session_id 可能空，由 dispatch 判空不入库）。
    if !reached_terminal {
        if let Ok(s) = &status {
            if !s.success() {
                tracing::warn!(
                    target: "imagent::backend",
                    backend = backend_name,
                    exit = %s,
                    "agent 非正常终止（未发 Final/Terminal 事件，exit 非 0），返回已收到的部分文本"
                );
            }
        }
    }

    let _ = chunks.send(AgentChunk::Final(final_text.clone())).await;
    Ok(RunOutcome {
        session_id: SessionId(session_id),
        final_text,
        terminal: reached_terminal,
        usage: usage_acc,
        // CLI 后端的终止原因在 result 事件里无对应字段（is_error 走 Err 路径），
        // 恒 None；ACP 后端由 PromptResponse.stopReason 填充。
        stop_reason: None,
    })
}

/// 无最终文本时的诊断：区分「正常终止但无文本」与「未终止/异常」。
fn diagnose(
    status: &std::io::Result<std::process::ExitStatus>,
    stderr: &str,
    name: &str,
    reached_terminal: bool,
) -> String {
    let code = match status {
        Ok(s) => format!("exit {s}"),
        Err(e) => format!("wait failed: {e}"),
    };
    let stderr_trim = stderr.trim();
    let head = if reached_terminal {
        format!("{name} terminated without text ({code})")
    } else {
        format!("{name} produced no result event ({code})")
    };
    if stderr_trim.is_empty() {
        head
    } else {
        format!("{head}; stderr: {stderr_trim}")
    }
}

/// 把子进程 stderr 读到字符串（非阻断）。三 CLI backend 共享。
///
/// S-5：双层上限防 OOM——
/// - 单行上限 [`MAX_STDERR_LINE_BYTES`]（按字节读行，防无 `\n` 的超长流——可被 prompt
///   injection 构造——单行全量分配撑爆内存）；
/// - 总量上限 [`MAX_STDERR_BYTES`]（超限截断 + 截断标记）。
///
/// 任一超限后继续 drain（不 break），防子进程 stderr 管道写满 ~64KB 阻塞子进程。
/// B1：真实 IO 错误（非 InvalidInput 超长语义）warn 并终止读取（忙循环无意义）。
pub async fn read_stderr_to_string(stderr: tokio::process::ChildStderr) -> String {
    let mut reader = BufReader::new(stderr);
    let mut total = 0usize;
    let mut truncated = false;
    let mut buf = Vec::new();
    loop {
        match read_line_capped(&mut reader, MAX_STDERR_LINE_BYTES).await {
            Ok(Some(line)) => {
                if truncated {
                    continue; // 已超上限：继续 drain 防管道阻塞，但不累积。
                }
                total += line.len() + 1;
                if total > MAX_STDERR_BYTES {
                    truncated = true;
                    buf.push(format!(
                        "…[stderr 截断：超过 {MAX_STDERR_BYTES} 字节上限，丢弃后续]"
                    ));
                    continue;
                }
                buf.push(line);
            }
            Ok(None) => break, // EOF
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                // B1（可跳过语义）：单行超 MAX_STDERR_LINE_BYTES（无 `\n` 超长输出）：
                // read_line_capped 已 consume 到上限点。push 截断标记并继续 drain 该行
                // 剩余（不累积），防 OOM。
                if !truncated {
                    truncated = true;
                    buf.push(format!(
                        "…[stderr 单行超过 {MAX_STDERR_LINE_BYTES} 字节，截断并丢弃后续]"
                    ));
                }
            }
            Err(e) => {
                // B1（持续性语义）：真实 IO 错误（管道 EIO 等）——继续 loop 只会忙循环
                // 空转到 EOF 永不可达。warn 并终止读取，返回已累积内容。
                tracing::warn!(
                    target: "imagent::backend",
                    error = %e,
                    "stderr 读取 IO 错误，终止读取"
                );
                break;
            }
        }
    }
    buf.join("\n")
}

/// stdout 单行字节上限（S-5）：防 agent 输出无 `\n` 的超长行（如 base64 流）
/// 被全量分配撑爆内存。
const MAX_STDOUT_LINE_BYTES: usize = 8 * 1024 * 1024;

/// stderr 累积字节上限（S-5）：长会话 stderr 膨胀，超限截断。
const MAX_STDERR_BYTES: usize = 64 * 1024;

/// stderr 单行字节上限（S-5）：防 agent 向 stderr 写无 `\n` 的超长流（可被 prompt
/// injection 构造）导致单行全量分配 OOM。与 stdout 的 [`MAX_STDOUT_LINE_BYTES`] 对称。
const MAX_STDERR_LINE_BYTES: usize = 1024 * 1024;

/// 按字节读一行，上限 `max_bytes`（S-5）。超限返回 Err（调用方跳过该行）。
/// 覆盖 `AsyncBufReadExt::lines()` 无上限的语义（一行无 `\n` 的超长输出会全量分配）。
/// B1：Err 语义二分——`ErrorKind::InvalidInput` = 单行超长（调用方可跳行继续）；
/// 其它 kind = 真实 IO 错误（持续性，调用方应终止读取）。
async fn read_line_capped<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if buf.is_empty() {
                Ok(None)
            } else {
                Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
            };
        }
        if let Some(nl) = available.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&available[..=nl]);
            reader.consume(nl + 1);
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
        buf.extend_from_slice(available);
        let n = available.len();
        reader.consume(n);
        if buf.len() > max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("line exceeds {max_bytes} bytes"),
            ));
        }
    }
}

/// 视为需要写/执行权限的工具名（codex sandbox / gemini approval 收敛用，大小写敏感）。
/// codex/gemini 原各自定义且逐字相同。
pub const WRITE_OR_EXEC: &[&str] = &[
    "Edit",
    "Write",
    "Bash",
    "MultiEdit",
    "NotebookEdit",
    "WriteQuery",
    "execute_bash",
];

/// `allowed_tools` 是否为「不限制」（全量工具）语义：空列表（未收敛/不指定）
/// 或显式 `["*"]`。各 backend 对此统一取自己的最宽档：claude 不附加
/// `--allowedTools`（CLI 自身默认 = 全量）；codex `workspace-write`（按设计
/// 绝不自动 danger-full-access）；gemini `auto_edit`。
pub fn tools_unrestricted(tools: &[String]) -> bool {
    tools.is_empty() || tools.iter().any(|t| t == "*")
}

/// `env_clear()` 后始终透传给 agent 子进程的运行时必需变量（S-2）。
///
/// - `PATH`/`HOME`/`USER`/`LOGNAME`：子进程找可执行、读自身配置的最小必需；
/// - `LANG`/`LC_ALL`/`LC_CTYPE`/`TZ`：locale 与时区，缺 `LANG` 有的 CLI 报 UTF-8 警告；
/// - `TMPDIR`：临时目录。
///
/// 其余 env（含各类 `*_API_KEY`、`DATABASE_URL`、CI secret 等）一律不透传——
/// 由各 backend 经 `spawn_cli_backend` 的 `passthrough_env` 显式声明自己所需 key。
const ALWAYS_PASSTHROUGH_ENV: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR",
];

#[cfg(test)]
mod tests {
    use super::{
        apply_task_tool, image_write_path, parse_task_list_snapshot, resolve_task_create_result,
        spawn_cli_backend, CliEvent, ControlIo,
    };
    use crate::types::{TodoItem, TodoStatus};

    /// B9：多条 agent_message（Text 事件）应按序拼接（\n\n 分隔）进 final_text，
    /// 而非最后一条覆盖（会丢多消息 turn 的前几条内容）。
    #[cfg(unix)]
    #[tokio::test]
    async fn multiple_text_messages_concatenate_into_final_text() {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg("printf 'one\\ntwo\\n'");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::types::AgentChunk>(64);
        let parse = |line: &str| CliEvent::Text(line.trim_end().to_string());
        let outcome = spawn_cli_backend(cmd, parse, tx, "test-backend", &[], None)
            .await
            .expect("echo run 应成功");
        assert_eq!(outcome.final_text, "one\n\ntwo");
        // 两个 Text chunk 均已推送。
        let mut texts = Vec::new();
        while let Ok(c) = rx.try_recv() {
            if let crate::types::AgentChunk::Text(t) = c {
                texts.push(t);
            }
        }
        assert_eq!(texts, vec!["one".to_string(), "two".to_string()]);
    }

    /// B10：final 为空但沿途有 TransientError（如「API key invalid」）时，
    /// 错误内容应作为失败原因返回（Err），而非无信息量的 diagnose 文案。
    #[cfg(unix)]
    #[tokio::test]
    async fn transient_errors_surface_when_final_empty() {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg("printf 'ERRLINE\\n'");
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::types::AgentChunk>(64);
        let parse = |line: &str| {
            if line.trim() == "ERRLINE" {
                CliEvent::TransientError("API key invalid".to_string())
            } else {
                CliEvent::Skip
            }
        };
        let err = spawn_cli_backend(cmd, parse, tx, "test-backend", &[], None)
            .await
            .expect_err("无 final 文本应失败");
        assert!(
            err.to_string().contains("API key invalid"),
            "错误信息应含 error 事件内容: {err}"
        );
    }

    #[test]
    fn image_write_path_detects_write_png() {
        assert_eq!(
            image_write_path("Write", r#"{"file_path":"/tmp/a.png","content":"x"}"#),
            Some("/tmp/a.png".to_string())
        );
        assert_eq!(
            image_write_path("Write", r#"{"file_path":"out.JPG"}"#),
            Some("out.JPG".to_string())
        );
    }

    #[test]
    fn image_write_path_ignores_non_image_or_other_tool() {
        assert_eq!(image_write_path("Write", r#"{"file_path":"a.txt"}"#), None);
        assert_eq!(
            image_write_path("Bash", r#"{"command":"cp x.png y.png"}"#),
            None
        );
        assert_eq!(image_write_path("Write", "not-json"), None);
        assert_eq!(image_write_path("Write", r#"{"content":"x"}"#), None);
        assert_eq!(image_write_path("Write", r#"{"file_path":"noext"}"#), None);
    }

    /// W2-2：TodoWrite 入参解析为任务清单；非 TodoWrite / 坏 JSON / 空 todos 回 None。
    #[test]
    fn todo_write_items_parses_and_rejects() {
        let items = super::todo_write_items(
            "TodoWrite",
            r#"{"todos":[
                {"content":"分析需求","status":"completed"},
                {"content":"写代码","status":"in_progress"},
                {"content":"测试","status":"pending"}
            ]}"#,
        )
        .expect("合法 TodoWrite 应解析");
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].status, crate::types::TodoStatus::Completed);
        assert_eq!(items[1].status, crate::types::TodoStatus::InProgress);
        assert_eq!(items[2].status, crate::types::TodoStatus::Pending);
        assert_eq!(items[0].text, "分析需求");

        assert!(super::todo_write_items("Bash", r#"{"todos":[]}"#).is_none());
        assert!(super::todo_write_items("TodoWrite", "not-json").is_none());
        assert!(super::todo_write_items("TodoWrite", r#"{"todos":[]}"#).is_none());
        assert!(super::todo_write_items("TodoWrite", r#"{}"#).is_none());
    }

    /// Usage 事件累积进 RunOutcome.usage（合并：input/output 求和、cost 取最后）；
    /// 与 Final 同批（Multi）时 Usage 须在前（Final break 循环）。
    #[cfg(unix)]
    #[tokio::test]
    async fn usage_events_accumulate_into_outcome() {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg("printf 'U1\\nU2\\nEND\\n'");
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::types::AgentChunk>(64);
        let parse = |line: &str| match line.trim() {
            "U1" => CliEvent::Usage(crate::types::UsageStats {
                input_tokens: 10,
                output_tokens: 5,
                cached_tokens: Some(2),
                total_cost_usd: None,
            }),
            "U2" => CliEvent::Usage(crate::types::UsageStats {
                input_tokens: 1,
                output_tokens: 2,
                cached_tokens: None,
                total_cost_usd: Some(0.012),
            }),
            "END" => CliEvent::Multi(vec![
                CliEvent::Usage(crate::types::UsageStats {
                    input_tokens: 0,
                    output_tokens: 0,
                    cached_tokens: None,
                    total_cost_usd: Some(0.05),
                }),
                CliEvent::Final {
                    text: "done".into(),
                    session: None,
                },
            ]),
            _ => CliEvent::Skip,
        };
        let outcome = spawn_cli_backend(cmd, parse, tx, "test-backend", &[], None)
            .await
            .expect("run 应成功");
        let u = outcome.usage.expect("应累积出 usage");
        assert_eq!(u.input_tokens, 11);
        assert_eq!(u.output_tokens, 7);
        assert_eq!(u.cached_tokens, Some(2));
        assert_eq!(u.total_cost_usd, Some(0.05)); // 最后非 None 胜出
        assert_eq!(outcome.final_text, "done");
    }
    /// control 通道真机校准回归：终态事件（result）后子进程不退出（stream-input
    /// 语义：stdin 开着等下一条消息）——spawn_cli_backend 必须收尾返回（关 stdin
    /// → 5s 超时 kill），不得永久挂起。假 claude 脚本复现真机事件序列。
    #[tokio::test]
    async fn control_channel_terminal_with_alive_child_returns() {
        let script = "/tmp/fake_claude.sh";
        if !std::path::Path::new(script).exists() {
            return; // 环境无脚本（CI）：跳过——形态由真机校准背书。
        }
        let cmd = tokio::process::Command::new(script);
        let parse = |line: &str| -> CliEvent {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("type").and_then(|t| t.as_str()) == Some("result") {
                    return CliEvent::Final {
                        text: v
                            .get("result")
                            .and_then(|r| r.as_str())
                            .unwrap_or("")
                            .into(),
                        session: v
                            .get("session_id")
                            .and_then(|s| s.as_str())
                            .map(String::from),
                    };
                }
            }
            CliEvent::Skip
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(32);
        let io = ControlIo {
            sock: "/nonexistent/permission.sock".into(),
            conv_id: "test-conv".into(),
            ask_timeout: std::time::Duration::from_secs(2),
            initial_stdin_message: "{\"type\":\"user\"}\n".into(),
        };
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            spawn_cli_backend(cmd, parse, tx, "test-ctrl", &[], Some(io)),
        )
        .await;
        assert!(
            res.is_ok(),
            "spawn_cli_backend 应在终态后收尾返回（不挂起）"
        );
        let outcome = res.unwrap().expect("run ok");
        assert!(outcome.terminal, "终态事件应被识别");
        assert!(
            outcome.final_text.contains("2026-08-29"),
            "final 文本: {}",
            outcome.final_text
        );
    }
    /// 真机校准（2026-08-30）：Task* 工具族（claude 2.x 的 TodoWrite 后继）
    /// → 待办快照。Create 无 id（占位）→ 结果 "Task #N created" 回填真实 id
    ///（会话级连续编号：--resume 后新任务 #3 而非 #1）；Update 按 true id 翻状态。
    #[test]
    fn task_tools_build_todo_snapshot() {
        let mut list: Vec<(String, TodoItem)> = Vec::new();
        // 创建两行（占位空 id）。
        assert!(apply_task_tool(
            &mut list,
            "TaskCreate",
            r#"{"subject":"foo"}"#
        ));
        assert!(apply_task_tool(
            &mut list,
            "TaskCreate",
            r#"{"subject":"bar"}"#
        ));
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|(id, _)| id.is_empty()), "创建时占位空 id");
        // 结果回填：真实 id #7/#8（跨轮续编形态）。
        assert!(resolve_task_create_result(
            &mut list,
            0,
            "Task #7 created successfully: foo"
        ));
        assert!(resolve_task_create_result(
            &mut list,
            1,
            "Task #8 created successfully: bar"
        ));
        assert_eq!(list[0].0, "7");
        assert_eq!(list[1].0, "8");
        // Update 按 true id 翻状态（旧按创建序 1..N 的错位不复存在）。
        assert!(apply_task_tool(
            &mut list,
            "TaskUpdate",
            r#"{"taskId":"7","status":"in_progress"}"#
        ));
        assert_eq!(list[0].1.status, TodoStatus::InProgress);
        assert!(apply_task_tool(
            &mut list,
            "TaskUpdate",
            r#"{"status":"completed","taskId":"7"}"#
        ));
        assert_eq!(list[0].1.status, TodoStatus::Completed);
        // 无 status 的结构更新 / 未知 id / 非法输入：no-op。
        assert!(!apply_task_tool(
            &mut list,
            "TaskUpdate",
            r#"{"taskId":"8","addBlockedBy":["7"]}"#
        ));
        assert!(!apply_task_tool(
            &mut list,
            "TaskUpdate",
            r#"{"taskId":"9","status":"completed"}"#
        ));
        assert!(!apply_task_tool(&mut list, "TaskCreate", "not json"));
        // 创建失败（结果无 Task #N）：占位行移除。
        let mut fail = vec![(
            String::new(),
            TodoItem {
                text: "x".into(),
                status: TodoStatus::Pending,
            },
        )];
        assert!(resolve_task_create_result(
            &mut fail,
            0,
            "Error: task tool disabled"
        ));
        assert!(fail.is_empty(), "失败创建移除占位行");
    }
    /// 真机校准（2026-08-30）：TaskList 结果快照（`#N [status] subject`）——
    /// 含桌面端历史任务；杂行跳过；无可解析行返回 None（不动既有清单）。
    #[test]
    fn task_list_snapshot_parses_authoritative_view() {
        let snap = parse_task_list_snapshot(
            "#1 [completed] 盘点 Makefile\n#2 [in_progress] 编写 justfile\n#7 [pending] 桌面端遗留任务\n杂行\n#x [bad]",
        )
        .expect("应解析出 3 行");
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].0, "1");
        assert_eq!(snap[0].1.status, TodoStatus::Completed);
        assert_eq!(snap[1].1.text, "编写 justfile");
        assert_eq!(snap[1].1.status, TodoStatus::InProgress);
        assert_eq!(snap[2].0, "7", "桌面端遗留任务按真实 id 进入面板");
        assert_eq!(snap[2].1.status, TodoStatus::Pending);
        // 空/全杂行 → None。
        assert!(parse_task_list_snapshot("no tasks here").is_none());
        assert!(parse_task_list_snapshot("").is_none());
    }
}
