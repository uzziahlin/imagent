# Code Review v12 — v1.21/v1.22 新代码对抗复审 + 盲区补审 + 体验/产品挖掘

> **审查对象**：`imagent v1.22.0` @ `7301039`。三路并行：① v1.20.0..HEAD 全量 delta 对抗复审（含对 openlark SDK 源码交叉验证）；② 盲区补审（wecom/ilink/setup/service/card.rs/mcp/permission/CLI）；③ 交互体验与产品完整性。
> **总体结论**：新代码暴露一个模式性问题——**三处「修复/设计意图只写在注释里，没落成断言」导致的方向性缺陷**（本批 P1 即其一）。盲区文件防御密度显著低于主链路（card.rs 第三次出现同类注入收口遗漏）。本批修复 24 项。

## 一、P1（本批已修 ✅）

| # | 缺陷 | 修复 |
|---|---|---|
| 1 | **崩溃恢复 last_prompt 保护方向反了**（v1.21 引入）：实现写成「存在 last_prompt 即不覆盖」，从未比较时间戳——轮次串行下崩溃残留 inflight **必然新于**先前失败轮的 last_prompt，等于在「前一轮曾失败」的常见场景整体禁用崩溃恢复，且通知仍引导 /retry 到**更旧**的 prompt（副作用指令重复执行风险） | 解析 inflight 自带 at 真比较：现存严格更新（陈旧 inflight 清理失败残留）才保留；新增回归锚 `crashed_round_recovery_prefers_newer_inflight`（注释里的时序不变量翻成断言——本批教训的直接落实） |
| 2 | **card.rs `<at>` 语义注入收口遗漏**（第三次同类）：多题卡题面/描述（agent 生成，可引不可信网页）、/resume 表格「内容」列（**用户输入原文**）、未配对行兜底、heading 首行、已记录选择回显（用户自由输入）、询问终态 tool_name（可破坏 code span）——六出口未过 escape_lt，bot 名义 @ 任意租户用户 + 钓书链接伪装 | 六出口全部收口（escape_lt + mask_emails / sanitize_inline）；建议后续把收口下沉到 markdown 元素构造处（结构化防再犯） |

## 二、P2（本批已修 ✅）

| # | 缺陷 | 修复 |
|---|---|---|
| 3 | **WS 看门狗有害且冗余**（v1.22 引入）：核实 openlark SDK 自带 WS 层心跳检查（`heartbeat_timeout=120s`，入站 Ping 刷新 last_activity，超时 begin_close）——静默黑洞 120s 内已被 SDK 处理；而看门狗判据是**业务 payload**，低流量部署 30min 无事件是常态 → 健康连接每半小时被误杀一次 | 撤看门狗 + 转发 task（保留 Ok 分支退避条件修正）；注释记录 SDK 依据 |
| 4 | `/cron enable` 不重置 next_run：停用期 next_run 停在过去，enable 只翻位 → 下个 tick 立即「补跑」（One 档一轮、All 档 3 轮完整 agent 轮次），与回复承诺的「下次时刻」直接矛盾 | enable 成功后 `bump_cron_job` 重排 next_run 到下个未来时刻（停用期槽不补） |
| 5 | `/cron enable` 权限口径与 fire 侧不一致：判命令发送者而非任务归属 → 已 deny 会话里 enable 他人任务 → enable 通过 → 下 tick 又自动停用，来回通知 | 改判 `job.sender / job.conv`（与 fire 侧失权预检同源） |
| 6 | `/resume` 接管会话不清 session allow-set：「本次会话始终允许」跨会话延续，与「换会话不继承授权」语义矛盾 | 接管成功路径补 `clear_session_allows`（与 /new、/stop 硬停同款） |
| 7 | `token_refresh_waiters` 指标在 future 取消下单调泄漏（裸 inc/dec，取消路径 dec 不执行）→ 持续高水位假故障 | Drop guard 化（DrainEventTimer 同款手法） |
| 8 | github_event_text 字段缺失产出空壳文本（「⏹️ GitHub Actions：「」（ 分支）…」）驱动一整轮 agent 却无信息量；push 提交首行未截断（64KB body 内可注入超长 prompt） | workflow_run/push/issues/PR 必要字段缺失即 Skip；commit 首行截断 200 字符 |
| 9 | wecom subscribe ack 判定 `errcode.unwrap_or(0)==0`：首个非 ack 帧即伪装认证通过 → 真失败时静默无入站 | 首帧必须携带 `errcode` 字段且为 0 |
| 10 | ilink 游标**读**失败被静默吞（`unwrap_or(None)` → 带空游标请求，服务端可能按重置语义大批重放；DB 故障跨 dedup 窗后恢复甚至重复驱动 agent） | 读失败即 Err——recv 退避接管，宁停不空（与写侧对称） |
| 11 | setup workdir 录入 stdin EOF 死循环（忙转不退出）；覆盖已有配置无备份（手工调好的白名单/管理员全清零） | prompt EOF → Err 优雅退出；覆盖前备份 `config.toml.bak.<epoch>` 并提示 |
| 12 | service plist/unit 模板无转义：secret 含 `&`/`<` 产生非法 XML（launchd 加载失败）；systemd 值含 `"`/换行可注入额外指令 | xml_escape（plist）/ unit_escape（systemd）全内插点覆盖 |

## 三、P3（本批已修 ✅）

13. outbox_mark_failed 的 UPDATE→SELECT→DELETE 包单事务（BUSY 重放 attempts 双跳）
14. `/health` 接入 `outbox_pending`（outbox 深度——泵健康的唯一窗口，此前 doc 声称接入实无调用方）
15. ACP SessionStarted 30s 超时补 warn（sid 不落库 = 崩溃续接兜底缺失，此前零日志）
16. inject 拿 tasks 锁后复查 shutdown（check-then-act 窗口内 spawn 的任务会被 drain 后无声取消）
17. 旧版（<v1.21）落盘的 cron/webhook 排队行重放时按 `webhook:` 前缀补判 no_steer
18. 发送令牌桶覆盖面补齐：send_media（最重的上传请求）与 send_command_card 入口
19. feishu_send_rps 配置校验 [0,100]（负值/NaN 怪路径）
20. ConvState LRU：note_card_tail（发送侧活跃）补 touch + drain 侧冗余 touch 清理 + 孤儿 doc 注释清理
21. permission 路由 ask_counters/last_decision_at 随 clear_session_allows 一并清理（缓慢泄漏）
22. ilink persist_media 改 OpenOptions 原子 0600 创建（消 umask 0644 暴露窗口）
23. ilink typing_tickets 过期清理 + 1024 粗上限（此前只判失效不删除）
24. ilink 出站媒体 50MB 读前预检（与入站上限对称，防 OOM 尖峰）

**测试**：680 通过（新增 last_prompt 方向回归锚）；clippy 零警告。

## 四、遗留（记录在案，不修的理由）

| # | 项 | 理由 |
|---|---|---|
| L1 | GH_TOKEN/GITHUB_TOKEN 全量透传扩大子进程权限面 | /cron 查 CI 的既定决策；agent 经 Bash 拿到 gh 凭据的威胁模型 = agent 本身（IM 审批管 Bash 执行）；可后续加 config 开关 |
| L2 | outbox 同 conv 多条乱序（退避后排到队尾）| 提示类消息弱序可接受；ORDER BY next_try,id 是一行改进，待有真实数据再动 |
| L3 | ilink send_lock 在重试/熔断期持锁阻塞全平台出站（最长 ~40s） | 重构需动 breaker 语义，次级平台风险/收益不划算；主用 ilink 时再议 |
| L4 | wecom 投入判断：**冻结在 MVP** | 三平台最薄一层（纯 WS 透传/单聊文本/无媒体），已知问题都指向无真机流量验证；修掉 P2-9 后转维护 |
| L5 | MCP stdio server 单线程串行（ask 阻塞时后续请求排队）| claude 审批回调本身串行，当前可用；mcp-ask 挂终端 agent 并发场景出现再议 |
| L6 | card.rs「调用点转义」模式已三次失效 | 根治需把收口下沉到 markdown 元素构造处（`md_element()`）——涉及 28 处调用点的行为等价改造，独立小迭代做 |

## 五、产品方向 Top5（下批迭代候选，按价值排序）

1. **会话记忆可辨认**（★★★★★）：`session_history` 加 first_prompt 列（IM 会话在 /resume 里只剩 id 前缀无法辨认——纯 IM 用户是最主流群体）；`/export [n]` 导历史会话；`/sessions` 表格化。schema v15 一条 ALTER + 3 个命令局部改动，低风险快赢。
2. **审批等待可视化 + 流式卡心跳**（★★★★★）：审批等待期卡片完全冻结（秒数不走、阶段不变，体感「卡死」）——CardPhase 增 WaitingApproval + permission hook 双向通知 + Running 态 30s 心跳 patch。长任务体验的最大单点。
3. **allow-set 可见性**（★★★★☆）：`/perm list` 展示会话已放行工具、`/perm revoke <tool>` 单项撤销、「始终允许」按钮挂二次确认（持续授权比单次决策重，基建 cb_button_confirm 已有）。
4. **说话人归属贯通**（★★★★☆）：steering 注入无说话人标注（排队的有、转向的没有）；卡片发起者随「最近 sender」漂移（A 的审批卡可能突然点不了）；merge_batch 用裸 open_id。InboundMessage 增 sender_name + 发起者锚定轮次首条消息。
5. **指令复用**（★★★☆☆）：成功轮 prompt 落库 + `/again` 命令 + 终态卡「再跑一次」按钮（与 /retry 对称）；/help 动态列出 shortcuts（已建成的基建零发现性）。

**不建议做**：per-sender 会话隔离（与群协作心智相反，等于重写调度核）、RAG 式回复溯源（黑盒 CLI 无引用结构，假溯源比无溯源危险）、自动跨会话记忆（污染不可审计，显式 /remember → AGENTS.md 才是对的形态）、云文档双向编辑（权限面陡增）、cron 之上的晨报模板系统（cron+shortcuts 已能表达）。
