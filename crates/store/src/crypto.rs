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
//! enc:v2:<base64(iters_be[u32] || salt[16] || nonce[12] || ciphertext)>
//! ```
//! - KDF：PBKDF2-SHA256；v2 把迭代次数嵌入 payload（`iters_be`，大端 u32），
//!   读取时按嵌入值派生——此后提升迭代次数无需再换格式；新写入用
//!   [`PBKDF2_ITERS_V2`]（600k）。v1 为旧格式：固定 100k、无嵌入值，仅兼容
//!   读取（读到即按 100k 派生），不再新写。
//! - AEAD：AES-256-GCM，每次加密随机 12 字节 nonce，密文自带认证 tag——
//!   错误 passphrase / 篡改都会在解密时失败。v2 额外把 AAD 绑定为
//!   `platform:account_id`（见 [`encrypt`]）：密文被挪到其它行（不同
//!   platform/account）时 GCM 认证失败，防错配注入。v1 无 AAD，兼容读取。
//!
//! `enc:` 之后的版本号解析自前缀：读到不认识的版本（如将来 `enc:v3`）时旧
//! 二进制给出可读错误而不是误当明文。

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::Aes256Gcm;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use sha2::Sha256;

/// 旧格式前缀（固定 100k 迭代、无 AAD；仅兼容读取，不再新写）。
pub(crate) const ENC_PREFIX_V1: &str = "enc:v1:";
/// 新写入格式前缀（迭代数嵌入 payload + AAD 绑定）。
pub(crate) const ENC_PREFIX: &str = "enc:v2:";
/// 所有加密形态的通用前缀（版本无关，用于识别「是加密 blob 但版本未知」）。
const ENC_FAMILY_PREFIX: &str = "enc:";
/// v1（旧格式）PBKDF2-SHA256 迭代次数——仅读取旧 blob 时使用。
const PBKDF2_ITERS_V1: u32 = 100_000;
/// v2 新写入的 PBKDF2-SHA256 迭代次数（600k，2023+ OWASP 对 SHA-256 的
/// 建议量级；v2 把迭代数嵌入 payload，后续再提升无需换格式）。
const PBKDF2_ITERS_V2: u32 = 600_000;
/// 随机 salt 长度（字节）。
const SALT_LEN: usize = 16;
/// GCM nonce 长度（字节，标准 96-bit）。
const NONCE_LEN: usize = 12;
/// v2 payload 头部嵌入的迭代次数长度（大端 u32）。
const ITERS_LEN: usize = 4;
/// AES-256 密钥长度（字节）。
const KEY_LEN: usize = 32;

/// 判断 blob 是否为当前支持的加密形态（`enc:v1:` / `enc:v2:` 前缀）。
pub(crate) fn is_encrypted(blob: &str) -> bool {
    (blob.starts_with(ENC_PREFIX) || blob.starts_with(ENC_PREFIX_V1))
        && blob.len() > ENC_FAMILY_PREFIX.len() + 3 // "v_:" 至少还要有 1 字节 payload
}

/// 用 passphrase 加密明文 blob，产出 `enc:v2:...` 形态字符串。
///
/// `aad` 为 GCM 附加认证数据——绑定凭据归属（`platform:account_id`），
/// 密文挪行（同库不同账号）时解密失败，防错配注入。
pub(crate) fn encrypt(passphrase: &str, plaintext: &str, aad: &str) -> Result<String, String> {
    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    let key = derive_key(passphrase, &salt, PBKDF2_ITERS_V2);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("AES init: {e}"))?;
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let payload = aead_payload(plaintext, aad);
    let ct = cipher
        .encrypt(&nonce, payload.as_slice())
        .map_err(|e| format!("AES-256-GCM encrypt: {e}"))?;
    let mut payload = Vec::with_capacity(ITERS_LEN + SALT_LEN + NONCE_LEN + ct.len());
    payload.extend_from_slice(&PBKDF2_ITERS_V2.to_be_bytes());
    payload.extend_from_slice(&salt);
    payload.extend_from_slice(nonce.as_slice());
    payload.extend_from_slice(&ct);
    Ok(format!("{ENC_PREFIX}{}", B64.encode(payload)))
}

/// 解密 `enc:v1:` / `enc:v2:` 形态的 blob。错误信息面向运维可读（提示
/// passphrase 或版本）。`aad` 须与加密时一致（v1 旧格式无 AAD，忽略之）。
pub(crate) fn decrypt(passphrase: &str, blob: &str, aad: &str) -> Result<String, String> {
    let v2 = blob.starts_with(ENC_PREFIX);
    if !v2 && !blob.starts_with(ENC_PREFIX_V1) {
        if blob.starts_with(ENC_FAMILY_PREFIX) {
            // 版本化前缀：识别出「加密 blob 但非 v1/v2」，避免误当明文或 base64 噪声。
            let ver = blob[ENC_FAMILY_PREFIX.len()..]
                .split(':')
                .next()
                .unwrap_or("");
            return Err(format!(
                "凭据为加密形态但格式版本未知（enc:{ver}:），当前二进制仅支持 enc:v1:/enc:v2:\
                 （可能由更新版本写入，请升级 imagent）"
            ));
        }
        return Err("凭据 blob 不是加密形态（enc:v1:/enc:v2:）".to_string());
    }
    let body = &blob[ENC_FAMILY_PREFIX.len() + 3..]; // 跳过 "v<N>:"
    let payload = B64
        .decode(body.as_bytes())
        .map_err(|e| format!("加密凭据 base64 解码失败（blob 可能损坏）: {e}"))?;
    // v2: iters[u32] || salt || nonce || ct；v1: salt || nonce || ct（固定 100k，无 AAD）。
    let (iters, rest) = if v2 {
        if payload.len() <= ITERS_LEN + SALT_LEN + NONCE_LEN {
            return Err("加密凭据 payload 过短（blob 可能损坏）".to_string());
        }
        let (iters_raw, rest) = payload.split_at(ITERS_LEN);
        let iters = u32::from_be_bytes([iters_raw[0], iters_raw[1], iters_raw[2], iters_raw[3]]);
        if iters == 0 {
            return Err("加密凭据嵌入的 PBKDF2 迭代数非法（0）".to_string());
        }
        (iters, rest)
    } else {
        if payload.len() <= SALT_LEN + NONCE_LEN {
            return Err("加密凭据 payload 过短（blob 可能损坏）".to_string());
        }
        (PBKDF2_ITERS_V1, &payload[..])
    };
    let (salt, rest) = rest.split_at(SALT_LEN);
    let (nonce, ct) = rest.split_at(NONCE_LEN);
    let key = derive_key(passphrase, salt, iters);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("AES init: {e}"))?;
    let nonce = aes_gcm::Nonce::from_slice(nonce);
    let plain = cipher
        .decrypt(nonce, ct)
        .map_err(|_| "凭据解密失败：passphrase 不正确、凭据归属不匹配（AAD）或 blob 已损坏（请检查 IMAGENT_PASSPHRASE）".to_string())?;
    // v2：AEAD 明文单元为 aad_len[u32] || aad || msg，校验 aad 一致防挪行；
    // v1 无 AAD，明文即消息本体。
    let plaintext_bytes = if v2 {
        split_aead_payload(&plain, aad)?
    } else {
        plain
    };
    String::from_utf8(plaintext_bytes).map_err(|e| format!("解密后非 UTF-8: {e}"))
}

/// v2 AEAD 明文单元编码：`aad_len[u32 BE] || aad || message`——把 AAD 放进
/// 认证范围且解密时可校验归属。
fn aead_payload(msg: &str, aad: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + aad.len() + msg.len());
    out.extend_from_slice(&(aad.len() as u32).to_be_bytes());
    out.extend_from_slice(aad.as_bytes());
    out.extend_from_slice(msg.as_bytes());
    out
}

/// 拆 v2 AEAD 明文单元并校验 AAD 与预期一致（不一致 = 密文被挪行，报错）。
fn split_aead_payload(pt: &[u8], expected_aad: &str) -> Result<Vec<u8>, String> {
    if pt.len() < 4 {
        return Err("加密凭据 payload 损坏（AAD 长度缺失）".to_string());
    }
    let aad_len = u32::from_be_bytes([pt[0], pt[1], pt[2], pt[3]]) as usize;
    if pt.len() < 4 + aad_len {
        return Err("加密凭据 payload 损坏（AAD 截断）".to_string());
    }
    let (aad, msg) = pt[4..].split_at(aad_len);
    if aad != expected_aad.as_bytes() {
        return Err(
            "凭据解密失败：密文归属（platform:account_id）与读取位置不匹配——\
             可能密文被挪行或库损坏，拒绝错配返回"
                .to_string(),
        );
    }
    Ok(msg.to_vec())
}

/// PBKDF2-SHA256 派生 256-bit 密钥（迭代次数由调用方给定：v2 读嵌入值、
/// v1 固定 100k）。
fn derive_key(passphrase: &str, salt: &[u8], iters: u32) -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    pbkdf2::pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, iters, &mut key);
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let enc = encrypt("pass-1", r#"{"bot_token":"secret"}"#, "ilink:bot1").unwrap();
        assert!(enc.starts_with("enc:v2:"), "新写入应为 v2: {enc}");
        assert_ne!(enc, r#"{"bot_token":"secret"}"#);
        assert!(!enc.contains("secret"), "密文不得含明文片段");
        assert_eq!(
            decrypt("pass-1", &enc, "ilink:bot1").unwrap(),
            r#"{"bot_token":"secret"}"#
        );
    }

    #[test]
    fn wrong_passphrase_fails() {
        let enc = encrypt("right", "blob", "p:a").unwrap();
        let err = decrypt("wrong", &enc, "p:a").unwrap_err();
        assert!(
            err.contains("IMAGENT_PASSPHRASE"),
            "错误应提示 passphrase: {err}"
        );
    }

    /// AAD 绑定：同一密文用不同归属（platform:account）解密必须失败。
    #[test]
    fn aad_mismatch_fails() {
        let enc = encrypt("p", "secret-blob", "ilink:bot1").unwrap();
        let err = decrypt("p", &enc, "ilink:bot2").unwrap_err();
        assert!(
            err.contains("不匹配") || err.contains("IMAGENT_PASSPHRASE"),
            "{err}"
        );
        // 正确归属仍可解。
        assert_eq!(decrypt("p", &enc, "ilink:bot1").unwrap(), "secret-blob");
    }

    /// v2 迭代数嵌入：解密按嵌入值派生（手构低迭代 blob 验证读取路径遵守之）。
    #[test]
    fn v2_embedded_iterations_are_honored() {
        // 手构一个 iters=1000 的 v2 blob（不走 encrypt 的 600k），解密应成功。
        let salt = [7u8; SALT_LEN];
        let key = derive_key("p", &salt, 1000);
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let nonce = [9u8; NONCE_LEN];
        let ct = cipher
            .encrypt(
                aes_gcm::Nonce::from_slice(&nonce),
                aead_payload("handmade", "p:a").as_slice(),
            )
            .unwrap();
        let mut payload = Vec::new();
        payload.extend_from_slice(&1000u32.to_be_bytes());
        payload.extend_from_slice(&salt);
        payload.extend_from_slice(&nonce);
        payload.extend_from_slice(&ct);
        let blob = format!("{ENC_PREFIX}{}", B64.encode(payload));
        assert_eq!(decrypt("p", &blob, "p:a").unwrap(), "handmade");
    }

    /// v1 旧格式（固定 100k、无 AAD）兼容读取。
    #[test]
    fn v1_legacy_blob_still_decrypts() {
        // 手构 v1 payload：salt || nonce || ct（100k 迭代、无 AAD）。
        let salt = [3u8; SALT_LEN];
        let key = derive_key("legacy-pass", &salt, PBKDF2_ITERS_V1);
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let nonce = [5u8; NONCE_LEN];
        let ct = cipher
            .encrypt(
                aes_gcm::Nonce::from_slice(&nonce),
                b"{\"bot_token\":\"legacy\"}".as_slice(),
            )
            .unwrap();
        let mut payload = Vec::new();
        payload.extend_from_slice(&salt);
        payload.extend_from_slice(&nonce);
        payload.extend_from_slice(&ct);
        let blob = format!("{ENC_PREFIX_V1}{}", B64.encode(payload));
        assert!(is_encrypted(&blob));
        // v1 无 AAD：读取时传入的 aad 被忽略，不校验归属。
        assert_eq!(
            decrypt("legacy-pass", &blob, "any:where").unwrap(),
            r#"{"bot_token":"legacy"}"#
        );
    }

    #[test]
    fn random_salt_nonce_per_encryption() {
        // 同一明文两次加密产出不同密文（salt/nonce 随机）。
        let a = encrypt("p", "x", "k:v").unwrap();
        let b = encrypt("p", "x", "k:v").unwrap();
        assert_ne!(a, b);
        assert_eq!(decrypt("p", &a, "k:v").unwrap(), "x");
        assert_eq!(decrypt("p", &b, "k:v").unwrap(), "x");
    }

    #[test]
    fn unknown_version_rejected_with_readable_error() {
        let err = decrypt("p", "enc:v3:AAAA", "k:v").unwrap_err();
        assert!(err.contains("v3"), "错误应说明版本: {err}");
        // is_encrypted 只认 v1/v2：未知版本不会被误当加密形态。
        assert!(!is_encrypted("enc:v3:AAAA"));
    }

    #[test]
    fn tampered_payload_fails() {
        let mut enc = encrypt("p", "blob", "k:v").unwrap();
        // 翻转 base64 尾字符（篡改 tag/密文）。
        let last = enc.pop().unwrap();
        enc.push(if last == 'A' { 'B' } else { 'A' });
        assert!(decrypt("p", &enc, "k:v").is_err(), "GCM 认证应拒绝篡改");
    }

    #[test]
    fn is_encrypted_and_marker_disambiguation() {
        // enc 前缀不会被 is_keyring_marker 误判（在 credentials::tests 亦有覆盖）。
        assert!(is_encrypted(&encrypt("p", "x", "k:v").unwrap()));
        assert!(!is_encrypted("enc:v1:")); // 空 payload 不算
        assert!(!is_encrypted("enc:v2:")); // 空 payload 不算
        assert!(is_encrypted("enc:v1:AAAA"));
        assert!(!is_encrypted("keyring:ilink:bot-1"));
        assert!(!is_encrypted(r#"{"bot_token":"x"}"#));
    }
}
