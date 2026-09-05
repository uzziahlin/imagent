# Code Review v10 — 发布后复审（新代码自审 + 盲区补审）与迭代路线

> **审查对象**：`imagent v1.18.0` @ `1a53f7d`（main，含 4ac0146..1a53f7d 八个提交：30 项修复批次、v9 清零、intake 解耦、排队持久化、ConvState/update_card 重构、比例档阈值）。
> **审查方法**：三路并行——① 本会话新增代码的对抗性自审（dedup/housekeeping/泵/排队持久化/cron/ACP 基线/比例档）；② 盲区补审（store 全量、main/service/setup 组装、mcp/metrics/instance/auth）；③ 次级平台（wecom/ilink/gemini/codex）+ 功能挖掘。真机冒烟（A1-A6/C3/M5）已先行通过。
> **总体结论**：既有主线（feishu×claude）稳固；**新增代码自审抓出 4 个 P1**（泵丢消息、排队清行窗口、exit 0、ACP 基线随回收丢失）——均为集成缝隙类，单测覆盖不到；盲区补审贡献 `switch_named_session` 丢列等服务层 bug。本批修复 X 项（见 §三），余项入 §四 路线。

## 一、缺陷清单（本批已修 ✅ / 遗留 ⬜）

| # | 级别 | 状态 | 缺陷 | 位置 |
|---|---|---|---|---|
| 1 | 🔴 | ✅ | 飞书 per-conv 泵丢消息：①泵空闲退出与 send 竞态，缓冲作业随 rx drop 丢失；②SendError（含被退回 job）被 `let _ =` 丢弃，可检测的丢失无兜底。dedup 已消费 id → 永久丢 | `feishu/platform.rs` conv_pump/pump_send |
| 2 | 🔴 | ✅ | 排队持久化并发清行：取批 `clear_queued` 按整 conv DELETE，连带删掉「取批释放锁后、DELETE 前」并发入队新消息的行（内存排队、DB 无行=崩溃即丢）；None 分支（runner 首条）完全绕过持久化 | `dispatch/mod.rs` + `store.rs` |
| 3 | 🔴 | ✅ | dispatcher `Err` 退出码 0：systemd `Restart=on-failure` 永不拉起（bot 静默下线）；launchd 侧 session-expired 热循环重启 | `src/main.rs` |
| 4 | 🟠 | ✅ | ACP cost 基线挂连接级：空闲回收（600s）重建即丢，下轮把会话累计全额记单轮 → per-sender 24h 预算误拒。提升为 backend 级 per-session map（上限 512） | `claude/acp.rs` |
| 5 | 🟠 | ✅ | `switch_named_session` INSERT/DO UPDATE 漏 `task_todos` 列：/switch 后旧会话任务快照泄漏进新会话并自我延续 | `store.rs:1648` |
| 6 | 🟠 | ✅ | cron DST 春分日漏跑：日边界+命中复核仍救不回被步过的真值。改为**每步**实时偏移 | `cron.rs` next_after |
| 7 | 🟠 | ✅ | cron 表达式无解自动停用完全静默（list 过滤 enabled=1，rm 需要看不见的 id）：停用通知 conv + /cron list 显示已停用行 | `cron.rs` + `store.rs` |
| 8 | 🟠 | ✅ | service 单元文件内嵌明文 secret 按 umask 0644 落盘；install 对 load/enable 失败照样报成功 | `src/service.rs` |
| 9 | 🟠 | ✅ | SIGHUP 不热载 `admin_senders`（移除的管理员保留权限到重启） | `main.rs` + `dispatch/mod.rs` |
| 10 | 🟠 | ✅ | housekeeping 整体 `clear()` 非 cosmetic：comment conv 锚被清 = 回复硬失败；审批中 ask_slot 丢失。改选择性驱逐（评论会话/挂起审批豁免） | `feishu/platform.rs` |
| 11 | 🟡 | ✅ | 80k 水位提示用 input 裸值，与触发口径（input+cached）不一致 | `dispatch/round.rs` |
| 12 | 🟡 | ✅ | queued 写路径未用 BUSY 退避（争用一次即丢崩溃保护）；`/stop` 提示打印自身 PID；SIGTERM 无二次强退；比例档 ratio=1.0 无警示；AskSlot 死 rustdoc 链接 | 多处 |
| 13 | ⬜ | ⬜ | 排队持久化**被取批次**在轮次中途崩溃不重放（at-most-once）——语义已诚实文档化；at-least-once 需「轮末删行」+ 幂等保障，独立迭代 | `store.rs` 注释 |
| 14 | ⬜ | ⬜ | ACP stdout/stderr 无上限（v8-M2 遗留，SDK 无注入点，需上游） | `claude/acp.rs` |
| 15 | ⬜ | ⬜ | wecom `split_text_by_bytes` 按**序列化前**字节切分：含引号/控制符的代码类输出转义后超 4096 被服务端整体拒收；`(i/n)` 前缀未计入预算 | `wecom/platform.rs:41` |
| 16 | ⬜ | ⬜ | wecom 非 text 入站静默吞（media_errors 未填）；重连退避长连接不重置 | `wecom/` |
| 17 | ⬜ | ⬜ | codex `session_exists` 每轮全目录扫描（无 mtime 排序/上限）；gemini 无 ghost 预检（毒化循环，claude/codex 均已修）；gemini `delta:true` 消息会碎化/重复回复 | `codex/sessions.rs`、`gemini/` |
| 18 | ⬜ | ⬜ | ilink 无 message_id 时去重键=from+text（连发两条相同 "y" 被吞）；出站 video/voice 一律 `file_item`；入站媒体失败静默 | `ilink/platform.rs` |
| 19 | ⬜ | ⬜ | setup 向导 secret 输入未关终端回显（注释声称 termios）；setup TOML 拼接无转义 | `src/setup.rs` |
| 20 | ⬜ | ⬜ | metrics/health 端口被占仅 warn（监控失明等同宕机）；`replay_task_todos` 无字节预过滤（百 MB 转录秒级 CPU） | `main.rs`、`claude/sessions.rs` |

## 二、确认无恙（本轮专项核查）

SQL 全参数化无注入；schema v1→v12 迁移链完整幂等；queued 启动重放先于 recv 循环（load/clear 与外部流量无竞态）；instance lock L10 修复在位；HTTP bearer 恒定时间 + 非 loopback fail-closed；crypto（PBKDF2 600k + AAD）无恙；dedup 24h 窗的清理节奏与硬上限数学正确；ConvState 迁移后无锁跨 await、无双 map 原子性破坏。

## 三、功能挖掘（价值排序，供后续迭代）

**已承诺未实现**（docs/internal/P4_ROADMAP.md P8 后排 + 代码核对）：
1. `/export` 直出飞书云文档（fix 234001 根因）——MISSING
2. 审批待办聚合卡（多 pending 合并卡，request_id 路由已具备）——MISSING
3. 入站 webhook（CI/告警 → 会话注入，走 handle() 管线继承鉴权）——MISSING
4. ~~审批卡复用~~——确认**仅存在于文档**从未实现（每次询问新卡为实际行为）

**新功能候选**（价值 × 实现面评估）：
1. **ACP 自动学习模型窗口**：`UsageUpdate.size` 已在协议里但被丢弃——学习值回写可替代静态 `model_context_window_tokens`，200k 模型部署不再误配。面：acp.rs + config 联动。风险低。
2. **/cron 停机补跑策略**：`cron_catchup = off|one|all`（有界回填 N 条）。夜间/周报任务熬不过 deploy。面：cron.rs + store。风险低。
3. **崩溃轮次恢复**：/retry 只覆盖失败轮；启动扫描 last round 无 outcome → 推「↻ 恢复上一轮」卡。面：round.rs + run_stats。风险中（副作用幂等）。
4. **per-conv 模型切换**：/model 复制 /timeout 的 per-conv override 模式。风险低。
5. **compact 卡片化**：自动压缩提示走命令卡（本轮冒烟实测 400 字纯文本突兀）。风险低。
6. wecom 结构化分片（与 #15 同修）：复用 core split_message 的换行优先逻辑。

## 四、后续迭代优先级

1. **短平快批**（半天）：#15 wecom 序列化分片、#17 gemini ghost 预检、#19 termios 回显、#20 预过滤
2. **功能批**：ACP 窗口自动学习 → /cron 补跑 → compact 卡片化 → webhook 入站
3. **语义批**（需决策）：排队 at-least-once（#13，副作用幂等前提）、审批聚合卡
