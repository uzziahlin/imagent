//! 多平台组合器（M2.5）：把 feishu + web 等多个子平台合并成一个
//! `Arc<dyn Platform>` 交给 Dispatcher——**Dispatcher 零改动**（仍持单个
//! 平台 trait 对象、单点 `recv()`）。
//!
//! ## 为什么可行（已考察确认的事实）
//!
//! - `Platform::name()` 在 core 里只用于日志与 /doctor 文案，无逻辑特判；
//! - 各平台入站 conv 前缀恒等于其 `name()`（`ilink:<uid>` / `wecom:<uid>` /
//!   `feishu:<chat_id>` / `web:<session>`）→ **按 conv 前缀路由到子平台**是
//!   确定性的；
//! - `Platform` trait 的全部可选方法都带安全 default（no-op 或纯文本降级），
//!   子平台的 default 降级经本组合器透传后自然生效。
//!
//! ## 方法路由三分类
//!
//! - **A. 带 `conv: &ConvId` 的方法** → 取 conv 前缀（首个 `:` 之前）匹配子
//!   平台 `name()`，委托给命中者；无命中返回 Err（`supports_streaming_card`
//!   返回 false、`note_round_initiator` 静默——两者无错误可表达/无返回值）；
//! - **B. 无 conv 的全局方法** → 聚合（`capabilities` 并集、`reconnect` /
//!   `set_require_mention_in_group` 广播全败才 Err、`list_joined_chats` 合并
//!   Ok 结果、`doctor_probes` 逐行加 `[<name>]` 前缀）；
//! - **C. `recv`** → 每子平台一个泵任务合流（见 [`MultiPlatform`]）。
//!
//! Web 驾驶舱的定位是「与 IM 并存互补：Web 富渲染做驾驶舱，IM 做轻通知兜底」
//! ——没有本组合器则选 web 即掉 feishu，与主使用场景矛盾。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex as TokioMutex};
use tokio::task::JoinHandle;

use crate::error::{CoreError, Result};
use crate::platform::{Platform, PlatformCaps};
use crate::types::{
    CardButton, ConfigFormField, ConvId, InboundMessage, JoinedChat, MediaRef, MsgReaction,
    OutboundCard, ReplyHint,
};

/// 组合器自身的平台名（日志 / /doctor 文案用；conv 路由不经过它）。
const NAME: &str = "multi";

/// 入站合流 channel 容量：泵与 dispatcher 消费端之间的缓冲。真实平台的入站
/// 突发远小于此量级；满了泵会在 `tx.send` 处背压（不丢消息）。
const INBOUND_CHANNEL_CAP: usize = 64;

/// 泵任务非致命 recv 错误的退避起点 / 封顶（对齐 dispatch 主循环的
/// 1s→30s 指数退避语义）。
const PUMP_BACKOFF_START: Duration = Duration::from_secs(1);
const PUMP_BACKOFF_CAP: Duration = Duration::from_secs(30);

/// 多平台组合器：自身实现 [`Platform`]，把 ≥2 个子平台合并为一个平台对象。
///
/// ## recv 合流（每子平台一个泵任务，错误隔离）
///
/// 构造时为每个子平台 spawn 一个泵：`loop { child.recv() → tx.send }` 把各
/// 平台入站流合流到一条 mpsc channel，`recv()` 即从该 channel 取。错误处理
/// 对齐 dispatch 主循环的语义，分两类：
///
/// - `CoreError::SessionExpired`（致命，需重新 login）→ **透传**给 dispatcher
///   （其收到后 break 走统一 drain 收尾，行为与单平台一致）且该泵退出；
/// - 其它 Err → 泵内 `warn` + 自身指数退避重试（1s 起、封顶 30s、成功重置），
///   **不透传**——不让一个子平台的抖动把 dispatcher 的 recv 整体拖进退避、
///   延迟另一个健康平台的消息。
///
/// 全部泵退出后 channel 关闭，`recv()` 返回 Err（正常退出路径是 dispatcher
/// 先 break/drop，Drop 会 abort 全部泵，不会走到这里）。
pub struct MultiPlatform {
    /// 子平台（顺序 = 配置顺序；前缀路由按 name() 匹配，与顺序无关）。
    children: Vec<Arc<dyn Platform>>,
    /// 入站合流 channel 的接收端。Mutex 仅因 `Receiver` 非 `Sync`——消费端
    /// 唯一（dispatcher 的 recv 单点），锁不构成竞争。
    inbound_rx: TokioMutex<mpsc::Receiver<Result<InboundMessage>>>,
    /// 能力并集（构造时算好缓存；**为什么是并集**见 [`MultiPlatform::capabilities`]）。
    caps: PlatformCaps,
    /// `supports_urgent_text` 的 any 聚合（构造时缓存）。
    urgent_any: bool,
    /// 子平台 recv 泵任务句柄。Drop 时 abort——dispatcher 退出（组合器被 drop）
    /// 后泵不应残留阻塞在子平台 recv 上。
    _pump_tasks: Vec<JoinHandle<()>>,
}

impl MultiPlatform {
    /// 合并 ≥2 个子平台。约束：
    /// - **至少 2 个**：1 个直接用原平台即可（调用方保证；本函数 debug_assert，
    ///   release 下 1 个也 merely 退化成透传组合器，无害）；
    /// - 子平台 `name()` **必须互异**：conv 前缀路由按 name 匹配，重名 = 路由
    ///   歧义，直接 Err。
    pub fn new(children: Vec<Arc<dyn Platform>>) -> Result<Self> {
        debug_assert!(
            children.len() >= 2,
            "MultiPlatform 预期合并 ≥2 个子平台（单个请直接用原平台）"
        );
        // name 互异校验（release 也生效——装配错误要在启动期暴露，不能等运行期
        // 路由歧义才炸）。
        let mut seen: Vec<&'static str> = Vec::with_capacity(children.len());
        for c in &children {
            let n = c.name();
            if seen.contains(&n) {
                return Err(CoreError::Platform(
                    NAME,
                    format!("子平台 name 重复（{n}）——conv 前缀路由歧义，拒绝合并"),
                ));
            }
            seen.push(n);
        }

        // 能力并集 + urgent any，构造时一次算好（capabilities() 是同步方法且
        // 会被 /doctor、启动日志反复读，不逐次遍历子平台）。
        let caps = children
            .iter()
            .fold(PlatformCaps::empty(), |acc, c| acc | c.capabilities());
        let urgent_any = children.iter().any(|c| c.supports_urgent_text());

        // 每子平台一个 recv 泵：合流 + 错误隔离（见类型文档）。
        let (tx, rx) = mpsc::channel(INBOUND_CHANNEL_CAP);
        let pumps: Vec<JoinHandle<()>> = children
            .iter()
            .map(|child| {
                let child = child.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let name = child.name();
                    let mut backoff = PUMP_BACKOFF_START;
                    loop {
                        match child.recv().await {
                            Ok(m) => {
                                backoff = PUMP_BACKOFF_START; // 成功，重置退避
                                if tx.send(Ok(m)).await.is_err() {
                                    // 接收端已 drop（dispatcher 退出、组合器析构）
                                    break;
                                }
                            }
                            Err(e) if matches!(e, CoreError::SessionExpired(_)) => {
                                // 致命：透传后本泵退出——dispatcher 收到 SessionExpired
                                // 会 break 走统一 drain 收尾（与单平台语义一致）。
                                tracing::error!(
                                    target: "imagent::core",
                                    platform = name,
                                    error = %e,
                                    "子平台 session 过期，透传给 dispatcher（需重新 login），该泵退出"
                                );
                                let _ = tx.send(Err(e)).await;
                                break;
                            }
                            Err(e) => {
                                // 非致命：泵内退避重试、不透传——错误隔离的意义所在
                                //（见类型文档）。dispatch 主循环对非致命 recv 错误
                                // 同样退避，但那是「单平台内」的重试；跨平台抖动
                                // 若上抛会让健康平台的合流消息一并被退避延迟。
                                tracing::warn!(
                                    target: "imagent::core",
                                    platform = name,
                                    error = %e,
                                    backoff_secs = backoff.as_secs(),
                                    "子平台 recv 失败，泵内退避重试（不透传，隔离平台间抖动）"
                                );
                                tokio::time::sleep(backoff).await;
                                backoff = (backoff * 2).min(PUMP_BACKOFF_CAP);
                            }
                        }
                    }
                    tracing::debug!(
                        target: "imagent::core",
                        platform = name,
                        "子平台 recv 泵退出"
                    );
                })
            })
            .collect();
        // 本地 tx 必须在此 drop：否则即使全部泵退出、channel 也因这个存活 sender
        // 不关闭，dispatcher 的 recv 将永远挂起而非收到「泵全退」错误。
        drop(tx);

        Ok(Self {
            children,
            inbound_rx: TokioMutex::new(rx),
            caps,
            urgent_any,
            _pump_tasks: pumps,
        })
    }

    /// 按 conv 前缀（首个 `:` 之前，如 `feishu:oc_x` → `feishu`）定位子平台。
    /// 无命中 → Err（配置撤下平台后残存会话/历史 conv 的调用会走到这里，报错
    /// 比静默丢弃可见性好）。
    fn child_for(&self, conv: &ConvId) -> std::result::Result<&Arc<dyn Platform>, CoreError> {
        let prefix = conv.0.split(':').next().unwrap_or("");
        self.children
            .iter()
            .find(|c| c.name() == prefix)
            .ok_or_else(|| {
                CoreError::Platform(
                    NAME,
                    format!("conv 前缀 {prefix:?} 无对应平台（conv={}）", conv.0),
                )
            })
    }
}

impl Drop for MultiPlatform {
    fn drop(&mut self) {
        // dispatcher 退出（run() 返回、组合器被 drop）后 abort 全部泵——泵任务
        // 阻塞在子平台 recv 上，不 abort 会随 runtime 退出才结束（子平台自身
        // 无 shutdown 信号面的前提下）。
        for t in self._pump_tasks.drain(..) {
            t.abort();
        }
    }
}

#[async_trait]
impl Platform for MultiPlatform {
    async fn recv(&self) -> Result<InboundMessage> {
        match self.inbound_rx.lock().await.recv().await {
            // 泵透传的 Err 目前只有 SessionExpired（非致命错误泵内消化），
            // 原样上抛交 dispatcher 走其既有的致命/退避分类。
            Some(r) => r,
            None => Err(CoreError::Platform(
                NAME,
                "所有子平台 recv 泵已退出（入站合流 channel 关闭）".into(),
            )),
        }
    }

    async fn send_text(&self, conv: &ConvId, text: &str, hint: &ReplyHint) -> Result<()> {
        self.child_for(conv)?.send_text(conv, text, hint).await
    }

    async fn send_media(&self, conv: &ConvId, media: &MediaRef, hint: &ReplyHint) -> Result<()> {
        self.child_for(conv)?.send_media(conv, media, hint).await
    }

    fn name(&self) -> &'static str {
        NAME
    }

    /// 能力 = 子平台能力**并集**（构造时缓存）。**为什么不是交集**：core 消费点
    /// 对能力位的用法是「位未置 → 跳过注定 no-op 的 best-effort 往返」。若取
    /// 交集，feishu 独有的 REACTIONS/TYPING/FORMS 等位会被抹掉 → core 对
    /// feishu conv 也跳过这些调用 → **feishu 功能被组合器削弱**。取并集则：
    /// core 对 web conv 调 `react_to_message` → 路由到 web → web 的 default
    /// no-op → 无害——trait 的 default 安全降级正是为此设计的。
    fn capabilities(&self) -> PlatformCaps {
        self.caps
    }

    /// 无命中前缀按 false（无该平台 = 该 conv 无卡片语义，core 走纯文本路径）。
    fn supports_streaming_card(&self, conv: &ConvId) -> bool {
        self.child_for(conv)
            .map(|c| c.supports_streaming_card(conv))
            .unwrap_or(false)
    }

    async fn send_card(
        &self,
        conv: &ConvId,
        card: &OutboundCard,
        hint: &ReplyHint,
    ) -> Result<Option<String>> {
        self.child_for(conv)?.send_card(conv, card, hint).await
    }

    async fn update_card(
        &self,
        conv: &ConvId,
        message_id: &str,
        card: &OutboundCard,
        hint: &ReplyHint,
    ) -> Result<()> {
        self.child_for(conv)?
            .update_card(conv, message_id, card, hint)
            .await
    }

    async fn send_permission_ask(
        &self,
        conv: &ConvId,
        request_id: &str,
        tool_name: &str,
        input_summary: &str,
        hint: &ReplyHint,
    ) -> Result<Option<String>> {
        self.child_for(conv)?
            .send_permission_ask(conv, request_id, tool_name, input_summary, hint)
            .await
    }

    async fn send_permission_ask_text(
        &self,
        conv: &ConvId,
        tool_name: &str,
        input_summary: &str,
        hint: &ReplyHint,
    ) -> Result<()> {
        self.child_for(conv)?
            .send_permission_ask_text(conv, tool_name, input_summary, hint)
            .await
    }

    async fn note_queued_on_ask(&self, conv: &ConvId, note: &str, hint: &ReplyHint) -> Result<()> {
        self.child_for(conv)?
            .note_queued_on_ask(conv, note, hint)
            .await
    }

    async fn cancel_permission_ask(&self, conv: &ConvId, request_id: &str) -> Result<()> {
        self.child_for(conv)?
            .cancel_permission_ask(conv, request_id)
            .await
    }

    async fn cancel_all_permission_asks(&self, conv: &ConvId) -> Result<()> {
        self.child_for(conv)?.cancel_all_permission_asks(conv).await
    }

    async fn resolve_permission_ask(
        &self,
        conv: &ConvId,
        request_id: &str,
        reply: &crate::permission::PermissionReply,
    ) -> Result<()> {
        self.child_for(conv)?
            .resolve_permission_ask(conv, request_id, reply)
            .await
    }

    async fn send_command_card(
        &self,
        conv: &ConvId,
        title: &str,
        body_md: &str,
        buttons: &[CardButton],
        hint: &ReplyHint,
    ) -> Result<()> {
        self.child_for(conv)?
            .send_command_card(conv, title, body_md, buttons, hint)
            .await
    }

    async fn send_config_form(
        &self,
        conv: &ConvId,
        entries: &[ConfigFormField],
        fallback: &str,
        hint: &ReplyHint,
    ) -> Result<()> {
        self.child_for(conv)?
            .send_config_form(conv, entries, fallback, hint)
            .await
    }

    async fn react_to_message(
        &self,
        conv: &ConvId,
        source_msg_id: &str,
        reaction: MsgReaction,
    ) -> Result<()> {
        self.child_for(conv)?
            .react_to_message(conv, source_msg_id, reaction)
            .await
    }

    async fn send_typing(&self, conv: &ConvId, hint: &ReplyHint) -> Result<()> {
        self.child_for(conv)?.send_typing(conv, hint).await
    }

    async fn send_urgent_text(&self, conv: &ConvId, text: &str, hint: &ReplyHint) -> Result<()> {
        self.child_for(conv)?
            .send_urgent_text(conv, text, hint)
            .await
    }

    /// any 聚合，与 [`Self::capabilities`] 取并集同理：core 对「完成强提醒」
    /// 按本值决定是否发送——取 any 则 web conv 的强提醒走 web 的
    /// `send_urgent_text` default（回退普通文本），内容仍送达；取 all/交集会让
    /// feishu 用户丢掉强提醒。
    fn supports_urgent_text(&self) -> bool {
        self.urgent_any
    }

    /// 广播到全部子平台；**全部 Err 才 Err**（拼接各平台文案）——部分平台
    /// 不支持重连（default Err「该平台不支持强制重连」）是正常态，任一成功
    /// 即整体成功。
    async fn reconnect(&self) -> Result<()> {
        let mut errs = Vec::new();
        for c in &self.children {
            if let Err(e) = c.reconnect().await {
                errs.push(format!("[{}] {e}", c.name()));
            }
        }
        if errs.len() == self.children.len() {
            Err(CoreError::Platform(NAME, errs.join("；")))
        } else {
            Ok(())
        }
    }

    /// 第一个返回 Some 的子平台的值（None = 该平台无群聊 @ 概念，跳过问下家）。
    async fn require_mention_in_group(&self) -> Option<bool> {
        let mut val = None;
        for c in &self.children {
            if let Some(v) = c.require_mention_in_group().await {
                val = Some(v);
                break;
            }
        }
        val
    }

    /// 广播；任一 Ok 即 Ok，全 Err 才 Err（语义同 [`Self::reconnect`]）。
    async fn set_require_mention_in_group(&self, on: bool) -> Result<()> {
        let mut errs = Vec::new();
        for c in &self.children {
            if let Err(e) = c.set_require_mention_in_group(on).await {
                errs.push(format!("[{}] {e}", c.name()));
            }
        }
        if errs.len() == self.children.len() {
            Err(CoreError::Platform(NAME, errs.join("；")))
        } else {
            Ok(())
        }
    }

    /// 合并所有 Ok 结果（子平台返回的 chat_id 为 conv 形态、自带平台前缀，
    /// 天然不冲突）；全部 Err 才 Err。部分失败打 warn（合并结果仍返回——
    /// 已知平台正常、另一平台无群概念是常态）。
    async fn list_joined_chats(&self) -> Result<Vec<JoinedChat>> {
        let mut all = Vec::new();
        let mut errs = Vec::new();
        for c in &self.children {
            match c.list_joined_chats().await {
                Ok(mut chats) => all.append(&mut chats),
                Err(e) => errs.push(format!("[{}] {e}", c.name())),
            }
        }
        if errs.len() == self.children.len() {
            return Err(CoreError::Platform(NAME, errs.join("；")));
        }
        if !errs.is_empty() {
            tracing::warn!(
                target: "imagent::core",
                errors = %errs.join("；"),
                "部分子平台 list_joined_chats 失败，已合并其余平台结果"
            );
        }
        Ok(all)
    }

    /// 无命中前缀静默跳过（无返回值的 best-effort 钩子，无处报错）。
    async fn note_round_initiator(&self, conv: &ConvId, sender: &str) {
        if let Ok(child) = self.child_for(conv) {
            child.note_round_initiator(conv, sender).await;
        }
    }

    /// 逐子平台探测，每行加 `[<name>] ` 前缀后合并——/doctor 里可分辨哪行
    /// 来自哪个平台。
    async fn doctor_probes(&self) -> Vec<String> {
        let mut out = Vec::new();
        for c in &self.children {
            for line in c.doctor_probes().await {
                out.push(format!("[{}] {}", c.name(), line));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    //! MultiPlatform 组合器本体：前缀路由 / recv 合流 / 错误隔离 / 能力并集 /
    //! 全局方法聚合。mock 独立定义（与 dispatch/tests.rs 的 MockPlatform 无
    //! 耦合——那边面向 dispatcher 集成，字段面完全不同）。

    use super::*;
    use crate::types::UserId;

    /// 极简 mock 平台：name/caps/urgent/probes 可配；recv 从 mpsc 取（测试
    /// 注入 Ok/Err）；send_text 记录到共享 Vec；reconnect_ok 决定 reconnect
    /// 返回 Ok 还是走 default Err（聚合语义测试用）。
    struct MockPlat {
        name: &'static str,
        caps: PlatformCaps,
        urgent: bool,
        rx: TokioMutex<mpsc::Receiver<Result<InboundMessage>>>,
        sent: Arc<TokioMutex<Vec<(String, String)>>>,
        probes: Vec<String>,
        reconnect_ok: bool,
    }

    /// 构造 mock：返回（平台句柄、recv 注入端、send_text 记录端）。
    /// `reconnect_ok` 决定 reconnect 返回 Ok 还是 Err（聚合语义测试用）。
    fn mock(
        name: &'static str,
        caps: PlatformCaps,
        urgent: bool,
        probes: Vec<String>,
        reconnect_ok: bool,
    ) -> (Arc<MockPlat>, InjectTx, SentLog) {
        let (tx, rx) = mpsc::channel(8);
        let sent = Arc::new(TokioMutex::new(Vec::new()));
        let p = Arc::new(MockPlat {
            name,
            caps,
            urgent,
            rx: TokioMutex::new(rx),
            sent: sent.clone(),
            probes,
            reconnect_ok,
        });
        (p, tx, sent)
    }

    fn imsg(conv: &str) -> InboundMessage {
        InboundMessage {
            conv_id: ConvId(conv.into()),
            sender: UserId("u1".into()),
            sender_name: None,
            text: Some("hi".into()),
            media: Vec::new(),
            media_errors: Vec::new(),
            mentions: Vec::new(),
            mentioned_bot: false,
            ask_req: None,
            reply_to: None,
            source_msg_id: None,
            control: None,
            no_steer: false,
            reply_hint: ReplyHint::None,
        }
    }

    #[async_trait]
    impl Platform for MockPlat {
        fn name(&self) -> &'static str {
            self.name
        }
        fn capabilities(&self) -> PlatformCaps {
            self.caps
        }
        fn supports_urgent_text(&self) -> bool {
            self.urgent
        }
        /// 队列空时挂起（sender 在测试手中），与真实平台「无消息长轮询」同形。
        async fn recv(&self) -> Result<InboundMessage> {
            self.rx.lock().await.recv().await.unwrap_or_else(|| {
                Err(CoreError::Platform(
                    self.name,
                    "mock recv 队列已关闭".into(),
                ))
            })
        }
        async fn send_text(&self, conv: &ConvId, text: &str, _hint: &ReplyHint) -> Result<()> {
            self.sent
                .lock()
                .await
                .push((conv.0.clone(), text.to_string()));
            Ok(())
        }
        async fn send_media(
            &self,
            _conv: &ConvId,
            _media: &MediaRef,
            _hint: &ReplyHint,
        ) -> Result<()> {
            Ok(())
        }
        async fn doctor_probes(&self) -> Vec<String> {
            self.probes.clone()
        }
        async fn reconnect(&self) -> Result<()> {
            if self.reconnect_ok {
                Ok(())
            } else {
                // 与 trait default 同文案（default 非公开可复用，直接重写）。
                Err(CoreError::Platform(
                    self.name,
                    "该平台不支持强制重连".into(),
                ))
            }
        }
    }

    /// send_text 记录端类型别名（mock 返回元组的公共成员，压平 clippy 复杂度）。
    type SentLog = Arc<TokioMutex<Vec<(String, String)>>>;
    /// mock 的 recv 注入端。
    type InjectTx = mpsc::Sender<Result<InboundMessage>>;

    fn two_children() -> (MultiPlatform, InjectTx, InjectTx, SentLog, SentLog) {
        let (a, atx, a_sent) = mock("a", PlatformCaps::empty(), false, Vec::new(), false);
        let (b, btx, b_sent) = mock("b", PlatformCaps::empty(), false, Vec::new(), false);
        let m = MultiPlatform::new(vec![a, b]).expect("合并 a+b");
        (m, atx, btx, a_sent, b_sent)
    }

    /// 用例 1：conv 前缀路由——a:x 落 a、b:y 落 b、无命中 c:z 报「无对应平台」；
    /// supports_streaming_card 无命中返回 false。
    #[tokio::test]
    async fn prefix_routing_send_text() {
        let (m, _atx, _btx, a_sent, b_sent) = two_children();
        m.send_text(&ConvId("a:x".into()), "hi-a", &ReplyHint::None)
            .await
            .unwrap();
        m.send_text(&ConvId("b:y".into()), "hi-b", &ReplyHint::None)
            .await
            .unwrap();
        assert_eq!(
            a_sent.lock().await.as_slice(),
            &[("a:x".to_string(), "hi-a".to_string())]
        );
        assert_eq!(
            b_sent.lock().await.as_slice(),
            &[("b:y".to_string(), "hi-b".to_string())]
        );
        let err = m
            .send_text(&ConvId("c:z".into()), "hi-c", &ReplyHint::None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("无对应平台"), "{err}");
        assert!(!m.supports_streaming_card(&ConvId("c:z".into())));
    }

    /// 用例 2：recv 合流——a、b 各注入一条，组合器 recv 两次拿到两条（顺序
    /// 不保证，断言集合）。
    #[tokio::test]
    async fn recv_merges_children() {
        let (m, atx, btx, _a_sent, _b_sent) = two_children();
        atx.send(Ok(imsg("a:1"))).await.unwrap();
        btx.send(Ok(imsg("b:1"))).await.unwrap();
        let mut got = vec![
            m.recv().await.unwrap().conv_id.0,
            m.recv().await.unwrap().conv_id.0,
        ];
        got.sort();
        assert_eq!(got, vec!["a:1".to_string(), "b:1".to_string()]);
    }

    /// 用例 3：错误隔离——非致命 Err（Platform 类）泵内退避重试不透传，随后
    /// 恢复吐 Ok。start_paused + 拨钟跳过退避等待。
    #[tokio::test(start_paused = true)]
    async fn recv_error_isolation_nonfatal() {
        let (m, atx, btx, _a_sent, _b_sent) = two_children();
        // a 泵先吃到非致命 Err → 泵内 warn+退避（paused 时间自动推进）→ 重试
        // 拿到 Ok；b 正常。组合器 recv 不应吐出任何 Err。
        atx.send(Err(CoreError::Platform("a", "boom".into())))
            .await
            .unwrap();
        atx.send(Ok(imsg("a:1"))).await.unwrap();
        btx.send(Ok(imsg("b:1"))).await.unwrap();
        let mut got = vec![
            m.recv().await.expect("非致命错误不得透传为 Err").conv_id.0,
            m.recv().await.expect("应拿到第二条 Ok").conv_id.0,
        ];
        got.sort();
        assert_eq!(got, vec!["a:1".to_string(), "b:1".to_string()]);
    }

    /// 用例 4：致命透传——SessionExpired 原样上抛（dispatcher 据此 break 走
    /// 统一 drain 收尾）。
    #[tokio::test]
    async fn recv_session_expired_passthrough() {
        let (m, atx, _btx, _a_sent, _b_sent) = two_children();
        atx.send(Err(CoreError::SessionExpired("re-login".into())))
            .await
            .unwrap();
        match m.recv().await {
            Err(CoreError::SessionExpired(_)) => {}
            other => panic!("应透传 SessionExpired，得到 {other:?}"),
        }
    }

    /// 用例 5：capabilities 并集（非交集）+ supports_urgent_text any。
    #[tokio::test]
    async fn capabilities_union_and_urgent_any() {
        let (a, _atx, _a_sent) = mock("a", PlatformCaps::CARDS, true, Vec::new(), false);
        let (b, _btx, _b_sent) = mock(
            "b",
            PlatformCaps::ASK | PlatformCaps::REACTIONS,
            false,
            Vec::new(),
            false,
        );
        let m = MultiPlatform::new(vec![a, b]).unwrap();
        assert_eq!(
            m.capabilities(),
            PlatformCaps::CARDS | PlatformCaps::ASK | PlatformCaps::REACTIONS
        );
        assert!(m.supports_urgent_text(), "任一子平台支持即 any");
    }

    /// 用例 6：new 校验——子平台 name 重复 → Err（前缀路由歧义）。
    #[tokio::test]
    async fn new_rejects_duplicate_names() {
        let (a1, _tx1, _s1) = mock("a", PlatformCaps::empty(), false, Vec::new(), false);
        let (a2, _tx2, _s2) = mock("a", PlatformCaps::empty(), false, Vec::new(), false);
        // .err() 避免 unwrap_err 对 Ok 侧的 Debug 要求（MultiPlatform 无 Debug）。
        let err = MultiPlatform::new(vec![a1, a2])
            .err()
            .expect("重名子平台应拒绝合并");
        assert!(err.to_string().contains("重复"), "{err}");
    }

    /// 用例 7：doctor_probes 聚合——每行带 `[<name>] ` 前缀。
    #[tokio::test]
    async fn doctor_probes_prefixed() {
        let (a, _atx, _a_sent) = mock(
            "a",
            PlatformCaps::empty(),
            false,
            vec!["✅ a-probe".into()],
            false,
        );
        let (b, _btx, _b_sent) = mock(
            "b",
            PlatformCaps::empty(),
            false,
            vec!["⚠️ b-probe".into()],
            false,
        );
        let m = MultiPlatform::new(vec![a, b]).unwrap();
        assert_eq!(
            m.doctor_probes().await,
            vec!["[a] ✅ a-probe".to_string(), "[b] ⚠️ b-probe".to_string()]
        );
    }

    /// 用例（补充）：reconnect 广播聚合——全部子平台失败才整体 Err（文案拼接
    /// 各平台名）；任一成功即 Ok。
    #[tokio::test]
    async fn reconnect_broadcast_aggregate() {
        // 两员全败（default Err「该平台不支持强制重连」）→ 整体 Err，文案含
        // 两个平台名。
        let (a, _atx, _a_sent) = mock("a", PlatformCaps::empty(), false, Vec::new(), false);
        let (b, _btx, _b_sent) = mock("b", PlatformCaps::empty(), false, Vec::new(), false);
        let m = MultiPlatform::new(vec![a, b]).unwrap();
        let err = m.reconnect().await.unwrap_err().to_string();
        assert!(
            err.contains("[a]") && err.contains("[b]"),
            "拼接各平台文案: {err}"
        );

        // 一员成功 → 整体 Ok（部分平台不支持重连是正常态）。
        let (bad, _btx, _s1) = mock("bad", PlatformCaps::empty(), false, Vec::new(), false);
        let (good, _gtx, _s2) = mock("good", PlatformCaps::empty(), false, Vec::new(), true);
        let m = MultiPlatform::new(vec![bad, good]).unwrap();
        m.reconnect().await.expect("任一子平台成功即整体 Ok");
    }
}
