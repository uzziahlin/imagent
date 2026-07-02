# P3 并行迭代 Roadmap（多窗口无冲突分配）

> 多窗口/多人同时推进 P3，按文件覆盖切分，避免 merge 冲突。配套 git worktree 隔离。
> ⚠️ **omp/agent CLI 共享 glm 配额**：多窗口同时委派 omp 会触发 429（type=1305）。建议**串行委派**，或各窗口错峰 / 用不同模型。

---

## 1. P3 剩余功能 × 文件覆盖矩阵

| 功能 | 主要文件 | 共享冲突点 |
|---|---|---|
| **P3.2 运维**（进行中） | `crates/core/src/{metrics.rs(新),dispatch.rs,config.rs}` + `src/main.rs` + `crates/ilink/src/platform.rs`(埋点) | main / core.dispatch / ilink.platform |
| **WeCom adapter** | `crates/wecom/`（**新 crate**）+ `crates/core/src/platform.rs` + `src/main.rs`(构造) + `crates/store/`(多平台凭据) | main / core.platform(trait) |
| **F2 ACP backend** | `crates/claude/src/acp.rs`（**新文件**）+ `crates/core/src/backend.rs`(trait) + `src/main.rs`(选 backend) | main / core.backend(trait) |
| **多 agent**（Codex/Gemini/…） | `crates/<agent>/`（**新 crate**）+ `crates/core/src/backend.rs` + `src/main.rs` | main / core.backend(trait) |
| **长消息分片** | `crates/ilink/src/platform.rs`(send_text) + `crates/core/src/dispatch.rs`(reply) | ilink.platform / core.dispatch（与运维冲突）|
| **凭据加密 v2**（context_tokens） | `crates/store/src/{store.rs,credentials.rs}` + `crates/ilink/src/platform.rs` | store / ilink |
| **D1/F1 真机联调** | 无代码（真机自测 + 可能字段微调） | 几乎无 |

## 2. 共享文件冲突分析

真正会撞的只有 3 类：
- **`src/main.rs`**：所有功能都要加构造/选择逻辑 → 多分支必撞。**对策**：各分支 main 改动**隔离到独立段**（各自函数/区段），合并时几乎自动；或最后由一人统一整合。
- **`crates/core/src/{platform,backend}.rs` 的 trait**：WeCom/ACP/多 agent 可能想改 trait 签名。**对策：约定 adapter/impl 适配现有 trait，不改 trait** → 各 crate 完全独立，零冲突。
- **`crates/core/src/dispatch.rs` + `crates/ilink/src/platform.rs`**：运维与长消息分片都改 → **串行**（分片在运维后）。

## 3. 并行分组（同时跑不冲突）

```
组 A（运维，进行中，独占 core.dispatch + ilink.platform + main）
  └ 完成后 ──┬─▶ 组 B  WeCom   (crates/wecom/ 新 crate，不改 trait)
             ├─▶ 组 C  ACP     (crates/claude/src/acp.rs 新文件，不改 trait)
             └─▶ 组 D  多agent (crates/<name>/ 新 crate，不改 trait)
             【B / C / D 互相无冲突，可三窗口同时跑】
  └ 之后 ────▶ 组 E  长消息分片（依赖运维改完的 core.dispatch + ilink.platform）
  └ 随时 ────▶ 组 F  D1/F1 真机联调（无代码，纯测试）
```

**关键约定**：B/C/D 各自 crate 内 impl 现有 `Platform`/`Backend` trait，**不改 trait 签名**。则三个新 crate 互不读对方文件，零冲突。各自 main 改动隔离，合并时整合。

## 4. 多窗口操作建议

1. **git worktree 隔离**（每功能一个工作树 + 分支）：
   ```bash
   git worktree add ../imagent-wecom   feat/wecom
   git worktree add ../imagent-acp     feat/acp
   git worktree add ../imagent-codex   feat/codex-agent
   ```
   各窗口在独立目录跑（互不干扰工作树）。
2. **omp 串行 / 错峰**：多窗口同时委派 omp 会 429。要么串行（一窗口委派时其它等），要么错峰（每窗口间隔几分钟），要么各窗口用不同 agent CLI/模型。
3. **trait 冻结**：约定期内不改 `Platform`/`Backend` trait 签名，需要扩展先在 docs 提案、统一升级（避免 N 个分支都改 trait 的地狱合并）。
4. **main 最后整合**：各功能 main 改动用独立函数（如 `build_wecom_platform()` / `build_acp_backend()`），main 只调用，合并时几乎无冲突。

## 5. 各功能前置研究（开做前）

| 功能 | 前置研究 |
|---|---|
| WeCom | 智能机器人长连接 API（[path/101463](https://developer.work.weixin.qq.com/document/path/101463)）：握手/收发消息字段；凭据（corpid/secret/agentid）。 |
| ACP | `claude-agent-acp` 入口/协议：JSON-RPC `session/new` + `session/prompt` + SessionUpdate 通知流（参考 feiyun `AcpAgent.cs`）。 |
| 多 agent | 各 agent CLI：codex/gemini 的 print 模式 + stream 格式 + session 续接。 |
| 长消息分片 | iLink `sendmessage` 单条长度上限实测（curl 递增长度找阈值）；微信侧折叠/截断行为。 |
| 凭据加密 v2 | `context_tokens` 是否也进 keyring（per-peer entry 多），或字段级加密（AES + 机器绑定 key）。 |

## 6. 推荐迭代顺序（单人串行 / 多人并行都适用）

1. ✅ P3.0 开源化 / ✅ P3.1 覆盖率+release+文档站（已合并 main）
2. 🚧 P3.2 运维（进行中）
3. ⏭ P3.3 WeCom（合规渠道，最大用户价值）— 可与 P3.4/P3.5 并行
4. ⏭ P3.4 F2 ACP（性能）— 可与 P3.3/P3.5 并行
5. ⏭ P3.5 多 agent（生态）— 可与 P3.3/P3.4 并行
6. ⏭ P3.6 长消息分片（运维后）
7. ⏭ P3.7 v1.0 发布：owner URL 填 + 真机自测 + tag v1.0.0（触发 release CI）
