//! relay 集成测试：真 server（lib `spawn_relay`，绑 127.0.0.1:0 随机端口）
//! 加 tokio-tungstenite 客户端，覆盖任务说明 §5 场景与多端并存改造用例
//!（多 browser 共存广播 / 并存上限 4001 / 单条退出其余存活）。
//! close code 断言依赖 tungstenite 把 Close 帧投递给应用层，可精确断言，
//! 无需退化。

use std::net::SocketAddr;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use imagent_relay::{spawn_relay, RelayConfig, RelayHandle};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 测试 token（本地随机端口 + loopback，无真实敏感面）。
const GW_TOKEN: &str = "test-gateway-token-0123456789abcdef";
const USER_TOKEN: &str = "test-user-token-0123456789abcdef00";

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

/// hello 首帧握手：发 hello、断言收到 hello_ack。
async fn hello_handshake(ws: &mut Ws, token: &str) {
    ws.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{token}"}}"#
    )))
    .await
    .unwrap();
    let ack = next_text(ws).await;
    assert!(ack.contains(r#""type":"hello_ack""#), "hello_ack: {ack}");
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

/// 裸 HTTP GET（loopback 直连不走代理，对齐 webhook 测试注释），返回
/// (状态码字符串, content-type, body)。
async fn http_get(addr: &SocketAddr, path: &str) -> (String, String, String) {
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
    let raw = String::from_utf8_lossy(&buf).to_string();
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

/// 场景 1：双端互通——browser↔gateway 文本帧原样透传（不解析不改写）。
#[tokio::test]
async fn e2e_bidirectional_passthrough() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut gw = connect_ws(&addr, "/gateway", Some(GW_TOKEN)).await;
    let ack = next_text(&mut gw).await;
    assert!(ack.contains(r#""role":"gateway""#), "{ack}");

    let mut br = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut br, USER_TOKEN).await;
    // 浏览器认证后立即收到 machine 快照（网关已在线）。
    let snap = next_text(&mut br).await;
    assert!(
        snap.contains(r#""type":"machine""#) && snap.contains(r#""online":true"#),
        "{snap}"
    );

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
///（服务端 1s 防爆破延迟后再关，超时预算放大到 5s）。
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

    handle.shutdown();
    handle.wait().await;
}

/// 场景 3：Authorization 头直通（网关 Rust 客户端便利形态）——正确头跳过
/// hello 直接收到 hello_ack；错误头走失败路径 1008。
#[tokio::test]
async fn e2e_bearer_header_direct_auth() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut gw = connect_ws(&addr, "/gateway", Some(GW_TOKEN)).await;
    let ack = next_text(&mut gw).await;
    assert!(
        ack.contains(r#""type":"hello_ack""#) && ack.contains(r#""role":"gateway""#),
        "{ack}"
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
    let snap = next_text(&mut br).await;
    assert!(snap.contains(r#""online":false"#), "网关离线快照：{snap}");

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

    let mut gw = connect_ws(&addr, "/gateway", Some(GW_TOKEN)).await;
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

/// 场景 5：machine 通知——网关连接/断开时 browser 收到 online true/false。
#[tokio::test]
async fn e2e_machine_online_offline_notify() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut br = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut br, USER_TOKEN).await;
    let snap = next_text(&mut br).await;
    assert!(snap.contains(r#""online":false"#), "{snap}");

    let mut gw = connect_ws(&addr, "/gateway", Some(GW_TOKEN)).await;
    let _ = next_text(&mut gw).await; // hello_ack
    let m = next_text(&mut br).await;
    assert!(m.contains(r#""online":true"#), "网关上线：{m}");

    // 网关干净断开（发 close 帧）→ 服务端清槽位 → browser 收 offline。
    gw.close(None).await.unwrap();
    let m = next_text(&mut br).await;
    assert!(m.contains(r#""online":false"#), "网关离线：{m}");

    handle.shutdown();
    handle.wait().await;
}

/// 场景 6：踢旧——第二条网关连接认证后，第一条收到 close 4001
///（reason superseded），槽位归新连接。
#[tokio::test]
async fn e2e_supersede_kicks_old_gateway_4001() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    let mut g1 = connect_ws(&addr, "/gateway", Some(GW_TOKEN)).await;
    let _ = next_text(&mut g1).await; // hello_ack

    let mut g2 = connect_ws(&addr, "/gateway", Some(GW_TOKEN)).await;
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
    let _snap = next_text(&mut br).await;
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

    // 两条 browser 先在线（各自 machine 快照 false，第一条不被踢）。
    let mut b1 = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut b1, USER_TOKEN).await;
    let snap = next_text(&mut b1).await;
    assert!(snap.contains(r#""online":false"#), "{snap}");
    let mut b2 = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut b2, USER_TOKEN).await;
    let snap = next_text(&mut b2).await;
    assert!(snap.contains(r#""online":false"#), "{snap}");

    // 网关上线：machine online 广播，两条都收到。
    let mut gw = connect_ws(&addr, "/gateway", Some(GW_TOKEN)).await;
    let _ = next_text(&mut gw).await; // hello_ack
    let m = next_text(&mut b1).await;
    assert!(m.contains(r#""online":true"#), "b1 收上线广播：{m}");
    let m = next_text(&mut b2).await;
    assert!(m.contains(r#""online":true"#), "b2 收上线广播：{m}");

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
    let m = next_text(&mut b1).await;
    assert!(m.contains(r#""online":false"#), "b1 收下线广播：{m}");
    let m = next_text(&mut b2).await;
    assert!(m.contains(r#""online":false"#), "b2 收下线广播：{m}");

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

    let mut gw = connect_ws(&addr, "/gateway", Some(GW_TOKEN)).await;
    let _ = next_text(&mut gw).await; // hello_ack

    let mut browsers = Vec::new();
    for _ in 0..8 {
        let mut ws = connect_ws(&addr, "/ws", None).await;
        hello_handshake(&mut ws, USER_TOKEN).await;
        let snap = next_text(&mut ws).await;
        assert!(snap.contains(r#""online":true"#), "{snap}");
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
    let _ = next_text(&mut b1).await; // machine false
    let mut b2 = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut b2, USER_TOKEN).await;
    let _ = next_text(&mut b2).await; // machine false

    // b1 干净断开 → 服务端只清 b1 自己的条目。
    b1.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await; // 等服务端 release

    // 网关上线：machine online 广播，b2（仍在线）收到。
    let mut gw = connect_ws(&addr, "/gateway", Some(GW_TOKEN)).await;
    let _ = next_text(&mut gw).await;
    let m = next_text(&mut b2).await;
    assert!(m.contains(r#""online":true"#), "b2 收上线广播：{m}");

    // gateway 业务帧：b2 仍正常收（b1 退出未影响广播目标）。
    let g = r#"{"v":1,"type":"text","text":"after-leave"}"#;
    gw.send(Message::text(g)).await.unwrap();
    assert_eq!(next_text(&mut b2).await, g);

    // 网关断开：machine offline 广播，b2 收到。
    gw.close(None).await.unwrap();
    let m = next_text(&mut b2).await;
    assert!(m.contains(r#""online":false"#), "b2 收下线广播：{m}");

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
    // hello_ack 之后紧跟 machine 快照（此时网关未连 → false）。
    let snap = next_text(&mut br).await;
    assert!(snap.contains(r#""online":false"#), "{snap}");

    let mut gw = connect_ws(&addr, "/imagent/gateway", Some(GW_TOKEN)).await;
    let ack = next_text(&mut gw).await;
    assert!(ack.contains(r#""role":"gateway""#), "{ack}");

    // 前缀端点与原端点共用同一 Hub：gateway 上线后 browser 收 machine
    // online:true。
    let online = next_text(&mut br).await;
    assert!(
        online.contains(r#""online":true"#),
        "带前缀两端应进同一 Hub：{online}"
    );

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
    let _snap = next_text(&mut br).await; // machine false（网关未连）

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
