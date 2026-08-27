# imagent

> **Instant messaging, meet your agent.**
> 一个用 Rust 写的、把即时通讯平台接入自主 agent 的网关。平台与后端双抽象：任何 IM（飞书 Feishu / 企业微信 WeCom / 个人微信 iLink）↔ 任何 agent（Claude Code / Codex / Gemini）。

---

## 你正在做什么（重开会话 onboarding）

这是一个 **Rust 写的 IM ↔ agent 网关**，8 个 crate 的 workspace（`core` 调度/鉴权/权限 / `feishu`+`wecom`+`ilink` 三平台 / `claude`（CLI+ACP）+`codex`+`gemini` 四后端 / `store` SQLite 持久化 + 二进制 `src/`）。

开始写代码前，**必须先读**：
1. **`docs/ARCHITECTURE.md`** —— 当前架构总览（crate 划分、数据流、安全不变量）。
2. **`docs/CODE_REVIEW_v7.md`** —— 最新深度审查清单 + 修复进度（v7：安全收紧 6 项 + 调度正确性 11 项 + backend 正确性 8 项 + 迭代批 12 项）。改代码前先看这里，避免重复已知问题；每条 issue 带 `file:line` + 失败场景 + 修复方向（历史 v1–v6 归档在 `docs/` 与 `docs/internal/`）。
3. **`docs/DESIGN.md` / `docs/FEISHU_DESIGN.md`** —— 架构设计与飞书平台（一等公民）专项设计。
4. **`docs/RESEARCH.md`** —— 调研结论归档（iLink 协议/合规、Claude CLI/ACP 接口、竞品对照）。
5. **`CHANGELOG.md`** —— 各版本交付明细（P4–P10 迭代纪要都在版本头引用里）。
6. 长期决策与教训在 **engram 记忆库**，`project_id = "imagent"`（不是 "engram"）：`search_memory` / `architectural_decisions` / `recent_failures`。

当前进度：**P0–P10 全部交付，v1.9.x 已发布**。三平台（飞书为一等公民：CardKit 真流式卡片/审批按钮卡/云文档评论；wecom 长连接；ilink 实验性私聊）× 四后端（claude-cli/claude-acp/codex/gemini）。安全审查最新为 `docs/CODE_REVIEW_v6.md` / `v7.md`（v1–v5 历史归档在 `docs/internal/`）。CI 双平台（ubuntu + macOS）跑 fmt/clippy/test/MSRV/audit/deny；tag push 触发 CI 测试 + Release 前置测试 job（发布流程：打 `v*` tag → release.yml 构建 macOS arm64/x86_64 + Linux x86_64 产物并附 sha256 上传 GitHub Releases）。

代码改动约定：`.rs` 生产代码需方案设计到位 + `cargo test --workspace` 验证 + commit 注明待 review（详见 `CONTRIBUTING.md`）。

## 核心定位

- **是什么**：一个常驻网关进程，监听 IM 私聊/群聊消息 → 鉴权 → 驱动 agent（默认 Claude Code）执行真实任务（读写文件/跑命令/改代码）→ 把结果回传 IM（流式卡片/分片文本）。
- **不是什么**：不是"控制你自己的微信号收发所有消息"。iLink 给的是一个**独立 bot 身份**，只能**私聊**可靠工作，普通微信群基本不可用（见 RESEARCH.md 能力边界）。

## 架构（三层 + 双抽象）

```
trait Platform                        trait Backend
├── feishu (飞书私聊/群/云文档评论,     ├── claude (CLI + ACP 长驻子进程,
│          一等公民: CardKit 卡片)     │          PermissionCapability 协商)
├── wecom  (企业微信智能机器人长连接)   ├── codex  (codex exec --json)
└── ilink  (个人微信私聊, 实验性)      └── gemini (gemini -p -o stream-json)
        ↕                              ↕
              core: 调度 / 鉴权 / 会话路由(store 持久化) / 权限审批闭环
                    任务控制(/stop/批处理/看门狗) / 会话白名单 / 统一 resume
```

详见 `docs/ARCHITECTURE.md` / `docs/DESIGN.md`。

## 开发约定

- **Rust**：edition 2021，workspace 多 crate，`?` + `thiserror`/`anyhow` 错误处理，`tracing` 日志，`tokio` 异步，`#![forbid(unsafe_code)]`，MSRV 1.88（rust-toolchain.toml pin）。
- **代码改动约定**：`.rs` 生产代码需方案设计 + `cargo test --workspace` 验证（详见 [CONTRIBUTING.md](CONTRIBUTING.md)）；纯配置/文档/脚手架可直接改。
- **安全是硬约束，不是可选**（fail-closed 一致倾向）：
  - IM 入口**必须**做发送者白名单 + 会话（群）白名单鉴权；审批回复鉴权 = sender 白名单 OR admin（空 admin 不放权）。
  - 工具收敛：`allowed_tools` 白名单 + `permission_mode` 分档；闭环档（ask/auto-claude）× 非 FullLoop 后端**启动 fail-closed 拒绝**（v1.9.0 行为变更，见 `Backend::PermissionCapability`）。
  - 权限审批闭环（`--permission-prompt-tool` / ACP `session/request_permission` → IM 批准/拒绝）是核心差异化，已跨后端统一。
- **iLink 合规姿势**：定位为「OpenClaw Weixin channel 协议的 Rust 实现」，引用腾讯官方包/文档为出处；README 含免责声明；**绝不实现**绕过频率/风控的功能（ClawBot 条款 4.6 红线）。详见 RESEARCH.md「合规」。

## engram 记忆使用

`project_id = "imagent"`。开始任务前 `search_memory`、改文件前 `related_files`、查背景 `architectural_decisions`、避坑 `recent_failures`。完成功能/决策/修 bug 后写对应记忆。
