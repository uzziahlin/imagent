//! DDL 与迁移。
//!
//! 用 `PRAGMA user_version` 做简单线性迁移：v1 = 建 5 张基础表，v2 = 动态白名单 + 审计日志。

/// v1 全部建表语句（`CREATE TABLE IF NOT EXISTS`，可重复执行）。
pub const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS credentials (
  platform   TEXT NOT NULL,
  account_id TEXT NOT NULL,
  blob       TEXT NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (platform, account_id)
);

CREATE TABLE IF NOT EXISTS sessions (
  conv_id    TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  agent_kind TEXT NOT NULL,
  workdir    TEXT NOT NULL,
  name       TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sync_buf (
  platform   TEXT NOT NULL,
  account_id TEXT NOT NULL,
  buf        TEXT NOT NULL,
  PRIMARY KEY (platform, account_id)
);

CREATE TABLE IF NOT EXISTS context_tokens (
  platform   TEXT NOT NULL,
  account_id TEXT NOT NULL,
  peer       TEXT NOT NULL,
  token      TEXT NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (platform, account_id, peer)
);

CREATE TABLE IF NOT EXISTS config (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;
/// v2：动态白名单 + 审计日志（C1）。
pub const SCHEMA_V2: &str = r#"
CREATE TABLE IF NOT EXISTS allowed_senders (
  sender    TEXT PRIMARY KEY,
  added_at  INTEGER NOT NULL,
  added_by  TEXT,
  source    TEXT
);

CREATE TABLE IF NOT EXISTS audit_log (
  id     INTEGER PRIMARY KEY AUTOINCREMENT,
  ts     INTEGER NOT NULL,
  action TEXT NOT NULL,
  actor  TEXT,
  target TEXT,
  detail TEXT
);
"#;

/// 在已打开的连接上跑线性迁移。幂等：逐版本推进（v1→v2→…），已到目标版本则跳过。
pub fn migrate(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if current < 1 {
        tracing::info!(current, "running store schema migration to v1");
        conn.execute_batch(SCHEMA_V1)?;
        conn.pragma_update(None, "user_version", 1_i64)?;
    } else {
        tracing::debug!(current, "store schema already >= v1");
    }
    if current < 2 {
        tracing::info!(current, "running store schema migration to v2");
        conn.execute_batch(SCHEMA_V2)?;
        conn.pragma_update(None, "user_version", 2_i64)?;
    }
    Ok(())
}
