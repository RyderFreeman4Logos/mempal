//! Integration tests for issue #57: MCP and REST ingest must chunk
//! content through the same token-aware chunker as the CLI pipeline.

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::embed::{EmbedError, Embedder, EmbedderFactory};
use mempal::mcp::{IngestRequest, MempalMcpServer};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

/// Embedder with a low `max_input_tokens` to force chunking on moderate content.
#[derive(Clone)]
struct SmallLimitEmbedderFactory {
    dim: usize,
    max_tokens: usize,
}

struct SmallLimitEmbedder {
    dim: usize,
    max_tokens: usize,
}

#[async_trait]
impl EmbedderFactory for SmallLimitEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(SmallLimitEmbedder {
            dim: self.dim,
            max_tokens: self.max_tokens,
        }))
    }
}

#[async_trait]
impl Embedder for SmallLimitEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.1_f32; self.dim]).collect())
    }

    fn dimensions(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &str {
        "small-limit-test"
    }

    fn max_input_tokens(&self) -> Option<usize> {
        Some(self.max_tokens)
    }

    fn estimate_tokens(&self, text: &str) -> usize {
        // Simple: 1 token per word (space-separated). This makes it easy
        // to control chunk boundaries in tests.
        text.split_whitespace().count()
    }
}

struct TestEnv {
    _tmp: TempDir,
    db_path: PathBuf,
    config_path: PathBuf,
}

impl TestEnv {
    fn new(chunker_max_tokens: usize) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let home = tmp.path().to_path_buf();
        let mempal_home = home.join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open db");
        let config_path = mempal_home.join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"
db_path = "{db_path}"

[config_hot_reload]
enabled = false

[chunker]
max_tokens = {chunker_max_tokens}
target_tokens = {target}
overlap_tokens = 0
"#,
                db_path = db_path.display(),
                target = chunker_max_tokens / 2,
            ),
        )
        .expect("write config");
        Self {
            _tmp: tmp,
            db_path,
            config_path,
        }
    }

    fn bootstrap_config(&self) -> Config {
        ConfigHandle::bootstrap(&self.config_path).expect("bootstrap config");
        Config::load_from(&self.config_path).expect("load config")
    }

    fn db(&self) -> Database {
        Database::open(&self.db_path).expect("open db")
    }
}

fn drawer_count(db: &Database) -> i64 {
    db.conn()
        .query_row("SELECT COUNT(*) FROM drawers", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count drawers")
}

fn vector_count(db: &Database) -> i64 {
    db.conn()
        .query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count vectors")
}

fn drawer_contents(db: &Database) -> Vec<String> {
    let mut stmt = db
        .conn()
        .prepare("SELECT content FROM drawers ORDER BY chunk_index ASC")
        .expect("prepare");
    stmt.query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect")
}

/// Generate content with exactly `n_words` space-separated words.
fn make_content(n_words: usize) -> String {
    (0..n_words)
        .map(|i| format!("word{i}"))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---- MCP tests ----

#[tokio::test]
async fn test_mcp_ingest_short_content_single_chunk() {
    // max_tokens=100, content has 10 words → single chunk
    let env = TestEnv::new(100);
    let config = env.bootstrap_config();
    let factory: Arc<dyn EmbedderFactory> = Arc::new(SmallLimitEmbedderFactory {
        dim: 4,
        max_tokens: 100,
    });
    let server = MempalMcpServer::new_with_factory_and_config(env.db_path.clone(), config, factory);

    let content = make_content(10);
    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content,
            wing: "test".to_string(),
            room: Some("chunker".to_string()),
            dry_run: Some(false),
            ..IngestRequest::default()
        }))
        .await
        .expect("mcp ingest")
        .0;

    assert!(!response.dropped, "should not be dropped");
    assert_eq!(response.chunk_count, 1, "short content = 1 chunk");
    assert_eq!(response.drawer_ids.len(), 1, "one drawer ID returned");
    assert_eq!(response.drawer_id, response.drawer_ids[0]);

    let db = env.db();
    assert_eq!(drawer_count(&db), 1);
    assert_eq!(vector_count(&db), 1);
}

#[tokio::test]
async fn test_mcp_ingest_large_content_multi_chunk() {
    // max_tokens=50 (after 32 safety margin → effective=18),
    // content has 100 words → should produce multiple chunks.
    let env = TestEnv::new(50);
    let config = env.bootstrap_config();
    let factory: Arc<dyn EmbedderFactory> = Arc::new(SmallLimitEmbedderFactory {
        dim: 4,
        max_tokens: 50,
    });
    let server = MempalMcpServer::new_with_factory_and_config(env.db_path.clone(), config, factory);

    let content = make_content(100);
    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content: content.clone(),
            wing: "test".to_string(),
            room: Some("chunker".to_string()),
            dry_run: Some(false),
            ..IngestRequest::default()
        }))
        .await
        .expect("mcp ingest")
        .0;

    assert!(!response.dropped, "should not be dropped");
    assert!(
        response.chunk_count >= 2,
        "100 words with max_tokens=50 must produce >=2 chunks, got {}",
        response.chunk_count
    );
    assert_eq!(
        response.drawer_ids.len(),
        response.chunk_count,
        "drawer_ids len must match chunk_count"
    );
    assert_eq!(response.drawer_id, response.drawer_ids[0]);

    let db = env.db();
    assert!(
        drawer_count(&db) >= 2,
        "must have >=2 drawers, got {}",
        drawer_count(&db)
    );
    assert_eq!(
        drawer_count(&db),
        vector_count(&db),
        "each drawer must have a matching vector"
    );

    // Verbatim invariant: every word from the original content must appear
    // in the chunks (in order). The chunker may consume whitespace at chunk
    // boundaries, so we verify word-level coverage rather than exact string
    // equality.
    let contents = drawer_contents(&db);
    let reconstructed = contents.join(" ");
    let original_words: Vec<&str> = content.split_whitespace().collect();
    let reconstructed_words: Vec<&str> = reconstructed.split_whitespace().collect();
    // All original words must be present (order preserved by chunk_index).
    for word in &original_words {
        assert!(
            reconstructed_words.contains(word),
            "word {word} missing from reconstructed chunks"
        );
    }
}

#[tokio::test]
async fn test_mcp_ingest_dry_run_returns_chunk_info() {
    let env = TestEnv::new(50);
    let config = env.bootstrap_config();
    let factory: Arc<dyn EmbedderFactory> = Arc::new(SmallLimitEmbedderFactory {
        dim: 4,
        max_tokens: 50,
    });
    let server = MempalMcpServer::new_with_factory_and_config(env.db_path.clone(), config, factory);

    let content = make_content(100);
    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content,
            wing: "test".to_string(),
            room: Some("chunker".to_string()),
            dry_run: Some(true),
            ..IngestRequest::default()
        }))
        .await
        .expect("mcp ingest dry_run")
        .0;

    assert!(
        response.chunk_count >= 2,
        "dry_run should still report chunk_count"
    );
    assert_eq!(response.drawer_ids.len(), response.chunk_count);
    // No drawers should be written in dry_run.
    let db = env.db();
    assert_eq!(drawer_count(&db), 0, "dry_run must not write drawers");
}

// ---- REST tests ----

#[cfg(feature = "rest")]
mod rest_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_rest_ingest_large_content_multi_chunk() {
        let env = TestEnv::new(50);
        let _config = env.bootstrap_config();
        let factory: Arc<dyn EmbedderFactory> = Arc::new(SmallLimitEmbedderFactory {
            dim: 4,
            max_tokens: 50,
        });
        let state = mempal::api::ApiState::new(env.db_path.clone(), factory);
        let app = mempal::api::router(state);

        let content = make_content(100);
        let body = serde_json::json!({
            "content": content,
            "wing": "test",
            "room": "chunker",
        });
        let request = Request::builder()
            .method("POST")
            .uri("/api/ingest")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let response: axum::http::Response<Body> = app.oneshot(request).await.expect("REST ingest");
        assert_eq!(response.status(), StatusCode::CREATED);

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let resp: serde_json::Value = serde_json::from_slice(&body_bytes).expect("parse json");

        let chunk_count = resp["chunk_count"].as_u64().unwrap_or(0);
        assert!(
            chunk_count >= 2,
            "REST ingest must produce >=2 chunks for 100 words with max_tokens=50, got {}",
            chunk_count
        );

        let drawer_ids = resp["drawer_ids"].as_array().expect("drawer_ids array");
        assert_eq!(drawer_ids.len() as u64, chunk_count);

        let db = env.db();
        assert!(drawer_count(&db) >= 2);
        assert_eq!(drawer_count(&db), vector_count(&db));
    }

    #[tokio::test]
    async fn test_rest_ingest_short_content_single_chunk() {
        let env = TestEnv::new(100);
        let _config = env.bootstrap_config();
        let factory: Arc<dyn EmbedderFactory> = Arc::new(SmallLimitEmbedderFactory {
            dim: 4,
            max_tokens: 100,
        });
        let state = mempal::api::ApiState::new(env.db_path.clone(), factory);
        let app = mempal::api::router(state);

        let content = make_content(10);
        let body = serde_json::json!({
            "content": content,
            "wing": "test",
            "room": "chunker",
        });
        let request = Request::builder()
            .method("POST")
            .uri("/api/ingest")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let response: axum::http::Response<Body> = app.oneshot(request).await.expect("REST ingest");
        assert_eq!(response.status(), StatusCode::CREATED);

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let resp: serde_json::Value = serde_json::from_slice(&body_bytes).expect("parse json");

        let chunk_count = resp["chunk_count"].as_u64().unwrap_or(0);
        assert_eq!(chunk_count, 1, "short content should produce 1 chunk");

        let db = env.db();
        assert_eq!(drawer_count(&db), 1);
        assert_eq!(vector_count(&db), 1);
    }
}
