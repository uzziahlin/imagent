//! fuzz: 飞书事件 payload 解析（任意 JSON 输入不 panic）。
//!
//! 真实外部输入攻击面：WS 长连接推来的事件 payload + 引用上下文拉取 API 返回的
//! `(msg_type, content)`。v13-P3 审查项「fuzz 只盖 4 个解析函数、drain 链其余
//! 裸奔」——本 target 把 drain 循环（crates/feishu/src/platform/drain.rs）对
//! payload 调用的**全部**纯解析/谓词函数收进来，同一 payload 逐个喂，任一路径
//! panic 即 crash：
//! - 消息类（im.message.receive_v1）：parse_message_event（P6-1 起带
//!   MentionPolicy × bot_open_id 有无，覆盖 @bot 过滤与占位剥离路径）/
//!   parse_merged_forward_event（合并转发，同策略矩阵；meta 字段访问覆盖
//!   转录头派生路径）/ unsupported_message_notice（不支持类型提示）/
//!   peek_reply_parent + peek_group_reply_parent（引用上下文与「回复即定向」
//!   预检）/ thread_key_of_payload（话题免 @ conv 键提取）；
//! - 事件类：parse_card_action_event（审批按钮回调）/ is_comment_event +
//!   parse_comment_event（云文档评论，bot id 两态）/ parse_reaction_event
//!   （表情回应快速审批）/ parse_menu_event（菜单跳转）/ parse_recall_event
//!   （撤回控制消息）/ parse_bot_removed_event + parse_bot_added_event
//!   （bot 进出群）；
//! - 引用正文转录（drain 的 enrich_with_quote 对 API 返回值同样按不可信输入
//!   处理）：quoted_context_text（msg_type × content）与 card_text_transcript
//!   （interactive 卡递归文本收集——v13 点名的递归体）。content 取 fuzz 字节
//!   的 lossy UTF-8 形态，msg_type 打 quoted_context_text 认的全部类型。
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    use imagent_feishu::proto;
    // 策略 × bot id 两态：4 组合全打（require_mention 强/弱过滤 + 剥离路径）。
    for policy in [proto::MentionPolicy::PERMISSIVE, proto::MentionPolicy::REQUIRE_BOT] {
        for bot in [None, Some("ou_fuzz_bot")] {
            if let Some((_key, msg, pending)) = proto::parse_message_event(data, &policy, bot) {
                // 解析产物再过一遍字段访问（构造 key、媒体键提取等派生路径）。
                let _ = msg.text;
                let _ = msg.media.len() + pending.len() + msg.mentions.len();
            }
            if let Some((_key, mf, meta)) = proto::parse_merged_forward_event(data, &policy, bot) {
                // meta 字段访问覆盖 drain 侧转录头（fetch 入参 + title/summary）。
                let _ = (&mf.text, &meta.message_id, &meta.title, &meta.summary);
            }
        }
    }
    if let Some((_key, msg, deny)) = proto::parse_card_action_event(data) {
        // deny（安全批次第三元素：过期/他人触发形态）也过一遍派生访问。
        let _ = (msg.text, deny.is_some());
    }
    if proto::is_comment_event(data) {
        let _ = proto::parse_comment_event(data, None);
        let _ = proto::parse_comment_event(data, Some("ou_fuzz_bot"));
    }
    let _ = proto::is_group_message_event(data);
    // 事件类（可选订阅）：表情回应审批 / 菜单 / 撤回 / bot 进出群。
    let _ = proto::parse_reaction_event(data);
    let _ = proto::parse_menu_event(data);
    let _ = proto::parse_recall_event(data);
    let _ = proto::parse_bot_removed_event(data);
    let _ = proto::parse_bot_added_event(data);
    // 消息事件的廉价谓词/预检路径（T19 后 drain 走 Value 导航，字节版保留作
    // 等价性钉子，仍属攻击面）。
    let _ = proto::peek_reply_parent(data);
    let _ = proto::peek_group_reply_parent(data);
    let _ = proto::thread_key_of_payload(data);
    let _ = proto::unsupported_message_notice(data);
    // 引用正文转录：enrich_with_quote 以 (msg_type, content) 为入参——content
    // 攻击面 = 任意字符串（API 返回的 message content），此处取 fuzz 字节的
    // lossy UTF-8；msg_type 打 quoted_context_text 认的全部类型（其余类型为
    // 恒 None 的兜底臂，不值得枚举）。
    let content = String::from_utf8_lossy(data);
    for mt in ["text", "post"] {
        let _ = proto::quoted_context_text(mt, &content);
    }
    // interactive 卡文本转录：递归下钻任意 JSON 结构（collect_card_text）。
    let _ = proto::card_text_transcript(&content);
});
