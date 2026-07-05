//! 入站消息去重：滑动窗口。
//!
//! 实现已提到 [`imagent_core::Dedup`]（平台无关共享，ilink / wecom 复用）；
//! 此处 re-export 供 ilink 内部沿用 `crate::dedup::Dedup` 路径（platform.rs 无需改动）。

pub use imagent_core::Dedup;
