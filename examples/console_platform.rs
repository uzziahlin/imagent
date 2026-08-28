//! 示例：自定义实现 `Platform` trait（一个把标准输入/输出当作 IM 的最简平台）。
//!
//! 与 `echo_backend.rs`（演示 `Backend`）对称，演示 imagent 的**平台双抽象**——
//! 任何实现 `Platform` 的 IM 适配器都可注入网关。真实平台见 `crates/{ilink,wecom}`。
//!
//! 运行：`echo '你好' | cargo run --example console_platform`
//! 或交互：`cargo run --example console_platform` 后输入一行文本回车。

use async_trait::async_trait;
use imagent_core::{ConvId, InboundMessage, MediaRef, Platform, ReplyHint, Result, UserId};
use tokio::io::{AsyncBufReadExt, BufReader};

/// 把标准输入/输出当作 IM 的最简 `Platform`（无网络、无副作用，演示用）。
pub struct ConsolePlatform;

#[async_trait]
impl Platform for ConsolePlatform {
    /// 从 stdin 读一行作为一条入站消息。
    async fn recv(&self) -> Result<InboundMessage> {
        let mut stdin = BufReader::new(tokio::io::stdin()).lines();
        let text = match stdin.next_line().await? {
            Some(line) => line,
            None => {
                return Err(
                    std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "stdin closed").into(),
                );
            }
        };
        Ok(InboundMessage {
            conv_id: ConvId("console:local".to_string()),
            sender: UserId("local-user".to_string()),
            text: Some(text),
            media: Vec::new(),
            media_errors: Vec::new(),
            mentions: Vec::new(),
            mentioned_bot: false,
            ask_req: None,
            reply_to: None,
            source_msg_id: None,
            control: None,
            reply_hint: ReplyHint::None,
        })
    }

    /// 把回复打印到 stdout（真实平台会发到 IM）。
    async fn send_text(&self, _conv: &ConvId, text: &str, _hint: &ReplyHint) -> Result<()> {
        println!("[reply] {text}");
        Ok(())
    }

    async fn send_media(&self, _conv: &ConvId, media: &MediaRef, _hint: &ReplyHint) -> Result<()> {
        println!("[media:{}] {}", media.kind, media.url);
        Ok(())
    }

    async fn send_typing(&self, _conv: &ConvId, _hint: &ReplyHint) -> Result<()> {
        println!("[typing...]");
        Ok(())
    }

    fn name(&self) -> &'static str {
        "console"
    }
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let platform = ConsolePlatform;

    println!("（console platform 演示：输入一行文本回车 → recv → send_text）");
    let msg = platform.recv().await?;
    println!(
        "[{}] recv from @{} in {}: {:?}",
        platform.name(),
        msg.sender.0,
        msg.conv_id.0,
        msg.text
    );
    // 演示 typing + 回显（真实网关会把 prompt 交给 Backend 执行，再把结果 send_text）。
    platform.send_typing(&msg.conv_id, &msg.reply_hint).await?;
    platform
        .send_text(&msg.conv_id, &msg.text.unwrap_or_default(), &msg.reply_hint)
        .await?;
    Ok(())
}
