//! 平台层测试公共底座（cfg(test)）：本地回环 mock 飞书 OpenAPI + 指向 mock 的
//! CoreConfig/预填 token/platform 构造（T19 拆分自 platform.rs 的 tests 模块，
//! 供 platform 与 drain 两个测试模块共用——逻辑零改动）。
//! T12：`pub(crate)` 开放给 crate::bitable 的数据面测试共用；新增带请求体捕获
//! 的 [`spawn_mock_feishu_req`]（append_row 一类 POST 需要 body 断言）。

#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use open_lark::CoreConfig;

use super::FeishuPlatform;

/// loopback mock 的代理豁免（一次性）：常见代理环境（`http_proxy=
/// http://127.0.0.1:7897` 一类）会把发往 127.0.0.1 的请求劫去代理回 502。
/// reqwest 在**首次构建** client 时读环境——须在此之前把 loopback 并进
/// NO_PROXY（保留用户原值）。api_client 是全局 OnceLock 且测试内无其它
/// 触网路径（发送类函数不被任何测试引用），无先后竞态。
pub(crate) fn ensure_no_proxy_for_loopback() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let merged = match std::env::var("NO_PROXY") {
            Ok(prev) if !prev.trim().is_empty() => format!("{prev},127.0.0.1,localhost"),
            _ => "127.0.0.1,localhost".to_string(),
        };
        std::env::set_var("NO_PROXY", merged);
    });
}

/// mock 的按 path 分发闭包类型（type_complexity 收敛）。
type MockRespond = std::sync::Arc<dyn Fn(&str) -> (u16, String) + Send + Sync>;

/// T12：带请求体捕获的分发闭包（(path?query, body)——POST body 断言用）。
pub(crate) type MockRespondReq = std::sync::Arc<dyn Fn(&str, &str) -> (u16, String) + Send + Sync>;

/// 本地回环 mock 飞书 OpenAPI：按请求 path 分发 `(status, JSON body)`。
/// 群上下文/引用上下文的拉取都走 `core_config.base_url()` 直拼 URL——指向
/// 本 server 即可离线验收完整注入管线（真实 reqwest HTTP 栈，假后端）。
/// path 已剥 query（既有测试按裸 path 精确分发；带 query/body 的断言用
/// [`spawn_mock_feishu_req`]）。
pub(crate) async fn spawn_mock_feishu(respond: MockRespond) -> String {
    let respond: MockRespondReq = Arc::new(move |path, _body| {
        let bare = path.split('?').next().unwrap_or("/");
        respond(bare)
    });
    spawn_mock_feishu_req(respond).await
}

/// [`spawn_mock_feishu`] 的 (path?query, body) 版：path 保留 query 串（分页
/// 参数断言用），body 为请求原文（POST 载荷断言用）。
pub(crate) async fn spawn_mock_feishu_req(respond: MockRespondReq) -> String {
    ensure_no_proxy_for_loopback();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let respond = respond.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 16 * 1024];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split(' ').nth(1).unwrap_or("/").to_string();
                let body = req
                    .split_once("\r\n\r\n")
                    .map(|(_, b)| b.to_string())
                    .unwrap_or_default();
                let (status, body) = respond(&path, &body);
                let resp = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://127.0.0.1:{}", addr.port())
}

/// 群上下文测试底座：指向 mock 的 CoreConfig（app_id 即 bot 判定入参）。
pub(crate) fn mock_core_config(base_url: &str) -> Arc<CoreConfig> {
    Arc::new(
        CoreConfig::builder()
            .app_id("cli_mock".to_string())
            .app_secret("sec_mock".to_string())
            .base_url(base_url.to_string())
            .req_timeout(Duration::from_secs(5))
            .build(),
    )
}

/// 预填缓存 token 的 lock（TTL 内命中，不打真 token 端点）。
pub(crate) fn cached_token() -> Arc<RwLock<Option<(String, Instant)>>> {
    Arc::new(RwLock::new(Some(("t_mock".to_string(), Instant::now()))))
}

/// 构造指向 mock 的 platform（token 预填缓存，不打真 token 端点）。
pub(crate) async fn mk_platform_with_mock(base: &str) -> FeishuPlatform {
    let p = FeishuPlatform::new(
        "cli_test".into(),
        "secret_test".into(),
        base.to_string(),
        true,
        None,
        300,
        None,
        1800,
        true,
        None,
        0.0,
        false,
        0,
    )
    .expect("构造");
    *p.token.write().await = Some(("t_mock".to_string(), Instant::now()));
    p
}
