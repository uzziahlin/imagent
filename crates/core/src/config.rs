//! 配置加载（`~/.imagent/config.toml`）。

use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    /// 不启用权限审批：claude 按 --allowedTools 自行处理（P1 既有行为）。
    #[default]
    Off,
    /// MCP server 永远 allow（不发 IM、不阻塞；快速放行模式）。
    Allow,
    /// MCP server 永远 deny（不发 IM、不阻塞；严格拦截模式）。
    Deny,
    /// 完整 IM approve/deny 闭环：发 IM 询问用户、等待回复路由回 MCP。
    Ask,
}

impl PermissionMode {
    /// 是否需要附加 --mcp-config / --permission-prompt-tool。
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
    /// 小写标签，用于 MCP 子命令 --mode 参数与日志。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "allow" => Self::Allow,
            "deny" => Self::Deny,
            "ask" => Self::Ask,
            _ => Self::Off,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Config {
    /// agent 工作根目录（安全边界）。必填，缺失或非绝对路径 => Config 错误。
    pub default_workdir: PathBuf,
    #[serde(default)]
    pub allowed_senders: Vec<String>,
    #[serde(default = "default_tools")]
    pub allowed_tools: Vec<String>,
    #[serde(default = "default_agent")]
    pub agent: String,
    #[serde(default = "default_platform")]
    pub platform: String,
    /// IM 权限审批模式（默认 Off，向后兼容 P1 行为）。
    #[serde(default)]
    pub permission_mode: PermissionMode,
}

fn default_tools() -> Vec<String> {
    vec!["Read".into(), "Edit".into()]
}
fn default_agent() -> String {
    "claude-cli".into()
}
fn default_platform() -> String {
    "ilink".into()
}

impl Config {
    /// 默认配置文件路径：`~/.imagent/config.toml`。
    pub fn default_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".imagent").join("config.toml"))
    }

    /// 读取并解析。文件不存在 => `CoreError::Config`。
    /// `default_workdir` 缺失或非绝对路径 => `CoreError::Config`（给出清晰提示）。
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(CoreError::Io)?;
        let cfg: Self = toml::from_str(&raw)
            .map_err(|e| CoreError::Config(format!("parse {}: {e}", path.display())))?;

        if cfg.default_workdir.as_os_str().is_empty() || !cfg.default_workdir.is_absolute() {
            return Err(CoreError::Config(format!(
                "default_workdir 必须是绝对路径，当前为 {:?}。请参考 EXAMPLE 模板。",
                cfg.default_workdir
            )));
        }

        Ok(cfg)
    }

    /// 供首次使用打印的模板字符串（default_workdir 用占位，不写死任何机器路径）。
    pub const EXAMPLE: &'static str = r#"# ~/.imagent/config.toml
default_workdir = "/absolute/path/to/agent/workspace"   # 必填，agent 只能在该目录 Read/Edit
allowed_senders = []        # 留空 = 发现模式（只打日志记录入站 sender，不驱动 agent）
allowed_tools = ["Read", "Edit"]
agent = "claude-cli"
platform = "ilink"
permission_mode = "off"     # off(默认,claude按allowedTools自行处理) | allow | deny | ask(IM审批闭环)
"#;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_path(name: &str, body: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "imagent_core_cfg_{}_{}.toml",
            std::process::id(),
            name
        ));
        let _ = std::fs::File::create(&p).and_then(|mut f| f.write_all(body.as_bytes()));
        p
    }

    fn cleanup(p: &Path) {
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn permission_mode_default_off() {
        let p = tmp_path(
            "cfg_perm_default",
            "default_workdir = \"/tmp/ws\"\nallowed_tools = [\"Read\"]\n",
        );
        let cfg = Config::load(&p).expect("parse");
        assert_eq!(cfg.permission_mode, PermissionMode::Off);
        cleanup(&p);
    }

    #[test]
    fn permission_mode_parses() {
        for (raw, expect) in [
            ("ask", PermissionMode::Ask),
            ("allow", PermissionMode::Allow),
            ("deny", PermissionMode::Deny),
            ("off", PermissionMode::Off),
        ] {
            let p = tmp_path(
                "cfg_perm",
                &format!("default_workdir = \"/tmp/ws\"\npermission_mode = \"{raw}\"\n"),
            );
            let cfg = Config::load(&p).expect("parse");
            assert_eq!(cfg.permission_mode, expect, "raw={raw}");
            cleanup(&p);
        }
    }

    #[test]
    fn parses_full() {
        let p = tmp_path(
            "full",
            r#"default_workdir = "/tmp/ws"
allowed_senders = ["u1"]
allowed_tools = ["Read"]
agent = "claude-cli"
platform = "ilink"
"#,
        );
        let cfg = Config::load(&p).expect("ok");
        assert_eq!(cfg.allowed_senders, vec!["u1".to_string()]);
        assert_eq!(cfg.allowed_tools, vec!["Read".to_string()]);
        assert_eq!(cfg.agent, "claude-cli");
        cleanup(&p);
    }

    #[test]
    fn applies_defaults() {
        let p = tmp_path("def", r#"default_workdir = "/tmp/ws""#);
        let cfg = Config::load(&p).expect("ok");
        assert!(cfg.allowed_senders.is_empty());
        assert_eq!(
            cfg.allowed_tools,
            vec!["Read".to_string(), "Edit".to_string()]
        );
        assert_eq!(cfg.agent, "claude-cli");
        assert_eq!(cfg.platform, "ilink");
        cleanup(&p);
    }

    #[test]
    fn rejects_relative_workdir() {
        let p = tmp_path("rel", r#"default_workdir = "relative/path""#);
        let err = Config::load(&p).unwrap_err();
        assert!(matches!(err, CoreError::Config(_)), "{err:?}");
        cleanup(&p);
    }

    #[test]
    fn missing_file_is_err() {
        let mut nope = std::env::temp_dir();
        nope.push("imagent_core_cfg_does_not_exist.toml");
        let err = Config::load(&nope).unwrap_err();
        assert!(matches!(err, CoreError::Io(_)), "{err:?}");
    }
}
