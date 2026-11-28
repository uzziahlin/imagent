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
//!    模式**（兼容形态：会话以空 sid 建入，enc 帧广播全部浏览器、仅持钥方
//!    可解；带 sid 的定向模式才是完整形态）。同一登录会话（同 sid）可以有
//!    多个标签页各自握手——每个标签页一把临时密钥，网关**全部保留、互不
//!    顶替**（见下文「多标签页」）。
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
//! 浏览器方向必带；浏览器→网关方向无 sid（逐把密钥按内容试解）>",
//! "box":"<base64>"}`；box = base64( 24B 随机 nonce || XChaCha20-Poly1305
//! 密文（明文 || 16B tag)），明文 = 原业务帧完整 JSON（UTF-8），无 AAD。
//!
//! ## 多标签页（v1.44）：每个登录会话保留多把密钥
//!
//! 所有标签页共用 localStorage 的登录会话，握手带的是**同一个 sid**，但每个
//! 标签页各自建 WS 连接、各自生成临时密钥。v1.43 及之前会话表是
//! `sid → 单把密钥`，同 sid 第二次握手**覆盖**旧密钥——被顶掉的标签页界面
//! 完全健康（状态条「已连接 / 已加密」），实际发出的帧被丢弃、收到的帧解
//! 不开，静默失效。relay 本来就允许同 sid 多条连接并存并把 `enc{sid}` 投给
//! 该会话的**全部**连接，所以修网关的会话模型就够了，不需要前端选主：
//! 表结构改为 `sid → Vec<密钥>`，握手**追加**不覆盖，出站**每把密钥各密封
//! 一份**（`seal_each`），每个标签页只解得开自己那份；入站不带 sid，逐把
//! 试解（`open_any`）。旧 SPA 不用改也不再被顶掉。
//!
//! 密钥的退场有三条路，缺一不可：
//! - `e2e_close`（封在 enc 里的业务帧，`{"v":1,"type":"e2e_close"}`）：SPA 在
//!   `pagehide` 时发，网关删除**解开它的那把**密钥、不回帧（正常关闭）；
//! - **空闲回收**：标签页崩溃 / 被 kill 时发不出 `e2e_close`，而标签页关闭
//!   对网关不可见（relay 不通知）——`prune` 把 `last_seen` 距今超过
//!   [`CONFIRMED_IDLE_TTL`]（600s）的已确认密钥移除。不回收则每把残留密钥
//!   都要让每个出站帧多密封一份、relay 多投一份。已连接的标签页每 25s 对
//!   每台在线机器发一帧 enc-ping（后台标签页被浏览器节流也在分钟级），600s
//!   远大于此；被冻结的标签页恢复时 WS 已断、会重新握手；
//! - **容量淘汰**：同会话超过 [`MAX_KEYS_PER_SESSION`]、全表超过
//!   [`MAX_KEYS_TOTAL`] 时淘汰最久未活跃的一把（兜底，正常不会触达）。
//!
//! ## 兼容与降级语义
//!
//! - **无任何握手密钥 → 出站行为不变（明文）**：加密是逐浏览器渐进的，
//!   旧 SPA 连接继续工作；
//! - **有密钥 → 每业务帧逐把密钥各加密一份**（enc{sid}，同会话每个标签页
//!   一份），不再发对应明文——否则「任一旧浏览器在线就全局明文」违背盲化
//!   目标；
//! - 网关重连 relay 即清空会话表（per-relay-连接重建，client 侧按连接重建），
//!   浏览器随后重连重握手；
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
/// 单个登录会话（一个 sid）可保留的密钥数上限：同会话的每个标签页 /
/// 浏览器连接各占一把（对齐 relay 的同会话浏览器并存上限 `MAX_BROWSERS`
/// = 8）。超过时淘汰该会话里最久未活跃的一把。
pub(crate) const MAX_KEYS_PER_SESSION: usize = 8;
/// 全表密钥总数上限（所有登录会话合计）。每把密钥让每个出站业务帧多一次
/// AEAD 加密 + relay 多投一份、每条入站 enc 帧多一次试解，须有硬上界
/// （防握手洪水）。16 = 两台设备各开满 8 个标签页；真机常态 2~5 台设备、
/// 每台 1~2 个标签页。超过时淘汰全表最久未活跃的一把。
pub(crate) const MAX_KEYS_TOTAL: usize = 16;
/// 已确认密钥的空闲回收窗口：`last_seen`（最近一次成功入站解密）距今超过
/// 本值的密钥被 [`E2eSessions::prune`] 移除。标签页关闭对网关不可见，崩溃 /
/// 被 kill 的标签页发不出 `e2e_close`，不回收则残留密钥永久占位。取值依据：
/// 已连接的标签页每 25s 对每台在线机器发一帧 enc-ping（后台标签页被浏览器
/// 节流也在分钟级），600s 远大于此，正常连接不会被误回收。
pub(crate) const CONFIRMED_IDLE_TTL: Duration = Duration::from_secs(600);
/// 握手半开清理窗口：e2e_ack 发出后 60s 内无一笔成功入站解密（浏览器
/// 握手后周期 enc-ping，正常秒级确认）即视为半开残留，清理（防探测
/// 洪水占满会话表）。已确认会话不按此清理（见 [`E2eSessions::prune`]）。
/// 浏览器在 TOFU 指纹确认弹层期间不发 enc 帧（心跳明文、业务帧阻断），
/// 停留超过本窗口的握手会被清理——这是预期行为：SPA 在用户确认后以
/// pin 身份重新握手（v1.41.9），不复用弹层期间的密钥，故本清理不影响
/// 首连。
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

/// E2E 会话表（v1.44 多密钥模型）：`sid → 该登录会话的全部密钥`。同一个
/// sid 下的每个标签页 / 浏览器连接各一把（握手**追加**不覆盖，理由见模块头
/// 「多标签页」）。密钥生命周期：e2e_hello 建入（半开）→ 首笔成功入站解密
/// 转确认 → `e2e_close` 删除 / 空闲回收 / 容量淘汰；网关重连 relay 整体清空
/// （client 侧按连接重建）。**半开 60s 清理**只针对未确认密钥（防握手洪水
/// 占表）；已确认密钥按 [`CONFIRMED_IDLE_TTL`] 空闲回收——标签页关闭对网关
/// 不可见，不回收则残留密钥永久占位。
pub(crate) struct E2eSessions {
    entries: HashMap<String, Vec<Entry>>,
    /// 本连接生命周期内是否加密过出站帧（「回退明文」降级观测，warn 一次）。
    /// [`Cell`]：seal_each 走只读出站路径（&self）也要能记录。
    ever_encrypted: std::cell::Cell<bool>,
}

struct Entry {
    key: [u8; 32],
    created: Instant,
    /// 是否已有成功入站解密（浏览器持正确密钥的证明；半开清理豁免条件）。
    confirmed: bool,
    /// 最近一次成功入站解密的时刻（建入时 = created）。已确认密钥的空闲
    /// 回收依据，也是容量淘汰的排序键。
    last_seen: Instant,
}

/// [`E2eSessions::open_any`] 的成功结果：解开这帧的那把密钥及其所属登录
/// 会话——client 层处理 `e2e_close` 时据 `(sid, key)` 精确删除**那一把**。
pub(crate) struct Opened {
    pub(crate) sid: String,
    pub(crate) key: [u8; 32],
    pub(crate) plaintext: String,
}

/// 日志用 sid 前 8 位（完整 sid 是登录会话 id，不进日志）。client 层的
/// e2e_close 日志同用它。
pub(crate) fn short_sid(sid: &str) -> String {
    sid.chars().take(8).collect()
}

/// bucket 内最久未活跃一把的下标（按 `last_seen`，同值按 `created`，再同按
/// 密钥字节——完全确定，测试可复现）。调用方保证 bucket 非空。
fn index_of_least_recently_active(bucket: &[Entry]) -> usize {
    let mut best = 0;
    for i in 1..bucket.len() {
        let (cand, cur) = (&bucket[i], &bucket[best]);
        if (cand.last_seen, cand.created, cand.key) < (cur.last_seen, cur.created, cur.key) {
            best = i;
        }
    }
    best
}

impl E2eSessions {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            ever_encrypted: std::cell::Cell::new(false),
        }
    }

    /// 密钥**总数**（所有登录会话合计；测试 / 日志用）——不是登录会话数。
    pub(crate) fn len(&self) -> usize {
        self.entries.values().map(Vec::len).sum()
    }

    /// 一把密钥都没有（「回退明文」降级观测与明文兼容路径的判据）。
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 某登录会话当前保留的密钥数（测试 / 日志用）；会话不存在为 0。
    pub(crate) fn keys_for_session(&self, sid: &str) -> usize {
        self.entries.get(sid).map_or(0, Vec::len)
    }

    /// 建入一把密钥（**追加**到该 sid，不覆盖同 sid 的其它密钥）。时间由
    /// 调用方注入（单测可拨钟）。两级容量上限（同会话 / 全表）都在**插入
    /// 之前**腾位：保证本函数返回后新密钥必在表里——handle_hello 随即把
    /// e2e_ack 发给浏览器，密钥若不在表里，浏览器永远解不开网关出站帧。
    /// 被淘汰的永远是旧条目里最久未活跃的一把。
    pub(crate) fn insert(&mut self, sid: &str, key: &[u8; 32], now: Instant) {
        // ① 同会话超限：淘汰该会话最久未活跃的一把（计数在删之前取——日志
        // 报的是触发淘汰时的水位）。
        let total_before = self.len();
        if let Some(bucket) = self.entries.get_mut(sid) {
            if bucket.len() >= MAX_KEYS_PER_SESSION {
                let session_keys = bucket.len();
                let i = index_of_least_recently_active(bucket);
                bucket.remove(i);
                warn!(
                    target: TARGET,
                    sid = %short_sid(sid),
                    session_keys,
                    total_keys = total_before,
                    "E2E 会话内密钥数达上限（{}），淘汰该会话最久未活跃的一把",
                    MAX_KEYS_PER_SESSION
                );
            }
        }
        // ② 全表超限：表已满则先淘汰全表最久未活跃的一把，再插入。
        if self.len() >= MAX_KEYS_TOTAL {
            if let Some((victim, i)) = self.least_recently_active() {
                warn!(
                    target: TARGET,
                    sid = %short_sid(&victim),
                    session_keys = self.keys_for_session(&victim),
                    total_keys = self.len(),
                    "E2E 密钥表满（{}），淘汰全表最久未活跃的一把",
                    MAX_KEYS_TOTAL
                );
                self.remove_at(&victim, i);
            }
        }
        // ③ 追加。
        self.entries
            .entry(sid.to_string())
            .or_default()
            .push(Entry {
                key: *key,
                created: now,
                confirmed: false,
                last_seen: now,
            });
    }

    /// 全表最久未活跃的一把：`(sid, bucket 内下标)`。空表 → None。排序键与
    /// [`index_of_least_recently_active`] 同款再按 sid 字典序兜底。
    fn least_recently_active(&self) -> Option<(String, usize)> {
        self.entries
            .iter()
            .flat_map(|(sid, bucket)| {
                bucket
                    .iter()
                    .enumerate()
                    .map(move |(i, e)| (sid.as_str(), i, e))
            })
            .min_by_key(|(sid, _, e)| (e.last_seen, e.created, *sid, e.key))
            .map(|(sid, i, _)| (sid.to_string(), i))
    }

    /// 按 (sid, 下标) 删除一把；会话的密钥删空则移除该 sid。
    fn remove_at(&mut self, sid: &str, i: usize) {
        if let Some(bucket) = self.entries.get_mut(sid) {
            bucket.remove(i);
            if bucket.is_empty() {
                self.entries.remove(sid);
            }
        }
    }

    /// 出站逐密钥加密：表里**每把密钥**各密封一份（enc{sid} 帧，信封 `sid`
    /// 是所属登录会话 id）——同会话的每个标签页各得一份、只解得开自己那份。
    /// **空表返回空 vec**——调用方据此走明文兼容路径（无任何握手密钥时
    /// 行为不变）。输出顺序确定：sid 字典序，同会话内按 `created` 升序
    /// （HashMap 迭代序随机，而帧字节可复现是本 crate 的既有纪律）。
    pub(crate) fn seal_each(&self, machine: &str, plaintext: &str) -> Vec<String> {
        if !self.is_empty() {
            // ever_encrypted 由 take_plaintext_revert 复位；Cell 让只读出站
            // 路径（&self）也能记录，不为它改 &mut 签名。
            self.ever_encrypted.set(true);
        }
        let mut sids: Vec<&String> = self.entries.keys().collect();
        sids.sort();
        let mut frames = Vec::with_capacity(self.len());
        for sid in sids {
            let mut bucket: Vec<&Entry> = self.entries[sid].iter().collect();
            bucket.sort_by_key(|e| (e.created, e.key));
            for e in bucket {
                frames.push(
                    serde_json::json!({
                        "v": 1,
                        "type": "enc",
                        "machine": machine,
                        "sid": sid,
                        "box": seal(&e.key, plaintext),
                    })
                    .to_string(),
                );
            }
        }
        frames
    }

    /// 入站试解（浏览器→网关 enc 帧无 sid——单网关连接复用，按内容试解）：
    /// 逐把密钥尝试 AEAD 开启（≤ [`MAX_KEYS_TOTAL`] 次，微秒级；不按内容
    /// 前缀选密钥——密文对无密钥方是随机的，无可用前缀信息，全试是最简
    /// 正确解）。遍历顺序与结果无关：不同密钥的 AEAD 恰有一把能解开。成功
    /// 即标记该密钥 confirmed（半开清理豁免）、`last_seen = now`（空闲回收
    /// 重新计时），并返回是哪把密钥、属于哪个登录会话（`e2e_close` 据此精确
    /// 删除）。全部失败 → None（调用方 warn 丢弃）。
    pub(crate) fn open_any(&mut self, boxed: &str, now: Instant) -> Option<Opened> {
        for (sid, bucket) in self.entries.iter_mut() {
            for e in bucket.iter_mut() {
                if let Some(plaintext) = open(&e.key, boxed) {
                    e.confirmed = true;
                    e.last_seen = now;
                    return Some(Opened {
                        sid: sid.clone(),
                        key: e.key,
                        plaintext,
                    });
                }
            }
        }
        None
    }

    /// 删除指定的一把密钥（`e2e_close`：解开它的那把）。返回是否真的删了
    /// （密钥已不在表里 → false，幂等）。会话的密钥删空则移除该 sid。
    pub(crate) fn remove(&mut self, sid: &str, key: &[u8; 32]) -> bool {
        let Some(bucket) = self.entries.get_mut(sid) else {
            return false;
        };
        let Some(i) = bucket.iter().position(|e| e.key == *key) else {
            return false;
        };
        bucket.remove(i);
        if bucket.is_empty() {
            self.entries.remove(sid);
        }
        true
    }

    /// 清理（时间注入；调用方在心跳 tick 顺带执行，无需独立定时器）：
    /// - **半开清理**（不变）：未确认 && 创建超 [`HALF_OPEN_TTL`] 的密钥移除；
    /// - **空闲回收**（v1.44）：已确认 && `last_seen` 距今超过
    ///   [`CONFIRMED_IDLE_TTL`] 的密钥移除（标签页关闭对网关不可见，见模块头
    ///   「多标签页」）。恰等于 TTL 不回收（600s 不回收、601s 回收）。
    ///
    /// 密钥删空的会话一并移除。
    pub(crate) fn prune(&mut self, now: Instant) {
        let mut reaped = 0usize;
        self.entries.retain(|_, bucket| {
            bucket.retain(|e| {
                let half_open = !e.confirmed && now.duration_since(e.created) >= HALF_OPEN_TTL;
                let idle =
                    e.confirmed && now.saturating_duration_since(e.last_seen) > CONFIRMED_IDLE_TTL;
                if idle {
                    reaped += 1;
                }
                !(half_open || idle)
            });
            !bucket.is_empty()
        });
        if reaped > 0 {
            info!(
                target: TARGET,
                reaped,
                remaining = self.len(),
                "E2E 已确认密钥空闲超时（{}s），回收（标签页已关闭 / 崩溃，未能发 e2e_close）",
                CONFIRMED_IDLE_TTL.as_secs()
            );
        }
    }

    /// 「加密→明文」回退观测（client warn 一次）：本连接曾加密过出站帧且
    /// 当前会话表已空（一把密钥都不剩）→ true（并复位，仅告警首次）。正常
    /// 触发 = 全部浏览器断开且密钥被 e2e_close / 空闲回收 / 淘汰；异常触发 =
    /// 恶意中继丢弃浏览器 enc 帧使半开会话 60s 清理（模块头注释的已知残留
    /// 风险）——观测点即为此设。
    pub(crate) fn take_plaintext_revert(&mut self) -> bool {
        if self.ever_encrypted.get() && self.is_empty() {
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
        // 未携带 sid 的 SPA 形态，warn 提示）。见模块头注释的偏离说明。
        // v1.44：两种模式下同 sid 的多次握手都是**追加**一把密钥（同一登录
        // 会话的多个标签页 / 多个无 sid 浏览器各持一把，互不顶替）。
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
            sid = %short_sid(&sid),
            session_keys = sessions.keys_for_session(&sid),
            total_keys = sessions.len(),
            fingerprint = %self.fingerprint(),
            "E2E 握手完成（会话密钥已追加建入，等浏览器首笔 enc 帧确认）"
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

    /// 测试助手：表里是否还持有某把密钥（经 seal_each + 用该密钥解，不触碰
    /// last_seen——open_any 成功会刷新活跃时刻，断言「是否被回收」时不能用）。
    fn holds(s: &E2eSessions, key: &[u8; 32]) -> bool {
        s.seal_each("m", "p")
            .iter()
            .any(|f| open(key, &boxed_of(f)).is_some())
    }

    /// 测试助手：从 enc 帧原文取 box 字段。
    fn boxed_of(frame: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(frame).unwrap();
        v["box"].as_str().unwrap().to_string()
    }

    /// 测试助手：enc 帧原文 → (sid, box)。
    fn sid_box_of(frame: &str) -> (String, String) {
        let v: serde_json::Value = serde_json::from_str(frame).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "enc");
        (
            v["sid"].as_str().unwrap().to_string(),
            v["box"].as_str().unwrap().to_string(),
        )
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// v1.44 多标签页：同 sid 两次握手后**两把密钥都在**（追加不覆盖）；
    /// `seal_each` 每把密钥各一份，各自只能被对应密钥解开；输出顺序确定
    /// （sid 字典序，同会话内 created 升序）；信封 `sid` 是所属登录会话。
    #[test]
    fn sessions_same_sid_keeps_both_keys_and_seal_each_is_ordered() {
        let t0 = Instant::now();
        let mut s = E2eSessions::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        // 空表 → seal_each 空（明文兼容路径由调用方兜底）。
        assert!(s.seal_each("m", r#"{"v":1,"type":"ping"}"#).is_empty());

        let k1 = rand::random::<[u8; 32]>();
        let k2 = rand::random::<[u8; 32]>();
        s.insert("sid-1", &k1, t0);
        // 同 sid 第二次握手（另一标签页）→ 追加。
        s.insert("sid-1", &k2, t0 + secs(1));
        assert_eq!(s.len(), 2, "同 sid 两次握手后两把密钥都在（不覆盖）");
        assert_eq!(s.keys_for_session("sid-1"), 2);
        assert!(!s.is_empty());

        // 另一个登录会话：字典序在 sid-1 之前，但 created 最晚——出站按 sid
        // 排序而不是按创建先后。
        let k_other = rand::random::<[u8; 32]>();
        s.insert("sid-0", &k_other, t0 + secs(2));

        let plaintext = r#"{"v":1,"type":"text","text":"hi"}"#;
        let frames = s.seal_each("home-mac", plaintext);
        assert_eq!(frames.len(), 3, "每把密钥各一份");
        let opened: Vec<(String, String)> = frames.iter().map(|f| sid_box_of(f)).collect();
        for f in &frames {
            let v: serde_json::Value = serde_json::from_str(f).unwrap();
            assert_eq!(v["machine"], "home-mac");
        }
        // 顺序：sid-0 一份；sid-1 两份，先 k1（created 早）后 k2。
        assert_eq!(opened[0].0, "sid-0");
        assert_eq!(opened[1].0, "sid-1");
        assert_eq!(opened[2].0, "sid-1");
        assert_eq!(
            open(&k_other, &opened[0].1).as_deref(),
            Some(plaintext),
            "sid-0 那份只有 k_other 能解"
        );
        assert_eq!(
            open(&k1, &opened[1].1).as_deref(),
            Some(plaintext),
            "同会话内 created 升序：第 2 份是 k1 的"
        );
        assert_eq!(
            open(&k2, &opened[2].1).as_deref(),
            Some(plaintext),
            "第 3 份是 k2 的"
        );
        // 交叉解不开（每份只有对应密钥能解——标签页之间互不可读）。
        assert!(open(&k2, &opened[1].1).is_none());
        assert!(open(&k1, &opened[2].1).is_none());
        assert!(open(&k1, &opened[0].1).is_none());
    }

    /// v1.44 `open_any` 返回解开这帧的密钥与所属登录会话；成功即置 confirmed、
    /// 把 last_seen 推到注入的 now（经 prune 的回收边界反证 last_seen 确实
    /// 更新）；全部失败 → None。
    #[test]
    fn open_any_returns_the_key_and_touches_it() {
        let t0 = Instant::now();
        let mut s = E2eSessions::new();
        let k1 = rand::random::<[u8; 32]>();
        let k2 = rand::random::<[u8; 32]>();
        s.insert("sid-1", &k1, t0);
        s.insert("sid-2", &k2, t0);

        let ping = r#"{"v":1,"type":"ping"}"#;
        let o = s
            .open_any(&seal(&k1, ping), t0 + secs(10))
            .expect("k1 密封的帧应解开");
        assert_eq!(o.sid, "sid-1");
        assert_eq!(o.key, k1);
        assert_eq!(o.plaintext, ping);

        let o = s.open_any(&seal(&k2, "x"), t0 + secs(20)).unwrap();
        assert_eq!(o.sid, "sid-2");
        assert_eq!(o.key, k2);
        assert_eq!(o.plaintext, "x");

        // 谁的密钥都不是 → None（且不改变任何条目）。
        assert!(s
            .open_any(&seal(&rand::random::<[u8; 32]>(), "x"), t0 + secs(30))
            .is_none());
        assert_eq!(s.len(), 2);

        // last_seen 可观察：k1 停在 t0+10s、k2 停在 t0+20s。在 t0+20s+600s
        // 时 prune——k1 空闲已 610s 被回收，k2 恰 600s 保留。
        s.prune(t0 + secs(20) + CONFIRMED_IDLE_TTL);
        assert_eq!(
            s.keys_for_session("sid-1"),
            0,
            "k1 的 last_seen 应是 t0+10s"
        );
        assert_eq!(
            s.keys_for_session("sid-2"),
            1,
            "k2 的 last_seen 应是 t0+20s"
        );
    }

    /// v1.44 `remove`（e2e_close）只删指定的那一把：之后出站只剩一份、旧密钥
    /// 的密文 open_any 失败、同会话另一把照常收发；幂等；删空则会话消失、
    /// 表空后 seal_each 回到空（明文兼容路径）；回退明文观测语义不变。
    #[test]
    fn remove_drops_only_that_key() {
        let t0 = Instant::now();
        let mut s = E2eSessions::new();
        let k1 = rand::random::<[u8; 32]>();
        let k2 = rand::random::<[u8; 32]>();
        s.insert("sid-1", &k1, t0);
        s.insert("sid-1", &k2, t0);

        assert!(s.remove("sid-1", &k1));
        assert_eq!(s.len(), 1);
        assert_eq!(s.keys_for_session("sid-1"), 1);

        let frames = s.seal_each("m", "p");
        assert_eq!(frames.len(), 1, "只剩 k2 那一份");
        let (_, boxed) = sid_box_of(&frames[0]);
        assert!(open(&k2, &boxed).is_some());
        assert!(open(&k1, &boxed).is_none());

        // 旧密钥（k1）的密文解不开；k2 仍能入站。
        assert!(s.open_any(&seal(&k1, "x"), t0).is_none());
        assert!(s.open_any(&seal(&k2, "y"), t0).is_some());

        // 幂等：已删的 / 不存在的会话 → false，表不变。
        assert!(!s.remove("sid-1", &k1));
        assert!(!s.remove("sid-nope", &k2));
        assert_eq!(s.len(), 1);

        // 删最后一把：会话消失、表空。
        assert!(s.remove("sid-1", &k2));
        assert!(s.is_empty());
        assert_eq!(s.keys_for_session("sid-1"), 0);
        assert!(s.seal_each("m", "p").is_empty(), "表空 → 明文兼容路径");
        // 回退明文观测：上面 seal_each 过（曾加密）且表现在空 → 恰一次 true。
        assert!(s.take_plaintext_revert(), "曾加密 + 表空 → 观测到回退");
        assert!(!s.take_plaintext_revert(), "仅告警首次");
    }

    /// v1.44 同会话上限：第 9 把淘汰该会话里**最久未活跃**的一把（按
    /// last_seen，不是按 created）——把 created 最小的那把刷新成最近活跃，
    // 被淘汰的应是次旧的 k1。时间全用注入的 now 推进。
    #[test]
    fn per_session_cap_evicts_least_recently_active() {
        let t0 = Instant::now();
        let mut s = E2eSessions::new();
        let keys: Vec<[u8; 32]> = (0..MAX_KEYS_PER_SESSION)
            .map(|i| {
                let k = rand::random::<[u8; 32]>();
                s.insert("sid-1", &k, t0 + secs(i as u64));
                k
            })
            .collect();
        assert_eq!(s.keys_for_session("sid-1"), MAX_KEYS_PER_SESSION);
        assert_eq!(s.len(), MAX_KEYS_PER_SESSION);

        // created 最小的 keys[0] 在 t0+100s 有入站（最近活跃）；其余停在各自
        // created（≤ t0+7s）→ 最久未活跃的是 keys[1]。
        assert!(s.open_any(&seal(&keys[0], "x"), t0 + secs(100)).is_some());

        let k_new = rand::random::<[u8; 32]>();
        s.insert("sid-1", &k_new, t0 + secs(200));
        assert_eq!(
            s.keys_for_session("sid-1"),
            MAX_KEYS_PER_SESSION,
            "一入一出"
        );
        assert_eq!(s.len(), MAX_KEYS_PER_SESSION);

        assert!(
            !holds(&s, &keys[1]),
            "最久未活跃的 keys[1] 被淘汰（不是 created 最小的 keys[0]）"
        );
        assert!(holds(&s, &keys[0]), "最近活跃过的 keys[0] 保留");
        for k in &keys[2..] {
            assert!(holds(&s, k), "其余密钥保留：{k:?}");
        }
        assert!(holds(&s, &k_new), "新插入的密钥必在表里");
    }

    /// v1.44 全表上限：第 17 把淘汰**全表**最久未活跃的一把（跨会话；按
    /// last_seen 不是 created）——两个会话各 8 把填满，把全表最旧的刷新成
    /// 最近活跃，新登录会话握手后被淘汰的应是次旧的那把。
    #[test]
    fn total_cap_evicts_global_least_recently_active() {
        let t0 = Instant::now();
        let mut s = E2eSessions::new();
        let mut ka = Vec::new();
        let mut kb = Vec::new();
        for i in 0..MAX_KEYS_PER_SESSION {
            let k = rand::random::<[u8; 32]>();
            s.insert("sess-a", &k, t0 + secs(i as u64));
            ka.push(k);
        }
        for i in 0..MAX_KEYS_PER_SESSION {
            let k = rand::random::<[u8; 32]>();
            s.insert("sess-b", &k, t0 + secs(10 + i as u64));
            kb.push(k);
        }
        assert_eq!(s.len(), MAX_KEYS_TOTAL, "两个会话填满全表");

        // 全表 created 最小的 ka[0] 刷新为最近活跃 → 全表最久未活跃 = ka[1]。
        assert!(s.open_any(&seal(&ka[0], "x"), t0 + secs(100)).is_some());

        let kc = rand::random::<[u8; 32]>();
        s.insert("sess-c", &kc, t0 + secs(200));
        assert_eq!(s.len(), MAX_KEYS_TOTAL, "一入一出");
        assert_eq!(
            (
                s.keys_for_session("sess-a"),
                s.keys_for_session("sess-b"),
                s.keys_for_session("sess-c")
            ),
            (MAX_KEYS_PER_SESSION - 1, MAX_KEYS_PER_SESSION, 1)
        );
        assert!(!holds(&s, &ka[1]), "全表最久未活跃的 ka[1] 被淘汰");
        assert!(holds(&s, &ka[0]), "最近活跃过的 ka[0] 保留");
        assert!(holds(&s, &kb[0]), "另一会话的密钥不受影响");
        assert!(holds(&s, &kc), "新会话的密钥在表里");
    }

    /// v1.44 prune：已确认密钥空闲恰 600s 不回收、601s 回收；半开清理不受
    /// 影响（未确认且创建超 60s 的照旧清、未满 60s 的保留）；密钥删空的会话
    /// 一并移除。
    #[test]
    fn prune_reaps_confirmed_after_idle_ttl_keeps_half_open_rule() {
        let t0 = Instant::now();
        let mut s = E2eSessions::new();
        let k_ok = rand::random::<[u8; 32]>();
        let k_half = rand::random::<[u8; 32]>();
        let k_late_half = rand::random::<[u8; 32]>();
        s.insert("sid-1", &k_ok, t0);
        s.insert("sid-1", &k_half, t0);
        s.insert("sid-2", &k_late_half, t0 + secs(700));
        // k_ok 在 t0+100s 被确认（last_seen = t0+100s）。
        assert!(s.open_any(&seal(&k_ok, "x"), t0 + secs(100)).is_some());

        // now = t0+100s+600s = t0+700s：k_ok 空闲恰 TTL。
        let now = t0 + secs(100) + CONFIRMED_IDLE_TTL;
        s.prune(now);
        assert!(holds(&s, &k_ok), "空闲恰 600s 不回收");
        assert!(
            !holds(&s, &k_half),
            "半开清理不受影响：未确认且创建超 60s 的被清"
        );
        assert!(holds(&s, &k_late_half), "刚建的半开（未满 60s）保留");

        // 再过 1s：k_ok 空闲 601s → 回收；sid-1 删空则移除。
        s.prune(now + secs(1));
        assert!(!holds(&s, &k_ok), "空闲 601s 回收");
        assert!(holds(&s, &k_late_half));
        assert_eq!(s.keys_for_session("sid-1"), 0, "删空的会话移除");
        assert_eq!(s.len(), 1);

        // 半开清理照旧：t0+700s 建的 k_late_half 在创建满 60s 后被清。
        s.prune(t0 + secs(700) + HALF_OPEN_TTL);
        assert!(s.is_empty(), "半开 60s 清理不变");
    }

    /// v1.44 计数语义：`len` 是密钥总数（不是登录会话数）、`is_empty` 是一把
    /// 密钥都没有、`keys_for_session` 按会话计。
    #[test]
    fn len_is_empty_and_keys_for_session_count_keys() {
        let t0 = Instant::now();
        let mut s = E2eSessions::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert_eq!(s.keys_for_session("x"), 0);
        assert!(!s.take_plaintext_revert(), "从未加密过：不报回退");

        let k1 = rand::random::<[u8; 32]>();
        let k2 = rand::random::<[u8; 32]>();
        let k3 = rand::random::<[u8; 32]>();
        s.insert("s1", &k1, t0);
        s.insert("s1", &k2, t0);
        s.insert("s2", &k3, t0);
        assert_eq!(s.len(), 3, "密钥总数");
        assert!(!s.is_empty());
        assert_eq!(
            (
                s.keys_for_session("s1"),
                s.keys_for_session("s2"),
                s.keys_for_session("nope")
            ),
            (2, 1, 0)
        );

        // 删光 s1 的两把：总数随密钥走，s1 归零。
        s.remove("s1", &k1);
        s.remove("s1", &k2);
        assert_eq!(s.len(), 1);
        assert_eq!(s.keys_for_session("s1"), 0);
        assert!(!s.is_empty());
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
