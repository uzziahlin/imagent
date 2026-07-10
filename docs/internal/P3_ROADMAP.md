# imagent P3 规划（探索 + 开源标准迭代）

> P2 已合并 main（功能完整 + 129 测试 + 真机启动确认）。P3 目标：**开源发布就绪**（可信/可用/可贡献）+ **核心扩展**（合规渠道/性能/生态）+ **运维健壮**。本文是探索框架，细节待各项前置研究。

---

## 1. 功能探索

### 1.1 平台扩展（`trait Platform`）
- **WeCom adapter**（企业微信官方 API，DESIGN §8）：智能机器人长连接 [path/101463](https://developer.work.weixin.qq.com/document/path/101463) / 应用消息。**合规主推**，对冲 iLink 平台存亡风险（RESEARCH §2.2 / 条款 7.2）。需前置研究 API。
- （未来）Telegram / Discord / 飞书——双抽象让加平台只加 adapter。

### 1.2 Agent 扩展（`trait Backend`）
- **F2 ACP backend**（P2 推 P3）：`claude-agent-acp` 长驻进程，复用 session，比 CLI 每消息 spawn 快。研究 JSON-RPC `session/new` + `session/prompt`（参考 feiyun `AcpAgent.cs`）。
- **多 agent**：Codex / Gemini / Cursor（`impl Backend`），扩生态（feiyun 已有多 agent）。

### 1.3 安全（硬约束延续）
- **凭据加密落盘**（DESIGN §9.4，P2 遗留）：`keyring` crate（macOS Keychain / Linux secret-service / Windows Credential Manager）加密 `bot_token`——当前明文存 SQLite，是开源发布的可信缺口。
- 沙箱增强：workdir 审计 + `claude --add-dir` 控制 + 命令黑名单。

### 1.4 可观测 / 运维
- Prometheus 指标（消息数 / 延迟 / 限流次数 / claude token 消耗）。
- `/health` HTTP endpoint。
- daemon 化（systemd / launchd unit + pid + 优雅重启，P1 是前台 Ctrl-C）。
- 配置热重载（SIGHUP）。
- tracing JSON format + 远程收集。

### 1.5 体验完善（P2 遗留 + 增强）
- D1 真机联调（MCP schema 校准 + Ask 全链路验证）。
- F1 媒体真机联调（AES key 编码 / PKCS7 校准）。
- 长消息分片（IM 长度限制，避免单条超长被截）。
- backend 失败消息队列 / 重试（不丢用户请求）。

---

## 2. 开源标准迭代（"按开源标准"重点）

成熟 Rust 开源项目的标配，按"发布前必须 → 贡献门槛 → 质量信号 → 发布 → 安全 → 文档"分。

### 2.1 发布前必须
- **LICENSE**：MIT（RESEARCH §2.3 建议，与上游 `openclaw-weixin` 一致）；或 Apache-2.0 / 双授权。
- **README.md** 打磨：slogan「Instant messaging, meet your agent.」+ 一句话定位 + **强制免责声明**（非腾讯官方/附属，使用者自负合规责任，RESEARCH §2.3）+ 安装 + 快速开始 + 架构图 + 配置 + 截图/GIF。
- **CHANGELOG.md**（keepachangelog 格式；P1/P2 已有内容）。
- `.gitignore`（`target/` / `~/.imagent` / `*.db`）。

### 2.2 CI/CD（贡献者门槛）
- **GitHub Actions**：`cargo test` + `cargo clippy -- -D warnings` + `cargo fmt --check` + `cargo audit`（依赖 CVE）+ `cargo deny`（license 合规）。
- `rustfmt.toml` + 分支保护 + PR 模板 + `CODEOWNERS`。
- `CONTRIBUTING.md` + `CODE_OF_CONDUCT.md` + Issue 模板（bug/feature）。

### 2.3 质量信号
- 测试覆盖率：`cargo-tarpaulin` + Codecov badge。
- rustdoc（`cargo doc` + docs.rs 自动发布）+ 文档测试。
- 集成测试：HTTP mock iLink（`wiremock`/`mockito`）+ 真 claude e2e harness（P1 仅 1 个 `#[ignore]` 真机测试）。

### 2.4 发布
- 语义版本（v0.1 → v1.0）+ git tag + GitHub Release（release notes）。
- **cross-compile 单二进制**（macOS arm/intel + Linux + Windows）+ Release artifact（Rust 核心卖点：单二进制部署）。
- `cargo-release`（自动化发版）+ Homebrew tap / `install.sh`（安装便捷）。

### 2.5 安全 / 治理
- `SECURITY.md`（漏洞披露流程 + 联系方式）。
- 凭据加密（§1.3）。
- 依赖审计（`cargo audit` CI 定期 + dependabot）。

### 2.6 文档
- docs/ 用 **mdBook** 发布 GitHub Pages（架构图 + 协议文档 + 教程 + FAQ）。
- 架构图（mermaid / draw.io，三层 + 双抽象）。

---

## 3. 优先级建议（我的建议，供你定）

**P3.0 开源发布就绪（先做，让项目能被使用/贡献）**
1. LICENSE + .gitignore + CHANGELOG
2. README 打磨（含免责声明 + 架构图）
3. CI（test/clippy/fmt/audit/deny）
4. SECURITY.md + **凭据加密落盘**（keyring）
5. 测试覆盖率 badge + rustdoc

**P3.1 核心扩展**
6. WeCom adapter（合规，扩用户面）
7. F2 ACP（性能）
8. 多 agent（生态）

**P3.2 运维 / 体验**
9. 指标 + /health + daemon
10. D1/F1 真机联调 + 长消息分片 + 失败重试

---

## 4. 决策点（需你定方向）

1. **License**：MIT（建议，与上游一致）/ Apache-2.0 / 双授权（MIT OR Apache-2.0，Rust 生态主流）？
2. **P3 优先**：先开源化（P3.0，我建议——让项目能发布/被贡献）还是先功能（WeCom/ACP）？
3. **发布时机**：现在就公开 GitHub，还是迭代到 v1.0 再公开？
4. **凭据加密**：`keyring` crate（跨平台 OS keychain，推荐）？
5. **CI 平台**：GitHub Actions（标准）？

---

## 5. P2 → P3 衔接的"已就绪"

P2 已为开源化打好基础：workspace 多 crate 清晰、`cargo test` 129 passed、clippy 净、tracing 日志、配置驱动、安全硬约束（白名单/allowedTools/workdir 锁定）落地、DESIGN/RESEARCH/P2_COMPLETE 文档齐全。P3.0 主要是"包装"（license/README/CI/加密），工程量小但发布价值高。
