//! 工作空间（Workspace）KV 键名与值格式（Web 协议 v1.6）。
//!
//! 「工作目录」升级为一等公民「工作空间」后，终端 `/ws` 命令与网页驾驶舱
//! （[`imagent_web::WebPlatform`]）**共用同一张 store KV 表**（终端保存的
//! 空间网页可见、反之亦然）。本模块是该表的唯一键名/值格式事实源，两侧
//! （core `cmd_ws` / web 平台泵）都从这里取读写辅助，杜绝格式漂移。
//!
//! ## 键布局（既有约定，v1.6 起不变）
//!
//! - `workspace:<name>`：命名工作空间（全局共享）。v1.6 前值为**裸路径字符串**；
//!   v1.6 起升级为 JSON `{"path":"…","lastUsedAt":<unix秒>}`（`lastUsedAt`
//!   可缺省——保存时未知、`conv_workdir_set` 命中同路径时 bump）。**读必须
//!   兼容旧值**（存量部署零迁移，裸路径 → `lastUsedAt` 缺席）。
//! - `workdir:<convId>`：per-conv 工作目录覆盖（`/cd` 与网页 `conv_workdir_set`
//!   同键——这是终端与网页互通的关键），值恒为裸路径字符串（无格式升级）。

use serde::{Deserialize, Serialize};

/// 命名工作空间键前缀（`/ws` 命令与 web 平台共用）。
pub const WORKSPACE_PREFIX: &str = "workspace:";
/// per-conv 工作目录覆盖键前缀（`/cd` 与 web `conv_workdir_set` 共用）。
pub const WORKDIR_PREFIX: &str = "workdir:";

/// 命名工作空间的 config 键：`workspace:<name>`（全局别名，所有 conv 共享）。
pub fn workspace_key(name: &str) -> String {
    format!("{WORKSPACE_PREFIX}{name}")
}

/// per-conv 工作目录的 config 键：`workdir:<conv_id>`（由 `/cd` 与网页
/// `conv_workdir_set` 设置，覆盖 default_workdir）。
pub fn workdir_key(conv_id: &str) -> String {
    format!("{WORKDIR_PREFIX}{conv_id}")
}

/// `workspace:<name>` 的值记录（v1.6 JSON 格式）：路径 + 最近使用时间
/// （unix 秒；保存时缺省，`conv_workdir_set` 命中同路径时 bump——SPA 据此
/// 排「最近用过」）。字段名 `lastUsedAt` 与帧契约逐字一致。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub path: String,
    #[serde(
        rename = "lastUsedAt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub last_used_at: Option<u64>,
}

impl WorkspaceRecord {
    /// 新建无使用时间戳的记录（`/ws save` / 网页 `workspace_save` 落库用）。
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            last_used_at: None,
        }
    }

    /// 编码为 KV 值（JSON）。纯 String/整数字段序列化不可失败。
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("WorkspaceRecord 序列化纯标量字段，不可失败")
    }

    /// 解码 KV 值：JSON（v1.6 新格式）→ 记录；裸路径字符串（v1.6 前旧值）→
    /// `lastUsedAt` 缺席的记录；空串 / 畸形 JSON 对象 → None（值损坏，调用方
    /// 自行决定跳过或回退展示原值）。裸路径必以 `/` 开头（绝对路径），与
    /// JSON 的 `{` 开头天然无歧义。
    pub fn decode(raw: &str) -> Option<Self> {
        let t = raw.trim();
        if t.is_empty() {
            return None;
        }
        if t.starts_with('{') {
            return serde_json::from_str(t).ok();
        }
        Some(Self {
            path: raw.to_string(),
            last_used_at: None,
        })
    }
}

#[cfg(test)]
mod tests {
    //! 值格式兼容性（任务说明「KV 兼容」项）：旧格式（裸路径）读出 lastUsedAt
    //! 缺席；新格式读写往返；畸形值 None。

    use super::*;

    #[test]
    fn encode_roundtrip_new_format() {
        let rec = WorkspaceRecord {
            path: "/Users/x/proj".into(),
            last_used_at: Some(1_770_000_000),
        };
        let raw = rec.encode();
        assert_eq!(
            raw, r#"{"path":"/Users/x/proj","lastUsedAt":1770000000}"#,
            "字段名与帧契约逐字一致"
        );
        assert_eq!(WorkspaceRecord::decode(&raw), Some(rec));
    }

    #[test]
    fn encode_omits_absent_last_used_at() {
        let raw = WorkspaceRecord::new("/Users/x/proj").encode();
        assert_eq!(raw, r#"{"path":"/Users/x/proj"}"#);
        let rec = WorkspaceRecord::decode(&raw).unwrap();
        assert_eq!(rec.path, "/Users/x/proj");
        assert_eq!(rec.last_used_at, None, "缺省 lastUsedAt 读出为 None");
    }

    /// 存量部署（v1.6 前裸路径值）零迁移直读：lastUsedAt 缺席。
    #[test]
    fn decode_legacy_bare_path() {
        let rec = WorkspaceRecord::decode("/Users/x/legacy-proj").unwrap();
        assert_eq!(rec.path, "/Users/x/legacy-proj");
        assert_eq!(rec.last_used_at, None);
    }

    #[test]
    fn decode_malformed() {
        assert!(WorkspaceRecord::decode("").is_none());
        assert!(WorkspaceRecord::decode("   ").is_none());
        // JSON 对象但缺 path / 畸形 JSON → None（值损坏）。
        assert!(WorkspaceRecord::decode(r#"{"lastUsedAt":1}"#).is_none());
        assert!(WorkspaceRecord::decode(r#"{"path":"#).is_none());
        // JSON 数组/标量不是合法值形态（非 `{` 开头按裸路径兜底无意义但保持
        // 确定：数字串会被当裸路径——真实部署不会出现，不为此加启发式）。
    }

    /// 键名布局钉住（终端 /ws 与网页共表的互通契约）。
    #[test]
    fn key_layout() {
        assert_eq!(workspace_key("main"), "workspace:main");
        assert_eq!(workdir_key("web:work"), "workdir:web:work");
        assert_eq!(workdir_key("local:work"), "workdir:local:work");
    }
}
