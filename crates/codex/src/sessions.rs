//! 本机 Codex CLI 会话扫描（统一 `/resume`，P5）。
//!
//! Codex 把每个会话存为 `~/.codex/sessions/YYYY/MM/DD/rollout-<时间>-<uuid>.jsonl`：
//! - 首行 `{"type":"session_meta","payload":{"id":"<uuid>","cwd":"…",…}}`——
//!   session id = `payload.id`（与 `codex exec --json` 报告的 thread_id 同源），
//!   cwd 用于按 workdir 过滤（目录按日期嵌套、不含项目信息，只能读文件头部判定）；
//! - user 消息在 `{"type":"response_item","payload":{"type":"message","role":"user",
//!   "content":[{"type":"input_text","text":"…"}]}}` 行；首条常是 AGENTS.md 注入
//!   （`#`/`<` 开头）需跳过。
//!
//! 全部纯函数 + 容错解析（异常按无摘要处理，不 panic）；目录不存在返回空
//!（codex 未用过 → `/resume` 退化为纯 IM 历史）。为控制开销，只按 mtime 倒序
//! 检查最近 [`SCAN_CAP`] 个文件。

use std::path::{Path, PathBuf};

use imagent_core::LocalSession;

/// 单文件头部最多读的字节数（session_meta 首行 + 前几条消息）。
const HEAD_CAP: usize = 64 * 1024;
/// 摘要长度上限（char 计，与 claude 扫描器一致）。
const SUMMARY_CHARS: usize = 60;
/// 最多检查的最近文件数（目录按日期嵌套无法按项目过滤，逐个读头有开销）。
const SCAN_CAP: usize = 200;
/// 默认列出条数（dispatch 侧再截前 10 展示）。
const DEFAULT_LIMIT: usize = 15;

/// 默认 codex 配置根：`~/.codex`。
pub fn default_codex_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex"))
}

/// 列出与 workdir 同项目的本机会话，按 mtime 倒序，最多 `limit` 条。
pub fn list_local_sessions(codex_dir: &Path, workdir: &Path, limit: usize) -> Vec<LocalSession> {
    // 收集 sessions/YYYY/MM/DD/ 下全部 jsonl（mtime, path）。
    let root = codex_dir.join("sessions");
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for y in dir_entries(&root) {
        for m in dir_entries(&y) {
            for d in dir_entries(&m) {
                for f in dir_entries(&d) {
                    if f.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let Ok(md) = std::fs::metadata(&f) else {
                        continue;
                    };
                    let Ok(mtime) = md.modified() else {
                        continue;
                    };
                    files.push((mtime, f));
                }
            }
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.truncate(SCAN_CAP);

    let mut out = Vec::new();
    for (mtime, path) in files {
        if out.len() >= limit {
            break;
        }
        // 读头部：session_meta（id + cwd）+ 首条可展示 user 消息。
        let Some(h) = read_head(&path) else {
            continue;
        };
        if h.cwd != workdir.to_string_lossy() {
            continue;
        }
        let updated_at = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        out.push(LocalSession {
            session_id: h.id,
            updated_at,
            first_prompt: h.first_prompt,
            cwd: Some(h.cwd),
        });
    }
    out
}

/// Backend trait 实现入口。
pub(crate) fn scan_for_backend(workdir: &Path) -> Vec<LocalSession> {
    match default_codex_dir() {
        Some(dir) => list_local_sessions(&dir, workdir, DEFAULT_LIMIT),
        None => Vec::new(),
    }
}

/// 目录条目列表（读取失败返回空）。
fn dir_entries(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// 文件头部的结构化信息。
struct Head {
    id: String,
    cwd: String,
    first_prompt: String,
}

/// 读文件头部（≤ HEAD_CAP）：首行 session_meta 取 id/cwd；随后逐行找首条可展示
/// 的 user 消息（跳过 `#`/`<` 开头的 AGENTS.md / 命令注入）。
fn read_head(path: &Path) -> Option<Head> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; HEAD_CAP];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    let mut head: Option<Head> = None;
    for line in buf.split(|&b| b == b'\n') {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if head.is_none() && v.get("type").and_then(|t| t.as_str()) == Some("session_meta") {
            let id = v
                .pointer("/payload/id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            let cwd = v
                .pointer("/payload/cwd")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            if !id.is_empty() && !cwd.is_empty() {
                head = Some(Head {
                    id,
                    cwd,
                    first_prompt: String::new(),
                });
            }
        }
        let Some(h) = head.as_mut() else {
            continue;
        };
        if !h.first_prompt.is_empty() {
            break; // 摘要已找到，无需继续读。
        }
        // user 消息：response_item → payload.type=message, role=user, content[]。
        let is_user = v.get("type").and_then(|t| t.as_str()) == Some("response_item")
            && v.pointer("/payload/type").and_then(|t| t.as_str()) == Some("message")
            && v.pointer("/payload/role").and_then(|r| r.as_str()) == Some("user");
        if !is_user {
            continue;
        }
        let text = user_text(v.pointer("/payload/content"));
        let t = text.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with('<') {
            continue;
        }
        h.first_prompt = sanitize_summary(t);
    }
    head
}

/// content 数组 → input_text/text 块拼接。
fn user_text(content: Option<&serde_json::Value>) -> String {
    let Some(items) = content.and_then(|c| c.as_array()) else {
        return String::new();
    };
    items
        .iter()
        .filter_map(|i| {
            let ty = i.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if matches!(ty, "input_text" | "text") {
                i.get("text").and_then(|t| t.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 摘要消毒：压空白、截 SUMMARY_CHARS 字符加省略号（同 claude 扫描器）。
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
            "imagent_codex_sess_{}_{}_{}",
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

    /// 写一个 rollout（sessions/<date>/ 下）。
    fn write_rollout(root: &Path, date: &str, uuid: &str, cwd: &str, user_lines: &[String]) {
        let dir = root.join("sessions").join(date);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-08-15T00-00-00-{uuid}.jsonl"));
        let mut lines = vec![serde_json::json!({
            "timestamp": "2026-08-15T00:00:00.000Z",
            "type": "session_meta",
            "payload": { "id": uuid, "cwd": cwd, "originator": "codex_exec" }
        })
        .to_string()];
        lines.extend(user_lines.iter().cloned());
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    }

    fn user_msg(text: &str) -> String {
        serde_json::json!({
            "timestamp": "2026-08-15T00:00:01.000Z",
            "type": "response_item",
            "payload": {
                "type": "message", "role": "user",
                "content": [ { "type": "input_text", "text": text } ]
            }
        })
        .to_string()
    }

    #[test]
    fn lists_matching_cwd_with_summary() {
        let root = tmp_root("basic");
        let wd = "/tmp/proj-a";
        write_rollout(
            &root,
            "2026/08/15",
            "uuid-aaa",
            wd,
            &[
                // 首条 user 是 AGENTS.md 注入（# 开头）→ 跳过。
                user_msg("# AGENTS.md instructions for /tmp/proj-a"),
                user_msg("帮我修这个 bug"),
            ],
        );
        // 其它 cwd 的会话不列出。
        write_rollout(
            &root,
            "2026/08/15",
            "uuid-bbb",
            "/tmp/other",
            &[user_msg("无关")],
        );

        let list = list_local_sessions(&root, Path::new(wd), 10);
        assert_eq!(list.len(), 1, "只列 cwd 匹配的: {list:?}");
        assert_eq!(list[0].session_id, "uuid-aaa");
        assert_eq!(list[0].first_prompt, "帮我修这个 bug");
        assert_eq!(list[0].cwd.as_deref(), Some(wd));
    }

    #[test]
    fn missing_dir_returns_empty() {
        assert!(list_local_sessions(&tmp_root("missing"), Path::new("/x"), 10).is_empty());
    }

    #[test]
    fn limit_and_partial_garbage_tolerated() {
        let root = tmp_root("limit");
        let wd = "/tmp/proj-c";
        write_rollout(&root, "2026/08/15", "u1", wd, &[user_msg("任务一")]);
        write_rollout(&root, "2026/08/14", "u2", wd, &[user_msg("任务二")]);
        // 损坏文件（非 JSON 行 + 无 meta）容忍跳过。
        let dir = root.join("sessions").join("2026/08/13");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("rollout-x.jsonl"), b"not json at all\n").unwrap();

        let all = list_local_sessions(&root, Path::new(wd), 10);
        assert_eq!(all.len(), 2, "损坏文件跳过: {all:?}");
        assert_eq!(
            list_local_sessions(&root, Path::new(wd), 1).len(),
            1,
            "limit 生效"
        );
    }
}
