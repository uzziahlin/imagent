//! imagent-wecom：企业微信智能机器人 WebSocket 长连接 Platform 适配器。
//!
//! 协议出处：官方 SDK `WecomTeam/aibot-node-sdk`（字段已逐字核实，见各模块注释）。
//! 鉴权（白名单）由 core 做，本 crate 不做白名单——只透传 `from.userid`。
//!
//! crate 结构：
//! - [`proto`]：WS 帧的 serde 结构 + 纯函数构造/解析（含单测，无网络）。
//! - [`client`]：`WeComWsClient` 负责 connect / 认证 / 心跳 / 重连 / 收发帧 / ack。
//! - [`platform`]：`WeComPlatform` 实现 [`imagent_core::Platform`]，委托 client。

#![forbid(unsafe_code)]

mod client;
mod platform;
mod proto;

pub use client::probe_credentials;
pub use platform::WeComPlatform;
pub use proto::Credentials;
