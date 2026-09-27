//! `imagent-store`：SQLite 持久化层。
//!
//! 最底层 crate，不依赖项目内任何其他 crate。被 `core`（会话路由）和
//! `ilink`（协议状态 sync_buf / context_tokens / credentials）共享。
//!
//! 异步模型：rusqlite 是同步阻塞 API，内部持 `Arc<parking_lot::Mutex<Connection>>`，
//! 每个 `async` 方法的 DB 操作用 `tokio::task::spawn_blocking` 包裹。锁 guard
//! 只存活于 blocking 线程内，绝不跨 `.await`。

#![forbid(unsafe_code)]

mod credentials;
mod crypto;
mod error;
mod schema;
mod store;

pub use error::{Result, StoreError};
pub use store::{
    AllowedSenderRow, AuditRow, CronJobRow, LiveCardRow, NamedSessionRow, OutboxRow, RunStatRow,
    SessionHistoryRow, SessionRow, Store, OUTBOX_MAX_ATTEMPTS,
};

/// bundled SQLite 编译期版本（libsqlite3-sys 的 `SQLITE_VERSION` 常量）。
///
/// bundled feature 下 SQLite 随 libsqlite3-sys 源码内嵌编译，编译期版本即运行时
/// 版本；暴露给 `/doctor` 展示（T16：内嵌 C 代码的 CVE 不进 cargo-audit，部署侧
/// 只能从这里观测实际 SQLite 版本）。
pub fn sqlite_version() -> &'static str {
    rusqlite::version()
}
