#[cfg(feature = "integration")]
mod common;

use std::fs;
use std::path::{Path, PathBuf};
#[cfg(feature = "integration")]
use std::{collections::HashMap, net::SocketAddr, time::Duration};

#[cfg(feature = "integration")]
use axum::{Json, Router, routing::get};
#[cfg(feature = "integration")]
use common::harness::McpStdio;
use mempal::core::compaction::merge_cluster;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::core::queue::{PendingMessageStore, QueueConfig, QueueFailureDisposition};
use mempal::core::types::{BootstrapEvidenceArgs, CompactionStrategy, Drawer, SourceType};
use mempal::mcp::MempalMcpServer;
#[cfg(feature = "integration")]
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

fn insert_drawer_with_source_type(db_path: &Path, id: &str, source_type: SourceType) {
    let db = Database::open(db_path).expect("open db for drawer insert");
    let drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: id.to_string(),
        content: format!("status source type fixture {id}"),
        wing: "mempal".to_string(),
        room: Some("status".to_string()),
        source_file: Some(format!("{id}.md")),
        source_type,
        added_at: "1700000000".to_string(),
        chunk_index: Some(0),
        importance: 1,
    });
    db.insert_drawer(&drawer).expect("insert drawer");
}

#[tokio::test]
async fn test_mcp_status_reports_llm_endpoint_pool() {
    let (tmp, db_path) = setup_home();
    let mempal_home = tmp.path().join(".mempal");
    let config_path = mempal_home.join("config.toml");
    fs::write(
        &config_path,
        format!(
            r#"
db_path = "{}"

[llm]
enabled = true

[[llm.endpoints]]
id = "primary"
base_url = "http://primary.local:8317/v1"
model = "primary-model"
priority = 10
max_concurrent = 2
retry_interval_secs = 3

[[llm.endpoints]]
id = "secondary"
base_url = "http://secondary.local:8317/v1"
model = "secondary-model"
priority = 10
max_concurrent = 3

[gating]
enabled = true

[gating.llm_judge]
enabled = true
"#,
            db_path.display()
        ),
    )
    .expect("write endpoint-pool config");
    ConfigHandle::bootstrap(&config_path).expect("bootstrap endpoint-pool config");
    let config = Config::load_from(&config_path).expect("load endpoint-pool config");

    let server = MempalMcpServer::new(db_path, config).expect("create MCP server");
    let response = server.mempal_status().await.expect("status").0;

    assert_eq!(
        response.llm_status.model.as_deref(),
        Some("primary=primary-model, secondary=secondary-model")
    );
    assert_eq!(
        response.ingest_gating_status.llm_model.as_deref(),
        Some("primary=primary-model, secondary=secondary-model")
    );
    assert_eq!(response.llm_status.endpoints.len(), 2);
    assert_eq!(response.llm_status.endpoints[0].id, "primary");
    assert_eq!(
        response.llm_status.endpoints[0].base_url,
        "http://primary.local:8317/v1"
    );
    assert_eq!(response.llm_status.endpoints[0].priority, 10);
    assert_eq!(response.llm_status.endpoints[0].retry_interval_secs, 3);
    assert_eq!(response.llm_status.endpoints[1].model, "secondary-model");
    assert_eq!(response.llm_status.endpoints[1].priority, 10);
    assert_eq!(response.llm_status.max_concurrent, 5);
    assert!(
        response.system_warnings.iter().any(|warning| {
            warning.source == "llm" && warning.message.contains("no judge endpoint is reachable")
        }),
        "{:?}",
        response.system_warnings
    );
}

#[cfg(feature = "integration")]
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

#[cfg(feature = "integration")]
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
    store.confirm(&done).expect("confirm");

    let server = MempalMcpServer::new(db_path, config).expect("create MCP server");
    let response = server.mempal_status().await.expect("status").0;

    assert!(response.db_holders.error.is_none());
    assert_eq!(response.queue_stats.pending, 1);
    assert_eq!(response.queue_stats.claimed, 0);
    assert_eq!(response.queue_stats.failed, 0);
    assert!((response.queue_stats.rate_per_min - 0.1).abs() < f64::EPSILON);
    assert!(response.queue_stats.avg_processing_ms.is_some());
    assert_eq!(response.queue_stats.eta_secs, Some(600));
    assert!(response.queue_stats.oldest_pending_age_secs.is_some());
}

#[tokio::test]
async fn test_mcp_status_headline_reflects_failed_queue() {
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
    let failed = store
        .claim_next("worker-failed", 60)
        .expect("claim")
        .expect("failed row");
    store
        .mark_failed_with_disposition(&failed, "boom", QueueFailureDisposition::Terminal)
        .expect("mark terminal failed");

    let server = MempalMcpServer::new(db_path, config).expect("create MCP server");
    let response = server.mempal_status().await.expect("status").0;

    assert_eq!(response.queue_stats.failed, 1);
    assert_eq!(response.embed_status.failed_count, 1);
    assert_eq!(response.embed_status.fail_count, 1);
    assert_eq!(response.embed_status.failure_count, 1);
    assert!(
        response.system_warnings.iter().any(|warning| {
            warning.source == "queue" && warning.message.contains("mempal reindex --failed")
        }),
        "{:?}",
        response.system_warnings
    );
}

#[tokio::test]
async fn test_mcp_status_live_queue_counts_ignore_stale_completion_op_state() {
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
    let failed_id = store.enqueue("hook_event", r#"{"n":1}"#).expect("enqueue");
    let failed = store
        .claim_next("worker-failed", 60)
        .expect("claim")
        .expect("failed row");
    assert_eq!(failed.id, failed_id);
    store
        .mark_failed_with_disposition(&failed, "boom", QueueFailureDisposition::Terminal)
        .expect("mark terminal failed");

    let db = Database::open(&db_path).expect("open db");
    db.conn()
        .execute_batch(
            r#"
            INSERT INTO pending_message_completions (
                message_id,
                kind,
                created_at,
                claimed_at,
                completed_at,
                processing_ms,
                op_state
            )
            VALUES
                ('history-running', 'hook_event', 1700000000, 1700000001, 1700000002, 1000, 'running'),
                ('history-queued', 'hook_event', 1700000003, 1700000004, 1700000005, 1000, 'queued');
            "#,
        )
        .expect("insert stale completion history");
    drop(db);

    let server = MempalMcpServer::new(db_path, config).expect("create MCP server");
    let response = server.mempal_status().await.expect("status").0;

    assert_eq!(response.queue_stats.pending, 0);
    assert_eq!(response.queue_stats.claimed, 0);
    assert_eq!(response.queue_stats.failed, 1);
    assert_eq!(response.embed_status.pending_count, 0);
    assert_eq!(response.embed_status.claimed_count, 0);
    assert_eq!(response.embed_status.failed_count, 1);
}

#[tokio::test]
async fn test_mcp_status_surfaces_source_type_distribution() {
    let (_tmp, db_path, config) = setup_env();
    insert_drawer_with_source_type(&db_path, "drawer-user", SourceType::UserExplicit);
    insert_drawer_with_source_type(&db_path, "drawer-hook", SourceType::SystemGenerated);

    let server = MempalMcpServer::new(db_path, config).expect("create MCP server");
    let response = server.mempal_status().await.expect("status").0;

    let user_count = response
        .source_type_distribution
        .iter()
        .find(|entry| entry.source_type == "user_explicit")
        .map(|entry| entry.count);
    let system_count = response
        .source_type_distribution
        .iter()
        .find(|entry| entry.source_type == "system_generated")
        .map(|entry| entry.count);
    assert_eq!(user_count, Some(1));
    assert_eq!(system_count, Some(1));
}

#[tokio::test]
async fn test_mcp_status_surfaces_consolidation_stats() {
    let (_tmp, db_path, config) = setup_env();
    let db = Database::open(&db_path).expect("open db");
    let mut ids = Vec::new();
    for id in ["compact-a", "compact-b", "compact-c"] {
        let drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
            id: id.to_string(),
            content: format!("compaction fixture {id}"),
            wing: "mempal".to_string(),
            room: Some("status".to_string()),
            source_file: Some(format!("{id}.md")),
            source_type: SourceType::AgentInference,
            added_at: "1700000000".to_string(),
            chunk_index: Some(0),
            importance: if id == "compact-a" { 5 } else { 1 },
        });
        db.insert_drawer(&drawer).expect("insert drawer");
        ids.push(id.to_string());
    }
    merge_cluster(&db, &ids, CompactionStrategy::RichestContent, false).expect("merge cluster");

    let server = MempalMcpServer::new(db_path, config).expect("create MCP server");
    let response = server.mempal_status().await.expect("status").0;

    assert_eq!(response.total_compacted_drawers, 2);
    assert_eq!(response.consolidation_runs, 1);
    assert!(response.last_consolidation_at.is_some());
}

#[cfg(feature = "integration")]
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
