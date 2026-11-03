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
use std::sync::{Mutex, RwLock};

use async_trait::async_trait;
use imagent_core::{
    AgentChunk, Backend, ConvId, CoreError, ImPermissionAsk, ImPermissionHook, LocalSession,
    PermissionCapability, PermissionMode, Result, RoundContext, RunOutcome, SessionId, TodoItem,
    TodoStatus, UsageStats,
};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

use crate::app_server::{
    AppServerProcess, IncomingNotification, IncomingRequest, RpcReply, REQUEST_TIMEOUT,
};

/// codex `AskForApproval` 缺省档位（批 2 前置契约）：auto（自动档）折算为
/// `on-request`——分类器筛 + 危险操作进审批，与 Claude auto 语义同构；
/// `untrusted` 对应 ask 档。
///
/// v1.38 P1-b：本常量与 [`CodexAppBackend::new`] 的 `approval_policy` 参数
/// 降级为**构造缺省基准**（直接调用方 / 独立测试用）；生产 run 内审批档以
/// ctx.permission_mode 映射（[`approval_policy_for`]）为权威。
pub(crate) const DEFAULT_APPROVAL_POLICY: &str = "on-request";

/// v1.38 P1-b：core 权限档位 → codex `approvalPolicy` 映射（纯函数）。
///
/// - `Ask` → `"untrusted"`：除信任白名单外全部进审批——最接近「每步审批」
///   闭环档（真机实测 untrusted 触发 `item/commandExecution/requestApproval`，
///   见 app_server 模块注释）；
/// - `Auto` → `"on-request"`：现行默认（构造缺省同值）——分类器筛 + 危险操作
///   进审批；`Auto` 对 codex-app 在 `PermissionMode::resolve` 保留（不退
///   Off），此映射即 auto 的完整语义；
/// - `Off` → `"never"`：全自动；
/// - `AutoClaude` / `Allow` / `Deny` → 回退 `"on-request"`：claude 专属档
///   （原生 `--permission-mode auto` 透传）与 MCP 定答档（allow/deny）不该
///   出现在 codex 会话（后者由 IM 审批闭环承担定答），防御性回退到三档中
///   最中性的 on-request。
///
/// `/perm` 全局热切生效链：`cmd_perm` 改 raw 档 → 下一条消息 run 入口现算
/// `ctx.permission_mode` → 本映射进 `thread/start|resume` 的 approvalPolicy
/// ——codex 会话免重启生效。
fn approval_policy_for(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Ask => "untrusted",
        PermissionMode::Auto => "on-request",
        PermissionMode::Off => "never",
        PermissionMode::AutoClaude | PermissionMode::Allow | PermissionMode::Deny => "on-request",
    }
}

/// 审批摘要截断（与 claude-acp / socket 闭环同口径：2000 字符）。
const SUMMARY_MAX_CHARS: usize = 2000;

/// v1.38 extras 补全：实时模型目录桥闭包类型（与 web 驾驶舱 ModelsContext 的
/// `ListModelsFn` 同构——bin 装配处直接绑定注入，default/extras 同源）。
pub type ListModelsBridge = dyn Fn() -> Vec<String> + Send + Sync;

/// OpenAI Codex app-server 双工后端。
pub struct CodexAppBackend {
    /// 审批档位（untrusted/on-request/never；构造缺省 `on-request`）。
    /// v1.38 P1-b：仅作构造缺省基准（直接调用方/测试）；生产 run 内以
    /// ctx.permission_mode 映射（[`approval_policy_for`]）为权威。
    approval_policy: String,
    /// v1.21 /model：运行时模型覆盖（thread/start / thread/resume 的 `model`
    /// 参数；None = CLI 默认）。std RwLock——与 exec 版同款（临界区纯读写）。
    /// v1.38：实例默认基准；会话级覆盖走 RoundContext.model 优先，本字段只
    /// 承担未覆盖会话的兜底。
    model: RwLock<Option<String>>,
    /// B3：IM 审批闭环回调（dispatcher `run()` 启动时注入；None = 独立使用，
    /// 审批请求 fail-closed 拒绝）。
    hook: RwLock<Option<ImPermissionHook>>,
    /// v1.37 优雅中断：conv_id → 在飞 run 的中断信号发送端。run 开始注册、
    /// 全路径收尾摘除（RAII 守卫）；per-conv 串行锁保证同 conv 单 run，map
    /// 天然无竞争。`interrupt_run` 命中即发信号（无在飞 run 时 no-op）。
    interrupts: Mutex<HashMap<String, watch::Sender<()>>>,
    /// app-server 可执行名（生产恒 "codex"；测试注入 mock 脚本路径——复用
    /// [`crate::app_server`] 既有 spawn_configured/mock_script 形态）。
    program: String,
    /// v1.38 extras 补全：实时模型目录桥（同步闭包，形态与 web 驾驶舱
    /// ModelsContext 的 ListModelsFn 同构——bin 侧直接绑定注入）。有桥 = 本
    /// 实例自含实时目录能力；None = 无桥（`list_models_live` 回空，语义同
    /// `list_models` 的 fail-soft）。default 与 extras 装配处同源注入，消除
    /// /backend 切换后的实例断链。
    list_models_bridge: RwLock<Option<std::sync::Arc<ListModelsBridge>>>,
}

impl CodexAppBackend {
    /// 构造。`approval_policy`：`None` / 空 → [`DEFAULT_APPROVAL_POLICY`]
    /// （on-request，auto 语义）；`"untrusted"` 对应 ask 档；`"never"` 全放行。
    /// 未知值原样透传（codex 在 thread/start 处显式报错，fail-open 上抛可见）。
    /// v1.38 P1-b：该参数为构造缺省基准——挂到 Dispatcher 的生产 run 内
    /// 审批档由 ctx.permission_mode 逐轮映射（[`Self::run`]），本字段不再
    /// 直接决定出站 approvalPolicy。
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
            interrupts: Mutex::new(HashMap::new()),
            program: "codex".to_string(),
            list_models_bridge: RwLock::new(None),
        }
    }

    /// v1.38 extras 补全：注入实时模型目录桥（同步入口，供 bin 装配 / 测试）。
    /// None 清除（回退链语义不变）。
    pub fn set_list_models_bridge(&self, f: Option<std::sync::Arc<ListModelsBridge>>) {
        *self
            .list_models_bridge
            .write()
            .unwrap_or_else(|e| e.into_inner()) = f;
    }

    /// v1.38 P1-b：构造缺省审批档位只读口（[`Self::new`] 的 `approval_policy`
    /// 参数落点）——直接调用方/测试的缺省基准。生产 run 内出站 approvalPolicy
    /// 以 ctx.permission_mode 映射（[`approval_policy_for`]）为权威，与本值无关。
    pub fn configured_approval_policy(&self) -> &str {
        &self.approval_policy
    }

    /// 装配便捷口：为本实例构造并注入「自桥」（线程 + 临时 current-thread
    /// runtime 桥接 [`Self::list_models`]——不依赖调用方 runtime 形态，任何
    /// 失败 fail-soft 回空；原 bin build_backend 的内联实现上移收口于此，
    /// default 与 extras 装配同源），返回桥句柄（default 路径转发给 web
    /// ModelsContext 注入；extras 实例注入后自含同款能力）。
    pub fn install_list_models_bridge(
        self: &std::sync::Arc<Self>,
    ) -> std::sync::Arc<ListModelsBridge> {
        let app = std::sync::Arc::clone(self);
        let bridge: std::sync::Arc<ListModelsBridge> = std::sync::Arc::new(move || {
            let app = std::sync::Arc::clone(&app);
            std::thread::scope(|s| {
                s.spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map(|rt| rt.block_on(app.list_models()))
                        .unwrap_or_default()
                })
                .join()
                .unwrap_or_default()
            })
        });
        self.set_list_models_bridge(Some(std::sync::Arc::clone(&bridge)));
        bridge
    }

    /// 同步取实时模型目录：有桥走桥，无桥回空（与 [`Self::list_models`] 的
    /// fail-soft 同语义）。web 驾驶舱 spawn_blocking 侧 / 实例自含能力共用。
    pub fn list_models_live(&self) -> Vec<String> {
        let f = self
            .list_models_bridge
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match f {
            Some(f) => f(),
            None => Vec::new(),
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

/// 在飞中断注册的 RAII 摘除守卫（v1.37）：run 全路径退出（spawn/initialize
/// 等早退 Err、事件循环连接死亡 return、正常收尾）时摘除本 conv 的中断
/// sender——表「无在飞即空」，陈旧句柄不滞留；下一 run 的 insert 覆盖同键
/// 只是防御路径。drop 顺序 = 注册的逆序，无跨字段借用问题。
struct RunInterruptGuard<'a> {
    interrupts: &'a Mutex<HashMap<String, watch::Sender<()>>>,
    conv_id: &'a str,
}

impl Drop for RunInterruptGuard<'_> {
    fn drop(&mut self) {
        self.interrupts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(self.conv_id);
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

    /// v1.37 优雅中断：命中在飞注册表即对当前 run 发信号（事件循环消费 →
    /// `turn/interrupt` → interrupted 终态自然收尾）。无在飞 run = no-op；
    /// 实际协议请求由 run 循环发出（本方法不持进程句柄，也不等待中断完成）。
    async fn interrupt_run(&self, conv: &ConvId) {
        let hit = {
            let map = self.interrupts.lock().unwrap_or_else(|e| e.into_inner());
            map.get(&conv.0).map(|tx| tx.send(())).is_some()
        };
        if hit {
            debug!(target: "imagent::codex-app", conv_id = %conv.0, "优雅中断信号已发（在飞 run 将发 turn/interrupt）");
        } else {
            debug!(target: "imagent::codex-app", conv_id = %conv.0, "无在飞 run，interrupt_run no-op");
        }
    }

    /// v1.37：协议中断有实效（事件循环消费信号发 turn/interrupt）——与
    /// interrupt_run 配对的能力位声明，/stop 据此走「先中断再有界等待」。
    fn supports_graceful_interrupt(&self) -> bool {
        true
    }

    /// v1.21 /model：model 经 thread/start / thread/resume 参数透传。
    fn supports_model_selection(&self) -> bool {
        true
    }

    /// v1.38 P1-b：思考深度调节——ctx.effort 经 `turn/start` 的 `effort`
    /// 参数透传（0.166 schema 实证，见 [`AppServerProcess::turn_start`]）。
    fn supports_effort(&self) -> bool {
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
        ctx: RoundContext<'_>,
        prompt: &str,
        session: Option<&SessionId>,
        chunks: mpsc::Sender<AgentChunk>,
    ) -> Result<RunOutcome> {
        // v1.38 P1-a：本轮执行环境自 RoundContext 解构（任务种子本后端不消费，
        // 保持原忽略语义）。v1.38 P1-b：effort → turn/start；permission_mode →
        // approvalPolicy 映射（ctx 权威，见 approval_policy_for）。
        let RoundContext {
            conv_id,
            workdir,
            allowed_tools,
            initial_todos: _,
            model: model_override,
            effort: effort_override,
            permission_mode,
            steer,
        } = ctx;
        let mut steer = steer;
        let approval_policy = approval_policy_for(permission_mode);
        debug!(
            target: "imagent::codex-app",
            conv_id, policy = approval_policy, effort = ?effort_override, "codex-app run start"
        );
        // v1.37 优雅中断：注册本 conv 的中断信号（`interrupt_run` 命中发信号；
        // watch 保留末值——信号早于事件循环启动也在首轮触发）。全路径收尾由
        // guard 摘除（spawn/initialize 早退 Err、循环连接死亡 return 均覆盖）。
        let (interrupt_tx, mut interrupt_rx) = watch::channel(());
        self.interrupts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(conv_id.to_string(), interrupt_tx);
        let _interrupt_guard = RunInterruptGuard {
            interrupts: &self.interrupts,
            conv_id,
        };
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
        let proc =
            AppServerProcess::spawn_configured(&self.program, workdir, REQUEST_TIMEOUT).await?;
        proc.request(
            "initialize",
            json!({"clientInfo": {"name": "imagent", "version": env!("CARGO_PKG_VERSION")}}),
        )
        .await?;
        let sandbox = crate::backend::pick_sandbox(allowed_tools);
        // v1.38 会话级模型：ctx.model（KV `model:<conv>` 命中）优先，None 回退
        // 实例默认（config 基准 / CLI 默认）。thread/start|resume 的 model 参数。
        let model = model_override
            .map(str::to_string)
            .or_else(|| self.model.read().unwrap_or_else(|e| e.into_inner()).clone());
        let thread_id = match session {
            Some(s) => {
                let res = proc
                    .thread_resume(&s.0, workdir, approval_policy, model.as_deref(), sandbox)
                    .await?;
                thread_id_of(&res)?
            }
            None => {
                let res = proc
                    .thread_start(workdir, approval_policy, model.as_deref(), sandbox)
                    .await?;
                thread_id_of(&res)?
            }
        };
        let _ = chunks
            .send(AgentChunk::SessionStarted(thread_id.clone()))
            .await;
        // v1.38 P1-b：effort 随 turn/start 透传（ctx.effort=None → 不带字段）。
        let turn_res = proc.turn_start(&thread_id, prompt, effort_override).await?;
        let turn_id = turn_res
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .map(str::to_string);
        if turn_id.is_none() {
            warn!(target: "imagent::codex-app", thread_id = %thread_id, "turn/start 响应缺 turn.id，steering 退化不可用");
        }

        // 事件循环：通知映射 / 审批闭环 / steering / 优雅中断 / 连接死亡五源
        // select。审批等待（hook → IM 用户表态）阻塞循环属设计内——codex 侧
        // 同样暂停等回执，事件在无界通道排队不丢（ACP 同构）；/stop 对审批
        // 等待期的中断经既有 cancel_all（deny 唤醒）→ 循环恢复 → 本分支信号
        // 消费，两路协作不打断彼此。
        let mut st = LoopState::default();
        let mut notif_open = true;
        let mut req_open = true;
        let mut steer_open = true;
        let mut interrupt_open = true;
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
                sig = interrupt_rx.changed(), if interrupt_open => match sig {
                    // 信号到达（send(()) 只置版本，changed 即信号——无需读值；
                    // watch 保留版本，信号早于循环启动也在本轮触发）。
                    Ok(()) => {
                        // /stop 优雅中断：对当前在飞 turn 发协议中断，然后
                        // 继续正常读流直到终态（interrupted turn 会回
                        // turn/completed(status=interrupted)）→ run 自然
                        // 返回 RunOutcome，已产出文本照常落库。信号一次性
                        // 消费（turn 已在协议层中断；重复 /stop 由调用方
                        // 有界等待超时后的 abort 兜底）。turn_interrupt 等
                        // 响应期间通知在无界通道排队，事件不丢。
                        interrupt_open = false;
                        match turn_id.as_deref() {
                            Some(tid) => match proc.turn_interrupt(&thread_id, tid).await {
                                Ok(_) => {
                                    debug!(target: "imagent::codex-app", conv_id, thread_id = %thread_id, turn_id = %tid, "优雅中断：turn/interrupt 已发，继续读流至终态");
                                }
                                Err(e) => {
                                    warn!(target: "imagent::codex-app", conv_id, error = %e, "turn/interrupt 失败，继续读流（/stop 超时 abort 兜底）");
                                }
                            },
                            None => {
                                warn!(target: "imagent::codex-app", conv_id, "无活跃 turn id，中断信号无法投递（/stop 超时 abort 兜底）");
                            }
                        }
                    }
                    // sender 被摘除（同 conv 下一 run 覆盖注册的竞态形态）：
                    // 本 run 已无中断通道，关闭分支，其余照常。
                    Err(_) => interrupt_open = false,
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
        assert_eq!(
            CodexAppBackend::new(None).configured_approval_policy(),
            "on-request"
        );
        assert_eq!(
            CodexAppBackend::new(Some("")).configured_approval_policy(),
            "on-request"
        );
        assert_eq!(
            CodexAppBackend::new(Some("untrusted")).configured_approval_policy(),
            "untrusted"
        );
        assert_eq!(
            CodexAppBackend::new(Some("never")).configured_approval_policy(),
            "never"
        );
    }

    /// v1.38 P1-b：permission_mode → approvalPolicy 四映射（Ask/Auto/Off/其他）。
    #[test]
    fn approval_policy_for_maps_permission_mode() {
        assert_eq!(approval_policy_for(PermissionMode::Ask), "untrusted");
        assert_eq!(approval_policy_for(PermissionMode::Auto), "on-request");
        assert_eq!(approval_policy_for(PermissionMode::Off), "never");
        // 其他变体（claude 专属 / MCP 定答档）防御性回退 on-request。
        assert_eq!(
            approval_policy_for(PermissionMode::AutoClaude),
            "on-request"
        );
        assert_eq!(approval_policy_for(PermissionMode::Allow), "on-request");
        assert_eq!(approval_policy_for(PermissionMode::Deny), "on-request");
    }

    /// v1.38 P1-b e2e（mock app-server 场景 "params"，带/不带两形态一条测试
    /// 顺序跑——减少并行 spawn 压力：mock 子进程 exec 偶发秒级延迟，两形态
    /// 分测会放大满载下的抖动命中面）：
    /// - ctx.effort=Some + permission_mode=Ask → thread/start 出站 params 带
    ///   `"approvalPolicy":"untrusted"`（映射消费），turn/start 出站 params 带
    ///   `"effort":"high"`（协议载体为 turn/start——0.166 schema 实证，
    ///   thread/start/resume 无 effort 字段）；
    /// - ctx.effort=None → turn/start 不带 effort 字段（保持现状字节）；
    ///   permission_mode=Off → thread/start 带 `"approvalPolicy":"never"`
    ///   （实例构造缺省被 ctx 权威覆盖）。
    ///
    /// 两轮共用同一 mock 场景脚本（幂等响应每轮 initialize/thread/start/
    /// turn/start），收帧日志顺序累积——第二轮在第一轮 await 完成后才起跑，
    /// 帧按轮序落同一日志，按下标断言两形态。
    #[tokio::test]
    async fn run_carries_and_omits_effort_and_maps_approval_policy() {
        let dir =
            std::env::temp_dir().join(format!("imagent-codex-app-params-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let (script, log) = crate::app_server::mock_support::mock_script(&dir, "params");
        let conv = "conv-1".to_string();
        let wd = dir.clone();

        // 形态一：effort=Some("high") × Ask。
        let mut backend = CodexAppBackend::new(Some("on-request"));
        backend.program = script.to_string_lossy().into_owned();
        let (tx, _rx) = mpsc::channel::<AgentChunk>(64);
        let (_steer_tx, steer_rx) = mpsc::channel::<String>(4);
        let outcome = backend
            .run(
                RoundContext {
                    conv_id: &conv,
                    workdir: &wd,
                    allowed_tools: &[],
                    initial_todos: &[],
                    model: None,
                    effort: Some("high"),
                    permission_mode: PermissionMode::Ask,
                    steer: steer_rx,
                },
                "带 effort 的一轮",
                None,
                tx,
            )
            .await
            .expect("run 应 Ok");
        assert!(outcome.terminal, "mock 立即 completed，应为正常终态");

        // 形态二（对照）：effort=None × Off，构造缺省 untrusted 亦被 ctx 覆盖。
        let mut backend2 = CodexAppBackend::new(Some("untrusted"));
        backend2.program = script.to_string_lossy().into_owned();
        let (tx2, _rx2) = mpsc::channel::<AgentChunk>(64);
        let (_steer_tx2, steer_rx2) = mpsc::channel::<String>(4);
        let conv2 = "conv-2".to_string();
        backend2
            .run(
                RoundContext {
                    conv_id: &conv2,
                    workdir: &wd,
                    allowed_tools: &[],
                    initial_todos: &[],
                    model: None,
                    effort: None,
                    permission_mode: PermissionMode::Off,
                    steer: steer_rx2,
                },
                "无 effort 的一轮",
                None,
                tx2,
            )
            .await
            .expect("run 应 Ok");

        // 断言按帧提取（不依赖 JSON 键序）：两轮各一组 thread/start /
        // turn/start，轮序 = 日志序。
        let logged = std::fs::read_to_string(&log).expect("读 mock 日志");
        let ts_lines: Vec<&str> = logged
            .lines()
            .filter(|l| l.contains("\"method\":\"thread/start\""))
            .collect();
        assert_eq!(ts_lines.len(), 2, "两轮各一条 thread/start: {logged}");
        assert!(
            ts_lines[0].contains("\"approvalPolicy\":\"untrusted\""),
            "Ask 应映射 untrusted: {}",
            ts_lines[0]
        );
        assert!(
            ts_lines[1].contains("\"approvalPolicy\":\"never\""),
            "Off 应映射 never（ctx 权威，非构造缺省 untrusted）: {}",
            ts_lines[1]
        );
        let tn_lines: Vec<&str> = logged
            .lines()
            .filter(|l| l.contains("\"method\":\"turn/start\""))
            .collect();
        assert_eq!(tn_lines.len(), 2, "两轮各一条 turn/start: {logged}");
        assert!(
            tn_lines[0].contains("\"effort\":\"high\""),
            "ctx.effort 应随 turn/start 透传: {}",
            tn_lines[0]
        );
        assert!(
            !tn_lines[1].contains("\"effort\""),
            "ctx.effort=None 不得带 effort 字段（保持现状字节）: {}",
            tn_lines[1]
        );
    }

    /// B3 能力声明与命名。
    #[test]
    fn capability_matrix() {
        let b = CodexAppBackend::new(None);
        assert_eq!(b.name(), "codex-app");
        assert_eq!(b.permission_capability(), PermissionCapability::FullLoop);
        assert!(b.supports_steering());
        assert!(b.supports_model_selection());
        // v1.37：graceful 中断能力位与 interrupt_run 配对声明。
        assert!(b.supports_graceful_interrupt());
        // v1.21 /model 语义：热设 + 回读。
        b.set_model(Some("gpt-5-codex".into()));
        assert_eq!(b.model(), Some("gpt-5-codex".into()));
        b.set_model(None);
        assert_eq!(b.model(), None);
    }

    /// v1.38 extras 补全：实时模型目录桥（同步入口）注入/清除/消费——extras
    /// 实例与 default 装配同源自含桥（消除 /backend 切换后的实例断链）。
    #[test]
    fn list_models_bridge_inject_consume_and_clear() {
        let b = CodexAppBackend::new(None);
        // 无桥：回空（fail-soft，与 list_models 的失败语义同向）。
        assert!(
            b.list_models_live().is_empty(),
            "无桥时 list_models_live 应回空"
        );
        // 注入测试桥：非空链可消费（任务验收项「codex-app extra list_models
        // 非空链」）。
        b.set_list_models_bridge(Some(std::sync::Arc::new(|| {
            vec!["gpt-5.3".to_string(), "gpt-5.3-codex".to_string()]
        })));
        assert_eq!(
            b.list_models_live(),
            vec!["gpt-5.3".to_string(), "gpt-5.3-codex".to_string()]
        );
        // 清除：回空（回退链接管语义不变）。
        b.set_list_models_bridge(None);
        assert!(b.list_models_live().is_empty());
    }

    /// v1.37 优雅中断端到端（mock app-server，复用 mock_support 既有形态）：
    /// turn 在飞后 interrupt_run 发信号 → 事件循环对当前 turn 发
    /// `turn/interrupt`（出站 JSON-RPC 断言：mock 收帧日志含该请求帧与
    /// turnId）→ mock 回 interrupted 终态 → run 自然返回 Ok（非 Err），部分
    /// 文本照常产出（terminal=false 对齐 exec reached_terminal），中断注册表
    /// 收尾摘空（RAII 守卫全路径摘除）。无注册 conv 的 interrupt_run = no-op。
    #[tokio::test]
    async fn interrupt_run_sends_turn_interrupt_and_run_returns_naturally() {
        let dir = std::env::temp_dir().join(format!(
            "imagent-codex-app-backend-interrupt-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let (script, log) = crate::app_server::mock_support::mock_script(&dir, "interrupt");
        let mut backend = CodexAppBackend::new(Some("on-request"));
        backend.program = script.to_string_lossy().into_owned();
        let backend = std::sync::Arc::new(backend);

        // 无在飞 run 的 no-op 路径（不 panic、无副作用）。
        backend.interrupt_run(&ConvId("conv-none".into())).await;

        let (tx, _rx) = mpsc::channel::<AgentChunk>(64);
        let (_steer_tx, steer_rx) = mpsc::channel::<String>(4);
        let conv = "conv-1".to_string();
        let wd = dir.clone();
        let run_handle = {
            let backend = backend.clone();
            tokio::spawn(async move {
                backend
                    .run(
                        RoundContext {
                            conv_id: &conv,
                            workdir: &wd,
                            allowed_tools: &[],
                            initial_todos: &[],
                            model: None,
                            effort: None,
                            permission_mode: PermissionMode::Off,
                            steer: steer_rx,
                        },
                        "长任务",
                        None,
                        tx,
                    )
                    .await
            })
        };
        // 等 mock 收到 turn/start（turn 在飞）再发中断信号——与真实 /stop 时
        // 序一致；即便信号早于事件循环启动，watch 保留版本也会首轮触发。
        // （预算放宽到 15s：全量并行下 mock 进程 spawn + 逐行轮询的尾部抖动。）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let logged = std::fs::read_to_string(&log).unwrap_or_default();
            if logged.contains("\"method\":\"turn/start\"") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "mock 未在预算内收到 turn/start"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        backend.interrupt_run(&ConvId("conv-1".into())).await;

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(25), run_handle)
            .await
            .expect("run 应在预算内自然结束（而非悬挂至超时）")
            .expect("run task join")
            .expect("优雅中断后 run 应 Ok 而非 Err");
        assert!(
            !outcome.terminal,
            "interrupted 终态 = 非正常完成（对齐 exec reached_terminal）"
        );
        assert_eq!(outcome.session_id.0, "th-1");
        assert!(
            outcome.final_text.contains("部分输出"),
            "已产出文本照常落账：{}",
            outcome.final_text
        );
        // 出站 JSON-RPC 断言：mock 收帧日志含 turn/interrupt 请求帧。
        let logged = std::fs::read_to_string(&log).expect("读 mock 日志");
        assert!(
            logged.contains("\"method\":\"turn/interrupt\""),
            "出站帧未到达 mock：{logged}"
        );
        assert!(logged.contains("\"turnId\":\"turn-1\""), "{logged}");
        // run 收尾（Ok 路径）注册表摘空。
        assert!(
            backend.interrupts.lock().unwrap().is_empty(),
            "run 收尾应摘除中断注册"
        );
    }

    /// v1.37 中断注册的摘除覆盖早退 Err 路径（spawn 失败 → guard 摘除）：
    /// 不可执行名 spawn 必败，run 返回 Err 后注册表应已摘空。
    #[tokio::test]
    async fn interrupt_registration_removed_on_early_error() {
        let dir = std::env::temp_dir().join(format!(
            "imagent-codex-app-backend-interrupt-err-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let mut backend = CodexAppBackend::new(None);
        backend.program = "imagent-nonexistent-codex-mock".to_string();
        let (tx, _rx) = mpsc::channel::<AgentChunk>(8);
        let (_steer_tx, steer_rx) = mpsc::channel::<String>(4);
        let res = backend
            .run(
                RoundContext {
                    conv_id: "conv-err",
                    workdir: &dir,
                    allowed_tools: &[],
                    initial_todos: &[],
                    model: None,
                    effort: None,
                    permission_mode: PermissionMode::Off,
                    steer: steer_rx,
                },
                "x",
                None,
                tx,
            )
            .await;
        assert!(res.is_err(), "spawn 必败应 Err");
        assert!(
            backend.interrupts.lock().unwrap().is_empty(),
            "早退 Err 路径也应摘除中断注册"
        );
    }
}
