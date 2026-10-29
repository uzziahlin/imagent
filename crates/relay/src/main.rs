//! `imagent-relay` bin：env 读取（fail-closed 校验）、tracing 初始化、
//! bind、serve 组装。核心逻辑全部在 lib（任务说明 §4.7 分层），这里只有
//! 「装配 + 生命周期」。
//!
//! env 一览（§4.6 + v1.2 + v1.3）：
//! - `IMAGENT_RELAY_GATEWAY_KEYS`（v1.3）：网关 machine-公钥绑定表
//!   （`home-mac=<64 hex ed25519 公钥>,…`，与 GATEWAY_TOKENS 同格式）——
//!   网关侧 `~/.imagent/web-gateway-key` 首次启动自动生成密钥对并打印
//!   公钥，把公钥配到对应机器名下即完成接入。与 TOKENS 至少配置其一
//!   （fail-closed）；双表并存任一命中（迁移期双保险）。
//! - `IMAGENT_RELAY_GATEWAY_TOKENS`：网关 token-machine 绑定表
//!   （`home-mac=9a08…,work-mac=7f2b…`，每台 Mac 一个独立 token；旧单
//!   token env `IMAGENT_RELAY_GATEWAY_TOKEN` 已删除——检测到旧值给迁移
//!   指引）。
//! - `IMAGENT_RELAY_USER_TOKEN`：浏览器登录 token（仅登录换发会话用）。
//! - `IMAGENT_RELAY_STATE_DIR`：会话状态目录（缺省
//!   `/var/lib/imagent-relay`，会话表落盘 `<dir>/sessions.json`）；systemd
//!   部署可配 `StateDirectory=imagent-relay`（落同一路径）。
//! - `IMAGENT_RELAY_RP_ID`（可选）：passkey（WebAuthn）RP ID 覆盖——缺省
//!   按请求 Host 去端口推导（公网 `relay.example.com:8443` →
//!   `relay.example.com`）；固定后换域名/端口不动已注册凭证。注意
//!   本地 http IP 直连（127.0.0.1）不可用 passkey（rp_id 域名校验 + secure
//!   context），本地调试走 USER_TOKEN 路径或用 `localhost` 主机名。
//! - `IMAGENT_RELAY_BIND`：缺省 127.0.0.1:3010（loopback-only）。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use imagent_relay::{
    spawn_relay, validate_bind, GatewayKeys, GatewayTokens, RelayConfig, DEFAULT_SESSION_TTL_SECS,
};
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};

/// 缺省绑定（§4.6）：127.0.0.1:3010，公网由 Caddy 反代。
const DEFAULT_BIND: &str = "127.0.0.1:3010";
/// 缺省会话状态目录（协议 v1.2）：systemd 部署配 `StateDirectory=imagent-relay`
/// 时 StateDirectory= 落点同为 /var/lib/imagent-relay，两种配法收敛。
const DEFAULT_STATE_DIR: &str = "/var/lib/imagent-relay";

#[tokio::main]
async fn main() -> ExitCode {
    // 1) env 读取 + fail-closed 校验（§4.2/§4.6 + v1.2）：网关 token 表 /
    //    USER_TOKEN 任一缺失、空、职责冲突都拒绝启动——不提供无认证模式。
    //    错误打到 stderr 后退出。
    let config = match relay_config_from_env() {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("imagent-relay: 启动失败：{msg}");
            return ExitCode::from(2);
        }
    };
    // 2) bind 地址：IMAGENT_RELAY_BIND 缺省 127.0.0.1:3010；非 loopback
    //    一律拒绝（§4.5，公网入口只允许 Caddy）。
    let bind: SocketAddr = match std::env::var("IMAGENT_RELAY_BIND") {
        Ok(v) => match v.parse() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("imagent-relay: 启动失败：IMAGENT_RELAY_BIND={v} 无法解析为地址（{e}）");
                return ExitCode::from(2);
            }
        },
        // 缺省值是编译期常量，必然合法。
        Err(_) => DEFAULT_BIND.parse().expect("缺省 bind 常量必然合法"),
    };
    if let Err(msg) = validate_bind(bind) {
        eprintln!("imagent-relay: 启动失败：{msg}");
        return ExitCode::from(2);
    }
    // 3) tracing：RUST_LOG 缺省 info（§4.6）。
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    // 日志纪律（§4.5）：网关 token 绝不输出（只报 machine 列表——machine
    // 是路由身份不是秘密）；公钥是公开材料，只报指纹（前 16 hex）降噪——
    // 排查握手不匹配时用网关侧启动日志的完整公钥对照；USER_TOKEN 只露
    // 前 4 位。
    tracing::info!(target: "imagent::relay",
        addr = %bind,
        gateway_machines = ?config.gateway_tokens.machines(),
        gateway_key_machines = ?config.gateway_keys.machines(),
        gateway_key_fps = ?config.gateway_keys.fingerprints(),
        user_token = %imagent_relay::mask_token(&config.user_token),
        state_dir = %config.state_dir.display(),
        session_ttl_days = config.session_ttl_secs / 86_400,
        "imagent-relay 启动（token 已脱敏）"
    );

    // 4) bind + serve（listener 由本处绑定，fail-closed 校验先行——对齐
    //    src/webhook.rs P2-2 先例）。
    let listener = match TcpListener::bind(bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("imagent-relay: 绑定 {bind} 失败：{e}");
            return ExitCode::FAILURE;
        }
    };
    let handle = match spawn_relay(listener, config) {
        Ok(h) => h,
        Err(msg) => {
            eprintln!("imagent-relay: 启动失败：{msg}");
            return ExitCode::from(2);
        }
    };

    // 5) SIGTERM（systemd stop）/ SIGINT（Ctrl-C）→ graceful shutdown
    //    （§4.5：停 accept、close 两条 WS 1001、退出）。
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("imagent-relay: 注册 SIGTERM 处理失败：{e}");
            return ExitCode::FAILURE;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
    tracing::info!(target: "imagent::relay", "收到停机信号，graceful shutdown");
    handle.shutdown();
    handle.wait().await;
    tracing::info!(target: "imagent::relay", "relay 已退出");
    ExitCode::SUCCESS
}

/// 从 env 构造配置（§4.6 汇总表 + v1.2 会话层 + v1.3 公钥表）。错误信息
/// 可读（含生成/迁移指引）。
fn relay_config_from_env() -> Result<RelayConfig, String> {
    // 网关公钥表（v1.3，key 挑战应答验签用）。未设/空 → 空表（不是错误：
    // 「KEYS/TOKENS 至少其一」由下方显式判定，只配 TOKENS 的既有部署
    // 零改动）。
    let gateway_keys = match std::env::var("IMAGENT_RELAY_GATEWAY_KEYS") {
        Ok(v) if !v.trim().is_empty() => GatewayKeys::parse(&v)?,
        _ => GatewayKeys::default(),
    };
    // 网关 token-machine 绑定表（v1.2）。旧单 token env 已删除：还设着旧
    // 值时给显式迁移指引，而不是一句「未设置」让人摸不着头脑。
    let gateway_tokens = match std::env::var("IMAGENT_RELAY_GATEWAY_TOKENS") {
        Ok(v) if !v.trim().is_empty() => GatewayTokens::parse(&v)?,
        _ => GatewayTokens::default(),
    };
    if gateway_tokens.is_empty() && gateway_keys.is_empty() {
        let legacy_set = std::env::var("IMAGENT_RELAY_GATEWAY_TOKEN")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        return Err(if legacy_set {
            "IMAGENT_RELAY_GATEWAY_TOKEN 已废弃：请改用 IMAGENT_RELAY_GATEWAY_TOKENS\
             （machine=token 逗号分隔，每台 Mac 一个独立 token；本机 ~/.imagent/web-relay.env \
             的 IMAGENT_WEB_GATEWAY_TOKEN 同步改为本机 machine 对应的 token），\
             或改用 IMAGENT_RELAY_GATEWAY_KEYS（machine=<64 hex ed25519 公钥>，v1.3 挑战应答）"
                .into()
        } else {
            "环境变量 IMAGENT_RELAY_GATEWAY_KEYS（machine=<64 hex ed25519 公钥> 逗号分隔，\
             v1.3 挑战应答）与 IMAGENT_RELAY_GATEWAY_TOKENS（machine=token 逗号分隔）\
             均未设置或为空——至少配置其一；relay 不提供无认证模式"
                .into()
        });
    }
    let user_token = non_empty_env("IMAGENT_RELAY_USER_TOKEN")?;
    // 会话状态目录（v1.2）：main 负责 mkdir（spawn_relay 侧只兜底），启动
    // 期就发现权限问题，而不是首次换发会话时才报。
    let state_dir = match std::env::var("IMAGENT_RELAY_STATE_DIR") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => PathBuf::from(DEFAULT_STATE_DIR),
    };
    if let Err(e) = std::fs::create_dir_all(&state_dir) {
        return Err(format!(
            "创建会话状态目录 {} 失败：{e}（设 IMAGENT_RELAY_STATE_DIR，或 systemd 配 \
             StateDirectory=imagent-relay）",
            state_dir.display()
        ));
    }
    // passkey RP ID 覆盖（可选；空值按未设处理——缺省推导）。合法性（须为
    // 请求 Host 的有效域名）在 webauthn begin 端点按 builder 校验兜底，不在
    // 启动期拒（推导出的 origin 请求期才知道）。
    let webauthn_rp_id = std::env::var("IMAGENT_RELAY_RP_ID")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let cfg = RelayConfig {
        gateway_tokens,
        gateway_keys,
        user_token,
        state_dir,
        session_ttl_secs: DEFAULT_SESSION_TTL_SECS,
        webauthn_rp_id,
    };
    cfg.validate()?;
    Ok(cfg)
}

fn non_empty_env(key: &str) -> Result<String, String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ => Err(format!(
            "环境变量 {key} 未设置或为空（必填；用 openssl rand -hex 32 生成；\
             relay 不提供无认证模式）"
        )),
    }
}
