# imagent P2 完成报告

> 分支 `feat/p2`。P2 全功能落地 + 真机启动确认。功能交互自测清单见下。

## 1. 已完成功能（commit 顺序）

| 组 | 功能 | commit | 要点 |
|---|---|---|---|
| A1 | sendmessage 限流熔断 | `24c5fa2` | 解析 ret/errcode；滑动窗口熔断（30s/1/30s）；限流退避 3s≤4 次；网络线性退避；出站串行；session 过期透传 |
| C1 | /allow 动态白名单 | `8892ab4` | IM 内 /allow /disallow /list /whoami（管理员模型，防自我授权）；发现态引导；CLI `imagent allow`；audit_log；store v2 |
| A2 | 错误恢复 | `722982a` | session_expired 优雅停止（return Err）；send 失败分级；main 提示重新 login |
| B1+B2 | /switch + /sessions | `2a5148d` | 多命名 session（named_sessions 侧表 + active_name config KV）；sessions 表不动；store v3 |
| B3 | /compact 软压缩 | `9c282b8` | claude -p 无 compact flag（CLI ref 确认）→ 摘要+重置+延续（resume 生成摘要→存→重置→下次注入） |
| E1 | 中间事件推流 | `6d712e6` | stream-json tool_use/tool_result 解析 → AgentChunk；reply 末尾附「🔧 工具调用」摘要（聚合不刷屏） |
| E2 | typing 指示 | `32ef795` | sendtyping（无 msg 包装）+ getconfig typing_ticket 缓存（500s TTL）+ dispatch 触发 |
| D1 | IM 权限审批闭环（杀手锉） | `b7b0912` | --permission-prompt-tool MCP：imagent mcp 子命令（stdio JSON-RPC）→ unix socket → PermissionRouter → send_text 询问 → recv 路由回复 → claude allow/deny；PermissionMode Off/Allow/Deny/Ask |
| F1 | 媒体收发 | `34a2181` | AES-128-ECB+PKCS7；CDN download/upload（POST+x-encrypted-param）；入站接收+出站发送全链路；SSRF 白名单；key 编码不对称 |
| — | clippy 净 | （clippy commit）| 全 workspace clippy 0 warning |

**测试**：`cargo test --workspace` = **129 passed / 0 failed / 1 ignored**。`cargo clippy --workspace --all-targets` = 0 warning。

## 2. 真机启动确认（已验）

`RUST_LOG=info ./target/debug/imagent start` 成功：
- store 自动迁移 v1→v2→v3（建 named_sessions / allowed_senders / audit_log，幂等无错）。
- `imagent started (platform=ilink, tools=[Read,Edit], discovery=false)`。
- recv 长轮询工作，**无 session_expired**（bot 凭据有效）。

## 3. 真机功能自测清单（用户，需另一微信号给 bot 发私聊）

```
imagent start                       # 前台常驻
# 用另一个微信号给 bot（150418d37ae5@im.bot）发私聊：
hello                               # → claude Read/Edit 回复；reply 末尾见 🔧 工具摘要（E1）
/new                                # 重置会话
/whoami                             # 看自己的 sender id
/allow <friend_sender_id>           # 授权另一人（管理员模型）
/list                               # 查白名单
/switch refactor                    # 开命名会话 refactor
（在 refactor 上下文聊几句）
/switch docs                        # 开另一命名会话
/sessions                           # 列命名会话（* 标活动）
/switch refactor                    # 切回（resume）
/compact                            # 软压缩（生成摘要+重置+下次注入）
发一张图片                          # → F1 入站接收（存 ~/.imagent/media/），claude 可 Read
```

**D1 权限审批自测**（需放危险工具）：
```
# config.toml 改：allowed_tools = ["Read","Edit","Bash"]，permission_mode = "ask"
imagent start
（让 claude 执行需要 Bash 的任务）→ IM 收到「🔐 Claude 请求执行 Bash(...)，回复 y 允许」
回复 y                              # → claude 执行；回复其它 → deny
```

**E2 typing**：agent 处理中，对方微信看到「正在输入...」指示。

**A1 限流熔断**：正常使用不触发；狂发才会（服从式退避，不绕风控）。

## 4. 遗留 / P3

- **F2 ACP backend**：CLI 够用，按 P2_ROADMAP §3 推 P3。
- **D1 真机联调**：MCP schema（claude 实际传参）+ Ask 全链路（claude→MCP→socket→IM→回复→claude）未经真机验证，需 config permission_mode=ask 实测校准。
- **F1 媒体真机联调**：AES key 编码不对称 / PKCS7 / getuploadurl 字段按 hermes 实现 + 单测自洽，真机收发字节需校准。
- **凭据加密落盘**：bot_token 仍明文存 SQLite（DESIGN §9.4，P2 未做 keyring）。
- **WeCom adapter**：P3。

## 5. omp 委派工作流（P2 跑通 9 功能，验证有效）

- `--append-system-prompt` 注入执行纪律（防 omp 读 CLAUDE.md 后自我委派空转）。
- 串行委派（防 429），每次 `--auto-approve --max-time 1800` 后台 + 只读 log 关键段。
- 每功能：主会话设计 + 写自包含 `/tmp/omp_task_*.md` → omp 实现 → 主会话 grep 改动 + **自跑 cargo test 复核**（omp 报告曾格式错位，必自验）。
- 协议字段 curl/WebFetch/hermes 一手实测（sendtyping/getconfig/媒体 AES/限频 errcode/claude CLI flag）。
