use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use crate::mcp::MempalMcpServer;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use rmcp::{
    RoleServer, model::ClientJsonRpcMessage, service::serve_directly, transport::OneshotTransport,
};

const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";
const MCP_BODY_LIMIT: usize =
    crate::ingest::admission::MAX_INGEST_REQUEST_BYTES + (2 * 1024 * 1024);

#[derive(Clone)]
struct HttpState {
    base_server: MempalMcpServer,
    sessions: Arc<Mutex<HashMap<String, MempalMcpServer>>>,
    next_session_id: Arc<AtomicU64>,
    bound_addr: Option<SocketAddr>,
}

pub(super) fn service(server: MempalMcpServer, bound_addr: Option<SocketAddr>) -> Router {
    Router::new()
        .route("/", post(handle))
        .with_state(HttpState {
            base_server: server,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_session_id: Arc::new(AtomicU64::new(0)),
            bound_addr,
        })
}

fn bad_request(message: &'static str) -> Response {
    (StatusCode::BAD_REQUEST, message).into_response()
}

fn validate_host(
    headers: &axum::http::HeaderMap,
    bound_addr: Option<SocketAddr>,
) -> Result<(), Response> {
    let Some(value) = headers.get(header::HOST) else {
        return Err(bad_request("MCP request requires a Host header"));
    };
    let value = value
        .to_str()
        .map_err(|_| bad_request("MCP Host header is not valid UTF-8"))?;
    let authority = value
        .parse::<axum::http::uri::Authority>()
        .map_err(|_| bad_request("MCP Host header is malformed"))?;
    let host = authority.host();
    let local_host = host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1";
    if !local_host {
        return Err(bad_request("MCP Host header is not loopback"));
    }
    if let Some(port) = authority.port_u16() {
        if bound_addr.is_none_or(|addr| addr.port() != port || !addr.ip().is_loopback()) {
            return Err(bad_request("MCP Host port is not the bound loopback port"));
        }
    }
    if bound_addr.is_some_and(|addr| !addr.ip().is_loopback()) {
        return Err(bad_request("MCP listener is not loopback"));
    }
    Ok(())
}

fn new_session_id(state: &HttpState) -> String {
    let counter = state.next_session_id.fetch_add(1, Ordering::Relaxed);
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"mempal mcp http session v1");
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&counter.to_le_bytes());
    hasher.update(&now_ns.to_le_bytes());
    format!("mcp-{}", hasher.finalize().to_hex())
}

fn session_server(
    state: &HttpState,
    requested_id: Option<&str>,
) -> Result<(String, MempalMcpServer), Response> {
    let session_id = requested_id
        .map(str::to_string)
        .unwrap_or_else(|| new_session_id(state));
    if session_id.is_empty() {
        return Err(bad_request("MCP session id must not be empty"));
    }
    let mut sessions = state.sessions.lock().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "MCP session state unavailable",
        )
            .into_response()
    })?;
    let server = sessions
        .entry(session_id.clone())
        .or_insert_with(|| state.base_server.new_http_session())
        .clone();
    Ok((session_id, server))
}

fn with_session_header(mut response: Response, session_id: &str) -> Response {
    if let Ok(value) = session_id.parse() {
        response.headers_mut().insert(MCP_SESSION_ID_HEADER, value);
    }
    response
}

async fn handle(State(state): State<HttpState>, request: Request<Body>) -> Response {
    if let Err(response) = validate_host(request.headers(), state.bound_addr) {
        return response;
    }
    let requested_session_id = match request.headers().get(MCP_SESSION_ID_HEADER) {
        None => None,
        Some(value) => match value.to_str() {
            Ok(value) if !value.is_empty() => Some(value),
            _ => return bad_request("MCP session id is malformed"),
        },
    };
    let (session_id, server) = match session_server(&state, requested_session_id) {
        Ok(session) => session,
        Err(response) => return response,
    };
    let accepts = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.contains("application/json") && value.contains("text/event-stream")
        });
    if !accepts {
        return with_session_header(
            (
                StatusCode::NOT_ACCEPTABLE,
                "MCP client must accept JSON and SSE",
            )
                .into_response(),
            &session_id,
        );
    }
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if !content_type {
        return with_session_header(
            (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "MCP content type must be application/json",
            )
                .into_response(),
            &session_id,
        );
    }

    let (_, body) = request.into_parts();
    let body = match to_bytes(body, MCP_BODY_LIMIT).await {
        Ok(body) => body,
        Err(error) => {
            return with_session_header(
                (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
                &session_id,
            );
        }
    };
    let message = match serde_json::from_slice::<ClientJsonRpcMessage>(&body) {
        Ok(message) => message,
        Err(error) => {
            return with_session_header(
                (StatusCode::UNSUPPORTED_MEDIA_TYPE, error.to_string()).into_response(),
                &session_id,
            );
        }
    };
    let ClientJsonRpcMessage::Request(request) = message else {
        return with_session_header(StatusCode::ACCEPTED.into_response(), &session_id);
    };

    let (transport, mut receiver) =
        OneshotTransport::<RoleServer>::new(ClientJsonRpcMessage::Request(request));
    let running = serve_directly(server, transport, None);
    tokio::spawn(async move {
        let _ = running.waiting().await;
    });
    let response = match receiver.recv().await {
        Some(message) => axum::Json(message).into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "MCP handler returned no response",
        )
            .into_response(),
    };
    with_session_header(response, &session_id)
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

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
        },
        embed::ConfiguredEmbedderFactory,
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
        let daemon_db = AsyncDb::open_for(&db_path, 4, DbHolderClass::Daemon)
            .context("open daemon-owned pool")?;
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
        let response: Value =
            serde_json::from_slice(&body).context("decode initialize response")?;
        anyhow::ensure!(
            response["result"].is_object(),
            "initialize failed: {response}"
        );
        session_id.context("initialize response omitted MCP session id")
    }

    async fn initialize(live: &LiveMcp) -> Result<()> {
        let response = post(
            live,
            1,
            "initialize",
            json!({
                "protocolVersion":"2025-03-26",
                "capabilities":{},
                "clientInfo":{"name":"listen-port-test","version":"0.1"}
            }),
            None,
        )
        .await?;
        anyhow::ensure!(
            response["result"].is_object(),
            "initialize failed: {response}"
        );
        Ok(())
    }

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
            anyhow::ensure!(status / 100 == 2, "localhost Host rejected: {status}");

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
            initialize(&live).await?;
            let tools = post(&live, 2, "tools/list", json!({}), None).await?;
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
                None,
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
                None,
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
        let daemon_db = AsyncDb::open_for(&db_path, 4, DbHolderClass::Daemon)
            .context("open daemon-owned pool")?;
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
            initialize(&live).await?;
            let _ = post(&live, 2, "tools/list", json!({}), None).await?;
            let _ = post(
                &live,
                3,
                "tools/call",
                json!({"name":"mempal_status","arguments":{}}),
                None,
            )
            .await?;
            let _ = post(
                &live,
                4,
                "tools/call",
                json!({"name":"mempal_search","arguments":{"query":"daemon","top_k":1}}),
                None,
            )
            .await?;
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
}
