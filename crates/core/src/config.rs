//! 配置加载（`~/.imagent/config.toml`）。

use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    /// 按后端自动选档（2026-08 起为**缺省**）：claude-cli（支持 IM 审批闭环）→
    /// [`PermissionMode::Ask`] 全闭环；claude-acp / codex / gemini（闭环未接）→
    /// [`PermissionMode::Off`]（靠各自 sandbox / approval-mode 兜底）。启动 /
    /// SIGHUP / `/perm auto` 均先 [`PermissionMode::resolve`] 成具体档再入运行时。
    #[default]
    Auto,
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
        matches!(self, Self::Allow | Self::Deny | Self::Ask)
    }

    /// `Auto` 按后端解析成具体档（具体档原样返回）。
    pub fn resolve(self, agent: &str) -> Self {
        match self {
            Self::Auto if agent == "claude-cli" => Self::Ask,
            Self::Auto => Self::Off,
            other => other,
        }
    }
    /// 小写标签，用于 MCP 子命令 --mode 参数与日志。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
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
    /// 简要工具摘要（默认：工具名 + 40 字符输入截断，最多 5 个）。
    #[default]
    Brief,
    /// 详细工具过程（200 字符输入截断，最多 10 个）。
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
    /// 工具输入摘要的字符截断上限。
    pub fn input_trunc(self) -> usize {
        match self {
            Self::Off => 0,
            Self::Brief => 40,
            Self::Detailed => 200,
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
    /// 管理员 sender（可执行 /allow /disallow 授权新用户）。空 = 向后兼容（所有
    /// 白名单用户可 /allow，P2-D 建议生产环境显式设置以收敛授权面）。
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
    /// Prometheus 指标 / 健康检查 HTTP 监听地址（如 `"127.0.0.1:9100"`）。
    /// 默认 `None`（关闭——开源分发时不默认开启监听端口）；显式设置地址即开启。
    #[serde(default = "default_metrics_addr")]
    pub metrics_addr: Option<String>,
    /// 出站消息单条字符上限（Unicode char 计）。超长则由各 Platform 的 `send_text`
    /// 在内部分片。`None` = 不分片（默认，保持单条发送的既有行为）。
    #[serde(default)]
    pub message_max_len: Option<usize>,
    /// 分片之间发送间隔（毫秒），避免多条叠加触发 IM 限流。默认 400。
    #[serde(default = "default_fragment_interval_ms")]
    pub message_fragment_interval_ms: u64,
    /// 单次 agent 运行超时（秒）。超时则中止该次 run（依赖 backend 的
    /// `kill_on_drop` 杀子进程），防止挂死的 agent 永久卡住会话。默认 600（10 分钟）。
    #[serde(default = "default_agent_timeout_secs")]
    pub agent_timeout_secs: u64,
    /// 权限审批（Ask 模式）等待用户回复的超时（秒），超时则 deny。默认 300（5 分钟）。
    /// S-3：独立预算——审批等待不再挤占 `agent_timeout` 的执行预算（`agent_timeout`
    /// 覆盖审批 + 执行总和）。建议 < `agent_timeout_secs`，否则慢审批可能撑满 agent 超时。
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
    /// 回复形态偏好（P7-A4；默认 card）。text = 不建卡走纯文本流（/config 可热改）。
    #[serde(default)]
    pub reply_mode: ReplyMode,
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
    const BROAD: &[&str] = &[
        "/tmp", "/var", "/etc", "/usr", "/bin", "/sbin", "/System", "/Library", "/Users", "/home",
        "/opt", "/srv", "/mnt", "/Volumes", "/proc", "/sys", "/dev", "/run",
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
    600
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

impl Config {
    /// 默认配置文件路径：`<imagent_home>/config.toml`（P4-10：随 profile 隔离）。
    pub fn default_path() -> Option<PathBuf> {
        Some(crate::paths::imagent_home().join("config.toml"))
    }

    /// 读取并解析。文件不存在 => `CoreError::Config`。
    /// `default_workdir` 缺失或非绝对路径 => `CoreError::Config`（给出清晰提示）。
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(CoreError::Io)?;
        let cfg: Self = toml::from_str(&raw)
            .map_err(|e| CoreError::Config(format!("parse {}: {e}", path.display())))?;

        if cfg.default_workdir.as_os_str().is_empty() || !cfg.default_workdir.is_absolute() {
            return Err(CoreError::Config(format!(
                "default_workdir 必须是绝对路径，当前为 {:?}。请参考 EXAMPLE 模板。",
                cfg.default_workdir
            )));
        }

        // P5 快赢：数值下界/上限校验——错误前置到启动期，而非运行期静默劣化
        //（0 值超时 = 所有 run 瞬时失败/审批必拒；超大批窗口 = 每条回复显著延迟）。
        if cfg.agent_timeout_secs == 0 {
            return Err(CoreError::Config(
                "agent_timeout_secs 必须 ≥ 1（0 会让所有 agent 运行瞬时超时）".into(),
            ));
        }
        if cfg.permission_ask_timeout_secs == 0 {
            return Err(CoreError::Config(
                "permission_ask_timeout_secs 必须 ≥ 1（0 会让所有审批必然超时拒否）".into(),
            ));
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

        Ok(cfg)
    }

    /// P5-7（安全）：危险组合探测——`allowed_chats`（群放行）非空但
    /// `admin_senders` 为空。群维度授权后所有成员过鉴权门，而 admins 为空时
    /// `is_admin` 对全员返回 true（向后兼容语义），组合效果 = 群内**任何成员**
    /// 都具备管理能力（/allow 自扩权、/chat 横向扩群、/config /perm 改全局）。
    /// 单用户私用无感；群部署必须显式设 admin_senders 收紧。
    pub fn admin_gap_with_chat_allowlist(&self) -> bool {
        !self.allowed_chats.is_empty() && self.admin_senders.is_empty()
    }

    /// 供首次使用打印的模板字符串（default_workdir 用占位，不写死任何机器路径）。
    pub const EXAMPLE: &'static str = r#"# ~/.imagent/config.toml
default_workdir = "/absolute/path/to/agent/workspace"   # 必填，agent 的 cwd（非沙箱：不限制可读路径，靠 allowed_tools + permission_mode 兜底）
allowed_senders = []        # 留空 = 发现模式（只打日志记录入站 sender，不驱动 agent）
# allowed_chats = ["feishu:oc_xxx"]   # 会话(群)白名单：群消息 chat 放行 OR sender 放行即过（/chat 可动态管理）
# admin_senders = []          # 可 /allow 的管理员 sender；空=所有白名单用户可(P2-D，生产建议显式设置收敛授权面)
# allowed_tools = ["*"]                      # 缺省=全部工具（不收敛）；要白名单显式列（如 ["Read","Edit"]）；执行类建议配 permission_mode="ask"
agent = "claude-cli"         # claude-cli(默认) | claude-acp(ACP长驻子进程) | codex | gemini
platform = "ilink"   # ilink(默认,扫码登录) | wecom(企业微信机器人) | feishu(飞书,配 feishu_app_id + 环境变量 IMAGENT_FEISHU_APP_SECRET)
# feishu_app_id = "cli_xxx"            # 飞书自建应用 app_id（仅 platform="feishu"；app_secret 走环境变量，keyring 为后续 P2）
# feishu_base_url = "https://open.feishu.cn"  # 可选，默认 https://open.feishu.cn；Lark 国际版 https://open.larksuite.com（MVP 不覆盖）
# feishu_require_mention_in_group = true       # 群消息须 @bot 才处理（默认 true：客户端过滤 + 剥离 @bot 占位；false=全收，过滤交给事件订阅 scope）
# stranger_mention_hint = false                # 未放行群里被 @ 时回一句引导（默认 false 完全静默防探测；私聊始终静默）
# reply_mode = "card"                          # 回复形态：card(默认,流式卡片) | text(纯文本)；/config 可热改
permission_mode = "auto"    # 缺省=auto：claude-cli 起 IM 审批闭环(同 ask)，其余后端=off；也可显式 off/allow/deny/ask
# metrics_addr = "127.0.0.1:9100"   # 默认关闭；设为 "ip:port" 开启 /metrics + /health HTTP server
# message_max_len = 2000              # 单条出站消息字符上限（Unicode char）；不设 = 不分片
# message_fragment_interval_ms = 400  # 分片间发送间隔（ms）
# agent_timeout_secs = 600            # 单次 agent 运行超时（秒）；超时中止防挂死
# agent_idle_timeout_secs = 300       # 空闲看门狗(秒)：连续无输出则终止本轮；0=关闭
# batch_window_ms = 1500              # 批处理窗口(ms)：连发消息合并为一轮 prompt；0=关闭
# cot_detail = "brief"                # 工具过程展示：off | brief(默认) | detailed（/config 可热改）
# permission_ask_timeout_secs = 300   # Ask 模式等用户回复超时(秒，独立预算，不挤占 agent 超时)
# ask_via_im_conv = "feishu:ou_xxx"   # 终端 agent 的 ask_via_im 工具投递目标会话（设了才启用；配合 `imagent mcp-ask` 挂到任意终端 agent 的 MCP 配置）
# ask_via_im_timeout_secs = 1800      # ask_via_im 等待回复超时(秒，默认 30 分钟，可被调用覆盖)
# shutdown_grace_secs = 60            # 优雅退出 drain 宽限(秒)；超时 abort 在飞 task
# require_keyring = false        # 默认 false(headless 明文回退+warn); true=keyring 不可用时拒绝明文落盘(fail-closed，安全部署建议)
"#;
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
        // 2026-08 起缺省 auto；resolve 按 backend 选档。
        assert_eq!(cfg.permission_mode, PermissionMode::Auto);
        assert_eq!(
            PermissionMode::Auto.resolve("claude-cli"),
            PermissionMode::Ask
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
        assert_eq!(cfg.cot_detail.input_trunc(), 40);
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
        assert_eq!(CotDetail::Detailed.input_trunc(), 200);
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
            ("timeout0", "agent_timeout_secs = 0\n"),
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
