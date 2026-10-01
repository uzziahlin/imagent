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

## 威胁模型与边界（必读）

**核心事实**：agent 子进程以**宿主用户同一 uid** 运行。IM 权限审批闭环约束的是
「agent（claude 等）发起的工具调用」，**不约束**「同 uid 进程对文件系统的读写能力」。
这两者是不同的信任边界——审批闭环管住了工具面，管不住进程面。

因此，在「agent 被提示注入操纵」（例如让它读了一个含恶意指令的网页/文件）的场景下，
即使 Bash 全部过审，被操纵的 agent 仍可用 claude 默认免审的 `Read`/`Glob`/`Grep`：

1. **读 `~/.imagent/` 状态目录**：`imagent.db`（明文回退形态下含 iLink `bot_token`；
   `allowed_senders`/`audit_log`/`session_history`——含全部会话 prompt；`queued_messages`）。
2. **读主进程环境**（Linux `/proc/<pid>/environ`，macOS `ps eww`）：取得
   `IMAGENT_FEISHU_APP_SECRET` / `IMAGENT_HTTP_TOKEN` / `IMAGENT_PASSPHRASE`——
   子进程 env 白名单透传只防「意外继承」，防不住「主动读取」。
3. **写 DB 做持久化后门**：向 `allowed_senders` 表 INSERT 攻击者 sender（下次
   SIGHUP/重启经 config ∪ store 并集生效）。
4. **权限 socket**：同 uid 通过 SO_PEERCRED，配合 token 文件可伪造 ask 注入
   （**不能**伪造 approve——闭环本身没破）。
5. Linux Secret Service 下 keyring 凭据对同会话任意进程可读。

**部署风险分级**：

| 形态 | 评估 |
|---|---|
| 单人私聊自用 | 可接受（攻击者 ≈ 你自己；真正边界是你的 IM 账号） |
| **多人群聊白名单** | **风险显著上调**：任一白名单成员触发的提示注入，即可能按上述路径拿到 bot 凭据并给自己开后门。群白名单的语义是「信任这些人**本人**」，不是「信任他们的 prompt 供应链」 |

**建议缓解**（按性价比）：

- 给 agent 配 `permission_mode = "ask"` + `approval_tools` 收紧审批面（已在做的）；
- **专用运行用户**跑 imagent（`User=imagent`），把 `~/.imagent` 与你的日常账户隔离；
- Linux systemd 加固：`ProtectHome=read-only`、`PrivateTmp=yes`、`NoNewPrivileges=yes`
  （`deploy/` 下的 unit 有雏形）；
- 设置 `IMAGENT_PASSPHRASE`（凭据回退形态加密，见 S3），并知悉其**不**防同 uid 读取
  （passphrase 在环境变量里，同 uid 可读）——它防的是**落盘文件泄漏**，不是本节场景。

## 已有的加固（defense-in-depth）

- **IM 入口白名单鉴权**：iLink bot 任何人可加好友，非白名单 sender 丢弃（DESIGN §9.①）。
- **`--allowedTools` 配置收敛**：起步 `Read,Edit`；`workdir` 用 `current_dir` 锁定。
- **D1 IM 内权限审批闭环**：危险工具（如 Bash）须用户在 IM approve/deny（`PermissionMode::Ask`）。
- **S3 凭据应用层加密**：设置环境变量 `IMAGENT_PASSPHRASE` 后，OS keyring 不可用（headless/CI 常见）或写入失败时，凭据以 **AES-256-GCM + PBKDF2-SHA256（100k 迭代）** 加密落 SQLite（`enc:v1:` 版本化格式，随机 salt + nonce）；读取兼容 keyring / 加密 / 明文三形态，存量明文在读取时惰性迁移为加密形态。未设 passphrase 的明文回退日志升级为 error（headless 场景不阻断的取舍）。实现见 `crates/store/src/crypto.rs`。
- **S7 metrics/health 端点鉴权**：设置环境变量 `IMAGENT_HTTP_TOKEN` 后，`/metrics` 与 `/health` 要求 Bearer token（不匹配返回 401）；**非 loopback 绑定且未配 token 时拒绝启动**（fail-closed）——暴露到网络的指标端点不会无鉴权裸奔。
- **权限能力协商 fail-closed（v1.9.0 行为变更）**：`Backend` trait 新增 `PermissionCapability`（FullLoop / NativeOnly / Unsupported）；闭环类权限档（`permission_mode = "ask"` / claude 的 auto 档）× 非 FullLoop 后端（codex / gemini / 旧配置形态）**启动即拒绝**，不再静默忽略权限模式（此前 ask 档在无审批能力后端被静默降级 = 事实上的 fail-open）。`/perm` 热切同口径校验。
- **状态目录 deny（T7，默认开）**：`hide_state_dir_from_agent`（缺省 `true`，仅 claude-cli 后端）每轮 spawn 注入 `--settings` 内联 deny 规则——`Read`/`Edit`/`Write`/`Glob`/`Grep` × `//<imagent_home>/**` 绝对路径 glob（profile 感知 + 基座 `~/.imagent`，覆盖 symlink 形态）——把 imagent 状态目录移出 agent 读视野，防提示注入后的 agent 用默认免审只读工具直读 `imagent.db`（凭据/白名单/全部会话 prompt）。deny 无条件压过任何来源的 allow，对默认免审工具同样生效。SIGHUP 热改（下一轮起）。**边界**：Bash/子进程间接读取仍不可挡（同 uid 进程级边界，见上文威胁模型；Read deny 对 Bash 中被识别的 `cat`/`head` 等点名文件的命令生效，对不点名文件的命令与任意子进程无效）；`Glob`/`Grep` 的路径形态规则当前 CLI 不 consult（`Read` 规则 best-effort 连带覆盖二者的 `path` 参数）；claude-acp 后端不注入（见已知限制）。需让 agent 读自家状态时显式设 `false`。
- **store 文件 0600 / 目录 0700**（unix）。
- **SSRF 白名单**：媒体下载仅允许 `novac2c.cdn.weixin.qq.com` 等 CDN 主机。
- **限流熔断**：sendmessage 服从式退避（不绕风控）。

## 已知限制

- `bot_token` 优先经 **OS keyring 加密落盘**（store `credentials` 表只存 `keyring:<platform>:<account>` 指针 marker）；无 keychain 环境（headless/CI）或 keyring 写入失败时回退落 SQLite——**设置了 `IMAGENT_PASSPHRASE` 则回退形态为 AES-256-GCM 加密**（见上方 S3），否则为明文（error 日志提示）。旧库中的明文凭据会在读取时懒迁移到 keyring 或加密形态（见 `crates/store/src/credentials.rs`）。
- **`wecom_secret` 明文存 config.toml**（与 iLink `bot_token` 走 OS keyring 不一致）：务必把 config.toml 收紧到 `0600`。完整 keyring 保护（含 bootstrap 命令）见 `docs/CODE_REVIEW_v6.md` R3。
- **ACP 后端（`agent = "claude-acp"`）`allowed_tools` 不生效**：ACP 协议无 `--allowedTools` 等价机制，工具收敛只能靠 `permission_mode = ask/deny` 兜底；且 `Off` 在 ACP = **全放行**（与 CLI 的 `Off` = 不挂审批不同）。如需 `--allowedTools` 收敛 + 完整 IM 审批闭环，请用 `claude-cli` 后端。
- **claude-acp 后端无状态目录 deny**：ACP 的 claude 命令行由 `IMAGENT_ACP_COMMAND` 外部指定，网关无法可靠追加 `--settings` deny 参数——`hide_state_dir_from_agent` 仅 claude-cli 生效。用 ACP 且在意该面时，可在 `IMAGENT_ACP_COMMAND` 指向的包装脚本里自行补 `--settings`，或依赖进程级隔离（专用用户/sandbox）。
- **ACP 后端子进程树无进程组收割**：ACP SDK（agent-client-protocol 1.0.1）在连接断开/超时 cancel 时只 kill 直接子进程（`claude-agent-acp`），其孙进程（agent 内部再 spawn 的 claude CLI / MCP server / Bash 工具）可能存活为孤儿并继续以该会话上下文运行。SDK 的 spawn 封闭在 `AcpAgent::connect_to` 内部（无 Command 注入点、无 pid 访问器），且 async-process 2.5 无 `process_group` API——网关无法对齐 claude-cli 路径的 killpg 组杀（详见 `crates/claude/src/acp.rs` 模块头）。上游暴露注入点前，缓解：给 `IMAGENT_ACP_COMMAND` 配 `exec` 单体化包装脚本（减少孙进程面）、以专用用户运行并以 systemd/cgroup 级进程树监管兜底。
- **云文档评论会话的信任边界**：conv `feishu:comment:<file_token>` 放行后，**所有能评论该文档的协作者**共享同一个 agent 会话（上下文 / 会话级 allow-set / 工作区互通），且回复锚定评论者本人（v13 后）。放行一个文档会话 = 放行该文档的**全体协作者**驱动 agent——协作者范围通常远大于 IM 白名单。文档权限请按此口径收敛（链接分享/协作者清单），或对高敏文档不开评论会话。
- iLink 是腾讯对外协议的第三方 Rust 实现，使用者自负合规责任（见 README 免责声明 + RESEARCH §2）。
