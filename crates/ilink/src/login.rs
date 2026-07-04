//! 扫码登录流程。
//!
//! 1. `get_bot_qrcode` 取二维码内容 → 终端渲染（`println!`）。
//! 2. 循环 `get_qrcode_status`（~2s 间隔）：`wait/scaned` 继续；`expired` 报错；
//!    `confirmed` 取 `{bot_token, ilink_bot_id, ilink_user_id, baseurl}`。
//! 3. 凭据落 store `credentials` 表。
//!
//! 登录涉及真机扫码，**不写自动测试**（端到端验收时验）。

use std::time::Duration;

use base64::Engine;
use rand::Rng;
use serde::{Deserialize, Serialize};

use imagent_core::{CoreError, Result};
use imagent_store::Store;

use crate::client::DEFAULT_BASE_URL;
use crate::proto::{QrcodeResp, QrcodeStatus};

/// 登录所得凭据（落盘 `credentials.blob`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub bot_token: String,
    pub ilink_bot_id: String,
    pub ilink_user_id: String,
    pub baseurl: String,
}

/// 用默认 base URL（`https://ilinkai.weixin.qq.com`）执行扫码登录。
pub async fn login_flow(store: &Store) -> Result<Credentials> {
    login_flow_with_base(store, DEFAULT_BASE_URL).await
}

/// 可指定 base URL 的登录流程（便于测试/灰度）。
pub async fn login_flow_with_base(store: &Store, base_url: &str) -> Result<Credentials> {
    let http = reqwest::Client::builder()
        // get_qrcode_status 是长轮询，未扫码时服务端 hold ~35s 才返回，
        // 故 client 超时需覆盖该窗口（留足余量）。
        .timeout(Duration::from_secs(45))
        // 禁 redirect：与运行时 client（client.rs）一致，防止 login 阶段被 3xx
        // 重定向劫持到非预期域名（SSRF / 凭据泄漏面，P1-J）。
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| CoreError::Platform("ilink", format!("build http client: {e}")))?;

    // 1. 取二维码（bot_type 作为 URL query 参数，curl 实测；body 为空对象）
    let body = serde_json::json!({});
    let qr: QrcodeResp = post_noauth(
        &http,
        base_url,
        "/ilink/bot/get_bot_qrcode?bot_type=3",
        &body,
    )
    .await?;
    // ret 非 0 或缺 qrcode hex token 视为失败
    if qr.ret.unwrap_or(0) != 0 || qr.qrcode.is_none() {
        return Err(CoreError::Platform(
            "ilink",
            format!(
                "get_bot_qrcode failed: ret={:?} err={:?}",
                qr.ret, qr.err_msg
            ),
        ));
    }
    // hex token：用于下一步 get_qrcode_status 的 query
    let qrcode_value = qr.qrcode.clone().unwrap_or_default();
    // 扫码 URL 优先用完整的 liteapp URL（更利于终端识别），缺省回退 hex token
    let qr_scan_data = qr
        .qrcode_img_content
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| qrcode_value.clone());
    print_qrcode(&qr_scan_data);
    tracing::info!(target: "ilink", "qrcode printed; waiting for scan…");

    // 2. 轮询扫码状态（长轮询：未扫码时服务端 hold ~35s 才返回，正常）
    loop {
        let endpoint = format!("/ilink/bot/get_qrcode_status?qrcode={qrcode_value}");
        let st: QrcodeStatus = post_noauth(&http, base_url, &endpoint, &body).await?;
        match st.status.as_deref() {
            Some("wait") | Some("scaned") => continue,
            Some("scaned_but_redirect") => {
                // P1：仅 log redirect_host，不切换 base_url
                tracing::info!(
                    target: "ilink",
                    redirect_host = ?st.redirect_host,
                    "qrcode scanned, redirect indicated (ignored at P1)"
                );
                continue;
            }
            Some("expired") => {
                return Err(CoreError::Platform(
                    "ilink",
                    "qrcode expired, please re-run login to scan again".into(),
                ))
            }
            Some("confirmed") => {
                let creds = Credentials {
                    bot_token: st.bot_token.ok_or_else(|| {
                        CoreError::Platform("ilink", "confirmed missing bot_token".into())
                    })?,
                    ilink_bot_id: st.ilink_bot_id.ok_or_else(|| {
                        CoreError::Platform("ilink", "confirmed missing ilink_bot_id".into())
                    })?,
                    ilink_user_id: st.ilink_user_id.ok_or_else(|| {
                        CoreError::Platform("ilink", "confirmed missing ilink_user_id".into())
                    })?,
                    baseurl: {
                        let raw = st.baseurl.unwrap_or_else(|| base_url.to_string());
                        validate_baseurl(&raw)?
                    },
                };
                // 3. 落盘
                let blob = serde_json::to_string(&creds)
                    .map_err(|e| CoreError::Platform("ilink", format!("serialize creds: {e}")))?;
                store
                    .put_credential("ilink", &creds.ilink_bot_id, &blob)
                    .await?;
                tracing::info!(target: "ilink", bot_id = %creds.ilink_bot_id, "login ok, credentials stored");
                return Ok(creds);
            }
            Some(other) => {
                return Err(CoreError::Platform(
                    "ilink",
                    format!("unknown qrcode status: {other}"),
                ))
            }
            None => continue,
        }
    }
}

/// 未鉴权 POST（登录阶段无 bot_token）。仍带 `AuthorizationType` + 随机
/// `X-WECHAT-UIN`（与运行时请求一致，避免被网关按缺头拒绝）。
async fn post_noauth<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    base_url: &str,
    endpoint: &str,
    body: &serde_json::Value,
) -> Result<T> {
    let url = format!("{base_url}{endpoint}");
    let uin = base64::engine::general_purpose::STANDARD
        .encode(rand::thread_rng().gen::<u32>().to_le_bytes());
    let resp = http
        .post(&url)
        .header("AuthorizationType", "ilink_bot_token")
        .header("X-WECHAT-UIN", uin)
        .json(body)
        .send()
        .await
        .map_err(|e| CoreError::Platform("ilink", format!("POST {endpoint}: {e}")))?;

    let status = resp.status();
    if status.is_server_error() || status.is_client_error() {
        return Err(CoreError::Platform(
            "ilink",
            format!("POST {endpoint}: HTTP {status}"),
        ));
    }
    resp.json::<T>()
        .await
        .map_err(|e| CoreError::Platform("ilink", format!("POST {endpoint}: decode: {e}")))
}

/// 用 `qrcode` crate 渲染终端二维码；渲染失败则打印原始内容兜底。
fn print_qrcode(content: &str) {
    match qrcode::QrCode::new(content.as_bytes()) {
        Ok(code) => {
            let image = code
                .render::<qrcode::render::unicode::Dense1x2>()
                .dark_color(qrcode::render::unicode::Dense1x2::Dark)
                .light_color(qrcode::render::unicode::Dense1x2::Light)
                .build();
            println!("\n请使用微信扫描以下二维码登录：\n");
            println!("{image}");
            println!("（若终端无法识别，可手动复制内容）\n{content}\n");
        }
        Err(e) => {
            tracing::warn!(target: "ilink", "render qrcode failed: {e}");
            println!("无法渲染终端二维码（内容过长？）：\n{content}\n");
        }
    }
}

/// 校验服务端返回的 baseurl：必须 `https://` 且 host 为 `ilinkai.weixin.qq.com`
/// 或以 `.weixin.qq.com` 结尾。防止 MITM/DNS 劫持把凭据与消息导向恶意域名。
fn validate_baseurl(raw: &str) -> Result<String> {
    let rest = raw.strip_prefix("https://").ok_or_else(|| {
        CoreError::Platform("ilink", format!("baseurl 必须 https:// 开头: {raw:?}"))
    })?;
    let host = rest.split(['/', ':', '?', '#']).next().unwrap_or("");
    if host.is_empty() {
        return Err(CoreError::Platform(
            "ilink",
            format!("baseurl 缺 host: {raw:?}"),
        ));
    }
    if host == "ilinkai.weixin.qq.com" || host.ends_with(".weixin.qq.com") {
        Ok(raw.to_string())
    } else {
        Err(CoreError::Platform(
            "ilink",
            format!("baseurl host 不在白名单 (*.weixin.qq.com): {host}（原始 {raw:?}）"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::validate_baseurl;

    #[test]
    fn baseurl_default_ok() {
        assert!(validate_baseurl("https://ilinkai.weixin.qq.com").is_ok());
    }

    #[test]
    fn baseurl_subdomain_ok() {
        assert!(validate_baseurl("https://foo.weixin.qq.com/path").is_ok());
    }

    #[test]
    fn baseurl_evil_domain_rejected() {
        // 关键回归：相似但恶意域名必须拒
        assert!(validate_baseurl("https://ilinkai.weixin.qq.com.evil.com").is_err());
        assert!(validate_baseurl("https://evil.com").is_err());
    }

    #[test]
    fn baseurl_internal_ip_rejected() {
        assert!(validate_baseurl("https://169.254.169.254/latest/meta-data").is_err());
        assert!(validate_baseurl("https://10.0.0.1").is_err());
    }

    #[test]
    fn baseurl_plain_http_rejected() {
        assert!(validate_baseurl("http://ilinkai.weixin.qq.com").is_err());
        assert!(validate_baseurl("ws://ilinkai.weixin.qq.com").is_err());
    }
}
