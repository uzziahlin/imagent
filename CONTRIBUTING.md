# Contributing to imagent

感谢你有兴趣贡献！🎉

## 开发流程

1. Fork + 从 `main` 开特性分支（`feat/<name>` / `fix/<name>`）。
2. 确保 CI 三件套本地过：
   ```bash
   cargo fmt --all --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```
3. 提 PR：描述**做了什么**、**为什么**、**怎么测的**。

## 代码约定

- **Rust edition 2021**，workspace 多 crate；`?` + `thiserror`（库）/ `anyhow`（main）错误处理；`tracing` 日志；`tokio` 异步。
- **安全是硬约束**（见 [CLAUDE.md](CLAUDE.md) / [docs/DESIGN.md §9](docs/DESIGN.md)）：
  - IM 入口必须白名单鉴权。
  - `--allowedTools` 配置收敛 + workdir 锁定。
  - iLink 定位「OpenClaw 协议 Rust 实现」，**绝不实现**绕频率/风控功能。
- 协议字段以**一手实测**为准（curl / 抓包 / 对照 hermes weixin.py），不照假设。
- commit message 清晰（`feat(crate): ...` / `fix: ...` / `docs: ...`）。

## 安全漏洞

**不要**开公开 issue 报告安全漏洞——见 [SECURITY.md](SECURITY.md)。

## 行为准则

参与即表示同意遵守 [Code of Conduct](CODE_OF_CONDUCT.md)——友善、尊重、包容。
