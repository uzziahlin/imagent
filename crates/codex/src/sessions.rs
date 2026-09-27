//! 本机 Codex CLI 会话扫描（统一 `/resume`，P5）。
//!
//! Codex 把每个会话存为 `~/.codex/sessions/YYYY/MM/DD/rollout-<时间>-<uuid>.jsonl`：
//! - 文件名内嵌 thread uuid（P2-12：`session_exists` 据此按文件名 O(1) 定位，
//!   免逐文件读头全扫）；
//! - 首行 `{"type":"session_meta","payload":{"id":"<uuid>","cwd":"…",…}}`——
//!   session id = `payload.id`（与 `codex exec --json` 报告的 thread_id 同源），
//!   cwd 用于按 workdir 过滤（目录按日期嵌套、不含项目信息，只能读文件头部判定）；
//! - user 消息在 `{"type":"response_item","payload":{"type":"message","role":"user",
//!   "content":[{"type":"input_text","text":"…"}]}}` 行；首条常是 AGENTS.md 注入
//!   （`#`/`<` 开头）需跳过。
//!
//! 全部纯函数 + 容错解析（异常按无摘要处理，不 panic）；目录不存在返回空
//!（codex 未用过 → `/resume` 退化为纯 IM 历史）。为控制开销，只按 mtime 倒序
//! 检查最近 [`SCAN_CAP`] 个文件。文件头读取与摘要消毒的公共骨架在
//! [`imagent_core::session_scan`]（与 claude 扫描器共享，v13 P2-12 去重）。

use std::path::{Path, PathBuf};

use imagent_core::session_scan::{dir_entries, read_head_values, sanitize_summary};
use imagent_core::LocalSession;

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
    // mtime 倒序；同 mtime 以 path 倒序决胜（P2-12：纯 mtime 比较在并列时
    // 依赖 read_dir 返回序，跨调用不稳定——/resume 列表序号会漂移错位）。
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
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

/// 该 thread id 是否在本机 rollout 存储中真实存在（B11 幽灵会话预检）。
///
/// 与 claude 的 `session_exists` 同语义：失败轮次的 stream 事件仍可能携带
/// thread_id，落库后成为「幽灵会话」——下次 `codex exec resume <id>` 必然
/// 失败且每轮再产新幽灵 id（毒化循环）。run 前用本函数预检，幽灵即弃用
/// 续接、开新会话。
///
/// codex 的 rollout 不按 workdir 编码分目录（只按日期），thread id 是 uuid
/// 全局唯一，故只按 id 匹配、不做 cwd 过滤（误判为幽灵会丢上下文，代价
/// 高于多扫几个文件）。
///
/// P2-12 定位策略（两级）：rollout 文件名内嵌 thread uuid
/// （`rollout-<时间>-<uuid>.jsonl`）——**快路径**按文件名后缀一次目录列举匹配
/// （零文件打开/头部读；旧实现逐文件读 ≤64KB 头 + JSON 解析，数月重度使用后
/// 每条消息都触发数千 rollout 全扫）；miss 才**退全扫**（读头比对
/// session_meta.id）兜住文件名形态差异/手工重命名。读失败/目录不存在按
/// 「不存在」处理——此时 resume 也必然失败，弃用续接与 claude 预检的行为
/// 口径一致。
pub(crate) fn session_exists(codex_dir: &Path, thread_id: &str) -> bool {
    let root = codex_dir.join("sessions");
    // 快路径：文件名后缀 `-{thread_id}.jsonl`（uuid 全局唯一，后缀命中即存在）。
    // 空 id 不走快路径（`-.jsonl` 后缀会误匹配奇异文件名；全扫读头的 id 恒
    // 非空，空 id 在慢路径自然判 false）。
    if !thread_id.is_empty() {
        let suffix = format!("-{thread_id}.jsonl");
        for y in dir_entries(&root) {
            for m in dir_entries(&y) {
                for d in dir_entries(&m) {
                    for f in dir_entries(&d) {
                        if f.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.ends_with(&suffix))
                        {
                            return true;
                        }
                    }
                }
            }
        }
    }
    // 慢路径（全扫）：文件名未命中时读头比对 session_meta.id（现有行为兜底）。
    for y in dir_entries(&root) {
        for m in dir_entries(&y) {
            for d in dir_entries(&m) {
                for f in dir_entries(&d) {
                    if f.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                        continue;
                    }
                    if let Some(h) = read_head(&f) {
                        if h.id == thread_id {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// 文件头部的结构化信息。
struct Head {
    id: String,
    cwd: String,
    first_prompt: String,
}

/// 读文件头部（≤ [`imagent_core::session_scan::HEAD_CAP`]，共享骨架）：首行
/// session_meta 取 id/cwd；随后逐行找首条可展示的 user 消息（跳过 `#`/`<`
/// 开头的 AGENTS.md / 命令注入）。
fn read_head(path: &Path) -> Option<Head> {
    let mut head: Option<Head> = None;
    for v in read_head_values(path)? {
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

// 摘要消毒（sanitize_summary）/文件头读取/dir_entries 已上移
// imagent_core::session_scan 共享（v13 P2-12 去重）。

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

    /// B11：幽灵会话预检——thread id 存在于 rollout 存储才允许 resume。
    #[test]
    fn session_exists_matches_id_only() {
        let root = tmp_root("exists");
        write_rollout(
            &root,
            "2026/08/15",
            "uuid-live",
            "/tmp/proj-a",
            &[user_msg("hi")],
        );
        // cwd 不同也不影响：thread id 是全局唯一 uuid，只按 id 匹配。
        write_rollout(
            &root,
            "2026/08/14",
            "uuid-other",
            "/tmp/other",
            &[user_msg("x")],
        );

        assert!(session_exists(&root, "uuid-live"));
        assert!(session_exists(&root, "uuid-other"));
        // 幽灵 id（存储里没有）与目录不存在都返回 false。
        assert!(!session_exists(&root, "uuid-ghost"));
        assert!(!session_exists(&tmp_root("exists-none"), "uuid-live"));
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

    /// P2-12 快路径：文件名内嵌 uuid 即命中——头部损坏/为空（读不出
    /// session_meta）也判存在。旧实现依赖读头比对，头部损坏的 rollout 会被
    /// 误判幽灵会话（弃用续接丢上下文）。
    #[test]
    fn session_exists_filename_match_skips_head_read() {
        let root = tmp_root("fastpath");
        let dir = root.join("sessions").join("2026/08/15");
        std::fs::create_dir_all(&dir).unwrap();
        // 文件名含 uuid，内容是垃圾（无 session_meta 可读）。
        std::fs::write(
            dir.join("rollout-2026-08-15T00-00-00-uuid-nameonly.jsonl"),
            b"not json at all\n",
        )
        .unwrap();
        assert!(
            session_exists(&root, "uuid-nameonly"),
            "文件名命中即存在（快路径，不依赖读头）"
        );
    }

    /// P2-12 慢路径兜底：文件名**不含** thread id（手工重命名/布局差异）但头部
    /// session_meta.id 匹配——快路径 miss 后全扫仍能找到，不误判幽灵。
    #[test]
    fn session_exists_fallback_matches_id_inside_head() {
        let root = tmp_root("fallback");
        let dir = root.join("sessions").join("2026/08/15");
        std::fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({
            "type": "session_meta",
            "payload": { "id": "uuid-in-head", "cwd": "/tmp/p" }
        })
        .to_string();
        // 文件名与 id 无关（renamed.jsonl）——只能靠读头命中。
        std::fs::write(dir.join("renamed.jsonl"), format!("{meta}\n")).unwrap();
        assert!(
            session_exists(&root, "uuid-in-head"),
            "文件名 miss 时全扫兜底命中"
        );
    }

    /// P2-12：同 mtime 并列时以 path 决胜（确定性排序）——纯 mtime 比较依赖
    /// read_dir 返回序，跨调用不稳定，/resume 序号会漂移错位。
    #[test]
    fn same_mtime_tiebreak_orders_by_path_deterministically() {
        let root = tmp_root("tiebreak");
        let wd = "/tmp/proj-tie";
        write_rollout(&root, "2026/08/15", "u-b", wd, &[user_msg("b")]);
        write_rollout(&root, "2026/08/15", "u-a", wd, &[user_msg("a")]);
        // 强制同 mtime（FileTimes 精确设置，绕开写入时序）。
        let same = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let times = std::fs::FileTimes::new().set_modified(same);
        for u in ["u-a", "u-b"] {
            let p = root
                .join("sessions")
                .join("2026/08/15")
                .join(format!("rollout-2026-08-15T00-00-00-{u}.jsonl"));
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_times(times)
                .unwrap();
        }
        // 多调几次断言顺序稳定（path 倒序决胜：u-b 在 u-a 前）。
        for _ in 0..3 {
            let list = list_local_sessions(&root, Path::new(wd), 10);
            let ids: Vec<&str> = list.iter().map(|s| s.session_id.as_str()).collect();
            assert_eq!(ids, vec!["u-b", "u-a"], "同 mtime 按 path 倒序决胜");
        }
    }
}
