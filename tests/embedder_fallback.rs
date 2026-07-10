#![cfg(all(feature = "integration", feature = "model2vec"))]

use std::fs;
use std::sync::{Arc, OnceLock};

use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::embed::global_embed_status;
use mempal::mcp::{IngestRequest, MempalMcpServer};
use tempfile::TempDir;

async fn test_guard() -> tokio::sync::OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await
}

fn config_text(db_path: &std::path::Path) -> String {
    format!(
        r#"
db_path = "{}"

[embed]
backend = "openai_compat"
fallback = "model2vec"
model = "minishlab/potion-base-8M"

[embed.openai_compat]
base_url = "http://127.0.0.1:9/v1"
model = "Qwen/Qwen3-Embedding-8B"
dim = 4096
request_timeout_secs = 1

[embed.retry]
interval_secs = 1
search_deadline_secs = 5
"#,
        db_path.display()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_embedder_fallback_to_model2vec_when_lan_unreachable() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    let config_path = mempal_home.join("config.toml");
    let hf_home = tmp.path().join("hf-home");
    let xdg_cache_home = tmp.path().join("xdg-cache");
    fs::create_dir_all(&hf_home).expect("create hf home");
    fs::create_dir_all(&xdg_cache_home).expect("create xdg cache home");
    let text = config_text(&db_path);
    fs::write(&config_path, &text).expect("write config");
    let config = Config::parse(&text).expect("parse config");
    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
    global_embed_status().reset_for_tests();
    // SAFETY: this test serializes environment mutation with a process-wide async mutex.
    unsafe {
        std::env::set_var("HF_HOME", &hf_home);
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("XDG_CACHE_HOME", &xdg_cache_home);
    }

    Database::open(&db_path).expect("initialize database fixture");
    let server = MempalMcpServer::new(db_path, config).expect("create MCP server");
    let response = server
        .ingest_json_for_test(
            serde_json::to_value(IngestRequest {
                content: "fallback path should still embed".to_string(),
                wing: "test".to_string(),
                room: Some("fallback".to_string()),
                dry_run: Some(false),
                ..IngestRequest::default()
            })
            .expect("serialize ingest request"),
        )
        .await
        .expect("fallback ingest should succeed");

    assert!(
        response
            .system_warnings
            .iter()
            .any(|warning| warning.message.contains("fallback active"))
    );
    // SAFETY: paired with the serialized mutation above.
    unsafe {
        std::env::remove_var("HF_HOME");
        std::env::remove_var("HOME");
        std::env::remove_var("XDG_CACHE_HOME");
    }
    global_embed_status().reset_for_tests();
}
