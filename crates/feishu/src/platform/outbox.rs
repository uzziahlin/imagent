//! v1.21 outbox 发送侧重试泵（T19 拆分自 platform.rs，纯移动）。
//!
//! drain 侧提示类发送失败后落盘（store outbox 表，kind=`feishu_text`），由本
//! 后台泵周期拉取到期行重发；成败信号来自 [`super::drain::send_drain_text_result`]
//! 的直发形态。

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tracing::{error, info, warn};

use imagent_core::ConvId;
use open_lark::CoreConfig;

use super::drain::send_drain_text_result_with_uuid;

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

/// v1.21 outbox 泵：每 10s 拉到期行重发（kind=feishu_text）。指数退避
/// 15s→1h 封顶；成功删行；超 [`imagent_store::OUTBOX_MAX_ATTEMPTS`] 放弃并
/// error 留痕（约 10h 仍不达——继续保留只会撑爆表，文案已过时效）。
pub(super) async fn outbox_pump(
    store: imagent_store::Store,
    core_config: Arc<CoreConfig>,
    token_lock: Arc<RwLock<Option<(String, Instant)>>>,
    app_id: String,
    app_secret: String,
) {
    const TICK: Duration = Duration::from_secs(10);
    loop {
        tokio::time::sleep(TICK).await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let due = match store.due_outbox(now, 10).await {
            Ok(d) => d,
            Err(e) => {
                warn!(target: "feishu", error = %e, "outbox 拉取失败（本轮跳过）");
                continue;
            }
        };
        for row in due {
            // P3o（code-review v14）：未知 kind 不再 continue 原地空转（该行
            // next_try 未变，会**永久占住** LIMIT 10 让真正的 feishu_text 行
            // 饿死）——推后 1h 重看（用现有 outbox_mark_failed 语义实现，
            // attempts 递增最终会被 OUTBOX_MAX_ATTEMPTS 回收，双保险）。
            if row.kind != "feishu_text" {
                warn!(target: "feishu", id = row.id, kind = %row.kind, "outbox 未知 kind，推后 1h 重看（不阻塞到期队列）");
                let _ = store.outbox_mark_failed(row.id, now + 3600).await;
                continue;
            }
            let Some((conv, text, uuid)) = parse_outbox_payload(&row.payload) else {
                error!(target: "feishu", id = row.id, "outbox 行解析失败，放弃");
                let _ = store.outbox_mark_sent(row.id).await;
                continue;
            };
            // P2-5：落盘时的 uuid 透传（旧行无 uuid → 现生成，整个重试周期内
            // 每次重试共用本键——重试间换键同样会双发）。
            let idem = uuid.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            // send_drain_text 内部已吞错误（warn），泵需要成败信号——直接
            // 调内部闭包等价逻辑：这里取 Result 的直发形态。
            let sent = send_drain_text_result_with_uuid(
                &core_config,
                &token_lock,
                &app_id,
                &app_secret,
                &ConvId(conv.clone()),
                &text,
                Some(&idem),
            )
            .await;
            match sent {
                Ok(()) => {
                    info!(target: "feishu", id = row.id, conv_id = conv, attempts = row.attempts, "outbox 重发成功");
                    let _ = store.outbox_mark_sent(row.id).await;
                }
                Err(e) => {
                    // 15s × 2^attempts，封顶 1h。
                    let backoff = (15i64 << row.attempts.min(8)).clamp(15, 3600);
                    let kept = store.outbox_mark_failed(row.id, now + backoff).await;
                    match kept {
                        Ok(true) => {
                            warn!(target: "feishu", id = row.id, attempts = row.attempts, error = %e, "outbox 重发失败（退避后再试）")
                        }
                        Ok(false) => {
                            error!(target: "feishu", id = row.id, attempts = row.attempts, conv_id = conv, "outbox 重试耗尽，放弃（提示丢失）")
                        }
                        Err(e2) => {
                            warn!(target: "feishu", id = row.id, error = %e2, "outbox 状态更新失败")
                        }
                    }
                }
            }
        }
    }
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
}
