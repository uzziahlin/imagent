//! peer 通道命令（v1.41 网关伪装 Claude Code peer）：`/peers` 查看在线窗口、
//! `/to` 跨平台派活（任意会话可用——飞书/终端用户经这两个命令完成全部 peer
//! 交互；驾驶舱用户亦可直接进 `peer:<窗口名>` 会话打字）。

use super::*;
use crate::peer::PeerEntry;

impl Dispatcher {
    /// `/peers` —— 列出在线的 Claude Code 窗口（socket 探活过滤 + 只认交互窗口
    /// 条目 `PeerEntry::is_window`——网关自身与无头会话都不在内，v1.43 起；
    /// `startedAt` 倒序）。一行一个：名字 + kind + status，尾附用法提示。
    /// 未启用 / 无窗口给可读文案，不报错。
    pub(super) async fn cmd_peers(&self, conv: &ConvId, hint: &ReplyHint) {
        let Some(peer) = self.peer.read().clone() else {
            self.reply(
                conv,
                "peer 通道未启用（config peer_enabled = false）：网关未注册为 \
                 Claude Code peer，无本地窗口通道。",
                hint,
            )
            .await;
            return;
        };
        let rows: Vec<PeerEntry> = peer.list_windows().await;
        if rows.is_empty() {
            self.reply(conv, "当前没有在线的 Claude Code 窗口。", hint)
                .await;
            return;
        }
        let mut text = String::from("在线的 Claude Code 窗口：\n");
        for r in &rows {
            text.push_str(&format!(
                "- {}（{} · {}）\n",
                r.name,
                r.kind.as_deref().unwrap_or("unknown"),
                r.status
            ));
        }
        text.push_str("首次回复某窗口请先 /to <名字> [文本]（无文本 = 仅建立会话），该窗口会话即出现在侧边栏，之后直接对话。");
        self.reply(conv, &text, hint).await;
    }

    /// `/to <窗口名> [文本…]` —— 跨平台派活入口：带正文按窗口名投递到本地
    /// 窗口，回执落当前会话；**无正文 = 仅建立会话**（只解析窗口名不投递，
    /// 向 `peer:<窗口名>` 会话落一条系统欢迎文本——侧边栏即出现该会话）。
    /// （驾驶舱用户建议直接进 peer: 会话；飞书/终端用户用 /to。）
    pub(super) async fn cmd_to(&self, conv: &ConvId, hint: &ReplyHint, parts: &[&str]) {
        let Some(name) = parts.get(1).map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            self.reply(
                conv,
                "用法：/to <窗口名> [文本…]（无文本 = 仅建立会话；/peers 查看在线窗口）",
                hint,
            )
            .await;
            return;
        };
        let text = parts[2..].join(" ");
        if text.trim().is_empty() {
            // 无正文：建立会话语义（peer 会话在首次使用前不在侧边栏，用户无处
            // 「进入」——显式给一条 send_text 让会话落 outbound_log 出现）。
            self.peer_establish_session(conv, hint, name).await;
            return;
        }
        self.peer_deliver_and_report(conv, hint, name, &text).await;
    }
}
