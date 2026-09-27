//! 本机会话扫描共享骨架（claude / codex 两 crate 的 `/resume` 扫描公共部分，
//! v13 P2-12 还债批抽取）。
//!
//! `crates/claude/src/sessions.rs`（`~/.claude/projects/<编码>/<uuid>.jsonl`）与
//! `crates/codex/src/sessions.rs`（`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`）
//! 的**文件头读取 + 摘要消毒**逻辑逐字同构（HEAD_CAP / SUMMARY_CHARS /
//! sanitize_summary / 容错逐行 JSON 解析），此前两份各自漂移。目录布局与
//! 头部格式（claude 的 user/message.content vs codex 的 session_meta/
//! response_item）差异大、强行泛型化会把两边的特殊判断塞进一个别扭的回调
//! 接口——故只下沉真正同构的部分到本模块，布局遍历与格式解析留在各自 crate。
//!
//! 全部纯函数 + 容错（任何异常按「无摘要」处理，不影响列出会话）。

use std::path::Path;

/// 单文件头部最多读的字节数（首行元数据 + 前几条消息；cap 防大文件全读）。
pub const HEAD_CAP: usize = 64 * 1024;

/// 摘要长度上限（char 计）。
pub const SUMMARY_CHARS: usize = 60;

/// 摘要消毒：压空白（含换行）、截 [`SUMMARY_CHARS`] 字符加省略号。
pub fn sanitize_summary(s: &str) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > SUMMARY_CHARS {
        format!("{}…", flat.chars().take(SUMMARY_CHARS).collect::<String>())
    } else {
        flat
    }
}

/// 目录条目列表（读取失败返回空——目录不存在是正常路径，不报错）。
pub fn dir_entries(dir: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// 读文件头部（≤ [`HEAD_CAP`] 字节）并逐行容错解析为 JSON 值序列。
///
/// - 打不开 / 读失败 → `None`（调用方按「无头」处理：跳过或无摘要列出）；
/// - 单行解析失败 → 跳过该行（损坏行不影响其余行的提取）；
/// - 不保证行完整：恰在 [`HEAD_CAP`] 处截断的末行可能是半行，解析失败自然
///   被跳过（与两 crate 原实现一致）。
pub fn read_head_values(path: &Path) -> Option<Vec<serde_json::Value>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; HEAD_CAP];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(
        buf.split(|&b| b == b'\n')
            .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_file(tag: &str, content: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "imagent_session_scan_{}_{}_{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn sanitize_flattens_and_truncates() {
        let long = "长".repeat(80) + "\n带换行 tail";
        let s = sanitize_summary(&long);
        assert!(s.chars().count() <= SUMMARY_CHARS + 1, "截断: {s}");
        assert!(s.ends_with('…'));
        assert!(!s.contains('\n'), "换行压平: {s}");
        assert_eq!(sanitize_summary("  a \n b "), "a b");
    }

    #[test]
    fn head_values_parse_tolerantly() {
        let p = tmp_file(
            "head",
            b"{\"type\":\"a\"}\nnot json\n{\"type\":\"b\"}\n{\"trunc",
        );
        let vals = read_head_values(&p).expect("应读出头");
        let types: Vec<&str> = vals
            .iter()
            .filter_map(|v| v.get("type").and_then(|t| t.as_str()))
            .collect();
        assert_eq!(types, vec!["a", "b"], "损坏行跳过、截断行丢弃");
        // 不存在的文件 → None。
        assert!(read_head_values(Path::new("/nonexistent/x.jsonl")).is_none());
    }

    #[test]
    fn dir_entries_missing_dir_is_empty() {
        assert!(dir_entries(Path::new("/nonexistent-dir-xyz")).is_empty());
    }
}
