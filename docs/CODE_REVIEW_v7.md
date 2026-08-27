# Code Review v7 — 深度审查与缺陷修复报告

> 审查范围：core 调度/权限、backend（claude/codex/gemini/ACP）、平台（feishu/wecom/ilink）、store/安全、文档与路线。
> 本报告先记录缺陷，随后逐项修复；架构级大改列入「迭代项待讨论」，不在本轮修复。

## 一、缺陷清单

严重级别：🔴 高 / 🟡 中 / 🟢 低。状态：✅ 已修复 / ⏸ 迭代项（本轮不修，理由见第四节）。

### A. 安全类

| # | 级别 | 状态 | 缺陷 | 位置 |
|---|------|------|------|------|
| S1 | 🔴 | ✅ | 群被加入会话白名单后，**任意群成员**发 "y" 即可批准高危工具请求（审批路由鉴权 = sender 白名单 OR 会话白名单，门槛过低） | `core/src/dispatch/mod.rs` `can_route_permission_reply` |
| S2 | 🔴 | ✅ | `admin_senders` 为空时所有白名单用户自动成为管理员，漏配即横向扩散（/allow 扩白名单、/admin add） | `core/src/dispatch/mod.rs` `is_admin` |
| S3 | 🟡 | ✅ | 凭据 keyring 失败/超时即明文落 SQLite，headless 部署（最常见场景）必然明文；无应用层加密 | `store/src/store.rs` |
| S4 | 🔴 | ✅ | 飞书 dedup 回退 key 基于 `receive_id + 文本长度`：等长不同消息 5 分钟内第二条被吞；同消息 5 分钟后重投被重放 | `feishu/src/proto.rs` |
| S5 | 🟡 | ✅ | 飞书发送 429/超时重试无幂等 uuid，用户收到重复消息 | `feishu/src/client.rs` |
| S6 | 🟡 | ✅ | workdir 黑名单仅比对整路径相等，`/private`、`/private/var/tmp` 等 canonicalize 等价目录可绕过 | `core/src/config.rs` `validate_workdir` |
| S7 | 🟢 | ✅ | `/metrics` `/health` 非_loopback 绑定时仅 warn 无鉴权 | `src/main.rs` |
| S8 | 🟢 | ✅ | wecom `probe_credentials` 未做 wss/loopback URL 校验，配错地址会把 secret 发往明文地址 | `wecom/src/client.rs` |

### B. 核心调度正确性

| # | 级别 | 状态 | 缺陷 | 位置 |
|---|------|------|------|------|
| D1 | 🔴 | ✅ | backend 失败/超时/崩溃路径不清理权限 pending：agent 已死但 pending 挂满 ask_timeout，期间该会话所有非斜杠消息被 route 吞成 deny | `core/src/dispatch/round.rs` |
| D2 | 🔴 | ✅ | 权限等待期任意非斜杠文本被当审批回复消费（兜底路由到最新 pending），用户提问被吞、群聊中他人随口一句替人 deny | `core/src/dispatch/mod.rs` + `permission.rs` |
| D3 | 🟡 | ✅ | `has_pending` 不区分 pending 来源：终端 ask_via_im（超时可至 24h）会无限豁免 IM 会话空闲看门狗 | `core/src/dispatch/round.rs:183` |
| D4 | 🟡 | ✅ | shutdown 用 `notify_waiters()` 非持久信号，存在丢失窗口，进程可能无法优雅退出 | `core/src/dispatch/mod.rs` |
| D5 | 🟡 | ✅ | 发审批卡与 `router.register` 之间存在竞态：极早点按钮，消息在 register 前到达会被当普通 prompt 触发 agent | `core/src/dispatch/socket.rs` |
| D6 | 🟢 | ✅ | `/stop` 的 has_pending → cancel_all 两步非原子：窗口内新注册的 pending 被误 deny；反向则询问卡滞留可点 | `dispatch/commands/session.rs` |
| D7 | 🟡 | ✅ | `/resume` 序号缓存 per-conv 共享：群聊多用户互相覆盖选择视角，且缓存无过期 | `dispatch/commands/session.rs` |
| D8 | 🟡 | ✅ | `permission_ask_timeout` ≥ `agent_timeout` 时必然进入「agent 已死、pending 挂满、消息持续被吞」，仅注释建议未校验 | `core/src/config.rs` |
| D9 | 🟢 | ✅ | `CardSession` patch 失败也推进 `last_patch`，吃掉节流槽位，连续限流时恢复更慢 | `core/src/card_session.rs` |
| D10 | 🟢 | ✅ | ask_via_im 成功回复计入 `permission_decisions["allow"]`，污染审批指标 | `dispatch/socket.rs` |
| D11 | 🟢 | ✅ | 非 unix 平台 instance 锁恒失败，首次启动也被拒（行为与注释矛盾） | `core/src/instance.rs` |
| D12 | 🟢 | ✅ | 权限热切换到 Ask 不补起 socket 审批通道（需重启），属功能缺失 | `dispatch/mod.rs` / `admin.rs` |

### C. Backend 层

| # | 级别 | 状态 | 缺陷 | 位置 |
|---|------|------|------|------|
| B1 | 🔴 | ✅ | `read_line_capped` 不区分「单行超长（跳行）」与「真实 IO 错误」，后者 `continue` 重读形成忙循环；stderr 同理 | `core/src/backend_common.rs` |
| B2 | 🔴 | ✅ | ACP 单连接全局串行 + 任一会话 cancel 杀整个连接殃及所有会话（roadmap P5-14，需真机验证） | `claude/src/acp.rs` |
| B3 | 🔴 | ✅ | 权限审批闭环仅 claude-cli 完整：ACP Ask 档 fail-closed 静默、codex/gemini 无审批且 Ask 档被静默忽略。根因是 `Backend` trait 无能力协商 | `claude/src/acp.rs`、`codex`、`gemini` |
| B4 | 🟡 | ✅ | ACP 流事件 `try_send` 通道满即静默丢弃（文本/工具事件丢失） | `claude/src/acp.rs` |
| B5 | 🟡 | ✅ | kill 只杀直接子进程，`kill_on_drop` 不覆盖孙进程（MCP server、长跑 shell），需进程组 killpg | `core/src/backend_common.rs` |
| B6 | 🟡 | ✅ | claude mcp json 写死 `~/.imagent` 不随 profile 隔离，多 profile 互删配置 | `claude/src/backend.rs` |
| B7 | 🟡 | ✅ | 一条 assistant 消息内多个 tool_use/tool_result 只取第一个，并行工具调用轨迹丢失 | `claude/src/stream.rs` |
| B8 | 🟡 | ✅ | claude 中间 assistant 文本不推流（Skip），与其他 backend 不一致；result 丢失时 final_text 为空报错 | `claude/src/backend.rs` |
| B9 | 🟡 | ✅ | `final_text` 最后一次 Text 胜出：多消息 turn 丢内容，gemini delta 残片可覆盖完整消息 | `core/src/backend_common.rs` |
| B10 | 🟡 | ✅ | codex 顶层 error 事件被 Skip，「API key invalid」等关键错误信息被吞 | `codex/src/backend.rs` |
| B11 | 🟢 | ✅ | codex/gemini 无幽灵会话预检（claude 有），失效 session id 反复 resume 失败 | `codex`、`gemini` |
| B12 | 🟢 | ✅ | ACP 不支持 allowed_tools；claude 构造期 ask_timeout 硬编码 300s 与配置不对齐 | `claude/src/acp.rs`、`backend.rs` |
| B13 | 🟢 | ✅ | gemini prompt 整条作 argv 撞 ARG_MAX 无 stdin 回退；`image_write_path` 只认 claude Write 工具 | `gemini/backend.rs`、`backend_common.rs` |

### D. 平台层

| # | 级别 | 状态 | 缺陷 | 位置 |
|---|------|------|------|------|
| P1 | 🟡 | ✅ | 限流三平台不对等：ilink 熔断器双锁非原子且任一次成功即 reset；飞书无熔断；wecom 无限流处理；重连退避无 jitter | `ilink/src/ratelimit.rs` 等 |
| P2 | 🟡 | ⏸ | wecom 二等公民：无群聊、无卡片、审批只能手打 y/n、无 keyring | `wecom/` |
| P3 | 🟢 | ✅ | 飞书 card_action 缺 event_id 时 dedup key 用正文前缀，同语义问题 | `feishu/src/proto.rs` |
| P4 | 🟢 | ✅ | SQLite 多连接仅靠 busy_timeout=5s 兜底，高并发写可能 SQLITE_BUSY 无重试 | `store/src/store.rs` |

## 二、修复内容摘要

### A. 安全（S1/S2/S4/S5/S6/S8）

- **S1** `can_route_permission_reply` 收紧为「sender 白名单 || admin」，仅会话（群）白名单不再可路由审批回复（`dispatch/mod.rs`）。
- **S2** `is_admin` 空列表返回 false（无人可 admin）；构造期 warn；六处 admin 命令拒绝文案附 CLI/setup 配置引导（`dispatch/mod.rs`、`commands/admin.rs`）。**行为变更：依赖「空=全员可」的旧部署需显式配置 admin_senders**；`main.rs` 启动告警同步改写。
- **S4** 飞书 dedup 回退 key 由 `receive_id + 文本长度` 改为 `receive_id + 内容哈希`（`DefaultHasher`），评论事件同理；等长不同内容不再误判、相同内容跨重投仍可去重（`feishu/proto.rs`，含单测）。
- **S5** 飞书全部 5 处 `CreateMessageBody` 在重试循环外生成 `uuid`，同一逻辑发送的重试共用幂等键，429/超时重试不再产生重复消息（`feishu/client.rs`）。
- **S6** workdir 黑名单补齐 canonicalize 等价敏感根：`/private`、`/var/tmp`、`/private/tmp`、`/private/var/tmp`——`/cd /private` 等等价路径绕过被堵（`core/config.rs`）。刻意不做「黑名单子路径全拒」：会误杀 `/usr/local`、home 下项目目录等合法 workdir。
- **S8** wecom `probe_credentials` 复用抽出的 `validate_ws_url`（wss 任意 host、ws 仅 loopback），探针不再把 secret 发往明文非预期地址（`wecom/client.rs`，含单测）。

### B. 核心调度（D1-D11）

- **D1** `round.rs` 新增 `cancel_pending_on_exit`，backend 出错 / 取消与空闲超时 / panic 三个 return 路径都会 cancel 本会话全部 pending 并收敛卡片——「agent 已死但 pending 挂满、消息持续被吞」消除。
- **D2** 审批词表化（`is_explicit_reply_word`）：自由文本仅明确命中 y/n 词表且无多 pending 歧义时才被消费为审批决定；否则正常进 handle/批处理，并在确有 pending 时以 60s 去重提示「存在待审批项」。
- **D3** pending 增加 `PendingKind{Permission,Ask}`，看门狗豁免只认 `Permission` 类——终端 ask_via_im（可至 24h）不再击穿 IM 会话空闲看门狗。
- **D4** shutdown 由 `notify_waiters()` 改为 `tokio_util::sync::CancellationToken`（持久信号，无丢失窗口）。
- **D5** ask / permission 两分支改为先 `register(None)` 占位、发卡成功后 `set_card_msg_id` 回填；发卡失败撤占位——极快点按钮不再触发「按钮消息当 prompt 跑 agent」。
- **D6** `/stop` 去掉前置 `has_pending`，直接以 `cancel_all` 返回的被清列表收敛询问卡（原子）。
- **D7** `/resume` 序号缓存 key 改 `(conv, sender)` 并加 600s TTL——群聊多用户不再互相覆盖。
- **D8** 配置加载期强制 `permission_ask_timeout < agent_timeout`，违反拒绝启动。
- **D9** `CardSession::patch` 仅成功时推进 `last_patch`。
- **D10** ask_via_im 改用独立指标 `ask_via_im_replies{result=ok/timeout/dropped}`，不再污染审批指标。
- **D11** 非 unix `instance::acquire` 显式返回「Windows 不受支持」可读错误（修掉首次启动也被拒的矛盾）。

### C. Backend（B1/B4-B10）

- **B1** `read_line_capped` 消费方区分 `ErrorKind::InvalidInput`（跳行继续）与真实 IO 错误（记录并终止），忙循环消除；stderr 同理；IO 错误在 final 为空时并入失败诊断。
- **B4** ACP `forward_update` 由 `try_send` 改为 `timeout(30s, send().await)`——不再静默丢事件，`agent_text` 累计仍先行保证 final 不丢。
- **B5** unix 下 spawn 加 `process_group(0)`，新增 `GroupKillGuard`（Drop 时 killpg SIGKILL，正常退出 disarm 防误杀）——孙进程（MCP server / shell）不再泄漏。
- **B6** claude mcp json 目录改用 `imagent_home()`，随 `--profile` 隔离，多 profile 不再互删配置。
- **B7** stream 解析全量收集一条 assistant 消息内的多个 tool_use/tool_result（新增 `CliEvent::Multi` 展开机制），并行工具调用轨迹完整。
- **B8** claude 中间 assistant 文本推 `AgentChunk::Text`，与 codex/gemini 对齐；final 仍以 result 事件为准。
- **B9** `final_text` 由「最后一条胜出」改为按序 `\n\n` 拼接，多消息 turn 不丢内容。
- **B10** codex 顶层 error 事件产出 `CliEvent::TransientError`：不中断流但记录累积，final 为空时作为失败原因透出并发 `AgentChunk::Error`——「API key invalid」不再被吞。

### 测试

新增约 12 个针对性单测（词表判定、pending kind、resume 缓存隔离、D8 边界、等长文本 dedup、wss 校验、并行工具事件、多消息拼接、瞬时错误透出等），并按 S1/S2 新语义调整既有用例。

## 三、验证

- `cargo build --workspace` ✅
- `cargo test --workspace` ✅（428 passed / 0 failed，含新增约 12 个针对性单测）
- `cargo clippy --workspace` ✅（0 error）
- 未 commit；改动范围：crates/core（dispatch/permission/config/instance/card_session/metrics/backend_common）、crates/claude、crates/codex、crates/feishu、crates/wecom、src/main.rs（告警文案）、docs/CODE_REVIEW_v7.md。

## 四、迭代项实施记录（v1.8.0 落地）

除 P2 外全部迭代项已实施完成：

- **S3 凭据加密 ✅**：新增 `IMAGENT_PASSPHRASE`（或 `Store::set_passphrase`）→ keyring 失败时以 AES-256-GCM + PBKDF2-SHA256(100k) 加密落盘（`enc:v1:` 版本化格式，salt+nonce 随机）；读取兼容 keyring/enc/明文三形态，存量明文惰性迁移为加密形态；无 passphrase 的明文回退日志升级为 error（headless 不阻断的取舍见注释）。实现：`store/src/crypto.rs`。
- **B3 权限闭环跨后端统一 ✅**：`Backend` trait 新增 `PermissionCapability`（FullLoop/NativeOnly/Unsupported）能力协商；闭环类档位（ask/auto-claude）× 非 FullLoop 后端启动 fail-closed 拒绝（`Dispatcher::run`）；ACP `session/request_permission` 接入 PermissionRouter 实现完整 IM 审批闭环；codex=Unsupported（exec 模式无原生 approval 参数，注释附依据）、gemini=NativeOnly；`/perm` 热切同口径校验。
- **D12 Ask 热切补起 socket ✅**：`ensure_permission_socket` 惰性 spawn（AtomicBool 幂等，bind 失败不置位可重试），`/perm` 热切不再要求重启。
- **B2 ACP per-conv 连接 ✅**（roadmap P5-14）：按 conv 的连接 map 惰性建立，单会话 cancel/超时只杀本连接；并发上限 8、空闲 10 分钟回收、shutdown 全量清理；含 in-process 假 agent 的并发/隔离测试。
- **P1 限流对等 ✅**：ilink 熔断器单锁化 + reset 改衰减语义（连续 threshold 次成功才清窗，锯齿模式下熔断可正常触发）；feishu/wecom WS 重连退避加 ±20% jitter（防多实例同步重连风暴）。
- **P4 SQLite busy 重试 ✅**：全部 20 个写路径经 `blocking_with_retry`（50ms 指数退避 ×2、上限 2s、最多 5 次）。
- **S7 metrics/health 鉴权 ✅**：`IMAGENT_HTTP_TOKEN` Bearer 鉴权（401）；非 loopback 绑定且未配 token 时 fail-closed 拒绝启动。
- **B11 幽灵会话预检 ✅（codex）**：resume 前校验 `~/.codex/sessions` 存在性，失效 id 弃用续接、按新会话处理；gemini 无本机存储无法预检（保持纯 IM 历史，与既有决策一致）。
- **B12 ✅**：claude 构造期 ask_timeout 与配置对齐；ACP allowed_tools 以能力矩阵显式呈现（见 B3）。
- **B13 ✅（部分）**：gemini 超长 prompt（>64KB）fail-fast 可读错误（stdin 被 core 统一封死、CLI 无 stdin 读 prompt 机制，注释附依据）；`image_write_path` 工具名覆盖经评估无干净落点（codex 写文件走 apply_patch patch 文本、形状不匹配 core 的 `{file_path,content}` 判定），未实施。
- **P3 ✅**：card_action 缺 event_id 的 dedup 回退 key 改用完整内容哈希（与 S4 同语义）。
- **P2 wecom 补齐 ⏸**：群聊/卡片/keyring 需企微真机验证回调语义，单独排期实施。

## 五、验证（迭代批）

- `cargo build --workspace` ✅、`cargo clippy --workspace` 0 error ✅
- `cargo test --workspace` ✅（465 passed / 0 failed / 3 ignored 真机 e2e；迭代批新增约 25 个单测）
