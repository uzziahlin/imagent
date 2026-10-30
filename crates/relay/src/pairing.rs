//! 新设备配对（准入管理批）：USER_TOKEN/passkey 都不在手的**新设备**首配
//! 流程——登录页发 `hello{pairing:true,device}`（无凭证）→ relay 创建
//! pending 记录（6 位数字码，5 分钟过期）并回 `pairing_pending` → 向全部
//! 已认证浏览器会话广播 `pairing_request` → 任一已登录设备经 re-auth
//! （adminToken）`pairing_approve` / `pairing_deny` → 批准则 pending 绑定
//! 的 WS 连接直接换发 session（hello_ack 形态与登录换发一致）。
//!
//! 帧契约（与 SPA 侧同一契约）：
//! - 入站 `{"v":1,"type":"hello","pairing":true,"device":"<名>"}`（无凭证）；
//! - 回 `{"v":1,"type":"pairing_pending","code":"483920"}`（该连接保持
//!   未认证态，只允许等待）；
//! - 广播 `{"v":1,"type":"pairing_request","code":"483920","device":"<名>",
//!   "expiresAt":<unix秒>}`；
//! - `{"v":1,"type":"pairing_approve","adminToken":"…","code":"483920"}` /
//!   `pairing_deny` 同形（须 adminToken，见 lib.rs 管理帧分拣）。
//!
//! 状态纪律：pending 全内存（重启即清 = 新设备重新发起，无跨重启语义）；
//! 同码并发唯一；上限 [`MAX_PENDING_PAIRING`] = 4；过期惰性清理（进出都
//! prune）。approve/deny 经 `claim` **原子取出**（先到先得，第二个批准者
//! 拿到 None → `pairing_not_found`）。时间由调用方注入（单测可拨钟）。

use std::collections::HashMap;
use std::sync::Mutex;

use axum::extract::ws::Message;

/// pending 码有效期（任务定案 5 分钟）。
pub(crate) const PAIRING_TTL_SECS: u64 = 5 * 60;
/// 并发 pending 上限（防滥用：真机常态同时最多一两台新设备在配）。
pub(crate) const MAX_PENDING_PAIRING: usize = 4;

/// 批准产物：换发给新设备的会话载荷（hello_ack.session 同形字段）。
pub(crate) struct PairedSession {
    pub id: String,
    pub token: String,
    pub expires_at: u64,
}

/// 投递给 pending 连接等待循环的信号（approve/deny 处理方经 mpsc 发出，
/// 跨连接任务协作——pending 连接自己完成注册，复用认证后公共路径）。
pub(crate) enum PairingSignal {
    Approved(PairedSession),
    Denied,
}

struct PendingEntry {
    device: String,
    expires_at: u64,
    signal: tokio::sync::mpsc::Sender<PairingSignal>,
}

/// pending 记录视图（pairing_pending / pairing_request 组帧用）。
pub(crate) struct PendingInfo {
    pub code: String,
    pub device: String,
    pub expires_at: u64,
}

/// 配对表（进程级单例，RelayState 持 Arc）。
#[derive(Default)]
pub(crate) struct PairingStore {
    inner: Mutex<HashMap<String, PendingEntry>>,
}

impl PairingStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingEntry>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 清理已过期条目（进出都顺带执行）。
    fn prune(st: &mut HashMap<String, PendingEntry>, now: u64) {
        st.retain(|_, e| e.expires_at > now);
    }

    /// 创建 pending：过期先清、上限已满 → None（调用方 close 1008 "too
    /// many pairings"）、6 位数字码在存活集内唯一（碰撞重试，存活 ≤4 条
    /// 碰撞概率可忽略）。
    pub(crate) fn create(
        &self,
        device: &str,
        signal: tokio::sync::mpsc::Sender<PairingSignal>,
        now: u64,
        ttl_secs: u64,
    ) -> Option<PendingInfo> {
        let mut st = self.lock();
        Self::prune(&mut st, now);
        if st.len() >= MAX_PENDING_PAIRING {
            return None;
        }
        let mut code = generate_code();
        for _ in 0..64 {
            if !st.contains_key(&code) {
                break;
            }
            code = generate_code();
        }
        let info = PendingInfo {
            code: code.clone(),
            device: device.to_string(),
            expires_at: now.saturating_add(ttl_secs.max(1)),
        };
        st.insert(
            code,
            PendingEntry {
                device: info.device.clone(),
                expires_at: info.expires_at,
                signal,
            },
        );
        Some(info)
    }

    /// 原子取出（approve/deny）：命中且未过期 → 移除并返回（设备名 + 信号
    /// 投递口——第二个处理者将拿到 None）；未命中 / 已过期 → None。
    pub(crate) fn claim(
        &self,
        code: &str,
        now: u64,
    ) -> Option<(String, tokio::sync::mpsc::Sender<PairingSignal>)> {
        let mut st = self.lock();
        Self::prune(&mut st, now);
        let entry = st.remove(code)?;
        Some((entry.device, entry.signal))
    }

    /// pending 设备主动断开等场景的移除（no-op 未知码）。
    pub(crate) fn remove(&self, code: &str) {
        self.lock().remove(code);
    }

    /// 存活 pending 数（日志用；时间注入与 create/claim 同口径）。
    pub(crate) fn len(&self, now: u64) -> usize {
        let mut st = self.lock();
        Self::prune(&mut st, now);
        st.len()
    }
}

/// 6 位数字码（`"000000"`..=`"999999"`，零填充保留）。
fn generate_code() -> String {
    use rand::Rng;
    format!("{:06}", rand::rng().random_range(0..1_000_000u32))
}

/// `pairing_pending` 帧：`{"v":1,"type":"pairing_pending","code":"483920"}`。
pub(crate) fn pairing_pending_frame(code: &str) -> Message {
    Message::text(
        serde_json::json!({ "v": 1, "type": "pairing_pending", "code": code }).to_string(),
    )
}

/// `pairing_request` 广播帧：
/// `{"v":1,"type":"pairing_request","code":"483920","device":"<名>","expiresAt":<unix秒>}`。
pub(crate) fn pairing_request_frame(info: &PendingInfo) -> Message {
    Message::text(
        serde_json::json!({
            "v": 1,
            "type": "pairing_request",
            "code": info.code,
            "device": info.device,
            "expiresAt": info.expires_at,
        })
        .to_string(),
    )
}

// ---------------------------------------------------------------------------
// 单元测试（时间注入拨钟）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn signal_channel() -> (
        tokio::sync::mpsc::Sender<PairingSignal>,
        tokio::sync::mpsc::Receiver<PairingSignal>,
    ) {
        tokio::sync::mpsc::channel(1)
    }

    /// 生命周期：create（码形/过期时间/唯一）/ claim 原子取出（第二次
    /// None）/ 过期拒领 / 上限 4 / remove。
    #[test]
    fn pairing_store_lifecycle() {
        let store = PairingStore::default();
        let t0 = 10_000u64;

        // 1) 创建：6 位数字码、expiresAt = now + ttl。
        let (tx1, rx1) = signal_channel();
        let info1 = store
            .create("iPhone-新", tx1, t0, PAIRING_TTL_SECS)
            .unwrap();
        assert_eq!(info1.code.len(), 6, "6 位数字码");
        assert!(
            info1.code.chars().all(|c| c.is_ascii_digit()),
            "{}",
            info1.code
        );
        assert_eq!(info1.device, "iPhone-新");
        assert_eq!(info1.expires_at, t0 + PAIRING_TTL_SECS);
        assert_eq!(store.len(t0), 1);

        // 2) 码唯一：灌满上限前再创建几条，码互不相同。
        let mut codes = vec![info1.code.clone()];
        for i in 0..(MAX_PENDING_PAIRING - 1) {
            let (tx, _rx) = signal_channel();
            let info = store
                .create(&format!("dev-{i}"), tx, t0, PAIRING_TTL_SECS)
                .unwrap();
            codes.push(info.code.clone());
        }
        let mut uniq = codes.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), codes.len(), "存活码唯一：{codes:?}");

        // 3) 上限：第 5 个 pending → None。
        let (tx, _rx) = signal_channel();
        assert!(
            store.create("overflow", tx, t0, PAIRING_TTL_SECS).is_none(),
            "上限 4 pending"
        );

        // 4) claim 原子取出：命中返回设备名 + 投递口；同码再 claim → None。
        let (device, sig) = store.claim(&info1.code, t0 + 1).unwrap();
        assert_eq!(device, "iPhone-新");
        drop(sig);
        assert!(store.claim(&info1.code, t0 + 2).is_none(), "单次消费");
        // 未知码（避开已发码的确定值）→ None。
        let unknown = (0..1_000_000u32)
            .map(|n| format!("{n:06}"))
            .find(|c| !codes.contains(c))
            .unwrap();
        assert!(store.claim(&unknown, t0).is_none(), "未知码 None");

        // 5) 过期：过 TTL 后 claim → None（惰性清理）。
        let (tx2, _rx2) = signal_channel();
        let info2 = store.create("late", tx2, t0, 60).unwrap();
        assert!(store.claim(&info2.code, t0 + 61).is_none(), "过期拒领");
        assert!(store.claim(&info2.code, t0 + 61).is_none());

        // 6) remove：pending 设备断开时移除（未知码 no-op）。
        let (tx3, _rx3) = signal_channel();
        let info3 = store.create("gone", tx3, t0, 60).unwrap();
        store.remove(&info3.code);
        assert!(store.claim(&info3.code, t0 + 1).is_none());
        store.remove("999999"); // no-op 不 panic
        drop(rx1); // rx1 全程存活（claim 投递口有效性的前提）
    }

    /// 过期腾位：过期条目被 prune 后上限腾出，可再创建。
    #[test]
    fn pairing_store_expiry_frees_capacity() {
        let store = PairingStore::default();
        let t0 = 100u64;
        for i in 0..MAX_PENDING_PAIRING {
            let (tx, _rx) = signal_channel();
            store.create(&format!("d{i}"), tx, t0, 10).unwrap();
        }
        let (tx, _rx) = signal_channel();
        assert!(store.create("blocked", tx, t0 + 5, 10).is_none(), "满员");
        let (tx, _rx) = signal_channel();
        assert!(
            store.create("freed", tx, t0 + 10 + 1, 10).is_some(),
            "过期清理腾位"
        );
    }

    /// 帧形状（协议对照逐字段）。
    #[test]
    fn pairing_frames_shape() {
        let m = pairing_pending_frame("483920");
        let v: serde_json::Value = serde_json::from_str(m.into_text().unwrap().as_str()).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "pairing_pending");
        assert_eq!(v["code"], "483920");

        let info = PendingInfo {
            code: "483920".to_string(),
            device: "新 Mac".to_string(),
            expires_at: 1_900_000_000,
        };
        let m = pairing_request_frame(&info);
        let v: serde_json::Value = serde_json::from_str(m.into_text().unwrap().as_str()).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "pairing_request");
        assert_eq!(v["code"], "483920");
        assert_eq!(v["device"], "新 Mac");
        assert_eq!(v["expiresAt"], 1_900_000_000);
    }
}
