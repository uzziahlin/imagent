//! 网关侧密钥管理（v1.3 认证 ed25519 + v1.4 E2E X25519）。
//!
//! ## 认证密钥（v1.3：挑战应答替代静态 token）
//!
//! 密钥文件 `<imagent_home>/web-gateway-key`（路径由 main 装配期传入——
//! 与 local.sock 同款注入模式，不硬编 home）：
//! - **不存在 → 首次自动生成**密钥对：私钥 seed（32B）以 64-hex 单行写
//!   文件，创建即 0600（`create_new` 原子防并发双写）；生成时 `warn!`
//!   打印完整公钥 hex 与配置指引——一次性、用户看得见（公钥是公开材料
//!   可全量打印）；
//! - **存在 → 加载**：内容须为 64-hex seed 单行（畸形 → Err，装配层决定
//!   是否回退 token）；权限过宽（非 0600）→ 直接收紧为 0600 并 warn
//!   （不停启动——私钥在本机盘上，收紧即消除暴露面）。
//!
//! 装配决策（[`load_gateway_credential`]）：**key 优先，token 回退**——
//! 密钥文件可用即走 key 握手（env 同时有 token 时 key 仍优先）；文件损坏
//! 时回退 token 并 warn；两者皆不可用 → Err（fail-closed，relay 无匿名
//! 形态）。
//!
//! ## E2E 密钥（v1.4：中继盲化的内容保密身份）
//!
//! 独立的 X25519 静态密钥对，文件 `<imagent_home>/web-e2e-key`（同款
//! 文件纪律，见 [`load_or_generate_e2e`]）——**独立于认证密钥是职责分离**：
//! ed25519 管「网关对 relay 的身份」（v1.3，公钥配在 relay env），X25519
//! 管「浏览器↔网关内容保密」（v1.4，公钥经 e2e_ack 递给浏览器做 TOFU
//! 指纹核对）。指纹 = SHA-256(公钥) 前 16 hex（见 [`e2e_fingerprint`]），
//! 启动日志打印——SPA 首连指纹确认弹层与终端日志人工对照。

use std::path::Path;

use ed25519_dalek::SigningKey;
use tracing::{debug, info, warn};
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret as XStaticSecret};

use crate::TARGET;

/// 网关认证凭证（v1.3）：握手形态二选一，client 据此分流（key 三帧 /
/// token 一步）。
#[derive(Clone, Debug)]
pub enum GatewayCredential {
    /// ed25519 挑战应答（hello 无凭证 → challenge → prove）：relay 只存
    /// 公钥，私钥 seed 不出本机。
    Key(SigningKey),
    /// 静态 bearer token（v1.2 兼容路径，hello 一步校验）。
    Token(String),
}

/// 密钥文件权限（unix）：私钥 seed 仅本用户可读写。
#[cfg(unix)]
const KEY_FILE_MODE: u32 = 0o600;

/// 加载或生成网关密钥（文件不存在 → 生成并 warn 公钥指引）。
pub fn load_or_generate(path: &Path) -> Result<SigningKey, String> {
    if path.exists() {
        load(path)
    } else {
        generate(path)
    }
}

/// 装配期入口（main build_platform web 分支）：**key 优先，token 回退**。
/// - 密钥文件可用 → [`GatewayCredential::Key`]（env token 存在也仅作
///   回退凭证保留，debug 说明）；
/// - 密钥文件损坏 → 回退 [`GatewayCredential::Token`] 并 warn；
/// - 无 token 可回退 → Err（fail-closed：错误信息含修复指引）。
pub fn load_gateway_credential(
    key_path: &Path,
    token: Option<String>,
) -> Result<GatewayCredential, String> {
    match load_or_generate(key_path) {
        Ok(sk) => {
            if token.is_some() {
                debug!(
                    target: TARGET,
                    key_path = %key_path.display(),
                    "网关走 ed25519 key 认证（env token 仅作密钥文件损坏时的回退凭证）"
                );
            }
            Ok(GatewayCredential::Key(sk))
        }
        Err(e) => match token {
            Some(t) => {
                warn!(
                    target: TARGET,
                    key_path = %key_path.display(),
                    error = %e,
                    "网关密钥文件不可用，回退静态 token 认证（v1.2 兼容路径）"
                );
                Ok(GatewayCredential::Token(t))
            }
            None => Err(format!(
                "{e}；且未设置 IMAGENT_WEB_GATEWAY_TOKEN（token 回退）——\
                 请修复密钥文件（删除后重启可自动重新生成），或设置该环境变量"
            )),
        },
    }
}

/// 首次生成：随机 seed → 0600 原子落盘（create_new：并发双写必有一方
/// 失败回退读取）→ warn 打印公钥与配置指引（一次性，用户看得见）。
fn generate(path: &Path) -> Result<SigningKey, String> {
    let seed = generate_seed_file(path)?;
    let sk = SigningKey::from_bytes(&seed);
    warn!(
        target: TARGET,
        key_path = %path.display(),
        pubkey = %hex::encode(sk.verifying_key().to_bytes()),
        "已生成网关 ed25519 密钥对（首次自动生成，仅此一次提示）：\
         将上方 pubkey（64 hex 公钥）配到 ECS 侧 IMAGENT_RELAY_GATEWAY_KEYS 的\
         对应机器名下（machine=<公钥>，逗号分隔多台），重启 relay 生效"
    );
    Ok(sk)
}

/// 加载既有密钥：64-hex seed 单行 → SigningKey；畸形 → Err；权限过宽 →
/// 收紧 0600 + warn（不停启动）。
fn load(path: &Path) -> Result<SigningKey, String> {
    let seed = load_seed(path)?;
    tighten_permissions(path)?;
    Ok(SigningKey::from_bytes(&seed))
}

// ---------------------------------------------------------------------------
// E2E X25519 密钥（协议 v1.4：浏览器↔网关业务帧中继盲化的内容保密身份）
// ---------------------------------------------------------------------------

/// 加载或生成 E2E X25519 静态密钥对（文件不存在 → 生成；同款 0600 文件
/// 纪律）。**E2E 握手的 e2e_ack 签名复用 v1.3 认证 ed25519 私钥**（浏览器
/// 已 pin 的网关公钥即它），故调用方（e2e::E2eIdentity 装配）只在 key
/// 凭证形态下构建 E2E 身份——token 凭证（v1.2 兼容）无签名密钥，E2E
/// 不可用（e2e_hello 忽略，SPA 侧 10s fail-closed 提示明文选择）。
///
/// **静态密钥的权衡**（协议定案）：浏览器侧每次连接新临时 X25519 密钥，
/// 网关侧 gk 静态——**网关静态私钥一旦泄露，其后被录制的流量可解**，这是
/// 定案接受的取舍（浏览器侧临时密钥保证不同浏览器会话间互不可解）。
/// 缓解：私钥文件级保护（0600）+ 轮换——删除 `web-e2e-key` 重启即换新钥
///（已 pin 的浏览器会触发「身份变化」告警并要求重 pin，正是 TOFU 的设计
/// 行为）。
pub fn load_or_generate_e2e(path: &Path) -> Result<(XStaticSecret, XPublicKey), String> {
    let (seed, fresh) = if path.exists() {
        (load_seed(path)?, false)
    } else {
        (generate_seed_file(path)?, true)
    };
    tighten_permissions(path)?;
    let secret = XStaticSecret::from(seed);
    let public = XPublicKey::from(&secret);
    let fp = e2e_fingerprint(&public);
    if fresh {
        warn!(
            target: TARGET,
            key_path = %path.display(),
            fingerprint = %fp,
            "已生成网关 E2E X25519 密钥对（首次自动生成，仅此一次提示）：\
             指纹（SHA-256 前 16 hex）用于浏览器首连 TOFU 核对——与 SPA 指纹\
             确认弹层人工对照后再确认 pin"
        );
    } else {
        info!(
            target: TARGET,
            key_path = %path.display(),
            fingerprint = %fp,
            "网关 E2E X25519 密钥就绪（指纹 = SHA-256(公钥) 前 16 hex，\
             浏览器首连 TOFU 核对用）"
        );
    }
    Ok((secret, public))
}

/// E2E 公钥指纹：SHA-256(公钥) 前 16 **hex**（= 前 8 字节；协议 v1.4；gk
/// 公开可打印，取指纹纯为对照体验）。
pub fn e2e_fingerprint(public: &XPublicKey) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(&Sha256::digest(public.as_bytes())[..8])
}

// ---------------------------------------------------------------------------
// seed 文件公共助手（认证 / E2E 密钥同款纪律）
// ---------------------------------------------------------------------------

/// 首次生成 seed 文件（32B 随机 → 64-hex 单行，0600 `create_new` 原子写；
/// 并发竞态 → 回退加载）。失败文案含修复指引。
fn generate_seed_file(path: &Path) -> Result<[u8; 32], String> {
    let seed = rand::random::<[u8; 32]>();
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        #[cfg(unix)]
        let mut f = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(KEY_FILE_MODE)
                .open(path)?
        };
        #[cfg(not(unix))]
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        writeln!(f, "{}", hex::encode(seed))
    };
    match write() {
        Ok(()) => Ok(seed),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // 并发竞态（两个网关进程同时首启）：他人已生成，改走加载。
            debug!(target: TARGET, key_path = %path.display(), "seed 文件竞态，回退加载");
            load_seed(path)
        }
        Err(e) => Err(format!(
            "生成密钥文件 {} 失败：{e}（检查目录权限）",
            path.display()
        )),
    }
}

/// 加载 seed 文件：64-hex 单行 → 32B；畸形 → Err（不静默重建——重建会换钥
/// 导致对端公钥失配）。错误文案由调用方补充语境（认证 / E2E）。
fn load_seed(path: &Path) -> Result<[u8; 32], String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("读取密钥文件 {} 失败：{e}", path.display()))?;
    let seed = hex::decode(raw.trim()).map_err(|_| {
        format!(
            "密钥文件 {} 内容非法（须为 64 hex 私钥 seed 单行；删除本文件\
             并重启可自动重新生成，届时需把新公钥同步到对端）",
            path.display()
        )
    })?;
    seed.try_into().map_err(|v: Vec<u8>| {
        format!(
            "密钥文件 {} 的 seed 长度非法：{} hex（须恰为 64 hex = 32 字节）",
            path.display(),
            v.len() * 2
        )
    })
}

/// 权限过宽 → 收紧 0600 并 warn。非 unix 平台无 POSIX 权限位，跳过。
#[cfg(unix)]
fn tighten_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::metadata(path)
        .map_err(|e| format!("读取网关密钥文件 {} 元数据失败：{e}", path.display()))?
        .permissions();
    let mode = perms.mode() & 0o777;
    if mode != KEY_FILE_MODE {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(KEY_FILE_MODE))
            .map_err(|e| format!("收紧网关密钥文件 {} 权限为 0600 失败：{e}", path.display()))?;
        warn!(
            target: TARGET,
            key_path = %path.display(),
            old_mode = format!("{mode:o}"),
            "网关密钥文件权限过宽，已收紧为 0600（私钥 seed 仅本用户可读写）"
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn tighten_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    //! 密钥文件管理（unix 权限位 + 临时目录，真实文件系统操作）。

    use super::*;

    /// 每用例独立的临时密钥文件路径。
    fn temp_key_path(name: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "imagent-web-key-{}-{name}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// 首次生成：文件存在、内容 64-hex 单行、权限 0600；再次加载得到
    /// 同一密钥（同一公钥）。
    #[test]
    fn generate_then_load_same_key() {
        let path = temp_key_path("gen");
        let sk1 = load_or_generate(&path).expect("首次应生成");
        assert!(path.exists(), "密钥文件应落盘");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.trim().len(), 64, "64-hex seed 单行");
        assert!(raw.trim().chars().all(|c| c.is_ascii_hexdigit()));
        #[cfg(unix)]
        assert_eq!(mode_of(&path), 0o600, "生成即 0600");
        let sk2 = load_or_generate(&path).expect("二次应加载");
        assert_eq!(
            hex::encode(sk1.verifying_key().to_bytes()),
            hex::encode(sk2.verifying_key().to_bytes()),
            "同一文件加载出同一密钥"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 既有文件畸形（非 hex / 长度错）→ Err（提示修复，不静默重建——
    /// 重建会换钥导致 relay 侧公钥失配）。
    #[test]
    fn malformed_file_rejected() {
        let path = temp_key_path("bad-hex");
        std::fs::write(&path, "not-hex!").unwrap();
        let err = load_or_generate(&path).unwrap_err();
        assert!(
            err.contains("内容非法") && err.contains("重新生成"),
            "{err}"
        );
        let _ = std::fs::remove_file(&path);

        let path = temp_key_path("bad-len");
        std::fs::write(&path, "ab".repeat(16)).unwrap();
        let err = load_or_generate(&path).unwrap_err();
        assert!(err.contains("长度非法"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    /// 权限过宽（0644）→ 加载时收紧 0600（密钥仍可用）。
    #[cfg(unix)]
    #[test]
    fn wide_permissions_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_key_path("wide");
        let sk = load_or_generate(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(mode_of(&path), 0o644);
        let sk2 = load_or_generate(&path).unwrap();
        assert_eq!(mode_of(&path), 0o600, "加载时已收紧");
        assert_eq!(
            hex::encode(sk.verifying_key().to_bytes()),
            hex::encode(sk2.verifying_key().to_bytes())
        );
        let _ = std::fs::remove_file(&path);
    }

    /// E2E X25519 密钥：首启生成（0600、64-hex 单行）→ 重启加载同一密钥
    ///（公钥不变——浏览器 pin 的指纹才跨重启有效）；畸形文件 → Err。
    #[test]
    fn e2e_key_generate_then_load_same_pair() {
        let path = temp_key_path("e2e-gen");
        let (secret1, public1) = load_or_generate_e2e(&path).expect("首次应生成");
        assert!(path.exists(), "E2E 密钥文件应落盘");
        #[cfg(unix)]
        assert_eq!(mode_of(&path), 0o600, "生成即 0600");
        let (secret2, public2) = load_or_generate_e2e(&path).expect("二次应加载");
        assert_eq!(
            hex::encode(secret1.as_bytes()),
            hex::encode(secret2.as_bytes()),
            "同一文件加载出同一私钥"
        );
        assert_eq!(
            hex::encode(public1.as_bytes()),
            hex::encode(public2.as_bytes()),
            "公钥跨重启不变（指纹/pin 依赖）"
        );
        // 指纹形状：16 hex。
        let fp = e2e_fingerprint(&public1);
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()), "{fp}");
        let _ = std::fs::remove_file(&path);

        // 畸形文件 → Err（不静默重建换钥）。
        let path = temp_key_path("e2e-bad");
        std::fs::write(&path, "garbage").unwrap();
        assert!(load_or_generate_e2e(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    /// 装配决策：key 优先（文件可用即 Key，token 仅回退）；文件损坏 +
    /// token → Token 回退；文件损坏且无 token → Err（fail-closed）。
    #[test]
    fn load_gateway_credential_priority() {
        // 文件可用 → Key（即使 env token 也在）。
        let path = temp_key_path("cred-key");
        let c = load_gateway_credential(&path, Some("tok".into())).unwrap();
        assert!(matches!(c, GatewayCredential::Key(_)));
        let _ = std::fs::remove_file(&path);

        // 文件损坏 + token → Token 回退。
        let path = temp_key_path("cred-fallback");
        std::fs::write(&path, "garbage").unwrap();
        let c = load_gateway_credential(&path, Some("tok".into())).unwrap();
        assert!(matches!(c, GatewayCredential::Token(t) if t == "tok"));
        let _ = std::fs::remove_file(&path);

        // 文件损坏且无 token → Err（含修复指引）。
        let path = temp_key_path("cred-fail");
        std::fs::write(&path, "garbage").unwrap();
        let err = load_gateway_credential(&path, None).unwrap_err();
        assert!(err.contains("IMAGENT_WEB_GATEWAY_TOKEN"), "{err}");
        let _ = std::fs::remove_file(&path);

        // 无文件且无 token → 自动生成 Key（首启零配置）。
        let path = temp_key_path("cred-fresh");
        let c = load_gateway_credential(&path, None).unwrap();
        assert!(matches!(c, GatewayCredential::Key(_)));
        let _ = std::fs::remove_file(&path);
    }
}
