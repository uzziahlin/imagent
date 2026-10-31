//! `imagent-codex`：基于 OpenAI Codex CLI 的 agent 后端。
//!
//! 两条执行线：
//! - [`CodexBackend`]：`codex exec --json` 单发流（无 IM 审批闭环、无 steering）；
//! - [`CodexAppBackend`]：`codex app-server` 双工协议（JSON-RPC over stdio，
//!   审批闭环 FullLoop + steering + 模型目录，批 1——dispatcher 装配接线属批 2）。
//!
//! app-server 协议客户端层在 [`crate::app_server`]（JSON-RPC 信封、请求-响应
//! 关联、通知/服务端请求双通道分发），双工协议的真机信封样本与 schema 差异点
//! 见该模块注释。

#![forbid(unsafe_code)]

mod app_backend;
mod app_server;
mod backend;
mod sessions;
mod stream;

pub use app_backend::CodexAppBackend;
pub use app_server::{AppServerProcess, IncomingNotification, IncomingRequest, RpcReply};
pub use backend::CodexBackend;
