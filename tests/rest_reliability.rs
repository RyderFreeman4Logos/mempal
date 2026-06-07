#![cfg(feature = "rest")]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header::CONTENT_TYPE};
use mempal::api::ApiState;
use mempal::core::config::ConfigHandle;
use mempal::core::db::Database;
use mempal::core::types::{BootstrapEvidenceArgs, Drawer, SourceType};
use mempal::core::utils::iso_timestamp;
use mempal::embed::{EmbedError, Embedder, EmbedderFactory, global_embed_status};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Notify;
use tower::ServiceExt;

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct TestEnv {
    _tmp: TempDir,
    db_path: PathBuf,
    config_path: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
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

[config_hot_reload]
enabled = false

[search]
bm25_fallback = true

[embed.retry]
search_deadline_secs = 5

[embed.degradation]
degrade_after_n_failures = 2
block_writes_when_degraded = true

[api]
write_queue_capacity = 10
write_drain_timeout_secs = 2
"#,
                db_path.display()
            ),
        )
        .expect("write config");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        global_embed_status().reset_for_tests();
        Self {
            _tmp: tmp,
            db_path,
            config_path,
        }
    }

    fn db(&self) -> Database {
        Database::open(&self.db_path).expect("open db")
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
struct StaticEmbedderFactory {
    dim: usize,
}

struct StaticEmbedder {
    dim: usize,
}

#[async_trait]
impl EmbedderFactory for StaticEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(StaticEmbedder { dim: self.dim }))
    }
}

#[async_trait]
impl Embedder for StaticEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.25; self.dim]).collect())
    }

    fn dimensions(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &str {
        "static-rest-test"
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
        Err(EmbedError::Runtime("embedder down".to_string()))
    }

    fn dimensions(&self) -> usize {
        4
    }

    fn name(&self) -> &str {
        "failing-rest-test"
    }
}

#[derive(Clone)]
struct BuildFailFactory;

#[async_trait]
impl EmbedderFactory for BuildFailFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Err(EmbedError::Runtime("embedder unavailable".to_string()))
    }
}

#[derive(Clone)]
struct BlockingEmbedderFactory {
    started: Arc<Notify>,
    released: Arc<Notify>,
    has_started: Arc<AtomicBool>,
}

struct BlockingEmbedder {
    started: Arc<Notify>,
    released: Arc<Notify>,
    has_started: Arc<AtomicBool>,
}

#[async_trait]
impl EmbedderFactory for BlockingEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(BlockingEmbedder {
            started: Arc::clone(&self.started),
            released: Arc::clone(&self.released),
            has_started: Arc::clone(&self.has_started),
        }))
    }
}

#[async_trait]
impl Embedder for BlockingEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.has_started.store(true, Ordering::SeqCst);
        self.started.notify_waiters();
        self.released.notified().await;
        Ok(texts.iter().map(|_| vec![0.5; 4]).collect())
    }

    fn dimensions(&self) -> usize {
        4
    }

    fn name(&self) -> &str {
        "blocking-rest-test"
    }
}

async fn get_json(state: ApiState, uri: &str) -> (StatusCode, axum::http::HeaderMap, Value) {
    let response = mempal::api::router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("REST request");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body = serde_json::from_slice(&bytes).expect("parse json");
    (status, headers, body)
}

async fn post_json(state: ApiState, uri: &str, body: Value) -> (StatusCode, Value) {
    let response = mempal::api::router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&body).expect("serialize body"),
                ))
                .expect("build request"),
        )
        .await
        .expect("REST request");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body = serde_json::from_slice(&bytes).expect("parse json");
    (status, body)
}

fn insert_search_drawer(db: &Database) {
    let drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: "drawer_rest_bm25_fallback".to_string(),
        content: "alpha fallback memory survives embedding outage".to_string(),
        wing: "test".to_string(),
        room: Some("search".to_string()),
        source_file: Some("tests://rest-reliability".to_string()),
        source_type: SourceType::AgentInference,
        added_at: iso_timestamp(),
        chunk_index: Some(0),
        importance: 3,
    });
    db.insert_drawer(&drawer).expect("insert drawer");
}

#[tokio::test]
async fn test_shutdown_drain_completes_pending() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let started = Arc::new(Notify::new());
    let released = Arc::new(Notify::new());
    let has_started = Arc::new(AtomicBool::new(false));
    let state = env.state(Arc::new(BlockingEmbedderFactory {
        started: Arc::clone(&started),
        released: Arc::clone(&released),
        has_started: Arc::clone(&has_started),
    }));

    let post_state = state.clone();
    let request = tokio::spawn(async move {
        post_json(
            post_state,
            "/api/ingest",
            json!({
                "content": "shutdown drain durable write",
                "wing": "test",
                "room": "shutdown",
            }),
        )
        .await
    });

    while !has_started.load(Ordering::SeqCst) {
        started.notified().await;
    }
    let drain_state = state.clone();
    let drain = tokio::spawn(async move { drain_state.drain_write_queue().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !drain.is_finished(),
        "drain should wait for in-flight write"
    );
    released.notify_waiters();

    assert!(drain.await.expect("join drain"));
    let (status, _body) = request.await.expect("join request");
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(env.db().drawer_count().expect("drawer count"), 1);
}

#[tokio::test]
async fn test_bm25_fallback_when_embedder_down() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    insert_search_drawer(&env.db());
    let state = env.state(Arc::new(FailingEmbedderFactory));

    let (status, headers, body) = get_json(state, "/api/search?q=alpha&top_k=5").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("degraded").and_then(|v| v.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        headers.get("search-mode").and_then(|v| v.to_str().ok()),
        Some("bm25_only")
    );
    let results = body.as_array().expect("search response array");
    assert_eq!(results[0]["search_mode"], "bm25_only");
    assert!(
        !results[0]["warnings"]
            .as_array()
            .expect("warnings")
            .is_empty()
    );
    assert_eq!(
        results[0]["warnings"][0].as_str(),
        Some(
            "embedding unavailable; using BM25-only search: embedding runtime error: embedder down (retry may help)"
        )
    );
    assert!(
        results[0]["content"]
            .as_str()
            .expect("content")
            .contains("fallback memory")
    );
}

#[tokio::test]
async fn test_status_shows_degraded_mode() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    for _ in 0..2 {
        global_embed_status().record_failure(&EmbedError::Runtime("down".to_string()));
    }
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));

    let (status, _headers, body) = get_json(state, "/api/status").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["embedding_status"], "degraded");
    assert_eq!(body["search_mode"], "bm25_only");
    let circuit = body["embedder_circuit"]
        .as_object()
        .expect("embedder circuit");
    assert_eq!(circuit.get("open").and_then(Value::as_bool), Some(true));
    assert_eq!(
        circuit.get("failure_count").and_then(Value::as_u64),
        Some(2)
    );
    assert_eq!(
        circuit.get("failure_threshold").and_then(Value::as_u64),
        Some(2)
    );
    assert_eq!(
        circuit
            .get("bm25_fallback_enabled")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        circuit.get("search_deadline_secs").and_then(Value::as_u64),
        Some(5)
    );
    assert_eq!(
        circuit.get("vector_search_mode").and_then(Value::as_str),
        Some("bm25_only")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_search_deadline_warning_surfaces_timeout_reason() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    insert_search_drawer(&env.db());
    let started = Arc::new(Notify::new());
    let released = Arc::new(Notify::new());
    let has_started = Arc::new(AtomicBool::new(false));
    let state = env.state(Arc::new(BlockingEmbedderFactory {
        started: Arc::clone(&started),
        released: Arc::clone(&released),
        has_started: Arc::clone(&has_started),
    }));

    tokio::time::pause();
    let request = tokio::spawn({
        let state = state.clone();
        async move { get_json(state, "/api/search?q=alpha&top_k=5").await }
    });

    while !has_started.load(Ordering::SeqCst) {
        started.notified().await;
    }
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(5)).await;

    let (status, headers, body) = request.await.expect("join search request");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("search-mode").and_then(|v| v.to_str().ok()),
        Some("bm25_only")
    );
    assert_eq!(
        headers.get("degraded").and_then(|v| v.to_str().ok()),
        Some("true")
    );
    let results = body.as_array().expect("search response array");
    assert_eq!(results[0]["search_mode"], "bm25_only");
    assert_eq!(
        results[0]["warnings"][0].as_str(),
        Some("embedding deadline exceeded after 5s; using BM25-only search (retry may help)")
    );
}

#[tokio::test]
async fn test_feature_flags_in_status() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));

    let (status, _headers, body) = get_json(state, "/api/status").await;

    assert_eq!(status, StatusCode::OK);
    let flags = body["feature_flags"].as_object().expect("feature flags");
    for key in [
        "typed_ingest",
        "pinned_facts",
        "compaction",
        "sleep_cycle",
        "crystallize",
        "intelligence_modes",
    ] {
        assert_eq!(flags.get(key).and_then(Value::as_bool), Some(true), "{key}");
    }
    assert!(body["hermes_compat_version"].as_str().is_some());
    let queue = body["write_queue"].as_object().expect("write queue");
    assert_eq!(queue.get("pending").and_then(Value::as_u64), Some(0));
    assert!(queue.contains_key("completed"));
    assert!(queue.contains_key("failed"));
}

#[tokio::test]
async fn test_availability_check_without_embedder() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let state = env.state(Arc::new(BuildFailFactory));

    let (status, _headers, body) = get_json(state, "/api/status").await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["db_size_bytes"].as_u64().is_some());
    assert!(body["embedding_status"].as_str().is_some());
}
