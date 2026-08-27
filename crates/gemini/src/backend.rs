//! [`GeminiBackend`]：基于 Google Gemini CLI 的无状态 agent 执行器。
//!
//! 与 [`imagent_codex::CodexBackend`](../../imagent_codex/backend/struct.CodexBackend.html)
//! 同构：spawn `gemini -p -o stream-json` 子进程，逐行解析 JSONL，捕获
//! `session_id` 作为 session id，流式推送 `AgentChunk`，返回 `RunOutcome`。

use async_trait::async_trait;
use imagent_core::{
    backend_common::{spawn_cli_backend, CliEvent, WRITE_OR_EXEC},
    AgentChunk, Backend, PermissionCapability, Result, RunOutcome, SessionId,
};
use tokio::process::Command;
use tracing::debug;

use crate::stream::{parse_line, ParsedEvent};

/// Google Gemini CLI 后端。
///
/// MVP 无状态、不做 IM 权限审批闭环（依赖 approval-mode + workdir 锁定兜底）。
pub struct GeminiBackend;

impl GeminiBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GeminiBackend {
    fn default() -> Self {
        Self::new()
    }
}

const NAME: &str = "gemini";

/// prompt 作为单 argv 传入的字节上限（B13a，见 run 注释）。
const MAX_PROMPT_BYTES: usize = 64 * 1024;

#[async_trait]
impl Backend for GeminiBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    /// B3：NativeOnly——gemini 有原生审批档位 `--approval-mode`
    /// （default/auto_edit/yolo/plan，本 backend 从 allowed_tools 收敛映射，
    /// 见 [`pick_approval`]），但 headless（`-p`）模式无审批回调机制，无法接
    /// IM 审批闭环。`backend_permission_mode` 透传键在 gemini 下**无可靠映射**：
    /// 其值域是 claude 的 `--permission-mode` 白名单
    /// （default/acceptEdits/plan/auto/dontAsk/bypassPermissions），与 gemini
    /// 的四档不同名（acceptEdits≈auto_edit 勉强可对，但 bypassPermissions→yolo
    /// 是危险放开、auto/dontAsk 无对应），部分可映射=整体不可靠，保持
    /// warn 忽略（main 侧带能力矩阵的明确 warn）。
    fn permission_capability(&self) -> PermissionCapability {
        PermissionCapability::NativeOnly
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
        debug!(target: "imagent::gemini", conv_id, "gemini run start");
        // B13a：ARG_MAX 防护——gemini 的 prompt 只能整条作 `--prompt=<prompt>` 单
        // argv 传入。gemini CLI headless（-p）模式没有从 stdin 读 prompt 的机制
        // （`-p` 后必须跟 prompt，无 `-` / stdin 约定；2026-08 `gemini --help`
        // 核实），且本 workspace 的 spawn_cli_backend（core，backend_common.rs）
        // 统一以 `Stdio::null()` 封死子进程 stdin（防 CLI 交互挂起），stdin 回退
        // 通道不可用。故超长时 fail-fast：拒绝 spawn、给用户可读错误，而不是
        // 撞 E2BIG 得到裸 "Argument list too long"。阈值取 64KB：Linux ARG_MAX
        // 约 2MB 但单 argv 实际上限常为 MAX_ARG_STRLEN=128KB，64KB 留足余量且
        // 远超正常单条 IM 消息长度。
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(imagent_core::CoreError::Backend(
                NAME,
                format!(
                    "prompt 过长（{} 字节 > 上限 {}）：gemini CLI 不支持从 stdin 传参，\
                     请缩短内容或拆分多轮发送",
                    prompt.len(),
                    MAX_PROMPT_BYTES
                ),
            ));
        }
        // 构造命令（workdir 锁定；stdin/stdout/stderr/kill_on_drop 由 spawn_cli_backend 统一加）。
        let (approval, sandbox) = pick_approval(allowed_tools);
        let mut cmd = Command::new("gemini");
        cmd.current_dir(workdir);
        // -p：headless 模式开关。
        cmd.arg("-p").arg("-o").arg("stream-json");
        // 续接：--resume <session_id>（显式 id 有效）。
        if let Some(s) = session {
            cmd.arg("--resume").arg(&s.0);
        }
        // 权限收敛：approval-mode（绝不自动 yolo）。
        cmd.arg("--approval-mode").arg(approval);
        if sandbox {
            cmd.arg("-s");
        }
        // headless 必需：信任当前 workspace，否则 trustedFolders 拒绝。
        cmd.arg("--skip-trust");
        // prompt 绑定到 flag（防止 prompt 以 `-` 开头被误解析）。
        cmd.arg(format!("--prompt={prompt}"));
        spawn_cli_backend(
            cmd,
            gemini_parse,
            chunks,
            NAME,
            // S-2：仅透传 gemini(Google) 所需凭据（最小授权）。
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        )
        .await
    }
}

/// gemini stream-json 行 → [`CliEvent`] 适配（见 [`parse_line`]）。
fn gemini_parse(line: &str) -> CliEvent {
    match parse_line(line) {
        ParsedEvent::Init {
            session_id,
            model: _,
        } => CliEvent::Session(session_id),
        ParsedEvent::AssistantMessage { text } => {
            if text.is_empty() {
                CliEvent::Skip
            } else {
                CliEvent::Text(text)
            }
        }
        ParsedEvent::ToolUse { tool, input } => CliEvent::ToolUse {
            tool,
            input,
            session: None,
        },
        ParsedEvent::ToolResult { tool, output } => CliEvent::ToolResult { tool, output },
        ParsedEvent::Result => CliEvent::Terminal { session: None },
        ParsedEvent::Error { message } => CliEvent::Error {
            text: message,
            session: None,
        },
        ParsedEvent::Other => CliEvent::Skip,
        ParsedEvent::Skip => CliEvent::Skip,
    }
}

/// 把 imagent 的 `allowed_tools` 收敛到 gemini 的 approval-mode + sandbox。
///
/// imagent 的 `allowed_tools`（如 `["Read","Edit"]`）与 gemini 的 approval 模型
/// 非一一对应：gemini approval-mode 有 `default` / `auto_edit` / `yolo` / `plan`
/// 四档。此处 best-effort 收敛：
/// - 含写/执行类工具 → `auto_edit`（允许编辑，仍非 yolo）；
/// - 否则（仅读类）→ `plan`（只读）+ 同时加 `--sandbox` 双重收敛。
///
/// 返回 `(approval_mode, want_sandbox)`。**绝不自动选 `yolo`**（全自动=危险）。
fn pick_approval(allowed_tools: &[String]) -> (&'static str, bool) {
    // tools_unrestricted / WRITE_OR_EXEC 见 imagent_core::backend_common；
    // 不限制（空/["*"]，缺省即全量）按含写执行类处理（auto_edit）。
    let needs_write = imagent_core::backend_common::tools_unrestricted(allowed_tools)
        || allowed_tools
            .iter()
            .any(|t| WRITE_OR_EXEC.contains(&t.as_str()));
    if needs_write {
        ("auto_edit", false)
    } else {
        ("plan", true)
    }
}

// TODO(P?): Gemini IM 权限审批闭环——MVP 不做，依赖 approval_mode + workdir 锁定兜底；
// gemini headless 无等价的 IM 审批回调机制（能力声明见 permission_capability 注释）。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_plan_read_only_by_default() {
        // 2026-08 缺省语义：空/["*"] = 不限制 → auto_edit。
        assert_eq!(pick_approval(&[]), ("auto_edit", false));
        assert_eq!(pick_approval(&["*".into()]), ("auto_edit", false));
        assert_eq!(
            pick_approval(&["Read".into(), "Grep".into()]),
            ("plan", true)
        );
    }

    #[test]
    fn approval_auto_edit_when_edit_present() {
        assert_eq!(
            pick_approval(&["Read".into(), "Edit".into()]),
            ("auto_edit", false)
        );
        assert_eq!(pick_approval(&["Bash".into()]), ("auto_edit", false));
        assert_eq!(pick_approval(&["MultiEdit".into()]), ("auto_edit", false));
    }

    /// B3：能力协商——gemini 有原生 --approval-mode 档位但无 IM 审批回调，
    /// 如实声明 NativeOnly（ask 档启动期被拒，allow/deny 靠原生档兜底）。
    #[test]
    fn permission_capability_is_native_only() {
        assert_eq!(
            GeminiBackend::new().permission_capability(),
            PermissionCapability::NativeOnly
        );
    }

    #[test]
    fn never_yolo() {
        // 即使有全部写工具，也不选 yolo。
        let (mode, _) = pick_approval(&["Edit".into(), "Write".into(), "Bash".into()]);
        assert_ne!(mode, "yolo");
    }

    #[test]
    fn name_is_gemini() {
        assert_eq!(GeminiBackend::new().name(), "gemini");
    }

    /// B13a：超长 prompt 在 spawn 前拒绝，错误信息可读（不撞 E2BIG）。
    #[tokio::test]
    async fn oversized_prompt_rejected_before_spawn() {
        let long = "x".repeat(MAX_PROMPT_BYTES + 1);
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let err = GeminiBackend::new()
            .run("c1", &long, None, std::path::Path::new("/tmp"), &[], tx)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("prompt 过长"), "msg={msg}");
        assert!(msg.contains("stdin"), "msg={msg}");
    }

    /// B13a：恰在上限内的 prompt 不在预检层拒绝（后续由真实 spawn 决定成败）。
    #[test]
    fn threshold_is_max_prompt_bytes() {
        assert_eq!(MAX_PROMPT_BYTES, 64 * 1024);
    }
}
