//! fuzz: ilink 媒体 SSRF host 校验（任意 URL 不 panic）。E-7。
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = imagent_ilink::media::assert_cdn_host(s);
    }
});
