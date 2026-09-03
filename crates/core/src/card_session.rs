//! 流式卡片会话：累积 agent 输出，节流 patch 到支持卡片的平台。
//!
//! 仅 `Platform::supports_streaming_card() == true` 的平台使用（dispatch 据此分支）。
//! 累积 `text` / `tool_calls`，按节流间隔 patch：首次 `send_card` 拿 `message_id`，
//! 后续 `update_card`。最终 `finalize` 强制 patch 终态（Done/Error）。
//!
//! 方法均返回 `()`——卡片发送失败在内部 `warn!` 记录（dispatch `handle` 返回 ()，
//! 无法传播卡片错误；卡片失败不应中断 agent 回复）。设计借鉴 lcab 的
//! `RunState + renderCard + update`，但 core 只产平台无关的 [`OutboundCard`]，
//! 卡片 JSON 渲染由各 Platform 实现。

use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::platform::Platform;
use crate::types::{CardPhase, CardTerminal, ConvId, OutboundCard, ReplyHint, TodoItem, ToolCall};
use imagent_store::Store;

/// 卡片 patch 节流间隔。飞书交互卡片更新有频率限制，500ms 平衡流畅与限流。
const CARD_THROTTLE: Duration = Duration::from_millis(500);

/// W2-1：思考片段累积上限（条数）——只保留最近的思考（旧思考对用户无回看价值，
/// 无上限会把卡片 payload 撑爆）。
const MAX_THOUGHTS: usize = 10;

/// W2-1：单条思考片段的字符截断上限（防超长推理占满卡片）。
const THOUGHT_TRUNC_CHARS: usize = 400;

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
/// 卡片成功终态下需要补发全文文本的字节阈值（真机校准 2026-08-30）：略高于
/// 平台侧 4KB+4KB 头尾窗预算——只有被截断的正文才补发，避免短内容卡+文本
/// 双发噪音。
const CARD_TEXT_FULL_THRESHOLD: usize = 8_500;

pub(crate) struct CardSession {
    text: String,
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
    /// 轮次起点（Running footer 运行时长的基准，见 [`OutboundCard::run_secs`]）。
    started: Instant,
    /// 在飞卡片登记（P4_ROADMAP 第六批）：首帧句柄落库、终态成功摘除——进程崩溃
    /// 后由 [`sweep_live_cards`] 启动扫描把滞留「生成中」的卡片 patch 成已中断。
    store: Store,
    conv: ConvId,
    platform_name: &'static str,
    /// P10：dispatcher 的排队状态句柄（每次 patch 拉取，见 [`queued_hint_display`]）。
    queued_hints: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, QueuedHint>>>,
    /// 本轮成本摘要（`UsageStats.display()`）——run 结束时由 round 写入，终态
    /// footer 追加展示（`✅ 已完成 · $0.012`）；None = backend 未产出 usage。
    pub(crate) usage_display: Option<String>,
}

impl CardSession {
    pub(crate) fn new(
        store: Store,
        conv: ConvId,
        platform_name: &'static str,
        queued_hints: std::sync::Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, QueuedHint>>,
        >,
    ) -> Self {
        Self {
            text: String::new(),
            tools: Vec::new(),
            thoughts: Vec::new(),
            todos: Vec::new(),
            phase: CardPhase::Thinking,
            msg_id: None,
            last_patch: Instant::now(),
            started: Instant::now(),
            store,
            conv,
            platform_name,
            queued_hints,
            usage_display: None,
        }
    }

    /// 立即发出初始卡片（若尚未发）。真机校准 UX：agent 首 chunk 前有数秒到
    /// 十几秒静默期（CLI 冷启动 + 模型首 token），轮次开始即发「执行中」卡，
    /// 用户才确知消息已被接收处理。飞书 send_card 用固定初始模板（打字机基座），
    /// 与本方法的无内容 dispatch 天然契合。
    pub(crate) async fn ensure_started(
        &mut self,
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) {
        if self.msg_id.is_none() {
            self.dispatch_card(CardTerminal::Running, conv, hint, platform)
                .await;
        }
    }

    /// 累积文本增量，节流 patch（Running 态）；阶段翻到「输出中」。
    pub(crate) async fn append_text(
        &mut self,
        text: &str,
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) {
        self.text.push_str(text);
        self.phase = CardPhase::Outputting;
        self.patch_if_due(CardTerminal::Running, conv, hint, platform)
            .await;
    }

    /// 累积工具调用（⏳ 执行中），节流 patch；阶段翻到「调用工具」。
    /// W2-3：`id` 供结果精确配对（None = 后端未提供）。
    pub(crate) async fn append_tool(
        &mut self,
        tool: &str,
        input_summary: &str,
        id: Option<&str>,
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) {
        self.tools.push(ToolCall {
            name: tool.to_string(),
            summary: input_summary.to_string(),
            done: false,
            id: id.map(str::to_string),
        });
        self.phase = CardPhase::ToolRunning;
        self.patch_if_due(CardTerminal::Running, conv, hint, platform)
            .await;
    }

    /// P8-1：工具结果到达——翻 ✅（W2-3：优先按 id 精确配对，无 id 回退同名
    /// 最早未完成；同名并发极少见，错配只影响图标不影响内容）。
    pub(crate) async fn finish_tool(
        &mut self,
        tool: &str,
        id: Option<&str>,
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) {
        // W2-3：优先按 id 精确配对（首个借用先落地结束，再做名字兜底——避免
        // 链式 or_else 的双重可变借用）。
        let by_id = match id {
            Some(i) => self
                .tools
                .iter_mut()
                .find(|t| !t.done && t.id.as_deref() == Some(i)),
            None => None,
        };
        let target = match by_id {
            Some(t) => Some(t),
            None => self.tools.iter_mut().find(|t| !t.done && t.name == tool),
        };
        if let Some(t) = target {
            t.done = true;
        }
        self.patch_if_due(CardTerminal::Running, conv, hint, platform)
            .await;
    }

    /// W2-1：累积思考片段（最近 N 条、单条截断），节流 patch；阶段保持 Thinking。
    pub(crate) async fn append_thought(
        &mut self,
        thought: &str,
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) {
        let t: String = thought.chars().take(THOUGHT_TRUNC_CHARS).collect();
        if t.trim().is_empty() {
            return;
        }
        self.thoughts.push(t);
        if self.thoughts.len() > MAX_THOUGHTS {
            self.thoughts.remove(0);
        }
        self.patch_if_due(CardTerminal::Running, conv, hint, platform)
            .await;
    }

    /// W2-2：任务清单（全量替换），节流 patch。
    pub(crate) async fn set_todos(
        &mut self,
        items: &[TodoItem],
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) {
        self.todos = items.to_vec();
        self.patch_if_due(CardTerminal::Running, conv, hint, platform)
            .await;
    }

    /// 最终 patch：用 `final_text` 覆盖累积文本，合并 dispatch 侧累积的 `extra_tools`，
    /// 强制 patch 终态（不受节流，确保 Done/Error 显示）。
    ///
    /// P5-11：终态 patch 失败（网络抖动 / 限流 / 卡片服务异常）时降级纯文本补发——
    /// 流式卡片可以停在「生成中」，但结论不能丢（用户至少拿到完整文本）。
    pub(crate) async fn finalize(
        &mut self,
        final_text: Option<&str>,
        extra_tools: &[ToolCall],
        terminal: CardTerminal,
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) {
        if let Some(f) = final_text {
            self.text.clear();
            self.text.push_str(f);
        }
        for t in extra_tools {
            // 按 (name, summary) 去重合并（done 标志可能不同步——以已存在的记录为准）。
            if !self
                .tools
                .iter()
                .any(|e| e.name == t.name && e.summary == t.summary)
            {
                self.tools.push(t.clone());
            }
        }
        // 终态强制 patch（绕过节流），确保用户看到 Done/Error；失败降级纯文本。
        let card_ok = self.dispatch_card(terminal, conv, hint, platform).await;
        if !card_ok && !self.text.is_empty() {
            match platform.send_text(conv, &self.text, hint).await {
                Ok(()) => warn!(target: "imagent::core", "卡片终态更新失败，已降级纯文本补发结论"),
                Err(e) => warn!(
                    target: "imagent::core",
                    error = %e,
                    "卡片终态更新失败，纯文本补发也失败（结论丢失）"
                ),
            }
        } else if card_ok && self.text.len() > CARD_TEXT_FULL_THRESHOLD {
            // 真机校准（2026-08-30）：卡片正文受字节上限截断（飞书侧 4KB+4KB
            // 头尾窗）但 patch **成功**时，此前不补发全文——卡片标注「完整内容
            // 见文本消息」却没有那条文本。超阈值的成功终态主动补发全文文本。
            if let Err(e) = platform.send_text(conv, &self.text, hint).await {
                warn!(target: "imagent::core", error = %e, "截断卡全文补发失败（卡片内仍有头尾窗口）");
            }
        }
    }

    /// 节流 patch：首次（无 msg_id）或距上次 patch ≥ THROTTLE 才发；未到期则
    /// **睡到到期再发**（尾帧 flush 保证）。
    ///
    /// 此前未到期直接跳过——若跳过后再无后续事件（如最后一个 ToolResult 后
    /// 模型直接收尾），卡片会永远停在过期状态（⏳ 不翻 ✅，直到 finalize 的
    /// 终态 patch）。改成睡到节流窗口到期再 patch：调用方（dispatch 的 chunk
    /// 消费循环）最多被阻塞到窗口边界（≤500ms），换来「每个事件最终都会上卡」；
    /// 连续高频 chunk 自然被合并成 2 次/秒的节奏，与节流初衷一致。
    async fn patch_if_due(
        &mut self,
        terminal: CardTerminal,
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) {
        if self.msg_id.is_some() {
            let since = self.last_patch.elapsed();
            if since < CARD_THROTTLE {
                tokio::time::sleep(CARD_THROTTLE - since).await;
            }
        }
        self.dispatch_card(terminal, conv, hint, platform).await;
    }

    /// 实际发送/更新卡片：首次 `send_card` 拿 msg_id，后续 `update_card`；失败
    /// `warn!` 并返回 false（调用方决定是否降级）。
    async fn dispatch_card(
        &mut self,
        terminal: CardTerminal,
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) -> bool {
        let queued = self
            .queued_hints
            .lock()
            .await
            .get(&self.conv.0)
            .and_then(queued_hint_display);
        let card = OutboundCard {
            text: self.text.clone(),
            tool_calls: self.tools.clone(),
            thoughts: self.thoughts.clone(),
            todos: self.todos.clone(),
            phase: self.phase,
            queued_hint: queued,
            // 运行时长：Running 态 10s 量化（footer 去重缓存按内容比对，量化后
            // 10s 内 footer 不变、不触发 patch，防高频更新）；**终态写全量秒数**
            // （Wave B-3：`✅ 已完成 · 30m · $0.012` 的总耗时来源——量化/清零都会
            // 让用户看到的完成时长失真）。终态 footer 不走去重缓存（每次终态只
            // patch 一次），全量值无刷屏风险。
            run_secs: if matches!(terminal, CardTerminal::Running) {
                (self.started.elapsed().as_secs() / 10) * 10
            } else {
                self.started.elapsed().as_secs()
            },
            usage_display: self.usage_display.clone(),
            terminal: terminal.clone(),
        };
        let res: crate::error::Result<()> = match &self.msg_id {
            None => match platform.send_card(conv, &card, hint).await {
                Ok(id) => {
                    self.msg_id = id;
                    // 拿到真实卡片句柄（None = 平台降级纯文本，无卡片可滞留）即登记；
                    // 失败仅 warn——卡片路径不能反向打断 agent 回复。
                    if let Some(h) = &self.msg_id {
                        if let Err(e) = self
                            .store
                            .record_live_card(&self.conv.0, self.platform_name, h)
                            .await
                        {
                            warn!(
                                target: "imagent::core",
                                error = %e,
                                "live_cards 登记失败（进程若崩溃，该卡片将滞留「生成中」）"
                            );
                        }
                    }
                    Ok(())
                }
                Err(e) => Err(e),
            },
            Some(mid) => match platform.update_card(conv, mid, &card, hint).await {
                // 句柄丢失自愈：平台回报「卡片不存在/已删除」（错误串含
                // CARD_HANDLE_LOST 哨兵，见 types.rs）——原卡片被用户删除/撤回后
                // patch 永远失败。摘除 live_cards 登记（终止启动扫描的无限重试）、
                // 句柄置空，Running 期立即重发一张新卡（句柄换新，继续流式）；
                // 终态不重发（轮次已结束，结论由 P5-11 纯文本兜底）。
                Err(e) if e.to_string().contains(crate::types::CARD_HANDLE_LOST) => {
                    warn!(
                        target: "imagent::core",
                        conv_id = %self.conv.0,
                        "流式卡片已被删除/撤回（句柄丢失），重发新卡"
                    );
                    if let Err(clear_err) = self.store.clear_live_card(&self.conv.0).await {
                        warn!(
                            target: "imagent::core",
                            error = %clear_err,
                            "live_cards 摘除失败（句柄丢失自愈路径）"
                        );
                    }
                    self.msg_id = None;
                    if !matches!(terminal, CardTerminal::Running) {
                        Err(e)
                    } else {
                        // 重发新卡（send_card 分支逻辑的等价重放——async fn 不能递归，
                        // 此处内联）：失败如实返回由调用方兜底（warn + 下帧再试）。
                        match platform.send_card(conv, &card, hint).await {
                            Ok(id) => {
                                self.msg_id = id;
                                if let Some(h) = &self.msg_id {
                                    if let Err(rec_err) = self
                                        .store
                                        .record_live_card(&self.conv.0, self.platform_name, h)
                                        .await
                                    {
                                        warn!(
                                            target: "imagent::core",
                                            error = %rec_err,
                                            "live_cards 重新登记失败（句柄丢失自愈路径）"
                                        );
                                    }
                                }
                                Ok(())
                            }
                            Err(send_err) => Err(send_err),
                        }
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
        // D9：仅成功才推进节流时钟——失败也推进会让紧随其后的重试被节流跳过，
        // 瞬时抖动演变成「整个流式期间不再更新卡片」。
        if ok {
            self.last_patch = Instant::now();
        }
        // 终态 patch 成功即摘除登记（卡片已闭环，无需启动扫描兜底）。失败保留——
        // 结论已降级纯文本补发（P5-11），卡片本身留给下次启动扫描关流。
        if ok && !matches!(terminal, CardTerminal::Running) {
            if let Err(e) = self.store.clear_live_card(&self.conv.0).await {
                warn!(
                    target: "imagent::core",
                    error = %e,
                    "live_cards 摘除失败（下次启动会误把已完成的卡片再 patch 一次，无害）"
                );
            }
        }
        ok
    }
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
        let plat = FailingCardPlatform {
            sent_text: StdMutex::new(Vec::new()),
        };
        let (store, db) = tmp_store("fallback").await;
        let conv = ConvId("c1".into());
        let hint = ReplyHint::None;
        let mut s = CardSession::new(store, conv.clone(), plat.name(), Default::default());
        // 流式阶段 send_card 即失败（msg_id 保持 None，仅 warn）。
        s.append_text("部分输出", &conv, &hint, &plat).await;
        s.finalize(
            Some("最终结论"),
            &[],
            CardTerminal::Done,
            &conv,
            &hint,
            &plat,
        )
        .await;
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
        let plat = RecordingCardPlatform {
            name: "mock-rec",
            update_fails: false,
            updates: StdMutex::new(Vec::new()),
            run_secs_seen: StdMutex::new(Vec::new()),
        };
        let conv = ConvId("c1".into());
        let hint = ReplyHint::None;
        let mut s = CardSession::new(store.clone(), conv.clone(), plat.name(), Default::default());
        s.append_text("流式片段", &conv, &hint, &plat).await;
        let rows = store.list_live_cards().await.expect("list");
        assert_eq!(rows.len(), 1, "首帧成功后应登记: {rows:?}");
        assert_eq!(rows[0].handle, "card:abc123");
        assert_eq!(rows[0].platform, "mock-rec");
        s.finalize(Some("完成"), &[], CardTerminal::Done, &conv, &hint, &plat)
            .await;
        let rows = store.list_live_cards().await.expect("list");
        assert!(rows.is_empty(), "终态成功后应摘除: {rows:?}");
        rm_db(&db);
    }

    /// 第六批：终态 patch 失败（P5-11 降级纯文本）→ 登记保留，交启动扫描关流。
    #[tokio::test]
    async fn live_card_kept_when_terminal_patch_fails() {
        let (store, db) = tmp_store("keep-on-fail").await;
        let plat = RecordingCardPlatform {
            name: "mock-rec",
            update_fails: true,
            updates: StdMutex::new(Vec::new()),
            run_secs_seen: StdMutex::new(Vec::new()),
        };
        let conv = ConvId("c1".into());
        let hint = ReplyHint::None;
        let mut s = CardSession::new(store.clone(), conv.clone(), plat.name(), Default::default());
        s.append_text("流式片段", &conv, &hint, &plat).await;
        s.finalize(Some("结论"), &[], CardTerminal::Done, &conv, &hint, &plat)
            .await;
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
        let plat = HandleLostPlatform {
            sends: StdMutex::new(Vec::new()),
        };
        let conv = ConvId("c1".into());
        let hint = ReplyHint::None;
        let mut s = CardSession::new(store.clone(), conv.clone(), plat.name(), Default::default());
        // 首帧：send_card 成功（句柄 card:new0），登记 live_cards。
        s.append_text("第一段", &conv, &hint, &plat).await;
        assert_eq!(
            store.list_live_cards().await.unwrap()[0].handle,
            "card:new0"
        );
        // 第二帧：update_card 回句柄丢失 → 摘登记 + 重发新卡（句柄换新再登记）。
        s.append_text("第二段", &conv, &hint, &plat).await;
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
        s.finalize(Some("结论"), &[], CardTerminal::Done, &conv, &hint, &plat)
            .await;
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
        let plat = RecordingCardPlatform {
            name: "mock-rec",
            update_fails: false,
            updates: StdMutex::new(Vec::new()),
            run_secs_seen: StdMutex::new(Vec::new()),
        };
        let conv = ConvId("c1".into());
        let hint = ReplyHint::None;
        let mut s = CardSession::new(store.clone(), conv.clone(), plat.name(), Default::default());
        // 首帧（Running）：立即发卡拿句柄。
        s.append_text("流式片段", &conv, &hint, &plat).await;
        // 回拨 75 秒（同文件测试可访问私有字段；无需真实等待）。
        s.started = Instant::now() - Duration::from_secs(75);
        s.finalize(Some("完成"), &[], CardTerminal::Done, &conv, &hint, &plat)
            .await;
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

    /// 尾帧 flush：节流窗口内被跳过的 patch 会**睡到窗口到期补发**——最后一个
    /// 事件（如末个 ToolResult 的 ✅）不再被静默丢弃。真实时钟跑（窗口 500ms，
    /// 测试耗时约半秒）。
    #[tokio::test]
    async fn throttled_patch_flushes_tail_frame() {
        let (store, db) = tmp_store("tail-flush").await;
        let plat = RecordingCardPlatform {
            name: "mock-rec",
            update_fails: false,
            updates: StdMutex::new(Vec::new()),
            run_secs_seen: StdMutex::new(Vec::new()),
        };
        let conv = ConvId("c1".into());
        let hint = ReplyHint::None;
        let mut s = CardSession::new(store, conv.clone(), plat.name(), Default::default());
        // 首帧：无 msg_id，立即发卡（send_card 成功拿句柄）。
        s.append_text("第一段", &conv, &hint, &plat).await;
        // 紧接第二帧：节流窗口内——必须睡到到期补发，而不是丢弃。
        s.append_text("第二段", &conv, &hint, &plat).await;
        let updates = plat.updates.lock().unwrap().clone();
        assert_eq!(
            updates,
            vec![("card:abc123".to_string(), "第一段第二段".to_string())],
            "窗口内的尾帧应 flush 上卡（累积文本）: {updates:?}"
        );
        rm_db(&db);
    }
}
