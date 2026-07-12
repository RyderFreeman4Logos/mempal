#![cfg(feature = "rest")]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use mempal::api::ApiState;
#[cfg(feature = "db-test-seam")]
use mempal::core::AsyncDb;
use mempal::core::config::ConfigHandle;
use mempal::core::db::Database;
use mempal::core::types::{BootstrapEvidenceArgs, Drawer, SourceType};
use mempal::core::utils::iso_timestamp;
use mempal::embed::{EmbedError, Embedder, EmbedderFactory, global_embed_status};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct TestEnv {
    _tmp: TempDir,
    db_path: PathBuf,
    config_path: PathBuf,
}

impl TestEnv {
    fn new(config_suffix: &str) -> Self {
        Self::with_options(true, 30, config_suffix)
    }

    fn with_options(
        bm25_fallback: bool,
        search_db_deadline_secs: u64,
        config_suffix: &str,
    ) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open database");
        let config_path = mempal_home.join("config.toml");
        let config = format!(
            r#"
db_path = "{}"

[config_hot_reload]
enabled = false

[search]
bm25_fallback = {bm25_fallback}

[embed.retry]
search_deadline_secs = 30

[api]
search_db_deadline_secs = {search_db_deadline_secs}
{config_suffix}
"#,
            db_path.display()
        );
        fs::write(&config_path, config).expect("write config");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        global_embed_status().reset_for_tests();
        Self {
            _tmp: tmp,
            db_path,
            config_path,
        }
    }

    fn db(&self) -> Database {
        Database::open(&self.db_path).expect("open database")
    }

    fn state(&self, factory: Arc<dyn EmbedderFactory>) -> ApiState {
        ApiState::with_write_queue_config(self.db_path.clone(), factory, 10, Duration::from_secs(2))
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        global_embed_status().reset_for_tests();
        let _ = ConfigHandle::bootstrap(&self.config_path);
    }
}

#[derive(Clone)]
struct SlowEmbedderFactory;

struct SlowEmbedder;

#[async_trait]
impl EmbedderFactory for SlowEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(SlowEmbedder))
    }
}

#[async_trait]
impl Embedder for SlowEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        tokio::time::sleep(Duration::from_secs(5)).await;
        Ok(texts.iter().map(|_| vec![0.5; 4]).collect())
    }

    fn dimensions(&self) -> usize {
        4
    }

    fn name(&self) -> &str {
        "slow-search-budget-test"
    }
}

#[derive(Clone)]
struct FastEmbedderFactory;

struct FastEmbedder;

#[async_trait]
impl EmbedderFactory for FastEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(FastEmbedder))
    }
}

#[async_trait]
impl Embedder for FastEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.5; 4]).collect())
    }

    fn dimensions(&self) -> usize {
        4
    }

    fn name(&self) -> &str {
        "fast-search-budget-test"
    }
}

#[derive(Clone)]
struct FailingEmbedderFactory;

struct FailingEmbedder;

#[async_trait]
impl EmbedderFactory for FailingEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(FailingEmbedder))
    }
}

#[async_trait]
impl Embedder for FailingEmbedder {
    async fn embed(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Err(EmbedError::Runtime(
            "synthetic embedder failure".to_string(),
        ))
    }

    fn dimensions(&self) -> usize {
        4
    }

    fn name(&self) -> &str {
        "failing-search-budget-test"
    }
}

fn insert_search_drawer(db: &Database, id: &str, content: &str, importance: i32) {
    let drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: id.to_string(),
        content: content.to_string(),
        wing: "test".to_string(),
        room: Some("search".to_string()),
        source_file: Some("tests://search-budget".to_string()),
        source_type: SourceType::AgentInference,
        added_at: iso_timestamp(),
        chunk_index: Some(0),
        importance,
    });
    db.insert_drawer(&drawer).expect("insert search drawer");
}

async fn get_json(state: ApiState, uri: &str) -> (StatusCode, HeaderMap, Value) {
    let response = mempal::api::router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("REST response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read response body");
    let body = serde_json::from_slice(&bytes).expect("parse response body");
    (status, headers, body)
}

fn search_metadata(headers: &HeaderMap) -> Value {
    serde_json::from_str(
        headers
            .get("mempal-search-metadata")
            .and_then(|value| value.to_str().ok())
            .expect("search metadata header"),
    )
    .expect("parse search metadata")
}

#[tokio::test]
async fn slow_embedder_uses_bm25_within_single_caller_budget() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new("");
    insert_search_drawer(
        &env.db(),
        "drawer_slow_embed_fallback",
        "alpha exact durable preference",
        4,
    );
    let state = env.state(Arc::new(SlowEmbedderFactory));
    let started = StdInstant::now();
    let (status, headers, body) = get_json(
        state.clone(),
        "/api/search?q=alpha&scope=global&top_k=5&deadline_ms=2000&correlation_id=slow-embed-test",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(started.elapsed() < Duration::from_secs(3));
    let results = body.as_array().expect("search result array");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["drawer_id"], "drawer_slow_embed_fallback");
    assert_eq!(results[0]["search_mode"], "bm25_only");
    let metadata = search_metadata(&headers);
    assert_eq!(metadata["correlation_id"], "slow-embed-test");
    assert_eq!(metadata["deadline_ms"], 2000);
    assert_eq!(metadata["fallback_used"], json!(["bm25"]));
    assert_eq!(
        metadata["timeouts"],
        json!([{"stage": "embedding", "boundary": "daemon.embedding"}])
    );
    assert!(metadata["retry_safe"].as_bool().unwrap_or(false));
    #[cfg(feature = "db-test-seam")]
    {
        let telemetry = state.search_telemetry_snapshot_for_test();
        assert_eq!(telemetry.stage_timeout_counts.get("embedding"), Some(&1));
        assert_eq!(telemetry.fallback_counts.get("bm25"), Some(&1));
    }
}

#[tokio::test]
async fn embedding_timeout_without_bm25_fallback_returns_gateway_timeout() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::with_options(false, 30, "");
    insert_search_drawer(
        &env.db(),
        "drawer_embed_timeout_no_fallback",
        "alpha exact durable preference",
        4,
    );
    let state = env.state(Arc::new(SlowEmbedderFactory));
    let started = StdInstant::now();
    let (status, _headers, body) = get_json(
        state,
        "/api/search?q=alpha&scope=global&top_k=5&deadline_ms=2000&correlation_id=embed-no-fallback",
    )
    .await;

    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(body["error"]["status"], 504);
    assert_eq!(body["error"]["message"], "embedding deadline exceeded");
    assert!(
        body.as_array().is_none(),
        "fallback-disabled embedding timeout must not return an empty success array: {body:#}"
    );
}

#[cfg(feature = "db-test-seam")]
#[tokio::test]
async fn hybrid_db_timeout_without_bm25_fallback_returns_gateway_timeout() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::with_options(false, 1, "");
    insert_search_drawer(
        &env.db(),
        "drawer_hybrid_timeout_no_fallback",
        "alpha exact durable preference",
        4,
    );
    let async_db = AsyncDb::open(&env.db_path, 4)
        .expect("open async db")
        .with_read_delay(Duration::from_millis(1_500));
    let state = env
        .state(Arc::new(FastEmbedderFactory))
        .with_async_db_for_test(async_db);
    let started = StdInstant::now();
    let (status, _headers, body) = get_json(
        state,
        "/api/search?q=alpha&scope=global&top_k=5&deadline_ms=2000&correlation_id=hybrid-no-fallback",
    )
    .await;

    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(body["error"]["status"], 504);
    let message = body["error"]["message"].as_str().expect("error message");
    assert!(
        message.contains("hybrid search deadline exceeded"),
        "message={message}"
    );
    assert!(
        body.as_array().is_none(),
        "fallback-disabled hybrid timeout must not return an empty success array: {body:#}"
    );
}

#[tokio::test]
async fn slow_reranker_preserves_ranking_before_caller_budget_expires() {
    let _guard = TEST_LOCK.lock().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind delayed reranker");
    let addr = listener.local_addr().expect("reranker address");
    let reranker = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;

        let (mut stream, _) = listener.accept().await.expect("accept reranker request");
        let mut buffer = [0_u8; 4096];
        let _ = stream.read(&mut buffer).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let env = TestEnv::new(&format!(
        r#"
[search.reranker]
enabled = true
endpoint = "http://{addr}/v1/rerank"
model = "slow-test-reranker"
timeout_secs = 30
top_k = 50
"#,
    ));
    let db = env.db();
    insert_search_drawer(&db, "drawer_rank_a", "alpha primary memory", 4);
    insert_search_drawer(&db, "drawer_rank_b", "alpha secondary memory", 2);
    let state = env.state(Arc::new(FailingEmbedderFactory));
    let started = StdInstant::now();

    let (status, headers, body) = get_json(
        state.clone(),
        "/api/search?q=alpha&scope=global&top_k=5&deadline_ms=2000&correlation_id=slow-rerank-test",
    )
    .await;
    reranker.abort();

    assert_eq!(status, StatusCode::OK);
    assert!(started.elapsed() < Duration::from_secs(3));
    let results = body.as_array().expect("search result array");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["drawer_id"], "drawer_rank_a");
    let metadata = search_metadata(&headers);
    assert_eq!(metadata["correlation_id"], "slow-rerank-test");
    assert_eq!(metadata["deadline_ms"], 2000);
    assert_eq!(
        metadata["fallback_used"],
        json!(["bm25", "original_ranking"])
    );
    assert_eq!(
        metadata["timeouts"],
        json!([{"stage": "rerank", "boundary": "daemon.reranker"}])
    );
    let (status_code, _status_headers, status_body) = get_json(state.clone(), "/api/status").await;
    assert_eq!(status_code, StatusCode::OK);
    assert_eq!(
        status_body["search_telemetry"]["stage_timeout_counts"]["rerank"],
        1
    );
    assert_eq!(
        status_body["search_telemetry"]["fallback_counts"]["original_ranking"],
        1
    );
    #[cfg(feature = "db-test-seam")]
    {
        let telemetry = state.search_telemetry_snapshot_for_test();
        assert_eq!(telemetry.stage_timeout_counts.get("rerank"), Some(&1));
        assert_eq!(telemetry.fallback_counts.get("original_ranking"), Some(&1));
    }
}
