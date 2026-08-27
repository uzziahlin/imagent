# Changelog

记录 imagent 所有显著变更。格式参照 [Keep a Changelog](https://keepachangelog.com/)，版本遵循 [Semantic Versioning](https://semver.org/)。

## [Unreleased]

（空——下一段变更从这里开始。）

## [1.9.1] — 2026-08-27

> agent 总超时语义修正：默认关闭墙钟总预算，防挂死完全由空闲看门狗承担——持续流式输出的长任务不再在 600s 被硬杀。

### Changed
- **`agent_timeout_secs` 默认 0（关闭）**：原默认 600s 的总超时是墙钟预算，与 agent 是否活跃无关，长任务（重构、大量工具调用、慢模型）即使持续输出也会被误杀终止。防挂死职责已由空闲看门狗（`agent_idle_timeout_secs`，默认 300s 连续无输出才杀）更准确地覆盖，总超时降级为可选硬上限（设正数即启用）。涉及主轮 runner 与 `/compact` 两条路径；`/config` 展示文案同步。存量部署若 config.toml 显式写了 `agent_timeout_secs = 600` 仍按原值生效（重启后生效）。
- **D8 校验条件化**：`permission_ask_timeout_secs < agent_timeout_secs` 约束仅在总超时非 0 时生效；`agent_timeout_secs = 0` 由非法值变为合法的「关闭」语义。

## [1.9.0] — 2026-08-27

> **CODE_REVIEW v7 迭代批**（详见 `docs/CODE_REVIEW_v7.md` 第四节）：权限审批闭环跨后端统一、ACP per-conv 连接（P5-14）、凭据应用层加密等 12 项，新增约 25 个单测。

### Added
- **权限能力协商（B3）**：`Backend` trait 新增 `PermissionCapability`（FullLoop/NativeOnly/Unsupported）；闭环类档位（ask/auto-claude）× 非 FullLoop 后端**启动 fail-closed 拒绝**（不再静默忽略权限模式）；`/perm` 热切同口径校验；codex=Unsupported（exec 模式无原生 approval，注释附依据）、gemini=NativeOnly，日志输出能力矩阵。
- **ACP 完整 IM 审批闭环（B3）**：`session/request_permission` 接入 PermissionRouter——审批卡进 IM、y/n/超时 deny，与 claude-cli 同体验；`allowed_tools` 以能力矩阵显式呈现。
- **Ask 档热切补起 socket（D12）**：`/perm ask`/auto 热切换后惰性 spawn 审批 socket（幂等，bind 失败可重试），不再要求重启。
- **ACP per-conv 连接（B2，P5-14）**：单条全局连接重构为按会话连接 map——多会话并发不再互相阻塞（head-of-line blocking 消除），单会话取消/超时不再殃及其他会话；并发上限 8、空闲 10 分钟回收、shutdown 全量清理；含 in-process 假 agent 的并发/隔离测试。
- **凭据应用层加密（S3）**：设置 `IMAGENT_PASSPHRASE` 后，keyring 不可用时凭据以 AES-256-GCM + PBKDF2-SHA256(100k 迭代) 加密落盘（`enc:v1:` 版本化格式，随机 salt+nonce）；读取兼容 keyring/加密/明文三形态，存量明文惰性迁移为加密形态；未设 passphrase 的明文回退日志升级为 error（headless 不阻断）。

### Security
- **`/metrics` `/health` 鉴权（S7）**：新增 `IMAGENT_HTTP_TOKEN` Bearer 鉴权（不匹配 401）；**非 loopback 绑定且未配 token 时拒绝启动**（fail-closed）。

### Changed
- **ilink 熔断器修复（P1）**：双锁合单锁；reset 语义改为衰减（连续 threshold 次成功才清窗）——「限流一次→成功一次」的锯齿模式下熔断可正常触发。
- **WS 重连退避加 jitter（P1）**：feishu/wecom 重连退避 ±20% 随机抖动，防多实例同步重连风暴。
- **SQLite 写路径 busy 重试（P4）**：20 个写路径统一 `blocking_with_retry` 指数退避（50ms×2 至 2s、最多 5 次），高并发写不再直接 SQLITE_BUSY 报错。

### Fixed
- **codex 幽灵会话预检（B11）**：resume 前校验 `~/.codex/sessions` 存在性，失效 thread id 弃用续接、按新会话处理，不再反复 resume 失败毒化循环（gemini 无本机存储，维持纯 IM 历史）。
- **gemini 超长 prompt fail-fast（B13）**：>64KB prompt 启动前即拒绝并给可读错误（此前撞 ARG_MAX 得到不可理解的 spawn 失败）。
- **飞书 card_action 去重回退 key 改内容哈希（P3）**：与 v1.7.1 消息/评论回退同语义，前缀相同的不同按钮不再误判/漏判。

## [1.8.0] — 2026-08-27

> P10 排队可见性：设计原则「状态上卡，不上消息流」——运行中入队的消息不再静默，排队状态实时显示在流式卡 footer 与审批卡上，会话消息流保持干净（只有用户消息与 agent 产出）。

### Added
- **排队状态上流式卡 footer（P10-①②）**：任务运行中收到新消息，footer 实时变为 `🧰 正在调用工具… · 📥 排队 2 条，最新：「别用 npm…」`——计数随入队跳动（随 chunk 节流刷新），最新一条内容 40 字截断预览（纯媒体给「（图片/文件）」占位）。入队即被看见，不再需要单独的确认消息；取批转入处理后归零；/stop 一并清零。核心为 `OutboundCard.queued_hint`（平台无关，仅卡平台渲染；wecom/ilink/text 模式维持现状——它们已有 typing/流式文本体现活跃）。
- **审批等待 × 排队联动（P10-③）**：当前轮卡在等审批、又有新消息排队时（流式卡最静默的窗口——无 chunk、footer 不动），审批卡的 note 行**推送重渲染**为 `⏳ 等待你审批 · 后面还排着 N 条消息`（按 AskRender 记录的原渲染输入重画整卡，按钮 value 不变；note 缓存去重，计数不变不重画）。直接命中「没注意到审批卡导致消息堆积」的场景。
- **合并保留说话人（P10-④）**：批内出现多个不同发送者（群聊多人）时，合并 prompt 各段加 `【sender】` 标注——agent 能区分谁说了哪句，修复群聊多人合并不再丢失归属；单人连发不加标注（零噪音）。纯文本平台同样生效。


## [1.7.1] — 2026-08-27

> **CODE_REVIEW v7 缺陷修复批**（详见 `docs/CODE_REVIEW_v7.md`）：安全收紧 6 项 + 调度正确性 11 项 + backend 正确性 8 项，含新增约 12 个针对性单测。

### Security
- **审批回复鉴权收紧（S1）**：`can_route_permission_reply` 由「sender 白名单 OR 会话白名单」收紧为「sender 白名单 OR admin」——群被 `/chat` 放行后，普通群成员不再能以 "y" 批准高危工具请求。
- **空管理员不再放权（S2，行为变更）**：`admin_senders` 为空时不再「全员管理员」，IM 内管理命令一律拒绝并附配置引导；启动期 warn 提示。**依赖旧「空=全员可」语义的部署需显式配置 `admin_senders`。**
- **飞书去重回退 key 改内容哈希（S4）**：事件缺 id 时不再用 `receive_id + 文本长度`——等长不同消息不再被误判重复吞掉，重放窗口同步消除；评论事件同理。
- **飞书发送幂等（S5）**：所有出站消息在重试循环外生成 uuid 作飞书幂等键，429/超时重试不再产生重复消息。
- **workdir 黑名单补齐（S6）**：新增 `/private`、`/var/tmp`、`/private/tmp`、`/private/var/tmp` 等 canonicalize 等价敏感根，堵住 `/cd /private` 类绕过。
- **wecom 探针 URL 校验（S8）**：`probe_credentials` 复用 `validate_ws_url`（wss 任意 host、ws 仅 loopback），secret 不再可能发往明文非预期地址。

### Fixed
- **backend 失败路径清理权限 pending（D1）**：backend 出错 / 取消 / 空闲超时 / panic 均会 cancel 本会话全部 pending 并收敛卡片——「agent 已死但 pending 挂满、期间消息持续被吞成 deny」消除。
- **审批等待期不再吞自由文本（D2）**：自由文本仅明确命中 y/n 词表且无多 pending 歧义时才作为审批决定消费；否则走正常消息路径，并 60s 去重提示「存在待审批项」。
- **看门狗豁免收窄（D3）**：pending 区分 Permission/Ask 来源，终端 ask_via_im（超时可至 24h）不再无限豁免 IM 会话空闲看门狗。
- **shutdown 改 `CancellationToken`（D4）**：消除 `notify_waiters` 非持久信号的丢失窗口。
- **审批卡竞态（D5）**：先 register 占位、发卡后回填 card_msg_id，极早点按钮不再把按钮消息当 prompt 跑 agent。
- **`/stop` 原子 cancel（D6）/ `/resume` 缓存按 (conv, sender) 隔离 + 600s TTL（D7）/ `permission_ask_timeout < agent_timeout` 启动强制校验（D8）** 及卡片节流、指标污染、非 unix 锁报错四个小项（D9-D11）。
- **读行忙循环（B1）**：区分「单行超长跳行」与真实 IO 错误（后者终止读取），ACP/各 backend 不再可能无限空转。
- **ACP 不丢事件（B4）**：`try_send` 改 `timeout(30s, send().await)`，通道满不再静默丢文本/工具事件。
- **进程组清理（B5）**：unix 下 `process_group(0)` + killpg，孙进程（MCP server / 长跑 shell）不再泄漏。
- **mcp json 随 profile 隔离（B6）**：目录改用 `imagent_home()`，多 profile 不再互删配置。
- **并行工具调用全量收集（B7）/ claude 中间文本推流（B8）/ final_text 多消息拼接（B9）/ codex 顶层 error 透出（B10）**。

## [1.7.0] — 2026-08-27

> P9 交互第二批（一二档快赢 + /config 表单卡）：流式卡终止按钮、邮箱掩码防租户审计 400、hr/flow 视觉细化、/ws 删除钮、空产出占位、`/config` 下拉表单卡。

### Added
- **流式卡 ⏹ 终止按钮**：Running 卡底部常驻 danger「⏹ 终止」，点击回调注入 `/stop`（imagent_cmd 机制，与手打命令同鉴权/分派）。managed 卡按钮不随终态移除（element PATCH 只能动 markdown，点击回「当前没有运行中的任务」，无害）；降级/话题路径整卡重渲染，终态自然消失。
- **`/config` 表单卡（P9-2）**：飞书 `/config` 无参时渲染 CardKit 表单——`form` + `select_static` 下拉（回复形态 / 工具过程展示 / 群消息须 @bot）+ 提交按钮；提交值经 `card.action.trigger` 的 `action.form_value` 回传（不在 `value`，lcab 同款校准），proto 侧按**键白名单**合成 `/config form k=v …` 走既有分派（admin 门槛不豁免）。不支持表单的平台 trait 默认降级纯文本（原当前值 + 用法）。`/config form k=v k=v` 文本命令也可直接用（多键一次应用、逐键回报）。
- **/ws 删除按钮**：每个命名工作空间「使用」（primary）+「删除」（danger）两钮（对标 lcab workspacesCard）。

### Changed
- **邮箱掩码防租户审计 400（P9-1）**：飞书租户开消息审计后，含裸邮箱的出站内容回 400（"contain sensitive data: EMAIL_ADDRESS"），流式卡**静默失败**（典型触发：git commit 的 Co-Authored-By 尾注）。渲染边界统一把 `@` 改写为 `[at]`（lcab mask-email 同款；刻意不用全角＠/零宽字符——中文审计归一化还原后会再次触发；点分 TLD 要求避开 npm scope/版本号/裸句柄）。覆盖：流式 body/终态、降级卡正文与工具面板、审批卡详情、问题卡、命令卡、全部出站文本（send_text）。
- **hr 分割线 + flow 按钮布局**：V2 卡片的 `hr`（lcab 生产验证）用于审批卡/问题卡/命令卡正文与按钮分隔；按钮组改 `flex_mode: "flow"` + `width: auto` 自适应布局（按内容宽度自动换行），替代此前每行 3 个等宽列。
- **空产出占位**：agent 零文本零工具时终态正文给「（未返回内容）」（空串 patch 组件可能被拒/显示空白）。


## [1.6.0] — 2026-08-27

> 卡片交互改版两连：**P8-1 视觉**（对标 lark-coding-agent-bridge）——工具行「裸 JSON 截断」→「状态图标 + 人可读摘要」、流式卡分阶段 footer、审批/问题/命令卡标题栏、命令文案分组；**P8-2 交互**——审批卡复用（顺序询问不再刷屏顶卡）+ 终态结果下沉（多轮审批后结论落在会话最下面，不再埋在第一张卡）；**P8-4 权限**——auto 档映射 Claude 原生 auto 权限模式（分类器自动放行 + 高危走 IM），新增 `backend_permission_mode` 通用透传配置。视觉层为对方项目生产验证过的 CardKit 2.0 字段集。

### Changed
- **工具调用智能摘要**：`tool_summary 把工具 input JSON 压成人可读单行（Bash 取 command、Read/Write/Edit 取 file_path、Grep 取 pattern in path、WebFetch 取 url、TodoWrite 计数；覆盖 codex 的 shell/read_file/apply_patch 命名；截断 JSON 解析失败回退压平原文）。所有展示面共用：流式卡片工具行、审批卡签名行、纯文本工具摘要（`🔧 工具调用：Bash — git status）。COT 截断档随之 Brief 40→80 / Detailed 200→240 字符。
- **工具状态跟踪（⏳/✅）**：`ToolResult` 不再被丢弃——同名最早未完成的调用翻成 ✅，卡片工具行实时反馈执行进度（工具结果内容仍不进 IM，防大段输出刷屏）。
- **流式卡分阶段 footer**：🧠 思考中… / 🧰 正在调用工具… / ✍️ 输出中…（按最近一次 chunk 类型翻转，patch 经 per-card 缓存去重，内容不变不发）；终态收敛 ✅ 已完成 / ❌ 出错 / ⏹ 已中断（/stop 单列，不再显示为出错）。
- **降级路径工具面板**（managed 创建失败/话题群）：lcab 同款折叠面板——蓝边框 + 圆角 + 内边距 + notation 小字号 + 展开箭头图标，正文为状态图标工具行列表；超 7 条只保留最近 5 行 + 「☕ … 前面还有 N 个」计数。
- **审批卡改版**：卡片级标题栏（🔐 权限审批，橙色主题）；正文 = 工具签名行（`**Bash** — git status）+ 参数代码块（Bash 用 bash 语言高亮，其余 pretty JSON）+ 超时提示 note 行；不再裸贴 JSON。文本降级路径同步用智能摘要。
- **问题卡/命令卡加标题栏**：❓ 需要你的输入（蓝）/ 命令卡标题进 header（蓝）；命令按钮支持 primary/danger 分层（`CardButton.style：/help 的 状态=primary、中断=danger，/ws 使用=primary，/resume 首个接管=primary，问题卡首选项=primary）。
- **命令文案重排**：/help 按 会话/目录与文件/权限与运行/状态与诊断/白名单与管理 五组 bullets（此前 26 条命令挤一段无分隔）；/status 字段行加图标（🤖 后端/💬 本会话/🔗 会话/📁 工作目录/🏃 全局在飞/⏱️ 运行时长）；/sessions 列表 bullets + 活动项「（当前）」标记（原 `*）。
- **流式终态工具统计**：`🔧 工具 N 次：Bash×2 · Read×3（含总次数，× 计数分隔）。

- **`permission_mode = "auto"` 映射 Claude 原生 auto 模式（P8-4）**：claude-cli 下 auto 不再解析为 `ask`（每个提示都进 IM），而是新运行时档 **auto-claude**——照挂 IM 审批闭环（高危提示进 IM），另透传 claude 2026 新出的 `--permission-mode auto`（独立分类器逐动作审查：安全操作自动放行，`curl|bash`/外发敏感数据/强推等高危动作才拦下）。零配置默认姿态 =「分类器自动放行 + 高危过审」，与 Claude Code 官方 auto 模式一致；显式 `ask` 仍全量进 IM。旧版 claude CLI（<2.1.228）不认 auto 静默回退 default（≈ask 档，降级安全）。
- **运行时适配（P8-4）**：auto-claude 为运行时专属档（配置面 serde 拒绝直写，仅由 auto 在 claude-cli 解析产生）；`needs_socket()` 统一 Ask 闭环类判定（dispatcher socket spawn / mcp 子进程 roundtrip / `/perm` 热切提示）；`/perm` 查看显示档位与说明；ACP 防御臂 fail-closed。

### Added
- **审批卡复用（P8-2）**：同一会话内**顺序到达**的询问（审批/AskUserQuestion）不再每条新发一张卡把流式卡顶离视口——收敛后的询问卡（已批准/已拒绝/已中断）保留为该会话的复用槽，下一个询问**原地 patch 成新询问**（按钮换绑新 request_id）。挂着未决询问时（并发审批）不认领槽、照旧另发新卡，多 pending 语义不变；复用 patch 失败自动降级发新卡；同卡重登记不再误判为「被新询问取代」。
- **终态结果下沉（P8-2）**：本轮发过询问卡（流式卡已被顶离阅读位置）时，终态把流式卡正文收成一行指针（`✅ 已完成 · 🔧 工具 N 次\n⬇️ 完整结果见下方消息），**完整结果另发一张新卡**落在会话最下面——多轮审批后结论不再埋在第一张卡里。managed/降级/话题群三路径均支持；重发失败上抛，由 core 的 P5-11 纯文本兜底补发全文（结论不丢）。未触发询问的普通轮次行为不变（结果仍在原流式卡）。

- **新配置 `backend_permission_mode`（后端原生权限模式透传，P8-4）**：claude-cli 映射 `--permission-mode`（default/manual | acceptEdits | plan | auto | dontAsk | bypassPermissions；manual 归一 default；未知值启动期报错）。缺省不写 = auto 档透传 `auto`、ask 档不透传；显式设置则两档都遵从。**通用键设计**：codex/gemini 后续接各自原生档（approval-policy / approval-mode）复用本键；暂不支持的后端启动 warn 并忽略（`Backend::supports_native_permission_mode`）。SIGHUP 热重载支持。

### Fixed
- **终态「完成」双行**（真机反馈）：managed 流式卡终态正文末尾与 md_footer 各渲染一次 `✅ 完成（v1.5.4 起即存在）。修正为**状态行统一由 footer 承载**：正文只保留内容（文本 + 工具统计 / 错误详情 / 中断说明），不再拼终态行；结果下沉的 stub 正文同样只留统计 + 指针。中断的错误前缀单列（`⏹ 已中断，不再套「❌ 出错：」）；降级路径 stub 卡补 footer 元素（与 managed 路径的 footer patch 等价）。

### 迁移与注意
- **auto 档语义变更（需重启进程）**：claude-cli 下 `permission_mode = "auto"` 从「同 ask（每个提示进 IM）」变为「透传 Claude 原生 auto 模式（分类器自动放行，高危进 IM）」——升级后审批卡会明显变少，属预期；要回到全量过审显式配 `permission_mode = "ask"`。需 claude CLI ≥ 2.1.228（旧版静默回退 default ≈ ask 档）。
- 卡片改版为纯展示层，无配置变更；重启即生效（install.sh 覆盖 + `imagent service install` 重启）。
- 真机待验证项：header/title 模板色、折叠面板 border/padding 字段、notation 字号在自建应用上的渲染（字段集与 lcab 生产一致，理论无风险；若审批卡发送失败会自动降级纯文本，不影响审批闭环）。

## [1.5.4] — 2026-08-26

### Added
- **`approval_tools` 审批集**：ask/auto→ask 模式下**只有**清单内工具走 IM 审批，其余权限请求直接放行（记日志 + 指标）——解决「全放开不安全、逐个配白名单太麻烦」：工具保持缺省全量，只点名需要过审的（如 `["Bash", "WebFetch", "mcp__*"]`，尾部 `*` 前缀匹配）。空 = 既有语义（全部过审）。SIGHUP 热重载支持；仅 claude-cli 生效（claude 自身默认放行的工具如 Read 不发起权限请求，不受影响）。

## [1.5.3] — 2026-08-25

> 缺省安全姿态重构：不写 `allowed_tools` = 全部工具；不写 `permission_mode` = `auto`（claude-cli 自动起 IM 审批闭环）——零配置即「能力全开 + 危险操作过审」。**两项均为缺省值语义变更，需重启进程生效（SIGHUP 不足以重算缺省）。**

### Changed
- **`allowed_tools` 缺省改为全部工具**（不指定 = 不收敛）：缺省值由「读/检索/联网/编辑类白名单」改为 `["*"]`——claude 不附加 `--allowedTools`（CLI 默认全量）、codex 收敛 `workspace-write`、gemini 收敛 `auto_edit`（均不进各自最高危档）；`[]` 与 `["*"]` 同义。要收敛能力边界仍可显式列白名单。危险操作建议配合 `permission_mode = "ask"` 走 IM 审批（全量≠免审）。install.sh 不再写入限制性缺省列表。

- **`permission_mode` 缺省改为 `auto`（按后端自动选档）**：claude-cli（支持 IM 审批闭环）→ 自动按 `ask` 起 `--permission-prompt-tool` 全闭环；claude-acp / codex / gemini（闭环未接）→ `off`（靠各自 sandbox / approval-mode 兜底）。启动 / SIGHUP 热重载 / `/perm auto` 均先 `resolve` 成具体档再入运行时（未解析的 `Auto` 按未接线处理，防半接状态）。与「缺省全量工具」组合成默认安全姿态：能力全开 + 危险操作过审。install.sh 不再显式写 `ask`（继承缺省）。

## [1.5.2] — 2026-08-25

### Added
- **`imagent setup --platform feishu|wecom|ilink`**：直达对应平台引导（免菜单）；菜单默认值自动取现有 config 的平台（重配场景直达在用平台）；ilink 纯指引分支支持非交互运行；非法平台名无论 tty 与否先报明确错误；「覆盖已有 config」确认移入真正写 config 的 feishu/wecom 分支。

### Docs
- install.sh：升级场景提示（覆盖旧版本后需重启在跑实例：前台 Ctrl-C 重跑 / 后台 `imagent service install` 重装即重启）；启动提示修正为 `imagent start`。

## [1.5.1] — 2026-08-24

### Fixed
- **飞书/企微用户 `service install` 后守护进程误走 ilink**：`start` 的 `--platform` 由硬默认 ilink 改为缺省读 `config.platform`（CLI 显式优先）；`service install` 把平台显式写进 launchd plist / systemd unit 的启动参数；platform=feishu 且未 `export IMAGENT_FEISHU_APP_SECRET` 时安装期即报错（守护进程取不到交互 shell 环境变量，安装时快照是唯一注入点）。

### Docs
- README 新增「后台常驻（imagent service）」章节：安装 / 状态 / 卸载、macOS 与 Linux 差异表（日志位置 / enable-linger）、secret 快照与二进制路径注意点、多 profile 用法。
- README 命令表同步 P6/P7 新命令（`/file`、`/timeout`、`/admin`、`/chat allow-all`、`/config` 新键 require_mention / reply_mode、`/allow @名字`）；修正飞书启动示例为 `imagent start`（缺省读 config.platform）。

## [1.5.0] — 2026-08-24

> allowed_tools 缺省放宽 + 飞书接入文档（本次发版未及记录，回填）。

### Changed
- **allowed_tools 缺省放宽**：未配置时默认放开读 / 检索 / 联网 / 编辑类全套工具，执行类（Bash 等）仍需显式 opt-in；README 补能力边界与「ask 仍走审批」语义说明。

### Docs
- README 新增「接入飞书（完整流程）」章节（自建应用 / 长连接事件订阅 / 权限清单 / 凭据配置 / 首条消息授权，约 10 分钟）；install.sh 升级为一键安装 + 配置 + MCP 挂载。

## [1.4.0] — 2026-08-24

> P7：对标收尾（A1-A5）——管理员管理、批量放行、陌生人提示、回复偏好、profile 迁移。

（见 [P4_ROADMAP](docs/internal/P4_ROADMAP.md) P7 实现纪要。）

### Added
- **`/admin [list|add|remove]`**：管理员 IM 内动态管理（store schema v7 `admin_senders` 表，config 种子 ∪ 动态条目，即时生效 + 审计）；首位管理员设立时操作者自动一并加入（防自锁）；不可移除自己。
- **`/chat allow-all`**：批量放行 bot 已加入的全部群（`Platform::list_joined_chats`，飞书分页聚合，200 群上限）。
- **陌生人被 @ 提示**（config `stranger_mention_hint`，默认关 = 完全静默防探测）：未放行群里 @bot 回一句 `/chat allow` 引导；`InboundMessage.mentioned_bot` 据群消息 mentions 元数据判定（弱过滤不误发）。
- **`/config reply_mode card|text`**：回复形态偏好热切换（text = 不建卡走纯文本流；config `reply_mode` 种子）。
- **`imagent profile export|import`**：JSON 导出/导入（config 行级脱敏 `wecom_secret`，`--include-secrets --yes` 才带明文；白名单/管理员/命名空间随迁；keyring/环境变量凭据不随导出并明示）。

## [1.3.0] — 2026-08-24

> ask_via_im：终端 agent 反向接入——电脑终端上的任意 agent 需要用户决策时，把问题转发到飞书，用户在手机点按钮/回文字作答，答案按 request_id 精确分发回发起方。多 agent 并发与 IM 会话审批共存。

### Added
- **`ask_via_im` MCP 工具 + `imagent mcp-ask` 子命令**：供任意终端 agent（Claude Code / ZCode / Codex…）挂载的 stdio MCP server；工具参数 `question`（多行 markdown 补充说明）/ `options`（≤8 选项按钮）/ `source`（提问方标记，多 agent 并发区分「谁在问」，卡片标题渲染「💻（终端 agent · \<source\>）」）/ `timeout_secs`。config `ask_via_im_conv`（设了才启用）+ `ask_via_im_timeout_secs`（默认 1800，可被调用覆盖）。
- **`imagent mcp-ask --print-config`**：输出 mcpServers JSON（command 自动填当前二进制绝对路径），一键贴进任意 MCP client。
- **install.sh 一键脚本**：二进制（sha256 强校验；release 缺 mcp-ask 时 cargo 源码构建兜底）→ 首次生成 config（交互填 workdir/飞书凭据，secret 写 shell rc 防重复，已有 config 绝不覆盖）→ MCP 自动挂载（有 `claude` CLI 直接 `claude mcp add`，否则打印 JSON）。支持 `--workdir/--app-id/--secret/--yes/--mcp-only/--version/--bin`。
- README「终端 agent 接入」章节 + 一键脚本「方式零」。

### Changed
- **`PermissionRouter` 多 pending（conv × request_id）**：同 conv 下终端提问与 IM 会话审批并存互不顶替（per-conv 上限 8，超限最旧收敛）；回复路由三级——按钮回调带 `req` 精确匹配 → 自由文本引用回复（`parent_id` 命中询问卡）→ 最新 pending 兜底；hint 未命中不劫持别的 pending。`PermissionReply` 新增 `raw_text`（ask 路径以原文回传）。
- **socket 协议加 `kind`/`request_id`**（缺省 `permission`/`legacy`，向后兼容旧 MCP 子进程）；ask 分支独立超时预算，超时回 error（非 fail-closed deny），agent 可自行重试。
- **`Platform` trait 询问三方法带 `request_id`**，`send_permission_ask` 返回卡片消息 id（引用回复路由锚点）；新增 `cancel_all_permission_asks`（/stop 按 conv 全量收敛）。
- 飞书：审批/问题卡按钮 value 带 `req`；`parse_message_event` 解析 `parent_id`；`pending_asks` 多卡登记、cancel/resolve 精确到单卡；同 request_id 异常重发时旧卡 patch superseded。
- MCP 审批路径（claude headless）每次调用生成 `p-` 前缀 request_id。

## [1.2.0] — 2026-08-22

> P6：第二轮对标 lark-coding-agent-bridge——mention 基础设施、命令按钮卡、话题群、开箱体验。（见 [P4_ROADMAP](docs/internal/P4_ROADMAP.md) P6 实现纪要。）

### Added
- **mention 基础设施**：群消息客户端 @bot 过滤（config `feishu_require_mention_in_group`，默认 true；bot id 未知退化弱过滤）；正文 `@_user_N` 占位清洗（@bot 剥离、@他人转 `@名字`，text/post 双路径）；`InboundMessage.mentions` 元数据；`/allow @名字` `/disallow @名字` 直接 @ 对方授权（名字精确匹配 + 唯一性兜底 + 歧义提示），免手打 open_id。
- **命令交互卡片**：`Platform::send_command_card`（默认纯文本降级）；`/help`（六个常用命令按钮）、`/ws list`（每空间「使用」）、`/resume`（前 9 条「接管」）返回按钮卡（V2 column_set→button，每行 3 列）；按钮回调 `imagent_cmd` 映射为命令文本，走与手打命令完全相同的鉴权/admin 门槛（仅接受 `/` 开头，防伪造普通文本）。
- **话题群（thread）会话隔离**：群消息带 root_id 时 conv 升级 `feishu:<chat>:<root>`，每个话题独立 session/批处理；文本/图片/文件回复走 `im/v1/messages/{root}/reply` 落回原话题；话题 conv 继承所属群白名单授权。
- **`imagent setup` 首次运行向导**：飞书应用配置六步清单引导 → 凭据录入 + tenant_access_token 连通性校验 → 工作目录安全校验 → 写 config（0600）；WeCom 分支同构（WS subscribe ack 探针，配错/吊销安装期即暴露）；iLink 指引 login。
- **`imagent service install|uninstall|status`**：程序化安装 launchd（macOS）/ systemd 用户单元（Linux），注册当前二进制与 `--profile`，凭据环境变量快照进服务定义。
- **出站文件发送**：`upload_file`（im/v1/files）+ file 消息；`send_media` 按 kind 分流 image/file；`/file <path>` 命令（workdir 限定）。
- **`/timeout [N|off|default]`**：会话级空闲看门狗覆盖（分钟粒度），`round.rs` 消费点接入 per-conv 值。
- **require_mention IM 内热切换**：`Platform` trait 新增查询/设置；`/config require_mention on|off` 对下一消息生效（重启回 config 值）；`/config` 展示当前值。
- **话题群流式卡片**：话题内走「reply 发 raw 卡 + `msg:` 句柄整卡 patch」，审批卡与命令卡在话题内同样发卡；managed 打字机流式仍限普通会话（卡片实体无法在话题内引用）。

### Changed
- **`/cd` 与 `/ws use` 安全校验**：拒绝 `/`、home 根、系统目录等过宽工作目录（黑名单与输入双侧 canonicalize，macOS symlink 归一），存量宽泛目录同样拦截。
- fuzz target `feishu_event_parse` 扩展：MentionPolicy × bot_open_id 四组合 + `is_group_message_event`。

## [1.1.0] — 2026-08-22

> v1.0.0（2026-07-29）以来的全部变更：P4/P5 六波迭代 + 依赖治理 + 文档站 + 真机校准七修 + AskUserQuestion 透传。store schema v1→v6（线性迁移，旧库自动升级）。

## [1.1.0] 波次 — P5 第七波（维护）：dependabot 八连清零、feishu fuzz、文档站同步

（见 [P4_ROADMAP](docs/internal/P4_ROADMAP.md) P5 第七批纪要。）

### Changed
- **依赖升级**（dependabot 积压 8 个 PR 全部落地）：GitHub Actions（deploy-pages v5 / upload-pages-artifact v5 / codecov-action v7 / upload-artifact v7 / action-gh-release v3）；tokio-tungstenite 0.24→0.29（`Message::Text` 载荷改 Utf8Bytes，wecom 客户端适配 `Message::text()`；与 openlark SDK 统一在 0.29 避免双版本栈）；aes 0.8→0.9（cipher 0.5 trait 改名 + `from_mut_slice` 弃用迁移）；clap →4.6.6。三个 cargo PR 的 CI 失败根因是过期分支（fmt 漂移 + lock 漂移），当前 main 重放全绿。
- **飞书事件解析 fuzz target**：消息/审批按钮/云文档评论三类 payload 全解析路径（`feishu_event_parse`），并入每周 fuzz 任务；本地冒烟 162 万次执行零崩溃。
- **mdBook 文档站同步**：新增 `ARCHITECTURE.md` 现状架构（文档站首页内容）；`SUMMARY.md` 补上此前遗漏的 FEISHU_DESIGN；DESIGN/FEISHU_DESIGN 状态头改为历史快照指向新文档；`.gitignore` 补 book/ 与 fuzz 本地产物。

## [1.1.0] 波次 — P5 第六波：路线图三大项——dispatch 拆模块、孤儿卡片关流（schema v6）、feishu token 自愈

（见 [P4_ROADMAP](docs/internal/P4_ROADMAP.md) P5 第六批纪要。）

### Changed
- **dispatch.rs 巨石拆分**：5238 行单文件拆为 `dispatch/` 目录模块——`mod.rs`（Dispatcher 生命周期/主循环/批处理 runner）、`commands/{mod,admin,session,misc}.rs`（21 个斜杠命令按主题分组，`handle()` 只留鉴权门 + 分派）、`round.rs`（单轮 agent 状态机）、`socket.rs`（权限审批 socket）、`tests.rs`。内容零转录搬运 + 子模块 `use super::*`，`lib.rs` 导出路径不变，行为等价由全量测试背书。
- **store schema v6**：新增 `live_cards` 表（在飞流式卡片登记，per-conv 至多一张，含 v5→v6 迁移）。

### Fixed
- **进程重启后飞书流式卡片不再永远停在「生成中」**：卡片首帧发出即句柄落库（`live_cards`），终态 patch 成功摘除；崩溃/被 kill 后下次启动按登记扫描，把孤儿卡片 patch 成「⏸️ imagent 已重启，本次生成被中断」终态（P5-11 的纯文本降级路径登记保留，同样由扫描收尾）；平台已切换的旧登记作废删除。
- **feishu 缓存 token 被服务端提前吊销后 2 小时内无法自愈**：识别 token 失效错误码（99991661-64/68/79 及 "invalid access token" 文案），清缓存强制刷新后重试一次——覆盖文本/评论分片、媒体上传、流式卡片创建/更新、审批卡片与撤卡、入站媒体下载。
- **feishu SDK 路径无 429 重试**：`send_text_msg`/`send_card_msg`/`patch_card`/`send_card_ref_msg`/`upload_image`/`send_image_msg` 六个 SDK 函数补齐 500ms→1s→2s 退避重试（识别串扩展到 SDK ApiError Display 形态）。
- workspace 测试 351 passed（新增 6：live_cards 生命周期 ×3、v5→v6 迁移回环、token/限流识别 ×2）；clippy 0 warning；fmt clean。

## [1.1.0] 波次 — P5 第五波：push 后自审——修复三处自引入回归 + 六处次级问题

（对 P5 前四波 diff 的二次审查结论，见 [P4_ROADMAP](docs/internal/P4_ROADMAP.md) P5 章节。）

### Fixed
- **（回归）ilink 游标推进失败不再丢消息**：P5-13 的「致命化」版本在 set_sync_buf 失败时丢弃整批——但 dedup 键已插入，重拉同批被去重吸收 = 静默丢消息。现改为原地重试 3 次，仍失败则照常投递本批 + error 告警（宁可 5 分钟后可能重复，不可丢用户消息）。
- **（回归）`imagent --profile <p> status` 读不到凭据**：status 漏设 keyring scope（读不到 scoped 键会报误导性错误或显示迁移前旧凭据）；同时改为按 config 平台判定——wecom/feishu 打印 config/env 配置指引而非误查 ilink。
- **（回归）/health 的 wecom logged_in 恒 false**：wecom 凭据在 config（store 查不到），启动时按 `wecom_bot_id`/`wecom_secret` 存在性预判定。
- **单实例锁改 flock**：内核随 fd 持锁到进程退出——消除「排他创建+事后写 PID」在两实例毫秒级并发启动时的删锁重建竞态（恰是要防的场景），也消除 PID 复用误判；锁文件内容降级为诊断信息。
- **流式去重不再吞推送失败的段落**：只累积发送成功的前缀（`reply_ok`），失败段落留给最终全量兜底（此前中间推送失败后该段两处皆失）。
- **feishu 429 重试在卡片/评论路径生效**：`cardkit_resp`/`reply_comment` 先判 HTTP 状态并把 429 归一为可识别标记（此前 429 的非 JSON 体解析错误不含标记，重试从不触发）。

### Changed
- `/ws use` 切目录后失效 /resume 列表缓存（同 /cd）；codex 本机会话扫描移入 `spawn_blocking`（防卡 tokio worker）；`/stop` 仅在确有 pending 审批时撤回询问卡（防误把已回答的旧卡 patch 成「已中断」）。
- workspace 测试 345 passed（新增 5 用例：flock 三场景、/stop 中断 /compact、Err 路径 session 持久化）；clippy 0 warning；fmt clean。

## [1.1.0] 波次 — P5 第四波：设计债务收敛（store 事务/轮转、keyring 隔离、metrics、媒体 TTL、feishu 限流、codex 扫描）

（发现与排期见 [P4_ROADMAP](docs/internal/P4_ROADMAP.md) 的 P5 章节。）

### Fixed
- **store**：`upsert_session` 主表 + `session_history` 两条语句包进同一事务（中间崩溃不再漏历史行）；`session_history` per-conv 轮转保留最近 50 条（此前只增不删，长生命周期部署无限增长）。
- **keyring profile 隔离**：username 改 `{profile}:{platform}:{account}`（无 profile 时保持旧格式，存量部署零迁移）；scoped 键 miss 回退旧键（过渡期老凭据可用，下次 login 写入新键）；删除时双键清理。多 profile 同机同平台不再互删凭据。
- **/health**：`logged_in` 按实际平台判定（ilink/wecom 查 store 凭据、feishu 查 `IMAGENT_FEISHU_APP_SECRET`）——此前固定查 ilink，feishu/wecom 下恒 false 有误导。
- **feishu token 读取**：读锁快路径 + 写锁双检——此前每次取写锁且跨网络调用（最坏 30s），刷新期间所有发送/媒体下载被串行阻塞。
- **feishu 限流**：手写 HTTP 路径（卡片实体/PATCH/评论回复/媒体下载）识别 HTTP 429 / code=230020，500ms→1s→2s 退避重试；`send_text` 分片失败标注「第 N/M 片发送失败（回复可能被截断）」。

### Added
- **metrics**：`imagent_permission_decisions_total{result=allow|deny|timeout|dropped}`（审批决策分类）+ `imagent_agent_timeouts_total{kind=idle|total}`（空闲看门狗与总预算超时分开计数）。
- **媒体 TTL 清理**：`<imagent_home>/media` 下 7 天前的入站媒体文件自动删除（启动清一次 + 每日循环，best-effort）。
- **codex 本机会话扫描**：`Backend::list_local_sessions` 的 codex 实现（扫 `~/.codex/sessions/**/rollout-*.jsonl`，session_meta 的 id + cwd 判定归属）——`/resume` 在 codex 后端不再退化为纯 IM 历史，💻 接管 + cwd 校验同样生效。

### Changed
- workspace 测试 343 passed（新增 7 用例）；clippy 0 warning；fmt clean。

## [1.1.0] 波次 — P5 第三波：单实例锁 / 握手 token / 游标致命化 / 编码校准 / /stop 收尾 + 六项快赢

（发现与排期见 [P4_ROADMAP](docs/internal/P4_ROADMAP.md) 的 P5 章节。）

### Fixed
- **P5-9（安全）单实例锁 + 权限 socket 握手 token**：`<imagent_home>/instance.lock`（排他创建 + PID 存活探测，陈旧自动接管；仅 `imagent start` 获取）防双实例互劫持 permission.sock 使 Ask 审批闭环静默失效；socket 连接首行须回传随机握手 token（`permission.token`，0600），同 uid 裸 connect 伪造审批请求的门槛从零提高到需读到 token。**注意：mcp 子命令与主进程须同版本部署（握手协议变为两行）。**
- **P5-13 ilink 游标推进失败升级为致命**：此前仅 warn 继续，服务端每轮重推同批消息、dedup 窗口（5min）过期后同批消息会**重复驱动一轮 agent**；现在返回 Err 走退避重试（at-least-once 语义不变）。
- **P5-15 本机会话扫描候选编码联合 + 接管 cwd 校验**：目录编码改为多候选（`/`、`/._`、非字母数字三种规则）联合扫描去重——不再漏扫含 `.`/`_` 的 workdir；`/resume` 接管本机会话前校验 jsonl 记录的 cwd 与当前 workdir 一致，编码冲突（`/a/b-c` vs `/a/b/c`）也不会串项目接管，不符时引导 `/cd`。
- **P5-16 /stop 收尾三件**：① `PermissionRouter::cancel` 先投递 fail-closed deny 再移除——审批等待方立即收到结果（此前挂满 300s）；② 新增 `Platform::cancel_permission_ask`，飞书把滞留的询问卡片 patch 成「已中断」终态（移除按钮，防对已死任务审批）；③ `/compact` 注册进在飞表，可被 `/stop` 中断。
- **快赢六项**：`Config::load` 数值边界校验（超时 ≥1、batch_window ≤10s，0 值超时启动期即报错）；配置加载失败改非零退出码（此前 0，systemd 视为成功）；二次 Ctrl-C 立即强退（130）；`/cd` 失效 `/resume` 列表缓存；ilink 媒体目录改走 `imagent_home()`（多 profile 隔离）；飞书媒体下载改手写实现带 Content-Length 预检 + 流式 50MB 上限（此前 SDK 版无上限）。

### Changed
- workspace 测试 337 passed（新增 8 用例）；clippy 0 warning；fmt clean。P5-14（ACP per-conv 连接）留待真机验证后实施。

## [1.1.0] 波次 — P5 第二波：深度 Review 安全 + 正确性修复（五项）

（发现与排期见 [P4_ROADMAP](docs/internal/P4_ROADMAP.md) 的 P5 章节。）

### Fixed
- **P5-7（安全）群放行 + 空管理员组合的启动硬告警**：`allowed_chats` 非空且 `admin_senders` 为空时，被授权群的所有成员都是事实管理员（/allow 扩权、/chat 扩群、/config /perm）。新增 `Config::admin_gap_with_chat_allowlist()` 探测 + main 启动期 error 级告警（含收紧指引）；不拒启以兼容单用户语义。
- **P5-8（安全）飞书云文档评论须 @bot 才触发**：此前任何带文字的评论都驱动一轮 agent 并回复到别人评论下。`parse_comment_event` 增加 bot id 参数——已知时要求 at 节点命中 bot 且 sender 非 bot 自身（防自回复循环）；bot open_id 经 `GET /bot/v3/info` 懒取缓存（取不到退化为「须含 @」弱过滤）。**行为变化：文档评论现在必须 @bot**。
- **P5-10 非卡片平台流式回复不再推两遍**：codex/gemini/ACP（中间 Text 流式 + Final 全量）此前在 ilink/wecom/飞书评论线程上整段重发；现累积已推前缀、最终只补差量，流式推完且无差量不发空消息。
- **P5-11 流式卡片终态失败降级纯文本**：终态 patch（Done/Error）失败时以 `send_text` 补发完整结论——卡片可以停在「生成中」，结论不能丢。残余：进程崩溃后的孤儿卡片（需启动扫描，待排期）。
- **P5-12 wecom 三处保守修复**：群消息显式拒收（此前被当单聊处理、回复错发到与发言者的私聊）；入站回调满由即丢改为 1s 有界背压（短暂消费抖动不再丢消息，仍护住心跳）；出站 ack errcode≠0 从 debug 升级为 warn（含 req_id，限流/非法 chatid 可查）。

### Changed
- workspace 测试 329 passed（新增 5 用例）；clippy 0 warning；fmt clean。wecom ack 完整等待闭环未做（需真机验证回执语义）；飞书 @bot 过滤含一次 /bot/v3/info 调用。

## [1.1.0] 波次 — P5 第一波：深度 Review 安全 + 正确性修复（六项）

（全量 review 发现与后续排期见 [P4_ROADMAP](docs/internal/P4_ROADMAP.md) 的 P5 章节。）

### Fixed
- **P5-1（安全·严重）审批回复路由绕过白名单**：审批回复的消费发生在 `handle()` 鉴权之前——群聊里非白名单成员发一条 "y" 即可批准 Bash 等高危工具、发任意文本被当 deny 吞掉。route 前增加 `can_route_permission_reply` 门槛（sender OR 会话白名单，与 handle() 完全一致）；飞书审批按钮回调携带 operator open_id 作 sender，同一门槛覆盖。
- **P5-2（安全）/perm 补管理员校验**：此前任何过门用户可把全局权限模式热切成 `off` 拆掉 IM 审批闭环；现与 `/config` 同级须管理员（查看仍开放）。
- **P5-3（安全）/disallow 补管理员校验**：此前任何过门用户可把管理员本人踢出白名单（DoS）；现与 `/allow` 对称。
- **P5-4 ACP 会话选择以 req.session 为权威**：删除 per-conv sessions 缓存（命中即用、无视外部传入，导致 `/new` `/resume` `/switch` 在 claude-acp 后端全部失效——以为切了会话实际跑在旧上下文），改为连接级 `loaded` 跟踪（同 sid 连续轮次免重复 LoadSession 的纯优化）。
- **P5-5 中断/失败路径不再丢 session id（「失忆」修复）**：新增 `AgentChunk::SessionStarted`——backend 一经学到 session id 即通知（CLI 五个学习点 + ACP 建会话后）；`/stop`、空闲超时、backend Err、panic 等拿不到 RunOutcome 的路径，只要学到过非空且与传入不同的 session id 就落库，下条消息续接本轮进度而非静默开新会话（与 Claude Code 自身中断语义一致；显式 `/new` 才重开）。空闲超时文案同步改为「进度已保留，下条消息续接」。顺带修正：落库 workdir 改记 `resolve_workdir` 实际值（原写 default_workdir，`/cd` 后记错）。
- **P5-6 ACP 每轮回复拖满空闲超时**：长驻 task turn 结束不清共享 `current`（StreamState 持 chunks sender 克隆），dispatch 的 chunk 循环等不到通道关闭、挂到空闲看门狗才退出；现在 turn 结束即清。

### Changed
- workspace 测试 324 passed（新增 4 用例：审批回复门×1、/perm 管理员×1、/disallow 管理员×1、/stop 中断保 session×1）；clippy 0 warning；fmt clean。ACP 改动需真机冒烟（`cargo test -p imagent-claude -- --ignored acp_e2e`）。

## [1.1.0] 波次 — P4 第三波：统一 /resume——无感接管电脑端 Claude Code 会话

### Added
- **统一恢复列表（P4-11）**：`/resume` 列表 = IM 会话历史（📱）∪ 本机同项目 agent 会话（💻）——用户按序号选择即接管，全程无需知道 session id。本机会话按 conv 当前 workdir 扫描（`/cd` 切换列表随之变化，workdir 对齐由扫描天然保证），首条用户消息摘要 + 相对时间展示。
- **`Backend::list_local_sessions(workdir)`** trait 方法（默认空，依赖方向不变）：claude-cli / claude-acp 扫 `~/.claude/projects/<workdir编码>/*.jsonl`（session id = 文件名；摘要取头部首条非元数据 user 消息，cap 64KiB 容错解析；排序按 mtime 原始精度防同秒并列）；codex/gemini 无本机存储概念，`/resume` 自动退化为纯 IM 历史。
- **接管语义**：选中 💻 会话 = 写 sessions 表自动绑定，回复附分叉提示（「续接将从此处分叉；若终端仍开着请先退出」）；列表 per-conv 缓存，序号选择取缓存防两次调用间 mtime 变化错位（选中即消费）。

### Changed
- workspace 测试 320 passed（新增：扫描器 7 用例 + 统一列表/接管/序号引导 3 用例 + 既有 resume 用例适配新文案）；clippy 0 warning；fmt clean；真机冒烟（imagent 项目 9 个本机会话列出、摘要/排序/截断正确）。
- 含默认 ignore 的真机冒烟测试（`IMAGENT_RESUME_SMOKE_WD=<proj> cargo test -p imagent-claude --lib smoke_real_dir -- --ignored`）。

## [1.1.0] 波次 — P4 第二波：对标差距 7 项全落地

（第一波见下方「P4 功能迭代」；路线与实现纪要见 [P4_ROADMAP](docs/internal/P4_ROADMAP.md)。）

### Added
- **P4-4 权限审批卡片按钮**：`Platform::send_permission_ask`（默认纯文本）+ feishu 覆写为 CardKit 2.0 按钮卡片（✅ 允许 / ⛔ 拒绝，callback value 编码 conv + 动作）；点击推 `card.action.trigger` 事件 → 解析成 `text="y"/"n"` 入站消息复用既有审批回复路由，core 零感知；卡片失败降级纯文本。需事件订阅 `card.action.trigger`。
- **P4-5 会话（群）白名单**：store v4 `allowed_chats` 表；鉴权改为「sender 放行 OR chat 放行」；`/chat allow|deny|list`（管理员门槛、缺省作用于当前会话）+ CLI `imagent allow-chat` + config `allowed_chats` 种子 + SIGHUP 热重载；发现态引导同时提示 sender/conv id。
- **P4-6 COT 三档 + `/config`**：`cot_detail = off|brief(默认)|detailed`（off 不收集工具过程；brief 40 字符/5 工具，detailed 200 字符/10 工具）；`/config` 查看/热改 `cot_detail`、`batch_window_ms`、`agent_idle_timeout_secs`（管理员）。
- **P4-7 IM 内诊断**：`/status`（平台/后端、本会话在跑与排队、会话、workdir、全局在飞、uptime）；`/doctor`（workdir / store 读写回环 / 会话数 / 在飞自检）；`/reconnect`（`Platform::reconnect`——feishu/wecom 经共享 Notify 断开当前长连接立即重连，其它平台回不支持提示）。
- **P4-8 `/resume` 历史会话**：store v5 `session_history` 表（session 变化时自动记录）；`/resume` 列最近 10 条（当前带 *），`/resume <序号|session_id>` 恢复（跨后端校验，恢复后回未命名）。
- **P4-9 云文档评论触发（MVP）**：订阅 `drive.file.comment.created_v1`（需 `drive:comment` 权限）→ 每条评论独立线程会话（conv `feishu:comment:<file>:<comment>`），回复走 `drive/v1/.../replies`（手写 HTTP）。评论线程不支持流式卡片与媒体回传（纯文本）；纯 @/纯图片不触发。
- **P4-10 Profile 多实例（MVP）**：`imagent_core::paths::imagent_home()`（env `IMAGENT_HOME`）统一锚定 config/db/sock/媒体（MCP 子进程 env 继承）；CLI `--profile <name>` + `profile create|list|remove`。已知限制：keyring 凭据键未按 profile 隔离。
- `supports_streaming_card` 改为 per-conv（评论线程走纯文本流）；`CotDetail`/`TaskBudgets` 导出；store 导出 `SessionHistoryRow`。

### Changed
- schema v3 → v5（`allowed_chats` + `session_history`，线性迁移幂等）。
- workspace 测试 311 passed（新增：会话白名单×3、cot/config×2、诊断命令、/resume、store chats/history、proto 按钮/评论解析×4、paths/config 单测）；clippy 0 warning；fmt clean；profile CLI 冒烟通过。

## [1.1.0] 波次 — P4 功能迭代（任务控制 / 批处理 / 看门狗）

对标 [lark-coding-agent-bridge](https://github.com/zarazhangrui/lark-coding-agent-bridge) 的差距分析（见 [P4_ROADMAP](docs/internal/P4_ROADMAP.md)）落地高优先级三项；其余 7 项待排期已记入路线。

### Added
- **P4-1 `/stop` 中断在飞任务**：per-conv 在飞注册表（conv_id → AbortHandle），IM 内随时中止跑偏/卡死的 agent——abort → `backend.run` future drop → 杀子进程（CLI 后端 kill_on_drop；ACP 后端既有 P1-E cancel 分支杀连接）。等 IM 权限审批时 `/stop` 先 `router.cancel`（MCP 收 deny，fail-closed）再中断；同时清空批处理排队消息并回报丢弃条数。被中断轮次：流式卡片 finalize 成 Error 终态（防停在「生成中」）、不落 session（保留上次成功映射）。
- **P4-2 运行中消息合并批处理**：runner 在飞期间到达的消息入 per-conv 队列（上限 100 条，超限回告警丢弃），当前轮结束后合并为下一轮单次执行（非空文本 `\n\n` 拼接、media 拼接）；批处理窗口 `batch_window_ms`（默认 1500，0 关闭）把连发消息并进同一轮 prompt。入队/取批共用一把 queues 锁原子判定，杜绝 lost-wakeup。会话续接不受影响（第二轮 resume 第一轮 session）。
- **P4-3 空闲看门狗 `agent_idle_timeout_secs`**（默认 300，0 关闭）：agent 连续无任何输出（chunk）该时长则终止本轮并杀子进程，防 stream 僵死干等 `agent_timeout` 总预算；core 收集循环单点实现，四个 backend 零改动；等待 IM 权限审批期间自动暂停（审批有独立预算）。触发后回「空闲超时，本轮输出未保存，会话保持上次成功状态」。
- **权限回复路由守卫**：等待 approve/deny 期间，斜杠命令（`/stop` 等）与空文本（纯媒体）不再被误当审批回复消费——`/stop` 在等审批时也可执行（此前会被吞成 deny，导致无法中断）。
- `Dispatcher` 构造参数聚合为 `TaskBudgets`（agent_timeout / permission_ask_timeout / shutdown_grace / agent_idle_timeout / batch_window），参数表 11 → 9。

### Changed
- conv 串行锁的获取/释放从单轮 agent 执行移到批处理 runner 循环外层（跨轮持有；P1-7 防泄漏语义不变，失败/panic/中断路径由循环统一释放）。
- workspace 测试全绿（新增 9 个用例：/stop 三态、排队丢弃、运行中合并、窗口合并、空闲看门狗、队列上限、合并纯函数、路由守卫分类）；clippy 0 warning；fmt clean。
- `cargo fmt` 顺带修复了此前提交未格式化的 `types.rs` / `crates/feishu/*`（纯重排，无逻辑变更）。

## [1.1.0] 波次 — 安全审查 v2/v3/v4/v5 修复

深度 Review v2/v3/v4/v5（见 [v2](docs/internal/CODE_REVIEW_v2.md) / [v3](docs/internal/CODE_REVIEW_v3.md) / [v4](docs/CODE_REVIEW_v4.md) / [v5](docs/CODE_REVIEW_v5.md)）的修复。

### Fixed
- **P0（阻塞）**：ACP 权限 fail-open→fail-closed（P0-A）、权限 socket 对端 uid 鉴权 + chmod 0600（P0-B）、login baseurl 域名白名单（P0-C）。
- **P1（凭据 / 安全姿态）**：WAL/SHM chmod 0600 + 凭据写入审计（P1-A/B）、keyring fail-closed 选项 `require_keyring` + metric（P1-C）、workdir「cwd（非沙箱）」措辞（P1-D）。
- **P1（健壮性）**：ilink 媒体解密 fail-closed + 流式下载防 OOM + login 禁 redirect（P1-H/J/L）、WeCom msgid 去重（`Dedup` 提到 core，P1-I）、compact_summary 删除推迟到 run 成功后（P1-K）、权限 socket 回复 `agent_timeout` 超时（P1-G）、`/new`/`/switch`/`/compact` 取 conv 串行锁（P1-F）。
- **工程化**：各 crate `#![forbid/deny(unsafe_code)]`（E-2）、MSRV 统一继承 workspace（E-1，由 1.80 抬至 1.88：`clap 4.6.1` 等声明 `edition2024` 需 cargo 1.85+，且 `agent-client-protocol-schema`/`serde_with` 等核心依赖声明 `rust-version 1.88`）、项目根 CLAUDE.md onboarding 更新（D-1）。
- **v3 P1（9 条，第三轮 review 新发现，见 [v3](docs/internal/CODE_REVIEW_v3.md)）：codex sandbox flag 错位（`-s` 移到 `--` 前）、CDN 下载强制 https scheme、send_text 失败不挂 pending、SIGTERM 优雅退出、in-flight task drain（JoinSet + shutdown Notify）、mcp read_line 超时、conv_locks 失败路径统一释放、PermissionRouter cancel API、permission socket read_line cap + write 超时。
- **v3 P2（10 条）**：权限回复 route 原子化（防 has_pending/route 间隙 race）、ACP sessions 有界 insert（防 clear 丢活跃）、backend panic 保留 final、peer_uid 威胁模型文档、macOS LOCAL_PEERCRED 比对 geteuid、wecom ws_url host 精确比较、明文→keyring 迁移审计、parse_reply 补中文确认词、upload_cdn percent-encode、~/.imagent chmod 0700。
- **v3 工程化**：CI lint-and-test 加 macOS 矩阵（peer_uid/SIGHUP/keychain 分支此前零覆盖）、clippy --all-features、book.toml owner 统一、文档漂移对齐（README 测试数/crate 列表、main 头注释、login 错误、SECURITY workdir 措辞）。
- **v4 第一波（开源基础设施 + 安全边界，见 [`docs/CODE_REVIEW_v4.md`](docs/CODE_REVIEW_v4.md)）**：
  - **S-2（安全）**：`spawn_cli_backend` 加 `env_clear()` + 运行时必需变量白名单（PATH/HOME/USER/LANG/...）+ per-backend 最小授权 API key 透传（claude `ANTHROPIC_API_KEY`、codex `OPENAI_API_KEY`、gemini `GEMINI_API_KEY`），防 agent 子进程继承父进程全部 env（`DATABASE_URL`/CI secret 等可经 `Bash env` 读取并经 tool_result 泄漏）。
  - **S-1（安全语义）**：`agent="claude-acp"` 且 `allowed_tools` 非空时启动 warn——ACP 无 `--allowedTools` 等价机制，工具收敛需靠 `permission_mode=ask/deny` 兜底。
  - **B2/B3/B4/B5（开源基础设施）**：README「双 license」→「MIT license」（事实错误）；`<owner>` 占位符 → `uzziah`（README clone 命令 + systemd Documentation，v3 E-2 漏修）；README 徽章改 pre-release + 路线表注明安全审查未发版；systemd `User=%i`（非模板单元开箱即坏）改注释 + 放开 `NoNewPrivileges`/`ProtectSystem`/`ReadWritePaths` 安全加固。
  - **文档/部署**：`docs/SUMMARY.md` 侧栏补 v2/v3/v4 review；launchd 日志 `/tmp` → `/usr/local/var/log`（原重启即丢）。
- **v4 第二波 A（低风险）**：S-5 `spawn_cli_backend` stdout 单行 8MiB 上限 + stderr 64KiB 截断（防 OOM，对称补齐 v3 P1-9 只给 permission socket 加的 cap）；S-6 MCP 配置 symlink 防护 + run 后清理（P3-2）；R-5 WeCom subscribe 认证失败改 return Err 触发重连（原空转发心跳致消息静默丢失）；R-6 WeCom channel 满改 warn 可观测 + Closed 退出；CI 新增 `fuzz.yml`（每周 cron）+ audit 回到 PR 阻塞。
- **v4 第二波 B（架构）**：S-3 新增 `permission_ask_timeout_secs`（默认 300s），审批等待独立预算不再挤占 `agent_timeout`；R-1 drain 宽限 `shutdown_grace_secs`（默认 60s，原硬编码 30s）；R-2 socket accept task 监听 shutdown + `handle_permission_socket` 纳入 JoinSet drain；R-3 main 退出清理 `permission.sock`（P1-5 计划③原未落地）；S-4 WeCom secret 明文限制文档化（完整 keyring 流程后续）。
- **v4 第三波（打磨）**：P2-10 新增 `Store::delete_credential`（删 SQLite + keyring + 审计，凭据轮换/吊销清理路径）+ `delete_from_keyring`；P2-R `append_audit` 轮转改 `max(id)` 范围删除（O(N)→O(logN)）；P3-N3 WeCom 收到 Ping 显式回 Pong；N18 metrics 命名 `imagent_claude_*`→`imagent_backend_*`（计所有 backend，避免误导）；CLI `--version` + `Cmd::Mcp` hide + `Stop` doc 对齐。

- **v5（开源首发就绪 + v4 半修复核，见 [v5](docs/CODE_REVIEW_v5.md)）**：
  - **F1 fuzz 编译**：`ilink/lib.rs` `mod proto/media` → `pub mod`；proto target 改调真实 `UpdatesResp` 反序列化 + `extract_text`；`cd fuzz && cargo +nightly check` 通过（原编译失败，README/CI 宣称的 fuzz 实际零覆盖）。
  - **F2 cargo-audit**：`ci.yml` audit step 加 `--ignore RUSTSEC-2024-0437`（protobuf 经 prometheus 引入，imagent 仅 exposition 不解析不可信 protobuf）+ 删死文件 cargo-audit.toml（cargo-audit 不读项目级 config）。
  - **F4 CI deny**：deny job 删 `if: push to main`，license/source/ban 现阻塞 PR（与 audit 一致）。
  - **F5 文档治理**：CODEOWNERS `@imagent/maintainers` → `@uzziah`；v1/v2/v3 review + P1/P2/P3/PARALLEL ROADMAP 移 `docs/internal/`（不进 SUMMARY）；根 CLAUDE.md + v4/v5/README/源码注释清理内部工作流措辞 + 进度更新到 P3。
  - **F7/F8 deploy**：`deploy/README.md` 日志路径 `/tmp` → `/usr/local/var/log`（对齐 plist）+ metrics_addr 默认值纠正（默认关闭）；systemd `ReadWritePaths` 注释强调必须加 `default_workdir`。
  - **N8 崩溃当成功**：final_text 非空但未由终止事件产出 + exit 非 0 → warn 标注（不静默当成功），仍返回部分文本；`dispatch.rs` 落库判空 session_id（崩溃未及分配时不入库，防 `--resume ""` 失败）。
  - **S-5 stderr 单行 cap**（v4 半修的补齐）：`read_stderr_to_string` 改 `read_line_capped` + `MAX_STDERR_LINE_BYTES=1MiB`，对称 stdout；防 prompt injection 写无 `\n` 超长流 OOM。
  - **S-3 mcp 超时对齐**（v4 半修的补齐）：MCP server 超时从硬编码 1200s 改经 `--ask-timeout` argv 传入（= `permission_ask_timeout_secs`），跨 mcp.rs/claude/main 传递。
  - **S-6 MCP 配置原子写**（v4 半修的补齐）：`write_mcp_config` 的 check-then-write TOCTOU 改 temp+rename（`create_new` + rename 不跟随 symlink）。

- **v6（开源首发收尾 + v5 诚信核实 + 新发现，见 [v6](docs/CODE_REVIEW_v6.md)）**：
  - **核实**：逐行复核 + 实跑 `cargo test`/`clippy`/`fmt`/`fuzz check`/`audit`，v5 的 F1/F2/F4/F5/F7/F8/N8/S-5/S-3/S-6 全部真修无谎报（241 passed）。
  - **R1 崩溃语义结构化**：`RunOutcome` 加 `terminal` 字段，dispatch 在非正常终止时回复前置「⚠️ agent 异常退出」告警（N8 的 warn 升级为用户可见）。测试 241→242。
  - **R2 metrics 默认安全**：`metrics_addr` 绑非 loopback 时 warn（/metrics + /health 无鉴权，防公网信息泄漏；不含凭据）。
  - **P1 ilink 游标 at-least-once**：`fetch_updates` 游标前进移到消息处理后，crash 不再丢整批消息（重复由 dedup 吸收）。
  - **P2/P3 wecom 日志卫生**：`WsFrame` redacting Debug（subscribe secret 不落日志）；`ws_url` 日志只记 host。
  - **P4 ilink post_json 上限**：响应体 16MiB 双重校验（Content-Length 头 + 实际 bytes），防异常/恶意超大响应 OOM。
  - **P6 mcp async**：`run_mcp_server` stdin 改 tokio async（消除 async fn 内同步阻塞反模式）。
  - **文档（D1-D5/R3，见 `0d5b935`）**：README 去写死测试数 / Cargo.toml 过时注释清理 / P2_COMPLETE 移 internal / 主 README 加 macOS 撞名警告 / SECURITY 补 wecom_secret 明文 + ACP allowed_tools 无效·Off 全放行。
  - **P5/P7 文档化（不做强制代码改动）**：WeCom markdown 渲染是平台特性（现有 `proto.rs` 注释覆盖，强制转义会破坏 agent 有意格式）；ACP `IMAGENT_ACP_COMMAND` env 替换威胁有限（不加硬白名单避免误伤合法切版本用法）。

### Changed
- workspace 测试 241 passed（2 ignored）；clippy 0 warning；fmt clean；macOS + ubuntu CI 矩阵。

## [1.0.0] — 待发布（pending git tag；见 [v5](docs/CODE_REVIEW_v5.md) F3）— P3 全部完成

首个稳定版本（功能完整，待打 tag 正式发布）。P3（开源化 + 多平台 + 多后端 + 运维）全部交付。

### Added
- **平台（Platform）**：iLink（个人微信私聊）+ **WeCom**（企业微信智能机器人 WebSocket 长连接）双 Platform adapter。
- **后端（Backend）**：Claude（CLI `claude -p` + **ACP** 长驻子进程，agent-client-protocol SDK）+ **Codex**（`codex exec --json`）+ **Gemini**（`gemini -p -o stream-json`）多 Backend。
- **运维**：Prometheus 指标 + `/health` + `/metrics` + `SIGHUP` 热重载 + daemon 部署（systemd/launchd 单元）。
- **消息**：iLink `send_text` 超长自动分片（`split_message` 纯函数，不切断 UTF-8）。
- **安全**：发送者白名单、workdir 锁定、**凭据加密落盘**（OS keyring）、IM 权限审批闭环（claude CLI `--permission-prompt-tool`）。
- **会话**：SQLite 持久化、`/new` `/switch` `/sessions` `/compact`、重启续接（`--resume`）。
- **工程**：MIT license、CI（test/fmt/clippy/coverage/release/MSRV）、mdBook 文档站。

### Changed
- workspace 测试 214 passed（2 ignored）；clippy 0 warning。
- 版本 0.1.0 → 1.0.0（workspace.package 一处生效，全 crate 跟随）。

## [0.2.0] — 2026-06-30 — P2

### Added
- **A1** `sendmessage` 限流熔断：解析 ret/errcode + 滑动窗口熔断（30s/1/30s）+ 限流退避（3s≤4 次）+ 网络线性退避 + 出站串行 + session 过期透传。
- **C1** `/allow` `/disallow` `/list` `/whoami` 动态白名单 + 审计日志 + 发现态引导 + CLI `imagent allow`。
- **A2** 错误恢复：session_expired 优雅停止 + send 失败分级 + 重新 login 提示。
- **B1+B2** `/switch <name>` 多命名 session + `/sessions`（`named_sessions` 侧表 + `active_name` config KV，sessions 表不动）。
- **B3** `/compact` 软上下文压缩（claude -p 无原生 compact flag → 摘要+重置+延续）。
- **E1** 中间事件推流：stream-json `tool_use`/`tool_result` → IM「🔧 工具」摘要（聚合不刷屏）。
- **E2** typing 指示：`sendtyping`（无 msg 包装）+ `getconfig` typing_ticket 缓存（500s TTL）。
- **D1** IM 内权限审批闭环（**杀手锉**）：`--permission-prompt-tool` MCP → IM approve/deny（PermissionMode Off/Allow/Deny/Ask）。
- **F1** 媒体收发：AES-128-ECB+PKCS7 + CDN download/upload + SSRF 白名单 + key 编码不对称（入站接收 + 出站发送）。
- store v2（`allowed_senders` + `audit_log`）+ v3（`named_sessions` + config KV）。

### Changed
- workspace 测试 129 passed；clippy 0 warning。

## [0.1.0] — 2026-06-29 — P1 MVP

### Added
- iLink ↔ Claude Code 闭环网关：扫码登录 → 收私聊（文字+语音转写）→ 白名单鉴权 → `claude -p --allowedTools Read,Edit` → 捕获 session_id → 回传 → `--resume` 续接。
- 四 crate：`core`（`Platform`/`Backend` trait + `Dispatcher` + `Auth` + session 路由）、`ilink`（登录/收发文本）、`claude`（CLI backend，stream-json 解析）、`store`（sessions/sync_buf/context_tokens/credentials）。
- `/new` 命令；per-conv 串行；store 文件 0600 / 目录 0700。
