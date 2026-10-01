//! iLink 协议 HTTP 客户端：组装请求头 + POST JSON。
//!
//! **请求头**（每请求）：`AuthorizationType: ilink_bot_token` +
//! `Authorization: Bearer <bot_token>` + 随机 `X-WECHAT-UIN`（base64 随机
//! u32 字节，防重放）。详见 DESIGN §6 / RESEARCH §1.2。

use base64::Engine;
use rand::Rng;
use serde::de::DeserializeOwned;

use imagent_core::{CoreError, Result};

pub(crate) const DEFAULT_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
/// post_json 响应体字节上限（防异常/恶意超大响应 OOM）。iLink 响应均为小 JSON，16 MiB 足够余量。
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// 鉴权后的运行时 HTTP 客户端（收/发消息用）。
///
/// 登录前的请求（取二维码/轮询状态）无 `bot_token`，由 `login.rs` 自带
/// 未鉴权 POST 处理，不经过本结构。
#[derive(Clone)]
pub struct ILinkClient {
    http: reqwest::Client,
    base_url: String,
    bot_token: String,
    #[allow(dead_code)]
    ilink_bot_id: String,
    #[allow(dead_code)]
    ilink_user_id: String,
}

// 🟡 Debug redacting：bot_token 是凭据，避免 `debug!(?client)` / `{:?}` 打印时落日志。
impl std::fmt::Debug for ILinkClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ILinkClient")
            .field("base_url", &self.base_url)
            .field("bot_token", &"<redacted>")
            .field("ilink_bot_id", &self.ilink_bot_id)
            .finish()
    }
}

impl ILinkClient {
    pub fn new(
        base_url: Option<String>,
        bot_token: String,
        ilink_bot_id: String,
        ilink_user_id: String,
    ) -> Result<Self> {
        // timeout ~45s，容纳 getupdates 长轮询（~35–40s）。
        // 禁用重定向：媒体 CDN 下载初始 URL 已校验白名单，跟随重定向可被引导到
        // 内网/元数据地址（SSRF 绕过）；iLink API 端点正常不重定向，禁之无副作用。
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(45))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| CoreError::Platform("ilink", format!("build http client: {e}")))?;
        Ok(Self {
            http,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            bot_token,
            ilink_bot_id,
            ilink_user_id,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 共享的 HTTP 客户端（媒体 CDN 下载/上传复用）。
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// 每请求随机 `X-WECHAT-UIN`（base64 随机 u32 小端字节），防重放。
    fn random_uin() -> String {
        let v: u32 = rand::rng().random();
        base64::engine::general_purpose::STANDARD.encode(v.to_le_bytes())
    }

    /// 组装鉴权头 + POST JSON，反序列化为 `T`。错误统一转 `CoreError::Platform`。
    pub(crate) async fn post_json<T: DeserializeOwned>(
        &self,
        endpoint: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        let url = format!("{}{}", self.base_url, endpoint);
        let uin = Self::random_uin();
        let resp = self
            .http
            .post(&url)
            .header("AuthorizationType", "ilink_bot_token")
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .header("X-WECHAT-UIN", uin)
            .json(body)
            .send()
            .await
            .map_err(|e| CoreError::Platform("ilink", format!("POST {endpoint}: {e}")))?;

        let status = resp.status();
        if status.is_server_error() || status.is_client_error() {
            // P3-a（code-review v14）：401/403 = 鉴权失效（bot_token 过期/吊销），
            // 直接返回 typed `CoreError::SessionExpired`——调用方用 `matches!`
            // 判定，不再靠 Display 字符串「HTTP 401/403」匹配（文案一改即静默
            // 失配）。其余 4xx/5xx（404 端点漂移、500 抖动重试等）仍为 Platform
            // 错误。
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(CoreError::SessionExpired(format!(
                    "POST {endpoint}: HTTP {status}"
                )));
            }
            return Err(CoreError::Platform(
                "ilink",
                format!("POST {endpoint}: HTTP {status}"),
            ));
        }

        if let Some(len) = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
        {
            if len > MAX_RESPONSE_BYTES {
                return Err(CoreError::Platform(
                    "ilink",
                    format!("POST {endpoint}: 响应过大 ({len} bytes > {MAX_RESPONSE_BYTES} 上限)"),
                ));
            }
        }
        let bytes = resp.bytes().await.map_err(|e| {
            CoreError::Platform("ilink", format!("POST {endpoint}: read body: {e}"))
        })?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(CoreError::Platform(
                "ilink",
                format!(
                    "POST {endpoint}: 响应体 {} bytes 超过 {MAX_RESPONSE_BYTES} 上限",
                    bytes.len()
                ),
            ));
        }
        serde_json::from_slice::<T>(&bytes)
            .map_err(|e| CoreError::Platform("ilink", format!("POST {endpoint}: decode: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uin_is_base64_of_four_bytes() {
        let s = ILinkClient::random_uin();
        // base64(u32 小端 4 字节) → 恰好 4 字符无填充（4 字节 → ceil(4/3)*4=8? 修正：4 字节 base64 = 8 字符含 1 个 =）
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&s)
            .unwrap();
        assert_eq!(decoded.len(), 4, "u32 little-endian = 4 bytes");
    }

    #[test]
    fn client_builds_with_default_base() {
        let c = ILinkClient::new(None, "tok".into(), "bot".into(), "user".into()).unwrap();
        assert_eq!(c.base_url(), DEFAULT_BASE_URL);
    }

    /// P3-a（code-review v14）：HTTP 401/403 必须映射为 typed `SessionExpired`、
    /// 其余 4xx/5xx 保持 `Platform`——platform 层已改为 `matches!` 判定，若此处
    /// 退回字符串形态，session 失效将漏判（表现为无限退避重试而非提示重登录）。
    /// 用一次性 TCP listener 回原始 HTTP 响应，不依赖外部 mock server crate。
    #[tokio::test]
    async fn post_json_maps_401_403_to_typed_session_expired() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // 开发机常配置 http_proxy/all_proxy 等代理环境变量——reqwest 默认遵循，
        // 会把发往 127.0.0.1 的请求交给代理（表现为 502），故显式豁免 loopback。
        // 本测试是 ilink 测试二进制中唯一读代理环境的用例，进程内无并发竞争。
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");

        async fn serve_once(listener: tokio::net::TcpListener, status_line: &'static str) {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            // 读完请求头（丢弃 body）即回响应。
            let _ = sock.read(&mut buf).await;
            let resp =
                format!("HTTP/1.1 {status_line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            sock.write_all(resp.as_bytes()).await.unwrap();
        }

        for (status_line, expect_expired) in [
            ("401 Unauthorized", true),
            ("403 Forbidden", true),
            ("500 Internal Server Error", false),
            ("404 Not Found", false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(serve_once(listener, status_line));
            let client = ILinkClient::new(
                Some(format!("http://{addr}")),
                "tok".into(),
                "b".into(),
                "u".into(),
            )
            .unwrap();
            let err = client
                .post_json::<serde_json::Value>("/x", &serde_json::json!({}))
                .await
                .unwrap_err();
            let is_expired = matches!(err, CoreError::SessionExpired(_));
            assert_eq!(
                is_expired, expect_expired,
                "{status_line} 判定错误：{err:?}"
            );
            server.await.unwrap();
        }
    }
}
