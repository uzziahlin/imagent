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
}

impl ClaudeBackend {
    pub fn new() -> Self {
        Self {
            permission_mode: Arc::new(RwLock::new(PermissionMode::Off)),
            native_perm_mode: RwLock::new(None),
            ask_timeout: std::time::Duration::from_secs(300),
        }
    }

    pub fn with_permission_mode(mode: PermissionMode) -> Self {
        Self {
            permission_mode: Arc::new(RwLock::new(mode)),
            native_perm_mode: RwLock::new(None),
            ask_timeout: std::time::Duration::from_secs(300),
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
        }
    }
}

impl ClaudeBackend {
    /// P8-4：设置 `claude_permission_mode` 透传覆盖（SIGHUP 热更新；见
    /// [`claude_native_perm_args`]）。值须已经过 config 校验归一。
    pub fn set_native_permission_mode(&self, mode: Option<String>) {
        *self.native_perm_mode.write() = mode;
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
async fn write_mcp_config(
    conv_id: &str,
    sock: &str,
    mode: PermissionMode,
    ask_timeout_secs: u64,
) -> std::io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    let cfg = serde_json::json!({
        "mcpServers": {
            imagent_core::mcp::SERVER_NAME: {
                "command": exe.to_string_lossy(),
                // S-3：--ask-timeout 把 permission_ask_timeout 传给 MCP server 子进程，
                // 与 dispatcher 审批等待预算对齐（防 MCP 先超时返 deny）。
                "args": [
                    "mcp", "--conv-id", conv_id, "--sock", sock,
                    "--mode", mode.as_str(),
                    "--ask-timeout", ask_timeout_secs.to_string(),
                ]
            }
        }
    });
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

    /// P4-11：扫 `~/.claude/projects/<workdir编码>/*.jsonl`（电脑端开的会话与
    /// IM 会话同存储），供统一 /resume 列表合并展示。
    async fn list_local_sessions(&self, workdir: &std::path::Path) -> Vec<LocalSession> {
        crate::sessions::scan_for_backend(workdir)
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
            .arg("-p")
            .arg(prompt)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose");
        // 「不限制」（空/["*"]，缺省即全量）不附加 flag——claude 自身默认 = 全量工具
        //（危险操作仍受 permission_mode 审批闭环约束）；显式列表才收敛。
        if !imagent_core::backend_common::tools_unrestricted(allowed_tools) {
            cmd.arg("--allowedTools").arg(allowed_tools.join(","));
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
        let mode = *self.permission_mode.read();
        let mcp_json: Option<std::path::PathBuf> = if mode.is_enabled() {
            let sock = permission_sock_path();
            match write_mcp_config(conv_id, &sock, mode, self.ask_timeout.as_secs()).await {
                Ok(p) => {
                    cmd.arg("--mcp-config").arg(&p);
                    // claude 要求 server 限定全名（mcp__<server>__<tool>）——真机
                    // 校准发现裸工具名被 CLI 2.1.x 拒绝（"MCP tool not found"）。
                    cmd.arg("--permission-prompt-tool")
                        .arg(imagent_core::mcp::qualified_tool_name());
                    // P8-4：原生权限模式透传（auto 档缺省 auto / 显式配置覆盖），
                    // 见 [`claude_native_perm_args`]。
                    for a in claude_native_perm_args(mode, self.native_perm_mode.read().as_deref())
                    {
                        cmd.arg(a);
                    }
                    Some(p)
                }
                Err(e) => {
                    // fail-closed：写 mcp 配置失败时拒绝运行，而非无审批放行。
                    return Err(CoreError::Backend(
                        NAME,
                        format!(
                            "permission_mode={mode:?} 要求权限审批，但写 mcp 配置失败，fail-closed 拒绝运行：{e}",
                        ),
                    ));
                }
            }
        } else {
            None
        };
        let result = spawn_cli_backend(
            cmd,
            claude_parse,
            chunks,
            NAME,
            // S-2：仅透传 claude 所需凭据/端点（最小授权）。
            &["ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL"],
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
            tool_uses,
            session_id,
        } => {
            // B7/B8：一条 assistant 消息可产出多个事件——session 捕获 + 中间文本
            // 推流（codex/gemini 均推 Text，claude 此前归 Skip）+ 全部并行 tool_use。
            // final_text 语义不变：中间 Text 只参与拼接候选，终止 result 事件仍
            // 整体覆盖（见 backend_common B9 注释）。
            let mut evs = Vec::new();
            if let Some(s) = session_id {
                if !s.is_empty() {
                    evs.push(CliEvent::Session(s));
                }
            }
            if !text.is_empty() {
                evs.push(CliEvent::Text(text));
            }
            for u in tool_uses {
                evs.push(CliEvent::ToolUse {
                    tool: u.tool,
                    input: u.input,
                    session: None,
                });
            }
            if evs.is_empty() {
                CliEvent::Skip
            } else {
                CliEvent::Multi(evs)
            }
        }
        ParsedEvent::ToolResults { results } => {
            // B7：一条 user 消息的全部并行 tool_result 都产出。
            if results.is_empty() {
                CliEvent::Skip
            } else {
                CliEvent::Multi(
                    results
                        .into_iter()
                        .map(|r| CliEvent::ToolResult {
                            tool: r.tool,
                            output: r.output,
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
}
