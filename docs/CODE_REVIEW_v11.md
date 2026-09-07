# Code Review v11 — v1.20 发布后复审（新功能集成缝隙专项）与修复

> **审查对象**：`imagent v1.20.0+` @ `4097cee`（main）。重点覆盖 v1.20 新代码：webhook 入站、ACP 窗口自学习、崩溃轮次恢复、/cron 补跑，以及此前轮次的盲区（ACP 连接生命周期、store 写幂等、feishu drain 管道、WS 存活性）。
> **审查方法**：三路并行（core 调度管线 / feishu 平台层 / claude+store 后端层）+ 本会话逐项核验（agent 报告全部交叉验证，1 项降级）。
> **总体结论**：v1.20 新功能集中暴露**集成缝隙类**缺陷——单点功能正确，但与 steering/启动时序/网络分区等既有机制的交互未校准。本批修复 26 项（4 类 P1 全修）。

## 一、P1（本批已修 ✅）

| # | 缺陷 | 位置 | 修复 |
|---|---|---|---|
| 1 | **/stop 拦截批次后 `break` 逃逸队列收尾 → 死队列**：take 留下的空 entry 悬挂后，本 conv 所有后续消息只入队不取批（⏳ 永久滞留），直到 /stop all 或重启。触发条件普通（批窗口期发一次 /stop 即中） | `dispatch/commands/mod.rs` runner 循环 | `break`→`continue`：走完 take→None→清 entry 的自然退出路径；/stop 后新到的消息照常执行 |
| 2 | **cron/webhook 合成消息被 steering 劫持**：定时任务文本经 `try_send` 灌进当轮无关任务的 stdin——独立轮次语义（可审计/可 /stop）破坏，且 steering 注入不落持久化、轮恰收尾即静默丢失；`catchup=all` 时 3 条补跑全灌同一轮 | `enqueue_or_become_runner` steering 分支 | `InboundMessage` 增 `no_steer` 位（serde default 兼容旧排队行），cron/webhook 合成时置位跳过 steering |
| 3 | **webhook server 先于 replay/recover 启动**：启动窗口内注入的消息落行后撞 `clear_queued_all`（ms 级窗口，实为**双执行**而非丢失——核验后降级 P2，但修法不变）；recover 也可能把抢跑轮次的 inflight 误判为崩溃轮 | `main.rs` 启动序列 | 抽 `startup_recovery()`（recover 先于 replay——扫描时不存在任何已起跑轮次，竞态归零），main 在 webhook accept 之前调用 |
| 4 | **ACP initialize 握手无超时 → 永久毒化**：子进程僵死时闭包永久挂起、槽位不释放，8 连接全占即全局拒绝服务，只能重启 | `claude/acp.rs` connect_with 闭包 | 60s 握手超时 → 闭包返回 Err → 连接销毁、`connect_error` 写入、run() 经 dead_reason 报真实原因 |
| 5 | **ACP LoadSession/NewSession/SessionStarted 阶段不被 cancel 覆盖**：dispatch drop run future 后任务在 session 建立阶段卡死 = 连接+子进程泄漏、conv 永久毒化 | 同上 turn 主循环 | load/new 各包 60s 超时（走既有 Err 分支：回真实原因+断连自愈）；SessionStarted 发送与 forward_update 对齐 30s 超时 |
| 6 | **store 写闭包非幂等 + BUSY 重放 → 成本双计**：append_run_stat / append_audit 的 INSERT+轮转 DELETE 是两条 autocommit，重放时 INSERT 再执行——`sender_cost_since` 虚高误触 per-sender 24h 预算（用户没超限却被拒） | `store.rs` | 两条写包进 `unchecked_transaction`（重放幂等）；delete_queued_rows 同款 |
| 7 | **send_media 全量读文件进内存**：50MB 检查在上传侧，`tokio::fs::read` 先行——agent 产出大文件即 OOM 尖峰 | `feishu/platform.rs` 两处 read | 读前 `metadata().len()` 预检（`media_size_violation`），超限可读错误 |
| 8 | **drain 串行循环内联 await 网络调用**：deny/过期/欢迎等提示直发（token 懒取+发送最坏 30s+）队头阻塞全平台入站；bot_open_id 取不到期间每条群消息重试一次网络 | `feishu/platform.rs` drain | 提示类全部 `spawn_drain_text` 解耦；ensure_bot_open_id 失败负缓存 60s |
| 9 | **WS run 循环无空闲看门狗**：连接静默黑洞（NAT 半开）时 `open()` 永不返回——事件断流且永不重连，只能人工 /reconnect | `feishu/client.rs` run() | 转发 task 记录活跃时刻 + select 第三支：30min 无事件强制丢弃 open future 重连 |

## 二、P2/P3（本批已修 ✅）

| # | 缺陷 | 修复 |
|---|---|---|
| 10 | webhook 停机仍 accept：drain 期间 inject 挂死在 tasks 锁、drain 后注入被无声取消（假 202） | `inject` 检查 shutdown → 503；webhook server `with_graceful_shutdown` 随停机关闭 |
| 11 | cron 任务不随会话失权清退：p2p conv + stranger_p2p_hint 时每分钟任务 = 每天 1440 条引导 DM | fire 前按 handle 同口径预检，失权自动停用 + 一次性通知；配套新增 `/cron enable\|disable` |
| 12 | 窗口自学习无护栏：异常小窗口（如 1）×比例截断 0 = 自动压缩静默关闭 | 学习值 clamp 到 `[AUTO_COMPACT_MIN, 50M]`，区间外丢弃+warn |
| 13 | 自动压缩失败无退避：持续失败时每个超阈轮都重试（两卡+一次完整 agent 跑） | per-conv 失败退避 1h（成功即清；手动 /compact 不受限） |
| 14 | 排队上限告警逐条回发：洪泛源（token 泄漏/CI 重试风暴）刷屏+可能触发平台频控 | per-conv 时间窗去重（同 budget_notice_last 手法，1h 一次） |
| 15 | run_stats 无 (sender,ts) 索引：配置日限额后每条入站消息 SUM 全表扫 | v13 迁移加索引 |
| 16 | argv 无长度预检：prompt 单 argv 超 MAX_ARG_STRLEN（128KB）时裸 E2BIG | spawn 层统一 100KB 预检，可读错误（gemini 侧守卫统一收编） |
| 17 | 非 control 路径 `child.wait()` 无超时：`agent_timeout=0 且 idle=0` 组合下永久挂起 | 10s wait 超时 + kill 兜底 |
| 18 | ACP try_lock 失败静默丢弃 agent_text 段/整条 UsageUpdate（cost 基线缺口+窗口学习丢失） | 改 lock().await（持锁均微秒级，零丢失） |
| 19 | env 白名单缺 ANTHROPIC_AUTH_TOKEN/代理/证书链 | AGENT_RUNTIME_ENV 增 9 键（HTTP(S)_PROXY/NO_PROXY/SSL_CERT_*）；ACP+CLI 两路径同步；CLI 另增 GH_TOKEN/GITHUB_TOKEN（/cron 拉 gh 查 CI 场景） |
| 20 | housekeeping 注释承诺的「仍超限才整体清空」未实现：评论会话数超限后表永不收缩 | 补兜底 clear（评论锚丢失是硬失败，但无界增长更糟） |
| 21 | card_seqs/card_footers 无上限：Failed/HandleLost 之外路径的条目永久滞留 | 写入侧 2048 粗上限兜底（seq 有 300317 自愈） |
| 22 | token 刷新失败无负缓存：端点故障期间调用方在 single-flight 门上线性放大等待 | 失败结果 5s 负缓存 |
| 23 | WS Ok() 无条件重置退避：握手即断的异常形态形成 ~1s 重连循环 | Ok 分支同样要求 HEALTHY_CONN_MIN_LIFETIME |
| 24 | BOT_SENT_MSGS `.expect("锁中毒")`：持锁 panic 后 drain 每事件级联 panic | `unwrap_or_else(|e| e.into_inner())` 恢复 |
| 25 | inflight 清除失败被 `let _ =` 吞：已完成轮次下次启动被误判崩溃（副作用 prompt 重复执行风险） | 失败 warn 留痕 |
| 26 | 崩溃恢复无条件覆盖 last_prompt：可能顶掉更近的真实失败轮 /retry 兜底 | 已有 last_prompt（at 更近）时不覆盖；cron extra_missed 计数移出 one/off 档（免白扫） |

**回归测试新增**：`no_steer_message_queues_instead_of_steering`（合成消息独立轮次）、`stop_interception_does_not_strand_queue`（死队列回归锚）、窗口学习护栏断言；`pending_queue_cap_warns_and_drops` 更新为去重后语义。

## 三、确认无恙（本轮专项核查）

- 锁序：`queues → {running, queued_hints}` 单向，无持锁跨 await、无死锁路径。
- 外部输入可触发的 panic：三路全量扫描未发现（`merge_batch` expect / cron parts / mentions[0] 等均有前置不变量）。
- SQL 全参数化；schema 迁移链 v13 幂等；ACP JSON 解析层容错完整（read_line_capped 半行/超长/非 UTF8 处理正确）。

## 四、遗留（有意不修，记录在案）

| # | 项 | 理由 |
|---|---|---|
| L1 | ACP JSON spec 形态（`IMAGENT_ACP_COMMAND` 为 JSON）绕过 env 消毒 | 需重建 spec env 语义（args 含空白时 shell_words 再切分破形），破坏自定义 spec 用户；注释已声明该取舍，待上游 env_clear 支持 |
| L2 | runner 任务 panic 无兜底（同死队列形态） | panic 路径已审计最小化；catch_unwind + entry 自愈需引入 panic 状态机，收益/复杂度不成比例，待真机出现首例再议 |
| L3 | steering try_send 成功但轮恰收尾 → 消息滞留 channel 随 drop 丢失 | 轮末「未消费 steer 补转排队」需改 backend 收尾协议；发生面窄（轮收尾与注入的微秒级交叠） |
| L4 | 排队批次 at-most-once（轮中崩溃不重放） | 既有文档化语义；at-least-once 需副作用幂等前提，独立迭代 |
| L5 | webhook RPS 限制 + HMAC 验签 | 告警去重+shutdown 拒绝已消解读面；防护套件作为功能迭代（GitHub 原生 payload + X-Hub-Signature-256 一并做） |
| L6 | `msg:` 句柄流式帧带 retry_on_rate_limit（与 managed 路径不一致，限流时阻塞 flush 最多 3.5s） | 降级路径真机行为未校准，盲改有回归风险 |
| L7 | canUseTool 审批在读循环内联 await（stdout 64KB 管道可能写满） | M3 注释自认的取舍；审批并发化需重构 ctrl_tx 通道语义 |
| L8 | v9/v10 的 ALTER TABLE 在「列已存在但 user_version 停旧」的手工恢复库上失败 | 仅影响手工修复过的库，可加 PRAGMA 预检，优先级低 |

## 五、高价值迭代候选（下一批功能，按性价比排序）

1. **webhook 防护套件 + GitHub 原生事件**：RPS 令牌桶 + `X-Hub-Signature-256` 验签 + GitHub payload 解析（CI 失败 → 自动格式化为可读注入文本）。基建（axum state、token 表）全在，是 v1.20 头牌的自然延伸。
2. **发送侧 outbox 持久化重试**：所有 best-effort 仅 warn 的丢失面（deny 提示、终态补发、限流重试耗尽后的用户可见消息）统一落盘重发 + per-conv 令牌桶主动预算 QPS——把「被动挨 429 再退避」翻转为「主动不触发 429」。
3. **codex/gemini 能力对齐**：codex `-m`/gemini `--model` 的 /model（现成参数）、codex todo_list 事件→TodoList 面板、gemini 本地会话扫描+幽灵预检——三条纯增量。
4. **入站管道可观测性**：drain 单事件时延、泵队列深度、payload channel 积压、token 刷新等待人数四项 metrics——本批 #8/#22/#9 三类停摆发生前都会先在这些指标显形。
5. **conv_states 精确 LRU**：entry 加 touch 时间戳替代粗上限+选择性驱逐（顺带收编 #20 的兜底语义）。
