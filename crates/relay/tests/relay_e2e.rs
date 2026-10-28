//! relay 集成测试：真 server（lib `spawn_relay`，绑 127.0.0.1:0 随机端口）
//! 加 tokio-tungstenite 客户端，覆盖任务说明 §5 场景与多端并存改造用例
//!（多 browser 共存广播 / 并存上限 4001 / 单条退出其余存活），以及 v1.1
//! 多终端用例（多机器并存 / machine 路由 / machines 帧广播 / 同机踢旧 /
//! 机器上限 / Bearer=default 回退 / 缺省歧义的 bootstrap sync/ping 静默
//! 豁免）与 v1.2 会话层用例（登录换发 / sessionToken 重连 / 过期与吊销
//! reason 区分 / 设备列表与吊销断连 / 会话落盘重启保留 / 会话表上限 /
//! 网关 token-machine 绑定）。close code 断言依赖 tungstenite 把 Close 帧
//! 投递给应用层，可精确断言，无需退化。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use imagent_relay::{
    spawn_relay, GatewayTokens, RelayConfig, RelayHandle, DEFAULT_SESSION_TTL_SECS,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 测试 token（本地随机端口 + loopback，无真实敏感面）。v1.2 起网关 token
/// 按 machine 绑定：测试表的每台机器一个独立 token（见 [`gw_token`]）。
const USER_TOKEN: &str = "test-user-token-0123456789abcdef00";
/// 测试绑定表覆盖的 machine（既有用例用到的全部身份）。
const TEST_MACHINES: [&str; 9] = [
    "default", "home", "work", "other", "m1", "m2", "m3", "m4", "m5",
];

/// 指定 machine 的网关 token。
fn gw_token(machine: &str) -> String {
    format!("test-gateway-token-{machine}")
}

fn test_gateway_tokens() -> GatewayTokens {
    let spec = TEST_MACHINES
        .iter()
        .map(|m| format!("{m}={}", gw_token(m)))
        .collect::<Vec<_>>()
        .join(",");
    GatewayTokens::parse(&spec).expect("测试绑定表合法")
}

/// 每个测试 relay 独立的会话状态目录（临时目录，不跨用例共享会话）。
fn temp_state_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "imagent-relay-e2e-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn test_config(state_dir: &std::path::Path, session_ttl_secs: u64) -> RelayConfig {
    RelayConfig {
        gateway_tokens: test_gateway_tokens(),
        user_token: USER_TOKEN.to_string(),
        state_dir: state_dir.to_path_buf(),
        session_ttl_secs,
    }
}

async fn spawn_with_config(config: RelayConfig) -> RelayHandle {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    spawn_relay(listener, config).expect("测试配置合法")
}

async fn spawn_test_relay() -> RelayHandle {
    spawn_with_config(test_config(&temp_state_dir(), DEFAULT_SESSION_TTL_SECS)).await
}

/// WS 握手：path + 可选 Bearer 头。tungstenite 0.29 对自建 Request 不再
/// 自动补握手头（Host/Upgrade/Key 等须自带），用 `generate_key` 生成。
async fn connect_ws(addr: &SocketAddr, path: &str, bearer: Option<&str>) -> Ws {
    use tokio_tungstenite::tungstenite::handshake::client::generate_key;
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
    let req = builder.body(()).unwrap();
    let (ws, _resp) = connect_async(req).await.expect("WS 握手");
    ws
}

/// hello 首帧握手（浏览器侧）：发 hello（无 machine 维度）、断言 hello_ack。
/// v1.2 兼容形态：token 登录（每次换发新会话）；session 形态见
/// [`resume_session`]。
async fn hello_handshake(ws: &mut Ws, token: &str) {
    ws.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{token}"}}"#
    )))
    .await
    .unwrap();
    let ack = next_text(ws).await;
    assert!(ack.contains(r#""type":"hello_ack""#), "hello_ack: {ack}");
}

/// v1.2 登录换发：hello 带 USER_TOKEN（可选 device）→ 断言 hello_ack 携带
/// session{id, token, expiresAt}，返回 session 对象。
async fn login_session(ws: &mut Ws, device: Option<&str>) -> serde_json::Value {
    let device_json = match device {
        Some(d) => format!(r#","device":"{d}""#),
        None => String::new(),
    };
    ws.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{USER_TOKEN}"{device_json}}}"#
    )))
    .await
    .unwrap();
    let ack = next_text(ws).await;
    let v: serde_json::Value = serde_json::from_str(&ack).unwrap();
    assert_eq!(v["type"], "hello_ack", "{ack}");
    assert_eq!(v["role"], "browser", "{ack}");
    let session = &v["session"];
    assert!(
        session.is_object(),
        "登录 hello_ack 应携带 session 载荷：{ack}"
    );
    assert!(
        session["id"].as_str().is_some_and(|s| s.len() == 36),
        "session.id 应为 UUID：{ack}"
    );
    assert!(
        session["token"].as_str().is_some_and(|t| t.len() == 64),
        "session.token 应为 32B hex：{ack}"
    );
    assert!(
        session["expiresAt"].as_u64().is_some(),
        "session.expiresAt 应为 unix 秒：{ack}"
    );
    v["session"].clone()
}

/// v1.2 会话重连：hello 带 sessionToken → 断言 hello_ack（含剩余
/// expiresAt）。
async fn resume_session(ws: &mut Ws, session_token: &str) -> serde_json::Value {
    ws.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","session":"{session_token}"}}"#
    )))
    .await
    .unwrap();
    let ack = next_text(ws).await;
    let v: serde_json::Value = serde_json::from_str(&ack).unwrap();
    assert_eq!(v["type"], "hello_ack", "{ack}");
    assert_eq!(
        v["session"]["token"], session_token,
        "重连 ack 回显 token：{ack}"
    );
    assert!(
        v["session"]["expiresAt"].as_u64().is_some(),
        "重连 ack 含剩余 expiresAt：{ack}"
    );
    v
}

/// 网关 hello 握手（v1.1 自报 machine + v1.2 token-machine 绑定）：发
/// hello（该 machine 绑定的 token + machine），断言 hello_ack 回帧回显
/// 相同 machine（身份确认）。
async fn gateway_hello(ws: &mut Ws, machine: &str) {
    ws.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{}","machine":"{machine}"}}"#,
        gw_token(machine)
    )))
    .await
    .unwrap();
    let ack = next_text(ws).await;
    assert!(
        ack.contains(r#""type":"hello_ack""#) && ack.contains(&format!(r#""machine":"{machine}""#)),
        "hello_ack 应回显 machine：{ack}"
    );
}

/// 读下一条文本帧（跳过 ping/pong；5s 超时防挂死）。
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

/// 读下一条 `type` 匹配的 JSON 帧（跳过 machine/machines/pong 等噪声帧）。
async fn next_of_type(ws: &mut Ws, want: &str) -> serde_json::Value {
    loop {
        let text = next_text(ws).await;
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        if v["type"].as_str() == Some(want) {
            return v;
        }
    }
}

/// 断言 machines 帧的全量在线列表（排序不敏感）。
fn assert_machines_list(v: &serde_json::Value, want: &[&str]) {
    let mut got: Vec<String> = v["online"]
        .as_array()
        .unwrap_or_else(|| panic!("machines 帧应有 online 数组：{v}"))
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    let mut want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
    got.sort();
    want.sort();
    assert_eq!(got, want, "machines 列表：{v}");
}

/// 浏览器认证后消费 v1.1 双快照：旧 machine 帧（default 机器在线状态，
/// 兼容 v1 帧序）+ machines 全量列表帧；断言两帧内容。
async fn consume_browser_snapshots(ws: &mut Ws, default_online: bool, machines: &[&str]) {
    let m = next_of_type(ws, "machine").await;
    assert_eq!(
        m["online"], default_online,
        "旧 machine 快照（default 机器）：{m}"
    );
    let v = next_of_type(ws, "machines").await;
    assert_machines_list(&v, machines);
}

/// 网关上/下线事件消费：machines 全量列表帧（default 机器上下线时前面还
/// 有旧 machine 帧——next_of_type 跳过即可）。断言列表内容。
async fn consume_machines_event(ws: &mut Ws, machines: &[&str]) {
    let v = next_of_type(ws, "machines").await;
    assert_machines_list(&v, machines);
}

/// 读 close（容忍其间夹杂文本帧），断言 code 并返回 CloseFrame（供 reason 断言）。
async fn expect_close(ws: &mut Ws, code: u16) -> CloseFrame {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("等 close 超时")
            .expect("流未断")
            .expect("WS 读正常");
        match msg {
            Message::Close(Some(f)) => {
                assert_eq!(u16::from(f.code), code, "close code（reason={}）", f.reason);
                return f;
            }
            Message::Close(None) => panic!("无 code 的 close（期望 {code}）"),
            Message::Text(_) | Message::Ping(_) | Message::Pong(_) => continue,
            m => panic!("意外帧（期待 close）：{m:?}"),
        }
    }
}

/// 断言静默：短窗口内无任何帧到达（用于「应被丢弃不回帧」判定；服务端对
/// 入站帧的处理与回帧是同连接任务内同步完成的，窗口只是防误判的保守等待
/// ——若真要回帧，远早于窗口结束）。连接中断（None/Err）视为失败。
async fn assert_no_frame(ws: &mut Ws, ctx: &str) {
    tokio::time::sleep(Duration::from_millis(200)).await;
    match tokio::time::timeout(Duration::from_millis(200), ws.next()).await {
        Err(_) => {} // 窗口内无帧——静默成立
        Ok(Some(Ok(m))) => panic!("{ctx}：不应收到帧，却收到 {m:?}"),
        Ok(None) => panic!("{ctx}：连接不应断开"),
        Ok(Some(Err(e))) => panic!("{ctx}：读失败 {e:?}"),
    }
}

/// 裸 HTTP GET（loopback 直连不走代理，对齐 webhook 测试注释），返回完整
/// 原始响应（状态行 + 头 + 体）——安全响应头断言需要读全部头，不只
/// content-type。
async fn http_get_raw(addr: &SocketAddr, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).to_string()
}

/// 裸 HTTP GET 解析版（`http_get_raw` 便捷封装），返回
/// (状态码字符串, content-type, body)。
async fn http_get(addr: &SocketAddr, path: &str) -> (String, String, String) {
    let raw = http_get_raw(addr, path).await;
    let (headers, body) = raw.split_once("\r\n\r\n").unwrap_or(("", ""));
    let status = headers
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    let ctype = headers
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-type")
                .then(|| v.trim().to_string())
        })
        .unwrap_or_default();
    (status, ctype, body.to_string())
}

/// 响应头查找（大小写不敏感的 name 匹配，取首个命中值）。
fn find_header(headers: &str, name: &str) -> Option<String> {
    headers.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().to_string())
    })
}

/// 场景 1：双端互通——browser↔gateway 文本帧原样透传（不解析不改写）。
#[tokio::test]
async fn e2e_bidirectional_passthrough() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut gw = connect_ws(&addr, "/gateway", Some(&gw_token("default"))).await;
    let ack = next_text(&mut gw).await;
    assert!(
        ack.contains(r#""role":"gateway""#) && ack.contains(r#""machine":"default""#),
        "Bearer 直通 = default 机器，hello_ack 回显：{ack}"
    );

    let mut br = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut br, USER_TOKEN).await;
    // 浏览器认证后立即收到双快照（default 机器在线 + machines 全量列表）。
    consume_browser_snapshots(&mut br, true, &["default"]).await;

    let m1 = r#"{"v":1,"type":"message","convId":"web:default","text":"列出目录"}"#;
    br.send(Message::text(m1)).await.unwrap();
    assert_eq!(next_text(&mut gw).await, m1, "browser → gateway 原样透传");

    let m2 = r#"{"v":1,"type":"text","convId":"web:default","text":"done"}"#;
    gw.send(Message::text(m2)).await.unwrap();
    assert_eq!(next_text(&mut br).await, m2, "gateway → browser 原样透传");

    handle.shutdown();
    handle.wait().await;
}

/// 场景 2：认证失败——错 token hello、非 hello 首帧，均 close 1008
///（服务端 1s 防爆破延迟后再关，超时预算放大到 5s）。v1.1：网关 hello
/// machine 非法字符同样 1008（machine 是路由主键，脏值拒绝）。
#[tokio::test]
async fn e2e_auth_failures_close_1008() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut br = connect_ws(&addr, "/ws", None).await;
    br.send(Message::text(
        r#"{"v":1,"type":"hello","token":"wrong-token"}"#,
    ))
    .await
    .unwrap();
    expect_close(&mut br, 1008).await;

    // 首帧非 hello（type=message）。
    let mut br2 = connect_ws(&addr, "/ws", None).await;
    br2.send(Message::text(r#"{"v":1,"type":"message","text":"hi"}"#))
        .await
        .unwrap();
    expect_close(&mut br2, 1008).await;

    // v1.1：machine 含非法字符（空格/感叹号）。
    let mut gw = connect_ws(&addr, "/gateway", None).await;
    gw.send(Message::text(
        r#"{"v":1,"type":"hello","token":"test-gateway-token-0123456789abcdef","machine":"bad machine!"}"#,
    ))
    .await
    .unwrap();
    expect_close(&mut gw, 1008).await;

    handle.shutdown();
    handle.wait().await;
}

/// 场景 3：Authorization 头直通（v1.1 回退兼容形态，视为 default 机器）——
/// 正确头跳过 hello 直接收到 hello_ack（回显 machine=default）；错误头走
/// 失败路径 1008。
#[tokio::test]
async fn e2e_bearer_header_direct_auth() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut gw = connect_ws(&addr, "/gateway", Some(&gw_token("default"))).await;
    let ack = next_text(&mut gw).await;
    assert!(
        ack.contains(r#""type":"hello_ack""#) && ack.contains(r#""role":"gateway""#),
        "{ack}"
    );
    assert!(
        ack.contains(r#""machine":"default""#),
        "Bearer 无 machine 维度 → default，hello_ack 回显：{ack}"
    );

    let mut bad = connect_ws(&addr, "/gateway", Some("wrong-token")).await;
    expect_close(&mut bad, 1008).await;

    handle.shutdown();
    handle.wait().await;
}

/// 场景 4：缓冲重放——网关未连时 browser 发 3 帧（未溢出，无 gateway_offline
/// error），网关连上后按 FIFO 序收到全部 3 帧；browser 收到 machine online:true。
#[tokio::test]
async fn e2e_offline_buffer_replay_in_order() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut br = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut br, USER_TOKEN).await;
    consume_browser_snapshots(&mut br, false, &[]).await;

    let frames = [
        r#"{"v":1,"type":"message","text":"1"}"#,
        r#"{"v":1,"type":"message","text":"2"}"#,
        r#"{"v":1,"type":"message","text":"3"}"#,
    ];
    for f in frames {
        br.send(Message::text(f)).await.unwrap();
    }
    // 等服务端把 3 帧吃进缓冲再连网关（确保走的是缓冲重放路径）。
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut gw = connect_ws(&addr, "/gateway", Some(&gw_token("default"))).await;
    let ack = next_text(&mut gw).await;
    assert!(ack.contains(r#""type":"hello_ack""#), "{ack}");
    for f in frames {
        assert_eq!(next_text(&mut gw).await, f, "重放保序（FIFO）");
    }
    let m = next_text(&mut br).await;
    assert!(m.contains(r#""online":true"#), "网关上线通知：{m}");

    handle.shutdown();
    handle.wait().await;
}

/// 场景 5：machine 通知——网关连接/断开时 browser 收到 online true/false
///（v1.1 语义收敛为 default 机器；另见 machines 帧）。
#[tokio::test]
async fn e2e_machine_online_offline_notify() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut br = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut br, USER_TOKEN).await;
    consume_browser_snapshots(&mut br, false, &[]).await;

    let mut gw = connect_ws(&addr, "/gateway", Some(&gw_token("default"))).await;
    let _ = next_text(&mut gw).await; // hello_ack
    let m = next_of_type(&mut br, "machine").await;
    assert_eq!(m["online"], true, "网关上线：{m}");
    consume_machines_event(&mut br, &["default"]).await;

    // 网关干净断开（发 close 帧）→ 服务端清槽位 → browser 收 offline。
    gw.close(None).await.unwrap();
    let m = next_of_type(&mut br, "machine").await;
    assert_eq!(m["online"], false, "网关离线：{m}");
    consume_machines_event(&mut br, &[]).await;

    handle.shutdown();
    handle.wait().await;
}

/// 场景 6：踢旧——同 machine（Bearer×2 = default）第二条网关连接认证后，
/// 第一条收到 close 4001（reason superseded），槽位归新连接。
#[tokio::test]
async fn e2e_supersede_kicks_old_gateway_4001() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut g1 = connect_ws(&addr, "/gateway", Some(&gw_token("default"))).await;
    let _ = next_text(&mut g1).await; // hello_ack

    let mut g2 = connect_ws(&addr, "/gateway", Some(&gw_token("default"))).await;
    let ack = next_text(&mut g2).await;
    assert!(ack.contains(r#""type":"hello_ack""#), "新连接正常：{ack}");

    let frame = expect_close(&mut g1, 4001).await;
    assert!(
        frame.reason.to_string().contains("superseded"),
        "reason={}",
        frame.reason
    );

    // 踢旧后新连接仍可工作：browser 上线 + 透传一帧。
    let mut br = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut br, USER_TOKEN).await;
    consume_browser_snapshots(&mut br, true, &["default"]).await;
    br.send(Message::text(r#"{"v":1,"type":"ping"}"#))
        .await
        .unwrap();
    assert_eq!(next_text(&mut g2).await, r#"{"v":1,"type":"ping"}"#);

    handle.shutdown();
    handle.wait().await;
}

/// 场景 11（多端并存改造）：两条 browser 连接共存（取代原「新 browser
/// 踢旧 4001」语义——真机常态是手机 + 电脑 + 多标签同时在线）：各自发帧
/// gateway 都收到；gateway 上/下线 machine 帧与业务帧两条都收到（广播）。
#[tokio::test]
async fn e2e_two_browsers_coexist_and_broadcast() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    // 两条 browser 先在线（各自快照 false + machines 空，第一条不被踢）。
    let mut b1 = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut b1, USER_TOKEN).await;
    consume_browser_snapshots(&mut b1, false, &[]).await;
    let mut b2 = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut b2, USER_TOKEN).await;
    consume_browser_snapshots(&mut b2, false, &[]).await;

    // 网关上线：machine online 广播，两条都收到。
    let mut gw = connect_ws(&addr, "/gateway", Some(&gw_token("default"))).await;
    let _ = next_text(&mut gw).await; // hello_ack
    for (name, ws) in [("b1", &mut b1), ("b2", &mut b2)] {
        let m = next_of_type(ws, "machine").await;
        assert_eq!(m["online"], true, "{name} 收上线广播：{m}");
        consume_machines_event(ws, &["default"]).await;
    }

    // 两条 browser 各自发帧 → gateway 都收到（入站语义不变）。
    let m1 = r#"{"v":1,"type":"message","from":"b1"}"#;
    b1.send(Message::text(m1)).await.unwrap();
    assert_eq!(next_text(&mut gw).await, m1, "b1 → gateway");
    let m2 = r#"{"v":1,"type":"message","from":"b2"}"#;
    b2.send(Message::text(m2)).await.unwrap();
    assert_eq!(next_text(&mut gw).await, m2, "b2 → gateway");

    // gateway 发一帧 → 两条 browser 都收到（广播）。
    let g = r#"{"v":1,"type":"text","text":"hi-all"}"#;
    gw.send(Message::text(g)).await.unwrap();
    assert_eq!(next_text(&mut b1).await, g, "b1 收广播");
    assert_eq!(next_text(&mut b2).await, g, "b2 收广播");

    // 网关干净断开 → machine offline 广播，两条都收到。
    gw.close(None).await.unwrap();
    for (name, ws) in [("b1", &mut b1), ("b2", &mut b2)] {
        let m = next_of_type(ws, "machine").await;
        assert_eq!(m["online"], false, "{name} 收下线广播：{m}");
        consume_machines_event(ws, &[]).await;
    }

    handle.shutdown();
    handle.wait().await;
}

/// 场景 12（多端并存改造）：并存上限——8 条（relay `MAX_BROWSERS`）注册
/// 成功后，第 9 条认证通过仍被 close 4001（满员让位，复用 superseded
/// 语义，SPA 已有终态处理）；在线 8 条收广播不受影响。
#[tokio::test]
async fn e2e_browser_cap_9th_close_4001() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut gw = connect_ws(&addr, "/gateway", Some(&gw_token("default"))).await;
    let _ = next_text(&mut gw).await; // hello_ack

    let mut browsers = Vec::new();
    for _ in 0..8 {
        let mut ws = connect_ws(&addr, "/ws", None).await;
        hello_handshake(&mut ws, USER_TOKEN).await;
        consume_browser_snapshots(&mut ws, true, &["default"]).await;
        browsers.push(ws);
    }

    // 第 9 条：认证通过 → 未注册 → close 4001（reason 表满员而非踢旧）。
    let mut b9 = connect_ws(&addr, "/ws", None).await;
    b9.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{USER_TOKEN}"}}"#
    )))
    .await
    .unwrap();
    let frame = expect_close(&mut b9, 4001).await;
    assert!(
        frame.reason.to_string().contains("browsers"),
        "reason={}",
        frame.reason
    );

    // 满员拒绝不影响在线 8 条：gateway 广播一帧全部收到。
    let g = r#"{"v":1,"type":"text","text":"fanout"}"#;
    gw.send(Message::text(g)).await.unwrap();
    for (i, ws) in browsers.iter_mut().enumerate() {
        assert_eq!(next_text(ws).await, g, "第 {} 条应照常收广播", i + 1);
    }

    handle.shutdown();
    handle.wait().await;
}

/// 场景 13（多端并存改造）：单条 browser 断开只清自己——其余照常收
/// gateway 业务广播与 machine 帧（下线不误伤并存连接）。
#[tokio::test]
async fn e2e_browser_disconnect_others_keep_receiving() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut b1 = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut b1, USER_TOKEN).await;
    consume_browser_snapshots(&mut b1, false, &[]).await;
    let mut b2 = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut b2, USER_TOKEN).await;
    consume_browser_snapshots(&mut b2, false, &[]).await;

    // b1 干净断开 → 服务端只清 b1 自己的条目。
    b1.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await; // 等服务端 release

    // 网关上线：machine online 广播，b2（仍在线）收到。
    let mut gw = connect_ws(&addr, "/gateway", Some(&gw_token("default"))).await;
    let _ = next_text(&mut gw).await;
    let m = next_of_type(&mut b2, "machine").await;
    assert_eq!(m["online"], true, "b2 收上线广播：{m}");
    consume_machines_event(&mut b2, &["default"]).await;

    // gateway 业务帧：b2 仍正常收（b1 退出未影响广播目标）。
    let g = r#"{"v":1,"type":"text","text":"after-leave"}"#;
    gw.send(Message::text(g)).await.unwrap();
    assert_eq!(next_text(&mut b2).await, g);

    // 网关断开：machine offline 广播，b2 收到。
    gw.close(None).await.unwrap();
    let m = next_of_type(&mut b2, "machine").await;
    assert_eq!(m["online"], false, "b2 收下线广播：{m}");

    handle.shutdown();
    handle.wait().await;
}

/// 场景 14（v1.1 多终端）：多机器并存 + machine 路由全分支——hello 自报
/// machine 身份、machines 帧广播时机（上/下线）、显式指定命中 / 指定离线
/// machine_offline / 缺省多台 ambiguous_machine / 缺省单台回退路由、
/// 非 default 机器上下线不发旧 machine 帧。
#[tokio::test]
async fn e2e_multi_machine_routing() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut br = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut br, USER_TOKEN).await;
    consume_browser_snapshots(&mut br, false, &[]).await;

    // home 上线：browser 直接收 machines 帧（非 default 机器 → 不发旧
    // machine online 帧，next_text 首条即 machines，含此断言语义）。
    let mut gw_home = connect_ws(&addr, "/gateway", None).await;
    gateway_hello(&mut gw_home, "home").await;
    let v = next_of_type(&mut br, "machines").await;
    assert_machines_list(&v, &["home"]);

    // work 上线：machines 全量更新（home + work）。
    let mut gw_work = connect_ws(&addr, "/gateway", None).await;
    gateway_hello(&mut gw_work, "work").await;
    consume_machines_event(&mut br, &["home", "work"]).await;

    // 显式指定且在线 → 路由到对应网关（原样透传）。
    let to_home = r#"{"v":1,"type":"message","machine":"home","text":"给家里"}"#;
    br.send(Message::text(to_home)).await.unwrap();
    assert_eq!(
        next_text(&mut gw_home).await,
        to_home,
        "machine=home → home 网关"
    );

    let to_work = r#"{"v":1,"type":"ping","machine":"work"}"#;
    br.send(Message::text(to_work)).await.unwrap();
    assert_eq!(
        next_text(&mut gw_work).await,
        to_work,
        "machine=work → work 网关"
    );

    // 显式指定但不在线 → 回 machine_offline error。
    br.send(Message::text(
        r#"{"v":1,"type":"message","machine":"ghost","text":"x"}"#,
    ))
    .await
    .unwrap();
    let e = next_of_type(&mut br, "error").await;
    assert_eq!(e["code"], "machine_offline", "指定离线机器：{e}");

    // 未指定 + 两台在线 → ambiguous_machine error。
    br.send(Message::text(r#"{"v":1,"type":"message","text":"给谁？"}"#))
        .await
        .unwrap();
    let e = next_of_type(&mut br, "error").await;
    assert_eq!(e["code"], "ambiguous_machine", "缺省 + 多台：{e}");

    // home 下线：非 default 机器 → 只有 machines 帧（无旧 machine offline
    // 帧），列表剩 work。
    gw_home.close(None).await.unwrap();
    consume_machines_event(&mut br, &["work"]).await;

    // 未指定 + 仅剩一台 → 路由它（单机部署 SPA 零升级无感）。
    br.send(Message::text(
        r#"{"v":1,"type":"message","text":"只剩一台"}"#,
    ))
    .await
    .unwrap();
    let routed = next_text(&mut gw_work).await;
    assert_eq!(
        routed, r#"{"v":1,"type":"message","text":"只剩一台"}"#,
        "缺省 + 单台 → 唯一在线机器"
    );

    handle.shutdown();
    handle.wait().await;
}

/// 场景 15（v1.1 多终端）：同 machine 重连踢旧——hello machine=home 的第二
/// 条连接认证后，第一条 close 4001 superseded；新连接接管该 machine 槽位
///（每台机器单连接语义）。
#[tokio::test]
async fn e2e_same_machine_reconnect_kicks_old() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut g1 = connect_ws(&addr, "/gateway", None).await;
    gateway_hello(&mut g1, "home").await;

    let mut g2 = connect_ws(&addr, "/gateway", None).await;
    gateway_hello(&mut g2, "home").await;

    let frame = expect_close(&mut g1, 4001).await;
    assert!(
        frame.reason.to_string().contains("superseded"),
        "reason={}",
        frame.reason
    );

    // 新连接接管槽位：browser 的 machine=home 帧路由到新连接。
    let mut br = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut br, USER_TOKEN).await;
    consume_browser_snapshots(&mut br, false, &["home"]).await;
    let f = r#"{"v":1,"type":"ping","machine":"home"}"#;
    br.send(Message::text(f)).await.unwrap();
    assert_eq!(next_text(&mut g2).await, f, "踢旧后槽位归新连接");

    handle.shutdown();
    handle.wait().await;
}

/// 场景 16（v1.1 多终端）：机器数上限——4 台（relay `MAX_GATEWAYS`）不同
/// machine 注册成功后，第 5 台（新 machine）close 4001（reason 表满员）；
/// 满员时**同 machine 重连不受限**（踢旧顶替，槽位复用）。
#[tokio::test]
async fn e2e_gateway_machine_cap_5th_close_4001() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut keep = Vec::new();
    for m in ["m1", "m2", "m3", "m4"] {
        let mut ws = connect_ws(&addr, "/gateway", None).await;
        gateway_hello(&mut ws, m).await;
        keep.push(ws);
    }

    // 第 5 台（新 machine）：认证通过 → close 4001（reason 满员）。
    let mut g5 = connect_ws(&addr, "/gateway", None).await;
    g5.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{}","machine":"m5"}}"#,
        gw_token("m5")
    )))
    .await
    .unwrap();
    let frame = expect_close(&mut g5, 4001).await;
    assert!(
        frame.reason.to_string().contains("gateways"),
        "reason={}",
        frame.reason
    );

    // 满员时同 machine（m2）重连：踢旧顶替，成功入座。
    let mut g2b = connect_ws(&addr, "/gateway", None).await;
    gateway_hello(&mut g2b, "m2").await;
    let frame = expect_close(&mut keep[1], 4001).await; // 旧 m2 被踢
    assert!(
        frame.reason.to_string().contains("superseded"),
        "同 machine 重连是踢旧而非满员拒绝：reason={}",
        frame.reason
    );

    handle.shutdown();
    handle.wait().await;
}

/// 场景 17（v1.1 多终端）：Bearer 直通 = default 机器——与 hello 自报的
/// 其它 machine 并存；machine=default 帧路由到 Bearer 连接；浏览器缺省
/// 帧（两台在线）回 ambiguous_machine。
#[tokio::test]
async fn e2e_bearer_coexists_with_named_machine() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut gw_def = connect_ws(&addr, "/gateway", Some(&gw_token("default"))).await;
    let ack = next_text(&mut gw_def).await;
    assert!(
        ack.contains(r#""machine":"default""#),
        "Bearer ack 回显 default：{ack}"
    );

    let mut gw_other = connect_ws(&addr, "/gateway", None).await;
    gateway_hello(&mut gw_other, "other").await;

    let mut br = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut br, USER_TOKEN).await;
    consume_browser_snapshots(&mut br, true, &["default", "other"]).await;

    // machine=default → Bearer 连接。
    let f = r#"{"v":1,"type":"ping","machine":"default"}"#;
    br.send(Message::text(f)).await.unwrap();
    assert_eq!(
        next_text(&mut gw_def).await,
        f,
        "machine=default → Bearer 网关"
    );

    // machine=other → hello 连接。
    let f = r#"{"v":1,"type":"ping","machine":"other"}"#;
    br.send(Message::text(f)).await.unwrap();
    assert_eq!(
        next_text(&mut gw_other).await,
        f,
        "machine=other → hello 网关"
    );

    // 缺省 + 两台 → ambiguous。探针用 message（真实用户操作帧）：v1.1 起
    // sync/ping 在缺省歧义下静默丢弃（见 e2e_bootstrap_sync_ping_silent），
    // 不再是回错探针。
    br.send(Message::text(r#"{"v":1,"type":"message","text":"给谁"}"#))
        .await
        .unwrap();
    let e = next_of_type(&mut br, "error").await;
    assert_eq!(e["code"], "ambiguous_machine", "{e}");

    handle.shutdown();
    handle.wait().await;
}

/// 场景 18（v1.1 bootstrap 时序豁免）：双网关在线时，SPA 首连的 bootstrap
/// sync 帧不带 machine（machines 列表帧尚未到达，鸡生蛋）、心跳 ping 同理
/// ——relay 对这两类帧在缺省歧义下静默丢弃（不回 ambiguous_machine error，
/// 否则 SPA 每次连接弹一次「多台机器在线」toast）；sync 幂等，SPA 收到
/// machines 帧后补发的带 machine sync 正常路由，连接经静默丢弃依然健在。
/// 反证：真实用户操作帧（message）缺省歧义仍回错。
#[tokio::test]
async fn e2e_bootstrap_sync_ping_silent() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut gw_home = connect_ws(&addr, "/gateway", None).await;
    gateway_hello(&mut gw_home, "home").await;
    let mut gw_work = connect_ws(&addr, "/gateway", None).await;
    gateway_hello(&mut gw_work, "work").await;

    // SPA 首连：hello_ack + 双快照（两台均非 default → machine 快照 false）。
    let mut br = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut br, USER_TOKEN).await;
    consume_browser_snapshots(&mut br, false, &["home", "work"]).await;

    // 无 machine 的 bootstrap sync：静默丢弃，不回 ambiguous error、不断连。
    br.send(Message::text(
        r#"{"v":1,"type":"sync","convId":"web:default"}"#,
    ))
    .await
    .unwrap();
    assert_no_frame(&mut br, "bootstrap sync（无 machine）应静默丢弃").await;
    // 无 machine 的心跳 ping：同款静默。
    br.send(Message::text(r#"{"v":1,"type":"ping"}"#))
        .await
        .unwrap();
    assert_no_frame(&mut br, "ping（无 machine）应静默丢弃").await;

    // 反证：歧义回错路径未被整体关掉——message（真实用户操作）缺省仍回错。
    br.send(Message::text(r#"{"v":1,"type":"message","text":"给谁？"}"#))
        .await
        .unwrap();
    let e = next_of_type(&mut br, "error").await;
    assert_eq!(e["code"], "ambiguous_machine", "{e}");

    // SPA 收 machines 帧后补发的带 machine sync / ping：正常路由到对应
    // 网关（也证明静默丢弃后连接健在——「也不崩」）。
    let f = r#"{"v":1,"type":"sync","machine":"home"}"#;
    br.send(Message::text(f)).await.unwrap();
    assert_eq!(
        next_text(&mut gw_home).await,
        f,
        "带 machine 的 sync → home"
    );
    let f = r#"{"v":1,"type":"ping","machine":"work"}"#;
    br.send(Message::text(f)).await.unwrap();
    assert_eq!(
        next_text(&mut gw_work).await,
        f,
        "带 machine 的 ping → work"
    );

    handle.shutdown();
    handle.wait().await;
}

/// 场景 7：静态与健康检查——GET / 返回 200 且 body 含占位文案；healthz 200。
#[tokio::test]
async fn e2e_static_index_and_healthz() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let (status, _ctype, body) = http_get(&addr, "/").await;
    assert_eq!(status, "200", "GET / 应 200");
    assert!(body.contains("imagent"), "占位文案：{body}");

    let (status, _ctype, _body) = http_get(&addr, "/healthz").await;
    assert_eq!(status, "200", "healthz 应 200");

    handle.shutdown();
    handle.wait().await;
}

/// 场景 9（M3 增补）：SPA base 前缀兼容——不经 Caddy 直连 relay（本地
/// 开发/验收）时，浏览器按 Vite base 配置以 `/imagent/` 前缀请求构建产物，
/// fallback 需剥一次前缀后命中内嵌产物。产物文件名带 hash 不可硬编码，
/// 用稳定文件名断言：/imagent/manifest.json → 200 application/json（真实
/// 文件而非 index.html 回落）；/imagent/ → 200 text/html（剥前缀为空 →
/// index.html）；不带前缀的原路径（Caddy strip 后形态）不受影响。
#[tokio::test]
async fn e2e_spa_base_prefix_and_bare_paths() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    // 带前缀：剥掉 imagent/ 后命中真实文件。
    let (status, ctype, body) = http_get(&addr, "/imagent/manifest.json").await;
    assert_eq!(status, "200", "带前缀 manifest 应剥前缀命中真实文件");
    assert!(
        ctype.starts_with("application/json"),
        "content-type 应按真实文件判定：{ctype}"
    );
    assert!(
        !body.to_ascii_lowercase().contains("<!doctype"),
        "应是 manifest 本体而非 index.html 回落：{body}"
    );

    // 带前缀根路径：剥前缀为空 → index.html。
    let (status, ctype, body) = http_get(&addr, "/imagent/").await;
    assert_eq!(status, "200", "/imagent/ 应回落 index.html");
    assert!(
        ctype.starts_with("text/html"),
        "content-type 应为 HTML：{ctype}"
    );
    assert!(
        body.to_ascii_lowercase().contains("<!doctype"),
        "应拿到 app 入口 HTML：{body}"
    );

    // 不带前缀原路径（Caddy strip 后形态）不受影响。
    let (status, ctype, _body) = http_get(&addr, "/manifest.json").await;
    assert_eq!(status, "200", "原路径 manifest 应 200");
    assert!(
        ctype.starts_with("application/json"),
        "content-type：{ctype}"
    );

    handle.shutdown();
    handle.wait().await;
}

/// 场景 10（M3 增补）：WS/healthz 前缀路由兼容——直连形态下 SPA 按 Vite
/// base 把 WS 地址推导为 ws://.../imagent/ws，Router 需同时注册带前缀
/// 路由（公网 Caddy strip 后走原路径 /ws）。断言 /imagent/ws hello 认证
/// 成功收到 hello_ack；/imagent/gateway Bearer 直通；两端进同一 Hub
///（browser 收到 machine online:true）；/imagent/healthz 是探活而非
/// index.html 回落。
#[tokio::test]
async fn e2e_prefixed_ws_and_healthz_endpoints() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let (status, _ctype, body) = http_get(&addr, "/imagent/healthz").await;
    assert_eq!(status, "200", "/imagent/healthz 应 200");
    assert!(
        !body.to_ascii_lowercase().contains("<!doctype"),
        "应是探活空体而非 spa fallback 的 index.html：{body}"
    );

    let mut br = connect_ws(&addr, "/imagent/ws", None).await;
    hello_handshake(&mut br, USER_TOKEN).await;
    // hello_ack 之后紧跟双快照（此时网关未连 → machine false + machines 空）。
    consume_browser_snapshots(&mut br, false, &[]).await;

    let mut gw = connect_ws(&addr, "/imagent/gateway", Some(&gw_token("default"))).await;
    let ack = next_text(&mut gw).await;
    assert!(ack.contains(r#""role":"gateway""#), "{ack}");

    // 前缀端点与原端点共用同一 Hub：gateway 上线后 browser 收 machine
    // online:true（default 机器旧帧）+ machines 列表。
    let online = next_of_type(&mut br, "machine").await;
    assert_eq!(online["online"], true, "带前缀两端应进同一 Hub：{online}");
    consume_machines_event(&mut br, &["default"]).await;

    handle.shutdown();
    handle.wait().await;
}

/// 场景 19（安全响应头，SPA 纵深防御）：全部响应——SPA 静态（GET /）、
/// healthz、WS upgrade 101——统一带 CSP 与四个辅助安全头。CSP 逐 directive
/// 断言，并反证 script-src 不含 unsafe-inline / unsafe-eval（堵 XSS 偷
/// localStorage 凭证的主通道；style-src 的 'unsafe-inline' 是 React 内联
/// 样式豁免，不得外溢到脚本通道）。
#[tokio::test]
async fn e2e_security_headers_on_all_responses() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    // HTTP 面：/（SPA 入口）与 /healthz 都带头。
    for path in ["/", "/healthz"] {
        let raw = http_get_raw(&addr, path).await;
        let (headers, _body) = raw.split_once("\r\n\r\n").unwrap_or(("", ""));

        let csp = find_header(headers, "content-security-policy").unwrap_or_default();
        for directive in [
            "default-src 'self'",
            "script-src 'self'",
            "style-src 'self' 'unsafe-inline'",
            "img-src 'self' data:",
            "connect-src 'self' wss: ws:",
            "font-src 'self'",
            "frame-ancestors 'none'",
            "base-uri 'none'",
            "form-action 'self'",
        ] {
            assert!(
                csp.contains(directive),
                "{path} CSP 应含 `{directive}`：{csp}"
            );
        }
        let script_src = csp
            .split(';')
            .find(|d| d.trim().starts_with("script-src"))
            .unwrap_or_default();
        assert!(
            !script_src.contains("unsafe-inline") && !script_src.contains("unsafe-eval"),
            "{path} script-src 不得放行 unsafe-*：{script_src}"
        );

        assert_eq!(
            find_header(headers, "x-content-type-options").as_deref(),
            Some("nosniff"),
            "{path} 应带 nosniff"
        );
        assert_eq!(
            find_header(headers, "referrer-policy").as_deref(),
            Some("no-referrer"),
            "{path} 应带 no-referrer"
        );
        assert_eq!(
            find_header(headers, "x-frame-options").as_deref(),
            Some("DENY"),
            "{path} 应带 X-Frame-Options DENY"
        );
        assert_eq!(
            find_header(headers, "cross-origin-opener-policy").as_deref(),
            Some("same-origin"),
            "{path} 应带 COOP same-origin"
        );
    }

    // WS upgrade（101 Switching Protocols）响应同样带头：connect_async 返回
    // 握手 Response，直接读其 headers（握手请求构建与 connect_ws 同款）。
    use tokio_tungstenite::tungstenite::handshake::client::generate_key;
    let req = Request::builder()
        .uri(format!("ws://{addr}/ws"))
        .header("host", addr.to_string())
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", generate_key())
        .body(())
        .unwrap();
    let (ws, resp) = connect_async(req).await.expect("WS 握手");
    let h = resp.headers();
    assert!(
        h.get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("script-src 'self'")),
        "101 升级响应应带 CSP：{h:?}"
    );
    assert_eq!(h.get("x-frame-options").unwrap(), "DENY");
    assert_eq!(h.get("x-content-type-options").unwrap(), "nosniff");
    drop(ws); // 未认证即断，服务端按超时/断连清理

    handle.shutdown();
    handle.wait().await;
}

/// 场景 8：限流——认证后连发 21 帧，收到 rate_limited error 后 close 1008。
#[tokio::test]
async fn e2e_rate_limit_21st_frame() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut br = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut br, USER_TOKEN).await;
    consume_browser_snapshots(&mut br, false, &[]).await; // 网关未连

    for i in 0..21 {
        br.send(Message::text(format!(
            r#"{{"v":1,"type":"message","text":"{i}"}}"#
        )))
        .await
        .unwrap();
    }
    let err = next_text(&mut br).await;
    assert!(
        err.contains(r#""type":"error""#) && err.contains("rate_limited"),
        "{err}"
    );
    expect_close(&mut br, 1008).await;

    handle.shutdown();
    handle.wait().await;
}

/// 场景 20（v1.2 会话层）：登录换发 + 重连 + 兼容形态——hello 带 USER_TOKEN
/// 换发 session{id,token,expiresAt}；新连接带 sessionToken 重连成功（ack
/// 回显 token 与剩余 expiresAt）；同 sessionToken 多连接共用同一会话（多
/// 标签）；旧 token 形态仍可用（每次换新会话——curl/脚本兼容路径）；
/// device_list 看到设备与 connected 标记。
#[tokio::test]
async fn e2e_session_login_reconnect_and_legacy_compat() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    // 登录换发（device 字段命名）。
    let mut b1 = connect_ws(&addr, "/ws", None).await;
    let s1 = login_session(&mut b1, Some("iPhone")).await;
    let (s1_id, s1_token) = (
        s1["id"].as_str().unwrap().to_string(),
        s1["token"].as_str().unwrap().to_string(),
    );
    // expiresAt ≈ now + 30d。
    let exp = s1["expiresAt"].as_u64().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(
        exp > now + 29 * 86_400 && exp < now + 31 * 86_400,
        "30 天有效期：{exp}"
    );
    consume_browser_snapshots(&mut b1, false, &[]).await;

    // device_list：1 台设备，字段齐全，connected=true（b1 在线）。
    b1.send(Message::text(r#"{"v":1,"type":"device_list"}"#))
        .await
        .unwrap();
    let devs = next_of_type(&mut b1, "devices").await;
    let arr = devs["devices"].as_array().expect("devices 数组");
    assert_eq!(arr.len(), 1, "{devs}");
    assert_eq!(arr[0]["id"], s1_id, "设备 id = session id：{devs}");
    assert_eq!(arr[0]["name"], "iPhone", "{devs}");
    assert_eq!(arr[0]["connected"], true, "{devs}");
    assert!(arr[0]["lastSeen"].as_u64().is_some(), "{devs}");
    assert!(arr[0]["created"].as_u64().is_some(), "{devs}");

    // 新连接（第二标签）带 sessionToken 重连：同一会话，connected 保持。
    let mut b2 = connect_ws(&addr, "/ws", None).await;
    resume_session(&mut b2, &s1_token).await;
    consume_browser_snapshots(&mut b2, false, &[]).await;
    b2.send(Message::text(r#"{"v":1,"type":"device_list"}"#))
        .await
        .unwrap();
    let devs = next_of_type(&mut b2, "devices").await;
    assert_eq!(
        devs["devices"].as_array().unwrap().len(),
        1,
        "同会话不新增设备：{devs}"
    );

    // 兼容形态：hello 带 USER_TOKEN（旧形态）→ 每次换发**新**会话。
    let mut b3 = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut b3, USER_TOKEN).await;
    consume_browser_snapshots(&mut b3, false, &[]).await;
    b3.send(Message::text(r#"{"v":1,"type":"device_list"}"#))
        .await
        .unwrap();
    let devs = next_of_type(&mut b3, "devices").await;
    let arr = devs["devices"].as_array().unwrap();
    assert_eq!(arr.len(), 2, "旧形态换新会话（新设备条目）：{devs}");
    assert!(
        arr.iter().any(|d| d["name"] == "unknown-device"),
        "无 device 字段且无 UA → unknown-device：{devs}"
    );

    handle.shutdown();
    handle.wait().await;
}

/// 场景 21（v1.2 会话层）：拒绝 reason 区分——伪造 sessionToken → 1008
/// "auth failed"；hello 无任何凭证 → 1008；过期会话（短 TTL 配置）→ 1008
/// "session expired"。
#[tokio::test]
async fn e2e_session_reject_reasons() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    // 伪造 sessionToken。
    let mut b1 = connect_ws(&addr, "/ws", None).await;
    b1.send(Message::text(
        r#"{"v":1,"type":"hello","session":"ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"}"#,
    ))
    .await
    .unwrap();
    let frame = expect_close(&mut b1, 1008).await;
    assert!(
        frame.reason.to_string().contains("auth failed"),
        "伪造 session token：reason={}",
        frame.reason
    );

    // 无任何凭证（token 与 session 皆无）。
    let mut b2 = connect_ws(&addr, "/ws", None).await;
    b2.send(Message::text(r#"{"v":1,"type":"hello"}"#))
        .await
        .unwrap();
    expect_close(&mut b2, 1008).await;

    // 过期：1s TTL 配置的独立 relay。
    drop(handle.wait());
    let dir = temp_state_dir();
    let handle2 = spawn_with_config(test_config(&dir, 1)).await;
    let addr2 = handle2.addr;
    let mut b3 = connect_ws(&addr2, "/ws", None).await;
    let s = login_session(&mut b3, Some("iPhone")).await;
    consume_browser_snapshots(&mut b3, false, &[]).await;
    drop(b3);
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let mut b4 = connect_ws(&addr2, "/ws", None).await;
    b4.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","session":"{}"}}"#,
        s["token"].as_str().unwrap()
    )))
    .await
    .unwrap();
    let frame = expect_close(&mut b4, 1008).await;
    assert!(
        frame.reason.to_string().contains("session expired"),
        "过期会话：reason={}",
        frame.reason
    );

    handle2.shutdown();
    handle2.wait().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 场景 22（v1.2 会话层）：会话表上限 32——32 次登录（逐连接）成功后第
/// 33 次 close 1008 "too many sessions"（先清理过期/墓碑仍满才拒）。
#[tokio::test]
async fn e2e_session_cap_33rd_login_rejected() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    // 32 次登录（每次新连接 = 新会话；连接即断，会话保留）。
    for i in 0..32 {
        let mut ws = connect_ws(&addr, "/ws", None).await;
        hello_handshake(&mut ws, USER_TOKEN).await;
        // 收到 ack 即会话已入表；断开不清会话（断开≠吊销）。
        drop(ws);
        let _ = i;
    }
    // 第 33 次：USER_TOKEN 正确但表满 → close 1008 "too many sessions"。
    let mut b33 = connect_ws(&addr, "/ws", None).await;
    b33.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{USER_TOKEN}"}}"#
    )))
    .await
    .unwrap();
    let frame = expect_close(&mut b33, 1008).await;
    assert!(
        frame.reason.to_string().contains("too many sessions"),
        "reason={}",
        frame.reason
    );

    handle.shutdown();
    handle.wait().await;
}

/// 场景 23（v1.2 设备管理）：device_revoke——两台设备在线，从 A 吊销 B：
/// B 的连接被断（close 1008 "session revoked"）、A 收到更新后的 devices 帧
///（不含 B）、B 的 sessionToken 重连 → 1008 "session revoked"、A 不受影响。
#[tokio::test]
async fn e2e_device_revoke_disconnects_target() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut a = connect_ws(&addr, "/ws", None).await;
    let sa = login_session(&mut a, Some("iPhone")).await;
    consume_browser_snapshots(&mut a, false, &[]).await;
    let mut b = connect_ws(&addr, "/ws", None).await;
    let sb = login_session(&mut b, Some("MacBook")).await;
    consume_browser_snapshots(&mut b, false, &[]).await;

    // A 吊销 B。
    a.send(Message::text(format!(
        r#"{{"v":1,"type":"device_revoke","deviceId":"{}"}}"#,
        sb["id"].as_str().unwrap()
    )))
    .await
    .unwrap();
    // B 被断：close 1008 "session revoked"。
    let frame = expect_close(&mut b, 1008).await;
    assert!(
        frame.reason.to_string().contains("session revoked"),
        "被吊销连接：reason={}",
        frame.reason
    );
    // A 收到更新后的 devices 帧：只剩 iPhone。
    let devs = next_of_type(&mut a, "devices").await;
    let arr = devs["devices"].as_array().unwrap();
    assert_eq!(arr.len(), 1, "{devs}");
    assert_eq!(arr[0]["name"], "iPhone", "{devs}");

    // B 的 sessionToken 重连 → "session revoked"（墓碑 reason 区分）。
    let mut b2 = connect_ws(&addr, "/ws", None).await;
    b2.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","session":"{}"}}"#,
        sb["token"].as_str().unwrap()
    )))
    .await
    .unwrap();
    let frame = expect_close(&mut b2, 1008).await;
    assert!(
        frame.reason.to_string().contains("session revoked"),
        "重连被吊销会话：reason={}",
        frame.reason
    );

    // A 不受影响（会话仍有效、device_list 可用）。
    a.send(Message::text(r#"{"v":1,"type":"device_list"}"#))
        .await
        .unwrap();
    let devs = next_of_type(&mut a, "devices").await;
    assert_eq!(devs["devices"].as_array().unwrap().len(), 1);
    let _ = sa;

    handle.shutdown();
    handle.wait().await;
}

/// 场景 24（v1.2 会话持久化）：relay 重启（同 state 目录）后会话保留——
/// sessionToken 重连成功、device_list 带出设备（connected 重算）。
#[tokio::test]
async fn e2e_sessions_persist_across_relay_restart() {
    let dir = temp_state_dir();
    let handle = spawn_with_config(test_config(&dir, DEFAULT_SESSION_TTL_SECS)).await;
    let addr = handle.addr;

    let mut b = connect_ws(&addr, "/ws", None).await;
    let s = login_session(&mut b, Some("iPhone")).await;
    consume_browser_snapshots(&mut b, false, &[]).await;
    let token = s["token"].as_str().unwrap().to_string();
    handle.shutdown();
    handle.wait().await;

    // 重启：新 relay 进程（同 state 目录、新端口）。
    let handle2 = spawn_with_config(test_config(&dir, DEFAULT_SESSION_TTL_SECS)).await;
    let addr2 = handle2.addr;
    let mut b2 = connect_ws(&addr2, "/ws", None).await;
    resume_session(&mut b2, &token).await;
    consume_browser_snapshots(&mut b2, false, &[]).await;
    b2.send(Message::text(r#"{"v":1,"type":"device_list"}"#))
        .await
        .unwrap();
    let devs = next_of_type(&mut b2, "devices").await;
    let arr = devs["devices"].as_array().unwrap();
    assert_eq!(arr.len(), 1, "重启后会话保留：{devs}");
    assert_eq!(arr[0]["name"], "iPhone", "{devs}");
    assert_eq!(arr[0]["connected"], true, "重连后 connected 重算：{devs}");

    handle2.shutdown();
    handle2.wait().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 场景 25（v1.2 网关 token-machine 绑定）：token 对且 machine 匹配 →
/// 通过；token 对但 machine 不匹配（顶替其它机器）→ 1008；未知 token →
/// 1008；Bearer = default 机器，只有绑定 default 的 token 可走 Bearer。
#[tokio::test]
async fn e2e_gateway_token_machine_binding() {
    let gateway_tokens = GatewayTokens::parse(
        "default=def-tok-0123456789abcdef,home-mac=home-tok-0123456789abcdef,work-mac=work-tok-0123456789abcdef",
    )
    .unwrap();
    let dir = temp_state_dir();
    let handle = spawn_with_config(RelayConfig {
        gateway_tokens,
        user_token: USER_TOKEN.to_string(),
        state_dir: dir.clone(),
        session_ttl_secs: DEFAULT_SESSION_TTL_SECS,
    })
    .await;
    let addr = handle.addr;

    // 正确绑定：token 对 + machine 对。
    let mut g1 = connect_ws(&addr, "/gateway", None).await;
    g1.send(Message::text(
        r#"{"v":1,"type":"hello","token":"home-tok-0123456789abcdef","machine":"home-mac"}"#,
    ))
    .await
    .unwrap();
    let ack = next_text(&mut g1).await;
    assert!(
        ack.contains(r#""machine":"home-mac""#),
        "正确绑定放行并回显：{ack}"
    );

    // token 对但 machine 不匹配：home 的 token 顶替 work → 1008。
    let mut g2 = connect_ws(&addr, "/gateway", None).await;
    g2.send(Message::text(
        r#"{"v":1,"type":"hello","token":"home-tok-0123456789abcdef","machine":"work-mac"}"#,
    ))
    .await
    .unwrap();
    expect_close(&mut g2, 1008).await;

    // 未知 token。
    let mut g3 = connect_ws(&addr, "/gateway", None).await;
    g3.send(Message::text(
        r#"{"v":1,"type":"hello","token":"nobody-knows-this-token","machine":"home-mac"}"#,
    ))
    .await
    .unwrap();
    expect_close(&mut g3, 1008).await;

    // Bearer = default 机器：非 default 绑定的 token → 1008。
    let mut g4 = connect_ws(&addr, "/gateway", Some("home-tok-0123456789abcdef")).await;
    expect_close(&mut g4, 1008).await;
    // default 绑定的 token → Bearer 直通 default 机器。
    let mut g5 = connect_ws(&addr, "/gateway", Some("def-tok-0123456789abcdef")).await;
    let ack = next_text(&mut g5).await;
    assert!(
        ack.contains(r#""machine":"default""#),
        "default 绑定 token 可走 Bearer：{ack}"
    );

    handle.shutdown();
    handle.wait().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 场景 26（v1.2 会话层）：浏览器 Bearer USER_TOKEN = 登录形态（curl/脚本
/// 便利，无从携带 sessionToken）→ 同样换发会话（hello_ack 带 session 载荷）。
#[tokio::test]
async fn e2e_browser_bearer_issues_session() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut br = connect_ws(&addr, "/ws", Some(USER_TOKEN)).await;
    let ack = next_text(&mut br).await;
    assert!(
        ack.contains(r#""type":"hello_ack""#) && ack.contains(r#""session""#),
        "Bearer 登录也应换发会话：{ack}"
    );
    consume_browser_snapshots(&mut br, false, &[]).await;

    // 错误 Bearer → 失败路径（1008）。
    let mut bad = connect_ws(&addr, "/ws", Some("wrong-token")).await;
    expect_close(&mut bad, 1008).await;

    handle.shutdown();
    handle.wait().await;
}
