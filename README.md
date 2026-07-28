# imagent

> **Instant messaging, meet your agent.**

一个用 Rust 写的、把即时通讯平台接入自主 agent 的网关。**任何 IM**（个人微信 iLink / 企业微信 WeCom）↔ **任何 agent**（Claude Code / …）。

![Rust](https://img.shields.io/badge/Rust-edition%202021-orange) ![License: MIT](https://img.shields.io/badge/License-MIT-blue) ![CI](https://github.com/uzziahlin/imagent/actions/workflows/ci.yml/badge.svg) ![Status](https://img.shields.io/badge/status-pre--release-yellow) ![Docs](https://img.shields.io/badge/docs-mdBook-blueviolet)

> 🌐 **English TL;DR** — `imagent` is a Rust gateway that bridges any instant-messaging platform (WeChat **iLink** / **WeCom**) with any autonomous agent (**Claude Code** / Codex / Gemini). It turns an IM private chat into an **approval-gated** agent cockpit: the agent runs real tasks (read/write files, run commands, edit code) but must ask for your `y/n` in IM before any dangerous tool. Pluggable on both sides (`Platform` / `Backend` traits), single binary, SQLite-backed sessions.
> **Unofficial — not affiliated with Tencent or Anthropic.** iLink is a third-party Rust re-implementation of Tencent's OpenClaw Weixin protocol; compliance and account risk are solely yours.
> The documentation below is in Chinese (the project targets the WeChat ecosystem).

---

## ⚠️ 免责声明

imagent 是**非官方**第三方开源项目，**不隶属于腾讯或 Anthropic**。

- iLink（智联 / ClawBot）接入定位为「[OpenClaw Weixin channel 协议](https://github.com/Tencent/openclaw-weixin)的 Rust 实现」——基于腾讯官方对外协议，**非** iPad 协议 / PC-hook 等逆向方案。使用者**自负合规责任**：使用可能违反微信/腾讯服务条款，账号风险（封号等）由使用者承担。
- 仅做**服从式退避**（被限流就退避等待），**绝不实现**绕过频率/风控的功能（ClawBot 条款 §4.6 红线）。
- 仅供学习研究。商用/生产使用前请咨询法律意见。建议绑定**小号**。

详见 [`docs/RESEARCH.md`](docs/RESEARCH.md) §2。

## 是什么

imagent 是一个常驻网关进程：监听 IM 私聊消息 → 鉴权 → 驱动 agent（默认 Claude Code）执行真实任务（读写文件 / 跑命令 / 改代码）→ 把结果回传 IM。

**杀手锏**：agent 遇危险操作（如 `Bash`）时，在 IM 里向你 approve/deny——把 agent 的执行权关进用户审批的笼子。

## 特性

- 🌉 **平台 / 后端双抽象**：换 IM 只加 adapter，换 agent 只加 impl。
- 🔐 **安全第一**：发送者白名单 + `--allowedTools` 收敛 + workdir 锁定 + **IM 内权限审批闭环**。
- 💬 **会话连续**：per-chat session 持久化（SQLite），重启可续；`--resume`；`/switch` 多命名会话。
- 🛡️ **限流熔断**：`sendmessage` 服从式退避（防封号，不绕风控）。
- 🎨 **媒体收发**：图片 / 文件（AES-128-ECB + CDN，协议强制）。
- ⚡ **流式反馈**：工具调用摘要、typing 指示、中间事件推流。
- 📦 **单二进制**、低占用，适合常驻 NAS / 小服务器 / 笔记本。

## 架构

```
trait Platform                        trait Backend
├── ilink (个人微信私聊, 实验性)        ├── claude (CLI + ACP 长驻子进程)
└── wecom  (企业微信长连接)             ├── codex  (codex exec --json)
                                       └── gemini (gemini -p -o stream-json)
        ↕                              ↕
              core: 调度 / 鉴权 / 会话路由 (store 持久化) / 权限审批闭环
```

三层 + 双抽象：core 持有 `Platform` 与 `Backend` trait，平台与后端各自独立可换。session 生命周期提到 core（store 持久化），Backend 退化为无状态执行器——比把 session 塞进 Backend 内存更干净，支持重启续接。

## 设计取舍

imagent 的几个关键取舍（解释「为什么这么设计」，而非与某个项目比高低）：

- **发送者白名单是硬约束，不是可选**：iLink bot 任何人都能加好友，没有白名单 = 任意人都能驱动你的 agent 执行命令。
- **session 持久化到 SQLite**：进程重启可续（`--resume`），崩溃不丢上下文。SQLite 经 `rusqlite` 的 `bundled` feature **静态链接进二进制**，运行时无需宿主安装 SQLite。
- **IM 内权限审批闭环**（核心特性）：危险工具（如 `Bash`）执行前，先在 IM 向你 approve/deny——把 agent 的执行权关进用户审批的笼子。
- **限流服从式退避**：被限流就退避等待，**绝不绕过风控**（合规红线）。
- **单二进制 + 低运行时依赖**：除 Linux 下凭据可选经 `libdbus`（Secret Service；无该环境则自动回退，见 [安全](#安全)）外，不依赖宿主环境。

## 快速开始

### 安装

**前置**：macOS 或 Linux（Windows 暂不支持——IM 权限审批闭环与配置热重载依赖 Unix domain socket / SIGHUP）；默认 agent 后端 Claude Code CLI（`npm i -g @anthropic-ai/claude-code`）。

**方式一 · 下载预编译二进制（推荐，免装 Rust）**：从 [GitHub Releases](https://github.com/uzziahlin/imagent/releases) 取对应平台文件（每个 release 附 `sha256` 校验）：

| 平台 | 文件 |
|---|---|
| macOS · Apple Silicon | `imagent-darwin-arm64` |
| macOS · Intel | `imagent-darwin-x86_64` |
| Linux · x86_64 | `imagent-linux-x86_64` |

```bash
# 示例：macOS Apple Silicon
curl -L -o imagent https://github.com/uzziahlin/imagent/releases/latest/download/imagent-darwin-arm64
chmod +x imagent && sudo mv imagent /usr/local/bin/
# 可选：校验完整性
curl -L -o /tmp/imagent.sha256 https://github.com/uzziahlin/imagent/releases/latest/download/imagent-darwin-arm64.sha256
(cd /tmp && shasum -a 256 -c imagent.sha256)
```

**方式二 · 源码构建（需 Rust 1.88+）**：

```bash
git clone https://github.com/uzziahlin/imagent
cd imagent
cargo build --release
# 二进制：target/release/imagent
```

### 配置

```bash
mkdir -p ~/.imagent
cat > ~/.imagent/config.toml <<'EOF'
default_workdir = "/absolute/path/to/agent/workspace"  # 必填，agent 的 cwd（非沙箱：不限制可读路径，靠 allowed_tools + permission_mode 兜底）
allowed_senders = []        # 留空 = 发现模式（先看日志拿你的 from_user_id）
allowed_tools = ["Read", "Edit"]
# permission_mode = "off"  # off / allow / deny / ask（放 Bash 等危险工具时用 ask）
EOF
```

### 登录 + 运行

> ⚠️ **macOS 撞名**：`imagent` 也是 macOS 系统输入法进程（Input Method Agent）。**不要用 `pkill imagent`**——会杀掉系统输入法。停止本程序请用前台 `Ctrl-C` 或全路径 `kill $(pgrep -f /usr/local/bin/imagent)`（详见 [部署](deploy/README.md)）。

```bash
imagent login            # 扫码登录 iLink，凭据落盘 ~/.imagent/imagent.db
imagent start            # 前台常驻，Ctrl-C 退出
```

用**另一个**微信号给 bot 发私聊：
1. 第一次用发现模式（`allowed_senders = []`），日志里看到你的 `from_user_id`。
2. `imagent allow <from_user_id>` 授权（或填进 config 重启）。
3. 之后发消息 → agent 执行 → 结果回传 IM。

## 命令（IM 内私聊）

| 命令 | 作用 |
|---|---|
| `/new` | 重置会话（开新上下文） |
| `/switch <name>` | 切到 / 新建命名会话（多任务并行上下文） |
| `/sessions` | 列命名会话（`*` 标当前） |
| `/compact` | 软压缩上下文（摘要 + 重置 + 延续） |
| `/allow <id>` | 授权一个 sender（仅已授权用户可执行——管理员模型） |
| `/disallow <id>` | 撤销（不可撤销自己，防锁死） |
| `/list` | 查白名单 |
| `/whoami` | 查自己的 sender id |

## 权限审批闭环（杀手锏）

`permission_mode = "ask"` + `allowed_tools = ["Read","Edit","Bash"]` 时，agent 调 `Bash` 前会在 IM 询问：

```
🔐 Claude 请求执行 Bash({"command":"..."})
回复 y 允许，其它拒绝。
```

回复 `y` → 执行；其它 → 拒绝。基于 Claude Code 的 `--permission-prompt-tool` MCP 回调实现。

## 安全

- **白名单鉴权**：非白名单 sender 丢弃（iLink bot 任何人可加好友，这步不可省）。
- **工具收敛**：`allowed_tools` 配置驱动（起步 `Read,Edit`）；workdir 用 `current_dir` 锁定。
- **权限审批**：危险操作 IM approve/deny。
- **store 加固**：文件 0600 / 目录 0700；CDN 下载 SSRF 白名单。
- 详见 [`SECURITY.md`](SECURITY.md)。

## 路线

| 阶段 | 状态 | 交付 |
|---|---|---|
| P0 | ✅ | 调研（iLink 协议/合规、Claude CLI/ACP、竞品 feiyun） |
| P1 | ✅ | MVP 闭环：扫码 → 私聊 → `claude -p` → 回传 → `--resume` |
| P2 | ✅ | 限流熔断 / 动态白名单 / 多命名会话 / 软 compact / 推流 / typing / **权限审批** / 媒体 |
| P3 | ✅ | 开源化（MIT license/CI/凭据加密/mdBook）+ WeCom + ACP + 多 agent（Codex/Gemini）+ 运维（指标/热重载/daemon）+ 长消息分片 |

> **当前状态**：功能完整，但仍在收尾安全审查修复（见 [`CHANGELOG`](CHANGELOG.md) `[Unreleased]` 与 [`CODE_REVIEW_v4`](docs/CODE_REVIEW_v4.md) / [`v5`](docs/CODE_REVIEW_v5.md)），故版本徽章为 pre-release。功能层面 P0–P3 均已交付。

详见 [`docs/`](docs/)（[DESIGN](docs/DESIGN.md) / [RESEARCH](docs/RESEARCH.md) / [CODE_REVIEW_v4](docs/CODE_REVIEW_v4.md) / [CODE_REVIEW_v5](docs/CODE_REVIEW_v5.md)）。

## 开发

```bash
cargo test --workspace                              # 全通过（详情见 CI）
cargo clippy --workspace --all-targets -- -D warnings   # 0 warning
cargo fmt --all --check
```

crate：`core`（调度/鉴权/session/权限）+ `ilink`（iLink 协议）+ `wecom`（企业微信长连接）+ `claude`（CLI/ACP backend）+ `codex` + `gemini` + `store`（SQLite）。

## License

MIT（见 [`LICENSE`](LICENSE)）。iLink 协议出处：腾讯官方 [`@tencent-weixin/openclaw-weixin`](https://github.com/Tencent/openclaw-weixin) / [ClawBot 文档](https://developers.weixin.qq.com/doc/aispeech/knowledge/openapi/Clawbotrelated.html)。
