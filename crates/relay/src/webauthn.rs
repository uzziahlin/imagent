//! passkey（WebAuthn）登录层：relay 作为 Relying Party，四个 HTTP 端点
//!（注册 begin/complete + 登录 begin/complete，Authorization: Bearer
//! `<sessionToken>` 认证注册路径）+ 凭证表落盘 + WS 侧 `cred_list` /
//! `cred_revoke` 管理帧（见 lib.rs 帧分拣）。
//!
//! 动机：USER_TOKEN 是静态共享秘密，手输体验差且泄露面大；passkey（平台
//! 认证器 / 同步钥匙串）把凭证留在设备上，服务器只存公钥。登录成功直接
//! 换发 L2 会话（[`SessionStore::issue`]，与 USER_TOKEN hello 换发同构），
//! USER_TOKEN 路径完整保留为恢复手段（登录页双入口）。
//!
//! 流程（任务定案）：
//! - **注册**（已登录会话才可注册新 passkey）：
//!   `POST /webauthn/register/begin`（Bearer sessionToken，body `{"label"?}`）
//!   → challenge（内存 challenge 表，60s 过期、单次消费）→ 浏览器
//!   `navigator.credentials.create` → `POST /webauthn/register/complete`
//!   （`{"beginId","credential"}`）→ webauthn-rs 验签 → 公钥入凭证表
//!   （`<state_dir>/credentials.json`，0600 原子写）。
//! - **登录**（免用户名）：
//!   `POST /webauthn/login/begin`（无认证）→ assertion challenge——
//!   **`allowCredentials` 下发为空**（discoverable credential / resident
//!   key 形态，浏览器免选用户）→ `navigator.credentials.get` →
//!   `POST /webauthn/login/complete` → 验签 → 直接创建 L2 会话，响应
//!   `{"session":{id,token,expiresAt}}`。
//!
//! rp_id / origin 推导（缺省）：
//! - rp_id = 请求 Host 去端口（公网 `relay.example.com:8443` →
//!   `relay.example.com`）；env `IMAGENT_RELAY_RP_ID` 可覆盖（固定
//!   rp_id 后换域名/端口不动凭证——WebAuthn 凭证绑定 rp_id，不是 origin）。
//! - origin = scheme + Host（含端口）：scheme 取 `X-Forwarded-Proto`（Caddy
//!   反代会带，仅认 http/https），缺省公网 https、loopback http。
//! - **本地 http 直连不可用 passkey**：`WebauthnBuilder` 要求 rp_id 是
//!   origin 的域名（IP 字面量过不了校验），且非 loopback 的 http 页面非
//!   secure context——本地调试走 USER_TOKEN 路径（`http://localhost:<port>`
//!   形态可用，rp_id=localhost）。
//!
//! 安全注意：
//! - challenge 表内存单次消费（take 即删）+ 60s 过期；重启失效 = 用户重新
//!   发起，无妨。
//! - begin 端点（注册/登录）per-IP 滑动窗口限流（公网形态直连对端恒为
//!   Caddy 127.0.0.1，取 `X-Forwarded-For` 首跳，缺省回落直连对端 IP）。
//! - 单用户语义（对齐 [`SessionStore::revoke`]）：任意 passkey 登录同一个
//!   用户；注册排除已注册 credential id（同认证器重复注册在浏览器侧即报
//!   excluded）。
//! - 登录 begin 的 state 持有全部已注册凭证（finish 验签按 credential id
//!   命中），对浏览器只下发空 allowCredentials——不影响验签安全（签名仍
//!   必须匹配某个已注册公钥）。
//! - 登录后按 `AuthenticationResult::needs_update` 更新计数器/备份标志
//!   （克隆认证器检测依赖计数器单调，webauthn-rs 在 finish 内校验）。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use webauthn_rs::prelude::{
    AuthenticationResult, CredentialID, Passkey, PasskeyAuthentication, PasskeyRegistration, Url,
    Uuid, Webauthn, WebauthnBuilder, WebauthnError,
};

use crate::sessions::{unix_now, write_atomic, SessionLookup};
use crate::{RelayState, TARGET};

/// challenge 有效期：60s 内未完成 ceremony 即作废（浏览器弹窗被晾着、用户
/// 走开等场景）。
const CHALLENGE_TTL: Duration = Duration::from_secs(60);
/// begin 端点限流：任意 [`RATE_WINDOW`] 内每 IP ≤ [`RATE_MAX_BEGINS`] 次
///（对齐 lib.rs 每连接限流的量级——真用户一次登录只 begin 一次，10 次/分
/// 只拦脚本）。
const RATE_MAX_BEGINS: usize = 10;
const RATE_WINDOW: Duration = Duration::from_secs(60);
/// 凭证表文件名（落在 state_dir 下）。
const CREDENTIALS_FILE: &str = "credentials.json";
/// 凭证 label 协议约束（对齐 device 名量级）：trim 后非空、≤64 char。
const LABEL_MAX_CHARS: usize = 64;
/// 单用户固定身份（user_handle）：所有 passkey 挂同一用户，浏览器钥匙串
/// 按此分组展示。
const USER_NAME: &str = "imagent";
const USER_DISPLAY_NAME: &str = "imagent";

// ---------------------------------------------------------------------------
// base64url（无 padding）：credential id 的对外字符串形态（与浏览器
// PublicKeyCredential.id 约定一致；不引 base64 crate，手写 20 行）
// ---------------------------------------------------------------------------

const B64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// bytes → base64url（无 padding）。
pub(crate) fn b64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64URL_ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(B64URL_ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(B64URL_ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL_ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 凭证表（落盘 <state_dir>/credentials.json）
// ---------------------------------------------------------------------------

/// 凭证记录运行时态：`passkey` 含公钥/计数器/备份标志（webauthn-rs 的
/// `Passkey` 支持 serde，可安全序列化入库）。
struct CredRecord {
    /// credential id 的 base64url 形态（对外展示与 cred_revoke 的键）。
    id: String,
    /// 设备标签（注册时传入，缺省按设备名）。
    label: String,
    created: u64,
    passkey: Passkey,
}

/// 落盘文件形状（`{"v":1,"user":"<uuid>","credentials":[...]}`；user 为
/// 单用户固定 user_handle，首注册时生成、跨重启稳定）。
#[derive(serde::Serialize, serde::Deserialize)]
struct CredentialsFile {
    v: u32,
    user: String,
    credentials: Vec<CredRecordOnDisk>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CredRecordOnDisk {
    id: String,
    label: String,
    created: u64,
    passkey: Passkey,
}

/// `cred_list` 帧的单凭证视图（lib.rs `creds_frame` 组帧）。
pub(crate) struct CredView {
    pub id: String,
    pub label: String,
    pub created: u64,
}

/// 凭证表（进程级单例，Arc 共享）。锁纪律对齐 SessionStore：锁内计算 →
/// 锁外写盘，绝不跨 await 持锁。
#[derive(Default)]
struct CredInner {
    records: Vec<CredRecord>, // 按 (created, id) 插入序有序
    user_uuid: Option<Uuid>,
}

pub(crate) struct CredStore {
    inner: Mutex<CredInner>,
    path: PathBuf,
}

impl CredStore {
    /// 启动加载：文件缺失 = 空表（首次部署）；解析失败 / IO 错误 warn 后按
    /// 空表启动——凭证全失效只是「重新用 USER_TOKEN 登录并重新注册」，不
    /// 放大权限面，不 brick relay（对齐 sessions.json 取舍）。
    pub(crate) fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(CREDENTIALS_FILE);
        let mut inner = CredInner::default();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<CredentialsFile>(&text) {
                Ok(f) if f.v == 1 => {
                    let total = f.credentials.len();
                    inner.user_uuid = f.user.parse().ok();
                    inner.records = f
                        .credentials
                        .into_iter()
                        .map(|r| CredRecord {
                            id: r.id,
                            label: r.label,
                            created: r.created,
                            passkey: r.passkey,
                        })
                        .collect();
                    tracing::info!(
                        target: TARGET,
                        credentials = inner.records.len(),
                        dropped_bad = total - inner.records.len(),
                        "凭证表加载（credentials.json）"
                    );
                }
                _ => tracing::warn!(
                    target: TARGET,
                    path = %path.display(),
                    "credentials.json 解析失败：按空表启动（USER_TOKEN 登录后可重新注册 passkey）"
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                target: TARGET,
                path = %path.display(),
                error = %e,
                "credentials.json 读取失败：按空表启动"
            ),
        }
        Self {
            inner: Mutex::new(inner),
            path,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CredInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 单用户 user_handle：首注册时生成并落盘，此后跨重启稳定。
    fn user_uuid(&self) -> Uuid {
        let mut st = self.lock();
        *st.user_uuid.get_or_insert_with(Uuid::new_v4)
    }

    /// 列表视图（created 有序）。
    pub(crate) fn list(&self) -> Vec<CredView> {
        let st = self.lock();
        st.records
            .iter()
            .map(|r| CredView {
                id: r.id.clone(),
                label: r.label.clone(),
                created: r.created,
            })
            .collect()
    }

    /// 全部 passkey（登录 begin 组 allowCredentials / finish 验签 state 用）。
    fn passkeys(&self) -> Vec<Passkey> {
        let st = self.lock();
        st.records.iter().map(|r| r.passkey.clone()).collect()
    }

    /// 已注册 credential id（注册 begin 的 exclude 列表，防同认证器重复注册）。
    fn credential_ids(&self) -> Vec<CredentialID> {
        let st = self.lock();
        st.records
            .iter()
            .map(|r| r.passkey.cred_id().clone())
            .collect()
    }

    /// 插入新凭证并立即落盘，返回其视图。同 id 重复注册 → None（幂等拒绝）。
    fn insert(&self, label: &str, passkey: Passkey, now: u64) -> Option<CredView> {
        let id = b64url_encode(passkey.cred_id().as_slice());
        {
            let mut st = self.lock();
            if st.records.iter().any(|r| r.id == id) {
                return None;
            }
            st.records.push(CredRecord {
                id: id.clone(),
                label: label.to_string(),
                created: now,
                passkey,
            });
        }
        self.persist();
        tracing::info!(target: TARGET, cred = %id_prefix(&id), label = %label, "passkey 注册");
        Some(CredView {
            id,
            label: label.to_string(),
            created: now,
        })
    }

    /// 删除凭证（cred_revoke）并落盘；未知 id → false。
    fn remove(&self, id: &str) -> bool {
        {
            let mut st = self.lock();
            let before = st.records.len();
            st.records.retain(|r| r.id != id);
            if st.records.len() == before {
                return false;
            }
        }
        self.persist();
        tracing::info!(target: TARGET, cred = %id_prefix(id), "passkey 删除");
        true
    }

    /// 登录成功后的计数器/备份标志回写（克隆检测依赖；变更才落盘）。
    fn update_credential(&self, res: &AuthenticationResult) {
        let mut changed = false;
        {
            let mut st = self.lock();
            for r in &mut st.records {
                if r.passkey.update_credential(res).unwrap_or(false) {
                    changed = true;
                }
            }
        }
        if changed {
            self.persist();
        }
    }

    /// 按 credential id（bytes）查 label（登录换发会话的设备名）。
    fn label_of(&self, cred: &[u8]) -> Option<String> {
        let st = self.lock();
        st.records
            .iter()
            .find(|r| r.passkey.cred_id().as_slice() == cred)
            .map(|r| r.label.clone())
    }

    /// 是否已注册任何 passkey（login/begin 前置判定）。
    fn is_empty(&self) -> bool {
        self.lock().records.is_empty()
    }

    /// 落盘（快照在锁内，写在锁外；0600 原子替换复用 sessions 的
    /// `write_atomic`——凭证表含公钥虽非机密，仍按敏感数据保护）。
    fn persist(&self) {
        let json = {
            let st = self.lock();
            let user = st.user_uuid.unwrap_or_else(Uuid::new_v4).to_string();
            let file = CredentialsFile {
                v: 1,
                user,
                credentials: st
                    .records
                    .iter()
                    .map(|r| CredRecordOnDisk {
                        id: r.id.clone(),
                        label: r.label.clone(),
                        created: r.created,
                        passkey: r.passkey.clone(),
                    })
                    .collect(),
            };
            serde_json::to_string(&file).unwrap_or_default()
        };
        if json.is_empty() {
            return;
        }
        if let Err(e) = write_atomic(&self.path, &json) {
            tracing::warn!(
                target: TARGET,
                path = %self.path.display(),
                error = %e,
                "credentials.json 写盘失败（在线登录不受影响，重启后丢失本次变更）"
            );
        }
    }
}

/// 日志里的 credential id 前缀（完整 id 太长，前 10 char 够排查）。
fn id_prefix(id: &str) -> String {
    id.chars().take(10).collect()
}

// ---------------------------------------------------------------------------
// challenge 表（内存、单次消费、60s 过期）
// ---------------------------------------------------------------------------

/// 一场 ceremony 的服务端状态（锁内存放，绝不发往客户端）。
enum Ceremony {
    Register {
        state: PasskeyRegistration,
        /// 注册标签（complete 时入凭证表）。
        label: String,
        /// 发起注册的会话 id：complete 必须同一会话（防会话间抢 challenge）。
        session_id: String,
    },
    Login {
        state: PasskeyAuthentication,
    },
    /// re-auth（管理门槛）：断言验证通过**不建会话**，只换发 5 分钟
    /// adminToken（管理帧门槛，见 [`AdminTokenTable`]）。
    Reauth {
        state: PasskeyAuthentication,
        /// 发起 re-auth 的会话 id：complete 必须同一会话。
        session_id: String,
    },
}

struct ChallengeEntry {
    ceremony: Ceremony,
    /// ceremony 绑定的 RP 参数：complete 时按此重建 Webauthn 实例（两请求
    /// 间 Host 变化的容忍——按 begin 时的 rp 验签，challenge 一致性不破）。
    rp_id: String,
    origin: Url,
    expires_at: Instant,
}

#[derive(Default)]
struct ChallengeTable {
    map: Mutex<HashMap<String, ChallengeEntry>>,
}

impl ChallengeTable {
    /// 存入（调用方生成 beginId）。
    fn insert(&self, id: String, ceremony: Ceremony, rp_id: String, origin: Url, now: Instant) {
        let mut st = self.map.lock().unwrap_or_else(|e| e.into_inner());
        purge_expired(&mut st, now);
        st.insert(
            id,
            ChallengeEntry {
                ceremony,
                rp_id,
                origin,
                expires_at: now + CHALLENGE_TTL,
            },
        );
    }

    /// 取出并删除（单次消费）：未知 id / 已过期 → None。
    fn take(&self, id: &str, now: Instant) -> Option<(Ceremony, String, Url)> {
        let mut st = self.map.lock().unwrap_or_else(|e| e.into_inner());
        purge_expired(&mut st, now);
        let e = st.remove(id)?;
        if now >= e.expires_at {
            return None;
        }
        Some((e.ceremony, e.rp_id, e.origin))
    }
}

/// 滑出过期条目（进出都顺带清，表不积压）。
fn purge_expired(st: &mut HashMap<String, ChallengeEntry>, now: Instant) {
    st.retain(|_, e| now < e.expires_at);
}

// ---------------------------------------------------------------------------
// admin 令牌表（re-auth 产物，内存态、5 分钟窗）
// ---------------------------------------------------------------------------

/// re-auth（管理门槛）换发的 adminToken 有效期：5 分钟（任务定案）——
/// 管理操作（机器增删/令牌轮换/配对批准）须 passkey 二次验证后短时间内
/// 完成，过期重新 re-auth。
const ADMIN_TOKEN_TTL_SECS: u64 = 5 * 60;

/// admin 令牌表（内存 `HashMap<token, expires>`，任务定案复用 challenge
/// 表模式）：token 为 32B hex 明文键（生命周期 ≤5 分钟的内存态，与
/// challenge 同级，无落盘面）；verify 单次判定（**非**单次消费——管理
/// 弹层内连续多个操作共享同一 5 分钟窗）。时间注入（单测拨钟）。
#[derive(Default)]
pub(crate) struct AdminTokenTable {
    map: Mutex<HashMap<String, u64>>,
}

impl AdminTokenTable {
    /// 换发：32B 随机 → 64 hex，`expires = now + 5min`。先清过期（表有界）。
    pub(crate) fn issue(&self, now: u64) -> (String, u64) {
        use rand::RngCore;
        let mut buf = [0u8; 32];
        rand::rng().fill_bytes(&mut buf);
        let token = hex::encode(buf);
        let expires = now.saturating_add(ADMIN_TOKEN_TTL_SECS);
        let mut st = self.map.lock().unwrap_or_else(|e| e.into_inner());
        st.retain(|_, exp| *exp > now);
        st.insert(token.clone(), expires);
        (token, expires)
    }

    /// 管理帧校验：存在且未过期。顺带清过期条目。
    pub(crate) fn verify(&self, token: &str, now: u64) -> bool {
        let mut st = self.map.lock().unwrap_or_else(|e| e.into_inner());
        st.retain(|_, exp| *exp > now);
        st.contains_key(token)
    }
}

// ---------------------------------------------------------------------------
// begin 端点 per-IP 限流（公网形态直连对端 = Caddy，取 XFF 首跳）
// ---------------------------------------------------------------------------

#[derive(Default)]
struct BeginRateLimiter {
    hits: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl BeginRateLimiter {
    /// 滑动窗口判定 + 记录（true = 放行）。顺带滑出全窗口外的键，防表膨胀。
    fn allow(&self, key: &str, now: Instant) -> bool {
        let mut st = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        st.retain(|_, v| {
            v.back()
                .is_some_and(|t| now.duration_since(*t) < RATE_WINDOW)
        });
        let q = st.entry(key.to_string()).or_default();
        while let Some(&oldest) = q.front() {
            if now.duration_since(oldest) >= RATE_WINDOW {
                q.pop_front();
            } else {
                break;
            }
        }
        if q.len() >= RATE_MAX_BEGINS {
            return false;
        }
        q.push_back(now);
        true
    }
}

// ---------------------------------------------------------------------------
// RP 参数推导（Host → rp_id/origin；env 覆盖 rp_id）
// ---------------------------------------------------------------------------

/// Host 头去端口：`relay.example.com:8443` → `relay.example.com`；
/// IPv6 `[::1]:3010` → `[::1]`（`[::1]` 本就过不了 builder 域名校验，这里
/// 只保证不取到残缺的 `[`）。
fn host_without_port(host: &str) -> String {
    if let Some(rest) = host.strip_prefix('[') {
        // IPv6 字面量：取 `[...]` 整段（`]` 后的 `:port` 丢弃）。
        let end = rest.find(']').unwrap_or(rest.len());
        return format!("[{}]", &rest[..end]);
    }
    host.split(':').next().unwrap_or(host).to_string()
}

/// 请求 Host 是否 loopback（缺省 scheme 判定用：本地 http 直连形态）。
fn is_loopback_host(host_no_port: &str) -> bool {
    matches!(host_no_port, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// 从请求头推导 (rp_id, origin)：
/// - rp_id：`IMAGENT_RELAY_RP_ID`（RelayConfig.webauthn_rp_id）覆盖，缺省
///   Host 去端口；
/// - origin scheme：`X-Forwarded-Proto`（Caddy 反代注入，仅认 http/https；
///   relay 只绑 loopback，该头只可能来自自家 Caddy 或直连本机客户端，
///   伪造只影响伪造者自己的 ceremony）→ 缺省 loopback http / 其它 https。
///
/// 推导失败（无 Host / origin 非法）→ None → 400。
fn derive_rp(headers: &HeaderMap, override_rp_id: Option<&str>) -> Option<(String, Url)> {
    let host = headers.get(header::HOST)?.to_str().ok()?.trim().to_string();
    let host_no_port = host_without_port(&host).to_string();
    if host_no_port.is_empty() {
        return None;
    }
    let rp_id = override_rp_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or(host_no_port.clone());
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| *s == "http" || *s == "https")
        .map(str::to_string)
        .unwrap_or_else(|| {
            if is_loopback_host(&host_no_port) {
                "http".to_string()
            } else {
                "https".to_string()
            }
        });
    let origin = Url::parse(&format!("{scheme}://{host}")).ok()?;
    Some((rp_id, origin))
}

/// 按 (rp_id, origin) 构建 Webauthn 实例（每请求构建，成本可忽略——纯
/// 配置对象）。rp_id 非 origin 有效域名（如 IP 字面量）→ Err（调用方回
/// 400 + 修复指引）。
fn build_webauthn(rp_id: &str, origin: &Url) -> Result<Webauthn, WebauthnError> {
    WebauthnBuilder::new(rp_id, origin)?
        .rp_name("imagent")
        .build()
}

// ---------------------------------------------------------------------------
// 进程级状态与 HTTP 端点
// ---------------------------------------------------------------------------

/// WebAuthn 层进程级单例（RelayState 持 Arc 共享）。
pub(crate) struct WebauthnState {
    creds: CredStore,
    challenges: ChallengeTable,
    limiter: BeginRateLimiter,
    /// re-auth 换发的 adminToken 表（管理帧门槛，5 分钟窗）。
    pub(crate) admin_tokens: AdminTokenTable,
    /// `IMAGENT_RELAY_RP_ID`（缺省 None = 按 Host 推导）。
    rp_id_override: Option<String>,
}

impl WebauthnState {
    pub(crate) fn new(state_dir: &Path, rp_id_override: Option<String>) -> Self {
        Self {
            creds: CredStore::load(state_dir),
            challenges: ChallengeTable::default(),
            limiter: BeginRateLimiter::default(),
            admin_tokens: AdminTokenTable::default(),
            rp_id_override,
        }
    }
}

/// JSON 错误响应：`{"error":"<code>"}`（SPA 按 code 分流文案；message 可选
/// 补充修复指引）。push 端点族（v1.10）同款错误形态，复用本助手。
pub(crate) fn err_json(status: StatusCode, code: &str, message: Option<&str>) -> Response {
    let mut body = serde_json::json!({ "error": code });
    if let Some(m) = message {
        body["message"] = serde_json::Value::String(m.to_string());
    }
    (status, Json(body)).into_response()
}

/// 限流键：X-Forwarded-For 首跳（公网形态，Caddy 注入）→ 缺省直连对端 IP。
fn rate_limit_key(headers: &HeaderMap, peer: std::net::SocketAddr) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| peer.ip().to_string())
}

/// begin 请求体（label 可选：缺省由 SPA 传设备名，裸调用给「passkey」）。
#[derive(serde::Deserialize, Default)]
pub(crate) struct RegisterBeginBody {
    #[serde(default)]
    label: Option<String>,
}

/// complete 请求体：credential 为浏览器 PublicKeyCredential 的 JSON 传输
/// 形态（webauthn-rs RegisterPublicKeyCredential / PublicKeyCredential 的
/// serde 形状，字段名一致）。
#[derive(serde::Deserialize)]
pub(crate) struct CompleteBody {
    #[serde(rename = "beginId")]
    begin_id: String,
    credential: serde_json::Value,
}

/// 注册会话认证（Bearer sessionToken → 有效会话 id）。失败统一 401（不区分
/// 过期/吊销/伪造，爆破面最小化）。push 端点族（v1.10）同款认证语义，复用
/// 本助手（同一路由族、同一错误形态）。
pub(crate) fn auth_session(st: &RelayState, headers: &HeaderMap) -> Result<String, Box<Response>> {
    let Some(token) = crate::bearer_token(headers) else {
        return Err(Box::new(err_json(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            None,
        )));
    };
    match st.sessions.verify(token, unix_now()) {
        SessionLookup::Valid { id, .. } => Ok(id),
        _ => Err(Box::new(err_json(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            None,
        ))),
    }
}

/// POST /webauthn/register/begin：已登录会话发起注册。响应
/// `{"beginId","publicKey":{CreationChallengeResponse}}`（challenge/user.id
/// 为 base64url 字符串，SPA 转 ArrayBuffer 后交 credentials.create）。
pub(crate) async fn register_begin(
    State(st): State<RelayState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: Option<Json<RegisterBeginBody>>,
) -> Response {
    let session_id = match auth_session(&st, &headers) {
        Ok(id) => id,
        Err(resp) => return *resp,
    };
    if !st
        .webauthn
        .limiter
        .allow(&rate_limit_key(&headers, peer), Instant::now())
    {
        return err_json(StatusCode::TOO_MANY_REQUESTS, "rate_limited", None);
    }
    let Some((rp_id, origin)) = derive_rp(&headers, st.webauthn.rp_id_override.as_deref()) else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            Some("缺少 Host，无法推导 RP"),
        );
    };
    let Ok(webauthn) = build_webauthn(&rp_id, &origin) else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "rp_unsupported",
            Some("rp_id 非法（IP 直连形态不受支持）：本地 http 直连请改用 localhost 主机名，或设 IMAGENT_RELAY_RP_ID"),
        );
    };
    // label：trim 后非空、≤64 char（脏值按缺省——注册不是高频路径，放宽
    // 不如统一规整）。
    let label = body
        .and_then(|Json(b)| b.label)
        .and_then(|l| {
            let t = l.trim().to_string();
            (!t.is_empty() && t.chars().count() <= LABEL_MAX_CHARS).then_some(t)
        })
        .unwrap_or_else(|| "passkey".to_string());
    let user_uuid = st.webauthn.creds.user_uuid();
    let excluded = st.webauthn.creds.credential_ids();
    let (ccr, state) = match webauthn.start_passkey_registration(
        user_uuid,
        USER_NAME,
        USER_DISPLAY_NAME,
        Some(excluded),
    ) {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!(target: TARGET, error = ?e, "webauthn 注册 challenge 生成失败");
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "server_error", None);
        }
    };
    let begin_id = Uuid::new_v4().to_string();
    st.webauthn.challenges.insert(
        begin_id.clone(),
        Ceremony::Register {
            state,
            label,
            session_id: session_id.clone(),
        },
        rp_id,
        origin,
        Instant::now(),
    );
    let Ok(ccr) = serde_json::to_value(&ccr) else {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, "server_error", None);
    };
    // CreationChallengeResponse 序列化为 {"publicKey":{…}}——取内层，响应
    // 形状 {"beginId", "publicKey": <浏览器 creation options（base64url 字符串形态）>}。
    let public_key = ccr
        .get("publicKey")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    tracing::info!(target: TARGET, session = %session_id, "passkey 注册 begin");
    (
        StatusCode::OK,
        Json(serde_json::json!({ "beginId": begin_id, "publicKey": public_key })),
    )
        .into_response()
}

/// POST /webauthn/register/complete：验签入凭证表。成功 200
/// `{"credential":{id,label,created}}`。
pub(crate) async fn register_complete(
    State(st): State<RelayState>,
    headers: HeaderMap,
    Json(body): Json<CompleteBody>,
) -> Response {
    let session_id = match auth_session(&st, &headers) {
        Ok(id) => id,
        Err(resp) => return *resp,
    };
    let (ceremony, rp_id, origin) =
        match st.webauthn.challenges.take(&body.begin_id, Instant::now()) {
            Some(x) => x,
            None => return err_json(StatusCode::BAD_REQUEST, "challenge_invalid", None),
        };
    let Ceremony::Register {
        state,
        label,
        session_id: begin_session,
    } = ceremony
    else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "challenge_invalid",
            Some("challenge 类型不匹配"),
        );
    };
    if begin_session != session_id {
        // challenge 由另一会话发起：不换发（会话间不抢 ceremony）。
        return err_json(StatusCode::UNAUTHORIZED, "unauthorized", None);
    }
    let webauthn = match build_webauthn(&rp_id, &origin) {
        Ok(w) => w,
        Err(_) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "server_error", None),
    };
    let reg = match serde_json::from_value::<webauthn_rs::prelude::RegisterPublicKeyCredential>(
        body.credential,
    ) {
        Ok(r) => r,
        Err(_) => return err_json(StatusCode::BAD_REQUEST, "invalid_credential", None),
    };
    let passkey = match webauthn.finish_passkey_registration(&reg, &state) {
        Ok(p) => p,
        Err(e) => {
            tracing::info!(target: TARGET, error = ?e, "passkey 注册验签失败");
            return err_json(StatusCode::UNAUTHORIZED, "verification_failed", None);
        }
    };
    let view = match st.webauthn.creds.insert(&label, passkey, unix_now()) {
        Some(v) => v,
        None => return err_json(StatusCode::CONFLICT, "duplicate_credential", None),
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "credential": { "id": view.id, "label": view.label, "created": view.created }
        })),
    )
        .into_response()
}

/// POST /webauthn/login/begin：无认证。已注册凭证为空 → 400 `no_credentials`
///（SPA 引导走 USER_TOKEN 入口）。响应
/// `{"beginId","publicKey":{RequestChallengeResponse}}`，其中
/// **`allowCredentials` 恒置空**（discoverable 形态，浏览器免选用户）——
/// 服务端 state 仍持有全部已注册凭证（finish 验签按 credential id 命中），
/// 下发面不泄露凭证数量。
pub(crate) async fn login_begin(
    State(st): State<RelayState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !st
        .webauthn
        .limiter
        .allow(&rate_limit_key(&headers, peer), Instant::now())
    {
        return err_json(StatusCode::TOO_MANY_REQUESTS, "rate_limited", None);
    }
    if st.webauthn.creds.is_empty() {
        return err_json(
            StatusCode::BAD_REQUEST,
            "no_credentials",
            Some("尚未注册任何通行钥匙：请先用访问令牌登录，在设备管理中注册"),
        );
    }
    let Some((rp_id, origin)) = derive_rp(&headers, st.webauthn.rp_id_override.as_deref()) else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            Some("缺少 Host，无法推导 RP"),
        );
    };
    let Ok(webauthn) = build_webauthn(&rp_id, &origin) else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "rp_unsupported",
            Some("rp_id 非法（IP 直连形态不受支持）：本地 http 直连请改用 localhost 主机名，或设 IMAGENT_RELAY_RP_ID"),
        );
    };
    let passkeys = st.webauthn.creds.passkeys();
    let (rcr, state) = match webauthn.start_passkey_authentication(&passkeys) {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!(target: TARGET, error = ?e, "webauthn 登录 challenge 生成失败");
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "server_error", None);
        }
    };
    let begin_id = Uuid::new_v4().to_string();
    st.webauthn.challenges.insert(
        begin_id.clone(),
        Ceremony::Login { state },
        rp_id,
        origin,
        Instant::now(),
    );
    // RequestChallengeResponse 序列化为 {"publicKey":{…}}——取内层后清空
    // allowCredentials（discoverable 形态，浏览器免选用户；state 不变，验签
    // 安全不降——finish 仍按 credential id 命中已注册公钥验签）。
    let mut public_key = match serde_json::to_value(&rcr) {
        Ok(v) => v
            .get("publicKey")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        Err(_) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "server_error", None),
    };
    public_key["allowCredentials"] = serde_json::json!([]);
    tracing::info!(target: TARGET, "passkey 登录 begin");
    (
        StatusCode::OK,
        Json(serde_json::json!({ "beginId": begin_id, "publicKey": public_key })),
    )
        .into_response()
}

/// POST /webauthn/login/complete：验签 → 直接创建 L2 会话（与 USER_TOKEN
/// hello 换发同构，设备名取 passkey label）。成功 200
/// `{"session":{id,token,expiresAt}}`（token 明文只此一次）；会话表满且
/// 全在线 → 503 `too_many_sessions`（对齐 WS close reason）。
pub(crate) async fn login_complete(
    State(st): State<RelayState>,
    Json(body): Json<CompleteBody>,
) -> Response {
    let (ceremony, rp_id, origin) =
        match st.webauthn.challenges.take(&body.begin_id, Instant::now()) {
            Some(x) => x,
            None => return err_json(StatusCode::BAD_REQUEST, "challenge_invalid", None),
        };
    let Ceremony::Login { state } = ceremony else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "challenge_invalid",
            Some("challenge 类型不匹配"),
        );
    };
    let webauthn = match build_webauthn(&rp_id, &origin) {
        Ok(w) => w,
        Err(_) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "server_error", None),
    };
    let assertion = match serde_json::from_value::<webauthn_rs::prelude::PublicKeyCredential>(
        body.credential,
    ) {
        Ok(a) => a,
        Err(_) => return err_json(StatusCode::BAD_REQUEST, "invalid_credential", None),
    };
    let result = match webauthn.finish_passkey_authentication(&assertion, &state) {
        Ok(r) => r,
        Err(e) => {
            tracing::info!(target: TARGET, error = ?e, "passkey 登录验签失败");
            return err_json(StatusCode::UNAUTHORIZED, "verification_failed", None);
        }
    };
    // 验签通过只证明「某个已注册凭证持有私钥」；label 取该凭证标签（会话
    // 设备名）。计数器/备份标志回写（needs_update 时）。
    let Some(label) = st
        .webauthn
        .creds
        .label_of(result.cred_id().as_ref())
        .map(|l| {
            if l.is_empty() {
                "passkey".to_string()
            } else {
                l
            }
        })
    else {
        // state 持有的凭证在 begin 后被 cred_revoke 删除：验签竟通过说明删除
        // 前的公钥匹配——按失效处理（凭证已删，不得登录）。
        tracing::info!(target: TARGET, "passkey 登录命中已删除凭证，拒绝");
        return err_json(StatusCode::UNAUTHORIZED, "verification_failed", None);
    };
    st.webauthn.creds.update_credential(&result);
    let issued = match st
        .sessions
        .issue(&label, unix_now(), st.config.session_ttl_secs)
    {
        Ok(s) => s,
        Err(()) => {
            tracing::warn!(target: TARGET, "passkey 登录换发会话被拒（表满且全在线）");
            return err_json(StatusCode::SERVICE_UNAVAILABLE, "too_many_sessions", None);
        }
    };
    tracing::info!(
        target: TARGET,
        session = %issued.id.chars().take(8).collect::<String>(),
        label = %label,
        "passkey 登录成功（换发会话）"
    );
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "session": { "id": issued.id, "token": issued.token, "expiresAt": issued.expires_at }
        })),
    )
        .into_response()
}

/// POST /webauthn/reauth/begin（管理门槛 re-auth 第一段）：**已登录才能
/// 发起**（`Authorization: Bearer <sessionToken>`），复用登录的 challenge
/// 机制（60s 单次消费 + per-IP 限流）。响应与 login/begin 同形
/// `{"beginId","publicKey":{RequestChallengeResponse}}`（allowCredentials
/// 恒空——discoverable 形态）。
pub(crate) async fn reauth_begin(
    State(st): State<RelayState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let session_id = match auth_session(&st, &headers) {
        Ok(id) => id,
        Err(resp) => return *resp, // 未登录 / 会话失效 → 401
    };
    if !st
        .webauthn
        .limiter
        .allow(&rate_limit_key(&headers, peer), Instant::now())
    {
        return err_json(StatusCode::TOO_MANY_REQUESTS, "rate_limited", None);
    }
    if st.webauthn.creds.is_empty() {
        return err_json(
            StatusCode::BAD_REQUEST,
            "no_credentials",
            Some("尚未注册任何通行钥匙：re-auth 依赖 passkey，请先用访问令牌登录并注册"),
        );
    }
    let Some((rp_id, origin)) = derive_rp(&headers, st.webauthn.rp_id_override.as_deref()) else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            Some("缺少 Host，无法推导 RP"),
        );
    };
    let Ok(webauthn) = build_webauthn(&rp_id, &origin) else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "rp_unsupported",
            Some("rp_id 非法（IP 直连形态不受支持）：本地 http 直连请改用 localhost 主机名，或设 IMAGENT_RELAY_RP_ID"),
        );
    };
    let passkeys = st.webauthn.creds.passkeys();
    let (rcr, state) = match webauthn.start_passkey_authentication(&passkeys) {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!(target: TARGET, error = ?e, "webauthn re-auth challenge 生成失败");
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "server_error", None);
        }
    };
    let begin_id = Uuid::new_v4().to_string();
    st.webauthn.challenges.insert(
        begin_id.clone(),
        Ceremony::Reauth {
            state,
            session_id: session_id.clone(),
        },
        rp_id,
        origin,
        Instant::now(),
    );
    // 与 login/begin 同款：allowCredentials 恒空（discoverable）。
    let mut public_key = match serde_json::to_value(&rcr) {
        Ok(v) => v
            .get("publicKey")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        Err(_) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "server_error", None),
    };
    public_key["allowCredentials"] = serde_json::json!([]);
    tracing::info!(target: TARGET, session = %session_id, "passkey re-auth begin");
    (
        StatusCode::OK,
        Json(serde_json::json!({ "beginId": begin_id, "publicKey": public_key })),
    )
        .into_response()
}

/// POST /webauthn/reauth/complete（管理门槛 re-auth 收口）：Bearer 同
/// begin（须同一会话）+ 断言 → 验签（**不建会话**）→ 换发 5 分钟
/// adminToken。成功 200 `{"adminToken":"<64hex>","expiresAt":<unix秒>}`
///（明文只此一次）；计数器/备份标志照常回写（克隆检测不分 ceremony）。
pub(crate) async fn reauth_complete(
    State(st): State<RelayState>,
    headers: HeaderMap,
    Json(body): Json<CompleteBody>,
) -> Response {
    let session_id = match auth_session(&st, &headers) {
        Ok(id) => id,
        Err(resp) => return *resp,
    };
    let (ceremony, rp_id, origin) =
        match st.webauthn.challenges.take(&body.begin_id, Instant::now()) {
            Some(x) => x,
            None => return err_json(StatusCode::BAD_REQUEST, "challenge_invalid", None),
        };
    let Ceremony::Reauth {
        state,
        session_id: begin_session,
    } = ceremony
    else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "challenge_invalid",
            Some("challenge 类型不匹配"),
        );
    };
    if begin_session != session_id {
        return err_json(StatusCode::UNAUTHORIZED, "unauthorized", None);
    }
    let webauthn = match build_webauthn(&rp_id, &origin) {
        Ok(w) => w,
        Err(_) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "server_error", None),
    };
    let assertion = match serde_json::from_value::<webauthn_rs::prelude::PublicKeyCredential>(
        body.credential,
    ) {
        Ok(a) => a,
        Err(_) => return err_json(StatusCode::BAD_REQUEST, "invalid_credential", None),
    };
    let result = match webauthn.finish_passkey_authentication(&assertion, &state) {
        Ok(r) => r,
        Err(e) => {
            tracing::info!(target: TARGET, error = ?e, "passkey re-auth 验签失败");
            return err_json(StatusCode::UNAUTHORIZED, "verification_failed", None);
        }
    };
    if st
        .webauthn
        .creds
        .label_of(result.cred_id().as_ref())
        .is_none()
    {
        // 与 login_complete 同款：challenge 持有的凭证在 begin 后被删除。
        tracing::info!(target: TARGET, "passkey re-auth 命中已删除凭证，拒绝");
        return err_json(StatusCode::UNAUTHORIZED, "verification_failed", None);
    }
    st.webauthn.creds.update_credential(&result);
    let (admin_token, expires_at) = st.webauthn.admin_tokens.issue(unix_now());
    tracing::info!(
        target: TARGET,
        session = %session_id.chars().take(8).collect::<String>(),
        expires_at,
        "passkey re-auth 成功（换发 adminToken，5 分钟窗）"
    );
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "adminToken": admin_token,
            "expiresAt": expires_at,
        })),
    )
        .into_response()
}

/// cred_list 帧载荷（lib.rs WS 管理帧分拣调用）：`{"v":1,"type":"creds",
/// "credentials":[{id,label,created}]}`。
pub(crate) fn creds_frame(creds: &[CredView]) -> String {
    let arr: Vec<serde_json::Value> = creds
        .iter()
        .map(|c| serde_json::json!({ "id": c.id, "label": c.label, "created": c.created }))
        .collect();
    serde_json::json!({ "v": 1, "type": "creds", "credentials": arr }).to_string()
}

/// WS 管理帧处理入口（lib.rs `handle_connection` 的 browser 分支调用）。
/// 返回回给该连接的文本帧。
pub(crate) fn handle_cred_frame(st: &RelayState, kind: &str, cred_id: &str) -> String {
    match kind {
        "cred_list" => creds_frame(&st.webauthn.creds.list()),
        "cred_revoke" => {
            // 单用户语义（对齐 device_revoke）：任一在线会话可删任意 passkey。
            st.webauthn.creds.remove(cred_id);
            creds_frame(&st.webauthn.creds.list())
        }
        _ => creds_frame(&st.webauthn.creds.list()),
    }
}

/// 组装 WebAuthn HTTP 路由（裸路径 + /imagent 前缀两形态，对齐 WS 端点：
/// Caddy strip 后走裸路径，本地直连走前缀）。re-auth（管理门槛）两端点
/// 与注册/登录同款形态。
pub(crate) fn routes() -> Router<crate::RelayState> {
    Router::new()
        .route("/webauthn/register/begin", post(register_begin))
        .route("/webauthn/register/complete", post(register_complete))
        .route("/webauthn/login/begin", post(login_begin))
        .route("/webauthn/login/complete", post(login_complete))
        .route("/webauthn/reauth/begin", post(reauth_begin))
        .route("/webauthn/reauth/complete", post(reauth_complete))
        .route("/imagent/webauthn/register/begin", post(register_begin))
        .route(
            "/imagent/webauthn/register/complete",
            post(register_complete),
        )
        .route("/imagent/webauthn/login/begin", post(login_begin))
        .route("/imagent/webauthn/login/complete", post(login_complete))
        .route("/imagent/webauthn/reauth/begin", post(reauth_begin))
        .route("/imagent/webauthn/reauth/complete", post(reauth_complete))
}

/// 进程级 WebAuthn 状态构造（spawn_relay 调用）。
pub(crate) fn new_state(state_dir: &Path, rp_id_override: Option<String>) -> Arc<WebauthnState> {
    Arc::new(WebauthnState::new(state_dir, rp_id_override))
}

// ---------------------------------------------------------------------------
// 单元测试（rp 推导 / base64url / challenge 过期 / 限流，纯函数注入拨钟）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_static(k),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    /// Host → rp_id/origin：公网 https（带端口）、XFF 覆盖 scheme、缺省
    /// https、loopback http、IPv6 Host、env 覆盖 rp_id、无 Host → None。
    #[test]
    fn rp_derivation() {
        let (rp, origin) =
            derive_rp(&headers(&[("host", "relay.example.com:8443")]), None).unwrap();
        assert_eq!(rp, "relay.example.com");
        assert_eq!(origin.as_str(), "https://relay.example.com:8443/");

        // X-Forwarded-Proto 覆盖（Caddy 已终结 TLS、回源 http 的形态）。
        let (rp, origin) = derive_rp(
            &headers(&[
                ("host", "relay.example.com:8443"),
                ("x-forwarded-proto", "https"),
            ]),
            None,
        )
        .unwrap();
        assert_eq!(origin.as_str(), "https://relay.example.com:8443/");
        assert_eq!(rp, "relay.example.com");
        // 非法 XFF 值忽略（回落缺省推导）。
        let (_, origin) = derive_rp(
            &headers(&[("host", "relay.example.com"), ("x-forwarded-proto", "ftp")]),
            None,
        )
        .unwrap();
        assert_eq!(origin.as_str(), "https://relay.example.com/");

        // loopback 缺省 http（本地直连调试形态）。
        let (rp, origin) = derive_rp(&headers(&[("host", "localhost:3010")]), None).unwrap();
        assert_eq!(rp, "localhost");
        assert_eq!(origin.as_str(), "http://localhost:3010/");
        assert!(build_webauthn(&rp, &origin).is_ok(), "localhost 可作 RP");

        // IPv6 Host：去端口取 `[::1]`（builder 域名校验会拒——文档化的本地
        // 直连限制，推导本身不 panic）。
        let (rp, _) = derive_rp(&headers(&[("host", "[::1]:3010")]), None).unwrap();
        assert_eq!(rp, "[::1]");

        // env 覆盖 rp_id：origin 仍按 Host 推导。
        let (rp, origin) = derive_rp(
            &headers(&[("host", "relay.example.com:8443")]),
            Some("example.com"),
        )
        .unwrap();
        assert_eq!(rp, "example.com");
        assert_eq!(origin.as_str(), "https://relay.example.com:8443/");
        // 覆盖值必须是 origin 的有效域名（builder 校验兜底——错误 env 在
        // begin 端点 400，不 panic）。
        assert!(build_webauthn("other.example", &origin).is_err());

        // 无 Host → None；空覆盖值按缺省推导。
        assert!(derive_rp(&HeaderMap::new(), None).is_none());
        let (rp, _) = derive_rp(&headers(&[("host", "relay.example.com")]), Some("  ")).unwrap();
        assert_eq!(rp, "relay.example.com");

        // IP 字面量 rp_id 过不了 builder 域名校验（本地 127.0.0.1 直连不可用
        // passkey 的根因，文档化）。
        let (rp, origin) = derive_rp(&headers(&[("host", "127.0.0.1:3010")]), None).unwrap();
        assert_eq!(rp, "127.0.0.1");
        assert!(build_webauthn(&rp, &origin).is_err());
    }

    /// base64url：RFC 4648 测试向量（无 padding）。
    #[test]
    fn b64url_vectors() {
        assert_eq!(b64url_encode(b""), "");
        assert_eq!(b64url_encode(b"f"), "Zg");
        assert_eq!(b64url_encode(b"fo"), "Zm8");
        assert_eq!(b64url_encode(b"foo"), "Zm9v");
        assert_eq!(b64url_encode(b"foob"), "Zm9vYg");
        assert_eq!(b64url_encode(b"fooba"), "Zm9vYmE");
        assert_eq!(b64url_encode(b"foobar"), "Zm9vYmFy");
    }

    /// challenge 表：insert/take 单次消费、过期拒收、未知 id None。
    #[test]
    fn challenge_table_single_use_and_expiry() {
        let table = ChallengeTable::default();
        let t0 = Instant::now();
        let origin = Url::parse("https://relay.example.com").unwrap();
        table.insert(
            "c1".into(),
            Ceremony::Login {
                state: login_state(),
            },
            "relay.example.com".into(),
            origin.clone(),
            t0,
        );
        let (ceremony, rp, _) = table.take("c1", t0 + Duration::from_secs(1)).unwrap();
        assert!(matches!(ceremony, Ceremony::Login { .. }));
        assert_eq!(rp, "relay.example.com");
        assert!(
            table.take("c1", t0 + Duration::from_secs(2)).is_none(),
            "单次消费"
        );

        // 过期：60s 边界外拒收。
        table.insert(
            "c2".into(),
            Ceremony::Login {
                state: login_state(),
            },
            "relay.example.com".into(),
            origin,
            t0,
        );
        assert!(
            table.take("c2", t0 + CHALLENGE_TTL).is_none(),
            "60s 整点过期"
        );
        assert!(table.take("nobody", t0).is_none(), "未知 id");
    }

    /// 登录 state 生成（过 builder 需要 localhost RP）。
    fn login_state() -> PasskeyAuthentication {
        let origin = Url::parse("https://relay.example.com").unwrap();
        let wa = build_webauthn("relay.example.com", &origin).unwrap();
        wa.start_passkey_authentication(&[]).unwrap().1
    }

    /// begin 限流：窗口内第 11 次拒、窗口滑出后恢复。
    #[test]
    fn begin_rate_limiter() {
        let limiter = BeginRateLimiter::default();
        let t0 = Instant::now();
        for i in 0..RATE_MAX_BEGINS {
            assert!(limiter.allow("1.2.3.4", t0), "第 {} 次放行", i + 1);
        }
        assert!(!limiter.allow("1.2.3.4", t0), "窗口内第 11 次拒");
        assert!(
            !limiter.allow("1.2.3.4", t0 + Duration::from_secs(30)),
            "半窗仍拒"
        );
        assert!(
            limiter.allow("1.2.3.4", t0 + RATE_WINDOW),
            "全窗滑出恢复放行"
        );
        // 不同 IP 互不影响。
        assert!(limiter.allow("5.6.7.8", t0), "per-IP 隔离");
    }

    /// 凭证表单元层：user_uuid 生成即稳定；未注册前不落盘（凭证插入/删除/
    /// 重载保留经 e2e 软认证器用真 Passkey 覆盖——Passkey 字段私有无法在单
    /// 元层凭空构造）。
    #[test]
    fn cred_store_user_uuid_stable() {
        let dir = std::env::temp_dir().join(format!("imagent-relay-wa-{}", Uuid::new_v4()));
        let store = CredStore::load(&dir);
        assert!(store.is_empty());
        let user = store.user_uuid();
        assert_eq!(user, store.user_uuid(), "user_uuid 一旦生成即稳定");
        assert!(!dir.join(CREDENTIALS_FILE).exists(), "未注册前不写盘");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// admin 令牌表：换发（64 hex + now+300）/ 窗口内多次 verify（非单次
    /// 消费——管理弹层内连续操作共享同一窗）/ 未知 token 拒 / 5 分钟整点
    /// 过期 / 过期条目被顺带清理（表有界）。
    #[test]
    fn admin_token_table_issue_verify_window() {
        let table = AdminTokenTable::default();
        let now = 1_000u64;
        let (token, expires) = table.issue(now);
        assert_eq!(token.len(), 64, "32B hex");
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(expires, now + ADMIN_TOKEN_TTL_SECS, "5 分钟窗");
        assert!(table.verify(&token, now + 60), "窗口内有效");
        assert!(table.verify(&token, now + 60), "非单次消费（同窗可复用）");
        assert!(!table.verify("nobody", now), "未知 token 拒");
        assert!(!table.verify("", now), "空 token 拒");
        // 边界：expiresAt 时刻已过期（> now 才存活）。
        assert!(!table.verify(&token, expires), "5 分钟整点过期");
        // 过期条目被后续 verify/issue 顺带清理。
        let (token2, _) = table.issue(expires + 1);
        assert!(table.verify(&token2, expires + 2));
        assert!(!table.verify(&token, expires + 2), "旧 token 过期即失效");
    }
}
