# Code Review v13 — 全仓对抗性深审 + 吞吐面/威胁模型补审

> ✅ **交付状态（2026-09-27）**：本报告迭代路线**全部落地**——修复批
> （4 P1 + 6 P2）与威胁模型文档随 v1.27.0 发布；安全批/产品批/运维批/
> 还债批随 v1.28.0 发布（详见 CHANGELOG 对应版本）。P3 项除「队列重放
> clear 后丢批窗口」「/img TOCTOU」（遗留表内已记档不修理由）外全部
> 修复；RoundSlot 显式状态机为 ConvState 单表后的可选后续项。

> **审查对象**：`imagent v1.26.0` + 工作区（自动压缩默认关闭批）。四路并行对抗深审
> （core 调度 / feishu 平台 / 四后端+MCP / 横切层+安全模型），最重 8 项由主会话
> 复核调用链确认。基线 `cargo test --workspace` 全绿。
> **总体结论**：12 轮 review 后行级 bug 确已收敛；本轮发现集中在四类系统性问题——
> ① 跨层交互缝隙（单层都对、组合起来错）；② 安全校验矩阵的**反向缝隙**（只做了
> `ask × 非FullLoop` 一个象限）；③ 吞吐面（消费端背压/全局并发，12 轮均为盲区）；
> ④ 威胁模型缺「agent = 同 uid 宿主用户」一章。

## 一、P1（4 项，均复核确认）

| # | 缺陷 | 位置 | 失败场景 | 修复方向 |
|---|---|---|---|---|
| 1 | **卡片节流「睡到窗口」把 chunk 消费钉死在 ~2 条/秒，背压传导到 ACP 通道** | `card_session.rs:327-350` × `round.rs:554-557` × `acp.rs:1125-1133` | feishu+claude-acp 旗舰组合：ACP `AgentMessageChunk` 是 delta 级粒度，每消费一条先睡 ~500ms → 数千 delta 的长轮纯排管道就要 N×500ms，可拖到逼近 `agent_timeout` 被总超时杀掉；消费方停摆 >30s 时 delta 被 30s 超时丢弃。CLI 后端 chunk 为消息级故从未暴露 | patch 移出消费路径：CardSession 内常驻 patcher 任务（chunk 到达只置脏+notify，patcher 统一 500ms 节流 flush，finalize 时 join） |
| 2 | **`md_element_anchored` 绕过 v1.26 转义收口——`<at>` 注入第 4 洞** | `card.rs:80-94` ← `card.rs:670,678`（task_digest=用户原始 prompt 前 60 字） | 用户 prompt 含 `<at id="ou_受害者"></at>`，managed 流式卡**初始帧**以 bot 身份 @ 任意租户用户（patch 路径有转义、初始帧没有——同一 md_body 组件不一致）；prompt 含邮箱时初始卡被租户审计整卡拒收 → 永久降级 raw 卡片路径且无日志 | `md_element_anchored` 内建 `escape_lt(&mask_emails(...))`（仅 2 处调用，行为等价零风险）；`sender_anchor_line` 加注释声明其为唯一合法 `<at>` 构造点 |
| 3 | **ACP 后端完全忽略 `allowed_tools` 白名单，仅 debug log** | `acp.rs:910-919` | `allowed_tools=["Read"]` + `permission_mode="off"`：claude-cli 下 `--allowedTools` 收敛生效，切 claude-acp（同为 FullLoop，能力校验放行）后 agent 实持**全量工具**且无审批兜底——两个 FullLoop 后端之间安全配置静默分叉 | ①启动/SIGHUP 时非全量 allowlist × acp 后端 warn；②补 `supports_tool_allowlist()` 能力位，dispatcher 显式拒绝或告警；③探索 `--settings`/env 注入映射 |
| 4 | **webhook 无重放防护，且非 loopback 绑定无 fail-closed（与 metrics 双标）** | `main.rs:1434-1457`（HMAC 只签 body）× `main.rs:744-766`（无 `validate_metrics_bind` 同款校验） | 公网/隧道部署下截获一条合法签名请求 → 无限重放（默认 rps=10 允许 600 次/min），每次驱动一整轮 agent 烧预算；metrics 侧非 loopback 无 token 拒启，更高危的 webhook 面反而只靠注释劝告 | ①自约定 `X-Imagent-Timestamp` + ±5min 窗 + 签名覆盖 `ts.body`（opt-in per-webhook）；②非 loopback 且条目全无 secret → 拒绝启动 |

## 二、P2（9 项）

| # | 缺陷 | 位置 | 要点 |
|---|---|---|---|
| 5 | 命令类按钮私聊免检 sender——转发卡片=跨会话注入 | `proto.rs:483-494` vs `467-480` | 询问类已全形态校验（注释自己论证过「卡片可被转发到任意会话」），命令类却私聊免检。A 转发私聊卡给白名单用户 B → B 点击 = 在 A 的私聊会话执行 /stop /new /again（按钮回调是 B 触达该 conv 的唯一通道） |
| 6 | `cancel_all` 把 cancel 文案当「用户回答」回写终端 agent | `permission.rs:561-580` × `socket.rs:556-606` | conv 的 IM 轮被 /stop/失败 → 挂着的 `ask_via_im` 终端提问收到「用户回答」= 字符串 `cancelled（任务被 /stop 中断…）`，agent 据此继续推理。修复：cancel 带哨兵，ask 分支识别后回 Err |
| 7 | recv 主循环内联 await 平台/存储调用，一处慢全平台入站停摆 | `mod.rs:1395-1447` | 审批路由命中后 append_audit/resolve_permission_ask/D2 提示全内联——飞书 429/token 刷新（最坏 30s+）期间所有 conv 的入站、/stop 排队停摆（v11 在 feishu drain 修过同族，core 主循环是残余）。修复：决策已送达 oneshot，收尾全 spawn 化 |
| 8 | 审批发起者锚定未覆盖文本路径：群内白名单成员打 "y" 仍可代批 | `permission.rs:479-530` | v1.23 锚定只做了按钮/表情路径。修复：pending 存发起者，群 conv 文本回复校验同锚定（私聊免检） |
| 9 | 正常完成路径 stderr 无超时：孙进程持有 stderr → run 挂到看门狗 | `backend_common.rs:1056-1061` | wait Ok → disarm 后裸 `stderr_handle.await`——孙进程继承 stderr 写端不退出 → 挂到 idle_timeout（默认 20min）+ fd 泄漏。cancel/超时路径都有 killpg 兜底，唯独 happy path 漏了。修复：`timeout(5s, ...)` |
| 10 | 全局零并发护栏：N 个 conv 在飞 = N 个 agent 子进程 | `mod.rs`（无 Semaphore；ACP 8 槽位隐式封顶、CLI 无界） | 多群部署/cron 齐点/webhook 风暴 → 内存/CPU/API 配额同炸。修复：`max_concurrent_rounds` 信号量 + 排队水位 metric + /status 可见 |
| 11 | 非卡片平台对 ACP delta 逐条 send_text 刷屏 | `round.rs:558-565` | claude-acp × wecom/ilink：一段长回答=数百条几字符消息，打爆 QPS。修复：文本平台 400ms 合帧缓冲 |
| 12 | codex `session_exists` 每轮消息全目录扫描 | `codex/sessions.rs:106-125` × `backend.rs:95-114` | 数月重度使用后每条消息触发数千 rollout 文件遍历；文件名含 thread uuid 可 O(1) 定位 |
| 13 | ACP 权限审批 handler 阻塞 SDK dispatch loop | `acp.rs:517-534`（内联 await 审批最长 900s；SDK 源码明示 callback 阻塞后续消息） | 一轮多审批强制串行、通知积压。修复：handler 内 `cx.spawn` 化（SDK 正规逃生门），与 v11-L7 并列记档 |

## 三、P3（摘要）

- **core**：stop-拦截批次表情恒挂 👀（`round.rs:236-283`）；/compact 被 /stop 打断裸泄 JoinError（`session.rs:649`）；崩溃恢复 prompt 双注入摘要（`round.rs:249` 存注入后 prompt）；话题群 conv 无限基数下 `pending_hint_last`/`resume_cache`/`idle_overrides`/`compact_fail_last` 慢泄漏（`mod.rs:554-579`）；看门狗豁免预算=单次 ask 超时，多次慢审批总和超预算会被杀（`round.rs:426-449`）；cron drain 期 spawn 竞态（`cron.rs:611`）；`/perm list`/`revoke` 无 admin 门槛（低危记录在案）
- **feishu**：fuzz 只盖 4 个解析函数、drain 链上 11 个裸奔（含新 `card_text_transcript` 递归）；单事件最坏 14 次完整 JSON 反序列化（应一次解析按 event_type 分派）；SDK 错误串匹配承载全部自愈语义（Display 格式隐性契约）；reply→create 双跳幂等 uuid 不共享→超时重发收两条；`card.action.trigger` 全量 payload 进 debug 日志（含审批命令全文）；降级卡终止按钮恒无发起者编码；撤回 notify_conv 回退非法 conv（`proto.rs:1180`）；云文档评论会话=全体协作者共享（信任边界未文档化）
- **backend**：MCP 子进程 stdin 读行无上限（三处口径不一致）；gemini `delta:true` 碎化重复（`stream.rs:76-86`）；`/perm deny` 对 codex/gemini 无执行点无提示；`read_line_capped` core 内两份逐字重复；claude/codex 会话扫描器结构性重复；同 mtime 排序不稳定
- **横切**：macOS daemon.log 无限增长无轮转；`/img` `/file` TOCTOU；队列重放 clear 后崩溃丢批窗口；**文档漂移三连**（ARCHITECTURE 写 schema v9 实为 v15 且缺 cron/webhook/outbox/排队持久化/崩溃恢复/媒体 GC 六子系统、README 写 v1.19.x/schema v12、CLAUDE.md 写 v1.9.x/review v7；README 命令表 `/export` 重复两行）；`deploy/launchd` 静态模板与 `service install` 两套分叉（模板无 `--platform`，README 恰好警告过这个坑）；仓库根目录 tracked 垃圾文件 `300s，中间无任何`；rusqlite 0.31 锁 bundled SQLite ~3.45（2024 初），cargo-audit 对 C 代码 CVE 失明；login/allow 子命令不持实例锁；媒体 7 天 GC 与排队无 TTL 错位

## 四、系统性设计问题

1. **威胁模型缺一章**：审批闭环只约束「claude 发起的工具调用」，不约束「同 uid 进程的读写能力」。提示注入后的 agent 用默认免审 Read/Grep 即可：读 `~/.imagent/imagent.db`（明文回退形态下含 bot_token/白名单/全部会话 prompt）→ 读 `/proc/<pid>/environ` 拿 APP_SECRET → **写 allowed_senders 给自己开后门**。个人部署可接受；多人群白名单部署下任一成员的提示注入即可能拿到 bot 凭据——任何文档都没写这个边界。修复：SECURITY.md 增「威胁模型与边界」节（必做）+ claude 注入 per-run deny 规则把 `~/.imagent` 移出 agent 视野 + opt-in 沙箱档。
2. **调度核结构债**：`/stop` 三元机制（running 句柄+stop_requested 水位/TTL+conv 锁 try_lock）是历轮竞态补丁沉积物；8 个按 conv 键控 map 分散加锁靠注释维持锁序；feishu 与 core 双份 pending 登记。方向：ConvState 单表合并 + 显式 RoundSlot 状态机。
3. **运维故事缺口**：无 backup（`VACUUM INTO` 快照+keyring 说明）；macOS 日志无轮转；多用户共享 default_workdir 的后果零文档（A 读得到 B 的中间产物、两 agent 并发编辑同 repo）。
4. **能力协商需补反向矩阵**：`allowed_tools × ACP`、`deny 档 × codex/gemini`、`非 loopback × 无 secret` 全没堵。方向：PermissionCapability 扩成显式能力面（tool_allowlist/permission_hook/sandbox），启动+SIGHUP 全矩阵校验+/doctor 可见。

## 五、迭代路线（本报告的落地批次）

| 批次 | 内容 |
|---|---|
| **v1.27.0 修复批** | P1×4 + P2-5/6/7/9/10/11（注入第4洞、流式管道解耦、webhook 重放、cancel×ask、recv 停摆、stderr 挂起、全局并发护栏、文本合帧、命令按钮校验）+ 威胁模型文档 |
| **v1.28.0 安全批** | agent 最小权限化（deny `~/.imagent`）、/doctor 安全自检、审批文本锚定（P2-8）、backup、日志轮转 |
| **v1.29+ 产品批** | ACP 生态泛化（审批闭环 2→N 后端，最大架构杠杆）、群聊上下文注入（产品最大单点）、并行任务一等公民、可观测三指标、Bitable 数据面、评论链路补全 |
| **还债批** | ConvState/RoundSlot 收敛、platform.rs 拆分、文档单一事实源+CI 断言、rusqlite 升级、deploy/ 合流、read_line_capped/会话扫描器去重、gemini delta、fuzz 面补齐 |

**维持 v12 的不做判断**（per-sender 会话隔离、RAG 溯源、自动跨会话记忆、云文档双向编辑），另冻结 wecom（无真机流量验证）。

## 遗留（记录在案，不修的理由）

| 项 | 理由 |
|---|---|
| `/img` TOCTOU | 同 uid agent 本可直读（见威胁模型章），边际价值低；要修则 O_NOFOLLOW+fstat |
| 队列重放 clear 后丢批窗口 | at-most-once 同族已记档（v11-L4）；改 exactly-once 需逐条 ack，成本不成比例 |
| 群内未 @bot 斜杠免检噪音 | 需跨 crate 命令表共享，独立小迭代再议 |
| ACP 通知 handler 持 state clone 跨 30s | 影响极小，记录在案 |
| GH_TOKEN 透传扩大权限面 | /cron 查 CI 既定决策（v12-L1 维持） |
