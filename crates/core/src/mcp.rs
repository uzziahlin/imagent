//! MCP server over stdio（claude `--permission-prompt-tool` 回调目标）。
//!
//! claude 遇需权限的工具时，通过 MCP JSON-RPC 2.0 调用名为 `permission_request`
//! 的工具。本模块实现一个最小 stdio server：
//! - `initialize` → 返回协议版本 + capabilities + serverInfo；
//! - `tools/list` → 返回单个工具 `permission_request`；
//! - `tools/call(name=permission_request)` → 依 `PermissionMode` 返回 allow/deny：
//!   - `Allow`/`Deny`：固定策略，立即返回；
//!   - `Ask`：通过 unix socket 请求主进程路由到 IM，阻塞等待用户回复；
//!   - `Off`：不应到达（Off 时不挂 MCP），按 deny 兜底。
//!
//! 纯函数 `build_tools_list` / `build_call_response` 便于单测；真实 socket 连接
//! 在 `run_mcp_server` 中包。

use std::io;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, warn};

use crate::config::PermissionMode;
use crate::permission::PermissionReply;

pub const TOOL_NAME: &str = "permission_request";

/// `--mcp-config` 里注册的 server 名（backend 写配置时用）。与 [`TOOL_NAME`] 一起
/// 决定 claude 眼中的工具全名。
pub const SERVER_NAME: &str = "imagent";

/// claude `--permission-prompt-tool` 需要的 **server 限定全名**
/// （`mcp__<server>__<tool>`；真机校准 2026-08：claude CLI 2.1.x 只认全名，裸
/// 工具名报 "MCP tool not found. Available MCP tools: mcp__imagent__permission_request"）。
pub fn qualified_tool_name() -> String {
    format!("mcp__{SERVER_NAME}__{TOOL_NAME}")
}

const PROTOCOL_VERSION: &str = "2024-11-05";

/// `tools/list` 的工具描述（纯函数，便于单测）。
pub fn build_tools_list() -> Value {
    json!({
        "tools": [{
            "name": TOOL_NAME,
            "description": "IM 权限审批：claude 遇需权限的工具时回调，由 imagent 转交 IM 用户 approve/deny。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tool_name": { "type": "string", "description": "请求授权的工具名（如 Bash）" },
                    "input": { "type": "object", "description": "工具入参" }
                },
                "required": ["tool_name"]
            }
        }]
    })
}

/// 由回复构造 `tools/call` 的结果（纯函数，便于单测）。
///
/// claude 期望（约定）：`{behavior: "allow"|"deny", updatedInput?: {...}, message?: "..."}`
/// 序列化为 MCP `content[0].text`。
pub fn build_call_response(reply: &PermissionReply, updated_input: &Value) -> Value {
    let mut payload = json!({
        "behavior": if reply.allow { "allow" } else { "deny" },
        "updatedInput": updated_input,
    });
    if let Some(msg) = &reply.message {
        payload["message"] = json!(msg);
    }
    json!({
        "content": [ { "type": "text", "text": payload.to_string() } ]
    })
}

/// 构造固定策略回复（Allow/Deny 模式）。
pub fn fixed_reply(mode: PermissionMode) -> PermissionReply {
    match mode {
        PermissionMode::Allow => PermissionReply {
            allow: true,
            message: None,
            raw_text: None,
        },
        PermissionMode::Deny => PermissionReply {
            allow: false,
            message: Some("denied by imagent permission_mode=deny".into()),
            raw_text: None,
        },
        // Off / Ask 不应走固定策略；兜底 deny。
        _ => PermissionReply {
            allow: false,
            message: Some("imagent permission mode does not allow".into()),
            raw_text: None,
        },
    }
}

/// 处理单个 JSON-RPC 请求（不涉及 socket；Ask 模式的真实 roundtrip 在 server 循环里）。
///
/// 返回要写回 stdout 的 JSON-RPC 响应（已含 `id`）。通知（无 `id`）返回 None。
/// `params_for_call` 是 Ask 模式下需要 socket roundtrip 时调用的回调；纯函数版本
/// 传入 `None` 时，Ask 按 deny 兜底（便于单测）。
pub fn handle_request(req: &Value, mode: PermissionMode) -> Option<Value> {
    let id = req.get("id")?;
    let method = req.get("method")?.as_str()?;
    let result: Value = match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "imagent-permission", "version": env!("CARGO_PKG_VERSION") }
        }),
        "tools/list" => build_tools_list(),
        "tools/call" => {
            let name = req
                .pointer("/params/name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if name != TOOL_NAME {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": format!("unknown tool: {name}") }
                }));
            }
            // 纯函数版本：Allow/Deny 固定；Ask/Off 兜底 deny（真实 Ask roundtrip 在 server 循环）。
            let reply = if matches!(mode, PermissionMode::Allow | PermissionMode::Deny) {
                fixed_reply(mode)
            } else {
                PermissionReply {
                    allow: false,
                    message: Some("ask/off not handled in pure handler".into()),
                    raw_text: None,
                }
            };
            let input = req
                .pointer("/params/arguments/input")
                .cloned()
                .unwrap_or(json!({}));
            build_call_response(&reply, &input)
        }
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") }
            }));
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// 从 tools/call 请求里提取 (tool_name, input)。
pub fn extract_call_args(req: &Value) -> (String, Value) {
    let tool_name = req
        .pointer("/params/arguments/tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let input = req
        .pointer("/params/arguments/input")
        .cloned()
        .unwrap_or_else(|| {
            // claude 可能直接把工具参数平铺在 arguments 里（无 input 包裹）。
            req.pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}))
        });
    (tool_name, input)
}

/// Ask 模式：经 unix socket 请求主进程路由，阻塞等待回复。
///
/// 协议（一行 JSON 请求 / 一行 JSON 回复）：
/// - 请求：`{ "conv_id": "...", "request_id": "...", "tool_name": "...", "input": {...} }`
/// - 回复：`{ "allow": bool, "message": null|string }`
pub async fn ask_via_socket(
    sock: &str,
    conv_id: &str,
    request_id: &str,
    tool_name: &str,
    input: &Value,
    ask_timeout: std::time::Duration,
) -> io::Result<PermissionReply> {
    let mut stream = UnixStream::connect(sock).await?;
    // P5-9b：握手 token——主进程 bind socket 时生成并写 <sock_dir>/permission.token
    //（0600）。连接首行必须是 token，不符即被丢弃（把同 uid 进程裸 connect 伪造
    // conv_id 推送审批钓鱼的门槛从零提高到需读到 token）。文件缺失（主进程写
    // 失败/旧版）按空串发送，主进程侧 fail-closed 拒绝。
    let token_path = std::path::Path::new(sock)
        .parent()
        .map(|d| d.join("permission.token"))
        .unwrap_or_else(|| std::path::PathBuf::from("permission.token"));
    let token = std::fs::read_to_string(&token_path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if token.is_empty() {
        tracing::warn!(
            target: "imagent::mcp",
            ?token_path,
            "permission.token 缺失，握手将被主进程拒绝（fail-closed）"
        );
    }
    stream.write_all(format!("{token}\n").as_bytes()).await?;
    stream.flush().await?;
    let req = json!({
        "conv_id": conv_id,
        "request_id": request_id,
        "tool_name": tool_name,
        "input": input,
    });
    let line = format!("{req}\n");
    stream.write_all(line.as_bytes()).await?;
    stream.flush().await?;

    let mut reader = BufReader::new(&mut stream);
    let mut buf = String::new();
    // P1-6/S-3：read_line 加超时——主进程异常（dispatcher task 被 cancel/panic 但未写回
    // socket）时，mcp 子进程不会在 socket 上永久挂死变僵尸。ask_timeout 由主进程经
    // --ask-timeout 传入（= config.permission_ask_timeout_secs），与 dispatcher 的审批
    // 等待预算对齐——防 MCP 先于 dispatcher 超时返 deny 使 Ask 闭环静默失效。
    match tokio::time::timeout(ask_timeout, reader.read_line(&mut buf)).await {
        Ok(res) => {
            res?;
        }
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("permission reply timed out (>{ask_timeout:?})"),
            ));
        }
    }
    let v: Value = serde_json::from_str(buf.trim())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("parse reply: {e}")))?;
    let allow = v.get("allow").and_then(|a| a.as_bool()).unwrap_or(false);
    let message = v
        .get("message")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());
    Ok(PermissionReply {
        allow,
        message,
        raw_text: None,
    })
}

/// 生成 request_id（多 pending 路由 key）：`<prefix>-<hex>`。
fn new_request_id(prefix: &str) -> String {
    format!(
        "{prefix}-{:08x}{:08x}",
        rand::random::<u32>(),
        rand::random::<u32>()
    )
}

/// MCP server 主循环（stdio）。读 stdin 一行 JSON、写 stdout 一行 JSON。
pub async fn run_mcp_server(
    conv_id: String,
    sock: String,
    mode: PermissionMode,
    ask_timeout: std::time::Duration,
) -> io::Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break, // EOF（claude 关闭 stdin）
            Err(e) => {
                warn!(target: "imagent::mcp", error = %e, "stdin read error");
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                debug!(target: "imagent::mcp", raw = trimmed, error = %e, "ignore non-json line");
                continue;
            }
        };

        // tools/call 在 Ask 模式需要 socket roundtrip；其它走纯 handler。
        let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let resp = if method == "tools/call" && matches!(mode, PermissionMode::Ask) {
            let (tool_name, input) = extract_call_args(&req);
            // 多 pending：每次调用独立 request_id（同 conv 与其它询问并存互不顶替）。
            let request_id = new_request_id("p");
            let reply = match ask_via_socket(
                &sock,
                &conv_id,
                &request_id,
                &tool_name,
                &input,
                ask_timeout,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => PermissionReply {
                    allow: false,
                    message: Some(format!("imagent socket error: {e}")),
                    raw_text: None,
                },
            };
            let result = build_call_response(&reply, &input);
            let id = req.get("id").cloned().unwrap_or(Value::Null);
            json!({ "jsonrpc": "2.0", "id": id, "result": result })
        } else {
            match handle_request(&req, mode) {
                Some(v) => v,
                None => continue, // 通知（无 id），不回
            }
        };

        let mut out = resp.to_string();
        out.push('\n');
        if let Err(e) = stdout.write_all(out.as_bytes()).await {
            warn!(target: "imagent::mcp", error = %e, "stdout write error");
            break;
        }
        let _ = stdout.flush().await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ask_via_im：面向终端 agent 的通用「问人」MCP server（`imagent mcp-ask`）。
// ---------------------------------------------------------------------------

/// 工具名（终端 agent 调用）。
pub const ASK_TOOL_NAME: &str = "ask_via_im";

/// `source` 参数的字符上限（卡片标题空间有限）。
const ASK_SOURCE_MAX_CHARS: usize = 48;

/// 清洗 source 标签：去首尾/内部多余空白（折成单空格，卡片单行展示）+ 截断。
fn sanitize_source(raw: &str) -> String {
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(ASK_SOURCE_MAX_CHARS).collect()
}

/// 问题正文的来源前缀：有 source 标出「哪个 agent」，无则通用标记。
/// 多 agent 共用同一 conv 时用户靠它区分提问方。
pub fn ask_source_prefix(source: Option<&str>) -> String {
    match source.map(sanitize_source).filter(|s| !s.is_empty()) {
        Some(s) => format!("💻（终端 agent · {s}）"),
        None => "💻（终端 agent 提问）".to_string(),
    }
}

/// `--print-config` 输出的 mcpServers 配置 JSON（一键挂到任意 MCP client）。
pub fn mcp_servers_config(exe: &str) -> String {
    json!({
        "mcpServers": {
            "imagent": { "command": exe, "args": ["mcp-ask"] }
        }
    })
    .to_string()
}

/// `tools/list`（纯函数，便于单测）。
pub fn build_ask_tools_list() -> Value {
    json!({
        "tools": [{
            "name": ASK_TOOL_NAME,
            "description": "向用户的 IM（飞书）发送问题并阻塞等待回复。适合需要用户决策/确认而用户可能不在终端前的场景；用户在 IM 点选项按钮或回复文字，答案作为工具结果返回。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "问题正文（可含决策所需的上下文摘要，支持多行 markdown）" },
                    "options": {
                        "type": "array", "items": { "type": "string" }, "maxItems": 8,
                        "description": "可选的选项按钮（用户也可直接回复自由文本）"
                    },
                    "source": {
                        "type": "string", "maxLength": 48,
                        "description": "提问方标记（如项目名/机器名），显示在问题卡标题——多 agent 并发时用于区分"
                    },
                    "timeout_secs": { "type": "integer", "description": "等待超时（秒），缺省用服务端默认" }
                },
                "required": ["question"]
            }
        }]
    })
}

/// `kind = "ask"` 的 socket roundtrip：发问题 → 阻塞等回复。
///
/// 返回 `Ok(Ok(text))` = 用户回复原文；`Ok(Err(err))` = 主进程侧错误
/// （超时/发送失败等，文案可直读）；`Err(io)` = socket 层失败（主进程未运行等）。
pub async fn ask_user_via_socket(
    sock: &str,
    conv_id: &str,
    request_id: &str,
    question: &str,
    options: &[String],
    source: Option<&str>,
    timeout_secs: u64,
) -> io::Result<std::result::Result<String, String>> {
    let mut stream = UnixStream::connect(sock).await?;
    let token_path = std::path::Path::new(sock)
        .parent()
        .map(|d| d.join("permission.token"))
        .unwrap_or_else(|| std::path::PathBuf::from("permission.token"));
    let token = std::fs::read_to_string(&token_path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    stream.write_all(format!("{token}\n").as_bytes()).await?;
    stream.flush().await?;
    let req = json!({
        "kind": "ask",
        "conv_id": conv_id,
        "request_id": request_id,
        "question": question,
        "options": options,
        "source": source,
        "timeout_secs": timeout_secs,
    });
    stream.write_all(format!("{req}\n").as_bytes()).await?;
    stream.flush().await?;

    let mut reader = BufReader::new(&mut stream);
    let mut buf = String::new();
    // 读超时 = 等待超时 + 60s 余量（主进程回写前还有卡片发送等开销）。
    let budget = std::time::Duration::from_secs(timeout_secs.saturating_add(60));
    match tokio::time::timeout(budget, reader.read_line(&mut buf)).await {
        Ok(res) => {
            res?;
        }
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("ask reply timed out (>{budget:?})"),
            ))
        }
    }
    let v: Value = serde_json::from_str(buf.trim())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("parse reply: {e}")))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        return Ok(Err(err.to_string()));
    }
    let text = v
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(Ok(text))
}

/// ask server 主循环（stdio）。`imagent mcp-ask` 子命令入口——供任意终端 agent
/// 作为 MCP server 挂载；tools/call 走 `kind:"ask"` socket 协议到主进程。
pub async fn run_ask_mcp_server(
    conv_id: String,
    sock: String,
    default_timeout: std::time::Duration,
) -> io::Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(e) => {
                warn!(target: "imagent::mcp", error = %e, "stdin read error");
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                debug!(target: "imagent::mcp", raw = trimmed, error = %e, "ignore non-json line");
                continue;
            }
        };
        let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let resp: Value = match method {
            "initialize" => json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "imagent-ask", "version": env!("CARGO_PKG_VERSION") }
            }),
            "tools/list" => build_ask_tools_list(),
            "tools/call" => {
                let name = req
                    .pointer("/params/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if name != ASK_TOOL_NAME {
                    json!({
                        "jsonrpc": "2.0", "id": req.get("id").cloned().unwrap_or(Value::Null),
                        "error": { "code": -32602, "message": format!("unknown tool: {name}") }
                    })
                } else {
                    let args = req
                        .pointer("/params/arguments")
                        .cloned()
                        .unwrap_or(json!({}));
                    let question = args
                        .get("question")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let options: Vec<String> = args
                        .get("options")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|o| o.as_str())
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default();
                    let source = args
                        .get("source")
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    let timeout_secs = args
                        .get("timeout_secs")
                        .and_then(|v| v.as_u64())
                        .filter(|s| (1..=86_400).contains(s))
                        .unwrap_or(default_timeout.as_secs());
                    let request_id = new_request_id("t");
                    let id = req.get("id").cloned().unwrap_or(Value::Null);
                    if question.is_empty() {
                        json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {
                                "content": [ { "type": "text", "text": "question 不能为空" } ],
                                "isError": true
                            }
                        })
                    } else {
                        let text: String = match ask_user_via_socket(
                            &sock,
                            &conv_id,
                            &request_id,
                            &question,
                            &options,
                            source.as_deref(),
                            timeout_secs,
                        )
                        .await
                        {
                            Ok(Ok(text)) => text,
                            Ok(Err(err)) => format!("ask_via_im 失败：{err}"),
                            // 主进程未运行（connect refused / token 缺失）给出可操作提示。
                            Err(e) => format!(
                                "imagent 主进程不可达（{e}）——请确认 `imagent start feishu` 已在运行"
                            ),
                        };
                        let is_error = text.starts_with("ask_via_im 失败")
                            || text.starts_with("imagent 主进程不可达");
                        json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {
                                "content": [ { "type": "text", "text": text } ],
                                "isError": is_error
                            }
                        })
                    }
                }
            }
            _ => {
                if req.get("id").is_none() {
                    continue; // 通知，不回
                }
                json!({
                    "jsonrpc": "2.0", "id": req.get("id").cloned().unwrap_or(Value::Null),
                    "error": { "code": -32601, "message": format!("method not found: {method}") }
                })
            }
        };
        let mut out = resp.to_string();
        out.push('\n');
        if let Err(e) = stdout.write_all(out.as_bytes()).await {
            warn!(target: "imagent::mcp", error = %e, "stdout write error");
            break;
        }
        let _ = stdout.flush().await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_list_has_permission_request() {
        let list = build_tools_list();
        let tools = list.get("tools").and_then(|t| t.as_array()).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], TOOL_NAME);
    }

    /// ask server 的 tools/list 只暴露 ask_via_im。
    #[test]
    fn ask_tools_list_has_ask_via_im() {
        let list = build_ask_tools_list();
        let tools = list.get("tools").and_then(|t| t.as_array()).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], ASK_TOOL_NAME);
        assert_eq!(
            tools[0]["inputSchema"]["required"][0], "question",
            "question 必填"
        );
        assert!(
            tools[0]["inputSchema"]["properties"]["source"].is_object(),
            "source 可选参数应在 schema 中"
        );
    }

    /// source 前缀：清洗空白、截断；无 source 用通用标记。
    #[test]
    fn ask_source_prefix_sanitizes_and_falls_back() {
        assert_eq!(
            ask_source_prefix(Some(" imagent  项目 ")),
            "💻（终端 agent · imagent 项目）",
            "内部多余空白折成单空格"
        );
        let long = "很".repeat(200);
        let p = ask_source_prefix(Some(&long));
        assert!(p.chars().count() < long.chars().count(), "超长应截断");
        assert_eq!(ask_source_prefix(Some("   ")), "💻（终端 agent 提问）");
        assert_eq!(ask_source_prefix(None), "💻（终端 agent 提问）");
    }

    /// --print-config 的 mcpServers JSON：command 是实际二进制、args 带子命令。
    #[test]
    fn mcp_servers_config_shape() {
        let raw = mcp_servers_config("/usr/local/bin/imagent");
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v["mcpServers"]["imagent"]["command"],
            "/usr/local/bin/imagent"
        );
        assert_eq!(v["mcpServers"]["imagent"]["args"][0], "mcp-ask");
    }

    /// 真机校准（claude CLI 2.1.156）：--permission-prompt-tool 只认 server 限定
    /// 全名。回归防线：全名格式与 backend 写入 mcp-config 的 server 名联动。
    #[test]
    fn qualified_tool_name_matches_claude_mcp_naming() {
        assert_eq!(qualified_tool_name(), "mcp__imagent__permission_request");
    }

    #[test]
    fn call_response_allow() {
        let reply = PermissionReply {
            allow: true,
            message: None,
            raw_text: None,
        };
        let resp = build_call_response(&reply, &json!({"command": "ls"}));
        let text = resp["content"][0]["text"].as_str().unwrap();
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["behavior"], "allow");
        assert_eq!(payload["updatedInput"]["command"], "ls");
        assert!(payload.get("message").is_none());
    }

    #[test]
    fn call_response_deny_with_message() {
        let reply = PermissionReply {
            allow: false,
            message: Some("nope".into()),
            raw_text: None,
        };
        let resp = build_call_response(&reply, &json!({}));
        let text = resp["content"][0]["text"].as_str().unwrap();
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["behavior"], "deny");
        assert_eq!(payload["message"], "nope");
    }

    #[test]
    fn handle_initialize_returns_capabilities() {
        let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" });
        let resp = handle_request(&req, PermissionMode::Allow).unwrap();
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn handle_tools_list() {
        let req = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
        let resp = handle_request(&req, PermissionMode::Allow).unwrap();
        assert_eq!(resp["result"]["tools"][0]["name"], TOOL_NAME);
    }

    #[test]
    fn handle_call_allow_mode() {
        let req = json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": TOOL_NAME, "arguments": { "tool_name": "Bash", "input": {"command":"ls"} } }
        });
        let resp = handle_request(&req, PermissionMode::Allow).unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["behavior"], "allow");
    }

    #[test]
    fn handle_call_deny_mode() {
        let req = json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": TOOL_NAME, "arguments": { "tool_name": "Bash" } }
        });
        let resp = handle_request(&req, PermissionMode::Deny).unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["behavior"], "deny");
    }

    #[test]
    fn handle_unknown_method_returns_error() {
        let req = json!({ "jsonrpc": "2.0", "id": 5, "method": "foo/bar" });
        let resp = handle_request(&req, PermissionMode::Allow).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn handle_notification_no_response() {
        let req = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle_request(&req, PermissionMode::Allow).is_none());
    }
}
