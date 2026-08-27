//! 平台抽象 trait。

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{
    CardButton, ConvId, InboundMessage, JoinedChat, MediaRef, OutboundCard, ReplyHint,
};

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
    /// 「按钮卡片」——用户点击后平台侧产生携带 `ask_req`（request_id）的
    /// InboundMessage，复用既有审批回复路由送达 MCP，core 无需感知按钮形态。
    ///
    /// 返回询问卡的 IM 侧消息 id（无卡片句柄的平台/路径返回 None）——作为
    /// `PermissionRouter` 的 `card_msg_id` 锚点，供自由文本「引用回复」精确路由。
    async fn send_permission_ask(
        &self,
        conv: &ConvId,
        _request_id: &str,
        tool_name: &str,
        input_summary: &str,
        hint: &ReplyHint,
    ) -> Result<Option<String>> {
        self.send_permission_ask_text(conv, tool_name, input_summary, hint)
            .await?;
        Ok(None)
    }

    /// 纯文本审批询问（独立方法而非闭在 send_permission_ask 默认实现里——覆写
    /// send_permission_ask 的平台卡片失败时可调它降级，避免动态分发自递归）。
    /// P8-1：input JSON 压成人可读摘要（同卡片路径），不再裸贴 JSON。
    async fn send_permission_ask_text(
        &self,
        conv: &ConvId,
        tool_name: &str,
        input_summary: &str,
        hint: &ReplyHint,
    ) -> Result<()> {
        let summary = crate::render::tool_summary(tool_name, input_summary);
        let text = format!("🔐 请求执行 {tool_name}：{summary}\n\n回复 y 允许，其它拒绝。");
        self.send_text(conv, &text, hint).await
    }

    /// P5-16：撤回/收敛单个权限询问（超时/顶替时调用，防审批卡滞留可点）。默认
    /// no-op：纯文本询问平台无句柄概念，滞留文本无害。支持交互卡片的平台按
    /// request_id 记录卡片句柄并 patch 成「已中断」终态。
    async fn cancel_permission_ask(&self, _conv: &ConvId, _request_id: &str) -> Result<()> {
        Ok(())
    }

    /// 收敛该 conv 的**全部** pending 询问卡（/stop 中断任务时调用）。默认 no-op；
    /// 多卡并存的平台（飞书）覆写为逐卡收敛。
    async fn cancel_all_permission_asks(&self, _conv: &ConvId) -> Result<()> {
        Ok(())
    }

    /// 真机校准（2026-08 UX）：用户已对询问做出 approve/deny 决策后，把询问卡
    /// patch 成「已批准/已拒绝」终态——否则卡片保持可点、且用户在任务完成前
    /// 得不到任何点击反馈。reply 携带 message（P6：AskUserQuestion 的用户选择），
    /// 问题卡据此显示「已记录你的选择」。默认 no-op（无卡片句柄的平台）。
    async fn resolve_permission_ask(
        &self,
        _conv: &ConvId,
        _request_id: &str,
        _reply: &crate::permission::PermissionReply,
    ) -> Result<()> {
        Ok(())
    }

    /// P9-2：`/config` 偏好设置**表单卡**（下拉 + 提交）。支持表单组件的平台
    /// （飞书 CardKit form）覆写渲染；默认实现降级纯文本（`fallback` 为各平台
    /// 通用的当前值 + 用法说明）。提交回调由平台侧合成 `/config form k=v …`
    /// 命令文本，走与手打命令相同的鉴权/分派。
    async fn send_config_form(
        &self,
        conv: &ConvId,
        _entries: &[crate::types::ConfigFormField],
        fallback: &str,
        hint: &ReplyHint,
    ) -> Result<()> {
        self.send_text(conv, fallback, hint).await
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

    /// P7-A2：bot 已加入的群列表（`/chat allow-all` 批量放行）。`chat_id` 为
    /// **conv 形态**（含平台前缀，如 `feishu:oc_xxx`），可直接入 allowed_chats。
    /// 默认 Err（平台无群概念——ilink / wecom 现状）。
    async fn list_joined_chats(&self) -> Result<Vec<JoinedChat>> {
        Err(crate::error::CoreError::Platform(
            self.name(),
            "该平台不支持列出已加入的群".into(),
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
