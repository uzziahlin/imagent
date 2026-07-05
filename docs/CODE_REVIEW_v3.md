# imagent 深度 Review v3 — Issue 清单

> **评审对象**：`imagent v1.0.0` @ commit `5d03ac2`（2026-07-05，已合并 `fix/code-review-v2`）。
> **评审范围**：`crates/{core,ilink,wecom,claude,codex,gemini,store}` + `src/main.rs` + 开源工程化层（CI / Cargo metadata / 文档）。
> **评审方法**：主会话工程化层逐行精读 + 3 个独立子审查并行精读 7 crate（claude+core 安全核心 / core 调度并发状态机 / 平台存储加密），主会话对最关键的 5 条新 P1 逐一读源码复核定级；每条 finding 带 `file:line` + 失败场景 + 修复方向。
> **与 [`CODE_REVIEW_v2.md`](CODE_REVIEW_v2.md) 的关系**：本轮先**逐条核实 v2 声称已修的 21 项**（v1 当年谎报 6 项「全部完成」，v2 才补上；本轮独立核实 v2 是否重蹈覆辙），再查漏 v2 未覆盖的新问题。
> **总体评分**：**开源就绪度 7.5 / 10**（与 v2 评分持平——v2 修复全部真落地拉升了底子，但本轮暴露了 v2 的两个盲区「失败路径资源回收」+「进程边界优雅退出」共 9 条新 P1）→ 修完第一波 P1 + CI macOS + owner 对齐后约 **8.5 / 10**。

## 📋 修复进度（2026-07-05，分支 `fix/code-review-v3`）

**本轮核实结论**：v2 声称已修的 21 项中 **20 项真修、1 项部分修**（P2-L ws_url localhost `contains` 子串匹配）——**v2 没有重演 v1 的谎报**，工程诚信过关。

**本轮新发现**：9 条 P1 + 一批 P2 + 若干 P3 + 工程化缺口，集中在两个 v2 盲区 + 新引入代码（codex backend）边角。

每条 issue 带 checkbox，id 形如 `P1-1`，便于转 GitHub issue 追踪。

---

## ✅ 亮点（无需改动，记录在案）

- **三层 + 双抽象架构干净**：core 持 `Platform`/`Backend` trait，session 生命周期提到 core（store 持久化），Backend 退化为无状态执行器——优于竞品 feiyun。
- **v2 修复经受住第三轮独立核实**：P0 三项、P1 安全链/健壮性、P2 store/平台打磨均真落地，有测试佐证，无谎报。
- **工程纪律上游水平**：生产代码（非 `#[cfg(test)]`）仅 **11 处 `unwrap/expect/panic`**（263 处是把测试模块算进来的误读）；SQL 全参数化；日志不泄漏凭据明文；fail-closed 倾向一致。
- **开源标配齐全**：LICENSE/README/CHANGELOG/SECURITY.md/CODE_OF_CONDUCT/CONTRIBUTING/PR+Issue 模板/CODEOWNERS/dependabot + CI（lint/test/MSRV/audit/deny/coverage/release/pages）+ 三重供应链防护 + mdBook 文档站 + deploy 单元（systemd/launchd）。

---

## 🔴 P1 — 阻塞项（开源前必修，9 条）

### A. 功能正确性

#### P1-1  codex sandbox flag 错位（沙箱配置从未生效）⬜

- **位置**：`crates/codex/src/backend.rs:75`。
- **本质**：`-s <sandbox_mode>` 被加在 `--`（67/72 行）**之后**。clap 的 `--` 语义是「之后全是 positional」，codex `exec` 用 `trailing_var_arg` 收集 prompt，于是 `-s workspace-write` 被并入 prompt 字符串或触发「too many args」。
- **影响（定性修正）**：这是**「隐式过度收紧」而非「安全放宽」**——codex 默认 sandbox 为 `read-only`（最安全），用户配 `Edit/Write` 想要 `workspace-write` 却从未生效，结果是写工具失败。**不构成越权**，但所有 codex 后端用户的沙箱配置形同虚设。
- **修复**：`-s <mode>` 移到 `--` 之前，prompt 放最后。
- **回归测试**：`sandbox_flag_before_dashdash`（断言 `Command` 的 arg 序列里 `-s` 在 `--` 之前；或集成测试断言 codex 收到 `-s`）。

### B. 安全防御姿态不一致

#### P1-2  CDN 下载只校验 host 不校验 scheme（http:// 可通过）⬜

- **位置**：`crates/ilink/src/media.rs:153-184`（`assert_cdn_host` + `resolve_download_url`）。
- **本质**：`assert_cdn_host` 只取 host 比对 `CDN_HOSTS` 白名单，`full_url = "http://novac2c.cdn.weixin.qq.com/..."` 能通过校验，原样返回 → 下载走明文 HTTP，泄漏 `encrypted_query_param`。与 P0-C「login baseurl 强制 https」姿态矛盾；ECB 无完整性，链路明文后密文可被替换。
- **修复**：`assert_cdn_host` 用 `url::Url::parse` 后断言 `scheme() == "https"`。
- **回归测试**：`cdn_http_rejected`（`http://novac2c.cdn.weixin.qq.com/...` → Err）、`cdn_https_allowed`。

#### P1-3  send_text 失败仍 register pending（IM 抖动 → conv 卡死 + 吞消息）⬜

- **位置**：`crates/core/src/dispatch.rs:364-371`。
- **本质**：`handle_permission_socket` 在 `send_text`（发「🔐 请求执行 Bash」）失败时只 `warn!` 不 return，继续 `router.register`。用户永远看不到询问 → 不回复 → agent 卡满 `agent_timeout`（默认 **600s**）→ 期间该 conv 所有入站消息被 `dispatch.rs:211-218` 当作回复吞掉。
- **修复**：`send_text` 失败时直接回写 socket `{allow:false, message:"send_text failed"}` 并 return，不挂 pending。
- **回归测试**：`permission_ask_send_text_failure_does_not_register`（MockPlatform send_text 返 Err → 断言 router 无 pending、socket 收到 deny）。

### C. 进程边界与退出语义（v2 最大盲区）

#### P1-4  无 SIGTERM 处理（容器/systemd 停止无优雅退出）⬜

- **位置**：`src/main.rs:214-230`。
- **本质**：`tokio::select!` 只等 `ctrl_c()`（SIGINT）。`docker stop` / `systemctl stop` / k8s 滚动更新默认发 SIGTERM → 进程被直接杀，没机会 flush SQLite WAL、写 audit、清理 `permission.sock`、回写 in-flight agent 的 deny。
- **修复**：同时 `tokio::signal::unix::signal(SignalKind::terminate())`，select! 任一触发即退出。
- **回归测试**：`#[cfg(unix)]` 集成测试发 SIGTERM 断言退出（或单测抽 `shutdown_signal()` 函数）。

#### P1-5  Ctrl-C 不 drain in-flight task（子进程可能半写）⬜

- **位置**：`src/main.rs:214-230`。
- **本质**：退出时 `dispatcher.run()` future 被 drop，所有 `tokio::spawn` 的任务（agent task / socket accept / SIGHUP / metrics）被 runtime 直接 abort；正在写文件的 claude 子进程（`kill_on_drop`）被 SIGKILL → 可能留下半写文件。
- **修复**：引入 `CancellationToken`，退出时：① 停收新消息；② 给在飞 task 一个 bounded grace；③ 显式 `unlink permission.sock`；④ store 显式 close。
- **回归测试**：留 e2e（grace 语义难单测）；抽 `shutdown()` 函数单测 cancel 传播。

#### P1-6  MCP 子进程 read_line 无超时（主进程崩溃 → 僵尸）⬜

- **位置**：`crates/core/src/mcp.rs:177`（`ask_via_socket`）。
- **本质**：mcp 子进程 connect + 写请求后用 `reader.read_line` 等回复，**无超时**。主进程崩溃 / socket 对端 close 但 TCP 半连接 → read_line 永久 await，mcp 子进程泄漏为僵尸。
- **修复**：包 `tokio::time::timeout(agent_timeout, read_line)`，超时返 deny。
- **回归测试**：`mcp_ask_times_out_when_no_response`（MockListener 不回 → 超时 deny）。

### D. 失败路径资源泄漏（v2 第二个盲区）

#### P1-7  conv_locks 失败路径 HashMap 项永久泄漏 ⬜

- **位置**：`crates/core/src/dispatch.rs:967, 974`（已主会话复核）。
- **本质**：`handle()` 普通消息分支的两条错误 `return`（`Ok(Err(e))` / `Err(JoinError)`）在 conv_locks 清理逻辑（1055-1062）之前。`_guard`/`lock` 虽 drop（strong_count 2→1），但 HashMap 项不移除；下次该 conv 消息又把 strong_count 推回 2，永久漏清。海量 conv + 运行足够久 → `conv_locks` 无界增长。
- **修复**：把「释放 + strong_count==1 则 remove」抽成 `release_conv_lock(conv, lock)`，三条 return（含正常路径）统一调用；或 RAII guard。顺带修 P2（slash 路径也走统一 release）。
- **回归测试**：`conv_lock_released_on_backend_failure`（MockBackend run 返 Err → 触发多次 → 断言 `conv_locks` 不增长）。

#### P1-8  PermissionRouter 超时不清理 sender（pending 残留）⬜

- **位置**：`crates/core/src/permission.rs:60` + `crates/core/src/dispatch.rs:374-384`。
- **本质**：`handle_permission_socket` 的 `tokio::time::timeout` 超时分支只回 deny，不调任何清理；`PermissionRouter` 无 `cancel`/`remove_pending` API。P1-G 修了「agent 不永驻等」，没修「map 项留存」。叠加 P1-7，发生 ask-timeout 的 conv 留死 sender。
- **修复**：`PermissionRouter` 加 `pub async fn cancel(&self, conv_id)`（lock + remove）；`Err(_elapsed)` / `Ok(Err(_))` 分支显式调用。
- **回归测试**：`permission_router_cancel_removes_pending`。

#### P1-9  handle_permission_socket 读行无上限 / 写无超时（同 uid DoS）⬜

- **位置**：`crates/core/src/dispatch.rs:333-336, 392-393`。
- **本质**：`reader.read_line(&mut line)` 无 cap；`stream.write_all` / `flush` 无 timeout。peer_uid 挡住跨 uid 攻击者，但任何**同 uid 进程**可连上发巨大行 OOM，或永不读响应让 write_all 长时阻塞。
- **修复**：`read_line` 改按字节读 + 上限（如 64KiB）；`write_all` 包 `tokio::time::timeout`。
- **回归测试**：`permission_socket_rejects_oversized_line`、`permission_socket_write_timeout`。

---

## 🟠 P2 — 应优化项（精选 + 其余概括）

### 并发 / 状态机

- ⬜ **P2-1  ACP cancel 命中 `break` 杀跨 conv 共享连接** — `crates/claude/src/acp.rs:257-263`。cancel 后 `break` 退出 `while let req = prompt_rx.recv()`，整个长驻 task 结束，排队中的其他 conv turn（`prompt_tx` capacity 8）只能各自等超时。修复：cancel 分支只 drop 当前 `prompt_fut` 并 continue，不 break；让 SDK `ChildGuard` 仅在真正退出时 kill。
- ⬜ **P2-2  权限回复路由与 conv_lock race** — `crates/core/src/dispatch.rs:208-218`。recv 循环 `has_pending` → `route` 不取 conv_lock，与普通消息路径的 conv_lock 存在 race，「yes」可能被当新 prompt 发给 claude。修复：合并为单次 `try_route_if_pending(conv, reply) -> Option<bool>`，或 register 提前到 send_text 之前收窄窗口。
- ⬜ **P2-3  ACP sessions 缓存 clear() 可能丢活跃 session** — `crates/claude/src/acp.rs:224-227`。`len()>1000` 时 `clear()`（HashMap 无序），可能清掉当前活跃 conv 的 sessionId → 用户上下文丢失。修复：LRU 或只 evict 最早插入项。
- ⬜ **P2-4  SIGHUP 三步非原子** — `src/main.rs:446-483`。`reload(senders)` / `reload_tools` / `reload_permission_mode` 独立写三个 RwLock，收紧权限时存在混合配置窗口。修复：聚合为单一 `Arc<RwLock<RuntimeConfig>>` 原子替换。
- ⬜ **P2-5  backend panic 分支丢 final_text** — `crates/core/src/dispatch.rs:939-976`。backend 产出 `Final(t)` 后 panic，走 `Err(JoinError)` 只回「backend task panicked」，已收 final 丢失。修复：panic 分支也检查 final_text，有则优先回传。
- ⬜ **P2-6  conv_locks slash 路径不显式 release** — `crates/core/src/dispatch.rs:398-405`。纯 slash 流（只 `/sessions` 查询）的 conv 永不清理。修复：随 P1-7 统一 release。

### 安全 / 凭据

- ⬜ **P2-7  peer_uid 同 uid 信任模型未文档化** — `crates/core/src/dispatch.rs:286`。注释只描述正向（MCP 子进程同 uid）；实际任何同 uid 进程（被入侵的浏览器/CI/恶意 npm 包）可连 socket 伪造 `conv_id` 钓鱼（如向 CEO 推送假 Bash 审批）。修复：注释补威胁模型；考虑 socket 加握手 token（spawn MCP 子进程时 env 传一次性 token，首包校验）。
- ⬜ **P2-8  macOS LOCAL_PEERCRED 返 effective uid，比对 real uid** — `crates/core/src/dispatch.rs:1090 vs 1135`。`current_uid()` 用 `getuid()`（real），macOS 分支取 `xucred.cr_uid`（effective）。setuid 部署下 Ask 闭环完全瘫痪。修复：macOS 分支用 `geteuid()` 比对，或 `getuid()==geteuid()` 才信任。
- ⬜ **P2-9  ws_url localhost 例外用 `contains` 子串匹配（v2 P2-L 部分修）** — `crates/wecom/src/client.rs:80-85`。`ws://localhost.evil.com` / `ws://evil.com/?to=127.0.0.1` 可绕过。修复：`url::Url::parse` 取 `host_str()` 后 `==` 比较。
- ⬜ **P2-10  缺 `delete_credential`/logout，凭据永驻** — `crates/store/src/store.rs`。无吊销/轮换路径，keyring 条目也无清理 API。修复：`Store::delete_credential(platform, account)` 同步删 SQLite + keyring + 审计。
- ⬜ **P2-11  明文→keyring 迁移不写审计** — `crates/store/src/store.rs:245-267`（`update_credential_blob`）。绕过 P1-B 审计。修复：迁移成功后 `append_audit("credential_migrated", ...)`。
- ⬜ **P2-12  permission.rs:30 文档残留「首字符 y/Y」+ 中文词过窄** — `crates/core/src/permission.rs:30, 46`（已主会话复核）。函数 doc 仍写「首字符 y/Y → allow」与代码矛盾；allow 词清单漏「可以/行/没问题/对/嗯」等中文高频词，误 deny 率高。修复：删 doc 首字符句；补中文词。
- ⬜ **P2-13  upload_cdn URL 未 percent-encode** — `crates/ilink/src/media.rs:309-312`。`x_encrypted_param` / `filekey` 含 `&`/`=`/`#` 可注入额外 query 项。修复：`url::Url::parse_with_params` 或 percent-encode。
- ⬜ **P2-14  `~/.imagent` 目录默认 0755 + 注释矛盾** — `src/main.rs:79` + `crates/core/src/dispatch.rs:269` vs `crates/store/src/store.rs:796`。create_dir_all 不 chmod（默认 0755）；注释互相矛盾（dispatch 说 store 保证 0700，store 说部署者负责）。**注**：0755 目录其他用户不可写，symlink race 实际不可利用，故降 P2（最小权限收紧 + 注释一致）。修复：`main.rs` 创建后 `set_permissions(0o700)`；统一注释。
- ⬜ **P2-15  metrics `register_*!` 用 expect（poison panic）** — `crates/core/src/metrics.rs:34-49`。LazyLock 内 expect，同名 metric 已注册时 panic。修复：Err 转 `error!` + noop 实例。
- ⬜ **P2-16  tools 持读锁 clone Vec** — `crates/core/src/dispatch.rs:909`。读锁内深拷贝，SIGHUP 频繁热重载时竞争。修复：`Arc<Vec<String>>` 整体 swap。
- ⬜ **P2-17  PKCS7 unpad 非常量时间（理论性）** — `crates/ilink/src/media.rs:42-54`。配合 download_media 区分 parse/padding 错误消息，弱 padding oracle。实际可利用性低（aes_key 随密文同链路投递）。修复：`subtle::ConstantTimeEq`；错误消息统一。

---

## 🟡 P3 — 打磨项（概括）

- **P3-1** `getsockopt` 不校验返回 `*len` 是否填满结构（dispatch.rs:1109-1118/1126-1138）——Linux ucred/macOS xucred 稳定 ABI，实际不触发。
- **P3-2** `write_mcp_config` 临时文件不清理，堆积 `mcp_*.json`（claude/backend.rs:101-102）——即 v2 P2-I defer 部分；文件名暴露 conv_id。
- **P3-3** `media_dir` TOCTOU + chmod 错误吞没（ilink/platform.rs:628-639）——`let _ = set_permissions`。
- **P3-4** `is_session_expired` 字符串匹配错误消息（ilink/platform.rs:623-625）——服务端改 401 文案/本地化即失效。
- **P3-5** keyring marker `starts_with` 撞名（store/credentials.rs:34-41）——加 magic + blob_kind 列。
- **P3-6** `cfg!(test)` 硬编码让 keyring 测试恒失败（store/credentials.rs:55,95）——注入 Keyring trait + CI 跑真 keychain。
- **P3-7** `normalize_sender` 每条消息 String 分配（core/auth.rs:43-45）——改 `&str` + trim 借代。
- **P3-8** 入站媒体下载失败但文本空时仍上报 agent（ilink/platform.rs:148-170）——空 prompt 误触发。
- **P3-9** mcp.rs 同步 `stdin.lock().lines()` 阻塞 tokio worker（core/mcp.rs:189-243）——换 `tokio::io::stdin()` 或改同步 main。

---

## 🛠 开源工程化层

- ⬜ **E-1（P1）CI 无 macOS test 矩阵** — `.github/workflows/ci.yml` 只在 `ubuntu-latest` 跑，但项目一等支持 macOS（README / P0-B peer_uid macOS LOCAL_PEERCRED 分支 / SIGHUP）。release.yml 有 macOS build 但不跑 test。**macOS 平台分支零 CI 验证**。修复：lint-and-test job 加 `strategy.matrix: [ubuntu-latest, macos-latest]`；audit/deny 维持 ubuntu。
- ⬜ **E-2（P1）GitHub owner 不一致** — `Cargo.toml`/README badge 用 `uzziah/imagent`，`book.toml` 用 `UzziahLin/imagent`——文档站 edit 链接、badge 会 404。修复：统一为真实 owner。
- ⬜ **E-3（P2）各 crate Cargo.toml metadata 不全** — 7 crate 只有 name/version/rust-version/license/publish，缺 `description/repository/keywords/categories/readme`。crates.io 发布姿态不全（当前 `publish=false` 影响有限）。修复：workspace 加 `[workspace.package] description/repository`，各 crate `description.workspace=true` 等 + keywords/categories。
- ⬜ **E-4（P2）文档漂移多处**：① README 测试数 229（实际 235）；② DESIGN/CODE_REVIEW_v2 仍写 MSRV 1.80（已抬至 1.88）；③ `main.rs:1-4` 头注释仍写「四个 crate / 驱动 claude -p」（实际 7 crate 多后端）；④ `login --platform wecom` 错误信息仍写「P1 仅支持」（src/main.rs:85）；⑤ README:155 crate 列表只列 4 个（漏 wecom/codex/gemini）。
- ⬜ **E-5（P2）SECURITY.md 措辞漂移** — 第 15 行仍写「沙箱逃逸 / workdir 锁定」，与 P1-D「cwd（非沙箱）」澄清矛盾，会误导用户以为 workdir 是安全边界。修复：改「agent cwd（非沙箱）」。
- ⬜ **E-6（P2）`Cmd::Stop` 空壳** — `src/main.rs:274-276` 只打印「请 Ctrl-C」。修复：实现（PID 文件 + 信号），或从 CLI 移除并文档说明「前台运行，Ctrl-C 停止」。
- ⬜ **E-7（P3）CODEOWNERS 占位** `@imagent/maintainers`——非真实账号，review 指派不生效。
- ⬜ **E-8（P3）clippy 缺 `--all-features`** — 当前无 feature flag，影响不大；加 `--all-features` 保险。

---

## 🧭 架构评价

**优点**：双 trait 抽象边界干净、`spawn_cli_backend` 消除三后端重复、`CoreError::SessionExpired` 类型化、出站分片 UTF-8 完整、conv 级串行锁、生产代码极少 unwrap。

**结构性建议**（本轮新发现印证 v2 §架构评价）：

1. **状态机收敛**：会话状态散在 `sessions` + `named_sessions` + `config` 三表（无事务），是 P1-7 / P2-2 / P2-3 / P2-4 / P2-6 的共同温床。收敛到 per-conv `ConvState` + 单一 mutator。
2. **失败路径资源回收统一**：`conv_locks`、`PermissionRouter`、`permission.sock` 三处泄漏，根因都是「正常路径清理逻辑在 return 之后」。统一 RAII guard 模式。
3. **进程退出语义**：SIGTERM + drain + cleanup 是一整套（P1-4 / P1-5 / P1-6），当前完全缺失。
4. **`ReplyHint::ILink` 泄漏到 core 类型**：core 知道 ilink 实现细节，改关联类型/泛型。
5. **可测试性**：权限审批闭环是杀手锉功能，却零端到端测试（全 mock），macOS/真实 keyring 路径无覆盖。建真机 e2e checklist。

---

## 🎯 修复优先级

**第一波（开源前必修）**：P1 全部 9 条 → E-1（CI macOS）+ E-2（owner 统一）。
**第二波（上线前）**：P2 精选（P2-1~P2-17）→ E-3~E-6（crate metadata + 文档对齐 + SECURITY + Stop）。
**第三波（打磨）**：P3 + 架构性建议（状态机收敛 / RAII guard / 退出语义 / 权限闭环 e2e）。

---

## ⚠️ 破例说明（生产代码主会话实现）

继承 `CODE_REVIEW_v2.md` 顶部先例：omp 工具链在本项目累计 **8 次异常**（含 3 次「空手退出」exit 0 零产出，engram `cd4f3255` / `33d52163`）。依项目根 `CLAUDE.md` 明确指示「生产代码改动依 `CODE_REVIEW_v2` 顶部先例破例主会话实现（方案设计到位 + `cargo test` 验证 + commit 注明待 review）」，本轮 P1/P2 生产代码修复**破例主会话自行 Edit**。**违反全局 CLAUDE.md omp 委派硬规则**，请 review。

每项修复均：① 基于本轮已 review 的方案；② `cargo test --workspace` 验证；③ commit message 注明对应 issue id + 待 review；④ 文档/配置/CI 类（E-1~E-8、D-4）直接改。

---

## 附录：v2 已修项第三轮核实矩阵

| v2 编号 | v2 声明 | v3 核实 | 去向 |
|---|---|---|---|
| P0-A ACP fail-closed | ✅ | **真修** | — |
| P0-B socket peer_uid | ✅ | **真修**（macOS effective/real 见 P2-8） | P2-8 |
| P0-C login baseurl 白名单 | ✅ | **真修** | — |
| P1-A WAL/SHM 0600 | ✅ | **真修** | — |
| P1-B 凭据审计 | ✅ | **真修**（迁移路径漏审计） | P2-11 |
| P1-C require_keyring | ✅ | **真修** | — |
| P1-D workdir 措辞 | ✅ | **真修**（SECURITY.md 漏改） | E-5 |
| P1-E ACP cancel | ✅ | **真修**（break 杀共享连接） | P2-1 |
| P1-F conv_lock slash | ✅ | **真修**（失败路径漏 + slash 不 release） | P1-7/P2-6 |
| P1-G pending 超时 | ✅ | **真修**（超时不清理 map） | P1-8 |
| P1-H 媒体解密 fail-closed | ✅ | **真修**（scheme 漏校验） | P1-2 |
| P1-I WeCom 去重 | ✅ | **真修** | — |
| P1-J login 禁 redirect | ✅ | **真修** | — |
| P1-K compact_summary 推迟删除 | ✅ | **真修** | — |
| P1-L 媒体流式 OOM | ✅ | **真修** | — |
| P2-A~X（24 项） | ✅ | **23 真修 / 1 部分修**（P2-L ws_url `contains`） | P2-9 |

**结论**：v2 声称已修 21 项中 **20 真修、1 部分修**，无谎报。v2 的修复真实落地，但每条修复都留下了「失败路径」或「实现粗糙」的尾巴，正是本轮 P1/P2 的主要来源。
