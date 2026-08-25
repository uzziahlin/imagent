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
- 💻 **终端 agent 反向接入（ask_via_im）**：电脑终端上任意 agent 需要你决策时，把问题转发到飞书——人不在电脑前也能在手机上点按钮作答；多 agent 并发按 request_id 精确分发（见[终端 agent 接入](#终端-agent-接入ask_via-im人不在电脑前也能问你)）。
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

**方式零 · 一键脚本（推荐）**：安装二进制（含 sha256 校验）→ 首次生成 `~/.imagent/config.toml`（可交互填飞书凭据）→ 自动挂载 MCP（有 `claude` CLI 直接 `claude mcp add`，否则打印可贴的 JSON）。已有 config 绝不覆盖，可重复运行：

```bash
bash <(curl -fsSL https://raw.githubusercontent.com/uzziahlin/imagent/main/install.sh)
# 等价参数式：--workdir <path> --app-id <cli_xxx> --secret <s> --yes --mcp-only
#            （--version <tag> / --bin <dir> 指定版本与安装目录；详见脚本头注释）
```

> 最新 release 尚未包含 `mcp-ask` 子命令（ask_via_im 需 v1.3.0+）时，脚本检测到后会用本机 cargo 自动源码构建兜底。

**方式一 · 下载预编译二进制（免装 Rust）**：从 [GitHub Releases](https://github.com/uzziahlin/imagent/releases) 取对应平台文件（每个 release 附 `sha256` 校验）：

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
# allowed_tools 不写 = 全部工具（不收敛）；要白名单就显式列，如 ["Read","Edit"]；执行类建议配 permission_mode="ask" 过审
# permission_mode = "auto"  # 缺省=auto：claude-cli 起 IM 审批闭环（同 ask），其余后端=off；也可显式 off/allow/deny/ask
# allowed_chats = ["feishu:oc_xxx"]  # 会话(群)白名单：群消息 chat 放行 OR sender 放行（/chat 可动态管理）
# ask_via_im_conv = "feishu:ou_xxx"  # 终端 agent 的 ask_via_im 提问投递会话（配了才启用，见「终端 agent 接入」）
# agent_idle_timeout_secs = 300      # 空闲看门狗：连续无输出 N 秒自动终止（0=关）
# batch_window_ms = 1500             # 连发消息合并为一轮 prompt 的窗口（0=关）
# cot_detail = "brief"               # 工具过程展示 off / brief / detailed（/config 可热改）
# platform = "feishu"                # wecom/feishu 经 config 凭据接入（见下）
EOF
```

> **`allowed_tools` 要不要写？** 不必填——**缺省即全部工具**（`["*"]` 语义：不附加 claude 的 `--allowedTools`，CLI 自身默认全量；codex 收敛到 `workspace-write`、gemini 收敛到 `auto_edit`，均不进各自最高危档）。要收敛 agent 的能力边界就显式列白名单：清单外的工具 agent 根本用不了。注意**全量/清单内 ≠ 免审**——缺省 `permission_mode = "auto"`（claude-cli 即 IM 审批闭环）下，危险操作（如每条 Bash 命令）执行前仍会在 IM 向你审批；显式写 `[]` 与 `["*"]` 同义（不限制）。

> **飞书**：`platform = "feishu"` + `feishu_app_id` + 环境变量 `IMAGENT_FEISHU_APP_SECRET`——完整开通步骤见[接入飞书](#接入飞书完整流程)。**WeCom**：`wecom_bot_id` + `wecom_secret`。两者都免公网（长连接收，HTTP 发）。

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

## 接入飞书（完整流程）

飞书走**企业自建应用 + 长连接**：不需要公网 IP / 域名 / 证书，imagent 主动连飞书 WS 收事件、走 OpenAPI 发消息，适合家宽 / NAS 部署。全程约 10 分钟（`imagent setup` 向导可交互走一遍同样流程并校验凭据连通性；`--platform feishu|wecom|ilink` 直达对应平台引导）：

**① 创建应用**：打开 [open.feishu.cn/app](https://open.feishu.cn/app) →「创建企业自建应用」→「添加应用能力」→ 启用**机器人**。

**② 事件订阅（长连接）**：「开发配置」→「事件与回调」→ 订阅方式选**使用长连接接收事件**，然后添加事件：

| 事件 | 用途 | 必须 |
|---|---|---|
| `im.message.receive_v1` | 收私聊 / 群 @ 消息 | ✅ |
| `card.action.trigger` | 卡片按钮回调（审批 / 问题 / 命令按钮卡） | ✅ |
| `drive.file.comment.created_v1` | 云文档评论 @bot 触发 | 可选 |

**③ 开通权限**：「权限管理」开通并**发布**：

- `im:message`（读取与发送单聊、群聊消息）——必须；
- `im:message.group_at_msg`（仅收 @机器人 的群消息；要全收群消息改用 `group_msg` 并把 config 的 `feishu_require_mention_in_group` 设为 `false`）；
- `cardkit:card:write`（CardKit 流式卡片）——可选，缺省自动降级整卡刷新；
- `drive:comment`（云文档评论）——可选，配合上表评论事件。

**④ 发布生效**：「版本管理与发布」→ 创建版本并发布——**权限与事件订阅都要发布后才生效**，新手最常漏这步。

**⑤ 配置凭据**：开放平台「凭证与基础信息」页拿 App ID / App Secret：

```toml
# ~/.imagent/config.toml
platform = "feishu"
feishu_app_id = "cli_xxx"
```
```bash
export IMAGENT_FEISHU_APP_SECRET="你的 App Secret"   # 建议写进 ~/.zshrc；secret 不落 config
```

**⑥ 启动 + 授权自己**：

```bash
imagent start               # 缺省读 config 的 platform（feishu）；显式可 --platform feishu
                            # 日志看到 connected to wss://msg-frontier.feishu.cn 即接入成功
```

在飞书里搜到机器人，给它发一条消息（此时白名单为空，日志会打出你的 `ou_xxx` open_id）→ 授权：

```bash
imagent allow ou_xxx        # 或 config 里填 allowed_senders = ["ou_xxx"]
```

之后发消息 agent 即执行并回传；要启用终端 agent 提问转发（ask_via_im），再在 config 设 `ask_via_im_conv = "feishu:ou_xxx"`（见[终端 agent 接入](#终端-agent-接入ask_via-im人不在电脑前也能问你)）。


## 后台常驻（imagent service）

前台 `start` 验证可用后，装成 OS 级后台服务（需 ≥ v1.5.1：早前版本生成的服务定义
缺 `--platform`，飞书用户守护进程会误走 ilink）：

```bash
# ① secret 必须在当前 shell 里 export——install 会把它「快照」进服务定义
#    （守护进程起不来交互 shell，这是唯一注入点；缺失会直接报错提示）
export IMAGENT_FEISHU_APP_SECRET="你的 App Secret"

# ② 安装并启动（注册当前二进制路径 + config 里的 platform；崩溃自动拉起、开机自启）
imagent service install
```

> 二进制先放到稳定路径（如 `/usr/local/bin/imagent`）再 install——注册的是
> `current_exe`，别用下载目录 / 临时构建产物。

```bash
imagent service status     # 运行状态
imagent service uninstall  # 停止并卸载
```

| | macOS（launchd 用户代理） | Linux（systemd 用户单元） |
|---|---|---|
| 服务名 | `com.imagent[.<profile>]` | `imagent[-<profile>]` |
| 定义 | `~/Library/LaunchAgents/*.plist` | `~/.config/systemd/user/*.service` |
| 日志 | `~/.imagent/logs/daemon.log` | `journalctl --user -u imagent -f` |
| 无人登录也运行 | 天然支持（登录即启） | 需一次 `loginctl enable-linger $USER`（服务器场景） |

secret 轮换 / 环境变量变化后：重新 `export` + `imagent service install`（先卸旧再装新，等效更新）。多实例：`imagent --profile work service install` → 独立服务与状态目录。

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
| `/img <path>` `/file <path>` | 发 workdir 内图片 / 任意文件到 IM |
| `/timeout [N\|off\|default]` | 会话级空闲看门狗（分钟） |
| `/perm <auto\|off\|allow\|deny\|ask>` | 权限模式热切（auto=按后端自动选档） |
| `/stop` | 中断当前在飞任务（杀 agent 子进程，清空排队消息） |
| `/config [k v]` | 查看 / 热改配置（cot_detail / batch_window_ms / agent_idle_timeout_secs / require_mention / reply_mode） |
| `/status` `/doctor` `/reconnect` | 运行状态 / 自检 / 强制平台重连 |
| `/allow <id\|@名字>` `/disallow` | 授权 / 撤销 sender（飞书群内可直接 @ 对方，管理员门槛） |
| `/admin [list\|add\|remove]` | 管理员动态管理（首位设立自动带操作者，防自锁） |
| `/chat allow\|deny\|allow-all\|list` | 会话（群）白名单；`allow-all` 批量放行 bot 已加入的全部群 |
| `/list` `/whoami` | 查白名单 / 查自己的 sender 与会话 id |

群消息默认须 `@机器人`（`feishu_require_mention_in_group`，正文 @ 占位自动清洗）；`/config reply_mode text` 可切纯文本回复（无卡片权限或偏好简洁时）。

## 权限审批闭环（杀手锏）

`permission_mode = "ask"` + `allowed_tools = ["Read","Edit","Bash"]` 时，agent 调 `Bash` 前会在 IM 询问：

```
🔐 Claude 请求执行 Bash({"command":"..."})
回复 y 允许，其它拒绝。
```

回复 `y` → 执行；其它 → 拒绝。基于 Claude Code 的 `--permission-prompt-tool` MCP 回调实现。**飞书**下询问是「✅ 允许 / ⛔ 拒绝」按钮卡片——点一下即回，无需打字。等审批期间 `/stop` 仍可用（自动回 deny 中止）。

## 终端 agent 接入：ask_via_im（人不在电脑前也能问你）

反向场景：你电脑终端上跑的 **任意 agent**（Claude Code / ZCode / Codex…）需要你决策时，把问题转发到你的飞书——你在手机上点选项或回文字，答案直接回到终端的 agent。适合挂个长任务离开工位。

```
终端 agent ──MCP(stdio)──► imagent mcp-ask ──unix socket──► imagent 主进程
                                                                │ 飞书问题卡（选项按钮）
终端 agent ◄──用户回复原文────────────────────────────────────────┘
```

### 1. 主进程配置（一次）

```toml
# ~/.imagent/config.toml（platform = "feishu" 时）
ask_via_im_conv = "feishu:ou_xxx"     # 你和 bot 的私聊（/whoami 可查）
# ask_via_im_timeout_secs = 1800      # 等待超时，默认 30 分钟
```

`imagent start feishu` 保持运行即可（socket/token 鉴权与审批闭环共用）。

### 2. 挂到终端 agent（一键）

```bash
# 生成 mcpServers 配置（command 自动填当前二进制的绝对路径）：
imagent mcp-ask --print-config
# {"mcpServers":{"imagent":{"command":"/usr/local/bin/imagent","args":["mcp-ask"]}}}
```

- **Claude Code**：`claude mcp add imagent -- /usr/local/bin/imagent mcp-ask`
- **其它 MCP client（ZCode / Cursor 等）**：把上面 `--print-config` 的 JSON 并入 MCP 配置即可。
- **懒人路径**：`bash <(curl -fsSL .../install.sh)` 的安装脚本最后一步会自动完成上述挂载（见[安装](#安装)）。

再在 agent 的指令文件（`CLAUDE.md` / `AGENTS.md`）里加一句：

> 需要我决策/确认且我可能不在终端前时，调用 `ask_via_im` 工具提问（`source` 传项目名），不要只在本地等待。

### 3. 使用语义

- 工具参数：`question`（多行 markdown 可写补充说明）、`options`（≤8 个选项按钮）、`source`（提问方标记，多 agent 并发时区分「谁在问」）、`timeout_secs`。
- 多 agent 并发提问互不干扰（`conv + request_id` 多 pending 路由）：**点按钮=精确回答那张卡**；直接打字=回答**最新**一张；引用回复=回答被引用的卡。
- 超时返回错误（非 deny），agent 可自行决定重试。

## 安全

- **白名单鉴权**：sender 白名单 + 会话（群）白名单，非授权丢弃（iLink bot 任何人可加好友，这步不可省）。
- **工具收敛**：`allowed_tools` 可选（缺省 = 全部工具，`[]`/`["*"]` 同义不限制；显式清单 = 白名单）；workdir 用 `current_dir` 锁定，危险操作靠 `permission_mode = "ask"` IM 审批兜底。
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
| P6 | ✅ | 第二轮对标：mention 基础设施（@过滤/@剥离/`/allow @提及`）+ 命令交互卡片 + 话题群隔离 + `setup` 向导 / `service` 自管理 + 出站文件 + `/cd` 安全校验 + 会话级 `/timeout` |
| P7 | ✅ | 对标收尾：`/admin` 管理员动态管理（防自锁）+ `/chat allow-all` 批量放行 + 陌生人 @ 提示开关 + `/config reply_mode` 回复偏好 + `profile export/import`（纪要见 [`docs/internal/P4_ROADMAP.md`](docs/internal/P4_ROADMAP.md) §P7） |

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
