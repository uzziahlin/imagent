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

/// 鉴权后的运行时 HTTP 客户端（收/发消息用）。
///
/// 登录前的请求（取二维码/轮询状态）无 `bot_token`，由 `login.rs` 自带
/// 未鉴权 POST 处理，不经过本结构。
#[derive(Debug, Clone)]
pub struct ILinkClient {
    http: reqwest::Client,
    base_url: String,
    bot_token: String,
    #[allow(dead_code)]
    ilink_bot_id: String,
    #[allow(dead_code)]
    ilink_user_id: String,
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
        let v: u32 = rand::thread_rng().gen();
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
            // session 失效等多以 401/403 体现：在错误信息里保留状态码，
            // 便于 platform 层做 SESSION_EXPIRED 判定。
            return Err(CoreError::Platform(
                "ilink",
                format!("POST {endpoint}: HTTP {status}"),
            ));
        }

        resp.json::<T>()
            .await
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
}
