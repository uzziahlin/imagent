# Web 平台设计（ECS 中继 + 浏览器驾驶舱）

> 状态：设计稿 v1（MVP 范围）。姊妹文档：[ARCHITECTURE.md](ARCHITECTURE.md)、[FEISHU_DESIGN.md](FEISHU_DESIGN.md)。
> 本文回答三个问题：① 为什么用 ECS 中继而不是直连/内网穿透；② 安全世界面怎么划；③ MVP 切多大、怎么落地。

## 1. 背景与目标

imagent 现有平台（ilink / wecom / feishu）都是 IM 通道，渲染被 IM 卡片格式约束（文本 + 按钮卡片）。本设计为 imagent 增加**第四个平台：Web**——浏览器里获得 IM 渠道的全部能力（审批闭环、会话、steering、队列、cron），外加 IM 给不了的富渲染（流式 Markdown、代码高亮、diff、工具卡片），形态对标 ZCode 的会话视图。

### 目标

- 手机/任意浏览器（含 PWA「添加到主屏幕」）远程驾驶 Mac 上的 agent：发消息、批权限、看流式输出、`/stop`、切会话
- Mac **零入站端口**：网关只发起出站连接
- 单用户自用，零注册零第三方依赖；部署到已有 ECS（复用现有 Caddy + 证书 + 安全组）

### 非目标（明确不做）

- 多用户 / 注册体系
- 端到端加密（MVP 为 TLS-only：自家服务器 + 全程 HTTPS，中继可见明文可接受；协议留演进位，见 D6）
- 替代 IM 通道（Web 与 IM 并存互补：Web 富渲染做驾驶舱，IM 做轻通知兜底）

## 2. 总体架构

```
浏览器（手机/桌面 PWA）
   │  WSS + HTTPS   https://relay.example.com:8443/imagent/*
   ▼
ECS 8443 Caddy（已有，TLS 终结）
   │  handle_path /imagent/* → 127.0.0.1:3010
   ▼
crates/relay（新增，Rust，~400 行）：无状态帧中继 + 双向有界缓冲
   ▲  WSS 出站（网关侧发起，Mac 零入站）
   │  wss://…/imagent/gateway + GATEWAY_TOKEN
Mac：imagent gateway
   └─ crates/web：impl Platform（recv ← WS 入站帧；send_text/send_card/update_card → WS 出站帧）
        └─ Dispatcher 全管线（审批 / 会话 / 队列 / steering / cron / delegate 全部继承）
```

要点：**两条连接都是出站/客户端模式**。Mac 上没有任何监听端口（`ss -tln` 可验证）；浏览器只认识 ECS 的公网地址。这与 Happy 的「CLI 出站连 relay、手机连 relay」同构，是本项目已验证可跑通大陆网络的模式。

### D1 为什么选 ECS 中继（决策记录）

| 备选 | 结论 | 理由 |
|---|---|---|
| **ECS 中继（选定）** | ✅ | 任意浏览器零客户端直用；Mac 零暴露；深圳节点延迟 10-30ms（实测链路已存在）；复用现有 Caddy/证书/安全组，零新增暴露面 |
| Tailscale 私网 | 备选保留 | 无公网中继、隐私最优；但每台设备装客户端、依赖家里网络可达、iOS 上 PWA 与 VPN 共存体验一般。适合作为「仅在私网使用」的降级形态，与中继不互斥 |
| 家宽端口转发 / DDNS | ❌ | 暴露家庭网络、运营商动态 IP/ NAT 不稳定、安全责任面大 |
| 复用 happy-server 中继 | ❌ | 协议语义不同（imagent 的 Platform 帧 ≠ happy 会话协议），硬塞会把两套系统耦合进对方的演进节奏 |

### D2 中继为独立小服务（`crates/relay`）

- 与 imagent 同仓同语言（axum 0.8 + tokio-tungstenite），单二进制，`rust-embed` 内嵌 SPA 静态资源 → ECS 上就是一个 systemd 服务
- 中继**不理解业务**：只做帧转发、认证、缓冲、限流。业务语义（会话、审批状态机）全部留在 gateway 侧的 core 管线——中继保持「笨」，两侧协议演进不受彼此牵制
- 绑定 `127.0.0.1:3010`，公网入口只有 Caddy

### D3 网关侧零监听

`crates/web` 不起任何本地服务：`Platform::recv()` 的实现是「WS 客户端任务 → mpsc channel」，`send_text/send_card/update_card` 是「出站帧队列」。断线重连、指数退避、心跳（30s ping/pong）全部封装在 WS 客户端任务内，对 Dispatcher 透明。

### D4 帧协议（JSON，一帧一 WS message）

信封：`{"v":1, "type": …}`。MVP 帧集：

**browser → gateway**（经 relay 转发）：

| type | 载荷 | 对接 |
|---|---|---|
| `message` | `{convId?, text}` | → `Platform::recv()` 的 `InboundMessage`（复用现有鉴权/审批/排队管线） |
| `approval_response` | `{convId, requestId, decision: allow\|deny\|always}` | → `PermissionRouter::route()`（等价于 IM 按钮点击 / y-n）。**convId 必填**——router 按 conv+request_id 路由，多平台并存时不可缺省（2026-10-28 评审修订） |
| `control` | `{cmd: stop\|resume\|switch, args}` | → 现有 `/stop` `/resume` `/switch` 同语义入口 |
| `ping` | — | 心跳 |

**gateway → browser**：

| type | 载荷 | 来源 |
|---|---|---|
| `card` / `card_patch` | `{convId, card, state}` | `send_card` / `update_card`（card_session 节流语义不变） |
| `text` | `{convId, text, hint}` | `send_text` 降级路径 |
| `approval_request` | `{convId, requestId, tool, inputSummary, waitedSecs}` | 审批 pending 时推送（UI 弹按钮） |
| `status` | `{convId, state: running\|done\|error\|awaiting_approval}` | 轮次状态 |
| `machine` | `{online, version, sandboxProfile}` | 网关能力/状态卡 |
| `pong` / `error` | — | 心跳应答 / 错误码 |

协议演进位：`v` 字段 + `type` 枚举留扩展；phase2 若做 E2E，仅把 `body` 字段换成密文信封（NaCl box，浏览器与网关互持密钥），中继与帧路由完全不用改。

### D5 认证（双 token，无 cookie → 无 CSRF 面）

| 主体 | 凭证 | 存放 |
|---|---|---|
| gateway → relay | `GATEWAY_TOKEN`（`openssl rand -hex 32`） | ECS `/opt/imagent-relay/env`（0600）+ Mac 侧 config |
| browser → relay | `USER_TOKEN`（同强度） | 浏览器首次访问输入，存 localStorage；PWA 同理 |

- 校验用 constant-time 比较（`subtle` crate）；token 走 WS 首帧或 `Authorization` 头，**不进 URL**
- 轮换 = 改 env 重启 relay（秒级）；两端同步更新
- MVP 不做密码/JWT（单人场景静态 token 足够）；phase2 可换「口令 → 短时效 JWT + refresh」

### D6 状态与缓冲（故障语义）

| 场景 | 行为 |
|---|---|
| gateway 离线，browser 在线 | relay 缓冲 browser 帧（上限 200 帧 / 1MB，FIFO），并向 browser 持续发 `{type:"machine", online:false}`；溢出回 `error: gateway_offline`。恢复后按序重放 |
| browser 离线，gateway 在线 | gateway 的出站帧在 relay 侧丢弃（不缓存）——**会话历史不依赖中继**：重连后 UI 通过「拉取末页」恢复上下文（MVP 简化为 gateway 侧内存保留最近 N 条 + 网页刷新时 gateway 全量重发当前卡片终态） |
| relay 重启 | 双端 socket.io 风格自动重连（指数退避 1s→30s），无持久化状态可丢 |

为什么 browser 侧不缓存：MVP 单用户单会话，丢帧窗口 = 断线期间；agent 侧执行状态由 imagent 既有机制兜底（inflight 标记 + `/retry`），中继缓存反而引入重复投递问题。

## 3. 组件设计

### 3.1 `crates/relay`（ECS 侧）

- `GET /imagent/gateway`（Upgrade）：gateway WS；首帧 `hello` 校验 `GATEWAY_TOKEN`（失败 401 关闭 + 1s 延迟防爆破）
- `GET /imagent/ws`（Upgrade）：browser WS；同上校验 `USER_TOKEN`
- `GET /imagent/`（及静态路径）：`rust-embed` 吐 SPA（`Cache-Control: no-store` 对 index，hash 资源长缓存）
- 中继逻辑：两端各一条 mpsc；按帧类型定向转发；限流（每连接 20 帧/10s，超限 429 关闭）、单帧上限 256KB、连接数上限（每 token 1 条，重复连接踢旧）
- 加固：仅绑 127.0.0.1；systemd `ProtectSystem=strict` + `PrivateTmp` + `NoNewPrivileges`；日志只记连接/断开/字节数，**不落帧内容**
- 部署：`cargo build --release`（Mac 交叉 `--target x86_64-unknown-linux-gnu` 或 ECS 本地构建）→ workbench 上传 → systemd；Caddy 现有 8443 site 增加 `handle_path /imagent/*` 反代 3010（不动 happy 路由）

### 3.2 `crates/web`（gateway 侧 Platform 实现）

- `WebPlatform::new(config)`：持有 WS 客户端任务句柄；`recv()` = channel 前端
- 帧映射：`message` → `InboundMessage`（convId 为空则落当前默认命名会话）；`approval_response` → 注入 `PermissionRouter`；`control` → 复用现有命令入口（与 `/stop` 等价路径，不走文本解析）
- 出站：`send_text`/`send_card`/`update_card` → 帧（`capabilities()` 声明 `CARDS | MEDIA`——MEDIA 的文件传输留 phase2，先声明不实现会与「覆写须置位」纪律冲突，故 MVP 只声明 `CARDS`）
- 重连与保活：30s ping；断线时 `recv()` 保持 pending（Dispatcher 天然阻塞等待，与 IM 平台断线行为一致）
- 配置：`config.toml` 新增 `[[platform.web]]`：`relay_url`、`gateway_token`、`user_token`、`default_session`

### 3.3 `web-ui/`（SPA，Vite + React + Tailwind）

- 页面：登录页（输 USER_TOKEN 一次）→ 会话页（消息流 / 流式卡片 / 审批弹层 / composer / 状态条）
- 渲染：react-markdown + 代码高亮（**必须过 DOMPurify**，防消息内容 XSS 偷 token）；工具调用渲染成可折叠卡片（MVP 只展示 name + 摘要，diff 视图 phase2）
- PWA：`manifest.json` + 「添加到主屏幕」（service worker 与离线缓存 phase2）
- 为什么直接上 Vite+React 而不是无构建单页：富渲染（diff/工具卡片）是本项目的核心增量，值得从 phase1 就用可演进的工程结构

## 4. 安全设计

### 威胁模型与缓解

| 威胁 | 面 | 缓解 |
|---|---|---|
| 公网扫描 / 未授权访问 WS | relay | 双 token + constant-time 比较 + 失败延迟；每 token 单连接、踢旧 |
| token 爆破 | relay | 32 字节随机（熵足够，爆破不可行）；失败限速 |
| DoS / 滥用 | relay | 帧率限制、帧大小上限、缓冲上限、连接数上限；Caddy 层再叠一层限速；异常可临时 SG 收紧 |
| 消息内容窃听 | 链路 | 全程 TLS（Caddy LE 证书）；中继可见明文 = 已知取舍（自家服务器），phase2 可升级 E2E |
| 中继被攻破 | relay | 同上；systemd 加固缩小二进制的可写面；ECS 本身是最小攻击面（仅 8443 + 22） |
| XSS → 偷 localStorage token | SPA | Markdown 渲染强制 sanitize；React 默认转义；CSP 头（`connect-src 'self'`） |
| CSRF | SPA | 无 cookie 认证（token 在 header/首帧），天然免疫 |
| Mac 侧暴露 | gateway | **零监听端口**（架构性消除）；出站仅 WSS 到自家 relay |
| agent 危险操作 | core | 审批闭环不变——Web 只是审批的另一个入口，且 MVP 审批按钮带过期语义（pending 超时沿用 core 现有规则） |

原则：**不新增信任假设**。所有缓解都在「已有 ECS + 已有 TLS + 新增两个随机 token」范围内，不引入第三方服务。

## 5. MVP 里程碑与验收

| 里程碑 | 内容 | 验收 |
|---|---|---|
| M1 relay ✅ | 双 WS 端点 + token + 帧中继 + 缓冲/限流 + 内嵌静态；部署 ECS + Caddy 路由（2026-10-28 完成 `e6d11ca`，Node 双端实测 + 未带 token close 1008） | ✅ |
| M2 gateway ✅ | `crates/web` Platform impl + WS 客户端 + 配置项（`eee5719`，26 测试含回环集成） | ✅ |
| **M2.5 MultiPlatform**（2026-10-28 评审新增，开工前已拍板） | 多平台组合器：自身实现 `Platform`——`recv()` select 合并子平台、`send_*` 按 conv 前缀路由（`feishu:` → feishu、`web:` → web）、capabilities 取交集；Dispatcher **零改动**。背景：现调度器为单 Platform（`Config.platform` 单值 → `build_platform` 单个 → Dispatcher 持一个），不补此件则选 web 即掉 feishu，与「IM 做轻通知兜底」自相矛盾。配置形态定为 `platforms = ["feishu", "web"]` 列表（与 `[[platform.web]]` 二选一，实现时定稿） | feishu + web 同时在线：web 驱动轮次、feishu 收审批加急/完成通知；单平台配置行为不变（回归） |
| M3a web-ui 基座 | 登录页 + 会话页（**文本流 + composer + 审批弹层**先行） | 手机 4G：网页发「列出目录」→ 审批按钮 → 批准 → 流式回显（M3 拆两步：渲染保真度是最大工作量风险，先闭环最短链路） |
| M3b web-ui 富渲染 | 流式卡片 / 工具折叠卡（对标 ZCode 渐进逼近；diff 视图仍 phase2） | 卡片流式节流与 IM 侧观感等价；长输出截断 + 补全文路径可用 |
| M4 端到端 | PWA 主屏 + 文档（book）+ 故障演练 + **`/resume` 接管本机会话**（2026-10-28 评审：从 §6 路线 4 提前——核心已存在，网页把 `/resume` 列表做成可点 UI 即「手机接管本地会话」的正解形态） | 断 Mac 网络→网页显示 offline→恢复自动重连；`ss -tln` 证明 Mac 无新增监听；终端 Ctrl-C 后网页 `/resume` 点选同一会话无缝续接 |

## 6. 迭代路线（MVP 之后）

1. **富渲染**（diff 视图、工具卡片细化）：core 事件模型加富负载通道（tool diff 事件），Web 平台独享——不动 IM 卡片格式
2. **通知**：审批请求双通道推送（Web Push 或复用已有 IM 通道发「去网页处理」提示）
3. **E2E 加密**：NaCl box（浏览器 ↔ 网关），中继盲化
4. **多会话 UI**：`/switch` 会话的网页列表化、`/resume` 接管电脑端 Claude Code 会话
5. **delegate UI**：网页 fan-out 子任务给 `[[delegate]]` 副后端、进度聚合视图
6. **PWA 完整化**：service worker、Web Push（iOS 需 PWA 安装）

## 7. 决策记录与遗留

- ✅ **会话模型（已定 2026-10-28）**：网页与 IM **共享同一个 `/switch` 命名会话池**。网页 composer 默认发往「当前活跃命名会话」，`/switch` `/resume` 命令与列表在 Web UI 可用；跨端接续（IM 聊一半、网页接着看）为一等场景。实现注意：Dispatcher 的 per-conv 会话绑定不变，Web 只是又一个 conv 来源
- ✅ **前置事项（已定 2026-10-28）**：delegate 批次 WIP 先收尾提交，再从干净基线开 `feat/web-platform`（已完成：v1.31 委托编排 + v1.32 OS 沙箱双批已提交 main——`3aea1d8` feat / `5546886` release，954 tests 全绿）
- ✅ **M2.5 MultiPlatform + 帧修订（已定 2026-10-28 评审）**：① 多平台并存经 MultiPlatform 组合器（Dispatcher 零改动）落地，M2 与 M3 之间执行；② `approval_response` 帧补必填 `convId`；③ `/resume` 接管本机会话提进 M4 验收、M3 拆 a/b。定位认知（防走偏）：Web 平台的增量是**驾驶舱体验 + 自有栈**，不是 takeover 实时性——「接管正在跑的本地会话」短期靠 Happy（已部署）+ `/resume` 手递手，终态是远期的本地前端（`imagent term`，同会话池双视图），均不进 MVP
- 审批超时：沿用 core 现有 pending 超时语义，Web 不新增（待实现时核对 waited_secs 审计字段是否透出）
- MEDIA_UPLOAD：MVP 不声明；文件传输（上传附件到 agent）放 phase2，届时走 relay 的二进制帧或独立 REST 上传口
