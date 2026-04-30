use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

use axum::{Json, Router, routing::get};
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::core::queue::{PendingMessageStore, QueueConfig};
use mempal::mcp::MempalMcpServer;
use serde_json::json;
use tempfile::TempDir;

fn setup_env() -> (TempDir, PathBuf, Config) {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");
    let config_path = mempal_home.join("config.toml");
    fs::write(
        &config_path,
        format!(
            r#"
db_path = "{}"
"#,
            db_path.display()
        ),
    )
    .expect("write config");
    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");

    let config = Config {
        db_path: db_path.display().to_string(),
        ..Config::default()
    };
    (tmp, db_path, config)
}

async fn spawn_models_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/v1/models",
        get(|| async { Json(json!({ "object": "list", "data": [] })) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (addr, handle)
}

#[tokio::test]
async fn test_mcp_status_surfaces_queue_stats() {
    let (_tmp, db_path, config) = setup_env();
    let store = PendingMessageStore::with_config(
        &db_path,
        QueueConfig {
            base_delay_ms: 0,
            max_delay_ms: 0,
            max_retries: 0,
        },
    )
    .expect("create store");

    store.enqueue("hook_event", r#"{"n":1}"#).expect("enqueue");
    store.enqueue("hook_event", r#"{"n":2}"#).expect("enqueue");
    let done = store
        .claim_next("worker-done", 60)
        .expect("claim")
        .expect("done");
    store.confirm(&done.id).expect("confirm");

    let server = MempalMcpServer::new(db_path, config);
    let response = server.mempal_status().await.expect("status").0;

    assert_eq!(response.queue_stats.pending, 1);
    assert_eq!(response.queue_stats.claimed, 0);
    assert_eq!(response.queue_stats.failed, 0);
    assert!((response.queue_stats.rate_per_min - 0.1).abs() < f64::EPSILON);
    assert!(response.queue_stats.avg_processing_ms.is_some());
    assert_eq!(response.queue_stats.eta_secs, Some(600));
    assert!(response.queue_stats.oldest_pending_age_secs.is_some());
}

#[tokio::test]
async fn test_mcp_status_surfaces_endpoint_health() {
    let (tmp, db_path, mut config) = setup_env();
    let (addr, handle) = spawn_models_server().await;
    let base_url = format!("http://{addr}/v1");
    let config_path = tmp.path().join(".mempal").join("config.toml");
    fs::write(
        &config_path,
        format!(
            r#"
db_path = "{}"

[embed]
backend = "openai_compat"

[embed.openai_compat]
base_url = "{}"
model = "embed-test"

[llm]
enabled = true
base_url = "{}"
model = "llm-test"
"#,
            db_path.display(),
            base_url,
            base_url
        ),
    )
    .expect("write config");
    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");

    config.embed.backend = "openai_compat".to_string();
    config.embed.openai_compat.base_url = Some(base_url.clone());
    config.embed.openai_compat.model = Some("embed-test".to_string());
    config.llm.enabled = true;
    config.llm.base_url = Some(base_url);
    config.llm.model = Some("llm-test".to_string());

    let server = MempalMcpServer::new(db_path, config);
    let response = server.mempal_status().await.expect("status").0;

    assert!(
        response.endpoint_health.embedding_reachable,
        "{response:#?}"
    );
    assert!(response.endpoint_health.embedding_latency_ms.is_some());
    assert!(response.endpoint_health.llm_reachable, "{response:#?}");
    assert!(response.endpoint_health.llm_latency_ms.is_some());

    handle.abort();
    let _ = handle.await;
}
