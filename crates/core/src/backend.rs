//! Agent 后端抽象 trait。

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::Result;
use crate::types::{AgentChunk, LocalSession, RunOutcome, SessionId};

/// B3：backend 的权限审批能力档位（能力协商，dispatcher 启动时校验）。
///
/// - `FullLoop`：完整 IM 审批闭环（审批卡进 IM、等 y/n、超时 deny）——claude-cli
///   （MCP 回调）与 claude-acp（session/request_permission 经 [`ImPermissionHook`]
///   进 IM）；
/// - `NativeOnly`：只透传原生 CLI 档位（无 IM 闭环，但有原生 approval 参数可
///   映射 allowed_tools 收敛）——gemini（`--approval-mode`）；
/// - `Unsupported`：既无 IM 闭环也无可用原生审批参数——codex（exec 模式无
///   `--ask-for-approval`，仅有拒绝映射的 bypass 参数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionCapability {
    /// IM 审批闭环（`permission_mode = ask/auto-claude` 可用）。
    FullLoop,
    /// 仅原生 CLI 档位透传（ask 档不可用）。
    NativeOnly,
    /// 无权限审批能力（ask 档不可用）。
    Unsupported,
}

impl PermissionCapability {
    /// 能力矩阵日志/错误信息用的短名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FullLoop => "full-loop(IM 审批闭环)",
            Self::NativeOnly => "native-only(仅原生 CLI 档位)",
            Self::Unsupported => "unsupported(无审批能力)",
        }
    }
}

/// B3：backend → dispatcher 的 IM 审批请求（与 claude-cli MCP 闭环同一语义通道；
/// ACP 的 `session/request_permission` 映射到此处）。
#[derive(Debug, Clone)]
pub struct ImPermissionAsk {
    /// 审批卡要投递的 IM 会话。
    pub conv_id: String,
    /// 路由用请求 id（router pending key）。
    pub request_id: String,
    /// 工具名（审批卡标题）。
    pub tool_name: String,
    /// 工具输入摘要（审批卡正文，调用方负责截断）。
    pub input_summary: String,
}

/// IM 审批回调：返回 `true` = 放行（选 allow 类选项），`false` = 拒绝/超时（deny）。
/// dispatcher 注入（复用 PermissionRouter + platform 发卡），backend 在权限请求
/// 到达时调用并阻塞等待用户表态。
pub type ImPermissionHook =
    Arc<dyn Fn(ImPermissionAsk) -> futures::future::BoxFuture<'static, bool> + Send + Sync>;

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

    /// W1-2：运行时模型选择（`/model` 命令热设；None = backend/CLI 自身默认）。
    /// 进程内生效，重启/SIGHUP 恢复为 config 基准值（main 注入）。默认 no-op
    /// （不支持的后端忽略）；claude-cli（`--model`）与 claude-acp
    /// （ANTHROPIC_MODEL env）覆写。
    fn set_model(&self, _model: Option<String>) {}

    /// W1-2：当前生效的运行时模型（`/model` 展示用；None = 默认模型）。
    fn model(&self) -> Option<String> {
        None
    }

    /// W1-2：是否支持模型选择（不支持的后端 `/model` 回明确提示而非静默无效）。
    fn supports_model_selection(&self) -> bool {
        false
    }

    /// B3：权限审批能力声明（能力协商）。dispatcher 启动时校验：
    /// `permission_mode = ask/auto-claude`（needs_socket 闭环类档）而本能力非
    /// `FullLoop` 时 fail-closed 拒绝启动。默认 `Unsupported`，各 backend
    /// 如实覆写（claude-cli / claude-acp = FullLoop，gemini = NativeOnly）。
    fn permission_capability(&self) -> PermissionCapability {
        PermissionCapability::Unsupported
    }

    /// B3：注入 IM 审批闭环回调（dispatcher `run()` 启动时调用一次）。
    /// 默认 no-op（无闭环的后端忽略）；claude-acp 接线后覆写。
    fn set_im_permission_hook(&self, _hook: Option<ImPermissionHook>) {}

    /// 列出与该 workdir 同项目的本机会话（P4-11 统一 `/resume`：电脑端开的
    /// agent 会话与 IM 会话合并展示）。默认空——无本机存储概念的 backend
    /// （codex/gemini）不参与合并，`/resume` 自动退化为纯 IM 历史。
    async fn list_local_sessions(&self, _workdir: &Path) -> Vec<LocalSession> {
        Vec::new()
    }

    /// W4-2：会话转录导出为 Markdown（`/export` 数据源）。返回 None = 该后端
    /// 不支持（codex/gemini 无本机可读存储或未接）。默认 None；claude 系覆写
    /// （`~/.claude/projects/<编码>/<id>.jsonl` → 人读对话转录）。
    async fn export_session_markdown(&self, _workdir: &Path, _session_id: &str) -> Option<String> {
        None
    }

    /// 进程退出前的显式清理（dispatcher.run() 返回后由 main 调用一次）。
    /// 默认 no-op；持有长驻资源的后端覆写（如 claude-acp 断开全部 per-conv
    /// 连接、kill ACP 子进程）。此前仅靠 Drop 兜底——Arc 泄漏/静态持有场景
    /// 下子进程会活到 OS 清理，显式接线让退出路径确定。
    async fn shutdown(&self) {}
}
