//! v1.21 入站管道可观测性：feishu 侧指标（与 core/ilink 同款——直接注册到
//! 进程级默认 registry，`/metrics` 的 `render()` 统一收集，无需传句柄）。
//!
//! 四项指标对应 v11 复审确认的三类停摆发生前的先行信号：
//! - `drain_event_seconds`（histogram）：WS 事件从 drain 取出到处理完的时延
//!   ——内联 await/队头阻塞（HTTP 分区）时 p99 先行抬升；
//! - `ws_payload_backlog`（gauge）：WS → drain 的无界 channel 积压长度
//!   ——drain 停摆时先行增长（正常恒近 0）；
//! - `pump_pending`（gauge）：per-conv 泵的待处理 job 总数——媒体队头阻塞
//!   （大文件/转写）先行增长；
//! - `token_refresh_waiters`（gauge）：token single-flight 门的等待人数
//!   ——token 端点故障时先行增长（正常 0/短暂 1-2）。
use std::sync::LazyLock;

use prometheus::{register_gauge, register_histogram, Gauge, Histogram};

pub(crate) struct Metrics {
    /// drain 单事件处理时延（秒）。
    pub drain_event: Histogram,
    /// WS payload channel 积压长度（drain 侧每次循环采样）。
    pub ws_backlog: Gauge,
    /// per-conv 泵待处理 job 总数（pump_send/取走后采样）。
    pub pump_pending: Gauge,
    /// token 刷新 single-flight 门当前等待人数。
    pub token_waiters: Gauge,
}

impl Metrics {
    fn new() -> Self {
        Self {
            drain_event: register_histogram!(
                "imagent_feishu_drain_event_seconds",
                "drain 单事件处理时延（秒；p99 抬升 = 队头阻塞先行信号）"
            )
            .expect("register feishu drain_event"),
            ws_backlog: register_gauge!(
                "imagent_feishu_ws_payload_backlog",
                "WS → drain 无界 channel 积压长度（增长 = drain 停摆先行信号）"
            )
            .expect("register feishu ws_backlog"),
            pump_pending: register_gauge!(
                "imagent_feishu_pump_pending",
                "per-conv 泵待处理 job 总数（增长 = 媒体队头阻塞先行信号）"
            )
            .expect("register feishu pump_pending"),
            token_waiters: register_gauge!(
                "imagent_feishu_token_refresh_waiters",
                "token 刷新 single-flight 等待人数（增长 = token 端点故障先行信号）"
            )
            .expect("register feishu token_waiters"),
        }
    }
}

pub(crate) static METRICS: LazyLock<Metrics> = LazyLock::new(Metrics::new);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feishu_metrics_renderable() {
        METRICS.drain_event.observe(0.01);
        METRICS.ws_backlog.set(0.0);
        METRICS.pump_pending.set(0.0);
        METRICS.token_waiters.set(0.0);
        let out = imagent_core::metrics::render();
        for name in [
            "imagent_feishu_drain_event_seconds",
            "imagent_feishu_ws_payload_backlog",
            "imagent_feishu_pump_pending",
            "imagent_feishu_token_refresh_waiters",
        ] {
            assert!(out.contains(name), "missing {name}: {out}");
        }
    }
}
