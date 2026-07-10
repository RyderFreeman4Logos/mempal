#![cfg(feature = "rest")]

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header::CONTENT_TYPE};
use mempal::api::ApiState;
#[cfg(feature = "db-test-seam")]
use mempal::core::AsyncDb;
use mempal::core::config::ConfigHandle;
use mempal::core::db::{CURRENT_SCHEMA_VERSION, Database};
use mempal::core::types::{BootstrapEvidenceArgs, Drawer, SourceType};
use mempal::core::utils::iso_timestamp;
use mempal::embed::{EmbedError, Embedder, EmbedderFactory, global_embed_status};
use mempal::observability::{
    IoOperationPath, OperationTelemetrySummaryOptions, operation_telemetry_summary,
    record_io_burst_sample, reset_io_burst_for_tests,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Notify;
use tower::ServiceExt;

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn json_string_values_contain(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(text) => text.contains(needle),
        Value::Array(values) => values
            .iter()
            .any(|value| json_string_values_contain(value, needle)),
        Value::Object(fields) => fields
            .values()
            .any(|value| json_string_values_contain(value, needle)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

struct TestEnv {
    _tmp: TempDir,
    db_path: PathBuf,
    config_path: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        Self::new_with_api_search_deadline_secs(30)
    }

    fn new_with_api_search_deadline_secs(api_search_deadline_secs: u64) -> Self {
        Self::new_with_config_suffix(api_search_deadline_secs, "")
    }

    fn new_with_config_suffix(api_search_deadline_secs: u64, config_suffix: &str) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open db");
        let config_path = mempal_home.join("config.toml");
        let mut config = format!(
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
search_db_deadline_secs = {api_search_deadline_secs}
"#,
            db_path.display()
        );
        if !config_suffix.trim().is_empty() {
            config.push('\n');
            config.push_str(config_suffix);
        }
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
        Database::open(&self.db_path).expect("open db")
    }

    fn force_future_schema(&self) {
        let conn = rusqlite::Connection::open(&self.db_path).expect("open raw sqlite");
        conn.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 1)
            .expect("set future schema version");
    }

    fn state(&self, factory: Arc<dyn EmbedderFactory>) -> ApiState {
        ApiState::with_write_queue_config(self.db_path.clone(), factory, 10, Duration::from_secs(2))
    }

    fn state_with_write_queue_byte_capacity(
        &self,
        factory: Arc<dyn EmbedderFactory>,
        byte_capacity: u64,
    ) -> ApiState {
        ApiState::with_write_queue_limits(
            self.db_path.clone(),
            factory,
            10,
            byte_capacity,
            Duration::from_secs(2),
        )
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

struct LogCapture {
    buffer: Arc<StdMutex<Vec<u8>>>,
}

impl Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer
            .lock()
            .expect("log mutex poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn install_log_capture() -> (Arc<StdMutex<Vec<u8>>>, tracing::dispatcher::DefaultGuard) {
    let logs = Arc::new(StdMutex::new(Vec::new()));
    let writer_logs = Arc::clone(&logs);
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(move || LogCapture {
            buffer: Arc::clone(&writer_logs),
        })
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (logs, guard)
}

fn captured_logs(logs: &Arc<StdMutex<Vec<u8>>>) -> String {
    String::from_utf8(logs.lock().expect("log mutex poisoned").clone()).expect("utf8 logs")
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
    get_json_with_user_agent(state, uri, None).await
}

async fn get_json_with_user_agent(
    state: ApiState,
    uri: &str,
    user_agent: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut request = Request::builder().method("GET").uri(uri);
    if let Some(user_agent) = user_agent {
        request = request.header("user-agent", user_agent);
    }
    let response = mempal::api::router(state)
        .oneshot(request.body(Body::empty()).expect("build request"))
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

#[tokio::test]
async fn test_ingest_rejects_oversized_content_with_413_without_leaking_content() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));
    let secret = "REST_OVERSIZE_SECRET_DO_NOT_LEAK";
    let mut content = "x".repeat(mempal::ingest::admission::MAX_INGEST_REQUEST_BYTES);
    content.push_str(secret);
    let content_bytes = content.len();

    let (status, body) = post_json(
        state.clone(),
        "/api/ingest",
        json!({
            "content": content,
            "wing": "test",
            "room": "oversize",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "body={body}");
    assert_eq!(body["error"]["kind"], "payload_too_large");
    let rendered = serde_json::to_string(&body).expect("serialize error");
    assert!(rendered.contains(&content_bytes.to_string()), "{rendered}");
    assert!(
        rendered.contains(&mempal::ingest::admission::MAX_INGEST_REQUEST_BYTES.to_string()),
        "{rendered}"
    );
    assert!(!rendered.contains(secret), "{rendered}");

    let (status, _headers, status_body) = get_json(state, "/api/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(status_body["queue_stats"]["rejected_oversize"], 1);
    assert_eq!(status_body["write_queue"]["rejected_oversize"], 1);
}

#[tokio::test]
async fn test_ingest_body_limit_rejection_uses_product_error_and_metrics() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));
    let body_bytes = mempal::api::MAX_REST_INGEST_BODY_BYTES + 1;
    let response = mempal::api::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/ingest")
                .header(CONTENT_TYPE, "application/json")
                .header("content-length", body_bytes)
                .body(Body::from(vec![b'x'; body_bytes]))
                .expect("build request"),
        )
        .await
        .expect("REST request");

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body: Value = serde_json::from_slice(&bytes).expect("parse structured error");
    assert_eq!(body["error"]["kind"], "payload_too_large");
    assert_eq!(body["error"]["retryable"], false);

    let (status, _headers, status_body) = get_json(state, "/api/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(status_body["queue_stats"]["rejected_oversize"], 1);
    assert_eq!(status_body["write_queue"]["rejected_oversize"], 1);
}

#[tokio::test(flavor = "current_thread")]
async fn test_concurrent_medium_ingests_respect_write_queue_byte_budget() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let started = Arc::new(Notify::new());
    let released = Arc::new(Notify::new());
    let has_started = Arc::new(AtomicBool::new(false));
    let state = env.state_with_write_queue_byte_capacity(
        Arc::new(BlockingEmbedderFactory {
            started: Arc::clone(&started),
            released: Arc::clone(&released),
            has_started: Arc::clone(&has_started),
        }),
        1_000,
    );
    let content = "m".repeat(600);

    let first = tokio::spawn({
        let state = state.clone();
        let content = content.clone();
        async move {
            post_json(
                state,
                "/api/ingest",
                json!({"content": content, "wing": "test", "room": "budget"}),
            )
            .await
        }
    });
    while !has_started.load(Ordering::SeqCst) {
        started.notified().await;
    }

    let (second_status, second_body) = post_json(
        state.clone(),
        "/api/ingest",
        json!({"content": content, "wing": "test", "room": "budget"}),
    )
    .await;

    assert_eq!(
        second_status,
        StatusCode::SERVICE_UNAVAILABLE,
        "body={second_body}"
    );
    assert_eq!(second_body["error"]["kind"], "queue_byte_budget");
    assert_eq!(second_body["error"]["retryable"], true);

    let (status, _headers, status_body) = get_json(state, "/api/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(status_body["write_queue"]["pending_bytes"], 600);
    assert_eq!(status_body["write_queue"]["byte_capacity"], 1_000);
    assert_eq!(status_body["write_queue"]["rejected_oversize"], 1);
    assert_eq!(status_body["queue_stats"]["rejected_oversize"], 1);

    released.notify_waiters();
    let (first_status, first_body) = first.await.expect("join first request");
    assert_eq!(first_status, StatusCode::CREATED, "body={first_body}");
}

#[tokio::test]
async fn test_rest_request_records_operation_telemetry_without_query_or_body() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));

    let (status, _headers, _body) = get_json(
        state,
        "/api/status?secret=REST_TELEMETRY_SECRET_DO_NOT_STORE",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let db = env.db();
    let rows = operation_telemetry_summary(
        &db,
        OperationTelemetrySummaryOptions {
            since_unix_ms: None,
            limit: 20,
        },
    )
    .expect("summarize operation telemetry");
    let rest_row = rows
        .iter()
        .find(|row| row.source == "rest" && row.operation == "GET /api/status")
        .expect("REST status request telemetry row");
    assert_eq!(rest_row.call_site, "rest.request");
    assert_eq!(rest_row.operation_count, 1);
    assert_eq!(rest_row.success_count, 1);
    assert_eq!(rest_row.error_count, 0);
    assert!(!rest_row.operation.contains("secret="));
}

#[tokio::test]
async fn test_ingest_rejects_future_schema_without_leaking_content() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    env.force_future_schema();
    let (logs, _log_guard) = install_log_capture();
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));
    let secret = "REST_SCHEMA_SKEW_SECRET_503_DO_NOT_LEAK";

    let (status, body) = post_json(
        state,
        "/api/ingest",
        json!({
            "content": format!("write should be rejected before queueing {secret}"),
            "wing": "test",
            "room": "schema",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body={body}");
    let rendered = serde_json::to_string(&body).expect("serialize body");
    assert!(rendered.contains("schema_skew"), "{rendered}");
    assert!(rendered.contains("restart"), "{rendered}");
    assert!(rendered.contains("supported_schema_version"), "{rendered}");
    assert!(!rendered.contains(secret), "{rendered}");
    let logs = captured_logs(&logs);
    assert!(!logs.contains(secret), "{logs}");
    assert!(!logs.contains("drawer_content"), "{logs}");
}

#[tokio::test(flavor = "current_thread")]
async fn test_worker_schema_skew_log_uses_metadata_not_drawer_content() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let (logs, _log_guard) = install_log_capture();
    let started = Arc::new(Notify::new());
    let released = Arc::new(Notify::new());
    let has_started = Arc::new(AtomicBool::new(false));
    let state = env.state(Arc::new(BlockingEmbedderFactory {
        started: Arc::clone(&started),
        released: Arc::clone(&released),
        has_started: Arc::clone(&has_started),
    }));

    let first = tokio::spawn({
        let state = state.clone();
        async move {
            post_json(
                state,
                "/api/ingest",
                json!({
                    "content": "first queued write blocks the worker",
                    "wing": "test",
                    "room": "schema",
                }),
            )
            .await
        }
    });

    while !has_started.load(Ordering::SeqCst) {
        started.notified().await;
    }

    let secret = "WORKER_SCHEMA_SKEW_SECRET_503_DO_NOT_LEAK";
    let second = tokio::spawn({
        let state = state.clone();
        async move {
            post_json(
                state,
                "/api/ingest",
                json!({
                    "content": format!("second write should fail after enqueue {secret}"),
                    "wing": "test",
                    "room": "schema",
                }),
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !second.is_finished(),
        "second write should be queued behind the blocking first write"
    );

    env.force_future_schema();
    released.notify_waiters();

    let (first_status, first_body) = first.await.expect("join first write");
    assert_eq!(first_status, StatusCode::CREATED, "body={first_body}");
    let (second_status, second_body) = second.await.expect("join second write");
    assert_eq!(
        second_status,
        StatusCode::SERVICE_UNAVAILABLE,
        "body={second_body}"
    );
    let rendered = serde_json::to_string(&second_body).expect("serialize body");
    assert!(rendered.contains("schema_skew"), "{rendered}");
    assert!(!rendered.contains(secret), "{rendered}");

    let logs = captured_logs(&logs);
    assert!(logs.contains("REST write failed"), "{logs}");
    assert!(logs.contains("content_len"), "{logs}");
    assert!(logs.contains("content_hash_prefix"), "{logs}");
    assert!(logs.contains("schema_skew"), "{logs}");
    assert!(!logs.contains(secret), "{logs}");
    assert!(!logs.contains("drawer_content"), "{logs}");
    assert!(!logs.contains("manual recovery"), "{logs}");
}

#[tokio::test]
async fn test_status_reports_schema_skew_restart_warning() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    env.force_future_schema();
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));

    let (status, _headers, body) = get_json(state, "/api/status?diagnostic=true").await;

    assert_eq!(status, StatusCode::OK);
    let warnings = body["status_warnings"].as_array().expect("status warnings");
    assert!(
        warnings.iter().any(|warning| {
            warning.as_str().is_some_and(|warning| {
                warning.contains("palace.db schema")
                    && warning.contains("newer than this daemon supports")
                    && warning.contains("mempal daemon restart")
            })
        }),
        "status warnings should surface schema skew: {body:#}"
    );
    assert_eq!(body["drawer_count"].as_i64(), Some(0));
}

#[tokio::test]
async fn test_status_default_is_cheap_and_diagnostic_populates_db_snapshot() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    insert_search_drawer(&env.db());
    let db_snapshot_calls = Arc::new(AtomicU64::new(0));
    let state = env
        .state(Arc::new(StaticEmbedderFactory { dim: 4 }))
        .with_bounded_read_counter_for_test(Arc::clone(&db_snapshot_calls));

    let (status, _headers, body) = get_json(state.clone(), "/api/status").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["diagnostic"].as_bool(), Some(false));
    assert_eq!(
        body["turn_storage"]["storage_mode"].as_str(),
        Some("raw_evidence")
    );
    assert!(
        body.get("drawer_count").is_none(),
        "default cheap status must omit diagnostic drawer_count: {body:#}"
    );
    assert!(
        body.get("taxonomy_count").is_none(),
        "default cheap status must omit diagnostic taxonomy_count: {body:#}"
    );
    assert!(
        body.get("db_size_bytes").is_none(),
        "default cheap status must omit diagnostic db_size_bytes: {body:#}"
    );
    assert!(
        body["turn_storage"].get("raw_turn_count").is_none(),
        "default cheap status must omit diagnostic raw_turn_count: {body:#}"
    );
    assert!(
        body.get("wings").is_none(),
        "default cheap status must omit diagnostic scope counts: {body:#}"
    );
    assert!(
        body.get("source_type_distribution").is_none(),
        "default cheap status must omit diagnostic source type counts: {body:#}"
    );
    assert!(body.get("status_warnings").is_none());
    assert_eq!(
        db_snapshot_calls.load(Ordering::SeqCst),
        0,
        "default /api/status must not trigger the heavy bounded DB snapshot collector"
    );

    let (status, _headers, body) = get_json(state, "/api/status?diagnostic=true").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["diagnostic"].as_bool(), Some(true));
    assert_eq!(body["drawer_count"].as_i64(), Some(1));
    assert_eq!(body["taxonomy_count"].as_i64(), Some(0));
    assert!(
        body["db_size_bytes"].as_i64().is_some(),
        "diagnostic status should include DB size: {body:#}"
    );
    assert_eq!(body["turn_storage"]["raw_turn_count"].as_i64(), Some(0));
    assert_eq!(
        db_snapshot_calls.load(Ordering::SeqCst),
        1,
        "diagnostic /api/status should run the DB snapshot collector exactly once"
    );
    assert_eq!(
        body["turn_storage"]["storage_mode"].as_str(),
        Some("raw_evidence")
    );
    assert!(
        body["wings"]
            .as_array()
            .expect("diagnostic wings")
            .iter()
            .any(|scope| scope["wing"].as_str() == Some("test")
                && scope["drawer_count"].as_i64() == Some(1)),
        "diagnostic status should include DB scope counts: {body:#}"
    );
}

#[tokio::test]
async fn test_status_includes_vector_scan_and_backoff_telemetry() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));
    reset_io_burst_for_tests();
    record_io_burst_sample(IoOperationPath::Status, 4096, 8192, 2);

    let (status, _headers, body) = get_json(state, "/api/status").await;

    assert_eq!(status, StatusCode::OK);

    let io_burst = body["io_burst"].as_object().expect("io burst");
    assert_eq!(
        io_burst.get("total_read_bytes").and_then(Value::as_u64),
        Some(4096)
    );
    let paths = io_burst["paths"].as_array().expect("io burst paths");
    assert!(
        paths.iter().any(|path| {
            path.get("path").and_then(Value::as_str) == Some("status")
                && path.get("sample_count").and_then(Value::as_u64) == Some(1)
                && path.get("peak_read_bytes_per_sec").and_then(Value::as_u64) == Some(2_048_000)
        }),
        "status io burst path should be exposed without content: {io_burst:#?}"
    );

    let vector_scan = body["vector_scan"].as_object().expect("vector scan");
    assert_eq!(
        vector_scan.get("mode").and_then(Value::as_str),
        None,
        "vector scan mode should be absent until a search records one"
    );
    assert_eq!(
        vector_scan.get("candidate_count").and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        vector_scan.get("candidate_cap").and_then(Value::as_u64),
        Some(0)
    );

    let backoff = body["ingest_worker_backoff"]
        .as_object()
        .expect("ingest worker backoff");
    assert_eq!(backoff.get("retry_count").and_then(Value::as_u64), Some(0));
    assert_eq!(
        backoff.get("next_delay_ms").and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        backoff.get("last_error_class").and_then(Value::as_str),
        None
    );
    reset_io_burst_for_tests();
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

#[cfg(feature = "db-test-seam")]
async fn wait_for_active_searches(state: &ApiState, expected: usize) {
    let mut latest_active_count = 0;
    for _ in 0..100 {
        let telemetry = state.search_telemetry_snapshot_for_test();
        latest_active_count = telemetry.active_count;
        if latest_active_count >= expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected at least {expected} active searches, saw {latest_active_count}");
}

#[cfg(feature = "db-test-seam")]
async fn wait_for_active_search_stage(state: &ApiState, expected: usize, stage: &str) {
    let mut latest_stage_count = 0;
    for _ in 0..100 {
        let telemetry = state.search_telemetry_snapshot_for_test();
        latest_stage_count = telemetry
            .active_searches
            .iter()
            .filter(|search| search.stage == stage)
            .count();
        if latest_stage_count >= expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected at least {expected} active searches in {stage}, saw {latest_stage_count}");
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
        global_embed_status().record_failure(&EmbedError::Runtime(
            "down url=http://user:pass@127.0.0.1:18002/v1/private-token-path?api_key=sk-secret-should-not-print"
                .to_string(),
        ));
    }
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));

    let (status, _headers, body) = get_json(state, "/api/status").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["embedding_status"], "degraded");
    assert_eq!(body["search_mode"], "bm25_only");
    assert_eq!(body["embed_status"]["degraded"], true);
    assert_eq!(body["embed_status"]["block_writes_when_degraded"], true);
    assert_eq!(body["embed_status"]["write_refused"], true);
    assert_eq!(body["embed_status"]["fail_count"], 2);
    assert_eq!(body["embed_status"]["failure_count"], 2);
    assert_eq!(body["embed_status"]["failed_count"], 0);
    assert!(
        body["embed_status"]["last_error"]
            .as_str()
            .expect("last error")
            .contains("http://127.0.0.1:18002")
    );
    let rendered = serde_json::to_string(&body["embed_status"]).expect("serialize embed status");
    assert!(!rendered.contains("user:pass"), "{rendered}");
    assert!(!rendered.contains("private-token-path"), "{rendered}");
    assert!(!rendered.contains("api_key"), "{rendered}");
    assert!(
        !rendered.contains("sk-secret-should-not-print"),
        "{rendered}"
    );
    assert_eq!(body["queue_stats"]["failed_retryable_embed"], 0);
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

#[tokio::test]
async fn test_status_redacts_blocked_remote_embedding_endpoint_identity() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new_with_config_suffix(
        30,
        r#"
[privacy.remote_calls]
fail_closed = true

[embed]
backend = "openai_compat"
base_url = "https://api.openai.com:9443/v1/private-embed-path"
api_model = "text-embedding-3-large"

[embed.openai_compat]
api_key_env = "MEMPAL_SECRET_TOKEN_ENV"

[llm]
enabled = true
base_url = "https://llm.example.com:9444/v1/private-chat-path"
model = "judge"
api_key = "sk-secret-should-not-print"
enabled_for = ["gating"]

[search.reranker]
enabled = true
endpoint = "https://rerank.example.com:9445/private-rerank-path"
model = "rerank"
"#,
    );
    global_embed_status().record_endpoint_cooldown(
        "legacy",
        Duration::from_secs(30),
        &EmbedError::Runtime(
            "failed https://api.openai.com:9443/v1/private-embed-path?api_key=sk-secret-should-not-print MEMPAL_SECRET_TOKEN_ENV"
                .to_string(),
        ),
    );
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));

    let (status, _headers, body) = get_json(state, "/api/status").await;
    let rendered = serde_json::to_string(&body).expect("serialize status body");

    assert_eq!(status, StatusCode::OK);
    let endpoints = body["embedding_endpoints"]
        .as_array()
        .expect("embedding endpoints");
    assert_eq!(endpoints.len(), 1, "{rendered}");
    assert_eq!(
        endpoints[0]["base_url"].as_str(),
        Some(mempal::core::remote_calls::BLOCKED_REMOTE_ENDPOINT_LABEL),
        "{rendered}"
    );
    assert!(endpoints[0]["last_error"].is_null(), "{rendered}");
    assert!(rendered.contains(mempal::core::remote_calls::BLOCKED_REMOTE_ENDPOINT_LABEL));
    assert!(!rendered.contains("api.openai.com"), "{rendered}");
    assert!(!json_string_values_contain(&body, "9443"), "{rendered}");
    assert!(!rendered.contains("private-embed-path"), "{rendered}");
    assert!(!rendered.contains("api_key"), "{rendered}");
    assert!(
        !rendered.contains("sk-secret-should-not-print"),
        "{rendered}"
    );
    assert!(!rendered.contains("MEMPAL_SECRET_TOKEN_ENV"), "{rendered}");
    assert!(!rendered.contains("llm.example.com"), "{rendered}");
    assert!(!json_string_values_contain(&body, "9444"), "{rendered}");
    assert!(!rendered.contains("private-chat-path"), "{rendered}");
    assert!(!rendered.contains("rerank.example.com"), "{rendered}");
    assert!(!json_string_values_contain(&body, "9445"), "{rendered}");
    assert!(!rendered.contains("private-rerank-path"), "{rendered}");
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
async fn test_status_exposes_active_search_client_telemetry() {
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

    let request = tokio::spawn({
        let state = state.clone();
        async move {
            get_json_with_user_agent(
                state,
                "/api/search?q=alpha&scope=global&top_k=5",
                Some("hermes-test"),
            )
            .await
        }
    });

    while !has_started.load(Ordering::SeqCst) {
        started.notified().await;
    }

    let (status, _headers, body) = get_json(state.clone(), "/api/status").await;
    assert_eq!(status, StatusCode::OK);
    let telemetry = body["search_telemetry"]
        .as_object()
        .expect("search telemetry");
    assert_eq!(
        telemetry.get("active_count").and_then(Value::as_u64),
        Some(1)
    );
    let active = telemetry["active_searches"]
        .as_array()
        .expect("active searches");
    assert_eq!(active[0]["client"].as_str(), Some("rest:hermes-test"));
    assert_eq!(active[0]["stage"].as_str(), Some("embedding"));
    assert!(
        active[0]["scope"]
            .as_str()
            .expect("scope label")
            .contains("scope=global")
    );

    request.abort();
    assert!(
        request
            .await
            .expect_err("search request should be cancelled")
            .is_cancelled()
    );
    released.notify_waiters();
}

#[cfg(feature = "db-test-seam")]
#[tokio::test]
async fn test_search_db_deadline_returns_partial_warning_and_telemetry() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new_with_api_search_deadline_secs(1);
    insert_search_drawer(&env.db());
    let async_db = AsyncDb::open(&env.db_path, 4)
        .expect("open async db")
        .with_read_delay(Duration::from_millis(1_500));
    let state = env
        .state(Arc::new(FailingEmbedderFactory))
        .with_async_db_for_test(async_db);

    let (status, headers, body) =
        get_json(state.clone(), "/api/search?q=alpha&scope=global&top_k=5").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("search-mode").and_then(|v| v.to_str().ok()),
        Some("bm25_only")
    );
    let warning = headers
        .get("mempal-warnings")
        .and_then(|v| v.to_str().ok())
        .expect("warning header");
    assert!(
        warning.contains("deadline exceeded after 1s"),
        "warning header={warning}"
    );
    assert!(body.as_array().expect("search response array").is_empty());

    let telemetry = state.search_telemetry_snapshot_for_test();
    assert!(
        !telemetry.slow_queries.is_empty(),
        "slow query telemetry missing: {telemetry:#?}"
    );
    assert!(telemetry.slow_queries[0].partial);
    assert_eq!(telemetry.slow_queries[0].search_mode, "bm25_only");
    assert_eq!(telemetry.slow_queries[0].warning_count, 3);
}

#[cfg(feature = "db-test-seam")]
#[tokio::test]
async fn test_status_returns_telemetry_when_search_db_pool_is_saturated() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    insert_search_drawer(&env.db());
    let async_db = AsyncDb::open(&env.db_path, 4)
        .expect("open async db")
        .with_read_delay(Duration::from_secs(5));
    let state = env
        .state(Arc::new(FailingEmbedderFactory))
        .with_async_db_for_test(async_db);

    let mut requests = Vec::new();
    for _ in 0..4 {
        let request_state = state.clone();
        requests.push(tokio::spawn(async move {
            get_json(request_state, "/api/search?q=alpha&scope=global&top_k=5").await
        }));
    }
    wait_for_active_searches(&state, 4).await;
    wait_for_active_search_stage(&state, 4, "routing").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (status, _headers, body) = tokio::time::timeout(
        Duration::from_millis(1_800),
        get_json(state.clone(), "/api/status"),
    )
    .await
    .expect("status should return before daemon status 2s timeout");

    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("drawer_count").is_none(),
        "default cheap status must omit diagnostic drawer_count: {body:#}"
    );
    assert!(
        body.get("taxonomy_count").is_none(),
        "default cheap status must omit diagnostic taxonomy_count: {body:#}"
    );
    assert!(
        body.get("db_size_bytes").is_none(),
        "default cheap status must omit diagnostic db_size_bytes: {body:#}"
    );
    assert!(
        body["turn_storage"].get("raw_turn_count").is_none(),
        "default cheap status must omit diagnostic raw_turn_count: {body:#}"
    );
    assert!(
        body.get("wings").is_none(),
        "default cheap status must omit diagnostic scope counts: {body:#}"
    );
    assert!(
        body.get("source_type_distribution").is_none(),
        "default cheap status must omit diagnostic source counts: {body:#}"
    );
    assert!(
        body.get("status_warnings").is_none(),
        "default cheap status must not run the DB snapshot collector: {body:#}"
    );
    let telemetry = body["search_telemetry"]
        .as_object()
        .expect("search telemetry");
    assert!(
        telemetry
            .get("active_count")
            .and_then(Value::as_u64)
            .is_some_and(|count| count >= 4),
        "search telemetry={telemetry:#?}"
    );
    let active_searches = telemetry["active_searches"]
        .as_array()
        .expect("active searches");
    assert!(
        active_searches.iter().all(|search| {
            search
                .get("deadline_ms")
                .and_then(Value::as_u64)
                .is_some_and(|deadline_ms| deadline_ms >= 30_000)
        }),
        "active searches should retain the configured search deadline: {active_searches:#?}"
    );

    let (diagnostic_status, _headers, diagnostic_body) = tokio::time::timeout(
        Duration::from_millis(1_800),
        get_json(state.clone(), "/api/status?diagnostic=true"),
    )
    .await
    .expect("diagnostic status should return before daemon status 2s timeout");

    assert_eq!(diagnostic_status, StatusCode::OK);
    let warnings = diagnostic_body["status_warnings"]
        .as_array()
        .expect("diagnostic status warnings");
    assert_eq!(warnings.len(), 1);
    let warning = warnings[0].as_str().expect("status warning");
    assert!(
        warning.contains("status database snapshot deadline exceeded after 1s"),
        "warning={warning}"
    );
    assert!(
        !warning.contains("alpha"),
        "status warning must not leak query text: {warning}"
    );

    for request in requests {
        request.abort();
        let _ = request.await;
    }
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
    assert!(body.get("status_warnings").is_none());
}

#[tokio::test]
async fn test_availability_check_without_embedder() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let state = env.state(Arc::new(BuildFailFactory));

    let (status, _headers, body) = get_json(state, "/api/status").await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("db_size_bytes").is_none(),
        "default status must omit diagnostic DB size: {body:#}"
    );
    assert!(body["embedding_status"].as_str().is_some());
}
