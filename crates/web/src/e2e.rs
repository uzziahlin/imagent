//! E2E 中继盲化（协议 v1.4）：浏览器↔网关业务帧对 relay 全密文。
//!
//! ## 威胁模型与职责分工
//!
//! relay（ECS）持有网关身份公钥与会话表，能路由/录制全部帧——本批把它从
//! 「可信内容方」降级为「盲传输」：业务帧（text/card/card_patch/
//! approval_request/approval_resolved/pong 与 message/approval_response/
//! control/sync/ping，以及 v1.6 工作空间帧 workspaces/conv_workdir/error 与
//! workspace_list/workspace_save/workspace_remove/conv_workdir_set）全部包成
//! [`enc`](`enc 帧`) 密文，relay 只见 `type/machine/sid` 信封元数据（路由
//! 所需）与随机密文。密钥协商在浏览器与网关间端到端完成：
//!
//! ## 握手（session 认证通过后，浏览器逐机器发起；帧经 relay 按 machine
//! 路由，relay 透传不理解）
//!
//! 1. 浏览器 → `{"v":1,"type":"e2e_hello","machine":"<目标>","sid":"<本浏览器
//!    session id>","ek":"<浏览器临时 X25519 公钥 64hex>","n1":"<32B hex>"}`
//!    ——sid 是 relay 侧 enc 帧定向投递键（多条浏览器连接复用同一条网关
//!    WS 连接，sid 是网关的唯一浏览器寻址维度）。**sid 缺失/空 → 广播
//!    模式**（兼容形态：会话以空 sid 建入，enc 帧广播全部浏览器、仅持钥
//!    方可解；多浏览器并存时同槽顶替，最后握手者有效——带 sid 的定向
//!    模式才是完整形态）。
//! 2. 网关 → `{"v":1,"type":"e2e_ack","machine":"<自身>","gk":"<网关 E2E
//!    公钥 64hex>","ik":"<网关 v1.3 ed25519 认证公钥 64hex>","n2":"<32B
//!    hex>","sig":"<网关 v1.3 ed25519 认证私钥对 (ek||gk||n1||n2) 原始
//!    字节拼接的签名 128hex>"}`。`ik`（identity key）是浏览器 TOFU pin
//!    的身份公钥来源：未 pin 首连 SPA 以帧内 ik 自洽验签后弹指纹确认层，
//!    用户核对后 ik/gk 入 pin 表；已 pin 连接用 pin 的 ik 验 sig 并核对
//!    gk/ik 未变。**缺 ik 则 SPA 无法完成 TOFU（握手 fail-closed）**，
//!    故 ik 必须随 ack 携带（Rust 侧产出 sig 与 ik）。
//! 3. 会话密钥（两侧一致，逐字节定义——跨实现互操作锚点）：
//!    `k = HKDF-SHA256(ikm = X25519 共享秘密(32B) || n1(32B) || n2(32B),
//!    salt = 32 零字节（HKDF 无盐缺省，RFC 5869），info = "imagent-e2e-v1",
//!    L = 32)`。
//!
//! ## enc 帧（握手后双向业务帧包装）
//!
//! `{"v":1,"type":"enc","machine":"…","sid":"<浏览器 session id，网关→
//! 浏览器方向必带；浏览器→网关方向无 sid（单会话密钥按内容试解）>",
//! "box":"<base64>"}`；box = base64( 24B 随机 nonce || XChaCha20-Poly1305
//! 密文（明文 || 16B tag）)，明文 = 原业务帧完整 JSON（UTF-8），无 AAD。
//!
//! ## 兼容与降级语义
//!
//! - **无任何握手会话 → 出站行为不变（明文）**：加密是逐浏览器渐进的，
//!   旧 SPA 连接继续工作；
//! - **有活跃会话 → 每业务帧逐 sid 各加密一份**（enc{sid}），不再发对应
//!   明文——否则「任一旧浏览器在线就全局明文」违背盲化目标；
//! - 网关重连 relay 即清空会话（浏览器侧重连/重握手覆盖同 sid）；
//! - 已知残留风险（注释级声明）：恶意中继丢弃浏览器→网关 enc 帧可使
//!   半开会话 60s 后清理、网关回退明文（浏览器侧已握手则它自己的出站
//!   仍密文，SPA fail-closed 不明文降级）。回退时网关 warn 一次提示
//!   「可能存在中继干扰」。
//!
//! 本模块纯逻辑无 IO（密钥文件在 [`crate::key`]，网络循环在
//! [`crate::client`]），全部可单测。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::{Signer, SigningKey};
use hkdf::Hkdf;
use sha2::Sha256;
use tracing::{info, warn};
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret as XStaticSecret};

use crate::key;
use crate::TARGET;

/// HKDF info 标签（协议定案字面量，跨实现互操作锚点）。
const HKDF_INFO: &[u8] = b"imagent-e2e-v1";
/// 并存 E2E 会话上限（逐浏览器握手各占一席；对齐 relay 的浏览器并存上限
/// 量级，超出淘汰最旧——真机常态 2~5 台设备）。
pub(crate) const MAX_SESSIONS: usize = 8;
/// 握手半开清理窗口：e2e_ack 发出后 60s 内无一笔成功入站解密（浏览器
/// 握手后周期 enc-ping，正常秒级确认）即视为半开残留，清理（防探测
/// 洪水占满会话表）。已确认会话不按此清理（见 [`E2eSessions::prune`]）。
const HALF_OPEN_TTL: Duration = Duration::from_secs(60);
/// XChaCha20-Poly1305 nonce 长度（box 前缀）。
const NONCE_LEN: usize = 24;
/// Poly1305 认证_tag 长度。
const TAG_LEN: usize = 16;

/// 会话密钥派生（协议 v1.4 第 3 步，纯函数；两侧（浏览器/网关）以同一
/// IKM 各自计算得到同一 k——X25519 共享秘密对称）。
///
/// 字节级定义（SPA 侧 @noble 对齐锚点）：IKM = shared(32) || n1(32) ||
/// n2(32)；salt = None（HKDF 缺省 = HashLen 零字节；HMAC-SHA256 下「空
/// salt」与「32 零字节 salt」等价——密钥补零后相同）；info =
/// b"imagent-e2e-v1"；OKM 取前 32 字节。
pub fn derive_session_key(shared: &[u8; 32], n1: &[u8; 32], n2: &[u8; 32]) -> [u8; 32] {
    let mut ikm = [0u8; 96];
    ikm[..32].copy_from_slice(shared);
    ikm[32..64].copy_from_slice(n1);
    ikm[64..].copy_from_slice(n2);
    let hk = Hkdf::<Sha256>::new(None, &ikm);
    let mut okm = [0u8; 32];
    hk.expand(HKDF_INFO, &mut okm)
        .expect("OKM 32B ≤ 255×SHA256 块，恒合法");
    okm
}

/// 单帧密封（纯函数）：box = base64(nonce[24] || 密文||tag[16])。nonce 每帧
/// 随机——XChaCha20 的 192-bit nonce 随机取值碰撞概率可忽略（2^-96 生日界
/// 内安全），无需计数器状态。
fn seal(key: &[u8; 32], plaintext: &str) -> String {
    use base64::Engine as _;
    let nonce = rand::random::<[u8; NONCE_LEN]>();
    let cipher = XChaCha20Poly1305::new_from_slice(key).expect("32B 密钥恒合法");
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext.as_bytes())
        .expect("alloc 特性下加密仅内存分配失败，恒成功");
    let mut boxed = Vec::with_capacity(NONCE_LEN + ct.len());
    boxed.extend_from_slice(&nonce);
    boxed.extend_from_slice(&ct);
    base64::engine::general_purpose::STANDARD.encode(boxed)
}

/// 单帧开启（纯函数）：AEAD 验签失败 / base64 畸形 / UTF-8 畸形 → None。
fn open(key: &[u8; 32], boxed: &str) -> Option<String> {
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD
        .decode(boxed)
        .ok()?;
    if data.len() < NONCE_LEN + TAG_LEN {
        return None; // 至少 nonce + tag（空明文的下界）
    }
    let (nonce, ct) = data.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new_from_slice(key).expect("32B 密钥恒合法");
    let pt = cipher.decrypt(XNonce::from_slice(nonce), ct).ok()?;
    String::from_utf8(pt).ok()
}

/// 逐浏览器 E2E 会话表：`sid → 加密器`（上限 [`MAX_SESSIONS`]，超限淘汰
/// 最旧）。会话生命周期：e2e_hello 建入（半开）→ 首笔成功入站解密转确认 →
/// 网关重连 relay 整体清空（client 侧按连接重建）。**半开 60s 清理**只针对
/// 未确认会话（防握手洪水占表）；确认会话长期保留——浏览器断连网关不可见
///（relay 不通知），依赖浏览器重连重握手覆盖同 sid + 容量淘汰兜底。
pub(crate) struct E2eSessions {
    entries: HashMap<String, Entry>,
    /// 本连接生命周期内是否加密过出站帧（「回退明文」降级观测，warn 一次）。
    /// [`Cell`]：seal_each 走只读出站路径（&self）也要能记录。
    ever_encrypted: std::cell::Cell<bool>,
}

struct Entry {
    key: [u8; 32],
    created: Instant,
    /// 是否已有成功入站解密（浏览器持正确密钥的证明；半开清理豁免条件）。
    confirmed: bool,
}

impl E2eSessions {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            ever_encrypted: std::cell::Cell::new(false),
        }
    }

    /// 会话数（测试/日志用）。
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 建入/覆盖会话（同 sid 重握手 → 新密钥覆盖旧条目；浏览器重连场景）。
    /// 时间由调用方注入（单测可拨钟）。满 8 淘汰最旧（created 最小）。
    pub(crate) fn insert(&mut self, sid: &str, key: &[u8; 32], now: Instant) {
        if self.entries.len() >= MAX_SESSIONS && !self.entries.contains_key(sid) {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.created)
                .map(|(k, _)| k.clone())
            {
                warn!(
                    target: TARGET,
                    sid = %oldest,
                    "E2E 会话表满（{}），淘汰最旧会话",
                    MAX_SESSIONS
                );
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            sid.to_string(),
            Entry {
                key: *key,
                created: now,
                confirmed: false,
            },
        );
    }

    /// 出站逐会话加密：每个活跃 sid 各密封一份（enc{sid} 帧）。**空表返回
    /// 空 vec**——调用方据此走明文兼容路径（无任何握手会话时行为不变）。
    pub(crate) fn seal_each(&self, machine: &str, plaintext: &str) -> Vec<String> {
        if !self.entries.is_empty() {
            // ever_encrypted 由 take_plaintext_revert 复位；Cell 让只读出站
            // 路径（&self）也能记录，不为它改 &mut 签名。
            self.ever_encrypted.set(true);
        }
        self.entries
            .iter()
            .map(|(sid, e)| {
                serde_json::json!({
                    "v": 1,
                    "type": "enc",
                    "machine": machine,
                    "sid": sid,
                    "box": seal(&e.key, plaintext),
                })
                .to_string()
            })
            .collect()
    }

    /// 入站试解（浏览器→网关 enc 帧无 sid——单网关连接复用，按内容试解）：
    /// 逐活跃会话尝试 AEAD 开启（≤ [`MAX_SESSIONS`] 次，微秒级；不按内容
    /// 前缀选会话——密文对无密钥方是随机的，无可用前缀信息，全试是最简
    /// 正确解）。成功即标记该会话 confirmed（半开清理豁免）。全部失败 →
    /// None（调用方 warn 丢弃）。
    pub(crate) fn open_any(&mut self, boxed: &str) -> Option<String> {
        let sids: Vec<String> = self.entries.keys().cloned().collect();
        for sid in sids {
            let Some(entry) = self.entries.get_mut(&sid) else {
                continue;
            };
            if let Some(pt) = open(&entry.key, boxed) {
                entry.confirmed = true;
                return Some(pt);
            }
        }
        None
    }

    /// 半开清理（时间注入）：未确认 && 创建超 [`HALF_OPEN_TTL`] 的条目移除。
    /// 调用方在心跳 tick 顺带执行（无需独立定时器）。
    pub(crate) fn prune(&mut self, now: Instant) {
        self.entries
            .retain(|_, e| e.confirmed || now.duration_since(e.created) < HALF_OPEN_TTL);
    }

    /// 「加密→明文」回退观测（client warn 一次）：本连接曾加密过出站帧且
    /// 当前会话表已空 → true（并复位，仅告警首次）。正常触发 = 全部浏览器
    /// 断开且会话被清理/淘汰；异常触发 = 恶意中继丢弃浏览器 enc 帧使半开
    /// 会话 60s 清理（模块头注释的已知残留风险）——观测点即为此设。
    pub(crate) fn take_plaintext_revert(&mut self) -> bool {
        if self.ever_encrypted.get() && self.entries.is_empty() {
            self.ever_encrypted.set(false);
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// 网关 E2E 身份（静态 X25519 + v1.3 ed25519 签名）
// ---------------------------------------------------------------------------

/// 网关 E2E 身份：静态 X25519 密钥对（`web-e2e-key` 文件，见 [`key`]）+
/// v1.3 认证 ed25519 私钥（e2e_ack 签名复用——浏览器 pin 的网关公钥即它，
/// 独立签名密钥会让 TOFU pin 表出现第二种公钥，徒增核对负担；其公钥以
/// ack 的 `ik` 字段递送给浏览器做 pin）。
pub struct E2eIdentity {
    secret: XStaticSecret,
    public: XPublicKey,
    signing: SigningKey,
}

impl E2eIdentity {
    /// 从密钥文件装配（生成/加载见 [`key::load_or_generate_e2e`]）。
    pub fn load_or_generate(path: &std::path::Path, signing: SigningKey) -> Result<Self, String> {
        let (secret, public) = key::load_or_generate_e2e(path)?;
        Ok(Self {
            secret,
            public,
            signing,
        })
    }

    /// E2E 公钥指纹（SHA-256 前 16 hex；TOFU 核对用，日志已打印）。
    pub fn fingerprint(&self) -> String {
        key::e2e_fingerprint(&self.public)
    }

    /// 处理 e2e_hello（client 分拣后调用）：校验 machine/sid/ek/n1 → 生成
    /// n2 → X25519 派生会话密钥 → 建入会话表 → 产出 e2e_ack 帧。畸形/
    /// 越机（machine 非本机）→ None（warn 已记，不回帧——浏览器 10s 超时
    /// 走 fail-closed 明文选择，SPA 侧职责）。
    pub(crate) fn handle_hello(
        &self,
        sessions: &mut E2eSessions,
        v: &serde_json::Value,
        own_machine: &str,
    ) -> Option<String> {
        // machine 非本机 → 越机握手（多机广播残留/配置漂移），忽略。
        if let Some(m) = v.get("machine").and_then(|m| m.as_str()) {
            if m != own_machine {
                warn!(
                    target: TARGET,
                    frame_machine = %m,
                    own_machine = %own_machine,
                    "e2e_hello 的 machine 非本机，忽略"
                );
                return None;
            }
        }
        // sid（enc 帧定向路由键）：**带 sid → 定向模式**（enc{sid} 只投该
        // 浏览器会话，协议完整形态）；**缺失/空 → 广播模式**（会话以空 sid
        // 建入，enc{sid:""} 由 relay 广播全部浏览器，仅持钥方可解——兼容
        // 未携带 sid 的 SPA 形态，多浏览器并存时同槽互相顶替、最后握手者
        // 有效，warn 提示）。见模块头注释的偏离说明。
        let sid = v
            .get("sid")
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        if sid.is_empty() {
            warn!(
                target: TARGET,
                "e2e_hello 未携带 sid：会话以广播模式建入（enc 帧投递全部浏览器，\
                 仅持钥方可解）；新版 SPA 应带 sid 走定向投递"
            );
        }
        let ek = parse_key_hex(v.get("ek"), "ek")?;
        let n1 = parse_key_hex(v.get("n1"), "n1")?;
        let n2 = rand::random::<[u8; 32]>();
        let shared = self.secret.diffie_hellman(&XPublicKey::from(ek));
        let k = derive_session_key(shared.as_bytes(), &n1, &n2);
        sessions.insert(&sid, &k, Instant::now());
        let gk = self.public.as_bytes();
        let mut signed = [0u8; 128];
        signed[..32].copy_from_slice(&ek);
        signed[32..64].copy_from_slice(gk);
        signed[64..96].copy_from_slice(&n1);
        signed[96..].copy_from_slice(&n2);
        info!(
            target: TARGET,
            sid = %sid.chars().take(8).collect::<String>(),
            fingerprint = %self.fingerprint(),
            "E2E 握手完成（会话密钥已建入，等浏览器首笔 enc 帧确认）"
        );
        Some(
            serde_json::json!({
                "v": 1,
                "type": "e2e_ack",
                "machine": own_machine,
                "gk": hex::encode(gk),
                // ik：v1.3 认证 ed25519 公钥——SPA TOFU pin 表的存储与后续
                // 验签锚（缺它 SPA 无法完成首连 pin，见模块头第 2 步）。
                "ik": hex::encode(self.signing.verifying_key().to_bytes()),
                "n2": hex::encode(n2),
                "sig": hex::encode(self.signing.sign(&signed).to_bytes()),
            })
            .to_string(),
        )
    }
}

/// hex 字段解析（ek/n1：恰 32B hex）。畸形 → warn + None。
fn parse_key_hex(field: Option<&serde_json::Value>, name: &str) -> Option<[u8; 32]> {
    let raw = field
        .and_then(|f| f.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            warn!(target: TARGET, field = name, "e2e_hello 字段缺失/为空，忽略");
            None
        })?;
    let bytes = hex::decode(raw)
        .map_err(|_| {
            warn!(target: TARGET, field = name, "e2e_hello 字段非合法 hex，忽略");
        })
        .ok()?;
    <[u8; 32]>::try_from(bytes).map_err(|v: Vec<u8>| {
        warn!(target: TARGET, field = name, len_hex = v.len() * 2, "e2e_hello 字段长度非法（须 64 hex），忽略");
    }).ok()
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! 协议 v1.4 纯函数单测：两侧派生一致（浏览器视角模拟）、签名验签、
    //! 加解密往返、会话表上限/半开清理、enc/e2e_ack 帧形状、明文兼容路径。
    //! HKDF 参数化正确性另以 RFC 5869 官方向量钉住（hkdf_rfc5869_vector）。

    use super::*;
    use x25519_dalek::{PublicKey as XPublicKey, StaticSecret as XStaticSecret};

    /// HKDF-SHA256 参数化正确性：RFC 5869 Test Case 1 官方向量（不依赖本
    /// 协议任何自产数据——排除「用错 salt/info 顺序却自我一致」的循环验证）。
    #[test]
    fn hkdf_rfc5869_vector() {
        let ikm = [0x0bu8; 22];
        let salt = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
        ];
        let info = [0xf0u8, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9];
        let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
        let mut okm = [0u8; 42];
        hk.expand(&info, &mut okm).unwrap();
        assert_eq!(
            hex::encode(okm),
            "3cb25f25faacd57a90434f64d0362f2a\
             2d2d0a90cf1a5a4c5db02d56ecc4c5bf\
             34007208d5b887185865"
        );
    }

    /// 浏览器/网关两侧独立派生出同一会话密钥（X25519 对称性 + HKDF 一致）。
    #[test]
    fn both_sides_derive_same_session_key() {
        let gw_secret = XStaticSecret::from(rand::random::<[u8; 32]>());
        let gw_public = XPublicKey::from(&gw_secret);
        let browser_secret = XStaticSecret::from(rand::random::<[u8; 32]>());
        let browser_public = XPublicKey::from(&browser_secret);
        let n1 = rand::random::<[u8; 32]>();
        let n2 = rand::random::<[u8; 32]>();
        let k_gw = derive_session_key(
            gw_secret.diffie_hellman(&browser_public).as_bytes(),
            &n1,
            &n2,
        );
        let k_browser = derive_session_key(
            browser_secret.diffie_hellman(&gw_public).as_bytes(),
            &n1,
            &n2,
        );
        assert_eq!(hex::encode(k_gw), hex::encode(k_browser));
        // n1/n2 任一不同 → 密钥全变（nonce 参与派生）。
        let n2b = rand::random::<[u8; 32]>();
        let k2 = derive_session_key(
            gw_secret.diffie_hellman(&browser_public).as_bytes(),
            &n1,
            &n2b,
        );
        assert_ne!(hex::encode(k_gw), hex::encode(k2));
    }

    /// 密封/开启往返：box = base64(nonce24 || ct||tag16)；错误密钥 AEAD
    /// 失败；篡改 box（翻转末位 tag 字节）失败。
    #[test]
    fn seal_open_roundtrip_and_tamper() {
        use base64::Engine as _;
        let k = rand::random::<[u8; 32]>();
        let plaintext = r#"{"v":1,"type":"message","text":"你好"}"#;
        let boxed = seal(&k, plaintext);
        assert_eq!(open(&k, &boxed).as_deref(), Some(plaintext));
        // 长度形状：nonce 24 + 明文 + tag 16（base64 前的字节数）。
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&boxed)
            .unwrap();
        assert_eq!(raw.len(), NONCE_LEN + plaintext.len() + TAG_LEN);
        // 错误密钥。
        let other = rand::random::<[u8; 32]>();
        assert!(open(&other, &boxed).is_none());
        // 篡改末位（tag）。
        let mut tampered = boxed.clone();
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'A' { 'B' } else { 'A' });
        assert!(open(&k, &tampered).is_none(), "篡改后 AEAD 须失败");
        // 非 base64 / 过短。
        assert!(open(&k, "not-base64!!!").is_none());
        assert!(open(&k, "AAAA").is_none());
    }

    /// 会话表：逐 sid 加密出站（每 sid 一份 enc 帧，box 互不相同）；
    /// 无 sid 的入站试解命中正确会话并确认；同 sid 重握手覆盖；上限 8
    /// 淘汰最旧；半开 60s 清理、已确认豁免。
    #[test]
    fn sessions_seal_open_cap_and_prune() {
        let t0 = Instant::now();
        let mut s = E2eSessions::new();
        assert!(s.is_empty());
        // 无会话 → seal_each 空（明文兼容路径由调用方兜底）。
        assert!(s.seal_each("m", r#"{"v":1,"type":"ping"}"#).is_empty());

        let k1 = rand::random::<[u8; 32]>();
        let k2 = rand::random::<[u8; 32]>();
        s.insert("sid-1", &k1, t0);
        s.insert("sid-2", &k2, t0);
        assert_eq!(s.len(), 2);

        // 出站：两份 enc 帧，sid 各异、box 各异（不同密钥）。
        let frames = s.seal_each("home-mac", r#"{"v":1,"type":"text","text":"hi"}"#);
        assert_eq!(frames.len(), 2);
        let mut sids = Vec::new();
        for f in &frames {
            let v: serde_json::Value = serde_json::from_str(f).unwrap();
            assert_eq!(v["v"], 1);
            assert_eq!(v["type"], "enc");
            assert_eq!(v["machine"], "home-mac");
            assert!(v["box"].as_str().is_some_and(|b| !b.is_empty()));
            sids.push(v["sid"].as_str().unwrap().to_string());
        }
        sids.sort();
        assert_eq!(sids, vec!["sid-1".to_string(), "sid-2".to_string()]);

        // 入站试解：用 sid-1 的密钥密封 → open_any 命中。
        let boxed = seal(&k1, r#"{"v":1,"type":"ping"}"#);
        assert_eq!(
            s.open_any(&boxed).as_deref(),
            Some(r#"{"v":1,"type":"ping"}"#)
        );
        // 全部密钥不匹配 → None。
        assert!(s
            .open_any(&seal(&rand::random::<[u8; 32]>(), "x"))
            .is_none());

        // 半开清理：t0+61s 时 sid-2（未确认）被清，sid-1（已确认）保留。
        s.prune(t0 + HALF_OPEN_TTL + Duration::from_secs(1));
        assert_eq!(s.len(), 1);
        let frames = s.seal_each("m", "p");
        assert_eq!(frames.len(), 1);
        assert!(frames[0].contains("\"sid\":\"sid-1\""));

        // 上限 8：再插 7 个（sid-1 占 1 席）→ 满 8；第 9 个淘汰最旧。
        for i in 2..=8 {
            s.insert(
                &format!("sid-{i}"),
                &rand::random::<[u8; 32]>(),
                t0 + Duration::from_secs(i as u64),
            );
        }
        assert_eq!(s.len(), 8);
        s.insert(
            "sid-9",
            &rand::random::<[u8; 32]>(),
            t0 + Duration::from_secs(99),
        );
        assert_eq!(s.len(), 8, "满 8 淘汰一入一出");
        assert!(
            s.seal_each("m", "p")
                .iter()
                .all(|f| !f.contains("\"sid\":\"sid-1\"")),
            "最旧（sid-1，t0 建入）被淘汰"
        );

        // 同 sid 重握手覆盖（密钥更新，条目数不涨）。
        let before = s.len();
        s.insert(
            "sid-9",
            &rand::random::<[u8; 32]>(),
            t0 + Duration::from_secs(100),
        );
        assert_eq!(s.len(), before);
    }

    /// 握手全流程：e2e_hello → e2e_ack（ack 携带 ik=认证公钥；浏览器视角
    /// 以帧内 ik 自洽验签——TOFU 未 pin 路径 + 两侧派生一致 + 会话建入）；
    /// 畸形 hello 各分支忽略。
    #[test]
    fn handshake_produces_verifiable_ack() {
        use ed25519_dalek::Verifier;
        let signing = SigningKey::from_bytes(&rand::random::<[u8; 32]>());
        let dir = std::env::temp_dir().join(format!("imagent-web-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("web-e2e-key");
        let id = E2eIdentity::load_or_generate(&path, signing.clone()).unwrap();

        // 浏览器侧临时密钥。
        let browser_secret = XStaticSecret::from(rand::random::<[u8; 32]>());
        let browser_public = XPublicKey::from(&browser_secret);
        let n1 = rand::random::<[u8; 32]>();
        let sid = "11111111-2222-3333-4444-555555555555";
        let hello = serde_json::json!({
            "v": 1, "type": "e2e_hello", "machine": "home-mac", "sid": sid,
            "ek": hex::encode(browser_public.as_bytes()),
            "n1": hex::encode(n1),
        });
        let mut sessions = E2eSessions::new();
        let ack = id
            .handle_hello(&mut sessions, &hello, "home-mac")
            .expect("合法 hello 应产 ack");
        let _ = std::fs::remove_file(&path);

        let ack: serde_json::Value = serde_json::from_str(&ack).unwrap();
        assert_eq!(ack["v"], 1);
        assert_eq!(ack["type"], "e2e_ack");
        assert_eq!(ack["machine"], "home-mac");
        assert_eq!(ack["gk"], hex::encode(id.public.as_bytes()));
        // ik：TOFU pin 身份公钥来源，须随 ack 携带且与认证公钥一致
        //（SPA 解析兼容 ik/id/pk 别名，但缺任一身份公钥字段即握手失败）。
        let ik = ack["ik"]
            .as_str()
            .expect("ack 须携带 ik（SPA TOFU pin 依赖）");
        assert_eq!(ik, hex::encode(signing.verifying_key().to_bytes()));
        let n2 = hex::decode(ack["n2"].as_str().unwrap()).unwrap();
        assert_eq!(n2.len(), 32);
        let n2_arr: [u8; 32] = n2.as_slice().try_into().unwrap();

        // 验签对象 = ek||gk||n1||n2 原始字节（浏览器视角独立重构，并用帧内
        // ik 自洽验签——SPA 未 pin 首连路径；已 pin 则用 pin 表里的同一 ik）。
        let gk = hex::decode(ack["gk"].as_str().unwrap()).unwrap();
        let mut signed = Vec::with_capacity(128);
        signed.extend_from_slice(browser_public.as_bytes());
        signed.extend_from_slice(&gk);
        signed.extend_from_slice(&n1);
        signed.extend_from_slice(&n2);
        let sig = <[u8; 64]>::try_from(hex::decode(ack["sig"].as_str().unwrap()).unwrap()).unwrap();
        let ik_vk = ed25519_dalek::VerifyingKey::from_bytes(
            &<[u8; 32]>::try_from(hex::decode(ik).unwrap()).unwrap(),
        )
        .unwrap();
        ik_vk
            .verify(&signed, &ed25519_dalek::Signature::from_bytes(&sig))
            .expect("浏览器须能用 ack 携带的 ik 验签");

        // 两侧派生一致：浏览器用（ek_priv, gk, n1, n2）派生的 k 能解网关
        // 用会话表密封的 enc box（网关侧 k 来自 X25519(gw_priv, ek)）。
        let k_browser = derive_session_key(
            browser_secret
                .diffie_hellman(&XPublicKey::from(<[u8; 32]>::try_from(gk).unwrap()))
                .as_bytes(),
            &n1,
            &n2_arr,
        );
        assert_eq!(sessions.len(), 1);
        let frames = sessions.seal_each("home-mac", r#"{"v":1,"type":"text","text":"密"}"#);
        assert_eq!(frames.len(), 1);
        let f: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(f["sid"], sid, "enc 帧定向路由键 = 握手 sid");
        let pt = open(&k_browser, f["box"].as_str().unwrap()).expect("浏览器侧密钥须能开网关 box");
        assert!(pt.contains(r#""text":"密""#));

        // 畸形分支：越机 / 坏 ek / 坏 n1 / 缺 ek。
        let mut bad = hello.clone();
        bad["machine"] = "other-mac".into();
        assert!(id.handle_hello(&mut sessions, &bad, "home-mac").is_none());
        let mut bad = hello.clone();
        bad["ek"] = "zz".into();
        assert!(id.handle_hello(&mut sessions, &bad, "home-mac").is_none());
        let mut bad = hello.clone();
        bad["n1"] = "abcd".into(); // 长度错
        assert!(id.handle_hello(&mut sessions, &bad, "home-mac").is_none());
        let mut bad = hello.clone();
        bad.as_object_mut().unwrap().remove("ek");
        assert!(id.handle_hello(&mut sessions, &bad, "home-mac").is_none());

        // sid 缺失 → 广播模式（兼容形态）：ack 照发，会话以空 sid 建入，
        // 出站 enc 帧 sid=""（relay 广播）且同密钥可解。
        let mut compat = hello.clone();
        compat.as_object_mut().unwrap().remove("sid");
        let ack = id
            .handle_hello(&mut sessions, &compat, "home-mac")
            .expect("无 sid 的 e2e_hello 应回退广播模式，不拒绝");
        assert!(ack.contains(r#""type":"e2e_ack""#));
        assert_eq!(sessions.len(), 2, "定向 + 广播两个会话并存");
        let frames = sessions.seal_each("home-mac", "x");
        assert_eq!(frames.len(), 2);
        assert!(
            frames.iter().any(|f| f.contains(r#""sid":"""#)),
            "广播模式会话的 enc 帧 sid 为空串：{frames:?}"
        );
    }

    /// HKDF-SHA256 `new(None)`（空 salt）与 32 零字节 salt 等价（RFC 5869
    /// 缺省盐；HMAC 补零后同键）——SPA 侧 @noble 两种写法都得到同一 k。
    #[test]
    fn hkdf_none_salt_equals_zero_salt() {
        let ikm = rand::random::<[u8; 96]>();
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        Hkdf::<Sha256>::new(None, &ikm)
            .expand(HKDF_INFO, &mut a)
            .unwrap();
        Hkdf::<Sha256>::new(Some(&[0u8; 32]), &ikm)
            .expand(HKDF_INFO, &mut b)
            .unwrap();
        assert_eq!(a, b);
    }
}
