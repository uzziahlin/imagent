# imagent 深度 Review v2 — Issue 清单

> **评审对象**：`imagent v1.0.0` @ commit `c6efe58`（2026-07-04，已合并 `fix/code-review-p0` 分支）。
> **评审范围**：`crates/{core,ilink,wecom,claude,codex,gemini,store}` + `src/main.rs` + 工程化（CI/LICENSE/docs）。
> **评审方法**：主会话逐行精读工程化层 + 4 个独立子审查并行精读 7 个 crate（ilink / store / core / backend 三件套+wecom），交叉验证；每条 finding 带 `file:line` + 失败场景 + 修复方向。
> **与 [`CODE_REVIEW.md`](CODE_REVIEW.md) 的关系**：v1 已修项本次**逐条独立核实**（见文末核实矩阵），并发现 v1 **未覆盖的 3 个新 P0** 与一批新 P1/P2。
> **总体评分**：**开源就绪度 6.5 / 10**（评审时）→ 修完 P0 后约 **7.5 / 10**。架构与基础工程质量优秀；剩余阻塞集中在「凭据保护链在 headless 部署名存实亡」+「开源工程化标配未落地」。

## 📋 修复进度（2026-07-04，分支 `fix/code-review-v2`）

**已落地（17 commit，workspace 230 passed，clippy 0 warning）**：
- ✅ **P0 全部（3/3）**：P0-A（ACP fail-open→fail-closed）、P0-B（权限 socket 对端 uid 鉴权 + chmod 0600）、P0-C（login baseurl 域名白名单）
- ✅ **P1-D**：workdir「安全边界」措辞修正为「cwd（非沙箱）」
- ✅ **E-1**：各 crate MSRV 统一继承 workspace 1.80 + 修复 ilink `media.rs` 的 `is_multiple_of`（1.87 API，CI @1.80 实际会失败）
- ✅ **E-2**：7 crate + `main.rs` 加 `#![forbid(unsafe_code)]`；core 因 P0-B peer-uid 鉴权保留必要 unsafe，改用 `#![deny(unsafe_code)]` + 抽 `current_uid()` helper、`peer_uid` 局部 `#[allow(unsafe_code)]` 隔离（开源姿态：默认禁 unsafe，必要处显式标注 + SAFETY）
- ✅ **P1-A**：WAL/SHM 边车文件 chmod 0600（堵 headless 明文回退泄漏面）
- ✅ **P1-B**：凭据写入审计（`credential_put` best-effort `append_audit`）
- ✅ **P1-C**：keyring fail-closed 选项（`require_keyring` 配置 + 明文回退/拒绝 metric counter）
- ✅ **P1-J**：login reqwest 禁 redirect（与运行时 client 一致，堵 login 阶段 SSRF/重定向劫持）
- ✅ **P1-H/L**：媒体簇——解密 fail-closed（aes_key 解析失败返回 Err 而非当明文泄漏）+ 流式下载防 chunked OOM（`bytes_stream()` 累计 + 超限中止）
- ✅ **P1-I**：WeCom msgid 去重（`Dedup` 提到 core 共享，drain task 接线）
- ✅ **P1-K**：compact_summary 删除推迟到 run 成功落库后（run 失败不再永久丢失摘要）
- ✅ **P1-G**：权限 socket 回复等待加 `agent_timeout` 超时（agent 死/用户不回复不再永驻吞消息）
- ✅ **P1-F**：`/new`/`/switch`/`/compact` 取 conv 串行锁（不再与在飞 agent task 并发损坏 session 状态）
- ✅ **D-1**：项目根 `CLAUDE.md` onboarding 更新（反映业务代码已实现 + P1 基本完成 + omp 故障教训，不再误导新会话）
- ✅ **第三波（D-2/D-3/D-4/E-3/E-5/E-6）**：v1 CODE_REVIEW 顶部声明修正；CHANGELOG `[Unreleased]` 归位 [1.0.0] 之前 + 填 v2 修复；README/CHANGELOG 测试数 229；coverage 去 `|| true`；`rust-toolchain.toml` pin 1.80；release artifact 打包 LICENSE
- ✅ **E-4**：cargo-deny（`deny.toml` + CI `deny` job）+ dependabot（cargo/actions 周更）+ CODEOWNERS（占位待填 owner）

> ⚠️ **破例说明（P0-B + P1-A/B/C 由主会话自行 Edit）**：omp 工具链反复异常——上会话 5 次（并发 exit 1（API 限流）×2、「清理工作树到只剩本任务文件」覆盖前序成果×2、强约束下 noop×1）；本会话 P1-C 委派 omp **第 3 次「空手退出」**（exit 0、零产出、log 仅 1 行主会话口吻废话；前两次 6/30、7/04 已记 engram `cd4f3255`/`33d52163`）。依 `CODE_REVIEW.md` 顶部先例（"omp 工具链对重构类任务反复委派/挂死，用户授权破例自行 Edit"），P0-B/P1-A/B/C 破例主会话自行 Edit（方案已完整 review/设计，每项 `cargo test` 验证）。**违反 CLAUDE.md omp 委派硬规则**，请 review；P0-A/C 由 omp 完成，P1-D/E-1/E-2 为注释/配置/attribute 类主会话直接改。后续 omp 任务说明须显式禁 git 写操作 + 禁删非任务文件（教训已记 memory：`omp-worktree-protection`）。

**剩余（后续 issue 跟进）**：P1-E（方案已设计，实现 defer——见下）、P2 全部、E-3~E-7、D-1~D-4。

每条 issue 带 checkbox，id 形如 `P0-A`，便于转 GitHub issue 追踪。

---

## ✅ 亮点（无需改动，记录在案）

- **三层 + 双抽象架构干净**：core 持 `Platform`/`Backend` trait，依赖倒置正确。session 生命周期提到 core（store 持久化），Backend 退化为无状态执行器——优于竞品 feiyun。
- **`spawn_cli_backend` 抽象到位**（`core/backend_common.rs`）：三 CLI backend 脚手架重复已消除。
- **工程纪律上游水平**：SQL 全参数化、生产代码无 `unwrap/expect/panic`、日志不泄漏凭据明文、出站分片 UTF-8 完整、`CoreError::SessionExpired` 类型化。
- **v1 的 P0 大部分真修了**（见核实矩阵）。

---

## 🔴 P0 — 阻塞项

### P0-A  ACP 后端权限模式 fail-OPEN ✅ 已修

- [x] **位置**：`crates/claude/src/acp.rs:413-425`（`select_option` 的 `.or_else(|| options.first())`）。
- **本质**：`select_option` 找不到目标 kind 时无差别 fallback 到 `options.first()`。`Deny`/`Ask` 走 `select_option(.., false)`（找 `Reject*`），当 agent 只给 `Allow*` 选项时 → fallback 击穿为 `Selected(Allow)`。
- **修复**：移除 `select_option` 内部 fallback（只返回 `find` 结果），`allow_outcome`（Allow/Off）显式保留 fallback（行为不变），Deny/Ask 自然 `None → Cancelled` fail-closed。
- **回归测试**：`permission_outcome_deny_without_reject_cancels`、`permission_outcome_ask_without_reject_cancels`（options 只含 `Allow*` → 断言 `Cancelled`）。

### P0-B  权限审批 socket 无对端鉴权 ✅ 已修

- [x] **位置**：`crates/core/src/dispatch.rs`（`spawn_socket_accept` + `handle_permission_socket`）。
- **本质**：`permission.sock` 不设权限、不校验对端，任何本地进程可 connect 伪造权限请求（社工/DoS）。
- **修复**：① bind 后 `chmod 0600`；② accept 后 `peer_uid`（Linux `SO_PEERCRED` / macOS `LOCAL_PEERCRED`）校验对端 `uid == getuid()`，不匹配或取不到一律 `warn!` 拒绝（fail-closed）。新增 `[target.'cfg(unix)'.dependencies] libc`。
- **回归测试**：`permission_socket_tests::peer_uid_returns_self_for_local_pair`（socketpair 断言 `peer_uid == getuid()`）。

### P0-C  login 阶段 `baseurl` 无白名单 ✅ 已修

- [x] **位置**：`crates/ilink/src/login.rs:107`。
- **本质**：无条件信任服务端返回的 `baseurl`，MITM/DNS 劫持可把带 `bot_token` 的请求与全部消息导向恶意域名。
- **修复**：新增 `validate_baseurl`——强制 `https://` 且 host 为 `ilinkai.weixin.qq.com` 或 `*.weixin.qq.com`，否则 `Err`；confirmed 分支校验。
- **回归测试**：5 个（默认/子域名放行；`ilinkai.weixin.qq.com.evil.com`/内网 IP/`http`/`ws` 拒绝）。

---

## 🟠 P1 — 应优化项

### 安全 / 凭据保护链（headless 部署实质泄漏）

- [x] **P1-A  WAL/SHM 边车文件 0644 + headless 明文回退** ✅ 已修（1b5d0b9）：`crates/store/src/store.rs:687-713`。`tighten_permissions` 只 chmod 主 db，WAL 模式的 `-wal`/`-shm` 按 umask 创建（0644）。headless 无 secret-service → 明文 bot_token 写 SQLite → WAL 持明文副本 → 同机其他用户可读。修复：`-wal`/`-shm` 显式 chmod 0600 且每次 open 重做；或启动强制校验 `~/.imagent` 为 0700。
- [x] **P1-B  凭据事件零审计** ✅ 已修（1b5d0b9）：`crates/store/src/store.rs:75`。`put_credential` 等不入审计。修复：补 `append_audit("credential_put", ...)`。
- [x] **P1-C  keyring 回退明文无 fail-closed 选项** ✅ 已修：`Config.require_keyring`（默认 false）+ `Store::set_require_keyring`；`put_credential` 在 `require_keyring=true` 且 keyring 失败时返回 `Err`（fail-closed，不落盘）；新增 metric `imagent_credential_plaintext_fallback_total` / `_keyring_rejected_total`（store 注册默认 registry，与 ilink ratelimit 同模式）；`get_credential` 读取路径只计数、不 fail-closed（保历史明文凭据可用）。回归测试 4 个（config 默认/解析 + store 拒绝/回退）。

### 安全姿态误导

- [x] **P1-D  `default_workdir` 被称「安全边界」实际只是 cwd** ✅ 已修（fa10f9c）：`crates/core/src/types.rs:17`、`config.rs:47`、`crates/claude/src/backend.rs:107`。`--allowedTools Read,Edit` 不限路径，agent 可读 `~/.ssh/id_rsa`。**开源前至少改措辞为「agent cwd（非沙箱）」**。

### 健壮性 / 正确性

- [ ] **P1-E  ACP 超时不杀子进程** ⏳ **方案已设计，实现 defer**：长驻 task 的 `connection.send_request(PromptRequest).block_task().await`（`acp.rs:222`）不响应外部取消；run future（dispatch `tokio::time::timeout` 包装）超时 drop 只丢 `resp_rx`，长驻 task 继续等子进程 turn，子进程资源泄漏。**设计方案**：`PromptReq` 加 `cancel: oneshot::Receiver<()>`；run 创建 `cancel_tx` 随 run future 生命周期（drop 触发）；长驻 task `tokio::select!` prompt 执行 + cancel，cancel 分支 `return Err` 退出 `connect_with` 闭包 → connection drop → SDK `ChildGuard` kill 子进程（基于 `acp.rs:154` 注释假设）。**代价**：超时后长驻 connection 销毁，下次 run 重建（性能换正确性）。**defer 原因**：① SDK 不确定性（`connect_with` 闭包 Err 类型 + `return Err` 是否触发 connection drop / ChildGuard kill 需验证）；② 无单测保护（cancel 传播需真子进程 e2e）；③ 高风险核心异步。建议专门 PR + SDK 调研 + e2e 验证。
- [x] **P1-F  `/compact`/`/new`/`/switch` 绕过 `conv_locks`** ✅ 已修：提取 `acquire_conv_lock` helper；`/new`/`/switch`/`/compact` 分支开头取 conv 串行锁，与普通消息 agent task 复用同一 `conv_locks` 串行化。slash 路径不显式 release（`_guard`/`_lock` RAII drop），HashMap 回收延迟到下次普通消息 release（最终回收）。`/compact` 持锁跨 agent run 与正常 agent task 串行；无死锁（agent run 不取 conv_lock）。正确性由 conv_lock 机制 + 现有 slash/serial_order 测试保证。
- [x] **P1-G  权限 pending 无超时，agent 死后下一条消息被静默吞** ✅ 已修：`handle_permission_socket` 的 `rx.await` 改 `tokio::time::timeout(agent_timeout, rx)`；超时回 deny 并 drop receiver。agent 死后至多 agent_timeout 窗口内的消息可能被吞，之后 socket task 超时 drop rx → 后续 route miss → dispatch fallthrough 正常 handle（route miss 的 fallthrough 已存在）。`agent_timeout` 经 `spawn_socket_accept` 传入。超时由 tokio 库保证，集成测试留 e2e。
- [x] **P1-H  媒体解密 silent failure** ✅ 已修：`download_media` 区分 None / parse 失败 / 解密失败三分支，aes_key 存在但解析失败时返回 Err（不再落入 None 当明文泄漏）。
- [x] **P1-I  WeCom 完全无消息去重** ✅ 已修：`Dedup` 从 ilink 提到 `core`（平台无关共享；ilink re-export 保持 `crate::dedup::Dedup` 路径不变，platform.rs 零改动）；`parse_msg_callback` 返回 `(msgid, InboundMessage)` 暴露 msgid；wecom drain task 持 `Dedup` 对 msgid 滑动窗口去重。回归测试 `drain_drops_duplicate_msgid`。
- [x] **P1-J  `login.rs` reqwest 未禁 redirect** ✅ 已修：login client builder 加 `.redirect(Policy::none())`，与运行时 client（`client.rs:42`）一致。
- [x] **P1-K  `/compact` 首条消息失败 → compact_summary 永久丢失** ✅ 已修：注入 summary 时只设 flag 不立即 delete；delete 推迟到 run 成功 + upsert 落库之后。run 失败（backend Err / panic）的 `return` 在 delete 之前，summary 保留供下次注入。正确性由控制流保证（现有成功路径测试覆盖；失败路径受 MockBackend 不支持 run 失败所限，留 e2e）。
- [x] **P1-L  媒体下载 chunked OOM** ✅ 已修：`bytes_stream()` 流式累计 + 超 `MEDIA_MAX_BYTES` 即中止（覆盖 chunked / 无 Content-Length 的大文件）；Content-Length 预检保留作快速路径。

---

## 🟡 P2 — 打磨项

- [ ] **P2-A** `/switch` 不校验 agent_kind（`dispatch.rs:498-528`）
- [ ] **P2-B** socket `bind` 失败只 warn（`dispatch.rs:232-238`，与 P0-B 同源，可顺手）
- [ ] **P2-C** socket 问询强制 `ReplyHint::None` 丢 iLink context_token（`dispatch.rs:309-313`）
- [ ] **P2-D** `/allow` 无角色区分，任意白名单用户可授权新用户（`dispatch.rs:399-433`）
- [ ] **P2-E** `/allow` store 失败仍回「已授权」（`dispatch.rs:405-431`）
- [ ] **P2-F** 中间 Text chunk 全丢弃，「流式」实际一次性（`dispatch.rs:824-834`）
- [x] **P2-G** `parse_reply` 首字符 y/Y 误判 ✅ 已修：去掉首字符 y/Y 宽匹配（会把 year/yellow/yesterday 误 allow——权限 approve/deny 的真实安全 bug），改精确匹配 `y/yes/ye/yep/yeah/ok/okay/是/允许/好/好的`。回归测试 `parse_reply_year_not_allowed`。
- [ ] **P2-H** Auth 无归一化（`auth.rs:35-37`）
- [x] **P2-I** conv_id 未消毒 ✅ 已修 / mcp.json 不清理 ⏳ defer：① conv_id 经 `sanitize_filename` 消毒文件名（防 `../` / `/` / `:` 路径遍历；`--conv-id` 参数仍用原值保路由），单测 `sanitize_filename_strips_traversal`；② mcp.json 用完清理需重构 run 的 spawn 路径，defer 专门小 PR（整洁性，非安全）。
- [x] **P2-J** codex prompt 裸 positional arg ✅ 已修：`codex exec <prompt>` 的 prompt 改为经 `--` 分隔的纯 positional（resume / 新建两分支），防止 prompt 以 `-` 开头被误解析为 flag（参数注入）。`Command::arg` 不经 shell（无 shell 注入），此修复针对 codex CLI 的 arg 解析。
- [ ] **P2-K** ACP `sessions` HashMap 无界增长（`acp.rs:193,220,238`）
- [x] **P2-L** ws_url 不校验 wss ✅ 已修 / ack 失败仍继续 ⏳ defer：① `connect_and_serve` 开头校验 ws_url 远端必须 `wss://`（`ws://` 仅 `localhost`/`127.0.0.1`/`[::1]` 例外，测试 `run_loops_on_connect_failure` 用 `ws://127.0.0.1` 不破坏）；② ack 失败仍继续涉及 wecom 协议层可靠性，defer 专门评估。
- [ ] **P2-M** 固定 `permission.sock` 路径，单实例硬约束（`claude/backend.rs:55-64`）
- [x] **P2-N** store schema 迁移未事务化 ✅ 已修：migrate 整体包在 `unchecked_transaction` + `commit` 内，失败回滚（避免半迁移状态不一致）。
- [x] **P2-O** 迁移无「user_version 过新」拒绝 ✅ 已修：加 `SCHEMA_VERSION=3` 常量；`user_version > SCHEMA_VERSION` 时 `Err` 拒绝（旧代码跑新 DB 风险）。
- [x] **P2-P** store 无 `busy_timeout` ✅ 已修：`open_and_setup` 加 `PRAGMA busy_timeout=5000`（多连接竞争时等待而非立即 SQLITE_BUSY 失败）。
- [x] **P2-Q** `first_credential` 无 ORDER BY ✅ 已修：SQL 加 `ORDER BY account_id`（多行时顺序确定，原 `LIMIT 1` 无 ORDER BY 顺序未定义）。
- [ ] **P2-R** 审计日志无轮转（`store/schema.rs:55-63`）
- [ ] **P2-S** ilink `ilink_bot_id`/`ilink_user_id` 全程 dead_code（`client.rs:24-27`，需抓包确认）
- [ ] **P2-T** ilink breaker threshold=1 单次即熔断（`platform.rs:70-74`）
- [ ] **P2-U** ilink extract_host 裸字符串 split（`media.rs:146-156`，建议 `url::Url`）
- [ ] **P2-V** ilink 媒体目录/文件权限不严谨（`platform.rs:655,660`）
- [x] **P2-W** ilink `let _ =` 吞错无 log ✅ 已修：`set_sync_buf` / `set_context_token` 失败改 `if let Err warn`（best-effort 不阻断，但可观测）；`platform.rs:655` 的 `set_permissions` 属 P2-V（媒体目录权限），保留。
- [x] **P2-X** ilink `dedup.rs:31`/`ratelimit.rs:21` 的 `expect`（mutex poison 永久 panic） ✅ 已修（仅 dedup 部分）：dedup check 的 `std::sync::Mutex::lock().expect` 改 `unwrap_or_else(into_inner)`（持锁 panic 的 poison 后恢复，避免永久 panic；dedup 是 best-effort 去重）。`ratelimit.rs:21` 是 `register_int_counter!().expect`（启动期注册，非 mutex poison），保留。

---

## 🛠 开源就绪度工程化（`P3_ROADMAP.md §2` 自定标准未达成）

- [x] **E-1（P1）MSRV 声明混乱** ✅ 已修（5dbccc1）：workspace 写 1.80 但无 crate 继承，`store` 写死 1.75。修复：各 crate 加 `rust-version.workspace=true`。
- [x] **E-2（P1）无 `#![forbid(unsafe_code)]`** ✅ 已修：7 crate + `main.rs` 加 `#![forbid(unsafe_code)]`；core 用 `#![deny(unsafe_code)]` + `current_uid()`/`peer_uid` 局部 `#[allow]` 隔离 P0-B 必要 unsafe。
- [x] **E-3（P1）coverage 形同摆设** ✅ 已修：去掉 tarpaulin 的 `|| true`（失败可见）；保留 `cargo install` 的 `|| true`（已装重试语义）+ codecov `fail_ci_if_error: false`（上传失败兜底）。
- [x] **E-4（P2）无 cargo-deny / dependabot / CODEOWNERS** ✅ 已修：新增 `deny.toml`（license 白名单 + source/ ban）、`.github/dependabot.yml`（cargo + github-actions 周更）、`.github/CODEOWNERS`（占位 `@imagent/maintainers`，TODO 待填真实 owner）；CI 加 `deny` job（`cargo deny check licenses sources bans`，与既有 `audit` job 互补）。
- [x] **E-5（P2）无 rust-toolchain.toml** ✅ 已修：新增 `rust-toolchain.toml` pin `channel = "1.80"`（= workspace MSRV），本地 + CI 统一 MSRV 验证。
- [x] **E-6（P2）release artifact 不含 LICENSE** ✅ 已修：`release.yml` Package 步骤 `cp LICENSE dist/`，gh-release `files` 含 `dist/LICENSE`。
- [ ] **E-7（迭代）无 fuzz**（ilink proto / stream 解析适合 fuzz）。

### 文档状态漂移

- [x] **D-1（P0 流程）项目根 `CLAUDE.md` 严重过时** ✅ 已修：onboarding 段重写——反映三层+双抽象业务代码已实现、当前 P1 基本完成的进度、新增 CODE_REVIEW_v2 为必读、omp v16.x 反复故障的破例先例。
- [x] **D-2（P2）`CODE_REVIEW.md` 顶部「未 commit」声明过时** ✅ 已修：改为「已合并到 main + 指向 v2 核实矩阵」。
- [x] **D-3（P2）`CHANGELOG.md` `[Unreleased]` 位置不规范** ✅ 已修：`[Unreleased]` 移到 `[1.0.0]` 之前（Keep a Changelog 规范），填入 v2 修复内容。
- [x] **D-4（P2）测试数漂移** ✅ 已修：README 214→229；CHANGELOG `[Unreleased]` 注 229 passed（`[1.0.0]` 的 214 为发布时历史快照，保留）。

---

## 🧭 架构评价

**优点**：双 trait 抽象边界干净、`spawn_cli_backend` 消除三后端重复、`CoreError::SessionExpired` 类型化、出站分片 UTF-8 完整、conv 级串行锁。

**结构性建议**：
1. **状态机收敛**：会话状态散在 `sessions`+`named_sessions`+`config` 三表（无事务），是 P1-F/P1-K 温床。收敛到 per-conv `ConvState` + 单一 mutator。
2. **`ReplyHint::ILink` 泄漏到 core 类型**：core 知道 ilink 实现细节。改关联类型/泛型。
3. **可测试性**：权限闭环端到端零集成测试，全是 mock。建真机 e2e checklist。
4. **store 并发**：单 `Mutex<Connection>` + `spawn_blocking`（IM 低 QPS 可接受，未来可 sqlx）。

---

## 🎯 修复优先级建议

**第一波（开源前必修）**：~~P0-A/B/C~~ ✅ → P1-D（workdir 措辞）→ E-1（MSRV）+ E-2（forbid unsafe）→ P1-A（WAL chmod）+ P1-C（keyring fail-closed）+ P1-B（凭据审计）。

**第二波（上线前）**：P1-E（ACP 取消传播）+ P1-F（slash 取锁）+ P1-G（pending 超时）+ P1-I（WeCom 去重）+ P1-H/J/L（媒体/login 收敛）。

**第三波（开源打磨）**：E-4/E-5/E-6 + E-3（coverage）+ D-1~D-4（文档对齐）+ E-7（fuzz）+ 权限闭环 e2e。

---

## 附录：v1 已修项独立核实矩阵

| v1 编号 | v1 声明 | 本次核实 | 去向 |
|---|---|---|---|
| P0-1 MSRV 1.80 | ✅ | 部分 | → E-1 |
| P0-2 Windows cfg gate | ✅ | 已修 | — |
| P0-3 LICENSE 统一 | ✅ | 已修 | — |
| P0-4 ACP fail-closed | ✅ | 部分修 | → P0-A ✅ |
| P0-5 backend 超时 | ✅ | CLI 已修/ACP 未修 | → P1-E |
| P0-6 kill_on_drop | ✅ | 已修 | — |
| P0-7 stderr 并发读 | ✅ | 已修 | — |
| P0-8 SSRF redirect | ✅ | 运行时已修/login 漏 | → P1-J |
| P0-9 mcp fail-closed | ✅ | 已修 | — |
| P0-10 codex/gemini warn | ✅ | 已修 | — |
| P1-1 抽 backend helper | ✅ | 已修 | — |
| P1-2 conv_locks 回收 | ✅ | 已修（有保留） | — |
| P1-3 agent_kind 校验 | ✅ | 普通消息已修/switch 漏 | → P2-A |
| P1-6 hold timeout | ✅ | 部分修 | → P1-L 注 |
| P1-7 媒体大小上限 | ✅ | 部分修 | → P1-L |
| P1-8 tighten_permissions | ✅ | 已修/漏 WAL | → P1-A |
| P1-9 health 真实化 | ✅ | 已修 | — |
| P1-11 wecom channel | ✅ | 已修 | — |
| P1-12 metrics 默认关 | ✅ | 已修 | — |
| P2-4 audit PR 也跑 | 建议 | 未改 | → E-4 |
| P2-6 SessionExpired variant | ✅ | 已修 | — |
| P2-7 allowedTools 空串 | ✅ | 已修 | — |

**结论**：22 项核实中真修 14、部分修 6、未改 2。v1「✅ 全部完成」偏乐观——6 项「部分修」正是本文 P0-A / P1-E / P1-J / P1-L / P1-A / E-1 的来源。
