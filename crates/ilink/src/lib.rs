//! `imagent-ilink`：iLink（OpenClaw Weixin channel）协议的 Rust 实现。
//!
//! 合规定位见 `docs/RESEARCH.md` §1/§2、`docs/DESIGN.md` §6：本 crate 是
//! OpenClaw Weixin channel 协议的重实现（官方 npm 包
//! `@tencent-weixin/openclaw-weixin`、官方文档 developers.weixin.qq.com
//! 的 ClawBot 接口为协议出处）。仅做**服从式退避**——被限流就退避等待，
//! 不绕过；不实现多账号、不破解签名（ClawBot 条款 §4.6 红线）。
//!
//! 实现 `imagent_core::Platform`，依赖 `imagent-core`（trait）+ `imagent-store`
//! （管自己的协议状态：sync_buf 游标 / context_tokens / credentials）。
//!
//! 鉴权由 core 做：adapter 只透传 `from_user_id`，自己**不**做白名单
//! （DESIGN §9 硬约束①）。

mod client;
mod dedup;
mod login;
mod platform;
mod proto;

pub use client::ILinkClient;
pub use login::{login_flow, Credentials};
pub use platform::ILinkPlatform;
