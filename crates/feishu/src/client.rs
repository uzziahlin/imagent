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
use open_lark::communication::im::v1::chat::list::ListChatsRequest;
use open_lark::communication::im::v1::image::create::CreateImageRequest;
use open_lark::communication::im::v1::image::models::ImageType;
use open_lark::communication::im::v1::message::create::{CreateMessageBody, CreateMessageRequest};
use open_lark::communication::im::v1::message::models::ReceiveIdType;
use open_lark::communication::im::v1::message::patch::PatchMessageCardRequest;
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

/// 识别限流类错误（HTTP 429 / 飞书频控业务码 230020）。
/// 两种错误串形态都要覆盖：手写 HTTP 路径（"HTTP 429" / "code=230020"）与
/// SDK 路径（open-lark `ApiError` Display = "API错误 {raw_code} {endpoint}: {msg}"，
/// raw_code 即飞书业务码或合成 HTTP status；业务变体则 Debug 打印枚举名）。
fn is_rate_limited_msg(msg: &str) -> bool {
    msg.contains("HTTP 429")
        || msg.contains("code=230020")
        || msg.contains("API错误 429")
        || msg.contains("API错误 230020")
        || msg.contains("业务错误 TooManyRequests")
}

/// 识别 token 失效类错误码（99991661-64/68/79：tenant/app access token 空、格式错、
/// 无效、内部错误）。缓存 token 被服务端提前吊销（app_secret 轮换 / 后台强制失效）
/// 时，TTL 内重用旧值永远失败——platform 层据此清缓存强制刷新重试一次。
pub(crate) fn is_token_invalid_msg(msg: &str) -> bool {
    const TOKEN_INVALID_CODES: [&str; 6] = [
        "99991661", "99991662", "99991663", "99991664", "99991668", "99991679",
    ];
    TOKEN_INVALID_CODES.iter().any(|c| msg.contains(c))
        || msg.to_ascii_lowercase().contains("invalid access token")
}

/// 限流退避重试——500ms → 1s → 2s 最多三次重试，其它错误立即失败。
/// 手写 HTTP 与 SDK 路径通用（识别见 [`is_rate_limited_msg`]）。
macro_rules! retry_on_rate_limit {
    ($body:expr) => {{
        let mut delay = std::time::Duration::from_millis(500);
        loop {
            match $body.await {
                Ok(v) => break Ok(v),
                Err(e) => {
                    if is_rate_limited_msg(&format!("{e}")) && delay <= std::time::Duration::from_secs(2)
                    {
                        tracing::warn!(
                            target: "feishu",
                            backoff_ms = delay.as_millis() as u64,
                            "限流（429/230020），退避后重试"
                        );
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                        continue;
                    }
                    break Err(e);
                }
            }
        }
    }};
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
    retry_on_rate_limit!(async {
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
    })
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
    retry_on_rate_limit!(async {
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
    })
}

/// 增量更新（patch）已发卡片。`card_json` 为新的 CardKit JSON 字符串。
pub async fn patch_card(
    core_config: &CoreConfig,
    token: &str,
    message_id: &str,
    card_json: &str,
) -> imagent_core::Result<()> {
    retry_on_rate_limit!(async {
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
    })
}

// ---------------------------------------------------------------------------
// CardKit 真流式（managed card）：open-lark 0.20 无 cardkit 模块，以下手写 HTTP。
// 链路：create_card_entity 拿 card_id → send_card_ref_msg 发引用消息 →
// patch_card_element 流式更新 markdown 组件（打字机）→ patch_card_settings 关流式。
// ---------------------------------------------------------------------------

/// CardKit API 基址（手写 HTTP；与 open-lark 的 CoreConfig.base_url 默认值一致）。
const CARDKIT_BASE: &str = "https://open.feishu.cn/open-apis/cardkit/v1";

/// 解析 CardKit 响应信封：code 非 0 报错，否则取 `data` 下指定字段的字符串值。
/// P5-第五批：先判 HTTP 状态——429 归一为含「HTTP 429」标记的错误（供
/// retry_on_rate_limit 识别重试；此前直接 json() 解析非 JSON 体，错误串不含
/// 标记导致重试不生效）。
async fn cardkit_resp(resp: reqwest::Response, op: &str) -> imagent_core::Result<String> {
    if resp.status().as_u16() == 429 {
        return Err(imagent_core::CoreError::Platform(
            PLATFORM,
            format!("{op}: HTTP 429"),
        ));
    }
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
    retry_on_rate_limit!(async {
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
                imagent_core::CoreError::Platform(
                    PLATFORM,
                    "create_card_entity: 响应缺 card_id".into(),
                )
            })
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
    retry_on_rate_limit!(async {
        // 真机校准（2026-08）：请求体须为 `partial_element`——组件新配置的 **JSON
        // 字符串**（双重编码，同 create 实体的 card_json）；此前直传 `content` 字段
        // 被 99992402 "field validation failed" 拒绝。
        let partial = json!({ "content": content }).to_string();
        let client = reqwest::Client::new();
        let resp = client
            .patch(format!(
                "{CARDKIT_BASE}/cards/{card_id}/elements/{element_id}"
            ))
            .bearer_auth(token)
            .json(&json!({ "partial_element": partial, "sequence": sequence }))
            .send()
            .await
            .map_err(|e| {
                imagent_core::CoreError::Platform(PLATFORM, format!("patch_card_element: {e}"))
            })?;
        cardkit_resp(resp, "patch_card_element").await.map(|_| ())
    })
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
    retry_on_rate_limit!(async {
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
    })
}

/// 发送引用卡片实体的 interactive 消息，返回 message_id。
///
/// content 为 `{"type":"card","data":{"card_id":"..."}}`（官方 im message create
/// 文档「方式一」；真机校准 2026-08：`type` 必须是 **"card"**——此前写 "card_id"
/// 被 230099/200621 "parse card json err" 拒绝），后续对该实体的 element/settings
/// PATCH 即时反映到这条消息。
pub async fn send_card_ref_msg(
    core_config: &CoreConfig,
    token: &str,
    receive_id: &str,
    kind: ReceiveIdKind,
    card_id: &str,
) -> imagent_core::Result<Option<String>> {
    retry_on_rate_limit!(async {
        let body = CreateMessageBody {
            receive_id: receive_id.to_string(),
            msg_type: "interactive".to_string(),
            content: json!({ "type": "card", "data": { "card_id": card_id } }).to_string(),
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
    })
}

/// 媒体下载大小上限（与 ilink 一致：50MB；防恶意/误发大文件把内存打爆）。
const MEDIA_MAX_BYTES: u64 = 50 * 1024 * 1024;

/// 「获取消息中的资源文件」手写实现（P5 快赢：SDK 版全量缓冲无大小上限）。
/// GET `/im/v1/messages/{message_id}/resources/{file_key}?type=<kind>`，
/// Content-Length 预检 + 流式累计上限（同 ilink 的双重上限做法）。
/// 注意：`GetImage`(`/im/v1/images/{key}`) 只能下「机器人自己上传」的图，用户
/// 发来的图用它会被飞书拒（234001）。需应用开通 `im:resource` 权限。
async fn download_message_resource(
    core_config: &CoreConfig,
    token: &str,
    message_id: &str,
    file_key: &str,
    kind: &str,
) -> imagent_core::Result<Vec<u8>> {
    retry_on_rate_limit!(async {
        let base = core_config.base_url().trim_end_matches('/').to_string();
        let url = format!(
            "{base}/open-apis/im/v1/messages/{message_id}/resources/{file_key}?type={kind}"
        );
        let client = reqwest::Client::new();
        let mut resp = client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| {
                imagent_core::CoreError::Platform(PLATFORM, format!("download resource: {e}"))
            })?;
        if resp.status().as_u16() == 429 {
            // 归一为可被 retry 宏识别的限流标记。
            return Err(imagent_core::CoreError::Platform(
                PLATFORM,
                "download resource: HTTP 429".to_string(),
            ));
        }
        if !resp.status().is_success() {
            return Err(imagent_core::CoreError::Platform(
                PLATFORM,
                format!("download resource: HTTP {}", resp.status()),
            ));
        }
        if let Some(len) = resp.content_length() {
            if len > MEDIA_MAX_BYTES {
                return Err(imagent_core::CoreError::Platform(
                    PLATFORM,
                    format!("download resource too large: {len} > {MEDIA_MAX_BYTES} bytes"),
                ));
            }
        }
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("download resource: {e}"))
        })? {
            if buf.len() as u64 + chunk.len() as u64 > MEDIA_MAX_BYTES {
                return Err(imagent_core::CoreError::Platform(
                    PLATFORM,
                    format!("download resource too large: > {MEDIA_MAX_BYTES} bytes (streamed)"),
                ));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    })
}

/// 下载用户发来的消息图片，返回二进制（带双重大小上限，见
/// [`download_message_resource`]）。
pub async fn download_image(
    core_config: &CoreConfig,
    token: &str,
    message_id: &str,
    image_key: &str,
) -> imagent_core::Result<Vec<u8>> {
    download_message_resource(core_config, token, message_id, image_key, "image").await
}

/// 下载用户发来的消息文件，返回二进制（带双重大小上限，理由同 [`download_image`]）。
pub async fn download_file(
    core_config: &CoreConfig,
    token: &str,
    message_id: &str,
    file_key: &str,
) -> imagent_core::Result<Vec<u8>> {
    download_message_resource(core_config, token, message_id, file_key, "file").await
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
    retry_on_rate_limit!(async {
        // bytes 被请求体消费，重试路径须重建（图片通常 <几 MB，clone 可接受）。
        let bytes = bytes.clone();
        let option = RequestOption::builder()
            .tenant_access_token(token.to_string())
            .build();
        let resp = CreateImageRequest::new(core_config.clone())
            .image_type(ImageType::Message)
            .file_name(file_name)
            .execute_with_options(bytes, option)
            .await
            .map_err(|e| {
                imagent_core::CoreError::Platform(PLATFORM, format!("upload_image: {e}"))
            })?;
        Ok(resp.image_key)
    })
}

/// 上传文件拿 file_key（P6-7 出站文件）：POST /open-apis/im/v1/files（multipart）。
/// SDK 无此 API，raw reqwest（同 reply_message 模式）。bytes 被请求体消费，
/// 重试路径 clone 重建（与 upload_image 同姿态）。
pub async fn upload_file(
    core_config: &CoreConfig,
    token: &str,
    file_name: &str,
    bytes: Vec<u8>,
) -> imagent_core::Result<String> {
    retry_on_rate_limit!(async {
        let bytes = bytes.clone();
        let base = core_config.base_url().trim_end_matches('/').to_string();
        let url = format!("{base}/open-apis/im/v1/files");
        let part = reqwest::multipart::Part::bytes(bytes).file_name(file_name.to_string());
        let form = reqwest::multipart::Form::new()
            .text("file_type", "file")
            .text("file_name", file_name.to_string())
            .part("file", part);
        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .bearer_auth(token)
            .multipart(form)
            .send()
            .await
            .map_err(|e| {
                imagent_core::CoreError::Platform(PLATFORM, format!("upload_file: {e}"))
            })?;
        if resp.status().as_u16() == 429 {
            return Err(imagent_core::CoreError::Platform(
                PLATFORM,
                "upload_file: HTTP 429".to_string(),
            ));
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("upload_file: {e}"))
        })?;
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
            return Err(imagent_core::CoreError::Platform(
                PLATFORM,
                format!("upload_file: code={code} msg={msg}"),
            ));
        }
        v.get("data")
            .and_then(|d| d.get("file_key"))
            .and_then(|k| k.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                imagent_core::CoreError::Platform(PLATFORM, "upload_file: 响应缺 file_key".into())
            })
    })
}

/// 发送文件消息（P6-7，msg_type=file），content 为 `{"file_key":"..."}`。
pub async fn send_file_msg(
    core_config: &CoreConfig,
    token: &str,
    receive_id: &str,
    kind: ReceiveIdKind,
    file_key: &str,
) -> imagent_core::Result<()> {
    retry_on_rate_limit!(async {
        let body = CreateMessageBody {
            receive_id: receive_id.to_string(),
            msg_type: "file".to_string(),
            content: json!({ "file_key": file_key }).to_string(),
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
                imagent_core::CoreError::Platform(PLATFORM, format!("send_file_msg: {e}"))
            })?;
        Ok(())
    })
}

/// 发送图片消息（msg_type=image），content 为 `{"image_key":"..."}`。
pub async fn send_image_msg(
    core_config: &CoreConfig,
    token: &str,
    receive_id: &str,
    kind: ReceiveIdKind,
    image_key: &str,
) -> imagent_core::Result<()> {
    retry_on_rate_limit!(async {
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
            .map_err(|e| {
                imagent_core::CoreError::Platform(PLATFORM, format!("send_image_msg: {e}"))
            })?;
        Ok(())
    })
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

/// 列出 bot 已加入的群（P7-A2 `/chat allow-all`）：GET /im/v1/chats 分页聚合。
/// 返回 `(conv 形态 chat_id, 群名)`——conv 形态 = `feishu:<oc_xxx>`，可直接入
/// allowed_chats。上限 200 群（防异常翻页 runaway）。
pub async fn list_joined_chats(
    core_config: &CoreConfig,
    token: &str,
) -> imagent_core::Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut page_token: Option<String> = None;
    for _ in 0..10 {
        // 每页 50 × 至多 10 页 = 500 上限内再按 200 截断。
        let mut req = ListChatsRequest::new(core_config.clone()).page_size(50);
        if let Some(t) = page_token.clone() {
            req = req.page_token(t);
        }
        let option = RequestOption::builder()
            .tenant_access_token(token.to_string())
            .build();
        let resp: serde_json::Value = req.execute_with_options(option).await.map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("list_joined_chats: {e}"))
        })?;
        if let Some(items) = resp.get("items").and_then(|v| v.as_array()) {
            for it in items {
                let chat_id = it.get("chat_id").and_then(|v| v.as_str()).unwrap_or("");
                let name = it.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if !chat_id.is_empty() {
                    out.push((format!("feishu:{chat_id}"), name.to_string()));
                }
            }
        }
        let has_more = resp
            .get("has_more")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        page_token = resp
            .get("page_token")
            .and_then(|v| v.as_str())
            .filter(|t| !t.is_empty())
            .map(String::from);
        if !has_more || page_token.is_none() || out.len() >= 200 {
            break;
        }
    }
    out.truncate(200);
    Ok(out)
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
    retry_on_rate_limit!(async {
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
            .map_err(|e| {
                imagent_core::CoreError::Platform(PLATFORM, format!("reply_comment: {e}"))
            })?;
        // P5-第五批：429 先归一标记（否则非 JSON 体解析错误不含可识别串）。
        if resp.status().as_u16() == 429 {
            return Err(imagent_core::CoreError::Platform(
                PLATFORM,
                "reply_comment: HTTP 429".to_string(),
            ));
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("reply_comment: {e}"))
        })?;
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
            return Err(imagent_core::CoreError::Platform(
                PLATFORM,
                format!("reply_comment: code={code} msg={msg}"),
            ));
        }
        Ok(())
    })
}

/// 话题群内回复（P6-4）：POST /im/v1/messages/{message_id}/reply。
/// `message_id` 为话题根消息；`content` 为对应 msg_type 的 JSON 字符串
/// （与 create 一致，如 `{"text":"…"}` / `{"image_key":"…"}`）。
/// SDK（openlark 0.20）无此 API，raw reqwest（同 reply_comment 模式）。
pub async fn reply_message(
    core_config: &CoreConfig,
    token: &str,
    message_id: &str,
    msg_type: &str,
    content: &str,
) -> imagent_core::Result<Option<String>> {
    retry_on_rate_limit!(async {
        let base = core_config.base_url().trim_end_matches('/').to_string();
        let url = format!("{base}/open-apis/im/v1/messages/{message_id}/reply");
        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .bearer_auth(token)
            .json(&serde_json::json!({
                "msg_type": msg_type,
                "content": content,
            }))
            .send()
            .await
            .map_err(|e| {
                imagent_core::CoreError::Platform(PLATFORM, format!("reply_message: {e}"))
            })?;
        if resp.status().as_u16() == 429 {
            return Err(imagent_core::CoreError::Platform(
                PLATFORM,
                "reply_message: HTTP 429".to_string(),
            ));
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("reply_message: {e}"))
        })?;
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
            return Err(imagent_core::CoreError::Platform(
                PLATFORM,
                format!("reply_message: code={code} msg={msg}"),
            ));
        }
        // P6 遗留补齐：返回回执消息 id（话题内 interactive 卡需要它作 patch 句柄）。
        Ok(v.pointer("/data/message_id")
            .and_then(|m| m.as_str())
            .map(String::from))
    })
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

    /// 第六批：token 失效错误码识别——SDK ApiError Display 形态（"API错误
    /// 99991663 response: ..."）、手写路径形态、以及消息文案形态都要命中；
    /// 无关错误码不能误伤。
    #[test]
    fn token_invalid_msg_detection() {
        for hit in [
            "API错误 99991663 response: Invalid access token for authorization",
            "API错误 99991661 response: tenant access token is empty",
            "send_message: 认证失败: invalid access token for authorization",
            "fetch_bot_open_id: code=99991664 msg=xx",
            "API错误 99991668 response: invalid app_access_token",
        ] {
            assert!(is_token_invalid_msg(hit), "应识别: {hit}");
        }
        for miss in [
            "send_message: API错误 230020 too many request",
            "download resource: HTTP 429",
            "API错误 99991400 bad request",
            "网络错误: connection refused",
        ] {
            assert!(!is_token_invalid_msg(miss), "不应识别: {miss}");
        }
    }

    /// 第六批：限流识别扩展到 SDK 错误串形态（ApiError Display 携带 raw_code；
    /// 业务变体 Debug 打印枚举名 TooManyRequests）。
    #[test]
    fn rate_limit_msg_detection_covers_sdk_forms() {
        for hit in [
            "download resource: HTTP 429",
            "reply_comment: code=230020",
            "API错误 429 response: too many request",
            "API错误 230020 response: reach rate limit",
            "业务错误 TooManyRequests: xx",
        ] {
            assert!(is_rate_limited_msg(hit), "应识别: {hit}");
        }
        for miss in [
            "API错误 99991663",
            "send_message: 网络错误",
            "API错误 400 bad request",
        ] {
            assert!(!is_rate_limited_msg(miss), "不应识别: {miss}");
        }
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
