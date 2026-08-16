//! 发送者白名单鉴权（双态）。
//!
//! 白名单非空时按白名单过滤；为空时进入「发现模式」（P1：只打日志记录入站
//! sender，C1 起对非白名单 sender 回一条引导消息，不驱动 agent），便于首次使用时
//! 收集真实 sender id。
//!
//! C1：白名单运行时可变。内部用 `Arc<RwLock<HashSet>>` 共享，短临界区、不跨 `.await`，
//! 保持同步 API 不破坏调用点（dispatch 全程同步调用）。`Clone` 后共享底层集合（Arc）。

use std::collections::HashSet;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::types::UserId;

/// P2-H：归一化 sender id（trim 首尾空白；不改大小写——IM userid 大小写语义
/// 未知，保守不转换）。保证 config 白名单与入站 sender 比较一致。
fn normalize_sender(s: &str) -> String {
    s.trim().to_string()
}

#[derive(Debug, Clone)]
pub struct Auth {
    allowed: Arc<RwLock<HashSet<String>>>,
    /// 会话（群）白名单（P4-5）：存 conv_id 原样。群消息「chat 放行 OR sender
    /// 放行」即过——群维度授权后无需逐个 allow 成员；任何人仍受 admin/命令层约束。
    allowed_chats: Arc<RwLock<HashSet<String>>>,
}

impl Auth {
    /// 用配置的 `allowed_senders` 构造（会话白名单为空）。空 vec = 发现模式。
    pub fn new(allowed_senders: Vec<String>) -> Self {
        Self::with_chats(allowed_senders, Vec::new())
    }

    /// 带会话白名单构造（P4-5）。两个列表都空 = 发现模式。
    pub fn with_chats(allowed_senders: Vec<String>, allowed_chats: Vec<String>) -> Self {
        Self {
            allowed: Arc::new(RwLock::new(
                allowed_senders
                    .into_iter()
                    .map(|s| normalize_sender(&s))
                    .collect(),
            )),
            allowed_chats: Arc::new(RwLock::new(
                allowed_chats
                    .into_iter()
                    .map(|s| normalize_sender(&s))
                    .collect(),
            )),
        }
    }

    /// 两个白名单都为空 => 发现模式。
    pub fn is_discovery(&self) -> bool {
        self.allowed.read().is_empty() && self.allowed_chats.read().is_empty()
    }

    pub fn is_allowed(&self, uid: &UserId) -> bool {
        self.allowed.read().contains(&normalize_sender(&uid.0))
    }

    /// 会话是否在白名单（P4-5：群维度放行）。
    pub fn is_chat_allowed(&self, conv_id: &str) -> bool {
        self.allowed_chats
            .read()
            .contains(&normalize_sender(conv_id))
    }

    /// 加入白名单，返回是否新增（已存在返回 false）。
    pub fn allow(&self, sender: &str) -> bool {
        self.allowed.write().insert(normalize_sender(sender))
    }

    /// 移除，返回是否原本存在。
    pub fn revoke(&self, sender: &str) -> bool {
        self.allowed.write().remove(&normalize_sender(sender))
    }

    /// 加入会话白名单，返回是否新增。
    pub fn allow_chat(&self, conv_id: &str) -> bool {
        self.allowed_chats.write().insert(normalize_sender(conv_id))
    }

    /// 移除会话白名单，返回是否原本存在。
    pub fn revoke_chat(&self, conv_id: &str) -> bool {
        self.allowed_chats
            .write()
            .remove(&normalize_sender(conv_id))
    }

    /// 用新白名单整体替换（SIGHUP 热重载用）。清空后整体写入新集合。
    pub fn reload(&self, new_senders: Vec<String>) {
        let mut g = self.allowed.write();
        g.clear();
        g.extend(new_senders.into_iter().map(|s| normalize_sender(&s)));
    }

    /// 会话白名单整体替换（SIGHUP 热重载用；config 种子 ∪ store 动态授权）。
    pub fn reload_chats(&self, new_chats: Vec<String>) {
        let mut g = self.allowed_chats.write();
        g.clear();
        g.extend(new_chats.into_iter().map(|s| normalize_sender(&s)));
    }

    /// 当前白名单快照（按 id 升序）。
    pub fn snapshot(&self) -> Vec<String> {
        let s = self.allowed.read();
        let mut v: Vec<String> = s.iter().cloned().collect();
        v.sort();
        v
    }

    /// 会话白名单快照（升序）。
    pub fn snapshot_chats(&self) -> Vec<String> {
        let s = self.allowed_chats.read();
        let mut v: Vec<String> = s.iter().cloned().collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_then_is_allowed() {
        let a = Auth::new(vec!["alice".into()]);
        assert!(a.is_allowed(&UserId("alice".into())));
        assert!(!a.is_allowed(&UserId("bob".into())));

        assert!(a.allow("bob"));
        assert!(a.is_allowed(&UserId("bob".into())));
        // 重复 allow 返回 false。
        assert!(!a.allow("bob"));
    }

    #[test]
    fn revoke_then_not_allowed() {
        let a = Auth::new(vec!["alice".into()]);
        assert!(a.revoke("alice"));
        assert!(!a.is_allowed(&UserId("alice".into())));
        // 再移除返回 false。
        assert!(!a.revoke("alice"));
    }

    #[test]
    fn snapshot_sorted() {
        let a = Auth::new(vec!["charlie".into(), "alice".into(), "bob".into()]);
        assert_eq!(
            a.snapshot(),
            vec!["alice".to_string(), "bob".into(), "charlie".into()]
        );
    }

    #[test]
    fn clone_shares_underlying_set() {
        let a = Auth::new(vec![]);
        let b = a.clone();
        assert!(a.is_discovery());
        a.allow("x");
        // Clone 后共享（Arc）：a 的改动对 b 可见。
        assert!(b.is_allowed(&UserId("x".into())));
        assert!(!b.is_discovery());
    }

    #[test]
    fn reload_replaces_whitelist() {
        let a = Auth::new(vec!["alice".into(), "bob".into()]);
        // clone 共享底层，模拟 dispatcher 持有的句柄。
        let observer = a.clone();
        a.reload(vec!["carol".into()]);
        // 旧的已不在，新的生效。
        assert!(!observer.is_allowed(&UserId("alice".into())));
        assert!(!observer.is_allowed(&UserId("bob".into())));
        assert!(observer.is_allowed(&UserId("carol".into())));
        // reload 到空 = 回到发现模式。
        a.reload(vec![]);
        assert!(observer.is_discovery());
    }

    #[test]
    fn discovery_when_empty() {
        let a = Auth::new(vec![]);
        assert!(a.is_discovery());
        a.allow("x");
        assert!(!a.is_discovery());
        a.revoke("x");
        assert!(a.is_discovery());
    }

    // ---------- P4-5：会话（群）白名单 ----------

    #[test]
    fn chat_allowlist_gates_by_conv() {
        let a = Auth::with_chats(vec!["alice".into()], vec!["feishu:oc_g1".into()]);
        // 群成员不在 sender 白名单，但群已授权 → 放行。
        assert!(!a.is_allowed(&UserId("bob".into())));
        assert!(a.is_chat_allowed("feishu:oc_g1"));
        // 未授权群 + 未授权 sender → 双双拒绝。
        assert!(!a.is_chat_allowed("feishu:oc_g2"));
    }

    #[test]
    fn chat_allow_revoke_and_snapshot() {
        let a = Auth::new(vec![]);
        assert!(a.allow_chat("feishu:oc_x"));
        assert!(!a.allow_chat("feishu:oc_x"), "重复 allow 返回 false");
        assert!(a.revoke_chat("feishu:oc_x"));
        assert!(!a.revoke_chat("feishu:oc_x"));
        a.allow_chat("feishu:oc_b");
        a.allow_chat("feishu:oc_a");
        assert_eq!(
            a.snapshot_chats(),
            vec!["feishu:oc_a".to_string(), "feishu:oc_b".to_string()]
        );
    }

    #[test]
    fn chats_only_is_not_discovery() {
        // 仅配置会话白名单（sender 空）不进入发现模式：群成员可直接使用，
        // 管理员可经 admin_senders 授权。
        let a = Auth::with_chats(vec![], vec!["feishu:oc_g".into()]);
        assert!(!a.is_discovery());
    }

    #[test]
    fn reload_chats_replaces() {
        let a = Auth::with_chats(vec![], vec!["feishu:oc_old".into()]);
        let observer = a.clone();
        a.reload_chats(vec!["feishu:oc_new".into()]);
        assert!(!observer.is_chat_allowed("feishu:oc_old"));
        assert!(observer.is_chat_allowed("feishu:oc_new"));
    }
}
