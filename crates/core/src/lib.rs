//! `imagent-core`：调度核心。
//!
//! 定义双抽象 trait（`Platform` 平台、`Backend` agent）、鉴权（`Auth`）、
//! 会话路由与消息调度（`Dispatcher`）、配置加载（`Config`）。
//!
//! core 是契约源头：`ilink` 实现 `Platform`、`claude` 实现 `Backend`，
//! 二者依赖 core；core 通过 `Arc<dyn Platform>` / `Arc<dyn Backend>` 注入实现，
//! 不反向依赖具体 crate（依赖倒置）。

// 注：core 用 `deny`（非 `forbid`）：P0-B 的权限 socket 对端 uid 鉴权需要
// SO_PEERCRED/LOCAL_PEERCRED（必然 unsafe），集中在 `dispatch::socket::current_uid` /
// `dispatch::socket::peer_uid`，两处均 `#[allow(unsafe_code)]` + SAFETY 注释。`deny` 允许
// 这种显式局部豁免；`forbid` 不允许，故不适用。其余全部 crate 用 `forbid`。
#![deny(unsafe_code)]

mod auth;
mod backend;
pub mod backend_common;
mod card_session;
mod config;
pub mod dedup;
mod dispatch;
mod error;
pub mod instance;
pub mod mcp;
mod message;
pub mod metrics;
pub mod paths;
mod permission;
mod platform;
pub mod render;
mod types;

pub use auth::Auth;
pub use backend::{Backend, ImPermissionAsk, ImPermissionHook, PermissionCapability};
pub use card_session::sweep_live_cards;
pub use config::{validate_workdir, Config, CotDetail, PermissionMode, ReplyMode};
pub use dedup::Dedup;
pub use dispatch::{Dispatcher, TaskBudgets};
pub use error::{CoreError, Result};
pub use message::split_message;
pub use metrics::Metrics;
pub use permission::{default_sock_path, parse_reply, PermissionReply, PermissionRouter};
pub use platform::{command_card_fallback_text, Platform};
pub use types::{
    AgentChunk, CardButton, CardButtonStyle, CardPhase, CardTerminal, ConfigFormField, ConvId,
    InboundMessage, JoinedChat, LocalSession, MediaRef, Mention, OutboundCard, ReplyHint,
    RunOutcome, SessionId, ToolCall, UsageStats, UserId, Workdir, CARD_HANDLE_LOST,
};
