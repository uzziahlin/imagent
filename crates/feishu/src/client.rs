//! 飞书长连接驱动 + 发消息（HTTP OpenAPI）。
//!
//! 职责拆分（与 wecom 不同）：
//! - **收**：[`FeishuWsClient::run`] 驱动 `open-lark` 的 `LarkWsClient::open`，事件
//!   payload bytes 通过 channel 推给上层 drain task。SDK 不内置重连，外层指数退避
//!   loop 兜底（照 `wecom/client.rs` 的退避策略）。
//! - **发**：[`send_text_msg`] 走独立 HTTP `CreateMessageRequest`（不经 WS）。
//! - **token**：[`fetch_token`] 手动取 `tenant_access_token`（配合低层发消息）。
//!
//! 错误统一 `CoreError::Platform("feishu", _)`，日志 target 统一 `"feishu"`。

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;
use tracing::{info, warn};

use open_lark::auth::AuthService;
use open_lark::communication::im::v1::message::create::{CreateMessageBody, CreateMessageRequest};
use open_lark::communication::im::v1::message::patch::PatchMessageCardRequest;
use open_lark::communication::im::v1::message::resource::get::{
    GetMessageResourceRequest, MessageResourceType,
};
use open_lark::communication::im::v1::message::models::ReceiveIdType;
use open_lark::ws_client::{EventDispatcherHandler, LarkWsClient, WsClientError};
use open_lark::{CoreConfig, RequestOption};

use crate::proto::ReceiveIdKind;

/// 平台名常量（错误构造用）。
const PLATFORM: &str = "feishu";
/// 重连退避上限（照 wecom）。
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// 飞书长连接驱动：外层重连 loop 包住 SDK 的 `LarkWsClient::open`。
///
/// SDK 不内置重连（官方 example #441 明说），断开后由本 loop 指数退避重连
/// （1s → 2s → … 封顶 30s）。事件 payload 通过 `payload_tx` 推给上层 drain task。
pub struct FeishuWsClient {
    /// 长连接配置（含 app_id/app_secret，SDK 自动认证 + token cache）。
    ws_config: Arc<open_lark::Config>,
}

impl FeishuWsClient {
    /// 构造长连接驱动。
    pub fn new(ws_config: Arc<open_lark::Config>) -> Self {
        Self { ws_config }
    }

    /// 主循环：重连外层 loop。`LarkWsClient::open` 阻塞运行会话，结束/断开才返回，
    /// 返回即按指数退避 sleep 后重连。
    pub async fn run(self, payload_tx: mpsc::UnboundedSender<Vec<u8>>) {
        let mut backoff = Duration::from_secs(1);
        loop {
            let handler = EventDispatcherHandler::builder()
                .payload_sender(payload_tx.clone())
                .build();
            match LarkWsClient::open(self.ws_config.clone(), handler).await {
                Ok(()) => {
                    info!(target: "feishu", "长连接正常结束，重连");
                    backoff = Duration::from_secs(1);
                }
                Err(WsClientError::ConnectionClosed { reason }) => {
                    warn!(target: "feishu", ?reason, "长连接关闭，重连");
                }
                Err(e) => {
                    warn!(target: "feishu", error = %e, "长连接异常，重连");
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_CAP);
        }
    }
}

/// 发送一条文本消息（HTTP OpenAPI，低层写法，手动注入 token）。
///
/// `core_config` 为发消息用配置；`token` 为当前 `tenant_access_token`；
/// `receive_id`/`kind` 决定 `receive_id_type`（OpenId/ChatId）。
pub async fn send_text_msg(
    core_config: &CoreConfig,
    token: &str,
    receive_id: &str,
    kind: ReceiveIdKind,
    text: &str,
) -> imagent_core::Result<()> {
    let body = CreateMessageBody {
        receive_id: receive_id.to_string(),
        msg_type: "text".to_string(),
        content: json!({ "text": text }).to_string(),
        uuid: None,
    };
    let id_type = match kind {
        ReceiveIdKind::OpenId => ReceiveIdType::OpenId,
        ReceiveIdKind::ChatId => ReceiveIdType::ChatId,
    };
    let option = RequestOption::builder()
        .tenant_access_token(token.to_string())
        .build();
    CreateMessageRequest::new(core_config.clone())
        .receive_id_type(id_type)
        .execute_with_options(body, option)
        .await
        .map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("send_message: {e}"))
        })?;
    Ok(())
}
/// 发送交互卡片，返回 message_id（供后续 [`patch_card`] 增量更新）。
///
/// `card_json` 为 [`crate::card::render_card`] 产出的 CardKit JSON 字符串，直接作为
/// `msg_type = "interactive"` 的 content。返回的 Value 已是信封 `data` 内容，message_id
/// 在顶层（SDK 已拆 `{"code","msg","data"}` 信封）。
pub async fn send_card_msg(
    core_config: &CoreConfig,
    token: &str,
    receive_id: &str,
    kind: ReceiveIdKind,
    card_json: &str,
) -> imagent_core::Result<Option<String>> {
    let body = CreateMessageBody {
        receive_id: receive_id.to_string(),
        msg_type: "interactive".to_string(),
        content: card_json.to_string(),
        uuid: None,
    };
    let id_type = match kind {
        ReceiveIdKind::OpenId => ReceiveIdType::OpenId,
        ReceiveIdKind::ChatId => ReceiveIdType::ChatId,
    };
    let option = RequestOption::builder()
        .tenant_access_token(token.to_string())
        .build();
    let resp: serde_json::Value = CreateMessageRequest::new(core_config.clone())
        .receive_id_type(id_type)
        .execute_with_options(body, option)
        .await
        .map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("send_card: {e}"))
        })?;
    // resp 已是 data 内容；message_id 在顶层（非 data.message_id）。
    let message_id = resp
        .get("message_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    Ok(message_id)
}

/// 增量更新（patch）已发卡片。`card_json` 为新的 CardKit JSON 字符串。
pub async fn patch_card(
    core_config: &CoreConfig,
    token: &str,
    message_id: &str,
    card_json: &str,
) -> imagent_core::Result<()> {
    let option = RequestOption::builder()
        .tenant_access_token(token.to_string())
        .build();
    // patch 请求体形态（open-lark patch.rs doc 确认）：
    //   {"content": "<卡片JSON序列化字符串>"}；card_json 已是字符串，直接作 content 值。
    let body = serde_json::json!({ "content": card_json });
    PatchMessageCardRequest::new(core_config.clone())
        .message_id(message_id.to_string())
        .execute_with_options(body, option)
        .await
        .map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("patch_card: {e}"))
        })?;
    Ok(())
}

/// 下载用户发来的消息图片，返回二进制。
///
/// 走「获取消息中的资源文件」接口（`/im/v1/messages/{message_id}/resources/{file_key}?type=image`）。
/// 注意：`GetImage`(`/im/v1/images/{key}`) 只能下「机器人自己上传」的图，用户发来的图用它会被
/// 飞书拒（234001 Invalid request param）。需应用开通 `im:resource` 权限。
pub async fn download_image(
    core_config: &CoreConfig,
    token: &str,
    message_id: &str,
    image_key: &str,
) -> imagent_core::Result<Vec<u8>> {
    let option = RequestOption::builder()
        .tenant_access_token(token.to_string())
        .build();
    GetMessageResourceRequest::new(core_config.clone())
        .message_id(message_id.to_string())
        .file_key(image_key.to_string())
        .resource_type(MessageResourceType::Image)
        .execute_with_options(option)
        .await
        .map_err(|e| imagent_core::CoreError::Platform(PLATFORM, format!("download_image: {e}")))
}

/// 下载用户发来的消息文件，返回二进制。
///
/// 走「获取消息中的资源文件」接口（type=file），理由同 [`download_image`]。需 `im:resource` 权限。
pub async fn download_file(
    core_config: &CoreConfig,
    token: &str,
    message_id: &str,
    file_key: &str,
) -> imagent_core::Result<Vec<u8>> {
    let option = RequestOption::builder()
        .tenant_access_token(token.to_string())
        .build();
    GetMessageResourceRequest::new(core_config.clone())
        .message_id(message_id.to_string())
        .file_key(file_key.to_string())
        .resource_type(MessageResourceType::File)
        .execute_with_options(option)
        .await
        .map_err(|e| imagent_core::CoreError::Platform(PLATFORM, format!("download_file: {e}")))
}

/// 获取 `tenant_access_token`（手动，配合 [`send_text_msg`] 的低层写法）。
///
/// 飞书 token 有效期 2h；上层 platform 持有 token 缓存 lazy 刷新（见 platform.rs）。
pub async fn fetch_token(
    core_config: &CoreConfig,
    app_id: &str,
    app_secret: &str,
) -> imagent_core::Result<String> {
    let token = AuthService::new(core_config.clone())
        .v3()
        .tenant_access_token_internal()
        .app_id(app_id.to_string())
        .app_secret(app_secret.to_string())
        .execute()
        .await
        .map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("fetch token: {e}"))
        })?
        .data
        .tenant_access_token;
    Ok(token)
}

// ---------------------------------------------------------------------------
// 单测：纯逻辑/退避计算。真实 WS / HTTP 需真凭据，不进默认 cargo test。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_caps_at_30s() {
        let cap = Duration::from_secs(30);
        let mut b = Duration::from_secs(1);
        for _ in 0..10 {
            b = (b * 2).min(cap);
        }
        assert_eq!(b, cap);
    }

    #[test]
    fn constants_sane() {
        assert!(BACKOFF_CAP >= Duration::from_secs(30));
        assert_eq!(PLATFORM, "feishu");
    }

    #[tokio::test]
    async fn run_loops_on_connect_failure() {
        // 占位凭据 → LarkWsClient::open 连飞书服务器失败/断开 → 重连。
        // run 应持续重试（永不正常返回）；timeout 触发 = 正常。
        let ws_config = Arc::new(
            open_lark::Config::builder()
                .app_id("placeholder_app_id".to_string())
                .app_secret("placeholder_secret".to_string())
                .base_url("https://open.feishu.cn".to_string())
                .build(),
        );
        let client = FeishuWsClient::new(ws_config);
        let (payload_tx, _payload_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        let res = tokio::time::timeout(
            Duration::from_millis(800),
            client.run(payload_tx),
        )
        .await;
        // run 永不返回 → timeout 触发（Err(Elapsed)）= 正常。
        assert!(res.is_err(), "run 应持续重连而非返回");
    }
}
