//! 入站消息去重：滑动窗口（平台无关，供 ilink / wecom 等 Platform 共享）。
//!
//! 同一 key 在窗口内视为重复丢弃；窗口用 `std::time::Instant`，无系统时钟依赖。
//! 用 `std::sync::Mutex`：临界区极短（HashMap 查插 + 清理），不跨 `.await`。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 滑动窗口去重器。窗口默认 5 分钟。
#[derive(Debug)]
pub struct Dedup {
    seen: Mutex<HashMap<String, Instant>>,
    window: Duration,
}

impl Dedup {
    pub fn new(window: Duration) -> Self {
        Self {
            seen: Mutex::new(HashMap::new()),
            window,
        }
    }

    /// 检查 key 是否为新消息。
    ///
    /// 返回 `true` 表示新（已插入并清理过期项）；`false` 表示窗口内重复。
    pub fn check(&self, key: &str) -> bool {
        // P2-X：std Mutex poison（持锁 panic）后用 into_inner 恢复，避免永久 panic
        // （dedup 是 best-effort 去重，poison 后继续用可接受）。
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        // 清理窗口外旧项，避免无界增长。
        seen.retain(|_, ts| now.duration_since(*ts) < self.window);
        if seen.contains_key(key) {
            false
        } else {
            seen.insert(key.to_string(), now);
            true
        }
    }
}

impl Default for Dedup {
    fn default() -> Self {
        Self::new(Duration::from_secs(300))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_seen_then_dup() {
        let d = Dedup::new(Duration::from_secs(300));
        assert!(d.check("a"));
        assert!(!d.check("a"), "same key within window must be dup");
        assert!(d.check("b"), "different key is new");
        assert!(!d.check("b"));
    }

    #[test]
    fn expired_entry_is_reseen() {
        // 零窗口：插入后立即过期，下次再见即视为新。
        let d = Dedup::new(Duration::ZERO);
        assert!(d.check("x"));
        assert!(d.check("x"), "zero window → entry expired → re-seen as new");
    }

    #[test]
    fn eviction_keeps_map_bounded() {
        let d = Dedup::new(Duration::ZERO);
        for i in 0..1000 {
            d.check(&format!("k{i}"));
        }
        let seen = d.seen.lock().unwrap();
        // 全部因零窗口在下一次操作被清理；这里仅触发一次清理即清空。
        assert!(seen.len() <= 1);
    }
}
