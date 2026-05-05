mod common;

use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use axum::{Json, Router, routing::get};
use common::harness::McpStdio;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::core::queue::{PendingMessageStore, QueueConfig};
use mempal::mcp::MempalMcpServer;
use serde_json::{Value, json};
use tempfile::TempDir;

fn setup_home() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");
    (tmp, db_path)
}

fn setup_env() -> (TempDir, PathBuf, Config) {
    let (tmp, db_path) = setup_home();
    let mempal_home = tmp.path().join(".mempal");
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

async fn call_mcp_status(client: &mut McpStdio) -> Value {
    let result = match tokio::time::timeout(
        Duration::from_secs(5),
        client.call(
            "tools/call",
            json!({
                "name": "mempal_status",
                "arguments": {},
            }),
        ),
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            let stderr = client.stderr_lines().await.join("\n");
            panic!("call mempal_status failed: {error}\nstderr:\n{stderr}");
        }
        Err(_) => {
            let stderr = client.stderr_lines().await.join("\n");
            panic!("call mempal_status timed out\nstderr:\n{stderr}");
        }
    };
    result["structuredContent"].clone()
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
    let (_tmp, db_path) = setup_home();
    let (addr, handle) = spawn_models_server().await;
    let base_url = format!("http://{addr}/v1");
    let mut client = McpStdio::start(
        &db_path,
        HashMap::from([
            ("MEMPAL_TEST_EMBED_BASE_URL".to_string(), base_url.clone()),
            ("MEMPAL_TEST_LLM_BASE_URL".to_string(), base_url),
        ]),
    )
    .await
    .expect("start mcp stdio");
    tokio::time::timeout(Duration::from_secs(5), client.initialize())
        .await
        .expect("initialize timed out")
        .expect("initialize mcp client");
    let response = call_mcp_status(&mut client).await;
    client.shutdown().await.expect("shutdown mcp client");

    assert!(
        response["endpoint_health"]["embedding_reachable"]
            .as_bool()
            .expect("embedding_reachable bool"),
        "{response:#?}"
    );
    assert!(
        response["endpoint_health"]["embedding_latency_ms"]
            .as_u64()
            .is_some(),
        "{response:#?}"
    );
    assert!(
        response["endpoint_health"]["llm_reachable"]
            .as_bool()
            .expect("llm_reachable bool"),
        "{response:#?}"
    );
    assert!(
        response["endpoint_health"]["llm_latency_ms"]
            .as_u64()
            .is_some(),
        "{response:#?}"
    );

    handle.abort();
    let _ = handle.await;
}
