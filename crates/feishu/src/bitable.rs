//! T12（v13 产品批 #3）：[`FeishuBitable`]——core [`BitableApi`] 的飞书实现。
//!
//! 结构化产出（任务清单、巡检结果、成本台账）写进飞书多维表格：main 在
//! `platform=feishu` 且 config `feishu_bitable_app_token`/`feishu_bitable_table_id`
//! 齐备时构造并注入 Dispatcher（socket `kind=bitable` 请求路由到本实现）。
//! HTTP 走 [`crate::client`]（429 退避 + 信封解析），token 与 platform 共用
//! 同一套 lazy 刷新缓存（[`crate::platform::fetch_cached_token`]——本实现自带
//! 一份独立缓存句柄，与 FeishuPlatform 生命周期解耦，SIGHUP 可独立重建）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::RwLock;

use open_lark::CoreConfig;

use imagent_core::{BitableApi, BitableField, Result};

use crate::client::{append_bitable_record, list_bitable_fields};
use crate::platform::fetch_cached_token;

/// 飞书 Bitable 数据面实现：包既有 HTTP client（两个操作 + token lazy 刷新）。
///
/// 构造零 IO（token 首次调用时才取）；`Send + Sync`（Arc 共享进 Dispatcher）。
pub struct FeishuBitable {
    core_config: Arc<CoreConfig>,
    app_id: String,
    app_secret: String,
    /// token 缓存（(token, fetched_at)，TTL 语义与 platform 相同）。
    token: Arc<RwLock<Option<(String, Instant)>>>,
    app_token: String,
    table_id: String,
}

impl FeishuBitable {
    /// 构造（凭据与平台同源：`feishu_app_id` + `IMAGENT_FEISHU_APP_SECRET` env
    /// + `feishu_base_url`；app_token/table_id 来自 bitable 配置对）。
    pub fn new(
        app_id: String,
        app_secret: String,
        base_url: String,
        app_token: String,
        table_id: String,
    ) -> Self {
        let core_config = Arc::new(
            CoreConfig::builder()
                .app_id(app_id.clone())
                .app_secret(app_secret.clone())
                .base_url(base_url)
                // 与 FeishuPlatform::new 的 CoreConfig 同款超时（连接黑洞防挂起）。
                .req_timeout(Duration::from_secs(30))
                .build(),
        );
        Self {
            core_config,
            app_id,
            app_secret,
            token: Arc::new(RwLock::new(None)),
            app_token,
            table_id,
        }
    }
}

#[async_trait]
impl BitableApi for FeishuBitable {
    async fn list_fields(&self) -> Result<Vec<BitableField>> {
        let token = fetch_cached_token(
            &self.token,
            &self.core_config,
            &self.app_id,
            &self.app_secret,
        )
        .await?;
        list_bitable_fields(&self.core_config, &token, &self.app_token, &self.table_id).await
    }

    async fn append_row(
        &self,
        fields: serde_json::Map<String, serde_json::Value>,
    ) -> Result<String> {
        let token = fetch_cached_token(
            &self.token,
            &self.core_config,
            &self.app_id,
            &self.app_secret,
        )
        .await?;
        append_bitable_record(
            &self.core_config,
            &token,
            &self.app_token,
            &self.table_id,
            &fields,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::testutil::spawn_mock_feishu_req;
    use serde_json::json;

    /// 构造指向 mock 的实例并预填 token（同模块可触私有字段，免打 token 端点）。
    async fn mk_bitable(base: &str) -> FeishuBitable {
        let bt = FeishuBitable::new(
            "cli_test".into(),
            "secret_test".into(),
            base.to_string(),
            "appT_token".into(),
            "tbl_test".into(),
        );
        *bt.token.write().await = Some(("t_mock".to_string(), Instant::now()));
        bt
    }

    /// list_fields：GET 路径含 app_token/table_id、query 带 page_size=50；
    /// 响应 items 解析为 name/type（ui_type 优先、type 整数回退）；has_more
    /// 只取首页（debug 留痕）不报错。
    #[tokio::test]
    async fn list_fields_hits_bitable_path_and_parses() {
        let body = json!({
            "code": 0,
            "data": { "has_more": true, "items": [
                { "field_name": "任务", "ui_type": "Text", "type": 1 },
                { "field_name": "完成时间", "ui_type": "DateTime", "type": 5 },
                { "field_name": "旧形态列", "type": 2 }
            ]}
        })
        .to_string();
        let body = std::sync::Arc::new(body);
        let base = spawn_mock_feishu_req(std::sync::Arc::new(move |path, _body| {
            assert!(
                path.starts_with("/open-apis/bitable/v1/apps/appT_token/tables/tbl_test/fields"),
                "路径应含 app_token/table_id: {path}"
            );
            assert!(path.contains("page_size=50"), "单页 50: {path}");
            (200u16, body.to_string())
        }))
        .await;
        let bt = mk_bitable(&base).await;
        let fields = bt.list_fields().await.expect("应成功");
        assert_eq!(fields.len(), 3, "has_more 时取首页全量: {fields:?}");
        assert_eq!(fields[0].name, "任务");
        assert_eq!(fields[0].field_type, "Text");
        assert_eq!(fields[1].field_type, "DateTime");
        assert_eq!(fields[2].field_type, "2", "无 ui_type 回退 type 整数串");
    }

    /// append_row：POST 路径正确、body 为 `{"fields": {...}}` 原样承载列值；
    /// 成功解析 record/record_id。
    #[tokio::test]
    async fn append_row_posts_fields_and_returns_record_id() {
        let base = spawn_mock_feishu_req(std::sync::Arc::new(|path, body| {
            assert_eq!(
                path,
                "/open-apis/bitable/v1/apps/appT_token/tables/tbl_test/records"
            );
            let v: serde_json::Value = serde_json::from_str(body).expect("body 应为 JSON");
            assert_eq!(v["fields"]["任务"], "巡检");
            assert_eq!(v["fields"]["耗时分钟"], 42);
            (
                200u16,
                r#"{"code":0,"data":{"record":{"record_id":"recXYZ"}}}"#.to_string(),
            )
        }))
        .await;
        let bt = mk_bitable(&base).await;
        let mut fields = serde_json::Map::new();
        fields.insert("任务".into(), json!("巡检"));
        fields.insert("耗时分钟".into(), json!(42));
        let rid = bt.append_row(fields).await.expect("应成功");
        assert_eq!(rid, "recXYZ");
    }

    /// append_row 失败：飞书业务错误（字段名不匹配类）message 透传——agent 可
    /// 按原文修列名（高频错误，刻意不吞）。
    #[tokio::test]
    async fn append_row_error_message_passthrough() {
        let base = spawn_mock_feishu_req(std::sync::Arc::new(|_p, _b| {
            (
                200u16,
                r#"{"code":1254045,"msg":"FieldNameNotFound: 字段 XXX 不存在"}"#.to_string(),
            )
        }))
        .await;
        let bt = mk_bitable(&base).await;
        let mut fields = serde_json::Map::new();
        fields.insert("不存在的列".into(), json!("x"));
        let err = bt.append_row(fields).await.expect_err("业务码非 0 应报错");
        let msg = err.to_string();
        assert!(
            msg.contains("FieldNameNotFound"),
            "飞书 message 透传: {msg}"
        );
        assert!(msg.contains("bitable_append_row"), "op 前缀可定位: {msg}");
    }
}
