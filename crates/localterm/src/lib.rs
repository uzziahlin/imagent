//! `imagent-localterm`：`imagent claude` 本地终端——网关侧
//! [`LocalPlatform`](platform::LocalPlatform) + 终端客户端 [`term`](term)。
//!
//! # 拓扑（定案）
//!
//! ```text
//! imagent claude [会话名]        （TUI 客户端进程，rustyline 行编辑）
//!    ↕ ~/.imagent/local.sock    （unix socket，JSON 行帧——与 web 帧协议 v1.2 同构）
//! LocalPlatform（gateway 内，crates/localterm/src/platform.rs）
//!    ↓ Dispatcher 全管线（会话池/steering/审批/stop 既有能力，零新增）
//!    ↕ MultiPlatform：platforms = ["feishu","web","local"] 并存——
//!      web conv 走 web、local conv 出站帧路由回终端
//! ```
//!
//! # 三个关键设计决策（理由即文档）
//!
//! 1. **终端走文本流路径**：[`LocalPlatform`](platform::LocalPlatform) 的
//!    `supports_streaming_card` 恒 false——core dispatch（round.rs）据此不建
//!    CardSession，agent 的 Text chunk 经 400ms 合帧窗口逐段 `send_text` 到
//!    终端（贴近裸 claude CLI 的流式体感）。实测 core 文本路径对 tool_use /
//!    TodoList 的展示行为：**轮次中不出站工具行**（ToolUse/ToolResult 只累积
//!    状态），最终回复自动追加 `\n\n🔧 工具调用：Bash — xxx，…` 与
//!    `\n\n📋 计划进度：N/M 完成`——终端经 text 帧自然受益，platform 侧不动，
//!    不为此改 core。
//! 2. **TUI 是视图不是宿主**：`imagent claude` 的 Ctrl-C / exit / Ctrl-D 只断开
//!    终端视图（关 socket），会话继续在 gateway 跑——重连 `imagent claude
//!    <名>` 可续看（sync 帧重发最近文本缓存），这是本设计的核心价值。
//! 3. **帧协议与 web v1.2 同构，但本地免 token**：信封同为 `{"v":1,"type":…}`
//!    的 JSON 行帧；差异仅认证与机器语义——本地 socket 文件 0600 + **peer uid
//!    校验**即凭证（同 uid = 机器本主，hello 无 token 字段；web 的
//!    token/machine 字段属 relay 多机场景，本地单机免带，帧内其余字段同形）。
//!
//! 本模块承载帧类型定义与纯函数（入站映射、出站序列化、conv 归一），全部
//! 无 IO 便于单测；网络循环在 [`platform`]，终端交互在 [`term`]。映射函数
//! 本 crate 自写（不跨 crate 依赖 imagent-web——web 的 `normalize_conv` 会把
//! 一切裸名前缀成 `web:`，语义不同）。

#![deny(unsafe_code)]
// 局部 unsafe 豁免（peer uid 的 getsockopt）在 platform.rs，同 core
// dispatch::socket 先例：deny 允许显式豁免 + SAFETY 注释。

pub mod platform;
pub mod term;

pub use platform::LocalPlatform;

/// 统一 tracing target（仓库纪律）。
pub(crate) const TARGET: &str = "imagent::localterm";

/// 平台名（= conv 前缀路由键，与 `platforms = [... "local"]` 名单一致）。
pub const PLATFORM_NAME: &str = "local";

/// conv 前缀：终端会话形如 `local:<会话名>`。
pub const CONV_PREFIX: &str = "local:";

/// 终端合成消息的 sender id。装配层（main）在挂 local 平台时自动加入
/// allowed_senders / admin_senders——本地 socket peer uid 校验已确认同 uid
/// （机器本主），与 CLI 操作者同级，无需再走 IM 白名单 bootstrap。
pub const SENDER: &str = "local:user";

/// 协议版本（信封 `v` 字段；帧形态对齐 web v1.2）。
pub const PROTOCOL_VERSION: u8 = 1;

// ---------------------------------------------------------------------------
// 会话名与 conv
// ---------------------------------------------------------------------------

/// 会话名长度上限（防帧滥用；终端会话名非路径，64 char 足够表达）。
pub const SESSION_MAX_CHARS: usize = 64;

/// 会话名规范化（纯函数便于单测）：trim 后非空、≤64 char、不含 `:`
/// （conv 前缀路由分隔符——会话名带冒号会造出 `local:a:b` 破坏归一唯一性）
/// 与控制字符。非法返回 None（hello 握手据此拒绝连接）。
pub fn sanitize_session_name(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.chars().count() > SESSION_MAX_CHARS {
        return None;
    }
    if t.contains(':') || t.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(t.to_string())
}

/// 会话名 → 完整 conv（`local:<session>`）。
pub fn conv_of(session: &str) -> String {
    format!("{CONV_PREFIX}{session}")
}

/// 帧内 convId 归一（与 web 的 normalize_conv 同族，但前缀为 local 且**拒绝
/// 外平台前缀**）：
/// - None / 空串 → 该连接 hello 声明的会话 `local:<session>`；
/// - 裸会话名（无 `:`）→ `local:<名>`；
/// - `local:<名>` → 原样（不双前缀）；
/// - 其它含 `:` 形态（如 `web:x` / `feishu:oc_x`）→ Err——本地终端只操作
///   local 会话，跨平台插话是 web SPA 的演进面（现版 SPA 无对应 UI，且 web
///   侧 normalize_conv 已透传 local: 前缀——网页可直发 local 会话，回执经
///   MultiPlatform 镜像路由双投 local+web）。
pub(crate) fn normalize_conv(
    raw: Option<&str>,
    session: &str,
) -> std::result::Result<String, &'static str> {
    let raw = raw.map(str::trim).filter(|c| !c.is_empty());
    match raw {
        None => Ok(conv_of(session)),
        Some(c) if !c.contains(':') => Ok(conv_of(c)),
        Some(c) => match c.strip_prefix(CONV_PREFIX).filter(|r| !r.is_empty()) {
            Some(_) => Ok(c.to_string()),
            None => Err("convId 含 ':' 但非 local: 前缀——本地终端只操作 local:* 会话"),
        },
    }
}

// ---------------------------------------------------------------------------
// 入站帧（终端 → 网关）映射
// ---------------------------------------------------------------------------

/// 入站帧分拣结果（serve_conn 消费；纯函数便于单测，形态对齐 web crate）。
#[derive(Debug)]
pub(crate) enum InboundAction {
    /// 业务消息（message / approval_response / control）→ inbound channel →
    /// `Platform::recv`。（Box 消除变体间大 size 差——InboundMessage 含多个
    /// Vec，clippy large_enum_variant。）
    Message(Box<InagentInboundMessage>),
    /// sync 请求 → 连接任务直接重发文本缓存（不进 recv）。
    Sync,
    /// 未知 type / 畸形 JSON：debug 忽略——前向兼容 + 杂帧不断连。
    Ignore,
    /// 协议违规 / 缺必填字段：warn 原因并丢弃（不进 recv）。
    Warn(&'static str),
}

/// 入站合成消息类型别名（避免与 InboundAction::Message 变体重名遮蔽）。
use imagent_core::InboundMessage as InagentInboundMessage;

/// `approval_response.decision` → 审批词表精确词（对齐 web 先例）：allow→`y`、
/// deny→`n`、always→`always`（`imagent_core::permission` 词表均含——ask_req
/// 锚定后 parse_reply 消费，词必须精确命中）。
fn decision_word(decision: &str) -> Option<&'static str> {
    match decision {
        "allow" => Some("y"),
        "deny" => Some("n"),
        "always" => Some("always"),
        _ => None,
    }
}

/// 合成 InboundMessage 公共骨架（字段全集对照 web crate synth_message：终端
/// 手打消息走 steering——no_steer=false，与 IM 用户同权）。
fn synth_message(conv: String, text: String, ask_req: Option<String>) -> InagentInboundMessage {
    InagentInboundMessage {
        conv_id: imagent_core::ConvId(conv),
        sender: imagent_core::UserId(SENDER.to_string()),
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
        reply_hint: imagent_core::ReplyHint::None,
    }
}

/// 入站业务帧映射（纯函数）。`session` 为该连接 hello 声明的会话名（无
/// convId 帧的落点）。畸形由调用方先行丢弃；本函数只处理合法 JSON Value。
pub(crate) fn map_inbound_frame(v: &serde_json::Value, session: &str) -> InboundAction {
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        // message：text 必填；convId 缺省/裸名/完整 local: 形态见 normalize_conv。
        "message" => {
            let Some(text) = v.get("text").and_then(|t| t.as_str()) else {
                return InboundAction::Warn("message 帧缺 text 字段，丢弃");
            };
            match normalize_conv(v.get("convId").and_then(|c| c.as_str()), session) {
                Ok(conv) => {
                    InboundAction::Message(Box::new(synth_message(conv, text.to_string(), None)))
                }
                Err(reason) => InboundAction::Warn(reason),
            }
        }
        // approval_response：convId 可缺省（终端连接 1:1 绑定 hello 会话，比
        // web 的必填宽松）；requestId / decision 必填，决策词须精确命中词表。
        "approval_response" => {
            let Some(request_id) = v.get("requestId").and_then(|r| r.as_str()) else {
                return InboundAction::Warn("approval_response 帧缺 requestId，丢弃");
            };
            let Some(decision) = v.get("decision").and_then(|d| d.as_str()) else {
                return InboundAction::Warn("approval_response 帧缺 decision，丢弃");
            };
            let Some(word) = decision_word(decision) else {
                return InboundAction::Warn(
                    "approval_response 帧 decision 非法（须 allow|deny|always），丢弃",
                );
            };
            match normalize_conv(v.get("convId").and_then(|c| c.as_str()), session) {
                Ok(conv) => InboundAction::Message(Box::new(synth_message(
                    conv,
                    word.to_string(),
                    Some(request_id.to_string()),
                ))),
                Err(reason) => InboundAction::Warn(reason),
            }
        }
        // control：转命令文本（与 IM 按钮点击同机制——走既有鉴权/分派）。
        // conv 落 hello 会话（终端按钮是本会话操作；/stop 无参数时中断本会话）。
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
            InboundAction::Message(Box::new(synth_message(conv_of(session), text, None)))
        }
        "sync" => InboundAction::Sync,
        _ => InboundAction::Ignore,
    }
}

/// 测试用：原始 JSON 文本一步映射（解析失败归 Ignore——畸形帧不断连，与
/// serve_conn 侧 debug 容错一致；对齐 web crate 的 map_raw_frame）。
#[cfg(test)]
pub(crate) fn map_raw_frame(text: &str, session: &str) -> InboundAction {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => map_inbound_frame(&v, session),
        Err(_) => InboundAction::Ignore,
    }
}

// ---------------------------------------------------------------------------
// 出站帧（网关 → 终端）
// ---------------------------------------------------------------------------

use serde::Serialize;

/// 文本帧（send_text 药化）。convId 恒为完整 conv（`local:<名>`）；无 web 的
/// machine 字段（无 relay 多机场景）、无 240KB 截断（无 relay 单帧上限——
/// unix socket 本地大帧无碍，长文本拆分由 core 的 IM 平台侧 split_message
/// 语义负责，本地终端整体接收）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TextFrame {
    pub(crate) v: u8,
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    #[serde(rename = "convId")]
    pub(crate) conv_id: String,
    pub(crate) text: String,
}

/// 审批请求帧（send_permission_ask 药化；形状对齐 web，inputSummary 已压成
/// 人可读单行）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ApprovalRequestFrame {
    pub(crate) v: u8,
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    #[serde(rename = "convId")]
    pub(crate) conv_id: String,
    #[serde(rename = "requestId")]
    pub(crate) request_id: String,
    pub(crate) tool: String,
    #[serde(rename = "inputSummary")]
    pub(crate) input_summary: String,
}

/// 审批收敛帧（resolve / cancel / cancel_all）。decision ∈ allow|deny|always
/// （用户真实决定）| cancelled（系统取消——超时/顶替//stop）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ApprovalResolvedFrame {
    pub(crate) v: u8,
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    #[serde(rename = "convId")]
    pub(crate) conv_id: String,
    #[serde(rename = "requestId")]
    pub(crate) request_id: String,
    pub(crate) decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
}

/// hello_ack（握手确认，回显会话名）。
#[derive(Debug, Serialize)]
pub(crate) struct HelloAckFrame {
    pub(crate) v: u8,
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) session: String,
}

/// 错误帧（踢旧 4001 / peer uid 拒绝 4003 / 握手违规 4002——code 值对齐 relay
/// close code 语义：4001 顶替、4xxx 协议层拒绝）。
#[derive(Debug, Serialize)]
pub(crate) struct ErrorFrame {
    pub(crate) v: u8,
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) code: &'static str,
    pub(crate) message: &'static str,
}

pub(crate) fn encode_text_frame(conv_id: &str, text: &str) -> imagent_core::Result<String> {
    serde_json::to_string(&TextFrame {
        v: PROTOCOL_VERSION,
        kind: "text",
        conv_id: conv_id.to_string(),
        text: text.to_string(),
    })
    .map_err(|e| imagent_core::CoreError::Platform(TARGET, format!("text 帧序列化失败：{e}")))
}

pub(crate) fn encode_approval_request_frame(
    conv_id: &str,
    request_id: &str,
    tool: &str,
    input_summary: &str,
) -> imagent_core::Result<String> {
    serde_json::to_string(&ApprovalRequestFrame {
        v: PROTOCOL_VERSION,
        kind: "approval_request",
        conv_id: conv_id.to_string(),
        request_id: request_id.to_string(),
        tool: tool.to_string(),
        input_summary: input_summary.to_string(),
    })
    .map_err(|e| imagent_core::CoreError::Platform(TARGET, format!("approval 帧序列化失败：{e}")))
}

pub(crate) fn encode_approval_resolved_frame(
    conv_id: &str,
    request_id: &str,
    decision: &str,
    message: Option<&str>,
) -> imagent_core::Result<String> {
    serde_json::to_string(&ApprovalResolvedFrame {
        v: PROTOCOL_VERSION,
        kind: "approval_resolved",
        conv_id: conv_id.to_string(),
        request_id: request_id.to_string(),
        decision: decision.to_string(),
        message: message.map(String::from),
    })
    .map_err(|e| {
        imagent_core::CoreError::Platform(TARGET, format!("approval_resolved 帧序列化失败：{e}"))
    })
}

pub(crate) fn encode_hello_ack(session: &str) -> imagent_core::Result<String> {
    serde_json::to_string(&HelloAckFrame {
        v: PROTOCOL_VERSION,
        kind: "hello_ack",
        session: session.to_string(),
    })
    .map_err(|e| imagent_core::CoreError::Platform(TARGET, format!("hello_ack 序列化失败：{e}")))
}

pub(crate) fn encode_error_frame(code: &'static str, message: &'static str) -> String {
    // 纯 &'static str 字段，序列化不可失败；兜底手拼保证错误帧恒可发。
    serde_json::to_string(&ErrorFrame {
        v: PROTOCOL_VERSION,
        kind: "error",
        code,
        message,
    })
    .unwrap_or_else(|_| format!(r#"{{"v":1,"type":"error","code":"{code}"}}"#))
}

/// resolve 药化的决策词（PermissionReply → 帧内 decision；对齐 web platform）。
pub(crate) fn resolved_decision(reply: &imagent_core::PermissionReply) -> &'static str {
    if reply.always {
        "always"
    } else if reply.allow {
        "allow"
    } else {
        "deny"
    }
}

#[cfg(test)]
mod tests {
    //! 帧协议纯函数单测：入站映射 / conv 归一 / 出站形状 / 会话名规范化。

    use super::*;
    use imagent_core::{is_explicit_reply_word, parse_reply};

    const SESSION: &str = "work";

    // ---------------- 会话名 ----------------

    #[test]
    fn session_name_sanitization() {
        assert_eq!(sanitize_session_name("work"), Some("work".into()));
        assert_eq!(sanitize_session_name("  spaced  "), Some("spaced".into()));
        assert_eq!(sanitize_session_name(""), None, "空串");
        assert_eq!(sanitize_session_name("   "), None, "纯空白");
        assert_eq!(
            sanitize_session_name("a:b"),
            None,
            "冒号（conv 前缀分隔符）"
        );
        assert_eq!(sanitize_session_name("web:x"), None, "外平台前缀形态");
        assert_eq!(
            sanitize_session_name(&"x".repeat(SESSION_MAX_CHARS + 1)),
            None,
            "超长"
        );
        assert_eq!(
            sanitize_session_name(&"x".repeat(SESSION_MAX_CHARS)),
            Some("x".repeat(SESSION_MAX_CHARS)),
            "恰好上限"
        );
        assert_eq!(sanitize_session_name("a\nb"), None, "控制字符");
    }

    // ---------------- conv 归一 ----------------

    #[test]
    fn normalize_conv_forms() {
        // 缺省 → hello 会话。
        assert_eq!(normalize_conv(None, SESSION), Ok(conv_of(SESSION)));
        assert_eq!(normalize_conv(Some(""), SESSION), Ok(conv_of(SESSION)));
        // 裸会话名 → 补前缀。
        assert_eq!(
            normalize_conv(Some("other"), SESSION),
            Ok("local:other".into())
        );
        // 完整 local: 形态原样（不双前缀）。
        assert_eq!(
            normalize_conv(Some("local:other"), SESSION),
            Ok("local:other".into())
        );
        // 空段（"local:"）拒绝。
        assert!(normalize_conv(Some("local:"), SESSION).is_err());
        // 外平台前缀拒绝（本地终端只操作 local 会话）。
        for foreign in ["web:x", "feishu:oc_x", "ilink:u1"] {
            assert!(
                normalize_conv(Some(foreign), SESSION).is_err(),
                "{foreign} 应被拒绝"
            );
        }
    }

    // ---------------- message ----------------

    #[test]
    fn map_message_defaults_to_hello_session() {
        let act = map_raw_frame(r#"{"v":1,"type":"message","text":"列出目录"}"#, SESSION);
        let InboundAction::Message(m) = act else {
            panic!("应映射为 Message：{act:?}")
        };
        assert_eq!(m.conv_id.0, "local:work");
        assert_eq!(m.sender.0, SENDER);
        assert_eq!(m.text.as_deref(), Some("列出目录"));
        assert_eq!(m.ask_req, None);
        assert!(!m.no_steer, "终端手打消息走 steering");
        assert!(m.media.is_empty() && m.mentions.is_empty() && !m.mentioned_bot);

        // convId 裸会话名 → local:other（同连接可插话本平台其它会话）。
        let act = map_raw_frame(
            r#"{"v":1,"type":"message","convId":"other","text":"hi"}"#,
            SESSION,
        );
        let InboundAction::Message(m) = act else {
            panic!("应映射为 Message：{act:?}")
        };
        assert_eq!(m.conv_id.0, "local:other");

        // 缺 text → 丢弃；外平台 convId → 丢弃。
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"message"}"#, SESSION),
            InboundAction::Warn(_)
        ));
        assert!(matches!(
            map_raw_frame(
                r#"{"v":1,"type":"message","convId":"web:x","text":"hi"}"#,
                SESSION
            ),
            InboundAction::Warn(_)
        ));
    }

    // ---------------- approval_response ----------------

    /// 三决策词必须精确命中 permission.rs 词表（ask_req 锚定后 parse_reply
    /// 消费——词表漂移会让审批闭环静默失效，此处钉住）。
    #[test]
    fn map_approval_response_three_decisions_hit_vocabulary() {
        for (decision, word) in [("allow", "y"), ("deny", "n"), ("always", "always")] {
            let act = map_raw_frame(
                &format!(
                    r#"{{"v":1,"type":"approval_response","requestId":"req-1","decision":"{decision}"}}"#
                ),
                SESSION,
            );
            let InboundAction::Message(m) = act else {
                panic!("{decision} 应映射为 Message：{act:?}")
            };
            assert_eq!(m.conv_id.0, "local:work", "convId 缺省落 hello 会话");
            assert_eq!(m.ask_req.as_deref(), Some("req-1"));
            let text = m.text.clone().unwrap();
            assert_eq!(text, word);
            assert!(is_explicit_reply_word(&text), "决策词须命中审批词表");
            let reply = parse_reply(&text);
            match decision {
                "allow" => assert!(reply.allow && !reply.always),
                "deny" => assert!(!reply.allow),
                "always" => assert!(reply.allow && reply.always),
                _ => unreachable!(),
            }
        }
        // convId 回显完整 conv（终端按帧内 convId 原样回传）→ 原样。
        let act = map_raw_frame(
            r#"{"v":1,"type":"approval_response","convId":"local:work","requestId":"r","decision":"deny"}"#,
            SESSION,
        );
        let InboundAction::Message(m) = act else {
            panic!("应映射为 Message：{act:?}")
        };
        assert_eq!(m.conv_id.0, "local:work");
        assert_eq!(m.text.as_deref(), Some("n"));
        // 必填缺失 / 非法 decision → 丢弃。
        assert!(matches!(
            map_raw_frame(
                r#"{"v":1,"type":"approval_response","decision":"allow"}"#,
                SESSION
            ),
            InboundAction::Warn(_)
        ));
        assert!(matches!(
            map_raw_frame(
                r#"{"v":1,"type":"approval_response","requestId":"r","decision":"maybe"}"#,
                SESSION
            ),
            InboundAction::Warn(_)
        ));
    }

    // ---------------- control ----------------

    #[test]
    fn map_control_synthesizes_command_text() {
        let cases = [
            (r#"{"v":1,"type":"control","cmd":"stop"}"#, "/stop"),
            (r#"{"v":1,"type":"control","cmd":"resume"}"#, "/resume"),
            (
                r#"{"v":1,"type":"control","cmd":"switch","args":"proj-a"}"#,
                "/switch proj-a",
            ),
        ];
        for (raw, want) in cases {
            let act = map_raw_frame(raw, SESSION);
            let InboundAction::Message(m) = act else {
                panic!("{raw} 应映射为 Message")
            };
            assert_eq!(m.text.as_deref(), Some(want));
            assert_eq!(m.conv_id.0, "local:work", "control 落 hello 会话");
        }
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"control","cmd":"boom"}"#, SESSION),
            InboundAction::Warn(_)
        ));
    }

    // ---------------- sync / 未知 ----------------

    #[test]
    fn map_sync_and_unknown() {
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"sync"}"#, SESSION),
            InboundAction::Sync
        ));
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"future-thing"}"#, SESSION),
            InboundAction::Ignore
        ));
        assert!(matches!(
            map_raw_frame("not json", SESSION),
            InboundAction::Ignore
        ));
    }

    // ---------------- 出站帧形状 ----------------

    #[test]
    fn text_frame_shape() {
        let json = encode_text_frame("local:work", "你好").unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "text");
        assert_eq!(v["convId"], "local:work");
        assert_eq!(v["text"], "你好");
        assert!(
            v.get("machine").is_none(),
            "本地无 relay 多机语义，不带 machine"
        );
        assert!(v.get("hint").is_none());
    }

    #[test]
    fn approval_frames_shape() {
        let json =
            encode_approval_request_frame("local:work", "req-9", "Bash", "rm -rf /tmp/x").unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "approval_request");
        assert_eq!(v["convId"], "local:work");
        assert_eq!(v["requestId"], "req-9");
        assert_eq!(v["tool"], "Bash");
        assert_eq!(v["inputSummary"], "rm -rf /tmp/x");

        let json =
            encode_approval_resolved_frame("local:work", "req-9", "cancelled", Some("审批已取消"))
                .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "approval_resolved");
        assert_eq!(v["decision"], "cancelled");
        assert_eq!(v["message"], "审批已取消");
        // message 缺省不序列化。
        let json = encode_approval_resolved_frame("local:work", "req-9", "allow", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("message").is_none());
    }

    #[test]
    fn hello_ack_and_error_frame_shape() {
        let json = encode_hello_ack("work").unwrap();
        assert_eq!(json, r#"{"v":1,"type":"hello_ack","session":"work"}"#);
        let json = encode_error_frame("4001", "被新连接顶替");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["code"], "4001");
        assert_eq!(v["message"], "被新连接顶替");
    }

    /// resolve 决策词映射（PermissionReply → allow|deny|always）。
    #[test]
    fn resolved_decision_mapping() {
        use imagent_core::PermissionReply;
        let reply = |allow: bool, always: bool| PermissionReply {
            allow,
            always,
            message: None,
            raw_text: None,
            cancelled: false,
        };
        assert_eq!(resolved_decision(&reply(true, false)), "allow");
        assert_eq!(resolved_decision(&reply(true, true)), "always");
        assert_eq!(resolved_decision(&reply(false, false)), "deny");
    }
}
