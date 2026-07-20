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
        },
        PermissionMode::Deny => PermissionReply {
            allow: false,
            message: Some("denied by imagent permission_mode=deny".into()),
        },
        // Off / Ask 不应走固定策略；兜底 deny。
        _ => PermissionReply {
            allow: false,
            message: Some("imagent permission mode does not allow".into()),
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
/// - 请求：`{ "conv_id": "...", "tool_name": "...", "input": {...} }`
/// - 回复：`{ "allow": bool, "message": null|string }`
pub async fn ask_via_socket(
    sock: &str,
    conv_id: &str,
    tool_name: &str,
    input: &Value,
    ask_timeout: std::time::Duration,
) -> io::Result<PermissionReply> {
    let mut stream = UnixStream::connect(sock).await?;
    let req = json!({
        "conv_id": conv_id,
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
    Ok(PermissionReply { allow, message })
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
            let reply = match ask_via_socket(&sock, &conv_id, &tool_name, &input, ask_timeout).await
            {
                Ok(r) => r,
                Err(e) => PermissionReply {
                    allow: false,
                    message: Some(format!("imagent socket error: {e}")),
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

    #[test]
    fn call_response_allow() {
        let reply = PermissionReply {
            allow: true,
            message: None,
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
