use anyhow::Result;
use serde_json::json;
use tokio::net::TcpListener;

#[tokio::test]
async fn daemon_mcp_listen_port_fails_closed_when_daemon_down() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    drop(listener);
    let error = reqwest::Client::new()
        .post(format!("http://{address}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
        .send()
        .await
        .expect_err("daemon-down MCP URL unexpectedly accepted a connection");
    anyhow::ensure!(
        error.is_connect(),
        "daemon-down error was not connection refusal: {error}"
    );
    Ok(())
}
