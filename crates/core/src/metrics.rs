//! Prometheus 指标（best-effort 埋点，失败仅 warn 不阻断主流程）。
//!
//! 所有指标通过 `prometheus::register_*!` 宏注册到进程级**默认 registry**；
//! `render()` 用 `TextEncoder` 收集默认 registry 的全部指标——含 ilink crate
//! 自行注册的 `imagent_rate_limit_events_total`。各 crate 共享默认 registry，
//! 无需跨 crate 传递 Metrics 句柄。
//!
//! `sessions_active` gauge 暂未接入（避免 per-message 查库；活跃会话数当前
//! 由 `/health` JSON 即时查 store 提供，见 main.rs）。
use std::sync::LazyLock;

use prometheus::{
    register_histogram, register_int_counter, Encoder, Histogram, IntCounter, TextEncoder,
};

/// 全局指标集合。惰性初始化（首次访问即注册到默认 registry）。
#[derive(Debug)]
pub struct Metrics {
    /// 入站消息数（`Dispatcher::handle` 入口）。
    pub messages_in: IntCounter,
    /// 成功回传消息数（`Dispatcher::reply` send_text 成功）。
    pub messages_out: IntCounter,
    /// `backend.run` 调用数（正常完成）。
    pub backend_calls: IntCounter,
    /// `backend.run` 失败数（Err 或 task panic）。
    pub backend_errors: IntCounter,
    /// `backend.run` 耗时分布（秒）。
    pub backend_duration: Histogram,
}

impl Metrics {
    fn new() -> Self {
        Self {
            messages_in: register_int_counter!("imagent_messages_in_total", "入站消息数")
                .expect("register messages_in"),
            messages_out: register_int_counter!("imagent_messages_out_total", "成功回传消息数")
                .expect("register messages_out"),
            backend_calls: register_int_counter!(
                "imagent_backend_calls_total",
                "backend.run 调用数"
            )
            .expect("register backend_calls"),
            backend_errors: register_int_counter!(
                "imagent_backend_errors_total",
                "backend.run 失败数"
            )
            .expect("register backend_errors"),
            backend_duration: register_histogram!(
                "imagent_backend_duration_seconds",
                "backend.run 耗时（秒）"
            )
            .expect("register backend_duration"),
        }
    }
}

/// 全局指标单例。访问即触发注册。
pub static METRICS: LazyLock<Metrics> = LazyLock::new(Metrics::new);

/// 收集默认 registry 全部指标为 Prometheus 文本格式（供 `/metrics`）。
pub fn render() -> String {
    let encoder = TextEncoder::new();
    let mfs = prometheus::gather();
    let mut buf = Vec::new();
    if let Err(e) = encoder.encode(&mfs, &mut buf) {
        tracing::warn!(target: "imagent::metrics", error = %e, "encode metrics failed");
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_contains_registered_metrics() {
        // 触发惰性初始化并产生一次计数。
        METRICS.messages_in.inc();
        METRICS.backend_calls.inc();
        let out = render();
        assert!(
            out.contains("imagent_messages_in_total"),
            "missing messages_in: {out}"
        );
        assert!(
            out.contains("imagent_backend_calls_total"),
            "missing backend_calls: {out}"
        );
        assert!(
            out.contains("imagent_backend_duration_seconds"),
            "missing backend_duration: {out}"
        );
    }
}
