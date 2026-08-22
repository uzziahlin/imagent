# imagent

> **Instant messaging, meet your agent.**

一个用 Rust 写的、把即时通讯平台接入自主 agent 的网关。**任何 IM**（个人微信 iLink / 企业微信 WeCom / 飞书 Feishu）↔ **任何 agent**（Claude Code / Codex / Gemini）。

![Rust](https://img.shields.io/badge/Rust-edition%202021-orange) ![License: MIT](https://img.shields.io/badge/License-MIT-blue) ![CI](https://github.com/uzziahlin/imagent/actions/workflows/ci.yml/badge.svg) ![GitHub release](https://img.shields.io/github/v/release/uzziahlin/imagent) ![Docs](https://img.shields.io/badge/docs-mdBook-blueviolet)

> 🌐 **English TL;DR** — `imagent` is a Rust gateway that bridges any instant-messaging platform (WeChat **iLink** / **WeCom** / **Feishu**) with any autonomous agent (**Claude Code** / Codex / Gemini). It turns an IM chat into an **approval-gated** agent cockpit: the agent runs real tasks (read/write files, run commands, edit code) but must ask for your `y/n` (or a tap on an approval button card) in IM before any dangerous tool. Pluggable on both sides (`Platform` / `Backend` traits), single binary, SQLite-backed sessions, batched messages, `/stop` task control and an idle watchdog.
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
- 🔐 **安全第一**：发送者白名单 + 会话（群）白名单 + `--allowedTools` 收敛 + workdir 锁定 + **IM 内权限审批闭环**（按钮卡片 / 文本 y/n）。
- 💬 **会话连续**：per-chat session 持久化（SQLite），重启可续；`--resume`；`/switch` 多命名会话；`/resume` 统一列表无感接管历史/电脑端 Claude Code 会话。
- 🛑 **任务控制**：`/stop` 随时中断在飞任务（杀 agent 子进程）；空闲看门狗自动终止无输出的僵死任务。
- 🔁 **消息批处理**：运行中到达的消息排队，与连发消息合并为一轮执行（不重复跑轮、不烧 token）。
- 🛠️ **IM 内运维**：`/status` `/doctor` `/reconnect` `/config`（COT 三档展示 off/brief/detailed 等热改）。
- 📄 **飞书生态**：CardKit 真流式卡片、审批按钮卡片、云文档评论 @bot 触发（同评论线程回复）。
- 🧩 **Profile 多实例**：`--profile` 一部署多 bot 身份（config/db/socket/媒体全隔离）。
- 🛡️ **限流熔断**：`sendmessage` 服从式退避（防封号，不绕风控）。
- 🎨 **媒体收发**：图片 / 文件（AES-128-ECB + CDN，协议强制）。
- ⚡ **流式反馈**：工具调用摘要、typing 指示、中间事件推流。
- 📦 **单二进制**、低占用，适合常驻 NAS / 小服务器 / 笔记本。

## 架构

```
trait Platform                        trait Backend
├── ilink  (个人微信私聊, 实验性)       ├── claude (CLI + ACP 长驻子进程)
├── wecom  (企业微信长连接)             ├── codex  (codex exec --json)
└── feishu (飞书私聊/群/云文档评论)     └── gemini (gemini -p -o stream-json)
        ↕                              ↕
              core: 调度 / 鉴权 / 会话路由 (store 持久化) / 权限审批闭环
                    任务控制(/stop/批处理/看门狗) / 会话白名单 / 统一 resume
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

**方式二 · 源码构建（需 Rust 1.88+；启用飞书平台需额外 `protoc`）**：

> 飞书平台经 `open-lark` 的 websocket feature 编译，其 build script 需要系统 `protoc`（Protocol Buffers 编译器）：macOS `brew install protobuf`，Ubuntu `sudo apt-get install -y protobuf-compiler`。**仅构建时需要，运行时不需要**。

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
# permission_mode = "off"   # off / allow / deny / ask（放 Bash 等危险工具时用 ask）
# allowed_chats = ["feishu:oc_xxx"]  # 会话(群)白名单：群消息 chat 放行 OR sender 放行（/chat 可动态管理）
# agent_idle_timeout_secs = 300      # 空闲看门狗：连续无输出 N 秒自动终止（0=关）
# batch_window_ms = 1500             # 连发消息合并为一轮 prompt 的窗口（0=关）
# cot_detail = "brief"               # 工具过程展示 off / brief / detailed（/config 可热改）
# platform = "feishu"                # wecom/feishu 经 config 凭据接入（见下）
EOF
```

> **飞书**：`platform = "feishu"` + `feishu_app_id` + 环境变量 `IMAGENT_FEISHU_APP_SECRET`；需在飞书后台开通长连接事件订阅（消息 / `card.action.trigger` 审批回调 / 可选 `drive.file.comment.created_v1` 云文档评论）。**WeCom**：`wecom_bot_id` + `wecom_secret`。两者都免公网（长连接收，HTTP 发）。

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

**多实例（Profile）**：`imagent profile create work` → `imagent --profile work start`——config/db/socket/媒体全隔离，一机多 bot 身份。

## 命令（IM 内）

| 命令 | 作用 |
|---|---|
| `/new` | 重置会话（开新上下文） |
| `/switch <name>` | 切到 / 新建命名会话（多任务并行上下文） |
| `/sessions` | 列命名会话（`*` 标当前） |
| `/resume [n]` | 统一恢复列表：📱 IM 会话 ∪ 💻 电脑端 Claude Code 会话（摘要+时间辨认，按序号接管，无需会话 id） |
| `/compact` | 软压缩上下文（摘要 + 重置 + 延续） |
| `/cd [path]` | 切工作目录（`/resume` 本机会话列表随之变化） |
| `/ws list\|save\|use\|remove` | 命名工作空间 |
| `/img <path>` | 发 workdir 内图片到 IM |
| `/perm <off\|allow\|deny\|ask>` | 权限模式热切 |
| `/stop` | 中断当前在飞任务（杀 agent 子进程，清空排队消息） |
| `/config [k v]` | 查看 / 热改配置（cot_detail / batch_window_ms / agent_idle_timeout_secs） |
| `/status` `/doctor` `/reconnect` | 运行状态 / 自检 / 强制平台重连 |
| `/allow <id>` `/disallow <id>` | 授权 / 撤销 sender（管理员门槛） |
| `/chat allow\|deny\|list` | 会话（群）白名单管理 |
| `/list` `/whoami` | 查白名单 / 查自己的 sender 与会话 id |

## 权限审批闭环（杀手锏）

`permission_mode = "ask"` + `allowed_tools = ["Read","Edit","Bash"]` 时，agent 调 `Bash` 前会在 IM 询问：

```
🔐 Claude 请求执行 Bash({"command":"..."})
回复 y 允许，其它拒绝。
```

回复 `y` → 执行；其它 → 拒绝。基于 Claude Code 的 `--permission-prompt-tool` MCP 回调实现。**飞书**下询问是「✅ 允许 / ⛔ 拒绝」按钮卡片——点一下即回，无需打字。等审批期间 `/stop` 仍可用（自动回 deny 中止）。

## 安全

- **白名单鉴权**：sender 白名单 + 会话（群）白名单，非授权丢弃（iLink bot 任何人可加好友，这步不可省）。
- **工具收敛**：`allowed_tools` 配置驱动（起步 `Read,Edit`）；workdir 用 `current_dir` 锁定。
- **权限审批**：危险操作 IM approve/deny（文本 / 按钮卡片）。
- **store 加固**：文件 0600 / 目录 0700；CDN 下载 SSRF 白名单。
- 详见 [`SECURITY.md`](SECURITY.md)。

## 路线

| 阶段 | 状态 | 交付 |
|---|---|---|
| P0 | ✅ | 调研（iLink 协议/合规、Claude CLI/ACP、竞品 feiyun） |
| P1 | ✅ | MVP 闭环：扫码 → 私聊 → `claude -p` → 回传 → `--resume` |
| P2 | ✅ | 限流熔断 / 动态白名单 / 多命名会话 / 软 compact / 推流 / typing / **权限审批** / 媒体 |
| P3 | ✅ | 开源化（MIT license/CI/凭据加密/mdBook）+ WeCom + ACP + 多 agent（Codex/Gemini）+ 运维（指标/热重载/daemon）+ 长消息分片 |
| P4 | ✅ | 任务控制（`/stop`/消息批处理/空闲看门狗）+ 飞书平台（CardKit 流式卡片/审批按钮/云文档评论）+ 会话白名单 + COT 三档 `/config` + IM 诊断命令 + 统一 `/resume`（接管电脑端会话）+ Profile 多实例 |
| P6 | 🔜 | 第二轮对标：mention 基础设施（@过滤/@剥离/`/allow @提及`）+ 命令交互卡片 + 话题群隔离 + `setup` 向导 / `service` 自管理 + 出站文件 + `/cd` 安全校验 + 会话级 `/timeout`（纪要见 [`docs/internal/P4_ROADMAP.md`](docs/internal/P4_ROADMAP.md) §P6） |

> **当前状态**：**v1.0.0 已发布**（见 [Releases](https://github.com/uzziahlin/imagent/releases)）。P0–P4 全部交付；P4 纪要见 [`docs/internal/P4_ROADMAP.md`](docs/internal/P4_ROADMAP.md)，P5（安全与正确性）七波已收官；剩余为 v1.1+ 架构建议（见 [`CODE_REVIEW_v6`](docs/CODE_REVIEW_v6.md) §架构建议）。

详见 [`docs/`](docs/)（[DESIGN](docs/DESIGN.md) / [RESEARCH](docs/RESEARCH.md) / [CODE_REVIEW_v4](docs/CODE_REVIEW_v4.md) / [CODE_REVIEW_v5](docs/CODE_REVIEW_v5.md)）。

## 开发

```bash
cargo test --workspace                              # 全通过（详情见 CI）
cargo clippy --workspace --all-targets -- -D warnings   # 0 warning
cargo fmt --all --check
```

crate：`core`（调度/鉴权/session/权限/任务控制）+ `ilink`（iLink 协议）+ `wecom`（企业微信长连接）+ `feishu`（飞书长连接 + CardKit + 云文档评论）+ `claude`（CLI/ACP backend）+ `codex` + `gemini` + `store`（SQLite）。

## License

MIT（见 [`LICENSE`](LICENSE)）。iLink 协议出处：腾讯官方 [`@tencent-weixin/openclaw-weixin`](https://github.com/Tencent/openclaw-weixin) / [ClawBot 文档](https://developers.weixin.qq.com/doc/aispeech/knowledge/openapi/Clawbotrelated.html)。
