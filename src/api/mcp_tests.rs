use std::{fs, path::PathBuf, process::Command, sync::Arc};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{net::TcpListener, sync::Notify, task::JoinHandle};

use super::*;
use crate::{
    core::{
        AsyncDb,
        config::Config,
        db::Database,
        db_admission::{DbHolderClass, ProfileDbAdmission},
        types::{Drawer, SourceType},
    },
    embed::{ConfiguredEmbedderFactory, EmbedderFactory},
    mcp::MempalMcpServer,
};

struct LiveMcp {
    address: SocketAddr,
    client: reqwest::Client,
    stop: Arc<Notify>,
    task: JoinHandle<()>,
}

fn fixture() -> Result<(TempDir, PathBuf, Config, MempalMcpServer)> {
    let tempdir = TempDir::new_in("/tmp").context("create MCP HTTP fixture")?;
    let db_path = tempdir.path().join("palace.db");
    Database::open(&db_path).context("initialize MCP HTTP database")?;
    let config = Config {
        db_path: db_path.display().to_string(),
        embed: crate::core::config::EmbedConfig {
            backend: "stub".to_string(),
            ..Default::default()
        },
        ..Config::default()
    };
    let factory = Arc::new(ConfiguredEmbedderFactory::new(config.clone()));
    let server =
        MempalMcpServer::new_with_factory_and_config(db_path.clone(), config.clone(), factory)
            .context("create MCP HTTP server")?;
    Ok((tempdir, db_path, config, server))
}

async fn live_mcp() -> Result<(TempDir, LiveMcp)> {
    let (tempdir, db_path, _config, server) = fixture()?;
    let daemon_db =
        AsyncDb::open_for(&db_path, 4, DbHolderClass::Daemon).context("open daemon-owned pool")?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = server.with_daemon_owned_async_db(daemon_db);
    #[cfg(feature = "rest")]
    let app = crate::api::router_with_mcp_at(
        crate::api::ApiState::new(db_path, Arc::new(ConfiguredEmbedderFactory::new(_config))),
        server,
        address,
    );
    #[cfg(not(feature = "rest"))]
    let app = Router::new().nest_service("/mcp", service(server, Some(address)));
    let stop = Arc::new(Notify::new());
    let stop_task = Arc::clone(&stop);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                stop_task.notified().await;
            })
            .await;
    });
    tokio::task::yield_now().await;
    Ok((
        tempdir,
        LiveMcp {
            address,
            client: reqwest::Client::new(),
            stop,
            task,
        },
    ))
}

async fn live_mcp_with_projects() -> Result<(TempDir, LiveMcp)> {
    let (tempdir, db_path, config, server) = fixture()?;
    let db = Database::open(&db_path).context("open MCP HTTP seed database")?;
    let embedder = ConfiguredEmbedderFactory::new(config.clone())
        .build()
        .await
        .context("build MCP HTTP search embedder")?;
    for project_id in ["project-a", "project-b"] {
        let project_root = tempdir.path().join(project_id);
        fs::create_dir_all(&project_root).context("create MCP HTTP project root")?;
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&project_root)
            .status()
            .context("initialize MCP HTTP project root")?;
        anyhow::ensure!(status.success(), "git init failed for {project_id}");
        let drawer = Drawer {
            id: format!("{project_id}-drawer"),
            content: format!("memory for {project_id}"),
            wing: "project".to_string(),
            room: Some("decision".to_string()),
            source_file: Some(format!("{project_id}.md")),
            source_type: SourceType::AgentInference,
            added_at: "1700000000".to_string(),
            ..Drawer::default()
        };
        db.insert_drawer_with_project(&drawer, Some(project_id))
            .context("seed MCP HTTP project drawer")?;
        let vector = embedder
            .embed(&[drawer.content.as_str()])
            .await
            .context("embed MCP HTTP project drawer")?
            .into_iter()
            .next()
            .context("MCP HTTP project drawer embedding omitted")?;
        db.insert_vector_with_project(&drawer.id, &vector, Some(project_id))
            .context("seed MCP HTTP project vector")?;
    }
    let daemon_db =
        AsyncDb::open_for(&db_path, 4, DbHolderClass::Daemon).context("open daemon-owned pool")?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = server.with_daemon_owned_async_db(daemon_db);
    let app = Router::new().nest_service("/mcp", service(server, Some(address)));
    let stop = Arc::new(Notify::new());
    let stop_task = Arc::clone(&stop);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                stop_task.notified().await;
            })
            .await;
    });
    tokio::task::yield_now().await;
    let _ = config;
    Ok((
        tempdir,
        LiveMcp {
            address,
            client: reqwest::Client::new(),
            stop,
            task,
        },
    ))
}

async fn stop(live: LiveMcp) {
    live.stop.notify_one();
    let _ = live.task.await;
}

async fn post_raw(
    live: &LiveMcp,
    id: u64,
    method: &str,
    params: Value,
    host: Option<&str>,
    session_id: Option<&str>,
    body: Option<&str>,
) -> Result<(u16, Option<String>, Vec<u8>)> {
    let mut request = live
        .client
        .post(format!("http://{}/mcp", live.address))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json");
    if let Some(host) = host {
        request = request.header("host", host);
    }
    if let Some(session_id) = session_id {
        request = request.header(MCP_SESSION_ID_HEADER, session_id);
    }
    let body = body.map(str::to_string).unwrap_or_else(|| {
        json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}).to_string()
    });
    let response = request.body(body).send().await?;
    let status = response.status().as_u16();
    let session_id = response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .map(|value| value.to_str().map(str::to_string))
        .transpose()
        .context("decode MCP session header")?;
    Ok((status, session_id, response.bytes().await?.to_vec()))
}

async fn post(
    live: &LiveMcp,
    id: u64,
    method: &str,
    params: Value,
    session_id: Option<&str>,
) -> Result<Value> {
    let host = live.address.to_string();
    let (status, _, body) =
        post_raw(live, id, method, params, Some(&host), session_id, None).await?;
    anyhow::ensure!(
        (200..300).contains(&status),
        "MCP HTTP status {status}: {}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).context("decode MCP JSON response")
}

async fn initialize_session(live: &LiveMcp, client_name: &str) -> Result<String> {
    let host = format!("localhost:{}", live.address.port());
    let (status, session_id, body) = post_raw(
        live,
        1,
        "initialize",
        json!({
            "protocolVersion":"2025-03-26",
            "capabilities":{"roots":{"listChanged":true}},
            "clientInfo":{"name":client_name,"version":"0.1"}
        }),
        Some(&host),
        None,
        None,
    )
    .await?;
    anyhow::ensure!(
        (200..300).contains(&status),
        "initialize failed with {status}: {}",
        String::from_utf8_lossy(&body)
    );
    let response: Value = serde_json::from_slice(&body).context("decode initialize response")?;
    anyhow::ensure!(
        response["result"].is_object(),
        "initialize failed: {response}"
    );
    session_id.context("initialize response omitted MCP session id")
}

async fn initialize(live: &LiveMcp) -> Result<String> {
    let host = live.address.to_string();
    let (status, session_id, body) = post_raw(
        live,
        1,
        "initialize",
        json!({
            "protocolVersion":"2025-03-26",
            "capabilities":{},
            "clientInfo":{"name":"listen-port-test","version":"0.1"}
        }),
        Some(&host),
        None,
        None,
    )
    .await?;
    anyhow::ensure!(
        (200..300).contains(&status),
        "initialize failed with {status}: {}",
        String::from_utf8_lossy(&body)
    );
    let response: Value = serde_json::from_slice(&body).context("decode initialize response")?;
    anyhow::ensure!(
        response["result"].is_object(),
        "initialize failed: {response}"
    );
    session_id.context("initialize response omitted MCP session id")
}

#[tokio::test]
async fn daemon_mcp_listen_port_rejects_unmatched_client_response_without_session_leak()
-> Result<()> {
    let (_tempdir, live) = live_mcp().await?;
    let result = async {
        let session_id = initialize(&live).await?;
        let host = live.address.to_string();
        for body in [
            r#"{"jsonrpc":"2.0","id":999,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":999,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":999,"error":{"code":-32603,"message":"duplicate"}}"#,
        ] {
            let (status, _, _) = tokio::time::timeout(
                Duration::from_secs(1),
                post_raw(
                    &live,
                    999,
                    "ignored",
                    json!({}),
                    Some(&host),
                    Some(&session_id),
                    Some(body),
                ),
            )
            .await
            .context("unmatched client response/error hung")??;
            anyhow::ensure!(status == 202, "unmatched client message status: {status}");
        }

        for id in 0..=MAX_HTTP_SESSIONS {
            let session_id = initialize_session(&live, &format!("bounded-{id}")).await?;
            let (status, _, _) = tokio::time::timeout(
                Duration::from_secs(1),
                post_raw(
                    &live,
                    id as u64 + 10,
                    "ignored",
                    json!({}),
                    Some(&host),
                    Some(&session_id),
                    Some(r#"{"jsonrpc":"2.0","id":999,"result":{}}"#),
                ),
            )
            .await
            .context("evicted-session ownership escaped its bound")??;
            anyhow::ensure!(status == 202, "bounded unmatched response status: {status}");
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    stop(live).await;
    result
}

#[path = "mcp_reservation_tests.rs"]
mod reservation_tests;

#[tokio::test]
async fn daemon_mcp_listen_port_rejects_dns_rebinding_host() -> Result<()> {
    let (_tempdir, live) = live_mcp().await?;
    let result = async {
        let (status, _, body) = post_raw(
            &live,
            1,
            "initialize",
            json!({}),
            Some("attacker.example"),
            None,
            Some("not-json"),
        )
        .await?;
        anyhow::ensure!(
            status == 400,
            "attacker Host was accepted or parsed: {status} {}",
            String::from_utf8_lossy(&body)
        );

        let host = live.address.to_string();
        let (status, session_id, _) = post_raw(
            &live,
            2,
            "initialize",
            json!({
                "protocolVersion":"2025-03-26",
                "capabilities":{},
                "clientInfo":{"name":"loopback","version":"0.1"}
            }),
            Some(&host),
            None,
            None,
        )
        .await?;
        anyhow::ensure!(
            status / 100 == 2 && session_id.is_some(),
            "loopback Host rejected: {status}"
        );

        let (status, _, _) = post_raw(
            &live,
            3,
            "initialize",
            json!({
                "protocolVersion":"2025-03-26",
                "capabilities":{},
                "clientInfo":{"name":"loopback","version":"0.1"}
            }),
            Some("localhost"),
            None,
            None,
        )
        .await?;
        anyhow::ensure!(
            status == 400,
            "portless localhost Host was accepted: {status}"
        );

        let wrong_port_host = format!("127.0.0.1:{}", live.address.port() + 1);
        let (status, _, _) = post_raw(
            &live,
            4,
            "initialize",
            json!({}),
            Some(&wrong_port_host),
            None,
            None,
        )
        .await?;
        anyhow::ensure!(status == 400, "wrong-port Host was accepted: {status}");
        Ok::<(), anyhow::Error>(())
    }
    .await;
    stop(live).await;
    result
}

#[tokio::test]
async fn daemon_mcp_listen_port_admits_only_valid_initialized_sessions() -> Result<()> {
    let (_tempdir, live) = live_mcp().await?;
    let result = async {
        let host = live.address.to_string();
        let (status, session_id, _) = post_raw(
            &live,
            1,
            "initialize",
            json!({}),
            Some(&host),
            None,
            Some("not-json"),
        )
        .await?;
        anyhow::ensure!(status == 415, "malformed pre-init request status: {status}");
        anyhow::ensure!(
            session_id.is_none(),
            "malformed pre-init request was assigned a session"
        );

        let (status, _, _) = post_raw(
            &live,
            2,
            "initialize",
            json!({
                "protocolVersion":"2025-03-26",
                "capabilities":{},
                "clientInfo":{"name":"forged-session","version":"0.1"}
            }),
            Some(&host),
            Some("client-supplied-session"),
            None,
        )
        .await?;
        anyhow::ensure!(
            status == 404,
            "unknown client-supplied session was admitted: {status}"
        );

        let (status, _, _) =
            post_raw(&live, 3, "tools/list", json!({}), Some(&host), None, None).await?;
        anyhow::ensure!(
            status == 400,
            "non-initialize request created a session: {status}"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    stop(live).await;
    result
}

fn take_sse_event(buffer: &mut Vec<u8>) -> Option<Value> {
    let end = buffer.windows(2).position(|window| window == b"\n\n")?;
    let event = buffer.drain(..end + 2).collect::<Vec<_>>();
    String::from_utf8_lossy(&event).lines().find_map(|line| {
        line.strip_prefix("data:")
            .and_then(|data| serde_json::from_str(data.trim()).ok())
    })
}

async fn next_sse_event(response: &mut reqwest::Response, buffer: &mut Vec<u8>) -> Result<Value> {
    loop {
        if let Some(event) = take_sse_event(buffer) {
            return Ok(event);
        }
        let chunk = response
            .chunk()
            .await?
            .context("MCP SSE stream ended before the next event")?;
        buffer.extend_from_slice(&chunk);
    }
}

async fn search_with_roots(
    live: &LiveMcp,
    session_id: &str,
    root_uri: &str,
    id: u64,
    project_id: &str,
) -> Result<Value> {
    let host = live.address.to_string();
    let body = json!({
        "jsonrpc":"2.0",
        "id":id,
        "method":"tools/call",
        "params":{"name":"mempal_search","arguments":{"query":format!("memory for {project_id}"),"top_k":1}}
    });
    let mut response = live
        .client
        .post(format!("http://{}/mcp", live.address))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("host", &host)
        .header(MCP_SESSION_ID_HEADER, session_id)
        .json(&body)
        .send()
        .await?;
    anyhow::ensure!(
        response.status().is_success(),
        "search request failed: {}",
        response.status()
    );
    let mut buffer = Vec::new();
    let roots_request = next_sse_event(&mut response, &mut buffer).await?;
    anyhow::ensure!(
        roots_request["method"] == "roots/list",
        "search did not dispatch roots/list: {roots_request}"
    );
    let roots_response = json!({
        "jsonrpc":"2.0",
        "id":roots_request["id"],
        "result":{"roots":[{"uri":root_uri,"name":"test-root"}]}
    })
    .to_string();
    let (status, _, _) = post_raw(
        live,
        id + 1,
        "ignored",
        json!({}),
        Some(&host),
        Some(session_id),
        Some(&roots_response),
    )
    .await?;
    anyhow::ensure!(status == 202, "roots response failed: {status}");
    next_sse_event(&mut response, &mut buffer).await
}

#[tokio::test]
async fn daemon_mcp_listen_port_isolates_two_roots_sessions_and_dispatches_changed() -> Result<()> {
    let (tempdir, live) = live_mcp_with_projects().await?;
    let root_a = format!("file://{}", tempdir.path().join("project-a").display());
    let root_b = format!("file://{}", tempdir.path().join("project-b").display());
    let result = async {
        let session_a = initialize_session(&live, "roots-project-a").await?;
        let session_b = initialize_session(&live, "roots-project-b").await?;
        let search_a = search_with_roots(&live, &session_a, &root_a, 2, "project-a").await?;
        let search_b = search_with_roots(&live, &session_b, &root_b, 4, "project-b").await?;
        anyhow::ensure!(
            search_a["result"]["structuredContent"]["results"][0]["content"]
                == "memory for project-a",
            "roots project-a was not isolated: {search_a}"
        );
        anyhow::ensure!(
            search_b["result"]["structuredContent"]["results"][0]["content"]
                == "memory for project-b",
            "roots project-b was not isolated: {search_b}"
        );

        let changed = json!({
            "jsonrpc":"2.0",
            "method":"notifications/roots/list_changed",
            "params":{}
        })
        .to_string();
        let (status, _, _) = post_raw(
            &live,
            6,
            "ignored",
            json!({}),
            Some(&live.address.to_string()),
            Some(&session_a),
            Some(&changed),
        )
        .await?;
        anyhow::ensure!(
            status == 202,
            "roots/list_changed was not dispatched: {status}"
        );
        let search_a_again = search_with_roots(&live, &session_a, &root_a, 7, "project-a").await?;
        anyhow::ensure!(
            search_a_again["result"]["structuredContent"]["results"][0]["content"]
                == "memory for project-a",
            "roots project-a changed after roots/list_changed: {search_a_again}"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    stop(live).await;
    result
}

#[tokio::test]
async fn daemon_mcp_listen_port_isolates_http_client_sessions() -> Result<()> {
    let (_tempdir, live) = live_mcp().await?;
    let result = async {
            let claude_session = initialize_session(&live, "claude-code").await?;
            let codex_session = initialize_session(&live, "codex-mcp-client").await?;
            let claude = post(
                &live,
                2,
                "tools/call",
                json!({"name":"mempal_peek_partner","arguments":{"tool":"auto","limit":1}}),
                Some(&claude_session),
            )
            .await?;
            let codex = post(
                &live,
                3,
                "tools/call",
                json!({"name":"mempal_peek_partner","arguments":{"tool":"auto","limit":1}}),
                Some(&codex_session),
            )
            .await?;
            let claude_again = post(
                &live,
                4,
                "tools/call",
                json!({"name":"mempal_peek_partner","arguments":{"tool":"auto","limit":1}}),
                Some(&claude_session),
            )
            .await?;
            anyhow::ensure!(
                claude["result"]["structuredContent"]["partner_tool"] == "codex"
                    && codex["result"]["structuredContent"]["partner_tool"] == "claude"
                    && claude_again["result"]["structuredContent"]["partner_tool"] == "codex",
                "MCP client identity crossed sessions: claude={claude} codex={codex} claude_again={claude_again}"
            );
            Ok::<(), anyhow::Error>(())
        }
        .await;
    stop(live).await;
    result
}

#[tokio::test]
async fn daemon_mcp_listen_port_serves_status_and_search() -> Result<()> {
    let (_tempdir, live) = live_mcp().await?;
    let result = async {
        let session_id = initialize(&live).await?;
        let tools = post(&live, 2, "tools/list", json!({}), Some(&session_id)).await?;
        anyhow::ensure!(
            tools["result"]["tools"].as_array().is_some_and(|tools| {
                tools.iter().any(|tool| tool["name"] == "mempal_status")
                    && tools.iter().any(|tool| tool["name"] == "mempal_search")
            }),
            "tools/list omitted stable tools: {tools}"
        );
        let status = post(
            &live,
            3,
            "tools/call",
            json!({"name":"mempal_status","arguments":{}}),
            Some(&session_id),
        )
        .await?;
        anyhow::ensure!(
            status["result"]["structuredContent"].is_object(),
            "status failed: {status}"
        );
        let search = post(
            &live,
            4,
            "tools/call",
            json!({"name":"mempal_search","arguments":{"query":"daemon","top_k":1}}),
            Some(&session_id),
        )
        .await?;
        anyhow::ensure!(
            search["result"]["structuredContent"].is_object(),
            "search failed: {search}"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    stop(live).await;
    result
}

#[tokio::test]
async fn daemon_mcp_listen_port_reuses_daemon_holder() -> Result<()> {
    let (_tempdir, db_path, _config, server) = fixture()?;
    let before = ProfileDbAdmission::snapshot(&db_path).context("snapshot before MCP HTTP")?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let daemon_db =
        AsyncDb::open_for(&db_path, 4, DbHolderClass::Daemon).context("open daemon-owned pool")?;
    let app = Router::new().nest_service(
        "/mcp",
        service(server.with_daemon_owned_async_db(daemon_db), Some(address)),
    );
    let stop_signal = Arc::new(Notify::new());
    let stop_task = Arc::clone(&stop_signal);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { stop_task.notified().await })
            .await;
    });
    let live = LiveMcp {
        address,
        client: reqwest::Client::new(),
        stop: stop_signal,
        task,
    };
    let result = async {
        let session_id = initialize(&live).await?;
        let _ = post(&live, 2, "tools/list", json!({}), Some(&session_id)).await?;
        let _ = post(
            &live,
            3,
            "tools/call",
            json!({"name":"mempal_status","arguments":{}}),
            Some(&session_id),
        )
        .await?;
        let _ = post(
            &live,
            4,
            "tools/call",
            json!({"name":"mempal_search","arguments":{"query":"daemon","top_k":1}}),
            Some(&session_id),
        )
        .await?;
        let live_snapshot =
            ProfileDbAdmission::snapshot(&db_path).context("snapshot while MCP HTTP is live")?;
        anyhow::ensure!(
            live_snapshot
                .holders
                .iter()
                .all(|holder| holder.holder_class != DbHolderClass::Mcp),
            "daemon MCP HTTP admitted an MCP holder while live: {live_snapshot:?}"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    stop(live).await;
    let after = ProfileDbAdmission::snapshot(&db_path).context("snapshot after MCP HTTP")?;
    anyhow::ensure!(
        after
            .holders
            .iter()
            .all(|holder| holder.holder_class != DbHolderClass::Mcp),
        "daemon MCP HTTP admitted an MCP holder: {after:?}"
    );
    anyhow::ensure!(
        before.holders.len() == after.holders.len(),
        "daemon MCP HTTP changed holder count: before={before:?} after={after:?}"
    );
    result
}
