# 交接文档（HANDOFF）

> 用途：清空上一会话上下文后的无损接续。新会话开场白建议：「读 ~/Work/codebase/imagent/HANDOFF.md，继续 Web 平台项目」。
> 更新时间：2026-10-28。上下文：本文件由前一工作会话生成，配套设计文档 `docs/WEB_DESIGN.md`，另有 engram 记忆可查（project_id=default，关键词 happy-server / P2023 / PGlite）。

## 0. 一页速览

- **已完成并稳定运行**：Happy 自建中继全套基础设施（ECS 深圳 + 域名 HTTPS + happy-server + 自动续期），手机 App「会话列表不实时刷新」为上游 App 渲染 bug，与我们的部署无关，等 App 更新即可
- **当前主任务**：imagent **Web 驾驶舱平台**（分支 `feat/web-platform`）——**M1 已完成并部署验收**（`crates/relay` 代码 commit `e6d11ca`（15 测试全绿）+ ECS systemd 服务 active + Caddy 路由 + Mac 公网 wss 全链路双端实测通过：healthz/错 token 1008/双端互通/machine 上下线通知）。下一步 M2 `crates/web` Platform impl（帧协议以 crates/relay 实现为准：hello 首帧/Bearer 直通两种认证、hello_ack/machine/error 自产帧、ping 文本帧端到端透传）
- **M2+ 计划**：M2 `crates/web`（Platform impl + WS 客户端 + 配置）→ M2.5 MultiPlatform 组合器（feishu+web 并存）→ M3a/M3b web-ui SPA → M4 端到端验收（含 /resume 接管）。设计全文 `docs/WEB_DESIGN.md`（2026-10-28 评审修订已写入），编码派 subagent、主会话 review（用户 AGENTS.md 纪律）
- **开工前置（已完成）**：delegate/sandbox 双批已收尾提交 main（`3aea1d8` feat + `5546886` release，v1.32.0）；本文件与 `docs/WEB_DESIGN.md` 已随分支首个 commit（`d6803ce`）入库

## 1. 基础设施清单（Happy 自建中继，全在运行）

| 项 | 值 |
|---|---|
| ECS | 阿里云深圳 `i-example1234567890`，2C2G/40G，Ubuntu 26.04，公网 IP `192.0.2.10` |
| 安全组 | `sg-wz9gcfdx3kcmdo7ejoxo`，**只开放 8443 + 22**（3005/443 已撤销） |
| 域名 | `example.com`（阿里云，已实名），A 记录 `reply` → 192.0.2.10 |
| HTTPS | Caddy 2.6.2，Let's Encrypt 证书（`/etc/ssl/happy/`，caddy 属主 0600），到期 2027-01-24，**自动续期** |
| happy-server | systemd `happy-server.service`，npm 包 happy-server-self-host@1.1.11 装在 `/opt/happy`，数据 `/opt/happy-server/data`（PGlite） |
| 中继地址 | `https://relay.example.com:8443`（手机 App 与 Mac CLI 均指向此） |
| 主密钥 | `/opt/happy-server/happy.env`（HANDLY_MASTER_SECRET，丢失则全设备重新配对） |

### 关键本地文件（Mac）

- `~/.happy-relay/`：`adapter-patch/`（**重装 npm 包后必须重打的补丁**，见 §4 坑3）、`acme-auth.sh`/`acme-cleanup.sh`（DNS-01 钩子，用 aliyun CLI OAuth）、`go-live.sh`、`renew.sh` + launchd `com.uzziah.happy-cert-renew`（每 20 天自动续期+部署）
- `~/.happy/settings.json`：Mac CLI 的 serverUrl（https://relay.example.com:8443）
- `~/.workbench/config.json`：workbench CLI 凭证（AliyunCLI 模式，复用 aliyun CLI 的 OAuth）
- `~/.local/bin/`：workbench、aliyun

### 访问 ECS 的两种方式

1. **workbench CLI**（主力，阿里云官方通道，免公网 IP 免 SSH）：`workbench exec -i i-example1234567890 -c "命令"`（无状态，多命令用 && 串联；长任务 nohup 后台化）、`workbench upload/download`（走 OSS 中转，**远端已存在同路径文件会拒传，先 rm**）
2. **aliyun CLI 3.5.2**（OAuth 已配，profile default，region cn-shenzhen）：安全组增删（AuthorizeSecurityGroup/RevokeSecurityGroup）、实例重启等 API 操作

## 2. 运维手册（常用命令）

```bash
# happy-server（在 ECS 上，经 workbench exec）
systemctl status happy-server && journalctl -u happy-server -n 50
systemctl restart happy-server

# 证书手动续期+部署（Mac 上；正常不需要，launchd 自动）
bash ~/.happy-relay/renew.sh    # 日志 ~/.happy-relay/renew.log

# Caddy 配置在 /etc/caddy/Caddyfile（8443 = happy 反代 + welcome banner 分流 + imagent relay 分流；443 空闲备用；改前备份 Caddyfile.bak-web-20261028）
# imagent relay（Web 驾驶舱 M1，已部署运行 2026-10-28）：
systemctl status imagent-relay && journalctl -u imagent-relay -n 50
systemctl restart imagent-relay
#   二进制 /opt/imagent-relay/imagent-relay（musl 静态 3MB）；env /opt/imagent-relay/env（0600，双 token）
#   本地 token 副本 ~/.imagent/web-relay.env（0600）；systemd DynamicUser + 全套加固
# 升级流程（Mac）：cargo zigbuild --release -p imagent-relay --target x86_64-unknown-linux-musl
#   → workbench upload target/x86_64-unknown-linux-musl/release/imagent-relay /tmp/imagent-relay -i i-example1234567890
#   → exec "mv /tmp/imagent-relay /opt/imagent-relay/imagent-relay && chmod 755 … && systemctl restart imagent-relay"
#   ⚠️ workbench 直传 /opt/imagent-relay/ 会报 PathNoWritePermission，一律 /tmp 中转再 mv
#   ⚠️ cargo install cargo-zigbuild 必须 --locked（非锁定版依赖要 rustc 1.91 > 本地 1.88）
```

### 坑清单（血泪，均已踩过）

1. **PGlite 单进程红线**：绝不在 happy-server 运行时用第二个进程打开 `/opt/happy-server/data/pglite`（曾因此 WAL 损坏、深夜重建库）。排查数据走服务 HTTP 接口，或先 `systemctl stop` 再直查，查完清 `postmaster.pid` 与 `/tmp/pglite` 再启动
2. **重装 npm 包必须重打补丁**：`pglite-prisma-adapter` 的 `convertBytes` 返回 Uint8Array 跨 JS→Rust 边界会被 JSON 化为 `{"0":..}`，Prisma 引擎抛 P2023。修复 = 把 `Uint8Array.from({length: hexString.length/2}, ...)` 换成 `Array.from(...)`（dist/index.cjs 和 index.mjs 都要，运行时走 CJS）。上游 issue slopus/happy#686/#1723、PR #1719 未合并，合并后此坑消失
3. **2G 小机跑 npm install 会高负载**：一律 nohup 后台 + 远离轮询高峰；机器假死（云助手心跳冻结）就控制台/API 重启，别硬耗
4. **prisma 相关**：@prisma/client 必须 6.19.2（overrides 已钉在 /opt/happy/package.json）；generate 需 `PATH=node_modules/.bin` + `PRISMA_ENGINES_MIRROR=https://registry.npmmirror.com/-/binary/prisma/`
5. **Mac 直连 ECS 的 TLS**：curl/LibreSSL 指纹会触发运营商 DPI RST，**Node 程序不受影响**——诊断网络问题时别用 curl 下结论
6. **npm 用 npmmirror**：`--registry=https://registry.npmmirror.com`；npm 上 happy@1.3.1-3 是 2026-04 老预发布（版本号陷阱），CLI 正确版本 = 1.2.6-beta.1（已装）
7. **版本错位坑**：App Store 版 Happy App 较新、CLI 落后时，手机消息会被 CLI 标记 `contentType:'unknown'` 丢弃——升级 CLI 解决（1.2.6-beta.1 实测正常）

## 3. 主任务：imagent Web 驾驶舱平台

### 3.1 是什么

给 imagent（本仓库，Rust 的 IM↔agent 网关）增加第四个平台 **Web**：浏览器（手机 PWA/桌面）远程驾驶 Mac 上的 Claude Code——发消息、批权限、流式输出、/stop、/switch。定位「ZCode 式渲染 + imagent 全部既有能力」。

### 3.2 设计文档（必读）

`docs/WEB_DESIGN.md`——架构（ECS relay 中继 + Mac 零入站 + `crates/web` Platform impl + Vite/React SPA）、帧协议 v1、双 token 认证、故障语义、安全威胁模型、M1-M4 里程碑。**关键决策已定**：ECS 中继（否掉 Tailscale 主案/端口转发）；中继独立小服务 `crates/relay`（axum 0.8 + tokio-tungstenite，绑 127.0.0.1:3010，Caddy 挂 /imagent/*）；双 token（GATEWAY_TOKEN/USER_TOKEN，constant-time，不进 URL）；browser 侧断线不缓存（gateway 侧内存保留终态 + 刷新重发）。

### 3.3 决策记录（用户已拍板 2026-10-28）

1. **会话模型**：网页与 IM **共享同一个 `/switch` 命名会话池**——composer 默认发往当前活跃命名会话，`/switch` `/resume` 在 Web UI 可用
2. **前置**：delegate 批次 WIP（16 文件 +1249 行）**已提交完成**（v1.31 委托编排 + v1.32 OS 沙箱双批，见 git log `3aea1d8`/`5546886`）
3. **评审修订（2026-10-28 代码级评审后拍板）**：① 新增 **M2.5 MultiPlatform 组合器**——现调度器为单 Platform（`Config.platform` 单值），不补则选 web 即掉 feishu，与「IM 兜底」矛盾；组合器自身实现 Platform（recv select 合并 / send 按 conv 前缀路由 / capabilities 交集），Dispatcher 零改动，配置形态 `platforms = ["feishu", "web"]`；② `approval_response` 帧补**必填 convId**（PermissionRouter 按 conv+request_id 路由）；③ `/resume` 接管本机会话**提进 M4 验收**（核心已存在，网页侧只是可点列表）+ M3 拆 M3a（文本流/composer/审批）/ M3b（卡片富渲染）；④ 定位锚点：Web 的增量是驾驶舱体验 + 自有栈，**不是** takeover 实时性——「接管正在跑的本地会话」短期靠 Happy + `/resume` 手递手，终态 `imagent term`（本地前端复用同会话池）上远期路线，均不进 MVP

### 3.4 里程碑与工时（AI 辅助编码前提）

- M1 `crates/relay` ✅ **已完成**（2026-10-28：代码 `e6d11ca` + ECS 部署 + 公网验收全过；实做 ~700 行含 15 测试，超出 ~400 行估计——认证双形态/踢旧 conn_id/集成测试更完整）
- M2 `crates/web`（Platform impl + WS 客户端 + 配置 `[[platform.web]]`）—— 0.5-1 天
- M3 `web-ui/`（Vite+React+Tailwind：登录/消息流/流式卡片/审批/composer；DOMPurify 必须）—— 1-2 天
- M4 端到端验收（PWA、断线演练、`ss -tln` 证 Mac 零监听）—— 0.5 天

### 3.5 imagent 代码接线要点（已源码考察确认）

- Platform trait：`crates/core/src/platform.rs:160`——`recv()/send_text/send_media/send_card/update_card + capabilities()` 能力位模型；Web 声明 `CARDS`（MEDIA phase2，声明与实现须一致是仓库纪律）
- 流式卡片：`crates/core/src/card_session.rs`（累积+节流 patch+finalize，Web 直接消费）
- 审批：`crates/core/src/permission.rs` 的 `PermissionRouter`（approve/deny/always D-记忆 + waited_secs 审计）——Web 的 approval_response 帧等价于 IM 按钮点击
- webhook 先例：axum Router 在 core（`POST /hook/<token>`，HMAC+防重放）——relay 的认证/路由可参考其风格
- 未提交 delegate 工作与 Web 互补不冲突（将来网页 fan-out 子任务）

## 4. 环境备忘（Mac）

- Claude Code 2.1.310 + GLM 网关（会话模型 glm-5.3[1m]，走 ~/.claude 配置；手机 App 显示的模型列表是 App 内置目录，与此无关——远程接管时模型选 Auto 避免覆盖）
- happy CLI 1.2.6-beta.1（npm）；happy-server-self-host 1.1.11
- engram 记忆库有本项目全过程记录（project_id=default）
- zsh；AI 协作纪律：主会话设计/review，生产代码编码派 subagent（用户 AGENTS.md 规则）

## 5. 新会话启动检查单

1. 读本文档 + `docs/WEB_DESIGN.md`（评审修订已并入，勿按旧版记忆施工）
2. `cd ~/Work/codebase/imagent && git log --oneline -3` 确认基线（`5546886` chore(release): v1.32.0 在顶）；HANDOFF/WEB_DESIGN 未跟踪属预期
3. §3.3 决策已全部拍板，无需再确认
4. 开 `feat/web-platform` 分支（首个 commit 带上 HANDOFF.md + docs/WEB_DESIGN.md）→ M1 开工（编码派 subagent）→ M2 → **M2.5 MultiPlatform** → M3a/M3b → M4
