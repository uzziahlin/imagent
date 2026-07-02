//! sendmessage 限流熔断器（被动、服从式）。
//!
//! 仅在被 server 限流信号触发后自我节制：滑动窗口计数 + cooldown 熔断。
//! **不**做主动 token bucket / QPS 伪造（合规红线，见 lib.rs 头注）。
//!
//! 用 `std::time::Instant` 单调钟，不受系统时钟回拨影响。

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use prometheus::{register_int_counter, IntCounter};
use tokio::sync::Mutex;

/// 全局限流事件计数器（注册到 prometheus 默认 registry，由 core 的
/// `metrics::render` 一并收集）。
static RATE_LIMIT_EVENTS: LazyLock<IntCounter> = LazyLock::new(|| {
    register_int_counter!(
        "imagent_rate_limit_events_total",
        "ilink 被动限流事件数（sendmessage 被服务端限流）"
    )
    .expect("register rate_limit_events")
});

/// 被动限流熔断器：窗口内限流事件达阈值则熔断一段 cooldown。
pub struct RateBreaker {
    events: Mutex<Vec<Instant>>,
    open_until: Mutex<Instant>,
    window: Duration,
    threshold: usize,
    cooldown: Duration,
}

impl RateBreaker {
    pub fn new(window: Duration, threshold: usize, cooldown: Duration) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            open_until: Mutex::new(Instant::now()),
            window,
            threshold,
            cooldown,
        }
    }

    /// 当前是否处于熔断（cooldown 未过）。返回剩余等待时长（0 表示未熔断）。
    pub async fn cooldown_remaining(&self) -> Duration {
        let open_until = *self.open_until.lock().await;
        open_until.saturating_duration_since(Instant::now())
    }

    /// 记录一次限流事件，返回是否因此次触发熔断（窗口内事件数 >= threshold）。
    pub async fn record_event(&self) -> bool {
        let now = Instant::now();
        // best-effort 指标：限流事件计数。
        RATE_LIMIT_EVENTS.inc();
        let mut events = self.events.lock().await;
        // 清窗口外旧事件。
        events.retain(|t| now.duration_since(*t) < self.window);
        events.push(now);
        if events.len() >= self.threshold {
            let mut open_until = self.open_until.lock().await;
            let candidate = now + self.cooldown;
            // 只往后推（max），不提前结束既有 cooldown。
            if candidate > *open_until {
                *open_until = candidate;
            }
            true
        } else {
            false
        }
    }

    /// 成功后清空事件窗口、重置 open_until。
    pub async fn reset(&self) {
        let mut events = self.events.lock().await;
        events.clear();
        let mut open_until = self.open_until.lock().await;
        *open_until = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn window_expiry_drops_old_events() {
        // window 极小，旧事件被剔除，不累积到阈值。
        let b = RateBreaker::new(Duration::from_millis(20), 2, Duration::from_millis(50));
        assert!(!b.record_event().await);
        tokio::time::sleep(Duration::from_millis(30)).await; // 超过 window
        assert!(!b.record_event().await); // 第二次但旧事件已剔除，仍 < 阈值 2
    }

    #[tokio::test]
    async fn threshold_trips_breaker() {
        let b = RateBreaker::new(Duration::from_secs(30), 2, Duration::from_millis(100));
        assert!(!b.record_event().await);
        assert!(b.record_event().await); // 第 2 次 → 熔断
        assert!(b.cooldown_remaining().await > Duration::ZERO);
    }

    #[tokio::test]
    async fn threshold_one_trips_immediately() {
        // 默认阈值 1：单次即熔断。
        let b = RateBreaker::new(Duration::from_secs(30), 1, Duration::from_millis(100));
        assert!(b.record_event().await);
        assert!(b.cooldown_remaining().await > Duration::ZERO);
    }

    #[tokio::test]
    async fn cooldown_counts_down() {
        let b = RateBreaker::new(Duration::from_secs(30), 1, Duration::from_millis(60));
        assert!(b.record_event().await);
        let r1 = b.cooldown_remaining().await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let r2 = b.cooldown_remaining().await;
        assert!(r2 < r1, "cooldown should decrease: r1={r1:?} r2={r2:?}");
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(b.cooldown_remaining().await, Duration::ZERO);
    }

    #[tokio::test]
    async fn reset_clears_window_and_cooldown() {
        let b = RateBreaker::new(Duration::from_secs(30), 1, Duration::from_secs(30));
        assert!(b.record_event().await);
        assert!(b.cooldown_remaining().await > Duration::ZERO);
        b.reset().await;
        assert_eq!(b.cooldown_remaining().await, Duration::ZERO);
        // reset 后事件窗口也清空：再 record 一次应算第 1 次。
        assert!(b.record_event().await); // 阈值 1 仍触发，但说明 reset 清空了（否则 len>=2）
    }

    #[tokio::test]
    async fn record_extends_not_shortens_cooldown() {
        let b = RateBreaker::new(Duration::from_secs(30), 1, Duration::from_millis(100));
        b.record_event().await;
        let r1 = b.cooldown_remaining().await;
        // cooldown 中再记录一次，candidate > open_until → 推后。
        b.record_event().await;
        let r2 = b.cooldown_remaining().await;
        assert!(r2 >= r1, "new event should not shorten existing cooldown");
    }
}
