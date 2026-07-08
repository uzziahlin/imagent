//! OS keyring 凭据加密封装（DESIGN §9.4）。
//!
//! 真凭据优先写入 OS keyring（macOS Keychain / Linux secret-service /
//! Windows Credential Manager），SQLite `credentials.blob` 仅存一个标记
//! `"keyring:{platform}:{account_id}"`，表示真值在 keyring。无 OS keychain
//! （headless/CI 无 D-Bus、沙箱无 GUI）时 fallback 明文存 SQLite + `warn!`，
//! 不阻断、不 panic。
//!
//! keyring entry 约定：`service = "imagent"`，`username = "{platform}:{account_id}"`。
//!
//! keychain 访问是**阻塞**系统调用，且在某些环境（macOS 沙箱无 GUI）会**永久
//! 阻塞**。故 keychain 工作下放到**游离 `std::thread`**（非 tokio 阻塞池），
//! 主流程用 `recv_timeout` 等待：超时即 fallback 明文。游离线程若卡死不会拖住
//! tokio 运行时关闭，进程退出时由 OS 回收。

use std::sync::mpsc;
use std::time::Duration;

use keyring::Entry;

const KEYRING_SERVICE: &str = "imagent";
/// blob 形如 `"keyring:{platform}:{account_id}"` 表示真实凭据在 keyring，非明文。
pub(crate) const KEYRING_MARKER_PREFIX: &str = "keyring:";
/// 单次 keychain 操作的硬超时：超时即判失败、回退明文（避免沙箱/无 GUI 环境挂起）。
const KEYRING_OP_TIMEOUT: Duration = Duration::from_secs(3);

/// 构造 keyring entry（service 固定 `"imagent"`，username = `"{platform}:{account_id}"`）。
/// `Entry::new` 仅创建 specifier，不访问 keychain；返回 `None` 表示无可用 backend。
fn entry(platform: &str, account_id: &str) -> Option<Entry> {
    Entry::new(KEYRING_SERVICE, &format!("{platform}:{account_id}")).ok()
}

/// 生成写入 SQLite `credentials.blob` 的 marker，表示真凭据在 keyring。
pub(crate) fn marker_for(platform: &str, account_id: &str) -> String {
    format!("{KEYRING_MARKER_PREFIX}{platform}:{account_id}")
}

/// 判断 SQLite blob 是否为 keyring marker（真值在 keyring，非明文）。
pub(crate) fn is_keyring_marker(blob: &str) -> bool {
    blob.starts_with(KEYRING_MARKER_PREFIX)
}

/// 在游离线程执行 `f`，最多等待 `KEYRING_OP_TIMEOUT`。超时 / 失败返回 `None`。
/// 用游离线程（非 tokio 阻塞池）确保卡死的 keychain 调用不拖住运行时关闭。
fn run_with_timeout<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = mpsc::sync_channel::<T>(1);
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(KEYRING_OP_TIMEOUT).ok()
}

/// 尝试把真实 blob 写入 keyring。成功返回 `true`；失败 / 超时 / 无 backend 返回
/// `false`，调用方应 fallback 明文。
pub(crate) async fn store_in_keyring(platform: &str, account_id: &str, blob: &str) -> bool {
    if cfg!(test) {
        return false;
    }
    let key = format!("{platform}:{account_id}");
    let (p, a, b) = (
        platform.to_string(),
        account_id.to_string(),
        blob.to_string(),
    );
    // Ok(true)=成功；Ok(false)=失败（有错误信息）；None=超时。
    let res = run_with_timeout(move || -> bool {
        let Some(e) = entry(&p, &a) else {
            return false;
        };
        e.set_password(&b).is_ok()
    });
    match res {
        Some(true) => true,
        Some(false) => {
            tracing::warn!(
                target: "store", key = %key,
                "keyring 写入失败，回退明文存储（headless/CI 正常）"
            );
            false
        }
        None => {
            tracing::warn!(
                target: "store", key = %key,
                "keyring 写入超时（{}s，可能无 GUI/沙箱），回退明文存储",
                KEYRING_OP_TIMEOUT.as_secs()
            );
            false
        }
    }
}

/// 从 keyring 读取真实 blob。
/// - `Some(s)`：命中；
/// - `None`：keyring 中无此条目（`NoEntry`）、超时、或 keychain 不可用。
pub(crate) async fn load_from_keyring(platform: &str, account_id: &str) -> Option<String> {
    if cfg!(test) {
        return None;
    }
    let key = format!("{platform}:{account_id}");
    let (p, a) = (platform.to_string(), account_id.to_string());
    // 三态：Some(Ok(s))=命中；Some(Err(NoEntry))=无此条目（静默）；Some(Err(other))=失败；None=超时。
    enum KRes {
        Hit(String),
        NoEntry,
        Failed,
    }
    let res = run_with_timeout(move || -> KRes {
        let Some(e) = entry(&p, &a) else {
            return KRes::NoEntry;
        };
        match e.get_password() {
            Ok(s) => KRes::Hit(s),
            Err(keyring::Error::NoEntry) => KRes::NoEntry,
            Err(_) => KRes::Failed,
        }
    });
    match res {
        Some(KRes::Hit(s)) => Some(s),
        Some(KRes::NoEntry) => None,
        Some(KRes::Failed) => {
            tracing::warn!(target: "store", key = %key, "keyring 读取失败");
            None
        }
        None => {
            tracing::warn!(
                target: "store", key = %key,
                "keyring 读取超时（{}s），可能无 GUI/沙箱", KEYRING_OP_TIMEOUT.as_secs()
            );
            None
        }
    }
}

/// 删除 keyring 条目（P2-10，best-effort）。无条目/不可用静默返回。
pub(crate) async fn delete_from_keyring(platform: &str, account_id: &str) {
    if cfg!(test) {
        return;
    }
    let (p, a) = (platform.to_string(), account_id.to_string());
    let _ = run_with_timeout(move || -> bool {
        let Some(e) = entry(&p, &a) else {
            return false;
        };
        e.delete_credential().is_ok()
    });
}
