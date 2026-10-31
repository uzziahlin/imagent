//! [`CodexAppBackend`]：`codex app-server` 双工协议后端（Backend trait 全套）。
//!
//! 与 [`crate::backend::CodexBackend`]（exec 单发版）的关键差异：
//! - **审批闭环成立**（[`PermissionCapability::FullLoop`]）：app-server 把命令
//!   执行/文件修改/权限提升/工具输入四类审批作为 **server→client JSON-RPC 请求**
//!   推来（`item/commandExecution/requestApproval` 等，信封样本见
//!   [`crate::app_server`] 模块注释），本 backend 经注入的 [`ImPermissionHook`]
//!   走 IM 审批闭环（与 claude-acp 的 `session/request_permission` 同构），
//!   hook 返回 bool 后按 `decision: accept/decline` 回执；
//! - **steering 真实可用**：`_steer` receiver 接 `turn/steer`（exec 版的
//!   stdin 转写在 app-server 协议下无 stdin 可写，这里走协议方法）；
//! - **AgentMessageDelta 是真实增量**：v2 协议的 `item/agentMessage/delta`
//!   携带纯增量文本（实测 0.166 确认），映射为 Text 增量流式；exec 线的
//!   `item.updated` agent_message 是累计快照（防御性 Skip，见 stream.rs）——
//!   两条线语义相反，**不可复用 exec 的 Skip 策略**。completed 快照仍作权威
//!   兜底：按 itemId 记账已流出的增量，completed 只补差额（前缀吻合），
//!   不吻合宁缺勿重（协议异常告警）。
//!
//! ## 与 core 的 Ask 通道核实结论（任务前置核实项）
//!
//! core 的 `AgentChunk` **没有 Ask 变体**——「dispatcher Ask 通道」的实际形态
//! 是 [`ImPermissionHook`]（`set_im_permission_hook` 注入，[`ImPermissionAsk`]
//! 携带 `request_id` 路由键，返回 `bool` = allow/deny）。本实现按 ACP 先例走
//! hook 通道，不新增 chunk 形态。
//!
//! ## usage 与 v2 协议差异
//!
//! v2 的 `turn/completed` 不携带 usage（schema 的 `Turn` 无该字段）；token 用量
//! 走独立通知 `thread/tokenUsage/updated`（`last`/`total` + `modelContextWindow`）。
//! 本 backend 随到随记账，轮末并入 `RunOutcome.usage`（AgentChunk 无 Usage
//! 变体，与 exec 版经 spawn_cli_backend 的 usage_acc 汇总同构）。
//!
//! ## 进程生命周期
//!
//! 每轮 run spawn 一个 app-server 子进程（initialize → thread/start|resume →
//! turn/start → 事件流 → drop/kill_on_drop）。thread 历史经 rollout 持久化，
//! 跨轮续接用 `thread/resume`（id 与 exec 版 rollout 存储同源，实测确认：
//! app-server 会话同样落 `~/.codex/sessions/.../rollout-<thread_id>.jsonl`，
//! 无需映射）。长驻进程池化属批 2 接线范围。

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;
use imagent_core::{
    AgentChunk, Backend, CoreError, ImPermissionAsk, ImPermissionHook, LocalSession,
    PermissionCapability, Result, RunOutcome, SessionId, TodoItem, TodoStatus, UsageStats,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::app_server::{AppServerProcess, IncomingNotification, IncomingRequest, RpcReply};

/// codex `AskForApproval` 缺省档位（批 2 前置契约）：auto（自动档）折算为
/// `on-request`——分类器筛 + 危险操作进审批，与 Claude auto 语义同构；
/// `untrusted` 对应 ask 档。
pub(crate) const DEFAULT_APPROVAL_POLICY: &str = "on-request";

/// 审批摘要截断（与 claude-acp / socket 闭环同口径：2000 字符）。
const SUMMARY_MAX_CHARS: usize = 2000;

/// OpenAI Codex app-server 双工后端。
pub struct CodexAppBackend {
    /// 审批档位（untrusted/on-request/never；构造缺省 `on-request`）。
    approval_policy: String,
    /// v1.21 /model：运行时模型覆盖（thread/start / thread/resume 的 `model`
    /// 参数；None = CLI 默认）。std RwLock——与 exec 版同款（临界区纯读写）。
    model: RwLock<Option<String>>,
    /// B3：IM 审批闭环回调（dispatcher `run()` 启动时注入；None = 独立使用，
    /// 审批请求 fail-closed 拒绝）。
    hook: RwLock<Option<ImPermissionHook>>,
}

impl CodexAppBackend {
    /// 构造。`approval_policy`：`None` / 空 → [`DEFAULT_APPROVAL_POLICY`]
    /// （on-request，auto 语义）；`"untrusted"` 对应 ask 档；`"never"` 全放行。
    /// 未知值原样透传（codex 在 thread/start 处显式报错，fail-open 上抛可见）。
    pub fn new(approval_policy: Option<&str>) -> Self {
        let policy = approval_policy
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| DEFAULT_APPROVAL_POLICY.to_string());
        if !matches!(policy.as_str(), "untrusted" | "on-request" | "never") {
            warn!(
                target: "imagent::codex-app",
                policy = %policy,
                "非标准审批档位（协议只认 untrusted/on-request/never），原样透传交由 codex 校验"
            );
        }
        Self {
            approval_policy: policy,
            model: RwLock::new(None),
            hook: RwLock::new(None),
        }
    }

    /// 模型目录（批 2 三层回退链的实时层）：spawn 临时 app-server 调
    /// `model/list` 分页取全，返回模型 id 列表。**任何失败回空**（批 2 回退
    /// 到 config.toml → 网关官方列表）。
    pub async fn list_models(&self) -> Vec<String> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let proc = match AppServerProcess::spawn(&cwd).await {
            Ok(p) => p,
            Err(e) => {
                warn!(target: "imagent::codex-app", error = %e, "model/list：app-server spawn 失败，回退链接管");
                return Vec::new();
            }
        };
        let init = proc
            .request(
                "initialize",
                json!({"clientInfo": {"name": "imagent", "version": env!("CARGO_PKG_VERSION")}}),
            )
            .await;
        if init.is_err() {
            warn!(target: "imagent::codex-app", "model/list：initialize 失败，回退链接管");
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        // 分页上限防失控（官方分页 nextCursor 正常几页收敛；20 页远超已知目录规模）。
        for _ in 0..20 {
            let page = match proc.model_list_page(cursor.as_deref()).await {
                Ok(v) => v,
                Err(e) => {
                    warn!(target: "imagent::codex-app", error = %e, "model/list 失败，回退链接管");
                    return Vec::new();
                }
            };
            if let Some(data) = page.get("data").and_then(Value::as_array) {
                for m in data {
                    let id = m
                        .get("id")
                        .and_then(Value::as_str)
                        .or_else(|| m.get("model").and_then(Value::as_str));
                    if let Some(id) = id.filter(|s| !s.is_empty()) {
                        let id = id.to_string();
                        if !out.contains(&id) {
                            out.push(id);
                        }
                    }
                }
            }
            match page
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
            {
                Some(c) => cursor = Some(c.to_string()),
                None => return out,
            }
        }
        warn!(target: "imagent::codex-app", "model/list 分页未收敛（20 页上限），截断返回");
        out
    }

    /// 服务端请求（审批四类）处理：hook Ask → 等用户表态 → 按结果回执。
    async fn handle_server_request(&self, conv_id: &str, req: IncomingRequest) {
        if !is_approval_method(&req.method) {
            // item/tool/call（DynamicToolCall）等未接请求：显式 -32601 失败，
            // 不悬挂服务端等待。
            warn!(target: "imagent::codex-app", method = %req.method, "未支持的服务端请求，回 error 帧");
            let reply = RpcReply::Error {
                code: -32601,
                message: format!("imagent codex-app: {} not implemented", req.method),
            };
            let _ = req.respond(reply);
            return;
        }
        let hook = self.hook.read().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(hook) = hook else {
            // B3 fail-closed（对齐 ACP）：hook 未注入（backend 未挂到
            // Dispatcher）时无 IM 闭环可用，一律拒绝，绝不静默放行。
            warn!(
                target: "imagent::codex-app",
                method = %req.method,
                "IM 审批回调未注入（backend 未挂到 Dispatcher？），fail-closed 拒绝该次审批"
            );
            let reply = approval_reply(&req.method, &req.params, false);
            let _ = req.respond(reply);
            return;
        };
        let (tool_name, input_summary) = describe_approval(&req.method, &req.params);
        let request_id = format!("codex-app-{}", compact_id(&req.id));
        let allow = hook(ImPermissionAsk {
            conv_id: conv_id.to_string(),
            request_id,
            tool_name,
            input_summary,
        })
        .await;
        debug!(target: "imagent::codex-app", method = %req.method, allow, "审批闭环回执");
        let reply = approval_reply(&req.method, &req.params, allow);
        let _ = req.respond(reply);
    }
}

/// 四类审批 server-request 方法名（ServerRequest schema oneOf 实证）。
fn is_approval_method(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "item/tool/requestUserInput"
    )
}

/// 审批回执构造（纯函数；回执字段名/取值按 0.166 schema + 真机往返实测）。
///
/// - commandExecution / fileChange → `{"decision": "accept"|"decline"}`
///   （不用 `acceptForSession`：会话级缓存绕过 IM 审批卡；不用 `cancel`：
///   decline 让 turn 继续，与 ACP 拒绝语义一致）；
/// - permissions → `{"permissions": <allow 时请求的 profile 原样回授 | deny 时
///   空 profile>, "scope":"turn"}`（空 profile = 不授任何附加权限，即 deny 的
///   表达形态——该请求的响应 schema 无 decline 值）；
/// - requestUserInput → `{"answers": {}}`（批 1 的 bool 通道表达不了选项级
///   回答，空 answers 让 codex 侧按未答处理；选项级交互属批 2 接线）。
fn approval_reply(method: &str, params: &Value, allow: bool) -> RpcReply {
    match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            let decision = if allow { "accept" } else { "decline" };
            RpcReply::Result(json!({ "decision": decision }))
        }
        "item/permissions/requestApproval" => {
            let profile = if allow {
                params
                    .get("permissions")
                    .cloned()
                    .unwrap_or_else(|| json!({}))
            } else {
                json!({})
            };
            RpcReply::Result(json!({ "permissions": profile, "scope": "turn" }))
        }
        "item/tool/requestUserInput" => RpcReply::Result(json!({ "answers": {} })),
        other => RpcReply::Error {
            code: -32601,
            message: format!("imagent codex-app: {other} not implemented"),
        },
    }
}

/// 审批卡文案（tool_name 进 core `needs_approval` 匹配 + 审批卡标题；
/// input_summary 是审批卡正文）。tool_name 沿用 exec 线工具名（shell），
/// fileChange 映射 Edit（claude 系审批清单惯例名）——`approval_tools` 配置
/// 需含对应名才命中审批集，别名归一策略由批 2 统一决定。
fn describe_approval(method: &str, params: &Value) -> (String, String) {
    let tool: &str;
    let summary: String;
    match method {
        "item/commandExecution/requestApproval" => {
            tool = "shell";
            summary = params
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("（命令详情缺失）")
                .to_string();
        }
        "item/fileChange/requestApproval" => {
            tool = "Edit";
            summary = params
                .get("reason")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("文件修改审批")
                .to_string();
        }
        "item/permissions/requestApproval" => {
            tool = "permissions";
            summary = params
                .get("reason")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("附加权限提升审批")
                .to_string();
        }
        _ => {
            tool = "input";
            let questions = params
                .get("questions")
                .map(|q| serde_json::to_string(q).unwrap_or_default())
                .filter(|s| !s.is_empty());
            summary = questions.unwrap_or_else(|| "工具输入确认".to_string());
        }
    }
    (
        tool.to_string(),
        truncate_chars(&summary, SUMMARY_MAX_CHARS),
    )
}

/// char 安全截断（中文审批摘要不能从 UTF-8 边界劈开）。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// JSON-RPC id → 紧凑字符串（审批 request_id 路由键用）。
fn compact_id(id: &Value) -> String {
    match id {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 一轮 turn 的事件流消费状态。
#[derive(Default)]
struct LoopState {
    /// 收到终态（completed/interrupted/failed）。
    done: bool,
    /// 正常完成（interrupted/failed 为 false——terminal 语义对齐 exec 版
    /// `reached_terminal`）。
    completed: bool,
    /// failed 终态的错误消息（run 收尾转 Err）。
    error: Option<String>,
    /// final_text 累积（B9：完整消息段间 "\n\n"；delta 直接续接）。
    text_acc: String,
    /// itemId → 已流式发出的增量文本（completed 快照只补差额，防重复）。
    msg_deltas: HashMap<String, String>,
    /// `thread/tokenUsage/updated` 的最新快照（turn/completed 无 usage；轮末
    /// 一次性并入 RunOutcome.usage）。
    usage: Option<UsageStats>,
}

impl LoopState {
    /// 累积正文并产出 Text chunk。`new_segment`：完整消息段（段间 "\n\n"，
    /// B9）；false = 同条消息增量续接（不插分隔，对齐 CliEvent::TextDelta 语义）。
    fn push_text(&mut self, piece: String, new_segment: bool) -> AgentChunk {
        if new_segment && !self.text_acc.is_empty() {
            self.text_acc.push_str("\n\n");
        }
        self.text_acc.push_str(&piece);
        AgentChunk::Text(piece)
    }
}

/// app-server 通知 → AgentChunk 序列（纯函数，真机信封 fixture 单测覆盖）。
/// v2 有而本批不消费的：`item/reasoning/*Delta`（💭 按完成快照整段上卡，
/// 与 exec 线同语义）、`item/plan/delta`（完成快照为准）、`userMessage`
/// item（自家 prompt 回显）、`thread/started`（thread id 取 thread/start
/// 响应，request 失败即整体失败）、`warning`/`configWarning`（日志可见）。
fn map_notification(method: &str, params: &Value, st: &mut LoopState) -> Vec<AgentChunk> {
    match method {
        "item/started" => map_item_started(params),
        "item/completed" => map_item_completed(params, st),
        // v2 真实增量（exec 线为累计快照语义，两线不可互用映射，见模块注释）。
        "item/agentMessage/delta" => {
            let delta = params.get("delta").and_then(Value::as_str).unwrap_or("");
            let item_id = params.get("itemId").and_then(Value::as_str).unwrap_or("");
            if delta.is_empty() {
                Vec::new()
            } else {
                st.msg_deltas
                    .entry(item_id.to_string())
                    .or_default()
                    .push_str(delta);
                vec![st.push_text(delta.to_string(), false)]
            }
        }
        // v2 的结构化待办（step + pending/inProgress/completed → TodoList 全量
        // 快照，与 exec 线 todo_list item 同面板）。
        "turn/plan/updated" => {
            let items: Vec<TodoItem> = params
                .get("plan")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|s| TodoItem {
                            id: None,
                            text: s
                                .get("step")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            status: match s.get("status").and_then(Value::as_str) {
                                Some("inProgress") => TodoStatus::InProgress,
                                Some("completed") => TodoStatus::Completed,
                                _ => TodoStatus::Pending,
                            },
                        })
                        .collect()
                })
                .unwrap_or_default();
            if items.is_empty() {
                Vec::new()
            } else {
                vec![AgentChunk::TodoList { items }]
            }
        }
        "thread/tokenUsage/updated" => {
            stash_usage(params, st);
            Vec::new()
        }
        // v2 无 turn/failed 通知：失败以 turn/completed 的 status=failed 送达。
        "turn/completed" => {
            let status = params
                .pointer("/turn/status")
                .and_then(Value::as_str)
                .unwrap_or("completed");
            match status {
                "completed" => {
                    st.completed = true;
                    st.done = true;
                    finish_chunks(st)
                }
                "interrupted" => {
                    st.done = true;
                    warn!(target: "imagent::codex-app", "turn 被中断，返回已收到的部分文本");
                    finish_chunks(st)
                }
                "failed" => {
                    st.done = true;
                    st.error = Some(
                        params
                            .pointer("/turn/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("turn failed")
                            .to_string(),
                    );
                    Vec::new()
                }
                other => {
                    debug!(target: "imagent::codex-app", status = other, "非终态 turn/completed，忽略");
                    Vec::new()
                }
            }
        }
        _ => Vec::new(),
    }
}

/// item/started → 工具调用开始（commandExecution/mcpToolCall；userMessage
/// 是自家 prompt 回显，reasoning started 无文本，均跳过）。
fn map_item_started(params: &Value) -> Vec<AgentChunk> {
    let item = match params.get("item") {
        Some(i) => i,
        None => return Vec::new(),
    };
    let id = item.get("id").and_then(Value::as_str).map(str::to_string);
    match item.get("type").and_then(Value::as_str) {
        Some("commandExecution") => vec![AgentChunk::ToolUse {
            tool: "shell".into(),
            input: item
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            id,
        }],
        Some("mcpToolCall") => vec![AgentChunk::ToolUse {
            tool: item
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("mcp")
                .to_string(),
            input: item
                .get("arguments")
                .map(|v| v.to_string())
                .unwrap_or_default(),
            id,
        }],
        _ => Vec::new(),
    }
}

/// item/completed → 正文/思考/工具结果（权威快照）。
fn map_item_completed(params: &Value, st: &mut LoopState) -> Vec<AgentChunk> {
    let item = match params.get("item") {
        Some(i) => i,
        None => return Vec::new(),
    };
    let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
    match item.get("type").and_then(Value::as_str) {
        // 正文完成：增量已流出的只补差额（前缀吻合）；无增量则整段新起；
        // 快照与增量拼不拢 = 协议异常，宁缺勿重（防正文重复污染，权威性由
        // final_text 累积语义兜底）。
        Some("agentMessage") => {
            let full = item.get("text").and_then(Value::as_str).unwrap_or("");
            let mut chunks = Vec::new();
            if !full.is_empty() {
                let prev = st.msg_deltas.get(item_id).map(String::as_str).unwrap_or("");
                if prev.is_empty() {
                    let piece = full.to_string();
                    chunks.push(st.push_text(piece, true));
                } else if let Some(rest) = full.strip_prefix(prev) {
                    let rest = rest.to_string();
                    if !rest.is_empty() {
                        chunks.push(st.push_text(rest, false));
                    }
                } else {
                    warn!(
                        target: "imagent::codex-app",
                        item_id,
                        "agentMessage 完成快照与已流出增量不一致，丢弃快照尾段（宁缺勿重）"
                    );
                }
            }
            st.msg_deltas.remove(item_id);
            chunks
        }
        // 思考完成：v2 文本在 summary 数组（exec 线在 item.text），两形态都收
        // （宽容形状回退）；空 summary 跳过。💭 按完成快照整段上卡（与 exec
        // 线一致），summaryTextDelta 增量不消费（避免重复）。
        Some("reasoning") => {
            let mut text = item
                .get("summary")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            if text.trim().is_empty() {
                text = item
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
            }
            if text.trim().is_empty() {
                Vec::new()
            } else {
                vec![AgentChunk::Thought(text)]
            }
        }
        Some("commandExecution") => vec![AgentChunk::ToolResult {
            tool: "shell".into(),
            output: item
                .get("aggregatedOutput")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            id: item.get("id").and_then(Value::as_str).map(str::to_string),
        }],
        Some("mcpToolCall") => {
            let tool = item
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("mcp")
                .to_string();
            // 优先 result（序列化为 JSON 字符串），无则 error.message（对齐 exec 线）。
            let output = match item.get("result") {
                Some(v) if !v.is_null() => v.to_string(),
                _ => item
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            };
            vec![AgentChunk::ToolResult {
                tool,
                output,
                id: item.get("id").and_then(Value::as_str).map(str::to_string),
            }]
        }
        // plan item 是实验性文本快照（结构化待办走 turn/plan/updated）。
        _ => Vec::new(),
    }
}

/// tokenUsage 快照入账（last = 最近一轮，total 兜底；字段 camelCase，见模块注释）。
fn stash_usage(params: &Value, st: &mut LoopState) {
    let u = match params.get("tokenUsage") {
        Some(u) => u,
        None => return,
    };
    let Some(src) = u
        .get("last")
        .filter(|v| !v.is_null())
        .or_else(|| u.get("total"))
    else {
        return;
    };
    let input = src.get("inputTokens").and_then(Value::as_u64).unwrap_or(0);
    let output = src.get("outputTokens").and_then(Value::as_u64).unwrap_or(0);
    if input == 0 && output == 0 {
        return;
    }
    st.usage = Some(UsageStats {
        input_tokens: input,
        output_tokens: output,
        cached_tokens: src.get("cachedInputTokens").and_then(Value::as_u64),
        total_cost_usd: None,
        context_window: u.get("modelContextWindow").and_then(Value::as_u64),
    });
}

/// 轮末收尾 chunk（usage 在 RunOutcome，正文 Final 权威终止）。
fn finish_chunks(st: &LoopState) -> Vec<AgentChunk> {
    vec![AgentChunk::Final(st.text_acc.clone())]
}

/// 从 thread/start / thread/resume 响应提取 thread.id。
fn thread_id_of(res: &Value) -> Result<String> {
    res.pointer("/thread/id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| CoreError::Backend("codex-app", format!("thread 响应缺 thread.id：{res}")))
}

const NAME: &str = "codex-app";

#[async_trait]
impl Backend for CodexAppBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    /// B3：审批闭环成立——app-server 的 server-request 审批经 ImPermissionHook
    /// 进 IM（信封与往返实测见 app_server 模块注释）。
    fn permission_capability(&self) -> PermissionCapability {
        PermissionCapability::FullLoop
    }

    /// v1.17：steering 走 `turn/steer`（协议方法，不再忽略）。
    fn supports_steering(&self) -> bool {
        true
    }

    /// v1.21 /model：model 经 thread/start / thread/resume 参数透传。
    fn supports_model_selection(&self) -> bool {
        true
    }

    fn set_model(&self, model: Option<String>) {
        *self.model.write().unwrap_or_else(|e| e.into_inner()) = model;
    }

    fn model(&self) -> Option<String> {
        self.model.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// B3：IM 审批闭环回调注入（dispatcher run() 启动时调用一次）。
    fn set_im_permission_hook(&self, hook: Option<ImPermissionHook>) {
        *self.hook.write().unwrap_or_else(|e| e.into_inner()) = hook;
    }

    /// P5：扫 `~/.codex/sessions/.../rollout-*.jsonl`（与 exec 版同一存储，
    /// thread/resume 直接用 rollout thread id——同源性实测确认）。
    async fn list_local_sessions(&self, workdir: &std::path::Path) -> Vec<LocalSession> {
        let wd = workdir.to_path_buf();
        tokio::task::spawn_blocking(move || crate::sessions::scan_for_backend(&wd))
            .await
            .unwrap_or_default()
    }

    async fn run(
        &self,
        conv_id: &str,
        prompt: &str,
        session: Option<&SessionId>,
        workdir: &std::path::Path,
        allowed_tools: &[String],
        chunks: mpsc::Sender<AgentChunk>,
        _initial_todos: &[TodoItem],
        mut steer: mpsc::Receiver<String>,
    ) -> Result<RunOutcome> {
        debug!(
            target: "imagent::codex-app",
            conv_id, policy = %self.approval_policy, "codex-app run start"
        );
        // B11 幽灵会话预检（与 exec 版同语义同存储）：thread id 不在本机
        // rollout 存储则弃用续接、按新会话处理（fail-open 放行无法判定时）。
        let session = match session {
            Some(s) => {
                let tid = s.0.clone();
                let exists = match crate::sessions::default_codex_dir() {
                    Some(dir) => tokio::task::spawn_blocking(move || {
                        crate::sessions::session_exists(&dir, &tid)
                    })
                    .await
                    .unwrap_or(true),
                    None => true,
                };
                if exists {
                    Some(s)
                } else {
                    warn!(target: "imagent::codex-app", conv_id, thread_id = %s.0, "幽灵会话预检：thread id 不在本机存储，弃用续接开新会话");
                    None
                }
            }
            None => None,
        };
        let proc = AppServerProcess::spawn(workdir).await?;
        proc.request(
            "initialize",
            json!({"clientInfo": {"name": "imagent", "version": env!("CARGO_PKG_VERSION")}}),
        )
        .await?;
        let sandbox = crate::backend::pick_sandbox(allowed_tools);
        let model = self.model.read().unwrap_or_else(|e| e.into_inner()).clone();
        let thread_id = match session {
            Some(s) => {
                let res = proc
                    .thread_resume(
                        &s.0,
                        workdir,
                        &self.approval_policy,
                        model.as_deref(),
                        sandbox,
                    )
                    .await?;
                thread_id_of(&res)?
            }
            None => {
                let res = proc
                    .thread_start(workdir, &self.approval_policy, model.as_deref(), sandbox)
                    .await?;
                thread_id_of(&res)?
            }
        };
        let _ = chunks
            .send(AgentChunk::SessionStarted(thread_id.clone()))
            .await;
        let turn_res = proc.turn_start(&thread_id, prompt).await?;
        let turn_id = turn_res
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .map(str::to_string);
        if turn_id.is_none() {
            warn!(target: "imagent::codex-app", thread_id = %thread_id, "turn/start 响应缺 turn.id，steering 退化不可用");
        }

        // 事件循环：通知映射 / 审批闭环 / steering / 连接死亡四源 select。
        // 审批等待（hook → IM 用户表态）阻塞循环属设计内——codex 侧同样暂停
        // 等回执，事件在无界通道排队不丢（ACP 同构）。
        let mut st = LoopState::default();
        let mut notif_open = true;
        let mut req_open = true;
        let mut steer_open = true;
        while !st.done {
            tokio::select! {
                notif = proc.next_notification(), if notif_open => match notif {
                    Some(IncomingNotification { method, params }) => {
                        for c in map_notification(&method, &params, &mut st) {
                            let _ = chunks.send(c).await;
                        }
                    }
                    None => notif_open = false,
                },
                req = proc.next_request(), if req_open => match req {
                    Some(r) => self.handle_server_request(conv_id, r).await,
                    None => req_open = false,
                },
                msg = steer.recv(), if steer_open => match msg {
                    Some(m) => match turn_id.as_deref() {
                        Some(tid) => {
                            if let Err(e) = proc.turn_steer(&thread_id, tid, &m).await {
                                warn!(target: "imagent::codex-app", error = %e, "turn/steer 失败，本轮转向消息丢弃");
                            }
                        }
                        None => {
                            warn!(target: "imagent::codex-app", "无活跃 turn id，steering 消息丢弃");
                        }
                    },
                    None => steer_open = false,
                },
                _ = proc.closed_signal() => {
                    // 终态与连接死亡竞态：终态优先（已 done 则正常收尾）。
                    if st.done {
                        break;
                    }
                    return Err(CoreError::Backend(
                        NAME,
                        "app-server 连接中断，本轮终止".into(),
                    ));
                },
            }
        }
        // run 结束 proc drop → kill_on_drop 杀 app-server（批 2 如做进程池化在此改）。

        // 失败终态：Error chunk + Err（对齐 exec 版 turn.failed 路径）。
        // 正常终态的 Final chunk 已在 map_notification 的 turn/completed 分支
        // 随事件流发出（finish_chunks）——此处不得重复发送。
        if let Some(err) = st.error.take() {
            let _ = chunks.send(AgentChunk::Error(err.clone())).await;
            return Err(CoreError::Backend(NAME, err));
        }
        Ok(RunOutcome {
            session_id: SessionId(thread_id),
            final_text: st.text_acc,
            terminal: st.completed,
            usage: st.usage,
            stop_reason: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 纯解析一块通知序列 → chunks（不含终态收尾，终态单测单独覆盖）。
    fn feed(lines: &[(&str, Value)], st: &mut LoopState) -> Vec<AgentChunk> {
        let mut out = Vec::new();
        for (m, p) in lines {
            out.extend(map_notification(m, p, st));
        }
        out
    }

    /// 真机信封 fixture（2026-10-31 codex-cli 0.166.0 实测样本脱敏，命令输出
    /// 内容取自实测）。
    #[test]
    fn live_probe_sequence_maps_to_chunks() {
        let mut st = LoopState::default();
        let chunks = feed(
            &[
                (
                    "item/started",
                    json!({"item":{"type":"reasoning","id":"rs_1","summary":[],"content":[]},
                           "threadId":"th","turnId":"t1","startedAtMs":1}),
                ),
                (
                    "item/reasoning/summaryTextDelta",
                    json!({"threadId":"th","turnId":"t1","itemId":"rs_1","delta":"User wants","summaryIndex":0}),
                ),
                (
                    "item/completed",
                    json!({"item":{"type":"reasoning","id":"rs_1",
                           "summary":["User wants exact OK. Must final exactly."],"content":[],
                           "startedAtMs":1,"completedAtMs":2},
                           "threadId":"th","turnId":"t1","completedAtMs":2}),
                ),
                (
                    "item/started",
                    json!({"item":{"type":"agentMessage","id":"msg_1","text":"","phase":null,
                           "startedAtMs":2,"completedAtMs":null},
                           "threadId":"th","turnId":"t1","startedAtMs":2}),
                ),
                (
                    "item/agentMessage/delta",
                    json!({"threadId":"th","turnId":"t1","itemId":"msg_1","delta":"OK"}),
                ),
                (
                    "thread/tokenUsage/updated",
                    json!({"threadId":"th","turnId":"t1","tokenUsage":{
                        "total":{"totalTokens":100,"inputTokens":90,"cachedInputTokens":10,
                                 "outputTokens":10,"reasoningOutputTokens":4},
                        "last":{"totalTokens":100,"inputTokens":90,"cachedInputTokens":10,
                                "outputTokens":10},
                        "modelContextWindow":190000}}),
                ),
                (
                    "item/completed",
                    json!({"item":{"type":"agentMessage","id":"msg_1","text":"OK","phase":null,
                           "startedAtMs":2,"completedAtMs":3},
                           "threadId":"th","turnId":"t1","completedAtMs":3}),
                ),
                (
                    "turn/completed",
                    json!({"threadId":"th","turn":{"id":"t1","rootTurnId":"t1","items":[],
                           "status":"completed","error":null}}),
                ),
            ],
            &mut st,
        );
        // reasoning delta 不消费；completed summary 整段 → Thought。
        assert!(
            matches!(&chunks[0], AgentChunk::Thought(t) if t == "User wants exact OK. Must final exactly."),
            "{chunks:?}"
        );
        // delta → Text 增量；completed 快照只补差额（无新增）→ 不重复。
        assert!(
            matches!(&chunks[1], AgentChunk::Text(t) if t == "OK"),
            "{chunks:?}"
        );
        // turn/completed → Final（无重复正文）。
        assert_eq!(chunks.len(), 3, "{chunks:?}");
        assert!(
            matches!(&chunks[2], AgentChunk::Final(t) if t == "OK"),
            "{chunks:?}"
        );
        // usage 记账（thread/tokenUsage/updated，context_window 学习）。
        let usage = st.usage.expect("usage 入账");
        assert_eq!(usage.input_tokens, 90);
        assert_eq!(usage.output_tokens, 10);
        assert_eq!(usage.cached_tokens, Some(10));
        assert_eq!(usage.context_window, Some(190000));
        assert!(st.completed && st.done);
    }

    /// 工具事件：commandExecution started/completed → ToolUse/ToolResult
    /// （id 配对）；mcpToolCall 同理。
    #[test]
    fn command_execution_maps_to_tooluse_and_result() {
        let mut st = LoopState::default();
        let chunks = feed(
            &[
                (
                    "item/started",
                    json!({"item":{"type":"commandExecution","id":"call_1","command":"/bin/zsh -lc 'echo hi'",
                           "status":"inProgress","aggregatedOutput":null,"exitCode":null},
                           "threadId":"th","turnId":"t1","startedAtMs":1}),
                ),
                (
                    "item/completed",
                    json!({"item":{"type":"commandExecution","id":"call_1","command":"...",
                           "status":"completed","aggregatedOutput":"hi\n","exitCode":0},
                           "threadId":"th","turnId":"t1","completedAtMs":2}),
                ),
                (
                    "item/started",
                    json!({"item":{"type":"mcpToolCall","id":"t2","server":"fs","tool":"read_file",
                           "arguments":{"path":"/tmp/x"},"status":"inProgress"},
                           "threadId":"th","turnId":"t1","startedAtMs":2}),
                ),
                (
                    "item/completed",
                    json!({"item":{"type":"mcpToolCall","id":"t2","server":"fs","tool":"read_file",
                           "arguments":{},"status":"completed","result":{"lines":3},"error":null},
                           "threadId":"th","turnId":"t1","completedAtMs":3}),
                ),
            ],
            &mut st,
        );
        assert!(
            matches!(&chunks[0], AgentChunk::ToolUse { tool, input, id: Some(id) }
                if tool == "shell" && input.contains("echo hi") && id == "call_1"),
            "{chunks:?}"
        );
        assert!(
            matches!(&chunks[1], AgentChunk::ToolResult { tool, output, id: Some(id) }
                if tool == "shell" && output == "hi\n" && id == "call_1"),
            "{chunks:?}"
        );
        assert!(
            matches!(&chunks[2], AgentChunk::ToolUse { tool, .. } if tool == "read_file"),
            "{chunks:?}"
        );
        assert!(
            matches!(&chunks[3], AgentChunk::ToolResult { tool, output, .. }
                if tool == "read_file" && output == r#"{"lines":3}"#),
            "{chunks:?}"
        );
    }

    /// turn/plan/updated → TodoList（status 映射）。
    #[test]
    fn plan_update_maps_to_todo_list() {
        let mut st = LoopState::default();
        let chunks = feed(
            &[(
                "turn/plan/updated",
                json!({"threadId":"th","turnId":"t1","explanation":null,"plan":[
                    {"step":"扫描代码","status":"completed"},
                    {"step":"修复缺陷","status":"inProgress"},
                    {"step":"回归测试","status":"pending"}]}),
            )],
            &mut st,
        );
        match &chunks[0] {
            AgentChunk::TodoList { items } => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0].status, TodoStatus::Completed);
                assert_eq!(items[1].status, TodoStatus::InProgress);
                assert_eq!(items[2].status, TodoStatus::Pending);
            }
            other => panic!("期望 TodoList，得到 {other:?}"),
        }
    }

    /// 无增量的 completed 快照整段新起（段间 \n\n 累积）；多 agentMessage 分段。
    #[test]
    fn completed_without_deltas_starts_new_segment() {
        let mut st = LoopState::default();
        let chunks = feed(
            &[
                (
                    "item/completed",
                    json!({"item":{"type":"agentMessage","id":"m1","text":"第一段"},
                           "threadId":"th","turnId":"t1","completedAtMs":1}),
                ),
                (
                    "item/completed",
                    json!({"item":{"type":"agentMessage","id":"m2","text":"第二段"},
                           "threadId":"th","turnId":"t1","completedAtMs":2}),
                ),
            ],
            &mut st,
        );
        assert!(matches!(&chunks[0], AgentChunk::Text(t) if t == "第一段"));
        assert!(matches!(&chunks[1], AgentChunk::Text(t) if t == "第二段"));
        let fin = feed(
            &[(
                "turn/completed",
                json!({"threadId":"th","turn":{"id":"t1","status":"completed","items":[]}}),
            )],
            &mut st,
        );
        assert!(
            matches!(&fin[0], AgentChunk::Final(t) if t == "第一段\n\n第二段"),
            "B9 段间分隔：{:?}",
            fin[0]
        );
    }

    /// 快照与增量拼不拢（协议异常）→ 宁缺勿重，不产出重复正文。
    #[test]
    fn snapshot_delta_mismatch_drops_tail() {
        let mut st = LoopState::default();
        feed(
            &[(
                "item/agentMessage/delta",
                json!({"threadId":"th","turnId":"t1","itemId":"m1","delta":"HE"}),
            )],
            &mut st,
        );
        let chunks = feed(
            &[(
                "item/completed",
                json!({"item":{"type":"agentMessage","id":"m1","text":"完全不同"},
                       "threadId":"th","turnId":"t1","completedAtMs":1}),
            )],
            &mut st,
        );
        assert!(chunks.is_empty(), "异常快照不得产出 chunk：{chunks:?}");
        // 记账已清（item 结束）。
        assert!(!st.msg_deltas.contains_key("m1"));
    }

    /// 增量只发了前缀、completed 补差额（前缀吻合 → 只补尾部，不重复）。
    #[test]
    fn snapshot_prefix_match_emits_remainder_only() {
        let mut st = LoopState::default();
        feed(
            &[(
                "item/agentMessage/delta",
                json!({"threadId":"th","turnId":"t1","itemId":"m1","delta":"HE"}),
            )],
            &mut st,
        );
        let chunks = feed(
            &[(
                "item/completed",
                json!({"item":{"type":"agentMessage","id":"m1","text":"HELLO"},
                       "threadId":"th","turnId":"t1","completedAtMs":1}),
            )],
            &mut st,
        );
        assert_eq!(chunks.len(), 1);
        assert!(
            matches!(&chunks[0], AgentChunk::Text(t) if t == "LLO"),
            "{chunks:?}"
        );
        let fin = feed(
            &[(
                "turn/completed",
                json!({"threadId":"th","turn":{"id":"t1","status":"completed","items":[]}}),
            )],
            &mut st,
        );
        assert!(
            matches!(&fin[0], AgentChunk::Final(t) if t == "HELLO"),
            "{fin:?}"
        );
    }

    /// failed 终态 → error 入账（run 收尾转 Err）；interrupted → 部分文本收尾。
    #[test]
    fn failed_and_interrupted_turn_states() {
        let mut st = LoopState::default();
        feed(
            &[(
                "turn/completed",
                json!({"threadId":"th","turn":{"id":"t1","status":"failed",
                       "error":{"message":"rate limit exceeded"},"items":[]}}),
            )],
            &mut st,
        );
        assert_eq!(st.error.as_deref(), Some("rate limit exceeded"));
        assert!(st.done && !st.completed);

        let mut st2 = LoopState::default();
        st2.push_text("部分".into(), true);
        let chunks = feed(
            &[(
                "turn/completed",
                json!({"threadId":"th","turn":{"id":"t1","status":"interrupted","items":[]}}),
            )],
            &mut st2,
        );
        assert!(matches!(&chunks[0], AgentChunk::Final(t) if t == "部分"));
        assert!(st2.done && !st2.completed);
    }

    /// 审批回执构造：四类 × allow/deny（回执字段与取值按 schema + 实测）。
    #[test]
    fn approval_reply_matrix() {
        // commandExecution / fileChange：decision accept/decline。
        for m in [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
        ] {
            let RpcReply::Result(v) = approval_reply(m, &json!({}), true) else {
                panic!("应 Result")
            };
            assert_eq!(v["decision"], "accept", "{m}");
            let RpcReply::Result(v) = approval_reply(m, &json!({}), false) else {
                panic!("应 Result")
            };
            assert_eq!(v["decision"], "decline", "{m}");
        }
        // permissions：allow 回授请求 profile；deny 空 profile（= 不授附加权限）。
        let params = json!({"permissions": {"network": {"enabled": true}}});
        let RpcReply::Result(v) = approval_reply("item/permissions/requestApproval", &params, true)
        else {
            panic!("应 Result")
        };
        assert_eq!(v["permissions"]["network"]["enabled"], true);
        assert_eq!(v["scope"], "turn");
        let RpcReply::Result(v) =
            approval_reply("item/permissions/requestApproval", &params, false)
        else {
            panic!("应 Result")
        };
        assert_eq!(v["permissions"], json!({}));
        // requestUserInput：bool 通道限制，空 answers。
        let RpcReply::Result(v) = approval_reply(
            "item/tool/requestUserInput",
            &json!({"questions": []}),
            true,
        ) else {
            panic!("应 Result")
        };
        assert_eq!(v["answers"], json!({}));
        // 未知方法 → error 帧。
        assert!(matches!(
            approval_reply("item/tool/call", &json!({}), true),
            RpcReply::Error { .. }
        ));
    }

    /// 审批 Ask 往返：hook 收到 ImPermissionAsk（含 request_id 路由键与审批
    /// 卡文案）→ 返回 bool → 回执 decision 与之对应；未注入 hook → fail-closed。
    #[tokio::test]
    async fn approval_ask_round_trip_via_hook() {
        let backend = CodexAppBackend::new(Some("untrusted"));
        // allow 路径：hook 捕获 ask 字段并放行。
        let got = std::sync::Arc::new(std::sync::Mutex::new(None::<ImPermissionAsk>));
        let got_c = got.clone();
        let hook: ImPermissionHook = std::sync::Arc::new(move |ask: ImPermissionAsk| {
            *got_c.lock().unwrap() = Some(ask);
            Box::pin(async move { true })
        });
        backend.set_im_permission_hook(Some(hook));
        let (req, reply_rx) = IncomingRequest::new(
            json!(0),
            "item/commandExecution/requestApproval".into(),
            json!({"kind":"command","threadId":"th","turnId":"t1","itemId":"call_1",
                   "command":"rm -rf /","cwd":"/tmp"}),
        );
        // IncomingRequest::new 的回执通道由 handle_server_request 消费——直接断言回执。
        backend.handle_server_request("conv-1", req).await;
        let reply = reply_rx.await.expect("回执帧");
        let RpcReply::Result(v) = reply else {
            panic!("应 Result 回执")
        };
        assert_eq!(v["decision"], "accept");
        let ask = got.lock().unwrap().clone().expect("hook 应被调用");
        assert_eq!(ask.conv_id, "conv-1");
        assert_eq!(ask.request_id, "codex-app-0");
        assert_eq!(ask.tool_name, "shell");
        assert_eq!(ask.input_summary, "rm -rf /");

        // deny 路径。
        let deny_hook: ImPermissionHook =
            std::sync::Arc::new(|_ask| Box::pin(async move { false }));
        backend.set_im_permission_hook(Some(deny_hook));
        let (req, reply_rx) = IncomingRequest::new(
            json!(1),
            "item/commandExecution/requestApproval".into(),
            json!({"command":"curl evil"}),
        );
        backend.handle_server_request("conv-1", req).await;
        let RpcReply::Result(v) = reply_rx.await.expect("回执帧") else {
            panic!("应 Result 回执")
        };
        assert_eq!(v["decision"], "decline");
    }

    /// hook 未注入（独立使用）→ fail-closed decline，绝不静默放行。
    #[tokio::test]
    async fn approval_without_hook_fails_closed() {
        let backend = CodexAppBackend::new(None);
        let (req, reply_rx) = IncomingRequest::new(
            json!(2),
            "item/commandExecution/requestApproval".into(),
            json!({"command":"rm -rf /"}),
        );
        backend.handle_server_request("conv-1", req).await;
        let RpcReply::Result(v) = reply_rx.await.expect("回执帧") else {
            panic!("应 Result 回执")
        };
        assert_eq!(v["decision"], "decline");
    }

    /// 未支持的服务端请求（item/tool/call）→ -32601 error 帧，不悬挂服务端。
    #[tokio::test]
    async fn unsupported_server_request_gets_error_frame() {
        let backend = CodexAppBackend::new(None);
        let hook: ImPermissionHook = std::sync::Arc::new(|_ask| Box::pin(async move { true }));
        backend.set_im_permission_hook(Some(hook));
        let (req, reply_rx) =
            IncomingRequest::new(json!(3), "item/tool/call".into(), json!({"tool":"x"}));
        backend.handle_server_request("conv-1", req).await;
        let RpcReply::Error { code, .. } = reply_rx.await.expect("回执帧") else {
            panic!("应 Error 回执")
        };
        assert_eq!(code, -32601);
    }

    /// 审批卡文案与截断。
    #[test]
    fn describe_approval_truncates_summary() {
        let (tool, summary) = describe_approval(
            "item/commandExecution/requestApproval",
            &json!({"command": "x".repeat(3000)}),
        );
        assert_eq!(tool, "shell");
        assert_eq!(summary.chars().count(), SUMMARY_MAX_CHARS);
        let (tool, summary) = describe_approval(
            "item/fileChange/requestApproval",
            &json!({"reason": "需要写入配置"}),
        );
        assert_eq!(tool, "Edit");
        assert_eq!(summary, "需要写入配置");
        let (tool, _) = describe_approval(
            "item/tool/requestUserInput",
            &json!({"questions": [{"id":"q1","header":"H","question":"选哪个？"}]}),
        );
        assert_eq!(tool, "input");
    }

    /// 构造档位契约：None/空 → on-request（auto 缺省）；untrusted 透传。
    #[test]
    fn approval_policy_default_and_passthrough() {
        assert_eq!(CodexAppBackend::new(None).approval_policy, "on-request");
        assert_eq!(CodexAppBackend::new(Some("")).approval_policy, "on-request");
        assert_eq!(
            CodexAppBackend::new(Some("untrusted")).approval_policy,
            "untrusted"
        );
        assert_eq!(CodexAppBackend::new(Some("never")).approval_policy, "never");
    }

    /// B3 能力声明与命名。
    #[test]
    fn capability_matrix() {
        let b = CodexAppBackend::new(None);
        assert_eq!(b.name(), "codex-app");
        assert_eq!(b.permission_capability(), PermissionCapability::FullLoop);
        assert!(b.supports_steering());
        assert!(b.supports_model_selection());
        // v1.21 /model 语义：热设 + 回读。
        b.set_model(Some("gpt-5-codex".into()));
        assert_eq!(b.model(), Some("gpt-5-codex".into()));
        b.set_model(None);
        assert_eq!(b.model(), None);
    }
}
