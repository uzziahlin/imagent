//! ACP 连接探针（真机排障用，不进 CI）：用与 AcpBackend 相同的 crate API
//! spawn 适配器，打印全部行级流量 + initialize/session/new 结果。
//!
//! 用法：`cargo run -p imagent-claude --example acp_probe -- /path/to/claude-agent-acp [cwd]`

use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{AcpAgent, Client, ConnectionTo};
use std::str::FromStr;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cmd = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "claude-agent-acp".into());
    let cwd = std::env::args().nth(2).unwrap_or_else(|| ".".into());

    let agent = AcpAgent::from_str(&cmd)?;
    fn dbg(line: &str, dir: agent_client_protocol::LineDirection) {
        eprintln!("[debug:{dir:?}] {}", &line[..line.len().min(400)]);
    }
    let agent = agent.with_debug(dbg as fn(&str, _));

    let cwd_clone = std::path::PathBuf::from(&cwd);
    let probe = async {
        Client
            .builder()
            .connect_with(agent, |connection: ConnectionTo<_>| async move {
                let init = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                eprintln!("[probe] initialize ok: {:?}", init.agent_info);

                let cwd = cwd_clone
                    .canonicalize()
                    .map_err(|e| anyhow::anyhow!("cwd 无效: {e}"))?;
                let new_session = NewSessionRequest::new(cwd.clone());
                let session = connection.send_request(new_session).block_task().await?;
                let sid = session.session_id.clone();
                eprintln!("[probe] session/new ok: {sid}");

                // 一轮真实 prompt（与 e2e 同款构造）：观察是否返回。
                let prompt = PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::Text(TextContent::new(
                        "Reply with exactly: hi",
                    ))],
                );
                match connection.send_request(prompt).block_task().await {
                    Ok(_) => eprintln!("[probe] session/prompt ok"),
                    Err(e) => eprintln!("[probe] session/prompt err: {e}"),
                }
                Ok::<_, agent_client_protocol::Error>(())
            })
            .await
    };

    match tokio::time::timeout(std::time::Duration::from_secs(45), probe).await {
        Ok(Ok(())) => {
            eprintln!("[probe] PASS");
            Ok(())
        }
        Ok(Err(e)) => Err(anyhow::anyhow!("probe 失败: {e}")),
        Err(_) => {
            eprintln!("[probe] TIMEOUT（45s）——卡在 initialize 或 session/new");
            Err(anyhow::anyhow!("probe 超时"))
        }
    }
}
