//! [`ClaudeBackend`]：基于 Claude Code CLI 的无状态 agent 执行器。

use std::sync::Arc;

use async_trait::async_trait;
use imagent_core::{
    backend_common::{spawn_cli_backend, CliEvent},
    AgentChunk, Backend, CoreError, PermissionMode, Result, RunOutcome, SessionId,
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
}

impl ClaudeBackend {
    pub fn new() -> Self {
        Self {
            permission_mode: Arc::new(RwLock::new(PermissionMode::Off)),
        }
    }

    pub fn with_permission_mode(mode: PermissionMode) -> Self {
        Self {
            permission_mode: Arc::new(RwLock::new(mode)),
        }
    }

    /// 用外部共享句柄构造——与 `Dispatcher` 共享同一 `Arc<RwLock<PermissionMode>>`，
    /// 使 SIGHUP 热重载对 backend 即时生效（每次 `run` 取最新值）。
    pub fn with_permission_mode_shared(mode: Arc<RwLock<PermissionMode>>) -> Self {
        Self {
            permission_mode: mode,
        }
    }
}

impl Default for ClaudeBackend {
    fn default() -> Self {
        Self::new()
    }
}

const NAME: &str = "claude-cli";

/// 固定 socket 路径（主进程 PermissionRouter 监听、MCP server 连接）。
fn permission_sock_path() -> String {
    dirs::home_dir()
        .map(|h| {
            h.join(".imagent")
                .join("permission.sock")
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| "/tmp/imagent-permission.sock".into())
}

/// 写临时 mcp.json，返回路径。claude 据此 spawn MCP server 子进程。
async fn write_mcp_config(
    conv_id: &str,
    sock: &str,
    mode: PermissionMode,
) -> std::io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    let cfg = serde_json::json!({
        "mcpServers": {
            "imagent": {
                "command": exe.to_string_lossy(),
                "args": ["mcp", "--conv-id", conv_id, "--sock", sock, "--mode", mode.as_str()]
            }
        }
    });
    let dir = dirs::home_dir()
        .map(|h| h.join(".imagent"))
        .unwrap_or_else(std::env::temp_dir);
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("mcp_{conv_id}.json"));
    tokio::fs::write(&path, cfg.to_string()).await?;
    Ok(path)
}

#[async_trait]
impl Backend for ClaudeBackend {
    fn name(&self) -> &'static str {
        NAME
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
        // allowed_tools 非空才附加（空串语义不明，避免意外禁用全部工具）。
        if !allowed_tools.is_empty() {
            cmd.arg("--allowedTools").arg(allowed_tools.join(","));
        }
        if let Some(s) = session {
            cmd.arg("--resume").arg(&s.0);
        }
        // 权限审批：非 Off 时附加 MCP server（imagent mcp 子命令）；claude 遇需权限工具
        // 时回调 permission_request，由 MCP server 依模式 allow/deny 或经 socket 转 IM 询问。
        let mode = *self.permission_mode.read();
        if mode.is_enabled() {
            let sock = permission_sock_path();
            match write_mcp_config(conv_id, &sock, mode).await {
                Ok(mcp_json) => {
                    cmd.arg("--mcp-config").arg(&mcp_json);
                    cmd.arg("--permission-prompt-tool")
                        .arg(imagent_core::mcp::TOOL_NAME);
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
        }
        spawn_cli_backend(cmd, claude_parse, chunks, NAME).await
    }
}

/// claude stream-json 行 → [`CliEvent`] 适配（见 [`parse_line`]）。
fn claude_parse(line: &str) -> CliEvent {
    match parse_line(line) {
        ParsedEvent::Result {
            text,
            is_error,
            session_id,
        } => {
            if is_error {
                CliEvent::Error {
                    text,
                    session: session_id,
                }
            } else {
                CliEvent::Final {
                    text,
                    session: session_id,
                }
            }
        }
        ParsedEvent::ToolUse {
            tool,
            input,
            session_id,
        } => CliEvent::ToolUse {
            tool,
            input,
            session: session_id,
        },
        ParsedEvent::ToolResult { tool, output } => CliEvent::ToolResult { tool, output },
        ParsedEvent::Other { session_id } => session_id.map_or(CliEvent::Skip, CliEvent::Session),
        ParsedEvent::Skip => CliEvent::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_stable() {
        let b = ClaudeBackend::new();
        assert_eq!(b.name(), "claude-cli");
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
