//! [`ClaudeBackend`]：基于 Claude Code CLI 的无状态 agent 执行器。

use std::sync::Arc;

use async_trait::async_trait;
use imagent_core::{
    backend_common::{spawn_cli_backend, CliEvent},
    AgentChunk, Backend, CoreError, LocalSession, PermissionCapability, PermissionMode, Result,
    RunOutcome, SessionId,
};
use parking_lot::RwLock;
use tokio::process::Command;

use crate::stream::{parse_line, ParsedEvent};

/// Claude Code CLI 后端。
///
/// `permission_mode` 非 Off 时，spawn claude 时附加 `--mcp-config` +
/// `--permission-prompt-tool`，把权限决策回调到 imagent 的 MCP server 子进程
/// （imagent mcp 子命令）。
pub struct ClaudeBackend {
    permission_mode: Arc<RwLock<PermissionMode>>,
    /// P8-4：`claude_permission_mode` 配置透传（`--permission-mode` 原生值）。
    /// None = 按档缺省（auto-claude → "auto"，其余不透传）；Some = 显式覆盖
    ///（两档都遵从）。SIGHUP 经 [`Backend::set_native_permission_mode`] 热更新。
    native_perm_mode: RwLock<Option<String>>,
    /// S-3：MCP server（imagent mcp）socket 读超时，与 dispatcher 的 permission_ask_timeout
    /// 对齐——防 MCP 子进程在用户慢回复时先于 dispatcher 超时返 deny，使 Ask 闭环静默失效。
    ask_timeout: std::time::Duration,
    /// W1-2：运行时模型（`/model` 热设；main 启动时以 config `claude_model` 为
    /// 初值注入同一句柄——config 与命令同源，SIGHUP 重设回 config 值）。
    model: RwLock<Option<String>>,
    /// W1-2/W1-3/W1-4：config 侧运行参数（SIGHUP 经 [`Self::set_runtime_opts`] 整体替换）。
    runtime: RwLock<RuntimeOpts>,
    /// 审批传输通道（config `claude_permission_channel`，SIGHUP 热切）：
    /// Control = canUseTool 双工协议（SDK 现行标准，缺省）；Mcp = 旧
    /// `--permission-prompt-tool` 机制（legacy 回退）。
    permission_channel: RwLock<PermissionChannel>,
}

/// claude 审批传输通道（见 [`ClaudeBackend::permission_channel`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionChannel {
    /// canUseTool：`--input-format stream-json` 双工，审批经 control_request/
    /// control_response 走 stdin/stdout，复用 permission.sock 路由到 IM。
    #[default]
    Control,
    /// MCP `--permission-prompt-tool`（legacy）：MCP 子进程 + UDS + 每轮临时
    /// mcp_*.json——保留为回退通道（真机校准有问题时 config 切回）。
    Mcp,
}

/// W1：config 注入的 claude 运行参数（一次锁读全取，避免 run 热路径多次加锁）。
#[derive(Debug, Clone, Default)]
pub struct RuntimeOpts {
    /// fallback 模型（`--fallback-model`；仅 model 设置时附加）。
    pub fallback_model: Option<String>,
    /// 禁用工具黑名单（`--disallowedTools`；空 = 不附加）。
    pub disallowed_tools: Vec<String>,
    /// 附加系统提示（`--append-system-prompt`）。
    pub append_system_prompt: Option<String>,
    /// 用户 MCP servers 配置（`mcp_config_path` 的解析产物，顶层含 `mcpServers`；
    /// 合并进每次生成的 mcp 配置）。
    pub extra_mcp: Option<serde_json::Value>,
}

impl ClaudeBackend {
    pub fn new() -> Self {
        Self {
            permission_mode: Arc::new(RwLock::new(PermissionMode::Off)),
            native_perm_mode: RwLock::new(None),
            ask_timeout: std::time::Duration::from_secs(300),
            model: RwLock::new(None),
            runtime: RwLock::new(RuntimeOpts::default()),
            permission_channel: RwLock::new(PermissionChannel::default()),
        }
    }

    pub fn with_permission_mode(mode: PermissionMode) -> Self {
        Self {
            permission_mode: Arc::new(RwLock::new(mode)),
            native_perm_mode: RwLock::new(None),
            ask_timeout: std::time::Duration::from_secs(300),
            model: RwLock::new(None),
            runtime: RwLock::new(RuntimeOpts::default()),
            permission_channel: RwLock::new(PermissionChannel::default()),
        }
    }

    /// 用外部共享句柄构造——与 `Dispatcher` 共享同一 `Arc<RwLock<PermissionMode>>`，
    /// 使 SIGHUP 热重载对 backend 即时生效（每次 `run` 取最新值）。`ask_timeout` 为
    /// MCP server 的 socket 读超时（S-3，= config.permission_ask_timeout_secs）。
    pub fn with_permission_mode_shared(
        mode: Arc<RwLock<PermissionMode>>,
        ask_timeout: std::time::Duration,
    ) -> Self {
        Self {
            permission_mode: mode,
            native_perm_mode: RwLock::new(None),
            ask_timeout,
            model: RwLock::new(None),
            runtime: RwLock::new(RuntimeOpts::default()),
            permission_channel: RwLock::new(PermissionChannel::default()),
        }
    }
}

impl ClaudeBackend {
    /// 设置审批传输通道（config `claude_permission_channel`，main 启动 +
    /// SIGHUP 调用；值已过 config 校验，lossy 兜底 control）。
    pub fn set_permission_channel(&self, channel: &str) {
        let ch = match channel.trim().to_ascii_lowercase().as_str() {
            "mcp" => PermissionChannel::Mcp,
            _ => PermissionChannel::Control,
        };
        *self.permission_channel.write() = ch;
    }

    /// P8-4：设置 `claude_permission_mode` 透传覆盖（SIGHUP 热更新；见
    /// [`claude_native_perm_args`]）。值须已经过 config 校验归一。
    pub fn set_native_permission_mode(&self, mode: Option<String>) {
        *self.native_perm_mode.write() = mode;
    }

    /// W1-2/W1-3/W1-4：注入 config 侧运行参数（main 启动与 SIGHUP 调用；
    /// `extra_mcp_path` 现场读取解析，读失败 warn 后按无用户 servers 处理——
    /// config load 期已校验过一次，此处失败属文件后来被改动）。
    pub fn set_runtime_opts(
        &self,
        fallback_model: Option<String>,
        disallowed_tools: Vec<String>,
        append_system_prompt: Option<String>,
        extra_mcp_path: Option<&std::path::Path>,
    ) {
        let extra_mcp = extra_mcp_path.and_then(|p| {
            std::fs::read_to_string(p)
                .ok()
                .and_then(|raw| serde_json::from_str(&raw).ok())
                .map(|v: serde_json::Value| {
                    if v.get("mcpServers").is_some() {
                        v
                    } else {
                        tracing::warn!(
                            target: "imagent::backend",
                            path = %p.display(),
                            "mcp 配置缺 mcpServers，按无用户 servers 处理"
                        );
                        serde_json::Value::Null
                    }
                })
                .filter(|v| !v.is_null())
        });
        *self.runtime.write() = RuntimeOpts {
            fallback_model,
            disallowed_tools,
            append_system_prompt,
            extra_mcp,
        };
    }
}

impl Default for ClaudeBackend {
    fn default() -> Self {
        Self::new()
    }
}

const NAME: &str = "claude-cli";

/// 固定 socket 路径（主进程 PermissionRouter 监听、MCP server 连接）。
/// P4-10：锚定 `imagent_home()`（`--profile` 时随 profile 隔离，env 对被 spawn 的
/// MCP 子进程同样生效）；home 不可解析时回退 /tmp。
/// P8-4：档位 + 透传覆盖 → claude 原生 `--permission-mode` flag。
///
/// - 显式覆盖（`claude_permission_mode` 配置）：两档（auto-claude / ask）都遵从。
/// - 缺省：auto-claude（auto 在 claude-cli 的解析产物）透传 **auto**——Claude Code
///   2026 新档：独立分类器逐动作审查，安全操作自动放行，只有高危动作（curl|bash、
///   外发敏感数据、强推等）拦下提示 → 经审批闭环进 IM。ask 档不透传（claude
///   default 手动把关 = 每个提示都进 IM，全量交给用户）。与 approval_tools 可
///   叠加（剩余提示再按清单过滤）。旧版 CLI（<2.1.228）不认 auto 会静默回退
///   default（≈ask 档行为，降级安全）。
/// Control 通道首条 stdin 消息（SDK 式 user 投递，`--input-format stream-json`）：
/// `{"type":"user","message":{"role":"user","content":[{"type":"text","text":…}]}}`。
/// 形态**待真机校准**（SDK 公开协议建模；resume 仍走 `--resume` flag）。
fn sdk_user_message(prompt: &str) -> String {
    let msg = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [ { "type": "text", "text": prompt } ],
        }
    });
    format!("{msg}\n")
}

fn claude_native_perm_args(mode: PermissionMode, native_override: Option<&str>) -> Vec<String> {
    let native: Option<String> = match native_override {
        Some(m) => Some(m.to_string()),
        None if mode == PermissionMode::AutoClaude => Some("auto".to_string()),
        None => None,
    };
    match native {
        Some(m) => vec!["--permission-mode".to_string(), m],
        None => Vec::new(),
    }
}

fn permission_sock_path() -> String {
    imagent_core::paths::imagent_home()
        .join("permission.sock")
        .to_string_lossy()
        .into_owned()
}

/// 把 conv_id 消毒为文件名安全片段（P2-I：防路径遍历——`/`、`..`、`:` 等替换为 `_``）。
/// 仅用于构造 `mcp_<conv>.json` 文件名；MCP server 的 `--conv-id` 参数仍用原 conv_id（路由一致）。
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 写临时 mcp.json，返回路径。claude 据此 spawn MCP server 子进程。
///
/// W1-3：`extra`（用户 `mcp_config_path` 的解析产物）中的 `mcpServers` 条目会
/// **合并**进生成配置（给 agent 挂用户工具）；名为 [`imagent_core::mcp::SERVER_NAME`]
/// 的条目跳过（审批闭环专用名，防遮蔽）。`sock = None` 表示不挂审批闭环
/// （permission_mode=Off 但用户配置了 MCP servers 时：纯用户工具，无 imagent 条目）。
async fn write_mcp_config(
    conv_id: &str,
    sock: Option<&str>,
    mode: PermissionMode,
    ask_timeout_secs: u64,
    extra: Option<&serde_json::Value>,
) -> std::io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    let mut servers = serde_json::Map::new();
    if let Some(user) = extra
        .and_then(|v| v.get("mcpServers"))
        .and_then(|m| m.as_object())
    {
        for (k, v) in user {
            if k == imagent_core::mcp::SERVER_NAME {
                tracing::warn!(
                    target: "imagent::backend",
                    "用户 mcp 配置含保留名 {}，跳过该条目（审批闭环专用）",
                    imagent_core::mcp::SERVER_NAME
                );
                continue;
            }
            servers.insert(k.clone(), v.clone());
        }
    }
    if let Some(sock) = sock {
        servers.insert(
            imagent_core::mcp::SERVER_NAME.to_string(),
            serde_json::json!({
                "command": exe.to_string_lossy(),
                // S-3：--ask-timeout 把 permission_ask_timeout 传给 MCP server 子进程，
                // 与 dispatcher 审批等待预算对齐（防 MCP 先超时返 deny）。
                "args": [
                    "mcp", "--conv-id", conv_id, "--sock", sock,
                    "--mode", mode.as_str(),
                    "--ask-timeout", ask_timeout_secs.to_string(),
                ]
            }),
        );
    }
    let cfg = serde_json::json!({ "mcpServers": serde_json::Value::Object(servers) });
    // B6：mcp json 目录与 permission.sock 一致锚定 `imagent_home()`——`--profile`
    // 时随 profile 隔离（此前写死 `~/.imagent`，多实例共用会互相覆盖 mcp_*.json）。
    // 旧路径 `~/.imagent/mcp_*.json` 残留文件不迁移（run 结束会删本次文件；全局
    // grep 确认无其他代码引用旧路径）。
    let dir = imagent_core::paths::imagent_home();
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("mcp_{}.json", sanitize_filename(conv_id)));
    // S-6：temp + rename 原子替换（替代 check-then-write 的 TOCTOU 竞窗）。临时文件用
    // create_new（O_CREAT|O_EXCL）原子创建——对已存在 symlink 返回 EEXIST，不跟随；
    // rename 不跟随目标 symlink（替换目录项本身）。防 ~/.imagent 被植入 symlink 指向
    // 受害者文件（如 ~/.ssh/authorized_keys）导致覆写。
    let tmp = dir.join(format!(
        ".mcp_{}.{}.tmp",
        sanitize_filename(conv_id),
        std::process::id()
    ));
    let _ = tokio::fs::remove_file(&tmp).await; // 清理上次异常残留的 tmp
    {
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .await?;
        f.write_all(cfg.to_string().as_bytes()).await?;
        f.flush().await?;
    }
    tokio::fs::rename(&tmp, &path).await?;
    Ok(path)
}

#[async_trait]
impl Backend for ClaudeBackend {
    fn set_native_permission_mode(&self, mode: Option<String>) {
        self.set_native_permission_mode(mode);
    }
    fn supports_native_permission_mode(&self) -> bool {
        true
    }

    /// B3：claude-cli 经 `--permission-prompt-tool`（MCP 子进程 → permission
    /// socket → PermissionRouter → IM）实现完整审批闭环。
    fn permission_capability(&self) -> PermissionCapability {
        PermissionCapability::FullLoop
    }

    fn name(&self) -> &'static str {
        NAME
    }

    /// W1-2：/model 热设（进程内；SIGHUP 时 main 重设回 config 基准值）。
    fn set_model(&self, model: Option<String>) {
        *self.model.write() = model;
    }

    fn model(&self) -> Option<String> {
        self.model.read().clone()
    }

    fn supports_model_selection(&self) -> bool {
        true
    }

    /// P4-11：扫 `~/.claude/projects/<workdir编码>/*.jsonl`（电脑端开的会话与
    /// IM 会话同存储），供统一 /resume 列表合并展示。
    async fn list_local_sessions(&self, workdir: &std::path::Path) -> Vec<LocalSession> {
        crate::sessions::scan_for_backend(workdir)
    }

    /// W4-2：会话转录导出（与 /resume 同一 ~/.claude 存储）。
    async fn export_session_markdown(
        &self,
        workdir: &std::path::Path,
        session_id: &str,
    ) -> Option<String> {
        crate::sessions::export_session_md(workdir, session_id)
    }

    async fn run(
        &self,
        conv_id: &str,
        prompt: &str,
        session: Option<&SessionId>,
        workdir: &std::path::Path,
        allowed_tools: &[String],
        chunks: tokio::sync::mpsc::Sender<AgentChunk>,
    ) -> Result<RunOutcome> {
        // 构造命令（prompt 作为单个 arg，不经 shell；stdin/stdout/stderr/kill_on_drop
        // 由 spawn_cli_backend 统一加）。
        let mut cmd = Command::new("claude");
        cmd.current_dir(workdir)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose");
        // 审批通道分流（config `claude_permission_channel`，SIGHUP 热切）：
        // - Control（缺省）：SDK 式双工——`--input-format stream-json`，prompt 经
        //   stdin user 消息投递，stdin 保持打开供 control_response 回写；
        // - Mcp（legacy 回退）：`-p <prompt>` + MCP prompt-tool + 子进程/临时配置。
        let channel = *self.permission_channel.read();
        let control_io = if channel == PermissionChannel::Control {
            cmd.arg("-p").arg("--input-format").arg("stream-json");
            // 真机校准（2026-08-30）：`stdio` 特殊值把审批接到 stdio 控制通道
            //（SDK 同款接线）——不传则 claude 对未批工具**直接拒绝**不发
            // control_request（实测）。
            cmd.arg("--permission-prompt-tool").arg("stdio");
            Some(imagent_core::backend_common::ControlIo {
                sock: permission_sock_path(),
                conv_id: conv_id.to_string(),
                ask_timeout: self.ask_timeout,
                initial_stdin_message: sdk_user_message(prompt),
            })
        } else {
            cmd.arg("-p").arg(prompt);
            None
        };
        // 「不限制」（空/["*"]，缺省即全量）不附加 flag——claude 自身默认 = 全量工具
        //（危险操作仍受 permission_mode 审批闭环约束）；显式列表才收敛。
        if !imagent_core::backend_common::tools_unrestricted(allowed_tools) {
            cmd.arg("--allowedTools").arg(allowed_tools.join(","));
        }
        // W1-2/W1-4：模型选择 + 禁用工具黑名单 + 附加系统提示（一次锁读全取）。
        let opts = self.runtime.read().clone();
        if let Some(m) = self.model.read().clone() {
            cmd.arg("--model").arg(&m);
            if let Some(f) = opts.fallback_model.clone() {
                cmd.arg("--fallback-model").arg(&f);
            }
        }
        if !opts.disallowed_tools.is_empty() {
            cmd.arg("--disallowedTools")
                .arg(opts.disallowed_tools.join(","));
        }
        if let Some(sys) = opts.append_system_prompt.clone() {
            cmd.arg("--append-system-prompt").arg(&sys);
        }
        // 幽灵会话预检（真机校准）：失败轮次泄漏并落库的 session id 在 ~/.claude
        // 本地存储并无对应 jsonl——resume 它只会得到 is_error 空文本 result 且每轮
        // 再产新幽灵 id（毒化循环）。不存在即弃用续接、开新会话。
        let session = session.filter(|s| {
            let ok = crate::sessions::session_exists(workdir, &s.0);
            if !ok {
                tracing::warn!(
                    target: "imagent::backend",
                    session_id = %s.0,
                    "续接的 session 在 ~/.claude 本地存储不存在（幽灵会话），弃用续接开新会话"
                );
            }
            ok
        });
        if let Some(s) = session {
            cmd.arg("--resume").arg(&s.0);
        }
        // 权限审批：非 Off 时附加 MCP server（imagent mcp 子命令）；claude 遇需权限工具
        // 时回调 permission_request，由 MCP server 依模式 allow/deny 或经 socket 转 IM 询问。
        // W1-3：用户配置了 MCP servers 时，即使 Off 档也写 mcp 配置（纯用户工具，
        // 不含 imagent 审批条目、不挂 --permission-prompt-tool）。
        let mode = *self.permission_mode.read();
        let extra = opts.extra_mcp.clone();
        let use_control = control_io.is_some();
        let mcp_json: Option<std::path::PathBuf> = if mode.is_enabled() || extra.is_some() {
            // Control 通道：审批不经 MCP（sock=None——mcp 配置只承载用户 servers，
            // 无 imagent 条目、不挂 --permission-prompt-tool）。
            let sock = (mode.is_enabled() && !use_control).then(permission_sock_path);
            match write_mcp_config(
                conv_id,
                sock.as_deref(),
                mode,
                self.ask_timeout.as_secs(),
                extra.as_ref(),
            )
            .await
            {
                Ok(p) => {
                    cmd.arg("--mcp-config").arg(&p);
                    if mode.is_enabled() && !use_control {
                        // claude 要求 server 限定全名（mcp__<server>__<tool>）——真机
                        // 校准发现裸工具名被 CLI 2.1.x 拒绝（"MCP tool not found"）。
                        cmd.arg("--permission-prompt-tool")
                            .arg(imagent_core::mcp::qualified_tool_name());
                    }
                    if mode.is_enabled() {
                        // P8-4：原生权限模式透传（auto 档缺省 auto / 显式配置覆盖；
                        // 与通道正交——分类器层，两通道都在 canUseTool/prompt-tool
                        // 之前先放行安全操作），见 [`claude_native_perm_args`]。
                        for a in
                            claude_native_perm_args(mode, self.native_perm_mode.read().as_deref())
                        {
                            cmd.arg(a);
                        }
                    }
                    Some(p)
                }
                Err(e) => {
                    if mode.is_enabled() {
                        // fail-closed：写 mcp 配置失败时拒绝运行，而非无审批放行。
                        return Err(CoreError::Backend(
                            NAME,
                            format!(
                                "permission_mode={mode:?} 要求权限审批，但写 mcp 配置失败，fail-closed 拒绝运行：{e}",
                            ),
                        ));
                    }
                    // 纯用户工具路径：warn 后继续（工具缺席不是安全问题）。
                    tracing::warn!(
                        target: "imagent::backend",
                        error = %e,
                        "写用户 mcp 配置失败，本轮不挂用户 MCP servers（不影响运行）"
                    );
                    None
                }
            }
        } else {
            None
        };
        // 诊断（control 通道真机校准）：完整 spawn 参数。
        tracing::debug!(target: "imagent::backend", args = ?cmd,
            "claude spawn 参数（get_all）");
        let result = spawn_cli_backend(
            cmd,
            claude_parse,
            chunks,
            NAME,
            // S-2：仅透传 claude 所需凭据/端点（最小授权）。
            &["ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL"],
            control_io,
        )
        .await;
        // S-6 / P3-2：run 结束（claude 子进程已退出）清理本次 mcp 配置，避免
        // ~/.imagent 堆积 mcp_*.json 残留 + 文件名泄漏 conv_id。
        if let Some(p) = &mcp_json {
            let _ = tokio::fs::remove_file(p).await;
        }
        result
    }
}

/// claude stream-json 行 → [`CliEvent`] 适配（见 [`parse_line`]）。
fn claude_parse(line: &str) -> CliEvent {
    // canUseTool 控制请求（--input-format stream-json 双工协议）：弱解析——
    // type==control_request 即认，字段名容错（tool_name|tool、input|arguments），
    // 提取失败也产生事件（subtype 透传；responder 对非 can_use_tool 回 error
    // 响应防挂起）。字段形态**待真机校准**（SDK 公开协议按文档建模）。
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
        if v.get("type").and_then(|t| t.as_str()) == Some("control_request") {
            // 真机校准（2026-08-30 实测 2.1.250）：payload 嵌套在 `request` 键下
            //（subtype/tool_name/input 全在内层）；顶层形态作回退。
            let req = v.get("request");
            let pick = |top: &str, inner: &str| -> Option<String> {
                v.get(top)
                    .and_then(|x| x.as_str())
                    .map(str::to_string)
                    .or_else(|| {
                        req.and_then(|r| r.get(inner))
                            .and_then(|x| x.as_str())
                            .map(str::to_string)
                    })
            };
            let request_id = v
                .get("request_id")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            let subtype = pick("subtype", "subtype").unwrap_or_default();
            let tool_name = pick("tool_name", "tool_name")
                .or_else(|| pick("tool", "tool_name"))
                .unwrap_or_default();
            let input = v
                .get("input")
                .cloned()
                .or_else(|| req.and_then(|r| r.get("input")).cloned())
                .map(|i| i.to_string())
                .unwrap_or_else(|| "{}".into());
            return CliEvent::ControlRequest {
                request_id,
                subtype,
                tool_name,
                input,
            };
        }
    }
    match parse_line(line) {
        ParsedEvent::Result {
            text,
            is_error,
            session_id,
            usage,
        } => {
            // usage 事件须排在终止事件之前——spawn_cli_backend 的读取循环在
            // Final/Error 处 break，排在后的同批事件会被丢弃。
            let term = if is_error {
                CliEvent::Error {
                    text,
                    session: session_id,
                }
            } else {
                CliEvent::Final {
                    text,
                    session: session_id,
                }
            };
            match usage {
                Some(u) => CliEvent::Multi(vec![CliEvent::Usage(u), term]),
                None => term,
            }
        }
        ParsedEvent::Assistant {
            text,
            thoughts,
            tool_uses,
            session_id,
        } => {
            // B7/B8：一条 assistant 消息可产出多个事件——session 捕获 + 中间文本
            // 推流（codex/gemini 均推 Text，claude 此前归 Skip）+ 全部并行 tool_use。
            // final_text 语义不变：中间 Text 只参与拼接候选，终止 result 事件仍
            // 整体覆盖（见 backend_common B9 注释）。
            // W2-1：thinking 块透出为 Thought（卡片折叠区，与正文分离）。
            let mut evs = Vec::new();
            if let Some(s) = session_id {
                if !s.is_empty() {
                    evs.push(CliEvent::Session(s));
                }
            }
            if !thoughts.is_empty() {
                evs.push(CliEvent::Thought(thoughts));
            }
            if !text.is_empty() {
                evs.push(CliEvent::Text(text));
            }
            for u in tool_uses {
                // W2-2：TodoWrite 结构化为任务清单（卡片 checklist 进度组件），
                // 不再按普通工具行展示（信息重复且无进度语义）。
                if let Some(items) =
                    imagent_core::backend_common::todo_write_items(&u.tool, &u.input)
                {
                    evs.push(CliEvent::TodoList { items });
                    continue;
                }
                evs.push(CliEvent::ToolUse {
                    tool: u.tool,
                    input: u.input,
                    session: None,
                    id: u.id,
                });
            }
            if evs.is_empty() {
                CliEvent::Skip
            } else {
                CliEvent::Multi(evs)
            }
        }
        ParsedEvent::ToolResults { results } => {
            // B7：一条 user 消息的全部并行 tool_result 都产出（W2-3：带
            // tool_use_id 供精确配对）。
            if results.is_empty() {
                CliEvent::Skip
            } else {
                CliEvent::Multi(
                    results
                        .into_iter()
                        .map(|r| CliEvent::ToolResult {
                            tool: r.tool,
                            output: r.output,
                            id: r.id,
                        })
                        .collect(),
                )
            }
        }
        ParsedEvent::Other { session_id } => session_id.map_or(CliEvent::Skip, CliEvent::Session),
        ParsedEvent::Skip => CliEvent::Skip,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// B3：能力协商——claude-cli 经 MCP → socket → PermissionRouter 走完整
    /// IM 审批闭环，声明 FullLoop（ask/auto-claude 档启动放行）。
    #[test]
    fn permission_capability_is_full_loop() {
        assert_eq!(
            ClaudeBackend::new().permission_capability(),
            PermissionCapability::FullLoop
        );
    }

    #[test]
    fn name_is_stable() {
        let b = ClaudeBackend::new();
        assert_eq!(b.name(), "claude-cli");
    }

    /// P8-4：auto-claude 档缺省透传新 auto 模式；显式覆盖两档都遵从；其余档
    ///（含未解析的 Auto）不附加。
    #[test]
    fn native_perm_args_default_and_override() {
        // 缺省：auto-claude → auto；ask / 其它 → 不附加。
        assert_eq!(
            claude_native_perm_args(PermissionMode::AutoClaude, None),
            vec!["--permission-mode".to_string(), "auto".to_string()]
        );
        for m in [
            PermissionMode::Off,
            PermissionMode::Allow,
            PermissionMode::Deny,
            PermissionMode::Ask,
            PermissionMode::Auto,
        ] {
            assert!(
                claude_native_perm_args(m, None).is_empty(),
                "{m:?} 缺省不应附加原生权限 flag"
            );
        }
        // 显式覆盖：任意档都遵从（含 ask）。
        assert_eq!(
            claude_native_perm_args(PermissionMode::Ask, Some("acceptEdits")),
            vec!["--permission-mode".to_string(), "acceptEdits".to_string()]
        );
        assert_eq!(
            claude_native_perm_args(PermissionMode::AutoClaude, Some("bypassPermissions")),
            vec![
                "--permission-mode".to_string(),
                "bypassPermissions".to_string()
            ]
        );
    }

    #[test]
    fn sanitize_filename_strips_traversal() {
        // P2-I：路径遍历 / 分隔符必须消毒为文件名安全片段。
        assert_eq!(sanitize_filename("wecom:alice"), "wecom_alice");
        assert_eq!(sanitize_filename("../etc/passwd"), "___etc_passwd");
        assert_eq!(sanitize_filename("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_filename("ilink_wxid_123"), "ilink_wxid_123");
        assert!(sanitize_filename("../..").chars().all(|c| c == '_'));
    }

    /// W1-3：用户 MCP servers 合并——extra 的条目并入、保留名 imagent 被跳过
    /// （审批条目由 sock 分支写入、同名遮蔽不可能）；sock=None 时纯用户条目、
    /// 无 imagent 审批 server。
    #[tokio::test]
    async fn mcp_config_merges_user_servers_and_guards_reserved_name() {
        // 隔离 IMAGENT_HOME（write_mcp_config 锚定它写文件），测试后恢复。
        let home = std::env::temp_dir().join(format!("imagent_claude_mcp_{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var(imagent_core::paths::IMAGENT_HOME_ENV, &home);
        let extra: serde_json::Value = serde_json::from_str(
            r#"{"mcpServers": {
                "fetch": {"command": "uvx", "args": ["mcp-server-fetch"]},
                "imagent": {"command": "evil-override"}
            }}"#,
        )
        .unwrap();

        // sock = Some：审批 server + 用户条目（imagent 保留名跳过）。
        let p = write_mcp_config(
            "test_conv",
            Some("/tmp/perm.sock"),
            PermissionMode::Ask,
            300,
            Some(&extra),
        )
        .await
        .expect("写 mcp 配置");
        let raw = std::fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let servers = v["mcpServers"].as_object().expect("mcpServers 对象");
        assert!(servers.contains_key("fetch"), "用户 server 应并入: {raw}");
        assert!(
            servers.contains_key(imagent_core::mcp::SERVER_NAME),
            "审批 server 应存在: {raw}"
        );
        assert_eq!(
            servers[imagent_core::mcp::SERVER_NAME]["command"],
            serde_json::json!(std::env::current_exe().unwrap().to_string_lossy()),
            "imagent 条目须为审批闭环定义（用户 evil-override 被跳过）: {raw}"
        );
        let _ = std::fs::remove_file(&p);

        // sock = None：纯用户条目，无 imagent 审批 server。
        let p = write_mcp_config("test_conv", None, PermissionMode::Off, 300, Some(&extra))
            .await
            .expect("写 mcp 配置");
        let raw = std::fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let servers = v["mcpServers"].as_object().expect("mcpServers 对象");
        assert!(servers.contains_key("fetch"), "用户 server 应并入: {raw}");
        assert!(
            !servers.contains_key(imagent_core::mcp::SERVER_NAME),
            "Off 档不应挂审批 server: {raw}"
        );
        let _ = std::fs::remove_file(&p);

        // extra = None + sock = None：空 mcpServers（调用方不会走到，防呆）。
        let p = write_mcp_config("test_conv", None, PermissionMode::Off, 300, None)
            .await
            .expect("写 mcp 配置");
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains("\"mcpServers\":{}"), "空配置形态: {raw}");
        let _ = std::fs::remove_file(&p);
        // 恢复 env 并清理临时 home。
        std::env::remove_var(imagent_core::paths::IMAGENT_HOME_ENV);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 真跑 `claude` CLI 的集成测试。标 `#[ignore]` 以免 CI 依赖 claude。
    ///
    /// 运行：`cargo test -p imagent-claude -- --ignored real_cli`
    #[tokio::test]
    #[ignore]
    async fn real_cli_replies_with_text_and_session() {
        let backend = ClaudeBackend::new();
        let workdir = std::env::current_dir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentChunk>(64);
        let allowed: Vec<String> = vec![];

        let outcome = backend
            .run(
                "test-conv",
                "reply with the single word: pong",
                None,
                &workdir,
                &allowed,
                tx,
            )
            .await
            .expect("claude run should succeed");

        assert!(
            !outcome.session_id.0.is_empty(),
            "session_id must be non-empty"
        );
        assert!(
            !outcome.final_text.trim().is_empty(),
            "final_text must be non-empty"
        );

        // 应至少收到一个 Final chunk。
        let mut got_final = false;
        while let Ok(chunk) = rx.try_recv() {
            if let AgentChunk::Final(t) = chunk {
                assert_eq!(t, outcome.final_text);
                got_final = true;
            }
        }
        assert!(got_final, "expected a Final chunk");
    }

    /// canUseTool 控制请求解析（真机校准轮次新增）：type==control_request 弱
    /// 解析，字段名容错（tool_name|tool、input|arguments）；未知 subtype 也透传
    /// （responder 回 error 防挂起）；非 control_request 行不受影响。
    #[test]
    fn claude_parse_control_request() {
        let ev = claude_parse(
            r#"{"type":"control_request","request_id":"req_1","subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}"#,
        );
        match ev {
            CliEvent::ControlRequest {
                request_id,
                subtype,
                tool_name,
                input,
            } => {
                assert_eq!(request_id, "req_1");
                assert_eq!(subtype, "can_use_tool");
                assert_eq!(tool_name, "Bash");
                assert!(input.contains("command"));
            }
            other => panic!("应为 ControlRequest: {other:?}"),
        }
        // 字段容错形态（arguments / tool）。
        let ev2 = claude_parse(
            r#"{"type":"control_request","request_id":"r2","subtype":"can_use_tool","tool":"Read","arguments":{"file_path":"/a"}}"#,
        );
        assert!(matches!(
            ev2,
            CliEvent::ControlRequest { ref tool_name, ref subtype, .. }
                if tool_name == "Read" && subtype == "can_use_tool"
        ));
        // 未知 subtype 透传（防挂起路径）。
        assert!(matches!(
            claude_parse(r#"{"type":"control_request","request_id":"r3","subtype":"mcp_server_op"}"#),
            CliEvent::ControlRequest { ref subtype, .. } if subtype == "mcp_server_op"
        ));
        // 普通事件不受影响。
        assert!(matches!(
            claude_parse(r#"{"type":"system","subtype":"init","session_id":"s1"}"#),
            CliEvent::Session(ref s) if s == "s1"
        ));
    }

    /// 真机校准（2026-08-30 实测 2.1.250）：control_request payload 嵌套在
    /// `request` 键下（subtype/tool_name/input 全在内层）。
    #[test]
    fn claude_parse_control_request_nested_shape() {
        let ev = claude_parse(
            r#"{"type":"control_request","request_id":"rid-1","request":{"subtype":"can_use_tool","tool_name":"Bash","display_name":"Bash","input":{"command":"ls","description":"x"},"tool_use_id":"call_1"}}"#,
        );
        match ev {
            CliEvent::ControlRequest {
                request_id,
                subtype,
                tool_name,
                input,
            } => {
                assert_eq!(request_id, "rid-1");
                assert_eq!(subtype, "can_use_tool");
                assert_eq!(tool_name, "Bash");
                assert!(input.contains("command"));
            }
            other => panic!("应为 ControlRequest: {other:?}"),
        }
    }

    /// SDK 式 stdin user 消息形态（--input-format stream-json 的 prompt 投递）。
    #[test]
    fn sdk_user_message_shape() {
        let m = sdk_user_message("你好");
        let v: serde_json::Value = serde_json::from_str(m.trim()).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"][0]["type"], "text");
        assert_eq!(v["message"]["content"][0]["text"], "你好");
        assert!(m.ends_with('\n'), "行分隔");
    }

    /// 通道选择：Control 时 spawn 参数走 --input-format stream-json（不挂
    /// --permission-prompt-tool）；Mcp 时走 -p <prompt> 旧路。以 flags 构造逻辑
    /// 间接验证——run() 全链路需子进程，见真机校准。
    #[test]
    fn permission_channel_default_control() {
        let b = ClaudeBackend::new();
        assert_eq!(*b.permission_channel.read(), PermissionChannel::Control);
        b.set_permission_channel("Mcp");
        assert_eq!(*b.permission_channel.read(), PermissionChannel::Mcp);
        b.set_permission_channel("control");
        assert_eq!(*b.permission_channel.read(), PermissionChannel::Control);
        // lossy：未知值兜底 control。
        b.set_permission_channel("nope");
        assert_eq!(*b.permission_channel.read(), PermissionChannel::Control);
    }
}
