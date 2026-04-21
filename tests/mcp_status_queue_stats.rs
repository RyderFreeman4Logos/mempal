use std::fs;
use std::path::PathBuf;

use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::core::queue::{PendingMessageStore, QueueConfig};
use mempal::mcp::MempalMcpServer;
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
    assert!(response.queue_stats.oldest_pending_age_secs.is_some());
}
