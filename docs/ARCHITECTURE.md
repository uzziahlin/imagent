# imagent — 现状架构（as-built）

> 本文描述**当前代码的真实结构**（随迭代维护）。历史设计快照见 [DESIGN.md](./DESIGN.md)，
> 平台扩展设计见 [FEISHU_DESIGN.md](./FEISHU_DESIGN.md)，用户视角的使用说明见仓库
> [README.md](https://github.com/uzziahlin/imagent#readme)。

## 1. 三层 + 双抽象

```
trait Platform（收发 IM）              trait Backend（执行 agent）
├── ilink   个人微信私聊（实验性）      ├── claude  CLI 每轮 spawn + ACP 长驻子进程
├── wecom   企业微信智能机器人长连接    ├── codex   codex exec --json
└── feishu  飞书私聊/群/@bot/云文档评论 └── gemini  gemini -p -o stream-json
        ↕                                      ↕
              core：调度 / 鉴权 / 会话路由 / 权限审批闭环
              任务控制 / 批处理 / 流式回传（store 持久化）
```

- **Platform**（`core::platform`）：`recv` / `send_text` / `send_media` / `send_card` +
  `update_card`（流式卡片）/ `send_permission_ask` + `cancel_permission_ask`（审批卡）/
  `reconnect` / `supports_streaming_card`。默认实现把卡片降级为纯文本——新平台只需
  实现文本路径即可接入。
- **Backend**（`core::backend`）：**无状态执行器**。`run(conv, prompt, session,
  workdir, allowed_tools, chunks)` 经 `AgentChunk` 流推中间事件（`Text` / `ToolUse` /
  `ToolResult` / `Media` / `SessionStarted` / `Final` / `Error`）；session 生命周期
  归 core（`SessionStarted` 让中断/失败路径也能持久化 backend 学到的 session id）。

依赖方向：`main → core → {Platform impl, Backend impl, store}`；实现 crate 只依赖
core + store（依赖倒置，core 不认任何具体平台/后端类型）。

## 2. crate 全景

```
crates/
├── core/    调度核心（见 §3）
├── ilink/   iLink 协议：登录扫码 / 长轮询 / AES-128-ECB 媒体 / 服从式限流退避
├── wecom/   企微 OpenWS 长连接：subscribe 认证 / 心跳 / markdown 回复
├── feishu/  open-lark WS 长连接 + 手写 CardKit/评论/媒体/ASR HTTP；429/230020 退避
├── claude/  CLI（stream-json 解析）+ ACP（JSON-RPC 长驻）；~/.claude 会话扫描
├── codex/   codex exec；~/.codex/sessions rollout 扫描（/resume 接管）
├── gemini/  gemini CLI（无本机存储概念，/resume 仅 IM 历史）
└── store/   SQLite（bundled 静态链接）schema v1→v9 线性迁移（见 §5）
fuzz/        cargo-fuzz targets：ilink 协议解析 / CDN host SSRF / 飞书事件解析
src/main.rs  组装：CLI（clap）、单实例锁、信号、/health + /metrics、孤儿卡片扫描
```

## 3. core 内部结构（dispatch/ 目录）

```
crates/core/src/
├── dispatch/
│   ├── mod.rs       Dispatcher 状态 + run() 主循环 + conv 锁/批处理 runner + reply 基元
│   ├── commands/    handle()：发现态引导 → 白名单门 → 28 命令分派 → 普通消息入口
│   │   ├── admin.rs   /allow /disallow /list /whoami /chat /config /perm（多数 admin 门槛）
│   │   ├── session.rs /new /switch /sessions /resume /compact /stop
│   │   └── misc.rs    /status /doctor /reconnect /cd /ws /img /help
│   ├── round.rs     单轮 agent 状态机：typing → 续接 → 摘要注入 → 流式收集（看门狗）
│   │                → 回传 → 落库；中止/失败路径的 session 持久化
│   └── socket.rs    权限审批 Unix socket（peer-uid 鉴权 + token 双行握手）
├── card_session.rs  流式卡片会话：累积 + 500ms 节流 patch；live_cards 登记/摘除；
│                    sweep_live_cards() 启动扫描关孤儿卡片
├── permission.rs    PermissionRouter：register/route/cancel + parse_reply（fail-closed）
├── auth.rs          双白名单（sender / 会话）+ admin_senders 门槛 + 发现模式
├── backend_common.rs 跨后端共用：session 学习、工具摘要、超时包装
├── mcp.rs           权限 MCP 子命令（`imagent mcp`，作为 claude 的 --permission-prompt-tool）
├── store ↔ imagent-store、instance.rs（flock 单实例）、metrics、paths、dedup、message
```

## 4. 消息流水线

```
Platform::recv ─→ 鉴权门（sender ∪ 会话白名单；空白名单=发现模式回引导）
   ├→ 斜杠命令：admin 门槛 → cmd_x（多数取 conv 锁与在飞任务串行）
   └→ 普通消息：enqueue_or_become_runner（PENDING_QUEUE_CAP=100）
        runner 循环〔持 conv 锁〕→ batch_window 静默判停合并连发（连发未停继续
        等，3× 窗口封顶 10s）→ run_agent_round〔成功轮水位超
        auto_compact_threshold_tokens 自动走 /compact 管道〕
             ├ 续接：store.get_session → Some(id) ?
             ├ 注入：/compact 摘要（新会话时）+ 媒体路径提示
             ├ 执行：Backend::run（tokio::spawn 注册 running 表，/stop 可 abort）
             ├ 流式：CardSession（支持卡片平台）或文本分片（reply_ok 只记成功前缀）
             ├ 看门狗：agent_idle_timeout 无 chunk → abort + Error 终态
             └ 落库：upsert_session（事务）+ session_history（per-conv 保 50）
```

关键不变量：**同 conv 串行**（锁跨轮次）；**/stop 不取锁**（取了等价于没停；
中断后**排队消息保留**、runner 自动取批续跑 = steering 语义，`/stop all` 才硬停
清队列）；**审批等待暂停看门狗**（审批有独立超时预算）。

## 5. 存储（schema v9）

| 表 | 用途 |
|---|---|
| `credentials` | 平台凭据（keyring 优先，明文回退可关 fail-closed） |
| `sessions` / `named_sessions` / `session_history` | 每 conv 活动 session / 命名会话 / 历史侧表（/resume 数据源，保 50） |
| `sync_buf` / `context_tokens` | iLink 长轮询游标 / 出站 context_token |
| `run_stats` | per-run 用量/成本（v8；v9 加 `sender` 列——per-sender 成本上限数据源；轮转 10000 条） |
| `config` | KV：workdir、active_name、compact 摘要、命名工作空间 |
| `allowed_senders` / `allowed_chats` / `audit_log` | 双白名单 + 审计 |
| `live_cards` | 在飞流式卡片登记（孤儿卡片启动关流，见 §7） |

keyring username 带 profile 段：`{profile}:{platform}:{account}`（旧键读取
fallback）。DB / WAL / SHM / socket / token / 媒体文件统一 0600，媒体目录 0700。

## 6. 权限审批闭环

```
claude（--permission-prompt-tool）─MCP─→ imagent mcp 子进程
   └ Unix socket（<sock_dir>/imagent-perm.sock）
     · 对端 SO_PEERCRED uid 必须 = 本进程 uid
     · 双行握手：token 行（读 <sock_dir>/permission.token，0600）+ JSON 请求行
   └ core PermissionRouter.register（oneshot 等待，permission_ask_timeout 预算）
        └ Platform::send_permission_ask（飞书=按钮卡片，其它=文本 y/n）
             └ 用户回复 / 按钮回调 / 卡片上表情回应（飞书 👍/👎，v3）
                → can_route_permission_reply（同一白名单门）
                  → parse_reply（仅精确 y/allow 词过，fail-closed）→ 写回 socket
```

- 权限模式 `/perm off|allow|deny|ask`（热切；ask 的 socket 随 run() 启动，切到 ask
  需重启生效）。
- `/stop` 或超时 → cancel：pending 回复通道以 deny 唤醒（MCP 侧不悬挂），并撤回
  询问卡（仅确有 pending 时，防误撤旧卡）。

## 7. 可靠性机制

- **单实例**：`<imagent_home>/instance.lock` flock（内核随 fd 持锁，消除并发启动
  竞态与 PID 复用误判）。
- **孤儿流式卡片**：卡片首帧句柄登记 `live_cards`，终态 patch 成功摘除；进程崩溃
  后下次启动 `sweep_live_cards` 把滞留「生成中」的卡片 patch 成「已重启中断」。
  终态 patch 失败时结论降级纯文本补发（P5-11），卡片留给扫描收尾。
- **看门狗**：agent_idle_timeout 无输出 → abort 杀子进程（CLI kill_on_drop；ACP
  cancel 分支断连接）。
- **限流**：飞书手写 HTTP + SDK 路径统一 429/230020 退避重试；token 失效错误码
  （99991663 族）清缓存强制刷新重试一次。
- **热重载**：SIGHUP 重读 config（permission_mode / allowed_tools 即时生效）。

## 8. 可观测

- Prometheus（`/metrics`）：`imagent_messages_in/out_total`、`backend_calls/errors`、
  `permission_decisions_total{allow|deny|timeout|dropped}`、
  `agent_timeouts_total{idle|total}`。
- `/health`：logged_in 按实际平台判定（ilink/wecom 查 store 凭据、feishu 查环境
  变量）+ 启动时长。
- 结构化日志 `tracing`（RUST_LOG 控制）；密钥在 Debug 输出中脱敏。

## 9. 安全模型（硬约束）

1. 双白名单：sender / 会话（群），二者其一过门；空白名单 = 发现模式（不驱动
   agent，回引导）。授权类命令（/allow /perm /config …）另需 admin_senders。
2. `--allowedTools` 配置收敛 + workdir 锁定（/cd、/ws、/resume 接管本机会话时校验
   cwd 一致）。
3. 权限审批 fail-closed：非精确允许词一律 deny；超时/中断 = deny。
4. 凭据 OS keyring 优先（profile 隔离）；明文回退可配置禁用（fail-closed）。
5. 文件权限：db/socket/token/媒体 0600；/img 路径 canonicalize 后必须仍在 workdir
   内。
6. 入站不可信输入：协议解析有 fuzz 覆盖（ilink 协议 / CDN host SSRF 白名单 /
   飞书事件 payload）。

## 10. Profile 多实例

`--profile <p>` 隔离 config 路径、DB、socket 目录、媒体目录与 keyring username
段——同机多 bot 身份互不干扰（单实例锁也按 profile 隔离）。
