mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use common::harness::start as start_mock;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
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
                source_type: SourceType::Project,
                added_at: format!("17130000{index:02}"),
                chunk_index: Some(index as i64),
                importance: 0,
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
    command.arg("reindex");
    command.args(args);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().expect("run reindex")
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
            source: None,
            project_id: None,
            dry_run: Some(false),
            importance: None,
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
            "UPDATE fork_ext_meta SET value = 'old' WHERE key = 'reindex:drawer-01:normalize_version'",
            [],
        )
        .expect("mark normalize stale");
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
    assert_eq!(handle.request_count(), 8);

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
                source: None,
                project_id: None,
                dry_run: Some(false),
                importance: None,
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
    let chunk_count = mempal::ingest::chunk::chunk_text(&text, 800, 100).len();
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
            dry_run: false,
            project_id: None,
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
