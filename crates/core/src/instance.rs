//! 单实例锁（P5-9）：`<imagent_home>/instance.lock`。
//!
//! 同一 IMAGENT_HOME 跑第二个实例会互劫持资源——最危险的是 permission.sock：
//! 第二实例启动时无条件删旧 socket 文件重 bind，第一实例的 Ask 审批闭环**静默
//! 失效**（accept 仍挂在旧 listener 上，MCP 子进程却连到了新实例）。
//!
//! 实现为 `flock(LOCK_EX | LOCK_NB)`（P5-第五批修正：早期版本用「排他创建 +
//! 事后写 PID + 存活探测」，两实例毫秒级并发启动时败者可能读到未写完的锁文件
//! 误判陈旧而删锁重建——恰好在要防的场景失效）。flock 由内核随 fd 持有到进程
//! 退出（含崩溃），天然无「陈旧锁」与删除竞态；锁文件内容仅作诊断信息（PID）。
//! 仅 `imagent start` 获取；`imagent mcp` 等子命令与主进程共存，不得加锁。

use std::io::Write;
use std::path::Path;

use crate::error::{CoreError, Result};

/// 获取单实例锁。返回的 File 须持有到进程结束（drop / 进程退出即释放锁）。
/// 已有实例持锁 → `Err`（附诊断指引）。
pub fn acquire(home: &Path) -> Result<std::fs::File> {
    // D11：非 unix（Windows）显式不支持——此前 try_flock 恒 false 会让**首次
    // 启动**也被当成「已有实例持锁」拒绝，与注释语义矛盾。项目明确不做
    // Windows（permission.sock 本就 unix-only），直接给出可读错误。
    #[cfg(not(unix))]
    {
        let _ = home;
        return Err(CoreError::Config(
            "Windows 不受支持：单实例锁依赖 flock（unix），且权限审批闭环的 \
             Unix domain socket 也仅在 macOS/Linux 可用。请在 macOS/Linux 运行。"
                .into(),
        ));
    }
    #[cfg(unix)]
    let lock = home.join("instance.lock");
    // O_CREAT（非 create_new）：锁文件可以复用，互斥由 flock 保证而非文件存在性。
    // L10（code-review v8）：open 不带 truncate——先 truncate 会把第一实例写入
    // 的 PID 清空，flock 失败的诊断恒为「pid 未知」；拿到锁之后再清写 PID。
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        // 显式不截断（L10：先 truncate 会清掉第一实例的 PID；拿到锁后 set_len(0)）
        .truncate(false)
        .open(&lock)
        .map_err(CoreError::Io)?;
    if !try_flock_exclusive(&f) {
        // 锁文件内容（持有者 PID）给排障用。
        let holder = std::fs::read_to_string(&lock)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .map(|pid| format!("pid={pid}"))
            .unwrap_or_else(|| "pid 未知".into());
        return Err(CoreError::Config(format!(
            "已有一个 imagent 实例正在运行（{holder}，锁 {}）。\
             同一 IMAGENT_HOME 不允许双实例：第二个实例会劫持 permission.sock，\
             使第一个实例的 Ask 审批闭环静默失效。",
            lock.display()
        )));
    }
    // 写 PID 供诊断（失败不致命——互斥不依赖文件内容）。L10：锁到手后先清旧
    // 内容（复用锁文件时上一持有者的 PID 不应残留）。
    let _ = f.set_len(0);
    let _ = writeln!(f, "{}", std::process::id());
    Ok(f)
}

/// `flock(fd, LOCK_EX | LOCK_NB)`：成功 true；已被其它实例（或本进程其它 fd）
/// 持有返回 false（EWOULDBLOCK）；其它错误按持锁失败（保守拒绝启动）处理。
#[cfg(unix)]
#[allow(unsafe_code)] // 局部豁免先例同 dispatch::peer_uid（lib 顶层 deny(unsafe_code)）
fn try_flock_exclusive(f: &std::fs::File) -> bool {
    use std::os::unix::io::AsRawFd;
    // SAFETY: flock(2) 对有效 fd 做 advisory 锁操作，无指针解引用；
    // LOCK_NB 保证不阻塞。
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    rc == 0
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "imagent_instance_{}_{}_{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn second_acquire_refuses_while_holder_alive() {
        let home = tmp_home("flock_alive");
        let _guard = acquire(&home).expect("首个获取应成功");
        let err = acquire(&home).expect_err("持锁期间第二次应拒绝");
        let msg = format!("{err}");
        assert!(msg.contains("已有一个"), "应提示已有实例: {msg}");
        assert!(msg.contains("permission.sock"), "应说明危害: {msg}");
        // 释放后（模拟持有者退出）可再次获取。
        drop(_guard);
        let _again = acquire(&home).expect("释放后应可重新获取");
    }

    #[test]
    fn stale_file_without_holder_is_taken_over() {
        // 残留锁文件（无人持锁）：flock 直接成功——无需 PID 探测。
        let home = tmp_home("flock_stale");
        std::fs::write(home.join("instance.lock"), "999999\n").unwrap();
        let _guard = acquire(&home).expect("无人持锁的残留文件应直接接管");
    }

    #[test]
    fn garbage_file_still_locks_correctly() {
        let home = tmp_home("flock_garbage");
        std::fs::write(home.join("instance.lock"), "not-a-pid\n").unwrap();
        let _guard = acquire(&home).expect("内容异常不影响 flock 语义");
        assert!(acquire(&home).is_err(), "持锁后第二次仍应拒绝");
    }
}
