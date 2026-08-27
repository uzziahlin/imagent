//! fuzz: 企微 WS 入站帧解析（任意 JSON 输入不 panic）。
//!
//! 真实外部输入攻击面：企微服务端经 WS 长连接推来的原始帧文本。client 收到
//! 后先 `proto::parse_frame`（serde 反序列化为 `WsFrame`），回调帧再经
//! `proto::parse_msg_callback` 解析为 `InboundMessage`（含 `userid_from_conv`
//! 的 conv 派生路径）。两级解析对任意输入都必须不 panic。
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    use imagent_wecom::proto;
    // 输入按 UTF-8 str 尝试（parse_frame 入参即 &str；非法 UTF-8 直接跳过，
    // 真实 WS 消息文本层已保证 UTF-8）。
    if let Ok(raw) = std::str::from_utf8(data) {
        if let Ok(frame) = proto::parse_frame(raw) {
            // 解析成功的帧再过回调解析路径（cmd/body/errcode 任意组合）。
            if let Ok((_msgid, msg)) = proto::parse_msg_callback(&frame) {
                // 解析产物再过一遍字段访问（conv 派生、媒体/文本提取等派生路径）。
                let _ = msg.text;
                let _ = &msg.conv_id.0;
            }
        }
    }
});
