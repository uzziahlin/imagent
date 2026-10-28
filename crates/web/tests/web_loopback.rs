//! 回环集成测试（M2 核心验收 + M5 多终端 v1.1）：真 relay
//!（`imagent_relay::spawn_relay` 随机端口）+ 真 `WebPlatform`（出站连
//! `/gateway`，hello 自报 machine=test-mac）+ tokio-tungstenite 模拟浏览器
//!（连 `/ws`，hello 认证），覆盖任务说明 §8 场景 1-6：
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

use std::net::SocketAddr;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use imagent_core::{CardPhase, CardTerminal, ConvId, OutboundCard, Platform, ReplyHint};
use imagent_relay::{spawn_relay, RelayConfig, RelayHandle};
use imagent_web::WebPlatform;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 测试 token（本地随机端口 + loopback，无真实敏感面；两 token 不同是 relay
/// fail-closed 校验要求）。
const GW_TOKEN: &str = "test-gw-token-0123456789abcdef";
const USER_TOKEN: &str = "test-user-token-0123456789abc";
const CONV: &str = "web:work";
/// 本平台自报的 machine 身份（v1.1 hello + 出站帧携带）。
const MACHINE: &str = "test-mac";

async fn spawn_test_relay() -> RelayHandle {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    spawn_relay(
        listener,
        RelayConfig {
            gateway_token: GW_TOKEN.to_string(),
            user_token: USER_TOKEN.to_string(),
        },
    )
    .expect("测试配置合法")
}

/// 起真 relay + 真平台（hello 自报 machine，等在线）+ 模拟浏览器（hello
/// 认证 + machines 快照断言）。
async fn setup() -> (RelayHandle, WebPlatform, Ws) {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;
    let platform = WebPlatform::new(
        format!("ws://{addr}/gateway"),
        GW_TOKEN.to_string(),
        "default".to_string(),
        MACHINE.to_string(),
    );
    // 等 platform 连上 relay（client spawn 后异步建连 + hello 认证）。
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
