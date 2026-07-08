# Changelog

记录 imagent 所有显著变更。格式参照 [Keep a Changelog](https://keepachangelog.com/)，版本遵循 [Semantic Versioning](https://semver.org/)。

## [Unreleased] — 安全审查 v2/v3/v4 修复

深度 Review v2/v3/v4（见 [`docs/CODE_REVIEW_v2.md`](docs/CODE_REVIEW_v2.md) / [`v3`](docs/CODE_REVIEW_v3.md) / [`v4`](docs/CODE_REVIEW_v4.md)）的修复。

### Fixed
- **P0（阻塞）**：ACP 权限 fail-open→fail-closed（P0-A）、权限 socket 对端 uid 鉴权 + chmod 0600（P0-B）、login baseurl 域名白名单（P0-C）。
- **P1（凭据 / 安全姿态）**：WAL/SHM chmod 0600 + 凭据写入审计（P1-A/B）、keyring fail-closed 选项 `require_keyring` + metric（P1-C）、workdir「cwd（非沙箱）」措辞（P1-D）。
- **P1（健壮性）**：ilink 媒体解密 fail-closed + 流式下载防 OOM + login 禁 redirect（P1-H/J/L）、WeCom msgid 去重（`Dedup` 提到 core，P1-I）、compact_summary 删除推迟到 run 成功后（P1-K）、权限 socket 回复 `agent_timeout` 超时（P1-G）、`/new`/`/switch`/`/compact` 取 conv 串行锁（P1-F）。
- **工程化**：各 crate `#![forbid/deny(unsafe_code)]`（E-2）、MSRV 统一继承 workspace（E-1，由 1.80 抬至 1.88：`clap 4.6.1` 等声明 `edition2024` 需 cargo 1.85+，且 `agent-client-protocol-schema`/`serde_with` 等核心依赖声明 `rust-version 1.88`）、项目根 CLAUDE.md onboarding 更新（D-1）。
- **v3 P1（9 条，第三轮 review 新发现，见 [`docs/CODE_REVIEW_v3.md`](docs/CODE_REVIEW_v3.md)）**：codex sandbox flag 错位（`-s` 移到 `--` 前）、CDN 下载强制 https scheme、send_text 失败不挂 pending、SIGTERM 优雅退出、in-flight task drain（JoinSet + shutdown Notify）、mcp read_line 超时、conv_locks 失败路径统一释放、PermissionRouter cancel API、permission socket read_line cap + write 超时。
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

### Changed
- workspace 测试 241 passed（2 ignored）；clippy 0 warning；fmt clean；macOS + ubuntu CI 矩阵。

## [1.0.0] — 2026-07-02 — P3 全部完成

首个稳定发布。P3（开源化 + 多平台 + 多后端 + 运维）全部交付。

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
