//! iLink 协议响应结构（反序列化容错）。
//!
//! 字段名以 OpenClaw Weixin channel 协议（hermes/feiyun 验证）为准，
//! 用 `rename_all = "camelCase"` 匹配官方响应，并对常见异名加 `alias` 容错。
//! 全部字段 `default`，避免缺字段反序列化崩溃（真机验收时据实微调）。

use serde::Deserialize;

use imagent_core::{ConvId, InboundMessage, ReplyHint, UserId};

/// 取登录二维码响应。
///
/// 真实字段为 **snake_case**（curl 实测样本），故结构上去掉 `rename_all="camelCase"`，
/// 逐字段显式 rename/alias。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QrcodeResp {
    /// hex token（用于 get_qrcode_status 的 query）。
    #[serde(
        default,
        alias = "qrcode_token",
        alias = "token",
        alias = "qrcodeUrl",
        alias = "url"
    )]
    pub qrcode: Option<String>,
    /// 完整可扫码 liteapp URL（终端二维码渲染这个）。snake_case，需显式 rename。
    #[serde(default, rename = "qrcode_img_content", alias = "qrcodeImgContent")]
    pub qrcode_img_content: Option<String>,
    /// 0=成功，非 0=错误。
    #[serde(default)]
    pub ret: Option<i64>,
    /// 错误信息（ret 非 0 时）。
    #[serde(default, alias = "errmsg", alias = "errMsg")]
    pub err_msg: Option<String>,
}
/// 扫码状态响应。
///
/// `status` 取值：`wait` / `scaned` / `scaned_but_redirect`（带 `redirect_host`）/
/// `confirmed` / `expired`。`confirmed` 时附带 `bot_token` / `ilink_bot_id` /
/// `ilink_user_id` / `baseurl`。
///
/// 真实字段为 **snake_case**（hermes 实测），故去掉 `rename_all="camelCase"`，
/// 逐字段显式 rename/alias；对历史 camelCase 样本保留 alias 容错。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QrcodeStatus {
    #[serde(default, alias = "code", alias = "state")]
    pub status: Option<String>,
    #[serde(default, alias = "botToken", alias = "bot_token")]
    pub bot_token: Option<String>,
    #[serde(default, alias = "botId", alias = "bot_id", alias = "ilinkBotId")]
    pub ilink_bot_id: Option<String>,
    #[serde(default, alias = "userId", alias = "user_id", alias = "ilinkUserId")]
    pub ilink_user_id: Option<String>,
    #[serde(default, alias = "baseUrl", alias = "base_url")]
    pub baseurl: Option<String>,
    /// `scaned_but_redirect` 时携带的重定向主机（P1 仅 log，不切换 base_url）。
    #[serde(default, alias = "redirectHost", alias = "redirect_host")]
    pub redirect_host: Option<String>,
}

/// 收消息（长轮询）响应。
///
/// 顶层仅 `msgs` + `get_updates_buf`（curl 实测：无顶层 `context_token`，
/// 每条 msg 自带 `context_token`）。顶层另有 `sync_buf`，忽略。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdatesResp {
    #[serde(default, alias = "messages", alias = "data", alias = "list")]
    pub msgs: Vec<Msg>,
    /// 长轮询游标（真实字段 `get_updates_buf`；alias 兼容 camelCase 与异名）。
    #[serde(
        default,
        alias = "getUpdatesBuf",
        alias = "buf",
        alias = "cursor",
        alias = "next_cursor"
    )]
    pub get_updates_buf: Option<String>,
}

/// 单条入站消息（snake_case，curl/hermes 验证）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Msg {
    #[serde(default)]
    pub from_user_id: String,
    #[serde(
        default,
        alias = "id",
        alias = "messageId",
        alias = "msg_id",
        alias = "msgid"
    )]
    pub message_id: Option<serde_json::Value>,
    #[serde(default)]
    pub context_token: Option<String>,
    #[serde(default, alias = "msgtype", alias = "type")]
    #[allow(dead_code)]
    pub msg_type: Option<i64>,
    #[serde(default)]
    pub item_list: Vec<Item>,
}

/// msg 内条目；`type==1` 为文本，`type==3` 为语音（转写文本在 `voice_item.text`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Item {
    #[serde(default, rename = "type")]
    pub item_type: i64,
    #[serde(default)]
    pub text_item: Option<TextItem>,
    #[serde(default)]
    pub voice_item: Option<VoiceItem>,
    /// type==2 图片。
    #[serde(default)]
    pub image_item: Option<ImageItem>,
    /// type==4 文件。
    #[serde(default)]
    pub file_item: Option<FileItem>,
    /// type==5 视频（媒体引用同 file，按 file 处理）。
    #[serde(default)]
    pub video_item: Option<FileItem>,
}

/// 媒体引用（CDN 凭证 / aes_key / 直链）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaRefProto {
    /// CDN 下载凭证（优先于 full_url）。
    #[serde(default)]
    pub encrypt_query_param: Option<String>,
    /// base64 编码的 aes_key（video/file/voice 通用）。
    #[serde(default)]
    pub aes_key: Option<String>,
    /// 直链（不可信，下载前必须 SSRF 白名单校验）。
    #[serde(default)]
    pub full_url: Option<String>,
}

/// 图片条目；image 的 aeskey 是**裸 hex**（与其它媒体 base64 不同，hermes 实测）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImageItem {
    #[serde(default)]
    pub aeskey: Option<String>,
    #[serde(default)]
    pub media: Option<MediaRefProto>,
}

/// 文件条目（file/video 通用）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FileItem {
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub media: Option<MediaRefProto>,
}

/// 文本条目载荷。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TextItem {
    #[serde(default)]
    pub text: Option<String>,
}

/// 语音条目载荷；媒体字段忽略，仅取转写文本 `text`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct VoiceItem {
    #[serde(default)]
    pub text: Option<String>,
}

/// 从 `item_list` 提取首个可用文本：
/// - `type==1`（文本）取 `text_item.text`
/// - `type==3`（语音）取 `voice_item.text`（转写文本）
pub fn extract_text(msg: &Msg) -> String {
    for item in &msg.item_list {
        match item.item_type {
            1 => {
                if let Some(ti) = &item.text_item {
                    if let Some(t) = &ti.text {
                        if !t.is_empty() {
                            return t.clone();
                        }
                    }
                }
            }
            3 => {
                if let Some(vi) = &item.voice_item {
                    if let Some(t) = &vi.text {
                        if !t.is_empty() {
                            return t.clone();
                        }
                    }
                }
            }
            _ => {}
        }
    }
    String::new()
}

/// 将协议消息转换为 core 的 `InboundMessage`（纯函数，可单测）。
///
/// - `conv_id = "ilink:<from_user_id>"`
/// - `sender = from_user_id`
/// - `text` 取自 `item_list`（`type==1` 的 `text_item.text`）
/// - `reply_hint` 携带 msg 的 `context_token`（空则默认空串）
/// - `media` 仍为空：媒体引用由 `extract_media_refs` 提取，platform 层异步下载后填充。
pub fn msg_to_inbound(msg: &Msg) -> InboundMessage {
    let text = extract_text(msg);
    InboundMessage {
        conv_id: ConvId(format!("ilink:{}", msg.from_user_id)),
        sender: UserId(msg.from_user_id.clone()),
        text: if text.is_empty() { None } else { Some(text) },
        media: Vec::new(),
        media_errors: Vec::new(),
        mentions: Vec::new(),
        reply_hint: ReplyHint::ILink {
            context_token: msg.context_token.clone().unwrap_or_default(),
        },
    }
}

/// 入站媒体引用（platform 层据此下载/解密/存盘，再生成 `MediaRef`）。
#[derive(Debug, Clone)]
pub struct RawMediaRef {
    /// `"image"` / `"file"`（video/file/voice 一律按 file 处理，hermes 经验）。
    pub kind: &'static str,
    /// CDN 凭证（优先）；缺失时 fallback `full_url`。
    pub encrypt_query_param: Option<String>,
    /// aes_key 的原始字符串（image=裸 hex；其它=base64；可能内含 hex）。
    pub aes_key: Option<String>,
    /// 直链（不可信，下载前必须 SSRF 白名单校验）。
    pub full_url: Option<String>,
    /// 文件名（用于推断扩展名；可空）。
    pub file_name: Option<String>,
}

/// 从 `item_list` 提取媒体引用（image=type2 / file=type4 / video=type5）。
///
/// aes_key 来源（hermes 实测的非对称编码）：
/// - image：优先 `image_item.aeskey`（裸 hex），其次 `media.aes_key`。
/// - file/video：`media.aes_key`（base64）。
pub fn extract_media_refs(msg: &Msg) -> Vec<RawMediaRef> {
    let mut out = Vec::new();
    for item in &msg.item_list {
        match item.item_type {
            2 => {
                if let Some(img) = &item.image_item {
                    let (eqp, key, url) = media_triple(img.media.as_ref());
                    out.push(RawMediaRef {
                        kind: "image",
                        encrypt_query_param: eqp,
                        aes_key: key.or_else(|| img.aeskey.clone()),
                        full_url: url,
                        file_name: None,
                    });
                }
            }
            4 | 5 => {
                let fi = item.file_item.as_ref().or(item.video_item.as_ref());
                if let Some(fi) = fi {
                    let (eqp, key, url) = media_triple(fi.media.as_ref());
                    out.push(RawMediaRef {
                        kind: "file",
                        encrypt_query_param: eqp,
                        aes_key: key,
                        full_url: url,
                        file_name: fi.file_name.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// 从 `MediaRefProto` 取 (encrypt_query_param, aes_key, full_url) 三元组。
fn media_triple(m: Option<&MediaRefProto>) -> (Option<String>, Option<String>, Option<String>) {
    match m {
        Some(m) => (
            m.encrypt_query_param.clone(),
            m.aes_key.clone(),
            m.full_url.clone(),
        ),
        None => (None, None, None),
    }
}
/// sendmessage 响应（HTTP 200 body，snake_case 单层，无 `base_resp` 包裹，hermes 实测）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SendMsgResp {
    #[serde(default)]
    pub ret: Option<i64>,
    #[serde(default, alias = "errCode")]
    pub errcode: Option<i64>,
    #[serde(default, alias = "msg", alias = "errMsg")]
    pub errmsg: Option<String>,
}

/// getconfig 响应（hermes 实测：含 typing_ticket）。
///
/// 字段 snake_case 为主，对 camelCase 异名容错；全部 `default` 防缺字段崩溃。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GetConfigResp {
    #[serde(default, alias = "typingTicket")]
    pub typing_ticket: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // 协议字段，保留供诊断
    pub ret: Option<i64>,
    #[serde(default, alias = "errmsg", alias = "msg")]
    #[allow(dead_code)] // 协议字段，保留供诊断
    pub err_msg: Option<String>,
}

/// sendmessage 响应分类（按协议事实 §2 优先级）。
#[derive(Debug)]
pub enum SendOutcome {
    Success,
    RateLimited,
    SessionExpired,
    OtherError(String),
}

/// 按 hermes 实测规则分类 sendmessage 响应。
///
/// 优先级：成功 → session 过期（-14 或 -2+"unknown error" 伪装）→ 限流（-2）→ 其他。
pub fn classify_send(resp: &SendMsgResp) -> SendOutcome {
    let ret = resp.ret.unwrap_or(0);
    let errcode = resp.errcode.unwrap_or(0);
    let errmsg_norm = resp
        .errmsg
        .as_deref()
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();

    // 成功：两者都 ∈ {0}（None 视为 0）。
    if ret == 0 && errcode == 0 {
        return SendOutcome::Success;
    }
    // session 过期：任一 == -14；或（任一 == -2 且 errmsg == "unknown error" 伪装）。
    if ret == -14 || errcode == -14 {
        return SendOutcome::SessionExpired;
    }
    if (ret == -2 || errcode == -2) && errmsg_norm == "unknown error" {
        return SendOutcome::SessionExpired;
    }
    // 限流：任一 == -2（到这里说明不是上面的伪装）。
    if ret == -2 || errcode == -2 {
        return SendOutcome::RateLimited;
    }
    // 其他错误。
    SendOutcome::OtherError(format!(
        "ret={:?} errcode={:?} errmsg={:?}",
        resp.ret, resp.errcode, resp.errmsg
    ))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_text_from_item_list() {
        // 真实 msg 样本：文本在 item_list 中 type==1 的 text_item.text。
        let json = r#"{"from_user_id":"u1","message_id":"m1","context_token":"tok","msg_type":1,"item_list":[{"type":1,"text_item":{"text":"hello"}}]}"#;
        let m: Msg = serde_json::from_str(json).unwrap();
        assert_eq!(extract_text(&m), "hello");
        let ib = msg_to_inbound(&m);
        assert_eq!(ib.conv_id.0, "ilink:u1");
        assert_eq!(ib.sender.0, "u1");
        assert_eq!(ib.text.as_deref(), Some("hello"));
        assert!(ib.media.is_empty());
        match ib.reply_hint {
            ReplyHint::ILink { context_token } => assert_eq!(context_token, "tok"),
            ReplyHint::None => panic!("expected ILink hint"),
        }
    }

    #[test]
    fn extract_text_empty_when_no_text_item() {
        // item_list 缺文本 → text 为 None，token 仍透传。
        let json = r#"{"from_user_id":"u1","item_list":[]}"#;
        let m: Msg = serde_json::from_str(json).unwrap();
        assert_eq!(extract_text(&m), "");
        let ib = msg_to_inbound(&m);
        assert_eq!(ib.text, None);
        match ib.reply_hint {
            ReplyHint::ILink { context_token } => assert_eq!(context_token, ""),
            ReplyHint::None => panic!("expected ILink hint"),
        }
    }

    #[test]
    fn extract_text_skips_non_text_items() {
        // type!=1 的条目跳过，取首个 type==1。
        let json = r#"{"from_user_id":"u","item_list":[{"type":2},{"type":1,"text_item":{"text":"first"}}]}"#;
        let m: Msg = serde_json::from_str(json).unwrap();
        assert_eq!(extract_text(&m), "first");
    }

    #[test]
    fn parse_updates_resp_real_sample() {
        // curl 实测样本（snake_case 顶层，含 sync_buf 忽略）。
        let json = r#"{"msgs":[{"from_user_id":"u1","message_id":"m1","context_token":"t","msg_type":1,"item_list":[{"type":1,"text_item":{"text":"hi"}}]}],"sync_buf":"SB","get_updates_buf":"CgkI"}"#;
        let r: UpdatesResp = serde_json::from_str(json).unwrap();
        assert_eq!(r.msgs.len(), 1);
        assert_eq!(r.msgs[0].from_user_id, "u1");
        assert_eq!(
            r.msgs[0].message_id.as_ref().and_then(|v| v.as_str()),
            Some("m1")
        );
        assert_eq!(r.get_updates_buf.as_deref(), Some("CgkI"));
    }

    #[test]
    fn parse_empty_updates_resp() {
        // 缺字段不崩。
        let json = r#"{}"#;
        let r: UpdatesResp = serde_json::from_str(json).unwrap();
        assert!(r.msgs.is_empty());
        assert!(r.get_updates_buf.is_none());
    }

    #[test]
    fn parse_qrcode_status_confirmed() {
        let json = r#"{"status":"confirmed","botToken":"bt","ilinkBotId":"bid","ilinkUserId":"uid","baseurl":"https://x"}"#;
        let s: QrcodeStatus = serde_json::from_str(json).unwrap();
        assert_eq!(s.status.as_deref(), Some("confirmed"));
        assert_eq!(s.bot_token.as_deref(), Some("bt"));
        assert_eq!(s.ilink_bot_id.as_deref(), Some("bid"));
        assert_eq!(s.ilink_user_id.as_deref(), Some("uid"));
        assert_eq!(s.baseurl.as_deref(), Some("https://x"));
    }
    #[test]
    fn parse_qrcode_resp_real_sample() {
        // curl 实测样本（snake_case 字段）
        let json = r#"{"qrcode":"57677957d0077a13666d59fc00f6fb5c","qrcode_img_content":"https://liteapp.weixin.qq.com/q/7GiQu1?qrcode=57677957d0077a13666d59fc00f6fb5c&bot_type=3","ret":0}"#;
        let r: QrcodeResp = serde_json::from_str(json).unwrap();
        assert_eq!(
            r.qrcode.as_deref(),
            Some("57677957d0077a13666d59fc00f6fb5c")
        );
        assert!(r
            .qrcode_img_content
            .as_deref()
            .unwrap()
            .starts_with("https://liteapp.weixin.qq.com/"));
        assert_eq!(r.ret, Some(0));
        assert!(r.err_msg.is_none());
    }

    #[test]
    fn parse_qrcode_resp_error() {
        // ret 非 0 错误响应
        let json = r#"{"err_msg":"missing bot_type","ret":1}"#;
        let r: QrcodeResp = serde_json::from_str(json).unwrap();
        assert_eq!(r.ret, Some(1));
        assert_eq!(r.err_msg.as_deref(), Some("missing bot_type"));
        assert!(r.qrcode.is_none());
    }

    #[test]
    fn qrcode_resp_ret_zero_is_success() {
        // 取码成功判定逻辑（login 同款判定）：ret==0 且有 qrcode hex token
        let json = r#"{"qrcode":"abc","ret":0}"#;
        let r: QrcodeResp = serde_json::from_str(json).unwrap();
        assert!(r.ret.unwrap_or(0) == 0 && r.qrcode.is_some());
    }

    #[test]
    fn qrcode_resp_ret_nonzero_is_failure() {
        let json = r#"{"ret":1,"err_msg":"bad"}"#;
        let r: QrcodeResp = serde_json::from_str(json).unwrap();
        assert!(r.ret.unwrap_or(0) != 0 || r.qrcode.is_none());
    }

    #[test]
    fn parse_qrcode_status_scaned_but_redirect() {
        let json =
            r#"{"status":"scaned_but_redirect","redirect_host":"https://ilink-bak.weixin.qq.com"}"#;
        let s: QrcodeStatus = serde_json::from_str(json).unwrap();
        assert_eq!(s.status.as_deref(), Some("scaned_but_redirect"));
        assert_eq!(
            s.redirect_host.as_deref(),
            Some("https://ilink-bak.weixin.qq.com")
        );
    }

    #[test]
    fn parse_qrcode_status_confirmed_snake_case() {
        // 真实 confirmed 样本（snake_case）
        let json = r#"{"status":"confirmed","bot_token":"bt","ilink_bot_id":"bid","ilink_user_id":"uid","baseurl":"https://ilinkai.weixin.qq.com"}"#;
        let s: QrcodeStatus = serde_json::from_str(json).unwrap();
        assert_eq!(s.status.as_deref(), Some("confirmed"));
        assert_eq!(s.bot_token.as_deref(), Some("bt"));
        assert_eq!(s.ilink_bot_id.as_deref(), Some("bid"));
        assert_eq!(s.ilink_user_id.as_deref(), Some("uid"));
        assert_eq!(s.baseurl.as_deref(), Some("https://ilinkai.weixin.qq.com"));
    }

    #[test]
    fn parse_updates_resp_numeric_message_id_and_voice() {
        // curl 实测样本：message_id 为裸数字（非字符串），item_list 为语音转写（type==3）。
        let json = r#"{"msgs":[{"seq":2,"message_id":7477577657987792776,"from_user_id":"o9cq804lZUXdvf2eN6CDMFQJeyYQ@im.wechat","to_user_id":"150418d37ae5@im.bot","client_id":"x","create_time_ms":1782793439833,"message_type":1,"message_state":2,"item_list":[{"type":3,"create_time_ms":1782793439833,"is_completed":true,"msg_id":"v1:4108213093427624530","voice_item":{"text":"你好，你好，测试验证","media":{"encrypt_query_param":"x","aes_key":"y","full_url":"z"}}}],"context_token":"AARzJWAFAAABAAAAAACSV+1ou9OOC8ZA4ERDaiAAAAB+9905Q6UiugPBawU3n3cyzQX+LkN8ofRzsCZYN0mt7oitn7j0r/pDtU4YYseJixIz5j1U5+OveKdLZAd1oU15zmn2oHGQ","root_id":0,"parent_id":0}],"get_updates_buf":"Buf"}"#;
        let r: UpdatesResp = serde_json::from_str(json).expect("must decode numeric message_id");
        assert_eq!(r.msgs.len(), 1);
        let m = &r.msgs[0];
        // message_id 是数字，反序列化为 Value::Number。
        assert_eq!(
            m.message_id.as_ref().unwrap().to_string(),
            "7477577657987792776"
        );
        assert_eq!(extract_text(m), "你好，你好，测试验证");
        let ib = msg_to_inbound(m);
        assert_eq!(ib.conv_id.0, "ilink:o9cq804lZUXdvf2eN6CDMFQJeyYQ@im.wechat");
        assert_eq!(ib.sender.0, "o9cq804lZUXdvf2eN6CDMFQJeyYQ@im.wechat");
        assert_eq!(ib.text.as_deref(), Some("你好，你好，测试验证"));
        match &ib.reply_hint {
            ReplyHint::ILink { context_token } => {
                assert!(context_token.starts_with("AARzJWAFAA"));
            }
            ReplyHint::None => panic!("expected ILink hint"),
        }
    }

    #[test]
    fn extract_text_from_voice_item() {
        // 仅语音条目（type==3），文本来自 voice_item.text。
        let json =
            r#"{"from_user_id":"u","item_list":[{"type":3,"voice_item":{"text":"语音转写"}}]}"#;
        let m: Msg = serde_json::from_str(json).unwrap();
        assert_eq!(extract_text(&m), "语音转写");
    }

    #[test]
    fn extract_text_still_handles_text_item_regression() {
        // 回归：普通文本消息（type==1）仍正常。
        let json = r#"{"from_user_id":"u","message_id":42,"item_list":[{"type":1,"text_item":{"text":"plain"}}]}"#;
        let m: Msg = serde_json::from_str(json).unwrap();
        assert_eq!(extract_text(&m), "plain");
        // 数字 message_id 也能 decode。
        assert_eq!(m.message_id.as_ref().unwrap().to_string(), "42");
    }
    #[test]
    fn classify_success_both_zero() {
        let r = SendMsgResp {
            ret: Some(0),
            errcode: Some(0),
            errmsg: Some("ok".into()),
        };
        assert!(matches!(classify_send(&r), SendOutcome::Success));
    }

    #[test]
    fn classify_success_none_fields() {
        // ret/errcode 为 None 视为 0 → 成功。
        let r = SendMsgResp::default();
        assert!(matches!(classify_send(&r), SendOutcome::Success));
    }

    #[test]
    fn classify_session_expired_minus14() {
        let r = SendMsgResp {
            ret: Some(-14),
            errcode: Some(0),
            errmsg: None,
        };
        assert!(matches!(classify_send(&r), SendOutcome::SessionExpired));
        let r2 = SendMsgResp {
            ret: Some(0),
            errcode: Some(-14),
            errmsg: None,
        };
        assert!(matches!(classify_send(&r2), SendOutcome::SessionExpired));
    }

    #[test]
    fn classify_session_expired_unknown_error_disguise() {
        // ret==-2 且 errmsg=="unknown error"（伪装的 stale session）→ SessionExpired，不是 RateLimited。
        let r = SendMsgResp {
            ret: Some(-2),
            errcode: Some(0),
            errmsg: Some("unknown error".into()),
        };
        assert!(matches!(classify_send(&r), SendOutcome::SessionExpired));
        // 大小写/首尾空白容错。
        let r2 = SendMsgResp {
            ret: Some(0),
            errcode: Some(-2),
            errmsg: Some("  Unknown Error  ".into()),
        };
        assert!(matches!(classify_send(&r2), SendOutcome::SessionExpired));
    }

    #[test]
    fn classify_rate_limited_minus2() {
        // -2 但 errmsg 非 "unknown error" → 限流。
        let r = SendMsgResp {
            ret: Some(-2),
            errcode: Some(0),
            errmsg: Some("rate limited".into()),
        };
        assert!(matches!(classify_send(&r), SendOutcome::RateLimited));
        let r2 = SendMsgResp {
            ret: Some(0),
            errcode: Some(-2),
            errmsg: None,
        };
        assert!(matches!(classify_send(&r2), SendOutcome::RateLimited));
    }

    #[test]
    fn classify_other_error() {
        let r = SendMsgResp {
            ret: Some(-99),
            errcode: Some(0),
            errmsg: Some("boom".into()),
        };
        match classify_send(&r) {
            SendOutcome::OtherError(s) => {
                assert!(s.contains("ret=Some(-99)") && s.contains("boom"))
            }
            other => panic!("expected OtherError, got {other:?}"),
        }
    }

    #[test]
    fn extract_media_refs_image_and_file() {
        // image(type2)：aeskey 在 image_item.aeskey（裸 hex），media 带 eqp。
        let json = r#"{"from_user_id":"u","item_list":[
            {"type":2,"image_item":{"aeskey":"00112233445566778899aabbccddeeff","media":{"encrypt_query_param":"IMGQ"}}},
            {"type":4,"file_item":{"file_name":"a.pdf","media":{"encrypt_query_param":"FILEQ","aes_key":"AAAA"}}}
        ]}"#;
        let m: Msg = serde_json::from_str(json).unwrap();
        let refs = extract_media_refs(&m);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].kind, "image");
        assert_eq!(refs[0].encrypt_query_param.as_deref(), Some("IMGQ"));
        // image 优先 image_item.aeskey。
        assert_eq!(
            refs[0].aes_key.as_deref(),
            Some("00112233445566778899aabbccddeeff")
        );
        assert_eq!(refs[1].kind, "file");
        assert_eq!(refs[1].encrypt_query_param.as_deref(), Some("FILEQ"));
        assert_eq!(refs[1].aes_key.as_deref(), Some("AAAA"));
        assert_eq!(refs[1].file_name.as_deref(), Some("a.pdf"));
    }

    #[test]
    fn extract_media_refs_empty_when_no_media_items() {
        let json = r#"{"from_user_id":"u","item_list":[{"type":1,"text_item":{"text":"hi"}}]}"#;
        let m: Msg = serde_json::from_str(json).unwrap();
        assert!(extract_media_refs(&m).is_empty());
    }

    #[test]
    fn extract_media_refs_image_falls_back_to_media_aes_key() {
        // image 无 image_item.aeskey，回退到 media.aes_key。
        let json = r#"{"from_user_id":"u","item_list":[
            {"type":2,"image_item":{"media":{"encrypt_query_param":"Q","aes_key":"FFFF"}}}
        ]}"#;
        let m: Msg = serde_json::from_str(json).unwrap();
        let refs = extract_media_refs(&m);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].aes_key.as_deref(), Some("FFFF"));
    }
}
