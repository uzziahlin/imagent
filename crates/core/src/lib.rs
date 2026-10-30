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

pub mod agent_process;
mod auth;
mod backend;
pub mod backend_common;
pub mod bitable;
mod card_session;
mod config;
pub mod dedup;
pub mod delegate;
mod dispatch;
mod error;
pub mod instance;
pub mod lineio;
pub mod mcp;
mod message;
pub mod metrics;
mod multi_platform;
pub mod outbox;
pub mod paths;
mod permission;
mod platform;
pub mod render;
pub mod sandbox;
pub mod session_scan;
mod types;
pub mod workspace;

pub use auth::Auth;
pub use backend::{Backend, ImPermissionAsk, ImPermissionHook, PermissionCapability};
pub use bitable::{BitableApi, BitableField};
pub use card_session::sweep_live_cards;
pub use config::{
    validate_workdir, Config, CotDetail, DelegateEntry, PermissionMode, QuietHours, ReplyMode,
    WebhookEntry,
};
pub use dedup::Dedup;
pub use delegate::{DelegateApi, DelegateDesc, DelegateRequest};
pub use dispatch::{Dispatcher, TaskBudgets, WebhookExposure};
pub use error::{CoreError, Result};
pub use message::split_message;
pub use metrics::Metrics;
pub use multi_platform::MultiPlatform;
pub use permission::{
    default_sock_path, is_explicit_reply_word, needs_approval, parse_reply, PermissionReply,
    PermissionRouter,
};
pub use platform::{command_card_fallback_text, Platform, PlatformCaps};
pub use sandbox::{resolve as resolve_sandbox, SandboxPlan, SandboxSpec, SandboxTier};
pub use types::{
    AgentChunk, CardButton, CardButtonStyle, CardPhase, CardTerminal, ConfigFormField, ConvId,
    InboundControl, InboundMessage, JoinedChat, LocalSession, MediaRef, Mention, MsgReaction,
    OutboundCard, ReplyHint, RunOutcome, SessionId, TodoItem, TodoStatus, ToolCall, UsageStats,
    UserId, Workdir, CARD_HANDLE_LOST,
};
pub use workspace::{workdir_key, workspace_key, WorkspaceRecord};
