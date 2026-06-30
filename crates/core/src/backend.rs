//! Agent 后端抽象 trait。

use std::path::Path;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::Result;
use crate::types::{AgentChunk, RunOutcome, SessionId};

/// agent 后端抽象（无状态执行器）。
///
/// core 传入 `session`（续接）或 `None`（新建），Backend 执行并流式产出
/// chunks，返回 `RunOutcome`（含本次 session_id）。由 `claude` 等 crate 实现，
/// 注入到 `Dispatcher`。
#[async_trait]
pub trait Backend: Send + Sync {
    /// 执行一次 agent 调用。
    ///
    /// - `prompt`：用户文本；
    /// - `session`：`None` 新建，`Some(id)` 续接已存在会话；
    /// - `workdir`：agent 工作根目录（安全边界）；
    /// - `allowed_tools`：允许的工具白名单（如 `["Read","Edit"]`）；
    /// - `chunks`：流式分块通道，core 消费。
    async fn run(
        &self,
        prompt: &str,
        session: Option<&SessionId>,
        workdir: &Path,
        allowed_tools: &[String],
        chunks: mpsc::Sender<AgentChunk>,
    ) -> Result<RunOutcome>;

    /// agent 类型，如 `"claude-cli"`。
    fn name(&self) -> &'static str;
}
