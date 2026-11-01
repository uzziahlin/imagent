//! [`WebPlatform`]：实现 [`imagent_core::Platform`]，接入 Dispatcher 全管线。
//!
//! 组织方式参照 wecom（client 任务 + 双 mpsc + 平台薄壳），但 web 无 proto
//! 加密、无 outbox、无 IM 协议——出站即 JSON 帧：
//! - `recv()`：inbound channel 前端（client 已把浏览器帧映射成
//!   [`InboundMessage`]）；断线期间无消息天然 pending（与 IM 平台断线一致）。
//! - `send_text` / `send_card` / `update_card` / `send_permission_ask` 族 /
//!   `reconnect` / `doctor_probes`：覆写为对应帧（见 lib.rs 帧药化函数），
//!   经出站 channel（容量 256）交给 client。断线期帧排队（client 重连后按序
//!   发出）；队列满（relay 长时间不可达）→ Err 上抛。
//! - sync 重发（D6 故障语义）：per-conv 缓存最近卡片（send_card / update_card
//!   更新，上限 32 conv 淘汰最旧），浏览器发 `sync` 时由独立 responder 任务
//!   全量重发 card 帧。relay 保证单浏览器连接，无定向问题。
//! - workspace 泵（v1.6 工作空间）：消费 client 转来的
//!   workspace_list/save/remove/conv_workdir_set 动作，直接读写 store KV
//!   （`workspace:`/`workdir:` 表，与终端 /ws /cd 同源互通）+
//!   [`imagent_core::validate_workdir`] 校验（复用 /cd 护栏，不经 dispatch
//!   命令链），每帧回 workspaces 全量快照（失败前置 error 帧）。构造注入
//!   store 句柄 + default_workdir（main 装配处传入）。
//! - 媒体泵（v1.7 媒体上行）：消费 client 转来的 message+media 动作——校验
//!   （mime 须 image/*、b64 解码 ≤8MB）→ 落盘 `<imagent_home>/media/
//!   up-<unix_ms>-<安全化name>`（目录 0755、文件 0600）→ 合成
//!   InboundMessage（text 非空才 Some）投回 inbound channel；任一步失败回
//!   error 帧（media_too_large / media_invalid）且不投递。
//! - 出站落盘 + history 泵（v1.8 会话历史重放）：send_text / card 终态帧
//!   （state=="done"）出站成功后异步落 store `outbound_log`（spawn 短任务
//!   不阻塞热路径；同 messageId 终态只落一次；流式中间帧 / sync 重发不落）；
//!   history_query 动作查 `list_outbound_since` 回 history 帧（增量重放，
//!   浏览器按本地高水位拉取）。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use imagent_store::{OutboundPage, Store, OUTBOUND_RETAIN};
use tokio::sync::{mpsc, Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use imagent_core::{
    validate_workdir,
    workspace::{workdir_key, workspace_key, WorkspaceRecord, WORKDIR_PREFIX, WORKSPACE_PREFIX},
    ConvId, CoreError, InboundMessage, MediaRef, OutboundCard, PermissionReply, Platform,
    PlatformCaps, ReplyHint, Result,
};

use crate::client::WebClient;
use crate::e2e::E2eIdentity;
use crate::models::{resolve_models, CliSource, ModelsContext};
use crate::titles::{TitleCandidate, TitleGen};
use crate::{
    encode_approval_request_frame, encode_approval_resolved_frame, encode_card_frame,
    encode_conv_workdir_frame, encode_error_frame, encode_history_frame, encode_text_frame,
    encode_workspaces_frame, history_entry_from_row, push_notify_title, session_title, CardPayload,
    GatewayCredential, InboundAction, PushEvent, PushNotifyKind, SessionEntryFrame,
    WorkspaceEntryFrame, TARGET,
};

const PLATFORM: &str = "web";

/// 出站 channel 容量（§5.1）：断线期 send_* 帧在此排队（client 重连后按序
/// 发出）；打满（relay 长时间不可达）→ send_* 返回 Err。
const OUTBOUND_CHANNEL_CAPACITY: usize = 256;
/// 入站 channel 容量（wecom 先例：dispatch 消费短暂抖动不丢消息）。
const INBOUND_CHANNEL_CAPACITY: usize = 64;
/// sync 请求 channel 容量。
const SYNC_CHANNEL_CAPACITY: usize = 8;
/// workspace 动作 channel 容量（v1.6）：SPA 点按级频率（保存/删除/切目录），
/// 16 足以吸收突发；打满由 client 侧 1s 有界背压丢弃 + SPA 重发兜底。
const WORKSPACE_CHANNEL_CAPACITY: usize = 16;
/// 卡片缓存 conv 上限（§5.4）：超限淘汰最旧（seq 最小），防长寿命进程内存
/// 无界增长。
const CARD_CACHE_MAX_CONVS: usize = 32;
/// v1.6 工作空间错误帧 code（帧契约定案字面量，SPA 据此分支渲染）。
const WORKSPACE_ERROR_CODE: &str = "workspace_invalid";
/// workspace_save 的 name 上限（帧契约：trim 后 1..=32 char）。
const WORKSPACE_NAME_MAX_CHARS: usize = 32;
/// v1.7 媒体上行：解码后大小上限（契约：≤8MB，超限 media_too_large）。
const MEDIA_MAX_BYTES: usize = 8 * 1024 * 1024;
/// v1.7 媒体上行：media.name 上限（契约：超长截断，按码点）。
const MEDIA_NAME_MAX_CHARS: usize = 64;
/// v1.7 媒体错误帧 code（帧契约定案字面量）：超 8MB 上限。
const MEDIA_TOO_LARGE_CODE: &str = "media_too_large";
/// v1.7 媒体错误帧 code：非 image/* mime / base64 畸形 / 落盘失败。
const MEDIA_INVALID_CODE: &str = "media_invalid";
/// v1.7 媒体动作 channel 容量：用户点按级频率（单条含 ≤8MB 解码 + 落盘，
/// 泵毫秒级吞吐），8 足以吸收突发；打满由 client 侧 1s 有界背压丢弃。
const MEDIA_CHANNEL_CAPACITY: usize = 8;
/// v1.7 workspaces 快照 sessions 段上限（契约：最近 50 条）。
const WEB_SESSIONS_MAX: usize = 50;
/// v1.8 history 动作 channel 容量：SPA 连接 ready + 每次 select_conv 各一条，
/// 点按级频率，8 足以吸收突发；打满由 client 侧 1s 有界背压丢弃（下次
/// 切会话/重连的 history_query 兜底）。
const HISTORY_CHANNEL_CAPACITY: usize = 8;
/// v1.10 push_notify 埋点事件 channel 容量（共享契约定案 8）：事件由回合
/// 终态/审批触发，用户回合级频率，8 足以吸收突发；打满丢帧 warn（通知丢
/// 一条无正确性影响——下一终态/审批事件天然重试，SPA 侧 tag=convId 亦防
/// 重）。这是 client select 直消费的「泵」入端，无需背压等待。
const PUSH_CHANNEL_CAPACITY: usize = 8;
/// v1.8 已落盘终态卡片 messageId 查重集容量上限（per conv）：防长寿命进程
/// 内存无界增长。契约允许同 messageId 重复行（SPA 幂等替换），超限清空
/// 整 conv 集合最坏多落一行，无正确性影响。
const LOGGED_CARDS_PER_CONV: usize = 100;

// ---------------------------------------------------------------------------
// 卡片缓存（D6 sync 重发）
// ---------------------------------------------------------------------------

/// per-conv 最近卡片缓存：`conv -> (seq, message_id, card)`。seq 单调递增，
/// 兼作淘汰序（最小者最旧）与快照序（旧→新重发）。纯内存临界区用 std
/// Mutex（锁内计算 → clone 出锁，relay Hub 同款纪律）。
#[derive(Default)]
struct CardCache {
    entries: HashMap<String, (u64, String, CardPayload)>,
    next_seq: u64,
}

impl CardCache {
    fn upsert(&mut self, conv: &str, message_id: &str, card: CardPayload) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries
            .insert(conv.to_string(), (seq, message_id.to_string(), card));
        if self.entries.len() > CARD_CACHE_MAX_CONVS {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (s, _, _))| *s)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest);
            }
        }
    }

    /// 全量快照（按 seq 旧→新排序，重发顺序与卡片创建顺序一致）。
    fn snapshot(&self) -> Vec<(String, String, CardPayload)> {
        let mut v: Vec<(u64, String, String, CardPayload)> = self
            .entries
            .iter()
            .map(|(k, (s, mid, c))| (*s, k.clone(), mid.clone(), c.clone()))
            .collect();
        v.sort_by_key(|(s, _, _, _)| *s);
        v.into_iter().map(|(_, k, mid, c)| (k, mid, c)).collect()
    }
}

/// pending 审批记录（cancel_all 逐条收敛用）：`conv -> request_id 列表`。
type PendingAsks = HashMap<String, Vec<String>>;

/// v1.8 已落盘终态卡片 messageId 查重集：`conv -> 已落盘 messageId`。
/// 契约语义「同 messageId 终态只落一次」（重复终态 patch / 同 id 复用跳过）；
/// HashSet 无序，超 [`LOGGED_CARDS_PER_CONV`] 上限时整 conv 清空重建
///（查重只是省一次重复落盘的优化——契约允许同 messageId 重复行，SPA
/// 幂等替换）。纯内存临界区用 std Mutex（lock_or_recover 同款纪律）。
#[derive(Default)]
struct LoggedCardIds(HashMap<String, HashSet<String>>);

impl LoggedCardIds {
    /// 登记一个待落盘的终态 messageId：返回 true = 首次（应落盘），
    /// false = 已登记过（跳过落盘）。
    fn register(&mut self, conv: &str, message_id: &str) -> bool {
        let set = self.0.entry(conv.to_string()).or_default();
        if set.contains(message_id) {
            return false;
        }
        if set.len() >= LOGGED_CARDS_PER_CONV {
            set.clear(); // 容量满：整 conv 清空（见类型注释的取舍）
        }
        set.insert(message_id.to_string());
        true
    }
}

/// 中毒（临界区 panic）时取回数据继续服务：纯内存临界区无 IO 副作用可回滚
/// （relay Hub / webhook 同款取舍）。
fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// sync responder：消费 client 转来的 sync 请求，把卡片缓存全量重发（card
/// 帧，state 依 terminal，v1.1 起带本机 machine 字段——与首发帧同形）。
/// best-effort——出站队列满即丢弃该轮重发并 warn（浏览器可再发 sync 刷新，
/// 不值得反压阻塞 client 的 sync channel）。
async fn serve_sync_requests(
    mut sync_rx: mpsc::Receiver<()>,
    machine_id: String,
    cards: Arc<Mutex<CardCache>>,
    outbound_tx: mpsc::Sender<String>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            req = sync_rx.recv() => {
                if req.is_none() {
                    break; // client 退出（platform drop 连动），无后续 sync
                }
                let snapshot = lock_or_recover(&cards).snapshot();
                debug!(target: TARGET, convs = snapshot.len(), "sync：重发缓存卡片");
                for (conv, message_id, card) in snapshot {
                    match encode_card_frame(&machine_id, "card", &conv, &message_id, &card) {
                        Ok(json) => {
                            if outbound_tx.try_send(json).is_err() {
                                warn!(target: TARGET, conv = %conv, "sync 重发出站队列满，丢弃该轮重发");
                                break;
                            }
                        }
                        Err(e) => warn!(target: TARGET, error = %e, conv = %conv, "sync 重发帧序列化失败"),
                    }
                }
            }
        }
    }
    debug!(target: TARGET, "sync responder 退出");
}

// ---------------------------------------------------------------------------
// workspace 泵（v1.6 工作空间：store KV 读写 + 校验 + 回帧）
// ---------------------------------------------------------------------------

/// workspace 泵主循环：消费 client 转来的工作空间动作（store API 是 async，
/// 在本泵内 await——不阻塞 client 收发循环与 dispatch）。`machine_id` 为
/// 本机身份（出站帧必带，v1.1 语义；与 serve_sync_requests 同款注入）。
/// `models` 为 v1.8 模型目录上下文（backend 句柄 + 官方回退列表）——每次
/// 组快照现读 CLI settings（低频操作可接受，不做缓存）。`backends` 为
/// v1.36 引擎名单（default + extras 保序去重，main 装配处注入的纯字符串
/// 表——快照 backends 段直传，无解析链）。`titles` 为 v1.9
/// 语义标题生成器（快照组装懒触发的共享防重复集，见 [`crate::titles`]）。
/// 平台 Drop 时经 shutdown token 连动退出。
#[allow(clippy::too_many_arguments)] // 参数已达 9——v1.36 注入 backends（先例 new_with_e2e）
async fn serve_workspace_actions(
    mut workspace_rx: mpsc::Receiver<InboundAction>,
    machine_id: String,
    store: Store,
    default_workdir: PathBuf,
    models: ModelsContext,
    backends: Vec<String>,
    titles: TitleGen,
    outbound_tx: mpsc::Sender<String>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            act = workspace_rx.recv() => {
                let Some(act) = act else {
                    break; // client 退出（platform drop 连动），无后续动作
                };
                // v1.8：每次组装现读 CLI/codex 配置（ccswitch 切供应商 / 改
                // ~/.codex/config.toml 后下一条快照即生效，无需重启/缓存失效）。
                // codex-app 批：按 backend 名分链（claude 系读 settings、
                // codex-app 读 codex config.toml）。
                let cli = crate::models::read_cli_for_backend(models.backend.name());
                handle_workspace_action(
                    &machine_id,
                    &store,
                    &default_workdir,
                    &models,
                    &backends,
                    &cli,
                    &titles,
                    act,
                    &outbound_tx,
                )
                .await;
            }
        }
    }
    debug!(target: TARGET, "workspace 泵退出");
}

/// 处理一条工作空间动作并回帧。规则（协议 v1.6 语义）：
/// - **每帧处理完（无论成败）都回一条 workspaces 全量快照**（SPA 单一数据
///   来源；失败时快照不变 + 前置 error 帧）。
/// - conv_workdir_set 成功时**额外**推一条 conv_workdir 增量帧。
/// - workspace_remove 未命中幂等（store delete 对不存在键即 Ok）。
#[allow(clippy::too_many_arguments)] // 参数已达 9——v1.36 注入 backends（先例 new_with_e2e）
async fn handle_workspace_action(
    machine: &str,
    store: &Store,
    default_workdir: &Path,
    models: &ModelsContext,
    backends: &[String],
    cli: &CliSource,
    titles: &TitleGen,
    act: InboundAction,
    outbound_tx: &mpsc::Sender<String>,
) {
    match act {
        InboundAction::WorkspaceList => {} // 直接回快照（收尾统一发）
        InboundAction::WorkspaceSave { name, path } => {
            if let Err(msg) = validate_workspace_save(&name, &path) {
                reply_workspace_error(machine, outbound_tx, &msg);
            } else {
                // 同名覆盖：写全新记录（lastUsedAt 缺席——刚保存未被用过）。
                let key = workspace_key(name.trim());
                let value = WorkspaceRecord::new(&path).encode();
                if let Err(e) = store.set_config(&key, &value).await {
                    reply_workspace_error(machine, outbound_tx, &format!("保存失败：{e}"));
                } else {
                    info!(target: TARGET, name = %name.trim(), "workspace_save：已保存工作空间");
                }
            }
        }
        InboundAction::WorkspaceRemove { name } => {
            // 未命中幂等：delete_config 对不存在键返回 Ok，不报错。
            if let Err(e) = store.delete_config(&workspace_key(&name)).await {
                reply_workspace_error(machine, outbound_tx, &format!("删除失败：{e}"));
            }
        }
        InboundAction::ConvWorkdirSet { conv, path } => {
            if let Err(msg) = validate_workdir_str(&path) {
                reply_workspace_error(machine, outbound_tx, &msg);
            } else {
                // 与 /cd 同键（workdir:<conv>）——终端与网页互通的关键。
                match store.set_config(&workdir_key(&conv), &path).await {
                    Err(e) => {
                        reply_workspace_error(machine, outbound_tx, &format!("保存失败：{e}"))
                    }
                    Ok(()) => {
                        bump_workspace_last_used(store, &path).await;
                        // 额外增量帧（先增量后快照——快照是 SPA 单一来源，放最后）。
                        match encode_conv_workdir_frame(machine, &conv, &path) {
                            Ok(json) => enqueue_best_effort(outbound_tx, json),
                            Err(e) => warn!(
                                target: TARGET,
                                conv = %conv,
                                error = %e,
                                "conv_workdir 增量帧序列化失败（快照兜底）"
                            ),
                        }
                    }
                }
            }
        }
        // client 只转发上述四个变体进 workspace channel，其余分支不可达
        //（防御性忽略，不 panic——通道元素类型复用 InboundAction）。
        other => {
            debug!(target: TARGET, act = ?other, "workspace 泵收到非工作空间动作，忽略")
        }
    }
    // 每帧收尾：全量快照（无论成败）。
    match build_workspaces_frame(
        machine,
        store,
        default_workdir,
        models,
        backends,
        cli,
        titles,
    )
    .await
    {
        Ok(json) => enqueue_best_effort(outbound_tx, json),
        Err(e) => warn!(target: TARGET, error = %e, "workspaces 快照帧序列化失败"),
    }
}

/// workspaces 全量快照组帧：default = 配置 default_workdir；workspaces =
/// 全部命名空间（name 稳定序）；convs = 全部已知 conv 的当前 workdir
///（含 local: 会话）；sessions = `web:` 会话历史段（v1.7，见
/// [`collect_session_entries`]）；models = 模型目录段（**按 backend 分链**：
/// claude 链 = CLI 映射优先 + [`ModelsContext::official`].claude 回退；codex-app
/// 链 = 实时 list_models（[`ModelsContext::list_models`]，经 `spawn_blocking`
/// 调——实现侧自 spawn 临时 app-server，秒级阻塞不占 reactor worker，panic/
/// 失败回空静默落下层）→ codex config catalog → official.codex。current =
/// backend.model()——/model 命令热设值；空 options 组帧出口整段缺席）。
/// best-effort：单段 list 失败 → warn + 该段空，快照照发（SPA 单一数据来源，
/// 宁可缺段不可断流）。
async fn build_workspaces_frame(
    machine: &str,
    store: &Store,
    default_workdir: &Path,
    models: &ModelsContext,
    backends: &[String],
    cli: &CliSource,
    titles: &TitleGen,
) -> Result<String> {
    let mut entries: Vec<WorkspaceEntryFrame> = Vec::new();
    match store.list_config(WORKSPACE_PREFIX).await {
        Ok(rows) => {
            for (k, v) in rows {
                let Some(name) = k.strip_prefix(WORKSPACE_PREFIX) else {
                    continue;
                };
                // 旧值（裸路径）与 v1.6 JSON 值统一 decode；损坏值跳过。
                if let Some(rec) = WorkspaceRecord::decode(&v) {
                    entries.push(WorkspaceEntryFrame {
                        name: name.to_string(),
                        path: rec.path,
                        last_used_at: rec.last_used_at,
                    });
                }
            }
        }
        Err(e) => warn!(target: TARGET, error = %e, "workspaces 快照：列出工作空间失败（段空）"),
    }
    let mut convs: BTreeMap<String, String> = BTreeMap::new();
    match store.list_config(WORKDIR_PREFIX).await {
        Ok(rows) => {
            for (k, v) in rows {
                if let Some(conv) = k.strip_prefix(WORKDIR_PREFIX) {
                    // workdir KV 值恒为裸路径（/cd 与本泵同形态写入）。
                    convs.insert(conv.to_string(), v);
                }
            }
        }
        Err(e) => {
            warn!(target: TARGET, error = %e, "workspaces 快照：列出 conv workdir 失败（段空）")
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name)); // 契约：按 name 稳定序
    let sessions = collect_session_entries(store, titles).await;
    // models 段：按 backend 分链汇聚（调用方现读注入，见
    // serve_workspace_actions）+ backend.model()（/model 热设值）；空 options
    // 整段缺席。
    let models_frame = match cli {
        // claude 链（含 exec codex 行为冻结：旧单列表消费者，不动）。
        CliSource::Claude(c) => resolve_models(c, &models.official.claude, models.backend.model()),
        // codex-app 链：实时 list_models（spawn_blocking 隔离秒级 app-server
        // 往返）→ codex config catalog → official.codex；实时空/panic 回空
        // 静默落下层。
        CliSource::Codex(c) => {
            let live = match &models.list_models {
                Some(f) => {
                    let f = f.clone();
                    tokio::task::spawn_blocking(move || f())
                        .await
                        .unwrap_or_default()
                }
                None => Vec::new(),
            };
            crate::models::resolve_codex_models(
                &live,
                c,
                &models.official.codex,
                models.backend.model(),
            )
        }
    };
    encode_workspaces_frame(
        machine,
        &default_workdir.to_string_lossy(),
        entries,
        convs,
        sessions,
        Some(models_frame),
        // v1.36 backends 段：引擎名单直传（无 IO、无解析链——注册表在进程
        // 生命周期内静态）；current = 首位 default。空名单 → None（整段
        // 缺席；encode 出口再兜底一次空 options，同 models 段模式）。
        (!backends.is_empty()).then(|| crate::BackendsFrame {
            current: backends[0].clone(),
            options: backends.to_vec(),
        }),
    )
}

/// v1.7 sessions 段数据源（契约 B）+ v1.9 语义标题懒触发点：store sessions
/// 表 `web:` 前缀行，按 updated_at 降序，上限 [`WEB_SESSIONS_MAX`]（50）。
/// title 三档：SessionRow.name（手动名）> sessions.title 语义标题（v1.9，
/// 独列单查——core 共享的 SessionRow 形状不扩散本列）> 该 conv 最近一条
/// session_history 的 first_prompt 摘要（主表无此列，无语义标题时补查
/// `list_session_history(conv, 1)`）。store 无「跨 conv 列 sessions」现成
/// API：`list_session_convs` 全枚举 + 逐 conv `get_session` 后排序截 50。
/// best-effort：读失败 → warn + 跳过（或整段空），快照其余部分照发。
///
/// v1.9 懒触发：组装中对「无手动名、无语义 title 且 first_prompt 非空」的
/// conv 记为候选（快照序新→旧），收尾交 [`TitleGen::spawn_lazy`]——每快照
/// 至多 3 个**最旧**候选 spawn headless CLI 生成，成功写库后**下一条**快照
/// 自然带出新 title（SPA 打开面板/切会话都会拉快照，多次快照逐批收敛）。
async fn collect_session_entries(store: &Store, titles: &TitleGen) -> Vec<SessionEntryFrame> {
    let convs = match store.list_session_convs().await {
        Ok(convs) => convs,
        Err(e) => {
            warn!(target: TARGET, error = %e, "workspaces 快照：列出会话失败（sessions 段空）");
            return Vec::new();
        }
    };
    let mut rows = Vec::new();
    for conv in convs {
        if !conv.starts_with("web:") {
            continue; // 仅本平台会话（ilink:/local: 等不进 SPA 驾驶舱列表）
        }
        match store.get_session(&conv).await {
            Ok(Some(row)) => rows.push(row),
            // list 与 get 之间被 /new 删除（或副表轮转），跳过。
            Ok(None) => {}
            Err(e) => warn!(target: TARGET, conv = %conv, error = %e, "读取会话行失败（跳过）"),
        }
    }
    // updated_at 降序；同秒并列按 conv_id 升序钉稳定序（帧字节可复现）。
    rows.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.conv_id.cmp(&b.conv_id))
    });
    rows.truncate(WEB_SESSIONS_MAX);
    let mut entries = Vec::with_capacity(rows.len());
    let mut candidates: Vec<TitleCandidate> = Vec::new();
    for row in rows {
        let has_manual_name = row.name.as_deref().is_some_and(|n| !n.trim().is_empty());
        // 语义标题独立单查（手动名会话显示不走此档，免查）。
        let semantic_title = if has_manual_name {
            None
        } else {
            match store.get_session_title(&row.conv_id).await {
                Ok(t) => t,
                Err(e) => {
                    warn!(target: TARGET, conv = %row.conv_id, error = %e, "读取语义标题失败（视为无）");
                    None
                }
            }
        };
        // first_prompt 摘要：仅当手动名与语义标题皆无才查（显示兜底 + 生成种子）。
        let first_prompt = if has_manual_name || semantic_title.is_some() {
            None
        } else {
            match store.list_session_history(&row.conv_id, 1).await {
                Ok(h) => h.into_iter().next().and_then(|r| r.first_prompt),
                Err(e) => {
                    warn!(
                        target: TARGET,
                        conv = %row.conv_id,
                        error = %e,
                        "读取会话 first_prompt 失败（title 回退空串）"
                    );
                    None
                }
            }
        };
        // v1.9 懒触发候选：无语义 title 且 first_prompt 非空（手动名会话
        // first_prompt 恒 None，天然排除——语义标题对显示无增益）。
        if semantic_title.is_none() {
            if let Some(prompt) = first_prompt.as_deref().filter(|p| !p.trim().is_empty()) {
                candidates.push(TitleCandidate {
                    conv: row.conv_id.clone(),
                    first_prompt: prompt.to_string(),
                    workdir: PathBuf::from(&row.workdir),
                });
            }
        }
        entries.push(SessionEntryFrame {
            id: row.conv_id.clone(),
            title: session_title(
                row.name.as_deref(),
                semantic_title.as_deref(),
                first_prompt.as_deref(),
            ),
            updated_at: u64::try_from(row.updated_at).unwrap_or(0),
        });
    }
    titles.spawn_lazy(&candidates);
    entries
}

/// conv_workdir_set 的 path 校验链（与 /cd 同链）：绝对路径 + is_dir +
/// [`validate_workdir`] 过宽目录拒绝——三者由 validate_workdir 一体覆盖
///（其内部先验绝对/存在再黑名单），错误文案即人话中文。
fn validate_workdir_str(path: &str) -> std::result::Result<(), String> {
    validate_workdir(Path::new(path))
}

/// workspace_save 校验：name trim 后 1..=32 char 非空；path 走
/// [`validate_workdir_str`] 同链。Err(中文原因) → error 帧 message。
fn validate_workspace_save(name: &str, path: &str) -> std::result::Result<(), String> {
    let chars = name.trim().chars().count();
    if chars == 0 {
        return Err("名称不能为空（trim 后 1~32 字符）".into());
    }
    if chars > WORKSPACE_NAME_MAX_CHARS {
        return Err(format!(
            "名称最长 {WORKSPACE_NAME_MAX_CHARS} 字符（当前 {chars}）"
        ));
    }
    validate_workdir_str(path)
}

/// conv_workdir_set 命中某空间 path 时 bump 其 lastUsedAt（best-effort：
/// 读写失败仅 warn——时间戳只影响 SPA「最近用过」排序，不影响功能）。
async fn bump_workspace_last_used(store: &Store, path: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let rows = match store.list_config(WORKSPACE_PREFIX).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(target: TARGET, error = %e, "bump lastUsedAt：列出工作空间失败（跳过）");
            return;
        }
    };
    for (k, v) in rows {
        let Some(name) = k.strip_prefix(WORKSPACE_PREFIX) else {
            continue;
        };
        let Some(rec) = WorkspaceRecord::decode(&v) else {
            continue;
        };
        if rec.path == path {
            let bumped = WorkspaceRecord {
                last_used_at: Some(now),
                ..rec
            };
            if let Err(e) = store
                .set_config(&workspace_key(name), &bumped.encode())
                .await
            {
                warn!(target: TARGET, name, error = %e, "bump lastUsedAt 写回失败（忽略）");
            }
        }
    }
}

/// 出站帧 best-effort 入队（workspace 路径专用：队列满/通道关闭丢帧 warn——
/// SPA 重发或下条消息触发的快照兜底，不反压泵）。
fn enqueue_best_effort(outbound_tx: &mpsc::Sender<String>, json: String) {
    if let Err(e) = outbound_tx.try_send(json) {
        warn!(target: TARGET, error = %e, "工作空间出站帧入队失败（队列满/通道关闭），丢弃");
    }
}

/// 工作空间 error 帧（code=workspace_invalid，复用既有 error 帧形态；
/// machine 为本机身份，v1.1 出站帧必带）。
fn reply_workspace_error(machine: &str, outbound_tx: &mpsc::Sender<String>, message: &str) {
    match encode_error_frame(machine, WORKSPACE_ERROR_CODE, message) {
        Ok(json) => enqueue_best_effort(outbound_tx, json),
        Err(e) => warn!(target: TARGET, error = %e, "workspace error 帧序列化失败"),
    }
}

// ---------------------------------------------------------------------------
// history 泵（v1.8 会话历史重放：history_query → outbound_log 增量拉取回帧）
// ---------------------------------------------------------------------------

/// history 泵主循环：消费 client 转来的 history_query 动作（store IO 在本泵
/// 内 await，不阻塞 client 收发循环；workspace 泵同款纪律）。平台 Drop 时
/// 经 shutdown token 连动退出。回帧经 outbound_tx（client 出站路径——E2E
/// 会话活跃时同被 enc 包装，中继保持盲化）。
async fn serve_history_queries(
    mut history_rx: mpsc::Receiver<InboundAction>,
    machine_id: String,
    store: Store,
    outbound_tx: mpsc::Sender<String>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            act = history_rx.recv() => {
                let Some(act) = act else {
                    break; // client 退出（platform drop 连动），无后续查询
                };
                match act {
                    InboundAction::HistoryQuery { conv, since } => {
                        handle_history_query(&machine_id, &store, &conv, since, &outbound_tx).await;
                    }
                    // client 只转发 HistoryQuery 进 history channel，其余分支
                    // 不可达（防御性忽略，不 panic——通道元素类型复用
                    // InboundAction）。
                    other => {
                        debug!(target: TARGET, act = ?other, "history 泵收到非 history_query 动作，忽略")
                    }
                }
            }
        }
    }
    debug!(target: TARGET, "history 泵退出");
}

/// 处理一条 history_query 并回 history 帧（契约 v1.8 语义）：
/// - `seq > since` 的行 → entries（升序，≤[`OUTBOUND_RETAIN`] 条；超限截
///   最近 N 条 + trimmed=true）；行 payload 畸形单条跳过（best-effort）。
/// - lastSeq = 该 conv 当前最大 seq（无任何记录 0——entries 空时浏览器只
///   更新水位）。
/// - 查库/组帧失败仅 warn 不回帧（SPA 下次切会话/重连的 history_query 兜底，
///   契约未定义 history 错误帧，不复用 workspace_invalid）。
async fn handle_history_query(
    machine: &str,
    store: &Store,
    conv: &str,
    since: i64,
    outbound_tx: &mpsc::Sender<String>,
) {
    let OutboundPage { rows, trimmed } = match store
        .list_outbound_since(conv, since, OUTBOUND_RETAIN)
        .await
    {
        Ok(page) => page,
        Err(e) => {
            warn!(target: TARGET, conv = %conv, since, error = %e, "history_query 查库失败，不回帧");
            return;
        }
    };
    let last_seq = match store.max_outbound_seq(conv).await {
        Ok(n) => n,
        Err(e) => {
            warn!(target: TARGET, conv = %conv, error = %e, "history_query 查 lastSeq 失败，不回帧");
            return;
        }
    };
    let entries: Vec<serde_json::Value> = rows.iter().filter_map(history_entry_from_row).collect();
    debug!(
        target: TARGET,
        conv = %conv,
        since,
        entries = entries.len(),
        last_seq,
        trimmed,
        "history 回放"
    );
    match encode_history_frame(
        machine,
        conv,
        serde_json::Value::Array(entries),
        last_seq,
        trimmed,
    ) {
        Ok(json) => enqueue_best_effort(outbound_tx, json),
        Err(e) => warn!(target: TARGET, conv = %conv, error = %e, "history 帧序列化失败"),
    }
}

/// unix 毫秒（v1.8 出站落盘 payload.at / outbound_log.ts 粒度契约）。
fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 媒体泵（v1.7 媒体上行：message 帧 media 段——校验 + 落盘 + 合成投递）
// ---------------------------------------------------------------------------

/// 媒体泵主循环：消费 client 转来的 MediaMessage 动作。文件系统 IO（b64
/// 解码 + 写盘）在本泵内 await——不阻塞 client 收发循环；平台 Drop 时经
/// shutdown token 连动退出。`media_dir` 生产为 `<imagent_home>/media`（与
/// ilink 入站媒体同目录，7 天 GC 同源兜底），测试注入临时目录。
async fn serve_media_messages(
    mut media_rx: mpsc::Receiver<InboundAction>,
    machine_id: String,
    media_dir: PathBuf,
    inbound_tx: mpsc::Sender<InboundMessage>,
    outbound_tx: mpsc::Sender<String>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            act = media_rx.recv() => {
                let Some(act) = act else {
                    break; // client 退出（platform drop 连动），无后续动作
                };
                process_media_message(&machine_id, &media_dir, act, &inbound_tx, &outbound_tx).await;
            }
        }
    }
    debug!(target: TARGET, "媒体泵退出");
}

/// 处理一条媒体消息（契约 A 全链）：b64 解码 → 校验（mime 须 image/*、解码
/// ≤8MB）→ 落盘（路径模板与安全化见 [`persist_up_media`]）→ 合成
/// InboundMessage（text 非空才 Some，media 引用落盘路径）投 inbound channel。
/// 任一步失败：回 error 帧（media_too_large / media_invalid）且**不投递消息**
/// （半途写坏的文件留给媒体目录 7 天 GC）。已知取舍：媒体消息经本泵多一跳，
/// 与随后到达的纯文本消息可能乱序——dispatch 按 conv 队列消费，影响仅为
/// 用户主动「先图后文」的展示序，不破坏正确性。
async fn process_media_message(
    machine: &str,
    media_dir: &Path,
    act: InboundAction,
    inbound_tx: &mpsc::Sender<InboundMessage>,
    outbound_tx: &mpsc::Sender<String>,
) {
    let InboundAction::MediaMessage { conv, text, media } = act else {
        debug!(target: TARGET, "媒体泵收到非媒体动作，忽略");
        return;
    };
    // mime 白名单：本批仅图片（契约：其它类型拒绝）。
    if !media.mime.starts_with("image/") {
        reply_media_error(
            machine,
            outbound_tx,
            MEDIA_INVALID_CODE,
            &format!("仅支持图片（image/*），收到 {}", media.mime),
        );
        return;
    }
    // b64 解码（契约：单行无换行标准 base64，畸形即拒）。
    let bytes = {
        use base64::Engine as _;
        match base64::engine::general_purpose::STANDARD.decode(media.b64.as_bytes()) {
            Ok(b) => b,
            Err(e) => {
                reply_media_error(
                    machine,
                    outbound_tx,
                    MEDIA_INVALID_CODE,
                    &format!("base64 解码失败：{e}"),
                );
                return;
            }
        }
    };
    if bytes.len() > MEDIA_MAX_BYTES {
        // message 为契约原文（SPA 据此提示预压缩规避）。
        reply_media_error(
            machine,
            outbound_tx,
            MEDIA_TOO_LARGE_CODE,
            "图片超过 8MB 上限",
        );
        return;
    }
    let path = match persist_up_media(media_dir, &media.name, &bytes) {
        Ok(p) => p,
        Err(e) => {
            reply_media_error(
                machine,
                outbound_tx,
                MEDIA_INVALID_CODE,
                &format!("媒体落盘失败：{e}"),
            );
            return;
        }
    };
    let msg = crate::synth_media_message(
        conv.clone(),
        text,
        vec![MediaRef {
            // MediaRef 无独立 mime/name 字段（core 契约 kind+url）：kind 载
            // mime（media_hint_for 展示），name 已并入落盘文件名。
            kind: media.mime.clone(),
            url: path.to_string_lossy().into_owned(),
        }],
    );
    info!(
        target: TARGET,
        conv = %conv,
        path = %path.display(),
        bytes = bytes.len(),
        "媒体上行：已落盘并投递"
    );
    // 与 client 直投同款 1s 有界背压：dispatch 消费抖动不丢消息，持续满才丢弃。
    match tokio::time::timeout(Duration::from_secs(1), inbound_tx.send(msg)).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => warn!(target: TARGET, "入站 channel 已关闭（platform 退出），丢弃媒体消息"),
        Err(_) => warn!(target: TARGET, "入站 channel 持续满 1s，丢弃媒体消息"),
    }
}

/// 媒体 error 帧（复用既有 error 帧形态；code=media_too_large|media_invalid，
/// machine 为本机身份，v1.1 出站帧必带）。
fn reply_media_error(machine: &str, outbound_tx: &mpsc::Sender<String>, code: &str, message: &str) {
    match encode_error_frame(machine, code, message) {
        Ok(json) => enqueue_best_effort(outbound_tx, json),
        Err(e) => warn!(target: TARGET, error = %e, "媒体 error 帧序列化失败"),
    }
}

/// media.name 安全化（契约：仅保留 [A-Za-z0-9._-]，其余替换 `_`；≤64 字符
/// 超长截断——按码点，非 ASCII 1 码点 → 1 个 `_`）。路径分隔符 `/` 同被
/// 替换，穿越片段（`../`）退化为无害平文件名。纯函数便于单测。
fn sanitize_media_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(MEDIA_NAME_MAX_CHARS)
        .collect()
}

/// 媒体落盘（契约路径模板 `<media_dir>/up-<unix_ms>-<安全化name>`）：目录
/// 不存在则创建（0755）；文件 0600（OpenOptions 带 mode 原子创建——无
/// 「先 write 后 chmod」的 umask 暴露窗口，ilink persist_media 同款纪律）。
/// 同毫秒同名重复上传按 truncate 覆盖（模板契约固定，不追加去重后缀）。
fn persist_up_media(
    media_dir: &Path,
    name: &str,
    bytes: &[u8],
) -> std::result::Result<PathBuf, String> {
    let build_dir = || -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut b = std::fs::DirBuilder::new();
            // recursive：父目录（<imagent_home>）缺失时一并创建；已存在不报错
            //（mode 只作用于新建目录——不动 ilink 既有目录的权限）。
            b.recursive(true).mode(0o755);
            b.create(media_dir)
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(media_dir)
        }
    };
    build_dir().map_err(|e| format!("创建媒体目录 {} 失败：{e}", media_dir.display()))?;

    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = media_dir.join(format!("up-{unix_ms}-{}", sanitize_media_name(name)));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("写媒体文件 {} 失败：{e}", path.display()))?;
        f.write_all(bytes)
            .map_err(|e| format!("写媒体文件 {} 失败：{e}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, bytes)
            .map_err(|e| format!("写媒体文件 {} 失败：{e}", path.display()))?;
    }
    Ok(path)
}

// ---------------------------------------------------------------------------
// WebPlatform
// ---------------------------------------------------------------------------

/// Web 平台适配器（网关侧薄壳：两条 channel + 后台 client / sync responder）。
pub struct WebPlatform {
    /// relay 地址（doctor_probes 展示用）。
    relay_url: String,
    /// 本机 machine 身份（v1.1 多终端管理）：hello 自报 + 全部出站帧携带。
    machine_id: String,
    /// 出站帧通道（发给 client → relay → 浏览器）。
    outbound_tx: mpsc::Sender<String>,
    /// v1.10 push_notify 埋点事件通道（发给 client → relay，明文管理帧族；
    /// client select 直消费——不经 outbound 的 E2E seal 路径）。
    push_tx: mpsc::Sender<PushEvent>,
    /// `/reconnect` 强制重连信号（与 client run task 共享）。
    reconnect: Arc<Notify>,
    /// 入站消息 channel 前端（`recv` 直接 await；AsyncMutex 跨 await 持锁）。
    inbound_rx: Arc<AsyncMutex<mpsc::Receiver<InboundMessage>>>,
    /// 在线标志（client 维护）——doctor_probes / 测试用。
    online: Arc<AtomicBool>,
    /// per-conv 最近卡片缓存（sync 重发）。
    cards: Arc<Mutex<CardCache>>,
    /// v1.8 出站落盘句柄（text / card 终态帧异步写 outbound_log；与
    /// workspace / history 泵共享同一 store 连接）。
    store: Store,
    /// v1.8 已落盘终态卡片 messageId 查重集（同 messageId 终态只落一次）。
    logged_cards: Arc<Mutex<LoggedCardIds>>,
    /// 合成卡片 id 计数器：`web-card-<n>` 单调递增（§4.2）。
    card_seq: Arc<AtomicU64>,
    /// pending 审批记录（cancel_all 收敛用）。
    pending_asks: Arc<Mutex<PendingAsks>>,
    /// 停机信号（Drop 时 cancel，client / sync responder 连动退出）。
    shutdown: CancellationToken,
}

impl WebPlatform {
    /// 构造并后台 spawn client 任务（建连/认证/心跳/重连/收发）与 sync
    /// responder / workspace 泵任务。`relay_url` 形如 `wss://…/imagent/gateway`；
    /// `credential` 为网关认证凭证（v1.3：key 优先 token 回退，main 经
    /// [`crate::key::load_gateway_credential`] 装配——密钥文件
    /// `<imagent_home>/web-gateway-key` 缺省自动生成；`IMAGENT_WEB_GATEWAY_TOKEN`
    /// 为兼容回退）；`machine_id` 为本机 machine 身份（v1.1，main 经
    /// [`crate::resolve_machine_id`] 解析：config 显式值 / hostname 短名）；
    /// `default_session` 为无 convId 浏览器消息的落点（缺省 "default"）；
    /// `store` / `default_workdir` 为 v1.6 工作空间泵所需（store KV 句柄 +
    /// 快照 default 字段来源，main 装配处注入）；`models` 为 v1.8 模型目录
    /// 上下文（backend 句柄 + config official_models，main 装配处传现有
    /// backend clone——快照 models.current 取 backend.model()；测试传 mock）；
    /// `backends` 为 v1.36 引擎名单（default 在首位 + extras 保序去重的纯
    /// 名单，main 装配处随 ModelsContext 一并注入——快照 backends 段数据源，
    /// 空名单整段缺席；测试传 `Vec::new()`）。
    ///
    /// E2E 密钥（v1.4）锚定 `<imagent_home>/web-e2e-key`（main 不感知——
    /// E2E 是内容保密增强，装配失败不阻断网关启动，仅降级明文 + warn；
    /// 测试经 [`Self::new_with_e2e`] 注入临时路径）。
    #[allow(clippy::too_many_arguments)] // 参数已达 8——v1.36 注入 backends（先例 new_with_e2e）
    pub fn new(
        relay_url: String,
        credential: GatewayCredential,
        default_session: String,
        machine_id: String,
        store: Store,
        default_workdir: PathBuf,
        models: ModelsContext,
        backends: Vec<String>,
    ) -> Self {
        let e2e_key = imagent_core::paths::imagent_home().join("web-e2e-key");
        Self::new_with_e2e(
            relay_url,
            credential,
            default_session,
            machine_id,
            &e2e_key,
            store,
            default_workdir,
            models,
            backends,
        )
    }

    /// [`Self::new`] 的全参形态（E2E 密钥路径可注入——集成测试用临时目录，
    /// 不落真实 `~/.imagent`）。
    #[allow(clippy::too_many_arguments)] // 参数已达 9——v1.36 注入 backends（历史演化提醒）
    pub fn new_with_e2e(
        relay_url: String,
        credential: GatewayCredential,
        default_session: String,
        machine_id: String,
        e2e_key_path: &std::path::Path,
        store: Store,
        default_workdir: PathBuf,
        models: ModelsContext,
        backends: Vec<String>,
    ) -> Self {
        let (inbound_tx, inbound_rx) = mpsc::channel::<InboundMessage>(INBOUND_CHANNEL_CAPACITY);
        let (outbound_tx, outbound_rx) = mpsc::channel::<String>(OUTBOUND_CHANNEL_CAPACITY);
        let (sync_tx, sync_rx) = mpsc::channel::<()>(SYNC_CHANNEL_CAPACITY);
        let (workspace_tx, workspace_rx) =
            mpsc::channel::<InboundAction>(WORKSPACE_CHANNEL_CAPACITY);
        let (media_tx, media_rx) = mpsc::channel::<InboundAction>(MEDIA_CHANNEL_CAPACITY);
        let (history_tx, history_rx) = mpsc::channel::<InboundAction>(HISTORY_CHANNEL_CAPACITY);
        let (push_tx, push_rx) = mpsc::channel::<PushEvent>(PUSH_CHANNEL_CAPACITY);

        // E2E 身份装配（v1.4）：e2e_ack 签名复用 v1.3 认证 ed25519 私钥——
        // 仅 key 凭证形态可用；token 回退形态无签名密钥，E2E 不可用（出站
        // 恒明文 = v1.3 行为，浏览器侧 10s fail-closed 自行提示明文选择）。
        // 密钥文件损坏同理降级不阻断（认证不受影响——两把密钥独立）。
        let e2e = match &credential {
            GatewayCredential::Key(sk) => {
                match E2eIdentity::load_or_generate(e2e_key_path, sk.clone()) {
                    Ok(id) => Some(id),
                    Err(e) => {
                        warn!(
                            target: TARGET,
                            key_path = %e2e_key_path.display(),
                            error = %e,
                            "E2E 密钥装配失败，业务帧保持明文中继（网关认证不受影响）"
                        );
                        None
                    }
                }
            }
            GatewayCredential::Token(_) => {
                debug!(
                    target: TARGET,
                    "token 凭证（v1.2 兼容路径）无 ed25519 签名密钥，E2E 加密不可用（明文）"
                );
                None
            }
        };

        let reconnect = Arc::new(Notify::new());
        let online = Arc::new(AtomicBool::new(false));
        let shutdown = CancellationToken::new();

        // client 任务：连接立即开始（断线自动重连）。
        let client = WebClient {
            relay_url: relay_url.clone(),
            credential,
            machine_id: machine_id.clone(),
            default_session,
            online: online.clone(),
            e2e,
            inbound_tx: inbound_tx.clone(),
            sync_tx,
            workspace_tx,
            media_tx,
            history_tx,
        };
        tokio::spawn(client.run(outbound_rx, push_rx, reconnect.clone(), shutdown.clone()));

        // sync responder（D6 会话恢复）。
        let cards: Arc<Mutex<CardCache>> = Arc::new(Mutex::new(CardCache::default()));
        tokio::spawn(serve_sync_requests(
            sync_rx,
            machine_id.clone(),
            cards.clone(),
            outbound_tx.clone(),
            shutdown.clone(),
        ));

        // workspace 泵（v1.6 工作空间 + v1.8 models 段 + v1.9 语义标题懒触发 +
        // v1.36 backends 段；出站帧带 machine，v1.1 语义）。
        let titles = TitleGen::new(store.clone());
        tokio::spawn(serve_workspace_actions(
            workspace_rx,
            machine_id.clone(),
            store.clone(),
            default_workdir,
            models,
            backends,
            titles,
            outbound_tx.clone(),
            shutdown.clone(),
        ));

        // history 泵（v1.8 会话历史重放）：history_query → outbound_log 增量
        // 拉取 → history 帧回放（出站帧带 machine，v1.1 语义）。
        tokio::spawn(serve_history_queries(
            history_rx,
            machine_id.clone(),
            store.clone(),
            outbound_tx.clone(),
            shutdown.clone(),
        ));

        // 媒体泵（v1.7 上行图片落盘；落盘目录 <imagent_home>/media 与 ilink
        // 入站媒体共享，7 天 GC 同源兜底。error 帧带 machine，v1.1 语义）。
        let media_dir = imagent_core::paths::imagent_home().join("media");
        tokio::spawn(serve_media_messages(
            media_rx,
            machine_id.clone(),
            media_dir,
            inbound_tx,
            outbound_tx.clone(),
            shutdown.clone(),
        ));

        Self {
            relay_url,
            machine_id,
            outbound_tx,
            push_tx,
            reconnect,
            inbound_rx: Arc::new(AsyncMutex::new(inbound_rx)),
            online,
            cards,
            store,
            logged_cards: Arc::new(Mutex::new(LoggedCardIds::default())),
            card_seq: Arc::new(AtomicU64::new(0)),
            pending_asks: Arc::new(Mutex::new(PendingAsks::default())),
            shutdown,
        }
    }

    /// relay 连接是否在线（doctor_probes / 测试用）。
    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// 本机 machine 身份（测试用）。
    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    /// 出站帧入队（§5.1 断线语义）：try_send——断线期帧排队（Ok），队列满 /
    /// client 退出才 Err，不挂起 dispatch。
    fn enqueue(&self, json: String) -> Result<()> {
        match self.outbound_tx.try_send(json) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(CoreError::Platform(
                PLATFORM,
                "出站队列满：relay 长时间不可达".into(),
            )),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(CoreError::Platform(
                PLATFORM,
                "出站通道已关闭（client 已退出）".into(),
            )),
        }
    }

    /// 登记一条 pending 审批（cancel_all 逐条收敛用）。
    fn track_ask(&self, conv: &str, request_id: &str) {
        lock_or_recover(&self.pending_asks)
            .entry(conv.to_string())
            .or_default()
            .push(request_id.to_string());
    }

    /// 移除 pending 记录（已 resolve / 已 cancel）。
    fn untrack_ask(&self, conv: &str, request_id: &str) {
        let mut map = lock_or_recover(&self.pending_asks);
        if let Some(list) = map.get_mut(conv) {
            list.retain(|r| r != request_id);
            if list.is_empty() {
                map.remove(conv);
            }
        }
    }

    // —— v1.8 出站落盘（outbound_log）：send 成功后异步写库，不阻塞热路径 ——

    /// text 帧出站成功后落盘（契约：kind='text'，payload=`{"text":<原文>,
    /// "at":<unix_ms>}`——落盘取 encode 截断前的原文；`at` 由调用方传入，
    /// 与实时帧同源同值，不再单独取时刻——v1.36 起 SPA 靠它跨帧去重）。
    /// spawn 短任务写库：出站队列是 try_send 语义，落盘绝不在 dispatch
    /// 热路径上 await。
    fn log_outbound_text(&self, conv: &str, text: &str, at: u64) {
        let payload = serde_json::json!({"text": text, "at": at}).to_string();
        self.spawn_outbound_log(conv.to_string(), "text", payload);
    }

    /// card 终态帧（state=="done"，含 done/error 两终态）出站成功后收口
    ///（v1.8 落盘 + v1.10 push_notify 埋点，**同一查重闸**——同 messageId
    /// 终态只落一次也只推一次；sync 重发不经本路径）。查重登记在 spawn 前
    /// 同步完成——两条同 id 终态连发不会都过闸。
    fn on_terminal_card(&self, conv: &str, message_id: &str, card: &CardPayload) {
        if !lock_or_recover(&self.logged_cards).register(conv, message_id) {
            return;
        }
        // v1.10 push_notify：终态 → round_done / round_error；title 取
        // task_digest 优先、正文兜底（≤32 码点摘要，契约帧注）。
        let kind = if card.terminal == "error" {
            PushNotifyKind::RoundError
        } else {
            PushNotifyKind::RoundDone
        };
        let title = push_notify_title(card.task_digest.as_deref().unwrap_or(&card.text));
        self.spawn_push_notify(kind, conv, title);
        // v1.8 落盘主体（payload=encode 截断前的完整卡片 JSON）。
        let card_json = match serde_json::to_value(card) {
            Ok(v) => v,
            Err(e) => {
                // CardPayload 纯数据字段，序列化不可失败；兜底不阻断。
                warn!(target: TARGET, error = %e, "card payload 序列化失败，跳过落盘");
                return;
            }
        };
        let payload = serde_json::json!({"messageId": message_id, "card": card_json}).to_string();
        self.spawn_outbound_log(conv.to_string(), "card", payload);
    }

    /// v1.10 push_notify 埋点出站（spawn 短任务——对齐 spawn_outbound_log
    /// 纪律：dispatch 热路径零阻塞、绝不在通知上等待；try_send 打满即丢帧
    /// warn 不重试——丢一条通知无正确性影响，下一事件天然重试）。
    fn spawn_push_notify(&self, kind: PushNotifyKind, conv: &str, title: String) {
        let tx = self.push_tx.clone();
        let conv = conv.to_string();
        tokio::spawn(async move {
            let event = PushEvent {
                kind,
                conv_id: conv,
                title,
            };
            if let Err(e) = tx.try_send(event) {
                warn!(target: TARGET, error = %e, "push_notify 事件入队失败，丢弃");
            }
        });
    }

    /// 落盘执行体：spawn 短任务写 outbound_log。best-effort——写失败仅 warn
    /// 不阻断出站（契约纪律），也不重试（下一条出站消息 / SPA 重连重查兜底）。
    fn spawn_outbound_log(&self, conv: String, kind: &'static str, payload: String) {
        let store = self.store.clone();
        tokio::spawn(async move {
            if let Err(e) = store.append_outbound(&conv, kind, &payload).await {
                warn!(
                    target: TARGET,
                    conv = %conv,
                    kind,
                    error = %e,
                    "outbound_log 落盘失败（best-effort，不影响出站）"
                );
            }
        });
    }
}

/// 平台 drop 即 cancel：后台 client / sync responder 任务感知退出，不再无限
/// 重连（wecom P3-g 同款停机通路——测试隔离与多 profile 热替换不泄漏旧连接
/// 循环）。
impl Drop for WebPlatform {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[async_trait]
impl Platform for WebPlatform {
    /// B1 能力声明：CARDS | ASK | RECONNECT，与覆写清单逐族对照：
    /// - CARDS：send_card（card 帧 + 缓存终态供 sync 重发）/ update_card
    ///   （card_patch 帧 + 同步更新缓存）✅。（supports_streaming_card 恒 true
    ///   是 per-conv 细化方法，非能力位——core CardSession 据它分支建会话。）
    /// - ASK：send_permission_ask（approval_request）/ resolve_permission_ask /
    ///   cancel_permission_ask / cancel_all_permission_asks（approval_resolved）
    ///   ✅。（note_queued_on_ask / send_permission_ask_text 不覆写，走 default
    ///   ——web 卡无 note 行需求 / 纯文本降级路径无需平台实现。）
    /// - RECONNECT：reconnect（Notify 唤醒 client task 重连）✅。
    /// - ❌ 不声明（走 default 降级）：COMMAND_CARDS / FORMS（纯文本降级对
    ///   本平台已够——SPA 的按钮在浏览器侧由 approval/command 帧渲染）、
    ///   REACTIONS / TYPING / URGENT_TEXT / GROUP_CHATS（无对应语义）；
    ///   MEDIA_UPLOAD：send_media 覆写但显式 Err（MVP 无媒体上行）——声明
    ///   必须与实际能力一致（wecom send_media 同款处理），不声明。
    /// 一致性测试 `web_caps_match_overrides` 钉住本值。
    fn capabilities(&self) -> PlatformCaps {
        PlatformCaps::CARDS | PlatformCaps::ASK | PlatformCaps::RECONNECT
    }

    fn name(&self) -> &'static str {
        PLATFORM
    }

    async fn recv(&self) -> Result<InboundMessage> {
        self.inbound_rx.lock().await.recv().await.ok_or_else(|| {
            CoreError::Platform(PLATFORM, "入站 channel 已关闭（client 已退出）".into())
        })
    }

    async fn send_text(&self, conv: &ConvId, text: &str, _hint: &ReplyHint) -> Result<()> {
        // v1.36：一次取值同源——实时 text 帧与落盘行共享同一 at（SPA 按
        // (text, at) 精确去重防 history 重放重复；unix 毫秒恒非负，u64 契约）。
        let at = unix_ms_now() as u64;
        let json = encode_text_frame(&self.machine_id, &conv.0, text, at)?;
        self.enqueue(json)?;
        // v1.8：出站成功后异步落盘（原文；不阻塞 try_send 热路径）。
        self.log_outbound_text(&conv.0, text, at);
        Ok(())
    }

    async fn send_media(&self, _conv: &ConvId, _media: &MediaRef, _hint: &ReplyHint) -> Result<()> {
        // MVP 无媒体上行通道（relay 帧协议 v1 只有文本帧）。显式报错而非谎报
        // Ok——core 命令层（/img /file）会把文案回给用户。
        Err(CoreError::Platform(
            PLATFORM,
            "web 暂不支持媒体发送（M2 无媒体上行）".into(),
        ))
    }

    /// web 无 per-conv 差异：SPA 恒走流式卡片路径。
    fn supports_streaming_card(&self, _conv: &ConvId) -> bool {
        true
    }

    async fn send_card(
        &self,
        conv: &ConvId,
        card: &OutboundCard,
        _hint: &ReplyHint,
    ) -> Result<Option<String>> {
        let message_id = format!(
            "web-card-{}",
            self.card_seq.fetch_add(1, Ordering::Relaxed) + 1
        );
        let payload = CardPayload::from_card(card);
        // v1.8：终态帧（state=="done"）出站成功后落盘——先留完整 clone
        //（encode 内截断不影响落盘原文），非终态不 clone 不落盘。
        let log_card = (payload.state() == "done").then(|| payload.clone());
        let json = encode_card_frame(&self.machine_id, "card", &conv.0, &message_id, &payload)?;
        lock_or_recover(&self.cards).upsert(&conv.0, &message_id, payload);
        self.enqueue(json)?;
        if let Some(card) = log_card {
            self.on_terminal_card(&conv.0, &message_id, &card);
        }
        Ok(Some(message_id))
    }

    async fn update_card(
        &self,
        conv: &ConvId,
        message_id: &str,
        card: &OutboundCard,
        _hint: &ReplyHint,
    ) -> Result<()> {
        let payload = CardPayload::from_card(card);
        // v1.8：同 send_card——仅终态（state=="done"）落盘，同 messageId 幂等。
        let log_card = (payload.state() == "done").then(|| payload.clone());
        let json = encode_card_frame(
            &self.machine_id,
            "card_patch",
            &conv.0,
            message_id,
            &payload,
        )?;
        // 缓存语义是「该 conv 最近卡片」，patch 后直接覆盖（message_id 不匹配
        // 的陈旧 patch 也覆盖为最新——终态以最后一次为准）。
        lock_or_recover(&self.cards).upsert(&conv.0, message_id, payload);
        self.enqueue(json)?;
        if let Some(card) = log_card {
            self.on_terminal_card(&conv.0, message_id, &card);
        }
        Ok(())
    }

    async fn send_permission_ask(
        &self,
        conv: &ConvId,
        request_id: &str,
        tool_name: &str,
        input_summary: &str,
        _hint: &ReplyHint,
    ) -> Result<Option<String>> {
        // inputSummary 压成人可读单行（P8-1，与 feishu 审批卡同源）。
        let summary = imagent_core::render::tool_summary(tool_name, input_summary);
        let json = encode_approval_request_frame(
            &self.machine_id,
            &conv.0,
            request_id,
            tool_name,
            &summary,
        )?;
        self.track_ask(&conv.0, request_id);
        self.enqueue(json)?;
        // v1.10 push_notify：审批请求出站即埋点（title=工具名，≤32 码点）。
        // 网关无脑发——SPA 在线免打扰判定在中继（订阅 session 有活跃 WS 则
        // 跳过投递）。
        self.spawn_push_notify(
            PushNotifyKind::Approval,
            &conv.0,
            push_notify_title(tool_name),
        );
        // 返回 request_id 作 card_msg_id 锚点：浏览器审批回复带 requestId
        // （ask_req）精确路由，SPA 无「引用回复」形态，锚点即请求 id 本身。
        Ok(Some(request_id.to_string()))
    }

    async fn resolve_permission_ask(
        &self,
        conv: &ConvId,
        request_id: &str,
        reply: &PermissionReply,
    ) -> Result<()> {
        let decision = if reply.always {
            "always"
        } else if reply.allow {
            "allow"
        } else {
            "deny"
        };
        let json = encode_approval_resolved_frame(
            &self.machine_id,
            &conv.0,
            request_id,
            decision,
            reply.message.as_deref(),
        )?;
        self.untrack_ask(&conv.0, request_id);
        self.enqueue(json)
    }

    async fn cancel_permission_ask(&self, conv: &ConvId, request_id: &str) -> Result<()> {
        let json = encode_approval_resolved_frame(
            &self.machine_id,
            &conv.0,
            request_id,
            "cancelled",
            Some("审批已取消（超时或任务被中断）"),
        )?;
        self.untrack_ask(&conv.0, request_id);
        self.enqueue(json)
    }

    async fn cancel_all_permission_asks(&self, conv: &ConvId) -> Result<()> {
        // 该 conv 记录中的全部 pending 逐条发 approval_resolved(cancelled)。
        let drained = lock_or_recover(&self.pending_asks)
            .remove(&conv.0)
            .unwrap_or_default();
        for request_id in drained {
            let json = encode_approval_resolved_frame(
                &self.machine_id,
                &conv.0,
                &request_id,
                "cancelled",
                Some("审批已取消（任务被 /stop 中断）"),
            )?;
            self.enqueue(json)?;
        }
        Ok(())
    }

    /// 强制重连：notify_one 存 permit，client run task 的 select 消费后丢弃
    /// 连接 future 断开重连（退避重置为 1s）。
    async fn reconnect(&self) -> Result<()> {
        self.reconnect.notify_one();
        Ok(())
    }

    async fn doctor_probes(&self) -> Vec<String> {
        let state = if self.is_online() {
            "在线"
        } else {
            "重连中"
        };
        vec![format!(
            "✅ relay 连接：{state}（{}，machine={}）",
            self.relay_url, self.machine_id
        )]
    }

    /// v1.6 工作空间：终端 `/cd` 成功后实时推 conv_workdir 增量帧（同步
    /// best-effort——try_send 入队，队列满丢帧 warn，下条消息触发的快照
    /// 兜底；trait 约定本方法不得 IO/await）。帧带本机 machine（v1.1 出站
    /// 帧必带——SPA 增量合并按来源机器）。
    fn on_workdir_changed(&self, conv: &ConvId, path: &str) {
        match encode_conv_workdir_frame(&self.machine_id, &conv.0, path) {
            Ok(json) => {
                if let Err(e) = self.enqueue(json) {
                    warn!(
                        target: TARGET,
                        conv = %conv.0,
                        error = %e,
                        "conv_workdir 增量帧入队失败，丢弃（下条快照兜底）"
                    );
                }
            }
            Err(e) => warn!(
                target: TARGET,
                conv = %conv.0,
                error = %e,
                "conv_workdir 增量帧序列化失败"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    //! 逻辑测试不连真实 relay（地址用 loopback 拒连端口，后台任务退避空转，
    //! wecom 同款手法）；端到端行为见 tests/web_loopback.rs。

    use super::*;
    use crate::models::CliModels;
    use imagent_core::{Backend, CardPhase, CardTerminal};
    use imagent_store::OutboundRow;

    /// 工作空间直测的 machine 断言锚点（出站帧必带，v1.1 语义）。
    const TEST_MACHINE: &str = "test-mac";

    /// 测试用临时 store（真实 sqlite，临时目录——工作空间泵的 KV 读写走
    /// 与生产同一条路径）。路径唯一性不能依赖 SystemTime 纳秒（实际分辨率
    /// 约微秒级，并行测试同时构造 store 会碰撞同名 db 文件导致测试间数据
    /// 互串——曾致 collect_session_entries_* 族 1/5~1/10 偶发失败）；
    /// pid + 原子计数器 = 进程内绝对唯一。
    static STORE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    async fn test_store() -> Store {
        let n = STORE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "imagent-web-platform-{}-{n}.sqlite",
            std::process::id()
        ));
        Store::open(&p).await.expect("open store")
    }

    /// 通过 validate_workdir 的测试目录（绝对 + is_dir + 非过宽）。
    fn valid_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("imagent-web-ws-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    async fn offline_platform() -> WebPlatform {
        offline_platform_with_store(test_store().await).await
    }

    /// [`offline_platform`] 的 store 注入形态（v1.8 落盘测试用：平台与断言
    /// 共享同一 store 句柄——落盘是 spawn 出去的异步任务，只能从库侧观测）。
    async fn offline_platform_with_store(store: Store) -> WebPlatform {
        // Token 凭证 → E2E 身份不装配（无文件 IO），new_with_e2e 的路径仅
        // 满足签名（避免 new 的缺省路径锚定真实 ~/.imagent）。
        let e2e_key = std::env::temp_dir().join("imagent-web-platform-e2e-unused");
        WebPlatform::new_with_e2e(
            "ws://127.0.0.1:1/gateway".into(),
            GatewayCredential::Token("unused-token".into()),
            "default".into(),
            "test-mac".into(),
            &e2e_key,
            store,
            valid_dir(),
            empty_models(),
            Vec::new(), // 空名单 → 快照 backends 段整段缺席（既有测试零感知）
        )
    }

    /// 出站帧注入的测试夹具（v1.36 at 同源测试用）：替换平台的 outbound_tx
    ///（子模块直取私有字段，同 offline_platform_with_push_capture 手法；
    /// client 持有的原 rx 随假连接空转），实时帧可从 test rx 观察。
    async fn offline_platform_with_outbound_capture(
        store: Store,
    ) -> (WebPlatform, mpsc::Receiver<String>) {
        let mut p = offline_platform_with_store(store).await;
        let (tx, rx) = mpsc::channel::<String>(OUTBOUND_CHANNEL_CAPACITY);
        p.outbound_tx = tx;
        (p, rx)
    }

    /// Drop 平台必须 cancel 停机信号（后台任务连动退出）。
    #[tokio::test]
    async fn drop_platform_cancels_shutdown_token() {
        let p = offline_platform().await;
        let token = p.shutdown.clone();
        assert!(!token.is_cancelled());
        drop(p);
        assert!(token.is_cancelled(), "Drop 必须 cancel 停机信号");
    }

    /// B1：caps 声明与覆写清单一致性（钉值防漂移，对照 capabilities() 旁注释）。
    #[tokio::test]
    async fn web_caps_match_overrides() {
        let p = offline_platform().await;
        assert_eq!(
            p.capabilities(),
            PlatformCaps::CARDS | PlatformCaps::ASK | PlatformCaps::RECONNECT,
            "web 声明 cards+ask+reconnect"
        );
        assert_eq!(p.name(), "web");
        assert!(p.supports_streaming_card(&ConvId("web:x".into())));
        // send_media 覆写为显式 Err → MEDIA_UPLOAD 不声明（覆写≠支持）。
        assert!(!p.capabilities().contains(PlatformCaps::MEDIA_UPLOAD));
        drop(p);
    }

    /// doctor_probes：离线（初始未连接）显示重连中；在线标志翻转后显示在线。
    #[tokio::test]
    async fn doctor_probes_reflect_online_state() {
        let p = offline_platform().await;
        let probes = p.doctor_probes().await;
        assert_eq!(probes.len(), 1);
        assert!(
            probes[0].contains("重连中") && probes[0].contains("ws://127.0.0.1:1/gateway"),
            "{}",
            probes[0]
        );
        // 直接翻 client 维护的标志（在线翻转路径由回环测试覆盖真连接）。
        p.online.store(true, Ordering::Relaxed);
        let probes = p.doctor_probes().await;
        assert!(probes[0].contains("在线"), "{}", probes[0]);
        drop(p);
    }

    /// send_media 显式 Err（不谎报 Ok）。
    #[tokio::test]
    async fn send_media_errors_explicitly() {
        let p = offline_platform().await;
        let e = p
            .send_media(
                &ConvId("web:x".into()),
                &MediaRef {
                    kind: "image".into(),
                    url: "/tmp/a.png".into(),
                },
                &ReplyHint::None,
            )
            .await
            .unwrap_err();
        assert!(format!("{e}").contains("媒体"), "{e}");
        drop(p);
    }

    // ---- v1.6 工作空间动作处理（handle_workspace_action 直测：store + 出站
    // channel 注入，不经网络；端到端路径见 tests/web_loopback.rs——含
    // on_workdir_changed 的 /cd 通知路径，帧在真实 relay 的浏览器端观测）----

    /// v1.8 mock backend（只读 model()——models.current 数据源；run 不可达）。
    struct TestBackend(Option<String>);

    #[async_trait]
    impl Backend for TestBackend {
        #[allow(clippy::too_many_arguments)] // trait 方法固定形状（先例 backend.rs）
        async fn run(
            &self,
            _conv_id: &str,
            _prompt: &str,
            _session: Option<&imagent_core::SessionId>,
            _workdir: &Path,
            _allowed_tools: &[String],
            _chunks: tokio::sync::mpsc::Sender<imagent_core::AgentChunk>,
            _initial_todos: &[imagent_core::TodoItem],
            _steer: tokio::sync::mpsc::Receiver<String>,
        ) -> Result<imagent_core::RunOutcome> {
            Err(CoreError::Platform(
                "test",
                "mock backend 不执行 run".into(),
            ))
        }

        fn name(&self) -> &'static str {
            "test"
        }

        fn model(&self) -> Option<String> {
            self.0.clone()
        }
    }

    /// 既有动作测试的空模型上下文（official 两后端全空 + 无 CLI 映射 → models
    /// 段整段缺席，快照字节与 v1.6/v1.7 一致——断言不依赖运行机器的
    /// ~/.claude 状态，确定性钉住）。
    fn empty_models() -> ModelsContext {
        ModelsContext {
            backend: Arc::new(TestBackend(None)),
            official: imagent_core::OfficialModels::default(),
            list_models: None,
        }
    }

    /// 处理一条动作并取回全部出站帧（解析为 Value 便于断言）。
    /// machine 固定 TEST_MACHINE（出站帧 machine 断言锚点）。
    async fn run_action(
        store: &Store,
        default: &Path,
        act: InboundAction,
    ) -> Vec<serde_json::Value> {
        run_action_with(
            store,
            default,
            &empty_models(),
            &[],
            &CliSource::Claude(CliModels::default()),
            act,
        )
        .await
    }

    /// [`run_action`] 的 v1.8 全参形态（模型上下文 + CLI/codex 配置快照 +
    /// v1.36 引擎名单可注入）。
    async fn run_action_with(
        store: &Store,
        default: &Path,
        models: &ModelsContext,
        backends: &[String],
        cli: &CliSource,
        act: InboundAction,
    ) -> Vec<serde_json::Value> {
        let (tx, mut rx) = mpsc::channel::<String>(32);
        handle_workspace_action(
            TEST_MACHINE,
            store,
            default,
            models,
            backends,
            cli,
            &disabled_titles(store),
            act,
            &tx,
        )
        .await;
        let mut out = Vec::new();
        while let Ok(json) = rx.try_recv() {
            out.push(serde_json::from_str(&json).unwrap());
        }
        out
    }

    /// 禁用语义标题的生成器（既有动作测试注入）：假执行器恒失败——懒触发
    /// 路径走通但零副作用（不依赖运行机器上的 claude，确定性钉住）。
    fn disabled_titles(store: &Store) -> TitleGen {
        TitleGen::with_runner(
            store.clone(),
            std::sync::Arc::new(|_argv: Vec<String>, _cwd: PathBuf| {
                let fut: futures::future::BoxFuture<'static, std::result::Result<String, String>> =
                    Box::pin(async { Err("测试禁用语义标题".into()) });
                fut
            }),
            Duration::from_millis(10),
        )
    }

    /// v1.8 models 段组装（任务验收项）：官方回退（无 CLI 映射 → options =
    /// official.claude 列表、target null、current = backend.model()）；CLI 映射
    /// 优先（只出有映射的别名、target = 映射值、cliDefault 原样）；空 official
    /// 且无映射 → 整段缺席（旧形态字节不变）。
    #[tokio::test]
    async fn workspace_snapshot_models_section() {
        let store = test_store().await;
        let default = valid_dir();

        // 官方回退 + current。
        let ctx = ModelsContext {
            backend: Arc::new(TestBackend(Some("glm-4.6".into()))),
            official: imagent_core::OfficialModels {
                claude: vec!["sonnet".into(), "opus".into(), "haiku".into()],
                codex: vec![],
            },
            list_models: None,
        };
        let frames = run_action_with(
            &store,
            &default,
            &ctx,
            &[],
            &CliSource::Claude(CliModels::default()),
            InboundAction::WorkspaceList,
        )
        .await;
        let snap = frames
            .iter()
            .find(|f| f["type"] == "workspaces")
            .unwrap_or_else(|| panic!("应回快照：{frames:?}"));
        assert_eq!(snap["machine"], TEST_MACHINE, "快照帧带本机 machine");
        let models = &snap["models"];
        assert_eq!(models["current"], "glm-4.6", "current = backend.model()");
        assert_eq!(models["cliDefault"], serde_json::Value::Null);
        let opts = models["options"].as_array().unwrap();
        assert_eq!(opts.len(), 3, "官方回退三件套：{opts:?}");
        assert_eq!(opts[0]["label"], "sonnet");
        assert!(
            opts.iter().all(|o| o["target"].is_null()),
            "官方回退 target 恒 null：{opts:?}"
        );

        // CLI 映射优先（2/3 子集；opus 无映射不出）+ cliDefault。
        let cli = CliModels {
            cli_default: Some("glm-4.6".into()),
            aliases: [
                ("sonnet".to_string(), "glm-4.6".to_string()),
                ("haiku".to_string(), "glm-4.5-air".to_string()),
            ]
            .into_iter()
            .collect(),
        };
        let frames = run_action_with(
            &store,
            &default,
            &ctx,
            &[],
            &CliSource::Claude(cli),
            InboundAction::WorkspaceList,
        )
        .await;
        let snap = frames.iter().find(|f| f["type"] == "workspaces").unwrap();
        let opts = snap["models"]["options"].as_array().unwrap();
        assert_eq!(
            opts.iter()
                .map(|o| (o["label"].as_str().unwrap(), o["target"].as_str().unwrap()))
                .collect::<Vec<_>>(),
            vec![("sonnet", "glm-4.6"), ("haiku", "glm-4.5-air")],
            "仅有映射的别名（opus 不出），展示序 sonnet→haiku"
        );
        assert_eq!(snap["models"]["cliDefault"], "glm-4.6");

        // 空 official 且无映射 → 整段缺席（旧形态字节不变）。
        let frames = run_action(&store, &default, InboundAction::WorkspaceList).await;
        let snap = frames.iter().find(|f| f["type"] == "workspaces").unwrap();
        assert!(
            snap.get("models").is_none(),
            "空 options 整段缺席（契约）：{snap}"
        );
    }

    /// codex-app 链 models 段组装（codex-app 批验收项）：实时 list_models 闭包
    ///（经 build_workspaces_frame 的 spawn_blocking 通路）非空短路——options
    /// label=target=id；cliDefault 取 codex config 快照（与 options 来源解耦）；
    /// 实时空 → official.codex 接管（target null）。
    #[tokio::test]
    async fn workspace_snapshot_models_section_codex_app() {
        let store = test_store().await;
        let default = valid_dir();

        // 第①层：实时闭包非空 → 整表接管（target=Some(id)）。
        let ctx = ModelsContext {
            backend: Arc::new(TestBackend(Some("glm-5.1".into()))),
            official: imagent_core::OfficialModels {
                claude: vec![],
                codex: vec!["gpt-5-codex".into()],
            },
            list_models: Some(Arc::new(|| vec!["glm-5.1".into(), "gpt-5.3".into()])),
        };
        let codex = crate::models::CodexModels {
            cli_default: Some("gpt-5-codex".into()),
            catalog: vec!["gpt-5-codex".into()],
        };
        let frames = run_action_with(
            &store,
            &default,
            &ctx,
            &[],
            &CliSource::Codex(codex.clone()),
            InboundAction::WorkspaceList,
        )
        .await;
        let snap = frames.iter().find(|f| f["type"] == "workspaces").unwrap();
        let models = &snap["models"];
        assert_eq!(models["current"], "glm-5.1", "current = backend.model()");
        assert_eq!(
            models["cliDefault"], "gpt-5-codex",
            "cliDefault 取 codex config"
        );
        let opts = models["options"].as_array().unwrap();
        assert_eq!(
            opts.iter()
                .map(|o| (o["label"].as_str().unwrap(), o["target"].as_str().unwrap()))
                .collect::<Vec<_>>(),
            vec![("glm-5.1", "glm-5.1"), ("gpt-5.3", "gpt-5.3")],
            "实时层短路（catalog 不出），label=target=id：{opts:?}"
        );

        // 第①层空回（失败降级形态）→ official.codex 接管，target null。
        let ctx_empty = ModelsContext {
            backend: Arc::new(TestBackend(None)),
            official: imagent_core::OfficialModels {
                claude: vec![],
                codex: vec!["gpt-5-codex".into(), "gpt-5.1-codex-mini".into()],
            },
            list_models: Some(Arc::new(Vec::new)),
        };
        let codex_empty = crate::models::CodexModels::default();
        let frames = run_action_with(
            &store,
            &default,
            &ctx_empty,
            &[],
            &CliSource::Codex(codex_empty),
            InboundAction::WorkspaceList,
        )
        .await;
        let snap = frames.iter().find(|f| f["type"] == "workspaces").unwrap();
        let opts = snap["models"]["options"].as_array().unwrap();
        assert_eq!(opts.len(), 2, "实时空 → 官方层接管：{opts:?}");
        assert!(
            opts.iter().all(|o| o["target"].is_null()),
            "官方层 target 恒 null：{opts:?}"
        );
    }

    /// v1.36 backends 段组装（任务验收项）：装配注入引擎名单后快照带
    /// `backends{current, options[]}`——current 恒 = 首位 default（未设
    /// `backend:<conv>` KV 的会话实际引擎），options 保序全名单；空名单 →
    /// 整段缺席（旧形态字节不变）。与 models 段同帧并存互不干扰（两段
    /// 数据源解耦：models 走 CLI/官方链，backends 纯名单直传）。
    #[tokio::test]
    async fn workspace_snapshot_backends_section() {
        let store = test_store().await;
        let default = valid_dir();

        // 注入名单：default 在首位 + extras 保序（main 装配形态）。
        let names = ["claude-cli".to_string(), "codex-app".to_string()];
        let frames = run_action_with(
            &store,
            &default,
            &empty_models(),
            &names,
            &CliSource::Claude(CliModels::default()),
            InboundAction::WorkspaceList,
        )
        .await;
        let snap = frames
            .iter()
            .find(|f| f["type"] == "workspaces")
            .unwrap_or_else(|| panic!("应回快照：{frames:?}"));
        assert_eq!(snap["machine"], TEST_MACHINE, "快照帧带本机 machine");
        let backends = &snap["backends"];
        assert_eq!(backends["current"], "claude-cli", "current = 首位 default");
        assert_eq!(
            backends["options"].as_array().map(Vec::len),
            Some(2),
            "options = 全注册表名单"
        );
        assert_eq!(backends["options"][0], "claude-cli");
        assert_eq!(backends["options"][1], "codex-app");

        // 与 models 段并存：两段各自独立成形（官方回退非空 + 名单非空）。
        let ctx = ModelsContext {
            backend: Arc::new(TestBackend(Some("glm-4.6".into()))),
            official: imagent_core::OfficialModels {
                claude: vec!["sonnet".into()],
                codex: vec![],
            },
            list_models: None,
        };
        let frames = run_action_with(
            &store,
            &default,
            &ctx,
            &names,
            &CliSource::Claude(CliModels::default()),
            InboundAction::WorkspaceList,
        )
        .await;
        let snap = frames.iter().find(|f| f["type"] == "workspaces").unwrap();
        assert_eq!(
            snap["models"]["current"], "glm-4.6",
            "models 段不受名单影响"
        );
        assert_eq!(
            snap["backends"]["current"], "claude-cli",
            "backends 段独立成形"
        );

        // 空名单 → 整段缺席（未注入名单的装配形态，既有网关零感知）。
        let frames = run_action(&store, &default, InboundAction::WorkspaceList).await;
        let snap = frames.iter().find(|f| f["type"] == "workspaces").unwrap();
        assert!(
            snap.get("backends").is_none(),
            "空名单整段缺席（契约）：{snap}"
        );
    }

    /// conv_workdir_set 校验链（任务说明验收项）：非绝对路径 / 目录不存在 /
    /// 过宽目录（/、home 根）→ error 帧 code=workspace_invalid 且
    /// `workdir:<conv>` 未写入 + 照常回快照（快照不变）。
    #[tokio::test]
    async fn conv_workdir_set_validation_chain_rejects() {
        let store = test_store().await;
        let default = valid_dir();
        let home = std::env::var("HOME").expect("测试环境应有 HOME");
        for bad in ["relative/x", "/definitely/not/exist/dir", "/", &home] {
            let frames = run_action(
                &store,
                &default,
                InboundAction::ConvWorkdirSet {
                    conv: "web:work".into(),
                    path: bad.to_string(),
                },
            )
            .await;
            let err = frames
                .iter()
                .find(|f| f["type"] == "error")
                .unwrap_or_else(|| panic!("{bad} 应回 error 帧：{frames:?}"));
            assert_eq!(err["code"], WORKSPACE_ERROR_CODE, "{bad}：{err}");
            assert_eq!(
                err["machine"], TEST_MACHINE,
                "{bad}：error 帧带本机 machine"
            );
            assert!(
                err["message"].as_str().is_some_and(|m| !m.is_empty()),
                "message 须为非空人话：{err}"
            );
            // 快照照常回（SPA 单一数据来源），但 workdir KV 未写入。
            assert!(
                frames.iter().any(|f| f["type"] == "workspaces"),
                "{bad} 失败也应回快照：{frames:?}"
            );
            assert!(
                store
                    .get_config(&workdir_key("web:work"))
                    .await
                    .unwrap()
                    .is_none(),
                "{bad} 不得写入 workdir:<conv>"
            );
            // conv_workdir 增量帧不出现（校验失败）。
            assert!(
                !frames.iter().any(|f| f["type"] == "conv_workdir"),
                "{bad} 校验失败不得推增量帧"
            );
        }
    }

    /// conv_workdir_set 成功链（任务说明验收项）：写 `workdir:<conv>`（与
    /// /cd 同键）；命中空间 path 时 bump lastUsedAt；额外推 conv_workdir
    /// 增量帧 + workspaces 快照（快照含 local: conv、含 legacy 裸路径值的
    /// 兼容读）。
    #[tokio::test]
    async fn conv_workdir_set_success_writes_bumps_and_replies() {
        let store = test_store().await;
        let default = valid_dir();
        let dir = valid_dir();
        // 种子：一个新格式空间（path 命中 dir，无 lastUsedAt）+ 一个旧格式
        // 裸路径空间（存量部署兼容读）+ 一条 local: conv 的 workdir（/cd 写）。
        store
            .set_config(
                &workspace_key("proj"),
                &WorkspaceRecord::new(dir.to_string_lossy()).encode(),
            )
            .await
            .unwrap();
        store
            .set_config(&workspace_key("legacy"), "/tmp/imagent-legacy-ws")
            .await
            .unwrap();
        store
            .set_config(&workdir_key("local:work"), "/tmp/imagent-local-ws")
            .await
            .unwrap();

        let frames = run_action(
            &store,
            &default,
            InboundAction::ConvWorkdirSet {
                conv: "local:work".into(),
                path: dir.to_string_lossy().into_owned(),
            },
        )
        .await;

        // KV：与 /cd 同键写入。
        assert_eq!(
            store
                .get_config(&workdir_key("local:work"))
                .await
                .unwrap()
                .as_deref(),
            Some(dir.to_string_lossy().as_ref()),
            "写 workdir:<conv>（裸路径，与 /cd 同形态）"
        );
        // bump：proj 的 lastUsedAt 出现。
        let bumped = WorkspaceRecord::decode(
            &store
                .get_config(&workspace_key("proj"))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(bumped.last_used_at.is_some(), "命中空间 path 须 bump");
        // 出站：增量帧 + 快照（均带 machine，v1.1 出站帧必带——SPA 按其
        // 归属机器）。
        let delta = frames.iter().find(|f| f["type"] == "conv_workdir").unwrap();
        assert_eq!(delta["convId"], "local:work");
        assert_eq!(delta["machine"], TEST_MACHINE, "增量帧带本机 machine");
        assert_eq!(delta["workdir"], dir.to_string_lossy().as_ref());
        let snap = frames.iter().find(|f| f["type"] == "workspaces").unwrap();
        assert_eq!(snap["machine"], TEST_MACHINE, "快照帧带本机 machine");
        assert_eq!(snap["default"], default.to_string_lossy().as_ref());
        // 快照 workspaces：name 稳定序 + legacy 兼容读（裸路径 + lastUsedAt 缺席）。
        let ws = snap["workspaces"].as_array().unwrap();
        assert_eq!(ws.len(), 2);
        assert_eq!(ws[0]["name"], "legacy", "name 稳定序（legacy < proj）");
        assert!(
            ws[0].get("lastUsedAt").is_none(),
            "旧值读出 lastUsedAt 缺席"
        );
        assert_eq!(ws[0]["path"], "/tmp/imagent-legacy-ws");
        assert_eq!(ws[1]["name"], "proj");
        assert!(ws[1]["lastUsedAt"].is_u64(), "bump 后快照携带 lastUsedAt");
        // 快照 convs：含 local: 会话。
        assert_eq!(snap["convs"]["local:work"], dir.to_string_lossy().as_ref());
    }

    /// workspace_save / remove / list：非法 name（空白/超长）与非法 path →
    /// error 帧；合法保存落库新 JSON 格式；remove 未命中幂等（仅快照，无
    /// error）；list 仅回快照。每帧必有快照。
    #[tokio::test]
    async fn workspace_save_remove_list_flow() {
        let store = test_store().await;
        let default = valid_dir();
        let dir = valid_dir();

        // 非法 name：空白 / 超 32 char。
        for bad_name in ["   ", &"x".repeat(33)] {
            let frames = run_action(
                &store,
                &default,
                InboundAction::WorkspaceSave {
                    name: bad_name.to_string(),
                    path: dir.to_string_lossy().into_owned(),
                },
            )
            .await;
            let err = frames.iter().find(|f| f["type"] == "error").unwrap();
            assert_eq!(err["code"], WORKSPACE_ERROR_CODE);
            assert!(
                store
                    .list_config(WORKSPACE_PREFIX)
                    .await
                    .unwrap()
                    .is_empty(),
                "非法 name 不得落库"
            );
        }

        // 合法保存（trim 生效：` proj ` → `proj`）。
        let frames = run_action(
            &store,
            &default,
            InboundAction::WorkspaceSave {
                name: " proj ".into(),
                path: dir.to_string_lossy().into_owned(),
            },
        )
        .await;
        assert!(
            !frames.iter().any(|f| f["type"] == "error"),
            "合法保存不应有 error：{frames:?}"
        );
        let raw = store
            .get_config(&workspace_key("proj"))
            .await
            .unwrap()
            .unwrap();
        let rec = WorkspaceRecord::decode(&raw).unwrap();
        assert_eq!(rec.path, dir.to_string_lossy().as_ref());
        assert_eq!(rec.last_used_at, None, "刚保存 lastUsedAt 缺席");
        assert!(
            frames.iter().any(|f| f["type"] == "workspaces"),
            "每帧回快照"
        );

        // remove：命中删除；再次 remove（未命中）幂等——只回快照无 error。
        let frames = run_action(
            &store,
            &default,
            InboundAction::WorkspaceRemove {
                name: "proj".into(),
            },
        )
        .await;
        assert!(!frames.iter().any(|f| f["type"] == "error"));
        assert!(store
            .get_config(&workspace_key("proj"))
            .await
            .unwrap()
            .is_none());
        let frames = run_action(
            &store,
            &default,
            InboundAction::WorkspaceRemove {
                name: "proj".into(),
            },
        )
        .await;
        assert!(
            !frames.iter().any(|f| f["type"] == "error"),
            "未命中 remove 幂等：{frames:?}"
        );
        assert!(frames.iter().any(|f| f["type"] == "workspaces"));

        // list：空表也回快照（default + 空集合）。
        let frames = run_action(&store, &default, InboundAction::WorkspaceList).await;
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["type"], "workspaces");
        assert_eq!(frames[0]["workspaces"].as_array().unwrap().len(), 0);
    }

    // ---- v1.7 媒体上行泵（process_media_message 直测：临时媒体目录 + 双通道
    // 注入，不经网络；端到端 enc 路径与 message 帧同链无需重复覆盖）----

    use crate::MediaPayload;
    use imagent_store::SessionRow;

    /// 每测试独立的临时媒体目录（pid+nanos 防并行互踩）。
    fn media_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "imagent-web-media-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn media_b64(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// 构造一条媒体动作（text 缺省纯图形态）。
    fn media_act(name: &str, mime: &str, b64: &str) -> InboundAction {
        InboundAction::MediaMessage {
            conv: "web:work".into(),
            text: None,
            media: MediaPayload {
                name: name.into(),
                mime: mime.into(),
                b64: b64.into(),
            },
        }
    }

    /// 处理一条媒体动作，取回出站帧（解析为 Value）与投递的入站消息。
    async fn run_media(
        media_dir: &Path,
        act: InboundAction,
    ) -> (Vec<serde_json::Value>, Vec<InboundMessage>) {
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<String>(32);
        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(8);
        process_media_message(TEST_MACHINE, media_dir, act, &inbound_tx, &outbound_tx).await;
        let mut frames = Vec::new();
        while let Ok(json) = outbound_rx.try_recv() {
            frames.push(serde_json::from_str(&json).unwrap());
        }
        let mut msgs = Vec::new();
        while let Ok(m) = inbound_rx.try_recv() {
            msgs.push(m);
        }
        (frames, msgs)
    }

    /// 媒体上行正例（任务验收项）：image/png 合法 → 落盘（路径模板
    /// `up-<unix_ms>-<安全化name>`、0600、内容一致）+ InboundMessage 断言
    /// （media 引用落盘路径、kind 载 mime、text 非空才 Some）。无 error 帧。
    #[tokio::test]
    async fn media_message_persists_and_delivers() {
        let dir = media_dir();
        let bytes = b"\x89PNG\r\n\x1a\nfake-png-body".to_vec();
        let (frames, msgs) = run_media(
            &dir,
            InboundAction::MediaMessage {
                conv: "web:work".into(),
                text: Some("看这张图".into()),
                media: MediaPayload {
                    name: "shot.png".into(),
                    mime: "image/png".into(),
                    b64: media_b64(&bytes),
                },
            },
        )
        .await;
        assert!(frames.is_empty(), "成功路径无出站帧：{frames:?}");
        assert_eq!(msgs.len(), 1);
        let m = &msgs[0];
        assert_eq!(m.conv_id, ConvId("web:work".into()));
        assert_eq!(m.text.as_deref(), Some("看这张图"));
        assert_eq!(m.media.len(), 1);
        assert_eq!(m.media[0].kind, "image/png");
        let path = Path::new(&m.media[0].url);
        assert!(path.is_file(), "媒体须落盘：{}", path.display());
        let fname = path.file_name().unwrap().to_str().unwrap();
        assert!(
            fname.starts_with("up-") && fname.ends_with("-shot.png"),
            "路径模板 up-<unix_ms>-<name>：{fname}"
        );
        assert_eq!(
            std::fs::read(path).unwrap(),
            bytes,
            "落盘内容与解码字节一致"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "媒体文件 0600");
        }

        // 纯图：text 空串 → None（契约：非空才 Some）。
        let (frames, msgs) = run_media(
            &dir,
            InboundAction::MediaMessage {
                conv: "web:work".into(),
                text: Some(String::new()),
                media: MediaPayload {
                    name: "shot2.png".into(),
                    mime: "image/png".into(),
                    b64: media_b64(&bytes),
                },
            },
        )
        .await;
        assert!(frames.is_empty());
        assert_eq!(msgs[0].text, None, "空串说明归一为 None");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 媒体上行反例（任务验收项）：非 image/* mime / 坏 b64 / 超 8MB → 对应
    /// error 帧（media_invalid / media_too_large，后者 message 为契约原文），
    /// 不投递消息、不落盘。边界：恰好 8MB（= 上限）放行。
    #[tokio::test]
    async fn media_message_rejects_invalid() {
        let dir = media_dir();
        // 非 image/* mime。
        let (frames, msgs) =
            run_media(&dir, media_act("clip.mp4", "video/mp4", &media_b64(b"x"))).await;
        let err = frames
            .iter()
            .find(|f| f["type"] == "error")
            .unwrap_or_else(|| panic!("非 image mime 应回 error 帧：{frames:?}"));
        assert_eq!(err["code"], MEDIA_INVALID_CODE);
        assert_eq!(err["machine"], TEST_MACHINE, "error 帧带本机 machine");
        assert!(msgs.is_empty(), "不投递消息");

        // 坏 b64。
        let (frames, msgs) =
            run_media(&dir, media_act("a.png", "image/png", "!!!not-base64!!!")).await;
        assert_eq!(frames[0]["code"], MEDIA_INVALID_CODE, "{frames:?}");
        assert!(msgs.is_empty());

        // 超 8MB（上限 +1 字节）：契约原文 message。
        let oversized = media_b64(&vec![0u8; MEDIA_MAX_BYTES + 1]);
        let (frames, msgs) = run_media(&dir, media_act("big.png", "image/png", &oversized)).await;
        let err = frames.iter().find(|f| f["type"] == "error").unwrap();
        assert_eq!(err["code"], MEDIA_TOO_LARGE_CODE);
        assert_eq!(err["message"], "图片超过 8MB 上限", "契约原文 message");
        assert!(msgs.is_empty());

        // 失败路径不落盘任何文件（目录尚未创建）。
        assert!(
            std::fs::read_dir(&dir)
                .map(|mut it| it.next().is_none())
                .unwrap_or(true),
            "校验失败不得落盘"
        );

        // 边界：恰好 8MB（≤ 上限）放行。
        let exact = media_b64(&vec![0u8; MEDIA_MAX_BYTES]);
        let (frames, msgs) = run_media(&dir, media_act("exact.png", "image/png", &exact)).await;
        assert!(frames.is_empty(), "恰 8MB 不超限：{frames:?}");
        assert_eq!(msgs.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// media.name 安全化：非 [A-Za-z0-9._-] 替换 `_`（`/` 被替换——路径穿越
    /// 不可达）；≤64 字符按码点截断。
    #[test]
    fn sanitize_media_name_rules() {
        assert_eq!(sanitize_media_name("shot.png"), "shot.png");
        assert_eq!(sanitize_media_name("截 图 v1★.png"), "____v1_.png");
        let trav = sanitize_media_name("../../etc/passwd.png");
        assert!(!trav.contains('/'), "路径分隔符被替换：{trav}");
        assert_eq!(trav, ".._.._etc_passwd.png");
        assert_eq!(sanitize_media_name(&"a".repeat(70)), "a".repeat(64));
        assert_eq!(
            sanitize_media_name(&"中".repeat(80)),
            "_".repeat(64),
            "非 ASCII 按码点 1:1 替换后截 64"
        );
    }

    // ---- v1.7 sessions 段数据源（collect_session_entries 直测：真实 store，
    // upsert_session 同表写入主表+历史副表）----

    /// 种子一条 session 行（主表 + 历史副表同事务写入，updated_at=now）。
    async fn seed_session(
        store: &Store,
        conv: &str,
        name: Option<&str>,
        first_prompt: Option<&str>,
    ) {
        store
            .upsert_session(&SessionRow {
                conv_id: conv.into(),
                session_id: format!("s-{conv}"),
                agent_kind: "claude".into(),
                workdir: "/w".into(),
                name: name.map(String::from),
                created_at: 0,
                updated_at: 0, // upsert 实际落库取 now_secs()，此处不生效
                first_prompt: first_prompt.map(String::from),
                task_todos: None,
            })
            .await
            .unwrap();
    }

    /// sessions 段数据源（任务验收项）：仅 `web:` 前缀；按 updated_at 降序
    /// （单调不增）；title 手动名优先 / first_prompt 摘要（历史副表最近行）
    /// 按码点截 24 / 皆无 → 空串。
    #[tokio::test]
    async fn collect_session_entries_filters_sorts_and_titles() {
        let store = test_store().await;
        let titles = disabled_titles(&store);
        // 非 web 会话：不进段。
        seed_session(&store, "ilink:u1", None, Some("不该出现")).await;
        seed_session(&store, "local:work", None, Some("也不该出现")).await;
        // web 会话：手动名 / 长 first_prompt（>24 中文）/ 两者皆无。
        seed_session(&store, "web:alpha", Some("手动名"), None).await;
        seed_session(
            &store,
            "web:beta",
            None,
            Some("这是一段超过二十四个字符长度的会话首条消息摘要样例文本"),
        )
        .await;
        seed_session(&store, "web:gamma", None, None).await;

        let entries = collect_session_entries(&store, &titles).await;
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), 3, "仅 web: 前缀：{ids:?}");
        assert!(ids.iter().all(|i| i.starts_with("web:")));
        // 降序（同秒并列按 conv_id 升序——alpha < beta < gamma）。
        assert_eq!(ids, vec!["web:alpha", "web:beta", "web:gamma"]);
        for w in entries.windows(2) {
            assert!(
                w[0].updated_at >= w[1].updated_at,
                "updated_at 降序：{entries:?}"
            );
        }
        let title_of = |id: &str| {
            entries
                .iter()
                .find(|e| e.id == id)
                .unwrap_or_else(|| panic!("{id} 缺席：{entries:?}"))
                .title
                .clone()
        };
        assert_eq!(title_of("web:alpha"), "手动名", "手动名优先");
        assert_eq!(
            title_of("web:beta"),
            "这是一段超过二十四个字符长度的会话首条消息摘要样",
            "first_prompt 摘要按码点截 24（中文）"
        );
        assert_eq!(title_of("web:gamma"), "", "无手动名无 first_prompt → 空串");
        assert!(
            entries.iter().all(|e| e.updated_at > 0),
            "updatedAt 为 unix 秒"
        );
    }

    /// sessions 段上限 50（契约）：种 52 条 web: 会话 → 恰 50 条。
    #[tokio::test]
    async fn collect_session_entries_caps_at_50() {
        let store = test_store().await;
        let titles = disabled_titles(&store);
        for i in 0..(WEB_SESSIONS_MAX + 2) {
            seed_session(&store, &format!("web:s{i:02}"), None, Some("首条")).await;
        }
        let entries = collect_session_entries(&store, &titles).await;
        assert_eq!(entries.len(), WEB_SESSIONS_MAX, "上限 50（52 条种子）");
        for w in entries.windows(2) {
            assert!(w[0].updated_at >= w[1].updated_at, "截断前已按降序排");
        }
    }

    /// v1.9 title 三档优先级（任务验收项）：手动名 > 语义标题
    ///（sessions.title 列）> first_prompt 摘要；语义标题已生成后不再查
    /// first_prompt（显示档位已定）。
    #[tokio::test]
    async fn collect_session_entries_title_priority_manual_semantic_prompt() {
        let store = test_store().await;
        let titles = disabled_titles(&store);
        // 手动名 + 语义标题 + first_prompt 三者齐备：手动名胜出。
        seed_session(&store, "web:p1", Some("手动名"), Some("首条摘要")).await;
        store.set_session_title("web:p1", "语义标题").await.unwrap();
        // 语义标题 + first_prompt：语义标题胜出。
        seed_session(&store, "web:p2", None, Some("首条摘要")).await;
        store.set_session_title("web:p2", "语义标题").await.unwrap();
        // 仅 first_prompt：机械摘要兜底（v1.7 行为不变）。
        seed_session(&store, "web:p3", None, Some("首条摘要")).await;

        let entries = collect_session_entries(&store, &titles).await;
        let title_of = |id: &str| {
            entries
                .iter()
                .find(|e| e.id == id)
                .unwrap_or_else(|| panic!("{id} 缺席：{entries:?}"))
                .title
                .clone()
        };
        assert_eq!(title_of("web:p1"), "手动名", "手动名 > 语义标题");
        assert_eq!(
            title_of("web:p2"),
            "语义标题",
            "语义标题 > first_prompt 摘要"
        );
        assert_eq!(title_of("web:p3"), "首条摘要", "first_prompt 摘要兜底");
    }

    /// v1.9 懒触发集成（任务验收项）：快照组装对「无手动名、无语义 title、
    /// first_prompt 非空」的 conv 逐批生成——假执行器记录调用，验证每快照
    /// 至多 3 个（防风暴）、手动名/已有语义标题/无 first_prompt 的 conv 不
    /// 触发、成功写库后下一条快照 title 升级为语义标题。「最旧优先」的精确
    /// 排序断言见 titles.rs 单测（此处种子同秒 upsert，秒界跨越会让「最旧」
    /// 组成抖动，故只钉批大小与收敛性）。
    #[tokio::test]
    async fn collect_session_entries_lazy_triggers_title_generation() {
        let store = test_store().await;
        // 5 个候选 + 三个不触发样本。
        for i in 1..=5 {
            seed_session(
                &store,
                &format!("web:t{i}"),
                None,
                Some(&format!("任务{i}的首条消息")),
            )
            .await;
        }
        seed_session(&store, "web:named", Some("手动名"), Some("不该触发的首条")).await; // 手动名：免生成
        seed_session(&store, "web:done", None, Some("已生成的首条")).await;
        store
            .set_session_title("web:done", "已有语义标题")
            .await
            .unwrap(); // 已有：免生成
        seed_session(&store, "web:empty", None, None).await; // 无 first_prompt：免生成

        // 记录型假执行器：按 prompt 末行（first_prompt 原文）回可辨认的标题。
        let calls: std::sync::Arc<Mutex<Vec<String>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        let rec = calls.clone();
        let runner = std::sync::Arc::new(move |argv: Vec<String>, _cwd: PathBuf| {
            let rec = rec.clone();
            let fut: futures::future::BoxFuture<'static, std::result::Result<String, String>> =
                Box::pin(async move {
                    let body = argv[2].lines().last().unwrap_or_default().to_string();
                    rec.lock().unwrap().push(body.clone());
                    // 从「任务N的…」提取 N 回标题（假「语义标题」）。
                    let n = body.chars().find_map(|c| c.to_digit(10)).unwrap_or(0);
                    Ok(format!("语义标题{n}"))
                });
            fut
        });
        let titles = TitleGen::with_runner(store.clone(), runner, Duration::from_secs(2));

        // 轮询：等待 5 个候选中恰 `want` 个已写语义标题。
        async fn wait_titled(store: &Store, want: usize) {
            for _ in 0..250 {
                let mut n = 0;
                for i in 1..=5 {
                    if store
                        .get_session_title(&format!("web:t{i}"))
                        .await
                        .unwrap()
                        .is_some()
                    {
                        n += 1;
                    }
                }
                if n >= want {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("等待 {want} 个语义标题写库超时（5s）");
        }

        // 第一条快照：5 个候选只 spawn 3 个（防风暴），且不含三个不触发样本。
        collect_session_entries(&store, &titles).await;
        wait_titled(&store, 3).await;
        let prompts = calls.lock().unwrap().clone();
        assert_eq!(
            prompts.len(),
            3,
            "每快照至多 3 个，手动名/已有语义标题/无首条的不触发：{prompts:?}"
        );
        assert!(
            prompts
                .iter()
                .all(|p| p.starts_with("任务") && p.ends_with("的首条消息")),
            "全部为候选 conv 的 first_prompt：{prompts:?}"
        );

        // 第二条快照：已生成的 3 个 title 升级为语义标题（网关权威，SPA merge
        // 靠快照带出），并补齐剩余 2 个候选。先定格库内 title 快照（第二批
        // spawn 发生在 collect 内，collect 后写入会造成比较竞态）。
        let mut db_titles: Vec<Option<String>> = Vec::new();
        for i in 1..=5 {
            db_titles.push(store.get_session_title(&format!("web:t{i}")).await.unwrap());
        }
        let entries = collect_session_entries(&store, &titles).await;
        let title_of = |entries: &[SessionEntryFrame], id: &str| {
            entries
                .iter()
                .find(|e| e.id == id)
                .unwrap_or_else(|| panic!("{id} 缺席：{entries:?}"))
                .title
                .clone()
        };
        for (i, expected) in db_titles.iter().enumerate() {
            let id = format!("web:t{}", i + 1);
            match expected {
                Some(t) => assert_eq!(title_of(&entries, &id), *t, "已生成的快照 title = 语义标题"),
                None => assert_eq!(
                    title_of(&entries, &id),
                    format!("任务{}的首条消息", i + 1),
                    "未及生成的仍是机械摘要"
                ),
            }
        }
        assert_eq!(
            title_of(&entries, "web:done"),
            "已有语义标题",
            "已有语义标题直读"
        );
        assert_eq!(title_of(&entries, "web:named"), "手动名", "手动名不受影响");
        wait_titled(&store, 5).await;
        assert_eq!(
            calls.lock().unwrap().len(),
            5,
            "第二条快照补齐剩余 2 个候选"
        );

        // 第三条快照：全部升级，不再有新 spawn（防重复）。
        let entries = collect_session_entries(&store, &titles).await;
        for i in 1..=5 {
            assert_eq!(
                title_of(&entries, &format!("web:t{i}")),
                format!("语义标题{i}"),
                "全部收敛为语义标题"
            );
        }
        assert_eq!(calls.lock().unwrap().len(), 5, "已生成/已登记的不再触发");
    }

    /// 卡片缓存：上限 32 conv 淘汰最旧；快照按 seq 旧→新排序。
    #[test]
    fn card_cache_evicts_oldest_and_snapshots_in_order() {
        let mut cache = CardCache::default();
        let card = || {
            CardPayload::from_card(&OutboundCard {
                text: "t".into(),
                tool_calls: vec![],
                thoughts: vec![],
                todos: vec![],
                phase: CardPhase::Thinking,
                queued_hint: None,
                run_secs: 0,
                usage_display: None,
                task_digest: None,
                terminal: CardTerminal::Running,
            })
        };
        for i in 0..=CARD_CACHE_MAX_CONVS {
            cache.upsert(&format!("web:c{i}"), &format!("web-card-{i}"), card());
        }
        assert_eq!(cache.entries.len(), CARD_CACHE_MAX_CONVS, "超限淘汰");
        assert!(!cache.entries.contains_key("web:c0"), "最旧（c0）被淘汰");
        let snapshot = cache.snapshot();
        let convs: Vec<&str> = snapshot.iter().map(|(c, _, _)| c.as_str()).collect();
        assert_eq!(convs.len(), CARD_CACHE_MAX_CONVS);
        // seq 升序：c1 在 c2 前。
        assert!(
            convs.iter().position(|c| *c == "web:c1").unwrap()
                < convs.iter().position(|c| *c == "web:c2").unwrap()
        );
        // 同 conv 二次 upsert 覆盖（不涨条目）。
        cache.upsert("web:c1", "web-card-again", card());
        assert_eq!(cache.entries.len(), CARD_CACHE_MAX_CONVS);
        assert_eq!(cache.entries.get("web:c1").unwrap().1, "web-card-again");
    }

    // ---- v1.8 出站落盘 + history 泵（outbound_log 读写路径直测：store +
    // 出站 channel 注入，不经网络；帧映射/组帧字节级见 lib.rs 测试）----

    /// 测试用卡片骨架（phase/terminal 可调）。
    fn v18_card(text: &str, terminal: CardTerminal) -> OutboundCard {
        OutboundCard {
            text: text.into(),
            tool_calls: vec![],
            thoughts: vec![],
            todos: vec![],
            phase: CardPhase::Outputting,
            queued_hint: None,
            run_secs: 1,
            usage_display: None,
            task_digest: None,
            terminal,
        }
    }

    /// 等待异步落盘达到目标行数（spawn 的短任务写库，只能轮询观测）。
    async fn wait_outbound_rows(store: &Store, conv: &str, want: usize) -> Vec<OutboundRow> {
        for _ in 0..250 {
            if let Ok(page) = store.list_outbound_since(conv, 0, OUTBOUND_RETAIN).await {
                if page.rows.len() >= want {
                    return page.rows;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("等待 outbound_log 落盘 {want} 行超时（5s）");
    }

    /// 落盘写入点（任务验收项）：text 帧落 kind=text（原文 + at 毫秒）；
    /// card 流式中间帧（state=streaming）不落；card 终态帧（state=done）落
    /// kind=card 且同 messageId 幂等（重复终态只落一次）。
    #[tokio::test]
    async fn outbound_log_persists_text_and_terminal_card_idempotently() {
        let store = test_store().await;
        let p = offline_platform_with_store(store.clone()).await;
        let conv = ConvId("web:log".into());

        // text 出站成功 → 异步落一行 kind=text（payload 含原文与 at）。
        p.send_text(&conv, "你好，世界", &ReplyHint::None)
            .await
            .unwrap();
        let rows = wait_outbound_rows(&store, &conv.0, 1).await;
        assert_eq!(rows[0].kind, "text");
        let payload: serde_json::Value = serde_json::from_str(&rows[0].payload).unwrap();
        assert_eq!(payload["text"], "你好，世界", "落盘取 encode 前原文");
        assert!(
            payload["at"].as_i64().is_some_and(|a| a > 0),
            "at 为 unix 毫秒"
        );

        // 流式中间帧（Running → state=streaming）：不落盘（非终态无 spawn，
        // 此刻 text 行已确认入库，行数不变即证明）。
        let msg_id = p
            .send_card(
                &conv,
                &v18_card("流式中", CardTerminal::Running),
                &ReplyHint::None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .list_outbound_since(&conv.0, 0, OUTBOUND_RETAIN)
                .await
                .unwrap()
                .rows
                .len(),
            1,
            "流式中间帧不应落盘"
        );

        // 终态 patch（Done → state=done）：落一行 kind=card（messageId + 完整
        // CardPayload）。
        p.update_card(
            &conv,
            &msg_id,
            &v18_card("结论", CardTerminal::Done),
            &ReplyHint::None,
        )
        .await
        .unwrap();
        let rows = wait_outbound_rows(&store, &conv.0, 2).await;
        assert_eq!(rows[1].kind, "card");
        let payload: serde_json::Value = serde_json::from_str(&rows[1].payload).unwrap();
        assert_eq!(payload["messageId"], msg_id);
        assert_eq!(payload["card"]["text"], "结论", "CardPayload 完整 JSON");
        assert_eq!(payload["card"]["terminal"], "done");

        // 同 messageId 重复终态（如 sync 后补 patch）：幂等跳过（register 同步
        // 拒绝、无 spawn），仍 2 行。
        p.update_card(
            &conv,
            &msg_id,
            &v18_card("结论2", CardTerminal::Done),
            &ReplyHint::None,
        )
        .await
        .unwrap();
        assert_eq!(
            store
                .list_outbound_since(&conv.0, 0, OUTBOUND_RETAIN)
                .await
                .unwrap()
                .rows
                .len(),
            2,
            "同 messageId 终态只落一次"
        );
        drop(p);
    }

    /// v1.36 at 同源（本批核心）：send_text 一次取值——实时 text 帧携带
    /// `at`（number），落盘行 payload 的 `at` 与实时帧**相等**（SPA 跨两路
    /// 按 (text, at) 精确去重的事实源）。帧侧观察经 outbound capture 夹具，
    /// 行侧经 wait_outbound_rows 轮询（spawn 落库有异步窗口）。
    #[tokio::test]
    async fn text_frame_at_matches_outbound_log_row() {
        let store = test_store().await;
        let (p, mut rx) = offline_platform_with_outbound_capture(store.clone()).await;
        let conv = ConvId("web:at".into());

        p.send_text(&conv, "你好，世界", &ReplyHint::None)
            .await
            .unwrap();

        // 实时帧 JSON 含 at（number）。
        let json = rx.recv().await.expect("实时 text 帧应入出站通道");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "text");
        let at = v["at"]
            .as_u64()
            .expect("实时 text 帧应带 at（unix 毫秒 number）");

        // 落盘行 payload 的 at 与实时帧相等（同源断言）。
        let rows = wait_outbound_rows(&store, &conv.0, 1).await;
        assert_eq!(rows[0].kind, "text");
        let payload: serde_json::Value = serde_json::from_str(&rows[0].payload).unwrap();
        assert_eq!(
            payload["at"].as_u64(),
            Some(at),
            "落盘行 at 必须等于实时帧 at（一次取值同源）"
        );
        drop(p);
    }

    /// 已落盘终态卡片查重集：重复 register 返回 false；per conv 容量上限
    /// （超限整 conv 清空重建——契约允许重复行，清空最坏多落一行）。
    #[test]
    fn logged_card_ids_register_and_capacity() {
        let mut ids = LoggedCardIds::default();
        assert!(ids.register("web:a", "m1"));
        assert!(!ids.register("web:a", "m1"), "重复 register 拒绝");
        assert!(ids.register("web:b", "m1"), "跨 conv 互不影响");
        for i in 0..LOGGED_CARDS_PER_CONV {
            assert!(ids.register("web:a", &format!("n{i}")));
        }
        // 容量满后：老 id 被清掉（可重复 register，语义退化为可能多落一行）、
        // 新 id 仍正常登记。
        assert!(
            ids.register("web:a", "m1"),
            "清空后旧 id 可再登记（best-effort）"
        );
        assert!(ids.register("web:a", "fresh"));
        assert!(!ids.register("web:a", "fresh"));
    }

    // ---- v1.10 push_notify 埋点（mock 观察：替换 push 通道，直测事件触发
    // 与查重；帧字节级与中继侧解析见 lib.rs / relay push.rs 测试）----

    /// push 通道注入的测试夹具：替换平台的 push_tx（子模块直取私有字段，
    /// client 持有的原 rx 随假连接空转），事件可从 test rx 观察。
    async fn offline_platform_with_push_capture() -> (WebPlatform, mpsc::Receiver<PushEvent>) {
        let mut p = offline_platform().await;
        let (tx, rx) = mpsc::channel::<PushEvent>(PUSH_CHANNEL_CAPACITY);
        p.push_tx = tx;
        (p, rx)
    }

    /// 埋点触发矩阵（mock 观察）：流式中间帧不触发；终态 done/error 各触发
    /// round_done/round_error 且 title 取 task_digest；审批触发 approval 且
    /// title=工具名；同 messageId 重复终态不重复触发（与落盘同查重闸）。
    #[tokio::test]
    async fn push_notify_events_on_terminal_and_approval() {
        let (p, mut rx) = offline_platform_with_push_capture().await;
        let conv = ConvId("web:push".into());

        // 流式中间帧：不触发。
        let msg_id = p
            .send_card(
                &conv,
                &v18_card("流式中", CardTerminal::Running),
                &ReplyHint::None,
            )
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err(), "流式中间帧不应触发 push");

        // 终态 done：round_done + title 取 task_digest 优先。
        let mut done_card = v18_card("结论文本", CardTerminal::Done);
        done_card.task_digest = Some("统计 Cargo 依赖".into());
        p.update_card(&conv, &msg_id, &done_card, &ReplyHint::None)
            .await
            .unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("终态应触发 push 事件")
            .expect("通道开着");
        assert_eq!(ev.kind, PushNotifyKind::RoundDone);
        assert_eq!(ev.conv_id, "web:push");
        assert_eq!(ev.title, "统计 Cargo 依赖", "title 取 task_digest");

        // 同 messageId 重复终态：不重复触发（查重闸）。
        p.update_card(
            &conv,
            &msg_id,
            &v18_card("结论2", CardTerminal::Done),
            &ReplyHint::None,
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err(), "同 messageId 终态只推一次");

        // 终态 error：round_error + title 兜底取正文。
        p.send_card(
            &conv,
            &v18_card("炸了", CardTerminal::Error("boom".into())),
            &ReplyHint::None,
        )
        .await
        .unwrap()
        .unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("error 终态应触发")
            .expect("通道开着");
        assert_eq!(ev.kind, PushNotifyKind::RoundError);
        assert_eq!(ev.title, "炸了", "无 task_digest 时取正文");

        // 审批请求：approval + title=工具名。
        p.send_permission_ask(&conv, "req-1", "Bash", "rm -rf /tmp/x", &ReplyHint::None)
            .await
            .unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("审批应触发")
            .expect("通道开着");
        assert_eq!(ev.kind, PushNotifyKind::Approval);
        assert_eq!(ev.conv_id, "web:push");
        assert_eq!(ev.title, "Bash");
        drop(p);
    }

    /// 埋点 title 规整贯通：审批工具名前后空白被 trim（push_notify_title
    /// 在埋点路径生效）。
    #[tokio::test]
    async fn push_notify_ask_title_is_trimmed() {
        let (p, mut rx) = offline_platform_with_push_capture().await;
        p.send_permission_ask(
            &ConvId("web:t".into()),
            "req-2",
            "  WebFetch  ",
            "https://…",
            &ReplyHint::None,
        )
        .await
        .unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("审批应触发")
            .expect("通道开着");
        assert_eq!(ev.title, "WebFetch");
        drop(p);
    }

    /// history_query 处理回帧（任务验收项）：全量（since=0）/ 增量（since=
    /// 水位）/ 空（新会话 entries=[] + lastSeq=0，浏览器只更新水位）。条目
    /// 形态与帧字段逐字断言（字节级契约）；trimmed 分支在此不可达——查询
    /// 上限与 GC 保留同为 500 条，`since 之后超过 500 行` 需保留量 > 500，
    /// 由 store 层（小 limit 参数）与 encode 层（lib.rs）分别钉住。
    #[tokio::test]
    async fn history_query_replies_full_incremental_and_empty() {
        let store = test_store().await;
        let (tx, mut rx) = mpsc::channel::<String>(32);

        // 种子：text / card / text 三行（与生产同路径 append_outbound）。
        store
            .append_outbound("web:h", "text", r#"{"text":"第一条","at":100}"#)
            .await
            .unwrap();
        store
            .append_outbound(
                "web:h",
                "card",
                r#"{"messageId":"web-card-1","card":{"text":"结论","terminal":"done"}}"#,
            )
            .await
            .unwrap();
        store
            .append_outbound("web:h", "text", r#"{"text":"第三条","at":300}"#)
            .await
            .unwrap();

        // 全量（since=0）。
        handle_history_query(TEST_MACHINE, &store, "web:h", 0, &tx).await;
        let v: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "history");
        assert_eq!(v["machine"], TEST_MACHINE, "出站帧带本机 machine");
        assert_eq!(v["convId"], "web:h", "convId 同请求原样");
        assert_eq!(v["lastSeq"], 3);
        assert_eq!(v["trimmed"], false);
        let entries = v["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["kind"], "text");
        assert_eq!(entries[0]["key"], "h:1");
        assert_eq!(entries[0]["text"], "第一条");
        assert_eq!(entries[0]["at"], 100);
        assert_eq!(entries[1]["kind"], "card");
        assert_eq!(entries[1]["key"], "h:2");
        assert_eq!(entries[1]["messageId"], "web-card-1");
        assert_eq!(entries[1]["card"]["terminal"], "done");
        assert_eq!(entries[1]["state"], "done");
        assert_eq!(entries[2]["key"], "h:3", "seq 升序");

        // 增量（since=1）：只回 seq>1 的行，lastSeq 仍报全量水位。
        handle_history_query(TEST_MACHINE, &store, "web:h", 1, &tx).await;
        let v: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        let entries = v["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["key"], "h:2");
        assert_eq!(entries[1]["key"], "h:3");
        assert_eq!(v["lastSeq"], 3);

        // 空增量（since=最大 seq）：entries=[] + lastSeq 照发。
        handle_history_query(TEST_MACHINE, &store, "web:h", 3, &tx).await;
        let v: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(v["entries"].as_array().map(Vec::len), Some(0));
        assert_eq!(v["lastSeq"], 3, "空增量仍报水位");
        assert_eq!(v["trimmed"], false);

        // 新会话（无任何记录）：entries=[] + lastSeq=0。
        handle_history_query(TEST_MACHINE, &store, "web:fresh", 0, &tx).await;
        let v: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(v["entries"].as_array().map(Vec::len), Some(0));
        assert_eq!(v["lastSeq"], 0);
        assert_eq!(v["trimmed"], false);
    }
}
