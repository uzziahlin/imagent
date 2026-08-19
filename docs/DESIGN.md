# imagent — Detailed Design

> 实现阶段的主要依据。重开会话写代码前必读。调研背景见 `RESEARCH.md`。

> ⚠️ **文档状态（2026-07）**：本文档是 P1 阶段的初始设计快照，**保留作历史参考**。
> 当前代码的真实结构见 [ARCHITECTURE.md](./ARCHITECTURE.md)（随迭代维护）。代码已实现至
> P3 之后（三平台 ilink/wecom/feishu、四后端、IM 权限审批闭环、keyring 凭据、流式卡片、
> 批处理、孤儿卡片关流等）。其中：`Backend::run` 实际签名已增加 `conv_id`
> 参数（见 `crates/core/src/backend.rs`）。具体行为以代码 + `CHANGELOG.md` +
> `README.md` 为准。

## 1. 目标与非目标

**目标**
- 常驻网关：监听 IM 私聊 → 鉴权 → 驱动 agent 执行真实任务 → 流式回传结果。
- **平台抽象**（`trait Platform`）：iLink（个人微信私聊）+ WeCom（企业微信官方 API）。
- **后端抽象**（`trait Backend`）：Claude Code（CLI 优先，ACP 留 P2），可换。
- **会话连续**：per-chat `conversationId → agent sessionId`，SQLite 持久化，重启可续；`/new` `/switch` `/sessions` `/compact`。
- **安全**：发送者白名单、`--allowedTools` 收敛、workdir 锁定、**IM 内权限审批**（P2）。
- 单二进制、低占用，适合常驻。

**非目标**
- 不"控制用户自己的微信号收发所有消息"——iLink 只给独立 bot 身份，私聊可靠、群不可用（见 RESEARCH.md）。
- 不做多账号轮换/反风控（ClawBot 条款 4.6 红线）。

## 2. crate 结构

```
imagent/
├── Cargo.toml          # workspace（实现时把 members 取消注释）
├── crates/
│   ├── core/           # 调度核心：trait Platform/Backend、鉴权、会话路由、消息流
│   ├── ilink/          # impl Platform：iLink 协议客户端（个人微信）
│   ├── wecom/          # impl Platform：企业微信官方 API（P3）
│   ├── claude/         # impl Backend：Claude Code（CLI + ACP）
│   └── store/          # SQLite：凭据、会话映射、游标、配置
└── src/main.rs         # 组装：加载配置、起 tokio、信号、daemon
```

依赖方向：`main` → `core` → `{Platform, Backend, store}`；`ilink`/`wecom`/`claude` 实现 core 的 trait，只依赖 core + store。

## 3. 核心抽象（trait 签名）

```rust
// crates/core/src/types.rs
pub struct ConvId(pub String);     // 形如 "ilink:<from_user_id>"、wecom:<user>
pub struct UserId(pub String);
pub struct SessionId(pub String);  // agent 分配的会话 id（Claude 的 session_id）
pub struct Workdir(pub PathBuf);

pub struct InboundMessage {
    pub conv_id: ConvId,
    pub sender: UserId,
    pub text: Option<String>,
    pub media: Vec<MediaRef>,
    pub reply_hint: ReplyHint,     // 平台回传所需（如 iLink 的 context_token）
}

// crates/core/src/platform.rs
#[async_trait]
pub trait Platform: Send + Sync {
    /// 阻塞取下一条入站消息（内部自管长轮询/重连）。
    async fn recv(&self) -> Result<InboundMessage>;
    async fn send_text(&self, conv: &ConvId, text: &str, hint: &ReplyHint) -> Result<()>;
    async fn send_media(&self, conv: &ConvId, media: &MediaRef, hint: &ReplyHint) -> Result<()>;
    async fn send_typing(&self, conv: &ConvId, hint: &ReplyHint) -> Result<()>; // 可选
    fn name(&self) -> &'static str;
}

// crates/core/src/backend.rs
/// agent 输出的分块（流式推给 core，core 再发 IM）
pub enum AgentChunk {
    Text(String),
    ToolUse { tool: String, input: String },
    ToolResult { tool: String, output: String },
    Final(String),
    Error(String),
}

pub struct RunOutcome { pub session_id: SessionId, pub final_text: String }

/// Backend 是**无状态执行器**：core 传入 sessionId（续接）或 None（新建），它执行并流式产出。
#[async_trait]
pub trait Backend: Send + Sync {
    async fn run(
        &self,
        conv_id: &str,                  // 当前会话标识（IM 权限审批路由用）
        prompt: &str,
        session: Option<&SessionId>,   // None=新建
        workdir: &Path,
        allowed_tools: &[String],       // 如 ["Read","Edit"]
        chunks: mpsc::Sender<AgentChunk>,
    ) -> Result<RunOutcome>;
    fn name(&self) -> &'static str;
}
```

> **设计要点（为什么这么分）**：feiyun 把 `conversationId→sessionId` 映射塞在 Backend 内（内存）。imagent 把 **session 生命周期提到 core**（store 持久化），Backend 退化为无状态执行器。这样 core 能实现 `/new`（清映射）/`/switch`/`/compact`，且重启续接——比 feiyun 更干净的职责分离。

## 4. 数据流与调度

```
Platform::recv() ──InboundMessage──→ core::dispatch
                                        │
                          ┌─────────────┼──────────────┐
                          ▼             ▼              ▼
                   鉴权白名单?      斜杠命令?      普通消息
                   (非白名单丢弃)   (/new /switch…)  │
                                                     ▼
                                  store.get_session(conv_id) → Option<SessionId>
                                                     │
                                  Backend::run(prompt, session, workdir, tools, chunks)
                                                     │  (tokio::spawn, 不阻塞 recv)
                                  ◄────────── AgentChunk 流 ──────────◄
                                                     │
                                  Platform::send_text/media(conv, chunk, hint)
                                                     │
                                  store.upsert_session(conv_id, outcome.session_id)
```

- 收消息（长轮询）与 agent 执行**解耦**：agent 任务 `tokio::spawn`，不阻塞后续消息；并发任务按 conv_id 串行（同一会话排队，避免 session 冲突）。
- `chunks` 流：MVP 可只发 `Final`；P2 可把 `ToolUse`/`ToolResult` 也推 IM（差异化体验，注意 IM 长度限制需聚合/截断）。

## 5. session 机制（core + store）

```sql
CREATE TABLE sessions (
  conv_id      TEXT PRIMARY KEY,     -- "ilink:<from_user_id>"
  session_id   TEXT NOT NULL,        -- Claude 分配的 session_id
  agent_kind   TEXT NOT NULL,        -- "claude-cli" / "claude-acp"
  workdir      TEXT NOT NULL,
  name         TEXT,                 -- 命名会话（/switch 用）
  created_at   INTEGER, updated_at   INTEGER
);
```

- **新建**：`session=None` → Backend 创建 → 返回 `outcome.session_id` → store 写入。
- **续接**：store 命中 → `session=Some(id)` → Backend 用它（CLI `--resume`）→ 返回（一般不变）→ 更新 `updated_at`。
- `/new`：删除该 conv 的行（下次新建）。`/switch <name>`：按 name 切到另一 session（同 conv 多任务并行上下文）。`/sessions`：列出该 conv 的历史。`/compact`：对当前 session 触发上下文压缩。

## 6. iLink adapter（`crates/ilink`）

协议细节见 `RESEARCH.md`，此处是实现要点。

**端点**（base `https://ilinkai.weixin.qq.com`）：
| 用途 | 端点 |
|---|---|
| 取登录二维码 | `POST /ilink/bot/get_bot_qrcode` |
| 轮询扫码状态→拿凭据 | `POST /ilink/bot/get_qrcode_status`（状态 wait/scaned/confirmed/expired；confirmed 返回 `bot_token`+`ilink_bot_id`+`ilink_user_id`+`baseurl`） |
| 收消息（长轮询） | `POST /ilink/bot/getupdates`（timeout ~35–40s，游标 `get_updates_buf`，响应 `msgs[]`） |
| 发消息 | `POST /ilink/bot/sendmessage`（**必须回传最新 `context_token`**） |
| typing / config / 媒体 | `sendtyping` / `getconfig` / `getuploadurl` + CDN `novac2c.cdn.weixin.qq.com/c2c` |

**请求头**：`AuthorizationType: ilink_bot_token` + `Authorization: Bearer <bot_token>` + 每请求随机 `X-WECHAT-UIN`（base64 随机 uint32，防重放）。参考 hermes `_headers` / feiyun `PostAsync`。

**关键状态**：
- `context_token`：每个 peer 存最新，发消息回传（关联对话）。存 store `context_tokens` 表。
- `get_updates_buf`：长轮询游标，存 store `sync_buf` 表，重启续接不丢消息/不重复。

**媒体**：`getuploadurl` 预签名 → 上传 CDN；下载用返回的 `encrypted_query_param`。**AES-128-ECB + PKCS7**（协议强制）。Rust：`aes`+`ecb`+`cipher` crate。MVP 可先不做媒体（P2）。

**两个真实坑（P1 实测/处理）**：
1. **SSL**：hermes（Python）需特殊 SSL connector；feiyun（.NET）默认 HttpClient 即可。imagent 用 `reqwest`+`rustls-tls`，**P1 实测** `getupdates` 是否握手成功；若失败，对该域名自定义 root certs 或（仅此域名、记录原因）关闭验证。倾向 .NET 结论：rustls 默认能过。
2. **发送限频**：微信对 `sendmessage` 限频。发消息层带指数退避 + 熔断（参考 hermes rate-limit circuit）。MVP 可先最简重试，P2 补熔断。

**鉴权**：adapter 透传 `from_user_id` 给 core；core 用配置的 `ALLOWED_SENDERS` 白名单过滤，**非白名单直接丢弃并记日志**（feiyun 没做这步，是安全缺口）。

## 7. Claude Backend（`crates/claude`）

### 7.1 CLI 模式（MVP，已由 feiyun 验证可行）

```bash
claude -p "<prompt>" \
  --output-format stream-json --verbose \
  --model <model?> --append-system-prompt "<sys?>" \
  --allowedTools "Read,Edit" \
  [--resume <session_id>]      # 续接时才带
# cwd = workdir（Command::current_dir 锁定）
```

**逐行解析 stdout（stream-json，每行一个 JSON）**：
- 捕获 `session_id`（`result` 事件带，存为本次 session）。
- `type == "result"`：`result`(文本) + `is_error`。取作 `Final`。
- 中间事件（`assistant` 文本 / `tool_use` / `tool_result`）：MVP 忽略；P2 转 `AgentChunk` 推 IM。
- **不传 `--session-id` 自造 UUID**——而是**捕获 Claude 分配的 session_id**，下次 `--resume`。比自造 UUID 稳（避免 `--session-id` 对已存在 session 的语义不确定性）。

错误：`is_error=true` 或非零退出但无 result → `AgentChunk::Error`。

### 7.2 ACP 模式（P2）

`claude-agent-acp` 长驻进程，JSON-RPC 2.0 over stdin/stdout：
- `session/new` → `sessionId`
- `session/prompt { sessionId, prompt }` → 结果 + `SessionUpdate` 通知流
- 复用进程与 session，比 CLI（每消息 spawn）快。Backend 同 trait，内部换实现。
- P2 前先确认 `claude-agent-acp` 入口与协议（参考 feiyun `AcpAgent.cs`）。

### 7.3 IM 内权限审批（P2，杀手级差异化）

`claude -p ... --permission-prompt-tool <mcp_tool>`：Claude 遇到需权限的工具调用时，回调该 MCP 工具。imagent 实现这个 MCP 工具：把权限请求转成 IM 消息发给用户 → 用户回复 approve/deny → 回传给 Claude。实现"危险命令在微信里批准"。feiyun 无此能力。

## 8. WeCom adapter（`crates/wecom`，P3）

企业微信官方 API（合规主推）。候选：智能机器人长连接（[path/101463](https://developer.work.weixin.qq.com/document/path/101463)）/ 应用消息。**不能直发个人微信**，是 iLink 的合规补充渠道，非替代。`impl Platform`。

## 9. 安全设计（硬约束）

1. **发送者白名单**：每平台 `ALLOWED_SENDERS`，core 入口过滤。iLink bot 任何人可加好友，**这步不可省**。
2. **`--allowedTools` 收敛**：配置驱动，起步仅 `Read,Edit`；稳了再放 `Bash`。workdir 用 `current_dir` 锁定到指定项目。
3. **IM 内权限审批**（P2）：危险操作人工放行。
4. **凭据加密落盘**：`bot_token` 等用 OS keyring 或加密文件，不明文存配置。
5. **不实现**多账号轮换/反风控（合规红线）。

## 10. store（`crates/store`，SQLite via rusqlite）

```sql
credentials(platform, account_id, blob_encrypted, updated_at)
sessions(conv_id PK, session_id, agent_kind, workdir, name, created_at, updated_at)
sync_buf(platform, account_id, buf)                 -- iLink 长轮询游标
context_tokens(platform, account_id, peer, token)   -- iLink 出站回传
config(key PK, value)
```

## 11. 配置 / CLI（`clap`）

```
imagent login   --platform ilink          # 扫码登录，存凭据
imagent start   [--platform ilink]        # 常驻（默认后台 daemon）
imagent stop | status
```
配置文件 `~/.imagent/config.toml`：平台凭据引用、`allowed_senders`、默认 workdir、`allowed_tools`、agent 选择。

## 12. 错误处理 / 可观测

- `thiserror`（库错误）+ `anyhow`（main）。`tracing` 结构化日志，`RUST_LOG` 控制。
- 长轮询失败：指数退避 + 重连；session 过期（iLink SESSION_EXPIRED）→ 暂停 + 提示重新登录。
- 去重：入站消息 5 分钟滑动窗口去重（参考 hermes `MessageDeduplicator`）。

## 13. 路线（P0–P3）

| 阶段 | 交付 |
|---|---|
| **P0** ✅ | 调研：iLink 协议/合规、Claude CLI/ACP、竞品 feiyun、命名（见 RESEARCH.md） |
| **P1 MVP** | core + ilink（登录/收发文本）+ claude（CLI，捕获 session_id+`--resume`）+ store（sessions/sync_buf/context_tokens/credentials）+ 鉴权白名单。闭环：扫码→私聊→`claude -p --allowedTools Read,Edit`→文本回传。 |
| **P2** | 会话命令（/new /switch /sessions /compact）、IM 权限审批闭环、媒体收发、ACP backend、限流熔断、typing、中间工具事件推流 |
| **P3** | WeCom adapter、可观测/指标、打包发布、开源化（CI、安全审计、文档） |

## 14. 待实测/确认（P1 起逐项验证）

- `reqwest`+`rustls` 对 `ilinkai.weixin.qq.com` 默认 TLS 是否握手成功。
- stream-json 中间事件（`tool_use`/`tool_result`）确切字段。
- `claude --resume <id>` 续接行为（feiyun 已验证可行，复验）。
- `claude-agent-acp` 是否存在、调用方式（ACP P2 前确认）。
- WeCom 具体用哪条 API（P3 前定）。

## 15. 依赖

见根 `Cargo.toml` `[workspace.dependencies]`：tokio / reqwest(rustls) / serde / aes+ecb+cipher / rusqlite / uuid / qrcode / clap / tracing / thiserror+anyhow。
