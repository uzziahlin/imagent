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

/// 平台回传一条消息所需的信息。A3（架构债）：原 `ILink` 变体泛化为
/// `ContextToken`——「回传需携带会话上下文 token」是平台无关概念（iLink 的
/// context_token 是首个实例，命名不再绑定具体平台）。
#[derive(Debug, Clone)]
pub enum ReplyHint {
    ContextToken { context_token: String },
    None,
}

/// 流式卡片句柄丢失标记（错误串哨兵）：平台侧 patch 遇「卡片不存在/已删除」类
/// 错误时，清自身缓存后返回**错误串含本标记**的错误——core 的 CardSession 据此
/// 摘除 live_cards 登记并把 msg_id 置空（下帧重发新卡），启动扫描据此作废登记
/// （终止无限重试）。用字符串哨兵而非新错误变体：错误跨 `Platform` trait 边界
/// 统一为 `CoreError`，变体拆分需动全部平台。
pub const CARD_HANDLE_LOST: &str = "imagent:card-handle-lost";

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

/// 平台控制信号（非用户消息）：平台侧把系统事件（消息撤回 / bot 被移出群等）
/// 合成 `InboundMessage` 投递进 recv 通道，dispatch 在白名单校验**之前**消费
/// （见 `handle` 的 control 分支）——控制信号不是对话输入，不应被鉴权丢弃。
/// 普通消息为 `None`。
#[derive(Debug, Clone)]
pub enum InboundControl {
    /// 用户撤回了消息（飞书 `im.message.recalled_v1`）：被撤回消息的平台侧 id 在
    /// [`InboundMessage::source_msg_id`]，dispatch 据此把同 id 的**排队**消息移出。
    /// `notify_conv` 为可回执的会话（事件携带 chat_id / sender 时给，撤回提示的
    /// 发送对象）；`probe_convs` 为可能承载该消息的会话 key 集合（群 conv 与
    /// 发送者私聊 conv 两种形态都可能——在飞判定用）。
    MessageRecalled {
        notify_conv: Option<ConvId>,
        probe_convs: Vec<ConvId>,
    },
    /// bot 被移出群（飞书 `im.chat.member.bot.deleted_v1`）：`conv_id` 即群 conv，
    /// dispatch 据此从会话白名单移除并通知管理员。
    BotRemovedFromChat,
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
    /// 消息来源的平台侧消息 id（飞书 message_id 等；撤回事件按此匹配排队消息，
    /// 见 [`InboundControl::MessageRecalled`]）。平台无消息 id 概念 / 按钮回调
    /// 等合成消息为 None。
    pub source_msg_id: Option<String>,
    /// 平台控制信号（消息撤回 / bot 被移出群等系统事件的合成载体）。普通用户
    /// 消息为 None。
    pub control: Option<InboundControl>,
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

/// W2-2：任务清单项状态（Claude Code 的 TodoWrite / ACP Plan）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// W2-2：任务清单项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub text: String,
    pub status: TodoStatus,
}

/// Backend 流式产出的分块。
#[derive(Debug, Clone)]
pub enum AgentChunk {
    Text(String),
    /// W2-1：思考过程（与正文分离）——CLI 的 thinking 块 / ACP 的
    /// AgentThoughtChunk。不混入 Text（正文流式/最终回复保持纯净），卡片侧
    /// 折叠展示、cot 档位控制；纯文本平台忽略（正文流已够）。
    Thought(String),
    ToolUse {
        tool: String,
        input: String,
        /// W2-3：工具调用 id（CLI `tool_use.id` / ACP `tool_call_id`）——与
        /// ToolResult 精确配对（并行同名调用不错配）；后端拿不到为 None，
        /// 消费方回退按名配对。
        id: Option<String>,
    },
    ToolResult {
        tool: String,
        output: String,
        /// W2-3：对应 ToolUse 的 id（CLI `tool_result.tool_use_id`）。
        id: Option<String>,
    },
    /// W2-2：任务清单（Claude Code TodoWrite 工具调用 / ACP Plan 通知）。
    /// 每次携带**全量**清单状态（两个来源都是全量替换语义），消费方直接覆盖。
    TodoList {
        items: Vec<TodoItem>,
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

/// 一次 run 的 token 用量与成本（按三 CLI backend 实际可得字段设计，缺失为 None）：
/// - claude `result` 事件：usage.input_tokens/output_tokens/cache_read_input_tokens +
///   顶层 total_cost_usd；
/// - codex `turn.completed`：usage.input_tokens/output_tokens（无 cost）；
/// - gemini `result`：stats.input_tokens/output_tokens（无 cost）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsageStats {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 缓存命中 token（claude cache_read_input_tokens；codex/gemini 缺失为 None）。
    pub cached_tokens: Option<u64>,
    /// 本次 run 的美元成本（仅 claude 提供；缺失为 None）。
    pub total_cost_usd: Option<f64>,
}

impl UsageStats {
    /// 合并多个 usage 事件（一个 run 可能产出多条）：input/output 累加求和，
    /// cost 取最后一个非 None 值（后到的终态事件权威），cached 同理取最后非 None。
    pub fn merge(self, later: UsageStats) -> UsageStats {
        UsageStats {
            input_tokens: self.input_tokens + later.input_tokens,
            output_tokens: self.output_tokens + later.output_tokens,
            cached_tokens: later.cached_tokens.or(self.cached_tokens),
            total_cost_usd: later.total_cost_usd.or(self.total_cost_usd),
        }
    }

    /// 人可读单行摘要（footer / 日志用）：cost 优先，缺失退化为 tokens 维度。
    pub fn display(&self) -> String {
        match self.total_cost_usd {
            Some(c) => format!("${c:.4}"),
            None => format!(
                "in {} / out {} tokens",
                self.input_tokens, self.output_tokens
            ),
        }
    }
}

/// Backend 单次执行的结果。
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub session_id: SessionId,
    pub final_text: String,
    /// 本次 run 是否由终止事件正常产出（Final/Terminal/ACP prompt 正常完成）。
    /// false = agent 非正常终止（崩溃等），final_text 为已收到的部分文本。
    pub terminal: bool,
    /// 本次 run 的 token 用量与成本（backend 未产出 usage 事件时为 None）。
    pub usage: Option<UsageStats>,
    /// W2-4：终止原因（`stop_reason`；None = 正常结束或后端未提供）——
    /// max_tokens / max_turn_requests / refusal / cancelled。dispatch 据此给
    /// 用户「输出被截断」类提示。
    pub stop_reason: Option<String>,
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
    /// W2-3：工具调用 id（有则与 ToolResult 精确配对；None = 后端未提供，
    /// 按名配对兜底）。
    pub id: Option<String>,
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
    /// W2-1：思考过程片段（与正文分离；平台按 cot 档位折叠展示，off 档不显）。
    /// 累积上限见 CardSession（防超长思考撑爆卡片 payload）。
    pub thoughts: Vec<String>,
    /// W2-2：任务清单（Claude Code todo / ACP Plan；全量替换语义，最新一次
    /// TodoList chunk 为准）。空 = 无清单。
    pub todos: Vec<TodoItem>,
    /// 执行阶段（Running 态 footer 文案依据；终态忽略）。
    pub phase: CardPhase,
    /// 排队提示（P10：本轮运行中入队的消息摘要，如 `📥 排队 2 条，最新：「…」`；
    /// None = 无排队）。由 CardSession 每次 patch 从 dispatcher 的排队状态拉取
    /// （活动期随 chunk 刷新）；平台在 Running footer 追加展示，终态忽略。
    pub queued_hint: Option<String>,
    /// 本轮运行的时长（秒；Running 态 10s 粒度量化，终态为**全量秒数**——
    /// Wave B-3：终态 footer 展示总耗时）。Running 态由 CardSession 从轮次起点
    /// 计算；量化保证 footer 去重缓存命中（10s 内内容不变不 patch）。终态不做
    /// 量化（每次终态只 patch 一次，无刷屏风险），平台据此拼「✅ 已完成 · 30m」。
    pub run_secs: u64,
    /// 本轮成本摘要（UsageStats.display()，如 `$0.012` 或 `in 1.2k · out 3.4k tokens`）；
    /// None = backend 未产出 usage。终态 footer 追加展示（`✅ 已完成 · $0.012`）。
    pub usage_display: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::UsageStats;

    fn u(input: u64, output: u64, cached: Option<u64>, cost: Option<f64>) -> UsageStats {
        UsageStats {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            total_cost_usd: cost,
        }
    }

    /// 合并语义：input/output 求和；cost/cached 取最后非 None（后到权威）。
    #[test]
    fn usage_merge_sums_tokens_and_takes_last_cost() {
        let a = u(10, 5, Some(2), None);
        let b = u(1, 2, None, Some(0.012));
        let m = a.merge(b);
        assert_eq!(m.input_tokens, 11);
        assert_eq!(m.output_tokens, 7);
        assert_eq!(m.cached_tokens, Some(2));
        assert_eq!(m.total_cost_usd, Some(0.012));

        // cost 先有后有：后到的 None 不覆盖已有值。
        let c = u(0, 0, Some(9), Some(0.5)).merge(u(0, 0, None, None));
        assert_eq!(c.total_cost_usd, Some(0.5));
        assert_eq!(c.cached_tokens, Some(9));
    }

    /// display：有 cost 显示美元，无 cost 退化 tokens 维度。
    #[test]
    fn usage_display_prefers_cost() {
        assert_eq!(u(3, 4, None, Some(0.0123)).display(), "$0.0123");
        assert_eq!(u(3, 4, None, None).display(), "in 3 / out 4 tokens");
    }
}
