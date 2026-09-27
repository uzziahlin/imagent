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

use super::drain::send_drain_text_result;

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
            if row.kind != "feishu_text" {
                continue; // 未知 kind 不动（当前只有 feishu_text 一种）
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&row.payload) else {
                error!(target: "feishu", id = row.id, "outbox 行解析失败，放弃");
                let _ = store.outbox_mark_sent(row.id).await;
                continue;
            };
            let (Some(conv), Some(text)) = (
                v.get("conv").and_then(|c| c.as_str()),
                v.get("text").and_then(|c| c.as_str()),
            ) else {
                error!(target: "feishu", id = row.id, "outbox 行缺 conv/text，放弃");
                let _ = store.outbox_mark_sent(row.id).await;
                continue;
            };
            // send_drain_text 内部已吞错误（warn），泵需要成败信号——直接
            // 调内部闭包等价逻辑：这里取 Result 的直发形态。
            let sent = send_drain_text_result(
                &core_config,
                &token_lock,
                &app_id,
                &app_secret,
                &ConvId(conv.to_string()),
                text,
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
