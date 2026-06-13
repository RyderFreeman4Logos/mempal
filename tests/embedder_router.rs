use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mempal::core::config::Config;
use mempal::embed::{
    ConfiguredEmbedderFactory, EmbedError, EmbedderFactory, router::EmbeddingRouter,
};
use mockito::{Matcher, Server};
use tokio::sync::Barrier;

fn embedding_body(vector: &[f32]) -> String {
    serde_json::json!({
        "data": [
            {
                "embedding": vector
            }
        ]
    })
    .to_string()
}

fn pool_config(entries: &[(&str, &str, i32, usize)]) -> mempal::core::config::EmbedConfig {
    pool_full_config(entries).embed
}

fn pool_full_config(entries: &[(&str, &str, i32, usize)]) -> Config {
    let mut toml = String::from("[embed]\nbackend = \"openai_compat\"\n");
    for (id, base_url, priority, max_concurrent) in entries {
        toml.push_str(&format!(
            r#"
[[embed.endpoints]]
id = "{id}"
base_url = "{base_url}"
model = "Qwen/Qwen3-Embedding-8B"
dim = 3
priority = {priority}
max_concurrent = {max_concurrent}
"#
        ));
    }
    Config::parse(&toml).expect("parse embedding pool config")
}

#[tokio::test]
async fn test_embedding_router_primary_5xx_falls_back_to_secondary_success() {
    let mut primary = Server::new_async().await;
    let mut secondary = Server::new_async().await;
    let primary_mock = primary
        .mock("POST", "/v1/embeddings")
        .with_status(500)
        .with_body("server error")
        .create_async()
        .await;
    let secondary_mock = secondary
        .mock("POST", "/v1/embeddings")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "model": "Qwen/Qwen3-Embedding-8B",
            "input": ["hello"]
        })))
        .with_status(200)
        .with_body(embedding_body(&[0.1, 0.2, 0.3]))
        .create_async()
        .await;
    let config = pool_config(&[
        ("primary", &format!("{}/v1", primary.url()), 0, 1),
        ("secondary", &format!("{}/v1", secondary.url()), 10, 1),
    ]);
    let router = EmbeddingRouter::from_config(&config).expect("build embedding router");

    let response = router
        .embed_routed(&["hello"], None)
        .await
        .expect("fallback response");

    primary_mock.assert_async().await;
    secondary_mock.assert_async().await;
    assert_eq!(response.endpoint_id, "secondary");
    assert_eq!(response.vectors, vec![vec![0.1, 0.2, 0.3]]);
}

#[tokio::test]
async fn test_embedding_router_429_reset_seconds_marks_cooldown() {
    let mut primary = Server::new_async().await;
    let primary_mock = primary
        .mock("POST", "/v1/embeddings")
        .with_status(429)
        .with_body(r#"{"error":{"code":"model_cooldown","reset_seconds":7}}"#)
        .expect(1)
        .create_async()
        .await;
    let config = pool_config(&[("primary", &format!("{}/v1", primary.url()), 0, 1)]);
    let router = EmbeddingRouter::from_config(&config).expect("build embedding router");

    let error = router
        .embed_routed(&["hello"], None)
        .await
        .expect_err("cooldown should be retryable");
    let second = router
        .embed_routed(&["hello"], None)
        .await
        .expect_err("cached cooldown should avoid another request");

    primary_mock.assert_async().await;
    assert!(matches!(
        error,
        EmbedError::TemporarilyUnavailable {
            retry_after,
            ..
        } if retry_after == Duration::from_secs(7)
    ));
    assert!(error.is_retryable());
    assert!(matches!(
        second,
        EmbedError::TemporarilyUnavailable {
            retry_after,
            ..
        } if retry_after <= Duration::from_secs(7)
    ));
}

async fn spawn_counting_embedding_server(
    delay: Duration,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    use axum::{Json, Router, routing::post};

    let count = Arc::new(AtomicUsize::new(0));
    let count_for_handler = Arc::clone(&count);
    let app = Router::new().route(
        "/v1/embeddings",
        post(move || {
            let count = Arc::clone(&count_for_handler);
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(delay).await;
                Json(serde_json::json!({
                    "data": [
                        {
                            "embedding": [0.1, 0.2, 0.3]
                        }
                    ]
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind counting embedding server");
    let addr = listener.local_addr().expect("server addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve counting embedding server");
    });
    (format!("http://{addr}/v1"), count, handle)
}

#[tokio::test]
async fn test_embedding_router_same_priority_uses_each_endpoint_capacity_concurrently() {
    let (gb10_url, gb10_count, gb10_server) =
        spawn_counting_embedding_server(Duration::from_millis(200)).await;
    let (spark_url, spark_count, spark_server) =
        spawn_counting_embedding_server(Duration::from_millis(200)).await;
    let config = pool_config(&[("gb10", &gb10_url, 0, 4), ("spark", &spark_url, 0, 1)]);
    let router = Arc::new(EmbeddingRouter::from_config(&config).expect("build embedding router"));
    assert_eq!(router.pool_capacity(), 5);

    let barrier = Arc::new(Barrier::new(6));
    let mut tasks = Vec::new();
    for _ in 0..5 {
        let router = Arc::clone(&router);
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            router.embed_routed(&["hello"], None).await
        }));
    }
    barrier.wait().await;
    for task in tasks {
        task.await.expect("join").expect("embedding response");
    }
    gb10_server.abort();
    spark_server.abort();
    let _ = gb10_server.await;
    let _ = spark_server.await;

    assert_eq!(gb10_count.load(Ordering::SeqCst), 4);
    assert_eq!(spark_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_configured_embedder_factory_builds_share_endpoint_capacity() {
    let (gb10_url, gb10_count, gb10_server) =
        spawn_counting_embedding_server(Duration::from_millis(200)).await;
    let (spark_url, spark_count, spark_server) =
        spawn_counting_embedding_server(Duration::from_millis(200)).await;
    let config = pool_full_config(&[("gb10", &gb10_url, 0, 1), ("spark", &spark_url, 0, 1)]);
    let factory = ConfiguredEmbedderFactory::new(config);
    let first = factory.build().await.expect("first embedder");
    let second = factory.build().await.expect("second embedder");

    let barrier = Arc::new(Barrier::new(3));
    let first_barrier = Arc::clone(&barrier);
    let first_task = tokio::spawn(async move {
        first_barrier.wait().await;
        first.embed(&["hello"]).await
    });
    let second_barrier = Arc::clone(&barrier);
    let second_task = tokio::spawn(async move {
        second_barrier.wait().await;
        second.embed(&["hello"]).await
    });
    barrier.wait().await;
    first_task.await.expect("first join").expect("first embed");
    second_task
        .await
        .expect("second join")
        .expect("second embed");
    gb10_server.abort();
    spark_server.abort();
    let _ = gb10_server.await;
    let _ = spark_server.await;

    assert_eq!(gb10_count.load(Ordering::SeqCst), 1);
    assert_eq!(spark_count.load(Ordering::SeqCst), 1);
}
