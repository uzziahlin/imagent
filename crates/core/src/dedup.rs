//! 入站消息去重：滑动窗口（平台无关，供 ilink / wecom 等 Platform 共享）。
//!
//! 同一 key 在窗口内视为重复丢弃；窗口用 `std::time::Instant`，无系统时钟依赖。
//! 用 `std::sync::Mutex`：临界区极短（HashMap 查插 + 摊销清理），不跨 `.await`。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 滑动窗口去重器。
///
/// 窗口默认 24 小时（v1.18 review：此前 5 分钟 < 飞书长连接重投递视界——
/// 断连超 5 分钟（合盖睡眠/网络分区、飞书侧故障）后重投的未 ack 事件被
/// 当新事件，同一消息会**重复驱动一整轮 agent**（Bash 副作用重复执行）。
/// 24h 覆盖过夜重连场景；进程重启清空（接受——重投主要发生在重连瞬间，
/// 持久化去重的收益不抵一张 schema 迁移的复杂度，超长断连仍靠用户观察）。
#[derive(Debug)]
pub struct Dedup {
    state: Mutex<State>,
    window: Duration,
    /// 硬上限：超限先做过期清理，仍超则整体清空（best-effort 去重宁漏杀不
    /// OOM——清空后退化等价于旧短窗行为，不会比修复前更差）。
    max_entries: usize,
}

#[derive(Debug, Default)]
struct State {
    seen: HashMap<String, Instant>,
    checks: u64,
}

/// 默认硬上限：10 万事件（约每条 key 几十字节，~10MB 级），远超单部署
/// 日常事件量，仅作内存护栏。
const DEFAULT_MAX_ENTRIES: usize = 100_000;

impl Dedup {
    pub fn new(window: Duration) -> Self {
        Self {
            state: Mutex::new(State::default()),
            window,
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }

    /// 检查 key 是否为新消息。
    ///
    /// 返回 `true` 表示新（已插入）；`false` 表示窗口内重复。
    pub fn check(&self, key: &str) -> bool {
        // P2-X：std Mutex poison（持锁 panic）后用 into_inner 恢复，避免永久 panic
        // （dedup 是 best-effort 去重，poison 后继续用可接受）。
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        // 摊销清理：窗口拉长到 24h 后全量 retain（O(n)）不再适合每次执行——
        // 每 64 次检查或超硬上限时清理一次；窗口内命中不受影响（contains_key
        // 与插入均在清理之外判定）。
        st.checks = st.checks.wrapping_add(1);
        if st.checks % 64 == 0 || st.seen.len() > self.max_entries {
            st.seen
                .retain(|_, ts| now.duration_since(*ts) < self.window);
            if st.seen.len() > self.max_entries {
                st.seen.clear();
            }
        }
        if st.seen.contains_key(key) {
            false
        } else {
            st.seen.insert(key.to_string(), now);
            true
        }
    }

    /// 当前登记条数（测试/诊断用）。
    #[cfg(test)]
    fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .seen
            .len()
    }
}

impl Default for Dedup {
    fn default() -> Self {
        Self::new(Duration::from_secs(24 * 60 * 60))
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
        // 零窗口：下一次摊销清理后全过期，再见即视为新。
        let d = Dedup::new(Duration::ZERO);
        assert!(d.check("x"));
        // 凑满一个清理周期（64 次）后条目过期清除。
        for i in 0..64 {
            d.check(&format!("filler-{i}"));
        }
        assert!(d.check("x"), "zero window → entry expired → re-seen as new");
    }

    #[test]
    fn eviction_keeps_map_bounded() {
        let d = Dedup::new(Duration::ZERO);
        for i in 0..1000 {
            d.check(&format!("k{i}"));
        }
        // 摊销清理每 64 次一次：任意时刻残留 ≤ 64 + 触发清理前的余量。
        assert!(d.len() <= 128, "len={}", d.len());
    }

    #[test]
    fn hard_cap_clears_map() {
        let d = Dedup::new(Duration::from_secs(24 * 60 * 60));
        d.state.lock().unwrap().seen.reserve(0);
        // 直接预填超限条目（绕过 check 注入，模拟长窗累积），下一次 check
        // 触发超限清理路径。
        {
            let mut st = d.state.lock().unwrap();
            for i in 0..=DEFAULT_MAX_ENTRIES {
                st.seen.insert(format!("old-{i}"), Instant::now());
            }
        }
        assert!(d.check("new-key"));
        assert!(d.len() <= d.max_entries, "len={}", d.len());
    }

    #[test]
    fn default_window_covers_overnight() {
        assert!(Dedup::default().window >= Duration::from_secs(23 * 60 * 60));
    }
}
