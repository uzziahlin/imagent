//! 回环集成测试：真 [`LocalPlatform`]（unix socket，临时路径）+ tokio 模拟
//! 终端客户端（socket 直连，hello 免 token——同 uid 测试进程天然过 peer uid
//! 校验），覆盖任务说明场景：
//! 1. hello 握手（免 token）+ hello_ack 回显会话名；坏 hello → 4002 断开；
//! 2. message 帧 → `Platform::recv`（conv=local:<session>，sender=local:user）；
//! 3. send_text → text 帧；
//! 4. approval 闭环（send_permission_ask → approval_request 帧 →
//!    approval_response → recv 带 ask_req+y → resolve → approval_resolved 帧）；
//! 5. sync 重发（已送达文本落缓存 → 断开重连 → 快照一条 text 帧）；
//! 6. 踢旧（第二个连接 → 旧连接收 error 4001 后 EOF，新连接可用）；
//! 7. capabilities 钉值 = ASK（真实例）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use imagent_core::{ConvId, PermissionReply, Platform, ReplyHint};
use imagent_localterm::LocalPlatform;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const SESSION: &str = "work";
const CONV: &str = "local:work";

fn tmp_sock(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "imagent-localterm-loop-{}-{}-{name}.sock",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// 轮询等待条件成立（50ms 节拍，防各任务就绪竞态导致的偶发失败）。
async fn eventually(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    f()
}

/// 模拟终端客户端（socket 直连 + 行帧）。
struct TermClient {
    wr: tokio::net::unix::OwnedWriteHalf,
    rd: BufReader<tokio::net::unix::OwnedReadHalf>,
}

impl TermClient {
    /// 连接 + hello（免 token）→ hello_ack。不发 sync（由用例按需发）。
    async fn connect(path: &Path, session: &str) -> Self {
        let stream = UnixStream::connect(path).await.expect("连 local.sock");
        let (rd, wr) = stream.into_split();
        let mut c = Self {
            wr,
            rd: BufReader::new(rd),
        };
        c.send(&format!(
            r#"{{"v":1,"type":"hello","session":"{session}"}}"#
        ))
        .await;
        let ack = c.next_frame().await;
        assert_eq!(ack["type"], "hello_ack", "首帧应为 hello_ack：{ack}");
        assert_eq!(ack["session"], session, "回显会话名：{ack}");
        c
    }

    /// 连接 + hello + sync（重连恢复上下文的完整握手）。
    async fn connect_with_sync(path: &Path, session: &str) -> Self {
        let mut c = Self::connect(path, session).await;
        c.send(r#"{"v":1,"type":"sync"}"#).await;
        c
    }

    async fn send(&mut self, json: &str) {
        self.wr.write_all(json.as_bytes()).await.unwrap();
        self.wr.write_all(b"\n").await.unwrap();
    }

    /// 读下一帧（5s 超时防挂死）；EOF 返回 None（连接关闭）。
    async fn try_next_frame(&mut self) -> Option<serde_json::Value> {
        let mut line = String::new();
        let n = match tokio::time::timeout(Duration::from_secs(5), self.rd.read_line(&mut line))
            .await
            .expect("等帧超时（5s）")
        {
            Ok(n) => n,
            Err(_) => return None,
        };
        if n == 0 {
            return None;
        }
        Some(serde_json::from_str(line.trim()).expect("帧 JSON"))
    }

    /// 读下一帧；EOF / 超时视为失败（期待连接存活时使用）。
    async fn next_frame(&mut self) -> serde_json::Value {
        self.try_next_frame()
            .await
            .expect("连接不应提前 EOF / 等帧超时")
    }

    /// 读到指定 type 的下一帧（跳过其它帧；error 帧直接失败）。
    async fn next_of_type(&mut self, ty: &str) -> serde_json::Value {
        loop {
            let v = self.next_frame().await;
            assert_ne!(v["type"], serde_json::json!("error"), "意外 error 帧：{v}");
            if v["type"] == ty {
                return v;
            }
        }
    }
}

async fn setup(name: &str) -> (PathBuf, LocalPlatform, TermClient) {
    let path = tmp_sock(name);
    let platform = LocalPlatform::new(path.clone()).expect("bind 测试 socket");
    let term = TermClient::connect_with_sync(&path, SESSION).await;
    // 等 handshake 完成 + 就绪（pump 开始投递）。
    assert!(
        eventually(Duration::from_secs(5), || platform.is_online()).await,
        "终端连接应就绪"
    );
    (path, platform, term)
}

/// 场景 1+2+3：免 token 握手、message → recv、send_text → text 帧、
/// capabilities 钉值。
#[tokio::test]
async fn hello_message_and_text_roundtrip() {
    let (_path, platform, mut term) = setup("roundtrip").await;

    // capabilities 钉值（真实例）：只声明 ASK。
    assert_eq!(platform.capabilities(), imagent_core::PlatformCaps::ASK);
    assert_eq!(platform.name(), "local");

    // message 帧 → recv（conv 前缀 local:<session>，sender=local:user，steering）。
    term.send(r#"{"v":1,"type":"message","text":"列出目录"}"#)
        .await;
    let msg = tokio::time::timeout(Duration::from_secs(5), platform.recv())
        .await
        .expect("recv 超时")
        .expect("recv 应成功");
    assert_eq!(msg.conv_id.0, CONV);
    assert_eq!(msg.sender.0, "local:user");
    assert_eq!(msg.text.as_deref(), Some("列出目录"));
    assert_eq!(msg.ask_req, None);
    assert!(!msg.no_steer);

    // 外平台 convId 的消息被丢弃（后面紧跟的合法消息先到 recv 即证明）。
    term.send(r#"{"v":1,"type":"message","convId":"web:x","text":"插话"}"#)
        .await;
    term.send(r#"{"v":1,"type":"message","convId":"other","text":"hi"}"#)
        .await;
    let msg = tokio::time::timeout(Duration::from_secs(5), platform.recv())
        .await
        .expect("recv 超时")
        .expect("recv 应成功");
    assert_eq!(msg.conv_id.0, "local:other", "裸会话名归一；web:x 被丢弃");

    // send_text → text 帧（完整 conv 回显）。
    platform
        .send_text(&ConvId(CONV.into()), "第一段输出", &ReplyHint::None)
        .await
        .expect("send_text");
    let f = term.next_of_type("text").await;
    assert_eq!(f["type"], "text");
    assert_eq!(f["convId"], CONV);
    assert_eq!(f["text"], "第一段输出");

    // send_media 覆写：本地文件路径文本（不报错）。
    platform
        .send_media(
            &ConvId(CONV.into()),
            &imagent_core::MediaRef {
                kind: "image".into(),
                url: "/tmp/a.png".into(),
            },
            &ReplyHint::None,
        )
        .await
        .expect("send_media 路径文本");
    let f = term.next_of_type("text").await;
    assert!(f["text"].as_str().unwrap().contains("/tmp/a.png"));
}

/// 场景 4：审批闭环——ask 帧（requestId 锚点）→ 客户端 allow → recv 带
/// ask_req+词表精确词 → resolve → approval_resolved 帧。
#[tokio::test]
async fn approval_full_loop() {
    let (_path, platform, mut term) = setup("approval").await;
    let conv = ConvId(CONV.into());

    let anchor = platform
        .send_permission_ask(&conv, "req-1", "Bash", "rm -rf /tmp/x", &ReplyHint::None)
        .await
        .expect("ask 入队");
    assert_eq!(anchor.as_deref(), Some("req-1"), "锚点 = request_id");

    let f = term.next_of_type("approval_request").await;
    assert_eq!(f["convId"], CONV);
    assert_eq!(f["requestId"], "req-1");
    assert_eq!(f["tool"], "Bash");
    assert!(
        f["inputSummary"].as_str().unwrap().contains("rm -rf"),
        "inputSummary 压缩保留命令：{f}"
    );

    // 客户端回复 allow → recv：ask_req 锚定 + 词表精确词 y。
    term.send(r#"{"v":1,"type":"approval_response","requestId":"req-1","decision":"allow"}"#)
        .await;
    let msg = tokio::time::timeout(Duration::from_secs(5), platform.recv())
        .await
        .expect("recv 超时")
        .expect("recv 应成功");
    assert_eq!(msg.conv_id.0, CONV, "convId 缺省落 hello 会话");
    assert_eq!(msg.ask_req.as_deref(), Some("req-1"));
    assert_eq!(msg.text.as_deref(), Some("y"));
    assert!(
        imagent_core::is_explicit_reply_word(msg.text.as_deref().unwrap()),
        "决策词须命中审批词表"
    );

    // resolve → approval_resolved（allow）帧。
    platform
        .resolve_permission_ask(
            &conv,
            "req-1",
            &PermissionReply {
                allow: true,
                always: false,
                message: None,
                raw_text: None,
                cancelled: false,
            },
        )
        .await
        .expect("resolve");
    let f = term.next_of_type("approval_resolved").await;
    assert_eq!(f["requestId"], "req-1");
    assert_eq!(f["decision"], "allow");

    // cancel_all（/stop 路径）：新 ask 被逐条收敛为 cancelled。
    platform
        .send_permission_ask(&conv, "req-2", "Bash", "curl evil.sh", &ReplyHint::None)
        .await
        .unwrap();
    let _ = term.next_of_type("approval_request").await;
    platform.cancel_all_permission_asks(&conv).await.unwrap();
    let f = term.next_of_type("approval_resolved").await;
    assert_eq!(f["requestId"], "req-2");
    assert_eq!(f["decision"], "cancelled");
}

/// 场景 5：sync 重发——已送达文本落缓存，断开重连后快照一条 text 帧。
#[tokio::test]
async fn sync_restores_delivered_text_cache() {
    let (path, platform, mut term) = setup("sync").await;

    platform
        .send_text(&ConvId(CONV.into()), "第一段", &ReplyHint::None)
        .await
        .unwrap();
    platform
        .send_text(&ConvId(CONV.into()), "第二段", &ReplyHint::None)
        .await
        .unwrap();
    // 客户端收帧 = 已送达（服务端写成功才入缓存）。
    assert_eq!(term.next_of_type("text").await["text"], "第一段");
    assert_eq!(term.next_of_type("text").await["text"], "第二段");

    // 断开 → 服务端离线；重连 + sync → 快照一条（旧→新 join）。
    drop(term);
    assert!(
        eventually(Duration::from_secs(5), || !platform.is_online()).await,
        "断开后应离线"
    );
    let mut term2 = TermClient::connect_with_sync(&path, SESSION).await;
    assert!(
        eventually(Duration::from_secs(5), || platform.is_online()).await,
        "重连后应在线"
    );
    let f = term2.next_of_type("text").await;
    assert_eq!(f["convId"], CONV);
    assert_eq!(f["text"], "第一段\n第二段", "快照按送达顺序 join");
}

/// 场景 6：踢旧——第二个连接接入，旧连接收 error 4001 后 EOF；新连接可用。
#[tokio::test]
async fn second_connection_kicks_first() {
    let (path, platform, mut term1) = setup("kick").await;

    let mut term2 = TermClient::connect_with_sync(&path, "work2").await;
    assert!(
        eventually(Duration::from_secs(5), || platform.is_online()).await,
        "新连接就绪"
    );

    // 旧连接：先收 4001 error 帧，随后 EOF（连接关闭）。
    loop {
        let v = term1.next_frame().await;
        if v["type"] == "error" {
            assert_eq!(v["code"], "4001", "顶替错误码：{v}");
            break;
        }
    }
    let closed = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            if term1.try_next_frame().await.is_none() {
                break true;
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(closed, "顶替后旧连接应关闭（EOF）");

    // 新连接可用（会话按各自 hello 会话名路由）。
    term2.send(r#"{"v":1,"type":"message","text":"hi"}"#).await;
    let msg = tokio::time::timeout(Duration::from_secs(5), platform.recv())
        .await
        .expect("recv 超时")
        .expect("新连接消息应送达");
    assert_eq!(msg.conv_id.0, "local:work2");
}

/// 场景 1b：坏 hello（缺会话名）→ error 4002 + 关闭。
#[tokio::test]
async fn bad_hello_rejected() {
    let path = tmp_sock("badhello");
    let platform = LocalPlatform::new(path.clone()).expect("bind");
    let stream = UnixStream::connect(&path).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);
    // hello 缺 session 字段。
    wr.write_all(br#"{"v":1,"type":"hello"}"#).await.unwrap();
    wr.write_all(b"\n").await.unwrap();

    let mut line = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), rd.read_line(&mut line))
        .await
        .expect("error 帧超时")
        .expect("读正常");
    assert!(n > 0, "应先回 error 帧再关闭");
    let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["type"], "error");
    assert_eq!(v["code"], "4002");

    let mut line = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), rd.read_line(&mut line))
        .await
        .expect("EOF 等待超时")
        .expect("读正常");
    assert_eq!(n, 0, "拒绝后应关闭连接（EOF）");
    drop(platform);
}
