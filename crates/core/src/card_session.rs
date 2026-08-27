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
use crate::types::{CardPhase, CardTerminal, ConvId, OutboundCard, ReplyHint, ToolCall};
use imagent_store::Store;

/// 卡片 patch 节流间隔。飞书交互卡片更新有频率限制，500ms 平衡流畅与限流。
const CARD_THROTTLE: Duration = Duration::from_millis(500);

/// P10：本会话的排队状态（运行中入队的消息摘要）。入队路径写、取批/中断清、
/// CardSession 每次 patch 拉取（活动期随 chunk 刷新 footer 的排队提示）。
#[derive(Debug, Clone, Default)]
pub(crate) struct QueuedHint {
    /// 排队消息条数。
    pub count: usize,
    /// 最新一条的摘要（≤40 字符；纯媒体消息给「（图片/文件）」占位）。
    pub latest: String,
}

/// 排队状态 → 展示文案（None = 无需展示）。`📥 排队 N 条，最新：「…」`。
pub(crate) fn queued_hint_display(h: &QueuedHint) -> Option<String> {
    if h.count == 0 {
        return None;
    }
    let mut out = format!("📥 排队 {} 条", h.count);
    if !h.latest.is_empty() {
        let latest: String = h.latest.chars().take(40).collect();
        out.push_str(&format!("，最新：「{latest}」"));
    }
    Some(out)
}

/// 流式卡片会话。
pub(crate) struct CardSession {
    text: String,
    tools: Vec<ToolCall>,
    /// P8-1：执行阶段（思考中/调用工具/输出中）——按最近一次 chunk 类型翻转，
    /// 平台渲染成分状态 footer。
    phase: CardPhase,
    msg_id: Option<String>,
    last_patch: Instant,
    /// 在飞卡片登记（P4_ROADMAP 第六批）：首帧句柄落库、终态成功摘除——进程崩溃
    /// 后由 [`sweep_live_cards`] 启动扫描把滞留「生成中」的卡片 patch 成已中断。
    store: Store,
    conv: ConvId,
    platform_name: &'static str,
    /// P10：dispatcher 的排队状态句柄（每次 patch 拉取，见 [`queued_hint_display`]）。
    queued_hints: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, QueuedHint>>>,
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
            phase: CardPhase::Thinking,
            msg_id: None,
            last_patch: Instant::now(),
            store,
            conv,
            platform_name,
            queued_hints,
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
    pub(crate) async fn append_tool(
        &mut self,
        tool: &str,
        input_summary: &str,
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) {
        self.tools.push(ToolCall {
            name: tool.to_string(),
            summary: input_summary.to_string(),
            done: false,
        });
        self.phase = CardPhase::ToolRunning;
        self.patch_if_due(CardTerminal::Running, conv, hint, platform)
            .await;
    }

    /// P8-1：工具结果到达——把同名工具里最早未完成的一条翻成 ✅（工具结果
    /// 不带调用 id，按序配对；同名并发极少见，错配只影响图标不影响内容）。
    pub(crate) async fn finish_tool(
        &mut self,
        tool: &str,
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) {
        if let Some(t) = self.tools.iter_mut().find(|t| !t.done && t.name == tool) {
            t.done = true;
        }
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
        if !self.dispatch_card(terminal, conv, hint, platform).await && !self.text.is_empty() {
            match platform.send_text(conv, &self.text, hint).await {
                Ok(()) => warn!(target: "imagent::core", "卡片终态更新失败，已降级纯文本补发结论"),
                Err(e) => warn!(
                    target: "imagent::core",
                    error = %e,
                    "卡片终态更新失败，纯文本补发也失败（结论丢失）"
                ),
            }
        }
    }

    /// 节流 patch：首次（无 msg_id）或距上次 patch ≥ THROTTLE 才发；否则跳过。
    async fn patch_if_due(
        &mut self,
        terminal: CardTerminal,
        conv: &ConvId,
        hint: &ReplyHint,
        platform: &dyn Platform,
    ) {
        let due = self.msg_id.is_none() || self.last_patch.elapsed() >= CARD_THROTTLE;
        if !due {
            return;
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
            phase: self.phase,
            queued_hint: queued,
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
            Some(mid) => platform.update_card(conv, mid, &card, hint).await,
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
            phase: CardPhase::Thinking,
            queued_hint: None,
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
}
