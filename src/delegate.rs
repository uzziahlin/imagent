//! 委托编排装配（v1.31）：`[[delegate]]` 条目 → 副后端实例表 →
//! [`imagent_core::DelegateApi`] 实现，注入 Dispatcher（`set_delegate`）。
//!
//! core 只定契约（依赖倒置——backend crate 依赖 core，构造副后端只能在 bin
//! 侧），本模块是唯一的装配点：`ops::apply_delegates`（启动）与 SIGHUP 共用
//! [`apply`]，整体重建委托表（one-shot 后端无长驻资源，重建即热改语义）。
//!
//! 安全设计（MVP）：
//! - **副 claude = Off 档 + legacy mcp 通道 + 空 RuntimeOpts**：无审批接线
//!   （headless claude 对未批工具直接拒绝——真机校准语义，即副轮的「危险操作
//!   拒绝」）；`-p <prompt>` 单发不建 ControlIo，副子进程不碰共享的
//!   permission.sock；不注入 extra_mcp / bitable / delegates——副 claude 看不到
//!   delegate_run（防递归委托）与主轮的用户工具面（隔离默认，文档声明）。
//! - **预算 = min(调用方期望, 条目上限)**：agent 只能调低不能放大。
//! - **全局并发闸**：同时在飞副轮上限 4（agent 并行 fan-out 防护，与 core 的
//!   max_concurrent_rounds 主轮护栏同量级取舍）。
//! - **输出截断**：头尾窗 24k chars——claude 工具结果载荷有限，超长整体丢弃
//!   会丢结论，头尾保窗 + 截断标记。

use std::collections::BTreeMap;
use std::sync::Arc;

use imagent_core::{Backend, Config, DelegateApi, DelegateDesc, DelegateRequest};

/// 全局并发闸：同时在飞的副轮上限（等待期间不影响其它委托名——按先来先得）。
const DELEGATE_MAX_CONCURRENT: usize = 4;

/// 副轮输出回传上限（Unicode chars）与头/尾保窗。
const OUTPUT_MAX_CHARS: usize = 24_000;
const OUTPUT_HEAD_CHARS: usize = 18_000;
const OUTPUT_TAIL_CHARS: usize = 6_000;

/// 单个委托的运行目标（config 条目的装配产物）。
struct DelegateTarget {
    backend: Arc<dyn Backend>,
    /// 副后端类型名（describe 展示）。
    agent: String,
    allowed_tools: Vec<String>,
    timeout_secs: u64,
}

/// `DelegateApi` 的主进程实现：名 → 目标路由 + one-shot run + 预算/并发/截断。
pub struct MainDelegateRunner {
    /// BTreeMap：describe 输出按名稳定排序（HashMap 遍历序随机，工具描述/命令
    /// 列表不该每次抖动）。
    targets: BTreeMap<String, DelegateTarget>,
    workdir: std::path::PathBuf,
    gate: Arc<tokio::sync::Semaphore>,
}

impl MainDelegateRunner {
    /// 从 config 构建委托表。条目 agent 值域已过 config 校验
    /// （codex/gemini/claude-cli）；此处防御性兜底——未知值记 error 跳过
    ///（而非 panic：单条坏配置不该带走整个启动）。
    fn build(config: &Config, sandbox_plan: &imagent_core::SandboxPlan) -> Self {
        let sandbox_effective = match sandbox_plan {
            imagent_core::SandboxPlan::Off => None,
            p => Some(p.clone()),
        };
        let targets = config
            .delegates
            .iter()
            .filter_map(|entry| {
                match build_target(
                    entry,
                    sandbox_effective.clone(),
                    config.agent_sandbox_extra_write.clone(),
                ) {
                    Ok(t) => Some((entry.name.clone(), t)),
                    Err(e) => {
                        tracing::error!(
                            target: "imagent::delegate",
                            delegate = %entry.name,
                            agent = %entry.agent,
                            error = %e,
                            "委托条目装配失败，跳过（config 校验应已拦截，属防御兜底）"
                        );
                        None
                    }
                }
            })
            .collect();
        Self {
            targets,
            workdir: config.default_workdir.clone(),
            gate: Arc::new(tokio::sync::Semaphore::new(DELEGATE_MAX_CONCURRENT)),
        }
    }
}

/// 单条目 → 副后端实例。三系 one-shot 后端的隔离缺省见模块文档。
/// `sandbox`（v1.32）：仅 claude 系副后端注入（codex/gemini 原生沙箱不叠加）。
fn build_target(
    entry: &imagent_core::DelegateEntry,
    sandbox: Option<imagent_core::SandboxPlan>,
    sandbox_extra_write: Vec<std::path::PathBuf>,
) -> anyhow::Result<DelegateTarget> {
    let backend: Arc<dyn Backend> = match entry.agent.as_str() {
        "codex" => {
            let b = imagent_codex::CodexBackend::new();
            b.set_model(entry.model.clone());
            Arc::new(b)
        }
        "gemini" => {
            let b = imagent_gemini::GeminiBackend::new();
            b.set_model(entry.model.clone());
            Arc::new(b)
        }
        "claude-cli" => {
            // Off 档 + legacy mcp 通道（`-p <prompt>` 单发）：无审批接线（headless
            // claude 对未批工具直接拒绝）、不建 ControlIo（不碰共享 permission.sock）、
            // RuntimeOpts 保持 default（无用户 MCP / bitable / delegates——防递归）。
            let b = imagent_claude::ClaudeBackend::with_permission_mode(
                imagent_core::PermissionMode::Off,
            );
            b.set_permission_channel("mcp");
            b.set_model(entry.model.clone());
            // v1.32：与主后端同款沙箱（mcp 通道 + 空 RuntimeOpts 下 Off 档无
            // mcp json 例外需求——profile 例外段对不存在的路径无害）。
            b.set_sandbox(sandbox, sandbox_extra_write);
            Arc::new(b)
        }
        other => {
            return Err(anyhow::anyhow!(
                "未知副后端 {other:?}（可用 codex | gemini | claude-cli）"
            ));
        }
    };
    Ok(DelegateTarget {
        backend,
        agent: entry.agent.clone(),
        allowed_tools: entry.allowed_tools.clone(),
        timeout_secs: entry.timeout_secs,
    })
}

/// 头尾窗截断：超长输出保留头部 + 尾部（结论通常在尾部），中段标记省略量。
fn truncate_output(text: &str) -> String {
    let total = text.chars().count();
    if total <= OUTPUT_MAX_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(OUTPUT_HEAD_CHARS).collect();
    let tail: String = text
        .chars()
        .skip(total.saturating_sub(OUTPUT_TAIL_CHARS))
        .collect();
    format!(
        "{head}\n\n…[输出超长，中段截断约 {} 字符]…\n\n{tail}",
        total - OUTPUT_HEAD_CHARS - OUTPUT_TAIL_CHARS
    )
}

#[async_trait::async_trait]
impl DelegateApi for MainDelegateRunner {
    async fn run(&self, req: &DelegateRequest) -> std::result::Result<String, String> {
        let Some(target) = self.targets.get(&req.name) else {
            let names: Vec<&str> = self.targets.keys().map(String::as_str).collect();
            return Err(format!(
                "未配置的委托名 {:?}（可用：{}）",
                req.name,
                names.join(" | ")
            ));
        };
        // 生效预算 = min(调用方期望, 条目上限)——缺省用条目值；agent 只能调低。
        let budget_secs = req
            .timeout_secs
            .unwrap_or(target.timeout_secs)
            .min(target.timeout_secs);
        let budget = std::time::Duration::from_secs(budget_secs);

        // 并发闸：acquire 无限期等待（预算超时兜底防永挂——超时 drop 掉的等待
        // 自动释放队列位）。permit 持有到 run 结束。
        let permit = self
            .gate
            .acquire()
            .await
            .map_err(|_| "委托并发闸已关闭（主进程退出中）".to_string())?;

        tracing::info!(
            target: "imagent::delegate",
            delegate = %req.name,
            agent = %target.agent,
            budget_secs = budget_secs,
            "委托副轮开始"
        );

        // chunks 消费：run() 返回前持续排空防背压阻塞；Text 累积作兜底
        //（RunOutcome.final_text 为空的非正常终止路径仍有部分输出可回）。
        let (chunks_tx, mut chunks_rx) = tokio::sync::mpsc::channel(64);
        let texts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let drain_texts = texts.clone();
        let drain = tokio::spawn(async move {
            while let Some(chunk) = chunks_rx.recv().await {
                if let imagent_core::AgentChunk::Text(t) = chunk {
                    drain_texts
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(t);
                }
            }
        });
        // steer 通道立即丢 sender：副轮不接受运行中转向（一次性语义）。
        let (_steer_tx, steer_rx) = tokio::sync::mpsc::channel::<String>(1);
        drop(_steer_tx);

        let run_fut = target.backend.run(
            // conv_id 仅用于后端内日志/MCP 标注；副后端 Off 档不挂审批，
            // 不会经它路由任何 IM 消息（见模块文档的隔离设计）。
            "delegate",
            &req.prompt,
            None,
            &self.workdir,
            &target.allowed_tools,
            chunks_tx,
            &[],
            steer_rx,
        );
        let outcome = tokio::time::timeout(budget, run_fut).await;
        // run 结束（或超时被 drop）→ chunks_tx 已释放 → drain 自然收尾。
        let _ = drain.await;
        drop(permit);

        match outcome {
            Ok(Ok(o)) => {
                let text = if o.final_text.is_empty() {
                    texts.lock().unwrap_or_else(|e| e.into_inner()).join("\n\n")
                } else {
                    o.final_text
                };
                if text.trim().is_empty() {
                    return Err(format!(
                        "委托 {:?} 副轮无输出（{} 非正常终止；检查该后端 CLI 是否安装/登录）",
                        req.name, target.agent
                    ));
                }
                let prefix = if o.terminal {
                    String::new()
                } else {
                    "⚠️ 副轮非正常终止，以下为已收到的部分输出：\n\n".to_string()
                };
                tracing::info!(
                    target: "imagent::delegate",
                    delegate = %req.name,
                    agent = %target.agent,
                    terminal = o.terminal,
                    chars = text.chars().count(),
                    "委托副轮完成"
                );
                Ok(format!("{prefix}{}", truncate_output(&text)))
            }
            Ok(Err(e)) => Err(format!("委托 {:?} 副轮运行失败：{e}", req.name)),
            Err(_) => Err(format!(
                "委托 {:?} 运行超时（>{budget_secs}s，副进程已中止）——可调大 \
                 [[delegate]] 的 timeout_secs 或拆小子任务",
                req.name
            )),
        }
    }

    fn describe(&self) -> Vec<DelegateDesc> {
        self.targets
            .iter()
            .map(|(name, t)| DelegateDesc {
                name: name.clone(),
                agent: t.agent.clone(),
                tools: if imagent_core::backend_common::tools_unrestricted(&t.allowed_tools) {
                    "全量".to_string()
                } else {
                    t.allowed_tools.join(",")
                },
            })
            .collect()
    }
}

/// 启动 / SIGHUP 共用的装配入口：重建委托表注入 Dispatcher。
/// 空表 = 显式置 None（SIGHUP 删光条目的语义——后续调用回「未配置」）。
/// `[[delegate]]` 非空而主后端非 claude-cli 时 warn（delegate_run 工具面只随
/// claude-cli 的 mcp 配置挂载；`/delegate` 命令仍可用——路由不依赖主后端）。
/// `sandbox_plan`（v1.32）：claude 副后端与主后端同注一份（Off 自然裸跑）。
pub(crate) fn apply(
    dispatcher: &imagent_core::Dispatcher,
    config: &Config,
    sandbox_plan: &imagent_core::SandboxPlan,
) {
    if config.delegates.is_empty() {
        dispatcher.set_delegate(None);
        return;
    }
    if config.agent != "claude-cli" {
        tracing::warn!(
            target: "imagent::delegate",
            agent = %config.agent,
            "[[delegate]] 已配置但主后端非 claude-cli：delegate_run MCP 工具不挂载 \
             （仅 claude-cli）；/delegate 命令仍可用"
        );
    }
    let runner = MainDelegateRunner::build(config, sandbox_plan);
    let descs = runner.describe();
    dispatcher.set_delegate(Some(Arc::new(runner)));
    tracing::info!(
        target: "imagent::delegate",
        delegates = ?descs.iter().map(|d| format!("{}({})", d.name, d.agent)).collect::<Vec<_>>(),
        "委托编排已启用（claude-cli agent 获得 delegate_run 工具；/delegate 命令可用）"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 截断头尾窗：短文原样；超长保头尾 + 标记中段省略量。
    #[test]
    fn truncate_output_head_tail_window() {
        let short = "hello".repeat(100);
        assert_eq!(truncate_output(&short), short, "短文不截断");

        let long = format!(
            "{}{}{}",
            "a".repeat(OUTPUT_HEAD_CHARS),
            "M".repeat(6_000),
            "z".repeat(OUTPUT_TAIL_CHARS + 500)
        );
        let out = truncate_output(&long);
        assert!(out.contains("输出超长"), "应带截断标记");
        assert!(out.starts_with(&"a".repeat(100)), "头部保窗");
        assert!(out.ends_with(&"z".repeat(100)), "尾部保窗");
        // 头 18k + 尾 6k = 24k 上限内 + 标记行。
        assert!(out.chars().count() < OUTPUT_MAX_CHARS + 200);
    }

    /// 空表 build 出的 runner：run 回未配置（带空名单文案）、describe 为空。
    #[tokio::test]
    async fn empty_runner_reports_unconfigured() {
        let r = MainDelegateRunner::build(&test_config(""), &imagent_core::SandboxPlan::Off);
        assert!(r.describe().is_empty());
        let err = r
            .run(&DelegateRequest {
                name: "x".into(),
                prompt: "y".into(),
                timeout_secs: None,
            })
            .await
            .unwrap_err();
        assert!(err.contains("未配置的委托名"), "{err}");
    }

    /// 三系合法 agent 均可装配；未知 agent 跳过（防御兜底，config 已拦）。
    #[test]
    fn build_targets_for_all_one_shot_backends() {
        let mk = |agent: &str| imagent_core::DelegateEntry {
            name: format!("d-{agent}"),
            agent: agent.into(),
            allowed_tools: vec!["Read".into()],
            model: None,
            timeout_secs: 60,
        };
        for agent in ["codex", "gemini", "claude-cli"] {
            assert!(
                build_target(&mk(agent), None, Vec::new()).is_ok(),
                "{agent} 应可装配"
            );
        }
        assert!(
            build_target(&mk("acp"), None, Vec::new()).is_err(),
            "acp 应被拒（防御）"
        );

        let cfg = test_config(
            r#"
[[delegate]]
name = "d-codex"
agent = "codex"
allowed_tools = ["Read"]
timeout_secs = 60

[[delegate]]
name = "d-claude-cli"
agent = "claude-cli"
allowed_tools = ["Read"]
timeout_secs = 60
"#,
        );
        let r = MainDelegateRunner::build(&cfg, &imagent_core::SandboxPlan::Off);
        let descs = r.describe();
        assert_eq!(descs.len(), 2);
        assert_eq!(descs[0].name, "d-claude-cli", "按名稳定排序");
        assert_eq!(descs[0].agent, "claude-cli");
        assert_eq!(descs[1].name, "d-codex");
        assert_eq!(descs[1].tools, "Read");
    }

    /// 测试用 Config（临时文件 + `Config::load`——与 core config 测试同款，
    /// 顺带覆盖 load 校验路径；条目按合法形态书写）。
    fn test_config(entries_toml: &str) -> Config {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "imagent_bin_delegate_{}_{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let raw = format!("default_workdir = \"/tmp/ws\"\n{entries_toml}");
        std::fs::write(&p, raw).expect("写测试 config");
        let cfg = Config::load(&p).expect("测试 config 应可加载");
        let _ = std::fs::remove_file(&p);
        cfg
    }
}
