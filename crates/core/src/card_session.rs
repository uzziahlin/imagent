//! 流式卡片会话：累积 agent 输出，节流 patch 到支持卡片的平台。
//!
//! 仅 `Platform::supports_streaming_card() == true` 的平台使用（dispatch 据此分支）。
//! 累积 `text` / `tool_calls`，按节流间隔 patch：首次 `send_card` 拿 `message_id`，
//! 后续 `update_card`。最终 `finalize` 强制 patch 终态（Done/Error）。
//!
//! 方法均返回 `()`——卡片发送失败在内部 `warn!` 记录（dispatch `handle` 返回 ()，
//! 无法传播卡片错误；卡片失败不应中断 agent 回复）。设计借鉴 lcab 的
//! `RunState + renderCard + update`，但 core 只产平台无关的 [`OutboundCard`]，
//! 卡片 JSON 渲染由各 Platform 实现。

use std::time::{Duration, Instant};

use tracing::warn;

use crate::platform::Platform;
use crate::types::{CardTerminal, ConvId, OutboundCard, ReplyHint};

/// 卡片 patch 节流间隔。飞书交互卡片更新有频率限制，500ms 平衡流畅与限流。
const CARD_THROTTLE: Duration = Duration::from_millis(500);

/// 流式卡片会话。
pub(crate) struct CardSession {
    text: String,
    tools: Vec<(String, String)>,
    msg_id: Option<String>,
    last_patch: Instant,
}

impl CardSession {
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
            tools: Vec::new(),
            msg_id: None,
            last_patch: Instant::now(),
        }
    }

    /// 累积文本增量，节流 patch（Running 态）。
    pub(crate) async fn append_text(
        &mut self,
        text: &str,
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) {
        self.text.push_str(text);
        self.patch_if_due(CardTerminal::Running, conv, hint, platform)
            .await;
    }

    /// 累积工具调用摘要，节流 patch（Running 态；工具块需展示）。
    pub(crate) async fn append_tool(
        &mut self,
        tool: &str,
        input_summary: &str,
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) {
        self.tools
            .push((tool.to_string(), input_summary.to_string()));
        self.patch_if_due(CardTerminal::Running, conv, hint, platform)
            .await;
    }

    /// 最终 patch：用 `final_text` 覆盖累积文本，合并 dispatch 侧累积的 `extra_tools`，
    /// 强制 patch 终态（不受节流，确保 Done/Error 显示）。
    pub(crate) async fn finalize(
        &mut self,
        final_text: Option<&str>,
        extra_tools: &[(String, String)],
        terminal: CardTerminal,
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) {
        if let Some(f) = final_text {
            self.text.clear();
            self.text.push_str(f);
        }
        for t in extra_tools {
            if !self.tools.contains(t) {
                self.tools.push(t.clone());
            }
        }
        // 终态强制 patch（绕过节流），确保用户看到 Done/Error。
        self.dispatch_card(terminal, conv, hint, platform).await;
    }

    /// 节流 patch：首次（无 msg_id）或距上次 patch ≥ THROTTLE 才发；否则跳过。
    async fn patch_if_due(
        &mut self,
        terminal: CardTerminal,
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) {
        let due = self.msg_id.is_none() || self.last_patch.elapsed() >= CARD_THROTTLE;
        if !due {
            return;
        }
        self.dispatch_card(terminal, conv, hint, platform).await;
    }

    /// 实际发送/更新卡片：首次 `send_card` 拿 msg_id，后续 `update_card`；失败 `warn!`。
    async fn dispatch_card(
        &mut self,
        terminal: CardTerminal,
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) {
        let card = OutboundCard {
            text: self.text.clone(),
            tool_calls: self.tools.clone(),
            terminal,
        };
        let res: crate::error::Result<()> = match &self.msg_id {
            None => match platform.send_card(conv, &card, hint).await {
                Ok(id) => {
                    self.msg_id = id;
                    Ok(())
                }
                Err(e) => Err(e),
            },
            Some(mid) => platform.update_card(conv, mid, &card, hint).await,
        };
        if let Err(e) = res {
            warn!(target: "imagent::core", error = %e, "卡片更新失败");
        }
        self.last_patch = Instant::now();
    }
}
