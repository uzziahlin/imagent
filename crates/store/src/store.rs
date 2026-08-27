use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use prometheus::{register_int_counter, IntCounter};

use crate::error::{Result, StoreError};
use crate::schema;

/// 凭据因 keyring 不可用而明文回退写入的次数（`require_keyring=false` 时）。
static CREDENTIAL_PLAINTEXT_FALLBACK: LazyLock<IntCounter> = LazyLock::new(|| {
    register_int_counter!(
        "imagent_credential_plaintext_fallback_total",
        "凭据因 keyring 不可用而明文回退写入的次数"
    )
    .expect("register credential_plaintext_fallback")
});

/// `require_keyring=true` 时 keyring 失败被 fail-closed 拒绝（不落盘）的次数。
static CREDENTIAL_KEYRING_REJECTED: LazyLock<IntCounter> = LazyLock::new(|| {
    register_int_counter!(
        "imagent_credential_keyring_rejected_total",
        "require_keyring=true 时 keyring 失败被拒绝（fail-closed）的次数"
    )
    .expect("register credential_keyring_rejected")
});

/// 一行 session 记录（core 用于会话路由）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionRow {
    pub conv_id: String,
    pub session_id: String,
    pub agent_kind: String,
    pub workdir: String,
    pub name: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 一行动态白名单记录（C1）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AllowedSenderRow {
    pub sender: String,
    pub added_at: i64,
    pub added_by: Option<String>,
    pub source: Option<String>,
}

/// 一行审计日志记录（C1）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditRow {
    pub id: i64,
    pub ts: i64,
    pub action: String,
    pub actor: Option<String>,
    pub target: Option<String>,
    pub detail: Option<String>,
}
/// 一行命名 session 记录（B1/B2：同一 conv 的多个命名 session）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct NamedSessionRow {
    pub conv_id: String,
    pub name: String,
    pub session_id: String,
    pub agent_kind: Option<String>,
    pub workdir: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 一行历史 session 记录（P4-8 `/resume`：该 conv 出现过的所有 session_id）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionHistoryRow {
    pub conv_id: String,
    pub session_id: String,
    pub agent_kind: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 一行 per-run 用量记录（schema v8，`/stats` 数据源）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunStatRow {
    pub id: i64,
    pub conv_id: String,
    pub agent_kind: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub ts: i64,
}

/// 在飞流式卡片登记行（schema v6）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct LiveCardRow {
    pub conv_id: String,
    pub platform: String,
    pub handle: String,
    pub updated_at: i64,
}

struct Inner {
    conn: Mutex<rusqlite::Connection>,
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
    /// 若为 true，`put_credential` 在 keyring 不可用时拒绝明文落盘（fail-closed）。
    /// 默认 false（headless 明文回退 + warn，向后兼容）。由 main 据 config 设置。
    require_keyring: Arc<AtomicBool>,
    /// keyring 用户名前缀段（P5：profile 隔离）。空 = 无 profile（username 保持
    /// `{platform}:{account}` 旧格式，存量部署零迁移）；非空 = `{scope}:{platform}:
    /// {account}`，读取时对旧键 fallback。由 main 按 `--profile` 设置。
    keyring_scope: Arc<parking_lot::RwLock<String>>,
    /// S3：应用层加密 passphrase（keyring 不可用时的加密回退）。`None` = 未设置。
    /// 优先取 `set_passphrase` 的显式值（main 从 config/env 注入），否则读
    /// 环境变量 `IMAGENT_PASSPHRASE`。测试环境忽略 env（并行测试共享进程 env，
    /// 避免互相污染），测试用 `set_passphrase` 显式注入。
    passphrase: Arc<parking_lot::RwLock<Option<String>>>,
}

impl Store {
    /// 打开（不存在则创建）数据库文件，建表迁移，并把文件权限收紧到 0600、
    /// 所在目录 0700（仅 unix）。开启 WAL。
    pub async fn open(path: &Path) -> Result<Self> {
        let path = path.to_path_buf();
        let inner = blocking_open(path).await?;
        Ok(Store {
            inner,
            require_keyring: Arc::new(AtomicBool::new(false)),
            keyring_scope: Arc::new(parking_lot::RwLock::new(String::new())),
            passphrase: Arc::new(parking_lot::RwLock::new(None)),
        })
    }

    /// 设置 keyring 用户名的 profile 前缀段（见字段注释；P5 profile 隔离）。
    pub fn set_keyring_scope(&self, scope: &str) {
        *self.keyring_scope.write() = scope.trim().to_string();
    }

    fn scope(&self) -> String {
        self.keyring_scope.read().clone()
    }

    /// 设置是否要求凭据必须入 keyring（true = keyring 不可用时拒绝明文落盘）。
    /// 由 main 启动时据 `config.require_keyring` 设置。
    pub fn set_require_keyring(&self, on: bool) {
        self.require_keyring.store(on, Ordering::Relaxed);
    }

    fn require_keyring(&self) -> bool {
        self.require_keyring.load(Ordering::Relaxed)
    }

    /// S3：设置应用层加密 passphrase（keyring 不可用时的加密回退）。
    /// 传入 `None` 清除（回退到读环境变量）。由 main 启动时注入。
    pub fn set_passphrase(&self, pass: Option<&str>) {
        // 空串与 None 等同处理（与 effective_passphrase 的 env 路径一致——
        // 环境变量为空串时也视为未配置）：防止把「显式设了空口令」误当成
        // 有效 passphrase 走加密回退（空口令派生的密钥无安全性）。
        *self.passphrase.write() = pass
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
    }

    /// 当前生效的 passphrase：显式 set 值优先，其次环境变量 `IMAGENT_PASSPHRASE`。
    /// 测试环境跳过 env（见字段注释）。
    fn effective_passphrase(&self) -> Option<String> {
        if let Some(p) = self.passphrase.read().clone() {
            return Some(p);
        }
        if cfg!(test) {
            return None;
        }
        std::env::var("IMAGENT_PASSPHRASE")
            .ok()
            .filter(|s| !s.is_empty())
    }

    // —— credentials ——

    /// 写凭据。优先写入 OS keyring（DESIGN §9.4）：成功则 SQLite `blob` 存
    /// marker `"keyring:{platform}:{account_id}"`（真值在 keyring）；无
    /// keychain（headless/CI）时 fallback 明文存 SQLite + warn。
    pub async fn put_credential(&self, platform: &str, account_id: &str, blob: &str) -> Result<()> {
        let (platform, account_id) = (platform.to_string(), account_id.to_string());
        // keychain I/O 经游离线程 + 超时，失败/超时回退明文（见 credentials 模块）。
        let scope = self.scope();
        let keyring_ok =
            crate::credentials::store_in_keyring(&scope, &platform, &account_id, blob).await;
        if !keyring_ok && self.require_keyring() {
            CREDENTIAL_KEYRING_REJECTED.inc();
            return Err(StoreError::Other(format!(
                "require_keyring=true 但 keyring 写入失败，拒绝明文落盘：{platform}:{account_id}\
                 （headless/CI 无 keychain 时请设 require_keyring=false 或配置 OS keyring）"
            )));
        }
        let stored_blob = if keyring_ok {
            crate::credentials::marker_for(&platform, &account_id)
        } else {
            // S3：keyring 不可用的回退形态——有 passphrase 则加密落盘，否则明文。
            match self.effective_passphrase() {
                Some(pass) => {
                    CREDENTIAL_PLAINTEXT_FALLBACK.inc();
                    let aad = credential_aad(&platform, &account_id);
                    let enc = encrypt_blocking(pass, blob.to_string(), aad).await?;
                    tracing::info!(
                        target: "store",
                        platform = %platform, account_id = %account_id,
                        "keyring 不可用，凭据已用 passphrase 加密落盘（enc:v2）"
                    );
                    enc
                }
                None => {
                    CREDENTIAL_PLAINTEXT_FALLBACK.inc();
                    // S3：明文落盘从 warn 升级为 error——headless 下 bot_token/secret
                    // 明文进 SQLite（及 WAL 副本）是真实泄漏面。仍不阻断（headless
                    // 兼容取舍：宁可带噪运行也不让登录路径直接失败），但必须把
                    // 风险与补救手段（IMAGENT_PASSPHRASE）喊到位。
                    tracing::error!(
                        target: "store",
                        platform = %platform, account_id = %account_id,
                        "keyring 不可用且未配置 passphrase，凭据将以明文写入 SQLite（含 WAL 副本）！\
                         任何能读该数据库文件的主体都能直接取得 bot_token/secret。\
                         补救：设置环境变量 IMAGENT_PASSPHRASE（应用层 AES-256-GCM 加密），\
                         或配置可用 OS keyring / 设 require_keyring=true 强制 fail-closed"
                    );
                    blob.to_string()
                }
            }
        };
        let inner = self.inner.clone();
        // 闭包需 'static（spawn_blocking），clone 一份供 DB 写入；account_id 本体保留给下方审计。
        let stored_encrypted = crate::crypto::is_encrypted(&stored_blob);
        let (plat_db, acct_db, blob_db) = (platform.clone(), account_id.clone(), stored_blob);
        let res = blocking_with_retry(inner, move |conn| {
            let now = now_secs();
            conn.execute(
                "INSERT INTO credentials (platform, account_id, blob, updated_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(platform, account_id) DO UPDATE SET blob = excluded.blob, updated_at = excluded.updated_at",
                rusqlite::params![plat_db, acct_db, blob_db, now],
            )?;
            Ok(())
        })
        .await;
        // P1-B：凭据写入审计（best-effort——失败只 warn，不影响凭据写入结果）。
        if res.is_ok() {
            let detail = if keyring_ok {
                "keyring"
            } else if stored_encrypted {
                // S3：加密回退形态留痕（与明文回退区分）。
                "encrypted-fallback"
            } else {
                "plaintext-fallback"
            };
            if let Err(e) = self
                .append_audit("credential_put", None, Some(&account_id), Some(detail))
                .await
            {
                tracing::warn!(
                    target: "store",
                    error = %e,
                    "凭据写入审计失败（best-effort，已忽略）"
                );
            }
        }
        res
    }

    /// 删除凭据（P2-10）：同步删 SQLite 行 + keyring 条目（若有）+ 审计。
    /// 用于凭据轮换/吊销的清理路径。返回是否删了 SQLite 行。
    pub async fn delete_credential(&self, platform: &str, account_id: &str) -> Result<bool> {
        let (platform, account_id) = (platform.to_string(), account_id.to_string());
        // best-effort 删 keyring（无条目/不可用静默）。scoped 与旧键都尝试，
        // 防止迁移中途的残留。
        let scope = self.scope();
        crate::credentials::delete_from_keyring(&scope, &platform, &account_id).await;
        if !scope.is_empty() {
            crate::credentials::delete_from_keyring("", &platform, &account_id).await;
        }
        let (p, a) = (platform.clone(), account_id.clone());
        let inner = self.inner.clone();
        let removed = blocking_with_retry(inner, move |conn| {
            let n = conn.execute(
                "DELETE FROM credentials WHERE platform = ?1 AND account_id = ?2",
                rusqlite::params![p, a],
            )?;
            Ok(n > 0)
        })
        .await?;
        if let Err(e) = self
            .append_audit(
                "credential_delete",
                None,
                Some(&account_id),
                Some(&platform),
            )
            .await
        {
            tracing::warn!(target: "store", error = %e, "凭据删除审计失败（best-effort）");
        }
        Ok(removed)
    }

    /// 读凭据。SQLite `blob` 为 marker 时从 keyring 取真值；为明文（旧库 /
    /// 无 keychain）时尝试懒迁移到 keyring（成功则把 DB blob 更新为 marker，
    /// 失败则保持明文）。返回值始终为真实 blob。
    pub async fn get_credential(&self, platform: &str, account_id: &str) -> Result<Option<String>> {
        let (platform, account_id) = (platform.to_string(), account_id.to_string());
        let (q_platform, q_account) = (platform.clone(), account_id.clone());
        let inner = self.inner.clone();
        let row = blocking_with(inner, move |conn| {
            let mut stmt = conn
                .prepare("SELECT blob FROM credentials WHERE platform = ?1 AND account_id = ?2")?;
            let mut rows = stmt.query(rusqlite::params![q_platform, q_account])?;
            Ok(rows.next()?.map(|r| r.get::<_, String>(0)).transpose()?)
        })
        .await?;

        match row {
            None => Ok(None),
            Some(raw_blob) => {
                let resolved = self
                    .resolve_credential_blob(&raw_blob, &platform, &account_id)
                    .await?;
                Ok(Some(resolved))
            }
        }
    }

    /// 取该 platform 的第一条凭据 (account_id, blob)。P1 单账号。blob 经
    pub async fn first_credential(&self, platform: &str) -> Result<Option<(String, String)>> {
        let platform = platform.to_string();
        let q_platform = platform.clone();
        let inner = self.inner.clone();
        let row = blocking_with(inner, move |conn| {
            let mut stmt = conn
                .prepare("SELECT account_id, blob FROM credentials WHERE platform = ?1 ORDER BY account_id LIMIT 1")?;
            let mut rows = stmt.query(rusqlite::params![q_platform])?;
            rows.next()?
                .map(|r| Ok::<_, StoreError>((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .transpose()
        })
        .await?;

        match row {
            None => Ok(None),
            Some((account_id, raw_blob)) => {
                let resolved = self
                    .resolve_credential_blob(&raw_blob, &platform, &account_id)
                    .await?;
                Ok(Some((account_id, resolved)))
            }
        }
    }

    /// 把 DB 中读出的原始 blob 解析为真实凭据（S3：三种形态）：
    /// - `keyring:` marker → 从 keyring 取真值；marker 在但 keyring 读不到（keychain 被清）→ 报错；
    /// - `enc:v1:` / `enc:v2:`（passphrase 加密）→ 解密返回（AAD 绑定
    ///   `platform:account_id`，v1 旧格式无 AAD 兼容读取）；passphrase 缺失 /
    ///   解密失败 → 可读错误（提示设置 IMAGENT_PASSPHRASE）；
    /// - 裸明文（旧库 / 无 keychain）→ 尝试懒迁移：先 keyring，其次（配置了 passphrase 且
    ///   keyring 不可用时）重写为加密形态；都失败则保持明文。无论迁移结果都返回该明文。
    ///
    /// 迁移为 compare-and-swap（`WHERE blob = 读到的明文`）：并发写新凭据时
    /// CAS 失败即放弃迁移（DB 已是新值，不覆盖）；迁移回写失败也不让读路径
    /// 报错——降级返回手上的明文（warn），与「迁移是 best-effort 优化」语义一致。
    async fn resolve_credential_blob(
        &self,
        raw_blob: &str,
        platform: &str,
        account_id: &str,
    ) -> Result<String> {
        if crate::credentials::is_keyring_marker(raw_blob) {
            let scope = self.scope();
            match crate::credentials::load_from_keyring(&scope, platform, account_id).await {
                Some(real) => Ok(real),
                None => Err(StoreError::Other(format!(
                    "凭据标记表明真值在 keyring，但读取失败（可能 keychain 被清）：\
                     {platform}:{account_id}"
                ))),
            }
        } else if crate::crypto::is_encrypted(raw_blob) {
            // S3：加密形态。passphrase 是解密前提，缺失/错误都给出面向运维的提示。
            let pass = self.effective_passphrase().ok_or_else(|| {
                StoreError::Other(format!(
                    "凭据以加密形态（enc:v1/v2）存储，但未配置 passphrase：{platform}:{account_id}\
                     （请设置环境变量 IMAGENT_PASSPHRASE 为写入时使用的口令）"
                ))
            })?;
            let aad = credential_aad(platform, account_id);
            decrypt_blocking(pass, raw_blob.to_string(), aad).await
        } else {
            // 明文：懒迁移——优先 keyring。迁移回写一律 CAS（见函数注释）。
            let scope = self.scope();
            if crate::credentials::store_in_keyring(&scope, platform, account_id, raw_blob).await {
                let marker = crate::credentials::marker_for(platform, account_id);
                match self
                    .try_migrate_credential_blob(platform, account_id, raw_blob, &marker)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        // CAS 失败：另一写者已更新 DB（如并发 put 新凭据），放弃迁移。
                        tracing::info!(
                            target: "store",
                            platform = %platform, account_id = %account_id,
                            "明文凭据懒迁移 CAS 失败（另一写者已更新 blob），放弃迁移"
                        );
                    }
                    Err(e) => {
                        // keyring 迁移成功但 marker 回写失败：降级返回明文（而非报错）——
                        // 手上的明文仍有效，下次读取会重试迁移。
                        tracing::warn!(
                            target: "store",
                            platform = %platform, account_id = %account_id, error = %e,
                            "keyring 迁移成功但 marker 回写失败，降级返回明文（下次读取重试）"
                        );
                    }
                }
            } else if let Some(pass) = self.effective_passphrase() {
                // keyring 不可用但配置了 passphrase → 惰性重写为加密形态（S3 迁移路径）。
                let aad = credential_aad(platform, account_id);
                let enc = encrypt_blocking(pass, raw_blob.to_string(), aad).await?;
                match self
                    .try_migrate_credential_blob(platform, account_id, raw_blob, &enc)
                    .await
                {
                    Ok(true) => {
                        tracing::info!(
                            target: "store",
                            platform = %platform, account_id = %account_id,
                            "历史明文凭据已惰性迁移为加密形态（enc:v2）"
                        );
                    }
                    Ok(false) => {
                        tracing::info!(
                            target: "store",
                            platform = %platform, account_id = %account_id,
                            "明文凭据惰性加密迁移 CAS 失败（另一写者已更新 blob），放弃迁移"
                        );
                    }
                    Err(e) => {
                        // 与 keyring 路径同语义：回写失败降级返回明文，不阻断读。
                        tracing::warn!(
                            target: "store",
                            platform = %platform, account_id = %account_id, error = %e,
                            "惰性加密迁移回写失败，降级返回明文（下次读取重试）"
                        );
                    }
                }
            } else {
                // 读取路径：历史明文凭据未能迁移（无 keyring 也无 passphrase），计数但不
                // fail-closed（否则历史明文凭据将不可读，破坏可用性；fail-closed 仅作用
                // 于写入）。
                CREDENTIAL_PLAINTEXT_FALLBACK.inc();
            }
            Ok(raw_blob.to_string())
        }
    }

    /// CAS 式迁移回写：仅当 DB 中 blob 仍等于读到的旧明文（`expected_old`）时
    /// 才更新为 `new_blob`，返回是否生效。
    ///
    /// 背景（lost-update）：读明文与回写之间无并发保护，另一写者（如并发
    /// put_credential）写入的新凭据会被旧值覆盖。`WHERE blob = ?old` 使回写
    /// 成为 compare-and-swap：影响行数为 0 = 另一写者已更新 → 放弃迁移。
    async fn try_migrate_credential_blob(
        &self,
        platform: &str,
        account_id: &str,
        expected_old: &str,
        new_blob: &str,
    ) -> Result<bool> {
        let (platform, account_id, expected_old, new_blob) = (
            platform.to_string(),
            account_id.to_string(),
            expected_old.to_string(),
            new_blob.to_string(),
        );
        let platform_for_audit = platform.clone();
        let account_for_audit = account_id.clone();
        let inner = self.inner.clone();
        let applied = blocking_with_retry(inner, move |conn| {
            let now = now_secs();
            // CAS：blob 仍为读到的旧值才更新，防并发写新凭据被旧值覆盖。
            let n = conn.execute(
                "UPDATE credentials SET blob = ?3, updated_at = ?4 \
                 WHERE platform = ?1 AND account_id = ?2 AND blob = ?5",
                rusqlite::params![platform, account_id, new_blob, now, expected_old],
            )?;
            Ok(n > 0)
        })
        .await?;
        // P2-11：迁移成功审计（明文 → keyring marker / enc 的形态变更留痕，绕过
        // P1-B 的 put_credential 审计路径；best-effort，仅在实际生效时记录）。
        if applied {
            let _ = self
                .append_audit(
                    "credential_migrated",
                    None,
                    Some(&account_for_audit),
                    Some(&format!("platform={platform_for_audit}")),
                )
                .await;
        }
        Ok(applied)
    }

    // —— sessions（core 用）——

    pub async fn get_session(&self, conv_id: &str) -> Result<Option<SessionRow>> {
        let conv_id = conv_id.to_string();
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt = conn.prepare(
                "SELECT conv_id, session_id, agent_kind, workdir, name, created_at, updated_at \
                 FROM sessions WHERE conv_id = ?1",
            )?;
            let mut rows = stmt.query(rusqlite::params![conv_id])?;
            match rows.next()? {
                None => Ok(None),
                Some(r) => Ok(Some(SessionRow {
                    conv_id: r.get(0)?,
                    session_id: r.get(1)?,
                    agent_kind: r.get(2)?,
                    workdir: r.get(3)?,
                    // name 列可为 NULL（DDL 为 TEXT 可空）
                    name: r.get::<_, Option<String>>(4)?,
                    created_at: r.get(5)?,
                    updated_at: r.get(6)?,
                })),
            }
        })
        .await
    }

    /// 插入或更新（按 conv_id）。created_at 仅新建时写入；更新时保留原 created_at、刷新 updated_at。
    ///
    /// P4-8：session_id 发生变化时同步写 `session_history` 侧表（`/resume` 数据源）——
    /// INSERT OR REPLACE 刷新 updated_at，同 session 重复 upsert 不产生新行。
    pub async fn upsert_session(&self, row: &SessionRow) -> Result<()> {
        let row = row.clone();
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            let now = now_secs();
            // P5-store：主表 + 历史侧表同事务——此前两条语句各自 autocommit，中间
            // 崩溃会漏历史行（/resume 丢会话），且每轮两次独立 fsync。
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO sessions (conv_id, session_id, agent_kind, workdir, name, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(conv_id) DO UPDATE SET \
                   session_id = excluded.session_id, \
                   agent_kind = excluded.agent_kind, \
                   workdir    = excluded.workdir, \
                   name       = excluded.name, \
                   updated_at = excluded.updated_at",
                // 更新分支：created_at 不在 SET 列中 → 保留原值。
                rusqlite::params![
                    row.conv_id,
                    row.session_id,
                    row.agent_kind,
                    row.workdir,
                    row.name,
                    row.created_at,
                    now,
                ],
            )?;
            tx.execute(
                "INSERT INTO session_history (conv_id, session_id, agent_kind, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(conv_id, session_id) DO UPDATE SET updated_at = excluded.updated_at",
                rusqlite::params![row.conv_id, row.session_id, row.agent_kind, now, now],
            )?;
            // P5-store：session_history per-conv 轮转——保留最近 50 条（调用方
            // list_session_history 上限 50；此前只增不删，长生命周期部署无限增长）。
            tx.execute(
                "DELETE FROM session_history WHERE conv_id = ?1 AND session_id NOT IN \
                 (SELECT session_id FROM session_history WHERE conv_id = ?1 \
                  ORDER BY updated_at DESC, rowid DESC LIMIT 50)",
                rusqlite::params![row.conv_id],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// 删除该 conv 的 session 行（core 的 /new 命令用）。
    pub async fn delete_session(&self, conv_id: &str) -> Result<()> {
        let conv_id = conv_id.to_string();
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            conn.execute(
                "DELETE FROM sessions WHERE conv_id = ?1",
                rusqlite::params![conv_id],
            )?;
            Ok(())
        })
        .await
    }

    // —— sync_buf（ilink 长轮询游标）——

    pub async fn get_sync_buf(&self, platform: &str, account_id: &str) -> Result<Option<String>> {
        let (platform, account_id) = (platform.to_string(), account_id.to_string());
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt =
                conn.prepare("SELECT buf FROM sync_buf WHERE platform = ?1 AND account_id = ?2")?;
            let mut rows = stmt.query(rusqlite::params![platform, account_id])?;
            Ok(rows.next()?.map(|r| r.get::<_, String>(0)).transpose()?)
        })
        .await
    }

    pub async fn set_sync_buf(&self, platform: &str, account_id: &str, buf: &str) -> Result<()> {
        let (platform, account_id, buf) = (
            platform.to_string(),
            account_id.to_string(),
            buf.to_string(),
        );
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            conn.execute(
                "INSERT INTO sync_buf (platform, account_id, buf) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(platform, account_id) DO UPDATE SET buf = excluded.buf",
                rusqlite::params![platform, account_id, buf],
            )?;
            Ok(())
        })
        .await
    }

    // —— context_tokens（ilink 出站回传）——

    pub async fn get_context_token(
        &self,
        platform: &str,
        account_id: &str,
        peer: &str,
    ) -> Result<Option<String>> {
        let (platform, account_id, peer) = (
            platform.to_string(),
            account_id.to_string(),
            peer.to_string(),
        );
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt = conn.prepare(
                "SELECT token FROM context_tokens WHERE platform = ?1 AND account_id = ?2 AND peer = ?3",
            )?;
            let mut rows = stmt.query(rusqlite::params![platform, account_id, peer])?;
            Ok(rows.next()?.map(|r| r.get::<_, String>(0)).transpose()?)
        })
        .await
    }

    pub async fn set_context_token(
        &self,
        platform: &str,
        account_id: &str,
        peer: &str,
        token: &str,
    ) -> Result<()> {
        let (platform, account_id, peer, token) = (
            platform.to_string(),
            account_id.to_string(),
            peer.to_string(),
            token.to_string(),
        );
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            let now = now_secs();
            conn.execute(
                "INSERT INTO context_tokens (platform, account_id, peer, token, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(platform, account_id, peer) DO UPDATE SET \
                   token = excluded.token, updated_at = excluded.updated_at",
                rusqlite::params![platform, account_id, peer, token, now],
            )?;
            Ok(())
        })
        .await
    }

    // —— allowed_senders（动态白名单，C1）——

    /// 返回所有已授权 sender（按 id 升序）。
    pub async fn list_allowed_senders(&self) -> Result<Vec<String>> {
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt = conn.prepare("SELECT sender FROM allowed_senders ORDER BY sender")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
        .await
    }

    /// 返回所有已授权 sender 的完整行（含元数据，按 sender 升序）。
    pub async fn list_allowed_senders_detailed(&self) -> Result<Vec<AllowedSenderRow>> {
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt = conn.prepare(
                "SELECT sender, added_at, added_by, source FROM allowed_senders ORDER BY sender",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(AllowedSenderRow {
                    sender: r.get(0)?,
                    added_at: r.get(1)?,
                    added_by: r.get::<_, Option<String>>(2)?,
                    source: r.get::<_, Option<String>>(3)?,
                })
            })?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
        .await
    }

    /// 加入白名单。`INSERT OR IGNORE`：已存在不报错、不覆盖原 added_at/by/source。
    pub async fn add_allowed_sender(
        &self,
        sender: &str,
        added_by: Option<&str>,
        source: Option<&str>,
    ) -> Result<()> {
        let (sender, added_by, source) = (
            sender.to_string(),
            added_by.map(|s| s.to_string()),
            source.map(|s| s.to_string()),
        );
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            let now = now_secs();
            conn.execute(
                "INSERT OR IGNORE INTO allowed_senders (sender, added_at, added_by, source) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![sender, now, added_by, source],
            )?;
            Ok(())
        })
        .await
    }

    /// 移除白名单条目。返回是否原本存在。
    pub async fn remove_allowed_sender(&self, sender: &str) -> Result<bool> {
        let sender = sender.to_string();
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            let n = conn.execute(
                "DELETE FROM allowed_senders WHERE sender = ?1",
                rusqlite::params![sender],
            )?;
            Ok(n > 0)
        })
        .await
    }

    // —— admin_senders（管理员动态白名单，P7-A1 `/admin add|remove`）——

    /// 返回全部动态管理员（升序；与 config `admin_senders` 种子取并集用）。
    pub async fn list_admin_senders(&self) -> Result<Vec<String>> {
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt = conn.prepare("SELECT sender FROM admin_senders ORDER BY sender")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
        .await
    }

    /// 加入管理员。`INSERT OR IGNORE`：已存在不报错、不覆盖原元数据。
    pub async fn add_admin_sender(
        &self,
        sender: &str,
        added_by: Option<&str>,
        source: Option<&str>,
    ) -> Result<()> {
        let (sender, added_by, source) = (
            sender.to_string(),
            added_by.map(|s| s.to_string()),
            source.map(|s| s.to_string()),
        );
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            let now = now_secs();
            conn.execute(
                "INSERT OR IGNORE INTO admin_senders (sender, added_at, added_by, source) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![sender, now, added_by, source],
            )?;
            Ok(())
        })
        .await
    }

    /// 移除管理员条目。返回是否原本存在。
    pub async fn remove_admin_sender(&self, sender: &str) -> Result<bool> {
        let sender = sender.to_string();
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            let n = conn.execute(
                "DELETE FROM admin_senders WHERE sender = ?1",
                rusqlite::params![sender],
            )?;
            Ok(n > 0)
        })
        .await
    }

    // —— allowed_chats（会话/群白名单，P4-5）——

    /// 返回所有已授权会话 conv_id（升序）。
    pub async fn list_allowed_chats(&self) -> Result<Vec<String>> {
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt = conn.prepare("SELECT conv_id FROM allowed_chats ORDER BY conv_id")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
        .await
    }

    /// 加入会话白名单。`INSERT OR IGNORE`：已存在不报错、不覆盖元数据。
    pub async fn add_allowed_chat(
        &self,
        conv_id: &str,
        added_by: Option<&str>,
        source: Option<&str>,
    ) -> Result<()> {
        let (conv_id, added_by, source) = (
            conv_id.to_string(),
            added_by.map(|s| s.to_string()),
            source.map(|s| s.to_string()),
        );
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO allowed_chats (conv_id, added_at, added_by, source) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![conv_id, now_secs(), added_by, source],
            )?;
            Ok(())
        })
        .await
    }

    /// 移除会话白名单条目。返回是否原本存在。
    pub async fn remove_allowed_chat(&self, conv_id: &str) -> Result<bool> {
        let conv_id = conv_id.to_string();
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            let n = conn.execute(
                "DELETE FROM allowed_chats WHERE conv_id = ?1",
                rusqlite::params![conv_id],
            )?;
            Ok(n > 0)
        })
        .await
    }

    // —— session_history（历史会话侧表，P4-8 /resume 数据源）——

    /// 列出该 conv 的历史 session（按 updated_at 倒序，最多 `limit` 条）。
    pub async fn list_session_history(
        &self,
        conv_id: &str,
        limit: usize,
    ) -> Result<Vec<SessionHistoryRow>> {
        let conv_id = conv_id.to_string();
        let limit_i = i64::try_from(limit).unwrap_or(i64::MAX);
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt = conn.prepare(
                "SELECT conv_id, session_id, agent_kind, created_at, updated_at \
                 FROM session_history WHERE conv_id = ?1 ORDER BY updated_at DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![conv_id, limit_i], |r| {
                Ok(SessionHistoryRow {
                    conv_id: r.get(0)?,
                    session_id: r.get(1)?,
                    agent_kind: r.get::<_, Option<String>>(2)?,
                    created_at: r.get(3)?,
                    updated_at: r.get(4)?,
                })
            })?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
        .await
    }

    // —— live_cards（在飞流式卡片登记，P4_ROADMAP 第六批孤儿卡片关流）——

    /// 登记/刷新一张在飞流式卡片（每 conv 至多一张，upsert 覆盖）。
    pub async fn record_live_card(
        &self,
        conv_id: &str,
        platform: &str,
        handle: &str,
    ) -> Result<()> {
        let (conv_id, platform, handle) = (
            conv_id.to_string(),
            platform.to_string(),
            handle.to_string(),
        );
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            conn.execute(
                "INSERT INTO live_cards (conv_id, platform, handle, updated_at) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(conv_id) DO UPDATE SET platform = ?2, handle = ?3, updated_at = ?4",
                rusqlite::params![conv_id, platform, handle, now_secs()],
            )?;
            Ok(())
        })
        .await
    }

    /// 摘除该 conv 的在飞卡片登记（终态 patch 成功后调用）。
    pub async fn clear_live_card(&self, conv_id: &str) -> Result<()> {
        let conv_id = conv_id.to_string();
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            conn.execute(
                "DELETE FROM live_cards WHERE conv_id = ?1",
                rusqlite::params![conv_id],
            )?;
            Ok(())
        })
        .await
    }

    /// 列出全部在飞卡片登记（启动扫描用）。
    pub async fn list_live_cards(&self) -> Result<Vec<LiveCardRow>> {
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt = conn.prepare(
                "SELECT conv_id, platform, handle, updated_at FROM live_cards ORDER BY updated_at",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(LiveCardRow {
                    conv_id: r.get(0)?,
                    platform: r.get(1)?,
                    handle: r.get(2)?,
                    updated_at: r.get(3)?,
                })
            })?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
        .await
    }

    // —— audit_log（审计日志，C1）——

    /// 追加一条审计记录。
    pub async fn append_audit(
        &self,
        action: &str,
        actor: Option<&str>,
        target: Option<&str>,
        detail: Option<&str>,
    ) -> Result<()> {
        let (action, actor, target, detail) = (
            action.to_string(),
            actor.map(|s| s.to_string()),
            target.map(|s| s.to_string()),
            detail.map(|s| s.to_string()),
        );
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            let now = now_secs();
            conn.execute(
                "INSERT INTO audit_log (ts, action, actor, target, detail) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![now, action, actor, target, detail],
            )?;
            // P2-R：审计日志轮转——保留最近 10000 条。用 max(id) 范围删除（索引高效），
            // 替代原 `NOT IN (SELECT ... LIMIT 10000)` 子查询（每条 O(N) 全扫）。
            conn.execute(
                "DELETE FROM audit_log WHERE id <= (SELECT MAX(id) FROM audit_log) - 10000",
                [],
            )?;
            Ok(())
        })
        .await
    }

    /// 返回最近 `limit` 条审计记录（按 id 倒序）。
    pub async fn list_audit(&self, limit: usize) -> Result<Vec<AuditRow>> {
        let limit_i = i64::try_from(limit).unwrap_or(i64::MAX);
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, ts, action, actor, target, detail FROM audit_log \
                 ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(rusqlite::params![limit_i], |r| {
                Ok(AuditRow {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    action: r.get(2)?,
                    actor: r.get::<_, Option<String>>(3)?,
                    target: r.get::<_, Option<String>>(4)?,
                    detail: r.get::<_, Option<String>>(5)?,
                })
            })?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
        .await
    }
    // —— run_stats（per-run 用量记录，schema v8 /stats 数据源）——

    /// 追加一条 per-run 用量记录（append-only；轮转保留最近 10000 条，参照
    /// audit_log 的 max(id) 范围删除）。`usage` 各字段由调用方从 RunOutcome 展平；
    /// 失败轮次（无 usage）也记一行（tokens 为 0），保证轮次数统计完整。
    pub async fn append_run_stat(
        &self,
        conv_id: &str,
        agent_kind: Option<&str>,
        input_tokens: i64,
        output_tokens: i64,
        cached_tokens: Option<i64>,
        cost_usd: Option<f64>,
    ) -> Result<()> {
        let (conv_id, agent_kind) = (conv_id.to_string(), agent_kind.map(|s| s.to_string()));
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            conn.execute(
                "INSERT INTO run_stats                    (conv_id, agent_kind, input_tokens, output_tokens, cached_tokens, cost_usd, ts)                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    conv_id,
                    agent_kind,
                    input_tokens,
                    output_tokens,
                    cached_tokens,
                    cost_usd,
                    now_secs(),
                ],
            )?;
            // 轮转：保留最近 10000 条（同 audit_log 的 P2-R 手法）。
            conn.execute(
                "DELETE FROM run_stats WHERE id <= (SELECT MAX(id) FROM run_stats) - 10000",
                [],
            )?;
            Ok(())
        })
        .await
    }

    /// 列出 `since`（epoch 秒）之后的全部用量记录（按 id 升序）。/stats 聚合用。
    pub async fn list_run_stats_since(&self, since: i64) -> Result<Vec<RunStatRow>> {
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, conv_id, agent_kind, input_tokens, output_tokens, cached_tokens, cost_usd, ts                  FROM run_stats WHERE ts >= ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map(rusqlite::params![since], |r| {
                Ok(RunStatRow {
                    id: r.get(0)?,
                    conv_id: r.get(1)?,
                    agent_kind: r.get::<_, Option<String>>(2)?,
                    input_tokens: r.get(3)?,
                    output_tokens: r.get(4)?,
                    cached_tokens: r.get::<_, Option<i64>>(5)?,
                    cost_usd: r.get::<_, Option<f64>>(6)?,
                    ts: r.get(7)?,
                })
            })?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
        .await
    }

    // —— config KV（B1：active_name:<conv_id> 等通用键值）——

    pub async fn get_config(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_string();
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt = conn.prepare("SELECT value FROM config WHERE key = ?1")?;
            let mut rows = stmt.query(rusqlite::params![key])?;
            Ok(rows.next()?.map(|r| r.get::<_, String>(0)).transpose()?)
        })
        .await
    }

    pub async fn set_config(&self, key: &str, value: &str) -> Result<()> {
        let (key, value) = (key.to_string(), value.to_string());
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            conn.execute(
                "INSERT INTO config (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![key, value],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn delete_config(&self, key: &str) -> Result<()> {
        let key = key.to_string();
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            conn.execute("DELETE FROM config WHERE key = ?1", rusqlite::params![key])?;
            Ok(())
        })
        .await
    }

    /// 列出所有以 `prefix` 开头的 config KV（key 升序）。/ws list 用（prefix="workspace:"）。
    pub async fn list_config(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let prefix = prefix.to_string();
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt =
                conn.prepare("SELECT key, value FROM config WHERE key LIKE ?1 ORDER BY key")?;
            let like = format!("{prefix}%");
            let rows = stmt.query_map(rusqlite::params![like], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
        .await
    }

    // —— named_sessions（B1/B2：命名 session 侧表）——

    /// 插入或更新（按 conv_id + name）。created_at 仅新建时写入；更新时保留原 created_at、刷新 updated_at。
    pub async fn upsert_named_session(&self, row: &NamedSessionRow) -> Result<()> {
        let row = row.clone();
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            let now = now_secs();
            conn.execute(
                "INSERT INTO named_sessions \
                   (conv_id, name, session_id, agent_kind, workdir, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(conv_id, name) DO UPDATE SET \
                   session_id = excluded.session_id, \
                   agent_kind = excluded.agent_kind, \
                   workdir    = excluded.workdir, \
                   updated_at = excluded.updated_at",
                // 更新分支：created_at 不在 SET 列中 → 保留原值。
                rusqlite::params![
                    row.conv_id,
                    row.name,
                    row.session_id,
                    row.agent_kind,
                    row.workdir,
                    row.created_at,
                    now,
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_named_session(
        &self,
        conv_id: &str,
        name: &str,
    ) -> Result<Option<NamedSessionRow>> {
        let (conv_id, name) = (conv_id.to_string(), name.to_string());
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt = conn.prepare(
                "SELECT conv_id, name, session_id, agent_kind, workdir, created_at, updated_at \
                 FROM named_sessions WHERE conv_id = ?1 AND name = ?2",
            )?;
            let mut rows = stmt.query(rusqlite::params![conv_id, name])?;
            match rows.next()? {
                None => Ok(None),
                Some(r) => Ok(Some(NamedSessionRow {
                    conv_id: r.get(0)?,
                    name: r.get(1)?,
                    session_id: r.get(2)?,
                    agent_kind: r.get::<_, Option<String>>(3)?,
                    workdir: r.get::<_, Option<String>>(4)?,
                    created_at: r.get(5)?,
                    updated_at: r.get(6)?,
                })),
            }
        })
        .await
    }

    /// 列出该 conv 的所有命名 session（按 updated_at 倒序）。
    pub async fn list_named_sessions(&self, conv_id: &str) -> Result<Vec<NamedSessionRow>> {
        let conv_id = conv_id.to_string();
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt = conn.prepare(
                "SELECT conv_id, name, session_id, agent_kind, workdir, created_at, updated_at \
                 FROM named_sessions WHERE conv_id = ?1 ORDER BY updated_at DESC",
            )?;
            let rows = stmt.query_map(rusqlite::params![conv_id], |r| {
                Ok(NamedSessionRow {
                    conv_id: r.get(0)?,
                    name: r.get(1)?,
                    session_id: r.get(2)?,
                    agent_kind: r.get::<_, Option<String>>(3)?,
                    workdir: r.get::<_, Option<String>>(4)?,
                    created_at: r.get(5)?,
                    updated_at: r.get(6)?,
                })
            })?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
        .await
    }

    /// 当前活动 session 数（`sessions` 表行数）。供 `/health` 报告。
    pub async fn count_sessions(&self) -> Result<i64> {
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let n: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
            Ok(n)
        })
        .await
    }

    pub async fn delete_named_session(&self, conv_id: &str, name: &str) -> Result<()> {
        let (conv_id, name) = (conv_id.to_string(), name.to_string());
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            conn.execute(
                "DELETE FROM named_sessions WHERE conv_id = ?1 AND name = ?2",
                rusqlite::params![conv_id, name],
            )?;
            Ok(())
        })
        .await
    }

    /// 【A1】原子化 `/switch`：单事务完成命名切换的全部 DB 写入，替代
    /// dispatch/commands/session.rs 中「upsert/delete_session + set_config」
    /// 的多次独立 autocommit（中间崩溃会留下 active_name 指向旧 session 等
    /// 不一致状态）。
    ///
    /// 单事务内容：
    /// 1. `activate = Some(row)`：把该命名 session 写成活动 session（续接用，
    ///    同时刷新 session_history，与 upsert_session 同构）；`None`：删除当前
    ///    活动 session 行（新命名 session，下一条消息再新建）；
    /// 2. `config[active_name:<conv>] = name`；
    /// 3. 删除 `config[compact_summary:<conv>]`（切换后旧会话的压缩摘要
    ///    不应再注入新会话）。
    ///
    /// 注：core（dispatch/commands/session.rs 的 `/switch`）尚未接线——本 API
    /// 在 store 层就绪，core 侧接入点为 cmd_switch 的两个分支（Some 分支用
    /// `activate=Some(&sr)`；None 分支用 `activate=None`），待 Wave3/后续接线。
    pub async fn switch_named_session(
        &self,
        conv_id: &str,
        name: &str,
        activate: Option<&SessionRow>,
    ) -> Result<()> {
        let conv_id = conv_id.to_string();
        let name = name.to_string();
        let activate = activate.cloned();
        let inner = self.inner.clone();
        blocking_with_retry(inner, move |conn| {
            let now = now_secs();
            let tx = conn.unchecked_transaction()?;
            match &activate {
                Some(row) => {
                    tx.execute(
                        "INSERT INTO sessions (conv_id, session_id, agent_kind, workdir, name, created_at, updated_at) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                         ON CONFLICT(conv_id) DO UPDATE SET \
                           session_id = excluded.session_id, \
                           agent_kind = excluded.agent_kind, \
                           workdir    = excluded.workdir, \
                           name       = excluded.name, \
                           updated_at = excluded.updated_at",
                        rusqlite::params![
                            row.conv_id,
                            row.session_id,
                            row.agent_kind,
                            row.workdir,
                            row.name,
                            row.created_at,
                            now,
                        ],
                    )?;
                    tx.execute(
                        "INSERT INTO session_history (conv_id, session_id, agent_kind, created_at, updated_at) \
                         VALUES (?1, ?2, ?3, ?4, ?5) \
                         ON CONFLICT(conv_id, session_id) DO UPDATE SET updated_at = excluded.updated_at",
                        rusqlite::params![row.conv_id, row.session_id, row.agent_kind, now, now],
                    )?;
                }
                None => {
                    // 新命名 session：清活动 session（下次新建）。
                    tx.execute("DELETE FROM sessions WHERE conv_id = ?1", rusqlite::params![conv_id])?;
                }
            }
            tx.execute(
                "INSERT INTO config (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![format!("active_name:{conv_id}"), name],
            )?;
            // 切换后旧摘要不再注入新会话。
            tx.execute(
                "DELETE FROM config WHERE key = ?1",
                rusqlite::params![format!("compact_summary:{conv_id}")],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }
}

// —— spawn_blocking 调度 ——

/// `open` 专用：从路径打开连接、建表迁移、收紧权限，返回 Inner。
/// 连接独占于此 blocking 线程，随后由 Inner 的 Mutex 保护。
async fn blocking_open(path: PathBuf) -> Result<Arc<Inner>> {
    let join = tokio::task::spawn_blocking(move || {
        let conn = match open_and_setup(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "store open failed");
                return Err(e);
            }
        };
        Ok(Arc::new(Inner {
            conn: Mutex::new(conn),
        }))
    })
    .await
    .map_err(|e| StoreError::Other(format!("spawn_blocking join: {e}")))?;
    join
}

/// 在 blocking 线程内取共享连接锁执行闭包。锁 guard 不跨 `.await`。
async fn blocking_with<F, T>(inner: Arc<Inner>, f: F) -> Result<T>
where
    F: FnOnce(&rusqlite::Connection) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let join = tokio::task::spawn_blocking(move || {
        let conn = inner.conn.lock();
        f(&conn)
    })
    .await
    .map_err(|e| StoreError::Other(format!("spawn_blocking join: {e}")))?;
    join
}

/// 同 `blocking_with`，但闭包要求 `Fn`（可重放）：执行中遇到 SQLITE_BUSY /
/// SQLITE_LOCKED 时指数退避重试（50ms 起、×2、上限 2s、最多 5 次尝试），
/// 重试耗尽仍失败才把错误返回。读路径不重试（WAL 下读不阻塞），仅写路径用。
///
/// P4（v7 review）：多连接（core store + ilink store + /health 各自 open）高并发
/// 写时 busy_timeout=5s 之后仍可能 BUSY；退避重试让瞬时竞争自愈，而非把
/// SQLITE_BUSY 冒泡成登录/会话写入失败。
///
/// 实现要点（两项修复）：
/// 1. 防御性 rollback：每次尝试前先 `ROLLBACK` 清掉连接上可能残留的打开事务
///    （如上一轮 `tx.commit()` 返回 BUSY 后事务仍打开）——否则重放闭包再
///    `unchecked_transaction()` 会报 "cannot start a transaction within a
///    transaction" 且毒化连接（此后该连接所有写路径都失败）。无残留事务时
///    ROLLBACK 报 "no transaction is active"，忽略其错误即可。
/// 2. 退避不持锁：连接 Mutex guard 在每次尝试结束即释放，sleep 期间不持有
///    锁——否则持锁 sleep 会饿死所有共享该连接的 DB 操作。事务重放需要同一
///    连接，因此锁在每次重试时重新获取（Inner.conn 是共享单连接，语义不变；
///    代价是重试间隙其他操作可能插队，对可重放的幂等写闭包无害）。
async fn blocking_with_retry<F, T>(inner: Arc<Inner>, f: F) -> Result<T>
where
    F: Fn(&rusqlite::Connection) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    const MAX_ATTEMPTS: u32 = 5;
    let join = tokio::task::spawn_blocking(move || {
        let mut delay = std::time::Duration::from_millis(50);
        let mut attempt = 1u32;
        loop {
            // 锁作用域限于单次尝试：BUSY 退避 sleep 前必须 drop guard（见注释 2）。
            let res = {
                let conn = inner.conn.lock();
                // 防御性 rollback（见注释 1）：清掉上一轮 BUSY 后残留的打开事务。
                // 无事务时该语句报错，忽略。
                let _ = conn.execute_batch("ROLLBACK");
                f(&conn)
            }; // guard 在此 drop，sleep 不持锁。
            match res {
                Ok(v) => return Ok(v),
                Err(e) if is_busy(&e) && attempt < MAX_ATTEMPTS => {
                    tracing::warn!(
                        target: "store",
                        attempt, max = MAX_ATTEMPTS,
                        delay_ms = delay.as_millis() as u64,
                        "sqlite busy/locked，指数退避重试写操作"
                    );
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(std::time::Duration::from_millis(2000));
                    attempt += 1;
                }
                Err(e) => {
                    if is_busy(&e) {
                        tracing::error!(
                            target: "store",
                            attempts = attempt,
                            "sqlite busy/locked，重试耗尽，写操作失败"
                        );
                    }
                    return Err(e);
                }
            }
        }
    })
    .await
    .map_err(|e| StoreError::Other(format!("spawn_blocking join: {e}")))?;
    join
}

/// 是否为 SQLITE_BUSY / SQLITE_LOCKED（含其扩展码——rusqlite 归一为主错误码）。
fn is_busy(e: &StoreError) -> bool {
    matches!(
        e,
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::DatabaseBusy
                    | rusqlite::ffi::ErrorCode::DatabaseLocked,
                ..
            },
            _,
        ))
    )
}

/// S3：在 blocking 线程做 passphrase 加密（PBKDF2 600k 迭代 ~几百 ms，不占运行时线程）。
/// `aad` 绑定凭据归属（`platform:account_id`），防密文挪行错配（见 crypto 模块）。
async fn encrypt_blocking(pass: String, plaintext: String, aad: String) -> Result<String> {
    tokio::task::spawn_blocking(move || crate::crypto::encrypt(&pass, &plaintext, &aad))
        .await
        .map_err(|e| StoreError::Other(format!("spawn_blocking join: {e}")))?
        .map_err(StoreError::Other)
}

/// S3：在 blocking 线程解密（同上，KDF 计算不占运行时线程）。
async fn decrypt_blocking(pass: String, blob: String, aad: String) -> Result<String> {
    tokio::task::spawn_blocking(move || crate::crypto::decrypt(&pass, &blob, &aad))
        .await
        .map_err(|e| StoreError::Other(format!("spawn_blocking join: {e}")))?
        .map_err(StoreError::Other)
}

/// 凭据归属 AAD（GCM 附加认证数据）：`platform:account_id`。
fn credential_aad(platform: &str, account_id: &str) -> String {
    format!("{platform}:{account_id}")
}

/// 打开连接、设 PRAGMA、跑迁移、收紧文件/目录权限。
fn open_and_setup(path: &Path) -> Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // P2-P：busy_timeout 5s——多连接（core store + ilink store + /health 查询）竞争时
    // 等待而非立即 SQLITE_BUSY 失败。
    conn.pragma_update(None, "busy_timeout", 5000_i64)?;
    schema::migrate(&conn).map_err(|e| {
        tracing::error!(error = %e, "store schema migration failed");
        StoreError::Sqlite(e)
    })?;
    tighten_permissions(path)?;
    Ok(conn)
}

#[cfg(unix)]
fn tighten_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // 对单个文件 chmod 0600；文件不存在（如首次 open 时 WAL/SHM 尚未创建）则跳过。
    let chmod_0600 = |p: &Path| -> Result<()> {
        if let Ok(md) = std::fs::metadata(p) {
            let mut perms = md.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(p, perms)?;
        }
        Ok(())
    };

    // 主库文件 + WAL/SHM 边车文件都收紧到 0600。WAL 模式下 SQLite 创建的
    // {db}-wal / {db}-shm 按 umask（常 0644 世界可读），而 WAL 持有明文凭据副本
    // 直到 checkpoint——headless 明文回退部署下是凭据泄漏面。open_and_setup 在
    // migrate（已触发 WAL 创建）之后调用本函数，故此时 WAL/SHM 通常已存在。
    chmod_0600(path)?;
    let base = path.to_string_lossy();
    chmod_0600(&PathBuf::from(format!("{base}-wal")))?;
    chmod_0600(&PathBuf::from(format!("{base}-shm")))?;

    // 注：不再 chmod 父目录——若 db_path 位于共享/系统目录（如 /tmp 或用户自定义路径），
    // 无条件 chmod 父目录会误伤其它内容。父目录权限由部署者负责（建议把 db 放在专属目录
    // 如 ~/.imagent 并自行设 0700，兜底 checkpoint 后重建的 WAL）。
    Ok(())
}

#[cfg(not(unix))]
fn tighten_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db_path(name: &str) -> PathBuf {
        let pid = std::process::id();
        let mut p = std::env::temp_dir();
        p.push(format!("imagent_store_test_{pid}_{name}.db"));
        p
    }

    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        async fn new(name: &str) -> Self {
            let path = temp_db_path(name);
            Self::cleanup(&path);
            Self { path }
        }
        fn cleanup(path: &Path) {
            for ext in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
            }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            Self::cleanup(&self.path);
        }
    }

    async fn list_tables(store: &Store) -> Vec<String> {
        let inner = store.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt =
                conn.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn open_creates_tables() {
        let db = TempDb::new("open").await;
        let store = Store::open(&db.path).await.expect("open");
        let tables = list_tables(&store).await;
        for t in [
            "allowed_senders",
            "allowed_chats",
            "audit_log",
            "config",
            "context_tokens",
            "credentials",
            "named_sessions",
            "run_stats",
            "session_history",
            "sessions",
            "sync_buf",
        ] {
            assert!(tables.iter().any(|x| x == t), "missing table: {t}");
        }
    }

    // ---------- run_stats（schema v8）----------

    #[tokio::test]
    async fn run_stats_append_and_list_since() {
        let db = TempDb::new("run_stats").await;
        let store = Store::open(&db.path).await.unwrap();
        store
            .append_run_stat(
                "feishu:u1",
                Some("claude-cli"),
                100,
                50,
                Some(30),
                Some(0.012),
            )
            .await
            .unwrap();
        // 失败轮次：无 usage 也记一行（tokens 0）。
        store
            .append_run_stat("feishu:u1", Some("codex"), 0, 0, None, None)
            .await
            .unwrap();
        let rows = store.list_run_stats_since(0).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].conv_id, "feishu:u1");
        assert_eq!(rows[0].input_tokens, 100);
        assert_eq!(rows[0].output_tokens, 50);
        assert_eq!(rows[0].cached_tokens, Some(30));
        assert_eq!(rows[0].cost_usd, Some(0.012));
        assert_eq!(rows[0].agent_kind.as_deref(), Some("claude-cli"));
        assert_eq!(rows[1].input_tokens, 0);

        // since 过滤：未来的时间窗排除全部。
        let future = now_secs() + 1000;
        assert!(store.list_run_stats_since(future).await.unwrap().is_empty());
    }

    /// 轮转：保留最近 10000 条，最老淘汰（同 audit_log 手法）。
    #[tokio::test]
    async fn run_stats_rotates_to_10000() {
        let db = TempDb::new("run_stats_rot").await;
        let store = Store::open(&db.path).await.unwrap();
        for _ in 0..10_010 {
            store
                .append_run_stat("c", None, 1, 1, None, None)
                .await
                .unwrap();
        }
        let rows = store.list_run_stats_since(0).await.unwrap();
        assert_eq!(rows.len(), 10_000, "应轮转到 10000 条");
    }

    // ---------- P4-5：allowed_chats ----------

    #[tokio::test]
    async fn allowed_chats_add_list_remove() {
        let db = TempDb::new("chats").await;
        let store = Store::open(&db.path).await.unwrap();
        assert!(store.list_allowed_chats().await.unwrap().is_empty());
        store
            .add_allowed_chat("feishu:oc_b", Some("admin"), Some("im"))
            .await
            .unwrap();
        // 重复 add 不报错、不覆盖元数据。
        store
            .add_allowed_chat("feishu:oc_b", None, None)
            .await
            .unwrap();
        store
            .add_allowed_chat("feishu:oc_a", None, Some("cli"))
            .await
            .unwrap();
        assert_eq!(
            store.list_allowed_chats().await.unwrap(),
            vec!["feishu:oc_a".to_string(), "feishu:oc_b".to_string()]
        );
        assert!(store.remove_allowed_chat("feishu:oc_b").await.unwrap());
        assert!(!store.remove_allowed_chat("feishu:oc_b").await.unwrap());
        assert_eq!(
            store.list_allowed_chats().await.unwrap(),
            vec!["feishu:oc_a".to_string()]
        );
    }

    // ---------- P4-8：session_history ----------

    #[tokio::test]
    async fn session_history_records_and_orders() {
        let db = TempDb::new("hist").await;
        let store = Store::open(&db.path).await.unwrap();
        let row = |sid: &str, at: i64| SessionRow {
            conv_id: "c1".into(),
            session_id: sid.into(),
            agent_kind: "mock".into(),
            workdir: "/tmp".into(),
            name: None,
            created_at: at,
            updated_at: at,
        };
        store.upsert_session(&row("s1", 100)).await.unwrap();
        store.upsert_session(&row("s2", 200)).await.unwrap();
        // 同 session 重复 upsert 不产生新历史行，只刷新 updated_at。
        store.upsert_session(&row("s1", 300)).await.unwrap();
        let hist = store.list_session_history("c1", 10).await.unwrap();
        assert_eq!(hist.len(), 2, "两个不同 session 各一行: {hist:?}");
        // 最近更新的排前（s1 刚被 300 时刻刷新）。
        assert_eq!(hist[0].session_id, "s1");
        assert_eq!(hist[1].session_id, "s2");
        // limit 生效。
        assert_eq!(store.list_session_history("c1", 1).await.unwrap().len(), 1);
        // 其它 conv 不串。
        assert!(store
            .list_session_history("c2", 10)
            .await
            .unwrap()
            .is_empty());
    }

    /// P5-store：session_history per-conv 轮转——保留最近 50 条，最老淘汰，
    /// 其它 conv 不受影响。
    #[tokio::test]
    async fn session_history_rotates_per_conv() {
        let db = TempDb::new("hist_rot").await;
        let store = Store::open(&db.path).await.unwrap();
        let row = |conv: &str, sid: &str| SessionRow {
            conv_id: conv.into(),
            session_id: sid.into(),
            agent_kind: "mock".into(),
            workdir: "/tmp".into(),
            name: None,
            created_at: 1,
            updated_at: 1,
        };
        for i in 0..60 {
            store
                .upsert_session(&row("c1", &format!("s{i}")))
                .await
                .unwrap();
        }
        // 另一个 conv 的历史不被 c1 的轮转波及。
        store.upsert_session(&row("c2", "x1")).await.unwrap();
        let hist = store.list_session_history("c1", 100).await.unwrap();
        assert_eq!(hist.len(), 50, "应轮转到 50 条: {hist:?}");
        assert!(
            hist.iter().any(|r| r.session_id == "s59"),
            "最新保留: {hist:?}"
        );
        assert!(
            !hist.iter().any(|r| r.session_id == "s0"),
            "最老淘汰: {hist:?}"
        );
        assert_eq!(
            store.list_session_history("c2", 100).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn put_credential_rejects_plaintext_when_require_keyring() {
        // require_keyring=true 且 keyring 不可用（cfg!(test) 恒失败）→ fail-closed 拒绝。
        let db = TempDb::new("reqkr_reject").await;
        let store = Store::open(&db.path).await.unwrap();
        store.set_require_keyring(true);
        let err = store
            .put_credential("ilink", "bot1", "{\"bot_token\":\"s\"}")
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("require_keyring"),
            "应 fail-closed 拒绝明文落盘：{err}"
        );
        assert!(CREDENTIAL_KEYRING_REJECTED.get() >= 1);
    }

    #[tokio::test]
    async fn put_credential_falls_back_to_plaintext_by_default() {
        // 默认 require_keyring=false → keyring 失败时明文回退成功。
        let db = TempDb::new("reqkr_fallback").await;
        let store = Store::open(&db.path).await.unwrap();
        store
            .put_credential("ilink", "bot1", "{\"bot_token\":\"s\"}")
            .await
            .unwrap();
        let (_acct, blob) = store.first_credential("ilink").await.unwrap().unwrap();
        assert_eq!(blob, "{\"bot_token\":\"s\"}");
        assert!(CREDENTIAL_PLAINTEXT_FALLBACK.get() >= 1);
    }

    #[tokio::test]
    async fn credential_put_writes_audit() {
        // P1-B：put_credential 应留下审计（测试环境 keyring 走 cfg!(test) fallback → 明文）。
        let db = TempDb::new("cred_audit").await;
        let store = Store::open(&db.path).await.unwrap();
        store
            .put_credential("ilink", "bot1", "{\"bot_token\":\"secret\"}")
            .await
            .unwrap();
        let audit = store.list_audit(10).await.unwrap();
        let cred_puts: Vec<_> = audit
            .iter()
            .filter(|a| a.action == "credential_put")
            .collect();
        assert_eq!(
            cred_puts.len(),
            1,
            "应有 1 条 credential_put 审计: {audit:?}"
        );
        assert_eq!(cred_puts[0].target.as_deref(), Some("bot1"));
        assert_eq!(cred_puts[0].detail.as_deref(), Some("plaintext-fallback"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn db_and_wal_files_are_0600() {
        // P1-A：主库 + WAL/SHM 边车文件都应收紧到 0600。
        use std::os::unix::fs::PermissionsExt;
        let db = TempDb::new("perm0600").await;
        let store = Store::open(&db.path).await.unwrap();
        // 写凭据触发 WAL 活动。
        store
            .put_credential("ilink", "bot1", "{\"bot_token\":\"secret\"}")
            .await
            .unwrap();
        let mode_of = |suffix: &str| -> Option<u32> {
            let p = format!("{}{suffix}", db.path.display());
            std::fs::metadata(&p)
                .ok()
                .map(|md| md.permissions().mode() & 0o777)
        };
        // 主库文件必须 0600。
        assert_eq!(mode_of(""), Some(0o600), "主库文件应为 0600");
        // WAL/SHM 若存在（open 时 migrate 通常已触发 WAL 创建），必须 0600——
        // 修复前默认按 umask 0644 世界可读，是 headless 明文回退下的凭据泄漏面。
        if let Some(m) = mode_of("-wal") {
            assert_eq!(m, 0o600, "WAL 应 0600，实际 {m:o}");
        }
        if let Some(m) = mode_of("-shm") {
            assert_eq!(m, 0o600, "SHM 应 0600，实际 {m:o}");
        }
    }

    #[tokio::test]
    async fn session_upsert_get_delete() {
        let db = TempDb::new("sessions").await;
        let store = Store::open(&db.path).await.unwrap();

        let row = SessionRow {
            conv_id: "ilink:user1".into(),
            session_id: "sess-A".into(),
            agent_kind: "claude-cli".into(),
            workdir: "/tmp/proj".into(),
            name: None,
            created_at: 1_000_000,
            updated_at: 1_000_000,
        };
        store.upsert_session(&row).await.unwrap();

        let got = store
            .get_session("ilink:user1")
            .await
            .unwrap()
            .expect("row");
        assert_eq!(got.session_id, "sess-A");
        assert_eq!(got.created_at, 1_000_000);
        let created = got.created_at;
        let first_updated = got.updated_at;

        // 让 updated_at（unix 秒）能推进
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let mut row2 = row.clone();
        row2.session_id = "sess-B".into();
        row2.name = Some("named".into());
        store.upsert_session(&row2).await.unwrap();

        let got2 = store
            .get_session("ilink:user1")
            .await
            .unwrap()
            .expect("row2");
        assert_eq!(got2.session_id, "sess-B");
        assert_eq!(got2.name.as_deref(), Some("named"));
        // created_at 保留不变
        assert_eq!(got2.created_at, created);
        // updated_at 刷新
        assert!(got2.updated_at > first_updated);

        store.delete_session("ilink:user1").await.unwrap();
        assert!(store.get_session("ilink:user1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn credentials_roundtrip() {
        let db = TempDb::new("creds").await;
        let store = Store::open(&db.path).await.unwrap();
        assert!(store
            .get_credential("ilink", "acc1")
            .await
            .unwrap()
            .is_none());
        store
            .put_credential("ilink", "acc1", r#"{"token":"x"}"#)
            .await
            .unwrap();
        assert_eq!(
            store.get_credential("ilink", "acc1").await.unwrap(),
            Some(r#"{"token":"x"}"#.to_string())
        );
        // 覆盖
        store
            .put_credential("ilink", "acc1", r#"{"token":"y"}"#)
            .await
            .unwrap();
        assert_eq!(
            store.get_credential("ilink", "acc1").await.unwrap(),
            Some(r#"{"token":"y"}"#.to_string())
        );
    }

    #[tokio::test]
    async fn first_credential_and_clone() {
        let db = TempDb::new("first").await;
        let store = Store::open(&db.path).await.unwrap();
        // 空库 => None
        assert!(store.first_credential("ilink").await.unwrap().is_none());

        store
            .put_credential("ilink", "acc1", r#"{"t":"1"}"#)
            .await
            .unwrap();
        store
            .put_credential("ilink", "acc2", r#"{"t":"2"}"#)
            .await
            .unwrap();

        // clone 后仍可用（验证 Store: Clone）
        let cloned = store.clone();
        let (account_id, blob) = cloned
            .first_credential("ilink")
            .await
            .unwrap()
            .expect("present");
        assert_eq!(blob, r#"{"t":"1"}"#);
        assert!(account_id == "acc1" || account_id == "acc2");

        // 其它 platform => None
        assert!(store.first_credential("wecom").await.unwrap().is_none());
    }

    #[test]
    fn keyring_marker_pure_helpers() {
        use crate::credentials;
        // marker_for 产出 KEYRING_MARKER_PREFIX 前缀
        let m = credentials::marker_for("ilink", "bot-123");
        assert_eq!(m, "keyring:ilink:bot-123");
        // is_keyring_marker：marker → true
        assert!(credentials::is_keyring_marker(&m));
        assert!(credentials::is_keyring_marker("keyring:foo:bar"));
        // 明文 JSON / 空 / 普通字符串 → false
        assert!(!credentials::is_keyring_marker(r#"{"bot_token":"x"}"#));
        assert!(!credentials::is_keyring_marker(""));
        assert!(!credentials::is_keyring_marker("Keyring:foo")); // 大小写敏感
                                                                 // account_id 含 ":" 也不影响判定（marker 仍以固定前缀起始）
        let m2 = credentials::marker_for("ilink", "a:b");
        assert!(credentials::is_keyring_marker(&m2));
    }

    #[tokio::test]
    async fn credential_plaintext_migration_preserves_value() {
        // 旧库场景：blob 是明文 JSON（无 marker）。get 时应懒迁移到 keyring，
        // 失败（CI 无 keychain）则保持明文。无论是否迁移成功，返回值必须 == 原明文。
        // 直接用 SQL 种入明文，绕过 put_credential 的 keyring 写入。
        let db = TempDb::new("migrate").await;
        let store = Store::open(&db.path).await.unwrap();
        let plain = r#"{"bot_token":"secret","ilink_bot_id":"b1"}"#;
        {
            let inner = store.inner.clone();
            blocking_with(inner, move |conn| {
                Ok(conn.execute(
                    "INSERT INTO credentials (platform, account_id, blob, updated_at) \
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params!["ilink", "bot-old", plain, 1_i64],
                )?)
            })
            .await
            .unwrap();
        }
        // get 经过 resolve：marker? 否 → 明文路径，返回明文（并尝试迁移）。
        assert_eq!(
            store.get_credential("ilink", "bot-old").await.unwrap(),
            Some(plain.to_string())
        );
        // first_credential 同样解析
        let (aid, blob) = store
            .first_credential("ilink")
            .await
            .unwrap()
            .expect("present");
        assert_eq!(aid, "bot-old");
        assert_eq!(blob, plain);
    }

    // ---------- S3：凭据应用层加密 ----------

    /// 读出 credentials.blob 原始值（不经 resolve），断言落盘形态用。
    async fn raw_blob(store: &Store, platform: &str, account: &str) -> String {
        let inner = store.inner.clone();
        let (p, a) = (platform.to_string(), account.to_string());
        blocking_with(inner, move |conn| {
            Ok(conn.query_row(
                "SELECT blob FROM credentials WHERE platform = ?1 AND account_id = ?2",
                rusqlite::params![p, a],
                |r| r.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap()
    }

    /// keyring 不可用（cfg!(test) 恒失败）+ passphrase 已配置 → 落盘为 enc:v1 而非明文，
    /// 读取（解密）还原明文；审计 detail 为 encrypted-fallback。
    #[tokio::test]
    async fn credential_encrypted_fallback_roundtrip() {
        let db = TempDb::new("enc_rt").await;
        let store = Store::open(&db.path).await.unwrap();
        store.set_passphrase(Some("s3-pass"));
        let plain = r#"{"bot_token":"secret-token"}"#;
        store.put_credential("ilink", "bot1", plain).await.unwrap();
        let raw = raw_blob(&store, "ilink", "bot1").await;
        assert!(raw.starts_with("enc:v2:"), "应加密落盘: {raw}");
        assert!(!raw.contains("secret-token"), "落盘不得含明文");
        // get / first_credential 都解密还原。
        assert_eq!(
            store.get_credential("ilink", "bot1").await.unwrap(),
            Some(plain.to_string())
        );
        assert_eq!(
            store.first_credential("ilink").await.unwrap().unwrap().1,
            plain
        );
        let audit = store.list_audit(10).await.unwrap();
        let put = audit.iter().find(|a| a.action == "credential_put").unwrap();
        assert_eq!(put.detail.as_deref(), Some("encrypted-fallback"));
    }

    /// enc blob + 未配置 passphrase → 可读错误（提示 IMAGENT_PASSPHRASE）；
    /// 错误 passphrase → 解密失败错误。
    #[tokio::test]
    async fn credential_encrypted_missing_or_wrong_passphrase_errors() {
        let db = TempDb::new("enc_err").await;
        let store = Store::open(&db.path).await.unwrap();
        store.set_passphrase(Some("right"));
        store.put_credential("ilink", "bot1", "s").await.unwrap();
        // 换口令模拟「重启后未配置 / 配错」。
        store.set_passphrase(Some("wrong"));
        let err = store.get_credential("ilink", "bot1").await.unwrap_err();
        assert!(format!("{err}").contains("IMAGENT_PASSPHRASE"), "{err}");
        store.set_passphrase(None);
        // cfg!(test) 下 env 回退被跳过（见 effective_passphrase 注释）→ 视为未配置。
        let err = store.get_credential("ilink", "bot1").await.unwrap_err();
        assert!(format!("{err}").contains("IMAGENT_PASSPHRASE"), "{err}");
    }

    /// 惰性迁移：存量明文 blob + 配置 passphrase + keyring 不可用 → get 返回明文，
    /// 且 DB 中 blob 被重写为 enc:v1。
    #[tokio::test]
    async fn credential_plaintext_lazily_migrates_to_encrypted() {
        let db = TempDb::new("enc_migrate").await;
        let store = Store::open(&db.path).await.unwrap();
        let plain = r#"{"bot_token":"legacy"}"#;
        {
            let inner = store.inner.clone();
            let p = plain.to_string();
            blocking_with(inner, move |conn| {
                Ok(conn.execute(
                    "INSERT INTO credentials (platform, account_id, blob, updated_at) \
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params!["ilink", "bot-old", p, 1_i64],
                )?)
            })
            .await
            .unwrap();
        }
        store.set_passphrase(Some("migrate-pass"));
        // 第一次 get：返回明文，并把 blob 重写为 enc:v1。
        assert_eq!(
            store.get_credential("ilink", "bot-old").await.unwrap(),
            Some(plain.to_string())
        );
        assert!(raw_blob(&store, "ilink", "bot-old")
            .await
            .starts_with("enc:v2:"));
        // 第二次 get：走解密路径，仍还原明文。
        assert_eq!(
            store.get_credential("ilink", "bot-old").await.unwrap(),
            Some(plain.to_string())
        );
    }

    // ---------- P4：SQLite busy 重试 ----------

    /// 并发写压力：两个 Store 实例（各自独立连接，模拟 core/ilink 多连接）+
    /// 多 task 并发 upsert，busy 重试下应全部成功、最终状态正确。
    ///
    /// 强度说明：真实 BUSY 需要跨连接的长事务竞争，单测难以稳定触发，此处
    /// 用「双连接 × 4 task × 25 次 upsert + 同 key 竞争」制造锁竞争面；CI 环境若
    /// 偶发不稳，可把 TASKS/TIMES 减半（重试路径本身由 busy_timeout+退避保证）。
    #[tokio::test]
    async fn concurrent_upserts_with_busy_retry_all_succeed() {
        let db = TempDb::new("busy").await;
        let store_a = Store::open(&db.path).await.unwrap();
        let store_b = Store::open(&db.path).await.unwrap();
        const TASKS: usize = 4;
        const TIMES: usize = 25;
        let mut handles = Vec::new();
        for t in 0..TASKS {
            // 交替用两个连接（不同 Connection，形成真实的跨连接写竞争）。
            let store = if t % 2 == 0 {
                store_a.clone()
            } else {
                store_b.clone()
            };
            handles.push(tokio::spawn(async move {
                for i in 0..TIMES {
                    store
                        .upsert_session(&SessionRow {
                            conv_id: format!("conv-{t}"),
                            session_id: format!("sess-{t}-{i}"),
                            agent_kind: "mock".into(),
                            workdir: "/tmp".into(),
                            name: None,
                            created_at: 1,
                            updated_at: 1,
                        })
                        .await
                        .expect("并发 upsert 不应因 BUSY 失败");
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        for t in 0..TASKS {
            let got = store_a.get_session(&format!("conv-{t}")).await.unwrap();
            assert_eq!(
                got.expect("row").session_id,
                format!("sess-{t}-{}", TIMES - 1),
                "最终状态应为最后一次 upsert"
            );
        }
    }

    #[tokio::test]
    async fn sync_buf_roundtrip() {
        let db = TempDb::new("syncbuf").await;
        let store = Store::open(&db.path).await.unwrap();
        assert!(store.get_sync_buf("ilink", "acc1").await.unwrap().is_none());
        store
            .set_sync_buf("ilink", "acc1", "cursor-1")
            .await
            .unwrap();
        assert_eq!(
            store.get_sync_buf("ilink", "acc1").await.unwrap(),
            Some("cursor-1".into())
        );
        store
            .set_sync_buf("ilink", "acc1", "cursor-2")
            .await
            .unwrap();
        assert_eq!(
            store.get_sync_buf("ilink", "acc1").await.unwrap(),
            Some("cursor-2".into())
        );
    }

    #[tokio::test]
    async fn context_token_roundtrip() {
        let db = TempDb::new("ctx").await;
        let store = Store::open(&db.path).await.unwrap();
        assert!(store
            .get_context_token("ilink", "acc1", "peer1")
            .await
            .unwrap()
            .is_none());
        store
            .set_context_token("ilink", "acc1", "peer1", "tok-1")
            .await
            .unwrap();
        assert_eq!(
            store
                .get_context_token("ilink", "acc1", "peer1")
                .await
                .unwrap(),
            Some("tok-1".into())
        );
        store
            .set_context_token("ilink", "acc1", "peer1", "tok-2")
            .await
            .unwrap();
        assert_eq!(
            store
                .get_context_token("ilink", "acc1", "peer1")
                .await
                .unwrap(),
            Some("tok-2".into())
        );
    }

    #[tokio::test]
    async fn migrate_is_idempotent() {
        let db = TempDb::new("idem").await;
        let s1 = Store::open(&db.path).await.unwrap();
        drop(s1);
        // 再次 open 同一库（迁移已是 v1，应跳过建表、不报错）
        let s2 = Store::open(&db.path).await.unwrap();
        drop(s2);
    }

    #[tokio::test]
    async fn allowed_senders_roundtrip() {
        let db = TempDb::new("allow").await;
        let store = Store::open(&db.path).await.unwrap();

        // 空
        assert!(store.list_allowed_senders().await.unwrap().is_empty());

        // add
        store
            .add_allowed_sender("bob", Some("alice"), Some("im"))
            .await
            .unwrap();
        store.add_allowed_sender("amy", None, None).await.unwrap();

        let list = store.list_allowed_senders().await.unwrap();
        assert_eq!(list, vec!["amy".to_string(), "bob".to_string()]);

        // 重复 add：不报错、不覆盖原 added_by/source。
        let before = store
            .list_allowed_senders_detailed()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.sender == "bob")
            .unwrap();
        store
            .add_allowed_sender("bob", Some("cli"), Some("manual"))
            .await
            .unwrap();
        let after = store
            .list_allowed_senders_detailed()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.sender == "bob")
            .unwrap();
        assert_eq!(after.added_at, before.added_at, "重复 add 不刷新 added_at");
        assert_eq!(
            after.added_by.as_deref(),
            Some("alice"),
            "重复 add 不覆盖 added_by"
        );
        assert_eq!(
            after.source.as_deref(),
            Some("im"),
            "重复 add 不覆盖 source"
        );

        // remove
        assert!(store.remove_allowed_sender("bob").await.unwrap());
        assert!(!store.remove_allowed_sender("bob").await.unwrap());
        let list = store.list_allowed_senders().await.unwrap();
        assert_eq!(list, vec!["amy".to_string()]);
    }

    #[tokio::test]
    async fn audit_append_list_descending() {
        let db = TempDb::new("audit").await;
        let store = Store::open(&db.path).await.unwrap();

        store
            .append_audit("allow", Some("alice"), Some("bob"), Some("added"))
            .await
            .unwrap();
        store
            .append_audit("disallow", Some("alice"), Some("bob"), None)
            .await
            .unwrap();
        store
            .append_audit("allow", Some("cli"), Some("amy"), Some("cli-bootstrap"))
            .await
            .unwrap();

        let list = store.list_audit(10).await.unwrap();
        assert_eq!(list.len(), 3);
        // 倒序：最新在最前。
        assert_eq!(list[0].action, "allow");
        assert_eq!(list[0].actor.as_deref(), Some("cli"));
        assert_eq!(list[0].target.as_deref(), Some("amy"));
        assert_eq!(list[1].action, "disallow");
        assert_eq!(list[2].action, "allow");
        assert_eq!(list[2].actor.as_deref(), Some("alice"));
        // id 倒序。
        assert!(list[0].id > list[1].id);
        assert!(list[1].id > list[2].id);

        // limit 生效。
        let limited = store.list_audit(1).await.unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].id, list[0].id);
    }

    #[tokio::test]
    async fn migrate_v1_to_v2_adds_tables() {
        // 手动建一个仅 v1 的库，再 open 触发 v2 迁移，验证新表存在。
        let db = TempDb::new("migrate_v2").await;
        {
            let conn = rusqlite::Connection::open(&db.path).unwrap();
            conn.execute_batch(crate::schema::SCHEMA_V1).unwrap();
            conn.pragma_update(None, "user_version", 1_i64).unwrap();
        }
        let store = Store::open(&db.path).await.unwrap();
        let tables = list_tables(&store).await;
        assert!(
            tables.iter().any(|x| x == "allowed_senders"),
            "v2 迁移应建 allowed_senders"
        );
        assert!(
            tables.iter().any(|x| x == "audit_log"),
            "v2 迁移应建 audit_log"
        );
        // 新表可写。
        store.add_allowed_sender("zoe", None, None).await.unwrap();
        assert_eq!(
            store.list_allowed_senders().await.unwrap(),
            vec!["zoe".to_string()]
        );
    }
    #[tokio::test]
    async fn config_kv_roundtrip() {
        let db = TempDb::new("config").await;
        let store = Store::open(&db.path).await.unwrap();

        // 不存在 → None。
        assert!(store.get_config("active_name:c1").await.unwrap().is_none());

        // set + get。
        store
            .set_config("active_name:c1", "refactor")
            .await
            .unwrap();
        assert_eq!(
            store.get_config("active_name:c1").await.unwrap(),
            Some("refactor".to_string())
        );

        // 覆盖。
        store.set_config("active_name:c1", "docs").await.unwrap();
        assert_eq!(
            store.get_config("active_name:c1").await.unwrap(),
            Some("docs".to_string())
        );

        // delete。
        store.delete_config("active_name:c1").await.unwrap();
        assert!(store.get_config("active_name:c1").await.unwrap().is_none());
        // 重复 delete 不报错。
        store.delete_config("active_name:c1").await.unwrap();
    }

    #[tokio::test]
    async fn named_sessions_roundtrip() {
        let db = TempDb::new("named").await;
        let store = Store::open(&db.path).await.unwrap();

        let row = NamedSessionRow {
            conv_id: "ilink:u1".into(),
            name: "refactor".into(),
            session_id: "sess-A".into(),
            agent_kind: Some("claude-cli".into()),
            workdir: Some("/tmp/p".into()),
            created_at: 1_000_000,
            updated_at: 1_000_000,
        };
        store.upsert_named_session(&row).await.unwrap();

        let got = store
            .get_named_session("ilink:u1", "refactor")
            .await
            .unwrap()
            .expect("row");
        assert_eq!(got.session_id, "sess-A");
        assert_eq!(got.name, "refactor");
        let created = got.created_at;
        let first_updated = got.updated_at;

        // 让 updated_at 推进。
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // 更新：created_at 保留、updated_at 刷新、session_id 改变。
        let mut row2 = row.clone();
        row2.session_id = "sess-B".into();
        store.upsert_named_session(&row2).await.unwrap();
        let got2 = store
            .get_named_session("ilink:u1", "refactor")
            .await
            .unwrap()
            .expect("row2");
        assert_eq!(got2.session_id, "sess-B");
        assert_eq!(got2.created_at, created, "created_at 应保留");
        assert!(got2.updated_at > first_updated, "updated_at 应刷新");

        // 列出：多个命名，按 updated_at 倒序。
        let row3 = NamedSessionRow {
            conv_id: "ilink:u1".into(),
            name: "docs".into(),
            session_id: "sess-C".into(),
            agent_kind: None,
            workdir: None,
            created_at: 2_000_000,
            updated_at: 2_000_000,
        };
        store.upsert_named_session(&row3).await.unwrap();

        let list = store.list_named_sessions("ilink:u1").await.unwrap();
        assert_eq!(list.len(), 2, "list={list:?}");
        // 倒序：docs 刚 upsert（updated_at 最新）应在前。
        assert_eq!(list[0].name, "docs");
        // 删除。
        store
            .delete_named_session("ilink:u1", "docs")
            .await
            .unwrap();
        assert!(
            store
                .get_named_session("ilink:u1", "docs")
                .await
                .unwrap()
                .is_none(),
            "删除后应不存在"
        );
        let list2 = store.list_named_sessions("ilink:u1").await.unwrap();
        assert_eq!(list2.len(), 1);

        // 其它 conv 隔离。
        assert!(
            store
                .list_named_sessions("ilink:u2")
                .await
                .unwrap()
                .is_empty(),
            "其它 conv 应为空"
        );
    }

    #[tokio::test]
    async fn migrate_v2_to_v3_adds_named_sessions() {
        // 手动建一个仅到 v2 的库，再 open 触发 v3 迁移。
        let db = TempDb::new("migrate_v3").await;
        {
            let conn = rusqlite::Connection::open(&db.path).unwrap();
            conn.execute_batch(crate::schema::SCHEMA_V1).unwrap();
            conn.execute_batch(crate::schema::SCHEMA_V2).unwrap();
            conn.pragma_update(None, "user_version", 2_i64).unwrap();
        }
        let store = Store::open(&db.path).await.unwrap();
        let tables = list_tables(&store).await;
        assert!(
            tables.iter().any(|x| x == "named_sessions"),
            "v3 迁移应建 named_sessions"
        );
        // 新表可写。
        store
            .upsert_named_session(&NamedSessionRow {
                conv_id: "c".into(),
                name: "n".into(),
                session_id: "s".into(),
                agent_kind: None,
                workdir: None,
                created_at: 1,
                updated_at: 1,
            })
            .await
            .unwrap();
        assert!(store.get_named_session("c", "n").await.unwrap().is_some());
    }

    /// v6：live_cards 表迁移 + record/clear/list 回环 + per-conv upsert 覆盖。
    #[tokio::test]
    async fn migrate_v5_to_v6_adds_live_cards() {
        let db = TempDb::new("migrate_v6").await;
        {
            let conn = rusqlite::Connection::open(&db.path).unwrap();
            conn.execute_batch(crate::schema::SCHEMA_V1).unwrap();
            conn.execute_batch(crate::schema::SCHEMA_V2).unwrap();
            conn.execute_batch(crate::schema::SCHEMA_V3).unwrap();
            conn.execute_batch(crate::schema::SCHEMA_V4).unwrap();
            conn.execute_batch(crate::schema::SCHEMA_V5).unwrap();
            conn.pragma_update(None, "user_version", 5_i64).unwrap();
        }
        let store = Store::open(&db.path).await.unwrap();
        let tables = list_tables(&store).await;
        assert!(
            tables.iter().any(|x| x == "live_cards"),
            "v6 迁移应建 live_cards"
        );
        store
            .record_live_card("c1", "feishu", "card:abc")
            .await
            .unwrap();
        // 同 conv 再登记 → upsert 覆盖（每 conv 至多一张）。
        store
            .record_live_card("c1", "feishu", "card:def")
            .await
            .unwrap();
        store
            .record_live_card("c2", "feishu", "msg:x")
            .await
            .unwrap();
        let rows = store.list_live_cards().await.unwrap();
        assert_eq!(rows.len(), 2, "per-conv upsert 后应只 2 行: {rows:?}");
        let c1 = rows.iter().find(|r| r.conv_id == "c1").unwrap();
        assert_eq!(c1.handle, "card:def", "覆盖后应保留新句柄");
        store.clear_live_card("c1").await.unwrap();
        let rows = store.list_live_cards().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].conv_id, "c2");
    }

    /// P7-A1：admin_senders 表 CRUD 往返 + 幂等（INSERT OR IGNORE 不覆盖元数据）。
    #[tokio::test]
    async fn admin_senders_roundtrip() {
        let db = TempDb::new("admin_rt").await;
        let store = Store::open(&db.path).await.unwrap();
        assert!(store.list_admin_senders().await.unwrap().is_empty());
        store
            .add_admin_sender("ou_a", Some("ou_root"), Some("im"))
            .await
            .unwrap();
        // 重复 add 幂等：仍一条。
        store.add_admin_sender("ou_a", None, None).await.unwrap();
        store.add_admin_sender("ou_b", None, None).await.unwrap();
        assert_eq!(
            store.list_admin_senders().await.unwrap(),
            vec!["ou_a".to_string(), "ou_b".to_string()]
        );
        assert!(store.remove_admin_sender("ou_a").await.unwrap());
        assert!(
            !store.remove_admin_sender("ou_a").await.unwrap(),
            "再删应返回 false"
        );
        assert_eq!(
            store.list_admin_senders().await.unwrap(),
            vec!["ou_b".to_string()]
        );
        drop(store);
        TempDb::cleanup(&db.path);
    }

    // ---------- 缺陷修复：busy 重试 / 持锁 sleep / 残留事务 ----------

    /// 枙造 SQLITE_BUSY 形态的 StoreError（与 is_busy 匹配的主错误码）。
    fn busy_error() -> StoreError {
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::DatabaseBusy,
                extended_code: 5,
            },
            Some("database is locked".into()),
        ))
    }

    /// #1+#3：BUSY 后重试成功——重试前防御性 ROLLBACK 清掉残留事务，且闭包
    /// 可重放（Fn）；成功后连接仍可用（未毒化）。
    #[tokio::test]
    async fn busy_retry_recovers_and_keeps_connection_usable() {
        let db = TempDb::new("busy_replay").await;
        let store = Store::open(&db.path).await.unwrap();
        let inner = store.inner.clone();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let v = blocking_with_retry(inner, move |conn| {
            if c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                // 模拟「commit 返回 BUSY 后事务仍打开」：开一个事务不提交，
                // guard drop 时 rusqlite 会尝试回滚——但即便残留（或回滚也被
                // BUSY 拒绝），下一轮开头的防御性 ROLLBACK 也应兜住。
                let _ = conn.unchecked_transaction();
                return Err(busy_error());
            }
            Ok(42_i32)
        })
        .await
        .unwrap();
        assert_eq!(v, 42);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        // 连接未毒化：正常写路径仍可用。
        store.set_config("k", "v").await.unwrap();
        assert_eq!(store.get_config("k").await.unwrap().as_deref(), Some("v"));
    }

    /// #1：连接上有残留打开事务（模拟 commit BUSY 后毒化）时，重试路径的
    /// 防御性 ROLLBACK 应清掉它，写操作照常成功。
    #[tokio::test]
    async fn retry_path_rolls_back_residual_open_transaction() {
        let db = TempDb::new("busy_residual").await;
        let store = Store::open(&db.path).await.unwrap();
        {
            // 人为留下一个打开的事务（BEGIN 后不提交不回滚）。
            let inner = store.inner.clone();
            blocking_with(inner, move |conn| {
                conn.execute_batch("BEGIN IMMEDIATE")?;
                Ok(())
            })
            .await
            .unwrap();
        }
        // blocking_with_retry 的防御性 ROLLBACK 清掉残留事务后写入成功。
        store.set_config("k2", "v2").await.unwrap();
        assert_eq!(store.get_config("k2").await.unwrap().as_deref(), Some("v2"));
    }

    /// #3：退避 sleep 期间不持连接锁——用**顺序断言**而非绝对时长（CI runner
    /// 调度抖动会让时长断言误报）：闭包首次调用持锁 sleep 后返回 BUSY，退避
    /// 50ms（锁外）后重试。插队写入若在重试开始前完成，即证明退避期间锁
    /// 已释放（若持锁 sleep，插队必然排到重试之后）。10ms 为唤醒调度容差。
    #[tokio::test]
    async fn busy_backoff_does_not_hold_lock() {
        let db = TempDb::new("busy_lock").await;
        let store = Store::open(&db.path).await.unwrap();
        let inner = store.inner.clone();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let retry2_start = std::sync::Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
        let c = calls.clone();
        let r2 = retry2_start.clone();
        let slow = tokio::spawn(async move {
            blocking_with_retry(inner, move |_conn| {
                if c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 1 {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    return Err(busy_error());
                }
                *r2.lock().unwrap() = Some(std::time::Instant::now());
                Ok(())
            })
            .await
        });
        // 等闭包确认进入首次持锁执行段，再等到持锁中段（远离窗口边界）。
        while calls.load(std::sync::atomic::Ordering::SeqCst) < 1 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        store.set_config("k3", "v3").await.unwrap();
        let done_instant = std::time::Instant::now();
        slow.await.unwrap().unwrap();
        let retry_at = *retry2_start.lock().unwrap();
        let retry_at = retry_at.expect("重试应已发生");
        assert!(
            done_instant <= retry_at + std::time::Duration::from_millis(10),
            "插队写入（完成于 {done_instant:?}）应先于退避重试（{retry_at:?}）——\
             退避期间锁应已释放"
        );
    }

    // ---------- 缺陷修复：迁移 CAS / 降级 ----------

    /// #2：CAS 迁移——expected_old 不匹配（另一写者已更新）时放弃，返回
    /// false 且不覆盖新值；匹配时生效并留审计。
    #[tokio::test]
    async fn credential_migration_is_compare_and_swap() {
        let db = TempDb::new("mig_cas").await;
        let store = Store::open(&db.path).await.unwrap();
        let plain = r#"{"bot_token":"old"}"#;
        {
            let inner = store.inner.clone();
            let p = plain.to_string();
            blocking_with(inner, move |conn| {
                Ok(conn.execute(
                    "INSERT INTO credentials (platform, account_id, blob, updated_at) \
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params!["ilink", "bot1", p, 1_i64],
                )?)
            })
            .await
            .unwrap();
        }
        // CAS 生效：旧值匹配 → 更新。
        assert!(store
            .try_migrate_credential_blob("ilink", "bot1", plain, "keyring:ilink:bot1")
            .await
            .unwrap());
        assert_eq!(
            raw_blob(&store, "ilink", "bot1").await,
            "keyring:ilink:bot1"
        );
        // 模拟并发写新凭据后，再用过期旧值迁移 → CAS 失败，不覆盖新值。
        assert!(!store
            .try_migrate_credential_blob("ilink", "bot1", plain, "enc:v2:whatever")
            .await
            .unwrap());
        assert_eq!(
            raw_blob(&store, "ilink", "bot1").await,
            "keyring:ilink:bot1"
        );
        // 审计只在 CAS 生效时留痕（一次）。
        let audit = store.list_audit(20).await.unwrap();
        assert_eq!(
            audit
                .iter()
                .filter(|a| a.action == "credential_migrated")
                .count(),
            1
        );
    }

    /// #7：迁移回写失败（如 DB 写错误）时 get 降级返回手上的明文，而非整条报错。
    /// 用 BEFORE UPDATE 触发器 RAISE 模拟回写失败。
    #[tokio::test]
    async fn credential_migration_write_failure_degrades_to_plaintext() {
        let db = TempDb::new("mig_degrade").await;
        let store = Store::open(&db.path).await.unwrap();
        let plain = r#"{"bot_token":"legacy"}"#;
        {
            let inner = store.inner.clone();
            let p = plain.to_string();
            blocking_with(inner, move |conn| {
                conn.execute(
                    "INSERT INTO credentials (platform, account_id, blob, updated_at) \
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params!["ilink", "bot1", p, 1_i64],
                )?;
                // 迁移回写（UPDATE credentials）一律失败。
                conn.execute_batch(
                    "CREATE TRIGGER fail_cred_update BEFORE UPDATE ON credentials \
                     BEGIN SELECT RAISE(ABORT, 'simulated write failure'); END",
                )?;
                Ok(())
            })
            .await
            .unwrap();
        }
        store.set_passphrase(Some("p"));
        // keyring 在 cfg!(test) 下恒失败 → 走加密惰性迁移；回写触发器失败 →
        // 降级返回明文而不是报错。
        assert_eq!(
            store.get_credential("ilink", "bot1").await.unwrap(),
            Some(plain.to_string())
        );
    }

    // ---------- 缺陷修复：passphrase 空串过滤 ----------

    /// #5：set_passphrase(Some("")) 与 None 等同（过滤空串）——不进入加密回退。
    #[tokio::test]
    async fn empty_passphrase_is_treated_as_unset() {
        let db = TempDb::new("empty_pass").await;
        let store = Store::open(&db.path).await.unwrap();
        store.set_passphrase(Some(""));
        store.set_passphrase(Some("   ")); // 纯空白同样过滤
        store
            .put_credential("ilink", "bot1", "{\"t\":\"x\"}")
            .await
            .unwrap();
        // 未生效 passphrase → 明文回退（而非用空口令加密）。
        let raw = raw_blob(&store, "ilink", "bot1").await;
        assert_eq!(raw, "{\"t\":\"x\"}");
        // 显式清除回 None 后 env/加密路径同样不误触发。
        store.set_passphrase(None);
        assert_eq!(
            store.get_credential("ilink", "bot1").await.unwrap(),
            Some("{\"t\":\"x\"}".to_string())
        );
    }

    // ---------- 缺陷修复：AAD 绑定 ----------

    /// #6：加密 blob 绑定 `platform:account_id`——密文挪到另一行（不同归属）
    /// 读取必须失败（防错配注入），正确归属照常解密。
    #[tokio::test]
    async fn encrypted_blob_is_bound_to_owner_via_aad() {
        let db = TempDb::new("aad").await;
        let store = Store::open(&db.path).await.unwrap();
        store.set_passphrase(Some("aad-pass"));
        store
            .put_credential("ilink", "bot1", "{\"t\":\"secret\"}")
            .await
            .unwrap();
        let raw = raw_blob(&store, "ilink", "bot1").await;
        assert!(raw.starts_with("enc:v2:"));
        // 正确归属可解。
        assert_eq!(
            store.get_credential("ilink", "bot1").await.unwrap(),
            Some("{\"t\":\"secret\"}".to_string())
        );
        // 把密文挪到另一账号（模拟错配/挪行）→ 读取失败。
        {
            let inner = store.inner.clone();
            let r = raw.clone();
            blocking_with(inner, move |conn| {
                Ok(conn.execute(
                    "INSERT INTO credentials (platform, account_id, blob, updated_at) \
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params!["ilink", "bot2", r, 1_i64],
                )?)
            })
            .await
            .unwrap();
        }
        let err = store.get_credential("ilink", "bot2").await.unwrap_err();
        assert!(
            format!("{err}").contains("解密失败"),
            "挪行密文应解密失败: {err}"
        );
    }

    // ---------- A1：switch_named_session 原子 API ----------

    /// #8：`activate = Some`（切回历史命名）：单事务完成 sessions upsert +
    /// active_name 设置 + compact_summary 清理。
    #[tokio::test]
    async fn switch_named_session_activates_atomically() {
        let db = TempDb::new("sw_act").await;
        let store = Store::open(&db.path).await.unwrap();
        // 预置：历史命名 session + 旧活动 session + 旧摘要。
        store
            .upsert_named_session(&NamedSessionRow {
                conv_id: "c1".into(),
                name: "refactor".into(),
                session_id: "sess-named".into(),
                agent_kind: Some("mock".into()),
                workdir: Some("/tmp".into()),
                created_at: 1,
                updated_at: 1,
            })
            .await
            .unwrap();
        store
            .upsert_session(&SessionRow {
                conv_id: "c1".into(),
                session_id: "sess-old".into(),
                agent_kind: "mock".into(),
                workdir: "/tmp".into(),
                name: None,
                created_at: 1,
                updated_at: 1,
            })
            .await
            .unwrap();
        store
            .set_config("compact_summary:c1", "旧摘要")
            .await
            .unwrap();
        let sr = SessionRow {
            conv_id: "c1".into(),
            session_id: "sess-named".into(),
            agent_kind: "mock".into(),
            workdir: "/tmp".into(),
            name: Some("refactor".into()),
            created_at: 1,
            updated_at: 1,
        };
        store
            .switch_named_session("c1", "refactor", Some(&sr))
            .await
            .unwrap();
        // 三项效果同时成立。
        let got = store.get_session("c1").await.unwrap().unwrap();
        assert_eq!(got.session_id, "sess-named");
        assert_eq!(got.name.as_deref(), Some("refactor"));
        assert_eq!(
            store.get_config("active_name:c1").await.unwrap().as_deref(),
            Some("refactor")
        );
        assert!(store
            .get_config("compact_summary:c1")
            .await
            .unwrap()
            .is_none());
        // session_history 同步（与 upsert_session 同构）。
        let hist = store.list_session_history("c1", 10).await.unwrap();
        assert!(hist.iter().any(|h| h.session_id == "sess-named"));
    }

    /// #8：`activate = None`（新命名）：删活动 session + 设 active_name + 清摘要。
    #[tokio::test]
    async fn switch_named_session_new_name_clears_active_session() {
        let db = TempDb::new("sw_new").await;
        let store = Store::open(&db.path).await.unwrap();
        store
            .upsert_session(&SessionRow {
                conv_id: "c2".into(),
                session_id: "sess-old".into(),
                agent_kind: "mock".into(),
                workdir: "/tmp".into(),
                name: None,
                created_at: 1,
                updated_at: 1,
            })
            .await
            .unwrap();
        store
            .set_config("compact_summary:c2", "旧摘要")
            .await
            .unwrap();
        store
            .switch_named_session("c2", "newtask", None)
            .await
            .unwrap();
        assert!(store.get_session("c2").await.unwrap().is_none());
        assert_eq!(
            store.get_config("active_name:c2").await.unwrap().as_deref(),
            Some("newtask")
        );
        assert!(store
            .get_config("compact_summary:c2")
            .await
            .unwrap()
            .is_none());
    }
}
