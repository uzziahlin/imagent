//! `imagent-core`：调度核心。
//!
//! 定义双抽象 trait（`Platform` 平台、`Backend` agent）、鉴权（`Auth`）、
//! 会话路由与消息调度（`Dispatcher`）、配置加载（`Config`）。
//!
//! core 是契约源头：`ilink` 实现 `Platform`、`claude` 实现 `Backend`，
//! 二者依赖 core；core 通过 `Arc<dyn Platform>` / `Arc<dyn Backend>` 注入实现，
//! 不反向依赖具体 crate（依赖倒置）。

mod auth;
mod backend;
mod config;
mod dispatch;
mod error;
mod platform;
mod types;

pub use auth::Auth;
pub use backend::Backend;
pub use config::Config;
pub use dispatch::Dispatcher;
pub use error::{CoreError, Result};
pub use platform::Platform;
pub use types::{AgentChunk, ConvId, InboundMessage, MediaRef, ReplyHint, RunOutcome, SessionId, UserId, Workdir};
