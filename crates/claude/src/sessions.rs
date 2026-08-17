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

/// workdir → projects 子目录名（`/` → `-`）。
pub fn encode_project_dir(workdir: &Path) -> String {
    workdir.to_string_lossy().replace('/', "-")
}

/// 列出与 workdir 同项目的本机会话，按 mtime 倒序（原始精度排序，避免同秒并列
/// 顺序不稳定），最多 `limit` 条。
pub fn list_local_sessions(claude_dir: &Path, workdir: &Path, limit: usize) -> Vec<LocalSession> {
    let dir = claude_dir
        .join("projects")
        .join(encode_project_dir(workdir));
    let rd = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return Vec::new(), // 目录不存在/不可读：无本机会话
    };
    // (mtime 原始 SystemTime, LocalSession)——mtime 到展示层才折算秒。
    let mut out: Vec<(std::time::SystemTime, LocalSession)> = rd
        .flatten()
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .filter_map(|e| {
            let session_id = e.path().file_stem()?.to_str()?.to_string();
            let mtime = e.metadata().and_then(|m| m.modified()).ok()?;
            let updated_at = mtime
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let first_prompt = read_first_user_prompt(&e.path()).unwrap_or_default();
            Some((
                mtime,
                LocalSession {
                    session_id,
                    updated_at,
                    first_prompt,
                },
            ))
        })
        .collect();
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

/// 读文件头部（≤ HEAD_CAP），逐行找首条可展示的 user 消息文本。
///
/// 跳过：非 user 行、`isMeta` 行、`<` 开头的命令/系统注入文本、tool_result 块。
fn read_first_user_prompt(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; HEAD_CAP];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    for line in buf.split(|&b| b == b'\n') {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
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
        return Some(sanitize_summary(t));
    }
    None
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

    /// 写一个会话 jsonl（lines 为原始 JSON 行）。
    fn write_session(root: &Path, workdir: &Path, id: &str, lines: &[String]) {
        let dir = root.join("projects").join(encode_project_dir(workdir));
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

    #[test]
    fn encode_matches_claude_layout() {
        // 本机实测：/Users/x/Work/imagent → -Users-x-Work-imagent。
        assert_eq!(
            encode_project_dir(Path::new("/Users/x/Work/imagent")),
            "-Users-x-Work-imagent"
        );
    }

    #[test]
    fn lists_sessions_sorted_by_mtime_desc() {
        let root = tmp_root("sort");
        let wd = Path::new("/tmp/proj-a");
        write_session(&root, wd, "old", &[user_line("old work".into())]);
        std::fs::write(
            root.join("projects")
                .join(encode_project_dir(wd))
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
        let dir = root.join("projects").join(encode_project_dir(wd));
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
