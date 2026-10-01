# imagent

> **Instant messaging, meet your agent.**

一个用 Rust 写的、把即时通讯平台接入自主 agent 的网关。**任何 IM**（个人微信 iLink / 企业微信 WeCom / 飞书 Feishu）↔ **任何 agent**（Claude Code / Codex / Gemini）。

![Rust](https://img.shields.io/badge/Rust-edition%202021-orange) ![License: MIT](https://img.shields.io/badge/License-MIT-blue) ![CI](https://github.com/uzziahlin/imagent/actions/workflows/ci.yml/badge.svg) ![GitHub release](https://img.shields.io/github/v/release/uzziahlin/imagent) ![Docs](https://img.shields.io/badge/docs-mdBook-blueviolet)

> 🌐 **English TL;DR** — `imagent` is a Rust gateway that bridges any instant-messaging platform (WeChat **iLink** / **WeCom** / **Feishu**) with any autonomous agent (**Claude Code** / Codex / Gemini). It turns an IM chat into an **approval-gated** agent cockpit: the agent runs real tasks (read/write files, run commands, edit code) but must ask for your `y/n` (or a tap on an approval button card) in IM before any dangerous tool. Pluggable on both sides (`Platform` / `Backend` traits), single binary, SQLite-backed sessions, crash-safe message queue, scheduled prompts (`/cron`), batched messages, `/stop` task control and an idle watchdog.
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

imagent 是一个常驻网关进程：监听 IM 私聊 / 群聊消息 → 鉴权 → 驱动 agent（默认 Claude Code）执行真实任务（读写文件 / 跑命令 / 改代码）→ 把结果流式回传 IM。

**杀手锏**：agent 遇危险操作（如 `Bash`）时，在 IM 里向你 approve/deny——把 agent 的执行权关进用户审批的笼子。

## 特性

- 🌉 **平台 / 后端双抽象**：换 IM 只加 adapter，换 agent 只加 impl。
- 🔐 **安全第一**：发送者白名单 + 会话（群）白名单 + `allowed_tools` 收敛 + workdir 锁定 + **IM 内权限审批闭环**（按钮卡片 / 文本 y/n——按钮卡片仅飞书）。
- 💬 **会话连续**：per-chat session 持久化（SQLite），重启可续；`--resume`；`/switch` 多命名会话；`/resume` 统一列表无感接管历史/电脑端 Claude Code 会话。
- ⏰ **定时任务（/cron）**：5 字段 cron 表达式（本地时区含 DST）+ store 持久化，到期消息走与手打完全相同的鉴权/审批管线——日报、巡检、定时批处理一句话建好；停机补跑策略 `cron_catchup = one|off|all`（逐周期补跑上限 3 条 / 陈旧跳过 / 触发一次）。
- 📡 **Webhook 入站**：`POST /hook/<token>` 把 CI 失败、监控告警等外部事件注入指定会话——与手打消息同权走鉴权/审批管线（token 路径鉴权 + HMAC 验签 + 两层防重放 + 会话白名单，无旁路；非 loopback 绑定未配 secret 拒绝启动），agent 接事件自动排障、审批卡上放行修复。
- 🛑 **任务控制（steering）**：`/stop` 随时中断在飞任务（杀 agent 子进程），**排队消息保留并自动转入下一轮**（对齐 Claude Code 的 Esc + 队列注入语义——运行中发补充/纠正不再丢，注入条数上卡片 footer 可见）；空闲看门狗自动终止无输出的僵死任务；失败后一键 `/retry` 续接。
- 🛟 **崩溃不丢消息**：排队消息实时落库（schema v12），进程崩溃 / `kill -9` / 断电后重启自动重放——批处理与 steering 队列不再随进程消失；**执行中的轮次**同样留痕（轮首落 inflight 标记），重启后通知会话可 `/retry` 一键续跑。
- 🔁 **消息批处理**：运行中到达的消息排队，与连发消息合并为一轮执行（不重复跑轮、不烧 token；批窗口静默判停自适应）；`/queue list|drop` 队列可视化管理。
- 📊 **用量护栏**：`/stats` 成本统计 + 自动 compact（**比例档：水位达模型上下文窗口 80% 触发**；ACP 路径经 `UsageUpdate.size` **自动学习真实窗口**——200k 模型零配置防溢出）+ per-sender 成本上限（滚动 24h）；压缩通知/摘要走命令卡。
- 💭 **thinking / 任务清单**：思考过程与正文分离透出（卡片折叠区展示，cot 档位控制）；Claude Code 的 Task* / ACP Plan 渲染成卡片 checklist 进度。
- 🎤 **语音输入（飞书）**：语音条自动转文字进 prompt（speech_to_text，需后台申请语音识别权限）。
- 🛠️ **IM 内运维**：`/status` `/doctor` `/reconnect` `/config`（COT 三档展示 off/brief/detailed 等热改；SIGHUP 热载工具白名单/审批集/管理员名单/压缩阈值）。
- 📄 **飞书生态**（一等公民）：CardKit 真流式卡片（分阶段 footer + 工具 ⏳/✅ 实时行 + ⏹ 终止按钮）、审批/问题/命令标题卡（按钮 primary/danger + flow 自适应布局）、`/config` 下拉表单卡、邮箱掩码防租户审计拦截、云文档评论 @bot 触发（同评论线程回复）、合并转发聊天记录自动转录、**群聊上下文注入**（群消息触发轮次自动拉本群最近 N 条消息作前置上下文——Slack 线程上下文的飞书等价物，`feishu_group_context_messages`，默认 10、0=关闭）、**群里回复 bot 消息发图/文件 = 显式定向**（豁免 @，手机端纯图片可达）、**多维表格数据面**（agent 经 `bitable_list_fields` / `bitable_append_row` 工具把任务清单、巡检结果、成本台账等结构化产出直接写进飞书 Bitable——Slack 生态无等价物，见[多维表格数据面](#多维表格数据面agent-的结构化产出)）。
- 💻 **终端 agent 反向接入（ask_via_im）**：电脑终端上任意 agent 需要你决策时，把问题转发到飞书——人不在电脑前也能在手机上点按钮作答；多 agent 并发按 request_id 精确分发（见[终端 agent 接入](#终端-agent-接入ask_via-im人不在电脑前也能问你)）。
- 🧩 **Profile 多实例**：`--profile` 一部署多 bot 身份（config/db/socket/媒体全隔离）。
- 🛡️ **限流熔断**：`sendmessage` 服从式退避（防封号，不绕风控）。
- 🎨 **媒体收发**：图片 / 文件（AES-128-ECB + CDN，协议强制；仅 iLink——飞书走 OpenAPI 上传，wecom 暂不支持媒体发送）；下载/转码全程超时与大小上限，媒体目录 7 天自动 GC。
- ⚡ **流式反馈**：工具调用摘要（`Bash — git status` 人可读单行 + 执行状态图标）、typing 指示、中间事件推流；媒体处理与消息收发解耦——飞书已完全解耦（文本先投递、媒体异步后补），iLink 为批内并发下载（4 并发限流）+ 单条消息 60s 总预算限界，大文件不再无限期阻塞收发。
- 📦 **单二进制**、低占用，适合常驻 NAS / 小服务器 / 笔记本；`service install` 一键装成 launchd/systemd 服务（异常退出自动拉起）。

## 架构

```
trait Platform                        trait Backend
├── ilink  (个人微信私聊, 实验性)       ├── claude (CLI + ACP 长驻子进程)
├── wecom  (企业微信长连接, 单聊文本)   ├── acp    (任意 ACP agent, 配 acp_command)
└── feishu (飞书私聊/群/云文档评论)     ├── codex  (codex exec --json)
                                      └── gemini (gemini -p -o stream-json)
        ↕                              ↕
              core: 调度 / 鉴权 / 会话路由 (store 持久化) / 权限审批闭环
                    任务控制(/stop/批处理/看门狗/排队持久化) / /cron 调度
                    会话白名单 / 统一 resume
```

**平台能力边界**：三平台体验并不对等——卡片交互（流式卡/审批按钮卡/命令卡/表单卡）**仅飞书**支持，wecom 与 ilink 自动降级为纯文本 + y/n 审批；**wecom** 为单聊文本通道，暂不支持群聊与媒体发送（`/img` `/file` 会明确报错而非谎报成功）；**ilink** 仅私聊可靠工作（普通微信群基本不可用，见 RESEARCH.md），整体标记为实验性。

三层 + 双抽象：core 持有 `Platform` 与 `Backend` trait，平台与后端各自独立可换。session 生命周期提到 core（store 持久化），Backend 退化为无状态执行器——比把 session 塞进 Backend 内存更干净，支持重启续接。

## 设计取舍

imagent 的几个关键取舍（解释「为什么这么设计」，而非与某个项目比高低）：

- **发送者白名单是硬约束，不是可选**：iLink bot 任何人都能加好友，没有白名单 = 任意人都能驱动你的 agent 执行命令。
- **session 持久化到 SQLite**：进程重启可续（`--resume`），崩溃不丢上下文；排队消息同样落库（schema v12）——重启重放。SQLite 经 `rusqlite` 的 `bundled` feature **静态链接进二进制**，运行时无需宿主安装 SQLite。
- **IM 内权限审批闭环**（核心特性）：危险工具（如 `Bash`）执行前，先在 IM 向你 approve/deny——把 agent 的执行权关进用户审批的笼子。
- **自动压缩可选、默认关闭**：上下文水位（input + cache_read）达「模型窗口 × 比例」才压缩；v1.27.0 起默认不启用（网关 token 口径与 CLI 估算存在偏差，误压缩丢上下文细节的代价高于收益），显式声明 `model_context_window_tokens`（比例档）或 `auto_compact_threshold_tokens`（绝对值档）开启；200k 窗口的 Claude 系模型声明 200000 即可，双档并存。
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
# permission_mode = "auto"  # 缺省=auto：claude-cli=透传 claude 原生 auto 模式(分类器自动放行安全操作,高危进 IM)+审批闭环；其余后端=off；显式 ask=每个提示都进 IM
# backend_permission_mode = "auto"  # 后端原生权限模式透传(claude→--permission-mode，覆盖 auto 档缺省)：default|acceptEdits|plan|auto|dontAsk|bypassPermissions；codex/gemini 暂不支持(warn 忽略)
# approval_tools = ["Bash", "WebFetch", "mcp__*"]  # 审批集：ask 模式下只有这些工具过 IM 审批，其余直接放行；空=全部过审
# agent = "claude-cli"   # 换 agent 后端：claude-cli(默认) | claude-acp(Claude ACP 长驻子进程) | acp(任意 ACP agent，配 acp_command) | codex | gemini
# acp_command = "opencode-acp"  # agent="acp" 必填：启动目标 ACP agent 的命令（如 "opencode-acp" / "gemini --experimental-acp"，以目标 agent 的 ACP 接入文档为准；claude-acp 下可选覆盖默认命令，优先级 acp_command > 环境变量 IMAGENT_ACP_COMMAND > claude-agent-acp）
# allowed_chats = ["feishu:oc_xxx"]  # 会话(群)白名单：群消息 chat 放行 OR sender 放行（/chat 可动态管理）
# ask_via_im_conv = "feishu:ou_xxx"  # 终端 agent 的 ask_via_im 提问投递会话（配了才启用，见「终端 agent 接入」）
# agent_timeout_secs = 3600          # 单次运行总超时(秒)；默认 1 小时，0=关闭(防挂死全靠空闲看门狗)
# agent_idle_timeout_secs = 1200     # 空闲看门狗：连续无输出 N 秒自动终止；默认 20 分钟（0=关；/timeout 可按会话覆盖）
# batch_window_ms = 1500             # 连发消息合并为一轮 prompt 的窗口（0=关）
# cot_detail = "brief"               # 工具过程展示 off / brief / detailed（/config 可热改；/config cot 为 per-conv 覆盖）
# quiet_hours = "22:00-08:00"        # 免打扰时段(本地时区,可跨天)：时段内加急(buzz)提醒降级普通消息，内容不变；不设=不启用
# feishu_thread_active_window_secs = 1800  # 话题群免@窗口(秒)：话题内近期有消息则豁免群消息须@bot；默认30分钟，0=关闭
# feishu_bitable_app_token = "bascnXXX"    # 多维表格数据面（仅 feishu + claude-cli）：app_token + table_id 齐备才启用，
# feishu_bitable_table_id  = "tblXXX"      #   agent 获得 bitable_list_fields/bitable_append_row 工具可向该表写行（建议专用表）
# platform = "feishu"                # wecom/feishu 经 config 凭据接入（见下）

# ===== 事件入站（v1.20 webhook；v1.21 防护套件 + GitHub 原生事件；v1.24 防重放）=====
# webhook_addr = "127.0.0.1:18443"   # POST /hook/<token>；非 loopback 绑定且任一条目未配 secret → 拒绝启动（fail-closed）
# [[webhook]]                         # token → 会话（须 /chat allow 放行才会驱动 agent）
# token = "0123456789abcdef0123456789abcdef"
# conv  = "feishu:oc_xxx"
# name  = "ci"                        # 注入消息带【ci】来源前缀
# secret = "github-webhook-secret"    # 可选 HMAC-SHA256 验签（GitHub webhook secret 同款：
#                                     #   X-Hub-Signature-256: sha256=<hex>；公网/隧道部署强烈建议）
# rps = 10                            # 可选限速（请求/秒，缺省 10；0 = 不限）
# replay_window_secs = 300            # v1.24 防重放·第 2 层（opt-in，须配 secret）：请求须带
#                                     #   X-Imagent-Timestamp: <unix秒>，验签串改为 "{ts}.{body}"，
#                                     #   |now-ts| > 窗口 → 401。自建发送端建议开启，示例：
#                                     #   ts=$(date +%s); body='{"text":"CI failed"}'
#                                     #   sig=$(printf '%s.%s' "$ts" "$body" | openssl dgst -sha256 -hmac "$SECRET" | sed 's/^.* //')
#                                     #   curl -H "X-Imagent-Timestamp: $ts" -H "X-Hub-Signature-256: sha256=$sig" \
#                                     #        -d "$body" http://127.0.0.1:18443/hook/<token>
#                                     #   GitHub 原生签名不含时间戳、无法配合本协议——公网 + GitHub 场景
#                                     #   靠第 1 层去重 + HTTPS。两层防重放：① 验签通过后按 (token,签名)
#                                     #   去重（默认启用，10 分钟内同签名重发 409——重放字节必相同即被拦；
#                                     #   代价：合法的同字节重发也被拦，等 10 分钟后重发即可）；② 本协议。
#                                     #   ① 的去重记录同时持久化到 SQLite（配置 replay_window_secs 的条目，
#                                     #   重启不失效）。
# feishu_urgent_on_ask = true         # 审批/问题卡到达即应用内加急弹通知（缺省开；免打扰时段自动跳过）
# GitHub 原生事件：带 X-GitHub-Event 头的请求自动解析为可读摘要注入
#（workflow_run 终态/push/issues/评论/PR/ping；未识别事件确认但不注入）；
# 其它来源请 POST JSON {"text":"..."} 或纯文本。
# cron_catchup = "one"                # /cron 停机补跑：one(缺省)|off(陈旧跳过)|all(逐周期补跑,上限3)

# ===== 用量护栏（v1.19 比例档）=====
# 自动压缩：**默认关闭**（v1.27.0 起——不自动压缩、不主动提醒；需要者显式开启）。
# 开启比例档：上下文水位达 模型窗口 × ratio 触发（摘要+重置+下轮注入【前情摘要】）。
# model_context_window_tokens = 1000000   # 模型上下文窗口（设 >0 即开启比例档；200k 窗口的 Claude 系模型写 200000；缺省 0 = 关闭）
# auto_compact_window_ratio = 0.8         # 触发比例（缺省 0.8；两项均支持 SIGHUP 热改）
# auto_compact_threshold_tokens = 120000  # 绝对值档：仅窗口设 0 且本值 >0 时生效
EOF
```

> **`allowed_tools` 要不要写？** 不必填——**缺省即全部工具**（`["*"]` 语义：不附加 claude 的 `--allowedTools`，CLI 自身默认全量；codex 收敛到 `workspace-write`、gemini 收敛到 `auto_edit`，均不进各自最高危档）。要收敛 agent 的能力边界就显式列白名单：清单外的工具 agent 根本用不了。注意**全量 ≠ 免审**——缺省 `permission_mode = "auto"`（claude-cli 即透传 Claude Code 2026 新出的 **auto 权限模式**：独立分类器逐动作审查，安全操作自动放行，只有高危动作——`curl|bash`、外发敏感数据、强推、`git reset --hard` 等——拦下经 IM 审批）下，危险操作执行前仍会在 IM 向你审批；显式写 `[]` 与 `["*"]` 同义（不限制）。嫌全审太吵？配 **`approval_tools` 审批集**（如 `["Bash", "mcp__*"]`）：只有清单内工具过 IM 审批，其余权限请求直接放行（支持尾部 `*` 前缀匹配；空 = 全部过审）。

> **飞书**：`platform = "feishu"` + `feishu_app_id` + 环境变量 `IMAGENT_FEISHU_APP_SECRET`——完整开通步骤见[接入飞书](#接入飞书完整流程)。**WeCom**：`wecom_bot_id` + `wecom_secret`。两者都免公网（长连接收，HTTP 发）。

> **接入任意 ACP agent**：ACP（Agent Client Protocol）是协议不是单 agent——`agent = "acp"` + `acp_command = "<启动命令>"` 即可把任何说 ACP 协议的 agent 接进来（opencode / gemini-cli / cursor 等均有（或有）ACP 适配器），例如 `acp_command = "opencode-acp"` 或 `acp_command = "gemini --experimental-acp"`（具体命令以目标 agent 的 ACP 接入文档为准）。IM 审批闭环（`permission_mode = "ask"`：危险操作发审批卡等你 y/n）随协议自动覆盖这些后端。注意两点：`allowed_tools` 逐工具白名单在 ACP 系后端不生效（协议无等价机制，见 [SECURITY.md](SECURITY.md) 已知限制）；泛化装配无 Claude 本机会话存储概念，`/resume` 退化为纯 IM 历史、`/export` 不可用。

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
| `im.message.recalled_v1` | 消息撤回：移出未处理的排队消息（任务已开始则提示可 /stop，不自动中断） | 可选 |
| `im.chat.member.bot.deleted_v1` | bot 被移出群：自动从会话白名单移除并私聊通知管理员 | 可选 |
| `im.chat.member.bot.added_v1` | bot 被加入群：回欢迎语（含 /chat allow 放行指引） | 可选 |
| `im.message.reaction.created_v1` | 表情回应快速审批：审批卡上回 👍=允许 / 👎=拒绝 | 可选 |
| `application.url.menu_v6` | 自定义菜单跳转：点击菜单即回 /help 使用说明（后台可配自定义菜单） | 可选 |

> **自定义菜单（可选）**：订阅 `application.url.menu_v6` 后，可在飞书后台「应用能力 → 机器人 → 自定义菜单」配置菜单项——点击菜单 bot 会直接回 `/help` 使用说明（新手引导入口）。

**③ 开通权限**：「权限管理」开通并**发布**：

- `im:message`（读取与发送单聊、群聊消息）——必须。合并转发聊天记录转录（自动拉子消息转成文本给 agent 阅读）与群聊上下文注入（拉本群最近 N 条消息作前置上下文，`feishu_group_context_messages`）**复用同一读权限**（分别调「查询合并转发消息列表」「获取会话历史消息」接口，拉不到自动跳过不阻塞轮次）——真机确认：若拉取报权限错误，需在后台补开对应的 im:message 读权限并发布版本；
- `im:message.group_at_msg`（仅收 @机器人 的群消息；要全收群消息改用 `group_msg` 并把 config 的 `feishu_require_mention_in_group` 设为 `false`）；
- `cardkit:card:write`（CardKit 流式卡片）——可选，缺省自动降级整卡刷新；
- `drive:comment`（云文档评论）——可选，配合上表评论事件；
- `bitable:app`（多维表格读写）——可选，仅启用[多维表格数据面](#多维表格数据面agent-的结构化产出)时需要。

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

## 多维表格（Bitable）数据面：agent 的结构化产出

任务清单、巡检结果、成本台账这类**结构化**产出，此前只能刷屏卡片或落文件。开启 Bitable 数据面后，agent（claude-cli）直接把结果写进飞书多维表格——与 `/cron` 定时巡检天然咬合（每轮结果一行，表格即台账）；Slack 生态没有等价物。

```toml
# ~/.imagent/config.toml（platform = "feishu" 且 agent = "claude-cli" 时生效）
feishu_bitable_app_token = "bascnXXX"   # 多维表格 URL .../base/<这段>
feishu_bitable_table_id  = "tblXXX"     # 同 URL .../table/<这段>
```

两项**齐备才启用**（缺一整组不生效，启动时 warn 提示）；SIGHUP 热改生效。启用后 agent 的 MCP 工具面多出两个工具（复用 `imagent mcp` 审批 server 挂载，不新起子进程）：

- `bitable_list_fields`——列出该表全部列（列名 + 类型），写行前先调它对齐列名；
- `bitable_append_row`——追加一行（`fields` 键=列名、值=标量），返回 record_id。列名写错会被飞书拒绝，错误信息含原因（agent 可自行修正重试）。

**安全注意**：

- 启用 = agent 可向该表**追加任意行**（MVP 无删改面）。**建议用专用表**（如 `imagent-巡检台账`），不要指向生产台账；
- 写入需过 IM 审批的用户，把 `mcp__imagent__bitable_append_row` 加进 `approval_tools`（ask 类档位下该工具每次写入都会进审批卡）；
- 应用需开通 `bitable:app` 读写权限（见[接入飞书](#接入飞书完整流程)③）；
- 仅 claude-cli 后端支持（ACP 的 MCP 配置不经 write_mcp_config）；仅 `platform = "feishu"` 生效，其它平台配置了会 warn 并忽略。

链路：`claude（MCP stdio）→ imagent mcp server（bitable_* 工具）→ permission socket（kind=bitable）→ core BitableApi → 飞书 OpenAPI（走既有 429 退避）`。


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
> v1.19.0 起：服务定义文件 0600（内嵌 secret 不再按 umask 可读）、load/enable
> 失败如实报错、异常退出码非 0（systemd `Restart=on-failure` 真正生效）。

```bash
imagent service status     # 运行状态
imagent service uninstall  # 停止并卸载
imagent service print      # 预览将写入的服务定义（--format launchd|systemd、--exe/--platform/--log-path 覆盖；不安装、不写文件、不需 root）
```

> `deploy/` 下的静态 launchd/systemd 模板即 `service print` 的输出（单一事实源，
> 勿手改；刷新方法见 `deploy/README.md` 顶部）。

| | macOS（launchd 用户代理） | Linux（systemd 用户单元） |
|---|---|---|
| 服务名 | `com.imagent[.<profile>]` | `imagent[-<profile>]` |
| 定义 | `~/Library/LaunchAgents/*.plist` | `~/.config/systemd/user/*.service` |
| 日志 | `~/.imagent/logs/daemon.log` | `journalctl --user -u imagent -f` |
| 无人登录也运行 | 天然支持（登录即启） | 需一次 `loginctl enable-linger $USER`（服务器场景） |

secret 轮换 / 环境变量变化后：重新 `export` + `imagent service install`（先卸旧再装新，等效更新）。多实例：`imagent --profile work service install` → 独立服务与状态目录。

**日志轮转**：macOS 守护日志 `~/.imagent/logs/daemon.log` 由进程内置 size 轮转——超过 50MB 触发（copytruncate，保留最近 5 份）；阈值用 `IMAGENT_LOG_MAX_MB` 调整（单位 MB，`0` = 不限，改后 `export` + 重装服务生效）。Linux 日志走 journal，轮转/限额由 journald 自身配置（如 `SystemMaxUse=`）管理。

## 备份与恢复（imagent backup）

升级 / 换机 / 误删 profile 前，一条命令拿到一致性快照：

```bash
imagent backup                    # 默认 <imagent_home>/backups/（profile 感知）
imagent backup --out /mnt/nas/    # 自定义输出根目录
imagent --profile work backup     # 备份 work profile（落 profile 目录下 backups/）
```

每份备份是单个目录（`imagent-backup-<profile>-<UTC时间戳>/`），内含三件：

| 产物 | 内容 |
|---|---|
| `imagent.db.snapshot` | SQLite `VACUUM INTO` 一致性快照（全部会话/白名单/凭据密文/审计/定时任务等） |
| `config.toml` | 配置副本（备份时无 config 则跳过，MANIFEST 如实记录） |
| `MANIFEST.txt` | imagent 版本、schema 版本、各文件 sha256、恢复步骤与限制说明 |

要点：

- **运行中可备份**（不持实例锁、无需先停机）：`VACUUM INTO` 对运行中的 WAL 库生成读一致快照，包含备份开始前已提交的全部事务（含 WAL 中尚未 checkpoint 的已提交数据）；快照开始后新提交的数据不在内——不停机备份的固有语义，恢复后以快照点为准继续。要拿到「绝对完整」的停机点快照，先停服务再 backup 也完全可以。
- **keyring 凭据不在快照内**：iLink 登录态存在 OS 钥匙串（机器绑定），换机/重装后需重录——iLink 重新 `imagent login` 扫码；WeCom 重配 `wecom_secret`；飞书重设 `IMAGENT_FEISHU_APP_SECRET`。
- **媒体目录不进快照**（`<imagent_home>/media` 为入站缓存，体积不可控且可重新获取）。
- **保留策略**：备份根目录自动保留最近 10 份，超出清最旧（按目录名排序）。
- 备份产物统一 0600、目录 0700（快照含 credentials 表，config 副本可能含 secret）。

**恢复步骤**（README 与每份 MANIFEST.txt 内同款）：

```bash
# 1. 停止 imagent（start 终端 Ctrl-C / `imagent service uninstall` / kill）
# 2. 用快照替换主库（旧库的 -wal/-shm 残留一并删掉，避免旧 WAL 混入新库）
cp imagent-backup-default-YYYYMMDD-HHMMSS/imagent.db.snapshot ~/.imagent/imagent.db
rm -f ~/.imagent/imagent.db-wal ~/.imagent/imagent.db-shm
# 3. 如有 config 副本，一并放回
cp imagent-backup-default-YYYYMMDD-HHMMSS/config.toml ~/.imagent/config.toml
# 4. 重录凭据（见上：login / wecom_secret / 飞书 secret）
# 5. 重新启动——schema 迁移自动向前
imagent start
```

> **降级不可恢复**：恢复只能「旧快照 + 相同或更新的 imagent 二进制」（启动时 schema 自动向前迁移）；**schema 更新的快照不能被旧版 imagent 打开**（启动会拒绝比代码新的 `user_version`）。升级出问题想回退二进制时，请用升级**前**生成的备份，不要用升级后新产生的备份配旧二进制。

## 命令（IM 内）

| 命令 | 作用 |
|---|---|
| `/new` | 重置会话（开新上下文） |
| `/switch <name>` | 切到 / 新建命名会话（多任务并行上下文） |
| `/sessions` | 列命名会话（`*` 标当前） |
| `/resume [n]` | 统一恢复列表：📱 IM 会话 ∪ 💻 电脑端 Claude Code 会话（摘要+时间辨认，按序号接管，无需会话 id） |
| `/export [n]` | 导出当前（或 /resume 序号）会话为 Markdown 文件（claude 系后端） |
| `/again` | 再跑最近一次成功指令（与 /retry 的失败轮重试对称） |
| `/compact` | 软压缩上下文（摘要 + 重置 + 延续）；自动触发**默认关闭**，开启方式见[用量护栏](#设计取舍)：水位（input+缓存）达 模型窗口 × `auto_compact_window_ratio`（缺省 80%） |
| `/retry` | 重发最近一轮指令（失败/中断后一键续接） |
| `/model [名称\|default]` | 查看/热切模型（切换需管理员；claude 系 / codex `-m` / gemini `-m` 全支持） |
| `/cd [path]` | 切工作目录（`/resume` 本机会话列表随之变化） |
| `/ws list\|save\|use\|remove` | 命名工作空间 |
| `/img <path>` `/file <path>` | 发 workdir 内图片 / 任意文件到 IM |
| `/timeout [N\|off\|default]` | 会话级空闲看门狗（分钟） |
| `/perm <auto\|off\|allow\|deny\|ask>` | 权限模式热切（auto=按后端自动选档） |
| `/perm list` / `/perm revoke <工具>` | 查看/单项撤销本会话「始终允许」清单 |
| `/mcp list · add <名> <url> · rm <名>` | agent 的 MCP servers 热管理（管理员；下一轮生效，重启不丢；与 config `mcp_config_path` 文件源合并） |
| `/stop [all]` | 中断在飞任务（**排队消息保留并自动转入下一轮**——对齐 Claude Code 的 Esc+队列注入语义；`/stop all` 硬停清空排队；任务恰在收尾时如实回「已完成」不谎报中断） |
| `/queue list\|drop <n>` | 查看当前会话排队消息 / 丢弃指定序号（自己的或管理员） |
| `/cron add <分 时 日 月 周> <指令>` | 定时任务（本地时区含 DST；`*`/`*/n`/范围/列表，`/cron add */10 * * * * 检查构建`） |
| `/cron list` `/cron rm <id>` | 列出（含已停用）/ 删除定时任务（限创建者或管理员；每会话上限 20 条） |
| `/config [k v]` | 查看 / 热改配置（cot_detail / batch_window_ms / agent_idle_timeout_secs / require_mention / reply_mode，管理员）；`/config cot <off\|brief\|detailed\|default>` 为**本会话** COT 偏好（白名单可用，免 admin） |
| `/status` `/doctor` `/reconnect` | 运行状态（含上下文水位与阈值距离）/ 自检（平台权限 + 安全维度：凭据明文形态、webhook 暴露面、共享工作区、权限×能力错配、护栏水位、DB/媒体体积）/ 强制平台重连 |
| `/tasks` | 本会话在飞轮次的实时进度（checklist + 工具统计）——纯文本平台/不想翻卡片场景的进度入口 |
| `/allow <id\|@名字>` `/disallow` | 授权 / 撤销 sender（飞书群内可直接 @ 对方，管理员门槛） |
| `/admin [list\|add\|remove]` | 管理员动态管理（首位设立自动带操作者，防自锁；SIGHUP 同步 config 变更） |
| `/chat allow\|deny\|allow-all\|list` | 会话（群）白名单；`allow-all` 批量放行 bot 已加入的全部群 |
| `/list` `/whoami` | 查白名单 / 查自己的 sender 与会话 id |

群消息默认须 `@机器人`（`feishu_require_mention_in_group`，正文 @ 占位自动清洗）；话题群内近期（`feishu_thread_active_window_secs`，默认 30 分钟，0=关闭）有过消息则免 @ 追问。`/config reply_mode text` 可切纯文本回复（无卡片权限或偏好简洁时）。群 conv 的回复与流式卡会**引用发起消息**（reply API）并标注发起者；加急提醒（审批过半催办、长任务完成通知）受 `quiet_hours = "22:00-08:00"`（本地时区，可跨天）约束——时段内降级为普通消息（内容不变）。

## 权限审批闭环（杀手锏）

`permission_mode = "ask"` + `allowed_tools = ["Read","Edit","Bash"]` 时，agent 调 `Bash` 前会在 IM 询问：

```
🔐 Claude 请求执行 Bash({"command":"..."})
回复 y 允许，其它拒绝。
```

回复 `y` → 执行；其它 → 拒绝。基于 Claude Code 的 `--permission-prompt-tool` MCP 回调实现。**飞书**下询问是「✅ 允许 / ⛔ 拒绝 / 始终允许」按钮卡片——点一下即回，无需打字；「始终允许」记入会话级 allow-set（同工具不再问，`/stop all` 或 `/new` 清空）。等审批期间 `/stop` 仍可用（自动回 deny 中止）。超长输出的终态卡自动截断（头尾窗）并**补发全文文本**——结论不因卡片大小限制丢失。

审批粒度（claude-cli，由松到紧叠加）：`permission_mode = "auto"`（缺省：透传 Claude Code 原生 **auto 模式** `--permission-mode auto`——分类器自动放行安全操作，高危提示进 IM）→ `backend_permission_mode` 改透传值（`default`/`acceptEdits`/`plan`/`auto`/`dontAsk`/`bypassPermissions`）→ `ask`（claude 的每个权限提示都进 IM）→ `approval_tools` 审批集（清单外提示直接放行，可与任意档叠加）。`/perm` 可热切（Ask 闭环类需重启生效）；`backend_permission_mode` 支持 SIGHUP 热重载。群聊多人协作时消息带【名字】归属标注（飞书经 contact API 懒解析展示名，需 `contact:user.base:readonly` 权限——缺权限回退 open_id 短版）；审批等待期流式卡显示「⏳ 等待审批中」并持续心跳（不再冻结成卡死假象）。旧版 claude CLI（<2.1.228）不认 auto 会静默回退 default（≈全量进 IM，降级安全）。

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
- 同一 MCP server 还暴露 `notify_via_im(message, source?)`：向该会话发一条**单向通知**后立即返回（不等回复、不占审批槽）——适合长任务跑完「叫一声」、阶段性进度汇报；需要用户回答/决策时仍用 `ask_via_im`。

## 安全

- **白名单鉴权**：sender 白名单 + 会话（群）白名单，非授权丢弃（iLink bot 任何人可加好友，这步不可省）。
- **工具收敛**：`allowed_tools` 可选（缺省 = 全部工具，`[]`/`["*"]` 同义不限制；显式清单 = 白名单）；workdir 用 `current_dir` 锁定，危险操作靠 `permission_mode = "ask"` IM 审批兜底。
- **状态目录隔离**：`hide_state_dir_from_agent`（默认开，仅 claude-cli）每轮注入 `--settings` deny 规则把 `~/.imagent`（含 profile）挡在 agent 读视野外，防提示注入后直读 `imagent.db`；Bash/子进程间接读取仍属同 uid 进程边界（见 [SECURITY.md 威胁模型](SECURITY.md#威胁模型与边界必读)）。
- **权限审批**：危险操作 IM approve/deny（文本 / 按钮卡片）；卡片 markdown 层 `<at>` 注入面全路径转义（bot 不可被借以 @ 任意租户用户）。
- **store 加固**：文件 0600 / 目录 0700；CDN 下载 SSRF 白名单；服务定义（内嵌 secret）0600。
- **威胁模型**：审批闭环约束「agent 发起的工具调用」，不约束「同 uid 进程的读写能力」——agent 子进程与 imagent 同用户运行，提示注入场景下仍有残留攻击面。**多人群白名单部署前务必读 [SECURITY.md「威胁模型与边界」](SECURITY.md#威胁模型与边界必读)**。
- 详见 [`SECURITY.md`](SECURITY.md)。

## 路线

| 阶段 | 状态 | 交付 |
|---|---|---|
| P0 | ✅ | 调研（iLink 协议/合规、Claude CLI/ACP、竞品） |
| P1 | ✅ | MVP 闭环：扫码 → 私聊 → `claude -p` → 回传 → `--resume` |
| P2 | ✅ | 限流熔断 / 动态白名单 / 多命名会话 / 软 compact / 推流 / typing / **权限审批** / 媒体 |
| P3 | ✅ | 开源化（MIT/CI/凭据加密/mdBook）+ WeCom + ACP + 多 agent（Codex/Gemini）+ 运维（指标/热重载/daemon）+ 长消息分片 |
| P4 | ✅ | 任务控制（`/stop`/批处理/看门狗）+ 飞书平台（CardKit 流式卡/审批按钮/云文档评论）+ 会话白名单 + COT 三档 `/config` + IM 诊断命令 + 统一 `/resume` + Profile 多实例 |
| P6 | ✅ | mention 基础设施 + 命令交互卡片 + 话题群隔离 + `setup`/`service` 自管理 + 出站文件 + `/cd` 校验 + 会话级 `/timeout` |
| P7 | ✅ | `/admin` 动态管理 + `/chat allow-all` + 陌生人提示开关 + `/config reply_mode` + `profile export/import` |
| v1.8–v1.10 | ✅ | 四轮深度 code review（60+ 项修复：env 消毒/超时纪律/审批 fail-closed/转发代批防护）；AUQ 自由输入；steering；上下文水位含缓存 |
| v1.18 | ✅ | `/cron` 定时任务（头牌）+ 群媒体「回复即定向」+ 转向回执上卡 |
| v1.20 | ✅ | **Webhook 入站**（事件驱动：CI/告警→会话→审批）+ ACP 窗口自学习 + 崩溃轮次恢复（/retry 续跑）+ compact 卡片化 + /cron 停机补跑 |
| v1.19 | ✅ | 深度 review 双批修复（42 项）+ 事件 intake 与媒体 IO 解耦 + **排队消息持久化（崩溃不丢）** + update_card 状态机化 / ConvState 收敛 + **自动压缩比例档（窗口 80%）** + housekeeping（媒体 GC） |

> **当前状态**：以 [CHANGELOG.md](CHANGELOG.md) 最新版本为准（历史复审记录见 `docs/CODE_REVIEW_*.md`，最新一轮 [`docs/CODE_REVIEW_v13.md`](docs/CODE_REVIEW_v13.md)）。质量基线：`cargo test --workspace` 全绿、clippy 零警告、CI 双平台 + audit/deny。

详见 [`docs/`](docs/)（[ARCHITECTURE](docs/ARCHITECTURE.md) / [DESIGN](docs/DESIGN.md) / [FEISHU_DESIGN](docs/FEISHU_DESIGN.md) / [RESEARCH](docs/RESEARCH.md) / [CODE_REVIEW_v10](docs/CODE_REVIEW_v10.md)）。

## 开发

```bash
cargo test --workspace                              # 全通过（详情见 CI）
cargo clippy --workspace --all-targets -- -D warnings   # 0 warning
cargo fmt --all --check
```

crate：`core`（调度/鉴权/session/权限/任务控制/cron）+ `ilink`（iLink 协议）+ `wecom`（企业微信长连接）+ `feishu`（飞书长连接 + CardKit + 云文档评论）+ `claude`（CLI/ACP backend）+ `codex` + `gemini` + `store`（SQLite，schema v16 线性迁移）。

## License

MIT（见 [`LICENSE`](LICENSE)）。iLink 协议出处：腾讯官方 [`@tencent-weixin/openclaw-weixin`](https://github.com/Tencent/openclaw-weixin) / [ClawBot 文档](https://developers.weixin.qq.com/doc/aispeech/knowledge/openapi/Clawbotrelated.html)。
