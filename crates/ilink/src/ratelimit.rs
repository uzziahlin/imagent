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
///
/// P1：全部状态收进**单把锁**（此前 events / open_until 两把锁，record_event
/// 双锁非原子，并发下窗口计数与熔断判定可观察到撕裂状态）。
pub struct RateBreaker {
    state: Mutex<BreakerState>,
    window: Duration,
    threshold: usize,
    cooldown: Duration,
}

#[derive(Default)]
struct BreakerState {
    /// 窗口内限流事件时间戳。
    events: Vec<Instant>,
    /// 熔断截止时刻（now 之后表示熔断中）。
    open_until: Option<Instant>,
    /// 连续成功计数（P1 reset 衰减语义，见 [`RateBreaker::reset`]）。
    consecutive_successes: usize,
}

impl RateBreaker {
    pub fn new(window: Duration, threshold: usize, cooldown: Duration) -> Self {
        Self {
            state: Mutex::new(BreakerState::default()),
            window,
            threshold,
            cooldown,
        }
    }

    /// 当前是否处于熔断（cooldown 未过）。返回剩余等待时长（0 表示未熔断）。
    pub async fn cooldown_remaining(&self) -> Duration {
        let st = self.state.lock().await;
        breaker_remaining(&st, Instant::now())
    }

    /// 记录一次限流事件，返回是否因此次触发熔断（窗口内事件数 >= threshold）。
    pub async fn record_event(&self) -> bool {
        let now = Instant::now();
        // best-effort 指标：限流事件计数。
        RATE_LIMIT_EVENTS.inc();
        let mut st = self.state.lock().await;
        // 任何失败都打断连续成功序列（P1：见 reset 注释的锯齿防护）。
        st.consecutive_successes = 0;
        // 清窗口外旧事件。
        st.events.retain(|t| now.duration_since(*t) < self.window);
        st.events.push(now);
        if st.events.len() >= self.threshold {
            let candidate = now + self.cooldown;
            // 只往后推（max），不提前结束既有 cooldown。
            st.open_until = Some(st.open_until.map_or(candidate, |u| u.max(candidate)));
            true
        } else {
            false
        }
    }

    /// 记录一次成功。P1 reset 语义修正（锯齿防护）：
    ///
    /// 此前任一次成功即清空窗口 + 提前结束 cooldown——「失败、成功、失败、
    /// 成功…」的锯齿模式下窗口永远攒不到阈值，熔断永不触发，限流事件全部
    /// 穿透。修正为**衰减而非立即清零**：
    /// - cooldown 期间的成功**不计入清零**也不缩短 cooldown（熔断必须完整冷却）；
    /// - 连续 `threshold` 次成功才清空事件窗口（与触发阈值对称：攒够同样
    ///   份量的成功证据才赦免）。取舍：成功越多越容易解除，但单次成功绝不
    ///   抹掉既有失败——宁可多熔断一拍（被动限流下安全），不可穿透。
    pub async fn reset(&self) {
        let now = Instant::now();
        let mut st = self.state.lock().await;
        if now < st.open_until.unwrap_or(now) {
            return; // 熔断中：成功不计入，cooldown 不缩短。
        }
        st.consecutive_successes += 1;
        if st.consecutive_successes >= self.threshold {
            st.events.clear();
            st.consecutive_successes = 0;
        }
    }
}

/// [`RateBreaker::cooldown_remaining`] 的纯函数核心（测试复用）。
fn breaker_remaining(st: &BreakerState, now: Instant) -> Duration {
    st.open_until.unwrap_or(now).saturating_duration_since(now)
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

    /// P1：单次成功不再清空窗口/缩短 cooldown（锯齿防护）。
    #[tokio::test]
    async fn single_reset_does_not_clear_window_or_cooldown() {
        let b = RateBreaker::new(Duration::from_secs(30), 2, Duration::from_secs(30));
        assert!(!b.record_event().await);
        b.reset().await;
        // 窗口未清：第 2 次事件仍触发熔断（旧实现会被单次成功抹掉）。
        assert!(b.record_event().await);
        assert!(b.cooldown_remaining().await > Duration::ZERO);
        // cooldown 期间的成功不缩短 cooldown：剩余时长仍接近整段。
        b.reset().await;
        assert!(
            b.cooldown_remaining().await > Duration::from_secs(29),
            "cooldown 不应被成功提前结束"
        );
    }

    /// P1：连续 threshold 次成功（且不在 cooldown 中）才清空窗口。
    #[tokio::test]
    async fn consecutive_successes_decay_window() {
        let b = RateBreaker::new(Duration::from_secs(30), 2, Duration::from_millis(50));
        assert!(!b.record_event().await);
        // 1 次成功不够：窗口保留，第 2 次事件仍熔断。
        b.reset().await;
        assert!(b.record_event().await, "单次成功不应清空失败窗口");
        assert!(b.cooldown_remaining().await > Duration::ZERO);
        // 熔断后的 cooldown 过去（用小 cooldown），再攒满 2 次连续成功 → 清空。
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(b.cooldown_remaining().await, Duration::ZERO);
        b.reset().await;
        b.reset().await;
        assert!(!b.record_event().await, "窗口应已清空，此事件算第 1 次");
        assert!(b.record_event().await);
    }

    /// P1 锯齿场景：失败-成功交替，熔断必须照常触发（旧实现永不触发）。
    #[tokio::test]
    async fn sawtooth_fail_success_still_trips() {
        let b = RateBreaker::new(Duration::from_secs(30), 2, Duration::from_millis(100));
        assert!(!b.record_event().await);
        b.reset().await; // 夹杂的成功不抹掉失败计数
        assert!(b.record_event().await, "锯齿模式下第 2 次失败应触发熔断");
        assert!(b.cooldown_remaining().await > Duration::ZERO);
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
