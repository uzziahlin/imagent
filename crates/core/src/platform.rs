//! 平台抽象 trait。

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{CardButton, ConvId, InboundMessage, MediaRef, OutboundCard, ReplyHint};

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
    /// 该会话是否支持流式卡片。dispatch 据此选"卡片 patch"还是"文本多发"。
    /// 默认 false（ilink/wecom 不支持，走原有文本路径）。per-conv：飞书评论线程
    /// 只能回评论（无卡片语义），返回 false 走纯文本流（P4-9）。
    fn supports_streaming_card(&self, _conv: &ConvId) -> bool {
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

    /// 强制平台重连（P4-7 `/reconnect`）：断开当前长连接并立即重连，排查僵死连接。
    /// 默认不支持（返回 Err 由命令层提示）。
    async fn reconnect(&self) -> Result<()> {
        Err(crate::error::CoreError::Platform(
            "platform",
            "该平台不支持强制重连".into(),
        ))
    }

    /// 发权限审批询问（P4-4）。默认实现走纯文本；支持交互卡片的平台可覆写为
    /// 「按钮卡片」——用户点击后平台侧产生 `text = "y"/"n"` 的 InboundMessage，
    /// 复用既有审批回复路由（`parse_reply`）送达 MCP，core 无需感知按钮。
    async fn send_permission_ask(
        &self,
        conv: &ConvId,
        tool_name: &str,
        input_summary: &str,
        hint: &ReplyHint,
    ) -> Result<()> {
        self.send_permission_ask_text(conv, tool_name, input_summary, hint)
            .await
    }

    /// 纯文本审批询问（独立方法而非闭在 send_permission_ask 默认实现里——覆写
    /// send_permission_ask 的平台卡片失败时可调它降级，避免动态分发自递归）。
    async fn send_permission_ask_text(
        &self,
        conv: &ConvId,
        tool_name: &str,
        input_summary: &str,
        hint: &ReplyHint,
    ) -> Result<()> {
        let text =
            format!("🔐 Claude 请求执行 {tool_name}：{input_summary}\n回复 y 允许，其它拒绝。");
        self.send_text(conv, &text, hint).await
    }

    /// P5-16：撤回/收敛该 conv 最近一次权限询问（`/stop` 中断任务时调用，防审批
    /// 卡片滞留可点、用户对一个已死的任务做审批）。默认 no-op：纯文本询问平台
    /// 无句柄概念，滞留文本无害（其后的 y/n 因 pending 已清走正常处理路径）。
    /// 支持交互卡片的平台应记录最近询问的卡片句柄并在此 patch 成「已中断」终态。
    async fn cancel_permission_ask(&self, _conv: &ConvId) -> Result<()> {
        Ok(())
    }

    /// 真机校准（2026-08 UX）：用户已对询问做出 approve/deny 决策后，把询问卡
    /// patch 成「已批准/已拒绝」终态——否则卡片保持可点、且用户在任务完成前
    /// 得不到任何点击反馈。默认 no-op（无卡片句柄的平台）。
    async fn resolve_permission_ask(&self, _conv: &ConvId, _allowed: bool) -> Result<()> {
        Ok(())
    }

    /// 发命令交互卡片（P6-3）：markdown 正文 + 按钮组。按钮点击由平台侧转成
    /// `text = <command>` 的 InboundMessage（走与手打命令相同的鉴权/分派）。
    /// 默认降级纯文本：title + body + 可手打的命令清单（无按钮能力的平台无需感知）。
    async fn send_command_card(
        &self,
        conv: &ConvId,
        title: &str,
        body_md: &str,
        buttons: &[CardButton],
        hint: &ReplyHint,
    ) -> Result<()> {
        self.send_text(
            conv,
            &command_card_fallback_text(title, body_md, buttons),
            hint,
        )
        .await
    }

    /// P6 遗留补齐：查询「群消息须 @bot」当前策略（`/config` 展示用）。
    /// 默认 None（平台无群聊 @ 概念或未实现——ilink 无群、wecom 群消息不收）。
    async fn require_mention_in_group(&self) -> Option<bool> {
        None
    }

    /// P6 遗留补齐：热切换「群消息须 @bot」（`/config require_mention on|off`，
    /// 对下一消息生效；进程内不落盘，重启回 config 值）。默认 Err（不支持）。
    async fn set_require_mention_in_group(&self, _on: bool) -> Result<()> {
        Err(crate::error::CoreError::Platform(
            self.name(),
            "该平台不支持 require_mention（无群聊 @ 语义）".into(),
        ))
    }
}

/// 命令卡片的纯文本降级形态（默认 trait 实现与 dispatch 层失败降级共用）：
/// 标题 + 正文 + 「可手打命令」提示（按钮不可用时保底可用性）。
pub fn command_card_fallback_text(title: &str, body_md: &str, buttons: &[CardButton]) -> String {
    let mut text = if title.trim().is_empty() {
        body_md.to_string()
    } else {
        format!("{title}\n{body_md}")
    };
    if !buttons.is_empty() {
        let cmds: Vec<&str> = buttons.iter().map(|b| b.command.as_str()).collect();
        text.push_str(&format!(
            "\n（本会话不支持按钮，可直接发送：{}）",
            cmds.join("、")
        ));
    }
    text
}
