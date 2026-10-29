//! `imagent-relay`：Web 驾驶舱——ECS 侧 WS 帧中继 + 会话层
//!（设计见 docs/WEB_DESIGN.md §2/§3.1，规格以任务评审修订版为准）。
//!
//! 拓扑：`浏览器 WS（多连接并存）⇄ relay ⇄ 网关 WS（单连接）`，连接都是
//! 客户端模式连到本进程（Mac 网关零入站端口）。本 crate 是中继：
//! **不理解任何业务语义**，只做认证（fail-closed、网关 token-machine
//! 绑定 + ed25519 挑战应答、浏览器会话层）、转发（Hub 状态机）、网关离线
//! 缓冲（200 帧 / 1MB）、每连接限流（20 帧 / 10s）。部署形态：ECS systemd
//! 服务绑 127.0.0.1:3010，公网由 Caddy（8443，TLS 终结）
//! `handle_path /imagent/*` strip 前缀后反代——relay 看到的路径已是 strip
//! 后的 `/xxx`。
//!
//! 端点语义（真机常态：手机 + 电脑 + 多标签同时在线）：
//! - **浏览器侧多连接并存 + 广播**：同一 user token 的多条 browser 连接
//!   同时注册在 Hub（并存上限 [`MAX_BROWSERS`] = 8，防滥用——满时新连接
//!   认证通过后仍被 close 4001，复用 superseded 让位语义，SPA 已有 4001
//!   终态处理：不重连、不与在线连接互踢）；gateway→browser 业务帧与
//!   machines 上/下线通知向**全部**在线 browser 广播（逐个 try_send，单个
//!   打满/关闭不影响其余）；任一 browser 的入站帧按 machine 字段路由给
//!   对应网关（或入离线缓冲），见 [`Hub::route_browser_frame`]。
//! - **网关侧多机器并存（帧协议 v1.1，M5 多终端管理）**：每条网关连接在
//!   hello 帧自报 `machine` 身份（Bearer 直通回退兼容 = `"default"` 机器），
//!   Hub 按 machine 槽位存多连接（上限 [`MAX_GATEWAYS`] = 4——满员且非同
//!   机重连的新连接 close 4001）；**同一 machine 重连仍踢旧**（每台机器
//!   单连接语义，旧连接 close 4001 superseded）。
//! - **网关 token-machine 绑定（帧协议 v1.2，L2 安全加固）**：env
//!   `IMAGENT_RELAY_GATEWAY_TOKENS`（`home-mac=9a08…,work-mac=7f2b…`，
//!   每台 Mac 一个独立 token，见 [`GatewayTokens`]）：token 匹配**且**
//!   hello 自报 machine 与绑定一致才放行——一台 Mac 的 token 泄露只能
//!   扮演它自己，不能顶替其它机器。旧单 token env
//!   （`IMAGENT_RELAY_GATEWAY_TOKEN`）已删除；两者都未设拒启
//!   （fail-closed）。
//! - **网关 ed25519 挑战应答（帧协议 v1.3，L3 安全加固）**：hello 不带
//!   token 的网关走公私钥握手——`hello{machine}` → relay 下发一次性
//!   `challenge{nonce}`（32B hex，60s 过期单次消费，见
//!   [`ChallengeTable`]）→ 网关回 `prove{pubkey,signature}` → relay 用
//!   env `IMAGENT_RELAY_GATEWAY_KEYS`（`home-mac=<64 hex 公钥>`，见
//!   [`GatewayKeys`]）绑定的公钥验签。防的是「ECS 被攻破 = 网关身份
//!   凭证泄露」：relay 只存公钥，私钥 seed（0600 文件）永不出网关本机；
//!   一次性 nonce 防录制重放。**token 路径完整保留**（hello 带 token 走
//!   既有一步校验；KEYS/TOKENS 双表并存，机器在任一表命中即放行——迁移
//!   期双保险，旧网关/旧 env 零改动；两表均空仍拒启 fail-closed）。
//!   认证失败不区分原因（统一 1s 延迟 + close 1008，防探测）。
//! - **浏览器会话层（帧协议 v1.2，L2 安全加固，见 [`sessions`]）**：
//!   USER_TOKEN 仅登录 hello 一次性换发 30 天会话凭证（sessionToken），
//!   此后重连用 sessionToken（SPA 不再长期持有 USER_TOKEN）；会话可列表
//!   /吊销（`device_list` / `device_revoke` transport 管理帧）；会话表落
//!   盘 `<state_dir>/sessions.json`（env `IMAGENT_RELAY_STATE_DIR`，缺省
//!   `/var/lib/imagent-relay`，main 负责 mkdir）——**部署注意**：systemd
//!   需配 `StateDirectory=imagent-relay`（落于同一路径）或显式设该 env，
//!   否则写盘失败只会降级为「重启后全员重登」。
//! - **passkey 登录（帧协议 v1.3，见 [`webauthn`]）**：relay 作为 WebAuthn
//!   Relying Party——`POST /webauthn/{register,login}/{begin,complete}` 四
//!   端点（注册经 `Authorization: Bearer <sessionToken>` 认证，登录免认证、
//!   discoverable credential 免选用户）；登录成功直接换发 L2 会话（响应与
//!   hello 换发同构）。凭证表落盘 `<state_dir>/credentials.json`，管理帧
//!   `cred_list` / `cred_revoke`（设备管理弹层）。rp_id 缺省按请求 Host
//!   去端口推导，env `IMAGENT_RELAY_RP_ID` 可覆盖；本地 http IP 直连形态
//!   不可用 passkey（secure context + rp_id 域名校验），走 USER_TOKEN 路径。
//! - **E2E 中继盲化（帧协议 v1.4）**：浏览器↔网关业务帧以 `enc{sid,box}`
//!   包装——内容 XChaCha20-Poly1305 端到端加密（密钥协商经 `e2e_hello` /
//!   `e2e_ack` 握手帧完成，relay 透传不理解），本中继保持「盲」：唯一新增
//!   认知是 enc 帧信封的 `sid` 字段（transport 路由键，与 `machine` 同级，
//!   见 [`frame_sid`]）——非空且匹配在线浏览器会话 → 只投该连接（逐浏览器
//!   独立密钥的前提）；空/无匹配 → 广播现状。enc 帧不进 sync/ping 歧义
//!   豁免表（照常 machine 路由）。
//! - **准入与管理面（管理批）**：机器公钥表从 env 静态迁移为
//!   `<state_dir>/machines.json` 落盘 + 热更新（env `GATEWAY_KEYS` 仅首次
//!   种子，见 [`machines`]）；USER_TOKEN 哈希化落盘
//!   （`<state_dir>/user_token_hash`，env 仅首次种子，见
//!   [`sessions::UserTokenStore`]）。管理操作须 passkey re-auth
//!   （`POST /webauthn/reauth/{begin,complete}` → 5 分钟 adminToken）：四个
//!   管理帧 `machine_list` / `machine_add` / `machine_remove` /
//!   `user_token_rotate` + 配对批准 `pairing_approve` / `pairing_deny`。
//!   新设备配对流（无凭证 hello `pairing:true` → 6 位码 pending → 广播
//!   `pairing_request` → 批准换发 session / 拒绝 close 1008）见 [`pairing`]。
//!
//! 分层（任务说明 §4.7）：核心状态机全部在本 lib（可单测 + 集成测试直接
//! [`spawn_relay`] 起真 server），`main.rs` 只做 env / bind / tracing 组装。

#![deny(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::header::HeaderName;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use futures::StreamExt;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

mod machines;
mod pairing;
mod sessions;
mod webauthn;

use pairing::PairingSignal;
use sessions::{DeviceView, SessionLookup, SessionStore, UserTokenStore};

/// 统一 tracing target（仓库纪律）。
const TARGET: &str = "imagent::relay";

/// 认证前等待 hello 首帧的超时（§4.2）：hello 迟迟不来直接 close，
/// 防止未认证连接长期占用资源。
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// 认证失败后的关停延迟（§4.2）：给爆破加节奏（1s/次探测）。
const AUTH_FAIL_DELAY: Duration = Duration::from_secs(1);
/// 认证后空闲超时（§4.4）：90s 未收到任何 WS message（含 pong）→ close 1000。
/// 保活 ping 由客户端负责（网关 30s ping，浏览器发 ping 文本帧——端到端
/// 语义，relay 透传，自己不做应用层应答）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// 限流窗口（§4.4）：任意 10s 滑动窗口内 ≤ [`RATE_MAX_FRAMES`] 帧。
const RATE_WINDOW: Duration = Duration::from_secs(10);
const RATE_MAX_FRAMES: usize = 20;
/// 网关离线缓冲上限（§4.3）：帧数 / 字节数先到为准。
const BUF_MAX_FRAMES: usize = 200;
const BUF_MAX_BYTES: usize = 1024 * 1024;
/// 单 WS message / 单 frame 上限 256KB（§4.4）：超限 axum 自会以 1009 断开。
const MAX_WS_MESSAGE_BYTES: usize = 256 * 1024;
/// 每连接出站 channel 容量。上限推导：网关侧 hello_ack + 离线缓冲全量
/// 重放（≤200 帧）、浏览器侧 hello_ack + machine/machines 双快照，必须在
/// **进入消费循环之前**一次性入队也不阻塞，故 ≥202；取 512 留余量。业务
/// 帧转发走 try_send（打满即丢，见 [`try_send`]），连接之间不存在互相
/// await 的投递环，不会死锁。
const CHANNEL_CAPACITY: usize = 512;
/// 浏览器侧最多并存连接数（防滥用）：真机常态是手机 + 电脑 + 多标签
///（≤3~4 条），取 8 留余量。满时新连接认证通过后仍被 close 4001
///（复用 superseded 让位语义，SPA 已有 4001 终态处理：不重连）。
const MAX_BROWSERS: usize = 8;
/// 网关侧最多并存机器数（v1.1 多终端管理）：每条网关连接按 hello 自报的
/// machine 占一个槽位。真机常态是 2~3 台（家/公司/便携），取 4 留余量；
/// 满员且非同机重连的新连接 close 4001（同 machine 重连不受限——踢旧
/// 顶替，每台机器单连接语义）。
const MAX_GATEWAYS: usize = 4;
/// 缺省 machine id（v1.1）：hello 不带 machine / Bearer 直通（无 machine
/// 维度）都落到 default——与单机时期的语义完全一致（旧网关/旧 SPA 回退
/// 兼容），旧 `machine` 帧也只表达 default 机器的在线状态。
const DEFAULT_MACHINE: &str = "default";
/// machine id 协议约束（v1.1）：≤32 char，仅 ASCII 字母数字与 `-_`。
const MACHINE_ID_MAX_CHARS: usize = 32;
/// 会话有效期缺省（协议 v1.2）：30 天；测试可经
/// [`RelayConfig::session_ttl_secs`] 缩短验证过期路径。
pub const DEFAULT_SESSION_TTL_SECS: u64 = 30 * 24 * 3600;
/// hello `device` 字段协议约束（v1.2）：≤64 char。
const DEVICE_NAME_MAX_CHARS: usize = 64;
/// 连接日志里 session id 的展示前缀长度（完整 UUID 不进日志噪音）。
const SESSION_LOG_PREFIX: usize = 8;
/// challenge nonce 有效期（协议 v1.3）：60s 内须回 prove，过期作废。
const CHALLENGE_TTL: Duration = Duration::from_secs(60);
/// challenge 表容量上限（协议 v1.3 的防滥用护栏）：无凭证 hello 也会触发
/// challenge 签发（anti-probe：不区分 machine 是否在表），上限防未认证
/// 连接洪水撑爆内存。签发时先清过期，仍满则逐出**最早过期**（= 最早签发）
/// 的条目——正常网关握手毫秒级完成，60s 窗口内 1024 个并发未完成握手已
/// 远超真实部署形态。
const CHALLENGE_MAX_ENTRIES: usize = 1024;
/// 新设备配对 pending 码有效期缺省（管理批）：5 分钟；测试可经
/// [`RelayConfig::pairing_ttl_secs`] 缩短验证过期路径（单一事实源在
/// [`pairing::PAIRING_TTL_SECS`]）。
pub const DEFAULT_PAIRING_TTL_SECS: u64 = pairing::PAIRING_TTL_SECS;

/// WS close code（RFC 6455 + 私有码，§4.2/§4.3/§4.5）。
const CLOSE_POLICY: u16 = 1008; // 认证失败 / 协议违规（binary、非 hello 首帧）/ 限流
const CLOSE_NORMAL: u16 = 1000; // idle 超时正常关闭
const CLOSE_GOING_AWAY: u16 = 1001; // 服务停机
const CLOSE_SUPERSEDED: u16 = 4001; // 网关新连接顶替旧连接（踢旧）；browser 并存数达上限

// ---------------------------------------------------------------------------
// 配置与 fail-closed 校验
// ---------------------------------------------------------------------------

/// 网关 token-machine 绑定表（协议 v1.2，L2 安全加固）：每台 Mac 一个
/// 独立 token，env `IMAGENT_RELAY_GATEWAY_TOKENS`（逗号分隔的
/// `machine=token` 对，如 `home-mac=9a08…,work-mac=7f2b…`）。防的是「单
/// token 泄露即可扮演任意机器」——绑定校验下，一台 Mac 的 token 泄露只能
/// 扮演它自己（token 匹配**且** hello 自报 machine 与绑定一致才放行）。
///
/// 旧单 token env（`IMAGENT_RELAY_GATEWAY_TOKEN`）已删除：部署面只有我们
/// 自己的 ECS，升级时同步换 env——ECS 侧改为本表形态；每台 Mac 的
/// `~/.imagent/web-relay.env`（`IMAGENT_WEB_GATEWAY_TOKEN`）填**自己
/// machine** 对应的 token（`crates/web` 的 gateway_token 参数不变）。
#[derive(Clone, Debug, Default)]
pub struct GatewayTokens {
    /// machine → token（machine 已过与 hello `machine` 同款的合法性校验）。
    map: HashMap<String, String>,
}

impl GatewayTokens {
    /// 解析 `machine=token` 逗号分隔表（纯函数便于单测）。fail-closed：
    /// 整体为空 / 表项缺 `=` / machine 或 token 空 / machine 非法（同
    /// machine id 协议约束）/ machine 重复 / token 重复绑定 → Err（可读
    /// 信息含修复指引）。空表项（尾随逗号）容忍跳过——有意义的失败
    ///（缺 `=`、空段值、重复）都会被抓住。
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut map: HashMap<String, String> = HashMap::new();
        let mut seen_tokens: HashMap<String, String> = HashMap::new();
        for pair in spec.split(',') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            let (machine, token) = pair.split_once('=').ok_or_else(|| {
                format!("网关 token 表项 `{pair}` 缺 `=`（格式：machine=token 逗号分隔）")
            })?;
            let (machine, token) = (machine.trim(), token.trim());
            if machine.is_empty() || token.is_empty() {
                return Err(format!(
                    "网关 token 表项 `{pair}` 的 machine 或 token 为空（格式：machine=token）"
                ));
            }
            if !valid_machine_id(machine) {
                return Err(format!(
                    "网关 token 表项的 machine `{machine}` 非法：≤32 char，仅 ASCII 字母数字与 -_（与 hello machine 同款约束）"
                ));
            }
            if map.insert(machine.to_string(), token.to_string()).is_some() {
                return Err(format!(
                    "网关 token 表项的 machine `{machine}` 重复（每台机器一个 token）"
                ));
            }
            if let Some(first) = seen_tokens.insert(token.to_string(), machine.to_string()) {
                return Err(format!(
                    "网关 token `{}`… 同时绑定 `{first}` 与 `{machine}`：一个 token 只能绑一台机器（绑定语义）",
                    token.chars().take(4).collect::<String>()
                ));
            }
        }
        if map.is_empty() {
            return Err(
                "网关 token 表为空：必填 IMAGENT_RELAY_GATEWAY_TOKENS（machine=token 逗号分隔，\
                 每台 Mac 一个独立 token；openssl rand -hex 32 生成）"
                    .into(),
            );
        }
        Ok(Self { map })
    }

    /// 绑定校验（协议 v1.2）：token 逐表项**恒定时间**匹配（不提前
    /// break），命中后校验 machine 与绑定一致。Bearer 直通无 machine 维度
    /// → 调用方传 [`DEFAULT_MACHINE`]（只有绑定 default 的 token 可走
    /// Bearer）。machine 明文等值比较——machine 本就随路由明文出现，不是
    /// 秘密；恒定时间只保护 token。
    pub fn verify(&self, machine: &str, token: &str) -> bool {
        let mut bound: Option<&String> = None;
        for (m, t) in &self.map {
            if constant_time_eq(token, t) {
                bound = Some(m); // 不 break：恒定时间全表扫描
            }
        }
        bound.is_some_and(|m| m == machine)
    }

    /// 已绑定的 machine 列表（有序，启动日志用；token 绝不外泄）。
    pub fn machines(&self) -> Vec<String> {
        let mut v: Vec<String> = self.map.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 是否有 token 与给定值相同（RelayConfig 职责分离校验用；配置期
    /// 等值比较即可，不涉运行时探测）。
    fn contains_token(&self, token: &str) -> bool {
        self.map.values().any(|t| t == token)
    }
}

/// 网关公钥表条目：原始 hex（恒定时间比对用）+ 解析好的验签公钥。
#[derive(Clone, Debug)]
struct GatewayKeyEntry {
    /// 64 hex 公钥原文（prove.pubkey 与之恒定时间比对）。
    hex: String,
    /// 解析好的 ed25519 验签公钥（parse 期已校验点合法性）。
    key: VerifyingKey,
}

/// 网关 machine-公钥绑定表（协议 v1.3，L3 安全加固）：env
/// `IMAGENT_RELAY_GATEWAY_KEYS`（逗号分隔的 `machine=<64 hex ed25519 公钥>`
/// 对，如 `home-mac=9a08…,work-mac=7f2b…`，与 GATEWAY_TOKENS 同格式）。
/// 防的是「ECS 被攻破 = 网关身份凭证泄露」：relay 只存**公钥**，拿到全表
/// 也无法扮演任何网关（私钥 seed 永不出网关本机）；一次性 nonce 挑战应答
/// 防录制重放。与 [`GatewayTokens`] 双表并存：机器在任一表命中即放行
///（迁移期双保险）；两表均空由 [`RelayConfig::validate`] fail-closed 拒启。
#[derive(Clone, Debug, Default)]
pub struct GatewayKeys {
    /// machine → 公钥（machine 已过与 hello `machine` 同款的合法性校验）。
    map: HashMap<String, GatewayKeyEntry>,
}

impl GatewayKeys {
    /// 解析 `machine=<64 hex 公钥>` 逗号分隔表（纯函数便于单测）。与
    /// [`GatewayTokens::parse`] 同款表项纪律：缺 `=` / machine 或公钥空 /
    /// machine 非法 / machine 重复 / 公钥重复绑定（一个公钥只能绑一台
    /// 机器）→ Err；公钥须为合法 hex 且能解出 ed25519 公钥点。**空表 Ok**
    ///（不是错误——「KEYS/TOKENS 至少一个非空」的 fail-closed 判定在
    /// [`RelayConfig::validate`]，只配 TOKENS 的既有部署零改动）。
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut map: HashMap<String, GatewayKeyEntry> = HashMap::new();
        let mut seen_keys: HashMap<String, String> = HashMap::new();
        for pair in spec.split(',') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            let (machine, pubkey) = pair.split_once('=').ok_or_else(|| {
                format!("网关公钥表项 `{pair}` 缺 `=`（格式：machine=<64 hex 公钥> 逗号分隔）")
            })?;
            let (machine, pubkey) = (machine.trim(), pubkey.trim());
            if machine.is_empty() || pubkey.is_empty() {
                return Err(format!(
                    "网关公钥表项 `{pair}` 的 machine 或公钥为空（格式：machine=<64 hex 公钥>）"
                ));
            }
            if !valid_machine_id(machine) {
                return Err(format!(
                    "网关公钥表项的 machine `{machine}` 非法：≤32 char，仅 ASCII 字母数字与 -_（与 hello machine 同款约束）"
                ));
            }
            let bytes = hex::decode(pubkey).map_err(|_| {
                format!("网关公钥表项 `{machine}` 的公钥非合法 hex（须为 ed25519 公钥 64 hex，网关侧启动日志打印）")
            })?;
            let arr: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
                format!(
                    "网关公钥表项 `{machine}` 的公钥长度 {:?} 非法（须恰为 64 hex = 32 字节）",
                    v.len() * 2
                )
            })?;
            let key = VerifyingKey::from_bytes(&arr)
                .map_err(|_| format!("网关公钥表项 `{machine}` 的公钥不是合法 ed25519 公钥点"))?;
            if map
                .insert(
                    machine.to_string(),
                    GatewayKeyEntry {
                        hex: pubkey.to_string(),
                        key,
                    },
                )
                .is_some()
            {
                return Err(format!(
                    "网关公钥表项的 machine `{machine}` 重复（每台机器一个公钥）"
                ));
            }
            if let Some(first) = seen_keys.insert(pubkey.to_string(), machine.to_string()) {
                return Err(format!(
                    "网关公钥 `{}`… 同时绑定 `{first}` 与 `{machine}`：一个公钥只能绑一台机器（绑定语义）",
                    pubkey.chars().take(4).collect::<String>()
                ));
            }
        }
        Ok(Self { map })
    }

    /// prove 验证（协议 v1.3 ①+②）：machine 在表中**且**绑定公钥与
    /// prove.pubkey 恒定时间一致（公钥虽是公开材料，任务定案要求恒定时间
    /// 比对，复用 [`constant_time_eq`]）**且** ed25519 验签 nonce 通过。
    /// `nonce` 为 challenge 下发的 hex 字符串原文（网关对收到的 nonce
    /// 原样签名，验签对象即其 UTF-8 字节——两侧无解码歧义）。任何一步
    /// 失败统一 false（调用方不区分原因，防探测）。
    pub fn verify(&self, machine: &str, pubkey: &str, nonce: &str, signature: &str) -> bool {
        let Some(entry) = self.map.get(machine) else {
            return false;
        };
        if !constant_time_eq(pubkey, entry.hex.as_str()) {
            return false;
        }
        let Ok(sig) = hex::decode(signature) else {
            return false;
        };
        let Ok(sig) = <[u8; 64]>::try_from(sig) else {
            return false;
        };
        entry
            .key
            .verify(nonce.as_bytes(), &Signature::from_bytes(&sig))
            .is_ok()
    }

    /// 已绑定的 machine 列表（有序，启动日志用）。
    pub fn machines(&self) -> Vec<String> {
        let mut v: Vec<String> = self.map.keys().cloned().collect();
        v.sort();
        v
    }

    /// 公钥指纹表（有序，启动日志用）：`machine=<公钥前 16 hex>`——公钥是
    /// 公开材料可全量打印，取指纹纯为日志降噪；排查握手不匹配时用网关侧
    /// 启动日志的完整公钥对照。
    pub fn fingerprints(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .map
            .iter()
            .map(|(m, e)| format!("{m}={}", &e.hex[..16]))
            .collect();
        v.sort();
        v
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// machine → 公钥 hex 迭代（[`crate::machines::MachineStore::load`] 首次
    /// 种子导入用；运行时认证走 MachineStore，本表只在启动期消费一次）。
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.map.iter().map(|(m, e)| (m, &e.hex))
    }
}

/// challenge nonce 表条目：签发时绑定 hello 自报的 machine，60s 过期。
struct ChallengeEntry {
    machine: String,
    expires_at: Instant,
}

/// 网关 key 握手的 challenge 表（协议 v1.3，内存态、进程重启即清空——
/// 挑战应答本就要求握手当次完成，无跨重启语义）：`nonce → machine`，
/// 60s 过期、**单次消费**（consume 即移除，验签失败也不可重试同一 nonce
/// ——防重放的核心）。容量护栏 [`CHALLENGE_MAX_ENTRIES`] 见其注释。
/// 时间由调用方注入（单测可拨钟）；std Mutex 纯内存临界区，与 Hub 同款
/// 「锁内计算不跨 await」纪律。
#[derive(Default)]
struct ChallengeTable {
    map: HashMap<String, ChallengeEntry>,
}

impl ChallengeTable {
    /// 签发一次性 nonce（32B 随机 → 64 hex）并绑定 machine。先清过期条目；
    /// 仍满容量则逐出最早过期（= 最早签发）的条目。
    fn issue(&mut self, machine: &str, now: Instant) -> String {
        self.prune(now);
        if self.map.len() >= CHALLENGE_MAX_ENTRIES {
            if let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, e)| e.expires_at)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&oldest);
            }
        }
        let nonce = hex::encode(rand::random::<[u8; 32]>());
        self.map.insert(
            nonce.clone(),
            ChallengeEntry {
                machine: machine.to_string(),
                expires_at: now + CHALLENGE_TTL,
            },
        );
        nonce
    }

    /// 消费 nonce：命中即移除（单次），返回绑定的 machine；未知 / 已消费 /
    /// 已过期 → None。
    fn consume(&mut self, nonce: &str, now: Instant) -> Option<String> {
        let entry = self.map.remove(nonce)?;
        (now < entry.expires_at).then_some(entry.machine)
    }

    /// 清除已过期条目（issue/consume 时顺带执行，无需独立定时器）。
    fn prune(&mut self, now: Instant) {
        self.map.retain(|_, e| now < e.expires_at);
    }
}

/// relay 运行配置。main 从 env 读取；测试直接构造。
#[derive(Clone)]
pub struct RelayConfig {
    /// 网关侧 token-machine 绑定表（`IMAGENT_RELAY_GATEWAY_TOKENS`）。
    /// **env 静态兼容路径**（管理批不管理面化）：迁移期保留，机器切到
    /// key 握手后可移除。
    pub gateway_tokens: GatewayTokens,
    /// 网关侧 machine-公钥绑定表（`IMAGENT_RELAY_GATEWAY_KEYS`）——**仅首次
    /// 种子**（管理批起运行表为 `<state_dir>/machines.json` 落盘 + 热更新，
    /// 见 [`machines::MachineStore`]）：文件不存在时种子导入，存在则本表
    /// 被忽略（文件权威）。
    pub gateway_keys: GatewayKeys,
    /// 浏览器侧登录 token（`IMAGENT_RELAY_USER_TOKEN`）——**仅首次种子**
    /// （管理批起运行值为 `<state_dir>/user_token_hash` 哈希落盘，见
    /// [`sessions::UserTokenStore`]）：文件不存在时种子写哈希，存在则本值
    /// 被忽略（文件权威）。可为空串（迁移后 env 已移除的形态）。
    pub user_token: String,
    /// 会话状态目录（`IMAGENT_RELAY_STATE_DIR`，缺省
    /// `/var/lib/imagent-relay`，main 负责 mkdir）：会话表落盘
    /// `<dir>/sessions.json`，机器表 `<dir>/machines.json`，USER_TOKEN 哈希
    /// `<dir>/user_token_hash`，passkey 凭证 `<dir>/credentials.json`。
    /// systemd 部署可用 `StateDirectory=imagent-relay` 落到同一路径。
    pub state_dir: PathBuf,
    /// 会话有效期（秒）：缺省 [`DEFAULT_SESSION_TTL_SECS`]（30 天）；测试
    /// 可缩短验证过期路径。
    pub session_ttl_secs: u64,
    /// 新设备配对 pending 码有效期（秒）：缺省
    /// [`DEFAULT_PAIRING_TTL_SECS`]（5 分钟）；测试可缩短验证过期路径。
    pub pairing_ttl_secs: u64,
    /// passkey RP ID 覆盖（`IMAGENT_RELAY_RP_ID`，缺省 None = 按请求 Host
    /// 去端口推导）：固定 rp_id 后换域名/端口不动已注册凭证（WebAuthn
    /// 凭证绑定 rp_id 而非 origin）。
    pub webauthn_rp_id: Option<String>,
}

impl RelayConfig {
    /// 配置期 fail-closed 校验（纯函数便于单测）。运行期校验（落盘权威
    /// 表的「至少一个非空」/「USER_TOKEN 种子或文件必居其一」）在
    /// [`spawn_relay`] 组装真实存储后执行——本函数只看得到 env 值。保留：
    /// 职责分离——env USER_TOKEN 种子与任一网关 token 相同 → 拒（拿到一
    /// 个 token 不能同时扮演网关与浏览器两端；公钥无须此项：持有公钥不
    /// 构成任何一端的登录能力）。
    pub fn validate(&self) -> Result<(), String> {
        if !self.user_token.is_empty() && self.gateway_tokens.contains_token(&self.user_token) {
            return Err(
                "IMAGENT_RELAY_USER_TOKEN 不得与任何网关 token 相同：职责分离，\
                 防止拿到一个 token 就能扮演网关与浏览器两端"
                    .into(),
            );
        }
        Ok(())
    }
}

/// §4.5 fail-closed 绑定校验（哲学对齐 `src/webhook.rs` 的
/// `validate_webhook_bind`，纯函数便于单测）：relay 必须绑 loopback——公网
/// 入口只允许 Caddy（TLS 终结 + 统一暴露面），直接对外绑定等于绕过 TLS
/// 与 Caddy 层防护。给出可读错误说明原因。
pub fn validate_bind(addr: SocketAddr) -> Result<(), String> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    Err(format!(
        "IMAGENT_RELAY_BIND={addr} 非 loopback 地址：relay 只允许绑 127.0.0.1（或 [::1]），\
         公网入口必须由 Caddy（8443，TLS 终结）反代；请改回 loopback 绑定"
    ))
}

/// token 日志脱敏（§4.5 日志纪律：绝不 log 完整 token，只露前 4 位）。
pub fn mask_token(token: &str) -> String {
    format!("{}…", token.chars().take(4).collect::<String>())
}

/// 机器表落盘文件（`<state_dir>/machines.json`）是否已存在（main 的启动
/// 期 fail-closed 判定用：文件在 = 管理面已接管机器表，env 两网关认证表
/// 均空不再拒启——「UI 删光机器后重启 brick」不是 fail-closed 想防的）。
pub fn machines_table_exists(state_dir: &std::path::Path) -> bool {
    machines::table_file_exists(state_dir)
}

/// 常数时间比较（防时序侧信道）。写法对齐 `src/webhook.rs`
/// `verify_webhook_signature` 尾部的 XOR 累计——不引入 subtle 依赖的
/// 仓库既有先例：长度差异单独进 diff，字节差异逐位 OR 累计。
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let n = a.len().max(b.len());
    let mut diff: u8 = (a.len() != b.len()) as u8;
    for i in 0..n {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// hello 首帧解析（§4.2 主认证形态 + v1.2 会话层）
// ---------------------------------------------------------------------------

/// hello 首帧反序列化形状（v1.2 会话层后的全量字段 + 管理批配对入口）：
/// `{"v":1,"type":"hello","token":"…","machine":"…","session":"…","device":"…","pairing":true}`
/// - 网关侧：`token`（machine 绑定校验）+ 可选 `machine` 自报身份
///   （v1.1）；v1.3 起也接受**不带任何凭证**的 hello（走 key 挑战应答，
///   见 [`HelloCredential::Challenge`]）；网关无会话概念，`session` 字段
///   一律拒绝；
/// - 浏览器侧：`session`（重连）优先，`token`（USER_TOKEN 登录换发）次之，
///   可选 `device`（≤64 char，缺省记 UA 摘要）；浏览器无 machine 概念
///   （带了也只经校验不消费）；
/// - 配对入口（管理批）：`pairing:true` 且**无任何凭证**——新设备首配
///   形态，分拣见 [`PairingHello`]（`pairing:true` 与凭证并存按认证失败
///   拒绝，不留「带 token 的配对」歧义形态）。
#[derive(Deserialize)]
struct HelloFrame {
    v: i64,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    machine: Option<String>,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    pairing: Option<bool>,
}

/// hello 携带的凭证（v1.2 + v1.3）：`session` 字段优先（重连），`token`
/// 次之（登录换发/网关认证）；**两者均缺席**（注意：缺席而非空串）为
/// [`HelloCredential::Challenge`]（v1.3 网关 key 握手入口，浏览器侧在
/// 认证层一律拒绝）。
#[derive(Debug, PartialEq, Eq)]
enum HelloCredential {
    /// USER_TOKEN（浏览器登录换发路径）/ 网关 token（machine 绑定校验）。
    Bootstrap(String),
    /// 浏览器会话凭证（重连路径）。
    Session(String),
    /// 无静态凭证（v1.3）：网关 ed25519 挑战应答握手的入口形态——
    /// `{"v":1,"type":"hello","machine":"home-mac"}`。仅网关侧认证
    /// 分流消费；浏览器侧无此路径（challenge → "auth failed"）。
    Challenge,
}

/// hello 解析产物。
#[derive(Debug, PartialEq, Eq)]
struct ParsedHello {
    credential: HelloCredential,
    /// 网关侧自报身份（缺省 `"default"`；浏览器侧只校验不消费）。
    machine: String,
    /// 浏览器侧设备名（trim 后非空才算；缺省走 UA 摘要，见 `device_name`）。
    device: Option<String>,
}

/// hello 解析失败原因（只为日志可读；对外行为一致——close 1008）。
#[derive(Debug, PartialEq, Eq)]
enum HelloParseError {
    /// JSON 畸形 / 缺任何凭证（token 与 session 皆无）/ 凭证空串 /
    /// machine 非法（空串、trim 后为空、超 32 char、含字母数字-_ 之外的
    /// 字符）/ device 超 64 char。
    Malformed,
    /// 首帧 type 不是 hello。
    NotHello,
    /// 信封版本不为 1。
    BadVersion,
}

/// machine id 合法性判定（trim 后非空、≤32 char、仅 ASCII 字母数字与
/// `-_`）：hello 自报 machine 与 [`GatewayTokens`] 的绑定 machine 共用
/// 同一约束。
fn valid_machine_id(m: &str) -> bool {
    !m.is_empty()
        && m.chars().count() <= MACHINE_ID_MAX_CHARS
        && m.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// machine id 校验（v1.1 协议约束，纯函数便于单测）：trim、非空、
/// ≤32 char、仅 ASCII 字母数字与 `-_`。通过返回 trim 后的值。
fn validate_machine(raw: &str) -> Result<String, HelloParseError> {
    let m = raw.trim();
    if valid_machine_id(m) {
        Ok(m.to_string())
    } else {
        Err(HelloParseError::Malformed)
    }
}

/// trim 后非空才算有值（None = 字段缺省/空白）。
fn non_empty_field(raw: &Option<String>) -> Option<String> {
    let t = raw.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
    Some(t.to_string())
}

/// 解析首帧 hello（纯函数便于单测）。凭证分拣（v1.2 + v1.3）：`session`
/// 字段优先、`token` 字段次之（换发路径）、**两者均缺席**为
/// [`HelloCredential::Challenge`]（v1.3 网关 key 握手入口；空串凭证仍按
/// Malformed 拒——空串不可能是「有意的挑战形态」，提前拒绝语义更明确）。
/// machine 缺省 → `"default"`，带值经 [`validate_machine`] 校验
///（machine 是路由主键，脏值宁可断连也不静默改写）。device ≤64 char
///（超限 Malformed），trim 后为空按缺省处理。
fn parse_hello(text: &str) -> Result<ParsedHello, HelloParseError> {
    let h: HelloFrame = serde_json::from_str(text).map_err(|_| HelloParseError::Malformed)?;
    if h.v != 1 {
        return Err(HelloParseError::BadVersion);
    }
    if h.kind != "hello" {
        return Err(HelloParseError::NotHello);
    }
    let credential = match (non_empty_field(&h.session), non_empty_field(&h.token)) {
        (Some(s), _) => HelloCredential::Session(s),
        (None, Some(t)) => HelloCredential::Bootstrap(t),
        // 两者均缺席（None 而非空串）→ v1.3 网关 key 握手入口。
        (None, None) if h.session.is_none() && h.token.is_none() => HelloCredential::Challenge,
        // 有字段但值为空/空白 → Malformed（空串凭证不构成任何合法形态）。
        (None, None) => return Err(HelloParseError::Malformed),
    };
    let machine = match h.machine.as_deref() {
        None => DEFAULT_MACHINE.to_string(),
        Some(m) => validate_machine(m)?,
    };
    let device = non_empty_field(&h.device);
    if device
        .as_deref()
        .is_some_and(|d| d.chars().count() > DEVICE_NAME_MAX_CHARS)
    {
        return Err(HelloParseError::Malformed);
    }
    Ok(ParsedHello {
        credential,
        machine,
        device,
    })
}

/// 浏览器 hello 的配对入口分拣（管理批，纯函数便于单测）：
/// - `pairing:true` 且**无任何凭证**（token/session 均缺席）→ [`PairingHello::Pairing`]
///   （新设备首配形态，设备名同款 trim/≤64 约束，超限 Reject）；
/// - `pairing:true` 与凭证并存 → [`PairingHello::Reject`]（意图矛盾：配对
///   本就是无凭证设备的入口，带凭证走正常登录——不留歧义形态）；
/// - 其余（pairing 缺席/false、非 hello、畸形 JSON）→ [`PairingHello::NotPairing`]
///   （走既有认证分拣，畸形由 parse_hello 统一拒）。
///
/// 网关侧不消费本判定（配对是浏览器登录页路径；网关 hello 的 pairing 字段
/// 被忽略，走既有 v1.2/v1.3 分流）。
#[derive(Debug, PartialEq, Eq)]
enum PairingHello {
    Pairing { device: Option<String> },
    Reject,
    NotPairing,
}

fn classify_pairing_hello(text: &str) -> PairingHello {
    let Ok(h) = serde_json::from_str::<HelloFrame>(text) else {
        return PairingHello::NotPairing;
    };
    // 信封版本与帧类型先拒（v!=1 / 非 hello → NotPairing，由 parse_hello
    // 统一按 BadVersion/NotHello 拒——失败形态与正常认证一致）。
    if h.v != 1 || h.kind != "hello" || h.pairing != Some(true) {
        return PairingHello::NotPairing;
    }
    if h.token.is_some() || h.session.is_some() {
        return PairingHello::Reject;
    }
    let device = non_empty_field(&h.device);
    if device
        .as_deref()
        .is_some_and(|d| d.chars().count() > DEVICE_NAME_MAX_CHARS)
    {
        return PairingHello::Reject;
    }
    PairingHello::Pairing { device }
}

// ---------------------------------------------------------------------------
// 滑动窗口限流（§4.4，纯函数可拨钟）
// ---------------------------------------------------------------------------

/// 每连接滑动窗口限流：任意 [`RATE_WINDOW`] 内 ≤ [`RATE_MAX_FRAMES`] 帧。
/// 时间由调用方注入（单测可拨钟）；被拒的帧不记录——拒绝即断连，且不应
/// 延长窗口记忆。
#[derive(Default)]
struct RateWindow {
    events: VecDeque<Instant>,
}

impl RateWindow {
    /// 记录一帧并判定放行：先滑出窗口外（elapsed ≥ 窗口长）的旧事件再计数。
    fn check_and_record(&mut self, now: Instant) -> bool {
        while let Some(&oldest) = self.events.front() {
            if now.duration_since(oldest) < RATE_WINDOW {
                break;
            }
            self.events.pop_front();
        }
        if self.events.len() >= RATE_MAX_FRAMES {
            return false;
        }
        self.events.push_back(now);
        true
    }
}

// ---------------------------------------------------------------------------
// 网关离线缓冲（§4.3）
// ---------------------------------------------------------------------------

/// 网关离线缓冲：FIFO，[`BUF_MAX_FRAMES`] 帧 / [`BUF_MAX_BYTES`] 字节先到
/// 为准，溢出拒新（丢新保旧——保证重放顺序完整）。字节按 UTF-8 计。
///
/// M5 多机简化：缓冲保持 per-relay 单 FIFO，**不按 machine 分桶**。理由：
/// 帧进缓冲的唯一时机是**全部**网关离线（任一在线都不会缓冲）；缓冲帧
/// 重放给第一个上线的网关。多机并存时 SPA 本就必须显式选 machine 才能发
/// 帧（缺省路由在多机下是 ambiguous error——bootstrap 期的 sync/ping 静默
/// 豁免除外，见 [`AMBIGUOUS_SILENT_KINDS`]），「全下线」窗口内积压的帧语义
/// 上属于「当时选中的机器」，但 SPA 重连后所选机器未必恢复——按 machine
/// 分桶要么帧永久滞留（无人来取），要么协议再加清理规则，复杂度不匹配
/// 收益；单 FIFO「先上线先重放」足够覆盖主场景（单机部署 / 多机短暂全掉
/// 后任一台先恢复）。
#[derive(Default)]
struct OfflineBuffer {
    frames: VecDeque<String>,
    bytes: usize,
}

impl OfflineBuffer {
    /// 入缓冲：帧数已满或加入后超字节上限 → false（不入，调用方向该
    /// browser 回 gateway_offline error）。
    fn push(&mut self, text: &str) -> bool {
        if self.frames.len() >= BUF_MAX_FRAMES {
            return false;
        }
        if self.bytes + text.len() > BUF_MAX_BYTES {
            return false;
        }
        self.frames.push_back(text.to_string());
        self.bytes += text.len();
        true
    }

    /// 取走全部缓冲（网关上线按序重放用），计数清零。
    fn drain(&mut self) -> Vec<String> {
        self.bytes = 0;
        self.frames.drain(..).collect()
    }
}

// ---------------------------------------------------------------------------
// Hub 状态机（§4.3）
// ---------------------------------------------------------------------------

/// 连接角色：决定槽位、认证路径与转发方向。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Gateway,
    Browser,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Gateway => "gateway",
            Role::Browser => "browser",
        }
    }
}

/// relay 自产帧（§4.3 行为 7：除此之外 relay 不解析、不改写、不记录任何
/// 业务帧内容——machine/error/session/devices 之外 relay 对帧是「盲」的）。
/// 网关侧 hello_ack 携带 `machine`（v1.1 身份确认：回显 hello 自报的
/// machine）；浏览器侧见 [`browser_hello_ack`]（v1.2 携带 session）。
fn gateway_hello_ack_frame(role: Role, machine: &str) -> Message {
    Message::text(
        serde_json::json!({"v": 1, "type": "hello_ack", "role": role.name(), "machine": machine})
            .to_string(),
    )
}

/// 浏览器侧 hello_ack（协议 v1.2 §1/§2）：携带会话凭证载荷
/// `session:{id,token,expiresAt}`。登录路径 token = 新换发的会话 token
///（明文只此一次）；重连路径 token = 回显客户端自报的 token（客户端本来
/// 就持有，回显只为帧形状统一）。expiresAt 为剩余有效期的绝对 unix 秒。
fn browser_hello_ack(role: Role, session: &BrowserSession) -> Message {
    Message::text(
        serde_json::json!({
            "v": 1,
            "type": "hello_ack",
            "role": role.name(),
            "session": {
                "id": session.id,
                "token": session.ack_token,
                "expiresAt": session.expires_at,
            }
        })
        .to_string(),
    )
}

/// devices 帧（协议 v1.2 §4）：设备列表快照（`device_list` 的应答与
/// `device_revoke` 的回执）。设备按 (created, id) 有序。
fn devices_frame(devices: &[DeviceView]) -> Message {
    let arr: Vec<serde_json::Value> = devices
        .iter()
        .map(|d| {
            serde_json::json!({
                "id": d.id,
                "name": d.name,
                "connected": d.connected,
                "lastSeen": d.last_seen,
                "created": d.created,
            })
        })
        .collect();
    Message::text(serde_json::json!({"v": 1, "type": "devices", "devices": arr}).to_string())
}

/// 浏览器侧 transport 管理帧（协议 v1.2 §4 + passkey 扩展 + 管理批管理帧
/// 族）：设备列表 / 吊销 + 通行钥匙列表 / 删除 + 机器表增删查 / USER_TOKEN
/// 轮换 / 配对批准拒绝——浏览器与 relay 自产自消，**不进业务路由**（与
/// sync/ping 的歧义豁免同级，在 machine 路由之前分拣）。`device_revoke`
/// 缺 deviceId / `cred_revoke` 缺 credId / 非字符串 → 按 no-op 处理（回当前
/// 列表帧），不让畸形管理帧漏进业务路由。管理帧族（machine_* /
/// user_token_rotate / pairing_*）**全部须 `adminToken` 且有效**（缺失/
/// 无效 → `admin_auth_required`，见 [`admin_frame_reply`]）；载荷字段缺失
/// 按空串进入各自校验路径。
#[derive(Debug)]
enum ManageFrame {
    DeviceList,
    DeviceRevoke(String),
    CredList,
    CredRevoke(String),
    /// `machine_list`：`{v,type,adminToken}` → machines_cfg。
    MachineList {
        admin_token: Option<String>,
    },
    /// `machine_add`：`{v,type,adminToken,name(1..=32 [a-zA-Z0-9_-]),pubkey(64hex)}`。
    MachineAdd {
        admin_token: Option<String>,
        name: String,
        pubkey: String,
    },
    /// `machine_remove`：`{v,type,adminToken,name}`。
    MachineRemove {
        admin_token: Option<String>,
        name: String,
    },
    /// `user_token_rotate`：`{v,type,adminToken}` → user_token_rotated
    ///（新 token 明文仅此一次返回）。
    UserTokenRotate {
        admin_token: Option<String>,
    },
    /// `pairing_approve` / `pairing_deny`：`{v,type,adminToken,code}`（配对
    /// 批准属管理级，须 re-auth）。
    PairingApprove {
        admin_token: Option<String>,
        code: String,
    },
    PairingDeny {
        admin_token: Option<String>,
        code: String,
    },
}

fn parse_manage_frame(text: &str) -> Option<ManageFrame> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    // 管理帧族公共字段提取（缺失按 None / 空串，校验在处理侧统一做）。
    let admin = || {
        v.get("adminToken")
            .and_then(|t| t.as_str())
            .map(str::to_string)
    };
    let field = |key: &str| {
        v.get(key)
            .and_then(|f| f.as_str())
            .unwrap_or("")
            .to_string()
    };
    match v.get("type")?.as_str()? {
        "device_list" => Some(ManageFrame::DeviceList),
        "device_revoke" => Some(ManageFrame::DeviceRevoke(field("deviceId"))),
        "cred_list" => Some(ManageFrame::CredList),
        "cred_revoke" => Some(ManageFrame::CredRevoke(field("credId"))),
        "machine_list" => Some(ManageFrame::MachineList {
            admin_token: admin(),
        }),
        "machine_add" => Some(ManageFrame::MachineAdd {
            admin_token: admin(),
            name: field("name"),
            pubkey: field("pubkey"),
        }),
        "machine_remove" => Some(ManageFrame::MachineRemove {
            admin_token: admin(),
            name: field("name"),
        }),
        "user_token_rotate" => Some(ManageFrame::UserTokenRotate {
            admin_token: admin(),
        }),
        "pairing_approve" => Some(ManageFrame::PairingApprove {
            admin_token: admin(),
            code: field("code"),
        }),
        "pairing_deny" => Some(ManageFrame::PairingDeny {
            admin_token: admin(),
            code: field("code"),
        }),
        _ => None,
    }
}

/// 设备名解析（协议 v1.2 §1）：hello `device` 字段优先；缺省记 UA 摘要
///（sha256 前 8 hex，前缀 `ua-`——UA 全文太长且不稳定，摘要够辨识）；
/// 无 UA（curl/测试客户端）→ `unknown-device`。
fn device_name(device: Option<&str>, headers: &HeaderMap) -> String {
    if let Some(d) = device.map(str::trim).filter(|d| !d.is_empty()) {
        return d.to_string();
    }
    match headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
    {
        Some(ua) => format!("ua-{}", &sessions::hash_token(ua)[..8]),
        None => "unknown-device".to_string(),
    }
}

/// 旧 `machine` 帧（v1 唯一通知形态，v1.1 保留）：语义收敛为「default
/// 机器的在线状态」——单机部署（default 机器）与旧 SPA 的行为完全不变；
/// 非 default 机器上下线不发本帧（新 SPA 看 [`machines_frame`]）。
fn machine_frame(online: bool) -> Message {
    Message::text(serde_json::json!({"v": 1, "type": "machine", "online": online}).to_string())
}

/// v1.1 `machines` 帧：全量在线机器列表（排序不敏感，此处给有序输出便于
/// 测试与日志），任一网关上/下线时向全部在线浏览器广播。
fn machines_frame(machines: &[String]) -> Message {
    Message::text(serde_json::json!({"v": 1, "type": "machines", "online": machines}).to_string())
}

fn error_frame(code: &str) -> Message {
    Message::text(serde_json::json!({"v": 1, "type": "error", "code": code}).to_string())
}

/// error 帧带 message 变体（管理帧校验失败的可读指引，任务定案
/// `machine_invalid` 形态）：`{"v":1,"type":"error","code":"…","message":"…"}`。
fn error_frame_msg(code: &str, message: &str) -> Message {
    Message::text(
        serde_json::json!({"v": 1, "type": "error", "code": code, "message": message}).to_string(),
    )
}

/// `user_token_rotated` 帧（user_token_rotate 管理帧应答）：
/// `{"v":1,"type":"user_token_rotated","token":"<64hex>"}`——**新 token
/// 明文仅此一次返回**（落盘的是哈希），SPA 提示用户立即保存。
fn user_token_rotated_frame(token: &str) -> Message {
    Message::text(
        serde_json::json!({"v": 1, "type": "user_token_rotated", "token": token}).to_string(),
    )
}

/// challenge 帧（协议 v1.3 key 握手第二帧）：`{"v":1,"type":"challenge",
/// "nonce":"<32B hex>"}`——nonce 一次性（[`ChallengeTable::consume`] 单次
/// 消费）且 60s 过期，录制重放无效。
fn challenge_frame(nonce: &str) -> Message {
    Message::text(serde_json::json!({"v": 1, "type": "challenge", "nonce": nonce}).to_string())
}

/// prove 帧反序列化形状（协议 v1.3 key 握手第三帧）：
/// `{"v":1,"type":"prove","pubkey":"<64 hex>","signature":"<128 hex>"}`。
/// 签名对象 = challenge nonce 的 **hex 字符串原文的 UTF-8 字节**（网关侧
/// 对收到的 nonce 原样签名——两侧无 hex 解码歧义；验签见
/// [`GatewayKeys::verify`]）。畸形/超长由 256KB message 上限与 hex 解码
/// 兜底，任何失败统一 "auth failed"。
#[derive(Deserialize)]
struct ProveFrame {
    v: i64,
    #[serde(rename = "type")]
    kind: String,
    pubkey: String,
    signature: String,
}

/// 出站投递项：业务帧 / 主动 close（code+reason）。
/// close 必须走 channel 而非直接操作 socket：socket 由连接任务独占，而
/// 踢旧（4001）与服务停机（1001）都发生在其它任务的上下文里。
#[derive(Debug, Clone)]
enum Outbound {
    Msg(Message),
    Close(u16, &'static str),
}

/// Hub 槽位：指向某条连接的出站 channel + 连接身份。conn_id 用于网关
/// machine 槽位防「踢旧后旧连接退出时误清新连接」；browser 并存表以
/// conn_id 为键，字段保持单一 Endpoint 形状。
struct Endpoint {
    conn_id: u64,
    tx: mpsc::Sender<Outbound>,
}

/// browser 槽位（v1.4 enc 帧定向路由）：Endpoint + 该连接绑定的**浏览器
/// 会话 id**（认证产物，每条浏览器连接必有一个）。sid 是 transport 路由键
/// （与 machine 同级——见 [`frame_sid`]）：网关出站 `enc{sid}` 帧据此只
/// 投给该会话的连接（逐浏览器独立 E2E 密钥的前提），其余帧广播不受影响。
struct BrowserSlot {
    session_id: String,
    ep: Endpoint,
}

struct HubState {
    /// 网关多机器并存表（v1.1）：machine id → 连接槽位（上限
    /// [`MAX_GATEWAYS`]；同 machine 重连顶替旧连接——每台机器单连接）。
    gateways: HashMap<String, Endpoint>,
    /// 浏览器多连接并存表：conn_id → 连接槽位（上限 [`MAX_BROWSERS`]）。
    browsers: HashMap<u64, BrowserSlot>,
    buf: OfflineBuffer,
}

/// 进程级单例 Hub（Arc 共享，每连接 Clone）。锁纪律（任务说明 §4.3）：
/// std Mutex 保护纯内存临界区，所有方法「锁内计算 → clone sender 出锁 →
/// 锁外投递」，绝不跨 await 持锁。
#[derive(Clone)]
struct Hub {
    state: Arc<Mutex<HubState>>,
    conn_ids: Arc<AtomicU64>,
    shutdown: watch::Receiver<bool>,
}

/// 锁内取全部在线 browser 的投递口（广播目标：machines/machine 通知 /
/// 业务帧 fan-out / 停机 close）。
fn browser_senders(st: &HubState) -> Vec<mpsc::Sender<Outbound>> {
    st.browsers.values().map(|b| b.ep.tx.clone()).collect()
}

/// 锁内取全量在线 machine 列表（有序输出——machines 帧对 SPA 排序不敏感，
/// 有序只为测试与日志稳定）。
fn online_machines(st: &HubState) -> Vec<String> {
    let mut v: Vec<String> = st.gateways.keys().cloned().collect();
    v.sort();
    v
}

/// 网关注册结果：锁外要做的副作用（踢旧 / 广播上线通知 / 重放缓冲）。
struct GatewayRegistered {
    /// 同 machine 旧连接被顶替（每台机器单连接语义）→ 调用方对其 close 4001。
    superseded: Option<mpsc::Sender<Outbound>>,
    /// 全部在线 browser 的投递口（machines/machine 上线广播目标）。
    browsers: Vec<mpsc::Sender<Outbound>>,
    /// 注册后全量在线 machine 列表（machines 帧载荷）。
    machines: Vec<String>,
    replay: Vec<String>,
    /// 注册后在线网关数（日志）。
    gateways: usize,
    /// 本次注册的是否 default 机器（决定是否附带旧 machine 上线帧）。
    is_default: bool,
}

/// browser 注册结果（多连接并存版）：不再踢旧。`full` = 并存数已达
/// [`MAX_BROWSERS`]，本连接未注册（调用方对**自己** close 4001——复用
/// superseded 让位语义，SPA 已有终态处理）；`machine_online` 为注册时
/// **default 机器**在线快照（旧 machine 帧语义，v1.1）；`machines` 为
/// 注册时全量在线机器列表（v1.1 machines 快照帧载荷）；`browsers` 为
/// 注册后在线 browser 数（含本连接），供连接日志。
struct BrowserRegistered {
    full: bool,
    machine_online: bool,
    machines: Vec<String>,
    browsers: usize,
}

/// browser 业务帧的路向（§4.3 行为 4 + v1.1 §3 路由规则）。
#[derive(Debug)]
enum BrowserRoute {
    /// 命中网关（显式指定该机器在线 / 未指定且恰好一台在线）→ 透传
    ///（携带网关出站 channel）。
    Deliver(mpsc::Sender<Outbound>),
    /// 全部网关离线 → 已入缓冲（单 FIFO，见 [`OfflineBuffer`] 头注释）。
    Buffered,
    /// 全部网关离线且缓冲满 → 不入，调用方向该 browser 回 gateway_offline error。
    OfflineFull,
    /// 显式指定 machine 但该机器不在线 → 调用方回 machine_offline error。
    MachineOffline,
    /// 未指定 machine 且多台网关在线，但帧类型为 bootstrap 豁免类型
    ///（sync/ping，见 [`AMBIGUOUS_SILENT_KINDS`]）→ 静默丢弃：不路由、
    /// 不回错（SPA 收到 machines 帧后会补发带 machine 的 sync；ping 无
    /// 副作用）。其余帧类型仍 [`BrowserRoute::Ambiguous`]——真实用户操作
    /// 报错是对的。
    Ignored,
    /// 未指定且多台网关在线（缺省路由歧义）→ 调用方回 ambiguous_machine error。
    Ambiguous,
}

/// browser→gateway 业务帧的可选 `machine` 路由键提取（v1.1 §3）。
///
/// relay 对业务帧保持「盲」（零解析零改写零记录），唯一例外是这一个
/// 字段：`machine` 是 **transport 层路由键**（决定帧投给哪条网关连接），
/// 不是业务语义——不读它就无法多机路由；除它之外的任何字段仍然不碰。
/// JSON 畸形 / 字段缺失 / 非字符串 / trim 后为空 → None（按「未指定」
/// 路由；畸形帧本身照原样透传或入缓冲，盲转语义不变）。
fn frame_machine(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let m = v.get("machine")?.as_str()?.trim();
    if m.is_empty() {
        return None;
    }
    Some(m.to_string())
}

/// gateway→browser 帧的可选 `sid` 路由键提取（协议 v1.4，与 [`frame_machine`]
/// 同级同纪律）：`sid` 是 **transport 层路由键**——enc 帧逐浏览器独立加密
///（网关对每个已 E2E 握手的浏览器会话各密封一份），relay 据此把帧只投给
/// 该会话的连接；**box 内容 relay 不理解也不触碰**（盲转语义不变，本函数
/// 只读信封元数据）。JSON 畸形 / 字段缺失 / 非字符串 / trim 后为空 → None
///（按「未指定」广播，现状语义）。`type=="enc"` 不参与浏览器→网关方向的
/// sync/ping 歧义豁免（[`AMBIGUOUS_SILENT_KINDS`]）——enc 帧照常按 machine
/// 路由，豁免表不含它。
fn frame_sid(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let s = v.get("sid")?.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    Some(s.to_string())
}

/// 缺省路由歧义（未指定 machine 且多台网关在线）时**静默忽略**的帧类型：
/// `sync` 与 `ping`。理由（bootstrap 时序豁免）：
/// - SPA 首连在 hello_ack 后立即发 bootstrap sync，此时 machines 列表帧
///   尚未到达、无从带 machine（鸡生蛋）；sync 幂等可补发——SPA 收到
///   machines 帧后会再发一次带 machine 的 sync，此前的无 machine sync
///   丢弃无害；
/// - ping 是无副作用探活，丢弃同样无害。
///
/// 这两类帧若照常回 `ambiguous_machine`，只会让 SPA 每次连接都弹一次
///「多台机器在线」toast（4s 噪音）。其余类型（message / approval_response
/// / control）是真实用户操作，歧义仍回错——见 [`BrowserRoute::Ambiguous`]。
/// **`enc` 不在豁免表**（协议 v1.4）：enc 帧是真实密文业务帧，缺省路由
/// 歧义回错是对的（SPA 补 machine 重发）；其 `type` 是信封明文元数据，
/// [`frame_kind`] 读取不破坏中继盲化。
const AMBIGUOUS_SILENT_KINDS: [&str; 2] = ["sync", "ping"];

/// browser→gateway 帧的 `type` 提取（仅服务于 [`AMBIGUOUS_SILENT_KINDS`]
/// 判定）。与 [`frame_machine`] 同款纪律：relay 对业务帧保持「盲」，`type`
/// 只用于歧义分支的静默豁免判定，不消费业务语义。JSON 畸形 / 字段缺失 /
/// 非字符串 → None（按未知类型处理，走既有 Ambiguous 回错路径）。
fn frame_kind(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(v.get("type")?.as_str()?.to_string())
}

/// 缺省路由歧义分支的静默豁免判定：帧 type 属于 [`AMBIGUOUS_SILENT_KINDS`]
///（解析失败 / 未知类型 → false，不豁免）。
fn is_ambiguous_silent_kind(text: &str) -> bool {
    match frame_kind(text).as_deref() {
        Some(k) => AMBIGUOUS_SILENT_KINDS.contains(&k),
        None => false,
    }
}

impl Hub {
    fn new(shutdown: watch::Receiver<bool>) -> Self {
        Self {
            state: Arc::new(Mutex::new(HubState {
                gateways: HashMap::new(),
                browsers: HashMap::new(),
                buf: OfflineBuffer::default(),
            })),
            conn_ids: Arc::new(AtomicU64::new(1)),
            shutdown,
        }
    }

    /// 中毒（临界区 panic）时取回数据继续服务：纯内存临界区无 IO 副作用
    /// 可回滚，与 `src/webhook.rs` 的限速桶/去重表同款取舍。
    fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next_conn_id(&self) -> u64 {
        self.conn_ids.fetch_add(1, Ordering::Relaxed)
    }

    fn shutdown_subscribe(&self) -> watch::Receiver<bool> {
        self.shutdown.clone()
    }

    /// 网关注册（v1.1 多机器并存）：满员（[`MAX_GATEWAYS`]）且非同机重连
    /// → None（调用方 close 4001 自己）；否则占该 machine 的槽位——同
    /// machine 旧连接被顶替（踢旧）、取全部在线 browser 的投递口（上线
    /// 广播目标）、drain 离线缓冲（重放顺序 = 入队顺序，见
    /// [`OfflineBuffer`] 头注释的单 FIFO 简化）。
    fn register_gateway(
        &self,
        machine: &str,
        conn_id: u64,
        tx: mpsc::Sender<Outbound>,
    ) -> Option<GatewayRegistered> {
        let mut st = self.lock();
        if st.gateways.len() >= MAX_GATEWAYS && !st.gateways.contains_key(machine) {
            return None;
        }
        let superseded = st
            .gateways
            .insert(machine.to_string(), Endpoint { conn_id, tx })
            .map(|e| e.tx);
        let reg = GatewayRegistered {
            superseded,
            browsers: browser_senders(&st),
            machines: online_machines(&st),
            replay: st.buf.drain(),
            gateways: st.gateways.len(),
            is_default: machine == DEFAULT_MACHINE,
        };
        Some(reg)
    }

    /// browser 注册（多连接并存）：上限未满 → 注册新 conn_id（不踢任何旧
    /// 连接）+ default 机器快照 + 全量 machines 快照；已满 → full=true
    ///（本连接不注册，调用方 close 4001 自己）。`sid` 为该连接绑定的浏览
    /// 器会话 id（v1.4 enc 帧定向投递键，见 [`BrowserSlot`]）。
    fn register_browser(
        &self,
        conn_id: u64,
        sid: &str,
        tx: mpsc::Sender<Outbound>,
    ) -> BrowserRegistered {
        let mut st = self.lock();
        if st.browsers.len() >= MAX_BROWSERS {
            return BrowserRegistered {
                full: true,
                machine_online: st.gateways.contains_key(DEFAULT_MACHINE),
                machines: online_machines(&st),
                browsers: st.browsers.len(),
            };
        }
        st.browsers.insert(
            conn_id,
            BrowserSlot {
                session_id: sid.to_string(),
                ep: Endpoint { conn_id, tx },
            },
        );
        BrowserRegistered {
            full: false,
            machine_online: st.gateways.contains_key(DEFAULT_MACHINE),
            machines: online_machines(&st),
            browsers: st.browsers.len(),
        }
    }

    /// browser→gateway 帧路向（§4.3 行为 4 + v1.1 §3 路由规则）：
    /// - 显式 machine 在线 → 透传；不在线 → [`BrowserRoute::MachineOffline`]；
    /// - 未指定：恰好一台 → 透传（单机部署 SPA 零升级无感）；多台 → sync/
    ///   ping 静默丢弃（bootstrap 时序豁免，[`BrowserRoute::Ignored`]），
    ///   其余 [`BrowserRoute::Ambiguous`]；零台 → 沿用 M1 行为入单 FIFO
    ///   缓冲（sync/ping 亦照旧入缓冲——豁免只作用于歧义分支），缓冲满
    ///   拒新（丢新保旧）。
    fn route_browser_frame(&self, text: &str) -> BrowserRoute {
        let mut st = self.lock();
        match frame_machine(text) {
            Some(m) => match st.gateways.get(&m) {
                Some(e) => BrowserRoute::Deliver(e.tx.clone()),
                None => BrowserRoute::MachineOffline,
            },
            None => match st.gateways.len() {
                1 => {
                    let e = st.gateways.values().next().expect("len==1 必有槽位");
                    BrowserRoute::Deliver(e.tx.clone())
                }
                0 => {
                    if st.buf.push(text) {
                        BrowserRoute::Buffered
                    } else {
                        BrowserRoute::OfflineFull
                    }
                }
                _ if is_ambiguous_silent_kind(text) => BrowserRoute::Ignored,
                _ => BrowserRoute::Ambiguous,
            },
        }
    }

    /// gateway→browser 帧路向（§4.3 行为 5，多 browser 广播版 + v1.4 sid
    /// 定向）：帧带非空 `sid` 且匹配在线浏览器会话 → **只投该会话的连接**
    ///（逐浏览器独立 E2E 密钥——每会话的 enc box 只有本会话能解，广播给
    /// 他人也只是打不开的随机字节；同会话多连接（多标签）全部投递——
    /// 只有最近一次握手的连接能解密，其余连接 AEAD 失败由 SPA 自行处理）；
    /// sid 空 / 无匹配 → 广播全部在线 browser（现状语义——e2e_ack 等握手
    /// 前帧、明文兼容期帧、SPA 未上线窗口的 enc 帧都靠广播兜底）。返回
    /// 空 = 无在线 browser，帧直接丢弃不缓存——会话恢复走网关侧重发机制，
    /// 不归 relay 管（docs/WEB_DESIGN.md D6）。网关侧出站帧自带 machine
    /// 字段（v1.1），relay 透传不改。
    fn route_gateway_frame(&self, text: &str) -> Vec<mpsc::Sender<Outbound>> {
        let st = self.lock();
        if let Some(sid) = frame_sid(text) {
            let targeted: Vec<mpsc::Sender<Outbound>> = st
                .browsers
                .values()
                .filter(|b| b.session_id == sid)
                .map(|b| b.ep.tx.clone())
                .collect();
            if !targeted.is_empty() {
                return targeted;
            }
        }
        browser_senders(&st)
    }

    /// browser 退出只清自己的槽位（按键精确移除，天然不误伤其它并存连接），
    /// 返回剩余在线 browser 数（供断开日志）。
    fn release_browser(&self, conn_id: u64) -> usize {
        let mut st = self.lock();
        st.browsers.remove(&conn_id);
        st.browsers.len()
    }

    /// 网关退出清 machine 槽位（§4.3 行为 2，v1.1 多机版）：仅当该 machine
    /// 槽位仍指向本连接（conn_id 相同）才清——踢旧后旧连接退出不得误清新
    /// 连接；清槽位后锁外向**全部**在线 browser 广播 default 机器下线（旧
    /// machine 帧，仅 default 机器）+ 全量 machines 列表（v1.1）。
    fn release_gateway(&self, machine: &str, conn_id: u64) -> (usize, usize) {
        // 锁内：判定槽位归属（非本连接 → 提前 return，不广播）、清槽位、
        // 快照广播目标与计数。
        let (senders, machines, is_default, browsers, gateways) = {
            let mut st = self.lock();
            let is_current = st
                .gateways
                .get(machine)
                .is_some_and(|e| e.conn_id == conn_id);
            if !is_current {
                return (st.browsers.len(), st.gateways.len());
            }
            st.gateways.remove(machine);
            (
                browser_senders(&st),
                online_machines(&st),
                machine == DEFAULT_MACHINE,
                st.browsers.len(),
                st.gateways.len(),
            )
        };
        // 旧 machine 帧在前（v1 旧 SPA 看到的帧序不变），machines 帧在后。
        if is_default {
            let offline = Outbound::Msg(machine_frame(false));
            for b in &senders {
                try_send(b, offline.clone());
            }
        }
        let list = Outbound::Msg(machines_frame(&machines));
        for b in &senders {
            try_send(b, list.clone());
        }
        (browsers, gateways)
    }

    /// 服务停机：向全部网关与全部在线 browser 发 Close（§4.5 graceful
    /// shutdown，1001）。
    fn close_all(&self, code: u16, reason: &'static str) {
        let senders = {
            let st = self.lock();
            st.gateways
                .values()
                .map(|e| e.tx.clone())
                .chain(browser_senders(&st))
                .collect::<Vec<_>>()
        };
        for tx in senders {
            try_send(&tx, Outbound::Close(code, reason));
        }
    }

    /// 全部**已认证**在线 browser 的投递口（管理批：`pairing_request`
    /// 广播目标——未认证连接不在 browsers 表，天然收不到）。
    pub(crate) fn browser_broadcast(&self) -> Vec<mpsc::Sender<Outbound>> {
        let st = self.lock();
        browser_senders(&st)
    }

    /// 指定 machine 网关连接的投递口（machine_remove 断开该机器当前连接
    /// 用）。**不摘槽位**——Close 经 channel 送达后由该连接自己的退出路径
    /// release_gateway 清槽 + 广播 machines 更新，语义与自然断开一致。
    pub(crate) fn gateway_sender(&self, machine: &str) -> Option<mpsc::Sender<Outbound>> {
        let st = self.lock();
        st.gateways.get(machine).map(|e| e.tx.clone())
    }
}

/// best-effort 投递：channel 打满 / 接收端已撤 = 对端消费停滞，丢弃并记
/// 日志。业务层有心跳与重发兜底（§4.3 行为 5），中继不做交付保证；
/// try_send 让连接之间不存在互相 await 的投递环，杜绝死锁。
fn try_send(tx: &mpsc::Sender<Outbound>, item: Outbound) -> bool {
    match tx.try_send(item) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Closed(_)) => false,
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!(target: TARGET, "出站 channel 打满，帧被丢弃（对端消费停滞）");
            false
        }
    }
}

/// close 帧构造（axum 的 CloseCode 即 u16 类型别名）。
fn close_msg(code: u16, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }))
}

/// 连接日志的 session 标识（UUID 前 8 位：日志可对上设备，又不至于把
/// 整个 id 刷进日志噪音）。
fn session_log_id(id: &str) -> String {
    id.chars().take(SESSION_LOG_PREFIX).collect()
}

// ---------------------------------------------------------------------------
// HTTP 路由（§4.1；relay 看到的是 Caddy strip /imagent 后的路径）
// ---------------------------------------------------------------------------

/// 安全响应头 · CSP（SPA 纵深防御）：浏览器把 user token 存在 localStorage，
/// XSS 是偷 token 的主通道——CSP 让「即使被注入 HTML，脚本也不执行」。
/// 逐项理由：
/// - `default-src 'self'`：未显式列出的加载类 directive 一律收紧到同源；
/// - `script-src 'self'`：**核心项**，不放行 inline / eval——已核实
///   `static/index.html` 与 Vite 构建产物无 inline script（唯一
///   `<script type="module" crossorigin src>` 外链，无 `%VITE%` 注入、无
///   inline 事件属性），故无需 nonce 方案；注入的内联脚本一律不执行；
/// - `style-src 'self' 'unsafe-inline'`：React 运行时 `<elem style="...">`
///   属性注入与 Tailwind 产物所需。风险可接受——CSS 注入改的是样式，
///   偷不走 token（数据外带走 connect-src，已被 'self' 封死）；
/// - `img-src 'self' data:`：内联 `data:` URI 图标/占位图所需；
/// - `connect-src 'self' wss: ws:`：fetch/XHR/WebSocket 只准同源。scheme
///   双列是部署形态决定的：公网经 Caddy（TLS 终结）是 `wss://`，本地
///   http 直连调试形态（http://127.0.0.1:3010/）是 `ws://`——都属同源
///   WebSocket，不是放行外站；
/// - `font-src 'self'`：字体同源（产物无外链字体）；
/// - `frame-ancestors 'none'`：任何页面不得嵌入本页（clickjacking）；
/// - `base-uri 'none'`：禁 `<base>` 注入改写相对 URL 的解析根（绕过
///   script-src 的相对路径技俩）；
/// - `form-action 'self'`：表单只准提交同源，堵「注入表单外带凭证」。
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' \
     'unsafe-inline'; img-src 'self' data:; connect-src 'self' wss: ws:; \
     font-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

/// 对全部响应（SPA 静态 / healthz / WS upgrade 101）统一注入安全响应头。
/// 用 `Router::layer`（见 [`spawn_relay`]）盖住所有路由与 fallback。逐项：
/// - CSP：见 [`CSP`]；
/// - `X-Content-Type-Options: nosniff`：禁 MIME 嗅探——配 `content_type`
///   白名单，防 svg/js 被浏览器「猜」成 HTML 执行；
/// - `Referrer-Policy: no-referrer`：Referer 泄漏面清零（token 本就不进
///   URL，纵深防御）；
/// - `X-Frame-Options: DENY`：与 CSP `frame-ancestors` 双保险——旧浏览器
///   不认 frame-ancestors，两者取严即可；
/// - `Cross-Origin-Opener-Policy: same-origin`：切断跨源 `window.opener`
///   句柄（reverse tabnabbing / 钓鱼跳板）。
async fn security_headers(req: Request, next: Next) -> Response {
    /// COOP：http crate 未提供标准常量，静态字符串构造（编译期校验合法）。
    const CROSS_ORIGIN_OPENER_POLICY_NAME: HeaderName =
        HeaderName::from_static("cross-origin-opener-policy");
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        CROSS_ORIGIN_OPENER_POLICY_NAME,
        HeaderValue::from_static("same-origin"),
    );
    res
}

/// axum 共享状态（每连接 Clone；Arc 内部共享同一 Hub / 配置 / 各存储）。
#[derive(Clone)]
struct RelayState {
    hub: Hub,
    config: Arc<RelayConfig>,
    /// 会话表（v1.2）：启动时从 `<state_dir>/sessions.json` 加载。
    sessions: Arc<SessionStore>,
    /// passkey 层（凭证表 + challenge 表 + begin 限流 + admin 令牌表）：
    /// 启动时从 `<state_dir>/credentials.json` 加载。
    webauthn: Arc<webauthn::WebauthnState>,
    /// 网关 key 握手 challenge 表（v1.3）：内存态，nonce 一次性 60s 过期。
    challenges: Arc<Mutex<ChallengeTable>>,
    /// 机器公钥表（管理批）：`<state_dir>/machines.json` 落盘 + 热更新，
    /// env GATEWAY_KEYS 仅首次种子（运行时 prove 验签权威）。
    machines: Arc<machines::MachineStore>,
    /// USER_TOKEN 哈希存储（管理批）：`<state_dir>/user_token_hash` 落盘，
    /// env 仅首次种子；user_token_rotate 热轮换。
    user_token: Arc<UserTokenStore>,
    /// 新设备配对 pending 表（管理批）：内存态。
    pairing: Arc<pairing::PairingStore>,
}

/// SPA 静态资源（M1 占位 index.html；M3 构建产物落入 `static/` 后零改动挂载）。
#[derive(rust_embed::RustEmbed)]
#[folder = "static/"]
struct Spa;

/// GET /gateway：网关侧 WS 端点（Upgrade）。
async fn gateway_ws(
    State(st): State<RelayState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    ws_endpoint(st, Role::Gateway, headers, ws).await
}

/// GET /ws：浏览器侧 WS 端点（Upgrade）。
async fn browser_ws(
    State(st): State<RelayState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    ws_endpoint(st, Role::Browser, headers, ws).await
}

/// 两端点公共入口：套上 256KB message/frame 上限（§4.4）后升级。
async fn ws_endpoint(
    st: RelayState,
    role: Role,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    ws.max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_connection(socket, st, role, headers))
}

/// GET /healthz：健康检查，200 空体。
async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// 静态 SPA fallback（§4.1）：`/` 与所有未匹配路径。缓存头规则：`/assets/`
/// 前缀视为带 hash 资源 → 长缓存 immutable；其余（index.html 与 SPA 客户端
/// 路由回落）一律 no-store——无 hash 文件名的内容随时会变，不可缓存。
async fn spa_fallback(uri: Uri) -> Response {
    const NO_STORE: &str = "no-store";
    const IMMUTABLE: &str = "public, max-age=31536000, immutable";
    let raw = uri.path().trim_start_matches('/');
    // 兼容不经 Caddy 的直连形态（本地开发/验收直开 http://127.0.0.1:3010/）：
    // SPA 构建产物（Vite base=/imagent/）的资源引用自带 `/imagent/` 前缀，
    // 公网由 Caddy `handle_path /imagent/*` strip 后转给 relay，但直连时
    // 浏览器请求的是 /imagent/assets/...——这里剥一次前缀再查内嵌产物，
    // 带前缀与不带前缀两种 URL 都可用；缓存头按剥前缀后的路径判定
    //（/imagent/assets/... 同样享 immutable），剥完仍未命中的照旧回落
    // index.html（含 SPA 客户端路由）。
    let path = raw.strip_prefix("imagent/").unwrap_or(raw);
    let (file, cache, ctype_of) = if path.is_empty() {
        (Spa::get("index.html"), NO_STORE, "index.html")
    } else if path.starts_with("assets/") {
        (Spa::get(path), IMMUTABLE, path)
    } else {
        match Spa::get(path) {
            Some(f) => (Some(f), NO_STORE, path),
            // 未命中的路径回落 index.html：SPA 客户端路由（/session/xxx 等）
            // 需要拿到同一个 app 入口。
            None => (Spa::get("index.html"), NO_STORE, "index.html"),
        }
    };
    match file {
        Some(f) => (
            [
                (header::CACHE_CONTROL, cache),
                (header::CONTENT_TYPE, content_type(ctype_of)),
            ],
            f.data.into_owned(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "static assets missing\n").into_response(),
    }
}

/// 静态资源 Content-Type（M1 只有 html；扩展映射为 M3 真 SPA 备好）。
fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "json" => "application/json",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------------------
// WS 连接生命周期
// ---------------------------------------------------------------------------

/// `Authorization: Bearer <token>` 提取（§4.2 形态 2，网关 Rust 客户端的
/// 便利形态；浏览器唯一可用形态是首帧 hello，但本端点不歧视来源）。
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// 认证失败路径（§4.2 + v1.2 reason 区分）：sleep 1s（防爆破节奏）后
/// close 1008。reason 区分：`"auth failed"`（凭证不匹配/畸形）/
/// `"session expired"` / `"session revoked"` / `"too many sessions"`（会话
/// 表满）。日志只记角色与原因，绝不记 token。
async fn auth_reject(socket: &mut WebSocket, role: Role, reason: &'static str) {
    tracing::warn!(target: TARGET, role = role.name(), reason, "认证失败，延迟关闭");
    tokio::time::sleep(AUTH_FAIL_DELAY).await;
    let _ = socket.send(close_msg(CLOSE_POLICY, reason)).await;
}

/// 浏览器连接的会话绑定（v1.2）：认证产物，注册 Hub 后 `bind` 进会话表
///（revoke 断连 / connected 标记 / 日志标识都靠它）。`ack_token` 是回给
/// 客户端的 hello_ack session 载荷：登录 = 新 token 明文（只此一次），
/// 重连 = 回显。
#[derive(Clone)]
struct BrowserSession {
    id: String,
    ack_token: String,
    expires_at: u64,
}

/// hello 认证产物（两角色共用一个分拣入口）。
enum HelloOutcome {
    /// 网关：machine 身份（token-machine 绑定校验已通过）。
    Gateway(String),
    /// 浏览器：会话（登录换发或重连命中）。
    Browser(BrowserSession),
}

/// 换发新会话（USER_TOKEN / passkey 登录验证通过后调用）。会话表满时三级
/// 腾位（过期 → 墓碑 → LRU 最旧离线，见 [`SessionStore::issue`]）；全部
/// 32 个会话都在线才 Err（调用方 close 1008 "too many sessions"）。
fn issue_session(
    st: &RelayState,
    headers: &HeaderMap,
    device: Option<&str>,
) -> Result<BrowserSession, ()> {
    let name = device_name(device, headers);
    let issued = st
        .sessions
        .issue(&name, sessions::unix_now(), st.config.session_ttl_secs)?;
    Ok(BrowserSession {
        id: issued.id,
        ack_token: issued.token,
        expires_at: issued.expires_at,
    })
}

/// hello 首帧认证分拣（纯内存操作，锁内微临界区）——浏览器侧（v1.2）：
/// Session 凭证过 [`SessionStore::verify`]（reason 区分 expired/revoked/
/// auth failed）；Bootstrap 凭证比对 USER_TOKEN → 换发；**Challenge（无
/// 凭证）一律拒**（浏览器无 key 握手概念）。网关侧见 [`gateway_auth`]
///（v1.3 起含挑战应答分支，需 socket IO 故独立成 async 函数）。
fn browser_auth(
    st: &RelayState,
    text: &str,
    headers: &HeaderMap,
) -> Result<HelloOutcome, &'static str> {
    let parsed = parse_hello(text).map_err(|_| "auth failed")?;
    match parsed.credential {
        HelloCredential::Challenge => Err("auth failed"),
        HelloCredential::Session(tok) => {
            match st.sessions.verify(&tok, sessions::unix_now()) {
                SessionLookup::Valid { id, expires_at } => {
                    Ok(HelloOutcome::Browser(BrowserSession {
                        id,
                        // 重连：回显客户端自报的 token。
                        ack_token: tok,
                        expires_at,
                    }))
                }
                SessionLookup::Expired => Err("session expired"),
                SessionLookup::Revoked => Err("session revoked"),
                SessionLookup::NotFound => Err("auth failed"),
            }
        }
        HelloCredential::Bootstrap(tok) => {
            // USER_TOKEN 验证（管理批迁移）：对提交值 sha256 后与
            // user_token_hash 存值恒定时间比较（存储权威已落盘，可轮换）。
            if !st.user_token.verify(&tok) {
                return Err("auth failed");
            }
            issue_session(st, headers, parsed.device.as_deref())
                .map(HelloOutcome::Browser)
                .map_err(|_| "too many sessions")
        }
    }
}

/// 网关侧 hello 认证分流（v1.3）：
/// - **Bootstrap（hello 带 token，旧形态）**：一步 token-machine 绑定校验
///   （[`GatewayTokens::verify`]）——迁移期兼容路径，行为与 v1.2 完全一致；
/// - **Challenge（hello 无凭证）**：ed25519 挑战应答两步——签发一次性
///   nonce（绑定 hello 自报 machine）下发 challenge 帧 → 等 prove（10s）
///   → [`GatewayKeys::verify`]（machine 在 KEYS 表且绑定公钥恒定时间一致
///   + 验签 nonce）；
/// - **Session**：网关无会话概念，一律拒。
///
/// 防探测纪律：所有失败统一 "auth failed"（不区分 machine 是否在表 / 公钥
/// 不匹配 / 验签失败，由 [`auth_reject`] 加 1s 延迟）；prove 等待超时按
/// "auth timeout"。challenge 签发对**任何** shape 合法的 machine 一视同仁
///（未知 machine 也拿到 nonce——不构成存在性 oracle，prove 必然验不过）。
async fn gateway_auth(
    socket: &mut WebSocket,
    st: &RelayState,
    text: &str,
) -> Result<HelloOutcome, &'static str> {
    let parsed = parse_hello(text).map_err(|_| "auth failed")?;
    match parsed.credential {
        HelloCredential::Session(_) => Err("auth failed"),
        HelloCredential::Bootstrap(tok) => {
            if st.config.gateway_tokens.verify(&parsed.machine, &tok) {
                Ok(HelloOutcome::Gateway(parsed.machine))
            } else {
                Err("auth failed")
            }
        }
        HelloCredential::Challenge => {
            // 机器表（管理批：machines.json 落盘权威 + 热更新）全空 → key
            // 握手无任何成功路径，直接失败（不签发 challenge：省一条
            // challenge 表条目与客户端等待，且不暴露「机器在不在表」的
            // 配置差异——失败形态与验签失败一致）。
            if st.machines.is_empty() {
                return Err("auth failed");
            }
            let nonce = {
                let mut table = st.challenges.lock().unwrap_or_else(|e| e.into_inner());
                table.issue(&parsed.machine, Instant::now())
            };
            if socket.send(challenge_frame(&nonce)).await.is_err() {
                return Err("auth failed"); // 对端握手途中断开
            }
            let prove = match tokio::time::timeout(AUTH_TIMEOUT, socket.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => t,
                Ok(Some(Ok(_))) => return Err("auth failed"), // prove 须为文本帧
                Ok(None) | Ok(Some(Err(_))) => return Err("auth failed"),
                Err(_) => {
                    tracing::warn!(target: TARGET, machine = %parsed.machine, "等 prove 超时（10s），关闭");
                    return Err("auth timeout");
                }
            };
            verify_prove(st, &nonce, prove.as_str()).map(HelloOutcome::Gateway)
        }
    }
}

/// prove 帧验证（v1.3 key 握手收口）：nonce 先**单次消费**（取出即作废——
/// 验签失败也不可重试同一 nonce，防重放），再过 [`GatewayKeys::verify`]
///（machine 在表 + 公钥恒定时间一致 + 验签）。失败统一 "auth failed"。
fn verify_prove(st: &RelayState, nonce: &str, text: &str) -> Result<String, &'static str> {
    let p: ProveFrame = serde_json::from_str(text).map_err(|_| "auth failed")?;
    if p.v != 1 || p.kind != "prove" {
        return Err("auth failed");
    }
    let machine = {
        let mut table = st.challenges.lock().unwrap_or_else(|e| e.into_inner());
        table.consume(nonce, Instant::now())
    }
    .ok_or("auth failed")?;
    if !st.machines.verify(&machine, &p.pubkey, nonce, &p.signature) {
        return Err("auth failed");
    }
    Ok(machine)
}

/// 向自身 channel 顺序投递（连接尚未进入消费循环）。容量推导见
/// [`CHANNEL_CAPACITY`]：hello_ack + machine 快照 + ≤200 重放帧恒不阻塞。
async fn send_all(tx: &mpsc::Sender<Outbound>, items: impl IntoIterator<Item = Outbound>) {
    for item in items {
        if tx.send(item).await.is_err() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// 新设备配对等待流 + 管理帧处理（管理批）
// ---------------------------------------------------------------------------

/// 管理帧 adminToken 门槛（管理批）：缺失 / 无效 / 已过期（5 分钟窗）→
/// false（调用方回 `admin_auth_required` 并丢弃帧——任务定案的统一失败
/// 形态，不区分原因防探测）。
fn admin_gate(st: &RelayState, token: &Option<String>) -> bool {
    token
        .as_deref()
        .is_some_and(|t| st.webauthn.admin_tokens.verify(t, sessions::unix_now()))
}

/// 管理操作审计日志（journalctl）：操作 / 机器名或配对码 / session id 前
/// 8 位；token 绝不记录（§4.5 日志纪律）。
fn admin_audit(session_id: &str, op: &str, detail: &str) {
    tracing::info!(
        target: TARGET,
        session = %session_log_id(session_id),
        op,
        detail,
        "管理操作（审计）"
    );
}

/// 本连接的会话 id（管理审计标注发起者；浏览器连接必有会话，防御性空串）。
fn admin_session_id(session: &Option<BrowserSession>) -> String {
    session.as_ref().map(|s| s.id.clone()).unwrap_or_default()
}

/// 新设备配对等待流（管理批，`hello{pairing:true}` 入口）：创建 pending
///（6 位码，`pairing_ttl_secs` 过期，上限 4）→ 回 `pairing_pending` → 向
/// 全部**已认证**浏览器广播 `pairing_request` → 等待 approve/deny 信号、
/// 过期或对端断开。
///
/// 返回 `Some(会话)` = 批准通过（换发的 session 明文经 approve 信号送达，
/// 调用方走公共注册路径回 hello_ack——形态与登录换发一致）；`None` = 连接
/// 已在本流内收口（pending 上限 / 过期 / 拒绝 / 对端断开），调用方直接
/// 返回。等待期「只允许等待」：入站文本帧一律忽略（含 SPA 保活 ping——
/// 配对页连接还在等批准，不是业务通道），二进制按协议违规 close。
async fn run_pairing(
    socket: &mut WebSocket,
    st: &RelayState,
    headers: &HeaderMap,
    device: Option<&str>,
) -> Option<BrowserSession> {
    let name = device_name(device, headers);
    let (sig_tx, mut sig_rx) = mpsc::channel::<PairingSignal>(1);
    let Some(pending) = st.pairing.create(
        &name,
        sig_tx,
        sessions::unix_now(),
        st.config.pairing_ttl_secs,
    ) else {
        tracing::warn!(
            target: TARGET,
            device = %name,
            pending = st.pairing.len(sessions::unix_now()),
            "配对 pending 达上限（4），拒绝新配对"
        );
        let _ = socket
            .send(close_msg(CLOSE_POLICY, "too many pairings"))
            .await;
        return None;
    };
    // 回 pairing_pending（该连接保持未认证态；发不出去 = 对端已断，清表）。
    if socket
        .send(pairing::pairing_pending_frame(&pending.code))
        .await
        .is_err()
    {
        st.pairing.remove(&pending.code);
        return None;
    }
    // 广播 pairing_request 给全部已认证浏览器会话（逐个 try_send，单个
    // 打满/关闭不影响其余）。
    let request = Outbound::Msg(pairing::pairing_request_frame(&pending));
    let targets = st.hub.browser_broadcast();
    let notified = targets.len();
    for tx in targets {
        try_send(&tx, request.clone());
    }
    tracing::info!(
        target: TARGET,
        code = %pending.code,
        device = %name,
        expires_at = pending.expires_at,
        notified,
        "配对 pending 创建（pairing_request 已广播）"
    );

    loop {
        // 过期判定在每轮循环顶部（sleep 到点后由此收口）。
        let Some(remaining) = pending.expires_at.checked_sub(sessions::unix_now()) else {
            st.pairing.remove(&pending.code);
            tracing::info!(target: TARGET, code = %pending.code, "配对 pending 过期，关闭");
            let _ = socket
                .send(close_msg(CLOSE_POLICY, "pairing expired"))
                .await;
            return None;
        };
        tokio::select! {
            // 批准/拒绝信号（来自批准者连接任务的 claim + try_send）。
            sig = sig_rx.recv() => {
                match sig {
                    Some(PairingSignal::Approved(s)) => {
                        tracing::info!(
                            target: TARGET,
                            code = %pending.code,
                            session = %session_log_id(&s.id),
                            "配对批准：pending 连接换发会话（走公共注册路径）"
                        );
                        return Some(BrowserSession {
                            id: s.id,
                            ack_token: s.token,
                            expires_at: s.expires_at,
                        });
                    }
                    Some(PairingSignal::Denied) | None => {
                        // None = 信号端被丢弃（批准者任务在投递前退出的防御
                        // 路径），与拒绝同收口。
                        st.pairing.remove(&pending.code);
                        let _ = socket.send(close_msg(CLOSE_POLICY, "pairing denied")).await;
                        return None;
                    }
                }
            }
            // pending 设备侧行为：断开 / Close → 清表静默退出；文本帧忽略
            //（只允许等待）；二进制按协议违规。
            msg = socket.next() => {
                match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => {
                        st.pairing.remove(&pending.code);
                        return None;
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) | Some(Ok(Message::Text(_))) => {}
                    Some(Ok(Message::Binary(_))) => {
                        st.pairing.remove(&pending.code);
                        let _ = socket
                            .send(close_msg(CLOSE_POLICY, "binary not supported"))
                            .await;
                        return None;
                    }
                }
            }
            // 到点睡醒 → 循环顶部过期分支收口（+1s 下限防 0 秒忙转）。
            _ = tokio::time::sleep(Duration::from_secs(remaining.clamp(1, 60))) => {}
        }
    }
}

/// `pairing_approve` 处理（须 adminToken）：原子 claim pending → 为 pending
/// 绑定的设备签发会话 → 经信号通道投递（pending 连接在 run_pairing 等待
/// 循环里收到后走公共注册路径回 hello_ack）。会话表满 → 信号 Denied 收口，
/// 批准者收 too_many_sessions error。未知/已消费/已过期码 →
/// `pairing_not_found`。成功对批准者不回帧（任务定案未定义应答——新设备
/// 进入 devices 列表 / pending 连接收 hello_ack 即反馈）。
fn handle_pairing_approve(st: &RelayState, code: &str, session_id: &str) -> Option<Message> {
    let Some((device, sig)) = st.pairing.claim(code, sessions::unix_now()) else {
        admin_audit(
            session_id,
            "pairing_approve",
            &format!("code={code} 未命中（未知/已消费/过期）"),
        );
        return Some(error_frame("pairing_not_found"));
    };
    match st
        .sessions
        .issue(&device, sessions::unix_now(), st.config.session_ttl_secs)
    {
        Ok(issued) => {
            let delivered = sig
                .try_send(PairingSignal::Approved(pairing::PairedSession {
                    id: issued.id,
                    token: issued.token,
                    expires_at: issued.expires_at,
                }))
                .is_ok();
            admin_audit(
                session_id,
                "pairing_approve",
                &format!("code={code} device={device} delivered={delivered}"),
            );
            None
        }
        Err(()) => {
            // 会话表满且全在线：pending 无法进入，按拒绝收口。
            let _ = sig.try_send(PairingSignal::Denied);
            tracing::warn!(
                target: TARGET,
                code,
                device = %device,
                "配对批准时会话表满（too many sessions），按拒绝收口"
            );
            Some(error_frame("too_many_sessions"))
        }
    }
}

/// `pairing_deny` 处理（须 adminToken）：原子 claim pending → 信号 Denied
///（pending 连接 close 1008 "pairing denied"）。
fn handle_pairing_deny(st: &RelayState, code: &str, session_id: &str) -> Option<Message> {
    let Some((device, sig)) = st.pairing.claim(code, sessions::unix_now()) else {
        admin_audit(
            session_id,
            "pairing_deny",
            &format!("code={code} 未命中（未知/已消费/过期）"),
        );
        return Some(error_frame("pairing_not_found"));
    };
    let _ = sig.try_send(PairingSignal::Denied);
    admin_audit(
        session_id,
        "pairing_deny",
        &format!("code={code} device={device}"),
    );
    None
}

/// 单条 WS 连接完整生命周期：认证（hello / Bearer 直通）→ 注册 Hub →
/// 转发主循环（限流 + idle 超时 + 停机响应 + transport 管理帧）→ 退出清
/// 槽位。
async fn handle_connection(mut socket: WebSocket, st: RelayState, role: Role, headers: HeaderMap) {
    // ---- 认证（§4.2 + v1.2 会话层，fail-closed）----
    // 认证产物：网关侧 machine 身份（token-machine 绑定校验通过）；浏览器
    // 侧会话（登录换发或重连命中——每条浏览器连接都绑定一个会话）。
    let mut auth_machine: Option<String> = None;
    let mut browser_session: Option<BrowserSession> = None;
    // Bearer 正确 → 直进转发态；Bearer 带了但不正确 → 失败路径（不回落
    // hello：发起方已表明认证意图，回落只会让探测多一种节奏）。
    match bearer_token(&headers) {
        Some(t) => {
            let ok = match role {
                // Bearer 无 machine 维度 → default 机器：v1.2 绑定语义下只有
                // 绑定 default 的 token 可走 Bearer（其余 1008）。
                Role::Gateway => st.config.gateway_tokens.verify(DEFAULT_MACHINE, t),
                // 浏览器 Bearer = USER_TOKEN 登录形态（curl/脚本便利）→ 换发
                // 新会话（Bearer 无从携带 sessionToken，重连必须走 hello 帧）。
                // 管理批：验证走 user_token_hash 哈希存储（可轮换）。
                Role::Browser => st.user_token.verify(t),
            };
            if !ok {
                auth_reject(&mut socket, role, "auth failed").await;
                return;
            }
            match role {
                Role::Gateway => auth_machine = Some(DEFAULT_MACHINE.to_string()),
                Role::Browser => match issue_session(&st, &headers, None) {
                    Ok(s) => browser_session = Some(s),
                    Err(()) => {
                        auth_reject(&mut socket, role, "too many sessions").await;
                        return;
                    }
                },
            }
        }
        None => {
            // hello 首帧认证：10s 内必须是 `{"v":1,"type":"hello",…}`（网关
            // 带 machine 自报 + token（v1.2 兼容）或无凭证走 key 挑战应答
            //（v1.3）；浏览器带 session 重连 / token 登录）。
            match tokio::time::timeout(AUTH_TIMEOUT, socket.next()).await {
                Err(_) => {
                    tracing::warn!(target: TARGET, role = role.name(), "认证超时（10s 无 hello），关闭");
                    let _ = socket.send(close_msg(CLOSE_POLICY, "auth timeout")).await;
                    return;
                }
                Ok(None) | Ok(Some(Err(_))) => return, // 连接已断 / 协议错误
                Ok(Some(Ok(msg))) => {
                    let outcome = match msg {
                        Message::Text(t) => match role {
                            // 网关侧（v1.3）：token 一步 / key 挑战应答两步
                            //（网关无配对概念，pairing 字段被忽略）。
                            Role::Gateway => gateway_auth(&mut socket, &st, t.as_str()).await,
                            // 浏览器侧：先分拣配对入口（管理批：pairing:true
                            // 无凭证 → 配对等待流，不走认证），其余走 v1.2
                            // 会话/token 认证。
                            Role::Browser => match classify_pairing_hello(t.as_str()) {
                                PairingHello::Pairing { device } => {
                                    match run_pairing(&mut socket, &st, &headers, device.as_deref())
                                        .await
                                    {
                                        Some(s) => Ok(HelloOutcome::Browser(s)),
                                        // 连接已在配对流内收口（上限/过期/
                                        // 拒绝/断开）。
                                        None => return,
                                    }
                                }
                                // pairing:true 与凭证并存：意图矛盾，按认证
                                // 失败拒（不留歧义形态）。
                                PairingHello::Reject => Err("auth failed"),
                                PairingHello::NotPairing => browser_auth(&st, t.as_str(), &headers),
                            },
                        },
                        // 首帧必须是文本 hello。
                        _ => Err("auth failed"),
                    };
                    match outcome {
                        Ok(HelloOutcome::Gateway(machine)) => auth_machine = Some(machine),
                        Ok(HelloOutcome::Browser(s)) => browser_session = Some(s),
                        Err(reason) => {
                            auth_reject(&mut socket, role, reason).await;
                            return;
                        }
                    }
                }
            }
        }
    }

    // ---- 注册与自产帧（§4.3 行为 1/3）----
    let conn_id = st.hub.next_conn_id();
    let (tx, mut rx) = mpsc::channel::<Outbound>(CHANNEL_CAPACITY);
    let mut shutdown = st.hub.shutdown_subscribe();

    match role {
        Role::Gateway => {
            let machine = auth_machine
                .clone()
                .expect("网关认证通过后必有 machine 身份");
            let Some(reg) = st.hub.register_gateway(&machine, conn_id, tx.clone()) else {
                // 满员（MAX_GATEWAYS 且非同机重连）：认证已通过，close 4001
                // 让位——复用 superseded 终态语义（新网关客户端按可重连错误
                // 处理，退避后重试；同 machine 槽位释放后即可再入）。
                tracing::warn!(target: TARGET, machine = %machine, "网关机器数达上限（MAX_GATEWAYS），close 4001");
                let _ = socket
                    .send(close_msg(CLOSE_SUPERSEDED, "too many gateways"))
                    .await;
                return;
            };
            let replayed = reg.replay.len();
            if let Some(old) = reg.superseded {
                tracing::info!(target: TARGET, machine = %machine, "踢旧：同 machine 新连接顶替旧连接（close 4001 superseded）");
                try_send(&old, Outbound::Close(CLOSE_SUPERSEDED, "superseded"));
            }
            // hello_ack 先于重放帧：客户端先确认角色、认证结果与 machine 身份
            //（v1.1 回显自报的 machine），再消费重放。
            let mut init = vec![Outbound::Msg(gateway_hello_ack_frame(role, &machine))];
            init.extend(
                reg.replay
                    .into_iter()
                    .map(|f| Outbound::Msg(Message::text(f))),
            );
            send_all(&tx, init).await;
            // 上线通知广播给全部在线 browser（多端并存，逐个 try_send）：
            // default 机器附带旧 machine 上线帧（v1 兼容），随后全量 machines
            // 列表帧（v1.1）。
            if reg.is_default {
                let online = Outbound::Msg(machine_frame(true));
                for b in &reg.browsers {
                    try_send(b, online.clone());
                }
            }
            let list = Outbound::Msg(machines_frame(&reg.machines));
            for b in reg.browsers {
                try_send(&b, list.clone());
            }
            tracing::info!(target: TARGET, machine = %machine, gateways = reg.gateways, replayed, "gateway 连接（认证通过）");
        }
        Role::Browser => {
            // 会话绑定（v1.2）：失败 = 认证与注册之间会话被吊销（竞态窗口）
            // ——fail-closed 断开，不留「已吊销会话的幽灵连接」。sid 先于
            // 注册取出（注册要把它记进 BrowserSlot 作 v1.4 enc 定向路由键）。
            let session = browser_session.clone().expect("浏览器认证通过后必有会话");
            let reg = st.hub.register_browser(conn_id, &session.id, tx.clone());
            if reg.full {
                // 并存数达上限（认证已通过）：本连接 close 4001 让位——复用
                // superseded 语义，SPA 已有终态处理（不重连），在线连接零影响。
                tracing::warn!(target: TARGET, browsers = reg.browsers, "browser 并存数达上限（MAX_BROWSERS），close 4001");
                let _ = socket
                    .send(close_msg(CLOSE_SUPERSEDED, "too many browsers"))
                    .await;
                return;
            }
            if !st.sessions.bind(&session.id, conn_id, tx.clone()) {
                tracing::warn!(target: TARGET, session = %session_log_id(&session.id), "会话在注册前被吊销，断开");
                st.hub.release_browser(conn_id);
                let _ = socket
                    .send(close_msg(CLOSE_POLICY, "session revoked"))
                    .await;
                return;
            }
            // hello_ack（含 session 载荷）后立即推双快照（§4.3 行为 3）：旧
            // machine 帧（default 机器在线状态，v1 兼容——帧序不变）+ 全量
            // machines 列表（v1.1）。
            send_all(
                &tx,
                vec![
                    Outbound::Msg(browser_hello_ack(role, &session)),
                    Outbound::Msg(machine_frame(reg.machine_online)),
                    Outbound::Msg(machines_frame(&reg.machines)),
                ],
            )
            .await;
            tracing::info!(target: TARGET, session = %session_log_id(&session.id), machine_online = reg.machine_online, machines = ?reg.machines, browsers = reg.browsers, "browser 连接（认证通过）");
        }
    }

    // ---- 转发主循环 ----
    // 限流只计本连接**入站**业务帧（hello_ack 之前不算；relay 自产帧是出站
    // 的，天然不计入任何一端的限流——§4.4）。
    let mut rate = RateWindow::default();
    let mut last_rx = Instant::now();
    loop {
        // idle 剩余时间每轮从 last_rx 重算：出站流量不重置 idle 时钟，
        // 「90s 未收到对端任何 message」的语义才成立。
        let remaining = IDLE_TIMEOUT.saturating_sub(last_rx.elapsed());
        tokio::select! {
            // 服务停机兜底（正常路径是 close_all 经 channel 发来 Close(1001)）。
            changed = shutdown.changed() => {
                // 虚醒（值未翻 true）继续循环；信号到或控制端丢弃（Err）→ 停机。
                if changed.is_ok() && !*shutdown.borrow() {
                    continue;
                }
                let _ = socket.send(close_msg(CLOSE_GOING_AWAY, "going away")).await;
                break;
            }
            out = rx.recv() => match out {
                None => break, // 防御路径：槽位已清且自身 sender 已丢弃
                Some(Outbound::Msg(m)) => {
                    if socket.send(m).await.is_err() {
                        break; // 对端 socket 已死
                    }
                }
                Some(Outbound::Close(code, reason)) => {
                    let _ = socket.send(close_msg(code, reason)).await;
                    break;
                }
            },
            item = tokio::time::timeout(remaining, socket.next()) => match item {
                Err(_elapsed) => {
                    tracing::info!(target: TARGET, role = role.name(), "idle 90s 超时，close 1000");
                    let _ = socket.send(close_msg(CLOSE_NORMAL, "idle timeout")).await;
                    break;
                }
                Ok(None) | Ok(Some(Err(_))) => break, // 对端关闭 / 协议错误
                Ok(Some(Ok(msg))) => {
                    last_rx = Instant::now(); // 任何 message（含 ping/pong）都重置 idle
                    match msg {
                        // ping/pong 是端到端语义（§4.4）：relay 不做应用层应答；
                        // WS 协议层 Ping 由 axum 自动回 Pong。
                        Message::Ping(_) | Message::Pong(_) => {}
                        Message::Close(_) => break,
                        Message::Binary(_) => {
                            // MVP 无二进制载荷（§4.3 行为 8）。
                            let _ = socket.send(close_msg(CLOSE_POLICY, "binary not supported")).await;
                            break;
                        }
                        Message::Text(text) => {
                            // 限流（§4.4）：窗口内第 21 帧 → error + close 1008。
                            if !rate.check_and_record(Instant::now()) {
                                tracing::warn!(target: TARGET, role = role.name(), "限流触发（10s 内超过 20 帧），close 1008");
                                let _ = socket.send(error_frame("rate_limited")).await;
                                let _ = socket.send(close_msg(CLOSE_POLICY, "rate limited")).await;
                                break;
                            }
                            // 定向转发（§4.3 行为 4/5 + v1.1 §3 路由）：零拷贝
                            // 转交 Utf8Bytes；try_send 不跨连接 await，杜绝
                            // 互相顶死的环。
                            match role {
                                Role::Browser => {
                                    // transport 管理帧（v1.2 设备管理 + passkey
                                    // 凭证管理）：relay 自产自消，不进业务路由
                                    //（先于 machine 分拣）。
                                    if let Some(mf) = parse_manage_frame(text.as_str()) {
                                        match mf {
                                            ManageFrame::DeviceList => {
                                                let _ = socket
                                                    .send(devices_frame(&st.sessions.devices(
                                                        sessions::unix_now(),
                                                    )))
                                                    .await;
                                            }
                                            ManageFrame::DeviceRevoke(device_id) => {
                                                // 单用户语义（见 SessionStore::revoke
                                                // 头注释）：任一在线会话可吊销其它。
                                                // 先摘投递口再回 devices 帧：被吊销
                                                // 连接的 1008 与新列表都是确定态。
                                                let closed = st.sessions.revoke(&device_id, sessions::unix_now());
                                                tracing::info!(target: TARGET, device = %device_id, closed = closed.len(), "会话吊销（device_revoke）");
                                                for tx in closed {
                                                    try_send(&tx, Outbound::Close(CLOSE_POLICY, "session revoked"));
                                                }
                                                let _ = socket
                                                    .send(devices_frame(&st.sessions.devices(
                                                        sessions::unix_now(),
                                                    )))
                                                    .await;
                                            }
                                            // ---- 管理帧族（管理批）：全部须
                                            // adminToken 有效，缺失/无效统一回
                                            // admin_auth_required 并丢弃。----
                                            ManageFrame::MachineList { admin_token } => {
                                                if !admin_gate(&st, &admin_token) {
                                                    let _ = socket.send(error_frame("admin_auth_required")).await;
                                                    continue;
                                                }
                                                admin_audit(
                                                    &admin_session_id(&browser_session),
                                                    "machine_list",
                                                    &format!("machines={}", st.machines.list().len()),
                                                );
                                                let _ = socket
                                                    .send(machines::machines_cfg_frame(
                                                        &st.machines.list(),
                                                    ))
                                                    .await;
                                            }
                                            ManageFrame::MachineAdd { admin_token, name, pubkey } => {
                                                if !admin_gate(&st, &admin_token) {
                                                    let _ = socket.send(error_frame("admin_auth_required")).await;
                                                    continue;
                                                }
                                                match st.machines.add(&name, &pubkey) {
                                                    Ok(()) => {
                                                        admin_audit(
                                                            &admin_session_id(&browser_session),
                                                            "machine_add",
                                                            &format!("machine={name}"),
                                                        );
                                                        let _ = socket
                                                            .send(machines::machines_cfg_frame(
                                                                &st.machines.list(),
                                                            ))
                                                            .await;
                                                    }
                                                    Err(msg) => {
                                                        admin_audit(
                                                            &admin_session_id(&browser_session),
                                                            "machine_add(拒绝)",
                                                            &format!("machine={name} 原因={msg}"),
                                                        );
                                                        let _ = socket
                                                            .send(error_frame_msg(
                                                                "machine_invalid", &msg,
                                                            ))
                                                            .await;
                                                    }
                                                }
                                            }
                                            ManageFrame::MachineRemove { admin_token, name } => {
                                                if !admin_gate(&st, &admin_token) {
                                                    let _ = socket.send(error_frame("admin_auth_required")).await;
                                                    continue;
                                                }
                                                let removed = st.machines.remove(&name);
                                                if removed {
                                                    // 断开该 machine 当前网关连接
                                                    //（close 1008 "machine removed"；
                                                    // 槽位由该连接自己的退出路径清理
                                                    // 并广播 machines 更新）。
                                                    if let Some(gw) = st.hub.gateway_sender(&name) {
                                                        try_send(&gw, Outbound::Close(CLOSE_POLICY, "machine removed"));
                                                    }
                                                }
                                                admin_audit(
                                                    &admin_session_id(&browser_session),
                                                    "machine_remove",
                                                    &format!("machine={name} removed={removed}"),
                                                );
                                                let _ = socket
                                                    .send(machines::machines_cfg_frame(
                                                        &st.machines.list(),
                                                    ))
                                                    .await;
                                            }
                                            ManageFrame::UserTokenRotate { admin_token } => {
                                                if !admin_gate(&st, &admin_token) {
                                                    let _ = socket.send(error_frame("admin_auth_required")).await;
                                                    continue;
                                                }
                                                let token = st.user_token.rotate();
                                                admin_audit(
                                                    &admin_session_id(&browser_session),
                                                    "user_token_rotate",
                                                    "token 不记录（新明文仅回执一次）",
                                                );
                                                // 明文只此一次返回；旧值即刻失效。
                                                let _ = socket.send(user_token_rotated_frame(&token)).await;
                                            }
                                            ManageFrame::PairingApprove { admin_token, code } => {
                                                if !admin_gate(&st, &admin_token) {
                                                    let _ = socket.send(error_frame("admin_auth_required")).await;
                                                    continue;
                                                }
                                                if let Some(reply) = handle_pairing_approve(
                                                    &st,
                                                    &code,
                                                    &admin_session_id(&browser_session),
                                                ) {
                                                    let _ = socket.send(reply).await;
                                                }
                                            }
                                            ManageFrame::PairingDeny { admin_token, code } => {
                                                if !admin_gate(&st, &admin_token) {
                                                    let _ = socket.send(error_frame("admin_auth_required")).await;
                                                    continue;
                                                }
                                                if let Some(reply) = handle_pairing_deny(
                                                    &st,
                                                    &code,
                                                    &admin_session_id(&browser_session),
                                                ) {
                                                    let _ = socket.send(reply).await;
                                                }
                                            }
                                            ManageFrame::CredList
                                            | ManageFrame::CredRevoke(_) => {
                                                // passkey 凭证列表/删除（webauthn
                                                // 模块自产 creds 帧；单用户语义同
                                                // device 族——任一在线会话可删）。
                                                let (kind, cred_id) = match mf {
                                                    ManageFrame::CredRevoke(id) => ("cred_revoke", id),
                                                    _ => ("cred_list", String::new()),
                                                };
                                                let _ = socket
                                                    .send(Message::text(
                                                        webauthn::handle_cred_frame(
                                                            &st, kind, &cred_id,
                                                        ),
                                                    ))
                                                    .await;
                                            }
                                        }
                                        continue;
                                    }
                                    match st.hub.route_browser_frame(text.as_str()) {
                                    BrowserRoute::Deliver(gw) => {
                                        tracing::debug!(target: TARGET, bytes = text.len(), "browser → gateway 透传");
                                        try_send(&gw, Outbound::Msg(Message::Text(text)));
                                    }
                                    BrowserRoute::Buffered => {
                                        tracing::debug!(target: TARGET, bytes = text.len(), "网关全下线，帧入缓冲（单 FIFO）");
                                    }
                                    BrowserRoute::OfflineFull => {
                                        tracing::warn!(target: TARGET, bytes = text.len(), "网关离线缓冲已满，拒新帧（丢新保旧）");
                                        let _ = socket.send(error_frame("gateway_offline")).await;
                                    }
                                BrowserRoute::MachineOffline => {
                                    tracing::debug!(target: TARGET, bytes = text.len(), "目标 machine 不在线，回 machine_offline");
                                    let _ = socket.send(error_frame("machine_offline")).await;
                                }
                                BrowserRoute::Ignored => {
                                    // bootstrap 时序豁免（见 AMBIGUOUS_SILENT_KINDS）：
                                    // 静默丢弃不回错——回 ambiguous_machine 只会让 SPA
                                    // 每次连接弹一次「多台机器在线」toast。
                                    tracing::debug!(target: TARGET, bytes = text.len(), "sync/ping 未指定 machine 且多台网关在线，静默丢弃");
                                }
                                    BrowserRoute::Ambiguous => {
                                        tracing::debug!(target: TARGET, bytes = text.len(), "未指定 machine 且多台网关在线，回 ambiguous_machine");
                                        let _ = socket.send(error_frame("ambiguous_machine")).await;
                                    }
                                    }
                                },
                                Role::Gateway => {
                                    // v1.4：enc{sid} 帧按 sid 定向投递（匹配
                                    // 在线会话 → 只投该连接；空/无匹配 → 广播），
                                    // 见 Hub::route_gateway_frame。
                                    let targets = st.hub.route_gateway_frame(text.as_str());
                                    if targets.is_empty() {
                                        tracing::debug!(target: TARGET, bytes = text.len(), "无在线 browser，帧丢弃（不缓存）");
                                    } else {
                                        tracing::debug!(target: TARGET, bytes = text.len(), browsers = targets.len(), "gateway → browser 投递（enc 帧可定向）");
                                        // Utf8Bytes 是 Arc 语义，逐个 clone 广播零拷贝。
                                        let msg = Outbound::Msg(Message::Text(text));
                                        for b in targets {
                                            try_send(&b, msg.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            },
        }
    }

    // ---- 退出：清槽位（browser 只清自己；网关退出清 machine 槽位并广播）----
    if role == Role::Browser {
        // 摘会话绑定（connected 置 false；会话保留——断开不等于吊销）。
        if let Some(session) = &browser_session {
            st.sessions.unbind(&session.id, conn_id);
        }
        let browsers = st.hub.release_browser(conn_id);
        // 多端排查：断开后还剩几条 browser 在线。
        tracing::info!(target: TARGET, role = role.name(), browsers, "连接断开");
    } else {
        let machine = auth_machine.unwrap_or_default();
        let (browsers, gateways) = st.hub.release_gateway(&machine, conn_id);
        tracing::info!(target: TARGET, role = role.name(), machine = %machine, gateways, browsers, "连接断开");
    }
}

// ---------------------------------------------------------------------------
// server 组装（§4.7：bind 由调用方完成，fail-closed 校验先行）
// ---------------------------------------------------------------------------

/// relay server 句柄：测试与 main 共用。
pub struct RelayHandle {
    /// 实际绑定地址（测试绑 127.0.0.1:0 后由此取随机端口）。
    pub addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl RelayHandle {
    /// 触发 graceful shutdown（幂等；停 accept → 两条 WS close 1001 → drain）。
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// 等 serve 任务退出（shutdown 后 drain 完成才返回）。
    pub async fn wait(self) {
        let _ = self.task.await;
    }
}

/// 起 relay server。必须在 tokio runtime 内调用（内部 tokio::spawn）。
/// listener 由调用方绑定（校验先行，对齐 `src/webhook.rs` P2-2 先例）；
/// 配置不合法直接 Err（fail-closed 双保险）。会话表从
/// `<state_dir>/sessions.json` 加载（v1.2：重启后会话保留）；机器表/
/// USER_TOKEN 哈希按管理批存储权威迁移加载（env 仅首次种子）。
pub fn spawn_relay(listener: TcpListener, config: RelayConfig) -> Result<RelayHandle, String> {
    config.validate()?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("获取监听地址失败：{e}"))?;
    // 管理批存储权威迁移：
    // - 机器表：machines.json 不存在 → env GATEWAY_KEYS 种子导入并落盘；
    //   存在 → 文件权威（env 忽略）。热更新（machine_add/remove）即时生效。
    // - USER_TOKEN：user_token_hash 不存在 → env USER_TOKEN 种子写哈希；
    //   存在 → 文件权威。两者皆无 → Err（fail-closed，不提供无认证模式）。
    let machines = Arc::new(machines::MachineStore::load(
        &config.state_dir,
        &config.gateway_keys,
    ));
    let user_token = Arc::new(UserTokenStore::load(&config.state_dir, &config.user_token)?);
    // fail-closed（§4.2 运行期形态）：网关认证形态（env TOKENS 静态表 /
    // 机器表落盘）至少其一非空。**例外**：machines.json 已存在（= 管理面
    // 已接管机器表，哪怕是清空后的空表）不拒——「UI 删光机器后重启 brick」
    // 不是 fail-closed 想防的东西（空表 = 无网关可认证，是锁死不是裸奔）。
    if config.gateway_tokens.is_empty()
        && machines.is_empty()
        && !machines::table_file_exists(&config.state_dir)
    {
        return Err(
            "网关认证表缺失：env IMAGENT_RELAY_GATEWAY_KEYS（首次种子，machine=<64 hex \
             ed25519 公钥> 逗号分隔）与 IMAGENT_RELAY_GATEWAY_TOKENS（machine=token 逗号\
             分隔）均未配置或为空，且 state_dir 下无 machines.json——全新部署至少配置\
             其一（relay 不提供无认证模式）；已落盘机器表的部署重启不受本检查影响"
                .into(),
        );
    }
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let hub = Hub::new(shutdown_rx.clone());
    let sessions = Arc::new(SessionStore::load(&config.state_dir));
    let webauthn_state = webauthn::new_state(&config.state_dir, config.webauthn_rp_id.clone());
    let state = RelayState {
        hub: hub.clone(),
        config: Arc::new(config),
        sessions,
        webauthn: webauthn_state,
        challenges: Arc::new(Mutex::new(ChallengeTable::default())),
        machines,
        user_token,
        pairing: Arc::new(pairing::PairingStore::default()),
    };
    let app = Router::new()
        .route("/gateway", get(gateway_ws))
        .route("/ws", get(browser_ws))
        .route("/healthz", get(healthz))
        // passkey（WebAuthn）HTTP 端点（注册 begin/complete + 登录 begin/
        // complete，同源 fetch；裸路径 + /imagent 前缀两形态对齐 WS 端点）。
        .merge(webauthn::routes())
        // 前缀路由与静态前缀剥离（spa_fallback）同款理由：兼容不经 Caddy
        // 的直连形态（本地开发/验收直开 http://127.0.0.1:3010/）——SPA 按
        // Vite base=/imagent/ 把 WS/探活地址推导为 /imagent/ws 等；公网
        // Caddy `handle_path /imagent/*` strip 后走上面的原路径。同一
        // handler，两种形态都可用。
        .route("/imagent/gateway", get(gateway_ws))
        .route("/imagent/ws", get(browser_ws))
        .route("/imagent/healthz", get(healthz))
        .fallback(spa_fallback)
        // 安全响应头（SPA 纵深防御）盖住**全部**响应：路由（WS upgrade 101 /
        // healthz）+ fallback（SPA 静态）。注意 layer 只作用于此前已注册的
        // 路由与 fallback，故放在 fallback 之后。
        .layer(middleware::from_fn(security_headers))
        .with_state(state);
    // graceful shutdown（§4.5）：信号到 → close 两条 WS（1001）→ 连接退出
    // → serve drain 完成退出。控制端（RelayHandle）被丢弃同样触发停机：
    // 无人再能主动停机的服务与其失控常驻，不如随控制端退出。
    let drain = {
        let hub = hub.clone();
        let mut rx = shutdown_rx;
        async move {
            loop {
                if rx.changed().await.is_err() {
                    break; // 控制端已丢弃 → 停机
                }
                if *rx.borrow() {
                    break;
                }
            }
            hub.close_all(CLOSE_GOING_AWAY, "going away");
        }
    };
    let task = tokio::spawn(async move {
        // into_make_service_with_connect_info：WebAuthn begin 端点限流需要
        // 直连对端 IP（公网形态恒为 Caddy 127.0.0.1，真实 IP 走
        // X-Forwarded-For，见 webauthn::rate_limit_key）。
        let make_service = app.into_make_service_with_connect_info::<SocketAddr>();
        if let Err(e) = axum::serve(listener, make_service)
            .with_graceful_shutdown(drain)
            .await
        {
            tracing::warn!(target: TARGET, error = %e, "relay serve 退出（错误）");
        }
    });
    tracing::info!(target: TARGET, addr = %addr, "relay listening（/gateway /ws /healthz /webauthn/* + SPA fallback）");
    Ok(RelayHandle {
        addr,
        shutdown: shutdown_tx,
        task,
    })
}

// ---------------------------------------------------------------------------
// 单元测试（纯函数拨钟，任务说明 §5）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 常数时间比较：等长相等 / 等长不等 / 不等长 / 前缀 / 空串。
    #[test]
    fn constant_time_compare_variants() {
        assert!(constant_time_eq("abcdef0123456789", "abcdef0123456789"));
        assert!(
            !constant_time_eq("abcdef0123456789", "abcdef0123456788"),
            "末位不同"
        );
        assert!(!constant_time_eq("short", "short-but-longer"), "不等长");
        assert!(!constant_time_eq("prefix-extra", "prefix"), "前缀（反向）");
        assert!(!constant_time_eq("", "x"));
        assert!(constant_time_eq("", ""));
    }

    /// 滑动窗口限流：20 帧内放行、第 21 帧拒、窗口滑出后恢复。
    #[test]
    fn sliding_window_rate_limit() {
        let mut w = RateWindow::default();
        let t0 = Instant::now();
        for _ in 0..RATE_MAX_FRAMES {
            assert!(w.check_and_record(t0), "前 20 帧放行");
        }
        assert!(!w.check_and_record(t0), "窗口内第 21 帧拒");
        assert!(
            !w.check_and_record(t0 + Duration::from_secs(9)),
            "9s 时旧帧仍在窗内，仍拒"
        );
        assert!(
            w.check_and_record(t0 + RATE_WINDOW),
            "10s 整点旧帧滑出窗口，恢复放行"
        );
        // 恢复后窗口内只有 1 帧：再来 19 帧仍放行，第 21 帧拒。
        for _ in 0..RATE_MAX_FRAMES - 1 {
            assert!(w.check_and_record(t0 + RATE_WINDOW));
        }
        assert!(!w.check_and_record(t0 + RATE_WINDOW));
    }

    /// 离线缓冲：FIFO 序、200 帧上限、1MB 字节上限、溢出拒新、drain 清零。
    #[test]
    fn offline_buffer_fifo_caps_and_drain() {
        // FIFO。
        let mut b = OfflineBuffer::default();
        for f in ["a", "b", "c"] {
            assert!(b.push(f));
        }
        assert_eq!(b.drain(), vec!["a", "b", "c"]);

        // 帧数上限：恰好 200 帧允许，第 201 帧拒。
        let mut b = OfflineBuffer::default();
        for _ in 0..BUF_MAX_FRAMES {
            assert!(b.push("f"));
        }
        assert!(!b.push("f"), "第 201 帧拒（帧数上限）");
        let drained = b.drain();
        assert_eq!(drained.len(), BUF_MAX_FRAMES, "溢出拒新保旧：旧帧完整");
        assert!(b.push("z"), "drain 后清零可再入");
        assert_eq!(b.drain(), vec!["z"]);

        // 字节上限：恰好 1MB 允许（加入后 == 上限未超），再入 1 字节拒。
        let mut b = OfflineBuffer::default();
        let frame_1m = "x".repeat(BUF_MAX_BYTES);
        assert!(b.push(&frame_1m), "恰好 1MB 允许");
        assert!(!b.push("y"), "加入后超 1MB → 拒");
    }

    /// hello 帧解析（v1.2 全量 + v1.3 挑战形态）：token 形态（machine 缺省
    /// default / 显式 / trim / 合法字符 / 各类非法拒绝）；session 形态（优先
    /// 级：session 带值即走会话路径，token 同时存在也不回落）；无凭证形态
    /// （v1.3 Challenge：两字段均缺席才成立，空串仍拒）；device（≤64 char /
    /// trim 后空按缺省 / 超限拒绝）。
    #[test]
    fn hello_frame_parsing() {
        let boot = |t: &str| HelloCredential::Bootstrap(t.to_string());
        let sess = |t: &str| HelloCredential::Session(t.to_string());
        let ok = |cred: HelloCredential, machine: &str, device: Option<&str>| {
            Ok(ParsedHello {
                credential: cred,
                machine: machine.to_string(),
                device: device.map(|d| d.to_string()),
            })
        };
        // token 形态（v1.1 行为不变）。
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc"}"#),
            ok(boot("abc"), "default", None),
            "缺 machine → default"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","machine":"home-mac"}"#),
            ok(boot("abc"), "home-mac", None),
            "显式 machine"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","machine":" home-mac "}"#),
            ok(boot("abc"), "home-mac", None),
            "machine trim"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","machine":"work_01"}"#),
            ok(boot("abc"), "work_01", None),
            "下划线与数字合法"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","machine":""}"#),
            Err(HelloParseError::Malformed),
            "空串 machine 拒绝"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","machine":"   "}"#),
            Err(HelloParseError::Malformed),
            "trim 后为空拒绝"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","machine":"home mac"}"#),
            Err(HelloParseError::Malformed),
            "非法字符（空格）拒绝"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","machine":"家-mac"}"#),
            Err(HelloParseError::Malformed),
            "非 ASCII 拒绝（machine 是路由主键，限 ASCII 可打印子集）"
        );
        assert_eq!(
            parse_hello(&format!(
                r#"{{"v":1,"type":"hello","token":"abc","machine":"{}"}}"#,
                "m".repeat(MACHINE_ID_MAX_CHARS + 1)
            )),
            Err(HelloParseError::Malformed),
            "超 32 char 拒绝"
        );
        // session 形态（v1.2）：session 优先，兼容的 token 形态并存不回落。
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","session":"deadbeef"}"#),
            ok(sess("deadbeef"), "default", None),
            "session 重连形态"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","session":"s1","token":"abc"}"#),
            ok(sess("s1"), "default", None),
            "session 字段优先（同时带 token 不回落换发路径）"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","device":"iPhone"}"#),
            ok(boot("abc"), "default", Some("iPhone")),
            "device 字段"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","device":"  iPhone  "}"#),
            ok(boot("abc"), "default", Some("iPhone")),
            "device trim"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":"abc","device":"   "}"#),
            ok(boot("abc"), "default", None),
            "device trim 后为空按缺省（UA 摘要）"
        );
        assert_eq!(
            parse_hello(&format!(
                r#"{{"v":1,"type":"hello","token":"abc","device":"{}"}}"#,
                "d".repeat(DEVICE_NAME_MAX_CHARS + 1)
            )),
            Err(HelloParseError::Malformed),
            "device 超 64 char 拒绝"
        );
        // 无凭证形态（v1.3 网关 key 握手入口）：两字段均缺席 → Challenge；
        // machine 缺省 default / 显式均可。
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello"}"#),
            ok(HelloCredential::Challenge, "default", None),
            "无凭证 hello → Challenge 形态（v1.3 网关 key 握手；浏览器侧在认证层拒）"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","machine":"home-mac"}"#),
            ok(HelloCredential::Challenge, "home-mac", None),
            "v1.3 key 握手 hello：只带 machine，不带任何凭证"
        );
        // 空串凭证仍拒（缺席 ≠ 空串；空串不构成任何有意的认证形态）。
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","session":""}"#),
            Err(HelloParseError::Malformed),
            "空 session 串拒绝（即使同时带合法 token——session 优先）"
        );
        assert_eq!(
            parse_hello("not json"),
            Err(HelloParseError::Malformed),
            "畸形 JSON"
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"message","token":"x"}"#),
            Err(HelloParseError::NotHello),
            "非 hello 首帧"
        );
        assert_eq!(
            parse_hello(r#"{"v":2,"type":"hello","token":"x"}"#),
            Err(HelloParseError::BadVersion)
        );
        assert_eq!(
            parse_hello(r#"{"v":1,"type":"hello","token":""}"#),
            Err(HelloParseError::Malformed),
            "空 token"
        );
    }

    /// 管理帧分拣（v1.2 设备族 + passkey 凭证族）：device_list /
    /// device_revoke（含缺 deviceId 的 no-op 形态）/ cred_list / cred_revoke
    ///（含缺 credId 的 no-op 形态）/ 业务帧不误判。
    #[test]
    fn manage_frame_parsing() {
        assert!(matches!(
            parse_manage_frame(r#"{"v":1,"type":"device_list"}"#),
            Some(ManageFrame::DeviceList)
        ));
        match parse_manage_frame(r#"{"v":1,"type":"device_revoke","deviceId":"abc"}"#) {
            Some(ManageFrame::DeviceRevoke(id)) => assert_eq!(id, "abc"),
            other => panic!("device_revoke 应解析出 deviceId：{other:?}"),
        }
        // 畸形 device_revoke（缺 deviceId / 非字符串）→ no-op 吊销（回
        // devices 帧），不漏进业务路由。
        match parse_manage_frame(r#"{"v":1,"type":"device_revoke"}"#) {
            Some(ManageFrame::DeviceRevoke(id)) => assert_eq!(id, ""),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse_manage_frame(r#"{"v":1,"type":"device_revoke","deviceId":42}"#),
            Some(ManageFrame::DeviceRevoke(ref id)) if id.is_empty()
        ));
        // passkey 凭证族。
        assert!(matches!(
            parse_manage_frame(r#"{"v":1,"type":"cred_list"}"#),
            Some(ManageFrame::CredList)
        ));
        match parse_manage_frame(r#"{"v":1,"type":"cred_revoke","credId":"c-1"}"#) {
            Some(ManageFrame::CredRevoke(id)) => assert_eq!(id, "c-1"),
            other => panic!("cred_revoke 应解析出 credId：{other:?}"),
        }
        // 畸形 cred_revoke（缺 credId / 非字符串）→ no-op 删除。
        assert!(matches!(
            parse_manage_frame(r#"{"v":1,"type":"cred_revoke"}"#),
            Some(ManageFrame::CredRevoke(ref id)) if id.is_empty()
        ));
        assert!(matches!(
            parse_manage_frame(r#"{"v":1,"type":"cred_revoke","credId":true}"#),
            Some(ManageFrame::CredRevoke(ref id)) if id.is_empty()
        ));
        // 业务帧 / 畸形 JSON → None（走正常路由）。
        assert!(parse_manage_frame(r#"{"v":1,"type":"message","text":"hi"}"#).is_none());
        assert!(parse_manage_frame(r#"{"v":1,"type":"sync"}"#).is_none());
        assert!(parse_manage_frame("not json").is_none());
    }

    /// 管理帧族解析（管理批）：machine_list / machine_add（含字段载荷）/
    /// machine_remove / user_token_rotate / pairing_approve / pairing_deny；
    /// adminToken 缺失 / 非字符串按 None / 空串进入（校验在处理侧统一做）；
    /// machine_add 缺 name/pubkey 按空串。
    #[test]
    fn manage_admin_frame_parsing() {
        let tok = r#","adminToken":"at-1""#;
        match parse_manage_frame(&format!(r#"{{"v":1,"type":"machine_list"{tok}}}"#)) {
            Some(ManageFrame::MachineList { admin_token }) => {
                assert_eq!(admin_token.as_deref(), Some("at-1"))
            }
            other => panic!("machine_list：{other:?}"),
        }
        // adminToken 缺失 → None（处理侧回 admin_auth_required）。
        match parse_manage_frame(r#"{"v":1,"type":"machine_list"}"#) {
            Some(ManageFrame::MachineList { admin_token }) => assert!(admin_token.is_none()),
            other => panic!("{other:?}"),
        }
        match parse_manage_frame(&format!(
            r#"{{"v":1,"type":"machine_add"{tok},"name":"home-mac","pubkey":"ab"}}"#
        )) {
            Some(ManageFrame::MachineAdd {
                admin_token,
                name,
                pubkey,
            }) => {
                assert_eq!(admin_token.as_deref(), Some("at-1"));
                assert_eq!(name, "home-mac");
                assert_eq!(pubkey, "ab");
            }
            other => panic!("machine_add：{other:?}"),
        }
        // machine_add 缺字段 → 空串（machine_invalid 校验路径）。
        match parse_manage_frame(&format!(r#"{{"v":1,"type":"machine_add"{tok}}}"#)) {
            Some(ManageFrame::MachineAdd { name, pubkey, .. }) => {
                assert_eq!(name, "");
                assert_eq!(pubkey, "");
            }
            other => panic!("{other:?}"),
        }
        match parse_manage_frame(&format!(
            r#"{{"v":1,"type":"machine_remove"{tok},"name":"m"}}"#
        )) {
            Some(ManageFrame::MachineRemove { admin_token, name }) => {
                assert_eq!(admin_token.as_deref(), Some("at-1"));
                assert_eq!(name, "m");
            }
            other => panic!("{other:?}"),
        }
        match parse_manage_frame(&format!(r#"{{"v":1,"type":"user_token_rotate"{tok}}}"#)) {
            Some(ManageFrame::UserTokenRotate { admin_token }) => {
                assert_eq!(admin_token.as_deref(), Some("at-1"))
            }
            other => panic!("{other:?}"),
        }
        for kind in ["pairing_approve", "pairing_deny"] {
            match parse_manage_frame(&format!(
                r#"{{"v":1,"type":"{kind}"{tok},"code":"483920"}}"#
            )) {
                Some(ManageFrame::PairingApprove { admin_token, code })
                | Some(ManageFrame::PairingDeny { admin_token, code }) => {
                    assert_eq!(admin_token.as_deref(), Some("at-1"));
                    assert_eq!(code, "483920");
                }
                other => panic!("{kind}：{other:?}"),
            }
        }
        // adminToken 非字符串 → None。
        match parse_manage_frame(r#"{"v":1,"type":"machine_list","adminToken":42}"#) {
            Some(ManageFrame::MachineList { admin_token }) => assert!(admin_token.is_none()),
            other => panic!("{other:?}"),
        }
    }

    /// 配对 hello 分拣（管理批）：pairing:true 无凭证 → Pairing（device
    /// trim/缺省）；pairing:true 与凭证并存 → Reject；device 超限 → Reject；
    /// pairing 缺席/false/非 hello/畸形 → NotPairing。
    #[test]
    fn pairing_hello_classification() {
        let pair = |d: Option<&str>| PairingHello::Pairing {
            device: d.map(|s| s.to_string()),
        };
        assert_eq!(
            classify_pairing_hello(r#"{"v":1,"type":"hello","pairing":true,"device":"新 Mac"}"#),
            pair(Some("新 Mac")),
            "配对入口形态"
        );
        assert_eq!(
            classify_pairing_hello(
                r#"{"v":1,"type":"hello","pairing":true,"device":"  iPhone  "}"#
            ),
            pair(Some("iPhone")),
            "device trim"
        );
        assert_eq!(
            classify_pairing_hello(r#"{"v":1,"type":"hello","pairing":true}"#),
            pair(None),
            "device 缺省（UA 摘要）"
        );
        // pairing:true 与凭证并存 → Reject（意图矛盾）。
        assert_eq!(
            classify_pairing_hello(
                r#"{"v":1,"type":"hello","pairing":true,"token":"abc","device":"d"}"#
            ),
            PairingHello::Reject
        );
        assert_eq!(
            classify_pairing_hello(r#"{"v":1,"type":"hello","pairing":true,"session":"s"}"#),
            PairingHello::Reject
        );
        // device 超限 → Reject。
        assert_eq!(
            classify_pairing_hello(&format!(
                r#"{{"v":1,"type":"hello","pairing":true,"device":"{}"}}"#,
                "d".repeat(DEVICE_NAME_MAX_CHARS + 1)
            )),
            PairingHello::Reject
        );
        // 非 pairing 形态 → NotPairing（走既有认证分拣）。
        assert_eq!(
            classify_pairing_hello(r#"{"v":1,"type":"hello","token":"abc"}"#),
            PairingHello::NotPairing
        );
        assert_eq!(
            classify_pairing_hello(r#"{"v":1,"type":"hello","pairing":false}"#),
            PairingHello::NotPairing,
            "pairing:false = 缺席语义"
        );
        assert_eq!(
            classify_pairing_hello(r#"{"v":2,"type":"hello","pairing":true}"#),
            PairingHello::NotPairing,
            "v!=1 → NotPairing（parse_hello 统一按 BadVersion 拒，形态一致）"
        );
        assert_eq!(
            classify_pairing_hello(r#"{"v":1,"type":"message","pairing":true}"#),
            PairingHello::NotPairing,
            "非 hello"
        );
        assert_eq!(classify_pairing_hello("not json"), PairingHello::NotPairing);
    }

    /// 管理批帧形状（协议对照逐字段）：user_token_rotated / error+message。
    #[test]
    fn admin_frames_shape() {
        let rotated = "ab".repeat(32);
        let m = user_token_rotated_frame(&rotated);
        let v: serde_json::Value = serde_json::from_str(m.into_text().unwrap().as_str()).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "user_token_rotated");
        assert_eq!(v["token"], rotated);

        let m = error_frame_msg("machine_invalid", "机器名 `x` 非法");
        let v: serde_json::Value = serde_json::from_str(m.into_text().unwrap().as_str()).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "error");
        assert_eq!(v["code"], "machine_invalid");
        assert_eq!(v["message"], "机器名 `x` 非法");

        let m = error_frame("admin_auth_required");
        let v: serde_json::Value = serde_json::from_str(m.into_text().unwrap().as_str()).unwrap();
        assert_eq!(v["code"], "admin_auth_required");
        assert!(v.get("message").is_none(), "无 message 变体不带该字段");
    }

    /// 设备名解析：device 字段优先（trim）/ UA 摘要 / 无 UA 回退。
    #[test]
    fn device_name_resolution() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            device_name(Some(" iPhone "), &headers),
            "iPhone",
            "device 字段 trim"
        );
        assert_eq!(
            device_name(Some("   "), &headers),
            "unknown-device",
            "device 空白按缺省，且无 UA → unknown-device"
        );
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0)"),
        );
        let n = device_name(None, &headers);
        assert!(
            n.starts_with("ua-") && n.len() == 11,
            "UA 摘要 ua-+8hex：{n}"
        );
        // 相同 UA → 相同摘要；不同 UA → 不同摘要。
        assert_eq!(n, device_name(None, &headers));
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static("Mozilla/5.0 (Mac)"),
        );
        assert_ne!(n, device_name(None, &headers));
        // 有 device 字段时 UA 不参与。
        assert_eq!(device_name(Some("MacBook"), &headers), "MacBook");
    }

    /// 浏览器 hello_ack 会话载荷 + devices 帧形状（v1.2 帧形状与协议逐字
    /// 段对照——SPA 侧解析依赖字段名）。
    #[test]
    fn session_frames_shape() {
        let s = BrowserSession {
            id: "id-1234".to_string(),
            ack_token: "tok-5678".to_string(),
            expires_at: 1_900_000_000,
        };
        let ack = browser_hello_ack(Role::Browser, &s);
        let ack: serde_json::Value =
            serde_json::from_str(ack.into_text().unwrap().as_str()).unwrap();
        assert_eq!(ack["type"], "hello_ack");
        assert_eq!(ack["role"], "browser");
        assert_eq!(ack["session"]["id"], "id-1234");
        assert_eq!(ack["session"]["token"], "tok-5678");
        assert_eq!(ack["session"]["expiresAt"], 1_900_000_000);

        let devs = devices_frame(&[
            DeviceView {
                id: "d1".to_string(),
                name: "iPhone".to_string(),
                connected: true,
                last_seen: 100,
                created: 90,
            },
            DeviceView {
                id: "d2".to_string(),
                name: "Mac".to_string(),
                connected: false,
                last_seen: 200,
                created: 95,
            },
        ]);
        let v: serde_json::Value =
            serde_json::from_str(devs.into_text().unwrap().as_str()).unwrap();
        assert_eq!(v["type"], "devices");
        let arr = v["devices"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], "d1");
        assert_eq!(arr[0]["name"], "iPhone");
        assert_eq!(arr[0]["connected"], true);
        assert_eq!(arr[0]["lastSeen"], 100);
        assert_eq!(arr[0]["created"], 90);
        assert_eq!(arr[1]["connected"], false);
    }

    /// 业务帧 machine 路由键提取：缺失 / 空串 / 空白 / 非字符串 / 畸形 JSON /
    /// 畸形但含 machine（JSON 解析失败时整体 None——relay 只在解析成功后取
    /// 单字段）。
    #[test]
    fn frame_machine_extraction() {
        assert_eq!(
            frame_machine(r#"{"v":1,"type":"message","text":"hi"}"#),
            None
        );
        assert_eq!(
            frame_machine(r#"{"v":1,"type":"message","machine":"home","text":"hi"}"#),
            Some("home".to_string())
        );
        assert_eq!(
            frame_machine(r#"{"v":1,"type":"ping","machine":"  work  "}"#),
            Some("work".to_string()),
            "路由键同样 trim"
        );
        assert_eq!(frame_machine(r#"{"machine":""}"#), None, "空串按未指定");
        assert_eq!(frame_machine(r#"{"machine":"   "}"#), None, "空白按未指定");
        assert_eq!(frame_machine(r#"{"machine":42}"#), None, "非字符串按未指定");
        assert_eq!(
            frame_machine("not json"),
            None,
            "畸形 JSON 按未指定（帧本身照旧盲转）"
        );
    }

    /// enc 帧 sid 路由键提取（v1.4）：缺失 / 空串 / 空白 / 非字符串 / 畸形
    /// JSON → None（广播现状）；enc 信封形状带 box（relay 不触碰其内容）。
    #[test]
    fn frame_sid_extraction() {
        assert_eq!(
            frame_sid(r#"{"v":1,"type":"enc","machine":"m","sid":"1111-22","box":"AAE"}"#),
            Some("1111-22".to_string())
        );
        assert_eq!(
            frame_sid(r#"{"v":1,"type":"enc","machine":"m","box":"AAE"}"#),
            None,
            "无 sid → 广播（e2e_ack / 明文兼容期形态）"
        );
        assert_eq!(
            frame_sid(r#"{"v":1,"type":"text","machine":"m","text":"hi"}"#),
            None,
            "明文业务帧无 sid → 广播"
        );
        assert_eq!(
            frame_sid(r#"{"v":1,"type":"enc","sid":"  s1  ","box":"x"}"#),
            Some("s1".to_string()),
            "路由键 trim（与 machine 同款）"
        );
        assert_eq!(frame_sid(r#"{"sid":""}"#), None, "空串按未指定");
        assert_eq!(frame_sid(r#"{"sid":"   "}"#), None, "空白按未指定");
        assert_eq!(frame_sid(r#"{"sid":42}"#), None, "非字符串按未指定");
        assert_eq!(frame_sid("not json"), None, "畸形 JSON 按未指定");
    }

    /// gateway→browser 帧 sid 定向路由（v1.4）：enc{sid} 命中在线会话 → 只投
    /// 该会话的连接（同会话多连接全投——多标签）；sid 无匹配 / 无 sid /
    /// 明文帧 → 广播全部（现状语义）。用真实 channel 断言投递归属。
    #[test]
    fn hub_gateway_frame_sid_targeting() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let hub = Hub::new(shutdown_rx);

        // 三个 browser：A 会话两条连接（多标签）+ B 会话一条。
        let (a1_tx, mut a1_rx) = mpsc::channel(8);
        let (a2_tx, mut a2_rx) = mpsc::channel(8);
        let (b_tx, mut b_rx) = mpsc::channel(8);
        hub.register_browser(1, "sess-a", a1_tx);
        hub.register_browser(2, "sess-a", a2_tx);
        hub.register_browser(3, "sess-b", b_tx);

        // enc{sid=sess-a} → 只投 A 的两条连接，B 不收。
        let targets = hub.route_gateway_frame(
            r#"{"v":1,"type":"enc","machine":"m","sid":"sess-a","box":"AAE"}"#,
        );
        assert_eq!(targets.len(), 2, "同会话多标签全投");
        for t in &targets {
            try_send(t, Outbound::Msg(Message::text("enc-a")));
        }
        assert_eq!(
            a1_rx.try_recv().ok().map(|o| text_of(&o)),
            Some("enc-a".into())
        );
        assert_eq!(
            a2_rx.try_recv().ok().map(|o| text_of(&o)),
            Some("enc-a".into())
        );
        assert!(b_rx.try_recv().is_err(), "B 会话不得收到 A 的定向 enc 帧");

        // enc{sid=unknown} → 无匹配，广播全部（SPA 未上线窗口兜底）。
        let targets = hub.route_gateway_frame(
            r#"{"v":1,"type":"enc","machine":"m","sid":"sess-gone","box":"AAE"}"#,
        );
        assert_eq!(targets.len(), 3, "无匹配回退广播");
        for t in &targets {
            try_send(t, Outbound::Msg(Message::text("fanout")));
        }
        for rx in [&mut a1_rx, &mut a2_rx, &mut b_rx] {
            assert_eq!(
                rx.try_recv().ok().map(|o| text_of(&o)),
                Some("fanout".into())
            );
        }

        // 无 sid（明文业务帧 / e2e_ack）→ 广播全部（现状语义）。
        assert_eq!(
            hub.route_gateway_frame(r#"{"v":1,"type":"text","machine":"m","text":"t"}"#)
                .len(),
            3
        );
        assert_eq!(
            hub.route_gateway_frame(r#"{"v":1,"type":"e2e_ack","machine":"m"}"#)
                .len(),
            3
        );

        // A 会话一条连接断开（多标签关一）→ 定向只投剩下那条；B 不受影响。
        hub.release_browser(1);
        let targets = hub.route_gateway_frame(
            r#"{"v":1,"type":"enc","machine":"m","sid":"sess-a","box":"AAE"}"#,
        );
        assert_eq!(targets.len(), 1, "同会话剩一条连接 → 只投它");
    }

    /// Outbound::Msg 文本载荷提取（sid 路由测试断言用）。
    fn text_of(o: &Outbound) -> String {
        match o {
            Outbound::Msg(m) => m.clone().into_text().unwrap().as_str().to_string(),
            Outbound::Close(..) => panic!("期待 Msg"),
        }
    }

    /// 帧 type 提取（歧义静默豁免判定用）：常规 / 缺失 / 非字符串 / 畸形。
    #[test]
    fn frame_kind_extraction() {
        assert_eq!(
            frame_kind(r#"{"v":1,"type":"sync"}"#),
            Some("sync".to_string())
        );
        assert_eq!(
            frame_kind(r#"{"v":1,"type":"ping","machine":"m"}"#),
            Some("ping".to_string())
        );
        assert_eq!(frame_kind(r#"{"v":1,"text":"无类型"}"#), None, "缺 type");
        assert_eq!(frame_kind(r#"{"type":42}"#), None, "非字符串");
        assert_eq!(frame_kind("not json"), None, "畸形 JSON");
        // 豁免判定：sync/ping 命中，message / 未知 / 畸形不豁免。
        assert!(is_ambiguous_silent_kind(r#"{"v":1,"type":"sync"}"#));
        assert!(is_ambiguous_silent_kind(r#"{"v":1,"type":"ping"}"#));
        assert!(!is_ambiguous_silent_kind(r#"{"v":1,"type":"message"}"#));
        assert!(!is_ambiguous_silent_kind(
            r#"{"v":1,"type":"approval_response"}"#
        ));
        assert!(!is_ambiguous_silent_kind(r#"{"v":1,"type":"control"}"#));
        assert!(!is_ambiguous_silent_kind("not json"));
    }

    /// 网关 token-machine 绑定表（v1.2）：解析（多对/trim/尾随逗号容忍/
    /// 各类畸形拒绝/重复拒绝）+ 绑定校验（对/错 machine、未知 token、
    /// Bearer=default 只认 default 绑定）。
    #[test]
    fn gateway_tokens_parse_and_verify() {
        // 合法多对（trim + 尾随逗号容忍）。
        let t = GatewayTokens::parse(" home-mac = 9a08 , work-mac=7f2b ,, ").unwrap();
        assert_eq!(
            t.machines(),
            vec!["home-mac".to_string(), "work-mac".to_string()]
        );
        // 绑定校验：token 对 + machine 对 → 放行。
        assert!(t.verify("home-mac", "9a08"), "正确绑定");
        assert!(t.verify("work-mac", "7f2b"), "正确绑定");
        // token 对但 machine 不匹配 → 拒（泄露只能扮演自己）。
        assert!(
            !t.verify("work-mac", "9a08"),
            "home 的 token 顶替 work → 拒"
        );
        assert!(
            !t.verify("default", "9a08"),
            "home 的 token 走 Bearer(default) → 拒"
        );
        // 未知 token → 拒。
        assert!(!t.verify("home-mac", "unknown"), "未知 token");
        assert!(!t.verify("home-mac", ""), "空 token");
        // default 绑定的 token 可走 Bearer。
        let t2 = GatewayTokens::parse("default=dk").unwrap();
        assert!(t2.verify("default", "dk"));

        // 解析失败（fail-closed）：空表 / 缺 `=` / 空段 / 非法 machine /
        // machine 重复 / token 重复绑定。
        for (spec, needle) in [
            ("", "为空"),
            ("   ", "为空"),
            ("home-mac", "缺 `=`"),
            ("home-mac=", "为空"),
            ("=9a08", "为空"),
            ("home mac=t1", "非法"),
            ("家=t1", "非法"),
            ("home-mac=t1,home-mac=t2", "重复"),
            ("home-mac=t1,work-mac=t1", "同时绑定"),
        ] {
            let err = GatewayTokens::parse(spec).unwrap_err();
            assert!(err.contains(needle), "spec=`{spec}` msg={err}");
        }
    }

    /// 测试内派生 ed25519 密钥对（seed 固定可复现）。
    fn test_signing_key(seed: u8) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
    }

    /// 网关公钥表（v1.3）：解析（合法多对 / 尾随逗号容忍 / 空表 Ok /
    /// 各类畸形拒绝 / 非法公钥点拒绝 / 重复绑定拒绝）+ prove 验证（对/
    /// 错 machine / 公钥不一致 / 签名错 / 畸形 hex / 签名对象 = nonce 原文）。
    #[test]
    fn gateway_keys_parse_and_verify() {
        use ed25519_dalek::Signer;
        let pk = |seed: u8| hex::encode(test_signing_key(seed).verifying_key().to_bytes());
        // 合法多对（trim + 尾随逗号容忍）；machine 列表与指纹有序。
        let k =
            GatewayKeys::parse(&format!(" home-mac = {} , work-mac={} ,, ", pk(1), pk(2))).unwrap();
        assert_eq!(
            k.machines(),
            vec!["home-mac".to_string(), "work-mac".to_string()]
        );
        let fps = k.fingerprints();
        assert_eq!(fps.len(), 2);
        assert!(fps[0].starts_with("home-mac=") && fps[0].len() == "home-mac=".len() + 16);

        // prove 验证：正确私钥签 nonce → 通过。
        let nonce = "a".repeat(64);
        let sk = test_signing_key(1);
        let sig = hex::encode(sk.sign(nonce.as_bytes()).to_bytes());
        assert!(
            k.verify("home-mac", &pk(1), &nonce, &sig),
            "绑定公钥 + 正确签名 → 通过"
        );
        // 签名被篡改（首字符翻转）→ 拒。
        let tampered = format!(
            "{}{}",
            if sig.starts_with('a') { 'b' } else { 'a' },
            &sig[1..]
        );
        assert!(
            !k.verify("home-mac", &pk(1), &nonce, &tampered),
            "篡改签名 → 拒"
        );
        // machine 不在表 → 拒。
        assert!(!k.verify("ghost", &pk(1), &nonce, &sig), "未知 machine");
        // 公钥不一致（另一把合法钥）→ 拒（恒定时间比对路径）。
        assert!(
            !k.verify("home-mac", &pk(2), &nonce, &sig),
            "prove 公钥 ≠ 绑定公钥 → 拒"
        );
        // 正确公钥但签名来自错私钥 → 拒（验签路径）。
        let bad_sig = hex::encode(test_signing_key(2).sign(nonce.as_bytes()).to_bytes());
        assert!(
            !k.verify("home-mac", &pk(1), &nonce, &bad_sig),
            "验签失败 → 拒"
        );
        // 畸形输入：非 hex 签名 / 签名长度错 / 空。
        assert!(!k.verify("home-mac", &pk(1), &nonce, "zz"));
        assert!(!k.verify("home-mac", &pk(1), &nonce, "ab"));
        assert!(!k.verify("home-mac", &pk(1), &nonce, ""));

        // 空表 Ok（validate 层做「至少其一」判定）。
        assert!(GatewayKeys::parse("").unwrap().is_empty());
        assert!(GatewayKeys::parse("  ,, ").unwrap().is_empty());
        // 找一个确定非法的「曲线点」：随机形态的字节串约半数解不出点，
        // 逐个递增首字节直到 from_bytes 拒（两三步内必然命中）。
        let mut bad = [0xffu8; 32];
        let mut probe = 0u8;
        while VerifyingKey::from_bytes(&bad).is_ok() {
            bad[0] = probe;
            probe = probe.wrapping_add(1);
        }
        let not_a_point = hex::encode(bad);
        // 解析失败：缺 `=` / 空段 / 非法 machine / 非 hex / 长度错 /
        // machine 重复 / 公钥重复绑定 / 非法曲线点。
        for (spec, needle) in [
            ("home-mac", "缺 `=`"),
            ("home-mac=", "为空"),
            ("=ab", "为空"),
            ("home mac=k", "非法"),
            (r#"home-mac=xyz"#, "非合法 hex"),
            (&format!("home-mac={}", "ab".repeat(31)), "长度"),
            (&format!("home-mac={},home-mac={}", pk(1), pk(2)), "重复"),
            (
                &format!("home-mac={},work-mac={}", pk(1), pk(1)),
                "同时绑定",
            ),
            (&format!("home-mac={not_a_point}"), "合法 ed25519 公钥点"),
        ] {
            let err = GatewayKeys::parse(spec).unwrap_err();
            assert!(err.contains(needle), "spec=`{spec}` msg={err}");
        }
    }

    /// challenge 表（v1.3）：签发绑定 machine / 单次消费（第二次 None）/
    /// 60s 过期 / 未知 nonce / 容量上限驱逐最旧。
    #[test]
    fn challenge_table_lifecycle() {
        let t0 = Instant::now();
        let mut t = ChallengeTable::default();
        let n1 = t.issue("home-mac", t0);
        let n2 = t.issue("work-mac", t0);
        assert_ne!(n1, n2, "nonce 互不相同");
        assert_eq!(n1.len(), 64, "32B hex");
        assert!(n1.chars().all(|c| c.is_ascii_hexdigit()));
        // 单次消费：命中返回绑定 machine，再消费 None。
        assert_eq!(t.consume(&n1, t0).as_deref(), Some("home-mac"));
        assert!(t.consume(&n1, t0).is_none(), "nonce 已消费（防重放）");
        // 过期（恰 60s 边界视为过期——prove 须在 TTL 内到达）。
        assert!(t.consume(&n2, t0 + CHALLENGE_TTL).is_none(), "过期作废");
        // 未知 nonce。
        assert!(t.consume(&"f".repeat(64), t0).is_none());
        // 过期条目在后续签发时被清出（容量不泄漏）。
        let n3 = t.issue("m3", t0 + CHALLENGE_TTL + Duration::from_secs(1));
        assert!(!t.map.contains_key(&n2), "过期条目已 prune");
        assert!(t
            .consume(&n3, t0 + CHALLENGE_TTL + Duration::from_secs(2))
            .is_some());
        // 容量上限：灌满 +1 条，最早的（n3 时刻之后最早签发的）被驱逐，
        // 表大小不超过上限。
        let mut t = ChallengeTable::default();
        let base = Instant::now();
        let first = t.issue("first", base);
        for i in 0..CHALLENGE_MAX_ENTRIES {
            t.issue(&format!("m{i}"), base + Duration::from_millis(i as u64 + 1));
        }
        assert!(t.map.len() <= CHALLENGE_MAX_ENTRIES, "容量有界");
        assert!(!t.map.contains_key(&first), "最早签发的被驱逐（最旧优先）");
    }

    /// challenge 帧形状（协议对照逐字段）：v/type/nonce。
    #[test]
    fn challenge_frame_shape() {
        let m = challenge_frame("deadbeef");
        let v: serde_json::Value = serde_json::from_str(m.into_text().unwrap().as_str()).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "challenge");
        assert_eq!(v["nonce"], "deadbeef");
    }

    /// 配置期校验（管理批后收敛）：env USER_TOKEN 种子与任一网关 token
    /// 相同 → 拒（职责分离）；网关表空 / USER_TOKEN 种子空不再由
    /// [`RelayConfig::validate`] 拒——存储权威迁移后（machines.json /
    /// user_token_hash 落盘）是否还有可用认证形态是**运行期**问题，由
    /// [`spawn_relay`] 结合落盘真实状态判定（空表 + 无种子 + 无文件才拒）。
    #[test]
    fn fail_closed_token_validation() {
        let cfg = |g_tokens: GatewayTokens, u: &str| RelayConfig {
            gateway_tokens: g_tokens,
            gateway_keys: GatewayKeys::default(),
            user_token: u.to_string(),
            state_dir: std::env::temp_dir(),
            session_ttl_secs: DEFAULT_SESSION_TTL_SECS,
            pairing_ttl_secs: DEFAULT_PAIRING_TTL_SECS,
            webauthn_rp_id: None,
        };
        // 迁移后 env 全移除形态（落盘文件权威）：validate 不拒。
        assert!(
            cfg(GatewayTokens::default(), "").validate().is_ok(),
            "空表 + 空种子：落盘权威形态由 spawn_relay 运行期判定"
        );
        // 正常形态。
        assert!(
            cfg(GatewayTokens::parse("default=g-token").unwrap(), "u-token")
                .validate()
                .is_ok()
        );
        // 职责分离：USER_TOKEN 种子与网关 token 相同 → 拒。
        let err = cfg(
            GatewayTokens::parse("default=same,home=other").unwrap(),
            "same",
        )
        .validate()
        .unwrap_err();
        assert!(err.contains("不得与任何网关 token 相同"), "msg={err}");
    }

    /// fail-closed：非 loopback bind → 拒（报错给修复指引）。
    #[test]
    fn fail_closed_bind_validation() {
        assert!(validate_bind("127.0.0.1:3010".parse().unwrap()).is_ok());
        assert!(validate_bind("[::1]:3010".parse().unwrap()).is_ok());
        for bad in ["0.0.0.0:3010", "192.168.1.10:3010", "10.0.0.1:8443"] {
            let err = validate_bind(bad.parse().unwrap()).unwrap_err();
            assert!(
                err.contains("loopback") && err.contains("Caddy"),
                "addr={bad} msg={err}"
            );
        }
    }

    /// Hub 槽位语义（v1.1 多机版）：注册/路由/释放互不误伤（无网络，纯
    /// channel）。覆盖：多机器并存、同 machine 踢旧、browser 帧 machine
    /// 路由（指定命中 / 指定离线 / 缺省单台 / 缺省多台 ambiguous / 全下线
    /// 入缓冲）、release 广播帧形态。
    #[test]
    fn hub_register_route_release() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(false); // sender 保留，避免触发「控制端丢弃=停机」
        let hub = Hub::new(shutdown_rx);

        // 网关注册（default 机器）：空环境 → 无踢旧、无在线 browser、无重放；
        // machines 列表含 default、is_default=true。
        let (gw_tx, mut gw_rx) = mpsc::channel(8);
        let reg = hub.register_gateway("default", 1, gw_tx).unwrap();
        assert!(reg.superseded.is_none() && reg.browsers.is_empty() && reg.replay.is_empty());
        assert_eq!(reg.machines, vec!["default".to_string()]);
        assert_eq!(reg.gateways, 1);
        assert!(reg.is_default);

        // browser 注册（多连接并存）：default 机器在线快照 + 全量 machines 快照。
        let (br_tx, mut br_rx) = mpsc::channel(8);
        let regb = hub.register_browser(3, "sess-br3", br_tx);
        assert!(regb.machine_online && !regb.full && regb.browsers == 1);
        assert_eq!(regb.machines, vec!["default".to_string()]);

        // 第二台机器注册（home）：并存不踢 default；browser 快照含两台。
        let (home_tx, mut home_rx) = mpsc::channel(8);
        let reg2 = hub.register_gateway("home", 2, home_tx).unwrap();
        assert!(reg2.superseded.is_none(), "不同机器不互踢");
        assert_eq!(
            reg2.machines,
            vec!["default".to_string(), "home".to_string()]
        );
        assert_eq!(reg2.gateways, 2);
        assert!(!reg2.is_default);

        // ---- machine 路由（v1.1 §3 四分支）----
        // 显式指定且在线 → 路由到对应机器。
        match hub.route_browser_frame(r#"{"v":1,"type":"message","machine":"home","text":"a"}"#) {
            BrowserRoute::Deliver(tx) => {
                try_send(&tx, Outbound::Msg(Message::text("a")));
                assert_eq!(
                    home_rx.try_recv().ok().and_then(|o| match o {
                        Outbound::Msg(m) => Some(m.into_text().unwrap().as_str().to_string()),
                        _ => None,
                    }),
                    Some("a".to_string()),
                    "machine=home 应路由到 home 网关"
                );
                // default 机器不受影响。
                assert!(gw_rx.try_recv().is_err(), "default 网关不应收到 home 帧");
            }
            other => panic!("machine=home 在线应 Deliver：{other:?}"),
        }
        match hub.route_browser_frame(r#"{"v":1,"type":"message","machine":"default"}"#) {
            BrowserRoute::Deliver(tx) => {
                try_send(&tx, Outbound::Msg(Message::text("b")));
                match gw_rx.try_recv() {
                    Ok(Outbound::Msg(m)) => assert_eq!(m.into_text().unwrap().as_str(), "b"),
                    other => panic!("应透传到 default 网关 channel：{other:?}"),
                }
            }
            other => panic!("machine=default 在线应 Deliver：{other:?}"),
        }
        // 显式指定但不在线 → MachineOffline。
        assert!(matches!(
            hub.route_browser_frame(r#"{"v":1,"type":"message","machine":"ghost"}"#),
            BrowserRoute::MachineOffline
        ));
        // 未指定 + 两台在线 → Ambiguous。
        assert!(matches!(
            hub.route_browser_frame(r#"{"v":1,"type":"message","text":"c"}"#),
            BrowserRoute::Ambiguous
        ));
        // bootstrap 豁免（v1.1）：sync/ping 未指定 + 多台在线 → 静默忽略
        //（不路由不回错）；畸形 JSON（type 不可知）不豁免，仍 Ambiguous。
        assert!(matches!(
            hub.route_browser_frame(r#"{"v":1,"type":"sync"}"#),
            BrowserRoute::Ignored
        ));
        assert!(matches!(
            hub.route_browser_frame(r#"{"v":1,"type":"ping"}"#),
            BrowserRoute::Ignored
        ));
        assert!(matches!(
            hub.route_browser_frame("not json"),
            BrowserRoute::Ambiguous
        ));

        // 踢旧：同 machine（default）第二条注册顶替第一条，旧投递口由调用方
        // close 4001（每台机器单连接语义）。
        let (gw2_tx, _gw2_rx) = mpsc::channel(8);
        let reg3 = hub.register_gateway("default", 5, gw2_tx).unwrap();
        let old = reg3.superseded.expect("同 machine 应踢旧");
        try_send(&old, Outbound::Close(CLOSE_SUPERSEDED, "superseded"));
        assert!(matches!(
            gw_rx.try_recv(),
            Ok(Outbound::Close(CLOSE_SUPERSEDED, "superseded"))
        ));
        assert_eq!(reg3.gateways, 2, "踢旧是顶替不是新增槽位");

        // 旧连接（conn_id=1）退出不得误清新槽位：default 机器仍在线。
        hub.release_gateway("default", 1);
        assert!(matches!(
            hub.route_browser_frame(r#"{"v":1,"type":"message","machine":"default","text":"d"}"#),
            BrowserRoute::Deliver(_)
        ));

        // 现 default 网关（conn_id=5）退出 → 广播：旧 machine(false)（default
        // 机器，v1 兼容）+ machines 全量列表（剩 home）；两帧顺序 machine 在前。
        hub.release_gateway("default", 5);
        match br_rx.try_recv() {
            Ok(Outbound::Msg(m)) => {
                let s = m.into_text().unwrap();
                assert!(
                    s.contains(r#""type":"machine""#) && s.contains("false"),
                    "旧 machine 帧（default 下线）：{s}"
                );
            }
            other => panic!("应先广播旧 machine 帧：{other:?}"),
        }
        match br_rx.try_recv() {
            Ok(Outbound::Msg(m)) => {
                let s = m.into_text().unwrap();
                assert!(
                    s.contains(r#""type":"machines""#)
                        && s.contains("home")
                        && !s.contains("default"),
                    "machines 列表应只剩 home：{s}"
                );
            }
            other => panic!("应后广播 machines 帧：{other:?}"),
        }

        // 未指定 + 仅剩一台（home）→ 路由它（单机部署 SPA 零升级无感）。
        assert!(matches!(
            hub.route_browser_frame(r#"{"v":1,"type":"message","text":"e"}"#),
            BrowserRoute::Deliver(_)
        ));

        // home 网关退出：非 default 机器 → 只广播 machines，无旧 machine 帧。
        hub.release_gateway("home", 2);
        match br_rx.try_recv() {
            Ok(Outbound::Msg(m)) => {
                let s = m.into_text().unwrap();
                assert!(s.contains(r#""type":"machines""#), "machines 广播：{s}");
            }
            other => panic!("应广播 machines 帧：{other:?}"),
        }
        assert!(
            br_rx.try_recv().is_err(),
            "非 default 机器不发旧 machine 帧"
        );

        // 全下线：未指定帧入缓冲（单 FIFO，重放序 = 入队序）；sync/ping 的
        // 静默豁免只作用于歧义分支（多台在线），零台在线沿用缓冲语义。
        let frames = ["1", "2", "3", r#"{"v":1,"type":"sync"}"#];
        for f in frames {
            assert!(matches!(hub.route_browser_frame(f), BrowserRoute::Buffered));
        }
        let (gw3_tx, _gw3_rx) = mpsc::channel(8);
        let reg4 = hub.register_gateway("home", 9, gw3_tx).unwrap();
        assert_eq!(reg4.replay, frames, "重放保 FIFO 序");
        assert_eq!(reg4.browsers.len(), 1, "上线广播目标含全部在线 browser");

        // browser 退出只清自己：br（conn_id=3）退出后广播目标清零。
        assert_eq!(hub.release_browser(3), 0);
        assert!(hub
            .route_gateway_frame(r#"{"v":1,"type":"text"}"#)
            .is_empty());
    }

    /// 网关机器数上限：第 [`MAX_GATEWAYS`] 台（不同 machine）注册成功，第
    /// MAX_GATEWAYS+1 台 None（调用方 close 4001）；**同 machine 重连不受
    /// 上限约束**（踢旧顶替，槽位复用）；释放一台后新 machine 可再入。
    #[test]
    fn hub_gateway_machine_cap() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let hub = Hub::new(shutdown_rx);

        for i in 1..=MAX_GATEWAYS {
            let (tx, _rx) = mpsc::channel(8);
            let reg = hub
                .register_gateway(&format!("m{i}"), i as u64, tx)
                .unwrap_or_else(|| panic!("第 {i} 台应注册成功"));
            assert_eq!(reg.gateways, i);
        }
        // 满员 + 新 machine → None。
        let (tx, _rx) = mpsc::channel(8);
        assert!(
            hub.register_gateway("m-extra", 99, tx).is_none(),
            "满员新机器应被拒"
        );
        // 满员 + 同 machine 重连 → 踢旧顶替（不受上限约束）。
        let (tx2, _rx2) = mpsc::channel(8);
        let reg = hub.register_gateway("m2", 100, tx2).unwrap();
        assert!(reg.superseded.is_some(), "同 machine 重连踢旧");
        assert_eq!(reg.gateways, MAX_GATEWAYS);
        // 旧连接（conn_id=2）退出不误清新连接。
        hub.release_gateway("m2", 2);
        let (tx3, _rx3) = mpsc::channel(8);
        assert!(
            hub.register_gateway("m2", 101, tx3).is_some(),
            "新连接仍在槽位，同 machine 再连仍走踢旧"
        );
        // 释放一台 → 新 machine 槽位立即可复用。
        hub.release_gateway("m1", 1);
        let (tx4, _rx4) = mpsc::channel(8);
        assert!(hub.register_gateway("m-extra", 102, tx4).is_some());
    }

    /// browser 并存上限：第 [`MAX_BROWSERS`] 条注册成功，第 MAX_BROWSERS+1
    /// 条 full（未入表，调用方 close 4001 自己）；未注册 conn_id 的退出清
    /// 槽位是 no-op；释放一条后槽位立即可复用。
    #[test]
    fn hub_browser_cap() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let hub = Hub::new(shutdown_rx);

        for i in 1..=MAX_BROWSERS {
            let (tx, _rx) = mpsc::channel(8);
            let reg = hub.register_browser(i as u64, &format!("sess-{i}"), tx);
            assert!(!reg.full, "第 {i} 条应注册成功");
            assert_eq!(reg.browsers, i);
        }
        // 超上限：full=true 且不注册（该连接随后 close 4001 自己）。
        let (tx, _rx) = mpsc::channel(8);
        let reg = hub.register_browser(999, "sess-999", tx);
        assert!(reg.full);
        assert_eq!(reg.browsers, MAX_BROWSERS, "满员时在线数不变");

        // 未注册的 conn_id 退出是 no-op（不误伤在线连接）。
        assert_eq!(hub.release_browser(999), MAX_BROWSERS);

        // 释放一条 → 槽位立即可复用。
        assert_eq!(hub.release_browser(1), MAX_BROWSERS - 1);
        let (tx, _rx) = mpsc::channel(8);
        let reg = hub.register_browser(1000, "sess-1000", tx);
        assert!(!reg.full && reg.browsers == MAX_BROWSERS);
    }
}
