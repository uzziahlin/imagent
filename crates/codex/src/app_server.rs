//! `codex app-server` 双工协议客户端（JSON-RPC over stdio）。
//!
//! 与 `codex exec --json` 单发流不同，`codex app-server` 是长驻子进程上的
//! JSON-RPC 2.0 双工会话：客户端发请求（`thread/start`、`turn/start`…），
//! 服务端回响应 + 推通知（`item/completed`…），还会**反向发请求**（审批
//! `item/commandExecution/requestApproval` 等），客户端须回执。
//!
//! ## 真机信封实证（2026-10-31，codex-cli 0.166.0，`codex app-server`
//! 手动交互实测，样本脱敏自 /tmp/appserver_live*.log）
//!
//! 服务端的响应/通知**不带 `jsonrpc` 字段**（宽松信封），解析必须按
//! 「有 method → 通知/server 请求；有 id → 响应」分类而不能要求 `"jsonrpc":"2.0"`；
//! 客户端发出的帧带不带 `jsonrpc` 均被接受（实测确认），本实现统一带上（标准形）：
//!
//! ```text
//! （客户端→） {"jsonrpc":"2.0","id":3,"method":"thread/start","params":{"cwd":"/tmp","approvalPolicy":"on-request"}}
//! （服务端→） {"id":3,"result":{"thread":{"id":"01a1902a-c0f9-7370-ad53-bdf52f2c345d",...},"approvalPolicy":"on-request",...}}
//! （服务端→） {"method":"thread/started","params":{"thread":{"id":"01a1902a-...",...}},"emittedAtMs":1793420083587}
//! （服务端→） {"method":"turn/started","params":{"threadId":"01a1902a-...","turn":{"id":"01a1902a-cab1-...","status":"inProgress",...}}}
//! （服务端→） {"method":"item/started","params":{"item":{"type":"reasoning","id":"rs_...","summary":[],...},"threadId":"...","turnId":"..."}}
//! （服务端→） {"method":"item/agentMessage/delta","params":{"threadId":"...","turnId":"...","itemId":"resp_...","delta":"OK"}}
//! （服务端→） {"method":"turn/completed","params":{"threadId":"...","turn":{"id":"...","status":"completed","items":[...]}}}
//! （服务端→） {"error":{"code":-32600,"message":"invalid thread id: ..."},"id":4}
//! ```
//!
//! 审批闭环实测（approvalPolicy=untrusted，命令执行触发）：
//!
//! ```text
//! （服务端→） {"method":"item/commandExecution/requestApproval","id":0,"params":{"kind":"command","threadId":"...","turnId":"...","itemId":"call_...","command":"/bin/zsh -lc 'echo live-probe-approval-42 && echo done'","cwd":"/private/tmp",...}}
//! （客户端→） {"jsonrpc":"2.0","id":0,"result":{"decision":"accept"}}
//! （服务端→） {"method":"item/completed","params":{"item":{"type":"commandExecution",...,"aggregatedOutput":"live-probe-approval-42\ndone\n","exitCode":0,"status":"completed",...}}}
//! ```
//!
//! 与 schema（/tmp/codex-schema/，0.166 `generate-json-schema` 产物）的差异点：
//! - v2 schema 的 `Turn` 无 `usage` 字段——token 用量走独立通知
//!   `thread/tokenUsage/updated`（`tokenUsage.last/total`，camelCase
//!   `inputTokens/outputTokens/cachedInputTokens` + `modelContextWindow`），
//!   实测确认 `turn/completed` 不携带 usage；
//! - item 类型判别符是 camelCase（`agentMessage`/`commandExecution`），exec
//!   JSONL 线是 snake_case（`agent_message`）；
//! - reasoning 的文本在 `item.summary: [..]` 数组（exec 线在 `item.text`）；
//! - 待办的结构化形态是通知 `turn/plan/updated`（`plan[].step/status`），
//!   v2 无 exec 线的 `todo_list` item。
//!
//! 全部错误 fail-open 上抛（连接死/超时 → `Err`，与 exec 版同语义）。

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use imagent_core::{agent_process::AGENT_RUNTIME_ENV, CoreError, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::debug;

/// 客户端→服务端请求超时（任务契约：30s；`thread/start` 冷启动 + `turn/start`
/// 派发均在秒级，30s 已含慢盘/慢网余量）。
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 服务端 → 客户端通知（无 id 的 JSON-RPC notification）。
#[derive(Debug, Clone)]
pub struct IncomingNotification {
    pub method: String,
    pub params: Value,
}

/// 客户端回执载荷：正常 result 或 JSON-RPC error 帧（如 `item/tool/call`
/// 这类本批未实现的服务端请求，回 -32601 让服务端显式失败而非悬挂）。
#[derive(Debug, Clone)]
pub enum RpcReply {
    Result(Value),
    Error { code: i64, message: String },
}

/// 服务端 → 客户端请求（审批四类 + `item/tool/call` 等）。带**响应回执
/// sender**：`respond`/`approval_respond` 消费 self 把回执帧写回 stdin——
/// id 绑定在请求对象上，不存在「响应 id 与请求错配」的可能。
#[derive(Debug)]
pub struct IncomingRequest {
    pub id: Value,
    pub method: String,
    pub params: Value,
    reply: oneshot::Sender<RpcReply>,
}

impl IncomingRequest {
    /// 测试构造入口（生产的唯一构造源是 reader task 的帧分类）。
    #[cfg(test)]
    pub(crate) fn new(
        id: Value,
        method: String,
        params: Value,
    ) -> (Self, oneshot::Receiver<RpcReply>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                id,
                method,
                params,
                reply: tx,
            },
            rx,
        )
    }

    /// 通用回执（result 或 error 帧）。
    pub fn respond(self, reply: RpcReply) -> bool {
        self.reply.send(reply).is_ok()
    }

    /// 审批回执（schema：CommandExecutionRequestApprovalResponse /
    /// FileChangeRequestApprovalResponse 顶层 `{"decision": ...}`）。
    /// decision 取值：`accept` / `decline`（fileChange 另有 `cancel`、
    /// `acceptForSession`——本层不用：IM 闭环每次都要过人，会话级缓存会
    /// 绕过审批卡）。
    pub fn approval_respond(self, decision: &str) -> bool {
        self.respond(RpcReply::Result(json!({ "decision": decision })))
    }
}

/// app-server 子进程连接（每轮 run 一个：spawn → initialize → thread →
/// turn → 事件流 → drop；kill_on_drop 兜底，与 exec 版同生命周期语义）。
pub struct AppServerProcess {
    child: Child,
    stdin_tx: mpsc::UnboundedSender<String>,
    /// 请求 id → 回执 oneshot（`request()` 注册，reader task 消费）。
    pending: ArcPending,
    next_id: AtomicU64,
    /// 通知/服务端请求通道：tokio Mutex 包一层使 `next_*` 可用 `&self` 调用
    ///（backend 的 run 事件循环在同一 `select!` 里要同时 await 通知、请求、
    /// steering、死亡信号四个源；`&mut` 接口会造成多重可变借用编译失败）。
    /// tokio MutexGuard 可跨 await 且 Send，不污染调用方 future 的 Send 性。
    notifications: tokio::sync::Mutex<mpsc::UnboundedReceiver<IncomingNotification>>,
    requests: tokio::sync::Mutex<mpsc::UnboundedReceiver<IncomingRequest>>,
    /// 连接死亡信号（reader task 退出置 true；`request()` 前置检查用）。
    closed_rx: watch::Receiver<bool>,
    request_timeout: Duration,
}

impl AppServerProcess {
    /// spawn 长驻 `codex app-server`（stdin/stdout JSONL 双工，stderr 管道日志）。
    pub async fn spawn(workdir: &Path) -> Result<Self> {
        Self::spawn_configured("codex", workdir, REQUEST_TIMEOUT).await
    }

    /// 可配置形态（测试注入 mock 可执行与短超时；生产走 [`spawn`]）。
    pub(crate) async fn spawn_configured(
        program: &str,
        workdir: &Path,
        request_timeout: Duration,
    ) -> Result<Self> {
        let mut cmd = Command::new(program);
        cmd.arg("app-server");
        cmd.current_dir(workdir);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        // unix 进程组隔离（对齐 exec 版 spawn_cli_backend）：app-server 执行
        // 命令类 turn 时会派生孙进程，kill_on_drop 只杀直接子进程，进程组
        // 保证 /stop 等中断路径不留孤儿（组级 SIGKILL 守卫属 exec 专有，此处
        // 依赖每轮 run 结束 drop + 进程组随组长退出由 OS 回收）。
        #[cfg(unix)]
        cmd.process_group(0);
        // S-2 最小授权（对齐 exec 版）：env_clear + 运行时必需变量 + codex 所需
        // 凭据/端点。注意代理变量（HTTP(S)_PROXY）**不在白名单**——app-server 的
        // 模型流量目标可能是 loopback（本机 cc-switch 代理 127.0.0.1:15721），
        // 继承代理变量会被 Clash 劫持成 502（任务实测结论）；白名单天然隔离。
        cmd.env_clear();
        for &key in AGENT_RUNTIME_ENV
            .iter()
            .chain(["OPENAI_API_KEY", "OPENAI_BASE_URL"].iter())
        {
            if let Ok(val) = std::env::var(key) {
                cmd.env(key, val);
            }
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| CoreError::Backend("codex-app", format!("spawn codex app-server: {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CoreError::Backend("codex-app", "app-server stdin 不可用".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CoreError::Backend("codex-app", "app-server stdout 不可用".into()))?;
        let stderr = child.stderr.take();

        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
        let (notif_tx, notifications_rx) = mpsc::unbounded_channel::<IncomingNotification>();
        let (req_tx, requests_rx) = mpsc::unbounded_channel::<IncomingRequest>();
        let (closed_tx, closed_rx) = watch::channel(false);
        let pending = ArcPending(std::sync::Arc::new(StdMutex::new(HashMap::new())));
        let pending_for_reader = pending.clone();
        // stderr → 日志（管道消费防子进程写阻塞；app-server 启动期配置告警走这里）。
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!(target: "imagent::codex-app", stderr = %line, "app-server stderr");
                }
            });
        }

        // 写任务：出站帧（请求 + 审批回执）统一经此串行写 stdin。
        tokio::spawn(async move {
            let mut stdin = stdin;
            let mut buf = String::new();
            while let Some(line) = stdin_rx.recv().await {
                buf.clear();
                buf.push_str(&line);
                buf.push('\n');
                if stdin.write_all(buf.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        // 读任务：逐行分类（宽松信封，见模块注释）→ 响应关联 / 通知 / 服务端请求。
        let reader_stdin_tx = stdin_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                dispatch_frame(
                    &line,
                    &pending_for_reader,
                    &notif_tx,
                    &req_tx,
                    &reader_stdin_tx,
                );
            }
            // 连接死亡（EOF/读错误）：置位 + fail-open 冲掉全部在途请求。
            fail_all_pending(&pending_for_reader, "app-server 连接已断（stdout EOF）");
            closed_tx.send_replace(true);
        });

        Ok(Self {
            child,
            stdin_tx,
            pending,
            next_id: AtomicU64::new(1),
            notifications: tokio::sync::Mutex::new(notifications_rx),
            requests: tokio::sync::Mutex::new(requests_rx),
            closed_rx,
            request_timeout,
        })
    }

    /// 发送客户端→服务端请求并等响应（id 计数关联 + 30s 超时；连接死/超时 → Err）。
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if *self.closed_rx.borrow() {
            return Err(CoreError::Backend(
                "codex-app",
                "app-server 连接已断".into(),
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        let frame = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        if self.stdin_tx.send(frame.to_string()).is_err() {
            self.pending.lock().remove(&id);
            return Err(CoreError::Backend(
                "codex-app",
                "app-server stdin 已关闭".into(),
            ));
        }
        match tokio::time::timeout(self.request_timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => {
                // reader 退出时 pending 已被冲掉（oneshot sender drop）——此处
                // recv Err 即连接死亡语义。
                self.pending.lock().remove(&id);
                Err(CoreError::Backend(
                    "codex-app",
                    "app-server 连接已断".into(),
                ))
            }
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(CoreError::Backend(
                    "codex-app",
                    format!(
                        "app-server 请求超时（{}s）：{method}",
                        self.request_timeout.as_secs()
                    ),
                ))
            }
        }
    }

    // ---- 协议方法封装（params 形状按 0.166 schema + 实测信封） ----

    /// `thread/start`：新建会话。返回响应 result（含 `thread.id`）。
    pub async fn thread_start(
        &self,
        cwd: &Path,
        approval_policy: &str,
        model: Option<&str>,
        sandbox: &str,
    ) -> Result<Value> {
        let mut params = json!({
            "cwd": cwd.to_string_lossy(),
            "approvalPolicy": approval_policy,
            "sandbox": sandbox,
        });
        if let Some(m) = model {
            params["model"] = json!(m);
        }
        self.request("thread/start", params).await
    }

    /// `thread/resume`：续接既有 thread（id 与 exec 版 rollout 存储同源——
    /// 实测：app-server 会话同样落 `~/.codex/sessions/.../rollout-<thread_id>.jsonl`，
    /// 无需 id 映射）。
    pub async fn thread_resume(
        &self,
        thread_id: &str,
        cwd: &Path,
        approval_policy: &str,
        model: Option<&str>,
        sandbox: &str,
    ) -> Result<Value> {
        let mut params = json!({
            "threadId": thread_id,
            "cwd": cwd.to_string_lossy(),
            "approvalPolicy": approval_policy,
            "sandbox": sandbox,
        });
        if let Some(m) = model {
            params["model"] = json!(m);
        }
        self.request("thread/resume", params).await
    }

    /// `turn/start`：发起一轮（input 为 UserInput 数组，文本形
    /// `{"type":"text","text":..}`）。返回 result（含 `turn.id`）。
    pub async fn turn_start(&self, thread_id: &str, prompt: &str) -> Result<Value> {
        self.request(
            "turn/start",
            json!({"threadId": thread_id, "input": [{"type":"text","text":prompt}]}),
        )
        .await
    }

    /// `turn/steer`：运行中转向（expectedTurnId 为活跃 turn 前置条件）。
    pub async fn turn_steer(&self, thread_id: &str, turn_id: &str, message: &str) -> Result<Value> {
        self.request(
            "turn/steer",
            json!({
                "threadId": thread_id,
                "expectedTurnId": turn_id,
                "input": [{"type":"text","text":message}],
            }),
        )
        .await
    }

    /// `turn/interrupt`：中断活跃 turn。
    pub async fn turn_interrupt(&self, thread_id: &str, turn_id: &str) -> Result<Value> {
        self.request(
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
        )
        .await
    }

    /// `model/list` 单页（分页 cursor；batch 2 的 `list_models` 负责翻页取全）。
    pub async fn model_list_page(&self, cursor: Option<&str>) -> Result<Value> {
        let mut params = json!({});
        if let Some(c) = cursor {
            params["cursor"] = json!(c);
        }
        self.request("model/list", params).await
    }

    /// 取下一条服务端通知（`None` = 连接已断、通知通道关闭）。
    pub async fn next_notification(&self) -> Option<IncomingNotification> {
        self.notifications.lock().await.recv().await
    }

    /// 取下一个服务端请求（审批等；`None` = 连接已断）。
    pub async fn next_request(&self) -> Option<IncomingRequest> {
        self.requests.lock().await.recv().await
    }

    /// 连接是否已死（stdout EOF 后置位）。
    pub fn is_closed(&self) -> bool {
        *self.closed_rx.borrow()
    }

    /// 等待连接死亡信号（事件循环 select 分支用；已死则立即返回）。
    pub async fn closed_signal(&self) {
        let mut rx = self.closed_rx.clone();
        if *rx.borrow_and_update() {
            return;
        }
        let _ = rx.changed().await;
    }

    /// 显式终止子进程（Drop / kill_on_drop 兜底之外的确定路径）。
    pub async fn kill(&mut self) {
        let _ = self.child.kill().await;
    }
}

/// `pending` 共享句柄（reader task / `request()` 复用同一张在途表；
/// Arc 共享使 reader task 能拿到 clone）。
#[derive(Clone, Default)]
struct ArcPending(std::sync::Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>);

impl ArcPending {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<Result<Value>>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 连接死亡：冲掉全部在途请求（fail-open 上抛）。
fn fail_all_pending(pending: &ArcPending, msg: &str) {
    for (_, tx) in pending.lock().drain() {
        let _ = tx.send(Err(CoreError::Backend("codex-app", msg.to_string())));
    }
}

/// 单行帧分类分发（宽松信封：服务端帧不带 `jsonrpc` 字段，见模块注释）。
/// 有 `method` → 通知 / 服务端请求（后者带 id，配回执 oneshot）；否则有 `id`
/// → 客户端请求的响应（result / error 帧关联 pending）；两者皆非 → 噪声丢弃。
fn dispatch_frame(
    line: &str,
    pending: &ArcPending,
    notif_tx: &mpsc::UnboundedSender<IncomingNotification>,
    req_tx: &mpsc::UnboundedSender<IncomingRequest>,
    stdin_tx: &mpsc::UnboundedSender<String>,
) {
    let t = line.trim();
    if t.is_empty() {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(t) else {
        debug!(target: "imagent::codex-app", line = %t, "app-server 非 JSON 行，丢弃");
        return;
    };
    if let Some(method) = v.get("method").and_then(Value::as_str) {
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        match v.get("id") {
            // 服务端 → 客户端请求（审批等）：配回执 sender，回执帧由 mini task
            // 经 stdin 写回（与出站请求共用写通道，串行化）。
            Some(id) => {
                let (reply_tx, reply_rx) = oneshot::channel::<RpcReply>();
                let req = IncomingRequest {
                    id: id.clone(),
                    method: method.to_string(),
                    params,
                    reply: reply_tx,
                };
                if req_tx.send(req).is_err() {
                    return;
                }
                let stdin = stdin_tx.clone();
                let id = id.clone();
                tokio::spawn(async move {
                    if let Ok(reply) = reply_rx.await {
                        let frame = match reply {
                            RpcReply::Result(v) => json!({"jsonrpc":"2.0","id":id,"result":v}),
                            RpcReply::Error { code, message } => json!(
                                {"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}
                            ),
                        };
                        let _ = stdin.send(frame.to_string());
                    }
                });
            }
            // 通知。
            None => {
                let _ = notif_tx.send(IncomingNotification {
                    method: method.to_string(),
                    params,
                });
            }
        }
        return;
    }
    // 响应帧（`{"id":N,"result"| "error":{...}}`）。id 我们恒发 u64；服务端
    // 回显同型，宽容兼容字符串数字。
    let Some(id) = v
        .get("id")
        .and_then(|i| i.as_u64().or_else(|| i.as_str()?.parse().ok()))
    else {
        debug!(target: "imagent::codex-app", raw = %t, "无法分类的 app-server 帧，丢弃");
        return;
    };
    let sender = pending.lock().remove(&id);
    match sender {
        Some(tx) => {
            let res = if let Some(err) = v.get("error") {
                let msg = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("app-server 请求失败")
                    .to_string();
                let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
                Err(CoreError::Backend(
                    "codex-app",
                    format!("app-server 错误 {code}: {msg}"),
                ))
            } else {
                Ok(v.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = tx.send(res);
        }
        // 未知 id（超时已移除 / 服务端自发响应）——丢弃即可。
        None => debug!(id, "响应无匹配的在途请求，丢弃"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 构造 mock `codex app-server`：一个 sh 脚本按场景吐固定 JSONL，并把收到
    /// 的帧记录到日志文件（审批回执断言数据源）。返回 (脚本路径, 日志路径)。
    fn mock_script(dir: &Path, scenario: &str) -> (PathBuf, PathBuf) {
        let script = dir.join(format!("mock-{scenario}.sh"));
        let log = dir.join(format!("mock-{scenario}.log"));
        let body = match scenario {
            // 场景 A：请求-响应关联 + 通知分发 + 垃圾行容错。
            "echo" => r#"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$LOG"
  case "$line" in
    *'"method":"initialize"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '{"id":%s,"result":{"userAgent":"mock"}}\n' "$id" ;;
    *'"thread/start"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf 'not json noise\n'
      printf '{"method":"thread/started","params":{"thread":{"id":"th-1"}}}\n'
      printf '{"id":%s,"result":{"thread":{"id":"th-1"}}}\n' "$id" ;;
    *'"model/list"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '{"id":%s,"result":{"data":[{"id":"m-1"},{"id":"m-2"}],"nextCursor":null}}\n' "$id" ;;
  esac
done
"#
            .replace("$LOG", &log.to_string_lossy()),
            // 场景 B：审批往返——turn/start 后发 server-request，等客户端回执
            // （记录到日志）再继续吐完成事件。
            "approval" => r#"
phase=1
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$LOG"
  case "$line" in
    *'"method":"initialize"'*|*'"thread/start"'*|*'"turn/start"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      case "$line" in
        *'"method":"initialize"'*) printf '{"id":%s,"result":{}}\n' "$id" ;;
        *'"thread/start"'*)
          printf '{"id":%s,"result":{"thread":{"id":"th-1"}}}\n' "$id" ;;
        *'"turn/start"'*)
          printf '{"id":%s,"result":{"turn":{"id":"turn-1"}}}\n' "$id"
          printf '{"method":"item/commandExecution/requestApproval","id":0,"params":{"kind":"command","threadId":"th-1","turnId":"turn-1","itemId":"call-1","command":"rm -rf /"}}\n'
          phase=2 ;;
      esac
      ;;
    # 客户端审批回执（无 method，有 decision）→ 记录后继续事件流收尾。
    *'"decision"'*)
      printf '{"method":"item/completed","params":{"threadId":"th-1","turnId":"turn-1","item":{"type":"agentMessage","id":"m1","text":"done"}}}\n'
      printf '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"turn-1","status":"completed","items":[]}}}\n'
      phase=3 ;;
    *'"error"'*) : ;;
  esac
done
"#
            .replace("$LOG", &log.to_string_lossy()),
            // 场景 C：响应迟到（超时路径）。
            "slow" => r#"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      sleep 1
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '{"id":%s,"result":{}}\n' "$id" ;;
  esac
done
"#
            .to_string(),
            // 场景 D：立即退出（连接死亡路径）。
            "die" => r#"
exit 0
"#
            .to_string(),
            _ => String::new(),
        };
        std::fs::write(
            &script,
            format!("#!/bin/sh\nLOG='{}'\n{}", log.to_string_lossy(), body),
        )
        .expect("写 mock 脚本");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        (script, log)
    }

    async fn spawn_mock(scenario: &str, timeout: Duration) -> (AppServerProcess, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "imagent-codex-app-test-{}-{}",
            scenario,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let (script, log) = mock_script(&dir, scenario);
        let proc =
            AppServerProcess::spawn_configured(script.to_string_lossy().as_ref(), &dir, timeout)
                .await
                .expect("spawn mock");
        (proc, log)
    }

    #[tokio::test]
    async fn request_response_correlation_and_notifications() {
        let (mut proc, _log) = spawn_mock("echo", Duration::from_secs(5)).await;
        let res = proc
            .request(
                "initialize",
                json!({"clientInfo":{"name":"t","version":"0"}}),
            )
            .await
            .expect("initialize");
        assert_eq!(res["userAgent"], "mock");
        // 通知（thread/started）先于响应帧到达也应被正确分发（乱序容错）。
        let res = proc
            .thread_start(Path::new("/tmp"), "on-request", None, "read-only")
            .await
            .expect("thread_start");
        assert_eq!(res["thread"]["id"], "th-1");
        let notif = proc.next_notification().await.expect("通知");
        assert_eq!(notif.method, "thread/started");
        assert_eq!(notif.params["thread"]["id"], "th-1");
        // 垃圾行不应污染后续关联。
        let res = proc.model_list_page(None).await.expect("model/list");
        assert_eq!(res["data"].as_array().expect("data").len(), 2);
        proc.kill().await;
    }

    #[tokio::test]
    async fn request_timeout_errors() {
        let (mut proc, _log) = spawn_mock("slow", Duration::from_millis(150)).await;
        let err = proc
            .request(
                "initialize",
                json!({"clientInfo":{"name":"t","version":"0"}}),
            )
            .await
            .expect_err("应超时");
        assert!(err.to_string().contains("超时"), "{err}");
        proc.kill().await;
    }

    #[tokio::test]
    async fn approval_round_trip() {
        let (mut proc, log) = spawn_mock("approval", Duration::from_secs(10)).await;
        proc.request(
            "initialize",
            json!({"clientInfo":{"name":"t","version":"0"}}),
        )
        .await
        .expect("initialize");
        proc.thread_start(Path::new("/tmp"), "untrusted", None, "read-only")
            .await
            .expect("thread_start");
        proc.turn_start("th-1", "run command")
            .await
            .expect("turn_start");
        // 服务端审批请求到达 → allow 回执 → mock 记录后继续吐完成流。
        let req = proc.next_request().await.expect("server request");
        assert_eq!(req.method, "item/commandExecution/requestApproval");
        assert_eq!(req.params["command"], "rm -rf /");
        assert!(req.approval_respond("accept"), "回执送达");
        // 事件流收尾可达（未被悬挂的审批阻塞）。
        let notif = loop {
            let n = proc.next_notification().await.expect("通知流");
            if n.method == "turn/completed" {
                break n;
            }
        };
        assert_eq!(notif.params["turn"]["status"], "completed");
        // mock 日志应记录客户端回的审批帧（decision 值断言）。
        let logged = std::fs::read_to_string(&log).expect("读 mock 日志");
        assert!(
            logged.contains("\"decision\":\"accept\""),
            "回执帧未到达 mock：{logged}"
        );
        proc.kill().await;
    }

    #[tokio::test]
    async fn connection_death_fails_pending_requests() {
        let dir =
            std::env::temp_dir().join(format!("imagent-codex-app-die-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let (script, _log) = mock_script(&dir, "die");
        let mut proc = AppServerProcess::spawn_configured(
            script.to_string_lossy().as_ref(),
            &dir,
            Duration::from_secs(5),
        )
        .await
        .expect("spawn mock");
        // 等 reader 收尾（EOF 检出）。
        tokio::time::sleep(Duration::from_millis(300)).await;
        let err = proc
            .request(
                "initialize",
                json!({"clientInfo":{"name":"t","version":"0"}}),
            )
            .await
            .expect_err("连接已死应报错");
        assert!(
            err.to_string().contains("连接已断") || err.to_string().contains("关闭"),
            "{err}"
        );
        assert!(proc.is_closed());
        let _ = proc.next_notification().await;
        proc.kill().await;
    }

    /// 帧分类纯逻辑：错误帧 → 关联请求以 Err 解析；未知 id 丢弃不 panic。
    #[tokio::test]
    async fn dispatch_error_frame_resolves_pending_with_err() {
        let shared = ArcPending::default();
        let (rx_ready, rx) = oneshot::channel::<Result<Value>>();
        shared.lock().insert(7, rx_ready);
        let (notif_tx, mut notif_rx) = mpsc::unbounded_channel();
        let (req_tx, _req_rx) = mpsc::unbounded_channel();
        let (stdin_tx, _stdin_rx) = mpsc::unbounded_channel();
        dispatch_frame(
            r#"{"error":{"code":-32600,"message":"invalid thread id"},"id":7}"#,
            &shared,
            &notif_tx,
            &req_tx,
            &stdin_tx,
        );
        let res = rx.await.expect("回执").expect_err("错误帧应 Err");
        assert!(res.to_string().contains("invalid thread id"), "{res}");
        assert!(notif_rx.try_recv().is_err(), "错误帧不应进通知通道");
        // 通知帧走通知通道；噪声行静默丢弃。
        dispatch_frame(
            r#"{"method":"turn/started","params":{}}"#,
            &shared,
            &notif_tx,
            &req_tx,
            &stdin_tx,
        );
        let n = notif_rx.try_recv().expect("通知");
        assert_eq!(n.method, "turn/started");
        dispatch_frame("garbage", &shared, &notif_tx, &req_tx, &stdin_tx);
        assert!(notif_rx.try_recv().is_err());
    }
}
