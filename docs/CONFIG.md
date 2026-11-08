# 配置参考

`imagent` 读取 `<imagent_home>/config.toml`（缺省 `~/.imagent/config.toml`，profile 为 `~/.imagent/profiles/<name>/config.toml`，可用 `IMAGENT_HOME` 整体改家目录）。

- 机读事实源：`crates/core/src/config.rs` 的字段文档注释——本文与其同步维护，冲突时以源码为准。
- 标注「热改」的键改完 `kill -HUP <pid>`（SIGHUP）即生效；未标注的**改动需重启**。
- **secret 一律走环境变量，不落 config 文件**。

## 最小可用样例

```toml
# ~/.imagent/config.toml（飞书 5 分钟打通）
default_workdir = "/absolute/path/to/agent/workspace"   # 必填，绝对路径；agent 的工作目录
platform = "feishu"
feishu_app_id = "cli_xxx"
allowed_senders = []        # 空 = 发现模式：首启日志拿你的 open_id，imagent allow 授权
```

其余全部键都有缺省，不写即用缺省。逐键参考如下。

## 平台与多平台

| 键 | 缺省 | 说明 |
|---|---|---|
| `platform` | `ilink` | 单平台：`ilink`（个人微信，扫码登录）/ `wecom` / `feishu` / `web` / `local` |
| `platforms` | `[]` | **多平台并存列表**，非空时优先于 `platform`（装配优先级：CLI `--platform` > 本列表 > 单值）。元素 ∈ {ilink, wecom, feishu, web, local} 且互异，MultiPlatform 组合器按 conv 前缀路由。改动需重启 |
| `feishu_app_id` | — | 飞书企业自建应用 App ID（secret 走环境变量 `IMAGENT_FEISHU_APP_SECRET`） |
| `wecom_bot_id` / `wecom_secret` | — | 企业微信机器人凭据 |
| `web_relay_url` | — | Web 驾驶舱：ECS relay 端点（`wss://<ecs>:8443/imagent/gateway`）。认证 ed25519 key 优先（`~/.imagent/web-gateway-key` 首启自动生成），token 为回退 |
| `web_machine_id` | hostname 短名 | Web 驾驶舱：本机在 relay 侧的 machine 身份（多台 Mac 接同一 relay 时区分；同机重连顶替旧连接） |
| `web_default_session` | `default` | Web 驾驶舱：无 convId 的浏览器消息落到的默认会话名（conv `web:<名>`） |
| `whisper_bin` / `whisper_model` | — | Web 驾驶舱语音转写：`whisper-cli` 路径 + ggml 模型路径，**双键齐备才启用**（`brew install whisper-cpp`；中文建议 medium 及以上）。改动需重启 |

## agent 与引擎

| 键 | 缺省 | 说明 |
|---|---|---|
| `agent` | `claude-cli` | 后端：`claude-cli` / `claude-acp`（Claude ACP 长驻子进程）/ `acp`（任意 ACP agent）/ `codex`（codex exec）/ `codex-app`（app-server 双工，审批闭环/steering 全量可用）/ `gemini`。改动需重启（SIGHUP 只热载参数，不换后端） |
| `acp_command` | — | `agent = "acp"` 必填：启动目标 ACP agent 的命令（如 `opencode-acp`、`gemini --experimental-acp`，以目标 agent 的 ACP 文档为准）。claude-acp 下可选覆盖默认命令 |
| `allowed_tools` | 全部 | 工具白名单。**缺省 = 全部工具**（`[]` / `["*"]` 同义不限制；codex 收敛 `workspace-write`、gemini 收敛 `auto_edit`）。显式清单 = 白名单，清单外工具 agent 用不了。注意全量 ≠ 免审——危险操作仍过权限审批 |
| `mcp_config_path` | — | agent 的 MCP servers 配置文件路径（与 IM 内 `/mcp add` 管理的动态清单合并） |
| `[official_models]` | 见左 | `/model` 菜单官方回退列表，per-backend 表：`claude = ["sonnet","opus","haiku"]`、`codex = ["gpt-5-codex","gpt-5.1-codex-mini"]`（codex-app 走实时 model/list 与 `~/.codex/config.toml` 目录优先，此表最末层回退）。旧单列表 `official_models = ["a","b"]` 仍兼容，映射到 claude |

## 权限与审批

| 键 | 缺省 | 说明 |
|---|---|---|
| `permission_mode` | `auto` | IM 权限审批模式：`auto`（按后端自动选档）/ `ask`（每个提示进 IM）/ `off` / `allow` / `deny`。`/perm` 可按会话热切 |
| `backend_permission_mode` | — | claude 后端原生模式透传（`--permission-mode`，覆盖 auto 档缺省）：`default` / `acceptEdits` / `plan` / `auto` / `dontAsk` / `bypassPermissions`。codex/codex-app/gemini 不支持透传（warn 忽略）。SIGHUP 热改 |
| `approval_tools` | `[]` | 审批集：ask 类档位下**只有**清单内工具过 IM 审批，其余直接放行。支持尾部 `*` 前缀匹配（如 `mcp__*`）；空 = 全部过审。仅 claude-cli 生效 |
| `claude_permission_channel` | `control` | claude 审批传输通道：`control`（canUseTool 双工协议，现行标准）/ `mcp`（旧 `--permission-prompt-tool` 回退通道）。SIGHUP 热切 |
| `hide_state_dir_from_agent` | 开 | 每轮注入 `--settings` deny 规则，把 `~/.imagent` 挡在 agent 读视野外（仅 claude-cli；防提示注入直读 db，Bash 子进程级由沙箱兜底） |

## 任务控制

| 键 | 缺省 | 说明 |
|---|---|---|
| `agent_timeout_secs` | `3600` | 单次运行总超时（秒）；`0` = 关闭（防挂死全靠空闲看门狗） |
| `agent_idle_timeout_secs` | `1200` | 空闲看门狗：连续无输出 N 秒自动终止；`0` = 关。`/timeout` 可按会话覆盖 |
| `batch_window_ms` | `1500` | 连发消息合并为一轮 prompt 的批窗口（ms）；`0` = 关。`/config` 可热改 |

## 展示与免打扰

| 键 | 缺省 | 说明 |
|---|---|---|
| `cot_detail` | `brief` | 工具过程展示：`off` / `brief` / `detailed`。`/config` 可热改，`/config cot` 按会话覆盖 |
| `reply_mode` | `card` | 回复形态偏好：`card`（卡片）/ `text`（纯文本流，无卡片权限或偏好简洁时）。改动需重启（IM 内 `/config reply_mode` 热改不受限） |
| `quiet_hours` | — | 免打扰时段（本地时区，可跨天，如 `"22:00-08:00"`）：时段内加急（buzz）提醒降级普通消息，内容不变 |
| `feishu_require_mention_in_group` | 开 | 群消息默认须 `@机器人`；全收群消息设 `false`（配 `im:message.group_msg` 权限） |
| `feishu_thread_active_window_secs` | `1800` | 话题群免 @ 窗口：话题内近期有过消息则豁免群消息须 @bot；`0` = 关 |
| `feishu_urgent_on_ask` | 开 | 审批/问题卡到达即应用内加急弹通知（免打扰时段自动跳过） |
| `feishu_group_context_messages` | `10` | 群聊上下文注入：群消息触发轮次自动拉本群最近 N 条消息作前置上下文；`0` = 关 |
| `shortcuts` | — | 快捷命令表：`/name` 映射到 prompt 模板（`$args` = 命令剩余参数），按普通 agent 消息分派（完整鉴权/批处理路径） |

## 定时与事件入站

| 键 | 缺省 | 说明 |
|---|---|---|
| `cron_catchup` | `one` | `/cron` 停机补跑：`one`（触发一次）/ `off`（陈旧跳过）/ `all`（逐周期补跑，上限 3 条） |
| `webhook_addr` | — | Webhook 入站监听地址（如 `127.0.0.1:18443`，`POST /hook/<token>`）。非 loopback 绑定且任一条目未配 secret → 拒绝启动（fail-closed） |

`[[webhook]]` 条目（token → 会话；须 `/chat allow` 放行才会驱动 agent）：

```toml
# ===== 事件入站 =====
webhook_addr = "127.0.0.1:18443"
[[webhook]]
token  = "0123456789abcdef0123456789abcdef"   # 32 hex，URL 路径鉴权
conv   = "feishu:oc_xxx"                       # 注入目标会话
name   = "ci"                                  # 注入消息带【ci】来源前缀
secret = "github-webhook-secret"               # 可选 HMAC-SHA256 验签（X-Hub-Signature-256；公网/隧道部署强烈建议）
rps    = 10                                    # 可选限速（请求/秒，缺省 10；0 = 不限）
replay_window_secs = 300                       # 防重放第 2 层（opt-in，须配 secret）：请求须带
                                               #   X-Imagent-Timestamp: <unix秒>，验签串 "{ts}.{body}"，
                                               #   |now-ts| > 窗口 → 401
```

防重放两层：① 验签通过后按 `(token, 签名)` 去重（默认启用，10 分钟内同签名重发 409；记录持久化到 SQLite，重启不失效）；② `replay_window_secs` 时间窗（GitHub 原生签名不含时间戳无法配合，公网 + GitHub 场景靠 ① + HTTPS）。GitHub 原生事件（带 `X-GitHub-Event` 头）自动解析为可读摘要注入（workflow_run 终态 / push / issues / 评论 / PR / ping）；其它来源 POST `{"text":"..."}` 或纯文本。

## 委托编排

```toml
# 配置后 claude-cli agent 获得 delegate_run 工具、/delegate 命令可用——
# 把子任务 fan-out 给副后端（如 Claude 写完叫 codex 审）。SIGHUP 热改生效。
[[delegate]]
name = "review"                              # 委托名（唯一；delegate_run / /delegate 按名调用）
agent = "codex"                              # 副后端：codex | gemini | claude-cli（one-shot 系）
allowed_tools = ["Read", "Grep", "Glob"]     # 缺省只读三件套；要放宽显式列（风险自负）
model = "gpt-5-codex"                        # 可选模型
timeout_secs = 600                           # 单次预算（秒，缺省 600，上限 3600；工具只能调低）
```

副轮 one-shot（无会话续接、无 IM 审批闭环）、全局并发闸 4、输出头尾窗截断。已知限制见 CHANGELOG v1.31.0。

## 用量护栏

| 键 | 缺省 | 说明 |
|---|---|---|
| `model_context_window_tokens` | `0` | 模型上下文窗口（token）。设 >0 即开启**比例档**自动压缩：水位达 窗口 × ratio 触发（摘要+重置+下轮注入【前情摘要】）。200k 窗口的 Claude 系模型写 `200000` |
| `auto_compact_window_ratio` | `0.8` | 比例档触发比例（与上一键配合；均支持 SIGHUP 热改） |
| `auto_compact_threshold_tokens` | `0` | **绝对值档**：仅窗口设 0 且本值 >0 时生效 |

自动压缩**默认关闭**（不自动压缩、不主动提醒；网关 token 口径与 CLI 估算有偏差，误压缩丢上下文的代价高于收益）。ACP 路径经 `UsageUpdate.size` 自动学习真实窗口——200k 模型零配置防溢出。

## OS 级 agent 沙箱（仅 claude-cli）

| 键 | 缺省 | 说明 |
|---|---|---|
| `agent_sandbox` | `auto` | `auto` = 内核层定点 deny：状态目录 `~/.imagent` 读+写（例外：审批 sock/token/每轮 mcp json）+ 持久化热点写（`~/.ssh`、shell rc、LaunchAgents；读保留）——正常开发零影响。`strict` = 再把写收敛到 workdir/临时目录/`~/.claude`/extra_write（包管理器缓存需显式加清单）。`off` = 关闭。macOS Seatbelt / Linux bwrap（未安装降级 off + warn）。SIGHUP 热改 |
| `agent_sandbox_extra_write` | `[]` | `strict` 档额外可写目录（绝对路径） |

## 多维表格数据面（Bitable）

| 键 | 说明 |
|---|---|
| `feishu_bitable_app_token` | 多维表格 URL `.../base/<这段>` |
| `feishu_bitable_table_id` | 同 URL `.../table/<这段>` |

仅 `platform = "feishu"` 且 `agent = "claude-cli"` 时生效；**双键齐备才启用**。启用后 agent 获得 `bitable_list_fields` / `bitable_append_row` 工具可向该表写行（建议专用表；需 `bitable:app` 权限）。SIGHUP 热改。

## 终端 agent 接入（ask_via_im）

| 键 | 缺省 | 说明 |
|---|---|---|
| `ask_via_im_conv` | — | 提问投递会话（如 `"feishu:ou_xxx"`，`/whoami` 可查）。配了才启用 |
| `ask_via_im_timeout_secs` | `1800` | 等待用户回复超时（秒） |

## 环境变量

| 变量 | 用途 |
|---|---|
| `IMAGENT_FEISHU_APP_SECRET` | 飞书 App Secret（不落 config） |
| `IMAGENT_WEB_GATEWAY_TOKEN` | Web 驾驶舱网关 token（ed25519 密钥文件损坏时的回退；与 relay 侧本机 machine 绑定项同值） |
| `IMAGENT_ACP_COMMAND` | `agent = "claude-acp"` 时覆盖默认 ACP 命令（优先级：`acp_command` > 本变量 > claude-agent-acp） |
| `IMAGENT_LOG_MAX_MB` | macOS 守护日志轮转阈值（MB，`0` = 不限；改后需重装服务生效） |
| `IMAGENT_HOME` | 状态目录根（缺省 `~/.imagent`） |

profile 的 secret 快照：`imagent service install` 把当前 shell 的 secret 环境变量「快照」进服务定义（0600）——守护进程起不来交互 shell，这是唯一注入点，轮换后重新 `export` + 重装。
