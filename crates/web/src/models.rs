//! 模型目录动态化（协议 v1.8）：CLI 配置优先 + 网关官方列表回退。
//!
//! Web 驾驶舱命令面板的 /model 快捷 chips 三层来源：
//! 1. **CLI（Claude Code）配置映射**（最高优先）：用户用 ccswitch 切第三方
//!    供应商时，`~/.claude/settings.json` 的 `env` 块会写
//!    `ANTHROPIC_DEFAULT_{SONNET,OPUS,HAIKU}_MODEL`（别名 → 实际模型 id
//!    映射；裸名 `..._SONNET` 等宽容别名兼容）——有映射的别名成为菜单项
//!    （label=别名，target=映射值，SPA title 提示）；
//! 2. **网关 config 官方列表回退**（`official_models`，缺省
//!    sonnet/opus/haiku）：无任何映射（官方直连形态）时菜单项即本列表
//!    （label=官方名，target=null）；
//! 3. **SPA 内置兜底**：网关未下发 models 段（空 options 整段缺席，如
//!    official_models 配空）时 SPA 用自己的内置列表（见 web-ui lib/commands）。
//!
//! 由网关统一汇聚（[`resolve_models`]，`current` 取 backend.model()——/model
//! 命令热设值）后随 workspaces 快照下发（platform 的 workspace 泵，每次组装
//! 经 [`read_cli_models`] 现读 settings 文件——低频快照操作，不做缓存）。
//!
//! 本模块全部为可单测的纯逻辑（文件读取入参 [`read_cli_models_at`] 可注入，
//! 不锚定真实 home——测试不依赖运行机器的 ~/.claude 状态）。

use std::collections::BTreeMap;
use std::path::Path;

use imagent_core::Backend;
use std::sync::Arc;

use crate::{ModelOption, ModelsFrame};

/// 三别名固定序（协议惯例：菜单项按 sonnet → opus → haiku 排列；BTreeMap
/// 键序虽稳定但字母序会打成 haiku/opus/sonnet，显式钉住展示序）。
pub(crate) const MODEL_ALIASES: [&str; 3] = ["sonnet", "opus", "haiku"];

/// CLI（`~/.claude/settings.json`）声明的模型映射快照（[`read_cli_models`]
/// 产物）。全空 = 官方直连形态（无第三方映射）。
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CliModels {
    /// `env.ANTHROPIC_MODEL`（CLI 侧默认模型；无/空白 → None）。
    pub cli_default: Option<String>,
    /// 别名（sonnet/opus/haiku 小写）→ 实际模型 id。仅有非空字符串值的别名。
    pub aliases: BTreeMap<String, String>,
}

/// 读 CLI 配置的模型映射：`<home>/.claude/settings.json` 的 `env` 块提取
/// `ANTHROPIC_MODEL`（cliDefault）与 `ANTHROPIC_DEFAULT_SONNET/OPUS/HAIKU`
/// （别名→实际 id）。**绝不 Err**：文件不存在 / JSON 解析失败 / env 块缺失
/// → 全空（静默回退官方列表——模型菜单是展示面，坏配置不值得刷日志打搅）。
pub(crate) fn read_cli_models() -> CliModels {
    match dirs::home_dir() {
        Some(home) => read_cli_models_at(&home.join(".claude").join("settings.json")),
        None => CliModels::default(),
    }
}

/// [`read_cli_models`] 的路径注入形态（单测/集成测试用临时文件，不锚定
/// 真实 home）。容错语义同上：任何读/解析失败 → 全空。
pub(crate) fn read_cli_models_at(path: &Path) -> CliModels {
    match std::fs::read_to_string(path) {
        Ok(raw) => parse_cli_models(&raw),
        Err(_) => CliModels::default(),
    }
}

/// settings.json 文本 → [`CliModels`]（纯函数）：顶层 `env` 对象提取两族键，
/// 值 trim 后非空才算（ccswitch 写空串 = 未映射）；env 块缺失 / 整体非 JSON
/// → 全空。
///
/// 别名映射的键名取**实证形态** `ANTHROPIC_DEFAULT_{SONNET|OPUS|HAIKU}_MODEL`
///（Claude Code / ccswitch 实际写入的 env 键——本机 `~/.claude/settings.json`
/// 可证；任务说明里的裸名 `ANTHROPIC_DEFAULT_SONNET` 等作为宽容别名兼容，
/// 两形态并存时 `_MODEL` 形态优先）。
pub(crate) fn parse_cli_models(raw: &str) -> CliModels {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return CliModels::default(),
    };
    let Some(env) = v.get("env").and_then(|e| e.as_object()) else {
        return CliModels::default();
    };
    let nonempty = |s: &str| -> Option<String> {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    let cli_default = env
        .get("ANTHROPIC_MODEL")
        .and_then(|m| m.as_str())
        .and_then(nonempty);
    let mut aliases = BTreeMap::new();
    for alias in MODEL_ALIASES {
        let upper = alias.to_uppercase();
        let canonical = format!("ANTHROPIC_DEFAULT_{upper}_MODEL");
        let bare = format!("ANTHROPIC_DEFAULT_{upper}");
        let target = env
            .get(&canonical)
            .or_else(|| env.get(&bare))
            .and_then(|m| m.as_str())
            .and_then(nonempty);
        if let Some(target) = target {
            aliases.insert(alias.to_string(), target);
        }
    }
    CliModels {
        cli_default,
        aliases,
    }
}

/// 汇聚 models 段（纯函数，字节级契约见 lib.rs [`ModelsFrame`]）：
/// - 映射 ≥1 条 → options = 三别名中**有映射的**那些（{label: 别名,
///   target: Some(映射值)}；无映射的别名不出——切过去是无人消费的空别名）；
/// - 无任何映射 → options = official 列表（{label: 官方名, target: None}）；
/// - official 为空且无映射 → options 空（整段缺席，SPA 第③层兜底）。
///
/// `current` 为 backend.model()（/model 命令热设值；None = 未设置，跟随 CLI
/// 默认），原样进帧（serde 按可空字符串序列化，契约 `"current":null` 形态）。
pub(crate) fn resolve_models(
    cli: &CliModels,
    official: &[String],
    current: Option<String>,
) -> ModelsFrame {
    let options = if !cli.aliases.is_empty() {
        MODEL_ALIASES
            .iter()
            .filter_map(|alias| {
                cli.aliases.get(*alias).map(|target| ModelOption {
                    label: (*alias).to_string(),
                    target: Some(target.clone()),
                })
            })
            .collect()
    } else {
        official
            .iter()
            .map(|name| ModelOption {
                label: name.clone(),
                target: None,
            })
            .collect()
    };
    ModelsFrame {
        current,
        cli_default: cli.cli_default.clone(),
        options,
    }
}

/// 模型目录装配上下文（[`crate::WebPlatform`] 构造注入，main 装配处传现有
/// backend clone + config `official_models`）：workspace 泵每次组快照时用它
/// 读 CLI 映射 + 取 backend.model() 组 models 段。测试注入 mock backend 与
/// 受控 official 列表（不依赖运行机器的 ~/.claude 状态）。
pub struct ModelsContext {
    /// backend 句柄（models.current 数据源；不执行 run——只读 model()）。
    pub backend: Arc<dyn Backend>,
    /// config 官方回退列表（official_models）。
    pub official: Vec<String>,
}

#[cfg(test)]
mod tests {
    //! 纯函数单测（任务验收项）：第三方映射子集 / 官方回退 / 坏 JSON 与缺
    //! 文件容错 / 空 official；settings.json 解析样例见
    //! `parse_settings_example`（ccswitch 第三方供应商形态）。

    use super::*;

    /// 官方回退列表测试基准。
    fn official() -> Vec<String> {
        vec!["sonnet".into(), "opus".into(), "haiku".into()]
    }

    #[test]
    fn parse_settings_example() {
        // ccswitch 切第三方供应商形态（本机 ~/.claude/settings.json 实证键名）：
        // env 块带 ANTHROPIC_MODEL + 三别名中两别名有 _MODEL 后缀映射
        //（haiku 未映射）+ 无关 env 键共存。
        let raw = r#"{
            "permissions": {"allow": ["Bash"]},
            "env": {
                "ANTHROPIC_MODEL": "glm-4.6",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm-4.6",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-4.7",
                "ANTHROPIC_BASE_URL": "https://open.bigmodel.cn/api/anthropic",
                "SHELL": "/bin/zsh"
            }
        }"#;
        let cli = parse_cli_models(raw);
        assert_eq!(cli.cli_default.as_deref(), Some("glm-4.6"));
        assert_eq!(
            cli.aliases,
            BTreeMap::from([
                ("sonnet".to_string(), "glm-4.6".to_string()),
                ("opus".to_string(), "glm-4.7".to_string()),
            ]),
            "仅有映射的别名进表（haiku 缺席），无关 env 键忽略"
        );

        // 裸名别名（任务说明字面形态）同样接受；两形态并存 _MODEL 优先。
        let bare_only = parse_cli_models(r#"{"env": {"ANTHROPIC_DEFAULT_HAIKU": "glm-4.5-air"}}"#);
        assert_eq!(
            bare_only.aliases,
            BTreeMap::from([("haiku".to_string(), "glm-4.5-air".to_string())]),
            "裸名 ANTHROPIC_DEFAULT_HAIKU 兼容"
        );
        let both = parse_cli_models(
            r#"{"env": {
                "ANTHROPIC_DEFAULT_SONNET": "bare-value",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "model-value"
            }}"#,
        );
        assert_eq!(
            both.aliases.get("sonnet").map(String::as_str),
            Some("model-value"),
            "_MODEL 实证形态优先于裸名"
        );
    }

    #[test]
    fn parse_tolerates_missing_env_blank_values_and_non_string() {
        // 无 env 块 / env 非对象 / 空串值 / 空白串 / 非字符串值 → 全部容忍。
        let cli = parse_cli_models(r#"{"permissions": {}}"#);
        assert_eq!(cli, CliModels::default(), "无 env 块 → 全空");
        let cli = parse_cli_models(r#"{"env": "not-an-object"}"#);
        assert_eq!(cli, CliModels::default(), "env 非对象 → 全空");
        let cli = parse_cli_models(
            r#"{"env": {"ANTHROPIC_MODEL": "  ", "ANTHROPIC_DEFAULT_SONNET": ""}}"#,
        );
        assert_eq!(cli, CliModels::default(), "空白/空串值视为未映射");
        let cli = parse_cli_models(
            r#"{"env": {"ANTHROPIC_MODEL": 42, "ANTHROPIC_DEFAULT_HAIKU": ["x"]}}"#,
        );
        assert_eq!(cli, CliModels::default(), "非字符串值视为未映射");
        // trim 生效：前后空白剥掉。
        let cli = parse_cli_models(r#"{"env": {"ANTHROPIC_MODEL": " glm-4.6 "}}"#);
        assert_eq!(cli.cli_default.as_deref(), Some("glm-4.6"));
    }

    #[test]
    fn read_cli_models_at_tolerates_bad_json_and_missing_file() {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "imagent-web-models-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // 缺文件（尚未写入）→ 全空，不 panic 不 Err。
        assert_eq!(read_cli_models_at(&p), CliModels::default());
        // 坏 JSON → 全空。
        std::fs::write(&p, "{ not json !!!").unwrap();
        assert_eq!(read_cli_models_at(&p), CliModels::default());
        // 合法形态 → 正常解析。
        std::fs::write(&p, r#"{"env": {"ANTHROPIC_DEFAULT_SONNET": "glm-4.6"}}"#).unwrap();
        let cli = read_cli_models_at(&p);
        assert_eq!(cli.cli_default, None);
        assert_eq!(cli.aliases.len(), 1);
        std::fs::remove_file(&p).ok();
    }

    /// 第三方映射（2/3 子集）：options 只含有映射的别名（展示序
    /// sonnet→opus→haiku），target=映射值；current / cliDefault 原样进帧。
    #[test]
    fn resolve_mapped_subset() {
        let cli = CliModels {
            cli_default: Some("glm-4.6".into()),
            aliases: BTreeMap::from([
                ("haiku".to_string(), "glm-4.5-air".to_string()),
                ("sonnet".to_string(), "glm-4.6".to_string()),
            ]),
        };
        let frame = resolve_models(&cli, &official(), Some("glm-4.7".into()));
        assert_eq!(frame.current.as_deref(), Some("glm-4.7"));
        assert_eq!(frame.cli_default.as_deref(), Some("glm-4.6"));
        // 展示序按 MODEL_ALIASES（sonnet 在前），opus（无映射）不出。
        assert_eq!(
            frame
                .options
                .iter()
                .map(|o| (o.label.as_str(), o.target.clone()))
                .collect::<Vec<_>>(),
            vec![
                ("sonnet", Some("glm-4.6".to_string())),
                ("haiku", Some("glm-4.5-air".to_string())),
            ]
        );
    }

    /// 官方回退：无任何映射 → options = official 列表（target 全 None）；
    /// current None 原样（契约 `"current":null`）。
    #[test]
    fn resolve_official_fallback() {
        let frame = resolve_models(&CliModels::default(), &official(), None);
        assert_eq!(frame.current, None);
        assert_eq!(frame.cli_default, None);
        assert_eq!(
            frame
                .options
                .iter()
                .map(|o| (o.label.as_str(), o.target.is_none()))
                .collect::<Vec<_>>(),
            vec![("sonnet", true), ("opus", true), ("haiku", true)],
            "官方回退 target 恒 None"
        );
        // 有 ANTHROPIC_MODEL 但无别名映射：仍走官方回退（cliDefault 只影响
        // SPA 行尾「默认 · x」提示，不构成映射）。
        let cli = CliModels {
            cli_default: Some("glm-4.6".into()),
            aliases: BTreeMap::new(),
        };
        let frame = resolve_models(&cli, &official(), None);
        assert_eq!(frame.options.len(), 3, "无别名映射即官方回退");
        assert!(frame.options.iter().all(|o| o.target.is_none()));
    }

    /// 空 official 且无映射 → options 空（整段缺席语义，序列化面见
    /// lib.rs 字节级测试）。
    #[test]
    fn resolve_empty_official_yields_empty_options() {
        let frame = resolve_models(&CliModels::default(), &[], None);
        assert!(frame.options.is_empty());
        // 映射存在则不受 official 空影响（第①层优先）。
        let cli = CliModels {
            cli_default: None,
            aliases: BTreeMap::from([("sonnet".to_string(), "glm-4.6".to_string())]),
        };
        let frame = resolve_models(&cli, &[], None);
        assert_eq!(frame.options.len(), 1);
        assert_eq!(frame.options[0].label, "sonnet");
        assert_eq!(frame.options[0].target.as_deref(), Some("glm-4.6"));
    }
}
