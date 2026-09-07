//! [`CodexBackend`]：基于 OpenAI Codex CLI 的无状态 agent 执行器。
//!
//! 与 [`imagent_claude::ClaudeBackend`](../../imagent_claude/backend/struct.ClaudeBackend.html)
//! 同构：spawn `codex exec --json` 子进程，逐行解析 JSONL，捕获 `thread_id`
//! 作为 session id，流式推送 `AgentChunk`，返回 `RunOutcome`。

use async_trait::async_trait;
use imagent_core::{
    backend_common::{spawn_cli_backend, CliEvent, WRITE_OR_EXEC},
    AgentChunk, Backend, LocalSession, PermissionCapability, Result, RunOutcome, SessionId,
};
use tokio::process::Command;
use tracing::{debug, warn};

use crate::stream::{parse_line, ParsedEvent};

/// OpenAI Codex CLI 后端。
///
/// MVP 无状态、不做 IM 权限审批闭环（Codex 自身有沙箱模型兜底）。
pub struct CodexBackend {
    /// v1.21 /model：运行时模型覆盖（`codex exec -m <model>`；None = CLI 默认）。
    /// std RwLock——临界区纯读写无 await，trait 的 &self 接口下最小改造。
    model: std::sync::RwLock<Option<String>>,
}

impl CodexBackend {
    pub fn new() -> Self {
        Self {
            model: std::sync::RwLock::new(None),
        }
    }
}

impl Default for CodexBackend {
    fn default() -> Self {
        Self::new()
    }
}

const NAME: &str = "codex";

#[async_trait]
impl Backend for CodexBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    /// B3：Unsupported——依据见文件底部注释（codex exec 无 --ask-for-approval；
    /// bypass 参数拒绝映射）。trait 默认即 Unsupported，此处显式覆写留注释锚点。
    fn permission_capability(&self) -> PermissionCapability {
        PermissionCapability::Unsupported
    }

    /// v1.21 /model：codex exec 原生 `-m` 档位（gpt-5-codex 等）。
    fn supports_model_selection(&self) -> bool {
        true
    }

    fn set_model(&self, model: Option<String>) {
        *self.model.write().unwrap_or_else(|e| e.into_inner()) = model;
    }

    fn model(&self) -> Option<String> {
        self.model.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// P5：扫 `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`（session_meta 的
    /// id + cwd 判定归属），统一 /resume 可接管本机 codex 会话。
    async fn list_local_sessions(&self, workdir: &std::path::Path) -> Vec<LocalSession> {
        // P5-第五批：扫描是同步阻塞 IO（目录遍历 + 头部读），下放 blocking 池
        // 防 /resume 卡 tokio worker。
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
        chunks: tokio::sync::mpsc::Sender<AgentChunk>,
        _initial_todos: &[imagent_core::TodoItem],
        _steer: tokio::sync::mpsc::Receiver<String>,
    ) -> Result<RunOutcome> {
        debug!(target: "imagent::codex", conv_id, "codex run start");
        // B11 幽灵会话预检（参照 claude 的 session_exists 毒化防护）：失败轮次的
        // stream 事件仍可能携带 thread_id，落库后 resume 必然失败且每轮再产新
        // 幽灵 id。resume 前校验 thread id 在本机 rollout 存储中真实存在，
        // 不存在则弃用续接、按新会话处理。目录扫描是同步阻塞 IO，下放
        // blocking 池；无法判定存储根（如无 HOME）时保守放行，不误伤正常续接。
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
                    warn!(target: "imagent::codex", conv_id, thread_id = %s.0, "幽灵会话预检：thread id 不在本机存储，弃用续接开新会话");
                    None
                }
            }
            None => None,
        };
        // 构造命令（workdir 锁定，prompt 单 arg 不经 shell；stdin/stdout/stderr/kill_on_drop
        // 由 spawn_cli_backend 统一加）。
        let sandbox_mode = pick_sandbox(allowed_tools);
        // P1-1：-s 必须在 `--` 之前——clap 的 `--` 之后全是 positional，codex exec
        // 用 trailing_var_arg 收集 prompt，原实现把 -s 放在 -- 之后导致 sandbox 模式
        // 被并入 prompt 字符串、从未生效（退到默认 read-only，用户配 Edit/Write 仍写不了）。
        let model = self.model.read().unwrap_or_else(|e| e.into_inner()).clone();
        let args = codex_args(session, sandbox_mode, model.as_deref(), prompt);
        let mut cmd = Command::new("codex");
        cmd.current_dir(workdir);
        cmd.args(args);
        spawn_cli_backend(
            cmd,
            codex_parse,
            chunks,
            NAME,
            // S-2：仅透传 codex(OpenAI) 所需凭据/端点（最小授权）。
            &["OPENAI_API_KEY", "OPENAI_BASE_URL"],
            None,
            // codex 无 Task\* 工具族：不播种。
            Vec::new(),
            None,
        )
        .await
    }
}

/// codex JSONL 行 → [`CliEvent`] 适配（见 [`parse_line`]）。
fn codex_parse(line: &str) -> CliEvent {
    match parse_line(line) {
        ParsedEvent::ThreadStarted { thread_id } => CliEvent::Session(thread_id),
        ParsedEvent::Other { thread_id } => thread_id.map_or(CliEvent::Skip, CliEvent::Session),
        ParsedEvent::AgentMessage { text } => CliEvent::Text(text),
        ParsedEvent::ToolUse { tool, input } => CliEvent::ToolUse {
            tool,
            input,
            session: None,
            id: None,
        },
        ParsedEvent::ToolResult { tool, output } => CliEvent::ToolResult {
            tool,
            output,
            id: None,
        },
        ParsedEvent::TurnCompleted { usage } => {
            // usage 须在 Terminal 之前——读取循环在 Terminal 处 break。
            match usage {
                Some(u) => CliEvent::Multi(vec![
                    CliEvent::Usage(u),
                    CliEvent::Terminal { session: None },
                ]),
                None => CliEvent::Terminal { session: None },
            }
        }
        ParsedEvent::TurnFailed { message } => CliEvent::Error {
            text: message,
            session: None,
        },
        // B10：顶层 error 不再吞成 Skip——转 TransientError：不中断流（保留「可能
        // 瞬时重连」的原考量），但内容会被记录；若最终无任何 final 文本（如
        // 「API key invalid」这类致命错误），作为失败原因在 IM 可见。
        ParsedEvent::Error { message } => CliEvent::TransientError(message),
        ParsedEvent::TodoList { items } => CliEvent::TodoList { items },
        ParsedEvent::Skip => CliEvent::Skip,
    }
}

/// 把 imagent 的 `allowed_tools` 收敛到 codex 的沙箱模式。
///
/// imagent 的 `allowed_tools`（如 `["Read","Edit"]`）与 codex 的沙箱模型
/// 非一一对应：codex 沙箱只有 `read-only` / `workspace-write` /
/// `danger-full-access` 三档。此处 best-effort 收敛：
/// - 不限制（空/`["*"]`，缺省即全量）或含写/执行类工具 → `workspace-write`；
/// - 否则 → `read-only`。
///
/// **绝不**自动选 `danger-full-access`。
fn pick_sandbox(allowed_tools: &[String]) -> &'static str {
    // tools_unrestricted / WRITE_OR_EXEC 见 imagent_core::backend_common。
    let needs_write = imagent_core::backend_common::tools_unrestricted(allowed_tools)
        || allowed_tools
            .iter()
            .any(|t| WRITE_OR_EXEC.contains(&t.as_str()));
    if needs_write {
        "workspace-write"
    } else {
        "read-only"
    }
}

/// 构造 `codex exec` 的 arg 序列（不含 program name），抽成纯函数便于单测顺序。
///
/// - resume 续接：`exec resume <thread_id> <opts> -- <prompt>`；
/// - 新建：`exec <opts> -- <prompt>`。
///
/// **P1-1**：`-s <sandbox>` 必须在 `--` **之前**（options 区）；`--` 之后只留 prompt
/// 作纯 positional（P2-J：防 prompt 以 `-` 开头被误解析为 flag）。
fn codex_args(
    session: Option<&SessionId>,
    sandbox_mode: &str,
    model: Option<&str>,
    prompt: &str,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["exec".into()];
    if let Some(s) = session {
        args.push("resume".into());
        args.push(s.0.clone());
    }
    args.push("--json".into());
    args.push("--skip-git-repo-check".into());
    // v1.21 /model：-m 与 -s 同在 options 区（`--` 之前）。
    if let Some(m) = model {
        args.push("-m".into());
        args.push(m.into());
    }
    args.push("-s".into());
    args.push(sandbox_mode.into());
    args.push("--".into());
    args.push(prompt.into());
    args
}

// TODO(P?): Codex IM 权限审批闭环——当前仅依赖 codex 沙箱 + workdir 锁定兜底，
// 未实现类似 ClaudeBackend 的 MCP 回调式 IM 审批。codex exec 暂无等价的
// --permission-prompt-tool 机制。
//
// B3 能力声明（Unsupported 的依据，2026-08 真机 `codex exec --help` 核实）：
// - codex exec（非交互 json 模式）**没有** `--ask-for-approval` / `-a` 类原生
//   approval 档位参数——交互式审批仅在 TUI 会话里可用；
// - 唯一的审批相关参数是 `--dangerously-bypass-approvals-and-sandbox`（跳过
//   全部审批 + 解除沙箱），把任何档位映射到它都等于自动放开沙箱，拒绝映射；
// - 因此 codex 既无 IM 闭环也无原生 approval 档位可透传（沙箱 `-s` 三档是
//   allowed_tools 的既有收敛路径，与权限审批档位语义不同），如实声明
//   `Unsupported`。`backend_permission_mode` 在 codex 下保持忽略 + warn
//   （见 main 的能力矩阵 warn）。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_read_only_by_default() {
        // 2026-08 缺省语义：空/["*"] = 不限制 → workspace-write（仍绝不 danger-full-access）。
        assert_eq!(pick_sandbox(&[]), "workspace-write");
        assert_eq!(pick_sandbox(&["*".into()]), "workspace-write");
        assert_eq!(pick_sandbox(&["Read".into(), "Grep".into()]), "read-only");
    }

    #[test]
    fn sandbox_workspace_write_when_edit_present() {
        assert_eq!(
            pick_sandbox(&["Read".into(), "Edit".into()]),
            "workspace-write"
        );
        assert_eq!(pick_sandbox(&["Bash".into()]), "workspace-write");
        assert_eq!(pick_sandbox(&["MultiEdit".into()]), "workspace-write");
    }

    /// B3：能力协商——codex exec 模式无 --ask-for-approval，如实声明 Unsupported
    ///（dispatcher 启动期据此对 ask/auto-claude 档 fail-closed）。
    #[test]
    fn permission_capability_is_unsupported() {
        assert_eq!(
            CodexBackend::new().permission_capability(),
            PermissionCapability::Unsupported
        );
    }

    #[test]
    fn name_is_codex() {
        assert_eq!(CodexBackend::new().name(), "codex");
    }

    /// B10：顶层 error 事件映射为 TransientError（可观测），不再吞成 Skip。
    #[test]
    fn top_level_error_maps_to_transient_error() {
        match codex_parse(r#"{"type":"error","message":"API key invalid"}"#) {
            CliEvent::TransientError(t) => assert_eq!(t, "API key invalid"),
            other => panic!("期望 TransientError，得到 {other:?}"),
        }
    }

    #[test]
    fn sandbox_flag_precedes_dashdash_new_session() {
        // P1-1：-s 必须在 -- 之前，否则被 codex 当 positional 并入 prompt。
        let args = codex_args(None, "workspace-write", None, "hello");
        let dashdash = args.iter().position(|a| a == "--").unwrap();
        let s_idx = args.iter().position(|a| a == "-s").unwrap();
        assert!(
            s_idx < dashdash,
            "-s must precede -- (got -s@{s_idx}, --@{dashdash})"
        );
        assert_eq!(args[s_idx + 1], "workspace-write");
        assert_eq!(args[dashdash + 1], "hello");
    }

    #[test]
    fn sandbox_flag_precedes_dashdash_resume() {
        let sid = SessionId("thread-123".into());
        let args = codex_args(Some(&sid), "read-only", None, "do thing");
        let dashdash = args.iter().position(|a| a == "--").unwrap();
        let s_idx = args.iter().position(|a| a == "-s").unwrap();
        assert!(s_idx < dashdash);
        // resume 分支：exec resume <thread_id>
        let resume_idx = args.iter().position(|a| a == "resume").unwrap();
        assert_eq!(args[resume_idx + 1], "thread-123");
        assert_eq!(args[dashdash + 1], "do thing");
    }

    /// v1.21 /model：-m 注入且在 options 区（`--` 之前）；None 时不加。
    #[test]
    fn model_flag_in_options_region() {
        let args = codex_args(None, "read-only", Some("gpt-5-codex"), "hi");
        let dashdash = args.iter().position(|a| a == "--").unwrap();
        let m_idx = args.iter().position(|a| a == "-m").unwrap();
        assert!(m_idx < dashdash, "-m 必须在 -- 之前");
        assert_eq!(args[m_idx + 1], "gpt-5-codex");
        assert!(
            !codex_args(None, "read-only", None, "hi").contains(&"-m".to_string()),
            "None 模型不应加 -m"
        );
    }

    /// v1.21：todo_list 事件 → TodoList 面板（此前归 Other 丢弃）。
    #[test]
    fn todo_list_event_maps_to_todo_panel() {
        let line = r#"{"type":"item.completed","item":{"id":"t1","type":"todo_list","items":[
            {"id":"1","text":"扫描代码","status":"completed"},
            {"id":"2","text":"修复缺陷","status":"in_progress"},
            {"id":"3","text":"回归测试"}
        ]}}"#;
        match codex_parse(line) {
            CliEvent::TodoList { items } => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0].status, imagent_core::TodoStatus::Completed);
                assert_eq!(items[1].status, imagent_core::TodoStatus::InProgress);
                assert_eq!(items[2].status, imagent_core::TodoStatus::Pending);
            }
            other => panic!("期望 TodoList，得到 {other:?}"),
        }
        // items 缺失/为空 → Other（宁可无面板不可错面板；thread_id 缺席 →
        // codex_parse 把 Other 映射为 Skip）。
        let bad = r#"{"type":"item.completed","item":{"id":"t1","type":"todo_list"}}"#;
        assert!(matches!(codex_parse(bad), CliEvent::Skip));
    }
}
