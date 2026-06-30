//! 平台抽象 trait。

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{ConvId, InboundMessage, MediaRef, ReplyHint};

/// IM 平台抽象。由 `ilink` / `wecom` 等适配器实现，注入到 `Dispatcher`。
#[async_trait]
pub trait Platform: Send + Sync {
    /// 阻塞取下一条入站消息（实现内部自管长轮询/重连）。
    async fn recv(&self) -> Result<InboundMessage>;
    async fn send_text(&self, conv: &ConvId, text: &str, hint: &ReplyHint) -> Result<()>;
    async fn send_media(&self, conv: &ConvId, media: &MediaRef, hint: &ReplyHint) -> Result<()>;
    /// 可选：typing 指示。P1 默认空实现。
    async fn send_typing(&self, _conv: &ConvId, _hint: &ReplyHint) -> Result<()> {
        Ok(())
    }
    /// 平台名，如 `"ilink"`。
    fn name(&self) -> &'static str;
}
