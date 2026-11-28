//! `imagent-web`：Web 驾驶舱 M2——网关侧 WebPlatform。
//!
//! 拓扑（M1 见 `imagent-relay`）：`浏览器 WS ⇄ ECS relay ⇄ 本 crate`。本 crate
//! 是网关侧**出站** WS 客户端（连 `wss://…/imagent/gateway`，hello 首帧
//! 认证并自报 machine 身份，Mac 零监听端口）+ [`Platform`](imagent_core::Platform)
//! 实现：
//! - [`client`]：WS 客户端任务——重连（指数退避 1s→封顶 30s，成功建连重置）、
//!   心跳（30s 协议层 Ping，对齐 relay 90s idle 超时）、停机
//!   （CancellationToken，平台 drop 即退出）。把浏览器帧分拣进 inbound /
//!   sync channel，把出站帧写回 relay。
//! - [`platform`]：`WebPlatform`——`recv`/`send_*` 覆写为帧收发，含审批闭环
//!   （approval_request / approval_resolved）与 sync 重发（D6 会话恢复）。
//! - [`key`]：网关认证凭证（v1.3）——ed25519 密钥文件
//!   （`<imagent_home>/web-gateway-key`，首次自动生成 + 0600 + 公钥指引
//!   日志）优先，静态 token（v1.2 兼容路径）回退。
//!
//! 网关认证（v1.3，见 [`client`] 三帧握手）：token 形态 hello 一步校验
//!（兼容）；key 形态 `hello{machine}` → relay `challenge{nonce}` →
//! `prove{pubkey,signature}`（对 nonce 原文签名）→ `hello_ack`——relay 只
//! 存公钥，私钥 seed 不出本机。
//!
//! E2E 中继盲化（v1.4，见 [`e2e`]）：session 认证后浏览器逐机器发起
//! `e2e_hello{sid,ek,n1}` → 网关回 `e2e_ack{gk,ik,n2,sig}`（复用 v1.3 ed25519
//! 密钥签名）→ 双方派生 XChaCha20-Poly1305 会话密钥，业务帧包装为
//! `enc{sid,box}`——relay 对业务内容全盲，仅按 `machine`/`sid` 信封元数据
//! 路由。无任何握手会话时出站行为不变（明文，兼容旧浏览器）。
//!
//! 帧协议 v1.1（与 relay / SPA 对齐，M5 多终端管理）：JSON 文本帧，信封
//! `{"v":1,"type":…}`。入站（浏览器→网关）：`message` / `approval_response` /
//! `control` / `sync` / `ping`（均可带可选 `machine` 路由字段——relay 据此
//! 路由到本网关，网关侧无需消费；`machine`/`machines`/`error`/`hello_ack`
//! 为 relay 自产帧，client 分拣自理）。出站（网关→浏览器）：`text` / `card` /
//! `card_patch` / `approval_request` / `approval_resolved` / `pong`，**全部
//! 带本机 `machine` 字段**（SPA 据此区分多台终端）。
//!
//! 工作空间帧 v1.6（字节级契约与 SPA 侧任务说明一致，不得增删改字段名）：
//! 入站 `workspace_list` / `workspace_save` / `workspace_remove` /
//! `conv_workdir_set`——不进 recv / dispatch 命令链，由 platform 的
//! workspace 泵直接读写 store KV（`workspace:`/`workdir:` 表，与终端
//! /ws /cd 同源互通）。出站 `workspaces`（全量快照，SPA 单一数据来源）/
//! `conv_workdir`（增量：conv_workdir_set 成功或终端 /cd 通知）。**这两类
//! 出站帧不带 machine 字段**（v1.6 帧契约如此定义——E2E 活跃时 enc 信封
//! 本身携带 machine，与 v1.1 帧「帧内携带」并存不冲突）。错误复用
//! `error` 帧（code=workspace_invalid）。
//!
//! v1.7 扩展（媒体上行 + 会话持久化，契约同款字节级纪律）：入站 `message`
//! 帧可带可选 `media{name,mime,b64}`（图片 base64 上行——字段存在性/类型
//! 检查在本层 [`map_inbound_frame`]，值域校验 + 落盘 + 合成 InboundMessage
//! 在 platform 媒体泵，失败回 error 帧 code=media_too_large|media_invalid；
//! 纯图消息 text 可缺省/空串）；出站 `workspaces` 快照帧可带可选 `sessions`
//! 段（`web:` 会话历史 `{id,title,updatedAt}`——title=手动名优先 /
//! first_prompt 摘要按码点截 24；空集整段不序列化，旧形态字节不变）。
//!
//! v1.8 扩展（模型菜单动态化，契约同款字节级纪律）：出站 `workspaces`
//! 快照帧可带可选 `models` 段 `{current, cliDefault, options[{label,
//! target}]}`——按 backend 分链（codex-app 批）：claude 系 = CLI 配置
//!（`~/.claude/settings.json` env 的 ANTHROPIC_DEFAULT_* 映射，ccswitch 第三方
//! 供应商场景）优先、config `official_models.claude` 回退；codex-app = 实时
//! model/list 闭包 → `~/.codex/config.toml` + catalog →
//! `official_models.codex`。汇聚见 [`models`]；空 options 整段不序列化，
//! 旧形态字节不变。
//!
//! v1.8 扩展（会话历史重放，契约同款字节级纪律）：出站 text 帧 / card
//! 终态帧（state=="done"）经 platform 异步落盘 store `outbound_log`
//!（per-conv seq 单调 + 滚动 GC 500 条；同 messageId 终态只落一次；
//! 流式中间帧 / approval_resolved / sync 重发不落）。入站 `history_query
//! {convId, since}`（经既有业务帧白名单路径，E2E 下自动解密）→ platform
//! history 泵查 `list_outbound_since` 回出站 `history{machine, convId,
//! entries, lastSeq, trimmed}` 帧（entries 元素 key 前缀 `h:<seq>` 与实时
//! 条目区分，card 的 state 恒 "done"）。
//!
//! v1.9 扩展（会话语义标题）：CLI 的语义标题不持久化，网关用 headless
//! CLI（`claude -p`）单次调用自行生成（见 [`titles`]）——快照组装时懒触发
//!（无手动名且无语义 title 的 conv，每快照至多 3 个最旧），成功写 store
//! sessions.title（schema v18），下一条快照带出；失败/超时静默降级回
//! first_prompt 机械摘要。`sessions` 段 title 优先级：手动名 > 语义标题 >
//! first_prompt 摘要。
//!
//! v1.36 扩展（引擎选择，契约同款字节级纪律）：出站 `workspaces` 快照帧
//! 可带可选 `backends` 段 `{current, options[]}`——网关多引擎注册表名单
//!（default + extras，main 装配处一次性注入纯名单，无 IO）：`current` =
//! default 引擎名（未设 `backend:<conv>` KV 的会话实际用的引擎），
//! `options` = 全注册表名（SPA 新会话弹窗引擎 chips 数据源，per-conv 补发
//! `/backend <名>` 落在 core 侧）。空 options 整段不序列化，旧形态字节不变。
//!
//! v1.40 扩展（语音转写 whisper 方案网关侧，契约同款字节级纪律）：入站
//! `asr_request {audio, durationMs}`（SPA 录 16k/mono/16-bit WAV 的 base64
//! 上行，mime 恒 `audio/wav` 契约定死不传）→ platform 的 ASR 泵：b64 解码
//!（坏 → `asr_invalid`；解码后 >8MB 复用 media 上限 → `asr_too_large`）→
//! 写 `<imagent_home>/tmp/asr-<unix_ms>.wav` 临时文件 → spawn `whisper-cli`
//!（30s 超时 kill，逐次拉起用完即退不常驻，泵串行消费天然排队）→ stdout
//! 多行合并单行空格拼接 → 出站 `asr_result {text, durationMs}`（text = 识别
//! 文本，空也算成功由 SPA 提示「没听清」；durationMs = 转写耗时）。失败走
//! 既有 error 帧：`asr_unconfigured`（未配 whisper_bin/whisper_model）/
//! `asr_failed`（转写失败/超时/二进制不存在）/ `asr_too_large` /
//! `asr_invalid`。能力通告：`workspaces` 快照帧可带可选 `"asr": true`
//!（whisper_bin 与 whisper_model 均已配置才序列化；缺省/false 不序列化，
//! 旧形态字节不变）。WAV 规格由 SPA 保证（16k/mono/16-bit），网关不校验
//! WAV 头细节（坏文件 whisper 自身报错 → asr_failed）。
//!
//! v1.41.8 扩展（用户自发文本镜像标记，契约同款字节级纪律）：出站 `text`
//! 帧可带可选布尔字段 `"self":true`——该文本是**本用户自己发出**的消息镜像
//! 而非 agent 回复（`Platform::send_user_text`，现仅 peer 通道的两处镜像：
//! 驾驶舱在 `peer:<窗口名>` 会话直接打的字、`/to` 的派活正文）。SPA 据此
//! 按用户气泡渲染并与本机回显去重；落盘 outbound_log 的 payload 同带
//! `"self":true`，[`history_entry_from_row`] 原样透传进 history 条目（重放
//! 后仍是用户气泡）。缺席 = 既有语义（agent 文本）——`send_text` 的帧与
//! 落盘 payload 字节不变（旧网关不发该键；旧 SPA 忽略未知键）。
//!
//! v1.42 扩展（驾驶舱核心交互修复，契约同款字节级纪律）：
//! - 入站 `control` 帧可带可选 `convId`：合成的命令文本落该会话（经
//!   [`normalize_conv`] 归一）；缺席 / 空串 / 非字符串维持旧行为落默认会话。
//!   `/stop` 等命令**按会话**生效（cmd_stop 只中断该 conv 的在飞轮），SPA
//!   停止按钮必须指明目标会话——恒落默认会话时，非默认会话里点停止停不掉
//!   当前任务，默认会话反而多出一条「当前没有运行中的任务」。
//! - 待审批快照（浏览器请求 → 网关回权威快照，参照 `workspace_list` →
//!   `workspaces`）：入站 `approvals_query`（无必填字段，不进 recv / dispatch
//!   命令链，由 platform 的 sync 泵处理）→ 出站 `approvals{machine,
//!   items[{convId, requestId, tool, inputSummary, timeoutSecs}]}`。`items`
//!   为该网关**当前全部待审批**（跨会话、不分平台前缀；可为空数组 `[]`，
//!   键不省略），`timeoutSecs` 为**剩余**秒数（整数 ≥1；SPA 倒计时锚点 =
//!   收到时刻 + timeoutSecs），已到期的条目不进快照，输出序确定（convId
//!   升序，同 conv 按请求先后）。离线期间到达的审批重连后经此恢复按钮；
//!   离开期间已超时 / 已解决的审批经此收敛。旧网关对 `approvals_query`
//!   走未知 type 忽略（前向兼容）。
//! - `push_notify` 帧的 kind 新增 `peer_reply`（窗口回信推送：本地 Claude
//!   窗口经 peer 通道回给驾驶舱的文本，convId = `peer:<窗口名>`，见
//!   [`PushNotifyKind`]）。
//!
//! v1.43 扩展（本地窗口看板 + 窗口事件推送，契约同款字节级纪律）：
//! - 入站 `windows_query`（无必填字段，由 platform 的 sync 泵回快照）→ 出站
//!   `windows{machine, hookInstalled, items[{name, status, waitingFor?, cwd,
//!   startedAt?, statusUpdatedAt?, watched}]}`：本机**不归网关管**的 Claude Code
//!   窗口清单（数据源是网关对注册表的轮询，见 `imagent-core` peer 模块；
//!   `?` 字段为 None 时省略键；`items` 可为空数组，键不省略）。peer 通道未启用
//!   的网关不回任何帧（SPA 据此不显示看板）；旧网关按未知 type 忽略。
//! - 入站 `window_watch{name, watch}`：关注 / 取消关注一个窗口（决定「跑完
//!   一轮」是否推送）；处理完重发一帧 `windows` 让所有设备的 `watched` 收敛。
//!   **关注集合是跨设备的全局状态，归网关所有**：持久化在网关 store `config`
//!   表（`imagent_core::peer::WATCH_CONFIG_KEY`），重启后保留；前端只发
//!   `window_watch`、不保存名单、不做对账重发（各存一份名单时，A 设备取消关注
//!   会被 B 设备的对账立刻补发回来，等于取消不了）。
//! - 网关侧变化（轮询发现快照变化 / 事件）经 [`crate::platform::WebPlatform`] 的
//!   `on_windows_changed` 钩子主动推 `windows` 帧（best-effort）。
//! - `push_notify` 帧的 kind 新增 `window_turn_end` / `window_waiting`（convId =
//!   `peer:<窗口名>`，title = 窗口名截 32 码点）。
//!
//! v1.44 扩展（同浏览器多标签页 + 消息送达回执，契约同款字节级纪律）：
//! - 多标签页是**纯网关侧修复**：E2E 会话表改为每个登录会话保留多把密钥
//!   （每个标签页一把，见 [`e2e`]），出站逐密钥各密封一份——中继本来就按会话
//!   广播，每个标签页只解得开自己那份。新增入站 `e2e_close`（封在 enc 里，
//!   标签页关闭时发，网关删除解开它的那把密钥、不回帧）；空闲密钥由网关按时
//!   回收。旧 SPA 不用改也不再被顶掉。
//! - 入站 `message` 帧可带可选 `clientId`（字符串，1..=64，`[A-Za-z0-9_-]`；
//!   不合规当缺席）。出站 `ack{machine, clientId, convId, status, reason?,
//!   message?}`：`received`（网关已收到并接下——client 层 / 媒体泵在消息进入
//!   入站 channel 之后发）或 `rejected`（明确的丢弃点：`malformed` 由本层
//!   帧校验产生，`not_allowed` / `queue_full` / `peer_unreachable` 由 Dispatcher
//!   经 [`imagent_core::Platform::on_message_rejected`] 上报）。同一 clientId
//!   至多一条 received、一条 rejected，received 恒先。
//! - 入站 `gateway_info_query`（无必填字段）→ 出站 `gateway_info{machine,
//!   version, caps}`：`caps` 含 `"acks"` 即支持回执；旧网关按未知 type 忽略，
//!   SPA 据此判定不支持、不显示任何状态。
//!
//! **帧内 convId 协议约定：恒为完整 conv 形态（`web:<session>`）**。出站帧
//! （text / card / approval_request …）写入的即完整 conv，浏览器回执帧
//! （message / approval_response）**原样回传收到的 convId** 即可，不得剥前
//! 缀；裸 session 名（如 `work`）仅作入站兼容输入，由 `normalize_conv`
//! 统一归一为完整 conv。例外透传：`local:<会话名>`（网关侧本地终端会话的
//! 既有 conv 形态）原样放行——MultiPlatform 把 local 会话出站镜像给 web
//! 平台（SPA 驾驶舱实时视图），SPA 对 local 会话的干预消息/审批回执须回到
//! local 会话，不得被强加 `web:` 前缀（详见 [`normalize_conv`]）。
//!
//! 本模块承载**帧类型定义与纯函数**（入站映射、出站药化、超限截断、machine
//! id 规范化），全部无 IO 便于单测（`default_machine_id` 的 hostname 探测
//! 除外，见其注释）；网络循环在 [`client`]，[`Platform`] 装配在 [`platform`]。

#![deny(unsafe_code)]

pub mod client;
pub mod e2e;
pub mod key;
pub mod models;
pub mod platform;
pub mod titles;

pub use key::GatewayCredential;
pub use platform::{AsrContext, WebPlatform};

use crate::platform::ApprovalSnapshotItem;
use imagent_core::peer::{WindowInfo, WindowStatus};
use imagent_core::{
    CardPhase, CardTerminal, ConvId, CoreError, InboundMessage, MediaRef, OutboundCard, ReplyHint,
    Result, TodoStatus, UserId,
};
use serde::Serialize;
use std::collections::BTreeMap;

/// 统一 tracing target（仓库纪律）。
pub(crate) const TARGET: &str = "imagent::web";

// ---------------------------------------------------------------------------
// machine 身份（v1.1 多终端管理）
// ---------------------------------------------------------------------------

/// machine id 协议约束（与 relay hello 校验对齐）：≤32 char，仅 ASCII
/// 字母数字与 `-_`。
pub const MACHINE_ID_MAX_CHARS: usize = 32;
/// machine 身份不可得时的回退值（= 单机旧形态，与 relay 的 default 机器
/// / Bearer 直通语义同源）。
pub const DEFAULT_MACHINE_ID: &str = "default";

/// machine id 规范化（纯函数便于单测）：非法字符替换为 `-`、截断到
/// [`MACHINE_ID_MAX_CHARS`] char、剥首尾 `-`。剥尾在截断**之后**收口
/// （首轮 trim + 截断后补一轮 trim）：截断发生在字符流中间，截断点可能
/// 恰好落在中间连字符上（如超长 runner hostname 的第 32 个 char 恰为
/// `-`），只在截断前剥尾会让产出重新以 `-` 结尾。规范化后为空（如输入
/// 全是标点/空白）→ None。网关 hello 必须携带 relay 可接受的 machine
/// （非法值会被 relay 1008 拒绝进入重连死循环），所以装配期先过本函数。
pub fn sanitize_machine_id(raw: &str) -> Option<String> {
    let mapped: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = mapped.trim_matches('-');
    if trimmed.is_empty() {
        return None;
    }
    // 截断后补一轮剥尾：截断点可能恰好落在中间连字符上，截断结果会重新
    // 以 `-` 结尾。二次 trim 不可能引入新的空值分支——首轮 trim 后首字符
    // 必非 `-`，截断取前缀不改变首字符，故二次 trim 至多剥掉尾部、结果
    // 非空；此处空检查纯防御，与首轮空分支同构。
    let truncated = trimmed
        .chars()
        .take(MACHINE_ID_MAX_CHARS)
        .collect::<String>();
    let canonical = truncated.trim_matches('-');
    if canonical.is_empty() {
        return None;
    }
    Some(canonical.to_string())
}

/// 缺省 machine id：hostname 短名（第一个 `.` 前的标签，如
/// `Uzziahs-MacBook-Pro.local` → `Uzziahs-MacBook-Pro`）规范化而来。
///
/// 实现说明：web 依赖清单固定（不为此加 crate），本 crate 又
/// `deny(unsafe_code)`（libc gethostname 需 unsafe），故走 POSIX 标准命令
/// `hostname`（macOS/Linux 均自带）。只在进程启动装配期调用一次，同步
/// 开销可忽略。拿不到/规范化为空 → [`DEFAULT_MACHINE_ID`]。
pub fn default_machine_id() -> String {
    let raw = std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let short = raw.split('.').next().unwrap_or_default();
    sanitize_machine_id(short).unwrap_or_else(|| DEFAULT_MACHINE_ID.to_string())
}

/// 装配期 machine id 解析入口（main build_platform web 分支）：
/// config `web_machine_id` 显式值优先（经 [`sanitize_machine_id`] 规范化），
/// 缺省取 hostname 短名（[`default_machine_id`]）。显式配置规范化后为空
/// → warn 并走缺省链（machine 身份非安全面，坏配置不至于阻断网关启动）。
pub fn resolve_machine_id(configured: Option<&str>) -> String {
    if let Some(c) = configured {
        if let Some(s) = sanitize_machine_id(c.trim()) {
            return s;
        }
        tracing::warn!(target: TARGET, raw = %c, "web_machine_id 规范化后为空，回退 hostname 短名");
    }
    default_machine_id()
}

// ---------------------------------------------------------------------------
// 出站帧（网关 → 浏览器）
// ---------------------------------------------------------------------------

/// relay 单帧上限 256KB（M1 §4.4）；出站软限取 180KB——v1.4 E2E 会话活跃
/// 时业务帧以 `enc` 包装：box = base64(nonce24 || 密文 || tag16)，base64 膨胀
/// 4/3，180KB 明文 → ~246KB enc 帧 < 256KB（信封字段余量已含）。软限统一下
/// 调（明文路径一并收紧——无 E2E 部署只在超巨帧边界上略早截断，语义不变），
/// 避免「按模式选阈值」在 platform（编码方）与 client（加密方）间泄漏模式
/// 感知。超限即截断 text 字段（见 [`encode_text_frame`] / [`encode_card_frame`]）。
pub(crate) const FRAME_SOFT_LIMIT_BYTES: usize = 180 * 1024;
/// 超限截断的头部保窗（char，先例 src/delegate.rs 头尾窗）。
pub(crate) const TRUNC_HEAD_CHARS: usize = 18_000;
/// 超限截断的尾部保窗（结论通常在尾部）。
pub(crate) const TRUNC_TAIL_CHARS: usize = 6_000;

/// 文本帧（send_text 药化）。`machine` 为本机身份（v1.1 起全部出站帧必带，
/// SPA 据此区分多台终端）；`hint` 为协议预留位（web 无 context token，
/// 恒 None 不序列化）；`at` 为该次出站时刻（v1.36 可选字段，unix 毫秒，
/// 与落盘 outbound_log 行 payload.at 同源同值——SPA 按 (text, at) 精确去重
/// 防 history 重放重复；编码方每帧必传 Some，缺席仅是字段可选的向后兼容
/// 语义，见 [`encode_text_frame`]）。
///
/// `self_flag`（v1.41.8 可选字段，序列化键 `self`——`self` 是 Rust 关键字，
/// 字段名加 `_flag` 后缀、serde rename 对齐契约键）：`true` = 该文本是**本
/// 用户自己发出**的消息镜像（[`encode_user_text_frame`]），SPA 按用户气泡
/// 渲染；`false` = 既有语义（agent 文本），不序列化——普通 text 帧字节与
/// v1.41.7 及之前完全一致。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TextFrame {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    machine: String,
    #[serde(rename = "convId")]
    conv_id: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
    #[serde(rename = "self", skip_serializing_if = "std::ops::Not::not")]
    self_flag: bool,
}

/// 卡片帧（send_card → `card` / update_card → `card_patch`；sync 重发用 `card`）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CardFrame {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    machine: String,
    #[serde(rename = "convId")]
    conv_id: String,
    #[serde(rename = "messageId")]
    message_id: String,
    card: CardPayload,
    /// M3 SPA 据此渲染进行中/终态样式：`streaming` = terminal Running，
    /// `done` = 其余（由 [`CardPayload::state`] 派生）。
    state: &'static str,
}

/// 审批请求帧（send_permission_ask 覆写）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ApprovalRequestFrame {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    machine: String,
    #[serde(rename = "convId")]
    conv_id: String,
    #[serde(rename = "requestId")]
    request_id: String,
    tool: String,
    #[serde(rename = "inputSummary")]
    input_summary: String,
    /// v1.37：本次审批的超时预算（= config `permission_ask_timeout_secs`，秒；
    /// 非剩余时间——SPA 以本地收到时刻起倒数）。缺席 = 网关未配置/旧网关，
    /// SPA 不渲染倒计时。
    #[serde(rename = "timeoutSecs", skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
}

/// 审批收敛帧（resolve / cancel / cancel_all 覆写）。decision ∈
/// allow|deny|always（用户真实决定）| cancelled（系统取消——超时/顶替//stop）。
/// 协议另留 `timeout` 值供 M3 细分（M2 的超时路径复用 cancelled）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ApprovalResolvedFrame {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    machine: String,
    #[serde(rename = "convId")]
    conv_id: String,
    #[serde(rename = "requestId")]
    request_id: String,
    decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// pong 帧（client 应答浏览器应用层 ping）。
#[derive(Debug, Serialize)]
pub(crate) struct PongFrame {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    machine: String,
}

/// pong 帧序列化（client 心跳应答用；形状固定无截断面）。
pub(crate) fn encode_pong_frame(machine: &str) -> String {
    serde_json::to_string(&PongFrame {
        v: 1,
        kind: "pong",
        machine: machine.to_string(),
    })
    .unwrap_or_else(|_| {
        // 序列化不可失败（纯 String 字段），兜底手拼保证 pong 恒可发。
        format!(r#"{{"v":1,"type":"pong","machine":"{machine}"}}"#)
    })
}

/// [`OutboundCard`] 的可序列化 DTO（core 类型未派生 serde——web 帧协议自带
/// 形状，M3 SPA 直接消费）。字段与 core 逐一对应，语义见 `types.rs`。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CardPayload {
    pub text: String,
    pub tool_calls: Vec<ToolCallPayload>,
    pub thoughts: Vec<String>,
    pub todos: Vec<TodoPayload>,
    /// thinking | tool_running | outputting | waiting_approval。
    pub phase: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queued_hint: Option<String>,
    pub run_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_display: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_digest: Option<String>,
    /// running | done | error。
    pub terminal: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 单条工具调用记录（[`imagent_core::types::ToolCall`] 的 DTO）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolCallPayload {
    pub name: String,
    pub summary: String,
    pub done: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// #7 diff 视图：unified 风格 diff 文本（core ToolCall.diff 同源透传；
    /// 缺失 = 无内容——非文件修改类工具，或 codex fileChange 仅通知场景下
    /// 协议未携带）。SPA 按字段存在性渲染折叠区。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

/// 任务清单项（[`TodoItem`] 的 DTO）；status: pending | in_progress | completed。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TodoPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub text: String,
    pub status: &'static str,
}

impl CardPayload {
    pub fn from_card(card: &OutboundCard) -> Self {
        CardPayload {
            text: card.text.clone(),
            tool_calls: card
                .tool_calls
                .iter()
                .map(|t| ToolCallPayload {
                    name: t.name.clone(),
                    summary: t.summary.clone(),
                    done: t.done,
                    id: t.id.clone(),
                    diff: t.diff.clone(),
                })
                .collect(),
            thoughts: card.thoughts.clone(),
            todos: card
                .todos
                .iter()
                .map(|t| TodoPayload {
                    id: t.id.clone(),
                    text: t.text.clone(),
                    status: todo_status_str(t.status),
                })
                .collect(),
            phase: phase_str(card.phase),
            queued_hint: card.queued_hint.clone(),
            run_secs: card.run_secs,
            usage_display: card.usage_display.clone(),
            task_digest: card.task_digest.clone(),
            terminal: terminal_str(&card.terminal),
            error: match &card.terminal {
                CardTerminal::Error(e) => Some(e.clone()),
                _ => None,
            },
        }
    }

    /// 帧级 state（§4.2）：Running → `streaming`，其余终态 → `done`。
    pub fn state(&self) -> &'static str {
        if self.terminal == "running" {
            "streaming"
        } else {
            "done"
        }
    }
}

fn phase_str(p: CardPhase) -> &'static str {
    match p {
        CardPhase::Thinking => "thinking",
        CardPhase::ToolRunning => "tool_running",
        CardPhase::Outputting => "outputting",
        CardPhase::WaitingApproval => "waiting_approval",
    }
}

fn terminal_str(t: &CardTerminal) -> &'static str {
    match t {
        CardTerminal::Running => "running",
        CardTerminal::Done => "done",
        CardTerminal::Error(_) => "error",
    }
}

fn todo_status_str(s: TodoStatus) -> &'static str {
    match s {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in_progress",
        TodoStatus::Completed => "completed",
    }
}

/// 头尾窗截断（§4.3，先例 src/delegate.rs）：保头 [`TRUNC_HEAD_CHARS`] /
/// 尾 [`TRUNC_TAIL_CHARS`] char，中段替换为标记。短文原样返回。
pub(crate) fn truncate_text(text: &str) -> String {
    let total = text.chars().count();
    if total <= TRUNC_HEAD_CHARS + TRUNC_TAIL_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(TRUNC_HEAD_CHARS).collect();
    let tail: String = text.chars().skip(total - TRUNC_TAIL_CHARS).collect();
    format!("{head}\n…[已截断]…\n{tail}")
}

/// 序列化 + 超限截断守卫（出站帧公共出口，§4.3）：序列化后 > 240KB 时把
/// `raw_text`（text 帧的正文 / card 帧的 card.text）头尾窗截断后经 `rebuild`
/// 重建帧再序列化，并 log warn。截断只针对 text 类字段——其余字段（工具
/// 记录/思考片段）有 CardSession 累积上限兜底，不构成超限主因。
pub(crate) fn encode_frame<T, F>(frame: &T, raw_text: &str, rebuild: F) -> Result<String>
where
    T: Serialize,
    F: FnOnce(&str) -> T,
{
    let json = serde_json::to_string(frame)
        .map_err(|e| CoreError::Platform(TARGET, format!("出站帧序列化失败：{e}")))?;
    if json.len() <= FRAME_SOFT_LIMIT_BYTES {
        return Ok(json);
    }
    tracing::warn!(
        target: TARGET,
        orig_bytes = json.len(),
        "出站帧超 240KB（relay 单帧上限 256KB），截断 text 后重发"
    );
    let truncated = truncate_text(raw_text);
    serde_json::to_string(&rebuild(&truncated))
        .map_err(|e| CoreError::Platform(TARGET, format!("出站帧截断后序列化失败：{e}")))
}

/// text 帧药化（send_text）。`at` 为该次 send_text 取的出站时刻（unix 毫秒），
/// 调用方必传且与落盘行共享同一值（一次取值同源，v1.36 契约）。
pub(crate) fn encode_text_frame(
    machine: &str,
    conv_id: &str,
    text: &str,
    at: u64,
) -> Result<String> {
    encode_text_frame_inner(machine, conv_id, text, at, false)
}

/// v1.41.8 用户自发文本镜像帧药化（`Platform::send_user_text`）：与
/// [`encode_text_frame`] 仅 `self_flag` 不同（帧多一个 `"self":true` 键），
/// `at` 同源契约与超限截断完全一致。
pub(crate) fn encode_user_text_frame(
    machine: &str,
    conv_id: &str,
    text: &str,
    at: u64,
) -> Result<String> {
    encode_text_frame_inner(machine, conv_id, text, at, true)
}

/// text 帧药化公共实现（普通 / 用户镜像两形态共用——帧形、`at` 同源、超限
/// 截断后重建不丢 `self` 标记，只在组帧出口一处维护）。
fn encode_text_frame_inner(
    machine: &str,
    conv_id: &str,
    text: &str,
    at: u64,
    self_flag: bool,
) -> Result<String> {
    let frame = TextFrame {
        v: 1,
        kind: "text",
        machine: machine.to_string(),
        conv_id: conv_id.to_string(),
        text: text.to_string(),
        at: Some(at),
        hint: None,
        self_flag,
    };
    encode_frame(&frame, text, |t| TextFrame {
        text: t.to_string(),
        ..frame.clone()
    })
}

/// card / card_patch 帧药化（send_card / update_card / sync 重发共用）。
/// `kind` 取 "card" | "card_patch"。
pub(crate) fn encode_card_frame(
    machine: &str,
    kind: &'static str,
    conv_id: &str,
    message_id: &str,
    card: &CardPayload,
) -> Result<String> {
    let frame = CardFrame {
        v: 1,
        kind,
        machine: machine.to_string(),
        conv_id: conv_id.to_string(),
        message_id: message_id.to_string(),
        card: card.clone(),
        state: card.state(),
    };
    encode_frame(&frame, &card.text, |t| CardFrame {
        card: CardPayload {
            text: t.to_string(),
            ..card.clone()
        },
        ..frame.clone()
    })
}

/// approval_request 帧药化（send_permission_ask）。inputSummary 经
/// [`imagent_core::render::tool_summary`] 压缩（≤80 char），无超限面。
/// `timeout_secs` 为本次审批的超时预算（秒；None = 网关未配置/旧网关，
/// 帧不带该字段）。
pub(crate) fn encode_approval_request_frame(
    machine: &str,
    conv_id: &str,
    request_id: &str,
    tool: &str,
    input_summary: &str,
    timeout_secs: Option<u64>,
) -> Result<String> {
    let frame = ApprovalRequestFrame {
        v: 1,
        kind: "approval_request",
        machine: machine.to_string(),
        conv_id: conv_id.to_string(),
        request_id: request_id.to_string(),
        tool: tool.to_string(),
        input_summary: input_summary.to_string(),
        timeout_secs,
    };
    serde_json::to_string(&frame)
        .map_err(|e| CoreError::Platform(TARGET, format!("approval 帧序列化失败：{e}")))
}

/// approval_resolved 帧药化（resolve / cancel / cancel_all）。
pub(crate) fn encode_approval_resolved_frame(
    machine: &str,
    conv_id: &str,
    request_id: &str,
    decision: &str,
    message: Option<&str>,
) -> Result<String> {
    let frame = ApprovalResolvedFrame {
        v: 1,
        kind: "approval_resolved",
        machine: machine.to_string(),
        conv_id: conv_id.to_string(),
        request_id: request_id.to_string(),
        decision: decision.to_string(),
        message: message.map(String::from),
    };
    serde_json::to_string(&frame)
        .map_err(|e| CoreError::Platform(TARGET, format!("approval_resolved 帧序列化失败：{e}")))
}

// ---------------------------------------------------------------------------
// approvals 快照帧出站（协议 v1.42：浏览器 approvals_query 的应答）
// ---------------------------------------------------------------------------

/// approvals 快照帧的单个条目（数据来源
/// [`crate::platform::ApprovalSnapshotItem`]；字段名逐字对齐 SPA 契约）。
#[derive(Serialize)]
struct ApprovalItemFrame<'a> {
    #[serde(rename = "convId")]
    conv_id: &'a str,
    #[serde(rename = "requestId")]
    request_id: &'a str,
    tool: &'a str,
    #[serde(rename = "inputSummary")]
    input_summary: &'a str,
    /// 剩余秒数（整数 ≥1）。与 approval_request 帧的 timeoutSecs 同名不同义：
    /// 那边是超时预算，这边是剩余时限——SPA 以「收到时刻 + timeoutSecs」恢复
    /// 倒计时。
    #[serde(rename = "timeoutSecs")]
    timeout_secs: u64,
}

/// approvals 快照帧（struct 序列化保持声明序——本文件既有纪律）。`items`
/// 为空时输出 `"items":[]`，**不省略键**：空数组即「当前没有待审批」的权威
/// 结论，SPA 据此清空面板里离开期间已超时 / 已解决的审批。
#[derive(Serialize)]
struct ApprovalsFrame<'a> {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    machine: &'a str,
    items: Vec<ApprovalItemFrame<'a>>,
}

/// approvals 快照帧药化（platform 的 sync 泵处理 approvals_query 后调用）。
/// 共享契约字节级：`{"v":1,"type":"approvals","machine":…,"items":[…]}`
/// （条目字段 convId / requestId / tool / inputSummary / timeoutSecs）。
pub(crate) fn encode_approvals_frame(
    machine: &str,
    items: &[ApprovalSnapshotItem],
) -> Result<String> {
    let frame = ApprovalsFrame {
        v: 1,
        kind: "approvals",
        machine,
        items: items
            .iter()
            .map(|i| ApprovalItemFrame {
                conv_id: &i.conv_id,
                request_id: &i.request_id,
                tool: &i.tool_name,
                input_summary: &i.summary,
                timeout_secs: i.timeout_secs,
            })
            .collect(),
    };
    serde_json::to_string(&frame)
        .map_err(|e| CoreError::Platform(TARGET, format!("approvals 帧序列化失败：{e}")))
}

// ---------------------------------------------------------------------------
// windows 快照帧出站（协议 v1.43：本地窗口看板）
// ---------------------------------------------------------------------------

/// `window_watch` 帧的 `name` 长度上限（字符）。窗口名是注册表里的短标识
/// （如 `imagent-a7`），上限只为挡畸形帧。
pub(crate) const WINDOW_NAME_MAX_CHARS: usize = 256;

/// windows 快照帧的单个窗口条目（字段名逐字对齐 SPA 契约；`?` 字段为 None
/// 时省略键）。
#[derive(Serialize)]
struct WindowItemFrame<'a> {
    name: &'a str,
    status: &'static str,
    #[serde(rename = "waitingFor", skip_serializing_if = "Option::is_none")]
    waiting_for: Option<&'a str>,
    cwd: &'a str,
    #[serde(rename = "startedAt", skip_serializing_if = "Option::is_none")]
    started_at: Option<i64>,
    #[serde(rename = "statusUpdatedAt", skip_serializing_if = "Option::is_none")]
    status_updated_at: Option<i64>,
    watched: bool,
}

/// windows 快照帧（struct 序列化保持声明序：`v`、`type`、`machine`、
/// `hookInstalled`、`items`）。`items` 为空时输出 `"items":[]`，**不省略键**
/// ——空数组即「peer 通道已启用但当前没有在线窗口」的权威结论。
#[derive(Serialize)]
struct WindowsFrame<'a> {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    machine: &'a str,
    #[serde(rename = "hookInstalled")]
    hook_installed: bool,
    items: Vec<WindowItemFrame<'a>>,
}

/// `status` 的帧字面量（与注册表取值同名；`WindowStatus::Unknown` →
/// `"unknown"`——注册表里的值不是 busy / idle / waiting 三者之一或缺失）。
fn window_status_str(status: &WindowStatus) -> &'static str {
    match status {
        WindowStatus::Busy => "busy",
        WindowStatus::Idle => "idle",
        WindowStatus::Waiting => "waiting",
        WindowStatus::Unknown => "unknown",
    }
}

/// windows 快照帧药化（`windows_query` 的应答、`window_watch` 后的重发、轮询
/// 发现变化时的主动推送共用）。`windows` 须已按 core `sort_windows` 排好
/// （`PeerState::windows()` 的产出即是）——帧字节可复现。
pub(crate) fn encode_windows_frame(
    machine: &str,
    hook_installed: bool,
    windows: &[WindowInfo],
) -> Result<String> {
    let frame = WindowsFrame {
        v: 1,
        kind: "windows",
        machine,
        hook_installed,
        items: windows
            .iter()
            .map(|w| WindowItemFrame {
                name: &w.name,
                status: window_status_str(&w.status),
                waiting_for: w.waiting_for.as_deref(),
                cwd: &w.cwd,
                started_at: w.started_at,
                status_updated_at: w.status_updated_at,
                watched: w.watched,
            })
            .collect(),
    };
    serde_json::to_string(&frame)
        .map_err(|e| CoreError::Platform(TARGET, format!("windows 帧序列化失败：{e}")))
}

// ---------------------------------------------------------------------------
// push_notify 帧出站（协议 v1.10：网关 → 中继，明文管理帧族）
// ---------------------------------------------------------------------------

/// push_notify 帧的 kind（协议 v1.10 共享契约三值 + v1.42 第四值 + v1.43 两个
/// 窗口事件）：回合终态（done/error）、审批请求、窗口回信（v1.42 `peer_reply`
/// ——本地 Claude Code 窗口经 peer 通道回给驾驶舱的文本，此前纯文本帧无卡片
/// 终态，从不推送），以及本地窗口「跑完一轮」/「进入 waiting」（v1.43）。
/// 字面量与中继 `parse_notify_frame` 白名单、SPA 通知文案三端逐字对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushNotifyKind {
    /// card 终态 done（回合正常结束）。
    RoundDone,
    /// card 终态 error（回合出错）。
    RoundError,
    /// approval_request 出站。
    Approval,
    /// v1.42 peer 入站回复已落 `peer:<窗口名>` 会话（`Platform::on_peer_reply`
    /// 钩子；conv_id 即该 peer 会话的完整 conv，title = 回信正文摘要）。
    PeerReply,
    /// v1.43 本地窗口跑完一轮（被关注或本次运行里驾驶舱给它发过消息的窗口；
    /// conv_id = `peer:<窗口名>`，title = 窗口名）。
    WindowTurnEnd,
    /// v1.43 本地窗口进入 waiting——卡住等人（所有窗口都推；conv_id / title 同上）。
    WindowWaiting,
}

impl PushNotifyKind {
    /// 帧上的字符串形态（共享契约逐字）。
    pub fn as_str(&self) -> &'static str {
        match self {
            PushNotifyKind::RoundDone => "round_done",
            PushNotifyKind::RoundError => "round_error",
            PushNotifyKind::Approval => "approval",
            PushNotifyKind::PeerReply => "peer_reply",
            PushNotifyKind::WindowTurnEnd => "window_turn_end",
            PushNotifyKind::WindowWaiting => "window_waiting",
        }
    }
}

/// 埋点事件（platform → client 的 push 通道载荷；帧形状见
/// [`encode_push_notify_frame`]）。SPA 在线时中继会跳过——网关无脑发，
/// 判定在中继（按订阅 session 的活跃 WS）。
#[derive(Debug, Clone)]
pub(crate) struct PushEvent {
    pub kind: PushNotifyKind,
    pub conv_id: String,
    pub title: String,
}

/// push_notify 帧 title 上限（共享契约：≤32 字摘要——终态卡 task/审批工
/// 具名/v1.42 窗口回信正文开头）。按 Unicode 码点截（与
/// SESSION_TITLE_MAX_CHARS 同口径，中文/emoji 不劈半）。
pub(crate) const PUSH_TITLE_MAX_CHARS: usize = 32;

/// title 摘要规整（纯函数）：trim 后按码点截 [`PUSH_TITLE_MAX_CHARS`]。
pub(crate) fn push_notify_title(raw: &str) -> String {
    raw.trim().chars().take(PUSH_TITLE_MAX_CHARS).collect()
}

/// push_notify 帧形状（struct 序列化保持声明序——与 lib.rs 其余出站帧同款
/// 纪律；`json!` 宏默认 BTreeMap 键序会破坏字节级契约）。
#[derive(Serialize)]
struct PushNotifyFrame<'a> {
    v: u8,
    r#type: &'static str,
    machine: &'a str,
    kind: &'static str,
    #[serde(rename = "convId")]
    conv_id: &'a str,
    title: &'a str,
}

/// push_notify 帧药化（网关 → 中继，明文管理帧族——不经 E2E seal，与
/// device_list 等管理帧同路：中继需明文读取 kind/machine/convId 做订阅投递
/// 与在线免打扰判定）。**title 是明文经中继的**：契约取 ≤32 码点摘要
/// （终态卡 task / 审批工具名 / v1.42 起窗口回信正文的开头）——业务正文
/// 本体仍受 E2E 保护，但这截摘要不在中继盲化范围内，属既有设计的已知
/// 取舍（见 relay push.rs 模块头）。共享契约字节级：
/// `{"v":1,"type":"push_notify","machine":…,"kind":…,"convId":…,"title":…}`。
pub(crate) fn encode_push_notify_frame(machine: &str, ev: &PushEvent) -> String {
    serde_json::to_string(&PushNotifyFrame {
        v: 1,
        r#type: "push_notify",
        machine,
        kind: ev.kind.as_str(),
        conv_id: &ev.conv_id,
        title: &ev.title,
    })
    .expect("push_notify 帧字段均为可序列化标量，序列化不可失败")
}

// ---------------------------------------------------------------------------
// 出站工作空间帧（协议 v1.6，字段与 SPA 侧契约逐字一致，不得增删改）
// ---------------------------------------------------------------------------

/// workspaces 快照帧的单个空间条目。`lastUsedAt` 缺席不序列化（旧值/刚
/// 保存未被用过）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WorkspaceEntryFrame {
    pub(crate) name: String,
    pub(crate) path: String,
    #[serde(rename = "lastUsedAt", skip_serializing_if = "Option::is_none")]
    pub(crate) last_used_at: Option<u64>,
}

/// 会话显示名上限（协议 v1.7：按 Unicode 码点截 24，与 SPA
/// CONV_TITLE_MAX_CHARS 同口径——中文/emoji 均按 1 码点计）。
pub(crate) const SESSION_TITLE_MAX_CHARS: usize = 24;

/// 会话 title 规则（协议 v1.7 契约 B + v1.9 语义标题扩展，纯函数便于单
/// 测）：手动名（SessionRow.name，trim 后非空）优先；否则语义标题
/// （sessions.title 列，headless CLI 生成，trim 后非空，按码点截
/// [`SESSION_TITLE_MAX_CHARS`]——生成时已清洗截断，此处防御性再截）；
/// 否则 first_prompt 单行摘要——空白序列（含换行/制表）折叠为单空格后按
/// 码点截 [`SESSION_TITLE_MAX_CHARS`]；三者皆无 → 空串（网关不编造显示
/// 名，SPA 侧空 title 自行兜底渲染）。
pub(crate) fn session_title(
    name: Option<&str>,
    title: Option<&str>,
    first_prompt: Option<&str>,
) -> String {
    if let Some(manual) = name.map(str::trim).filter(|n| !n.is_empty()) {
        return manual.to_string();
    }
    if let Some(semantic) = title.map(str::trim).filter(|t| !t.is_empty()) {
        return semantic.chars().take(SESSION_TITLE_MAX_CHARS).collect();
    }
    let prompt = first_prompt.unwrap_or_default().trim();
    let mut collapsed = String::with_capacity(prompt.len());
    let mut pending_space = false;
    for ch in prompt.chars() {
        if ch.is_whitespace() {
            pending_space = true;
        } else {
            if pending_space {
                collapsed.push(' ');
                pending_space = false;
            }
            collapsed.push(ch);
        }
    }
    collapsed.chars().take(SESSION_TITLE_MAX_CHARS).collect()
}

/// workspaces 快照帧的单个会话历史条目（协议 v1.7 可选 sessions 段）：该
/// 网关 `web:` 前缀会话行。`title` 见 [`session_title`]；`updated_at` 为
/// store sessions 行的 unix 秒（serde rename 对齐契约 `updatedAt`）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct SessionEntryFrame {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(rename = "updatedAt")]
    pub(crate) updated_at: u64,
}

/// v1.8 模型菜单单项（models 段 options 元素，字段名按契约不得漂移）：
/// `label` = 菜单显示名（别名 sonnet/opus/haiku 或官方名）；`target` =
/// 第三方映射的实际模型 id（**无映射序列化为 null**——契约示例
/// `{"label":"opus","target":null}`，非缺席）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ModelOption {
    pub(crate) label: String,
    pub(crate) target: Option<String>,
}

/// v1.8 workspaces 快照帧可选 models 段（CLI 配置优先 + 网关官方列表回退，
/// 汇聚逻辑见 [`crate::models::resolve_models`]）。`current` = backend.model()
/// （引擎实例默认，null = 未设置跟随 CLI 默认；v1.38 会话级 /model 覆盖不进
/// 此快照——以 IM /model 回显为准）；`cliDefault` =
/// settings.json env.ANTHROPIC_MODEL（serde rename 对齐契约 `cliDefault`，
/// 无则 null）；`options` = 菜单项（≥1 才由调用方序列化整段，见
/// [`encode_workspaces_frame`]）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ModelsFrame {
    pub(crate) current: Option<String>,
    #[serde(rename = "cliDefault")]
    pub(crate) cli_default: Option<String>,
    pub(crate) options: Vec<ModelOption>,
}

/// v1.36 引擎选择段（workspaces 快照帧可选 backends 段，新会话弹窗引擎
/// chips 的数据源）：`current` = 网关默认引擎名（default backend 的
/// `name()`——未设 `backend:<conv>` KV 的会话实际用的引擎）；`options` =
/// 全注册表名（default + extras，保序去重，default 在首位）。名单由 main
/// 装配处一次性注入（纯字符串透传，无 IO）；空 options 整段不序列化（见
/// [`encode_workspaces_frame`]）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct BackendsFrame {
    pub(crate) current: String,
    pub(crate) options: Vec<String>,
}

/// workspaces 全量快照帧（SPA 工作空间视图的单一数据来源）：
/// `default` = 配置 default_workdir；`workspaces` = 全部命名空间（调用方
/// 按 name 稳定序排好）；`convs` = 该网关全部已知 conv 的当前 workdir
///（遍历 `workdir:` KV，**含 local: 会话**——SPA 展示本地终端会话目录
/// 同样有意义）。`machine` 为本机身份（v1.1 起全部出站业务帧必带——SPA
/// reducer 按 `frame.machine` 把快照归属到机器；enc 信封的 machine 要经
/// 解密分发处兜底注入才可达内帧，内帧自带才是权威）。`convs` 用
/// BTreeMap：键序稳定，SPA 解析与测试断言可复现。v1.7 起可带可选
/// `sessions` 段（`web:` 会话历史，见 [`SessionEntryFrame`]）；v1.8 起可带
/// 可选 `models` 段（模型菜单动态化，见 [`ModelsFrame`]）；v1.36 起可带
/// 可选 `backends` 段（引擎名单，见 [`BackendsFrame`]）；v1.40 起可带可选
/// `asr` 布尔能力位（语音转写已配置，字面 `true` 非对象段）——各段空值
/// 整段不序列化（无对应数据的网关快照与 v1.6 逐字节一致，旧端零感知）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkspacesFrame {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    machine: String,
    default: String,
    workspaces: Vec<WorkspaceEntryFrame>,
    convs: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sessions: Vec<SessionEntryFrame>,
    #[serde(skip_serializing_if = "Option::is_none")]
    models: Option<ModelsFrame>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backends: Option<BackendsFrame>,
    /// v1.40 语音转写能力位：whisper_bin 与 whisper_model 均已配置 = true。
    /// bool 的 skip 用 `std::ops::Not::not`（false 不序列化——契约「缺省/
    /// false 不序列化，旧形态字节不变」；仓库内首个 bool 段，无既有惯例
    /// 可循，以此形态落地）。
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    asr: bool,
}

/// conv_workdir 增量帧：单个 conv 的工作目录已切换（网页 conv_workdir_set
/// 成功，或终端 /cd 经 [`Platform::on_workdir_changed`] 的通知）。SPA 收到
/// 后局部更新；下一条 workspaces 快照天然兜底。`machine` 同
/// [`WorkspacesFrame`]（v1.1 出站帧必带）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ConvWorkdirFrame {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    machine: String,
    #[serde(rename = "convId")]
    conv_id: String,
    workdir: String,
}

/// error 帧（工作空间校验失败复用）：code=workspace_invalid + 中文人话
/// 原因。`machine` 为本机身份（v1.1 出站帧必带；SPA error 渲染不依赖
/// machine，多余字段宽松兼容）。与 relay 自产的 error 帧同渲染路径
/// （relay 自产帧无 machine——那是 relay 侧帧，不经网关）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ErrorFrame {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    machine: String,
    code: String,
    message: String,
}

/// workspaces 快照帧组帧。无截断面（无自由文本大字段；convs/sessions 量级
/// = 会话数，远低于帧软限）。`sessions` 传空 Vec 时整段不序列化（旧形态
/// 字节不变，协议 v1.7 可选段语义）；`models` 传 None 或 options 空的帧时
/// 整段不序列化（协议 v1.8 同语义——契约「≥1 才序列化整段」）；`backends`
/// 传 None 或 options 空的帧时同整段缺席（协议 v1.36 同语义）；`asr` 传
/// false 整段缺席（协议 v1.40 同语义——true 序列化为字面 `"asr":true`）。
#[allow(clippy::too_many_arguments)] // 参数已达 8——v1.40 注入 asr 能力位（先例 new_with_e2e）
pub(crate) fn encode_workspaces_frame(
    machine: &str,
    default: &str,
    workspaces: Vec<WorkspaceEntryFrame>,
    convs: BTreeMap<String, String>,
    sessions: Vec<SessionEntryFrame>,
    models: Option<ModelsFrame>,
    backends: Option<BackendsFrame>,
    asr: bool,
) -> Result<String> {
    // 契约「options ≥1 才序列化」在组帧出口统一兜底（调用方无需各自判断）。
    let models = models.filter(|m| !m.options.is_empty());
    let backends = backends.filter(|b| !b.options.is_empty());
    let frame = WorkspacesFrame {
        v: 1,
        kind: "workspaces",
        machine: machine.to_string(),
        default: default.to_string(),
        workspaces,
        convs,
        sessions,
        models,
        backends,
        asr,
    };
    serde_json::to_string(&frame)
        .map_err(|e| CoreError::Platform(TARGET, format!("workspaces 帧序列化失败：{e}")))
}

/// conv_workdir 增量帧组帧。
pub(crate) fn encode_conv_workdir_frame(
    machine: &str,
    conv_id: &str,
    workdir: &str,
) -> Result<String> {
    let frame = ConvWorkdirFrame {
        v: 1,
        kind: "conv_workdir",
        machine: machine.to_string(),
        conv_id: conv_id.to_string(),
        workdir: workdir.to_string(),
    };
    serde_json::to_string(&frame)
        .map_err(|e| CoreError::Platform(TARGET, format!("conv_workdir 帧序列化失败：{e}")))
}

/// error 帧组帧（工作空间错误复用，code 见 platform 泵）。
pub(crate) fn encode_error_frame(machine: &str, code: &str, message: &str) -> Result<String> {
    let frame = ErrorFrame {
        v: 1,
        kind: "error",
        machine: machine.to_string(),
        code: code.to_string(),
        message: message.to_string(),
    };
    serde_json::to_string(&frame)
        .map_err(|e| CoreError::Platform(TARGET, format!("error 帧序列化失败：{e}")))
}

// ---------------------------------------------------------------------------
// 出站历史帧（协议 v1.8 会话历史重放，字段与 SPA 侧契约逐字一致，不得增删改）
// ---------------------------------------------------------------------------

/// history 帧的 entries 元素：行记录 → 契约 JSON（纯函数便于单测）。
/// - text 行：`{"kind":"text","key":"h:<seq>","text","at"}`（key 的 seq 用
///   行 seq；text/at 取自落盘 payload）；payload 带 `"self":true`（v1.41.8
///   用户自发文本镜像的落盘行，见 [`encode_user_text_frame`]）时条目追加
///   同名键 `"self":true`——缺席 / false / 非布尔不输出键（旧行形态不变）；
/// - card 行：`{"kind":"card","key":"h:<seq>","messageId","card","state":
///   "done"}`（card 为落盘的 CardPayload 完整 JSON 原样内嵌；state 恒
///   "done"——只有终态帧落盘）。
///
/// payload 畸形 / kind 未知 → None（调用方跳过该行；历史重放 best-effort，
/// 单行损坏不拖垮整帧）。
pub(crate) fn history_entry_from_row(
    row: &imagent_store::OutboundRow,
) -> Option<serde_json::Value> {
    let payload: serde_json::Value = serde_json::from_str(&row.payload).ok()?;
    match row.kind.as_str() {
        "text" => {
            let text = payload.get("text").and_then(|t| t.as_str())?;
            let at = payload.get("at").and_then(|a| a.as_i64())?;
            let mut entry = serde_json::json!({
                "kind": "text",
                "key": format!("h:{}", row.seq),
                "text": text,
                "at": at,
            });
            // v1.41.8：仅布尔 true 透传（false / 非布尔按缺席——不输出键）。
            if payload.get("self").and_then(|s| s.as_bool()) == Some(true) {
                entry["self"] = serde_json::Value::Bool(true);
            }
            Some(entry)
        }
        "card" => {
            let message_id = payload.get("messageId").and_then(|m| m.as_str())?;
            let card = payload.get("card")?.clone();
            Some(serde_json::json!({
                "kind": "card",
                "key": format!("h:{}", row.seq),
                "messageId": message_id,
                "card": card,
                "state": "done",
            }))
        }
        other => {
            tracing::warn!(
                target: TARGET,
                kind = other,
                seq = row.seq,
                "outbound_log 行 kind 未知，history 条目跳过"
            );
            None
        }
    }
}

/// history 回放帧（协议 v1.8）：`entries` 为 [`history_entry_from_row`]
/// 组好的条目数组（seq 升序，≤500 条）；`lastSeq` 为该 conv 当前最大 seq
///（无任何记录时 0——浏览器只更新水位）；`trimmed` 见
/// [`imagent_store::OutboundPage`]。`machine` 为本机身份（v1.1 出站帧必带）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct HistoryFrame {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    machine: String,
    #[serde(rename = "convId")]
    conv_id: String,
    entries: serde_json::Value,
    #[serde(rename = "lastSeq")]
    last_seq: i64,
    trimmed: bool,
}

/// history 帧组帧（无截断面：entries 量级由 store GC 封顶 500 条）。
pub(crate) fn encode_history_frame(
    machine: &str,
    conv_id: &str,
    entries_json: serde_json::Value,
    last_seq: i64,
    trimmed: bool,
) -> Result<String> {
    let frame = HistoryFrame {
        v: 1,
        kind: "history",
        machine: machine.to_string(),
        conv_id: conv_id.to_string(),
        entries: entries_json,
        last_seq,
        trimmed,
    };
    serde_json::to_string(&frame)
        .map_err(|e| CoreError::Platform(TARGET, format!("history 帧序列化失败：{e}")))
}

// ---------------------------------------------------------------------------
// 出站语音转写帧（协议 v1.40 whisper 方案网关侧，字段与 SPA 侧契约逐字一致，不得增删改）
// ---------------------------------------------------------------------------

/// asr_result 帧（v1.40 语音转写成功回帧）：`text` = whisper stdout 多行
/// 合并单行空格拼接后的识别文本（**空串也算成功**——静音/没听清由 SPA
/// 端提示）；`durationMs` = 转写耗时（unix 墙钟差，毫秒，>0——mock 场景
/// 也为正，spawn+读 stdout 至少毫秒级）。`machine` 为本机身份（v1.1 出站
/// 业务帧必带）。入站请求的 `durationMs`（录音时长）与出站的同名不同义，
/// 不回传（契约如此——SPA 本地自有录音时长）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AsrResultFrame {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    machine: String,
    text: String,
    #[serde(rename = "durationMs")]
    duration_ms: u64,
}

/// asr_result 帧组帧（无截断面：whisper 文本量级 ≤ 录音时长，远低于帧软限）。
pub(crate) fn encode_asr_result_frame(
    machine: &str,
    text: &str,
    duration_ms: u64,
) -> Result<String> {
    let frame = AsrResultFrame {
        v: 1,
        kind: "asr_result",
        machine: machine.to_string(),
        text: text.to_string(),
        duration_ms,
    };
    serde_json::to_string(&frame)
        .map_err(|e| CoreError::Platform(TARGET, format!("asr_result 帧序列化失败：{e}")))
}

// ---------------------------------------------------------------------------
// 出站送达回执 / 网关信息帧（协议 v1.44，字段与 SPA 侧契约逐字一致，不得增删改）
// ---------------------------------------------------------------------------

/// 本网关支持的可选能力字面量（`gateway_info` 帧 `caps` 数组的全部取值，
/// 排序稳定）。目前只有 `acks`（支持送达回执）——SPA 据此决定是否给用户
/// 消息显示「发送中 / 未送达」状态；旧网关不回 `gateway_info` → 视为不支持。
const GATEWAY_CAPS: &[&str] = &["acks"];

/// `malformed` 回执的 reason / message 字面量（契约 §2.2 第四行）。其余三种
/// reason（`not_allowed` / `queue_full` / `peer_unreachable`）由 Dispatcher 的
/// 丢弃点上报，字面量在 core 的 `MessageRejectReason`（单一事实源）；
/// `malformed` 是 web client 层自己产生的，字面量留在本 crate。
pub(crate) const ACK_MALFORMED_REASON: &str = "malformed";
pub(crate) const ACK_MALFORMED_MESSAGE: &str = "消息格式无效";

/// ack 帧的状态（协议 v1.44）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AckStatus {
    /// 网关已收到并接下这条消息（消息进入平台入站 channel 之后由 client 层
    /// / 媒体泵发出）。证明的是「到达了网关」，不带 `reason` / `message`。
    Received,
    /// Dispatcher / client 层在明确的丢弃点拒绝。
    Rejected {
        /// 拒绝原因字面量（`reason` 键）。
        reason: &'static str,
        /// 中文人话（`message` 键）。
        message: &'static str,
    },
}

impl AckStatus {
    /// `status` 键的字面量。
    fn status_str(&self) -> &'static str {
        match self {
            AckStatus::Received => "received",
            AckStatus::Rejected { .. } => "rejected",
        }
    }
}

/// ack 帧（struct 序列化保持声明序：`v`、`type`、`machine`、`clientId`、
/// `convId`、`status`、`reason`、`message`）。`reason` / `message` 只在
/// `rejected` 时出现（`received` 省略这两个键——契约键序固定）。
#[derive(Serialize)]
struct AckFrame<'a> {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    machine: &'a str,
    #[serde(rename = "clientId")]
    client_id: &'a str,
    #[serde(rename = "convId")]
    conv_id: &'a str,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'static str>,
}

/// ack 帧药化（送达回执）。`machine` 为本机身份；`conv` 为完整 conv（帧内
/// convId 协议约定）。随其它出站业务帧逐会话加密（经 outbound channel →
/// client 的 send_business），所有在线标签页 / 设备都会收到，各自按 clientId
/// 对回自己发出的消息、对不上的忽略。
pub(crate) fn encode_ack_frame(
    machine: &str,
    client_id: &str,
    conv: &str,
    status: AckStatus,
) -> Result<String> {
    let (status_str, reason, message) = match status {
        AckStatus::Received => (status.status_str(), None, None),
        AckStatus::Rejected { reason, message } => {
            (status.status_str(), Some(reason), Some(message))
        }
    };
    let frame = AckFrame {
        v: 1,
        kind: "ack",
        machine,
        client_id,
        conv_id: conv,
        status: status_str,
        reason,
        message,
    };
    serde_json::to_string(&frame)
        .map_err(|e| CoreError::Platform(TARGET, format!("ack 帧序列化失败：{e}")))
}

/// gateway_info 帧（struct 序列化保持声明序：`v`、`type`、`machine`、
/// `version`、`caps`）。`gateway_info_query` 的应答。
#[derive(Serialize)]
struct GatewayInfoFrame<'a> {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    machine: &'a str,
    version: &'static str,
    caps: &'static [&'static str],
}

/// gateway_info 帧药化（`gateway_info_query` 的应答）。`version` = 本 crate
/// 版本（workspace 统一版本号 = 网关版本）；管理页据此显示「网关 v{version}」
/// ——此前网关版本任何帧都不透出，排查「发版后没更新」只能上机器看。
pub(crate) fn encode_gateway_info_frame(machine: &str) -> Result<String> {
    let frame = GatewayInfoFrame {
        v: 1,
        kind: "gateway_info",
        machine,
        version: env!("CARGO_PKG_VERSION"),
        caps: GATEWAY_CAPS,
    };
    serde_json::to_string(&frame)
        .map_err(|e| CoreError::Platform(TARGET, format!("gateway_info 帧序列化失败：{e}")))
}

// ---------------------------------------------------------------------------
// 入站帧（浏览器 → 网关）映射
// ---------------------------------------------------------------------------

/// message 帧的可选媒体载荷 DTO（协议 v1.7，契约字段名 `name`/`mime`/`b64`
/// 不得偏离）：浏览器侧 canvas 压缩后的图片 base64（单行无换行）。本层只做
/// 字段存在性/类型检查（见 [`parse_media_payload`]）；值域校验（mime 须
/// image/*、b64 解码 ≤8MB）与落盘在 platform 媒体泵（IO + 失败须回 error 帧，
/// 不适合纯函数层）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MediaPayload {
    pub(crate) name: String,
    pub(crate) mime: String,
    pub(crate) b64: String,
}

/// media 段解析（纯函数）：name/mime/b64 三字段齐备且均为字符串才 Some。
/// 无 media 段（`v` 为 None）与字段缺失/类型错同样返回 None——调用方以
/// 「帧内 media 字段是否存在」区分两者（不存在 → 纯文本 message 旧路径；
/// 存在但解析失败 → Warn 丢弃）。
fn parse_media_payload(v: Option<&serde_json::Value>) -> Option<MediaPayload> {
    let m = v?;
    Some(MediaPayload {
        name: m.get("name")?.as_str()?.to_string(),
        mime: m.get("mime")?.as_str()?.to_string(),
        b64: m.get("b64")?.as_str()?.to_string(),
    })
}

/// 浏览器入站帧分拣结果（client 侧消费；纯函数便于单测）。
#[derive(Debug)]
pub(crate) enum InboundAction {
    /// 业务消息（message / approval_response / control）→ inbound channel →
    /// `Platform::recv`。（Box 消除变体间大 size 差——InboundMessage 含多个
    /// Vec，clippy large_enum_variant。）
    Message(Box<InboundMessage>),
    /// v1.7 媒体上行（message 帧带可选 media 段）→ platform 媒体泵：值域
    /// 校验（mime 须 image/*、b64 合法、解码 ≤8MB）+ 落盘
    /// `<imagent_home>/media/up-<unix_ms>-<安全化name>` 后合成 InboundMessage
    /// 投回 inbound channel；任一步失败回 error 帧
    /// （media_too_large / media_invalid）且不投递。`conv` 已归一；`text`
    /// 为可选说明（纯图消息缺省/空串——泵内归一为 text=None）。`client_id`
    /// （v1.44 送达回执关联 id，经 [`parse_client_id`]）随动作带到媒体泵——
    /// 回执要在合成消息**进入入站 channel 之后**才发，泵是唯一知道这一时刻
    /// 的地方。
    MediaMessage {
        conv: String,
        text: Option<String>,
        media: MediaPayload,
        client_id: Option<String>,
    },
    /// sync 请求 → sync responder 重发缓存卡片（不进 recv）。
    Sync,
    /// v1.42 待审批快照请求（approvals_query 帧）→ sync responder 按
    /// [`crate::platform::PendingAsks`] 回 `approvals` 权威快照（不进 recv /
    /// dispatch 命令链；与 `Sync` 共用通道与泵）。无必填字段——`machine` 由
    /// relay 路由用，网关不消费。
    ApprovalsQuery,
    /// v1.43 本地窗口快照请求（windows_query 帧）→ sync responder 按 peer 共享态
    /// 回 `windows` 权威快照（不进 recv / dispatch 命令链；与 `Sync` /
    /// `ApprovalsQuery` 共用通道与泵）。无必填字段——`machine` 由 relay 路由
    /// 用，网关不消费。peer 通道未启用的网关不回帧。
    WindowsQuery,
    /// v1.44 网关信息查询（gateway_info_query 帧）：client 层直接回
    /// `gateway_info{machine, version, caps}`（无必填字段、不依赖任何共享态，
    /// 不进 recv / dispatch 命令链，也不必绕 sync 泵）。SPA 据应答里的 `caps`
    /// 判定该网关是否支持送达回执；旧网关按未知 type 忽略 → SPA 判定不支持。
    GatewayInfoQuery,
    /// v1.43 关注 / 取消关注一个本地窗口（window_watch{name, watch} 帧）：
    /// client 层直接改 peer 共享态的关注名单（跨设备的全局状态，由网关持久化
    /// 在 store `config` 表——前端不存名单、不对账），再经 sync 泵重发一帧
    /// `windows` 让所有设备的 `watched` 收敛（不进 recv / dispatch 命令链）。
    /// `name` 非空字符串 ≤ [`WINDOW_NAME_MAX_CHARS`]，`watch` 布尔。
    WindowWatch {
        name: String,
        watch: bool,
    },
    /// 应用层 ping → client 直接回 pong（不进 recv）。
    Ping,
    /// v1.6 工作空间（workspace_list / workspace_save / workspace_remove /
    /// conv_workdir_set）→ platform 的 workspace 泵（store KV 读写 + 校验 +
    /// 回 workspaces 快照；不进 recv / dispatch 命令链）。`conv` 为经
    /// [`normalize_conv`] 归一的完整 conv。
    WorkspaceList,
    WorkspaceSave {
        name: String,
        path: String,
    },
    WorkspaceRemove {
        name: String,
    },
    ConvWorkdirSet {
        conv: String,
        path: String,
    },
    /// v1.8 会话历史重放：history_query{convId, since} → platform 的
    /// history 泵（查 outbound_log + 回 history 帧；不进 recv / dispatch
    /// 命令链）。`conv` 为经 [`normalize_conv`] 归一的完整 conv；`since`
    /// 为浏览器本地高水位 seq（0 = 从头）。
    HistoryQuery {
        conv: String,
        since: i64,
    },
    /// v1.40 语音转写：asr_request{audio, durationMs} → platform 的 ASR 泵
    /// （b64 解码 + 写临时 WAV + spawn whisper-cli + 回 asr_result 帧；不进
    /// recv / dispatch 命令链）。`audio` 为 base64 音频（mime 恒
    /// audio/wav——契约定死不传，WAV 规格由 SPA 保证）；`duration_ms` 为
    /// SPA 上报的录音时长（可选字段，缺省 0；网关仅日志用——转写限制在
    /// 字节数不在时长，60s 截断是 SPA 侧职责）。本层只做字段存在性/类型
    /// 检查；值域校验（b64 合法性、解码 ≤8MB）在泵——失败回对应 error 帧。
    AsrRequest {
        audio: String,
        duration_ms: u64,
    },
    /// v1.41.3 会话删除：conv_delete{convId} → platform 的 workspace 泵（活跃
    /// 轮探测 + sessions/outbound_log/卡片缓存三处删除 + 回执文本 + workspaces
    /// 快照；不进 recv / dispatch 命令链——删除走网关数据面，不经 agent）。
    /// `conv` 为经 [`normalize_conv`] 归一的完整 conv（`peer:`/`local:` 前缀
    /// 原样透传——peer 会话同样可删，见 /rm 的例外语义）。
    ConvDelete {
        conv: String,
    },
    /// 未知 type：debug 忽略——前向兼容，M3 SPA 新增帧不致网关断连。
    Ignore,
    /// 协议违规 / 缺必填字段：warn 原因并丢弃（不进 recv）。
    Warn(&'static str),
}

/// `approval_response.decision` → 审批词表精确词（§5.2）：allow→`y`、
/// deny→`n`、always→`always`（`imagent_core::permission` 的 ALLOW/DENY/
/// ALWAYS 词表均含——ask_req 锚定后 parse_reply 消费，词必须精确命中）。
fn decision_word(decision: &str) -> Option<&'static str> {
    match decision {
        "allow" => Some("y"),
        "deny" => Some("n"),
        "always" => Some("always"),
        _ => None,
    }
}

/// 帧内 convId 归一（协议约定的唯一入口）：**帧内 convId 恒为完整 conv
/// 形态（`web:<session>`）**——出站帧写入的就是完整 conv，浏览器原样回传
/// 即可（SPA 回执 approval_response / message 的正常路径）；裸 session 名
/// （如 `work`）仅作兼容输入，补 `web:` 前缀归一。
/// - raw 为 None / 空串 → 默认会话 `web:<default_session>`
/// - raw 以 `web:` 开头 → 原样返回（不双前缀）
/// - raw 以 `local:` 开头 → **原样透传**：web 出生缺省 `web:` 前缀，而
///   `local:` 是网关侧本地终端会话（crates/localterm）的既有 conv 形态——
///   MultiPlatform 镜像路由把 local 会话的出站帧同步给 web 平台，SPA 可直接
///   寻址 local 会话干预；回执若被强加 `web:` 前缀会造出无人消费的
///   `web:local:xxx` 幽灵会话，干预/审批闭环断裂。
/// - raw 以 `peer:` 开头 → **原样透传**：`peer:` 是网关伪装 Claude Code peer
///   的会话前缀（crates/core/src/peer，网关注册为本机 peer，本地 claude 窗口
///   经原生 SendMessage 与驾驶舱 `peer:<窗口名>` 会话双向互通）；dispatch 有
///   专门的 `peer:` 拦截臂（dispatch_peer_message，消息不进 agent backend）——
///   强加 `web:` 前缀会让用户消息绕过拦截直进 agent（误触发 agent 轮、落
///   outbound_log card），并造出 `web:peer:xxx` 幽灵会话。
/// - 其它（裸 session 名）→ `web:<raw>`
///
/// `pub(crate)`：client 层组 `malformed` 回执时对违规帧的 convId 做同一套
/// 归一（v1.44）。
pub(crate) fn normalize_conv(raw: Option<&str>, default_session: &str) -> String {
    match raw {
        Some(c) if c.starts_with("local:") => c.to_string(),
        Some(c) if c.starts_with("peer:") => c.to_string(),
        Some(c) if !c.is_empty() && c.starts_with("web:") => c.to_string(),
        Some(c) if !c.is_empty() => format!("web:{c}"),
        _ => format!("web:{default_session}"),
    }
}

/// 合成 InboundMessage 公共骨架（§5.2；字段全集对照 src/webhook.rs:650 构造
/// 示例）。web 用户手打消息走 steering（no_steer=false，与 webhook 合成消息
/// 相反）。`conv` 为已经 [`normalize_conv`] 归一的完整 conv 字符串
/// （`web:<session>`），本函数不再加前缀。`client_id` 为 v1.44 送达回执的
/// 关联 id（只有 `message` 帧经 [`parse_client_id`] 解析后可能为 Some；
/// approval_response / control 等合成消息传 None——契约只给 message 帧定义
/// 了 `clientId`）。
pub(crate) fn synth_message(
    conv: String,
    text: String,
    ask_req: Option<String>,
    client_id: Option<String>,
) -> InboundMessage {
    InboundMessage {
        conv_id: ConvId(conv),
        sender: UserId("web:user".to_string()),
        sender_name: None,
        text: Some(text),
        media: Vec::new(),
        media_errors: Vec::new(),
        mentions: Vec::new(),
        mentioned_bot: false,
        ask_req,
        reply_to: None,
        source_msg_id: None,
        control: None,
        no_steer: false,
        reply_hint: ReplyHint::None,
        client_id,
    }
}

/// v1.7 媒体消息合成（platform 媒体泵落盘后调用）：text 非空才 Some
/// （契约：纯图消息 text=None——空串说明与缺省同归一），media 为已落盘的
/// 本地路径引用（[`MediaRef`] 无独立 mime/name 字段：kind 载 mime，
/// name 已并入落盘文件名）。其余字段与 [`synth_message`] 同（用户手打，
/// steering 生效）。`client_id`（v1.44）随媒体动作从 client 层一路带到泵。
pub(crate) fn synth_media_message(
    conv: String,
    text: Option<String>,
    media: Vec<MediaRef>,
    client_id: Option<String>,
) -> InboundMessage {
    let mut msg = synth_message(conv, String::new(), None, client_id);
    msg.text = text.filter(|t| !t.is_empty());
    msg.media = media;
    msg
}

/// `clientId` 长度上限（契约 §2.1：1..=64，字符集 `[A-Za-z0-9_-]`）。SPA 的
/// 生成形态是 `m_` + 时间戳 36 进制 + 8 位随机十六进制，远在上限内；上限只
/// 为挡畸形帧（超长 id 会原样进 ack 帧、日志与排队行）。
pub(crate) const CLIENT_ID_MAX_CHARS: usize = 64;

/// 入站 `clientId` 形状校验（契约 §2.1，纯函数）：字符串、长度 1..=64、仅
/// `[A-Za-z0-9_-]`。**不合规一律当缺席**（返回 None——照旧处理消息，只是
/// 不发回执；不因一个可选字段丢掉用户消息）。字符集限定 ASCII，故按字节
/// 计长即按字符计长。client 层对 `Warn` 帧也用它判断能否回 `malformed`
/// 回执（同一规则，单一事实源）。
pub(crate) fn parse_client_id(v: Option<&serde_json::Value>) -> Option<String> {
    let s = v?.as_str()?;
    if s.is_empty() || s.len() > CLIENT_ID_MAX_CHARS {
        return None;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .then(|| s.to_string())
}

/// 入站业务帧映射（§4.1 表；client 已解析为 JSON Value，machine/error 等
/// relay 自产帧由 client 分拣，不进本函数）。帧内 convId 协议约定为完整
/// conv（`web:<session>`，出站帧写入的形态）——浏览器原样回传即可；裸
/// session 名仅作兼容输入，三分支统一经 [`normalize_conv`] 归一。
pub(crate) fn map_inbound_frame(v: &serde_json::Value, default_session: &str) -> InboundAction {
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        // message：convId 缺省 / 空串 → default_session。v1.7 起可带可选
        // media 段（图片 base64 上行）——本层只查字段存在性/类型，值域校验 +
        // 落盘在 platform 媒体泵；纯图消息 text 可缺省/空串，text 与 media
        // 至少其一。v1.44 起可带可选 `clientId`（送达回执关联 id，形状不合规
        // 当缺席——消息照旧处理，只是不发回执）。
        "message" => {
            let conv = normalize_conv(v.get("convId").and_then(|c| c.as_str()), default_session);
            let text = v.get("text").and_then(|t| t.as_str());
            let client_id = parse_client_id(v.get("clientId"));
            // media:null（JS 可选字段的 null 序列化形态）按「无 media 段」处理。
            let media_field = v.get("media").filter(|m| !m.is_null());
            if media_field.is_some() {
                match parse_media_payload(media_field) {
                    Some(media) => InboundAction::MediaMessage {
                        conv,
                        text: text.map(str::to_string),
                        media,
                        client_id,
                    },
                    None => InboundAction::Warn(
                        "message 帧 media 字段畸形（须 name/mime/b64 字符串），丢弃",
                    ),
                }
            } else {
                match text {
                    // 无 media 时 text 必填（缺失无意义，丢弃——v1.7 前既有纪律）。
                    Some(t) => InboundAction::Message(Box::new(synth_message(
                        conv,
                        t.to_string(),
                        None,
                        client_id,
                    ))),
                    None => InboundAction::Warn("message 帧缺 text 字段，丢弃"),
                }
            }
        }
        // approval_response：convId 必填（审批按 conv 路由，缺省无法投递）；
        // 决策词必须精确命中词表。
        "approval_response" => {
            let Some(conv) = v.get("convId").and_then(|c| c.as_str()) else {
                return InboundAction::Warn(
                    "approval_response 帧缺 convId（审批按 conv 路由），丢弃",
                );
            };
            let Some(request_id) = v.get("requestId").and_then(|r| r.as_str()) else {
                return InboundAction::Warn("approval_response 帧缺 requestId，丢弃");
            };
            let Some(decision) = v.get("decision").and_then(|d| d.as_str()) else {
                return InboundAction::Warn("approval_response 帧缺 decision，丢弃");
            };
            match decision_word(decision) {
                Some(word) => InboundAction::Message(Box::new(synth_message(
                    normalize_conv(Some(conv), default_session),
                    word.to_string(),
                    Some(request_id.to_string()),
                    None,
                ))),
                None => InboundAction::Warn(
                    "approval_response 帧 decision 非法（须 allow|deny|always），丢弃",
                ),
            }
        }
        // control：平台侧转命令文本（与 IM 按钮点击同机制——走既有鉴权/分派，
        // 见 platform.rs send_command_card 文档注释的既定惯例）。v1.42 起可带
        // 可选 convId：合成命令**按会话**生效（/stop 只中断该 conv 的在飞
        // 轮，/resume / /switch 的回执也落该会话），SPA 须指明目标会话——
        // 恒落默认会话时，非默认会话里点停止停不掉当前任务。convId 缺席 /
        // 空串 / 非字符串 → 默认会话（旧 SPA 行为不变）；存在 → 经
        // normalize_conv 归一（web:/local:/peer: 前缀透传，裸名补 web:）。
        // 三种 cmd 统一适用，不区分。
        "control" => {
            let cmd = v.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
            let args = v.get("args").and_then(|a| a.as_str()).unwrap_or("");
            let text = match cmd {
                "stop" => "/stop".to_string(),
                "resume" => "/resume".to_string(),
                "switch" => {
                    if args.is_empty() {
                        "/switch".to_string()
                    } else {
                        format!("/switch {args}")
                    }
                }
                _ => {
                    return InboundAction::Warn(
                        "control 帧 cmd 非法（须 stop|resume|switch），丢弃",
                    )
                }
            };
            InboundAction::Message(Box::new(synth_message(
                normalize_conv(v.get("convId").and_then(|c| c.as_str()), default_session),
                text,
                None,
                None,
            )))
        }
        "sync" => InboundAction::Sync,
        // v1.42 待审批快照：浏览器请求 → 网关回权威快照（参照 workspace_list →
        // workspaces）。无字段校验面（无必填字段），组装与回帧在 platform
        // 的 sync 泵。
        "approvals_query" => InboundAction::ApprovalsQuery,
        // v1.43 本地窗口快照：浏览器请求 → 网关回权威快照（同 approvals_query）。
        // 无字段校验面，组装与回帧在 platform 的 sync 泵。
        "windows_query" => InboundAction::WindowsQuery,
        // v1.44 网关信息查询：无字段校验面，应答帧由 client 层直接出站
        // （`encode_gateway_info_frame`）——任何网关都回，旧网关按未知 type
        // 忽略。
        "gateway_info_query" => InboundAction::GatewayInfoQuery,
        // v1.43 关注 / 取消关注窗口：本层只做字段校验（`name` 须非空字符串且
        // ≤ 上限，`watch` 须布尔；缺失 / 类型错 / 超长 → Warn 丢弃——契约：
        // 畸形 window_watch 一律丢弃，不改关注名单）。名字原样使用、不 trim：
        // 它是 `windows` 帧里 `name` 的原样回传，与注册表条目名逐字相等才匹配。
        "window_watch" => {
            let Some(name) = v.get("name").and_then(|n| n.as_str()) else {
                return InboundAction::Warn("window_watch 帧缺 name 字段（或非字符串），丢弃");
            };
            let Some(watch) = v.get("watch").and_then(|w| w.as_bool()) else {
                return InboundAction::Warn("window_watch 帧缺 watch 字段（或非布尔），丢弃");
            };
            if name.is_empty() || name.chars().count() > WINDOW_NAME_MAX_CHARS {
                return InboundAction::Warn("window_watch 帧 name 为空或超长，丢弃");
            }
            InboundAction::WindowWatch {
                name: name.to_string(),
                watch,
            }
        }
        "ping" => InboundAction::Ping,
        // v1.6 工作空间帧：本层只做字段存在性/类型检查（缺失/类型错 → Warn
        // 丢弃，与 message 同纪律）；**值域校验（空 name / 非法路径）在
        // platform 泵做**——失败须回 error 帧（workspace_invalid）+ 快照给
        // SPA 反馈，静默丢弃会让 UI 无响应。
        "workspace_list" => InboundAction::WorkspaceList,
        "workspace_save" => {
            let Some(name) = v.get("name").and_then(|n| n.as_str()) else {
                return InboundAction::Warn("workspace_save 帧缺 name 字段，丢弃");
            };
            let Some(path) = v.get("path").and_then(|p| p.as_str()) else {
                return InboundAction::Warn("workspace_save 帧缺 path 字段，丢弃");
            };
            InboundAction::WorkspaceSave {
                name: name.to_string(),
                path: path.to_string(),
            }
        }
        "workspace_remove" => {
            let Some(name) = v.get("name").and_then(|n| n.as_str()) else {
                return InboundAction::Warn("workspace_remove 帧缺 name 字段，丢弃");
            };
            InboundAction::WorkspaceRemove {
                name: name.to_string(),
            }
        }
        "conv_workdir_set" => {
            let Some(conv) = v.get("convId").and_then(|c| c.as_str()) else {
                return InboundAction::Warn("conv_workdir_set 帧缺 convId，丢弃");
            };
            let Some(path) = v.get("path").and_then(|p| p.as_str()) else {
                return InboundAction::Warn("conv_workdir_set 帧缺 path 字段，丢弃");
            };
            InboundAction::ConvWorkdirSet {
                conv: normalize_conv(Some(conv), default_session),
                path: path.to_string(),
            }
        }
        // v1.8 会话历史重放：本层只做字段校验（convId 须字符串、since 须非负
        // 整数；since 缺省按 0=从头），值域/查库/回帧在 platform history 泵。
        "history_query" => {
            let Some(conv) = v.get("convId").and_then(|c| c.as_str()) else {
                return InboundAction::Warn("history_query 帧缺 convId 字段，丢弃");
            };
            let since = match v.get("since") {
                None => 0,
                Some(s) => match s.as_i64() {
                    Some(n) if n >= 0 => n,
                    _ => {
                        return InboundAction::Warn(
                            "history_query 帧 since 非法（须非负整数），丢弃",
                        )
                    }
                },
            };
            InboundAction::HistoryQuery {
                conv: normalize_conv(Some(conv), default_session),
                since,
            }
        }
        // v1.40 语音转写：本层只做字段校验（audio 须字符串、durationMs 可选
        // 须非负整数缺省 0），值域校验（b64 合法性、解码 ≤8MB）与 spawn 在
        // platform ASR 泵——失败回 error 帧（asr_invalid / asr_too_large），
        // 静默丢弃会让 SPA 语音弹层无响应。
        "asr_request" => {
            let Some(audio) = v.get("audio").and_then(|a| a.as_str()) else {
                return InboundAction::Warn("asr_request 帧缺 audio 字段（b64），丢弃");
            };
            let duration_ms = match v.get("durationMs") {
                None => 0,
                Some(d) => match d.as_u64() {
                    Some(n) => n,
                    None => {
                        return InboundAction::Warn(
                            "asr_request 帧 durationMs 非法（须非负整数），丢弃",
                        )
                    }
                },
            };
            InboundAction::AsrRequest {
                audio: audio.to_string(),
                duration_ms,
            }
        }
        // v1.41.3 会话删除：本层只做字段校验（convId 须字符串，缺失/类型错 →
        // Warn 丢弃），值域校验（活跃轮拒绝）与三处删除在 platform workspace
        // 泵——失败须回 error 帧（conv_busy）+ 快照给 SPA 反馈，静默丢弃会让
        // 删除确认弹层无响应。
        "conv_delete" => {
            let Some(conv) = v.get("convId").and_then(|c| c.as_str()) else {
                return InboundAction::Warn("conv_delete 帧缺 convId 字段，丢弃");
            };
            InboundAction::ConvDelete {
                conv: normalize_conv(Some(conv), default_session),
            }
        }
        _ => InboundAction::Ignore,
    }
}

/// 测试/集成用：把原始 JSON 文本一步映射（解析失败归 Ignore——畸形帧不断连，
/// 与 client 侧 debug 容错一致）。
#[cfg(test)]
pub(crate) fn map_raw_frame(text: &str, default_session: &str) -> InboundAction {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => map_inbound_frame(&v, default_session),
        Err(_) => InboundAction::Ignore,
    }
}

#[cfg(test)]
mod tests {
    //! 帧协议纯函数单测（任务说明 §8）：入站映射 / 出站药化 / 截断。

    use super::*;
    use imagent_core::{is_explicit_reply_word, parse_reply, TodoItem, ToolCall};

    const SESSION: &str = "default";

    #[test]
    fn map_message_with_conv_id() {
        let act = map_raw_frame(
            r#"{"v":1,"type":"message","convId":"work","text":"列出目录"}"#,
            SESSION,
        );
        let InboundAction::Message(m) = act else {
            panic!("message 应映射为 Message：{act:?}")
        };
        assert_eq!(m.conv_id, ConvId("web:work".into()));
        assert_eq!(m.sender, UserId("web:user".into()));
        assert_eq!(m.text.as_deref(), Some("列出目录"));
        assert_eq!(m.ask_req, None);
        assert!(!m.no_steer, "用户手打消息走 steering");
        assert!(m.media.is_empty() && m.mentions.is_empty() && !m.mentioned_bot);
        assert!(matches!(m.reply_hint, ReplyHint::None));
        // 完整 conv 形态（SPA 原样回传出站帧 convId）→ 原样，不双前缀。
        let act = map_raw_frame(
            r#"{"v":1,"type":"message","convId":"web:work","text":"hi"}"#,
            SESSION,
        );
        let InboundAction::Message(m) = act else {
            panic!("完整 convId 的 message 应映射为 Message：{act:?}")
        };
        assert_eq!(m.conv_id, ConvId("web:work".into()));
    }

    /// convId 归一五分支：None / 空串 → 默认会话；完整 web conv 原样（SPA
    /// 原样回传出站帧 convId 的正常路径）；裸 session 名补前缀（兼容形态）；
    /// `local:` 前缀原样透传（本地终端会话）；`peer:` 前缀原样透传（网关伪装
    /// Claude Code peer 会话，均见 normalize_conv 注释）。
    #[test]
    fn normalize_conv_three_forms() {
        assert_eq!(normalize_conv(None, "default"), "web:default");
        assert_eq!(normalize_conv(Some(""), "default"), "web:default");
        assert_eq!(normalize_conv(Some("web:work"), "default"), "web:work");
        assert_eq!(normalize_conv(Some("work"), "default"), "web:work");
        // local: 透传（不双前缀成 web:local:xxx 幽灵会话）。
        assert_eq!(normalize_conv(Some("local:work"), "default"), "local:work");
        // 恰好名为 local 的裸 session 名仍走 web: 前缀（无冒号非 local: 前缀）。
        assert_eq!(normalize_conv(Some("local"), "default"), "web:local");
        // peer: 透传（dispatch 有 peer: 拦截臂不进 agent；不双前缀成
        // web:peer:xxx 幽灵会话）。
        assert_eq!(normalize_conv(Some("peer:win"), "default"), "peer:win");
        // 恰好名为 peer 的裸 session 名仍走 web: 前缀（无冒号非 peer: 前缀）。
        assert_eq!(normalize_conv(Some("peer"), "default"), "web:peer");
    }

    /// local 会话干预闭环（镜像路由配套，必修回归钉）：MultiPlatform 把 local
    /// 会话出站镜像给 web 平台 → SPA 看到 `local:<名>` conv；用户对它的干预
    /// message 与审批 approval_response 回执必须原样回到 local conv——被强加
    /// web: 前缀则路由进 web 幽灵会话，干预与审批全部失效。
    #[test]
    fn map_frames_pass_through_local_conv() {
        // message：SPA 发往 local:work 的干预消息。
        let act = map_raw_frame(
            r#"{"v":1,"type":"message","convId":"local:work","text":"继续"}"#,
            SESSION,
        );
        let InboundAction::Message(m) = act else {
            panic!("local conv 的 message 应映射为 Message：{act:?}")
        };
        assert_eq!(m.conv_id, ConvId("local:work".into()));
        assert_eq!(m.text.as_deref(), Some("继续"));

        // approval_response：镜像 approval_request 的回执（SPA 原样回传 convId）。
        let act = map_raw_frame(
            r#"{"v":1,"type":"approval_response","convId":"local:work","requestId":"req-1","decision":"allow"}"#,
            SESSION,
        );
        let InboundAction::Message(m) = act else {
            panic!("local conv 的 approval_response 应映射为 Message：{act:?}")
        };
        assert_eq!(m.conv_id, ConvId("local:work".into()));
        assert_eq!(m.ask_req.as_deref(), Some("req-1"));
        assert_eq!(m.text.as_deref(), Some("y"));
    }

    #[test]
    fn map_message_without_conv_id_falls_to_default_session() {
        let act = map_raw_frame(r#"{"v":1,"type":"message","text":"hi"}"#, SESSION);
        let InboundAction::Message(m) = act else {
            panic!("应映射为 Message：{act:?}")
        };
        assert_eq!(m.conv_id, ConvId("web:default".into()));
        // 空 convId 同缺省。
        let act = map_raw_frame(
            r#"{"v":1,"type":"message","convId":"","text":"hi"}"#,
            SESSION,
        );
        assert!(matches!(act, InboundAction::Message(_)));
        // 缺 text → 丢弃。
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"message"}"#, SESSION),
            InboundAction::Warn(_)
        ));
    }

    /// 三决策词必须精确命中 permission.rs 词表（ask_req 锚定后 parse_reply
    /// 消费——词表漂移会让审批闭环静默失效，此处钉住）。
    #[test]
    fn map_approval_response_words_hit_vocabulary() {
        for (decision, word) in [("allow", "y"), ("deny", "n"), ("always", "always")] {
            let act = map_raw_frame(
                &format!(
                    r#"{{"v":1,"type":"approval_response","convId":"c1","requestId":"req-1","decision":"{decision}"}}"#
                ),
                SESSION,
            );
            let InboundAction::Message(m) = act else {
                panic!("{decision} 应映射为 Message")
            };
            assert_eq!(m.conv_id, ConvId("web:c1".into()));
            assert_eq!(m.ask_req.as_deref(), Some("req-1"));
            let text = m.text.unwrap();
            assert_eq!(text, word);
            assert!(
                is_explicit_reply_word(&text),
                "决策词 {word} 必须命中审批词表"
            );
            let reply = parse_reply(&text);
            match decision {
                "allow" => assert!(reply.allow && !reply.always),
                "deny" => assert!(!reply.allow),
                "always" => assert!(reply.allow && reply.always),
                _ => unreachable!(),
            }
        }
        // convId 必填：缺省丢弃。
        assert!(matches!(
            map_raw_frame(
                r#"{"v":1,"type":"approval_response","requestId":"r","decision":"allow"}"#,
                SESSION,
            ),
            InboundAction::Warn(_)
        ));
        // 非法 decision 丢弃。
        assert!(matches!(
            map_raw_frame(
                r#"{"v":1,"type":"approval_response","convId":"c","requestId":"r","decision":"maybe"}"#,
                SESSION,
            ),
            InboundAction::Warn(_)
        ));
    }

    /// 审批闭环协议对称性（必修回归钉）：浏览器**原样回传**出站
    /// approval_request 帧的 convId（完整 conv `web:work`，SPA 正常路径）与
    /// 裸 session 名（兼容形态）都必须归一到同一 conv `web:work`——否则
    /// PermissionRouter 按 conv 路由找不到 pending，审批闭环断裂。
    #[test]
    fn map_approval_response_accepts_full_and_bare_conv() {
        for conv_id in ["web:work", "work"] {
            let act = map_raw_frame(
                &format!(
                    r#"{{"v":1,"type":"approval_response","convId":"{conv_id}","requestId":"req-1","decision":"allow"}}"#
                ),
                SESSION,
            );
            let InboundAction::Message(m) = act else {
                panic!("convId={conv_id} 应映射为 Message：{act:?}")
            };
            assert_eq!(m.conv_id, ConvId("web:work".into()), "convId={conv_id}");
            assert_eq!(m.ask_req.as_deref(), Some("req-1"));
            assert_eq!(m.text.as_deref(), Some("y"));
        }
    }

    #[test]
    fn map_control_synthesizes_command_text() {
        let cases = [
            (r#"{"v":1,"type":"control","cmd":"stop"}"#, "/stop"),
            (r#"{"v":1,"type":"control","cmd":"resume"}"#, "/resume"),
            (
                r#"{"v":1,"type":"control","cmd":"switch","args":"web:proj-a"}"#,
                "/switch web:proj-a",
            ),
        ];
        for (raw, want) in cases {
            let act = map_raw_frame(raw, SESSION);
            let InboundAction::Message(m) = act else {
                panic!("{raw} 应映射为 Message")
            };
            assert_eq!(m.text.as_deref(), Some(want));
            assert_eq!(m.conv_id, ConvId("web:default".into()));
        }
        // 非法 cmd 丢弃。
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"control","cmd":"boom"}"#, SESSION),
            InboundAction::Warn(_)
        ));
    }

    /// v1.42 control 帧可选 convId（必修回归钉）：合成命令落指定会话——
    /// `/stop` 按会话中断，恒落默认会话时非默认会话里点停止停不掉当前任务。
    /// 六种形态：完整 web conv / 缺席 / 空串 / 裸名补前缀 / `local:` `peer:`
    /// 透传 / 非字符串按缺席处理（不 panic）。
    #[test]
    fn map_control_conv_id_routes_command_to_session() {
        let mapped = |raw: &str| match map_raw_frame(raw, SESSION) {
            InboundAction::Message(m) => (m.conv_id, m.text.unwrap()),
            other => panic!("{raw} 应映射为 Message：{other:?}"),
        };

        // 完整 conv（SPA 正常路径：原样回传当前会话的 convId）。
        let (conv, text) = mapped(
            r#"{"v":1,"type":"control","cmd":"stop","machine":"home-mac","convId":"web:s-ab12"}"#,
        );
        assert_eq!(conv, ConvId("web:s-ab12".into()));
        assert_eq!(text, "/stop");

        // 缺席 / 空串 / null / 非字符串（数字、数组）→ 默认会话（旧行为），
        // 命令文本不受影响，非字符串不 panic。
        for raw in [
            r#"{"v":1,"type":"control","cmd":"stop"}"#,
            r#"{"v":1,"type":"control","cmd":"stop","convId":""}"#,
            r#"{"v":1,"type":"control","cmd":"stop","convId":null}"#,
            r#"{"v":1,"type":"control","cmd":"stop","convId":42}"#,
            r#"{"v":1,"type":"control","cmd":"stop","convId":["web:s-ab12"]}"#,
        ] {
            let (conv, text) = mapped(raw);
            assert_eq!(conv, ConvId("web:default".into()), "{raw}");
            assert_eq!(text, "/stop", "{raw}");
        }

        // 裸名补 web: 前缀；local: / peer: 前缀原样透传（不双前缀）。
        let (conv, _) = mapped(r#"{"v":1,"type":"control","cmd":"stop","convId":"s-ab12"}"#);
        assert_eq!(conv, ConvId("web:s-ab12".into()));
        let (conv, _) = mapped(r#"{"v":1,"type":"control","cmd":"stop","convId":"peer:foo"}"#);
        assert_eq!(conv, ConvId("peer:foo".into()));
        let (conv, _) = mapped(r#"{"v":1,"type":"control","cmd":"stop","convId":"local:bar"}"#);
        assert_eq!(conv, ConvId("local:bar".into()));

        // resume / switch 同样落到该会话（三种 cmd 统一适用）。
        let (conv, text) =
            mapped(r#"{"v":1,"type":"control","cmd":"resume","convId":"web:s-ab12"}"#);
        assert_eq!(conv, ConvId("web:s-ab12".into()));
        assert_eq!(text, "/resume");
        let (conv, text) = mapped(
            r#"{"v":1,"type":"control","cmd":"switch","args":"proj-b","convId":"web:s-ab12"}"#,
        );
        assert_eq!(conv, ConvId("web:s-ab12".into()));
        assert_eq!(text, "/switch proj-b");
    }

    #[test]
    fn map_sync_ping_and_unknown() {
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"sync"}"#, SESSION),
            InboundAction::Sync
        ));
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"ping"}"#, SESSION),
            InboundAction::Ping
        ));
        // 未知 type 前向兼容忽略；畸形 JSON 同样忽略（不断连）。
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"future-thing"}"#, SESSION),
            InboundAction::Ignore
        ));
        assert!(matches!(
            map_raw_frame("not json", SESSION),
            InboundAction::Ignore
        ));
    }

    /// v1.42 approvals_query 入站映射：无必填字段（`machine` 由 relay 路由用，
    /// 网关不消费）→ [`InboundAction::ApprovalsQuery`]，不进 recv / dispatch
    /// 命令链。
    #[test]
    fn map_approvals_query() {
        for raw in [
            r#"{"v":1,"type":"approvals_query","machine":"home-mac"}"#,
            r#"{"v":1,"type":"approvals_query"}"#,
        ] {
            assert!(
                matches!(map_raw_frame(raw, SESSION), InboundAction::ApprovalsQuery),
                "{raw}"
            );
        }
    }

    /// v1.43 windows_query 入站映射：无必填字段（`machine` 由 relay 路由用）
    /// → [`InboundAction::WindowsQuery`]，不进 recv / dispatch 命令链。
    #[test]
    fn map_windows_query() {
        for raw in [
            r#"{"v":1,"type":"windows_query","machine":"home-mac"}"#,
            r#"{"v":1,"type":"windows_query"}"#,
        ] {
            assert!(
                matches!(map_raw_frame(raw, SESSION), InboundAction::WindowsQuery),
                "{raw}"
            );
        }
    }

    /// v1.43 window_watch 入站映射（契约 §1 形状规则）：`name` 须为非空字符串
    /// ≤ 256 字符（原样使用、不 trim），`watch` 须为布尔；缺失 / 类型错 / 空 /
    /// 超长 → Warn 丢弃（不改关注名单）。
    #[test]
    fn map_window_watch_shapes() {
        let watch_of = |raw: &str| match map_raw_frame(raw, SESSION) {
            InboundAction::WindowWatch { name, watch } => Some((name, watch)),
            InboundAction::Warn(reason) => panic!("合法帧被拒：{raw}（{reason}）"),
            other => panic!("{raw} 应映射为 WindowWatch：{other:?}"),
        };
        assert_eq!(
            watch_of(
                r#"{"v":1,"type":"window_watch","machine":"home-mac","name":"jormungandr-f8","watch":true}"#
            ),
            Some(("jormungandr-f8".to_string(), true))
        );
        assert_eq!(
            watch_of(r#"{"v":1,"type":"window_watch","name":"win-a","watch":false}"#),
            Some(("win-a".to_string(), false))
        );
        // 恰到上限的名字放行（256 字符）；名字原样（不 trim）。
        let at_limit = "w".repeat(WINDOW_NAME_MAX_CHARS);
        assert_eq!(
            watch_of(&format!(
                r#"{{"v":1,"type":"window_watch","name":"{at_limit}","watch":true}}"#
            ))
            .map(|(n, _)| n.len()),
            Some(WINDOW_NAME_MAX_CHARS)
        );
        assert_eq!(
            watch_of(r#"{"v":1,"type":"window_watch","name":" win-a ","watch":true}"#),
            Some((" win-a ".to_string(), true)),
            "名字原样使用"
        );
        // 反例。
        let too_long = "w".repeat(WINDOW_NAME_MAX_CHARS + 1);
        for raw in [
            r#"{"v":1,"type":"window_watch","watch":true}"#.to_string(),
            r#"{"v":1,"type":"window_watch","name":"win-a"}"#.to_string(),
            r#"{"v":1,"type":"window_watch","name":"","watch":true}"#.to_string(),
            r#"{"v":1,"type":"window_watch","name":42,"watch":true}"#.to_string(),
            r#"{"v":1,"type":"window_watch","name":"win-a","watch":"true"}"#.to_string(),
            r#"{"v":1,"type":"window_watch","name":"win-a","watch":1}"#.to_string(),
            r#"{"v":1,"type":"window_watch","name":"win-a","watch":null}"#.to_string(),
            format!(r#"{{"v":1,"type":"window_watch","name":"{too_long}","watch":true}}"#),
        ] {
            assert!(
                matches!(map_raw_frame(&raw, SESSION), InboundAction::Warn(_)),
                "畸形 window_watch 应 Warn 丢弃：{raw}"
            );
        }
    }

    /// v1.43 windows 快照帧字节级（共享契约 §2）：顶层键序 `v / type / machine /
    /// hookInstalled / items`；条目键序 `name / status / waitingFor / cwd /
    /// startedAt / statusUpdatedAt / watched`，三个可选字段为 None 时**省略键**；
    /// `items` 空时输出 `[]` 不省略键；`status` 四值字面量。
    #[test]
    fn windows_frame_byte_level() {
        // 空清单（peer 通道已启用但没有在线窗口）：键不省略。
        assert_eq!(
            encode_windows_frame("home-mac", true, &[]).unwrap(),
            r#"{"v":1,"type":"windows","machine":"home-mac","hookInstalled":true,"items":[]}"#
        );
        assert_eq!(
            encode_windows_frame("home-mac", false, &[]).unwrap(),
            r#"{"v":1,"type":"windows","machine":"home-mac","hookInstalled":false,"items":[]}"#
        );
        let win = |name: &str, status: WindowStatus| WindowInfo {
            name: name.into(),
            status,
            waiting_for: None,
            cwd: "/Users/demo/work/api".into(),
            started_at: Some(1_795_114_000_000),
            status_updated_at: Some(1_795_351_568_790),
            watched: false,
        };
        // 契约 §2 的示例帧逐字节（两个条目：无 waitingFor / 有 waitingFor）。
        let mut idle = win("jormungandr-f8", WindowStatus::Idle);
        idle.cwd = "/Users/demo/work/api".into();
        let mut waiting = win("imagent-a7", WindowStatus::Waiting);
        waiting.waiting_for = Some("dialog open".into());
        waiting.cwd = "/Users/demo/work/b".into();
        waiting.started_at = Some(1_795_115_000_000);
        waiting.status_updated_at = Some(1_795_352_000_000);
        waiting.watched = true;
        assert_eq!(
            encode_windows_frame("home-mac", true, &[idle, waiting]).unwrap(),
            r#"{"v":1,"type":"windows","machine":"home-mac","hookInstalled":true,"items":[{"name":"jormungandr-f8","status":"idle","cwd":"/Users/demo/work/api","startedAt":1795114000000,"statusUpdatedAt":1795351568790,"watched":false},{"name":"imagent-a7","status":"waiting","waitingFor":"dialog open","cwd":"/Users/demo/work/b","startedAt":1795115000000,"statusUpdatedAt":1795352000000,"watched":true}]}"#
        );
        // 可选字段为 None → 省略键；status 其余取值字面量。
        let bare = WindowInfo {
            name: "w".into(),
            status: WindowStatus::Busy,
            waiting_for: None,
            cwd: "/c".into(),
            started_at: None,
            status_updated_at: None,
            watched: false,
        };
        assert_eq!(
            encode_windows_frame("m", false, &[bare]).unwrap(),
            r#"{"v":1,"type":"windows","machine":"m","hookInstalled":false,"items":[{"name":"w","status":"busy","cwd":"/c","watched":false}]}"#
        );
        for (status, literal) in [
            (WindowStatus::Busy, "busy"),
            (WindowStatus::Idle, "idle"),
            (WindowStatus::Waiting, "waiting"),
            (WindowStatus::Unknown, "unknown"),
        ] {
            let json = encode_windows_frame("m", false, &[win("w", status)]).unwrap();
            assert!(
                json.contains(&format!(r#""status":"{literal}""#)),
                "{literal}：{json}"
            );
        }
    }

    /// v1.42 approvals 快照帧字节级（共享契约）：键名驼峰、声明序（v / type /
    /// machine / items；条目 convId / requestId / tool / inputSummary /
    /// timeoutSecs）、`items` 空时输出 `[]` 不省略键。
    #[test]
    fn approvals_frame_byte_level() {
        // 空表：items 为空数组，键不省略。
        assert_eq!(
            encode_approvals_frame("home-mac", &[]).unwrap(),
            r#"{"v":1,"type":"approvals","machine":"home-mac","items":[]}"#
        );

        // 非空：两条（跨会话 / 跨平台前缀），逐字节。
        let item =
            |conv: &str, req: &str, tool: &str, summary: &str, secs: u64| ApprovalSnapshotItem {
                conv_id: conv.into(),
                request_id: req.into(),
                tool_name: tool.into(),
                summary: summary.into(),
                timeout_secs: secs,
            };
        let items = vec![
            item("web:s-192d", "req-1", "Bash", "npm test", 731),
            item("local:work", "req-2", "Write", "/tmp/a", 12),
        ];
        assert_eq!(
            encode_approvals_frame("home-mac", &items).unwrap(),
            r#"{"v":1,"type":"approvals","machine":"home-mac","items":[{"convId":"web:s-192d","requestId":"req-1","tool":"Bash","inputSummary":"npm test","timeoutSecs":731},{"convId":"local:work","requestId":"req-2","tool":"Write","inputSummary":"/tmp/a","timeoutSecs":12}]}"#
        );
    }

    /// v1.6 工作空间入站帧：四帧合法形态解析 + conv_workdir_set 的 convId
    /// 三形态归一（裸名补前缀 / 完整 conv 原样 / local: 透传——与 message
    /// 帧同纪律，SPA 侧 local 会话目录同样可设）。
    #[test]
    fn map_workspace_frames_valid() {
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"workspace_list"}"#, SESSION),
            InboundAction::WorkspaceList
        ));
        let act = map_raw_frame(
            r#"{"v":1,"type":"workspace_save","name":"proj","path":"/Users/x/proj"}"#,
            SESSION,
        );
        let InboundAction::WorkspaceSave { name, path } = act else {
            panic!("workspace_save 应映射为 WorkspaceSave：{act:?}")
        };
        assert_eq!(name, "proj");
        assert_eq!(path, "/Users/x/proj");
        let act = map_raw_frame(
            r#"{"v":1,"type":"workspace_remove","name":"proj"}"#,
            SESSION,
        );
        let InboundAction::WorkspaceRemove { name } = act else {
            panic!("workspace_remove 应映射为 WorkspaceRemove：{act:?}")
        };
        assert_eq!(name, "proj");
        for (raw, want) in [
            (
                r#"{"v":1,"type":"conv_workdir_set","convId":"work","path":"/w"}"#,
                "web:work",
            ),
            (
                r#"{"v":1,"type":"conv_workdir_set","convId":"web:work","path":"/w"}"#,
                "web:work",
            ),
            (
                r#"{"v":1,"type":"conv_workdir_set","convId":"local:work","path":"/w"}"#,
                "local:work",
            ),
        ] {
            let act = map_raw_frame(raw, SESSION);
            let InboundAction::ConvWorkdirSet { conv, path } = act else {
                panic!("{raw} 应映射为 ConvWorkdirSet：{act:?}")
            };
            assert_eq!(conv, want, "convId 归一（{raw}）");
            assert_eq!(path, "/w");
        }
    }

    /// v1.6 工作空间入站帧反例：字段缺失 / 类型错 → Warn 丢弃；**空 name
    /// 仍映射为动作**（值域校验在 platform 泵——失败要回 error 帧 + 快照，
    /// 静默丢弃会让 SPA 无响应）。
    #[test]
    fn map_workspace_frames_missing_or_wrong_fields() {
        // workspace_save：缺 name / 缺 path / name 非字符串。
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"workspace_save","path":"/w"}"#, SESSION),
            InboundAction::Warn(_)
        ));
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"workspace_save","name":"a"}"#, SESSION),
            InboundAction::Warn(_)
        ));
        assert!(matches!(
            map_raw_frame(
                r#"{"v":1,"type":"workspace_save","name":123,"path":"/w"}"#,
                SESSION
            ),
            InboundAction::Warn(_)
        ));
        // workspace_remove：缺 name / 类型错。
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"workspace_remove"}"#, SESSION),
            InboundAction::Warn(_)
        ));
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"workspace_remove","name":[]}"#, SESSION),
            InboundAction::Warn(_)
        ));
        // conv_workdir_set：缺 convId / 缺 path / convId 类型错。
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"conv_workdir_set","path":"/w"}"#, SESSION),
            InboundAction::Warn(_)
        ));
        assert!(matches!(
            map_raw_frame(
                r#"{"v":1,"type":"conv_workdir_set","convId":"work"}"#,
                SESSION
            ),
            InboundAction::Warn(_)
        ));
        assert!(matches!(
            map_raw_frame(
                r#"{"v":1,"type":"conv_workdir_set","convId":9,"path":"/w"}"#,
                SESSION
            ),
            InboundAction::Warn(_)
        ));
        // 空 name / 空白 name：解析层放行（值域校验在泵，回 error 帧）。
        let act = map_raw_frame(
            r#"{"v":1,"type":"workspace_save","name":"  ","path":"/w"}"#,
            SESSION,
        );
        assert!(
            matches!(act, InboundAction::WorkspaceSave { .. }),
            "空白 name 仍应映射（泵回 error 帧）：{act:?}"
        );
    }

    /// v1.8 history_query 入站帧正例：完整形态（machine 路由字段网关不消费）、
    /// convId 归一（裸 session 名补前缀 / local: 透传）、since 缺省 0。
    #[test]
    fn map_history_query_frames() {
        let act = map_raw_frame(
            r#"{"v":1,"type":"history_query","convId":"web:work","since":7,"machine":"m"}"#,
            SESSION,
        );
        let InboundAction::HistoryQuery { conv, since } = act else {
            panic!("history_query 应映射为 HistoryQuery：{act:?}")
        };
        assert_eq!(conv, "web:work");
        assert_eq!(since, 7);
        for (raw, want) in [
            (
                r#"{"v":1,"type":"history_query","convId":"work","since":0}"#,
                "web:work",
            ),
            (
                r#"{"v":1,"type":"history_query","convId":"local:x","since":0}"#,
                "local:x",
            ),
            // convId 空串 → 归一到默认会话（normalize_conv 既有三分支纪律；
            // 完全缺失则 Warn——见反例测试，与 conv_workdir_set 同款）。
            (
                r#"{"v":1,"type":"history_query","convId":"","since":0}"#,
                "web:default",
            ),
        ] {
            let act = map_raw_frame(raw, SESSION);
            let InboundAction::HistoryQuery { conv, since } = act else {
                panic!("{raw} 应映射为 HistoryQuery：{act:?}")
            };
            assert_eq!(conv, want, "convId 归一（{raw}）");
            assert_eq!(since, 0);
        }
        // since 缺省 = 0（从头）。
        let act = map_raw_frame(
            r#"{"v":1,"type":"history_query","convId":"web:work"}"#,
            SESSION,
        );
        let InboundAction::HistoryQuery { since, .. } = act else {
            panic!("缺省 since 应映射为 HistoryQuery：{act:?}")
        };
        assert_eq!(since, 0);
    }

    /// v1.8 history_query 入站帧反例：convId 缺失/非字符串、since 负数/非整数
    ///（字符串、数组、浮点）→ Warn 丢弃。
    #[test]
    fn map_history_query_frames_missing_or_wrong_fields() {
        for raw in [
            r#"{"v":1,"type":"history_query","since":0}"#,
            r#"{"v":1,"type":"history_query","convId":9,"since":0}"#,
            r#"{"v":1,"type":"history_query","convId":"web:w","since":-1}"#,
            r#"{"v":1,"type":"history_query","convId":"web:w","since":"5"}"#,
            r#"{"v":1,"type":"history_query","convId":"web:w","since":[1]}"#,
            r#"{"v":1,"type":"history_query","convId":"web:w","since":1.5}"#,
        ] {
            assert!(
                matches!(map_raw_frame(raw, SESSION), InboundAction::Warn(_)),
                "{raw} 应 Warn 丢弃"
            );
        }
    }

    /// v1.41.3 conv_delete 入站帧正例：convId 完整形态原样；裸名补 web: 前缀；
    /// peer: / local: 前缀透传（peer 会话同样可删——例外语义见 /rm 注释）。
    #[test]
    fn map_conv_delete_frames() {
        for (raw, want) in [
            (
                r#"{"v":1,"type":"conv_delete","convId":"web:s-aa94","machine":"m"}"#,
                "web:s-aa94",
            ),
            // 裸名兼容输入（SPA 帧契约恒完整形态，此处同 history_query 纪律）。
            (
                r#"{"v":1,"type":"conv_delete","convId":"s-aa94"}"#,
                "web:s-aa94",
            ),
            (
                r#"{"v":1,"type":"conv_delete","convId":"peer:imagent-bc"}"#,
                "peer:imagent-bc",
            ),
            (
                r#"{"v":1,"type":"conv_delete","convId":"local:work"}"#,
                "local:work",
            ),
        ] {
            let act = map_raw_frame(raw, SESSION);
            let InboundAction::ConvDelete { conv } = act else {
                panic!("{raw} 应映射为 ConvDelete：{act:?}")
            };
            assert_eq!(conv, want, "convId 归一（{raw}）");
        }
    }

    /// v1.41.3 conv_delete 入站帧反例：convId 缺失/非字符串 → Warn 丢弃。
    #[test]
    fn map_conv_delete_frames_missing_or_wrong_fields() {
        for raw in [
            r#"{"v":1,"type":"conv_delete"}"#,
            r#"{"v":1,"type":"conv_delete","convId":42}"#,
            r#"{"v":1,"type":"conv_delete","convId":null}"#,
        ] {
            assert!(
                matches!(map_raw_frame(raw, SESSION), InboundAction::Warn(_)),
                "{raw} 应 Warn 丢弃"
            );
        }
    }

    /// v1.40 asr_request 入站帧正例：完整形态（machine 路由字段网关不消费、
    /// mime 契约定死不传）、durationMs 可选（缺省 0）、audio 原样透传
    ///（b64 合法性/大小校验留泵——解析层不做值域检查）。
    #[test]
    fn map_asr_request_frames() {
        let act = map_raw_frame(
            r#"{"v":1,"type":"asr_request","audio":"aGVsbG8=","durationMs":3000,"machine":"m"}"#,
            SESSION,
        );
        let InboundAction::AsrRequest { audio, duration_ms } = act else {
            panic!("asr_request 应映射为 AsrRequest：{act:?}")
        };
        assert_eq!(audio, "aGVsbG8=");
        assert_eq!(duration_ms, 3000);
        // durationMs 缺省 0。
        let act = map_raw_frame(
            r#"{"v":1,"type":"asr_request","audio":"aGVsbG8="}"#,
            SESSION,
        );
        let InboundAction::AsrRequest { duration_ms, .. } = act else {
            panic!("缺 durationMs 应映射为 AsrRequest（缺省 0）")
        };
        assert_eq!(duration_ms, 0);
        // 值域校验留泵：坏 b64 字符集同样放行到泵（泵解码失败回 asr_invalid）。
        assert!(matches!(
            map_raw_frame(
                r#"{"v":1,"type":"asr_request","audio":"not*b64!","durationMs":0}"#,
                SESSION
            ),
            InboundAction::AsrRequest { .. }
        ));
    }

    /// v1.40 asr_request 入站帧反例：audio 缺失/非字符串、durationMs 负数/
    /// 浮点/字符串 → Warn 丢弃。
    #[test]
    fn map_asr_request_frames_missing_or_wrong_fields() {
        for raw in [
            r#"{"v":1,"type":"asr_request","durationMs":0}"#,
            r#"{"v":1,"type":"asr_request","audio":123,"durationMs":0}"#,
            r#"{"v":1,"type":"asr_request","audio":"aGk=","durationMs":-1}"#,
            r#"{"v":1,"type":"asr_request","audio":"aGk=","durationMs":1.5}"#,
            r#"{"v":1,"type":"asr_request","audio":"aGk=","durationMs":"3000"}"#,
        ] {
            assert!(
                matches!(map_raw_frame(raw, SESSION), InboundAction::Warn(_)),
                "{raw} 应 Warn 丢弃"
            );
        }
    }

    /// v1.8 history 条目组装（行 → 契约 JSON）+ history 帧**键序无关逐字段
    /// 断言**：顶层字段名/值（struct 声明序组帧，逐字段钉住）、条目字段名
    /// 逐字与键数（防多/漏字段）、key 前缀 `h:<seq>`、card 的 state 恒
    /// "done"；空集/新会话形态。
    ///
    /// 不做完整字节串断言：workspace 统一构建/发布形态下 serde_json 的
    /// `preserve_order` feature 经 agent-client-protocol 依赖链激活，Value
    /// 对象键序由默认字母序变插入序（entries 为动态 Value，两种构建形态
    /// 字节串不同），字节串断言依赖构建配置故不可用；键序对协议无语义
    ///（JSON 对象序，SPA 按字段名解析）。
    #[test]
    fn history_entry_and_frame_byte_level() {
        use imagent_store::OutboundRow;

        let row = |seq: i64, kind: &str, payload: &str| OutboundRow {
            conv_id: "web:work".into(),
            seq,
            ts: 1_000 + seq,
            kind: kind.into(),
            payload: payload.into(),
        };
        let text_row = row(1, "text", r#"{"text":"你好","at":1234}"#);
        let card_row = row(
            2,
            "card",
            r#"{"messageId":"web-card-1","card":{"text":"结论","terminal":"done"}}"#,
        );
        let t = history_entry_from_row(&text_row).expect("text 行应组装条目");
        let c = history_entry_from_row(&card_row).expect("card 行应组装条目");
        assert_eq!(t["kind"], "text");
        assert_eq!(t["key"], "h:1", "key 前缀 h: 用行 seq");
        assert_eq!(t["text"], "你好");
        assert_eq!(t["at"], 1234);
        assert_eq!(c["kind"], "card");
        assert_eq!(c["key"], "h:2");
        assert_eq!(c["messageId"], "web-card-1");
        assert_eq!(c["card"]["terminal"], "done", "card 原样内嵌");
        assert_eq!(c["state"], "done", "card 条目 state 恒 done");
        // 条目字段全集（防多字段/漏字段——恰好 4 键）。
        assert_eq!(t.as_object().map(|m| m.len()), Some(4));
        assert_eq!(c.as_object().map(|m| m.len()), Some(5));

        // 畸形 payload / kind 未知 / 必填字段缺失 → None（调用方跳过）。
        assert!(history_entry_from_row(&row(3, "text", "not json")).is_none());
        assert!(history_entry_from_row(&row(4, "mystery", "{}")).is_none());
        assert!(history_entry_from_row(&row(5, "text", r#"{"text":"x"}"#)).is_none());
        assert!(
            history_entry_from_row(&row(6, "card", r#"{"messageId":"m"}"#)).is_none(),
            "card 行缺 card 字段应跳过"
        );

        // 空集形态（新会话/未落盘）：entries=[] + lastSeq=0 + trimmed=false
        //（键序无关：解析回 Value 逐字段断言，理由见本测试 doc 注释）。
        let json =
            encode_history_frame("home-mac", "web:work", serde_json::json!([]), 0, false).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().map(|m| m.len()), Some(7), "顶层恰好 7 键");
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "history");
        assert_eq!(v["machine"], "home-mac", "出站帧带本机 machine");
        assert_eq!(v["convId"], "web:work", "convId 同请求原样");
        assert_eq!(v["entries"].as_array().map(Vec::len), Some(0));
        assert_eq!(v["lastSeq"], 0);
        assert_eq!(v["trimmed"], false);

        // 非空 + trimmed=true：键序无关逐字段复核（顶层 7 键 + 条目字段全集
        // 与键数，防组帧丢改字段）。
        let json = encode_history_frame(
            "home-mac",
            "web:work",
            serde_json::Value::Array(vec![t, c]),
            9,
            true,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().map(|m| m.len()), Some(7), "顶层恰好 7 键");
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "history");
        assert_eq!(v["machine"], "home-mac", "出站帧带本机 machine");
        assert_eq!(v["convId"], "web:work", "convId 同请求原样");
        assert_eq!(v["lastSeq"], 9);
        assert_eq!(v["trimmed"], true);
        let entries = v["entries"].as_array().expect("entries 为数组");
        assert_eq!(entries.len(), 2);
        let e0 = &entries[0];
        assert_eq!(
            e0.as_object().map(|m| m.len()),
            Some(4),
            "text 条目恰好 4 键"
        );
        assert_eq!(e0["kind"], "text");
        assert_eq!(e0["key"], "h:1", "key 前缀 h: 用行 seq");
        assert_eq!(e0["text"], "你好");
        assert_eq!(e0["at"], 1234);
        let e1 = &entries[1];
        assert_eq!(
            e1.as_object().map(|m| m.len()),
            Some(5),
            "card 条目恰好 5 键"
        );
        assert_eq!(e1["kind"], "card");
        assert_eq!(e1["key"], "h:2");
        assert_eq!(e1["messageId"], "web-card-1");
        assert_eq!(e1["card"]["text"], "结论", "card 原样内嵌");
        assert_eq!(e1["card"]["terminal"], "done");
        assert_eq!(
            e1["card"].as_object().map(|m| m.len()),
            Some(2),
            "card 恰好 2 键"
        );
        assert_eq!(e1["state"], "done", "card 条目 state 恒 done");
    }

    /// v1.6 出站工作空间帧**键序无关逐字段断言**（与任务契约逐字段一致，SPA
    /// 侧并行实现对齐锚点）：字段名/缺省行为不得漂移；**带 machine**（v1.1
    /// 出站业务帧必带——SPA reducer 据此归属机器）。
    ///
    /// 不做完整字节串断言：workspace 统一构建下 serde_json `preserve_order`
    /// 经 agent-client-protocol 依赖链激活会改变 Value 键序——字节串断言会把
    /// 键序钉进契约且依赖构建配置，而键序对协议无语义（JSON 对象序，SPA 按
    /// 字段名解析）。本组帧虽为 derive struct 序列化（声明序稳定），仍统一
    /// 键序无关口径。
    #[test]
    fn workspace_outbound_frames_byte_level() {
        let mut convs = BTreeMap::new();
        convs.insert("local:work".to_string(), "/lw".to_string());
        convs.insert("web:work".to_string(), "/ww".to_string());
        let json = encode_workspaces_frame(
            "home-mac",
            "/def",
            vec![
                WorkspaceEntryFrame {
                    name: "a".into(),
                    path: "/a".into(),
                    last_used_at: None,
                },
                WorkspaceEntryFrame {
                    name: "b".into(),
                    path: "/b".into(),
                    last_used_at: Some(1_770_000_000),
                },
            ],
            convs,
            Vec::new(),
            None,
            None,
            false, // v1.40 asr 能力位：缺省 false 整段缺席（旧形态字节不变）
        )
        .unwrap();
        // 键序无关逐字段复核（convs 为 BTreeMap 键序稳定 + serde 按声明序
        // 组帧，但键序不纳入契约——理由见本测试 doc 注释）。
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().map(|m| m.len()), Some(6), "顶层恰好 6 键");
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "workspaces");
        assert_eq!(v["machine"], "home-mac", "出站帧带本机 machine");
        assert_eq!(v["default"], "/def");
        let ws = v["workspaces"].as_array().expect("workspaces 为数组");
        assert_eq!(ws.len(), 2);
        assert_eq!(ws[0]["name"], "a");
        assert_eq!(ws[0]["path"], "/a");
        assert!(ws[0].get("lastUsedAt").is_none(), "缺席不序列化");
        assert_eq!(
            ws[0].as_object().map(|m| m.len()),
            Some(2),
            "无 lastUsedAt 条目恰好 2 键"
        );
        assert_eq!(ws[1]["name"], "b");
        assert_eq!(ws[1]["path"], "/b");
        assert_eq!(ws[1]["lastUsedAt"], 1_770_000_000);
        let convs = v["convs"].as_object().expect("convs 为对象");
        assert_eq!(convs.len(), 2);
        assert_eq!(convs["local:work"], "/lw", "含 local: 会话");
        assert_eq!(convs["web:work"], "/ww");

        // 空集合形态：workspaces=[] / convs={}（sessions/models/backends
        // 整段缺席）。
        let json = encode_workspaces_frame(
            "home-mac",
            "/def",
            Vec::new(),
            BTreeMap::new(),
            Vec::new(),
            None,
            None,
            false, // v1.40 asr 能力位：缺省 false 整段缺席（旧形态字节不变）
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().map(|m| m.len()), Some(6), "空集恰好 6 键");
        assert_eq!(v["workspaces"].as_array().map(Vec::len), Some(0));
        assert_eq!(v["convs"].as_object().map(|m| m.len()), Some(0));
        assert!(v.get("sessions").is_none());
        assert!(v.get("models").is_none());
        assert!(v.get("backends").is_none());

        // conv_workdir 增量帧（键序无关逐字段）。
        let json = encode_conv_workdir_frame("home-mac", "web:work", "/Users/x/proj").unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().map(|m| m.len()), Some(5), "恰好 5 键");
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "conv_workdir");
        assert_eq!(v["machine"], "home-mac");
        assert_eq!(v["convId"], "web:work");
        assert_eq!(v["workdir"], "/Users/x/proj");

        // error 帧复用（workspace_invalid）（键序无关逐字段）。
        let json = encode_error_frame("home-mac", "workspace_invalid", "名称不能为空").unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().map(|m| m.len()), Some(5), "恰好 5 键");
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "error");
        assert_eq!(v["machine"], "home-mac");
        assert_eq!(v["code"], "workspace_invalid");
        assert_eq!(v["message"], "名称不能为空");
    }

    /// v1.7 workspaces 快照 sessions 段**键序无关逐字段断言**（契约逐字段
    /// 一致：段置尾、rename `updatedAt`、条目三字段齐备）；sessions 段存在
    /// 时旧三段（default/workspaces/convs）字段形态不变；空集整段缺席
    ///（无 web: 会话的网关快照与 v1.6 字段级一致）。不做完整字节串断言
    ///（preserve_order 经依赖链激活改变 Value 键序、依赖构建配置；键序对
    /// 协议无语义——JSON 对象序，SPA 按字段名解析）。
    #[test]
    fn workspaces_frame_sessions_byte_level() {
        let json = encode_workspaces_frame(
            "home-mac",
            "/def",
            Vec::new(),
            BTreeMap::new(),
            vec![
                SessionEntryFrame {
                    id: "web:work".into(),
                    title: "列出目录".into(),
                    updated_at: 1_770_000_002,
                },
                SessionEntryFrame {
                    id: "web:default".into(),
                    title: "手动名".into(),
                    updated_at: 1_770_000_001,
                },
            ],
            None,
            None,
            false, // v1.40 asr 能力位：缺省 false 整段缺席（旧形态字节不变）
        )
        .unwrap();
        // 键序无关逐字段复核（derive struct 声明序组帧、sessions 段在 convs
        // 之后，但键序不纳入契约——理由见本测试 doc 注释）。
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v.as_object().map(|m| m.len()),
            Some(7),
            "带 sessions 恰好 7 键"
        );
        assert_eq!(v["default"], "/def");
        assert_eq!(v["workspaces"].as_array().map(Vec::len), Some(0));
        assert_eq!(v["convs"].as_object().map(|m| m.len()), Some(0));
        let sessions = v["sessions"].as_array().expect("sessions 为数组");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0]["id"], "web:work");
        assert_eq!(sessions[0]["title"], "列出目录");
        assert_eq!(sessions[0]["updatedAt"], 1_770_000_002);
        assert_eq!(
            sessions[0].as_object().map(|m| m.len()),
            Some(3),
            "条目恰好 3 键（id/title/updatedAt）"
        );
        assert_eq!(sessions[1]["id"], "web:default");
        assert_eq!(sessions[1]["title"], "手动名");
        assert_eq!(sessions[1]["updatedAt"], 1_770_000_001);

        // 空集：整段缺席（与 v1.6 字段级一致，键序无关口径）。
        let json = encode_workspaces_frame(
            "home-mac",
            "/def",
            Vec::new(),
            BTreeMap::new(),
            Vec::new(),
            None,
            None,
            false, // v1.40 asr 能力位：缺省 false 整段缺席（旧形态字节不变）
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().map(|m| m.len()), Some(6), "空集恰好 6 键");
        assert!(v.get("sessions").is_none(), "sessions 整段缺席");
    }

    /// v1.8 workspaces 快照 models 段**键序无关逐字段断言**（契约逐字段一致：
    /// 段置尾（sessions 之后）、rename `cliDefault`、current/cliDefault/target
    /// 的 null 形态、options 元素恰 label/target 两键）；models 段存在时旧三段
    ///（default/workspaces/convs）字段形态不变；options 空 / None → 整段缺席。
    /// 不做完整字节串断言（preserve_order 经依赖链激活改变 Value 键序、依赖
    /// 构建配置；键序对协议无语义——JSON 对象序，SPA 按字段名解析）。
    #[test]
    fn workspaces_frame_models_byte_level() {
        let json = encode_workspaces_frame(
            "home-mac",
            "/def",
            Vec::new(),
            BTreeMap::new(),
            Vec::new(),
            Some(ModelsFrame {
                current: None,
                cli_default: Some("glm-4.6".into()),
                options: vec![
                    ModelOption {
                        label: "sonnet".into(),
                        target: Some("glm-4.6".into()),
                    },
                    ModelOption {
                        label: "opus".into(),
                        target: None,
                    },
                ],
            }),
            None,
            false, // v1.40 asr 能力位：缺省 false 整段缺席（旧形态字节不变）
        )
        .unwrap();
        // 键序无关逐字段复核（derive struct 声明序组帧、models 段在 sessions
        // 之后；null 按契约序列化为字面 null，非缺席——以键数钉住）。
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v.as_object().map(|m| m.len()),
            Some(7),
            "带 models 恰好 7 键"
        );
        assert_eq!(
            v["models"].as_object().map(|m| m.len()),
            Some(3),
            "models 恰好 3 键（current/cliDefault/options）"
        );
        assert_eq!(v["models"]["current"], serde_json::Value::Null);
        assert_eq!(v["models"]["cliDefault"], "glm-4.6");
        let options = v["models"]["options"].as_array().expect("options 为数组");
        assert_eq!(options.len(), 2);
        assert_eq!(options[0]["label"], "sonnet");
        assert_eq!(options[0]["target"], "glm-4.6");
        assert_eq!(options[0].as_object().map(|m| m.len()), Some(2));
        assert_eq!(options[1]["label"], "opus");
        assert_eq!(
            options[1]["target"],
            serde_json::Value::Null,
            "target null 序列化为字面 null，非缺席"
        );
        assert_eq!(options[1].as_object().map(|m| m.len()), Some(2));

        // options 空 → 整段缺席（组帧出口统一兜底，与 None 等价）。
        for models in [
            None,
            Some(ModelsFrame {
                current: Some("sonnet".into()),
                cli_default: None,
                options: Vec::new(),
            }),
        ] {
            let json = encode_workspaces_frame(
                "home-mac",
                "/def",
                Vec::new(),
                BTreeMap::new(),
                Vec::new(),
                models,
                None,
                false, // v1.40 asr 能力位：缺省 false 整段缺席（旧形态字节不变）
            )
            .unwrap();
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(
                v.as_object().map(|m| m.len()),
                Some(6),
                "空 options / None 整段缺席"
            );
            assert!(v.get("models").is_none(), "空 options / None 整段缺席");
        }

        // 与 sessions 段并存：sessions 在前 models 在尾。
        let json = encode_workspaces_frame(
            "home-mac",
            "/def",
            Vec::new(),
            BTreeMap::new(),
            vec![SessionEntryFrame {
                id: "web:work".into(),
                title: "标题".into(),
                updated_at: 1,
            }],
            Some(ModelsFrame {
                current: Some("sonnet".into()),
                cli_default: None,
                options: vec![ModelOption {
                    label: "sonnet".into(),
                    target: None,
                }],
            }),
            Some(BackendsFrame {
                current: "claude-cli".into(),
                options: vec!["claude-cli".into(), "codex-app".into()],
            }),
            false, // v1.40 asr 能力位：缺省 false 整段缺席（旧形态字节不变）
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            json.find(r#""sessions":"#).unwrap() < json.find(r#""models":"#).unwrap(),
            "sessions 在 models 前：{json}"
        );
        assert!(
            json.find(r#""models":"#).unwrap() < json.find(r#""backends":"#).unwrap(),
            "models 在 backends 前（backends 段置尾）：{json}"
        );
        assert_eq!(v["models"]["current"], "sonnet");
    }

    /// v1.36 workspaces 快照 backends 段**键序无关逐字段断言**（契约逐字段
    /// 一致：段置尾（models 之后）、current/options 纯字符串、options 元素
    /// 即引擎名裸串）；backends 段存在时旧各段字段形态不变；options 空 /
    /// None → 整段缺席（组帧出口统一兜底）。不做完整字节串断言（理由同
    /// sessions/models 段测试）。
    #[test]
    fn workspaces_frame_backends_byte_level() {
        // 含段：current = 首位 default；options 保序全注册表名。
        let json = encode_workspaces_frame(
            "home-mac",
            "/def",
            Vec::new(),
            BTreeMap::new(),
            Vec::new(),
            None,
            Some(BackendsFrame {
                current: "claude-cli".into(),
                options: vec!["claude-cli".into(), "codex-app".into()],
            }),
            false, // v1.40 asr 能力位：缺省 false 整段缺席（旧形态字节不变）
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v.as_object().map(|m| m.len()),
            Some(7),
            "带 backends 恰好 7 键"
        );
        assert_eq!(
            v["backends"].as_object().map(|m| m.len()),
            Some(2),
            "backends 恰好 2 键（current/options）"
        );
        assert_eq!(v["backends"]["current"], "claude-cli");
        let options = v["backends"]["options"].as_array().expect("options 为数组");
        assert_eq!(options.len(), 2, "options = 全注册表名：{options:?}");
        assert_eq!(options[0], "claude-cli", "保序：default 在首位");
        assert_eq!(options[1], "codex-app");

        // 缺段两形态：None / options 空 → 旧形态字节不变（键序无关口径）。
        for backends in [
            None,
            Some(BackendsFrame {
                current: "claude-cli".into(),
                options: Vec::new(),
            }),
        ] {
            let json = encode_workspaces_frame(
                "home-mac",
                "/def",
                Vec::new(),
                BTreeMap::new(),
                Vec::new(),
                None,
                backends,
                false, // v1.40 asr 能力位：缺省 false 整段缺席（旧形态字节不变）
            )
            .unwrap();
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(
                v.as_object().map(|m| m.len()),
                Some(6),
                "空 options / None 整段缺席"
            );
            assert!(v.get("backends").is_none(), "空 options / None 整段缺席");
        }
    }

    /// v1.40 workspaces 快照 asr 能力位**键序无关逐字段断言**（契约逐字段
    /// 一致：字面布尔 `true` 非对象段、置尾）；asr=false 整段缺席（旧形态
    /// 字节不变）；与既有各段并存时键数正确。不做完整字节串断言（理由同
    /// sessions/models 段测试）。
    #[test]
    fn workspaces_frame_asr_flag_byte_level() {
        // true：顶层 8 键（基线 7 + asr），字面 true。
        let json = encode_workspaces_frame(
            "home-mac",
            "/def",
            Vec::new(),
            BTreeMap::new(),
            Vec::new(),
            None,
            None,
            true,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v.as_object().map(|m| m.len()),
            Some(7),
            "带 asr 恰好 7 键（v1.36 基线 6 + asr）：{json}"
        );
        assert_eq!(v["asr"], true, "asr = 字面 true");
        assert!(json.contains(r#""asr":true"#), "字面 true 非对象段：{json}");

        // false：整段缺席（6 键 = v1.36 基线，旧形态字节不变）。
        let json = encode_workspaces_frame(
            "home-mac",
            "/def",
            Vec::new(),
            BTreeMap::new(),
            Vec::new(),
            None,
            None,
            false,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v.as_object().map(|m| m.len()),
            Some(6),
            "false 整段缺席（v1.36 基线键数）"
        );
        assert!(v.get("asr").is_none(), "false 整段缺席");

        // 与 backends 段并存：7 键，asr 在 backends 之后（struct 声明序）。
        let json = encode_workspaces_frame(
            "home-mac",
            "/def",
            Vec::new(),
            BTreeMap::new(),
            Vec::new(),
            None,
            Some(BackendsFrame {
                current: "claude-cli".into(),
                options: vec!["claude-cli".into()],
            }),
            true,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v.as_object().map(|m| m.len()),
            Some(8),
            "backends + asr 并存恰好 8 键（基线 7 + asr）"
        );
        assert_eq!(v["asr"], true);
        assert!(
            json.find(r#""backends":"#).unwrap() < json.find(r#""asr":true"#).unwrap(),
            "asr 段置尾（backends 之后）：{json}"
        );
    }

    /// v1.40 asr_result 帧**键序无关逐字段断言 + 字节级精确串**（契约逐字段
    /// 一致：v/type/machine/text/durationMs 恰 5 键、rename `durationMs`、
    /// 带本机 machine；derive struct 声明序稳定——完整字节串断言钉住组帧
    /// 形态，SPA 并行批对齐锚点；若未来声明序变化需同步更新 SPA 侧预期）。
    #[test]
    fn asr_result_frame_byte_level() {
        let json = encode_asr_result_frame("home-mac", "你好 世界", 1234).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().map(|m| m.len()), Some(5), "恰好 5 键");
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "asr_result");
        assert_eq!(v["machine"], "home-mac", "出站帧带本机 machine");
        assert_eq!(v["text"], "你好 世界");
        assert_eq!(v["durationMs"], 1234, "rename durationMs");
        // 字节级精确串（键序 = 结构体声明序，序列化确定性）。
        assert_eq!(
            json,
            r#"{"v":1,"type":"asr_result","machine":"home-mac","text":"你好 世界","durationMs":1234}"#
        );
        // 空文本也是合法成功帧（契约：没听清由 SPA 提示）。
        let json = encode_asr_result_frame("home-mac", "", 1).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["text"], "");
        assert!(v["durationMs"].as_u64().is_some_and(|n| n > 0));
    }

    /// v1.9 title 规则三档（任务验收项）：手动名优先（trim 生效、空白名
    /// 视为缺席）> 语义标题（trim 非空 + 防御性按码点截 24）> first_prompt
    /// 单行摘要（空白折叠单空格 + 按码点截 24）；皆无 → 空串。
    #[test]
    fn session_title_manual_semantic_then_prompt_digest() {
        // 手动名最优先（有语义标题也不抢）。
        assert_eq!(
            session_title(Some("  手动名  "), Some("语义标题"), Some("第一句")),
            "手动名"
        );
        assert_eq!(
            session_title(Some("   "), Some("语义标题"), Some("第一句")),
            "语义标题",
            "空白手动名视为缺席"
        );
        // 语义标题次优：trim 生效 + 防御性截 24（生成侧已截，此处钉住口径）。
        assert_eq!(session_title(None, Some("  部署巡检 "), None), "部署巡检");
        assert_eq!(
            session_title(None, Some("   "), Some("第一句")),
            "第一句",
            "空白语义标题视为缺席"
        );
        let long_semantic = "语义标题生成器输出了一行超过二十四码点的长标题样例文本甲乙丙";
        assert_eq!(long_semantic.chars().count(), 30);
        assert_eq!(
            session_title(None, Some(long_semantic), None),
            "语义标题生成器输出了一行超过二十四码点的长标题样"
        );
        // first_prompt 摘要兜底（v1.7 原逻辑不变）。
        let long = "这是一段超过二十四个字符长度的会话首条消息摘要样例文本";
        assert_eq!(long.chars().count(), 27);
        assert_eq!(
            session_title(None, None, Some(long)),
            "这是一段超过二十四个字符长度的会话首条消息摘要样"
        );
        // 空白折叠：换行/制表/连续空格 → 单空格；首尾 trim。
        assert_eq!(
            session_title(None, None, Some("  hello \n world\t\tnext  ")),
            "hello world next"
        );
        // 皆无 → 空串（网关不编造显示名）。
        assert_eq!(session_title(None, None, None), "");
        assert_eq!(session_title(Some(""), None, None), "");
        assert_eq!(session_title(None, Some(""), None), "");
    }

    /// v1.7 message 帧媒体段映射：合法形态 → MediaMessage（text 可缺省/空
    /// 串——纯图消息）；media 字段畸形（缺字段/类型错/非对象）→ Warn 丢弃；
    /// 无 media 且无 text → Warn（既有纪律不变）。
    #[test]
    fn map_message_with_media() {
        let act = map_raw_frame(
            r#"{"v":1,"type":"message","convId":"web:work","text":"看图","media":{"name":"shot.png","mime":"image/png","b64":"aGk="}}"#,
            SESSION,
        );
        let InboundAction::MediaMessage {
            conv,
            text,
            media,
            client_id,
        } = act
        else {
            panic!("带 media 的 message 应映射为 MediaMessage：{act:?}")
        };
        assert_eq!(conv, "web:work");
        assert_eq!(text.as_deref(), Some("看图"));
        assert_eq!(media.name, "shot.png");
        assert_eq!(media.mime, "image/png");
        assert_eq!(media.b64, "aGk=");
        assert_eq!(client_id, None, "帧无 clientId → None");

        // 纯图：text 缺省 / 空串（泵内归一 text=None）；裸 session 名补前缀。
        for raw in [
            r#"{"v":1,"type":"message","convId":"work","media":{"name":"shot.png","mime":"image/png","b64":"aGk="}}"#,
            r#"{"v":1,"type":"message","convId":"work","text":"","media":{"name":"shot.png","mime":"image/png","b64":"aGk="}}"#,
        ] {
            let act = map_raw_frame(raw, SESSION);
            let InboundAction::MediaMessage { conv, .. } = act else {
                panic!("纯图 message 应映射为 MediaMessage：{act:?}")
            };
            assert_eq!(conv, "web:work");
        }

        // media 畸形：缺 b64 / mime 非字符串 / media 非对象 → Warn。
        for raw in [
            r#"{"v":1,"type":"message","text":"x","media":{"name":"a.png","mime":"image/png"}}"#,
            r#"{"v":1,"type":"message","text":"x","media":{"name":"a.png","mime":1,"b64":"aGk="}}"#,
            r#"{"v":1,"type":"message","text":"x","media":"png"}"#,
        ] {
            assert!(
                matches!(map_raw_frame(raw, SESSION), InboundAction::Warn(_)),
                "{raw} 应 Warn 丢弃"
            );
        }
        // 无 media 无 text → Warn（v1.7 前既有行为不变）。
        assert!(matches!(
            map_raw_frame(r#"{"v":1,"type":"message"}"#, SESSION),
            InboundAction::Warn(_)
        ));
        // media:null（JS 可选字段 null 形态）按无 media 处理：text 在 → 纯文本。
        let act = map_raw_frame(
            r#"{"v":1,"type":"message","convId":"work","text":"纯文本","media":null}"#,
            SESSION,
        );
        assert!(
            matches!(act, InboundAction::Message(_)),
            "media:null 应走纯文本路径：{act:?}"
        );
    }

    /// v1.7 媒体消息合成：text 非空才 Some（空串 → None），media 引用落盘
    /// 路径（kind 载 mime）。
    #[test]
    fn synth_media_message_text_and_media() {
        let m = synth_media_message(
            "web:work".into(),
            Some("看图".into()),
            vec![MediaRef {
                kind: "image/png".into(),
                url: "/m/up-1-a.png".into(),
            }],
            Some("m_img00001".into()),
        );
        assert_eq!(m.conv_id, ConvId("web:work".into()));
        assert_eq!(m.text.as_deref(), Some("看图"));
        assert_eq!(m.media.len(), 1);
        assert_eq!(m.media[0].kind, "image/png");
        assert_eq!(m.media[0].url, "/m/up-1-a.png");
        assert!(!m.no_steer, "用户手打媒体消息走 steering");
        assert_eq!(
            m.client_id.as_deref(),
            Some("m_img00001"),
            "v1.44：关联 id 随合成消息写入 InboundMessage"
        );

        let m = synth_media_message(
            "web:work".into(),
            Some(String::new()),
            vec![MediaRef {
                kind: "image/jpeg".into(),
                url: "/m/up-2-b.jpg".into(),
            }],
            None,
        );
        assert_eq!(m.text, None, "空串说明归一为 None（纯图消息）");
        assert_eq!(m.client_id, None);
    }

    /// v1.44 `clientId` 形状校验（契约 §2.1）：字符串、1..=64、仅
    /// `[A-Za-z0-9_-]`；边界（恰 64 / 65、单字符）与非法形态（空串、空白、
    /// 标点、非 ASCII、非字符串、缺席）一律当缺席。
    #[test]
    fn parse_client_id_shapes() {
        let ok = |raw: &str| parse_client_id(Some(&serde_json::json!(raw)));
        assert_eq!(ok("m_1a2b3c4d").as_deref(), Some("m_1a2b3c4d"));
        assert_eq!(ok("A").as_deref(), Some("A"), "单字符合法");
        assert_eq!(ok("a-b_C9").as_deref(), Some("a-b_C9"), "字符集全集");
        let max = "m".repeat(CLIENT_ID_MAX_CHARS);
        assert_eq!(ok(&max).as_deref(), Some(max.as_str()), "恰 64 合法");
        let over = "m".repeat(CLIENT_ID_MAX_CHARS + 1);
        assert_eq!(ok(&over), None, "65 字符超长 → 当缺席");

        for bad in ["", " ", "a b", "a.b", "a:b", "a/b", "中文", "a+b", "m_1\n"] {
            assert_eq!(ok(bad), None, "非法字符 / 空串应判缺席：{bad:?}");
        }
        // 缺席与任何非字符串形态。
        assert_eq!(parse_client_id(None), None);
        for v in [
            serde_json::json!(42),
            serde_json::json!(null),
            serde_json::json!(true),
            serde_json::json!(["m_1"]),
            serde_json::json!({ "id": "m_1" }),
        ] {
            assert_eq!(parse_client_id(Some(&v)), None, "非字符串：{v}");
        }
    }

    /// v1.44 message 帧的 `clientId` 透传（纯文本与媒体两条路径）；不合规时
    /// 当缺席且**消息照旧处理**；契约只给 message 帧定义了 `clientId`，
    /// approval_response / control 帧即使带了也不透传。
    #[test]
    fn map_message_client_id_passthrough() {
        let client_id_of = |raw: &str| match map_raw_frame(raw, SESSION) {
            InboundAction::Message(m) => (m.conv_id.0.clone(), m.text.clone(), m.client_id),
            other => panic!("{raw} 应映射为 Message：{other:?}"),
        };
        // 合规：原样写入 InboundMessage.client_id，其余字段不受影响。
        let (conv, text, client_id) = client_id_of(
            r#"{"v":1,"type":"message","convId":"web:s-1","text":"列出目录","machine":"home-mac","clientId":"m_1a2b3c4d"}"#,
        );
        assert_eq!(conv, "web:s-1");
        assert_eq!(text.as_deref(), Some("列出目录"));
        assert_eq!(client_id.as_deref(), Some("m_1a2b3c4d"));

        // 不合规一律当缺席，消息照旧映射（不因可选字段丢掉用户消息）。
        let over = "m".repeat(CLIENT_ID_MAX_CHARS + 1);
        for raw in [
            format!(
                r#"{{"v":1,"type":"message","convId":"web:s-1","text":"hi","clientId":"{over}"}}"#
            ),
            r#"{"v":1,"type":"message","convId":"web:s-1","text":"hi","clientId":"m_1a.2b"}"#
                .to_string(),
            r#"{"v":1,"type":"message","convId":"web:s-1","text":"hi","clientId":42}"#.to_string(),
            r#"{"v":1,"type":"message","convId":"web:s-1","text":"hi","clientId":""}"#.to_string(),
            r#"{"v":1,"type":"message","convId":"web:s-1","text":"hi"}"#.to_string(),
        ] {
            let (conv, text, client_id) = client_id_of(&raw);
            assert_eq!(conv, "web:s-1", "{raw}");
            assert_eq!(text.as_deref(), Some("hi"), "{raw}");
            assert_eq!(client_id, None, "不合规 / 缺席 → None：{raw}");
        }

        // 媒体路径：透传到 MediaMessage.client_id。
        let act = map_raw_frame(
            r#"{"v":1,"type":"message","convId":"web:s-1","text":"看图","clientId":"m_img00001","media":{"name":"a.png","mime":"image/png","b64":"aGk="}}"#,
            SESSION,
        );
        let InboundAction::MediaMessage {
            conv, client_id, ..
        } = act
        else {
            panic!("带 media 的 message 应映射为 MediaMessage：{act:?}")
        };
        assert_eq!(conv, "web:s-1");
        assert_eq!(client_id.as_deref(), Some("m_img00001"));

        // 其它帧类型带了 clientId 也不透传（契约只定义在 message 帧上）。
        let (.., client_id) = client_id_of(
            r#"{"v":1,"type":"approval_response","convId":"web:s-1","requestId":"r1","decision":"allow","clientId":"m_no00001"}"#,
        );
        assert_eq!(client_id, None, "approval_response 不透传 clientId");
        let (.., client_id) = client_id_of(
            r#"{"v":1,"type":"control","cmd":"stop","convId":"web:s-1","clientId":"m_no00002"}"#,
        );
        assert_eq!(client_id, None, "control 不透传 clientId");
    }

    /// v1.44 gateway_info_query 入站映射：无必填字段（`machine` 由 relay 路由
    /// 用）→ [`InboundAction::GatewayInfoQuery`]，不进 recv / dispatch 命令链。
    #[test]
    fn map_gateway_info_query() {
        for raw in [
            r#"{"v":1,"type":"gateway_info_query","machine":"home-mac"}"#,
            r#"{"v":1,"type":"gateway_info_query"}"#,
        ] {
            assert!(
                matches!(map_raw_frame(raw, SESSION), InboundAction::GatewayInfoQuery),
                "{raw}"
            );
        }
    }

    /// v1.44 ack 帧字节级（契约 §2.2 的两条示例逐字）：`received` 恰 6 键、
    /// **不带** `reason` / `message`；`rejected` 恰 8 键，键序固定
    /// `v,type,machine,clientId,convId,status,reason,message`。rejected 示例的
    /// reason / message 取自 core 的 `MessageRejectReason`（单一事实源）——
    /// 本用例同时钉住 core 字面量与 web 契约示例跨 crate 逐字一致。
    #[test]
    fn ack_frame_byte_level() {
        use imagent_core::MessageRejectReason;
        let received =
            encode_ack_frame("home-mac", "m_1a2b3c4d", "web:s-1", AckStatus::Received).unwrap();
        assert_eq!(
            received,
            r#"{"v":1,"type":"ack","machine":"home-mac","clientId":"m_1a2b3c4d","convId":"web:s-1","status":"received"}"#
        );
        let v: serde_json::Value = serde_json::from_str(&received).unwrap();
        assert_eq!(v.as_object().map(|m| m.len()), Some(6), "received 恰 6 键");
        assert!(
            v.get("reason").is_none() && v.get("message").is_none(),
            "received 不带 reason / message 键：{received}"
        );

        let rejected = encode_ack_frame(
            "home-mac",
            "m_1a2b3c4d",
            "web:s-1",
            AckStatus::Rejected {
                reason: MessageRejectReason::NotAllowed.code(),
                message: MessageRejectReason::NotAllowed.text(),
            },
        )
        .unwrap();
        assert_eq!(
            rejected,
            r#"{"v":1,"type":"ack","machine":"home-mac","clientId":"m_1a2b3c4d","convId":"web:s-1","status":"rejected","reason":"not_allowed","message":"发送者未获授权，消息未被处理"}"#
        );
        let v: serde_json::Value = serde_json::from_str(&rejected).unwrap();
        assert_eq!(v.as_object().map(|m| m.len()), Some(8), "rejected 恰 8 键");

        // client 层自产的 malformed（字面量在本 crate）。
        let malformed = encode_ack_frame(
            "home-mac",
            "m_bad0001",
            "web:s-1",
            AckStatus::Rejected {
                reason: ACK_MALFORMED_REASON,
                message: ACK_MALFORMED_MESSAGE,
            },
        )
        .unwrap();
        assert_eq!(
            malformed,
            r#"{"v":1,"type":"ack","machine":"home-mac","clientId":"m_bad0001","convId":"web:s-1","status":"rejected","reason":"malformed","message":"消息格式无效"}"#
        );
    }

    /// v1.44 gateway_info 帧字节级（契约 §2.2 示例逐字；version 取本 crate
    /// 版本 = 网关版本）：恰 5 键，`caps` 为数组 `["acks"]`。
    #[test]
    fn gateway_info_frame_byte_level() {
        let json = encode_gateway_info_frame("home-mac").unwrap();
        assert_eq!(
            json,
            format!(
                r#"{{"v":1,"type":"gateway_info","machine":"home-mac","version":"{}","caps":["acks"]}}"#,
                env!("CARGO_PKG_VERSION")
            )
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().map(|m| m.len()), Some(5), "恰 5 键");
        assert_eq!(v["caps"], serde_json::json!(["acks"]), "caps 含 acks");
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        // machine 原样（任何网关都回）。
        assert!(encode_gateway_info_frame("office-mac")
            .unwrap()
            .starts_with(r#"{"v":1,"type":"gateway_info","machine":"office-mac","version":"#));
    }

    #[test]
    fn text_frame_shape() {
        let json = encode_text_frame("home-mac", "web:work", "你好", 1_730_000_000_000).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "text");
        assert_eq!(v["machine"], "home-mac", "v1.1 出站帧必带 machine");
        assert_eq!(v["convId"], "web:work");
        assert_eq!(v["text"], "你好");
        assert_eq!(v["at"], 1_730_000_000_000_u64, "v1.36 实时帧携带出站时刻");
        assert!(v.get("hint").is_none(), "hint 恒 None 不序列化");
        assert!(v.get("self").is_none(), "普通 text 帧不带 self 键");
    }

    /// v1.41.8 用户自发文本镜像帧（`send_user_text`）：恰比普通 text 帧多一个
    /// `"self":true` 键（其余字段同值）；超限截断重建后标记不丢。
    #[test]
    fn user_text_frame_carries_self_flag() {
        let json = encode_user_text_frame(
            "home-mac",
            "peer:imagent-a7",
            "帮我跑下构建",
            1_795_528_250_098,
        )
        .unwrap();
        assert_eq!(
            json,
            r#"{"v":1,"type":"text","machine":"home-mac","convId":"peer:imagent-a7","text":"帮我跑下构建","at":1795528250098,"self":true}"#,
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["self"], true);
        assert_eq!(v.as_object().map(|m| m.len()), Some(7), "恰好 7 键");
        // 除 self 外与普通帧逐字段同值（同参对照）。
        let plain: serde_json::Value = serde_json::from_str(
            &encode_text_frame(
                "home-mac",
                "peer:imagent-a7",
                "帮我跑下构建",
                1_795_528_250_098,
            )
            .unwrap(),
        )
        .unwrap();
        let mut expect = plain.clone();
        expect["self"] = serde_json::Value::Bool(true);
        assert_eq!(v, expect);

        // 超限截断：截断重建的帧仍带 self（标记不因 rebuild 丢失）。
        let long_text = "长".repeat(100_000); // 300_000 字节 > 帧软限
        let json = encode_user_text_frame("home-mac", "peer:win", &long_text, 1).unwrap();
        assert!(json.len() <= FRAME_SOFT_LIMIT_BYTES);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["text"].as_str().unwrap().contains("…[已截断]…"));
        assert_eq!(v["self"], true, "截断重建不丢 self 标记");
        assert_eq!(v["machine"], "home-mac", "截断重建不丢 machine 字段");
    }

    /// v1.41.8 history 条目透传 `self`：payload 带 `"self":true` → 条目带同名
    /// 键（恰好 5 键）；缺席 / false / 非布尔 → 不输出键（旧行形态恰 4 键，
    /// 与 history_entry_and_frame_byte_level 的键数断言同一口径）。
    #[test]
    fn history_entry_passes_through_self_flag() {
        use imagent_store::OutboundRow;
        let row = |payload: &str| OutboundRow {
            conv_id: "peer:win".into(),
            seq: 7,
            ts: 1_007,
            kind: "text".into(),
            payload: payload.into(),
        };
        let with_self =
            history_entry_from_row(&row(r#"{"text":"帮我跑下构建","at":1234,"self":true}"#))
                .expect("self 行应组装条目");
        assert_eq!(with_self["self"], true);
        assert_eq!(
            with_self.as_object().map(|m| m.len()),
            Some(5),
            "self 条目恰好 5 键"
        );
        assert_eq!(with_self["kind"], "text");
        assert_eq!(with_self["key"], "h:7");
        assert_eq!(with_self["text"], "帮我跑下构建");
        assert_eq!(with_self["at"], 1234);

        for legacy in [
            r#"{"text":"你好","at":1234}"#,
            r#"{"text":"你好","at":1234,"self":false}"#,
            r#"{"text":"你好","at":1234,"self":"true"}"#,
        ] {
            let e = history_entry_from_row(&row(legacy)).expect("行应组装条目");
            assert!(
                e.get("self").is_none(),
                "非 true 不得输出 self 键：{legacy}"
            );
            assert_eq!(
                e.as_object().map(|m| m.len()),
                Some(4),
                "旧形态恰好 4 键：{legacy}"
            );
        }
    }

    /// v1.41.8 回归钉：普通 text 帧（`send_text`）**逐字节**与加 `self` 字段
    /// 之前一致——`self=false` 不序列化。本串是 derive struct 按声明序序列化
    /// 的产物（不受 serde_json `preserve_order` 构建形态影响——与 `json!`
    /// 动态 Value 组帧不同，见 history_entry_and_frame_byte_level 注释），故
    /// 字节断言跨构建形态稳定。旧网关 / 旧 SPA 兼容与 E2E 密文内容均以此为
    /// 前提。
    #[test]
    fn text_frame_bytes_unchanged_without_self() {
        assert_eq!(
            encode_text_frame("home-mac", "web:work", "你好", 1_730_000_000_000).unwrap(),
            r#"{"v":1,"type":"text","machine":"home-mac","convId":"web:work","text":"你好","at":1730000000000}"#,
        );
    }

    fn sample_card(text: &str, terminal: CardTerminal) -> OutboundCard {
        OutboundCard {
            text: text.to_string(),
            tool_calls: vec![ToolCall {
                name: "Bash".into(),
                summary: "ls -la".into(),
                done: true,
                id: Some("t1".into()),
                diff: None,
            }],
            thoughts: vec!["想一想".into()],
            todos: vec![TodoItem {
                id: Some("1".into()),
                text: "任务".into(),
                status: TodoStatus::InProgress,
            }],
            phase: CardPhase::Outputting,
            queued_hint: None,
            run_secs: 42,
            usage_display: Some("$0.012".into()),
            task_digest: Some("列出目录".into()),
            terminal,
        }
    }

    #[test]
    fn push_notify_frame_byte_level() {
        // 共享契约字节级：{"v":1,"type":"push_notify","machine","kind",
        // "convId","title"}——字段名与取值逐字（SPA 侧 push.ts 与中继
        // parse_notify_frame 双端对齐锚点）。
        let ev = PushEvent {
            kind: PushNotifyKind::RoundDone,
            conv_id: "web:s-ab12".into(),
            title: "统计依赖数量".into(),
        };
        let json = encode_push_notify_frame("home-mac", &ev);
        assert_eq!(
            json,
            r#"{"v":1,"type":"push_notify","machine":"home-mac","kind":"round_done","convId":"web:s-ab12","title":"统计依赖数量"}"#
        );
        // 另两 kind 的字符串形态。
        let ev = PushEvent {
            kind: PushNotifyKind::RoundError,
            conv_id: "web:c".into(),
            title: "t".into(),
        };
        assert!(encode_push_notify_frame("m", &ev).contains(r#""kind":"round_error""#));
        let ev = PushEvent {
            kind: PushNotifyKind::Approval,
            conv_id: "web:c".into(),
            title: "Bash".into(),
        };
        assert!(encode_push_notify_frame("m", &ev).contains(r#""kind":"approval""#));

        // v1.42 peer_reply：字符串形态 + 字节级（convId = peer:<窗口名> 完整
        // conv，title = 回信正文摘要）。
        assert_eq!(PushNotifyKind::PeerReply.as_str(), "peer_reply");
        let ev = PushEvent {
            kind: PushNotifyKind::PeerReply,
            conv_id: "peer:win-a".into(),
            title: push_notify_title("  已修好构建  "),
        };
        assert_eq!(
            encode_push_notify_frame("home-mac", &ev),
            r#"{"v":1,"type":"push_notify","machine":"home-mac","kind":"peer_reply","convId":"peer:win-a","title":"已修好构建"}"#
        );
        // 空白正文 → title 空串（契约：前端有兜底文案），键不省略。
        let ev = PushEvent {
            kind: PushNotifyKind::PeerReply,
            conv_id: "peer:win-a".into(),
            title: push_notify_title(" \n "),
        };
        assert_eq!(
            encode_push_notify_frame("home-mac", &ev),
            r#"{"v":1,"type":"push_notify","machine":"home-mac","kind":"peer_reply","convId":"peer:win-a","title":""}"#
        );

        // v1.43 两个窗口事件 kind：字符串形态逐字 + 字节级（convId =
        // peer:<窗口名>，title = 窗口名——与中继 parse_notify_frame 白名单、SPA
        // notificationCopy 三端对齐锚点）。
        assert_eq!(PushNotifyKind::WindowTurnEnd.as_str(), "window_turn_end");
        assert_eq!(PushNotifyKind::WindowWaiting.as_str(), "window_waiting");
        let ev = PushEvent {
            kind: PushNotifyKind::WindowTurnEnd,
            conv_id: "peer:win-a".into(),
            title: "win-a".into(),
        };
        assert_eq!(
            encode_push_notify_frame("home-mac", &ev),
            r#"{"v":1,"type":"push_notify","machine":"home-mac","kind":"window_turn_end","convId":"peer:win-a","title":"win-a"}"#
        );
        let ev = PushEvent {
            kind: PushNotifyKind::WindowWaiting,
            conv_id: "peer:win-a".into(),
            title: "win-a".into(),
        };
        assert_eq!(
            encode_push_notify_frame("home-mac", &ev),
            r#"{"v":1,"type":"push_notify","machine":"home-mac","kind":"window_waiting","convId":"peer:win-a","title":"win-a"}"#
        );
    }

    #[test]
    fn push_notify_title_truncates_by_codepoints() {
        // trim + 按码点截 32（中文不劈半）。
        assert_eq!(push_notify_title("  hello world  "), "hello world");
        let cn = "统".repeat(40);
        let t = push_notify_title(&cn);
        assert_eq!(t.chars().count(), PUSH_TITLE_MAX_CHARS);
        assert_eq!(t, "统".repeat(PUSH_TITLE_MAX_CHARS));
        // 混合码点（emoji 按 1 码点计）。
        let mixed = format!("{}{}", "🦀".repeat(40), "尾巴");
        assert_eq!(
            push_notify_title(&mixed).chars().count(),
            PUSH_TITLE_MAX_CHARS
        );
        assert_eq!(push_notify_title(""), "");
    }

    #[test]
    fn card_frame_shape_and_state_derivation() {
        let running = CardPayload::from_card(&sample_card("流式中", CardTerminal::Running));
        assert_eq!(running.state(), "streaming");
        let json =
            encode_card_frame("home-mac", "card", "web:work", "web-card-1", &running).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "card");
        assert_eq!(v["machine"], "home-mac", "v1.1 出站帧必带 machine");
        assert_eq!(v["convId"], "web:work");
        assert_eq!(v["messageId"], "web-card-1");
        assert_eq!(v["state"], "streaming");
        assert_eq!(v["card"]["text"], "流式中");
        assert_eq!(v["card"]["phase"], "outputting");
        assert_eq!(v["card"]["terminal"], "running");
        assert_eq!(v["card"]["tool_calls"][0]["name"], "Bash");
        assert_eq!(v["card"]["todos"][0]["status"], "in_progress");
        assert_eq!(v["card"]["usage_display"], "$0.012");
        assert!(v["card"].get("queued_hint").is_none());

        // card_patch + 终态 state=done；Error 终态带 error 字段。
        let done = CardPayload::from_card(&sample_card("完成", CardTerminal::Done));
        assert_eq!(done.state(), "done");
        let json =
            encode_card_frame("home-mac", "card_patch", "web:work", "web-card-1", &done).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "card_patch");
        assert_eq!(v["machine"], "home-mac");
        assert_eq!(v["state"], "done");
        assert!(v["card"].get("error").is_none());

        let err = CardPayload::from_card(&sample_card("炸了", CardTerminal::Error("boom".into())));
        assert_eq!(err.state(), "done");
        let json = encode_card_frame("home-mac", "card", "web:work", "web-card-1", &err).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["card"]["terminal"], "error");
        assert_eq!(v["card"]["error"], "boom");
    }

    /// #7 diff 视图：ToolCallPayload 的 diff 字段带/缺两形态字节级（SPA 折叠区
    /// 按字段存在性渲染——缺失即不序列化，非 null）。
    #[test]
    fn tool_call_diff_payload_bytes() {
        let mut card = sample_card("改文件", CardTerminal::Done);
        card.tool_calls[0].name = "Edit".into();
        card.tool_calls[0].diff = Some("- old\n+ new".into());
        let payload = CardPayload::from_card(&card);
        let json =
            encode_card_frame("home-mac", "card", "web:work", "web-card-1", &payload).unwrap();
        // 字节级精确串（键序 = 结构体声明序）：diff 紧随 id。
        assert!(json.contains(
            r#""tool_calls":[{"name":"Edit","summary":"ls -la","done":true,"id":"t1","diff":"- old\n+ new"}]"#
        ), "带 diff 应整块精确序列化: {json}");

        // 缺 diff（None → 字段缺席，缺省形态字节与 v1.38 一致）。
        let card = sample_card("无 diff", CardTerminal::Done);
        let payload = CardPayload::from_card(&card);
        let json =
            encode_card_frame("home-mac", "card", "web:work", "web-card-1", &payload).unwrap();
        assert!(
            json.contains(
                r#""tool_calls":[{"name":"Bash","summary":"ls -la","done":true,"id":"t1"}]"#
            ),
            "缺 diff 不应序列化字段（非 null）: {json}"
        );
    }

    #[test]
    fn approval_frames_shape() {
        // 带 timeoutSecs（v1.37）：字节级钉住全帧形状（键名 camelCase 契约，
        // SPA 侧按此对齐——断言不依赖 JSON 键序）。
        let json = encode_approval_request_frame(
            "home-mac",
            "web:c",
            "req-9",
            "Bash",
            "rm -rf /tmp/x",
            Some(300),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "approval_request");
        assert_eq!(v["machine"], "home-mac", "v1.1 出站帧必带 machine");
        assert_eq!(v["convId"], "web:c");
        assert_eq!(v["requestId"], "req-9");
        assert_eq!(v["tool"], "Bash");
        assert_eq!(v["inputSummary"], "rm -rf /tmp/x");
        assert_eq!(v["timeoutSecs"], 300, "v1.37 审批超时预算下发");
        // 字节级精确串（键序 = 结构体声明序，序列化确定性）。
        assert_eq!(
            json,
            r#"{"v":1,"type":"approval_request","machine":"home-mac","convId":"web:c","requestId":"req-9","tool":"Bash","inputSummary":"rm -rf /tmp/x","timeoutSecs":300}"#
        );

        // 不带（None → 缺席，旧网关/未配置形态——SPA 不渲染倒计时）。
        let json = encode_approval_request_frame(
            "home-mac",
            "web:c",
            "req-9",
            "Bash",
            "rm -rf /tmp/x",
            None,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("timeoutSecs").is_none(), "None 不得序列化该字段");
        assert_eq!(
            json,
            r#"{"v":1,"type":"approval_request","machine":"home-mac","convId":"web:c","requestId":"req-9","tool":"Bash","inputSummary":"rm -rf /tmp/x"}"#
        );

        let json = encode_approval_resolved_frame(
            "home-mac",
            "web:c",
            "req-9",
            "cancelled",
            Some("审批已取消"),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "approval_resolved");
        assert_eq!(v["machine"], "home-mac");
        assert_eq!(v["decision"], "cancelled");
        assert_eq!(v["message"], "审批已取消");
        // message 缺省不序列化。
        let json =
            encode_approval_resolved_frame("home-mac", "web:c", "req-9", "allow", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("message").is_none());
    }

    /// 截断：超 240KB 的 card 帧截断 card.text 保头尾（头 18000 / 尾 6000 char
    /// + 标记），截后帧 < 240KB。
    #[test]
    fn oversized_card_frame_truncated_head_tail() {
        // 25000 个中文 = 75000 字节 < 240KB 不触发；用 10 万中文字（300KB）触发。
        let long_text = "长".repeat(100_000); // 300_000 字节
        let card = CardPayload::from_card(&sample_card(&long_text, CardTerminal::Running));
        let json = encode_card_frame("home-mac", "card", "web:work", "web-card-1", &card).unwrap();
        assert!(
            json.len() <= FRAME_SOFT_LIMIT_BYTES,
            "截断后帧应 ≤240KB：{}",
            json.len()
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let text = v["card"]["text"].as_str().unwrap();
        assert!(text.contains("…[已截断]…"), "应有截断标记");
        assert!(text.starts_with(&"长".repeat(TRUNC_HEAD_CHARS)), "头部保窗");
        assert!(text.ends_with(&"长".repeat(TRUNC_TAIL_CHARS)), "尾部保窗");
        assert_eq!(v["machine"], "home-mac", "截断重建不丢 machine 字段");

        // text 帧同款截断。
        let json = encode_text_frame("home-mac", "web:work", &long_text, 1).unwrap();
        assert!(json.len() <= FRAME_SOFT_LIMIT_BYTES);
        assert!(json.contains("…[已截断]…"));

        // 未超限帧原样通过。
        let small = encode_text_frame("home-mac", "web:work", "short", 1).unwrap();
        assert!(small.contains("short"));
    }

    #[test]
    fn truncate_text_head_tail_window() {
        // 短文原样。
        assert_eq!(truncate_text("hello"), "hello");
        assert_eq!(truncate_text(""), "");
        // 超长：头尾保留 + 标记（构造恰好头+尾+1 char 的输入）。
        let mut text = String::new();
        text.push_str(&"a".repeat(TRUNC_HEAD_CHARS));
        text.push('X'); // 中段唯一字符，被截掉
        text.push_str(&"z".repeat(TRUNC_TAIL_CHARS));
        let out = truncate_text(&text);
        assert!(out.starts_with(&"a".repeat(TRUNC_HEAD_CHARS)));
        assert!(out.ends_with(&"z".repeat(TRUNC_TAIL_CHARS)));
        assert!(!out.contains('X'), "中段应被截断");
        assert!(out.contains("…[已截断]…"));
    }

    #[test]
    fn pong_frame_shape() {
        // 键序无关逐字段（pong 为 format! 手拼帧、键序固定，仍统一键序无关
        // 口径——完整字节串断言不纳入契约，理由见 history 测试 doc 注释）。
        let json = encode_pong_frame("home-mac");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().map(|m| m.len()), Some(3), "恰好 3 键");
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "pong");
        assert_eq!(v["machine"], "home-mac");
    }

    /// machine id 规范化：合法值原样 / 非法字符替换 / 首尾 `-` 剥离 / 截断
    /// 32 char / 全非法输入 → None / 空串 → None。
    #[test]
    fn machine_id_sanitization() {
        assert_eq!(
            sanitize_machine_id("home-mac"),
            Some("home-mac".to_string()),
            "合法值原样"
        );
        assert_eq!(sanitize_machine_id("work_01"), Some("work_01".to_string()));
        assert_eq!(
            sanitize_machine_id("Uzziahs MacBook.(local)"),
            Some("Uzziahs-MacBook--local".to_string()),
            "非法字符替换为 -，首尾 - 剥离"
        );
        assert_eq!(
            sanitize_machine_id(&"m".repeat(MACHINE_ID_MAX_CHARS + 5)),
            Some("m".repeat(MACHINE_ID_MAX_CHARS)),
            "截断到 32 char"
        );
        assert_eq!(sanitize_machine_id("..."), None, "全标点 → None");
        assert_eq!(sanitize_machine_id("   "), None, "空白 → None");
        assert_eq!(sanitize_machine_id(""), None, "空串 → None");
        assert_eq!(sanitize_machine_id("家"), None, "非 ASCII → None");
    }

    /// 回归：超长输入的截断点恰好落在中间连字符上时（GitHub Actions
    /// runner hostname 形态），截断后必须再次剥尾——产出不以 `-` 结尾，
    /// 且自身已是规范形态（sanitize 幂等）。这钉住
    /// `resolve_machine_id_prefers_configured` 在 CI runner 上不退化。
    #[test]
    fn machine_id_truncation_at_hyphen_strips_trailing_hyphen() {
        // 35 char，第 MACHINE_ID_MAX_CHARS(=32) 个 char 恰为 `-`。
        let raw = "iad20-gt1023-4ce47d8d-f80c-4519-xyz";
        assert_eq!(raw.chars().count(), MACHINE_ID_MAX_CHARS + 3);
        assert_eq!(
            raw.chars().nth(MACHINE_ID_MAX_CHARS - 1),
            Some('-'),
            "前提：截断点落在中间连字符上"
        );

        let out = sanitize_machine_id(raw).expect("字符集合法，不应为 None");
        // 截断产出以 `-` 结尾，剥尾后长度必为 31（< 上限但 != 上限）。
        assert_eq!(
            out.chars().count(),
            MACHINE_ID_MAX_CHARS - 1,
            "尾部截断连字符被剥掉：{out}"
        );
        assert!(
            out.chars().count() <= MACHINE_ID_MAX_CHARS,
            "协议上限仍须满足"
        );
        assert!(
            !out.starts_with('-') && !out.ends_with('-'),
            "产出不得以 - 开头/结尾：{out}"
        );
        assert_eq!(
            sanitize_machine_id(&out).as_deref(),
            Some(out.as_str()),
            "产出自身已是规范形态（幂等）：{out}"
        );
    }

    /// 回归：≤[`MACHINE_ID_MAX_CHARS`] 的普通输入，首尾 `-` 剥离仍生效
    /// （补丁不得回归未触发截断路径的既有语义）。
    #[test]
    fn machine_id_short_input_still_trims_edges() {
        assert_eq!(
            sanitize_machine_id("-short-name-"),
            Some("short-name".to_string()),
            "未触发截断时首尾 - 照常剥离"
        );
    }

    /// 装配期 machine id 解析：显式配置优先（trim + 规范化）；显式但坏值
    /// / 缺省 → hostname 短名链（此处只断言非空 + 合法形状——hostname 探测
    /// 是 IO，交给 `default_machine_id` 的装配期日志，不在单测里钉具体值）。
    #[test]
    fn resolve_machine_id_prefers_configured() {
        assert_eq!(resolve_machine_id(Some(" home-mac ")), "home-mac");
        assert_eq!(resolve_machine_id(Some("bad value!")), "bad-value");
        // 缺省链结果必为协议合法值（非空、≤32、合法字符集）。
        let got = resolve_machine_id(None);
        assert_eq!(
            sanitize_machine_id(&got).as_deref(),
            Some(got.as_str()),
            "缺省链产出必须已是规范形态：{got}"
        );
    }
}
