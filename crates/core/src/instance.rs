//! 单实例锁（P5-9）：`<imagent_home>/instance.lock`。
//!
//! 同一 IMAGENT_HOME 跑第二个实例会互劫持资源——最危险的是 permission.sock：
//! 第二实例启动时无条件删旧 socket 文件重 bind，第一实例的 Ask 审批闭环**静默
//! 失效**（accept 仍挂在旧 listener 上，MCP 子进程却连到了新实例）。锁 = 排他
//! 创建 + PID 存活探测（持有者已死时陈旧锁自动接管，崩溃不留死锁）。
//!
//! 仅 `imagent start` 获取；`imagent mcp` 等子命令与主进程共存，不得加锁。

use std::io::Write;
use std::path::Path;

use crate::error::{CoreError, Result};

/// 获取单实例锁。返回的 File 须持有到进程结束（drop 即释放锁）。
/// 已有存活实例 → `Err`（附 PID 与处理指引）；陈旧锁（持有者已退出/PID 无法
/// 解析）删除后接管。
pub fn acquire(home: &Path) -> Result<std::fs::File> {
    let lock = home.join("instance.lock");
    // 两轮：首轮撞「已存在」且判定陈旧 → 删后重试一轮。
    for round in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(mut f) => {
                // 写 PID 供后续存活探测；写失败不致命（锁文件本身已排他创建）。
                let _ = writeln!(f, "{}", std::process::id());
                return Ok(f);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && round == 0 => {
                let pid = std::fs::read_to_string(&lock)
                    .ok()
                    .and_then(|s| s.trim().parse::<i32>().ok());
                match pid {
                    Some(pid) if pid > 0 && process_alive(pid) => {
                        return Err(CoreError::Config(format!(
                            "已有一个 imagent 实例正在运行（pid={pid}，锁 {}）。\
                             同一 IMAGENT_HOME 不允许双实例：第二个实例会劫持 \
                             permission.sock，使第一个实例的 Ask 审批闭环静默失效。\
                             如确认无实例在跑，删除该锁文件后重试。",
                            lock.display()
                        )));
                    }
                    _ => {
                        // 陈旧（持有者已退出 / 内容异常）：删除接管。
                        let _ = std::fs::remove_file(&lock);
                    }
                }
            }
            Err(e) => return Err(CoreError::Io(e)),
        }
    }
    Err(CoreError::Config(format!(
        "获取单实例锁失败（两轮尝试后仍冲突）：{}",
        lock.display()
    )))
}

/// PID 是否存活。`kill(pid, 0)` 不发信号、仅做存在性/权限检查：0 = 存活；
/// EPERM = 存在但属其它用户（保守按存活处理）；ESRCH = 不存在。
#[cfg(unix)]
#[allow(unsafe_code)] // 局部豁免先例同 dispatch::peer_uid（lib 顶层 deny(unsafe_code)）
fn process_alive(pid: i32) -> bool {
    // SAFETY: kill(2) 以信号 0 调用仅做权限/存在性检查，不产生信号，
    // 不涉及任何指针解引用或内存安全问题。
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_alive(_pid: i32) -> bool {
    // 非 unix 无法探测：保守视为存活（拒绝接管，由错误信息引导手动清锁）。
    true
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
        let home = tmp_home("alive");
        let _guard = acquire(&home).expect("首个获取应成功");
        let err = acquire(&home).expect_err("持有者存活时第二次应拒绝");
        let msg = format!("{err}");
        assert!(msg.contains("已有一个"), "应提示已有实例: {msg}");
        assert!(msg.contains("permission.sock"), "应说明危害: {msg}");
    }

    #[test]
    fn stale_lock_is_taken_over() {
        let home = tmp_home("stale");
        // 构造陈旧锁：spawn 一个立即退出的进程，用其（已死的）pid 写锁。
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn sh");
        let pid = child.id() as i32;
        child.wait().unwrap();
        assert!(
            !process_alive(pid),
            "辅助进程应已退出（前提失效则测试无效）"
        );
        std::fs::write(home.join("instance.lock"), format!("{pid}\n")).unwrap();
        // 陈旧锁被接管，且新持有者（本进程）存活 → 再取被拒。
        let _guard = acquire(&home).expect("陈旧锁应被接管");
        assert!(acquire(&home).is_err(), "接管后第二次应拒绝");
    }

    #[test]
    fn garbage_lock_content_is_taken_over() {
        let home = tmp_home("garbage");
        std::fs::write(home.join("instance.lock"), "not-a-pid\n").unwrap();
        let _guard = acquire(&home).expect("无法解析 PID 的锁应视为陈旧并接管");
    }
}
