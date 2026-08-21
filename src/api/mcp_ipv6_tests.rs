use anyhow::Result;
use serde_json::json;

use super::*;

#[tokio::test]
async fn daemon_mcp_listen_port_admits_ipv6_loopback_host() -> Result<()> {
    let (_tempdir, live) = live_mcp_at("[::1]:0").await?;
    let result = async {
        let host = format!("[::1]:{}", live.address.port());
        let (status, session_id, body) = post_raw(
            &live,
            1,
            "initialize",
            json!({
                "protocolVersion":"2025-03-26",
                "capabilities":{},
                "clientInfo":{"name":"ipv6-loopback","version":"0.1"}
            }),
            Some(&host),
            None,
            None,
        )
        .await?;
        anyhow::ensure!(
            status / 100 == 2 && session_id.is_some(),
            "IPv6 Host with bracketed authority was rejected: {status} {}",
            String::from_utf8_lossy(&body)
        );

        let (status, _, body) = post_raw(
            &live,
            2,
            "initialize",
            json!({}),
            Some("[::1]"),
            None,
            Some("not-json"),
        )
        .await?;
        anyhow::ensure!(
            status == 400,
            "portless IPv6 Host was accepted: {status} {}",
            String::from_utf8_lossy(&body)
        );

        let non_loopback_host = format!("[2001:db8::1]:{}", live.address.port());
        let (status, _, body) = post_raw(
            &live,
            3,
            "initialize",
            json!({}),
            Some(&non_loopback_host),
            None,
            Some("not-json"),
        )
        .await?;
        anyhow::ensure!(
            status == 400,
            "non-loopback IPv6 Host was accepted: {status} {}",
            String::from_utf8_lossy(&body)
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    stop(live).await;
    result
}
