//! `imagent-gemini`：基于 Google Gemini CLI（`gemini --prompt=... -o stream-json`）的 agent 后端。
//!
//! 实现 [`imagent_core::Backend`]：spawn `gemini` 子进程，以 stream-json
//! （每行一个 JSON 对象）格式逐行解析，捕获 Gemini 分配的 `session_id`（即
//! session id），向调用方流式推送 [`imagent_core::AgentChunk`]，最终返回
//! [`imagent_core::RunOutcome`]。
//!
//! 与 [`imagent_codex::CodexBackend`] 同构：spawn + 逐行 JSONL 解析 + session
//! 捕获，差异仅在 CLI 命令与事件结构。对外只暴露 [`GeminiBackend`]。

mod backend;
mod stream;

pub use backend::GeminiBackend;
