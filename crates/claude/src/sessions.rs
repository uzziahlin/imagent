//! 本机 Claude Code 会话扫描（统一 `/resume`，P4-11）。
//!
//! Claude Code 把每个会话存为 `~/.claude/projects/<workdir编码>/<uuid>.jsonl`：
//! - 目录编码：workdir 绝对路径的 `/` 替换为 `-`（本机实测
//!   `/Users/x/Work/imagent` → `-Users-x-Work-imagent`）；
//! - session id = 文件名 stem（uuid）；
//! - 首条用户消息在 `type=="user"` 行的 `message.content`（str 或 blocks 数组），
//!   头部还有 `mode` / `permission-mode` / `file-history-snapshot` 等元数据行需跳过。
//!
//! 全部纯函数 + 容错解析（任何异常按「无摘要」处理，不影响列出会话）；时间统一用
//! 文件 mtime（免解析 JSONL 内的 ISO8601）。目录不存在返回空（claude 未用过/
//! 版本布局变化 → `/resume` 自动退化为纯 IM 历史，不报错）。

use std::path::{Path, PathBuf};

use imagent_core::LocalSession;

/// 每个会话头部最多读的字节数（首条 user 消息通常在前几行；cap 防大文件全读）。
const HEAD_CAP: usize = 64 * 1024;
/// 摘要长度上限（char 计）。
const SUMMARY_CHARS: usize = 60;
/// 默认列出条数（dispatch 侧再截前 10 展示）。
const DEFAULT_LIMIT: usize = 15;

/// 默认 claude 配置根：`~/.claude`。
pub fn default_claude_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude"))
}

/// workdir → projects 子目录名候选集（P5-15）。
///
/// 本机实测 `/` → `-`（`/Users/x/Work/imagent` → `-Users-x-Work-imagent`），但该
/// 规则让 `/a/b-c` 与 `/a/b/c` 编码冲突；且不同 Claude Code 版本对 `.` `_` 等字符
/// 的处理未实测（社区规则是也替换为 `-`）。联合扫描多个候选（去重），配合
/// [`LocalSession::cwd`] 的接管校验兜底：编码猜错最多扫不到（退化为纯 IM 历史），
/// 不会把别的项目的会话串进来。
pub fn encode_candidates(workdir: &Path) -> Vec<String> {
    let s = workdir.to_string_lossy();
    let mut v = vec![s.replace('/', "-")];
    let dots = s.replace(['/', '.', '_'], "-");
    if !v.contains(&dots) {
        v.push(dots);
    }
    let alnum: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if !v.contains(&alnum) {
        v.push(alnum);
    }
    v
}

/// 列出与 workdir 同项目的本机会话，按 mtime 倒序（原始精度排序，避免同秒并列
/// 顺序不稳定），最多 `limit` 条。多个候选编码目录联合扫描，session_id 去重。
pub fn list_local_sessions(claude_dir: &Path, workdir: &Path, limit: usize) -> Vec<LocalSession> {
    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut out: Vec<(std::time::SystemTime, LocalSession)> = Vec::new();
    for enc in encode_candidates(workdir) {
        let dir = claude_dir.join("projects").join(enc);
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue, // 候选目录不存在：正常（该编码规则下无会话）
        };
        for entry in rd.flatten() {
            if entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let session_id = match entry.path().file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if !seen.insert(session_id.clone()) {
                continue;
            }
            let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            let updated_at = mtime
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let (first_prompt, cwd) = read_head_info(&entry.path());
            out.push((
                mtime,
                LocalSession {
                    session_id,
                    updated_at,
                    first_prompt,
                    cwd,
                },
            ));
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.truncate(limit);
    out.into_iter().map(|(_, s)| s).collect()
}

/// Backend trait 实现共用入口（claude-cli / claude-acp 同一存储布局）。
pub(crate) fn scan_for_backend(workdir: &Path) -> Vec<LocalSession> {
    match default_claude_dir() {
        Some(dir) => list_local_sessions(&dir, workdir, DEFAULT_LIMIT),
        None => Vec::new(),
    }
}

/// 该 session id 在 `~/.claude/projects/<workdir 编码>/` 是否真实存在
/// （文件名 `<session_id>.jsonl`，跨全部编码候选查）。
///
/// 真机校准（2026-08）：失败轮次（如 CLI 参数被拒即退）的 stream 事件仍可能携带
/// session_id，imagent 落库后成为「幽灵会话」——下次 `--resume` 得到
/// `No conversation found` 的 is_error 空文本 result，且每轮失败再产新幽灵 id
/// （毒化循环）。run 前用本函数预检，幽灵即弃用续接、开新会话。
pub(crate) fn session_exists(workdir: &Path, session_id: &str) -> bool {
    // default_claude_dir 返回 ~/.claude，会话文件在 projects/ 子层——少拼这一段
    // 会把所有会话误判为幽灵（每轮开新会话、上下文全丢，真机踩过）。
    default_claude_dir()
        .is_some_and(|base| session_exists_in(&base.join("projects"), workdir, session_id))
}

/// W4-2：会话 jsonl → Markdown 转录（/export 数据源）。逐行解析 user/assistant
/// 消息的 text 块（tool_use/tool_result/meta 行跳过——转录面向人读对话，非完整
/// 调试转储）；行级解析失败跳过不中断。找不到文件或无对话内容返回 None。
pub fn export_session_md(workdir: &Path, session_id: &str) -> Option<String> {
    export_session_md_at(&default_claude_dir()?.join("projects"), workdir, session_id)
}

/// [`export_session_md`] 的可注入版本（测试用自定义根目录）。
pub fn export_session_md_at(base: &Path, workdir: &Path, session_id: &str) -> Option<String> {
    use std::io::BufRead;
    for enc in encode_candidates(workdir) {
        let path = base.join(enc).join(format!("{session_id}.jsonl"));
        let Ok(f) = std::fs::File::open(&path) else {
            continue;
        };
        let mut out = format!("# imagent 会话导出（{session_id}）\n\n");
        for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let role = match v.get("type").and_then(|t| t.as_str()) {
                Some("user") => "🧑 用户",
                Some("assistant") => "🤖 Claude",
                _ => continue,
            };
            let Some(blocks) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            else {
                continue;
            };
            let texts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .filter(|t| !t.trim().is_empty())
                .collect();
            if texts.is_empty() {
                continue;
            }
            out.push_str(&format!("## {role}\n\n{}\n\n", texts.join("\n\n")));
        }
        if out.lines().count() > 2 {
            return Some(out);
        }
    }
    None
}

fn session_exists_in(base: &Path, workdir: &Path, session_id: &str) -> bool {
    encode_candidates(workdir)
        .iter()
        .any(|enc| base.join(enc).join(format!("{session_id}.jsonl")).is_file())
}

/// 读文件头部（≤ HEAD_CAP）：`(首条可展示的 user 消息文本, 会话记录的 cwd)`。
///
/// 摘要跳过：非 user 行、`isMeta` 行、`<` 开头的命令/系统注入文本、tool_result 块。
/// cwd 取首个带非空 `cwd` 字符串字段的行（真实 jsonl 几乎每行都有；P5-15 接管
/// 校验用，解析不到为 None → 跳过校验不阻塞列出）。
fn read_head_info(path: &Path) -> (String, Option<String>) {
    use std::io::Read;
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (String::new(), None),
    };
    let mut buf = vec![0u8; HEAD_CAP];
    let Ok(n) = f.read(&mut buf) else {
        return (String::new(), None);
    };
    buf.truncate(n);
    let mut cwd: Option<String> = None;
    for line in buf.split(|&b| b == b'\n') {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if cwd.is_none() {
            if let Some(c) = v
                .get("cwd")
                .and_then(|c| c.as_str())
                .filter(|s| !s.is_empty())
            {
                cwd = Some(c.to_string());
            }
        }
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        if v.get("isMeta").and_then(|m| m.as_bool()).unwrap_or(false) {
            continue;
        }
        let Some(text) = extract_content_text(v.get("message").and_then(|m| m.get("content")))
        else {
            continue;
        };
        let t = text.trim();
        if t.is_empty() || t.starts_with('<') {
            continue;
        }
        return (sanitize_summary(t), cwd);
    }
    (String::new(), cwd)
}

/// 提取 user 消息 content 文本：`"str"` 直接取；blocks 数组取 text 块拼接
/// （tool_result / image 等块跳过）。
fn extract_content_text(content: Option<&serde_json::Value>) -> Option<String> {
    match content? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(blocks) => {
            let texts: Vec<String> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .map(String::from)
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join(" "))
            }
        }
        _ => None,
    }
}

/// 摘要消毒：压空白（含换行）、截 SUMMARY_CHARS 字符加省略号。
fn sanitize_summary(s: &str) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > SUMMARY_CHARS {
        format!("{}…", flat.chars().take(SUMMARY_CHARS).collect::<String>())
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    /// W4-2：jsonl → Markdown 转录——user/assistant 文本块成段、tool/meta 行
    /// 跳过；找不到文件 None。
    #[test]
    fn export_session_md_transcripts_dialog() {
        let dir = std::env::temp_dir().join(format!("imagent_export_{}", std::process::id()));
        let proj = dir.join("-tmp-ws");
        std::fs::create_dir_all(&proj).unwrap();
        let sid = "sess-exp1";
        let jsonl = proj.join(format!("{sid}.jsonl"));
        let lines = [
            r#"{"type":"user","message":{"content":[{"type":"text","text":"帮我看看"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"内部推理"},{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"结论 A"},{"type":"text","text":"补充 B"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
            r#"{"type":"summary","summary":"meta 行"}"#,
        ]
        .join("\n");
        std::fs::write(&jsonl, lines).unwrap();
        let md = export_session_md_at(&dir, std::path::Path::new("/tmp/ws"), sid).expect("应导出");
        assert!(md.contains("## 🧑 用户"), "{md}");
        assert!(md.contains("帮我看看"), "{md}");
        assert!(md.contains("## 🤖 Claude"), "{md}");
        assert!(md.contains("结论 A") && md.contains("补充 B"), "{md}");
        assert!(!md.contains("内部推理"), "thinking 不进转录: {md}");
        assert!(!md.contains("tool_result"), "工具行不进转录: {md}");
        assert!(!md.contains("meta 行"), "非对话行跳过: {md}");
        // 不存在的会话 → None。
        assert!(export_session_md_at(&dir, std::path::Path::new("/tmp/ws"), "nope").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "imagent_claude_sess_{}_{}_{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 写一个会话 jsonl（lines 为原始 JSON 行）。落到主候选（仅 `/` → `-`，
    /// 本机实测规则）目录下。
    fn write_session(root: &Path, workdir: &Path, id: &str, lines: &[String]) {
        let dir = root
            .join("projects")
            .join(workdir.to_string_lossy().replace('/', "-"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{id}.jsonl")), lines.join("\n") + "\n").unwrap();
    }

    fn user_line(content: serde_json::Value) -> String {
        serde_json::json!({
            "type": "user",
            "timestamp": "2026-08-17T00:00:00.000Z",
            "message": { "role": "user", "content": content }
        })
        .to_string()
    }

    /// 带 cwd 的 user 行（P5-15：真实 jsonl 每行带 cwd 字段）。
    fn line_with_cwd(wd: &Path, text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "cwd": wd.to_string_lossy(),
            "message": { "role": "user", "content": text }
        })
        .to_string()
    }

    /// P5-15：候选编码联合扫描 + cwd 提取 + 歧义去重。
    #[test]
    fn candidates_union_scan_and_cwd_extracted() {
        let root = tmp_root("cand");
        // 含 `.` 的 workdir：主候选（仅 / → -）与 dots 候选（/._ → -）目录不同。
        let wd = Path::new("/tmp/proj.x");
        write_session(&root, wd, "s1", &[line_with_cwd(wd, "主线会话")]);
        let alt_dir = root.join("projects").join("-tmp-proj-x");
        std::fs::create_dir_all(&alt_dir).unwrap();
        std::fs::write(alt_dir.join("s2.jsonl"), line_with_cwd(wd, "候选会话")).unwrap();

        let list = list_local_sessions(&root, wd, 10);
        let ids: Vec<&str> = list.iter().map(|s| s.session_id.as_str()).collect();
        assert!(
            ids.contains(&"s1") && ids.contains(&"s2"),
            "两个候选目录应联合可见: {ids:?}"
        );
        assert!(
            list.iter().all(|s| s.cwd.as_deref() == Some("/tmp/proj.x")),
            "cwd 应从 jsonl 提取: {list:?}"
        );

        // 歧义去重：同 session_id 出现在两个候选目录只列一次。
        std::fs::write(alt_dir.join("s1.jsonl"), line_with_cwd(wd, "dup")).unwrap();
        let list2 = list_local_sessions(&root, wd, 10);
        assert_eq!(
            list2.iter().filter(|s| s.session_id == "s1").count(),
            1,
            "跨候选目录同 id 应去重"
        );
    }

    /// 真机校准（幽灵会话）：session_exists 只认本地存储真实存在的 <id>.jsonl
    /// （跨编码候选），幽灵 id（失败轮次泄漏的）必须判 false——backend 据此弃用续接。
    #[test]
    fn session_exists_across_candidates() {
        let root = tmp_root("exists");
        let wd = Path::new("/tmp/proj.x");
        write_session(&root, wd, "real-1", &[line_with_cwd(wd, "真实会话")]);
        assert!(session_exists_in(&root.join("projects"), wd, "real-1"));
        assert!(!session_exists_in(&root.join("projects"), wd, "ghost-9"));
        let alt = root.join("projects").join("-tmp-proj-x");
        std::fs::create_dir_all(&alt).unwrap();
        std::fs::write(alt.join("alt-2.jsonl"), "{}").unwrap();
        assert!(session_exists_in(&root.join("projects"), wd, "alt-2"));
    }

    #[test]
    fn encode_matches_claude_layout() {
        // 本机实测：/Users/x/Work/imagent → -Users-x-Work-imagent（候选集首元素）。
        let cands = encode_candidates(Path::new("/Users/x/Work/imagent"));
        assert_eq!(cands[0], "-Users-x-Work-imagent");
        // 含 `.` 的路径产生多个不同候选（P5-15 联合扫描）。
        let cands = encode_candidates(Path::new("/tmp/proj.x"));
        assert_eq!(cands[0], "-tmp-proj.x");
        assert!(
            cands.contains(&"-tmp-proj-x".to_string()),
            "候选应含 /._ 规则: {cands:?}"
        );
    }

    #[test]
    fn lists_sessions_sorted_by_mtime_desc() {
        let root = tmp_root("sort");
        let wd = Path::new("/tmp/proj-a");
        write_session(&root, wd, "old", &[user_line("old work".into())]);
        std::fs::write(
            root.join("projects")
                .join(wd.to_string_lossy().replace('/', "-"))
                .join("new.jsonl"),
            user_line("new work".into()),
        )
        .unwrap();
        // 让 new 的 mtime 严格更晚（fs 精度兜底：不 sleep 则靠写入顺序，可能同秒）。
        let list = list_local_sessions(&root, wd, 10);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].session_id, "new", "mtime 新的在前: {list:?}");
        assert_eq!(list[1].session_id, "old");
        assert_eq!(list[1].first_prompt, "old work");
    }

    #[test]
    fn extracts_first_user_prompt_across_shapes() {
        let root = tmp_root("shapes");
        let wd = Path::new("/tmp/proj-b");
        // 头部元数据行 + isMeta 行 + 命令注入 + str 内容 + blocks 内容。
        write_session(
            &root,
            wd,
            "s1",
            &[
                r#"{"type":"mode"}"#.into(),
                r#"{"type":"file-history-snapshot"}"#.into(),
                serde_json::json!({"type":"user","isMeta":true,"message":{"role":"user","content":"meta"}}).to_string(),
                user_line("<command-name>/foo</command-name>".into()),
                user_line("真实的第一句".into()),
                user_line(serde_json::json!([{"type":"text","text":"第二句"}])),
            ],
        );
        let list = list_local_sessions(&root, wd, 10);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].first_prompt, "真实的第一句");
    }

    #[test]
    fn tool_result_blocks_skipped_and_text_blocks_joined() {
        let root = tmp_root("blocks");
        let wd = Path::new("/tmp/proj-c");
        // 首个 user 是纯 tool_result（无 text 块）→ 跳过；次个 blocks 取 text。
        write_session(
            &root,
            wd,
            "s2",
            &[
                user_line(serde_json::json!([{"type":"tool_result","content":"x"}])),
                user_line(serde_json::json!([
                    {"type":"text","text":"合并"},
                    {"type":"image","source":"..."},
                    {"type":"text","text":"多块"}
                ])),
            ],
        );
        let list = list_local_sessions(&root, wd, 10);
        assert_eq!(list[0].first_prompt, "合并 多块");
    }

    #[test]
    fn summary_flattens_and_truncates() {
        let root = tmp_root("trunc");
        let wd = Path::new("/tmp/proj-d");
        let long = "长".repeat(80) + "\n带换行 tail";
        write_session(&root, wd, "s3", &[user_line(long.into())]);
        let list = list_local_sessions(&root, wd, 10);
        let p = &list[0].first_prompt;
        assert!(p.chars().count() <= SUMMARY_CHARS + 1, "截断: {p}");
        assert!(p.ends_with('…'));
        assert!(!p.contains('\n'), "换行压平: {p}");
    }

    #[test]
    fn missing_dir_or_malformed_returns_empty_or_tolerant() {
        let root = tmp_root("missing");
        // 项目目录不存在 → 空。
        assert!(list_local_sessions(&root, Path::new("/nope"), 10).is_empty());
        // 非 jsonl 文件忽略；损坏行容忍（无摘要但不 panic）。
        let wd = Path::new("/tmp/proj-e");
        let dir = root
            .join("projects")
            .join(wd.to_string_lossy().replace('/', "-"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("somedir"), b"").unwrap();
        std::fs::write(dir.join("broken.jsonl"), b"not json\n{\"type\":\"user\"").unwrap();
        let list = list_local_sessions(&root, wd, 10);
        assert_eq!(list.len(), 1, "只 broken.jsonl 一个会话: {list:?}");
        assert!(list[0].first_prompt.is_empty(), "坏行无摘要但可列出");
    }

    #[test]
    fn limit_truncates() {
        let root = tmp_root("limit");
        let wd = Path::new("/tmp/proj-f");
        for i in 0..5 {
            write_session(
                &root,
                wd,
                &format!("s{i}"),
                &[user_line(format!("p{i}").into())],
            );
        }
        assert_eq!(list_local_sessions(&root, wd, 3).len(), 3);
    }

    /// 真机冒烟（默认忽略）：
    /// `IMAGENT_RESUME_SMOKE_WD=/path/to/proj cargo test -p imagent-claude --lib smoke_real_dir -- --ignored --nocapture`
    #[test]
    #[ignore = "需真实 ~/.claude，经 IMAGENT_RESUME_SMOKE_WD 指定项目目录"]
    fn smoke_real_dir() {
        if let Ok(wd) = std::env::var("IMAGENT_RESUME_SMOKE_WD") {
            let dir = default_claude_dir().expect("home");
            for s in list_local_sessions(&dir, Path::new(&wd), 10) {
                println!("{} | {} | {}", s.updated_at, s.session_id, s.first_prompt);
            }
        }
    }
}
