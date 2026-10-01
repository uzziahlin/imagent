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
    register_counter_vec, register_histogram, register_int_counter, register_int_counter_vec,
    register_int_gauge, Encoder, Histogram, IntCounter, IntCounterVec, IntGauge, TextEncoder,
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
    /// P5：权限审批决策数，label `result` = allow | deny | timeout | dropped。
    /// （/stop 触发的 fail-closed deny 计入 deny。）
    pub permission_decisions: IntCounterVec,
    /// P5：agent 超时分类计数，label `kind` = idle（空闲看门狗）| total（总预算）。
    pub agent_timeouts: IntCounterVec,
    /// D10：终端 `ask_via_im` 的回复计数，label `result` = ok | timeout | dropped。
    /// 与审批指标分离——ask 无 allow/deny 语义，混入会污染审批口径。
    pub ask_via_im_replies: IntCounterVec,
    /// token 用量计数，label `backend` + `kind` = input | output | cached。
    /// （backend 未产出 usage 的轮次不计。）
    pub token_usage: IntCounterVec,
    /// 成本（美元）累计，label `backend`。仅 claude 提供成本数据；f64 计数器
    /// （Prometheus 的 Counter 即 float64）。
    pub cost_usd: prometheus::CounterVec,
    /// P2（code-review v13）：当前在飞 agent 轮数（跨 conv）。permit 获取后 inc、
    /// 轮次持票（dispatch::round::RoundPermit）Drop 时 dec——上限 0（不限制）时
    /// 照常计数（gauge 直接维护，不依赖「上限 - 可用 permit」换算）。
    pub running_rounds: IntGauge,
    /// P2（code-review v13）：等待并发护栏 permit 的轮数（排队深度）。acquire 前
    /// inc、获得/被取消 dec（Drop guard 防 future 取消泄漏）。
    pub round_queue_depth: IntGauge,
    /// T11（v13 #4）：chunk channel 积压深度。round 消费循环**每次迭代**刷新
    /// （`rx.len()`，tokio mpsc `Receiver::len` 自 1.38 稳定，workspace 1.5x 可用）
    /// ——消费端背压（P1-1/P2-11 整类问题）的最直接先行信号：消费被平台 IO 钉死
    /// 时该值会顶到 channel 容量（32）并停留。全局单 gauge 不带 conv label（会话
    /// 维度做 label 是高基数时间序列）；P3-c（code-review v14）起由
    /// [`AgentChannelDepthSlot`] 按轮差量记账，多轮并发在飞时值为「Σ 各轮深度」
    ///（轮退出 Drop 归零自己的贡献，不再互踩）。
    pub agent_channel_depth: IntGauge,
    /// T11：卡片 patch 时延（秒）——card_session 的 `dispatch_patch` 全路径计时
    /// （平台 send_card/update_card + live_cards 登记落库）。bucket 对齐时延类
    /// 建议分布（5ms～10s）。
    pub card_patch_seconds: Histogram,
    /// T11：文本平台合帧 flush 的字节量（round::TextCoalescer::flush）。观察
    /// P2-11 合帧效果——delta 级 chunk（claude-acp）应聚成较大消息而非逐条
    /// 发送；bucket 对数分布（16B～64KB）。
    pub text_flush_bytes: Histogram,
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
            permission_decisions: register_int_counter_vec!(
                "imagent_permission_decisions_total",
                "权限审批决策数（allow/deny/timeout/dropped）",
                &["result"]
            )
            .expect("register permission_decisions"),
            agent_timeouts: register_int_counter_vec!(
                "imagent_agent_timeouts_total",
                "agent 超时分类（idle=空闲看门狗 / total=总预算）",
                &["kind"]
            )
            .expect("register agent_timeouts"),
            ask_via_im_replies: register_int_counter_vec!(
                "imagent_ask_via_im_replies_total",
                "终端 ask_via_im 回复数（ok/timeout/dropped）",
                &["result"]
            )
            .expect("register ask_via_im_replies"),
            token_usage: register_int_counter_vec!(
                "imagent_token_usage_total",
                "token 用量计数（backend/kind=input|output|cached）",
                &["backend", "kind"]
            )
            .expect("register token_usage"),
            cost_usd: register_counter_vec!(
                "imagent_cost_usd_total",
                "成本（美元）累计（backend 维度；仅提供成本的 backend 计数）",
                &["backend"]
            )
            .expect("register cost_usd"),
            running_rounds: register_int_gauge!(
                "imagent_running_rounds",
                "当前在飞 agent 轮数（跨 conv；上限 0=不限制时照常计数）"
            )
            .expect("register running_rounds"),
            round_queue_depth: register_int_gauge!(
                "imagent_round_queue_depth",
                "等待全局并发护栏 permit 的轮数（排队深度）"
            )
            .expect("register round_queue_depth"),
            agent_channel_depth: register_int_gauge!(
                "imagent_agent_channel_depth",
                "agent chunk channel 积压深度（round 消费循环每迭代刷新；消费端背压先行信号）"
            )
            .expect("register agent_channel_depth"),
            card_patch_seconds: register_histogram!(
                "imagent_card_patch_seconds",
                "卡片 patch 时延（card_session dispatch_patch 全路径）",
                vec![0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]
            )
            .expect("register card_patch_seconds"),
            text_flush_bytes: register_histogram!(
                "imagent_text_flush_bytes",
                "文本平台合帧 flush 的字节量（TextCoalescer::flush，观察合帧效果）",
                vec![16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0]
            )
            .expect("register text_flush_bytes"),
        }
    }
}

/// 全局指标单例。访问即触发注册。
pub static METRICS: LazyLock<Metrics> = LazyLock::new(Metrics::new);

/// P3-c（code-review v14）：[`Metrics::agent_channel_depth`] 的按轮记账句柄。
/// 旧的全局单值 `set()` 语义在多轮并发在飞时互踩：后观测的轮覆盖先写值、
/// 先退出的轮把 `set(0)` 打在别的轮头上（在飞深度被清零，背压信号失真）。
/// 改为「Σ 各轮本地深度」的合计记账：每轮 set 前先减旧值再加新值（差量
/// 增减），轮退出（含 future 被 abort——drain 的 abort_all）时 Drop guard 减掉
/// 最后观测值归零自己的贡献。合计语义下该 gauge 仍是消费端背压先行信号
///（消费被平台 IO 钉死时，Σ 顶到 32×在飞轮数并停留）。
#[derive(Debug, Default)]
pub(crate) struct AgentChannelDepthSlot {
    /// 本轮最近一次上报的深度（Drop 时的归还基数）。
    last: i64,
}

impl AgentChannelDepthSlot {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 上报本轮当前深度（差量应用到全局 gauge，不动其它轮的贡献）。
    pub(crate) fn set(&mut self, depth: i64) {
        METRICS.agent_channel_depth.add(depth - self.last);
        self.last = depth;
    }
}

impl Drop for AgentChannelDepthSlot {
    fn drop(&mut self) {
        if self.last != 0 {
            METRICS.agent_channel_depth.sub(self.last);
        }
    }
}

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

    /// 本模块两个测试都触碰全局 gauge，串行化防互相干扰（dispatch 集成测试
    /// 对 gauge 的贡献是差量平衡的，无净残值）。
    static METRICS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn render_contains_registered_metrics() {
        let _g = METRICS_TEST_LOCK.lock().unwrap();
        // 触发惰性初始化并产生一次计数。
        METRICS.messages_in.inc();
        METRICS.backend_calls.inc();
        METRICS
            .permission_decisions
            .with_label_values(&["allow"])
            .inc();
        METRICS.agent_timeouts.with_label_values(&["idle"]).inc();
        METRICS.ask_via_im_replies.with_label_values(&["ok"]).inc();
        METRICS
            .token_usage
            .with_label_values(&["claude-cli", "input"])
            .inc();
        METRICS
            .cost_usd
            .with_label_values(&["claude-cli"])
            .inc_by(0.5);
        METRICS.running_rounds.inc();
        METRICS.running_rounds.dec(); // 平衡：gauge 不留残值（其它测试断言归零态）
        METRICS.round_queue_depth.inc();
        METRICS.round_queue_depth.dec();
        // T11：消费侧三指标——gauge 观测后归零平衡，histogram 观测一次。
        METRICS.agent_channel_depth.set(3);
        METRICS.agent_channel_depth.set(0);
        METRICS.card_patch_seconds.observe(0.05);
        METRICS.text_flush_bytes.observe(256.0);
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
        assert!(
            out.contains("imagent_permission_decisions_total"),
            "missing permission_decisions: {out}"
        );
        assert!(
            out.contains("imagent_agent_timeouts_total"),
            "missing agent_timeouts: {out}"
        );
        assert!(
            out.contains("imagent_ask_via_im_replies_total"),
            "missing ask_via_im_replies: {out}"
        );
        assert!(
            out.contains("imagent_token_usage_total"),
            "missing token_usage: {out}"
        );
        assert!(
            out.contains("imagent_cost_usd_total"),
            "missing cost_usd: {out}"
        );
        assert!(
            out.contains("imagent_running_rounds"),
            "missing running_rounds: {out}"
        );
        assert!(
            out.contains("imagent_round_queue_depth"),
            "missing round_queue_depth: {out}"
        );
        assert!(
            out.contains("imagent_agent_channel_depth"),
            "missing agent_channel_depth: {out}"
        );
        assert!(
            out.contains("imagent_card_patch_seconds"),
            "missing card_patch_seconds: {out}"
        );
        assert!(
            out.contains("imagent_text_flush_bytes"),
            "missing text_flush_bytes: {out}"
        );
    }

    /// P3-c（code-review v14）：按轮记账槽——多轮（两个槽并发存活）时全局
    /// gauge 为「Σ 各轮深度」（差量增减），槽 Drop 归零自己的贡献；旧的裸
    /// set() 语义会让后写覆盖先写、先退出的轮清零在飞轮的观测。
    /// 同 binary 的 dispatch 集成测试会瞬时触碰该全局 gauge（µs 级、差量
    /// 平衡），断言偶发失配时整体重试——真实回归会稳定失败，不因此被掩盖。
    #[test]
    fn channel_depth_slot_sums_and_releases() {
        let _g = METRICS_TEST_LOCK.lock().unwrap();
        for _ in 0..100 {
            let before = METRICS.agent_channel_depth.get();
            let mut ok = true;
            {
                let mut s1 = AgentChannelDepthSlot::new();
                s1.set(3);
                ok &= METRICS.agent_channel_depth.get() == before + 3;
                {
                    let mut s2 = AgentChannelDepthSlot::new();
                    s2.set(5);
                    ok &= METRICS.agent_channel_depth.get() == before + 8;
                    // s2 退出：只归还自己的 5（旧 set(0) 语义会连 s1 的 3 一起清掉）。
                }
                ok &= METRICS.agent_channel_depth.get() == before + 3;
                // 同槽降观测：差量 -2。
                s1.set(1);
                ok &= METRICS.agent_channel_depth.get() == before + 1;
            }
            ok &= METRICS.agent_channel_depth.get() == before;
            if ok {
                return;
            }
        }
        panic!("AgentChannelDepthSlot 合计记账断言持续不成立（真实回归，非并行测试干扰）");
    }
}
