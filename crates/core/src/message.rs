//! 出站消息分片工具（platform 无关）。
//!
//! [`split_message`] 按 Unicode char 上限贪心切分文本，优先在自然边界
//! （换行 > 空格）断开，保证不切断 UTF-8 字符。供各 `Platform` 的 `send_text`
//! 在内部按各自的协议长度上限使用。

/// 按 `max_len`（**Unicode char 数**，非字节数）贪心切分 `text`。
///
/// 规则：
/// - `max_len == 0` 或 `text` 的 char 数 `<= max_len` → 返回 `vec![text.to_string()]`（不分片）。
/// - 否则切成多段，每段 char 数 `<= max_len`：
///   1. 取前 `max_len` 个 char 作为当前窗口；
///   2. 在窗口内**向前**找最后一个 `\n` 作切点；找不到则找最后一个空格（` `）；
///      再找不到（连续超长 token，如一行超长无空格代码）→ 按 `max_len` 个 char 硬切；
///   3. 切点前的入列，剩余部分进入下一轮。
/// - 切点处的分隔符（`\n` / 空格）**跟在前一片末尾**（前片含分隔符）。
/// - 保证每轮至少消费 1 个 char（不会死循环）。
/// - 空串 `""` → 返回 `vec![String::new()]`。
///
/// 不切断多字节 UTF-8 字符（全程按 `char` 边界操作）。所有片按顺序拼接后内容与原文
/// **完全相等**（分隔符不丢弃）。
pub fn split_message(text: &str, max_len: usize) -> Vec<String> {
    // 0 或未超上限：整条返回（空串走这里得 vec![""]）。
    let chars: Vec<char> = text.chars().collect();
    if max_len == 0 || chars.len() <= max_len {
        return vec![text.to_string()];
    }

    let mut result = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let end = (start + max_len).min(chars.len());
        // 剩余全部可入列 → 收尾。
        if end == chars.len() {
            result.push(String::from_iter(&chars[start..end]));
            break;
        }

        // 在窗口 chars[start..end] 内向前找自然边界：换行优先，其次空格。
        let window = &chars[start..end];
        let cut_rel = window
            .iter()
            .rposition(|&c| c == '\n')
            .or_else(|| window.iter().rposition(|&c| c == ' '))
            .unwrap_or(max_len - 1); // 硬切：窗口末尾

        // 切点字符（含）跟入当前片，保证每轮至少消费 1 个 char。
        let cut_abs = start + cut_rel + 1;
        result.push(String::from_iter(&chars[start..cut_abs]));
        start = cut_abs;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_returns_one_empty() {
        assert_eq!(split_message("", 10), vec!["".to_string()]);
    }

    #[test]
    fn under_limit_no_split() {
        assert_eq!(split_message("hello", 10), vec!["hello".to_string()]);
    }

    #[test]
    fn exactly_limit_no_split() {
        assert_eq!(split_message("hello", 5), vec!["hello".to_string()]);
    }

    #[test]
    fn zero_max_len_no_split() {
        // 0 视为不分片。
        assert_eq!(split_message("hello", 0), vec!["hello".to_string()]);
    }

    #[test]
    fn splits_on_newline_boundary() {
        // 换行符跟在前一片末尾。
        let parts = split_message("line1\nline2\nline3", 12);
        assert_eq!(
            parts,
            vec!["line1\nline2\n".to_string(), "line3".to_string()]
        );
        for p in &parts {
            assert!(p.chars().count() <= 12);
        }
    }

    #[test]
    fn splits_on_space_when_no_newline() {
        // 空格跟在前一片末尾。
        let parts = split_message("aaaa bbbb cccc", 10);
        assert_eq!(parts, vec!["aaaa bbbb ".to_string(), "cccc".to_string()]);
        for p in &parts {
            assert!(p.chars().count() <= 10);
        }
    }

    #[test]
    fn hard_split_long_token_no_space() {
        let text = "a".repeat(100);
        let parts = split_message(&text, 10);
        assert_eq!(parts.len(), 10);
        for p in &parts {
            assert_eq!(p.chars().count(), 10);
        }
        // 内容完整保留。
        assert_eq!(parts.join(""), text);
    }

    #[test]
    fn does_not_break_utf8_chinese() {
        let text = "中文测试".repeat(50); // 200 char
        let parts = split_message(&text, 10);
        assert_eq!(parts.len(), 20);
        for p in &parts {
            assert_eq!(p.chars().count(), 10);
        }
        // 字节不切断：每片本身是合法 UTF-8（String 天然保证），且拼接 == 原文。
        assert_eq!(parts.join(""), text);
    }

    #[test]
    fn does_not_break_emoji() {
        // emoji 是多字节但 1 个 Unicode scalar（部分是代理对，此处取 BMP 外单 char）。
        let text = "😀😁😂🤣😃😄😅😆😇😀😁😂🤣😃😄😅😆😇😀😁"; // 20 char
        let parts = split_message(text, 10);
        assert_eq!(parts.len(), 2);
        for p in &parts {
            assert!(p.chars().count() <= 10);
        }
        assert_eq!(parts.join(""), text);
    }

    #[test]
    fn total_content_preserved() {
        // 随机混合：换行 / 空格 / 中文 / emoji / 超长 token，拼接必须 == 原文。
        let mut text = String::new();
        text.push_str("第一行内容\n");
        text.push_str("second line with spaces\n");
        text.push_str(&"a".repeat(50));
        text.push(' ');
        text.push_str("中文混合😀😁😂结尾");

        let parts = split_message(&text, 20);
        // 每片 ≤ 上限。
        for p in &parts {
            assert!(
                p.chars().count() <= 20,
                "片超长: {} ({}chars)",
                p,
                p.chars().count()
            );
        }
        // 完整保留（分隔符不丢）。
        assert_eq!(parts.join(""), text);
    }
}
