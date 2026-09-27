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
    parse_comment_event, parse_merged_forward_event, parse_message_event, peek_reply_parent,
    MentionPolicy,
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

/// 云文档**划词**评论 @bot（T13）：comment_id / 发起者 / conv 解析 + 引用片段
/// 注入块在位。字段形态以 fixture 钉住——`quote` 对齐官方评论实体字段名
///（file-comment list API 的「局部评论的引用字段」），事件侧未经真机抓包
/// 确认前以此为假设基线（漂移 → fail-soft 不注入，本断言红即字段变了）。
#[test]
fn replay_comment_docx_quote() {
    let payload = fixture("comment_docx_quote.json");
    let (key, comment_id, msg) = parse_comment_event(&payload, BOT)
        .unwrap_or_else(|| panic!("划词评论事件应被解析（@bot 在 content at 节点）"));
    assert!(!key.is_empty(), "dedup key 存在");
    // 回复目标锚点（drain 登记进锚点表）与轮次发起者。
    assert_eq!(comment_id, "7034fixturecm1");
    assert_eq!(msg.conv_id.0, "feishu:comment:doxcnFixture");
    assert_eq!(msg.sender.0, "ou_fixture_user");
    // 引用片段注入块：块头 → 引用原文 → 用户评论正文。
    let text = msg.text.as_deref().expect("应有正文");
    let blk = text.find("【评论所在文档片段】").expect("引用块在位");
    let quote = text
        .find("本季度 Northwind 区域营收环比下降 12%")
        .expect("被引用的文档原文片段在位");
    let body = text.find("这段为什么下滑").expect("评论正文保留");
    assert!(blk < quote && quote < body, "块头→引用→正文: {text}");
}

/// 云文档**全文**评论 @bot（is_whole、无 quote）：无引用片段可注入——正文
/// 原样通过（fail-soft 不注入，主链路不受影响）。
#[test]
fn replay_comment_docx_whole_no_quote() {
    let payload = fixture("comment_docx_whole.json");
    let (_, comment_id, msg) =
        parse_comment_event(&payload, BOT).unwrap_or_else(|| panic!("全文评论事件应被解析"));
    assert_eq!(comment_id, "7034fixturecm2");
    let text = msg.text.as_deref().unwrap_or_default();
    assert!(
        !text.contains("【评论所在文档片段】"),
        "无 quote 不注入: {text}"
    );
    assert_eq!(text, " 总结一下全文要点", "正文原样: {text}");
}
