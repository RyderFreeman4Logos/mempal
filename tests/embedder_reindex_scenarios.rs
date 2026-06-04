#![cfg(feature = "integration")]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use common::harness::FailMode;
use common::harness::start as start_mock;
use mempal::core::config::{Config, ConfigHandle, DEFAULT_MODEL2VEC_FINGERPRINT_MODEL};
use mempal::core::db::{Database, VECTOR_DISTANCE_METRIC};
use mempal::core::reindex::ReindexProgressStore;
use mempal::core::types::{Drawer, SourceType};
use mempal::embed::{EmbedError, Embedder, EmbedderFactory, global_embed_status};
use mempal::ingest::{IngestError, IngestOptions, ingest_file_with_options};
use mempal::mcp::{IngestRequest, MempalMcpServer, SearchRequest};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;
use tokio::process::Command as TokioCommand;

async fn test_guard() -> tokio::sync::OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    let guard = GUARD
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await;
    global_embed_status().reset_for_tests();
    guard
}

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn write_config(path: &Path, content: &str) {
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, content).expect("write temp config");
    fs::rename(&tmp_path, path).expect("rename config");
}

fn reindex_config(db_path: &Path, base_url: &str, dim: usize, block_writes: bool) -> String {
    format!(
        r#"
db_path = "{}"

[embed]
backend = "openai_compat"

[embed.openai_compat]
base_url = "{}"
model = "Qwen/Qwen3-Embedding-8B"
dim = {}
request_timeout_secs = 30

[embed.retry]
interval_secs = 1
search_deadline_secs = 5

[embed.degradation]
degrade_after_n_failures = 2
block_writes_when_degraded = {}
"#,
        db_path.display(),
        base_url,
        dim,
        block_writes
    )
}

fn model2vec_reindex_config(db_path: &Path) -> String {
    format!(
        r#"
db_path = "{}"

[embed]
backend = "model2vec"
"#,
        db_path.display()
    )
}

struct TestHome {
    _tmp: TempDir,
    home: PathBuf,
    config_path: PathBuf,
    db_path: PathBuf,
}

impl TestHome {
    fn new(config_text: &str) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let home = tmp.path().join("home");
        let mempal_home = home.join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let config_path = mempal_home.join("config.toml");
        let db_path = mempal_home.join("palace.db");
        write_config(&config_path, config_text);
        Database::open(&db_path).expect("open db");
        Self {
            _tmp: tmp,
            home,
            config_path,
            db_path,
        }
    }
}

fn seed_drawers(db_path: &Path, count: usize, vector_dim: usize) {
    let db = Database::open(db_path).expect("open db");
    for index in 0..count {
        let id = format!("drawer-{index:02}");
        db.insert_drawer_with_project(
            &Drawer {
                id: id.clone(),
                content: format!("drawer content {index}"),
                wing: "test".to_string(),
                room: Some("reindex".to_string()),
                source_file: Some("fixtures/source.txt".to_string()),
                source_type: SourceType::AgentInference,
                added_at: format!("17130000{index:02}"),
                chunk_index: Some(index as i64),
                importance: 0,
                ..Drawer::default()
            },
            Some("default"),
        )
        .expect("insert drawer");
        let vector = vec![0.1_f32; vector_dim];
        db.insert_vector_with_project(&id, &vector, Some("default"))
            .expect("insert vector");
    }
}

fn run_reindex(home: &Path, args: &[&str], extra_env: &[(&str, String)]) -> Output {
    let mut command = Command::new(mempal_bin());
    command.env("HOME", home);
    for key in [
        "MEMPAL_EMBED_BACKEND",
        "MEMPAL_EMBED_BASE_URL",
        "MEMPAL_EMBED_MODEL",
        "MEMPAL_EMBED_DIM",
    ] {
        command.env_remove(key);
    }
    command.arg("reindex");
    command.args(args);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().expect("run reindex")
}

fn replace_vectors_with_metricless_l2(db_path: &Path, count: usize, dim: usize) {
    let db = Database::open(db_path).expect("open db");
    db.conn()
        .execute_batch(&format!(
            r#"
            DROP TABLE drawer_vectors;
            CREATE VIRTUAL TABLE drawer_vectors USING vec0(
                id TEXT PRIMARY KEY,
                embedding FLOAT[{dim}]
            );
            "#
        ))
        .expect("recreate metricless vector table");
    let vector = vec![0.2_f32; dim];
    let vector_json = serde_json::to_string(&vector).expect("serialize vector");
    for index in 0..count {
        let id = format!("drawer-{index:02}");
        db.conn()
            .execute(
                "INSERT INTO drawer_vectors (id, embedding) VALUES (?1, vec_f32(?2))",
                rusqlite::params![id, vector_json.as_str()],
            )
            .expect("insert metricless vector");
    }
}

fn read_vector(db: &Database, drawer_id: &str) -> Vec<f32> {
    let json = db
        .conn()
        .query_row(
            "SELECT vec_to_json(embedding) FROM drawer_vectors WHERE id = ?1",
            [drawer_id],
            |row| row.get::<_, String>(0),
        )
        .expect("read vector json");
    serde_json::from_str(&json).expect("parse vector json")
}

async fn wait_for_request_count(handle: &common::harness::MockEmbedHandle, expected: u32) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if handle.request_count() >= expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "mock server did not reach request_count={expected}, got {}",
        handle.request_count()
    );
}

#[derive(Clone)]
struct StubEmbedderFactory {
    vector: Vec<f32>,
}

#[derive(Clone)]
struct StubEmbedder {
    vector: Vec<f32>,
}

#[async_trait]
impl EmbedderFactory for StubEmbedderFactory {
    async fn build(&self) -> std::result::Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(StubEmbedder {
            vector: self.vector.clone(),
        }))
    }
}

#[async_trait]
impl Embedder for StubEmbedder {
    async fn embed(&self, texts: &[&str]) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| self.vector.clone()).collect())
    }

    fn dimensions(&self) -> usize {
        self.vector.len()
    }

    fn name(&self) -> &str {
        "stub"
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_reindex_with_resume() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start mock");
    let env = TestHome::new(&reindex_config(
        Path::new("/tmp/mempal-reindex-resume.db"),
        &format!("http://{addr}/v1"),
        4,
        true,
    ));
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr}/v1"), 4, true),
    );
    seed_drawers(&env.db_path, 30, 2);

    let first = run_reindex(
        &env.home,
        &["--embedder", "openai_compat"],
        &[("MEMPAL_TEST_REINDEX_STOP_AFTER", "10".to_string())],
    );
    assert!(!first.status.success());
    let second = run_reindex(&env.home, &["--embedder", "openai_compat", "--resume"], &[]);
    assert!(
        second.status.success(),
        "resume stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(handle.request_count(), 30);

    let db = Database::open(&env.db_path).expect("open db");
    let state = db
        .conn()
        .query_row(
            "SELECT last_processed_chunk_id, status FROM reindex_progress WHERE source_path = 'fixtures/source.txt'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("read progress");
    assert_eq!(state, (29, "done".to_string()));

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_search_and_ingest_during_partial_reindex() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start mock");
    handle.pause();
    let env = TestHome::new(&reindex_config(
        Path::new("/tmp/mempal-partial.db"),
        &format!("http://{addr}/v1"),
        4,
        true,
    ));
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr}/v1"), 4, true),
    );
    seed_drawers(&env.db_path, 3, 2);

    let mut child = TokioCommand::new(mempal_bin());
    child
        .arg("reindex")
        .arg("--embedder")
        .arg("openai_compat")
        .env("HOME", &env.home)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = child.spawn().expect("spawn reindex child");
    wait_for_request_count(&handle, 1).await;

    let server = MempalMcpServer::new_with_factory(
        env.db_path.clone(),
        Arc::new(StubEmbedderFactory {
            vector: vec![0.2, 0.3, 0.4, 0.5],
        }),
    );
    let search = server
        .mempal_search(Parameters(SearchRequest {
            query: "drawer content".to_string(),
            wing: None,
            room: None,
            top_k: Some(5),
            project_id: None,
            include_global: None,
            all_projects: None,
            disable_progressive: None,
            ..SearchRequest::default()
        }))
        .await
        .expect("search during reindex")
        .0;
    assert!(!search.results.is_empty());

    let ingest = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "ingest during partial reindex".to_string(),
            wing: "test".to_string(),
            room: Some("reindex".to_string()),
            dry_run: Some(false),
            ..IngestRequest::default()
        }))
        .await
        .expect("ingest during reindex")
        .0;
    assert!(!ingest.drawer_id.is_empty());

    handle.resume();
    let status = tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("child wait timeout")
        .expect("wait reindex child");
    assert!(status.success());
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_reindex_from_config_embedder_switch() {
    let _guard = test_guard().await;
    let (addr1, handle1) = start_mock(0).await.expect("start first mock");
    let (addr2, handle2) = start_mock(0).await.expect("start second mock");
    let env = TestHome::new(&reindex_config(
        Path::new("/tmp/mempal-from-config.db"),
        &format!("http://{addr1}/v1"),
        4,
        true,
    ));
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr1}/v1"), 4, true),
    );
    seed_drawers(&env.db_path, 5, 2);

    let first = run_reindex(&env.home, &["--from-config"], &[]);
    assert!(
        first.status.success(),
        "first stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(handle1.request_count(), 5);

    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr2}/v1"), 4, true),
    );
    let second = run_reindex(&env.home, &["--from-config"], &[]);
    assert!(
        second.status.success(),
        "second stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(handle2.request_count(), 5);

    handle1.shutdown().await;
    handle2.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_reindex_stale_only() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start mock");
    let env = TestHome::new(&reindex_config(
        Path::new("/tmp/mempal-stale.db"),
        &format!("http://{addr}/v1"),
        4,
        true,
    ));
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr}/v1"), 4, true),
    );
    seed_drawers(&env.db_path, 6, 2);

    let first = run_reindex(&env.home, &["--from-config"], &[]);
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(handle.request_count(), 6);

    let db = Database::open(&env.db_path).expect("open db");
    db.conn()
        .execute(
            "UPDATE fork_ext_meta SET value = 'old' WHERE key = 'reindex:drawer-01:index_version'",
            [],
        )
        .expect("mark index stale");
    db.conn()
        .execute(
            "UPDATE fork_ext_meta SET value = 'other' WHERE key = 'reindex:drawer-04:embedder_fingerprint'",
            [],
        )
        .expect("mark fingerprint stale");

    let second = run_reindex(&env.home, &["--from-config", "--stale"], &[]);
    assert!(
        second.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(handle.request_count(), 7);
    assert!(
        String::from_utf8_lossy(&second.stdout)
            .contains("batch 1: re-embedded 2 stale/new drawers"),
        "stdout: {}",
        String::from_utf8_lossy(&second.stdout)
    );

    handle.shutdown().await;
}

/// #301: a transient batch embed failure must be absorbed by a bounded retry
/// instead of aborting the entire stale reindex. The mock returns an
/// undecodable body for the first two embed attempts of the stale batch, then
/// recovers. Because that failure class (`DecodeResponse`) is non-retryable,
/// this also proves the reindex retry is error-class-agnostic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_absorbs_transient_batch_failure() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start mock");
    let env = TestHome::new(&reindex_config(
        Path::new("/tmp/mempal-retry-transient.db"),
        &format!("http://{addr}/v1"),
        4,
        false,
    ));
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr}/v1"), 4, false),
    );
    seed_drawers(&env.db_path, 4, 2);

    // Full reindex builds dim-4 vectors plus per-drawer fingerprint metadata.
    let full = run_reindex(&env.home, &["--from-config"], &[]);
    assert!(
        full.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&full.stderr)
    );
    assert_eq!(handle.request_count(), 4);

    // Mark two drawers stale so the stale path embeds them in a single batch.
    let db = Database::open(&env.db_path).expect("open db");
    for id in ["drawer-01", "drawer-02"] {
        db.conn()
            .execute(
                "UPDATE fork_ext_meta SET value = 'old' WHERE key = ?1",
                [format!("reindex:{id}:index_version")],
            )
            .expect("mark index stale");
    }

    // The first two embed attempts of the stale batch fail with an undecodable
    // body; the server then recovers. The bounded retry must absorb the blip.
    handle.set_fail_mode(FailMode::MalformedBody).await;
    handle.set_fail_first_n(2);

    let stale = run_reindex(&env.home, &["--from-config", "--stale"], &[]);
    assert!(
        stale.status.success(),
        "transient blip must not abort reindex; stdout={} stderr={}",
        String::from_utf8_lossy(&stale.stdout),
        String::from_utf8_lossy(&stale.stderr),
    );
    let stdout = String::from_utf8_lossy(&stale.stdout);
    assert!(
        stdout.contains("skipped failed batches: 0"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("skipped failed drawers: 0"),
        "stdout: {stdout}"
    );

    // 4 full + 2 failed retries + 1 success = 7 embed requests.
    assert_eq!(
        handle.request_count(),
        7,
        "expected two retries followed by success"
    );

    // Both previously-stale drawers are now freshly embedded.
    ConfigHandle::bootstrap(&env.config_path).expect("bootstrap config");
    for id in ["drawer-01", "drawer-02"] {
        let details = db.drawer_vector_details(id).expect("vector details");
        assert!(!details.stale, "{id} should be re-embedded: {details:?}");
    }

    handle.shutdown().await;
}

/// #301: a batch that fails persistently (past the retry cap) must be logged,
/// skipped, and the run must continue and finish every other batch. The
/// skipped drawer stays stale so a later `mempal reindex --stale` retries it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_batch_failure_skips_and_continues() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start mock");
    let env = TestHome::new(&reindex_config(
        Path::new("/tmp/mempal-persistent-skip.db"),
        &format!("http://{addr}/v1"),
        4,
        false,
    ));
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr}/v1"), 4, false),
    );
    seed_drawers(&env.db_path, 4, 2);

    let full = run_reindex(&env.home, &["--from-config"], &[]);
    assert!(
        full.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&full.stderr)
    );

    // Mark all four drawers stale; batch-size 1 makes each its own batch.
    let db = Database::open(&env.db_path).expect("open db");
    for index in 0..4 {
        db.conn()
            .execute(
                "UPDATE fork_ext_meta SET value = 'old' WHERE key = ?1",
                [format!("reindex:drawer-{index:02}:index_version")],
            )
            .expect("mark index stale");
    }

    // drawer-02's batch always fails; the other three succeed.
    handle.set_fail_mode(FailMode::MalformedBody).await;
    handle
        .set_fail_if_input_contains(Some("drawer content 2".to_string()))
        .await;

    let stale = run_reindex(
        &env.home,
        &[
            "--from-config",
            "--stale",
            "--batch-size",
            "1",
            "--max-batch-retries",
            "1",
        ],
        &[],
    );
    assert!(
        stale.status.success(),
        "persistent failure must skip-and-continue with exit 0; stdout={} stderr={}",
        String::from_utf8_lossy(&stale.stdout),
        String::from_utf8_lossy(&stale.stderr),
    );
    let stdout = String::from_utf8_lossy(&stale.stdout);
    assert!(
        stdout.contains("skipped failed batches: 1"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("skipped failed drawers: 1"),
        "stdout: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&stale.stderr);
    assert!(
        stderr.contains("skipping") && stderr.contains("drawer"),
        "expected a skip warning on stderr: {stderr}",
    );

    // The three healthy drawers are freshly embedded; the poisoned one stays
    // stale so a later `--stale` re-run will select it again.
    ConfigHandle::bootstrap(&env.config_path).expect("bootstrap config");
    for index in [0, 1, 3] {
        let id = format!("drawer-{index:02}");
        let details = db.drawer_vector_details(&id).expect("vector details");
        assert!(!details.stale, "{id} should be embedded: {details:?}");
    }
    let poisoned = db
        .drawer_vector_details("drawer-02")
        .expect("vector details");
    assert!(
        poisoned.stale,
        "drawer-02 must remain stale for a later re-run: {poisoned:?}"
    );

    handle.shutdown().await;
}

/// #301 regression: with no failures, reindex behaves exactly as before and
/// reports zero skips.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_reindex_no_skips_regression() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start mock");
    let env = TestHome::new(&reindex_config(
        Path::new("/tmp/mempal-clean-noskip.db"),
        &format!("http://{addr}/v1"),
        4,
        false,
    ));
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr}/v1"), 4, false),
    );
    seed_drawers(&env.db_path, 4, 2);

    let full = run_reindex(&env.home, &["--from-config"], &[]);
    assert!(
        full.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&full.stderr)
    );

    let db = Database::open(&env.db_path).expect("open db");
    for id in ["drawer-01", "drawer-02"] {
        db.conn()
            .execute(
                "UPDATE fork_ext_meta SET value = 'old' WHERE key = ?1",
                [format!("reindex:{id}:index_version")],
            )
            .expect("mark index stale");
    }

    let stale = run_reindex(&env.home, &["--from-config", "--stale"], &[]);
    assert!(
        stale.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&stale.stderr)
    );
    let stdout = String::from_utf8_lossy(&stale.stdout);
    assert!(
        stdout.contains("batch 1: re-embedded 2 stale/new drawers"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("skipped failed batches: 0"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("skipped failed drawers: 0"),
        "stdout: {stdout}"
    );
    assert_eq!(handle.request_count(), 5, "4 full + 1 stale batch");

    ConfigHandle::bootstrap(&env.config_path).expect("bootstrap config");
    for id in ["drawer-01", "drawer-02"] {
        let details = db.drawer_vector_details(id).expect("vector details");
        assert!(!details.stale, "{id} should be embedded: {details:?}");
    }

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_reindex_stale_drawer_id_targets_single_stale_vector() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start mock");
    let env = TestHome::new(&reindex_config(
        Path::new("/tmp/mempal-stale-drawer-id.db"),
        &format!("http://{addr}/v1"),
        4,
        true,
    ));
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr}/v1"), 4, true),
    );
    seed_drawers(&env.db_path, 4, 4);

    let first = run_reindex(&env.home, &["--from-config"], &[]);
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(handle.request_count(), 4);

    let db = Database::open(&env.db_path).expect("open db");
    for id in ["drawer-01", "drawer-03"] {
        db.conn()
            .execute(
                "UPDATE fork_ext_meta SET value = 'old' WHERE key = ?1",
                [format!("reindex:{id}:index_version")],
            )
            .expect("mark index stale");
    }

    let second = run_reindex(
        &env.home,
        &["--from-config", "--stale", "--drawer-id", "drawer-03"],
        &[],
    );
    assert!(
        second.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        handle.request_count(),
        5,
        "targeted stale reindex should embed exactly one additional drawer"
    );

    ConfigHandle::bootstrap(&env.config_path).expect("bootstrap reindex config");
    let target = db
        .drawer_vector_details("drawer-03")
        .expect("load target vector details");
    assert!(!target.stale, "{target:?}");
    let untouched = db
        .drawer_vector_details("drawer-01")
        .expect("load untouched vector details");
    assert!(untouched.stale, "{untouched:?}");

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_reindex_stale_drawer_id_refuses_on_stale_layout_table() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start mock");
    let env = TestHome::new(&reindex_config(
        Path::new("/tmp/mempal-stale-drawer-id-l2.db"),
        &format!("http://{addr}/v1"),
        4,
        true,
    ));
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr}/v1"), 4, true),
    );
    seed_drawers(&env.db_path, 6, 4);
    replace_vectors_with_metricless_l2(&env.db_path, 6, 4);
    let db = Database::open(&env.db_path).expect("open db");
    let before_metric = db
        .vector_table_distance_metric()
        .expect("read vector metric before targeted stale reindex");
    let before_count = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count vectors before targeted stale reindex");

    let output = run_reindex(
        &env.home,
        &["--from-config", "--stale", "--drawer-id", "drawer-04"],
        &[],
    );
    assert!(
        !output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires a full rebuild"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("mempal reindex --from-config --stale"),
        "stderr: {stderr}"
    );
    assert_eq!(
        handle.request_count(),
        0,
        "refused targeted stale reindex should not call the embedder"
    );

    ConfigHandle::bootstrap(&env.config_path).expect("bootstrap reindex config");
    assert_eq!(
        db.vector_table_distance_metric()
            .expect("read vector metric after targeted stale reindex"),
        before_metric,
        "targeted stale reindex must leave the legacy table layout intact"
    );
    let after_count = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count vectors");
    assert_eq!(after_count, before_count);
    let target = db
        .drawer_vector_details("drawer-04")
        .expect("load target vector details");
    assert!(target.has_vector, "{target:?}");
    assert!(target.stale, "{target:?}");
    let untouched = db
        .drawer_vector_details("drawer-01")
        .expect("load untouched vector details");
    assert!(untouched.has_vector, "{untouched:?}");
    assert!(untouched.stale, "{untouched:?}");

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_stale_reindex_skips_concurrent_content_update() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start mock");
    handle.pause();
    let env = TestHome::new(&reindex_config(
        Path::new("/tmp/mempal-stale-concurrent-update.db"),
        &format!("http://{addr}/v1"),
        4,
        true,
    ));
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr}/v1"), 4, true),
    );
    seed_drawers(&env.db_path, 1, 4);

    let db = Database::open(&env.db_path).expect("open db");
    db.conn()
        .execute(
            "UPDATE fork_ext_meta SET value = 'old' WHERE key = 'reindex:drawer-00:index_version'",
            [],
        )
        .expect("mark index stale");

    let mut child = TokioCommand::new(mempal_bin());
    child
        .arg("reindex")
        .arg("--from-config")
        .arg("--stale")
        .arg("--batch-size")
        .arg("1")
        .env("HOME", &env.home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = child.spawn().expect("spawn stale reindex child");
    wait_for_request_count(&handle, 1).await;

    ConfigHandle::bootstrap(&env.config_path).expect("bootstrap config");
    let fresh_vector = vec![0.25_f32, 0.5, 0.75, 1.0];
    db.upsert_drawer_and_replace_vector(
        &Drawer {
            id: "drawer-00".to_string(),
            content: "drawer content changed while stale embed was in flight".to_string(),
            wing: "test".to_string(),
            room: Some("reindex".to_string()),
            source_file: Some("fixtures/source.txt".to_string()),
            source_type: SourceType::AgentInference,
            added_at: "1713000000".to_string(),
            chunk_index: Some(0),
            importance: 0,
            ..Drawer::default()
        },
        &fresh_vector,
    )
    .expect("simulate concurrent ingest update");

    handle.resume();
    let output = tokio::time::timeout(Duration::from_secs(3), child.wait_with_output())
        .await
        .expect("child wait timeout")
        .expect("wait stale reindex child");
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("skipped 1 concurrent updates"),
        "stdout: {stdout}"
    );
    assert_eq!(
        handle.request_count(),
        1,
        "stale command should not re-embed the freshly updated drawer"
    );
    assert_eq!(read_vector(&db, "drawer-00"), fresh_vector);

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_stale_reindex_skips_concurrent_real_merge_update() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start mock");
    handle.pause();
    let env = TestHome::new(&reindex_config(
        Path::new("/tmp/mempal-stale-concurrent-merge.db"),
        &format!("http://{addr}/v1"),
        4,
        true,
    ));
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr}/v1"), 4, true),
    );
    seed_drawers(&env.db_path, 1, 4);

    let db = Database::open(&env.db_path).expect("open db");
    db.conn()
        .execute(
            "UPDATE fork_ext_meta SET value = 'old' WHERE key = 'reindex:drawer-00:index_version'",
            [],
        )
        .expect("mark index stale");

    let mut child = TokioCommand::new(mempal_bin());
    child
        .arg("reindex")
        .arg("--from-config")
        .arg("--stale")
        .arg("--batch-size")
        .arg("1")
        .env("HOME", &env.home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = child.spawn().expect("spawn stale reindex child");
    wait_for_request_count(&handle, 1).await;

    ConfigHandle::bootstrap(&env.config_path).expect("bootstrap config");
    let fresh_vector = vec![0.25_f32, 0.5, 0.75, 1.0];
    db.update_drawer_after_merge(
        "drawer-00",
        "drawer content 0\n\nSUPPLEMENTARY (test): merged while stale embed was in flight",
        "1713009999",
        &fresh_vector,
    )
    .expect("simulate concurrent novelty merge update");

    handle.resume();
    let output = tokio::time::timeout(Duration::from_secs(3), child.wait_with_output())
        .await
        .expect("child wait timeout")
        .expect("wait stale reindex child");
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("skipped 1 concurrent updates"),
        "stdout: {stdout}"
    );
    assert_eq!(
        handle.request_count(),
        1,
        "stale command should not re-embed the freshly merged drawer"
    );
    assert_eq!(read_vector(&db, "drawer-00"), fresh_vector);

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_reindex_stale_resume_rebuilds_metricless_vector_table() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start mock");
    let env = TestHome::new(&reindex_config(
        Path::new("/tmp/mempal-stale-resume-l2.db"),
        &format!("http://{addr}/v1"),
        4,
        true,
    ));
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr}/v1"), 4, true),
    );
    seed_drawers(&env.db_path, 6, 4);
    replace_vectors_with_metricless_l2(&env.db_path, 6, 4);
    ReindexProgressStore::new(&env.db_path)
        .upsert_running("fixtures/source.txt", Some(5), "openai_compat")
        .expect("write resume checkpoint");

    let output = run_reindex(&env.home, &["--from-config", "--stale", "--resume"], &[]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "resume checkpoint ignored because drawer_vectors metric or dimension is stale"
        ),
        "stdout: {stdout}"
    );
    assert_eq!(
        handle.request_count(),
        1,
        "stale batch reindex should send one embedding request for the six-row batch"
    );

    let db = Database::open(&env.db_path).expect("open db");
    assert_eq!(
        db.vector_table_distance_metric()
            .expect("read vector metric")
            .as_deref(),
        Some(VECTOR_DISTANCE_METRIC)
    );
    let count = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count vectors");
    assert_eq!(count, 6);

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_reindex_dim_change_invalidates_existing() {
    let _guard = test_guard().await;
    let (addr4, handle4) = start_mock(0).await.expect("start 4d mock");
    let (addr6, handle6) = start_mock(0).await.expect("start 6d mock");
    handle6.set_dim(6);
    let env = TestHome::new(&reindex_config(
        Path::new("/tmp/mempal-dim-change.db"),
        &format!("http://{addr4}/v1"),
        4,
        true,
    ));
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr4}/v1"), 4, true),
    );
    seed_drawers(&env.db_path, 4, 2);

    let first = run_reindex(&env.home, &["--from-config"], &[]);
    assert!(first.status.success());
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr6}/v1"), 6, true),
    );
    let second = run_reindex(&env.home, &["--from-config"], &[]);
    assert!(
        second.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    let db = Database::open(&env.db_path).expect("open db");
    let dim = db
        .conn()
        .query_row(
            "SELECT vec_length(embedding) FROM drawer_vectors LIMIT 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query dim");
    let count = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count vectors");
    assert_eq!(dim, 6);
    assert_eq!(count, 4);
    assert_eq!(handle6.request_count(), 4);

    handle4.shutdown().await;
    handle6.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_default_model2vec_reindex_records_non_stale_fingerprint() {
    let _guard = test_guard().await;
    let env = TestHome::new(&model2vec_reindex_config(Path::new(
        "/tmp/mempal-model2vec-fingerprint.db",
    )));
    write_config(&env.config_path, &model2vec_reindex_config(&env.db_path));
    seed_drawers(&env.db_path, 1, 2);

    let output = run_reindex(
        &env.home,
        &["--from-config"],
        &[("MEMPAL_EMBED_BACKEND", "model2vec".to_string())],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    ConfigHandle::bootstrap(&env.config_path).expect("bootstrap model2vec config");
    let db = Database::open(&env.db_path).expect("open db");
    let details = db
        .drawer_vector_details("drawer-00")
        .expect("load vector details");
    assert!(details.has_vector);
    assert_eq!(
        details.embedder_fingerprint.as_deref(),
        details.current_embedder_fingerprint.as_deref()
    );
    assert_eq!(
        details.model.as_deref(),
        Some(DEFAULT_MODEL2VEC_FINGERPRINT_MODEL)
    );
    assert!(!details.stale, "{details:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_embed_degraded_blocks_writes_when_configured() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let config_path = tmp.path().join("config.toml");
    write_config(
        &config_path,
        r#"
db_path = "__DB_PATH__"

[embed]
backend = "model2vec"

[embed.degradation]
degrade_after_n_failures = 2
block_writes_when_degraded = true
"#
        .replace("__DB_PATH__", &db_path.display().to_string())
        .as_str(),
    );
    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
    let config = Config::load_from(&config_path).expect("load config");
    let server = MempalMcpServer::new(db_path, config);
    let status = global_embed_status();
    status.reset_for_tests();
    status.record_failure(&"synthetic 1");
    status.record_failure(&"synthetic 2");
    let error = match server
        .mempal_ingest(Parameters(IngestRequest {
            content: "blocked".to_string(),
            wing: "test".to_string(),
            room: Some("room".to_string()),
            dry_run: Some(false),
            ..IngestRequest::default()
        }))
        .await
    {
        Ok(_) => panic!("write should be blocked"),
        Err(error) => error,
    };
    assert!(error.message.contains("writes are paused"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_embed_degraded_allows_writes_when_not_configured() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start mock");
    handle.pause();
    let env = TestHome::new(&reindex_config(
        Path::new("/tmp/mempal-degraded-allows.db"),
        &format!("http://{addr}/v1"),
        4,
        false,
    ));
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr}/v1"), 4, false),
    );
    ConfigHandle::bootstrap(&env.config_path).expect("bootstrap config");
    let config = Config::load_from(&env.config_path).expect("load config");
    let status = global_embed_status();
    status.reset_for_tests();
    status.record_failure(&"synthetic 1");
    status.record_failure(&"synthetic 2");

    let task = tokio::spawn(async move {
        let server = MempalMcpServer::new(env.db_path.clone(), config);
        server
            .mempal_ingest(Parameters(IngestRequest {
                content: "allowed after recovery".to_string(),
                wing: "test".to_string(),
                room: Some("room".to_string()),
                dry_run: Some(false),
                ..IngestRequest::default()
            }))
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !task.is_finished(),
        "ingest should block while embedder is paused"
    );
    handle.resume();
    let response = task.await.expect("join task").expect("ingest response").0;
    assert!(!response.drawer_id.is_empty());
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_mixed_dim_batch_aborts_before_begin_immediate() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start mock");
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let config_path = tmp.path().join("config.toml");
    write_config(
        &config_path,
        &reindex_config(&db_path, &format!("http://{addr}/v1"), 4, true),
    );
    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
    let config = Config::load_from(&config_path).expect("load config");
    let embedder = mempal::embed::from_config(&config)
        .await
        .expect("build embedder");
    let db = Database::open(&db_path).expect("open db");
    let source = tmp.path().join("mixed.txt");
    let text = "word ".repeat(2_000);
    let chunk_count = mempal::ingest::chunk::chunk_text_token_aware(
        &text,
        &config.chunker,
        embedder.as_ref(),
        None,
    )
    .len();
    let mut dims = vec![4_u32; chunk_count];
    if chunk_count > 1 {
        dims[chunk_count - 1] = 2;
    }
    handle.set_per_item_dims(Some(dims)).await;
    fs::write(&source, text).expect("write source");

    let error = ingest_file_with_options(
        &db,
        embedder.as_ref(),
        &source,
        "test",
        IngestOptions {
            room: Some("mixed"),
            source_root: source.parent(),
            ..IngestOptions::default()
        },
    )
    .await
    .expect_err("mixed-dim batch should fail");

    match &error {
        IngestError::EmbedChunks { .. } | IngestError::VectorDimensionMismatch { .. } => {}
        other => panic!("unexpected error: {other}"),
    }
    assert_eq!(db.drawer_count().expect("drawer count"), 0);
    let vector_count = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='drawer_vectors'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query vector table");
    assert_eq!(vector_count, 0);

    handle.shutdown().await;
}

/// #302 regression: a metric/dim-change reindex whose embed phase fails on
/// EVERY batch (embedder fully down) must leave the OLD `drawer_vectors`
/// table intact (NOT emptied), and report failure rather than silently
/// installing an empty table. Fails red against pre-#302 code, which recreates
/// the table up front and then leaves it empty when no batch embeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_recreate_preserves_old_vectors() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start mock");
    let env = TestHome::new(&reindex_config(
        Path::new("/tmp/mempal-302-preserve.db"),
        &format!("http://{addr}/v1"),
        4,
        false,
    ));
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr}/v1"), 4, false),
    );
    seed_drawers(&env.db_path, 6, 4);
    // Reproduce the pre-#287 state: a populated legacy l2 table. The l2 -> cosine
    // metric change forces `mempal reindex` to drop and recreate the table.
    replace_vectors_with_metricless_l2(&env.db_path, 6, 4);

    let db = Database::open(&env.db_path).expect("open db");
    let before_metric = db
        .vector_table_distance_metric()
        .expect("read vector metric before failed reindex");
    let before_count = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count vectors before failed reindex");
    assert_eq!(before_count, 6);

    // The embedder is fully down: every batch fails persistently (no recovery).
    handle.set_fail_mode(FailMode::Http500).await;

    let output = run_reindex(
        &env.home,
        &[
            "--from-config",
            "--stale",
            "--batch-size",
            "2",
            "--max-batch-retries",
            "1",
        ],
        &[],
    );
    // Zero vectors embedded on a table-recreating reindex MUST be reported as a
    // failure (non-zero exit) instead of a silent exit-0 that leaves the index
    // empty.
    assert!(
        !output.status.success(),
        "zero-embedded recreate must fail; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // The OLD l2 vectors must still be present and unchanged.
    ConfigHandle::bootstrap(&env.config_path).expect("bootstrap reindex config");
    let after_count = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count vectors after failed reindex");
    assert_eq!(
        after_count, 6,
        "old vectors must be preserved when no batch embeds"
    );
    assert_eq!(
        db.vector_table_distance_metric()
            .expect("read vector metric after failed reindex"),
        before_metric,
        "failed reindex must leave the legacy table layout intact"
    );
    assert!(
        handle.request_count() >= 3,
        "each of the three 2-row batches must be attempted at least once before giving up; got {}",
        handle.request_count(),
    );

    handle.shutdown().await;
}

/// #302 happy-path regression: a successful metric-change reindex installs the
/// new populated cosine table, drops the old one, and `mempal status` shows the
/// index as non-empty (`vector_index_empty: false`, `vector_rows: 6`). Guards
/// against the atomicity work breaking the clean recreate-and-swap path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_recreate_swaps_atomically_regression() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start mock");
    let env = TestHome::new(&reindex_config(
        Path::new("/tmp/mempal-302-clean.db"),
        &format!("http://{addr}/v1"),
        4,
        false,
    ));
    write_config(
        &env.config_path,
        &reindex_config(&env.db_path, &format!("http://{addr}/v1"), 4, false),
    );
    seed_drawers(&env.db_path, 6, 4);
    // Legacy l2 layout forces a full recreate during reindex.
    replace_vectors_with_metricless_l2(&env.db_path, 6, 4);

    // Embedder healthy: every batch succeeds.
    let output = run_reindex(
        &env.home,
        &["--from-config", "--stale", "--batch-size", "2"],
        &[],
    );
    assert!(
        output.status.success(),
        "clean recreate must succeed; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let db = Database::open(&env.db_path).expect("open db");
    // New cosine table installed, old dropped, fully repopulated.
    assert_eq!(
        db.vector_table_distance_metric()
            .expect("read vector metric")
            .as_deref(),
        Some(VECTOR_DISTANCE_METRIC),
    );
    let count = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count vectors after clean reindex");
    assert_eq!(count, 6);

    // `mempal status` must surface a healthy (non-empty) vector index.
    let status = Command::new(mempal_bin())
        .arg("status")
        .env("HOME", &env.home)
        .output()
        .expect("run mempal status");
    assert!(status.status.success(), "{status:?}");
    let stdout = String::from_utf8(status.stdout).expect("status stdout utf8");
    assert!(
        stdout.contains("vector_index_empty: false"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("vector_rows: 6"), "stdout: {stdout}");

    handle.shutdown().await;
}
