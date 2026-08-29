# Code Review v8 — 深度审查与缺陷修复跟踪

> **审查对象**：`imagent v1.14.1` @ `98ae037`（main）。
> **审查范围**：`crates/{core,feishu,claude,codex,gemini,ilink,wecom,store}` + `src/{main,service,setup}.rs`（约 3 万行核心代码）。
> **审查方法**：4 路并行子审查（① core 调度/权限/socket ② feishu 全 crate ③ claude/codex/gemini 后端 + ACP ④ store/config/main/ilink/wecom）+ 主会话对全部高/中危逐条读源码核验（含 vendored SDK `agent-client-protocol-1.0.1` 源码与 open-lark 0.20 源码核对）。基线实测：`cargo build --workspace` ✅、`cargo test --workspace` = **620 passed / 0 failed / 3 ignored**（真机 e2e）。
> **与 v4–v7 的关系**：全部为增量问题（v7 已修项不重复）；v7 报告中已知未修项（P2 wecom 补齐等）不在本清单。
> **修复约定**：本清单**全部计划修复**，在独立 worktree 分支 `fix/code-review-v8` 中进行；修复中遇到需要决策的点统一记入 §五「待对齐决策点」，最后与 owner 对齐，不得擅自跳过。

## 🎯 核心洞察（共性模式）

v7 之后的修复**没有同步覆盖新路径**——同一威胁模型/防线只落在旧调用点上，新路径各自绕开：

| 防线 | 旧路径（已覆盖） | 新路径（本轮发现缺口） |
|---|---|---|
| S-2 env 消毒 | `spawn_cli_backend`（3 个 CLI 后端） | H2：ACP 经 SDK spawn，全量继承 |
| S-5 IO 上限 | CLI stdout 8MB/stderr 64KB | M2：ACP stdout/stderr 无上限 |
| permission_mode 策略 | legacy MCP 通道（`needs_socket` 分流） | H1：canUseTool 控制通道不感知 mode |
| 数值配置校验 | 启动期 `Config::load` | H3/L5：`message_max_len`、`/timeout`、`/config batch_window_ms` 热改漏网 |
| HTTP client 纪律 | SDK 路径共享 client + WS 30s 超时 | M1/M6：手写 13 处裸 `Client::new()` 且无超时 |

修复完成定义（DoD）：每修一项，**grep 全部同类调用点**确认无残留（v8 遗留的 `encrypted_query_param` 只修 upload 不修 download、Import chmod 而 export 不 chmod，均属此类欠账）。

## 一、缺陷清单

严重级别：🔴 高 / 🟠 中 / 🟡 低。状态：⬜ 待修。

### A. 安全类

| # | 级别 | 状态 | 缺陷 | 位置 |
|---|------|------|------|------|
| H1 | 🔴 | ⬜ | canUseTool 控制通道不感知 permission_mode：`/perm deny\|off` 热切后审批闭环未拆，白名单用户点「允许」即绕过 deny 策略（fail-open）；且 `allow` 模式 + 缺省 control 通道下 socket 永不 bind，canUseTool 全部连接失败被 deny，**allow 退化成 deny**（与 legacy mcp 通道行为分叉） | `core/src/backend_common.rs:346-369`、`core/src/dispatch/socket.rs:583-624`、`core/src/dispatch/mod.rs:689-715`；对照正确实现 `core/src/mcp.rs:292-318` |
| H2 | 🔴 | ⬜ | ACP 子进程继承父进程**全部**环境变量——S-2 env 消毒未覆盖 ACP 路径；`claude-agent-acp` 内部再 spawn 的 claude CLI 同样继承，agent 可经 `Bash env` / `/proc/self/environ` 读走部署环境全部秘密并回传 IM | `claude/src/acp.rs:233-245` + SDK `agent-client-protocol-1.0.1/src/acp_agent.rs:159-175`（只 `cmd.env()` 增量、无 env_clear） |
| H3 | 🔴 | ⬜ | wecom 分片死循环：`message_max_len ≤ 3` 时 `split_text_by_bytes` 零前进死循环 + `chunks` 无限追加（OOM + CPU 满核）；config 注释「不设 = 仅用协议上限」诱导用户填 0。ilink/feishu 均有守卫，唯 wecom/config 层无下界校验 | `wecom/src/platform.rs:41-69` + `core/src/config.rs:274`（`Config::load` 无校验） |
| M4 | 🟠 | ⬜ | ACP 无 title 的权限请求：`tool_name_of` 回退 `tool_call_id`（如 `tc-01`）→ 审批集语义「清单外放行」→ **无询问自动放行**（W2-3 同族残余，应 fail-closed） | `claude/src/acp.rs:838-846` + `core/src/dispatch/socket.rs:610-617`（`needs_approval` 未命中即放行） |
| L8 | 🟡 | ⬜ | 卡片 markdown 未转义用户可控文本：`<at id=…>` 可借 bot 流式卡 @ 任意租户用户、`[text](href)` 以 bot 名义展示链接（JSON 层 serde 已封死，仅 markdown 语义层；v4 曾因「破坏格式风险」文档化搁置） | `feishu/src/card.rs:194`（`body_md`）、`:501-542`（`stream_body_md`） |
| L11 | 🟡 | ⬜ | `profile export --include-secrets` 产物未 chmod 0600——明文 secret 按 umask（典型 0644）落盘世界可读；Import 分支有 0600，export 没有 | `src/main.rs:391-393` |
| L12 | 🟡 | ⬜ | ilink CDN **下载** URL 的 `encrypted_query_param` 未 percent-encode（upload 路径 P2-13 已修，download 漏修——同类问题修一半） | `ilink/src/media.rs:191-195` 对照 `:324-329` |
| L13 | 🟡 | ⬜ | `/model` 值拼进 ACP spawn 命令串后过 `shell_words::split`——模型名含空格/引号可拆出多余 argv、改变 spawn 行为（需 admin 触发） | `claude/src/acp.rs:239-241` |
| L14 | 🟡 | ⬜ | metrics/health 明文 HTTP：非 loopback 部署下 Bearer token 过链路可被嗅探；`==` 逐字节短路比较非恒定时间（注释自知） | `src/main.rs:1180-1188` |

### B. 资源与可用性

| # | 级别 | 状态 | 缺陷 | 位置 |
|---|------|------|------|------|
| M1 | 🟠 | ⬜ | 飞书全部出站 HTTP 无超时（SDK `core_config` 无 `req_timeout` + 13 处手写裸 `Client::new()`），叠加 `fetch_cached_token` **持写锁跨网络调用**：token 刷新遇连接黑洞 → 写锁永不释放 → 全进程发送/下载永久阻塞；drain task 串行，一次下载挂起 → 所有会话消息/审批无界排队 | `feishu/src/platform.rs:195-209`（`core_config`）、`:1750-1757`（token 锁）、`feishu/src/client.rs` 13 处 |
| M2 | 🟠 | ⬜ | ACP 路径 stdout/stderr 无大小上限（SDK `lines()` 无行长上限、stderr 连接生命周期内无上限累积）——agent `cat` 大文件即 OOM 面 | SDK `acp_agent.rs:274-299`；需 imagent 侧带 cap 的代理 transport 或上游修复 |
| M3 | 🟠 | ⬜ | Control 通道审批**内联 await 最长 300s** 在 stdout 读循环里：并行工具的多个 canUseTool 串行化成串行 IM 审批，期间子进程输出滞留管道；且读循环 break 后 `stdin_w` 未 drop 即 `child.wait()`——CLI 若等 stdin EOF 才退出，成功轮次挂到看门狗 SIGKILL、被报失败 | `core/src/backend_common.rs:346-397`（内联 ask）、`:270`（take 后无 drop）、`:502`（wait） |
| M5 | 🟠 | ⬜ | 飞书 WS 重连退避对「正常断开」不重置：服务端按 PingInterval 例行踢空闲连接走 `ConnectionClosed` 分支（不重置 backoff），累计 5 次后每次断线等 24-36s 才重连且**永不回落**；进程跑得越久偶发 30s 无响应越频繁 | `feishu/src/client.rs:66-94`（P1 修了 jitter 漏了这一半） |
| M6 | 🟠 | ⬜ | 13 处 `reqwest::Client::new()` 每请求新建：流式卡 patch 高频路径（约每秒 1 帧）每帧做完整 TCP+TLS 握手、用完即弃——跨境多 100-200ms RTT、大量 TIME_WAIT、更快触碰建连速率限制 | `feishu/src/client.rs:189/231/273/459/501/526/564/677/734/986/1190/1264/1310` |
| L1 | 🟡 | ⬜ | `patch_managed` 终态不清理 `card_seqs`/`card_footers`（im-patch 路径注释声称「与 patch_managed 同语义」——那个清理不存在）：凡终态走 patch_managed 的卡（发过询问卡/带审批的常态轮次、重启后孤儿卡）都泄漏 2 个 map 条目，**无 cap 无过期** | `feishu/src/platform.rs:991-1027` 对照 `:2642-2645` |
| L3 | 🟡 | ⬜ | 排队表情竞态：recv 侧 ⏳ Queued 与 runner 侧 👀 OnIt 对同一消息并发执行「删旧表情→网络→打新表情」，交错时同消息挂两个表情、**⏳ 永久残留**在已完成消息上 | `feishu/src/platform.rs:1783-1815`；core 侧同类场景有 S-3/S-4 兜底（`mod.rs:1258-1263`），表情路径没有 |
| L9 | 🟡 | ⬜ | managed 流式 `md_body` 全量无截断重传：每节流窗上传累积全文——长输出 O(n²) 上传流量，超 ~30KB 元素上限后 Running 帧持续失败、流式卡中途死亡（有纯文本兜底，内容不丢） | `feishu/src/card.rs:501-543` 对照 `:34`；core `card_session.rs` 仅 thoughts/todo 有上限 |
| L15 | 🟡 | ⬜ | `socket_spawned` 幂等位 check-then-act：`load(Acquire)` 为 false 即走完整 bind 流程，run() 启动路径与 SIGHUP `reload_permission_mode` 并发可双 bind（后到者 EADDRINUSE 误报失败 / 交错时留不可达孤儿 listener）；应改 `compare_exchange` | `core/src/dispatch/socket.rs:13-19/163-166` |

### C. 调度与协议正确性

| # | 级别 | 状态 | 缺陷 | 位置 |
|---|------|------|------|------|
| M7 | 🟠 | ⬜ | ACP `UserMessageChunk` 被当 agent 文本推流（`AgentChunk::Text`）：每个 ACP 轮次 IM 流式卡以**用户自己刚发的话开头**（`final_text` 不受影响，纯展示层污染，每轮必现） | `claude/src/acp.rs:704`（对照 `:678` 注释） |
| L2 | 🟡 | ⬜ | `PENDING_PER_CONV_CAP` 淘汰最旧 pending 时只 `tx.send(deny)`，不收敛平台侧 `pending_asks`：残留条目无上限累积 + 过期卡点「允许」→ route miss → 字面 `"y"` 被当 prompt 跑一轮 agent | `core/src/permission.rs:402-410` + `core/src/dispatch/mod.rs:865-887`（Replied 分支无平台收敛）+ `feishu/src/platform.rs:1059-1094` |
| L4 | 🟡 | ⬜ | 排队提示 S-3/S-4 复查守卫写反：守卫是 `get(conv).is_none()` 才撤 stale hint，但 runner 取批不删 entry（留空 Vec）→ 网络慢时 hint 在清理后写入且永不撤回，footer 显示错误排队数；应 `is_none_or(\|q\| q.is_empty())` | `core/src/dispatch/mod.rs:1262-1266` 对照 `:1301-1313` |
| L5 | 🟡 | ⬜ | 热改数值无上限：`/timeout <分钟>` 任意白名单用户可发，`n * 60` 整型溢出（debug panic / release 回绕成任意值自 DoS）；`/config batch_window_ms` 热改无 `BATCH_WINDOW_MAX_MS` 校验（启动有、热改没有）——设巨值 runner 永睡且不在 running 注册表、`/stop` 救不回、conv 锁永久持有 | `core/src/dispatch/commands/misc.rs:544-548` + `admin.rs:717-722` |
| L6 | 🟡 | ⬜ | 终端问答（ask_via_im）回复被计入审批审计且 `parse_decision` 把 `ask:<选项>` 归为 deny——`/stats` 审批 allow/deny/timeout 占比失真 | `core/src/dispatch/mod.rs:1046-1079` + `misc.rs:864-868`（聚合源） |
| L7 | 🟡 | ⬜ | `/cd` 在 ACP 下 10 分钟 idle 窗口内不生效：`loaded` 会话缓存只比 session id 不比 cwd，跳过 LoadSession 时新 cwd 不传递——与用户收到的「已切到 X，下条消息生效」承诺相悖（CLI 后端每轮设 `current_dir` 无此问题） | `claude/src/acp.rs:437-439` |
| L16 | 🟡 | ⬜ | Control/MCP 通道 `always` 语义丢失：`ask_via_socket` 构造回复恒 `always: false` 不解析回传，`updatedPermissions` 分支恒为死代码（claude 每次重发 control_request）；顺带 client/server 超时起算点不一致（server 从 register、client 从 connect 起算，发卡慢时用户点「允许」落空） | `core/src/mcp.rs:242-247` + `socket.rs:233-236` + `backend_common.rs:210-214` |
| L17 | 🟡 | ⬜ | 询问卡 note 三方并发 patch 无版本控制：`note_queued_on_ask` 快照后跨网络全卡重渲染，与终态 patch 次序不定——终态卡可被翻回带按钮的 pending 态（点击有过期兜底，无安全影响但误导）+ 重渲染取当前时间戳等于把 24h 按钮 TTL 重新起算 | `feishu/src/platform.rs:2299-2351` 竞态 `:2259-2293` |
| L10 | 🟡 | ⬜ | `instance.lock` open 即 truncate：第二实例把第一实例写入的 PID 清空后再读，flock 失败的诊断恒为「pid 未知」——「写 PID 供排障」设计落空（互斥本身不受影响） | `core/src/instance.rs:36-54` |

## 二、缺陷详情与修法建议

### H1 canUseTool 控制通道不感知 permission_mode

**机理**（主会话逐行核验）：`claude_permission_channel = "control"` 为缺省值。此通道下 claude 权限请求不经 `run_mcp_server`（那里才有 `fixed_reply(Allow/Deny)` 的 mode 分流），由 `spawn_cli_backend` 直接 `ask_via_socket` 打到 permission.sock；dispatcher 侧 `handle_permission_kind_socket` 只看 `approval_tools` 与 `session_allows`。而 `claude_native_perm_args` 对 Allow/Deny 档**不加**原生 flag（`backend.rs:196-206`），claude 走自身默认门禁照常发 canUseTool；socket accept task 一旦 spawn 即运行到进程退出（select 只挂 shutdown），`reload_permission_mode` 切 deny/off 时既不校验也不回收。

**触发矩阵**：
1. ask 启动 → `/perm deny` / `/perm off` / SIGHUP 改 deny → socket 仍监听 → 审批卡照发 → 白名单用户批准即放行（**deny 被绕过**，`cmd_perm` 注释声称的「热切 off 即拆掉审批闭环」未兑现，admin.rs:786-788）。
2. 冷启动 `permission_mode = "allow"` + control 通道 → socket 永不 bind（`run()` 仅 `needs_socket()` 时 ensure，mod.rs:978）→ `ask_via_socket` 连接失败 → 需审批工具全 deny（**allow 退化成 deny**，与 legacy mcp 通道行为分叉）。

**修法建议**：把 mode/approval_tools/session_allows 判定收敛为共享「策略决策点」函数（MCP / control / ACP hook 三通道共用），handler 注入 `Arc<RwLock<PermissionMode>>`：Allow→固定放行、Deny/Off→固定拒绝；热切出闭环档位时停 accept task（生命周期跟随 `needs_socket()`）。补三个测试：热切 deny 后 socket 请求被拒、allow 模式 control 请求放行、三通道策略一致。

### H2 ACP 子进程继承全部环境变量

**机理**（已核对 SDK 源码）：`AcpAgent::from_str(&cmd)` → SDK `spawn_process` 用 `async_process::Command` 只做 `cmd.env(name, value)` 增量，无 `env_clear`；imagent 侧 acp.rs 无任何 env 处理。CLI 路径的 S-2 威胁模型（部署环境 `DATABASE_URL`、CI secret、其它工具 token 被 agent 读取回传）在 ACP 路径完全敞开。

**修法建议**：SDK 支持 `NAME=value` 前导 env 语法且 `from_str` 走 `shell_words::split` 后直接 exec——把默认命令构造为 `env -i PATH=... HOME=... ANTHROPIC_API_KEY=... claude-agent-acp`（`env` 作为被 exec 程序），复刻 S-2 白名单语义。白名单范围见 §五 D-1。

### H3 wecom 分片死循环

**机理**（已用脚本复刻控制流验证）：`max_bytes = 0` → `end = start`，`is_char_boundary(start)` 恒真，push 空串后 start 不前进；`max_bytes ∈ {1,2,3}` + 多字节字符 → 边界回退到 start 同样零前进。`wecom_split_cap` 只 `min(4000)` 无下界，`Config::load` 对 `message_max_len` 无校验（同类键 `batch_window_ms`/`permission_ask_timeout_secs` 均有启动期校验，唯此键漏网）。三平台对照：ilink `Some(n) if n > 0` 守卫（`ilink/platform.rs:592-593`）、feishu 走 core `split_message`（0 值整条返回，`core/message.rs:29-32`）。

**修法建议**：config 层统一（0 视为 None 或报错，三平台语义对齐），`wecom_split_cap` 兜底 clamp ≥ 4；补 0/1/2/3 值单测。

### M1 飞书出站 HTTP 无超时 + token 写锁跨网络调用

**机理**（已核验 openlark 源码 `config/mod.rs:117` 默认 `req_timeout: None`、`request_execution/mod.rs:85` 仅 Some 时设置）：`ws_config` 带 30s、`core_config` 不带；13 处手写 `reqwest::Client::new()` 无 `.timeout()`。`fetch_cached_token` 读锁快路径仅缓存有效时可用，TTL 到期后写锁内 `fetch_token().await`——挂起即全局停摆。

**修法建议**：模块级共享带 `timeout`/`connect_timeout` 的 `reqwest::Client`（一并解决 M6），`core_config` 补 `.req_timeout(30s)`；token 刷新可加一层带超时兜底（fetch 本身受 client 超时保护即可）。

### M2 ACP stdout/stderr 无上限

**机理**：SDK 读 stdout `BufReader::lines()` 无行长上限（`ToolCallUpdate.raw_output` 可为任意大单行 JSON）；stderr `collected: String` 在连接生命周期（≤10 分钟 idle 窗）内无上限累积。CLI 路径的双层上限（单行 8MB / stderr 总量 64KB）防的正是「prompt injection 构造超长流 OOM」，ACP 全部绕开。SDK 无封顶 API。

**修法建议**：见 §五 D-2（带 cap 的代理 transport vs 上游修复 vs 文档化）。

### M3 控制通道内联审批 + stdin 生命周期

**机理**：读循环事件处理内 `ask_via_socket(...).await`（最长 `ask_timeout` 缺省 300s）——claude 并行工具的多个 canUseTool 串行化，期间子进程其它输出滞留 ~64KB 管道，极端时子进程写管道阻塞；读循环 break 后 `stdin_w`（`backend_common.rs:270` take，全文件无 drop）在 `child.wait().await` 期间仍打开——`--input-format stream-json` 下 CLI 若等 stdin EOF，成功轮次挂到空闲看门狗被 SIGKILL、报为失败。

**修法建议**：control_request 拆出并发 responder（独立 task 或先收集后统一答复的流水线）；读循环 break 后无条件 `drop(stdin_w)` 再 wait（零成本消除不确定性）。真机校准清单加「多 control_request 并发」「result 后是否需要 stdin EOF」两项。

### M4 ACP 无 title 权限请求 fail-open

**机理**：`session/request_permission` 的 `tool_call` 是 `ToolCallUpdate`，协议无工具名字段，仅人读 `title`；`tool_name_of` 取 title 首 token（W2-3 修复），title 缺失回退 `tool_call_id`（单测 `tool_name_takes_first_token_of_title` 固化了 `tc-9` 回退）。审批集语义是「清单外放行」（空集 = 全部过审）→ `needs_approval(["Bash"], "tc-01") == false` → 无询问自动放行。

**修法建议**：title 缺失（或回退到 id 形态）时按「必须过审」处理（视为命中审批集），fail-closed；改 `tool_name_takes_first_token_of_title` 断言语义。

### M5 WS 重连退避不因健康连接重置

**机理**（已核对 open-lark 文档：正常 Close/空闲超时也走 Err，`Ok(())` 生产几乎不可达）：`ConnectionClosed` 分支不重置 backoff，例行踢连接累计翻倍至 30s 封顶后永不回落。

**修法建议**：连接存活时长 ≥ 阈值（建议 60s，见 §五 D-6）再断开时重置 backoff 为 1s。

### M6 13 处裸 `Client::new()`

**修法建议**：模块级 `OnceCell<reqwest::Client>`（与 M1 同一 client），删除 13 处局部构造。

### M7 `UserMessageChunk` 回显

**机理**：ACP 协议中该通知是 agent 回放用户消息；映射 `AgentChunk::Text` 后每轮流式卡以用户原话开头。`final_text` 只累计 `AgentMessageChunk`（acp.rs:693-699）不受影响。

**修法建议**：映射改为忽略（或独立 echo chunk 不进流式卡）。

### L1 `patch_managed` 终态泄漏 card_seqs/card_footers

**机理**：im-patch 终态分支（platform.rs:2641-2645）有清理且注释引用 patch_managed「那边的清理」——实际不存在。`buried=true`（本轮发过询问卡即置位，带审批轮次是常态）与重启后孤儿卡都走该路径泄漏；两 map 无 cap（`msg_reactions`/`managed_card_msgs` 有 1024 cap）。

**修法建议**：patch_managed Done/Error 分支补同样清理；顺带给两 map 加对齐的 cap（与「无界 map 上限」59e17b5 意图一致）。

### L2 PENDING_PER_CONV_CAP 淘汰不收敛平台侧

**机理**：淘汰路径 `tx.send(deny)` → 等待方收到 `Replied`，`mod.rs:865-887` 只有 TimedOut/Dropped 才调平台收敛 → feishu `pending_asks` 残留、卡片保持可点；之后点「允许」route miss 回落正常消息路径，字面 `"y"` 当 prompt 跑 agent。

**修法建议**：淘汰时改发独立信号（或 Replied 携带 `evicted` 标记）触发与 TimedOut 同款的平台收敛；`pending_asks` 顺带加过期清理。

### L3 排队表情竞态

**机理**：`react_to_message` 的「remove → 两次网络 await → insert」非原子，recv 侧（⏳）与 runner 侧（👀）对同 mid 并发交错 → 两表情并存、map 只记一个、终态只删一个 → ⏳ 永久残留。

**修法建议**：与 core 的 S-3/S-4 同思路：react 前检查消息是否已被本 conv 的 runner 接管（或 react 结果 map 加版本/世代校验）；至少保证「打 ⏳ 前先查 OnIt 已存在则跳过」+ 失败路径删表情兜底。

### L4 排队提示复查守卫写反

**修法建议**：`get(conv).is_none()` → `is_none_or(|q| q.is_empty())`（或 runner 取批时直接 remove entry）。补竞态单测。

### L5 热改数值无上限

**修法建议**：`/timeout` 上限（建议 ≤ 30 天，见 §五 D-5）；`/config batch_window_ms` 热改复用启动侧 `BATCH_WINDOW_MAX_MS` 校验。溢出用 `checked_mul` 兜底。

### L6 ask_via_im 污染审批审计

**修法建议**：Ask 类命中不落 `permission_decision` 审计（或独立 decision 词如 `ask_answer`），`/stats` 聚合口径同步。

### L7 `/cd` 在 ACP 不生效

**修法建议**：连接缓存记录 `loaded_cwd`，与 `req.cwd` 不一致时也走 LoadSession。

### L8 卡片 markdown 未转义

**修法建议**：见 §五 D-4（转义范围待对齐：至少 `<`（防 `<at>`），是否连 `[`（防链接伪装）一起）。

### L9 managed 流式 md_body 无截断

**修法建议**：`stream_body_md` 保留头部 + 尾部窗口（如各 12KB，中间 `…已截断 N 字符…`），终态仍全量走纯文本兜底；与 core `card_session.rs` 的上限策略对齐。

### L10 instance.lock truncate 顺序

**修法建议**：open 不带 truncate，flock 成功后再 truncate + 写 PID。

### L11 export 产物权限

**修法建议**：写后（或先 create+chmod 再写）`set_permissions(0o600)`，与 Import 分支对齐。

### L12 ilink download URL 编码

**修法建议**：与 upload 同法 `url.query_pairs_mut().append_pair()`。

### L13 `/model` 注入 ACP 命令串

**修法建议**：对 model 名做字符白名单校验（`[A-Za-z0-9._-]`），拒绝含空格/引号/`=` 前缀形态。

### L14 metrics 明文 HTTP

**修法建议**：非 loopback 部署文档要求反代 TLS（README/SECURITY 补充）；比较改恒定时间（如逐字节 XOR 累加）。

### L15 socket_spawned CAS

**修法建议**：`compare_exchange(false, true, AcqRel, Acquire)`，仅赢家走 bind。

### L16 always 语义丢失

**修法建议**：见 §五 D-7（补齐回传 `updatedPermissions` vs 删死代码）；client/server 超时起算点对齐（client 侧预算扣除发卡耗时或 server 侧从 connect 起算）。

### L17 询问卡 note 竞态

**修法建议**：patch 前复查 pending 状态（快照 request_id 仍活着才发），或 per-card 单调版本号丢弃过期 patch；重渲染不复取 TTL 时间戳（沿用卡片原始创建时刻）。

## 三、已查证无问题的维度（供后续 review 复用）

- **锁纪律/死锁**：全库锁序一致；parking_lot 守卫均未跨 `.await`；select 分支取消安全。
- **ACP 连接 map 对称性**：锁临界区内 insert（无 double-insert）、`same_channel` 防误删、retain 清僵尸、Drop/shutdown 双兜底、idle deadline 每轮重算、SDK child_monitor 保证子进程死亡强制收敛。
- **审批伪造面**：permission.sock 有 peer_uid + 0600 + 随机 token 三层；IM 回复路由过 sender 白名单/admin 门；飞书按钮 operator 三重校验 + TTL + 转发场景处理。
- **SQL**：store 全部 SQL 参数绑定（含 `format!` 动态串仅作绑定值）；migration 单事务；busy 重试防御性 ROLLBACK 正确；审计/历史表全部有界轮转。
- **秘密日志**：全局扫描 tracing 调用点，无 token/secret/passphrase 值入日志；凭据结构 Debug redacting。
- **ilink AES-ECB**：pkcs7_unpad 完整校验（无 padding-oracle 区分面）、key CSPRNG 每媒体新生成、媒体落盘 uuid 目录 + ext 白名单（路径穿越不可达）、下载 50MB 流式 + host 白名单 + 禁 redirect。
- **HTTP 面**：仅 /metrics /health 两个 GET、无 body extractor、Bearer 精确匹配无绕过、非 loopback 无 token fail-closed 拒启动。
- **CLI 协议解析**：`read_line_capped` 语义正确、`from_utf8_lossy`、serde 128 层递归限制、无对不可信输入的 unwrap/expect。
- **进程组/zombie**：`process_group(0)` + `GroupKillGuard`（wait 后 disarm）+ `kill_on_drop`；ACP 子进程 SDK ChildGuard 兜底。
- **命令行注入**：prompt 单 argv 不经 shell；codex `-s` 在 `--` 前、gemini `--prompt=` 绑定 + 64KB 预检；mcp json `sanitize_filename` + `create_new` 抗 symlink。
- **飞书分页/429/dedup**：10 页硬截断、429 归一重试 500ms→1s→2s、dedup 滑窗清理有界。

## 四、修复跟踪

> 在 worktree 分支 `fix/code-review-v8` 中逐项修复；每项 DoD = 修复 + 针对性测试 + grep 同类调用点确认无残留。修复批次：第一批 H1-H3，第二批 M1-M7，第三批 L1-L17，最后全量 `cargo test --workspace` / `cargo clippy` 验证。

| ID | 级别 | 标题 | 状态 |
|---|---|---|---|
| H1 | 🔴 | canUseTool 控制通道不感知 permission_mode | ⬜ |
| H2 | 🔴 | ACP 子进程继承全部环境变量 | ⬜ |
| H3 | 🔴 | wecom 分片死循环（message_max_len ≤ 3） | ⬜ |
| M1 | 🟠 | 飞书出站 HTTP 无超时 + token 写锁跨网络调用 | ⬜ |
| M2 | 🟠 | ACP stdout/stderr 无上限 | ⬜ |
| M3 | 🟠 | 控制通道内联审批 + stdin 未关即 wait | ⬜ |
| M4 | 🟠 | ACP 无 title 权限请求 fail-open | ⬜ |
| M5 | 🟠 | WS 重连退避不因健康连接重置 | ⬜ |
| M6 | 🟠 | 13 处裸 reqwest::Client::new() | ⬜ |
| M7 | 🟠 | UserMessageChunk 回显进流式卡 | ⬜ |
| L1 | 🟡 | patch_managed 终态泄漏 card_seqs/card_footers | ⬜ |
| L2 | 🟡 | PENDING_PER_CONV_CAP 淘汰不收敛平台侧 | ⬜ |
| L3 | 🟡 | 排队表情竞态 ⏳ 永久残留 | ⬜ |
| L4 | 🟡 | 排队提示复查守卫写反 | ⬜ |
| L5 | 🟡 | /timeout 溢出 + batch_window_ms 热改无上限 | ⬜ |
| L6 | 🟡 | ask_via_im 污染审批审计 | ⬜ |
| L7 | 🟡 | /cd 在 ACP idle 窗口不生效 | ⬜ |
| L8 | 🟡 | 卡片 markdown <at> 注入 | ⬜ |
| L9 | 🟡 | managed 流式 md_body 无截断 O(n²) | ⬜ |
| L10 | 🟡 | instance.lock truncate 顺序 | ⬜ |
| L11 | 🟡 | export --include-secrets 无 0600 | ⬜ |
| L12 | 🟡 | ilink download URL 未编码 | ⬜ |
| L13 | 🟡 | /model 注入 ACP 命令串 | ⬜ |
| L14 | 🟡 | metrics 明文 HTTP + 非恒定时间比较 | ⬜ |
| L15 | 🟡 | socket_spawned check-then-act | ⬜ |
| L16 | 🟡 | always 语义丢失 + 超时起算点不一致 | ⬜ |
| L17 | 🟡 | 询问卡 note 全卡重渲染竞态 | ⬜ |

## 五、待对齐决策点（修复中收集，最后统一对齐）

| # | 问题 | 选项与建议 |
|---|---|---|
| D-1 | **H2 ACP env 白名单范围**：`env -i` 后透传哪些变量 | 建议与 CLI 路径 S-2 白名单一致（`PATH`/`HOME`/`ANTHROPIC_API_KEY`/`ANTHROPIC_BASE_URL`）+ `TMPDIR`；是否需要 `SSL_CERT_FILE`（企业自签环境）待定 |
| D-2 | **M2 ACP IO cap 实现路径** | (a) imagent 侧包装带 cap 的代理 transport（彻底，工作量中）；(b) 给上游 agent-client-protocol 提 PR/issue（治本但周期长）；(c) 文档化限制。建议 (a)+(b) 并行，先落 (a) |
| D-3 | **M3 control 并发 responder 形态** | 拆独立 task 并发应答（改动大、需真机验证多 control_request 并发场景）；或先落「break 后 drop(stdin)」+ 审批串行化文档化（保守）。建议先保守后迭代 |
| D-4 | **L8 markdown 转义范围** | 只转义 `<`（防 @ 注入，推荐最小面）vs 连 `[`（防链接伪装）一起；v4 曾因「破坏 agent 输出格式」搁置——需定夺格式损失取舍 |
| D-5 | **L5 上限值**：`/timeout` 上限、batch_window_ms 热改上限 | 建议 timeout ≤ 30 天（43200 分钟）；batch_window 直接复用启动侧 `BATCH_WINDOW_MAX_MS` |
| D-6 | **M5 退避重置阈值** | 建议连接存活 ≥ 60s 后的断开视为「健康断连」重置 backoff=1s |
| D-7 | **L16 always 语义** | 补齐回传 `updatedPermissions`（对齐 claude 原生行为）vs 删除死代码维持网关侧 allow-set 短路现状（更简单）。建议后者 + 注释说明 |
| D-8 | **批次与提交粒度** | 建议 worktree `fix/code-review-v8`，按 H/M/L 三批 commit，全部完成后统一跑全量验证再合 main；是否需要 CHANGELOG 条目（v1.14.2 或 v1.15.0）待定 |
