use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use mempal::core::config::Config;
use mempal::embed::{
    ConfiguredEmbedderFactory, EmbedError, EmbedderFactory, router::EmbeddingRouter,
    shared_embedder_runtime_snapshot,
};
use mockito::{Matcher, Server};
use tokio::sync::{Barrier, Notify};

fn shared_embedder_cache_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

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

async fn spawn_first_request_gated_embedding_server() -> (
    String,
    Arc<AtomicUsize>,
    Arc<Notify>,
    Arc<Notify>,
    tokio::task::JoinHandle<()>,
) {
    use axum::{Json, Router, routing::post};

    let count = Arc::new(AtomicUsize::new(0));
    let first_started = Arc::new(Notify::new());
    let release_first = Arc::new(Notify::new());
    let count_for_handler = Arc::clone(&count);
    let first_started_for_handler = Arc::clone(&first_started);
    let release_first_for_handler = Arc::clone(&release_first);
    let app = Router::new().route(
        "/v1/embeddings",
        post(move || {
            let count = Arc::clone(&count_for_handler);
            let first_started = Arc::clone(&first_started_for_handler);
            let release_first = Arc::clone(&release_first_for_handler);
            async move {
                let request_index = count.fetch_add(1, Ordering::SeqCst);
                if request_index == 0 {
                    first_started.notify_one();
                    release_first.notified().await;
                }
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
        .expect("bind gated embedding server");
    let addr = listener.local_addr().expect("server addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve gated embedding server");
    });
    (
        format!("http://{addr}/v1"),
        count,
        first_started,
        release_first,
        handle,
    )
}

async fn spawn_embedding_cooldown_server() -> (
    String,
    Arc<AtomicUsize>,
    Arc<Notify>,
    tokio::task::JoinHandle<()>,
) {
    use axum::{Router, http::StatusCode, routing::post};

    let count = Arc::new(AtomicUsize::new(0));
    let called = Arc::new(Notify::new());
    let count_for_handler = Arc::clone(&count);
    let called_for_handler = Arc::clone(&called);
    let app = Router::new().route(
        "/v1/embeddings",
        post(move || {
            let count = Arc::clone(&count_for_handler);
            let called = Arc::clone(&called_for_handler);
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                called.notify_one();
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    r#"{"error":{"code":"model_cooldown","reset_seconds":60}}"#,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind embedding cooldown server");
    let addr = listener.local_addr().expect("server addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve embedding cooldown server");
    });
    (format!("http://{addr}/v1"), count, called, handle)
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
async fn test_embedding_router_waits_for_saturated_healthy_endpoint_before_cooldown() {
    let (primary_url, primary_count, first_started, release_first, primary_server) =
        spawn_first_request_gated_embedding_server().await;
    let (cooldown_url, cooldown_count, cooldown_called, cooldown_server) =
        spawn_embedding_cooldown_server().await;
    let config = pool_config(&[
        ("primary", &primary_url, 0, 1),
        ("cooldown", &cooldown_url, 10, 1),
    ]);
    let router = Arc::new(EmbeddingRouter::from_config(&config).expect("build embedding router"));
    let first_router = Arc::clone(&router);
    let first_task = tokio::spawn(async move { first_router.embed_routed(&["first"], None).await });
    first_started.notified().await;

    let second_router = Arc::clone(&router);
    let second_task =
        tokio::spawn(async move { second_router.embed_routed(&["second"], None).await });
    cooldown_called.notified().await;
    release_first.notify_one();

    first_task
        .await
        .expect("first join")
        .expect("first embedding response");
    let second = tokio::time::timeout(Duration::from_secs(2), second_task)
        .await
        .expect("second request should complete after primary permit frees")
        .expect("second join")
        .expect("second embedding response");
    primary_server.abort();
    cooldown_server.abort();
    let _ = primary_server.await;
    let _ = cooldown_server.await;

    assert_eq!(second.endpoint_id, "primary");
    assert_eq!(primary_count.load(Ordering::SeqCst), 2);
    assert_eq!(cooldown_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_configured_embedder_factory_builds_share_endpoint_capacity() {
    let _cache_guard = shared_embedder_cache_test_lock().lock().await;
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

#[tokio::test]
async fn test_daemon_embedder_factory_remote_mode_avoids_local_model2vec() {
    let _cache_guard = shared_embedder_cache_test_lock().lock().await;
    let (base_url, request_count, server) =
        spawn_counting_embedding_server(Duration::from_millis(1)).await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("daemon-embedder-router.db");
    let config = Config::parse(&format!(
        r#"
db_path = "{}"

[embed]
backend = "model2vec"

[embed.openai_compat]
base_url = "{base_url}"
model = "Qwen/Qwen3-Embedding-8B"
dim = 3

[daemon]
embedder_mode = "remote"
"#,
        db_path.display()
    ))
    .expect("parse config");
    let factory = ConfiguredEmbedderFactory::new_for_daemon(config);
    let embedder = factory.build().await.expect("daemon embedder");

    let vectors = embedder.embed(&["hello"]).await.expect("embed remotely");
    server.abort();
    let _ = server.await;

    assert_eq!(vectors, vec![vec![0.1, 0.2, 0.3]]);
    assert_eq!(request_count.load(Ordering::SeqCst), 1);
    let snapshot = shared_embedder_runtime_snapshot();
    assert!(snapshot.loaded);
    assert_eq!(snapshot.backend.as_deref(), Some("openai_compat"));
    assert_eq!(snapshot.model.as_deref(), Some("Qwen/Qwen3-Embedding-8B"));
    assert_eq!(snapshot.dimensions, Some(3));
}
