//! fuzz: 飞书事件 payload 解析（任意 JSON 输入不 panic）。
//!
//! 真实外部输入攻击面：WS 长连接推来的三类事件 payload——
//! `im.message.receive_v1`（消息，含媒体）、`card.action.trigger`（审批按钮）、
//! `drive.file.comment.created_v1`（云文档评论）。drain task 对每个 payload 依次
//! 过 parse_message_event（P6-1 起带 MentionPolicy × bot_open_id 有无，覆盖 @bot
//! 过滤与占位剥离路径）/ parse_card_action_event / is_comment_event +
//! parse_comment_event，全部必须不 panic。
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
        }
    }
    if let Some((_key, msg)) = proto::parse_card_action_event(data) {
        let _ = msg.text;
    }
    if proto::is_comment_event(data) {
        let _ = proto::parse_comment_event(data, None);
        let _ = proto::parse_comment_event(data, Some("ou_fuzz_bot"));
    }
    let _ = proto::is_group_message_event(data);
});
