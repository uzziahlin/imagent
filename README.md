# imagent

> **Instant messaging, meet your agent.** — 把自主 agent 关进你批准的笼子，再把笼子装进口袋。

任何 IM（个人微信 iLink / 企业微信 WeCom / 飞书）+ **Web 驾驶舱**（手机 PWA / 桌面浏览器）+ 本地终端 ↔ 任何 agent（Claude Code / Codex / Gemini / 任意 ACP）。Rust 单二进制。

![Rust](https://img.shields.io/badge/Rust-edition%202021-orange) ![License: MIT](https://img.shields.io/badge/License-MIT-blue) ![CI](https://github.com/uzziahlin/imagent/actions/workflows/ci.yml/badge.svg) ![GitHub release](https://img.shields.io/github/v/release/uzziahlin/imagent) ![Docs](https://img.shields.io/badge/docs-mdBook-blueviolet)

> 🌐 **English TL;DR** — A Rust gateway that puts autonomous agents behind your approval: any IM (WeChat **iLink** / **WeCom** / **Feishu**), a browser **cockpit** (mobile PWA / desktop — self-hosted relay, E2E-encrypted, zero inbound ports), or your local terminal drives **Claude Code / Codex / Gemini**. Dangerous tool calls require your explicit y/n — a tap on an approval card in IM or web — with an OS-level sandbox underneath.
> **Unofficial — not affiliated with Tencent or Anthropic.** iLink is a third-party Rust re-implementation of Tencent's OpenClaw Weixin protocol; the docs below are in Chinese (the project targets the WeChat ecosystem).

## 用起来长这样

```
你（手机飞书）    把 ~/data/results.csv 画成趋势图，发到「数据组」群

🤖 agent          需要执行 Bash(python ~/plot.py ~/data/results.csv)
                  🔐 权限审批卡 →  [✅ 允许]  [⛔ 拒绝]  [始终允许]

你                （手机上点「允许」）

🤖 agent          ✅ 已完成 → 📊 chart.png 已发到「数据组」群
                  ⚙️ Bash · 1.2s    📎 Read ~/plot.py → Write ~/out/chart.png
```

三个杀手锏：

- 🔐 **审批闭环** — agent 每个危险操作（`Bash` / 外发 / 强推…）先向你请示：飞书按钮卡片、Web 审批卡、或文本 y/n；「始终允许」记住你的选择。OS 级沙箱（macOS Seatbelt / Linux bwrap）在内核层兜底。
- 🖥️ **Web 驾驶舱** — 手机 PWA / 桌面浏览器远程驾驶**多台 Mac** 上的 agent：markdown 流式渲染、审批、`/stop`、系统推送、语音转写。自建中继：Mac **零入站端口**、业务帧**端到端加密**、relay 只存公钥不碰私钥。
- 🌉 **5 入口 × 6 引擎** — 微信 / 企微 / 飞书 / Web / 终端 ↔ Claude Code / Codex(app) / Gemini / 任意 ACP。`Platform` / `Backend` 双 trait 解耦，多平台同进程并存（`platforms = ["feishu", "web"]`），会话级 `/backend` 热切引擎。

## ⚠️ 免责声明

imagent 是**非官方**第三方开源项目，**不隶属于腾讯或 Anthropic**。iLink 接入基于腾讯官方 [OpenClaw Weixin channel 协议](https://github.com/Tencent/openclaw-weixin)的第三方实现（非逆向方案）；使用可能违反微信/腾讯服务条款，**账号风险自负**（建议绑定小号）。仅做**服从式退避**（被限流就退避等待），**绝不实现**绕过频率/风控的功能。仅供学习研究。详见 [`docs/RESEARCH.md`](docs/RESEARCH.md) §2。

## 快速开始

### ① 安装

**一键脚本（推荐）**：装二进制（含 sha256 校验）→ 首次生成 config → 自动挂载 MCP。已有 config 绝不覆盖，可重复运行：

```bash
bash <(curl -fsSL https://raw.githubusercontent.com/uzziahlin/imagent/main/install.sh)
```

**预编译二进制**（每个 release 附 `sha256`）：从 [Releases](https://github.com/uzziahlin/imagent/releases) 下载 `imagent-darwin-arm64` / `imagent-darwin-x86_64` / `imagent-linux-x86_64`，`chmod +x` 后放到 `PATH`。

**源码构建**（Rust 1.88+；飞书平台需额外 `protoc`：`brew install protobuf`）：

```bash
git clone https://github.com/uzziahlin/imagent && cd imagent
cargo build --release    # 二进制：target/release/imagent
```

### ② 最小配置（飞书 · 5 分钟）

```bash
mkdir -p ~/.imagent
cat > ~/.imagent/config.toml <<'EOF'
default_workdir = "/absolute/path/to/agent/workspace"   # 必填：agent 的工作目录
platform = "feishu"
feishu_app_id = "cli_xxx"     # 开放平台「凭证与基础信息」页拿
allowed_senders = []          # 空 = 发现模式：首启日志里拿你的 open_id
EOF
export IMAGENT_FEISHU_APP_SECRET="你的 App Secret"   # secret 不落 config
```

> 飞书应用的创建 / 事件订阅 / 权限开通 / 发布，见下文[飞书（完整流程）](#飞书完整流程)四步表。其它平台（WeCom / iLink / Web）见[接入平台](#接入平台)。

### ③ 跑起来

```bash
imagent start                # 前台常驻；日志看到 connected to wss://msg-frontier.feishu.cn 即成功
# 在飞书里给 bot 发一条消息 → 日志打出你的 ou_xxx open_id →
imagent allow ou_xxx         # 授权自己
# 再发消息：agent 执行 → 结果流式回传。权限审批见上方示意图。
```

> ⚠️ **macOS 撞名**：`imagent` 也是 macOS 系统输入法进程。**不要 `pkill imagent`**——会杀掉系统输入法。停止本程序用前台 `Ctrl-C` 或全路径 `kill $(pgrep -f /usr/local/bin/imagent)`。

更多命令（`/switch` `/cron` `/model` …）见 [docs/COMMANDS.md](docs/COMMANDS.md)；逐键配置参考见 [docs/CONFIG.md](docs/CONFIG.md)。

## 你属于哪种场景？

| 你想… | 去哪 |
|---|---|
| 在飞书 / 微信 / 企微里使唤 agent，审批全走手机 | [接入平台](#接入平台) |
| 浏览器/手机 PWA 远程驾驶电脑上的 agent | [Web 驾驶舱](#web-驾驶舱) |
| CI 失败、监控告警自动触发 agent 排障 | [Webhook 事件入站](#webhook-事件入站) |
| 定时巡检 / 日报，agent 每天自己跑 | `/cron`（[命令参考](docs/COMMANDS.md#定时任务)） |
| agent 把结构化产出写进飞书多维表格 | [多维表格数据面](#多维表格数据面bitable) |
| 终端上的 agent 有事在手机上问你 | [ask_via_im](#终端-agent-反向接入ask_via_im) |
| 多 agent 协作（Claude 写完叫 Codex 审） | [委托编排](#委托编排delegate) |
| 换 agent 引擎 / 接任意 ACP agent | [agent 与引擎](docs/CONFIG.md#agent-与引擎) |

## 核心特性

- 🔐 **安全闭环**：sender + 会话双白名单、`allowed_tools` 工具收敛、workdir 锁定、**IM 内权限审批闭环**、**OS 级 agent 沙箱**（默认 auto：状态目录与持久化热点内核层 deny，Bash 子进程间接读取同被拦）。
- 🖥️ **Web 驾驶舱**：PWA / 桌面，多机器管理、流式渲染、执行链 diff 视图、审批卡（倒计时）、推送深链、未读徽标、粘贴图、语音转写（whisper 本地）。
- 🌉 **多平台并存**：`platforms = ["feishu","web"]` 一进程多平台，conv 前缀路由，Dispatcher 零改动。
- 🧭 **多引擎会话级切换**：`/backend` 切引擎、`/model` 热切模型、`/effort` 思考深度；codex-app 平级后端（审批闭环 / steering 全量可用）。
- 💬 **会话连续**：per-chat session 落 SQLite（bundled 静态链接），重启可续（`--resume`）；排队消息实时落库——**崩溃不丢**（`kill -9` / 断电后自动重放）；`/resume` 统一接管 IM + 电脑端 Claude Code 会话。
- 🛑 **任务控制**：`/stop` 优雅中断（codex-app turn/interrupt 有界等待，排队消息自动转入下一轮）；连发消息批处理合并一轮；空闲看门狗；`/retry` 一键续接。
- ⏰ **自动化**：`/cron` 定时任务（停机补跑策略）、Webhook 事件入站（HMAC 验签 + 双层防重放）、委托编排 fan-out、Bitable 数据面。
- 📦 **低负担运维**：单二进制、`service install` 一键装 launchd/systemd（崩溃自动拉起）、`imagent backup` 一致性快照、`/doctor` 自检、Profile 多实例。

<details>
<summary><b>全部特性（细节展开）</b></summary>

- 🛡️ **限流熔断**：`sendmessage` 服从式退避（防封号，不绕风控）；iLink 媒体批内 4 并发限流 + 60s 总预算。
- 💭 **thinking / 任务清单**：思考过程与正文分离透出（cot 三档控制）；Task* / ACP Plan 渲染成 checklist 进度卡。
- 📊 **用量护栏**：`/stats` 成本统计 + 自动 compact（比例档：窗口 × 80%，默认关闭显式开启）+ per-sender 成本上限；ACP 自动学习真实窗口。
- 📡 **Webhook 入站**：`POST /hook/<token>` 注入 CI/告警——与手打消息同权走鉴权/审批管线，无旁路。
- 🎨 **媒体收发**：图片 / 文件（iLink 协议强制 AES-128-ECB + CDN；飞书走 OpenAPI；媒体目录 7 天 GC）。
- ⚡ **流式反馈**：工具调用摘要单行 + 状态图标、typing 指示；飞书文本先投递、媒体异步后补（收发解耦）。
- 📄 **飞书生态**：CardKit 真流式卡片、审批/问题/命令按钮卡、`/config` 表单卡、云文档评论 @bot、合并转发转录、群聊上下文注入、表情回应快速审批（👍/👎）、自定义菜单回 /help。
- 🔀 **委托编排**：`[[delegate]]` 副后端 one-shot fan-out，缺省只读三件套、预算只降不升、全局并发闸 4。
- 🎙️ **飞书语音输入**：语音条自动转文字（speech_to_text）。
- 🧩 **Profile 多实例**：一机多 bot 身份（config/db/socket/媒体全隔离）+ `profile export/import` 跨机迁移。
- 🛟 **崩溃轮次恢复**：执行中轮次留痕，重启后 `/retry` 续跑。
- 📁 **命名工作空间**：`/ws` `/cd` 与 Web 驾驶舱同源互通。

</details>

## 架构

```
trait Platform                            trait Backend
├── feishu (飞书私聊/群/云文档评论)         ├── claude (CLI + ACP 长驻子进程)
├── wecom  (企业微信长连接, 单聊文本)       ├── acp    (任意 ACP agent, 配 acp_command)
├── ilink  (个人微信私聊, 实验性)           ├── codex  (codex exec --json)
├── web    (浏览器驾驶舱, ECS relay + E2E)  ├── codex-app (app-server 双工: 审批闭环/steering/model-list)
└── local  (本地终端薄视图, 零配置)         └── gemini (gemini -p -o stream-json)
        ↕                              ↕
              core: 调度 / 鉴权 / 会话路由 (store 持久化) / 权限审批闭环
                    任务控制(/stop 优雅中断/批处理/看门狗/排队持久化) / /cron 调度
                    会话白名单 / 统一 resume / 多引擎 Dispatcher(会话级 /backend)
```

三层 + 双抽象：core 只认 `Platform` / `Backend` 两个 trait，平台与后端各自独立可换、多平台经 MultiPlatform 组合器并存；session 生命周期收到 core（store 持久化），Backend 退化为无状态执行器——支持重启续接。

**平台能力边界**：五平台体验不对等——卡片交互**仅飞书**（wecom/ilink 降级纯文本 + y/n 审批）；**wecom** 单聊文本、暂不支持群聊与媒体发送（明确报错不谎报成功）；**ilink** 仅私聊可靠（实验性，见 RESEARCH.md）；**web** 自带完整卡片交互（SPA 自渲染）；**local** 零配置。

## 接入平台

### 飞书（完整流程）

走**企业自建应用 + 长连接**：免公网 IP / 域名 / 证书。全程约 10 分钟（`imagent setup --platform feishu` 可交互走一遍并校验连通性）：

**① 创建应用**：[open.feishu.cn/app](https://open.feishu.cn/app) →「创建企业自建应用」→「添加应用能力」→ 启用**机器人**。

**② 事件订阅**：「事件与回调」→ 选**使用长连接接收事件**，添加：

| 事件 | 用途 | 必须 |
|---|---|---|
| `im.message.receive_v1` | 收私聊 / 群 @ 消息 | ✅ |
| `card.action.trigger` | 卡片按钮回调（审批 / 问题 / 命令卡） | ✅ |
| `drive.file.comment.created_v1` | 云文档评论 @bot 触发 | 可选 |
| `im.message.recalled_v1` | 撤回：移出排队消息 | 可选 |
| `im.chat.member.bot.deleted_v1` / `added_v1` | bot 进出群：白名单维护 / 欢迎语 | 可选 |
| `im.message.reaction.created_v1` | 表情回应快速审批（👍/👎） | 可选 |
| `application.url.menu_v6` | 自定义菜单点击回 /help | 可选 |

**③ 开通权限并发布**：`im:message`（必须——合并转发转录与群上下文注入复用同一读权限）、`im:message.group_at_msg`（群 @ 消息；全收改 `group_msg`）、可选 `cardkit:card:write`（流式卡，缺省降级整卡刷新）、`drive:comment`、`bitable:app`。

**④ 发布生效**：「版本管理与发布」→ 创建版本并发布——**权限与事件订阅都要发布后才生效**，新手最常漏这步。

**⑤ 配置凭据 + 启动**：见上文[快速开始](#②-最小配置飞书--5-分钟)。

### WeCom / iLink

- **WeCom**：config 填 `wecom_bot_id` + `wecom_secret`，同样免公网（长连接收，HTTP 发）。
- **iLink（个人微信，实验性）**：`imagent login` 扫码 → 私聊 bot。合规与账号风险见文首免责声明，仅私聊可靠工作。

### Web 驾驶舱

浏览器（手机 PWA / 桌面）远程驾驶家里 / 公司 Mac 上的 agent，与 IM 平台同权走鉴权 / 审批 / steering 管线，可同进程并存：

```
浏览器 ×N（PWA / 桌面，多标签并存）
    │ WSS（Caddy 8443 TLS 终结，/imagent/* 反代 relay）
ECS relay（imagent-relay：只做认证 + 转发 + 离线缓冲，不理解业务语义）
    ▲ 网关以客户端模式外连（Mac 零入站端口）
Mac 网关（platforms 含 "web"）→ 驱动 agent
```

**安全分层**（帧协议 v1.1 → v1.4，逐层收口）：

| 层 | 机制 |
|---|---|
| v1.1 | machine 槽位：多台 Mac 并存，每机单连接，同机重连顶替旧连接 |
| v1.2 | token-machine 绑定（泄露 token 只能扮演本机）+ 浏览器会话层（30 天 sessionToken，可吊销） |
| v1.3 | 网关 ed25519 挑战应答（一次性 nonce，**relay 只存公钥**）+ passkey（WebAuthn）登录 |
| v1.4 | E2E 中继盲化：业务帧对 relay 全密文（`enc{sid,box}`） |

**接入**（3 步）：

```toml
# ~/.imagent/config.toml
platforms = ["feishu", "web"]                 # 与既有 IM 平台并存
web_relay_url = "wss://your-ecs:8443/imagent/gateway"
web_machine_id = "home-mac"                   # 缺省取 hostname 短名
```

1. `imagent start`——首启自动生成 ed25519 身份（`~/.imagent/web-gateway-key`，0600），日志打印公钥；
2. 公钥登记到 relay 侧 env `IMAGENT_RELAY_GATEWAY_KEYS`（或回退用双端 token）；
3. Web 消息 sender 恒为 `web:user`，加入 `allowed_senders`。

新 Mac 一键接入：`imagent join --relay wss://… --name work-mac --workdir /path`（幂等，全参数可脚本化）。relay 部署（musl 静态编译 / systemd / Caddy / passkey 开通）见 [docs/WEB_DESIGN.md](docs/WEB_DESIGN.md)。

**语音转写（可选）**：config 配 `whisper_bin` + `whisper_model` 双键（`brew install whisper-cpp`，中文建议 medium 模型起）——SPA 显示 🎤，录音经 `asr_request` 帧到 Mac 本机 `whisper-cli` 转写回填（识别不上云）；未配置时桌面浏览器回退 Web Speech API。已知边界：iOS 主屏（standalone）模式不支持语音识别（入口会提示）；后台切换打断录音需重按。

## 进阶

### 权限审批闭环

`permission_mode = "ask"`（或缺省 auto 档拦下的高危动作）时，agent 调危险工具前向你请示（文本 y/n / 飞书按钮卡 / Web 审批卡），「始终允许」记入会话级 allow-set（`/stop all` 或 `/new` 清空）。**支持闭环的后端**：claude-cli（control 双工通道）、claude-acp（ACP permission 协议）、codex-app（app-server FullLoop）。

审批粒度（claude-cli，由松到紧叠加）：缺省 `auto`（透传 Claude Code 原生 auto 模式——分类器自动放行安全操作，高危进 IM）→ `backend_permission_mode` 改透传值 → `ask`（每个提示都进 IM）→ `approval_tools` 审批集（清单外直接放行）。`/perm` 可热切。等审批期间 `/stop` 仍可用（自动回 deny 中止）。

### /cron 定时任务

`/cron add */10 * * * * 检查构建`——5 字段 cron（本地时区含 DST），到期消息走与手打完全相同的鉴权/审批管线；停机补跑 `cron_catchup = one|off|all`。与 [Bitable 数据面](#多维表格数据面bitable)咬合可做每日巡检台账。

### Webhook 事件入站

`POST /hook/<token>` 把 CI 失败、监控告警注入指定会话——与手打消息同权走鉴权/审批，无旁路；token 路径鉴权 + HMAC 验签 + 双层防重放 + 会话白名单。GitHub 原生事件自动解析为可读摘要。逐字段配置见 [docs/CONFIG.md](docs/CONFIG.md#定时与事件入站)。

### 委托编排（delegate）

`[[delegate]]` 把命名副后端（codex / gemini / claude-cli）配成「子任务执行器」：agent 经 `delegate_run` 工具自行 fan-out（Claude 写完自动叫 Codex 审），或用户 `/delegate <name> <子任务>` 显式派活。副轮 one-shot、缺省只读（Read/Grep/Glob）、预算只能调低、全局并发闸 4、结果带【委托·name】前缀不占主会话上下文。TOML 见 [docs/CONFIG.md](docs/CONFIG.md#委托编排)。

### 多维表格数据面（Bitable）

config 配 `feishu_bitable_app_token` + `feishu_bitable_table_id`（双键齐备才启用，建议专用表）后，agent（claude-cli）把任务清单 / 巡检结果 / 成本台账等结构化产出直接写进飞书多维表格——与 `/cron` 天然咬合（每轮结果一行，表格即台账）。TOML 与安全注意见 [docs/CONFIG.md](docs/CONFIG.md#多维表格数据面bitable)。

### 终端 agent 反向接入（ask_via_im）

反向场景：电脑终端上跑的**任意 agent**（Claude Code / ZCode / Codex…）需要你决策时，把问题转发到飞书——手机上点按钮作答，答案回到终端。适合挂长任务离开工位。

```
终端 agent ──MCP(stdio)──► imagent mcp-ask ──unix socket──► imagent 主进程
                                                                │ 飞书问题卡（选项按钮）
终端 agent ◄──用户回复原文────────────────────────────────────────┘
```

```bash
# 主进程 config 配 ask_via_im_conv = "feishu:ou_xxx" 后，把 MCP 挂到终端 agent：
imagent mcp-ask --print-config
# {"mcpServers":{"imagent":{"command":"/usr/local/bin/imagent","args":["mcp-ask"]}}}
# Claude Code：claude mcp add imagent -- /usr/local/bin/imagent mcp-ask
```

多 agent 并发互不干扰（点按钮=精确回答那张卡；打字=答最新一张）；超时返回错误（非 deny）。`imagent install.sh` 一键脚本会自动完成挂载。同 server 还暴露 `notify_via_im(message)`：单向通知立即返回——长任务跑完「叫一声」用。

## 运维

### 后台常驻（service）

前台验证可用后，装成 OS 级后台服务（崩溃自动拉起、开机自启）：

```bash
export IMAGENT_FEISHU_APP_SECRET="..."   # secret 在当前 shell export——install 快照进服务定义（唯一注入点）
imagent service install                  # 二进制先放稳定路径（如 /usr/local/bin）再装
imagent service status / uninstall / print
```

| | macOS（launchd） | Linux（systemd user） |
|---|---|---|
| 日志 | `~/.imagent/logs/daemon.log`（50MB 内置轮转） | `journalctl --user -u imagent -f` |
| 无人登录也运行 | 天然支持 | 需 `loginctl enable-linger $USER` |

### 备份与恢复（backup）

```bash
imagent backup                    # 默认 <imagent_home>/backups/，自动保留最近 10 份
imagent --profile work backup     # profile 感知
```

每份备份含 SQLite 一致性快照（`VACUUM INTO`，**运行中可备份**）+ config 副本 + MANIFEST.txt；统一 0600。keyring 凭据不在快照内（换机后重新 login / export secret）。

**恢复**：

```bash
# 1. 停止 imagent；2. 快照替换主库（旧 -wal/-shm 一并删）；3. config 副本放回；4. 重录凭据；5. 启动（schema 自动向前迁移）
cp imagent-backup-*/imagent.db.snapshot ~/.imagent/imagent.db
rm -f ~/.imagent/imagent.db-wal ~/.imagent/imagent.db-shm && imagent start
```

> **降级不可恢复**：schema 更新的快照不能被旧版二进制打开；回退二进制请用升级**前**的备份。

### 多实例（Profile）

`imagent profile create work` → `imagent --profile work start`——config/db/socket/媒体全隔离；`profile export/import` 跨机迁移。

## 安全

- **白名单鉴权**：sender + 会话（群）双白名单，非授权丢弃（Web 消息 sender 恒为 `web:user`，同样须白名单）。
- **工具收敛**：`allowed_tools` 白名单（缺省全量但**全量 ≠ 免审**）；workdir 锁定；危险操作审批兜底。
- **状态目录隔离 + 沙箱**：`~/.imagent` 对 agent 读视野隐藏（CLI 层）+ `agent_sandbox`（内核层 deny，默认 auto）。
- **Web 驾驶舱纵深**：ed25519 挑战应答（relay 只存公钥）、token-machine 绑定、会话可吊销、passkey、E2E 中继盲化、限流与离线缓冲上限。
- **store 加固**：文件 0600 / 目录 0700；CDN SSRF 白名单；服务定义（内嵌 secret）0600。
- **威胁模型**：审批闭环约束「agent 发起的工具调用」，不约束「同 uid 进程的读写能力」——提示注入场景下仍有残留攻击面，**多人群白名单部署前务必读 [SECURITY.md](SECURITY.md)**。

## 开发

```bash
cargo test --workspace                              # 全通过（CI 详情见 .github/workflows/）
cargo clippy --workspace --all-targets -- -D warnings   # 0 warning
cargo fmt --all --check
(cd web-ui && npm test)                             # SPA 单测（vitest）
```

crate：`core`（调度/鉴权/session/权限/任务控制/cron/多引擎/多平台组合器）+ `ilink` + `wecom` + `feishu` + `claude`（CLI/ACP）+ `codex`（exec + app-server）+ `gemini` + `store`（SQLite，schema v18 线性迁移）+ `relay`（ECS 中继，独立二进制）+ `web`（网关侧 Web 平台）+ `localterm`（本地终端薄视图）；SPA 在 `web-ui/`（Vite + React + Tailwind，构建产物内嵌 relay）。

## 文档地图

| 文档 | 内容 |
|---|---|
| [docs/CONFIG.md](docs/CONFIG.md) | 全量配置参考（逐键语义 + 环境变量 + `[[webhook]]` / `[[delegate]]`） |
| [docs/COMMANDS.md](docs/COMMANDS.md) | IM 命令全量参考（分组 + 群消息行为） |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 现状架构（as-built） |
| [docs/WEB_DESIGN.md](docs/WEB_DESIGN.md) | Web 驾驶舱设计（ECS 中继 / 帧协议 / 部署） |
| [docs/FEISHU_DESIGN.md](docs/FEISHU_DESIGN.md) | 飞书平台设计 |
| [docs/RESEARCH.md](docs/RESEARCH.md) | iLink 协议 / 合规调研 |
| [SECURITY.md](SECURITY.md) | 安全与威胁模型 |
| [CHANGELOG.md](CHANGELOG.md) | 版本变更记录 |
| [docs/CODE_REVIEW_v13.md](docs/CODE_REVIEW_v13.md) | 最新一轮代码复审（更早轮次见 docs/） |

## License

MIT（见 [`LICENSE`](LICENSE)）。iLink 协议出处：腾讯官方 [`@tencent-weixin/openclaw-weixin`](https://github.com/Tencent/openclaw-weixin) / [ClawBot 文档](https://developers.weixin.qq.com/doc/aispeech/knowledge/openapi/Clawbotrelated.html)。
