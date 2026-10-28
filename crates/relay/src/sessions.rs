//! 会话层（协议 v1.2，L2 安全加固）：USER_TOKEN 一次性换发 30 天会话凭证
//! + 设备管理的数据面。
//!
//! 动机：静态 USER_TOKEN 长期存浏览器 localStorage，泄露即 agent 完全接管
//! 且不可单独撤销。改为：USER_TOKEN 仅登录换发一次会话凭证
//!（sessionToken），此后重连用 sessionToken；登录后 SPA 不再持有
//! USER_TOKEN；会话可列表/吊销（`device_list` / `device_revoke`，见 lib.rs
//! 的帧分拣）。
//!
//! 纪律：
//! - **token 落盘只存哈希**（sha256 hex，[`SessionRecord::token_hash`]），
//!   内存校验用恒定时间比较——即使读到 sessions.json 也伪造不出凭证；
//! - 会话表 ≤ [`MAX_SESSIONS`]（32）：超限时先惰性清理过期会话、再淘汰
//!   吊销墓碑（最旧优先），仍满则拒绝换发（调用方 close 1008
//!   "too many sessions"）；
//! - 吊销留**墓碑**（`revoked=true`——「会话表移除其有效会话资格」）：
//!   重连可区分 "session revoked" 与 "auth failed"（协议 v1.2 §2 的 reason
//!   区分要求）；墓碑不进 devices 列表，仅在容量压力下被淘汰；
//! - 落盘 `<state_dir>/sessions.json`（0600；tmp+rename 原子替换 + fsync）：
//!   结构性变更（换发/吊销/过期清理）立即写盘，last_seen 类软变更按
//!   [`PERSIST_THROTTLE`] 节流；重启后会话保留（不掉线）；
//! - 落盘内容**不含 USER_TOKEN 本体**，只有会话记录；
//! - 时间全部由调用方注入（单测可拨钟；lib.rs 装配处传真实 unix 秒）。
//!
//! 锁纪律对齐 Hub（std Mutex 纯内存临界区）：方法「锁内计算 → 锁外写盘」，
//! 绝不跨 await 持锁；写盘是微小文件的同步 IO（µs 级），可接受。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::{constant_time_eq, Outbound, TARGET};

/// 会话表上限（协议 v1.2）：表小是为了「按 token 全表恒定时间匹配」的
/// 扫描成本可控；超限拒绝**新换发**（先清理过期/墓碑腾位，见
/// [`SessionStore::issue`]）。真机常态 2~5 台设备，32 留足余量。
pub(crate) const MAX_SESSIONS: usize = 32;
/// 会话表文件名（落在 `state_dir` 下）。
const SESSIONS_FILE: &str = "sessions.json";
/// 落盘节流窗口：last_seen 这类软变更至多每 60s 写一次盘（结构性变更不受
/// 节流，立即写）。
const PERSIST_THROTTLE: Duration = Duration::from_secs(60);

/// 会话 token 哈希（sha256，hex 编码——落盘与内存统一形态；明文只在换发
/// 那一刻回给客户端一次）。
pub(crate) fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// 当前 unix 秒（会话时间戳统一口径；会话层方法的 `now` 参数由此注入）。
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// 会话记录（内存运行时态；落盘走 [`SessionRecordOnDisk`]）。
struct SessionRecord {
    id: String,
    /// sha256(token) 的 hex——**不存明文**。
    token_hash: String,
    /// 设备名（hello `device` 字段或 UA 摘要，lib.rs `device_name`）。
    name: String,
    created: u64,
    last_seen: u64,
    /// unix 秒；到此即过期（惰性清理）。
    expires_at: u64,
    /// 吊销墓碑：true 后不再可用也不进 devices 列表，仅保留「该 token 已
    /// 吊销」的记忆供重连 reason 区分。
    revoked: bool,
    /// 该会话当前的活跃 WS 连接（同设备多标签 = 多条；`connected` 标记与
    /// revoke 断连都看这里）。conn_id 用于连接退出时只摘自己的绑定。
    conns: Vec<ConnBinding>,
}

/// 单条 WS 连接到会话的绑定。
struct ConnBinding {
    conn_id: u64,
    tx: mpsc::Sender<Outbound>,
}

/// 落盘文件形状（`{"v":1,"sessions":[...]}`——版本字段为将来演进留口）。
#[derive(serde::Serialize, serde::Deserialize)]
struct SessionsFile {
    v: u32,
    sessions: Vec<SessionRecordOnDisk>,
}

/// 落盘形态：`conns` 不落盘（连接是进程内运行时态，重启即失效）。
#[derive(serde::Serialize, serde::Deserialize)]
struct SessionRecordOnDisk {
    id: String,
    token_hash: String,
    name: String,
    created: u64,
    last_seen: u64,
    expires_at: u64,
    revoked: bool,
}

/// 落盘时机提示（锁内计算 → 锁外执行）。
#[derive(Clone, Copy)]
enum PersistHint {
    /// 无变更，不写盘。
    None,
    /// 软变更（last_seen），受节流约束。
    Soft,
    /// 结构性变更（换发/吊销/过期清理/墓碑淘汰），立即写盘。
    Now,
}

#[derive(Default)]
struct StoreInner {
    sessions: HashMap<String, SessionRecord>,
    last_persist: Option<Instant>,
}

/// sessionToken 校验结果（协议 v1.2 §2 的 reason 区分）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SessionLookup {
    /// 有效（已顺带更新 last_seen）：携带 id 与剩余有效期。
    Valid { id: String, expires_at: u64 },
    /// 匹配到但已过期（条目已惰性清理）。
    Expired,
    /// 匹配到但已吊销（墓碑）。
    Revoked,
    /// 无匹配（token 伪造/被容量淘汰的墓碑）。
    NotFound,
}

/// 换发产物：`token` 明文只此一次回给客户端（hello_ack 的 session 载荷）。
pub(crate) struct IssuedSession {
    pub id: String,
    pub token: String,
    pub expires_at: u64,
}

/// devices 帧的单设备视图（lib.rs `devices_frame` 组帧）。
pub(crate) struct DeviceView {
    pub id: String,
    pub name: String,
    pub connected: bool,
    pub last_seen: u64,
    pub created: u64,
}

/// 会话表（进程级单例，Arc 共享；state 目录由 RelayConfig 注入）。
pub(crate) struct SessionStore {
    inner: Mutex<StoreInner>,
    path: PathBuf,
}

impl SessionStore {
    /// 启动加载（真实时钟）：读 `<state_dir>/sessions.json`。
    pub(crate) fn load(state_dir: &Path) -> Self {
        Self::load_at(state_dir, unix_now())
    }

    /// 启动加载（时钟注入，单测拨钟用）：文件缺失 = 空表（首次部署）；
    /// 解析失败 / IO 错误 = warn 后按空表启动——会话全失效只是「全员重新
    /// 登录」，不放大权限面（fail-closed 的安全语义不破），不能因状态文件
    /// 损坏 brick 整个 relay。加载时顺带清理已过期会话。
    fn load_at(state_dir: &Path, now: u64) -> Self {
        let path = state_dir.join(SESSIONS_FILE);
        let mut sessions = HashMap::new();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<SessionsFile>(&text) {
                Ok(f) if f.v == 1 => {
                    let total = f.sessions.len();
                    sessions = f
                        .sessions
                        .into_iter()
                        .filter(|r| r.expires_at > now)
                        .map(|r| {
                            let id = r.id.clone();
                            let rec = SessionRecord {
                                id,
                                token_hash: r.token_hash,
                                name: r.name,
                                created: r.created,
                                last_seen: r.last_seen,
                                expires_at: r.expires_at,
                                revoked: r.revoked,
                                conns: Vec::new(),
                            };
                            (rec.id.clone(), rec)
                        })
                        .collect();
                    tracing::info!(
                        target: TARGET,
                        sessions = sessions.len(),
                        dropped_expired = total - sessions.len(),
                        "会话表加载（sessions.json）"
                    );
                }
                Ok(_) | Err(_) => {
                    tracing::warn!(
                        target: TARGET,
                        path = %path.display(),
                        "sessions.json 解析失败：按空表启动（全部设备需重新登录；USER_TOKEN 登录即可恢复）"
                    );
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(
                    target: TARGET,
                    path = %path.display(),
                    error = %e,
                    "sessions.json 读取失败：按空表启动（全部设备需重新登录）"
                );
            }
        }
        Self {
            inner: Mutex::new(StoreInner {
                sessions,
                last_persist: None,
            }),
            path,
        }
    }

    /// 中毒（临界区 panic）时取回数据继续服务：对齐 Hub 的取舍。
    fn lock(&self) -> std::sync::MutexGuard<'_, StoreInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 锁内计算 + 落盘时机判定 → 释放锁 → 按提示写盘。
    fn with_lock<R>(&self, f: impl FnOnce(&mut StoreInner) -> (R, PersistHint)) -> R {
        let (r, hint) = {
            let mut st = self.lock();
            f(&mut st)
        }; // MutexGuard 在此释放
        match hint {
            PersistHint::None => {}
            PersistHint::Soft => self.persist(false),
            PersistHint::Now => self.persist(true),
        }
        r
    }

    /// sessionToken 校验（协议 v1.2 §2）：sha256 后**全表**恒定时间匹配
    ///（≤ [`MAX_SESSIONS`] 条，不提前 break），同一趟顺带惰性清理过期会话
    ///（§6）。命中有效会话时更新 last_seen（软落盘）。
    pub(crate) fn verify(&self, token: &str, now: u64) -> SessionLookup {
        self.with_lock(|st| {
            let hashed = hash_token(token);
            let mut matched: Option<String> = None;
            let mut expired: Vec<String> = Vec::new();
            for (id, rec) in st.sessions.iter() {
                if constant_time_eq(&hashed, &rec.token_hash) {
                    matched = Some(id.clone()); // 不 break：恒定时间全表扫描
                }
                if rec.expires_at <= now {
                    expired.push(id.clone());
                }
            }
            let purged = !expired.is_empty();
            if purged {
                for id in &expired {
                    st.sessions.remove(id);
                }
            }
            let hint_of = |h: PersistHint| if purged { PersistHint::Now } else { h };
            let Some(id) = matched else {
                return (SessionLookup::NotFound, hint_of(PersistHint::None));
            };
            if expired.contains(&id) {
                return (SessionLookup::Expired, PersistHint::Now);
            }
            match st.sessions.get_mut(&id) {
                Some(rec) if rec.revoked => (SessionLookup::Revoked, hint_of(PersistHint::None)),
                Some(rec) => {
                    rec.last_seen = now;
                    (
                        SessionLookup::Valid {
                            id,
                            expires_at: rec.expires_at,
                        },
                        hint_of(PersistHint::Soft),
                    )
                }
                // 防御：matched 未过期且未被清理，必然还在表内。
                None => (SessionLookup::NotFound, hint_of(PersistHint::None)),
            }
        })
    }

    /// 换发新会话（USER_TOKEN 验证通过后调用）：先惰性清理过期会话；表仍
    /// 满 → 淘汰吊销墓碑（last_seen 最旧优先——防「32 个墓碑永久堵死
    /// 登录」）；仍满 → Err（调用方 close 1008 "too many sessions"）。
    /// token 明文只在返回值里出现一次（不落盘，落盘的是哈希）。
    pub(crate) fn issue(&self, name: &str, now: u64, ttl_secs: u64) -> Result<IssuedSession, ()> {
        self.with_lock(|st| {
            st.sessions.retain(|_, r| r.expires_at > now);
            if st.sessions.len() >= MAX_SESSIONS {
                let mut tombstones: Vec<(u64, String)> = st
                    .sessions
                    .iter()
                    .filter(|(_, r)| r.revoked)
                    .map(|(id, r)| (r.last_seen, id.clone()))
                    .collect();
                tombstones.sort();
                for (_, id) in tombstones {
                    st.sessions.remove(&id);
                    if st.sessions.len() < MAX_SESSIONS {
                        break;
                    }
                }
            }
            if st.sessions.len() >= MAX_SESSIONS {
                return (Err(()), PersistHint::None);
            }
            let id = uuid::Uuid::new_v4().to_string();
            let token = generate_session_token();
            let expires_at = now.saturating_add(ttl_secs);
            st.sessions.insert(
                id.clone(),
                SessionRecord {
                    id: id.clone(),
                    token_hash: hash_token(&token),
                    name: name.to_string(),
                    created: now,
                    last_seen: now,
                    expires_at,
                    revoked: false,
                    conns: Vec::new(),
                },
            );
            (
                Ok(IssuedSession {
                    id,
                    token,
                    expires_at,
                }),
                PersistHint::Now,
            )
        })
    }

    /// 连接绑定（认证通过、注册 Hub 之后调用）：会话已被吊销（认证与注册
    /// 之间的竞态）→ false，调用方 fail-closed 断开。过期不在此复查——
    /// 认证刚通过（30 天尺度上毫秒级窗口无意义，且连接中途过期本就沿
    /// 既有语义到断连为止）。
    pub(crate) fn bind(&self, id: &str, conn_id: u64, tx: mpsc::Sender<Outbound>) -> bool {
        self.with_lock(|st| {
            let ok = match st.sessions.get_mut(id) {
                Some(rec) if !rec.revoked => {
                    rec.conns.push(ConnBinding { conn_id, tx });
                    true
                }
                _ => false,
            };
            (ok, PersistHint::None)
        })
    }

    /// 连接退出摘除自己的绑定（connected 置 false 靠它；会话本身保留）。
    pub(crate) fn unbind(&self, id: &str, conn_id: u64) {
        self.with_lock(|st| {
            if let Some(rec) = st.sessions.get_mut(id) {
                rec.conns.retain(|c| c.conn_id != conn_id);
            }
            ((), PersistHint::None)
        })
    }

    /// 吊销（协议 v1.2 §4）：置墓碑 + 摘走该会话全部在线连接的投递口
    /// （调用方锁外逐个 close 1008 "session revoked"）+ 立即落盘。未知 id /
    /// 已吊销 → 空 vec（幂等 no-op）。**单用户语义**：所有浏览器都过同一
    /// USER_TOKEN/session 层，任一在线会话可吊销其它（含自己——SPA 侧
    /// 自行引导重登），relay 不做更细的授权分层。
    pub(crate) fn revoke(&self, id: &str, now: u64) -> Vec<mpsc::Sender<Outbound>> {
        self.with_lock(|st| match st.sessions.get_mut(id) {
            Some(rec) if !rec.revoked => {
                rec.revoked = true;
                rec.last_seen = now;
                let senders = std::mem::take(&mut rec.conns)
                    .into_iter()
                    .map(|c| c.tx)
                    .collect();
                (senders, PersistHint::Now)
            }
            _ => (Vec::new(), PersistHint::None),
        })
    }

    /// devices 帧数据（时钟注入）：未吊销且未过期的会话，按 (created, id)
    /// 有序输出（协议对顺序不敏感，有序只为测试与展示稳定）。
    pub(crate) fn devices(&self, now: u64) -> Vec<DeviceView> {
        let st = self.lock();
        let mut v: Vec<DeviceView> = st
            .sessions
            .values()
            .filter(|r| !r.revoked && r.expires_at > now)
            .map(|r| DeviceView {
                id: r.id.clone(),
                name: r.name.clone(),
                connected: !r.conns.is_empty(),
                last_seen: r.last_seen,
                created: r.created,
            })
            .collect();
        v.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
        v
    }

    /// 按提示写盘。序列化在锁内快照（HashMap 无序 → 按 (created, id) 排序，
    /// 文件内容稳定可 diff）；Soft 受 [`PERSIST_THROTTLE`] 节流。
    fn persist(&self, force: bool) {
        let json = {
            let mut st = self.lock();
            if !force
                && st
                    .last_persist
                    .is_some_and(|t| t.elapsed() < PERSIST_THROTTLE)
            {
                return;
            }
            let mut recs: Vec<&SessionRecord> = st.sessions.values().collect();
            recs.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
            let file = SessionsFile {
                v: 1,
                sessions: recs
                    .iter()
                    .map(|r| SessionRecordOnDisk {
                        id: r.id.clone(),
                        token_hash: r.token_hash.clone(),
                        name: r.name.clone(),
                        created: r.created,
                        last_seen: r.last_seen,
                        expires_at: r.expires_at,
                        revoked: r.revoked,
                    })
                    .collect(),
            };
            let json = serde_json::to_string(&file).unwrap_or_else(|e| {
                tracing::warn!(target: TARGET, error = %e, "sessions.json 序列化失败（跳过本次落盘）");
                String::new()
            });
            st.last_persist = Some(Instant::now());
            json
        };
        if json.is_empty() {
            return;
        }
        if let Err(e) = write_atomic(&self.path, &json) {
            tracing::warn!(
                target: TARGET,
                path = %self.path.display(),
                error = %e,
                "sessions.json 写盘失败（在线会话不受影响，重启后丢失本次变更）"
            );
        }
    }
}

/// 生成会话 token：32 随机字节 → 64 hex 字符（协议 v1.2 §1）。
fn generate_session_token() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// 原子写盘（tmp + rename；0600 权限——会话哈希虽非明文凭证，仍按敏感
/// 数据保护）：fsync 保证重启后会话记录不丢（协议 v1.2 §5 的「重启后会话
/// 保留」依赖此处的持久性）。
fn write_atomic(path: &Path, json: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    #[cfg(unix)]
    let mut f = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?
    };
    #[cfg(not(unix))]
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(json.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 单元测试（时间注入拨钟，任务说明 §5）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用临时 state 目录（每次唯一，不跨用例共享）。
    fn temp_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("imagent-relay-sess-{}", uuid::Uuid::new_v4()))
    }

    /// 基础生命周期：换发（token 64 hex 明文不落盘）→ 有效期内 verify
    /// Valid → 过期 Expired + 惰性清理后 NotFound → 吊销墓碑 Revoked。
    #[test]
    fn session_issue_verify_expire_revoke() {
        let dir = temp_dir();
        let store = SessionStore::load_at(&dir, 0);

        let now = 1_000u64;
        let issued = store.issue("iPhone", now, 3_600).unwrap();
        assert_eq!(issued.expires_at, now + 3_600, "expiresAt = now + ttl");
        assert_eq!(issued.token.len(), 64, "token = 32B hex（64 字符）");
        assert!(
            issued.token.chars().all(|c| c.is_ascii_hexdigit()),
            "token 是 hex"
        );

        // 有效期内重连：Valid + 剩余 expiresAt。
        assert_eq!(
            store.verify(&issued.token, now + 60),
            SessionLookup::Valid {
                id: issued.id.clone(),
                expires_at: now + 3_600
            }
        );

        // 落盘不含 token 明文，只存哈希。
        let raw = std::fs::read_to_string(dir.join(SESSIONS_FILE)).unwrap();
        assert!(!raw.contains(&issued.token), "落盘不得含 token 明文：{raw}");
        assert!(raw.contains("token_hash"), "落盘存哈希：{raw}");

        // 过期：verify 给 Expired 且条目被惰性清理（再查 NotFound）。
        assert_eq!(
            store.verify(&issued.token, now + 3_600 + 1),
            SessionLookup::Expired
        );
        assert_eq!(
            store.verify(&issued.token, now + 3_600 + 2),
            SessionLookup::NotFound
        );

        // 吊销：墓碑语义（Revoked 而非 NotFound）。
        let s2 = store.issue("Mac", now, 3_600).unwrap();
        assert!(matches!(
            store.verify(&s2.token, now + 1),
            SessionLookup::Valid { .. }
        ));
        assert!(store.revoke(&s2.id, now + 2).is_empty(), "无在线连接");
        assert_eq!(store.verify(&s2.token, now + 3), SessionLookup::Revoked);
        assert!(
            store.revoke(&s2.id, now + 4).is_empty(),
            "重复吊销幂等 no-op"
        );
        // 未知 token。
        assert_eq!(store.verify("deadbeef", now), SessionLookup::NotFound);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 落盘重启保留：有效会话跨「重启」（同目录新实例）仍可重连；过期
    /// 会话加载即清理；吊销墓碑跨重启保留但不进 devices。
    #[test]
    fn session_persist_across_reload() {
        let dir = temp_dir();
        let now = 5_000u64;
        let (live_token, revoked_token) = {
            let store = SessionStore::load_at(&dir, now);
            let live = store.issue("alive", now, 3_600).unwrap();
            store.issue("doomed", now, 10).unwrap(); // 即将过期
            let revoked = store.issue("revoked", now, 3_600).unwrap();
            store.revoke(&revoked.id, now);
            let _ = store.verify(&live.token, now + 30); // last_seen 刷新
            (live.token, revoked.token)
        };
        // 「重启」：同目录新实例，时钟推进到 doomed 过期之后。
        let store2 = SessionStore::load_at(&dir, now + 60);
        assert!(
            matches!(
                store2.verify(&live_token, now + 61),
                SessionLookup::Valid { .. }
            ),
            "有效会话跨重启保留"
        );
        assert_eq!(
            store2.verify(&revoked_token, now + 61),
            SessionLookup::Revoked,
            "吊销墓碑跨重启保留（reason 仍可区分 revoked）"
        );
        let names: Vec<String> = store2
            .devices(now + 61)
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert!(
            !names.iter().any(|n| n == "doomed"),
            "加载即清理过期：{names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "revoked"),
            "吊销墓碑不进 devices：{names:?}"
        );
        assert!(
            names.iter().any(|n| n == "alive"),
            "有效会话在列表：{names:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 32 上限：灌满 → 新换发被拒；过期清理腾位后可再换发；满员时吊销
    /// 墓碑被淘汰（腾位）而非永久堵死登录。
    #[test]
    fn session_cap_eviction_and_reject() {
        let dir = temp_dir();
        let store = SessionStore::load_at(&dir, 0);
        let now = 100u64;

        // 3 个短命 + 29 个长期 = 恰好 32。
        for i in 0..3 {
            store
                .issue(&format!("short-{i}"), now, 5)
                .unwrap_or_else(|_| panic!("short-{i} 应成功"));
        }
        for i in 0..(MAX_SESSIONS - 3) {
            store
                .issue(&format!("d{i}"), now, 3_600)
                .unwrap_or_else(|_| panic!("d{i} 应成功"));
        }
        assert!(
            store.issue("extra", now, 3_600).is_err(),
            "满员（32 个未过期未吊销）拒绝新换发"
        );
        assert!(
            store.issue("ok", now + 6, 3_600).is_ok(),
            "过期清理腾位后可再换发"
        );

        // 灌满后吊销最旧 3 个 → 墓碑淘汰腾位，登录不被墓碑堵死。
        let mut t0 = now + 10;
        while store.issue("fill", t0, 3_600).is_ok() {
            t0 += 1; // 推进时钟让「最旧墓碑」可判定
        }
        assert_eq!(store.devices(t0).len(), MAX_SESSIONS);
        for d in store.devices(t0).iter().take(3) {
            store.revoke(&d.id, t0);
        }
        assert!(
            store.issue("after-revoke", t0 + 1, 3_600).is_ok(),
            "墓碑淘汰腾位：吊销不堵死登录"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// connected 标记与 revoke 断连：bind 后 connected=true；unbind 摘除
    /// 自己（断开不删除会话）；revoke 返回全部在线投递口并清空绑定；
    /// 绑定已吊销会话 fail-closed。
    #[test]
    fn session_bind_unbind_and_revoke_disconnect() {
        let dir = temp_dir();
        let store = SessionStore::load_at(&dir, 0);
        let now = 42u64;
        let s = store.issue("iPhone", now, 3_600).unwrap();

        let (tx1, rx1) = mpsc::channel(8);
        let (tx2, rx2) = mpsc::channel(8);
        assert!(store.bind(&s.id, 1, tx1));
        assert!(store.bind(&s.id, 2, tx2), "同会话多连接（多标签）并存");
        assert!(
            store.devices(now)[0].connected,
            "有活跃 WS → connected=true"
        );

        store.unbind(&s.id, 1);
        assert!(
            store.devices(now)[0].connected,
            "断开一条仍有连接，connected 保持 true"
        );
        store.unbind(&s.id, 2);
        assert!(
            !store.devices(now)[0].connected,
            "全部断开 → connected=false"
        );
        assert_eq!(store.devices(now).len(), 1, "断开不删除会话");

        // 重新绑定两条后 revoke：两条投递口都返回、绑定清空、不进列表。
        let (tx3, _rx3) = mpsc::channel(8);
        let (tx4, _rx4) = mpsc::channel(8);
        assert!(store.bind(&s.id, 3, tx3));
        assert!(store.bind(&s.id, 4, tx4));
        let senders = store.revoke(&s.id, now + 5);
        assert_eq!(senders.len(), 2, "revoke 摘走全部在线连接投递口");
        assert!(store.devices(now + 5).is_empty(), "吊销后不进 devices 列表");
        // 绑定已吊销会话 → false（fail-closed）。
        let (tx5, _rx5) = mpsc::channel(8);
        assert!(!store.bind(&s.id, 5, tx5));
        drop((rx1, rx2));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// sessions.json 权限 0600、无 .tmp 残留、畸形文件按空表启动不 panic。
    #[test]
    fn session_file_permissions_and_corrupt_recovery() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SESSIONS_FILE), "not json").unwrap();
        let store = SessionStore::load_at(&dir, 0); // 畸形：warn + 空表
        assert!(store.devices(0).is_empty());

        let s = store.issue("x", 1, 3_600).unwrap(); // 立即落盘覆盖畸形文件
        assert!(
            serde_json::from_str::<serde_json::Value>(
                &std::fs::read_to_string(dir.join(SESSIONS_FILE)).unwrap()
            )
            .is_ok(),
            "覆盖为合法 JSON"
        );
        assert!(
            !dir.join("sessions.json.tmp").exists(),
            "tmp 文件已被 rename 消费，无残留"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(SESSIONS_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "sessions.json 权限 0600");
        }
        let _ = s;

        let _ = std::fs::remove_dir_all(&dir);
    }
}
