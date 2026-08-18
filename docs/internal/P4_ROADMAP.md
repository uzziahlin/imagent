# P4 迭代路线 —— 功能迭代（对标 lark-coding-agent-bridge）

> 来源：2026-08-15 与 [lark-coding-agent-bridge](https://github.com/zarazhangrui/lark-coding-agent-bridge)（Node.js 飞书桥，2.3k stars）逐特性对比后的差距分析。
> 结论：核心链路（双向消息 / 流式卡片 / 媒体收发 / 权限审批闭环 / 多工作区 / 会话持久化）已对齐或反超；缺口集中在**任务控制、消息批处理、飞书交互深化**三块。
> 明确不跟：遥测 Adapter（已有 Prometheus）、Windows 支持（刻意只做 Unix）、lark-cli 身份策略（个人 token 场景）、QR 向导（飞书自建应用不适用）。

## 总览

| # | 功能 | 优先级 | 状态 |
|---|---|---|---|
| 1 | `/stop` 中断在飞任务 | 高 | ✅ 已实现 |
| 2 | 运行中消息合并批处理 | 高 | ✅ 已实现 |
| 3 | 空闲看门狗（agent 无输出自动终止） | 高 | ✅ 已实现 |
| 4 | 权限审批卡片按钮（CardKit 交互） | 中 | ✅ 已实现 |
| 5 | 群维度白名单（allowed chats） | 中 | ✅ 已实现 |
| 6 | COT 三档展示配置 + `/config` | 中 | ✅ 已实现 |
| 7 | IM 内诊断命令（`/status` `/doctor` `/reconnect`） | 中低 | ✅ 已实现 |
| 8 | IM 内恢复任意历史会话（`/resume`） | 中低 | ✅ 已实现 |
| 9 | 云文档评论触发（飞书 doc 评论 @bot） | 低（大特性） | ✅ 已实现（MVP） |
| 10 | Profile 多实例（一个部署多 bot 身份） | 低 | ✅ 已实现（MVP） |

---

## 高优先级三项设计（本轮实现）

### 1. `/stop` 中断在飞任务

**问题**：agent 跑偏 / 卡死时，用户在 IM 里只能干等 `agent_timeout`（默认 10 分钟）超时。

**方案**：Dispatcher 维护 per-conv 在飞任务注册表 `running: Mutex<HashMap<conv_id, AbortHandle>>`。
`run_agent_round` 在 spawn backend join task 后注册、轮次结束后移除（外层包装统一移除，失败/panic/中断路径不泄漏）。
`/stop` **不取 conv 串行锁**（取了会等到任务结束才生效，死锁式无效），直接：

1. `router.cancel(conv)` —— 若正等权限审批，pending 回复通道 drop → MCP 收到 deny（fail-closed），agent 侧不悬挂；
2. 取 `running` 条目并 `abort()` —— join task 被 abort → `backend.run` future drop：
   - CLI backend（claude-cli / codex / gemini）：`kill_on_drop` 杀子进程；
   - ACP backend：`cancel_tx` drop → 长驻 task select 命中 cancel 分支 → connection drop → ChildGuard 杀子进程（既有 P1-E 语义，全连接重建，下次 run 自动恢复）；
3. 清空该 conv 的批处理排队消息（stop = 全停，含未跑的），回复中断确认 + 丢弃条数。

被中断的轮次：卡片平台 finalize 成 `Error("已中断")` 终态（防流式卡片停在「生成中」）；**不再发文本回复**（`/stop` 命令本身已确认，避免双条）；不落 session（outcome 不可得，保留上次成功映射）。

**权限闭环交互修正**（顺带）：recv 循环把 pending conv 的任何消息当审批回复消费，`/stop` 会被误吞成 deny。增加守卫：以 `/` 开头的消息不进审批路由，走命令路径（`/stop` 在等审批时也能用；普通 `y/n` 回复语义不变）。

### 2. 运行中消息合并批处理

**问题**：per-conv 串行锁下每条消息独立跑一轮 agent——用户连发 3 条 = 3 轮，体验差且烧 token。

**方案**：per-conv 批处理队列 `queues: Mutex<HashMap<conv_id, ConvQueue{pending: Vec<InboundMessage>}>>`，
「入队 / 成为 runner」与「取批 / 交还」都在同一把 queues 锁内原子完成，杜绝 lost-wakeup（消息卡在无人认领的队列里）：

```
消息到达（普通）:
  entry 存在（runner 在飞）→ push pending，本 task 即返（静默排队，agent 仍在流式输出可见）
  entry 不存在 → 建 entry{pending=[msg]}，本 task 成为 runner

runner 循环（持有 conv 串行锁，跨轮次）:
  1. sleep(batch_window)            # 批处理窗口：等后续消息并入（0 = 关闭）
  2. take_batch():                  # 原子：pending 空 → 删 entry、退出循环；非空 → drain 返回
  3. merge(batch) → run_agent_round(merged)
  4. 回到 1（下一轮继续吃窗口期消息）
```

**合并语义**：非空文本以 `\n\n` 拼接；media / media_errors 列表拼接；sender / reply_hint 取首条（各消息入队前已各自过白名单）。
**批处理窗口** `batch_window_ms`（默认 1500，0 关闭）：runner 起跑前短等，把「连发补一句话」并进同一轮 prompt，而非多跑一轮。
**队列上限**：per-conv pending 上限 100 条，超出回「队列已满」并丢弃，防刷屏把 prompt 撑爆。
`/stop` 清空 pending（见上）；`/new` 等既有 slash 命令仍走 conv 锁排队，语义不变。

### 3. 空闲看门狗 `agent_idle_timeout_secs`

**问题**：`agent_timeout` 是总预算；流式卡死（子进程 hang、协议僵死）时要等满总超时。

**方案**：在 dispatch 收集 chunks 的 `rx.recv()` 循环上加 idle 超时（**core 单点实现，四个 backend 零改动**）：

```rust
match tokio::time::timeout(agent_idle_timeout, rx.recv()).await {
    Ok(chunk) => ...,           // 正常推进（每次收到 chunk 计时器自然重置）
    Err(_elapsed) => {
        if router.has_pending(conv) { continue; }  // 等审批期间暂停看门狗（审批有独立预算）
        idle_timed_out = true; break;              // 空闲超时：abort join → 杀子进程
    }
}
```

- 配置 `agent_idle_timeout_secs`，默认 300（5 分钟无输出），0 = 关闭；建议 ≤ `agent_timeout_secs`。
- 触发后：abort join（杀子进程链路同 `/stop`）、卡片 finalize `Error` 终态、回复「空闲超时，本轮输出未保存，会话保持上次成功状态」、`METRICS.backend_errors` 计数。
- 与 `/stop` 的区分：idle 是本地 flag（收 chunks 循环内判定）；/stop 走 `JoinError::is_cancelled()` 且非 idle → 用户中断分支（不发文本，仅卡片终态）。

### 公共实现要点

- 新增 `TaskBudgets` 聚合时长配置（agent_timeout / permission_ask_timeout / shutdown_grace / agent_idle_timeout / batch_window），`Dispatcher::new*` 参数表不再膨胀（原 11 参 → 9 参）。
- `Config::EXAMPLE` 与 README 配置段补两个新键；`/help` 补 `/stop`。
- conv 串行锁的获取/释放从单轮移到 runner 循环外层（P1-7 的防泄漏语义不变：循环结束统一释放）。

---

## 待排期各项（已全部实现，实现纪要如下）

### 4. 权限审批卡片按钮（中）✅
`Platform::send_permission_ask`（默认纯文本，`send_permission_ask_text` 独立方法供降级复用）；
feishu 覆写为 CardKit 2.0 action 卡片（✅ 允许 / ⛔ 拒绝按钮，`behaviors: callback`，
value 编码 conv + 动作）。点击推 `card.action.trigger` 事件 → drain 解析成
`text="y"/"n"` 的入站消息 → 复用既有审批回复路由（core 零感知）。卡片失败降级文本。
需应用开通卡片回调（事件订阅 `card.action.trigger`）。

### 5. 群维度白名单（中）✅
store v4 `allowed_chats` 表（conv_id 原样，平台无关）；`Auth::with_chats` +
`is_chat_allowed`；鉴权改为「sender 放行 OR chat 放行」；`/chat allow|deny|list`
（管理员门槛，缺省作用于当前会话）+ CLI `imagent allow-chat <conv_id>` + config
种子 `allowed_chats` + SIGHUP 热重载。引导（发现态）提示两个 id。

### 6. COT 三档展示配置 + `/config`（中）✅
`CotDetail`（off/brief/detailed；brief=40 字符 5 工具，detailed=200 字符 10 工具）。
off 档不收集工具过程（无摘要、无卡片工具面板）。`/config` 查看 + 热改
`cot_detail` / `batch_window_ms` / `agent_idle_timeout_secs`（管理员）。

### 7. IM 内诊断命令（中低）✅
`/status`（平台/后端、本会话在跑与排队、会话、workdir、全局在飞、uptime）；
`/doctor`（workdir / store 读写回环 / 会话数 / 在飞 / 平台能力自检）；
`/reconnect`（`Platform::reconnect`：feishu/wecom 经共享 `Notify` 唤醒 run loop 的
select 丢弃连接 future → 断开重连；其它平台回不支持提示）。

### 8. IM 内恢复任意历史会话（中低）✅
store v5 `session_history` 表（upsert_session 在 session_id 变化时同步记录）。
`/resume` 列最近 10 条（当前带 *）；`/resume <序号|session_id>` 恢复（跨后端校验同
/switch；恢复后回到未命名会话）。

**增强（P4-11，统一 /resume 无感接管）**：列表合并本机同项目 agent 会话——
`Backend::list_local_sessions`（claude 两后端扫 `~/.claude/projects/<workdir编码>/
*.jsonl`，首条用户消息摘要 + mtime 相对时间展示，💻/📱 标来源），按序号选中 💻 即
自动接管（sessions 表绑定 + 分叉提示）；workdir 对齐由「按 conv 当前 workdir 扫描」
天然保证。列表 per-conv 缓存防序号错位。codex/gemini 默认空 → 纯 IM 历史。

### 9. 云文档评论触发（低，大特性）✅（MVP）
事件 `drive.file.comment.created_v1`（需飞书后台订阅 + `drive:comment` 权限）→
conv `feishu:comment:<file_token>:<comment_id>`（每评论一线程，会话独立续接）；
回复走 `POST /drive/v1/files/{f}/comments/{c}/replies`（手写 HTTP，同 CardKit 做法）。
纯 @ / 纯图片评论不触发；评论线程不支持流式卡片（per-conv
`supports_streaming_card`），媒体回传只对聊天会话生效。评论里的回复（reply）
是新评论事件、各自成线程——线程树语义为后续增强。

### 10. Profile 多实例（低）✅（MVP）
`imagent_core::paths::imagent_home()`（env `IMAGENT_HOME` 覆写）统一锚定全部本地
状态（config / db / permission.sock / 媒体缓存；MCP 子进程经 env 继承同目录）。
CLI：`--profile <name>`（切到 `~/.imagent/profiles/<name>`，不存在报错引导）+
`profile create|list|remove`（create 写 config 模板；remove 需 --yes）。
已知限制：OS keyring 凭据键（platform+account）未按 profile 隔离——同机多
profile 跑同一平台会共享凭据条目（后续可加 profile 前缀）；SQLite/媒体/sock 已完全隔离。

---

# P5 迭代路线 —— 深度 Review：安全与正确性修复

> 来源：2026-08-18 全量深度 review（三个并行探查扫平台层/后端层/存储配置层 + 核心人工核实）。
> 结论：架构底子好（迁移原子性、子进程治理、批处理单锁原子性等验证无问题），但发现
> **6 个安全问题、9 个正确性 bug、一批设计债务**。第一批（P5-1～P5-6）已修复。

## 总览

### 第一批（已修复 ✅）

| # | 问题 | 严重度 | 状态 |
|---|---|---|---|
| P5-1 | 审批回复路由绕过白名单（陌生人 "y" 可批准高危工具） | 严重·安全 | ✅ |
| P5-2 | `/perm` 无管理员校验（群成员可热切 off 拆审批闭环） | 高·安全 | ✅ |
| P5-3 | `/disallow` 无管理员校验（可把管理员踢出白名单） | 高·安全 | ✅ |
| P5-4 | ACP session 缓存覆盖外部值（/new /resume /switch 全失效） | 高 | ✅ |
| P5-5 | 中断/失败路径丢 session id（下轮静默开新会话「失忆」） | 高 | ✅ |
| P5-6 | ACP turn 结束不清 current（每轮回复拖满 idle_timeout） | 高（连带发现） | ✅ |

### 第二批（已修复 ✅，同日）

| # | 问题 | 严重度 | 状态 |
|---|---|---|---|
| P5-7 | 群放行 + admin_senders 空 = 群内全员事实管理员，启动期无任何提示 | 高·安全 | ✅（硬告警） |
| P5-8 | 飞书评论事件不滤 @bot、不滤 bot 自身（任何评论都驱动 agent） | 高·安全 | ✅ |
| P5-10 | codex/gemini/ACP 在非卡片平台回复推两遍 | 高 | ✅ |
| P5-11 | 流式卡片终态更新失败 → 用户永远拿不到结论 | 高 | ✅（文本降级） |
| P5-12 | wecom 群消息错发单聊 / 入站满丢帧 / ack 错误静默 | 中高 | ✅（保守修） |

### 待排期（安全）

| # | 问题 | 严重度 |
|---|---|---|
| P5-9 | 单实例保护缺失（第二实例删并重 bind permission.sock，第一实例 Ask 闭环静默失效）→ lockfile/PID 互斥；同 uid socket 伪造（已知 P2-7）→ bind 时生成 token、MCP 子进程携带校验 | 中 |

### 待排期（正确性）

| # | 问题 | 严重度 |
|---|---|---|
| P5-13 | ilink 游标推进失败仅 warn → dedup 窗口（5min）过期后同批消息重复驱动 agent。方案：set_sync_buf 失败升级为 Err 走退避重试 | 中高 |
| P5-14 | ACP 单连接全局串行 + 排队期烧 agent_timeout（A 的长任务让 B 直接超时）。方案：per-conv 长驻连接；timeout 预算从出队起算 | 中高 |
| P5-15 | claude 项目目录编码歧义：`/`→`-` 使 `/a/b-c` 与 `/a/b/c` 同目录；真实 Claude 编码还处理 `.` `_` 等 → 含这些字符的 workdir 静默扫不到。方案：对照真实布局校准 + 接管前校验 jsonl 内 cwd | 中 |
| P5-16 | /stop 收尾不完整：cancel 不唤醒已注册的审批等待者（最长挂 300s）、IM 询问卡片不撤回、/compact 未注册进 running 无法被 /stop | 中 |

### 待排期（设计债务 / 体验）

- dispatch.rs 已 4500+ 行（命令解析/会话状态机/批处理/权限/2200 行测试混杂），拆 `commands/` 子模块——后续所有迭代的摩擦来源。
- 配置零校验：`agent_timeout_secs=0` → 全部瞬时超时（无「0=禁用」语义）、`batch_window_ms` 无上限、workdir 不查存在；config 加载失败退出码 0（systemd 视为成功）。`Config::load` 加下界/上限校验 + 失败改非零退出。
- 多 profile 隔离泄漏：keyring service 固定 `imagent`（两 profile 互删凭据）→ username 拼 profile 段；ilink 媒体目录写死 `~/.imagent/media` 不读 `IMAGENT_HOME` → 改 `imagent_home()`。
- 媒体治理：feishu 下载无大小上限（ilink 有 50MB，照抄）+ 两平台媒体目录均无 TTL/LRU 清理（磁盘只增不减）。
- 飞书限流与 token：send/patch 无 429 识别退避（分片中断产生截断回复无标记）；token 刷新持写锁跨 30s 网络（双检锁）+ token 失效错误码不主动清缓存。
- 存储：`session_history` 无上限增长（仿 audit_log 轮转，per-conv 保 50）；`upsert_session` 两条语句非同事务（包 `unchecked_transaction`）。
- 可观测性：无 permission approve/deny/timeout 计数、无超时分类；`/health` logged_in 固定查 ilink（feishu/wecom 恒 false 误导）。
- 优雅退出：无二次 Ctrl-C 强退（grace 大时只能 kill -9）。
- 能力一致性：codex/gemini 无 `list_local_sessions`（/resume 退化）；ACP Ask 一律 fail-closed 拒绝 + allowed_tools 忽略（接 PermissionRouter 到 session/request_permission）。
- 小项：`/resume` 缓存不随 `/cd` 失效（旧目录列表误导选择）；`/resume` 选中即消费后序号移位易误选；飞书 card action/comment 解析器无 fuzz target；mdBook 文档站内容陈旧（README 已同步、docs/ 未同步）。

---

## P5 第一批实现纪要（2026-08-18）

### P5-1 审批回复路由前置白名单 ✅

`dispatch.rs` run() 循环：route 消费审批回复前过 `can_route_permission_reply`
（`auth.is_allowed(sender) OR is_chat_allowed(conv)`——与 handle() 鉴权门完全一致）。
未过门的消息落到 handle() 走正常丢弃路径。飞书审批按钮回调自带 operator
open_id 作 sender，同一门槛覆盖按钮路径（零 feishu 改动）。

### P5-2/P5-3 `/perm` `/disallow` 管理员门槛 ✅

与 `/config` `/allow` `/chat` 对齐：`is_admin` 检查（admin_senders 空 = 向后兼容
全员可，非空严格匹配）。`/perm` 仅查看（无参）保持开放。

### P5-4 ACP 会话选择以 req.session 为权威 ✅

`acp.rs`：删除 per-conv `sessions: HashMap`（命中即用、无视 req.session，是
/new /resume /switch 失效的根因），改为连接级 `loaded: Option<String>`——同 sid
连续轮次免重复 LoadSession 的纯优化，无 per-conv 状态。`PromptReq` 去掉 conv_id
字段。

### P5-5 中断/失败路径持久化 session id ✅

- `AgentChunk::SessionStarted(String)` 新变体：backend 一经学到 session id 即通知。
  CLI 侧 backend_common 五个学习点（Session/ToolUse/Final/Error/Terminal）首学即发；
  ACP 建会话/续接后即发。
- dispatch chunk 循环记 `learned_sid`；`Ok(Err)` / cancelled（/stop、空闲超时）/ panic
  三个提前返回分支调用 `persist_learned_session`：学到的 id 非空且与本轮传入不同才
  upsert（sessions + session_history + 命名侧表），下条消息续接而非静默开新会话。
  与 Claude Code 自身中断语义一致（中断留原会话，显式 /new 才重开）。
- 顺带修正：主路径与中断路径落库 workdir 改用 `resolve_workdir` 实际值（原写
  default_workdir，/cd 后记错）；空闲超时文案改为「进度已保留，下条消息续接」。

### P5-6 ACP turn 结束清理 current ✅

长驻 task 每个 turn 结束（select 之后）清 `*current = None`——StreamState 持有
chunks sender 克隆，残留会让 dispatch 的 chunk 循环等不到通道关闭、挂到空闲看门狗
才退出（此前 ACP 每轮回复被拖满 agent_idle_timeout）。cancel 分支 break 跳过清理，
但连接随之销毁、sender 一并释放。

**验证**：4 个新测试（permission_reply_gate_checks_sender / perm_switch_requires_admin /
disallow_requires_admin / stop_persists_learned_session），全量 324 passed、fmt/clippy 绿。
已知限制：ACP 改动无单测覆盖（需真机 claude-agent-acp 冒烟：`cargo test -p
imagent-claude -- --ignored acp_e2e`）；SessionStarted 与 abort 之间存在极小竞态窗口
（chunk 未及消费则该轮仍可能丢 session，best-effort）。

---

## P5 第二批实现纪要（2026-08-18）

### P5-7 群放行 + 空管理员组合的启动硬告警 ✅

`Config::admin_gap_with_chat_allowlist()`（config.rs）：`allowed_chats` 非空且
`admin_senders` 为空即命中。main 启动期 `error!` 级告警（含收紧指引：/whoami 查 id →
config 设 admin_senders）。不拒启——单用户依赖「空=全员可」的既有语义，硬告警足够
可观测且不破坏存量部署。

### P5-8 飞书评论 @bot 过滤 + bot 自身过滤 ✅

- `parse_comment_event(payload, bot_open_id)` 新签名：bot id 已知时要求 at 节点命中
  bot（`user_id`/`open_id` 字段都认，前者是评论载荷的历史命名）**且** sender 非 bot
  自身（防自回复循环）；bot id 未知时退化为「至少含一个 at 节点」的弱过滤。
- bot open_id 经 `GET /open-apis/bot/v3/info` 懒取（`client::fetch_bot_open_id`，
  tenant token 复用既有缓存），drain 首次遇到评论事件时取一次并缓存（open_id 随应用
  固定）。`proto::is_comment_event` 廉价预判避免对无关事件发起 HTTP。
- **行为变化**：此前「任何带文字的评论都触发一轮 agent」，现在必须 @bot——文档里
  已有此约定（README/P4-9 描述），代码终于对齐。

### P5-10 非卡片平台流式文本去重 ✅

dispatch 收集循环累积 `streamed_text`（非卡片平台实时推送的 Text 前缀）；最终回复
构造后 `strip_prefix` 只补差量——codex/gemini/ACP（中间 Text + Final 全量）不再整段
重发两遍。前缀不对齐（后端语义异常）时保留全量（宁重复不丢内容）；流式已推完且无
差量/无摘要时不发空消息。claude-cli 无中间 Text，行为不变。

### P5-11 卡片终态失败降级纯文本 ✅

`CardSession::dispatch_card` 返回成功与否；`finalize` 在终态 patch 失败（网络/限流/
卡片服务异常）且文本非空时 `platform.send_text` 补发结论——**卡片可以停在「生成中」，
结论不能丢**。流式阶段（Running）失败不降级（后续 patch 自然重试）。残余：进程崩溃
后孤儿卡片仍会停在「生成中」（需持久化卡片句柄 + 启动扫描关流，留待排期）。

### P5-12 wecom 三处保守修复 ✅

- **群消息显式拒收**：`parse_msg_callback` 对 `chattype=group` 返回 Err（drain 层
  warn 可观测）——此前群消息被当 `wecom:<userid>` 单聊处理，回复错发到与发言者的
  私聊。群聊支持（按群 chatid 收发）留待后续。
- **入站有界背压**：`try_send` 满即丢改为 1s 超时的 `send().await`——消费端短暂
  抖动不再丢用户消息；仍不能无限 await（饿死心跳分支会被服务端 30s 断连）。
- **ack 错误升告**：无 cmd 的 ack 帧 errcode≠0（出站请求被拒：限流/chatid 非法等）
  从 debug 升级为 warn（带 req_id/errcode/errmsg）。req_id 关联的完整 ack 等待闭环
  **未做**——需真机验证企微对 aibot_send_msg 的回执语义后再设计（盲做可能因服务端
  不回 ack 把每次发送拖满超时）。

**验证**：新增 5 测试（config 组合探测 / 卡片降级 / 流式去重 / 评论 @bot 过滤 /
wecom 群拒收），全量 329 passed、fmt/clippy 绿。
