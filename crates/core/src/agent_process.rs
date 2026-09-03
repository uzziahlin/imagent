//! agent 子进程策略（跨后端共享的唯一事实来源）。
//!
//! 背景（v1.18 review 设计项）：CLI 路径（`spawn_cli_backend` 的
//! `env_clear` + 透传）与 ACP 路径（`/usr/bin/env -i NAME=value ...` argv
//! 注入）此前各自维护一份 env 白名单与值校验——v8 H2 的教训正是「同一防线
//! 修了旧路径漏了新路径」。两路径从此模块取同一份清单与校验。
//!
//! 行长上限（CLI 8MB/64KB）**不在此列**：ACP 路径的 stdout/stderr 由 SDK
//! `connect_with` 自持（无注入点，v8 M2 已评估并文档化），统一需上游配合。

/// 运行 agent 子进程所需的最小环境变量集（S-2 消毒后仅透传这些；各 backend
/// 的凭据类 key 由调用方经 `passthrough_env` / ACP 命令前导另行声明）。
pub const AGENT_RUNTIME_ENV: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR",
];

/// env 值安全性：ACP 路径把 `NAME=value` 作为 argv 注入 `/usr/bin/env`，值含
/// 空白/引号/特殊字符会拆参改变 spawn 行为（甚至注入额外参数）——不安全值
/// 跳过注入并告警（调用方负责）。CLI 路径经 `Command::env` 注入无此约束，
/// 但复用同一校验可避免两边语义分叉。
pub fn env_value_safe(v: &str) -> bool {
    !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/:=+-@[]".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_env_covers_process_basics() {
        // 与旧两处白名单逐项一致（漂移即测试失败）。
        for key in [
            "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR",
        ] {
            assert!(AGENT_RUNTIME_ENV.contains(&key), "缺 {key}");
        }
        assert_eq!(AGENT_RUNTIME_ENV.len(), 9, "白名单应恰好 9 项");
    }

    #[test]
    fn env_value_safe_rejects_argv_hazards() {
        assert!(env_value_safe("/usr/local/bin"));
        assert!(env_value_safe("en_US.UTF-8"));
        assert!(env_value_safe("glm-5.3[1M]"));
        assert!(!env_value_safe(""), "空值不安全");
        assert!(!env_value_safe("a b"), "空白拆参");
        assert!(!env_value_safe("a;b"), "shell 元字符");
        assert!(!env_value_safe("$(x)"), "命令替换");
        assert!(!env_value_safe("a'b"), "引号");
    }
}
