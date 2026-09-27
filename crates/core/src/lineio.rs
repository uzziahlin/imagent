//! 带上限的异步按行读取 util（S-5 / B1 / P1-9 的共享实现）。
//!
//! 此前 `backend_common.rs`（CLI 子进程 stdout/stderr）与 `dispatch/socket.rs`
//!（permission socket）各持一份逐字相同的 `read_line_capped`（v13 P3 还债批
//! 去重）；MCP server 的 stdin 读行（`mcp.rs`）原本裸用无上限的 `lines()`，
//! 同批改为走本模块。
//!
//! ## 语义（超长行「读到 `\n` 再丢」——T20 修正）
//!
//! - 行内容（不含行尾 `\n`）≤ `max_bytes` → `Ok(Some(line))`。**保留行尾 `\n`**
//!   （与 `AsyncBufReadExt::lines()` 的去尾行为不同），调用方按需 trim；
//! - 行内容超过 `max_bytes` → **继续消费到该行的 `\n`（或 EOF）后才返回**
//!   `Err(ErrorKind::InvalidInput)`。旧实现停在超限点即返回，同一行的残段
//!   会被调用方当「新行」误解析（半截 JSON 进解析器、半截报文当完整帧）。
//!   现语义下 Err 即「该行已整体丢弃」，调用方 continue/跳过后从下一行
//!   干净恢复。上限在**行粒度**精确判定——旧实现只在跨 `fill_buf` 块累积时
//!   检查，单块内含换行的整行（如短读窗口大的管道/slice reader）会整行
//!   返回、绕过上限；
//! - EOF：无残留 → `Ok(None)`；有未换行残段（未超限）→ `Ok(Some(残段))`；
//! - 其它 Err kind = 真实 IO 错误（管道 EIO 等，持续性），调用方应**终止**
//!   读取——continue 只会对同一错误忙循环空转（B1）。

use std::io;

/// 按字节读一行（保留行尾 `\n`），上限 `max_bytes` 字节（按不含行尾换行的
/// 行内容计）。完整语义见模块级文档。
pub async fn read_line_capped<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> io::Result<Option<String>> {
    use tokio::io::AsyncBufReadExt;
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut over_limit = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            // EOF。超长行在此收尾：整行（无换行的尾段在内）已消费完，按
            // InvalidInput 返回（调用方跳过后下一次读即得干净的 Ok(None)）。
            if over_limit {
                return Err(line_too_long(max_bytes));
            }
            return if buf.is_empty() {
                Ok(None)
            } else {
                Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
            };
        }
        if let Some(nl) = available.iter().position(|&b| b == b'\n') {
            // 行内容（不含 `\n`）= 已积累 buf + 本块换行前部分。行粒度精确
            // 判定上限（含跨块累积与单块整行两种到达形态）。
            if over_limit || buf.len() + nl > max_bytes {
                // 超长行：consume 掉行尾换行后整行丢弃（防残段被当新行）。
                reader.consume(nl + 1);
                return Err(line_too_long(max_bytes));
            }
            buf.extend_from_slice(&available[..=nl]);
            reader.consume(nl + 1);
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
        let n = available.len();
        if !over_limit {
            buf.extend_from_slice(available);
            if buf.len() > max_bytes {
                // 超限即弃缓冲（后续字节只消费不积累——无 `\n` 的超长流
                // 也不能继续吃内存，S-5 的 OOM 防护本体）。
                over_limit = true;
                buf.clear();
            }
        }
        reader.consume(n);
    }
}

/// 超长行统一错误（kind = InvalidInput，调用方以 kind 区分「跳行」与「终止」）。
fn line_too_long(max_bytes: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("line exceeds {max_bytes} bytes (skipped to end of line)"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 普通行（含换行）原样返回（保留 `\n`）。
    #[tokio::test]
    async fn reads_normal_line_with_newline() {
        let bytes: &[u8] = b"{\"conv_id\":\"c1\"}\nextra";
        let mut reader = tokio::io::BufReader::new(bytes);
        let line = read_line_capped(&mut reader, 1024).await.unwrap().unwrap();
        assert_eq!(line, "{\"conv_id\":\"c1\"}\n");
    }

    /// EOF 时无换行的尾段作为最后一行返回。
    #[tokio::test]
    async fn returns_tail_without_newline_at_eof() {
        let bytes: &[u8] = b"tail";
        let mut reader = tokio::io::BufReader::new(bytes);
        let line = read_line_capped(&mut reader, 1024).await.unwrap().unwrap();
        assert_eq!(line, "tail");
        assert_eq!(read_line_capped(&mut reader, 1024).await.unwrap(), None);
    }

    /// 恰在上限内的行正常返回（> max 才拒，边界值本身合法）。
    #[tokio::test]
    async fn line_at_cap_is_accepted() {
        let mut bytes = vec![b'x'; 100];
        bytes.push(b'\n');
        let mut reader = tokio::io::BufReader::new(&bytes[..]);
        let line = read_line_capped(&mut reader, 100).await.unwrap().unwrap();
        assert_eq!(line.len(), 101); // 100 字节 + \n
    }

    /// 超长（无换行直到 EOF）→ Err(InvalidInput)；下一次读得到干净 EOF。
    #[tokio::test]
    async fn oversized_line_without_newline_errors() {
        let bytes: Vec<u8> = vec![b'x'; 1000];
        let mut reader = tokio::io::BufReader::new(&bytes[..]);
        let err = read_line_capped(&mut reader, 100)
            .await
            .expect_err("oversized line must error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(read_line_capped(&mut reader, 100).await.unwrap(), None);
    }

    /// 【T20 回归：超长行「读到 `\n` 再丢」】超长行后跟一条合法行——首次读
    /// 返回 Err 且**消费到换行为止**，第二次读必须拿到完整的合法行（旧实现
    /// 停在超限点返回，合法行的前半会被当残段「新行」误解析）。
    #[tokio::test]
    async fn oversized_line_drains_to_newline_then_next_line_intact() {
        let mut bytes = vec![b'x'; 300];
        bytes.push(b'\n');
        bytes.extend_from_slice(b"{\"ok\":true}\n");
        let mut reader = tokio::io::BufReader::new(&bytes[..]);
        let err = read_line_capped(&mut reader, 100)
            .await
            .expect_err("oversized line must error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let next = read_line_capped(&mut reader, 1024)
            .await
            .unwrap()
            .expect("下一行应完整可读，而非残段");
        assert_eq!(next, "{\"ok\":true}\n");
    }

    /// 换行与超限点同批到达（fill_buf 一次返回「超限内容 + 换行 + 后续行」）：
    /// Err 须 consume 到该换行，后续行完整。
    #[tokio::test]
    async fn oversized_line_with_newline_in_same_buffer_drains() {
        let mut bytes = vec![b'x'; 250];
        bytes.push(b'\n');
        bytes.extend_from_slice(b"second\n");
        let mut reader = tokio::io::BufReader::new(&bytes[..]);
        assert!(read_line_capped(&mut reader, 100).await.is_err());
        let next = read_line_capped(&mut reader, 1024).await.unwrap().unwrap();
        assert_eq!(next, "second\n");
    }

    /// 两条普通行顺序读取互不吞并。
    #[tokio::test]
    async fn consecutive_lines_read_in_order() {
        let bytes: &[u8] = b"a\nb\n";
        let mut reader = tokio::io::BufReader::new(bytes);
        assert_eq!(
            read_line_capped(&mut reader, 16).await.unwrap().unwrap(),
            "a\n"
        );
        assert_eq!(
            read_line_capped(&mut reader, 16).await.unwrap().unwrap(),
            "b\n"
        );
        assert_eq!(read_line_capped(&mut reader, 16).await.unwrap(), None);
    }
}
