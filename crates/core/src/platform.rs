//! 平台抽象 trait。

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{ConvId, InboundMessage, MediaRef, OutboundCard, ReplyHint};

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
    /// 是否支持流式卡片。dispatch 据此选"卡片 patch"还是"文本多发"。
    /// 默认 false（ilink/wecom 不支持，走原有文本路径）。
    fn supports_streaming_card(&self) -> bool {
        false
    }

    /// 发卡片，返回 message_id（供后续 [`Platform::update_card`] 增量更新）。
    /// 不支持卡片的平台默认降级：把 `card.text` 当文本发送，返回 None。
    async fn send_card(
        &self,
        conv: &ConvId,
        card: &OutboundCard,
        hint: &ReplyHint,
    ) -> Result<Option<String>> {
        self.send_text(conv, &card.text, hint).await?;
        Ok(None)
    }

    /// 增量更新已发卡片。不支持卡片的平台默认 no-op（首条 send_card 已含全文）。
    async fn update_card(
        &self,
        _conv: &ConvId,
        _message_id: &str,
        _card: &OutboundCard,
        _hint: &ReplyHint,
    ) -> Result<()> {
        Ok(())
    }
}
