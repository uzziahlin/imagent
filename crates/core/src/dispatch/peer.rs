//! peer 会话路由（v1.41 网关伪装 Claude Code peer）：
//! - 出站（驾驶舱 → 窗口）：conv 前缀 `peer:` 的文本**不进 agent backend**，
//!   按窗口名解析注册表 → envelope user 帧投递；先经既有出站链路
//!   落 outbound_log（历史重放/未读/推送全复用），投递失败回 SPA error 帧
//!   （`peer_unreachable`）。落盘走 [`Platform::send_user_text`]（v1.41.8）
//!   ——这是**用户自己发出的文本**，驾驶舱按用户气泡渲染并与发送端本机
//!   回显去重（此前走 `send_text` 发普通 text 帧，被渲染成 agent 气泡：
//!   「网关复读」与「双显」）；
//! - 入站（窗口 → 驾驶舱）：[`PeerServer`](crate::peer::PeerServer) 收帧回调
//!   直投 `peer:<窗口名>` 会话（走 `send_text` 链路——窗口 claude 的回复是
//!   agent 文本；**不进** `handle()` 鉴权管线——socket 目录 0700 + sock 0600
//!   即本机凭证，与 `local` 平台同信任边界）。投递后调
//!   [`Platform::on_peer_reply`](crate::Platform::on_peer_reply) 通知钩子
//!   （v1.42：web 覆写为 push_notify kind=peer_reply——纯文本帧无卡片终态，
//!   不加钩子窗口回信永远不会推送，出门等回信手机不响）。

use super::*;
use crate::peer::{CONV_PREFIX, ERR_UNREACHABLE};

impl Dispatcher {
    /// `peer:` 会话文本的出站处理（handle() 的命令分派之后、agent 批处理之前
    /// 拦截——文本永不进 backend）。纯媒体不支持（窗口通道 MVP 只有文本）。
    pub(super) async fn dispatch_peer_message(&self, msg: InboundMessage) {
        let conv = msg.conv_id.clone();
        let hint = msg.reply_hint.clone();
        let Some(name) = conv
            .0
            .strip_prefix(CONV_PREFIX)
            .map(str::trim)
            .filter(|n| !n.is_empty())
        else {
            debug!(target: "imagent::core", conv_id = %conv.0, "peer 会话名缺失，忽略");
            return;
        };
        let Some(text) = msg.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) else {
            debug!(target: "imagent::core", conv_id = %conv.0, "peer 会话纯媒体消息不支持，忽略");
            return;
        };
        let Some(peer) = self.peer.read().clone() else {
            // sender 未注入（peer_enabled = false 或启动失败）——历史残留会话
            // 给可读提示，不静默吞。
            self.reply(
                &conv,
                "peer 通道未启用（config peer_enabled = false），消息未投递。",
                &hint,
            )
            .await;
            return;
        };
        // ① 既有出站链路：广播驾驶舱 + 落 outbound_log（历史重放/未读/推送
        //    全复用）。v1.41.8：走 send_user_text——这是用户自己发出的文本，
        //    驾驶舱按用户气泡渲染并与发送端本机回显去重（落库与其他设备
        //    可见不变）。
        self.reply_user_text(&conv, text, &hint).await;
        // ② peer 出站投递；失败回 SPA error 帧（peer_unreachable toast）。
        if let Err(e) = peer.send_to_window(name, text).await {
            warn!(
                target: "imagent::core",
                conv_id = %conv.0,
                window = name,
                error = %e,
                "peer 出站投递失败"
            );
            self.platform
                .send_error_note(
                    &conv,
                    ERR_UNREACHABLE,
                    &format!("窗口「{name}」不在线或已退出，消息未送达"),
                )
                .await;
        }
    }

    /// PeerServer 入站回调（main 装配传入）：窗口 user 帧正文直投驾驶舱
    /// `peer:<窗口名>` 会话。正文即 envelope 剥离结果（conv 本身已标识窗口，
    /// 不加前缀——任务口径）。窗口名已由 server 三级解析（注册表反查 →
    /// envelope from-name → sock 文件名）。
    ///
    /// v1.42：投递后调 [`Platform::on_peer_reply`]（**唯一调用点**）——通知型
    /// 钩子，IM 平台 default no-op，web 覆写为 push_notify kind=peer_reply。
    /// `/to` 回执 / 建立会话欢迎文本 / 用户自发文本镜像都不触发（不是窗口
    /// 的回信）。`body` 原样透传（title 截断在平台侧）。
    pub async fn handle_peer_inbound(&self, window: &str, body: &str) {
        let conv = ConvId(format!("{CONV_PREFIX}{window}"));
        self.reply(&conv, body, &ReplyHint::None).await;
        self.platform.on_peer_reply(&conv, body).await;
    }

    /// `/to <窗口名>`（无正文）= 仅建立会话：按窗口名解析（复用 sender 的
    /// `resolve_window` 解析路径，只解析不投递正文），成功则向
    /// `peer:<窗口名>` conv send_text 一条系统欢迎文本（走既有 send_text
    /// 链路落 outbound_log——侧边栏即出现该会话，用户可直接进入对话；欢迎
    /// 文本是系统提示而非用户自发文本，**不得**改走 send_user_text）；解析
    /// 失败沿用 `/to` 既有失败文案（含在线名单，可行动）。
    pub(super) async fn peer_establish_session(&self, conv: &ConvId, hint: &ReplyHint, name: &str) {
        let Some(peer) = self.peer.read().clone() else {
            self.reply(
                conv,
                "peer 通道未启用（config peer_enabled = false）。",
                hint,
            )
            .await;
            return;
        };
        match peer.resolve_window(name).await {
            Ok(entry) => {
                // conv 用解析后的规范名（前缀匹配也归一到条目全名，与后续
                // 对话同会话）。
                let peer_conv = ConvId(format!("{CONV_PREFIX}{}", entry.name));
                self.reply(
                    &peer_conv,
                    &format!(
                        "已建立与窗口「{}」的会话（cwd: {}）。直接输入文本即可派活，\
                         窗口空闲时会自动开始处理。",
                        entry.name, entry.cwd
                    ),
                    hint,
                )
                .await;
            }
            Err(e) => {
                warn!(
                    target: "imagent::core",
                    conv_id = %conv.0,
                    window = name,
                    error = %e,
                    "/to 建立会话失败"
                );
                self.reply(conv, &format!("❌ 投递失败：{e}"), hint).await;
            }
        }
    }

    /// `/to <窗口名> <文本…>` 投递核心（命令与复用路径共享）：投递成功 →
    /// 派活正文镜像进 `peer:<窗口名>` 会话（`send_user_text` 落 outbound_log，
    /// 驾驶舱历史完整且显示为用户自己的气泡）+ 当前会话回执（`send_text`，
    /// 系统提示）；失败 → 当前会话回错误（含在线名单，可行动）。
    pub(super) async fn peer_deliver_and_report(
        &self,
        conv: &ConvId,
        hint: &ReplyHint,
        name: &str,
        text: &str,
    ) {
        let Some(peer) = self.peer.read().clone() else {
            self.reply(
                conv,
                "peer 通道未启用（config peer_enabled = false）。",
                hint,
            )
            .await;
            return;
        };
        match peer.send_to_window(name, text).await {
            Ok(()) => {
                // 派活记录进 peer 会话（落 outbound_log——飞书/终端等非驾驶舱
                // 入口发出的派活，驾驶舱侧同样可见因果）。v1.41.8：走
                // send_user_text——正文是用户自己说的，驾驶舱按用户气泡渲染
                // （此前 send_text 把它显示成 imagent 的发言，像网关复读却
                // 没回答）。
                let peer_conv = ConvId(format!("{CONV_PREFIX}{name}"));
                self.reply_user_text(&peer_conv, text, hint).await;
                // 投递回执是系统提示，恒走 send_text（agent 样式）。
                self.reply(conv, &format!("✅ 已投递至 {name}。"), hint)
                    .await;
            }
            Err(e) => {
                warn!(
                    target: "imagent::core",
                    conv_id = %conv.0,
                    window = name,
                    error = %e,
                    "/to 投递失败"
                );
                self.reply(conv, &format!("❌ 投递失败：{e}"), hint).await;
            }
        }
    }
}
