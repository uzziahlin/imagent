//! iLink 媒体收发：AES-128-ECB + PKCS7 + CDN 上传/下载。
//!
//! 协议事实（hermes 一手研究，见 docs/RESEARCH.md）：
//! - 块密码 AES-128-ECB，无 IV，PKCS7 填充，块大小 16。
//! - key 客户端生成 16 字节随机；**编码不对称**：
//!   - 入站 image：`image_item.aeskey` 是**裸 hex**。
//!   - 入站 file/video/voice：`media.aes_key` 是 **base64**（其内容可能是 hex 字符串的 base64）。
//!   - 出站 sendmessage：`aes_key` 字段 = `base64(hex_string)`。
//! - CDN：
//!   - 下载：`GET https://novac2c.cdn.weixin.qq.com/c2c/download?encrypted_query_param=<param>`。
//!   - 上传：`POST https://novac2c.cdn.weixin.qq.com/c2c/upload?encrypted_query_param=<x-encrypted-param>&filekey=<fk>`，
//!     body 为加密后二进制，`Content-Type: application/octet-stream`；
//!     成功响应头 `x-encrypted-param` 即 sendmessage 的 `encrypt_query_param`。
//!
//! SSRF 硬约束：入站 `full_url` 是不可信输入，主机必须在 CDN 白名单内。

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::{Aes128, Block};
use base64::Engine;
use imagent_core::{CoreError, Result};

use crate::client::DEFAULT_BASE_URL;

/// CDN 主机白名单（SSRF 防护）。
const CDN_HOSTS: &[&str] = &["novac2c.cdn.weixin.qq.com"];

/// AES 块大小。
pub const BLOCK: usize = 16;

// ───────────────────────── AES-128-ECB ─────────────────────────

/// PKCS7 填充（块 16）：明文对齐时仍补一整块。
fn pkcs7_pad(data: &[u8]) -> Vec<u8> {
    let pad = BLOCK - (data.len() % BLOCK);
    let mut out = data.to_vec();
    out.resize(data.len() + pad, pad as u8);
    out
}

/// PKCS7 去填充；非法返回 None。
fn pkcs7_unpad(data: &[u8]) -> Option<Vec<u8>> {
    let (len, last) = (data.len(), *data.last()?);
    let p = last as usize;
    if len == 0 || last == 0 || p > BLOCK || p > len {
        return None;
    }
    if !data[len - p..].iter().all(|&b| b == last) {
        return None;
    }
    let mut out = data.to_vec();
    out.truncate(len - p);
    Some(out)
}

/// AES-128-ECB + PKCS7 加密。
pub fn aes_encrypt(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let cipher = Aes128::new_from_slice(key).expect("16-byte key");
    let mut out = pkcs7_pad(plaintext);
    for chunk in out.chunks_exact_mut(BLOCK) {
        let block = Block::from_mut_slice(chunk);
        cipher.encrypt_block(block);
    }
    out
}

/// AES-128-ECB + PKCS7 解密；密文长度非块倍数或填充非法时返回 `None`。
pub fn aes_decrypt(ciphertext: &[u8], key: &[u8; 16]) -> Option<Vec<u8>> {
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(BLOCK) {
        return None;
    }
    let cipher = Aes128::new_from_slice(key).expect("16-byte key");
    let mut buf = ciphertext.to_vec();
    for chunk in buf.chunks_exact_mut(BLOCK) {
        let block = Block::from_mut_slice(chunk);
        cipher.decrypt_block(block);
    }
    pkcs7_unpad(&buf)
}

/// 生成 16 字节随机 key。
pub fn random_aes_key() -> [u8; 16] {
    use rand::RngCore;
    let mut k = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut k);
    k
}

// ───────────────────────── key 编码解析 ─────────────────────────

/// 把入站 aes_key 字符串解析成 16 字节 key。
///
/// 三种形态（hermes 实测）：
/// 1. 裸 hex（32 字符）→ `from_hex` 直接得 16 字节。
/// 2. base64 → 解码：
///    - 若得 16 字节，直接用。
///    - 若得 32 字节且全是 hex 字符（hex 字符串的 base64），先当 ASCII 文本再 `from_hex`。
/// 3. base64 内是 32 字符 hex 文本 → 同上路径。
pub fn parse_aes_key(s: &str) -> Option<[u8; 16]> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 1. 裸 hex（32 字符）。
    if let Ok(bytes) = hex::decode(trimmed) {
        if bytes.len() == 16 {
            let mut k = [0u8; 16];
            k.copy_from_slice(&bytes);
            return Some(k);
        }
    }
    // 2. base64。
    let raw = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .or_else(|_| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(trimmed)
        })
        .ok()?;
    match raw.len() {
        16 => {
            let mut k = [0u8; 16];
            k.copy_from_slice(&raw);
            Some(k)
        }
        32 => {
            // 可能是 32 字符 hex 文本的 base64。
            let txt = std::str::from_utf8(&raw).ok()?;
            let bytes = hex::decode(txt).ok()?;
            if bytes.len() == 16 {
                let mut k = [0u8; 16];
                k.copy_from_slice(&bytes);
                return Some(k);
            }
            None
        }
        _ => None,
    }
}

/// 出站 aes_key 编码：`base64(hex_string)`（hermes 实测的非对称编码）。
pub fn encode_aes_key_outbound(key: &[u8; 16]) -> String {
    let hex_str = hex::encode(key);
    base64::engine::general_purpose::STANDARD.encode(hex_str.as_bytes())
}

// ───────────────────────── SSRF ─────────────────────────

/// 从 URL 字符串中提取域名部分（协议后的 host，去掉端口/路径/查询）。
fn extract_host(url: &str) -> Option<&str> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    // 去端口。
    Some(authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority))
}

/// 校验 URL 主机在 CDN 白名单内；非白名单返回 Err（SSRF 防护）。
pub fn assert_cdn_host(url: &str) -> Result<()> {
    let host = extract_host(url).unwrap_or("");
    if host.is_empty() {
        return Err(CoreError::Platform("ilink", format!("invalid url (no host): {url}")));
    }
    if CDN_HOSTS.contains(&host) {
        Ok(())
    } else {
        Err(CoreError::Platform(
            "ilink",
            format!("SSRF blocked: host {host:?} not in CDN whitelist"),
        ))
    }
}

/// 构造 CDN 下载 URL：优先 `encrypt_query_param`，否则 `full_url`（SSRF 校验）。
/// 两者皆空返回 Err。
pub fn resolve_download_url(encrypt_query_param: Option<&str>, full_url: Option<&str>) -> Result<String> {
    if let Some(p) = encrypt_query_param.filter(|s| !s.is_empty()) {
        Ok(format!("https://{}/c2c/download?encrypted_query_param={p}", CDN_HOSTS[0]))
    } else if let Some(u) = full_url.filter(|s| !s.is_empty()) {
        assert_cdn_host(u)?;
        Ok(u.to_string())
    } else {
        Err(CoreError::Platform(
            "ilink",
            "media has no encrypt_query_param or full_url".to_string(),
        ))
    }
}

// ───────────────────────── CDN 下载/上传 ─────────────────────────

/// CDN 下载 + AES 解密。
pub async fn download_media(
    client: &reqwest::Client,
    encrypt_query_param: Option<&str>,
    aes_key: Option<&str>,
    full_url: Option<&str>,
) -> Result<Vec<u8>> {
    let url = resolve_download_url(encrypt_query_param, full_url)?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| CoreError::Platform("ilink", format!("cdn download: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(CoreError::Platform(
            "ilink",
            format!("cdn download HTTP {status}"),
        ));
    }
    let ciphertext = resp
        .bytes()
        .await
        .map_err(|e| CoreError::Platform("ilink", format!("cdn download body: {e}")))?
        .to_vec();
    // 有 aes_key 则解密；否则直接返回（兼容明文）。
    match aes_key.and_then(parse_aes_key) {
        Some(k) => aes_decrypt(&ciphertext, &k).ok_or_else(|| {
            CoreError::Platform("ilink", "cdn download: aes decrypt/padding failed".to_string())
        }),
        None => Ok(ciphertext),
    }
}

#[allow(dead_code)]
/// getuploadurl 响应。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct UploadUrlResp {
    #[serde(default)]
    pub upload_full_url: Option<String>,
    #[serde(default)]
    pub upload_param: Option<String>,
}

/// 取上传凭证：`POST /ilink/bot/getuploadurl`。
///
/// body：`{filekey, media_type, to_user_id, rawsize, rawfilemd5, filesize, no_need_thumb, aeskey}`。
/// `media_type`：1=img / 2=video / 3=file / 4=voice（注意与 item type 不同，hermes 实测）。
// 8 个参数均为上传接口必需字段，无法进一步聚合；此 allow 为有意为之。
#[allow(clippy::too_many_arguments)]
pub async fn get_upload_url(
    client: &crate::client::ILinkClient,
    filekey: &str,
    media_type: i64,
    to_user_id: &str,
    raw_size: u64,
    raw_md5_hex: &str,
    file_size: u64,
    aeskey_hex: &str,
) -> Result<UploadUrlResp> {
    let body = serde_json::json!({
        "filekey": filekey,
        "media_type": media_type,
        "to_user_id": to_user_id,
        "rawsize": raw_size,
        "rawfilemd5": raw_md5_hex,
        "filesize": file_size,
        "no_need_thumb": true,
        "aeskey": aeskey_hex,
    });
    client.post_json("/ilink/bot/getuploadurl", &body).await
}

/// CDN 上传（POST 二进制）。返回响应头里的 `x-encrypted-param`（即 sendmessage 的凭证）。
pub async fn upload_cdn(
    http: &reqwest::Client,
    x_encrypted_param: &str,
    filekey: &str,
    ciphertext: &[u8],
) -> Result<String> {
    let url = format!(
        "https://{}/c2c/upload?encrypted_query_param={x_encrypted_param}&filekey={filekey}",
        CDN_HOSTS[0]
    );
    let resp = http
        .post(&url)
        .header("Content-Type", "application/octet-stream")
        .body(ciphertext.to_vec())
        .send()
        .await
        .map_err(|e| CoreError::Platform("ilink", format!("cdn upload: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(CoreError::Platform(
            "ilink",
            format!("cdn upload HTTP {status}"),
        ));
    }
    resp.headers()
        .get("x-encrypted-param")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| CoreError::Platform("ilink", "cdn upload: missing x-encrypted-param header".to_string()))
}

/// 暴露 base url 给媒体模块（仅用于文档化，避免未用警告）。
#[allow(dead_code)]
fn _base_url_doc() -> &'static str {
    DEFAULT_BASE_URL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_roundtrip_known_key() {
        let key = [0x42u8; 16];
        let pt = b"hello imagent media roundtrip";
        let ct = aes_encrypt(pt, &key);
        assert_eq!(ct.len() % BLOCK, 0);
        assert_ne!(&ct[..], pt);
        let dec = aes_decrypt(&ct, &key).expect("decrypt");
        assert_eq!(dec, pt);
    }

    #[test]
    fn aes_roundtrip_block_aligned() {
        // 恰好整块 → PKCS7 仍加一整块填充。
        let key = [7u8; 16];
        let pt = vec![0xABu8; 16];
        let ct = aes_encrypt(&pt, &key);
        assert_eq!(ct.len(), 32);
        assert_eq!(aes_decrypt(&ct, &key).unwrap(), pt);
    }

    #[test]
    fn aes_roundtrip_empty() {
        let key = [1u8; 16];
        let ct = aes_encrypt(&[], &key);
        assert_eq!(ct.len(), 16); // 一整块填充
        assert_eq!(aes_decrypt(&ct, &key).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn aes_decrypt_rejects_bad_input() {
        let key = [1u8; 16];
        // 长度非块倍数。
        assert!(aes_decrypt(&[0u8; 15], &key).is_none());
        // 空。
        assert!(aes_decrypt(&[], &key).is_none());
    }

    #[test]
    fn parse_key_bare_hex() {
        let hex_str = "0102030405060708090a0b0c0d0e0f10";
        let k = parse_aes_key(hex_str).expect("hex key");
        assert_eq!(k, [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
    }

    #[test]
    fn parse_key_base64_raw16() {
        // base64(16 字节)。
        let raw = [9u8; 16];
        let s = base64::engine::general_purpose::STANDARD.encode(raw);
        let k = parse_aes_key(&s).expect("b64 raw16");
        assert_eq!(k, raw);
    }

    #[test]
    fn parse_key_base64_of_hex_text() {
        // base64("00112233445566778899aabbccddeeff") —— 出站编码形态也可入站解析。
        let hex_str = "00112233445566778899aabbccddeeff";
        let s = base64::engine::general_purpose::STANDARD.encode(hex_str.as_bytes());
        let k = parse_aes_key(&s).expect("b64-of-hex");
        assert_eq!(k.len(), 16);
        assert_eq!(k, [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
    }

    #[test]
    fn outbound_key_encoding_roundtrips_through_parse() {
        let key = [0xA1u8; 16];
        let enc = encode_aes_key_outbound(&key);
        let back = parse_aes_key(&enc).expect("parse outbound-encoded key");
        assert_eq!(back, key);
    }

    #[test]
    fn ssrf_rejects_non_cdn_host() {
        let res = assert_cdn_host("https://evil.example.com/x");
        assert!(res.is_err(), "must reject non-CDN host");
        let msg = format!("{}", res.unwrap_err());
        assert!(msg.contains("SSRF"), "msg={msg}");
    }

    #[test]
    fn ssrf_allows_cdn_host() {
        assert_cdn_host("https://novac2c.cdn.weixin.qq.com/c2c/download?encrypted_query_param=x").unwrap();
    }

    #[test]
    fn resolve_url_prefers_query_param() {
        let url = resolve_download_url(Some("PARAM"), Some("https://novac2c.cdn.weixin.qq.com/x")).unwrap();
        assert!(url.contains("encrypted_query_param=PARAM"));
        assert!(url.contains("novac2c.cdn.weixin.qq.com"));
    }

    #[test]
    fn resolve_url_falls_back_to_full_url_with_ssrf() {
        let url = resolve_download_url(None, Some("https://novac2c.cdn.weixin.qq.com/c2c/download?x=1")).unwrap();
        assert_eq!(url, "https://novac2c.cdn.weixin.qq.com/c2c/download?x=1");
        // 非 CDN fallback 被拒。
        assert!(resolve_download_url(None, Some("https://evil.example.com/x")).is_err());
        // 全空报错。
        assert!(resolve_download_url(None, None).is_err());
    }
}
