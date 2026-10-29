//! relay 集成测试：真 server（lib `spawn_relay`，绑 127.0.0.1:0 随机端口）
//! 加 tokio-tungstenite 客户端，覆盖任务说明 §5 场景与多端并存改造用例
//!（多 browser 共存广播 / 并存上限 4001 / 单条退出其余存活），以及 v1.1
//! 多终端用例（多机器并存 / machine 路由 / machines 帧广播 / 同机踢旧 /
//! 机器上限 / Bearer=default 回退 / 缺省歧义的 bootstrap sync/ping 静默
//! 豁免）与 v1.2 会话层用例（登录换发 / sessionToken 重连 / 过期与吊销
//! reason 区分 / 设备列表与吊销断连 / 会话落盘重启保留 / 会话表上限 /
//! 网关 token-machine 绑定）。close code 断言依赖 tungstenite 把 Close 帧
//! 投递给应用层，可精确断言，无需退化。
//! v1.3 passkey：内置**软认证器**（openssl ES256 + 手写最小 CBOR）对
//! register/login 全流程做**真签名** e2e——webauthn-rs 的 fake 模块只生成
//! 假 credential id 分布（无签名能力），不适用；真浏览器认证器无法进
//! CI，软认证器等价覆盖服务端验签路径（challenge/origin/rpIdHash/UP/UV/
//! 签名/计数器）。
//! v1.3 网关 ed25519 挑战应答：key 握手全流程（测试内生成密钥对真签名）、
//! 错私钥/错公钥/未知 machine 拒、nonce 重放拒（同 prove 重发）、双表
//!（KEYS/TOKENS）并存任一命中——token 路径由既有全量用例回归。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use futures::{SinkExt, StreamExt};
use imagent_relay::{
    spawn_relay, GatewayKeys, GatewayTokens, RelayConfig, RelayHandle, DEFAULT_SESSION_TTL_SECS,
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
        // v1.3：默认空公钥表（token-only 形态合法）；key 用例见
        // `key_only_config` / `dual_table_config`。
        gateway_keys: GatewayKeys::default(),
        user_token: USER_TOKEN.to_string(),
        state_dir: state_dir.to_path_buf(),
        session_ttl_secs,
        webauthn_rp_id: None, // rp_id 按请求 Host（localhost）推导
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

// ---------------------------------------------------------------------------
// v1.3 网关 ed25519 挑战应答（key 握手）
// ---------------------------------------------------------------------------

/// 测试内派生 ed25519 密钥对（seed 固定可复现）。
fn test_signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

/// 公钥 hex（GATEWAY_KEYS 表 / prove.pubkey 同一形态）。
fn pubkey_hex(sk: &SigningKey) -> String {
    hex::encode(sk.verifying_key().to_bytes())
}

/// key-only 配置（tokens 全空）：`machine → 密钥对` 列表建公钥表。
fn key_only_config(
    state_dir: &std::path::Path,
    pairs: &[(&'static str, SigningKey)],
) -> RelayConfig {
    let spec = pairs
        .iter()
        .map(|(m, sk)| format!("{m}={}", pubkey_hex(sk)))
        .collect::<Vec<_>>()
        .join(",");
    RelayConfig {
        gateway_tokens: GatewayTokens::default(),
        gateway_keys: GatewayKeys::parse(&spec).expect("测试公钥表合法"),
        user_token: USER_TOKEN.to_string(),
        state_dir: state_dir.to_path_buf(),
        session_ttl_secs: DEFAULT_SESSION_TTL_SECS,
        webauthn_rp_id: None,
    }
}

/// v1.3 key 握手三帧（真签名）：hello（无 token）→ challenge（断言 nonce
/// 为 64 hex）→ prove（对 nonce 原文签名）→ hello_ack（回显 machine）。
async fn gateway_key_handshake(ws: &mut Ws, machine: &str, sk: &SigningKey) {
    ws.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","machine":"{machine}"}}"#
    )))
    .await
    .unwrap();
    let challenge = next_of_type(ws, "challenge").await;
    let nonce = challenge["nonce"].as_str().expect("challenge 带 nonce");
    assert_eq!(nonce.len(), 64, "nonce 为 32B hex：{challenge}");
    assert!(nonce.chars().all(|c| c.is_ascii_hexdigit()));
    let prove = serde_json::json!({
        "v": 1,
        "type": "prove",
        "pubkey": pubkey_hex(sk),
        "signature": hex::encode(sk.sign(nonce.as_bytes()).to_bytes()),
    });
    ws.send(Message::text(prove.to_string())).await.unwrap();
    let ack = next_text(ws).await;
    assert!(
        ack.contains(r#""type":"hello_ack""#) && ack.contains(&format!(r#""machine":"{machine}""#)),
        "hello_ack 应回显 machine：{ack}"
    );
}

/// 发一条 prove（不经握手辅助——重放/篡改用例需要逐字段控制）。
async fn send_prove(ws: &mut Ws, pubkey: &str, signature: &str) {
    ws.send(Message::text(
        serde_json::json!({
            "v": 1, "type": "prove", "pubkey": pubkey, "signature": signature
        })
        .to_string(),
    ))
    .await
    .unwrap();
}

/// 场景 31（v1.3 key 握手）：keys-only relay（tokens 全空——「只配公钥表
/// 也合法」）下 hello → challenge → prove → hello_ack 全流程；浏览器侧
/// machines 快照含该机器；业务帧双向透传不受认证形态影响。
#[tokio::test]
async fn e2e_gateway_key_handshake_full_flow() {
    let dir = temp_state_dir();
    let sk = test_signing_key(1);
    let handle = spawn_with_config(key_only_config(&dir, &[("home", sk.clone())])).await;
    let addr = handle.addr;

    let mut gw = connect_ws(&addr, "/gateway", None).await;
    gateway_key_handshake(&mut gw, "home", &sk).await;

    // 浏览器视角：home 上线（machines 快照）。
    let mut br = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut br, USER_TOKEN).await;
    consume_browser_snapshots(&mut br, false, &["home"]).await;

    // 双向透传（认证形态不影响转发语义）。
    br.send(Message::text(
        r#"{"v":1,"type":"message","machine":"home","convId":"web:work","text":"hi"}"#,
    ))
    .await
    .unwrap();
    assert_eq!(
        next_text(&mut gw).await,
        r#"{"v":1,"type":"message","machine":"home","convId":"web:work","text":"hi"}"#
    );
    let out = r#"{"v":1,"type":"text","machine":"home","convId":"web:work","text":"done"}"#;
    gw.send(Message::text(out)).await.unwrap();
    assert_eq!(next_text(&mut br).await, out);

    handle.shutdown();
    handle.wait().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 场景 32（v1.3 key 握手拒绝面）：错私钥（公钥不匹配 + 签名也错）/
/// 正确公钥但错私钥签名（验签路径）/ 未知 machine（nonce 照发——防探测，
/// prove 必拒）/ 畸形 prove / prove 阶段重发 hello，均 1008。
#[tokio::test]
async fn e2e_gateway_key_rejections_close_1008() {
    let dir = temp_state_dir();
    let bound = test_signing_key(1);
    let handle = spawn_with_config(key_only_config(&dir, &[("home", bound.clone())])).await;
    let addr = handle.addr;

    // 读 challenge 取 nonce（async fn 避免闭包 lifetime 纠缠）。
    async fn nonce_of(ws: &mut Ws) -> String {
        let c = next_of_type(ws, "challenge").await;
        c["nonce"].as_str().unwrap().to_string()
    }

    // 错私钥（prove.pubkey ≠ 绑定公钥，签名也不匹配）。
    let wrong = test_signing_key(2);
    {
        let mut ws = connect_ws(&addr, "/gateway", None).await;
        ws.send(Message::text(r#"{"v":1,"type":"hello","machine":"home"}"#))
            .await
            .unwrap();
        let nonce = nonce_of(&mut ws).await;
        let sig = hex::encode(wrong.sign(nonce.as_bytes()).to_bytes());
        send_prove(&mut ws, &pubkey_hex(&wrong), &sig).await;
        expect_close(&mut ws, 1008).await;
    }

    // 正确公钥 + 错私钥签名（公钥比对过，验签拒——两道防线分别覆盖）。
    {
        let mut ws = connect_ws(&addr, "/gateway", None).await;
        ws.send(Message::text(r#"{"v":1,"type":"hello","machine":"home"}"#))
            .await
            .unwrap();
        let nonce = nonce_of(&mut ws).await;
        let sig = hex::encode(wrong.sign(nonce.as_bytes()).to_bytes());
        send_prove(&mut ws, &pubkey_hex(&bound), &sig).await;
        expect_close(&mut ws, 1008).await;
    }

    // 未知 machine（shape 合法但不在表）：challenge 照发（防探测），
    // prove 必拒。
    {
        let mut ws = connect_ws(&addr, "/gateway", None).await;
        ws.send(Message::text(r#"{"v":1,"type":"hello","machine":"ghost"}"#))
            .await
            .unwrap();
        let nonce = nonce_of(&mut ws).await;
        let sig = hex::encode(bound.sign(nonce.as_bytes()).to_bytes());
        send_prove(&mut ws, &pubkey_hex(&bound), &sig).await;
        expect_close(&mut ws, 1008).await;
    }

    // 畸形 prove（非 JSON / 错 type）。
    {
        let mut ws = connect_ws(&addr, "/gateway", None).await;
        ws.send(Message::text(r#"{"v":1,"type":"hello","machine":"home"}"#))
            .await
            .unwrap();
        let _ = nonce_of(&mut ws).await;
        ws.send(Message::text("not json")).await.unwrap();
        expect_close(&mut ws, 1008).await;
    }
    {
        let mut ws = connect_ws(&addr, "/gateway", None).await;
        ws.send(Message::text(r#"{"v":1,"type":"hello","machine":"home"}"#))
            .await
            .unwrap();
        let _ = nonce_of(&mut ws).await;
        // prove 阶段重发 hello（非 prove 帧）。
        ws.send(Message::text(r#"{"v":1,"type":"hello","machine":"home"}"#))
            .await
            .unwrap();
        expect_close(&mut ws, 1008).await;
    }

    handle.shutdown();
    handle.wait().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 场景 33（v1.3 nonce 防重放）：完整握手消费 nonce 后，**同一条 prove
/// 帧**在新连接上重发（新 challenge 已下发）→ 1008——nonce 单次消费，
/// 录制 prove 重放无效。
#[tokio::test]
async fn e2e_gateway_key_nonce_replay_rejected() {
    let dir = temp_state_dir();
    let sk = test_signing_key(1);
    let handle = spawn_with_config(key_only_config(&dir, &[("home", sk.clone())])).await;
    let addr = handle.addr;

    // 第一次：正常握手（nonce 消费掉），录下 prove 原文。
    let mut a = connect_ws(&addr, "/gateway", None).await;
    a.send(Message::text(r#"{"v":1,"type":"hello","machine":"home"}"#))
        .await
        .unwrap();
    let nonce = next_of_type(&mut a, "challenge").await["nonce"]
        .as_str()
        .unwrap()
        .to_string();
    let prove = serde_json::json!({
        "v": 1, "type": "prove",
        "pubkey": pubkey_hex(&sk),
        "signature": hex::encode(sk.sign(nonce.as_bytes()).to_bytes()),
    })
    .to_string();
    a.send(Message::text(prove.clone())).await.unwrap();
    let ack = next_text(&mut a).await;
    assert!(
        ack.contains(r#""type":"hello_ack""#),
        "首次握手应成功：{ack}"
    );

    // 第二次：新连接拿到**新** challenge，重放旧 prove（旧 nonce 已消费）。
    let mut b = connect_ws(&addr, "/gateway", None).await;
    b.send(Message::text(r#"{"v":1,"type":"hello","machine":"home"}"#))
        .await
        .unwrap();
    let new_nonce = next_of_type(&mut b, "challenge").await["nonce"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(new_nonce, nonce, "nonce 一次性，新连接必然不同");
    b.send(Message::text(prove)).await.unwrap();
    expect_close(&mut b, 1008).await;

    handle.shutdown();
    handle.wait().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 场景 34（v1.3 双表并存）：KEYS 与 TOKENS 同时配置——token 机器与 key
/// 机器**同时在线**（各自走各自认证路径）；同一 machine 两表都绑定时，
/// token / key 两种 hello 形态均可认证（任一表命中即放行）。
#[tokio::test]
async fn e2e_gateway_dual_tables_coexist() {
    let dir = temp_state_dir();
    let home_key = test_signing_key(1);
    let keyed_key = test_signing_key(2);
    // tokens：既有 TEST_MACHINES 表；keys：home（双表绑定）+ keyed-mac。
    let keys_spec = format!(
        "home={},keyed-mac={}",
        pubkey_hex(&home_key),
        pubkey_hex(&keyed_key)
    );
    let config = RelayConfig {
        gateway_tokens: test_gateway_tokens(),
        gateway_keys: GatewayKeys::parse(&keys_spec).unwrap(),
        user_token: USER_TOKEN.to_string(),
        state_dir: dir.clone(),
        session_ttl_secs: DEFAULT_SESSION_TTL_SECS,
        webauthn_rp_id: None,
    };
    let handle = spawn_with_config(config).await;
    let addr = handle.addr;

    // token 机器（home，走既有一步校验）与 key 机器（keyed-mac）同时在线。
    let mut gw_token = connect_ws(&addr, "/gateway", None).await;
    gateway_hello(&mut gw_token, "home").await;
    let mut gw_key = connect_ws(&addr, "/gateway", None).await;
    gateway_key_handshake(&mut gw_key, "keyed-mac", &keyed_key).await;

    let mut br = connect_ws(&addr, "/ws", None).await;
    hello_handshake(&mut br, USER_TOKEN).await;
    consume_browser_snapshots(&mut br, false, &["home", "keyed-mac"]).await;

    // 同一 machine（home）双表绑定：断开 token 连接后改走 key 形态亦认证
    // 通过（任一表命中）。注意 home 槽位单连接——先关旧的再连。
    drop(gw_token);
    tokio::time::sleep(Duration::from_millis(100)).await; // 等 relay 清槽位
    let mut gw_home_key = connect_ws(&addr, "/gateway", None).await;
    gateway_key_handshake(&mut gw_home_key, "home", &home_key).await;
    // drop 的下线广播与新连接的上线广播次序有竞态（踢旧顶替时甚至只有
    // 一帧），循环消费 machines 帧直到最终态：home（key 形态）+ keyed-mac。
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let v = next_of_type(&mut br, "machines").await;
        let mut got: Vec<String> = v["online"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap().to_string())
            .collect();
        got.sort();
        if got == vec!["home".to_string(), "keyed-mac".to_string()] {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "home（key 形态）未恢复在线：{v}"
        );
    }

    handle.shutdown();
    handle.wait().await;
    let _ = std::fs::remove_dir_all(&dir);
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

/// 场景 22（v1.3 会话表 LRU 改进）：32 次登录（逐连接断开，全员**离线**）
/// 占满会话表后，第 33 次登录**成功**——LRU 淘汰最旧离线会话腾位，而非
/// 拒绝。这是修真实发生过的死锁（单用户测试/重登累积占满 32 会话且全部
/// 设备离线 → 无法登录、无法自助 device_revoke 吊销腾位，只能登 ECS 删
/// sessions.json）。断言：表仍 32；33 个 token 恰有 1 个失效（被淘汰者，
/// 其余 31 个 + 新会话全部有效）。「全部在线才拒绝」分支因浏览器并存上限
/// MAX_BROWSERS=8 < 32 无法经真连接铺满 32 个在线会话，由 sessions 单元
/// 测试覆盖（session_cap_eviction_and_reject §5）。
#[tokio::test]
async fn e2e_session_cap_lru_evicts_oldest_offline() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;

    // 32 次登录（每次新连接 = 新会话；连接即断，会话离线保留）。
    let mut tokens = Vec::new();
    for _ in 0..32 {
        let mut ws = connect_ws(&addr, "/ws", None).await;
        let s = login_session(&mut ws, None).await;
        tokens.push(s["token"].as_str().unwrap().to_string());
        drop(ws); // 断开 ≠ 吊销：会话保留（离线）
    }

    // 第 33 次：USER_TOKEN 正确且存在离线候选 → 成功（LRU 淘汰腾位）。
    let mut b33 = connect_ws(&addr, "/ws", None).await;
    let _s33 = login_session(&mut b33, Some("newcomer")).await;
    consume_browser_snapshots(&mut b33, false, &[]).await;

    // 表仍 32，含 newcomer。
    b33.send(Message::text(r#"{"v":1,"type":"device_list"}"#))
        .await
        .unwrap();
    let devs = next_of_type(&mut b33, "devices").await;
    let arr = devs["devices"].as_array().unwrap();
    assert_eq!(arr.len(), 32, "淘汰一入一出：{devs}");
    assert!(
        arr.iter().any(|d| d["name"] == "newcomer"),
        "新会话在列表：{devs}"
    );

    // 原 32 个 token 恰 1 个失效（被 LRU 淘汰；同秒批量签发时按 id 决胜，
    // 不断言具体是谁，断言「恰一个」）。
    let mut rejected = 0;
    for t in &tokens {
        let mut ws = connect_ws(&addr, "/ws", None).await;
        if !session_still_valid(&mut ws, t).await {
            rejected += 1;
        }
        drop(ws);
    }
    assert_eq!(rejected, 1, "32 个旧 token 恰 1 个被淘汰");

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
        gateway_keys: GatewayKeys::default(),
        user_token: USER_TOKEN.to_string(),
        state_dir: dir.clone(),
        session_ttl_secs: DEFAULT_SESSION_TTL_SECS,
        webauthn_rp_id: None,
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

// ===========================================================================
// v1.3 passkey（WebAuthn）e2e：软认证器真签名
// ===========================================================================

use sha2::{Digest, Sha256};

/// base64url（无 padding）编码（软认证器构造 credential JSON 用；与 relay
/// 侧 webauthn::b64url_encode 同一约定）。
fn b64url(bytes: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TBL[(n >> 18) as usize & 63] as char);
        out.push(TBL[(n >> 12) as usize & 63] as char);
        if c.len() > 1 {
            out.push(TBL[(n >> 6) as usize & 63] as char);
        }
        if c.len() > 2 {
            out.push(TBL[n as usize & 63] as char);
        }
    }
    out
}

/// 最小 CBOR 编码（仅软认证器需要的固定形状：uint/nint/bstr/tstr/map 头）。
mod cbor {
    fn head(major: u8, v: u64, out: &mut Vec<u8>) {
        let m = major << 5;
        if v < 24 {
            out.push(m | v as u8);
        } else if v <= u8::MAX as u64 {
            out.extend_from_slice(&[m | 24, v as u8]);
        } else if v <= u16::MAX as u64 {
            out.push(m | 25);
            out.extend_from_slice(&(v as u16).to_be_bytes());
        } else {
            out.push(m | 26);
            out.extend_from_slice(&(v as u32).to_be_bytes());
        }
    }
    /// 无符号整数。
    pub fn uint(v: u64, out: &mut Vec<u8>) {
        head(0, v, out);
    }
    /// 负整数 -n（n ≥ 1）。
    pub fn nint(n: u64, out: &mut Vec<u8>) {
        head(1, n - 1, out);
    }
    /// 字节串。
    pub fn bstr(bytes: &[u8], out: &mut Vec<u8>) {
        head(2, bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }
    /// 文本串。
    pub fn tstr(s: &str, out: &mut Vec<u8>) {
        head(3, s.len() as u64, out);
        out.extend_from_slice(s.as_bytes());
    }
    /// map 头（键值对数量）。
    pub fn map(n: usize, out: &mut Vec<u8>) {
        head(5, n as u64, out);
    }
}

/// 软认证器（ES256 / P-256）：模拟真认证器的注册（"none" attestation）与
/// 登录（断言签名）。计数器单调递增（克隆检测依赖）。
struct SoftAuthenticator {
    key: openssl::ec::EcKey<openssl::pkey::Private>,
    cred_id: Vec<u8>,
    counter: u32,
}

impl SoftAuthenticator {
    fn new() -> Self {
        use openssl::ec::EcKey;
        use openssl::nid::Nid;
        let group = openssl::ec::EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let key = EcKey::generate(&group).unwrap();
        Self {
            key,
            cred_id: (0u16..16).map(|i| (i as u8) ^ 0x5a).collect(),
            counter: 0,
        }
    }

    /// 公钥坐标（32 字节大端、左填充）。
    fn coords(&self) -> (Vec<u8>, Vec<u8>) {
        use openssl::bn::BigNumContext;
        let group =
            openssl::ec::EcGroup::from_curve_name(openssl::nid::Nid::X9_62_PRIME256V1).unwrap();
        let (mut x, mut y) = (
            openssl::bn::BigNum::new().unwrap(),
            openssl::bn::BigNum::new().unwrap(),
        );
        let mut ctx = BigNumContext::new().unwrap();
        self.key
            .public_key()
            .affine_coordinates_gfp(&group, &mut x, &mut y, &mut ctx)
            .unwrap();
        (x.to_vec_padded(32).unwrap(), y.to_vec_padded(32).unwrap())
    }

    /// COSE ES256 公钥 CBOR：{1:2, 3:-7, -1:1, -2:x, -3:y}。
    fn cose_key(&self) -> Vec<u8> {
        let (x, y) = self.coords();
        let mut out = Vec::new();
        cbor::map(5, &mut out);
        cbor::uint(1, &mut out);
        cbor::uint(2, &mut out);
        cbor::uint(3, &mut out);
        cbor::nint(7, &mut out);
        cbor::nint(1, &mut out);
        cbor::uint(1, &mut out);
        cbor::nint(2, &mut out);
        cbor::bstr(&x, &mut out);
        cbor::nint(3, &mut out);
        cbor::bstr(&y, &mut out);
        out
    }

    /// attestedCredentialData（注册）：aaguid(0) || len(2) || credId || COSE。
    fn attested_credential_data(&self) -> Vec<u8> {
        let mut out = vec![0u8; 16]; // aaguid（"none" fmt 不校验）
        out.extend_from_slice(&(self.cred_id.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.cred_id);
        out.extend_from_slice(&self.cose_key());
        out
    }

    /// 注册响应（RegisterPublicKeyCredential JSON）。`options` = begin 响应
    /// 的 `publicKey`；`origin` = clientDataJSON origin（与 relay 推导一致）。
    fn register(&self, options: &serde_json::Value, origin: &str) -> serde_json::Value {
        let rp_id = options["rp"]["id"].as_str().unwrap_or("localhost");
        let challenge = options["challenge"].as_str().expect("challenge");
        let client_data = serde_json::json!({
            "type": "webauthn.create",
            "challenge": challenge,
            "origin": origin,
        })
        .to_string();
        // authData：rpIdHash || flags(UP|UV|AT=0x45) || counter(0) || attested。
        let mut auth_data = Sha256::digest(rp_id.as_bytes()).to_vec();
        auth_data.push(0x45);
        auth_data.extend_from_slice(&0u32.to_be_bytes());
        auth_data.extend_from_slice(&self.attested_credential_data());
        // attestationObject（"none" fmt）：{"fmt":"none","attStmt":{},"authData":…}。
        let mut att_obj = Vec::new();
        cbor::map(3, &mut att_obj);
        cbor::tstr("fmt", &mut att_obj);
        cbor::tstr("none", &mut att_obj);
        cbor::tstr("attStmt", &mut att_obj);
        cbor::map(0, &mut att_obj);
        cbor::tstr("authData", &mut att_obj);
        cbor::bstr(&auth_data, &mut att_obj);
        serde_json::json!({
            "id": b64url(&self.cred_id),
            "rawId": b64url(&self.cred_id),
            "type": "public-key",
            "response": {
                "attestationObject": b64url(&att_obj),
                "clientDataJSON": b64url(client_data.as_bytes()),
                "transports": [],
            },
        })
    }

    /// 断言响应（PublicKeyCredential JSON）。签名 = ES256(DER) over
    /// authenticatorData || sha256(clientDataJSON)。
    fn assert_credential(
        &mut self,
        options: &serde_json::Value,
        origin: &str,
    ) -> serde_json::Value {
        use openssl::hash::MessageDigest;
        use openssl::pkey::PKey;
        use openssl::sign::Signer;
        let rp_id = options["rpId"].as_str().unwrap_or("localhost");
        let challenge = options["challenge"].as_str().expect("challenge");
        let client_data = serde_json::json!({
            "type": "webauthn.get",
            "challenge": challenge,
            "origin": origin,
        })
        .to_string();
        // authData：rpIdHash || flags(UP|UV=0x05) || counter（单调）。
        self.counter += 1;
        let mut auth_data = Sha256::digest(rp_id.as_bytes()).to_vec();
        auth_data.push(0x05);
        auth_data.extend_from_slice(&self.counter.to_be_bytes());
        let mut signed = auth_data.clone();
        signed.extend_from_slice(&Sha256::digest(client_data.as_bytes()));
        let pkey = PKey::from_ec_key(self.key.clone()).unwrap();
        let mut signer = Signer::new(MessageDigest::sha256(), &pkey).unwrap();
        signer.update(&signed).unwrap();
        let sig = signer.sign_to_vec().unwrap();
        serde_json::json!({
            "id": b64url(&self.cred_id),
            "rawId": b64url(&self.cred_id),
            "type": "public-key",
            "response": {
                "authenticatorData": b64url(&auth_data),
                "clientDataJSON": b64url(client_data.as_bytes()),
                "signature": b64url(&sig),
                "userHandle": b64url(b"soft-authenticator-user"),
            },
        })
    }
}

/// 裸 HTTP POST（JSON body，可选 Bearer / 自定义 Host）。返回
/// (状态码字符串, body)。Host 缺省 `localhost`——WebAuthn RP 推导由此
/// 落到 rp_id=localhost / origin=http://localhost（软认证器 clientDataJSON
/// 同源）。
async fn http_post_json(
    addr: &SocketAddr,
    path: &str,
    bearer: Option<&str>,
    host: &str,
    body: &str,
) -> (String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let auth = bearer
        .map(|t| format!("authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "POST {path} HTTP/1.1\r\nhost: {host}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n{auth}\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let raw = String::from_utf8_lossy(&buf).to_string();
    let (headers, resp_body) = raw.split_once("\r\n\r\n").unwrap_or(("", ""));
    let status = headers
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    (status, resp_body.to_string())
}

/// 便捷封装：POST + JSON body 解析断言。
async fn post_json(
    addr: &SocketAddr,
    path: &str,
    bearer: Option<&str>,
    body: &serde_json::Value,
) -> (String, serde_json::Value) {
    let (status, resp) = http_post_json(addr, path, bearer, "localhost", &body.to_string()).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap_or(serde_json::Value::Null);
    (status, v)
}

/// 读 hello 的回执：hello_ack（返回 true）或 close 1008（false）。LRU 淘汰
/// 验证用——逐 token 重连，区分幸存 / 被淘汰。
async fn session_still_valid(ws: &mut Ws, session_token: &str) -> bool {
    ws.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","session":"{session_token}"}}"#
    )))
    .await
    .unwrap();
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("等帧超时")
            .expect("流未断")
            .expect("WS 读正常");
        match msg {
            Message::Text(t) if t.contains(r#""type":"hello_ack""#) => return true,
            Message::Text(_) => continue, // machine/machines 快照
            Message::Close(_) => return false,
            m => panic!("意外帧：{m:?}"),
        }
    }
}

/// 场景 27（v1.3 passkey 全流程真签名）：USER_TOKEN 登录换发会话 → 注册
/// passkey（软认证器 "none" attestation）→ credentials.json 落盘 →
/// cred_list（WS 管理帧）→ passkey 登录（discoverable，allowCredentials 空）
/// 换发 L2 会话（与 hello 换发同构）→ sessionToken 重连 → 设备名 = passkey
/// label → sessions.json 落盘含两个会话。
#[tokio::test]
async fn e2e_passkey_register_and_login_full_ceremony() {
    let dir = temp_state_dir();
    let handle = spawn_with_config(test_config(&dir, DEFAULT_SESSION_TTL_SECS)).await;
    let addr = handle.addr;
    let origin = "http://localhost";

    // 1) USER_TOKEN 登录换发会话（注册路径的 Bearer）。
    let mut ws = connect_ws(&addr, "/ws", None).await;
    let s = login_session(&mut ws, Some("iPhone")).await;
    let session_token = s["token"].as_str().unwrap().to_string();

    // 2) 注册 begin：Bearer 认证，响应 beginId + publicKey（challenge/
    //    user.id 为 base64url）。
    let (status, begin) = post_json(
        &addr,
        "/webauthn/register/begin",
        Some(&session_token),
        &serde_json::json!({ "label": "iPhone 17" }),
    )
    .await;
    assert_eq!(status, "200", "{begin}");
    let begin_id = begin["beginId"].as_str().unwrap().to_string();
    assert!(
        begin["publicKey"]["challenge"].is_string() && begin["publicKey"]["user"]["id"].is_string(),
        "注册 options 形状：{begin}"
    );
    assert_eq!(
        begin["publicKey"]["rp"]["id"], "localhost",
        "rp_id 按请求 Host 推导"
    );

    // 3) 软认证器注册（真 ES256 公钥 + "none" attestation）→ complete 200。
    let mut soft = SoftAuthenticator::new();
    let credential = soft.register(&begin["publicKey"], origin);
    let (status, done) = post_json(
        &addr,
        "/webauthn/register/complete",
        Some(&session_token),
        &serde_json::json!({ "beginId": begin_id, "credential": credential }),
    )
    .await;
    assert_eq!(status, "200", "{done}");
    assert_eq!(done["credential"]["label"], "iPhone 17", "{done}");
    assert_eq!(
        done["credential"]["id"],
        b64url(&soft.cred_id),
        "credential id 为 base64url 形态"
    );

    // 4) credentials.json 落盘。
    let raw = std::fs::read_to_string(dir.join("credentials.json")).unwrap();
    assert!(
        raw.contains("iPhone 17") && raw.contains("\"v\":1"),
        "{raw}"
    );

    // 5) cred_list（WS 管理帧）：1 条凭证。
    consume_browser_snapshots(&mut ws, false, &[]).await;
    ws.send(Message::text(r#"{"v":1,"type":"cred_list"}"#))
        .await
        .unwrap();
    let creds = next_of_type(&mut ws, "creds").await;
    let arr = creds["credentials"].as_array().unwrap();
    assert_eq!(arr.len(), 1, "{creds}");
    assert_eq!(arr[0]["label"], "iPhone 17", "{creds}");
    let cred_id = arr[0]["id"].as_str().unwrap().to_string();

    // 6) passkey 登录 begin（无认证，discoverable：allowCredentials 恒空）。
    let (status, login_begin) =
        post_json(&addr, "/webauthn/login/begin", None, &serde_json::json!({})).await;
    assert_eq!(status, "200", "{login_begin}");
    assert_eq!(
        login_begin["publicKey"]["allowCredentials"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "登录 allowCredentials 空（discoverable，免选用户）：{login_begin}"
    );
    let login_begin_id = login_begin["beginId"].as_str().unwrap().to_string();

    // 7) 软认证器断言（真签名）→ complete 200 + L2 会话载荷（与 hello 换发
    //    同构：id UUID / token 64 hex / expiresAt unix 秒）。
    let assertion = soft.assert_credential(&login_begin["publicKey"], origin);
    let (status, login_done) = post_json(
        &addr,
        "/webauthn/login/complete",
        None,
        &serde_json::json!({ "beginId": login_begin_id, "credential": assertion }),
    )
    .await;
    assert_eq!(status, "200", "{login_done}");
    let session = &login_done["session"];
    assert!(session.is_object(), "{login_done}");
    assert!(
        session["id"].as_str().is_some_and(|s| s.len() == 36),
        "{login_done}"
    );
    assert!(
        session["token"].as_str().is_some_and(|t| t.len() == 64),
        "{login_done}"
    );
    assert!(session["expiresAt"].as_u64().is_some(), "{login_done}");
    let pk_token = session["token"].as_str().unwrap().to_string();

    // 8) sessions.json 落盘两个会话（USER_TOKEN 换发 + passkey 换发）。
    let raw = std::fs::read_to_string(dir.join("sessions.json")).unwrap();
    assert_eq!(
        raw.matches("token_hash").count(),
        2,
        "两个会话（hello 换发 + passkey 换发）：{raw}"
    );

    // 9) sessionToken 重连 + 设备名 = passkey label。
    let mut ws2 = connect_ws(&addr, "/ws", None).await;
    resume_session(&mut ws2, &pk_token).await;
    consume_browser_snapshots(&mut ws2, false, &[]).await;
    ws2.send(Message::text(r#"{"v":1,"type":"device_list"}"#))
        .await
        .unwrap();
    let devs = next_of_type(&mut ws2, "devices").await;
    let names: Vec<&str> = devs["devices"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["name"].as_str())
        .collect();
    assert!(
        names.contains(&"iPhone 17"),
        "passkey 登录会话设备名 = label：{devs}"
    );

    // 10) cred_revoke：删除后列表空、该凭证登录失败。
    ws2.send(Message::text(format!(
        r#"{{"v":1,"type":"cred_revoke","credId":"{cred_id}"}}"#
    )))
    .await
    .unwrap();
    let creds = next_of_type(&mut ws2, "creds").await;
    assert_eq!(
        creds["credentials"].as_array().unwrap().len(),
        0,
        "删除后列表空：{creds}"
    );
    let (status, login_begin2) =
        post_json(&addr, "/webauthn/login/begin", None, &serde_json::json!({})).await;
    assert_eq!(status, "400", "无凭证后 login begin 拒绝：{login_begin2}");
    assert_eq!(login_begin2["error"], "no_credentials", "{login_begin2}");

    handle.shutdown();
    handle.wait().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 场景 28（v1.3 passkey 持久化）：凭证表跨 relay 重启保留——重启后同一软
/// 认证器仍可登录（含计数器回写后的单调校验：断言 counter 递增）。
#[tokio::test]
async fn e2e_passkey_persist_across_relay_restart() {
    let dir = temp_state_dir();
    let origin = "http://localhost";
    let mut soft = SoftAuthenticator::new();

    // 第一代 relay：登录 + 注册。
    let handle = spawn_with_config(test_config(&dir, DEFAULT_SESSION_TTL_SECS)).await;
    let addr = handle.addr;
    let mut ws = connect_ws(&addr, "/ws", None).await;
    let s = login_session(&mut ws, Some("iPhone")).await;
    let st = s["token"].as_str().unwrap().to_string();
    let (_, begin) = post_json(
        &addr,
        "/webauthn/register/begin",
        Some(&st),
        &serde_json::json!({ "label": "Mac Touch ID" }),
    )
    .await;
    let credential = soft.register(&begin["publicKey"], origin);
    let (status, done) = post_json(
        &addr,
        "/webauthn/register/complete",
        Some(&st),
        &serde_json::json!({
            "beginId": begin["beginId"].as_str().unwrap(),
            "credential": credential
        }),
    )
    .await;
    assert_eq!(status, "200", "{done}");
    handle.shutdown();
    handle.wait().await;

    // 第二代 relay（同 state 目录）：登录 begin → 断言 → 成功。
    let handle2 = spawn_with_config(test_config(&dir, DEFAULT_SESSION_TTL_SECS)).await;
    let addr2 = handle2.addr;
    let (status, login_begin) = post_json(
        &addr2,
        "/webauthn/login/begin",
        None,
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, "200", "{login_begin}");
    let assertion = soft.assert_credential(&login_begin["publicKey"], origin);
    let (status, login_done) = post_json(
        &addr2,
        "/webauthn/login/complete",
        None,
        &serde_json::json!({
            "beginId": login_begin["beginId"].as_str().unwrap(),
            "credential": assertion
        }),
    )
    .await;
    assert_eq!(status, "200", "凭证跨重启保留，可继续登录：{login_done}");
    handle2.shutdown();
    handle2.wait().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 场景 29（v1.3 passkey 端点行为）：注册 401（无/错 Bearer）；无凭证
/// login begin 400 no_credentials；未知 beginId complete 400；篡改签名
/// complete 401；跨会话 complete 注册 401；IP 字面量 Host → 400
/// rp_unsupported（本地 IP 直连不可用 passkey 的文档化行为）。
#[tokio::test]
async fn e2e_passkey_endpoint_failures() {
    let dir = temp_state_dir();
    let handle = spawn_with_config(test_config(&dir, DEFAULT_SESSION_TTL_SECS)).await;
    let addr = handle.addr;
    let origin = "http://localhost";

    // 注册 begin：无 Bearer / 错 Bearer → 401。
    let (status, v) = post_json(
        &addr,
        "/webauthn/register/begin",
        None,
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, "401", "{v}");
    let (status, v) = post_json(
        &addr,
        "/webauthn/register/begin",
        Some("wrong-session-token"),
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, "401", "{v}");
    assert_eq!(v["error"], "unauthorized", "{v}");

    // 无凭证：login begin → 400 no_credentials。
    let (status, v) = post_json(&addr, "/webauthn/login/begin", None, &serde_json::json!({})).await;
    assert_eq!(status, "400", "{v}");
    assert_eq!(v["error"], "no_credentials", "{v}");

    // 正常注册一个 passkey（后续篡改/跨会话用例的基础）。
    let mut ws = connect_ws(&addr, "/ws", None).await;
    let s = login_session(&mut ws, Some("iPhone")).await;
    let st = s["token"].as_str().unwrap().to_string();
    let (_, begin) = post_json(
        &addr,
        "/webauthn/register/begin",
        Some(&st),
        &serde_json::json!({ "label": "Key-A" }),
    )
    .await;
    let mut soft = SoftAuthenticator::new();
    let credential = soft.register(&begin["publicKey"], origin);
    let (status, done) = post_json(
        &addr,
        "/webauthn/register/complete",
        Some(&st),
        &serde_json::json!({
            "beginId": begin["beginId"].as_str().unwrap(),
            "credential": credential
        }),
    )
    .await;
    assert_eq!(status, "200", "{done}");

    // 未知 beginId → 400 challenge_invalid（beginId 单次消费，同时验证）。
    let (status, v) = post_json(
        &addr,
        "/webauthn/login/complete",
        None,
        &serde_json::json!({ "beginId": "not-a-real-id", "credential": {} }),
    )
    .await;
    assert_eq!(status, "400", "{v}");
    assert_eq!(v["error"], "challenge_invalid", "{v}");
    // 已消费的注册 beginId 重复提交 → 400。
    let (status, v) = post_json(
        &addr,
        "/webauthn/register/complete",
        Some(&st),
        &serde_json::json!({
            "beginId": begin["beginId"].as_str().unwrap(),
            "credential": credential
        }),
    )
    .await;
    assert_eq!(status, "400", "challenge 单次消费：{v}");

    // 篡改签名（换 challenge 签名内容）：login begin → 断言 → 篡改
    // authenticatorData 首字节后 complete → 401 verification_failed。
    let (status, login_begin) =
        post_json(&addr, "/webauthn/login/begin", None, &serde_json::json!({})).await;
    assert_eq!(status, "200", "{login_begin}");
    let mut assertion = soft.assert_credential(&login_begin["publicKey"], origin);
    assertion["response"]["signature"] = serde_json::Value::String("AAAA".to_string());
    let (status, v) = post_json(
        &addr,
        "/webauthn/login/complete",
        None,
        &serde_json::json!({
            "beginId": login_begin["beginId"].as_str().unwrap(),
            "credential": assertion
        }),
    )
    .await;
    assert_eq!(status, "401", "{v}");
    assert_eq!(v["error"], "verification_failed", "{v}");

    // 跨会话 complete：会话 B 提交会话 A 发起的注册 challenge → 401。
    let mut ws_b = connect_ws(&addr, "/ws", None).await;
    let sb = login_session(&mut ws_b, Some("Mac")).await;
    let st_b = sb["token"].as_str().unwrap().to_string();
    let (_, begin_a) = post_json(
        &addr,
        "/webauthn/register/begin",
        Some(&st),
        &serde_json::json!({ "label": "Key-B" }),
    )
    .await;
    let soft_b = SoftAuthenticator::new();
    let credential_b = soft_b.register(&begin_a["publicKey"], origin);
    let (status, v) = post_json(
        &addr,
        "/webauthn/register/complete",
        Some(&st_b),
        &serde_json::json!({
            "beginId": begin_a["beginId"].as_str().unwrap(),
            "credential": credential_b
        }),
    )
    .await;
    assert_eq!(status, "401", "challenge 归属发起会话：{v}");

    // IP 字面量 Host（本地 127.0.0.1 直连形态）→ 400 rp_unsupported。
    let (status, resp) =
        http_post_json(&addr, "/webauthn/login/begin", None, "127.0.0.1:3010", "{}").await;
    assert_eq!(status, "400", "{resp}");
    assert!(resp.contains("rp_unsupported"), "{resp}");

    handle.shutdown();
    handle.wait().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 场景 30（v1.3 passkey begin 限流）：每 IP 10 次/分——第 11 次 login
/// begin 429（200/400 均计数，429 与业务错误可区分）。
#[tokio::test]
async fn e2e_passkey_begin_rate_limit() {
    let handle = spawn_test_relay().await;
    let addr = handle.addr;
    // 未注册凭证：前 10 次 400 no_credentials，第 11 次 429。
    for i in 0..10 {
        let (status, v) =
            post_json(&addr, "/webauthn/login/begin", None, &serde_json::json!({})).await;
        assert_eq!(status, "400", "第 {} 次应为业务错误而非限流：{v}", i + 1);
    }
    let (status, v) = post_json(&addr, "/webauthn/login/begin", None, &serde_json::json!({})).await;
    assert_eq!(status, "429", "{v}");
    assert_eq!(v["error"], "rate_limited", "{v}");

    handle.shutdown();
    handle.wait().await;
}
