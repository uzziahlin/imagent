# Changelog

记录 imagent 所有显著变更。格式参照 [Keep a Changelog](https://keepachangelog.com/)，版本遵循 [Semantic Versioning](https://semver.org/)。

## [Unreleased] — P4 第三波：统一 /resume——无感接管电脑端 Claude Code 会话

### Added
- **统一恢复列表（P4-11）**：`/resume` 列表 = IM 会话历史（📱）∪ 本机同项目 agent 会话（💻）——用户按序号选择即接管，全程无需知道 session id。本机会话按 conv 当前 workdir 扫描（`/cd` 切换列表随之变化，workdir 对齐由扫描天然保证），首条用户消息摘要 + 相对时间展示。
- **`Backend::list_local_sessions(workdir)`** trait 方法（默认空，依赖方向不变）：claude-cli / claude-acp 扫 `~/.claude/projects/<workdir编码>/*.jsonl`（session id = 文件名；摘要取头部首条非元数据 user 消息，cap 64KiB 容错解析；排序按 mtime 原始精度防同秒并列）；codex/gemini 无本机存储概念，`/resume` 自动退化为纯 IM 历史。
- **接管语义**：选中 💻 会话 = 写 sessions 表自动绑定，回复附分叉提示（「续接将从此处分叉；若终端仍开着请先退出」）；列表 per-conv 缓存，序号选择取缓存防两次调用间 mtime 变化错位（选中即消费）。

### Changed
- workspace 测试 320 passed（新增：扫描器 7 用例 + 统一列表/接管/序号引导 3 用例 + 既有 resume 用例适配新文案）；clippy 0 warning；fmt clean；真机冒烟（imagent 项目 9 个本机会话列出、摘要/排序/截断正确）。
- 含默认 ignore 的真机冒烟测试（`IMAGENT_RESUME_SMOKE_WD=<proj> cargo test -p imagent-claude --lib smoke_real_dir -- --ignored`）。

## [Unreleased] — P4 第二波：对标差距 7 项全落地

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

## [Unreleased] — P4 功能迭代（任务控制 / 批处理 / 看门狗）

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

## [Unreleased] — 安全审查 v2/v3/v4/v5 修复

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
