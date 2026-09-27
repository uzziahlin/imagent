//! ask/审批卡生命周期（T19 拆分自 platform.rs，纯移动）。
//!
//! 职责：`pending_asks` 登记（request_id → 卡片信息，按钮/表情回调据此路由与
//! 发起者校验）、resolve/cancel 收敛、P8-2 复用槽（`AskSlot`）与 note 联动
//! （`asks_since_card` 顶起标记）所需的状态操作。发送/patch 原语仍在
//! [`super`]（本模块只做状态登记与编排，HTTP 细节经 `with_token` 等复用）。

use tracing::{debug, warn};

use crate::client::patch_card;

use super::{fetch_cached_token, FeishuPlatform};

/// 一张 pending 询问卡的登记项：conv + 消息 id + 工具名 + **发起者**（群 conv 下
/// 按钮点击者校验用——询问由谁发起，只有其本人可答复；私聊不校验，单人）。
#[derive(Debug, Clone)]
pub(super) struct PendingAskCard {
    pub(super) conv_id: String,
    pub(super) msg_id: String,
    pub(super) tool_name: String,
    pub(super) sender: String,
}

/// P8-2：审批卡复用槽（per conv）。`pending_req = None` 表示卡已收敛
/// （已批准/已拒绝/已中断）可被下一个询问**原地 patch 复用**——顺序询问
/// 不再每条刷一张新卡把流式卡顶上去；`Some(req)` = 挂着未决询问
/// （并发询问须另发新卡，防顶掉别人还没答的请求）。
#[derive(Debug)]
pub(super) struct AskSlot {
    pub(super) msg_id: String,
    pub(super) pending_req: Option<String>,
    /// 最近一次询问收敛的时刻。真机校准（2026-08）：跨轮次复用会把新询问
    /// patch 到早已被结果卡/后续消息顶离视口的历史卡上——用户看不到询问，
    /// 表现为「卡住」直到超时催办。复用仅设计于新鲜窗口内成立；None = 挂着未决询问或刚登记的新卡（不可复用）。
    /// P10-③：重渲染输入（note 联动更新时按原参数重画整卡，按钮 value 不变）。
    pub(super) render: AskRender,
}

/// 询问卡的渲染输入（复用槽与新卡登记时记录）。
#[derive(Clone, Debug)]
pub(super) struct AskRender {
    /// 是否 AskUserQuestion 问题卡（否则审批卡）。
    pub(super) question: bool,
    pub(super) tool_name: String,
    /// 审批卡=input 摘要 JSON；问题卡=AskUserQuestion 原始 input JSON。
    pub(super) input: String,
    /// 询问发起者 open_id（note 联动重渲染时保持按钮 value 的 sender 编码不丢）。
    pub(super) sender: String,
}
impl FeishuPlatform {
    /// 登记一张 pending 询问卡；同 request_id 的旧卡 patch 成 superseded
    ///（异常重发场景，正常路径 request_id 唯一）。best-effort。
    async fn record_pending_ask(
        &self,
        request_id: &str,
        conv_id: &str,
        msg_id: &str,
        tool_name: &str,
        sender: &str,
    ) {
        let superseded = self.pending_asks.lock().await.insert(
            request_id.to_string(),
            PendingAskCard {
                conv_id: conv_id.to_string(),
                msg_id: msg_id.to_string(),
                tool_name: tool_name.to_string(),
                sender: sender.to_string(),
            },
        );
        if let Some(old) = superseded {
            // P8-2：同卡复用重登记（复用槽换 request_id）不是「取代」——同一张卡
            // 不能 patch 成 superseded 顶掉自己刚挂上的新询问。
            if old.msg_id == msg_id {
                return;
            }
            let card_json = crate::card::render_permission_card_superseded(&old.tool_name);
            if let Err(e) = self
                .with_token(|t| {
                    let old_mid = old.msg_id.clone();
                    let card_json = card_json.clone();
                    async move { patch_card(&self.core_config, &t, &old_mid, &card_json).await }
                })
                .await
            {
                warn!(target: "feishu", error = %e, "旧询问卡取代收敛失败（无害）");
            }
        }
    }
    /// P8-2：登记一张**新发**的询问卡：pending 登记（request_id 路由）+ 复用槽
    /// （收敛后供下一个询问原地复用）+ 顶起标记（终态结果下沉判定）。
    /// 安全批次：发起者（最近 sender）一并登记（群 conv 点击者校验）。
    pub(super) async fn register_ask_card(
        &self,
        conv_id: &str,
        msg_id: &str,
        request_id: &str,
        tool_name: &str,
        render: AskRender,
    ) {
        let sender = self.last_sender(conv_id).await;
        self.note_card_tail(conv_id, msg_id).await;
        // v1.24 卡片 UX：审批/问题卡到达即应用内加急——弹通知触达（免打扰
        // 时段跳过；urgent_app 只对最新卡生效，刚 note_card_tail 即本卡）。
        // spawn：不阻塞审批 hook 返回。加急对象 = 发起者（该答复的人）。
        if self.urgent_on_ask && !self.in_quiet_hours() && !sender.is_empty() {
            let cfg = self.core_config.clone();
            let token_lock = self.token.clone();
            let aid = self.app_id.clone();
            let sec = self.app_secret.clone();
            let mid = msg_id.to_string();
            let uid = sender.clone();
            tokio::spawn(async move {
                match fetch_cached_token(&token_lock, &cfg, &aid, &sec).await {
                    Ok(t) => {
                        if let Err(e) = crate::client::urgent_app_buzz(&cfg, &t, &mid, &uid).await {
                            debug!(target: "feishu", error = %e, "审批卡加急失败（不影响审批流程）");
                        }
                    }
                    Err(e) => debug!(target: "feishu", error = %e, "审批卡加急取 token 失败"),
                }
            });
        }
        self.record_pending_ask(request_id, conv_id, msg_id, tool_name, &sender)
            .await;
        self.conv_states
            .lock()
            .await
            .entry(conv_id.to_string())
            .or_default()
            .ask_slot = Some(AskSlot {
            msg_id: msg_id.to_string(),
            pending_req: Some(request_id.to_string()),
            render,
        });
        self.mark_ask_sent(conv_id).await;
    }

    /// P8-2：标记本轮流式卡之后发过询问卡（终态「结果下沉」判定）。
    pub(super) async fn mark_ask_sent(&self, conv_id: &str) {
        self.conv_states
            .lock()
            .await
            .entry(conv_id.to_string())
            .or_default()
            .asks_since_card = true;
    }

    /// P8-2：只读「发过询问卡」标记（终态下沉判定用，不消费）。
    /// v1.18 review：终态 patch 全链路（token 获取 → patch → 下沉重发）都成功
    /// 才由 [`clear_asks_flag`] 消费——此前 take 在 patch 前消费，patch 失败后
    /// 标志已丢：重试/孤儿扫描以「未下沉」形态把全文 patch 到早已被顶离视口
    /// 的旧卡上，设计的下沉 UX 静默丢失且不可恢复。
    pub(super) async fn peek_asks_flag(&self, conv_id: &str) -> bool {
        self.conv_states
            .lock()
            .await
            .get(conv_id)
            .is_some_and(|s| s.asks_since_card)
    }

    /// P8-2：消费「发过询问卡」标记（仅终态全链路成功后调用）。
    pub(super) async fn clear_asks_flag(&self, conv_id: &str) {
        self.conv_states
            .lock()
            .await
            .entry(conv_id.to_string())
            .or_default()
            .asks_since_card = false;
    }

    /// 兼容旧语义的单测辅助（取出并清除）。
    #[cfg(test)]
    async fn take_asks_flag(&self, conv_id: &str) -> bool {
        self.conv_states
            .lock()
            .await
            .get_mut(conv_id)
            .map(|s| std::mem::replace(&mut s.asks_since_card, false))
            .unwrap_or(false)
    }

    /// P8-2：释放该 conv 的复用槽（询问收敛后调用——卡保留在 IM 里，下一个
    /// 询问原地 patch 复用，不另发新卡）。
    pub(super) async fn free_ask_slot(&self, conv_id: &str, request_id: &str) {
        if let Some(slot) = self
            .conv_states
            .lock()
            .await
            .get_mut(conv_id)
            .and_then(|s| s.ask_slot.as_mut())
        {
            if slot.pending_req.as_deref() == Some(request_id) {
                slot.pending_req = None;
            }
        }
    }
}

/// 空串 sender → None（AskRender.sender 的 Option 形态适配渲染入参）。
pub(super) fn sender_opt_of(sender: &str) -> Option<&str> {
    (!sender.is_empty()).then_some(sender)
}
#[cfg(test)]
mod tests {
    use super::*;

    use imagent_core::ConvId;

    /// P8-2：顶起标记——send_card 清零、发询问卡置位、终态取走即清。
    #[tokio::test]
    async fn asks_flag_roundtrip() {
        let p = FeishuPlatform::new(
            "cli_test".into(),
            "secret_test".into(),
            "https://open.feishu.cn".into(),
            true,
            None,
            300,
            None,
            1800,
            true,
            // v1.21：outbox 未接 store / 发送限速关闭（测试态）；v1.24 加急关。
            None,
            0.0,
            false,
            0,
        )
        .expect("构造");
        let conv = ConvId("feishu:ou_x".into());
        // send_card 的清零等价于直接写 false（方法本身需 HTTP，此处测标记语义）。
        p.conv_states
            .lock()
            .await
            .entry(conv.0.clone())
            .or_default()
            .asks_since_card = false;
        assert!(!p.take_asks_flag(&conv.0).await, "未发询问 → 不下沉");
        p.mark_ask_sent(&conv.0).await;
        assert!(p.take_asks_flag(&conv.0).await, "发过询问 → 下沉");
        assert!(!p.take_asks_flag(&conv.0).await, "取走即清（不重复下沉）");
    }

    /// P8-2：同卡重登记（复用槽换 request_id）不是「取代」——同 msg_id 不走
    /// superseded patch（否则会把刚挂上的新询问顶掉）。guard 提前返回，无 HTTP。
    #[tokio::test]
    async fn record_pending_ask_same_card_not_superseded() {
        let p = FeishuPlatform::new(
            "cli_test".into(),
            "secret_test".into(),
            "https://open.feishu.cn".into(),
            true,
            None,
            300,
            None,
            1800,
            true,
            // v1.21：outbox 未接 store / 发送限速关闭（测试态）；v1.24 加急关。
            None,
            0.0,
            false,
            0,
        )
        .expect("构造");
        p.pending_asks.lock().await.insert(
            "r1".into(),
            PendingAskCard {
                conv_id: "feishu:ou_x".into(),
                msg_id: "m1".into(),
                tool_name: "Bash".into(),
                sender: "ou_owner".into(),
            },
        );
        // 同 request_id + 同卡重登记：不应触发 superseded（否则会真实发 HTTP）。
        p.record_pending_ask("r1", "feishu:ou_x", "m1", "Bash", "ou_owner")
            .await;
        let entry = p.pending_asks.lock().await.get("r1").cloned();
        assert!(
            entry.as_ref().is_some_and(|c| c.msg_id == "m1"),
            "登记保留: {entry:?}"
        );
    }
}
