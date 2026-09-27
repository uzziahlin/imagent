use super::*;
use crate::types::{ConvId, LocalSession, Mention, ReplyHint, SessionId, UserId};
// v13 P3：媒体提示构造（round 模块私有辅助，测试直调存在性矩阵）。
use super::round::media_hint_for;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;

// 注：read_line_capped 的读行测试已随 P3 还债批迁移至 `crate::lineio` 的测试
// 模块（原 Dispatcher::read_line_capped 重复实现已删除，两处共用一份）。

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
/// P2（v13）：MockPlatform 入站队列句柄（编程 run() 的 recv 流用）。
type RecvQueueHandle = Arc<TokioMutex<Option<Vec<InboundMessage>>>>;

// ---------- mock platform ----------

/// mock platform：`inbox` 收到的出站文本，`recv_queue` 可编程的入站流。
/// Wave B：`urgent` 变体支持加急文本（supports_urgent_text = true，加急消息以
/// `[buzz] ` 前缀记入 inbox——完成强提醒/催办测试断言用）。
/// P3（v13 批）：`reactions` 记录表情标注（react_to_message 全记录）；
/// `typing_gate` 把 send_typing 钉在可观测窗口（见 TypingGate）。
struct MockPlatform {
    recv_queue: Arc<TokioMutex<Option<Vec<InboundMessage>>>>,
    inbox: Arc<TokioMutex<Vec<String>>>,
    send_count: Arc<AtomicUsize>,
    urgent: bool,
    /// 表情标注记录（stop-拦截收口测试用；默认空收，不影响既有断言）。
    reactions: Arc<TokioMutex<Vec<(String, crate::types::MsgReaction)>>>,
    /// typing 闸门（默认 None：send_typing 立即返回）。
    typing_gate: Option<TypingGate>,
    /// P2（v13）：resolve_permission_ask 闸门（默认 None：立即返回）。Some 时
    /// 先记 [resolve-enter] 标记再挂起等 notify——验证 recv 循环不被慢收敛
    /// 卡死（spawn 化回归）。
    resolve_gate: Option<Arc<tokio::sync::Notify>>,
}

impl MockPlatform {
    fn new() -> (Self, InboxHandle, CounterHandle) {
        let inbox = Arc::new(TokioMutex::new(Vec::new()));
        let send_count = Arc::new(AtomicUsize::new(0));
        let p = Self {
            recv_queue: Arc::new(TokioMutex::new(None)),
            inbox: inbox.clone(),
            send_count: send_count.clone(),
            urgent: false,
            reactions: Arc::new(TokioMutex::new(Vec::new())),
            typing_gate: None,
            resolve_gate: None,
        };
        (p, inbox, send_count)
    }

    /// Wave B-2：支持加急的变体（supports_urgent_text = true）。
    fn new_urgent() -> (Self, InboxHandle, CounterHandle) {
        let (mut p, inbox, count) = Self::new();
        p.urgent = true;
        (p, inbox, count)
    }

    /// P2（v13）：resolve_permission_ask 挂起变体（闸门由测试持有）；一并返回
    /// recv 队列句柄（编程 run() 的入站流用）。
    #[allow(clippy::type_complexity)]
    fn new_slow_resolve() -> (
        Self,
        InboxHandle,
        CounterHandle,
        RecvQueueHandle,
        Arc<tokio::sync::Notify>,
    ) {
        let (mut p, inbox, count) = Self::new();
        let gate = Arc::new(tokio::sync::Notify::new());
        p.resolve_gate = Some(gate.clone());
        let queue = p.recv_queue.clone();
        (p, inbox, count, queue, gate)
    }
}

/// P3 stop-拦截测试的 typing 闸门：send_typing 置 entered 后挂起等一次性
/// release 信号——把 run_round_inner 的 preamble 钉在「停止水位已读（line ~78）、
/// 👀 未打（line ~236）」之间的确定窗口里，测试在窗口内注入停止标记，精确
/// 命中起跑前的二次复查分支（而非批循环顶部的首次检查）。
#[derive(Clone)]
struct TypingGate {
    entered: Arc<std::sync::atomic::AtomicBool>,
    release: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
}

impl TypingGate {
    fn new() -> (Self, tokio::sync::oneshot::Sender<()>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (
            Self {
                entered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                release: Arc::new(std::sync::Mutex::new(Some(rx))),
            },
            tx,
        )
    }

    async fn hold(&self) {
        self.entered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // 先把 Receiver 取出再 await——std MutexGuard 不能跨 await（Send 约束）。
        let rx = self.release.lock().unwrap().take();
        if let Some(rx) = rx {
            let _ = rx.await;
        }
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
    /// Wave B：加急文本以 `[buzz] ` 前缀记录（与普通消息区分断言）。
    async fn send_urgent_text(&self, _conv: &ConvId, text: &str, _hint: &ReplyHint) -> Result<()> {
        self.inbox.lock().await.push(format!("[buzz] {text}"));
        self.send_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn supports_urgent_text(&self) -> bool {
        self.urgent
    }
    /// P3：typing 闸门（默认立即返回；测试置 gate 后挂起等 release）。
    async fn send_typing(&self, _conv: &ConvId, _hint: &ReplyHint) -> Result<()> {
        if let Some(g) = &self.typing_gate {
            g.hold().await;
        }
        Ok(())
    }
    /// P3：表情标注全记录（stop-拦截收口测试断言用）。
    async fn react_to_message(
        &self,
        _conv: &ConvId,
        source_msg_id: &str,
        reaction: crate::types::MsgReaction,
    ) -> Result<()> {
        self.reactions
            .lock()
            .await
            .push((source_msg_id.to_string(), reaction));
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
    /// P2（v13）：闸门变体——记录「已进入」后挂起等 notify（30s 兜底防悬挂）。
    async fn resolve_permission_ask(
        &self,
        _conv: &ConvId,
        _request_id: &str,
        _reply: &crate::permission::PermissionReply,
    ) -> Result<()> {
        if let Some(g) = &self.resolve_gate {
            self.inbox.lock().await.push("[resolve-enter]".to_string());
            let _ = tokio::time::timeout(Duration::from_secs(30), g.notified()).await;
        }
        Ok(())
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
    /// steering（v1.17）：supports_steering 返回值 + 注入文本捕获。
    steerable: bool,
    steer_seen: Arc<TokioMutex<Vec<String>>>,
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
    /// P2-11：发完 stream_texts 后挂起 60s（无 Final、不返回）——中断路径的
    /// 合帧缓冲 flush 测试用（等 /stop abort）。
    stream_then_hang: bool,
    /// P5-第五批：announce session 后直接返 Err（Err 路径 session 持久化测试用）。
    fail_after_announce: Option<String>,
    /// `list_local_sessions` 返回的本机会话（P4-11 统一 /resume 测试用）。
    local_sessions: Arc<TokioMutex<Vec<LocalSession>>>,
    /// S-1/S-2：权限能力档位（默认 Unsupported；FullLoop 供闭环类测试覆写）。
    capability: crate::backend::PermissionCapability,
    /// T4（P1-3）：supports_tool_allowlist 能力位（默认 false = 不支持逐工具
    /// 白名单，能力面告警测试用）。
    allowlist_supported: bool,
    /// RunOutcome 携带的 usage（run_stats 落库/自动压缩阈值测试用；默认 None）。
    usage: Option<crate::types::UsageStats>,
    /// P2（v13）：完成闸门（默认 None：立即完成）。Some 时 run 记录调用后挂起
    /// 等 notify_waiters——并发护栏测试用（精确控制「轮次在飞」窗口）。
    complete_gate: Option<Arc<tokio::sync::Notify>>,
    /// T11（/tasks 面板）：进入完成闸门前先发的 TodoList / ToolUse chunk——制造
    /// 「轮次在飞且已有内部进度」的窗口（默认空 = 行为不变）。
    pre_gate_todos: Vec<crate::types::TodoItem>,
    pre_gate_tools: Vec<(String, String)>,
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
            steerable: false,
            steer_seen: Arc::new(TokioMutex::new(Vec::new())),
            terminal: true,
            slow_ms: 0,
            announce_session: None,
            stream_texts: Vec::new(),
            stream_then_hang: false,
            fail_after_announce: None,
            local_sessions: Arc::new(TokioMutex::new(Vec::new())),
            capability: crate::backend::PermissionCapability::Unsupported,
            allowlist_supported: false,
            usage: None,
            complete_gate: None,
            pre_gate_todos: Vec::new(),
            pre_gate_tools: Vec::new(),
        };
        (b, calls, prompts, order)
    }

    /// Wave B-9：带 usage 的变体（RunOutcome.usage 透传，上下文水位测试用）。
    fn new_with_usage(
        usage: crate::types::UsageStats,
    ) -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
        let (mut b, calls, prompts, order) = Self::new();
        b.usage = Some(usage);
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
    /// P2-11：发完 delta 后挂起（无 Final）——中断路径缓冲 flush 测试用。
    fn new_stream_then_hang(
        texts: Vec<String>,
    ) -> (Self, CallsHandle, PromptsHandle, CounterHandle) {
        let (mut b, calls, prompts, order) = Self::new();
        b.stream_texts = texts;
        b.stream_then_hang = true;
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
    /// T11（/tasks 面板测试）：gated 变体 + 挂起前先发 TodoList / ToolUse chunk
    /// ——「轮次在飞且已有内部进度」的精确窗口。
    fn new_gated_with_progress(
        todos: Vec<crate::types::TodoItem>,
        tools: Vec<(String, String)>,
    ) -> (
        Self,
        CallsHandle,
        PromptsHandle,
        CounterHandle,
        Arc<tokio::sync::Notify>,
    ) {
        let (mut b, calls, prompts, order) = Self::new();
        let gate = Arc::new(tokio::sync::Notify::new());
        b.complete_gate = Some(gate.clone());
        b.pre_gate_todos = todos;
        b.pre_gate_tools = tools;
        (b, calls, prompts, order, gate)
    }
    /// P2（v13）：完成闸门变体——run 开跑（调用已记录）后挂起等 notify_waiters，
    /// 精确制造「轮次在飞」窗口（并发护栏测试用）。
    fn new_gated() -> (
        Self,
        CallsHandle,
        PromptsHandle,
        CounterHandle,
        Arc<tokio::sync::Notify>,
    ) {
        let (mut b, calls, prompts, order) = Self::new();
        let gate = Arc::new(tokio::sync::Notify::new());
        b.complete_gate = Some(gate.clone());
        (b, calls, prompts, order, gate)
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
        _initial_todos: &[crate::types::TodoItem],
        steer: mpsc::Receiver<String>,
    ) -> Result<crate::types::RunOutcome> {
        // P0-5 测试：prompt 记录先于 fail 判断（失败轮也要捕获收到的 prompt）。
        self.prompts.lock().await.push(prompt.to_string());
        // steering（v1.17）：drain 转向通道并记录（sender drop 即结束）。
        {
            let seen = self.steer_seen.clone();
            let mut steer = steer;
            tokio::spawn(async move {
                while let Some(t) = steer.recv().await {
                    seen.lock().await.push(t);
                }
            });
        }
        // P1-7 测试：fail 模式直接返 Err，触发 handle 失败路径。
        if self.fail {
            return Err(crate::error::CoreError::Backend(
                "mock-backend",
                "mock failure (fail=true)".into(),
            ));
        }
        // 记录续接情况 + 执行顺序。
        let my_order = self.order.fetch_add(1, Ordering::SeqCst);
        self.calls.lock().await.push(session.map(|s| s.0.clone()));

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

        // T11（/tasks 面板）：进闸门前先发进度 chunk（默认空 = 行为不变）——
        // 消费循环把它们写进 RoundSnapshot 后挂起，面板查询窗口即就绪。
        if !self.pre_gate_todos.is_empty() {
            let _ = chunks
                .send(AgentChunk::TodoList {
                    items: self.pre_gate_todos.clone(),
                })
                .await;
        }
        for (tool, input) in &self.pre_gate_tools {
            let _ = chunks
                .send(AgentChunk::ToolUse {
                    tool: tool.clone(),
                    input: input.clone(),
                    id: None,
                })
                .await;
        }

        // P2（v13）：完成闸门——调用已记录（在飞可观测）后挂起，等测试放行。
        if let Some(g) = &self.complete_gate {
            let _ = tokio::time::timeout(Duration::from_secs(60), g.notified()).await;
        }

        // P2-11：发完 delta 即挂起（不发 Final、不返回）——等 /stop abort。
        if self.stream_then_hang {
            for t in &self.stream_texts {
                let _ = chunks.send(AgentChunk::Text(t.clone())).await;
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
            // 正常测试路径在上一行被 abort，走不到这里；返回值仅为类型完备。
            return Ok(crate::types::RunOutcome {
                session_id: SessionId("sess-hang".into()),
                final_text: String::new(),
                terminal: false,
                usage: None,
                stop_reason: None,
            });
        }

        // P4：慢后端——记录后挂起（不发任何 chunk），供 /stop、批处理、空闲
        // 看门狗测试制造「在飞任务」窗口。
        if self.slow_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.slow_ms)).await;
        }

        // 先发配置好的 ToolUse chunk（若有），再发 Final。
        let tools = self.tools_to_emit.lock().await.clone();
        for (tool, input) in tools {
            let _ = chunks
                .send(AgentChunk::ToolUse {
                    tool,
                    input,
                    id: None,
                })
                .await;
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
            usage: self.usage,
            stop_reason: None,
        })
    }
    fn name(&self) -> &'static str {
        "mock-backend"
    }

    fn supports_steering(&self) -> bool {
        self.steerable
    }
    async fn list_local_sessions(&self, _workdir: &std::path::Path) -> Vec<LocalSession> {
        self.local_sessions.lock().await.clone()
    }
    fn permission_capability(&self) -> crate::backend::PermissionCapability {
        self.capability
    }
    fn supports_tool_allowlist(&self) -> bool {
        self.allowlist_supported
    }
}

// ---------- helpers ----------

fn msg(conv: &str, sender: &str, text: &str) -> InboundMessage {
    InboundMessage {
        conv_id: ConvId(conv.into()),
        sender: UserId(sender.into()),
        sender_name: None,
        text: Some(text.into()),
        media: Vec::new(),
        media_errors: Vec::new(),
        mentions: Vec::new(),
        mentioned_bot: false,
        ask_req: None,
        reply_to: None,
        source_msg_id: None,
        control: None,
        no_steer: false,
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
/// Wave B-10：目录不存在则创建——run_round_inner 现对 workdir 做 is_dir 预检，
/// mock 后端虽不读目录，预检仍会拦下本轮（保持与生产行为一致的测试语义）。
async fn build_with_workdir(auth: Auth, default_workdir: std::path::PathBuf) -> Ctx {
    let _ = std::fs::create_dir_all(&default_workdir);
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
/// 与 build 相同，但可指定 permission_mode（B3 启动 fail-closed 校验测试用）。
async fn build_with_mode(auth: Auth, mode: PermissionMode) -> Ctx {
    let (plat, inbox, send_count) = MockPlatform::new();
    let (back, calls, prompts, order) = MockBackend::new();
    let (store, db) = tmp_store().await;
    let admins = auth.snapshot();
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        auth,
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into(), "Edit".into()],
        mode,
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

/// T4（能力面矩阵）：与 build 相同，但可指定 allowed_tools / 权限档位 /
/// MockBackend 能力位（P1-3 白名单能力 + P3-3 审批能力，告警矩阵测试用）。
async fn build_capability(
    auth: Auth,
    tools: Vec<String>,
    mode: PermissionMode,
    capability: crate::backend::PermissionCapability,
    allowlist_supported: bool,
) -> Ctx {
    let (plat, inbox, send_count) = MockPlatform::new();
    let (mut back, calls, prompts, order) = MockBackend::new();
    back.capability = capability;
    back.allowlist_supported = allowlist_supported;
    let (store, db) = tmp_store().await;
    let admins = auth.snapshot();
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        auth,
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        tools,
        mode,
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
        // W2-5：测试默认关闭自动 compact（个别用例显式开启）；W4-1 成本上限默认不限。
        // v1.20：窗口原料默认 0（绝对值档语义，学习 no-op）。
        auto_compact_threshold_tokens: 0,
        auto_compact_window_tokens: 0,
        auto_compact_window_ratio: 0.8,
        cron_catchup: crate::config::CronCatchup::One,
        sender_daily_cost_limit_usd: None,
        // P2（v13）：既有用例默认不限制并发（护栏用例显式设置上限）。
        max_concurrent_rounds: 0,
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

/// P2-11：发完 delta 挂起的流式后端（中断路径缓冲 flush 测试用）。
async fn build_stream_then_hang(auth: Auth, texts: Vec<String>) -> Ctx {
    let (plat, inbox, send_count) = MockPlatform::new();
    let (back, calls, prompts, order) = MockBackend::new_stream_then_hang(texts);
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

// ---------- P1-1 集成吞吐锚：卡片能力平台 mock ----------

/// 卡片能力平台 mock：send_card 恒成功返回句柄，update_card 全记录文本快照
///（模拟 feishu 卡片路径；平台侧无节流——节流在 CardSession）。文本路径同
/// MockPlatform（记 inbox）。
struct CardCapablePlatform {
    inbox: Arc<TokioMutex<Vec<String>>>,
    send_count: Arc<AtomicUsize>,
    updates: Arc<TokioMutex<Vec<String>>>,
}

#[async_trait]
impl Platform for CardCapablePlatform {
    async fn recv(&self) -> Result<InboundMessage> {
        // 测试不经 run/recv（直接 handle），永不返回。
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
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
        _media: &crate::types::MediaRef,
        _hint: &ReplyHint,
    ) -> Result<()> {
        Ok(())
    }
    fn name(&self) -> &'static str {
        "mock-cards"
    }
    fn supports_streaming_card(&self, _conv: &ConvId) -> bool {
        true
    }
    async fn send_card(
        &self,
        _conv: &ConvId,
        _card: &crate::types::OutboundCard,
        _hint: &ReplyHint,
    ) -> Result<Option<String>> {
        Ok(Some("card:tput".into()))
    }
    async fn update_card(
        &self,
        _conv: &ConvId,
        _message_id: &str,
        card: &crate::types::OutboundCard,
        _hint: &ReplyHint,
    ) -> Result<()> {
        self.updates.lock().await.push(card.text.clone());
        Ok(())
    }
}

/// P1-1：卡片平台 + delta 级流式后端的 dispatcher（集成吞吐锚用）。
/// 返回 Ctx 与 update_card 收到的文本快照句柄。
async fn build_card_streaming(
    auth: Auth,
    texts: Vec<String>,
) -> (Ctx, Arc<TokioMutex<Vec<String>>>) {
    let (back, calls, prompts, order) = MockBackend::new_streaming(texts);
    let plat = CardCapablePlatform {
        inbox: Arc::new(TokioMutex::new(Vec::new())),
        send_count: Arc::new(AtomicUsize::new(0)),
        updates: Arc::new(TokioMutex::new(Vec::new())),
    };
    let inbox = plat.inbox.clone();
    let send_count = plat.send_count.clone();
    let updates = plat.updates.clone();
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

    (
        Ctx {
            disp,
            inbox,
            send_count,
            calls,
            prompts,
            order,
            db,
        },
        updates,
    )
}

// T18 机械调整辅助：在飞轮次原活在独立 running map，并入 ConvState 单表后
// 测试断言改走下列只读辅助（语义与原 contains_key / len / is_empty 一致）。
async fn conv_running(disp: &Dispatcher, conv: &str) -> bool {
    disp.peek_conv(conv, |cs| cs.is_some_and(|c| c.running.is_some()))
        .await
}

async fn running_rounds(disp: &Dispatcher) -> usize {
    let map = disp.conv_states.lock().await;
    map.values().filter(|cs| cs.running.is_some()).count()
}

/// T18 机械调整辅助：conv 的排队条数（原 queues map get(conv).len()）。
async fn conv_queued_len(disp: &Dispatcher, conv: &str) -> usize {
    disp.peek_conv(conv, |cs| {
        cs.and_then(|c| c.queue.as_ref()).map_or(0, Vec::len)
    })
    .await
}

/// T18 机械调整辅助：是否有 conv 挂起队列（原 queues map is_empty 的反义；
/// 留空 Vec 的 entry 也算——与旧 entry 存在语义一致）。
async fn any_queued(disp: &Dispatcher) -> bool {
    let map = disp.conv_states.lock().await;
    map.values().any(|cs| cs.queue.is_some())
}

/// 等待 conv 的在飞任务注册出现（join spawn 后写入 ConvState.running）。
async fn wait_registered(ctx: &Ctx, conv: &str) -> bool {
    for _ in 0..400 {
        if conv_running(&ctx.disp, conv).await {
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
    let ctx = build_with_mode(Auth::new(vec!["alice".into()]), PermissionMode::Ask).await;
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
    let ctx = build_with_mode(Auth::new(vec!["alice".into()]), PermissionMode::Ask).await;
    let m = InboundMessage {
        conv_id: ConvId("feishu:ou_t".into()),
        sender: UserId("alice".into()),
        sender_name: None,
        text: None,
        media: vec![],
        media_errors: vec!["img_x: 下载失败: boom".into()],
        mentions: Vec::new(),
        mentioned_bot: false,
        ask_req: None,
        reply_to: None,
        source_msg_id: None,
        control: None,
        no_steer: false,
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
    ctx.disp
        .set_prefs(true, false, crate::config::ReplyMode::Card);
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
    // 表单键全部为全局热改键：admin 门槛之后才应用（安全修复）。
    let ctx = build_with_admin(Auth::new(vec!["alice".into()]), vec!["alice".into()]).await;
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

/// 表单提交的 admin 门槛：admin_senders 非空时，白名单内非 admin 用户提交
/// `/config form` 被拒且配置不变（防表单绕过——表单键全部为全局热改键）。
#[tokio::test]
async fn config_form_rejected_for_non_admin() {
    let _serial = SERIAL.lock().await;
    let ctx = build_with_admin(Auth::new(vec!["alice".into()]), vec!["boss".into()]).await;
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
        inbox.last().is_some_and(|t| t.contains("管理员")),
        "非 admin 表单提交应被拒: {inbox:?}"
    );
    assert_eq!(
        *ctx.disp.reply_mode.read(),
        ReplyMode::Card,
        "配置不应被改动"
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
    // v1.23 表格化：| 名称 | 时间 | 内容 |（内容 = first_prompt 摘要）。
    assert!(
        listing.contains("| 名称 | 时间 | 内容 |"),
        "listing={listing}"
    );
    assert!(listing.contains("alpha"), "listing={listing}");
    assert!(listing.contains("beta"), "listing={listing}");
    // 会话摘要可辨认（v1.23 核心目标）：各自的 first_prompt 出现在列表里。
    assert!(
        listing.contains("a work"),
        "alpha 摘要应可见，listing={listing}"
    );
    assert!(
        listing.contains("b work"),
        "beta 摘要应可见，listing={listing}"
    );
    // beta 为活动，应带（当前）；alpha 不应带。
    assert!(
        listing.contains("beta *（当前）*"),
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
    // P10-④：多说话人（u1/u2）标注归属；空文本段（u3）跳过不标注。
    assert_eq!(m.text.as_deref(), Some("【u1】first\n\n【u2】second"));
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

    // P10-④：同一发送者连发——不加说话人标注（避免噪音）。
    let same = merge_batch(vec![msg("c1", "u1", "aaa"), msg("c1", "u1", "bbb")]);
    assert_eq!(same.text.as_deref(), Some("aaa\n\nbbb"));
}

/// P10：排队状态展示文案——计数 + 最新摘要（40 字符截断）；零计数不展示。
#[test]
fn queued_hint_display_shapes() {
    use crate::card_session::{queued_hint_display, QueuedHint};
    assert!(queued_hint_display(&QueuedHint::default()).is_none());
    let h = QueuedHint {
        count: 2,
        latest: "别用 npm，改用 pnpm".into(),
        ..Default::default()
    };
    assert_eq!(
        queued_hint_display(&h).as_deref(),
        Some("📥 排队 2 条，最新：「别用 npm，改用 pnpm」")
    );
    let long = QueuedHint {
        count: 1,
        latest: "x".repeat(100),
        ..Default::default()
    };
    let out = queued_hint_display(&long).unwrap();
    assert!(out.chars().count() <= "📥 排队 1 条，最新：「」".chars().count() + 40);
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
    assert_eq!(running_rounds(&ctx.disp).await, 0, "在飞注册应清空");
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

/// P4-1/P4-2 → W1-1：`/stop all` 硬停——丢弃排队消息，回复含丢弃条数。
/// （缺省 `/stop` 已改为保留排队并自动续跑，见下方 steering 测试。）
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
        if conv_queued_len(&ctx.disp, "c1").await == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    ctx.disp.handle(msg("c1", "alice", "/stop all")).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), runner).await;
    let inbox = ctx.inbox.lock().await.clone();
    // 真机校准（2026-08）：回执改命令卡——mock 走 trait 默认文本降级
    //（title + body 两行），标题与丢弃条数分别断言。
    assert!(
        inbox.iter().any(|t| t.contains("已中断当前任务")),
        "应回中断标题: {inbox:?}"
    );
    assert!(
        inbox.iter().any(|t| t.contains("已丢弃 2 条排队消息")),
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

/// W1-1（steering）：缺省 `/stop` 保留排队消息——runner 在中断后自动取批续跑
/// （对齐 Claude Code Esc + 队列注入语义）；回复说明保留条数与 /stop all 逃生口。
#[tokio::test]
async fn stop_preserves_queued_messages_and_continues() {
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
    ctx.disp.handle(msg("c1", "alice", "queued B")).await;
    ctx.disp.handle(msg("c1", "alice", "queued C")).await;
    for _ in 0..400 {
        if conv_queued_len(&ctx.disp, "c1").await == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    ctx.disp.handle(msg("c1", "alice", "/stop")).await;
    let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
    assert!(done.is_ok(), "runner 应在续跑第二轮后退出");
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("2 条排队消息已保留，将自动转入新一轮")),
        "回复应说明保留条数与续跑: {inbox:?}"
    );
    let prompts = ctx.prompts.lock().await.clone();
    assert_eq!(
        prompts,
        vec!["round A".to_string(), "queued B\n\nqueued C".to_string()],
        "排队消息应自动转入第二轮（合并）"
    );
    drop_db(ctx.db).await;
}

/// P0-5（v1.17）：/queue 列表展示 + 选择性丢弃（自己的可删、他人的需 admin）。
#[tokio::test]
async fn queue_list_and_selective_drop() {
    let _serial = SERIAL.lock().await;
    // 显式 admin 列表（仅 bob）——alice 非 admin 才能测「他人消息拒删」。
    let _ = std::fs::create_dir_all("/tmp/imagent-test-ws");
    let (plat, inbox, send_count) = MockPlatform::new();
    let (back, calls, prompts, order) = MockBackend::new_slow(300);
    let (store, db) = tmp_store().await;
    let auth = Auth::new(vec!["alice".into(), "bob".into()]);
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        auth,
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into()],
        PermissionMode::Off,
        TaskBudgets {
            batch_window: Duration::from_millis(1),
            ..test_budgets()
        },
        CotDetail::Brief,
        vec!["bob".into()],
    ));
    let ctx = Ctx {
        disp,
        inbox,
        send_count,
        calls,
        prompts,
        order,
        db: db.clone(),
    };
    let disp = ctx.disp.clone();
    let runner = tokio::spawn(async move {
        disp.handle(msg("c1", "alice", "round A")).await;
    });
    assert!(wait_registered(&ctx, "c1").await, "在飞任务应已注册");
    ctx.disp.handle(msg("c1", "bob", "bob 的补充")).await;
    ctx.disp.handle(msg("c1", "alice", "alice 的补充")).await;
    for _ in 0..400 {
        if conv_queued_len(&ctx.disp, "c1").await == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // 列表：两条带发送者与摘要。
    ctx.disp.handle(msg("c1", "alice", "/queue")).await;
    let inbox = ctx.inbox.lock().await.clone();
    // v1.25 卡片化：表格行含发送者（短 id）与摘要。
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("bob 的补充") && t.contains("alice 的补充")),
        "列表应含发送者与摘要: {inbox:?}"
    );
    // alice（非 admin）不能删 bob 的（第 1 条）。
    ctx.disp.handle(msg("c1", "alice", "/queue drop 1")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("只能丢弃自己排队的消息")),
        "他人消息应拒删: {inbox:?}"
    );
    // 删自己的（第 2 条）成功，剩 bob 的一条。
    ctx.disp.handle(msg("c1", "alice", "/queue drop 2")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("已丢弃第 2 条")),
        "删除回执: {inbox:?}"
    );
    // （T18 机械调整：queues 并入 ConvState 单表；entry 保留 = queue 仍在。）
    let (q_len, first_sender) = {
        let map = ctx.disp.conv_states.lock().await;
        map.get("c1")
            .and_then(|cs| cs.queue.as_ref())
            .map(|q| (q.len(), q[0].msg.sender.0.clone()))
            .expect("entry 保留")
    };
    assert_eq!(q_len, 1, "应剩 bob 的一条");
    assert_eq!(first_sender, "bob");
    drop_db(ctx.db).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), runner).await;
}

/// 快捷命令（v1.17）：config.shortcuts 的 /name → prompt 展开（$args 替换），
/// 走完整 agent 分派路径；未命中的斜杠词仍回未知命令提示。
#[tokio::test]
async fn shortcut_expands_to_prompt() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    let mut shortcuts = std::collections::HashMap::new();
    shortcuts.insert(
        "deploy".into(),
        "跑 ./scripts/deploy.sh 并汇报结果 $args".into(),
    );
    ctx.disp.set_shortcuts(shortcuts);
    ctx.disp
        .handle(msg("c1", "alice", "/deploy --dry-run"))
        .await;
    let prompts = ctx.prompts.lock().await.clone();
    assert_eq!(
        prompts,
        vec!["跑 ./scripts/deploy.sh 并汇报结果 --dry-run".to_string()],
        "快捷命令应展开为 prompt: {prompts:?}"
    );
    // 未命中 → 未知命令提示（不进 agent）。
    ctx.disp.handle(msg("c1", "alice", "/nosuch")).await;
    let prompts = ctx.prompts.lock().await.clone();
    assert_eq!(prompts.len(), 1, "未知命令不应进 agent");
    drop_db(ctx.db).await;
}

/// steering（v1.17）：运行中到达的文本消息经 steer 通道注入当轮（不排队、
/// 不再起第二轮）；实验校准 CLI 在下个工具边界交付。不支持转向的后端回落排队
///（supports_steering=false 时 running 句柄 steer=None）。
#[tokio::test]
async fn steering_injects_midround_text() {
    let _serial = SERIAL.lock().await;
    let _ = std::fs::create_dir_all("/tmp/imagent-test-ws");
    let (plat, inbox, send_count) = MockPlatform::new();
    let (mut back, calls, prompts, order) = MockBackend::new_slow(400);
    back.steerable = true;
    let steer_seen = back.steer_seen.clone();
    let (store, db) = tmp_store().await;
    let auth = Auth::new(vec!["alice".into()]);
    let admins = auth.snapshot();
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        auth,
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into()],
        PermissionMode::Off,
        TaskBudgets {
            batch_window: Duration::from_millis(1),
            ..test_budgets()
        },
        CotDetail::Brief,
        admins,
    ));
    let ctx = Ctx {
        disp: disp.clone(),
        inbox,
        send_count,
        calls,
        prompts: prompts.clone(),
        order,
        db: db.clone(),
    };
    let d = disp.clone();
    let runner = tokio::spawn(async move {
        d.handle(msg("c1", "alice", "round A")).await;
    });
    assert!(wait_registered(&ctx, "c1").await, "在飞任务应已注册");
    // 两条运行中消息 → 注入当轮（不排队）。
    ctx.disp.handle(msg("c1", "alice", "中途补充")).await;
    ctx.disp.handle(msg("c1", "alice", "再补充")).await;
    for _ in 0..400 {
        if steer_seen.lock().await.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // v1.23 说话人归属：群 conv（c1 非 p2p）的转向注入带【标注】。
    assert_eq!(
        steer_seen.lock().await.clone(),
        vec![
            "【alice】中途补充".to_string(),
            "【alice】再补充".to_string()
        ],
        "两条都应注入"
    );
    assert!(
        conv_queued_len(&ctx.disp, "c1").await == 0,
        "注入的消息不进队列"
    );
    let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
    assert!(done.is_ok(), "runner 应结束");
    // 只跑了初始一轮（注入不产生新轮次）。
    assert_eq!(prompts.lock().await.clone(), vec!["round A".to_string()]);
    drop_db(ctx.db).await;
}

/// steering（v1.17）：不支持转向的后端（缺省）——运行中消息回落排队，
/// 轮结束后合并为下一轮（旧行为保持）。
#[tokio::test]
async fn steering_unsupported_falls_back_to_queue() {
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
    assert!(wait_registered(&ctx, "c1").await);
    ctx.disp.handle(msg("c1", "alice", "运行中补充")).await;
    let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
    assert!(done.is_ok());
    let prompts = ctx.prompts.lock().await.clone();
    assert_eq!(
        prompts,
        vec!["round A".to_string(), "运行中补充".to_string()],
        "不支持转向时应排队合并进下一轮"
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
    assert_eq!(running_rounds(&ctx.disp).await, 0, "在飞注册应清空");
    assert!(
        !any_queued(&ctx.disp).await,
        "runner 退出后队列 entry 应移除"
    );
    drop_db(ctx.db).await;
}

/// v13 P3：看门狗豁免预算按本轮 Permission 审批次数放大——同轮两次慢审批
/// 的累计静默超过单次 permission_ask_timeout 不被误杀（旧实现钳死单次预算，
/// 第 N 个审批中途被杀）；审批 pending 清空后长静默仍照常判停（防挂死语义
/// 不变）。无审批场景由 idle_watchdog_terminates_silent_agent 覆盖。
#[tokio::test]
async fn watchdog_exempt_budget_scales_with_approval_count() {
    let _serial = SERIAL.lock().await;
    // 时基：idle=500ms（豁免 tick），permission_ask_timeout=2s（单份预算）。
    // 豁免按整秒烧（as_secs().max(1)）：旧实现 cap=2 → 第 3 个 tick（t≈1.5s）
    // 即判停；新实现 cap=2 份 ×2s=4s → t≈1.8s 仍在飞（最早 t≈2.5s 才可能
    // 判停），两档间隔足以区分回归。
    let ctx = build_slow(
        Auth::new(vec!["alice".into()]),
        60_000,
        TaskBudgets {
            agent_idle_timeout: Duration::from_millis(500),
            permission_ask_timeout: Duration::from_secs(2),
            batch_window: Duration::from_millis(1),
            ..test_budgets()
        },
    )
    .await;
    let disp = ctx.disp.clone();
    let runner = tokio::spawn(async move { disp.handle(msg("c1", "alice", "slow work")).await });
    assert!(wait_registered(&ctx, "c1").await, "轮次应已起跑");
    // 同轮两次审批（登记即计数；保持 pending 使 D3 豁免的 waiting 成立——
    // 慢后端不产 chunk，静默持续累积）。
    let _rx1 = ctx
        .disp
        .router
        .register(
            "c1",
            "req-w1",
            None,
            crate::permission::PendingKind::Permission,
            Some("Bash"),
            None,
        )
        .await;
    let _rx2 = ctx
        .disp
        .router
        .register(
            "c1",
            "req-w2",
            None,
            crate::permission::PendingKind::Permission,
            Some("Bash"),
            None,
        )
        .await;
    tokio::time::sleep(Duration::from_millis(1_800)).await;
    assert!(
        conv_running(&ctx.disp, "c1").await,
        "两次审批的累计静默不应触发看门狗"
    );
    {
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            !inbox.iter().any(|t| t.contains("空闲超时")),
            "不应有超时提示: {inbox:?}"
        );
    }
    // 审批 pending 清空 → waiting 失效 → 照常按预算判停。
    ctx.disp.router.cancel_all("c1").await;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if ctx
            .inbox
            .lock()
            .await
            .iter()
            .any(|t| t.contains("空闲超时"))
        {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("审批清空后应恢复看门狗判停: {:?}", ctx.inbox.lock().await);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), runner).await;
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
    ctx.disp.handle(msg("c1", "alice", "/stop all")).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), runner).await;
    let inbox = ctx.inbox.lock().await.clone();
    // v1.21 review：告警改为 per-conv 时间窗去重（洪泛源刷屏/平台频控防护）
    //——5 条超限只回 1 条告警（丢弃照常）。
    let overflow = inbox.iter().filter(|t| t.contains("已达上限")).count();
    assert_eq!(overflow, 1, "超限告警应按 conv 去重（只回一次）: {inbox:?}");
    let prompts = ctx.prompts.lock().await.clone();
    assert_eq!(
        prompts.len(),
        1,
        "只应有首轮（已 /stop all 硬停）: {prompts:?}"
    );
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
    // T16：/doctor 展示 bundled SQLite 版本（内嵌 C 代码的 CVE 不进 cargo-audit）。
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("🩺") && t.contains("SQLite 3.")),
        "/doctor 应含 SQLite 版本行: {inbox:?}"
    );
    // MockPlatform 未覆写 reconnect → 默认不支持，回告警而非成功。
    assert!(
        inbox.iter().any(|t| t.contains("重连指令失败")),
        "默认平台应报不支持重连: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

// ---------- T8（v13 安全批）：/doctor 安全自检 ----------

/// /doctor 端到端：「🛡️ 安全」分组六项检查接线可见。凭据维度用空表断言
/// （确定性：不 put_credential——store 作为依赖编译时 cfg!(test) 不生效，
/// macOS 真机会写 OS keychain；明文/加密/keyring 各态由 store 侧
/// credential_forms 测试 + 下方纯函数测试覆盖）。
#[tokio::test]
async fn doctor_security_section_reports_risks() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 两个会话（无 workdir 覆盖 → 有效 workdir 均为 default）。
    for conv in ["c1", "c2"] {
        ctx.disp
            .store
            .upsert_session(&SessionRow {
                first_prompt: None,
                conv_id: conv.into(),
                session_id: "s".into(),
                agent_kind: "mock-backend".into(),
                workdir: "/tmp/imagent-test-ws".into(),
                name: None,
                created_at: 1,
                updated_at: 1,
                task_todos: None,
            })
            .await
            .unwrap();
    }
    // webhook 摘要：公网监听 + 条目无 secret（启动期应拒——出现即漂移，模拟注入）。
    ctx.disp.set_webhook_exposure(crate::WebhookExposure {
        listening: true,
        loopback: false,
        entry_secrets: vec![false],
        any_replay_window: false,
    });
    ctx.disp.handle(msg("c1", "alice", "/doctor")).await;
    let inbox = ctx.inbox.lock().await.clone();
    let doctor = inbox
        .iter()
        .rev()
        .find(|t| t.contains("🛡️ 安全"))
        .expect("应有安全分组: {inbox:?}");
    // ① 凭据：空表 → ✅（查询接线证明；各形态文案见纯函数测试）。
    assert!(
        doctor.contains("✅ 凭据：无落库凭据"),
        "空表应为无凭据行: {doctor}"
    );
    // ② webhook：非 loopback × 无 secret = 配置漂移兜底。
    assert!(
        doctor.contains("webhook 非 loopback 且有条目未配 secret"),
        "应告警 webhook 漂移: {doctor}"
    );
    // ③ 共享工作区：两会话共享 default workdir。
    assert!(
        doctor.contains("2 个会话共享工作区 /tmp/imagent-test-ws"),
        "应告警共享工作区: {doctor}"
    );
    // ④ 权限 × 能力：build() 的 tools = [Read, Edit] × Mock 不支持逐工具白名单。
    assert!(
        doctor.contains("不按逐工具白名单生效"),
        "应告警能力错配: {doctor}"
    );
    // ⑤ 护栏：test_budgets 默认 max_concurrent_rounds=0 + 自动压缩关闭。
    assert!(
        doctor.contains("全局并发护栏未设上限"),
        "应告警护栏无上限: {doctor}"
    );
    assert!(
        doctor.contains("自动压缩未启用（v1.27 起默认关闭"),
        "应有自动压缩信息行: {doctor}"
    );
    // ⑥ 体积信息行。
    assert!(doctor.contains("ℹ️ 体积：DB"), "应有体积信息行: {doctor}");
    drop_db(ctx.db).await;
}

/// 检查 ① 纯函数：凭据形态四态文案。
#[test]
fn doctor_credential_line_states() {
    use super::commands::doctor_credential_line;
    let f = |keyring, encrypted, plaintext, passphrase_set| imagent_store::CredentialForms {
        keyring,
        encrypted,
        plaintext,
        passphrase_set,
    };
    // 空表。
    assert!(doctor_credential_line(&f(0, 0, 0, false)).contains("无落库凭据"));
    // 明文 + 无 passphrase = ❌ + 补救指引。
    let s = doctor_credential_line(&f(0, 0, 2, false));
    assert!(
        s.starts_with("❌") && s.contains("2 条") && s.contains("IMAGENT_PASSPHRASE"),
        "{s}"
    );
    // 明文 + 已设 passphrase = ⚠️（惰性迁移提示）。
    let s = doctor_credential_line(&f(0, 0, 1, true));
    assert!(s.starts_with("⚠️") && s.contains("惰性迁移"), "{s}");
    // keyring / 加密形态 = ✅（计数可见）。
    let s = doctor_credential_line(&f(1, 2, 0, false));
    assert!(
        s.starts_with("✅") && s.contains("keyring 1") && s.contains("加密 2"),
        "{s}"
    );
}

/// 检查 ② 纯函数：webhook 暴露面四态（loopback 优先于 secret 缺配）。
#[test]
fn doctor_webhook_line_states() {
    use super::commands::doctor_webhook_line;
    let mk = |listening, loopback, secrets: &[bool], replay| crate::WebhookExposure {
        listening,
        loopback,
        entry_secrets: secrets.to_vec(),
        any_replay_window: replay,
    };
    assert!(doctor_webhook_line(&mk(false, false, &[], false)).contains("未启用"));
    // loopback 且有条目缺 secret 仍 ✅（loopback 免 secret，口径同启动校验）。
    assert!(doctor_webhook_line(&mk(true, true, &[false], false)).contains("loopback"));
    // 非 loopback × 有条目缺 secret = ❌ 配置漂移。
    let s = doctor_webhook_line(&mk(true, false, &[true, false], false));
    assert!(s.starts_with("❌") && s.contains("配置漂移"), "{s}");
    // 非 loopback × 全 secret = ⚠️ 重放防护（未配 replay_window 时给建议）。
    let s = doctor_webhook_line(&mk(true, false, &[true], false));
    assert!(
        s.starts_with("⚠️") && s.contains("去重 LRU 恒开") && s.contains("replay_window_secs"),
        "{s}"
    );
    // 已启用时间戳协议 → 不再给配置建议。
    assert!(doctor_webhook_line(&mk(true, false, &[true], true)).contains("时间戳协议已启用"));
}

/// 检查 ③ 纯函数：共享工作区分组（覆盖键共享 / default 隐式共享 / 尾部斜杠
/// 归一化 / 无共享）。
#[test]
fn doctor_shared_workdir_lines_grouping() {
    use super::commands::doctor_shared_workdir_lines;
    let default = std::path::Path::new("/tmp/imagent-test-ws");
    // 覆盖键共享（"/tmp/shared-a" 与 "/tmp/shared-a/" 归一为同一路径）。
    let overrides = vec![
        ("workdir:c1".to_string(), "/tmp/shared-a".to_string()),
        ("workdir:c2".to_string(), "/tmp/shared-a/".to_string()),
    ];
    let lines = doctor_shared_workdir_lines(&overrides, &[], default);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("2 个会话共享工作区 /tmp/shared-a"),
        "{}",
        lines[0]
    );
    // 无覆盖的多会话 → 隐式共享 default_workdir（SECURITY.md「多用户共享
    // default_workdir」风险入口）。
    let lines = doctor_shared_workdir_lines(
        &[],
        &["c1".to_string(), "c2".to_string(), "c3".to_string()],
        default,
    );
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("3 个会话共享工作区 /tmp/imagent-test-ws"),
        "{}",
        lines[0]
    );
    // 会话各有独立覆盖 → ✅ 无共享。
    let overrides = vec![
        ("workdir:c1".to_string(), "/tmp/w1".to_string()),
        ("workdir:c2".to_string(), "/tmp/w2".to_string()),
    ];
    let lines =
        doctor_shared_workdir_lines(&overrides, &["c1".to_string(), "c2".to_string()], default);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("✅"), "{}", lines[0]);
}

/// 检查 ④ 纯函数：权限 × 能力矩阵（复用 T4 文案）。
#[test]
fn doctor_capability_lines_matrix() {
    use super::commands::doctor_capability_lines;
    use crate::PermissionCapability as PC;
    // ① 工具白名单非全量 × 不支持逐工具白名单 → T4 P1-3 文案。
    let lines = doctor_capability_lines(
        &["Read".to_string()],
        PermissionMode::Off,
        "claude-acp",
        false,
        PC::FullLoop,
    );
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("不按逐工具白名单生效"), "{}", lines[0]);
    // ② allow 档 × 非 FullLoop → T4 P3-3 文案。
    let lines = doctor_capability_lines(
        &["*".to_string()],
        PermissionMode::Allow,
        "codex",
        false,
        PC::Unsupported,
    );
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("无执行点"), "{}", lines[0]);
    // 全匹配 → ✅。
    let lines = doctor_capability_lines(
        &["Read".to_string()],
        PermissionMode::Ask,
        "claude-cli",
        true,
        PC::FullLoop,
    );
    assert!(lines[0].starts_with("✅"), "{}", lines[0]);
}

/// 检查 ⑤⑥ 纯函数：护栏水位两档 + 体积行格式。
#[test]
fn doctor_guardrail_and_size_lines() {
    use super::commands::{doctor_guardrail_lines, doctor_size_line};
    // 0/0：护栏 ⚠️ + 自动压缩 ℹ️（v1.27 默认关，非风险项）。
    let lines = doctor_guardrail_lines(0, 0);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].starts_with("⚠️") && lines[0].contains("max_concurrent_rounds"),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].starts_with("ℹ️") && lines[1].contains("v1.27"),
        "{}",
        lines[1]
    );
    // 设值：✅ ×2。
    let lines = doctor_guardrail_lines(4, 120_000);
    assert!(
        lines[0].contains("4 轮上限") && lines[1].contains("120000 tok"),
        "{lines:?}"
    );
    // 体积行格式（B 与 MB 档）。
    assert_eq!(
        doctor_size_line(512, 3 * 1024 * 1024),
        "ℹ️ 体积：DB 512 B · 媒体 3.0 MB"
    );
}

// ---------- T11（v13 #4）：/tasks 轮次进度面板 ----------

/// /tasks：在飞轮次 → checklist 进度（▓ 进度条 + 逐项 ✅/⏳/◌）+ 工具统计；
/// 无在飞 → 提示文案（对齐 /stop 口径）。MockBackend 在完成闸门前发
/// TodoList + ToolUse chunk，制造「在飞且已有内部进度」的窗口。
#[tokio::test]
async fn tasks_command_shows_progress_and_tool_stats() {
    let _serial = SERIAL.lock().await;
    let auth = Auth::new(vec!["alice".into()]);
    let (plat, _pi, _pc) = MockPlatform::new();
    let (back, calls, _prompts, _order, gate) = MockBackend::new_gated_with_progress(
        vec![
            crate::types::TodoItem {
                id: None,
                text: "复现问题".into(),
                status: crate::types::TodoStatus::Completed,
            },
            crate::types::TodoItem {
                id: None,
                text: "修复代码".into(),
                status: crate::types::TodoStatus::InProgress,
            },
            crate::types::TodoItem {
                id: None,
                text: "回归测试".into(),
                status: crate::types::TodoStatus::Pending,
            },
        ],
        vec![
            ("Bash".into(), r#"{"command":"git status"}"#.into()),
            ("Read".into(), r#"{"file_path":"src/main.rs"}"#.into()),
        ],
    );
    let mut ctx =
        build_with_parts_full(auth, plat, back, test_budgets(), PermissionMode::Off).await;
    ctx.calls = calls.clone();
    // 起一轮（在飞），等 TodoList chunk 被消费进快照（todos 非空 = 面板数据就绪）。
    let d = ctx.disp.clone();
    let h = tokio::spawn(async move { d.handle(msg("c1", "alice", "修一个 bug")).await });
    assert!(
        wait_until(&ctx, |c| {
            Box::pin(async move {
                // T18 机械调整：running 并入 ConvState 单表。
                c.disp
                    .conv_states
                    .lock()
                    .await
                    .get("c1")
                    .and_then(|cs| cs.running.as_ref())
                    .is_some_and(|rh| !rh.snapshot.lock().unwrap().todos.is_empty())
            })
        })
        .await,
        "在飞轮次应已消费 TodoList chunk"
    );
    // /tasks：checklist 进度 + 逐项状态 + 工具统计（2 次调用，最近 = Read）。
    ctx.disp.handle(msg("c1", "alice", "/tasks")).await;
    let inbox = ctx.inbox.lock().await.clone();
    let panel = inbox
        .iter()
        .rev()
        .find(|t| t.contains("任务面板"))
        .expect("/tasks 应回进度面板");
    assert!(panel.contains("📋 计划"), "应含计划行: {panel}");
    assert!(panel.contains("▓"), "应含 ▓ 进度条: {panel}");
    assert!(panel.contains("1/3"), "应含完成计数 1/3: {panel}");
    assert!(panel.contains("✅ 复现问题"), "完成项图标+文本: {panel}");
    assert!(panel.contains("⏳ 修复代码"), "进行项图标+文本: {panel}");
    assert!(panel.contains("◌ 回归测试"), "待办项图标+文本: {panel}");
    assert!(
        panel.contains("🔧 工具 2 次，最近：Read — src/main.rs"),
        "工具统计应含次数与最近调用: {panel}"
    );
    // 放行 → 轮次收尾 → 在飞注册清除。
    gate.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
    assert!(
        wait_until(&ctx, |c| {
            Box::pin(async move { running_rounds(&c.disp).await == 0 })
        })
        .await,
        "轮次应已收尾"
    );
    // 无在飞轮次 → 无任务提示。
    ctx.disp.handle(msg("c1", "alice", "/tasks")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("当前没有运行中的任务")),
        "无在飞轮次应回提示: {inbox:?}"
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
    // 恢复 2 号（sess-0，较老那条）→ 下条消息续接 sess-0。列表排序
    // 「updated_at DESC, rowid DESC」：行 1 恒为最新（sess-1，当前）、行 2
    // 恒为 sess-0——不依赖两轮是否跨秒（同秒并列按插入序取新）。
    ctx.disp.handle(msg("c1", "alice", "/resume 2")).await;
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
    // 列表序：sess-0（新）在前，pc-9f86d081 第 2（表格行 | # | 来源 | 时间 | 内容 |）。
    let l1 = list.lines().find(|l| l.starts_with("| 1 |")).unwrap();
    let l2 = list.lines().find(|l| l.starts_with("| 2 |")).unwrap();
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
        !ctx.disp
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

/// B3（fail-closed）：闭环类档位（ask / auto-claude）× 非 FullLoop 后端，
/// Dispatcher::run() 启动即拒绝（MockBackend 用 trait 默认 Unsupported）。
/// 此前 codex/gemini 在 ask 档下静默忽略审批（等于全放行）——现须启动报错。
#[tokio::test]
async fn run_fails_closed_ask_mode_with_non_fullloop_backend() {
    let _serial = SERIAL.lock().await;
    for mode in [PermissionMode::Ask, PermissionMode::AutoClaude] {
        let ctx = build_with_mode(Auth::new(vec!["alice".into()]), mode).await;
        let run = ctx.disp.clone().run();
        let res = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("run 应立即返回（fail-closed），不进主循环");
        let err = res.expect_err("ask/auto-claude × 非 FullLoop 后端应启动失败");
        assert!(
            err.to_string().contains("IM 审批闭环"),
            "错误信息应指向闭环能力缺失: {err}"
        );
        drop_db(ctx.db).await;
    }
}

/// B3：非闭环档位（off/allow/deny）× 非 FullLoop 后端不拦截——run() 正常进入
/// 主循环（收到 shutdown 后优雅退出 Ok）。
#[tokio::test]
async fn run_allows_non_socket_mode_with_non_fullloop_backend() {
    let _serial = SERIAL.lock().await;
    let ctx = build_with_mode(Auth::new(vec!["alice".into()]), PermissionMode::Deny).await;
    let disp = ctx.disp.clone();
    let run = disp.clone().run();
    // 给 run 一点时间进入主循环，再触发优雅退出。
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        disp.shutdown();
    });
    let res = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("run 应在 shutdown 后退出");
    assert!(res.is_ok(), "非闭环档位不应被启动校验拦截: {res:?}");
    drop_db(ctx.db).await;
}

/// B3：/perm ask（闭环档）× 非 FullLoop 后端（MockBackend 默认 Unsupported）——
/// 管理员也拒绝热切，模式不变；/perm auto 按后端解析为非闭环档（mock → off），
/// 可正常切换。
#[tokio::test]
async fn perm_ask_rejected_for_non_fullloop_backend() {
    let _serial = SERIAL.lock().await;
    let ctx = build_with_admin(Auth::new(vec!["alice".into()]), vec!["alice".into()]).await;
    // 管理员切 ask → 拒绝且模式不变。
    ctx.disp.handle(msg("c1", "alice", "/perm ask")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("不支持 IM 审批闭环")),
        "应拒绝闭环档热切并说明能力: {inbox:?}"
    );
    drop(inbox);
    assert!(
        !matches!(*ctx.disp.permission_mode.read(), PermissionMode::Ask),
        "模式不得被切换到 ask"
    );
    // /perm auto：mock 后端解析为非闭环档，可切换成功。
    ctx.disp.handle(msg("c1", "alice", "/perm auto")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("✅ 权限模式")),
        "auto 解析为非闭环档应可热切: {inbox:?}"
    );
    drop(inbox);
    drop_db(ctx.db).await;
}

/// T4（P1-3/P3-3）：能力面告警判定的纯函数矩阵——「是否应告警」的单测锚点
///（仓内无 tracing 捕获先例，判定抽成纯函数；文案与三点位接线由下方
/// 集成测试覆盖）。
#[test]
fn capability_divergence_predicates_matrix() {
    use crate::backend::{perm_mode_lacks_execution, tool_allowlist_diverges};
    // P1-3：allowed_tools 非全量 × 不支持逐工具白名单 → 告警；全量（空/["*"]）
    // 或后端支持（claude-cli 的 --allowedTools）→ 不告警。
    assert!(tool_allowlist_diverges(&["Read".to_string()], false));
    assert!(
        !tool_allowlist_diverges(&["Read".to_string()], true),
        "CLI 后端 × 非全量 allowlist → 无 warn"
    );
    assert!(!tool_allowlist_diverges(&[], false));
    assert!(!tool_allowlist_diverges(&["*".to_string()], false));
    assert!(!tool_allowlist_diverges(&["*".to_string()], true));
    // P3-3：allow/deny × 非 FullLoop → 告警；FullLoop（deny 经审批回调执行）/
    // ask（由启动 fail-closed 拒绝，不进告警面）/ off（无 IM 侧决策）→ 不告警。
    use crate::backend::PermissionCapability::{FullLoop, NativeOnly, Unsupported};
    assert!(perm_mode_lacks_execution(PermissionMode::Deny, NativeOnly));
    assert!(perm_mode_lacks_execution(
        PermissionMode::Allow,
        Unsupported
    ));
    assert!(!perm_mode_lacks_execution(PermissionMode::Deny, FullLoop));
    assert!(
        !perm_mode_lacks_execution(PermissionMode::Ask, NativeOnly),
        "ask 档由 fail-closed 启动拒绝管，不进告警面"
    );
    assert!(!perm_mode_lacks_execution(PermissionMode::Off, Unsupported));
}

/// T4（P1-3）：配置非全量 allowlist × 不支持逐工具白名单的后端（模拟
/// claude-acp 能力位）→ 启动点位（run() 启动调用同一
/// capability_surface_warnings）产生告警；同状态去重只一次，配置变更
///（SIGHUP reload_tools）后再进入告警态重新告警。
#[tokio::test]
async fn capability_warns_allowlist_divergence_and_dedups() {
    let _serial = SERIAL.lock().await;
    let ctx = build_capability(
        Auth::new(vec!["alice".into()]),
        vec!["Read".into(), "Edit".into()],
        PermissionMode::Off,
        crate::backend::PermissionCapability::FullLoop,
        false, // 模拟 claude-acp：不支持逐工具白名单（P1-3 裂缝侧）。
    )
    .await;
    // 首评：恰一条告警，说明清单不生效 + 后端机制 + SECURITY.md 指引。
    let notices = ctx.disp.capability_surface_warnings();
    assert_eq!(
        notices.len(),
        1,
        "非全量 allowlist × 不支持 → 恰一条告警: {notices:?}"
    );
    assert!(
        notices[0].contains("allowed_tools")
            && notices[0].contains("不支持")
            && notices[0].contains("SECURITY.md"),
        "告警应说明白名单不生效并指向文档: {notices:?}"
    );
    // 同状态重评（模拟无变化 SIGHUP）：去重，不再告警。
    assert!(
        ctx.disp.capability_surface_warnings().is_empty(),
        "同状态不重复告警"
    );
    // 配置变更：全量 → 解除；再切回非全量 → 重新告警。
    assert!(
        ctx.disp.reload_tools(vec!["*".into()]).is_empty(),
        "全量语义不应告警"
    );
    let re = ctx.disp.reload_tools(vec!["Read".into()]);
    assert_eq!(re.len(), 1, "配置变更后再进入告警态应重新告警: {re:?}");
    drop_db(ctx.db).await;
}

/// T4（P1-3）：后端支持逐工具白名单（模拟 claude-cli 能力位 = true）× 非全量
/// allowlist → 两维均无告警（配置按清单生效，不应打扰）。
#[tokio::test]
async fn no_capability_warn_when_backend_supports_allowlist() {
    let _serial = SERIAL.lock().await;
    let ctx = build_capability(
        Auth::new(vec!["alice".into()]),
        vec!["Read".into(), "Edit".into()],
        PermissionMode::Deny,
        crate::backend::PermissionCapability::FullLoop,
        true, // 模拟 claude-cli：--allowedTools 透传。
    )
    .await;
    assert!(
        ctx.disp.capability_surface_warnings().is_empty(),
        "CLI 后端 × 非全量 allowlist（FullLoop）→ 无 warn"
    );
    drop_db(ctx.db).await;
}

/// T4（P3-3）：permission_mode = allow/deny × 非 FullLoop（NativeOnly /
/// Unsupported 均无审批回调）→ 告警「无执行点」；FullLoop / off → 无告警。
#[tokio::test]
async fn capability_warns_dead_perm_mode_on_non_fullloop() {
    let _serial = SERIAL.lock().await;
    use crate::backend::PermissionCapability::{FullLoop, NativeOnly, Unsupported};
    for cap in [NativeOnly, Unsupported] {
        let ctx = build_capability(
            Auth::new(vec!["alice".into()]),
            vec!["*".into()], // 全量工具：隔离维度二。
            PermissionMode::Deny,
            cap,
            false,
        )
        .await;
        let notices = ctx.disp.capability_surface_warnings();
        assert_eq!(notices.len(), 1, "deny × {cap:?} → 恰一条: {notices:?}");
        assert!(
            notices[0].contains("无执行点") && notices[0].contains("allowed_tools"),
            "告警应说明档位无执行点与实际边界: {notices:?}"
        );
        drop_db(ctx.db).await;
    }
    // 反例一：FullLoop × deny（deny 经审批回调固定答复执行）→ 无告警。
    let ctx = build_capability(
        Auth::new(vec!["alice".into()]),
        vec!["*".into()],
        PermissionMode::Deny,
        FullLoop,
        false,
    )
    .await;
    assert!(ctx.disp.capability_surface_warnings().is_empty());
    drop_db(ctx.db).await;
    // 反例二：off × 非 FullLoop（off 本就无 IM 侧决策）→ 无告警。
    let ctx = build_capability(
        Auth::new(vec!["alice".into()]),
        vec!["*".into()],
        PermissionMode::Off,
        Unsupported,
        false,
    )
    .await;
    assert!(ctx.disp.capability_surface_warnings().is_empty());
    drop_db(ctx.db).await;
}

/// T4（P3-3）：/perm 切 deny × NativeOnly 后端 → 回执直接带「无执行点」提示
///（用户能立刻看到，不用翻日志）；切 off → 回执不带提示。
#[tokio::test]
async fn perm_deny_receipt_carries_dead_mode_notice() {
    let _serial = SERIAL.lock().await;
    let ctx = build_capability(
        Auth::new(vec!["alice".into()]),
        vec!["*".into()],
        PermissionMode::Off,
        crate::backend::PermissionCapability::NativeOnly,
        false,
    )
    .await;
    ctx.disp.handle(msg("c1", "alice", "/perm deny")).await;
    assert!(
        matches!(*ctx.disp.permission_mode.read(), PermissionMode::Deny),
        "deny × 非 FullLoop 应热切成功（fail-closed 只管闭环档）"
    );
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("✅ 权限模式已切到 deny") && t.contains("无执行点")),
        "回执应带「无执行点」提示: {inbox:?}"
    );
    drop(inbox);
    // off：本就无 IM 侧决策，回执不带提示。
    ctx.disp.handle(msg("c1", "alice", "/perm off")).await;
    let inbox = ctx.inbox.lock().await.clone();
    let off_receipt = inbox
        .iter()
        .find(|t| t.contains("已切到 off"))
        .expect("应有 off 回执");
    assert!(
        !off_receipt.contains("无执行点"),
        "off 回执不应带提示: {off_receipt}"
    );
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
    assert!(!conv_running(&ctx.disp, "c1").await, "在飞注册应清空");
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
/// P2-11：推送粒度改为 400ms 合帧——两段 delta 合并为一条流式消息送达
///（语义等价：用户仍在流式阶段看到全部中间文本，且只看到一次）。
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
    // 两段流式文本都应实时推送（合帧后同一条消息内先后可见）。
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("答案第一段。") && t.contains("答案第二段。")),
        "应实时推送流式文本（合帧）: {inbox:?}"
    );
    // 全量文本不应作为最终回复再发一遍。
    let dup = inbox.iter().filter(|t| t.contains("答案第一段。")).count();
    assert_eq!(dup, 1, "Final 全量不应重发: {inbox:?}");
    drop_db(ctx.db).await;
}

/// P1-1 吞吐锚（集成）：卡片平台 × delta 级 Text 流（50 条，模拟 claude-acp
/// 粒度），整轮耗时应远低于「每 chunk 睡 500ms」的旧节奏（50×500ms = 25s，
/// 可拖到逼近 agent_timeout）——节流睡眠已移出消费路径（CardSession 常驻
/// patcher）；终态卡仍携带全部累积文本（数据不因解耦丢失）。
#[tokio::test]
async fn card_round_consumes_delta_stream_fast() {
    let _serial = SERIAL.lock().await;
    let texts: Vec<String> = (0..50).map(|i| format!("片段{i}，")).collect();
    let (ctx, updates) = build_card_streaming(Auth::new(vec!["alice".into()]), texts).await;
    let t0 = std::time::Instant::now();
    ctx.disp.handle(msg("c1", "alice", "长任务")).await;
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "delta 流整轮耗时应 < 2s（节流不得阻塞消费循环）: {elapsed:?}"
    );
    let ups = updates.lock().await.clone();
    assert!(
        ups.iter()
            .any(|t| t.contains("片段0，") && t.contains("片段49，")),
        "终态卡应含全部累积文本: {ups:?}"
    );
    drop_db(ctx.db).await;
}

/// P2-11：文本平台（无卡）delta 合帧——50 条 delta 只产生少量消息（≤5），
/// 且每段内容恰好送达一次（合帧只改发送粒度，不改「用户看到完整文本」）。
#[tokio::test]
async fn text_platform_coalesces_delta_stream() {
    let _serial = SERIAL.lock().await;
    let texts: Vec<String> = (0..50).map(|i| format!("段{i}；")).collect();
    let ctx = build_streaming(Auth::new(vec!["alice".into()]), texts).await;
    feed_and_wait(&ctx, vec![msg("c1", "alice", "问题")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    let streamed: Vec<&String> = inbox.iter().filter(|t| t.contains("段0；")).collect();
    assert!(!streamed.is_empty(), "流式文本应实时送达: {inbox:?}");
    assert!(
        streamed.len() <= 5,
        "50 条 delta 应合帧为 ≤5 条消息（防刷屏/打爆 QPS）: {inbox:?}"
    );
    // 完整性：50 段每段恰好出现一次（无丢失、无重复；Final 差量为空不再补发）。
    for i in 0..50 {
        let needle = format!("段{i}；");
        let n: usize = inbox.iter().map(|t| t.matches(&needle).count()).sum();
        assert_eq!(n, 1, "第 {i} 段应恰好送达一次: {inbox:?}");
    }
    drop_db(ctx.db).await;
}

/// P2-11：中断（/stop）退出路径的合帧缓冲不丢——缓冲中的未发文本在终态
/// 回复前 flush。abort 后 sender drop，消费方仍会先排空 channel 里缓冲的
/// chunk（tokio mpsc 语义）再退出循环，随后的统一收口 flush 把它们送达。
#[tokio::test]
async fn abort_flushes_coalesced_text_buffer() {
    let _serial = SERIAL.lock().await;
    let ctx = build_stream_then_hang(
        Auth::new(vec!["alice".into()]),
        vec!["中途的".to_string(), "部分输出".to_string()],
    )
    .await;
    let disp = ctx.disp.clone();
    let runner = tokio::spawn(async move {
        disp.handle(msg("c1", "alice", "长任务")).await;
    });
    assert!(wait_registered(&ctx, "c1").await, "任务应在飞");
    // 等到 backend run 已实际启动（calls 记录）再补少量通道发送余量后 /stop——
    // 固定 sleep 在 CI 慢机上不够（v1.28 首发翻车：调度停顿使 /stop 抢在
    // delta 产出之前，缓冲为空无可 flush）。run() 一旦被调度，两段 Text 的
    // 通道 send 与挂起之间无其它 await，100ms 余量足够；等待本身不限时长
    // （wait_registered 只保证注册，不保证 run 已跑）。
    for _ in 0..2000 {
        if !ctx.calls.lock().await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !ctx.calls.lock().await.is_empty(),
        "backend run 应已启动（否则测试前提不成立）"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    ctx.disp.handle(msg("c1", "alice", "/stop")).await;
    let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
    assert!(done.is_ok(), "被中断的 runner 应很快退出");
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("中途的部分输出")),
        "中断前缓冲的文本必须 flush（不能丢）: {inbox:?}"
    );
    assert!(
        inbox.iter().any(|t| t.contains("本轮已被中断")),
        "文本平台应补中断标记: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

/// P3（v13 批）：/stop 拦截（round.rs 起跑前二次复查命中）时本批消息已打
/// 👀（Processing）——early return 必须把表情翻回终态（Failed），否则用户
/// 消息永远挂着「在做了」。typing 闸门把 preamble 钉在「水位已读、👀 已打」
/// 之后的窗口，测试在窗口内注入停止标记，精确命中该分支。
#[tokio::test]
async fn stop_interception_flips_processing_reaction() {
    let _serial = SERIAL.lock().await;
    let auth = Auth::new(vec!["alice".into()]);
    let (gate, release) = TypingGate::new();
    let (plat, _inbox, _send_count) = MockPlatform::new();
    let reactions = plat.reactions.clone();
    let plat = MockPlatform {
        typing_gate: Some(gate.clone()),
        ..plat
    };
    let (back, _calls, _prompts, _order) = MockBackend::new();
    let (store, db) = tmp_store().await;
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
    let d = disp.clone();
    let runner = tokio::spawn(async move {
        d.run_agent_round(msg("c1", "alice", "将被拦截的批次"), vec!["om_9".into()])
            .await;
    });
    // 等 preamble 走到 typing（stop_mark_epoch 已读、👀 未打）。
    for _ in 0..400 {
        if gate.entered.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        gate.entered.load(Ordering::SeqCst),
        "preamble 应走到 typing 闸门"
    );
    // 窗口内注入停止标记（ts > 轮首水位 0）→ 👀 已打后的二次复查命中拦截。
    // （T18 机械调整：stop_requested 并入 ConvState 单表。）
    disp.with_conv("c1", |cs| {
        cs.stop_requested = Some(crate::dispatch::now_secs())
    })
    .await;
    release.send(()).expect("release typing gate");
    let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
    assert!(done.is_ok(), "拦截路径应结束");
    let reactions = reactions.lock().await.clone();
    let idx_processing = reactions
        .iter()
        .position(|(id, r)| id == "om_9" && *r == crate::types::MsgReaction::Processing);
    let idx_failed = reactions
        .iter()
        .position(|(id, r)| id == "om_9" && *r == crate::types::MsgReaction::Failed);
    assert!(idx_processing.is_some(), "拦截前应打 👀: {reactions:?}");
    assert!(
        idx_failed.is_some(),
        "拦截 early-return 应把 👀 翻回终态: {reactions:?}"
    );
    assert!(
        idx_failed.unwrap() > idx_processing.unwrap(),
        "Failed 应在 Processing 之后: {reactions:?}"
    );
    drop_db(db).await;
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
    assert_eq!(running_rounds(&ctx.disp).await, 0, "无在飞任务");
    drop_db(ctx.db).await;
}

/// P5-9b：权限 socket 握手 token——错 token 连接被丢弃（无询问无回复）；
/// 正确 token 的请求触发 IM 询问，cancel 立即唤醒回 deny（P5-16）。
#[cfg(unix)]
#[tokio::test]
async fn permission_socket_token_handshake() {
    let _serial = SERIAL.lock().await;
    let ctx = build_with_mode(Auth::new(vec!["alice".into()]), PermissionMode::Ask).await;
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

    // D12：幂等——再次 spawn（模拟 /perm ask 热切复用同一路径）返回 true 且
    // 不重复建 accept task（第二个 socket 路径不会被 bind，token 不变）。
    let sock2 = dir.join("permission2.sock");
    assert!(
        ctx.disp
            .spawn_socket_accept(sock2.to_string_lossy().into_owned()),
        "重复 spawn 应幂等返回 true"
    );
    assert!(!sock2.exists(), "幂等：不应 bind 第二个 socket 文件");
    let token_again = std::fs::read_to_string(&token_path)
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(token_again, token, "幂等：token 不应被重写");

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

/// T12（v13 产品批 #3）：Mock BitableApi——记录收到的 (op, fields) 供 socket
/// 路由测试断言。
type BitableCalls =
    Arc<TokioMutex<Vec<(String, Option<serde_json::Map<String, serde_json::Value>>)>>>;
struct MockBitable {
    calls: BitableCalls,
}

#[async_trait]
impl crate::bitable::BitableApi for MockBitable {
    async fn list_fields(&self) -> Result<Vec<crate::bitable::BitableField>> {
        self.calls.lock().await.push(("list_fields".into(), None));
        Ok(vec![
            crate::bitable::BitableField {
                name: "任务".into(),
                field_type: "Text".into(),
            },
            crate::bitable::BitableField {
                name: "完成时间".into(),
                field_type: "DateTime".into(),
            },
        ])
    }
    async fn append_row(
        &self,
        fields: serde_json::Map<String, serde_json::Value>,
    ) -> Result<String> {
        self.calls
            .lock()
            .await
            .push(("append_row".into(), Some(fields)));
        Ok("rec_mock_1".into())
    }
}

/// T12：`kind=bitable` socket 路由——op/fields 透传到注入的 BitableApi、
/// 回包 ok/data 形态；未注入（None）回「未配置 feishu_bitable_*」错误文案；
/// 参数残缺（fields 缺失/空）与未知 op 在主进程侧拦下。
#[cfg(unix)]
#[tokio::test]
async fn bitable_socket_routes_and_reports_unconfigured() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let _serial = SERIAL.lock().await;
    let ctx = build_with_mode(Auth::new(vec!["alice".into()]), PermissionMode::Off).await;
    let dir = std::env::temp_dir().join(format!("imagent-sock-bitable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("permission.sock");
    ctx.disp
        .spawn_socket_accept(sock.to_string_lossy().into_owned());
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

    // 辅助：发一行 kind=bitable 请求，读一行回复。
    async fn roundtrip(sock: &std::path::Path, token: &str, body: &str) -> String {
        let mut s = tokio::net::UnixStream::connect(sock).await.unwrap();
        s.write_all(format!("{token}\n").as_bytes()).await.unwrap();
        s.write_all(format!("{body}\n").as_bytes()).await.unwrap();
        s.flush().await.unwrap();
        let mut buf = String::new();
        let mut r = tokio::io::BufReader::new(s);
        let _ = tokio::time::timeout(Duration::from_secs(5), r.read_line(&mut buf)).await;
        buf
    }

    // ① 注入 mock → append_row 收到 op+fields，回包带 record_id。
    let calls: BitableCalls = Arc::new(TokioMutex::new(Vec::new()));
    ctx.disp.set_bitable(Some(Arc::new(MockBitable {
        calls: calls.clone(),
    })));
    let reply = roundtrip(
        &sock,
        &token,
        r#"{"kind":"bitable","conv_id":"c1","op":"append_row","fields":{"任务":"巡检","状态":"通过"}}"#,
    )
    .await;
    assert!(
        reply.contains("\"ok\":true") && reply.contains("rec_mock_1"),
        "append_row 应回 ok+record_id: {reply}"
    );
    let got = calls.lock().await.clone();
    assert_eq!(got.len(), 1, "mock 应收到一次调用");
    assert_eq!(got[0].0, "append_row");
    let fields = got[0].1.as_ref().expect("append_row 应带 fields");
    assert_eq!(fields.get("任务"), Some(&serde_json::json!("巡检")));
    assert_eq!(fields.get("状态"), Some(&serde_json::json!("通过")));

    // ② list_fields → 回字段数组（name/type 形态）。
    let reply = roundtrip(
        &sock,
        &token,
        r#"{"kind":"bitable","conv_id":"c1","op":"list_fields"}"#,
    )
    .await;
    assert!(reply.contains("\"ok\":true"), "{reply}");
    assert!(
        reply.contains("任务") && reply.contains("Text") && reply.contains("DateTime"),
        "字段摘要应含 name/type: {reply}"
    );
    assert_eq!(calls.lock().await.len(), 2, "list_fields 也应记一笔");

    // ③ 参数残缺：fields 缺失 / 空对象 → ok=false（不进 mock）。
    for bad in [
        r#"{"kind":"bitable","conv_id":"c1","op":"append_row"}"#,
        r#"{"kind":"bitable","conv_id":"c1","op":"append_row","fields":{}}"#,
        r#"{"kind":"bitable","conv_id":"c1","op":"bogus"}"#,
    ] {
        let before = calls.lock().await.len();
        let reply = roundtrip(&sock, &token, bad).await;
        assert!(
            reply.contains("\"ok\":false"),
            "残缺请求应回错: {bad} → {reply}"
        );
        assert_eq!(calls.lock().await.len(), before, "不应触达 mock: {bad}");
    }

    // ④ 未注入（None，配置不齐备）→ 明确的配置引导错误。
    ctx.disp.set_bitable(None);
    let reply = roundtrip(
        &sock,
        &token,
        r#"{"kind":"bitable","conv_id":"c1","op":"list_fields"}"#,
    )
    .await;
    assert!(reply.contains("\"ok\":false"), "{reply}");
    assert!(
        reply.contains("feishu_bitable_app_token") && reply.contains("feishu_bitable_table_id"),
        "未配置文案应指名配置键: {reply}"
    );

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
            first_prompt: None,
            conv_id: "c1".into(),
            session_id: "sess-9".into(),
            agent_kind: "mock-backend".into(),
            workdir: "/tmp/imagent-test-ws".into(),
            name: None,
            created_at: now,
            updated_at: now,
            task_todos: None,
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
    // P3（v13 批）：/stop 中断是正常语义，不再裸泄 JoinError（"task N was
    // cancelled"），改回可读文案（会话保留、可重新 /compact）。
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("已中断") && t.contains("可重新 /compact")),
        "应回中断提示（可重新 /compact）: {inbox:?}"
    );
    assert_eq!(running_rounds(&ctx.disp).await, 0, "在飞注册应清空");
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

// ---------- S 批：调度层安全/正确性 + 消息流 UX ----------

/// S-10：人读 Duration——秒/分钟/小时+分钟/天+小时四档。
#[test]
fn format_duration_human_shapes() {
    assert_eq!(format_duration_human(Duration::from_secs(45)), "45 秒");
    assert_eq!(format_duration_human(Duration::from_secs(180)), "3 分钟");
    assert_eq!(
        format_duration_human(Duration::from_secs(7_500)), // 2h5m
        "2 小时 5 分钟"
    );
    assert_eq!(
        format_duration_human(Duration::from_secs(27 * 3_600)), // 27h
        "1 天 3 小时"
    );
}

/// S-11：超 7 天不再回退裸 epoch，仍给人读相对时间。
#[test]
fn format_rel_ts_beyond_week_stays_human() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let out = format_rel_ts(now - 30 * 86_400);
    assert!(out.contains("30天前"), "超 7 天应仍为人读: {out}");
    assert!(!out.contains(&format!("{now}")), "不得回退裸时间戳: {out}");
}

/// S-6：latest_snippet 按注释口径截断前 40 字符。
#[test]
fn latest_snippet_truncates_to_40() {
    let m = msg("c1", "u1", &"x".repeat(100));
    let s = latest_snippet(&m);
    assert!(s.chars().count() <= 41, "40 字符 + 省略号: {s}"); // 40 + 「…」
    assert!(s.ends_with('…'));
    let short = latest_snippet(&msg("c1", "u1", "短消息"));
    assert_eq!(short, "短消息");
    let mut media_only = msg("c1", "u1", "");
    media_only.media.push(MediaRef {
        kind: "image".into(),
        url: "/tmp/a.png".into(),
    });
    assert_eq!(latest_snippet(&media_only), "（图片/文件）");
}

/// S-7：backend 失败文案模板——含可续接说明与建议动作，不含裸技术串占位。
#[test]
fn backend_failure_reply_template() {
    let m = backend_failure_reply("mock-backend");
    assert!(m.contains("mock-backend"), "应含后端名: {m}");
    assert!(m.contains("续接"), "应说明可续接: {m}");
    assert!(m.contains("/new"), "应给全新开始建议: {m}");
    assert!(m.contains("/doctor"), "应给自检建议: {m}");
    assert!(m.contains("日志"), "技术细节应指向日志: {m}");
}

/// S-12：未知命令模糊匹配（编辑距离 ≤2）与不匹配回退。
#[test]
fn suggest_command_fuzzy() {
    use super::commands::{suggest_command, unknown_command_reply, COMMAND_GROUPS};
    assert_eq!(suggest_command("/halp"), Some("/help"));
    assert_eq!(suggest_command("/sto"), Some("/stop"));
    assert_eq!(suggest_command("/rezume"), Some("/resume"));
    assert_eq!(suggest_command("/zzzzzz"), None, "距离 >2 不建议");
    let r = unknown_command_reply("/halp");
    assert!(r.contains("未知命令 /halp"), "{r}");
    assert!(r.contains("你是想找 /help 吗"), "{r}");
    // 分组竖排：每个分组头与其命令各占一行。
    for (group, cmds) in COMMAND_GROUPS {
        assert!(r.contains(group), "缺分组 {group}: {r}");
        for c in *cmds {
            assert!(r.contains(&format!("\n- {c}")), "缺命令行 {c}: {r}");
        }
    }
    // 完全未知：无建议句但仍有命令表。
    let r2 = unknown_command_reply("/zzzzzz");
    assert!(!r2.contains("你是想找"), "{r2}");
    assert!(r2.contains("🗂 会话"), "{r2}");
}

/// S-1：热切权限模式走与启动期同口径的能力校验——闭环档 × 非 FullLoop
/// 后端被拒且句柄不写（模式保持不变）。
#[tokio::test]
async fn reload_permission_mode_rejects_non_fullloop() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await; // MockBackend = Unsupported
    let res = ctx.disp.reload_permission_mode(PermissionMode::Ask);
    let err = res.expect_err("非 FullLoop 后端热切 ask 应被拒绝");
    assert!(err.to_string().contains("IM 审批闭环"), "{err}");
    assert!(
        !ctx.disp.permission_mode.read().needs_socket(),
        "拒绝时模式句柄不得被写入"
    );
    // 非闭环档不受影响。
    assert!(ctx
        .disp
        .reload_permission_mode(PermissionMode::Deny)
        .is_ok());
    drop_db(ctx.db).await;
}

/// S-2：FullLoop 后端 + 闭环档位，但 socket 路径无法绑定（被目录占位）——
/// run() fail-closed 拒绝启动，而非静默降级为「无审批」。
#[cfg(unix)]
#[tokio::test]
async fn run_fails_closed_when_socket_bind_fails() {
    let _serial = SERIAL.lock().await;
    // 隔离 IMAGENT_HOME（先设 env 再取路径），并在 socket 路径上放一个目录使 bind 必败。
    let home = std::env::temp_dir().join(format!("imagent_core_sockfail_{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var(crate::paths::IMAGENT_HOME_ENV, &home);
    let sock = crate::permission::default_sock_path().unwrap();
    let _ = std::fs::remove_file(&sock);
    // 非空目录占位 → bind Err。真机校准（2026-08）：空目录残留已被启动逻辑
    // 自愈（remove_dir），此处用「目录内有文件」构造不可自愈的失败形态。
    std::fs::create_dir_all(&sock).unwrap();
    std::fs::write(sock.join("keep"), b"x").unwrap();

    let (plat, _inbox, _send_count) = MockPlatform::new();
    let (mut back, _calls, _prompts, _order) = MockBackend::new();
    back.capability = crate::backend::PermissionCapability::FullLoop;
    let (store, db) = tmp_store().await;
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        Auth::new(vec!["alice".into()]),
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into()],
        PermissionMode::Ask,
        test_budgets(),
        CotDetail::Brief,
        vec!["alice".into()],
    ));
    let res = tokio::time::timeout(Duration::from_secs(5), disp.run())
        .await
        .expect("run 应立即返回（bind 失败 fail-closed）");
    let err = res.expect_err("socket bind 失败应拒绝启动");
    assert!(err.to_string().contains("拒绝启动"), "{err}");

    // 清理：恢复 env、撤掉占位目录与临时 home。
    std::env::remove_var(crate::paths::IMAGENT_HOME_ENV);
    let _ = std::fs::remove_dir(&sock);
    let _ = std::fs::remove_dir(&home);
    drop_db(db).await;
}

/// S-13：发现模式 + admin_senders 为空——引导不得提示 IM 内 /allow（S2 下
/// 无人可用管理命令，提示即误导）；仍保留 CLI 指引。
#[tokio::test]
async fn discovery_guide_without_admins_omits_allow_command() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec![])).await; // 发现模式；admins = snapshot = 空
    feed_and_wait(&ctx, vec![msg("c3", "anyone", "hi")], 0).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert_eq!(inbox.len(), 1, "应回一条引导: {inbox:?}");
    assert!(
        inbox[0].contains("imagent allow"),
        "保留 CLI 指引: {inbox:?}"
    );
    assert!(
        !inbox[0].contains("/allow"),
        "空 admin 下不得提示 /allow: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

/// S-16：/resume 选中序号不再消费缓存条目——连续选择序号不错位。
#[tokio::test]
async fn resume_numbering_stable_after_selection() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    feed_and_wait(
        &ctx,
        vec![msg("c1", "alice", "first"), msg("c1", "alice", "second")],
        2,
    )
    .await;
    // feed_and_wait 只等 backend 调用计数，第二轮的 session 落库在其之后
    // （慢 runner 上可能滞后）——轮询重发 /resume 直到列表出现两行，消除
    // 「列表仅 1 行 → /resume 2 无效」的竞态（单用户场景重发即刷新缓存）。
    for _ in 0..200 {
        ctx.disp.handle(msg("c1", "alice", "/resume")).await;
        let ok = ctx
            .inbox
            .lock()
            .await
            .last()
            .is_some_and(|t| t.contains("| 2 |"));
        if ok {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // 选中 1（sess-1，最新）后，2 仍指向 sess-0（而非移除后前移复用）——
    // 排序「updated_at DESC, rowid DESC」下行 1 恒为最新插入。
    ctx.disp.handle(msg("c1", "alice", "/resume 1")).await;
    ctx.disp.handle(msg("c1", "alice", "/resume 2")).await;
    feed_and_wait(&ctx, vec![msg("c1", "alice", "after")], 3).await;
    let calls = ctx.calls.lock().await.clone();
    assert_eq!(
        calls.last(),
        Some(&Some("sess-0".to_string())),
        "选中 2 应续接 sess-0（序号未被消费重排）: {calls:?}"
    );
    drop_db(ctx.db).await;
}

/// S-15：/switch 空参回用法 + 列出已有命名会话。
#[tokio::test]
async fn switch_without_name_lists_sessions() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 先跑一轮 + 命名，/sessions 才有实体条目可列。
    feed_and_wait(&ctx, vec![msg("c1", "alice", "hello")], 1).await;
    ctx.disp.handle(msg("c1", "alice", "/switch work")).await;
    // 命名 session 行在下一轮 run 落库——再跑一轮后 /sessions 才有实体条目。
    feed_and_wait(&ctx, vec![msg("c1", "alice", "in work")], 2).await;
    ctx.disp.handle(msg("c1", "alice", "/switch")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("用法") && t.contains("/switch <name>")),
        "应有用法提示: {inbox:?}"
    );
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("| 名称 | 时间 | 内容 |") && t.contains("work")),
        "应列出命名会话 work: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

/// S-17：/stop 打断后纯文本平台补「本轮已被中断」标记（半截流式文本不再
/// 无声终止）。
#[tokio::test]
async fn stop_on_text_platform_marks_interrupted() {
    let _serial = SERIAL.lock().await;
    let ctx = build_slow(
        Auth::new(vec!["alice".into()]),
        30_000,
        TaskBudgets {
            auto_compact_threshold_tokens: 0,
            auto_compact_window_tokens: 0,
            auto_compact_window_ratio: 0.8,
            cron_catchup: crate::config::CronCatchup::One,
            sender_daily_cost_limit_usd: None,
            agent_timeout: Duration::ZERO,
            permission_ask_timeout: Duration::from_secs(5),
            ask_via_im_timeout: Duration::from_secs(5),
            shutdown_grace: Duration::from_secs(5),
            agent_idle_timeout: Duration::ZERO,
            batch_window: Duration::ZERO,
            max_concurrent_rounds: 0,
        },
    )
    .await;
    let disp = ctx.disp.clone();
    let runner = tokio::spawn(async move {
        disp.handle(msg("c1", "alice", "long job")).await;
    });
    assert!(wait_registered(&ctx, "c1").await, "任务应在飞");
    ctx.disp.handle(msg("c1", "alice", "/stop")).await;
    let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
    assert!(done.is_ok(), "被中断的 runner 应很快退出");
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("本轮已被中断")),
        "文本平台应补中断标记: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

// ---------- Wave A：全角斜杠 / 私聊陌生人引导 / 撤回 / bot 移出群 ----------

/// 快赢：全角斜杠（U+FF0F）容错——`／help` 与 `／STATUS`（命令名大写）归一后
/// 按对应命令处理（handle 入口一处归一，覆盖所有命令）。
#[tokio::test]
async fn fullwidth_slash_commands_normalized() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    feed_and_wait(
        &ctx,
        vec![
            msg("feishu:ou_t", "alice", "／help"),
            msg("feishu:ou_t", "alice", "／STATUS"),
            msg("feishu:ou_t", "alice", "／nosuchcmd"),
        ],
        0,
    )
    .await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("/new") && t.contains("/help")),
        "／help 应出命令帮助: {inbox:?}"
    );
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("uptime") || t.contains("运行")),
        "／STATUS 应按 /status 处理: {inbox:?}"
    );
    assert!(
        inbox.iter().any(|t| t.contains("未知命令")),
        "未知全角命令应回未知提示: {inbox:?}"
    );
    // 全角斜杠消息不应驱动 agent（命令路径 return）。
    assert_eq!(ctx.order.load(Ordering::SeqCst), 0, "命令不驱动 agent");
    drop_db(ctx.db).await;
}

/// 快赢：私聊陌生人引导（stranger_p2p_hint 默认 true）——未放行用户的私聊回
/// 引导（含 sender id 与 /allow 指引）；关闭后完全静默；群内行为不变（默认仍
/// 静默——群提示走 stranger_mention_hint，两者独立）。
#[tokio::test]
async fn stranger_p2p_hint_on_off_and_group_unchanged() {
    let _serial = SERIAL.lock().await;
    // 默认开：私聊陌生人回引导。
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    feed_and_wait(&ctx, vec![msg("feishu:ou_stranger", "stranger", "hi")], 0).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("stranger") && t.contains("/allow stranger")),
        "私聊陌生人应回含 id 与 /allow 的引导: {inbox:?}"
    );
    assert!(
        inbox.iter().all(|t| !t.contains("发现模式")),
        "非发现模式（白名单非空）不给发现引导: {inbox:?}"
    );
    drop_db(ctx.db).await;

    // 默认开但群内陌生人（未 @bot）仍静默（群行为不变，群提示独立开关）。
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    feed_and_wait(&ctx, vec![msg("feishu:oc_g", "stranger", "hi")], 0).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(inbox.is_empty(), "群内默认静默: {inbox:?}");
    drop_db(ctx.db).await;

    // 关闭：私聊同样完全静默。
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    ctx.disp
        .set_prefs(false, false, crate::config::ReplyMode::Card);
    feed_and_wait(&ctx, vec![msg("feishu:ou_stranger", "stranger", "hi")], 0).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(inbox.is_empty(), "关闭后私聊静默: {inbox:?}");
    drop_db(ctx.db).await;
}

/// 构造撤回控制消息（feishu drain 合成形态的最小化模拟）。
fn recall_msg(conv: &str, msg_id: &str, notify: Option<&str>, probes: &[&str]) -> InboundMessage {
    InboundMessage {
        conv_id: ConvId(conv.into()),
        sender: UserId(String::new()),
        sender_name: None,
        text: None,
        media: Vec::new(),
        media_errors: Vec::new(),
        mentions: Vec::new(),
        mentioned_bot: false,
        ask_req: None,
        reply_to: None,
        source_msg_id: Some(msg_id.into()),
        control: Some(crate::types::InboundControl::MessageRecalled {
            notify_conv: notify.map(|c| ConvId(c.into())),
            probe_convs: probes.iter().map(|c| ConvId((*c).into())).collect(),
        }),
        no_steer: false,
        reply_hint: ReplyHint::None,
    }
}

/// 事件接入（一期）：撤回把同 id 的**排队**消息移出（下一轮不再合并）；另一条
/// 不同 id 的排队消息不受影响。
#[tokio::test]
async fn recall_removes_matching_queued_message() {
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
        disp.handle(msg("feishu:ou_t", "alice", "round A")).await;
    });
    assert!(
        wait_registered(&ctx, "feishu:ou_t").await,
        "在飞任务应已注册"
    );
    // 两条带平台消息 id 的排队消息。
    let mut m1 = msg("feishu:ou_t", "alice", "queued to recall");
    m1.source_msg_id = Some("om_rec_1".into());
    let mut m2 = msg("feishu:ou_t", "alice", "queued keep");
    m2.source_msg_id = Some("om_keep".into());
    ctx.disp.handle(m1).await;
    ctx.disp.handle(m2).await;
    for _ in 0..400 {
        if conv_queued_len(&ctx.disp, "feishu:ou_t").await == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // 撤回 om_rec_1（notify/probe 均给到可回执 conv）。
    ctx.disp
        .handle(recall_msg(
            "feishu:oc_chat",
            "om_rec_1",
            Some("feishu:oc_chat"),
            &["feishu:oc_chat", "feishu:ou_t"],
        ))
        .await;
    // 队列只剩 om_keep。
    let queued_ids: Vec<String> = {
        // （T18 机械调整：queues 并入 ConvState 单表。）
        let map = ctx.disp.conv_states.lock().await;
        map.get("feishu:ou_t")
            .and_then(|cs| cs.queue.as_ref())
            .map(|q| {
                q.iter()
                    .filter_map(|m| m.msg.source_msg_id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    assert_eq!(
        queued_ids,
        vec!["om_keep".to_string()],
        "撤回后队列应只剩 om_keep"
    );
    // 排队提示同步收缩。（T18 机械调整：queued_hints 并入 ConvState 单表。）
    assert!(
        ctx.disp
            .peek_conv("feishu:ou_t", |cs| {
                cs.and_then(|c| c.queued_hint.as_ref()).map(|h| h.count)
            })
            .await
            .is_some_and(|count| count == 1),
        "排队提示应收缩为 1"
    );
    // 撤回排队消息不回任何提示（静默移除）。
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().all(|t| !t.contains("已撤回")),
        "排队移除不发提示: {inbox:?}"
    );
    drop_db(ctx.db).await;
    runner.abort();
}

/// 事件接入（一期）：撤回未命中队列但该会话有在飞任务 → 回「已开始，可 /stop」
/// 提示（不自动停）；无在飞任务（已执行完/从未入队）→ 静默忽略。
#[tokio::test]
async fn recall_running_gets_hint_and_idle_silent() {
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
        disp.handle(msg("feishu:ou_t", "alice", "long job")).await;
    });
    assert!(
        wait_registered(&ctx, "feishu:ou_t").await,
        "在飞任务应已注册"
    );
    // 撤回一条不在队列的消息（probe 覆盖在飞 conv）。
    ctx.disp
        .handle(recall_msg(
            "feishu:ou_t",
            "om_gone",
            Some("feishu:ou_t"),
            &["feishu:ou_t"],
        ))
        .await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("已撤回") && t.contains("/stop")),
        "在飞会话的撤回应回提示: {inbox:?}"
    );
    drop_db(ctx.db).await;
    runner.abort();

    // 无在飞任务：静默忽略（不回提示）。
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    ctx.disp
        .handle(recall_msg(
            "feishu:ou_t",
            "om_never",
            Some("feishu:ou_t"),
            &["feishu:ou_t"],
        ))
        .await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(inbox.is_empty(), "无任务时撤回应静默: {inbox:?}");
    drop_db(ctx.db).await;
}

/// 事件接入：bot 被移出群——白名单群被收回（内存 + store 双写）+ 首位管理员
/// 收私聊通知；不在白名单的群移出只记日志（不打扰管理员）。
#[tokio::test]
async fn bot_removed_from_chat_revokes_and_notifies_admin() {
    let _serial = SERIAL.lock().await;
    let auth = Auth::with_chats(vec!["ou_admin".into()], vec!["feishu:oc_dead".into()]);
    let ctx = build(auth).await;
    let removed = InboundMessage {
        conv_id: ConvId("feishu:oc_dead".into()),
        sender: UserId(String::new()),
        sender_name: None,
        text: None,
        media: Vec::new(),
        media_errors: Vec::new(),
        mentions: Vec::new(),
        mentioned_bot: false,
        ask_req: None,
        reply_to: None,
        source_msg_id: None,
        control: Some(crate::types::InboundControl::BotRemovedFromChat),
        no_steer: false,
        reply_hint: ReplyHint::None,
    };
    ctx.disp.handle(removed).await;
    // 内存：群白名单已收回。
    assert!(
        !ctx.disp.auth.is_chat_allowed("feishu:oc_dead"),
        "移出后群应不再放行"
    );
    // 管理员收到私聊通知（mock platform 收到的 send_text）。
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("已被移出群 feishu:oc_dead") && t.contains("会话白名单移除")),
        "管理员应收到移出通知: {inbox:?}"
    );
    drop_db(ctx.db).await;

    // 不在白名单的群移出：不通知（避免噪音）。
    let auth = Auth::with_chats(vec!["ou_admin".into()], vec!["feishu:oc_other".into()]);
    let ctx = build(auth).await;
    let removed = InboundMessage {
        conv_id: ConvId("feishu:oc_unknown".into()),
        sender: UserId(String::new()),
        sender_name: None,
        text: None,
        media: Vec::new(),
        media_errors: Vec::new(),
        mentions: Vec::new(),
        mentioned_bot: false,
        ask_req: None,
        reply_to: None,
        source_msg_id: None,
        control: Some(crate::types::InboundControl::BotRemovedFromChat),
        no_steer: false,
        reply_hint: ReplyHint::None,
    };
    ctx.disp.handle(removed).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(inbox.is_empty(), "非白名单群移出不通知: {inbox:?}");
    drop_db(ctx.db).await;
}

// ===========================================================================
// Wave B：飞书交互专项测试（触达 / 群协作身份 / 恢复引导 / 运营数据）
// ===========================================================================

/// Wave B-2/B-9：自定义平台与后端的组装底座（新测试用，避免再复制 8 参构造）。
async fn build_with_parts(
    auth: Auth,
    plat: MockPlatform,
    back: MockBackend,
    default_workdir: std::path::PathBuf,
) -> Ctx {
    let _ = std::fs::create_dir_all(&default_workdir);
    let (inbox, send_count) = (plat_inbox_of(&plat), plat_count_of(&plat));
    let (store, db) = tmp_store().await;
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
        calls: Default::default(),
        prompts: Default::default(),
        order: Default::default(),
        db,
    }
}

/// 取 MockPlatform 的观测句柄（组装底座用；句柄本就 clone 共享）。
fn plat_inbox_of(p: &MockPlatform) -> InboxHandle {
    p.inbox.clone()
}
fn plat_count_of(p: &MockPlatform) -> CounterHandle {
    p.send_count.clone()
}

/// Wave B-2：加急平台 + 慢后端（进行中登记询问 → 完成强提醒测试用）。
async fn build_urgent_slow(auth: Auth, slow_ms: u64) -> Ctx {
    let (plat, _i, _c) = MockPlatform::new_urgent();
    let (back, calls, prompts, order) = MockBackend::new_slow(slow_ms);
    let mut ctx = build_with_parts(auth, plat, back, "/tmp/imagent-test-ws".into()).await;
    ctx.calls = calls;
    ctx.prompts = prompts;
    ctx.order = order;
    ctx
}

/// W4-1：per-sender 成本上限——近 24h 累计达上限的新轮次直接拒绝（不启动
/// agent），回执说明；未达上限正常执行。
#[tokio::test]
async fn sender_cost_limit_rejects_when_exceeded() {
    let _serial = SERIAL.lock().await;
    let (plat, inbox, send_count) = MockPlatform::new();
    let (back, calls, prompts, order) = MockBackend::new();
    let _ = std::fs::create_dir_all("/tmp/imagent-test-ws");
    let (store, db) = tmp_store().await;
    // 预置 alice 近 24h 已花 $2（上限 $1）。
    store
        .append_run_stat(
            "feishu:ou_other",
            Some("claude-cli"),
            0,
            0,
            None,
            Some(2.0),
            Some("alice"),
        )
        .await
        .unwrap();
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        Auth::new(vec!["alice".into()]),
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into()],
        PermissionMode::Off,
        TaskBudgets {
            sender_daily_cost_limit_usd: Some(1.0),
            ..test_budgets()
        },
        CotDetail::Brief,
        vec![],
    ));
    let ctx = Ctx {
        disp: disp.clone(),
        inbox,
        send_count,
        calls,
        prompts,
        order,
        db: db.clone(),
    };
    ctx.disp.handle(msg("c1", "alice", "新任务")).await;
    assert!(ctx.prompts.lock().await.is_empty(), "超限不应启动 agent");
    let inbox_seen = ctx.inbox.lock().await.clone();
    assert!(
        inbox_seen.iter().any(|t| t.contains("用量已达上限")),
        "应回上限说明: {inbox_seen:?}"
    );
    drop_db(ctx.db).await;
}

/// W3-3 + P0-5（v1.17）：/retry 重发**最近失败轮**的 prompt（成功轮不落库、
/// 不覆盖——失败卡按钮永远指向失败那轮）；无历史时回可行动提示。
#[tokio::test]
async fn retry_reruns_last_prompt() {
    let _serial = SERIAL.lock().await;
    // 失败轮：prompt 落库（store config），/retry 走完整 runner 路径重发。
    {
        let _ = std::fs::create_dir_all("/tmp/imagent-test-ws");
        let (plat, inbox, send_count) = MockPlatform::new();
        let (back, calls, prompts, order) = MockBackend::new_failing();
        let (store, db) = tmp_store().await;
        let auth = Auth::new(vec!["alice".into()]);
        let disp = Arc::new(Dispatcher::new(
            Arc::new(plat),
            Arc::new(back),
            store,
            auth.clone(),
            std::path::PathBuf::from("/tmp/imagent-test-ws"),
            vec!["Read".into()],
            PermissionMode::Off,
            test_budgets(),
            CotDetail::Brief,
            auth.snapshot(),
        ));
        let ctx = Ctx {
            disp,
            inbox,
            send_count,
            calls,
            prompts,
            order,
            db: db.clone(),
        };
        ctx.disp.handle(msg("c1", "alice", "第一轮任务")).await;
        assert_eq!(ctx.prompts.lock().await.len(), 1);
        ctx.disp.handle(msg("c1", "alice", "/retry")).await;
        let prompts = ctx.prompts.lock().await.clone();
        assert_eq!(
            prompts,
            vec!["第一轮任务".to_string(), "第一轮任务".to_string()],
            "/retry 应重发失败轮 prompt: {prompts:?}"
        );
        drop_db(ctx.db).await;
    }

    // 成功轮之后直接 /retry：无可重试（成功不落库）→ 提示。
    {
        let ctx = build(Auth::new(vec!["alice".into()])).await;
        ctx.disp.handle(msg("c1", "alice", "成功任务")).await;
        assert_eq!(ctx.prompts.lock().await.len(), 1);
        ctx.disp.handle(msg("c1", "alice", "/retry")).await;
        let inbox = ctx.inbox.lock().await.clone();
        assert!(
            inbox.iter().any(|t| t.contains("没有可重试")),
            "成功轮后 /retry 应提示: {inbox:?}"
        );
        drop_db(ctx.db).await;
    }

    // 无历史（新会话直接 /retry）→ 提示而非静默。
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    ctx.disp.handle(msg("c2", "alice", "/retry")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("没有可重试")),
        "无历史应提示: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

/// W2-5：自动 compact——成功轮次水位（usage.input_tokens）超阈值，runner 循环
/// 自动走压缩管道：第二条 prompt 为压缩指令、会话重置（sessions 表清空）、
/// 摘要落 KV、用户收到「已自动压缩」回执。阈值 0（默认）不触发。
#[tokio::test]
async fn auto_compact_triggers_after_threshold() {
    let _serial = SERIAL.lock().await;
    let (plat, inbox, send_count) = MockPlatform::new();
    let usage = crate::types::UsageStats {
        input_tokens: 150_000,
        output_tokens: 100,
        cached_tokens: None,
        total_cost_usd: Some(0.1),
        context_window: None,
    };
    let (back, calls, prompts, order) = MockBackend::new_with_usage(usage);
    let _ = std::fs::create_dir_all("/tmp/imagent-test-ws");
    let (store, db) = tmp_store().await;
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store.clone(),
        Auth::new(vec!["alice".into()]),
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into()],
        PermissionMode::Off,
        TaskBudgets {
            auto_compact_threshold_tokens: 100_000,
            ..test_budgets()
        },
        CotDetail::Brief,
        vec![],
    ));
    let ctx = Ctx {
        disp: disp.clone(),
        inbox,
        send_count,
        calls,
        prompts,
        order,
        db: db.clone(),
    };
    ctx.disp
        .handle(msg("c1", "alice", "long context task"))
        .await;
    let prompts_seen = ctx.prompts.lock().await.clone();
    assert_eq!(
        prompts_seen.len(),
        2,
        "应跑两轮（原任务 + 压缩）: {prompts_seen:?}"
    );
    assert_eq!(prompts_seen[0], "long context task");
    assert!(
        prompts_seen[1].contains("总结"),
        "第二轮应为压缩指令: {prompts_seen:?}"
    );
    let inbox_seen = ctx.inbox.lock().await.clone();
    assert!(
        inbox_seen.iter().any(|t| t.contains("上下文已压缩")),
        "应回自动压缩完成: {inbox_seen:?}"
    );
    // 压缩后活动会话被重置（下次消息新建）。
    assert!(
        ctx.check().await.get_session("c1").await.unwrap().is_none(),
        "压缩后活动 session 应被删除"
    );
    drop_db(ctx.db).await;

    // 阈值 0（默认关闭）：同样水位不触发压缩，只有一轮。
    let (plat, inbox, send_count) = MockPlatform::new();
    let (back, calls, prompts, order) = MockBackend::new_with_usage(usage);
    let _ = std::fs::create_dir_all("/tmp/imagent-test-ws");
    let (store, db) = tmp_store().await;
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        Auth::new(vec!["alice".into()]),
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into()],
        PermissionMode::Off,
        test_budgets(),
        CotDetail::Brief,
        vec![],
    ));
    let ctx = Ctx {
        disp,
        inbox,
        send_count,
        calls,
        prompts,
        order,
        db,
    };
    ctx.disp
        .handle(msg("c1", "alice", "long context task"))
        .await;
    assert_eq!(ctx.prompts.lock().await.len(), 1, "阈值 0 不触发自动压缩");
    drop_db(ctx.db).await;
}

/// Wave B-1：审批过半催办文案——剩余分钟向上取整（30s → 1 分钟）、工具名与
/// y/n 指引齐备。
#[test]
fn approval_buzz_text_formats() {
    let t = super::approval_buzz_text("Bash", Duration::from_secs(150));
    assert!(t.contains("⏰"), "{t}");
    assert!(t.contains("剩 3 分钟"), "150s → 3 分钟: {t}");
    assert!(t.contains("Bash"), "{t}");
    assert!(t.contains("回复 y/n 亦可"), "{t}");
    // 30 秒 → 1 分钟（向上取整，宁多勿少）。
    assert!(super::approval_buzz_text("WebFetch", Duration::from_secs(30)).contains("剩 1 分钟"),);
}

/// Wave B-1：等待过半催办——过半未决发**一次** buzz、随后超时出口；半程内回复
/// 则完全不催办。真实时钟（总量 ~300ms）。
#[tokio::test]
async fn wait_reply_buzz_once_then_timeout() {
    let (plat, inbox, _c) = MockPlatform::new_urgent();
    let router = crate::permission::PermissionRouter::new();
    let rx = router
        .register(
            "c1",
            "r1",
            None,
            crate::permission::PendingKind::Permission,
            Some("Bash"),
            None,
        )
        .await;
    let out = super::wait_reply_with_buzz(
        rx,
        &ConvId("c1".into()),
        "Bash",
        Duration::from_millis(200),
        &plat,
    )
    .await;
    assert!(
        matches!(out, super::AskWaitOutcome::TimedOut),
        "无回复应超时"
    );
    let snap = inbox.lock().await.clone();
    let buzzes: Vec<_> = snap.iter().filter(|t| t.starts_with("[buzz]")).collect();
    assert_eq!(buzzes.len(), 1, "只催办一次: {snap:?}");
    assert!(buzzes[0].contains("Bash"), "文案带工具名: {snap:?}");

    // 半程内回复：无催办、正常拿到回复。
    let rx2 = router
        .register(
            "c1",
            "r2",
            None,
            crate::permission::PendingKind::Permission,
            Some("Read"),
            None,
        )
        .await;
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        let _ = router
            .route(
                "c1",
                Some("r2"),
                None,
                crate::permission::PermissionReply {
                    allow: true,
                    always: false,
                    message: None,
                    raw_text: Some("y".into()),
                    cancelled: false,
                },
            )
            .await;
    });
    let out2 = super::wait_reply_with_buzz(
        rx2,
        &ConvId("c1".into()),
        "Read",
        Duration::from_millis(200),
        &plat,
    )
    .await;
    match out2 {
        super::AskWaitOutcome::Replied(r) => assert!(r.allow),
        other => panic!("应正常回复: {other:?}"),
    }
    let final_inbox = inbox.lock().await.clone();
    assert_eq!(
        final_inbox
            .iter()
            .filter(|t| t.starts_with("[buzz]"))
            .count(),
        1,
        "快速回复不追加催办: {final_inbox:?}"
    );
}

/// Wave B-2：完成强提醒文案与触发条件（纯函数）。
#[test]
fn task_done_buzz_text_and_threshold() {
    assert_eq!(
        super::task_done_buzz_text(Duration::from_secs(750), Some("$0.012")),
        "✅ 任务完成 · 12m30s · $0.012"
    );
    assert_eq!(
        super::task_done_buzz_text(Duration::from_secs(42), None),
        "✅ 任务完成 · 42s"
    );
    assert_eq!(
        super::task_done_buzz_text(Duration::from_secs(3600), None),
        "✅ 任务完成 · 1h00m"
    );
    // 触发条件（真机校准 2026-08 收紧）：>5 分钟，或本轮发生过询问且 >1 分钟
    //（刚点完审批的短轮次用户还在看会话，终态卡 footer 已含信息，不推）。
    assert!(
        !super::should_buzz_done(Duration::from_secs(300), 0),
        "恰好 300s 不触发"
    );
    assert!(
        super::should_buzz_done(Duration::from_secs(301), 0),
        "超 300s 触发"
    );
    assert!(
        !super::should_buzz_done(Duration::from_secs(24), 1),
        "含询问的短轮次（24s）不触发——真机校准案例"
    );
    assert!(
        super::should_buzz_done(Duration::from_secs(61), 1),
        "含询问且超 1 分钟触发"
    );
    assert!(!super::should_buzz_done(Duration::from_secs(1), 0));
}

/// Wave B-2：本轮发生过询问**且轮次 >1 分钟**才发 buzz 完成短文本（真机校准
/// 2026-08 收紧：短轮次终态卡 footer 已含信息，刚审批完的用户还在看会话）；
/// 本测试的快轮次（<1s 含询问）不再发；普通短轮次（无询问、<5 分钟）不发；
/// 不支持 buzz 的平台（默认 mock）整体 no-op。>60s 含询问的触发由
/// should_buzz_done 纯函数测试覆盖。
#[tokio::test]
async fn round_buzzes_done_when_ask_happened() {
    let _serial = SERIAL.lock().await;
    let auth = Auth::new(vec!["alice".into()]);
    // ① 支持 buzz 的平台：慢后端 300ms，轮次进行中登记一次询问（ask 计数 +1）。
    let ctx = build_urgent_slow(auth.clone(), 300).await;
    let disp = ctx.disp.clone();
    let handle = tokio::spawn(async move {
        disp.handle(msg("c1", "alice", "跑个任务")).await;
    });
    assert!(wait_registered(&ctx, "c1").await, "轮次应注册在飞");
    ctx.disp
        .router()
        .register(
            "c1",
            "r-1",
            None,
            crate::permission::PendingKind::Permission,
            Some("Bash"),
            None,
        )
        .await;
    handle.await.expect("round");
    let inbox = ctx.inbox.lock().await.clone();
    let buzzes: Vec<_> = inbox
        .iter()
        .filter(|t| t.starts_with("[buzz]") && t.contains("✅ 任务完成"))
        .collect();
    assert!(
        buzzes.is_empty(),
        "短轮次（<1s）含询问不弹完成强提醒（真机校准 2026-08）: {inbox:?}"
    );
    drop_db(ctx.db).await;

    // ② 普通短轮次（无询问）：不发。
    let ctx = build_urgent_slow(auth.clone(), 0).await;
    feed_and_wait(&ctx, vec![msg("c2", "alice", "hi")], 1).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        !inbox.iter().any(|t| t.contains("✅ 任务完成")),
        "短轮次无询问不发: {inbox:?}"
    );
    drop_db(ctx.db).await;

    // ③ 不支持 buzz 的平台（默认 mock，慢后端保证轮次可观测）：即便有询问也
    // 不发（no-op）。
    let (plat, _pi, _pc) = MockPlatform::new();
    let (back, calls, prompts, order) = MockBackend::new_slow(300);
    let mut ctx = build_with_parts(auth, plat, back, "/tmp/imagent-test-ws".into()).await;
    ctx.calls = calls;
    ctx.prompts = prompts;
    ctx.order = order;
    let disp = ctx.disp.clone();
    let handle = tokio::spawn(async move {
        disp.handle(msg("c3", "alice", "跑个任务")).await;
    });
    assert!(wait_registered(&ctx, "c3").await, "轮次应注册在飞");
    ctx.disp
        .router()
        .register(
            "c3",
            "r-2",
            None,
            crate::permission::PendingKind::Permission,
            Some("Bash"),
            None,
        )
        .await;
    handle.await.expect("round");
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        !inbox.iter().any(|t| t.contains("✅ 任务完成")),
        "不支持 buzz 的平台 no-op: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

/// Wave B-7：/config cot——白名单非 admin 可改**本会话**档位；cot_detail（全局）
/// 仍 admin 门槛；default 清除；非法值给用法。档位对下一轮生效（cot_for）。
#[tokio::test]
async fn config_cot_per_conv_for_whitelisted_non_admin() {
    let _serial = SERIAL.lock().await;
    // admin 为空：alice 只是白名单用户（非 admin）。
    let auth = Auth::new(vec!["alice".into()]);
    let ctx = build_with_admin(auth, vec![]).await;

    // 全局键仍被拒（admin 门槛）。
    feed_and_wait(&ctx, vec![msg("c1", "alice", "/config cot_detail off")], 0).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("仅管理员")),
        "cot_detail 全局键非 admin 被拒: {inbox:?}"
    );

    // per-conv 键白名单可用。
    feed_and_wait(&ctx, vec![msg("c1", "alice", "/config cot off")], 0).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("本会话 cot = off") && t.contains("仅本会话")),
        "per-conv 键成功: {inbox:?}"
    );
    // 档位生效：cot_for 返回覆盖值；其它 conv 跟随全局。
    assert_eq!(ctx.disp.cot_for("c1").await, CotDetail::Off);
    assert_eq!(ctx.disp.cot_for("c2").await, CotDetail::Brief);

    // 非法值 → 用法提示。
    feed_and_wait(&ctx, vec![msg("c1", "alice", "/config cot bogus")], 0).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.iter().any(|t| t.contains("用法：/config cot")),
        "非法值给用法: {inbox:?}"
    );

    // default 清除 → 回全局。
    feed_and_wait(&ctx, vec![msg("c1", "alice", "/config cot default")], 0).await;
    assert_eq!(ctx.disp.cot_for("c1").await, CotDetail::Brief);
    drop_db(ctx.db).await;
}

/// Wave B-9：「继续」类断档提示——无可续接会话且 prompt ≤4 字命中续接词表时
/// 回复前置提示；普通（长）prompt 与已有会话时不提示。
#[tokio::test]
async fn continuation_orphan_hint_on_fresh_conv() {
    let _serial = SERIAL.lock().await;
    let auth = Auth::new(vec!["alice".into()]);
    let ctx = build(auth).await;
    // 断档：新 conv + 续接词。
    for (conv, text) in [("c1", "继续"), ("c2", "go on"), ("c3", "然后")] {
        feed_and_wait(&ctx, vec![msg(conv, "alice", text)], 1).await;
    }
    let inbox = ctx.inbox.lock().await.clone();
    let hinted: Vec<_> = inbox
        .iter()
        .filter(|t| t.contains("当前无可续接会话"))
        .collect();
    assert_eq!(hinted.len(), 3, "三个续接词都提示: {inbox:?}");
    for h in &hinted {
        assert!(h.contains("/resume"), "指引 /resume: {h}");
    }
    // 普通 prompt 不提示。
    feed_and_wait(
        &ctx,
        vec![msg(
            "c4",
            "alice",
            "帮我继续把重构做完，重点是 dispatch 模块",
        )],
        1,
    )
    .await;
    let inbox = ctx.inbox.lock().await.clone();
    assert_eq!(
        inbox
            .iter()
            .filter(|t| t.contains("当前无可续接会话"))
            .count(),
        3,
        "长 prompt 不提示: {inbox:?}"
    );
    // 已有会话的「继续」不提示（真实续接）。
    feed_and_wait(&ctx, vec![msg("c1", "alice", "继续")], 2).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert_eq!(
        inbox
            .iter()
            .filter(|t| t.contains("当前无可续接会话"))
            .count(),
        3,
        "有会话续接不提示: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

/// Wave B-10：workdir 失效前置检查——目录不存在时不启动 agent，回可读错误。
#[tokio::test]
async fn round_preflights_missing_workdir() {
    let _serial = SERIAL.lock().await;
    let auth = Auth::new(vec!["alice".into()]);
    let missing = std::path::PathBuf::from("/tmp/imagent-definitely-missing-ws");
    let ctx = build_with_workdir(auth, missing.clone()).await;
    // 组装底座会把 workdir 建出来（与生产语义一致）；本测试模拟「启动后目录被
    // 删/失效」——建完再删，验证轮次预检拦截。
    std::fs::remove_dir_all(&missing).expect("删除测试目录");
    feed_and_wait(&ctx, vec![msg("c1", "alice", "hi")], 0).await;
    // backend 未被调用（未启动 agent）。
    assert_eq!(ctx.calls.lock().await.len(), 0, "agent 不应启动");
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox
            .iter()
            .any(|t| t.contains("不存在") && t.contains("/cd") && t.contains("工作目录")),
        "回可读错误与指引: {inbox:?}"
    );
    drop_db(ctx.db).await;
}

/// Wave B-11：/stats 审批分组——从 permission_decision 审计聚合近 7 天的
/// 次数/占比/平均响应。
#[tokio::test]
async fn stats_includes_approval_group() {
    let _serial = SERIAL.lock().await;
    let auth = Auth::new(vec!["alice".into()]);
    let ctx = build(auth).await;
    // 先跑一轮（run_stats 非空，跳过「暂无运行记录」早退）。
    feed_and_wait(&ctx, vec![msg("c1", "alice", "hi")], 1).await;
    // 手工落 4 条审批审计（3 决策 + 1 超时）。
    let store = ctx.check().await;
    for detail in [
        "tool=Bash decision=allow sender=u1 waited_secs=60",
        "tool=Bash decision=deny sender=u1 waited_secs=120",
        "tool=WebFetch decision=allow_always sender=u2 waited_secs=180",
        "tool=Bash decision=timeout waited_secs=300",
    ] {
        store
            .append_audit("permission_decision", None, Some("c1"), Some(detail))
            .await
            .unwrap();
    }
    feed_and_wait(&ctx, vec![msg("c1", "alice", "/stats")], 2).await;
    let inbox = ctx.inbox.lock().await.clone();
    let stats_msg = inbox
        .iter()
        .rev()
        .find(|t| t.contains("审批（近 7 天）"))
        .expect("应有审批分组");
    assert!(stats_msg.contains("4 次"), "总次数: {stats_msg}");
    // 1 allow + 1 deny + 1 timeout + 1 allow_always = 各 25%。
    for word in ["allow 25%", "deny 25%", "timeout 25%", "always 25%"] {
        assert!(stats_msg.contains(word), "{word}: {stats_msg}");
    }
    assert!(
        stats_msg.contains("平均响应 2 分钟"),
        "(60+120+180+300)/4 = 165s ≈ 2 分钟: {stats_msg}"
    );
    drop_db(ctx.db).await;
}

/// v1.20 /cron 停机补跑：all 策略下错过多周期逐条补跑（标注 i/n，上限 3）；
/// off 策略下陈旧到期只重排不触发。
#[tokio::test]
async fn cron_catchup_all_backfills_and_off_skips_stale() {
    let _serial = SERIAL.lock().await;
    // —— all：错过 3 个周期的每分钟任务 → 补 3 条 ——
    let budgets = TaskBudgets {
        cron_catchup: crate::config::CronCatchup::All,
        ..test_budgets()
    };
    let ctx = build_slow(Auth::new(vec!["alice".into()]), 0, budgets).await;
    ctx.disp
        .handle(msg("c1", "alice", "/cron add * * * * * 报数"))
        .await;
    let jobs = ctx.check().await.list_cron_jobs().await.unwrap();
    let j = &jobs[0];
    // 模拟停机错过：把 next_run 拨回 4 分钟前。
    ctx.check()
        .await
        .bump_cron_job(&j.id, 0, crate::dispatch::now_secs() - 240)
        .await
        .unwrap();
    ctx.disp.fire_due_cron_jobs().await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut count: u32;
    loop {
        // 批处理会把同 conv 的补跑消息合并为一轮 prompt——按「补跑」标记
        // 出现次数计（而非消息条数）。
        count = ctx
            .prompts
            .lock()
            .await
            .iter()
            .map(|p| p.matches("补跑").count() as u32)
            .sum();
        if count >= 3 || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        count,
        3,
        "all 应补跑 3 条（上限）: {:?}",
        ctx.prompts.lock().await
    );
    drop_db(ctx.db).await;

    // —— off：陈旧到期跳过 ——
    let budgets = TaskBudgets {
        cron_catchup: crate::config::CronCatchup::Off,
        ..test_budgets()
    };
    let ctx = build_slow(Auth::new(vec!["alice".into()]), 0, budgets).await;
    ctx.disp
        .handle(msg("c1", "alice", "/cron add * * * * * 报数"))
        .await;
    let jobs = ctx.check().await.list_cron_jobs().await.unwrap();
    let j = &jobs[0];
    ctx.check()
        .await
        .bump_cron_job(&j.id, 0, crate::dispatch::now_secs() - 240)
        .await
        .unwrap();
    ctx.disp.fire_due_cron_jobs().await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !ctx.prompts.lock().await.iter().any(|p| p.contains("报数")),
        "off 陈旧到期不应触发: {:?}",
        ctx.prompts.lock().await
    );
    drop_db(ctx.db).await;
}

/// v1.18 /cron 全链路：add 校验与落库 → list → 到期驱动 fire_due（合成消息走
/// handle，MockBackend 收到注入前缀 prompt，store 重排）→ rm。
/// v1.20 崩溃轮次恢复：inflight 残留 → 转 last_prompt（/retry 数据源）+
/// 会话收到可续跑通知。
#[tokio::test]
async fn crashed_round_recovery_moves_to_retry() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 模拟崩溃残留（轮首写入、未及清除）。
    let payload = serde_json::json!({ "prompt": "跑一半的长任务", "at": 100 });
    // Ctx 无 store 句柄——经 check() 重开同库写入（dispatcher 与之共享 db 文件）。
    let store = ctx.check().await;
    store
        .set_config("inflight_prompt:c1", &payload.to_string())
        .await
        .unwrap();
    ctx.disp.recover_crashed_rounds().await;
    // 已转 last_prompt 且 inflight 清除。
    let retry = store.get_config("last_prompt:c1").await.unwrap();
    assert!(
        retry.as_deref().unwrap_or("").contains("跑一半的长任务"),
        "last_prompt={retry:?}"
    );
    assert!(
        store
            .get_config("inflight_prompt:c1")
            .await
            .unwrap()
            .is_none(),
        "inflight 应已清除"
    );
    // 会话收到通知（轮询——reply 经 platform）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if ctx
            .inbox
            .lock()
            .await
            .iter()
            .any(|t| t.contains("/retry") && t.contains("未完成"))
        {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("未收到崩溃恢复通知: {:?}", ctx.inbox.lock().await);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    drop_db(ctx.db).await;
}

/// v1.23 review（方向修正回归锚）：前一轮失败留下 last_prompt（旧 at）、
/// 崩溃轮 inflight 更新——恢复后 /retry 必须指向**崩溃轮**（更新的那条）。
/// v1.21 的实现（存在即不覆盖）在此场景会保留旧 prompt，等于禁用崩溃恢复
/// 且引导用户重跑更早的副作用指令。
#[tokio::test]
async fn crashed_round_recovery_prefers_newer_inflight() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    let store = ctx.check().await;
    // 场景：轮 1「部署 staging」失败（last_prompt at=100）→ 轮 2「回滚」
    // 进行中崩溃（inflight at=200）。
    store
        .set_config(
            "last_prompt:c1",
            &serde_json::json!({ "prompt": "部署 staging", "at": 100 }).to_string(),
        )
        .await
        .unwrap();
    store
        .set_config(
            "inflight_prompt:c1",
            &serde_json::json!({ "prompt": "回滚", "at": 200 }).to_string(),
        )
        .await
        .unwrap();
    ctx.disp.recover_crashed_rounds().await;
    let retry = store.get_config("last_prompt:c1").await.unwrap();
    assert!(
        retry.as_deref().unwrap_or("").contains("回滚"),
        "崩溃轮（更新的 inflight）应接管 last_prompt：{retry:?}"
    );
    // 反向：现存 last_prompt 严格更新（inflight 清理失败残留的陈旧标记）→ 保留现存。
    store
        .set_config(
            "last_prompt:c2",
            &serde_json::json!({ "prompt": "新失败轮", "at": 500 }).to_string(),
        )
        .await
        .unwrap();
    store
        .set_config(
            "inflight_prompt:c2",
            &serde_json::json!({ "prompt": "陈旧残留", "at": 100 }).to_string(),
        )
        .await
        .unwrap();
    ctx.disp.recover_crashed_rounds().await;
    let keep = store.get_config("last_prompt:c2").await.unwrap();
    assert!(
        keep.as_deref().unwrap_or("").contains("新失败轮"),
        "陈旧 inflight 不应顶掉更新的 last_prompt：{keep:?}"
    );
    drop_db(ctx.db).await;
}

/// v13 P3（崩溃恢复双注入）：轮首 inflight 必须落 **base_prompt**（注入前）
/// ——带摘要注入的轮崩溃后 /retry 重放走完整注入管道，【前情摘要】在重放
/// prompt 里恰出现一次。旧实现存注入后的 prompt：崩溃轮未成功落库、摘要
/// 未删且重放轮 existing=None → 摘要二次注入（陈旧媒体路径提示同族残留）。
#[tokio::test]
async fn crashed_round_retry_replays_base_prompt_single_summary() {
    let _serial = SERIAL.lock().await;
    let ctx = build_slow(
        Auth::new(vec!["alice".into()]),
        60_000,
        TaskBudgets {
            batch_window: Duration::from_millis(1),
            ..test_budgets()
        },
    )
    .await;
    // 预置摘要 + 无活动 session → 首轮新建会话注入摘要。
    ctx.check()
        .await
        .set_config("compact_summary:c5", "旧会话的摘要内容")
        .await
        .unwrap();
    let disp = ctx.disp.clone();
    let runner = tokio::spawn(async move { disp.handle(msg("c5", "alice", "继续整理")).await });
    // 首轮起跑（注入后 prompt 已送达 backend，含一次摘要）。
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while ctx.prompts.lock().await.is_empty() {
        if std::time::Instant::now() > deadline {
            panic!("首轮未起跑: {:?}", ctx.prompts.lock().await);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // 轮在飞：inflight 行已落库——必须是注入前的 base（旧实现此处即回归点）。
    let store = ctx.check().await;
    let inflight = {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(v) = store.get_config("inflight_prompt:c5").await.unwrap() {
                break v;
            }
            if std::time::Instant::now() > deadline {
                panic!("inflight 标记未落库");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    let parsed: serde_json::Value = serde_json::from_str(&inflight).unwrap();
    assert_eq!(
        parsed["prompt"].as_str(),
        Some("继续整理"),
        "inflight 应存注入前 base prompt：{inflight}"
    );
    // 正常收尾（/stop 中断，摘要按 P1-K 保留）清除本轮 inflight；再把捕获的
    // base 版 payload 写回，模拟「轮首落库后进程崩溃」的残留行。
    ctx.disp.handle(msg("c5", "alice", "/stop")).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), runner).await;
    store
        .set_config("inflight_prompt:c5", &inflight)
        .await
        .unwrap();
    ctx.disp.recover_crashed_rounds().await;
    // /retry 重放：慢轮被中断未落 session（existing=None）→ 摘要恰注入一次。
    let disp = ctx.disp.clone();
    let runner2 = tokio::spawn(async move { disp.handle(msg("c5", "alice", "/retry")).await });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while ctx.prompts.lock().await.len() < 2 {
        if std::time::Instant::now() > deadline {
            panic!("/retry 未重放: {:?}", ctx.prompts.lock().await);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let replayed = ctx.prompts.lock().await[1].clone();
    assert_eq!(
        replayed.matches("【前情摘要】").count(),
        1,
        "重放轮摘要应恰注入一次：{replayed}"
    );
    assert!(replayed.ends_with("继续整理"), "base 应在末尾：{replayed}");
    // 收尾：中断重放轮，防 60s 慢后端拖住测试。
    ctx.disp.handle(msg("c5", "alice", "/stop")).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), runner2).await;
    drop_db(ctx.db).await;
}

/// v1.23 指令复用：成功轮 prompt 落 last_success_prompt → /again 重跑
///（与 /retry 的失败轮分键互不干扰）。
#[tokio::test]
async fn again_replays_last_success_prompt() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 一轮成功任务（MockBackend 默认成功终态）。
    ctx.disp.handle(msg("c1", "alice", "生成日报")).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !ctx.prompts.lock().await.is_empty() {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("首轮未执行");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // 等轮次收尾落库（handle 直跑完含收尾）。
    let store = ctx.check().await;
    assert!(
        store
            .get_config("last_success_prompt:c1")
            .await
            .unwrap()
            .is_some(),
        "成功轮应落 last_success_prompt"
    );
    // /again 重跑。
    ctx.disp.handle(msg("c1", "alice", "/again")).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if ctx.prompts.lock().await.len() >= 2 {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("/again 未重跑: {:?}", ctx.prompts.lock().await);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        ctx.prompts.lock().await[1],
        "生成日报",
        "/again 应重放成功轮 prompt"
    );
    drop_db(ctx.db).await;
}

/// v1.23 allow-set 可见性：always 落地 → /perm list 可见 → /perm revoke
/// 单项撤销 → 再次 list 为空。
#[tokio::test]
async fn perm_list_and_revoke_session_allows() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 模拟用户在审批卡点了「始终允许」。
    ctx.disp.router.allow_always("c1", "Bash").await;
    ctx.disp.router.allow_always("c1", "WebFetch").await;
    ctx.disp.handle(msg("c1", "alice", "/perm list")).await;
    let inbox = ctx.inbox.lock().await.clone();
    let listing = inbox.last().unwrap();
    assert!(
        listing.contains("Bash") && listing.contains("WebFetch"),
        "{listing}"
    );
    assert!(listing.contains("2 个工具"), "{listing}");
    // 撤销一项。
    ctx.disp
        .handle(msg("c1", "alice", "/perm revoke Bash"))
        .await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.last().unwrap().contains("已撤销"),
        "{}",
        inbox.last().unwrap()
    );
    // 再 list：只剩 WebFetch。
    ctx.disp.handle(msg("c1", "alice", "/perm list")).await;
    let inbox = ctx.inbox.lock().await.clone();
    let listing = inbox.last().unwrap();
    assert!(
        listing.contains("WebFetch") && !listing.contains("\n- Bash"),
        "{listing}"
    );
    // 撤销不存在的条目：明确回执。
    ctx.disp
        .handle(msg("c1", "alice", "/perm revoke Bash"))
        .await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.last().unwrap().contains("不在本会话"),
        "{}",
        inbox.last().unwrap()
    );
    drop_db(ctx.db).await;
}

/// v13 P3-7（安全收紧）：/perm list 与 /perm revoke 加 admin 门槛——「始终
/// 允许」清单是会话级持续授权面，查看/撤销与 /config 同门槛；非 admin 回
/// 明确拒绝且授权不被撤销。/perm 本体（模式热切）门槛不变（上方
/// perm_switch_requires_admin 覆盖）。
#[tokio::test]
async fn perm_list_and_revoke_require_admin() {
    let _serial = SERIAL.lock().await;
    // bob 白名单但非 admin（admin 只有 alice）。
    let ctx = build_with_admin(
        Auth::new(vec!["alice".into(), "bob".into()]),
        vec!["alice".into()],
    )
    .await;
    ctx.disp.router.allow_always("c1", "Bash").await;
    ctx.disp.handle(msg("c1", "bob", "/perm list")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.last().unwrap().contains("仅管理员"),
        "非 admin /perm list 应被拒：{}",
        inbox.last().unwrap()
    );
    ctx.disp.handle(msg("c1", "bob", "/perm revoke Bash")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.last().unwrap().contains("仅管理员"),
        "非 admin /perm revoke 应被拒：{}",
        inbox.last().unwrap()
    );
    // 拒绝不产生副作用：授权仍在。
    assert!(
        ctx.disp.router.is_session_allowed("c1", "Bash").await,
        "被拒的 revoke 不得撤销授权"
    );
    // admin 照常可用（可见 + 可撤销）。
    ctx.disp.handle(msg("c1", "alice", "/perm list")).await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.last().unwrap().contains("Bash"),
        "admin /perm list 应放行：{}",
        inbox.last().unwrap()
    );
    ctx.disp
        .handle(msg("c1", "alice", "/perm revoke Bash"))
        .await;
    let inbox = ctx.inbox.lock().await.clone();
    assert!(
        inbox.last().unwrap().contains("已撤销"),
        "admin /perm revoke 应放行：{}",
        inbox.last().unwrap()
    );
    drop_db(ctx.db).await;
}

/// v1.20 窗口自学习：ACP 报告窗口 → 比例档阈值重算（默认 1M×0.8=800k，
/// 学习 200k → 160k）；窗口未变 no-op。
#[tokio::test]
async fn learned_context_window_recalibrates_threshold() {
    let _serial = SERIAL.lock().await;
    // 测试基建默认关闭自动压缩——显式构造比例档预算（1M×0.8）。
    let ctx = build_slow(
        Auth::new(vec!["alice".into()]),
        0,
        TaskBudgets {
            auto_compact_threshold_tokens: 800_000,
            auto_compact_window_tokens: 1_000_000,
            auto_compact_window_ratio: 0.8,
            cron_catchup: crate::config::CronCatchup::One,
            sender_daily_cost_limit_usd: None,
            agent_timeout: Duration::ZERO,
            permission_ask_timeout: Duration::from_secs(5),
            ask_via_im_timeout: Duration::from_secs(5),
            shutdown_grace: Duration::from_secs(5),
            agent_idle_timeout: Duration::ZERO,
            batch_window: Duration::ZERO,
            max_concurrent_rounds: 0,
        },
    )
    .await;
    let t0 = ctx
        .disp
        .auto_compact_threshold
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(t0, 800_000, "比例档 1M×0.8");
    ctx.disp.note_learned_context_window(200_000);
    let t1 = ctx
        .disp
        .auto_compact_threshold
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(t1, 160_000, "学习 200k → 200k×0.8");
    // 重复同值 no-op（无重算副作用可从窗口不变验证）。
    ctx.disp.note_learned_context_window(200_000);
    let w = ctx
        .disp
        .auto_compact_window
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(w, 200_000);
    // v1.21 护栏：区间外的学习值丢弃（阈值不变）。
    ctx.disp.note_learned_context_window(1);
    ctx.disp.note_learned_context_window(999_999_999);
    let t2 = ctx
        .disp
        .auto_compact_threshold
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(t2, 160_000, "异常窗口（过小/过大）应被丢弃");
    drop_db(ctx.db).await;
}

/// v1.21 review（P1）：cron/webhook 合成消息（no_steer）不被 steering 劫持——
/// 运行中到达时排队为独立轮次（可审计/持久化），不注入当轮 stdin。
#[tokio::test]
async fn no_steer_message_queues_instead_of_steering() {
    let _serial = SERIAL.lock().await;
    let _ = std::fs::create_dir_all("/tmp/imagent-test-ws");
    let (plat, inbox, send_count) = MockPlatform::new();
    let (mut back, calls, prompts, order) = MockBackend::new_slow(300);
    back.steerable = true;
    let steer_seen = back.steer_seen.clone();
    let (store, db) = tmp_store().await;
    let auth = Auth::new(vec!["alice".into()]);
    let admins = auth.snapshot();
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        auth,
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into()],
        PermissionMode::Off,
        TaskBudgets {
            batch_window: Duration::from_millis(1),
            ..test_budgets()
        },
        CotDetail::Brief,
        admins,
    ));
    let ctx = Ctx {
        disp: disp.clone(),
        inbox,
        send_count,
        calls,
        prompts: prompts.clone(),
        order,
        db: db.clone(),
    };
    let d = disp.clone();
    let runner = tokio::spawn(async move {
        d.handle(msg("c1", "alice", "round A")).await;
    });
    assert!(wait_registered(&ctx, "c1").await, "在飞任务应已注册");
    // 合成消息（no_steer=true）：即使后端支持 steering 也应排队。
    // sender 用已授权的 alice（鉴权门与 steering 正交，本测试只验证路由层）。
    let mut cron_msg = msg("c1", "alice", "【ci】deploy failed");
    cron_msg.no_steer = true;
    ctx.disp.handle(cron_msg).await;
    let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
    assert!(done.is_ok(), "runner 应结束");
    // 排队消息随后作为独立轮次执行（prompts 两条），且从未进 steer 通道。
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if ctx.prompts.lock().await.len() >= 2 {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("合成消息未被排队执行");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let prompts_now = ctx.prompts.lock().await.clone();
    assert_eq!(
        prompts_now,
        vec!["round A".to_string(), "【ci】deploy failed".to_string()],
        "no_steer 消息应作为独立轮次执行"
    );
    assert!(
        steer_seen.lock().await.is_empty(),
        "no_steer 消息不应注入当轮 stdin"
    );
    drop_db(ctx.db).await;
}

/// v1.21 review（P1）：/stop 在批窗口期拦截批次后，队列不能死锁——后续消息
/// 仍应正常取批执行。回归锚：旧实现 break 跳过空 entry 收尾，本 conv 所有
/// 后续消息永久滞留。
#[tokio::test]
async fn stop_interception_does_not_strand_queue() {
    let _serial = SERIAL.lock().await;
    let ctx = build_slow(
        Auth::new(vec!["alice".into()]),
        150,
        TaskBudgets {
            batch_window: Duration::from_millis(50),
            ..test_budgets()
        },
    )
    .await;
    let disp = ctx.disp.clone();
    // 预设停止标记（模拟 /stop 恰在批窗口期到达：标记设置时 running 尚未注册）。
    // （T18 机械调整：stop_requested 并入 ConvState 单表。）
    disp.with_conv("c1", |cs| {
        cs.stop_requested = Some(crate::dispatch::now_secs())
    })
    .await;
    // 第一条消息成为 runner，取批后命中停止标记 → 批次丢弃。
    let runner = tokio::spawn(async move {
        disp.handle(msg("c1", "alice", "被拦截的批次")).await;
    });
    let done = tokio::time::timeout(Duration::from_secs(5), runner).await;
    assert!(done.is_ok(), "runner 应结束（拦截路径）");
    // 关键回归点：/stop 之后的新消息必须还能被处理（旧实现死队列）。
    let disp2 = ctx.disp.clone();
    let runner2 = tokio::spawn(async move {
        disp2.handle(msg("c1", "alice", "stop 之后的新消息")).await;
    });
    let done2 = tokio::time::timeout(Duration::from_secs(5), runner2).await;
    assert!(done2.is_ok(), "后续消息的 runner 应正常完成");
    let prompts = ctx.prompts.lock().await.clone();
    assert!(
        prompts.contains(&"stop 之后的新消息".to_string()),
        "新消息应被执行（死队列回归）：{prompts:?}"
    );
    drop_db(ctx.db).await;
}

/// v1.20 webhook 注入：inject() → handle() 完整管线（会话白名单门）→ 驱动 agent。
#[tokio::test]
async fn webhook_inject_drives_agent_via_handle() {
    let _serial = SERIAL.lock().await;
    // conv 白名单放行 c1（Auth::with_chats）。
    let auth = Auth::with_chats(vec![], vec!["c1".into()]);
    let ctx = build(auth).await;
    let msg = msg("c1", "webhook:ci", "【ci】deploy failed on main");
    // v1.21：inject 返回 Result（停机拒绝）；测试态未 shutdown，恒 Ok。
    ctx.disp.inject(msg).await.expect("inject 应成功（未停机）");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if ctx
            .prompts
            .lock()
            .await
            .iter()
            .any(|p| p.contains("deploy failed on main"))
        {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("webhook 注入未驱动 agent: {:?}", ctx.prompts.lock().await);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    drop_db(ctx.db).await;
}

/// v1.18 迭代（排队持久化）：启动重放——崩溃前落库的排队消息经
/// replay_persisted_queue 重新驱动 agent；重放行先清表防双份。
#[tokio::test]
async fn queued_messages_replay_after_restart() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 模拟崩溃前落库的排队消息（直接走 store，绕过内存入队）。
    let m = msg("c1", "alice", "崩溃前的任务");
    let json = serde_json::to_string(&m).unwrap();
    ctx.disp
        .store
        .persist_queued_msg("c1", m.source_msg_id.as_deref(), &json)
        .await
        .unwrap();
    ctx.disp.replay_persisted_queue().await;
    // 重放消息应驱动 MockBackend（轮询——handle 经 tasks spawn）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if ctx
            .prompts
            .lock()
            .await
            .iter()
            .any(|p| p.contains("崩溃前的任务"))
        {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("重放消息未驱动 agent: {:?}", ctx.prompts.lock().await);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // 清表生效：重放后表空。
    assert!(
        ctx.disp.store.load_queued_all().await.unwrap().is_empty(),
        "重放后持久化表应清空"
    );
    drop_db(ctx.db).await;
}

/// v13 P3（媒体 GC 与排队重放错位）：排队行引用的媒体路径已被 7 天 GC 删除
/// ——重放取批构造 prompt 时注入过期占位而非死路径（fail-soft 不阻断轮次，
/// agent 知道该让用户重发）。
#[tokio::test]
async fn queued_media_replay_expired_path_degrades() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 模拟崩溃前落库的排队消息：媒体指向已不存在的本地路径。
    let mut m = msg("c1", "alice", "看下这张图");
    m.media.push(MediaRef {
        kind: "image".to_string(),
        url: "/nonexistent/imagent/media/img_gone.png".to_string(),
    });
    let json = serde_json::to_string(&m).unwrap();
    ctx.disp
        .store
        .persist_queued_msg("c1", m.source_msg_id.as_deref(), &json)
        .await
        .unwrap();
    ctx.disp.replay_persisted_queue().await;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if ctx
            .prompts
            .lock()
            .await
            .iter()
            .any(|p| p.contains("看下这张图"))
        {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("重放消息未驱动 agent: {:?}", ctx.prompts.lock().await);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let prompt = ctx.prompts.lock().await[0].clone();
    assert!(prompt.contains("【用户发来媒体】"), "{prompt}");
    assert!(
        prompt.contains("（该媒体已过期自动清理，请让用户重发）"),
        "缺失媒体应注入过期占位：{prompt}"
    );
    assert!(
        !prompt.contains("/nonexistent/imagent/media/img_gone.png"),
        "死路径不应进 prompt：{prompt}"
    );
    drop_db(ctx.db).await;
}

/// v13 P3：media_hint_for 的存在性矩阵——文件在则路径照发；缺失替换为过期
/// 占位；下载失败项照旧列出；全空无提示。
#[test]
fn media_hint_checks_file_existence() {
    let dir = std::env::temp_dir().join(format!("imagent-media-hint-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let live = dir.join("live.png");
    std::fs::write(&live, b"png").unwrap();
    let media = vec![
        MediaRef {
            kind: "image".to_string(),
            url: live.to_string_lossy().to_string(),
        },
        MediaRef {
            kind: "file".to_string(),
            url: dir.join("gone.pdf").to_string_lossy().to_string(),
        },
    ];
    let hint = media_hint_for(&media, &[]);
    assert!(hint.contains("live.png"), "{hint}");
    assert!(
        hint.contains("（该媒体已过期自动清理，请让用户重发）"),
        "{hint}"
    );
    assert!(!hint.contains("gone.pdf"), "{hint}");
    // 下载失败项与全空行为不变。
    let hint2 = media_hint_for(&[], &["img_x: 下载失败".to_string()]);
    assert!(hint2.contains("该媒体获取失败"), "{hint2}");
    assert_eq!(media_hint_for(&[], &[]), "");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn cron_add_list_fire_rm() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 非法表达式（分钟 99）→ 明确回执，不落库。
    ctx.disp
        .handle(msg("c1", "alice", "/cron add 99 * * * * 坏表达式"))
        .await;
    assert!(
        ctx.inbox
            .lock()
            .await
            .iter()
            .any(|t| t.contains("表达式非法")),
        "非法表达式回执: {:?}",
        ctx.inbox.lock().await
    );
    assert!(
        ctx.disp.store.list_cron_jobs().await.unwrap().is_empty(),
        "非法表达式不应落库"
    );
    // 合法表达式（每分钟）→ 落库 + 创建回执。
    ctx.disp
        .handle(msg("c1", "alice", "/cron add * * * * * 报数"))
        .await;
    assert!(
        ctx.inbox
            .lock()
            .await
            .iter()
            .any(|t| t.contains("定时任务已创建")),
        "创建回执: {:?}",
        ctx.inbox.lock().await
    );
    let jobs = ctx.disp.store.list_cron_jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].conv, "c1");
    assert_eq!(jobs[0].sender, "alice");
    assert_eq!(jobs[0].prompt, "报数");
    // list → 本会话任务可见。
    ctx.disp.handle(msg("c1", "alice", "/cron list")).await;
    assert!(
        ctx.inbox
            .lock()
            .await
            .iter()
            .any(|t| t.contains("报数") && t.contains("⏰")),
        "list 回执: {:?}",
        ctx.inbox.lock().await
    );
    // 到期驱动：把 next_run 拨到过去 → fire_due 合成消息走 handle（一轮执行）。
    ctx.disp
        .store
        .bump_cron_job(&jobs[0].id, 0, 1)
        .await
        .unwrap();
    ctx.disp.fire_due_cron_jobs().await;
    // v1.18 review：fire 分发异步化（spawn 进 tasks——调度器内联 await 整轮
    // agent 会全局队头阻塞 + 关停失聪），注入经 task 异步到达，轮询等待。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if ctx
            .prompts
            .lock()
            .await
            .iter()
            .any(|p| p.contains("定时任务触发") && p.contains("报数"))
        {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("到期注入 prompt: {:?}", ctx.prompts.lock().await);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // fire 后已重排（next_run 推进到未来，不再 due）。
    let j = ctx
        .disp
        .store
        .get_cron_job(&jobs[0].id)
        .await
        .unwrap()
        .expect("任务仍在");
    assert!(j.next_run > 1, "重排后 next_run 应在未来: {}", j.next_run);
    // 他人不可删（非 admin 场景：build 把 alice 设为 admin，这里用第三者 bob
    // 未在白名单——handle 直接丢弃，改用 creator 本人删）。
    ctx.disp
        .handle(msg("c1", "alice", &format!("/cron rm {}", jobs[0].id)))
        .await;
    assert!(
        ctx.inbox
            .lock()
            .await
            .iter()
            .any(|t| t.contains("已删除定时任务")),
        "删除回执: {:?}",
        ctx.inbox.lock().await
    );
    assert!(ctx
        .disp
        .store
        .get_cron_job(&jobs[0].id)
        .await
        .unwrap()
        .is_none());
    drop_db(ctx.db).await;
}

/// v13 P3（cron drain 竞态，v11#10 webhook inject 同族）：drain 持 tasks 锁
/// 期间 fire 阻塞在 lock()——drain 结束后再 spawn 的 handle 无人 join、随
/// runtime 退出被无声取消（触发丢失）。修复：拿锁后复查 shutdown，已停机
/// 则不再 spawn。
#[tokio::test]
async fn cron_fire_race_with_drain_skips_spawn() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    ctx.disp
        .handle(msg("c1", "alice", "/cron add * * * * * 报数"))
        .await;
    let jobs = ctx.disp.store.list_cron_jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    ctx.disp
        .store
        .bump_cron_job(&jobs[0].id, 0, 1)
        .await
        .unwrap();
    // 模拟 drain 进行中：占住 tasks 锁，fire 将阻塞在 lock()（store 查询/重排
    // 均在锁外完成，150ms 足以到达锁点）。
    let guard = ctx.disp.tasks.lock().await;
    let disp = ctx.disp.clone();
    let fire = tokio::spawn(async move { disp.fire_due_cron_jobs().await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    // fire 挂锁期间 shutdown 开始（= drain 已在进行的语义），随后「drain 结束」。
    ctx.disp.shutdown();
    drop(guard);
    let _ = tokio::time::timeout(Duration::from_secs(5), fire).await;
    // 拿锁后复查拒绝 spawn：backend 未收到任何注入 prompt。
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        ctx.prompts.lock().await.is_empty(),
        "停机竞态窗口内不应 spawn：{:?}",
        ctx.prompts.lock().await
    );
    drop_db(ctx.db).await;
}

/// v13 P3 对照：shutdown 已开始时 fire 直接整轮返回——不 bump（错过槽保留
/// 在 next_run，下次启动按停机补跑语义处理、不丢该槽）也不触发。
#[tokio::test]
async fn cron_fire_after_shutdown_returns_without_bump() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    ctx.disp
        .handle(msg("c1", "alice", "/cron add * * * * * 报数"))
        .await;
    let jobs = ctx.disp.store.list_cron_jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    ctx.disp
        .store
        .bump_cron_job(&jobs[0].id, 0, 1)
        .await
        .unwrap();
    ctx.disp.shutdown();
    ctx.disp.fire_due_cron_jobs().await;
    assert!(
        ctx.prompts.lock().await.is_empty(),
        "停机后 fire 不应触发: {:?}",
        ctx.prompts.lock().await
    );
    let j = ctx
        .disp
        .store
        .get_cron_job(&jobs[0].id)
        .await
        .unwrap()
        .expect("任务仍在");
    assert!(
        j.next_run <= 1,
        "不应重排（错过槽保留给下次启动补跑）: {}",
        j.next_run
    );
    drop_db(ctx.db).await;
}

/// v1.18 footer：steered 段与排队段并存/单独展示；全零不展示。
#[test]
fn queued_hint_display_with_steered() {
    use crate::card_session::{queued_hint_display, QueuedHint};
    let s = QueuedHint {
        steered: 2,
        ..Default::default()
    };
    assert_eq!(
        queued_hint_display(&s).as_deref(),
        Some("📥 已注入 2 条运行中消息")
    );
    let both = QueuedHint {
        steered: 1,
        count: 3,
        latest: "看这张图".into(),
    };
    assert_eq!(
        queued_hint_display(&both).as_deref(),
        Some("📥 已注入 1 条运行中消息，排队 3 条，最新：「看这张图」")
    );
}

// ---------- P2（code-review v13）调度批：recv 解阻塞 / 并发护栏 / 发起者锚定 / ask 取消 ----------

/// 与 build_with_parts 相同但允许指定 budgets 与 permission_mode（v13 批测试用）。
async fn build_with_parts_full(
    auth: Auth,
    plat: MockPlatform,
    back: MockBackend,
    budgets: TaskBudgets,
    mode: PermissionMode,
) -> Ctx {
    let _ = std::fs::create_dir_all("/tmp/imagent-test-ws");
    let (inbox, send_count) = (plat_inbox_of(&plat), plat_count_of(&plat));
    let (store, db) = tmp_store().await;
    let admins = auth.snapshot();
    let disp = Arc::new(Dispatcher::new(
        Arc::new(plat),
        Arc::new(back),
        store,
        auth,
        std::path::PathBuf::from("/tmp/imagent-test-ws"),
        vec!["Read".into(), "Edit".into()],
        mode,
        budgets,
        CotDetail::Brief,
        admins,
    ));
    Ctx {
        disp,
        inbox,
        send_count,
        calls: Default::default(),
        prompts: Default::default(),
        order: Default::default(),
        db,
    }
}

/// 等待谓词成立（5s 上限；5ms 轮询）。谓词是「借 ctx 返回 future」的闭包
///（Box::pin 消 async 块对借用生命周期的推断歧义）。
async fn wait_until<F>(ctx: &Ctx, f: F) -> bool
where
    F: Fn(&Ctx) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + '_>>,
{
    for _ in 0..1000 {
        if f(ctx).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    f(ctx).await
}

/// P2（v13）①：审批决策后的平台收敛（resolve_permission_ask）挂起时，recv 主循环
/// 不得被卡死——后续消息照常处理。回归点：route 命中分支的 resolve/审计/提示
/// 此前内联 await，飞书 429/token 刷新（30s+）期间所有 conv 的入站消息停摆。
#[tokio::test]
async fn recv_loop_survives_slow_ask_resolution() {
    let _serial = SERIAL.lock().await;
    let (plat, inbox, _count, recv_queue, resolve_gate) = MockPlatform::new_slow_resolve();
    let (back, _calls, _prompts, _order) = MockBackend::new();
    let mut ctx = build_with_parts_full(
        Auth::new(vec!["alice".into()]),
        plat,
        back,
        test_budgets(),
        PermissionMode::Allow,
    )
    .await;
    ctx.inbox = inbox.clone();

    // 预挂一个带发起者的 pending；recv 队列：先 "y"（决策回复），后普通消息。
    let rx = ctx
        .disp
        .router()
        .register(
            "c1",
            "r-1",
            None,
            crate::permission::PendingKind::Permission,
            Some("Bash"),
            Some("alice"),
        )
        .await;
    *recv_queue.lock().await = Some(vec![
        msg("c1", "alice", "y"),
        msg("c1", "alice", "second message"),
    ]);

    let run_handle = {
        let disp = ctx.disp.clone();
        tokio::spawn(async move {
            let _ = disp.run().await;
        })
    };

    // ① resolve 已进入平台调用（决策已 route，spawn 的收敛任务在跑且挂起）。
    assert!(
        wait_until(&ctx, |c| {
            Box::pin(async move { c.inbox.lock().await.iter().any(|t| t == "[resolve-enter]") })
        })
        .await,
        "resolve 任务应已进入（决策已送达）"
    );
    // ② 闸门不放行的前提下，后续消息仍被处理（backend 跑完并回复）——
    //    旧实现里 resolve 内联 await 会把 recv 循环钉死在这里。
    assert!(
        wait_until(&ctx, |c| {
            Box::pin(async move { c.inbox.lock().await.iter().any(|t| t.starts_with("reply#")) })
        })
        .await,
        "慢 resolve 不得阻塞 recv 循环处理后续消息: {:?}",
        ctx.inbox.lock().await
    );
    // ③ 决策本体已送达等待者（allow——Allow 档无 R4 强制 deny）。
    let decision = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("决策应立即送达")
        .expect("sender 未 drop");
    assert!(decision.allow, "alice 本人回复 y 应放行");

    // 收尾：放行闸门 + shutdown，drain 不悬挂。
    resolve_gate.notify_waiters();
    ctx.disp.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
    drop_db(ctx.db).await;
}

/// P2（v13）②：全局并发护栏——上限 1 时两个 conv 的轮次串行（第二个等
/// permit 到第一个整轮结束）；上限 0（不限制）时并行。gated backend 精确
/// 控制「在飞」窗口。
#[tokio::test]
async fn max_concurrent_rounds_gates_cross_conv_parallelism() {
    let _serial = SERIAL.lock().await;
    let auth = Auth::new(vec!["alice".into()]);

    // ---- 上限 1：串行。----
    let (plat, _pi, _pc) = MockPlatform::new();
    let (back, calls, _prompts, _order, gate) = MockBackend::new_gated();
    let mut budgets = test_budgets();
    budgets.max_concurrent_rounds = 1;
    let mut ctx =
        build_with_parts_full(auth.clone(), plat, back, budgets, PermissionMode::Off).await;
    ctx.calls = calls.clone();

    let d1 = ctx.disp.clone();
    let h1 = tokio::spawn(async move { d1.handle(msg("c1", "alice", "task one")).await });
    let d2 = ctx.disp.clone();
    let h2 = tokio::spawn(async move { d2.handle(msg("c2", "alice", "task two")).await });

    // 第一个轮次起跑（调用已记录）。
    assert!(
        wait_until(&ctx, |c| Box::pin(async move {
            !c.calls.lock().await.is_empty()
        }))
        .await,
        "第一个 conv 的轮次应起跑"
    );
    // 闸门不放行期间，第二个 conv 必须还在等 permit（不得起跑）。
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        ctx.calls.lock().await.len(),
        1,
        "上限 1：第二轮次应等 permit，不得并行起跑"
    );
    // 放行第一轮 → permit 释放 → 第二轮起跑。
    gate.notify_one();
    assert!(
        wait_until(&ctx, |c| Box::pin(async move {
            c.calls.lock().await.len() >= 2
        }))
        .await,
        "第一轮完成后第二轮应立即起跑"
    );
    gate.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(5), h1).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), h2).await;
    // 在飞 gauge 归零（permit guard Drop 收口）。
    assert_eq!(
        crate::metrics::METRICS.running_rounds.get(),
        0,
        "全部轮次结束后在飞 gauge 应归零"
    );
    drop_db(ctx.db).await;

    // ---- 上限 0：并行不受限。----
    let (plat0, _pi0, _pc0) = MockPlatform::new();
    let (back0, calls0, _prompts0, _order0, gate0) = MockBackend::new_gated();
    let mut budgets0 = test_budgets();
    budgets0.max_concurrent_rounds = 0;
    let mut ctx0 = build_with_parts_full(auth, plat0, back0, budgets0, PermissionMode::Off).await;
    ctx0.calls = calls0.clone();
    let d1 = ctx0.disp.clone();
    let h1 = tokio::spawn(async move { d1.handle(msg("c1", "alice", "task one")).await });
    let d2 = ctx0.disp.clone();
    let h2 = tokio::spawn(async move { d2.handle(msg("c2", "alice", "task two")).await });
    // 闸门完全不放行，两个轮次都应已在飞。
    assert!(
        wait_until(&ctx0, |c| Box::pin(async move {
            c.calls.lock().await.len() >= 2
        }))
        .await,
        "上限 0 = 不限制：两轮应并行起跑"
    );
    gate0.notify_one();
    gate0.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(5), h1).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), h2).await;
    drop_db(ctx0.db).await;
}

/// P2（v13）②：热改重建信号量——上限 1 → 4 后，**后续 acquire** 走新闸立即
/// 放行；已在旧信号量上排队的等待者不受热改影响（仍按旧闸顺序）。notify_one
/// 的 stored permit 语义使放行确定性成立（notify_waiters 对未注册等待者丢唤醒）。
#[tokio::test]
async fn max_concurrent_rounds_hot_reload_rebuilds_gate() {
    let _serial = SERIAL.lock().await;
    let auth = Auth::new(vec!["alice".into()]);
    let (plat, _pi, _pc) = MockPlatform::new();
    let (back, calls, _prompts, _order, gate) = MockBackend::new_gated();
    let mut budgets = test_budgets();
    budgets.max_concurrent_rounds = 1;
    let mut ctx = build_with_parts_full(auth, plat, back, budgets, PermissionMode::Off).await;
    ctx.calls = calls;

    // A：占住旧闸唯一的 permit，在完成闸门挂起。
    let d1 = ctx.disp.clone();
    let h1 = tokio::spawn(async move { d1.handle(msg("c1", "alice", "one")).await });
    assert!(
        wait_until(&ctx, |c| Box::pin(async move {
            !c.calls.lock().await.is_empty()
        }))
        .await,
        "A 应起跑（旧闸唯一 permit）"
    );
    tokio::time::sleep(Duration::from_millis(100)).await; // A 注册到完成闸门。
                                                          // B：热改**前**起跑 → snapshot 旧信号量 → 在旧闸排队。
    let d2 = ctx.disp.clone();
    let h2 = tokio::spawn(async move { d2.handle(msg("c2", "alice", "two")).await });
    tokio::time::sleep(Duration::from_millis(100)).await; // 确保 B 已在旧 sem 等待。
    assert_eq!(ctx.calls.lock().await.len(), 1, "B 应在旧闸排队，不得起跑");
    // 热改放宽到 4：C 的新 acquire 走新闸，不等 A/B。
    ctx.disp.reload_max_concurrent_rounds(4);
    let d3 = ctx.disp.clone();
    let h3 = tokio::spawn(async move { d3.handle(msg("c3", "alice", "three")).await });
    assert!(
        wait_until(&ctx, |c| Box::pin(async move {
            c.calls.lock().await.len() >= 2
        }))
        .await,
        "热改后新起跑的 C 应立即经新闸拿到 permit"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        ctx.calls.lock().await.len(),
        2,
        "B 仍在旧闸排队（热改不影响已排队等待者），在飞 = A + C"
    );
    // 放行 A → 旧 permit 释放 → B 按旧闸起跑。
    gate.notify_one();
    assert!(
        wait_until(&ctx, |c| Box::pin(async move {
            c.calls.lock().await.len() >= 3
        }))
        .await,
        "A 完成后 B 应经旧闸放行"
    );
    gate.notify_one(); // 放行 B。
    gate.notify_one(); // 放行 C。
    let _ = tokio::time::timeout(Duration::from_secs(5), h1).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), h2).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), h3).await;
    drop_db(ctx.db).await;
}

/// P2（v13）③：审批发起者锚定贯通文本路径——群 conv 里 A 发起的审批，B 打
/// "y" 不得消费（pending 保留 + 明确提示 + 消息不吞继续走 agent 管线）；A 打
/// "y" 正常放行。经 run() recv 循环全链路验证。admin 代批另在 helper 级验证
/// （见 permission_initiator_block_admin_bypass）。
#[tokio::test]
async fn permission_initiator_anchor_gates_text_replies() {
    let _serial = SERIAL.lock().await;
    let (plat, inbox, _count, recv_queue) = {
        let (p, i, c) = MockPlatform::new();
        let q = p.recv_queue.clone();
        (p, i, c, q)
    };
    let (back, _calls, prompts, _order) = MockBackend::new();
    let mut ctx = build_with_parts_full(
        Auth::new(vec!["alice".into(), "bob".into()]),
        plat,
        back,
        test_budgets(),
        PermissionMode::Allow,
    )
    .await;
    // 测试基建默认把白名单全员设为 admin——本用例要求 bob 只是白名单成员
    //（非 admin 不可代批），收窄 admin 名单到 alice。
    ctx.disp.reload_admins(vec!["alice".into()]);
    ctx.inbox = inbox.clone();
    ctx.prompts = prompts.clone();

    // A（alice）发起的审批 pending。
    let rx = ctx
        .disp
        .router()
        .register(
            "c-group",
            "r-1",
            None,
            crate::permission::PendingKind::Permission,
            Some("Bash"),
            Some("alice"),
        )
        .await;
    // B 先答（不得消费）——先只投 B 的消息，断言完拦截形态再投 A 的
    //（两条背靠背到达时 pending 保留断言会有「A 已消费」竞态）。
    *recv_queue.lock().await = Some(vec![msg("c-group", "bob", "y")]);
    let run_handle = {
        let disp = ctx.disp.clone();
        tokio::spawn(async move {
            let _ = disp.run().await;
        })
    };

    // B 的 "y"：被锚定拦截——提示出现、pending 保留、消息本身继续走 agent
    //（不吞：backend 收到 prompt "y"）。
    assert!(
        wait_until(&ctx, |c| {
            Box::pin(async move {
                c.inbox
                    .lock()
                    .await
                    .iter()
                    .any(|t| t.contains("该询问由 alice 发起"))
            })
        })
        .await,
        "B 的回复应收到发起者锚定提示: {:?}",
        ctx.inbox.lock().await
    );
    assert!(
        wait_until(&ctx, |c| {
            Box::pin(async move { c.prompts.lock().await.iter().any(|p| p == "y") })
        })
        .await,
        "被拦截的消息不得被吞——应继续走 agent 管线: {:?}",
        ctx.prompts.lock().await
    );
    assert!(
        ctx.disp.router().has_pending("c-group").await,
        "拦截不得消费 pending（留给发起者）"
    );

    // A 的 "y"：正常放行。
    recv_queue
        .lock()
        .await
        .as_mut()
        .expect("recv 队列仍在")
        .push(msg("c-group", "alice", "y"));
    let decision = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("A 的回复应在合理时间内消费")
        .expect("sender 未 drop");
    assert!(decision.allow, "发起者本人回复 y 应放行");

    ctx.disp.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
    drop_db(ctx.db).await;
}

/// P2（v13）③：admin 代批豁免——pending 锚定发起者 alice，非 admin 的 bob 被
/// 拦、admin 的 bob 放行（helper 级；recv 循环行为由上一用例覆盖）。
#[tokio::test]
async fn permission_initiator_block_admin_bypass() {
    let _serial = SERIAL.lock().await;
    // alice/bob 白名单；仅 bob 是 admin。
    let ctx = build_with_admin(
        Auth::new(vec!["alice".into(), "bob".into()]),
        vec!["bob".into()],
    )
    .await;
    let _rx = ctx
        .disp
        .router()
        .register(
            "c-group",
            "r-1",
            None,
            crate::permission::PendingKind::Permission,
            Some("Bash"),
            Some("alice"),
        )
        .await;
    // bob 是 admin：放行（None）。
    assert!(
        ctx.disp
            .permission_initiator_block("c-group", &msg("c-group", "bob", "y"))
            .await
            .is_none(),
        "admin 可代批发起者锚定的询问"
    );
    // carol 非白名单也非 admin：锚定判定本身仍命中（消费门 can_route 另拦）。
    let _rx2 = ctx
        .disp
        .router()
        .register(
            "c-group",
            "r-2",
            None,
            crate::permission::PendingKind::Permission,
            Some("Bash"),
            Some("alice"),
        )
        .await;
    assert_eq!(
        ctx.disp
            .permission_initiator_block("c-group", &msg("c-group", "carol", "y"))
            .await,
        Some("alice".to_string()),
        "非 admin 仍被锚定拦截"
    );
    drop_db(ctx.db).await;
}

/// P2（v13）④：cancel_all/单条 cancel 对 ask 终端路径回 **Err**（提问被取消），
/// 不再把「cancelled（任务被 /stop 中断…）」当用户回答文本回写；permission
/// 路径语义不变（= deny）。
#[cfg(unix)]
#[tokio::test]
async fn cancelled_ask_returns_error_to_terminal() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let _serial = SERIAL.lock().await;
    let ctx = build_with_mode(Auth::new(vec!["alice".into()]), PermissionMode::Ask).await;
    let dir = std::env::temp_dir().join(format!("imagent-sock-cancel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("permission.sock");
    ctx.disp
        .spawn_socket_accept(sock.to_string_lossy().into_owned());
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

    // ask 分支：提问送达后 cancel（模拟 /stop 的 cancel_all）→ 终端收到
    // {"kind":"ask","text":null,"error":"cancelled…"}，而非把文案当答案。
    {
        let mut s = tokio::net::UnixStream::connect(&sock).await.unwrap();
        s.write_all(format!("{token}\n").as_bytes()).await.unwrap();
        s.write_all(
            "{\"kind\":\"ask\",\"conv_id\":\"c1\",\"request_id\":\"a-1\",\"question\":\"选哪个方案\"}\n"
                .as_bytes(),
        )
        .await
        .unwrap();
        s.flush().await.unwrap();
        assert!(
            wait_until(&ctx, |c| {
                Box::pin(async move {
                    c.inbox
                        .lock()
                        .await
                        .iter()
                        .any(|t| t.contains("AskUserQuestion"))
                })
            })
            .await,
            "ask 询问应送达 IM"
        );
        ctx.disp.router.cancel("c1", "a-1").await;
        let mut buf = String::new();
        let mut r = tokio::io::BufReader::new(s);
        let _ = tokio::time::timeout(Duration::from_secs(2), r.read_line(&mut buf)).await;
        assert!(
            buf.contains("\"text\":null") && buf.contains("cancelled"),
            "cancelled 的 ask 应回 Err（text=null + error），而非当用户回答: {buf}"
        );
    }

    // permission 分支：cancel 语义不变 = deny（cancelled 标记不改变 permission
    // 协议的 allow:false 回复形态）。
    {
        let mut s = tokio::net::UnixStream::connect(&sock).await.unwrap();
        s.write_all(format!("{token}\n").as_bytes()).await.unwrap();
        s.write_all(
            b"{\"conv_id\":\"c1\",\"request_id\":\"p-1\",\"tool_name\":\"Bash\",\"input\":{}}\n",
        )
        .await
        .unwrap();
        s.flush().await.unwrap();
        assert!(
            wait_until(&ctx, |c| {
                Box::pin(async move {
                    c.inbox
                        .lock()
                        .await
                        .iter()
                        .any(|t| t.contains("请求执行 Bash"))
                })
            })
            .await,
            "permission 询问应送达 IM"
        );
        ctx.disp.router.cancel("c1", "p-1").await;
        let mut buf = String::new();
        let mut r = tokio::io::BufReader::new(s);
        let _ = tokio::time::timeout(Duration::from_secs(2), r.read_line(&mut buf)).await;
        assert!(
            buf.contains("\"allow\":false"),
            "cancelled 的 permission 应回 deny: {buf}"
        );
    }

    ctx.disp.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
    drop_db(ctx.db).await;
}

// ---------------------------------------------------------------------------
// T18（ConvState 单表）：LRU 驱逐矩阵测试。随 map 迁移批次扩展豁免维度
//（running / 排队豁免在对应字段并入后补齐）。
// ---------------------------------------------------------------------------

/// T18：LRU 驱逐基础——超上限按 last_touched 驱逐最久未活跃的 conv；挂起审批
/// （router pending）豁免；未超上限不动。
#[tokio::test]
async fn conv_state_lru_evicts_idle_keeps_pending() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // 三个 conv：old（最久未活跃）/ fresh（较新）/ pending（挂起审批）。
    ctx.disp
        .with_conv("c_old", |cs| cs.idle_override = Some(Duration::ZERO))
        .await;
    // 刻意把 old 的 last_touched 回拨（checked_sub 防时钟过young panic）。
    {
        let mut map = ctx.disp.conv_states.lock().await;
        if let Some(cs) = map.get_mut("c_old") {
            cs.last_touched = cs
                .last_touched
                .checked_sub(Duration::from_secs(3600))
                .unwrap_or(cs.last_touched);
        }
    }
    ctx.disp
        .with_conv("c_fresh", |cs| {
            cs.idle_override = Some(Duration::from_secs(60))
        })
        .await;
    let _rx = ctx
        .disp
        .router()
        .register(
            "c_pending",
            "r-1",
            None,
            crate::permission::PendingKind::Permission,
            Some("Bash"),
            None,
        )
        .await;
    ctx.disp
        .with_conv("c_pending", |cs| cs.idle_override = Some(Duration::ZERO))
        .await;
    // 未超上限：no-op。
    assert_eq!(ctx.disp.evict_idle_conv_states(10).await, 0);
    // cap=2：驱逐最久未活跃的 old（fresh 较新、pending 挂审批豁免）。
    let removed = ctx.disp.evict_idle_conv_states(2).await;
    assert_eq!(removed, 1, "应驱逐 1 个（old）");
    let map = ctx.disp.conv_states.lock().await;
    assert!(map.contains_key("c_fresh"), "较新会话保留");
    assert!(map.contains_key("c_pending"), "挂起审批的会话豁免");
    assert!(!map.contains_key("c_old"), "最久未活跃会话被驱逐");
    drop(map);
    drop_db(ctx.db).await;
}

/// T18：驱逐后再来的 conv 从干净状态开始（等价重启语义）——内存覆盖项
///（/timeout 的 idle_override）丢失，回到全局默认。
#[tokio::test]
async fn conv_state_evicted_conv_starts_clean() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    ctx.disp
        .with_conv("c1", |cs| cs.idle_override = Some(Duration::ZERO))
        .await;
    // 驱逐（cap=0 强制清空可驱逐条目）。
    let removed = ctx.disp.evict_idle_conv_states(0).await;
    assert_eq!(removed, 1);
    {
        let map = ctx.disp.conv_states.lock().await;
        assert!(!map.contains_key("c1"), "应被驱逐");
    }
    // 再来：从干净状态开始（override 丢失 = 重启语义）。
    let v = ctx
        .disp
        .peek_conv("c1", |cs| cs.and_then(|c| c.idle_override))
        .await;
    assert_eq!(v, None, "驱逐后的 conv 从干净状态开始");
    drop_db(ctx.db).await;
}

/// T18：with_conv 把回到全空的 entry 自动剪除（表不积空 entry）。
#[tokio::test]
async fn conv_state_entry_pruned_when_empty() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    ctx.disp
        .with_conv("c1", |cs| cs.idle_override = Some(Duration::ZERO))
        .await;
    {
        let map = ctx.disp.conv_states.lock().await;
        assert!(map.contains_key("c1"));
    }
    ctx.disp.with_conv("c1", |cs| cs.idle_override = None).await;
    let map = ctx.disp.conv_states.lock().await;
    assert!(!map.contains_key("c1"), "回到全空的 entry 应被剪除");
    drop(map);
    drop_db(ctx.db).await;
}

/// T18：LRU 驱逐豁免矩阵（完整版）——running / queue（runner 活跃）的 conv
/// 不被驱逐，仅静默会话被驱逐。queue=Some(空 Vec)（取批间隙）同样豁免
///（runner 循环仍依赖该身份）。
#[tokio::test]
async fn conv_state_lru_exempts_running_and_queued() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    // idle：最久未活跃，应被驱逐。
    ctx.disp
        .with_conv("c_idle", |cs| cs.idle_override = Some(Duration::ZERO))
        .await;
    // running：伪造在飞句柄（abort handle 挂在一个长睡任务上）。
    let jh = tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    ctx.disp
        .with_conv("c_running", |cs| {
            cs.running = Some(RoundHandle {
                abort: jh.abort_handle(),
                steer: None,
                started: std::time::Instant::now(),
                digest: None,
                snapshot: Arc::new(std::sync::Mutex::new(RoundSnapshot::default())),
            });
        })
        .await;
    // queued：runner 活跃（含空 Vec 形态——取批间隙）。
    ctx.disp
        .with_conv("c_queued", |cs| {
            cs.queue = Some(Vec::new());
        })
        .await;
    // 回拨 idle 的 last_touched，确保它是最旧。
    {
        let mut map = ctx.disp.conv_states.lock().await;
        if let Some(cs) = map.get_mut("c_idle") {
            cs.last_touched = cs
                .last_touched
                .checked_sub(Duration::from_secs(3600))
                .unwrap_or(cs.last_touched);
        }
    }
    // cap=2：只能驱逐 idle（running/queued 豁免），驱逐后表长 = cap = 2
    //（豁免条目不强行清空——feishu 侧的兜底清空在 core 不适用：丢 running
    // 句柄会让 /stop 失效，比超限更糟）。
    let removed = ctx.disp.evict_idle_conv_states(2).await;
    assert_eq!(removed, 1, "只应驱逐 idle 会话");
    let map = ctx.disp.conv_states.lock().await;
    assert!(!map.contains_key("c_idle"), "静默会话被驱逐");
    assert!(map.contains_key("c_running"), "在飞轮会话豁免");
    assert!(map.contains_key("c_queued"), "排队（runner 活跃）会话豁免");
    assert_eq!(map.len(), 2, "驱逐非豁免条目后收缩到 cap，豁免条目保留");
    drop(map);
    jh.abort();
    drop_db(ctx.db).await;
}

/// T18：驱逐不丢排队语义的根基——队列本体在 ConvState，驱逐只发生在
/// queue=None 的会话上；再次断言 cap=0 下 running/queued 也不被清。
#[tokio::test]
async fn conv_state_evict_zero_cap_keeps_exempt() {
    let _serial = SERIAL.lock().await;
    let ctx = build(Auth::new(vec!["alice".into()])).await;
    let jh = tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    ctx.disp
        .with_conv("c_running", |cs| {
            cs.running = Some(RoundHandle {
                abort: jh.abort_handle(),
                steer: None,
                started: std::time::Instant::now(),
                digest: None,
                snapshot: Arc::new(std::sync::Mutex::new(RoundSnapshot::default())),
            });
        })
        .await;
    ctx.disp
        .with_conv("c_queued", |cs| {
            cs.queue = Some(vec![QueuedMsg {
                rowid: 0,
                msg: msg("c_queued", "alice", "排着"),
            }]);
        })
        .await;
    ctx.disp
        .with_conv("c_idle", |cs| cs.pending_hint_last = Some(Instant::now()))
        .await;
    let removed = ctx.disp.evict_idle_conv_states(0).await;
    assert_eq!(removed, 1, "cap=0 只驱逐非豁免条目");
    let map = ctx.disp.conv_states.lock().await;
    assert!(map.contains_key("c_running") && map.contains_key("c_queued"));
    assert!(!map.contains_key("c_idle"));
    drop(map);
    jh.abort();
    drop_db(ctx.db).await;
}
