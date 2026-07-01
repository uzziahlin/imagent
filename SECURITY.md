# Security Policy

## 报告漏洞

如果发现安全漏洞，请**不要**开公开 GitHub Issue。使用 GitHub 的 [private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)（仓库 Security 标签 → Report a vulnerability）。

- 收到报告后 **48 小时内**确认。
- 合理期限内（通常 ≤ 90 天）修复 + 发布 + 致谢。

## 范围

重点关注：
- **凭据泄露**：`bot_token` 等敏感信息。
- **鉴权绕过**：发送者白名单、权限审批闭环（D1）。
- **沙箱逃逸**：`workdir` 锁定、`--allowedTools` 收敛、`--permission-prompt-tool` 绕过。
- **SSRF**：媒体 CDN 下载。

## 已有的加固（defense-in-depth）

- **IM 入口白名单鉴权**：iLink bot 任何人可加好友，非白名单 sender 丢弃（DESIGN §9.①）。
- **`--allowedTools` 配置收敛**：起步 `Read,Edit`；`workdir` 用 `current_dir` 锁定。
- **D1 IM 内权限审批闭环**：危险工具（如 Bash）须用户在 IM approve/deny（`PermissionMode::Ask`）。
- **store 文件 0600 / 目录 0700**（unix）。
- **SSRF 白名单**：媒体下载仅允许 `novac2c.cdn.weixin.qq.com` 等 CDN 主机。
- **限流熔断**：sendmessage 服从式退避（不绕风控）。

## 已知限制

- `bot_token` 当前**明文存 SQLite**（DESIGN §9.4）——P3 计划用 OS keyring 加密落盘。
- iLink 是腾讯对外协议的第三方 Rust 实现，使用者自负合规责任（见 README 免责声明 + RESEARCH §2）。
