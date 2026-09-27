//! 飞书多维表格（Bitable）数据面抽象（T12，v13 产品方向 #3）。
//!
//! 结构化产出（任务清单、巡检结果、成本台账）此前只能刷屏卡片或落文件；
//! Bitable 是飞书生态的结构化数据面，与 cron/巡检场景天然咬合。链路：
//! agent（claude-cli）经 `imagent mcp` stdio server 的
//! `bitable_list_fields` / `bitable_append_row` 工具 → 既有 permission unix
//! socket（新消息 `kind=bitable`）→ Dispatcher → 本 trait → feishu 实现
//! （[`crate::config::Config::feishu_bitable_app_token`] +
//! `feishu_bitable_table_id` 齐备才注入，见 `Config::bitable_enabled_for`）。
//!
//! core 只定契约不实现 HTTP（依赖倒置，与 `Platform` / `Backend` 同构）；
//! 错误复用 [`crate::error::CoreError`]（thiserror 派生），feishu 侧透传
//! 飞书 API 的错误 message（字段名不匹配是高频错误，agent 需要原文修列名）。

use crate::error::Result;
use serde_json::Value;

/// socket 路由侧「未注入 BitableApi」的回错文案（配置引导，钉住供测试断言）。
pub const NOT_CONFIGURED_MSG: &str = "bitable 未启用：未配置 feishu_bitable_app_token 与 \
     feishu_bitable_table_id（需两者齐备且 platform = \"feishu\" 才启用，详见 config.toml \
     注释；改动经 SIGHUP 或重启生效）";

/// 多维表格字段摘要（`list_fields` 返回条目）：列名 + 类型。
/// `field_type` 是可读类型串（飞书 `ui_type`，如 `Text` / `DateTime`），
/// 序列化键为 `type`（agent 视角的列类型）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BitableField {
    /// 列名（写行时 `fields` 的键）。
    pub name: String,
    /// 可读类型串（如 `Text`）。
    #[serde(rename = "type")]
    pub field_type: String,
}

/// Bitable 数据面契约：列出当前表字段 / 追加一行记录。
///
/// 实现方（feishu）持有 app_token + table_id 与 token 缓存，两个操作都是
/// 幂等只读/追加语义——没有更新/删除（MVP 刻意收敛写面：agent 只能新增行，
/// 不能改表结构与既有数据）。
#[async_trait::async_trait]
pub trait BitableApi: Send + Sync {
    /// 列出当前表的全部字段（列名 + 类型）。agent 写行前先调它对齐列名。
    async fn list_fields(&self) -> Result<Vec<BitableField>>;

    /// 追加一行记录（`fields` 键=列名、值=标量），返回新记录 `record_id`。
    async fn append_row(&self, fields: serde_json::Map<String, Value>) -> Result<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 序列化形态：`{"name": ..., "type": ...}`——agent 读到的列类型键是 `type`
    /// （`field_type` 是 Rust 侧命名，避免与 JSON 惯例漂移）。
    #[test]
    fn bitable_field_serializes_with_type_key() {
        let f = BitableField {
            name: "任务".into(),
            field_type: "Text".into(),
        };
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["name"], "任务");
        assert_eq!(v["type"], "Text");
        assert!(v.get("field_type").is_none(), "Rust 侧命名不进 JSON");
        // 反序列化 roundtrip（socket data 透传链路无二次解析，钉住形态即可）。
        let back: BitableField = serde_json::from_value(v).unwrap();
        assert_eq!(back, f);
    }

    /// 未配置文案钉住：socket 路由 None 分支的回错必须指向配置键（用户可按图
    /// 索骥），且提及 SIGHUP 生效路径。
    #[test]
    fn not_configured_msg_names_config_keys() {
        assert!(NOT_CONFIGURED_MSG.contains("feishu_bitable_app_token"));
        assert!(NOT_CONFIGURED_MSG.contains("feishu_bitable_table_id"));
        assert!(NOT_CONFIGURED_MSG.contains("feishu"));
    }
}
