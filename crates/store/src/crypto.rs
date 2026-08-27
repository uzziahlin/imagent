//! 凭据应用层加密（S3，v7 review 迭代项）。
//!
//! OS keyring 不可用（headless/CI 无 D-Bus、沙箱无 GUI）时的中间形态：用
//! passphrase（`IMAGENT_PASSPHRASE`）派生密钥做 AES-256-GCM 加密后落盘，
//! 优于明文但仍弱于 OS keychain（passphrase 与密文同机存储，防的是「拿到
//! db 文件副本即得凭据」，不防拿到完整运行环境的攻击者）。
//!
//! blob 形态（版本化，便于将来换 KDF/AEAD）：
//! ```text
//! enc:v1:<base64(salt[16] || nonce[12] || ciphertext)>
//! ```
//! - KDF：PBKDF2-SHA256，100_000 迭代，16 字节随机 salt；
//! - AEAD：AES-256-GCM，每次加密随机 12 字节 nonce，密文自带认证 tag——
//!   错误 passphrase / 篡改都会在解密时失败。
//!
//! `v1` 之后的版本号解析自前缀：读到不认识的版本（如将来 `enc:v2`）时旧
//! 二进制给出可读错误而不是误当明文。

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::Aes256Gcm;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use sha2::Sha256;

/// 加密 blob 前缀（含版本号 v1）。
pub(crate) const ENC_PREFIX: &str = "enc:v1:";
/// 所有加密形态的通用前缀（版本无关，用于识别「是加密 blob 但版本未知」）。
const ENC_FAMILY_PREFIX: &str = "enc:";
/// PBKDF2-SHA256 迭代次数（≥100k，OWASP 2023 对 SHA-256 的下限建议）。
const PBKDF2_ITERS: u32 = 100_000;
/// 随机 salt 长度（字节）。
const SALT_LEN: usize = 16;
/// GCM nonce 长度（字节，标准 96-bit）。
const NONCE_LEN: usize = 12;
/// AES-256 密钥长度（字节）。
const KEY_LEN: usize = 32;

/// 判断 blob 是否为当前支持的加密形态（`enc:v1:` 前缀）。
pub(crate) fn is_encrypted(blob: &str) -> bool {
    blob.starts_with(ENC_PREFIX) && blob.len() > ENC_PREFIX.len()
}

/// 用 passphrase 加密明文 blob，产出 `enc:v1:...` 形态字符串。
pub(crate) fn encrypt(passphrase: &str, plaintext: &str) -> Result<String, String> {
    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    let key = derive_key(passphrase, &salt);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("AES init: {e}"))?;
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ct = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .map_err(|e| format!("AES-256-GCM encrypt: {e}"))?;
    let mut payload = Vec::with_capacity(SALT_LEN + NONCE_LEN + ct.len());
    payload.extend_from_slice(&salt);
    payload.extend_from_slice(nonce.as_slice());
    payload.extend_from_slice(&ct);
    Ok(format!("{ENC_PREFIX}{}", B64.encode(payload)))
}

/// 解密 `enc:v1:` 形态的 blob。错误信息面向运维可读（提示 passphrase 或版本）。
pub(crate) fn decrypt(passphrase: &str, blob: &str) -> Result<String, String> {
    if !is_encrypted(blob) {
        if blob.starts_with(ENC_FAMILY_PREFIX) {
            // 版本化前缀：识别出「加密 blob 但非 v1」，避免误当明文或 base64 噪声。
            let ver = blob[ENC_FAMILY_PREFIX.len()..]
                .split(':')
                .next()
                .unwrap_or("");
            return Err(format!(
                "凭据为加密形态但格式版本未知（enc:{ver}:），当前二进制仅支持 enc:v1:\
                 （可能由更新版本写入，请升级 imagent）"
            ));
        }
        return Err("凭据 blob 不是加密形态（enc:v1:）".to_string());
    }
    let payload = B64
        .decode(blob[ENC_PREFIX.len()..].as_bytes())
        .map_err(|e| format!("加密凭据 base64 解码失败（blob 可能损坏）: {e}"))?;
    if payload.len() <= SALT_LEN + NONCE_LEN {
        return Err("加密凭据 payload 过短（blob 可能损坏）".to_string());
    }
    let (salt, rest) = payload.split_at(SALT_LEN);
    let (nonce, ct) = rest.split_at(NONCE_LEN);
    let key = derive_key(passphrase, salt);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("AES init: {e}"))?;
    let nonce = aes_gcm::Nonce::from_slice(nonce);
    let plain = cipher
        .decrypt(nonce, ct)
        .map_err(|_| "凭据解密失败：passphrase 不正确或 blob 已损坏（请检查 IMAGENT_PASSPHRASE）".to_string())?;
    String::from_utf8(plain).map_err(|e| format!("解密后非 UTF-8: {e}"))
}

/// PBKDF2-SHA256 派生 256-bit 密钥。
fn derive_key(passphrase: &str, salt: &[u8]) -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    pbkdf2::pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, PBKDF2_ITERS, &mut key);
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let enc = encrypt("pass-1", r#"{"bot_token":"secret"}"#).unwrap();
        assert!(enc.starts_with("enc:v1:"), "前缀版本化: {enc}");
        assert_ne!(enc, r#"{"bot_token":"secret"}"#);
        assert!(!enc.contains("secret"), "密文不得含明文片段");
        assert_eq!(decrypt("pass-1", &enc).unwrap(), r#"{"bot_token":"secret"}"#);
    }

    #[test]
    fn wrong_passphrase_fails() {
        let enc = encrypt("right", "blob").unwrap();
        let err = decrypt("wrong", &enc).unwrap_err();
        assert!(err.contains("IMAGENT_PASSPHRASE"), "错误应提示 passphrase: {err}");
    }

    #[test]
    fn random_salt_nonce_per_encryption() {
        // 同一明文两次加密产出不同密文（salt/nonce 随机）。
        let a = encrypt("p", "x").unwrap();
        let b = encrypt("p", "x").unwrap();
        assert_ne!(a, b);
        assert_eq!(decrypt("p", &a).unwrap(), "x");
        assert_eq!(decrypt("p", &b).unwrap(), "x");
    }

    #[test]
    fn unknown_version_rejected_with_readable_error() {
        let err = decrypt("p", "enc:v2:AAAA").unwrap_err();
        assert!(err.contains("v2") && err.contains("enc:v1"), "错误应说明版本: {err}");
        // is_encrypted 只认 v1：未知版本不会被误当 v1 加密形态。
        assert!(!is_encrypted("enc:v2:AAAA"));
    }

    #[test]
    fn tampered_payload_fails() {
        let mut enc = encrypt("p", "blob").unwrap();
        // 翻转 base64 尾字符（篡改 tag/密文）。
        let last = enc.pop().unwrap();
        enc.push(if last == 'A' { 'B' } else { 'A' });
        assert!(decrypt("p", &enc).is_err(), "GCM 认证应拒绝篡改");
    }

    #[test]
    fn is_encrypted_and_marker_disambiguation() {
        // enc 前缀不会被 is_keyring_marker 误判（在 credentials::tests 亦有覆盖）。
        assert!(is_encrypted(&encrypt("p", "x").unwrap()));
        assert!(!is_encrypted("enc:v1:")); // 空 payload 不算
        assert!(!is_encrypted("keyring:ilink:bot-1"));
        assert!(!is_encrypted(r#"{"bot_token":"x"}"#));
    }
}
