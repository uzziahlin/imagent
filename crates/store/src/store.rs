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
        for t in ["config", "context_tokens", "credentials", "sessions", "sync_buf"] {
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
}
