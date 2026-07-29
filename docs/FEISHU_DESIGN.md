# 飞书（Feishu / Lark）平台接入设计

> 状态：**方案设计**，待 review；实现时委派 omp。
> 关联：本文件是 [`DESIGN.md`](./DESIGN.md) 三层 + 双抽象的「Platform 扩展」补充，聚焦新增 `feishu` 平台。
> 模板：实现高度参照现有 [`crates/wecom`](../crates/wecom)（企业微信官方 API），飞书与它同属「官方 API + 事件订阅 + 独立 bot 身份」一类。

---

## 1. 背景与目标

imagent 的双抽象（`trait Platform` ↔ `trait Backend`）天然支持换平台。本设计新增**飞书**作为第三个 Platform（与 `ilink` / `wecom` 并列），让用户在飞书私聊或群里 `@bot` 驱动 agent。

**MVP 目标**：文本消息双向打通（私聊 + 群聊 `@bot`）、长连接收消息、HTTP OpenAPI 发消息、token 自动管理、重连、消息去重、白名单鉴权（复用 core）、凭据走 OS keyring。

**非目标（P2 增量）**：飞书交互卡片（流式回复 / 权限审批按钮）、媒体收发、Lark 国际版专项适配。

---

## 2. 选型结论与权衡

| 决策 | 选择 | 依据 |
|---|---|---|
| 收消息模式 | **长连接 WebSocket** | 仅企业自建应用支持；无需公网 IP / 域名，与 imagent「常驻网关进程」模型契合；和 `wecom` 同构。Webhook（需公网 HTTPS）列为可选第二形态。 |
| 协议层 | **引入 `open-lark` SDK**（crate `openlark` v0.20） | 封装飞书自研长连接协议（握手 / 心跳 / 事件分发）+ token 管理 + 消息 OpenAPI。自己撸飞书 WS 协议成本远高于 wecom OpenWS。 |
| 凭据存储 | **OS keyring**（不学 wecom 明文 config） | 飞书 `app_secret` 敏感，参照 `ilink` 范本。 |
| 发消息通道 | **HTTP OpenAPI**（不走 WS） | 飞书收发分离：收走长连接，发走 `POST /open-apis/im/v1/messages`。 |

### open-lark 风险与缓解（必须知情）

1. **非官方、个人维护**（foxzool）：Cargo.toml 用 `=0.20.x` 或 `~0.20` 锁定，`Cargo.lock` 固定具体版本；关注上游 BREAKING。
2. **风格割裂**：open-lark 仅在 `crates/feishu` 内部使用，绝不泄漏到 `core`（依赖倒置不变，core 仍只认 `dyn Platform`）。
3. **长连接不内置重连**（官方 example 注释 #441 明说）：我们写重连 loop，照 `wecom/client.rs:47-71`。
4. **编译开销**：只开 `features = ["auth", "communication", "websocket"]`，按需编译。
5. **token 注入方式需实现时验证**：见 §9。

---

## 3. 飞书 vs wecom：异同对照

| 维度 | wecom（现状） | feishu（本设计） |
|---|---|---|
| 收消息 | WS 长连接，手撸协议（`connect_async` + subscribe 帧） | WS 长连接，`open-lark` 的 `LarkWsClient::open` 驱动 |
| 认证 | `aibot_subscribe` 帧（bot_id + secret） | `Config{app_id, app_secret}`，SDK 自动认证 |
| 入站数据 | `aibot_msg_callback` JSON 帧 | 原始事件 payload bytes（`im.message.receive_v1`） |
| 发消息 | 同一条 WS 连接发 `aibot_send_msg` 帧 | **独立 HTTP** `CreateMessageRequest`（不经 WS） |
| token | 无（固定 bot_id+secret） | `tenant_access_token`，2h 有效，SDK `token cache` 自动刷新 |
| 重连 | `run()` 外层 loop，指数退避 | 同（SDK 不内置，照抄） |
| 媒体 | MVP 空实现 | MVP 空实现 |
| typing | 空实现 | 空实现（飞书无 typing 语义） |

**核心结论**：飞书 client 比 wecom 更简单——**收**交给 SDK，**发**走独立 HTTP（不需要 wecom 那条 outbound channel 把帧委托给 WS）。唯一比 wecom 多的是 token，但被 SDK 吃掉了。

---

## 4. 架构：FeishuPlatform 适配层

### 4.1 数据流

```
                       ┌─────────────── FeishuPlatform ───────────────┐
recv 侧 (收):                                                  send 侧 (发):
  open-lark LarkWsClient::open(config, handler)                  send_text(conv, text)
       │ (长连接，SDK 管 WS 协议 + 心跳)                              │
       ▼ payload_sender(mpsc<Vec<u8>>)                              ▼ split_message 分片
  drain task                                                      for chunk:
       │ parse_message_event(payload) → InboundMessage              send_text_msg(client, receive_id, chunk)
       │ Dedup(event_id)                                                │ HTTP CreateMessageRequest
       ▼ inbound mpsc<InboundMessage>                                   │ token: client token cache
  Platform::recv()  ◄─────────────────────────────────────────────────┘
       │
       ▼ InboundMessage{ conv_id=feishu:<id>, sender=UserId(open_id), text }
   core::Dispatcher（鉴权 / 路由 / 驱动 Backend）   ← 不感知「飞书」，只认 Platform 契约
```

### 4.2 crate 四文件（照 wecom 结构）

```
crates/feishu/
├── Cargo.toml          # openlark 依赖 + 复用 workspace 依赖
└── src/
    ├── lib.rs          # #![forbid(unsafe_code)] + re-export FeishuPlatform, Credentials
    ├── proto.rs        # 纯函数 + serde：事件 payload 结构、解析、conv_id 映射（全单测）
    ├── client.rs       # 长连接驱动：LarkWsClient::open + 重连 loop
    └── platform.rs     # impl Platform：spawn client run + drain task；recv / send_text
```

---

## 5. Platform 契约映射（逐方法）

参照 [`crates/core/src/platform.rs:9-21`](../crates/core/src/platform.rs) 的 5 方法：

| 方法 | 飞书实现 |
|---|---|
| `recv()` | `self.inbound_rx.lock().await.recv().await`，drain task 已把 `InboundMessage` 推入。channel 关闭返 `CoreError::Platform("feishu", ..)`。**照抄 wecom `platform.rs:89-94`。** |
| `send_text(conv, text, _hint)` | `receive_target_from_conv(conv)` → `split_message` 分片 → 每片调 `send_text_msg`（HTTP `CreateMessageRequest`）。hint 忽略（飞书靠 conv_id 里的 open_id/chat_id）。 |
| `send_media(conv, media, hint)` | **MVP 空实现**（`Ok(())`），注释留 TODO（飞书媒体需先 `im/v1/files` 或 `im/v1/images` 上传拿 `file_key`/`image_key` 再发）。 |
| `send_typing(conv, hint)` | **空实现**（飞书协议无 typing）。 |
| `name()` | `"feishu"`。 |

---

## 6. proto.rs：serde 结构与纯函数设计

> 全部纯函数 + serde，无网络无副作用，照 `wecom/proto.rs` 全单测覆盖。这是验收核心。

### 6.1 事件结构（裁剪到关心的字段，基于真实 `im.message.receive_v1` payload）

```rust
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct FeishuEvent {
    pub header: EventHeader,
    pub event: EventBody,
}
#[derive(Debug, Deserialize)]
pub struct EventHeader {
    pub event_type: String,
    pub event_id: Option<String>,   // 去重 key 首选
}
#[derive(Debug, Deserialize)]
pub struct EventBody {
    pub sender: Sender,
    pub message: Message,
    #[serde(default)]
    pub chat: Option<Chat>,
}
#[derive(Debug, Deserialize)]
pub struct Sender { pub sender_id: SenderId }
#[derive(Debug, Deserialize)]
pub struct SenderId { pub open_id: String }   // 鉴权用稳定用户标识
#[derive(Debug, Deserialize)]
pub struct Message {
    pub message_type: String,                 // "text" / "image" / ...
    pub content: String,                      // JSON 字符串，如 {"text":"hi"}
    pub chat_type: String,                    // "p2p" / "group"
    #[serde(default)]
    pub chat_id: Option<String>,
    pub message_id: Option<String>,           // 去重 key 备选
}
#[derive(Debug, Deserialize)]
pub struct Chat { pub chat_id: String }
#[derive(Debug, Deserialize)]
pub struct TextContent { pub text: String }
```

### 6.2 核心纯函数

```rust
use imagent_core::{ConvId, InboundMessage, ReplyHint, UserId};

/// 解析长连接 payload。仅处理 im.message.receive_v1 的文本消息。
/// 返回 (dedup_key, InboundMessage)；非目标事件 / 非文本 / 空文本 / 非法 JSON 返回 None。
pub fn parse_message_event(payload: &[u8]) -> Option<(String, InboundMessage)> {
    let evt: FeishuEvent = serde_json::from_slice(payload).ok()?;
    if evt.header.event_type != "im.message.receive_v1" { return None; }
    if evt.event.message.message_type != "text" { return None; }
    let text = extract_text(&evt.event.message.content)?;
    if text.trim().is_empty() { return None; }

    let open_id = evt.event.sender.sender_id.open_id.clone();
    let (receive_id, _id_type) = receive_target(&evt.event)?;
    let dedup_key = evt.header.event_id.clone()
        .or_else(|| evt.event.message.message_id.clone())
        .unwrap_or_else(|| format!("{}:{}", receive_id, text.len()));
    let msg = InboundMessage {
        conv_id: ConvId(format!("feishu:{}", receive_id)),
        sender: UserId(open_id),
        text: Some(text),
        media: vec![],
        reply_hint: ReplyHint::None,
    };
    Some((dedup_key, msg))
}

/// 从消息 content JSON 提取文本：{"text":"hi"} -> "hi"
pub fn extract_text(content: &str) -> Option<String> {
    serde_json::from_str::<TextContent>(content).ok().map(|c| c.text)
}

/// p2p -> sender.open_id（OpenId）；group -> chat.chat_id（ChatId）。
fn receive_target(event: &EventBody) -> Option<(String, ReceiveIdKind)> {
    if event.message.chat_type == "p2p" {
        let oid = event.sender.sender_id.open_id.clone();
        return if oid.is_empty() { None } else { Some((oid, ReceiveIdKind::OpenId)) };
    }
    if let Some(c) = &event.chat { return Some((c.chat_id.clone(), ReceiveIdKind::ChatId)); }
    if let Some(cid) = &event.message.chat_id { return Some((cid.clone(), ReceiveIdKind::ChatId)); }
    None
}

/// 发消息反向解析：feishu:ou_xxx -> OpenId；其它（feishu:oc_xxx）-> ChatId。
/// 飞书 ID 前缀约定：ou_ = open_id（用户），oc_ = chat_id（群）。
pub fn receive_target_from_conv(conv: &ConvId) -> Option<(String, ReceiveIdKind)> {
    let id = conv.0.strip_prefix("feishu:")?;
    let kind = if id.starts_with("ou_") { ReceiveIdKind::OpenId } else { ReceiveIdKind::ChatId };
    Some((id.to_string(), kind))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveIdKind { OpenId, ChatId }
```

### 6.3 proto 单测（照 echo_bot tests，不连真机）

- `parse_message_event`：p2p 文本 → conv=`feishu:ou_x`、sender=ou_x、text 正确；
- group 文本 → conv=`feishu:oc_x`、sender=ou_x；
- 非 `im.message.receive_v1` / 非文本 / 空文本 / 非法 JSON → `None`；
- `receive_target_from_conv` roundtrip（`ou_` → OpenId，`oc_` → ChatId）；
- `extract_text` 正常 / 非法 JSON。

---

## 7. client.rs：长连接驱动 + 重连

> 职责收窄：**只负责收事件**（驱动 `LarkWsClient::open` + 重连）。发消息不经过这里。

```rust
use std::sync::Arc;
use std::time::Duration;
use open_lark::{Config, ws_client::{EventDispatcherHandler, LarkWsClient, WsClientError}};
use tokio::sync::mpsc;
use tracing::{info, warn};

const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// 飞书长连接驱动：外层重连 loop 包住 SDK 的 LarkWsClient::open。
/// event payload 通过 payload_tx 推给上层 drain task。
pub struct FeishuWsClient { ws_config: Arc<Config> }

impl FeishuWsClient {
    pub async fn run(self, payload_tx: mpsc::Sender<Vec<u8>>) {
        let mut backoff = Duration::from_secs(1);
        loop {
            let handler = EventDispatcherHandler::builder()
                .payload_sender(payload_tx.clone())
                .build();   // 实现时确认 builder 链式 API（见 echo_bot:59-66）
            match LarkWsClient::open(self.ws_config.clone(), handler).await {
                Ok(()) => { info!(target:"feishu","长连接正常结束，重连"); backoff = Duration::from_secs(1); }
                Err(WsClientError::ConnectionClosed { reason }) => {
                    warn!(target:"feishu", ?reason,"长连接关闭，重连");
                }
                Err(e) => { warn!(target:"feishu", error=%e,"长连接异常，重连"); }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_CAP);
        }
    }
}
```

- **重连退避照 `wecom/client.rs:47-71`**（1s → 2s → … 封顶 30s）。
- `payload_tx` 用 `try_send` 还是 `send`：open-lark handler 内部用 `payload_sender`，SDK 行为实现时确认；上层 drain task 消费端要即时（参照 wecom dispatcher 即时 spawn，channel 不会堆积）。

### client.rs 单测（照 wecom/client.rs tests）
- `backoff_caps_at_30s`、`constants_sane`；
- `run` 连无效配置持续重连（`tokio::time::timeout` 包住断言永不返回）—— 注意：真实建连需真凭据，单测用占位凭据 + timeout 断言「持续重试」。

---

## 8. platform.rs：recv / send_text 实现

```rust
use std::sync::Arc;
use async_trait::async_trait;
use open_lark::Client;
use tokio::sync::{mpsc, Mutex};
use tracing::warn;
use imagent_core::{split_message, ConvId, CoreError, Dedup, InboundMessage, MediaRef, Platform, ReplyHint, Result};

const PLATFORM: &str = "feishu";
/// 飞书单条文本消息 content 上限（实现时查官方文档确认精确值，约 30KB）。
const FEISHU_TEXT_MAX: usize = 28_000;

pub struct FeishuPlatform {
    /// 发消息用（HTTP OpenAPI + token cache）。
    client: Arc<Client>,
    inbound_rx: Arc<Mutex<mpsc::Receiver<InboundMessage>>>,
}

impl FeishuPlatform {
    /// 构造并 spawn：① WS client run task（收事件 + 重连）；② drain task（payload → InboundMessage）。
    pub fn new(app_id: String, app_secret: String, base_url: String) -> Result<Self> {
        let client = Arc::new(
            Client::builder().app_id(&app_id).app_secret(&app_secret).base_url(&base_url)
                .build().map_err(|e| CoreError::Platform(PLATFORM, format!("build client: {e}")))?
        );
        let ws_config = Config::builder().app_id(&app_id).app_secret(&app_secret)
            .base_url(&base_url).build();   // enable_token_cache 默认 true

        let (payload_tx, payload_rx) = mpsc::channel::<Vec<u8>>(64);
        let ws = FeishuWsClient { ws_config: Arc::new(ws_config) };
        tokio::spawn(async move { ws.run(payload_tx).await; });

        let (inbound_msg_tx, inbound_msg_rx) = mpsc::channel::<InboundMessage>(64);
        let dedup = Dedup::default();
        tokio::spawn(async move {
            let mut payload_rx = payload_rx;
            while let Some(payload) = payload_rx.recv().await {
                match crate::proto::parse_message_event(&payload) {
                    Some((msgid, msg)) => {
                        if !dedup.check(&msgid) { continue; }   // 重复事件丢弃
                        if inbound_msg_tx.send(msg).await.is_err() { break; }
                    }
                    None => { warn!(target:"feishu","无法解析/非目标事件，丢弃"); }
                }
            }
        });

        Ok(Self { client, inbound_rx: Arc::new(Mutex::new(inbound_msg_rx)) })
    }
}

#[async_trait]
impl Platform for FeishuPlatform {
    async fn recv(&self) -> Result<InboundMessage> {
        self.inbound_rx.lock().await.recv().await.ok_or_else(||
            CoreError::Platform(PLATFORM, "入站 channel 已关闭（client 已退出）".into()))
    }

    async fn send_text(&self, conv: &ConvId, text: &str, _hint: &ReplyHint) -> Result<()> {
        let (receive_id, kind) = crate::proto::receive_target_from_conv(conv)
            .ok_or_else(|| CoreError::Platform(PLATFORM, format!("非法 conv_id: {}", conv.0)))?;
        for chunk in split_message(text, FEISHU_TEXT_MAX) {
            crate::client::send_text_msg(&self.client, &receive_id, kind, &chunk).await?;
        }
        Ok(())
    }

    async fn send_media(&self, _conv: &ConvId, _media: &MediaRef, _hint: &ReplyHint) -> Result<()> {
        // TODO: 飞书媒体需 im/v1/images 或 im/v1/files 上传拿 key 再发。MVP 不支持。
        Ok(())
    }

    async fn send_typing(&self, _conv: &ConvId, _hint: &ReplyHint) -> Result<()> { Ok(()) }

    fn name(&self) -> &'static str { PLATFORM }
}
```

`client::send_text_msg`（HTTP 发消息，放 client.rs）：

```rust
use open_lark::communication::im::v1::message::{
    create::{CreateMessageBody, CreateMessageRequest},
    models::ReceiveIdType,
};
use open_lark::Client;
use serde_json::json;

pub async fn send_text_msg(
    client: &Client, receive_id: &str, kind: ReceiveIdKind, text: &str,
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
    // 实现时确认：高层 Client 是否自动注入 tenant_access_token；
    // 若需手动，则 fetch token + RequestOption::builder().tenant_access_token(t).build()
    // （见 echo_bot:236-260 的低层写法作兜底）。
    CreateMessageRequest::new(/* core_config 或 client.config() */)
        .receive_id_type(id_type)
        .execute_with_options(body, /* option */)
        .await
        .map_err(|e| imagent_core::CoreError::Platform("feishu", format!("send_message: {e}")))?;
    Ok(())
}
```

> ⚠️ 上面 `send_text_msg` 标注了两处**实现时验证**：① 高层 `Client` 发消息的精确调用链（`client.communication.im.v1.message.create...` vs 低层 `CreateMessageRequest::new(core_config)`）；② token 是否自动注入。以 echo_bot 的低层写法（手动 `RequestOption::tenant_access_token`）作为**确定可行的兜底**。

---

## 9. token 管理策略

飞书 `tenant_access_token` 有效期 2h。**推荐分层**：

1. **首选**：高层 `Client`（`enable_token_cache` 默认 `true`），SDK 自动获取 / 缓存 / 刷新 token，零心智负担。发消息时自动注入（实现时验证，见 §8）。
2. **兜底**（若高层不自动注入 token）：在 `FeishuPlatform` 持有一个简单 token manager——`token: Arc<RwLock<(String, Instant)>>`，发消息时 lazy 检查：距过期 < 5min 则用 `AuthService::new(core_config).v3().tenant_access_token_internal().app_id().app_secret().execute()` 重新 fetch（见 echo_bot:262-276）。**不用后台定时刷新 task**，lazy 刷新更简单且无 token 过期窗口。
3. **token 失效兜底**：发消息若返回 token 过期错误，强制刷新一次重试（单次重试，避免死循环）。

> 这是对之前「飞书相比 wecom 的 token 刷新硬骨头」判断的修正：**SDK token cache 把这块吃掉了**，不是飞书实现的主要成本。

---

## 10. conv_id 约定与鉴权

- **私聊（p2p）**：`conv_id = feishu:<open_id>`（如 `feishu:ou_xxx`），`sender = UserId(open_id)`。
- **群聊（group）**：`conv_id = feishu:<chat_id>`（如 `feishu:oc_xxx`，回复发回群里），`sender = UserId(open_id)`（群里发言者的 open_id）。
- **鉴权（core 硬约束）**：core 用 `InboundMessage.sender`（open_id）对 `allowed_senders` 白名单（[`DESIGN.md` 安全设计]）。飞书 bot 任何人都能 `@bot` / 私聊，**白名单不可省**，与 wecom/ilink 一致。
- **群 `@bot`**：飞书订阅 `im.message.receive_v1`，群消息需 `@bot` 才推送（后台事件订阅配置）；私聊直接推送。是否在 platform 层过滤「未 @bot 的群消息」由后台权限 scope 决定（`im:message.group_at_msg`），MVP 依赖后台配置。

---

## 11. 长消息分片

复用 core 的 `split_message(text, max_len)`（[`crates/core/src/message.rs:22-53`]），在 `send_text` 内部按 `FEISHU_TEXT_MAX`（约 28KB，留余量，实现时查官方阈值）切片，片间可加短 sleep（参照 ilink `fragment_interval`，飞书有发送频率限制）。

---

## 12. 凭据存储（keyring）

参照 [`src/main.rs:422-426`]（ilink 范本）用 `imagent_store::Store`：

- `config.toml` 只存 `feishu_app_id`（非敏感，`cli_xxx`）和可选 `feishu_base_url`（Lark 国际版 `https://open.larksuite.com`）。
- `feishu_app_secret` 走 `store.put_credential("feishu", &app_id, &secret)`，启动时 `first_credential("feishu")` 取回。**Debug redacting**（参照 `wecom/proto.rs:23-30`，secret 不落日志）。
- **bootstrap**：新增 `feishu login` 子命令（或交互式提示）输入 app_id/secret 写入 store；参照 ilink 的 login 流程。

---

## 13. 配置与 main 接线（改造点清单）

> 全部为配置 / main 接线，非 core 逻辑改动。core **零改动**（依赖倒置）。

1. **顶层 `Cargo.toml`**：
   - `[workspace] members` 加 `"crates/feishu"`（参照现有 members，`:7` 附近）。
   - `[workspace.dependencies]` 加 `openlark = { version = "0.20", default-features = false, features = ["auth", "communication", "websocket"] }`。
   - `[dependencies]` 加 `imagent-feishu = { path = "crates/feishu" }`。
2. **`crates/core/src/config.rs`**：加字段 `feishu_app_id: Option<String>`（`config.rs:94-103` 附近）、`feishu_base_url: Option<String>`；`Config::EXAMPLE`（`config.rs:155-170`）加注释行。**注意**：`app_secret` 不进 config，走 store。
3. **`src/main.rs`**：
   - `Cmd::Start { platform }` 与未知 platform 校验（`main.rs:117-146`）加 `"feishu"`。
   - `build_platform`（`main.rs:399-442`）加 `"feishu" =>` 分支：从 config 取 app_id + base_url，从 store 取 app_secret，缺则 `anyhow!` 报错；调 `FeishuPlatform::new(app_id, secret, base_url)`。
4. **`crates/feishu/Cargo.toml`**：照 `wecom/Cargo.toml`，依赖 `imagent-core` + workspace 依赖（serde / serde_json / tokio / async-trait / tracing / anyhow / thiserror）+ `openlark`（workspace）。

### 飞书后台前置配置（文档化，非代码）

- 创建企业自建应用，启用「机器人」能力。
- 权限 scope：`im:message`、`im:message:send_as_bot`、`im:message.group_at_msg`（群 @）、`im:message.p2p_msg`（私聊）。
- 事件订阅：选「使用长连接接收事件」，订阅 `im.message.receive_v1`。

---

## 14. 验收计划（cargo test）

照 wecom 测试风格（`wecom/proto.rs` / `platform.rs` / `client.rs` 各自 `#[cfg(test)]`）：

- **proto.rs**：§6.3 全覆盖（p2p / group / 非文本 / 空文本 / 非法 JSON / conv roundtrip / extract_text）。
- **platform.rs**：drain 去重（同 event_id 第二次丢弃）、drain 解析入队（照 `wecom/platform.rs:147-210`）；`send_text` 构造（mock / 断言调用，不连真机）。
- **client.rs**：`backoff_caps_at_30s`、`constants_sane`、`run` 持续重连（timeout 断言）。
- **整体**：`cargo test --workspace` 全绿；`cargo clippy --workspace -- -D warnings` 无 warning。

> 真机收发需真凭据，不进默认 `cargo test`（同 wecom）。

---

## 15. 借鉴 lark-coding-agent-bridge 的增量（P2，非 MVP）

`zarazhangrui/lark-coding-agent-bridge`（TypeScript）虽不能移植代码，交互形态值得借鉴：

1. **流式卡片回复**（最大价值）：imagent 当前 `reply` 是多次 `send_text` 纯文本；飞书可用 **interactive 卡片 + patch 更新**，让「文本 + 工具调用」实时更新在同一张卡片上。需要 core 的 `AgentChunk` 流式能力 + 新增 `Platform::update_card`（或复用 send_media）。**这是飞书相比 wecm 的核心体验增量。**
2. **COT 过程消息**：过程摘要 + 最终答案拆两条。
3. **消息合并 / 排队**：imagent 已有 per-conv 串行锁（`dispatch.rs`）；可加「短时间连续消息合并」。
4. **权限模式映射**：full/workspace/read-only → Claude permission mode（`bypassPermissions` / `acceptEdits` / `plan`）。
5. **扫码 bootstrap**：lcab 的二维码绑定应用流程（飞书有对应能力）。

> 这些是 core 层改动，单独立项（P2），不阻塞飞书 MVP。

---

## 16. 工作量评估与里程碑

| 里程碑 | 范围 | 相对 wecom |
|---|---|---|
| **M0** crate 骨架 + proto + 测试 | 四文件、proto.rs serde + 全单测、conv 映射 | 同等 |
| **M1** 长连接收消息打通 | client.rs（`LarkWsClient::open` + 重连）、platform recv、接 core 跑通「飞书消息 → agent」 | 比 wecom 简单（收交 SDK） |
| **M2** 发消息打通 | `send_text_msg`（HTTP）、token 验证、分片、跑通「agent → 飞书」 | token 验证是主要不确定点 |
| **M3** 凭据 keyring + config/main 接线 | `feishu login`、store、build_platform | 同 ilink 范本 |
| **M4** 端到端 + 文档 | 真机联调、README、cargo test 全绿 | — |

整体判断：**MVP 比 wecom 略简单**（收消息交 SDK、token 交 SDK），主要不确定点是 §8/§9 的 open-lark 高层 Client token 注入方式——但 echo_bot 的低层写法是确定可行的兜底，不会卡住。

---

## 17. 决策记录与实现时验证项

### 已决策（review 时拍板）

1. **Lark 国际版**：**MVP 不覆盖**，只做飞书（`base_url = https://open.feishu.cn`）。国际版（`open.larksuite.com`）留 `feishu_base_url` 参数，后续按需启用。
2. **openlark 版本锁定**：`version = "0.20"`（Cargo `^0.20` 语义，允许 0.20.x patch，不跨 0.21）。
3. **群聊 `@bot` 过滤**：**依赖飞书后台事件订阅配置**，platform 层不显式过滤 mentions（后台 `im.message.receive_v1` 订阅 + `im:message.group_at_msg` scope 决定是否推送未 @ 的群消息）。

### 实现时验证项（不阻塞，有兜底）

1. **open-lark 高层 `Client` 发消息的精确调用链 + token 自动注入**：§8/§9，以 echo_bot 低层写法（手动 `RequestOption::tenant_access_token`）为确定可行兜底。
2. **飞书文本消息精确长度阈值**：§11，`FEISHU_TEXT_MAX` 实现时查官方文档，MVP 取保守值 28_000。

---

## 附：参考资料

- open-lark SDK：https://github.com/foxzool/openlark （crate `openlark` v0.20）
- open-lark 长连接示例：`examples/01_getting_started/websocket_echo_bot.rs`
- 飞书长连接官方文档：https://open.feishu.cn/document/event-subscription-guide/callback-subscription/step-1-choose-a-subscription-mode/configure-callback-request-address
- 飞书发消息 API：`POST /open-apis/im/v1/messages`
- 借鉴（TS，交互形态）：https://github.com/zarazhangrui/lark-coding-agent-bridge
