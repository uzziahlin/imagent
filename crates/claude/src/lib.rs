//! `imagent-claude`：基于 Claude Code CLI（`claude -p`）的 agent 后端。
//!
//! 实现 `imagent_core::Backend`：spawn `claude` 子进程，以 `stream-json`
//! 输出格式逐行解析，捕获 Claude 分配的 `session_id`，向调用方流式推送
//! `AgentChunk`，最终返回 `RunOutcome`。
//!
//! 对外只暴露 [`ClaudeBackend`]。

mod acp;
mod backend;
mod stream;

pub use acp::AcpBackend;
pub use backend::ClaudeBackend;
