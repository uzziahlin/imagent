//! 平台抽象 trait + 能力协商（B1）。
//!
//! ## 能力协商模型
//!
//! `Platform` 的 ~20 个方法里过半是**可选交互特性**（卡片/询问/表单/表情……），
//! 全部带安全 default（no-op 或纯文本降级）——任何平台只实现 `recv`/`send_text`
//! 即可工作。这带来两个问题：① core 的 best-effort 调用点只能「调了看结果」
//! 探路；② 第四平台接入时得逐个翻 default 判断哪些能用。
//!
//! 对称于 Backend 侧的 `PermissionCapability`，本模块引入 [`PlatformCaps`]：
//!
//! - **default 方法 = 安全降级**：能力位未声明时方法的 default 行为就是最终
//!   行为（no-op / 纯文本），不因缺能力而报错；
//! - **`capabilities()` = 显式声明**：平台覆写了某族方法的平台语义实现，就置
//!   对应能力位。声明必须与实际覆写**逐一一致**（每个 impl 的 `capabilities()`
//!   旁有覆写清单注释；crate 测试钉住声明值防漂移）；
//! - **新增交互特性须同时做三件事**：加 trait 方法（带安全 default）+ 加
//!   [`PlatformCaps`] 能力位（含 `ALL` 表）+ `/doctor` 能力面自动可见（读 `ALL`）。
//!
//! 两类方法**没有**能力位：基础收发（`recv`/`send_text`/`send_media`/`name`，
//! trait 必备项）与协作钩子（`note_round_initiator`/`doctor_probes`，default
//! 天然无害，无需协商）。
//!
//! 注意：能力位描述「平台覆写了该族方法的平台语义」，而非「该方法可调」——
//! 例如 [`Platform::send_permission_ask`] 的纯文本 default 全平台可用，`ASK`
//! 位只声明「有交互询问卡句柄可收敛」。core 消费点（dispatch best-effort 调用、
//! `/doctor` 能力面、run() 启动日志）只把 caps 当**加速与可观测**依据，不改变
//! 任何用户可见行为。

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{
    CardButton, ConvId, InboundMessage, JoinedChat, MediaRef, OutboundCard, ReplyHint,
};

// ===== 能力集（B1）=====

/// 平台能力集：`Platform` 交互特性的显式声明（bitflags 风格手写——workspace
/// 无 bitflags 依赖，不为一个 u32 引入新依赖）。
///
/// 组合用 `|`（或 const 上下文 `union`），查询用 `contains`。空集 = 纯文本平台
/// （只实现基础收发，其余全走 default 降级）。
///
/// 每个能力位对应 trait 的一族方法（分组见 [`Platform`]）；**新增能力位必须
/// 同步 `ALL` 表**（/doctor 能力面与一致性测试都从它展开）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct PlatformCaps(u32);

impl PlatformCaps {
    /// 流式卡片：[`Platform::send_card`] / [`Platform::update_card`] 覆写为平台
    /// 卡片语义（飞书 CardKit 单表）。per-conv 细化仍以
    /// [`Platform::supports_streaming_card`] 为准（如飞书评论线程 false）。
    pub const CARDS: Self = Self(1 << 0);
    /// 询问卡闭环：[`Platform::send_permission_ask`] 交互卡 + cancel/resolve/
    /// note_queued 收敛族有平台句柄。**纯文本询问 default 全平台可用**，本位
    /// 只声明「有卡片句柄可收敛」。
    pub const ASK: Self = Self(1 << 1);
    /// 命令交互卡：[`Platform::send_command_card`] 按钮组（点击转命令）。
    pub const COMMAND_CARDS: Self = Self(1 << 2);
    /// /config 表单卡：[`Platform::send_config_form`]（下拉 + 提交组件）。
    pub const FORMS: Self = Self(1 << 3);
    /// 表情回执：[`Platform::react_to_message`] 有真实平台语义。
    pub const REACTIONS: Self = Self(1 << 4);
    /// typing 指示：[`Platform::send_typing`] 非空实现（协议有 typing 语义）。
    pub const TYPING: Self = Self(1 << 5);
    /// 媒体回传：[`Platform::send_media`] 可用（未实现的平台显式 Err，core
    /// 命令层把错误回给用户）。
    pub const MEDIA_UPLOAD: Self = Self(1 << 6);
    /// 加急文本：[`Platform::send_urgent_text`] 以 buzz 形态送达（与
    /// [`Platform::supports_urgent_text`] 声明一致，一致性测试钉住两者同步）。
    pub const URGENT_TEXT: Self = Self(1 << 7);
    /// 强制重连：[`Platform::reconnect`] 覆写（`/reconnect`）。
    pub const RECONNECT: Self = Self(1 << 8);
    /// 群管理语义：`require_mention_in_group` / `set_require_mention_in_group` /
    /// `list_joined_chats` 覆写。
    pub const GROUP_CHATS: Self = Self(1 << 9);

    /// 全量位表（能力位 ↔ 日志短名 ↔ /doctor 人读名）。**新增能力位必须同步
    /// 本表**：漏项 = /doctor 不可见 + `caps_all_table_covers_every_bit` 测试拦。
    pub const ALL: &'static [(Self, &'static str, &'static str)] = &[
        (Self::CARDS, "cards", "流式卡片"),
        (Self::ASK, "ask", "询问卡闭环"),
        (Self::COMMAND_CARDS, "command_cards", "命令按钮卡"),
        (Self::FORMS, "forms", "配置表单卡"),
        (Self::REACTIONS, "reactions", "表情回执"),
        (Self::TYPING, "typing", "typing 指示"),
        (Self::MEDIA_UPLOAD, "media_upload", "媒体回传"),
        (Self::URGENT_TEXT, "urgent_text", "加急文本"),
        (Self::RECONNECT, "reconnect", "强制重连"),
        (Self::GROUP_CHATS, "group_chats", "群管理"),
    ];

    /// 空集（纯文本平台）。
    pub const fn empty() -> Self {
        Self(0)
    }

    /// 并集（const 上下文用；运行时直接 `a | b`）。
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// 是否包含 `other` 的全部位（`other` 为空集时恒 true）。
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// 原始位（仅诊断/序列化展示用，不承诺位分配稳定性）。
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// 是否空集。
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// 已声明能力个数。
    pub const fn count(self) -> u32 {
        self.0.count_ones()
    }

    /// 日志摘要：`cards+ask+typing` 式短名并列（取自 `ALL`）；空集 =
    /// `none`（纯文本平台）。run() 启动日志与跳过告警共用。
    pub fn summary(self) -> String {
        if self.is_empty() {
            return "none（纯文本平台）".into();
        }
        let names: Vec<&str> = Self::ALL
            .iter()
            .filter(|(cap, _, _)| self.contains(*cap))
            .map(|(_, short, _)| *short)
            .collect();
        names.join("+")
    }
}

impl std::ops::BitOr for PlatformCaps {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for PlatformCaps {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

// ===== trait =====

/// IM 平台抽象。由 `ilink` / `wecom` / `feishu` 适配器实现，注入到 `Dispatcher`。
///
/// 方法按能力族分组（组序 = [`PlatformCaps::ALL`] 序）：**基础收发** → **能力
/// 协商** → 各能力族 → **协作钩子**。每组头注释标明对应能力位、default 降级
/// 形态与 core 消费点；能力协商总模型见[模块文档](self)。
#[async_trait]
pub trait Platform: Send + Sync {
    // ===== 基础收发（trait 必备，无 default、无能力位）=====

    /// 阻塞取下一条入站消息（实现内部自管长轮询/重连）。
    async fn recv(&self) -> Result<InboundMessage>;
    async fn send_text(&self, conv: &ConvId, text: &str, hint: &ReplyHint) -> Result<()>;
    /// 发送媒体（/img /file 与 agent 产图回传）。可用性由 `MEDIA_UPLOAD` 位
    /// 声明：未实现的平台显式 Err（core 命令层把文案回给用户，不静默）。
    async fn send_media(&self, conv: &ConvId, media: &MediaRef, hint: &ReplyHint) -> Result<()>;
    /// 平台名，如 `"ilink"`。
    fn name(&self) -> &'static str;

    // ===== 能力协商（B1）=====

    /// 平台能力声明（见[模块文档](self)的协商模型）。default 空集 = 纯文本
    /// 平台；**覆写了某族方法就必须置对应位**（与覆写清单逐一核对，新增覆写
    /// 须同步——各平台 impl 旁有清单注释，crate 测试钉住声明值）。
    fn capabilities(&self) -> PlatformCaps {
        PlatformCaps::empty()
    }

    // ===== CARDS：流式卡片族（能力位 CARDS）=====
    //
    // default：send_card 把 card.text 当文本发送（返回 None），update_card
    // no-op——纯文本平台行为完整，只是无卡片观感。core 流式路径按
    // supports_streaming_card（per-conv）分支建 CardSession，不查 CARDS 位。

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

    // ===== ASK：询问卡闭环族（能力位 ASK）=====
    //
    // default：send_permission_ask 走纯文本询问（全平台可用，闭环不缺环——
    // 用户回 y/n 文本即可）；cancel/resolve/note_queued 均 no-op（无卡可收敛，
    // 滞留文本无害）。core 的 cancel/resolve/note 调用点查 ASK 位跳过注定
    // no-op 的往返；send_permission_ask 本身**不**按位跳过（文本询问即交付）。

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

    /// P10-③：运行中入队消息时，若该会话挂着**未决审批卡**则更新其 note 行
    /// （如 `⏳ 等待你审批 · 后面还排着 N 条消息`）——审批等待是流式卡最静默
    /// 的窗口（无 chunk），排队状态需要推送而不是等拉取。默认 no-op（无审批卡
    /// 概念/不支持的平台）。best-effort：失败不影响排队本身。
    async fn note_queued_on_ask(
        &self,
        _conv: &ConvId,
        _note: &str,
        _hint: &ReplyHint,
    ) -> Result<()> {
        Ok(())
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

    // ===== COMMAND_CARDS：命令交互卡（能力位 COMMAND_CARDS）=====
    //
    // default：降级纯文本（title + body + 可手打的命令清单）——纯文本平台
    // 信息不丢，故 core 调用点不按位跳过（跳过会吞掉降级文本）。

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

    // ===== FORMS：/config 表单卡（能力位 FORMS）=====
    //
    // default：降级纯文本（fallback 为各平台通用的当前值 + 用法说明）——同
    // COMMAND_CARDS，调用点不按位跳过。

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

    // ===== REACTIONS：表情回执（能力位 REACTIONS）=====
    //
    // default：no-op。core 的标注调用点（轮次起/终态、排队、转向）查 REACTIONS
    // 位跳过注定 no-op 的往返（best-effort，失败本就不影响主流程）。

    /// bot 对用户入站消息的表情标注：轮次开始 [`MsgReaction::Processing`]
    ///（👀 类「在做了」），终态翻 [`MsgReaction::Done`] / [`MsgReaction::Failed`]
    /// ——反馈直接落在用户的消息上，零新增消息。默认 no-op（不支持/未配置的
    /// 平台）；best-effort：失败由调用方 warn，不影响轮次。
    async fn react_to_message(
        &self,
        _conv: &ConvId,
        _source_msg_id: &str,
        _reaction: crate::types::MsgReaction,
    ) -> Result<()> {
        Ok(())
    }

    // ===== TYPING：输入中指示（能力位 TYPING）=====
    //
    // default：no-op。core 轮首调用点查 TYPING 位跳过（协议无 typing 语义的
    // 平台覆写也是 no-op，跳过等价且省一次往返）。

    /// 可选：typing 指示。P1 默认空实现。
    async fn send_typing(&self, _conv: &ConvId, _hint: &ReplyHint) -> Result<()> {
        Ok(())
    }

    // ===== URGENT_TEXT：加急文本（能力位 URGENT_TEXT）=====
    //
    // default：send_urgent_text 回退普通 send_text（提醒内容仍送达，只是不
    // 振铃）——故催办调用点**不**按位跳过；supports_urgent_text 供「完成强
    // 提醒」这类「普通文本只是重复噪音」的场景整体 no-op（与 URGENT_TEXT 位
    // 同步，一致性测试钉住）。

    /// 发送**加急（buzz）**文本：用于需要用户及时处理的提醒（审批过半催办、
    /// 长任务完成强提醒）。支持的平台（飞书 text 消息体 `buzz` 字段）以加急
    /// 形态送达；默认实现回退普通 `send_text`——不支持 buzz 的平台提醒内容
    /// 仍然送达，只是不振铃。免打扰时段（`quiet_hours`）的降级由实现侧处理
    /// （只影响加急形态，不影响内容）。
    async fn send_urgent_text(&self, conv: &ConvId, text: &str, hint: &ReplyHint) -> Result<()> {
        self.send_text(conv, text, hint).await
    }

    /// 平台是否支持加急（buzz）文本。core 据此决定「长任务完成强提醒」是否
    /// 发送——普通回复已含全部信息的场景，不支持 buzz 的平台再发一条普通文本
    /// 只是重复噪音，故此类强提醒对不支持的平台整体 no-op（默认 false）。
    fn supports_urgent_text(&self) -> bool {
        false
    }

    // ===== RECONNECT：强制重连（能力位 RECONNECT）=====
    //
    // default：Err（`/reconnect` 命令层把文案回给用户）。core 不按位跳过——
    // 错误文案本身就是用户反馈；RECONNECT 位供 /doctor 能力面展示。

    /// 强制平台重连（P4-7 `/reconnect`）：断开当前长连接并立即重连，排查僵死连接。
    /// 默认不支持（返回 Err 由命令层提示）。
    async fn reconnect(&self) -> Result<()> {
        Err(crate::error::CoreError::Platform(
            "platform",
            "该平台不支持强制重连".into(),
        ))
    }

    // ===== GROUP_CHATS：群管理语义（能力位 GROUP_CHATS）=====
    //
    // default：查询类返回 None（平台无群聊 @ 概念）、变更/枚举类返回 Err（命令
    // 层把文案回给用户）。core 不按位跳过；GROUP_CHATS 位供 /doctor 展示。

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

    // ===== 协作钩子（安全 default，无能力位——无需协商）=====
    //
    // 这两个方法的 default 无副作用、也不对应任何可选特性，第四平台不覆写
    // 零损失，故不设能力位（与「新增交互特性须加位」的约定区分）。

    /// v1.23 说话人归属：dispatch 在每轮首条消息分派前调用——平台可据此
    /// 锚定「本轮发起者」（卡片标注/按钮点击权校验），避免被运行中的插话
    /// 者漂移。默认 no-op。
    async fn note_round_initiator(&self, _conv: &ConvId, _sender: &str) {}

    /// v1.26 权限自检：平台侧 API 权限/连通性探测（/doctor 追加段）。
    /// 返回逐行诊断（✅/⚠️ + 可行动指引）。默认无（纯文本平台）。
    async fn doctor_probes(&self) -> Vec<String> {
        Vec::new()
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

#[cfg(test)]
mod tests {
    //! B1：PlatformCaps 本体不变量（位表完备性/互异、集合运算、摘要）。
    //! 三平台的「声明值 ↔ 覆写清单」一致性测试在各平台 crate 内（构造真实
    //! platform 实例调 `Platform::capabilities` 钉值）。

    use super::*;

    /// ALL 表必须覆盖全部能力位且互异（新增 const 忘记进 ALL 在此拦截）。
    #[test]
    fn caps_all_table_covers_every_bit() {
        // 显式列出全部 pub const 位——新增能力位时本测试强制同步（漏列 = 计数
        // 不齐；ALL 里塞重复位 = count 不齐）。
        let every = [
            PlatformCaps::CARDS,
            PlatformCaps::ASK,
            PlatformCaps::COMMAND_CARDS,
            PlatformCaps::FORMS,
            PlatformCaps::REACTIONS,
            PlatformCaps::TYPING,
            PlatformCaps::MEDIA_UPLOAD,
            PlatformCaps::URGENT_TEXT,
            PlatformCaps::RECONNECT,
            PlatformCaps::GROUP_CHATS,
        ];
        assert_eq!(PlatformCaps::ALL.len(), every.len());
        let mut combined = PlatformCaps::empty();
        for cap in every {
            assert!(
                PlatformCaps::ALL.iter().any(|(c, _, _)| c == &cap),
                "能力位 {cap:?} 缺 ALL 表项（/doctor 将不可见）"
            );
            combined |= cap;
        }
        assert_eq!(combined.count(), every.len() as u32, "能力位存在重复位值");
        // ALL 表自身互异（防手抄错行）。
        let all_or = PlatformCaps::ALL
            .iter()
            .fold(PlatformCaps::empty(), |acc, (c, _, _)| acc | *c);
        assert_eq!(all_or.count(), PlatformCaps::ALL.len() as u32);
        // 三元组字段非空（日志/doctor 展示不露空串）。
        for (_, short, label) in PlatformCaps::ALL {
            assert!(!short.is_empty() && !label.is_empty());
        }
    }

    #[test]
    fn caps_set_ops() {
        let empty = PlatformCaps::empty();
        assert!(empty.is_empty() && empty.bits() == 0);
        assert!(empty.contains(empty) && !empty.contains(PlatformCaps::CARDS));

        let cards_ask = PlatformCaps::CARDS | PlatformCaps::ASK;
        assert_eq!(cards_ask, PlatformCaps::CARDS.union(PlatformCaps::ASK));
        assert!(cards_ask.contains(PlatformCaps::CARDS));
        assert!(cards_ask.contains(PlatformCaps::ASK));
        assert!(!cards_ask.contains(PlatformCaps::TYPING));
        assert_eq!(cards_ask.count(), 2);

        // BitOrAssign。
        let mut acc = PlatformCaps::empty();
        acc |= PlatformCaps::TYPING;
        assert!(acc.contains(PlatformCaps::TYPING) && acc.count() == 1);
    }

    #[test]
    fn caps_summary() {
        assert_eq!(PlatformCaps::empty().summary(), "none（纯文本平台）");
        let s = (PlatformCaps::CARDS | PlatformCaps::ASK | PlatformCaps::TYPING).summary();
        assert_eq!(s, "cards+ask+typing");
        // 全亮摘要含全部短名（ALL 序即摘要序）。
        let full = PlatformCaps::ALL
            .iter()
            .fold(PlatformCaps::empty(), |acc, (c, _, _)| acc | *c);
        assert_eq!(
            full.summary(),
            PlatformCaps::ALL
                .iter()
                .map(|(_, s, _)| *s)
                .collect::<Vec<_>>()
                .join("+")
        );
    }
}
