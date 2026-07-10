//! fuzz: ilink proto 响应反序列化 + 文本提取（任意 JSON 输入不 panic）。E-7 / v5-F1。
//!
//! 真实外部输入攻击面：`getupdates` 长轮询返回的服务端 JSON 响应——
//! 任意/恶意/异常 JSON 经 serde 反序列化为 `UpdatesResp`、再逐条 `extract_text`
//! 必须不 panic。覆盖协议解析的健壮性边界。
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        if let Ok(resp) = serde_json::from_str::<imagent_ilink::proto::UpdatesResp>(s) {
            for msg in &resp.msgs {
                let _ = imagent_ilink::proto::extract_text(msg);
            }
        }
    }
});
