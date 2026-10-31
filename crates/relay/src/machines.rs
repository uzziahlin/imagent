//! 管理面机器表（准入管理批）：v1.3 网关公钥表从 env 静态配置迁移为
//! `<state_dir>/machines.json` 落盘 + 进程内热更新——网页化管理机器公钥
//! （`machine_list` / `machine_add` / `machine_remove` 管理帧，见 lib.rs
//! 帧分拣），增删即时生效、不断线。
//!
//! 启动语义（任务定案）：
//! - 文件**不存在** → 从 env `IMAGENT_RELAY_GATEWAY_KEYS`
//!   （[`crate::GatewayKeys`] 种子）导入并落盘（首次部署零改动：照旧配 env
//!   一次，之后 env 仅作留档）；
//! - 文件**存在** → 文件权威（env 忽略，启动日志说明——改 env 不生效，
//!   增删机器走管理帧）；
//! - 单条记录损坏（name 非法 / 公钥解不出 ed25519 点）→ warn 跳过该条
//!   （不 brick 整表，对齐 sessions.json / credentials.json 取舍）。
//!
//! 锁纪律对齐 [`crate::sessions::SessionStore`]：std Mutex 纯内存临界区，
//! 「锁内计算 → 锁外写盘」，绝不跨 await 持锁。prove 验签语义与
//! [`crate::GatewayKeys::verify`] 完全一致（machine 在表 + 绑定公钥恒定
//! 时间一致 + ed25519 验签 nonce），运行时认证全部走本表（env 种子只在
//! [`MachineStore::load`] 消费一次）。
//!
//! 旧 `GATEWAY_TOKENS` 表维持 env 静态（迁移期兼容路径，**不做**管理面化：
//! token 是共享秘密，网页化管理面换发/展示的收益不如直接迁公钥表，保留
//! env 形态直到全部机器切到 key 握手）。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use axum::extract::ws::Message;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

use crate::sessions::{unix_now, write_atomic};
use crate::{constant_time_eq, valid_machine_id, GatewayKeys, TARGET};

/// 机器表文件名（落在 `state_dir` 下）。
const MACHINES_FILE: &str = "machines.json";

/// `machine_list` / `machine_add` / `machine_remove` 的应答帧载荷视图。
pub(crate) struct MachineView {
    pub name: String,
    /// 公钥指纹（前 16 hex——公开材料取指纹纯为降噪，排查对照网关侧完整
    /// 公钥日志）。
    pub pubkey_fp: String,
    /// unix 秒（种子导入条目 = 首次落盘时刻）。
    pub created: u64,
}

/// 运行时条目：hex 原文（恒定时间比对用）+ 解析好的验签公钥。
#[derive(Clone)]
struct MachineEntry {
    name: String,
    pubkey_hex: String,
    created: u64,
    key: VerifyingKey,
}

/// 落盘文件形状（任务定案字节级：`[{"name","pubkey","created"}]` 顶层数组；
/// pubkey 为 64 hex 原文——公钥是公开材料，非秘密）。
#[derive(serde::Serialize, serde::Deserialize)]
struct MachineOnDisk {
    name: String,
    pubkey: String,
    created: u64,
}

/// 机器表（进程级单例，Arc 共享，RelayState 持有）。
pub(crate) struct MachineStore {
    inner: Mutex<Vec<MachineEntry>>,
    path: PathBuf,
}

impl MachineStore {
    /// 启动加载：文件不存在 → env 种子导入并落盘；存在 → 文件权威（env
    /// 忽略 + 日志说明）。`seed` 为 main 解析的 `GatewayKeys`（空表合法：
    /// 文件在则照走文件权威）。
    pub(crate) fn load(state_dir: &Path, seed: &GatewayKeys) -> Self {
        let path = state_dir.join(MACHINES_FILE);
        let entries = match std::fs::read_to_string(&path) {
            Ok(text) => {
                if !seed.is_empty() {
                    tracing::info!(
                        target: TARGET,
                        path = %path.display(),
                        "machines.json 文件权威：env IMAGENT_RELAY_GATEWAY_KEYS 被忽略\
                         （增删机器走 machine_add/machine_remove 管理帧）"
                    );
                }
                match serde_json::from_str::<Vec<MachineOnDisk>>(&text) {
                    Ok(list) => {
                        let total = list.len();
                        let entries: Vec<MachineEntry> =
                            list.into_iter().filter_map(parse_entry).collect();
                        if entries.len() < total {
                            tracing::warn!(
                                target: TARGET,
                                path = %path.display(),
                                dropped_bad = total - entries.len(),
                                "machines.json 含非法条目，已跳过（不影响其余机器接入）"
                            );
                        }
                        entries
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: TARGET,
                            path = %path.display(),
                            error = %e,
                            "machines.json 解析失败：按空表启动（machine_add 可重新添加；\
                             网关 key 握手在重新添加前全部失败）"
                        );
                        Vec::new()
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // 首次部署：env 种子导入（created = 本次启动时刻）并落盘。
                let now = unix_now();
                let mut entries: Vec<MachineEntry> = seed
                    .iter()
                    .map(|(name, hex)| MachineOnDisk {
                        name: name.clone(),
                        pubkey: hex.clone(),
                        created: now,
                    })
                    .filter_map(parse_entry)
                    .collect();
                entries.sort_by(|a, b| a.name.cmp(&b.name));
                let store = Self {
                    inner: Mutex::new(entries.clone()),
                    path: path.clone(),
                };
                // 种子合法性在 GatewayKeys::parse 已过校验，这里 entries 与
                // 种子必然等长；落盘失败不阻断启动（warn，下次启动重新种子）。
                store.persist_snapshot(&entries);
                tracing::info!(
                    target: TARGET,
                    path = %path.display(),
                    machines = entries.len(),
                    fps = ?entries.iter().map(|e| format!("{}={}", e.name, &e.pubkey_hex[..16])).collect::<Vec<_>>(),
                    "machines.json 不存在：env IMAGENT_RELAY_GATEWAY_KEYS 种子导入并落盘（此后文件权威）"
                );
                return store;
            }
            Err(e) => {
                tracing::warn!(
                    target: TARGET,
                    path = %path.display(),
                    error = %e,
                    "machines.json 读取失败：按空表启动"
                );
                Vec::new()
            }
        };
        Self {
            inner: Mutex::new(entries),
            path,
        }
    }

    /// prove 验证（与 [`GatewayKeys::verify`] 同语义）：machine 在表**且**
    /// 绑定公钥与 prove.pubkey 恒定时间一致**且** ed25519 验签 nonce 通过。
    /// 失败统一 false（防探测）。
    pub(crate) fn verify(&self, machine: &str, pubkey: &str, nonce: &str, signature: &str) -> bool {
        let Some((pubkey_hex, key)) = ({
            let st = self.lock();
            // 克隆出锁再验签（VerifyingKey 是 Copy，临界区最小化）。
            st.iter()
                .find(|e| e.name == machine)
                .map(|e| (e.pubkey_hex.clone(), e.key))
        }) else {
            return false;
        };
        if !constant_time_eq(pubkey, pubkey_hex.as_str()) {
            return false;
        }
        let Ok(sig) = hex::decode(signature) else {
            return false;
        };
        let Ok(sig) = <[u8; 64]>::try_from(sig) else {
            return false;
        };
        key.verify(nonce.as_bytes(), &Signature::from_bytes(&sig))
            .is_ok()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// 列表视图（name 有序——展示与 machines_cfg 帧稳定）。
    pub(crate) fn list(&self) -> Vec<MachineView> {
        let st = self.lock();
        let mut v: Vec<MachineView> = st
            .iter()
            .map(|e| MachineView {
                name: e.name.clone(),
                pubkey_fp: e.pubkey_hex[..16].to_string(),
                created: e.created,
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// 新增（`machine_add` 管理帧）：name 1..=32 `[a-zA-Z0-9_-]`、pubkey
    /// 64 hex 合法 ed25519 点、不得重名（Err 文本可读，回 `machine_invalid`）。
    /// 成功立即落盘 + 热生效（新机器无需重连中继即可握手接入）。
    pub(crate) fn add(&self, name: &str, pubkey: &str) -> Result<(), String> {
        if !valid_machine_id(name) {
            return Err(format!(
                "机器名 `{name}` 非法：1..=32 char，仅 ASCII 字母数字与 -_"
            ));
        }
        let bytes = hex::decode(pubkey).map_err(|_| {
            "公钥非合法 hex（须为 ed25519 公钥 64 hex，网关侧启动日志打印）".to_string()
        })?;
        let arr: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
            format!("公钥长度 {:?} 非法（须恰为 64 hex = 32 字节）", v.len() * 2)
        })?;
        let key = VerifyingKey::from_bytes(&arr)
            .map_err(|_| "公钥不是合法 ed25519 公钥点".to_string())?;
        let snapshot = {
            let mut st = self.lock();
            if st.iter().any(|e| e.name == name) {
                return Err(format!(
                    "机器名 `{name}` 已被占用。若是新机器：在它上面运行 \
                     `imagent join --name <新名字>` 换名后重新添加；若是给这台\
                     机器换钥：先 remove 再 add"
                ));
            }
            if st.iter().any(|e| e.key == key) {
                return Err(format!(
                    "该公钥已绑定其它机器（{}）：一个公钥只能绑一台机器",
                    st.iter()
                        .find(|e| e.key == key)
                        .map(|e| e.name.clone())
                        .unwrap_or_default()
                ));
            }
            st.push(MachineEntry {
                name: name.to_string(),
                pubkey_hex: pubkey.to_ascii_lowercase(),
                created: unix_now(),
                key,
            });
            st.sort_by(|a, b| a.name.cmp(&b.name));
            st.clone()
        };
        self.persist_snapshot(&snapshot);
        Ok(())
    }

    /// 删除（`machine_remove` 管理帧）：命中删除 + 落盘，返回 true（未命中
    /// false，幂等 no-op）。热生效：该机器的网关连接由调用方负责断开。
    pub(crate) fn remove(&self, name: &str) -> bool {
        let snapshot = {
            let mut st = self.lock();
            let before = st.len();
            st.retain(|e| e.name != name);
            if st.len() == before {
                return false;
            }
            st.clone()
        };
        self.persist_snapshot(&snapshot);
        true
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<MachineEntry>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 快照落盘（锁外执行；0600 原子写复用 sessions 纪律）。
    fn persist_snapshot(&self, entries: &[MachineEntry]) {
        let file: Vec<MachineOnDisk> = entries
            .iter()
            .map(|e| MachineOnDisk {
                name: e.name.clone(),
                pubkey: e.pubkey_hex.clone(),
                created: e.created,
            })
            .collect();
        let Ok(json) = serde_json::to_string(&file) else {
            return;
        };
        if let Err(e) = write_atomic(&self.path, &json) {
            tracing::warn!(
                target: TARGET,
                path = %self.path.display(),
                error = %e,
                "machines.json 写盘失败（在线认证不受影响，重启后丢失本次变更）"
            );
        }
    }
}

/// 机器表落盘文件是否已存在（启动期 fail-closed 判定用：文件在 = 管理
/// 面已接管机器表，env 两表均空不再构成「全新部署误配」）。
pub(crate) fn table_file_exists(state_dir: &Path) -> bool {
    state_dir.join(MACHINES_FILE).exists()
}

/// 单条落盘记录解析 + 合法性校验（加载与种子导入共用的形状）。
fn parse_entry(r: MachineOnDisk) -> Option<MachineEntry> {
    if !valid_machine_id(&r.name) {
        return None;
    }
    let bytes = hex::decode(&r.pubkey).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    let key = VerifyingKey::from_bytes(&arr).ok()?;
    Some(MachineEntry {
        name: r.name,
        pubkey_hex: r.pubkey.to_ascii_lowercase(),
        created: r.created,
        key,
    })
}

/// `machines_cfg` 帧（machine_list / machine_add / machine_remove 的应答）：
/// `{"v":1,"type":"machines_cfg","machines":[{name,pubkey_fp,created}]}`
/// （pubkey_fp = 公钥前 16 hex；created = unix 秒）。
pub(crate) fn machines_cfg_frame(machines: &[MachineView]) -> Message {
    let arr: Vec<serde_json::Value> = machines
        .iter()
        .map(|m| {
            serde_json::json!({ "name": m.name, "pubkey_fp": m.pubkey_fp, "created": m.created })
        })
        .collect();
    Message::text(
        serde_json::json!({ "v": 1, "type": "machines_cfg", "machines": arr }).to_string(),
    )
}

// ---------------------------------------------------------------------------
// 单元测试（种子导入 / 文件权威 / 热更新增删 / 校验与重名 / 帧形状）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;

    fn temp_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("imagent-relay-mach-{}", uuid::Uuid::new_v4()))
    }

    /// 测试密钥对（seed 固定可复现）。
    fn sk(seed: u8) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
    }

    fn pk_hex(seed: u8) -> String {
        hex::encode(sk(seed).verifying_key().to_bytes())
    }

    /// env 种子（GatewayKeys 形态）。
    fn seed_of(specs: &[(&str, u8)]) -> GatewayKeys {
        let spec = specs
            .iter()
            .map(|(m, s)| format!("{m}={}", pk_hex(*s)))
            .collect::<Vec<_>>()
            .join(",");
        GatewayKeys::parse(&spec).unwrap()
    }

    /// 存储迁移核心：首次种子导入落盘 → 重启文件权威（env 变更忽略）→
    /// env 移除后仍可用。
    #[test]
    fn store_seed_then_file_authority() {
        let dir = temp_dir();
        // 首次：文件不存在 → 种子导入 + 落盘。
        let store = MachineStore::load(&dir, &seed_of(&[("home", 1), ("work", 2)]));
        let names: Vec<String> = store.list().iter().map(|m| m.name.clone()).collect();
        assert_eq!(
            names,
            vec!["home".to_string(), "work".to_string()],
            "name 有序"
        );
        let raw = std::fs::read_to_string(dir.join(MACHINES_FILE)).unwrap();
        assert!(
            raw.starts_with('[') && raw.contains(r#""name":"home""#),
            "顶层数组形状：{raw}"
        );
        assert!(
            raw.contains(r#""pubkey""#) && raw.contains(r#""created""#),
            "{raw}"
        );

        // 重启 + env 换成别的机器 → 文件权威（env 忽略）。
        let store2 = MachineStore::load(&dir, &seed_of(&[("ghost", 9)]));
        let names: Vec<String> = store2.list().iter().map(|m| m.name.clone()).collect();
        assert_eq!(names, vec!["home".to_string(), "work".to_string()]);
        assert!(!names.contains(&"ghost".to_string()), "env 变更不生效");

        // env 完全移除 → 文件照常。
        let store3 = MachineStore::load(&dir, &GatewayKeys::default());
        assert_eq!(store3.list().len(), 2);
        assert!(!store3.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 热更新增删：add 立即可验签（无重启）、remove 后验签失败、落盘跨
    /// 重启保留；add 校验（重名 / 非法名 / 非 hex / 长度错 / 非法点 /
    /// 公钥重复绑定）。
    #[test]
    fn store_add_remove_hot_update() {
        let dir = temp_dir();
        let store = MachineStore::load(&dir, &seed_of(&[("home", 1)]));
        let nonce = "a".repeat(64);

        // seed 机器可验签。
        let sig = hex::encode(sk(1).sign(nonce.as_bytes()).to_bytes());
        assert!(store.verify("home", &pk_hex(1), &nonce, &sig));

        // add 热生效：新机器无需重启即可握手验签。
        store.add("new-mac", &pk_hex(2)).unwrap();
        let sig2 = hex::encode(sk(2).sign(nonce.as_bytes()).to_bytes());
        assert!(
            store.verify("new-mac", &pk_hex(2), &nonce, &sig2),
            "add 后立即生效"
        );
        // 机器表列表含指纹（前 16 hex）。
        let list = store.list();
        assert_eq!(list.len(), 2);
        let new_view = list.iter().find(|m| m.name == "new-mac").unwrap();
        assert_eq!(new_view.pubkey_fp, pk_hex(2)[..16].to_string());
        assert!(new_view.created > 0, "created unix 秒");

        // 校验失败：重名 / 非法名 / 非 hex / 长度错 / 非法点 / 公钥重复绑定。
        let long_name = "m".repeat(33);
        let cases: Vec<(&str, String, &str)> = vec![
            ("home", pk_hex(3), "已被占用"),
            ("bad name!", pk_hex(3), "非法"),
            ("", pk_hex(3), "非法"),
            (&long_name, pk_hex(3), "非法"),
            ("m3", "xyz".to_string(), "非合法 hex"),
            ("m3", "ab".repeat(31), "长度"),
            (
                "m3",
                {
                    let mut bad = [0xffu8; 32];
                    let mut probe = 0u8;
                    while VerifyingKey::from_bytes(&bad).is_ok() {
                        bad[0] = probe;
                        probe = probe.wrapping_add(1);
                    }
                    hex::encode(bad)
                },
                "公钥点",
            ),
            ("dup-key", pk_hex(1), "一个公钥只能绑一台机器"),
        ];
        for (name, key, needle) in cases {
            let err = store.add(name, &key).unwrap_err();
            assert!(err.contains(needle), "name={name} msg={err}");
        }

        // remove 热生效：验签失败 + 未命中幂等 false。
        assert!(store.remove("home"));
        assert!(
            !store.verify("home", &pk_hex(1), &nonce, &sig),
            "remove 后验签失败"
        );
        assert!(!store.remove("home"), "未命中 false");
        assert!(!store.remove("nobody"), "未知机器 false");

        // 落盘跨重启：add/remove 后重启，表内容一致。
        let store2 = MachineStore::load(&dir, &GatewayKeys::default());
        let names: Vec<String> = store2.list().iter().map(|m| m.name.clone()).collect();
        assert_eq!(names, vec!["new-mac".to_string()], "增删跨重启保留");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件损坏韧性：整体非 JSON → 空表（warn 不 brick）；单条非法 → 跳过
    /// 该条保留其余；空文件（[]）合法空表。
    #[test]
    fn store_corrupt_entries_skipped() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MACHINES_FILE), "not json").unwrap();
        let store = MachineStore::load(&dir, &GatewayKeys::default());
        assert!(store.is_empty(), "整体损坏 → 空表启动");

        // 一好一坏：坏条目跳过。
        std::fs::write(
            dir.join(MACHINES_FILE),
            format!(
                r#"[{{"name":"good","pubkey":"{}","created":1}},{{"name":"bad name","pubkey":"ab","created":2}}]"#,
                pk_hex(5)
            ),
        )
        .unwrap();
        let store = MachineStore::load(&dir, &GatewayKeys::default());
        let names: Vec<String> = store.list().iter().map(|m| m.name.clone()).collect();
        assert_eq!(names, vec!["good".to_string()], "坏条目跳过，好条目保留");
        assert!(store.verify(
            "good",
            &pk_hex(5),
            "n",
            &hex::encode(sk(5).sign(b"n").to_bytes())
        ));

        // 空数组合法。
        std::fs::write(dir.join(MACHINES_FILE), "[]").unwrap();
        assert!(MachineStore::load(&dir, &GatewayKeys::default()).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// machines_cfg 帧形状（协议对照逐字段）：v/type/machines[].{name,
    /// pubkey_fp,created}。
    #[test]
    fn machines_cfg_frame_shape() {
        let m = machines_cfg_frame(&[
            MachineView {
                name: "home".to_string(),
                pubkey_fp: "0123456789abcdef".to_string(),
                created: 1_900_000_000,
            },
            MachineView {
                name: "work".to_string(),
                pubkey_fp: "fedcba9876543210".to_string(),
                created: 1_900_000_001,
            },
        ]);
        let v: serde_json::Value = serde_json::from_str(m.into_text().unwrap().as_str()).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "machines_cfg");
        let arr = v["machines"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "home");
        assert_eq!(arr[0]["pubkey_fp"], "0123456789abcdef");
        assert_eq!(arr[0]["created"], 1_900_000_000);
        assert_eq!(arr[1]["name"], "work");
    }
}
