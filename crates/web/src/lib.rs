//! `imagent-web`：Web 驾驶舱 M2——网关侧 WebPlatform。
//!
//! 拓扑（M1 见 `imagent-relay`）：`浏览器 WS ⇄ ECS relay ⇄ 本 crate`。本 crate
//! 是网关侧**出站** WS 客户端（连 `wss://…/imagent/gateway`，Bearer 头直通
//! 认证，Mac 零监听端口）+ [`Platform`](imagent_core::Platform) 实现：
//! - [`client`]：WS 客户端任务——重连（指数退避 1s→封顶 30s，成功建连重置）、
//!   心跳（30s 协议层 Ping，对齐 relay 90s idle 超时）、停机
//!   （CancellationToken，平台 drop 即退出）。把浏览器帧分拣进 inbound /
//!   sync channel，把出站帧写回 relay。
//! - [`platform`]：`WebPlatform`——`recv`/`send_*` 覆写为帧收发，含审批闭环
//!   （approval_request / approval_resolved）与 sync 重发（D6 会话恢复）。
//!
//! 帧协议 v1（与已部署 relay / M1 对齐）：JSON 文本帧，信封 `{"v":1,"type":…}`。
//! 入站（浏览器→网关）：`message` / `approval_response` / `control` / `sync` /
//! `ping`（`machine` / `error` / `hello_ack` 为 relay 自产帧，client 分拣自理）。
//! 出站（网关→浏览器）：`text` / `card` / `card_patch` / `approval_request` /
//! `approval_resolved` / `pong`。
//!
//! **帧内 convId 协议约定：恒为完整 conv 形态（`web:<session>`）**。出站帧
//! （text / card / approval_request …）写入的即完整 conv，浏览器回执帧
//! （message / approval_response）**原样回传收到的 convId** 即可，不得剥前
//! 缀；裸 session 名（如 `work`）仅作入站兼容输入，由 `normalize_conv`
//! 统一归一为完整 conv。
//!
//! 本模块承载**帧类型定义与纯函数**（入站映射、出站药化、超限截断），全部
//! 无 IO 便于单测；网络循环在 [`client`]，[`Platform`] 装配在 [`platform`]。

#![deny(unsafe_code)]

pub mod client;
pub mod platform;

pub use platform::WebPlatform;

use imagent_core::{
    CardPhase, CardTerminal, ConvId, CoreError, InboundMessage, OutboundCard, ReplyHint, Result,
    TodoStatus, UserId,
};
use serde::Serialize;

/// 统一 tracing target（仓库纪律）。
pub(crate) const TARGET: &str = "imagent::web";

// ---------------------------------------------------------------------------
// 出站帧（网关 → 浏览器）
// ---------------------------------------------------------------------------

/// relay 单帧上限 256KB（M1 §4.4）；出站软限取 240KB——预留 JSON 转义膨胀
/// 余量，超限即截断 text 字段（见 [`encode_text_frame`] / [`encode_card_frame`]）。
pub(crate) const FRAME_SOFT_LIMIT_BYTES: usize = 240 * 1024;
/// 超限截断的头部保窗（char，先例 src/delegate.rs 头尾窗）。
pub(crate) const TRUNC_HEAD_CHARS: usize = 18_000;
/// 超限截断的尾部保窗（结论通常在尾部）。
pub(crate) const TRUNC_TAIL_CHARS: usize = 6_000;

/// 文本帧（send_text 药化）。`hint` 为协议预留位（web 无 context token，
/// 恒 None 不序列化）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TextFrame {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(rename = "convId")]
    conv_id: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

/// 卡片帧（send_card → `card` / update_card → `card_patch`；sync 重发用 `card`）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CardFrame {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(rename = "convId")]
    conv_id: String,
    #[serde(rename = "messageId")]
    message_id: String,
    card: CardPayload,
    /// M3 SPA 据此渲染进行中/终态样式：`streaming` = terminal Running，
    /// `done` = 其余（由 [`CardPayload::state`] 派生）。
    state: &'static str,
}

/// 审批请求帧（send_permission_ask 覆写）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ApprovalRequestFrame {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(rename = "convId")]
    conv_id: String,
    #[serde(rename = "requestId")]
    request_id: String,
    tool: String,
    #[serde(rename = "inputSummary")]
    input_summary: String,
}

/// 审批收敛帧（resolve / cancel / cancel_all 覆写）。decision ∈
/// allow|deny|always（用户真实决定）| cancelled（系统取消——超时/顶替//stop）。
/// 协议另留 `timeout` 值供 M3 细分（M2 的超时路径复用 cancelled）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ApprovalResolvedFrame {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(rename = "convId")]
    conv_id: String,
    #[serde(rename = "requestId")]
    request_id: String,
    decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// pong 帧（client 应答浏览器应用层 ping）。
#[derive(Debug, Serialize)]
pub(crate) struct PongFrame {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
}

/// pong 帧序列化（client 心跳应答用；形状固定无截断面）。
pub(crate) fn encode_pong_frame() -> String {
    serde_json::to_string(&PongFrame { v: 1, kind: "pong" })
        .unwrap_or_else(|_| r#"{"v":1,"type":"pong"}"#.to_string())
}

/// [`OutboundCard`] 的可序列化 DTO（core 类型未派生 serde——web 帧协议自带
/// 形状，M3 SPA 直接消费）。字段与 core 逐一对应，语义见 `types.rs`。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CardPayload {
    pub text: String,
    pub tool_calls: Vec<ToolCallPayload>,
    pub thoughts: Vec<String>,
    pub todos: Vec<TodoPayload>,
    /// thinking | tool_running | outputting | waiting_approval。
    pub phase: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queued_hint: Option<String>,
    pub run_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_display: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_digest: Option<String>,
    /// running | done | error。
    pub terminal: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 单条工具调用记录（[`imagent_core::types::ToolCall`] 的 DTO）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolCallPayload {
    pub name: String,
    pub summary: String,
    pub done: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// 任务清单项（[`TodoItem`] 的 DTO）；status: pending | in_progress | completed。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TodoPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub text: String,
    pub status: &'static str,
}

impl CardPayload {
    pub fn from_card(card: &OutboundCard) -> Self {
        CardPayload {
            text: card.text.clone(),
            tool_calls: card
                .tool_calls
                .iter()
                .map(|t| ToolCallPayload {
                    name: t.name.clone(),
                    summary: t.summary.clone(),
                    done: t.done,
                    id: t.id.clone(),
                })
                .collect(),
            thoughts: card.thoughts.clone(),
            todos: card
                .todos
                .iter()
                .map(|t| TodoPayload {
                    id: t.id.clone(),
                    text: t.text.clone(),
                    status: todo_status_str(t.status),
                })
                .collect(),
            phase: phase_str(card.phase),
            queued_hint: card.queued_hint.clone(),
            run_secs: card.run_secs,
            usage_display: card.usage_display.clone(),
            task_digest: card.task_digest.clone(),
            terminal: terminal_str(&card.terminal),
            error: match &card.terminal {
                CardTerminal::Error(e) => Some(e.clone()),
                _ => None,
            },
        }
    }

    /// 帧级 state（§4.2）：Running → `streaming`，其余终态 → `done`。
    pub fn state(&self) -> &'static str {
        if self.terminal == "running" {
            "streaming"
        } else {
            "done"
        }
    }
}

fn phase_str(p: CardPhase) -> &'static str {
    match p {
        CardPhase::Thinking => "thinking",
        CardPhase::ToolRunning => "tool_running",
        CardPhase::Outputting => "outputting",
        CardPhase::WaitingApproval => "waiting_approval",
    }
}

fn terminal_str(t: &CardTerminal) -> &'static str {
    match t {
        CardTerminal::Running => "running",
        CardTerminal::Done => "done",
        CardTerminal::Error(_) => "error",
    }
}

fn todo_status_str(s: TodoStatus) -> &'static str {
    match s {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in_progress",
        TodoStatus::Completed => "completed",
    }
}

/// 头尾窗截断（§4.3，先例 src/delegate.rs）：保头 [`TRUNC_HEAD_CHARS`] /
/// 尾 [`TRUNC_TAIL_CHARS`] char，中段替换为标记。短文原样返回。
pub(crate) fn truncate_text(text: &str) -> String {
    let total = text.chars().count();
    if total <= TRUNC_HEAD_CHARS + TRUNC_TAIL_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(TRUNC_HEAD_CHARS).collect();
    let tail: String = text.chars().skip(total - TRUNC_TAIL_CHARS).collect();
    format!("{head}\n…[已截断]…\n{tail}")
}

/// 序列化 + 超限截断守卫（出站帧公共出口，§4.3）：序列化后 > 240KB 时把
/// `raw_text`（text 帧的正文 / card 帧的 card.text）头尾窗截断后经 `rebuild`
/// 重建帧再序列化，并 log warn。截断只针对 text 类字段——其余字段（工具
/// 记录/思考片段）有 CardSession 累积上限兜底，不构成超限主因。
pub(crate) fn encode_frame<T, F>(frame: &T, raw_text: &str, rebuild: F) -> Result<String>
where
    T: Serialize,
    F: FnOnce(&str) -> T,
{
    let json = serde_json::to_string(frame)
        .map_err(|e| CoreError::Platform(TARGET, format!("出站帧序列化失败：{e}")))?;
    if json.len() <= FRAME_SOFT_LIMIT_BYTES {
        return Ok(json);
    }
    tracing::warn!(
        target: TARGET,
        orig_bytes = json.len(),
        "出站帧超 240KB（relay 单帧上限 256KB），截断 text 后重发"
    );
    let truncated = truncate_text(raw_text);
    serde_json::to_string(&rebuild(&truncated))
        .map_err(|e| CoreError::Platform(TARGET, format!("出站帧截断后序列化失败：{e}")))
}

/// text 帧药化（send_text）。
pub(crate) fn encode_text_frame(conv_id: &str, text: &str) -> Result<String> {
    let frame = TextFrame {
        v: 1,
        kind: "text",
        conv_id: conv_id.to_string(),
        text: text.to_string(),
        hint: None,
    };
    encode_frame(&frame, text, |t| TextFrame {
        text: t.to_string(),
        ..frame.clone()
    })
}

/// card / card_patch 帧药化（send_card / update_card / sync 重发共用）。
/// `kind` 取 "card" | "card_patch"。
pub(crate) fn encode_card_frame(
    kind: &'static str,
    conv_id: &str,
    message_id: &str,
    card: &CardPayload,
) -> Result<String> {
    let frame = CardFrame {
        v: 1,
        kind,
        conv_id: conv_id.to_string(),
        message_id: message_id.to_string(),
        card: card.clone(),
        state: card.state(),
    };
    encode_frame(&frame, &card.text, |t| CardFrame {
        card: CardPayload {
            text: t.to_string(),
            ..card.clone()
        },
        ..frame.clone()
    })
}

/// approval_request 帧药化（send_permission_ask）。inputSummary 经
/// [`imagent_core::render::tool_summary`] 压缩（≤80 char），无超限面。
pub(crate) fn encode_approval_request_frame(
    conv_id: &str,
    request_id: &str,
    tool: &str,
    input_summary: &str,
) -> Result<String> {
    let frame = ApprovalRequestFrame {
        v: 1,
        kind: "approval_request",
        conv_id: conv_id.to_string(),
        request_id: request_id.to_string(),
        tool: tool.to_string(),
        input_summary: input_summary.to_string(),
    };
    serde_json::to_string(&frame)
        .map_err(|e| CoreError::Platform(TARGET, format!("approval 帧序列化失败：{e}")))
}

/// approval_resolved 帧药化（resolve / cancel / cancel_all）。
pub(crate) fn encode_approval_resolved_frame(
    conv_id: &str,
    request_id: &str,
    decision: &str,
    message: Option<&str>,
) -> Result<String> {
    let frame = ApprovalResolvedFrame {
        v: 1,
        kind: "approval_resolved",
        conv_id: conv_id.to_string(),
        request_id: request_id.to_string(),
        decision: decision.to_string(),
        message: message.map(String::from),
    };
    serde_json::to_string(&frame)
        .map_err(|e| CoreError::Platform(TARGET, format!("approval_resolved 帧序列化失败：{e}")))
}

// ---------------------------------------------------------------------------
// 入站帧（浏览器 → 网关）映射
// ---------------------------------------------------------------------------

/// 浏览器入站帧分拣结果（client 侧消费；纯函数便于单测）。
#[derive(Debug)]
pub(crate) enum InboundAction {
    /// 业务消息（message / approval_response / control）→ inbound channel →
    /// `Platform::recv`。（Box 消除变体间大 size 差——InboundMessage 含多个
    /// Vec，clippy large_enum_variant。）
    Message(Box<InboundMessage>),
    /// sync 请求 → sync responder 重发缓存卡片（不进 recv）。
    Sync,
    /// 应用层 ping → client 直接回 pong（不进 recv）。
    Ping,
    /// 未知 type：debug 忽略——前向兼容，M3 SPA 新增帧不致网关断连。
    Ignore,
    /// 协议违规 / 缺必填字段：warn 原因并丢弃（不进 recv）。
    Warn(&'static str),
}

/// `approval_response.decision` → 审批词表精确词（§5.2）：allow→`y`、
/// deny→`n`、always→`always`（`imagent_core::permission` 的 ALLOW/DENY/
/// ALWAYS 词表均含——ask_req 锚定后 parse_reply 消费，词必须精确命中）。
fn decision_word(decision: &str) -> Option<&'static str> {
    match decision {
        "allow" => Some("y"),
        "deny" => Some("n"),
        "always" => Some("always"),
        _ => None,
    }
}

/// 帧内 convId 归一（协议约定的唯一入口）：**帧内 convId 恒为完整 conv
/// 形态（`web:<session>`）**——出站帧写入的就是完整 conv，浏览器原样回传
/// 即可（SPA 回执 approval_response / message 的正常路径）；裸 session 名
/// （如 `work`）仅作兼容输入，补 `web:` 前缀归一。
/// - raw 为 None / 空串 → 默认会话 `web:<default_session>`
/// - raw 以 `web:` 开头 → 原样返回（不双前缀）
/// - 其它（裸 session 名）→ `web:<raw>`
fn normalize_conv(raw: Option<&str>, default_session: &str) -> String {
    match raw {
        Some(c) if !c.is_empty() && c.starts_with("web:") => c.to_string(),
        Some(c) if !c.is_empty() => format!("web:{c}"),
        _ => format!("web:{default_session}"),
    }
}

/// 合成 InboundMessage 公共骨架（§5.2；字段全集对照 src/webhook.rs:650 构造
/// 示例）。web 用户手打消息走 steering（no_steer=false，与 webhook 合成消息
/// 相反）。`conv` 为已经 [`normalize_conv`] 归一的完整 conv 字符串
/// （`web:<session>`），本函数不再加前缀。
fn synth_message(conv: String, text: String, ask_req: Option<String>) -> InboundMessage {
    InboundMessage {
        conv_id: ConvId(conv),
        sender: UserId("web:user".to_string()),
        sender_name: None,
        text: Some(text),
        media: Vec::new(),
        media_errors: Vec::new(),
        mentions: Vec::new(),
        mentioned_bot: false,
        ask_req,
        reply_to: None,
        source_msg_id: None,
        control: None,
        no_steer: false,
        reply_hint: ReplyHint::None,
    }
}

/// 入站业务帧映射（§4.1 表；client 已解析为 JSON Value，machine/error 等
/// relay 自产帧由 client 分拣，不进本函数）。帧内 convId 协议约定为完整
/// conv（`web:<session>`，出站帧写入的形态）——浏览器原样回传即可；裸
/// session 名仅作兼容输入，三分支统一经 [`normalize_conv`] 归一。
pub(crate) fn map_inbound_frame(v: &serde_json::Value, default_session: &str) -> InboundAction {
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        // message：convId 缺省 / 空串 → default_session；text 缺失无意义，丢弃。
        "message" => {
            let Some(text) = v.get("text").and_then(|t| t.as_str()) else {
                return InboundAction::Warn("message 帧缺 text 字段，丢弃");
            };
            let conv = normalize_conv(v.get("convId").and_then(|c| c.as_str()), default_session);
            InboundAction::Message(Box::new(synth_message(conv, text.to_string(), None)))
        }
        // approval_response：convId 必填（审批按 conv 路由，缺省无法投递）；
        // 决策词必须精确命中词表。
        "approval_response" => {
            let Some(conv) = v.get("convId").and_then(|c| c.as_str()) else {
                return InboundAction::Warn(
                    "approval_response 帧缺 convId（审批按 conv 路由），丢弃",
                );
            };
            let Some(request_id) = v.get("requestId").and_then(|r| r.as_str()) else {
                return InboundAction::Warn("approval_response 帧缺 requestId，丢弃");
            };
            let Some(decision) = v.get("decision").and_then(|d| d.as_str()) else {
                return InboundAction::Warn("approval_response 帧缺 decision，丢弃");
            };
            match decision_word(decision) {
                Some(word) => InboundAction::Message(Box::new(synth_message(
                    normalize_conv(Some(conv), default_session),
                    word.to_string(),
                    Some(request_id.to_string()),
                ))),
                None => InboundAction::Warn(
                    "approval_response 帧 decision 非法（须 allow|deny|always），丢弃",
                ),
            }
        }
        // control：平台侧转命令文本（与 IM 按钮点击同机制——走既有鉴权/分派，
        // 见 platform.rs send_command_card 文档注释的既定惯例）。conv 落默认
        // 会话（SPA 按钮是全局操作，不带 conv 概念）。
        "control" => {
            let cmd = v.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
            let args = v.get("args").and_then(|a| a.as_str()).unwrap_or("");
            let text = match cmd {
                "stop" => "/stop".to_string(),
                "resume" => "/resume".to_string(),
                "switch" => {
                    if args.is_empty() {
                        "/switch".to_string()
                    } else {
                        format!("/switch {args}")
                    }
                }
                _ => {
                    return InboundAction::Warn(
                        "control 帧 cmd 非法（须 stop|resume|switch），丢弃",
                    )
                }
            };
            InboundAction::Message(Box::new(synth_message(
                normalize_conv(None, default_session),
                text,
                None,
            )))
        }
        "sync" => InboundAction::Sync,
        "ping" => InboundAction::Ping,
        _ => InboundAction::Ignore,
    }
}

/// 测试/集成用：把原始 JSON 文本一步映射（解析失败归 Ignore——畸形帧不断连，
/// 与 client 侧 debug 容错一致）。
#[cfg(test)]
pub(crate) fn map_raw_frame(text: &str, default_session: &str) -> InboundAction {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => map_inbound_frame(&v, default_session),
        Err(_) => InboundAction::Ignore,
    }
}

#[cfg(test)]
mod tests {
    //! 帧协议纯函数单测（任务说明 §8）：入站映射 / 出站药化 / 截断。

    use super::*;
    use imagent_core::{is_explicit_reply_word, parse_reply, TodoItem, ToolCall};

    const SESSION: &str = "default";

    #[test]
    fn map_message_with_conv_id() {
        let act = map_raw_frame(
            r#"{"v":1,"type":"message","convId":"work","text":"列出目录"}"#,
            SESSION,
        );
        let InboundAction::Message(m) = act else {
            panic!("message 应映射为 Message：{act:?}")
        };
        assert_eq!(m.conv_id, ConvId("web:work".into()));
        assert_eq!(m.sender, UserId("web:user".into()));
        assert_eq!(m.text.as_deref(), Some("列出目录"));
        assert_eq!(m.ask_req, None);
        assert!(!m.no_steer, "用户手打消息走 steering");
        assert!(m.media.is_empty() && m.mentions.is_empty() && !m.mentioned_bot);
        assert!(matches!(m.reply_hint, ReplyHint::None));
        // 完整 conv 形态（SPA 原样回传出站帧 convId）→ 原样，不双前缀。
        let act = map_raw_frame(
            r#"{"v":1,"type":"message","convId":"web:work","text":"hi"}"#,
            SESSION,
        );
        let InboundAction::Message(m) = act else {
            panic!("完整 convId 的 message 应映射为 Message：{act:?}")
        };
        assert_eq!(m.conv_id, ConvId("web:work".into()));
    }

    /// convId 三形态归一：None / 空串 → 默认会话；完整 conv 原样（SPA 原样
    /// 回传出站帧 convId 的正常路径）；裸 session 名补前缀（兼容形态）。
    #[test]
    fn normalize_conv_three_forms() {
        assert_eq!(normalize_conv(None, "default"), "web:default");
        assert_eq!(normalize_conv(Some(""), "default"), "web:default");
        assert_eq!(normalize_conv(Some("web:work"), "default"), "web:work");
        assert_eq!(normalize_conv(Some("work"), "default"), "web:work");
    }

    #[test]
    fn map_message_without_conv_id_falls_to_default_session() {
        let act = map_raw_frame(r#"{"v":1,"type":"message","text":"hi"}"#, SESSION);
        let InboundAction::Message(m) = act else {
            panic!("应映射为 Message：{act:?}")
        };
        assert_eq!(m.conv_id, ConvId("web:default".into()));
        // 空 convId 同缺省。
        let act = map_raw_frame(
            r#"{"v":1,"type":"message","convId":"","text":"hi"}"#,
            SESSION,
        );
        assert!(matches!(act, InboundAction::Message(_)));
        // 缺 text → 丢弃。
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"message"}"#, SESSION),
            InboundAction::Warn(_)
        ));
    }

    /// 三决策词必须精确命中 permission.rs 词表（ask_req 锚定后 parse_reply
    /// 消费——词表漂移会让审批闭环静默失效，此处钉住）。
    #[test]
    fn map_approval_response_words_hit_vocabulary() {
        for (decision, word) in [("allow", "y"), ("deny", "n"), ("always", "always")] {
            let act = map_raw_frame(
                &format!(
                    r#"{{"v":1,"type":"approval_response","convId":"c1","requestId":"req-1","decision":"{decision}"}}"#
                ),
                SESSION,
            );
            let InboundAction::Message(m) = act else {
                panic!("{decision} 应映射为 Message")
            };
            assert_eq!(m.conv_id, ConvId("web:c1".into()));
            assert_eq!(m.ask_req.as_deref(), Some("req-1"));
            let text = m.text.unwrap();
            assert_eq!(text, word);
            assert!(
                is_explicit_reply_word(&text),
                "决策词 {word} 必须命中审批词表"
            );
            let reply = parse_reply(&text);
            match decision {
                "allow" => assert!(reply.allow && !reply.always),
                "deny" => assert!(!reply.allow),
                "always" => assert!(reply.allow && reply.always),
                _ => unreachable!(),
            }
        }
        // convId 必填：缺省丢弃。
        assert!(matches!(
            map_raw_frame(
                r#"{"v":1,"type":"approval_response","requestId":"r","decision":"allow"}"#,
                SESSION,
            ),
            InboundAction::Warn(_)
        ));
        // 非法 decision 丢弃。
        assert!(matches!(
            map_raw_frame(
                r#"{"v":1,"type":"approval_response","convId":"c","requestId":"r","decision":"maybe"}"#,
                SESSION,
            ),
            InboundAction::Warn(_)
        ));
    }

    /// 审批闭环协议对称性（必修回归钉）：浏览器**原样回传**出站
    /// approval_request 帧的 convId（完整 conv `web:work`，SPA 正常路径）与
    /// 裸 session 名（兼容形态）都必须归一到同一 conv `web:work`——否则
    /// PermissionRouter 按 conv 路由找不到 pending，审批闭环断裂。
    #[test]
    fn map_approval_response_accepts_full_and_bare_conv() {
        for conv_id in ["web:work", "work"] {
            let act = map_raw_frame(
                &format!(
                    r#"{{"v":1,"type":"approval_response","convId":"{conv_id}","requestId":"req-1","decision":"allow"}}"#
                ),
                SESSION,
            );
            let InboundAction::Message(m) = act else {
                panic!("convId={conv_id} 应映射为 Message：{act:?}")
            };
            assert_eq!(m.conv_id, ConvId("web:work".into()), "convId={conv_id}");
            assert_eq!(m.ask_req.as_deref(), Some("req-1"));
            assert_eq!(m.text.as_deref(), Some("y"));
        }
    }

    #[test]
    fn map_control_synthesizes_command_text() {
        let cases = [
            (r#"{"v":1,"type":"control","cmd":"stop"}"#, "/stop"),
            (r#"{"v":1,"type":"control","cmd":"resume"}"#, "/resume"),
            (
                r#"{"v":1,"type":"control","cmd":"switch","args":"web:proj-a"}"#,
                "/switch web:proj-a",
            ),
        ];
        for (raw, want) in cases {
            let act = map_raw_frame(raw, SESSION);
            let InboundAction::Message(m) = act else {
                panic!("{raw} 应映射为 Message")
            };
            assert_eq!(m.text.as_deref(), Some(want));
            assert_eq!(m.conv_id, ConvId("web:default".into()));
        }
        // 非法 cmd 丢弃。
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"control","cmd":"boom"}"#, SESSION),
            InboundAction::Warn(_)
        ));
    }

    #[test]
    fn map_sync_ping_and_unknown() {
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"sync"}"#, SESSION),
            InboundAction::Sync
        ));
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"ping"}"#, SESSION),
            InboundAction::Ping
        ));
        // 未知 type 前向兼容忽略；畸形 JSON 同样忽略（不断连）。
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"future-thing"}"#, SESSION),
            InboundAction::Ignore
        ));
        assert!(matches!(
            map_raw_frame("not json", SESSION),
            InboundAction::Ignore
        ));
    }

    #[test]
    fn text_frame_shape() {
        let json = encode_text_frame("web:work", "你好").unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "text");
        assert_eq!(v["convId"], "web:work");
        assert_eq!(v["text"], "你好");
        assert!(v.get("hint").is_none(), "hint 恒 None 不序列化");
    }

    fn sample_card(text: &str, terminal: CardTerminal) -> OutboundCard {
        OutboundCard {
            text: text.to_string(),
            tool_calls: vec![ToolCall {
                name: "Bash".into(),
                summary: "ls -la".into(),
                done: true,
                id: Some("t1".into()),
            }],
            thoughts: vec!["想一想".into()],
            todos: vec![TodoItem {
                id: Some("1".into()),
                text: "任务".into(),
                status: TodoStatus::InProgress,
            }],
            phase: CardPhase::Outputting,
            queued_hint: None,
            run_secs: 42,
            usage_display: Some("$0.012".into()),
            task_digest: Some("列出目录".into()),
            terminal,
        }
    }

    #[test]
    fn card_frame_shape_and_state_derivation() {
        let running = CardPayload::from_card(&sample_card("流式中", CardTerminal::Running));
        assert_eq!(running.state(), "streaming");
        let json = encode_card_frame("card", "web:work", "web-card-1", &running).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "card");
        assert_eq!(v["convId"], "web:work");
        assert_eq!(v["messageId"], "web-card-1");
        assert_eq!(v["state"], "streaming");
        assert_eq!(v["card"]["text"], "流式中");
        assert_eq!(v["card"]["phase"], "outputting");
        assert_eq!(v["card"]["terminal"], "running");
        assert_eq!(v["card"]["tool_calls"][0]["name"], "Bash");
        assert_eq!(v["card"]["todos"][0]["status"], "in_progress");
        assert_eq!(v["card"]["usage_display"], "$0.012");
        assert!(v["card"].get("queued_hint").is_none());

        // card_patch + 终态 state=done；Error 终态带 error 字段。
        let done = CardPayload::from_card(&sample_card("完成", CardTerminal::Done));
        assert_eq!(done.state(), "done");
        let json = encode_card_frame("card_patch", "web:work", "web-card-1", &done).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "card_patch");
        assert_eq!(v["state"], "done");
        assert!(v["card"].get("error").is_none());

        let err = CardPayload::from_card(&sample_card("炸了", CardTerminal::Error("boom".into())));
        assert_eq!(err.state(), "done");
        let json = encode_card_frame("card", "web:work", "web-card-1", &err).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["card"]["terminal"], "error");
        assert_eq!(v["card"]["error"], "boom");
    }

    #[test]
    fn approval_frames_shape() {
        let json =
            encode_approval_request_frame("web:c", "req-9", "Bash", "rm -rf /tmp/x").unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "approval_request");
        assert_eq!(v["convId"], "web:c");
        assert_eq!(v["requestId"], "req-9");
        assert_eq!(v["tool"], "Bash");
        assert_eq!(v["inputSummary"], "rm -rf /tmp/x");

        let json =
            encode_approval_resolved_frame("web:c", "req-9", "cancelled", Some("审批已取消"))
                .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "approval_resolved");
        assert_eq!(v["decision"], "cancelled");
        assert_eq!(v["message"], "审批已取消");
        // message 缺省不序列化。
        let json = encode_approval_resolved_frame("web:c", "req-9", "allow", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("message").is_none());
    }

    /// 截断：超 240KB 的 card 帧截断 card.text 保头尾（头 18000 / 尾 6000 char
    /// + 标记），截后帧 < 240KB。
    #[test]
    fn oversized_card_frame_truncated_head_tail() {
        // 25000 个中文 = 75000 字节 < 240KB 不触发；用 10 万中文字（300KB）触发。
        let long_text = "长".repeat(100_000); // 300_000 字节
        let card = CardPayload::from_card(&sample_card(&long_text, CardTerminal::Running));
        let json = encode_card_frame("card", "web:work", "web-card-1", &card).unwrap();
        assert!(
            json.len() <= FRAME_SOFT_LIMIT_BYTES,
            "截断后帧应 ≤240KB：{}",
            json.len()
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let text = v["card"]["text"].as_str().unwrap();
        assert!(text.contains("…[已截断]…"), "应有截断标记");
        assert!(text.starts_with(&"长".repeat(TRUNC_HEAD_CHARS)), "头部保窗");
        assert!(text.ends_with(&"长".repeat(TRUNC_TAIL_CHARS)), "尾部保窗");

        // text 帧同款截断。
        let json = encode_text_frame("web:work", &long_text).unwrap();
        assert!(json.len() <= FRAME_SOFT_LIMIT_BYTES);
        assert!(json.contains("…[已截断]…"));

        // 未超限帧原样通过。
        let small = encode_text_frame("web:work", "short").unwrap();
        assert!(small.contains("short"));
    }

    #[test]
    fn truncate_text_head_tail_window() {
        // 短文原样。
        assert_eq!(truncate_text("hello"), "hello");
        assert_eq!(truncate_text(""), "");
        // 超长：头尾保留 + 标记（构造恰好头+尾+1 char 的输入）。
        let mut text = String::new();
        text.push_str(&"a".repeat(TRUNC_HEAD_CHARS));
        text.push('X'); // 中段唯一字符，被截掉
        text.push_str(&"z".repeat(TRUNC_TAIL_CHARS));
        let out = truncate_text(&text);
        assert!(out.starts_with(&"a".repeat(TRUNC_HEAD_CHARS)));
        assert!(out.ends_with(&"z".repeat(TRUNC_TAIL_CHARS)));
        assert!(!out.contains('X'), "中段应被截断");
        assert!(out.contains("…[已截断]…"));
    }

    #[test]
    fn pong_frame_shape() {
        assert_eq!(encode_pong_frame(), r#"{"v":1,"type":"pong"}"#);
    }
}
