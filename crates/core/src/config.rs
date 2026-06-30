//! 配置加载（`~/.imagent/config.toml`）。

use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};

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
            name,
            std::process::id()
        ));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    fn cleanup(p: &Path) {
        let _ = std::fs::remove_file(p);
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
        assert_eq!(cfg.allowed_tools, vec!["Read".to_string(), "Edit".to_string()]);
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
