//! Agent 后端抽象 trait。

use std::path::Path;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::Result;
use crate::types::{AgentChunk, LocalSession, RunOutcome, SessionId};

/// agent 后端抽象（无状态执行器）。
///
/// core 传入 `session`（续接）或 `None`（新建），Backend 执行并流式产出
/// chunks，返回 `RunOutcome`（含本次 session_id）。由 `claude` 等 crate 实现，
/// 注入到 `Dispatcher`。
#[async_trait]
pub trait Backend: Send + Sync {
    /// 执行一次 agent 调用。
    ///
    /// - `conv_id`：当前会话标识（backend 用它给 MCP server 子进程标注当前 conv，
    ///   以便权限审批路由回正确的 IM 会话）；
    /// - `prompt`：用户文本；
    /// - `session`：`None` 新建，`Some(id)` 续接已存在会话；
    /// - `workdir`：agent 工作根目录（cwd，非沙箱）；
    /// - `allowed_tools`：允许的工具白名单（如 `["Read","Edit"]`）；
    /// - `chunks`：流式分块通道，core 消费。
    async fn run(
        &self,
        conv_id: &str,
        prompt: &str,
        session: Option<&SessionId>,
        workdir: &Path,
        allowed_tools: &[String],
        chunks: mpsc::Sender<AgentChunk>,
    ) -> Result<RunOutcome>;

    /// agent 类型，如 `"claude-cli"`。
    fn name(&self) -> &'static str;

    /// P8-4：backend 原生权限模式透传覆盖（如 claude 的 `--permission-mode`）。
    /// None = 按档缺省（见各 backend）；Some = 显式值。默认 no-op（不支持原生
    /// 透传的后端忽略）；SIGHUP 热重载调用。
    fn set_native_permission_mode(&self, _mode: Option<String>) {}

    /// P8-4：是否支持原生权限模式透传（main 据此对「已配置但不支持」warn，
    /// 不静默忽略）。默认 false；claude-cli 覆写 true。
    fn supports_native_permission_mode(&self) -> bool {
        false
    }

    /// 列出与该 workdir 同项目的本机会话（P4-11 统一 `/resume`：电脑端开的
    /// agent 会话与 IM 会话合并展示）。默认空——无本机存储概念的 backend
    /// （codex/gemini）不参与合并，`/resume` 自动退化为纯 IM 历史。
    async fn list_local_sessions(&self, _workdir: &Path) -> Vec<LocalSession> {
        Vec::new()
    }
}
