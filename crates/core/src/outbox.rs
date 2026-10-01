//! 出站可靠性：store 持久化 outbox 的通用交付泵（Wave C：出站可靠性收敛）。
//!
//! ## 背景与设计契约
//!
//! 同一「平台断连」事件，三平台此前的出站行为完全不同——feishu 有 store 持久化
//! outbox（v1.21 泵 + 指数退避，最完善）；wecom 是裸 channel + 5s 超时即报错
//!（断连期发送失败直接丢失）；ilink 只有进程内重试 + 熔断（重启即失忆）。本
//! 模块把 feishu 的泵抽成 core 共享设施 [`OutboxDriver`]，feishu / wecom 各以
//! 自己的 `kind` 接入，行为契约如下（全部继承自 feishu v1.21 泵的已验证语义，
//! 文案与日志逐字保留——用户侧日志过滤不断档）：
//!
//! - **at-least-once**：行落盘后至少尝试交付一次；成功即删行（`mark_sent`）。
//!   交付闭包须自带幂等保护（feishu 透传落盘时的幂等 uuid；wecom 只对「明确
//!   未送达」的失败入队，见 wecom/platform.rs 的取舍注释）。
//! - **退避**：失败后 `next_try = now + 15s × 2^attempts`（封顶 1h）。总计
//!   [`imagent_store::OUTBOX_MAX_ATTEMPTS`]（16）次尝试、约 10h 窗口；超限
//!   删行 + error 留痕（文案过时效 + 表无限膨胀两害取其轻）。
//! - **顺序**：逐行串行交付（前一条未落定不开下一条）；同批内按 store 的
//!   `(next_try, id)` 序——同 conv 行按入队序。**跨批**允许后入队的行越过仍在
//!   退避的旧行（旧行 next_try 未到、新行已到期即先行交付）——与 feishu 现网
//!   语义一致（提示类消息的轻微乱序无害；严格 per-conv FIFO 需按 conv 分组
//!   挡板，会拖慢整体消化，不在本轮范围）。
//! - **有界批次**：每 tick 最多拉 [`BATCH`]（10）条，防单轮重发风暴。
//! - **启动积压**：构造即扫描（先 sleep 一个 tick 再拉）——重启后排空上次
//!   未送达的行，这正是 outbox 存在的意义。
//! - **优雅停机**：`CancellationToken`（建议注入平台停机信号的 child token，
//!   如 wecom 的平台 Drop 信号）；[`Drop`] 亦 cancel——spawn 后 abort 任务即可
//!   停泵。停机只保证不再开始新行/新批，交付中的行让其完成（attempts 不多跳）。
//! - **与 live 发送路径的关系**：outbox 只承接 live 发送**失败后**的兜底重发
//!   （平台侧决定何种失败入队），不参与正常发送；两类路径共用平台各自的发送
//!   原语（退避/限流语义在原语内）。
//!
//! ## 未知 kind 防御（v14 P3o 的泛化）
//!
//! [`Store::due_outbox`] 按 kind 过滤后，无人认领的 kind（平台从 config 撤下、
//! kind 拼写错、未来新 kind 旧进程跑）对 driver 不可见，会永久滞留撑表。本模块
//! 提供 [`sweep_outbox_unknown_kinds`] / [`spawn_unknown_kind_sweeper`]：周期把
//! 不在 `known` 名单（= 当前进程在跑的 driver kind 集合，由 main 装配）的到期行
//! 推后 1h + warn 告警，attempts 递增最终被 [`imagent_store::OUTBOX_MAX_ATTEMPTS`]
//! 回收（推后 + 回收双保险，与 v14 feishu 泵内联防御同构，只是从「单泵顺手」
//! 泛化为「独立清扫者」——kind 过滤后单泵再也看不到别人的行）。
//!
//! ## ilink 不接入的决策（记档）
//!
//! ilink **不接** outbox driver，理由：
//! 1. **协议无交付回执 / 幂等键**——sendmessage 是 fire-and-forget，盲重试有
//!    重复触达风险（feishu 有幂等 uuid、wecom 只对明确未送达入队，ilink 两者
//!    皆无）；
//! 2. **SessionExpired 下跨重启重试无意义**——会话失效后重发必然再失败，白烧
//!    10h 退避窗口；
//! 3. **进程内重试 + 熔断已覆盖瞬态失败**（ratelimit/网络抖动），持久化重试的
//!    边际收益只剩「重启恢复」，而 ilink 的长轮询断连重连本身有退避兜底。
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::Result;
use imagent_store::{OutboxRow, Store};

/// 交付闭包：吃整行（`payload` 为平台自定义 opaque JSON；`id` 供解析失败放弃
/// 时的日志定位），返回成败——Err 触发退避。平台特有逻辑（payload 解析、幂等
/// 键透传、fail-soft 语义）全部留在闭包内，driver 不理解 payload。
pub type OutboxDeliver = Arc<dyn Fn(OutboxRow) -> BoxFuture<'static, Result<()>> + Send + Sync>;

/// 每 tick 最多拉取的到期行数（防单轮重发风暴；feishu v1.21 原值）。
const BATCH: u32 = 10;
/// 默认轮询间隔（feishu v1.21 原值；测试用 [`OutboxDriver::with_tick`] 调小）。
const TICK: Duration = Duration::from_secs(10);

/// tracing 宏的 `target:` 要求编译期常量（`Metadata::new` 是 const fn，运行期
/// 变量过不了宏展开），而泵的日志 target 由平台在构造时注入。按已知平台名
/// 分派到字面量分支；未知名回落 `imagent::outbox`。**接入新平台时在此加一
/// 行分支**（否则其泵日志会落到 imagent::outbox 而非平台名下——可观测但不
/// 聚合到平台过滤）。
macro_rules! tlog {
    (info, $t:expr, $($rest:tt)+) => {
        match $t {
            "feishu" => tracing::info!(target: "feishu", $($rest)+),
            "wecom" => tracing::info!(target: "wecom", $($rest)+),
            _ => tracing::info!(target: "imagent::outbox", $($rest)+),
        }
    };
    (warn, $t:expr, $($rest:tt)+) => {
        match $t {
            "feishu" => tracing::warn!(target: "feishu", $($rest)+),
            "wecom" => tracing::warn!(target: "wecom", $($rest)+),
            _ => tracing::warn!(target: "imagent::outbox", $($rest)+),
        }
    };
    (error, $t:expr, $($rest:tt)+) => {
        match $t {
            "feishu" => tracing::error!(target: "feishu", $($rest)+),
            "wecom" => tracing::error!(target: "wecom", $($rest)+),
            _ => tracing::error!(target: "imagent::outbox", $($rest)+),
        }
    };
}

/// store 持久化 outbox 的交付泵。每 [`TICK`] 拉本 kind 的到期行，逐条经
/// `deliver` 交付：成功删行、失败退避（超上限丢弃 + error）。构造与用法见
/// [模块文档](self)；一个 driver 一个 kind，平台侧在构造函数里 spawn。
pub struct OutboxDriver {
    store: Store,
    kind: &'static str,
    deliver: OutboxDeliver,
    /// 日志 target：平台侧传入自己的名字（"feishu" / "wecom"）——泵的日志文案
    /// 逐字继承 feishu v1.21，用户已有的 `target: "feishu"` 过滤不断档。
    log_target: &'static str,
    tick: Duration,
    shutdown: CancellationToken,
}

impl OutboxDriver {
    /// 构造（默认 10s tick、自持停机 token）。用 [`with_tick`] / [`with_shutdown`]
    /// 定制后 [`spawn`] 或裸 `run`。
    pub fn new(
        store: Store,
        kind: &'static str,
        deliver: OutboxDeliver,
        log_target: &'static str,
    ) -> Self {
        Self {
            store,
            kind,
            deliver,
            log_target,
            tick: TICK,
            shutdown: CancellationToken::new(),
        }
    }

    /// 测试钩子：调小轮询间隔（生产路径不传——退避语义按墙钟 next_try，tick
    /// 只影响「多久看一眼」，不影响退避时长）。
    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    /// 注入外部停机信号——替换自持 token。**建议传父 token 的
    /// `child_token()`**（如 wecom 平台 Drop 信号）：父 cancel 连动停泵，而泵
    /// 自身退出（含 [`Drop`] 的 cancel）不反向连动平台——泵与平台的生命周期
    /// 是单向包含关系，共享同一 token 会让泵的先行退出误杀平台后台任务。
    pub fn with_shutdown(mut self, shutdown: CancellationToken) -> Self {
        self.shutdown = shutdown;
        self
    }

    /// 停机信号（spawn 前需要显式控制时取 clone）。
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// 后台拉起泵（detached；abort 句柄任务或 cancel token 停机）。
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run())
    }

    /// 常驻泵主循环（feishu v1.21 outbox_pump 的忠实移植 + 停机通路）。
    pub async fn run(self) {
        let shutdown = self.shutdown.clone();
        let t = self.log_target;
        tlog!(info, t, kind = self.kind, "outbox 泵启动");
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tlog!(info, t, kind = self.kind, "outbox 泵收到停机信号，退出");
                    return;
                }
                _ = tokio::time::sleep(self.tick) => {}
            }
            let now = unix_now();
            let due = match self.store.due_outbox(self.kind, now, BATCH).await {
                Ok(d) => d,
                Err(e) => {
                    tlog!(warn, t, error = %e, "outbox 拉取失败（本轮跳过）");
                    continue;
                }
            };
            for row in due {
                // 停机检查点：批间也及时退出（当前行让其完成，attempts 不多跳）。
                if shutdown.is_cancelled() {
                    tlog!(
                        info,
                        t,
                        kind = self.kind,
                        "outbox 泵批中途收到停机信号，退出"
                    );
                    return;
                }
                let res = (self.deliver)(row.clone()).await;
                match res {
                    Ok(()) => {
                        tlog!(
                            info,
                            t,
                            id = row.id,
                            conv_id = row.conv,
                            attempts = row.attempts,
                            "outbox 重发成功"
                        );
                        let _ = self.store.outbox_mark_sent(row.id).await;
                    }
                    Err(e) => {
                        // 15s × 2^attempts，封顶 1h。
                        let backoff = backoff_secs(row.attempts);
                        let kept = self.store.outbox_mark_failed(row.id, now + backoff).await;
                        match kept {
                            Ok(true) => {
                                tlog!(
                                    warn, t, id = row.id, attempts = row.attempts, error = %e,
                                    "outbox 重发失败（退避后再试）"
                                )
                            }
                            Ok(false) => {
                                tlog!(
                                    error,
                                    t,
                                    id = row.id,
                                    attempts = row.attempts,
                                    conv_id = row.conv,
                                    "outbox 重试耗尽，放弃（提示丢失）"
                                )
                            }
                            Err(e2) => {
                                tlog!(warn, t, id = row.id, error = %e2, "outbox 状态更新失败")
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Drop 即 cancel 停机信号——覆盖两类路径：① 构造后未 run 即丢弃；② `run`
/// future 被 abort（spawn 句柄 abort / runtime 关闭）时随 future 一起 drop。
/// 传入 [`with_shutdown`] 的是 child token 时（推荐形态），cancel 只及泵自身、
/// 不反向连动平台的父 token——不出现孤儿泵，也不会误杀平台。
impl Drop for OutboxDriver {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// 失败退避秒数：`15 × 2^attempts`，封顶 1h（feishu v1.21 原公式；`min(8)`
/// 防 attempts 大值位移溢出）。纯函数便于单测。
fn backoff_secs(attempts: i64) -> i64 {
    (15i64 << attempts.clamp(0, 8)).clamp(15, 3600)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// sweeper 轮询间隔：远小于推后时长（1h），保证推后到期的行在一个周期内被
/// 再看一眼；比 driver tick 长得多（未知 kind 是冷路径，不值得高频扫）。
const SWEEP_TICK: Duration = Duration::from_secs(600);

/// 单趟清扫（`spawn_unknown_kind_sweeper` 的一轮，独立暴露便于测试）：把不在
/// `known` 名单的到期行推后 1h + warn；attempts 递增，超
/// [`imagent_store::OUTBOX_MAX_ATTEMPTS`] 时行被 `outbox_mark_failed` 删除
///（推后 + 回收双保险）。
pub async fn sweep_outbox_unknown_kinds(store: &Store, known: &[String]) {
    let now = unix_now();
    let unknown = match store.due_outbox_unknown_kinds(known, now, 100).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(target: "imagent::outbox", error = %e, "未知 kind 清扫拉取失败（本轮跳过）");
            return;
        }
    };
    for row in unknown {
        warn!(
            target: "imagent::outbox", id = row.id, kind = %row.kind,
            "outbox 未知 kind（无在跑 driver 认领），推后 1h 重看（超限自动回收）"
        );
        match store.outbox_mark_failed(row.id, now + 3600).await {
            Ok(true) | Err(_) => {}
            Ok(false) => {
                error!(
                    target: "imagent::outbox", id = row.id, kind = %row.kind,
                    "outbox 未知 kind 行重试耗尽，回收删除"
                )
            }
        }
    }
}

/// 后台拉起未知 kind 清扫者。`known` = 当前进程在跑的 driver kind 集合（main
/// 按 `--platform` 装配；如 feishu → `["feishu_text"]`）。进程生命周期常驻。
pub fn spawn_unknown_kind_sweeper(store: Store, known: &[&str]) {
    let known: Vec<String> = known.iter().map(|s| s.to_string()).collect();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP_TICK).await;
            sweep_outbox_unknown_kinds(&store, &known).await;
        }
    });
}

#[cfg(test)]
mod tests {
    //! driver / sweeper 行为测试：真 store（临时库）+ mock 交付闭包。退避按
    //! 墙钟 next_try 生效，tick 调成毫秒级让「到期拉取」快转；退避窗口本身
    //! 不压缩（那是要验证的语义，不是要绕过的等待）。

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    fn temp_db_path(name: &str) -> PathBuf {
        let pid = std::process::id();
        let mut p = std::env::temp_dir();
        p.push(format!("imagent_core_outbox_test_{pid}_{name}.db"));
        p
    }

    struct TempDb(PathBuf);

    impl TempDb {
        async fn new(name: &str) -> Self {
            let path = temp_db_path(name);
            for ext in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
            }
            Self(path)
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for ext in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{ext}", self.0.display()));
            }
        }
    }

    /// 轮询断言：异步谓词在 `timeout` 内变 true（CI 慢机余量），否则 panic。
    async fn eventually<F, Fut>(timeout: Duration, pred: F)
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if pred().await {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "条件在 {timeout:?} 内未满足"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn ok_deliver(seen: Arc<AtomicUsize>) -> OutboxDeliver {
        Arc::new(move |row| {
            seen.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let _ = row; // 不关心内容，计数即交付
                Ok(())
            })
        })
    }

    fn fail_deliver(calls: Arc<AtomicUsize>) -> OutboxDeliver {
        Arc::new(move |row| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let _ = row;
                Err(crate::CoreError::Platform("test", "mock 交付失败".into()))
            })
        })
    }

    /// 快速把一行 attempts 顶到 `n`（mark_failed 的 next_try 传当前秒，保持
    /// 行持续到期）——退避语义由 store 层测试覆盖，这里只造前置状态。
    async fn bump_attempts(store: &Store, id: i64, n: i64) {
        for _ in 0..n {
            let now = unix_now();
            assert!(store.outbox_mark_failed(id, now).await.unwrap());
        }
    }

    /// 基本生命周期：入队（含启动前积压）→ 泵拉取 → 交付成功 → 删行。
    #[tokio::test]
    async fn driver_delivers_backlog_and_deletes_row() {
        let db = TempDb::new("backlog").await;
        let store = Store::open(&db.0).await.unwrap();
        store
            .enqueue_outbox("feishu:a", "feishu_text", r#"{"text":"hi"}"#)
            .await
            .unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let driver = OutboxDriver::new(
            store.clone(),
            "feishu_text",
            ok_deliver(seen.clone()),
            "feishu",
        )
        .with_tick(Duration::from_millis(20));
        driver.spawn();
        eventually(Duration::from_secs(5), || async {
            seen.load(Ordering::SeqCst) >= 1
        })
        .await;
        eventually(Duration::from_secs(5), || async {
            store.outbox_depth().await.unwrap() == 0
        })
        .await;
    }

    /// 失败退避：首败后行保留（attempts=1）且退避窗口内不再重复交付——
    /// next_try 后移 15s，毫秒 tick 也不会把未到期行再拉出来。
    #[tokio::test]
    async fn driver_backs_off_on_failure() {
        let db = TempDb::new("backoff").await;
        let store = Store::open(&db.0).await.unwrap();
        store
            .enqueue_outbox("wecom:A", "wecom_text", r#"{"chunks":[]}"#)
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        OutboxDriver::new(
            store.clone(),
            "wecom_text",
            fail_deliver(calls.clone()),
            "wecom",
        )
        .with_tick(Duration::from_millis(20))
        .spawn();
        eventually(Duration::from_secs(5), || async {
            calls.load(Ordering::SeqCst) >= 1
        })
        .await;
        // 等若干个 tick（远小于 15s 退避）：调用次数应停在 1。
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "退避窗口（15s）内不得重试");
        // 行保留且退避中：当前时刻不可见，next_try（now+15s）之后可见且
        // attempts=1。
        assert_eq!(store.outbox_depth().await.unwrap(), 1, "首败后行保留");
        let due = store
            .due_outbox("wecom_text", unix_now() + 16, 10)
            .await
            .unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].attempts, 1, "首败后 attempts=1");
    }

    /// 超限丢弃：attempts 顶到 MAX-1 后由 driver 交付最后一次失败 → 行被
    /// 回收删除（error 留痕路径）。
    #[tokio::test]
    async fn driver_drops_row_after_max_attempts() {
        let db = TempDb::new("exhaust").await;
        let store = Store::open(&db.0).await.unwrap();
        store
            .enqueue_outbox("wecom:A", "wecom_text", "{}")
            .await
            .unwrap();
        let row = store
            .due_outbox("wecom_text", unix_now(), 10)
            .await
            .unwrap()
            .remove(0);
        bump_attempts(&store, row.id, imagent_store::OUTBOX_MAX_ATTEMPTS - 1).await;
        let calls = Arc::new(AtomicUsize::new(0));
        OutboxDriver::new(
            store.clone(),
            "wecom_text",
            fail_deliver(calls.clone()),
            "wecom",
        )
        .with_tick(Duration::from_millis(20))
        .spawn();
        eventually(Duration::from_secs(5), || async {
            calls.load(Ordering::SeqCst) >= 1
        })
        .await;
        eventually(Duration::from_secs(5), || async {
            store.outbox_depth().await.unwrap() == 0
        })
        .await;
    }

    /// per-conv 顺序：同一 conv 的多行按 id 序交付，且前一条未返回后一条不
    /// 开始（串行循环保证）；不同 conv 的行在同一批里穿插也无妨。
    #[tokio::test]
    async fn driver_delivers_same_conv_in_id_order_serially() {
        let db = TempDb::new("order").await;
        let store = Store::open(&db.0).await.unwrap();
        // 同 conv 三条 + 另一 conv 两条，全部立即到期（next_try=now）。
        for (conv, text) in [
            ("feishu:a", "a1"),
            ("feishu:b", "b1"),
            ("feishu:a", "a2"),
            ("feishu:b", "b2"),
            ("feishu:a", "a3"),
        ] {
            store
                .enqueue_outbox(conv, "feishu_text", &format!(r#"{{"text":"{text}"}}"#))
                .await
                .unwrap();
        }
        let order: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let rec = order.clone();
        let deliver: OutboxDeliver = Arc::new(move |row| {
            let rec = rec.clone();
            Box::pin(async move {
                // 小延迟放大并行度——若 driver 并发交付，顺序断言会露馅。
                tokio::time::sleep(Duration::from_millis(15)).await;
                let v: serde_json::Value = serde_json::from_str(&row.payload).unwrap();
                rec.lock()
                    .unwrap()
                    .push(v.get("text").and_then(|t| t.as_str()).unwrap().to_string());
                Ok(())
            })
        });
        OutboxDriver::new(store.clone(), "feishu_text", deliver, "feishu")
            .with_tick(Duration::from_millis(20))
            .spawn();
        eventually(Duration::from_secs(5), || async {
            order.lock().unwrap().len() >= 5
        })
        .await;
        let got = order.lock().unwrap().clone();
        let a: Vec<&str> = got
            .iter()
            .filter(|t| t.starts_with('a'))
            .map(String::as_str)
            .collect();
        let b: Vec<&str> = got
            .iter()
            .filter(|t| t.starts_with('b'))
            .map(String::as_str)
            .collect();
        assert_eq!(a, vec!["a1", "a2", "a3"], "同 conv 按入队（id）序");
        assert_eq!(b, vec!["b1", "b2"], "同 conv 按入队（id）序");
    }

    /// 优雅停机：cancel token 后 run 返回（不残留行处理）。
    #[tokio::test]
    async fn driver_stops_on_cancel() {
        let db = TempDb::new("shutdown").await;
        let store = Store::open(&db.0).await.unwrap();
        let token = CancellationToken::new();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let g = gate.clone();
        // 慢闭包：挂在 permit 上，验证「交付中的行完成后才退」不被强中断。
        let deliver: OutboxDeliver = Arc::new(move |row| {
            let g = g.clone();
            Box::pin(async move {
                let _ = row;
                let _ = g.acquire().await;
                Ok(())
            })
        });
        store
            .enqueue_outbox("wecom:A", "wecom_text", "{}")
            .await
            .unwrap();
        let driver = OutboxDriver::new(store, "wecom_text", deliver, "wecom")
            .with_tick(Duration::from_millis(10))
            .with_shutdown(token.clone());
        let handle = driver.spawn();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!handle.is_finished(), "泵应在运行");
        token.cancel();
        // 放行慢闭包后 run 才能退（当前行完成语义）。
        gate.close();
        eventually(Duration::from_secs(5), || async { handle.is_finished() }).await;
    }

    /// sweeper：未知 kind 的到期行被推后（attempts 递增），已知 kind 不动。
    #[tokio::test]
    async fn sweeper_defers_unknown_kinds_only() {
        let db = TempDb::new("sweep").await;
        let store = Store::open(&db.0).await.unwrap();
        store
            .enqueue_outbox("feishu:a", "feishu_text", "{}")
            .await
            .unwrap();
        store
            .enqueue_outbox("legacy:x", "old_kind", "{}")
            .await
            .unwrap();
        sweep_outbox_unknown_kinds(
            &store,
            &["feishu_text".to_string(), "wecom_text".to_string()],
        )
        .await;
        // 已知 kind：原样（attempts=0）。
        let known = store
            .due_outbox("feishu_text", unix_now(), 10)
            .await
            .unwrap();
        assert_eq!(known.len(), 1);
        assert_eq!(known[0].attempts, 0);
        // 未知 kind：attempts=1、next_try 推后 1h（本轮不可见）。
        assert!(store
            .due_outbox("old_kind", unix_now(), 10)
            .await
            .unwrap()
            .is_empty());
        let later = store
            .due_outbox("old_kind", unix_now() + 3601, 10)
            .await
            .unwrap();
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].attempts, 1);
    }

    /// 退避公式：15s 起步、指数、封顶 1h；负 attempts 钳到 0。
    #[test]
    fn backoff_formula() {
        assert_eq!(backoff_secs(0), 15);
        assert_eq!(backoff_secs(1), 30);
        assert_eq!(backoff_secs(2), 60);
        assert_eq!(backoff_secs(7), 15 << 7, "attempts=7 未触顶（1920s）");
        assert_eq!(backoff_secs(8), 3600, "attempts=8 起封顶（3840 → 1h）");
        assert_eq!(backoff_secs(16), 3600, "attempts 上限 16 时已封顶");
        assert_eq!(backoff_secs(-3), 15, "负值防御");
    }
}
