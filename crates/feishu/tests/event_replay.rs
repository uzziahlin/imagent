//! v1.26 事件回放测试基建：真实形态的飞书事件 payload（tests/fixtures/*.json）
//! → 解析管线（peek/parse）→ 断言最终路由结论。
//!
//! 动机（v1.25.2 真机教训）：引用上下文上线时「引用回复带 parent_id」的字段
//! 假设未经真实事件验证，静默失效了两周。fixture 回放把字段形态断言前移到
//! CI——新功能依赖的事件字段，先抓一份真实 payload（脱敏）丢进本目录 +
//! 对应断言，假设错了当场红。
//!
//! 维护约定：fixtures 是**真实事件的结构快照**（可改 id/内容脱敏，不动结构）；
//! 每个文件对应本文件一个断言函数；断言写「业务结论」（conv/sender/文本/
//! parent 提取），不写完整 JSON 比对（脆）。

use imagent_feishu::proto::{
    parse_merged_forward_event, parse_message_event, peek_reply_parent, MentionPolicy,
};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("读 fixture {path} 失败：{e}"))
}

const BOT: Option<&str> = Some("ou_fixture_bot");
const POLICY: &MentionPolicy = &MentionPolicy {
    require_mention_in_group: true,
};

/// 群聊引用回复 + @bot：①过 mention 门；②parent_id 提取（引用上下文的数据
/// 源——v1.25.2 修复的字段假设在这里钉死）；③正文 @ 占位剥离。
#[test]
fn replay_quote_reply_group() {
    let payload = fixture("quote_reply_group.json");
    let parsed = parse_message_event(&payload, POLICY, BOT)
        .unwrap_or_else(|| panic!("引用回复事件应被解析（@bot 在 mentions）"));
    let (_, msg, _) = parsed;
    assert_eq!(msg.conv_id.0, "feishu:oc_fixture_group");
    assert_eq!(msg.sender.0, "ou_fixture_user");
    assert!(
        msg.text.as_deref().unwrap_or("").contains("方案"),
        "正文应保留：{:?}",
        msg.text
    );
    assert!(
        !msg.text.as_deref().unwrap_or("").contains("@_user_1"),
        "@bot 占位应剥离：{:?}",
        msg.text
    );
    assert_eq!(
        peek_reply_parent(&payload).as_deref(),
        Some("om_fixture_quoted"),
        "引用上下文数据源：parent_id 必须可提取（v1.25.2 的字段假设）"
    );
}

/// 群聊合并转发 + @bot：专用解析路径命中（message_id 可拉子消息）。
#[test]
fn replay_merged_forward_group() {
    let payload = fixture("merged_forward_group.json");
    let (key, msg, meta) = parse_merged_forward_event(&payload, POLICY, BOT)
        .unwrap_or_else(|| panic!("合并转发事件应走专用解析"));
    assert_eq!(msg.conv_id.0, "feishu:oc_fixture_group");
    assert_eq!(meta.message_id, "om_fixture_mf");
    assert_eq!(meta.title.as_deref(), Some("讨论"));
    assert!(!key.is_empty(), "dedup key 存在");
}

/// 私聊纯文本：无 @ 门，直接过。
#[test]
fn replay_p2p_text() {
    let payload = fixture("p2p_text.json");
    let (_, msg, pending) =
        parse_message_event(&payload, POLICY, BOT).expect("私聊文本无 @ 门应通过");
    assert_eq!(msg.conv_id.0, "feishu:ou_fixture_user");
    assert_eq!(msg.text.as_deref(), Some("帮我看看这个"));
    assert!(pending.is_empty());
    assert!(peek_reply_parent(&payload).is_none(), "非回复形态无 parent");
}
