//! `imagent-relay` bin：env 读取（fail-closed 校验）、tracing 初始化、
//! bind、serve 组装。核心逻辑全部在 lib（任务说明 §4.7 分层），这里只有
//! 「装配 + 生命周期」。

use std::net::SocketAddr;
use std::process::ExitCode;

use imagent_relay::{mask_token, spawn_relay, validate_bind, RelayConfig};
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};

/// 缺省绑定（§4.6）：127.0.0.1:3010，公网由 Caddy 反代。
const DEFAULT_BIND: &str = "127.0.0.1:3010";

#[tokio::main]
async fn main() -> ExitCode {
    // 1) env 读取 + fail-closed 校验（§4.2/§4.6）：token 任一缺失/空/相同
    //    都拒绝启动——不提供无认证模式。错误打到 stderr 后退出。
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
    // token 打印脱敏（§4.5 日志纪律：只露前 4 位）。
    tracing::info!(target: "imagent::relay",
        addr = %bind,
        gateway_token = %mask_token(&config.gateway_token),
        user_token = %mask_token(&config.user_token),
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

/// 从 env 构造配置（§4.6 汇总表）。错误信息可读（含生成指引）。
fn relay_config_from_env() -> Result<RelayConfig, String> {
    let gateway_token = non_empty_env("IMAGENT_RELAY_GATEWAY_TOKEN")?;
    let user_token = non_empty_env("IMAGENT_RELAY_USER_TOKEN")?;
    let cfg = RelayConfig {
        gateway_token,
        user_token,
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
