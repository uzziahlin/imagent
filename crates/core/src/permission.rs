//! IM 权限审批路由（Ask 闭环用）。
//!
//! 主进程侧：`PermissionRouter` 维护每个 conv 的 pending 权限请求（oneshot）。
//! - socket accept task 收到 MCP server 转发的权限请求 → `send_text` 询问用户 →
//!   `register(conv, request_id)` 等待回复；
//! - dispatch recv 循环发现某 conv 有 pending 请求时，把该 conv 的下一条入站消息
//!   当作 approve/deny 回复，`route(conv, …)` 送达 oneshot，**不**走正常 handle。
//!
//! 多 pending 并存（终端 ask_via_im 改造）：key 为 `conv + request_id`，同 conv
//! 下终端 agent 的提问与 IM 会话的审批互不顶替；回复路由三级——按钮回调带
//! request_id 精确匹配 → 引用回复（parent 消息 id 命中询问卡）→ 最新 pending 兜底。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;
use tokio::sync::{oneshot, Mutex};

/// 单 conv 允许的 pending 上限：防泄漏（异常路径漏 cancel 时兜底收敛最旧的）。
const PENDING_PER_CONV_CAP: usize = 8;

/// 固定 socket 路径：`<imagent_home>/permission.sock`（P4-10：随 profile 隔离）。
pub fn default_sock_path() -> Option<PathBuf> {
    Some(crate::paths::imagent_home().join("permission.sock"))
}

/// 审批集条目匹配工具名：精确相等，或条目以 `*` 结尾时按前缀匹配
/// （`mcp__*` 命中所有 MCP 工具）。空格/大小写敏感（工具名本就如此）。
pub fn tool_matches_pattern(pattern: &str, tool_name: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        !prefix.is_empty() && tool_name.starts_with(prefix)
    } else {
        pattern == tool_name
    }
}

/// 该工具是否需要 IM 审批：审批集为空 = 全部过审（既有语义）；非空 = 仅清单内过审。
pub fn needs_approval(approval_tools: &[String], tool_name: &str) -> bool {
    // M4（code-review v8）：无法确定工具名的请求（ACP 无 title 权限请求的
    // 哨兵形态）恒过审——fail-closed，防「清单外放行」语义误放行未知工具。
    if tool_name.starts_with(crate::permission::UNTITLED_TOOL_PREFIX) {
        return true;
    }
    approval_tools.is_empty()
        || approval_tools
            .iter()
            .any(|p| tool_matches_pattern(p, tool_name))
}

/// M4：与 claude 后端的 [`crate::permission::UNTITLED_TOOL_SENTINEL`] 前缀对应
///（core 不依赖 claude crate，前缀字面量双写——两处测试互相锚定）。
pub const UNTITLED_TOOL_PREFIX: &str = "imagent:untitled-tool";

/// 审批决定（词表解析结果；tool 名由 pending 条目在 route 时补齐）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    /// 「always / 始终允许」：本次会话内该工具后续调用跳过审批。
    AllowAlways,
}

/// 文本 → 三态决定（allow/deny/always）。与 [`parse_reply`] 同词表，供需要
/// 区分「始终允许」的调用方使用（parse_reply 以 `always` 标志承载同一信息）。
pub fn parse_decision(text: &str) -> Decision {
    let r = parse_reply(text);
    if r.always {
        Decision::AllowAlways
    } else if r.allow {
        Decision::Allow
    } else {
        Decision::Deny
    }
}

/// 用户的 approve/deny 回复。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionReply {
    pub allow: bool,
    /// 审批记忆（D-记忆）：用户回复「always/始终允许」时为 true——route 侧据此
    /// 把 pending 条目的 tool 加入该 conv 的会话级 allow-set。
    pub always: bool,
    pub message: Option<String>,
    /// 用户回复的**原文**（按钮回调为 `ask:<选项>` 展开、自由文本为原文）。
    /// 权限路径不读它（allow/deny 语义不变）；ask_via_im 路径以它作为用户答案回传。
    pub raw_text: Option<String>,
}

/// 精确 allow 词表（trim + 小写后全字匹配）。P2-G/P2-12：不用「首字符 y/Y」
/// 宽匹配（旧逻辑会把 year/yellow/yesterday 误判 allow，是真实安全 bug）。
const ALLOW_WORDS: &[&str] = &[
    "y",
    "yes",
    "ye",
    "yep",
    "yeah",
    "ok",
    "okay",
    "是",
    "允许",
    "好",
    "好的",
    "可以",
    "行",
    "没问题",
    "好呀",
    "行吧",
    "可以吧",
    "嗯",
];

/// 精确 deny 词表（trim + 小写后全字匹配）。D2：自由文本只有**明确命中**
/// allow/deny 词表（或按钮回调带 ask_req / 引用回复锚定询问卡）才会被消费为
/// 审批决定；其它自由文本回落正常消息路径，不再被兜底当 deny 吞掉。
const DENY_WORDS: &[&str] = &[
    "n",
    "no",
    "nope",
    "nah",
    "不",
    "否",
    "不要",
    "不行",
    "不可以",
    "不许",
    "拒绝",
    "不批",
];

/// 「始终允许」词表（D-记忆：本次会话内该工具后续调用跳过审批）。
/// 全字匹配（trim + 小写），与 allow/deny 词表同口径。
/// L2：pending 淘汰（超上限挤最旧）哨兵——dispatch 的 Replied 分支据此触发
/// 平台侧收敛（等同 TimedOut 路径）。
pub const EVICTED_SENTINEL: &str = "imagent:evicted";

const ALWAYS_WORDS: &[&str] = &["always", "始终允许", "会话内允许"];

/// 文本是否命中「始终允许」词表（parse_reply 据此置 `always` 标志）。
pub fn is_always_word(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    ALWAYS_WORDS.contains(&lower.as_str())
}

/// 自由文本是否**明确命中**审批词表（allow 或 deny 词，全字匹配）。
/// D2：无 reply_to/ask_req 锚定的自由文本，只有命中此词表才可被当审批决定消费。
pub fn is_explicit_reply_word(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    ALLOW_WORDS.contains(&lower.as_str())
        || DENY_WORDS.contains(&lower.as_str())
        || ALWAYS_WORDS.contains(&lower.as_str())
}

/// 解析用户回复文本为 approve/deny。
///
/// 规则（trim 后）：
/// - 空串 / 无法判定 → deny；
/// - 精确匹配 [`ALLOW_WORDS`] 中的 allow 词 → allow；
/// - 其它 → deny（fail-closed）。
///
/// P2-G：不再用「首字符 y/Y」宽匹配（旧逻辑会把 year/yellow/yesterday 误判 allow，
/// 对权限 approve/deny 是真实安全 bug）。P2-12：补中文确认词（用户回复「可以」
/// 「行」「没问题」不再被误 deny）。注意：本函数对**无法解析的文本**返回 deny——
/// 仅用于已被确认要消费的回复；是否消费的判定见 [`is_explicit_reply_word`]（D2）。
pub fn parse_reply(text: &str) -> PermissionReply {
    let t = text.trim();
    if t.is_empty() {
        return PermissionReply {
            allow: false,
            always: false,
            message: Some("empty reply".into()),
            raw_text: None,
        };
    }
    // P6（AskUserQuestion 答案路由）：问题卡的选项按钮回调转成 "ask:<选项>"。
    // 语义 = 不执行内建工具（headless 下它没有交互面），选择经 message 回给
    // agent —— deny + message 是权限协议里 agent 能读到用户输入的唯一通道。
    if let Some(choice) = t.strip_prefix("ask:") {
        let choice = choice.trim();
        if !choice.is_empty() {
            return PermissionReply {
                allow: false,
                always: false,
                message: Some(format!("用户选择：{choice}")),
                raw_text: Some(format!("用户选择：{choice}")),
            };
        }
    }
    let lower = t.to_ascii_lowercase();
    // P2-G：去掉「首字符 y/Y」宽匹配（旧逻辑会把 year/yeah/yellow/yesterday 等
    // 误判为 allow，对权限 approve/deny 是真实安全 bug）。改为精确匹配词表。
    // P2-12：补中文高频确认词（「可以」「行」「没问题」等），降低中文用户误 deny 率。
    let allow = ALLOW_WORDS.contains(&lower.as_str());
    // D-记忆：「always / 始终允许」= 本次允许 + 会话级 allow（tool 名由 route 侧
    // 从 pending 条目补齐——parse_reply 无从得知请求的工具）。
    let always = is_always_word(t);
    PermissionReply {
        allow: allow || always,
        always,
        message: if allow || always {
            None
        } else {
            Some(format!("denied by user reply: {t}"))
        },
        raw_text: Some(t.to_string()),
    }
}

/// 单条 pending 询问的来源（D3）：权限审批（本会话 agent 后端发起，等待期间
/// IM 会话空闲看门狗豁免）vs 终端 `ask_via_im` 提问（超时可到 86400s，**不**豁免
/// 看门狗——否则终端挂着一条未回答的提问就能无限冻结 IM 会话）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    /// 本会话 agent 后端发起的权限审批（permission socket kind=permission）。
    Permission,
    /// 终端 agent 的 `ask_via_im` 提问（permission socket kind=ask）。
    Ask,
}

/// 单条 pending 询问。
struct PendingAsk {
    request_id: String,
    /// 询问卡的 IM 侧消息 id（自由文本引用回复的路由锚点；文本询问为 None）。
    /// D5：register 时可先为 None 占位，发卡成功后 `set_card_msg_id` 回填。
    card_msg_id: Option<String>,
    /// 请求审批的工具名（Permission 来源必带；Ask 来源为 None）——「始终允许」
    /// 回复据此把工具加入该 conv 的会话级 allow-set（D-记忆）。
    tool_name: Option<String>,
    /// 来源（D3 看门狗豁免只认 Permission）。
    kind: PendingKind,
    /// Wave B-11：登记时刻——route 时算 waited_secs（审批响应时长，审计/统计）。
    created_at: Instant,
    tx: oneshot::Sender<PermissionReply>,
}

/// route() 成功投递的决定（request_id + 该次审批的工具名）。
/// D-记忆/审计用：调用方据此落 `permission_decision` 审计（tool/decision/sender）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedDecision {
    pub request_id: String,
    /// 被审批的工具名（Ask 来源 / 未带工具名的 pending 为 None）。
    pub tool_name: Option<String>,
    /// Wave B-11：从 register 到用户回复的等待秒数（审批响应时长）。
    pub waited_secs: u64,
    /// L6（code-review v8）：pending 类别——Ask（终端问答）命中不落审批审计
    ///（防 /stats 的 allow/deny/timeout 占比失真）。
    pub kind: PendingKind,
}

/// per-conv × request_id 权限请求路由表（多 pending 并存）。
pub struct PermissionRouter {
    pending: Mutex<HashMap<String, Vec<PendingAsk>>>,
    /// D-记忆：per-conv 会话级 allow-set（用户回复「always/始终允许」后，该
    /// conv 上此工具的后续审批请求直接放行）。进程内状态——`/stop`、`/new`
    /// 时清空（换任务/新会话不应继承旧授权）。
    session_allows: Mutex<HashMap<String, HashSet<String>>>,
    /// Wave B-2：per-conv 询问登记计数（单调递增，不随 pending 清理回退）。
    /// 轮次起止各取一次快照对比，即可判定「本轮是否发生过审批/询问」——
    /// 完成强提醒的触发条件之一。
    ask_counters: Mutex<HashMap<String, u64>>,
    /// 真机校准（2026-08）：per-conv 最近一次**用户审批决定**（route 命中）的
    /// 时刻——完成强提醒的抑制条件：刚批准过 = 用户显然在线，紧接着的完成
    /// 推送是打扰（实测：3m11s 轮次批准后数十秒完成仍推送）。
    last_decision_at: Mutex<HashMap<String, std::time::Instant>>,
}

impl PermissionRouter {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            session_allows: Mutex::new(HashMap::new()),
            ask_counters: Mutex::new(HashMap::new()),
            last_decision_at: Mutex::new(HashMap::new()),
        }
    }

    /// Wave B-2：该 conv 的累计询问登记数（审批 + ask_via_im 提问；单调递增）。
    /// 轮次起止快照对比判定「本轮发生过询问」。
    /// 距最近一次用户审批决定的秒数（无记录 = None）。完成强提醒抑制条件：
    /// 60s 内有过决定 → 用户在线，跳过推送。
    pub async fn secs_since_decision(&self, conv_id: &str) -> Option<u64> {
        self.last_decision_at
            .lock()
            .await
            .get(conv_id)
            .map(|t| t.elapsed().as_secs())
    }

    pub async fn ask_count(&self, conv_id: &str) -> u64 {
        self.ask_counters
            .lock()
            .await
            .get(conv_id)
            .copied()
            .unwrap_or(0)
    }

    /// D-记忆：把工具加入该 conv 的会话级 allow-set（「始终允许」回复落地）。
    pub async fn allow_always(&self, conv_id: &str, tool_name: &str) {
        self.session_allows
            .lock()
            .await
            .entry(conv_id.to_string())
            .or_default()
            .insert(tool_name.to_string());
    }

    /// D-记忆：该 conv 上此工具是否已被「始终允许」（审批前置检查——命中即
    /// 跳过 IM 审批，直接放行）。
    pub async fn is_session_allowed(&self, conv_id: &str, tool_name: &str) -> bool {
        self.session_allows
            .lock()
            .await
            .get(conv_id)
            .is_some_and(|s| s.contains(tool_name))
    }

    /// D-记忆：清空该 conv 的会话级 allow-set（/stop、/new）。
    pub async fn clear_session_allows(&self, conv_id: &str) {
        self.session_allows.lock().await.remove(conv_id);
    }

    /// 是否有 conv 处于等待回复状态。
    pub async fn has_pending(&self, conv_id: &str) -> bool {
        self.pending
            .lock()
            .await
            .get(conv_id)
            .is_some_and(|v| !v.is_empty())
    }

    /// D3：是否该 conv 有**指定来源**的 pending（空闲看门狗豁免只认 Permission——
    /// 终端 ask_via_im 的 pending 超时可到 86400s，不得无限豁免 IM 会话看门狗）。
    pub async fn has_pending_of_kind(&self, conv_id: &str, kind: PendingKind) -> bool {
        self.pending
            .lock()
            .await
            .get(conv_id)
            .is_some_and(|v| v.iter().any(|p| p.kind == kind))
    }

    /// D2：该 conv 的 pending 条数（多 pending 且无 reply 锚定时拒绝兜底消费）。
    pub async fn pending_count(&self, conv_id: &str) -> usize {
        self.pending
            .lock()
            .await
            .get(conv_id)
            .map_or(0, |v| v.len())
    }

    /// D5：回填询问卡消息 id（register 先占位、发卡成功后锚定引用回复路由）。
    /// 返回是否命中仍在等待的 pending（已被消费/取消则 false，无害）。
    pub async fn set_card_msg_id(
        &self,
        conv_id: &str,
        request_id: &str,
        card_msg_id: Option<String>,
    ) -> bool {
        let mut map = self.pending.lock().await;
        let Some(list) = map.get_mut(conv_id) else {
            return false;
        };
        let mut hit = false;
        for p in list.iter_mut() {
            if p.request_id == request_id {
                p.card_msg_id = card_msg_id.clone();
                hit = true;
            }
        }
        hit
    }

    /// 注册一个 pending 请求，返回 receiver 用于等待回复。
    ///
    /// 同 request_id 重复注册会顶替旧条目（旧等待者立即收到 superseded deny）；
    /// 不同 request_id 并存（终端 ask 与 IM 审批互不干扰）。per-conv 超过上限时
    /// 最旧的按超时收敛（异常路径漏 cancel 的兜底）。D5：`card_msg_id` 可先为
    /// None 占位（发卡前注册，防极速按钮点击在注册前到达而落空），发卡成功后
    /// [`set_card_msg_id`](Self::set_card_msg_id) 回填。
    pub async fn register(
        &self,
        conv_id: &str,
        request_id: &str,
        card_msg_id: Option<String>,
        kind: PendingKind,
        tool_name: Option<&str>,
    ) -> oneshot::Receiver<PermissionReply> {
        let (tx, rx) = oneshot::channel();
        let entry = PendingAsk {
            request_id: request_id.to_string(),
            card_msg_id,
            tool_name: tool_name.filter(|s| !s.is_empty()).map(|s| s.to_string()),
            kind,
            created_at: Instant::now(),
            tx,
        };
        let mut map = self.pending.lock().await;
        // Wave B-2：询问计数（轮次 delta 判定用；与 pending 生命周期解耦，单调）。
        *self
            .ask_counters
            .lock()
            .await
            .entry(conv_id.to_string())
            .or_insert(0) += 1;
        let list = map.entry(conv_id.to_string()).or_default();
        if let Some(i) = list.iter().position(|p| p.request_id == request_id) {
            let old = list.remove(i);
            let _ = old.tx.send(PermissionReply {
                allow: false,
                always: false,
                message: Some("superseded（同一请求被重新发起）".into()),
                raw_text: None,
            });
        }
        list.push(entry);
        while list.len() > PENDING_PER_CONV_CAP {
            let oldest = list.remove(0);
            // L2（code-review v8）：淘汰携带哨兵 raw_text——Replied 分支据此走与
            // TimedOut 同款的平台收敛（撤卡），防残留 pending 卡点「允许」→
            // route miss → 字面 "y" 被当 prompt 跑一轮 agent。
            let _ = oldest.tx.send(PermissionReply {
                allow: false,
                always: false,
                message: Some("cancelled（pending 超上限，最旧询问被收敛）".into()),
                raw_text: Some(EVICTED_SENTINEL.to_string()),
            });
        }
        rx
    }

    /// 投递回复给 pending 请求，三级路由：
    /// 1. `req_hint`（按钮回调携带的 request_id）精确匹配；
    /// 2. `parent_msg_id`（自由文本引用回复的目标消息 id）命中询问卡；
    /// 3. 两者皆缺时最新 pending 兜底。
    ///
    /// req/parent **给了但未命中**视为未命中（陈旧回调/无关引用不得劫持别的
    /// pending，消息回落正常处理路径）。
    pub async fn route(
        &self,
        conv_id: &str,
        req_hint: Option<&str>,
        parent_msg_id: Option<&str>,
        reply: PermissionReply,
    ) -> Option<RoutedDecision> {
        let mut map = self.pending.lock().await;
        let list = map.get_mut(conv_id)?;
        let idx = match (req_hint, parent_msg_id) {
            (Some(req), _) => list.iter().position(|p| p.request_id == req)?,
            // 真机校准（2026-08）：引用的若不是询问卡（如对 ⏰ 催办文本回 OK），
            // 锚点不匹配不能让回复直接失效——**单 pending 无歧义**时兜底该条；
            // 多 pending 并存仍返回 None（锚定失败无法消解歧义，走提示引导）。
            (None, Some(mid)) => match list
                .iter()
                .position(|p| p.card_msg_id.as_deref() == Some(mid))
            {
                Some(i) => i,
                None if list.len() == 1 => 0,
                None => return None,
            },
            (None, None) => list.len().checked_sub(1)?,
        };
        let hit = list.remove(idx);
        if list.is_empty() {
            map.remove(conv_id);
        }
        // D-记忆：「始终允许」+ Permission 来源且带工具名 → 落入该 conv 的会话级
        // allow-set（后续同工具审批直接放行）。锁 pending 期间再锁 session_allows
        //（两锁无反向获取顺序，无死锁面）。
        if reply.always && hit.kind == PendingKind::Permission {
            if let Some(tool) = hit.tool_name.clone() {
                self.session_allows
                    .lock()
                    .await
                    .entry(conv_id.to_string())
                    .or_default()
                    .insert(tool);
            }
        }
        // send 失败说明 receiver 已 drop（register 方未在等），视为未命中。
        // Wave B-11：created_at 差值即审批响应时长（waited_secs，审计/统计用）。
        let waited_secs = hit.created_at.elapsed().as_secs();
        // 真机校准（2026-08）：用户真实决定时刻——完成强提醒抑制用。
        drop(map);
        self.last_decision_at
            .lock()
            .await
            .insert(conv_id.to_string(), std::time::Instant::now());
        hit.tx.send(reply).ok().map(|_| RoutedDecision {
            request_id: hit.request_id,
            tool_name: hit.tool_name,
            waited_secs,
            kind: hit.kind,
        })
    }

    /// 清理单个 pending（超时 / router-drop 路径）：投递 deny（fail-closed）唤醒
    /// 等待者。send 失败 = receiver 已 drop（等待方先超时），无害。
    pub async fn cancel(&self, conv_id: &str, request_id: &str) {
        let mut map = self.pending.lock().await;
        let Some(list) = map.get_mut(conv_id) else {
            return;
        };
        if let Some(i) = list.iter().position(|p| p.request_id == request_id) {
            let old = list.remove(i);
            if list.is_empty() {
                map.remove(conv_id);
            }
            drop(map);
            let _ = old.tx.send(PermissionReply {
                allow: false,
                always: false,
                message: Some("cancelled（任务被 /stop 中断或审批超时）".into()),
                raw_text: None,
            });
        }
    }

    /// 清理该 conv 的**全部** pending（/stop 路径）：逐个投递 deny 唤醒等待者，
    /// 返回被清理的 request_id 列表（调用方据此收敛询问卡）。
    pub async fn cancel_all(&self, conv_id: &str) -> Vec<String> {
        let removed = self
            .pending
            .lock()
            .await
            .remove(conv_id)
            .unwrap_or_default();
        removed
            .into_iter()
            .map(|p| {
                let _ = p.tx.send(PermissionReply {
                    allow: false,
                    always: false,
                    message: Some("cancelled（任务被 /stop 中断或审批超时）".into()),
                    raw_text: None,
                });
                p.request_id
            })
            .collect()
    }
}

impl Default for PermissionRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_removes_pending() {
        // P1-8：cancel 清理 pending，避免超时/router-drop 残留累积。
        let r = PermissionRouter::new();
        let _rx = r
            .register("conv1", "req1", None, PendingKind::Permission, None)
            .await;
        assert!(r.has_pending("conv1").await);
        r.cancel("conv1", "req1").await;
        assert!(!r.has_pending("conv1").await);
    }

    /// P5-16：cancel 唤醒等待者并 fail-closed 回 deny——不再挂满
    /// permission_ask_timeout 才超时。
    #[tokio::test]
    async fn cancel_waits_no_more_denies_waiter() {
        let r = PermissionRouter::new();
        let rx = r
            .register("conv1", "req1", None, PendingKind::Permission, None)
            .await;
        r.cancel("conv1", "req1").await;
        // 等待者应立即（而非超时后）收到 deny。
        let reply = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("cancel 应立即唤醒等待者")
            .expect("sender 未 drop");
        assert!(!reply.allow, "cancel 必须 fail-closed deny");
        assert!(reply.message.unwrap().contains("cancelled"));
    }

    /// 真机校准（2026-08）：引用回复锚点不匹配（用户对 ⏰ 催办**文本**回 OK，
    /// 而非对询问卡回复）——单 pending 无歧义时兜底命中，不再让回复失效；
    /// 多 pending 时仍 None（无法消解歧义）。
    #[tokio::test]
    async fn route_unmatched_anchor_falls_back_when_single_pending() {
        let r = PermissionRouter::new();
        let rx = r
            .register("c", "r-1", None, PendingKind::Permission, Some("Bash"))
            .await;
        // 锚定一个非询问卡的消息 id。
        let hit = r
            .route(
                "c",
                None,
                Some("om_not_a_card"),
                PermissionReply {
                    allow: true,
                    always: false,
                    message: None,
                    raw_text: Some("OK".into()),
                },
            )
            .await;
        assert_eq!(
            hit.as_ref().map(|d| d.request_id.as_str()),
            Some("r-1"),
            "单 pending 锚不匹配兜底命中"
        );
        assert!(rx.await.unwrap().allow);

        // 多 pending + 锚不匹配 → None（歧义不消费）。
        let _rx2 = r
            .register("c", "r-2", None, PendingKind::Permission, Some("Bash"))
            .await;
        let _rx3 = r
            .register("c", "r-3", None, PendingKind::Permission, Some("Read"))
            .await;
        assert!(
            r.route(
                "c",
                None,
                Some("om_not_a_card"),
                PermissionReply {
                    allow: true,
                    always: false,
                    message: None,
                    raw_text: None
                }
            )
            .await
            .is_none(),
            "多 pending 锚不匹配不消费"
        );
    }

    /// 多 pending 并存：同 conv 不同 request_id 互不顶替，按 req 精确路由。
    #[tokio::test]
    async fn multi_pending_routes_by_request_id() {
        let r = PermissionRouter::new();
        let rx_im = r
            .register("c", "im-1", None, PendingKind::Permission, None)
            .await;
        let rx_term = r
            .register("c", "t-1", None, PendingKind::Permission, None)
            .await;
        // 按钮/回调带 req=t-1 → 只唤醒终端一路。
        let hit = r
            .route(
                "c",
                Some("t-1"),
                None,
                PermissionReply {
                    allow: false,
                    always: false,
                    message: None,
                    raw_text: Some("用户选择：B".into()),
                },
            )
            .await;
        assert_eq!(hit.as_ref().map(|d| d.request_id.as_str()), Some("t-1"));
        let term = tokio::time::timeout(std::time::Duration::from_secs(1), rx_term)
            .await
            .expect("t-1 应被唤醒")
            .unwrap();
        assert_eq!(term.raw_text.as_deref(), Some("用户选择：B"));
        // IM 那路仍在等待，且成为唯一 pending（后续兜底路由命中它）。
        assert!(r.has_pending("c").await);
        let hit2 = r
            .route(
                "c",
                None,
                None,
                PermissionReply {
                    allow: true,
                    always: false,
                    message: None,
                    raw_text: Some("y".into()),
                },
            )
            .await;
        assert_eq!(hit2.as_ref().map(|d| d.request_id.as_str()), Some("im-1"));
        assert!(rx_im.await.unwrap().allow);
        assert!(!r.has_pending("c").await);
    }

    /// 引用回复：parent 消息 id 命中对应询问卡（card_msg_id 锚点）。
    #[tokio::test]
    async fn parent_msg_id_routes_to_matching_card() {
        let r = PermissionRouter::new();
        let _old = r
            .register(
                "c",
                "im-1",
                Some("om_old".to_string()),
                PendingKind::Permission,
                None,
            )
            .await;
        let _rx_new = r
            .register(
                "c",
                "t-1",
                Some("om_new".to_string()),
                PendingKind::Permission,
                None,
            )
            .await;
        let hit = r
            .route(
                "c",
                None,
                Some("om_old"),
                PermissionReply {
                    allow: true,
                    always: false,
                    message: None,
                    raw_text: Some("y".into()),
                },
            )
            .await;
        assert_eq!(
            hit.as_ref().map(|d| d.request_id.as_str()),
            Some("im-1"),
            "引用旧卡应路由 im-1 而非最新"
        );
        assert!(r.has_pending("c").await, "t-1 不应被消费");
    }

    /// 同 request_id 重复注册：旧的被顶替（superseded deny），不占两个槽位。
    #[tokio::test]
    async fn reregister_same_request_id_supersedes() {
        let r = PermissionRouter::new();
        let rx_old = r
            .register("c", "req1", None, PendingKind::Permission, None)
            .await;
        let _rx_new = r
            .register("c", "req1", None, PendingKind::Permission, None)
            .await;
        let old = tokio::time::timeout(std::time::Duration::from_secs(1), rx_old)
            .await
            .expect("旧等待者应立即被唤醒")
            .unwrap();
        assert!(!old.allow);
        assert!(old.message.unwrap().contains("superseded"));
        // 只剩一个 pending（t-1 未被顶掉）。
        assert!(r.has_pending("c").await);
    }

    /// /stop：cancel_all 清理全部并唤醒所有等待者。
    #[tokio::test]
    async fn cancel_all_wakes_every_waiter() {
        let r = PermissionRouter::new();
        let rx1 = r
            .register("c", "a", None, PendingKind::Permission, None)
            .await;
        let rx2 = r
            .register("c", "b", None, PendingKind::Permission, None)
            .await;
        let ids = r.cancel_all("c").await;
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
        for rx in [rx1, rx2] {
            let reply = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
                .await
                .expect("应立即唤醒")
                .unwrap();
            assert!(!reply.allow);
        }
        assert!(!r.has_pending("c").await);
    }

    #[test]
    fn parse_reply_allow_variants() {
        for s in [
            "y",
            "Y",
            "yes",
            "YES",
            "Yes",
            "ok",
            "OK",
            "是",
            "允许",
            "好",
            "好的",
            "可以",
            "行",
            "没问题",
            "好呀",
            "行吧",
            "可以吧",
            "嗯",
        ] {
            let r = parse_reply(s);
            assert!(r.allow, "should allow: {s:?}");
            assert!(r.message.is_none(), "no message when allow: {s:?}");
        }
    }

    /// P6：ask: 前缀 = 问题卡选项答案 → deny + message（agent 经 message 读到选择）。
    #[test]
    fn parse_reply_ask_prefix_carries_choice() {
        let r = parse_reply("ask:先做数据库迁移");
        assert!(!r.allow);
        assert_eq!(r.message.as_deref(), Some("用户选择：先做数据库迁移"));
        // 空 ask: 不当答案（回落正常 deny 路径）。
        let r2 = parse_reply("ask:");
        assert!(r2.message.is_none() || r2.message.as_deref() != Some("用户选择："));
    }

    #[test]
    fn parse_reply_deny_variants() {
        for s in ["", "   ", "n", "N", "no", "不", "拒绝", "随便", "rm -rf /"] {
            let r = parse_reply(s);
            assert!(!r.allow, "should deny: {s:?}");
        }
    }

    /// D-记忆：「always / 始终允许」词表——allow + always 双标志。
    #[test]
    fn parse_reply_always_variants() {
        for s in ["always", "ALWAYS", "Always", "始终允许", "会话内允许"] {
            let r = parse_reply(s);
            assert!(r.allow, "always 应同时 allow: {s:?}");
            assert!(r.always, "应带 always 标志: {s:?}");
            assert!(r.message.is_none(), "allow 类回复不带 message: {s:?}");
            assert_eq!(parse_decision(s), Decision::AllowAlways, "{s:?}");
        }
        // 近似词不得误命中（fail-closed：落到 deny）。
        for s in ["alway", "always?", "始终", "允许 always"] {
            let r = parse_reply(s);
            assert!(!r.always, "近似词不应命中 always: {s:?}");
        }
        // 普通 allow 词不带 always 标志。
        let r = parse_reply("y");
        assert!(r.allow && !r.always);
        assert_eq!(parse_decision("y"), Decision::Allow);
        assert_eq!(parse_decision("n"), Decision::Deny);
    }

    /// D-记忆：always 词命中 is_explicit_reply_word（自由文本可被消费为审批决定）。
    #[test]
    fn explicit_reply_word_includes_always() {
        assert!(is_explicit_reply_word("always"));
        assert!(is_explicit_reply_word("始终允许"));
        assert!(!is_explicit_reply_word("alway"));
    }

    /// D-记忆：AllowAlways 回复命中带工具名的 Permission pending → 工具进入该
    /// conv 的会话级 allow-set；Ask 来源不进。
    #[tokio::test]
    async fn route_always_populates_session_allow_set() {
        let r = PermissionRouter::new();
        let _rx = r
            .register("c", "p-1", None, PendingKind::Permission, Some("Bash"))
            .await;
        assert!(!r.is_session_allowed("c", "Bash").await);
        let hit = r
            .route(
                "c",
                Some("p-1"),
                None,
                PermissionReply {
                    allow: true,
                    always: true,
                    message: None,
                    raw_text: Some("always".into()),
                },
            )
            .await
            .expect("应命中 p-1");
        assert_eq!(hit.tool_name.as_deref(), Some("Bash"));
        assert!(
            r.is_session_allowed("c", "Bash").await,
            "Bash 应进入 allow-set"
        );
        assert!(
            !r.is_session_allowed("c", "Write").await,
            "其它工具不受影响"
        );
        assert!(
            !r.is_session_allowed("other", "Bash").await,
            "其它 conv 不受影响"
        );
        // Ask 来源的 always 不落 allow-set（提问无工具语义）。
        let _rx_ask = r
            .register("c", "a-1", None, PendingKind::Ask, Some("Bash"))
            .await;
        let _ = r
            .route(
                "c",
                Some("a-1"),
                None,
                PermissionReply {
                    allow: true,
                    always: true,
                    message: None,
                    raw_text: None,
                },
            )
            .await;
        // Bash 已在（来自 p-1），验证 Ask 不新增：换一个工具名观察。
        let _rx_ask2 = r
            .register("c", "a-2", None, PendingKind::Ask, Some("WebFetch"))
            .await;
        let _ = r
            .route(
                "c",
                Some("a-2"),
                None,
                PermissionReply {
                    allow: true,
                    always: true,
                    message: None,
                    raw_text: None,
                },
            )
            .await;
        assert!(
            !r.is_session_allowed("c", "WebFetch").await,
            "Ask 来源的 always 不落 allow-set"
        );
    }

    /// D-记忆：clear_session_allows 清空（/stop、/new 清理点语义）。
    #[tokio::test]
    async fn clear_session_allows_drops_entries() {
        let r = PermissionRouter::new();
        r.allow_always("c", "Bash").await;
        r.allow_always("c", "Write").await;
        assert!(r.is_session_allowed("c", "Bash").await);
        r.clear_session_allows("c").await;
        assert!(!r.is_session_allowed("c", "Bash").await);
        assert!(!r.is_session_allowed("c", "Write").await);
    }

    #[test]
    fn parse_reply_year_not_allowed() {
        // P2-G：首字符 y 但非 allow 词必须 deny（旧「首字符 y/Y」宽匹配会误 allow，
        // 对权限 approve/deny 是真实安全 bug）。
        for s in ["year", "yellow", "yesterday", "yeah no", "y?", "y3"] {
            let r = parse_reply(s);
            assert!(!r.allow, "应 deny（首字符 y 但非 allow 词）: {s:?}");
        }
    }

    #[test]
    fn parse_reply_deny_has_message() {
        let r = parse_reply("no way");
        assert!(!r.allow);
        assert!(r.message.unwrap().contains("no way"));
    }

    #[tokio::test]
    async fn router_register_route_hit() {
        let r = PermissionRouter::new();
        assert!(!r.has_pending("c1").await);
        let rx = r
            .register("c1", "req1", None, PendingKind::Permission, None)
            .await;
        assert!(r.has_pending("c1").await);
        let hit = r
            .route(
                "c1",
                Some("req1"),
                None,
                PermissionReply {
                    allow: true,
                    always: false,
                    message: None,
                    raw_text: None,
                },
            )
            .await;
        assert!(hit.is_some());
        assert!(!r.has_pending("c1").await);
        let reply = rx.await.unwrap();
        assert!(reply.allow);
    }

    #[tokio::test]
    async fn router_route_miss_when_no_pending() {
        let r = PermissionRouter::new();
        let hit = r
            .route(
                "c2",
                None,
                None,
                PermissionReply {
                    allow: false,
                    always: false,
                    message: None,
                    raw_text: None,
                },
            )
            .await;
        assert!(hit.is_none());
    }

    /// D3：pending 来源区分——has_pending_of_kind 只认对应 kind。
    #[tokio::test]
    async fn pending_kind_distinguishes_permission_and_ask() {
        let r = PermissionRouter::new();
        let _rx_perm = r
            .register("c", "p-1", None, PendingKind::Permission, None)
            .await;
        assert!(r.has_pending_of_kind("c", PendingKind::Permission).await);
        assert!(!r.has_pending_of_kind("c", PendingKind::Ask).await);
        let _rx_ask = r.register("c", "a-1", None, PendingKind::Ask, None).await;
        assert!(r.has_pending_of_kind("c", PendingKind::Ask).await);
        // pending_count 反映并存条数（D2 歧义判定用）。
        assert_eq!(r.pending_count("c").await, 2);
        r.cancel("c", "p-1").await;
        assert!(!r.has_pending_of_kind("c", PendingKind::Permission).await);
        assert!(r.has_pending_of_kind("c", PendingKind::Ask).await);
    }

    /// D5：register 先占位（card_msg_id=None），发卡成功后 set_card_msg_id 回填，
    /// 引用回复按回填后的锚点路由。
    #[tokio::test]
    async fn set_card_msg_id_backfills_placeholder() {
        let r = PermissionRouter::new();
        let _rx = r
            .register("c", "req1", None, PendingKind::Permission, None)
            .await;
        assert!(
            r.set_card_msg_id("c", "req1", Some("om_1".to_string()))
                .await
        );
        let hit = r
            .route(
                "c",
                None,
                Some("om_1"),
                PermissionReply {
                    allow: true,
                    always: false,
                    message: None,
                    raw_text: None,
                },
            )
            .await;
        assert_eq!(
            hit.as_ref().map(|d| d.request_id.as_str()),
            Some("req1"),
            "回填后应按卡片锚点路由"
        );
        // 已被消费后再回填 → 不命中（无害）。
        assert!(
            !r.set_card_msg_id("c", "req1", Some("om_2".to_string()))
                .await
        );
    }

    /// D2：is_explicit_reply_word 只认精确 allow/deny 词，自由文本不命中。
    #[test]
    fn explicit_reply_word_vocabulary() {
        for s in ["y", "Y", "yes", "ok", "是", "可以", "没问题"] {
            assert!(is_explicit_reply_word(s), "应命中 allow 词: {s:?}");
        }
        for s in ["n", "N", "no", "不", "不要", "拒绝", "不行"] {
            assert!(is_explicit_reply_word(s), "应命中 deny 词: {s:?}");
        }
        // 自由文本（含问句、year 类 y 开头词）不得命中——D2 不消费为审批决定。
        for s in ["year", "帮我看下这个报错", "y?", "嗯看一下", "", "  "] {
            assert!(!is_explicit_reply_word(s), "自由文本不应命中: {s:?}");
        }
    }

    /// Wave B-11：route 返回 waited_secs（register → 回复的等待时长）。
    #[tokio::test]
    async fn route_reports_waited_secs() {
        let r = PermissionRouter::new();
        let rx = r
            .register("c", "req1", None, PendingKind::Permission, Some("Bash"))
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let hit = r
            .route(
                "c",
                Some("req1"),
                None,
                PermissionReply {
                    allow: true,
                    always: false,
                    message: None,
                    raw_text: Some("y".into()),
                },
            )
            .await
            .expect("应命中");
        assert_eq!(hit.tool_name.as_deref(), Some("Bash"));
        assert!(
            hit.waited_secs <= 1,
            "30ms 等待应折算为 0..=1 秒: {}",
            hit.waited_secs
        );
        // 稍等后再 route 第二条，waited_secs 至少 1 秒（秒粒度）。
        let rx2 = r
            .register("c", "req2", None, PendingKind::Permission, None)
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let hit2 = r
            .route(
                "c",
                Some("req2"),
                None,
                PermissionReply {
                    allow: false,
                    always: false,
                    message: None,
                    raw_text: None,
                },
            )
            .await
            .expect("应命中");
        assert!(
            hit2.waited_secs >= 1,
            "1.1s 等待应 ≥1 秒: {}",
            hit2.waited_secs
        );
        drop(rx);
        drop(rx2);
    }

    /// Wave B-2：ask_count 随 register 单调递增（route/cancel 不回退）——轮次
    /// 起止快照对比即可判定「本轮发生过询问」。
    #[tokio::test]
    async fn ask_count_monotonic_per_conv() {
        let r = PermissionRouter::new();
        assert_eq!(r.ask_count("c").await, 0);
        let _rx1 = r
            .register("c", "a", None, PendingKind::Permission, None)
            .await;
        assert_eq!(r.ask_count("c").await, 1);
        r.cancel("c", "a").await;
        assert_eq!(
            r.ask_count("c").await,
            1,
            "cancel 清 pending 但计数不回退（单调）"
        );
        let _rx2 = r.register("c", "b", None, PendingKind::Ask, None).await;
        assert_eq!(r.ask_count("c").await, 2);
        // 其它 conv 不受影响。
        assert_eq!(r.ask_count("other").await, 0);
        let _ = r
            .route(
                "c",
                Some("b"),
                None,
                PermissionReply {
                    allow: false,
                    always: false,
                    message: None,
                    raw_text: None,
                },
            )
            .await;
        assert_eq!(r.ask_count("c").await, 2, "route 不回退计数");
    }
}

#[cfg(test)]
mod approval_set_tests {
    use super::*;

    #[test]
    fn pattern_matching() {
        assert!(tool_matches_pattern("Bash", "Bash"));
        assert!(!tool_matches_pattern("Bash", "BashOutput"));
        assert!(tool_matches_pattern(
            "mcp__*",
            "mcp__imagent__permission_request"
        ));
        assert!(!tool_matches_pattern("mcp__*", "Bash"));
        // 裸 "*" 不视为全匹配（防误配成「什么都不审」）；空条目同理。
        assert!(!tool_matches_pattern("*", "Bash"));
        assert!(!tool_matches_pattern("", "Bash"));
    }

    #[test]
    fn needs_approval_semantics() {
        // 空集 = 全部过审（既有语义）。
        assert!(needs_approval(&[], "Bash"));
        let set = vec!["Bash".to_string(), "mcp__*".to_string()];
        assert!(needs_approval(&set, "Bash"));
        assert!(needs_approval(&set, "mcp__x__y"));
        // 集外 = 放行。
        assert!(!needs_approval(&set, "Write"));
        assert!(!needs_approval(&set, "WebFetch"));
    }
}
