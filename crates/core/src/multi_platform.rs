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
//!
//! ## 镜像路由（local → web，叠加在 A 类归属路由之上）
//!
//! 「终端 local 会话 → 网页驾驶舱实时镜像 + 干预」：发往 `local:` 前缀 conv
//! 的**出站投递方法**，除按前缀归属路由 local 平台外，**同时镜像投递给 web
//! 平台**（若在 children 中；见 [`MultiPlatform::route_with_mirror`]）。理由：
//! - web 是驾驶舱（浏览器看全部机器会话的实时视图），local 会话的实时可视化
//!   正是其核心场景；feishu 等 IM 平台是通知渠道，不应被终端会话的流式文本
//!   刷屏——不镜像。
//! - 「一个会话多个视图」的镜像语义与「按前缀归属」的路由并存：主投递结果
//!   仍是方法的返回值，镜像是 best-effort 旁路（失败仅 warn，不阻塞主路由）。
//! - 两个例外不镜像：`supports_streaming_card` 是能力查询不是投递（保持按
//!   前缀单查——local conv 恒 local 平台 false=文本流不受影响）；`note_round_initiator`
//!   是协作钩子且 web 平台未覆写（镜像恒为 no-op）。
//! - 配置化锚点：未来若需更多「平台 A 的会话镜像到平台 B」的规则，把这条
//!   (源前缀 → 目标平台) 硬编码规则升级为 config 驱动的 mirror 规则表。

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

/// 镜像路由规则（理由见模块文档「镜像路由」节）：`local:` 前缀 conv 的出站
/// 投递除归属平台外同时镜像给 web 平台。web 不在 children（如 feishu+local
/// 两平台配置）时镜像静默跳过。
const MIRROR_SOURCE_PREFIX: &str = "local:";
/// 镜像目标平台名（= 其 conv 前缀）。
const MIRROR_TARGET_PLATFORM: &str = "web";

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

    /// 镜像目标查找：conv 命中镜像源前缀（[`MIRROR_SOURCE_PREFIX`]）且 children
    /// 含 web 平台时返回其句柄，否则 None（无镜像）。web conv 自身不命中
    /// （源前缀是 local:），不会出现「web 主投递 + web 镜像」双发。
    fn mirror_target(&self, conv: &ConvId) -> Option<Arc<dyn Platform>> {
        if !conv.0.starts_with(MIRROR_SOURCE_PREFIX) {
            return None;
        }
        self.children
            .iter()
            .find(|c| c.name() == MIRROR_TARGET_PLATFORM)
            .cloned()
    }

    /// A 类出站投递方法的公共骨架：按 conv 前缀定位归属子平台；命中镜像规则
    /// （local → web，见模块文档「镜像路由」节）时先把同一投递**镜像**给 web
    /// 平台，再执行主投递并返回其结果。
    ///
    /// - `deliver` 接收目标平台句柄执行真正的投递调用——各出站方法仅参数列表
    ///   不同，镜像判定/目标查找/warn 收敛在此一处，避免逐方法重复展开；
    /// - 镜像是 best-effort 旁路：失败（web 掉线、出站队列满等）仅 warn，不
    ///   阻塞、不改写主路由结果——驾驶舱缺帧由 sync/重连兜底，不能反压终端
    ///   主会话；
    /// - 主路由失败（`child_for` 无命中）直接 Err，不镜像——无归属平台的 conv
    ///   不该凭空造出 web 视图。
    async fn route_with_mirror<T, F, Fut>(&self, conv: &ConvId, deliver: F) -> Result<T>
    where
        F: Fn(Arc<dyn Platform>) -> Fut,
        Fut: std::future::Future<Output = Result<T>> + Send,
    {
        let child = self.child_for(conv)?.clone();
        if let Some(web) = self.mirror_target(conv) {
            if let Err(e) = deliver(web).await {
                tracing::warn!(
                    target: "imagent::core",
                    conv = %conv.0,
                    error = %e,
                    "local 会话镜像投递 web 平台失败（best-effort，不影响主投递）"
                );
            }
        }
        deliver(child).await
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
        self.route_with_mirror(conv, |p| async move { p.send_text(conv, text, hint).await })
            .await
    }

    async fn send_media(&self, conv: &ConvId, media: &MediaRef, hint: &ReplyHint) -> Result<()> {
        self.route_with_mirror(
            conv,
            |p| async move { p.send_media(conv, media, hint).await },
        )
        .await
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
    /// 能力查询不是投递，不参与镜像（local conv 恒 local 平台 false=文本流）。
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
        self.route_with_mirror(conv, |p| async move { p.send_card(conv, card, hint).await })
            .await
    }

    async fn update_card(
        &self,
        conv: &ConvId,
        message_id: &str,
        card: &OutboundCard,
        hint: &ReplyHint,
    ) -> Result<()> {
        self.route_with_mirror(conv, |p| async move {
            p.update_card(conv, message_id, card, hint).await
        })
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
        self.route_with_mirror(conv, |p| async move {
            p.send_permission_ask(conv, request_id, tool_name, input_summary, hint)
                .await
        })
        .await
    }

    async fn send_permission_ask_text(
        &self,
        conv: &ConvId,
        tool_name: &str,
        input_summary: &str,
        hint: &ReplyHint,
    ) -> Result<()> {
        self.route_with_mirror(conv, |p| async move {
            p.send_permission_ask_text(conv, tool_name, input_summary, hint)
                .await
        })
        .await
    }

    async fn note_queued_on_ask(&self, conv: &ConvId, note: &str, hint: &ReplyHint) -> Result<()> {
        self.route_with_mirror(conv, |p| async move {
            p.note_queued_on_ask(conv, note, hint).await
        })
        .await
    }

    async fn cancel_permission_ask(&self, conv: &ConvId, request_id: &str) -> Result<()> {
        self.route_with_mirror(conv, |p| async move {
            p.cancel_permission_ask(conv, request_id).await
        })
        .await
    }

    async fn cancel_all_permission_asks(&self, conv: &ConvId) -> Result<()> {
        self.route_with_mirror(
            conv,
            |p| async move { p.cancel_all_permission_asks(conv).await },
        )
        .await
    }

    async fn resolve_permission_ask(
        &self,
        conv: &ConvId,
        request_id: &str,
        reply: &crate::permission::PermissionReply,
    ) -> Result<()> {
        self.route_with_mirror(conv, |p| async move {
            p.resolve_permission_ask(conv, request_id, reply).await
        })
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
        self.route_with_mirror(conv, |p| async move {
            p.send_command_card(conv, title, body_md, buttons, hint)
                .await
        })
        .await
    }

    async fn send_config_form(
        &self,
        conv: &ConvId,
        entries: &[ConfigFormField],
        fallback: &str,
        hint: &ReplyHint,
    ) -> Result<()> {
        self.route_with_mirror(conv, |p| async move {
            p.send_config_form(conv, entries, fallback, hint).await
        })
        .await
    }

    async fn react_to_message(
        &self,
        conv: &ConvId,
        source_msg_id: &str,
        reaction: MsgReaction,
    ) -> Result<()> {
        self.route_with_mirror(conv, |p| async move {
            p.react_to_message(conv, source_msg_id, reaction).await
        })
        .await
    }

    async fn send_typing(&self, conv: &ConvId, hint: &ReplyHint) -> Result<()> {
        self.route_with_mirror(conv, |p| async move { p.send_typing(conv, hint).await })
            .await
    }

    async fn send_urgent_text(&self, conv: &ConvId, text: &str, hint: &ReplyHint) -> Result<()> {
        self.route_with_mirror(conv, |p| async move {
            p.send_urgent_text(conv, text, hint).await
        })
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

    /// 无命中前缀静默跳过（无返回值的 best-effort 钩子，无处报错）。不镜像：
    /// 协作钩子非出站投递，且 web 平台未覆写（镜像恒为 no-op）。
    async fn note_round_initiator(&self, conv: &ConvId, sender: &str) {
        if let Ok(child) = self.child_for(conv) {
            child.note_round_initiator(conv, sender).await;
        }
    }

    /// v1.6 工作空间钩子：**广播全部子平台**（default no-op——非 web 平台零
    /// 成本；web 覆写为 conv_workdir 增量帧推送）。不走 [`Self::route_with_mirror`]
    /// 的归属+镜像模型：web 驾驶舱的 workspaces 快照 `convs` 含**全平台**会话
    /// 的当前目录，任何平台 conv（feishu:/local:/web:）的 `/cd` 都应让 web 看到
    /// ——按归属路由会把非 web conv 的目录变更对 SPA 隐身。
    fn on_workdir_changed(&self, conv: &ConvId, path: &str) {
        for c in &self.children {
            c.on_workdir_changed(conv, path);
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
    use crate::types::{CardPhase, CardTerminal, OutboundCard, UserId};

    /// 极简 mock 平台：name/caps/urgent/probes 可配；recv 从 mpsc 取（测试
    /// 注入 Ok/Err）；send_text 记录到共享 Vec（send_text_ok=false 时恒 Err，
    /// 镜像失败语义测试用）；supports_streaming_card 可配（镜像路由测试用）；
    /// reconnect_ok 决定 reconnect 返回 Ok 还是走 default Err（聚合语义测试用）。
    struct MockPlat {
        name: &'static str,
        caps: PlatformCaps,
        urgent: bool,
        rx: TokioMutex<mpsc::Receiver<Result<InboundMessage>>>,
        sent: Arc<TokioMutex<Vec<(String, String)>>>,
        probes: Vec<String>,
        reconnect_ok: bool,
        /// supports_streaming_card 覆写值（默认 false，与既有用例行为一致）。
        streaming: bool,
        /// false = send_text 恒 Err（镜像投递失败不影响主投递的测试用）。
        send_text_ok: bool,
    }

    /// 构造全参数 mock（mock / mock_opt 的公共实现）。
    #[allow(clippy::too_many_arguments)]
    fn mock_inner(
        name: &'static str,
        caps: PlatformCaps,
        urgent: bool,
        probes: Vec<String>,
        reconnect_ok: bool,
        streaming: bool,
        send_text_ok: bool,
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
            streaming,
            send_text_ok,
        });
        (p, tx, sent)
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
        mock_inner(name, caps, urgent, probes, reconnect_ok, false, true)
    }

    /// 精简 mock（镜像路由测试用）：caps 空 / 无 probes / reconnect 走 default
    /// Err，仅可配 supports_streaming_card 与 send_text 是否成功。
    fn mock_opt(
        name: &'static str,
        streaming: bool,
        send_text_ok: bool,
    ) -> (Arc<MockPlat>, InjectTx, SentLog) {
        mock_inner(
            name,
            PlatformCaps::empty(),
            false,
            Vec::new(),
            false,
            streaming,
            send_text_ok,
        )
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

    /// 空 tool/todo 的最小卡片夹具（镜像路由测试 send_card 用；mock 未覆写
    /// send_card，default 降级为 send_text——断言经 sent 记录端生效）。
    fn sample_card() -> OutboundCard {
        OutboundCard {
            text: "卡片".into(),
            tool_calls: Vec::new(),
            thoughts: Vec::new(),
            todos: Vec::new(),
            phase: CardPhase::Outputting,
            queued_hint: None,
            run_secs: 1,
            usage_display: None,
            task_digest: None,
            terminal: CardTerminal::Running,
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
        /// 可配覆写（镜像路由测试用：web 平台恒 true 的形态）。
        fn supports_streaming_card(&self, _conv: &ConvId) -> bool {
            self.streaming
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
            if !self.send_text_ok {
                return Err(CoreError::Platform(self.name, "mock send_text 失败".into()));
            }
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

    /// 用例 8：local conv 出站镜像路由——send_text 归属落 local、镜像落 web
    /// （驾驶舱），feishu（IM 通知渠道）不镜像；web/feishu conv 行为不变；
    /// Option 返回型方法（send_card）同样镜像；supports_streaming_card 保持
    /// 按前缀单查（local=false，不因 web 恒 true 被镜像污染）。
    #[tokio::test]
    async fn local_conv_outbound_mirrors_to_web() {
        // local（streaming=false）/ web（streaming=true，真平台形态）/ feishu。
        let (l, _ltx, l_sent) = mock_opt("local", false, true);
        let (w, _wtx, w_sent) = mock_opt("web", true, true);
        let (f, _ftx, f_sent) = mock_opt("feishu", false, true);
        let m = MultiPlatform::new(vec![l, w, f]).expect("合并 local+web+feishu");

        // local conv：归属 + 镜像双投递。
        m.send_text(&ConvId("local:work".into()), "hi", &ReplyHint::None)
            .await
            .unwrap();
        assert_eq!(
            l_sent.lock().await.as_slice(),
            &[("local:work".to_string(), "hi".to_string())],
            "归属平台照常收到"
        );
        assert_eq!(
            w_sent.lock().await.as_slice(),
            &[("local:work".to_string(), "hi".to_string())],
            "web 驾驶舱镜像收到"
        );
        assert!(f_sent.lock().await.is_empty(), "feishu 通知渠道不镜像");

        // Option 返回型方法（send_card→mock default 降级为 send_text 记录）同镜像。
        m.send_card(
            &ConvId("local:work".into()),
            &sample_card(),
            &ReplyHint::None,
        )
        .await
        .unwrap();
        assert_eq!(l_sent.lock().await.len(), 2);
        assert_eq!(w_sent.lock().await.len(), 2, "send_card 同样镜像到 web");

        // web / feishu conv：既有归属路由不变（web conv 不自我镜像）。
        m.send_text(&ConvId("web:x".into()), "w", &ReplyHint::None)
            .await
            .unwrap();
        m.send_text(&ConvId("feishu:y".into()), "f", &ReplyHint::None)
            .await
            .unwrap();
        assert_eq!(w_sent.lock().await.len(), 3, "web 只多收自己 conv 那条");
        assert_eq!(
            f_sent.lock().await.as_slice(),
            &[("feishu:y".to_string(), "f".to_string())]
        );

        // 能力查询不镜像：按前缀单查 local（false），不取 web 的 true。
        assert!(!m.supports_streaming_card(&ConvId("local:work".into())));
    }

    /// 用例 9：镜像投递失败（web 平台 Err）不影响主投递——local conv 的
    /// send_text 仍 Ok 且 local 平台照常收到（best-effort 旁路语义）。
    #[tokio::test]
    async fn mirror_failure_does_not_block_primary() {
        let (l, _ltx, l_sent) = mock_opt("local", false, true);
        let (w, _wtx, _w_sent) = mock_opt("web", true, false); // send_text 恒 Err
        let m = MultiPlatform::new(vec![l, w]).unwrap();
        m.send_text(&ConvId("local:work".into()), "hi", &ReplyHint::None)
            .await
            .expect("镜像失败不得阻塞主投递");
        assert_eq!(
            l_sent.lock().await.as_slice(),
            &[("local:work".to_string(), "hi".to_string())]
        );
    }

    /// 用例 10：children 无 web 平台时镜像静默跳过——local conv 照常 Ok、仅
    /// local 平台收到（feishu+local 两平台配置形态）。
    #[tokio::test]
    async fn mirror_skipped_without_web_child() {
        let (l, _ltx, l_sent) = mock_opt("local", false, true);
        let (f, _ftx, f_sent) = mock_opt("feishu", false, true);
        let m = MultiPlatform::new(vec![l, f]).unwrap();
        m.send_text(&ConvId("local:x".into()), "hi", &ReplyHint::None)
            .await
            .unwrap();
        assert_eq!(l_sent.lock().await.len(), 1);
        assert!(f_sent.lock().await.is_empty());
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
