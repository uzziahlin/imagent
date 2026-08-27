//! 核心数据类型：会话/用户/agent 标识、入站消息、媒体引用、流式分块。

use std::path::PathBuf;

/// 平台会话标识，形如 `ilink:<from_user_id>`、`wecom:<user>`。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConvId(pub String);

/// 发送者标识（iLink 的 from_user_id 等）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UserId(pub String);

/// agent 分配的会话 id（如 Claude 的 session_id）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

/// agent 工作目录（agent 子进程的 cwd，**非沙箱**：仅决定工作目录，agent 仍可读取
/// 该目录之外的文件，需配合 allowed_tools + permission_mode 限制）。
#[derive(Debug, Clone)]
pub struct Workdir(pub PathBuf);

/// 平台回传一条消息所需的信息。iLink 需要回传最新 context_token。
#[derive(Debug, Clone)]
pub enum ReplyHint {
    ILink { context_token: String },
    None,
}

/// 媒体引用（ilink 入站媒体已下载落盘到 `~/.imagent/media/`，url 为本地路径）。
#[derive(Debug, Clone)]
pub struct MediaRef {
    pub kind: String,
    pub url: String,
}

/// 消息中 @ 提及的用户（P6-1：平台层从消息元数据解析，正文已替换为 `@名字` 可读
/// 文本）。命令据此把 `/allow @名字` 解析回平台用户 id，免手打 open_id。
#[derive(Debug, Clone)]
pub struct Mention {
    /// 平台用户 id（飞书 open_id / wecom userid / ilink from_user_id）。
    pub user_id: String,
    /// @ 提及的显示名（正文替换用；平台缺名时可为空）。
    pub name: String,
}

/// 命令卡片按钮的视觉样式（P8-1：对标 lcab 的 primary/danger 按钮分层——
/// 推荐动作用 primary 高亮，破坏性动作用 danger 示警；Default 为普通灰按钮）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CardButtonStyle {
    #[default]
    Default,
    Primary,
    Danger,
}

/// 命令卡片按钮（P6-3）：点击等价于发送者手打 `command` 文本——回调经平台侧
/// 转成 `text = <command>` 的 InboundMessage，走与手打命令完全相同的鉴权/分派。
#[derive(Debug, Clone)]
pub struct CardButton {
    /// 按钮展示文本（如「使用 main」）。
    pub label: String,
    /// 点击后注入的命令（如 `/ws use main`）。
    pub command: String,
    /// 视觉样式（缺省普通按钮）。
    pub style: CardButtonStyle,
}

/// 入站消息（`Platform::recv` 产出，core 消费）。
pub struct InboundMessage {
    pub conv_id: ConvId,
    pub sender: UserId,
    pub text: Option<String>,
    /// 入站媒体引用（ilink 图片/文件等已落盘；无媒体则空）。
    pub media: Vec<MediaRef>,
    /// 媒体下载/落盘失败的原因（platform 层记录，含真实错误）。
    /// dispatch 据此向用户报错/注入 prompt，而非静默丢弃。无失败则空。
    pub media_errors: Vec<String>,
    /// 本条消息中 @ 提及的用户（不含 bot 自身；无提及为空）。
    pub mentions: Vec<Mention>,
    /// 本条群消息是否 @ 了 bot（P7-A3：陌生人提示的触发条件；p2p 恒 false——
    /// 平台层据 mentions 元数据与 bot open_id 判定，弱过滤未知时为 false）。
    pub mentioned_bot: bool,
    /// 询问卡按钮回调携带的 request_id（多 pending 精确路由用；普通消息为 None）。
    pub ask_req: Option<String>,
    /// 引用回复的目标消息 id（自由文本路由到被引用的询问卡；无引用为 None）。
    pub reply_to: Option<String>,
    pub reply_hint: ReplyHint,
}

/// bot 已加入的群（P7-A2：`/chat allow-all` 批量放行用）。
#[derive(Debug, Clone)]
pub struct JoinedChat {
    /// 平台侧群标识（如飞书 `oc_xxx`）。
    pub chat_id: String,
    /// 群显示名（缺名为空串）。
    pub name: String,
}

/// Backend 流式产出的分块。
#[derive(Debug, Clone)]
pub enum AgentChunk {
    Text(String),
    ToolUse {
        tool: String,
        input: String,
    },
    ToolResult {
        tool: String,
        output: String,
    },
    /// agent 产出的媒体文件（绝对/工作目录相对路径）。目前仅 claude-cli 的 Write
    /// 工具写图片文件时产出；dispatch 在 run 结束后经 Platform::send_media 回传 IM。
    Media {
        path: String,
    },
    /// backend 已分配/续接的 session id——一经学到尽早通知（P5-5：让 dispatch 在
    /// /stop、超时、失败等拿不到 RunOutcome 的路径也能落库，下条消息续接而非
    /// 静默开新会话）。正常路径 RunOutcome 亦携带，此 chunk 仅供提前学习。
    SessionStarted(String),
    Final(String),
    Error(String),
}

/// Backend 单次执行的结果。
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub session_id: SessionId,
    pub final_text: String,
    /// 本次 run 是否由终止事件正常产出（Final/Terminal/ACP prompt 正常完成）。
    /// false = agent 非正常终止（崩溃等），final_text 为已收到的部分文本。
    pub terminal: bool,
}

/// 本机（电脑端）agent 会话条目——统一 `/resume` 列表用（P4-11）。
///
/// 由 Backend 从自己的本地存储扫描产出（如 claude 后端扫
/// `~/.claude/projects/<workdir编码>/*.jsonl`）；core 据此与 IM 会话历史合并展示，
/// 用户按序号接管，全程无需知道 session id。
#[derive(Debug, Clone)]
pub struct LocalSession {
    pub session_id: String,
    /// epoch 秒（按各 backend 存储的时间戳，如文件 mtime）。
    pub updated_at: i64,
    /// 首条用户消息摘要（帮助用户辨认会话；可为空）。
    pub first_prompt: String,
    /// 会话记录的工作目录（claude jsonl 行内 cwd 字段；解析不到为 None）——
    /// `/resume` 接管前校验：目录编码冲突（如 `/a/b-c` 与 `/a/b/c` 同码）时
    /// 防止把别的项目的会话接到当前 workdir（P5-15）。
    pub cwd: Option<String>,
}

/// 一次工具调用的展示记录（P8-1：摘要由 [`crate::render::tool_summary`] 从
/// input JSON 压成人可读单行，替代此前的裸 JSON 截断；`done` 由 ToolResult
/// 翻转——⏳ 执行中 → ✅ 已完成）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// 工具名（Bash / Read / codex 的 shell 等）。
    pub name: String,
    /// 人可读的单行摘要（如 `git status`、`src/main.rs`）。
    pub summary: String,
    /// 是否已收到 ToolResult（false = 执行中）。
    pub done: bool,
}

/// `/config` 表单卡的一个字段（P9-2：平台无关描述，飞书侧渲染成 CardKit
/// `select_static` 下拉；不支持表单的平台走 trait 默认的文本降级）。
#[derive(Debug, Clone)]
pub struct ConfigFormField {
    /// 配置键（提交时合成 `/config form k=v …`）。
    pub key: String,
    /// 展示名（如「回复形态」）。
    pub label: String,
    /// 当前值（下拉 initial_option）。
    pub current: String,
    /// 可选项：(value, 展示文案)。
    pub options: Vec<(String, String)>,
}

/// 流式卡片的执行阶段（P8-1：分状态 footer——思考中/调用工具/输出中，
/// 由 CardSession 按最近一次 chunk 类型翻转，平台渲染成各自 footer 文案）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CardPhase {
    /// 等待首 chunk（CLI 冷启动 + 模型首 token 的静默期）。
    #[default]
    Thinking,
    /// 最近事件是工具调用（ToolUse 之后、ToolResult/Text 之前）。
    ToolRunning,
    /// 正在流式输出正文。
    Outputting,
}

/// 流式卡片的抽象内容（平台无关）。core dispatch 累积 agent 输出成此结构，
/// Platform::send_card / update_card 负责渲染成各自平台的卡片格式（如飞书 CardKit JSON）。
///
/// 不支持卡片的平台（ilink/wecom）由 trait 默认实现降级：仅把 `text` 当文本发送。
#[derive(Debug, Clone)]
pub struct OutboundCard {
    /// 累积的回复文本（agent 流式 Text 拼接 + 最终 Final）。
    pub text: String,
    /// 工具调用记录（含状态），用于卡片里展示工具块。
    pub tool_calls: Vec<ToolCall>,
    /// 执行阶段（Running 态 footer 文案依据；终态忽略）。
    pub phase: CardPhase,
    /// 卡片终态。
    pub terminal: CardTerminal,
}

/// 卡片终态。
#[derive(Debug, Clone)]
pub enum CardTerminal {
    /// 流式输出中（agent 还在跑）。
    Running,
    /// 正常完成。
    Done,
    /// 出错（含错误信息）。
    Error(String),
}
