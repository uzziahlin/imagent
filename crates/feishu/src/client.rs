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
use open_lark::communication::im::v1::image::create::CreateImageRequest;
use open_lark::communication::im::v1::image::models::ImageType;
use open_lark::communication::im::v1::message::create::{CreateMessageBody, CreateMessageRequest};
use open_lark::communication::im::v1::message::models::ReceiveIdType;
use open_lark::communication::im::v1::message::patch::PatchMessageCardRequest;
use open_lark::communication::im::v1::message::resource::get::{
    GetMessageResourceRequest, MessageResourceType,
};
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
/// `reconnect`（P4-7 `/reconnect`）：`notify_one` 唤醒 select 丢弃 open future
/// （连接随 future drop 关闭）→ 退避后重连。退避 sleep 期间通知会存 permit，
/// 下一轮 select 立即消费。
pub struct FeishuWsClient {
    /// 长连接配置（含 app_id/app_secret，SDK 自动认证 + token cache）。
    ws_config: Arc<open_lark::Config>,
    /// `/reconnect` 强制重连信号（与 platform 共享）。
    reconnect: Arc<tokio::sync::Notify>,
}

impl FeishuWsClient {
    /// 构造长连接驱动。
    pub fn new(ws_config: Arc<open_lark::Config>) -> Self {
        Self {
            ws_config,
            reconnect: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// 强制重连信号的共享句柄（platform 的 `Platform::reconnect` 用）。
    pub fn reconnect_handle(&self) -> Arc<tokio::sync::Notify> {
        self.reconnect.clone()
    }

    /// 主循环：重连外层 loop。`LarkWsClient::open` 阻塞运行会话，结束/断开才返回，
    /// 返回即按指数退避 sleep 后重连。
    pub async fn run(self, payload_tx: mpsc::UnboundedSender<Vec<u8>>) {
        let mut backoff = Duration::from_secs(1);
        loop {
            let handler = EventDispatcherHandler::builder()
                .payload_sender(payload_tx.clone())
                .build();
            tokio::select! {
                res = LarkWsClient::open(self.ws_config.clone(), handler) => match res {
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
                },
                _ = self.reconnect.notified() => {
                    info!(target: "feishu", "收到 /reconnect 指令，主动断开重连");
                    backoff = Duration::from_secs(1);
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
        .map_err(|e| imagent_core::CoreError::Platform(PLATFORM, format!("send_message: {e}")))?;
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
        .map_err(|e| imagent_core::CoreError::Platform(PLATFORM, format!("send_card: {e}")))?;
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
        .map_err(|e| imagent_core::CoreError::Platform(PLATFORM, format!("patch_card: {e}")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// CardKit 真流式（managed card）：open-lark 0.20 无 cardkit 模块，以下手写 HTTP。
// 链路：create_card_entity 拿 card_id → send_card_ref_msg 发引用消息 →
// patch_card_element 流式更新 markdown 组件（打字机）→ patch_card_settings 关流式。
// ---------------------------------------------------------------------------

/// CardKit API 基址（手写 HTTP；与 open-lark 的 CoreConfig.base_url 默认值一致）。
const CARDKIT_BASE: &str = "https://open.feishu.cn/open-apis/cardkit/v1";

/// 解析 CardKit 响应信封：code 非 0 报错，否则取 `data` 下指定字段的字符串值。
async fn cardkit_resp(resp: reqwest::Response, op: &str) -> imagent_core::Result<String> {
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| imagent_core::CoreError::Platform(PLATFORM, format!("{op}: {e}")))?;
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
        return Err(imagent_core::CoreError::Platform(
            PLATFORM,
            format!("{op}: code={code} msg={msg}"),
        ));
    }
    Ok(v.get("data").map(|d| d.to_string()).unwrap_or_default())
}

/// 创建 CardKit 卡片实体，返回 `card_id`（managed 流式卡片第一步）。
///
/// `data` 为卡片 JSON **字符串**（官方要求双重编码：外层 JSON + 内层转义字符串）。
/// 需 `cardkit:card:write` 权限；失败时调用方降级走 raw 卡片（msg: 句柄）。
pub async fn create_card_entity(token: &str, card_json: &str) -> imagent_core::Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{CARDKIT_BASE}/cards"))
        .bearer_auth(token)
        .json(&json!({ "type": "card_json", "data": card_json }))
        .send()
        .await
        .map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("create_card_entity: {e}"))
        })?;
    let data = cardkit_resp(resp, "create_card_entity").await?;
    data.parse::<serde_json::Value>()
        .ok()
        .and_then(|v| v.get("card_id").and_then(|c| c.as_str()).map(String::from))
        .ok_or_else(|| {
            imagent_core::CoreError::Platform(PLATFORM, "create_card_entity: 响应缺 card_id".into())
        })
}

/// 流式更新 markdown 组件（全量文本 + 严格递增 sequence，服务端打字机渲染）。
///
/// 仅 markdown 组件可用（`element_id` 对应初始卡片中带 element_id 的 markdown 组件）；
/// 服务端旧文本是新文本前缀时增量打字机输出，否则全量上屏。
pub async fn patch_card_element(
    token: &str,
    card_id: &str,
    element_id: &str,
    content: &str,
    sequence: i64,
) -> imagent_core::Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .patch(format!(
            "{CARDKIT_BASE}/cards/{card_id}/elements/{element_id}"
        ))
        .bearer_auth(token)
        .json(&json!({ "content": content, "sequence": sequence }))
        .send()
        .await
        .map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("patch_card_element: {e}"))
        })?;
    cardkit_resp(resp, "patch_card_element").await.map(|_| ())
}

/// 更新卡片配置（结束流式：`settings_json` 传 `{"config":{"streaming_mode":false}}`）。
///
/// `sequence` 与 element PATCH **共用**同一 card_id 的严格递增计数（不递增报 300317）。
pub async fn patch_card_settings(
    token: &str,
    card_id: &str,
    settings_json: &str,
    sequence: i64,
) -> imagent_core::Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .patch(format!("{CARDKIT_BASE}/cards/{card_id}/settings"))
        .bearer_auth(token)
        .json(&json!({ "settings": settings_json, "sequence": sequence }))
        .send()
        .await
        .map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("patch_card_settings: {e}"))
        })?;
    cardkit_resp(resp, "patch_card_settings").await.map(|_| ())
}

/// 发送引用卡片实体的 interactive 消息，返回 message_id。
///
/// content 为 `{"type":"card_id","data":{"card_id":"..."}}`（官方「方式三」引用形式），
/// 后续对该实体的 element/settings PATCH 即时反映到这条消息。
pub async fn send_card_ref_msg(
    core_config: &CoreConfig,
    token: &str,
    receive_id: &str,
    kind: ReceiveIdKind,
    card_id: &str,
) -> imagent_core::Result<Option<String>> {
    let body = CreateMessageBody {
        receive_id: receive_id.to_string(),
        msg_type: "interactive".to_string(),
        content: json!({ "type": "card_id", "data": { "card_id": card_id } }).to_string(),
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
            imagent_core::CoreError::Platform(PLATFORM, format!("send_card_ref_msg: {e}"))
        })?;
    // resp 已是 data 内容；message_id 在顶层（同 send_card_msg）。
    Ok(resp
        .get("message_id")
        .and_then(|v| v.as_str())
        .map(String::from))
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

/// 上传图片到飞书（用于发图片消息），返回 image_key。
///
/// 走「上传图片」接口（POST /im/v1/images，multipart），需 `im:resource` 权限。
/// 上传后 image_key 即可用于 `msg_type=image` 消息。
pub async fn upload_image(
    core_config: &CoreConfig,
    token: &str,
    file_name: &str,
    bytes: Vec<u8>,
) -> imagent_core::Result<String> {
    let option = RequestOption::builder()
        .tenant_access_token(token.to_string())
        .build();
    let resp = CreateImageRequest::new(core_config.clone())
        .image_type(ImageType::Message)
        .file_name(file_name)
        .execute_with_options(bytes, option)
        .await
        .map_err(|e| imagent_core::CoreError::Platform(PLATFORM, format!("upload_image: {e}")))?;
    Ok(resp.image_key)
}

/// 发送图片消息（msg_type=image），content 为 `{"image_key":"..."}`。
pub async fn send_image_msg(
    core_config: &CoreConfig,
    token: &str,
    receive_id: &str,
    kind: ReceiveIdKind,
    image_key: &str,
) -> imagent_core::Result<()> {
    let body = CreateMessageBody {
        receive_id: receive_id.to_string(),
        msg_type: "image".to_string(),
        content: json!({ "image_key": image_key }).to_string(),
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
        .map_err(|e| imagent_core::CoreError::Platform(PLATFORM, format!("send_image_msg: {e}")))?;
    Ok(())
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
        .map_err(|e| imagent_core::CoreError::Platform(PLATFORM, format!("fetch token: {e}")))?
        .data
        .tenant_access_token;
    Ok(token)
}

/// 取机器人自身 open_id（P5-8：云文档评论 @bot 过滤用）。
/// GET `/open-apis/bot/v3/info`，需 tenant_access_token；成功后由调用方缓存
/// （open_id 随应用固定，进程内取一次即可）。
pub async fn fetch_bot_open_id(
    core_config: &CoreConfig,
    token: &str,
) -> imagent_core::Result<String> {
    let base = core_config.base_url().trim_end_matches('/').to_string();
    let url = format!("{base}/open-apis/bot/v3/info");
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("fetch_bot_open_id: {e}"))
        })?;
    let v: serde_json::Value = resp.json().await.map_err(|e| {
        imagent_core::CoreError::Platform(PLATFORM, format!("fetch_bot_open_id: {e}"))
    })?;
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
        return Err(imagent_core::CoreError::Platform(
            PLATFORM,
            format!("fetch_bot_open_id: code={code} msg={msg}"),
        ));
    }
    v.pointer("/data/open_id")
        .and_then(|o| o.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            imagent_core::CoreError::Platform(
                PLATFORM,
                "fetch_bot_open_id: 响应缺 data.open_id".into(),
            )
        })
}

/// 回复云文档评论（P4-9）：POST `/drive/v1/files/{file_token}/comments/{comment_id}/replies`。
///
/// 手写 HTTP（open-lark 0.20 无 drive 评论模块，同 CardKit 做法）。需应用开通
/// `drive:comment`（查看、创建评论）权限并在事件订阅开启
/// `drive.file.comment.created_v1`（收侧见 proto::parse_comment_event）。
/// body 为评论内容实体数组（与事件侧 content 同构）；`user_id_type=open_id` 与
/// 事件侧 sender 口径一致。
pub async fn reply_comment(
    core_config: &CoreConfig,
    token: &str,
    file_token: &str,
    comment_id: &str,
    text: &str,
) -> imagent_core::Result<()> {
    let base = core_config.base_url().trim_end_matches('/').to_string();
    let url = format!(
        "{base}/open-apis/drive/v1/files/{file_token}/comments/{comment_id}/replies?user_id_type=open_id"
    );
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({
            "content": [{ "type": "text", "text": text }]
        }))
        .send()
        .await
        .map_err(|e| imagent_core::CoreError::Platform(PLATFORM, format!("reply_comment: {e}")))?;
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| imagent_core::CoreError::Platform(PLATFORM, format!("reply_comment: {e}")))?;
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
        return Err(imagent_core::CoreError::Platform(
            PLATFORM,
            format!("reply_comment: code={code} msg={msg}"),
        ));
    }
    Ok(())
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

        let res = tokio::time::timeout(Duration::from_millis(800), client.run(payload_tx)).await;
        // run 永不返回 → timeout 触发（Err(Elapsed)）= 正常。
        assert!(res.is_err(), "run 应持续重连而非返回");
    }
}
