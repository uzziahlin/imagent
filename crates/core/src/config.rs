//! 配置加载（`~/.imagent/config.toml`）。

use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    /// 按后端自动选档（2026-08 起为**缺省**）：claude-cli（支持 IM 审批闭环）→
    /// [`PermissionMode::AutoClaude`]（claude 原生 acceptEdits + 危险工具走 IM，
    /// 即 Claude Code 的「auto 模式」）；claude-acp / codex / gemini（闭环未接）→
    /// [`PermissionMode::Off`]（靠各自 sandbox / approval-mode 兜底）。启动 /
    /// SIGHUP / `/perm auto` 均先 [`PermissionMode::resolve`] 成具体档再入运行时。
    #[default]
    Auto,
    /// **运行时专属档**（配置面不可直接写，仅由 `auto` 在 claude-cli 下解析产生）：
    /// 审批闭环照挂（分类器拦下的高危提示进 IM），另透传 claude 原生
    /// `--permission-mode auto`（2026 新档：独立分类器逐动作审查，安全操作自动
    /// 放行，高危动作才提示——比 [`PermissionMode::Ask`]（每个提示都进 IM）少
    /// 打扰）。透传值可由 `claude_permission_mode` 配置覆盖。
    #[serde(skip)]
    AutoClaude,
    /// 不启用权限审批：claude 按 --allowedTools 自行处理（P1 既有行为）。
    Off,
    /// MCP server 永远 allow（不发 IM、不阻塞；快速放行模式）。
    Allow,
    /// MCP server 永远 deny（不发 IM、不阻塞；严格拦截模式）。
    Deny,
    /// 完整 IM approve/deny 闭环：发 IM 询问用户、等待回复路由回 MCP。
    Ask,
}

impl PermissionMode {
    /// 是否需要附加 --mcp-config / --permission-prompt-tool。
    /// 注意 `Auto` 须先 [`PermissionMode::resolve`]——未解析的 Auto 按未接线
    /// 处理（false），防半接状态。
    pub fn is_enabled(self) -> bool {
        matches!(
            self,
            Self::Allow | Self::Deny | Self::Ask | Self::AutoClaude
        )
    }

    /// 是否需要主进程的权限审批 socket（Ask 闭环类：Ask / AutoClaude）——
    /// dispatcher 启动 socket accept task 与 `/perm` 热切的「须重启」提示共用。
    pub fn needs_socket(self) -> bool {
        matches!(self, Self::Ask | Self::AutoClaude)
    }

    /// `Auto` 按后端解析成具体档（具体档原样返回）。
    pub fn resolve(self, agent: &str) -> Self {
        match self {
            Self::Auto if agent == "claude-cli" => Self::AutoClaude,
            Self::Auto => Self::Off,
            other => other,
        }
    }
    /// 小写标签，用于 MCP 子命令 --mode 参数与日志。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::AutoClaude => "auto-claude",
            Self::Off => "off",
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            // MCP 子进程 --mode 往返（backend 经 as_str 写入 mcp 配置）。
            "auto-claude" => Self::AutoClaude,
            "allow" => Self::Allow,
            "deny" => Self::Deny,
            "ask" => Self::Ask,
            _ => Self::Off,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CotDetail {
    /// 不展示任何工具过程（只回最终结果）。
    Off,
    /// 简要工具摘要（默认：工具行 80 字符截断，最多 5 个）。
    #[default]
    Brief,
    /// 详细工具过程（240 字符截断，最多 10 个）。
    Detailed,
}

impl CotDetail {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Brief => "brief",
            Self::Detailed => "detailed",
        }
    }
    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "brief" => Some(Self::Brief),
            "detailed" => Some(Self::Detailed),
            _ => None,
        }
    }
    /// 工具摘要单行的字符截断上限（P8-1：截的是人可读摘要而非裸 JSON，
    /// 80 与 lcab 的 HEADER_SUMMARY_MAX 对齐）。
    pub fn input_trunc(self) -> usize {
        match self {
            Self::Off => 0,
            Self::Brief => 80,
            Self::Detailed => 240,
        }
    }
    /// 工具摘要最多展示条数。
    pub fn max_tools(self) -> usize {
        match self {
            Self::Off => 0,
            Self::Brief => 5,
            Self::Detailed => 10,
        }
    }
}

/// Wave B-4：免打扰时段（`quiet_hours = "22:00-08:00"`，**本地时区**）。
///
/// 窗口可跨天（22:00 → 次日 08:00）。只影响 **buzz 类加急提醒**（审批过半催办、
/// 长任务完成强提醒）：时段内降级为普通消息（不加 buzz 字段）——**不影响消息
/// 内容、不影响普通消息的发送**，仅去掉加急振铃。`None`（缺省）= 不启用。
/// start == end 表示空窗口（无任何效果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietHours {
    /// 起始时刻（当天 0:00 起的分钟数，0..=1439）。
    start: u32,
    /// 结束时刻（分钟数；start > end 表示跨天窗口）。
    end: u32,
}

impl QuietHours {
    /// 解析 `"HH:MM-HH:MM"`（含两端；结束时刻不含在窗口内——`22:00-08:00` 在
    /// 08:00 整点已结束）。非法格式（缺段 / 越界 / 非数字）返回 None。
    pub fn parse(raw: &str) -> Option<Self> {
        let (a, b) = raw.trim().split_once('-')?;
        let start = parse_hhmm(a)?;
        let end = parse_hhmm(b)?;
        Some(Self { start, end })
    }

    /// 时刻（当天 0:00 起的分钟数）是否落在窗口内：`start ≤ end` 常规窗口；
    /// `start > end` 跨天窗口（t ≥ start **或** t < end）。
    pub fn contains(&self, minute_of_day: u32) -> bool {
        if self.start == self.end {
            false
        } else if self.start < self.end {
            minute_of_day >= self.start && minute_of_day < self.end
        } else {
            minute_of_day >= self.start || minute_of_day < self.end
        }
    }

    /// 原文展示（/config 查看用）：`22:00-08:00`。
    pub fn display(&self) -> String {
        format!("{}-{}", fmt_hhmm(self.start), fmt_hhmm(self.end))
    }
}

/// `"HH:MM"` → 当天分钟数。
fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h < 24 && m < 60 {
        Some(h * 60 + m)
    } else {
        None
    }
}

/// 分钟数 → `"HH:MM"`。
fn fmt_hhmm(mins: u32) -> String {
    format!("{:02}:{:02}", mins / 60, mins % 60)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplyMode {
    /// 流式卡片（默认）：支持卡片的会话走 CardKit/整卡流式，无权限自动降级文本。
    #[default]
    Card,
    /// 纯文本：不建卡，流式走文本多发（偏好简单/无卡片权限的用户，P7-A4）。
    Text,
}

impl ReplyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Card => "card",
            Self::Text => "text",
        }
    }
    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "card" => Some(Self::Card),
            "text" => Some(Self::Text),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Config {
    /// agent 工作根目录（agent 的 cwd，**非沙箱**：仅决定工作目录，不限制可读路径；
    /// 危险操作用 permission_mode=ask 审批）。必填，缺失或非绝对路径 => Config 错误。
    pub default_workdir: PathBuf,
    #[serde(default)]
    pub allowed_senders: Vec<String>,
    /// 会话（群）白名单种子（P4-5）：存 conv_id 原样（如 `feishu:oc_xxx`）。
    /// 群消息「chat 放行 OR sender 放行」即过鉴权。运行时经 `/chat` 动态管理。
    #[serde(default)]
    pub allowed_chats: Vec<String>,
    /// 管理员 sender（可执行 /allow /disallow /config /perm /admin）。S2：空 =
    /// **无人**是管理员（IM 内管理命令不可用，须 CLI / setup 配置）；显式设置
    /// 以收敛授权面。
    #[serde(default)]
    pub admin_senders: Vec<String>,
    #[serde(default = "default_tools")]
    pub allowed_tools: Vec<String>,
    #[serde(default = "default_agent")]
    pub agent: String,
    #[serde(default = "default_platform")]
    pub platform: String,
    /// IM 权限审批模式（默认 Auto：按后端自动选档，见 [`PermissionMode::Auto`]）。
    #[serde(default)]
    pub permission_mode: PermissionMode,
    /// 审批集：ask/auto→ask 模式下**只有**清单内的工具走 IM 审批，其余权限
    /// 请求直接放行（记日志 + 指标）。空 = 现状（所有权限请求都过审）。
    /// 条目为工具名，支持尾部 `*` 前缀匹配（如 `mcp__*`）。仅 claude-cli 生效
    ///（其余后端无闭环；且 claude 自身默认放行的工具如 Read 不会发起请求，不受此影响）。
    #[serde(default)]
    pub approval_tools: Vec<String>,
    /// **后端原生权限模式透传**（各后端映射到自己的原生 flag）：
    /// - claude-cli → `--permission-mode`：default/manual | acceptEdits | plan |
    ///   **auto**（2026 新档：分类器自动放行安全操作，高危提示经 IM 审批闭环）|
    ///   dontAsk | bypassPermissions
    /// - codex / gemini / claude-acp → 暂不支持（启动时 warn 并忽略；后续接入
    ///   approval-policy / approval-mode 时复用本键，不再加新配置）
    ///
    /// **缺省 None**：permission_mode="auto" 在 claude-cli 下透传 `auto`，ask 档
    /// 不透传（全量进 IM）；显式设置则两档都遵从。值域按后端校验（claude 系白
    /// 名单；其余后端先存值、后端侧忽略）。旧版 claude CLI（<2.1.228）不认 auto
    /// 会静默回退 default（≈ask 档行为，降级安全）。
    #[serde(default)]
    pub backend_permission_mode: Option<String>,
    /// Prometheus 指标 / 健康检查 HTTP 监听地址（如 `"127.0.0.1:9100"`）。
    /// 默认 `None`（关闭——开源分发时不默认开启监听端口）；显式设置地址即开启。
    #[serde(default = "default_metrics_addr")]
    pub metrics_addr: Option<String>,
    /// 出站消息单条字符上限（Unicode char 计）。超长则由各 Platform 的 `send_text`
    /// 在内部分片——**三平台生效**（ilink / feishu / wecom，各平台再与自身协议
    /// 硬上限取 min：飞书 28000、企微 4000 字节）。`None` = 不按此配置分片
    /// （默认，仅用各平台协议上限）。
    #[serde(default)]
    pub message_max_len: Option<usize>,
    /// 分片之间发送间隔（毫秒），避免多条叠加触发 IM 限流。默认 400。
    #[serde(default = "default_fragment_interval_ms")]
    pub message_fragment_interval_ms: u64,
    /// 单次 agent 运行总超时（秒）。超时则中止该次 run（依赖 backend 的
    /// `kill_on_drop` 杀子进程）。**默认 0 = 关闭**：这是墙钟总预算，与 agent
    /// 是否活跃无关，长任务会被误杀；防挂死由空闲看门狗
    /// （`agent_idle_timeout_secs`，连续无输出才杀）承担。仅在需要硬上限时设置。
    #[serde(default = "default_agent_timeout_secs")]
    pub agent_timeout_secs: u64,
    /// 权限审批（Ask 模式）等待用户回复的超时（秒），超时则 deny。默认 300（5 分钟）。
    /// S-3：独立预算——审批等待不再挤占 `agent_timeout` 的执行预算（`agent_timeout`
    /// 覆盖审批 + 执行总和）。D8：`agent_timeout_secs` 非 0（启用总超时）时必须
    /// 小于它（否则慢审批撑满 agent 总预算、看门狗语义错乱），加载期强制校验、
    /// 违反拒绝启动；0（总超时关闭）时无此约束。
    #[serde(default = "default_permission_ask_timeout_secs")]
    pub permission_ask_timeout_secs: u64,
    /// 终端 agent 的 `ask_via_im` MCP 工具：询问默认投递的目标会话
    /// （如 `feishu:ou_xxx`）。None（默认）= 未启用，`imagent mcp-ask` 不暴露工具。
    #[serde(default)]
    pub ask_via_im_conv: Option<String>,
    /// `ask_via_im` 等待用户回复的超时（秒），可被工具调用的 timeout_secs 覆盖。
    /// 远程场景用户可能长时间不在，默认 1800（30 分钟），远大于审批的 300。
    #[serde(default = "default_ask_via_im_timeout_secs")]
    pub ask_via_im_timeout_secs: u64,
    /// 优雅退出（SIGINT/SIGTERM）drain in-flight task 的宽限期（秒）。超时则 abort
    /// 剩余（kill_on_drop 杀 agent 子进程）。默认 60。R-1：原硬编码 30s 偏短。
    #[serde(default = "default_shutdown_grace_secs")]
    pub shutdown_grace_secs: u64,
    /// 空闲看门狗（秒）：agent 连续该时长无任何输出（chunk）则终止本轮并杀子进程，
    /// 防 stream 僵死干等 agent_timeout 总预算。等待 IM 权限审批期间看门狗自动暂停
    /// （审批有独立的 permission_ask_timeout_secs 预算）。默认 300；0 = 关闭。
    #[serde(default = "default_agent_idle_timeout_secs")]
    pub agent_idle_timeout_secs: u64,
    /// 批处理窗口（毫秒）：runner 起跑前等待后续消息并入同一轮 prompt 的时长；
    /// 运行中到达的消息同样排队到下一轮合并（以 \n\n 拼接）。默认 1500；0 = 关闭。
    #[serde(default = "default_batch_window_ms")]
    pub batch_window_ms: u64,
    /// 工具过程（COT）展示档位（P4-6）：off / brief（默认）/ detailed。
    /// 控制「🔧 工具调用」摘要与流式卡片工具面板的粒度；可经 `/config` 热改。
    #[serde(default)]
    pub cot_detail: CotDetail,
    /// 若为 true，凭据必须写入 OS keyring；keyring 不可用时 **拒绝明文落盘**
    /// （`put_credential` 返回 Err，fail-closed）。默认 false（headless/CI 无 keychain
    /// 时明文回退 + warn，向后兼容）。安全敏感部署应设 true。
    #[serde(default)]
    pub require_keyring: bool,
    /// WeCom 智能机器人凭据（可选；仅 `platform = "wecom"` 时使用）。
    #[serde(default)]
    pub wecom_bot_id: Option<String>,
    /// WeCom 智能机器人 secret（可选）。
    ///
    /// ⚠️ S-4：secret 当前明文存于此文件（与 iLink bot_token 走 OS keyring 不一致）。
    /// 务必将 config.toml 收紧到 0600；完整 keyring 保护（含 bootstrap 命令）见
    /// docs/CODE_REVIEW_v4.md S-4（后续）。
    #[serde(default)]
    pub wecom_secret: Option<String>,
    /// 飞书自建应用 app_id（可选；仅 `platform = "feishu"` 时使用）。非敏感。
    #[serde(default)]
    pub feishu_app_id: Option<String>,
    /// 飞书 OpenAPI base_url（可选；默认 `https://open.feishu.cn`）。
    /// Lark 国际版用 `https://open.larksuite.com`（MVP 不覆盖）。
    #[serde(default)]
    pub feishu_base_url: Option<String>,
    /// 飞书群消息是否必须 @bot 才处理（P6-1；默认 true）。p2p 不受限。
    /// true 时客户端过滤未 @bot 的群消息（静默丢弃），并剥离正文里的 @bot 占位；
    /// bot id 取不到时退化为「消息内含任意 @」弱过滤。改此项需重启。
    #[serde(default = "default_feishu_require_mention_in_group")]
    pub feishu_require_mention_in_group: bool,
    /// 陌生人被 @ 提示（P7-A3；默认 false = 完全静默，防探测）。开启后：未过白名单
    /// 的群消息若 @ 了 bot，回一句「管理员可 /chat allow」引导（私聊始终静默）。
    #[serde(default)]
    pub stranger_mention_hint: bool,
    /// 私聊陌生人引导（默认 true）：未过白名单的**私聊**消息回一句引导（含 sender
    /// id 与「联系管理员 /allow <id>」）。与 `stranger_mention_hint` 的区别：群内
    /// 默认静默是防探测（群成员构成敏感面）；私聊是用户**主动**找 bot，消息本就
    /// 一对一到达，无探测面可言——默认引导，帮首次使用者拿到自己的 id 完成授权。
    /// 关闭后私聊同样完全静默。
    #[serde(default = "default_stranger_p2p_hint")]
    pub stranger_p2p_hint: bool,
    /// 回复形态偏好（P7-A4；默认 card）。text = 不建卡走纯文本流（/config 可热改）。
    #[serde(default)]
    pub reply_mode: ReplyMode,
    /// Wave B-4：免打扰时段（`"22:00-08:00"`，本地时区，可跨天；None = 不启用）。
    /// 只影响 buzz 类加急提醒：时段内降级为普通消息（不加 buzz 字段），内容不变。
    /// 改动需重启（提醒发送在各平台实现侧，无热改句柄）。
    #[serde(default)]
    pub quiet_hours: Option<String>,
    /// Wave B-4：`quiet_hours` 的解析产物（load 期填充；serde 跳过——原始串才是
    /// 配置面，解析失败的串在 load 期直接报错，不会残留到该字段）。
    #[serde(skip)]
    pub quiet_hours_parsed: Option<QuietHours>,
    /// Wave B-8：话题群「近期活跃」免 @ 窗口（秒，仅 feishu 平台）：该话题在此
    /// 窗口内有过消息即豁免 require_mention（追问场景免于每条 @）。默认 1800
    /// （30 分钟）；0 = 关闭豁免。改动需重启。
    #[serde(default = "default_feishu_thread_active_window_secs")]
    pub feishu_thread_active_window_secs: u64,
    /// W1-2：claude 模型选择（claude 系后端 `--model`；None = CLI 自身默认/本机
    /// 配置）。`/model` 命令可运行时热改（进程内），重启/SIGHUP 恢复为本值。
    #[serde(default)]
    pub claude_model: Option<String>,
    /// W1-2：claude fallback 模型（`--fallback-model`，主模型不可用时自动回退）。
    /// 仅在 `claude_model` 同时设置时附加。
    #[serde(default)]
    pub claude_fallback_model: Option<String>,
    /// W1-2：禁用工具清单（claude `--disallowedTools`；与 allowed_tools 独立：
    /// allowed 先收敛、disallowed 再剔除，黑名单优先）。空 = 不附加。
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// W1-3：用户 MCP servers 配置文件路径（标准 `.mcp.json` 形态，顶层
    /// `mcpServers` 表）。设置后每次 run 生成的审批用 mcp 配置会**合并**其中的
    /// servers（给 agent 挂用户工具，如飞书文档/消息读写）；名为 `imagent` 的
    /// 条目被跳过（审批闭环专用名，防覆盖）。load 期校验存在 + 可解析。
    #[serde(default)]
    pub mcp_config_path: Option<PathBuf>,
    /// W1-4：附加系统提示（claude `--append-system-prompt`）：网关人设 / 回复
    /// 格式约束等，追加在 agent 自身 system prompt 之后。改动需重启/SIGHUP。
    #[serde(default)]
    pub append_system_prompt: Option<String>,
    /// W2-5：自动 compact 阈值（tokens）——成功轮次的上下文水位
    /// （usage.input_tokens）达到阈值即自动走 /compact 管道（生成摘要 + 重置，
    /// 对齐 Claude Code 原生 auto-compact）。默认 120_000；0 = 关闭。
    #[serde(default = "default_auto_compact_threshold_tokens")]
    pub auto_compact_threshold_tokens: u64,
    /// W2-4：claude-acp 并发连接上限（每会话一条长驻子进程连接；超限拒绝）。
    /// 默认 8；仅 `agent = "claude-acp"` 生效。改动需重启。
    #[serde(default = "default_acp_max_connections")]
    pub acp_max_connections: usize,
    /// W2-4：claude-acp 连接空闲回收时长（秒；窗口内无新 prompt 则断开子进程）。
    /// 默认 600；仅 `agent = "claude-acp"` 生效。改动需重启。
    #[serde(default = "default_acp_idle_recycle_secs")]
    pub acp_idle_recycle_secs: u64,
    /// W3-1：飞书语音转文字（speech_to_text/v1/file_recognize，60s 内语音条）。
    /// 默认 true；需在飞书后台申请语音识别权限——无权限/识别失败时回退为
    /// 媒体错误提示（fail-soft，不影响其余消息）。仅 feishu 平台生效。
    #[serde(default = "default_feishu_asr_enabled")]
    pub feishu_asr_enabled: bool,
}

/// 缺省工具集：读/检索/联网/文件编辑类（与 Edit 同风险级：workdir 内写或只读），
/// **不含执行类**——Bash 等显式 opt-in，配 permission_mode=ask 过 IM 审批。
/// 2026-08 起：**缺省 = 全部工具**（`["*"]` 语义——不指定即不收敛，各 backend
/// 取自身最宽档）。要收敛就显式列白名单（如 `["Read","Edit"]`）。执行类工具
/// 建议始终配合 `permission_mode = "ask"` 走 IM 审批。
fn default_tools() -> Vec<String> {
    vec!["*".into()]
}
fn default_agent() -> String {
    "claude-cli".into()
}
fn default_platform() -> String {
    "ilink".into()
}
fn default_metrics_addr() -> Option<String> {
    None
}
fn default_feishu_require_mention_in_group() -> bool {
    true
}
fn default_stranger_p2p_hint() -> bool {
    true
}
fn default_feishu_thread_active_window_secs() -> u64 {
    30 * 60
}

/// P6-8：工作目录安全校验——拒绝过宽位置（agent 以 cwd 定位工作区，`/`、home 根、
/// 系统目录、temp 根等于放权全盘）。`/cd`、`/ws use`、`setup` 向导共用。
/// 返回 `Err(人话原因)`。
pub fn validate_workdir(p: &Path) -> std::result::Result<(), String> {
    if !p.is_absolute() {
        return Err(format!("工作目录必须是绝对路径：{}", p.display()));
    }
    if !p.is_dir() {
        return Err(format!("目录不存在或不是目录：{}", p.display()));
    }
    // 过宽位置黑名单——条目与输入**都**走 canonicalize 归一比较（macOS 的
    // /tmp→/private/tmp、/etc→/private/etc 等 symlink 形态两侧一致消解；
    // Linux 上不存在的条目 canonicalize 失败则保留原样，不影响命中）。
    // S6（v7 review）：补齐 canonicalize 后不再与任何条目相等的等价敏感根——
    // /private 本体，以及 /var/tmp（归一为 /private/var/tmp，不等于 /var）。
    const BROAD: &[&str] = &[
        "/tmp",
        "/private/tmp",
        "/var/tmp",
        "/private/var/tmp",
        "/private",
        "/var",
        "/etc",
        "/usr",
        "/bin",
        "/sbin",
        "/System",
        "/Library",
        "/Users",
        "/home",
        "/opt",
        "/srv",
        "/mnt",
        "/Volumes",
        "/proc",
        "/sys",
        "/dev",
        "/run",
    ];
    let canon = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let s = canon.to_string_lossy();
    if s == "/" {
        return Err("不允许用文件系统根 / 作为工作区（agent 将获得全盘上下文）".into());
    }
    let hit = BROAD.iter().any(|b| {
        let bc = std::path::Path::new(b)
            .canonicalize()
            .unwrap_or_else(|_| std::path::Path::new(b).to_path_buf());
        bc == canon
    });
    if hit {
        return Err(format!(
            "目录过于宽泛（{s}）：请指定具体项目子目录，而非系统/用户根级目录"
        ));
    }
    if let Some(home) = dirs::home_dir() {
        let home_canon = home.canonicalize().unwrap_or(home);
        if canon == home_canon {
            return Err("不允许用 home 根目录作为工作区，请指定项目子目录".into());
        }
    }
    Ok(())
}
fn default_fragment_interval_ms() -> u64 {
    400
}
fn default_agent_timeout_secs() -> u64 {
    0
}
fn default_permission_ask_timeout_secs() -> u64 {
    300
}
fn default_ask_via_im_timeout_secs() -> u64 {
    1800
}
fn default_shutdown_grace_secs() -> u64 {
    60
}
fn default_agent_idle_timeout_secs() -> u64 {
    300
}
fn default_batch_window_ms() -> u64 {
    1500
}
fn default_auto_compact_threshold_tokens() -> u64 {
    120_000
}
fn default_acp_max_connections() -> usize {
    8
}
fn default_acp_idle_recycle_secs() -> u64 {
    600
}
fn default_feishu_asr_enabled() -> bool {
    true
}

impl Config {
    /// 默认配置文件路径：`<imagent_home>/config.toml`（P4-10：随 profile 隔离）。
    pub fn default_path() -> Option<PathBuf> {
        Some(crate::paths::imagent_home().join("config.toml"))
    }

    /// 读取并解析。文件不存在 => `CoreError::Config`。
    /// `default_workdir` 缺失或非绝对路径 => `CoreError::Config`（给出清晰提示）。
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(CoreError::Io)?;
        let mut cfg: Self = toml::from_str(&raw)
            .map_err(|e| CoreError::Config(format!("parse {}: {e}", path.display())))?;

        if cfg.default_workdir.as_os_str().is_empty() || !cfg.default_workdir.is_absolute() {
            return Err(CoreError::Config(format!(
                "default_workdir 必须是绝对路径，当前为 {:?}。请参考 EXAMPLE 模板。",
                cfg.default_workdir
            )));
        }

        // Wave B-10：default_workdir 失效探测（validate_workdir 含 is_dir 与过宽
        // 位置检查）——启动 **warn 不拒启**。取舍：目录可能由远程挂载/容器卷在
        // 启动后延迟就绪，拒启会把「暂时不可用」放大成「服务起不来」；运行期
        // run_round_inner 有同款 is_dir 预检兜底（目录恢复后下一轮自动可用）。
        if let Err(e) = validate_workdir(&cfg.default_workdir) {
            tracing::warn!(
                target: "imagent::core",
                workdir = %cfg.default_workdir.display(),
                reason = %e,
                "default_workdir 校验未通过（不拒启；每轮运行前有 is_dir 预检兜底）"
            );
        }

        // Wave B-4：quiet_hours 解析（格式错误启动期报错，防拼错静默失效）。
        cfg.quiet_hours_parsed = match cfg.quiet_hours.as_deref() {
            None => None,
            Some(raw) => Some(QuietHours::parse(raw).ok_or_else(|| {
                CoreError::Config(format!(
                    "quiet_hours 格式须为 \"HH:MM-HH:MM\"（如 \"22:00-08:00\"，可跨天），当前 {raw:?}"
                ))
            })?),
        };
        // Wave B-8：话题免 @ 窗口边界（0 = 关闭豁免；上限 24h 防拼错单位）。
        const THREAD_WINDOW_MAX_SECS: u64 = 86_400;
        if cfg.feishu_thread_active_window_secs > THREAD_WINDOW_MAX_SECS {
            return Err(CoreError::Config(format!(
                "feishu_thread_active_window_secs 上限 {THREAD_WINDOW_MAX_SECS}（当前 {}）；0 = 关闭话题免 @ 豁免",
                cfg.feishu_thread_active_window_secs
            )));
        }

        // W1-2：模型串 trim，空白视为未设置（防 `claude_model = ""` 附加空 flag）。
        cfg.claude_model = cfg
            .claude_model
            .take()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty());
        cfg.claude_fallback_model = cfg
            .claude_fallback_model
            .take()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty());
        if cfg.claude_fallback_model.is_some() && cfg.claude_model.is_none() {
            return Err(CoreError::Config(
                "claude_fallback_model 需要 claude_model 同时设置（fallback 仅在主模型指定时有意义）".into(),
            ));
        }
        // W1-4：系统提示 trim，空白视为未设置。
        cfg.append_system_prompt = cfg
            .append_system_prompt
            .take()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        // W1-3：mcp_config_path 启动期校验——存在 + 顶层 mcpServers 是对象。
        // 显式配置的文件坏了应当立即报错，而不是每轮 run 静默丢工具。
        if let Some(p) = cfg.mcp_config_path.take() {
            let raw = std::fs::read_to_string(&p).map_err(|e| {
                CoreError::Config(format!("mcp_config_path 无法读取（{}）：{e}", p.display()))
            })?;
            let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
                CoreError::Config(format!(
                    "mcp_config_path（{}）不是合法 JSON：{e}",
                    p.display()
                ))
            })?;
            match v.get("mcpServers").and_then(|m| m.as_object()) {
                Some(_) => cfg.mcp_config_path = Some(p),
                None => {
                    return Err(CoreError::Config(format!(
                        "mcp_config_path（{}）缺少顶层 mcpServers 对象（标准 .mcp.json 形态：{{\"mcpServers\": {{…}}}}）",
                        p.display()
                    )))
                }
            }
        }

        // P8-4：后端原生权限模式透传值归一（trim + 小写）；值域按后端校验——
        // claude 系白名单（manual 是 default 的 CLI 别名；未知值启动期报错防拼出
        // 非法 flag），其余后端先存值（backend 侧忽略并 warn，接入时再收紧）。
        if let Some(m) = cfg.backend_permission_mode.as_mut() {
            let raw = std::mem::take(m);
            let normalized = raw.trim().to_ascii_lowercase();
            if cfg.agent.starts_with("claude") {
                *m = normalize_claude_permission_mode(&normalized)?;
            } else {
                *m = normalized;
            }
        }

        // P5 快赢：数值下界/上限校验——错误前置到启动期，而非运行期静默劣化
        //（0 值超时 = 所有 run 瞬时失败/审批必拒；超大批窗口 = 每条回复显著延迟）。
        if cfg.permission_ask_timeout_secs == 0 {
            return Err(CoreError::Config(
                "permission_ask_timeout_secs 必须 ≥ 1（0 会让所有审批必然超时拒否）".into(),
            ));
        }
        // D8：审批等待预算必须小于 agent 总预算——慢审批不再挤占执行时间的前提；
        // 违反（≥）直接拒绝启动（此前仅注释建议，运行期才以超时形式暴露）。
        if cfg.agent_timeout_secs != 0 && cfg.permission_ask_timeout_secs >= cfg.agent_timeout_secs
        {
            return Err(CoreError::Config(format!(
                "permission_ask_timeout_secs（{}）必须小于 agent_timeout_secs（{}）：\
                 审批等待有独立预算，≥ 总预算会让慢审批撑满 agent 超时",
                cfg.permission_ask_timeout_secs, cfg.agent_timeout_secs
            )));
        }
        const ASK_VIA_IM_TIMEOUT_MAX_SECS: u64 = 86_400;
        if cfg.ask_via_im_timeout_secs == 0
            || cfg.ask_via_im_timeout_secs > ASK_VIA_IM_TIMEOUT_MAX_SECS
        {
            return Err(CoreError::Config(format!(
                "ask_via_im_timeout_secs 须在 1..={ASK_VIA_IM_TIMEOUT_MAX_SECS}（当前 {}）",
                cfg.ask_via_im_timeout_secs
            )));
        }
        if let Some(conv) = cfg.ask_via_im_conv.as_deref() {
            let c = conv.trim();
            if !c.starts_with("feishu:ou_") && !c.starts_with("feishu:oc_") {
                return Err(CoreError::Config(format!(
                    "ask_via_im_conv 须为飞书会话 id（feishu:ou_xxx 私聊 / feishu:oc_xxx 群），当前 {c:?}"
                )));
            }
        }
        if cfg.shutdown_grace_secs == 0 {
            return Err(CoreError::Config(
                "shutdown_grace_secs 必须 ≥ 1（0 会让退出 drain 立即 abort 在飞任务）".into(),
            ));
        }
        const BATCH_WINDOW_MAX_MS: u64 = 10_000;
        if cfg.batch_window_ms > BATCH_WINDOW_MAX_MS {
            return Err(CoreError::Config(format!(
                "batch_window_ms 上限 {BATCH_WINDOW_MAX_MS}（当前 {}）——超大窗口会让每条回复都延迟一个窗口",
                cfg.batch_window_ms
            )));
        }
        // W2-5：自动 compact 阈值边界（0 = 关闭；非 0 须 ≥ 10k，防把阈值配成
        // 每轮必压的极小值）。
        const AUTO_COMPACT_MIN: u64 = 10_000;
        if cfg.auto_compact_threshold_tokens != 0
            && cfg.auto_compact_threshold_tokens < AUTO_COMPACT_MIN
        {
            return Err(CoreError::Config(format!(
                "auto_compact_threshold_tokens 须为 0（关闭）或 ≥ {AUTO_COMPACT_MIN}（当前 {}）",
                cfg.auto_compact_threshold_tokens
            )));
        }
        // W2-4：ACP 连接参数边界。
        if cfg.acp_max_connections == 0 {
            return Err(CoreError::Config(
                "acp_max_connections 必须 ≥ 1（0 会让所有 claude-acp 会话被拒绝）".into(),
            ));
        }
        const ACP_IDLE_MIN_SECS: u64 = 60;
        if cfg.acp_idle_recycle_secs < ACP_IDLE_MIN_SECS {
            return Err(CoreError::Config(format!(
                "acp_idle_recycle_secs 须 ≥ {ACP_IDLE_MIN_SECS}（当前 {}；过短会让连接频繁重建，失去长驻意义）",
                cfg.acp_idle_recycle_secs
            )));
        }

        Ok(cfg)
    }

    /// P5-7（安全）：组合探测——`allowed_chats`（群放行）非空但 `admin_senders`
    /// 为空。S2 收紧后 admins 为空 = **无人**是管理员（不再「全员可管」），组合
    /// 的危害已从「群内任意成员具备管理能力」降为「群部署下管理命令完全不可用
    /// （含 /chat deny 收回群授权）」——保留探测供启动告警提醒补配 admin_senders。
    pub fn admin_gap_with_chat_allowlist(&self) -> bool {
        !self.allowed_chats.is_empty() && self.admin_senders.is_empty()
    }

    /// 供首次使用打印的模板字符串（default_workdir 用占位，不写死任何机器路径）。
    pub const EXAMPLE: &'static str = r#"# ~/.imagent/config.toml
default_workdir = "/absolute/path/to/agent/workspace"   # 必填，agent 的 cwd（非沙箱：不限制可读路径，靠 allowed_tools + permission_mode 兜底）
allowed_senders = []        # 留空 = 发现模式（只打日志记录入站 sender，不驱动 agent）
# allowed_chats = ["feishu:oc_xxx"]   # 会话(群)白名单：群消息 chat 放行 OR sender 放行即过（/chat 可动态管理）
# admin_senders = []          # 可 /allow 等管理命令的管理员 sender；空=无人是管理员(IM 内管理命令不可用，须 CLI/setup 配置)
# allowed_tools = ["*"]                      # 缺省=全部工具（不收敛）；要白名单显式列（如 ["Read","Edit"]）；执行类建议配 permission_mode="ask"
agent = "claude-cli"         # claude-cli(默认) | claude-acp(ACP长驻子进程) | codex | gemini
platform = "ilink"   # ilink(默认,扫码登录) | wecom(企业微信机器人) | feishu(飞书,配 feishu_app_id + 环境变量 IMAGENT_FEISHU_APP_SECRET)
# feishu_app_id = "cli_xxx"            # 飞书自建应用 app_id（仅 platform="feishu"；app_secret 走环境变量，keyring 为后续 P2）
# feishu_base_url = "https://open.feishu.cn"  # 可选，默认 https://open.feishu.cn；Lark 国际版 https://open.larksuite.com（MVP 不覆盖）
# feishu_require_mention_in_group = true       # 群消息须 @bot 才处理（默认 true：客户端过滤 + 剥离 @bot 占位；false=全收，过滤交给事件订阅 scope）
# stranger_mention_hint = false                # 未放行群里被 @ 时回一句引导（默认 false 完全静默防探测；私聊始终静默）
# stranger_p2p_hint = true                     # 未放行用户的私聊回引导（默认 true：私聊是主动来找 bot 的，无探测面；含 sender id 与 /allow 指引）
# reply_mode = "card"                          # 回复形态：card(默认,流式卡片) | text(纯文本)；/config 可热改
# quiet_hours = "22:00-08:00"                  # 免打扰时段(本地时区,可跨天)：时段内加急(buzz)提醒降级为普通消息，内容不变；不设=不启用，改动需重启
# feishu_thread_active_window_secs = 1800      # 话题群免@窗口(秒,仅feishu)：话题内近期有消息则豁免群消息须@bot；默认30分钟，0=关闭，改动需重启
permission_mode = "auto"    # 缺省=auto：claude-cli=透传 claude 原生 auto 模式(分类器自动放行安全操作,高危进 IM)+审批闭环；其余后端=off；显式 ask=每个提示都进 IM
# backend_permission_mode = "auto" # 后端原生权限模式透传(claude→--permission-mode；可覆盖 auto 档缺省)：default|acceptEdits|plan|auto|dontAsk|bypassPermissions；codex/gemini 暂不支持(warn 忽略)
# approval_tools = ["Bash", "WebFetch", "mcp__*"]  # 审批集：ask 模式下只有这些工具过 IM 审批，其余直接放行；空=全部过审
# metrics_addr = "127.0.0.1:9100"   # 默认关闭；设为 "ip:port" 开启 /metrics + /health HTTP server
# message_max_len = 2000              # 单条出站消息字符上限（Unicode char，三平台生效：ilink/feishu/wecom 各与自身协议上限取 min）；不设 = 仅用平台协议上限
# message_fragment_interval_ms = 400  # 分片间发送间隔（ms）
# agent_timeout_secs = 0              # 单次运行总超时(秒)；0=关闭(默认，防挂死靠 idle 看门狗)；设为正数即硬上限
# agent_idle_timeout_secs = 300       # 空闲看门狗(秒)：连续无输出则终止本轮；0=关闭
# batch_window_ms = 1500              # 批处理窗口(ms)：连发消息合并为一轮 prompt；0=关闭
# cot_detail = "brief"                # 工具过程展示：off | brief(默认) | detailed（/config 可热改）
# permission_ask_timeout_secs = 300   # Ask 模式等用户回复超时(秒，独立预算，不挤占 agent 超时)
# ask_via_im_conv = "feishu:ou_xxx"   # 终端 agent 的 ask_via_im 工具投递目标会话（设了才启用；配合 `imagent mcp-ask` 挂到任意终端 agent 的 MCP 配置）
# ask_via_im_timeout_secs = 1800      # ask_via_im 等待回复超时(秒，默认 30 分钟，可被调用覆盖)
# shutdown_grace_secs = 60            # 优雅退出 drain 宽限(秒)；超时 abort 在飞 task
# require_keyring = false        # 默认 false(headless 明文回退+warn); true=keyring 不可用时拒绝明文落盘(fail-closed，安全部署建议)
# claude_model = "sonnet"        # claude 模型（--model；不设 = CLI 默认）；/model 命令可运行时热改（重启恢复本值）
# claude_fallback_model = "haiku"  # fallback 模型（--fallback-model；须与 claude_model 同时设置）
# disallowed_tools = ["WebSearch"]  # 禁用工具黑名单（--disallowedTools；与 allowed_tools 独立，黑名单优先）
# mcp_config_path = "~/.claude/mcp.json"  # 用户 MCP servers 合并进每次 run（给 agent 挂工具；imagent 条目名保留）
# append_system_prompt = "你在飞书里服务团队，回复保持简洁中文"  # 附加系统提示（--append-system-prompt）
# auto_compact_threshold_tokens = 120000  # 自动 compact：上下文水位超阈值自动压缩（对齐 Claude Code auto-compact；0=关闭）
# acp_max_connections = 8      # claude-acp 并发连接上限（仅 agent="claude-acp"）
# acp_idle_recycle_secs = 600  # claude-acp 连接空闲回收（秒；仅 agent="claude-acp"）
# feishu_asr_enabled = true     # 飞书语音转文字（需后台申请语音识别权限；失败回退提示，仅 feishu）
"#;
}
/// claude `--permission-mode` 合法值归一（入参已小写化）：manual→default
///（CLI 别名）；未知值 Err（启动期失败，防拼出非法 flag）。
fn normalize_claude_permission_mode(raw: &str) -> Result<String> {
    let m = if raw == "manual" {
        "default".to_string()
    } else {
        raw.to_string()
    };
    match m.as_str() {
        "default" | "acceptedits" | "plan" | "auto" | "dontask" | "bypasspermissions" => {
            Ok(m)
        }
        _ => Err(CoreError::Config(format!(
            "claude_permission_mode 非法值 {raw:?}：可用 default(manual) | acceptEdits | plan | auto | dontAsk | bypassPermissions"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_path(name: &str, body: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "imagent_core_cfg_{}_{}.toml",
            std::process::id(),
            name
        ));
        let _ = std::fs::File::create(&p).and_then(|mut f| f.write_all(body.as_bytes()));
        p
    }

    fn cleanup(p: &Path) {
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn permission_mode_default_auto_and_resolve() {
        let p = tmp_path(
            "cfg_perm_default",
            "default_workdir = \"/tmp/ws\"\nallowed_tools = [\"Read\"]\n",
        );
        let cfg = Config::load(&p).expect("parse");
        // 2026-08 起缺省 auto；resolve 按 backend 选档（claude-cli → AutoClaude：
        // 原生 acceptEdits + IM 闭环；其余 → Off）。
        assert_eq!(cfg.permission_mode, PermissionMode::Auto);
        assert_eq!(
            PermissionMode::Auto.resolve("claude-cli"),
            PermissionMode::AutoClaude
        );
        assert_eq!(PermissionMode::Auto.resolve("codex"), PermissionMode::Off);
        assert_eq!(PermissionMode::Auto.resolve("gemini"), PermissionMode::Off);
        assert_eq!(
            PermissionMode::Auto.resolve("claude-acp"),
            PermissionMode::Off
        );
        // 具体档原样透传；未解析的 Auto 按未接线（is_enabled=false）。
        assert_eq!(PermissionMode::Ask.resolve("codex"), PermissionMode::Ask);
        assert!(!PermissionMode::Auto.is_enabled());
        assert!(PermissionMode::Ask.is_enabled());
        // P8-4：AutoClaude 是运行时档——enabled/needs_socket/as_str/from_str 往返；
        // 配置面 serde 拒绝直写。
        assert!(PermissionMode::AutoClaude.is_enabled());
        assert!(PermissionMode::AutoClaude.needs_socket());
        assert!(!PermissionMode::Auto.needs_socket());
        assert_eq!(PermissionMode::AutoClaude.as_str(), "auto-claude");
        assert_eq!(
            PermissionMode::from_str_lossy("auto-claude"),
            PermissionMode::AutoClaude
        );
        let p2 = tmp_path(
            "cfg_perm_auto_edits_reject",
            "default_workdir = \"/tmp/ws\"\npermission_mode = \"auto-claude\"\n",
        );
        assert!(
            Config::load(&p2).is_err(),
            "auto-claude 是运行时档，配置面不可直写"
        );
        cleanup(&p2);
        cleanup(&p);
    }

    #[test]
    fn permission_mode_parses() {
        for (raw, expect) in [
            ("ask", PermissionMode::Ask),
            ("allow", PermissionMode::Allow),
            ("deny", PermissionMode::Deny),
            ("off", PermissionMode::Off),
            ("auto", PermissionMode::Auto),
        ] {
            let p = tmp_path(
                "cfg_perm",
                &format!("default_workdir = \"/tmp/ws\"\npermission_mode = \"{raw}\"\n"),
            );
            let cfg = Config::load(&p).expect("parse");
            assert_eq!(cfg.permission_mode, expect, "raw={raw}");
            cleanup(&p);
        }
    }

    #[test]
    fn parses_full() {
        let p = tmp_path(
            "full",
            r#"default_workdir = "/tmp/ws"
allowed_senders = ["u1"]
allowed_tools = ["Read"]
agent = "claude-cli"
platform = "ilink"
"#,
        );
        let cfg = Config::load(&p).expect("ok");
        assert_eq!(cfg.allowed_senders, vec!["u1".to_string()]);
        assert_eq!(cfg.allowed_tools, vec!["Read".to_string()]);
        assert_eq!(cfg.agent, "claude-cli");
        cleanup(&p);
    }

    #[test]
    fn applies_defaults() {
        let p = tmp_path("def", r#"default_workdir = "/tmp/ws""#);
        let cfg = Config::load(&p).expect("ok");
        assert!(cfg.allowed_senders.is_empty());
        // 2026-08 缺省语义：不写 allowed_tools = 全部工具（["*"]）。
        assert_eq!(cfg.allowed_tools, vec!["*".to_string()]);
        assert_eq!(cfg.platform, "ilink");
        assert_eq!(cfg.agent, "claude-cli");
        assert_eq!(cfg.platform, "ilink");
        cleanup(&p);
    }

    #[test]
    fn rejects_relative_workdir() {
        let p = tmp_path("rel", r#"default_workdir = "relative/path""#);
        let err = Config::load(&p).unwrap_err();
        assert!(matches!(err, CoreError::Config(_)), "{err:?}");
        cleanup(&p);
    }

    #[test]
    fn missing_file_is_err() {
        let mut nope = std::env::temp_dir();
        nope.push("imagent_core_cfg_does_not_exist.toml");
        let err = Config::load(&nope).unwrap_err();
        assert!(matches!(err, CoreError::Io(_)), "{err:?}");
    }

    #[test]
    fn metrics_addr_defaults_to_none() {
        let p = tmp_path("metrics_def", r#"default_workdir = "/tmp/ws""#);
        let cfg = Config::load(&p).expect("parse");
        assert_eq!(cfg.metrics_addr, None);
        cleanup(&p);
    }

    #[test]
    fn metrics_addr_empty_disables() {
        let p = tmp_path(
            "metrics_empty",
            r#"default_workdir = "/tmp/ws"
metrics_addr = ""
"#,
        );
        let cfg = Config::load(&p).expect("parse");
        // 解析得到空串；main 侧把 None / 空串都视为关闭。
        assert_eq!(cfg.metrics_addr.as_deref(), Some(""));
        cleanup(&p);
    }

    #[test]
    fn metrics_addr_custom() {
        let p = tmp_path(
            "metrics_custom",
            r#"default_workdir = "/tmp/ws"
metrics_addr = "0.0.0.0:9999"
"#,
        );
        let cfg = Config::load(&p).expect("parse");
        assert_eq!(cfg.metrics_addr.as_deref(), Some("0.0.0.0:9999"));
        cleanup(&p);
    }

    #[test]
    fn message_max_len_default_none() {
        let p = tmp_path("msg_def", r#"default_workdir = "/tmp/ws""#);
        let cfg = Config::load(&p).expect("parse");
        assert_eq!(cfg.message_max_len, None);
        cleanup(&p);
    }

    /// 私聊陌生人引导：默认 true（与群内 stranger_mention_hint 默认 false 相反——
    /// 私聊无探测面）；显式 false 可关闭。
    #[test]
    fn stranger_p2p_hint_default_true_and_custom() {
        let p = tmp_path("p2p_def", r#"default_workdir = "/tmp/ws""#);
        let cfg = Config::load(&p).expect("parse");
        assert!(cfg.stranger_p2p_hint, "私聊引导默认开启");
        assert!(!cfg.stranger_mention_hint, "群内提示默认静默");
        cleanup(&p);
        let p = tmp_path(
            "p2p_off",
            r#"default_workdir = "/tmp/ws"
stranger_p2p_hint = false
"#,
        );
        let cfg = Config::load(&p).expect("parse");
        assert!(!cfg.stranger_p2p_hint);
        cleanup(&p);
    }

    #[test]
    fn message_max_len_custom() {
        let p = tmp_path(
            "msg_custom",
            r#"default_workdir = "/tmp/ws"
message_max_len = 100
"#,
        );
        let cfg = Config::load(&p).expect("parse");
        assert_eq!(cfg.message_max_len, Some(100));
        cleanup(&p);
    }

    #[test]
    fn fragment_interval_default_400() {
        let p = tmp_path("frag_def", r#"default_workdir = "/tmp/ws""#);
        let cfg = Config::load(&p).expect("parse");
        assert_eq!(cfg.message_fragment_interval_ms, 400);
        cleanup(&p);
    }

    #[test]
    fn fragment_interval_custom() {
        let p = tmp_path(
            "frag_custom",
            r#"default_workdir = "/tmp/ws"
message_fragment_interval_ms = 250
"#,
        );
        let cfg = Config::load(&p).expect("parse");
        assert_eq!(cfg.message_fragment_interval_ms, 250);
        cleanup(&p);
    }

    #[test]
    fn require_keyring_default_false() {
        let p = tmp_path("reqkr_def", r#"default_workdir = "/tmp/ws""#);
        let cfg = Config::load(&p).expect("parse");
        assert!(!cfg.require_keyring);
        cleanup(&p);
    }

    #[test]
    fn require_keyring_parses_true() {
        let p = tmp_path(
            "reqkr_true",
            "default_workdir = \"/tmp/ws\"\nrequire_keyring = true\n",
        );
        let cfg = Config::load(&p).expect("parse");
        assert!(cfg.require_keyring);
        cleanup(&p);
    }

    #[test]
    fn cot_detail_default_brief_and_parse() {
        // 默认 brief。
        let p = tmp_path("cot_def", r#"default_workdir = "/tmp/ws""#);
        let cfg = Config::load(&p).expect("parse");
        assert_eq!(cfg.cot_detail, CotDetail::Brief);
        assert_eq!(cfg.cot_detail.input_trunc(), 80);
        assert_eq!(cfg.cot_detail.max_tools(), 5);
        cleanup(&p);
        // 三档解析 + 档位参数。
        for (raw, expect) in [
            ("off", CotDetail::Off),
            ("brief", CotDetail::Brief),
            ("detailed", CotDetail::Detailed),
        ] {
            let p = tmp_path(
                "cot_parse",
                &format!("default_workdir = \"/tmp/ws\"\ncot_detail = \"{raw}\"\n"),
            );
            let cfg = Config::load(&p).expect("parse");
            assert_eq!(cfg.cot_detail, expect, "raw={raw}");
            cleanup(&p);
        }
        assert_eq!(CotDetail::Detailed.input_trunc(), 240);
        assert_eq!(CotDetail::Detailed.max_tools(), 10);
        assert_eq!(CotDetail::Off.input_trunc(), 0);
        // 非法值 lossy 解析为 None（/config 输入校验用）。
        assert!(CotDetail::from_str_lossy("bogus").is_none());
    }

    #[test]
    fn allowed_chats_default_empty_and_parse() {
        let p = tmp_path("chats_def", r#"default_workdir = "/tmp/ws""#);
        let cfg = Config::load(&p).expect("parse");
        assert!(cfg.allowed_chats.is_empty());
        cleanup(&p);
        let p = tmp_path(
            "chats_parse",
            "default_workdir = \"/tmp/ws\"\nallowed_chats = [\"feishu:oc_a\", \"feishu:oc_b\"]\n",
        );
        let cfg = Config::load(&p).expect("parse");
        assert_eq!(cfg.allowed_chats.len(), 2);
        cleanup(&p);
    }

    #[test]
    fn idle_timeout_and_batch_window_default() {
        let p = tmp_path("idle_def", r#"default_workdir = "/tmp/ws""#);
        let cfg = Config::load(&p).expect("parse");
        assert_eq!(cfg.agent_idle_timeout_secs, 300);
        assert_eq!(cfg.batch_window_ms, 1500);
        cleanup(&p);
    }

    #[test]
    fn idle_timeout_and_batch_window_custom() {
        let p = tmp_path(
            "idle_custom",
            "default_workdir = \"/tmp/ws\"\nagent_idle_timeout_secs = 90\nbatch_window_ms = 0\n",
        );
        let cfg = Config::load(&p).expect("parse");
        assert_eq!(cfg.agent_idle_timeout_secs, 90);
        assert_eq!(cfg.batch_window_ms, 0);
        cleanup(&p);
    }

    /// P5 快赢：数值边界校验——0 值超时/超大批窗口在启动期报错。
    #[test]
    fn numeric_bounds_rejected() {
        for (tag, extra) in [
            ("ask0", "permission_ask_timeout_secs = 0\n"),
            ("grace0", "shutdown_grace_secs = 0\n"),
            ("window_huge", "batch_window_ms = 600000\n"),
        ] {
            let p = tmp_path(tag, &format!("default_workdir = \"/tmp/ws\"\n{extra}"));
            let err = Config::load(&p).expect_err("越界配置应报错");
            assert!(
                format!("{err}").contains("必须") || format!("{err}").contains("上限"),
                "应给出清晰指引: {err}"
            );
            cleanup(&p);
        }
        // 合法边界：批窗口上限值本身可用；idle 0 = 关闭（文档语义）。
        let p = tmp_path(
            "bounds_ok",
            "default_workdir = \"/tmp/ws\"\nbatch_window_ms = 10000\nagent_idle_timeout_secs = 0\n",
        );
        assert!(Config::load(&p).is_ok());
        cleanup(&p);
        // agent_timeout_secs = 0 = 关闭总超时（默认；防挂死靠 idle 看门狗）。
        let p = tmp_path(
            "bounds_timeout_off",
            "default_workdir = \"/tmp/ws\"\nagent_timeout_secs = 0\npermission_ask_timeout_secs = 300\n",
        );
        assert!(Config::load(&p).is_ok());
        cleanup(&p);
    }

    /// D8：permission_ask_timeout_secs 必须 < agent_timeout_secs，违反拒绝启动。
    #[test]
    fn permission_ask_timeout_must_be_less_than_agent_timeout() {
        // 违反：等于 / 大于都拒绝。
        for (tag, extra) in [
            (
                "eq",
                "agent_timeout_secs = 300\npermission_ask_timeout_secs = 300\n",
            ),
            (
                "gt",
                "agent_timeout_secs = 60\npermission_ask_timeout_secs = 300\n",
            ),
        ] {
            let p = tmp_path(tag, &format!("default_workdir = \"/tmp/ws\"\n{extra}"));
            let err = Config::load(&p).expect_err("违反预算关系应拒绝启动");
            assert!(
                format!("{err}").contains("必须小于"),
                "应说明预算关系: {err}"
            );
            cleanup(&p);
        }
        // 合法：显式小于通过；默认（agent_timeout=0 关闭）不适用该约束。
        let p = tmp_path("lt_ok", "default_workdir = \"/tmp/ws\"\nagent_timeout_secs = 301\npermission_ask_timeout_secs = 300\n");
        assert!(Config::load(&p).is_ok());
        let p = tmp_path("off_ok", "default_workdir = \"/tmp/ws\"\n");
        assert!(Config::load(&p).is_ok());
        assert_eq!(default_agent_timeout_secs(), 0);
        cleanup(&p);
    }

    /// ask_via_im 配置：默认未启用；conv 前缀与超时边界校验。
    #[test]
    fn ask_via_im_config_default_and_validation() {
        let p = tmp_path("askvi_def", r#"default_workdir = "/tmp/ws""#);
        let cfg = Config::load(&p).expect("parse");
        assert_eq!(cfg.ask_via_im_conv, None);
        assert_eq!(cfg.ask_via_im_timeout_secs, 1800);
        cleanup(&p);
        // 合法：飞书 conv + 自定义超时。
        let p = tmp_path(
            "askvi_ok",
            "default_workdir = \"/tmp/ws\"\nask_via_im_conv = \"feishu:ou_x\"\nask_via_im_timeout_secs = 60\n",
        );
        assert!(Config::load(&p).is_ok());
        cleanup(&p);
        // 非法 conv / 超时越界。
        for (tag, extra) in [
            ("askvi_conv_bad", "ask_via_im_conv = \"wecom:x\"\n"),
            ("askvi_t0", "ask_via_im_timeout_secs = 0\n"),
            ("askvi_huge", "ask_via_im_timeout_secs = 100000\n"),
        ] {
            let p = tmp_path(tag, &format!("default_workdir = \"/tmp/ws\"\n{extra}"));
            assert!(Config::load(&p).is_err(), "{tag} 应报错");
            cleanup(&p);
        }
    }

    /// P5-7：群放行 + admin_senders 为空的组合探测（群内全员 = 事实管理员）。
    #[test]
    fn admin_gap_with_chat_allowlist_detects_combo() {
        // 组合命中：有群放行、无管理员。
        let p = tmp_path(
            "gap_hit",
            "default_workdir = \"/tmp/ws\"\nallowed_chats = [\"feishu:oc_a\"]\n",
        );
        let cfg = Config::load(&p).expect("parse");
        assert!(cfg.admin_gap_with_chat_allowlist());
        cleanup(&p);
        // 设了管理员 → 不告警。
        let p = tmp_path(
            "gap_admin_set",
            "default_workdir = \"/tmp/ws\"\nallowed_chats = [\"feishu:oc_a\"]\nadmin_senders = [\"me\"]\n",
        );
        let cfg = Config::load(&p).expect("parse");
        assert!(!cfg.admin_gap_with_chat_allowlist());
        cleanup(&p);
        // 无群放行：空 admins 是单用户既有语义，不告警。
        let p = tmp_path("gap_no_chat", r#"default_workdir = "/tmp/ws""#);
        let cfg = Config::load(&p).expect("parse");
        assert!(!cfg.admin_gap_with_chat_allowlist());
        cleanup(&p);
    }

    /// P8-4：backend_permission_mode 校验与归一（manual→default；未知值启动期报错）。
    #[test]
    fn backend_permission_mode_validate_and_normalize() {
        let p_ok = tmp_path(
            "cfg_claude_perm_ok",
            "default_workdir = \"/tmp/ws\"\nbackend_permission_mode = \"Manual\"\n",
        );
        let cfg = Config::load(&p_ok).expect("parse");
        assert_eq!(cfg.backend_permission_mode.as_deref(), Some("default"));
        cleanup(&p_ok);

        for v in [
            "acceptEdits",
            "plan",
            "auto",
            "dontAsk",
            "bypassPermissions",
        ] {
            let p = tmp_path(
                &format!("cfg_claude_perm_{v}"),
                &format!("default_workdir = \"/tmp/ws\"\nbackend_permission_mode = \"{v}\"\n"),
            );
            let cfg = Config::load(&p).unwrap_or_else(|_| panic!("{v} 应合法"));
            assert_eq!(
                cfg.backend_permission_mode.as_deref(),
                Some(v.to_ascii_lowercase().as_str())
            );
            cleanup(&p);
        }

        let p_bad = tmp_path(
            "cfg_claude_perm_bad",
            "default_workdir = \"/tmp/ws\"\nbackend_permission_mode = \"yolo\"\n",
        );
        assert!(Config::load(&p_bad).is_err(), "未知值应启动期报错");
        cleanup(&p_bad);

        // 缺省 None。
        let p_def = tmp_path("cfg_claude_perm_def", "default_workdir = \"/tmp/ws\"\n");
        assert_eq!(Config::load(&p_def).unwrap().backend_permission_mode, None);
        cleanup(&p_def);
    }

    #[test]
    fn approval_tools_default_empty_and_parse() {
        let p = tmp_path("appr_def", r#"default_workdir = "/tmp/ws""#);
        let cfg = Config::load(&p).expect("parse");
        assert!(cfg.approval_tools.is_empty(), "缺省空 = 全部过审");
        cleanup(&p);
        let p = tmp_path(
            "appr_parse",
            "default_workdir = \"/tmp/ws\"\napproval_tools = [\"Bash\", \"mcp__*\"]\n",
        );
        let cfg = Config::load(&p).expect("parse");
        assert_eq!(cfg.approval_tools.len(), 2);
        cleanup(&p);
    }

    /// Wave B-4：quiet_hours 解析与跨天窗口判定（纯函数）。
    #[test]
    fn quiet_hours_parse_and_contains() {
        // 跨天窗口：22:00-08:00 → 22:00 起到次日 08:00 前。
        let q = QuietHours::parse("22:00-08:00").expect("合法格式");
        assert!(q.contains(22 * 60)); // 起点含
        assert!(q.contains(23 * 60 + 59));
        assert!(q.contains(0)); // 次日 00:00 在窗口内
        assert!(q.contains(7 * 60 + 59));
        assert!(!q.contains(8 * 60)); // 终点不含
        assert!(!q.contains(12 * 60));
        assert!(!q.contains(21 * 60 + 59));
        assert_eq!(q.display(), "22:00-08:00");
        // 常规窗口：09:00-18:00。
        let q = QuietHours::parse("09:00-18:00").expect("合法格式");
        assert!(q.contains(9 * 60));
        assert!(q.contains(17 * 60 + 59));
        assert!(!q.contains(18 * 60));
        assert!(!q.contains(8 * 60 + 59));
        // start == end：空窗口（无效果）。
        assert!(!QuietHours::parse("22:00-22:00").unwrap().contains(22 * 60));
        // 非法格式。
        for bad in [
            "22:00",
            "22:00-",
            "-08:00",
            "25:00-08:00",
            "22:60-08:00",
            "a:b-c:d",
            "",
            " ",
            "22:00-08:00-06:00",
        ] {
            assert!(QuietHours::parse(bad).is_none(), "应拒绝: {bad:?}");
        }
        // 首尾空白容忍。
        assert!(QuietHours::parse(" 22:00-08:00 ").is_some());
    }

    /// Wave B-4：config 面——quiet_hours 缺省 None；合法解析进 quiet_hours_parsed；
    /// 非法格式启动期报错。
    #[test]
    fn quiet_hours_config_default_parse_and_reject() {
        let p = tmp_path("quiet_def", r#"default_workdir = "/tmp/ws""#);
        let cfg = Config::load(&p).expect("ok");
        assert_eq!(cfg.quiet_hours, None);
        assert_eq!(cfg.quiet_hours_parsed, None);
        cleanup(&p);
        let p = tmp_path(
            "quiet_ok",
            "default_workdir = \"/tmp/ws\"\nquiet_hours = \"22:00-08:00\"\n",
        );
        let cfg = Config::load(&p).expect("ok");
        assert_eq!(cfg.quiet_hours.as_deref(), Some("22:00-08:00"));
        assert_eq!(
            cfg.quiet_hours_parsed.map(|q| q.display()),
            Some("22:00-08:00".to_string())
        );
        assert!(cfg.quiet_hours_parsed.unwrap().contains(23 * 60));
        cleanup(&p);
        for (tag, raw) in [
            ("bad_fmt", "quiet_hours = \"22-08\"\n"),
            ("bad_hour", "quiet_hours = \"24:00-08:00\"\n"),
        ] {
            let p = tmp_path(tag, &format!("default_workdir = \"/tmp/ws\"\n{raw}"));
            assert!(Config::load(&p).is_err(), "{tag} 应报错");
            cleanup(&p);
        }
    }

    /// Wave B-8：话题免 @ 窗口——默认 1800；可自定义（0 = 关闭）；超 24h 拒绝。
    #[test]
    fn thread_active_window_default_custom_and_bounds() {
        let p = tmp_path("tw_def", r#"default_workdir = "/tmp/ws""#);
        let cfg = Config::load(&p).expect("ok");
        assert_eq!(cfg.feishu_thread_active_window_secs, 1800);
        cleanup(&p);
        let p = tmp_path(
            "tw_custom",
            "default_workdir = \"/tmp/ws\"\nfeishu_thread_active_window_secs = 600\n",
        );
        let cfg = Config::load(&p).expect("ok");
        assert_eq!(cfg.feishu_thread_active_window_secs, 600);
        cleanup(&p);
        // 0 = 关闭豁免；上限 86400 本身合法。
        let p = tmp_path(
            "tw_zero",
            "default_workdir = \"/tmp/ws\"\nfeishu_thread_active_window_secs = 0\n",
        );
        assert!(Config::load(&p).is_ok());
        cleanup(&p);
        let p = tmp_path(
            "tw_huge",
            "default_workdir = \"/tmp/ws\"\nfeishu_thread_active_window_secs = 90000\n",
        );
        assert!(Config::load(&p).is_err(), "超 24h 应报错");
        cleanup(&p);
    }

    /// Wave B-10：default_workdir 失效（不存在）只 warn 不拒启——load 仍 Ok，
    /// 运行期预检兜底（取舍见 load 内注释）。
    #[test]
    fn invalid_workdir_warns_but_loads() {
        let p = tmp_path(
            "wd_gone",
            "default_workdir = \"/definitely/not/exist/ws\"\n",
        );
        let cfg = Config::load(&p).expect("目录失效应 warn 而非拒启");
        assert_eq!(
            cfg.default_workdir,
            PathBuf::from("/definitely/not/exist/ws")
        );
        cleanup(&p);
    }

    /// W1-2/W1-3/W1-4：claude 运行参数配置——默认值、trim、fallback 约束与
    /// mcp_config_path 校验。
    #[test]
    fn claude_runtime_opts_defaults_validation_and_mcp_path() {
        // 默认：全部未设置。
        let p = tmp_path("w1_def", r#"default_workdir = "/tmp/ws""#);
        let cfg = Config::load(&p).expect("ok");
        assert_eq!(cfg.claude_model, None);
        assert_eq!(cfg.claude_fallback_model, None);
        assert!(cfg.disallowed_tools.is_empty());
        assert_eq!(cfg.mcp_config_path, None);
        assert_eq!(cfg.append_system_prompt, None);
        cleanup(&p);

        // 模型/fallback/黑名单/系统提示正常解析 + 空白 trim 为 None。
        let p = tmp_path(
            "w1_parse",
            r#"default_workdir = "/tmp/ws"
claude_model = " sonnet "
claude_fallback_model = "haiku"
disallowed_tools = ["WebSearch", "Bash"]
append_system_prompt = " 你在飞书里服务团队 "
"#,
        );
        let cfg = Config::load(&p).expect("ok");
        assert_eq!(cfg.claude_model.as_deref(), Some("sonnet"));
        assert_eq!(cfg.claude_fallback_model.as_deref(), Some("haiku"));
        assert_eq!(cfg.disallowed_tools.len(), 2);
        assert_eq!(
            cfg.append_system_prompt.as_deref(),
            Some("你在飞书里服务团队")
        );
        cleanup(&p);

        let p = tmp_path(
            "w1_blank",
            r#"default_workdir = "/tmp/ws"
claude_model = "   "
append_system_prompt = ""
"#,
        );
        let cfg = Config::load(&p).expect("空白视同未设置");
        assert_eq!(cfg.claude_model, None);
        assert_eq!(cfg.append_system_prompt, None);
        cleanup(&p);

        // fallback 单独设置 → 启动期报错。
        let p = tmp_path(
            "w1_fb_only",
            "default_workdir = \"/tmp/ws\"\nclaude_fallback_model = \"haiku\"\n",
        );
        assert!(Config::load(&p).is_err(), "fallback 须与主模型同时设置");
        cleanup(&p);

        // mcp_config_path：合法文件（顶层 mcpServers 对象）通过；缺失/坏 JSON/
        // 缺 mcpServers 启动期报错。
        let dir = std::env::temp_dir();
        let good = dir.join(format!("imagent_mcp_good_{}.json", std::process::id()));
        std::fs::write(&good, r#"{"mcpServers":{"fetch":{"command":"uvx"}}}"#).unwrap();
        let p = tmp_path(
            "w1_mcp_ok",
            &format!(
                "default_workdir = \"/tmp/ws\"\nmcp_config_path = {:?}\n",
                good.display()
            ),
        );
        let cfg = Config::load(&p).expect("合法 mcp 配置应通过");
        assert_eq!(cfg.mcp_config_path.as_deref(), Some(good.as_path()));
        cleanup(&p);
        let _ = std::fs::remove_file(&good);

        let missing = dir.join("imagent_mcp_definitely_missing.json");
        let p = tmp_path(
            "w1_mcp_missing",
            &format!(
                "default_workdir = \"/tmp/ws\"\nmcp_config_path = {:?}\n",
                missing.display()
            ),
        );
        assert!(Config::load(&p).is_err(), "文件不存在应报错");
        cleanup(&p);

        let bad = dir.join(format!("imagent_mcp_bad_{}.json", std::process::id()));
        std::fs::write(&bad, r#"{"mcpServers": "not-an-object"}"#).unwrap();
        let p = tmp_path(
            "w1_mcp_bad",
            &format!(
                "default_workdir = \"/tmp/ws\"\nmcp_config_path = {:?}\n",
                bad.display()
            ),
        );
        assert!(Config::load(&p).is_err(), "mcpServers 非对象应报错");
        cleanup(&p);
        let _ = std::fs::remove_file(&bad);
    }

    /// P6-8：工作目录安全校验——过宽位置拒绝、正常项目目录放行。
    #[test]
    fn validate_workdir_rejects_broad() {
        // 相对路径 / 不存在。
        assert!(validate_workdir(Path::new("relative/x")).is_err());
        assert!(validate_workdir(Path::new("/definitely/not/exist")).is_err());
        // 系统根级目录（canonicalize 归一后命中黑名单；/tmp 在 macOS 归一为
        // /private/tmp，仍应被拒——用真实存在的系统目录探测）。
        for broad in ["/usr", "/etc", "/Library", "/System"] {
            assert!(
                validate_workdir(Path::new(broad)).is_err(),
                "{broad} 应被拒绝"
            );
        }
        // home 根拒绝；home 下的项目子目录放行。
        let home = dirs::home_dir().expect("home");
        assert!(validate_workdir(&home).is_err(), "home 根应被拒绝");
        let proj = home.join("Work"); // 测试机环境存在；不存在则跳过该断言
        if proj.is_dir() {
            assert!(validate_workdir(&proj).is_ok(), "{proj:?} 应放行");
        }
    }
}
