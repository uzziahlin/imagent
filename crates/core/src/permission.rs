//! IM 权限审批路由（Ask 闭环用）。
//!
//! 主进程侧：`PermissionRouter` 维护每个 conv 的 pending 权限请求（oneshot）。
//! - socket accept task 收到 MCP server 转发的权限请求 → `send_text` 询问用户 →
//!   `register(conv)` 等待回复；
//! - dispatch recv 循环发现某 conv 有 pending 请求时，把该 conv 的下一条入站消息
//!   当作 approve/deny 回复，`route(conv, reply)` 送达 oneshot，**不**走正常 handle。

use std::collections::HashMap;
use std::path::PathBuf;

use tokio::sync::{oneshot, Mutex};

/// 固定 socket 路径：`~/.imagent/permission.sock`。
pub fn default_sock_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".imagent").join("permission.sock"))
}


/// 用户的 approve/deny 回复。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionReply {
    pub allow: bool,
    pub message: Option<String>,
}

/// 解析用户回复文本为 approve/deny。
///
/// 规则（trim 后）：
/// - 空串 / 无法判定 → deny；
/// - 首字符 `y`/`Y` → allow；
/// - 等于 `是`/`允许`/`好`/`yes`/`ok`/`Y` → allow；
/// - 其它 → deny。
pub fn parse_reply(text: &str) -> PermissionReply {
    let t = text.trim();
    if t.is_empty() {
        return PermissionReply {
            allow: false,
            message: Some("empty reply".into()),
        };
    }
    let lower = t.to_ascii_lowercase();
    let allow = matches!(t.chars().next(), Some('y') | Some('Y'))
        || lower == "是"
        || lower == "允许"
        || lower == "好"
        || lower == "好的"
        || lower == "yes"
        || lower == "ok";
    PermissionReply {
        allow,
        message: if allow {
            None
        } else {
            Some(format!("denied by user reply: {t}"))
        },
    }
}

/// per-conv 权限请求路由表。
pub struct PermissionRouter {
    pending: Mutex<HashMap<String, oneshot::Sender<PermissionReply>>>,
}

impl PermissionRouter {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// 是否有 conv 处于等待回复状态。
    pub async fn has_pending(&self, conv_id: &str) -> bool {
        self.pending.lock().await.contains_key(conv_id)
    }

    /// 注册一个 pending 请求，返回 receiver 用于等待回复。
    /// 若该 conv 已有 pending，旧的 sender 被替换（旧 receiver 收到 drop 即返回错误）。
    pub async fn register(&self, conv_id: &str) -> oneshot::Receiver<PermissionReply> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(conv_id.to_string(), tx);
        rx
    }

    /// 投递回复给 pending 的 conv。返回 true 表示命中（该消息已被权限闭环消费）。
    pub async fn route(&self, conv_id: &str, reply: PermissionReply) -> bool {
        let mut map = self.pending.lock().await;
        if let Some(tx) = map.remove(conv_id) {
            // send 失败说明 receiver 已 drop（register 方未在等），视为未命中。
            tx.send(reply).is_ok()
        } else {
            false
        }
    }
}

impl Default for PermissionRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reply_allow_variants() {
        for s in ["y", "Y", "yes", "YES", "Yes", "ok", "OK", "是", "允许", "好", "好的"] {
            let r = parse_reply(s);
            assert!(r.allow, "should allow: {s:?}");
            assert!(r.message.is_none(), "no message when allow: {s:?}");
        }
    }

    #[test]
    fn parse_reply_deny_variants() {
        for s in ["", "   ", "n", "N", "no", "不", "拒绝", "随便", "rm -rf /"] {
            let r = parse_reply(s);
            assert!(!r.allow, "should deny: {s:?}");
        }
    }

    #[test]
    fn parse_reply_deny_has_message() {
        let r = parse_reply("no way");
        assert!(!r.allow);
        assert!(r.message.unwrap().contains("no way"));
    }

    #[tokio::test]
    async fn router_register_route_hit() {
        let r = PermissionRouter::new();
        assert!(!r.has_pending("c1").await);
        let rx = r.register("c1").await;
        assert!(r.has_pending("c1").await);
        let hit = r
            .route(
                "c1",
                PermissionReply {
                    allow: true,
                    message: None,
                },
            )
            .await;
        assert!(hit);
        assert!(!r.has_pending("c1").await);
        let reply = rx.await.unwrap();
        assert!(reply.allow);
    }

    #[tokio::test]
    async fn router_route_miss_when_no_pending() {
        let r = PermissionRouter::new();
        let hit = r
            .route(
                "c2",
                PermissionReply {
                    allow: false,
                    message: None,
                },
            )
            .await;
        assert!(!hit);
    }
}
