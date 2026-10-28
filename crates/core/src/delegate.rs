//! 委托编排抽象（v1.31）：主 agent / 用户把子任务 fan-out 给命名副后端
//! one-shot 执行。
//!
//! 链路（与 bitable 数据面同构的第三条 socket 通路）：
//! - agent 侧：claude-cli 经 `imagent mcp` stdio server 的 `delegate_run` 工具
//!   → permission unix socket（`kind=delegate`）→ Dispatcher → 本 trait；
//! - 用户侧：`/delegate <name> <prompt>` 斜杠命令直调本 trait（不经 socket）。
//!
//! core 只定契约不实现（依赖倒置——副后端的具体构造需要 claude/codex/gemini
//! crate，它们依赖 core，反向不可；由 main 装配注入，见 bin 侧
//! `MainDelegateRunner`）。错误用 `String`（agent / 用户可直读文案）。
//!
//! 安全缺省（MVP）：副轮无 IM 审批闭环——各后端按 Off 档语义处理权限请求
//! （claude-cli 直接拒危险工具、codex 落 read-only 沙箱档），工具面由
//! `[[delegate]]` 的 `allowed_tools`（缺省只读三件套）收敛。超时取
//! 「工具请求值与条目上限的较小者」——agent 不能自行放大预算。

use serde_json::Value;

/// socket 路由侧「未注入 DelegateApi」的回错文案（配置引导，钉住供测试断言）。
pub const NOT_CONFIGURED_MSG: &str =
    "delegate 未启用：config.toml 未配置 [[delegate]] 条目（每条 = name + agent；详见 \
     config.toml 注释；SIGHUP 或重启生效）";

/// 单次委托运行的请求参数（socket `kind=delegate` 报文 / `/delegate` 命令共用）。
#[derive(Debug, Clone, PartialEq)]
pub struct DelegateRequest {
    /// 目标委托名（`[[delegate]].name`）。
    pub name: String,
    /// 子任务正文（作为副后端 one-shot run 的 prompt）。
    pub prompt: String,
    /// 调用方期望的预算（秒）。生效值 = min(本值, 条目上限)——只能调低；
    /// None = 用条目缺省。
    pub timeout_secs: Option<u64>,
}

/// 委托名与一句话描述（工具描述 / `/delegate list` 展示用；`agent` 是副后端
/// 类型串，`tools` 是生效工具面摘要）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegateDesc {
    pub name: String,
    pub agent: String,
    pub tools: String,
}

/// 委托运行契约：按名执行一次副后端 one-shot run，返回最终文本。
///
/// 实现方负责：名路由（未知名回 Err 并列出可用名）、并发闸、总预算超时
/// （超时杀子进程——依赖 backend spawn 的 kill_on_drop）、输出截断。
#[async_trait::async_trait]
pub trait DelegateApi: Send + Sync {
    /// 执行一次委托。`Ok(text)` = 副 agent 最终回复（已截断）；`Err(msg)` =
    /// 可直读错误（未配置 / 未知名 / 超时 / 运行失败——调用方原样透出）。
    async fn run(&self, req: &DelegateRequest) -> std::result::Result<String, String>;

    /// 已配置的委托清单（空 = 未启用；工具描述与命令列表用）。
    fn describe(&self) -> Vec<DelegateDesc>;
}

/// 解析 socket `kind=delegate` 报文（纯函数，便于单测）。`Ok(None)` = 报文
/// 缺必填字段（name/prompt 空白）——调用方按参数错误回写。
pub fn parse_delegate_request(req: &Value) -> Option<DelegateRequest> {
    let name = req.get("name").and_then(|v| v.as_str())?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let prompt = req
        .get("prompt")
        .and_then(|v| v.as_str())?
        .trim()
        .to_string();
    if prompt.is_empty() {
        return None;
    }
    let timeout_secs = req
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .filter(|s| *s > 0);
    Some(DelegateRequest {
        name,
        prompt,
        timeout_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 报文解析：name/prompt 必填（空白即拒），timeout_secs 可选（0/缺失=None）。
    #[test]
    fn parse_delegate_request_shape() {
        let req = parse_delegate_request(&json!({
            "kind": "delegate",
            "conv_id": "feishu:ou_x",
            "name": " review ",
            "prompt": " 检查最近一次提交 ",
            "timeout_secs": 300,
        }))
        .expect("完整报文应解析");
        assert_eq!(req.name, "review", "name 应 trim");
        assert_eq!(req.prompt, "检查最近一次提交");
        assert_eq!(req.timeout_secs, Some(300));

        assert_eq!(
            parse_delegate_request(&json!({"name": "a", "prompt": "  "})),
            None,
            "空白 prompt 应拒"
        );
        assert_eq!(
            parse_delegate_request(&json!({"name": " ", "prompt": "x"})),
            None,
            "空白 name 应拒"
        );
        assert_eq!(
            parse_delegate_request(&json!({"prompt": "x"})),
            None,
            "缺 name 应拒"
        );
        // 0 视为未设置（socket 侧语义 = 用条目缺省），不是非法。
        assert_eq!(
            parse_delegate_request(&json!({"name": "a", "prompt": "x", "timeout_secs": 0}))
                .unwrap()
                .timeout_secs,
            None
        );
    }

    /// 未配置文案钉住（socket 分支与命令分支共用，防漂移）。
    #[test]
    fn not_configured_msg_mentions_config() {
        assert!(
            NOT_CONFIGURED_MSG.contains("[[delegate]]"),
            "{NOT_CONFIGURED_MSG}"
        );
    }
}
