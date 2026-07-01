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

#[derive(Debug, Clone)]
pub struct Auth {
    allowed: Arc<RwLock<HashSet<String>>>,
}

impl Auth {
    /// 用配置的 `allowed_senders` 构造。空 vec = 发现模式。
    pub fn new(allowed_senders: Vec<String>) -> Self {
        Self {
            allowed: Arc::new(RwLock::new(allowed_senders.into_iter().collect())),
        }
    }

    /// 白名单为空 => 发现模式。
    pub fn is_discovery(&self) -> bool {
        self.allowed.read().is_empty()
    }

    pub fn is_allowed(&self, uid: &UserId) -> bool {
        self.allowed.read().contains(&uid.0)
    }

    /// 加入白名单，返回是否新增（已存在返回 false）。
    pub fn allow(&self, sender: &str) -> bool {
        self.allowed.write().insert(sender.to_string())
    }

    /// 移除，返回是否原本存在。
    pub fn revoke(&self, sender: &str) -> bool {
        self.allowed.write().remove(sender)
    }

    /// 当前白名单快照（按 id 升序）。
    pub fn snapshot(&self) -> Vec<String> {
        let s = self.allowed.read();
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
        assert_eq!(a.snapshot(), vec!["alice".to_string(), "bob".into(), "charlie".into()]);
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
    fn discovery_when_empty() {
        let a = Auth::new(vec![]);
        assert!(a.is_discovery());
        a.allow("x");
        assert!(!a.is_discovery());
        a.revoke("x");
        assert!(a.is_discovery());
    }
}
