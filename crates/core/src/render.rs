//! 工具调用展示摘要（P8-1：对标 lcab 的 toolHeaderText/summarizeInput）。
//!
//! 把 backend 的 `(tool_name, input JSON)` 压成**人可读的单行摘要**——
//! `Bash` 取 command、`Read/Write/Edit` 取 file_path、`WebFetch` 取 url……
//! 替代此前「裸 JSON 截断 40 字符」的展示（`{"command":"git st…`）。
//! 流式卡片工具行 / 审批卡详情 / 纯文本工具摘要共用本模块。

use crate::types::ToolCall;

/// 摘要单行上限（char；lcab HEADER_SUMMARY_MAX=80 同款，防长命令撑爆卡片头）。
const SUMMARY_MAX: usize = 80;

/// 生成工具调用的单行摘要（不含工具名；解析不出有效字段时回退原始串压平）。
///
/// 覆盖三家 backend 的工具命名：claude 的 PascalCase（Bash/Read/Edit/…）、
/// codex 的 snake_case（shell/read_file/apply_patch）、gemini 沿用 claude 风格。
pub fn tool_summary(name: &str, input_json: &str) -> String {
    let v: Option<serde_json::Value> = serde_json::from_str(input_json).ok();
    // 取 JSON 顶层字符串字段 → 压平空白 → 截断。
    let get = |key: &str| -> Option<String> {
        let s = v.as_ref()?.get(key)?.as_str()?;
        let one_line = collapse_ws(s);
        (!one_line.is_empty()).then(|| truncate_chars(&one_line, SUMMARY_MAX))
    };
    let pick = |keys: &[&str]| -> Option<String> { keys.iter().find_map(|k| get(k)) };
    match name {
        "Bash" | "shell" => pick(&["command"]),
        "Read" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "read_file" | "apply_patch" => {
            pick(&["file_path", "path"])
        }
        "Grep" | "grep" => match (get("pattern"), get("path")) {
            (Some(p), Some(d)) => Some(format!("{p} in {}", truncate_chars(&d, 30))),
            (Some(p), None) => Some(p),
            _ => pick(&["query"]),
        },
        "Glob" => pick(&["pattern"]),
        "WebFetch" | "web_fetch" => pick(&["url"]),
        "WebSearch" | "web_search" => pick(&["query"]),
        "Task" | "Agent" | "agent" => pick(&["description", "prompt"]),
        "TodoWrite" | "update_plan" => v
            .as_ref()
            .and_then(|v| v.get("todos"))
            .and_then(|t| t.as_array())
            .map(|a| format!("{} 项待办", a.len())),
        _ => pick(&[
            "command",
            "file_path",
            "path",
            "url",
            "query",
            "pattern",
            "description",
        ]),
    }
    .unwrap_or_else(|| truncate_chars(&collapse_ws(input_json), SUMMARY_MAX))
}

/// 工具状态图标：✅ 已完成 / ⏳ 执行中。
pub fn tool_status_icon(done: bool) -> &'static str {
    if done {
        "✅"
    } else {
        "⏳"
    }
}

/// 卡片（markdown）形态的工具行：`✅ **Bash** — git status`。
pub fn tool_card_line(t: &ToolCall) -> String {
    let icon = tool_status_icon(t.done);
    if t.summary.is_empty() {
        format!("{icon} **{}**", t.name)
    } else {
        format!("{icon} **{}** — {}", t.name, t.summary)
    }
}

/// 纯文本形态的工具行（无 markdown 加粗，wecom/ilink 文本消息用）：
/// `✅ Bash — git status`。
pub fn tool_text_line(t: &ToolCall) -> String {
    let icon = tool_status_icon(t.done);
    if t.summary.is_empty() {
        format!("{icon} {}", t.name)
    } else {
        format!("{icon} {} — {}", t.name, t.summary)
    }
}

/// 压平空白：连续空白（含换行/缩进）折成单个空格（命令与文件路径压成一行）。
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 按 char 截断（避免半截 UTF-8），超长补 `…`。
pub(crate) fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_per_tool_shape() {
        assert_eq!(
            tool_summary("Bash", r#"{"command":"git status","timeout":500}"#),
            "git status"
        );
        assert_eq!(
            tool_summary("Read", r#"{"file_path":"/a/b/main.rs"}"#),
            "/a/b/main.rs"
        );
        // codex 命名；command 为数组（非字符串）→ 通用字段取不到 → 压平回退。
        let arr = tool_summary("shell", r#"{"command":["cargo","test"]}"#);
        assert!(arr.contains("cargo"), "回退应含原始内容: {arr}");
        assert_eq!(
            tool_summary("Grep", r#"{"pattern":"TODO","path":"/a/b/c/src"}"#),
            "TODO in /a/b/c/src"
        );
        assert_eq!(
            tool_summary("WebFetch", r#"{"url":"https://x.y/z"}"#),
            "https://x.y/z"
        );
        assert_eq!(
            tool_summary(
                "TodoWrite",
                r#"{"todos":[{"content":"a"},{"content":"b"}]}"#
            ),
            "2 项待办"
        );
    }

    #[test]
    fn summary_multiline_command_collapsed() {
        assert_eq!(
            tool_summary(
                "Bash",
                "{\n  \"command\": \"cargo build   --release\",\n  \"workdir\": \".\"\n}"
            ),
            "cargo build --release"
        );
    }

    /// 截断的 JSON（审批链路 input_summary 上限 2000 字符）解析失败 → 压平回退，
    /// 不 panic、不空串。
    #[test]
    fn summary_truncated_json_falls_back_to_raw() {
        let out = tool_summary("Bash", r#"{"command":"echo '"#);
        assert!(out.contains("echo"), "回退应含原始内容: {out}");
    }

    /// 未知工具：首个通用字段（command/file_path/…）。
    #[test]
    fn summary_unknown_tool_picks_generic_field() {
        assert_eq!(
            tool_summary("SomeMcpTool", r#"{"file_path":"/tmp/x"}"#),
            "/tmp/x"
        );
        assert_eq!(tool_summary("Whatever", r#"{"query":"hello"}"#), "hello");
    }

    #[test]
    fn summary_long_input_truncated() {
        let long = "x".repeat(300);
        let out = tool_summary("Bash", &format!(r#"{{"command":"{long}"}}"#));
        assert!(out.chars().count() <= SUMMARY_MAX, "超长应截断: {out}");
        assert!(out.ends_with('…'));
    }

    #[test]
    fn card_and_text_lines() {
        let t = ToolCall {
            name: "Bash".into(),
            summary: "git status".into(),
            done: true,
        };
        assert_eq!(tool_card_line(&t), "✅ **Bash** — git status");
        assert_eq!(tool_text_line(&t), "✅ Bash — git status");
        let running = ToolCall {
            name: "Read".into(),
            summary: String::new(),
            done: false,
        };
        assert_eq!(tool_card_line(&running), "⏳ **Read**");
    }
}
