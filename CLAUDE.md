# imagent

> **Instant messaging, meet your agent.**
> 一个用 Rust 写的、把即时通讯平台接入自主 agent 的网关。平台与后端双抽象：任何 IM（个人微信 iLink / 企业微信 WeCom）↔ 任何 agent（Claude Code / …）。

---

## 你正在做什么（重开会话 onboarding）

这是一个 **Rust 写的 IM ↔ agent 网关**，业务代码已大量实现：三层 + 双抽象（`core` 调度鉴权 / `ilink`+`wecom` 平台 / `claude`+`codex`+`gemini` 后端 / `store` SQLite 持久化）。

开始写代码前，**必须先读**：
1. **`docs/DESIGN.md`** —— 详细架构设计，实现的主要依据（crate 结构、trait 签名、session 机制、iLink 协议要点、安全设计、P0–P3 路线）。
2. **`docs/CODE_REVIEW_v2.md`** —— 深度审查清单 + 修复进度。改代码前先看这里，避免重复已知问题；每条 issue 带 `file:line` + 失败场景 + 修复方向。
3. **`docs/RESEARCH.md`** —— 调研结论归档（iLink 协议/合规、Claude CLI/ACP 接口、竞品 feiyun 对照、命名撞名）。
4. 长期决策与教训在 **engram 记忆库**，`project_id = "imagent"`（不是 "engram"）：`search_memory` / `architectural_decisions` / `recent_failures`。

当前进度：**P0 全修 + P1 基本完成**（凭据保护链 fail-closed、安全姿态、ilink 媒体收敛、WeCom 去重、dispatch 状态机收敛等，详见 CODE_REVIEW_v2「修复进度」段）。剩余：P1-E（ACP 取消，方案已设计 defer）、P2 打磨、开源工程化（E-3~E-7）、文档对齐（D-2~D-4）。

⚠️ **omp 工具链在本项目反复故障**（累计 8 次异常，含 3 次「空手退出」exit 0 零产出）——生产代码改动依 `CODE_REVIEW_v2` 顶部先例**破例主会话实现**（方案设计到位 + `cargo test` 验证 + commit 注明待 review）。任务说明再详尽也救不了 omp v16.x 的故障，不要再浪费时间委派。

## 核心定位

- **是什么**：一个常驻网关进程，监听 IM 私聊消息 → 鉴权 → 驱动 agent（默认 Claude Code）执行真实任务（读写文件/跑命令/改代码）→ 把结果回传 IM。
- **不是什么**：不是"控制你自己的微信号收发所有消息"。iLink 给的是一个**独立 bot 身份**，只能**私聊**可靠工作，普通微信群基本不可用（见 RESEARCH.md 能力边界）。

## 架构（三层 + 双抽象）

```
trait Platform                        trait Backend
├── ilink (个人微信私聊, 实验性)        ├── claude (CLI claude -p 优先; ACP 留 P2)
└── wecom  (企业微信官方 API)           └── (未来可换)
        ↕                              ↕
              core: 调度 / 鉴权 / 会话路由(store 持久化)
```

详见 `docs/DESIGN.md`。

## 开发约定

- **Rust**：edition 2021，workspace 多 crate，`?` + `thiserror`/`anyhow` 错误处理，`tracing` 日志，`tokio` 异步。
- **生产代码委派 omp**：继承全局规则——编写/修改 `.rs` 业务代码用 `omp-coder` subagent；纯配置/文档/脚手架可直接改。
- **安全是硬约束，不是可选**（详见 DESIGN.md「安全设计」）：
  - IM 入口**必须**做发送者白名单鉴权（iLink bot 任何人都能加好友）。
  - Claude 后端 `--allowedTools` 严格收敛（起步只 `Read,Edit`）、workdir 锁定。
  - 权限审批闭环（`--permission-prompt-tool` → IM 批准/拒绝）是核心差异化，P2 实现。
- **iLink 合规姿势**：定位为「OpenClaw Weixin channel 协议的 Rust 实现」，引用腾讯官方包/文档为出处；README 含免责声明；**绝不实现**绕过频率/风控的功能（ClawBot 条款 4.6 红线）。详见 RESEARCH.md「合规」。

## engram 记忆使用

`project_id = "imagent"`。开始任务前 `search_memory`、改文件前 `related_files`、查背景 `architectural_decisions`、避坑 `recent_failures`。完成功能/决策/修 bug 后写对应记忆。
