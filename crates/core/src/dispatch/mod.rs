//! 消息调度核心。
//!
//! `Dispatcher` 持有注入的 `Arc<dyn Platform>` / `Arc<dyn Backend>` / `Store` /
//! `Auth` / 配置，循环 `platform.recv()` 并对每条消息 `tokio::spawn` 处理。
//!
//! 两条硬约束在此体现：
//! 1. 非白名单 sender 丢弃；发现模式（白名单为空）回引导消息但不驱动 agent。
//! 2. backend 只用配置的 `allowed_tools`、workdir 用配置的 `default_workdir`。
//!
//! 结构（5238 行巨石拆分，见 P4_ROADMAP 第六批）：本文件保留 Dispatcher 状态与
//! 生命周期（构造 / run 主循环 / conv 锁与批处理 runner / reply 基元）；
//! [`commands`] 是斜杠命令分派；[`round`] 是单轮 agent 状态机；[`socket`] 是
//! 权限审批 Unix socket；`tests` 集中全部单测。

mod commands;
mod round;
mod socket;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::auth::Auth;
use crate::backend::Backend;
use crate::card_session::CardSession;
use crate::config::{CotDetail, PermissionMode, ReplyMode};
use crate::error::Result;
use crate::metrics::METRICS;
use crate::permission::{
    is_explicit_reply_word, parse_reply, PendingKind, PermissionReply, PermissionRouter,
};
use crate::platform::Platform;
use crate::types::{
    AgentChunk, CardButton, CardButtonStyle, CardTerminal, ConfigFormField, ConvId, InboundMessage,
    MediaRef, ReplyHint, SessionId, ToolCall,
};
use imagent_store::{NamedSessionRow, SessionRow, Store};
use parking_lot::RwLock;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

/// per-conv 排队消息上限：runner 在飞期间到达的消息暂存条数。超出回告警并丢弃，
/// 防刷屏把合并后的 prompt 撑爆。
const PENDING_QUEUE_CAP: usize = 100;

/// D2：存在待审批 pending 时，对「未被消费的自由文本」的提示去重间隔——
/// 同一 conv 在该窗口内只提示一次，避免每条消息都刷屏。
const PENDING_HINT_DEDUPE: Duration = Duration::from_secs(60);

/// D7：`/resume` 序号缓存的有效期——缓存按 (conv, sender) 隔离，过期防止
/// 陈旧序号在列表变化后错位。
const RESUME_CACHE_TTL: Duration = Duration::from_secs(600);

/// Dispatcher 时长类预算聚合（避免构造参数表随配置项继续膨胀）。
#[derive(Debug, Clone, Copy)]
pub struct TaskBudgets {
    /// 单次 agent 运行总超时（`agent_timeout_secs`；0 = 关闭，默认）。
    pub agent_timeout: Duration,
    /// Ask 权限审批等待回复超时（`permission_ask_timeout_secs`，独立预算）。
    pub permission_ask_timeout: Duration,
    /// 终端 agent `ask_via_im` 等待回复的默认超时（`ask_via_im_timeout_secs`；
    /// 可被请求的 timeout_secs 覆盖，上限 86400）。
    pub ask_via_im_timeout: Duration,
    /// 优雅退出 drain in-flight task 宽限（`shutdown_grace_secs`）。
    pub shutdown_grace: Duration,
    /// 空闲看门狗：agent 连续无输出该时长则终止本轮（`agent_idle_timeout_secs`；
    /// 零值 = 关闭）。
    pub agent_idle_timeout: Duration,
    /// 批处理窗口：runner 起跑前等待后续消息并入同一轮的时长（`batch_window_ms`；
    /// 零值 = 关闭）。
    pub batch_window: Duration,
    /// W2-5：自动 compact 阈值（`auto_compact_threshold_tokens`；0 = 关闭）——
    /// 成功轮次的上下文水位（usage.input_tokens）达到阈值即自动走 /compact 管道。
    pub auto_compact_threshold_tokens: u64,
    /// W4-1：per-sender 成本上限（美元，滚动 24h；None = 不限）。
    pub sender_daily_cost_limit_usd: Option<f64>,
}

impl TaskBudgets {
    /// 从 Config 构造（单位换算集中在这一处）。
    pub fn from_config(c: &crate::config::Config) -> Self {
        Self {
            agent_timeout: Duration::from_secs(c.agent_timeout_secs),
            permission_ask_timeout: Duration::from_secs(c.permission_ask_timeout_secs),
            ask_via_im_timeout: Duration::from_secs(c.ask_via_im_timeout_secs),
            shutdown_grace: Duration::from_secs(c.shutdown_grace_secs),
            agent_idle_timeout: Duration::from_secs(c.agent_idle_timeout_secs),
            batch_window: Duration::from_millis(c.batch_window_ms),
            auto_compact_threshold_tokens: c.auto_compact_threshold_tokens,
            sender_daily_cost_limit_usd: c.sender_daily_cost_limit_usd,
        }
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
/// 把字符串按字符截断到 n 个字符，超出则加省略号。
fn truncate_str(s: &str, n: usize) -> String {
    let count = s.chars().count();
    let t: String = s.chars().take(n).collect();
    if count > n {
        format!("{t}…")
    } else {
        t
    }
}

/// 人读运行时长（/status 用）：`2d3h` / `4h05m` / `7m` / `42s`。
fn format_uptime(d: Duration) -> String {
    let secs = d.as_secs();
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    let s = secs % 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else if mins > 0 {
        format!("{mins}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// 人读 Duration（S-10：超时/失败文案用，替代 `{:?}` 的 `180s` 裸 Debug 输出）：
/// `45 秒` / `3 分钟` / `2 小时 5 分钟` / `1 天 3 小时`。
fn format_duration_human(d: Duration) -> String {
    let secs = d.as_secs();
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    let s = secs % 60;
    if days > 0 {
        format!("{days} 天 {hours} 小时")
    } else if hours > 0 {
        format!("{hours} 小时 {mins} 分钟")
    } else if mins > 0 {
        format!("{mins} 分钟")
    } else {
        format!("{s} 秒")
    }
}

/// S-7：backend 失败统一文案模板——人可读摘要 + 是否可续接 + 建议动作；
/// 技术细节（原始错误串）只进服务端日志，不再裸发给用户。
fn backend_failure_reply(backend_name: &str) -> String {
    format!(
        "❌ {backend_name} 本轮执行失败，任务未完成。\n\
         已完成的进度已保留：直接重发消息即可续接（想全新开始可发 /new）。\n\
         若持续失败，可发 /doctor 自检；技术细节见服务端日志。"
    )
}

/// Wave B-1：审批等待过半的加急催办文案（纯函数，便于单测）。
/// 剩余分钟向上取整（剩 30 秒显示「剩 1 分钟」，宁多勿少）。
/// 真机校准（2026-08）：措辞平台中立——卡片平台按钮/👍 可用，纯文本平台
/// 回复 y/n；审批卡可能已被后续消息顶到上方，指明位置。
fn approval_buzz_text(tool_name: &str, remaining: Duration) -> String {
    let mins = remaining.as_secs().div_ceil(60).max(1);
    format!(
        "⏰ 审批即将超时（剩 {mins} 分钟）：{tool_name}——点上方审批卡的按钮或 👍，回复 y/n 亦可"
    )
}

/// 审批卡 note 行的倒计时警示（真机校准 2026-08：催办双通道之一——patch 卡
/// 片 note 行，上下文内可见；与 buzz 文本同分钟数）。
fn approval_buzz_note(remaining: Duration) -> String {
    let mins = remaining.as_secs().div_ceil(60).max(1);
    format!("⏰ 剩 {mins} 分钟将自动拒绝——点按钮或 👍 均可")
}

/// Wave B-1：审批等待出口（`wait_reply_with_buzz` 的结果，与原 timeout 包裹的
/// 三分支一一对应）。
#[derive(Debug)]
enum AskWaitOutcome {
    /// 用户已回复（allow/deny/always）。
    Replied(crate::permission::PermissionReply),
    /// receiver 被 drop（等待方已离开）。
    Dropped,
    /// 等满预算超时（fail-closed deny 由调用方落地）。
    TimedOut,
}

/// Wave B-1：等待审批回复——等待过半（elapsed ≥ timeout/2）仍未决时，向询问
/// 所在会话发一条 buzz 加急催办（**只发一次**），随后继续等剩余时间。
///
/// 取舍：催办发到询问所在 conv（群 conv 即发群里，询问卡本就贴在那）；
/// 分两段 `timeout` 而非 select 环路，结构上保证「最多提醒一次」。
async fn wait_reply_with_buzz(
    rx: tokio::sync::oneshot::Receiver<crate::permission::PermissionReply>,
    conv: &ConvId,
    tool_name: &str,
    timeout: Duration,
    platform: &dyn Platform,
) -> AskWaitOutcome {
    let mut rx = rx;
    let started = Instant::now();
    let half = timeout / 2;
    // 前半程：oneshot Receiver 是 Unpin，按引用等待（未决时 rx 仍可继续用）。
    match tokio::time::timeout(half, &mut rx).await {
        Ok(Ok(r)) => AskWaitOutcome::Replied(r),
        Ok(Err(_)) => AskWaitOutcome::Dropped,
        Err(_) => {
            // 过半未决：双通道催办（均 best-effort，失败仅 log 不影响等待）——
            // ① patch 审批卡的 note 行为倒计时警示（卡片平台上下文内可见；
            //    复用 P10-③ note 联动，纯文本平台 no-op）；
            // ② 加急文本推送（buzz 弹窗——用户可能没开聊天窗口，note patch
            //    无法主动触达；真机校准 2026-08 保留文本的根本原因）。
            let remaining = timeout.saturating_sub(started.elapsed());
            let text = approval_buzz_text(tool_name, remaining);
            let note = approval_buzz_note(remaining);
            let _ = platform
                .note_queued_on_ask(conv, &note, &ReplyHint::None)
                .await;
            if let Err(e) = platform
                .send_urgent_text(conv, &text, &ReplyHint::None)
                .await
            {
                tracing::warn!(
                    target: "imagent::core",
                    conv_id = %conv.0,
                    error = %e,
                    "审批过半催办发送失败（不影响等待）"
                );
            }
            // 继续等剩余时间（从轮次起点算，催办耗时不再挤占用户预算）。
            match tokio::time::timeout(timeout.saturating_sub(started.elapsed()), rx).await {
                Ok(Ok(r)) => AskWaitOutcome::Replied(r),
                Ok(Err(_)) => AskWaitOutcome::Dropped,
                Err(_) => AskWaitOutcome::TimedOut,
            }
        }
    }
}

/// Wave B-9：断档续接词表——「继续」类极短 prompt 命中且当前无可续接会话时，
/// 回复前置断档提示（用户预期续接旧会话、实际开了新会话，明确告知优于装正常）。
/// 词条精确全字匹配（trim + 小写）——中文条目 ≤4 字、英文 go on/continue 略长，
/// 全字匹配本身即白名单，无误伤面（长句不会命中）。
fn is_continuation_prompt(text: &str) -> bool {
    const WORDS: &[&str] = &["继续", "接着", "然后", "go on", "continue"];
    let t = text.trim();
    WORDS.contains(&t.to_ascii_lowercase().as_str())
}

/// Wave B-2：长任务完成强提醒文案（纯函数，便于单测）：
/// `✅ 任务完成 · 12m30s · $0.012`（usage 缺失时省略成本段）。
fn task_done_buzz_text(elapsed: Duration, usage_display: Option<&str>) -> String {
    let secs = elapsed.as_secs();
    let run = if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    };
    match usage_display {
        Some(u) => format!("✅ 任务完成 · {run} · {u}"),
        None => format!("✅ 任务完成 · {run}"),
    }
}

/// Wave B-2：完成强提醒触发条件（纯函数，便于单测）：运行超 5 分钟，或本轮
/// 发生过审批/询问（ask 计数有增量）**且运行超 1 分钟**——两者都意味着用户
/// 等过一段不确定的静默期，值得一条加急通知。含审批的短轮次（真机校准
/// 2026-08：24s 含审批也弹）不触发——刚点完审批的用户显然还在看着会话，
/// 终态卡 footer 已含时长/成本，再推一条纯噪音。
fn should_buzz_done(elapsed: Duration, asks_delta: u64) -> bool {
    elapsed > Duration::from_secs(300) || (asks_delta > 0 && elapsed > Duration::from_secs(60))
}

/// epoch 秒 → 相对时间（`/resume` 列表用）：`42秒前` / `5分钟前` / `3小时前` /
/// `2天前`；超 7 天仍用「N天前」（S-11：不再回退裸 epoch 时间戳——对用户无意义）。
fn format_rel_ts(ts: i64) -> String {
    let d = (now_secs() - ts).max(0);
    if d < 60 {
        format!("{d}秒前")
    } else if d < 3_600 {
        format!("{}分钟前", d / 60)
    } else if d < 86_400 {
        format!("{}小时前", d / 3_600)
    } else {
        format!("{}天前", d / 86_400)
    }
}

/// 格式化工具调用摘要：按 COT 档位展示（P4-6），超出 `max` 标 `…(+N)`。
/// P8-1：摘要是人可读单行（`Bash — git status`），形如
/// `\n\n🔧 工具调用：Bash — git status，Read — src/main.rs …(+3)`。
fn format_tool_summary(tool_calls: &[ToolCall], detail: CotDetail) -> String {
    let max = detail.max_tools();
    let shown: Vec<String> = tool_calls
        .iter()
        .take(max)
        .map(crate::render::tool_text_line)
        .collect();
    let mut s = format!("\n\n🔧 工具调用：{}", shown.join("，"));
    if tool_calls.len() > max {
        s.push_str(&format!(" …(+{})", tool_calls.len() - max));
    }
    s
}
/// 当前活动命名 session 的 config 键：`active_name:<conv_id>`。
/// 不存在/空值表示当前会话为默认未命名 session。
fn active_name_key(conv_id: &str) -> String {
    format!("active_name:{conv_id}")
}
/// 压缩摘要的 config 键：`compact_summary:<conv_id>`。
/// 由 /compact 写入，下次新建 session 时作为前情摘要注入后清除（一次性）。
fn compact_summary_key(conv_id: &str) -> String {
    format!("compact_summary:{conv_id}")
}
/// per-conv 工作目录的 config 键：`workdir:<conv_id>`（由 /cd 设置，覆盖默认 workdir）。
fn workdir_key(conv_id: &str) -> String {
    format!("workdir:{conv_id}")
}

/// 命名工作空间的 config 键：`workspace:<name>`（全局别名，所有 conv 共享）。由 /ws 设置。
fn workspace_key(name: &str) -> String {
    format!("workspace:{name}")
}

/// 错误是否指示 iLink session 过期（需重新 login）。///
/// 专用 `CoreError::SessionExpired` variant，靠类型判定而非 Display 子串（更鲁棒）。
fn is_session_expired_err(e: &crate::error::CoreError) -> bool {
    matches!(e, crate::error::CoreError::SessionExpired(_))
}

/// 消息是否可能作为权限审批回复被消费：非空且非斜杠命令。
/// 斜杠命令（如 `/stop`）在等待审批期间也必须可执行——否则会被当 deny 吞掉，
/// 用户将无法中断正等审批的任务；空文本（纯媒体消息）同样不消费。
fn is_permission_reply_candidate(text: &str) -> bool {
    let t = text.trim();
    !t.is_empty() && !t.starts_with('/')
}

/// 私聊 conv 判定（陌生人提示分流用）：按「明确的单人会话前缀」白名单识别——
/// 飞书私聊 `feishu:ou_*`、ilink / wecom 均为单人会话。识别不了的多方形态
/// （飞书群 `feishu:oc_*`、话题群、评论线程 `feishu:comment:*`）一律按非私聊
/// 处理（保持既有静默行为，宁可漏发引导不误发）。
fn is_p2p_conv(conv: &str) -> bool {
    conv.starts_with("feishu:ou_") || conv.starts_with("ilink:") || conv.starts_with("wecom:")
}

/// 合并一批排队消息为一轮 prompt 载体：非空文本拼接、media / media_errors
/// 拼接；sender 与 reply_hint 取首条（各消息入队前已各自过白名单）。
/// P10-④：批内出现**多个不同发送者**（群聊多人）时给各段加说话人标注——
/// 合并不再丢失归属，agent 能区分谁说了哪句；单人连发保持原样（不加噪音）。
fn merge_batch(batch: Vec<InboundMessage>) -> InboundMessage {
    let multi_sender = batch
        .iter()
        .map(|m| m.sender.0.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len()
        > 1;
    let mut it = batch.into_iter();
    let mut first = it.next().expect("merge_batch: batch 非空");
    let first_sender = first.sender.0.clone();
    let mut texts: Vec<String> = first
        .text
        .take()
        .filter(|t| !t.trim().is_empty())
        .map(|t| {
            if multi_sender {
                format!("【{first_sender}】{t}")
            } else {
                t
            }
        })
        .into_iter()
        .collect();
    for m in it {
        if let Some(t) = m.text.filter(|t| !t.trim().is_empty()) {
            texts.push(if multi_sender {
                format!("【{}】{t}", m.sender.0)
            } else {
                t
            });
        }
        first.media.extend(m.media);
        first.media_errors.extend(m.media_errors);
    }
    first.text = if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n\n"))
    };
    first
}

/// P10：排队消息 → 展示摘要（最新一条）：文本取前 40 字符（S-6：按注释口径
/// 截断，防长消息把卡片 footer 撑爆）；纯媒体给占位。
fn latest_snippet(msg: &InboundMessage) -> String {
    match msg.text.as_deref() {
        Some(t) if !t.trim().is_empty() => truncate_str(t.trim(), 40),
        _ if !msg.media.is_empty() => "（图片/文件）".to_string(),
        _ => String::new(),
    }
}

/// 统一 `/resume` 列表条目（P4-11）：IM 会话历史 ∪ 本机同项目 agent 会话。
#[derive(Debug, Clone)]
struct ResumeEntry {
    session_id: String,
    /// epoch 秒。
    updated_at: i64,
    /// 产生该会话的后端类型（历史行带原始 kind；本机会话按当前后端）。
    agent_kind: String,
    /// 首条用户消息摘要（本机扫描有；纯历史行可能空，展示回退 id 前缀）。
    first_prompt: String,
    /// 本机（电脑端）会话——不在 IM 历史表里的扫描结果；接管时附分叉提示。
    from_local: bool,
    /// 本机会话记录的工作目录（jsonl cwd；P5-15 接管前校验用）。
    cwd: Option<String>,
}

/// /resume 列表缓存（D7）：key = (conv, sender)，值带写入时刻（TTL 惰性过期）。
type ResumeCache = HashMap<(String, String), (Instant, Vec<ResumeEntry>)>;

pub struct Dispatcher {
    platform: Arc<dyn Platform>,
    backend: Arc<dyn Backend>,
    store: Store,
    auth: Auth,
    default_workdir: PathBuf,
    allowed_tools: Arc<RwLock<Vec<String>>>,
    /// per-conv 串行锁：同一会话的 agent 任务排队执行，避免 session 冲突。
    conv_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// IM 权限审批路由（Ask 闭环用）。
    router: Arc<PermissionRouter>,
    /// 权限审批模式。
    permission_mode: Arc<RwLock<PermissionMode>>,
    /// 单次 agent 运行超时。超时则中止该次 run（backend 的 kill_on_drop 杀子进程）。
    agent_timeout: std::time::Duration,
    /// 权限审批（Ask）等待用户回复的超时（S-3：独立预算，不挤占 agent_timeout）。
    permission_ask_timeout: std::time::Duration,
    /// 终端 agent `ask_via_im` 等待回复的默认超时（socket ask 分支用）。
    ask_via_im_timeout: std::time::Duration,
    /// 优雅退出 drain in-flight task 的宽限期（R-1：原硬编码 30s）。
    shutdown_grace: std::time::Duration,
    /// 空闲看门狗：agent 连续无输出该时长则终止本轮（零值 = 关闭）。
    /// `/config agent_idle_timeout_secs` 可热改，故共享句柄。
    agent_idle_timeout: Arc<RwLock<Duration>>,
    /// 批处理窗口：runner 起跑前等待后续消息并入同一轮的时长（零值 = 关闭）。
    /// `/config batch_window_ms` 可热改，故共享句柄。
    batch_window: Arc<RwLock<Duration>>,
    /// W2-5：自动 compact 阈值（tokens；0 = 关闭）。config 注入。
    auto_compact_threshold: u64,
    /// W4-1：per-sender 成本上限（美元，滚动 24h；None = 不限）。config 注入。
    sender_cost_limit: Option<f64>,
    /// 工具过程（COT）展示档位（P4-6）：`/config cot_detail` 可热改。
    cot_detail: Arc<RwLock<CotDetail>>,
    /// 进程启动时刻（`/status` uptime 用）。
    started_at: Instant,
    /// per-conv 在飞 agent 任务注册表（`/stop` 中断用）：conv_id → join task 的
    /// AbortHandle。同 conv 轮次串行（conv 锁保证），key 插入/移除无 ABA。
    running: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    /// per-conv 批处理队列：runner 在飞期间到达的消息暂存（entry 存在 = runner
    /// 活跃；runner 取空交还时移除）。入队与取批共用一把锁，杜绝 lost-wakeup。
    queues: Mutex<HashMap<String, Vec<InboundMessage>>>,
    /// P10：per-conv 排队状态（count + 最新摘要）——入队路径写、取批//stop 清、
    /// CardSession 每次 patch 拉取渲染进 Running footer（状态上卡，不发消息）。
    queued_hints: Arc<Mutex<HashMap<String, crate::card_session::QueuedHint>>>,
    /// per-conv 最近一次 `/resume` 渲染的列表（P4-11）：序号选择取缓存，
    /// 防两次调用间本机会话 mtime 变化导致错位；S-16：选中不移除条目（防序号
    /// 前移错位），陈旧由 D7 的 TTL 惰性过期兜底。
    /// D7：key 为 (conv, sender)——群聊多用户共用 conv，仅按 conv 缓存会互相
    /// 覆盖错位；值带写入时刻，超过 [`RESUME_CACHE_TTL`] 惰性过期。
    resume_cache: Mutex<ResumeCache>,
    /// P6-9：per-conv 空闲看门狗覆盖（`/timeout`）——`Some(ZERO)` = 本会话关闭；
    /// 无条目 = 跟随全局 `agent_idle_timeout`。进程内（会话级旋钮，不落盘）。
    idle_overrides: Mutex<HashMap<String, Duration>>,
    /// Wave B-7：per-conv COT 档位覆盖（`/config cot`，白名单用户可改自己会话）。
    /// 无条目 = 跟随全局 `cot_detail`（`/config cot_detail`，admin）。进程内
    /// （会话级偏好，不落盘——与 idle_overrides 同姿态）。
    cot_overrides: Mutex<HashMap<String, CotDetail>>,
    /// Wave B-4：quiet_hours 原文（config 注入，仅 /config 展示用——降级判定在
    /// 各平台实现侧，core 不重复实现时区逻辑）。
    quiet_hours_raw: RwLock<Option<String>>,
    /// P7-A3：陌生人被 @ 提示开关（config 注入，set_prefs 热设；共享句柄）。
    stranger_mention_hint: RwLock<bool>,
    /// 私聊陌生人引导开关（config 注入，默认 true——私聊是主动来找 bot 的，
    /// 无探测面；与群内 stranger_mention_hint 的静默默认相反，见 config 注释）。
    stranger_p2p_hint: RwLock<bool>,
    /// P7-A4：回复形态偏好（card/text，/config 可热改）。
    reply_mode: Arc<RwLock<ReplyMode>>,
    /// 审批集（ask 模式下仅清单内工具过 IM 审批，其余放行；空 = 全部过审）。
    /// main 启动注入 + SIGHUP 热重载（见 [`Self::set_approval_tools`]）。
    approval_tools: Arc<RwLock<Vec<String>>>,
    /// 管理员 sender（可 /allow /config /perm /admin）。S2：空 = **无人**是
    /// 管理员（IM 内管理命令全部不可用，须通过 CLI / setup 配置 admin_senders）。
    admin_senders: Arc<RwLock<Vec<String>>>,
    /// D2：per-conv 最近一次「存在待审批项」提示的时刻（PENDING_HINT_DEDUPE 去重）。
    pending_hint_last: Mutex<HashMap<String, Instant>>,
    /// W3-3：per-conv 最近一轮的用户 prompt（/retry 与失败快捷操作卡用）。
    /// 上限 500 条（超量整体清空——粗防泄漏，语义无损）。
    last_prompts: Mutex<HashMap<String, String>>,
    /// 优雅退出信号（P1-5）：收到 SIGINT/SIGTERM 后 cancel，run() 停止收新消息并
    /// drain。D4：改用 CancellationToken（持久信号）——`Notify::notify_waiters` 只
    /// 唤醒**已注册**的等待者，信号先于监听者 await 到达时存在丢失窗口。
    shutdown: Arc<tokio_util::sync::CancellationToken>,
    /// in-flight handle task 集合（P1-5）：drain 时等待其完成，避免 SIGKILL 正在
    /// 写文件的 agent 子进程导致半写。task 完成自动移除。
    tasks: Arc<Mutex<tokio::task::JoinSet<()>>>,
    /// D12：permission socket accept task 是否已 spawn（幂等防重复 spawn；
    /// run() 启动与 /perm ask 热切共用同一路径）。
    socket_spawned: std::sync::atomic::AtomicBool,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        platform: Arc<dyn Platform>,
        backend: Arc<dyn Backend>,
        store: Store,
        auth: Auth,
        default_workdir: PathBuf,
        allowed_tools: Vec<String>,
        permission_mode: PermissionMode,
        budgets: TaskBudgets,
        cot_detail: CotDetail,
        admin_senders: Vec<String>,
    ) -> Self {
        Self::new_with_handles(
            platform,
            backend,
            store,
            auth,
            default_workdir,
            Arc::new(RwLock::new(allowed_tools)),
            Arc::new(RwLock::new(permission_mode)),
            budgets,
            cot_detail,
            admin_senders,
        )
    }

    /// 与 [`new`](Self::new) 相同，但接受外部持有的共享句柄
    /// （`allowed_tools` / `permission_mode` 的 `Arc<RwLock>`）。
    ///
    /// main 用此构造，把 `permission_mode` 句柄同时共享给 `ClaudeBackend`，
    /// 使 SIGHUP 热重载对二者同时生效。
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_handles(
        platform: Arc<dyn Platform>,
        backend: Arc<dyn Backend>,
        store: Store,
        auth: Auth,
        default_workdir: PathBuf,
        allowed_tools: Arc<RwLock<Vec<String>>>,
        permission_mode: Arc<RwLock<PermissionMode>>,
        budgets: TaskBudgets,
        cot_detail: CotDetail,
        admin_senders: Vec<String>,
    ) -> Self {
        let disp = Self {
            platform,
            backend,
            store,
            auth,
            default_workdir,
            allowed_tools,
            conv_locks: Mutex::new(HashMap::new()),
            router: Arc::new(PermissionRouter::new()),
            permission_mode,
            agent_timeout: budgets.agent_timeout,
            permission_ask_timeout: budgets.permission_ask_timeout,
            ask_via_im_timeout: budgets.ask_via_im_timeout,
            shutdown_grace: budgets.shutdown_grace,
            agent_idle_timeout: Arc::new(RwLock::new(budgets.agent_idle_timeout)),
            batch_window: Arc::new(RwLock::new(budgets.batch_window)),
            auto_compact_threshold: budgets.auto_compact_threshold_tokens,
            sender_cost_limit: budgets.sender_daily_cost_limit_usd,
            cot_detail: Arc::new(RwLock::new(cot_detail)),
            started_at: Instant::now(),
            running: Mutex::new(HashMap::new()),
            queues: Mutex::new(HashMap::new()),
            queued_hints: Arc::new(Mutex::new(HashMap::new())),
            resume_cache: Mutex::new(HashMap::new()),
            pending_hint_last: Mutex::new(HashMap::new()),
            last_prompts: Mutex::new(HashMap::new()),
            idle_overrides: Mutex::new(HashMap::new()),
            cot_overrides: Mutex::new(HashMap::new()),
            quiet_hours_raw: RwLock::new(None),
            stranger_mention_hint: RwLock::new(false),
            stranger_p2p_hint: RwLock::new(true),
            reply_mode: Arc::new(RwLock::new(ReplyMode::Card)),
            approval_tools: Arc::new(RwLock::new(Vec::new())),
            admin_senders: Arc::new(RwLock::new(admin_senders)),
            shutdown: Arc::new(tokio_util::sync::CancellationToken::new()),
            tasks: Arc::new(Mutex::new(tokio::task::JoinSet::new())),
            socket_spawned: std::sync::atomic::AtomicBool::new(false),
        };
        // S2：admin_senders 为空 = 无人是管理员，IM 内管理命令全部不可用——
        // 构造即显著提示（防用户以为白名单用户仍可 /allow）。
        if disp.admin_senders.read().is_empty() {
            warn!(
                target: "imagent::core",
                "admin_senders 为空，IM 内管理命令不可用；请通过 CLI（imagent setup / config.toml admin_senders）配置管理员"
            );
        }
        disp
    }

    /// 审批集注入/热重载（main 启动与 SIGHUP 调用；空 = 全部权限请求过审）。
    pub fn set_approval_tools(&self, tools: Vec<String>) {
        *self.approval_tools.write() = tools;
    }

    /// P7：启动偏好注入（main 在 run 前调一次；构造器保持零新参，测试无感）。
    /// 私聊引导默认 true（构造器初值），未显式传前保持构造默认。
    pub fn set_prefs(
        &self,
        stranger_mention_hint: bool,
        stranger_p2p_hint: bool,
        reply_mode: ReplyMode,
    ) {
        *self.stranger_mention_hint.write() = stranger_mention_hint;
        *self.stranger_p2p_hint.write() = stranger_p2p_hint;
        *self.reply_mode.write() = reply_mode;
    }

    /// Wave B-4：quiet_hours 原文注入（main 启动时调一次；仅 /config 展示用，
    /// 降级判定在平台实现侧——core 不做时区换算）。
    pub fn set_quiet_hours(&self, raw: Option<String>) {
        *self.quiet_hours_raw.write() = raw;
    }

    /// P6-9：该会话的空闲看门狗——`/timeout` 覆盖优先（ZERO=关），否则全局值。
    async fn idle_timeout_for(&self, conv: &str) -> Duration {
        if let Some(d) = self.idle_overrides.lock().await.get(conv) {
            return *d;
        }
        *self.agent_idle_timeout.read()
    }

    /// Wave B-7：该会话的 COT 档位——`/config cot` 覆盖优先，否则全局值
    /// （`/config cot_detail`，admin）。每轮读取，热改对下一轮生效。
    async fn cot_for(&self, conv: &str) -> CotDetail {
        if let Some(d) = self.cot_overrides.lock().await.get(conv) {
            return *d;
        }
        *self.cot_detail.read()
    }

    /// 调用者是否为管理员（可 /allow /config /perm /admin）。S2：admin_senders
    /// 空 = **无人**是管理员（旧「空 = 全员可」语义使群部署下任意白名单成员可
    /// 自扩权，已收紧）；非空则严格匹配（P2-D）。
    fn is_admin(&self, sender: &str) -> bool {
        let admins = self.admin_senders.read();
        let trimmed = sender.trim();
        admins.iter().any(|a| a.trim() == trimmed)
    }

    /// P5-1/S1（安全）：审批回复的发送者须过 **sender 白名单**（或为管理员）。
    /// 审批路由发生在 handle() **之前**，天然绕过其鉴权；旧「sender OR 会话白
    /// 名单」门在群被 `/chat allow` 加白后，任意群成员发 "y" 即可批准 Bash 等
    /// 高危工具——群白名单只代表「可对话」，不代表「可批高危操作」，故收紧为
    /// 仅 sender 白名单（管理员兜底）。飞书审批按钮回调携带 operator open_id
    /// 作 sender，同一门槛覆盖按钮路径。
    fn can_route_permission_reply(&self, msg: &InboundMessage) -> bool {
        self.auth.is_allowed(&msg.sender) || self.is_admin(&msg.sender.0)
    }

    /// SIGHUP 热重载：整体替换 allowed_tools。
    pub fn reload_tools(&self, tools: Vec<String>) {
        *self.allowed_tools.write() = tools;
    }

    /// SIGHUP 热重载：更新 permission_mode（与 ClaudeBackend 共享同一句柄时
    /// 二者同步生效）。D12：热切到 Ask/auto-claude 闭环类档位时惰性补起
    /// socket accept task（幂等，见 [`Self::ensure_permission_socket`]），
    /// 不再要求重启。
    ///
    /// S-1（安全）：闭环档位 × 非 FullLoop 后端在热切路径同样校验（与 run()
    /// 启动期 fail-closed 同口径）——此前热重载只写句柄不校验，SIGHUP 重载
    /// 即可绕过启动期能力矩阵；校验不过返回 Err 且**不**写句柄（拒绝热切）。
    /// 闭环档位下 socket bind 失败同样返回 Err（模式已写入，但闭环不可用，
    /// 由调用方决定回滚/告警）。
    pub fn reload_permission_mode(
        &self,
        mode: PermissionMode,
    ) -> std::result::Result<(), crate::error::CoreError> {
        // 校验先于写句柄：拒绝热切时保持既有模式不变。
        if mode.needs_socket()
            && self.backend.permission_capability()
                != crate::backend::PermissionCapability::FullLoop
        {
            error!(
                target: "imagent::core",
                mode = mode.as_str(),
                backend = self.backend.name(),
                capability = self.backend.permission_capability().as_str(),
                "热切权限模式被拒绝：闭环档位需要 FullLoop 后端"
            );
            return Err(crate::error::CoreError::Config(format!(
                "permission_mode = \"{}\" 需要后端支持 IM 审批闭环，但当前后端 {} 的权限能力为 {}。\
                 请将 permission_mode 改为 auto/off/allow/deny，或改用支持闭环的 claude 系后端（claude-cli / claude-acp）",
                mode.as_str(),
                self.backend.name(),
                self.backend.permission_capability().as_str()
            )));
        }
        // 闭环档位先确保 socket 就绪再写句柄——socket 失败时同样保持原模式
        // （拒绝热切即完全拒绝，不留「模式已切但闭环不可用」的半切状态）。
        if mode.needs_socket() && !self.ensure_permission_socket() {
            return Err(crate::error::CoreError::Config(format!(
                "permission_mode = \"{}\" 需要权限审批 socket，但 socket 启动失败（路径/平台不支持）；\
                 Ask 审批闭环不可用，请检查日志或改用 off/allow/deny 档位",
                mode.as_str()
            )));
        }
        *self.permission_mode.write() = mode;
        Ok(())
    }

    /// D12：needs_socket 且 socket accept task 未启动时惰性 spawn（复用
    /// [`Self::spawn_socket_accept`]，其内部幂等防重复 spawn；socket 文件与
    /// token 残留由 spawn 时统一清理重建）。非 unix 平台恒 false。
    fn ensure_permission_socket(&self) -> bool {
        #[cfg(unix)]
        {
            match crate::permission::default_sock_path() {
                Some(sock) => self.spawn_socket_accept(sock.to_string_lossy().into_owned()),
                None => {
                    error!(
                        target: "imagent::core",
                        "Ask 模式但无法定位 socket 路径，权限请求将无法路由"
                    );
                    false
                }
            }
        }
        #[cfg(not(unix))]
        {
            warn!(
                target: "imagent::core",
                "Ask 权限审批闭环需要 Unix domain socket，当前平台(Windows)不可用；请改用 permission_mode = allow/deny/off 或在 macOS/Linux 运行"
            );
            false
        }
    }

    /// 暴露 auth（main 的 SIGHUP task 用其 reload）。
    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    /// 暴露 router（主进程 socket accept task 用）。
    pub fn router(&self) -> Arc<PermissionRouter> {
        self.router.clone()
    }

    /// B3：构造注入 backend 的 IM 审批闭环回调（ACP 的
    /// `session/request_permission` 经此进 IM）。与 socket.rs 的
    /// `handle_permission_kind_socket` 同一套语义（审批集过滤 → register →
    /// send_permission_ask → permission_ask_timeout 等待 → 超时/失败 deny），
    /// 只是回复走内存回调而非 socket 写回。
    fn build_im_permission_hook(&self) -> crate::backend::ImPermissionHook {
        let platform = self.platform.clone();
        let router = self.router.clone();
        let approval_tools = self.approval_tools.clone();
        let timeout = self.permission_ask_timeout;
        // Wave B-11：超时分支落 timeout 审计（审批统计聚合数据源）。
        let store = self.store.clone();
        Arc::new(move |ask: crate::backend::ImPermissionAsk| {
            let platform = platform.clone();
            let router = router.clone();
            let approval_tools = approval_tools.clone();
            let store = store.clone();
            Box::pin(async move {
                // 审批集外直接放行（空集 = 全部过审），与 socket 路径口径一致。
                if !crate::permission::needs_approval(&approval_tools.read(), &ask.tool_name) {
                    info!(
                        target: "imagent::core",
                        conv_id = %ask.conv_id,
                        tool = %ask.tool_name,
                        "审批集外工具，直接放行（approval_tools 未命中）"
                    );
                    METRICS
                        .permission_decisions
                        .with_label_values(&["allow"])
                        .inc();
                    return true;
                }
                // D-记忆：该 conv 已「始终允许」此工具 → 跳过 IM 审批直接放行
                //（连续 N 次同工具不再每条进 IM）。
                if router
                    .is_session_allowed(&ask.conv_id, &ask.tool_name)
                    .await
                {
                    info!(
                        target: "imagent::core",
                        conv_id = %ask.conv_id,
                        tool = %ask.tool_name,
                        "会话级始终允许命中，跳过审批（session allow-set）"
                    );
                    METRICS
                        .permission_decisions
                        .with_label_values(&["allow"])
                        .inc();
                    return true;
                }
                let conv = ConvId(ask.conv_id.clone());
                // D5：先 register 占位再发卡（防极速按钮回调先于 register 到达）。
                let rx = router
                    .register(
                        &ask.conv_id,
                        &ask.request_id,
                        None,
                        PendingKind::Permission,
                        Some(&ask.tool_name),
                    )
                    .await;
                // S-5（泄漏防护）：本 hook future 被 drop（run 被 /stop abort、总超时
                // 或空闲看门狗打断）时不会走任何 await 之后的 cancel 分支——挂上
                // Drop guard 兜底 cancel，防 pending 残留吞掉后续消息。正常完成 /
                // 显式 cancel 后 cancel 幂等（pending 已移除则 no-op）。
                let _leak_guard = PendingCancelGuard {
                    router: router.clone(),
                    conv: ask.conv_id.clone(),
                    request_id: ask.request_id.clone(),
                };
                let card_msg_id = match platform
                    .send_permission_ask(
                        &conv,
                        &ask.request_id,
                        &ask.tool_name,
                        &ask.input_summary,
                        &ReplyHint::None,
                    )
                    .await
                {
                    Ok(mid) => mid,
                    Err(e) => {
                        // P1-3：发卡失败 → 撤占位、deny，不留 pending。
                        warn!(
                            target: "imagent::core",
                            conv_id = %ask.conv_id,
                            error = %e,
                            "send permission ask 失败，回 deny 并撤占位 pending"
                        );
                        router.cancel(&ask.conv_id, &ask.request_id).await;
                        METRICS
                            .permission_decisions
                            .with_label_values(&["dropped"])
                            .inc();
                        return false;
                    }
                };
                router
                    .set_card_msg_id(&ask.conv_id, &ask.request_id, card_msg_id)
                    .await;
                // S-3：独立预算 permission_ask_timeout；超时 deny（fail-closed）。
                // Wave B-1：等待过半仍未决时先发一条 buzz 加急催办（只一次），
                // 再继续等剩余时间（见 wait_reply_with_buzz）。
                match wait_reply_with_buzz(rx, &conv, &ask.tool_name, timeout, platform.as_ref())
                    .await
                {
                    AskWaitOutcome::Replied(r) => {
                        METRICS
                            .permission_decisions
                            .with_label_values(&[if r.allow { "allow" } else { "deny" }])
                            .inc();
                        r.allow
                    }
                    AskWaitOutcome::Dropped => {
                        router.cancel(&ask.conv_id, &ask.request_id).await;
                        METRICS
                            .permission_decisions
                            .with_label_values(&["dropped"])
                            .inc();
                        false
                    }
                    AskWaitOutcome::TimedOut => {
                        router.cancel(&ask.conv_id, &ask.request_id).await;
                        // 超时自动拒绝后收敛滞留询问卡（best-effort）。
                        if let Err(e) = platform.cancel_permission_ask(&conv, &ask.request_id).await
                        {
                            warn!(target: "imagent::core", error = %e, "超时询问卡收敛失败（不影响 deny）");
                        }
                        // S-18：超时 deny 不能对用户无声——回一条可读消息，说明
                        // 已自动拒绝及后果（否则用户以为点了稍后再说还有效）。
                        let _ = platform
                            .send_text(
                                &conv,
                                &format!(
                                    "⏳ 审批超时（等待 {}），已自动拒绝该操作，任务将被中断。\
                                     如仍需执行请重新发起任务并在时限内回复。",
                                    format_duration_human(timeout)
                                ),
                                &ReplyHint::None,
                            )
                            .await;
                        METRICS
                            .permission_decisions
                            .with_label_values(&["timeout"])
                            .inc();
                        // Wave B-11：timeout 也是一次审批结果——落审计（/stats 审批
                        // 分组的数据源；用户侧无 decision 词可 parse，这里显式记）。
                        if let Err(e) = store
                            .append_audit(
                                "permission_decision",
                                None,
                                Some(&ask.conv_id),
                                Some(&format!(
                                    "tool={} decision=timeout waited_secs={}",
                                    ask.tool_name,
                                    timeout.as_secs()
                                )),
                            )
                            .await
                        {
                            tracing::warn!(
                                target: "imagent::core",
                                error = %e,
                                "append_audit(permission_decision timeout) 失败"
                            );
                        }
                        false
                    }
                }
            })
        })
    }

    /// 触发优雅退出（P1-5）：run() 收到后停止 recv 并 drain in-flight task。
    /// 由 main 的信号处理 task 调用（SIGINT/SIGTERM）。
    pub fn shutdown(&self) {
        // D4：CancellationToken 持久——先 cancel 后监听也不会丢信号。
        self.shutdown.cancel();
    }

    /// 主循环。循环 `platform.recv()`，每条消息 `tokio::spawn` 处理（不阻塞 recv）。
    /// recv 返回 Err 时：session 过期 → 优雅停止（返回 Err 让 main 提示重新 login）；
    /// 其它错误 → 指数退避后继续重试（防 client 异常退出导致 dispatcher 忙循环刷屏；ilink 长轮询层另有退避），不 panic。
    pub async fn run(self: Arc<Self>) -> Result<()> {
        // B3：能力矩阵一行（启动日志，审计/排障用）。
        let cap = self.backend.permission_capability();
        let mode = *self.permission_mode.read();
        info!(
            target: "imagent::core",
            backend = self.backend.name(),
            permission_mode = mode.as_str(),
            capability = cap.as_str(),
            native_passthrough = self.backend.supports_native_permission_mode(),
            "权限能力矩阵"
        );
        // B3（fail-closed）：闭环类档位（Ask / auto-claude）要求 backend 支持
        // IM 审批闭环。此前 codex/gemini 在 ask 档下静默忽略审批（等于全放行），
        // ACP fail-closed 拒绝——现统一为启动即拒绝，错误信息给可行动建议。
        if mode.needs_socket() && cap != crate::backend::PermissionCapability::FullLoop {
            return Err(crate::error::CoreError::Config(format!(
                "permission_mode = \"{}\" 需要后端支持 IM 审批闭环，但当前后端 {} 的权限能力为 {}。\
                 请将 permission_mode 改为 auto/off/allow/deny，或改用支持闭环的 claude 系后端（claude-cli / claude-acp）",
                mode.as_str(),
                self.backend.name(),
                cap.as_str()
            )));
        }
        // B3：把 IM 审批闭环回调注入 backend（与 claude-cli 的 MCP→socket 闭环
        // 同一条 PermissionRouter 通道；ACP 的 session/request_permission 走此）。
        let hook = self.build_im_permission_hook();
        self.backend.set_im_permission_hook(Some(hook));
        // Ask 模式：spawn unix socket accept task（MCP server 转发的权限请求经此进主进程）。
        // D12：抽到 ensure_permission_socket（热切 /perm ask 复用同一路径）。
        // S-2（fail-closed）：bind 失败（路径不可写 / 平台不支持）时**拒绝启动**——
        // 此前返回值被丢弃会静默降级为「无审批」，是安全 posture 退化。
        if self.permission_mode.read().needs_socket() && !self.ensure_permission_socket() {
            return Err(crate::error::CoreError::Config(
                "permission_mode 为 Ask/auto-claude（IM 审批闭环）档位，但权限审批 socket 启动失败\
                 （Unix domain socket 不可用或路径无法绑定）。Ask 闭环完全不可用，拒绝启动；\
                 请改用 permission_mode = auto/off/allow/deny，或在 macOS/Linux 修复 socket 路径后重启"
                    .to_string(),
            ));
        }

        // recv 失败退避（防 client 异常退出后 dispatcher 忙循环刷屏）。
        let mut recv_backoff = std::time::Duration::from_secs(1);
        const RECV_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(30);
        loop {
            // P1-5：监听 shutdown 信号，停止接收新消息并进入 drain。
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => {
                    info!(target: "imagent::core", "shutdown 信号到达，停止接收新消息，drain in-flight task");
                    break;
                }
                msg = self.platform.recv() => match msg {
                    Ok(msg) => {
                        recv_backoff = std::time::Duration::from_secs(1); // 成功，重置退避
                        let conv_id = msg.conv_id.0.clone();
                        // 权限闭环优先：若该 conv 正等待 approve/deny 回复，把这条消息
                        // 当作回复送达 oneshot。P2-2：直接 route（单次 lock 原子 check+
                        // remove+send），避免旧 has_pending→route 两次 lock 间隙被超时
                        // 清理（P1-8 cancel）击穿，导致 "yes" 误走 fallforward 当新 prompt。
                        // 斜杠命令不消费（/stop 在等审批时也要可执行），空文本（纯媒体）
                        // 同样不消费。P5-1：发送者须过白名单才可被消费（防群聊陌生人
                        // 用 "y" 批准权限请求）；未过门的消息落到 handle() 走正常鉴权
                        // 丢弃路径。
                        let text = msg.text.as_deref().unwrap_or("");
                        if is_permission_reply_candidate(text)
                            && self.can_route_permission_reply(&msg)
                        {
                            // D2：自由文本（无按钮回调 ask_req / 无引用回复
                            // reply_to 锚定询问卡）只有在**明确命中审批词表**
                            // （y/n 全字匹配，见 is_explicit_reply_word）时才可被
                            // 消费；否则回落正常 handle/批处理路径，不再被当
                            // deny 兜底吞掉。多 pending 并存且无锚定时无法消解
                            // 歧义，同样不消费，回一条去重提示引导回复卡片。
                            let anchored = msg.ask_req.is_some() || msg.reply_to.is_some();
                            let explicit_word = is_explicit_reply_word(text);
                            let consumable = if anchored {
                                true
                            } else if !explicit_word {
                                false
                            } else {
                                matches!(self.router.pending_count(&conv_id).await, 0 | 1)
                            };
                            let reply = parse_reply(text);
                            let reply_for_card = reply.clone();
                            // 多 pending 三级路由：按钮回调带 ask_req 精确 → 引用
                            // 回复（reply_to）命中询问卡 → 最新 pending 兜底。
                            let routed = if consumable {
                                self.router
                                    .route(
                                        &conv_id,
                                        msg.ask_req.as_deref(),
                                        msg.reply_to.as_deref(),
                                        reply,
                                    )
                                    .await
                            } else {
                                None
                            };
                            if let Some(decision) = routed {
                                let req = decision.request_id.clone();
                                // D-记忆/审计：审批决定（含「始终允许」）落一条
                                // permission_decision 审计——action/tool/decision/sender。
                                // 「始终允许」的 allow-set 写入在 router.route 内完成
                                //（pending 条目携带 tool_name）。
                                let decision_word = match crate::permission::parse_decision(text)
                                {
                                    crate::permission::Decision::AllowAlways => "allow_always",
                                    crate::permission::Decision::Allow => "allow",
                                    crate::permission::Decision::Deny => "deny",
                                };
                                let audit_detail = format!(
                                    "tool={} decision={} sender={} waited_secs={}",
                                    decision.tool_name.as_deref().unwrap_or("<unknown>"),
                                    decision_word,
                                    msg.sender.0,
                                    decision.waited_secs
                                );
                                if let Err(e) = self
                                    .store
                                    .append_audit(
                                        "permission_decision",
                                        Some(&msg.sender.0),
                                        Some(&conv_id),
                                        Some(&audit_detail),
                                    )
                                    .await
                                {
                                    tracing::warn!(
                                        target: "imagent::core",
                                        error = %e,
                                        "append_audit(permission_decision) 失败"
                                    );
                                }
                                // 真机校准 UX：决策已达 MCP，立即把询问卡收敛成
                                // 「已批准/已拒绝」终态（best-effort，无卡 no-op）；
                                // 问题卡（P6）显示「已记录你的选择：<选项>」。
                                if let Err(e) = self
                                    .platform
                                    .resolve_permission_ask(&msg.conv_id, &req, &reply_for_card)
                                    .await
                                {
                                    tracing::warn!(
                                        target: "imagent::core",
                                        error = %e,
                                        "询问卡收敛失败（不影响审批结果）"
                                    );
                                }
                                continue;
                            }
                            // D2：未被消费但确有 pending——回一条去重提示，引导
                            // 用户回复询问卡（或 y/n），避免静默落进 agent 批处理
                            // 造成「发了没人理」的困惑；60s 窗口去重防刷屏。
                            if self.router.has_pending(&conv_id).await {
                                let now = Instant::now();
                                let mut last = self.pending_hint_last.lock().await;
                                let due = last
                                    .get(&conv_id)
                                    .is_none_or(|t| now.duration_since(*t) >= PENDING_HINT_DEDUPE);
                                if due {
                                    last.insert(conv_id.clone(), now);
                                    drop(last);
                                    let n = self.router.pending_count(&conv_id).await;
                                    self.reply(
                                        &msg.conv_id,
                                        &format!(
                                            "⚠️ 当前有 {n} 项待审批/待回答的询问，请回复对应询问卡（多待决时需引用对应卡片），或直接回复 y / n 表态（始终允许可回复 always）。"
                                        ),
                                        &msg.reply_hint,
                                    )
                                    .await;
                                }
                            }
                        }
                        // 每条消息独立 spawn，不阻塞 recv。P1-5：入 JoinSet 以便 drain。
                        let this = self.clone();
                        self.tasks.lock().await.spawn(async move {
                            this.handle(msg).await;
                        });
                    }
                    Err(e) => {
                        if is_session_expired_err(&e) {
                            tracing::error!(
                                target: "imagent::core",
                                error = %e,
                                "session 过期，停止 dispatcher（需重新 login）"
                            );
                            return Err(e);
                        }
                        warn!(
                            target: "imagent::core",
                            error = %e,
                            backoff_secs = recv_backoff.as_secs(),
                            "platform.recv 失败，退避后继续重试（防忙循环刷屏）"
                        );
                        tokio::time::sleep(recv_backoff).await;
                        recv_backoff = (recv_backoff * 2).min(RECV_BACKOFF_CAP);
                    }
                },
            }
        }
        // P1-5/R-1：drain in-flight handle task（最多 shutdown_grace，默认 60s），超时 abort。
        // 避免 SIGKILL 正在写文件的 agent 子进程导致半写；超时兜底防无限等待。
        // R-2：handle_permission_socket 也纳入 self.tasks，drain 一并等待。
        let mut tasks = self.tasks.lock().await;
        let drain = async { while tasks.join_next().await.is_some() {} };
        match tokio::time::timeout(self.shutdown_grace, drain).await {
            Ok(_) => info!(target: "imagent::core", "drain 完成（in-flight task 已结束）"),
            Err(_) => {
                warn!(
                    target: "imagent::core",
                    grace = ?self.shutdown_grace,
                    "drain 超时，abort 剩余 in-flight task"
                );
                tasks.abort_all();
            }
        }
        Ok(())
    }

    /// 取（或创建）conv 串行锁的 Arc clone。
    /// P1-F：slash 命令复用，与普通消息 agent task 串行（避免并发改 session 损坏状态）。
    /// 回收沿用普通消息路径的 release（strong_count==1 时 remove）；slash 路径不显式
    /// release，其 lock clone drop 后由下次普通消息 release 清理（延迟回收，最终回收）。
    async fn acquire_conv_lock(&self, conv: &str) -> Arc<Mutex<()>> {
        let mut map = self.conv_locks.lock().await;
        map.entry(conv.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// 回收 conv 串行锁（P1-7：失败/正常路径统一调用，防 conv_locks HashMap 项
    /// 在 backend 失败/panic 的 return 路径永久泄漏）。调用方需先 drop guard 再传 lock。
    /// strong_count==1 表示只剩 HashMap 那份，安全移除；竞态最坏漏清（下轮再来）。
    async fn release_conv_lock(&self, conv: &str, lock: Arc<Mutex<()>>) {
        drop(lock);
        let mut map = self.conv_locks.lock().await;
        if let Some(arc) = map.get(conv) {
            if Arc::strong_count(arc) == 1 {
                map.remove(conv);
            }
        }
    }

    /// 普通消息入队（P4-2 批处理）：runner 在飞 → push pending 返回 false（本 task
    /// 即返，消息将在下一轮合并）；无 runner → 建 entry 返回 true（调用方成为
    /// runner）。入队/成为 runner 在同一把 queues 锁内原子判定——与
    /// [`take_batch_after_window`](Self::take_batch_after_window) 的取批/交还互斥，
    /// 杜绝「消息卡在无人认领的队列」（lost-wakeup）。超上限回告警并丢弃。
    async fn enqueue_or_become_runner(
        &self,
        conv: &str,
        msg: InboundMessage,
        hint: &ReplyHint,
    ) -> bool {
        let mut map = self.queues.lock().await;
        match map.get_mut(conv) {
            Some(pending) => {
                if pending.len() >= PENDING_QUEUE_CAP {
                    drop(map);
                    warn!(
                        target: "imagent::core",
                        conv_id = %conv,
                        cap = PENDING_QUEUE_CAP,
                        "排队消息超上限，丢弃本条"
                    );
                    self.reply(
                        &ConvId(conv.to_string()),
                        &format!("⚠️ 排队消息已达上限（{PENDING_QUEUE_CAP} 条），本条已丢弃；如需立即处理请发 /stop 中断当前任务后重发"),
                        hint,
                    )
                    .await;
                    return false;
                }
                info!(target: "imagent::core", conv_id = %conv, "runner 在飞，消息入队待下一轮合并");
                pending.push(msg);
                // S-3（P10）：锁内只做入队与快照，hint 写入 / note 推送（网络 IO）
                // 移到 drop(map) 之后——与上方上限分支同款纪律，不在 queues 锁
                // 持有期间 await。
                let count = pending.len();
                let latest = latest_snippet(&pending[pending.len() - 1]);
                drop(map);
                // P10：排队状态上卡——①②流式卡 footer 由 CardSession 下次 patch 拉取；
                // ③审批等待是最静默的窗口（无 chunk，footer 不动），推送重渲染审批卡
                // note 行（best-effort）。两者都是状态更新，不往消息流发任何东西。
                let note = format!("⏳ 等待你审批 · 后面还排着 {count} 条消息");
                self.queued_hints.lock().await.insert(
                    conv.to_string(),
                    crate::card_session::QueuedHint { count, latest },
                );
                if let Err(e) = self
                    .platform
                    .note_queued_on_ask(&ConvId(conv.to_string()), &note, hint)
                    .await
                {
                    tracing::debug!(target: "imagent::core", error = %e, "审批卡排队 note 更新失败（不影响排队）");
                }
                // S-3/S-4 竞态兜底：锁外写 hint 期间本批可能已被 runner 取走（queues
                // entry 已移除、hint 已清）——复查一次，entry 不在则撤回 stale hint。
                if self.queues.lock().await.get(conv).is_none() {
                    self.queued_hints.lock().await.remove(conv);
                }
                false
            }
            None => {
                map.insert(conv.to_string(), vec![msg]);
                true
            }
        }
    }

    /// runner 起跑前等批处理窗口，然后原子取批：pending 空 → 删 entry（交还 runner
    /// 身份）返回 None；非空 → drain 返回 Some（窗口期入队的消息自然并入本批）。
    ///
    /// W1-5（自适应窗口）：出批条件从「固定睡一个窗口」改为「**静默一个窗口**」
    /// ——连发未停（每窗口内仍有新消息入队）则继续等，硬上限（3× 窗口、封顶
    /// 10s）防持续输入把一轮无限推迟。单人单条消息路径与旧语义等价（睡一个
    /// 窗口、无新消息即出批）；用户长连发时不再被窗口边界切成多轮。
    async fn take_batch_after_window(&self, conv: &str) -> Option<Vec<InboundMessage>> {
        let window = *self.batch_window.read();
        if !window.is_zero() {
            const TOTAL_CAP_MS: u64 = 10_000;
            let cap = window
                .saturating_mul(3)
                .min(Duration::from_millis(TOTAL_CAP_MS));
            let started = Instant::now();
            let mut last_len = self.queues.lock().await.get(conv).map_or(0, |q| q.len());
            loop {
                tokio::time::sleep(window).await;
                let now_len = self.queues.lock().await.get(conv).map_or(0, |q| q.len());
                if now_len == last_len || started.elapsed() >= cap {
                    break;
                }
                last_len = now_len;
            }
        }
        let mut map = self.queues.lock().await;
        let pending = map.get_mut(conv)?;
        if pending.is_empty() {
            map.remove(conv);
            return None;
        }
        // S-4（原子）：取批与清 hint 在**同一** queues 临界区内完成——先清后取
        // 分离会有间隙：/stop 或新入队消息在两步之间落地导致 hint 与实际队列错位。
        // P10：本批转入处理——排队提示清零（本轮运行中新入队的会重新累积，
        // 展示在下一张卡 / 下一轮）。
        self.queued_hints.lock().await.remove(conv);
        Some(std::mem::take(pending))
    }

    /// 消息撤回（一期）：按平台消息 id 把同 id 的**排队**消息移出。全队列扫描——
    /// 撤回事件携带的会话 key（chat_id 形态）与排队 key（私聊为发送者 conv）可能
    /// 不同形，按 id 匹配最稳。命中则同步收缩排队提示（count/latest，空队列清
    /// hint；锁序 queues→queued_hints 与入队/取批路径一致）。返回命中条数。
    async fn remove_queued_by_msg_id(&self, msg_id: &str) -> usize {
        let mut map = self.queues.lock().await;
        let mut removed = 0usize;
        for (conv, pending) in map.iter_mut() {
            let before = pending.len();
            pending.retain(|m| m.source_msg_id.as_deref() != Some(msg_id));
            if pending.len() == before {
                continue;
            }
            removed += before - pending.len();
            let count = pending.len();
            let latest = pending.last().map(latest_snippet).unwrap_or_default();
            let mut hints = self.queued_hints.lock().await;
            if count == 0 {
                hints.remove(conv);
            } else {
                hints.insert(
                    conv.clone(),
                    crate::card_session::QueuedHint { count, latest },
                );
            }
        }
        removed
    }

    /// 平台控制信号分发（撤回 / bot 被移出群等系统事件的合成载体；handle 顶部、
    /// 白名单校验**之前**调用——控制信号不是用户对话输入，不应被鉴权丢弃）。
    async fn handle_control(&self, msg: InboundMessage) {
        let Some(control) = msg.control else {
            return;
        };
        match control {
            crate::types::InboundControl::MessageRecalled {
                notify_conv,
                probe_convs,
            } => {
                // 一期语义：只移出**排队**消息；已被 runner 取走（在飞/已执行）的
                // 不追杀——执行中仅回提示（不自动停，半途结果可能仍有价值），
                // 未入队/已执行完的静默忽略。
                let Some(msg_id) = msg.source_msg_id.clone().filter(|s| !s.is_empty()) else {
                    return;
                };
                let removed = self.remove_queued_by_msg_id(&msg_id).await;
                if removed > 0 {
                    info!(
                        target: "imagent::core",
                        message_id = %msg_id,
                        count = removed,
                        "消息撤回：已移出排队消息（下一轮不再合并）"
                    );
                    return;
                }
                let running_here = {
                    let running = self.running.lock().await;
                    probe_convs.iter().any(|c| running.contains_key(&c.0))
                };
                if running_here {
                    if let Some(conv) = notify_conv {
                        self.reply(
                            &conv,
                            "ℹ️ 消息已撤回，但对应任务已开始执行；如需中断可发送 /stop。",
                            &msg.reply_hint,
                        )
                        .await;
                    }
                }
            }
            crate::types::InboundControl::BotRemovedFromChat => {
                // bot 被移出群（飞书 im.chat.member.bot.deleted_v1）：收回群授权，
                // 防「群已失联但白名单仍在」的僵尸条目。内存 + store 双写 + 审计
                // （与 /chat deny 同族；auth.revoke_chat 即 remove_chat 语义）。
                warn!(
                    target: "imagent::core",
                    conv_id = %msg.conv_id.0,
                    "bot 已被移出群，收回会话白名单授权"
                );
                if !self.auth.revoke_chat(&msg.conv_id.0) {
                    return; // 本就不在白名单：无需清理，也不打扰管理员。
                }
                if let Err(e) = self.store.remove_allowed_chat(&msg.conv_id.0).await {
                    warn!(
                        target: "imagent::core",
                        error = %e,
                        "移出群的白名单持久化失败（内存已移除，重启后需重新 /chat deny）"
                    );
                }
                let _ = self
                    .store
                    .append_audit(
                        "chat_bot_removed",
                        None,
                        Some(&msg.conv_id.0),
                        Some("auto: bot removed from chat"),
                    )
                    .await;
                // 通知首位管理员（私聊）。该事件目前仅飞书产生，admin sender 即
                // open_id，直接拼私聊 conv。
                let admin = self.admin_senders.read().first().cloned();
                if let Some(admin) = admin {
                    self.reply(
                        &ConvId(format!("feishu:{admin}")),
                        &format!("🤖 bot 已被移出群 {}，已从会话白名单移除。", msg.conv_id.0),
                        &msg.reply_hint,
                    )
                    .await;
                }
            }
        }
    }

    /// 统一 `/resume` 列表（P4-11）：IM 会话历史（store `session_history`）∪
    /// 本机同项目 agent 会话（`Backend::list_local_sessions`，按 conv 当前
    /// workdir 扫描——workdir 对齐由扫描天然保证；`/cd` 切换后列表随之变化）。
    ///
    /// 归属标注：在历史表里的 id 标 📱（IM 创建，含也被扫到的），仅本机扫描出的
    /// 标 💻；历史里有但扫描没有的（其它 backend 会话/文件已删）仍列出（📱）。
    /// 按时间倒序取前 10。
    async fn merged_resume_list(&self, conv: &str) -> Vec<ResumeEntry> {
        const MAX: usize = 10;
        let history = self
            .store
            .list_session_history(conv, 50)
            .await
            .unwrap_or_default();
        let hist_kinds: HashMap<String, Option<String>> = history
            .iter()
            .map(|r| (r.session_id.clone(), r.agent_kind.clone()))
            .collect();
        let wd = self.resolve_workdir(conv).await;
        let local = self.backend.list_local_sessions(&wd).await;
        let backend_name = self.backend.name().to_string();
        let mut seen: std::collections::HashSet<String> = Default::default();
        let mut entries: Vec<ResumeEntry> = local
            .into_iter()
            .map(|l| {
                seen.insert(l.session_id.clone());
                ResumeEntry {
                    agent_kind: hist_kinds
                        .get(&l.session_id)
                        .cloned()
                        .flatten()
                        .unwrap_or_else(|| backend_name.clone()),
                    from_local: !hist_kinds.contains_key(&l.session_id),
                    session_id: l.session_id,
                    updated_at: l.updated_at,
                    first_prompt: l.first_prompt,
                    cwd: l.cwd,
                }
            })
            .collect();
        for r in history {
            if seen.insert(r.session_id.clone()) {
                entries.push(ResumeEntry {
                    session_id: r.session_id,
                    updated_at: r.updated_at,
                    agent_kind: r.agent_kind.unwrap_or_else(|| backend_name.clone()),
                    first_prompt: String::new(),
                    from_local: false,
                    cwd: None,
                });
            }
        }
        entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        entries.truncate(MAX);
        entries
    }

    /// 解析 conv 的工作目录：per-conv KV（`/cd` 设置）覆盖，否则回退 `default_workdir`。
    async fn resolve_workdir(&self, conv_id: &str) -> PathBuf {
        match self.store.get_config(&workdir_key(conv_id)).await {
            Ok(Some(p)) => PathBuf::from(p),
            _ => self.default_workdir.clone(),
        }
    }

    /// P5-5：中断/失败路径保住 backend 已学到（`SessionStarted`）的 session id。
    ///
    /// agent 可能在被 abort 前已建立新会话（如首轮任务跑了几分钟被 /stop 打断、
    /// 或正常完成但无最终文本被 backend 判 Err）——RunOutcome 拿不到，不落库则
    /// 下条消息静默开新会话，用户感知为「agent 失忆」。仅当学到的 id 非空且与本轮
    /// 传入的不同时写（相同 = 续接既有会话，映射未变）；失败仅 log 不影响回复。
    async fn persist_learned_session(
        &self,
        conv: &ConvId,
        existing: Option<&str>,
        learned: &Option<String>,
    ) {
        let Some(sid) = learned.as_deref().filter(|s| !s.is_empty()) else {
            return;
        };
        if Some(sid) == existing {
            return;
        }
        let now = now_secs();
        let active_name = self
            .store
            .get_config(&active_name_key(&conv.0))
            .await
            .unwrap_or(None)
            .filter(|s| !s.is_empty());
        let workdir = self
            .resolve_workdir(&conv.0)
            .await
            .to_string_lossy()
            .to_string();
        let row = SessionRow {
            conv_id: conv.0.clone(),
            session_id: sid.to_string(),
            agent_kind: self.backend.name().to_string(),
            workdir: workdir.clone(),
            name: active_name.clone(),
            created_at: now,
            updated_at: now,
        };
        if let Err(e) = self.store.upsert_session(&row).await {
            warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "中断路径 upsert_session 失败");
            return;
        }
        if let Some(name) = active_name {
            let nrow = NamedSessionRow {
                conv_id: conv.0.clone(),
                name,
                session_id: sid.to_string(),
                agent_kind: Some(self.backend.name().to_string()),
                workdir: Some(workdir),
                created_at: now,
                updated_at: now,
            };
            if let Err(e) = self.store.upsert_named_session(&nrow).await {
                warn!(target: "imagent::core", conv_id = %conv.0, error = %e, "中断路径 upsert_named_session 失败");
            }
        }
        info!(
            target: "imagent::core",
            conv_id = %conv.0,
            session_id = %sid,
            "中断/失败路径已持久化 backend 学到的 session id（下条消息续接）"
        );
    }

    /// 回传文本；发送失败仅 log（见 [`Self::reply_ok`]）。
    async fn reply(&self, conv: &ConvId, text: &str, hint: &ReplyHint) {
        let _ = self.reply_ok(conv, text, hint).await;
    }

    /// P6-3：回命令交互卡片；平台无卡片能力时默认实现已降级纯文本，卡片发送
    /// 失败（权限/网络）在此再兜一层纯文本——命令永远有回执。
    async fn reply_card(
        &self,
        conv: &ConvId,
        title: &str,
        body_md: &str,
        buttons: Vec<CardButton>,
        hint: &ReplyHint,
    ) {
        if let Err(e) = self
            .platform
            .send_command_card(conv, title, body_md, &buttons, hint)
            .await
        {
            warn!(
                target: "imagent::core",
                conv_id = %conv.0,
                error = %e,
                "命令卡片发送失败，降级纯文本"
            );
            let text = crate::platform::command_card_fallback_text(title, body_md, &buttons);
            self.reply(conv, &text, hint).await;
        }
    }

    /// 回传文本，返回是否成功送达。P5-第五批：流式前缀累积据此只记成功送达
    /// 的部分——失败段落留给最终全量兜底，而非两处皆失。session 过期升级为
    /// error（用户侧已收不到回复）。
    ///
    /// S-9：失败至少重试一次（短暂退避，瞬时网络抖动可自愈）；仍失败时记
    /// error 日志并附目标文本前 200 字符（排障定位「哪条消息没送出去」）。
    async fn reply_ok(&self, conv: &ConvId, text: &str, hint: &ReplyHint) -> bool {
        let mut attempt = 0;
        loop {
            match self.platform.send_text(conv, text, hint).await {
                Ok(()) => {
                    METRICS.messages_out.inc();
                    return true;
                }
                Err(e) => {
                    if is_session_expired_err(&e) {
                        // session 过期重试无意义，直接升级 error。
                        tracing::error!(
                            target: "imagent::core",
                            conv_id = %conv.0,
                            error = %e,
                            "send_text session 过期（用户侧已收不到）"
                        );
                        return false;
                    }
                    if attempt == 0 {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        continue; // 首次失败：退避后重试一次
                    }
                    tracing::error!(
                        target: "imagent::core",
                        conv_id = %conv.0,
                        error = %e,
                        retries = attempt,
                        text_head = %truncate_str(text, 200),
                        "send_text 重试后仍失败（附目标文本前 200 字符）"
                    );
                    return false;
                }
            }
        }
    }
}

/// S-5：权限审批 pending 的 Drop 兜底 guard。
///
/// `build_im_permission_hook` 的 future 在任一 await 点被 drop（run 被 /stop
/// abort、agent_timeout / 空闲看门狗超时）时不会走 cancel 分支，pending 会在
/// router 里残留（后续自由文本被误当审批回复吞掉）。guard 在 drop 时向运行时
/// 提交一次 cancel（幂等：pending 已被正常消费/显式清理则 no-op）。
struct PendingCancelGuard {
    router: Arc<PermissionRouter>,
    conv: String,
    request_id: String,
}

impl Drop for PendingCancelGuard {
    fn drop(&mut self) {
        let router = self.router.clone();
        let conv = self.conv.clone();
        let request_id = self.request_id.clone();
        // drop 发生在 runtime 线程（task abort / timeout）时可直接 spawn；脱离
        // runtime（如测试手工 drop）则只能放弃兜底（记日志）。
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(async move {
                    router.cancel(&conv, &request_id).await;
                });
            }
            Err(_) => {
                warn!(
                    target: "imagent::core",
                    conv_id = %self.conv,
                    request_id = %self.request_id,
                    "PendingCancelGuard drop 时无 runtime，无法兜底 cancel pending"
                );
            }
        }
    }
}
