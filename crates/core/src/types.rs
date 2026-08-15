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
    pub reply_hint: ReplyHint,
}

/// Backend 流式产出的分块。
#[derive(Debug, Clone)]
pub enum AgentChunk {
    Text(String),
    ToolUse { tool: String, input: String },
    ToolResult { tool: String, output: String },
    /// agent 产出的媒体文件（绝对/工作目录相对路径）。目前仅 claude-cli 的 Write
    /// 工具写图片文件时产出；dispatch 在 run 结束后经 Platform::send_media 回传 IM。
    Media { path: String },
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

/// 流式卡片的抽象内容（平台无关）。core dispatch 累积 agent 输出成此结构，
/// Platform::send_card / update_card 负责渲染成各自平台的卡片格式（如飞书 CardKit JSON）。
///
/// 不支持卡片的平台（ilink/wecom）由 trait 默认实现降级：仅把 `text` 当文本发送。
#[derive(Debug, Clone)]
pub struct OutboundCard {
    /// 累积的回复文本（agent 流式 Text 拼接 + 最终 Final）。
    pub text: String,
    /// 工具调用摘要：(tool_name, input 摘要)，用于卡片里展示工具块。
    pub tool_calls: Vec<(String, String)>,
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
