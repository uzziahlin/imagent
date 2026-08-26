//! IM 权限审批路由（Ask 闭环用）。
//!
//! 主进程侧：`PermissionRouter` 维护每个 conv 的 pending 权限请求（oneshot）。
//! - socket accept task 收到 MCP server 转发的权限请求 → `send_text` 询问用户 →
//!   `register(conv, request_id)` 等待回复；
//! - dispatch recv 循环发现某 conv 有 pending 请求时，把该 conv 的下一条入站消息
//!   当作 approve/deny 回复，`route(conv, …)` 送达 oneshot，**不**走正常 handle。
//!
//! 多 pending 并存（终端 ask_via_im 改造）：key 为 `conv + request_id`，同 conv
//! 下终端 agent 的提问与 IM 会话的审批互不顶替；回复路由三级——按钮回调带
//! request_id 精确匹配 → 引用回复（parent 消息 id 命中询问卡）→ 最新 pending 兜底。

use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::{oneshot, Mutex};

/// 单 conv 允许的 pending 上限：防泄漏（异常路径漏 cancel 时兜底收敛最旧的）。
const PENDING_PER_CONV_CAP: usize = 8;

/// 固定 socket 路径：`<imagent_home>/permission.sock`（P4-10：随 profile 隔离）。
pub fn default_sock_path() -> Option<PathBuf> {
    Some(crate::paths::imagent_home().join("permission.sock"))
}

/// 审批集条目匹配工具名：精确相等，或条目以 `*` 结尾时按前缀匹配
/// （`mcp__*` 命中所有 MCP 工具）。空格/大小写敏感（工具名本就如此）。
pub fn tool_matches_pattern(pattern: &str, tool_name: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        !prefix.is_empty() && tool_name.starts_with(prefix)
    } else {
        pattern == tool_name
    }
}

/// 该工具是否需要 IM 审批：审批集为空 = 全部过审（既有语义）；非空 = 仅清单内过审。
pub fn needs_approval(approval_tools: &[String], tool_name: &str) -> bool {
    approval_tools.is_empty()
        || approval_tools
            .iter()
            .any(|p| tool_matches_pattern(p, tool_name))
}

/// 用户的 approve/deny 回复。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionReply {
    pub allow: bool,
    pub message: Option<String>,
    /// 用户回复的**原文**（按钮回调为 `ask:<选项>` 展开、自由文本为原文）。
    /// 权限路径不读它（allow/deny 语义不变）；ask_via_im 路径以它作为用户答案回传。
    pub raw_text: Option<String>,
}

/// 解析用户回复文本为 approve/deny。
///
/// 规则（trim 后）：
/// - 空串 / 无法判定 → deny；
/// - 精确匹配常见 allow 词（`y`/`yes`/`ok`/`是`/`允许`/`好`/`可以`/`行`/`没问题` 等）→ allow；
/// - 其它 → deny（fail-closed）。
///
/// P2-G：不再用「首字符 y/Y」宽匹配（旧逻辑会把 year/yellow/yesterday 误判 allow，
/// 对权限 approve/deny 是真实安全 bug）。P2-12：补中文确认词（用户回复「可以」
/// 「行」「没问题」不再被误 deny）。
pub fn parse_reply(text: &str) -> PermissionReply {
    let t = text.trim();
    if t.is_empty() {
        return PermissionReply {
            allow: false,
            message: Some("empty reply".into()),
            raw_text: None,
        };
    }
    // P6（AskUserQuestion 答案路由）：问题卡的选项按钮回调转成 "ask:<选项>"。
    // 语义 = 不执行内建工具（headless 下它没有交互面），选择经 message 回给
    // agent —— deny + message 是权限协议里 agent 能读到用户输入的唯一通道。
    if let Some(choice) = t.strip_prefix("ask:") {
        let choice = choice.trim();
        if !choice.is_empty() {
            return PermissionReply {
                allow: false,
                message: Some(format!("用户选择：{choice}")),
                raw_text: Some(format!("用户选择：{choice}")),
            };
        }
    }
    let lower = t.to_ascii_lowercase();
    // P2-G：去掉「首字符 y/Y」宽匹配（旧逻辑会把 year/yeah/yellow/yesterday 等
    // 误判为 allow，对权限 approve/deny 是真实安全 bug）。改为精确匹配常见 allow 词。
    // P2-12：补中文高频确认词（「可以」「行」「没问题」等），降低中文用户误 deny 率。
    let allow = matches!(
        lower.as_str(),
        "y" | "yes"
            | "ye"
            | "yep"
            | "yeah"
            | "ok"
            | "okay"
            | "是"
            | "允许"
            | "好"
            | "好的"
            | "可以"
            | "行"
            | "没问题"
            | "好呀"
            | "行吧"
            | "可以吧"
            | "嗯"
    );
    PermissionReply {
        allow,
        message: if allow {
            None
        } else {
            Some(format!("denied by user reply: {t}"))
        },
        raw_text: Some(t.to_string()),
    }
}

/// 单条 pending 询问。
struct PendingAsk {
    request_id: String,
    /// 询问卡的 IM 侧消息 id（自由文本引用回复的路由锚点；文本询问为 None）。
    card_msg_id: Option<String>,
    tx: oneshot::Sender<PermissionReply>,
}

/// per-conv × request_id 权限请求路由表（多 pending 并存）。
pub struct PermissionRouter {
    pending: Mutex<HashMap<String, Vec<PendingAsk>>>,
}

impl PermissionRouter {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// 是否有 conv 处于等待回复状态。
    pub async fn has_pending(&self, conv_id: &str) -> bool {
        self.pending
            .lock()
            .await
            .get(conv_id)
            .is_some_and(|v| !v.is_empty())
    }

    /// 注册一个 pending 请求，返回 receiver 用于等待回复。
    ///
    /// 同 request_id 重复注册会顶替旧条目（旧等待者立即收到 superseded deny）；
    /// 不同 request_id 并存（终端 ask 与 IM 审批互不干扰）。per-conv 超过上限时
    /// 最旧的按超时收敛（异常路径漏 cancel 的兜底）。
    pub async fn register(
        &self,
        conv_id: &str,
        request_id: &str,
        card_msg_id: Option<String>,
    ) -> oneshot::Receiver<PermissionReply> {
        let (tx, rx) = oneshot::channel();
        let entry = PendingAsk {
            request_id: request_id.to_string(),
            card_msg_id,
            tx,
        };
        let mut map = self.pending.lock().await;
        let list = map.entry(conv_id.to_string()).or_default();
        if let Some(i) = list.iter().position(|p| p.request_id == request_id) {
            let old = list.remove(i);
            let _ = old.tx.send(PermissionReply {
                allow: false,
                message: Some("superseded（同一请求被重新发起）".into()),
                raw_text: None,
            });
        }
        list.push(entry);
        while list.len() > PENDING_PER_CONV_CAP {
            let oldest = list.remove(0);
            let _ = oldest.tx.send(PermissionReply {
                allow: false,
                message: Some("cancelled（pending 超上限，最旧询问被收敛）".into()),
                raw_text: None,
            });
        }
        rx
    }

    /// 投递回复给 pending 请求，三级路由：
    /// 1. `req_hint`（按钮回调携带的 request_id）精确匹配；
    /// 2. `parent_msg_id`（自由文本引用回复的目标消息 id）命中询问卡；
    /// 3. 两者皆缺时最新 pending 兜底。
    ///
    /// req/parent **给了但未命中**视为未命中（陈旧回调/无关引用不得劫持别的
    /// pending，消息回落正常处理路径）。
    pub async fn route(
        &self,
        conv_id: &str,
        req_hint: Option<&str>,
        parent_msg_id: Option<&str>,
        reply: PermissionReply,
    ) -> Option<String> {
        let mut map = self.pending.lock().await;
        let list = map.get_mut(conv_id)?;
        let idx = match (req_hint, parent_msg_id) {
            (Some(req), _) => list.iter().position(|p| p.request_id == req)?,
            (None, Some(mid)) => list
                .iter()
                .position(|p| p.card_msg_id.as_deref() == Some(mid))?,
            (None, None) => list.len().checked_sub(1)?,
        };
        let hit = list.remove(idx);
        if list.is_empty() {
            map.remove(conv_id);
        }
        // send 失败说明 receiver 已 drop（register 方未在等），视为未命中。
        hit.tx.send(reply).ok().map(|_| hit.request_id)
    }

    /// 清理单个 pending（超时 / router-drop 路径）：投递 deny（fail-closed）唤醒
    /// 等待者。send 失败 = receiver 已 drop（等待方先超时），无害。
    pub async fn cancel(&self, conv_id: &str, request_id: &str) {
        let mut map = self.pending.lock().await;
        let Some(list) = map.get_mut(conv_id) else {
            return;
        };
        if let Some(i) = list.iter().position(|p| p.request_id == request_id) {
            let old = list.remove(i);
            if list.is_empty() {
                map.remove(conv_id);
            }
            drop(map);
            let _ = old.tx.send(PermissionReply {
                allow: false,
                message: Some("cancelled（任务被 /stop 中断或审批超时）".into()),
                raw_text: None,
            });
        }
    }

    /// 清理该 conv 的**全部** pending（/stop 路径）：逐个投递 deny 唤醒等待者，
    /// 返回被清理的 request_id 列表（调用方据此收敛询问卡）。
    pub async fn cancel_all(&self, conv_id: &str) -> Vec<String> {
        let removed = self
            .pending
            .lock()
            .await
            .remove(conv_id)
            .unwrap_or_default();
        removed
            .into_iter()
            .map(|p| {
                let _ = p.tx.send(PermissionReply {
                    allow: false,
                    message: Some("cancelled（任务被 /stop 中断或审批超时）".into()),
                    raw_text: None,
                });
                p.request_id
            })
            .collect()
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

    #[tokio::test]
    async fn cancel_removes_pending() {
        // P1-8：cancel 清理 pending，避免超时/router-drop 残留累积。
        let r = PermissionRouter::new();
        let _rx = r.register("conv1", "req1", None).await;
        assert!(r.has_pending("conv1").await);
        r.cancel("conv1", "req1").await;
        assert!(!r.has_pending("conv1").await);
    }

    /// P5-16：cancel 唤醒等待者并 fail-closed 回 deny——不再挂满
    /// permission_ask_timeout 才超时。
    #[tokio::test]
    async fn cancel_waits_no_more_denies_waiter() {
        let r = PermissionRouter::new();
        let rx = r.register("conv1", "req1", None).await;
        r.cancel("conv1", "req1").await;
        // 等待者应立即（而非超时后）收到 deny。
        let reply = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("cancel 应立即唤醒等待者")
            .expect("sender 未 drop");
        assert!(!reply.allow, "cancel 必须 fail-closed deny");
        assert!(reply.message.unwrap().contains("cancelled"));
    }

    /// 多 pending 并存：同 conv 不同 request_id 互不顶替，按 req 精确路由。
    #[tokio::test]
    async fn multi_pending_routes_by_request_id() {
        let r = PermissionRouter::new();
        let rx_im = r.register("c", "im-1", None).await;
        let rx_term = r.register("c", "t-1", None).await;
        // 按钮/回调带 req=t-1 → 只唤醒终端一路。
        let hit = r
            .route(
                "c",
                Some("t-1"),
                None,
                PermissionReply {
                    allow: false,
                    message: None,
                    raw_text: Some("用户选择：B".into()),
                },
            )
            .await;
        assert_eq!(hit.as_deref(), Some("t-1"));
        let term = tokio::time::timeout(std::time::Duration::from_secs(1), rx_term)
            .await
            .expect("t-1 应被唤醒")
            .unwrap();
        assert_eq!(term.raw_text.as_deref(), Some("用户选择：B"));
        // IM 那路仍在等待，且成为唯一 pending（后续兜底路由命中它）。
        assert!(r.has_pending("c").await);
        let hit2 = r
            .route(
                "c",
                None,
                None,
                PermissionReply {
                    allow: true,
                    message: None,
                    raw_text: Some("y".into()),
                },
            )
            .await;
        assert_eq!(hit2.as_deref(), Some("im-1"));
        assert!(rx_im.await.unwrap().allow);
        assert!(!r.has_pending("c").await);
    }

    /// 引用回复：parent 消息 id 命中对应询问卡（card_msg_id 锚点）。
    #[tokio::test]
    async fn parent_msg_id_routes_to_matching_card() {
        let r = PermissionRouter::new();
        let _old = r.register("c", "im-1", Some("om_old".to_string())).await;
        let _rx_new = r.register("c", "t-1", Some("om_new".to_string())).await;
        let hit = r
            .route(
                "c",
                None,
                Some("om_old"),
                PermissionReply {
                    allow: true,
                    message: None,
                    raw_text: Some("y".into()),
                },
            )
            .await;
        assert_eq!(hit.as_deref(), Some("im-1"), "引用旧卡应路由 im-1 而非最新");
        assert!(r.has_pending("c").await, "t-1 不应被消费");
    }

    /// 同 request_id 重复注册：旧的被顶替（superseded deny），不占两个槽位。
    #[tokio::test]
    async fn reregister_same_request_id_supersedes() {
        let r = PermissionRouter::new();
        let rx_old = r.register("c", "req1", None).await;
        let _rx_new = r.register("c", "req1", None).await;
        let old = tokio::time::timeout(std::time::Duration::from_secs(1), rx_old)
            .await
            .expect("旧等待者应立即被唤醒")
            .unwrap();
        assert!(!old.allow);
        assert!(old.message.unwrap().contains("superseded"));
        // 只剩一个 pending（t-1 未被顶掉）。
        assert!(r.has_pending("c").await);
    }

    /// /stop：cancel_all 清理全部并唤醒所有等待者。
    #[tokio::test]
    async fn cancel_all_wakes_every_waiter() {
        let r = PermissionRouter::new();
        let rx1 = r.register("c", "a", None).await;
        let rx2 = r.register("c", "b", None).await;
        let ids = r.cancel_all("c").await;
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
        for rx in [rx1, rx2] {
            let reply = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
                .await
                .expect("应立即唤醒")
                .unwrap();
            assert!(!reply.allow);
        }
        assert!(!r.has_pending("c").await);
    }

    #[test]
    fn parse_reply_allow_variants() {
        for s in [
            "y",
            "Y",
            "yes",
            "YES",
            "Yes",
            "ok",
            "OK",
            "是",
            "允许",
            "好",
            "好的",
            "可以",
            "行",
            "没问题",
            "好呀",
            "行吧",
            "可以吧",
            "嗯",
        ] {
            let r = parse_reply(s);
            assert!(r.allow, "should allow: {s:?}");
            assert!(r.message.is_none(), "no message when allow: {s:?}");
        }
    }

    /// P6：ask: 前缀 = 问题卡选项答案 → deny + message（agent 经 message 读到选择）。
    #[test]
    fn parse_reply_ask_prefix_carries_choice() {
        let r = parse_reply("ask:先做数据库迁移");
        assert!(!r.allow);
        assert_eq!(r.message.as_deref(), Some("用户选择：先做数据库迁移"));
        // 空 ask: 不当答案（回落正常 deny 路径）。
        let r2 = parse_reply("ask:");
        assert!(r2.message.is_none() || r2.message.as_deref() != Some("用户选择："));
    }

    #[test]
    fn parse_reply_deny_variants() {
        for s in ["", "   ", "n", "N", "no", "不", "拒绝", "随便", "rm -rf /"] {
            let r = parse_reply(s);
            assert!(!r.allow, "should deny: {s:?}");
        }
    }

    #[test]
    fn parse_reply_year_not_allowed() {
        // P2-G：首字符 y 但非 allow 词必须 deny（旧「首字符 y/Y」宽匹配会误 allow，
        // 对权限 approve/deny 是真实安全 bug）。
        for s in ["year", "yellow", "yesterday", "yeah no", "y?", "y3"] {
            let r = parse_reply(s);
            assert!(!r.allow, "应 deny（首字符 y 但非 allow 词）: {s:?}");
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
        let rx = r.register("c1", "req1", None).await;
        assert!(r.has_pending("c1").await);
        let hit = r
            .route(
                "c1",
                Some("req1"),
                None,
                PermissionReply {
                    allow: true,
                    message: None,
                    raw_text: None,
                },
            )
            .await;
        assert!(hit.is_some());
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
                None,
                None,
                PermissionReply {
                    allow: false,
                    message: None,
                    raw_text: None,
                },
            )
            .await;
        assert!(hit.is_none());
    }
}

#[cfg(test)]
mod approval_set_tests {
    use super::*;

    #[test]
    fn pattern_matching() {
        assert!(tool_matches_pattern("Bash", "Bash"));
        assert!(!tool_matches_pattern("Bash", "BashOutput"));
        assert!(tool_matches_pattern(
            "mcp__*",
            "mcp__imagent__permission_request"
        ));
        assert!(!tool_matches_pattern("mcp__*", "Bash"));
        // 裸 "*" 不视为全匹配（防误配成「什么都不审」）；空条目同理。
        assert!(!tool_matches_pattern("*", "Bash"));
        assert!(!tool_matches_pattern("", "Bash"));
    }

    #[test]
    fn needs_approval_semantics() {
        // 空集 = 全部过审（既有语义）。
        assert!(needs_approval(&[], "Bash"));
        let set = vec!["Bash".to_string(), "mcp__*".to_string()];
        assert!(needs_approval(&set, "Bash"));
        assert!(needs_approval(&set, "mcp__x__y"));
        // 集外 = 放行。
        assert!(!needs_approval(&set, "Write"));
        assert!(!needs_approval(&set, "WebFetch"));
    }
}
