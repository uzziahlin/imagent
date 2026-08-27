use super::*;
use crate::types::{ConvId, LocalSession, Mention, ReplyHint, SessionId, UserId};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;

#[cfg(unix)]
#[tokio::test]
async fn read_line_capped_rejects_oversized() {
    // P1-9：超过上限（无换行）→ Err，防同 uid 进程发巨大行 OOM。
    let bytes: Vec<u8> = vec![b'x'; 1000];
    let mut reader = tokio::io::BufReader::new(&bytes[..]);
    let res = Dispatcher::read_line_capped(&mut reader, 100).await;
    assert!(res.is_err(), "oversized line must error");
}

#[cfg(unix)]
#[tokio::test]
async fn read_line_capped_reads_normal_line() {
    let bytes: &[u8] = b"{\"conv_id\":\"c1\"}\nextra";
    let mut reader = tokio::io::BufReader::new(bytes);
    let line = Dispatcher::read_line_capped(&mut reader, 1024)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(line, "{\"conv_id\":\"c1\"}\n");
}

#[tokio::test]
async fn conv_lock_released_on_backend_failure() {
    // P1-7：backend.run 失败时，handle 的失败 return 应释放 conv_lock，
    // 不在 conv_locks 留泄漏项。
    let _g = SERIAL.lock().await;
    let auth = Auth::new(vec!["u1".into()]);
    let (plat, inbox, send_count) = MockPlatform::new();
    let (back, _calls, _prompts, _order) = MockBackend::new_failing();
    let (store, _db) = tmp_store().await;
    let admins = auth.snapshot();
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        auth,
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into()],
        PermissionMode::Off,
        test_budgets(),
        CotDetail::Brief,
        admins,
    ));
    disp.handle(msg("c1", "u1", "hello")).await;
    // 失败路径 release 后，conv_locks 应为空（c1 已被移除，非永久泄漏）。
    let map = disp.conv_locks.lock().await;
    assert!(
        map.is_empty(),
        "conv_locks 应在 backend 失败后为空，残留: {:?}",
        map.keys().collect::<Vec<_>>()
    );
    drop(inbox);
    drop(send_count);
}

/// 串行化 dispatch 集成测试：避免并行开 /tmp WAL sqlite 触发 SQLITE_IOERR(1802)。
static SERIAL: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));
#[test]
fn is_session_expired_err_classifies() {
    use crate::error::CoreError;
    // SessionExpired variant 命中。
    assert!(is_session_expired_err(&CoreError::SessionExpired(
        "re-login required".into()
    )));
    assert!(is_session_expired_err(&CoreError::SessionExpired(
        "please re-login".into()
    )));
    // 其它 variant 不命中。
    assert!(!is_session_expired_err(&CoreError::Platform(
        "ilink",
        "getupdates exhausted retries".into()
    )));
    assert!(!is_session_expired_err(&CoreError::Config("bad".into())));
    assert!(!is_session_expired_err(&CoreError::Store(
        imagent_store::StoreError::Other("db: some failure".into())
    )));
    assert!(!is_session_expired_err(&CoreError::Platform(
        "ilink",
        "Session Expired".into()
    )));
}

type InboxHandle = Arc<TokioMutex<Vec<String>>>;
type CounterHandle = Arc<AtomicUsize>;
type CallsHandle = Arc<TokioMutex<Vec<Option<String>>>>;
type PromptsHandle = Arc<TokioMutex<Vec<String>>>;

// ---------- mock platform ----------

/// mock platform：`inbox` 收到的出站文本，`recv_queue` 可编程的入站流。
struct MockPlatform {
    recv_queue: Arc<TokioMutex<Option<Vec<InboundMessage>>>>,
    inbox: Arc<TokioMutex<Vec<String>>>,
    send_count: Arc<AtomicUsize>,
}

impl MockPlatform {
    fn new() -> (Self, InboxHandle, CounterHandle) {
        let inbox = Arc::new(TokioMutex::new(Vec::new()));
        let send_count = Arc::new(AtomicUsize::new(0));
        let p = Self {
            recv_queue: Arc::new(TokioMutex::new(None)),
            inbox: inbox.clone(),
            send_count: send_count.clone(),
        };
        (p, inbox, send_count)
    }
}

#[async_trait]
impl Platform for MockPlatform {
    async fn recv(&self) -> Result<InboundMessage> {
        loop {
            let mut q = self.recv_queue.lock().await;
            if let Some(list) = q.as_mut() {
                if !list.is_empty() {
                    return Ok(list.remove(0));
                }
            }
            drop(q);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    async fn send_text(&self, _conv: &ConvId, text: &str, _hint: &ReplyHint) -> Result<()> {
        self.inbox.lock().await.push(text.to_string());
        self.send_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn send_media(
        &self,
        _conv: &ConvId,
        media: &crate::types::MediaRef,
        _hint: &ReplyHint,
    ) -> Result<()> {
        // 以 [media:<url>] 记入 inbox，供 /img 等测试断言回传内容。
        self.inbox
            .lock()
            .await
            .push(format!("[media:{}]", media.url));
        self.send_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn name(&self) -> &'static str {
        "mock"
    }
}

// ---------- mock backend ----------

struct MockBackend {
    calls: Arc<TokioMutex<Vec<Option<String>>>>,
    prompts: Arc<TokioMutex<Vec<String>>>,
    order: Arc<AtomicUsize>,
    /// 每次 run 前发这些 ToolUse chunk（用于工具摘要测试）。默认空。
    tools_to_emit: Arc<TokioMutex<Vec<(String, String)>>>,
    /// run 直接返 Err（P1-7 失败路径测试用）。
    fail: bool,
    /// 本次 run 构造的 RunOutcome.terminal 值（默认 true = 正常终止）。
    terminal: bool,
    /// run 记录后先 sleep 该时长（P4 /stop、批处理、空闲看门狗测试用）。
    slow_ms: u64,
    /// P5-5：run 开跑即发 SessionStarted chunk（模拟 CLI 首事件带 session id），
    /// 供 /stop 中断路径的 session 持久化测试用。
    announce_session: Option<String>,
    /// P5-10：流式模式——逐段发 Text，Final/RunOutcome 为全量拼接（模拟
    /// codex/gemini/ACP「中间 Text + Final 全量」语义，去重测试用）。默认空。
    stream_texts: Vec<String>,
    /// P5-第五批：announce session 后直接返 Err（Err 路径 session 持久化测试用）。
    fail_after_announce: Option<String>,
    /// `list_local_sessions` 返回的本机会话（P4-11 统一 /resume 测试用）。
    local_sessions: Arc<TokioMutex<Vec<LocalSession>>>,
}

impl MockBackend {
    fn new() -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
        let calls = Arc::new(TokioMutex::new(Vec::new()));
        let prompts = Arc::new(TokioMutex::new(Vec::new()));
        let order = Arc::new(AtomicUsize::new(0));
        let b = Self {
            calls: calls.clone(),
            prompts: prompts.clone(),
            order: order.clone(),
            tools_to_emit: Arc::new(TokioMutex::new(Vec::new())),
            fail: false,
            terminal: true,
            slow_ms: 0,
            announce_session: None,
            stream_texts: Vec::new(),
            fail_after_announce: None,
            local_sessions: Arc::new(TokioMutex::new(Vec::new())),
        };
        (b, calls, prompts, order)
    }

    /// 返回带可配置 ToolUse 发射的 backend，以及设置 tool 列表的句柄。
    async fn new_with_tools(
        tools: Vec<(String, String)>,
    ) -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
        let (b, calls, prompts, order) = Self::new();
        *b.tools_to_emit.lock().await = tools;
        (b, calls, prompts, order)
    }

    /// run 直接返 Err（P1-7 失败路径测试用）。
    fn new_failing() -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
        let (mut b, calls, prompts, order) = Self::new();
        b.fail = true;
        (b, calls, prompts, order)
    }
    /// run 返回 terminal=false（模拟 agent 崩溃后的部分输出，R1 告警测试用）。
    fn new_non_terminal() -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
        let (mut b, calls, prompts, order) = Self::new();
        b.terminal = false;
        (b, calls, prompts, order)
    }
    /// run 记录后挂起 slow_ms（P4 /stop、批处理合并、空闲看门狗测试用）。
    fn new_slow(slow_ms: u64) -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
        let (mut b, calls, prompts, order) = Self::new();
        b.slow_ms = slow_ms;
        (b, calls, prompts, order)
    }
    /// P5-5：慢后端 + 开跑即 announce session id（/stop 中断保 session 测试用）。
    fn new_slow_with_session(
        slow_ms: u64,
        sid: &str,
    ) -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
        let (mut b, calls, prompts, order) = Self::new();
        b.slow_ms = slow_ms;
        b.announce_session = Some(sid.into());
        (b, calls, prompts, order)
    }
    /// P5-第五批：announce session 后返 Err（Err 路径持久化测试用）。
    fn new_announce_then_fail(sid: &str) -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
        let (mut b, calls, prompts, order) = Self::new();
        b.announce_session = Some(sid.into());
        b.fail_after_announce = Some(sid.into());
        (b, calls, prompts, order)
    }
    /// P5-10：流式后端——逐段 Text + Final/RunOutcome 全量（去重测试用）。
    fn new_streaming(texts: Vec<String>) -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
        let (mut b, calls, prompts, order) = Self::new();
        b.stream_texts = texts;
        (b, calls, prompts, order)
    }
    /// `list_local_sessions` 返回固定本机会话列表（P4-11 统一 /resume 测试用）。
    async fn new_with_local(
        local: Vec<LocalSession>,
    ) -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
        let (b, calls, prompts, order) = Self::new();
        *b.local_sessions.lock().await = local;
        (b, calls, prompts, order)
    }
}

#[async_trait]
impl Backend for MockBackend {
    async fn run(
        &self,
        _conv_id: &str,
        prompt: &str,
        session: Option<&SessionId>,
        _workdir: &std::path::Path,
        _allowed_tools: &[String],
        chunks: mpsc::Sender<AgentChunk>,
    ) -> Result<crate::types::RunOutcome> {
        // P1-7 测试：fail 模式直接返 Err，触发 handle 失败路径。
        if self.fail {
            return Err(crate::error::CoreError::Backend(
                "mock-backend",
                "mock failure (fail=true)".into(),
            ));
        }
        // 记录续接情况 + 执行顺序 + 收到的 prompt。
        let my_order = self.order.fetch_add(1, Ordering::SeqCst);
        self.calls.lock().await.push(session.map(|s| s.0.clone()));
        self.prompts.lock().await.push(prompt.to_string());

        // P5-5：开跑即 announce（模拟 CLI 首事件带 session id）。
        if let Some(sid) = &self.announce_session {
            let _ = chunks.send(AgentChunk::SessionStarted(sid.clone())).await;
        }
        // P5-第五批：announce 后失败模式（Err 路径持久化测试）。
        if let Some(sid) = &self.fail_after_announce {
            return Err(crate::error::CoreError::Backend(
                "mock-backend",
                format!("mock failure after announce (sid={sid})"),
            ));
        }

        // 稍微让出调度器，便于测试串行。
        tokio::task::yield_now().await;

        // P4：慢后端——记录后挂起（不发任何 chunk），供 /stop、批处理、空闲
        // 看门狗测试制造「在飞任务」窗口。
        if self.slow_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.slow_ms)).await;
        }

        // 先发配置好的 ToolUse chunk（若有），再发 Final。
        let tools = self.tools_to_emit.lock().await.clone();
        for (tool, input) in tools {
            let _ = chunks.send(AgentChunk::ToolUse { tool, input }).await;
        }
        // P5-10：流式模式——逐段 Text，Final/RunOutcome 为全量拼接。
        let mut full = String::new();
        for t in &self.stream_texts {
            full.push_str(t);
            let _ = chunks.send(AgentChunk::Text(t.clone())).await;
        }
        // 发一个 Final chunk（流式模式 = 全量；否则沿用 reply#N 供既有断言）。
        let final_chunk = if self.stream_texts.is_empty() {
            format!("reply#{my_order}")
        } else {
            full.clone()
        };
        let _ = chunks.send(AgentChunk::Final(final_chunk)).await;

        let outcome_final = if self.stream_texts.is_empty() {
            format!("final-{my_order}")
        } else {
            full
        };
        Ok(crate::types::RunOutcome {
            session_id: SessionId(format!("sess-{my_order}")),
            final_text: outcome_final,
            terminal: self.terminal,
        })
    }
    fn name(&self) -> &'static str {
        "mock-backend"
    }
    async fn list_local_sessions(&self, _workdir: &std::path::Path) -> Vec<LocalSession> {
        self.local_sessions.lock().await.clone()
    }
}

// ---------- helpers ----------

fn msg(conv: &str, sender: &str, text: &str) -> InboundMessage {
    InboundMessage {
        conv_id: ConvId(conv.into()),
        sender: UserId(sender.into()),
        text: Some(text.into()),
        media: Vec::new(),
        media_errors: Vec::new(),
        mentions: Vec::new(),
        mentioned_bot: false,
        ask_req: None,
        reply_to: None,
        reply_hint: ReplyHint::None,
    }
}

async fn tmp_store() -> (Store, std::path::PathBuf) {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "imagent_core_dispatch_{}_{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = Store::open(&p).await.expect("open store");
    (store, p)
}

/// 构造 dispatcher 并返回各观测句柄。`check()` 每次返回一个指向同一 db 文件的
/// 新 Store 连接（Store 未 impl Clone；rusqlite WAL 支持多连接，断言用）。
struct Ctx {
    disp: Arc<Dispatcher>,
    inbox: Arc<TokioMutex<Vec<String>>>,
    send_count: Arc<AtomicUsize>,
    calls: Arc<TokioMutex<Vec<Option<String>>>>,
    prompts: Arc<TokioMutex<Vec<String>>>,
    order: Arc<AtomicUsize>,
    db: std::path::PathBuf,
}

impl Ctx {
    async fn check(&self) -> Store {
        Store::open(&self.db).await.expect("reopen store")
    }
}

async fn build(auth: Auth) -> Ctx {
    build_with_workdir(auth, std::path::PathBuf::from("/tmp/imagent-test-ws")).await
}

/// 与 build 相同但可指定 default_workdir（/img 等需真实文件系统的测试用）。
async fn build_with_workdir(auth: Auth, default_workdir: std::path::PathBuf) -> Ctx {
    let (plat, inbox, send_count) = MockPlatform::new();
    let (back, calls, prompts, order) = MockBackend::new();
    let (store, db) = tmp_store().await;

    // S2：admin_senders 空 = 无人是管理员——测试默认把白名单 sender 全设为
    // admin，保持既有用例（alice /allow 等）语义不变。
    let admins = auth.snapshot();
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        auth,
        default_workdir,
        vec!["Read".into(), "Edit".into()],
        PermissionMode::Off,
        test_budgets(),
        CotDetail::Brief,
        admins,
    ));

    Ctx {
        disp,
        inbox,
        send_count,
        calls,
        prompts,
        order,
        db,
    }
}
/// 与 build 相同，但 MockBackend 返回 terminal=false（R1 非正常退出告警测试用）。
async fn build_non_terminal(auth: Auth) -> Ctx {
    let (plat, inbox, send_count) = MockPlatform::new();
    let (back, calls, prompts, order) = MockBackend::new_non_terminal();
    let (store, db) = tmp_store().await;

    let admins = auth.snapshot();
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        auth,
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into(), "Edit".into()],
        PermissionMode::Off,
        test_budgets(),
        CotDetail::Brief,
        admins,
    ));

    Ctx {
        disp,
        inbox,
        send_count,
        calls,
        prompts,
        order,
        db,
    }
}

/// P2-D：与 build 相同但允许指定 admin_senders（测试角色区分）。
async fn build_with_admin(auth: Auth, admin_senders: Vec<String>) -> Ctx {
    let (plat, inbox, send_count) = MockPlatform::new();
    let (back, calls, prompts, order) = MockBackend::new();
    let (store, db) = tmp_store().await;
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        auth,
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into(), "Edit".into()],
        PermissionMode::Off,
        test_budgets(),
        CotDetail::Brief,
        admin_senders,
    ));
    Ctx {
        disp,
        inbox,
        send_count,
        calls,
        prompts,
        order,
        db,
    }
}

/// 与 build 相同，但 MockBackend 在 Final 前会发指定的 ToolUse chunk。
async fn build_with_tools(auth: Auth, tools: Vec<(String, String)>) -> Ctx {
    let (plat, inbox, send_count) = MockPlatform::new();
    let (back, calls, prompts, order) = MockBackend::new_with_tools(tools).await;
    let (store, db) = tmp_store().await;

    let admins = auth.snapshot();
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        auth,
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into(), "Edit".into()],
        PermissionMode::Off,
        test_budgets(),
        CotDetail::Brief,
        admins, // S2：测试默认白名单全员 = admin
    ));

    Ctx {
        disp,
        inbox,
        send_count,
        calls,
        prompts,
        order,
        db,
    }
}

/// 测试默认预算：与线上默认一致，唯批处理窗口收窄到 1ms（不拖慢顺序喂消息的
/// 既有用例）。需要窗口/看门狗/慢后端的用例走 [`build_slow`]。
fn test_budgets() -> TaskBudgets {
    TaskBudgets {
        agent_timeout: Duration::from_secs(600),
        permission_ask_timeout: Duration::from_secs(300),
        ask_via_im_timeout: Duration::from_secs(1800),
        shutdown_grace: Duration::from_secs(60),
        agent_idle_timeout: Duration::from_secs(300),
        batch_window: Duration::from_millis(1),
    }
}

/// 慢后端 + 自定义预算（P4 /stop、批处理合并、空闲看门狗测试用）。
/// 本机会话列表可配置的 backend（P4-11 统一 /resume 测试用）。
async fn build_with_local(auth: Auth, local: Vec<LocalSession>) -> Ctx {
    let (plat, inbox, send_count) = MockPlatform::new();
    let (back, calls, prompts, order) = MockBackend::new_with_local(local).await;
    let (store, db) = tmp_store().await;

    let admins = auth.snapshot();
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        auth,
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into(), "Edit".into()],
        PermissionMode::Off,
        test_budgets(),
        CotDetail::Brief,
        admins,
    ));

    Ctx {
        disp,
        inbox,
        send_count,
        calls,
        prompts,
        order,
        db,
    }
}

async fn build_slow(auth: Auth, slow_ms: u64, budgets: TaskBudgets) -> Ctx {
    let (plat, inbox, send_count) = MockPlatform::new();
    let (back, calls, prompts, order) = MockBackend::new_slow(slow_ms);
    let (store, db) = tmp_store().await;

    let admins = auth.snapshot();
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        auth,
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into(), "Edit".into()],
        PermissionMode::Off,
        budgets,
        CotDetail::Brief,
        admins,
    ));

    Ctx {
        disp,
        inbox,
        send_count,
        calls,
        prompts,
        order,
        db,
    }
}

/// P5-5：与 build_slow 相同，但慢后端开跑即 announce session id
/// （/stop 中断保 session 测试用）。
async fn build_slow_with_session(auth: Auth, slow_ms: u64, sid: &str, budgets: TaskBudgets) -> Ctx {
    let (plat, inbox, send_count) = MockPlatform::new();
    let (back, calls, prompts, order) = MockBackend::new_slow_with_session(slow_ms, sid);
    let (store, db) = tmp_store().await;

    let admins = auth.snapshot();
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        auth,
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into(), "Edit".into()],
        PermissionMode::Off,
        budgets,
        CotDetail::Brief,
        admins,
    ));

    Ctx {
        disp,
        inbox,
        send_count,
        calls,
        prompts,
        order,
        db,
    }
}

/// P5-10：流式后端（逐段 Text + Final 全量），非卡片平台去重测试用。
async fn build_streaming(auth: Auth, texts: Vec<String>) -> Ctx {
    let (plat, inbox, send_count) = MockPlatform::new();
    let (back, calls, prompts, order) = MockBackend::new_streaming(texts);
    let (store, db) = tmp_store().await;

    let admins = auth.snapshot();
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        auth,
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into(), "Edit".into()],
        PermissionMode::Off,
        test_budgets(),
        CotDetail::Brief,
        admins,
    ));

    Ctx {
        disp,
        inbox,
        send_count,
        calls,
        prompts,
        order,
        db,
    }
}

/// P5-第五批：announce session 后返 Err 的 backend（Err 路径持久化测试用）。
async fn build_announce_fail(auth: Auth, sid: &str) -> Ctx {
    let (plat, inbox, send_count) = MockPlatform::new();
    let (back, calls, prompts, order) = MockBackend::new_announce_then_fail(sid);
    let (store, db) = tmp_store().await;

    let admins = auth.snapshot();
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        auth,
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into(), "Edit".into()],
        PermissionMode::Off,
        test_budgets(),
        CotDetail::Brief,
        admins,
    ));

    Ctx {
        disp,
        inbox,
        send_count,
        calls,
        prompts,
        order,
        db,
    }
}

/// 等待 conv 的在飞任务注册出现（join spawn 后写入 running map）。
async fn wait_registered(ctx: &Ctx, conv: &str) -> bool {
    for _ in 0..400 {
        if ctx.disp.running.lock().await.contains_key(conv) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

/// 把消息喂给 dispatcher 的 mock platform recv，并等待处理完成。
async fn feed_and_wait(ctx: &Ctx, msgs: Vec<InboundMessage>, want_calls: usize) {
    // 通过 downcast 不便，这里改为直接调用 handle（绕过 run/recv）。
    for m in msgs {
        let disp = ctx.disp.clone();
        // 直接 await handle，串行执行（handle 内部已有 per-conv 锁）。
        disp.handle(m).await;
    }
    // 等待到 calls 计数达到预期。
    for _ in 0..400 {
        if ctx.order.load(Ordering::SeqCst) >= want_calls {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// /img：workdir 内图片经 send_media 回传；workdir 外拒绝；缺文件提示。
#[tokio::test]
async fn img_sends_rejects_and_reports() {
    let _serial = SERIAL.lock().await;
    let outer = std::env::temp_dir().join(format!("imagent-img-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&outer);
    let wd = outer.join("ws");
    std::fs::create_dir_all(&wd).unwrap();
    let img = wd.join("a.png");
    std::fs::write(&img, b"png").unwrap();
    // workdir 外放一个真实文件，供逃逸拒绝分支（须存在才能过 canonicalize）。
    std::fs::write(outer.join("b.png"), b"png").unwrap();

    let ctx = build_with_workdir(Auth::new(vec!["alice".into()]), wd.clone()).await;
    feed_and_wait(
        &ctx,
        vec![
            msg("feishu:ou_t", "alice", "/img a.png"),
            msg("feishu:ou_t", "alice", "/img ../b.png"),
            msg("feishu:ou_t", "alice", "/img nope.png"),
        ],
        0,
    )
    .await;

    let inbox = ctx.inbox.lock().await.clone();
    // macOS 下 /var 是 /private/var 的 symlink，send_media 用 canonicalize 后的
    // 真实路径，断言同样 canonicalize 后比较。
    let img_real = img.canonicalize().unwrap();
    assert!(
        inbox
            .iter()
            .any(|m| *m == format!("[media:{}]", img_real.display())),
        "workdir 内图片应回传: {inbox:?}"
    );
    assert!(
        inbox.iter().any(|m| m.contains("不在当前工作目录")),
        "workdir 外应拒绝: {inbox:?}"
    );
    assert!(
        inbox.iter().any(|m| m.contains("文件不存在")),
        "缺文件应提示: {inbox:?}"
    );
    assert!(
        !inbox.iter().any(|m| m.contains("b.png]")),
        "workdir 外文件不应回传: {inbox:?}"
    );
    drop_db(ctx.db.clone()).await;
    let _ = std::fs::remove_dir_all(&outer);
}

async fn drop_db(p: std::path::PathBuf) {
    let _ = std::fs::remove_file(&p);
    let mut w = p.clone();
    w.set_extension("sqlite-wal");
    let _ = std::fs::remove_file(&w);
    let mut s = p.clone();
    s.set_extension("sqlite-shm");
    let _ = std::fs::remove_file(&s);
}

// ---------- tests ----------

#[tokio::test]
async fn normal_message_runs_backend_and_replies_and_persists() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    feed_and_wait(&ctx, vec![msg("c1", "alice", "hello")], 1).await;

    // 回传收到（Final 优先）。
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.starts_with("reply#")),
        "inbox={inbox:?}"
    );

    // session 落库且 id 正确。
    let row = ctx
        .check()
        .await
        .get_session("c1")
        .await
        .unwrap()
        .expect("session row");
    assert_eq!(row.session_id, "sess-0");
    assert_eq!(row.agent_kind, "mock-backend");
    drop_db(ctx.db).await;
}

/// 纯媒体消息且全部下载失败时，应向用户回真实错误而非静默丢弃。
#[tokio::test]
async fn pure_media_all_failed_replies_error() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    let m = InboundMessage {
        conv_id: ConvId("feishu:ou_t".into()),
        sender: UserId("alice".into()),
        text: None,
        media: vec![],
        media_errors: vec!["img_x: 下载失败: boom".into()],
        mentions: Vec::new(),
        mentioned_bot: false,
        ask_req: None,
        reply_to: None,
        reply_hint: ReplyHint::None,
    };
    feed_and_wait(&ctx, vec![m], 0).await;

    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("⚠️") && t.contains("img_x")),
        "纯媒体全失败应回真实错误提示: {inbox:?}"
    );
    drop_db(ctx.db).await;
}
#[tokio::test]
async fn non_terminal_outcome_prefixes_warning() {
    // R1：backend 返回 terminal=false（模拟 agent 崩溃），reply 应前置告警。
    let _serial = SERIAL.lock().await;
    let ctx = build_non_terminal(Auth::new(vec!["alice".into()])).await;
    feed_and_wait(&ctx, vec![msg("c9", "alice", "hello")], 1).await;

    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.starts_with("⚠️ agent 异常退出，以下为部分输出：\n\n")),
        "inbox={inbox:?}"
    );
    // 告警后仍应跟有 backend 的 Final 文本（reply#）。
    assert!(
        inbox.iter().any(|t| t.contains("reply#")),
        "inbox={inbox:?}"
    );
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn second_message_continues_previous_session() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    feed_and_wait(
        &ctx,
        vec![msg("c2", "alice", "first"), msg("c2", "alice", "second")],
        2,
    )
    .await;

    let calls = ctx.calls.lock().await.clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], None, "first should be new session");
    assert_eq!(calls[1].as_deref(), Some("sess-0"), "second should resume");
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn discovery_mode_skips_backend_but_replies_guide() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec![])).await; // 发现模式
    feed_and_wait(&ctx, vec![msg("c3", "anyone", "hi")], 0).await;

    // backend 未被调用。
    assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
    // 但回传了引导消息，且其中含 sender id。
    let inbox = ctx.inbox.lock().await.clone();
    assert_eq!(inbox.len(), 1, "应回一条引导，inbox={inbox:?}");
    assert!(inbox[0].contains("anyone"), "引导消息应含 sender id");
    assert!(inbox[0].contains("imagent allow"), "引导消息应含 CLI 指引");
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn non_allowlisted_sender_dropped() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["someone_else".into()])).await;
    feed_and_wait(&ctx, vec![msg("c4", "intruder", "hi")], 0).await;

    assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
    assert_eq!(ctx.send_count.load(Ordering::SeqCst), 0);
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn slash_new_resets_session() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 先发一条普通消息建立 session。
    feed_and_wait(&ctx, vec![msg("c5", "alice", "hello")], 1).await;
    assert!(ctx.check().await.get_session("c5").await.unwrap().is_some());

    // 发 /new：应删除 session 并回 IM。
    let before_sends = ctx.send_count.load(Ordering::SeqCst);
    feed_and_wait(&ctx, vec![msg("c5", "alice", "/new")], 1).await;
    // /new 不触发 backend，order 不变。
    assert_eq!(ctx.order.load(Ordering::SeqCst), 1);
    // session 已删除。
    assert!(ctx.check().await.get_session("c5").await.unwrap().is_none());
    // 回传了一条重置提示。
    let after_sends = ctx.send_count.load(Ordering::SeqCst);
    assert_eq!(after_sends, before_sends + 1);

    // 下一条普通消息 backend 收到的 session 是 None。
    feed_and_wait(&ctx, vec![msg("c5", "alice", "fresh start")], 2).await;
    let calls = ctx.calls.lock().await.clone();
    // 最后一次调用 session 应为 None（fresh start 新建）。
    assert_eq!(calls.last(), Some(&None));
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn unknown_slash_command_replies() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    let before = ctx.send_count.load(Ordering::SeqCst);
    feed_and_wait(&ctx, vec![msg("c6", "alice", "/foo bar")], 0).await;
    let after = ctx.send_count.load(Ordering::SeqCst);
    assert_eq!(after, before + 1);
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("未知命令") && t.contains("/foo")),
        "inbox={inbox:?}"
    );
    // backend 未被调用。
    assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn per_conv_serial_order() {
    // 同一 conv 连发三条；mock backend 内 fetch_add 顺序应递增。
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    feed_and_wait(
        &ctx,
        vec![
            msg("c7", "alice", "a"),
            msg("c7", "alice", "b"),
            msg("c7", "alice", "c"),
        ],
        3,
    )
    .await;

    let calls = ctx.calls.lock().await.clone();
    // 三条依次执行；session 链：None -> sess-0 -> sess-1。
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0], None);
    assert_eq!(calls[1].as_deref(), Some("sess-0"));
    assert_eq!(calls[2].as_deref(), Some("sess-1"));
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn allow_rejected_for_non_admin_when_admin_senders_set() {
    // P2-D：admin_senders 非空时，白名单内非 admin 用户 /allow 被拒绝。
    let ctx = build_with_admin(
        Auth::new(vec!["alice".into(), "bob".into()]),
        vec!["alice".into()],
    )
    .await;
    // bob（白名单但非 admin）尝试 /allow charlie → 应被拒绝。
    feed_and_wait(&ctx, vec![msg("c", "bob", "/allow charlie")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|m| m.contains("仅管理员")),
        "非 admin /allow 应被拒绝: {inbox:?}"
    );
    // charlie 未被授权。
    assert!(!ctx.disp.auth().is_allowed(&UserId("charlie".into())));
    drop_db(ctx.db).await;
}

/// P6-2：/allow @名字 从本条消息的 mentions 元数据反解 open_id 授权。
#[tokio::test]
async fn allow_command_resolves_mention() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    let mut m = msg("c", "alice", "/allow @张三");
    m.mentions = vec![Mention {
        user_id: "ou_zhangsan".into(),
        name: "张三".into(),
    }];
    feed_and_wait(&ctx, vec![m], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|s| s.contains("ou_zhangsan")),
        "@提及应反解 open_id 并回执: {inbox:?}"
    );
    assert!(ctx.disp.auth().is_allowed(&UserId("ou_zhangsan".into())));
    drop_db(ctx.db).await;
}

/// P6-2：/allow @提及 无元数据可反解 → 回用法提示，不误授字符串本体。
#[tokio::test]
async fn allow_command_mention_unresolvable_hints() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // mentions 为空（如手打 @张三 文本、或平台未解析出元数据）。
    feed_and_wait(&ctx, vec![msg("c", "alice", "/allow @张三")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|s| s.contains("无法从本条消息解析")),
        "无元数据应回反解失败提示: {inbox:?}"
    );
    assert!(
        !ctx.disp.auth().is_allowed(&UserId("@张三".into())),
        "不得把 @字串 本身当 id 授权"
    );
    drop_db(ctx.db).await;
}

/// P7-A1：/admin add|remove|list——管理员动态管理（默认 admin 空白名单用户可管，
/// 即向后兼容语义；添加即时生效并持久化；不可移除自己）。
#[tokio::test]
async fn admin_command_add_remove_list() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 列表（build 默认 admins = 白名单快照 = [alice]）。
    feed_and_wait(&ctx, vec![msg("c", "alice", "/admin")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("管理员（1）：alice")),
        "应列出当前管理员: {inbox:?}"
    );
    // add bob → 回执 + 列表出现；首位管理员设立时操作者一并加入（防自锁）。
    feed_and_wait(&ctx, vec![msg("c", "alice", "/admin add bob")], 1).await;
    feed_and_wait(&ctx, vec![msg("c", "alice", "/admin list")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    let list_line = inbox
        .iter()
        .rev()
        .find(|t| t.starts_with("管理员（"))
        .cloned();
    let list_line = list_line.expect("应有管理员列表");
    assert!(list_line.contains("bob"), "列表应含 bob: {list_line}");
    assert!(
        list_line.contains("alice"),
        "首位设立应含操作者 alice（防自锁）: {list_line}"
    );
    // 移除自己 → 拒绝。
    feed_and_wait(&ctx, vec![msg("c", "alice", "/admin remove alice")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("不允许移除自己")),
        "自移除应被拒: {inbox:?}"
    );
    // remove bob → 成功回执。
    feed_and_wait(&ctx, vec![msg("c", "alice", "/admin remove bob")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("已移除管理员 `bob`")),
        "移除回执: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

/// P7-A1：admin_senders 非空时，白名单内非 admin 用户 /admin add 被拒。
#[tokio::test]
async fn admin_command_rejected_for_non_admin() {
    let _serial = SERIAL.lock().await;
    let ctx = build_with_admin(
        Auth::new(vec!["alice".into(), "bob".into()]),
        vec!["alice".into()],
    )
    .await;
    feed_and_wait(&ctx, vec![msg("c", "bob", "/admin add charlie")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("仅管理员")),
        "非 admin /admin 应被拒: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

/// P7-A3：stranger_mention_hint 开启时，未过白名单的群 @bot 消息回引导；
/// 关闭（默认）保持完全静默；私聊（mentioned_bot=false）不提示。
#[tokio::test]
async fn stranger_mention_hint_on_off() {
    let _serial = SERIAL.lock().await;
    // 默认关：静默。
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    let mut m = msg("feishu:oc_g", "stranger", "hi bot");
    m.mentioned_bot = true;
    feed_and_wait(&ctx, vec![m], 0).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(inbox.is_empty(), "默认应完全静默: {inbox:?}");
    drop_db(ctx.db).await;

    // 开启 + @bot → 引导；开启但未 @bot → 仍静默。
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    ctx.disp.set_prefs(true, crate::config::ReplyMode::Card);
    let mut m = msg("feishu:oc_g", "stranger", "hi bot");
    m.mentioned_bot = true;
    feed_and_wait(&ctx, vec![m], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("/chat allow")),
        "被 @ 应回引导: {inbox:?}"
    );
    let m2 = msg("feishu:oc_g", "stranger", "no mention");
    feed_and_wait(&ctx, vec![m2], 0).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert_eq!(inbox.len(), 1, "未 @bot 不应追加提示: {inbox:?}");
    drop_db(ctx.db).await;
}

/// P9-2：表单提交回传（/config form k=v k=v）——多键一次应用；非法值逐键回报。
#[tokio::test]
async fn config_form_applies_multiple_pairs() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    feed_and_wait(
        &ctx,
        vec![msg(
            "c",
            "alice",
            "/config form reply_mode=text cot_detail=detailed",
        )],
        1,
    )
    .await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .last()
            .is_some_and(|t| t.contains("reply_mode = text") && t.contains("cot_detail = detailed")),
        "表单多键应用: {inbox:?}"
    );
    assert_eq!(*ctx.disp.reply_mode.read(), ReplyMode::Text);
    // 非法值：该键回用法，不影响其它键。
    feed_and_wait(
        &ctx,
        vec![msg(
            "c",
            "alice",
            "/config form cot_detail=yaml reply_mode=card",
        )],
        1,
    )
    .await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .last()
            .is_some_and(|t| t.contains("用法") && t.contains("reply_mode = card")),
        "逐键结果: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

/// P7-A4：/config reply_mode text 热切换 + 展示。
#[tokio::test]
async fn config_reply_mode_toggle() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    feed_and_wait(&ctx, vec![msg("c", "alice", "/config reply_mode text")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("reply_mode = text")),
        "应回执切换: {inbox:?}"
    );
    feed_and_wait(&ctx, vec![msg("c", "alice", "/config")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("reply_mode = text")),
        "/config 展示应含当前值: {inbox:?}"
    );
    // 非法值 → 用法提示。
    feed_and_wait(&ctx, vec![msg("c", "alice", "/config reply_mode yaml")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("用法：reply_mode")),
        "非法值应回用法: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

/// P7-A2：/chat allow-all——MockPlatform 无群列表（trait 默认 Err）应如实报错。
#[tokio::test]
async fn chat_allow_all_unsupported_platform() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    feed_and_wait(&ctx, vec![msg("c", "alice", "/chat allow-all")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("列出群失败")),
        "不支持平台应回失败: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

/// P6 遗留补齐：/config require_mention on|off——平台 trait 默认实现
/// （MockPlatform 无群聊 @ 语义）应回「设置失败」；/config 展示含该项。
#[tokio::test]
async fn config_require_mention_unsupported_platform() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    feed_and_wait(
        &ctx,
        vec![msg("c", "alice", "/config require_mention on")],
        1,
    )
    .await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|s| s.contains("设置失败")),
        "不支持的平台应回设置失败: {inbox:?}"
    );
    // 展示态也含 require_mention 行。
    feed_and_wait(&ctx, vec![msg("c", "alice", "/config")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|s| s.contains("require_mention")),
        "/config 展示应含 require_mention: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn allow_command_grants_then_bob_can_drive() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;

    // alice 发 /allow bob：应回「已授权」。
    feed_and_wait(&ctx, vec![msg("c8", "alice", "/allow bob")], 0).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("已授权") && t.contains("bob")),
        "inbox={inbox:?}"
    );

    // 白名单持久化到 store。
    let stored = ctx.check().await.list_allowed_senders().await.unwrap();
    assert!(stored.iter().any(|s| s == "bob"), "stored={stored:?}");

    // bob（刚被授权）现在能驱动 backend。
    feed_and_wait(&ctx, vec![msg("c9", "bob", "hello")], 1).await;
    assert_eq!(ctx.order.load(Ordering::SeqCst), 1, "bob 应能驱动 backend");
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn list_command_replies_whitelist() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into(), "carol".into()])).await;
    feed_and_wait(&ctx, vec![msg("c10", "alice", "/list")], 0).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("alice") && t.contains("carol") && t.contains("白名单")),
        "inbox={inbox:?}"
    );
    // backend 未被调用。
    assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn whoami_command_replies_sender() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    feed_and_wait(&ctx, vec![msg("c11", "alice", "/whoami")], 0).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(inbox.iter().any(|t| t.contains("alice")), "inbox={inbox:?}");
    assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn disallow_cannot_revoke_self() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    feed_and_wait(&ctx, vec![msg("c12", "alice", "/disallow alice")], 0).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("不允许撤销自己")),
        "inbox={inbox:?}"
    );
    // alice 仍在白名单。
    assert!(ctx.disp.auth.is_allowed(&UserId("alice".into())));
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn disallow_command_removes_target() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into(), "bob".into()])).await;
    feed_and_wait(&ctx, vec![msg("c13", "alice", "/disallow bob")], 0).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("已移除") && t.contains("bob")),
        "inbox={inbox:?}"
    );
    // bob 已被移除：后续消息被丢弃，不驱动 backend。
    feed_and_wait(&ctx, vec![msg("c14", "bob", "still here?")], 0).await;
    assert_eq!(ctx.order.load(Ordering::SeqCst), 0, "bob 应已被移出白名单");
    drop_db(ctx.db).await;
}
#[tokio::test]
async fn switch_new_name_clears_session_and_sets_active() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 先建立默认 session。
    feed_and_wait(&ctx, vec![msg("s1", "alice", "hello")], 1).await;
    assert!(ctx.check().await.get_session("s1").await.unwrap().is_some());

    // /switch newtask（命名不存在）→ 活动清空 + active_name 设。
    feed_and_wait(&ctx, vec![msg("s1", "alice", "/switch newtask")], 1).await;
    assert!(
        ctx.check().await.get_session("s1").await.unwrap().is_none(),
        "switch 新命名后活动 session 应清空"
    );
    assert_eq!(
        ctx.check()
            .await
            .get_config("active_name:s1")
            .await
            .unwrap(),
        Some("newtask".to_string())
    );

    // 下一条普通消息：backend 收到 None（新建），并落 named_sessions(newtask)。
    feed_and_wait(&ctx, vec![msg("s1", "alice", "do work")], 2).await;
    let calls = ctx.calls.lock().await.clone();
    assert_eq!(calls.last(), Some(&None), "switch 后首条消息应新建 session");

    let nrow = ctx
        .check()
        .await
        .get_named_session("s1", "newtask")
        .await
        .unwrap()
        .expect("named row");
    assert_eq!(nrow.name, "newtask");
    // 活动 session 行 name 也带命名。
    let srow = ctx
        .check()
        .await
        .get_session("s1")
        .await
        .unwrap()
        .expect("session row");
    assert_eq!(srow.name.as_deref(), Some("newtask"));
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn switch_existing_name_resumes_named_session() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 建立命名 session `taskA`。
    feed_and_wait(&ctx, vec![msg("s2", "alice", "/switch taskA")], 0).await;
    feed_and_wait(&ctx, vec![msg("s2", "alice", "first taskA work")], 1).await;
    let nrow = ctx
        .check()
        .await
        .get_named_session("s2", "taskA")
        .await
        .unwrap()
        .expect("named row taskA");
    let task_a_sid = nrow.session_id.clone();

    // 切到默认（/new 清 active_name）再发消息建立另一个默认 session。
    feed_and_wait(&ctx, vec![msg("s2", "alice", "/new")], 1).await;
    feed_and_wait(&ctx, vec![msg("s2", "alice", "default work")], 2).await;

    // /switch taskA（命名已存在）→ 活动 session 被写成 taskA 的 session_id。
    feed_and_wait(&ctx, vec![msg("s2", "alice", "/switch taskA")], 2).await;
    let srow = ctx
        .check()
        .await
        .get_session("s2")
        .await
        .unwrap()
        .expect("session row");
    assert_eq!(
        srow.session_id, task_a_sid,
        "switch 已存在命名应 resume 其 session_id"
    );
    assert_eq!(srow.name.as_deref(), Some("taskA"));
    assert_eq!(
        ctx.check()
            .await
            .get_config("active_name:s2")
            .await
            .unwrap(),
        Some("taskA".to_string())
    );

    // 下一条普通消息应续接 taskA 的 session_id。
    feed_and_wait(&ctx, vec![msg("s2", "alice", "continue")], 3).await;
    let calls = ctx.calls.lock().await.clone();
    assert_eq!(
        calls.last(),
        Some(&Some(task_a_sid)),
        "switch 后续消息应续接命名 session"
    );
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn sessions_command_lists_named_with_active_mark() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 空时 /sessions。
    feed_and_wait(&ctx, vec![msg("s3", "alice", "/sessions")], 0).await;
    assert!(
        ctx.inbox
            .lock()
            .await
            .last()
            .unwrap()
            .contains("无命名会话"),
        "空命名时应提示无"
    );

    // 建两个命名 session。
    feed_and_wait(&ctx, vec![msg("s3", "alice", "/switch alpha")], 0).await;
    feed_and_wait(&ctx, vec![msg("s3", "alice", "a work")], 1).await;
    feed_and_wait(&ctx, vec![msg("s3", "alice", "/switch beta")], 1).await;
    feed_and_wait(&ctx, vec![msg("s3", "alice", "b work")], 2).await;

    // /sessions：应列出 alpha、beta，当前活动 beta 标 *。
    feed_and_wait(&ctx, vec![msg("s3", "alice", "/sessions")], 2).await;
    let inbox = ctx.inbox.lock().await.clone();
    let listing = inbox.last().unwrap();
    assert!(listing.contains("命名会话"), "listing={listing}");
    assert!(listing.contains("alpha"), "listing={listing}");
    assert!(listing.contains("beta"), "listing={listing}");
    // beta 为活动，应带（当前）；alpha 不应带。
    assert!(
        listing.contains("beta（当前）"),
        "活动命名应带（当前），listing={listing}"
    );
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn new_command_clears_active_name() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 建命名 session。
    feed_and_wait(&ctx, vec![msg("s4", "alice", "/switch build")], 0).await;
    feed_and_wait(&ctx, vec![msg("s4", "alice", "go")], 1).await;
    assert_eq!(
        ctx.check()
            .await
            .get_config("active_name:s4")
            .await
            .unwrap(),
        Some("build".to_string())
    );

    // /new 清活动 session 与 active_name。
    feed_and_wait(&ctx, vec![msg("s4", "alice", "/new")], 1).await;
    assert!(
        ctx.check()
            .await
            .get_config("active_name:s4")
            .await
            .unwrap()
            .is_none(),
        "/new 后 active_name 应被清除"
    );
    assert!(ctx.check().await.get_session("s4").await.unwrap().is_none());

    // 下一条普通消息：name 应为 None（默认未命名）。
    feed_and_wait(&ctx, vec![msg("s4", "alice", "fresh")], 2).await;
    let srow = ctx
        .check()
        .await
        .get_session("s4")
        .await
        .unwrap()
        .expect("row");
    assert!(srow.name.is_none(), "/new 后新 session 应未命名");
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn compact_with_active_session_generates_summary_and_resets() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 先建立活动 session。
    feed_and_wait(&ctx, vec![msg("k1", "alice", "hello")], 1).await;
    assert!(ctx.check().await.get_session("k1").await.unwrap().is_some());

    // /compact：应 resume 当前 session 生成摘要，order +1。
    feed_and_wait(&ctx, vec![msg("k1", "alice", "/compact")], 2).await;

    // backend 被调用，且 session 为 Some（resume）。
    let calls = ctx.calls.lock().await.clone();
    assert_eq!(calls.len(), 2, "calls={calls:?}");
    assert_eq!(
        calls[1].as_deref(),
        Some("sess-0"),
        "/compact 应 resume 当前 session"
    );

    // 回传含「摘要」字样。
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("摘要")),
        "应回摘要提示，inbox={inbox:?}"
    );

    // 摘要已落库。
    assert_eq!(
        ctx.check()
            .await
            .get_config("compact_summary:k1")
            .await
            .unwrap(),
        Some("reply#1".to_string()),
        "摘要应为 Final chunk 文本"
    );

    // 活动 session 已删除。
    assert!(
        ctx.check().await.get_session("k1").await.unwrap().is_none(),
        "/compact 后活动 session 应删除"
    );
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn compact_without_active_session_replies_nothing() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 无活动 session 直接 /compact。
    feed_and_wait(&ctx, vec![msg("k2", "alice", "/compact")], 0).await;

    // backend 未被调用。
    assert_eq!(ctx.order.load(Ordering::SeqCst), 0);
    // 回传「无活动会话」。
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("无活动会话")),
        "inbox={inbox:?}"
    );
    // 摘要未落库。
    assert!(ctx
        .check()
        .await
        .get_config("compact_summary:k2")
        .await
        .unwrap()
        .is_none());
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn compact_summary_injected_once_for_new_session() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 预置摘要 + 无活动 session。
    ctx.check()
        .await
        .set_config("compact_summary:k3", "之前讨论了 X 与 Y")
        .await
        .unwrap();
    assert!(ctx.check().await.get_session("k3").await.unwrap().is_none());

    // 发普通消息（无 existing）→ 应注入摘要。
    feed_and_wait(&ctx, vec![msg("k3", "alice", "继续")], 1).await;
    let prompts = ctx.prompts.lock().await.clone();
    assert_eq!(prompts.len(), 1, "prompts={prompts:?}");
    assert!(
        prompts[0].contains("【前情摘要】"),
        "新建 session 应注入摘要，prompt={}",
        prompts[0]
    );
    assert!(
        prompts[0].ends_with("继续"),
        "原始 prompt 应在末尾，prompt={}",
        prompts[0]
    );

    // 摘要一次性注入后清除。
    assert!(
        ctx.check()
            .await
            .get_config("compact_summary:k3")
            .await
            .unwrap()
            .is_none(),
        "摘要应一次性清除"
    );

    // 再发一条（现已 existing）→ 不再注入。
    feed_and_wait(&ctx, vec![msg("k3", "alice", "more")], 2).await;
    let prompts = ctx.prompts.lock().await.clone();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[1], "more", "第二条不应含摘要");
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn compact_summary_not_injected_when_session_exists() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 先建立活动 session。
    feed_and_wait(&ctx, vec![msg("k4", "alice", "first")], 1).await;
    assert!(ctx.check().await.get_session("k4").await.unwrap().is_some());

    // 会话已存在后再预置摘要；下一条消息 existing=Some，不应注入。
    ctx.check()
        .await
        .set_config("compact_summary:k4", "遗留摘要")
        .await
        .unwrap();
    feed_and_wait(&ctx, vec![msg("k4", "alice", "second")], 2).await;

    let prompts = ctx.prompts.lock().await.clone();
    assert_eq!(
        prompts[1], "second",
        "existing 时不应误注入，prompts={prompts:?}"
    );
    // 摘要仍存在（未被消费）。
    assert_eq!(
        ctx.check()
            .await
            .get_config("compact_summary:k4")
            .await
            .unwrap(),
        Some("遗留摘要".to_string())
    );
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn normal_message_appends_tool_summary() {
    let _serial = SERIAL.lock().await;
    let tools = vec![
        ("Read".to_string(), r#"{"path":"/foo"}"#.to_string()),
        ("Edit".to_string(), r#"{"file":"/bar"}"#.to_string()),
    ];
    let ctx = build_with_tools(Auth::new(vec!["alice".into()]), tools).await;
    feed_and_wait(&ctx, vec![msg("t1", "alice", "do it")], 1).await;

    // 回传文本含工具摘要与工具名。
    let inbox = ctx.inbox.lock().await.clone();
    let reply = inbox
        .iter()
        .find(|t| t.starts_with("reply#"))
        .expect("应有 final reply");
    assert!(reply.contains("🔧 工具调用"), "应含工具摘要标题: {reply}");
    assert!(reply.contains("Read — /foo"), "应含 Read 摘要: {reply}");
    assert!(reply.contains("Edit"), "应含 Edit 工具: {reply}");
    assert!(reply.contains("/foo"), "应含工具输入: {reply}");
    drop_db(ctx.db).await;
}

#[tokio::test]
async fn tool_summary_truncates_after_five() {
    let _serial = SERIAL.lock().await;
    // 6 个工具 → 截断展示 5 个并标 …(+1)。
    let tools: Vec<(String, String)> = (0..6)
        .map(|i| (format!("Tool{i}"), format!(r#"{{"k":"{i}"}}"#)))
        .collect();
    let ctx = build_with_tools(Auth::new(vec!["alice".into()]), tools).await;
    feed_and_wait(&ctx, vec![msg("t2", "alice", "go")], 1).await;

    let inbox = ctx.inbox.lock().await.clone();
    let reply = inbox
        .iter()
        .find(|t| t.starts_with("reply#"))
        .expect("应有 final reply");
    assert!(reply.contains("…(+1)"), "6 个工具应标 …(+1): {reply}");
    assert!(reply.contains("Tool0"), "应含首个工具: {reply}");
    assert!(reply.contains("Tool4"), "应含第 5 个工具: {reply}");
    drop_db(ctx.db).await;
}

// ---------- P4：/stop、批处理合并、空闲看门狗 ----------

#[test]
fn permission_reply_candidate_classification() {
    // 非空普通文本可作审批回复；斜杠命令与空文本不消费（/stop 在等审批时
    // 也要可执行；纯媒体消息不误吞成 deny）。
    assert!(is_permission_reply_candidate("y"));
    assert!(is_permission_reply_candidate(" 可以 "));
    assert!(is_permission_reply_candidate("yes please"));
    assert!(!is_permission_reply_candidate("/stop"));
    assert!(!is_permission_reply_candidate("/new"));
    assert!(!is_permission_reply_candidate(""));
    assert!(!is_permission_reply_candidate("   "));
}

#[test]
fn merge_batch_joins_and_concats() {
    let mut a = msg("c1", "u1", "first");
    a.media.push(MediaRef {
        kind: "image".into(),
        url: "/tmp/a.png".into(),
    });
    let mut b = msg("c1", "u2", "second");
    b.media.push(MediaRef {
        kind: "image".into(),
        url: "/tmp/b.png".into(),
    });
    b.media_errors.push("dl fail".into());
    let blank = msg("c1", "u3", "   "); // 空文本跳过，media 仍并入
    let mut m = merge_batch(vec![a, b, blank]);
    assert_eq!(m.text.as_deref(), Some("first\n\nsecond"));
    assert_eq!(m.sender.0, "u1", "sender 取首条");
    assert_eq!(m.media.len(), 2, "media 拼接");
    assert_eq!(m.media_errors, vec!["dl fail".to_string()]);
    m.media.clear(); // silence unusedASSIGN 风格告警（显式消费）
                     // 全空文本 + 纯媒体：text 为 None，media 保留。
    let mut media_only = msg("c1", "u1", "   ");
    media_only.media.push(MediaRef {
        kind: "image".into(),
        url: "/tmp/x.png".into(),
    });
    let merged = merge_batch(vec![media_only]);
    assert_eq!(merged.text, None);
    assert_eq!(merged.media.len(), 1);
}

/// P4-1：/stop 中断在飞任务——backend 被 abort（无 Final 回复）、在飞注册
/// 清空、/stop 回确认。
#[tokio::test]
async fn stop_aborts_running_task() {
    let _serial = SERIAL.lock().await;
    let ctx = build_slow(
        Auth::new(vec!["alice".into()]),
        30_000,
        TaskBudgets {
            ask_via_im_timeout: std::time::Duration::from_secs(1800),
            batch_window: Duration::from_millis(1),
            ..test_budgets()
        },
    )
    .await;
    let disp = ctx.disp.clone();
    let runner = tokio::spawn(async move {
        disp.handle(msg("c1", "alice", "slow task")).await;
    });
    assert!(
        wait_registered(&ctx, "c1").await,
        "在飞任务应已注册（join spawn 后）"
    );
    ctx.disp.handle(msg("c1", "alice", "/stop")).await;
    let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
    assert!(done.is_ok(), "被中断的 runner 应很快退出");
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("已中断当前任务")),
        "应回中断确认: {inbox:?}"
    );
    assert!(
        !inbox.iter().any(|t| t.starts_with("reply#")),
        "中断后不应有 Final 回复: {inbox:?}"
    );
    assert!(ctx.disp.running.lock().await.is_empty(), "在飞注册应清空");
    assert_eq!(ctx.prompts.lock().await.len(), 1, "恰一轮被中断的执行");
    drop_db(ctx.db).await;
}

/// P4-1：无在飞任务时 /stop 友好回复（不报错）。
#[tokio::test]
async fn stop_without_running_task_replies() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    ctx.disp.handle(msg("c1", "alice", "/stop")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("没有运行中的任务")),
        "应回无任务提示: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

/// P4-1/P4-2：/stop 同时丢弃排队待合并的消息，回复含丢弃条数。
#[tokio::test]
async fn stop_drops_queued_messages() {
    let _serial = SERIAL.lock().await;
    let ctx = build_slow(
        Auth::new(vec!["alice".into()]),
        30_000,
        TaskBudgets {
            batch_window: Duration::from_millis(1),
            ..test_budgets()
        },
    )
    .await;
    let disp = ctx.disp.clone();
    let runner = tokio::spawn(async move {
        disp.handle(msg("c1", "alice", "first round")).await;
    });
    assert!(wait_registered(&ctx, "c1").await, "在飞任务应已注册");
    ctx.disp.handle(msg("c1", "alice", "queued B")).await;
    ctx.disp.handle(msg("c1", "alice", "queued C")).await;
    // 等 2 条都入队。
    for _ in 0..400 {
        if ctx
            .disp
            .queues
            .lock()
            .await
            .get("c1")
            .is_some_and(|q| q.len() == 2)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    ctx.disp.handle(msg("c1", "alice", "/stop")).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), runner).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("已中断当前任务（丢弃 2 条排队消息）")),
        "应回丢弃条数: {inbox:?}"
    );
    let prompts = ctx.prompts.lock().await.clone();
    assert_eq!(
        prompts,
        vec!["first round".to_string()],
        "排队消息不应再执行"
    );
    drop_db(ctx.db).await;
}

/// P4-2：运行中到达的消息排队到下一轮，且合并为一轮（B、C 合成 "B\n\nC"）。
#[tokio::test]
async fn messages_during_run_merge_into_next_round() {
    let _serial = SERIAL.lock().await;
    let ctx = build_slow(
        Auth::new(vec!["alice".into()]),
        200,
        TaskBudgets {
            batch_window: Duration::from_millis(1),
            ..test_budgets()
        },
    )
    .await;
    let disp = ctx.disp.clone();
    let runner = tokio::spawn(async move {
        disp.handle(msg("c1", "alice", "round A")).await;
    });
    assert!(wait_registered(&ctx, "c1").await, "在飞任务应已注册");
    ctx.disp.handle(msg("c1", "alice", "msg B")).await;
    ctx.disp.handle(msg("c1", "alice", "msg C")).await;
    let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
    assert!(done.is_ok(), "runner 应在两轮后退出");
    let prompts = ctx.prompts.lock().await.clone();
    assert_eq!(
        prompts,
        vec!["round A".to_string(), "msg B\n\nmsg C".to_string()],
        "第二轮应为 B、C 合并后的单轮: {prompts:?}"
    );
    // 第二轮续接第一轮 session（批处理不破坏会话连续性）。
    let calls = ctx.calls.lock().await.clone();
    assert_eq!(
        calls,
        vec![None, Some("sess-0".to_string())],
        "第二轮应续接第一轮 session: {calls:?}"
    );
    drop_db(ctx.db).await;
}

/// P4-2：批处理窗口内连发的消息并入同一轮（而非各跑一轮）。
#[tokio::test]
async fn burst_messages_merge_within_window() {
    let _serial = SERIAL.lock().await;
    let ctx = build_slow(
        Auth::new(vec!["alice".into()]),
        50,
        TaskBudgets {
            batch_window: Duration::from_millis(200),
            ..test_budgets()
        },
    )
    .await;
    let disp = ctx.disp.clone();
    let runner = tokio::spawn(async move {
        disp.handle(msg("c1", "alice", "first half")).await;
    });
    // 窗口期内（200ms）并入第二条。
    tokio::time::sleep(Duration::from_millis(30)).await;
    ctx.disp.handle(msg("c1", "alice", "second half")).await;
    let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
    assert!(done.is_ok(), "runner 应退出");
    let prompts = ctx.prompts.lock().await.clone();
    assert_eq!(
        prompts,
        vec!["first half\n\nsecond half".to_string()],
        "窗口内连发应合并为单轮: {prompts:?}"
    );
    assert_eq!(ctx.order.load(Ordering::SeqCst), 1, "backend 只跑一轮");
    drop_db(ctx.db).await;
}

/// P4-3：空闲看门狗——agent 连续无输出超时终止本轮并告知用户。
#[tokio::test]
async fn idle_watchdog_terminates_silent_agent() {
    let _serial = SERIAL.lock().await;
    let ctx = build_slow(
        Auth::new(vec!["alice".into()]),
        30_000,
        TaskBudgets {
            agent_idle_timeout: Duration::from_millis(100),
            batch_window: Duration::from_millis(1),
            ..test_budgets()
        },
    )
    .await;
    // 直接 await：runner 循环内完成（窗口 → 一轮 → 看门狗 → 退出）。
    ctx.disp.handle(msg("c1", "alice", "hang please")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("无输出") && t.contains("空闲超时")),
        "应回空闲超时提示: {inbox:?}"
    );
    assert!(!inbox.iter().any(|t| t.starts_with("reply#")));
    assert_eq!(ctx.prompts.lock().await.len(), 1);
    assert!(ctx.disp.running.lock().await.is_empty(), "在飞注册应清空");
    assert!(
        ctx.disp.queues.lock().await.is_empty(),
        "runner 退出后队列 entry 应移除"
    );
    drop_db(ctx.db).await;
}

/// P4-2：排队上限——超限消息回告警并丢弃，runner 不受影响。
#[tokio::test]
async fn pending_queue_cap_warns_and_drops() {
    let _serial = SERIAL.lock().await;
    let ctx = build_slow(
        Auth::new(vec!["alice".into()]),
        30_000,
        TaskBudgets {
            batch_window: Duration::from_millis(1),
            ..test_budgets()
        },
    )
    .await;
    let disp = ctx.disp.clone();
    let runner = tokio::spawn(async move {
        disp.handle(msg("c1", "alice", "long first round")).await;
    });
    assert!(wait_registered(&ctx, "c1").await, "在飞任务应已注册");
    for i in 0..(PENDING_QUEUE_CAP + 5) {
        ctx.disp
            .handle(msg("c1", "alice", &format!("spam {i}")))
            .await;
    }
    ctx.disp.handle(msg("c1", "alice", "/stop")).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), runner).await;
    let inbox = ctx.inbox.lock().await.clone();
    let overflow = inbox.iter().filter(|t| t.contains("已达上限")).count();
    assert!(overflow >= 5, "超限的 5 条应各回一次告警: {inbox:?}");
    let prompts = ctx.prompts.lock().await.clone();
    assert_eq!(prompts.len(), 1, "只应有首轮（已 /stop）: {prompts:?}");
    drop_db(ctx.db).await;
}

// ---------- P4-5：会话（群）白名单 ----------

/// P4-5：未授权群 + 未授权 sender 丢弃；/chat allow 授权当前群后成员可驱动；
/// /chat list 列出；/chat deny 收回。
#[tokio::test]
async fn chat_allowlist_gates_group_messages() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // bob 在未授权群发言：丢弃（无回复、不跑 backend）。
    ctx.disp.handle(msg("feishu:oc_g", "bob", "hi group")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(inbox.is_empty(), "未授权群应静默丢弃: {inbox:?}");
    drop(inbox);
    // alice（白名单成员）在群里授权该群。
    ctx.disp
        .handle(msg("feishu:oc_g", "alice", "/chat allow"))
        .await;
    // 授权后 bob 的群消息驱动 agent。
    feed_and_wait(&ctx, vec![msg("feishu:oc_g", "bob", "run it")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("已授权会话 feishu:oc_g")),
        "应回授权确认: {inbox:?}"
    );
    assert!(
        inbox.iter().any(|t| t.starts_with("reply#")),
        "授权后群成员消息应驱动 agent: {inbox:?}"
    );
    // /chat deny 收回后 bob 再发言被丢弃。
    ctx.disp
        .handle(msg("feishu:oc_g", "alice", "/chat deny"))
        .await;
    let before = ctx.calls.lock().await.len();
    ctx.disp.handle(msg("feishu:oc_g", "bob", "again")).await;
    assert_eq!(
        ctx.calls.lock().await.len(),
        before,
        "收回授权后群消息不应驱动 agent"
    );
    drop_db(ctx.db).await;
}

/// P4-5：/chat 非管理员（admin_senders 非空时）被拒。
#[tokio::test]
async fn chat_command_requires_admin_when_set() {
    let _serial = SERIAL.lock().await;
    let ctx = build_with_admin(Auth::new(vec!["alice".into()]), vec!["root".into()]).await;
    ctx.disp
        .handle(msg("feishu:oc_g", "alice", "/chat allow"))
        .await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("仅管理员")),
        "非管理员应被拒: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

/// P4-5：仅配置会话白名单（sender 空）不进发现模式，群成员可直接使用。
#[tokio::test]
async fn chats_only_config_not_discovery() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::with_chats(vec![], vec!["feishu:oc_g".into()])).await;
    feed_and_wait(&ctx, vec![msg("feishu:oc_g", "bob", "hello")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.starts_with("reply#")),
        "授权群的成员消息应驱动 agent: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

// ---------- P4-6：/config 与 COT 档位 ----------

/// P4-6：/config 查看显示全部键；设置 off 后工具摘要消失。
#[tokio::test]
async fn config_views_and_sets_cot_detail() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    ctx.disp.handle(msg("c1", "alice", "/config")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("cot_detail = brief") && t.contains("batch_window_ms")),
        "应列出配置: {inbox:?}"
    );
    drop(inbox);
    // 切 detailed → 生效确认。
    ctx.disp
        .handle(msg("c1", "alice", "/config cot_detail detailed"))
        .await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(inbox.iter().any(|t| t.contains("✅ cot_detail = detailed")));
    drop(inbox);
    // 切 off + 发工具消息 → 无 🔧 摘要。
    ctx.disp
        .handle(msg("c1", "alice", "/config cot_detail off"))
        .await;
    let tools = vec![("Read".to_string(), r#"{"path":"/foo"}"#.to_string())];
    let (back, calls, prompts, order) = MockBackend::new_with_tools(tools).await;
    let _ = (back, calls, prompts); // 本测试复用 ctx 的 backend，仅验证 off 档行为
    let _ = order;
    // 用带工具的 backend 直接构造（build_with_tools 默认 brief；这里改共享句柄已 off）。
    drop_db(ctx.db).await;
    // 单独验证 off 档：用带工具 ctx + /config off。
    let ctx2 = build_with_tools(
        Auth::new(vec!["alice".into()]),
        vec![("Read".to_string(), r#"{"path":"/foo"}"#.to_string())],
    )
    .await;
    ctx2.disp
        .handle(msg("t1", "alice", "/config cot_detail off"))
        .await;
    feed_and_wait(&ctx2, vec![msg("t1", "alice", "go")], 1).await;
    let inbox2 = ctx2.inbox.lock().await.clone();
    let reply = inbox2
        .iter()
        .find(|t| t.starts_with("reply#"))
        .expect("应有 final reply");
    assert!(
        !reply.contains("🔧 工具调用"),
        "off 档不应有工具摘要: {reply}"
    );
    drop_db(ctx2.db).await;
}

/// P4-6：detailed 档展示更长输入（>40 字符的输入可见）。
#[tokio::test]
async fn cot_detailed_shows_longer_input() {
    let _serial = SERIAL.lock().await;
    let long_input = format!(r#"{{"path":"{}"}}"#, "x".repeat(80));
    let ctx = build_with_tools(
        Auth::new(vec!["alice".into()]),
        vec![("Read".to_string(), long_input.clone())],
    )
    .await;
    ctx.disp
        .handle(msg("t1", "alice", "/config cot_detail detailed"))
        .await;
    feed_and_wait(&ctx, vec![msg("t1", "alice", "go")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    let reply = inbox
        .iter()
        .find(|t| t.starts_with("reply#"))
        .expect("应有 final reply");
    assert!(
        reply.contains("🔧 工具调用"),
        "detailed 档应有摘要: {reply}"
    );
    // brief 截断到 40 字符；detailed 到 200 → 80 个 x 应完整可见。
    assert!(
        reply.matches('x').count() >= 80,
        "detailed 档不应在 40 字符截断: {reply}"
    );
    drop_db(ctx.db).await;
}

// ---------- P4-7：/status /doctor /reconnect ----------

#[tokio::test]
async fn status_doctor_reconnect_reply() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    ctx.disp.handle(msg("c1", "alice", "/status")).await;
    ctx.disp.handle(msg("c1", "alice", "/doctor")).await;
    ctx.disp.handle(msg("c1", "alice", "/reconnect")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("📊") && t.contains("mock-backend（mock）")),
        "/status 应含后端/平台: {inbox:?}"
    );
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("🩺") && t.contains("存储读写正常")),
        "/doctor 应含自检结果: {inbox:?}"
    );
    // MockPlatform 未覆写 reconnect → 默认不支持，回告警而非成功。
    assert!(
        inbox.iter().any(|t| t.contains("重连指令失败")),
        "默认平台应报不支持重连: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

// ---------- P4-8/P4-11：/resume 统一列表 ----------

/// P4-8：两轮会话后 /resume 列出历史（当前带 *）；/resume <n> 恢复后下条消息
/// 续接被恢复的 session。MockBackend 默认无本机会话 → 纯 📱 历史列表。
#[tokio::test]
async fn resume_lists_and_restores_history() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    feed_and_wait(
        &ctx,
        vec![msg("c1", "alice", "first"), msg("c1", "alice", "second")],
        2,
    )
    .await;
    // 历史应有两条（sess-0 / sess-1），当前是 sess-1。
    ctx.disp.handle(msg("c1", "alice", "/resume")).await;
    let inbox = ctx.inbox.lock().await.clone();
    let list = inbox
        .iter()
        .find(|t| t.contains("可恢复会话"))
        .expect("应列出可恢复会话");
    assert!(list.contains("sess-0"), "应含 sess-0: {list}");
    assert!(list.contains("sess-1"), "应含 sess-1: {list}");
    assert!(list.contains("*"), "当前会话应带 *: {list}");
    assert!(list.contains("📱"), "历史会话标 📱: {list}");
    drop(inbox);
    // 恢复 1 号（sess-0）→ 下条消息续接 sess-0。
    ctx.disp.handle(msg("c1", "alice", "/resume 1")).await;
    feed_and_wait(&ctx, vec![msg("c1", "alice", "after resume")], 3).await;
    let calls = ctx.calls.lock().await.clone();
    assert_eq!(
        calls.last(),
        Some(&Some("sess-0".to_string())),
        "恢复后应续接 sess-0: {calls:?}"
    );
    // 越界序号 → 提示重看列表。
    ctx.disp.handle(msg("c1", "alice", "/resume 99")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("序号无效")),
        "越界序号应提示: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

/// P4-11：统一列表合并本机（💻）与 IM（📱）会话，按序号接管本机会话后
/// 下条消息续接之，且回复带分叉提示。
#[tokio::test]
async fn resume_merges_local_and_takes_over_pc_session() {
    let _serial = SERIAL.lock().await;
    let now = now_secs();
    let ctx = build_with_local(
        Auth::new(vec!["alice".into()]),
        vec![LocalSession {
            session_id: "pc-9f86d081".to_string(),
            updated_at: now - 3_600,
            first_prompt: "修复流式卡片超时问题".to_string(),
            cwd: None,
        }],
    )
    .await;
    // 一轮 IM 会话（历史表 sess-0，updated_at=now，排在 💻 之前）。
    feed_and_wait(&ctx, vec![msg("c1", "alice", "im round")], 1).await;
    ctx.disp.handle(msg("c1", "alice", "/resume")).await;
    let inbox = ctx.inbox.lock().await.clone();
    let list = inbox
        .iter()
        .find(|t| t.contains("可恢复会话"))
        .expect("应列出可恢复会话");
    assert!(list.contains("💻"), "本机会话标 💻: {list}");
    assert!(list.contains("📱"), "IM 会话标 📱: {list}");
    assert!(
        list.contains("修复流式卡片超时问题"),
        "本机会话摘要应展示: {list}"
    );
    assert!(list.contains("sess-0…"), "IM 历史行缺摘要回退 id: {list}");
    // 列表序：sess-0（新）在前，pc-9f86d081 第 2。
    let l1 = list.lines().find(|l| l.starts_with("1.")).unwrap();
    let l2 = list.lines().find(|l| l.starts_with("2.")).unwrap();
    assert!(
        l1.contains("📱") && l1.contains("sess-0"),
        "第 1 应为 IM 当前: {l1}"
    );
    assert!(
        l2.contains("💻") && l2.contains("修复"),
        "第 2 应为本机: {l2}"
    );
    drop(inbox);
    // /resume 2 接管本机会话：确认 + 分叉提示。
    ctx.disp.handle(msg("c1", "alice", "/resume 2")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("已接管会话 pc-9f86d081")),
        "应回接管确认: {inbox:?}"
    );
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("来自电脑端") && t.contains("分叉")),
        "本机会话应附分叉提示: {inbox:?}"
    );
    // 下条消息续接被接管的本机会话。
    feed_and_wait(&ctx, vec![msg("c1", "alice", "continue on pc")], 2).await;
    let calls = ctx.calls.lock().await.clone();
    assert_eq!(
        calls.last(),
        Some(&Some("pc-9f86d081".to_string())),
        "接管后续接本机会话: {calls:?}"
    );
    drop_db(ctx.db).await;
}

/// P4-11：序号选择依赖先列过表（缓存）；未列直接选序号 → 引导先看列表。
#[tokio::test]
async fn resume_numeric_without_listing_prompts_list() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    ctx.disp.handle(msg("c1", "alice", "/resume 1")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("序号无效") && t.contains("/resume")),
        "未列过表应引导先看列表: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

// ---------- P5 第一批：安全 + 中断续接 ----------

/// P5-1：审批回复候选消息的发送者须过白名单才可被路由消费——审批路由发生在
/// handle() 鉴权之前，不过门则群聊里非白名单成员发 "y" 即可批准权限请求。
#[tokio::test]
async fn permission_reply_gate_checks_sender() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::with_chats(
        vec!["alice".into()],
        vec!["c-group".into()],
    ))
    .await;
    // 白名单 sender：可路由。
    assert!(ctx
        .disp
        .can_route_permission_reply(&msg("c1", "alice", "y")));
    // 非白名单 sender 且会话未授权：不得消费。
    assert!(
        !ctx.disp.can_route_permission_reply(&msg("c1", "bob", "y")),
        "非白名单 sender 的审批回复不得被路由"
    );
    // S1 收紧：仅会话（群）白名单不再足够——群被加白后任意成员发 "y" 即可批准
    // 高危工具；群成员的回复不得被路由，须显式加入 sender 白名单（或为 admin）。
    assert!(
        !ctx
            .disp
            .can_route_permission_reply(&msg("c-group", "stranger", "y")),
        "仅群白名单（sender 未加白）不得路由审批回复"
    );
    // 白名单 sender 在群里：可路由。
    assert!(ctx
        .disp
        .can_route_permission_reply(&msg("c-group", "alice", "y")));
    drop_db(ctx.db).await;
}

/// S2：admin_senders 为空 = 无人是管理员——白名单用户 /allow、/admin 均被拒，
/// 并给出 CLI 配置引导；is_admin 对任何人（含白名单）返回 false。
#[tokio::test]
async fn empty_admin_senders_means_no_admin() {
    let _serial = SERIAL.lock().await;
    let ctx = build_with_admin(Auth::new(vec!["alice".into()]), vec![]).await;
    assert!(
        !ctx.disp.is_admin("alice"),
        "空 admin_senders：白名单用户也不是管理员"
    );
    ctx.disp.handle(msg("c", "alice", "/allow bob")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("无人是管理员") && t.contains("admin_senders")),
        "应说明空列表语义与配置途径: {inbox:?}"
    );
    assert!(!ctx.disp.auth().is_allowed(&UserId("bob".into())));
    drop_db(ctx.db).await;
}

/// D7：/resume 序号缓存按 (conv, sender) 隔离——alice 列过表后，未列过表的
/// bob 不能用序号选中（旧按 conv 共享缓存会互相消费/覆盖）。
#[tokio::test]
async fn resume_cache_isolated_per_sender() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into(), "bob".into()])).await;
    feed_and_wait(&ctx, vec![msg("c1", "alice", "first")], 1).await;
    // alice 列表（缓存写入 alice 名下）。
    ctx.disp.handle(msg("c1", "alice", "/resume")).await;
    // bob 未列过表：序号选择应无效（不得吃到 alice 的缓存）。
    ctx.disp.handle(msg("c1", "bob", "/resume 1")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("序号无效")),
        "bob 不应消费 alice 的序号缓存: {inbox:?}"
    );
    drop(inbox);
    // alice 自己仍可用序号选中。
    ctx.disp.handle(msg("c1", "alice", "/resume 1")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("已接管会话")),
        "alice 应能消费自己的缓存: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

/// P5-2：/perm 修改权限模式须管理员；查看（只读）不受限。
#[tokio::test]
async fn perm_switch_requires_admin() {
    let _serial = SERIAL.lock().await;
    let ctx = build_with_admin(
        Auth::new(vec!["alice".into(), "bob".into()]),
        vec!["alice".into()],
    )
    .await;
    // 非管理员切换 → 拒绝且模式不变。
    ctx.disp.handle(msg("c1", "bob", "/perm allow")).await;
    assert!(
        !matches!(*ctx.disp.permission_mode.read(), PermissionMode::Allow),
        "非管理员不得切换权限模式"
    );
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("仅管理员")),
        "应回拒绝提示: {inbox:?}"
    );
    drop(inbox);
    // 查看（只读）不受限。
    ctx.disp.handle(msg("c1", "bob", "/perm")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("当前权限模式")),
        "查看模式应放行: {inbox:?}"
    );
    drop(inbox);
    // 管理员切换成功。
    ctx.disp.handle(msg("c1", "alice", "/perm allow")).await;
    assert!(matches!(
        *ctx.disp.permission_mode.read(),
        PermissionMode::Allow
    ));
    drop_db(ctx.db).await;
}

/// P5-3：/disallow 须管理员——此前任何过门用户可把管理员本人踢出白名单。
#[tokio::test]
async fn disallow_requires_admin() {
    let _serial = SERIAL.lock().await;
    let ctx = build_with_admin(
        Auth::new(vec!["alice".into(), "bob".into(), "carol".into()]),
        vec!["alice".into()],
    )
    .await;
    // 非管理员 bob 踢 carol → 拒绝，carol 仍在白名单。
    ctx.disp.handle(msg("c1", "bob", "/disallow carol")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("仅管理员")),
        "应回拒绝提示: {inbox:?}"
    );
    drop(inbox);
    assert!(
        ctx.disp.auth.is_allowed(&UserId("carol".into())),
        "carol 应仍在白名单"
    );
    // 管理员 alice 撤销成功。
    ctx.disp.handle(msg("c1", "alice", "/disallow carol")).await;
    assert!(
        !ctx.disp.auth.is_allowed(&UserId("carol".into())),
        "carol 应已被移除"
    );
    drop_db(ctx.db).await;
}

/// P5-5：首轮任务被 /stop 中断，但 backend 已 announce 的 session id 应落库——
/// 下条消息续接该会话，而非静默开新会话（"失忆"）。
#[tokio::test]
async fn stop_persists_learned_session() {
    let _serial = SERIAL.lock().await;
    let ctx = build_slow_with_session(
        Auth::new(vec!["alice".into()]),
        60_000,
        "sess-learned",
        test_budgets(),
    )
    .await;
    // 首条消息起飞（backend 记录后即挂起，但已 announce sess-learned）。
    // handle 内联等整轮，慢后端须 spawn 驱动（同 stop_aborts_running_task）。
    let disp = ctx.disp.clone();
    let runner = tokio::spawn(async move {
        disp.handle(msg("c1", "alice", "first")).await;
    });
    assert!(wait_registered(&ctx, "c1").await, "任务应注册在飞");
    // /stop 中断。
    ctx.disp.handle(msg("c1", "alice", "/stop")).await;
    let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
    assert!(done.is_ok(), "被中断的 runner 应很快退出");
    // persist 在轮次结束（running 移除）之前完成，此处应已观察到。
    assert!(
        !ctx.disp.running.lock().await.contains_key("c1"),
        "在飞注册应清空"
    );
    // 下条消息：应续接学到的 sess-learned（而非 None 开新会话）。
    let disp = ctx.disp.clone();
    let runner2 = tokio::spawn(async move {
        disp.handle(msg("c1", "alice", "after stop")).await;
    });
    for _ in 0..400 {
        if ctx.calls.lock().await.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let calls = ctx.calls.lock().await.clone();
    assert_eq!(
        calls.last(),
        Some(&Some("sess-learned".to_string())),
        "中断后下条消息应续接学到的 session: {calls:?}"
    );
    // 收尾：中断第二个在飞任务再关库。
    ctx.disp.handle(msg("c1", "alice", "/stop")).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), runner2).await;
    drop_db(ctx.db).await;
}

/// P5-10：非卡片平台流式 Text 已实时推送——最终回复只补差量，不整段重发
/// （codex/gemini/ACP 的「中间 Text + Final 全量」语义此前会推两遍）。
#[tokio::test]
async fn streamed_text_not_duplicated_on_plain_platform() {
    let _serial = SERIAL.lock().await;
    let ctx = build_streaming(
        Auth::new(vec!["alice".into()]),
        vec!["答案第一段。".to_string(), "答案第二段。".to_string()],
    )
    .await;
    feed_and_wait(&ctx, vec![msg("c1", "alice", "问题")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    // 两段流式文本都应实时推送。
    assert!(
        inbox.iter().any(|t| t == "答案第一段。"),
        "应实时推送第一段: {inbox:?}"
    );
    assert!(
        inbox.iter().any(|t| t == "答案第二段。"),
        "应实时推送第二段: {inbox:?}"
    );
    // 全量文本不应作为最终回复再发一遍。
    let dup = inbox.iter().filter(|t| t.contains("答案第一段。")).count();
    assert_eq!(dup, 1, "Final 全量不应重发: {inbox:?}");
    drop_db(ctx.db).await;
}

/// P5-15：本机会话 cwd 与当前 workdir 不符时拒绝接管（防目录编码冲突串项目）。
#[tokio::test]
async fn resume_rejects_local_session_cwd_mismatch() {
    let _serial = SERIAL.lock().await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let ctx = build_with_local(
        Auth::new(vec!["alice".into()]),
        vec![LocalSession {
            session_id: "pc-other".to_string(),
            updated_at: now,
            first_prompt: "别的项目".to_string(),
            cwd: Some("/other/project".to_string()),
        }],
    )
    .await;
    ctx.disp.handle(msg("c1", "alice", "/resume")).await;
    ctx.disp.handle(msg("c1", "alice", "/resume 1")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("属于其它目录") && t.contains("/cd")),
        "cwd 不符应拒绝接管并引导 /cd: {inbox:?}"
    );
    // 未接管：session 映射不应变化。
    assert!(ctx.disp.running.lock().await.is_empty(), "无在飞任务");
    drop_db(ctx.db).await;
}

/// P5-9b：权限 socket 握手 token——错 token 连接被丢弃（无询问无回复）；
/// 正确 token 的请求触发 IM 询问，cancel 立即唤醒回 deny（P5-16）。
#[cfg(unix)]
#[tokio::test]
async fn permission_socket_token_handshake() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    let dir = std::env::temp_dir().join(format!("imagent-sock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("permission.sock");
    ctx.disp
        .spawn_socket_accept(sock.to_string_lossy().into_owned());
    // 等 socket 与 token 文件就绪。
    let token_path = dir.join("permission.token");
    for _ in 0..400 {
        if sock.exists() && token_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let token = std::fs::read_to_string(&token_path)
        .unwrap()
        .trim()
        .to_string();
    assert!(!token.is_empty(), "token 应已生成");

    // 错 token：连接被丢弃（不应有询问，也不应有任何回复）。
    {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut s = tokio::net::UnixStream::connect(&sock).await.unwrap();
        s.write_all(b"wrong-token\n{\"conv_id\":\"c1\"}\n")
            .await
            .unwrap();
        s.flush().await.unwrap();
        let _ = s.shutdown().await;
        let mut buf = String::new();
        let mut r = tokio::io::BufReader::new(s);
        let n = tokio::time::timeout(Duration::from_millis(300), r.read_line(&mut buf))
            .await
            .unwrap_or(Ok(0))
            .unwrap_or(0);
        assert_eq!(n, 0, "错 token 不应有任何回复: {buf}");
    }

    // 正确 token + 请求 → IM 询问送达；cancel 立即（而非 300s 后）回 deny。
    {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut s = tokio::net::UnixStream::connect(&sock).await.unwrap();
        s.write_all(format!("{token}\n").as_bytes()).await.unwrap();
        s.write_all(b"{\"conv_id\":\"c1\",\"tool_name\":\"Bash\",\"input\":{\"cmd\":\"ls\"}}\n")
            .await
            .unwrap();
        s.flush().await.unwrap();
        let mut asked = false;
        for _ in 0..400 {
            if ctx
                .inbox
                .lock()
                .await
                .iter()
                .any(|t| t.contains("请求执行"))
            {
                asked = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(asked, "正确 token 的请求应触发 IM 询问");
        ctx.disp.router.cancel("c1", "legacy").await;
        let mut buf = String::new();
        let mut r = tokio::io::BufReader::new(s);
        let _ = tokio::time::timeout(Duration::from_secs(2), r.read_line(&mut buf)).await;
        assert!(buf.contains("\"allow\":false"), "cancel 应回 deny: {buf}");
    }
    let _ = std::fs::remove_dir_all(&dir);
    drop_db(ctx.db).await;
}

/// P5-第五批：/stop 可中断 /compact（注册进 running；被中断后回异常提示，
/// 在飞注册清空）。
#[tokio::test]
async fn stop_aborts_compact() {
    let _serial = SERIAL.lock().await;
    let ctx = build_slow(Auth::new(vec!["alice".into()]), 60_000, test_budgets()).await;
    // 预置活动 session（/compact 需已有会话；不经消息路径避免慢后端卡住）。
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    ctx.check()
        .await
        .upsert_session(&imagent_store::SessionRow {
            conv_id: "c1".into(),
            session_id: "sess-9".into(),
            agent_kind: "mock-backend".into(),
            workdir: "/tmp/imagent-test-ws".into(),
            name: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let disp = ctx.disp.clone();
    let runner = tokio::spawn(async move {
        disp.handle(msg("c1", "alice", "/compact")).await;
    });
    assert!(wait_registered(&ctx, "c1").await, "/compact 任务应注册在飞");
    ctx.disp.handle(msg("c1", "alice", "/stop")).await;
    let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
    assert!(done.is_ok(), "被中断的 /compact 应很快退出");
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("摘要任务异常")),
        "应回中断提示: {inbox:?}"
    );
    assert!(ctx.disp.running.lock().await.is_empty(), "在飞注册应清空");
    drop_db(ctx.db).await;
}

/// P5-第五批：backend 报错但已 announce session——Err 路径也持久化，
/// 下条消息续接而非静默开新会话。
#[tokio::test]
async fn backend_error_persists_learned_session() {
    let _serial = SERIAL.lock().await;
    let ctx = build_announce_fail(Auth::new(vec!["alice".into()]), "sess-err").await;
    feed_and_wait(&ctx, vec![msg("c1", "alice", "boom")], 1).await;
    // 第一轮 Err 但已 announce；若持久化生效，第二轮 existing = Some(sess-err)。
    feed_and_wait(&ctx, vec![msg("c1", "alice", "again")], 2).await;
    let calls = ctx.calls.lock().await.clone();
    assert_eq!(
        calls,
        vec![None, Some("sess-err".to_string())],
        "Err 后下条消息应续接学到的 session: {calls:?}"
    );
}

#[cfg(all(test, unix))]
mod permission_socket_tests {
    use crate::dispatch::socket::peer_uid;

    #[tokio::test]
    async fn peer_uid_returns_self_for_local_pair() {
        // socketpair 两端同进程，peer_uid 必须返回本进程 uid。
        let (a, b) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        // from_std 要求非阻塞 socket（tokio issue #7172）。
        a.set_nonblocking(true).expect("set_nonblocking");
        let ta = tokio::net::UnixStream::from_std(a).expect("from_std");
        let got = peer_uid(&ta).expect("peer_uid 对本地连接应返回 Some");
        let self_uid = crate::dispatch::socket::current_uid();
        assert_eq!(got, self_uid);
        drop(ta);
        drop(b);
    }
}
