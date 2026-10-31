//! 模型目录动态化（协议 v1.8 + codex-app 批扩展）：CLI 配置优先 + 实时目录 +
//! 网关官方列表回退，**按 backend 分链**。
//!
//! Web 驾驶舱命令面板的 /model 快捷 chips 三层来源——claude 系后端
//!（claude-cli / claude-acp / acp 及历史单列表消费者）：
//! 1. **CLI（Claude Code）配置映射**（最高优先）：用户用 ccswitch 切第三方
//!    供应商时，`~/.claude/settings.json` 的 `env` 块会写
//!    `ANTHROPIC_DEFAULT_{SONNET,OPUS,HAIKU}_MODEL`（别名 → 实际模型 id
//!    映射；裸名 `..._SONNET` 等宽容别名兼容）——有映射的别名成为菜单项
//!    （label=别名，target=映射值，SPA title 提示）；
//! 2. **网关 config 官方列表回退**（`official_models.claude`，缺省
//!    sonnet/opus/haiku）：无任何映射（官方直连形态）时菜单项即本列表
//!    （label=官方名，target=null）；
//! 3. **SPA 内置兜底**：网关未下发 models 段（空 options 整段缺席，如
//!    official_models 配空）时 SPA 用自己的内置列表（见 web-ui lib/commands）。
//!
//! codex-app 后端（`agent = "codex-app"`，`~/.codex/config.toml` 与 Claude 的
//! settings 不同源）走独立三链，优先级 **实时目录 → 本机 codex 配置 → 官方
//! 回退**：
//! 1. **实时 `model/list`**（最高优先）：装配处注入 [`ModelsContext::list_models`]
//!    闭包（内部 spawn 临时 app-server，任何失败回空）——非空即整表接管
//!    （label=target=模型 id，SPA 直发精确 id）；
//! 2. **本机 codex 配置**：`~/.codex/config.toml` 的 `model = "<id>"` 作
//!    cliDefault；`model_catalog_json` 指向的 catalog 文件 `models[].model`
//!    数组作 options（label=target=模型 id）；
//! 3. **官方回退**（`official_models.codex`，缺省 gpt-5-codex /
//!    gpt-5.1-codex-mini，target=null 与 claude 层同构）。
//!
//! exec 版 codex（`agent = "codex"`）**行为冻结**：沿用 claude 链（settings
//! 映射 → official_models.claude——旧单列表形态的既有消费者），本批不动。
//!
//! 由网关统一汇聚（claude 链见 [`resolve_models`]、codex 链见
//! [`resolve_codex_models`]，`current` 取 backend.model()——/model 命令热设值）
//! 后随 workspaces 快照下发（platform 泵每次组装现读配置文件——低频快照操作，
//! 不做缓存）。
//!
//! 本模块全部为可单测的纯逻辑（文件读取入参 `*_at` 形态可注入，不锚定真实
//! home——测试不依赖运行机器的 ~/.claude / ~/.codex 状态）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use imagent_core::{Backend, OfficialModels};
use std::sync::Arc;

use crate::{ModelOption, ModelsFrame};

/// codex-app 实时模型目录的 sync 闭包类型（[`ModelsContext::list_models`]）。
/// 调用方（workspace 泵）经 `spawn_blocking` 调——实现侧自行桥接 async
/// （见 main 装配处），任何失败回空 Vec（回退链接管）。
pub type ListModelsFn = dyn Fn() -> Vec<String> + Send + Sync;

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

/// codex 本机配置（`~/.codex/config.toml` + catalog）的模型快照
///（[`read_codex_models`] 产物）。
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CodexModels {
    /// config.toml 顶层 `model = "<id>"`（codex CLI 默认模型；无/空白 → None）。
    pub cli_default: Option<String>,
    /// `model_catalog_json` 指向的 catalog 文件里 `models[].model` 数组
    ///（label=target=模型 id）。文件缺失/解析失败/键缺席 → 空。
    pub catalog: Vec<String>,
}

/// 按 backend 分链的 CLI 配置快照（workspace 泵每次组装时按
/// `Backend::name()` 选边读取——claude 系读 settings.json、codex-app 读
/// codex config.toml；exec codex 行为冻结走 claude 链）。
pub(crate) enum CliSource {
    Claude(CliModels),
    Codex(CodexModels),
}

/// 按 agent 名分链读本机 CLI/codex 配置（ pump 每动作调用；低频可接受）。
pub(crate) fn read_cli_for_backend(agent: &str) -> CliSource {
    if agent == "codex-app" {
        CliSource::Codex(read_codex_models())
    } else {
        CliSource::Claude(read_cli_models())
    }
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

/// 读 codex 本机配置：`<home>/.codex/config.toml`（顶层 `model` 作 cliDefault，
/// `model_catalog_json` 指向的 catalog 提取 options）。**绝不 Err**：文件不存在
/// / 解析失败 / 键缺失 → 全空（模型菜单是展示面，坏配置不值得刷日志打搅——
/// 与 claude 链同纪律）。
pub(crate) fn read_codex_models() -> CodexModels {
    match dirs::home_dir() {
        Some(home) => read_codex_models_at(&home.join(".codex").join("config.toml")),
        None => CodexModels::default(),
    }
}

/// [`read_codex_models`] 的路径注入形态（单测用临时 config.toml，不锚定真实
/// home）。catalog 路径解析（`~` 展开 / 相对路径锚定 config.toml 所在目录）
/// 后读盘提取 `models[].model`。
pub(crate) fn read_codex_models_at(config_path: &Path) -> CodexModels {
    let Ok(raw) = std::fs::read_to_string(config_path) else {
        return CodexModels::default();
    };
    let (cli_default, catalog_path) = parse_codex_config(&raw);
    let catalog = catalog_path
        .and_then(|p| resolve_catalog_path(&p, config_path))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|raw| parse_model_catalog(&raw))
        .unwrap_or_default();
    CodexModels {
        cli_default,
        catalog,
    }
}

/// config.toml 文本 → (model, model_catalog_json 原文)（纯函数）。TOML 解析
///（不手撕行——`model` 键可能合法出现在嵌套表里，顶层键须用解析器定位）；
/// 非 TOML / 键非字符串 / 空白串 → None。
pub(crate) fn parse_codex_config(raw: &str) -> (Option<String>, Option<String>) {
    let Ok(v) = toml::from_str::<toml::Value>(raw) else {
        return (None, None);
    };
    let nonempty = |s: &str| -> Option<String> {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    let model = v.get("model").and_then(|m| m.as_str()).and_then(nonempty);
    let catalog = v
        .get("model_catalog_json")
        .and_then(|m| m.as_str())
        .and_then(nonempty);
    (model, catalog)
}

/// catalog 路径解析：`~/` 前缀按 home 展开；相对路径锚定 config.toml 所在
/// 目录（codex 官方常用绝对路径，相对形态取「相对配置文件」直觉语义）。
fn resolve_catalog_path(raw: &str, config_path: &Path) -> Option<PathBuf> {
    let t = raw.trim();
    if let Some(rest) = t.strip_prefix("~/") {
        return Some(dirs::home_dir()?.join(rest));
    }
    if t == "~" {
        return dirs::home_dir();
    }
    let p = Path::new(t);
    if p.is_absolute() {
        Some(p.to_path_buf())
    } else {
        config_path.parent().map(|d| d.join(p))
    }
}

/// catalog JSON 文本 → 模型 id 列表（纯函数）：顶层 `models` 数组逐项取
/// `model` 字符串（非空）；数组缺失 / 项非对象 / 键缺失 → 跳过该项；整体
/// 非 JSON → 空。形态实证（codex model_catalog）：`{"models":[{"model":
/// "gpt-5-codex", ...}, ...]}`。保持文件序（菜单展示序 = 目录声明序）。
pub(crate) fn parse_model_catalog(raw: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    v.get("models")
        .and_then(|m| m.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|it| {
                    it.get("model")
                        .and_then(|m| m.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 汇聚 claude 链 models 段（纯函数，字节级契约见 lib.rs [`ModelsFrame`]）：
/// - 映射 ≥1 条 → options = 三别名中**有映射的**那些（{label: 别名,
///   target: Some(映射值)}；无映射的别名不出——切过去是无人消费的空别名）；
/// - 无任何映射 → options = official.claude 列表（{label: 官方名, target: None}）；
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

/// 汇聚 codex-app 链 models 段（纯函数；三层优先级：**list_models 实时非空
/// 短路 → codex config catalog → official_models.codex**，全空 → options 空
/// 整段缺席）。`live` 为调用方经 `spawn_blocking` 取回的实时目录（空 = 实时
/// 层失败/未注入，静默落下层——实时层只是加速与新鲜度，不构成可用性依赖）。
///
/// target 语义分两层：live / catalog 是真实模型 id → `target=Some(id)`（SPA
/// 直发精确 id）；official 沿 claude 官方层同构 `target=None`（SPA 发 label
/// 原文——缺省两件本就是完整 id，等价可达）。`cli_default` 恒取 codex
/// config.toml 的 `model`（与 options 来源解耦——实时目录接管时它仍是
/// 「本机默认」的真值）。
pub(crate) fn resolve_codex_models(
    live: &[String],
    codex: &CodexModels,
    official: &[String],
    current: Option<String>,
) -> ModelsFrame {
    let options: Vec<ModelOption> = if !live.is_empty() {
        live.iter()
            .map(|id| ModelOption {
                label: id.clone(),
                target: Some(id.clone()),
            })
            .collect()
    } else if !codex.catalog.is_empty() {
        codex
            .catalog
            .iter()
            .map(|id| ModelOption {
                label: id.clone(),
                target: Some(id.clone()),
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
        cli_default: codex.cli_default.clone(),
        options,
    }
}

/// 模型目录装配上下文（[`crate::WebPlatform`] 构造注入，main 装配处传现有
/// backend clone + config `official_models` + codex-app 实时闭包）：workspace
/// 泵每次组快照时用它读 CLI/codex 配置 + 取 backend.model() 组 models 段。
/// 测试注入 mock backend 与受控 official 列表/闭包（不依赖运行机器的
/// ~/.claude / ~/.codex 状态）。
pub struct ModelsContext {
    /// backend 句柄（models.current 数据源 + name() 分链判据；不执行 run——
    /// 只读 model()）。
    pub backend: Arc<dyn Backend>,
    /// config 官方回退列表（per-backend：claude 系消费 claude、codex-app
    /// 消费 codex）。
    pub official: OfficialModels,
    /// codex-app 实时模型目录闭包（三链第①层；仅 codex-app 装配注入——
    /// None / 空 Vec / panic 均回退链接管）。claude 链无此层。
    pub list_models: Option<Arc<ListModelsFn>>,
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

    // -- codex-app 链（codex config.toml + catalog + 三层回退）----------------

    /// codex config.toml 解析：真实形态 fixture（顶层 model /
    /// model_catalog_json + 无关键与嵌套表共存，脱敏）；坏 TOML / 键缺失 /
    /// 空白值容错。
    #[test]
    fn parse_codex_config_example() {
        // 真实 config.toml 形态（键名实证 ~/.codex/config.toml；值脱敏）：
        let raw = r#"
model = "gpt-5.1-codex-mini"
model_catalog_json = "/Users/me/.codex/models_catalog.json"
approval_policy = "on-request"
sandbox_mode = "workspace-write"

[model_providers.ccswitch]
name = "ccswitch"
base_url = "https://example.invalid/api"
"#;
        let (model, catalog) = parse_codex_config(raw);
        assert_eq!(model.as_deref(), Some("gpt-5.1-codex-mini"));
        assert_eq!(
            catalog.as_deref(),
            Some("/Users/me/.codex/models_catalog.json")
        );

        // 坏 TOML → 全空；缺键 → None；空白串值视为未设。
        assert_eq!(parse_codex_config("not toml {{{"), (None, None));
        assert_eq!(
            parse_codex_config("sandbox_mode = \"read-only\"\n"),
            (None, None)
        );
        assert_eq!(
            parse_codex_config("model = \"  \"\nmodel_catalog_json = \"\"\n"),
            (None, None),
            "空白/空串值视为未设"
        );
        // trim 生效。
        let (model, _) = parse_codex_config("model = \" gpt-5-codex \"\n");
        assert_eq!(model.as_deref(), Some("gpt-5-codex"));
    }

    /// catalog JSON 解析：codex model_catalog 真实形态（models[].model +
    /// 伴生元数据键共存）；数组缺失 / 项缺 model / 非字符串 / 整体坏 JSON
    /// 容错；保持文件序。
    #[test]
    fn parse_model_catalog_example() {
        let raw = r#"{
            "models": [
                {"model": "gpt-5-codex", "display_name": "GPT-5 Codex", "context_window": 272000},
                {"model": "gpt-5.1-codex-mini", "display_name": "GPT-5.1 Codex Mini"},
                {"display_name": "无 model 键，跳过"},
                {"model": 42, "note": "非字符串，跳过"},
                {"model": "  ", "note": "空白，跳过"}
            ],
            "version": 1
        }"#;
        assert_eq!(
            parse_model_catalog(raw),
            vec!["gpt-5-codex".to_string(), "gpt-5.1-codex-mini".to_string()],
            "保持文件序；非法项跳过不中断"
        );
        assert!(
            parse_model_catalog("not json !!!").is_empty(),
            "坏 JSON → 空"
        );
        assert!(
            parse_model_catalog(r#"{"version": 1}"#).is_empty(),
            "缺 models 数组 → 空"
        );
        assert!(
            parse_model_catalog(r#"{"models": "not-an-array"}"#).is_empty(),
            "models 非数组 → 空"
        );
    }

    /// read_codex_models_at：临时目录 fixture（config.toml + catalog.json 真
    /// 实两文件形态）；缺文件/坏 catalog 路径容错 → 全空/缺 catalog。
    #[test]
    fn read_codex_models_at_fixture() {
        let dir = std::env::temp_dir().join(format!(
            "imagent-web-codex-models-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("catalog.json"),
            r#"{"models": [{"model": "gpt-5-codex"}, {"model": "gpt-5.1"}]}"#,
        )
        .unwrap();
        // 绝对路径 catalog。
        let cfg_abs = dir.join("config-abs.toml");
        std::fs::write(
            &cfg_abs,
            format!(
                "model = \"gpt-5-codex\"\nmodel_catalog_json = \"{}/catalog.json\"\n",
                dir.display()
            ),
        )
        .unwrap();
        let m = read_codex_models_at(&cfg_abs);
        assert_eq!(m.cli_default.as_deref(), Some("gpt-5-codex"));
        assert_eq!(
            m.catalog,
            vec!["gpt-5-codex".to_string(), "gpt-5.1".to_string()]
        );

        // 相对路径 catalog：锚定 config.toml 所在目录。
        let cfg_rel = dir.join("config-rel.toml");
        std::fs::write(&cfg_rel, "model_catalog_json = \"catalog.json\"\n").unwrap();
        let m = read_codex_models_at(&cfg_rel);
        assert_eq!(m.cli_default, None);
        assert_eq!(m.catalog.len(), 2, "相对 catalog 按 config 目录解析");

        // catalog 指向不存在文件 → cliDefault 保留、catalog 空（落官方层）。
        let cfg_missing = dir.join("config-missing-catalog.toml");
        std::fs::write(
            &cfg_missing,
            "model = \"gpt-5-codex\"\nmodel_catalog_json = \"nope.json\"\n",
        )
        .unwrap();
        let m = read_codex_models_at(&cfg_missing);
        assert_eq!(m.cli_default.as_deref(), Some("gpt-5-codex"));
        assert!(m.catalog.is_empty());

        // config.toml 缺失 → 全空，不 panic 不 Err。
        assert_eq!(
            read_codex_models_at(&dir.join("absent.toml")),
            CodexModels::default()
        );
        // 坏 TOML → 全空。
        let cfg_bad = dir.join("config-bad.toml");
        std::fs::write(&cfg_bad, "{{{ not toml").unwrap();
        assert_eq!(read_codex_models_at(&cfg_bad), CodexModels::default());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// codex 三层链优先级（任务验收项）：list_models 实时非空短路；空走
    /// catalog；再空 official；全空 options 空（整段缺席）。target 语义：
    /// live/catalog = Some(id)（SPA 直发精确 id）、official = None。
    #[test]
    fn resolve_codex_three_layer_priority() {
        let codex = CodexModels {
            cli_default: Some("gpt-5-codex".into()),
            catalog: vec!["gpt-5-codex".into(), "gpt-5.1-codex-mini".into()],
        };
        let official = vec!["gpt-5-codex".into(), "gpt-5.1-codex-mini".into()];
        let opts = |f: &ModelsFrame| -> Vec<(String, Option<String>)> {
            f.options
                .iter()
                .map(|o| (o.label.clone(), o.target.clone()))
                .collect()
        };

        // 第①层：实时非空短路（catalog 不出）；cliDefault 仍取 codex config。
        let live = vec!["glm-5.1".to_string(), "gpt-5.3".to_string()];
        let f = resolve_codex_models(&live, &codex, &official, Some("glm-5.1".into()));
        assert_eq!(f.current.as_deref(), Some("glm-5.1"));
        assert_eq!(f.cli_default.as_deref(), Some("gpt-5-codex"));
        assert_eq!(
            opts(&f),
            vec![
                ("glm-5.1".to_string(), Some("glm-5.1".to_string())),
                ("gpt-5.3".to_string(), Some("gpt-5.3".to_string())),
            ],
            "实时层 label=target=id"
        );

        // 第②层：实时空 → catalog 接管。
        let f = resolve_codex_models(&[], &codex, &official, None);
        assert_eq!(
            opts(&f),
            vec![
                ("gpt-5-codex".to_string(), Some("gpt-5-codex".to_string())),
                (
                    "gpt-5.1-codex-mini".to_string(),
                    Some("gpt-5.1-codex-mini".to_string())
                ),
            ],
            "catalog 层 label=target=id"
        );

        // 第③层：实时空 + catalog 空 → official 接管（target=None，与 claude
        // 官方层同构）。
        let codex_no_catalog = CodexModels {
            cli_default: None,
            catalog: vec![],
        };
        let f = resolve_codex_models(&[], &codex_no_catalog, &official, None);
        assert_eq!(
            opts(&f),
            vec![
                ("gpt-5-codex".to_string(), None),
                ("gpt-5.1-codex-mini".to_string(), None),
            ],
            "官方层 target 恒 None"
        );

        // 全空 → options 空（整段缺席，SPA 内置兜底）。
        let f = resolve_codex_models(&[], &codex_no_catalog, &[], None);
        assert!(f.options.is_empty());
        assert_eq!(f.cli_default, None);
    }
}
