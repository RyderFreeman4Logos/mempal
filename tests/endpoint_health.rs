use axum::{
    Json, Router,
    routing::{get, post},
};
use mempal::core::config::Config;
use mempal::endpoint_health::probe_endpoints;
use mockito::Server;
use serde_json::json;
use std::net::SocketAddr;
use std::time::Duration;

async fn spawn_llm_health_server(
    chat_delay: Duration,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route(
            "/v1/models",
            get(|| async { Json(json!({ "object": "list", "data": [] })) }),
        )
        .route(
            "/v1/chat/completions",
            post(move || async move {
                tokio::time::sleep(chat_delay).await;
                Json(json!({
                    "model": "probe-model",
                    "choices": [{
                        "message": {"role": "assistant", "content": "ok"}
                    }]
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind LLM health server");
    let addr = listener.local_addr().expect("health server addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve LLM health server");
    });
    (addr, handle)
}

#[tokio::test]
async fn test_endpoint_health_probes_llm_endpoint_pool() {
    let mut primary = Server::new_async().await;
    let mut secondary = Server::new_async().await;
    let primary_mock = primary
        .mock("GET", "/v1/models")
        .with_status(500)
        .with_body("primary unavailable")
        .create_async()
        .await;
    let secondary_mock = secondary
        .mock("GET", "/v1/models")
        .with_status(200)
        .with_body(r#"{"object":"list","data":[]}"#)
        .create_async()
        .await;
    let primary_generation_mock = primary
        .mock("POST", "/v1/chat/completions")
        .with_status(500)
        .with_body("primary generation unavailable")
        .create_async()
        .await;
    let secondary_generation_mock = secondary
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#)
        .create_async()
        .await;
    let config = Config::parse(&format!(
        r#"
[llm]
enabled = true

[[llm.endpoints]]
id = "primary"
base_url = "{}/v1"
model = "primary-model"

[[llm.endpoints]]
id = "secondary"
base_url = "{}/v1"
model = "secondary-model"
"#,
        primary.url(),
        secondary.url()
    ))
    .expect("parse endpoint pool");

    let health = probe_endpoints(&config).await;

    primary_mock.assert_async().await;
    secondary_mock.assert_async().await;
    primary_generation_mock.assert_async().await;
    secondary_generation_mock.assert_async().await;
    assert!(health.llm.reachable, "{health:#?}");
    assert_eq!(health.llm_control_plane.detail, "http probe via secondary");
    assert_eq!(
        health.llm_generation.detail,
        "generation probe via secondary"
    );
    assert_eq!(health.llm.detail, health.llm_generation.detail);
}

#[tokio::test]
async fn test_endpoint_health_distinguishes_llm_models_from_generation_timeout() {
    let (addr, handle) = spawn_llm_health_server(Duration::from_secs(5)).await;
    let config = Config::parse(&format!(
        r#"
[llm]
enabled = true
base_url = "http://{addr}/v1"
model = "probe-model"
health_probe_timeout_secs = 1
"#
    ))
    .expect("parse LLM health config");

    let health = probe_endpoints(&config).await;
    handle.abort();
    let _ = handle.await;

    assert!(health.llm_control_plane.reachable, "{health:#?}");
    assert!(!health.llm_generation.reachable, "{health:#?}");
    assert!(
        health.llm_generation.detail.contains("timeout")
            || health.llm_generation.detail.contains("timed out"),
        "{health:#?}"
    );
}

#[tokio::test]
async fn test_endpoint_health_probes_embedding_endpoint_pool() {
    let mut primary = Server::new_async().await;
    let mut secondary = Server::new_async().await;
    let primary_mock = primary
        .mock("GET", "/v1/models")
        .with_status(500)
        .with_body("primary unavailable")
        .create_async()
        .await;
    let secondary_mock = secondary
        .mock("GET", "/v1/models")
        .with_status(200)
        .with_body(r#"{"object":"list","data":[]}"#)
        .create_async()
        .await;
    let config = Config::parse(&format!(
        r#"
[embed]
backend = "openai_compat"

[[embed.endpoints]]
id = "primary"
base_url = "{}/v1"
model = "Qwen/Qwen3-Embedding-8B"

[[embed.endpoints]]
id = "secondary"
base_url = "{}/v1"
model = "Qwen/Qwen3-Embedding-8B"
"#,
        primary.url(),
        secondary.url()
    ))
    .expect("parse embedding endpoint pool");

    let health = probe_endpoints(&config).await;

    primary_mock.assert_async().await;
    secondary_mock.assert_async().await;
    assert!(health.embedding.reachable, "{health:#?}");
    assert_eq!(health.embedding.detail, "http probe via secondary");
}
