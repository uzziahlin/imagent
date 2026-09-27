//! `imagent-feishu`：飞书（Feishu / Lark）长连接 Platform 适配器。
//!
//! 接入 `open-lark` SDK：长连接（WebSocket）收消息、HTTP OpenAPI 发消息、
//! `tenant_access_token` 自动刷新、消息去重。MVP 仅文本。
//!
//! crate 结构：
//! - [`proto`]：飞书 `im.message.receive_v1` 事件 payload 的 serde 结构 + 纯函数
//!   解析（含单测，无网络）。
//! - [`client`]：`FeishuWsClient` 驱动 `open-lark` 长连接（外层重连 loop）；
//!   `send_text_msg` / `fetch_token` 走独立 HTTP。
//! - [`platform`]：[`FeishuPlatform`] 实现 [`imagent_core::Platform`]，spawn
//!   双 task（WS 收事件 + drain 解析入队），recv / send_text。T19 拆分为子模块：
//!   `platform/drain.rs`（WS 事件 drain 循环——单次 Value 解析 + event_type 精确
//!   分派）、`platform/ask.rs`（ask/审批卡生命周期）、`platform/outbox.rs`
//!   （发送侧 outbox 泵）、`platform/state.rs`（per-conv 状态 + 媒体落盘 +
//!   housekeeping）。
//!
//! 鉴权（白名单）由 core 做，本 crate 不做白名单——只透传 sender 的 `open_id`。

#![forbid(unsafe_code)]

mod card;
mod client;
mod metrics;
mod platform;
/// 纯函数解析层（事件 payload → InboundMessage）。pub 供 fuzz target 直接
/// 打外部输入攻击面（fuzz/fuzz_targets/feishu_event_parse.rs）。
pub mod proto;

pub use platform::FeishuPlatform;
