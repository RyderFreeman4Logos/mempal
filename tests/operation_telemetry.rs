use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(all(feature = "rest", feature = "db-test-seam"))]
use std::sync::Arc;
#[cfg(all(feature = "rest", feature = "db-test-seam"))]
use std::time::Duration;

#[cfg(all(feature = "rest", feature = "db-test-seam"))]
use async_trait::async_trait;
#[cfg(all(feature = "rest", feature = "db-test-seam"))]
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
#[cfg(all(feature = "rest", feature = "db-test-seam"))]
use mempal::api::ApiState;
use mempal::core::db::{Database, set_fork_ext_version};
#[cfg(all(feature = "rest", feature = "db-test-seam"))]
use mempal::core::{
    AsyncDb,
    config::ConfigHandle,
    types::{BootstrapEvidenceArgs, Drawer, SourceType},
    utils::iso_timestamp,
};
#[cfg(all(feature = "rest", feature = "db-test-seam"))]
use mempal::embed::{EmbedError, Embedder, EmbedderFactory, global_embed_status};
use mempal::observability::{
    OperationTelemetryFormat, OperationTelemetryIo, OperationTelemetryRecord,
    OperationTelemetrySource, OperationTelemetrySummaryOptions, operation_telemetry_summary,
    record_operation_telemetry, render_operation_telemetry_summary,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tempfile::TempDir;
#[cfg(all(feature = "rest", feature = "db-test-seam"))]
use tower::ServiceExt;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn setup_home() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");
    fs::write(
        mempal_home.join("config.toml"),
        format!(
            r#"
db_path = "{}"
[embed]
backend = "stub"
"#,
            db_path.display()
        ),
    )
    .expect("write config");
    (tmp, db_path)
}

fn setup_home_without_operation_telemetry_table() -> (TempDir, PathBuf) {
    let (home, db_path) = setup_home();
    let conn = Connection::open(&db_path).expect("open sqlite connection");
    conn.execute_batch(
        r#"
        DROP INDEX IF EXISTS idx_operation_telemetry_started;
        DROP INDEX IF EXISTS idx_operation_telemetry_source_operation;
        DROP INDEX IF EXISTS idx_operation_telemetry_call_site;
        DROP TABLE IF EXISTS operation_telemetry;
        "#,
    )
    .expect("drop operation telemetry schema");
    set_fork_ext_version(&conn, 22).expect("simulate fork-ext v22 database");
    assert!(!table_exists_read_only(&db_path, "operation_telemetry"));
    (home, db_path)
}

fn table_exists_read_only(db_path: &Path, table: &str) -> bool {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open sqlite read-only connection");
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1)",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .expect("query sqlite_master")
        == 1
}

fn count_table_rows(db_path: &Path, table: &str) -> i64 {
    let db = Database::open(db_path).expect("open db");
    db.conn()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count table rows")
}

fn operation_metadata_values(db_path: &Path, operation: &str, call_site: &str) -> Vec<Value> {
    let conn = Connection::open(db_path).expect("open sqlite connection");
    let mut stmt = conn
        .prepare(
            r#"
            SELECT metadata_json
            FROM operation_telemetry
            WHERE operation = ?1 AND call_site = ?2
            ORDER BY started_at_unix_ms DESC
            "#,
        )
        .expect("prepare metadata query");
    stmt.query_map([operation, call_site], |row| row.get::<_, String>(0))
        .expect("query metadata rows")
        .map(|row| {
            let metadata_json = row.expect("metadata row");
            serde_json::from_str(&metadata_json).expect("metadata JSON")
        })
        .collect()
}

fn assert_cli_command_does_not_record_operation_telemetry(args: &[&str]) {
    let (home, db_path) = setup_home();
    assert_eq!(count_table_rows(&db_path, "operation_telemetry"), 0);

    let output = Command::new(mempal_bin())
        .args(args)
        .env("HOME", home.path())
        .output()
        .unwrap_or_else(|error| panic!("run mempal {args:?}: {error}"));

    assert!(
        output.status.success(),
        "mempal {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        count_table_rows(&db_path, "operation_telemetry"),
        0,
        "mempal {args:?} must not record CLI operation telemetry"
    );
}

#[cfg(all(feature = "rest", feature = "db-test-seam"))]
struct RestTelemetryEnv {
    _tmp: TempDir,
    db_path: PathBuf,
    config_path: PathBuf,
}

#[cfg(all(feature = "rest", feature = "db-test-seam"))]
impl RestTelemetryEnv {
    fn new_with_api_search_deadline_secs(api_search_deadline_secs: u64) -> Self {
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
search_db_deadline_secs = {api_search_deadline_secs}
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

#[cfg(all(feature = "rest", feature = "db-test-seam"))]
impl Drop for RestTelemetryEnv {
    fn drop(&mut self) {
        global_embed_status().reset_for_tests();
        let _ = ConfigHandle::bootstrap(&self.config_path);
    }
}

#[cfg(all(feature = "rest", feature = "db-test-seam"))]
#[derive(Clone)]
struct FailingEmbedderFactory;

#[cfg(all(feature = "rest", feature = "db-test-seam"))]
struct FailingEmbedder;

#[cfg(all(feature = "rest", feature = "db-test-seam"))]
#[async_trait]
impl EmbedderFactory for FailingEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(FailingEmbedder))
    }
}

#[cfg(all(feature = "rest", feature = "db-test-seam"))]
#[async_trait]
impl Embedder for FailingEmbedder {
    async fn embed(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Err(EmbedError::Runtime("embedder down".to_string()))
    }

    fn dimensions(&self) -> usize {
        4
    }

    fn name(&self) -> &str {
        "failing-operation-telemetry-test"
    }
}

#[cfg(all(feature = "rest", feature = "db-test-seam"))]
fn insert_search_drawer(db: &Database) {
    let drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: "drawer_operation_telemetry_bm25_timeout".to_string(),
        content: "alpha operation telemetry timeout memory".to_string(),
        wing: "test".to_string(),
        room: Some("search".to_string()),
        source_file: Some("tests://operation-telemetry".to_string()),
        source_type: SourceType::AgentInference,
        added_at: iso_timestamp(),
        chunk_index: Some(0),
        importance: 3,
    });
    db.insert_drawer(&drawer).expect("insert drawer");
}

#[cfg(all(feature = "rest", feature = "db-test-seam"))]
async fn get_json(state: ApiState, uri: &str) -> (StatusCode, Value) {
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
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body = serde_json::from_slice(&bytes).expect("parse json");
    (status, body)
}

#[test]
fn test_operation_telemetry_summary_returns_empty_without_table() {
    let (_home, db_path) = setup_home_without_operation_telemetry_table();
    let db = Database::open_read_only(&db_path).expect("open read-only db");

    let rows = operation_telemetry_summary(
        &db,
        OperationTelemetrySummaryOptions {
            since_unix_ms: None,
            limit: 20,
        },
    )
    .expect("summarize missing telemetry table");

    assert!(
        rows.is_empty(),
        "missing table should render as no telemetry"
    );
    assert!(
        !table_exists_read_only(&db_path, "operation_telemetry"),
        "read-only summary must not create operation_telemetry"
    );
}

#[test]
fn test_operation_telemetry_records_all_public_operation_sources_and_io_classes() {
    let (_home, db_path) = setup_home();
    let db = Database::open(&db_path).expect("open db");

    for (source, operation, call_site, physical_reads, logical_reads) in [
        (
            OperationTelemetrySource::Cli,
            "status",
            "cli.status",
            10,
            100,
        ),
        (
            OperationTelemetrySource::Mcp,
            "mempal_search",
            "mcp.mempal_search",
            20,
            200,
        ),
        (
            OperationTelemetrySource::Rest,
            "GET /api/status",
            "rest.api_status",
            30,
            300,
        ),
        (
            OperationTelemetrySource::Daemon,
            "ingest_worker",
            "daemon.ingest_worker.claim",
            40,
            400,
        ),
    ] {
        let record = OperationTelemetryRecord::new(source, operation, call_site)
            .with_duration_ms(25)
            .with_success(true)
            .with_result_count(3)
            .with_io(OperationTelemetryIo {
                physical_read_bytes: physical_reads,
                physical_write_bytes: 7,
                logical_read_bytes: logical_reads,
                logical_write_bytes: 11,
                cancelled_write_bytes: 0,
            });
        record_operation_telemetry(&db, record).expect("record telemetry");
    }

    assert_eq!(count_table_rows(&db_path, "operation_telemetry"), 4);

    let summaries = operation_telemetry_summary(
        &db,
        OperationTelemetrySummaryOptions {
            since_unix_ms: None,
            limit: 20,
        },
    )
    .expect("summarize telemetry");

    for expected_source in ["cli", "mcp", "rest", "daemon"] {
        assert!(
            summaries.iter().any(|row| row.source == expected_source),
            "missing {expected_source} summary: {summaries:?}"
        );
    }

    let cli = summaries
        .iter()
        .find(|row| row.source == "cli" && row.operation == "status")
        .expect("cli status summary");
    assert_eq!(cli.operation_count, 1);
    assert_eq!(cli.success_count, 1);
    assert_eq!(cli.result_count_total, 3);
    assert_eq!(cli.physical_read_bytes_total, 10);
    assert_eq!(cli.logical_read_bytes_total, 100);
    assert_eq!(cli.physical_write_bytes_total, 7);
    assert_eq!(cli.logical_write_bytes_total, 11);
}

#[test]
fn test_operation_telemetry_serializes_stage_and_search_mode_metadata() {
    let (_home, db_path) = setup_home();
    let db = Database::open(&db_path).expect("open db");
    let record = OperationTelemetryRecord::new(
        OperationTelemetrySource::Rest,
        "GET /api/search",
        "rest.search.read",
    )
    .with_stage("hybrid_db")
    .with_search_mode("hybrid")
    .with_timed_out(false)
    .with_detached_task_info(false, None);

    record_operation_telemetry(&db, record).expect("record telemetry");

    let metadata = operation_metadata_values(&db_path, "GET /api/search", "rest.search.read")
        .into_iter()
        .next()
        .expect("metadata row");
    assert_eq!(
        metadata.get("stage").and_then(Value::as_str),
        Some("hybrid_db")
    );
    assert_eq!(
        metadata.get("search_mode").and_then(Value::as_str),
        Some("hybrid")
    );
    assert_eq!(
        metadata.get("timed_out").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        metadata
            .get("detached_task_continued")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert!(metadata.get("detached_task_duration_ms").is_none());
}

#[test]
fn test_operation_telemetry_serializes_timed_out_metadata() {
    let (_home, db_path) = setup_home();
    let db = Database::open(&db_path).expect("open db");
    let record = OperationTelemetryRecord::new(
        OperationTelemetrySource::Rest,
        "GET /api/search",
        "rest.search.read",
    )
    .with_stage("bm25_db")
    .with_search_mode("bm25_only")
    .with_timed_out(true)
    .with_detached_task_info(true, Some(37));

    record_operation_telemetry(&db, record).expect("record telemetry");

    let metadata = operation_metadata_values(&db_path, "GET /api/search", "rest.search.read")
        .into_iter()
        .next()
        .expect("metadata row");
    assert_eq!(
        metadata.get("stage").and_then(Value::as_str),
        Some("bm25_db")
    );
    assert_eq!(
        metadata.get("search_mode").and_then(Value::as_str),
        Some("bm25_only")
    );
    assert_eq!(
        metadata.get("timed_out").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        metadata
            .get("detached_task_continued")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        metadata
            .get("detached_task_duration_ms")
            .and_then(Value::as_u64),
        Some(37)
    );
}

#[cfg(all(feature = "rest", feature = "db-test-seam"))]
#[tokio::test]
async fn test_timed_out_bounded_read_records_timed_out_metadata() {
    let env = RestTelemetryEnv::new_with_api_search_deadline_secs(1);
    insert_search_drawer(&env.db());
    let async_db = AsyncDb::open(&env.db_path, 4)
        .expect("open async db")
        .with_read_delay(Duration::from_millis(1_500));
    let state = env
        .state(Arc::new(FailingEmbedderFactory))
        .with_async_db_for_test(async_db);

    let (status, body) = get_json(state, "/api/search?q=alpha&scope=global&top_k=5").await;

    assert_eq!(status, StatusCode::OK, "body={body}");
    tokio::time::sleep(Duration::from_millis(700)).await;

    let metadata_values =
        operation_metadata_values(&env.db_path, "GET /api/search", "rest.search.read");
    assert!(
        metadata_values.iter().any(|metadata| {
            metadata.get("timed_out").and_then(Value::as_bool) == Some(true)
                && metadata
                    .get("detached_task_continued")
                    .and_then(Value::as_bool)
                    == Some(true)
                && metadata
                    .get("detached_task_duration_ms")
                    .and_then(Value::as_u64)
                    .is_some()
        }),
        "missing timed-out bounded read metadata: {metadata_values:#?}"
    );
}

#[test]
fn test_operation_telemetry_sanitizes_unbounded_labels_and_error_text() {
    let (_home, db_path) = setup_home();
    let db = Database::open(&db_path).expect("open db");
    let secret = "SECRET_TOKEN_SHOULD_NOT_APPEAR";
    let raw_sql = format!("SELECT * FROM drawers WHERE content = '{secret}'");
    let raw_endpoint = format!("/api/ingest?authorization={secret}");
    let record =
        OperationTelemetryRecord::new(OperationTelemetrySource::Rest, raw_sql, raw_endpoint)
            .with_duration_ms(9)
            .with_success(false)
            .with_error("database is locked while handling SECRET_TOKEN_SHOULD_NOT_APPEAR");

    record_operation_telemetry(&db, record).expect("record telemetry");
    let summaries = operation_telemetry_summary(
        &db,
        OperationTelemetrySummaryOptions {
            since_unix_ms: None,
            limit: 10,
        },
    )
    .expect("summarize telemetry");
    let rendered = render_operation_telemetry_summary(&summaries, OperationTelemetryFormat::Json)
        .expect("render telemetry JSON");

    assert!(!rendered.contains(secret), "rendered secret: {rendered}");
    assert!(
        !rendered.contains("SELECT *"),
        "rendered raw SQL: {rendered}"
    );
    assert!(
        !rendered.contains("authorization="),
        "rendered raw endpoint: {rendered}"
    );
    assert!(
        rendered.contains("untrusted_"),
        "expected hashed untrusted label: {rendered}"
    );
    assert!(
        rendered.contains("locked") || rendered.contains("busy"),
        "expected classified sqlite error, not raw error text: {rendered}"
    );
}

#[test]
fn test_telemetry_cli_json_reports_empty_summary_without_table() {
    let (home, db_path) = setup_home_without_operation_telemetry_table();

    let output = Command::new(mempal_bin())
        .args(["telemetry", "--json"])
        .env("HOME", home.path())
        .output()
        .expect("run mempal telemetry");

    assert!(
        output.status.success(),
        "telemetry failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let value: Value = serde_json::from_str(&stdout).expect("telemetry JSON");
    assert_eq!(
        value.as_array().map(Vec::is_empty),
        Some(true),
        "missing table should return an empty telemetry array: {stdout}"
    );
    assert!(
        !table_exists_read_only(&db_path, "operation_telemetry"),
        "read-only telemetry command must not run the v23 migration"
    );
}

#[test]
fn test_telemetry_cli_json_reports_recent_sanitized_operation_summary() {
    let (home, db_path) = setup_home();
    let db = Database::open(&db_path).expect("open db");
    record_operation_telemetry(
        &db,
        OperationTelemetryRecord::new(
            OperationTelemetrySource::Mcp,
            "mempal_search",
            "mcp.mempal_search",
        )
        .with_duration_ms(42)
        .with_success(true)
        .with_io(OperationTelemetryIo {
            physical_read_bytes: 1_024,
            physical_write_bytes: 0,
            logical_read_bytes: 4_096,
            logical_write_bytes: 512,
            cancelled_write_bytes: 0,
        }),
    )
    .expect("record telemetry");

    let output = Command::new(mempal_bin())
        .args(["telemetry", "--json", "--since", "1d"])
        .env("HOME", home.path())
        .output()
        .expect("run mempal telemetry");

    assert!(
        output.status.success(),
        "telemetry failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let value: Value = serde_json::from_str(&stdout).expect("telemetry JSON");
    let rows = value.as_array().expect("telemetry rows");
    assert!(
        rows.iter().any(|row| {
            row.get("source").and_then(Value::as_str) == Some("mcp")
                && row.get("operation").and_then(Value::as_str) == Some("mempal_search")
                && row.get("physical_read_bytes_total").and_then(Value::as_u64) == Some(1_024)
                && row.get("logical_read_bytes_total").and_then(Value::as_u64) == Some(4_096)
        }),
        "missing MCP telemetry row: {stdout}"
    );
    assert!(
        !stdout.contains("SECRET"),
        "unexpected secret echo: {stdout}"
    );
    assert_eq!(
        count_table_rows(&db_path, "operation_telemetry"),
        1,
        "telemetry summary command must not record another telemetry row"
    );
}

#[test]
fn test_read_only_cli_status_does_not_record_operation_telemetry() {
    let (home, db_path) = setup_home();
    assert_eq!(count_table_rows(&db_path, "operation_telemetry"), 0);

    let output = Command::new(mempal_bin())
        .arg("status")
        .env("HOME", home.path())
        .output()
        .expect("run mempal status");

    assert!(
        output.status.success(),
        "status failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        count_table_rows(&db_path, "operation_telemetry"),
        0,
        "read-only status command must not mutate telemetry tables"
    );
}

#[test]
fn test_read_only_cli_commands_do_not_record_operation_telemetry() {
    for args in [
        &["brief", "telemetry regression", "--format", "json"][..],
        &["foresight", "list", "--format", "json"][..],
    ] {
        assert_cli_command_does_not_record_operation_telemetry(args);
    }
}

#[test]
fn test_dry_run_cli_commands_do_not_record_operation_telemetry() {
    for args in [
        &[
            "skills",
            "propose",
            "--from-cases",
            "--min-support",
            "1",
            "--dry-run",
            "--json",
        ][..],
        &["crystallize", "--dry-run", "--json"][..],
        &["sleep", "--dry-run"][..],
    ] {
        assert_cli_command_does_not_record_operation_telemetry(args);
    }
}
