//! v1.9 会话语义标题生成器：网关用 headless CLI（`claude -p`）单次调用为
//! 会话起语义标题，懒触发 + 防重复 + 失败兜底机械摘要。
//!
//! 为什么自己生成：CLI 的语义标题不持久化（转录/索引均无），终端里看到的
//! 会话名是 CLI 侧运行时概念；网关要给 SPA 快照 `sessions.title` 提供语义
//! 标题，只能复用 CLI 的认证/供应商配置（ccswitch 跟随）再跑一次。
//!
//! 设计要点（任务说明 v1.9 定案）：
//! - **懒触发**：仅被查看的会话才生成——触发点在 [`crate::platform`] 的
//!   快照组装（`collect_session_entries`），SPA 打开面板/切会话都会拉快照；
//!   每快照至多 spawn [`TITLE_SPAWN_PER_SNAPSHOT`] 个**最旧**的未生成 conv
//!   （防一次快照生成风暴；快照低频到达，多次快照逐批收敛）。
//! - **防重复**：内存 `HashSet`（生成过的 conv，**含失败**——失败不重试防
//!   风暴）；容量 [`TITLE_ATTEMPTED_MAX`]，超出简单清半（清掉的 conv 最坏
//!   被再生成一次，幂等写库无正确性影响）。
//! - **成功** → `store.set_session_title`（sessions.title 列，schema v18），
//!   下一条快照自然带出新 title；**失败/超时/claude 不在 PATH** → 仅 debug
//!   日志静默降级（显示回退 first_prompt 机械摘要，既有逻辑不动）。
//! - **可注入**：执行外部命令抽成 [`RunCli`] 函数对象——生产实现
//!   [`run_claude_headless`]（tokio::process + kill_on_drop），测试注入假
//!   实现（成功/超时/失败路径全覆盖，不依赖运行机器上是否有 claude）。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use imagent_store::Store;
use tracing::debug;

use crate::{SESSION_TITLE_MAX_CHARS, TARGET};

/// first_prompt 注入生成 prompt 的截断上限（按 Unicode 码点）：500 字符
/// 足以定调任务主题，又防超长 prompt 拖慢 headless CLI。
const FIRST_PROMPT_MAX_CHARS: usize = 500;
/// 单次生成超时：超时即 kill（kill_on_drop），防慢 CLI 占住任务。
const TITLE_GEN_TIMEOUT: Duration = Duration::from_secs(10);
/// 防重复集容量上限：超出简单清半（见模块注释）。
const TITLE_ATTEMPTED_MAX: usize = 1000;
/// 每快照最多 spawn 的生成数（防一次快照生成风暴）。
pub(crate) const TITLE_SPAWN_PER_SNAPSHOT: usize = 3;

/// 执行外部命令的抽象（可注入便于测试）：入参 `(argv, cwd)`（argv[0] =
/// 程序名），Ok(stdout) = 成功；Err = spawn 失败（不在 PATH/目录不存在）/
/// 非零退出。调用方只关心成败与 stdout，语义在此合并。
pub(crate) type RunCli = Arc<
    dyn Fn(Vec<String>, PathBuf) -> BoxFuture<'static, std::result::Result<String, String>>
        + Send
        + Sync,
>;

/// 懒触发候选（快照组装点筛出）：`conv` + 生成所需的 `first_prompt`（种子
/// 文本）与 `workdir`（headless CLI 的 cwd——复用该会话的工程上下文）。
#[derive(Clone)]
pub(crate) struct TitleCandidate {
    pub(crate) conv: String,
    pub(crate) first_prompt: String,
    pub(crate) workdir: PathBuf,
}

/// 语义标题生成器：防重复集与执行器绑定成值，随 workspace 泵共享（每次
/// 快照组装共用同一集合——跨快照去重的关键）。`Clone` 仅共享句柄。
#[derive(Clone)]
pub(crate) struct TitleGen {
    store: Store,
    /// 已触发过生成的 conv（含失败——失败不重试防风暴）。纯内存临界区用
    /// std Mutex（lock_or_recover 同款纪律）。
    attempted: Arc<Mutex<HashSet<String>>>,
    runner: RunCli,
    timeout: Duration,
}

impl TitleGen {
    /// 生产构造：真 headless CLI 执行器 + 10s 超时。
    pub(crate) fn new(store: Store) -> Self {
        Self::with_runner(store, Arc::new(run_claude_headless), TITLE_GEN_TIMEOUT)
    }

    /// 注入形态（测试）：假执行器 + 可调超时（毫秒级以便测超时路径）。
    pub(crate) fn with_runner(store: Store, runner: RunCli, timeout: Duration) -> Self {
        Self {
            store,
            attempted: Arc::new(Mutex::new(HashSet::new())),
            runner,
            timeout,
        }
    }

    /// 懒触发入口（快照组装点调用）：`candidates` 为本快照筛出的「无语义
    /// title 且 first_prompt 非空」conv，按快照序（updated_at 降序 = 新→旧）
    /// 排好；取其中**最旧**的至多 [`TITLE_SPAWN_PER_SNAPSHOT`] 个 spawn。
    /// 登记发生在 spawn 前（同步临界区内）：并发的下一条快照不会重复选中
    /// 同一 conv；失败/超时同样不再重试。返回本次实际 spawn 数。
    pub(crate) fn spawn_lazy(&self, candidates: &[TitleCandidate]) -> usize {
        let picked: Vec<TitleCandidate> = {
            let mut set = lock_attempted(&self.attempted);
            let picked: Vec<TitleCandidate> = candidates
                .iter()
                .rev() // 快照序新→旧，反转后旧在前：优先补最旧会话
                .filter(|c| !set.contains(&c.conv))
                .take(TITLE_SPAWN_PER_SNAPSHOT)
                .cloned()
                .collect();
            for c in &picked {
                set.insert(c.conv.clone());
            }
            // 容量守卫：清到一半（任意一半即可——集合语义只是「别再生成」的
            // 优化，非正确性约束）。
            if set.len() >= TITLE_ATTEMPTED_MAX {
                let target = TITLE_ATTEMPTED_MAX / 2;
                while set.len() > target {
                    let Some(k) = set.iter().next().cloned() else {
                        break;
                    };
                    set.remove(&k);
                }
            }
            picked
        };
        for c in &picked {
            self.spawn(c.clone());
        }
        debug!(
            target: TARGET,
            candidates = candidates.len(),
            spawned = picked.len(),
            "语义标题懒触发"
        );
        picked.len()
    }

    /// spawn 一次生成任务（fire-and-forget，不阻塞快照组装）：成功 →
    /// `set_session_title`；失败/超时/清洗后为空 → 仅 debug 日志（防重复集
    /// 已登记，不重试）。
    fn spawn(&self, cand: TitleCandidate) {
        let store = self.store.clone();
        let runner = self.runner.clone();
        let timeout = self.timeout;
        let conv = cand.conv;
        tokio::spawn(async move {
            let prompt = build_title_prompt(&cand.first_prompt);
            let argv = vec![
                "claude".to_string(),
                "-p".to_string(),
                prompt,
                "--output-format".to_string(),
                "text".to_string(),
            ];
            let out = match tokio::time::timeout(timeout, runner(argv, cand.workdir)).await {
                Ok(Ok(out)) => out,
                Ok(Err(e)) => {
                    debug!(
                        target: TARGET,
                        conv = %conv,
                        error = %e,
                        "语义标题生成失败（claude 不在 PATH / spawn 失败 / 非零退出），不重试"
                    );
                    return;
                }
                Err(_) => {
                    debug!(target: TARGET, conv = %conv, "语义标题生成超时（已 kill），不重试");
                    return;
                }
            };
            let Some(title) = clean_title(&out) else {
                debug!(target: TARGET, conv = %conv, "语义标题输出清洗后为空，视为失败，不重试");
                return;
            };
            match store.set_session_title(&conv, &title).await {
                Ok(()) => debug!(target: TARGET, conv = %conv, title = %title, "语义标题已生成"),
                Err(e) => {
                    debug!(target: TARGET, conv = %conv, error = %e, "语义标题写库失败（best-effort）")
                }
            }
        });
    }
}

/// 中毒（临界区 panic）时取回数据继续服务：纯内存临界区无 IO 副作用可回滚
///（platform.rs lock_or_recover 同款取舍）。
fn lock_attempted(m: &Mutex<HashSet<String>>) -> std::sync::MutexGuard<'_, HashSet<String>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 生成 prompt（纯函数便于单测）：固定指令 + first_prompt 按码点截
/// [`FIRST_PROMPT_MAX_CHARS`]。指令要求「直接输出标题本身」，配合
/// [`clean_title`] 的剥引号兜底模型不听话的包裹。
pub(crate) fn build_title_prompt(first_prompt: &str) -> String {
    let truncated: String = first_prompt.chars().take(FIRST_PROMPT_MAX_CHARS).collect();
    format!(
        "用不超过12个汉字为下面这个编程任务起一个简短标题，直接输出标题本身，不要任何前缀、引号或标点解释：\n{truncated}"
    )
}

/// 输出清洗（纯函数便于单测）：trim → 剥首尾引号（`"`/`'`/`「`」`『』`/全
/// 角 `“”`——`trim_matches` 自带循环，容忍模型叠加包裹）→ 再 trim → 按码点
/// 截 [`SESSION_TITLE_MAX_CHARS`]（与 SPA 显示口径同源）；清洗后为空 →
/// None（视为失败）。
pub(crate) fn clean_title(raw: &str) -> Option<String> {
    let stripped = raw
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '「' | '」' | '『' | '』' | '“' | '”'))
        .trim();
    let title: String = stripped.chars().take(SESSION_TITLE_MAX_CHARS).collect();
    (!title.is_empty()).then_some(title)
}

/// 生产执行器：headless CLI 单次调用（`claude -p <prompt> --output-format
/// text`，cwd=该 conv 的 workdir——复用其认证/供应商配置，ccswitch 跟随）。
/// stdin 关闭（`-p` 不交互，防挂等输入）；kill_on_drop——超时丢弃 future
/// 时连子进程一并 kill（不留孤儿 claude）。stderr 丢弃（失败原因对调用方
/// 只有 debug 日志价值，stdout 才是标题）。
fn run_claude_headless(
    argv: Vec<String>,
    cwd: PathBuf,
) -> BoxFuture<'static, std::result::Result<String, String>> {
    Box::pin(async move {
        let program = argv.first().ok_or_else(|| "argv 为空".to_string())?.clone();
        let out = tokio::process::Command::new(program)
            .args(&argv[1..])
            .current_dir(&cwd)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| format!("spawn 失败（claude 不在 PATH？）：{e}"))?;
        if !out.status.success() {
            return Err(format!("退出码非 0：{}", out.status.code().unwrap_or(-1)));
        }
        String::from_utf8(out.stdout).map_err(|e| format!("stdout 非 UTF-8：{e}"))
    })
}

#[cfg(test)]
mod tests {
    //! 生成器全路径注入测试：假 RunCli（记录 argv/cwd + 可控行为）+ 临时
    //! store（真实 sqlite），不依赖运行机器上的 claude。

    use super::*;
    use std::sync::MutexGuard;

    /// 测试用临时 store（platform.rs test_store 同款手法）。
    async fn test_store() -> Store {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "imagent-web-titles-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Store::open(&p).await.expect("open store")
    }

    /// 假执行器的行为脚本（一次 spawn 消费一步）。
    #[derive(Clone)]
    enum Step {
        /// 成功：返回给定 stdout。
        Ok(&'static str),
        /// 失败：返回 Err。
        Fail,
        /// 挂起指定时长（配合短超时测 kill 路径）。
        Sleep(u64),
    }

    /// 记录器：runner 收到的 (argv, cwd) 序列。
    type Recorder = Arc<Mutex<Vec<(Vec<String>, PathBuf)>>>;

    /// 造一个假执行器 + 记录器。steps 顺序消费（耗尽后恒 Fail，防测试外
    /// 泄漏任务 panic）。
    fn fake_runner(steps: Vec<Step>) -> (RunCli, Recorder) {
        let steps = Arc::new(steps);
        let idx = Arc::new(Mutex::new(0usize));
        let rec: Recorder = Arc::new(Mutex::new(Vec::new()));
        let runner: RunCli = {
            let steps = steps.clone();
            let idx = idx.clone();
            let rec = rec.clone();
            Arc::new(move |argv: Vec<String>, cwd: PathBuf| {
                let steps = steps.clone();
                let idx = idx.clone();
                let rec = rec.clone();
                let fut: BoxFuture<'static, std::result::Result<String, String>> =
                    Box::pin(async move {
                        rec.lock().unwrap().push((argv, cwd));
                        // 取步骤须在 await 前释放 guard（Send 约束）。
                        let step = {
                            let mut i = idx.lock().unwrap();
                            let s = steps.get(*i).cloned();
                            *i += 1;
                            s
                        };
                        match step {
                            Some(Step::Ok(out)) => Ok((*out).to_string()),
                            Some(Step::Fail) => Err("假执行器失败".into()),
                            Some(Step::Sleep(ms)) => {
                                tokio::time::sleep(Duration::from_millis(ms)).await;
                                Ok("迟到的标题".into())
                            }
                            None => Err("脚本耗尽（视为失败）".into()),
                        }
                    });
                fut
            })
        };
        (runner, rec)
    }

    /// 种一条候选（cwd 用固定路径即可——假执行器不真跑）。
    fn cand(conv: &str) -> TitleCandidate {
        TitleCandidate {
            conv: conv.into(),
            first_prompt: format!("帮我把{conv}的构建修好"),
            workdir: PathBuf::from("/tmp/proj"),
        }
    }

    /// 种一条 session 行（title 列 UPDATE 的目标行）。
    async fn seed_session(store: &Store, conv: &str) {
        store
            .upsert_session(&imagent_store::SessionRow {
                conv_id: conv.into(),
                session_id: format!("sess-{conv}"),
                agent_kind: "claude".into(),
                workdir: "/tmp/proj".into(),
                name: None,
                created_at: 1,
                updated_at: 1,
                first_prompt: None,
                task_todos: None,
            })
            .await
            .unwrap();
    }

    /// 轮询等待条件成立（生成是 spawn 出去的异步任务，只能从库侧/记录器
    /// 侧观测；5s 上限）。
    async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        for _ in 0..250 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("等待超时（5s）：{what}");
    }

    /// 轮询等标题写库并返回（spawn 的短任务写库，只能轮询观测）。
    async fn wait_title(store: &Store, conv: &str) -> Option<String> {
        for _ in 0..250 {
            if let Some(t) = store.get_session_title(conv).await.unwrap() {
                return Some(t);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("等待语义标题写库超时（5s）：{conv}");
    }

    fn rec_lock(rec: &Recorder) -> MutexGuard<'_, Vec<(Vec<String>, PathBuf)>> {
        rec.lock().unwrap()
    }

    /// 清洗规则：trim / 剥引号包裹（ASCII+「」『』+全角，叠加可剥） /
    /// 按码点截 24 / 清洗后空 → None。
    #[test]
    fn clean_title_rules() {
        assert_eq!(clean_title("部署巡检"), Some("部署巡检".into()));
        assert_eq!(clean_title("  部署巡检 \n"), Some("部署巡检".into()));
        assert_eq!(clean_title("「部署巡检」"), Some("部署巡检".into()));
        assert_eq!(
            clean_title("『「部署巡检」』"),
            Some("部署巡检".into()),
            "叠加包裹可剥"
        );
        assert_eq!(clean_title("\"部署巡检\""), Some("部署巡检".into()));
        assert_eq!(clean_title("'部署巡检'"), Some("部署巡检".into()));
        assert_eq!(clean_title("“部署巡检”"), Some("部署巡检".into()));
        // 内部引号不动（标题本身含引号是合法内容）。
        assert_eq!(clean_title("修「构建」报错"), Some("修「构建」报错".into()));
        // 按码点截 24（中文 1 码点；30 字输入 → 恰 24 字）。
        let long = "语义标题生成器输出了一行超过二十四码点的长标题样例文本甲乙丙";
        assert_eq!(long.chars().count(), 30);
        assert_eq!(
            clean_title(long).unwrap(),
            "语义标题生成器输出了一行超过二十四码点的长标题样"
        );
        // 清洗后空 → None（视为失败）。
        assert_eq!(clean_title(""), None);
        assert_eq!(clean_title("   "), None);
        assert_eq!(clean_title("「」"), None);
        assert_eq!(clean_title("\"  \""), None);
    }

    /// prompt 组装：固定指令前缀 + first_prompt 按码点截 500。
    #[test]
    fn build_title_prompt_truncates_first_prompt() {
        let p = build_title_prompt("修复构建");
        assert!(
            p.starts_with("用不超过12个汉字为下面这个编程任务起一个简短标题"),
            "指令前缀：{p}"
        );
        assert!(p.contains("修复构建"), "正文拼接：{p}");
        // 600 字符 → 只保留前 500。
        let long = "任".repeat(600);
        let p = build_title_prompt(&long);
        let body = p.split('\n').nth(1).unwrap();
        assert_eq!(body.chars().count(), 500, "first_prompt 截 500 码点");
    }

    /// 成功路径（任务验收项）：假执行器回带包裹的标题 → 清洗（剥「」+trim）
    /// 后写库；runner 收到的 argv 形如 claude -p <prompt> --output-format
    /// text、cwd=候选 workdir。
    #[tokio::test]
    async fn spawn_success_cleans_and_writes_title() {
        let store = test_store().await;
        seed_session(&store, "web:s-01").await;
        let (runner, rec) = fake_runner(vec![Step::Ok("  「部署巡检」 \n")]);
        let gen = TitleGen::with_runner(store.clone(), runner, Duration::from_secs(2));
        let c = cand("web:s-01");
        let first_prompt = c.first_prompt.clone();
        assert_eq!(gen.spawn_lazy(&[c]), 1);

        let title = wait_title(&store, "web:s-01").await;
        assert_eq!(
            title.as_deref(),
            Some("部署巡检"),
            "清洗（剥「」+trim）后写库"
        );

        // argv / cwd 断言。
        let calls = rec_lock(&rec);
        assert_eq!(calls.len(), 1);
        let (argv, cwd) = &calls[0];
        assert_eq!(argv[0], "claude");
        assert_eq!(argv[1], "-p");
        assert_eq!(argv[3], "--output-format");
        assert_eq!(argv[4], "text");
        assert!(
            argv[2].contains(&first_prompt),
            "prompt 含 first_prompt：{}",
            argv[2]
        );
        assert_eq!(cwd, &PathBuf::from("/tmp/proj"), "cwd=conv workdir");
    }

    /// 超时路径（任务验收项）：执行器挂起超过注入超时 → 不写库；防重复集
    /// 已登记——同 conv 再次懒触发返回 0（不重试）。
    #[tokio::test]
    async fn spawn_timeout_no_write_no_retry() {
        let store = test_store().await;
        seed_session(&store, "web:s-02").await;
        // 执行器挂 5s，注入超时 50ms —— 必然走超时分支。
        let (runner, _rec) = fake_runner(vec![Step::Sleep(5_000)]);
        let gen = TitleGen::with_runner(store.clone(), runner, Duration::from_millis(50));
        assert_eq!(gen.spawn_lazy(&[cand("web:s-02")]), 1);
        // 等超时分支走完（远大于 50ms 超时、远小于 5s 挂起）。
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            store.get_session_title("web:s-02").await.unwrap(),
            None,
            "超时不写库"
        );
        assert_eq!(
            gen.spawn_lazy(&[cand("web:s-02")]),
            0,
            "防重复集已登记（失败/超时不重试）"
        );
    }

    /// 失败路径（任务验收项）：执行器 Err（对应 claude 不在 PATH / 非零
    /// 退出）→ 不写库、不重试；清洗后为空同样视为失败。
    #[tokio::test]
    async fn spawn_failure_and_empty_output_no_retry() {
        let store = test_store().await;
        seed_session(&store, "web:s-03").await;
        seed_session(&store, "web:s-04").await;
        // s-03 执行器失败；s-04 执行器成功但输出清洗后为空。
        let (runner, _rec) = fake_runner(vec![Step::Fail, Step::Ok("  「  」  ")]);
        let gen = TitleGen::with_runner(store.clone(), runner, Duration::from_secs(2));
        assert_eq!(gen.spawn_lazy(&[cand("web:s-03")]), 1);
        assert_eq!(gen.spawn_lazy(&[cand("web:s-04")]), 1);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            store.get_session_title("web:s-03").await.unwrap(),
            None,
            "失败不写库"
        );
        assert_eq!(
            store.get_session_title("web:s-04").await.unwrap(),
            None,
            "清洗后为空视为失败"
        );
        assert_eq!(gen.spawn_lazy(&[cand("web:s-03")]), 0, "失败不重试");
        assert_eq!(gen.spawn_lazy(&[cand("web:s-04")]), 0, "空输出不重试");
    }

    /// 懒触发批上限（任务验收项）：5 个候选（快照序新→旧）→ 首次只 spawn
    /// 最旧的 3 个；再次触发补齐剩余 2 个；第三次 0（全部已登记）。
    #[tokio::test]
    async fn spawn_lazy_caps_at_3_oldest_first() {
        let store = test_store().await;
        let (runner, rec) = fake_runner(vec![Step::Fail; 5]);
        let gen = TitleGen::with_runner(store.clone(), runner, Duration::from_secs(2));
        // 快照序：s-00 最新 → s-04 最旧。
        let candidates: Vec<TitleCandidate> =
            (0..5).map(|i| cand(&format!("web:s-0{i}"))).collect();
        assert_eq!(gen.spawn_lazy(&candidates), 3, "每快照至多 3 个");
        wait_until("首批 3 次执行", || rec_lock(&rec).len() == 3).await;
        // 首批为最旧的 3 个（s-02/s-03/s-04，各恰一次；spawn 是异步任务，
        // 执行先后不定，比较多重集合）。
        let all = ["web:s-00", "web:s-01", "web:s-02", "web:s-03", "web:s-04"];
        let mut markers: Vec<String> = rec_lock(&rec)
            .iter()
            .map(|(argv, _)| {
                all.iter()
                    .find(|c| argv[2].contains(*c))
                    .expect("prompt 含 conv 标记")
                    .to_string()
            })
            .collect();
        markers.sort();
        assert_eq!(
            markers,
            vec![
                "web:s-02".to_string(),
                "web:s-03".to_string(),
                "web:s-04".to_string()
            ]
        );

        assert_eq!(gen.spawn_lazy(&candidates), 2, "第二次补齐剩余 2 个");
        wait_until("累计 5 次执行", || rec_lock(&rec).len() == 5).await;
        assert_eq!(gen.spawn_lazy(&candidates), 0, "第三次无新候选（防重复）");
        assert_eq!(gen.spawn_lazy(&[]), 0, "空候选集 no-op");
    }

    /// 防重复集容量（任务验收项）：登记满 [`TITLE_ATTEMPTED_MAX`]（1000）
    /// 后简单清半——集合收缩到 500，防长寿命进程内存无界增长。
    #[tokio::test]
    async fn attempted_set_clears_half_at_capacity() {
        let store = test_store().await;
        let (runner, _rec) = fake_runner(vec![]);
        let gen = TitleGen::with_runner(store, runner, Duration::from_secs(2));
        // 1000 个候选反复整批懒触发（批上限 3 + 防重复过滤 → 每次登记 3 个
        // 新 conv），累计登记满 1000 时触发清半。
        let candidates: Vec<TitleCandidate> = (0..1000)
            .map(|i| cand(&format!("web:cap-{i:04}")))
            .collect();
        let mut spawned = 0;
        while spawned < 1000 {
            spawned += gen.spawn_lazy(&candidates);
        }
        assert_eq!(spawned, 1000);
        assert_eq!(gen.attempted_len(), 500, "满 1000 清半");
    }

    /// 测试专用：读防重复集大小（集合就在内存，直接同步读）。
    impl TitleGen {
        fn attempted_len(&self) -> usize {
            lock_attempted(&self.attempted).len()
        }
    }
}
