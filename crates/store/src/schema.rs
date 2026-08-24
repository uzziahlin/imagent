//! DDL 与迁移。
//!
//! 用 `PRAGMA user_version` 做简单线性迁移：v1 = 建 5 张基础表，v2 = 动态白名单 + 审计日志。

/// 当前代码支持的最新 schema 版本（migrate 上限 + user_version 过新拒绝阈值，P2-O）。
pub const SCHEMA_VERSION: i64 = 7;

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

/// v3：命名 session 侧表（B1/B2）。
///
/// `sessions` 表保持 conv_id PK（一对一 = 当前活动 session）不变；
/// 多命名 session 的历史/命名集合由本表承担，PK = (conv_id, name)。
pub const SCHEMA_V3: &str = r#"
CREATE TABLE IF NOT EXISTS named_sessions (
  conv_id    TEXT NOT NULL,
  name       TEXT NOT NULL,
  session_id TEXT NOT NULL,
  agent_kind TEXT,
  workdir    TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (conv_id, name)
);
"#;

/// v4：会话（群）白名单（P4-5）。存 conv_id 原样（平台无关，如 `feishu:oc_xxx`），
/// 与 `allowed_senders`（人维度）互补：群消息「chat 放行 OR sender 放行」即过。
pub const SCHEMA_V4: &str = r#"
CREATE TABLE IF NOT EXISTS allowed_chats (
  conv_id  TEXT PRIMARY KEY,
  added_at INTEGER NOT NULL,
  added_by TEXT,
  source   TEXT
);
"#;

/// v5：session 历史侧表（P4-8 `/resume`）。`sessions` 表只存每 conv 当前活动
/// session（upsert 覆盖），本表记每个出现过的 session_id 供 IM 内恢复。
pub const SCHEMA_V5: &str = r#"
CREATE TABLE IF NOT EXISTS session_history (
  conv_id    TEXT NOT NULL,
  session_id TEXT NOT NULL,
  agent_kind TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (conv_id, session_id)
);
"#;

/// v6：在飞流式卡片登记（P4_ROADMAP 第六批「孤儿卡片关流」）。卡片首帧发出时
/// upsert 本表；终态 patch 成功后删除。进程崩溃/重启后启动扫描据此把滞留在
/// 「生成中」的卡片 patch 成「已中断」终态。每 conv 至多一张在飞卡片（轮次串行）。
pub const SCHEMA_V6: &str = r#"
CREATE TABLE IF NOT EXISTS live_cards (
  conv_id    TEXT PRIMARY KEY,
  platform   TEXT NOT NULL,
  handle     TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
"#;

/// v7：管理员动态白名单（P7-A1，`/admin add|remove`）。与 config 的
/// `admin_senders` 种子取并集；结构对齐 allowed_senders（人维度）。
pub const SCHEMA_V7: &str = r#"
CREATE TABLE IF NOT EXISTS admin_senders (
  sender    TEXT PRIMARY KEY,
  added_at  INTEGER NOT NULL,
  added_by  TEXT,
  source    TEXT
);
"#;

/// 在已打开的连接上跑线性迁移。幂等：逐版本推进（v1→v2→…），已到目标版本则跳过。
pub fn migrate(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    // P2-O：拒绝比代码更新的 user_version（旧代码跑新 DB 可能丢迁移 / 数据不一致）。
    if current > SCHEMA_VERSION {
        return Err(rusqlite::Error::ToSqlConversionFailure(
            format!(
                "store user_version={current} 比代码支持({SCHEMA_VERSION})新，拒绝（旧代码跑新 DB 风险）"
            )
            .into(),
        ));
    }
    if current >= SCHEMA_VERSION {
        tracing::debug!(current, "store schema already at latest version");
        return Ok(());
    }
    // P2-N：整体迁移包在事务内，失败回滚（避免半迁移状态不一致）。
    tracing::info!(target: "store", current, goal = SCHEMA_VERSION, "running store schema migration");
    let tx = conn.unchecked_transaction()?;
    if current < 1 {
        tx.execute_batch(SCHEMA_V1)?;
        tx.pragma_update(None, "user_version", 1_i64)?;
    }
    if current < 2 {
        tx.execute_batch(SCHEMA_V2)?;
        tx.pragma_update(None, "user_version", 2_i64)?;
    }
    if current < 3 {
        tx.execute_batch(SCHEMA_V3)?;
        tx.pragma_update(None, "user_version", 3_i64)?;
    }
    if current < 4 {
        tx.execute_batch(SCHEMA_V4)?;
        tx.pragma_update(None, "user_version", 4_i64)?;
    }
    if current < 5 {
        tx.execute_batch(SCHEMA_V5)?;
        tx.pragma_update(None, "user_version", 5_i64)?;
    }
    if current < 6 {
        tx.execute_batch(SCHEMA_V6)?;
        tx.pragma_update(None, "user_version", 6_i64)?;
    }
    if current < 7 {
        tx.execute_batch(SCHEMA_V7)?;
        tx.pragma_update(None, "user_version", 7_i64)?;
    }
    tx.commit()?;
    Ok(())
}
