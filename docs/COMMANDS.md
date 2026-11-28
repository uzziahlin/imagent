# IM 命令参考

IM / Web 驾驶舱 / 本地终端里可直接发的斜杠命令。白名单用户可用；标注「管理员」的需要 `admin_senders`（或 IM 内 `/admin add` 授权）。

## 会话管理

| 命令 | 作用 |
|---|---|
| `/new` | 重置会话（开新上下文） |
| `/switch <name>` | 切到 / 新建命名会话（多任务并行上下文） |
| `/sessions` | 列命名会话（`*` 标当前） |
| `/resume [n]` | 统一恢复列表：📱 IM 会话 ∪ 💻 电脑端 Claude Code 会话（摘要+时间辨认，按序号接管，无需会话 id） |
| `/export [n]` | 导出当前（或 /resume 序号）会话为 Markdown 文件（claude 系后端） |
| `/again` | 再跑最近一次成功指令（与 /retry 的失败轮重试对称） |
| `/retry` | 重发最近一轮指令（失败/中断后一键续接） |
| `/compact` | 软压缩上下文（摘要 + 重置 + 延续）；自动触发默认关闭，开启见[用量护栏](./CONFIG.md#用量护栏) |
| `/cd [path]` | 切工作目录（`/resume` 本机会话列表随之变化） |
| `/ws list\|save\|use\|remove` | 命名工作空间（与 Web 驾驶舱同源互通） |

## 模型与引擎

| 命令 | 作用 |
|---|---|
| `/model [名称\|default]` | 查看/热切模型（切换需管理员；claude 系 / codex `-m` / gemini `-m` 全支持） |
| `/backend [名]` | 查看/切换**本会话** agent 引擎（如 `/backend codex-app`；下一条消息生效——切换后上下文不延续，原引擎会话可 /resume 找回） |
| `/effort [档\|default]` | 查看/设置**本会话**思考深度（仅受支持引擎如 codex-app，其余如实报错；管理员；`default`=清覆盖回引擎默认档） |
| `/delegate [list\|<name> <子任务>]` | 委托编排：列出已配置副后端 / 把子任务 one-shot 交给命名副 agent（需 config `[[delegate]]`） |

## 任务控制

| 命令 | 作用 |
|---|---|
| `/stop [all]` | 中断在飞任务（**排队消息保留并自动转入下一轮**；`/stop all` 硬停清空排队；任务恰在收尾时如实回「已完成」不谎报中断） |
| `/queue list\|drop <n>` | 查看当前会话排队消息 / 丢弃指定序号（自己的或管理员） |
| `/timeout [N\|off\|default]` | 会话级空闲看门狗（分钟） |
| `/perm <auto\|off\|allow\|deny\|ask>` | 权限模式热切（auto=按后端自动选档） |
| `/perm list` / `/perm revoke <工具>` | 查看/单项撤销本会话「始终允许」清单 |
| `/tasks` | 本会话在飞轮次的实时进度（checklist + 工具统计） |

## 本地窗口（peer 通道）

需要 `peer_enabled = true`（见[配置参考](./CONFIG.md)）。这里只算**终端里的交互窗口**：网关自己跑的无头会话、Claude Code 的后台作业不是窗口，不列出也不能派活。驾驶舱侧栏的「本地窗口」区块是同一份名单。

| 命令 | 作用 |
|---|---|
| `/peers` | 列出当前在线的本地 Claude Code 窗口 |
| `/to <窗口名>` | 进入和该窗口的对话（窗口名可用唯一前缀；不带正文 = 只建立会话，之后直接打字；窗口空闲时会自动开始处理） |

可选：在电脑上执行 `imagent peer hook install`，给 Claude Code 装官方 Stop 钩子，之后每个窗口每一轮的结论自动同步进驾驶舱里对应的对话（以「📋 本轮结束（自动同步）」开头）。`imagent peer hook status` 查看是否已装，`imagent peer hook uninstall` 撤掉；加 `--scope project` 装到当前目录的项目级配置而不是用户级。

## 定时任务

| 命令 | 作用 |
|---|---|
| `/cron add <分 时 日 月 周> <指令>` | 定时任务（本地时区含 DST；`*`/`*/n`/范围/列表，如 `/cron add */10 * * * * 检查构建`） |
| `/cron list` / `/cron rm <id>` | 列出（含已停用）/ 删除（限创建者或管理员；每会话上限 20 条） |

## 媒体与输出

| 命令 | 作用 |
|---|---|
| `/img <path>` | 发 workdir 内图片到 IM |
| `/file <path>` | 发 workdir 内任意文件到 IM |

## 配置与运维

| 命令 | 作用 |
|---|---|
| `/config [k v]` | 查看 / 热改配置（cot_detail / batch_window_ms / agent_idle_timeout_secs / require_mention / reply_mode，管理员）；`/config cot <off\|brief\|detailed\|default>` 为本会话 COT 偏好（白名单可用，免 admin） |
| `/status` | 运行状态（含上下文水位与阈值距离） |
| `/doctor` | 自检（平台权限 + 安全维度：凭据明文形态、webhook 暴露面、共享工作区、权限×能力错配、护栏水位、DB/媒体体积、沙箱档位） |
| `/reconnect` | 强制平台重连 |
| `/mcp list · add <名> <url> · rm <名>` | agent 的 MCP servers 热管理（管理员；下一轮生效，重启不丢；与 config `mcp_config_path` 文件源合并） |

## 管理与查询

| 命令 | 作用 |
|---|---|
| `/allow <id\|@名字>` `/disallow` | 授权 / 撤销 sender（飞书群内可直接 @ 对方，管理员门槛） |
| `/admin [list\|add\|remove]` | 管理员动态管理（首位设立自动带操作者，防自锁；SIGHUP 同步 config 变更） |
| `/chat allow\|deny\|allow-all\|list` | 会话（群）白名单；`allow-all` 批量放行 bot 已加入的全部群 |
| `/list` | 查看白名单 |
| `/whoami` | 查自己的 sender 与会话 id |

## 群消息行为

- 群消息默认须 `@机器人`（`feishu_require_mention_in_group`，正文 @ 占位自动清洗）；话题群内近期（`feishu_thread_active_window_secs`，默认 30 分钟，0=关闭）有过消息则免 @ 追问。
- 群会话的回复与流式卡会**引用发起消息**并标注发起者（飞书经 contact API 懒解析展示名，需 `contact:user.base:readonly` 权限，缺权限回退 open_id 短版）。
- 长任务（运行超 5 分钟）或含审批 / 询问的轮次结束后有一条完成提醒（60 秒内刚做过审批决定则不发，免打扰时段见下）。飞书对会话最新卡片发**应用内加急**——卡片弹通知，不产生新消息；纯文本回复模式下没有卡片，发一条加急文本。驾驶舱不单独发这条提醒：终态卡片的页脚已有时长与成本，卡片变为终态时点亮该会话的未读。
- 加急提醒受 `quiet_hours`（如 `"22:00-08:00"`，本地时区可跨天）约束——时段内审批过半催办、纯文本会话的完成提醒降级为不加急的普通消息（内容不变），卡片会话的完成提醒不对卡片加急。
- `/config reply_mode text` 可切纯文本回复（无卡片权限或偏好简洁时）。
