# imagent fuzz

cargo-fuzz 模糊测试（CODE_REVIEW_v2 E-7）。覆盖 ilink proto 帧解析 + 媒体 SSRF host 校验 + feishu 事件解析 + wecom WS 帧解析的鲁棒性（任意输入不 panic）。

## 前置

需 nightly 工具链 + cargo-fuzz：

```bash
rustup toolchain install nightly
cargo +nightly install cargo-fuzz
```

## 运行

```bash
cd fuzz
cargo +nightly fuzz run ilink_proto_parse
cargo +nightly fuzz run ilink_media_cdn_host
cargo +nightly fuzz run feishu_event_parse
cargo +nightly fuzz run wecom_frame_parse
```

corpus / crash artifacts 存于 `fuzz/<target>/`（已 gitignore）。

## target

| target | 覆盖 |
|---|---|
| `ilink_proto_parse` | `proto::parse_frame` 帧解析（任意字节） |
| `ilink_media_cdn_host` | `media::assert_cdn_host` SSRF host 校验（任意 URL） |
| `feishu_event_parse` | 飞书 drain 链**全部**纯解析/谓词函数（v13-P3 补齐）：消息（`parse_message_event` / `parse_merged_forward_event`，MentionPolicy × bot 两态 / `unsupported_message_notice` / `peek_reply_parent` / `peek_group_reply_parent` / `thread_key_of_payload`）+ 事件（卡片回调 / 云文档评论 / 表情回应 / 菜单 / 撤回 / bot 进出群）+ 引用正文转录（`quoted_context_text` / `card_text_transcript`，任意 JSON） |
| `wecom_frame_parse` | 企微 WS 入站帧两级解析：`proto::parse_frame` → `proto::parse_msg_callback`（任意 JSON 文本） |
