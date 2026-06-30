//! 发送者白名单鉴权（双态）。
//!
//! 白名单非空时按白名单过滤；为空时进入「发现模式」（P1：只打日志记录入站
//! sender，不回 IM、不驱动 agent），便于首次使用时收集真实 sender id。

use std::collections::HashSet;

use crate::types::UserId;

#[derive(Debug, Clone)]
pub struct Auth {
    allowed: HashSet<String>,
}

impl Auth {
    /// 用配置的 `allowed_senders` 构造。空 vec = 发现模式。
    pub fn new(allowed_senders: Vec<String>) -> Self {
        Self {
            allowed: allowed_senders.into_iter().collect(),
        }
    }

    /// 白名单为空 => 发现模式。
    pub fn is_discovery(&self) -> bool {
        self.allowed.is_empty()
    }

    pub fn is_allowed(&self, uid: &UserId) -> bool {
        self.allowed.contains(&uid.0)
    }
}
