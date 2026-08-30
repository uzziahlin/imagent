# Code Review v9 — v8 修复核验 + 增量审查报告

> **审查对象**：`imagent v1.15.0` @ `a2b276a`（main，已合并 v8 三批修复 + 真机校准第三轮）。
> **审查目的**：① 逐项核验 v8 的 27 项修复是否真实、完整、无回归；② 对修复引入的新代码（`98ae037..a2b276a`，891 行新增）做缺陷审查。
> **审查方法**：4 路并行子审查（core 控制通道 / ACP / feishu / wecom-main-ilink）逐行读 HEAD 代码 + `git diff 98ae037..a2b276a` 交叉比对 + SDK（agent-client-protocol 1.0.1、open-lark 0.20）源码核对；主会话对关键新发现（R1/R3/R4/R5）亲自读码复验；实跑 `cargo test --workspace` = **626 passed / 0 failed / 3 ignored**（与 CHANGELOG 声明一致）。
> **总体结论**：27 项中 **18 项修复到位且核验正确**，M2 按决策诚实文档化未修（「SDK 无注入点」经源码比对属实），**6 项部分修复留有残留缺口**（H1/M3/L2/L7/L8/L9），**修复本身引入 3 个需跟进的回归**（R1 内容丢失最重要）。本轮新增 R1-R15 编号供修复跟踪。

## 一、v8 修复核验汇总

| v8 项 | 结论 | 备注 |
|---|---|---|
| H2 / H3 / M4 / M5 / M6 / M7 | ✅ 已修正确 | H2 `env -i` 白名单与 CLI 路径精确对齐、SDK argv 形态无双重处理；M4 哨兵 `imagent:untitled-tool:` 恒过审（含空集 fail-open 堵死） |
| M1 | ✅ 已修正确 | 残留：`dl_client` 无 read_timeout → R15 |
| L1 / L3 / L4 / L5 / L6 / L10 / L11 / L12 / L14 / L16 / L17 | ✅ 已修正确 | L3/L17 主交错已防住，竞态窗口收窄到一个 RTT（可接受，机理已记录） |
| H1 | ⚠️ 部分修 | 请求侧闸门 + FullLoop 恒 bind 正确；回复侧残留 → R4 |
| M3 | ⚠️ 部分修（既定取舍） | stdin/终态悬挂/外层读循环跳出均已修；内联 300s 串行化为 D-3 保守路线文档化保留（迭代项，不计缺陷） |
| L2 | ⚠️ 部分修 | socket permission 路径已收敛；ACP hook 与 ask_via_im 两路径残留 → R5/R6 |
| L7 | ⚠️ 部分修 | 主路径已修；NewSession 分支漏设 `loaded_cwd` → R9 |
| L8 | ⚠️ 部分修 | 正文三路径已转义；工具/思考面板、询问卡绕过路径仍在 → R7 |
| L9 | ⚠️ 部分修 | cap 函数正确、网络 O(n²) 已解决；`stream_body_final` 未截断 + 两个内容丢失回归 → R1/R2/R8 |
| M2 | 📄 文档化未修（诚实） | SDK `connect_to` 自持 stdout/stderr fd、`with_debug` 只能观察不能丢弃，无外部注入点——评估属实；follow-up 指向上游 PR 或自定义 transport |

## 二、部分修复的残留缺口（机理均经读码确认）

### H1 残余 → R4：热切 deny 后已 pending 的审批卡仍可放行

请求侧闸门（`core/src/dispatch/socket.rs:615-632`：入口读 mode，非闭环档 `fixed_reply` 固定答复，先于 `needs_approval`/`session_allows`）与 FullLoop 恒 bind（`mod.rs:981-991`）正确。但**回复侧** `router.route()` 投递决定前不复查 mode（`mod.rs:1039-1046`）——`/perm deny` 切换前已发出的审批卡，在 `permission_ask_timeout`（缺省 300s）窗口内点「允许」仍写回放行。这是原缺陷「审批闭环未拆」的回复侧一半。
**修法**：热切出闭环档位时对该 conv `cancel_all`，或 route 投递前复查 mode（deny/off 档投递 deny）。

### L2 残余 → R5/R6：三条等待路径只修了 socket permission 一条

- **R5（ACP hook 路径）**：`core/src/dispatch/mod.rs:869-875` 的 `AskWaitOutcome::Replied(r)` 分支无 `imagent:evicted` 哨兵检查、不调 `cancel_permission_ask`——claude-acp（FullLoop）的权限请求全走该 hook，淘汰后询问卡保持可点，点「允许」→ route miss → 无其它 pending 时字面 `"y"` 被当 prompt 跑 agent（原危害在此路径原样存在）。
- **R6（ask_via_im 路径）**：`core/src/dispatch/socket.rs:535-563` 的 `Ok(Ok(r))` 分支同样无哨兵处理——淘汰的问答 (a) 问题卡不收敛，(b) **字面量 `"imagent:evicted"` 被当用户答案回写给终端 agent**，(c) `ask_via_im_replies["ok"]` 计数失真。
- 小口径问题：socket permission 路径把淘汰计 `deny` 标签（socket.rs:760-764），与 TimedOut 的 `timeout` 口径不一。

### L9 半修 + 内容丢失回归 → R1/R2/R8

`cap_md_bytes`（`feishu/src/card.rs:18-45`）字节制头尾窗口（4KB+4KB）本身正确（char 边界回退 ≤3 字节、窗口不重叠、有单测）。问题在三处：

- **R1【本轮最高优先】终态最小卡兜底吞错 → 完整内容丢失**：终态 patch 失败后最小卡重试成功时返回 `Ok(())`（`feishu/src/platform.rs:2678-2684`），原始 Err 被吞——core 的 P5-11 纯文本补发只在 Err 时触发（`core/src/card_session.rs:250`），buried 路径虽有新卡重发（platform.rs:2691），**非 buried 路径的完整内容永久丢失**，而卡上写着「完整内容已转为文本消息发送」。
- **R2【中】8KB-30KB 区间输出被截 + 虚假承诺**：截断标注写死「完整内容见文本消息」（card.rs:40），但该区间整卡 fast-path patch **成功**送达（platform.rs:2575，body 被 card.rs:231 截到 8KB）时 core 视为成功、不补发文本——修复前 30KB 内可完整上卡，现在被截 + 承诺落空；buried 新卡重发同样吃 8KB 上限。
- **R8【低】声称与代码不符**：commit d7c5953 称「三处正文统一字节预算（Running md_body / 终态 md_body / 整卡 body_md）」，实际 `stream_body_final`（card.rs:705，终态 managed md_body）**未截断**，仅靠「超限失败→最小卡兜底」收敛（而该兜底又踩 R1）。

### L8 残留 → R7：面板与询问卡绕过路径

`escape_lt` 已覆盖正文三路径（render_card body :231 / stream_body_md :591 / stream_body_final :705），但同卡其它 markdown 面未转义：
1. `render_card` 工具面板（card.rs:334，summary 上限 80 字符，足够容纳完整 `<at id=ou_…></at>` ~45 字符）；
2. 思考面板（card.rs:358，400 字符）；
3. 审批卡 `perm_detail_md` head 行（card.rs:775）；问题卡 question 直拼（card.rs:1057，**无长度上限**）；
4. footer queued_hint（上游截 40 字符，弱可利用）。
另 `escape_lt` 全串应用含围栏代码块——代码块内 `a < b` 显示为 `\<`（观感退化，CommonMark 代码块不处理反斜杠转义）→ R13。

### L7 残留 → R9：NewSession 分支漏设 `loaded_cwd`

复用守卫已改 sid+cwd 双比（`claude/src/acp.rs:496-502`）、LoadSession Ok 分支双更新（:508-511）——主路径正确。但 `NewSession` Ok 分支（acp.rs:526-535）只设 `loaded = Some(sid)` 不设 `loaded_cwd`：同连接 idle 窗内 `/cd C` + `/new` 得 S2 后，`loaded_cwd` 残留旧值 A → 再 `/cd A` 发消息时双命中假缓存 → prompt 实际跑在 C。次生：每个新会话第 2 轮必多一次冗余 LoadSession。该逻辑无测试覆盖。

## 三、修复引入的新问题（除上节外）

| # | 级别 | 问题 | 位置 | 机理 |
|---|---|---|---|---|
| R3 | 🟠 | **`socket_spawned` 失败不回滚（L15 修复反噬）** | `core/src/dispatch/socket.rs:17-28` vs :37/:58/:68 | CAS 置位从尾部提前到入口，但 remove_dir/bind/from_std 三条失败路径 `return false` 不复位标志——注释 :9-11「bind/token 失败不置位，下次热切可重试」已失真。后果：首次 ensure 失败后，`/perm ask` 热切拿到伪「已就绪」（CAS 失败返回 true）→ 写入 Ask 档但 accept task 不存在 → Ask 静默退化全 deny，与回执「✅ 已切到 ask」矛盾。修法一行：失败分支 `store(false, Release)` |
| R10 | 🟡 | env 隔离测试依赖宿主环境（已实证失败） | `claude/src/acp.rs:1708-1735` | 测试经 `agent_command()` 读 `IMAGENT_ACP_COMMAND` 且未加 `#[serial]`——部署机设了该变量（文档宣传的生产配置项）时 `cargo test` 确定性失败（实测 `IMAGENT_ACP_COMMAND="custom-agent --flag" cargo test …sanitized_agent_command` → FAILED）。修法：测试内 remove_var + 标 serial，断言改 `ends_with(agent_command())` |
| R11 | 🟡 | `claude_parse` 丢顶层 `arguments` 回退 | `claude/src/backend.rs:542-547` | 旧代码 `input.or(arguments)`，新代码只剩顶层 `input` 与嵌套 `request.input`；某版本 CLI 发顶层 `arguments` 时审批卡丢输入预览，且 :515 注释仍宣称「input\|arguments 容错」 |
| R12 | 🟡 | JSON 形态 `IMAGENT_ACP_COMMAND` 被破坏 | `claude/src/acp.rs:263-299` | 前导 `/usr/bin/env` 后 `AcpAgent::from_str` 的 JSON spec 分支（SDK :456-462）永远走不通，env 会 exec 字面 `{...}` 文件名。原属隐式能力，破坏面窄但为行为回归 |
| R15 | 🟡 | `dl_client` 无 read_timeout（M1 残留面） | `feishu/src/client.rs:40-59` | connect 10s 但 body 停滞（非建连黑洞）时 `resp.chunk()` 循环（client.rs:636）仍可永久挂起。reqwest 0.12 有 `read_timeout` 未用 |

**微项（可随批次顺手处理，不单列跟踪）**：`/timeout` 的 `checked_mul(1)` 为乘 1 no-op（防护实由 43200 上限承担，注释误导，misc.rs:549）；wecom 测试注释与实测不符（直接以 1/2/3 调 `split_text_by_bytes` 仍会挂死，clamp 只在 cap 层，wecom/platform.rs:466-471）；H3 校验块插在 P8-4 注释中间致注释与代码隔断（config.rs:645-657）；`bearer_authorized` doc 注释仍是「非恒定时间」旧文案（main.rs:1186-1189）；ACP 诊断计数器 thread_local 跨调用持久 + 注释声称取样实际每行都记（backend_common.rs:357-370）；`AskSlot.resolved_at` 只写不读（死字段）；L2 淘汰计 deny 标签口径；M7/L7 修复无针对性单测（DoD 缺口）。

## 四、文档与工程诚信

- 测试声明属实（626/0/3）；M2「未修 + 理由」经源码比对诚实；v8 三批 commit 与 CHANGELOG 描述基本一致（L9 一处覆盖面声称与代码不符，见 R8）。
- **欠账**：`docs/CODE_REVIEW_v8.md` 跟踪表未回填（55 处仍 ⬜）、§五 D-1~D-8 决策落地情况未记录——按本项目「决策—理由—测试」可追溯文化应补（含 M2 实际走 (c) 文档化而非 D-2 建议的 (a)）。

## 五、跟进修复优先级（建议）

1. **R1 终态兜底吞错 → 内容丢失**（功能性损伤，用户最先感知；兜底成功也返回 Err 或以哨兵触发 core 补发）。
2. **R3 socket_spawned 失败回滚**（一行修复 + 失败重试测试）。
3. **R4 H1 残余**（route 前 mode 复查或热切 cancel_all）+ **R5/R6 L2 残余**（ACP hook / ask_via_im 哨兵收敛，堵住哨兵泄漏进终端答案）。
4. **R2 截断标注语义**（区分「已补发文本」与「未补发」两种文案，或对 8KB-30KB 区间直接走文本补发）。
5. **R7 面板补转义**（工具/思考面板、询问卡 question）+ **R13 代码块观感**（escape 避开围栏代码块内容）。
6. **R8 stream_body_final 补截断**、**R9 NewSession 补 loaded_cwd**（各一行级修复 + 补测试）。
7. **R10-R12、R15** 及微项、v8 跟踪表回填。

## 六、修复跟踪

| ID | 级别 | 标题 | 状态 |
|---|---|---|---|
| R1 | 🟠 | 终态最小卡兜底吞错 → core 纯文本补发不触发 → 内容丢失 | ⬜ |
| R2 | 🟠 | 截断标注「见文本消息」虚假承诺（8KB-30KB 区间被截且无补发） | ⬜ |
| R3 | 🟠 | socket_spawned 失败不回滚（L15 修复反噬，Ask 静默退化） | ⬜ |
| R4 | 🟠 | H1 残余：route 投递前不复查 mode（热切 deny 后 300s 窗口可绕过） | ⬜ |
| R5 | 🟠 | L2 残余：ACP hook 路径无哨兵收敛（"y" 当 prompt 原样存在） | ⬜ |
| R6 | 🟠 | L2 残余：ask_via_im 路径哨兵泄漏为终端答案 + 卡不收敛 | ⬜ |
| R7 | 🟡 | L8 残留：工具/思考面板、询问卡 question 未转义 | ⬜ |
| R8 | 🟡 | L9 残留：stream_body_final 未截断（commit 声称三路径实为两处） | ⬜ |
| R9 | 🟡 | L7 残留：NewSession 分支漏设 loaded_cwd（/cd 假缓存命中） | ⬜ |
| R10 | 🟡 | env 隔离测试依赖宿主 IMAGENT_ACP_COMMAND（实证翻车） | ⬜ |
| R11 | 🟡 | claude_parse 丢顶层 arguments 回退 | ⬜ |
| R12 | 🟡 | JSON 形态 IMAGENT_ACP_COMMAND 破坏 | ⬜ |
| R13 | 🟡 | escape_lt 破坏代码块内 `<` 显示 | ⬜ |
| R14 | 🟡 | 微项集合：注释失真/口径/死字段/DoD 缺口/v8 跟踪表回填 | ⬜ |
| R15 | 🟡 | dl_client 无 read_timeout（下载 body 停滞挂起面） | ⬜ |
