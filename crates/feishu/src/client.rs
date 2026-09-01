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

use crate::proto::{MergedForwardItem, ReceiveIdKind};

/// 平台名常量（错误构造用）。
const PLATFORM: &str = "feishu";

/// M1/M6（code-review v8）：模块级共享 reqwest client——此前 13 处裸
/// `Client::new()` 每请求新建（流式卡 patch 每帧完整 TCP+TLS 握手、TIME_WAIT
/// 堆积），且全部无超时（连接黑洞 → 调用永久挂起）。
/// - [`api_client`]：JSON API（发消息/卡片/token）——总超时 30s；
/// - [`dl_client`]：媒体上传/下载（50MB 流式）——仅连接超时 10s，总时长不设
///   （流式大文件合法长传输）。
fn api_client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("reqwest client 构建")
    })
}

fn dl_client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("reqwest client 构建")
    })
}
/// 重连退避上限（照 wecom）。
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// M5（code-review v8）：连接存活 ≥ 该时长后的断开视为「健康断连」，重置退避。
const HEALTHY_CONN_MIN_LIFETIME: Duration = Duration::from_secs(60);

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
            let opened_at = std::time::Instant::now();
            tokio::select! {
                res = LarkWsClient::open(self.ws_config.clone(), handler) => match res {
                    Ok(()) => {
                        info!(target: "feishu", "长连接正常结束，重连");
                        backoff = Duration::from_secs(1);
                    }
                    Err(WsClientError::ConnectionClosed { reason }) => {
                        // M5（code-review v8）：服务端按 PingInterval 例行踢空闲
                        // 连接也走本分支——存活 ≥60s 视为健康断连，重置退避（否则
                        // 例行踢连累计翻倍到 30s 封顶且永不回落，长跑进程偶发
                        // 30s 无响应越来越频繁）。
                        if opened_at.elapsed() >= HEALTHY_CONN_MIN_LIFETIME {
                            info!(target: "feishu", ?reason, uptime_secs = opened_at.elapsed().as_secs(),
                                "长连接健康期后被服务端关闭，重置退避重连");
                            backoff = Duration::from_secs(1);
                        } else {
                            warn!(target: "feishu", ?reason, "长连接关闭，重连");
                        }
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
            // P1：退避加 ±20% 随机 jitter（防多实例同步重连风暴），基础值仍按
            // 指数增长（jitter 不参与翻倍，避免抖动累积漂移）。
            tokio::time::sleep(jittered_backoff(backoff, rand_jitter())).await;
            backoff = (backoff * 2).min(BACKOFF_CAP);
        }
    }
}

/// P1：`[-0.2, 0.2]` 均匀随机 jitter 因子。
fn rand_jitter() -> f64 {
    use rand::Rng;
    rand::thread_rng().gen_range(-0.2f64..=0.2f64)
}

/// P1：退避时长加 ±20% jitter（纯函数，便于单测）。固定退避序列会让多实例
/// 在同一时刻断连后同步重连（重连风暴）。
fn jittered_backoff(base: Duration, factor: f64) -> Duration {
    debug_assert!((-0.2..=0.2).contains(&factor));
    Duration::from_secs_f64((base.as_secs_f64() * (1.0 + factor)).max(0.001))
}

/// 识别限流类错误（HTTP 429 / 飞书频控业务码 230020）。
/// 两种错误串形态都要覆盖：手写 HTTP 路径（"HTTP 429" / "code=230020"）与
/// SDK 路径（open-lark `ApiError` Display = "API错误 {raw_code} {endpoint}: {msg}"，
/// raw_code 即飞书业务码或合成 HTTP status；业务变体则 Debug 打印枚举名）。
pub(crate) fn is_rate_limited_msg(msg: &str) -> bool {
    msg.contains("HTTP 429")
        || msg.contains("code=230020")
        || msg.contains("API错误 429")
        || msg.contains("API错误 230020")
        || msg.contains("业务错误 TooManyRequests")
}

/// 识别「卡片不存在/已删除」类错误（流式卡自愈用，platform 层据此清缓存 +
/// 回报 CARD_HANDLE_LOST 让 core 重发新卡）。
///
/// 覆盖形态（离线按飞书错误码知识取「不存在」类，**待真机校准**补全清单）：
/// - im 消息 patch：`code=230002`（消息/卡片不存在）、msg 含 "not exist"；
/// - CardKit element/settings patch：卡片实体被删后同样回 230002 形态信封；
/// - SDK ApiError Display 形态（"API错误 230002 …"）。
///
/// 刻意不含 300317（sequence 落后，另有自愈路径）与 300318 等。
pub(crate) fn is_card_not_exist_msg(msg: &str) -> bool {
    msg.contains("code=230002")
        || msg.contains("API错误 230002")
        || msg.to_ascii_lowercase().contains("card not exist")
        || msg.contains("卡片不存在")
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

/// 对消息添加表情回复（真机校准 2026-08 验证）：POST
/// `/im/v1/messages/{message_id}/reactions`，返回 reaction_id（终态翻转时先删
/// 旧表情）。emoji key **大小写敏感**：OnIt（在做了）/ DONE / CrossMark——
/// 全大写 ONIT 报 231001（实测）。现有 im:message 权限即可调用。
pub async fn create_reaction(
    core_config: &CoreConfig,
    token: &str,
    message_id: &str,
    emoji_type: &str,
) -> imagent_core::Result<String> {
    let base = core_config.base_url().trim_end_matches('/').to_string();
    let url = format!("{base}/open-apis/im/v1/messages/{message_id}/reactions");
    let client = api_client().clone();
    let resp = client
        .post(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({ "reaction_type": { "emoji_type": emoji_type } }))
        .send()
        .await
        .map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("create_reaction: {e}"))
        })?;
    let v: serde_json::Value = resp.json().await.map_err(|e| {
        imagent_core::CoreError::Platform(PLATFORM, format!("create_reaction: {e}"))
    })?;
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
        return Err(imagent_core::CoreError::Platform(
            PLATFORM,
            format!("create_reaction: code={code} msg={msg} emoji={emoji_type}"),
        ));
    }
    v.get("data")
        .and_then(|d| d.get("reaction_id"))
        .and_then(|r| r.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            imagent_core::CoreError::Platform(
                PLATFORM,
                "create_reaction: 响应缺 reaction_id".into(),
            )
        })
}

/// 删除表情回复（终态翻转前移除旧表情；best-effort——失败仅让旧表情滞留，
/// 不阻塞新表情）。
pub async fn delete_reaction(
    core_config: &CoreConfig,
    token: &str,
    message_id: &str,
    reaction_id: &str,
) -> imagent_core::Result<()> {
    let base = core_config.base_url().trim_end_matches('/').to_string();
    let url = format!("{base}/open-apis/im/v1/messages/{message_id}/reactions/{reaction_id}");
    let client = api_client().clone();
    let resp = client
        .delete(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("delete_reaction: {e}"))
        })?;
    let v: serde_json::Value = resp.json().await.map_err(|e| {
        imagent_core::CoreError::Platform(PLATFORM, format!("delete_reaction: {e}"))
    })?;
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
        return Err(imagent_core::CoreError::Platform(
            PLATFORM,
            format!("delete_reaction: code={code} msg={msg}"),
        ));
    }
    Ok(())
}

/// 应用内加急（buzz）已有消息（真机校准 2026-08）：PATCH
/// `/im/v1/messages/{message_id}/urgent_app`，body `user_id_list`——目标用户
/// 收到应用内强提醒弹窗，**不产生新消息**（强提醒迁移到卡上的关键：审批催办
/// 对审批卡、完成提醒对流式终态卡，替代此前另发一条 buzz 文本）。
///
/// 仅可加急机器人自己发的消息（本网关的卡片均满足）；`im:message` 系权限
/// 缺失 / 未读加急超 200 条等场景接口报错——调用方回退 buzz 文本（fail-soft）。
pub async fn urgent_app_buzz(
    core_config: &CoreConfig,
    token: &str,
    message_id: &str,
    user_open_id: &str,
) -> imagent_core::Result<()> {
    let base = core_config.base_url().trim_end_matches('/').to_string();
    // 真机校准（2026-08）：query 参数为 snake_case `user_id_type`——驼峰
    // userIdType 报 99992402 field validation failed（实测）。
    let url =
        format!("{base}/open-apis/im/v1/messages/{message_id}/urgent_app?user_id_type=open_id");
    let client = api_client().clone();
    let resp = client
        .patch(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({ "user_id_list": [user_open_id] }))
        .send()
        .await
        .map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("urgent_app_buzz: {e}"))
        })?;
    let status = resp.status().as_u16();
    let body = resp.text().await.map_err(|e| {
        imagent_core::CoreError::Platform(PLATFORM, format!("urgent_app_buzz: {e}"))
    })?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
        imagent_core::CoreError::Platform(
            PLATFORM,
            format!("urgent_app_buzz: HTTP {status} 非 JSON 响应: {body:?}"),
        )
    })?;
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
        return Err(imagent_core::CoreError::Platform(
            PLATFORM,
            format!("urgent_app_buzz: code={code} msg={msg}"),
        ));
    }
    Ok(())
}

/// 发送一条文本消息（HTTP OpenAPI，低层写法，手动注入 token）。
///
/// `core_config` 为发消息用配置；`token` 为当前 `tenant_access_token`；
/// `receive_id`/`kind` 决定 `receive_id_type`（OpenId/ChatId）。
///
/// Wave B：`buzz = true` 时 text 消息体附 `buzz` 字段（飞书加急：客户端强提醒
/// 振铃）。**待真机校准**：buzz 字段在部分租户/旧客户端不生效时按未知字段忽略，
/// 退化为普通消息（内容不受影响）。false 时完全不写字段（与既有形态一致）。
pub async fn send_text_msg(
    core_config: &CoreConfig,
    token: &str,
    receive_id: &str,
    kind: ReceiveIdKind,
    text: &str,
    buzz: bool,
) -> imagent_core::Result<()> {
    // 幂等键：每次逻辑发送生成一次，所有限流重试共用（飞书 message create 的
    // uuid 幂等键）——首次请求可能已达服务端，重试换新 uuid 会让用户收到重复消息。
    let idempotency_key = uuid::Uuid::new_v4().to_string();
    retry_on_rate_limit!(async {
        // buzz 仅在 true 时写入（缺省形态与历史一致，防旧端点拒绝未知字段）。
        let content = if buzz {
            json!({ "text": text, "buzz": true }).to_string()
        } else {
            json!({ "text": text }).to_string()
        };
        let body = CreateMessageBody {
            receive_id: receive_id.to_string(),
            msg_type: "text".to_string(),
            content,
            uuid: Some(idempotency_key.clone()),
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
    // 幂等键：同一次逻辑发送的所有重试共用（见 send_text_msg）。
    let idempotency_key = uuid::Uuid::new_v4().to_string();
    retry_on_rate_limit!(async {
        let body = CreateMessageBody {
            receive_id: receive_id.to_string(),
            msg_type: "interactive".to_string(),
            content: card_json.to_string(),
            uuid: Some(idempotency_key.clone()),
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
        let client = api_client().clone();
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
/// **不重试**（限流丢帧策略，安全批次）：429/230020 立即返回错误（含可被
/// [`is_rate_limited_msg`] 识别的标记），由调用方丢弃本帧——流式主循环不能被
/// 退避 sleep 阻塞（会卡住 agent chunk 消费），下个节流窗自然再发新帧。普通
/// 发送消息类调用仍走 retry_on_rate_limit（用户可见消息不能丢）。
pub async fn patch_card_element(
    token: &str,
    card_id: &str,
    element_id: &str,
    content: &str,
    sequence: i64,
) -> imagent_core::Result<()> {
    // 真机校准（2026-08）：请求体须为 `partial_element`——组件新配置的 **JSON
    // 字符串**（双重编码，同 create 实体的 card_json）；此前直传 `content` 字段
    // 被 99992402 "field validation failed" 拒绝。
    let partial = json!({ "content": content }).to_string();
    let client = api_client().clone();
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
}

/// 更新卡片配置（结束流式：`settings_json` 传 `{"config":{"streaming_mode":false}}`）。
///
/// `sequence` 与 element PATCH **共用**同一 card_id 的严格递增计数（不递增报 300317）。
/// **不重试**（限流丢帧策略，语义同 [`patch_card_element`] 的说明）。
pub async fn patch_card_settings(
    token: &str,
    card_id: &str,
    settings_json: &str,
    sequence: i64,
) -> imagent_core::Result<()> {
    let client = api_client().clone();
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

// Wave B-6：独立的 send_card_ref_msg 已删除——CardKit 实体引用消息统一走
// platform 的 send_interactive_anchored（有回复锚点时 reply API 引用发起消息，
// 否则 send_card_msg create；content 形态 {"type":"card","data":{"card_id":…}}
// 两路同构，见 platform.rs）。

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
        let client = dl_client().clone();
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
            // file_type 合法枚举 opus/mp4/pdf/doc/xls/ppt/stream（飞书文档）；
            // 通用文件用 stream——"file" 不在枚举内，真机报 234001。
            .text("file_type", "stream")
            .text("file_name", file_name.to_string())
            .part("file", part);
        let client = dl_client().clone();
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

/// W3-1：语音转文字（真机校准修订）——POST
/// `/open-apis/speech_to_text/v1/speech/file_recognize`，**JSON body**（非
/// multipart）：`speech.speech` = base64(pcm)，`config` = `{file_id(16 位),
/// format:"pcm", engine_type:"16k_auto"}`。官方支持 60 秒内音频，覆盖 IM
/// 语音条场景；需在飞书后台申请「语音识别(speech_to_text:speech)」权限。
///
/// 飞书语音条下载产物为 ogg/opus，接口仅收 16k s16le mono pcm——先经 ffmpeg
/// 子进程转码（缺 ffmpeg / 转码失败由调用方 fail-soft 回退）。响应
/// `{"code":0,"data":{"recognition_text":"…"}}`。单次不重试：99991400 实测为
/// HTTP 400 形态（标准频控是 429），属「特殊频控」——租户未开通语音服务/
/// 免费版门禁，立即重试无意义，报可行动原因。
pub async fn transcribe_audio(
    core_config: &CoreConfig,
    token: &str,
    bytes: Vec<u8>,
) -> imagent_core::Result<String> {
    let pcm = ogg_to_pcm(bytes).await?;
    let body = asr_request_body(&pcm);
    let base = core_config.base_url().trim_end_matches('/').to_string();
    let url = format!("{base}/open-apis/speech_to_text/v1/speech/file_recognize");
    let client = api_client().clone();
    let resp = client
        .post(&url)
        .bearer_auth(token)
        .header("Content-Type", "application/json; charset=utf-8")
        .body(body)
        .send()
        .await
        .map_err(|e| {
            imagent_core::CoreError::Platform(PLATFORM, format!("transcribe_audio: {e}"))
        })?;
    if resp.status().as_u16() == 429 {
        return Err(imagent_core::CoreError::Platform(
            PLATFORM,
            "transcribe_audio: HTTP 429".to_string(),
        ));
    }
    // 先取原始文本再解析：非 JSON 响应（如网关 404 页）时报出状态码与原文
    // 片段，而非无信息量的 "error decoding response body"（本次校准的实际教训）。
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|e| {
        imagent_core::CoreError::Platform(PLATFORM, format!("transcribe_audio: {e}"))
    })?;
    parse_asr_response(status, &text)
}

/// 解析 ASR 响应（纯函数，供单测）：非 JSON 报状态码+原文截断；code!=0 报
/// 错误码（99991400 附开通指引）；成功取 `data.recognition_text`。
fn parse_asr_response(status: u16, body: &str) -> imagent_core::Result<String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|_| {
        imagent_core::CoreError::Platform(
            PLATFORM,
            format!(
                "transcribe_audio: HTTP {status} 非 JSON 响应: {}",
                truncate_for_error(body, 120)
            ),
        )
    })?;
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
        // 99991400（HTTP 400 形态）：特殊频控门禁——语音识别权限未开通/未发布，
        // 或企业为免费版（官方文档：免费版不支持本接口）。
        let hint = if code == 99991400 {
            "（检查飞书后台已开通「语音识别」权限并发布版本；免费版企业不支持本接口）"
        } else {
            ""
        };
        return Err(imagent_core::CoreError::Platform(
            PLATFORM,
            format!("transcribe_audio: code={code} msg={msg}{hint}"),
        ));
    }
    v.get("data")
        .and_then(|d| d.get("recognition_text"))
        .and_then(|t| t.as_str())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            imagent_core::CoreError::Platform(PLATFORM, "transcribe_audio: 响应缺文本".into())
        })
}

/// 构造 file_recognize 的 JSON 请求体：pcm base64 + 16 位随机 file_id
///（官方要求：仅字母数字和下划线的 16 位字符串，用户生成）。
fn asr_request_body(pcm: &[u8]) -> String {
    use base64::Engine;
    use rand::Rng;
    let speech = base64::engine::general_purpose::STANDARD.encode(pcm);
    let file_id: String = rand::thread_rng()
        .sample_iter(rand::distributions::Alphanumeric)
        .take(16)
        .map(char::from)
        .collect();
    serde_json::json!({
        "speech": { "speech": speech },
        "config": {
            "file_id": file_id,
            "format": "pcm",
            "engine_type": "16k_auto",
        }
    })
    .to_string()
}

/// ogg/opus → 16k s16le mono pcm（ffmpeg 子进程，stdin/stdout 管道）。
/// ffmpeg 写 stdout 与主进程写 stdin 需并发，否则管道缓冲区写满会死锁。
async fn ogg_to_pcm(bytes: Vec<u8>) -> imagent_core::Result<Vec<u8>> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg("pipe:0")
        .arg("-f")
        .arg("s16le")
        .arg("-acodec")
        .arg("pcm_s16le")
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg("16000")
        .arg("pipe:1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                imagent_core::CoreError::Platform(
                    PLATFORM,
                    "语音转写需要 ffmpeg（未安装）：brew install ffmpeg".into(),
                )
            } else {
                imagent_core::CoreError::Platform(PLATFORM, format!("启动 ffmpeg 失败: {e}"))
            }
        })?;
    let mut stdin = child.stdin.take().expect("ffmpeg stdin piped");
    let feed = tokio::spawn(async move {
        // 写完即随 task 结束 drop stdin → EOF，ffmpeg 正常收尾。
        let _ = stdin.write_all(&bytes).await;
    });
    let out = child.wait_with_output().await.map_err(|e| {
        imagent_core::CoreError::Platform(PLATFORM, format!("等待 ffmpeg 失败: {e}"))
    })?;
    let _ = feed.await;
    if !out.status.success() {
        return Err(imagent_core::CoreError::Platform(
            PLATFORM,
            format!(
                "ffmpeg 转码失败: {}",
                truncate_for_error(&String::from_utf8_lossy(&out.stderr), 120)
            ),
        ));
    }
    if out.stdout.is_empty() {
        return Err(imagent_core::CoreError::Platform(
            PLATFORM,
            "ffmpeg 转码产物为空".into(),
        ));
    }
    Ok(out.stdout)
}

/// 错误信息内嵌的原文片段截断（多字节字符安全）。
fn truncate_for_error(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect::<String>()
        + if s.chars().count() > max_chars {
            "…"
        } else {
            ""
        }
}

/// 发送文件消息（P6-7，msg_type=file），content 为 `{"file_key":"..."}`。
pub async fn send_file_msg(
    core_config: &CoreConfig,
    token: &str,
    receive_id: &str,
    kind: ReceiveIdKind,
    file_key: &str,
) -> imagent_core::Result<()> {
    // 幂等键：同一次逻辑发送的所有重试共用（见 send_text_msg）。
    let idempotency_key = uuid::Uuid::new_v4().to_string();
    retry_on_rate_limit!(async {
        let body = CreateMessageBody {
            receive_id: receive_id.to_string(),
            msg_type: "file".to_string(),
            content: json!({ "file_key": file_key }).to_string(),
            uuid: Some(idempotency_key.clone()),
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
    // 幂等键：同一次逻辑发送的所有重试共用（见 send_text_msg）。
    let idempotency_key = uuid::Uuid::new_v4().to_string();
    retry_on_rate_limit!(async {
        let body = CreateMessageBody {
            receive_id: receive_id.to_string(),
            msg_type: "image".to_string(),
            content: json!({ "image_key": image_key }).to_string(),
            uuid: Some(idempotency_key.clone()),
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
    let client = api_client().clone();
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
    // 真机校准（2026-08-30）：bot/v3/info 响应为顶层 {"bot":{"open_id":…}}，
    // 无 data 包装（离线按 data.open_id 建模落空 → @ 过滤长期弱化运行）。
    v.pointer("/bot/open_id")
        .or_else(|| v.pointer("/data/open_id"))
        .and_then(|o| o.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            imagent_core::CoreError::Platform(
                PLATFORM,
                "fetch_bot_open_id: 响应缺 bot.open_id".into(),
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

/// 「查询合并转发消息列表」一页响应的解析产物（纯函数，mock JSON 可测）。
#[derive(Debug)]
struct MergeForwardPage {
    items: Vec<MergedForwardItem>,
    has_more: bool,
    page_token: Option<String>,
}

/// 解析一页「查询合并转发消息列表」响应（信封 code != 0 报错）。
///
/// 响应字段形态**待真机校准**——离线按飞书文档公开形态建模（`data.items[]` 带
/// message_id / message_type / content / sender{id,id_type,name} / create_time，
/// 分页 has_more + page_token），提取取**宽容姿态**（真机字段名有出入时尽量不炸）：
/// - 类型名兼容 `message_type` / `msg_type`；
/// - 时间戳兼容字符串 / 数字，秒级值（量级 < 1e11）自动 ×1000 归一毫秒；
/// - sender.id 兼容 `id` / `open_id`；字段缺失给默认值不丢整条（转录对残缺
///   条目有占位语义，见 proto::merge_forward_body）。
fn parse_merge_forward_page(v: &serde_json::Value) -> imagent_core::Result<MergeForwardPage> {
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
        return Err(imagent_core::CoreError::Platform(
            PLATFORM,
            format!("list_merge_forward: code={code} msg={msg}"),
        ));
    }
    let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
    let items = data
        .get("items")
        .and_then(|i| i.as_array())
        .map(|arr| arr.iter().filter_map(merge_forward_item_of).collect())
        .unwrap_or_default();
    let has_more = data
        .get("has_more")
        .and_then(|h| h.as_bool())
        .unwrap_or(false);
    let page_token = data
        .get("page_token")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .map(String::from);
    Ok(MergeForwardPage {
        items,
        has_more,
        page_token,
    })
}

/// 单个 item 的宽容提取（非对象跳过；字段缺失给默认值，见
/// [`parse_merge_forward_page`] 的形态说明）。
fn merge_forward_item_of(v: &serde_json::Value) -> Option<MergedForwardItem> {
    let obj = v.as_object()?;
    let str_of = |k: &str| {
        obj.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    let (sender_id, sender_name) = match obj.get("sender").and_then(|s| s.as_object()) {
        Some(s) => (
            s.get("id")
                .and_then(|x| x.as_str())
                .or_else(|| s.get("open_id").and_then(|x| x.as_str()))
                .unwrap_or("")
                .to_string(),
            s.get("name")
                .and_then(|x| x.as_str())
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(String::from),
        ),
        None => (String::new(), None),
    };
    Some(MergedForwardItem {
        message_id: str_of("message_id"),
        message_type: obj
            .get("message_type")
            .and_then(|x| x.as_str())
            .or_else(|| obj.get("msg_type").and_then(|x| x.as_str()))
            .unwrap_or("")
            .to_string(),
        content: str_of("content"),
        sender_id,
        sender_name,
        create_time_ms: merge_forward_create_time(obj.get("create_time")),
    })
}

/// create_time 宽容解析：字符串 / 数字皆可；秒级值（0 < ts < 1e11，毫秒形态最早
/// 1973 年）自动 ×1000 归一毫秒——飞书各 API 时间戳单位不统一，按量级判别。
/// **待真机校准**：真机确认恒为毫秒后可去掉归一。
fn merge_forward_create_time(v: Option<&serde_json::Value>) -> i64 {
    let raw = v
        .and_then(|x| {
            x.as_i64()
                .or_else(|| x.as_str().and_then(|s| s.parse::<i64>().ok()))
        })
        .unwrap_or(0);
    if raw > 0 && raw < 100_000_000_000 {
        raw * 1000
    } else {
        raw
    }
}

/// 查询合并转发消息的子消息列表（合并转发完整支持）：GET
/// `/im/v1/messages/{message_id}/merge_forward`，分页拉全（page_size=50，
/// page_token 翻页，上限 10 页 / 500 条防异常翻页 runaway——同 list_joined_chats
/// 取舍；转录侧另有 8000 字符截断兜底，见 proto::MERGE_FORWARD_TRANSCRIPT_MAX）。
///
/// SDK（open-lark 0.20）无此 API，raw reqwest（同 reply_message 模式）。**每页
/// 单独**走限流重试（整循环重试会重复拉已得页，浪费配额）。需 `im:message`
/// 读权限（真机确认：事件侧已有读权限通常即覆盖，若拉取报权限错误需在后台
/// 补开对应读权限并发布版本）。响应字段形态**待真机校准**（宽容提取见
/// [`parse_merge_forward_page`]）。
pub async fn list_merge_forward(
    core_config: &CoreConfig,
    token: &str,
    message_id: &str,
) -> imagent_core::Result<Vec<MergedForwardItem>> {
    let base = core_config.base_url().trim_end_matches('/').to_string();
    let mut out: Vec<MergedForwardItem> = Vec::new();
    let mut page_token: Option<String> = None;
    for _ in 0..10 {
        let page = retry_on_rate_limit!(async {
            let mut req = api_client()
                .clone()
                .get(format!(
                    "{base}/open-apis/im/v1/messages/{message_id}/merge_forward"
                ))
                .bearer_auth(token)
                .query(&[("page_size", "50")]);
            if let Some(t) = page_token.as_deref() {
                req = req.query(&[("page_token", t)]);
            }
            let resp = req.send().await.map_err(|e| {
                imagent_core::CoreError::Platform(PLATFORM, format!("list_merge_forward: {e}"))
            })?;
            // 429 先归一标记（否则非 JSON 体解析错误不含可识别串，重试不生效）。
            if resp.status().as_u16() == 429 {
                return Err(imagent_core::CoreError::Platform(
                    PLATFORM,
                    "list_merge_forward: HTTP 429".to_string(),
                ));
            }
            let v: serde_json::Value = resp.json().await.map_err(|e| {
                imagent_core::CoreError::Platform(PLATFORM, format!("list_merge_forward: {e}"))
            })?;
            parse_merge_forward_page(&v)
        })?;
        out.extend(page.items);
        if !page.has_more || page.page_token.is_none() || out.len() >= 500 {
            break;
        }
        page_token = page.page_token;
    }
    out.truncate(500);
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
    reply_comment_nodes(
        core_config,
        token,
        file_token,
        comment_id,
        serde_json::json!([{ "type": "text", "text": text }]),
    )
    .await
}

/// [`reply_comment`] 的内容实体数组版：`content_nodes` 为与事件侧 content 同构的
/// JSON 数组（text / at / img 等节点）。评论线程发图用——img 实体带上传产出的
/// image_key（**待真机校准**：离线无法确认评论 img 实体的资源字段名，先按
/// `{"type":"img","file_key":<image_key>}` 最合理形态实现，真机如被拒再校准）。
pub async fn reply_comment_nodes(
    core_config: &CoreConfig,
    token: &str,
    file_token: &str,
    comment_id: &str,
    content_nodes: serde_json::Value,
) -> imagent_core::Result<()> {
    retry_on_rate_limit!(async {
        let base = core_config.base_url().trim_end_matches('/').to_string();
        let url = format!(
            "{base}/open-apis/drive/v1/files/{file_token}/comments/{comment_id}/replies?user_id_type=open_id"
        );
        let client = api_client().clone();
        let resp = client
            .post(&url)
            .bearer_auth(token)
            .json(&serde_json::json!({ "content": content_nodes }))
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
        let client = api_client().clone();
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

    /// P1：jitter 后退避始终落在 ±20% 区间内（防同步重连风暴但不漂移）。
    #[test]
    fn jitter_keeps_backoff_within_20pct() {
        let base = Duration::from_secs(10);
        assert_eq!(jittered_backoff(base, 0.2), Duration::from_secs(12));
        assert_eq!(jittered_backoff(base, -0.2), Duration::from_secs(8));
        for _ in 0..200 {
            let j = jittered_backoff(base, rand_jitter());
            assert!(
                j >= Duration::from_secs(8) && j <= Duration::from_secs(12),
                "j={j:?}"
            );
        }
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

    /// 卡片不存在类错误识别（流式卡自愈触发条件）：im patch 的 230002 各错误串
    /// 形态都要命中；300317（sequence 落后，另有自愈）与普通错误不误伤。
    #[test]
    fn card_not_exist_msg_detection() {
        for hit in [
            "patch_card: code=230002 msg=card not exist",
            "patch_card_element: code=230002 msg=...",
            "API错误 230002 response: message not found",
            "patch_card: Card not exist",
            "patch_card: 卡片不存在",
        ] {
            assert!(is_card_not_exist_msg(hit), "应识别: {hit}");
        }
        for miss in [
            "patch_card_element: code=300317 msg=sequence error",
            "send_card: code=230020 too many request",
            "网络错误: connection refused",
        ] {
            assert!(!is_card_not_exist_msg(miss), "不应识别: {miss}");
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

    /// 合并转发列表响应解析（mock JSON）：信封 code!=0 报错；字段宽容提取
    /// （msg_type 别名、字符串/数字/秒级时间戳、sender.id/open_id、name 缺省）。
    #[test]
    fn merge_forward_page_parsing() {
        let ok = serde_json::json!({
            "code": 0,
            "msg": "success",
            "data": {
                "items": [
                    {
                        "message_id": "om_sub1",
                        "message_type": "text",
                        "content": "{\"text\":\"你好\"}",
                        "create_time": "1787912345678",
                        "sender": { "id": "ou_alice", "id_type": "open_id", "name": "Alice" }
                    },
                    {
                        "message_id": "om_sub2",
                        "msg_type": "image",
                        "content": "{\"image_key\":\"img_v3_x\"}",
                        "create_time": 1787912340,
                        "sender": { "open_id": "ou_bobxxxxxxxxxxxx" }
                    }
                ],
                "has_more": true,
                "page_token": "tok_2"
            }
        });
        let page = parse_merge_forward_page(&ok).expect("code=0 应解析成功");
        assert_eq!(page.items.len(), 2);
        assert!(page.has_more);
        assert_eq!(page.page_token.as_deref(), Some("tok_2"));
        // 文本条目：字符串毫秒时间戳原样。
        assert_eq!(page.items[0].message_type, "text");
        assert_eq!(page.items[0].content, "{\"text\":\"你好\"}");
        assert_eq!(page.items[0].sender_name.as_deref(), Some("Alice"));
        assert_eq!(page.items[0].sender_id, "ou_alice");
        assert_eq!(page.items[0].create_time_ms, 1_787_912_345_678);
        // 图片条目：msg_type 别名、数字秒级时间戳 ×1000 归一、name 缺省 None。
        assert_eq!(page.items[1].message_type, "image");
        assert_eq!(page.items[1].sender_name, None);
        assert_eq!(page.items[1].sender_id, "ou_bobxxxxxxxxxxxx");
        assert_eq!(page.items[1].create_time_ms, 1_787_912_340_000);

        // 业务错误（消息不存在/权限不足等）：code != 0 → Err，错误串可读。
        let err = serde_json::json!({
            "code": 230002, "msg": "message not exist", "data": null
        });
        let e = parse_merge_forward_page(&err).expect_err("code!=0 应报错");
        let msg = format!("{e}");
        assert!(
            msg.contains("code=230002") && msg.contains("message not exist"),
            "{msg}"
        );

        // 空数据 / 残缺条目：items 缺省空、非对象条目跳过、字段缺失给默认值。
        let empty = serde_json::json!({ "code": 0, "data": {} });
        let page = parse_merge_forward_page(&empty).expect("空数据应成功");
        assert!(page.items.is_empty());
        assert!(!page.has_more);
        assert!(page.page_token.is_none());
        let ragged = serde_json::json!({
            "code": 0,
            "data": { "items": [ "not-an-object", { "message_id": "om_x" } ] }
        });
        let page = parse_merge_forward_page(&ragged).expect("残缺条目不应整页报错");
        assert_eq!(page.items.len(), 1, "非对象跳过，残缺对象保留");
        assert_eq!(page.items[0].message_type, "");
        assert_eq!(page.items[0].create_time_ms, 0);
    }

    /// create_time 量级归一：秒级 ×1000，毫秒原样，非法/缺省 0。
    #[test]
    fn merge_forward_create_time_normalization() {
        assert_eq!(
            merge_forward_create_time(Some(&serde_json::json!(1787912340))),
            1_787_912_340_000,
            "秒级归一毫秒"
        );
        assert_eq!(
            merge_forward_create_time(Some(&serde_json::json!("1787912345678"))),
            1_787_912_345_678,
            "毫秒字符串原样"
        );
        assert_eq!(
            merge_forward_create_time(Some(&serde_json::json!(0))),
            0,
            "0 保持（缺失语义）"
        );
        assert_eq!(
            merge_forward_create_time(Some(&serde_json::json!("abc"))),
            0,
            "非法字符串 → 0"
        );
        assert_eq!(merge_forward_create_time(None), 0, "缺省 → 0");
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

    /// W3-1 校准回归：ASR 请求体为 JSON（非 multipart），speech 为 base64(pcm)，
    /// file_id 恰 16 位字母数字（官方约束），engine/format 固定值。
    #[test]
    fn asr_request_body_shape() {
        use base64::Engine;
        let pcm = vec![0x01u8, 0x02, 0x03, 0x04];
        let body = asr_request_body(&pcm);
        let v: serde_json::Value = serde_json::from_str(&body).expect("合法 JSON");
        let speech = v["speech"]["speech"].as_str().expect("speech.speech");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(speech)
                .unwrap(),
            pcm,
            "speech 字段为 pcm 的 base64"
        );
        let cfg = &v["config"];
        assert_eq!(cfg["format"], "pcm");
        assert_eq!(cfg["engine_type"], "16k_auto");
        let fid = cfg["file_id"].as_str().expect("file_id");
        assert_eq!(fid.len(), 16, "file_id 恰 16 位");
        assert!(
            fid.chars().all(|c| c.is_ascii_alphanumeric()),
            "file_id 仅字母数字"
        );
        // 两次生成的 file_id 不同（用户生成语义，非固定值）。
        assert_ne!(asr_request_body(&pcm), body);
    }

    /// W3-1 校准回归：成功响应取 `data.recognition_text`（**非** `data.text`——
    /// 真机校准前的旧代码按 text 解析会漏取）。空文本视为失败（fail-soft 提示）。
    #[test]
    fn asr_response_parses_recognition_text() {
        let ok = r#"{"code":0,"msg":"success","data":{"recognition_text":" 帮我看看现在几点了 "}}"#;
        assert_eq!(parse_asr_response(200, ok).unwrap(), "帮我看看现在几点了");
        // 旧字段形态（text）不属于本接口——不误读，按缺文本报错。
        let legacy = r#"{"code":0,"data":{"text":"x"}}"#;
        assert!(parse_asr_response(200, legacy).is_err());
    }

    /// W3-1 校准回归：非 JSON 响应（旧路径 404 网关页——本次真机故障的实测
    /// 形态）必须报出状态码与原文片段，而非 "error decoding response body"。
    #[test]
    fn asr_response_surfaces_non_json_body() {
        let err = parse_asr_response(404, "404 page not found").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("HTTP 404"), "msg={msg}");
        assert!(msg.contains("404 page not found"), "msg={msg}");
    }

    /// W3-1 校准回归：99991400（特殊频控门禁——权限未开通/免费版）附开通指引；
    /// 其它错误码原样报出。
    #[test]
    fn asr_response_rate_gate_hint() {
        let gated = r#"{"code":99991400,"msg":"request trigger frequency limit"}"#;
        let msg = format!("{}", parse_asr_response(400, gated).unwrap_err());
        assert!(msg.contains("99991400"), "msg={msg}");
        assert!(msg.contains("语音识别"), "msg={msg}");
        let other = r#"{"code":1040101,"msg":"invalid param"}"#;
        let msg = format!("{}", parse_asr_response(400, other).unwrap_err());
        assert!(
            msg.contains("1040101") && !msg.contains("语音识别"),
            "msg={msg}"
        );
    }

    /// W3-1 校准回归：ogg → 16k s16le mono pcm 转码（本机装了 ffmpeg 才跑；
    /// CI 无 ffmpeg 时跳过——转码正确性由真机校准背书）。
    #[tokio::test]
    async fn ogg_to_pcm_converts_via_ffmpeg() {
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_err()
        {
            return; // 环境无 ffmpeg：跳过（缺依赖路径由 fail-soft 覆盖）。
        }
        // 1 秒 440Hz 正弦 ogg（手搓最小 OggS 页不现实，借 ffmpeg 生成）。
        let gen = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:a",
                "libopus",
                "-f",
                "ogg",
                "pipe:1",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .expect("生成测试 ogg");
        assert!(
            gen.status.success(),
            "gen stderr={}",
            String::from_utf8_lossy(&gen.stderr)
        );
        let pcm = ogg_to_pcm(gen.stdout).await.expect("转码成功");
        // 16k Hz × 1 秒 × 2 字节（s16le）× 单声道 ≈ 32000 字节（容器开销致略少）。
        assert!(
            pcm.len() > 30_000 && pcm.len() < 33_000,
            "pcm_len={}",
            pcm.len()
        );
        assert!(pcm.len() % 2 == 0, "s16le 应为 2 字节对齐");
    }
}
