//! core 错误类型。

use thiserror::Error;

#[derive(Error, Debug)]
pub enum CoreError {
    #[error("store: {0}")]
    Store(#[from] imagent_store::StoreError),
    #[error("platform({0}): {1}")]
    Platform(&'static str, String),
    #[error("backend({0}): {1}")]
    Backend(&'static str, String),
    #[error("config: {0}")]
    Config(String),
    /// 会话过期（需重新登录）。专用 variant，避免靠 Display 子串匹配判定。
    #[error("session expired: {0}")]
    SessionExpired(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CoreError>;
