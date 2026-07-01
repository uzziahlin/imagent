use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::{Result, StoreError};
use crate::schema;

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

struct Inner {
    conn: Mutex<rusqlite::Connection>,
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

impl Store {
    /// 打开（不存在则创建）数据库文件，建表迁移，并把文件权限收紧到 0600、
    /// 所在目录 0700（仅 unix）。开启 WAL。
    pub async fn open(path: &Path) -> Result<Self> {
        let path = path.to_path_buf();
        let inner = blocking_open(path).await?;
        Ok(Store { inner })
    }

    // —— credentials ——

    // TODO(P2): 迁移到 OS keyring 加密落盘（DESIGN §9.4）
    pub async fn put_credential(&self, platform: &str, account_id: &str, blob: &str) -> Result<()> {
        let (platform, account_id, blob) = (platform.to_string(), account_id.to_string(), blob.to_string());
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let now = now_secs();
            conn.execute(
                "INSERT INTO credentials (platform, account_id, blob, updated_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(platform, account_id) DO UPDATE SET blob = excluded.blob, updated_at = excluded.updated_at",
                rusqlite::params![platform, account_id, blob, now],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_credential(&self, platform: &str, account_id: &str) -> Result<Option<String>> {
        let (platform, account_id) = (platform.to_string(), account_id.to_string());
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt = conn.prepare(
                "SELECT blob FROM credentials WHERE platform = ?1 AND account_id = ?2",
            )?;
            let mut rows = stmt.query(rusqlite::params![platform, account_id])?;
            Ok(rows.next()?.map(|r| r.get::<_, String>(0)).transpose()?)
        })
        .await
    }

    /// 取该 platform 的第一条凭据 (account_id, blob)。P1 单账号。
    pub async fn first_credential(&self, platform: &str) -> Result<Option<(String, String)>> {
        let platform = platform.to_string();
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let mut stmt = conn.prepare(
                "SELECT account_id, blob FROM credentials WHERE platform = ?1 LIMIT 1",
            )?;
            let mut rows = stmt.query(rusqlite::params![platform])?;
            rows.next()?
                .map(|r| Ok::<_, StoreError>((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .transpose()
        })
        .await
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
    pub async fn upsert_session(&self, row: &SessionRow) -> Result<()> {
        let row = row.clone();
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            let now = now_secs();
            conn.execute(
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
            Ok(())
        })
        .await
    }

    /// 删除该 conv 的 session 行（core 的 /new 命令用）。
    pub async fn delete_session(&self, conv_id: &str) -> Result<()> {
        let conv_id = conv_id.to_string();
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            conn.execute("DELETE FROM sessions WHERE conv_id = ?1", rusqlite::params![conv_id])?;
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
        let (platform, account_id, buf) = (platform.to_string(), account_id.to_string(), buf.to_string());
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
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

    pub async fn get_context_token(&self, platform: &str, account_id: &str, peer: &str) -> Result<Option<String>> {
        let (platform, account_id, peer) =
            (platform.to_string(), account_id.to_string(), peer.to_string());
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

    pub async fn set_context_token(&self, platform: &str, account_id: &str, peer: &str, token: &str) -> Result<()> {
        let (platform, account_id, peer, token) = (
            platform.to_string(),
            account_id.to_string(),
            peer.to_string(),
            token.to_string(),
        );
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
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
        blocking_with(inner, move |conn| {
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
        blocking_with(inner, move |conn| {
            let n = conn.execute(
                "DELETE FROM allowed_senders WHERE sender = ?1",
                rusqlite::params![sender],
            )?;
            Ok(n > 0)
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
        blocking_with(inner, move |conn| {
            let now = now_secs();
            conn.execute(
                "INSERT INTO audit_log (ts, action, actor, target, detail) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![now, action, actor, target, detail],
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
        blocking_with(inner, move |conn| {
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
        blocking_with(inner, move |conn| {
            conn.execute("DELETE FROM config WHERE key = ?1", rusqlite::params![key])?;
            Ok(())
        })
        .await
    }

    // —— named_sessions（B1/B2：命名 session 侧表）——

    /// 插入或更新（按 conv_id + name）。created_at 仅新建时写入；更新时保留原 created_at、刷新 updated_at。
    pub async fn upsert_named_session(&self, row: &NamedSessionRow) -> Result<()> {
        let row = row.clone();
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
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

    pub async fn get_named_session(&self, conv_id: &str, name: &str) -> Result<Option<NamedSessionRow>> {
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

    pub async fn delete_named_session(&self, conv_id: &str, name: &str) -> Result<()> {
        let (conv_id, name) = (conv_id.to_string(), name.to_string());
        let inner = self.inner.clone();
        blocking_with(inner, move |conn| {
            conn.execute(
                "DELETE FROM named_sessions WHERE conv_id = ?1 AND name = ?2",
                rusqlite::params![conv_id, name],
            )?;
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

/// 打开连接、设 PRAGMA、跑迁移、收紧文件/目录权限。
fn open_and_setup(path: &Path) -> Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
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

    // 库文件 0600
    let md = std::fs::metadata(path)?;
    let mut perms = md.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;

    // 父目录 0700（defense-in-depth：仅当该目录归本进程所有时才有意义）。
    // 对系统目录（如 /tmp）无权 chmod，属正常情况；失败时仅告警、不阻断 open。
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Ok(parent_md) = std::fs::metadata(parent) {
                let mut pperms = parent_md.permissions();
                pperms.set_mode(0o700);
                if let Err(e) = std::fs::set_permissions(parent, pperms) {
                    tracing::warn!(
                        parent = %parent.display(),
                        error = %e,
                        "could not tighten parent dir to 0700 (best-effort)"
                    );
                }
            }
        }
    }
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
            "audit_log",
            "config",
            "context_tokens",
            "credentials",
            "named_sessions",
            "sessions",
            "sync_buf",
        ] {
            assert!(tables.iter().any(|x| x == t), "missing table: {t}");
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

        let got = store.get_session("ilink:user1").await.unwrap().expect("row");
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

        let got2 = store.get_session("ilink:user1").await.unwrap().expect("row2");
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
        assert!(store.get_credential("ilink", "acc1").await.unwrap().is_none());
        store.put_credential("ilink", "acc1", r#"{"token":"x"}"#).await.unwrap();
        assert_eq!(
            store.get_credential("ilink", "acc1").await.unwrap(),
            Some(r#"{"token":"x"}"#.to_string())
        );
        // 覆盖
        store.put_credential("ilink", "acc1", r#"{"token":"y"}"#).await.unwrap();
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

        store.put_credential("ilink", "acc1", r#"{"t":"1"}"#).await.unwrap();
        store.put_credential("ilink", "acc2", r#"{"t":"2"}"#).await.unwrap();

        // clone 后仍可用（验证 Store: Clone）
        let cloned = store.clone();
        let (account_id, blob) = cloned.first_credential("ilink").await.unwrap().expect("present");
        assert_eq!(blob, r#"{"t":"1"}"#);
        assert!(account_id == "acc1" || account_id == "acc2");

        // 其它 platform => None
        assert!(store.first_credential("wecom").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sync_buf_roundtrip() {
        let db = TempDb::new("syncbuf").await;
        let store = Store::open(&db.path).await.unwrap();
        assert!(store.get_sync_buf("ilink", "acc1").await.unwrap().is_none());
        store.set_sync_buf("ilink", "acc1", "cursor-1").await.unwrap();
        assert_eq!(
            store.get_sync_buf("ilink", "acc1").await.unwrap(),
            Some("cursor-1".into())
        );
        store.set_sync_buf("ilink", "acc1", "cursor-2").await.unwrap();
        assert_eq!(
            store.get_sync_buf("ilink", "acc1").await.unwrap(),
            Some("cursor-2".into())
        );
    }

    #[tokio::test]
    async fn context_token_roundtrip() {
        let db = TempDb::new("ctx").await;
        let store = Store::open(&db.path).await.unwrap();
        assert!(store.get_context_token("ilink", "acc1", "peer1").await.unwrap().is_none());
        store.set_context_token("ilink", "acc1", "peer1", "tok-1").await.unwrap();
        assert_eq!(
            store.get_context_token("ilink", "acc1", "peer1").await.unwrap(),
            Some("tok-1".into())
        );
        store.set_context_token("ilink", "acc1", "peer1", "tok-2").await.unwrap();
        assert_eq!(
            store.get_context_token("ilink", "acc1", "peer1").await.unwrap(),
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
        assert_eq!(after.added_by.as_deref(), Some("alice"), "重复 add 不覆盖 added_by");
        assert_eq!(after.source.as_deref(), Some("im"), "重复 add 不覆盖 source");

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

        store.append_audit("allow", Some("alice"), Some("bob"), Some("added")).await.unwrap();
        store.append_audit("disallow", Some("alice"), Some("bob"), None).await.unwrap();
        store.append_audit("allow", Some("cli"), Some("amy"), Some("cli-bootstrap")).await.unwrap();

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
        assert!(tables.iter().any(|x| x == "allowed_senders"), "v2 迁移应建 allowed_senders");
        assert!(tables.iter().any(|x| x == "audit_log"), "v2 迁移应建 audit_log");
        // 新表可写。
        store.add_allowed_sender("zoe", None, None).await.unwrap();
        assert_eq!(store.list_allowed_senders().await.unwrap(), vec!["zoe".to_string()]);
    }
    #[tokio::test]
    async fn config_kv_roundtrip() {
        let db = TempDb::new("config").await;
        let store = Store::open(&db.path).await.unwrap();

        // 不存在 → None。
        assert!(store.get_config("active_name:c1").await.unwrap().is_none());

        // set + get。
        store.set_config("active_name:c1", "refactor").await.unwrap();
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
        store.delete_named_session("ilink:u1", "docs").await.unwrap();
        assert!(
            store.get_named_session("ilink:u1", "docs").await.unwrap().is_none(),
            "删除后应不存在"
        );
        let list2 = store.list_named_sessions("ilink:u1").await.unwrap();
        assert_eq!(list2.len(), 1);

        // 其它 conv 隔离。
        assert!(
            store.list_named_sessions("ilink:u2").await.unwrap().is_empty(),
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
}
