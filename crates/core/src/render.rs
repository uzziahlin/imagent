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

/// diff 文本行数上限（超过 → 截断，见 [`clamp_diff`]）。
const DIFF_MAX_LINES: usize = 200;
/// diff 文本字节上限（8KB；与行数上限任一超限即截断）。
const DIFF_MAX_BYTES: usize = 8 * 1024;
/// 截断尾行（不加 diff 行前缀——它是提示行，不是内容行）。
const DIFF_TRUNCATION_MARKER: &str = "…（已截断）";

/// #7 diff 视图：从工具调用 input JSON 合成 unified 风格 diff 文本
/// （`+`/`-` 前缀行，无 hunk 头要求；SPA 折叠区展示，飞书卡不消费）。
///
/// 白名单内且 input JSON 解析出预期字段才返回 Some，否则 None
/// （**纯函数绝不 Err**：解析失败/字段缺失/类型不符/空内容一律 None）。
/// 与 [`tool_summary`] 同签名同消费点（round.rs ToolUse 分支构建 ToolCall）。
///
/// - claude `Edit` `{file_path, old_string, new_string}`：old 每行 `- ` 前缀 +
///   new 每行 `+ `；
/// - claude `MultiEdit` `{file_path, edits: [{old_string, new_string}]}`：逐段
///   拼接（段内顺序同 Edit）；
/// - claude `Write` `{file_path, content}`：content 每行 `+ `（空串 = 零行，
///   清空文件不产假空行）；
/// - codex `fileChange`：input 由 app_backend 映射为 `{"path", "diff"}`
///   （diff = 协议 FileUpdateChange.diff 拼接，0.166 schema 实证必带——见
///   codex map_item_completed 注释），此处透传；
/// - 其余工具 / 坏 JSON → None。
pub fn synth_diff_from_tool_input(tool: &str, input: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    let get_str = |k: &str| v.get(k).and_then(serde_json::Value::as_str);
    let mut lines: Vec<String> = Vec::new();
    match tool {
        "Edit" => {
            lines.extend(prefixed_lines(get_str("old_string")?, "- "));
            lines.extend(prefixed_lines(get_str("new_string")?, "+ "));
        }
        "MultiEdit" => {
            let edits = v.get("edits").and_then(serde_json::Value::as_array)?;
            for e in edits {
                let old = e.get("old_string").and_then(serde_json::Value::as_str)?;
                let new = e.get("new_string").and_then(serde_json::Value::as_str)?;
                lines.extend(prefixed_lines(old, "- "));
                lines.extend(prefixed_lines(new, "+ "));
            }
        }
        "Write" => {
            lines.extend(prefixed_lines(get_str("content")?, "+ "));
        }
        // codex fileChange：协议自带 unified diff，直接透传（截断在 clamp 统一）。
        "fileChange" => {
            let d = get_str("diff")?;
            if !d.is_empty() {
                lines.push(d.to_string());
            }
        }
        _ => return None,
    }
    let diff = lines.join("\n");
    (!diff.is_empty()).then(|| clamp_diff(diff))
}

/// 多行内容逐行加前缀；空串 = 零行（无假空行）。
fn prefixed_lines(s: &str, prefix: &str) -> Vec<String> {
    if s.is_empty() {
        Vec::new()
    } else {
        s.split('\n').map(|l| format!("{prefix}{l}")).collect()
    }
}

/// diff 超限截断：>200 行先按行截，仍 >8KB 再按 UTF-8 安全边界截字节；
/// 被截断时追加尾行「…（已截断）」。未超限原样返回。
fn clamp_diff(diff: String) -> String {
    let over_lines = diff.lines().count() > DIFF_MAX_LINES;
    let over_bytes = diff.len() > DIFF_MAX_BYTES;
    if !over_lines && !over_bytes {
        return diff;
    }
    let mut out = if over_lines {
        diff.lines()
            .take(DIFF_MAX_LINES)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        diff
    };
    if out.len() > DIFF_MAX_BYTES {
        let mut end = DIFF_MAX_BYTES;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out.push('\n');
    out.push_str(DIFF_TRUNCATION_MARKER);
    out
}

/// 工具状态图标：✅ 已完成 / ⏳ 执行中。
pub fn tool_status_icon(done: bool) -> &'static str {
    if done {
        "✅"
    } else {
        "⏳"
    }
}

/// 卡片（markdown）形态的工具行：`✅ ⚡ **Bash** — git status`。
pub fn tool_card_line(t: &ToolCall) -> String {
    let icon = tool_status_icon(t.done);
    let type_icon = tool_type_icon(&t.name);
    if t.summary.is_empty() {
        format!("{icon} {type_icon} **{}**", t.name)
    } else {
        format!("{icon} {type_icon} **{}** — {}", t.name, t.summary)
    }
}

/// v1.25 工具类型图标（轨迹扫一眼有层次）：按工具名映射，未知归 🔧。
/// 前缀匹配（mcp__* / Task* 族）优先于全名表。
pub fn tool_type_icon(name: &str) -> &'static str {
    if name.starts_with("mcp__") {
        return "🔌";
    }
    if name.starts_with("Task") {
        return "📋";
    }
    match name {
        "Read" | "NotebookRead" => "📖",
        "Edit" | "Write" | "NotebookEdit" => "✏️",
        "Bash" => "⚡",
        "Grep" | "Glob" => "🔍",
        "WebFetch" | "WebSearch" => "🌐",
        "TodoWrite" => "📋",
        _ => "🔧",
    }
}

/// 纯文本形态的工具行（无 markdown 加粗，wecom/ilink 文本消息用）：
/// `✅ Bash — git status`。
pub fn tool_text_line(t: &ToolCall) -> String {
    let icon = tool_status_icon(t.done);
    let type_icon = tool_type_icon(&t.name);
    if t.summary.is_empty() {
        format!("{icon} {type_icon} {}", t.name)
    } else {
        format!("{icon} {type_icon} {} — {}", t.name, t.summary)
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
            id: None,
            diff: None,
        };
        assert_eq!(tool_card_line(&t), "✅ ⚡ **Bash** — git status");
        assert_eq!(tool_text_line(&t), "✅ ⚡ Bash — git status");
        let running = ToolCall {
            name: "Read".into(),
            summary: String::new(),
            done: false,
            id: None,
            diff: None,
        };
        assert_eq!(tool_card_line(&running), "⏳ 📖 **Read**");
    }

    // ---------- #7 diff 视图：synth_diff_from_tool_input 全态 ----------

    #[test]
    fn synth_edit_old_minus_new_plus() {
        let out = synth_diff_from_tool_input(
            "Edit",
            r#"{"file_path":"/a/b.rs","old_string":"let x = 1;\nfoo();","new_string":"let x = 2;"}"#,
        )
        .expect("Edit 应合成");
        // 行序：old 段在前（- 前缀）、new 段在后（+ 前缀），无 hunk 头。
        assert_eq!(out, "- let x = 1;\n- foo();\n+ let x = 2;");
    }

    #[test]
    fn synth_multiedit_concatenates_segments() {
        let out = synth_diff_from_tool_input(
            "MultiEdit",
            r#"{"file_path":"/a.rs","edits":[
                {"old_string":"a","new_string":"b"},
                {"old_string":"c","new_string":"d\ne"}]}"#,
        )
        .expect("MultiEdit 应合成");
        assert_eq!(out, "- a\n+ b\n- c\n+ d\n+ e");
    }

    #[test]
    fn synth_write_all_plus_lines() {
        let out = synth_diff_from_tool_input(
            "Write",
            r#"{"file_path":"/new.rs","content":"first\nsecond"}"#,
        )
        .expect("Write 应合成");
        assert_eq!(out, "+ first\n+ second");
        // 清空文件（content=""）= 零行 → 无内容 → None（不产假空行）。
        assert_eq!(
            synth_diff_from_tool_input("Write", r#"{"file_path":"/f","content":""}"#),
            None,
            "空 content 应 None"
        );
    }

    #[test]
    fn synth_filechange_passes_protocol_diff_through() {
        // codex fileChange 映射 input（app_backend 构造 {"path","diff"}）——
        // diff 为协议 FileUpdateChange.diff 拼接，直接透传。
        let out = synth_diff_from_tool_input(
            "fileChange",
            r#"{"path":"src/main.rs","diff":"--- a/src/main.rs\n+++ b\n-old\n+new"}"#,
        )
        .expect("fileChange 应透传");
        assert_eq!(out, "--- a/src/main.rs\n+++ b\n-old\n+new");
    }

    #[test]
    fn synth_rejects_bad_json_unknown_tool_missing_fields() {
        // 坏 JSON / 非白名单工具 / 字段缺失 / 类型不符 → None（绝不 Err）。
        assert_eq!(synth_diff_from_tool_input("Edit", r#"{"file_path":"#), None);
        assert_eq!(
            synth_diff_from_tool_input("Bash", r#"{"command":"ls"}"#),
            None,
            "非白名单工具应 None"
        );
        assert_eq!(
            synth_diff_from_tool_input("Edit", r#"{"file_path":"/a"}"#),
            None,
            "old_string 缺失应 None"
        );
        assert_eq!(
            synth_diff_from_tool_input("Edit", r#"{"old_string":1,"new_string":"x"}"#),
            None,
            "old_string 非字符串应 None"
        );
        assert_eq!(
            synth_diff_from_tool_input("MultiEdit", r#"{"edits":"no-array"}"#),
            None,
            "edits 非数组应 None"
        );
        assert_eq!(
            synth_diff_from_tool_input("fileChange", r#"{"path":"/a"}"#),
            None,
            "fileChange 缺 diff 应 None"
        );
        // 空串 input / 空对象同样 None。
        assert_eq!(synth_diff_from_tool_input("Edit", ""), None);
        assert_eq!(synth_diff_from_tool_input("Edit", "{}"), None);
    }

    #[test]
    fn synth_truncates_over_200_lines() {
        // join("\\n")：JSON 字符串内的换行转义序列（裸换行非法 JSON）。
        let old = (0..250)
            .map(|i| format!("old{i}"))
            .collect::<Vec<_>>()
            .join("\\n");
        let input = format!(r#"{{"file_path":"/a","old_string":"{old}","new_string":"+tail"}}"#);
        let out = synth_diff_from_tool_input("Edit", &input).expect("超限也应合成");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines.len(),
            DIFF_MAX_LINES + 1,
            "200 行内容 + 1 尾行: {out}"
        );
        assert_eq!(lines.last().copied(), Some(DIFF_TRUNCATION_MARKER));
        assert!(lines[0].starts_with("- old0"));
        assert!(lines[DIFF_MAX_LINES - 1].starts_with("- old199"));
    }

    #[test]
    fn synth_truncates_over_8kb_on_utf8_boundary() {
        // 中文 3 字节/行 × 500 行 × 40 字符（120B/行）≈ 60KB：行数（500>200）
        // 与字节双超，验证先按行截到 200 行（≈24KB）再按字节截到 UTF-8 边界。
        let line = "汉".repeat(40);
        let content = (0..500)
            .map(|i| format!("{line}{i}"))
            .collect::<Vec<_>>()
            .join("\\n");
        let input = format!(r#"{{"file_path":"/a","content":"{content}"}}"#);
        let out = synth_diff_from_tool_input("Write", &input).expect("应合成");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.last().copied(), Some(DIFF_TRUNCATION_MARKER));
        // 尾行之外的主体 ≤ 8KB（UTF-8 边界截断不劈半个字符）。
        let body = lines[..lines.len() - 1].join("\n");
        assert!(body.len() <= DIFF_MAX_BYTES, "主体应 ≤8KB: {}", body.len());
        // 不劈字符：按 char 计数重建字节长度应一致（无部分字符）。
        assert_eq!(body.chars().collect::<String>().len(), body.len());
    }

    #[test]
    fn synth_under_limits_keeps_content_verbatim() {
        // 199 行（<200 行、<8KB）不截断——内容逐行 `+ ` 前缀原样保留。
        let content = (0..199)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\\n");
        let input = format!(r#"{{"file_path":"/a","content":"{content}"}}"#);
        let out = synth_diff_from_tool_input("Write", &input).expect("应合成");
        let expected = (0..199)
            .map(|i| format!("+ l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(out, expected);
        assert!(!out.contains(DIFF_TRUNCATION_MARKER));
    }
}
