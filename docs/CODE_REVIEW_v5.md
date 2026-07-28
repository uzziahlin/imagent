# imagent 深度 Review v5 — 开源标准 · Issue 清单 + 修复跟踪

> **📖 历史代码审查记录**：本文为迭代过程留档，所有 issue 均已落地或文档化为已知限制，**不代表当前缺陷状态**；文中保留的内部术语（开发工具代号 / 会话编号 / 审查轮次标记）属迭代记录语境，不影响当前代码。用户文档见 [SUMMARY](./SUMMARY.md)。

> **评审对象**：`imagent` @ `7427a38`（main，已合并 `fix/code-review-v4`）。
> **评审标准**：开源首发（陌生开发者能否 clone → build → PR → 信任依赖 → 跑起来）。
> **评审范围**：`crates/{core,ilink,wecom,claude,codex,gemini,store}` + `src/main.rs` + 开源工程化层（CI / 供应链 / Cargo metadata / 文档 / deploy / fuzz）。
> **评审方法**：主会话逐行精读核心调度链路（auth / permission / dispatch / backend_common / main / config / store-credentials / types / error）+ 三个独立子审查并行精读（① ilink/wecom 平台协议安全 ② claude/codex/gemini 后端执行边界 ③ CI/供应链/文档等开源治理），对 v4「已修」项逐条读源码复核；实际运行 `cargo test --workspace` / `cargo tree -d` / `cargo audit` / `cd fuzz && cargo check` 验证声明真实性。
> **与 v4 的关系**：先逐条核实 v4 声称已修项是否真落地（v1 当年谎报、v2 补上、v3 部分修报、v4 自查无谎报——本轮独立复核 v4），再查 v4 未覆盖的新问题，最后补「开源首发就绪」视角（v1–v4 均为内部视角读代码，缺一次「陌生人照 README 跑」的实测）。
> **总体评分**：代码实现 **8/10**（架构干净、241 测试、`forbid(unsafe_code)`、fail-closed 一致、命令注入防线到位）；**安全 7.5/10**（硬约束扎实、威胁建模诚实，但 3 处 v4「声称已修实际半修」）；**开源就绪 6/10**（差距不在代码，而在「从未以外部视角跑通发布链」）。

## 🎯 核心洞察（第一性原理）

代码与安全设计是认真的、扎实的。但整个项目**从未以「外部用户/贡献者」视角完成过一次端到端的 clone → build → PR → release 流程**。v1–v4 全是「内部视角读代码」，缺一次「陌生人照 README 跑」的实测。证据链：

- fuzz target **编译不过**（配了 workflow 没验 target）；
- cargo-audit 有未修漏洞（CI audit job PR 一推就红）；
- 无 git remote / 无 tag（CI badge 404，release 从未跑）；
- 版本叙事三处打架；
- 3 处 v4「声称已修、实际半修」（§工程诚信复核）。

修完 §第一波 5 条 → 约 8/10 可真正开源。

## 📋 修复进度（分支 `fix/code-review-v5`）

**✅ 已落地（F1/F2/F4/F5/F7/F8/N8/S-5/S-3/S-6，2026-07-08）**：

- **第一波（阻塞开源）**：
  - **F1 fuzz 编译**：`ilink/lib.rs` `mod proto/media` → `pub mod`；proto target 改调真实 `UpdatesResp` 反序列化 + `extract_text`；fuzz Cargo.toml 加 serde_json。`cd fuzz && cargo +nightly check` 通过。
  - **F2 cargo-audit**：删死文件 `cargo-audit.toml`（cargo-audit 不读项目级 config）；`ci.yml` audit step 改 `cargo audit --ignore RUSTSEC-2024-0437`（protobuf 经 prometheus 引入，imagent 仅 exposition 不解析不可信 protobuf，风险低）+ justification 注释。
  - **F3 无 remote/tag**：⏳ 需仓库 owner 操作（建 GitHub 仓库 + `git remote add` + 打 tag 触发 release.yml），非代码改动。
  - **F4 ci.yml deny + 版本叙事**：deny job 删 `if: push to main`（license/source/ban 现阻塞 PR，与 audit 一致）；版本叙事对齐见 CHANGELOG（[1.0.0] 标待发布 + 徽章 pre-release）。
  - **F5 CODEOWNERS + 文档治理**：CODEOWNERS `@imagent/maintainers` → `@uzziah`；`docs/CODE_REVIEW v1/v2/v3` + `P1/P2/P3/PARALLEL ROADMAP` 移 `docs/internal/`（不进 SUMMARY）；根 CLAUDE.md 删 omp 故障段 + 进度更新到 P3；v4/v5/README/CLAUDE/源码注释的 omp 反噬措辞中性化 + 移走文档引用修正。
  - **F7/F8 deploy**：`deploy/README.md` 日志路径 `/tmp` → `/usr/local/var/log`（对齐 plist）+ metrics_addr 默认值纠正（默认关闭）；systemd `ReadWritePaths` 注释强调必须加 `default_workdir`（否则 ProtectSystem=strict 静默拒绝写）。
- **第二波（上线前）**：
  - **N8 崩溃当成功**：`backend_common.rs` Final 事件设 `reached_terminal`；返回前若 final_text 非空但未由终止事件产出 + exit 非 0 → warn 标注（不静默当成功），仍返回部分文本；`dispatch.rs` 落库前判空 session_id（崩溃未及分配时不入库，防 `--resume ""` 失败）。
  - **S-5 stderr 单行 cap**：`read_stderr_to_string` 改用 `read_line_capped`（按字节读行）+ `MAX_STDERR_LINE_BYTES=1MiB`，对称 stdout；防 prompt injection 写无 `\n` 超长流 OOM。
  - **S-3 mcp 超时对齐**：MCP server socket 读超时从硬编码 1200s 改为经 `--ask-timeout` argv 传入（= `config.permission_ask_timeout_secs`），跨 mcp.rs / claude/backend.rs / main.rs 传递，与 dispatcher 审批预算对齐。
  - **S-6 MCP 配置原子写**：`write_mcp_config` 的 check-then-write TOCTOU 改 temp+rename（`create_new` 原子创建 + rename 不跟随 symlink）。

**验证**：`cargo test --workspace` 241 passed / 2 ignored（单线程稳定；多线程偶发 `database is locked` 是既有 SQLite 并发 flaky，非本次引入）；`cargo clippy --workspace --all-targets --all-features -- -D warnings` 0 warning；`cargo fmt --all --check` clean；`cd fuzz && cargo +nightly check` 通过；`cargo audit --ignore RUSTSEC-2024-0437` 0 vuln。

**🟡 打磨（部分落地）**：**Debug 派生凭据 redacting** 已落地——`ILinkClient` / ilink `Credentials` / wecom `Credentials` 手写 redacting Debug（`bot_token` / `secret` 打 `<redacted>`），防 `{:?}` 落日志。其余（WsFrame subscribe body redact、游标 at-least-once、ws_url 日志 host、日志注入净化）见 §第三波，不阻塞首发。

---

## ✅ v4 核实结论（工程诚信）

**已修项真修、无谎报**（本轮逐行复核）：S-2 `env_clear` + 最小 env 白名单、S-1 ACP 启动 warn、R-1/R-2/R-3 退出 drain、N18 metrics 命名、P2-10 `delete_credential`、env/stdout cap（stdout 侧）、命令注入防线（argv / codex `-s` 在 `--` 前 / gemini `--prompt=` / ACP JSON-RPC）、SSRF（`url::Url` 精确匹配 + 强制 https + 禁重定向）、TLS 全程校验（零 `danger_accept_invalid`）、AES fail-closed、限流服从式退避、keyring `require_keyring` fail-closed、X-WECHAT-UIN 真 CSPRNG。

**但发现 3 处 v4「声称已修、实际半修」**（与 v1/v2/v3 当年「部分修报」同模式，需对 v4 已修项也逐 file:line 核实而非依赖声明）：

| v4 声明 | 实际 | 证据 |
|---|---|---|
| **S-5 stdout/stderr cap（已修）** | **stderr 侧半修** | `backend_common.rs:219-238` `read_stderr_to_string` 用 `next_line()`，单行无上限；`MAX_STDERR_BYTES=64KiB` 在 `next_line` 返回**之后**才判定，单行内存已分配完毕。stdout 修了，stderr 是对称漏修。 |
| **S-3 权限超时独立预算（已修，含「mcp 超时与 dispatcher 对齐」）** | **半修** | `mcp.rs:180` `MCP_ASK_TIMEOUT=1200s` 硬编码魔数不读配置；用户把 `permission_ask_timeout_secs` 调到 >1200 时 MCP 先超时返 deny，闭环静默失效。dispatch 侧真修了。 |
| **S-6 MCP 配置原子写（已修，symlink 防护 + 清理）** | **半修** | `claude/backend.rs:104-115` symlink 检查是 TOCTOU（check-then-write），未用 `create_new(true)`（`O_CREAT\|O_EXCL`），只防静态已存在 symlink，竞窗仍在。清理部分确已落地。 |

---

## 🔴 第一波 — 阻塞开源（开源前必修）

### F1  fuzz target 根本编译不过（宣称的 fuzz 覆盖是假的）⬜
- **位置**：`fuzz/fuzz_targets/ilink_proto_parse.rs:7`、`ilink_media_cdn_host.rs:7`、`crates/ilink/src/lib.rs:20-22`。
- **事实**：实测 `cd fuzz && cargo check` 报 3 个 error：① `E0603: module 'proto' is private`（`lib.rs` 是 `mod proto;` 非 `pub mod proto;`）；② `E0603: module 'media' is private`；③ `E0425: cannot find function 'parse_frame'`——该函数不在 ilink，在 `crates/wecom/src/proto.rs:192`。
- **影响**：`fuzz.yml` 每周日 cron 一跑就红；外部贡献者 clone 后 `cargo +nightly fuzz run` 直接失败。README/CI 宣称有 fuzz，实际零覆盖。开源首发即失信。
- **修复**：`crates/ilink/src/lib.rs` 把 `mod proto;`/`mod media;` 改 `pub mod proto;`/`pub mod media;`（或为 fuzz 暴露专用入口）；fuzz target 改调真实存在的函数（新增 `parse_frame`，或改调 `extract_text`/`msg_to_inbound`）。本地 `cd fuzz && cargo check` 必须过。
- **教训**：「存在代码 ≠ 代码正确」——配了 workflow 必须实测 target 能编译，不能只看文件非空。

### F2  cargo-audit 漏洞未修，CI audit job 一推就红 ⬜
- **位置**：`Cargo.lock`（`protobuf 2.28.0` 经 `prometheus 0.13.4` 传递引入）；`ci.yml` audit job（无 `if`、无 `continue-on-error`、PR 阻塞）。
- **事实**：`cargo audit` 报 **RUSTSEC-2024-0437**（protobuf uncontrolled recursion crash）。`deny.toml [advisories] ignore = []` 不管用（CI 跑 `cargo audit`，不是 `cargo deny check advisories`）。
- **影响**：首个 PR / 首个 push to main 直接 fail。实际风险低（imagent 只用 prometheus 做 exposition，不解析不可信 protobuf），但 CI 不加 ignore 就是硬红。
- **修复**：写 `.cargo-audit.toml` 带 justification ignore（低风险），或把 prometheus 换成 `metrics` crate。本地 `cargo audit` 必须 0 vuln。

### F3  无 git remote + 无 git tag（发布渠道不存在）⬜
- **位置**：`git remote -v` 空；`git tag -l` 空。
- **事实**：所有 `github.com/uzziah/imagent` 链接（CI badge、book edit、release.yml `on: tags: ["v*"]`）都是悬空引用。README 的 CI badge 当前是 404；release.yml 从未跑过，三平台二进制 + sha256 全为零。
- **影响**：「开源就绪」的最小定义（能 clone / 能下载 release）当前不成立。
- **修复**：① 建 GitHub 仓库 `uzziah/imagent`；② `git remote add origin …`；③ push 后打 tag 触发 release.yml。需仓库 owner 操作（非代码改动）。

### F4  ci.yml cargo-deny 不阻塞 PR + 版本叙事三处打架 ⬜
- **位置**：`ci.yml:60`（deny job `if: github.event_name == 'push' && github.ref == 'refs/heads/main'`）；`Cargo.toml version=1.0.0` + `CHANGELOG [1.0.0] — 2026-07-02 首个稳定发布` + README 徽章 `status-pre-release` + 无 `v1.0.0` tag。
- **事实**：v4 把 audit 搬回 PR 阻塞了，但 deny job 仍带 `if`——引入 GPL/git 源的 PR 能过 CI 合进 main。版本叙事三者矛盾：读者无法判断 1.0.0 到底发没发。
- **修复**：删 deny job 的 `if:`（与 audit 一致）；版本叙事对齐——打 tag 转 stable，或 Cargo.toml 降到 `0.1.0`/`1.0.0-rc.1`（取决于是否已建 remote）。

### F5  内部 review 文档全量公开 + CODEOWNERS 占位 ⬜
- **位置**：`docs/`（4 版 CODE_REVIEW + P1/P2/P3/PARALLEL ROADMAP 经 `pages.yml` 渲染到 GitHub Pages）；`.github/CODEOWNERS`（`* @imagent/maintainers`，team 不存在 → review 自动指派失效）；根 `CLAUDE.md:20`（omp 故障段也会公开）。
- **事实**：内部文档大量出现「谎报完成」「累计 8 次异常」「违反硬规则请 review」等措辞——健康的内部自省，但对陌生开发者信号是「维护流程混乱、历史谎报」。`P1_DESIGN`/`P2_ROADMAP`/`PARALLEL_ROADMAP` 还是不在 SUMMARY 侧栏的 orphan 页。
- **修复**：v1/v2/v3 review + P1/P2/PARALLEL ROADMAP 移到 `docs/internal/`（不进 SUMMARY）；根 CLAUDE.md 删 omp 故障段（内部 working agreement）；CODEOWNERS 换真实用户名 `@uzziah` 或删。只留 DESIGN/RESEARCH/SECURITY/v4/v5 面向用户。

### 第一波补充项 ⬜
- **deploy/README.md:25** 日志路径仍是 `/tmp/imagent.log`（plist 已改 `/usr/local/var/log`）→ 对齐。
- **systemd `ReadWritePaths=%h/.imagent`** 对 agent workdir 是地雷：`default_workdir` 不在 ReadWritePaths 里，Claude 写 workdir 被 `ProtectSystem=strict` 静默拒绝（首装必踩）→ 注释强调必须把 workdir 加进来。
- **README 无 coverage badge**；各 crate Cargo.toml 只继承 rust-version/license，缺 `description`/`repository`/`keywords`/`categories`（publish=false 不卡 crates.io，但门面不全）。

---

## 🟠 第二波 — 上线前必修

### N8  final_text 非空时忽略非零退出码（agent 崩溃被当成功）⬜
- **位置**：`crates/core/src/backend_common.rs:172-188`。
- **本质**：判定顺序是 `error_text? → final_text 空? → 否则 Ok`。只要循环中收到过一次 `CliEvent::Text`（中间文本，非终止事件），子进程随后自行崩溃（stdout EOF、非零退出）也落到最后一分支返回 `Ok(RunOutcome)`，`status` 完全没参与判定。`diagnose()` 只在 `final_text` 空时才读 `status`。
- **失败场景**：codex/gemini 跑到一半 OOM-kill 或 segfault，stdout 已吐过若干中间文本，但从未发 `TurnCompleted`/`Result`。imagent 把最后一条中间文本当最终答案回 IM，标成功，dispatch 不重试不告警。注意这命中「子进程自己挂」，不走 agent_timeout（future drop）路径。
- **修复**：`final_text` 非空但 `!reached_terminal` 且 `status != success` 时，warn 标注 + 仍返回 final_text（IM 场景拿到部分结果比报错有用），或返回 Err。补单测。v4 归 🟡，本轮提 🟠。

### S-5  stderr 侧单行未真正 cap（可被 prompt injection OOM）⬜
- **位置**：`crates/core/src/backend_common.rs:219-238`（`read_stderr_to_string`）。
- **本质**：stdout 用了自写 `read_line_capped`（按字节 fill/consume，超限返 Err），但 stderr 用 `AsyncBufReadExt::next_line()`——读到 `\n` 或 EOF，单行无上限地全量分配。`MAX_STDERR_BYTES=64KiB` 在 `next_line` 返回**之后**才执行，单行内存已分配完毕。
- **失败场景**：被 prompt injection 操纵的 agent（或恶意 workdir 里 hook）向 stderr 写一条无 `\n` 的 10 GiB 流。管道缓冲满后子进程阻塞，`next_line` 持续读入累积到 10 GiB，远在 64 KiB cap 触发前 OOM。stdout 同维度修了，stderr 对称漏修。
- **修复**：stderr 也走 `read_line_capped` 或给 `next_line` 套字节预算。补单测。

### S-3  MCP 子进程超时硬编码不读配置（v4 半修的补齐）⬜
- **位置**：`crates/core/src/mcp.rs:180`；对照 `config.rs:83`（`permission_ask_timeout_secs` 默认 300）+ `dispatch.rs:521`。
- **本质**：dispatch 侧已独立预算（S-3 dispatch 部分真修），但 MCP server 子进程 socket read_line 超时硬编码 `MCP_ASK_TIMEOUT=1200s`，不读配置。若用户把 `permission_ask_timeout_secs` 调到 >1200，dispatch 还在等，MCP 先超时返 Err→deny，闭环静默失效。
- **修复**：MCP 超时读配置（经 `--mode` 同款 argv 传 `--ask-timeout`，或与 dispatcher 对齐）。

### S-6  MCP 配置 symlink 防护是 TOCTOU（v4 半修的补齐）⬜
- **位置**：`crates/claude/src/backend.rs:104-115`。
- **本质**：先 `symlink_metadata` 检查 is_symlink，再 `tokio::fs::write` 覆写。check 与 write 之间存在竞窗；且 `write` 是 `O_TRUNC|O_CREAT` 而非 `create_new(true)`（`O_CREAT|O_EXCL`）。v4 给的修复方向明确是 `create_new` 或 temp+rename，代码都没采用。
- **修复**：`OpenOptions::new().write(true).create_new(true)` 原子拒绝已存在；或 temp+rename。

---

## 🏛️ 架构评价

**优点**（无需改动）：双 trait 抽象 + 依赖倒置干净；Backend 无状态、session 生命周期在 core；`spawn_cli_backend` 消除三后端重复；conv 级串行锁；`SessionExpired` 类型化判定；生产代码仅约 11 处 `unwrap/expect/panic`（多在 LazyLock metric 注册）；fail-closed 倾向一致；`#![forbid(unsafe_code)]`；命令注入防线完整（argv 传递、codex `-s` 在 `--` 前、gemini `--prompt=` 防 flag 误解析、ACP JSON-RPC）；**威胁建模诚实**（`dispatch.rs:332` 主动标注 P2-7 残余风险）。

**结构性建议**（第三波重构，非阻塞）：
1. **状态机收敛 + 事务**：session 状态散在 `sessions`/`named_sessions`/`config(active_name/compact_summary)` 三表无事务（v4 R-4）→ per-conv `ConvState` + 单 mutator + 单事务。
2. **后端安全语义统一**：CLI 用 `--allowedTools` 收敛、ACP 无等价机制、`Off` 在 CLI（不挂 MCP）vs ACP（全放行）语义差异大 → trait 层定义统一「工具策略」。SECURITY.md 应显式标注「ACP 后端不强制 allowed_tools，仅依赖 cwd + permission_mode」，避免用户误以为三后端安全语义等价。
3. **`ReplyHint::ILink` 泄漏到 core 类型** → 关联类型/泛型（加第 3 平台前收敛）。

---

## 🟡 第三波 — 打磨项

- **Debug 派生泄漏凭据 footgun**：`ilink/client.rs:19`（`ILinkClient`）、`ilink/login.rs:23` + `wecom/proto.rs:16`（`Credentials`）、`wecom/proto.rs:33`（`WsFrame` subscribe body）derive `Debug` 含 `bot_token`/`secret`。当前调用点未泄漏，但是定时炸弹——开源后任何贡献者加一行 `debug!(?frame)` 就泄密。修：手写 redacting Debug 或 `secrecy::Secret`。
- **游标 at-most-once 丢消息**：`ilink/platform.rs:103-119` 先 `set_sync_buf`（游标前进落盘）再 `process_msg`（含同步媒体下载）。进程在游标写库后、消息交付前 crash → 整批永久丢失。IM 场景宜 at-least-once（重复由 dedup 吸收）。修：颠倒顺序或 pending-cursor。
- **N9** CLI 后端 `chunks.send().await` 无超时（`backend_common.rs:125,139,142`），慢消费者拖死读循环，与 ACP 的 `try_send` 不一致。
- **N12** `Off` 跨后端语义不一致（CLI 不挂 MCP vs ACP 全放行）→ 文档明确。
- **N10-acp** `IMAGENT_ACP_COMMAND` env 可替换 spawn 命令（`acp.rs:91`），无 basename 白名单。
- **N6** MCP server 在 async fn 里同步 `stdin.lock().lines()` 阻塞 worker（`mcp.rs:203`）。
- **P3-N2** WeCom 出站 markdown 未转义（`wecom/proto.rs:124`）。
- **P3-N4** ilink `post_json` 响应体无大小上限（`client.rs:98`）。
- **日志卫生**：`wecom/client.rs:99` ws_url 整串进 INFO 日志（误把凭据写 URL 会泄漏）；不可信字段（`from_user_id`）直入 tracing 结构化字段（日志注入）。
- **R-6 根因更正**：v4 doc 称「丢帧前 dedup 已记 msgid」，实际 dedup 在 channel 下游——`try_send` 满时帧被丢、msgid 未进 dedup。结论（R-6 未解决）不变，但 v4 根因描述需更正为「背压 / 满则断连重连重发」。
- **供应链**：重复依赖 `getrandom` 0.2/0.4（rand 0.8 vs uuid 1.23）、`hashbrown` 0.14/0.17（rusqlite vs indexmap），均为传递依赖，`deny.toml multiple-versions="warn"` 配置正确；`Cargo.lock` 残留不可达 `thiserror 2.0.18`（`cargo update` 可清）；`deny.toml [advisories]` 段是死配置（CI deny job 不查 advisories）。
- **CI `@stable` 与 rust-toolchain `1.88` 交互易混**：lint/coverage/pages/release 全 `@stable` 但实际跑 1.88（toolchain pin），读 CI 时易误判。

---

## 代码改动约定

本轮 issue 修复均：① 基于已 review 的方案；② `cargo test --workspace` / `cd fuzz && cargo check` 验证；③ commit message 注对应 issue id + 待 review；文档/配置/CI 类直接改。详见 [`CONTRIBUTING.md`](../CONTRIBUTING.md)。
