//! 回环集成测试（M2 核心验收 + M5 多终端 v1.1 + v1.3 key 认证）：真 relay
//!（`imagent_relay::spawn_relay` 随机端口）+ 真 `WebPlatform`（出站连
//! `/gateway`，v1.3 起走 ed25519 挑战应答——hello 自报 machine=test-mac →
//! challenge → prove → hello_ack）+ tokio-tungstenite 模拟浏览器（连
//! `/ws`，hello 认证），覆盖任务说明 §8 场景 1-6：
//! 1. 建连与在线；
//! 2. message 帧 → `Platform::recv`；
//! 3. send_text / send_card / update_card → text / card / card_patch 帧；
//! 4. approval 闭环（approval_request → approval_response → recv ask_req+词）；
//! 5. sync 重发；
//! 6. 断线语义（帧排队不 panic、队列满报错、doctor_probes 重连中——同端口
//!    重启有竞态，采用任务说明允许的简化方案，见 `disconnect_*` 注释）。
//!
//! M5 增补：出站帧全部带 machine 字段；浏览器帧 machine 路由（指定本机 /
//! 指定离线 machine_offline）。
//! v1.3 增补：网关凭证默认 key 形态（测试内生成密钥对，relay 的 KEYS 表
//! 绑定其公钥，平台侧持私钥走三帧握手）；token 形态（v1.2 兼容路径）
//! 单独一条回归用例。

use std::net::SocketAddr;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use futures::{SinkExt, StreamExt};
use imagent_core::{CardPhase, CardTerminal, ConvId, OutboundCard, Platform, ReplyHint};
use imagent_relay::{spawn_relay, RelayConfig, RelayHandle};
use imagent_web::{GatewayCredential, WebPlatform};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 测试 token（本地随机端口 + loopback，无真实敏感面；两 token 不同是 relay
/// fail-closed 校验要求）。v1.3 起 token 仅用于兼容回归用例。
const GW_TOKEN: &str = "test-gw-token-0123456789abcdef";
const USER_TOKEN: &str = "test-user-token-0123456789abc";
const CONV: &str = "web:work";
/// 本平台自报的 machine 身份（v1.1 hello + 出站帧携带）。
const MACHINE: &str = "test-mac";

/// 测试内生成 ed25519 密钥对（随机 seed）。
fn gen_signing_key() -> SigningKey {
    SigningKey::from_bytes(&rand::random::<[u8; 32]>())
}

/// 测试用 E2E 密钥路径（临时目录——`WebPlatform::new` 的缺省路径锚定真实
/// `~/.imagent`，测试不得触碰）。
fn e2e_test_key_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "imagent-web-loopback-e2e-{tag}-{}",
        std::process::id()
    ))
}

/// 起 key-only relay（v1.3：KEYS 表绑本机公钥，TOKENS 全空——「只配公钥
/// 表」形态）+ 对应私钥。返回 (handle, 私钥)。
async fn spawn_test_relay() -> (RelayHandle, SigningKey) {
    use imagent_relay::{
        GatewayKeys, GatewayTokens, DEFAULT_PAIRING_TTL_SECS, DEFAULT_SESSION_TTL_SECS,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sk = gen_signing_key();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let handle = spawn_relay(
        listener,
        RelayConfig {
            gateway_tokens: GatewayTokens::default(),
            gateway_keys: GatewayKeys::parse(&format!(
                "{MACHINE}={}",
                hex::encode(sk.verifying_key().to_bytes())
            ))
            .unwrap(),
            user_token: USER_TOKEN.to_string(),
            state_dir: std::env::temp_dir().join(format!(
                "imagent-web-loopback-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            )),
            session_ttl_secs: DEFAULT_SESSION_TTL_SECS,
            pairing_ttl_secs: DEFAULT_PAIRING_TTL_SECS,
            webauthn_rp_id: None,
        },
    )
    .expect("测试配置合法");
    (handle, sk)
}

/// 起真 relay（key 形态）+ 真平台（三帧握手，等在线）+ 模拟浏览器（hello
/// 认证 + machines 快照断言）。E2E 密钥走临时路径（new 的缺省路径锚定真实
/// `~/.imagent`，测试不得触碰）。
async fn setup() -> (RelayHandle, WebPlatform, Ws) {
    let (handle, sk) = spawn_test_relay().await;
    let addr = handle.addr;
    let platform = WebPlatform::new_with_e2e(
        format!("ws://{addr}/gateway"),
        GatewayCredential::Key(sk),
        "default".to_string(),
        MACHINE.to_string(),
        &e2e_test_key_path("platform"),
    );
    // 等 platform 连上 relay（client spawn 后异步建连 + 三帧握手认证）。
    eventually(Duration::from_secs(10), || platform.is_online()).await;
    // 浏览器：hello 认证（浏览器唯一形态是首帧 hello）。
    let mut ws = connect_ws(&addr, "/ws", None).await;
    ws.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{USER_TOKEN}"}}"#
    )))
    .await
    .unwrap();
    let ack = next_text(&mut ws).await;
    assert!(ack.contains(r#""type":"hello_ack""#), "hello_ack：{ack}");
    // v1.1 machines 快照：平台以 machine=test-mac 在线（default 机器不在线，
    // 旧 machine 快照为 false，由 next_of_type 跳过）。
    let machines = next_of_type(&mut ws, "machines").await;
    let list: Vec<&str> = machines["online"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(list, vec![MACHINE], "machines 快照应含本机：{machines}");
    (handle, platform, ws)
}

/// WS 握手（对齐 relay e2e 测试：tungstenite 0.29 自建 Request 须自带头）。
async fn connect_ws(addr: &SocketAddr, path: &str, bearer: Option<&str>) -> Ws {
    let mut builder = Request::builder()
        .uri(format!("ws://{addr}{path}"))
        .header("host", addr.to_string())
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", generate_key());
    if let Some(t) = bearer {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let (ws, _resp) = connect_async(builder.body(()).unwrap())
        .await
        .expect("WS 握手");
    ws
}

/// 读下一条文本帧（跳过协议层 ping/pong；5s 超时防挂死）。
async fn next_text(ws: &mut Ws) -> String {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("等帧超时")
            .expect("流未断")
            .expect("WS 读正常");
        match msg {
            Message::Text(t) => return t.to_string(),
            Message::Ping(_) | Message::Pong(_) => continue,
            m => panic!("意外帧（期待文本）：{m:?}"),
        }
    }
}

/// 读下一条 `type` 匹配的 JSON 帧（跳过 machine / pong 等噪声帧）。
async fn next_of_type(ws: &mut Ws, want: &str) -> serde_json::Value {
    loop {
        let text = next_text(ws).await;
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        if v["type"].as_str() == Some(want) {
            return v;
        }
    }
}

/// 轮询断言：谓词在 timeout 内变 true，否则 panic。
async fn eventually(timeout: Duration, mut pred: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if pred() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "条件在 {timeout:?} 内未满足"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 测试用卡片（Running / 终态两形态）。
fn sample_card(text: &str, terminal: CardTerminal) -> OutboundCard {
    OutboundCard {
        text: text.to_string(),
        tool_calls: vec![],
        thoughts: vec![],
        todos: vec![],
        phase: CardPhase::Outputting,
        queued_hint: None,
        run_secs: 3,
        usage_display: None,
        task_digest: Some("测试任务".into()),
        terminal,
    }
}

fn conv() -> ConvId {
    ConvId(CONV.to_string())
}

/// 场景 1-3：双向通路——message → recv；send_text / send_card / update_card
/// → text / card / card_patch；应用层 ping → pong。
#[tokio::test]
async fn loopback_message_and_outbound_frames() {
    let (handle, p, mut br) = setup().await;

    // 浏览器 message 帧（裸 session 名，兼容形态）→ recv 归一为 web:work。
    br.send(Message::text(
        r#"{"v":1,"type":"message","convId":"work","text":"列出目录"}"#,
    ))
    .await
    .unwrap();
    let m = tokio::time::timeout(Duration::from_secs(5), p.recv())
        .await
        .expect("recv 超时")
        .expect("recv 正常");
    assert_eq!(m.conv_id, conv());
    assert_eq!(m.text.as_deref(), Some("列出目录"));
    assert_eq!(m.sender.0, "web:user");

    // 无 convId → default_session。
    br.send(Message::text(r#"{"v":1,"type":"message","text":"hi"}"#))
        .await
        .unwrap();
    let m = tokio::time::timeout(Duration::from_secs(5), p.recv())
        .await
        .expect("recv 超时")
        .unwrap();
    assert_eq!(m.conv_id, ConvId("web:default".into()));

    // send_text → text 帧（带 machine，v1.1）。
    p.send_text(&conv(), "已收到", &ReplyHint::None)
        .await
        .unwrap();
    let v = next_of_type(&mut br, "text").await;
    assert_eq!(v["machine"], MACHINE, "出站帧带本机 machine");
    assert_eq!(v["convId"], CONV);
    assert_eq!(v["text"], "已收到");

    // message 帧原样回传出站帧 convId（真实 SPA 行为——出站帧 convId 即完整
    // conv）→ 归一后不得产生 web:web: 双前缀。
    let frame_conv = v["convId"].as_str().expect("text 帧带 convId").to_string();
    br.send(Message::text(format!(
        r#"{{"v":1,"type":"message","convId":"{frame_conv}","text":"再问一次"}}"#
    )))
    .await
    .unwrap();
    let m = tokio::time::timeout(Duration::from_secs(5), p.recv())
        .await
        .expect("recv 超时")
        .unwrap();
    assert_eq!(m.conv_id, conv(), "原样回传 convId 不得双前缀");
    assert_eq!(m.text.as_deref(), Some("再问一次"));

    // send_card（Running）→ card 帧，messageId 为合成 id，state=streaming。
    let mid = p
        .send_card(
            &conv(),
            &sample_card("流式中", CardTerminal::Running),
            &ReplyHint::None,
        )
        .await
        .unwrap()
        .expect("send_card 返回合成 messageId");
    assert!(mid.starts_with("web-card-"), "{mid}");
    let v = next_of_type(&mut br, "card").await;
    assert_eq!(v["machine"], MACHINE, "card 帧带本机 machine");
    assert_eq!(v["messageId"], mid);
    assert_eq!(v["state"], "streaming");
    assert_eq!(v["card"]["text"], "流式中");

    // update_card（Done）→ card_patch 帧，state=done。
    p.update_card(
        &conv(),
        &mid,
        &sample_card("完成了", CardTerminal::Done),
        &ReplyHint::None,
    )
    .await
    .unwrap();
    let v = next_of_type(&mut br, "card_patch").await;
    assert_eq!(v["machine"], MACHINE, "card_patch 帧带本机 machine");
    assert_eq!(v["messageId"], mid);
    assert_eq!(v["state"], "done");
    assert_eq!(v["card"]["terminal"], "done");

    // 应用层 ping → client 直接回 pong（不进 recv：下一步 recv 超时证明）。
    br.send(Message::text(r#"{"v":1,"type":"ping"}"#))
        .await
        .unwrap();
    let v = next_of_type(&mut br, "pong").await;
    assert_eq!(v["type"], "pong");
    assert_eq!(v["machine"], MACHINE, "pong 帧带本机 machine");

    handle.shutdown();
    handle.wait().await;
    drop(p);
}

/// 场景 4：审批闭环——approval_request 出站 → 浏览器 approval_response →
/// recv 得 ask_req + 精确词表决策词；resolve/cancel 出站 approval_resolved。
#[tokio::test]
async fn loopback_approval_roundtrip() {
    let (handle, p, mut br) = setup().await;

    // send_permission_ask → approval_request（inputSummary 经 tool_summary 压缩：
    // Bash 取 command 字段）。
    let anchor = p
        .send_permission_ask(
            &conv(),
            "req-1",
            "Bash",
            r#"{"command":"rm -rf /tmp/x"}"#,
            &ReplyHint::None,
        )
        .await
        .unwrap();
    assert_eq!(
        anchor.as_deref(),
        Some("req-1"),
        "request_id 即 card_msg_id 锚点"
    );
    let v = next_of_type(&mut br, "approval_request").await;
    assert_eq!(v["machine"], MACHINE, "approval_request 帧带本机 machine");
    assert_eq!(v["convId"], CONV);
    assert_eq!(v["requestId"], "req-1");
    assert_eq!(v["tool"], "Bash");
    assert_eq!(v["inputSummary"], "rm -rf /tmp/x");

    // 浏览器**原样回传** approval_request 帧的 convId（真实 SPA 行为——帧内
    // convId 即完整 conv `web:work`，不手工剥前缀）→ recv 归一后不得产生
    // web:web: 双前缀（否则 PermissionRouter 按 conv 路由找不到 pending）。
    let frame_conv = v["convId"]
        .as_str()
        .expect("approval_request 带 convId")
        .to_string();
    assert_eq!(frame_conv, CONV);
    br.send(Message::text(format!(
        r#"{{"v":1,"type":"approval_response","convId":"{frame_conv}","requestId":"req-1","decision":"always"}}"#
    )))
    .await
    .unwrap();
    let m = tokio::time::timeout(Duration::from_secs(5), p.recv())
        .await
        .expect("recv 超时")
        .unwrap();
    assert_eq!(m.ask_req.as_deref(), Some("req-1"));
    assert_eq!(m.text.as_deref(), Some("always"));

    // resolve → approval_resolved（decision=always）。
    p.resolve_permission_ask(
        &conv(),
        "req-1",
        &imagent_core::PermissionReply {
            allow: true,
            always: true,
            message: None,
            raw_text: None,
            cancelled: false,
        },
    )
    .await
    .unwrap();
    let v = next_of_type(&mut br, "approval_resolved").await;
    assert_eq!(v["machine"], MACHINE, "approval_resolved 帧带本机 machine");
    assert_eq!(v["requestId"], "req-1");
    assert_eq!(v["decision"], "always");

    // cancel_all：两条 pending 逐条收敛为 cancelled。
    p.send_permission_ask(
        &conv(),
        "req-2",
        "Write",
        r#"{"file_path":"/tmp/a"}"#,
        &ReplyHint::None,
    )
    .await
    .unwrap();
    p.send_permission_ask(
        &conv(),
        "req-3",
        "Read",
        r#"{"file_path":"/tmp/b"}"#,
        &ReplyHint::None,
    )
    .await
    .unwrap();
    p.cancel_all_permission_asks(&conv()).await.unwrap();
    let mut seen = Vec::new();
    for _ in 0..2 {
        let v = next_of_type(&mut br, "approval_resolved").await;
        assert_eq!(v["decision"], "cancelled");
        seen.push(v["requestId"].as_str().unwrap().to_string());
    }
    seen.sort();
    assert_eq!(seen, vec!["req-2".to_string(), "req-3".to_string()]);

    handle.shutdown();
    handle.wait().await;
    drop(p);
}

/// 场景 5：sync 重发——send_card 后浏览器发 sync，重收该 conv 的 card 帧
/// （最新缓存内容与终态 state）。
#[tokio::test]
async fn loopback_sync_resends_cached_card() {
    let (handle, p, mut br) = setup().await;

    let mid = p
        .send_card(
            &conv(),
            &sample_card("进行中", CardTerminal::Running),
            &ReplyHint::None,
        )
        .await
        .unwrap()
        .unwrap();
    let _first = next_of_type(&mut br, "card").await;

    // patch 到终态后 sync：重发的是**最新**缓存（done 状态）。
    p.update_card(
        &conv(),
        &mid,
        &sample_card("最终结果", CardTerminal::Done),
        &ReplyHint::None,
    )
    .await
    .unwrap();
    let _patch = next_of_type(&mut br, "card_patch").await;

    br.send(Message::text(r#"{"v":1,"type":"sync"}"#))
        .await
        .unwrap();
    let v = next_of_type(&mut br, "card").await;
    assert_eq!(v["machine"], MACHINE, "sync 重发帧带本机 machine");
    assert_eq!(v["messageId"], mid, "重发携带原 messageId");
    assert_eq!(v["card"]["text"], "最终结果", "重发最新缓存内容");
    assert_eq!(v["state"], "done");

    // sync 不进 recv（recv 持续 pending）。
    assert!(
        tokio::time::timeout(Duration::from_millis(300), p.recv())
            .await
            .is_err(),
        "sync 不应产生入站消息"
    );

    handle.shutdown();
    handle.wait().await;
    drop(p);
}

/// 场景 6（简化方案）：断线语义——relay 停机后平台在线标志熄灭；断线期
/// send_* 帧排队返回 Ok（不 panic / 不挂死）；队列打满（容量 256）后报
/// 「出站队列满」；doctor_probes 显示重连中。
///
/// 注：同端口重启 relay 验证「重连后恢复」与 shutdown→重新 bind 之间存在
/// 端口复用竞态（任务说明 §8.6 明示简化方案可接受），故此处只验证断线侧
/// 不变量；重连能力由 client run 的退避循环结构与 wecom 同源保证。
#[tokio::test]
async fn disconnect_queues_frames_and_reports_reconnecting() {
    let (handle, p, mut br) = setup().await;
    let _ = &mut br; // 浏览器保持连接（relay 停机会一并断掉）

    handle.shutdown();
    handle.wait().await;

    eventually(Duration::from_secs(10), || !p.is_online()).await;

    // 断线期 send_text 排队 Ok（帧留在出站 channel，client 重连后按序发出）。
    for i in 0..256 {
        p.send_text(&conv(), &format!("排队帧 {i}"), &ReplyHint::None)
            .await
            .unwrap_or_else(|e| panic!("断线期第 {i} 帧应排队 Ok：{e}"));
    }
    // 队列打满（容量 256）→ 明确报错（relay 长时间不可达）。
    let e = p
        .send_text(&conv(), "第 257 帧", &ReplyHint::None)
        .await
        .expect_err("队列满应报错");
    assert!(
        format!("{e}").contains("出站队列满"),
        "错误文案应说明队列满：{e}"
    );

    // recv 在断线期天然 pending（无消息可收，不报错）。
    assert!(
        tokio::time::timeout(Duration::from_millis(300), p.recv())
            .await
            .is_err(),
        "断线期 recv 应 pending"
    );

    // doctor_probes 显示重连中。
    let probes = p.doctor_probes().await;
    assert_eq!(probes.len(), 1);
    assert!(probes[0].contains("重连中"), "{}", probes[0]);

    drop(p);
}

/// 场景 7（M5 多终端 v1.1）：浏览器帧 machine 路由——指定本机（test-mac）
/// 正常送达平台（machine 字段对网关透明，不影响 conv 映射）；指定不在线
/// 机器由 relay 回 machine_offline error（帧不进网关，recv 无感知）。
#[tokio::test]
async fn loopback_browser_machine_field_routing() {
    let (handle, p, mut br) = setup().await;

    // 指定本机 → 正常路由到平台，machine 字段不影响 message 映射。
    br.send(Message::text(format!(
        r#"{{"v":1,"type":"message","machine":"{MACHINE}","convId":"work","text":"指定本机"}}"#
    )))
    .await
    .unwrap();
    let m = tokio::time::timeout(Duration::from_secs(5), p.recv())
        .await
        .expect("recv 超时")
        .expect("recv 正常");
    assert_eq!(m.conv_id, conv());
    assert_eq!(m.text.as_deref(), Some("指定本机"));

    // 指定离线机器 → relay 回 machine_offline（网关侧无感知：recv 持续 pending）。
    br.send(Message::text(
        r#"{"v":1,"type":"message","machine":"other-mac","text":"指定别机"}"#,
    ))
    .await
    .unwrap();
    let e = next_of_type(&mut br, "error").await;
    assert_eq!(e["code"], "machine_offline", "relay 拒绝离线机器：{e}");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), p.recv())
            .await
            .is_err(),
        "machine_offline 帧不应进 recv"
    );

    handle.shutdown();
    handle.wait().await;
    drop(p);
}

/// 场景 8（v1.3 token 兼容路径回归）：凭证为 Token 形态（密钥文件损坏时
/// 的回退路径）时，真 client 走 v1.2 一步 hello（token + machine）照常
/// 认证上线、双向通路可用——key 升级不破坏既有部署。
#[tokio::test]
async fn loopback_token_credential_compat() {
    use imagent_relay::{
        GatewayKeys, GatewayTokens, DEFAULT_PAIRING_TTL_SECS, DEFAULT_SESSION_TTL_SECS,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let handle = spawn_relay(
        listener,
        RelayConfig {
            // tokens-only 形态（KEYS 空）。
            gateway_tokens: GatewayTokens::parse(&format!("{MACHINE}={GW_TOKEN}")).unwrap(),
            gateway_keys: GatewayKeys::default(),
            user_token: USER_TOKEN.to_string(),
            state_dir: std::env::temp_dir().join(format!(
                "imagent-web-loopback-tok-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            )),
            session_ttl_secs: DEFAULT_SESSION_TTL_SECS,
            pairing_ttl_secs: DEFAULT_PAIRING_TTL_SECS,
            webauthn_rp_id: None,
        },
    )
    .expect("测试配置合法");

    let p = WebPlatform::new(
        format!("ws://{}/gateway", handle.addr),
        GatewayCredential::Token(GW_TOKEN.to_string()),
        "default".to_string(),
        MACHINE.to_string(),
    );
    eventually(Duration::from_secs(10), || p.is_online()).await;

    // 双向通路：浏览器 message → recv；send_text → text 帧。
    let addr = handle.addr;
    let mut br = connect_ws(&addr, "/ws", None).await;
    br.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{USER_TOKEN}"}}"#
    )))
    .await
    .unwrap();
    let ack = next_text(&mut br).await;
    assert!(ack.contains(r#""type":"hello_ack""#), "hello_ack：{ack}");
    let _machines = next_of_type(&mut br, "machines").await;

    br.send(Message::text(
        r#"{"v":1,"type":"message","convId":"work","text":"token 形态"}"#,
    ))
    .await
    .unwrap();
    let m = tokio::time::timeout(Duration::from_secs(5), p.recv())
        .await
        .expect("recv 超时")
        .expect("recv 正常");
    assert_eq!(m.conv_id, conv());
    p.send_text(&conv(), "收到", &ReplyHint::None)
        .await
        .unwrap();
    let v = next_of_type(&mut br, "text").await;
    assert_eq!(v["text"], "收到");

    handle.shutdown();
    handle.wait().await;
    drop(p);
}

/// 场景 9（协议 v1.4 E2E 中继盲化全流程）：握手（e2e_hello{sid,ek,n1} →
/// e2e_ack{gk,ik,n2,sig}，浏览器视角以帧内 ik 自洽验签——TOFU 未 pin 路径）
/// → 会话密钥两侧
/// 一致 → 双向业务帧 enc 密文（message 入站试解、text/pong 出站逐 sid 定
/// 向）→ 握手前明文兼容路径回归（无会话时行为不变）。
#[tokio::test]
async fn loopback_e2e_encrypted_roundtrip() {
    use base64::Engine as _;
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};
    use ed25519_dalek::Verifier;
    use x25519_dalek::{PublicKey as XPublicKey, StaticSecret as XStaticSecret};

    /// 浏览器侧单帧密封（与 crates/web/src/e2e.rs seal 同构：nonce24‖ct‖tag）。
    fn seal(key: &[u8; 32], plaintext: &str) -> String {
        let nonce = rand::random::<[u8; 24]>();
        let cipher = XChaCha20Poly1305::new_from_slice(key).unwrap();
        let ct = cipher
            .encrypt(XNonce::from_slice(&nonce), plaintext.as_bytes())
            .unwrap();
        let mut boxed = Vec::with_capacity(24 + ct.len());
        boxed.extend_from_slice(&nonce);
        boxed.extend_from_slice(&ct);
        base64::engine::general_purpose::STANDARD.encode(boxed)
    }
    /// 浏览器侧单帧开启。
    fn open(key: &[u8; 32], boxed: &str) -> String {
        let data = base64::engine::general_purpose::STANDARD
            .decode(boxed)
            .unwrap();
        let (nonce, ct) = data.split_at(24);
        let cipher = XChaCha20Poly1305::new_from_slice(key).unwrap();
        let pt = cipher.decrypt(XNonce::from_slice(nonce), ct).unwrap();
        String::from_utf8(pt).unwrap()
    }

    let (handle, sk) = spawn_test_relay().await;
    let addr = handle.addr;
    let key_path = e2e_test_key_path("e2e-flow");
    let p = WebPlatform::new_with_e2e(
        format!("ws://{addr}/gateway"),
        GatewayCredential::Key(sk.clone()),
        "default".to_string(),
        MACHINE.to_string(),
        &key_path,
    );
    eventually(Duration::from_secs(10), || p.is_online()).await;

    // 浏览器登录：session id 即 enc 帧定向路由键。
    let mut br = connect_ws(&addr, "/ws", None).await;
    br.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{USER_TOKEN}"}}"#
    )))
    .await
    .unwrap();
    let ack: serde_json::Value = serde_json::from_str(&next_text(&mut br).await).unwrap();
    let sid = ack["session"]["id"].as_str().unwrap().to_string();
    let _machines = next_of_type(&mut br, "machines").await;

    // ---- 握手前：明文兼容路径回归（无 E2E 会话 → 出站行为不变）----
    p.send_text(&conv(), "握手前明文", &ReplyHint::None)
        .await
        .unwrap();
    let v = next_of_type(&mut br, "text").await;
    assert_eq!(v["text"], "握手前明文", "握手前出站应明文：{v}");

    // ---- E2E 握手（浏览器视角：临时密钥 + 验签 + 派生）----
    let browser_secret = XStaticSecret::from(rand::random::<[u8; 32]>());
    let browser_public = XPublicKey::from(&browser_secret);
    let n1 = rand::random::<[u8; 32]>();
    br.send(Message::text(
        serde_json::json!({
            "v": 1, "type": "e2e_hello", "machine": MACHINE, "sid": sid,
            "ek": hex::encode(browser_public.as_bytes()),
            "n1": hex::encode(n1),
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let ack = next_of_type(&mut br, "e2e_ack").await;
    assert_eq!(ack["machine"], MACHINE, "{ack}");
    // ik：TOFU pin 身份公钥来源，须随 ack 携带且与网关 v1.3 认证公钥一致
    //（SPA 首连以 ik 自洽验签 + 弹指纹确认层，pin 表存 ik/gk）。
    assert_eq!(
        ack["ik"],
        hex::encode(sk.verifying_key().to_bytes()),
        "e2e_ack 须携带与认证公钥一致的 ik：{ack}"
    );
    let gk = hex::decode(ack["gk"].as_str().unwrap()).unwrap();
    let gk_arr: [u8; 32] = gk.clone().try_into().unwrap();
    let n2 = hex::decode(ack["n2"].as_str().unwrap()).unwrap();
    let n2_arr: [u8; 32] = n2.clone().try_into().unwrap();
    // 浏览器用 pin 的网关 ed25519 公钥验签（对象 = ek||gk||n1||n2 原始字节）。
    let mut signed = Vec::with_capacity(128);
    signed.extend_from_slice(browser_public.as_bytes());
    signed.extend_from_slice(&gk);
    signed.extend_from_slice(&n1);
    signed.extend_from_slice(&n2);
    let sig = <[u8; 64]>::try_from(hex::decode(ack["sig"].as_str().unwrap()).unwrap()).unwrap();
    sk.verifying_key()
        .verify(&signed, &ed25519_dalek::Signature::from_bytes(&sig))
        .expect("e2e_ack 签名须可用 pin 的网关公钥验证");
    // 浏览器侧派生（X25519 对称性 → 与网关一致）。
    let k = imagent_web::e2e::derive_session_key(
        browser_secret
            .diffie_hellman(&XPublicKey::from(gk_arr))
            .as_bytes(),
        &n1,
        &n2_arr,
    );

    // ---- 入站：message 包 enc（无 sid）→ 网关试解 → recv ----
    br.send(Message::text(
        serde_json::json!({
            "v": 1, "type": "enc", "machine": MACHINE,
            "box": seal(&k, r#"{"v":1,"type":"message","convId":"work","text":"密文消息"}"#),
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let m = tokio::time::timeout(Duration::from_secs(5), p.recv())
        .await
        .expect("recv 超时")
        .expect("recv 正常");
    assert_eq!(m.conv_id, conv());
    assert_eq!(m.text.as_deref(), Some("密文消息"));

    // ---- 出站：send_text → enc{sid} 定向帧，浏览器解密得到原 text 帧 ----
    p.send_text(&conv(), "机密回复", &ReplyHint::None)
        .await
        .unwrap();
    let v = next_of_type(&mut br, "enc").await;
    assert_eq!(v["v"], 1);
    assert_eq!(v["machine"], MACHINE);
    assert_eq!(
        v["sid"], sid,
        "出站 enc 帧携带握手 sid（relay 定向投递键）：{v}"
    );
    let inner: serde_json::Value = serde_json::from_str(&open(&k, v["box"].as_str().unwrap()))
        .expect("浏览器侧会话密钥须能解网关 box");
    assert_eq!(inner["type"], "text");
    assert_eq!(inner["text"], "机密回复");
    assert_eq!(inner["convId"], CONV);

    // ---- 应用层 ping 同样走 enc：enc-wrapped ping → enc-wrapped pong ----
    br.send(Message::text(
        serde_json::json!({
            "v": 1, "type": "enc", "machine": MACHINE,
            "box": seal(&k, r#"{"v":1,"type":"ping"}"#),
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let v = next_of_type(&mut br, "enc").await;
    let inner: serde_json::Value =
        serde_json::from_str(&open(&k, v["box"].as_str().unwrap())).expect("pong 也应逐会话加密");
    assert_eq!(inner["type"], "pong");
    assert_eq!(inner["machine"], MACHINE);
    // pong 不进 recv。
    assert!(
        tokio::time::timeout(Duration::from_millis(300), p.recv())
            .await
            .is_err(),
        "enc ping 不应产生入站消息"
    );

    let _ = std::fs::remove_file(&key_path);
    handle.shutdown();
    handle.wait().await;
    drop(p);
}
