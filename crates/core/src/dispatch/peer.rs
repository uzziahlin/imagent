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
//! - 钩子上报（v1.43，窗口 → 驾驶舱的旁路）：[`Dispatcher::handle_peer_report`]
//!   ——Claude Code 的 Stop 钩子经 `imagent peer-hook` 把每轮结论送到网关，
//!   按 session 反查窗口、去重后注入同一个 `peer:<窗口名>` 会话，再走
//!   「跑完一轮」推送闸门（[`Platform::on_window_event`]）。
//! - 窗口看板通知（v1.43）：[`Dispatcher::publish_windows_changed`] /
//!   [`Dispatcher::publish_window_event`] 是 peer 轮询任务回调的落点——把
//!   快照与可推送事件转给平台钩子（main 只做接线）。

use super::*;
use crate::peer::{
    normalize_ws, WindowEvent, WindowEventKind, WindowInfo, CONV_PREFIX, ERR_UNREACHABLE,
    HOOK_MESSAGE_MAX_CHARS, HOOK_TRUNCATED_MARK,
};
use std::time::Instant;

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
    ///
    /// v1.43：`reply` 之前先把这条回信登记进 peer 共享态
    /// （[`crate::peer::PeerState::note_inbound`]）——Stop 钩子上报与它是同
    /// 一段话时据此去重。登记在投递前：投递可能带退避重试，而钩子在窗口
    /// 收尾时立刻就到，顺序反了会让第一轮的上报漏判成「不重复」。
    pub async fn handle_peer_inbound(&self, window: &str, body: &str) {
        let conv = ConvId(format!("{CONV_PREFIX}{window}"));
        if let Some(peer) = self.peer.read().clone() {
            peer.state().note_inbound(window, body, Instant::now());
        }
        self.reply(&conv, body, &ReplyHint::None).await;
        self.platform.on_peer_reply(&conv, body).await;
    }

    /// v1.43 Stop 钩子上报（PeerServer 的 hook 帧回调；main 装配传入）。本机
    /// **所有** Claude Code 会话的每一轮结束都会触发钩子——网关自己用
    /// `claude-cli` 引擎跑的每一轮、ACP 子进程、后台作业、别的工具调的
    /// `claude -p`——所以第一道闸是注册表反查：
    ///
    /// 1. **解析窗口**：`sessionId` 相等、是窗口（`entrypoint == "cli"` 且非
    ///    `bg`）、套接字为活。查不到 → debug 后返回（网关自己的会话是
    ///    `sdk-cli`，天然被挡在外面——否则网关每一轮的结论都会被当成窗口结论
    ///    注入驾驶舱、重复推送）。
    /// 2. **去重**：`message` 归一化（trim、空白折叠）后为空 → 不注入会话；
    ///    与该窗口 180 秒内最近一条 peer 回信的归一化正文相同（互为前缀，取
    ///    前 200 字符比较）→ 不注入也不推送（那条回信已经到了、`peer_reply`
    ///    推送已发）。
    /// 3. **注入**：向 `peer:<窗口名>` 会话 `reply`「📋 本轮结束（自动同步）」+
    ///    正文（按字符截到 6000，超出追加截断标记），走既有 `send_text` 链路、
    ///    落 outbound_log。每个窗口 20 秒内最多注入一次（节流窗口内的上报只走
    ///    推送闸门、不注入）。**用普通 `reply`、不调 `on_peer_reply`**——那
    ///    会再发一条 `peer_reply` 推送，同一段结论推两次。
    /// 4. **推送**：`should_push_turn_end`（被关注或对话过，且过了 30 秒节流）
    ///    → [`Platform::on_window_event`]（`TurnEnd`）。节流与轮询判定共用，
    ///    同一轮两条路径至多推一次。
    ///
    /// **绝不向窗口回发任何消息**（上报是单向的，不存在环路）。peer 句柄缺席
    /// （peer 未启用）→ 直接返回。注入正文只 trim 首尾、**保留内部换行与
    /// 缩进**：它是窗口 claude 的最终回复（markdown），驾驶舱按 markdown
    /// 渲染；折叠成单行会把代码块与列表压成一团。归一化只用于第 2 步比较。
    pub async fn handle_peer_report(&self, session_id: &str, message: &str) {
        let Some(peer) = self.peer.read().clone() else {
            return;
        };
        let Some(entry) = peer.registry().find_by_session_id(session_id).await else {
            debug!(
                target: "imagent::core",
                session_id,
                "Stop 钩子上报找不到对应的交互窗口，忽略（无头会话 / 后台作业 / 已退出的窗口）"
            );
            return;
        };
        let state = peer.state().clone();
        let window = entry.name;
        let now = Instant::now();
        // 去重：归一化只用于比较，注入用的是 trim 后的原文（见函数注释）。
        let normalized = normalize_ws(message);
        if !normalized.is_empty() && state.inbound_duplicate(&window, &normalized, now) {
            debug!(
                target: "imagent::core",
                window = %window,
                "Stop 钩子上报与最近的窗口回信重复，忽略（回信与 peer_reply 推送已到）"
            );
            return;
        }
        let conv = ConvId(format!("{CONV_PREFIX}{window}"));
        let body = message.trim();
        if !normalized.is_empty() && state.report_inject_allowed(&window, now) {
            self.reply(&conv, &report_text(body), &ReplyHint::None)
                .await;
            state.note_report_injected(&window, now);
        }
        if state.should_push_turn_end(&window, now) {
            self.platform
                .on_window_event(
                    &conv,
                    &WindowEvent {
                        name: window,
                        kind: WindowEventKind::TurnEnd,
                        waiting_for: None,
                        push: true,
                    },
                )
                .await;
        }
    }

    /// v1.43 窗口快照变更 → [`Platform::on_windows_changed`]（同步钩子；peer
    /// 轮询任务的回调落点——main 只做接线，平台句柄不出 Dispatcher）。
    pub fn publish_windows_changed(&self, windows: &[WindowInfo]) {
        self.platform.on_windows_changed(windows);
    }

    /// v1.43 窗口事件 → [`Platform::on_window_event`]。`peer:<窗口名>` 会话的
    /// 组装只在这里和 [`Self::handle_peer_report`]（推送 convId 与会话注入同键）。
    /// 是否可推送由 `PeerState` 判定并放进事件的 `push` 位——这里只转可推送
    /// 的（平台钩子的约定是「仅在 push = true 时被调」），main 的回调闭包
    /// 不必再判一遍。
    pub async fn publish_window_event(&self, event: WindowEvent) {
        if !event.push {
            debug!(
                target: "imagent::core",
                window = %event.name,
                "窗口事件未过推送闸门（未关注 / 未对话过 / 节流中），不转平台"
            );
            return;
        }
        let conv = ConvId(format!("{CONV_PREFIX}{}", event.name));
        self.platform.on_window_event(&conv, &event).await;
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

/// 钩子上报的注入文本：「📋 本轮结束（自动同步）」+ 空行 + 正文。正文按字符
/// 截到 [`HOOK_MESSAGE_MAX_CHARS`]，超出追加 [`HOOK_TRUNCATED_MARK`]——peer-hook
/// 客户端截完的总长（含标记）仍 ≤ 上限，这里的截断只对不规范的发送方生效。
fn report_text(body: &str) -> String {
    let mut out = String::from("📋 本轮结束（自动同步）\n\n");
    if body.chars().count() > HOOK_MESSAGE_MAX_CHARS {
        out.extend(body.chars().take(HOOK_MESSAGE_MAX_CHARS));
        out.push_str(HOOK_TRUNCATED_MARK);
    } else {
        out.push_str(body);
    }
    out
}
