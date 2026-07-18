# imagent 深度 Review v6 — 开源首发收尾（v5 核实 + 新发现 + 打磨落地）

> **评审对象**：`imagent` @ `fdecc4d`（main，已合并 `fix/code-review-v5`）。
> **评审标准**：开源首发——陌生开发者能否 clone → build → PR → 信任依赖 → 跑通 release。
> **评审范围**：`crates/{core,ilink,wecom,claude,codex,gemini,store}` + `src/main.rs` + 开源工程化层（CI / 供应链 / Cargo metadata / 文档 / deploy / fuzz）。
> **评审方法**：主会话逐行精读核心调度链路（auth / permission / dispatch / backend_common / mcp / config / store / credentials / main / claude-backend / acp）+ 实跑 `cargo test --workspace` / `cargo clippy` / `cargo fmt --check` / `cd fuzz && cargo +nightly check` / `cargo audit` / `cargo tree -d` 验证声明真实性；对 v5「已修」项逐 file:line 复核。
> **与 v5 的关系**：先独立核实 v5 声称已修项是否真落地（结论：**v5 无谎报**——这是相对 v1 谎报、v2/v3 部分修报、v4 半修的实质性进步），再把 v5 第三波「打磨项」确认现状并落地，最后补 v5 未覆盖的新发现（2 处上线前结构化缺口 + 5 处文档门面）。
> **总体评分**：代码实现 **8.5/10**（架构干净、241 测试、`forbid(unsafe_code)`、fail-closed 一致、命令注入/SSRF/TLS 防线到位）；**安全 8/10**（硬约束扎实、威胁建模诚实，遗留 `wecom_secret` 明文 + 1 处可观测性结构化缺口 + metrics 默认安全姿态）；**开源就绪 7/10**（门面齐全，但**发布渠道仍不存在**——无 remote/tag 是唯一硬阻塞）。

## 🎯 核心洞察（第一性原理）

v1–v4 的核心病灶是「工程诚信」（谎报/半修），v5 已治愈——本轮逐行复核 + 实跑验证，**v5 声称的 F1/F2/F4/F5/F7/F8/N8/S-5/S-3/S-6 全部真修、无谎报**。代码与安全设计已达到开源标准。

**距离「陌生人 clone → build → PR → release 跑通」只差两件事**：
1. **一个 git remote + tag**（O1，唯一硬阻塞，非代码改动，需仓库 owner）；
2. **几处上线前打磨**：本轮新发现 2 个（R1 崩溃语义未结构化传递、R2 metrics 默认安全），外加 v5 第三波已知未修的 7 个打磨项。

代码层面没有新的高危问题。v6 的增量价值在：① 核实 v5 诚信；② 把 v5 第三波打磨项落地；③ 补 2 个上线前结构化缺口；④ 5 处文档门面细节（含一处真实运维踩坑：macOS `imagent` 撞名系统输入法）。

## 📋 修复进度（分支 `fix/code-review-v6`）

| ID | 级别 | 标题 | 位置 | 负责人 | 状态 |
|---|---|---|---|---|---|
| **O1** | 🔴 阻塞 | 无 git remote + 无 tag（发布渠道不存在） | `git remote -v` 空 / `tag=0` | 仓库 owner | ⬜ |
| **R1** | 🟠 上线前 | `RunOutcome` 缺状态字段——N8 崩溃语义无法传到 IM | `backend_common.rs` / `types.rs` / `dispatch.rs` | omp | ✅ |
| **R2** | 🟠 上线前 | metrics / health endpoint 无鉴权 + 无绑定校验 | `main.rs` | omp | ✅ |
| **R3** | 🟠 上线前 | `wecom_secret` 明文存 config.toml（SECURITY 未列） | `config.rs` / `SECURITY.md` | 文档 ✅ / keyring 后续 | 🟡 |
| **P1** | 🟡 打磨 | ilink 游标 at-most-once（crash 丢消息） | `ilink/platform.rs` | omp | ✅ |
| **P2** | 🟡 打磨 | `WsFrame` subscribe body 未 redact（Debug footgun） | `wecom/proto.rs` | omp | ✅ |
| **P3** | 🟡 打磨 | wecom `ws_url` 整串进 INFO 日志 | `wecom/client.rs` | omp | ✅ |
| **P4** | 🟡 打磨 | ilink `post_json` 响应体无大小上限 | `ilink/client.rs` | omp | ✅ |
| **P5** | 🟡 打磨 | WeCom 出站 markdown 未转义 | `wecom/proto.rs` | 文档化 | 🟡 |
| **P6** | 🟡 打磨 | mcp `stdin.lock().lines()` 在 async fn 同步阻塞 | `mcp.rs` | omp | ✅ |
| **P7** | 🟡 打磨 | `IMAGENT_ACP_COMMAND` env 可换 spawn 命令无白名单 | `acp.rs` | 文档化 | 🟡 |
| **D1** | 🔵 文档 | README 写死 "241 passed"（会漂移） | `README.md` | 我 | ✅ |
| **D2** | 🔵 文档 | workspace `Cargo.toml` 顶部注释过时（P1 残留） | `Cargo.toml` | 我 | ✅ |
| **D3** | 🔵 文档 | `docs/P2_COMPLETE.md` 漏移（仍在根 + 进 SUMMARY） | `docs/` / `SUMMARY.md` | 我 | ✅ |
| **D4** | 🔵 文档 | macOS `imagent` 撞名警告仅在 deploy/README | `README.md` | 我 | ✅ |
| **D5** | 🔵 文档 | SECURITY.md 未标注 ACP `allowed_tools` 无效 / Off 全放行 | `SECURITY.md` | 我 | ✅ |
| **A1** | 🏛 架构 | session 状态散三表无事务（v4-R4） | `store.rs` / `dispatch.rs` | 后续 | ⬜ |
| **A2** | 🏛 架构 | 后端安全语义统一（CLI/ACP/Off 分裂） | `backend.rs` trait | 后续 | ⬜ |
| **A3** | 🏛 架构 | `ReplyHint::ILink` 泄漏 core 类型 | `types.rs` | 后续 | ⬜ |

**✅ 落地核实（2026-07-18，分支 `fix/code-review-v6`）**：R1/R2/P1/P2/P3/P4/P6 经 omp 实现 + 主会话逐行 Review + `cargo test --workspace`（**242 passed**）/ `clippy` 0 warning / `fmt` clean 验证，按功能域分 5 个提交（`150b2f7` R1 / `a7200c9` R2 / `e494337` ilink P1+P4 / `313c548` wecom P2+P3 / `dbff92d` core P6）；D1-D5 + R3-文档随 `0d5b935`。P5/P7 评估后**文档化**（不强制转义/白名单——见对应小节）。**仅 O1（git remote + tag）待仓库 owner 操作**；架构建议 A1-A3 留 v1.1+。

**分工**（按全局约定）：`.md` 文档 + 配置由主会话直接改；`.rs` 生产代码委派 omp 实现，主会话负责方案、Review、`cargo test` 验证。

---

## ✅ v5 核实结论（工程诚信）

**本轮逐 file:line 复核 + 实跑验证：v5 声称已修项全部真修、无谎报。**

| v5 声明 | 核实 | 证据 |
|---|---|---|
| F1 fuzz 编译 | ✅ 真修 | `ilink/lib.rs:20-22` `pub mod media/proto`；fuzz target 调真实 `UpdatesResp` + `extract_text`；`cd fuzz && cargo +nightly check` **exit 0**。 |
| F2 cargo-audit | ✅ 真修 | `ci.yml` audit step `cargo audit --ignore RUSTSEC-2024-0437` + justification，无 `if`，PR 阻塞。 |
| F4 ci deny 阻塞 PR | ✅ 真修 | deny job 无 `if:`，`cargo deny check licenses sources bans`。 |
| F5 CODEOWNERS + 文档治理 | ✅ 真修 | `CODEOWNERS = @uzziah`；v1/v2/v3 + P1/P2/P3/PARALLEL 移 `internal/`。 |
| F7/F8 deploy | ✅ 真修 | `deploy/README` 日志 `/usr/local/var/log`；metrics 默认关闭；systemd `ReadWritePaths` 注释。 |
| N8 崩溃当成功 | ✅ 真修（但见 R1） | `backend_common.rs:149,163,188` `reached_terminal` 标志 + warn；`dispatch.rs:1173` 空 session_id 不入库。 |
| S-5 stderr cap | ✅ 真修 | `backend_common.rs:240` `read_stderr_to_string` 用 `read_line_capped` + 单行/总量双上限。 |
| S-3 MCP 超时对齐 | ✅ 真修 | `mcp.rs:159` `ask_via_socket(.., ask_timeout)`；`main.rs:153,375` 经 argv 全链路传 `permission_ask_timeout_secs`。 |
| S-6 MCP 配置原子写 | ✅ 真修 | `claude/backend.rs:131` `create_new(true)` + temp+rename，不跟随 symlink。 |
| Debug redact | ✅ 真修 | `ilink/client.rs:35`、`ilink/login.rs:35`、`wecom/proto.rs:27` 手写 redacting Debug（`<redacted>`）。 |

**实跑验证**（`/tmp/imagent_verify.log`）：`cargo fmt --all --check` exit 0；`cargo clippy --workspace --all-targets --all-features -- -D warnings` **0 warning**；`cargo test --workspace` **241 passed / 0 failed / 2 ignored**；`cd fuzz && cargo +nightly check` exit 0；`cargo audit` 仅 RUSTSEC-2024-0437（经 prometheus→protobuf，已被 CI `--ignore` 豁免，imagent 仅 exposition 不解析不可信 protobuf，风险低）。

**额外核实**：SECURITY.md「旧库明文凭据读取时懒迁移到 keyring」声明真实存在（`store.rs:249-276 resolve_credential_blob`，非 `credentials.rs`——起初质疑被推翻）。

---

## 🔴 第一波 — 阻塞开源（开源前必修）

### O1  无 git remote + 无 git tag（= v5-F3，仍未解决）⬜
- **位置**：`git remote -v` 空；`git tag -l` → `tag-count=0`。
- **事实**：所有 `github.com/uzziah/imagent` 链接悬空——README 的 CI badge **404**；`release.yml`（`on: push: tags: ["v*"]`）**从未跑过**（三平台二进制 + sha256 全为零）；`book.toml` edit-url 404；`Cargo.toml repository` 字段无效。
- **影响**：「开源就绪」的最小定义（能 clone / 能下载 release）当前不成立。**这是唯一硬阻塞项。**
- **修复**（需仓库 owner，非代码改动）：① 在 GitHub 建 `uzziah/imagent`；② `git remote add origin <url>`；③ `git push -u origin main`；④ `git tag v1.0.0 && git push --tags` 触发 `release.yml`。
- **配套代码就绪检查**：push 前确认 `release.yml` 的 target 三元组、`CHANGELOG [1.0.0]` 叙事、README badge URL 与实际仓库路径一致。

---

## 🟠 第二波 — 上线前必修

### R1  `RunOutcome` 缺状态字段——N8 崩溃语义无法传到 IM（新发现）⬜
- **位置**：`crates/core/src/backend_common.rs:185-205`（warn）→ `crates/core/src/types.rs`（`RunOutcome` 定义）→ `crates/core/src/dispatch.rs:1142-1160`（回传）。
- **本质**：N8 在 backend 层做了正确的 warn（`!reached_terminal && exit≠0`），但 `RunOutcome { session_id, final_text }` **没有**携带「是否正常终止」的字段。dispatch 收到的是 `Ok(RunOutcome)`，于是把「崩溃后的部分文本」按**正常 final** 回 IM，用户完全无感知 agent 已 OOM/segfault——那行 warn 只进了日志，对终端用户不可见。
- **失败场景**：codex/gemini 跑到一半 OOM-kill，stdout 已吐若干中间 Text，从未发终止事件；imagent 把最后一条中间文本当最终答案回 IM，标 done。用户以为任务成功，实则 agent 半途崩溃。
- **修复**：`RunOutcome` 加 `terminal: bool`（终止事件产出 = true）；`spawn_cli_backend` 据已有的 `reached_terminal` 填充；`AcpBackend` 填 true（ACP 的 `session/prompt` 正常完成即终止）；dispatch 在 `!outcome.terminal` 时于回复前置 `⚠️ agent 异常退出，以下为部分输出：\n\n`。这是 N8 修复的自然收尾——把已有的 warn 升级为用户可见。补单测（mock backend 发 Text 后退出码非 0 → reply 含告警前缀）。

### R2  metrics / health endpoint 无鉴权 + 无绑定校验（新发现）⬜
- **位置**：`src/main.rs:216-229`（`spawn_metrics_server`）。
- **本质**：`/metrics`（Prometheus：messages_in/out、backend_calls/errors/duration、credential 指标）与 `/health`（session 数、登录状态、uptime）直接 `axum::serve`，无 auth、无绑定校验。若用户配 `metrics_addr = "0.0.0.0:9100"`，**公网可拉取**。当前缓解：默认 `None`（关闭）、文档示例用 `127.0.0.1`。但代码不强制——开源分发「默认安全」应补一道。
- **失败场景**：用户在 VPS 部署，为方便外部 Prometheus 抓取配 `0.0.0.0:9100`，未意识到 `/metrics` 暴露了活跃度/会话规模等运营情报（虽无凭据/PII，但属信息泄漏）。
- **修复**：① 解析到非 loopback 地址时 `warn!`（"metrics 监听非 loopback 地址，/metrics 无鉴权，公网可访问"）；② docs（deploy/README + config EXAMPLE）明确「metrics 不含鉴权，务必绑 127.0.0.1 或置于反代理后」。不强行拒绝非 loopback（合法的容器内抓取场景）。

### R3  `wecom_secret` 明文存 config.toml（= v5-S-4，未修）🟡
- **位置**：`crates/core/src/config.rs:99-103`（`wecom_secret: Option<String>`，明文）；`SECURITY.md`「已知限制」。
- **本质**：`wecom_secret` 明文存配置文件，与 iLink `bot_token` 走 OS keyring 不一致。代码注释承认，但 **SECURITY.md「已知限制」只提 bot_token，未提 wecom_secret**——安全敏感读者会误以为所有凭据都受 keyring 保护。
- **修复**（分两步）：① **文档（本轮我改）**：SECURITY.md「已知限制」明确补 wecom_secret 明文 + 务必 config.toml 0600；config EXAMPLE 注释强化。② **代码（后续）**：把 wecom_secret 也纳入 keyring 路径（需 bootstrap 命令，工程量较大，单开迭代）。

---

## 🟡 第三波 — 打磨项（v5 第三波确认现状 + 落地）

> 这些 v5 已列为「不阻塞首发」的打磨项，本轮确认其当前代码状态（均未修），并给出修复方向。

### P1  ilink 游标 at-most-once（crash 丢消息）⬜
- **位置**：`crates/ilink/src/platform.rs:107`（`set_sync_buf` 游标前进落盘）→ `:117`（`process_msg`，含同步媒体下载）。
- **本质**：游标先前进落盘，再处理消息（含媒体下载）。进程在游标写库后、消息交付前 crash → **整批永久丢失**。IM 场景宜 at-least-once（重复由 dedup 吸收）。
- **修复**：颠倒顺序（先 process_msg 完成再 set_sync_buf），或 pending-cursor（处理完才提交游标）。注意：媒体下载同步阻塞在循环内，颠倒后单批耗时会推迟游标推进——需评估对长轮询节奏的影响。

### P2  `WsFrame` subscribe body 未 redact（Debug footgun）⬜
- **位置**：`crates/wecom/src/proto.rs:33`（`#[derive(Debug)]` on `WsFrame`，subscribe body 含 `bot_id`/`secret`）。
- **本质**：bot_token/secret 的 Credentials/ILinkClient 已 redact（v5 落地），但 `WsFrame` 还 `derive(Debug)`——开源后任何贡献者加一行 `debug!(?frame)` 就把 subscribe 的鉴权字段泄进日志。
- **修复**：`WsFrame` 手写 redacting Debug（subscribe body 的 `secret` → `<redacted>`），或字段用 `secrecy::Secret`。

### P3  wecom `ws_url` 整串进 INFO 日志 ⬜
- **位置**：`crates/wecom/src/client.rs:99`（`info!(url = %self.ws_url, "ws 连接中")`）。
- **本质**：ws_url 整串进 INFO 日志。**当前实际风险低**（main 写死 `wss://openws.work.weixin.qq.com`，不含凭据），但模式是 footgun——若未来 ws_url 可配置且含 token，会直接泄漏。
- **修复**：只记 `host`（`parsed.host_str()`），不记整串 URL。

### P4  ilink `post_json` 响应体无大小上限 ⬜
- **位置**：`crates/ilink/src/client.rs:81`（`post_json<T: DeserializeOwned>`）。
- **本质**：响应体直接 `json()` 反序列化，无 `Content-Length` / 流式上限。恶意/异常服务端返回超大响应可致内存膨胀（与 `backend_common` 已修的 stdout/stderr cap 不对称）。
- **修复**：用 `resp.bytes()` + `MAX_RESPONSE_BYTES`（如 8 MiB）截断，超限返 Err；或 `resp.json()` 前检查 `Content-Length`。

### P5  WeCom 出站 markdown 未转义 ⬜
- **位置**：`crates/wecom/src/proto.rs:134`（`build_send_markdown_frame`，content 直接进 markdown）。
- **本质**：agent 输出含 markdown 特殊字符（`#`/`*`/`[link]`/反引号）会被企业微信 markdown 渲染，破坏排版甚至注入格式。注释称「渲染纯文本正常」，但 agent 自由文本不可控。
- **修复**：对 content 做 markdown 特殊字符转义，或评估 WeCom 是否有 plain text msgtype。

### P6  mcp `stdin.lock().lines()` 在 async fn 同步阻塞 ⬜
- **位置**：`crates/core/src/mcp.rs:213`（`run_mcp_server` 是 `async fn`，内部 `for line in stdin.lock().lines()`）。
- **本质**：`std::io::stdin().lock().lines()` 是同步阻塞迭代，在 async fn 里阻塞 worker 线程。**影响有限**（MCP server 是 claude spawn 的独立子进程，阻塞的是自己的进程，不在主 tokio runtime 关键路径），但是反模式。
- **修复**：改 `tokio::io::stdin()` + `BufReader::lines()`（async），或 `tokio::io::AsyncBufReadExt`。优先级低。

### P7  `IMAGENT_ACP_COMMAND` env 可换 spawn 命令无白名单 ⬜
- **位置**：`crates/claude/src/acp.rs:91-93`（`agent_command()` 读 `IMAGENT_ACP_COMMAND`）。
- **本质**：env 可替换 spawn 命令（任意可执行）。**威胁有限**（需能设运行环境），但部署者误配/恶意 env 可 spawn 任意命令。
- **修复**：basename 白名单（仅允许 `claude-agent-acp` / `npx`），或至少 warn 非 PATH 默认值。优先级低。

---

## 🔵 文档 / 工程化（新发现，主会话直接修）

### D1  README 写死 "241 passed"（会漂移）⬜
- `README.md:152` `cargo test --workspace # 241 passed`。测试数会增减，数字漂移（v3 已提过同类文档漂移）。改为「全通过」或删数字。

### D2  workspace `Cargo.toml` 顶部注释过时（P1 残留）⬜
- `Cargo.toml:1-10` 仍写「P1 实现阶段」「members 暂留空，避免 cargo 解析不存在的 crate 报错」，但 `members` 早已填满 7 crate。**仓库首文件即误导**，清理为当前事实。

### D3  `docs/P2_COMPLETE.md` 漏移 ⬜
- v5-F5 把 P1/P2/P3/PARALLEL ROADMAP 移 `internal/`，但 `docs/P2_COMPLETE.md` **仍在 `docs/` 根且进了 `SUMMARY.md` 侧栏**（面向用户）。内部完成报告不宜进用户侧栏，移 `internal/`。

### D4  macOS `imagent` 撞名警告仅在 deploy/README ⬜
- `imagent` = macOS 系统输入法进程（Input Method Agent）。`pkill imagent` **会杀系统输入法**。当前警告只在 `deploy/README.md`。这是**真实运维踩坑点**，应提升到主 README「快速开始」醒目处（首次安装/停止即踩）。

### D5  SECURITY.md 未标注 ACP `allowed_tools` 无效 / Off 全放行 ⬜
- ACP 后端 `allowed_tools` **完全无效**（`acp.rs:292-301`，无映射），且 `Off` 在 ACP = **全放行**（`allow_outcome`），与 CLI（Off 不挂 MCP）语义分裂。SECURITY.md「agent 权限收敛」一节未说明此差异——用户可能误以为三后端安全语义等价（main.rs:171 有 warn，但 SECURITY 文档没写）。补「已知限制」。

---

## 🏛️ 架构建议（第三波重构，非阻塞）

### A1  session 状态收敛 + 事务（v4-R4）
session 状态散在 `sessions` / `named_sessions` / `config(active_name / compact_summary)` 三表，无事务。一次 `/switch` 跨多表写，中途失败留不一致。→ per-conv `ConvState` + 单 mutator + 单事务。

### A2  后端安全语义统一（v5 架构评价#2）
CLI（`--allowedTools` 收敛）/ ACP（无等价机制，靠 cwd + permission_mode）/ `Off`（CLI 不挂 MCP vs ACP 全放行）语义差异大。→ trait 层定义统一「工具策略」；SECURITY.md 显式标注（见 D5）。

### A3  `ReplyHint::ILink` 泄漏 core 类型
`ReplyHint::ILink { context_token, ... }` 把平台细节提到 core 类型层。加第 3 平台前应收敛（关联类型 / 泛型 / 不透明 handle）。

---

## 代码改动约定

本轮 issue 修复均：① 基于已 review 的方案；② `.rs` 生产代码委派 omp 实现 + 主会话 Review + `cargo test --workspace` 验证；`.md` 文档 / 配置主会话直接改；③ commit 按功能域拆分（`docs(crate)` / `fix(core)` / `fix(ilink)` / `fix(wecom)` / `fix(claude)` / `fix(ops)`），message 注对应 issue id + 待 review；④ O1 由仓库 owner 操作。详见 [`CONTRIBUTING.md`](../CONTRIBUTING.md)。
