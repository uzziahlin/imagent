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

### 第三批（已修复 ✅，同日）

| # | 问题 | 严重度 | 状态 |
|---|---|---|---|
| P5-9 | 双实例互劫持 permission.sock（Ask 闭环静默失效）+ 同 uid 裸 connect 伪造审批 | 中·安全 | ✅（单实例锁 + 握手 token） |
| P5-13 | ilink 游标推进失败仅 warn → 5min 后同批消息重复驱动 agent | 中高 | ✅ |
| P5-15 | 项目目录编码歧义（`/a/b-c` 与 `/a/b/c` 同码；`.` `_` 规则未实测） | 中 | ✅（候选联合扫描 + cwd 校验） |
| P5-16 | /stop 收尾：审批等待者挂 300s、询问卡片滞留可点、/compact 不可中断 | 中 | ✅ |
| 快赢 | config 数值零校验、加载失败退出码 0、无二次 Ctrl-C、/resume 缓存不随 /cd 失效、ilink 媒体目录写死、feishu 下载无上限 | 中低 | ✅ |

### 待排期（正确性）

| # | 问题 | 严重度 |
|---|---|---|
| P5-14 | ACP 单连接全局串行 + 排队期烧 agent_timeout（A 的长任务让 B 直接超时）。方案：per-conv 长驻连接；timeout 预算从出队起算。**需真机 claude-agent-acp 验证后实施** | 中高 |

### 第五批（push 后自审回归修复 ✅，同日）

> 来源：push 后对 P5 四批 diff 的二次审查（自查 + 独立复查），发现三处自己引入的
> 回归与若干次级问题，本批全部修复。

| 项 | 内容 | 状态 |
|---|---|---|
| 回归 | P5-13 游标致命化实为**丢消息**（dedup 键已插，重拉被吸收）→ 改原地重试 3 次，仍失败照常投递 + error 告警（宁重复不丢） | ✅ |
| 回归 | `--profile` 下 `status` 漏设 keyring scope（读不到 scoped 凭据/显示旧凭据）→ 补齐 + 按平台查凭据（wecom/feishu 给出 config/env 指引） | ✅ |
| 回归 | `/health` wecom 恒 false（凭据在 config 不在 store）→ 启动时按存在性预算 hint | ✅ |
| 缺陷 | 单实例锁「排他创建+事后写 PID」有并发启动竞态 → 改 `flock`（内核持锁，无陈旧锁/删除竞态） | ✅ |
| 缺陷 | 流式去重会把推送失败的段落从最终回复裁掉（两处皆失）→ 只累积发送成功的前缀（`reply_ok`） | ✅ |
| 缺陷 | feishu 429 重试在卡片/评论路径不生效（cardkit_resp 不看 HTTP 状态）→ 先判 429 归一标记 | ✅ |
| 小项 | `/ws use` 失效 /resume 缓存；codex 扫描 spawn_blocking；/stop 仅在有 pending 时撤询问卡（防误 patch 已回答的旧卡） | ✅ |
| 补测 | /stop 中断 /compact；Err 路径 session 持久化 | ✅ |

### 第四批（设计债务收敛 ✅，同日）

| 项 | 内容 | 状态 |
|---|---|---|
| store | `upsert_session` 事务化（主表+历史同事务）+ `session_history` per-conv 轮转保 50 | ✅ |
| keyring | username 按 profile 分段（`{scope}:{platform}:{account}`），读取旧键 fallback，删除双键清理 | ✅ |
| metrics | `permission_decisions_total{allow/deny/timeout/dropped}` + `agent_timeouts_total{idle/total}`；`/health` logged_in 按实际平台判定 | ✅ |
| 媒体治理 | `<imagent_home>/media` TTL 清理（7 天，启动 + 每日循环） | ✅ |
| feishu | token 读锁快路径 + 双检；手写 HTTP（卡片 PATCH/评论回复/媒体下载）429/230020 退避重试；分片失败标注序号 | ✅ |
| codex | `list_local_sessions`：扫 `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`（session_meta id+cwd），/resume 可接管本机 codex 会话 | ✅ |

### 待排期（设计债务 / 体验，剩余）

- dispatch.rs 已 4700+ 行（命令解析/会话状态机/批处理/权限/2400 行测试混杂），拆 `commands/` 子模块——后续所有迭代的摩擦来源。
- 飞书 token 失效错误码（99991663 类）不主动清缓存重试（SDK 路径无状态透传，需改手写或错误码管道）；SDK 路径（send_text_msg 等）的 429 重试同此。
- 进程崩溃后的孤儿流式卡片仍停在「生成中」（P5-11 只覆盖进程活着时 patch 失败；需持久化卡片句柄 + 启动扫描关流，涉及 store schema）。
- wecom 出站 ack 完整等待闭环（req_id 关联 oneshot；需真机验证回执语义后设计）。
- ACP Ask 接 PermissionRouter 到 session/request_permission + allowed_tools 映射；P5-14（per-conv 连接）需真机验证。
- gemini 无本机存储概念，/resume 保持纯 IM 历史（不跟进）。
- 小项：飞书 card action/comment 解析器无 fuzz target；mdBook 文档站内容陈旧（README 已同步、docs/ 未同步）。

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

---

## P5 第三批实现纪要（2026-08-18）

### P5-9 单实例锁 + 权限 socket 握手 token ✅

- **单实例锁**（`core::instance`，`<imagent_home>/instance.lock`）：排他创建 +
  PID 存活探测（`kill(pid, 0)`：0/EPERM=存活，ESRCH=陈旧接管）。仅 `imagent start`
  获取（`mcp` 子命令与主进程共存，不加锁）；锁随 File 句柄持有到退出。
- **握手 token**：dispatcher bind socket 时随机生成，写
  `<sock_dir>/permission.token`（0600）；`imagent mcp` 连接时读取并作为**首行**
  回传，不符即丢弃连接（同 uid 裸 connect 伪造 conv_id 推送审批钓鱼的门槛从零
  提高到需读到 token）。残余：同 uid 进程仍可从文件/env/cmdline 获取 token——
  提高门槛而非绝对防护（绝对防护需继承 fd 或抽象命名空间 socket）。退出时
  sock 与 token 文件一并清理。
- 注意协议变化：mcp ↔ 主进程现在是「token 行 + JSON 行」两行握手，主进程与
  mcp 子命令须同版本部署。

### P5-13 ilink 游标推进失败升级为致命 ✅

`set_sync_buf` 失败从「warn 继续」改为返回 `Err`——recv 循环退避后重试整轮
（消息重拉由 dedup 吸收，at-least-once 不变）。此前服务端会每轮重推同批消息，
dedup 窗口（5min）过期后同批被当新消息**重复驱动一轮 agent**。

### P5-15 目录编码候选联合扫描 + 接管 cwd 校验 ✅

- `encode_candidates`：三个候选（仅 `/`→`-` 本机实测规则；`/._`→`-`；非字母
  数字→`-`）联合扫描，session_id 去重。编码规则猜错最多扫不到（退化为纯 IM
  历史），不再漏扫含 `.`/`_` 的 workdir。
- `LocalSession.cwd`：扫描时从 jsonl 头部提取会话记录的 cwd；`/resume` 接管
  本机会话前校验 cwd == 当前 workdir，不符拒绝并引导 `/cd`——即使编码冲突把
  两个项目扫进同一列表，也不会串项目接管。

### P5-16 /stop 收尾三件 ✅

- **cancel 唤醒等待者**：`PermissionRouter::cancel` 移除 pending 前先投递
  fail-closed deny——审批等待方立即收到结果（此前要挂满 permission_ask_timeout
  默认 300s）。
- **询问卡片撤回**：`Platform::cancel_permission_ask`（默认 no-op）；feishu 记录
  询问卡 message_id，`/stop` 时 patch 成「已中断」终态（移除按钮，防对已死任务
  审批）；文本询问平台无句柄、滞留无害。
- **/compact 可中断**：摘要生成任务注册进 `running`（conv 锁持有期间注册/移除
  无 ABA），`/stop` 生效。

### 快赢六项 ✅

- `Config::load` 数值校验：三个超时 ≥ 1、batch_window_ms ≤ 10s（0 值超时 = 全部
  瞬时失败，错误前置到启动期）；加载失败非零退出码（此前 0，systemd 视为成功）。
- 二次 Ctrl-C 强退（130）：优雅退出最长 shutdown_grace，期间操作员不再只能
  kill -9。
- `/cd` 清 `/resume` 列表缓存（列表按当前目录扫描，切目录后旧序号指向旧目录）。
- ilink 媒体目录改走 `imagent_home()`（多 profile 隔离；此前写死 `~/.imagent/media`）。
- feishu 媒体下载改手写实现：Content-Length 预检 + 流式累计 50MB 上限（同 ilink
  双重上限；此前 SDK 版全量缓冲无上限）。

**验证**：新增 8 测试（单实例锁 ×3 / cancel 唤醒 / 候选扫描+cwd 提取 / 接管
cwd 拒绝 / 握手 token 端到端 / config 边界），全量 337 passed、fmt/clippy 绿。
P5-14（ACP per-conv 连接）留待真机验证后实施。


---

## P5 第四批实现纪要（2026-08-18）

### store：upsert 事务化 + session_history 轮转 ✅

`upsert_session` 的主表/历史侧表两条语句包进 `unchecked_transaction`（中间崩溃
不再漏历史行，单次 fsync）；同事务内 `DELETE ... NOT IN (SELECT ... LIMIT 50)`
按 conv 轮转（保留最近 50 条 = 调用方查询上限；此前只增不删）。

### keyring profile 隔离 ✅

username 从 `{platform}:{account}` 改为 `{scope}:{platform}:{account}`（scope =
`--profile` 名；空 = 无 profile 保持旧格式，**存量部署零迁移**）。读取 scoped 键
miss 时回退旧键（过渡期老凭据继续可用，下次 login 写入 scoped 键）；删除时双键
都清理。SQLite marker 不变（DB 文件本身已按 profile 隔离）。main 在 start/login
两处 `set_keyring_scope`。

### metrics + /health ✅

- `imagent_permission_decisions_total{result=allow|deny|timeout|dropped}`：审批
  决策分类（/stop 的 fail-closed deny 计入 deny）。
- `imagent_agent_timeouts_total{kind=idle|total}`：空闲看门狗与总预算超时分开
  计数（定位「agent 慢」是卡死还是预算不足）。
- `/health` 的 `logged_in` 按实际平台判定（ilink/wecom 查 store 凭据，feishu 查
  `IMAGENT_FEISHU_APP_SECRET` 环境变量）——此前固定查 ilink，feishu/wecom 下恒
  false 有误导。

### 媒体 TTL 清理 ✅

`paths::sweep_media_before(dir, cutoff)`（纯函数 + 单测）；main 启动 spawn 后台
任务：启动清一次 + 每日循环，删 `<imagent_home>/media` 下 7 天前的文件。

### feishu：token 双检锁 + 限流退避 + 截断标记 ✅

- `fetch_cached_token` 读锁快路径 + 写锁双检（此前每次直取写锁且跨网络调用，
  最坏 30s 内所有发送串行阻塞）。
- 手写 HTTP 路径（create_card_entity / patch_card_element / patch_card_settings /
  reply_comment / download_message_resource）识别 HTTP 429 / code=230020，
  500ms→1s→2s 退避重试（流式卡片 PATCH 是最高频路径）。SDK 路径无状态透传，
  留待后续。
- `send_text` 分片失败标注「第 N/M 片发送失败（回复可能被截断）」——截断可感知。

### codex 本机会话扫描 ✅

`crates/codex/src/sessions.rs`：扫 `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`
（mtime 倒序检查最近 200 个，读头部 64KiB）——首行 `session_meta` 的 `id`（与
`codex exec --json` 的 thread_id 同源）+ `cwd` 判定归属；首条可展示 user 消息做
摘要（跳过 AGENTS.md 注入）。`/resume` 在 codex 后端不再退化为纯 IM 历史；
`LocalSession.cwd` 接管校验同样生效。

**验证**：新增 7 测试（轮转 / scoped username / TTL 清理 / metrics 注册 / codex
扫描 ×3），全量 343 passed、fmt/clippy 绿。

---

## P5 第五批实现纪要（2026-08-18，push 后自审）

对已 push 的 P5 四批 diff 做二次审查，修复三处自引入回归 + 六处次级问题：

1. **ilink 游标（P5-13 修正）**：致命化版本在 `set_sync_buf` 失败时丢整批消息——
   `process_msg` 已把 dedup 键插入（5min 窗口），退避后重拉同批被去重吸收 = 静默
   丢消息。改为：失败原地重试 3 次（300ms 间隔）；仍失败则**照常投递本批** +
   error 告警（DB 持续故障超 dedup 窗口才可能重复执行——宁可重复，不可丢）。
2. **status 的 keyring scope**：`Cmd::Status` 补 `set_keyring_scope`（此前只有
   Login/Start 设）；平台按 config 判定——wecom/feishu 打印 config/env 指引而非
   误查 ilink 凭据。
3. **/health wecom**：凭据来自 config 的 `wecom_bot_id`/`wecom_secret`，启动时按
   存在性预算 `logged_in_hint` 传入（此前查 store 恒 false）。
4. **单实例锁改 flock**：内核随 fd 持锁到进程退出，消除「排他创建 + 事后写 PID」
   的并发启动竞态（败者读到空锁文件误判陈旧删锁重建）与 PID 复用误判；锁文件
   内容（PID）降级为纯诊断信息。
5. **流式去重失败回滚**：`reply_ok` 返回送达结果，`streamed_text` 只累积成功
   前缀——推送失败的段落留给最终全量兜底（此前两处皆失）。`reply` 保持 `()`，
   既有调用点零扰动。
6. **feishu 429 归一**：`cardkit_resp`/`reply_comment` 先判 HTTP 状态，429 归一为
   含「HTTP 429」标记的错误（此前直接 json() 解析非 JSON 体，重试识别不生效）。
7. 小项：`/ws use` 成功后失效 `/resume` 缓存（同 /cd）；codex 扫描
   `spawn_blocking`（防卡 tokio worker）；`/stop` 仅在 `router.has_pending` 时撤
   询问卡（防把已被正常回答的旧卡误 patch 成「已中断」）。

**验证**：新增 3 测试（flock ×3 场景合并为 3 个用例 + /stop 中断 /compact +
Err 路径持久化），全量 345 passed、fmt/clippy 绿。
---

## P5 第六批实现纪要（2026-08-19，路线图三大项）

### 1. dispatch.rs 巨石拆分 ✅

5238 行单文件拆为 `crates/core/src/dispatch/` 目录模块（后续所有迭代的摩擦来源
消除）：

| 文件 | 行数 | 内容 |
|---|---|---|
| `mod.rs` | ~730 | Dispatcher 状态与生命周期：构造 / run 主循环 / conv 锁与批处理 runner / merged_resume_list / reply 基元 / TaskBudgets |
| `commands/mod.rs` | ~190 | `handle()`：发现态引导、白名单门、21 命令 match 分派、普通消息批处理入口 |
| `commands/admin.rs` | ~340 | /allow /disallow /list /whoami /chat /config /perm（白名单/权限/配置） |
| `commands/session.rs` | ~480 | /new /switch /sessions /resume /compact /stop（会话生命周期） |
| `commands/misc.rs` | ~340 | /status /doctor /reconnect /cd /ws /img /help（状态/环境/媒体） |
| `round.rs` | ~470 | run_agent_round/run_round_inner：单轮 agent 状态机（流式收集/看门狗/落库） |
| `socket.rs` | ~400 | 权限审批 Unix socket（双行握手协议）+ peer_uid 鉴权 |
| `tests.rs` | ~2400 | 全部单测原样集中 |

手法：sed 行区间搬运（内容零转录）+ 子模块 `use super::*`（子模块可见父模块
私有项，字段/方法零可见性翻动）+ 命令臂逐个提取为 `cmd_x` 方法（签名按臂捕获
变量最小化）。跨模块方法 `pub(super)`/`pub(crate)`，`lib.rs` 的
`dispatch::{Dispatcher, TaskBudgets}` 导出路径不变。行为等价性由 351 项测试背书。

### 2. 孤儿流式卡片启动关流 ✅（store schema v6）

进程崩溃/被 kill 后，飞书流式卡片永远停在「生成中」（P5-11 只覆盖进程活着时
的终态 patch 失败）。闭环：

- **schema v6**：`live_cards(conv_id PK, platform, handle, updated_at)`——每 conv
  至多一张在飞卡片（轮次串行）。
- **登记**：`CardSession` 首帧 `send_card` 拿到真实句柄（None=降级纯文本，无卡片
  可滞留）即 upsert 登记；终态 patch **成功**才摘除。P5-11 降级路径（patch 失败
  纯文本补发）登记保留——卡片本身留给扫描关流。
- **扫描**：`imagent_core::sweep_live_cards`（Start 时调）：本平台孤儿卡片
  `update_card` patch 成「⏸️ imagent 已重启，本次生成被中断」Error 终态；异平台
  登记（平台已切换）无处 patch，作废删除；patch 失败保留登记下次启动再试。
  飞书 update_card 仅凭 handle（`card:`/`msg:` 前缀）即可 patch，重启后依然可关流。

### 3. feishu token 失效自愈 + SDK 路径 429 重试 ✅

- **token 失效错误码清缓存重试**：缓存 tenant_access_token 被服务端提前吊销
  （app_secret 轮换/后台强制失效）时，TTL 内重用旧值永远失败。新增
  `is_token_invalid_msg`（99991661-64/68/79 + "invalid access token" 文案；SDK
  ApiError Display 携带 raw_code，字符串识别两路径通用）+ 平台侧 `with_token`
  辅助：遇失效码 → 清缓存强制刷新 → 重试一次。覆盖 send_text（双路径分片）、
  send_media、send_card、update_card、审批卡片/撤卡、drain task 媒体下载。
- **SDK 路径 429 重试**：`retry_on_rate_limit!` 此前只包手写 HTTP 路径；现在
  send_text_msg / send_card_msg / patch_card / send_card_ref_msg / upload_image /
  send_image_msg 六个 SDK 函数全部包裹，识别串扩展 SDK Display 形态
  （"API错误 429/230020"、"业务错误 TooManyRequests"）。

**验证**：新增 6 测试（live_cards 登记/摘除/保留 + sweep 两场景 + v5→v6 迁移
回环 + 识别函数 ×2），全量 351 passed、fmt/clippy 绿。

**剩余待办**：三个 dependabot 失败 PR（clap MSRV / aes cargo-deny / tokio-tungstenite
API break）、真机冒烟（ACP e2e + 权限握手）、wecom 出站 ack 闭环、P5-14 per-conv
连接、飞书 fuzz target、mdBook 文档站同步。

---

## P5 第七批实现纪要（2026-08-19，维护波：dependabot 清零 + fuzz + 文档站）

### 1. dependabot 积压 8 个 PR 全部清零 ✅

- **#1-#5（GitHub Actions 升级）**：deploy-pages v5、upload-pages-artifact v5、
  codecov-action v7、upload-artifact v7、action-gh-release v3。gh 的 OAuth token
  无 workflow scope 无法 API 合并 → 本地应用等价变更（`66a8d26`），commit message
  自动关闭（#1 恰好已被仓库主在远端合并，rebase 去重）。
- **#6 tokio-tungstenite 0.24→0.29**：API break 是 `Message::Text` 载荷
  String→Utf8Bytes（0.26 起），wecom 客户端三处改 `Message::text()` 构造器；版本
  统一在 0.29 与 openlark SDK 对齐（0.30 会双版本栈）。
- **#7 clap →4.6.6**：lock 重解即可（PR 失败根因是过期分支的 fmt 漂移 + 旧 lock
  把 keyring dbus 后端拉进 MSRV job）。
- **#8 aes 0.8→0.9**：cipher 0.5 trait 改名（BlockEncrypt→BlockCipherEncrypt）+
  `from_mut_slice` 弃用改 TryFrom 定长数组两步走；webpki-roots 的
  CDLA-Permissive-2.0 此前已入 deny 清单。

三个 cargo PR 的 CI 失败共同根因：**分支基于 7 月旧 main**（新 rustfmt 对旧代码
的 fmt 差异 + Cargo.lock 漂移），非依赖本身问题——当前 main 重放全绿。

### 2. feishu 事件解析 fuzz target ✅

`fuzz_targets/feishu_event_parse.rs`：三类事件 payload（消息/审批按钮/云文档评论）
过 parse_message_event / parse_card_action_event / is_comment_event +
parse_comment_event（bot_open_id 有无两态），drain task 同路径。`proto` 模块转
pub 供 fuzz 直打。本地 60s 冒烟 162 万次执行零崩溃；周任务 fuzz.yml 加 300s 档。

### 3. mdBook 文档站同步 ✅

- 新增 `docs/ARCHITECTURE.md`（现状架构 as-built：crate 全景 / dispatch 拆分后
  结构 / 消息流水线 / schema v6 / 权限闭环 / 可靠性机制 / 安全模型），列为文档站
  首页。
- `SUMMARY.md` 修复：FEISHU_DESIGN.md 此前根本不在目录里。
- DESIGN.md / FEISHU_DESIGN.md 状态头修正为「历史快照/已实现」，指向
  ARCHITECTURE.md。本地 mdbook build 验证通过。
- `.gitignore` 补 `/book`、`/fuzz/corpus`、`/fuzz/artifacts`。

**剩余待办**：真机冒烟（ACP e2e + 权限握手 + 孤儿卡片 kill -9 验证）→ P5-14
per-conv 连接 + ACP Ask 接线；wecom ack 闭环；gemini /resume 不跟进。

---

# P6 迭代路线 —— 第二轮对标 lark-coding-agent-bridge：mention 基础设施与开箱体验

> 来源：2026-08-22 与 [lark-coding-agent-bridge](https://github.com/zarazhangrui/lark-coding-agent-bridge)
> 二次逐特性对比。P4 对标补齐了任务控制 / 批处理 / 交互深化；本轮缺口集中在
> **mention 基础设施、命令交互卡片、话题群、开箱体验（向导 / 服务自管理）、媒体出站**。
> 明确不跟：Lark 国际版（FEISHU_DESIGN §16 维持预留，无海外需求）。

## 总览

| # | 功能 | 优先级 | 状态 |
|---|---|---|---|
| 1 | mention 基础设施：群消息 @bot 客户端过滤 + @文本剥离 + require-@ 开关 | 高 | ✅ |
| 2 | `/allow @提及`：mention 解析换 open_id，免手打 ID | 高 | ✅ |
| 3 | 命令交互卡片：`/help` `/ws list` `/resume` 按钮卡（复用 card.action.trigger 回调） | 高 | ✅ |
| 4 | 话题群（thread）会话隔离：每个话题独立 conv/session | 高 | ✅ |
| 5 | `imagent setup` 首次运行向导（权限清单引导 + 凭据连通性校验 + 写配置） | 中 | ✅ |
| 6 | 服务自管理：`imagent service install/uninstall/status`（程序化装 launchd/systemd） | 中 | ✅ |
| 7 | 出站文件发送：`im/v1/files` 上传 + file 消息 + `/file <path>` 命令 | 中 | ✅ |
| 8 | `/cd` 目录安全校验：拒绝 `/`、home 根、系统目录、temp 根等过宽位置 | 低 | ✅ |
| 9 | 会话级 `/timeout [N|off|default]`（看门狗 per-conv 覆盖） | 低 | ✅ |

**实现顺序**：1+2（mention 基础设施，同一波）→ 3（复用按钮卡回调）→ 4（话题群）
→ 5、6（开箱体验）→ 7、8、9 按需收尾。

## 设计要点

### P6-1+2 mention 基础设施（本轮首波）

**问题**：群消息是否只收 @消息完全依赖飞书后台事件订阅 scope
（`im:message.group_at_msg` vs `im:message.group_msg`）；消息正文中的
`@_user_1` 占位不清理（proto.rs 对 at 元素「暂忽略」），agent 收到的 prompt
混有噪音；`/allow` 需要手打 open_id，用户体验差。

**方案**：
- proto 层解析 text 消息 content 中的 `@_user_N` 占位与 mentions 数组的对应关系，
  剥离 @bot 文本（bot open_id 匹配），其余 @提及转可读 `@名字` 文本；
- `InboundMessage` 增加 mentions 元数据（id → 名字），群聊 @bot 检测在客户端做；
- 新增 config `feishu.require_mention_in_group`（默认 true）：群消息未 @bot 时
  静默忽略（不回「权限拒绝」，与陌生人静默策略一致）；
- `/allow @名字`、`/disallow @名字`：从最近一条消息的 mentions 缓存解析 open_id，
  替代手打 ID（管理员门槛不变）。

### P6-3 命令交互卡片

**方案**：`/help` `/ws list` `/resume`（列表）返回按钮卡（V2 column_set→button，
与审批卡同构）；`card.action.trigger` 回调的 action.value 直接映射为命令文本
（如 `{"cmd":"/ws use main"}`），复用既有审批按钮回调解析路径。`/resume`
每项「接管」按钮 → `/resume <n>`。

### P6-4 话题群隔离

**方案**：消息事件带 root_id（话题根）时，conv key 从 `feishu:<oc_chat_id>`
升级为 `feishu:<oc_chat_id>:<root_id>`，session/批处理/白名单语义自动继承；
回复仍走原 chat_id（回复消息自动落回话题，无需 parent_id）。

### P6-5 setup 向导 / P6-6 服务自管理

**方案**：`imagent setup` 交互式 TUI：检查必配权限清单（CardKit 流式卡片 /
按钮回调 / 云文档评论所需 scope）→ 引导填 AppID/AppSecret → 调 tenant_token
校验连通性 → 引导事件订阅配置说明 → 写 config。QR 扫码建应用不适用飞书自建
应用（P4 已明确），向导止步于凭据校验。
`imagent service install` 程序化生成 launchd plist / systemd unit（模板已在
deploy/），注册当前二进制路径与 --profile；`uninstall`/`status` 对应管理。

---

## P6 实现纪要（2026-08-22，worktree 分支 `p6`）

九项全部交付；`cargo test --workspace` 全绿、clippy `-D warnings` 零告警、fmt 干净。

### 第一波：P6-1+2 mention 基础设施

- **类型层**：`InboundMessage` 新增 `mentions: Vec<Mention>`（user_id + name），
  全平台构造点补齐（feishu/ilink/wecom/测试/example）。
- **proto**（feishu）：`Message.mentions` 元数据解析（嵌套 `id.open_id` 与平铺
  `open_id/user_id` 双形态）；`apply_text_mentions` 纯函数——@bot 占位连同尾随
  空格剥离、@他人替换为 `@名字`、孤儿占位原样保留；`parse_post` 处理 at 节点
  （@bot 剔除、@他人渲染并进 mentions）；`group_mention_ok` 群过滤——bot id
  已知须 @bot、未知弱过滤（mentions 非空，同评论 P5-8 语义）。
- **platform**：`MentionPolicy`（config `feishu_require_mention_in_group`，默认
  true，重启生效）注入 `parse_message_event`；群消息懒取 bot open_id 与评论事件
  共用缓存（`ensure_bot_open_id` 重构去重）。
- **命令**：`/allow @名字` / `/disallow @名字`——`resolve_mention_target` 从
  **本条消息** mentions 反解（名字精确命中 → 唯一性兜底 → 歧义提示）；反解不出
  不误把 @字串当 id。
- fuzz target 同步：策略 × bot id 四组合全打。

### 第二波：P6-3 命令交互卡片

- **类型层**：`CardButton { label, command }`；`Platform::send_command_card`
  默认降级纯文本（`command_card_fallback_text`：标题 + 正文 + 可手打命令清单）。
- **飞书**：`render_command_card`（V2 column_set→button，每行 3 列自动换行），
  value 编码 `{"imagent_cmd": <command>, "conv": <conv>}`；`parse_card_action_event`
  扩展识别 `imagent_cmd`——只接受 `/` 开头（防伪造普通文本），映射为
  `text = <command>` 走**手打命令同路径**（鉴权/admin 门槛不豁免）。
- **接线**：`Dispatcher::reply_card`（平台失败再兜一层纯文本）；`/help` 六按钮、
  `/ws list` 每空间「使用」按钮、`/resume` 前 9 条「接管」按钮。

### 第三波：P6-4 话题群隔离

- conv 升级 `feishu:<chat_id>:<root_id>`（仅 group + root_id 为 `om_` 前缀——
  普通群回复只有 parent_id 不受影响）；session/批处理/conv 锁/审批路由自动
  per-topic 隔离。
- 发送分流：`thread_target_from_conv` 命中 → 文本/图片/文件全走
  `POST /im/v1/messages/{root_id}/reply`（SDK 无此 API，raw reqwest，同
  reply_comment 模式）落回原话题；话题群无流式卡片语义（`supports_streaming_card`
  false），审批/命令卡降级文本。
- `receive_target_from_conv` 取首段（chat/open id）；`Auth::is_chat_allowed`
  前缀规则——话题 conv（≥2 冒号）剥末段继承所属群授权，评论 conv 天然不命中。

### 第四波：P6-5 setup 向导 + P6-6 服务自管理

- `imagent setup`（src/setup.rs）：非 tty 直接拒绝；平台选择 → 飞书六步清单
  （建应用/开机器人/长连接订阅/三类事件/权限发布）→ 凭据录入 →
  tenant_access_token 连通性校验（真实 HTTP）→ 工作目录（过宽拒绝）→ 写
  config 0600。app_secret 不落盘，打印 export / service 注入指引。
- `imagent service install|uninstall|status`（src/service.rs）：macOS launchd
  用户代理（`~/Library/LaunchAgents/com.imagent[.<profile>].plist`，日志
  `~/.imagent/logs/daemon.log`）/ Linux systemd 用户单元（enable --now，journal）；
  注册 current_exe + `--profile`；安装时快照 `IMAGENT_FEISHU_APP_SECRET` 等
  凭据环境变量进服务定义（不快照则守护进程取不到）。

### 第五波：P6-7/8/9 收尾

- **出站文件**：`upload_file`（im/v1/files multipart）+ `send_file_msg`；
  `send_media` 按 `MediaRef.kind` 分流 image/file（话题群走 reply API）；
  `/file <path>` 命令（workdir 限定，同 /img）。
- **/cd 安全校验**：`imagent_core::validate_workdir`（黑名单条目与输入**双侧**
  canonicalize——macOS /etc→/private/etc 等 symlink 形态一致消解；拒 `/`、
  home 根、系统目录）接入 `/cd`、`/ws use`（存量宽泛目录也拦）与 setup 向导。
- **/timeout**：`idle_overrides` per-conv 覆盖（`/timeout <分钟>` / `off` /
  `default`），round.rs 两处消费点改 `idle_timeout_for`；纯进程内（会话级旋钮）。

### 遗留补齐（2026-08-22 第二批，三项全部交付）

- **require_mention IM 内热切换**：`Platform` trait 新增
  `require_mention_in_group()` / `set_require_mention_in_group()`（默认
  None/Err——平台无群聊 @ 语义时如实报告）；飞书侧策略从构造期定值改为
  `Arc<RwLock<MentionPolicy>>` 共享句柄，drain task 每消息现读。`/config
  require_mention on|off` 热切换对下一消息生效；进程内不落盘（重启回 config
  值，与 cot_detail 同姿态）。`/config` 展示含当前值（不支持的平台显示「本平台
  不支持」）。
- **话题群流式卡片**：managed 卡片实体无法在话题内引用（send_card_ref_msg 到
  chat 会开新话题），但 reply API 的 interactive 回执是普通消息——`send_card`
  话题分支走「reply 发 raw 卡 + `msg:<message_id>` 句柄」，后续整卡 im patch
  （体验同无 cardkit 权限的降级路径，打字机 managed 流式仍限普通会话）。
  `supports_streaming_card` 对话题放开；审批卡与命令卡在话题内同路发卡
  （`reply_message` 补返回 message_id 供句柄与 pending_asks 登记）。
- **setup WeCom 连通性探针**：企微无独立 HTTP token 接口，但 WS subscribe ack
  是真凭据校验面——`imagent_wecom::probe_credentials`（pub 导出）建连 → 发
  `aibot_subscribe` → 等 ack（errcode≠0 报 errmsg）→ 断开；setup 向导 WeCom
  分支接线，bot_id/secret 配错/吊销在安装期即暴露。

真机验证清单（合入前建议跑一遍）：话题群内发卡/流式 patch/审批按钮回调；
`/config require_mention off` 后未 @bot 群消息放行；setup 两个平台的校验步骤。

---

# P7 迭代路线 —— 对标收尾（A1-A5）：管理员管理 / 批量放行 / 陌生人提示 / 回复偏好 / profile 迁移

> 来源：2026-08-24 对 lark-coding-agent-bridge 的存量复核——P6 之后真正剩余的
> 五个「参考项目有、我们没有」的体验项。核心功能面已追平/反超，本轮全部为补齐。

## 总览

| # | 功能 | 状态 |
|---|---|---|
| A1 | `/admin [list\|add\|remove]`：管理员 IM 内动态管理（store schema v7） | ✅ |
| A2 | `/chat allow-all`：批量放行 bot 已加入的全部群（`Platform::list_joined_chats`） | ✅ |
| A3 | 陌生人被 @ 提示（config `stranger_mention_hint`，默认关；`InboundMessage.mentioned_bot`） | ✅ |
| A4 | 回复形态偏好 `/config reply_mode card\|text`（text = 不建卡走纯文本流） | ✅ |
| A5 | `imagent profile export\|import`（JSON；secret 默认脱敏，`--include-secrets --yes` 才带明文） | ✅ |

## 实现纪要（2026-08-24，worktree 分支 `p7`）

- **A1**：store schema **v7**（`admin_senders` 表，结构对齐 allowed_senders；线性迁移）；
  启动合并 config 种子 ∪ store 动态条目；`/admin add|remove` 内存 + store 双写 + 审计，
  即时生效。**防自锁**（测试暴露的真实缺陷）：向后兼容模式（列表空 = 全员可管）下
  设立首位管理员会立即收回操作者权限——空 → 非空转换时把操作者一并加入；
  `/admin remove` 不可移除自己；清空列表时显式警示语义回退。@提及形态复用
  P6 的 `resolve_mention_target`。
- **A2**：`Platform::list_joined_chats`（默认 Err，返回 JoinedChat{chat_id 为 conv
  形态, name}）；飞书走 SDK `ListChatsRequest` 分页聚合（每页 50 × 至多 10 页，
  200 群截断）；`/chat allow-all` 逐群双写 + 汇总回执 + 审计。
- **A3**：`InboundMessage.mentioned_bot`（飞书 proto：群消息 + bot id 已知 + mentions
  命中 bot 才为 true；p2p/弱过滤恒 false——提示宁可漏发不可误发）；config
  `stranger_mention_hint`（默认 false 完全静默防探测）；handle() 未过白名单分支
  据此回一句「管理员可 /chat allow」引导。
- **A4**：`ReplyMode`（Card 默认 / Text）config + `/config reply_mode` 热切换；
  round.rs 建卡闸门 `supports_streaming_card && reply_mode==Card`——text 模式
  下一轮起走纯文本流（含命令卡降级同路）。
- **A5**：`profile export`（config_toml 行级脱敏 wecom_secret + allowed_senders/
  chats/admins/命名空间表 → JSON；keyring 凭据与环境变量 secret 不随导出，回执
  明示）；`profile import`（导入为新 profile，写 config 0600 + 种子表；不允许
  导入为 default）。default profile 的导出经 `profile_state_dir` 映射到 ~/.imagent。

真机验证清单：`/chat allow-all` 真实翻页；`/admin add @名字` 群内流程；
reply_mode=text 一轮完整对话的纯文本流形态。

# P8 待办 —— 2026-09-03 真机校准遗留

## v1.18 提案（2026-09-03 评估，按 价值×成本 排序）

- **`/cron` 定时任务（头牌）**：IM 场景刚需——每日站会摘要 / 每周报告 / 定时
  提醒。实现面：cron 表达式解析 + store 持久化 + dispatcher 定时器到期把
  「指令文本」注入正常 inbound 管线（天然继承权限/会话域/审批链）。命令面：
  `/cron add <expr> <prompt>`、`/cron list`、`/cron rm <id>`。
- **群媒体「回复即定向」**：见下方准入缺口——回复 bot 消息的图片视为隐式
  @，手机端唯一可行路径。实现面小（proto 层 parent 归属判定）。
- **转向回执上卡**：流式卡 footer 追加「📥 已注入 N 条运行中消息」——
  2026-09-03 两次真机困惑（👀 表情太隐蔽，与排队/送达状态混淆）。

后排（按需排期）：`/export` 直出飞书云文档（修 234001 上传失败的根因替代）；
审批待办聚合卡（多审批挂起时合并通知）；入站 webhook（CI/监控告警推会话，
token 鉴权）。

- **群媒体准入缺口**：群聊纯图片/文件消息无法携带 @（手机端无富文本合成路径），
  `require_mention_in_group` 默认下媒体在群里永远进不来——媒体实际只在私聊可用。
  候选方案：**回复 bot 消息的图片视为隐式定向**（reply 的 parent 指向 bot 近期
  消息 → 免 @ 放行，语义对齐「点消息回复进话题」的定向意图）；或媒体单独豁免
  （风险：群里所有图片都进 agent，噪声大，不推荐）。
- **转向回执可见性**：steering 注入消息打 👀（OnIt）表情，真机反馈「太隐蔽、
  与普通送达状态混淆」。候选：流式卡 footer 追加「📥 已注入 N 条运行中消息」。
- **08-31 启动 panic 存档**：两次启动死在 "Cannot block the current thread from
  within a runtime"（std 锁在 runtime 内 block；当日后续启动已正常，未复现）——
  下次大改启动路径时留意 sync 锁的调用位置。
