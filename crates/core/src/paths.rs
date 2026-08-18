//! 状态目录解析（P4-10 Profile 多实例）。
//!
//! 所有本地状态（config / SQLite / permission.sock / 媒体缓存）统一锚定
//! [`imagent_home`]：默认 `~/.imagent`；`--profile <name>` 时 main 在进程早期把
//! `IMAGENT_HOME` 指到 `~/.imagent/profiles/<name>`，此后各处（含被 spawn 的
//! agent 子进程，env 继承）自动隔离。env 优先于一切，便于容器/测试覆写。

use std::path::PathBuf;

/// 环境变量名：覆写状态根目录（`--profile` 的实现机制）。
pub const IMAGENT_HOME_ENV: &str = "IMAGENT_HOME";

/// 状态根目录：`IMAGENT_HOME`（非空时）否则 `~/.imagent`。
/// home 不可解析时回退 `./.imagent`（相对 cwd，极罕见）。
pub fn imagent_home() -> PathBuf {
    if let Ok(p) = std::env::var(IMAGENT_HOME_ENV) {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".imagent")
}

/// P5：媒体目录 TTL 清理——删除 mtime 早于 `cutoff` 的文件，返回删除数。
/// best-effort（单文件失败仅跳过）；目录不存在返回 0；不递归（媒体目录是平的）。
/// ilink/feishu 的入站媒体都落 `<imagent_home>/media`，只增不减会撑爆磁盘。
pub fn sweep_media_before(dir: &std::path::Path, cutoff: std::time::SystemTime) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut n = 0;
    for entry in rd.flatten() {
        let Ok(md) = entry.metadata() else {
            continue;
        };
        if !md.is_file() {
            continue;
        }
        let Ok(mtime) = md.modified() else {
            continue;
        };
        if mtime < cutoff && std::fs::remove_file(entry.path()).is_ok() {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_overrides_home() {
        // env 覆盖优先（--profile 的机制基础）。
        std::env::set_var(IMAGENT_HOME_ENV, "/tmp/imagent-profile-x");
        assert_eq!(imagent_home(), PathBuf::from("/tmp/imagent-profile-x"));
        // 空串视为未设置。
        std::env::set_var(IMAGENT_HOME_ENV, "  ");
        assert_ne!(imagent_home(), PathBuf::from("  "));
        std::env::remove_var(IMAGENT_HOME_ENV);
    }

    /// P5：TTL 清理只删 cutoff 之前的文件，目录缺失安全返回。
    #[test]
    fn sweep_media_respects_cutoff() {
        let dir = std::env::temp_dir().join(format!("imagent-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.bin"), b"x").unwrap();
        std::fs::write(dir.join("b.bin"), b"y").unwrap();
        // cutoff 在极远的过去：不删任何文件。
        assert_eq!(
            sweep_media_before(&dir, std::time::UNIX_EPOCH),
            0,
            "新文件不应被删"
        );
        assert!(dir.join("a.bin").exists());
        // cutoff 在极远的未来：全删。
        let far_future = std::time::UNIX_EPOCH + std::time::Duration::from_secs(86400 * 365 * 100);
        assert_eq!(sweep_media_before(&dir, far_future), 2);
        assert!(!dir.join("a.bin").exists());
        // 目录不存在：0，不 panic。
        assert_eq!(sweep_media_before(&dir.join("nope"), far_future), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
