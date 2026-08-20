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

pub(super) fn service(server: MempalMcpServer) -> Router {
    Router::new().route("/", post(handle)).with_state(server)
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
    let body = match to_bytes(body, super::handlers::MAX_REST_INGEST_BODY_BYTES).await {
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
