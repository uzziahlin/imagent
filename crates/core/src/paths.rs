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
}
