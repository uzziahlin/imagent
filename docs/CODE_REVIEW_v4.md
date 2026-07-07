# imagent 深度 Review v4 — Issue 清单 + 修复跟踪

> **评审对象**：`imagent v1.0.0` @ `4090c01`（main，已合并 `fix/code-review-v3`）。
> **评审范围**：`crates/{core,ilink,wecom,claude,codex,gemini,store}` + `src/main.rs` + 开源工程化层（CI / Cargo metadata / 文档 / deploy）。
> **评审方法**：主会话逐行精读核心调度链路（dispatch/permission/auth/main/config/backend_common/mcp/error/metrics/types）+ 3 个独立子审查并行精读（① ilink/wecom 平台协议安全 ② claude/codex/gemini 后端执行 + store 凭据 ③ CI/供应链/文档等开源治理），对 v3「已修」项逐条读源码复核。实测 `cargo test --workspace` = 241 passed / 0 failed / 2 ignored。
> **与 v3 的关系**：先逐条核实 v3 声称已修项是否真落地（v1 当年谎报、v2 补上、v3 自查无谎报——本轮独立复核 v3），再查 v3 未覆盖的新问题。
> **总体评分**：代码实现质量 **8/10**（架构干净、241 测试、生产代码极少 unwrap、fail-closed 倾向一致）；**开源就绪度 6.5/10**（差距不在代码，而在发布基础设施缺失 + v3 工程化项「部分修报完成」）。修完第一波 → 约 8/10 可开源；再修第二波 → 8.5/10。

## 📋 修复进度（2026-07-08 起，分支 `fix/code-review-v4`）

| 波次 | 范围 | 状态 |
|---|---|---|
| **第一波（开源前）** | B1–B5（开源基础设施）+ S-1/S-2（安全语义 + env 边界） | 🔧 进行中 |
| **第二波（上线前）** | S-3/S-4/S-5 + R-1~R-6 + WeCom 协议健壮性 + CI fuzz/audit | ⬜ 待修 |
| **第三波（打磨/重构）** | P3 清单 + 架构性建议（状态机收敛 / RAII / 退出语义 / ReplyHint 泛型 / 后端语义统一） | ⬜ 待修 |

---

## ✅ v3 核实结论（工程诚信）

**代码 bug 维度：无谎报。** v3 的 9 个 P1 + 10 个 P2 done 经本轮逐行读源码复核，**全部真修、有测试佐证**。7 个 P2 defer + 8 个 P3 defer 描述准确、理由成立。

**工程化维度：E-2/E-5「部分修报完成」。** v3 把 E-2（owner 统一）标 ✅，实际 `README.md:74` + `deploy/systemd/imagent.service:3` 仍是 `<owner>` 占位符（v3 只改了 book.toml 一处）；E-5（Cmd::Stop）从空壳升级到「打印 kill PID」，doc comment 仍误导。这是 v1/v2 当年「谎报完成」模式的轻度重演（v2 是 P0 谎报，v3 是 E 类部分修报）——后续 review 对 E 类项也应像 P 类一样逐 file:line 核实，而非依赖声明。

| v3 声明 | 本轮核实 |
|---|---|
| 代码 P1（9/9）| **真修，无谎报** |
| 代码 P2 done（10）| **真修** |
| 代码 defer（7 P2 + 8 P3）| **描述准确，理由成立** |
| E-1 CI macOS 矩阵 / E-7 clippy --all-features / E-8 fuzz exclude | **真修**（ci.yml 实测） |
| **E-2 owner 统一** | **⚠️ 部分修报完成** → 本轮 B3 |
| **E-5 Cmd::Stop** | **⚠️ 半修**（doc 仍误导）→ 本轮 S4 |
| **B1 无 remote/tag** | **v3 未识别，本轮新增阻塞项** |

---

## 🔴 第一波 — 阻塞开源（开源前必修）

### B1  无 git remote + 无 git tag（发布渠道不存在）⬜
- **事实**：`git remote -v` 空；`git tag -l` 空（0 个 tag）。所有 `github.com/uzziah/imagent` 链接（CI badge、book edit、release.yml `on: tags: ["v*"]`）都是悬空引用，release.yml 从未跑过。
- **影响**：「开源就绪」的最小定义（能 clone / 能下载 release）当前不成立。CHANGELOG `[1.0.0] — 2026-07-02` 这个版本从未真实发布。
- **修复**：① 建 GitHub 仓库 `uzziah/imagent`；② `git remote add origin …`；③ push 后打 `v1.0.0`（或先 `v0.1.0` 诚实表态）tag 触发 release.yml 出三平台二进制 + sha256。
- **归属**：需仓库 owner 操作（非代码改动）。

### B2  README 协议声明事实错误：「双 license」实际是单 MIT ⬜
- **位置**：`README.md:143`「P3 ✅ 开源化（双 license/CI/…）」。
- **事实**：`LICENSE` 单 MIT；`Cargo.toml` `license = "MIT"`；全项目无任何双 license 痕迹，唯独路线表这一格写「双 license」。
- **修复**：改「MIT license」。

### B3  v3 E-2「owner 统一」部分修报完成 — `<owner>` 占位符 2 处残留 ⬜
- **位置**：`README.md:74`（快速开始第一行 clone 命令）+ `deploy/systemd/imagent.service:3`（Documentation）。
- **影响**：新用户照抄 clone 命令必失败。
- **修复**：`<owner>` → `uzziah`（与 Cargo.toml/book.toml 一致）。

### B4  README 路线表与 CHANGELOG `[Unreleased]` 自相矛盾 ⬜
- **位置**：`README.md:140-143`「P0-P3 全 ✅」+ `status-v1.0` 徽章 vs `CHANGELOG.md:5` `[Unreleased]`（含 v3 的 9 P1 + 17 P2 + 工程化修复）。
- **修复**：README 徽章暂改 `status-pre-release`；路线表 P3 行注明「v3/v4 安全审查修复见 CHANGELOG Unreleased」；CHANGELOG 加 v4 段。

### B5  systemd `User=%i` 在非模板单元里开箱即坏 ⬜
- **位置**：`deploy/systemd/imagent.service:11`。`%i` 是模板单元（`imagent@.service`）specifier，本文件名是 `imagent.service`，`%i` 展开为空 → systemd 报 `User may not be empty`。
- **修复**：改 `# User=imagent  # 必须改成运行用户`（注释 + 提示）；顺带放开安全加固（`NoNewPrivileges`/`ProtectSystem`/`ReadWritePaths`）并说明。

### 第一波工程化补充项 ⬜
- **SUMMARY.md** 只引 v1 CODE_REVIEW，侧栏缺 v2/v3（mdBook 站点看不到最新 review）→ 补条目。
- **launchd** 日志写到 `/tmp`（重启即丢）→ 改 `~/Library/Logs/imagent.log`。
- **CI audit/deny** 加了 `if: push to main`（PR 不阻塞）→ dependabot 升级 PR 可带 advisory 直接合入。改回 PR 也跑（advisory 级 deny），或 PR 上 `continue-on-error` 可见。
- **CI fuzz cron**：`fuzz/` 有 2 个 target 但永不跑 → 加 `fuzz.yml` 每周跑。

---

## 🛡️ 第一波 — 安全（开源前必修）

### S-1  ACP 后端 `allowed_tools` 完全不强制（跨后端安全语义不一致）⬜
- **位置**：`crates/claude/src/acp.rs:283-301`（`run()` 仅 debug-log allowed_tools 后忽略）。
- **本质**：CLI 后端用 `--allowedTools Read,Edit` 强制收敛；ACP 后端无等价机制，`allowed_tools` 参数被吞。配合 `PermissionMode::Allow`/`Off`（`permission_outcome` 对每个 `session/request_permission` 全放行）→ claude 可用其请求的任何工具，无门禁。用户配 `allowed_tools=["Read"]` + `agent="claude-acp"` 期望只读，实际可任意 Edit/Bash，且**无启动告警**（不像 codex/gemini 在 main.rs:150-158 有 warn）。
- **修复**：启动时若 `agent="claude-acp"` 且 `allowed_tools` 非空 → warn（仿 codex/gemini）；在 `permission_outcome` 的 Allow/Off 路径补 tool 名交叉校验；SECURITY.md 声明 ACP 限制。

### S-2  `spawn_cli_backend` 未 `env_clear`（agent 子进程继承父进程全部 env）⬜
- **位置**：`crates/core/src/backend_common.rs:54-67`；三后端 `Command::new(...)` 均未 `env_clear()`。
- **本质**：claude/codex/gemini 子进程 inherit 父进程整个 environ，经 `Bash`(env / `cat /proc/self/environ`) 可读取部署环境的 `DATABASE_URL`/CI secret/其他工具 token，再经 tool_result 回传 IM 或写 workdir 被 exfil。违背「最小授权」姿态。
- **修复**：`spawn_cli_backend` 接受 `passthrough_env: &[&str]`，内部 `cmd.env_clear()` 后只显式 set 白名单（各后端传自己的 key：claude `ANTHROPIC_API_KEY`、codex `OPENAI_API_KEY`、gemini `GEMINI_API_KEY`，外加 `PATH`/`HOME`/`LANG` 等必要项）。

---

## 🟠 第二波 — 安全 / 健壮性（上线前首周）

### S-3  权限审批超时与 agent 执行共用同一个 600s 预算（慢审批误杀 agent）⬜
- **位置**：`crates/core/src/dispatch.rs:1048`（backend.run `timeout(agent_timeout)`）+ `:499`（handle_permission_socket 等 `agent_timeout`）+ `crates/core/src/mcp.rs:180`（`MCP_ASK_TIMEOUT=1200s`）。
- **本质**：三个时钟在 `handle` 内同时启动。用户慢审批挤占 agent 执行预算（590s 回复 → agent 剩 10s 被杀）；1200 vs 600 两个 magic number 不一致。
- **修复**：审批等待用独立预算，不计入 agent run 超时；或 agent run 超时在审批期间暂停计时；mcp 超时与 dispatcher 对齐。

### S-4  WeCom secret 明文存 config.toml（凭据保护不一致）⬜
- **位置**：`crates/core/src/config.rs:90`。iLink `bot_token` 走 keyring + fail-closed，WeCom secret 明文读 config。
- **修复**：WeCom secret 支持 keyring（与 ilink 一致）。

### S-5  agent stdout/stderr 读循环无长度上限（OOM / 内存膨胀）⬜
- **位置**：`crates/core/src/backend_common.rs:81,87,187`。单行无上限（10GB 输出全量分配）；stderr 全量累积到子进程退出。v3 P1-9 给 permission socket read_line 加了 64KiB cap，同维度的 agent stdout/stderr 漏修。
- **修复**：按字节读 + 单行上限（如 8 MiB）超限截断 warn 跳过；stderr 改环形缓冲（保留最后 64 KiB 诊断）。

### S-6  MCP 临时配置文件非原子写 + symlink 攻击面 ⬜
- **位置**：`crates/claude/src/backend.rs:101-103`。`tokio::fs::write` 直接覆写，无 `O_CREAT|O_EXCL`；叠加 v3 P3-2（不清理 + 文件名泄漏 conv_id）。
- **修复**：`OpenOptions::new().write(true).create_new(true)` 拒绝已存在；或 temp+rename 原子替换；运行结束清理。

### R-1  优雅退出 drain 30s ≪ agent_timeout 600s（P1-5 半写防护对长任务未兑现）⬜
- **位置**：`crates/core/src/dispatch.rs:259-267`。drain 硬编码 30s，而 JoinSet 里 handle 含 backend.run（超时 600s）。>30s 任务被 abort_all → kill_on_drop 杀子进程 → 半写风险仍在。
- **修复**：drain 超时与 agent_timeout 关联，或分级（停接收 → 等当前 chunk → 超时杀）。

### R-2  socket_accept / handle_permission_socket 未纳入 JoinSet，SIGTERM 不 drain ⬜
- **位置**：`crates/core/src/dispatch.rs:313,327` 裸 `tokio::spawn`，不在 `self.tasks`。
- **修复**：纳入 JoinSet 或独立跟踪，drain 时一并处理。

### R-3  main 退出路径无 cleanup（不 unlink permission.sock / 不 close store）⬜
- **位置**：`src/main.rs:234-245`。P1-5 计划 ③④ 未落地；runtime abort 时 SQLite WAL checkpoint 不保证执行，可能丢最后写入。
- **修复**：退出前 unlink sock + 显式 store close/flush。

### R-4  状态机多步 store 操作无事务 ⬜
- **位置**：`/switch`（dispatch.rs:744-771）`upsert_session`→`set_config`；落库（:1147-1173）`upsert_session`→`upsert_named_session`。两步无事务，中途失败留不一致。
- **修复**：store 提供事务 API，core 在 mutator 内单事务提交。

### R-5  WeCom subscribe ack 认证失败静默不重连 ⬜
- **位置**：`crates/wecom/src/client.rs:117-147`。认证失败仍进入收发循环空转发心跳，进程存活但无 inbound，`/health` 不报错，消息静默丢失。
- **修复**：`if !authed { return Err(...) }` 触发外层重连。

### R-6  WeCom inbound channel 满静默丢帧（永久丢失）⬜
- **位置**：`crates/wecom/src/client.rs:191` `try_send` 失败丢帧，丢帧前 dedup 已记 msgid → 重启也不重投。
- **修复**：`send().await` 背压；或丢帧时不入 dedup 保留重投；或满时断连重连触发服务端重发。

---

## 🏛️ 架构评价

**优点**（无需改动）：双 trait 抽象 + 依赖倒置干净；Backend 无状态、session 生命周期在 core；`SessionExpired` 类型化判定（优于 ilink platform.rs:623 的 `contains("SESSION_EXPIRED")` 子串匹配）；`spawn_cli_backend` 消除三后端重复；conv 级串行锁；生产代码仅约 11 处 `unwrap/expect/panic`（多在 LazyLock metric 注册）；fail-closed 倾向一致。

**结构性建议**（第三波重构）：
1. **状态机收敛 + 事务**：session 状态散在 `sessions` + `named_sessions` + `config(active_name/compact_summary)` 三表无事务（R-4 / 原 P1-7/P2-2/P2-3/P2-6 共同温床）→ per-conv `ConvState` + 单 mutator + 单事务。
2. **失败路径 RAII 统一**：`conv_locks`/`PermissionRouter`/`permission.sock` 三处泄漏根因都是「正常路径清理在 return 之后」→ RAII guard。
3. **进程退出语义完整**：SIGTERM + drain 全 task（含 socket task）+ unlink sock + store close（R-1/R-2/R-3 是一整套）。
4. **后端安全语义统一**：CLI vs ACP 的 `allowed_tools`/`Off` 语义不一致（S-1 + N12）→ trait 层定义统一「工具策略」。
5. **`ReplyHint::ILink` 泄漏到 core 类型**（v3 已提）→ 关联类型/泛型。
6. **子进程边界对称加固**：v3 只收紧 permission socket，agent stdout/stderr/env 是对称漏修面（S-2/S-5）。

---

## 🟡 第三波 — 打磨项（概括）

- **N4** metrics 漏计权限询问等直接 send_text（dispatch.rs:479 绕过 reply）。
- **N5** `Cmd::Mcp` 内部子命令未 `#[command(hide = true)]`（main.rs:56）。
- **N6** discovery 模式回引导无限流，可能触发风控（dispatch.rs:554-568）。
- **N10** mcp `run_mcp_server` 同步 `stdin.lock().lines()` 阻塞 worker（v3 P3-9 确认）。
- **N18** metrics 命名硬编码 "claude" 但计所有 backend（metrics.rs:38-49）→ 改 `imagent_backend_*` 或加 `backend` label。
- **N8** spawn_cli_backend 在 final_text 非空时忽略非零退出码（backend_common.rs:140-159）。
- **N9** CLI 后端 `chunks.send().await` 无超时，慢消费者拖死读循环（backend_common.rs:96,144）；与 ACP 的 `try_send` 策略不一致。
- **N12** `Off` 模式跨后端语义不一致（CLI 不挂 MCP vs ACP 全放行）→ 文档明确。
- **P3-2** write_mcp_config 临时文件不清理 + 文件名泄漏 conv_id（v3 defer，agent 2 确认未修）。
- **P2-10** 缺 `delete_credential`/logout，凭据永驻（v3 defer）。
- **P3-5/P3-6** keyring marker `starts_with` 撞名 + `cfg!(test)` 让 keyring 测试恒失败（v3 defer）。
- **N10-acp** `IMAGENT_ACP_COMMAND` env 可替换后端 spawn 命令（acp.rs:88-93）→ basename 白名单。
- **N11** store 单 Connection + Mutex 串行，WAL 读并发优势未用（store.rs:72-74）。
- **P3-N2** WeCom 出站 markdown 未转义（proto.rs:124-138）→ agent 输出注入渲染。
- **P3-N3** WeCom ws Ping 未回 Pong（client.rs:160）→ 潜在频繁重连。
- **P3-N4** ilink `post_json` 响应体无大小上限（client.rs:98）。
- **P3-N6** wecom ws_url 缺 host 白名单（defense in depth，当前硬编码不可配）。
- **N6-store** `append_audit` 每次跑 O(N) 轮转 DELETE（store.rs:547-553）。
- **N7-store** credential_put vs credential_migrated 审计 detail 格式不一致。
- **P1/P2/P3 治理** 各 crate 缺 `#![warn(missing_docs)]`；README 无 codecov badge；`imagent --version` 未挂；CODEOWNERS 占位；SECURITY.md 位置；deny.toml schema；thiserror 钉 1.x 但传递依赖引入 2.0.18；P2/P3_ROADMAP 过时应归档。

---

## ⚠️ 破例说明（生产代码主会话实现）

继承 `CODE_REVIEW_v2.md`/`v3.md` 顶部先例：omp 工具链在本项目累计 **8 次异常**（含 3 次「空手退出」exit 0 零产出）。依项目根 `CLAUDE.md`「生产代码改动依 CODE_REVIEW_v2 顶部先例破例主会话实现（方案设计到位 + `cargo test` 验证 + commit 注明待 review）」，本轮生产代码修复**破例主会话自行 Edit**。违反全局 CLAUDE.md omp 委派硬规则，请 review。每项修复均：① 基于本轮已 review 的方案；② `cargo test --workspace` 验证；③ commit message 注对应 issue id + 待 review；文档/配置/CI 类直接改。
