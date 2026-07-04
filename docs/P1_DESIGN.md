# P1-1 / P1-4 设计方案（大重构，待真机 + 评审后执行）

> 本文是 `CODE_REVIEW.md` 中 P1-1（抽 backend helper，完整统一）与 P1-4（ACP 真长驻）的详细设计。
> 这两项是设计性大重构，**mock 测试无法验证真实 CLI/进程行为**，必须真机回归 + 设计评审后才能落代码。
> P1-1 的安全部分（去重 `read_stderr_to_string` + `WRITE_OR_EXEC` 到 `crates/core/src/backend_common.rs`）已先行完成（零行为变化，已 215 passed）。

## P1-1 完整统一：抽 `spawn_cli_backend` 泛型 helper

### 现状
三 CLI backend（claude/codex/gemini）的 `run()` 脚手架 ~80 行 90% 相同：spawn cmd → kill_on_drop → 并发 stderr → 读 stdout 循环 → session 捕获 → 错误优先级 → Final → RunOutcome。差异仅在：
- cmd 构造（claude `-p --output-format stream-json --verbose` / codex `exec --json` / gemini `-p -o stream-json`）
- stdout 事件解析（claude `result`/`is_error`，codex `ThreadStarted`/`TurnCompleted`，gemini `Init`/`Result`）

### 设计
1. **`crates/core/src/backend_common.rs` 新增**（已有 `read_stderr_to_string` + `WRITE_OR_EXEC`）：
   ```rust
   /// 统一事件：三 backend 的 parse_line 都映射到它。
   pub enum CliEvent {
       Session(String),          // 捕获 session_id（claude result / codex thread_id / gemini init）
       Text(String),             // 中间文本（best-effort 推 IM）
       ToolUse { tool: String, input: String },
       ToolResult { tool: String, output: String },
       Final(String),            // 终止 + 最终文本
       Error(String),            // 终止 + 错误
       Terminal,                 // 终止信号（无文本，codex TurnCompleted / gemini Result）
       Skip,
   }

   /// 泛型 run 脚手架：三 backend 共用。
   pub async fn spawn_cli_backend(
       mut cmd: tokio::process::Command,   // 已构造好（含 cwd/args/stdin/stdout/stderr/kill_on_drop）
       parse: impl Fn(&str) -> CliEvent,   // 各 backend 的 parse_line
       chunks: mpsc::Sender<AgentChunk>,
   ) -> Result<RunOutcome> { /* spawn + 并发 stderr + 读循环（调 parse）+ session 捕获 + Final */ }
   ```
2. **三 backend `stream.rs`**：`parse_line` 返回 `CliEvent`（映射各自事件）。
3. **三 backend `run()`**：收缩到「构造 cmd + 调 `spawn_cli_backend(cmd, parse_line, chunks)`」。

### 风险（必须真机回归）
- 事件映射易漏：codex `TurnFailed`、gemini `Error`、claude `is_error` 等终止事件的优先级必须逐一对照原实现，漏一个就改变错误行为。
- session 捕获时机：claude `result.session_id` 优先 > 中间事件；codex `thread_id`；gemini `init.session_id`。统一后顺序需保持。
- **真机验证**：`claude -p` / `codex exec` / `gemini -p` 各跑一轮（正常 + 错误 + 工具调用 + session 续接），对照重构前后行为一致。

## P1-4 ACP 真长驻：跨 run 复用 AcpAgent + connection

### 现状
`crates/claude/src/acp.rs` 每次 `run` 都 `AcpAgent::from_str` + `Client.builder().connect_with(agent, ...)`，turn 结束随 connection 退出。性能收益（进程复用）零——等同 CLI 每次 spawn。

### 设计
1. `AcpBackend` 持长驻连接：
   ```rust
   pub struct AcpBackend {
       permission_mode: Arc<RwLock<PermissionMode>>,
       // 长驻：跨 run 复用 AcpAgent + connection。崩溃时 None（下次 run 重建）。
       agent: Arc<Mutex<Option<PersistentAcp>>>,
   }
   ```
2. `PersistentAcp` 封装 `AcpAgent` + 长期持有的 connection（具体形态取决于 `agent-client-protocol` SDK：`ConnectionTo<Client>` 能否跨 `connect_with` 闭包存活——**需先读 SDK 源码/示例确认**）。
3. `run()`：取 `agent.lock()`；若 None 或上次崩溃 → 重新 `from_str` + `connect_with` 建连接并缓存；复用连接发 `session/load`（续接）或 `session/new` + `session/prompt`。
4. session 缓存：`HashMap<conv_id, session_id>`，避免重复 `session/new`（进程复用后多次同 conv 用同 session）。
5. 崩溃恢复：`session/prompt` 返回连接错误 → 置 `agent = None` → 下次 `run` 重建。

### 风险（必须真机 + SDK 熟悉）
- **SDK 不确定性**：`agent-client-protocol` 的 `AcpAgent::connect_with` 是否消费 `agent`？`ConnectionTo` 能否跨 turn 存活？长驻是否需要自己 spawn `claude-agent-acp` 子进程（而非靠 SDK spawn）？—— 这些**必须读 SDK 源码确认**，盲改会 panic/泄漏子进程。
- 进程生命周期：长驻子进程崩溃检测、僵尸回收、信号处理。
- **真机验证**：装 `claude-agent-acp`，跨多轮 prompt 复用同一进程（`ps` 确认子进程不增），崩溃后能恢复。

## 执行前提（满足后才落代码）
1. **omp 工具链修复**：当前 omp 对重构类任务挂死（daemon 模式），这两项需 omp 或主会话仔细 Edit。
2. **真机环境**：`claude` / `codex` / `gemini` / `claude-agent-acp` 四个 CLI 装好 + 联网，能实跑。
3. **设计评审**：本文方案过一遍（尤其 P1-4 SDK 部分），确认无遗漏。
4. **回归对照**：重构前后真机跑同样 prompt，对照 session 续接 / 工具调用 / 错误处理行为一致。

满足后，P1-1 完整统一预计 ~200 行重构（core helper + 三 stream + 三 backend），P1-4 长驻 ~150 行（acp.rs 重写 + PersistentAcp）。
