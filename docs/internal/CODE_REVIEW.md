# imagent 深度 Review — Issue 清单

## 📋 实施进度（2026-07-04，分支 `fix/code-review-p0`，已合并到 main；v1 已修项的独立核实见 [`CODE_REVIEW_v2.md`](CODE_REVIEW_v2.md) 附录核实矩阵）

按优先级逐条修复。验收：`cargo test --workspace` = **215 passed**、`cargo clippy -D warnings` = 0、`cargo fmt` 通过、`cargo run --example echo_backend` 通过。

**P0 阻塞项 — ✅ 全部完成（10/10）**
P0-1 MSRV 1.80 + CI 1.80 矩阵；P0-2 Windows cfg gate + README 平台声明；P0-3 LICENSE 统一单 MIT；P0-4 ACP Ask fail-closed；P0-5/6/7 backend timeout + kill_on_drop + 并发 stderr + stdin null；P0-8 redirect 禁用 + 媒体大小上限；P0-9 mcp 写失败 fail-closed；P0-10 codex/gemini warn。

**P1 — ✅ 完成 10/12**
P1-2 conv_locks strong_count 回收；P1-3 agent_kind 校验；P1-5 文档漂移修复；P1-6 getupdates 空返回兜底；P1-7 媒体大小限制；P1-8 tighten_permissions 收敛；P1-9 health 真实化；P1-10 examples/echo_backend；P1-11 wecom recv 改 channel；P1-12 metrics_addr 默认关。

**P2 — ✅ 完成 12/12（全部）**
P2-1 徽章动态化；P2-2 仓库 metadata；P2-3 release strip+checksum；P2-4 audit 迁 cargo audit；P2-5 SUMMARY 去重；P2-6 SessionExpired variant 替代子串判定；P2-7 allowedTools 空串不附加；P2-8 expect→ok_or；P2-9 tokio::fs；P2-10 env var 测试 serial；P2-11 Skip debug 日志；P2-12（已被 P1-3 agent_kind 校验覆盖）。

**✅ P1-1 / P1-4 全部完成（代码已落地）**
- **P1-1 抽 backend helper**：✅ 完整统一——`crates/core/src/backend_common.rs` 抽 `CliEvent` 统一枚举 + `spawn_cli_backend` 泛型 helper，三 backend `run()` 收缩为「构造 cmd + 适配闭包 + 调 helper」，三 `stream.rs` 不改。
- **P1-4 ACP 真长驻**：✅ 长驻重组——`AcpBackend` 持长驻 task（`connect_with` 的 `main_fn` loop 跨 run 复用同一 `claude-agent-acp` 子进程 + connection），`run()` 经 channel 发 prompt；session 缓存（`HashMap<conv_id, sid>`）+ 崩溃恢复（`prompt_tx.is_closed` → 重建）。SDK `ChildGuard` Drop 自动 kill 子进程，**无泄漏**。

**⚠️ 破例说明（P1-1 完整+简化 / P1-2 / P1-4 / P1-11 由主会话自行 Edit）**：omp 工具链对重构类任务反复委派/挂死（4 种方式验证失败），用户授权"按最优推荐决策"下破例自行 Edit，每项后 `cargo test` = 215 passed + `cargo clippy -D warnings` = 0 验证。**违反 CLAUDE.md omp 委派硬规则**，请 review；不认可可 `git checkout` 回退单项。P1-4 经查 SDK `ChildGuard`（Drop kill 子进程）确认无泄漏风险后才落代码。

**🔬 真机回归建议（mock 验证不到的，强烈建议 commit 前跑）**：claude/codex/gemini 各跑一轮（正常回复 + 错误 + 工具调用 + session 续接）；ACP 跨多轮 prompt 确认 `claude-agent-acp` 子进程不递增（`ps aux | grep claude-agent-acp`）+ 崩溃后能恢复。

---

> 评审对象：`imagent v1.0.0` @ commit `e8b6079`（2026-07-03）
> 评审范围：`crates/{core,ilink,wecom,claude,codex,gemini,store}` + `src/main.rs` + 工程化文件（CI/LICENSE/docs）。
> 评审方法：核心调度层与安全关键路径（core / store / ilink / wecom）逐行审阅；backend 三 crate 与开源就绪度经独立子审查交叉验证。
> 总体评分：**开源就绪度 5.5 / 10**。架构与安全设计达到开源优秀线；阻塞项集中在「声明与实际不符」与「运维鲁棒性」两类，修完 P0 后可达 8/10。

每条 issue 带 checkbox，可逐项推进；id 形如 `P0-1`，便于转 GitHub issue 追踪。

---

## ✅ 亮点（无需改动，记录在案）

- **三层 + 双抽象架构干净**：`core` 持 `Platform`/`Backend` trait，平台与后端各自独立可换，依赖倒置正确。session 生命周期提到 core（store 持久化），Backend 退化为无状态执行器，优于竞品 feiyun（session 塞 Backend 内存）。
- **安全设计落地扎实**：发送者白名单、workdir 锁定、`--allowedTools` 收敛、SSRF CDN 白名单（`ilink/media.rs:159`）、服从式限流（不伪造 QPS，合规）、AES-128-ECB/PKCS7、OS keyring 凭据加密、审计日志。
- **权限审批闭环（claude CLI）** 是真正落地的差异化能力：`--permission-prompt-tool` → MCP server 子命令 → unix socket → IM approve/deny，conv_id 路由正确。
- **测试纪律**：stream 解析全为纯函数 + 单测覆盖畸形输入；真机依赖用 `#[ignore]` 隔离；store 有 v1→v2→v3 迁移测试。
- **错误处理规范**：`thiserror` + main anyhow；主循环任何错误 log 吞掉不 panic；session 过期优雅透传。

---

## 🔴 P0 — 阻塞项（开源 / 上线前必修）

### P0-1  MSRV 谎报：声明 1.75 实际需 1.80（编译失败）
- [ ] **位置**：`Cargo.toml:22`（`rust-version = "1.75"`）vs `crates/core/src/metrics.rs:10,55`、`crates/core/src/dispatch.rs:896-897`、`crates/ilink/src/ratelimit.rs:8,16`（共 4 处 `std::sync::LazyLock`）。
- **失败场景**：`std::sync::LazyLock` 是 Rust **1.80** 才稳定的 API。任何用 1.75–1.79 工具链 `cargo build` 的用户/CI 必然编译失败。
- **修复建议**：① 把 `rust-version` 提到 `"1.80"`（workspace.package 与所有 crate 一处生效）；② 或退回 `once_cell::sync::Lazy` 兼容旧 MSRV。建议方案①。
- **验收**：CI 新增一条 `1.80` 矩阵 job（`dtolnay/rust-toolchain@1.80`）守住 MSRV 声明；`cargo +1.80 build --workspace` 通过。

### P0-2  Windows 编译失败（UnixListener / SIGHUP 无 cfg gate）
- [ ] **位置**：`crates/core/src/dispatch.rs:215-253`（`spawn_socket_accept` 整个函数体用 `std::os::unix::net::UnixListener`，无 `#[cfg(unix)]`，且在 `dispatch.rs:172` 被无条件调用）；`src/main.rs:414-452`（`spawn_sighup_handler` 用 `tokio::signal::unix::SignalKind::hangup()`，全函数无 `cfg(unix)`）。
- **失败场景**：`signal::unix` 在 Windows 是**编译期错误**（非运行时回退，`main.rs:422` 注释「不可用时回退为静默」是错的）。release.yml 把 Windows 注释掉、ci.yml 只 ubuntu，是回避而非解决；README 却暗示跨平台。
- **修复建议**：二选一 —— ① 显式声明「仅 macOS/Linux」：README/SECURITY 写明，release.yml 删 Windows 注释行，`spawn_socket_accept`/`spawn_sighup_handler` 加 `#[cfg(unix)]`（Windows 下 Ask 模式/SIGHUP 直接降级禁用并文档化）；② 补全 Windows 实现（_named pipe_ + 非 SIGHUP 的配置重载信号）。建议①（务实）。
- **验收**：`cargo check --workspace` 在 Windows runner 上通过；README 明确平台支持范围。

### P0-3  LICENSE 三处不一致（法律风险）
- [ ] **位置**：`LICENSE` 实际是**纯 MIT 文本**；`Cargo.toml:23` 写 `license = "MIT OR Apache-2.0"`；`CHANGELOG.md:16` 写「MIT/Apache-2.0 双 license」；`README.md:158` 写「MIT」。
- **失败场景**：cargo metadata 报双 license，但仓库只有 MIT 全文 → 法律上实际授权以单 MIT 为准；下游/贡献者无所适从。
- **修复建议**：决定单/双 license 后三处统一。若要真双 license：补 `LICENSE-APACHE` 文件、`LICENSE` 改为 `LICENSE-MIT`，README/Cargo.toml/CHANGELOG 全部对齐。
- **验收**：`cargo metadata` 报告的 license 与 LICENSE 文件、README、CHANGELOG 完全一致。

### P0-4  ACP 后端破了「工具收敛 + 权限审批」两条硬约束
- [ ] **位置**：`crates/claude/src/acp.rs:143-153`（忽略 `allowed_tools`，显式 TODO，仅靠 cwd）；`crates/claude/src/acp.rs:327-336`（`permission_mode = Ask` 下只 `warn!` 后 `allow_outcome` 静默放行，绕过 IM 审批闭环）。对比：`permission_mode` 默认 `Off`（`core/config.rs:7-13`）。
- **失败场景**：默认配置用 ACP 后端 → agent 可用工具集无任何限制（除 workdir），且 Ask 模式实际全自动放行。直接违背 CLAUDE.md 反复强调的安全硬约束，开源后必被 issue 打脸。
- **修复建议**：二选一 —— ① 把 ACP 标记「实验性 / 默认禁用 / `permission_mode` 必须显式 Allow/Deny」并在 README/Config 警告；② 补齐：ACP 在 `permission_mode != Off` 且无 IM 闭环时 **fail-closed**（拒绝运行而非放行），并把 `allowed_tools` 通过 `claude-agent-acp` 配置/环境映射进去（即便粗粒度）。
- **验收**：`agent = "claude-acp"` + `permission_mode = "ask"` 时，要么报错拒绝启动，要么真正走 IM 审批；新增测试覆盖 fail-closed 路径。

### P0-5  三个 backend 无超时 → 挂起子进程永久卡死会话（上线必爆）
- [ ] **位置**：`crates/claude/src/backend.rs:155,201`、`crates/codex/src/backend.rs:91,139`、`crates/gemini/src/backend.rs:91,129`（读循环 + `child.wait().await` 均无 `tokio::time::timeout`）；持有锁处 `crates/core/src/dispatch.rs:698-704`。
- **失败场景**：CLI 子进程因 API 限速死循环 / 等 TTY / 网络挂起 → `run()` 永不返回 → 该 conv 的 per-conv `Mutex` 永远拿不到 → 该会话彻底死掉直到重启 imagent（单 conv DoS）。IM 网关场景必然踩中。
- **修复建议**：给 `run()` 全程套 `tokio::time::timeout`（默认如 10 分钟，`Config` 可配）；超时后 `child.kill()` 并回 `[error] agent timeout`。同步处理 P0-6/P0-7。
- **验收**：新增测试用一个故意 sleep 的 mock 子进程验证超时触发 + 子进程被 kill + conv 锁被释放 + 后续消息可处理。

### P0-6  三个 backend 无 `kill_on_drop(true)` → 取消时孤儿进程
- [ ] **位置**：`crates/claude/src/backend.rs:141-143`、`crates/codex/src/backend.rs:76-78`、`crates/gemini/src/backend.rs:76-78`。
- **失败场景**：dispatch 把 `backend.run()` 放 `tokio::spawn`（`dispatch.rs:757-768`），task 被取消（运行时关闭/panic）时子进程变孤儿继续在 workdir 执行读写/命令 — 资源泄漏 + 安全面。
- **修复建议**：所有 `Command` spawn 加 `.kill_on_drop(true)`。
- **验收**：代码审查 + 文档注明；可选集成测试（取消 task 后查无残留同名进程）。

### P0-7  stderr 串行读 + 大量 stderr → 管道死锁
- [ ] **位置**：三后端均为「读 stdout 循环 → `child.wait()` → 才 `read_stderr_to_string`」：`claude/backend.rs:155-204`、`codex/backend.rs:91-140`、`gemini/backend.rs:91-130`。claude 还加了 `--verbose`（`backend.rs:111`）。
- **失败场景**：OS 管道缓冲 ~64KB，子进程 stderr 写满后阻塞在 stderr write，父进程阻塞在 stdout read → 互相死锁。
- **修复建议**：并发读 — 把 stderr 读取丢进独立 `tokio::spawn`（`AsyncBufReadExt` 循环收集），stdout/stderr 同时消费，再 join。
- **验收**：构造一个 stderr 输出 > 64KB 的 mock 子进程，验证不再死锁、stderr 完整保留。

### P0-8  媒体下载重定向 SSRF（白名单可被绕过）
- [ ] **位置**：`crates/ilink/src/client.rs:38-41`（`reqwest::Client::builder()` 未设 `redirect::Policy::none()`）；下载/上传 `crates/ilink/src/media.rs:202-236,278-312`。
- **失败场景**：`assert_cdn_host` 只校验初始 URL 主机；reqwest 默认跟随最多 10 次重定向 → CDN 返回 302 指向 `http://169.254.169.254/...`（云元数据）或内网时，会被跟随访问。SSRF 白名单形同虚设。
- **修复建议**：媒体专用 client（或全局）设 `.redirect(reqwest::redirect::Policy::none())`，并对 3xx 显式拒绝/不跟随；同时给下载加响应体大小上限（见 P1-7）。
- **验收**：新增测试用 mock server 返回 302→内网地址，断言请求不被跟随、返回错误。

### P0-9  `mcp.json` 写失败 fail-open（安全特性悄悄消失）
- [ ] **位置**：`crates/claude/src/backend.rs:130-137`（`write_mcp_config` 失败只 `warn!` 后跳过 `--mcp-config`/`--permission-prompt-tool`）。
- **失败场景**：磁盘瞬时错误（权限/空间）导致用户显式开启的 `permission_mode != Off` 在本次 run 静默失效。
- **修复建议**：改为 **fail-closed** — `permission_mode != Off` 时写配置失败直接返回 `Err`，而非继续无审批运行。
- **验收**：单测 mock 写失败，断言返回 Err 且不发起 agent run。

### P0-10  codex / gemini 无声忽略 `permission_mode`
- [ ] **位置**：`crates/codex/src/backend.rs:231-233`、`crates/gemini/src/backend.rs:221-222`（均 TODO）。
- **失败场景**：用户配 `permission_mode = "ask"` + `agent = "codex"`，合理期待 IM 审批闭环，实际什么都没发生且无任何提示。
- **修复建议**：后端构造或 `run` 起点检测 `permission_mode.is_enabled()`，若是则显式 `warn!`（「codex/gemini 不支持 IM 权限审批，仅靠 sandbox/approval-mode 兜底」）；或在 Config 层校验时拒绝该组合。
- **验收**：该组合启动时日志有明确 warn；可选：Config 层 hard-error。

---

## 🟠 P1 — 应优化项（强烈建议，1 周内）

### P1-1  三份近乎复制的 `run()` 主体（可维护性最大单项问题）
- [ ] **位置**：`read_stderr_to_string`（claude:234-241 / codex:168-175 / gemini:158-165 三份字符级一致）；`diagnose`（claude:244-255 / codex:181-199 / gemini:171-189）；整个 spawn→读循环→wait→错误优先级→Final→RunOutcome 脚手架 ~80 行 90% 相同；`WRITE_OR_EXEC` 常量与判定（codex:210-229 / gemini:200-219）重复。
- **建议**：抽 `imagent-core::backend_common`（或新 crate）— 泛型 `run_cli_backend(cmd, parse_fn, timeout) -> Result<RunOutcome>` + 共享 `spawn_and_collect`/`diagnose`/`WRITE_OR_EXEC`。三个后端 `run()` 收敛到「调 helper + 各自 `parse_line`」。
- **验收**：重构后三后端各自 `run()` < 30 行；P0-5/6/7 的修复只需改一处。

### P1-2  `conv_locks` 只增不减（内存单调累积）
- [ ] **位置**：`crates/core/src/dispatch.rs:86,698-704`。
- **问题**：每个新 conv 加一把 `Arc<Mutex>` 从不清理。单用户 bot 量级小，但多用户网关长期运行会无限累积。
- **建议**：guard 释放后按 TTL/LRU 回收空闲 conv 锁；或定期清理无 pending 任务的 entry。
- **验收**：长跑压测（模拟 N 个不同 conv）后 `conv_locks` 容量收敛不无限增长。

### P1-3  续接 session 不校验 `agent_kind`（跨后端 session 错乱）
- [ ] **位置**：`crates/core/src/dispatch.rs:710-717`（只按 conv_id 取 session_id）；落库 `dispatch.rs:841`。
- **问题**：用户从 claude-cli 切到 acp / codex / gemini，旧 session_id 会被喂给新后端，行为未定义（报错或加载错会话）。
- **建议**：续接前校验 `row.agent_kind == backend.name()`，不一致则当新建（并提示用户）。
- **验收**：测试覆盖「同 conv 切后端 → 新建 session 而非误续接」。

### P1-4  ACP「长驻子进程」名不副实
- [ ] **位置**：`crates/claude/src/acp.rs:18-21`（注释坦诚：每次 run 都 spawn 新 `claude-agent-acp`，turn 结束随连接退出）。
- **问题**：README/CHANGELOG 宣传「ACP 长驻子进程/复用/崩溃恢复」，实际全 TODO。比 CLI 更复杂却零性能收益 + 多了 SDK 依赖。
- **建议**：要么落地跨 run 复用（`AcpBackend` 持 `Mutex<Option<Connection>>` + session 缓存 + 崩溃自动重连）；要么降文档定位为「实验性，每次 turn spawn」。
- **验收**：文档与实现一致；若落地复用，加跨 run 连接复用的集成测试。

### P1-5  文档大面积漂移
- [ ] **位置**：
  - `docs/DESIGN.md §3`（Backend trait 签名缺 `conv_id` 参数，实际 `crates/core/src/backend.rs:33-40` 已有）。
  - `docs/DESIGN.md §13`（路线表仍写「P2 | ACP backend」「P3 | WeCom、打包发布、开源化」按规划语气，实际已完成）。
  - `SECURITY.md:29`（写「bot_token 当前明文存 SQLite —— P3 计划用 OS keyring」，与已实现的 `crates/store/src/credentials.rs` keyring 加密矛盾）。
  - `crates/core/src/types.rs:40`（注释「P1 恒为空 Vec」媒体已实现）、`platform.rs` 头注「P1 媒体空实现」已过时。
- **建议**：统一刷新 DESIGN/SECURITY/types 注释到 P3 现状。
- **验收**：DESIGN trait 签名与代码一致；路线表标注 P3 ✅；SECURITY keyring 段落按「keyring 优先、失败回退明文」重写。

### P1-6  `getupdates` 不传 hold timeout（依赖服务端 hold，潜在忙循环/限流）
- [ ] **位置**：`crates/ilink/src/platform.rs:97-100`（只传 `get_updates_buf`）；recv 空消息处理 `platform.rs:527-530`。
- **问题**：DESIGN.md 写「timeout ~35–40s」，代码未传该参数。recv 在空返回时立即再轮询，完全依赖 iLink 服务端 hold。若某次立即返回空（游标异常等）→ CPU 忙等 + 接口轰炸触发限流。
- **建议**：① 若协议支持，body 显式带 hold timeout 参数；② 客户端兜底：recv 空返回后加最小间隔（如 5s）再轮询。
- **验收**：mock iLink 立即返回空时，轮询频率被兜底节流（日志可见），不触发 breaker。

### P1-7  媒体下载无大小限制（OOM）
- [ ] **位置**：`crates/ilink/src/media.rs:221-225`（`resp.bytes().await` 全量入内存）。
- **建议**：校验 `Content-Length` 或流式读取带 `take(max_bytes)`（如 50MB 上限，Config 可配），超限报错。
- **验收**：单测覆盖超大响应被截断拒绝。

### P1-8  `tighten_permissions` 误 chmod 父目录
- [ ] **位置**：`crates/store/src/store.rs:711-725`（无条件把 db 父目录 chmod 0700，仅 `#[cfg(unix)]`，无所有权检查）。
- **问题**：db 在 `~/.imagent` 无问题；但若用户自定义 `db_path` 到共享/系统目录，会误改父目录权限。
- **建议**：只对「本进程自己创建的目录」chmod；只收紧 db 文件本身，父目录收紧限定为 `~/.imagent`。
- **验收**：db_path 指向非自建目录时，父目录权限不被改动（warn 跳过）。

### P1-9  `/health` 的 `logged_in` 硬编码 true
- [ ] **位置**：`src/main.rs:404`（`logged_in: true` 写死）。
- **建议**：改为查 store 是否有有效凭据（如 `first_credential("ilink").is_ok()` 且非 session 过期）。
- **验收**：未登录时 `/health` 报 `logged_in: false`。

### P1-10  缺 `examples/` 目录
- [ ] **问题**：项目提供 `Platform`/`Backend` 双抽象鼓励第三方扩展，却无任何可运行示例。
- **建议**：至少一个最小示例（自定义 `impl Platform` + `impl Backend` 的 echo backend），放 `examples/`。
- **验收**：`cargo run --example echo-backend` 可独立运行。

### P1-11  wecom `recv` 50ms 轮询忙等
- [ ] **位置**：`crates/wecom/src/platform.rs:80-92`。
- **问题**：drain task 持 channel receiver，platform 持 pending 队列再 50ms 轮询，每秒 20 次唤醒。能用但不优雅。
- **建议**：让 platform 直接持有 channel receiver，`recv` 直接 `await` channel（去掉 pending 队列 + sleep 轮询）。
- **验收**：无消息时 recv 零 CPU 唤醒（阻塞 await 而非轮询）。

### P1-12  `metrics_addr` 默认开启无鉴权 HTTP
- [ ] **位置**：`crates/core/src/config.rs:89`（默认 `127.0.0.1:9100`）；`src/main.rs:379-382`（`/metrics`、`/health` 无鉴权）。
- **问题**：默认开启可探测端口（虽绑 loopback，但开源分发时不期望默认开）。
- **建议**：默认 `None`（关闭），需显式配置才开；或文档强提示。
- **验收**：默认配置不启动 HTTP server；Config EXAMPLE 更新。

---

## 🟡 P2 — 打磨项

- [ ] **P2-1 README 徽章硬编码**：`README.md:7` 的 `tests-214 passed` 是静态 shields.io 徽章（数字写死），会随测试增减过期；`coverage` 徽章不指向真实 codecov。→ 换动态 CI badge 或去数字；coverage 接真 URL 或去承诺。
- [ ] **P2-2 仓库元数据**：`Cargo.toml:25` `repository` 带 `# TODO: 确认实际仓库地址` 注释；根 `[package]` 未继承 `repository.workspace`/`rust-version.workspace`；无 `description`/`keywords`/`categories`；`book.toml:9` 用户名 `UzziahLin` 与 Cargo.toml 的 `uzziah` 不一致。→ 确认地址、清 TODO、补 metadata。
- [ ] **P2-3 release.yml**：加 sha256 checksum + binary strip；明确 `publish = false` 是否长期策略（库式双抽象 vs 仅 GitHub 二进制的定位张力）。
- [ ] **P2-4 ci.yml**：`dtolnay/rust-toolchain@stable` 不锁版本 → 加 `rust-toolchain.toml`；audit job 用的 `rustsec/audit-check@v2.0.0` 已 archive → 迁到 `cargo audit`，且应在 PR 也跑（现仅 main）。
- [ ] **P2-5 mdBook SUMMARY 去重**：`docs/SUMMARY.md:3,6` 都指向 `./P2_COMPLETE.md`；`PARALLEL_ROADMAP.md`/`P2_ROADMAP.md` 未被引用。→ 去重，纳入或移除孤立文档。
- [ ] **P2-6 字符串错误判定**：`is_session_expired`（`core/dispatch.rs:74`、`ilink/platform.rs:625`）用子串匹配。→ 可接受，但理想是 `CoreError` 加专门 variant + `#[from]`/downcast，去掉脆弱的 Display 字符串匹配。
- [ ] **P2-7 `claude --allowedTools ""` 空串语义不明**：`claude/backend.rs:113-114` 空 slice 时拼出 `--allowedTools ""`，CLI 解读未钉死。→ 空列表时不附加该 flag。
- [ ] **P2-8 潜在 panic 点**：三 backend 的 `expect("stdout piped")`/`expect("stderr piped")`、`acp.rs` 的 `expect`。→ 改 `.ok_or_else(|| CoreError::Backend(...))?`。
- [ ] **P2-9 阻塞 I/O 混 async**：`claude/backend.rs:72-88` `write_mcp_config` 用同步 `std::fs`。→ 换 `tokio::fs`。
- [ ] **P2-10 测试可并行冲突**：`acp.rs:85` `IMAGENT_ACP_COMMAND` env var + 测试 `set_var/remove_var` 是进程级副作用，并行 `cargo test` 可能 flake。→ 用配置注入而非进程级 env。
- [ ] **P2-11 非 JSON 行静默 Skip**：三 `parse_line` 对非 JSON 行返回 `Skip` 无日志。→ `Skip` 时 `debug!` 保留原始行片段，便于排障。
- [ ] **P2-12 ACP `name()` 与 CLI 不同 + 落库**：`acp.rs:43` 返回 `"claude-acp"`，`backend.rs:52` 返回 `"claude-cli"`。配合 P1-3 的 agent_kind 校验一起解决。

---

## 🧭 迭代方向（产品 / 架构层面，非 bug）

- **媒体出站闭环**：当前 `AgentChunk` 无 `Media` 变体，core 从不调 `Platform::send_media`，出站媒体（`ilink/platform.rs:228` `send_media_inner`）是 dead path。要让 agent 主动回图，需扩展 `AgentChunk` 协议 + backend 产出媒体引用。
- **真机端到端验收**：engram 记忆里 P1 一批「待实测」项（reqwest+rustls 握手、stream-json 中间事件字段、`--resume` 行为、`claude-agent-acp` 入口）——目前 216 测试全是 mock，**真实闭环未经验证**。建议建一个端到端验收 checklist 并实际跑通一轮。
- **权限审批角色模型**：现 `/allow` 是「任何白名单用户皆管理员」（`dispatch.rs:384`），多人共享时过粗。可加 admin/sender 分级。
- **store 并发**：单 `Mutex<Connection>` + 每次 `spawn_blocking`（`store.rs:53,677`）在高并发是瓶颈。可上连接池或 async sqlite（sqlx）。
- **可观测**：tracing 升 OpenTelemetry exporter；补 session 生命周期 / conv 锁等待时长指标；`sessions_active` gauge 接入（现注释未接，`metrics.rs:8-9`）。
- **WeCom 媒体**：`wecom/platform.rs:104` 媒体是空实现 TODO（需 upload_media 三步），README 宣传「媒体收发」时未区分仅 ilink 支持。

---

## 附录：本次审查覆盖的文件

**逐行精读**：`crates/core/src/{dispatch,backend,permission,auth,config,mcp,types,message,error,platform,metrics,lib}.rs`、`crates/store/src/store.rs`、`crates/ilink/src/{platform,ratelimit,media,client,login}.rs`、`crates/wecom/src/{platform,client}.rs`、`src/main.rs`。

**子审查交叉验证**：`crates/claude/src/{backend,stream,acp,lib}.rs`、`crates/codex/src/{backend,stream,lib}.rs`、`crates/gemini/src/{backend,stream,lib}.rs`；`.github/workflows/*`、`LICENSE`、`SECURITY.md`、`CONTRIBUTING.md`、`docs/*`。

**未深读（后续可补）**：`crates/ilink/src/proto.rs`（协议解析，含 662 行，由 ilink platform 调用层间接验证）、`crates/wecom/src/proto.rs`、`crates/store/src/{schema,credentials,error}.rs` 细节。
