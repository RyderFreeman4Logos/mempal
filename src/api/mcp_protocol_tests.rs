use anyhow::Result;
use serde_json::json;

use super::*;

#[tokio::test]
async fn daemon_mcp_listen_port_negotiates_protocol_version() -> Result<()> {
    let (_tempdir, live) = live_mcp().await?;
    let result = async {
        let (status, session_id, body) = post_raw(
            &live,
            1,
            "initialize",
            json!({
                "protocolVersion":"2025-03-26",
                "capabilities":{},
                "clientInfo":{"name":"protocol-version","version":"0.1"}
            }),
            Some(&live.address.to_string()),
            None,
            None,
        )
        .await?;
        let response: serde_json::Value = serde_json::from_slice(&body)?;
        anyhow::ensure!(
            status / 100 == 2 && session_id.is_some(),
            "initialize failed: {status} {response}"
        );
        anyhow::ensure!(
            response["result"]["protocolVersion"] == "2025-03-26",
            "protocol version mismatch: {response}"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    stop(live).await;
    result
}

#[tokio::test]
async fn daemon_mcp_listen_port_rejects_unsupported_protocol_without_session_id() -> Result<()> {
    let (_tempdir, live) = live_mcp().await?;
    let result = async {
        let (status, session_id, body) = post_raw(
            &live,
            1,
            "initialize",
            json!({
                "protocolVersion":"2099-01-01",
                "capabilities":{},
                "clientInfo":{"name":"unsupported-protocol","version":"0.1"}
            }),
            Some(&live.address.to_string()),
            None,
            None,
        )
        .await?;
        anyhow::ensure!(
            status == 400 && session_id.is_none(),
            "unsupported protocol was accepted: {status} {}",
            String::from_utf8_lossy(&body)
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    stop(live).await;
    result
}
