//! `imagent-relay` bin：env 读取（fail-closed 校验）、tracing 初始化、
//! bind、serve 组装。核心逻辑全部在 lib（任务说明 §4.7 分层），这里只有
//! 「装配 + 生命周期」。
//!
//! env 一览（§4.6 + v1.2）：
//! - `IMAGENT_RELAY_GATEWAY_TOKENS`：网关 token-machine 绑定表
//!   （`home-mac=9a08…,work-mac=7f2b…`，每台 Mac 一个独立 token；旧单
//!   token env `IMAGENT_RELAY_GATEWAY_TOKEN` 已删除——检测到旧值给迁移
//!   指引）；与 USER_TOKEN 均未设 → 拒启（fail-closed）。
//! - `IMAGENT_RELAY_USER_TOKEN`：浏览器登录 token（仅登录换发会话用）。
//! - `IMAGENT_RELAY_STATE_DIR`：会话状态目录（缺省
//!   `/var/lib/imagent-relay`，会话表落盘 `<dir>/sessions.json`）；systemd
//!   部署可配 `StateDirectory=imagent-relay`（落同一路径）。
//! - `IMAGENT_RELAY_BIND`：缺省 127.0.0.1:3010（loopback-only）。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use imagent_relay::{
    spawn_relay, validate_bind, GatewayTokens, RelayConfig, DEFAULT_SESSION_TTL_SECS,
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
    // 是路由身份不是秘密）；USER_TOKEN 只露前 4 位。
    tracing::info!(target: "imagent::relay",
        addr = %bind,
        gateway_machines = ?config.gateway_tokens.machines(),
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

/// 从 env 构造配置（§4.6 汇总表 + v1.2 会话层）。错误信息可读（含生成/
/// 迁移指引）。
fn relay_config_from_env() -> Result<RelayConfig, String> {
    // 网关 token-machine 绑定表（v1.2）。旧单 token env 已删除：还设着旧
    // 值时给显式迁移指引，而不是一句「未设置」让人摸不着头脑。
    let gateway_tokens = match std::env::var("IMAGENT_RELAY_GATEWAY_TOKENS") {
        Ok(v) if !v.trim().is_empty() => GatewayTokens::parse(&v)?,
        _ => {
            let legacy_set = std::env::var("IMAGENT_RELAY_GATEWAY_TOKEN")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false);
            return Err(if legacy_set {
                "IMAGENT_RELAY_GATEWAY_TOKEN 已废弃：请改用 IMAGENT_RELAY_GATEWAY_TOKENS\
                 （machine=token 逗号分隔，每台 Mac 一个独立 token；本机 ~/.imagent/web-relay.env \
                 的 IMAGENT_WEB_GATEWAY_TOKEN 同步改为本机 machine 对应的 token）"
                    .into()
            } else {
                "环境变量 IMAGENT_RELAY_GATEWAY_TOKENS 未设置或为空（machine=token 逗号分隔，\
                 每台 Mac 一个独立 token；openssl rand -hex 32 生成；relay 不提供无认证模式）"
                    .into()
            });
        }
    };
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
    let cfg = RelayConfig {
        gateway_tokens,
        user_token,
        state_dir,
        session_ttl_secs: DEFAULT_SESSION_TTL_SECS,
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
