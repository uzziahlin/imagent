//! 流式卡片会话：累积 agent 输出，节流 patch 到支持卡片的平台。
//!
//! 仅 `Platform::supports_streaming_card() == true` 的平台使用（dispatch 据此分支）。
//! 累积 `text` / `tool_calls`，按节流间隔 patch：首次 `send_card` 拿 `message_id`，
//! 后续 `update_card`。最终 `finalize` 强制 patch 终态（Done/Error）。
//!
//! P1-1（code-review v13）：节流的「睡到窗口」此前发生在 chunk 消费路径上——
//! claude-acp 的 Text chunk 是 delta 级（每条几字符），每消费一条先睡 ~500ms
//! 把消费速率钉死在 ~2 条/秒，长轮纯排管道可达 agent_timeout，channel(32) 打满
//! 后 ACP 侧 send 30s 超时丢 delta。现重构为**常驻 patcher 任务**：
//! - 消费方方法（[`CardSession::append_text`] 等）只更新累积状态并推进事件
//!   代数（gen）唤醒 patcher——同步、零睡眠、零平台调用，立即返回；
//! - 节流睡眠与 platform patch 全部在 patcher 任务内（[`CARD_THROTTLE`] 节流
//!   统一 flush，「每个事件最终都会上卡」的尾帧语义由 pending 状态天然保证：
//!   只要 gen ≠ flushed_gen，patcher 一定会睡到窗口边界补发，不依赖后续事件）；
//! - [`CardSession::finalize`] 发终态并**等 patcher 把终态真正发完**（oneshot
//!   ack）才返回——终态先于轮次收尾落定。
//!
//! 方法均不向调用方传播卡片错误——失败在内部 `warn!` 记录（卡片失败不应中断
//! agent 回复）。设计借鉴 lcab 的 `RunState + renderCard + update`，但 core 只产
//! 平台无关的 [`OutboundCard`]，卡片 JSON 渲染由各 Platform 实现。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use crate::platform::Platform;
use crate::types::{CardPhase, CardTerminal, ConvId, OutboundCard, ReplyHint, TodoItem, ToolCall};
use imagent_store::Store;

/// 卡片 patch 节流间隔。飞书交互卡片更新有频率限制，500ms 平衡流畅与限流。
const CARD_THROTTLE: Duration = Duration::from_millis(500);

/// 建卡失败（无句柄时的 send_card 失败）退避窗口：上次失败 2s 内的待发状态
/// 合帧到窗口边界再试一次，而不是每 chunk 全新建卡（实体创建 + 锚定回复），
/// 零节流零退避——与 500ms 节流设计相反的 API 压力与 warn 刷屏（见
/// `CardState::last_create_fail`）。终态不受此约束（finalize 绕过节流）。
const CREATE_FAIL_BACKOFF: Duration = Duration::from_secs(2);

/// W2-1：思考片段累积上限（条数）——只保留最近的思考（旧思考对用户无回看价值，
/// 无上限会把卡片 payload 撑爆）。
const MAX_THOUGHTS: usize = 10;

/// W2-1：单条思考片段的字符截断上限（防超长推理占满卡片）。
const THOUGHT_TRUNC_CHARS: usize = 400;

/// P10/T18：dispatcher 侧 per-conv 状态表句柄（patcher 每次 patch 拉取该
/// conv 的排队提示快照；原为独立的 queued_hints 表句柄，T18 并入 ConvState
/// 单表后共享整表——锁内只做 get + clone 的纯内存快照）。
type QueuedHints = crate::dispatch::ConvStates;

/// P10：本会话的排队状态（运行中入队的消息摘要）。入队路径写、取批/中断清、
/// CardSession 每次 patch 拉取（活动期随 chunk 刷新 footer 的排队提示）。
/// v1.18：`steered` 计运行中转向注入的条数（👀 回执之外的卡面可见性——
/// 真机反馈表情太隐蔽，两次误判「消息丢了」），轮次结束清零。
#[derive(Debug, Clone, Default)]
pub(crate) struct QueuedHint {
    /// 排队消息条数。
    pub count: usize,
    /// 最新一条的摘要（≤40 字符；纯媒体消息给「（图片/文件）」占位）。
    pub latest: String,
    /// 本轮 steering 注入条数（footer「已注入 N 条」）。
    pub steered: usize,
}

/// 状态 → 展示文案（None = 无需展示）。`📥 已注入 N 条 · 排队 M 条，最新：「…」`。
pub(crate) fn queued_hint_display(h: &QueuedHint) -> Option<String> {
    let mut segs: Vec<String> = Vec::new();
    if h.steered > 0 {
        segs.push(format!("已注入 {} 条运行中消息", h.steered));
    }
    if h.count > 0 {
        let mut q = format!("排队 {} 条", h.count);
        if !h.latest.is_empty() {
            let latest: String = h.latest.chars().take(40).collect();
            q.push_str(&format!("，最新：「{latest}」"));
        }
        segs.push(q);
    }
    (!segs.is_empty()).then(|| format!("📥 {}", segs.join("，")))
}

/// 流式卡片会话。
/// 卡片成功终态下需要补发全文文本的字节阈值（真机校准 2026-08-30）。
/// R2（code-review v9）：取值必须**低于**平台侧 4KB+4KB（8192）头尾窗——
/// 此前 8500 在 (8192, 8500] 区间留下「卡被截断却不补发」的空洞；正文
/// 超 8KB ⇒ 卡片 md 必超窗被截 ⇒ 必补发。略低于窗防 md 头部（状态行等）
/// 占用造成的边界漏发；短内容卡+文本双发噪音的代价可接受（8KB 以上本就
/// 不适合只读卡）。
const CARD_TEXT_FULL_THRESHOLD: usize = 8_000;

/// patcher 任务的固定环境（轮次内不变量：conv / hint / platform 在一轮里恒定，
/// 构造时一次性捕获——patcher 是 'static 任务，拿不到调用方的 `&dyn Platform`）。
struct PatchEnv {
    /// 在飞卡片登记（P4_ROADMAP 第六批）：首帧句柄落库、终态成功摘除——进程崩溃
    /// 后由 [`sweep_live_cards`] 启动扫描把滞留「生成中」的卡片 patch 成已中断。
    store: Store,
    conv: ConvId,
    /// 轮次回复 hint（打在本轮触发消息的上下文）。
    hint: ReplyHint,
    platform: Arc<dyn Platform>,
    platform_name: &'static str,
    /// P10：dispatcher 的排队状态句柄（每次 patch 拉取，见 [`queued_hint_display`]）。
    queued_hints: QueuedHints,
}

/// 消费方与 patcher 共享的可变状态。`std::sync::Mutex` 短临界区访问——
/// **绝不跨 await 持有**（clippy::await_holding_lock 把关）：platform/store IO
/// 期间消费方仍要写入累积字段，state 锁一旦跨 await 就会把消费路径重新钉死在
/// 平台延迟上，背刺本次重构。字段写权分属：累积字段（text/tools/thoughts/
/// todos/phase/usage_display/gen）由消费方写、patcher 读快照；句柄与时钟字段
/// （msg_id/last_patch/last_create_fail/flushed_gen/finished）只由 patcher 写
/// （finalize 的终态写入经 `final_pending` 转交 patcher 执行）。
struct CardState {
    text: String,
    /// v1.24 卡片 UX：任务摘要（首条 prompt 前 N 字）。现状未入 `OutboundCard`
    /// （dispatch_patch 渲染时恒 None，历史遗留；保留字段与写入路径供后续接入），
    /// 故更新不触发 patch。
    task_digest: Option<String>,
    tools: Vec<ToolCall>,
    /// W2-1：思考片段（最近 MAX_THOUGHTS 条，单条截断 THOUGHT_TRUNC_CHARS）。
    thoughts: Vec<String>,
    /// W2-2：任务清单（全量替换语义——最新一次 TodoList chunk 为准）。
    todos: Vec<TodoItem>,
    /// P8-1：执行阶段（思考中/调用工具/输出中）——按最近一次 chunk 类型翻转，
    /// 平台渲染成分状态 footer。
    phase: CardPhase,
    msg_id: Option<String>,
    last_patch: Instant,
    /// v1.18 review：无句柄时的**建卡失败**退避时钟。D9 只覆盖有句柄的 update
    /// 重试（last_patch 仅成功推进）；send_card 持续失败（权限缺失/持续限流）
    /// 时若不退避，每 chunk 都触发一次全新建卡（实体创建 + 锚定回复）——与
    /// 500ms 节流设计相反的 API 压力与 warn 刷屏。None = 最近建卡成功（或
    /// 从未失败），走常规节流。
    last_create_fail: Option<Instant>,
    /// 轮次起点（Running footer 运行时长的基准，见 [`OutboundCard::run_secs`]）。
    started: Instant,
    /// 本轮成本摘要（`UsageStats.display()`）——run 结束时由 round 写入，终态
    /// footer 追加展示（`✅ 已完成 · $0.012`）；None = backend 未产出 usage。
    usage_display: Option<String>,
    /// 事件代数：消费方每次状态更新 +1。gen ≠ flushed_gen 即有待上卡状态。
    gen: u64,
    /// patcher 已 flush 的代数。
    flushed_gen: u64,
    /// finalize 请求（终态 + ack 通道）。置入后 patcher 优先处理：绕过节流发
    /// 终态 patch，ack 回传成败后退出。
    final_pending: Option<(CardTerminal, oneshot::Sender<bool>)>,
    /// 终态已发（patcher 置位）：此后绝不能再发 Running 帧（会把 Done/Error 卡
    /// 翻回「生成中」），Drop 收尾 flush 据此让路。
    finished: bool,
}

/// 流式卡片会话（见模块文档的 patcher 架构说明）。
///
/// 生命周期：`Drop`（含持有它的轮次 future 被 abort——/stop、看门狗、总超时）
/// → wake 通道关闭 → patcher 补发最后的脏帧后自行退出；未走到 `finalize` 的
/// 卡片滞留「生成中」，由 [`sweep_live_cards`] 启动扫描兜底（与进程崩溃同款）。
pub(crate) struct CardSession {
    env: Arc<PatchEnv>,
    inner: Arc<Mutex<CardState>>,
    /// patcher 唤醒通道：send 方随本结构 Drop 关闭，即 patcher 的「收尾并退出」
    /// 信号（unbounded：无丢失唤醒窗口，消费方 send 永不阻塞）。
    wake: mpsc::UnboundedSender<()>,
    /// finalize 只允许一次（重复调用直接忽略——patcher 处理完首个请求即退出，
    /// 第二次 ack 会永等）。
    finalized: bool,
}

impl CardSession {
    /// 构造即起常驻 patcher 任务（见模块文档）。调用方须在 tokio 上下文。
    pub(crate) fn new(
        store: Store,
        conv: ConvId,
        platform: Arc<dyn Platform>,
        hint: ReplyHint,
        queued_hints: QueuedHints,
    ) -> Self {
        let env = Arc::new(PatchEnv {
            store,
            conv,
            hint,
            platform_name: platform.name(),
            platform,
            queued_hints,
        });
        let state = Arc::new(Mutex::new(CardState {
            text: String::new(),
            task_digest: None,
            tools: Vec::new(),
            thoughts: Vec::new(),
            todos: Vec::new(),
            phase: CardPhase::Thinking,
            msg_id: None,
            last_patch: Instant::now(),
            last_create_fail: None,
            started: Instant::now(),
            usage_display: None,
            gen: 0,
            flushed_gen: 0,
            final_pending: None,
            finished: false,
        }));
        let (wake_tx, wake_rx) = mpsc::unbounded_channel();
        tokio::spawn(patcher_task(env.clone(), state.clone(), wake_rx));
        Self {
            env,
            inner: state,
            wake: wake_tx,
            finalized: false,
        }
    }

    /// 消费方更新入口：锁内改累积状态并推进事件代数（= 有待上卡状态），锁外
    /// 唤醒 patcher。**不做任何 sleep / 平台调用**——chunk 消费路径零阻塞，
    /// 这正是 P1-1 的核心约束。
    fn touch(&self, mutate: impl FnOnce(&mut CardState)) {
        {
            let mut s = self.inner.lock().unwrap();
            mutate(&mut s);
            s.gen += 1;
        }
        // unbounded send 永不阻塞；patcher 已退出（finalize 后）时静默忽略。
        let _ = self.wake.send(());
    }

    /// v1.24 任务摘要（见 `CardState::task_digest` 文档：现状不触发 patch）。
    pub(crate) fn set_task_digest(&self, digest: Option<String>) {
        self.inner.lock().unwrap().task_digest = digest;
    }

    /// 本轮成本摘要（成功终态 footer `✅ 已完成 · $0.012`）。轮末 finalize 前
    /// 写入（gen 推进无实际意义——终态帧由 final_pending 优先驱动，保持一致
    /// 仅为语义完备）。
    pub(crate) fn set_usage_display(&self, usage: Option<String>) {
        self.touch(|s| s.usage_display = usage);
    }

    /// 请求发出初始卡片（若尚未发）。真机校准 UX：agent 首 chunk 前有数秒到
    /// 十几秒静默期（CLI 冷启动 + 模型首 token），轮次开始即发「执行中」卡，
    /// 用户才确知消息已被接收处理。飞书 send_card 用固定初始模板（打字机基座），
    /// 与本方法的无内容 dispatch 天然契合。patcher 即刻建卡（无句柄时不受
    /// 500ms 节流，仅受建卡失败退避约束）。
    pub(crate) fn ensure_started(&self) {
        let due = {
            let mut s = self.inner.lock().unwrap();
            if s.msg_id.is_none() {
                s.gen += 1;
                true
            } else {
                false
            }
        };
        if due {
            let _ = self.wake.send(());
        }
    }

    /// v1.23 审批等待可视化：chunk 静默期的心跳 patch——卡片 footer 的运行
    /// 时长继续走动（run_secs 随 patch 刷新），审批等待时阶段翻
    /// WaitingApproval。仅在距上次 patch ≥ HEARTBEAT 时动作（不冲击 500ms
    /// 节流语义）。`waiting` = 当前有权限审批 pending。
    /// P3-d（code-review v14）：无句柄（msg_id=None）不再短路——建卡瞬时失败
    /// （限流/网络抖动）后 patcher 已把 flushed_gen 追平，若无新事件就停留在
    /// Idle 永不重试建卡；「建卡失败 + 首 token 长静默」期间用户完全看不到
    /// 「任务已接收」卡。心跳改为无句柄也 bump gen 驱动 patcher 重试建卡：
    /// 重试节奏受 CREATE_FAIL_BACKOFF（last_create_fail 2s 窗口，见
    /// [`patch_delay`]）自然约束，成功拿到句柄后恢复常规 25s 心跳，不会风暴。
    pub(crate) fn heartbeat(&self, waiting: bool) {
        const HEARTBEAT: Duration = Duration::from_secs(25);
        let due = {
            let mut s = self.inner.lock().unwrap();
            if s.msg_id.is_none() {
                // 无句柄：无 last_patch 可依（从未成功 patch 过），每次心跳都
                // 推进 gen 让 patcher 重试建卡（退避窗口内 patcher 睡到边界）。
                if waiting {
                    s.phase = CardPhase::WaitingApproval;
                }
                s.gen += 1;
                true
            } else if s.last_patch.elapsed() < HEARTBEAT {
                false
            } else {
                if waiting {
                    s.phase = CardPhase::WaitingApproval;
                }
                s.gen += 1;
                true
            }
        };
        if due {
            let _ = self.wake.send(());
        }
    }

    /// 累积文本增量（Running 态由 patcher 节流上卡）；阶段翻到「输出中」。
    pub(crate) fn append_text(&self, text: &str) {
        self.touch(|s| {
            s.text.push_str(text);
            s.phase = CardPhase::Outputting;
        });
    }

    /// 累积工具调用（⏳ 执行中）；阶段翻到「调用工具」。
    /// W2-3：`id` 供结果精确配对（None = 后端未提供）。
    pub(crate) fn append_tool(&self, tool: &str, input_summary: &str, id: Option<&str>) {
        self.touch(|s| {
            s.tools.push(ToolCall {
                name: tool.to_string(),
                summary: input_summary.to_string(),
                done: false,
                id: id.map(str::to_string),
            });
            s.phase = CardPhase::ToolRunning;
        });
    }

    /// P8-1：工具结果到达——翻 ✅（W2-3：优先按 id 精确配对，无 id 回退同名
    /// 最早未完成；同名并发极少见，错配只影响图标不影响内容）。
    pub(crate) fn finish_tool(&self, tool: &str, id: Option<&str>) {
        self.touch(|s| {
            // W2-3：优先按 id 精确配对（首个借用先落地结束，再做名字兜底——避免
            // 链式 or_else 的双重可变借用）。
            let by_id = match id {
                Some(i) => s
                    .tools
                    .iter_mut()
                    .find(|t| !t.done && t.id.as_deref() == Some(i)),
                None => None,
            };
            let target = match by_id {
                Some(t) => Some(t),
                None => s.tools.iter_mut().find(|t| !t.done && t.name == tool),
            };
            if let Some(t) = target {
                t.done = true;
            }
        });
    }

    /// W2-1：累积思考片段（最近 N 条、单条截断）；阶段保持 Thinking。
    pub(crate) fn append_thought(&self, thought: &str) {
        let t: String = thought.chars().take(THOUGHT_TRUNC_CHARS).collect();
        if t.trim().is_empty() {
            return;
        }
        self.touch(|s| {
            s.thoughts.push(t);
            if s.thoughts.len() > MAX_THOUGHTS {
                s.thoughts.remove(0);
            }
        });
    }

    /// W2-2：任务清单（全量替换）。
    pub(crate) fn set_todos(&self, items: &[TodoItem]) {
        self.touch(|s| s.todos = items.to_vec());
    }

    /// 最终 patch：用 `final_text` 覆盖累积文本，合并 dispatch 侧累积的
    /// `extra_tools`，**绕过节流**发终态 patch（不受 500ms 窗口，确保
    /// Done/Error 显示），并等 patcher 把终态真正发完（oneshot ack）才返回——
    /// 终态必须先于轮次收尾落定（/stop、空闲看门狗、panic 等所有收尾路径都经
    /// 此收敛，卡片不会停在「生成中」）。
    ///
    /// P5-11：终态 patch 失败（网络抖动 / 限流 / 卡片服务异常）时降级纯文本
    /// 补发——流式卡片可以停在「生成中」，但结论不能丢（用户至少拿到完整文本）。
    pub(crate) async fn finalize(
        &mut self,
        final_text: Option<&str>,
        extra_tools: &[ToolCall],
        terminal: CardTerminal,
    ) {
        if self.finalized {
            // 轮次内 finalize 只应调用一次；重复调用（调用方 bug）不能让第二次
            // ack 永等（patcher 处理完首个请求即退出）。
            warn!(target: "imagent::core", "CardSession::finalize 重复调用（忽略）");
            return;
        }
        self.finalized = true;
        let (ack_tx, ack_rx) = oneshot::channel();
        {
            let mut s = self.inner.lock().unwrap();
            if let Some(f) = final_text {
                s.text.clear();
                s.text.push_str(f);
            }
            for t in extra_tools {
                // 按 (name, summary) 去重合并（done 标志可能不同步——以已存在的记录为准）。
                if !s
                    .tools
                    .iter()
                    .any(|e| e.name == t.name && e.summary == t.summary)
                {
                    s.tools.push(t.clone());
                }
            }
            s.final_pending = Some((terminal, ack_tx));
        }
        let _ = self.wake.send(());
        // ack 失败 = patcher 意外消失（panic 等）——按卡片失败处理，走 P5-11
        // 文本兜底，结论不丢。
        let card_ok = ack_rx.await.unwrap_or(false);
        let text = self.inner.lock().unwrap().text.clone();
        if !card_ok && !text.is_empty() {
            match self
                .env
                .platform
                .send_text(&self.env.conv, &text, &self.env.hint)
                .await
            {
                Ok(()) => warn!(target: "imagent::core", "卡片终态更新失败，已降级纯文本补发结论"),
                Err(e) => warn!(
                    target: "imagent::core",
                    error = %e,
                    "卡片终态更新失败，纯文本补发也失败（结论丢失）"
                ),
            }
        } else if card_ok && text.len() > CARD_TEXT_FULL_THRESHOLD {
            // 真机校准（2026-08-30）：卡片正文受字节上限截断（飞书侧 4KB+4KB
            // 头尾窗）但 patch **成功**时，此前不补发全文——卡片标注「完整内容
            // 见文本消息」却没有那条文本。超阈值的成功终态主动补发全文文本。
            if let Err(e) = self
                .env
                .platform
                .send_text(&self.env.conv, &text, &self.env.hint)
                .await
            {
                warn!(target: "imagent::core", error = %e, "截断卡全文补发失败（卡片内仍有头尾窗口）");
            }
        }
    }
}

/// patcher 单步决策。
enum Step {
    /// 无待发状态：等唤醒（或通道关闭 = 会话 Drop → 退出收尾）。
    Idle,
    /// 有待发但未到节流/退避窗口边界：睡到边界（可被唤醒打断重判——终态与
    /// Drop 不被节流推迟）。
    Sleep(Duration),
    /// 到点：发一帧 Running patch。
    FlushRunning,
    /// finalize 请求：绕过节流发终态 patch，ack 回传成败后退出。
    Final(CardTerminal, oneshot::Sender<bool>),
}

/// 距下一次 flush 需等待的时长（None = 立即）：有句柄走 500ms 节流窗（D9：
/// 时钟仅成功推进，失败后重试不被节流跳过）；无句柄且最近建卡失败走 2s 退避
/// 窗；其余（首帧 / 窗口已过）立即。
fn patch_delay(s: &CardState) -> Option<Duration> {
    if s.msg_id.is_some() {
        let since = s.last_patch.elapsed();
        (since < CARD_THROTTLE).then(|| CARD_THROTTLE - since)
    } else if let Some(ts) = s.last_create_fail {
        let since = ts.elapsed();
        (since < CREATE_FAIL_BACKOFF).then(|| CREATE_FAIL_BACKOFF - since)
    } else {
        None
    }
}

/// 常驻 patcher 任务（P1-1）：消费方推进 gen 即被唤醒，按 [`CARD_THROTTLE`]
/// 节流统一 flush；finalize 请求优先且绕过节流。wake 通道关闭（CardSession
/// Drop）时补发最后的脏帧后退出——终态无人发的场景（轮次 future 被 abort）
/// 卡片滞留「生成中」，由 [`sweep_live_cards`] 启动扫描兜底。
async fn patcher_task(
    env: Arc<PatchEnv>,
    state: Arc<Mutex<CardState>>,
    mut wake: mpsc::UnboundedReceiver<()>,
) {
    loop {
        let step = {
            let mut s = state.lock().unwrap();
            if let Some((terminal, ack)) = s.final_pending.take() {
                Step::Final(terminal, ack)
            } else if s.gen != s.flushed_gen {
                match patch_delay(&s) {
                    Some(d) => Step::Sleep(d),
                    None => Step::FlushRunning,
                }
            } else {
                Step::Idle
            }
        };
        match step {
            Step::Idle => {
                if wake.recv().await.is_none() {
                    break; // CardSession 已 Drop
                }
            }
            Step::Sleep(d) => {
                tokio::select! {
                    _ = tokio::time::sleep(d) => {}
                    // 窗口内新事件 / finalize / Drop 到达：立刻重判（终态与
                    // 收尾不被节流窗口推迟）。
                    r = wake.recv() => { if r.is_none() { break; } }
                }
            }
            Step::FlushRunning => {
                let snap_gen = state.lock().unwrap().gen;
                dispatch_patch(&env, &state, CardTerminal::Running).await;
                let mut s = state.lock().unwrap();
                // 成败都标记「本代已处理」：失败重试由下一个事件（gen 推进）
                // 触发——与旧实现「每个事件至多尝试一次 patch」的重试节奏一致，
                // 避免持久故障下 patcher 无事件驱动的热循环。patch 期间有新
                // 事件到达（gen 变化）则保持待发，下轮窗口边界补上。
                if s.gen == snap_gen {
                    s.flushed_gen = s.gen;
                }
            }
            Step::Final(terminal, ack) => {
                let ok = dispatch_patch(&env, &state, terminal).await;
                {
                    let mut s = state.lock().unwrap();
                    s.finished = true;
                    s.flushed_gen = s.gen;
                }
                let _ = ack.send(ok);
                break;
            }
        }
    }
    // Drop 收尾：未 finalize（finished=false）且仍有脏状态 → 补最后一帧
    // Running（卡面尽量新鲜；终态无人发，滞留卡交启动扫描）。已 finalize 则
    // 什么都不做——绝不能在终态之后再发 Running 帧把 Done/Error 卡翻回
    // 「生成中」。
    let pending = {
        let s = state.lock().unwrap();
        !s.finished && s.gen != s.flushed_gen
    };
    if pending {
        dispatch_patch(&env, &state, CardTerminal::Running).await;
    }
}

/// 执行一次卡片发送/更新（原 `dispatch_card` 逻辑）：首次 `send_card` 拿句柄 +
/// live_cards 登记、后续 `update_card`、句柄丢失自愈、终态成功摘除登记。
/// 仅由 patcher 任务调用（单任务串行 ⇒ msg_id / last_patch 等句柄与时钟字段
/// 无并发写）。**任何时刻不得持有 state 锁**（platform/store 的 await 期间
/// chunk 消费方仍需写入累积字段——消费路径绝不能被平台 IO 阻塞，这正是
/// P1-1 的核心约束；clippy::await_holding_lock 把关）。锁序：先 ConvState
/// 表（短暂持有，取排队提示快照）后 state，无嵌套。
async fn dispatch_patch(env: &PatchEnv, state: &Mutex<CardState>, terminal: CardTerminal) -> bool {
    // T11：patch 全路径计时（平台 send/update + live_cards 登记落库）——消费侧
    // 可观测指标 imagent_card_patch_seconds 的唯一观测点。函数单出口，结尾 observe。
    let patch_started = Instant::now();
    // ① 快照：排队提示（ConvState 表短临界区，无 await）+ 卡片内容（state）。
    let queued = env
        .queued_hints
        .lock()
        .await
        .get(&env.conv.0)
        .and_then(|cs| cs.queued_hint_display());
    let (card, msg_id) = {
        let s = state.lock().unwrap();
        let card = OutboundCard {
            task_digest: None,
            text: s.text.clone(),
            tool_calls: s.tools.clone(),
            thoughts: s.thoughts.clone(),
            todos: s.todos.clone(),
            phase: s.phase,
            queued_hint: queued,
            // 运行时长：Running 态 10s 量化（footer 去重缓存按内容比对，量化后
            // 10s 内 footer 不变、不触发 patch，防高频更新）；**终态写全量秒数**
            // （Wave B-3：`✅ 已完成 · 30m · $0.012` 的总耗时来源——量化/清零都会
            // 让用户看到的完成时长失真）。终态 footer 不走去重缓存（每次终态只
            // patch 一次），全量值无刷屏风险。
            run_secs: if matches!(terminal, CardTerminal::Running) {
                (s.started.elapsed().as_secs() / 10) * 10
            } else {
                s.started.elapsed().as_secs()
            },
            usage_display: s.usage_display.clone(),
            terminal: terminal.clone(),
        };
        (card, s.msg_id.clone())
    };
    // ② platform 调用（state 锁外）。副作用经标记延迟到 ④ 提交：
    // `handle_write` = 需写回的句柄（Some(None) = 句柄丢失清空；Some(Some(h)) =
    // send_card 拿到的新句柄）；`create_fail` = 本次 send_card 失败（退避时钟）。
    let mut handle_write: Option<Option<String>> = None;
    let mut create_fail = false;
    let res: crate::error::Result<()> = match msg_id.as_deref() {
        None => match env.platform.send_card(&env.conv, &card, &env.hint).await {
            Ok(id) => {
                handle_write = Some(id);
                Ok(())
            }
            Err(e) => {
                create_fail = true;
                Err(e)
            }
        },
        Some(mid) => match env
            .platform
            .update_card(&env.conv, mid, &card, &env.hint)
            .await
        {
            Ok(()) => Ok(()),
            // 句柄丢失自愈：平台回报「卡片不存在/已删除」（错误串含
            // CARD_HANDLE_LOST 哨兵，见 types.rs）——原卡片被用户删除/撤回后
            // patch 永远失败。摘除 live_cards 登记（终止启动扫描的无限重试）、
            // 句柄置空，Running 期立即重发一张新卡（句柄换新，继续流式）；
            // 终态不重发（轮次已结束，结论由 P5-11 纯文本兜底）。
            Err(e) if e.to_string().contains(crate::types::CARD_HANDLE_LOST) => {
                warn!(
                    target: "imagent::core",
                    conv_id = %env.conv.0,
                    "流式卡片已被删除/撤回（句柄丢失），重发新卡"
                );
                if let Err(clear_err) = env.store.clear_live_card(&env.conv.0).await {
                    warn!(
                        target: "imagent::core",
                        error = %clear_err,
                        "live_cards 摘除失败（句柄丢失自愈路径）"
                    );
                }
                handle_write = Some(None);
                if matches!(terminal, CardTerminal::Running) {
                    // 重发新卡（send_card 分支的等价重放）：失败如实返回，由
                    // 下一个事件再试（warn + 下帧再试）。
                    match env.platform.send_card(&env.conv, &card, &env.hint).await {
                        Ok(id) => {
                            handle_write = Some(id);
                            Ok(())
                        }
                        Err(send_err) => {
                            create_fail = true;
                            Err(send_err)
                        }
                    }
                } else {
                    Err(e)
                }
            }
            other => other,
        },
    };
    let ok = match res {
        Ok(()) => true,
        Err(e) => {
            warn!(target: "imagent::core", error = %e, "卡片更新失败");
            false
        }
    };
    // ③ live_cards 登记/摘除（store，state 锁外）：新句柄登记（None = 平台降级
    // 纯文本，无卡片可滞留）；终态成功摘除（卡片已闭环，无需启动扫描兜底）。
    // 失败保留——结论已降级纯文本补发（P5-11），卡片本身留给下次启动扫描关流。
    if ok {
        if let Some(Some(h)) = &handle_write {
            if let Err(e) = env
                .store
                .record_live_card(&env.conv.0, env.platform_name, h)
                .await
            {
                warn!(
                    target: "imagent::core",
                    error = %e,
                    "live_cards 登记失败（进程若崩溃，该卡片将滞留「生成中」）"
                );
            }
        }
        if !matches!(terminal, CardTerminal::Running) {
            if let Err(e) = env.store.clear_live_card(&env.conv.0).await {
                warn!(
                    target: "imagent::core",
                    error = %e,
                    "live_cards 摘除失败（下次启动会误把已完成的卡片再 patch 一次，无害）"
                );
            }
        }
    }
    // ④ 提交句柄与节流时钟（state 短临界区）。
    {
        let mut s = state.lock().unwrap();
        if let Some(h) = handle_write {
            s.msg_id = h;
            s.last_create_fail = None;
        }
        if create_fail {
            s.last_create_fail = Some(Instant::now());
        }
        // D9：仅成功才推进节流时钟——失败也推进会让紧随其后的重试被节流跳过，
        // 瞬时抖动演变成「整个流式期间不再更新卡片」。
        if ok {
            s.last_patch = Instant::now();
        }
    }
    crate::metrics::METRICS
        .card_patch_seconds
        .observe(patch_started.elapsed().as_secs_f64());
    ok
}

/// 启动扫描（P4_ROADMAP 第六批「孤儿卡片关流」）：把上次进程退出时仍在「生成中」
/// 的流式卡片 patch 成「已中断」终态。P5-11 只覆盖进程活着时的终态 patch；进程
/// 崩溃/被 kill 后卡片无人收尾，本函数在 Start 时按 store 登记逐张关流。
///
/// - patch 成功 → 摘除登记；失败 → 保留（下次启动再试），不阻塞启动。
/// - 平台已切换（登记的平台 ≠ 当前平台）→ 句柄无处 patch，登记作废删除。
/// - `update_card` 默认实现 no-op 且返回 Ok：非卡片平台本不会有登记，兜底无害。
pub async fn sweep_live_cards(store: &Store, platform: &dyn Platform) {
    let rows = match store.list_live_cards().await {
        Ok(r) => r,
        Err(e) => {
            warn!(target: "imagent::core", error = %e, "读取 live_cards 失败，跳过孤儿卡片扫描");
            return;
        }
    };
    for row in rows {
        if row.platform != platform.name() {
            warn!(
                target: "imagent::core",
                conv_id = %row.conv_id,
                card_platform = %row.platform,
                "在飞卡片登记属于其它平台（已切换平台），作废删除"
            );
            let _ = store.clear_live_card(&row.conv_id).await;
            continue;
        }
        let card = OutboundCard {
            task_digest: None,
            text: "⏸️ imagent 已重启，本次生成被中断（未产出结论）。请重新发送指令。".to_string(),
            tool_calls: Vec::new(),
            thoughts: Vec::new(),
            todos: Vec::new(),
            phase: CardPhase::Thinking,
            queued_hint: None,
            run_secs: 0,
            usage_display: None,
            terminal: CardTerminal::Error("进程重启中断".into()),
        };
        let conv = ConvId(row.conv_id.clone());
        match platform
            .update_card(&conv, &row.handle, &card, &ReplyHint::None)
            .await
        {
            Ok(()) => {
                let _ = store.clear_live_card(&row.conv_id).await;
                info!(target: "imagent::core", conv_id = %row.conv_id, "孤儿卡片已关流");
            }
            // 句柄丢失（卡片已被用户删除/撤回）：patch 永远不可能成功——作废登记
            // 而非保留（保留会让每次启动都重试一次注定失败的 patch，无限重试）。
            Err(e) if e.to_string().contains(crate::types::CARD_HANDLE_LOST) => {
                let _ = store.clear_live_card(&row.conv_id).await;
                info!(target: "imagent::core", conv_id = %row.conv_id, "孤儿卡片已不存在（被删除/撤回），作废登记");
            }
            Err(e) => warn!(
                target: "imagent::core",
                conv_id = %row.conv_id,
                error = %e,
                "孤儿卡片关流失败（保留登记，下次启动再试）"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{CoreError, Result};
    use std::sync::Mutex as StdMutex;

    /// 轮询等待异步条件成立（真实时钟，~3s 预算）。patch 已异步化（patcher
    /// 任务），消费方方法返回时 patch 未必落盘，测试据此等 patcher 完成。
    async fn wait_until<F, Fut>(cond: F)
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for _ in 0..600 {
            if cond().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// 卡片全失败的平台 mock：send_card/update_card 恒 Err，send_text 记录。
    struct FailingCardPlatform {
        sent_text: StdMutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Platform for FailingCardPlatform {
        async fn recv(&self) -> Result<crate::types::InboundMessage> {
            Err(CoreError::Platform("mock-card", "无入站".into()))
        }
        async fn send_text(&self, _conv: &ConvId, text: &str, _hint: &ReplyHint) -> Result<()> {
            self.sent_text.lock().unwrap().push(text.to_string());
            Ok(())
        }
        async fn send_media(
            &self,
            _conv: &ConvId,
            _media: &crate::types::MediaRef,
            _hint: &ReplyHint,
        ) -> Result<()> {
            Ok(())
        }
        fn name(&self) -> &'static str {
            "mock-card"
        }
        fn supports_streaming_card(&self, _conv: &ConvId) -> bool {
            true
        }
        async fn send_card(
            &self,
            _conv: &ConvId,
            _card: &OutboundCard,
            _hint: &ReplyHint,
        ) -> Result<Option<String>> {
            Err(CoreError::Platform(
                "mock-card",
                "send_card 失败（模拟）".into(),
            ))
        }
        async fn update_card(
            &self,
            _conv: &ConvId,
            _message_id: &str,
            _card: &OutboundCard,
            _hint: &ReplyHint,
        ) -> Result<()> {
            Err(CoreError::Platform(
                "mock-card",
                "update_card 失败（模拟）".into(),
            ))
        }
    }

    /// 临时 store（孤儿卡片登记测试用）。
    async fn tmp_store(tag: &str) -> (Store, std::path::PathBuf) {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "imagent_card_session_test_{}_{tag}.db",
            std::process::id()
        ));
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", p.display()));
        }
        let store = Store::open(&p).await.expect("open store");
        (store, p)
    }

    /// P5-11：终态卡片更新失败 → 降级纯文本补发结论（卡片可停「生成中」，
    /// 结论不能丢）。
    #[tokio::test]
    async fn finalize_falls_back_to_text_when_card_fails() {
        let plat = Arc::new(FailingCardPlatform {
            sent_text: StdMutex::new(Vec::new()),
        });
        let (store, db) = tmp_store("fallback").await;
        let conv = ConvId("c1".into());
        let mut s = CardSession::new(
            store,
            conv.clone(),
            plat.clone(),
            ReplyHint::None,
            Default::default(),
        );
        // 流式阶段 send_card 即失败（msg_id 保持 None，仅 warn）。
        s.append_text("部分输出");
        s.finalize(Some("最终结论"), &[], CardTerminal::Done).await;
        let sent = plat.sent_text.lock().unwrap().clone();
        assert!(
            sent.iter().any(|t| t.contains("最终结论")),
            "卡片失败应降级纯文本补发: {sent:?}"
        );
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", db.display()));
        }
    }

    /// 卡片收发全记录的平台 mock：send_card 恒成功返回句柄；update_card 按开关
    /// 成功/失败，调用全记录（孤儿卡片关流测试用）。
    struct RecordingCardPlatform {
        name: &'static str,
        update_fails: bool,
        updates: StdMutex<Vec<(String, String)>>, // (handle, text)
        /// Wave B-3：每次 update 的 run_secs 快照（终态总耗时断言用）。
        run_secs_seen: StdMutex<Vec<(String, u64)>>, // (terminal 名, run_secs)
    }

    #[async_trait::async_trait]
    impl Platform for RecordingCardPlatform {
        async fn recv(&self) -> Result<crate::types::InboundMessage> {
            Err(CoreError::Platform(self.name, "无入站".into()))
        }
        async fn send_text(&self, _conv: &ConvId, _text: &str, _hint: &ReplyHint) -> Result<()> {
            Ok(())
        }
        async fn send_media(
            &self,
            _conv: &ConvId,
            _media: &crate::types::MediaRef,
            _hint: &ReplyHint,
        ) -> Result<()> {
            Ok(())
        }
        fn name(&self) -> &'static str {
            self.name
        }
        fn supports_streaming_card(&self, _conv: &ConvId) -> bool {
            true
        }
        async fn send_card(
            &self,
            _conv: &ConvId,
            _card: &OutboundCard,
            _hint: &ReplyHint,
        ) -> Result<Option<String>> {
            Ok(Some("card:abc123".into()))
        }
        async fn update_card(
            &self,
            _conv: &ConvId,
            handle: &str,
            card: &OutboundCard,
            _hint: &ReplyHint,
        ) -> Result<()> {
            if self.update_fails {
                return Err(CoreError::Platform(
                    self.name,
                    "update_card 失败（模拟）".into(),
                ));
            }
            let terminal = match card.terminal {
                CardTerminal::Running => "running",
                CardTerminal::Done => "done",
                CardTerminal::Error(_) => "error",
            };
            self.run_secs_seen
                .lock()
                .unwrap()
                .push((terminal.to_string(), card.run_secs));
            self.updates
                .lock()
                .unwrap()
                .push((handle.to_string(), card.text.clone()));
            Ok(())
        }
    }

    fn rm_db(p: &std::path::Path) {
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", p.display()));
        }
    }

    /// 第六批：首帧成功 → live_cards 登记；终态 patch 成功 → 摘除。
    #[tokio::test]
    async fn live_card_recorded_then_cleared_on_terminal_ok() {
        let (store, db) = tmp_store("lifecycle").await;
        let plat = Arc::new(RecordingCardPlatform {
            name: "mock-rec",
            update_fails: false,
            updates: StdMutex::new(Vec::new()),
            run_secs_seen: StdMutex::new(Vec::new()),
        });
        let conv = ConvId("c1".into());
        let mut s = CardSession::new(
            store.clone(),
            conv.clone(),
            plat.clone(),
            ReplyHint::None,
            Default::default(),
        );
        s.append_text("流式片段");
        // patcher 异步落卡：先等首帧句柄登记——终态 dispatch 必须发生在首帧
        // 之后（否则 send_card 直接建终态卡，登记/摘除断言会错位）。
        wait_until(|| async { store.list_live_cards().await.is_ok_and(|r| r.len() == 1) }).await;
        let rows = store.list_live_cards().await.expect("list");
        assert_eq!(rows.len(), 1, "首帧成功后应登记: {rows:?}");
        assert_eq!(rows[0].handle, "card:abc123");
        assert_eq!(rows[0].platform, "mock-rec");
        s.finalize(Some("完成"), &[], CardTerminal::Done).await;
        let rows = store.list_live_cards().await.expect("list");
        assert!(rows.is_empty(), "终态成功后应摘除: {rows:?}");
        rm_db(&db);
    }

    /// 第六批：终态 patch 失败（P5-11 降级纯文本）→ 登记保留，交启动扫描关流。
    #[tokio::test]
    async fn live_card_kept_when_terminal_patch_fails() {
        let (store, db) = tmp_store("keep-on-fail").await;
        let plat = Arc::new(RecordingCardPlatform {
            name: "mock-rec",
            update_fails: true,
            updates: StdMutex::new(Vec::new()),
            run_secs_seen: StdMutex::new(Vec::new()),
        });
        let conv = ConvId("c1".into());
        let mut s = CardSession::new(
            store.clone(),
            conv.clone(),
            plat.clone(),
            ReplyHint::None,
            Default::default(),
        );
        s.append_text("流式片段");
        // 先等首帧登记（update_fails 只影响 update_card；首帧 send_card 成功）。
        wait_until(|| async { store.list_live_cards().await.is_ok_and(|r| r.len() == 1) }).await;
        s.finalize(Some("结论"), &[], CardTerminal::Done).await;
        let rows = store.list_live_cards().await.expect("list");
        assert_eq!(rows.len(), 1, "终态失败应保留登记: {rows:?}");
        rm_db(&db);
    }

    /// 第六批：启动扫描——本平台孤儿卡片 patch 成 Error 终态并摘除；异平台登记
    /// 无处 patch，直接作废删除。
    #[tokio::test]
    async fn sweep_closes_orphans_and_drops_foreign_rows() {
        let (store, db) = tmp_store("sweep").await;
        store
            .record_live_card("c1", "mock-rec", "card:abc123")
            .await
            .expect("record");
        store
            .record_live_card("c2", "ilink", "msg:xyz")
            .await
            .expect("record");
        let plat = RecordingCardPlatform {
            name: "mock-rec",
            update_fails: false,
            updates: StdMutex::new(Vec::new()),
            run_secs_seen: StdMutex::new(Vec::new()),
        };
        sweep_live_cards(&store, &plat).await;
        let updates = plat.updates.lock().unwrap().clone();
        assert_eq!(
            updates,
            vec![(
                "card:abc123".to_string(),
                "⏸️ imagent 已重启，本次生成被中断（未产出结论）。请重新发送指令。".to_string()
            )],
            "只应关流本平台的孤儿卡片: {updates:?}"
        );
        let rows = store.list_live_cards().await.expect("list");
        assert!(rows.is_empty(), "两条登记都应清理: {rows:?}");
        rm_db(&db);
    }

    /// 句柄丢失自愈（安全批次）：update_card 回报「卡片不存在」（错误串含
    /// CARD_HANDLE_LOST 哨兵）→ 摘 live_cards + 句柄换新（Running 期立即重发新卡，
    /// send_card 再次登记新句柄）；终态不重发（错误如实返回走 P5-11 文本兜底）。
    /// 句柄丢失型平台 mock：首次 send_card 成功；update_card 一律回句柄丢失错误；
    /// resend（第二次 send_card）成功返回新句柄并记录。
    struct HandleLostPlatform {
        sends: StdMutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Platform for HandleLostPlatform {
        async fn recv(&self) -> Result<crate::types::InboundMessage> {
            Err(CoreError::Platform("mock-hl", "无入站".into()))
        }
        async fn send_text(&self, _conv: &ConvId, _text: &str, _hint: &ReplyHint) -> Result<()> {
            Ok(())
        }
        async fn send_media(
            &self,
            _conv: &ConvId,
            _media: &crate::types::MediaRef,
            _hint: &ReplyHint,
        ) -> Result<()> {
            Ok(())
        }
        fn name(&self) -> &'static str {
            "mock-hl"
        }
        fn supports_streaming_card(&self, _conv: &ConvId) -> bool {
            true
        }
        async fn send_card(
            &self,
            _conv: &ConvId,
            _card: &OutboundCard,
            _hint: &ReplyHint,
        ) -> Result<Option<String>> {
            let n = self.sends.lock().unwrap().len();
            let h = format!("card:new{n}");
            self.sends.lock().unwrap().push(h.clone());
            Ok(Some(h))
        }
        async fn update_card(
            &self,
            _conv: &ConvId,
            _handle: &str,
            _card: &OutboundCard,
            _hint: &ReplyHint,
        ) -> Result<()> {
            Err(CoreError::Platform(
                "mock-hl",
                format!(
                    "patch_card: code=230002 msg=card not exist（{}）",
                    crate::types::CARD_HANDLE_LOST
                ),
            ))
        }
    }

    #[tokio::test]
    async fn handle_lost_resends_new_card_when_running() {
        let (store, db) = tmp_store("handle-lost").await;
        let plat = Arc::new(HandleLostPlatform {
            sends: StdMutex::new(Vec::new()),
        });
        let conv = ConvId("c1".into());
        let mut s = CardSession::new(
            store.clone(),
            conv.clone(),
            plat.clone(),
            ReplyHint::None,
            Default::default(),
        );
        // 首帧：send_card 成功（句柄 card:new0），登记 live_cards。
        s.append_text("第一段");
        wait_until(|| async {
            store
                .list_live_cards()
                .await
                .is_ok_and(|r| r.iter().any(|x| x.handle == "card:new0"))
        })
        .await;
        assert_eq!(
            store.list_live_cards().await.unwrap()[0].handle,
            "card:new0"
        );
        // 第二帧：update_card 回句柄丢失 → 摘登记 + 重发新卡（句柄换新再登记）。
        // 节流窗口内的帧由 patcher 睡到边界再 patch（~500ms），轮询等自愈完成。
        s.append_text("第二段");
        wait_until(|| async {
            let sends_ok = plat.sends.lock().unwrap().len() >= 2;
            let reg_ok = store
                .list_live_cards()
                .await
                .is_ok_and(|r| r.len() == 1 && r.iter().any(|x| x.handle == "card:new1"));
            sends_ok && reg_ok
        })
        .await;
        let sends = plat.sends.lock().unwrap().clone();
        assert_eq!(
            sends,
            vec!["card:new0", "card:new1"],
            "应重发新卡: {sends:?}"
        );
        let rows = store.list_live_cards().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].handle, "card:new1", "登记应更新为新句柄");
        // 终态遇句柄丢失：不重发（send 次数不变），错误走 P5-11 文本兜底。
        s.finalize(Some("结论"), &[], CardTerminal::Done).await;
        let sends = plat.sends.lock().unwrap().clone();
        assert_eq!(sends.len(), 2, "终态不重发新卡: {sends:?}");
        rm_db(&db);
    }

    /// 启动扫描遇句柄丢失：登记作废删除（不再无限重试注定失败的 patch）。
    #[tokio::test]
    async fn sweep_drops_gone_card_registration() {
        let (store, db) = tmp_store("sweep-gone").await;
        store
            .record_live_card("c1", "mock-hl", "card:gone")
            .await
            .unwrap();
        let plat = HandleLostPlatform {
            sends: StdMutex::new(Vec::new()),
        };
        sweep_live_cards(&store, &plat).await;
        assert!(
            store.list_live_cards().await.unwrap().is_empty(),
            "句柄丢失登记应作废删除"
        );
        rm_db(&db);
    }

    /// Wave B-3：终态卡 run_secs 写全量（非 10s 量化、非清零）——把 started 回拨
    /// 75 秒后 finalize，断言终态帧携带 75（Running 帧仍走量化路径）。
    #[tokio::test]
    async fn terminal_card_carries_full_run_secs() {
        let (store, db) = tmp_store("terminal-secs").await;
        let plat = Arc::new(RecordingCardPlatform {
            name: "mock-rec",
            update_fails: false,
            updates: StdMutex::new(Vec::new()),
            run_secs_seen: StdMutex::new(Vec::new()),
        });
        let conv = ConvId("c1".into());
        let mut s = CardSession::new(
            store.clone(),
            conv.clone(),
            plat.clone(),
            ReplyHint::None,
            Default::default(),
        );
        // 首帧（Running）：立即发卡拿句柄——等登记落库确保首帧已发（终态须走
        // update_card 分支才会被 run_secs_seen 记录）。
        s.append_text("流式片段");
        wait_until(|| async { store.list_live_cards().await.is_ok_and(|r| !r.is_empty()) }).await;
        // 回拨 75 秒（同文件测试可访问内部状态；无需真实等待）。
        s.inner.lock().unwrap().started = Instant::now() - Duration::from_secs(75);
        s.finalize(Some("完成"), &[], CardTerminal::Done).await;
        let seen = plat.run_secs_seen.lock().unwrap().clone();
        assert!(
            seen.contains(&("done".to_string(), 75)),
            "终态帧应携带全量 75s: {seen:?}"
        );
        assert!(
            !seen.contains(&("done".to_string(), 70)),
            "终态不应是 10s 量化值: {seen:?}"
        );
        assert!(
            !seen.contains(&("done".to_string(), 0)),
            "终态不应清零: {seen:?}"
        );
        rm_db(&db);
    }

    /// 尾帧 flush：节流窗口内的 pending 状态由 patcher **睡到窗口边界补发**——
    /// 最后一个事件（如末个 ToolResult 的 ✅）不再被静默丢弃（旧实现的语义
    /// 在 patcher 化后由「gen ≠ flushed_gen 必 flush」天然保证）。真实时钟跑
    /// （窗口 500ms，测试耗时约半秒）。
    #[tokio::test]
    async fn throttled_patch_flushes_tail_frame() {
        let (store, db) = tmp_store("tail-flush").await;
        let plat = Arc::new(RecordingCardPlatform {
            name: "mock-rec",
            update_fails: false,
            updates: StdMutex::new(Vec::new()),
            run_secs_seen: StdMutex::new(Vec::new()),
        });
        let conv = ConvId("c1".into());
        let s = CardSession::new(
            store.clone(),
            conv.clone(),
            plat.clone(),
            ReplyHint::None,
            Default::default(),
        );
        // 首帧：无 msg_id，立即发卡（send_card 成功拿句柄）。等首帧真正落发
        //（live_cards 登记）再喂第二帧——patcher 化后首帧 dispatch 异步，若
        // 两条 append 都赶在首帧前落累积，会被直接合并进首卡（语义等价且更省
        // 一次 API）；本测试针对的是「已发卡后窗口内的尾帧」路径（update_card）。
        s.append_text("第一段");
        wait_until(|| async { store.list_live_cards().await.is_ok_and(|r| !r.is_empty()) }).await;
        // 紧接第二帧：节流窗口内——patcher 必须睡到到期补发（update_card），
        // 而不是丢弃。
        s.append_text("第二段");
        wait_until(|| async {
            plat.updates
                .lock()
                .unwrap()
                .iter()
                .any(|(_, t)| t == "第一段第二段")
        })
        .await;
        let updates = plat.updates.lock().unwrap().clone();
        assert_eq!(
            updates,
            vec![("card:abc123".to_string(), "第一段第二段".to_string())],
            "窗口内的尾帧应 flush 上卡（累积文本）: {updates:?}"
        );
        rm_db(&db);
    }

    /// v1.23 心跳：静默 ≥25s 才 patch（节流语义不破坏）；waiting=true 翻
    /// WaitingApproval 阶段（footer 文案由平台渲染）。
    #[tokio::test]
    async fn heartbeat_patches_when_stale_and_flips_waiting_phase() {
        let platform = Arc::new(RecordingCardPlatform {
            name: "rec",
            update_fails: false,
            updates: StdMutex::new(Vec::new()),
            run_secs_seen: StdMutex::new(Vec::new()),
        });
        let (store, _db) = tmp_store("hb").await;
        let conv = ConvId("c1".into());
        let cs = CardSession::new(
            store,
            conv.clone(),
            platform.clone(),
            ReplyHint::None,
            Default::default(),
        );
        cs.append_text("hi");
        let updates_after_append = platform.updates.lock().unwrap().len();
        // 刚 patch 过：心跳应跳过（< 25s）。
        cs.heartbeat(false);
        assert_eq!(
            platform.updates.lock().unwrap().len(),
            updates_after_append,
            "25s 内心跳不应 patch"
        );
        // 模拟静默超窗：回拨 last_patch。
        cs.inner.lock().unwrap().last_patch =
            std::time::Instant::now() - std::time::Duration::from_secs(30);
        cs.heartbeat(false);
        wait_until(|| async { platform.updates.lock().unwrap().len() > updates_after_append })
            .await;
        assert!(
            cs.inner.lock().unwrap().phase == CardPhase::Outputting,
            "非 waiting 不改阶段"
        );
        // waiting=true：阶段翻 WaitingApproval。
        cs.inner.lock().unwrap().last_patch =
            std::time::Instant::now() - std::time::Duration::from_secs(30);
        cs.heartbeat(true);
        wait_until(|| async { cs.inner.lock().unwrap().phase == CardPhase::WaitingApproval }).await;
    }

    /// P3-d（code-review v14）：首建失败的平台 mock——send_card 第一次失败
    /// （模拟瞬时限流/网络抖动），之后成功；尝试次数全记录。
    struct CreateFailOncePlatform {
        sends: StdMutex<usize>,
        fail_first: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl Platform for CreateFailOncePlatform {
        async fn recv(&self) -> Result<crate::types::InboundMessage> {
            Err(CoreError::Platform("mock-cf", "无入站".into()))
        }
        async fn send_text(&self, _conv: &ConvId, _text: &str, _hint: &ReplyHint) -> Result<()> {
            Ok(())
        }
        async fn send_media(
            &self,
            _conv: &ConvId,
            _media: &crate::types::MediaRef,
            _hint: &ReplyHint,
        ) -> Result<()> {
            Ok(())
        }
        fn name(&self) -> &'static str {
            "mock-cf"
        }
        fn supports_streaming_card(&self, _conv: &ConvId) -> bool {
            true
        }
        async fn send_card(
            &self,
            _conv: &ConvId,
            _card: &OutboundCard,
            _hint: &ReplyHint,
        ) -> Result<Option<String>> {
            *self.sends.lock().unwrap() += 1;
            if self
                .fail_first
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                Err(CoreError::Platform(
                    "mock-cf",
                    "send_card 失败（模拟瞬时抖动）".into(),
                ))
            } else {
                Ok(Some("card:re".into()))
            }
        }
        async fn update_card(
            &self,
            _conv: &ConvId,
            _handle: &str,
            _card: &OutboundCard,
            _hint: &ReplyHint,
        ) -> Result<()> {
            Ok(())
        }
    }

    /// P3-d（code-review v14）：建卡瞬时失败 + 首 token 长静默——首次 send_card
    /// 失败后 patcher 回到 Idle（gen 已追平），无新 chunk 就永不重试；heartbeat
    /// 不再因「无句柄」短路，bump gen 驱动 patcher 重建成功（用户在静默期仍能
    /// 看到「任务已接收」卡）。重试节奏由 CREATE_FAIL_BACKOFF 约束（此处回拨
    /// 退避时钟跳过 2s 等待，退避语义由 patch_delay 覆盖，不重复验证）。
    #[tokio::test]
    async fn heartbeat_recreates_card_after_transient_create_failure() {
        let (store, db) = tmp_store("hb-recreate").await;
        let plat = Arc::new(CreateFailOncePlatform {
            sends: StdMutex::new(0),
            fail_first: std::sync::atomic::AtomicBool::new(true),
        });
        let conv = ConvId("c1".into());
        let mut s = CardSession::new(
            store.clone(),
            conv.clone(),
            plat.clone(),
            ReplyHint::None,
            Default::default(),
        );
        // 首建失败（句柄仍无，patcher 记退避后回 Idle）。
        s.ensure_started();
        wait_until(|| async { *plat.sends.lock().unwrap() == 1 }).await;
        assert!(
            s.inner.lock().unwrap().msg_id.is_none(),
            "首次建卡应失败（模拟瞬时抖动）"
        );
        // 长静默期心跳驱动重建。
        s.inner.lock().unwrap().last_create_fail =
            Some(std::time::Instant::now() - CREATE_FAIL_BACKOFF);
        s.heartbeat(false);
        wait_until(|| async { store.list_live_cards().await.is_ok_and(|r| !r.is_empty()) }).await;
        let sends = *plat.sends.lock().unwrap();
        assert_eq!(
            sends, 2,
            "心跳应驱动第二次建卡（失败一次后重试成功）: {sends}"
        );
        assert!(
            s.inner.lock().unwrap().msg_id.is_some(),
            "重试成功后应持有句柄"
        );
        s.finalize(Some("结论"), &[], CardTerminal::Done).await;
        rm_db(&db);
    }

    /// P1-1 吞吐锚：delta 级 chunk 流（50 条）的消费路径零阻塞——`append_text`
    /// 同步返回（不 sleep、不等平台），50 条总耗时应远低于节流周期 × N
    ///（旧实现每条先睡 ~500ms，50 条 ≈ 25s）。数据不因解耦丢失：finalize
    /// 同步落定后，终态卡携带全部累积文本。
    #[tokio::test]
    async fn delta_stream_consumption_not_throttled() {
        let (store, db) = tmp_store("throughput").await;
        let plat = Arc::new(RecordingCardPlatform {
            name: "mock-rec",
            update_fails: false,
            updates: StdMutex::new(Vec::new()),
            run_secs_seen: StdMutex::new(Vec::new()),
        });
        let conv = ConvId("c1".into());
        let mut s = CardSession::new(
            store.clone(),
            conv.clone(),
            plat.clone(),
            ReplyHint::None,
            Default::default(),
        );
        s.append_text("首段。");
        // 等首帧句柄登记：确保终态走 update_card（进 updates 记录）。
        wait_until(|| async { store.list_live_cards().await.is_ok_and(|r| !r.is_empty()) }).await;
        let t0 = Instant::now();
        for i in 0..50 {
            s.append_text(&format!("段{i}，"));
        }
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "50 条 delta 的消费耗时应 < 2s（节流不得阻塞消费路径）: {elapsed:?}"
        );
        s.finalize(None, &[], CardTerminal::Done).await;
        let updates = plat.updates.lock().unwrap().clone();
        assert!(
            updates
                .iter()
                .any(|(_, t)| t.contains("段0，") && t.contains("段49，")),
            "全文应随终态上卡: {updates:?}"
        );
        rm_db(&db);
    }

    /// P1-1：平台 IO 慢（update_card 200ms）也不阻塞消费路径——patcher 与消费
    /// 方彻底解耦的直接证明（旧实现 append 内联 await 平台调用 + 节流睡眠）。
    struct SlowCardPlatform {
        delay: Duration,
        updates: StdMutex<usize>,
    }

    #[async_trait::async_trait]
    impl Platform for SlowCardPlatform {
        async fn recv(&self) -> Result<crate::types::InboundMessage> {
            Err(CoreError::Platform("mock-slow-card", "无入站".into()))
        }
        async fn send_text(&self, _conv: &ConvId, _text: &str, _hint: &ReplyHint) -> Result<()> {
            Ok(())
        }
        async fn send_media(
            &self,
            _conv: &ConvId,
            _media: &crate::types::MediaRef,
            _hint: &ReplyHint,
        ) -> Result<()> {
            Ok(())
        }
        fn name(&self) -> &'static str {
            "mock-slow-card"
        }
        fn supports_streaming_card(&self, _conv: &ConvId) -> bool {
            true
        }
        async fn send_card(
            &self,
            _conv: &ConvId,
            _card: &OutboundCard,
            _hint: &ReplyHint,
        ) -> Result<Option<String>> {
            Ok(Some("card:slow".into()))
        }
        async fn update_card(
            &self,
            _conv: &ConvId,
            _message_id: &str,
            _card: &OutboundCard,
            _hint: &ReplyHint,
        ) -> Result<()> {
            tokio::time::sleep(self.delay).await;
            *self.updates.lock().unwrap() += 1;
            Ok(())
        }
    }

    #[tokio::test]
    async fn consumer_not_blocked_by_slow_platform() {
        let (store, db) = tmp_store("slow-card").await;
        let plat = Arc::new(SlowCardPlatform {
            delay: Duration::from_millis(200),
            updates: StdMutex::new(0),
        });
        let conv = ConvId("c1".into());
        let mut s = CardSession::new(
            store.clone(),
            conv.clone(),
            plat.clone(),
            ReplyHint::None,
            Default::default(),
        );
        s.append_text("首段。");
        wait_until(|| async { store.list_live_cards().await.is_ok_and(|r| !r.is_empty()) }).await;
        let t0 = Instant::now();
        for i in 0..10 {
            s.append_text(&format!("s{i}"));
        }
        let elapsed = t0.elapsed();
        // 上界取「串行被平台 IO 阻塞（10 × 200ms = 2s）」与「完全无阻塞」之间
        // 的 1.5s：证明消费路径不被平台延迟钉死即可——CI 慢机的调度抖动不该
        // 让断言 flake（此前 500ms 在 2 核 runner 上偶发超限）。
        assert!(
            elapsed < Duration::from_millis(1500),
            "消费方不得被平台 IO 阻塞（10 次 append 应远快于串行 2s）: {elapsed:?}"
        );
        s.finalize(None, &[], CardTerminal::Done).await;
        assert!(
            *plat.updates.lock().unwrap() >= 1,
            "终态 patch 应已实际发生"
        );
        rm_db(&db);
    }
}
