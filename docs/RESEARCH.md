# imagent — Research Archive

> P0 摸底 + 竞品调研的事实归档，实现时回查。设计落地见 `DESIGN.md`。
> 所有结论尽量基于**一手/官方来源**；营销内容（知乎/什么值得买/卖 API 中转站）已剔除。

## 1. iLink / ClawBot 协议

### 1.1 官方身份（硬证据，非逆向）
- `github.com/Tencent/openclaw-weixin`：GitHub API `owner.login=Tencent`、`owner.type=Organization`、`owner.id=18461506`。腾讯官方组织账号下公开仓库，活跃维护。
- npm `@tencent-weixin/openclaw-weixin`：所有版本由 `pumpkinxing@tencent.com`（腾讯员工邮箱）发布，**MIT**，已到 v2.4.6。
- 微信开放文档官方页面：[ClawBot 接口](https://developers.weixin.qq.com/doc/aispeech/knowledge/openapi/Clawbotrelated.html)。
- 2026-03-22 腾讯官方宣布；IT之家/新浪科技（腾讯张军回应）报道。

**结论**：iLink（智联）是腾讯**主动对外开放**的协议，不是抓包逆向。封号风险**显著低于** itchat / iPad 协议 / PC-hook。`hermes-agent` 只是众多第三方消费者之一。

> 证伪一个流传说法：有人称"ClawBot 是开源社区项目，与腾讯无关"。这是把"OpenClaw 框架（社区）"和"`@tencent-weixin/openclaw-weixin` 插件（腾讯官方发布）"混为一谈。后者是官方的。

### 1.2 端点（base `https://ilinkai.weixin.qq.com`）
| 用途 | 端点 |
|---|---|
| 取登录二维码 | `POST /ilink/bot/get_bot_qrcode` |
| 轮询扫码状态 | `POST /ilink/bot/get_qrcode_status` → `wait/scaned/confirmed/expired`；`confirmed` 返回 `bot_token`+`ilink_bot_id`+`ilink_user_id`+`baseurl` |
| 收消息（长轮询） | `POST /ilink/bot/getupdates`（~35–40s，游标 `get_updates_buf`，响应 `msgs[]`） |
| 发消息 | `POST /ilink/bot/sendmessage`（必须回传 `context_token`） |
| typing/config/媒体 | `sendtyping` / `getconfig` / `getuploadurl` + CDN `novac2c.cdn.weixin.qq.com/c2c` |

请求头：`AuthorizationType: ilink_bot_token` + `Authorization: Bearer <bot_token>` + 每请求随机 `X-WECHAT-UIN`（base64 随机 uint32，防重放）。

### 1.3 协议三要素
1. 长轮询 `getupdates` 驱动入站；2. 出站必须 echo 最新 `context_token`；3. 媒体走 AES-128-ECB + PKCS7 加密的 CDN。

### 1.4 能力边界（硬限制，必须接受）
- 扫码产生**独立 bot 身份**（`xxx@im.bot`），**不是**用户自己的微信号。
- **私聊可靠**；**普通微信群基本进不去 / 收不到 @**。群 Bot 做不了。
- 任何人都能加 bot 好友 → **网关必须做发送者白名单**。

## 2. 合规与开源姿势

### 2.1 关键条款
- 《腾讯微信软件许可及服务协议》8.2.1.4 / 8.2.1.6 / 8.2.1.7：禁止"非腾讯授权第三方接入"、自动化操作、绕过技术保护。**用官方插件路径落入授权范围，不触发 8.2.1.6**；iPad/PC-hook 全部命中，是真正高风险区。
- 《微信ClawBot功能使用条款》：**4.6** 不得绕过技术保护；**4.7** 腾讯有单方裁量权限流/拦截；**6.4** 可终止连接/暂停微信服务（封号是明示手段）；**7.2** 可随时终止整个 ClawBot 功能（平台存亡风险）。

### 2.2 风险评估
- 封号：用 iLink 风险低于逆向方案，但**非零**（4.7/6.4），高频/违规触发。
- 平台存亡：7.2 允许腾讯随时关 ClawBot——**不能作为核心业务依赖**，靠"平台/后端解耦"对冲。
- 灰色地带：独立复刻协议无明确授权文书（官方包 README "developers integrating with their own backend need to implement the following interfaces" 可解读为允许，但不明确）。

### 2.3 开源正确姿势
- 定位为「**OpenClaw Weixin channel 协议的 Rust 实现**」，README 引用官方 npm 包/文档为协议出处，**避免"逆向/破解"字眼**。
- License：MIT 或 Apache-2.0（与上游一致）。
- **强制免责声明**：非腾讯官方/附属；使用者自负合规责任。
- 不打包/分发腾讯密钥或专有二进制，纯协议重实现。
- **绝不实现**绕过频率/风控的功能（4.6 红线）。
- 限制 DM 场景，不宣称群聊（实测不支持）。
- AES-128-ECB 是协议强制，README 标注（非设计选择）。
- 建议**小号**绑定，避免主号风险。

## 3. Claude Code 接入

### 3.1 CLI（MVP，feiyun 已验证）
```bash
claude -p "<prompt>" --output-format stream-json --verbose \
  [--model M] [--append-system-prompt S] [--allowedTools "Read,Edit"] [--resume <session_id>]
```
- print mode **默认持久化 session**（`--no-session-persistence` 才不存），故可 `--resume`。
- **关键**：不传 `--session-id` 自造 UUID，而是**从 stream-json 的 `result` 事件捕获 Claude 分配的 `session_id`**，下次 `--resume`。更稳。

### 3.2 stream-json schema
每行一个 JSON。`result` 事件：`{ "type":"result", "result":<文本>, "is_error":<bool>, "session_id":<id> }`。中间还有 `assistant` 文本 / `tool_use` / `tool_result` 事件（MVP 可忽略，P2 可推流）。

### 3.3 ACP（P2）
`claude-agent-acp` 长驻进程，JSON-RPC 2.0 over stdin/stdout：`session/new`→`sessionId`，`session/prompt{sessionId,prompt}`→结果+`SessionUpdate`通知。复用进程与 session，比每消息 spawn 快。P2 前确认入口与协议。

### 3.4 IM 内权限审批（差异化）
`--permission-prompt-tool <mcp>`：Claude 遇需权限的工具时回调该 MCP 工具。imagent 实现它→转 IM 消息让用户 approve/deny→回传。feiyun 无此能力。

## 4. 竞品：feiyun0112/AgentBridge（.NET，28★）

**同一件事**：微信 iLink ↔ 多 agent（Claude/Codex/Gemini/Cursor/Copilot…），已实现核心。

### 4.1 可借鉴（feiyun 验证可行）
- 三层分离（CLI/Core/Weixin）+ `IAgent` 抽象（`ChatAsync(conversationId,message)`）。
- Claude CLI：捕获 `result` 的 `session_id` + `--resume`。
- iLink 协议端点/认证/context_token（与 hermes 一致）。
- daemon + HTTP 管理 API；`AgentDetector` 自动检测已装 agent；`/cc <name>` 切默认 agent。
- ACP 双模式（`claude-agent-acp` + JSON-RPC）。

### 4.2 差异化靶点（feiyun 源码证实的缺口）
| feiyun 缺口 | 源码证据 | imagent 对策 |
|---|---|---|
| 无发送者白名单 | `MessageHandler` 无鉴权 | core 入口白名单（安全必须） |
| session 只内存 | `ConcurrentDictionary`，重启丢 | SQLite 持久化 |
| 无会话命令 | 只有 `/cc` | `/new` `/switch` `/sessions` `/compact` |
| 无权限审批 | 无 `--permission-prompt-tool` | IM 内审批（杀手级） |
| 只 weixin | `--provider weixin` 唯一 | iLink + WeCom |
| iLink 无限流 | client 层无 retry/backoff | 可选熔断 |
| .NET 运行时 | 需运行时 | Rust 单二进制 |

> 另两个同名项目无关：`catatafishen/agentbridge`（61★，JetBrains IDE 插件，agent↔IDE）、`iflytek/AgentBridge`（24★，工作流 DSL 转换）。

### 4.3 参考实现
- hermes：[`gateway/platforms/weixin.py`](https://github.com/NousResearch/hermes-agent/blob/main/gateway/platforms/weixin.py)（2359 行，MIT，iLink 完整实现，含 SSL/限流/去重细节）。
- feiyun：`src/AgentBridge.Weixin/ILink/*`、`src/AgentBridge.Core/Agent/{AcpAgent,CliAgent}.cs`。

## 5. 命名

选定 **`imagent`**（IM + agent 融合，蹭 agent 热度，crates.io/npm/GitHub 均干净；GitHub 同名仅 8★）。淘汰：`agentbridge`（61★+科大讯飞撞名）、`chatagent`（多个同名稀释）、`imbridge`（最干净但不点 agent，埋了核心卖点）。`imagent` 有 `image` 视觉联想的小代价，靠 slogan/首屏锁定认知即可。

## 6. 一手来源

- 腾讯官方仓库：https://github.com/Tencent/openclaw-weixin
- 官方 npm 包：https://www.npmjs.com/package/@tencent-weixin/openclaw-weixin
- 微信开放文档 ClawBot：https://developers.weixin.qq.com/doc/aispeech/knowledge/openapi/Clawbotrelated.html
- ClawBot 使用条款：https://github.com/hao-ji-xing/openclaw-weixin/blob/main/protocol.md
- 微信软件许可协议：https://weixin.qq.com/agreement?lang=zh_CN
- Claude Code CLI 参考：https://code.claude.com/docs/en/cli-reference
- hermes weixin.py：https://github.com/NousResearch/hermes-agent/blob/main/gateway/platforms/weixin.py
- 企业微信智能机器人 API：https://developer.work.weixin.qq.com/document/path/101463
