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
//! v1.41.8 增补：`send_user_text`（用户自发文本镜像，peer 通道）出站帧
//! `"self":true` + outbound_log payload 同标记 + history 重放透传；该帧在
//! E2E 活跃时同被 enc 包装（见 `loopback_e2e_encrypted_roundtrip`）。
//! v1.42 增补：待审批快照 `approvals_query` → `approvals`（空表字节级
//! `items:[]`；E2E 活跃时双向 enc 包装，见 `loopback_approvals_query_snapshot`）。
//! v1.44 增补：同浏览器多标签页——同一登录会话两条连接各自握手，网关保留
//! 两把密钥、各自可收发、`e2e_close` 摘钥（见
//! `loopback_multi_tab_same_session_two_keys_and_e2e_close`）；送达回执 `ack`
//! 与网关信息 `gateway_info`（明文期字节级 + E2E 活跃时 enc 包装，见
//! `loopback_ack_and_gateway_info_frames`）。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ed25519_dalek::SigningKey;
use futures::{SinkExt, StreamExt};
use imagent_core::{
    AgentChunk, Backend, CardPhase, CardTerminal, ConvId, CoreError, OutboundCard, Platform,
    ReplyHint, Result, RunOutcome, SessionId,
};
use imagent_relay::{spawn_relay, RelayConfig, RelayHandle};
use imagent_store::Store;
use imagent_web::{GatewayCredential, WebPlatform};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
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

/// v1.6 工作空间泵用的测试 store（真实 sqlite，临时目录）。
async fn loopback_store(tag: &str) -> Store {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "imagent-web-loopback-ws-{tag}-{}-{}.sqlite",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    Store::open(&p).await.expect("open store")
}

/// 通过 validate_workdir 的测试目录（工作空间帧的合法 path）。
fn valid_ws_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("imagent-web-loopback-wsdir-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// v1.8 模型目录的 mock backend（model 恒 None——本集成测试不断言 models 段
/// 内容（纯函数与 platform 单测覆盖），run 不可达）。
struct NopBackend;

#[async_trait]
impl Backend for NopBackend {
    async fn run(
        &self,
        _ctx: imagent_core::RoundContext<'_>,
        _prompt: &str,
        _session: Option<&SessionId>,
        _chunks: mpsc::Sender<AgentChunk>,
    ) -> Result<RunOutcome> {
        Err(CoreError::Platform("test", "nop backend 不执行 run".into()))
    }

    fn name(&self) -> &'static str {
        "nop"
    }
}

/// v1.8 模型目录上下文：official 空 + mock backend——无 CLI 映射的机器上
/// models 段整段缺席（快照与 v1.7 形态一致；映射机器上段出现亦不影响本
/// 文件的 Value 级断言）。
fn nop_models() -> imagent_web::models::ModelsContext {
    imagent_web::models::ModelsContext {
        backend: Arc::new(NopBackend),
        official: imagent_core::OfficialModels::default(),
        list_models: None,
    }
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
    let (handle, platform, ws, _store) = setup_with_store().await;
    (handle, platform, ws)
}

/// [`setup`] 的 store 外露形态（v1.41.8：outbound_log 落盘行只能从库侧直接
/// 断言——store 在 setup 内创建，经此把同一库的句柄交给用例）。
async fn setup_with_store() -> (RelayHandle, WebPlatform, Ws, Store) {
    let (handle, sk) = spawn_test_relay().await;
    let addr = handle.addr;
    let store = loopback_store("setup").await;
    let platform = WebPlatform::new_with_e2e(
        format!("ws://{addr}/gateway"),
        GatewayCredential::Key(sk),
        "default".to_string(),
        MACHINE.to_string(),
        &e2e_test_key_path("platform"),
        store.clone(),
        valid_ws_dir(),
        nop_models(),
        Vec::new(), // v1.36 backends 名单：回环测试不驱动引擎选择，空名单整段缺席
        imagent_web::AsrContext::default(), // v1.40 未配置 → 快照 asr 段缺席
        300,        // v1.37 审批超时预算（approval_request 帧 timeoutSecs）
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
    (handle, platform, ws, store)
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

/// 读下一条 `type` 匹配的 JSON 帧**原文**（v1.42 字节级契约断言用；跳过
/// machine / pong 等噪声帧）。
async fn next_raw_of_type(ws: &mut Ws, want: &str) -> String {
    loop {
        let text = next_text(ws).await;
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        if v["type"].as_str() == Some(want) {
            return text;
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

/// 等 outbound_log 某 conv 落盘达到 `want` 行，返回全部行（seq 升序）。落盘
/// 是平台 spawn 的异步短任务，只能从库侧轮询观测（5s 超时防挂死）。
async fn wait_outbound_rows(
    store: &Store,
    conv: &str,
    want: usize,
) -> Vec<imagent_store::OutboundRow> {
    for _ in 0..250 {
        if let Ok(page) = store
            .list_outbound_since(conv, 0, imagent_store::OUTBOUND_RETAIN)
            .await
        {
            if page.rows.len() >= want {
                return page.rows;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("等待 outbound_log 落盘 {want} 行超时（5s）");
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

/// v1.41.8 用户自发文本镜像（`Platform::send_user_text`，peer 通道）：出站
/// text 帧带 `"self":true`（convId 原样、machine 照带）；普通 `send_text` 帧
/// 不带 self 键（旧 SPA 字节兼容）；outbound_log 落盘 payload 同带
/// `"self":true`；浏览器 `history_query` 重放出的条目仍带 `self`（刷新 / 跨
/// 设备后仍是用户气泡）。
#[tokio::test]
async fn loopback_send_user_text_self_flag_and_history_replay() {
    let (handle, p, mut br, store) = setup_with_store().await;
    let peer_conv = ConvId("peer:win-a".into());

    // 用户自发文本镜像帧：self:true。
    p.send_user_text(&peer_conv, "帮我跑下构建", &ReplyHint::None)
        .await
        .unwrap();
    let v = next_of_type(&mut br, "text").await;
    assert_eq!(v["machine"], MACHINE, "出站帧带本机 machine");
    assert_eq!(v["convId"], "peer:win-a", "peer: conv 原样出站");
    assert_eq!(v["text"], "帮我跑下构建");
    assert_eq!(v["self"], true, "用户自发文本镜像帧带 self:true：{v}");
    let frame_at = v["at"].as_u64().expect("镜像帧带 at（unix 毫秒）");

    // 等镜像行落盘再发下一条（落盘是 spawn 的异步任务，两条连发时入库先后
    // 不保证——逐条等齐才能按序断言重放条目）。
    let rows = wait_outbound_rows(&store, "peer:win-a", 1).await;
    assert_eq!(rows[0].kind, "text");
    let payload: serde_json::Value = serde_json::from_str(&rows[0].payload).unwrap();
    assert_eq!(payload["self"], true, "落盘 payload 带 self：{payload}");
    assert_eq!(payload["text"], "帮我跑下构建");
    assert_eq!(
        payload["at"].as_u64(),
        Some(frame_at),
        "落盘行 at 与实时帧同源"
    );

    // 紧随其后的普通文本（窗口回复形态）：帧与落盘行均不带 self 键。
    p.send_text(&peer_conv, "窗口的回复", &ReplyHint::None)
        .await
        .unwrap();
    let v = next_of_type(&mut br, "text").await;
    assert_eq!(v["text"], "窗口的回复");
    assert!(v.get("self").is_none(), "普通 text 帧不得带 self 键：{v}");
    let rows = wait_outbound_rows(&store, "peer:win-a", 2).await;
    let plain: serde_json::Value = serde_json::from_str(&rows[1].payload).unwrap();
    assert!(
        plain.get("self").is_none(),
        "普通文本落盘行不得带 self 键：{plain}"
    );

    // 重放：浏览器 history_query（多机模式带 machine 路由）→ history 条目
    // 透传 self（key 前缀 h:<seq>，镜像行在前、普通行在后）。
    br.send(Message::text(
        r#"{"v":1,"type":"history_query","convId":"peer:win-a","since":0,"machine":"test-mac"}"#,
    ))
    .await
    .unwrap();
    let h = next_of_type(&mut br, "history").await;
    assert_eq!(h["convId"], "peer:win-a");
    assert_eq!(h["lastSeq"], 2);
    let entries = h["entries"].as_array().expect("entries 为数组");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["kind"], "text");
    assert_eq!(entries[0]["key"], "h:1");
    assert_eq!(entries[0]["text"], "帮我跑下构建");
    assert_eq!(
        entries[0]["self"], true,
        "重放条目带 self（刷新后仍是用户气泡）"
    );
    assert_eq!(entries[1]["key"], "h:2");
    assert_eq!(entries[1]["text"], "窗口的回复");
    assert!(entries[1].get("self").is_none(), "普通重放条目不带 self 键");

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

// ---------------------------------------------------------------------------
// v1.42 待审批快照（approvals_query → approvals）：真实 WS 回环
// ---------------------------------------------------------------------------

/// 浏览器侧单帧密封（与场景 9 / 10 内联实现同构；提为文件级助手供 v1.42
/// 用例复用，既有用例保持各自的内联实现不动）。
fn e2e_seal(key: &[u8; 32], plaintext: &str) -> String {
    use base64::Engine as _;
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};

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

/// 浏览器侧单帧开启（解密出内层业务帧 JSON 原文）。
fn e2e_open(key: &[u8; 32], boxed: &str) -> String {
    use base64::Engine as _;
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};

    let data = base64::engine::general_purpose::STANDARD
        .decode(boxed)
        .unwrap();
    let (nonce, ct) = data.split_at(24);
    let cipher = XChaCha20Poly1305::new_from_slice(key).unwrap();
    let pt = cipher.decrypt(XNonce::from_slice(nonce), ct).unwrap();
    String::from_utf8(pt).unwrap()
}

/// 浏览器 E2E 握手：发 `e2e_hello{sid,ek,n1}` → 收 `e2e_ack{gk,n2}` → 派生
/// 会话密钥（握手细节断言见场景 9，此处只取密钥）。`sid` = hello_ack 的
/// session id（enc 帧定向路由键）。
async fn e2e_handshake(br: &mut Ws, sid: &str) -> [u8; 32] {
    use x25519_dalek::{PublicKey as XPublicKey, StaticSecret as XStaticSecret};

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
    let ack = next_of_type(br, "e2e_ack").await;
    let gk: [u8; 32] = hex::decode(ack["gk"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let n2: [u8; 32] = hex::decode(ack["n2"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    imagent_web::e2e::derive_session_key(
        browser_secret
            .diffie_hellman(&XPublicKey::from(gk))
            .as_bytes(),
        &n1,
        &n2,
    )
}

/// v1.42 待审批快照全链路（真实 WS 回环）：
/// - 明文期空表：`approvals_query` → `approvals` 帧**字节级** `items:[]`（键
///   不省略）；
/// - E2E 会话活跃后（生产形态）：enc 包裹的 `approvals_query` 进、enc 包裹的
///   `approvals` 出——快照含跨会话 / 跨平台前缀的待审批（convId 升序、字段
///   逐项、timeoutSecs 为剩余秒数）；中继可见的 enc 帧体不泄露摘要与内帧
///   类型（E2E 中继盲化不因新帧破口）；
/// - resolve / cancel_all 之后再查 → 收敛为空数组。
#[tokio::test]
async fn loopback_approvals_query_snapshot() {
    /// 发一条 approvals_query（enc 包裹），读到 approvals 内帧为止（先到的
    /// 其它 enc 帧——如 approval_request——解密识别后跳过；单会话恒可解），
    /// 返回 approvals 内帧原文。
    async fn query_enc(br: &mut Ws, k: &[u8; 32]) -> String {
        br.send(Message::text(
            serde_json::json!({
                "v": 1, "type": "enc", "machine": MACHINE,
                "box": e2e_seal(k, r#"{"v":1,"type":"approvals_query"}"#),
            })
            .to_string(),
        ))
        .await
        .unwrap();
        loop {
            let envelope = next_raw_of_type(br, "enc").await;
            let boxed = serde_json::from_str::<serde_json::Value>(&envelope).unwrap()["box"]
                .as_str()
                .unwrap()
                .to_string();
            let inner = e2e_open(k, &boxed);
            let is_approvals =
                serde_json::from_str::<serde_json::Value>(&inner).unwrap()["type"] == "approvals";
            if is_approvals {
                assert!(
                    !envelope.contains(r#""type":"approvals""#)
                        && !envelope.contains("npm test")
                        && !envelope.contains("/tmp/a"),
                    "中继可见的 enc 帧体不得泄露内帧类型 / 待审批摘要：{envelope}"
                );
                return inner;
            }
        }
    }

    let (handle, sk) = spawn_test_relay().await;
    let addr = handle.addr;
    let key_path = e2e_test_key_path("approvals");
    let p = WebPlatform::new_with_e2e(
        format!("ws://{addr}/gateway"),
        GatewayCredential::Key(sk),
        "default".to_string(),
        MACHINE.to_string(),
        &key_path,
        loopback_store("approvals").await,
        valid_ws_dir(),
        nop_models(),
        Vec::new(), // v1.36 backends 名单：回环测试不驱动引擎选择，空名单整段缺席
        imagent_web::AsrContext::default(), // v1.40 未配置 → 快照 asr 段缺席
        300,        // v1.37 审批超时预算（approval_request 帧 timeoutSecs）
    );
    eventually(Duration::from_secs(10), || p.is_online()).await;

    // 浏览器登录（session id 稍后供 E2E 握手用）。
    let mut br = connect_ws(&addr, "/ws", None).await;
    br.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{USER_TOKEN}"}}"#
    )))
    .await
    .unwrap();
    let ack: serde_json::Value = serde_json::from_str(&next_text(&mut br).await).unwrap();
    let sid = ack["session"]["id"].as_str().unwrap().to_string();
    let _machines = next_of_type(&mut br, "machines").await;

    // ---- 明文期空表：字节级契约（空数组，键不省略）----
    br.send(Message::text(format!(
        r#"{{"v":1,"type":"approvals_query","machine":"{MACHINE}"}}"#
    )))
    .await
    .unwrap();
    let raw = next_raw_of_type(&mut br, "approvals").await;
    let empty_snapshot =
        format!(r#"{{"v":1,"type":"approvals","machine":"{MACHINE}","items":[]}}"#);
    assert_eq!(raw, empty_snapshot, "空表快照须逐字节一致");

    // ---- E2E 握手：此后业务帧双向 enc 包裹（生产形态）----
    let k = e2e_handshake(&mut br, &sid).await;

    // 空表（enc）：解密后仍是字节级空数组。
    assert_eq!(query_enc(&mut br, &k).await, empty_snapshot);

    // 两条待审批：跨会话 + 跨平台前缀。
    p.send_permission_ask(
        &ConvId("web:s-ab12".into()),
        "req-1",
        "Bash",
        r#"{"command":"npm test"}"#,
        &ReplyHint::None,
    )
    .await
    .unwrap();
    p.send_permission_ask(
        &ConvId("local:work".into()),
        "req-2",
        "Write",
        r#"{"file_path":"/tmp/a"}"#,
        &ReplyHint::None,
    )
    .await
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&query_enc(&mut br, &k).await).unwrap();
    assert_eq!(v["v"], 1);
    assert_eq!(v["machine"], MACHINE, "快照带本机 machine");
    let items = v["items"].as_array().expect("items 为数组");
    assert_eq!(items.len(), 2, "{v}");
    // convId 升序（local:work < web:s-ab12）。
    assert_eq!(items[0]["convId"], "local:work");
    assert_eq!(items[0]["requestId"], "req-2");
    assert_eq!(items[0]["tool"], "Write");
    assert_eq!(items[0]["inputSummary"], "/tmp/a", "已渲染摘要");
    assert_eq!(items[1]["convId"], "web:s-ab12");
    assert_eq!(items[1]["requestId"], "req-1");
    assert_eq!(items[1]["tool"], "Bash");
    assert_eq!(items[1]["inputSummary"], "npm test");
    // timeoutSecs = 剩余秒数（预算 300 − 已过整秒；留 10s 余量防慢机器）。
    for it in items {
        let t = it["timeoutSecs"].as_u64().unwrap();
        assert!((290..=300).contains(&t), "剩余秒数：{it}");
    }

    // resolve（用户决定）一条 → 只剩另一条。
    p.resolve_permission_ask(
        &ConvId("web:s-ab12".into()),
        "req-1",
        &imagent_core::PermissionReply {
            allow: true,
            always: false,
            message: None,
            raw_text: None,
            cancelled: false,
        },
    )
    .await
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&query_enc(&mut br, &k).await).unwrap();
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{v}");
    assert_eq!(items[0]["requestId"], "req-2");

    // cancel_all 另一会话 → 收敛为空数组。
    p.cancel_all_permission_asks(&ConvId("local:work".into()))
        .await
        .unwrap();
    assert_eq!(query_enc(&mut br, &k).await, empty_snapshot);

    let _ = std::fs::remove_file(&key_path);
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
        loopback_store("token").await,
        valid_ws_dir(),
        nop_models(),
        Vec::new(), // v1.36 backends 名单：回环测试不驱动引擎选择，空名单整段缺席
        imagent_web::AsrContext::default(), // v1.40 未配置 → 快照 asr 段缺席
        300,        // v1.37 审批超时预算（approval_request 帧 timeoutSecs）
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
        loopback_store("e2e").await,
        valid_ws_dir(),
        nop_models(),
        Vec::new(), // v1.36 backends 名单：回环测试不驱动引擎选择，空名单整段缺席
        imagent_web::AsrContext::default(), // v1.40 未配置 → 快照 asr 段缺席
        300,        // v1.37 审批超时预算（approval_request 帧 timeoutSecs）
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

    // ---- v1.41.8：send_user_text 的 self 标记帧同走 enc（中继盲化不因新
    // 字段破口）——中继可见帧体不含正文与 self 标记，解密后内帧带 self ----
    p.send_user_text(
        &ConvId("peer:win-a".into()),
        "帮我跑下构建",
        &ReplyHint::None,
    )
    .await
    .unwrap();
    let v = next_of_type(&mut br, "enc").await;
    let raw = v.to_string();
    assert!(
        !raw.contains("帮我跑下构建") && !raw.contains(r#""self""#) && !raw.contains(r#""text""#),
        "中继可见的 enc 帧体不得泄露正文 / self 标记 / 内帧类型：{raw}"
    );
    let inner: serde_json::Value = serde_json::from_str(&open(&k, v["box"].as_str().unwrap()))
        .expect("self 标记帧须能被会话密钥解密");
    assert_eq!(inner["type"], "text");
    assert_eq!(inner["self"], true, "解密后内帧带 self:true：{inner}");
    assert_eq!(inner["convId"], "peer:win-a");
    assert_eq!(inner["text"], "帮我跑下构建");

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

/// 场景 10（协议 v1.6 工作空间帧 × E2E）：E2E 会话活跃机器下，工作空间
/// 帧双向全部走 enc——入站 workspace_save / conv_workdir_set（合法与非法）
/// 包 enc 投递；出站 workspaces 快照 / conv_workdir 增量 / error 复原帧
/// 逐 sid 加密回浏览器；/cd 的 on_workdir_changed 通知同样 enc。
#[tokio::test]
async fn loopback_workspace_frames_e2e_encrypted() {
    use base64::Engine as _;
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};
    use x25519_dalek::{PublicKey as XPublicKey, StaticSecret as XStaticSecret};

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
    fn open(key: &[u8; 32], boxed: &str) -> String {
        let data = base64::engine::general_purpose::STANDARD
            .decode(boxed)
            .unwrap();
        let (nonce, ct) = data.split_at(24);
        let cipher = XChaCha20Poly1305::new_from_slice(key).unwrap();
        let pt = cipher.decrypt(XNonce::from_slice(nonce), ct).unwrap();
        String::from_utf8(pt).unwrap()
    }
    /// 读下一条 enc 帧并解密为 JSON（跳过其它帧）。
    async fn next_enc_json(ws: &mut Ws, k: &[u8; 32]) -> serde_json::Value {
        loop {
            let v = next_of_type(ws, "enc").await;
            if let Some(pt) = v["box"].as_str() {
                // 解不开（其它会话的 box）→ 继续等；本测试单会话恒可解。
                return serde_json::from_str(&open(k, pt)).unwrap_or(v);
            }
        }
    }

    let (handle, sk) = spawn_test_relay().await;
    let addr = handle.addr;
    let key_path = e2e_test_key_path("ws-e2e");
    let store = loopback_store("ws-e2e").await;
    let default_dir = valid_ws_dir();
    let ws_dir = valid_ws_dir();
    let p = WebPlatform::new_with_e2e(
        format!("ws://{addr}/gateway"),
        GatewayCredential::Key(sk),
        "default".to_string(),
        MACHINE.to_string(),
        &key_path,
        store.clone(),
        default_dir.clone(),
        nop_models(),
        Vec::new(), // v1.36 backends 名单：回环测试不驱动引擎选择，空名单整段缺席
        imagent_web::AsrContext::default(), // v1.40 未配置 → 快照 asr 段缺席
        300,        // v1.37 审批超时预算（approval_request 帧 timeoutSecs）
    );
    eventually(Duration::from_secs(10), || p.is_online()).await;

    // 浏览器登录 + E2E 握手（与场景 9 同构，握手细节断言从简——那边已钉）。
    let mut br = connect_ws(&addr, "/ws", None).await;
    br.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{USER_TOKEN}"}}"#
    )))
    .await
    .unwrap();
    let ack: serde_json::Value = serde_json::from_str(&next_text(&mut br).await).unwrap();
    let sid = ack["session"]["id"].as_str().unwrap().to_string();
    let _machines = next_of_type(&mut br, "machines").await;

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
    let e2e_ack = next_of_type(&mut br, "e2e_ack").await;
    let gk: [u8; 32] = hex::decode(e2e_ack["gk"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let n2: [u8; 32] = hex::decode(e2e_ack["n2"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let k = imagent_web::e2e::derive_session_key(
        browser_secret
            .diffie_hellman(&XPublicKey::from(gk))
            .as_bytes(),
        &n1,
        &n2,
    );

    // ---- 入站 workspace_save（enc 包裹）→ 出站 workspaces 快照（enc 包裹）。
    br.send(Message::text(
        serde_json::json!({
            "v": 1, "type": "enc", "machine": MACHINE,
            "box": seal(&k, &serde_json::json!({
                "v": 1, "type": "workspace_save",
                "name": "proj", "path": ws_dir.to_string_lossy(),
            }).to_string()),
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let snap = next_enc_json(&mut br, &k).await;
    assert_eq!(snap["type"], "workspaces", "保存后回快照（enc）：{snap}");
    assert_eq!(
        snap["machine"], MACHINE,
        "快照带本机 machine（v1.1）：{snap}"
    );
    assert_eq!(snap["default"], default_dir.to_string_lossy().as_ref());
    let ws_list = snap["workspaces"].as_array().unwrap();
    assert_eq!(ws_list.len(), 1);
    assert_eq!(ws_list[0]["name"], "proj");
    assert_eq!(ws_list[0]["path"], ws_dir.to_string_lossy().as_ref());
    assert!(
        ws_list[0].get("lastUsedAt").is_none(),
        "刚保存 lastUsedAt 缺席"
    );

    // ---- 入站 conv_workdir_set（合法，enc）→ 出站 conv_workdir 增量 +
    // workspaces 快照（均 enc）；KV 与 /cd 同键；命中空间 bump。
    br.send(Message::text(
        serde_json::json!({
            "v": 1, "type": "enc", "machine": MACHINE,
            "box": seal(&k, &serde_json::json!({
                "v": 1, "type": "conv_workdir_set",
                "convId": "web:work", "path": ws_dir.to_string_lossy(),
            }).to_string()),
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let delta = next_enc_json(&mut br, &k).await;
    assert_eq!(
        delta["type"], "conv_workdir",
        "成功额外推增量帧（enc）：{delta}"
    );
    assert_eq!(delta["machine"], MACHINE, "增量帧带本机 machine（v1.1）");
    assert_eq!(delta["convId"], "web:work");
    assert_eq!(delta["workdir"], ws_dir.to_string_lossy().as_ref());
    let snap = next_enc_json(&mut br, &k).await;
    assert_eq!(snap["type"], "workspaces");
    assert_eq!(
        snap["convs"]["web:work"],
        ws_dir.to_string_lossy().as_ref(),
        "快照 convs 反映新 workdir"
    );
    assert!(
        snap["workspaces"][0]["lastUsedAt"].is_u64(),
        "命中空间 path 须 bump lastUsedAt"
    );
    assert_eq!(
        store
            .get_config("workdir:web:work")
            .await
            .unwrap()
            .as_deref(),
        Some(ws_dir.to_string_lossy().as_ref()),
        "与 /cd 同键落库"
    );

    // ---- 入站 conv_workdir_set（非法路径，enc）→ error 帧（enc）+ 快照不变。
    br.send(Message::text(
        serde_json::json!({
            "v": 1, "type": "enc", "machine": MACHINE,
            "box": seal(&k, r#"{"v":1,"type":"conv_workdir_set","convId":"web:work","path":"/"}"#),
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let err = next_enc_json(&mut br, &k).await;
    assert_eq!(err["type"], "error", "校验失败回 error 帧（enc）：{err}");
    assert_eq!(err["code"], "workspace_invalid");
    assert_eq!(err["machine"], MACHINE, "error 帧带本机 machine（v1.1）");
    assert!(!err["message"].as_str().unwrap().is_empty());
    let snap = next_enc_json(&mut br, &k).await;
    assert_eq!(
        snap["convs"]["web:work"],
        ws_dir.to_string_lossy().as_ref(),
        "失败后快照不变"
    );

    // ---- /cd 通知路径（on_workdir_changed）→ conv_workdir 增量帧（enc）。
    p.on_workdir_changed(&ConvId("local:work".into()), "/tmp/imagent-cd-ws");
    let delta = next_enc_json(&mut br, &k).await;
    assert_eq!(delta["type"], "conv_workdir");
    assert_eq!(
        delta["machine"], MACHINE,
        "/cd 增量帧带本机 machine（v1.1）"
    );
    assert_eq!(delta["convId"], "local:work");
    assert_eq!(delta["workdir"], "/tmp/imagent-cd-ws");

    let _ = std::fs::remove_file(&key_path);
    handle.shutdown();
    handle.wait().await;
    drop(p);
}

/// 场景 11（协议 v1.40 语音转写 × E2E）：E2E 会话活跃机器下，ASR 全链路
/// 走 enc——入站 asr_request（enc 包裹，b64 音频）→ 网关 spawn mock
/// whisper（真进程：校验临时 WAV 存在非空 + printf 两行识别文本）→ 出站
/// asr_result（enc 包裹：text 多行合并、durationMs>0、带 machine）；快照
/// 能力位随配置点亮（workspace_list → workspaces 帧 asr:true）。
#[tokio::test]
async fn loopback_asr_request_result_e2e_encrypted() {
    use base64::Engine as _;
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};
    use x25519_dalek::{PublicKey as XPublicKey, StaticSecret as XStaticSecret};

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
    fn open(key: &[u8; 32], boxed: &str) -> String {
        let data = base64::engine::general_purpose::STANDARD
            .decode(boxed)
            .unwrap();
        let (nonce, ct) = data.split_at(24);
        let cipher = XChaCha20Poly1305::new_from_slice(key).unwrap();
        let pt = cipher.decrypt(XNonce::from_slice(nonce), ct).unwrap();
        String::from_utf8(pt).unwrap()
    }
    /// 读下一条 enc 帧并解密为 JSON（跳过其它帧；解开非 JSON 的继续等）。
    async fn next_enc_json(ws: &mut Ws, k: &[u8; 32]) -> serde_json::Value {
        loop {
            let v = next_of_type(ws, "enc").await;
            if let Some(pt) = v["box"].as_str() {
                return serde_json::from_str(&open(k, pt)).unwrap_or(v);
            }
        }
    }

    // mock whisper：校验 `-f` 的临时 WAV 存在且非空（泵把 -f 放最后），输出
    // 两行识别文本（验证网关多行合并）。
    let asr_dir = std::env::temp_dir().join(format!(
        "imagent-web-loopback-asr-{}-{}",
        std::process::id(),
        rand::random::<u32>()
    ));
    std::fs::create_dir_all(&asr_dir).unwrap();
    let bin = asr_dir.join("whisper-mock");
    std::fs::write(
        &bin,
        "#!/bin/sh\nfor last; do :; done\ntest -s \"$last\" || exit 3\nprintf '你好 世界\\n第二行\\n'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let asr = imagent_web::AsrContext {
        bin: Some(bin.to_string_lossy().into_owned()),
        model: Some("/fake/ggml-medium.bin".into()),
    };

    let (handle, sk) = spawn_test_relay().await;
    let addr = handle.addr;
    let key_path = e2e_test_key_path("asr-e2e");
    let p = WebPlatform::new_with_e2e(
        format!("ws://{addr}/gateway"),
        GatewayCredential::Key(sk),
        "default".to_string(),
        MACHINE.to_string(),
        &key_path,
        loopback_store("asr-e2e").await,
        valid_ws_dir(),
        nop_models(),
        Vec::new(),
        asr, // v1.40 mock whisper 注入：快照 asr 段点亮 + 泵 spawn 本脚本
        300,
    );
    eventually(Duration::from_secs(10), || p.is_online()).await;

    // 浏览器登录 + E2E 握手（与场景 10 同构，细节断言从简）。
    let mut br = connect_ws(&addr, "/ws", None).await;
    br.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{USER_TOKEN}"}}"#
    )))
    .await
    .unwrap();
    let ack: serde_json::Value = serde_json::from_str(&next_text(&mut br).await).unwrap();
    let sid = ack["session"]["id"].as_str().unwrap().to_string();
    let _machines = next_of_type(&mut br, "machines").await;

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
    let e2e_ack = next_of_type(&mut br, "e2e_ack").await;
    let gk: [u8; 32] = hex::decode(e2e_ack["gk"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let n2: [u8; 32] = hex::decode(e2e_ack["n2"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let k = imagent_web::e2e::derive_session_key(
        browser_secret
            .diffie_hellman(&XPublicKey::from(gk))
            .as_bytes(),
        &n1,
        &n2,
    );

    // ---- 入站 asr_request（enc）→ 出站 asr_result（enc）。
    br.send(Message::text(
        serde_json::json!({
            "v": 1, "type": "enc", "machine": MACHINE,
            "box": seal(&k, &serde_json::json!({
                "v": 1, "type": "asr_request",
                "audio": base64::engine::general_purpose::STANDARD.encode(b"RIFF....WAVE"),
                "durationMs": 2500,
            }).to_string()),
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let result = next_enc_json(&mut br, &k).await;
    assert_eq!(
        result["type"], "asr_result",
        "mock whisper 全链回帧（enc 解出）：{result}"
    );
    assert_eq!(
        result["machine"], MACHINE,
        "asr_result 带本机 machine（v1.1）"
    );
    assert_eq!(
        result["text"], "你好 世界 第二行",
        "stdout 多行合并单行空格拼接"
    );
    assert!(
        result["durationMs"].as_u64().is_some_and(|n| n > 0),
        "durationMs > 0：{result}"
    );

    // ---- workspace_list（enc）→ 快照（enc）：能力位随 mock 配置点亮。
    br.send(Message::text(
        serde_json::json!({
            "v": 1, "type": "enc", "machine": MACHINE,
            "box": seal(&k, r#"{"v":1,"type":"workspace_list"}"#),
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let snap = next_enc_json(&mut br, &k).await;
    assert_eq!(snap["type"], "workspaces", "{snap}");
    assert_eq!(
        snap["asr"], true,
        "已配置 mock whisper → 快照 asr:true：{snap}"
    );

    // 临时文件生命周期：泵处理收尾即删（目录只剩 mock 脚本）。
    let leftovers: Vec<String> = std::fs::read_dir(&asr_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("asr-") && n.ends_with(".wav"))
        .collect();
    assert!(leftovers.is_empty(), "临时 WAV 已删除：{leftovers:?}");

    let _ = std::fs::remove_file(&key_path);
    std::fs::remove_dir_all(&asr_dir).ok();
    handle.shutdown();
    handle.wait().await;
    drop(p);
}

// ---------------------------------------------------------------------------
// v1.10 push_notify 全链路：平台埋点（终态卡）→ client 明文管理帧 → relay
// → mock push 服务投递（真实 aes128gcm 加密 + VAPID Authorization）
// ---------------------------------------------------------------------------

/// 真实 P-256 订阅密钥（web-push crate 文档示例密钥——合法曲线点，服务端
/// 加密可过；本地 mock 只验证投递到达，不解密，无需对应私钥）。
const PUSH_P256DH: &str =
    "BLMbF9ffKBiWQLCKvTHb6LO8Nb6dcUh6TItC455vu2kElga6PQvUmaFyCdykxY2nOSSL3yKgfbmFLRTUaGv4yV8";
const PUSH_AUTH: &str = "xS03Fi5ErfTNH_l9WHE9Ig";

/// 本地 mock push 服务（对齐 relay e2e 同名夹具：收 POST → 按预设状态码
/// 应答 → 上报 (authorization, content-encoding, body_len)）。
async fn spawn_mock_push_service(
    status: u16,
) -> (SocketAddr, mpsc::UnboundedReceiver<(String, String, usize)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = Vec::new();
                let head_end = loop {
                    let mut chunk = [0u8; 4096];
                    let n = stream.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "mock push 服务连接早断");
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos;
                    }
                };
                let headers = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let content_length = headers
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.trim()
                            .eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                let body_start = head_end + 4;
                while buf.len() < body_start + content_length {
                    let mut chunk = [0u8; 4096];
                    let n = stream.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "mock push 服务 body 早断");
                    buf.extend_from_slice(&chunk[..n]);
                }
                let grab = |name: &str| {
                    headers
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.trim()
                                .eq_ignore_ascii_case(name)
                                .then(|| v.trim().to_string())
                        })
                        .unwrap_or_default()
                };
                let _ = tx.send((
                    grab("authorization"),
                    grab("content-encoding"),
                    content_length,
                ));
                let resp = format!(
                    "HTTP/1.1 {status} TEST\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    (addr, rx)
}

/// 裸 HTTP POST/GET（带可选 Bearer），返回 (状态码, body)。
async fn http_json(
    addr: &SocketAddr,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<&str>,
) -> (String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let auth = bearer
        .map(|t| format!("authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let payload = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n{auth}\r\n{payload}",
        payload.len()
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

/// 全链路：登录 → HTTP 订阅（loopback mock）→ 断开浏览器（离线）→ 平台
/// 终态卡 → push_notify 帧 → 中继投递（mock 收到 vapid + aes128gcm POST）。
#[tokio::test]
async fn loopback_push_notify_full_chain() {
    // 本机可能配了系统代理（http_proxy env）——loopback mock 须经 no_proxy
    // 直连（reqwest 默认读代理 env）。
    std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
    std::env::set_var("no_proxy", "127.0.0.1,localhost");

    let (handle, sk) = spawn_test_relay().await;
    let addr = handle.addr;
    let platform = WebPlatform::new_with_e2e(
        format!("ws://{addr}/gateway"),
        GatewayCredential::Key(sk),
        "default".to_string(),
        MACHINE.to_string(),
        &e2e_test_key_path("push"),
        loopback_store("push").await,
        valid_ws_dir(),
        nop_models(),
        Vec::new(), // v1.36 backends 名单：回环测试不驱动引擎选择，空名单整段缺席
        imagent_web::AsrContext::default(), // v1.40 未配置 → 快照 asr 段缺席
        300,        // v1.37 审批超时预算（approval_request 帧 timeoutSecs）
    );
    eventually(Duration::from_secs(10), || platform.is_online()).await;

    // 浏览器登录拿 sessionToken。
    let mut ws = connect_ws(&addr, "/ws", None).await;
    ws.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{USER_TOKEN}"}}"#
    )))
    .await
    .unwrap();
    let ack = next_text(&mut ws).await;
    let v: serde_json::Value = serde_json::from_str(&ack).unwrap();
    let token = v["session"]["token"].as_str().unwrap().to_string();

    // HTTP 订阅（端点 = loopback mock push 服务；relay 校验的 loopback http
    // 豁免面）。此刻会话在线（WS 活跃）——但在线判定在投递时进行。
    let (push_addr, mut push_rx) = spawn_mock_push_service(200).await;
    let ep = format!("http://{push_addr}/fcm/send/full-chain");
    let (status, resp) = http_json(
        &addr,
        "POST",
        "/push/subscribe",
        Some(&token),
        Some(
            &serde_json::json!({
                "endpoint": ep,
                "keys": {"p256dh": PUSH_P256DH, "auth": PUSH_AUTH},
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(status, "200", "{resp}");
    // status 端点确认订阅生效 + VAPID 公钥下发。
    let (status, body) = http_json(&addr, "GET", "/push/status", Some(&token), None).await;
    assert_eq!(status, "200", "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["subscriptions"], 1, "{body}");
    assert_eq!(
        v["vapidPublicKey"].as_str().unwrap().len(),
        87,
        "VAPID 公钥 65B base64url：{body}"
    );

    // 断开浏览器 WS → 订阅会话离线（SPA 在线免打扰解除）。
    drop(ws);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 平台终态卡（Done）→ 埋点 → push_notify 明文管理帧 → relay → 投递。
    // card 帧本体经 E2E seal 发给（不存在的）浏览器，push_notify 走明文
    // 管理帧族——两端路径互不干扰。
    platform
        .send_card(
            &conv(),
            &sample_card("全链路", CardTerminal::Done),
            &ReplyHint::None,
        )
        .await
        .unwrap()
        .unwrap();
    let (authorization, encoding, body_len) =
        tokio::time::timeout(Duration::from_secs(10), push_rx.recv())
            .await
            .expect("离线订阅应收到投递")
            .expect("channel 开着");
    assert_eq!(encoding, "aes128gcm", "RFC8188 aes128gcm 内容编码");
    assert!(
        authorization.starts_with("vapid t="),
        "VAPID Authorization 头：{authorization}"
    );
    assert!(body_len > 0, "加密载荷非空");

    handle.shutdown();
    handle.wait().await;
    drop(platform);
}

/// v1.42 窗口回信推送全链路：登录 → HTTP 订阅 → 断开浏览器（离线）→ 平台
/// `on_peer_reply`（dispatch 的 `handle_peer_inbound` 在窗口回信落 `peer:`
/// 会话后调用）→ `push_notify{kind=peer_reply, convId=peer:<窗口名>}` 明文
/// 管理帧 → **真实 relay 接受新 kind** 并投递（mock push 服务收到 vapid +
/// aes128gcm POST）。钉的是跨 crate 契约：网关侧 `PushNotifyKind::as_str`
/// 与中继侧白名单逐字一致——任一侧漂移，本用例是唯一会在真实链路上转红的。
#[tokio::test]
async fn loopback_peer_reply_push_notify_full_chain() {
    std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
    std::env::set_var("no_proxy", "127.0.0.1,localhost");

    let (handle, sk) = spawn_test_relay().await;
    let addr = handle.addr;
    let platform = WebPlatform::new_with_e2e(
        format!("ws://{addr}/gateway"),
        GatewayCredential::Key(sk),
        "default".to_string(),
        MACHINE.to_string(),
        &e2e_test_key_path("peer-reply-push"),
        loopback_store("peer-reply-push").await,
        valid_ws_dir(),
        nop_models(),
        Vec::new(), // v1.36 backends 名单：回环测试不驱动引擎选择，空名单整段缺席
        imagent_web::AsrContext::default(), // v1.40 未配置 → 快照 asr 段缺席
        300,        // v1.37 审批超时预算（approval_request 帧 timeoutSecs）
    );
    eventually(Duration::from_secs(10), || platform.is_online()).await;

    // 浏览器登录拿 sessionToken + 订阅（端点 = loopback mock push 服务）。
    let mut ws = connect_ws(&addr, "/ws", None).await;
    ws.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{USER_TOKEN}"}}"#
    )))
    .await
    .unwrap();
    let ack = next_text(&mut ws).await;
    let v: serde_json::Value = serde_json::from_str(&ack).unwrap();
    let token = v["session"]["token"].as_str().unwrap().to_string();
    let (push_addr, mut push_rx) = spawn_mock_push_service(200).await;
    let ep = format!("http://{push_addr}/fcm/send/peer-reply");
    let (status, resp) = http_json(
        &addr,
        "POST",
        "/push/subscribe",
        Some(&token),
        Some(
            &serde_json::json!({
                "endpoint": ep,
                "keys": {"p256dh": PUSH_P256DH, "auth": PUSH_AUTH},
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(status, "200", "{resp}");

    // 断开浏览器 WS → 订阅会话离线（SPA 在线免打扰解除）。
    drop(ws);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 窗口回信：钩子 → push_notify(kind=peer_reply) 明文管理帧 → relay 投递。
    platform
        .on_peer_reply(&ConvId("peer:win-a".into()), "窗口的回复")
        .await;
    let (authorization, encoding, body_len) =
        tokio::time::timeout(Duration::from_secs(10), push_rx.recv())
            .await
            .expect("离线订阅应收到窗口回信推送（relay 须接受 kind=peer_reply）")
            .expect("channel 开着");
    assert_eq!(encoding, "aes128gcm", "RFC8188 aes128gcm 内容编码");
    assert!(
        authorization.starts_with("vapid t="),
        "VAPID Authorization 头：{authorization}"
    );
    assert!(body_len > 0, "加密载荷非空");
    // 恰一次投递（钩子每次回信只推一条）。
    assert!(
        tokio::time::timeout(Duration::from_millis(200), push_rx.recv())
            .await
            .is_err(),
        "on_peer_reply 恰投递一次"
    );

    handle.shutdown();
    handle.wait().await;
    drop(platform);
}

// ---------------------------------------------------------------------------
// v1.43 本地窗口看板（windows_query / window_watch → windows）+ 窗口事件推送
// ---------------------------------------------------------------------------

/// v1.43 测试用 peer 共享态：已建基线的两窗口快照（startedAt 倒序：win-a 在
/// 前）+ 钩子已装（生产由 PeerServer 的轮询任务写入，形态相同）。
fn windows_state() -> Arc<imagent_core::peer::PeerState> {
    use imagent_core::peer::{PeerState, WindowInfo, WindowStatus};
    let state = Arc::new(PeerState::new());
    let w = |name: &str, status: WindowStatus, started: i64| WindowInfo {
        name: name.into(),
        status,
        waiting_for: None,
        cwd: format!("/tmp/{name}"),
        started_at: Some(started),
        status_updated_at: Some(1_000),
        watched: false,
    };
    let _ = state.apply_snapshot(
        &[
            w("win-a", WindowStatus::Idle, 200),
            w("win-b", WindowStatus::Busy, 100),
        ],
        std::time::Instant::now(),
    );
    state.set_hook_installed(true);
    state
}

/// `windows_state()` 对应的 windows 帧（machine = 本平台自报的 test-mac）。
const WINDOWS_FRAME: &str = r#"{"v":1,"type":"windows","machine":"test-mac","hookInstalled":true,"items":[{"name":"win-a","status":"idle","cwd":"/tmp/win-a","startedAt":200,"statusUpdatedAt":1000,"watched":false},{"name":"win-b","status":"busy","cwd":"/tmp/win-b","startedAt":100,"statusUpdatedAt":1000,"watched":false}]}"#;

/// v1.43：发一条 enc 包裹的业务帧，读到内层类型为 `want` 的 enc 帧为止，返回
/// 内层 JSON 原文；断言中继可见的 enc 信封不泄露内帧类型 / 窗口名 / cwd
/// （E2E 中继盲化不因新帧破口）。
async fn send_enc_until_inner(br: &mut Ws, k: &[u8; 32], frame: &str, want: &str) -> String {
    br.send(Message::text(
        serde_json::json!({
            "v": 1, "type": "enc", "machine": MACHINE,
            "box": e2e_seal(k, frame),
        })
        .to_string(),
    ))
    .await
    .unwrap();
    loop {
        let envelope = next_raw_of_type(br, "enc").await;
        let boxed = serde_json::from_str::<serde_json::Value>(&envelope).unwrap()["box"]
            .as_str()
            .unwrap()
            .to_string();
        let inner = e2e_open(k, &boxed);
        let is_want = serde_json::from_str::<serde_json::Value>(&inner).unwrap()["type"] == want;
        if is_want {
            assert!(
                !envelope.contains(r#""type":"windows""#)
                    && !envelope.contains("win-a")
                    && !envelope.contains("/tmp/win-"),
                "中继可见的 enc 帧体不得泄露内帧类型 / 窗口名 / cwd：{envelope}"
            );
            return inner;
        }
    }
}

/// v1.43 本地窗口看板全链路（真实 WS 回环）：
/// - peer 通道未启用（web 平台未注入共享态）：`windows_query` **不回任何帧**
///   ——前端据此不显示看板；
/// - 注入共享态后，明文期 `windows_query` → `windows` 帧**字节级**（machine =
///   本机、hookInstalled、items 已排序）；
/// - E2E 会话活跃后（生产形态）：enc 包裹的 `windows_query` 进、enc 包裹的
///   `windows` 出，中继只见密文；
/// - `window_watch` 经 client 层改共享态并重发快照：回帧里 `watched` 收敛为
///   true / false（所有在线设备以网关为准）；关注变化经共享态的变化回调送达
///   持久化方（名单归网关所有、跨设备全局——前端不存名单、不对账）；畸形
///   `window_watch` 不改状态、不回帧、不触发持久化。
#[tokio::test]
async fn loopback_windows_query_and_watch_e2e_encrypted() {
    let (handle, sk) = spawn_test_relay().await;
    let addr = handle.addr;
    let key_path = e2e_test_key_path("windows");
    let p = WebPlatform::new_with_e2e(
        format!("ws://{addr}/gateway"),
        GatewayCredential::Key(sk),
        "default".to_string(),
        MACHINE.to_string(),
        &key_path,
        loopback_store("windows").await,
        valid_ws_dir(),
        nop_models(),
        Vec::new(), // v1.36 backends 名单：回环测试不驱动引擎选择，空名单整段缺席
        imagent_web::AsrContext::default(), // v1.40 未配置 → 快照 asr 段缺席
        300,        // v1.37 审批超时预算（approval_request 帧 timeoutSecs）
    );
    eventually(Duration::from_secs(10), || p.is_online()).await;

    // 浏览器登录（session id 稍后供 E2E 握手用）。
    let mut br = connect_ws(&addr, "/ws", None).await;
    br.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{USER_TOKEN}"}}"#
    )))
    .await
    .unwrap();
    let ack: serde_json::Value = serde_json::from_str(&next_text(&mut br).await).unwrap();
    let sid = ack["session"]["id"].as_str().unwrap().to_string();
    let _machines = next_of_type(&mut br, "machines").await;

    // ---- peer 通道未启用：不回任何帧（前端收不到 windows 即不显示看板）----
    br.send(Message::text(format!(
        r#"{{"v":1,"type":"windows_query","machine":"{MACHINE}"}}"#
    )))
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), next_of_type(&mut br, "windows"))
            .await
            .is_err(),
        "peer 未启用的网关不得回 windows 帧"
    );

    // ---- 注入共享态（生产：PeerServer::start 成功后 main 后填）----
    // 关注名单由网关持久化（跨设备全局状态）：window_watch 经真实 client 路径改
    // 共享态后，变化回调（生产装配为落盘泵）必须收到排序后的完整名单。
    let state = windows_state();
    let persisted: Arc<std::sync::Mutex<Vec<Vec<String>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let persisted_cb = persisted.clone();
    state.set_watch_changed_callback(Some(Arc::new(move |names: Vec<String>| {
        persisted_cb.lock().unwrap().push(names);
    })));
    p.set_peer_state(state.clone());

    // ---- 明文期：字节级契约 ----
    br.send(Message::text(format!(
        r#"{{"v":1,"type":"windows_query","machine":"{MACHINE}"}}"#
    )))
    .await
    .unwrap();
    assert_eq!(
        next_raw_of_type(&mut br, "windows").await,
        WINDOWS_FRAME,
        "windows_query 应答须与契约逐字节一致"
    );

    // ---- E2E 握手：此后业务帧双向 enc 包裹（生产形态）----
    let k = e2e_handshake(&mut br, &sid).await;
    assert_eq!(
        send_enc_until_inner(&mut br, &k, r#"{"v":1,"type":"windows_query"}"#, "windows").await,
        WINDOWS_FRAME,
        "enc 包裹的 windows_query 进、enc 包裹的 windows 出"
    );

    // ---- window_watch：关注 → 重发的快照里 watched 收敛为 true ----
    let inner = send_enc_until_inner(
        &mut br,
        &k,
        r#"{"v":1,"type":"window_watch","machine":"test-mac","name":"win-b","watch":true}"#,
        "windows",
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&inner).unwrap();
    assert!(state.is_watched("win-b"), "关注名单写进共享态");
    assert_eq!(
        *persisted.lock().unwrap(),
        vec![vec!["win-b".to_string()]],
        "关注变化经回调送达持久化方（入参 = 排序后的完整名单）"
    );
    let items = v["items"].as_array().expect("items 为数组");
    assert_eq!(items.len(), 2, "{v}");
    assert_eq!(items[0]["name"], "win-a");
    assert_eq!(items[0]["watched"], false, "未关注的窗口不受影响：{v}");
    assert_eq!(items[1]["name"], "win-b");
    assert_eq!(
        items[1]["watched"], true,
        "window_watch 后回的快照 watched:true：{v}"
    );
    // 主动推送（轮询发现变化）同样是 enc 包裹的 windows 帧。
    let pushed = state.windows();
    p.on_windows_changed(&pushed);
    let inner = next_enc_inner(&mut br, &k, "windows").await;
    assert_eq!(
        inner,
        WINDOWS_FRAME.replace(
            r#""name":"win-b","status":"busy","cwd":"/tmp/win-b","startedAt":100,"statusUpdatedAt":1000,"watched":false"#,
            r#""name":"win-b","status":"busy","cwd":"/tmp/win-b","startedAt":100,"statusUpdatedAt":1000,"watched":true"#
        ),
        "主动推送（轮询发现变化）同样 enc 包裹、同一帧形态、watched 已收敛"
    );

    // ---- 取消关注 → false ----
    let inner = send_enc_until_inner(
        &mut br,
        &k,
        r#"{"v":1,"type":"window_watch","name":"win-b","watch":false}"#,
        "windows",
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&inner).unwrap();
    assert!(!state.is_watched("win-b"));
    assert_eq!(v["items"][1]["watched"], false, "{v}");
    assert_eq!(
        *persisted.lock().unwrap(),
        vec![vec!["win-b".to_string()], vec![]],
        "取消关注同样落盘（空名单也是一次变化）"
    );

    // ---- 畸形 window_watch（缺 watch）：Warn 丢弃——不改状态、不回帧 ----
    br.send(Message::text(
        serde_json::json!({
            "v": 1, "type": "enc", "machine": MACHINE,
            "box": e2e_seal(&k, r#"{"v":1,"type":"window_watch","name":"win-a"}"#),
        })
        .to_string(),
    ))
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), next_raw_of_type(&mut br, "enc"))
            .await
            .is_err(),
        "畸形 window_watch 不回帧"
    );
    assert!(!state.is_watched("win-a"), "畸形 window_watch 不改关注名单");
    assert_eq!(
        persisted.lock().unwrap().len(),
        2,
        "畸形 window_watch 不触发持久化"
    );

    // windows_query / window_watch 不进 recv（不驱动 agent）。
    assert!(
        tokio::time::timeout(Duration::from_millis(300), p.recv())
            .await
            .is_err(),
        "窗口帧不应产生入站消息"
    );

    let _ = std::fs::remove_file(&key_path);
    handle.shutdown();
    handle.wait().await;
    drop(p);
}

/// 读下一条能解出内层类型 `want` 的 enc 帧，返回内层 JSON 原文（不发送任何
/// 帧——观测网关主动推送用）。
async fn next_enc_inner(br: &mut Ws, k: &[u8; 32], want: &str) -> String {
    loop {
        let envelope = next_raw_of_type(br, "enc").await;
        let boxed = serde_json::from_str::<serde_json::Value>(&envelope).unwrap()["box"]
            .as_str()
            .unwrap()
            .to_string();
        let inner = e2e_open(k, &boxed);
        if serde_json::from_str::<serde_json::Value>(&inner).unwrap()["type"] == want {
            return inner;
        }
    }
}

/// v1.43 窗口事件推送全链路：登录 → HTTP 订阅 → 断开浏览器（离线）→ 平台
/// `on_window_event`（dispatch 在窗口跑完一轮 / 进入 waiting 且过推送闸门时
/// 调用）→ `push_notify{kind=window_turn_end | window_waiting, convId=
/// peer:<窗口名>}` 明文管理帧 → **真实 relay 接受两个新 kind** 并投递（mock
/// push 服务收到 vapid + aes128gcm POST）。钉的是跨 crate 契约：网关侧
/// `PushNotifyKind::as_str` 与中继侧白名单逐字一致——任一侧漂移（或白名单漏了
/// 一个 kind），对应的投递就不会发生（下方两次 `recv_delivery` 各自 10s 超时
/// 即失败）。载荷里 kind 字面量的原样透传由 relay push.rs 的单测钉住
/// （`payload_json_window_kinds_passthrough`）——密文经 aes128gcm 记录填充，
/// 这里不凭长度反推。
#[tokio::test]
async fn loopback_window_events_push_notify_full_chain() {
    use imagent_core::peer::{WindowEvent, WindowEventKind};
    std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
    std::env::set_var("no_proxy", "127.0.0.1,localhost");

    let (handle, sk) = spawn_test_relay().await;
    let addr = handle.addr;
    let platform = WebPlatform::new_with_e2e(
        format!("ws://{addr}/gateway"),
        GatewayCredential::Key(sk),
        "default".to_string(),
        MACHINE.to_string(),
        &e2e_test_key_path("window-events-push"),
        loopback_store("window-events-push").await,
        valid_ws_dir(),
        nop_models(),
        Vec::new(), // v1.36 backends 名单：回环测试不驱动引擎选择，空名单整段缺席
        imagent_web::AsrContext::default(), // v1.40 未配置 → 快照 asr 段缺席
        300,        // v1.37 审批超时预算（approval_request 帧 timeoutSecs）
    );
    eventually(Duration::from_secs(10), || platform.is_online()).await;

    // 浏览器登录拿 sessionToken + 订阅（端点 = loopback mock push 服务）。
    let mut ws = connect_ws(&addr, "/ws", None).await;
    ws.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{USER_TOKEN}"}}"#
    )))
    .await
    .unwrap();
    let ack = next_text(&mut ws).await;
    let v: serde_json::Value = serde_json::from_str(&ack).unwrap();
    let token = v["session"]["token"].as_str().unwrap().to_string();
    let (push_addr, mut push_rx) = spawn_mock_push_service(200).await;
    let ep = format!("http://{push_addr}/fcm/send/window-events");
    let (status, resp) = http_json(
        &addr,
        "POST",
        "/push/subscribe",
        Some(&token),
        Some(
            &serde_json::json!({
                "endpoint": ep,
                "keys": {"p256dh": PUSH_P256DH, "auth": PUSH_AUTH},
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(status, "200", "{resp}");

    // 断开浏览器 WS → 订阅会话离线（SPA 在线免打扰解除）。
    drop(ws);
    tokio::time::sleep(Duration::from_millis(100)).await;

    /// 收一次投递（10s 内），返回 (authorization, encoding, body_len)。
    async fn recv_delivery(
        rx: &mut mpsc::UnboundedReceiver<(String, String, usize)>,
    ) -> (String, String, usize) {
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("离线订阅应收到窗口事件推送（relay 须接受该 kind）")
            .expect("channel 开着")
    }
    let event = |kind: WindowEventKind| WindowEvent {
        name: "win-a".into(),
        kind,
        waiting_for: None,
        push: true,
    };

    // 跑完一轮：kind=window_turn_end。
    platform
        .on_window_event(
            &ConvId("peer:win-a".into()),
            &event(WindowEventKind::TurnEnd),
        )
        .await;
    let (authorization, encoding, body_len) = recv_delivery(&mut push_rx).await;
    assert_eq!(encoding, "aes128gcm", "RFC8188 aes128gcm 内容编码");
    assert!(
        authorization.starts_with("vapid t="),
        "VAPID Authorization 头：{authorization}"
    );
    assert!(body_len > 0, "加密载荷非空");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), push_rx.recv())
            .await
            .is_err(),
        "on_window_event 恰投递一次"
    );

    // 进入 waiting：kind=window_waiting（独立投递——白名单漏掉它则收不到）。
    platform
        .on_window_event(
            &ConvId("peer:win-a".into()),
            &event(WindowEventKind::Waiting),
        )
        .await;
    let (_, encoding, body_len) = recv_delivery(&mut push_rx).await;
    assert_eq!(encoding, "aes128gcm");
    assert!(body_len > 0, "加密载荷非空");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), push_rx.recv())
            .await
            .is_err(),
        "on_window_event 恰投递一次"
    );

    handle.shutdown();
    handle.wait().await;
    drop(platform);
}

// ---------------------------------------------------------------------------
// v1.44 同浏览器多标签页（每个登录会话多把密钥）+ 送达回执 / 网关信息
// ---------------------------------------------------------------------------

/// 浏览器侧「尝试开启」：解不开返回 None。多密钥下同一条连接会收到同会话
/// 其它标签页那份、自己解不开的 enc 帧——[`e2e_open`] 解不开会 panic。
fn e2e_try_open(key: &[u8; 32], boxed: &str) -> Option<String> {
    use base64::Engine as _;
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};

    let data = base64::engine::general_purpose::STANDARD
        .decode(boxed)
        .ok()?;
    let (nonce, ct) = data.split_at(24);
    let cipher = XChaCha20Poly1305::new_from_slice(key).ok()?;
    let pt = cipher.decrypt(XNonce::from_slice(nonce), ct).ok()?;
    String::from_utf8(pt).ok()
}

/// 从 enc 信封原文取 box 字段。
fn enc_box_of(envelope: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(envelope).unwrap();
    v["box"].as_str().unwrap().to_string()
}

/// 内层业务帧 JSON 原文的 type。
fn inner_type(inner: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(inner).unwrap();
    v["type"].as_str().unwrap_or("").to_string()
}

/// 浏览器侧发一个 enc 包裹的业务帧（浏览器→网关方向无 sid）。
async fn send_enc(ws: &mut Ws, key: &[u8; 32], inner: &str) {
    let envelope = serde_json::json!({
        "v": 1, "type": "enc", "machine": MACHINE,
        "box": e2e_seal(key, inner),
    })
    .to_string();
    ws.send(Message::text(envelope)).await.unwrap();
}

/// 哨兵发送：发一个 enc 包裹的应用层 ping。网关对表里**每把密钥**各回一份
/// enc 包裹的 pong——出站份数即该时刻表里的密钥数，是「某把密钥还在不在
/// 表里」的可观察信号。
async fn send_enc_ping(ws: &mut Ws, key: &[u8; 32]) {
    send_enc(ws, key, r#"{"v":1,"type":"ping"}"#).await;
}

/// 哨兵接收（只收 enc 帧）：读 enc 帧直到用 `key` 解出 pong 的那一帧（含），
/// 返回期间收到的全部 enc 信封原文，明文帧跳过。用于出站份数 / 回执内容的
/// 断言——连接上有握手阶段广播来的明文 e2e_ack 残留，不能算进出站份数。
/// 出站按「sid 字典序、同会话内 created 升序」，用该会话里 created 最晚的
/// 密钥做 `key` 即可收齐全部份数。
async fn collect_enc_until_pong(ws: &mut Ws, key: &[u8; 32]) -> Vec<String> {
    let mut got = Vec::new();
    loop {
        let envelope = next_raw_of_type(ws, "enc").await;
        let is_pong = e2e_try_open(key, &enc_box_of(&envelope))
            .is_some_and(|inner| inner_type(&inner) == "pong");
        got.push(envelope);
        if is_pong {
            return got;
        }
    }
}

/// 哨兵接收（收**全部**应用帧）：读应用帧直到用 `key` 解出 pong 的那一帧
/// enc（含），返回期间收到的全部帧原文，明文帧也收——「某操作不回任何帧」
/// 的断言必须把明文回应算进去（只收 enc 帧会让一个错误的明文回应被静默
/// 跳过：首版「e2e_close 不回帧」用 ping 做屏障且只看第一帧，「e2e_close 后
/// 回一帧 pong」的变异因此存活）。
async fn collect_all_until_pong(ws: &mut Ws, key: &[u8; 32]) -> Vec<String> {
    let mut got = Vec::new();
    loop {
        let raw = next_text(ws).await;
        let is_pong = type_of(&raw) == "enc"
            && e2e_try_open(key, &enc_box_of(&raw))
                .is_some_and(|inner| inner_type(&inner) == "pong");
        got.push(raw);
        if is_pong {
            return got;
        }
    }
}

/// 应用帧（明文或 enc 信封）的 type。
fn type_of(raw: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(raw).unwrap();
    v["type"].as_str().unwrap_or("").to_string()
}

/// 读应用帧直到遇到明文 `type == want` 的那一帧（含），返回期间收到的全部
/// 应用帧原文。enc 帧的外层 type 是 `enc`，不会被当成屏障。
async fn read_app_frames_until(ws: &mut Ws, want: &str) -> Vec<String> {
    let mut got = Vec::new();
    loop {
        let raw = next_text(ws).await;
        got.push(raw);
        if type_of(got.last().unwrap()) == want {
            return got;
        }
    }
}

/// 读下一条用 `key` 解开后内层类型为 `want` 的 enc 帧，返回 (信封原文,
/// 内层 JSON 原文)；不发送任何帧（观测网关主动出站用）。中间解不开的
/// enc 帧（同会话其它标签页的回执副本）跳过。
async fn next_enc_raw_inner(ws: &mut Ws, key: &[u8; 32], want: &str) -> (String, String) {
    loop {
        let envelope = next_raw_of_type(ws, "enc").await;
        if let Some(inner) = e2e_try_open(key, &enc_box_of(&envelope)) {
            if inner_type(&inner) == want {
                return (envelope, inner);
            }
        }
    }
}

/// 多标签页专用握手：发 `e2e_hello` 后，读到**对自己这次握手签名**的
/// `e2e_ack` 为止。同会话另一标签页握手的 ack 是明文广播帧，也会到达本
/// 连接——靠验签区分，不靠排空缓冲（那要猜「缓冲里还剩几帧」）。返回会话
/// 密钥。`sk` = 网关认证私钥（e2e_ack 的签名密钥，测试内生成）。
async fn handshake_verified(ws: &mut Ws, sid: &str, sk: &SigningKey) -> [u8; 32] {
    use ed25519_dalek::Verifier;
    use x25519_dalek::{PublicKey as XPublicKey, StaticSecret as XStaticSecret};

    let browser_secret = XStaticSecret::from(rand::random::<[u8; 32]>());
    let browser_public = XPublicKey::from(&browser_secret);
    let n1 = rand::random::<[u8; 32]>();
    ws.send(Message::text(
        serde_json::json!({
            "v": 1, "type": "e2e_hello", "machine": MACHINE, "sid": sid,
            "ek": hex::encode(browser_public.as_bytes()),
            "n1": hex::encode(n1),
        })
        .to_string(),
    ))
    .await
    .unwrap();
    loop {
        let ack = next_of_type(ws, "e2e_ack").await;
        let take = |field: &str| -> [u8; 32] {
            hex::decode(ack[field].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap()
        };
        let (gk, n2) = (take("gk"), take("n2"));
        let mut signed = Vec::with_capacity(128);
        signed.extend_from_slice(browser_public.as_bytes());
        signed.extend_from_slice(&gk);
        signed.extend_from_slice(&n1);
        signed.extend_from_slice(&n2);
        let sig = <[u8; 64]>::try_from(hex::decode(ack["sig"].as_str().unwrap()).unwrap()).unwrap();
        let verified = sk
            .verifying_key()
            .verify(&signed, &ed25519_dalek::Signature::from_bytes(&sig))
            .is_ok();
        if verified {
            return imagent_web::e2e::derive_session_key(
                browser_secret
                    .diffie_hellman(&XPublicKey::from(gk))
                    .as_bytes(),
                &n1,
                &n2,
            );
        }
    }
}

/// v1.44 同浏览器多标签页全链路（真实 relay）。同一个登录会话的两条浏览器
/// 连接（两个标签页共用 session）各自 E2E 握手，网关保留两把密钥：
/// - 出站：两条连接各收到两份 enc（每把密钥一份），各自只解得开自己那份；
/// - 入站：两个标签页都能发，`Platform::recv` 都收到（`client_id` 带上）；
/// - `received` 回执经真实中继回到浏览器：每个 ack 对每把密钥各一份，标签页
///   能解出自己发的与对方发的那条（字节级）；
/// - `e2e_close`：标签页 A 注销自己的密钥后，出站只剩一份、A 的旧密文入站被
///   丢弃、B 照常收发；B 再注销最后一把后连接上不回任何字节。
/// 时序全部用可观察状态，不用 sleep 表达：「某把密钥是否已被摘除」= 哨兵 pong
/// 的份数（2 → 1），轮询期间逐帧判定「除哨兵 pong 外没有任何帧」；最后一条
/// `e2e_close`「不回帧」= 表空后明文 `gateway_info_query` 回**明文**
/// `gateway_info` 这一正向屏障，读到它为止收到的应用帧必须恰好这一帧（任何
/// 类型的回应帧都会排在它前面；屏障不选 pong——它是错误回应最可能撞上的类型）。
#[tokio::test]
async fn loopback_multi_tab_same_session_two_keys_and_e2e_close() {
    let (handle, sk) = spawn_test_relay().await;
    let addr = handle.addr;
    let key_path = e2e_test_key_path("multi-tab");
    let p = WebPlatform::new_with_e2e(
        format!("ws://{addr}/gateway"),
        GatewayCredential::Key(sk.clone()),
        "default".to_string(),
        MACHINE.to_string(),
        &key_path,
        loopback_store("multi-tab").await,
        valid_ws_dir(),
        nop_models(),
        Vec::new(), // v1.36 backends 名单：回环测试不驱动引擎选择，空名单整段缺席
        imagent_web::AsrContext::default(), // v1.40 未配置 → 快照 asr 段缺席
        300,        // v1.37 审批超时预算（approval_request 帧 timeoutSecs）
    );
    eventually(Duration::from_secs(10), || p.is_online()).await;

    // 标签页 A：登录（hello_ack 携带会话凭证载荷）。
    let mut tab_a = connect_ws(&addr, "/ws", None).await;
    tab_a
        .send(Message::text(format!(
            r#"{{"v":1,"type":"hello","token":"{USER_TOKEN}"}}"#
        )))
        .await
        .unwrap();
    let login: serde_json::Value = serde_json::from_str(&next_text(&mut tab_a).await).unwrap();
    assert_eq!(login["type"], "hello_ack", "{login}");
    let sid = login["session"]["id"].as_str().unwrap().to_string();
    let session_token = login["session"]["token"].as_str().unwrap().to_string();
    let _ = next_of_type(&mut tab_a, "machines").await;

    // 标签页 B：复用同一登录会话（session token）→ 同一个 sid。
    let mut tab_b = connect_ws(&addr, "/ws", None).await;
    tab_b
        .send(Message::text(format!(
            r#"{{"v":1,"type":"hello","session":"{session_token}"}}"#
        )))
        .await
        .unwrap();
    let relogin: serde_json::Value = serde_json::from_str(&next_text(&mut tab_b).await).unwrap();
    assert_eq!(
        relogin["type"], "hello_ack",
        "同一会话重连应通过：{relogin}"
    );
    assert_eq!(
        relogin["session"]["id"].as_str(),
        Some(sid.as_str()),
        "同一登录会话 → 同一 sid"
    );
    let _ = next_of_type(&mut tab_b, "machines").await;

    // 各自握手（同一 sid、各自临时密钥）。先 A 后 B：B 的连接上会先到达 A
    // 握手的 ack（明文广播），handshake_verified 靠验签跳过它。
    let ka = handshake_verified(&mut tab_a, &sid, &sk).await;
    let kb = handshake_verified(&mut tab_b, &sid, &sk).await;
    assert_ne!(ka, kb, "两个标签页各自一把不同的会话密钥");

    // ---- 出站：每把密钥一份，各标签页只解得开自己那份 ----
    p.send_text(&conv(), "两个标签页都能看到", &ReplyHint::None)
        .await
        .unwrap();
    send_enc_ping(&mut tab_b, &kb).await;
    // B（kb 最晚 → 它的 pong 排在最后）：text×2 + pong×2 共 4 帧，kb 解得开
    // 第 2、4 帧。
    let b_frames = collect_enc_until_pong(&mut tab_b, &kb).await;
    assert_eq!(b_frames.len(), 4, "{b_frames:?}");
    assert!(
        e2e_try_open(&kb, &enc_box_of(&b_frames[0])).is_none(),
        "ka 那份 B 解不开"
    );
    assert!(
        e2e_open(&kb, &enc_box_of(&b_frames[1])).contains("两个标签页都能看到"),
        "B 解得出自己那份"
    );
    assert!(e2e_try_open(&kb, &enc_box_of(&b_frames[2])).is_none());
    assert_eq!(
        inner_type(&e2e_open(&kb, &enc_box_of(&b_frames[3]))),
        "pong"
    );
    // A（ka 较早 → 读到自己的 pong 为止）：text×2 + pong(ka) 共 3 帧，ka 解
    // 得开第 1、3 帧。
    let a_frames = collect_enc_until_pong(&mut tab_a, &ka).await;
    assert_eq!(a_frames.len(), 3, "{a_frames:?}");
    assert!(
        e2e_open(&ka, &enc_box_of(&a_frames[0])).contains("两个标签页都能看到"),
        "A 解得出自己那份"
    );
    assert!(
        e2e_try_open(&ka, &enc_box_of(&a_frames[1])).is_none(),
        "kb 那份 A 解不开"
    );
    assert_eq!(
        inner_type(&e2e_open(&ka, &enc_box_of(&a_frames[2]))),
        "pong"
    );
    // A 的队列里还剩 kb 那份 pong：读掉（A 解不开），队列归零。
    let leftover = next_raw_of_type(&mut tab_a, "enc").await;
    assert!(e2e_try_open(&ka, &enc_box_of(&leftover)).is_none());

    // ---- 入站：两个标签页都能发；received 回执经真实中继回到浏览器 ----
    let message = |text: &str, client_id: &str| {
        format!(
            r#"{{"v":1,"type":"message","convId":"{CONV}","text":"{text}","machine":"{MACHINE}","clientId":"{client_id}"}}"#
        )
    };
    let ack = |client_id: &str| {
        format!(
            r#"{{"v":1,"type":"ack","machine":"{MACHINE}","clientId":"{client_id}","convId":"{CONV}","status":"received"}}"#
        )
    };
    // 每条消息发出后先等 recv：确认网关已处理它（回执已入队）再发下一条，
    // 两条 ack 的先后由此确定，不依赖两条连接的到达顺序。
    send_enc(&mut tab_a, &ka, &message("来自标签页 A", "m_tabA001")).await;
    let m = tokio::time::timeout(Duration::from_secs(5), p.recv())
        .await
        .expect("A 的消息到达 recv")
        .expect("recv 正常");
    assert_eq!(
        (m.conv_id.clone(), m.text.clone(), m.client_id.clone()),
        (
            conv(),
            Some("来自标签页 A".to_string()),
            Some("m_tabA001".to_string()),
        ),
        "标签页 A 的入站被业务分发，client_id 带上"
    );
    send_enc(&mut tab_b, &kb, &message("来自标签页 B", "m_tabB001")).await;
    let m = tokio::time::timeout(Duration::from_secs(5), p.recv())
        .await
        .expect("B 的消息到达 recv")
        .expect("recv 正常");
    assert_eq!(
        (m.text.clone(), m.client_id.clone()),
        (
            Some("来自标签页 B".to_string()),
            Some("m_tabB001".to_string())
        ),
        "标签页 B 的入站被业务分发"
    );

    // 回执：每个 ack 两份（每把密钥一份）。A 读到自己的 pong 为止共 5 帧，
    // ka 解得开其中两条 ack + pong；B 共 6 帧，kb 解得开两条 ack + 末帧 pong。
    send_enc_ping(&mut tab_a, &ka).await;
    let a_frames = collect_enc_until_pong(&mut tab_a, &ka).await;
    assert_eq!(a_frames.len(), 5, "{a_frames:?}");
    let a_decoded: Vec<String> = a_frames[..4]
        .iter()
        .filter_map(|f| e2e_try_open(&ka, &enc_box_of(f)))
        .collect();
    assert_eq!(
        a_decoded,
        vec![ack("m_tabA001"), ack("m_tabB001")],
        "A 解得出两条回执（自己发的 + 标签页 B 发的），字节级"
    );
    assert_eq!(
        inner_type(&e2e_open(&ka, &enc_box_of(&a_frames[4]))),
        "pong"
    );
    let leftover = next_raw_of_type(&mut tab_a, "enc").await;
    assert!(e2e_try_open(&ka, &enc_box_of(&leftover)).is_none());
    let b_frames = collect_enc_until_pong(&mut tab_b, &kb).await;
    assert_eq!(b_frames.len(), 6, "{b_frames:?}");
    let b_decoded: Vec<String> = b_frames[..5]
        .iter()
        .filter_map(|f| e2e_try_open(&kb, &enc_box_of(f)))
        .collect();
    assert_eq!(b_decoded, vec![ack("m_tabA001"), ack("m_tabB001")]);
    assert_eq!(
        inner_type(&e2e_open(&kb, &enc_box_of(&b_frames[5]))),
        "pong"
    );

    // ---- e2e_close：标签页 A 注销自己的密钥 ----
    send_enc(&mut tab_a, &ka, r#"{"v":1,"type":"e2e_close"}"#).await;
    // 等「ka 已被摘除」：哨兵 pong 的份数从 2 变 1。每轮一个哨兵，读到份数
    // 变化即停——用可观察状态等，不猜时序。同时钉住「不回任何帧」：每轮
    // 收到的必须只有哨兵 pong（逐帧用 ka / kb 解密判定；ka 摘除前 2 份、之后
    // 1 份）。e2e_close 的任何回应——明文或密文——都排在「ka 被摘除」之前
    // 入队，必然落在某一轮的收集范围里；只看帧数不够（1 份密文回应与
    // 「pong×2」同为长度 2）。
    let decrypt = |f: &str| {
        [ka, kb]
            .iter()
            .find_map(|k| e2e_try_open(k, &enc_box_of(f)))
    };
    let mut rounds = 0usize;
    loop {
        rounds += 1;
        assert!(rounds <= 25, "e2e_close 后 ka 应在若干哨兵内被摘除");
        send_enc_ping(&mut tab_b, &kb).await;
        let frames = collect_all_until_pong(&mut tab_b, &kb).await;
        assert!(
            frames.len() <= 2
                && frames.iter().all(|f| {
                    type_of(f) == "enc"
                        && decrypt(f).is_some_and(|inner| inner_type(&inner) == "pong")
                }),
            "e2e_close 不得回任何帧（明文或密文）：每轮只应有哨兵 pong：{frames:?}"
        );
        if frames.len() == 1 {
            assert_eq!(inner_type(&e2e_open(&kb, &enc_box_of(&frames[0]))), "pong");
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // A 的旧密文入站被丢弃：A、B 各发一条，recv 拿到的必须是 B 的，且之后
    // 也没有 A 的（ka 不在表里，A 的帧永远到不了 recv——与到达顺序无关）。
    send_enc(&mut tab_a, &ka, &message("A-密钥已注销", "m_tabA002")).await;
    send_enc(&mut tab_b, &kb, &message("B-仍然可用", "m_tabB002")).await;
    let m = tokio::time::timeout(Duration::from_secs(5), p.recv())
        .await
        .expect("B 的消息到达 recv")
        .expect("recv 正常");
    assert_eq!(
        (m.text.clone(), m.client_id.clone()),
        (
            Some("B-仍然可用".to_string()),
            Some("m_tabB002".to_string())
        ),
        "A 的旧密文被丢弃，B 照常可用"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), p.recv())
            .await
            .is_err(),
        "A 的消息没有到达"
    );
    // B 注销前仍收得到自己那条的回执；读掉它让 B 的队列归零。
    let (_, ack_inner) = next_enc_raw_inner(&mut tab_b, &kb, "ack").await;
    assert_eq!(
        ack_inner,
        ack("m_tabB002"),
        "B 仍能解出回执（ka 已不在表里，只有 kb 一份）"
    );

    // ---- B 注销最后一把密钥：e2e_close 不回任何帧 ----
    send_enc(&mut tab_b, &kb, r#"{"v":1,"type":"e2e_close"}"#).await;
    // 正向屏障：表空后**明文** gateway_info_query 回**明文** gateway_info。读到
    // 这帧应答为止，B 收到的应用帧必须**恰好这一帧**（字节级）——e2e_close 的
    // 任何回应帧，无论什么类型，都会排在它前面。屏障刻意选 gateway_info 而
    // 不是 pong：pong 是「错误地回了一帧」时最可能撞上的类型，首版用 ping 做
    // 屏障且只看第一帧，「e2e_close 后回一帧 pong」的变异因此存活。kb 若没被
    // 摘除，应答仍是 enc 包裹的（外层 type 是 enc），读不到明文 gateway_info。
    tab_b
        .send(Message::text(format!(
            r#"{{"v":1,"type":"gateway_info_query","machine":"{MACHINE}"}}"#
        )))
        .await
        .unwrap();
    let gateway_info = format!(
        r#"{{"v":1,"type":"gateway_info","machine":"{MACHINE}","version":"{}","caps":["acks"]}}"#,
        env!("CARGO_PKG_VERSION")
    );
    let frames = read_app_frames_until(&mut tab_b, "gateway_info").await;
    assert_eq!(
        frames,
        vec![gateway_info.clone()],
        "e2e_close 不回帧：B 在屏障应答之前不得收到任何帧；表空后应答为明文"
    );
    // A 同样收到这条明文 gateway_info（轮询期间积压的 enc 帧被跳过）。
    let a_info = next_raw_of_type(&mut tab_a, "gateway_info").await;
    assert_eq!(a_info, gateway_info, "{a_info}");

    let _ = std::fs::remove_file(&key_path);
    handle.shutdown();
    handle.wait().await;
    drop(p);
}

/// v1.44 送达回执与网关信息全链路（真实 relay）。
/// 明文期（无 E2E 会话——出站明文直发）：
/// - `gateway_info_query` → `gateway_info` 帧**字节级**（version = 本 crate 版本，
///   caps = ["acks"]）；
/// - `message{clientId}` → `ack{received}` 帧字节级，消息进 `recv` 且带上
///   `client_id`；
/// - 不带 / 不合规 clientId 的消息照旧到达，但**不回 ack**——以紧随其后的
///   `gateway_info` 应答为界（同一连接按序到达网关），之前不得出现 ack 帧；
/// - 违规帧（缺 text）带合规 clientId → `ack{rejected, malformed}` 字节级。
/// E2E 会话活跃后（生产形态）：`gateway_info_query` / `message{clientId}` /
/// 平台钩子 `on_message_rejected` 的应答全部 enc 包裹，解密后与明文期逐字节
/// 相同；中继可见的帧体不泄露内帧类型与 clientId。
#[tokio::test]
async fn loopback_ack_and_gateway_info_frames() {
    let (handle, sk) = spawn_test_relay().await;
    let addr = handle.addr;
    let key_path = e2e_test_key_path("ack-info");
    let p = WebPlatform::new_with_e2e(
        format!("ws://{addr}/gateway"),
        GatewayCredential::Key(sk),
        "default".to_string(),
        MACHINE.to_string(),
        &key_path,
        loopback_store("ack-info").await,
        valid_ws_dir(),
        nop_models(),
        Vec::new(), // v1.36 backends 名单：回环测试不驱动引擎选择，空名单整段缺席
        imagent_web::AsrContext::default(), // v1.40 未配置 → 快照 asr 段缺席
        300,        // v1.37 审批超时预算（approval_request 帧 timeoutSecs）
    );
    eventually(Duration::from_secs(10), || p.is_online()).await;
    let mut br = connect_ws(&addr, "/ws", None).await;
    br.send(Message::text(format!(
        r#"{{"v":1,"type":"hello","token":"{USER_TOKEN}"}}"#
    )))
    .await
    .unwrap();
    let login: serde_json::Value = serde_json::from_str(&next_text(&mut br).await).unwrap();
    let sid = login["session"]["id"].as_str().unwrap().to_string();
    let _ = next_of_type(&mut br, "machines").await;

    let gateway_info = format!(
        r#"{{"v":1,"type":"gateway_info","machine":"{MACHINE}","version":"{}","caps":["acks"]}}"#,
        env!("CARGO_PKG_VERSION")
    );
    let message = |client_id_part: &str| {
        format!(
            r#"{{"v":1,"type":"message","convId":"{CONV}","text":"带回执","machine":"{MACHINE}"{client_id_part}}}"#
        )
    };

    // ---- 明文期 ----
    br.send(Message::text(format!(
        r#"{{"v":1,"type":"gateway_info_query","machine":"{MACHINE}"}}"#
    )))
    .await
    .unwrap();
    assert_eq!(
        next_raw_of_type(&mut br, "gateway_info").await,
        gateway_info,
        "gateway_info 帧须逐字节一致"
    );

    br.send(Message::text(message(r#","clientId":"m_1a2b3c4d""#)))
        .await
        .unwrap();
    assert_eq!(
        next_raw_of_type(&mut br, "ack").await,
        r#"{"v":1,"type":"ack","machine":"test-mac","clientId":"m_1a2b3c4d","convId":"web:work","status":"received"}"#,
        "received 回执须逐字节一致"
    );
    let m = tokio::time::timeout(Duration::from_secs(5), p.recv())
        .await
        .expect("消息到达 recv")
        .expect("recv 正常");
    assert_eq!(
        (m.conv_id.clone(), m.client_id.clone()),
        (conv(), Some("m_1a2b3c4d".to_string())),
        "消息进入入站 channel，client_id 带上"
    );

    // 不带 / 不合规 clientId：消息照旧到达，但不回 ack。
    let over = "m".repeat(65);
    for (raw, why) in [
        (message(""), "无 clientId"),
        (
            message(&format!(r#","clientId":"{over}""#)),
            "超长 clientId",
        ),
        (message(r#","clientId":"a.b""#), "非法字符 clientId"),
        (message(r#","clientId":42"#), "非字符串 clientId"),
    ] {
        br.send(Message::text(raw)).await.unwrap();
        br.send(Message::text(format!(
            r#"{{"v":1,"type":"gateway_info_query","machine":"{MACHINE}"}}"#
        )))
        .await
        .unwrap();
        let m = tokio::time::timeout(Duration::from_secs(5), p.recv())
            .await
            .expect("消息到达 recv")
            .expect("recv 正常");
        assert_eq!(m.client_id, None, "{why}：clientId 当缺席");
        // 读到 gateway_info 应答为止：其间出现的任何 ack 帧都是违约。
        loop {
            let text = next_text(&mut br).await;
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_ne!(v["type"], "ack", "{why}：不得回 ack：{text}");
            if v["type"] == "gateway_info" {
                assert_eq!(text, gateway_info);
                break;
            }
        }
    }

    // 违规帧（缺 text）带合规 clientId → malformed 回执（字节级）。
    br.send(Message::text(format!(
        r#"{{"v":1,"type":"message","convId":"{CONV}","machine":"{MACHINE}","clientId":"m_bad0001"}}"#
    )))
    .await
    .unwrap();
    assert_eq!(
        next_raw_of_type(&mut br, "ack").await,
        r#"{"v":1,"type":"ack","machine":"test-mac","clientId":"m_bad0001","convId":"web:work","status":"rejected","reason":"malformed","message":"消息格式无效"}"#,
        "malformed 回执须逐字节一致"
    );

    // ---- E2E 会话活跃后：全部 enc 包裹，解密后与明文期逐字节相同 ----
    let k = e2e_handshake(&mut br, &sid).await;
    let assert_blind = |envelope: &str, needle: &str| {
        assert!(
            !envelope.contains(needle),
            "中继可见的 enc 帧体不得泄露 {needle:?}：{envelope}"
        );
    };

    send_enc(
        &mut br,
        &k,
        r#"{"v":1,"type":"gateway_info_query","machine":"test-mac"}"#,
    )
    .await;
    let (envelope, inner) = next_enc_raw_inner(&mut br, &k, "gateway_info").await;
    assert_eq!(inner, gateway_info, "enc 内层与明文期逐字节一致");
    assert_blind(&envelope, "gateway_info");
    assert_blind(&envelope, "acks");

    send_enc(
        &mut br,
        &k,
        r#"{"v":1,"type":"message","convId":"web:work","text":"密文消息","clientId":"m_enc00001"}"#,
    )
    .await;
    let (envelope, inner) = next_enc_raw_inner(&mut br, &k, "ack").await;
    assert_eq!(
        inner,
        r#"{"v":1,"type":"ack","machine":"test-mac","clientId":"m_enc00001","convId":"web:work","status":"received"}"#
    );
    assert_blind(&envelope, r#""type":"ack""#);
    assert_blind(&envelope, "m_enc00001");
    assert_blind(&envelope, "密文消息");
    let m = tokio::time::timeout(Duration::from_secs(5), p.recv())
        .await
        .expect("消息到达 recv")
        .expect("recv 正常");
    assert_eq!(
        (m.text.clone(), m.client_id.clone()),
        (Some("密文消息".to_string()), Some("m_enc00001".to_string()))
    );

    // Dispatcher 的明确丢弃点 → 平台钩子 → enc 包裹的 rejected 回执。三个
    // 丢弃点调用钩子由 core 单测钉住，这里直接调平台钩子验证出站链路与字节。
    p.on_message_rejected(
        &conv(),
        "m_enc00002",
        imagent_core::MessageRejectReason::NotAllowed,
    );
    let (_, inner) = next_enc_raw_inner(&mut br, &k, "ack").await;
    assert_eq!(
        inner,
        r#"{"v":1,"type":"ack","machine":"test-mac","clientId":"m_enc00002","convId":"web:work","status":"rejected","reason":"not_allowed","message":"发送者未获授权，消息未被处理"}"#
    );

    let _ = std::fs::remove_file(&key_path);
    handle.shutdown();
    handle.wait().await;
    drop(p);
}
