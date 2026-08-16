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
