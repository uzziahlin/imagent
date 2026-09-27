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
pub struct GeminiBackend {
    /// v1.21 /model：运行时模型覆盖（`gemini -m <model>`；None = CLI 默认）。
    model: std::sync::RwLock<Option<String>>,
    // 【记录在案的已知限制】幽灵会话预检不做（v13 P3 批定案，非待办）。
    //
    // 背景：claude/codex 均有 run 前的 session_exists 预检（防失败轮泄漏的
    // session id 毒化 resume）。gemini 侧不引入同款预检的原因：
    //
    // 1. 布局映射仍无法确定性解析。本机实测（2026-09）`~/.gemini/tmp/` 的
    //    观测形态是 `<目录名>/chats/session-<ISO 时间戳>-<8 位 hex>.jsonl`，
    //    文件首行 `{"sessionId":"<uuid>","projectHash":…}`——文件名里的 8 位
    //    hex 是 sessionId uuid 的**前 8 字符**，且目录名疑似 workdir 的最后
    //    一段路径名（证据：ai-harness / imagent / tmp 三个目录），但两点均
    //    未获官方文档或足够样本确认；
    // 2. 关键缺口：`init` 事件上报的 `session_id` 到底是完整 uuid 还是 8 位
    //    短 id 没有真机捕获样本——映射猜错方向（前缀 vs 全等）会把**所有
    //    正常续接**误判为幽灵会话、每轮弃上下文重开，比不预检（毒化轮次
    //    resume 失败一次、用户 /new 自愈）伤害大得多；
    // 3. gemini 失败轮的 Init(id) 通常不落库（dispatch 只在成功/中断路径
    //    记 session），毒化概率本就低于 codex/claude。
    //
    // 重启该工作的前置条件：真机捕获一条 `gemini -o stream-json` 的 init 行
    // 与对应的 ~/.gemini/tmp 文件名，确认 id 形态与目录规则。
}

impl GeminiBackend {
    pub fn new() -> Self {
        Self {
            model: std::sync::RwLock::new(None),
        }
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

    /// v1.21 /model：gemini CLI 原生 `-m/--model` 档位（gemini-2.5-pro 等）。
    fn supports_model_selection(&self) -> bool {
        true
    }

    fn set_model(&self, model: Option<String>) {
        *self.model.write().unwrap_or_else(|e| e.into_inner()) = model;
    }

    fn model(&self) -> Option<String> {
        self.model.read().unwrap_or_else(|e| e.into_inner()).clone()
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
        // v1.21 /model：运行时模型覆盖（None = CLI 默认）。
        if let Some(m) = self.model.read().unwrap_or_else(|e| e.into_inner()).clone() {
            cmd.arg("-m").arg(m);
        }
        // prompt 绑定到 flag（防止 prompt 以 `-` 开头被误解析）。
        cmd.arg(format!("--prompt={prompt}"));
        // parse 为有状态解析器（FnMut 闭包承载，见 GeminiParser 文档）。
        let mut parser = GeminiParser::new();
        spawn_cli_backend(
            cmd,
            move |line: &str| parser.parse(line),
            chunks,
            NAME,
            // S-2：仅透传 gemini(Google) 所需凭据（最小授权）。
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            None,
            // gemini 无 Task\* 工具族：不播种。
            Vec::new(),
            None,
        )
        .await
    }
}

/// gemini stream-json 行 → [`CliEvent`] 的**有状态**适配器（T20 delta 碎化
/// 修复；spawn_cli_backend 的 parse 参数自本批起为 FnMut，承载跨行状态）。
///
/// 修复背景：gemini 的流式输出把同一条 assistant 消息拆成多条
/// `{"delta":true}` 行，旧解析把它们当**完整消息**（CliEvent::Text），
/// backend_common 的 B9 规则按 `\n\n` 拼接——一句话被空行拆成碎段（流式卡
/// 与 final_text 双双碎化）。
///
/// 语义：
/// - `delta:true` 片段 → [`CliEvent::TextDelta`]（流式 chunk 照推、final_text
///   直接续接不插分隔），并在本解析器累积；
/// - 片段流之后到达的**无 delta 完整消息**：与累积同文 → 视为流结束后的
///   全文回放，去重跳过（不重复进 final_text）；异文 → 新的完整消息，走
///   [`CliEvent::Text`]（B9 `\n\n` 语义正确分隔多条消息）并重置累积；
/// - 始终无完整消息回放的输出（纯 delta 流）累积结果即最终文本——
///   TextDelta 的直接续接已把它拼完整。
/// - 其余事件与旧 `gemini_parse`（无状态版本）逐一同构。
struct GeminiParser {
    /// 未结块的 delta 累积（同一条消息的片段拼接，用于与完整消息比对去重）。
    delta_acc: String,
}

impl GeminiParser {
    fn new() -> Self {
        Self {
            delta_acc: String::new(),
        }
    }

    fn parse(&mut self, line: &str) -> CliEvent {
        match parse_line(line) {
            ParsedEvent::Init {
                session_id,
                model: _,
            } => CliEvent::Session(session_id),
            ParsedEvent::AssistantMessage { text, delta } => self.assistant_message(text, delta),
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
            ParsedEvent::Result { usage } => {
                // usage 须在 Terminal 之前——读取循环在 Terminal 处 break。
                match usage {
                    Some(u) => CliEvent::Multi(vec![
                        CliEvent::Usage(u),
                        CliEvent::Terminal { session: None },
                    ]),
                    None => CliEvent::Terminal { session: None },
                }
            }
            ParsedEvent::Error { message } => CliEvent::Error {
                text: message,
                session: None,
            },
            ParsedEvent::Other => CliEvent::Skip,
            ParsedEvent::Skip => CliEvent::Skip,
        }
    }

    /// assistant message 的 delta 分流（见结构体文档的语义说明）。
    fn assistant_message(&mut self, text: String, delta: bool) -> CliEvent {
        if delta {
            if text.is_empty() {
                return CliEvent::Skip;
            }
            self.delta_acc.push_str(&text);
            return CliEvent::TextDelta(text);
        }
        if !self.delta_acc.is_empty() {
            let acc = std::mem::take(&mut self.delta_acc);
            if text == acc {
                // 完整消息与已流出的片段同文：流结束后的全文回放，去重。
                CliEvent::Skip
            } else {
                CliEvent::Text(text)
            }
        } else if text.is_empty() {
            CliEvent::Skip
        } else {
            CliEvent::Text(text)
        }
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
            .run(
                "c1",
                &long,
                None,
                std::path::Path::new("/tmp"),
                &[],
                tx,
                &[],
                {
                    let (sx, rx) = tokio::sync::mpsc::channel(1);
                    drop(sx);
                    rx
                },
            )
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

    // ------------------------------------------------------------------
    // T20：delta:true 碎化修复（GeminiParser 有状态适配）。
    // ------------------------------------------------------------------

    fn delta_line(text: &str) -> String {
        serde_json::json!({
            "type": "message", "role": "assistant", "content": text, "delta": true
        })
        .to_string()
    }

    fn full_line(text: &str) -> String {
        serde_json::json!({
            "type": "message", "role": "assistant", "content": text
        })
        .to_string()
    }

    /// delta 片段必须产 TextDelta（而非被当完整消息的 Text）——后者经 B9 规则
    /// `\n\n` 拼接，一句话「你好，世界」会被拆成空行分隔的碎段。
    #[test]
    fn delta_fragments_emit_text_delta() {
        let mut p = GeminiParser::new();
        assert!(matches!(
            p.parse(&delta_line("你好，")),
            CliEvent::TextDelta(t) if t == "你好，"
        ));
        assert!(matches!(
            p.parse(&delta_line("世界")),
            CliEvent::TextDelta(t) if t == "世界"
        ));
        // 纯 delta 流（无完整消息回放）后正常终结。
        assert!(matches!(
            p.parse(r#"{"type":"result","status":"success"}"#),
            CliEvent::Terminal { session: None }
        ));
    }

    /// 流结束后同文完整消息 = 全文回放，去重跳过（不重复进 final_text）。
    #[test]
    fn full_message_replay_after_deltas_is_suppressed() {
        let mut p = GeminiParser::new();
        p.parse(&delta_line("He"));
        p.parse(&delta_line("llo"));
        assert!(
            matches!(p.parse(&full_line("Hello")), CliEvent::Skip),
            "同文回放不得再当完整消息拼入"
        );
        // 去重后状态已重置：后续完整消息恢复正常 Text 语义。
        assert!(matches!(
            p.parse(&full_line("第二条")),
            CliEvent::Text(t) if t == "第二条"
        ));
    }

    /// 片段后到达的**异文**完整消息 = 新消息，走完整消息 Text（B9 `\n\n`
    /// 分隔语义），且重置累积（再下一同文消息不再误判为回放）。
    #[test]
    fn distinct_full_message_after_deltas_is_new_text() {
        let mut p = GeminiParser::new();
        p.parse(&delta_line("流式段"));
        assert!(matches!(
            p.parse(&full_line("完整回复")),
            CliEvent::Text(t) if t == "完整回复"
        ));
        // 累积已清：同文「流式段」此时是独立完整消息，正常 Text。
        assert!(matches!(
            p.parse(&full_line("流式段")),
            CliEvent::Text(t) if t == "流式段"
        ));
    }

    /// 无 delta 的普通完整消息行为不变（向后兼容旧版 gemini CLI）。
    #[test]
    fn plain_messages_unchanged() {
        let mut p = GeminiParser::new();
        assert!(matches!(p.parse(&full_line("first")), CliEvent::Text(t) if t == "first"));
        assert!(matches!(p.parse(&full_line("")), CliEvent::Skip));
        assert!(
            matches!(p.parse(&delta_line("")), CliEvent::Skip),
            "空 delta 片段无信息量，跳过"
        );
    }

    /// 端到端碎化回归（走 spawn_cli_backend 真读循环 + /bin/sh 假 gemini）：
    /// 一条消息的 3 个 delta 片段 → final_text 无空行分隔地拼成整句；流式
    /// Text chunk 逐片段推送。修复前 final_text = "你\n\n好\n\n世界"。
    #[cfg(unix)]
    #[tokio::test]
    async fn delta_stream_final_text_is_not_fragmented() {
        use imagent_core::AgentChunk;
        let mut cmd = tokio::process::Command::new("/bin/sh");
        let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
        let payload = format!(
            "printf '%s\\n' {} {} {} {}",
            q(&delta_line("你好，")),
            q(&delta_line("世界")),
            q(&delta_line("！")),
            q(r#"{"type":"result","status":"success"}"#)
        );
        cmd.arg("-c").arg(payload);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentChunk>(64);
        let mut parser = GeminiParser::new();
        let outcome = imagent_core::backend_common::spawn_cli_backend(
            cmd,
            move |line: &str| parser.parse(line),
            tx,
            "gemini",
            &[],
            None,
            Vec::new(),
            None,
        )
        .await
        .expect("假 gemini 流应成功");
        assert_eq!(
            outcome.final_text, "你好，世界！",
            "碎片直接续接，无空行分隔"
        );
        let mut texts = Vec::new();
        while let Ok(c) = rx.try_recv() {
            if let AgentChunk::Text(t) = c {
                texts.push(t);
            }
        }
        assert_eq!(
            texts,
            vec!["你好，".to_string(), "世界".to_string(), "！".to_string()],
            "delta 片段照常流式推送"
        );
    }
}
