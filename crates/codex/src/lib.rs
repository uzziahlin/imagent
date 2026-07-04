//! `imagent-codex`：基于 OpenAI Codex CLI（`codex exec --json`）的 agent 后端。
//!
//! 实现 [`imagent_core::Backend`]：spawn `codex exec` 子进程，以每行一个 JSON
//! 对象的输出格式逐行解析，捕获 Codex 分配的 `thread_id`（即 session id），向
//! 调用方流式推送 [`imagent_core::AgentChunk`]，最终返回
//! [`imagent_core::RunOutcome`]。
//!
//! 与 [`imagent_claude::ClaudeBackend`] 同构：spawn + 逐行 JSONL 解析 + session
//! 捕获，差异仅在 CLI 命令与事件结构。对外只暴露 [`CodexBackend`]。

#![forbid(unsafe_code)]

mod backend;
mod stream;

pub use backend::CodexBackend;
