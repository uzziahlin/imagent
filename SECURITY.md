# Security Policy

## 报告漏洞

如果发现安全漏洞，请**不要**开公开 GitHub Issue。使用 GitHub 的 [private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)（仓库 Security 标签 → Report a vulnerability）。

- 收到报告后 **48 小时内**确认。
- 合理期限内（通常 ≤ 90 天）修复 + 发布 + 致谢。

## 范围

重点关注：
- **凭据泄露**：`bot_token` 等敏感信息。
- **鉴权绕过**：发送者白名单、权限审批闭环（D1）。
- **agent 权限收敛**：`workdir` 仅作 cwd（**非沙箱**，不限制可读路径，靠 `--allowedTools` + `permission_mode` 兜底）、`--allowedTools` 配置收敛、`--permission-prompt-tool` 绕过。
- **SSRF**：媒体 CDN 下载。

## 已有的加固（defense-in-depth）

- **IM 入口白名单鉴权**：iLink bot 任何人可加好友，非白名单 sender 丢弃（DESIGN §9.①）。
- **`--allowedTools` 配置收敛**：起步 `Read,Edit`；`workdir` 用 `current_dir` 锁定。
- **D1 IM 内权限审批闭环**：危险工具（如 Bash）须用户在 IM approve/deny（`PermissionMode::Ask`）。
- **store 文件 0600 / 目录 0700**（unix）。
- **SSRF 白名单**：媒体下载仅允许 `novac2c.cdn.weixin.qq.com` 等 CDN 主机。
- **限流熔断**：sendmessage 服从式退避（不绕风控）。

## 已知限制

- `bot_token` 优先经 **OS keyring 加密落盘**（store `credentials` 表只存 `keyring:<platform>:<account>` 指针 marker）；无 keychain 环境（headless/CI）或 keyring 写入失败时回退明文存 SQLite。旧库中的明文凭据会在读取时懒迁移到 keyring（见 `crates/store/src/credentials.rs`）。
- **`wecom_secret` 明文存 config.toml**（与 iLink `bot_token` 走 OS keyring 不一致）：务必把 config.toml 收紧到 `0600`。完整 keyring 保护（含 bootstrap 命令）见 `docs/CODE_REVIEW_v6.md` R3。
- **ACP 后端（`agent = "claude-acp"`）`allowed_tools` 不生效**：ACP 协议无 `--allowedTools` 等价机制，工具收敛只能靠 `permission_mode = ask/deny` 兜底；且 `Off` 在 ACP = **全放行**（与 CLI 的 `Off` = 不挂审批不同）。如需 `--allowedTools` 收敛 + 完整 IM 审批闭环，请用 `claude-cli` 后端。
- iLink 是腾讯对外协议的第三方 Rust 实现，使用者自负合规责任（见 README 免责声明 + RESEARCH §2）。
