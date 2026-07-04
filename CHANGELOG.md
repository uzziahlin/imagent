# Changelog

记录 imagent 所有显著变更。格式参照 [Keep a Changelog](https://keepachangelog.com/)，版本遵循 [Semantic Versioning](https://semver.org/)。

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

## [Unreleased]

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
