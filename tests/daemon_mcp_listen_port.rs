use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result, anyhow};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post as post_route,
};
use mempal::{
    core::{
        AsyncDb,
        config::Config,
        db::Database,
        db_admission::{DbHolderClass, ProfileDbAdmission},
    },
    embed::ConfiguredEmbedderFactory,
    mcp::MempalMcpServer,
};
use rmcp::{
    RoleServer, model::ClientJsonRpcMessage, service::serve_directly, transport::OneshotTransport,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tower::ServiceExt;

fn fixture() -> Result<(TempDir, PathBuf, MempalMcpServer)> {
    let tempdir = TempDir::new_in("/tmp").context("create MCP HTTP fixture")?;
    let db_path = tempdir.path().join("palace.db");
    Database::open(&db_path).context("initialize MCP HTTP database")?;
    let config = Config {
        db_path: db_path.display().to_string(),
        embed: mempal::core::config::EmbedConfig {
            backend: "stub".to_string(),
            ..Default::default()
        },
        ..Config::default()
    };
    let factory = Arc::new(ConfiguredEmbedderFactory::new(config.clone()));
    let server =
        MempalMcpServer::new_with_factory_and_config(db_path.clone(), config, factory.clone())
            .context("create MCP HTTP server")?;
    Ok((tempdir, db_path, server))
}

type HttpMcp = Router;

fn service(server: MempalMcpServer) -> HttpMcp {
    Router::new()
        .route("/mcp", post_route(handle))
        .with_state(server)
}

async fn handle(State(server): State<MempalMcpServer>, request: Request<Body>) -> Response {
    let accepts = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.contains("application/json") && value.contains("text/event-stream")
        });
    if !accepts {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "MCP client must accept JSON and SSE",
        )
            .into_response();
    }
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if !content_type {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "MCP content type must be application/json",
        )
            .into_response();
    }
    let (_, body) = request.into_parts();
    let body = match to_bytes(body, 4 * 1024 * 1024).await {
        Ok(body) => body,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let message = match serde_json::from_slice::<ClientJsonRpcMessage>(&body) {
        Ok(message) => message,
        Err(error) => {
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, error.to_string()).into_response();
        }
    };
    let ClientJsonRpcMessage::Request(request) = message else {
        return StatusCode::ACCEPTED.into_response();
    };
    let (transport, mut receiver) =
        OneshotTransport::<RoleServer>::new(ClientJsonRpcMessage::Request(request));
    let running = serve_directly(server, transport, None);
    tokio::spawn(async move {
        let _ = running.waiting().await;
    });
    match receiver.recv().await {
        Some(message) => axum::Json(message).into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "MCP handler returned no response",
        )
            .into_response(),
    }
}

async fn post(app: &HttpMcp, id: u64, method: &str, params: Value) -> Result<Value> {
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "127.0.0.1")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}).to_string(),
        ))
        .context("build MCP HTTP request")?;
    let response = app
        .clone()
        .oneshot(request)
        .await
        .map_err(|error| anyhow!("MCP HTTP request failed: {error}"))?;
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .context("read MCP HTTP response")?;
    if !status.is_success() {
        return Err(anyhow!(
            "MCP HTTP status {status}: {}",
            String::from_utf8_lossy(&body)
        ));
    }
    serde_json::from_slice(&body).context("decode MCP JSON response")
}

async fn initialize(app: &HttpMcp) -> Result<()> {
    let response = post(
        app,
        1,
        "initialize",
        json!({
            "protocolVersion":"2025-03-26",
            "capabilities":{},
            "clientInfo":{"name":"listen-port-test","version":"0.1"}
        }),
    )
    .await?;
    anyhow::ensure!(
        response["result"].is_object(),
        "initialize failed: {response}"
    );
    Ok(())
}

#[tokio::test]
async fn daemon_mcp_listen_port_serves_status_and_search() -> Result<()> {
    let (_tempdir, db_path, server) = fixture()?;
    let daemon_db =
        AsyncDb::open_for(&db_path, 4, DbHolderClass::Daemon).context("open daemon-owned pool")?;
    let app = service(server.with_daemon_owned_async_db(daemon_db));
    initialize(&app).await?;

    let tools = post(&app, 2, "tools/list", json!({})).await?;
    anyhow::ensure!(
        tools["result"]["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "mempal_status"))
            && tools["result"]["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "mempal_search")),
        "tools/list omitted stable tools: {tools}"
    );

    let status = post(
        &app,
        3,
        "tools/call",
        json!({"name":"mempal_status","arguments":{}}),
    )
    .await?;
    anyhow::ensure!(
        status["result"]["structuredContent"].is_object(),
        "status failed: {status}"
    );

    let search = post(
        &app,
        4,
        "tools/call",
        json!({"name":"mempal_search","arguments":{"query":"daemon","top_k":1}}),
    )
    .await?;
    anyhow::ensure!(
        search["result"]["structuredContent"].is_object(),
        "search failed: {search}"
    );
    Ok(())
}

#[tokio::test]
async fn daemon_mcp_listen_port_reuses_daemon_holder() -> Result<()> {
    let (_tempdir, db_path, server) = fixture()?;
    let daemon_db =
        AsyncDb::open_for(&db_path, 4, DbHolderClass::Daemon).context("open daemon-owned pool")?;
    let before = ProfileDbAdmission::snapshot(&db_path).context("snapshot before MCP HTTP")?;
    let app = service(server.with_daemon_owned_async_db(daemon_db));
    initialize(&app).await?;
    let _ = post(&app, 2, "tools/list", json!({})).await?;
    let _ = post(
        &app,
        3,
        "tools/call",
        json!({"name":"mempal_status","arguments":{}}),
    )
    .await?;
    let _ = post(
        &app,
        4,
        "tools/call",
        json!({"name":"mempal_search","arguments":{"query":"daemon","top_k":1}}),
    )
    .await?;
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
    Ok(())
}

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
