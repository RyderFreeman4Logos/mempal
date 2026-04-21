use std::fs;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mempal::core::config::ConfigHandle;
use mempal::embed::{EmbedError, Embedder, EmbedderFactory, global_embed_status};
use mempal::mcp::{IngestRequest, MempalMcpServer, SearchRequest};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

async fn test_guard() -> tokio::sync::OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await
}

struct StubEmbedder;

#[async_trait]
impl Embedder for StubEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
    }

    fn dimensions(&self) -> usize {
        3
    }

    fn name(&self) -> &str {
        "stub"
    }
}

#[derive(Clone)]
struct StubEmbedderFactory;

#[async_trait]
impl EmbedderFactory for StubEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(StubEmbedder))
    }
}

fn bootstrap_config(tmp: &TempDir) -> std::path::PathBuf {
    let config_path = tmp.path().join("config.toml");
    fs::write(
        &config_path,
        r#"
db_path = "/tmp/mempal-test.db"

[embed]
backend = "openai_compat"

[embed.degradation]
degrade_after_n_failures = 10
block_writes_when_degraded = true

[embed.retry]
interval_secs = 1
search_deadline_secs = 5
"#,
    )
    .expect("write config");
    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
    config_path
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_degraded_state_blocks_mcp_writes() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let _config_path = bootstrap_config(&tmp);
    let db_path = tmp.path().join("palace.db");
    let server = MempalMcpServer::new_with_factory(db_path, Arc::new(StubEmbedderFactory));
    let status = global_embed_status();
    status.reset_for_tests();

    for index in 0..11 {
        status.record_failure(&format!("synthetic failure {index}"));
    }

    let error = match server
        .mempal_ingest(Parameters(IngestRequest {
            content: "blocked".to_string(),
            wing: "test".to_string(),
            room: Some("room".to_string()),
            source: None,
            project_id: None,
            dry_run: Some(false),
            importance: None,
        }))
        .await
    {
        Ok(_) => panic!("write should be blocked"),
        Err(error) => error,
    };

    let search = server
        .mempal_search(Parameters(SearchRequest {
            query: "blocked".to_string(),
            wing: None,
            room: None,
            top_k: Some(5),
            project_id: None,
            include_global: None,
            all_projects: None,
            disable_progressive: None,
        }))
        .await
        .expect("search should still work")
        .0;

    assert!(format!("{error:?}").contains("writes are paused"));
    assert!(!search.system_warnings.is_empty());
    assert!(
        search
            .system_warnings
            .iter()
            .any(|warning| warning.message.contains("degraded"))
    );

    status.reset_for_tests();
}
