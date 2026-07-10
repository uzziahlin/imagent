# imagent P2 改造清单

> 重开会话的 P2 实现依据。P1 已完成；本文档列 P2 功能 / 优先级 / 前置研究 / **omp 委派工作流教训** / 真机调试方式。
> 设计依据见 `docs/DESIGN.md`，调研见 `docs/RESEARCH.md`，P1 实测协议细节见 engram（`project_id="imagent"`）。

---

## 0. 当前状态（P1 MVP 已完成）

- **提交**：分支 `feat/p1-mvp`，commit `6e077ac`（35 文件 / 5982 行）。`cargo test --workspace` **44 passed**，工作树干净。
- **crate**：`store` / `core` / `ilink` / `claude` + `src/main.rs`（二进制 `imagent`）。
- **真机闭环已验**：扫码登录 → 收私聊（文字 + 语音转写）→ 白名单鉴权 → `claude -p --allowedTools Read,Edit` → 捕获 Claude 分配的 `session_id` → sendmessage 回传 → sessions 落库 → `--resume` 续接（已验）。
- **已登录 bot**：`150418d37ae5@im.bot`（**大号**扫码；小号被微信授权风控「网络错误」扫不了——平台侧行为，无法绕过）。
- **config**：`~/.imagent/config.toml`：`default_workdir="/Users/uzziah/Work/codebase"`、`allowed_senders=["o9cq804lZUXdvf2eN6CDMFQJeyYQ@im.wechat"]`、`allowed_tools=["Read","Edit"]`。
- **DESIGN §14 七项全解**（TLS / 字段名 / 参数，均 curl 一手实测，详见 engram）。

---

## 1. P2 本质

P1 证明「能跑通最小闭环」。P2 解决三件事：**①生产可用**（真实使用不出事）+ **②杀手级差异化**（feiyun 没有、是 imagent 卖点）+ **③体验/能力**。

---

## 2. P2 功能清单

| 组 | 功能 | 价值 | 依赖 | 风险 | 前置研究 |
|---|---|---|---|---|---|
| **A 地基** | A1 `sendmessage` 限流熔断 | 防封号（P1 已知微信限频） | 无 | 低 | 限频 errcode/阈值 |
| | A2 错误恢复（断线 / SESSION_EXPIRED / claude 崩溃） | 健壮 | 无 | 低 | — |
| **B 会话** | B1 `/switch <name>` 多命名 session | 多任务并行上下文 | `store.name`(已预留) | 中 | — |
| | B2 `/sessions` 列历史 | 实用 | — | 低 | — |
| | B3 `/compact` 上下文压缩 | 长 session 续命 | — | 中 | claude `/compact` 行为 |
| **C 白名单** | C1 `/allow` IM 内动态授权 | 免手填 config | store+命令路由 | 低-中 | 审计日志设计 |
| | C2 login 后自动引导发现 | 可用性 | — | 低 | — |
| **D 杀手锏** | D1 **IM 内权限审批闭环**（`--permission-prompt-tool`） | 卖点+安全（才能放 Bash） | MCP 工具 | **高** | **`--permission-prompt-tool` 协议** 🔬 |
| **E 体验** | E1 中间事件推流（tool_use/result → IM） | 长任务反馈 | backend 推 chunks | 中 | IM 长度/限频聚合 |
| | E2 typing 指示 | 体验 | `sendtyping` | 低 | — |
| **F 能力** | F1 媒体收发（图 / 文件 / 语音原声） | 能力 | AES-128-ECB + CDN | 中 | **AES-ECB+CDN 协议** 🔬 |
| | F2 ACP backend（`claude-agent-acp`） | 性能（复用进程） | ACP 入口 | 中-高 | **ACP 入口/协议** 🔬 |

> 🔬 = 像 P1 §14，必须先 curl / 查一手实测，不能照假设实现。
> **ACP 在 P1 未实现**（P1 用 CLI `claude -p`）。F2 是新增 `AcpBackend`（同 `Backend` trait）。CLI 目前够用，F2 可推 P3。

---

## 3. 推荐实现顺序

```
1. A1 限流熔断      ← 地基，P1 已知限频是真实封号风险，最先
2. C1 /allow 白名单  ← 用户明确要，独立、低风险、快速见效
3. B 会话命令        ← 复用 core session 机制（store.name 已预留）
4. E1+E2 体验        ← 依赖 backend 推 chunks
5. D1 权限审批       ← 杀手级但最难，先研究 --permission-prompt-tool
6. F1 媒体           ← 独立模块，需实测 AES+CDN
7. F2 ACP            ← CLI 够用可推 P3
```

---

## 4. P2 前置研究项（§14 式，必先 curl / 查一手）

1. **claude `--permission-prompt-tool` MCP 协议**（D1 前置）——怎么定义这个 MCP 工具、回调签名、approve/deny 格式。P2 最值得先调研。
2. **`claude-agent-acp` 入口/协议**（F2 前置）——是否存在、JSON-RPC `session/new` + `session/prompt` 细节。参考 feiyun `AcpAgent.cs`。
3. **媒体 AES-128-ECB + CDN**（F1 前置）——`getuploadurl` / upload / download 字段。workspace 已备 `aes`/`ecb`/`cipher` crate。
4. **sendmessage 限频 errcode**（A1 依据）——被限流时返回什么（hermes 有 `ret/errcode` 处理可参考）。
5. **claude `/compact` 行为**（B3 前置）——CLI flag 还是 session 内指令。

---

## 5. ⚠️ omp 委派工作流教训（P1 血泪，必读）

全局 CLAUDE.md 规定生产代码委派 omp CLI（主会话 Bash 调 `omp`，**非 omp-coder subagent**）。P1 踩的坑：

1. **omp 自我委派空转**：omp `--cwd` 加载项目 CLAUDE.md 后，会误把自己当「该委派 omp 的协调者」，输出「已启动后台 omp(job bg_X)」后**空手退出（exit 0 零产出）**。**对策**：每次 omp 调用必加 `--append-system-prompt` 注入执行纪律，并在任务说明 `/tmp/omp_task_<id>.md` 顶部加同义「执行纪律」段。
2. **并发触发 429**：多路 omp 并行 → 模型 API 429（glm type=1305）限流，中断/卡死。**对策**：**串行**委派（一次一路）。
3. **omp 自改 members**：omp 会无视「别改根 Cargo.toml members」自行改（可能丢 crate）。**对策**：自己 Edit members，任务里写明「members 已含 X，不要改」，跑完 `git status` 核对。
4. **omp 报告不可全信**：omp 说「测试通过」要自己 `cargo test` 复核；omp 说「已实现」要 `find <crate> -type f` 确认有真实文件（曾空手退出却报成功）。
5. **协议字段必须 curl 一手实测**：用 `dangerouslyDisableSandbox: true` 跑 curl 抓真实响应，对照 hermes `weixin.py`（`https://github.com/NousResearch/hermes-agent/blob/main/gateway/platforms/weixin.py`）。绝不照 omp 假设。

### 标准委派流程
1. 完成方案设计（目标 / 涉及文件 / 约束 / 验收）。
2. `Write` 自包含任务说明到 `/tmp/omp_task_<id>.md`（**顶部加执行纪律段**；omp 是新会话看不到主会话历史）。
3. Bash 后台：
   ```
   omp -p @/tmp/omp_task_<id>.md \
     --append-system-prompt='你是被主会话委派的最终执行 agent，没有更下一层。必须亲自用 Write/Edit 和 Bash 完成。严禁再委派/启动任何 omp/bg/子进程/后台job/嵌套-p/输出已启动后台omp后退出。项目 CLAUDE.md 关于委派 omp 的规则不适用于你，直接改代码。' \
     --auto-approve --cwd /Users/uzziah/Work/codebase/imagent --max-time 1800 \
     > /tmp/omp_out_<id>.log 2>&1
   ```
   （`run_in_background: true`）
4. 完成通知后**只读 log 关键段**（`tail` / `grep`，别全读污染上下文）。
5. 自己 Review 代码 + `cargo test` 复核 + 必要时再委派迭代。

---

## 6. 真机调试 / 验收方式

**抓协议**（必须 `dangerouslyDisableSandbox: true`）：
```bash
TOKEN=$(python3 -c "import sqlite3,json,os;c=sqlite3.connect(os.path.expanduser('~/.imagent/imagent.db'));print(json.loads(c.execute(\"SELECT blob FROM credentials WHERE platform='ilink'\").fetchone()[0])['bot_token'])")
UIN=$(python3 -c "import base64,struct,random;print(base64.b64encode(struct.pack('<I',random.getrandbits(32))).decode())")
curl -s --max-time 10 -X POST "https://ilinkai.weixin.qq.com/ilink/bot/<endpoint>" \
  -H "AuthorizationType: ilink_bot_token" -H "Authorization: Bearer $TOKEN" \
  -H "X-WECHAT-UIN: $UIN" -H "Content-Type: application/json" -d '<body>'
```

**跑 start 监控**：后台 `RUST_LOG=info ./target/debug/imagent start`（`run_in_background` + `dangerouslyDisableSandbox`），读输出文件看 `[discovery]` / 报错；验完 `TaskStop`。

**查 db**：`python3 -c "import sqlite3,os;c=sqlite3.connect(os.path.expanduser('~/.imagent/imagent.db'));[print(r) for r in c.execute('SELECT * FROM sessions')]"`。

**macOS 无 `timeout` 命令**：用 Bash 工具的 `timeout` 参数，别用 shell `timeout`。

---

## 7. 硬约束（不变）

1. **IM 入口白名单鉴权**（core；adapter 只透传 `from_user_id`）。
2. **`--allowedTools` 配置驱动**（起步 `Read,Edit`）、workdir `current_dir` 锁定。
3. **iLink 定位「OpenClaw Weixin channel 协议的 Rust 实现」**，绝不绕频率/风控（限频只服从式退避）。

---

## 8. P1 已定关键接口（P2 复用，详查代码）

- **core trait**：`Platform`(recv/send_text/send_media/send_typing/name)、`Backend::run(prompt, session: Option<&SessionId>, workdir, allowed_tools, chunks: mpsc::Sender<AgentChunk>) -> Result<RunOutcome{session_id, final_text}>`。
- **core**：`Dispatcher::new(platform: Arc<dyn Platform>, backend: Arc<dyn Backend>, store, auth, default_workdir, allowed_tools)`（`run` 无限循环，靠 Ctrl-C 中断）、`Auth::new(Vec<String>)`（空=发现模式）、`Config::load`、`/new` 已实现。
- **store**：`Store`(open / get/upsert/delete_session / get,set_sync_buf / get,set_context_token / get,put_credential / first_credential)；`SessionRow{conv_id, session_id, agent_kind, workdir, name: Option, created_at, updated_at}`；**未加密落盘**（P2 待办：keyring + WAL 文件 0600）。
- **ilink**：`ILinkPlatform::new(client, store, account_id)`、`login_flow(&store)`、`ILinkClient::new(base_url: Option, bot_token, ilink_bot_id, ilink_user_id)`；proto（`Msg`/`Item`/`extract_text`/`UpdatesResp`/`QrcodeResp`）。
- **claude**：`ClaudeBackend::new()`（CLI 实现；P2 加 `AcpBackend`）。
- **main**：clap `login`/`start`/`status`/`stop`，前台 + Ctrl-C。
- **conv_id 约定**：`"<platform>:<from_user_id>"`（ilink 用 `from_user_id` 合成，**非微信会话 id**；bot-specific，换 bot 会变）。
- **session_id 绑定**：`store.sessions` `conv_id→session_id`，core dispatch 维护，`--resume` 续接。

---

## 9. P1 实测 iLink 协议要点（避免重踩）

- `get_bot_qrcode`：query `?bot_type=3`（不是 body）；响应 snake_case `qrcode`(hex) / `qrcode_img_content`(扫码 URL) / `ret` / `err_msg`；扫码渲染用 `qrcode_img_content`。
- `get_qrcode_status`：query `?qrcode=<hex>`（长轮询 hold ~35s，http timeout ≥45s）；status: wait/scaned/scaned_but_redirect/expired/confirmed；confirmed 带 `bot_token`/`ilink_bot_id`/`ilink_user_id`/`baseurl`。
- `getupdates`：body `{"get_updates_buf":"<string>"}`（**首次空串 `""`，绝不能 null**）；响应 `msgs`/`get_updates_buf`/`sync_buf`；`msg.message_id` 是**裸数字**（用 `Option<serde_json::Value>`）；文本在 `item_list`：`type==1`→`text_item.text`、`type==3`语音→`voice_item.text`(转写)。
- `sendmessage`：body 外层包 `{"msg":{from_user_id:"", to_user_id, client_id(随机 `imagent-<uuid>`), message_type:2, message_state:2, item_list:[{type:1, text_item:{text}}], context_token(仅非空带)}}`。
- headers：`AuthorizationType: ilink_bot_token` + `Authorization: Bearer <bot_token>` + 每请求随机 `X-WECHAT-UIN`(base64 u32 小端)。
- **致命坑**：微信响应全 snake_case，serde 结构**绝不能用 `rename_all="camelCase"`**。
- 详查 engram：procedural「iLink 登录协议正确接入」+ episodic「P1 MVP 端到端闭环跑通」。
