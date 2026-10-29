//! `imagent-relay` bin：env 读取（fail-closed 校验）、tracing 初始化、
//! bind、serve 组装。核心逻辑全部在 lib（任务说明 §4.7 分层），这里只有
//! 「装配 + 生命周期」。
//!
//! env 一览（§4.6 + v1.2 + v1.3 + 管理批）：
//! - `IMAGENT_RELAY_GATEWAY_KEYS`（v1.3，管理批起**仅首次种子**）：网关
//!   machine-公钥绑定表（`home-mac=<64 hex ed25519 公钥>,…`）——首次启动
//!   种子导入 `<state_dir>/machines.json` 并落盘，此后**文件权威**（本 env
//!   被忽略；增删机器走网页管理面 `machine_add` / `machine_remove`）。
//! - `IMAGENT_RELAY_GATEWAY_TOKENS`：网关 token-machine 绑定表（**env 静态
//!   兼容路径**，管理批不做管理面化；机器切到 key 握手后可移除）。与
//!   KEYS 种子/机器表至少配置其一（fail-closed——已落盘 machines.json 的
//!   部署重启不受此限）。
//! - `IMAGENT_RELAY_USER_TOKEN`（管理批起**仅首次种子**）：浏览器登录
//!   token。首次启动写 `<state_dir>/user_token_hash`（sha256），此后文件
//!   权威（本 env 被忽略；轮换走网页管理面 `user_token_rotate`）。env 与
//!   哈希文件均无 → 拒启。
//! - `IMAGENT_RELAY_STATE_DIR`：状态目录（缺省 `/var/lib/imagent-relay`）：
//!   `sessions.json` / `machines.json` / `user_token_hash` /
//!   `credentials.json` 落盘处；systemd 部署可配
//!   `StateDirectory=imagent-relay`（落同一路径）。
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
    machines_table_exists, spawn_relay, validate_bind, GatewayKeys, GatewayTokens, RelayConfig,
    DEFAULT_PAIRING_TTL_SECS, DEFAULT_SESSION_TTL_SECS,
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
    // 日志纪律（§4.5）：网关 token / USER_TOKEN 绝不输出；公钥只报指纹
    // （前 16 hex）降噪。管理批后 KEYS/USER_TOKEN 是种子语义——运行权威
    // 在 state_dir 落盘文件（spawn_relay 侧另有加载日志）。
    tracing::info!(target: "imagent::relay",
        addr = %bind,
        gateway_machines = ?config.gateway_tokens.machines(),
        gateway_key_seed_machines = ?config.gateway_keys.machines(),
        gateway_key_seed_fps = ?config.gateway_keys.fingerprints(),
        user_token_seed = %if config.user_token.is_empty() {
            "(env 未设；user_token_hash 文件权威)".to_string()
        } else {
            format!("{}…（仅首次种子，此后文件权威）", &config.user_token[..4])
        },
        state_dir = %config.state_dir.display(),
        session_ttl_days = config.session_ttl_secs / 86_400,
        pairing_ttl_secs = config.pairing_ttl_secs,
        "imagent-relay 启动（token 已脱敏；机器表/USER_TOKEN 运行权威在 state_dir）"
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

/// 从 env 构造配置（§4.6 汇总表 + v1.2 会话层 + v1.3 公钥表 + 管理批种子
/// 语义）。错误信息可读（含生成/迁移指引）。
fn relay_config_from_env() -> Result<RelayConfig, String> {
    // 网关公钥表（v1.3 → 管理批起仅首次种子）：未设/空 → 空种子（合法：
    // machines.json 已存在的部署是文件权威，或只配 TOKENS 的既有部署）。
    let gateway_keys = match std::env::var("IMAGENT_RELAY_GATEWAY_KEYS") {
        Ok(v) if !v.trim().is_empty() => GatewayKeys::parse(&v)?,
        _ => GatewayKeys::default(),
    };
    // 网关 token-machine 绑定表（v1.2，env 静态兼容路径）。旧单 token env
    // 已删除：还设着旧值时给显式迁移指引。
    let gateway_tokens = match std::env::var("IMAGENT_RELAY_GATEWAY_TOKENS") {
        Ok(v) if !v.trim().is_empty() => GatewayTokens::parse(&v)?,
        _ => GatewayTokens::default(),
    };
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
    // fail-closed：网关认证形态至少其一（env TOKENS 静态表 / KEYS 种子 /
    // 已落盘的 machines.json）。**例外**：machines.json 存在 = 管理面已
    // 接管机器表（哪怕是清空后的空表）——「UI 删光机器后重启 brick」不是
    // fail-closed 想防的（空表 = 无网关可认证，是锁死不是裸奔）。
    if gateway_tokens.is_empty() && gateway_keys.is_empty() && !machines_table_exists(&state_dir) {
        let legacy_set = std::env::var("IMAGENT_RELAY_GATEWAY_TOKEN")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        return Err(if legacy_set {
            "IMAGENT_RELAY_GATEWAY_TOKEN 已废弃：请改用 IMAGENT_RELAY_GATEWAY_TOKENS\
             （machine=token 逗号分隔，每台 Mac 一个独立 token；本机 ~/.imagent/web-relay.env \
             的 IMAGENT_WEB_GATEWAY_TOKEN 同步改为本机 machine 对应的 token），\
             或改用 IMAGENT_RELAY_GATEWAY_KEYS（machine=<64 hex ed25519 公钥>，v1.3 挑战应答、\
             管理批起首次种子进 machines.json）"
                .into()
        } else {
            "环境变量 IMAGENT_RELAY_GATEWAY_KEYS（首次种子，machine=<64 hex ed25519 公钥> \
             逗号分隔，v1.3 挑战应答）与 IMAGENT_RELAY_GATEWAY_TOKENS（machine=token 逗号分隔）\
             均未设置或为空，且 state_dir 下无 machines.json——至少配置其一；\
             relay 不提供无认证模式"
                .into()
        });
    }
    // USER_TOKEN（管理批起仅首次种子）：文件权威迁移后 env 可移除——种子
    // 与 user_token_hash 文件**均无**才拒启（判定在 spawn_relay 的
    // UserTokenStore::load，这里不再硬性要求 env 存在）。
    let user_token = std::env::var("IMAGENT_RELAY_USER_TOKEN")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_default();
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
        pairing_ttl_secs: DEFAULT_PAIRING_TTL_SECS,
        webauthn_rp_id,
    };
    cfg.validate()?;
    Ok(cfg)
}
