//! 示例：自定义实现 `Backend` trait（一个把 prompt 原样回显的 echo backend）。
//!
//! 演示 imagent 的后端双抽象——任何实现 `Backend` 的执行器都可注入网关。
//! 真实后端见 `crates/{claude,codex,gemini}`。
//!
//! 运行：`cargo run --example echo_backend -- "你好"`

use std::path::Path;

use async_trait::async_trait;
use imagent_core::{AgentChunk, Backend, Result, RunOutcome, SessionId};
use tokio::sync::mpsc;

/// 把 prompt 原样回显的最简 `Backend`（无副作用，演示用）。
pub struct EchoBackend;

#[async_trait]
impl Backend for EchoBackend {
    async fn run(
        &self,
        _conv_id: &str,
        prompt: &str,
        _session: Option<&SessionId>,
        _workdir: &Path,
        _allowed_tools: &[String],
        chunks: mpsc::Sender<AgentChunk>,
        _initial_todos: &[imagent_core::TodoItem],
    ) -> Result<RunOutcome> {
        // 流式推一个 Final chunk（core 收到后回传 IM）。
        let _ = chunks.send(AgentChunk::Final(prompt.to_string())).await;
        Ok(RunOutcome {
            stop_reason: None,
            session_id: SessionId("echo-demo".to_string()),
            final_text: prompt.to_string(),
            terminal: true,
            usage: None,
        })
    }

    fn name(&self) -> &'static str {
        "echo"
    }
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "hello from echo backend".to_string());

    let backend = EchoBackend;
    let (tx, mut rx) = mpsc::channel::<AgentChunk>(8);
    let workdir = std::env::current_dir()?;
    let tools: Vec<String> = vec!["Read".to_string()];

    let outcome = backend
        .run("example-conv", &prompt, None, &workdir, &tools, tx, &[])
        .await?;

    println!("session_id: {}", outcome.session_id.0);
    println!("final_text: {}", outcome.final_text);
    while let Some(chunk) = rx.recv().await {
        if let AgentChunk::Final(t) = chunk {
            println!("Final chunk: {t}");
        }
    }
    Ok(())
}
