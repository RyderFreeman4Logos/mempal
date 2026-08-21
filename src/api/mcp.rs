use std::{
    collections::HashMap,
    convert::Infallible,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::mcp::MempalMcpServer;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::post,
};
use rmcp::{
    RoleServer,
    model::{ClientJsonRpcMessage, RequestId, ServerJsonRpcMessage},
    service::serve_directly,
    transport::Transport,
};

const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";
const MCP_BODY_LIMIT: usize =
    crate::ingest::admission::MAX_INGEST_REQUEST_BYTES + (2 * 1024 * 1024);
const MAX_HTTP_SESSIONS: usize = 64;
const MAX_PENDING_HTTP_SESSIONS: usize = 64;
const HTTP_SESSION_TTL: Duration = Duration::from_secs(10 * 60);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_MESSAGE_SEND_TIMEOUT: Duration = Duration::from_secs(1);
const HTTP_SSE_BUFFER: usize = 16;

struct HttpSessionTransport {
    incoming: tokio::sync::mpsc::Receiver<ClientJsonRpcMessage>,
    outgoing: tokio::sync::mpsc::Sender<ServerJsonRpcMessage>,
}

impl Transport<RoleServer> for HttpSessionTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: ServerJsonRpcMessage,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let outgoing = self.outgoing.clone();
        async move {
            outgoing.send(item).await.map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "MCP HTTP session closed")
            })
        }
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<ClientJsonRpcMessage>> + Send {
        self.incoming.recv()
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Clone)]
struct HttpSession {
    incoming: tokio::sync::mpsc::Sender<ClientJsonRpcMessage>,
    outgoing: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<ServerJsonRpcMessage>>>,
    request_lock: Arc<tokio::sync::Mutex<()>>,
    active_requests: Arc<AtomicUsize>,
    last_used: Arc<Mutex<Instant>>,
}

struct SessionActivity {
    active_requests: Arc<AtomicUsize>,
    sessions: Arc<Mutex<HashMap<String, HttpSession>>>,
    session_id: String,
    completed: AtomicBool,
}

impl SessionActivity {
    fn reserve(
        session: &HttpSession,
        sessions: Arc<Mutex<HashMap<String, HttpSession>>>,
        session_id: &str,
    ) -> Self {
        session.active_requests.fetch_add(1, Ordering::AcqRel);
        Self {
            active_requests: Arc::clone(&session.active_requests),
            sessions,
            session_id: session_id.to_string(),
            completed: AtomicBool::new(false),
        }
    }

    fn complete(&self) {
        self.completed.store(true, Ordering::Release);
    }
}

impl Drop for SessionActivity {
    fn drop(&mut self) {
        if let Ok(mut sessions) = self.sessions.lock() {
            let remaining = self.active_requests.fetch_sub(1, Ordering::AcqRel) - 1;
            if remaining == 0 && !self.completed.load(Ordering::Acquire) {
                sessions.remove(&self.session_id);
            }
        }
    }
}

#[derive(Clone)]
struct HttpState {
    base_server: MempalMcpServer,
    sessions: Arc<Mutex<HashMap<String, HttpSession>>>,
    next_session_id: Arc<AtomicU64>,
    pending_sessions: Arc<AtomicUsize>,
    bound_addr: Option<SocketAddr>,
}

pub(super) fn service(server: MempalMcpServer, bound_addr: Option<SocketAddr>) -> Router {
    Router::new()
        .route("/", post(handle))
        .with_state(HttpState {
            base_server: server,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_session_id: Arc::new(AtomicU64::new(0)),
            pending_sessions: Arc::new(AtomicUsize::new(0)),
            bound_addr,
        })
}

fn bad_request(message: &'static str) -> Response {
    (StatusCode::BAD_REQUEST, message).into_response()
}

fn validate_host(
    headers: &axum::http::HeaderMap,
    bound_addr: Option<SocketAddr>,
) -> Result<(), Box<Response>> {
    let Some(value) = headers.get(header::HOST) else {
        return Err(Box::new(bad_request("MCP request requires a Host header")));
    };
    let value = value
        .to_str()
        .map_err(|_| Box::new(bad_request("MCP Host header is not valid UTF-8")))?;
    let authority = value
        .parse::<axum::http::uri::Authority>()
        .map_err(|_| Box::new(bad_request("MCP Host header is malformed")))?;
    let host = authority.host();
    let local_host = host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1";
    if !local_host {
        return Err(Box::new(bad_request("MCP Host header is not loopback")));
    }
    if let Some(addr) = bound_addr {
        if !addr.ip().is_loopback() {
            return Err(Box::new(bad_request("MCP listener is not loopback")));
        }
        if authority.port_u16() != Some(addr.port()) {
            return Err(Box::new(bad_request(
                "MCP Host port is not the bound loopback port",
            )));
        }
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

fn reap_expired_sessions(sessions: &mut HashMap<String, HttpSession>) {
    let now = Instant::now();
    sessions.retain(|_, session| {
        if session.active_requests.load(Ordering::Acquire) != 0 {
            return true;
        }
        session
            .last_used
            .lock()
            .map(|last_used| now.duration_since(*last_used) < HTTP_SESSION_TTL)
            .unwrap_or(false)
    });
    if sessions.len() < MAX_HTTP_SESSIONS {
        return;
    }
    let oldest = sessions
        .iter()
        .filter(|(_, session)| session.active_requests.load(Ordering::Acquire) == 0)
        .min_by_key(|(_, session)| {
            session
                .last_used
                .lock()
                .map(|last_used| *last_used)
                .unwrap_or(now)
        })
        .map(|(session_id, _)| session_id.clone());
    if let Some(session_id) = oldest {
        sessions.remove(&session_id);
    }
}

fn remove_session(sessions: &Arc<Mutex<HashMap<String, HttpSession>>>, session_id: &str) {
    if let Ok(mut sessions) = sessions.lock() {
        sessions.remove(session_id);
    }
}

fn reserve_http_session(
    sessions: &Arc<Mutex<HashMap<String, HttpSession>>>,
    session_id: &str,
) -> Option<(HttpSession, SessionActivity)> {
    let sessions_guard = sessions.lock().ok()?;
    let session = sessions_guard.get(session_id)?;
    let activity = SessionActivity::reserve(session, Arc::clone(sessions), session_id);
    Some((session.clone(), activity))
}

fn touch_session(session: &HttpSession) {
    if let Ok(mut last_used) = session.last_used.lock() {
        *last_used = Instant::now();
    }
}

async fn start_http_session(
    server: MempalMcpServer,
    initialize: ClientJsonRpcMessage,
) -> Option<(HttpSession, ServerJsonRpcMessage)> {
    let (incoming, incoming_rx) = tokio::sync::mpsc::channel(32);
    let (outgoing_tx, outgoing_rx) = tokio::sync::mpsc::channel(32);
    let running = serve_directly(
        server,
        HttpSessionTransport {
            incoming: incoming_rx,
            outgoing: outgoing_tx,
        },
        None,
    );
    tokio::spawn(async move {
        let _ = running.waiting().await;
    });
    incoming.send(initialize).await.ok()?;
    let mut outgoing_rx = outgoing_rx;
    let response = tokio::time::timeout(HTTP_REQUEST_TIMEOUT, outgoing_rx.recv())
        .await
        .ok()??;
    Some((
        HttpSession {
            incoming,
            outgoing: Arc::new(tokio::sync::Mutex::new(outgoing_rx)),
            request_lock: Arc::new(tokio::sync::Mutex::new(())),
            active_requests: Arc::new(AtomicUsize::new(0)),
            last_used: Arc::new(Mutex::new(Instant::now())),
        },
        response,
    ))
}

fn terminal_response(message: &ServerJsonRpcMessage, request_id: &RequestId) -> bool {
    match message {
        ServerJsonRpcMessage::Response(response) => &response.id == request_id,
        ServerJsonRpcMessage::Error(error) => &error.id == request_id,
        _ => false,
    }
}

async fn send_sse_message(
    sender: &tokio::sync::mpsc::Sender<String>,
    message: &ServerJsonRpcMessage,
) -> bool {
    let Ok(payload) = serde_json::to_string(message) else {
        return false;
    };
    sender.send(payload).await.is_ok()
}

struct HttpForward {
    outgoing: tokio::sync::OwnedMutexGuard<tokio::sync::mpsc::Receiver<ServerJsonRpcMessage>>,
    first: ServerJsonRpcMessage,
    request_id: RequestId,
    deadline: Instant,
    sender: tokio::sync::mpsc::Sender<String>,
    sessions: Arc<Mutex<HashMap<String, HttpSession>>>,
    session_id: String,
    _request_guard: tokio::sync::OwnedMutexGuard<()>,
    activity: SessionActivity,
}

async fn forward_http_session(forward: HttpForward) {
    let HttpForward {
        mut outgoing,
        first,
        request_id,
        deadline,
        sender,
        sessions,
        session_id,
        _request_guard,
        activity,
    } = forward;
    if !send_sse_message(&sender, &first).await {
        remove_session(&sessions, &session_id);
        return;
    }
    loop {
        let next = tokio::time::timeout_at(deadline.into(), outgoing.recv()).await;
        let Some(message) = next.ok().flatten() else {
            remove_session(&sessions, &session_id);
            return;
        };
        let is_terminal = terminal_response(&message, &request_id);
        if !send_sse_message(&sender, &message).await {
            remove_session(&sessions, &session_id);
            return;
        }
        if is_terminal {
            activity.complete();
            return;
        }
    }
}

async fn dispatch_http_session(
    sessions: Arc<Mutex<HashMap<String, HttpSession>>>,
    session: HttpSession,
    message: ClientJsonRpcMessage,
    session_id: &str,
    activity: SessionActivity,
) -> Response {
    let request_id = match &message {
        ClientJsonRpcMessage::Request(request) => request.id.clone(),
        ClientJsonRpcMessage::Notification(_)
        | ClientJsonRpcMessage::Response(_)
        | ClientJsonRpcMessage::Error(_) => {
            let delivered =
                tokio::time::timeout(HTTP_MESSAGE_SEND_TIMEOUT, session.incoming.send(message))
                    .await
                    .is_ok_and(|result| result.is_ok());
            if !delivered {
                drop(activity);
                remove_session(&sessions, session_id);
                return (StatusCode::NOT_FOUND, "MCP session is no longer available")
                    .into_response();
            }
            touch_session(&session);
            activity.complete();
            return with_session_header(StatusCode::ACCEPTED.into_response(), session_id);
        }
    };

    let request_guard = match tokio::time::timeout(
        HTTP_REQUEST_TIMEOUT,
        Arc::clone(&session.request_lock).lock_owned(),
    )
    .await
    {
        Ok(guard) => guard,
        Err(_) => {
            drop(activity);
            remove_session(&sessions, session_id);
            return with_session_header(
                (StatusCode::GATEWAY_TIMEOUT, "MCP request timed out").into_response(),
                session_id,
            );
        }
    };
    let delivered = tokio::time::timeout(HTTP_MESSAGE_SEND_TIMEOUT, session.incoming.send(message))
        .await
        .is_ok_and(|result| result.is_ok());
    if !delivered {
        drop(request_guard);
        drop(activity);
        remove_session(&sessions, session_id);
        return (StatusCode::NOT_FOUND, "MCP session is no longer available").into_response();
    }
    touch_session(&session);

    let mut outgoing = Arc::clone(&session.outgoing).lock_owned().await;
    let deadline = Instant::now() + HTTP_REQUEST_TIMEOUT;
    let Some(first) = tokio::time::timeout_at(deadline.into(), outgoing.recv())
        .await
        .ok()
        .flatten()
    else {
        drop(request_guard);
        drop(activity);
        remove_session(&sessions, session_id);
        return with_session_header(
            (StatusCode::GATEWAY_TIMEOUT, "MCP request timed out").into_response(),
            session_id,
        );
    };
    if terminal_response(&first, &request_id) {
        activity.complete();
        drop(outgoing);
        drop(request_guard);
        drop(activity);
        return with_session_header(axum::Json(first).into_response(), session_id);
    }

    let (sender, receiver) = tokio::sync::mpsc::channel(HTTP_SSE_BUFFER);
    let sessions_for_task = Arc::clone(&sessions);
    let session_id_for_task = session_id.to_string();
    let request_id_for_task = request_id.clone();
    tokio::spawn(forward_http_session(HttpForward {
        outgoing,
        first,
        request_id: request_id_for_task,
        deadline,
        sender,
        sessions: sessions_for_task,
        session_id: session_id_for_task,
        _request_guard: request_guard,
        activity,
    }));
    let stream = futures::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|payload| {
            (
                Ok::<Event, Infallible>(Event::default().data(payload)),
                receiver,
            )
        })
    });
    with_session_header(Sse::new(stream).into_response(), session_id)
}

fn with_session_header(mut response: Response, session_id: &str) -> Response {
    if let Ok(value) = session_id.parse() {
        response.headers_mut().insert(MCP_SESSION_ID_HEADER, value);
    }
    response
}

async fn handle(State(state): State<HttpState>, request: Request<Body>) -> Response {
    if let Err(response) = validate_host(request.headers(), state.bound_addr) {
        return *response;
    }
    let requested_session_id = match request.headers().get(MCP_SESSION_ID_HEADER) {
        None => None,
        Some(value) => match value.to_str() {
            Ok(value) if !value.is_empty() => Some(value.to_string()),
            _ => return bad_request("MCP session id is malformed"),
        },
    };
    let accepts = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.contains("application/json") && value.contains("text/event-stream")
        });
    if !accepts {
        return bad_request("MCP client must accept JSON and SSE");
    }
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if !content_type {
        return bad_request("MCP content type must be application/json");
    }
    let (_, body) = request.into_parts();
    let body = match to_bytes(body, MCP_BODY_LIMIT).await {
        Ok(body) => body,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let message = match serde_json::from_slice::<ClientJsonRpcMessage>(&body) {
        Ok(message) => message,
        Err(error) => {
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, error.to_string()).into_response();
        }
    };

    if let Some(session_id) = requested_session_id {
        return match reserve_http_session(&state.sessions, &session_id) {
            Some((session, activity)) => {
                dispatch_http_session(
                    state.sessions.clone(),
                    session,
                    message,
                    &session_id,
                    activity,
                )
                .await
            }
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }

    let is_initialize = matches!(
        &message,
        ClientJsonRpcMessage::Request(request)
            if matches!(
                &request.request,
                rmcp::model::ClientRequest::InitializeRequest(_)
            )
    );
    if !is_initialize {
        return bad_request("MCP session requires initialize request");
    }
    let pending = state.pending_sessions.fetch_add(1, Ordering::Relaxed);
    if pending >= MAX_PENDING_HTTP_SESSIONS {
        state.pending_sessions.fetch_sub(1, Ordering::Relaxed);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "MCP session admission limit reached",
        )
            .into_response();
    }
    let started = start_http_session(state.base_server.new_http_session(), message).await;
    state.pending_sessions.fetch_sub(1, Ordering::Relaxed);
    let Some((session, response)) = started else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "MCP handler returned no response",
        )
            .into_response();
    };
    if matches!(response, ServerJsonRpcMessage::Error(_)) {
        return axum::Json(response).into_response();
    }
    let session_id = new_session_id(&state);
    if let Ok(mut sessions) = state.sessions.lock() {
        reap_expired_sessions(&mut sessions);
        if sessions.len() >= MAX_HTTP_SESSIONS {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "MCP session admission limit reached",
            )
                .into_response();
        }
        sessions.insert(session_id.clone(), session);
    } else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "MCP session state unavailable",
        )
            .into_response();
    }
    with_session_header(axum::Json(response).into_response(), &session_id)
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
