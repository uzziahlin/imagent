//! v1.20+ webhook 入站子系统：HTTP server（`POST /hook/<token>`）、路由/验签
//! （HMAC-SHA256 + opt-in 时间戳协议）、两层防重放（ReplayGuard 内存 LRU +
//! SQLite webhook_seen 持久层）、每 token 令牌桶限速、GitHub 原生 payload
//! 解析器。入站驱动面：bind/暴露面校验 fail-closed，由 main 的 Start 装配
//! 调用（validate_webhook_bind / spawn_webhook_server），server 不随 SIGHUP
//! 重启。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;

/// 起 HTTP server（/metrics + /health），独立 tokio task。失败仅 warn。
/// v1.20 webhook 入站：`POST /hook/<token>` → 事件文本注入对应会话。
/// 鉴权 = 路径 token 与 config [[webhook]] 精确匹配；body ≤64KB；
/// JSON `{"text": "..."}` 取 text，否则整包作为纯文本。与 /cron 同走
/// handle() 完整管线（会话白名单门内才有 agent，无旁路）。
/// v1.21 防护套件：可选 HMAC-SHA256 验签（GitHub webhook secret 协议）+
/// 每 token 令牌桶限速 + GitHub 原生 payload 结构化摘要。
/// v1.24 防重放两层：① 验签通过后按 (token, 签名) 进程内去重（同字节重放
/// 409）；② opt-in 时间戳协议 `X-Imagent-Timestamp`（验签串 `{ts}.{body}`，
/// 过窗 401）——见 [`webhook_gate`] / [`ReplayGuard`]。
#[derive(Clone)]
struct WebhookState {
    /// token → 路由（含验签密钥/限速桶）
    routes: std::collections::HashMap<String, std::sync::Arc<WebhookRoute>>,
    dispatcher: Arc<imagent_core::Dispatcher>,
    /// v1.24 第 1 层防重放：验签通过签名的进程级去重表（所有路由共享，
    /// 键含 token）。Clone 语义 = 共享同一张表（axum 每 connection 克隆
    /// state，去重必须进程级生效）。
    seen: ReplayGuard,
}

/// 单条 webhook 路由：投递目标 + 防护配置 + 限速桶（std Mutex——临界区纯内存
/// 无 await）。
struct WebhookRoute {
    conv: String,
    name: String,
    /// HMAC-SHA256 验签密钥（GitHub webhook secret 协议）。None = 不验签。
    secret: Option<String>,
    /// v1.24 时间戳协议窗口（秒）；0 = 不启用（验签串 = body 原文）。
    replay_window_secs: u64,
    /// 令牌桶速率（请求/秒）；0 = 不限速。
    rps: f64,
    bucket: std::sync::Mutex<TokenBucket>,
}

/// v1.24 第 1 层防重放：验签通过签名的去重表（进程内 LRU——容量
/// [`REPLAY_SEEN_CAPACITY`] / 条目 TTL [`REPLAY_SEEN_TTL`]）。手法与 feishu
/// 评论锚点表同款：HashMap + 插入时间戳，超容先清过期、再淘汰最旧；不引
/// lru 依赖。std Mutex——临界区纯内存无 await（与限速桶同款理由）。
///
/// 原理：攻击者无法伪造新签名（HMAC），因此所有「能通过验签的重放」与
/// 首发字节完全相同 → 签名相同 → 去重即可拦截。代价：**合法的完全相同
/// 事件在 TTL 内重发会被误拦**（如手动重跑同一 GitHub event——罕见，
/// README 已说明；等 TTL 过后可重发，或发送端启用时间戳协议让每次签名
/// 天然不同）。
///
/// P3-b（code-review v14）：`store` 挂 SQLite 持久层（schema v16 `webhook_seen`）
/// ——内存表随进程重启清零，重启窗口内的同签名重放会再次通过；落库后跨
/// 进程生效（`replay_window_secs > 0` 的路由启用，窗口即保留期）。
#[derive(Clone)]
struct ReplayGuard {
    inner: Arc<std::sync::Mutex<std::collections::HashMap<String, Instant>>>,
    /// None = 未接 store（持久层关闭，仅 [`ReplayGuard::default`] 的测试形态）。
    store: Option<imagent_store::Store>,
}

impl Default for ReplayGuard {
    fn default() -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            store: None,
        }
    }
}

/// 签名去重表容量：默认 rps=10 下 10 分钟 ≈ 6000 个合法签名，1024 是
/// 「正常流量全覆盖 + 洪泛下内存有界」的折中——被容量挤出的条目失去
/// 去重记忆（极端洪泛下少量重放漏网），换来表恒定 ≤1024 条。
const REPLAY_SEEN_CAPACITY: usize = 1024;
/// 签名去重条目 TTL：≥ GitHub 对重复 delivery 的自查窗口（5 分钟），
/// 覆盖绝大多数重试/重放场景。
const REPLAY_SEEN_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// webhook 入站路由模式（axum 0.8 / matchit 0.8 语法 `{token}`）。**版本错位
/// 警示**：axum 0.7/matchit 0.7 的参数语法是 `:token`，`{token}` 在其下会被当
/// **字面量段**——编译照过、运行时 webhook 全量 404（依赖升级批曾实际踩到：
/// axum pin 漏改时路由字符串已先行切换）。路由匹配回归测试钉在
/// `hook_route_matches_under_current_axum`。
const HOOK_ROUTE: &str = "/hook/{token}";

impl ReplayGuard {
    /// 首见（或条目已过 TTL）→ 记录并放行（true）；TTL 内重复 → 判定
    /// 重放（false）。`now` 由调用方传入便于单测拨钟。
    fn admit(&self, key: &str, now: Instant) -> bool {
        let fresh = |t: &Instant| -> bool {
            now.checked_duration_since(*t)
                .unwrap_or(std::time::Duration::ZERO)
                < REPLAY_SEEN_TTL
        };
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // 命中未过期条目 = 确定重放——判定优先于容量淘汰（被查键不应在
        // 自己的查询里被挤出而放行）。
        if let Some(t) = m.get(key) {
            if fresh(t) {
                return false;
            }
        }
        // 容量护栏：先清过期；仍满则淘汰最旧（重复命中不刷新时间戳，
        // 插入序即新旧序）。
        if m.len() >= REPLAY_SEEN_CAPACITY {
            m.retain(|_, t| fresh(t));
            while m.len() >= REPLAY_SEEN_CAPACITY {
                let Some(oldest) = m.iter().min_by_key(|(_, t)| *t).map(|(k, _)| k.clone()) else {
                    break;
                };
                m.remove(&oldest);
            }
        }
        m.insert(key.to_string(), now);
        true
    }
}

/// v1.24 fail-closed 绑定校验（纯函数，便于单测；口径对齐
/// [`crate::ops::validate_metrics_bind`]）：webhook 绑定非 loopback 且任一条目未配
/// `secret` → 拒绝启动。webhook 能直接驱动一整轮 agent，防护面不得低于
/// metrics 端点——「公网裸 token 即可伪造事件」不允许带病运行。
pub(crate) fn validate_webhook_bind(
    socket: SocketAddr,
    entries: &[imagent_core::WebhookEntry],
) -> Result<(), String> {
    if socket.ip().is_loopback() {
        return Ok(());
    }
    match entries.iter().find(|e| e.secret.is_none()) {
        None => Ok(()),
        Some(e) => Err(format!(
            "绑定非 loopback 地址 {socket} 且 [[webhook]] name={} 未配置 secret，\
             webhook 可直接驱动 agent，公网裸 token 即可伪造事件；\
             请为每条 webhook 配置 secret 或绑定 127.0.0.1",
            e.name
        )),
    }
}

/// v1.24 webhook 入站防护门（纯函数，便于单测）：验签（含 opt-in 时间戳协议）。
/// 通过返回 `Ok(Some(去重键))`（配置了 secret 的路由；`Ok(None)` = 未配
/// secret、无可验/可去重）；拒绝返回 (状态码, 文案)。
///
/// P3-b（code-review v14）：**本函数只做验证、不再记录签名**——去重键交还
/// 调用方，在「验签 + 时间窗通过 + 未被限流拒绝」之后才进内存/持久去重表。
/// 旧序「gate 内先 admit 再限流」会让被 429 的请求也占住签名：GitHub 对同一
/// delivery 的合规重试（同字节同签名）会吃 409「重放」，违反重试协议。
///
/// 两层防重放：
/// 1. 签名去重（默认启用）——见 [`ReplayGuard`]（进程内）+ webhook_seen 表（持久）；
/// 2. 时间戳协议（`replay_window_secs > 0` 时）——请求须带
///    `X-Imagent-Timestamp: <unix 秒>`，验签串从 `body` 改为 `{ts}.{body}`
///    （ts 参与签名、不可伪造），`|now - ts| > 窗口` 即 401。两层叠加：
///    去重拦「同字节重放」，时间戳拦「窗口外的任何重放（含首发迟到）」。
fn webhook_gate(
    route: &WebhookRoute,
    token: &str,
    headers: &axum::http::HeaderMap,
    body: &[u8],
    now_unix: u64,
) -> Result<Option<String>, (StatusCode, &'static str)> {
    let Some(secret) = route.secret.as_deref() else {
        // 未配 secret：无签名可验/可去重（非 loopback 绑定由启动期
        // validate_webhook_bind fail-closed 把关）。
        return Ok(None);
    };
    // 验签串：默认 body 原文；启用时间戳协议时 "{ts}.{body}"（u64 无负值，
    // 解析失败 = 头缺失/非数字，一律 401）。
    let (signing_input, ts) = if route.replay_window_secs > 0 {
        let ts = headers
            .get("x-imagent-timestamp")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .ok_or((
                StatusCode::UNAUTHORIZED,
                "missing or invalid X-Imagent-Timestamp\n",
            ))?;
        let mut input = format!("{ts}.").into_bytes();
        input.extend_from_slice(body);
        (input, Some(ts))
    } else {
        (body.to_vec(), None)
    };
    let sig = webhook_signature_header(headers);
    if !sig.is_some_and(|s| verify_webhook_signature(secret, &signing_input, s)) {
        return Err((StatusCode::UNAUTHORIZED, "invalid signature\n"));
    }
    // 时间戳新鲜度：ts 已参与验签，此处拒绝的必然是「曾经合法」的旧签名
    // 请求 = 重放（绝对差判定，容忍双向时钟偏移）。ts 仅在协议启用时为 Some。
    if let Some(ts) = ts {
        if !timestamp_within_window(now_unix, ts, route.replay_window_secs) {
            return Err((
                StatusCode::UNAUTHORIZED,
                "timestamp outside replay window\n",
            ));
        }
    }
    // 去重键 = (token, 规范化签名 hex)，交调用方在限流通过后 admit。
    let key = format!(
        "{token}\u{0}{}",
        webhook_signature_key_hex(sig.unwrap_or_default())
    );
    Ok(Some(key))
}

/// 验签头原文（GitHub `X-Hub-Signature-256: sha256=<hex>` 或裸 hex 的
/// `X-Signature`）。两层去重键从同一处取值。
fn webhook_signature_header(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get("x-hub-signature-256")
        .or_else(|| headers.get("x-signature"))
        .and_then(|h| h.to_str().ok())
}

/// 验签头 → 去重键的规范化 hex：去 `sha256=` 前缀 + trim + 小写。大小写
/// hex 解码等值（验签都会过），不规范化则攻击者换大小写即可绕过去重。
fn webhook_signature_key_hex(sig: &str) -> String {
    sig.strip_prefix("sha256=")
        .unwrap_or(sig)
        .trim()
        .to_ascii_lowercase()
}

/// v1.24 时间戳新鲜度：|now - ts| ≤ window 即新鲜（绝对差容忍双向时钟
/// 偏移；相等视为新鲜——窗口边界含端点）。
fn timestamp_within_window(now_unix: u64, ts: u64, window: u64) -> bool {
    now_unix.abs_diff(ts) <= window
}

/// 简单令牌桶（纯内存，webhook 单进程内生效）。
struct TokenBucket {
    tokens: f64,
    last: std::time::Instant,
}

impl TokenBucket {
    fn new(full: f64) -> Self {
        Self {
            tokens: full,
            last: std::time::Instant::now(),
        }
    }
    /// 尝试取 1 个令牌：先按 elapsed×rps 回填（封顶容量 = max(1, rps)，约 1s
    /// 突发余量），余量 ≥1 才放行。
    fn try_take(&mut self, rps: f64) -> bool {
        if rps <= 0.0 {
            return true; // 不限速
        }
        let cap = rps.max(1.0);
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * rps).min(cap);
            self.last = now;
        }
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// v1.21 HMAC-SHA256 验签（GitHub webhook secret 同款协议）：
/// `X-Hub-Signature-256: sha256=<hex>`（也兼容裸 hex 的 `X-Signature` 形态）。
/// 常数时间比较（防时序侧信道）；hex 解码失败/长度不符直接 false。
fn verify_webhook_signature(secret: &str, body: &[u8], header_value: &str) -> bool {
    use hmac::{KeyInit as _, Mac as _};
    let hex_sig = header_value
        .strip_prefix("sha256=")
        .unwrap_or(header_value)
        .trim();
    let Ok(expect) = hex::decode(hex_sig) else {
        return false;
    };
    let Ok(mut mac) = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    let computed = mac.finalize().into_bytes();
    // 常数时间比较（与 bearer_authorized 同款 XOR 累计——不引入 subtle 依赖）。
    let n = computed.len().max(expect.len());
    let mut diff: u8 = (computed.len() != expect.len()) as u8;
    for i in 0..n {
        let x = computed.get(i).copied().unwrap_or(0);
        let y = expect.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

pub(crate) fn spawn_webhook_server(
    listener: tokio::net::TcpListener,
    entries: Vec<imagent_core::WebhookEntry>,
    dispatcher: Arc<imagent_core::Dispatcher>,
    store: imagent_store::Store,
) {
    const DEFAULT_WEBHOOK_RPS: f64 = 10.0;
    let routes = entries
        .into_iter()
        .map(|e| {
            let rps = e.rps.unwrap_or(DEFAULT_WEBHOOK_RPS);
            (
                e.token,
                std::sync::Arc::new(WebhookRoute {
                    conv: e.conv,
                    name: e.name,
                    secret: e.secret,
                    replay_window_secs: e.replay_window_secs,
                    rps,
                    bucket: std::sync::Mutex::new(TokenBucket::new(rps.max(1.0))),
                }),
            )
        })
        .collect();
    // P3-b（code-review v14）：去重表挂 store 持久层（webhook_seen，v16）——
    // replay_window_secs > 0 的路由重启后窗口内重放仍被拦。
    let state = WebhookState {
        routes,
        dispatcher,
        seen: ReplayGuard {
            inner: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            store: Some(store),
        },
    };
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    // v1.21 review：停机时停止 accept——drain 期间注入只会挂死/被丢弃，
    // 优雅关停让客户端拿到连接关闭而非假 202。
    let shutdown = state.dispatcher.shutdown_token();
    let app = Router::new()
        .route(HOOK_ROUTE, post(webhook_handler))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(state);
    tokio::spawn(async move {
        // P2-2（code-review v14）：bind 已由调用方完成（fail-closed 拒启）；
        // 此处仅 accept/serve。
        tracing::info!(target: "imagent::ops", addr = %addr, "webhook 入站 listening（POST /hook/<token>）");
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
        {
            tracing::warn!(target: "imagent::ops", addr = %addr, error = %e, "webhook HTTP server 退出");
        }
        tracing::info!(target: "imagent::ops", "webhook server 已随停机关闭");
    });
}

/// body → 注入文本：JSON 带 text 字段取之；否则整包 UTF-8 文本；空/非 UTF-8 拒。
fn webhook_body_text(body: &[u8]) -> Option<String> {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
            let t = t.trim();
            if t.is_empty() {
                return None;
            }
            return Some(t.to_string());
        }
    }
    let s = String::from_utf8_lossy(body);
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// v1.21 GitHub 原生事件解析结果：`Text` 注入会话；`Skip` 确认收到但不注入
///（进行中/未订阅的中间事件——注入只会产生噪音与无效 agent 轮次）。
enum GithubEventText {
    Text(String),
    Skip,
}

/// GitHub webhook payload（`X-GitHub-Event` 头存在时）→ 可读事件摘要。
/// 覆盖 CI/协作主链路事件；未识别的事件类型一律 Skip（raw JSON 注入只会让
/// agent 解析噪音——自定义注入请走无该头的 JSON `{"text": ...}` 形态）。
fn github_event_text(event: &str, body: &[u8]) -> GithubEventText {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return GithubEventText::Skip;
    };
    let s = |path: &[&str]| -> String {
        let mut cur = &v;
        for k in path {
            cur = &cur[*k];
        }
        cur.as_str().unwrap_or_default().to_string()
    };
    match event {
        "ping" => GithubEventText::Text(format!(
            "🏓 GitHub webhook 连通性测试成功：仓库 {} 的事件订阅已生效。",
            s(&["repository", "full_name"])
        )),
        "workflow_run" => {
            // 只注入终态（completed）——requested/in_progress 每次跑三连发，
            // 中间态对「驱动 agent」无信息量。
            if s(&["action"]) != "completed" {
                return GithubEventText::Skip;
            }
            // v1.23 review：必要字段缺失（name/html_url）拼出的空壳文本会
            // 驱动一整轮 agent 却无信息量——与「未识别事件 Skip」同哲学拦下。
            let wf_name = s(&["workflow_run", "name"]);
            let wf_url = s(&["workflow_run", "html_url"]);
            if wf_name.is_empty() || wf_url.is_empty() {
                return GithubEventText::Skip;
            }
            let conclusion = s(&["workflow_run", "conclusion"]);
            let mark = match conclusion.as_str() {
                "success" => "✅",
                "failure" => "❌",
                "cancelled" => "⚠️",
                _ => "⏹️",
            };
            GithubEventText::Text(format!(
                "{mark} GitHub Actions：「{}」（{} 分支）{}\n仓库 {} · 触发 {} · 第 {} 次运行\n{}",
                wf_name,
                s(&["workflow_run", "head_branch"]),
                conclusion,
                s(&["repository", "full_name"]),
                s(&["workflow_run", "actor", "login"]),
                v["workflow_run"]["run_number"].as_i64().unwrap_or(0),
                wf_url,
            ))
        }
        "push" => {
            let pusher = s(&["pusher", "name"]);
            let repo = s(&["repository", "full_name"]);
            if pusher.is_empty() || repo.is_empty() {
                return GithubEventText::Skip;
            }
            let commits = v["commits"].as_array().cloned().unwrap_or_default();
            let msgs: Vec<String> = commits
                .iter()
                .take(3)
                .filter_map(|c| {
                    c["message"]
                        .as_str()
                        .and_then(|m| m.lines().next())
                        .map(|m| truncate_chars(m, 200))
                })
                .collect();
            let list = if msgs.is_empty() {
                "（无提交——可能是分支删除或 tag 操作）".to_string()
            } else {
                let mut l = msgs
                    .iter()
                    .map(|m| format!("- {m}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                if commits.len() > msgs.len() {
                    l.push_str(&format!("\n- ……等共 {} 个提交", commits.len()));
                }
                l
            };
            let git_ref = s(&["ref"]).trim_start_matches("refs/heads/").to_string();
            GithubEventText::Text(format!(
                "📦 GitHub push：{} → {}（{}）\n{}\n{}",
                pusher,
                repo,
                git_ref,
                list,
                s(&["compare"])
            ))
        }
        "issues" => {
            let action = s(&["action"]);
            // assigned/labeled/unassigned 等低价值动作跳过。
            if !matches!(action.as_str(), "opened" | "closed" | "reopened") {
                return GithubEventText::Skip;
            }
            let (title, url) = (s(&["issue", "title"]), s(&["issue", "html_url"]));
            if title.is_empty() || url.is_empty() {
                return GithubEventText::Skip;
            }
            let mark = if action == "closed" { "✅" } else { "🎯" };
            GithubEventText::Text(format!(
                "{mark} GitHub issue {}：#{} {}\nby {} · {}\n{}",
                action,
                v["issue"]["number"].as_i64().unwrap_or(0),
                s(&["issue", "title"]),
                s(&["issue", "user", "login"]),
                s(&["repository", "full_name"]),
                s(&["issue", "html_url"]),
            ))
        }
        "issue_comment" => {
            if s(&["action"]) != "created" {
                return GithubEventText::Skip;
            }
            let body_txt = truncate_chars(&s(&["comment", "body"]), 300);
            GithubEventText::Text(format!(
                "💬 GitHub 新评论（{}#{} {}）：\n{}：{}\n{}",
                s(&["repository", "full_name"]),
                v["issue"]["number"].as_i64().unwrap_or(0),
                s(&["issue", "title"]),
                s(&["comment", "user", "login"]),
                body_txt,
                s(&["comment", "html_url"]),
            ))
        }
        "pull_request" => {
            let action = s(&["action"]);
            if !matches!(
                action.as_str(),
                "opened" | "closed" | "reopened" | "review_requested"
            ) {
                return GithubEventText::Skip;
            }
            let merged = v["pull_request"]["merged"].as_bool().unwrap_or(false);
            let action_disp = if merged {
                "merged（已合并）".to_string()
            } else {
                action
            };
            let (title, url) = (
                s(&["pull_request", "title"]),
                s(&["pull_request", "html_url"]),
            );
            if title.is_empty() || url.is_empty() {
                return GithubEventText::Skip;
            }
            let mark = if merged { "🎉" } else { "🌱" };
            GithubEventText::Text(format!(
                "{mark} GitHub PR {}：#{} {}\nby {} · {} ← {}\n{}",
                action_disp,
                v["pull_request"]["number"].as_i64().unwrap_or(0),
                s(&["pull_request", "title"]),
                s(&["pull_request", "user", "login"]),
                s(&["pull_request", "base", "ref"]),
                s(&["pull_request", "head", "ref"]),
                s(&["pull_request", "html_url"]),
            ))
        }
        _ => GithubEventText::Skip,
    }
}

/// 按字符截断（中文安全），尾加省略标记。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let t: String = s.chars().take(max).collect();
    format!("{t}…")
}

async fn webhook_handler(
    State(st): State<WebhookState>,
    axum::extract::Path(token): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> impl IntoResponse {
    let Some(route) = st.routes.get(&token) else {
        return (StatusCode::NOT_FOUND, "unknown token\n");
    };
    let Ok(bytes) = body else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "body too large (64KB)\n");
    };
    // v1.21 防护① + v1.24 两层防重放（配置了 secret 时强制）：HMAC 验签
    //（GitHub 头 `X-Hub-Signature-256: sha256=<hex>`，兼容裸 hex 的
    // `X-Signature`）→ opt-in 时间戳协议 → 限速 → 签名去重（内存 LRU +
    // SQLite 持久层，同签名 409）。
    // P3-b（code-review v14）顺序修正：旧序「签名去重在限速之前」会让被 429
    // 拒绝的请求也占住签名——GitHub 对同一 delivery 的合规重试（同字节同签名）
    // 会吃 409「重放」。签名记录必须发生在验签+时间窗通过**且**未被限流拒绝
    // 之后（429 拒绝不记录签名）。
    // 时钟早于 epoch（系统时钟异常）时 now_unix 取 u64::MAX：时间戳协议
    // 请求全部 401（fail-closed），未启用协议的路径不受影响。
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(u64::MAX);
    let dedup_key = match webhook_gate(route, &token, &headers, &bytes, now_unix) {
        Ok(k) => k,
        Err((code, msg)) => {
            tracing::warn!(target: "imagent::ops", name = %route.name, status = %code, "webhook 验签/防重放拒绝");
            return (code, msg);
        }
    };
    // v1.21 防护②：每 token 令牌桶限速。
    {
        let mut bucket = route.bucket.lock().unwrap_or_else(|e| e.into_inner());
        if !bucket.try_take(route.rps) {
            return (StatusCode::TOO_MANY_REQUESTS, "rate limited\n");
        }
    }
    // 签名去重（限流通过后才记录，见上）。内存层恒启用；持久层仅
    // `replay_window_secs > 0` 的路由（窗口即 SQLite 保留期，重启不失效）。
    if let Some(key) = dedup_key {
        if !st.seen.admit(&key, Instant::now()) {
            tracing::warn!(target: "imagent::ops", name = %route.name, status = %StatusCode::CONFLICT, "webhook 签名去重拦截（内存层命中：重放）");
            return (StatusCode::CONFLICT, "replay detected\n");
        }
        if route.replay_window_secs > 0 {
            if let Some(db) = &st.seen.store {
                // 内存未命中 → 查库（重启后内存表清零，持久层兜底）；库中也
                // 未见过才入库（验签+限流已通过 = 真实首见）。查/写失败仅
                // warn fail-open：持久层是第二道去重，第一道签名验证与内存
                // 去重不受影响，不值得让 webhook 入站因观测性写失败而拒绝。
                let boundary =
                    (now_unix.saturating_sub(route.replay_window_secs)).min(i64::MAX as u64) as i64;
                let ts = now_unix.min(i64::MAX as u64) as i64;
                match db.webhook_seen_contains(&key).await {
                    Ok(true) => {
                        tracing::warn!(target: "imagent::ops", name = %route.name, status = %StatusCode::CONFLICT, "webhook 签名去重拦截（持久层命中：重放）");
                        return (StatusCode::CONFLICT, "replay detected\n");
                    }
                    Ok(false) => {
                        if let Err(e) = db.webhook_seen_insert(&key, ts, boundary).await {
                            tracing::warn!(target: "imagent::ops", error = %e, "webhook 签名入库失败（持久去重 best-effort）");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "imagent::ops", error = %e, "webhook 签名查库失败（持久去重降级内存层）");
                    }
                }
            }
        }
    }
    // v1.21 GitHub 原生事件：X-GitHub-Event 头存在 → 结构化摘要（未知事件
    // Skip 确认不注入）；否则走通用 text 提取。
    let text = match headers.get("x-github-event").and_then(|h| h.to_str().ok()) {
        Some(event) => match github_event_text(event, &bytes) {
            GithubEventText::Text(t) => t,
            GithubEventText::Skip => {
                tracing::debug!(target: "imagent::ops", event, name = %route.name, "GitHub 事件按策略跳过（中间态/未订阅）");
                return (StatusCode::ACCEPTED, "ignored\n");
            }
        },
        None => match webhook_body_text(&bytes) {
            Some(t) => t,
            None => return (StatusCode::BAD_REQUEST, "empty or undecodable body\n"),
        },
    };
    let msg = imagent_core::InboundMessage {
        conv_id: imagent_core::ConvId(route.conv.clone()),
        sender: imagent_core::UserId(format!("webhook:{}", route.name)),
        sender_name: None,
        text: Some(format!("【{}】{}", route.name, text)),
        media: vec![],
        media_errors: Vec::new(),
        mentions: Vec::new(),
        mentioned_bot: false,
        ask_req: None,
        reply_to: None,
        source_msg_id: None,
        control: None,
        // v1.21 review：合成消息不走 steering（独立轮次 + 持久化兜底）。
        no_steer: true,
        reply_hint: imagent_core::ReplyHint::None,
    };
    tracing::info!(target: "imagent::ops", conv = %route.conv, name = %route.name, "webhook 命中，注入 dispatcher");
    match st.dispatcher.inject(msg).await {
        Ok(()) => (StatusCode::ACCEPTED, "queued\n"),
        Err(e) => {
            tracing::warn!(target: "imagent::ops", error = %e, "webhook 注入被拒（停机中）");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "shutting down, retry later\n",
            )
        }
    }
}

/// S7：Bearer 鉴权判定（纯函数，便于单测）。`token` 为 None 表示未启用
/// 鉴权（loopback 部署），一律放行；Some 时要求 Authorization 头精确等于
/// `Bearer <token>`（前缀多余字符不匹配）。恒定时间比较（L14，见函数体）。
/// v1.20 webhook：body → 注入文本（JSON text 字段优先 / 纯文本兜底 / 空拒绝）。
#[test]
fn webhook_body_text_variants() {
    assert_eq!(
        webhook_body_text(br#"{"text":"deploy failed","run":42}"#),
        Some("deploy failed".into())
    );
    assert_eq!(webhook_body_text(b"  raw text  "), Some("raw text".into()));
    assert_eq!(webhook_body_text(b""), None);
    assert_eq!(webhook_body_text(b"   "), None);
    // JSON 无 text 字段 → 整包作为文本（lossy 容忍）。
    assert!(webhook_body_text(br#"{"other":1}"#).is_some());
}

/// v1.21：HMAC 验签——正签通过、错签/错体/畸形 hex 拒绝（GitHub sha256= 前缀
/// 与裸 hex 两形态）。
#[test]
fn webhook_signature_verify() {
    use hmac::{KeyInit as _, Mac as _};
    let secret = "topsecret-0123456789";
    let body = br#"{"text":"hi"}"#;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    let sig = hex::encode(mac.finalize().into_bytes().as_slice());
    assert!(verify_webhook_signature(
        secret,
        body,
        &format!("sha256={sig}")
    ));
    assert!(verify_webhook_signature(secret, body, &sig));
    assert!(!verify_webhook_signature(secret, body, "sha256=deadbeef"));
    assert!(!verify_webhook_signature(
        secret,
        b"tampered",
        &format!("sha256={sig}")
    ));
    assert!(!verify_webhook_signature(
        "other-secret",
        body,
        &format!("sha256={sig}")
    ));
    assert!(!verify_webhook_signature(secret, body, "not-hex!"));
    assert!(!verify_webhook_signature(secret, body, ""));
}

/// v1.21：令牌桶——rps>0 时容量 = max(1, rps)，耗尽 429，回填后恢复；
/// rps=0 不限速。
#[test]
fn webhook_token_bucket() {
    let mut b = TokenBucket::new(2.0);
    assert!(b.try_take(2.0));
    assert!(b.try_take(2.0));
    assert!(!b.try_take(2.0), "容量 2 应耗尽");
    std::thread::sleep(std::time::Duration::from_millis(600));
    assert!(b.try_take(2.0), "回填 ~1.2 个后应恢复");
    let mut unbounded = TokenBucket::new(0.0);
    for _ in 0..100 {
        assert!(unbounded.try_take(0.0), "rps=0 不限速");
    }
}

/// v1.21：GitHub 原生 payload → 摘要文本（workflow_run 终态/中间态、push、
/// 未知事件 Skip）。
#[test]
fn github_event_summary_variants() {
    let wf = br#"{"action":"completed","workflow_run":{"name":"CI","head_branch":"main","conclusion":"failure","run_number":42,"html_url":"https://github.com/u/r/actions/runs/1","actor":{"login":"uzziahlin"}},"repository":{"full_name":"u/r"}}"#;
    match github_event_text("workflow_run", wf) {
        GithubEventText::Text(t) => {
            assert!(t.contains("CI") && t.contains("failure") && t.contains("runs/1"));
        }
        GithubEventText::Skip => panic!("completed 应产出文本"),
    }
    let wf_mid = br#"{"action":"in_progress","workflow_run":{"name":"CI"}}"#;
    assert!(matches!(
        github_event_text("workflow_run", wf_mid),
        GithubEventText::Skip
    ));
    let push = br#"{"pusher":{"name":"alice"},"ref":"refs/heads/main","compare":"https://github.com/u/r/compare/a...b","repository":{"full_name":"u/r"},"commits":[{"message":"fix: one\n\nbody"},{"message":"feat: two"}]}"#;
    match github_event_text("push", push) {
        GithubEventText::Text(t) => {
            assert!(t.contains("alice") && t.contains("fix: one") && t.contains("feat: two"));
        }
        GithubEventText::Skip => panic!("push 应产出文本"),
    }
    assert!(matches!(
        github_event_text("star", br#"{}"#),
        GithubEventText::Skip
    ));
    assert_eq!(truncate_chars("abcdef", 3), "abc…");
    assert_eq!(truncate_chars("ab", 3), "ab");
}

/// v1.24 webhook 防重放 / fail-closed 单测（纯函数层——防护门
/// [`webhook_gate`] 与 [`validate_webhook_bind`] 均为无 IO 纯函数）。
#[cfg(test)]
mod webhook_replay_tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};
    use hmac::{KeyInit as _, Mac as _};

    const SECRET: &str = "topsecret-0123456789";

    fn hmac_hex(secret: &str, input: &[u8]) -> String {
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(input);
        hex::encode(mac.finalize().into_bytes().as_slice())
    }

    /// 时间戳协议验签串：`{ts}.{body}`。
    fn ts_signing_input(ts: u64, body: &[u8]) -> Vec<u8> {
        let mut v = format!("{ts}.").into_bytes();
        v.extend_from_slice(body);
        v
    }

    fn route(secret: Option<&str>, replay_window_secs: u64) -> WebhookRoute {
        WebhookRoute {
            conv: "feishu:oc_t".into(),
            name: "t".into(),
            secret: secret.map(str::to_string),
            replay_window_secs,
            rps: 0.0,
            bucket: std::sync::Mutex::new(TokenBucket::new(0.0)),
        }
    }

    fn headers(pairs: &[(&str, String)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                k.parse::<axum::http::HeaderName>().unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    /// 第 1 层去重表本体：TTL 内同键拦截、TTL 过后放行；容量护栏——
    /// 满容量先清过期、再淘汰最旧，被查键不被自己的查询挤出。
    #[test]
    fn replay_guard_ttl_and_capacity() {
        let g = ReplayGuard::default();
        let t0 = Instant::now();
        assert!(g.admit("a", t0), "首见放行");
        assert!(
            !g.admit(
                "a",
                t0 + REPLAY_SEEN_TTL - std::time::Duration::from_secs(1)
            ),
            "TTL 内重复 → 重放"
        );
        assert!(
            g.admit(
                "a",
                t0 + REPLAY_SEEN_TTL + std::time::Duration::from_secs(1)
            ),
            "TTL 过后重发 → 放行（合法同字节事件的重发出口）"
        );

        // 容量：灌满 1024 个新鲜键（时间戳严格递增，k0 最旧）。
        let g2 = ReplayGuard::default();
        for i in 0..REPLAY_SEEN_CAPACITY {
            assert!(g2.admit(
                &format!("k{i}"),
                t0 + std::time::Duration::from_millis(i as u64)
            ));
        }
        let len = |g: &ReplayGuard| g.inner.lock().unwrap_or_else(|e| e.into_inner()).len();
        assert_eq!(len(&g2), REPLAY_SEEN_CAPACITY);
        assert!(
            g2.admit("overflow", t0 + std::time::Duration::from_secs(5)),
            "满容量时新键仍放行"
        );
        assert_eq!(len(&g2), REPLAY_SEEN_CAPACITY, "容量恒定（最旧 k0 被淘汰）");
        assert!(
            !g2.admit("k1", t0 + std::time::Duration::from_secs(5)),
            "未淘汰的近期键仍拦截"
        );
        assert!(
            g2.admit("k0", t0 + std::time::Duration::from_secs(6)),
            "k0 已被容量淘汰，去重记忆重开——容量护栏优先于 TTL"
        );
    }

    /// P3-b（code-review v14）后的 gate 纯验证语义：验签通过返回**去重键**而非
    /// 自行记录（记录移到限流之后，429 不占签名）；键含 token + 规范化签名
    /// （大小写 hex / 裸 hex 形态同键，防换大小写绕过）；未配 secret → None。
    #[test]
    fn gate_returns_dedup_key_without_side_effect() {
        let body = br#"{"text":"deploy failed"}"#;
        let sig = hmac_hex(SECRET, body);
        let r = route(Some(SECRET), 0);
        let now = 1_700_000_000_u64;

        let h = headers(&[("x-hub-signature-256", format!("sha256={sig}"))]);
        let key = webhook_gate(&r, "token-a", &h, body, now)
            .expect("首发验签通过")
            .expect("配 secret 的路由应有去重键");
        assert_eq!(
            key,
            format!("token-a\u{0}{}", sig.to_ascii_lowercase()),
            "键 = token + 小写规范化签名"
        );

        // 大小写 hex / 裸 hex 头形态：验签等价 → 去重键规范化后相同（防绕过）。
        let h_upper = headers(&[(
            "x-hub-signature-256",
            format!("sha256={}", sig.to_uppercase()),
        )]);
        assert_eq!(
            webhook_gate(&r, "token-a", &h_upper, body, now)
                .unwrap()
                .unwrap(),
            key,
            "签名换大小写不可绕过去重"
        );
        let h_bare = headers(&[("x-signature", sig.clone())]);
        assert_eq!(
            webhook_gate(&r, "token-a", &h_bare, body, now)
                .unwrap()
                .unwrap(),
            key,
            "裸 hex 头形态与 sha256= 前缀形态同键"
        );

        // 不同 token（另一条 webhook 路由收到同样签名）键不同，互不误拦。
        let key_b = webhook_gate(&r, "token-b", &h, body, now).unwrap().unwrap();
        assert_ne!(key_b, key, "键含 token，跨路由不串");

        // gate 不再消费 ReplayGuard：同签名重复过 gate 恒 Ok（去重由调用方在
        // 限流通过后显式 admit——见 handler；这里直接验证 admit 语义）。
        let guard = ReplayGuard::default();
        assert!(guard.admit(&key, Instant::now()), "首见放行");
        assert!(!guard.admit(&key, Instant::now()), "TTL 内重放拦截");

        // 未配 secret 的路由：门直通且无去重键（非 loopback 无 secret 由启动校验拒绝）。
        let r_nosec = route(None, 0);
        assert_eq!(
            webhook_gate(&r_nosec, "token-a", &HeaderMap::new(), body, now),
            Ok(None)
        );
    }

    /// 第 2 层时间戳协议（opt-in）：缺失 ts → 401；过期 ts（签名正确）→ 401；
    /// 新鲜 ts + `{ts}.{body}` 正签 → 放行；body 原文签名（协议未生效形态）→ 401；
    /// 两层叠加——新鲜 ts 重复 POST → 409。
    #[test]
    fn gate_timestamp_protocol_variants() {
        let body = br#"{"text":"deploy failed"}"#;
        let r = route(Some(SECRET), 300);
        let now = 1_700_000_000_u64;
        let fresh_ts = now - 10;
        let stale_ts = now - 1000; // 超出 300s 窗口

        // 缺失 X-Imagent-Timestamp → 401。
        let sig_fresh = hmac_hex(SECRET, &ts_signing_input(fresh_ts, body));
        let h_no_ts = headers(&[("x-hub-signature-256", format!("sha256={sig_fresh}"))]);
        let err = webhook_gate(&r, "t", &h_no_ts, body, now).unwrap_err();
        assert_eq!(
            (err.0, err.1),
            (
                StatusCode::UNAUTHORIZED,
                "missing or invalid X-Imagent-Timestamp\n"
            )
        );

        // 过期 ts + 对 "{ts}.{body}" 的正确签名 → 401（曾经合法的旧请求 = 重放）。
        let sig_stale = hmac_hex(SECRET, &ts_signing_input(stale_ts, body));
        let h_stale = headers(&[
            ("x-imagent-timestamp", stale_ts.to_string()),
            ("x-hub-signature-256", format!("sha256={sig_stale}")),
        ]);
        let err = webhook_gate(&r, "t", &h_stale, body, now).unwrap_err();
        assert_eq!(
            (err.0, err.1),
            (
                StatusCode::UNAUTHORIZED,
                "timestamp outside replay window\n"
            )
        );

        // 未来侧偏移同样拒绝（绝对差判定）。
        let future_ts = now + 1000;
        let sig_future = hmac_hex(SECRET, &ts_signing_input(future_ts, body));
        let h_future = headers(&[
            ("x-imagent-timestamp", future_ts.to_string()),
            ("x-hub-signature-256", format!("sha256={sig_future}")),
        ]);
        assert_eq!(
            webhook_gate(&r, "t", &h_future, body, now).unwrap_err().0,
            StatusCode::UNAUTHORIZED,
            "未来侧超窗同样拒绝（容忍双向偏移但不放行超窗）"
        );

        // 对 body 原文的签名（协议生效后旧发送端形态）→ 验签即 401。
        let sig_body_only = hmac_hex(SECRET, body);
        let h_body_sig = headers(&[
            ("x-imagent-timestamp", fresh_ts.to_string()),
            ("x-hub-signature-256", format!("sha256={sig_body_only}")),
        ]);
        assert_eq!(
            webhook_gate(&r, "t", &h_body_sig, body, now).unwrap_err().0,
            StatusCode::UNAUTHORIZED,
            "验签串已改为 {{ts}}.{{body}}，body 原文签名不再通过"
        );

        // 新鲜 ts + 正确签名 → 放行（返回去重键）；同键 admit 二次 → 拦截
        //（两层叠加：时间戳拦窗口外，去重拦窗口内同字节重放）。
        let h_fresh = headers(&[
            ("x-imagent-timestamp", fresh_ts.to_string()),
            ("x-hub-signature-256", format!("sha256={sig_fresh}")),
        ]);
        let key = webhook_gate(&r, "t", &h_fresh, body, now)
            .unwrap()
            .expect("新鲜请求应有去重键");
        let guard = ReplayGuard::default();
        assert!(guard.admit(&key, Instant::now()));
        assert!(
            !guard.admit(&key, Instant::now()),
            "时间戳协议与去重叠加：同签名重发被拦"
        );

        // 窗口边界：|now - ts| == window 视为新鲜（端点含）。
        let edge_ts = now - 300;
        let sig_edge = hmac_hex(SECRET, &ts_signing_input(edge_ts, body));
        let h_edge = headers(&[
            ("x-imagent-timestamp", edge_ts.to_string()),
            ("x-hub-signature-256", format!("sha256={sig_edge}")),
        ]);
        assert!(
            webhook_gate(&r, "t", &h_edge, body, now).unwrap().is_some(),
            "窗口端点视为新鲜"
        );
    }

    /// v1.24 fail-closed：非 loopback 且任一条目未配 secret → 拒绝；
    /// loopback 或全部条目配齐 secret → 放行。
    #[test]
    fn non_loopback_webhook_without_secret_is_rejected() {
        let pub_addr: SocketAddr = "0.0.0.0:18443".parse().unwrap();
        let lo_addr: SocketAddr = "127.0.0.1:18443".parse().unwrap();
        let v6lo: SocketAddr = "[::1]:18443".parse().unwrap();
        let mk = |secret: Option<&str>, name: &str| imagent_core::WebhookEntry {
            token: "0123456789abcdef0123456789abcdef".into(),
            conv: "feishu:oc_g".into(),
            name: name.into(),
            secret: secret.map(str::to_string),
            rps: None,
            replay_window_secs: 0,
        };

        assert!(validate_webhook_bind(pub_addr, &[mk(None, "ci")]).is_err());
        // 任一条目未配即拒绝（混配不放行），报错点名具体条目。
        let err = validate_webhook_bind(pub_addr, &[mk(Some("s-pair-01"), "ci"), mk(None, "cron")])
            .unwrap_err();
        assert!(
            err.contains("name=cron") && err.contains("secret"),
            "msg={err}"
        );
        assert!(err.contains("127.0.0.1"), "给出可操作修复指引：{err}");
        assert!(validate_webhook_bind(pub_addr, &[mk(Some("s-pair-01"), "ci")]).is_ok());
        assert!(validate_webhook_bind(lo_addr, &[mk(None, "ci")]).is_ok());
        assert!(validate_webhook_bind(v6lo, &[mk(None, "ci")]).is_ok());
        // 空表 + 非 loopback：server 本就不启动，校验不阻拦（保持既有 warn 路径）。
        assert!(validate_webhook_bind(pub_addr, &[]).is_ok());
    }
}

/// 依赖升级批（v1.30.1）回归：`HOOK_ROUTE` 的参数语法必须与**当前 axum 版本**
/// 的 matchit 语法一致——axum 0.7 下 `{token}` 是字面量段（编译照过、运行时
/// webhook 全量 404）。用同一常量装一枚极简 router、原生 TCP 发真实 POST，
/// 断言拿到非 404 响应（这里 handler 恒回 200，路由不匹配才会 404）——
/// axum 升降版本时若语法错位，本测试直接红。
#[tokio::test]
async fn hook_route_matches_under_current_axum() {
    use axum::routing::post;

    let app = axum::Router::new().route(HOOK_ROUTE, post(|| async { axum::http::StatusCode::OK }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    // 环境代理不得劫持 loopback 直连。
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    use tokio::io::AsyncWriteExt as _;
    stream
        .write_all(
            b"POST /hook/some-token-value HTTP/1.1\r\nhost: localhost\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let mut buf = Vec::new();
    use tokio::io::AsyncReadExt as _;
    let _ = stream.read_to_end(&mut buf).await.unwrap();
    let head = String::from_utf8_lossy(&buf);
    let status = head
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .unwrap_or_default();
    assert_eq!(
        status,
        "200",
        "HOOK_ROUTE 未匹配（状态行：{}）——axum 版本与路径参数语法错位？",
        head.lines().next().unwrap_or_default()
    );
}
