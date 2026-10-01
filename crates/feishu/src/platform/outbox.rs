//! v1.21 outbox 发送侧持久化重试（T19 拆分自 platform.rs；Wave C 出站可靠性
//! 收敛：泵本体上移 core `OutboxDriver`，本文件只剩 feishu 的 payload 契约与
//! 交付闭包接线）。
//!
//! drain 侧提示类发送失败后落盘（store outbox 表，kind=[`OUTBOX_KIND`]），
//! 由 core driver 周期拉取到期行重发；成败信号来自
//! [`super::drain::send_drain_text_result`] 的直发形态。泵的退避/超限丢弃/
//! 停机语义与日志文案见 `imagent_core::outbox` 模块文档（feishu v1.21 行为
//! 逐字保留，日志 target 仍为 "feishu"——用户既有日志过滤不断档）。

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;
use tracing::error;

use imagent_core::outbox::{OutboxDeliver, OutboxDriver};
use imagent_core::ConvId;
use open_lark::CoreConfig;

use super::drain::send_drain_text_result_with_uuid;

/// feishu 在 store outbox 表的 kind（main 装配 sweeper 的 known 名单同源）。
pub const OUTBOX_KIND: &str = "feishu_text";

/// P2-5（code-review v14）：outbox 落盘 payload 构造（含幂等 uuid）。
/// uuid 在 enqueue 时生成一次、随 payload 持久化——泵重发时**透传同一 uuid**
/// 给 send_text_msg_with_uuid / reply_message（飞书幂等键语义「一次逻辑发送
/// 一个键」；重试换新 uuid 会让首达+重试的用户收到两条）。
pub(super) fn outbox_payload(conv: &str, text: &str) -> String {
    serde_json::json!({
        "conv": conv,
        "text": text,
        "uuid": uuid::Uuid::new_v4().to_string(),
    })
    .to_string()
}

/// P2-5：payload 往返解析（纯函数，便于单测）——`(conv, text, uuid)`。
/// 旧行（无 uuid 字段）回 None uuid，调用方重发时新生成（向后兼容）。
fn parse_outbox_payload(payload: &str) -> Option<(String, String, Option<String>)> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let conv = v.get("conv").and_then(|c| c.as_str())?.to_string();
    let text = v.get("text").and_then(|c| c.as_str())?.to_string();
    let uuid = v
        .get("uuid")
        .and_then(|c| c.as_str())
        .filter(|u| !u.is_empty())
        .map(String::from);
    Some((conv, text, uuid))
}

/// Wave C：拉起 core [`OutboxDriver`]（kind=`feishu_text`）。交付闭包内聚
/// feishu 特有逻辑：payload 解析（失败按旧语义放弃——error 留痕 + 返回 Ok
/// 让 driver 删行）、幂等 uuid 透传（P2-5）、评论/话题/普通 conv 三路直发
/// （[`send_drain_text_result_with_uuid`]）。
pub(super) fn spawn_outbox_driver(
    store: imagent_store::Store,
    core_config: Arc<CoreConfig>,
    token_lock: Arc<RwLock<Option<(String, Instant)>>>,
    app_id: String,
    app_secret: String,
) {
    let deliver: OutboxDeliver = Arc::new(move |row| {
        let core_config = core_config.clone();
        let token_lock = token_lock.clone();
        let app_id = app_id.clone();
        let app_secret = app_secret.clone();
        Box::pin(async move {
            let Some((conv, text, uuid)) = parse_outbox_payload(&row.payload) else {
                // 旧 feishu 泵语义：解析失败按放弃处理（重试 16 次也不会成功）
                // ——error 留痕、返回 Ok 让 driver 删行。
                error!(target: "feishu", id = row.id, "outbox 行解析失败，放弃");
                return Ok(());
            };
            // P2-5：落盘时的 uuid 透传（旧行无 uuid → 现生成，整个重试周期内
            // 每次重试共用本键——重试间换键同样会双发）。
            let idem = uuid.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            // send_drain_text 内部已吞错误（warn），泵需要成败信号——直接
            // 调内部闭包等价逻辑：这里取 Result 的直发形态。
            send_drain_text_result_with_uuid(
                &core_config,
                &token_lock,
                &app_id,
                &app_secret,
                &ConvId(conv.clone()),
                &text,
                Some(&idem),
            )
            .await
        })
    });
    OutboxDriver::new(store, OUTBOX_KIND, deliver, "feishu").spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P2-5：payload 往返后 uuid 不变——落盘侧生成、泵侧透传同一幂等键；
    /// 旧形态（无 uuid 字段）回 None（调用方新生成，向后兼容）。
    #[test]
    fn outbox_payload_uuid_roundtrip() {
        let payload = outbox_payload("feishu:ou_t", "提示文本");
        let (conv, text, uuid) = parse_outbox_payload(&payload).expect("应可解析");
        assert_eq!(conv, "feishu:ou_t");
        assert_eq!(text, "提示文本");
        let uuid = uuid.expect("新行应带 uuid");
        assert!(!uuid.is_empty(), "uuid 非空");
        // 再解析一次仍不变（持久化的值，不是现生成的）。
        let (_, _, uuid2) = parse_outbox_payload(&payload).expect("应可解析");
        assert_eq!(uuid2.as_deref(), Some(uuid.as_str()), "uuid 往返不变");
        // 每次构造生成不同 uuid（不同逻辑发送不同键）。
        let other = outbox_payload("feishu:ou_t", "提示文本");
        let (_, _, other_uuid) = parse_outbox_payload(&other).unwrap();
        assert_ne!(other_uuid.as_deref(), Some(uuid.as_str()));
        // 旧形态（v14 前落盘的行）：无 uuid 字段 → None。
        let legacy = serde_json::json!({ "conv": "feishu:ou_t", "text": "旧文案" }).to_string();
        let (c, t, u) = parse_outbox_payload(&legacy).expect("旧行应可解析");
        assert_eq!((c.as_str(), t.as_str()), ("feishu:ou_t", "旧文案"));
        assert!(u.is_none(), "旧行无 uuid → None");
        // 缺字段 / 非 JSON：None（调用方放弃该行）。
        assert!(parse_outbox_payload("not json").is_none());
        assert!(parse_outbox_payload(r#"{"conv":"c"}"#).is_none());
    }

    /// Wave C：kind 常量钉值——store 行、main 的 sweeper 名单、日志口径都
    /// 以它为单一事实源，漂移即测试红。
    #[test]
    fn outbox_kind_value() {
        assert_eq!(OUTBOX_KIND, "feishu_text");
    }
}
