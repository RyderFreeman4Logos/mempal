mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Result;
use common::harness::start as start_mock;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use mempal::embed::openai_compat::OpenAiCompatibleEmbedder;
use mempal::embed::retry::retry_embed_operation;
use mempal::embed::{EmbedError, EmbedStatus, from_config, global_embed_status};
use mempal::mcp::{IngestRequest, MempalMcpServer, SearchRequest};
use rmcp::ServerHandler;
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

async fn test_guard() -> tokio::sync::OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await
}

fn write_config(path: &std::path::Path, content: &str) {
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, content).expect("write temp config");
    fs::rename(&tmp_path, path).expect("rename config");
}

fn embed_config(db_path: &std::path::Path, base_url: &str, extra: &str) -> String {
    format!(
        r#"
db_path = "{}"

[embed]
backend = "openai_compat"

[embed.openai_compat]
base_url = "{}"
model = "Qwen/Qwen3-Embedding-8B"
dim = 4
request_timeout_secs = 30
{}

[embed.retry]
interval_secs = 2
search_deadline_secs = 5

[embed.degradation]
degrade_after_n_failures = 2
block_writes_when_degraded = true
"#,
        db_path.display(),
        base_url,
        extra
    )
}

struct TestHome {
    _tmp: TempDir,
    config_path: std::path::PathBuf,
    db_path: std::path::PathBuf,
}

impl TestHome {
    fn new(config_text: &str) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let config_path = mempal_home.join("config.toml");
        let db_path = mempal_home.join("palace.db");
        write_config(&config_path, config_text);
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        Database::open(&db_path).expect("open db");
        Self {
            _tmp: tmp,
            config_path,
            db_path,
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_fixed_two_second_retry_interval() {
    let _guard = test_guard().await;
    let times = Arc::new(Mutex::new(Vec::<i64>::new()));
    let start = std::time::Instant::now();
    let status = EmbedStatus::new();
    let attempts = Arc::new(Mutex::new(0usize));
    let vectors = retry_embed_operation(&status, None, || {
        let times = Arc::clone(&times);
        let attempts = Arc::clone(&attempts);
        async move {
            times
                .lock()
                .expect("times mutex")
                .push(start.elapsed().as_millis() as i64);
            let mut guard = attempts.lock().expect("attempts mutex");
            *guard += 1;
            if *guard < 4 {
                Err(EmbedError::Runtime(format!("synthetic failure {}", *guard)))
            } else {
                Ok(vec![vec![0.1, 0.2, 0.3]])
            }
        }
    })
    .await
    .expect("retry result");
    assert_eq!(vectors.len(), 1);

    let millis = times.lock().expect("times mutex").clone();
    assert_eq!(millis.len(), 4);
    for (actual, expected) in millis.into_iter().zip([0_i64, 2_000, 4_000, 6_000]) {
        assert!(
            (actual - expected).abs() <= 250,
            "expected retry near {expected}ms, got {actual}ms"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_search_deadline_bm25_fallback() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start embed mock");
    handle.pause();

    let config_text = embed_config(
        std::path::Path::new("/tmp/placeholder.db"),
        &format!("http://{addr}/v1"),
        "",
    );
    let env = TestHome::new(
        &config_text.replace("/tmp/placeholder.db", "/tmp/mempal-search-fallback.db"),
    );
    let config = Config::load_from(&env.config_path).expect("load config");
    let db = Database::open(&env.db_path).expect("open db");
    db.insert_drawer_with_project(
        &Drawer {
            id: "bm25-hit".to_string(),
            content: "fallback keyword memory".to_string(),
            wing: "test".to_string(),
            room: Some("fallback".to_string()),
            source_file: Some("fixtures/fallback.txt".to_string()),
            source_type: SourceType::Project,
            added_at: "1713000000".to_string(),
            chunk_index: Some(0),
            importance: 2,
            ..Drawer::default()
        },
        Some("default"),
    )
    .expect("insert drawer");

    let server = MempalMcpServer::new(env.db_path.clone(), config);
    tokio::time::pause();
    let search = tokio::spawn({
        let server = server.clone();
        async move {
            server
                .mempal_search(Parameters(SearchRequest {
                    query: "fallback keyword".to_string(),
                    top_k: Some(5),
                    ..SearchRequest::default()
                }))
                .await
        }
    });

    tokio::time::advance(Duration::from_secs(5)).await;
    let response = search
        .await
        .expect("join search task")
        .expect("search result")
        .0;
    assert_eq!(response.results[0].drawer_id, "bm25-hit");
    assert!(response.system_warnings.iter().any(|warning| {
        warning.message.contains("BM25 fallback") || warning.message.contains("vector unavailable")
    }));

    handle.resume();
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_successful_embed_exits_degraded() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start embed mock");
    let env = TestHome::new(&embed_config(
        std::path::Path::new("/tmp/mempal-degraded.db"),
        &format!("http://{addr}/v1"),
        "",
    ));
    let config = Config::load_from(&env.config_path).expect("load config");
    let server = MempalMcpServer::new(env.db_path.clone(), config.clone());
    let status = global_embed_status();
    status.reset_for_tests();
    status.record_failure(&"synthetic failure 1");
    status.record_failure(&"synthetic failure 2");
    assert!(
        server
            .mempal_status()
            .await
            .expect("status")
            .0
            .embed_status
            .degraded
    );

    let embedder = from_config(&config).await.expect("build managed embedder");
    embedder
        .embed(&["recover me"])
        .await
        .expect("successful embed");

    let snapshot = server
        .mempal_status()
        .await
        .expect("status after recover")
        .0;
    assert!(!snapshot.embed_status.degraded);
    assert_eq!(snapshot.embed_status.failure_count, 0);
    assert!(snapshot.embed_status.last_success_at_unix_ms.is_some());

    handle.shutdown().await;
}

async fn ingest_with_config(db_path: PathBuf, config: Config) -> Result<serde_json::Value> {
    let server = MempalMcpServer::new(db_path, config);
    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "blocked until recover".to_string(),
            wing: "test".to_string(),
            room: Some("recover".to_string()),
            dry_run: Some(false),
            ..IngestRequest::default()
        }))
        .await?;
    Ok(serde_json::to_value(response.0).expect("serialize response"))
}

#[tokio::test(flavor = "current_thread")]
async fn test_ingest_blocks_but_not_rejected_when_block_writes_false() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start embed mock");
    handle.pause();
    let config_text = embed_config(
        std::path::Path::new("/tmp/mempal-block-false.db"),
        &format!("http://{addr}/v1"),
        "",
    )
    .replace(
        "block_writes_when_degraded = true",
        "block_writes_when_degraded = false",
    );
    let env = TestHome::new(&config_text);
    let config = Config::load_from(&env.config_path).expect("load config");
    let status = global_embed_status();
    status.reset_for_tests();
    status.record_failure(&"synthetic failure 1");
    status.record_failure(&"synthetic failure 2");

    let task = tokio::spawn(ingest_with_config(env.db_path.clone(), config));
    tokio::task::yield_now().await;
    assert!(
        !task.is_finished(),
        "ingest should block while embedder is paused"
    );

    handle.resume();
    let payload = task
        .await
        .expect("join ingest task")
        .expect("ingest succeeds");
    assert!(payload.get("drawer_id").is_some());

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_server_info_injects_rule_11_when_degraded() {
    let _guard = test_guard().await;
    let config = Config::parse(
        r#"
db_path = "/tmp/mempal-server-info.db"

[embed]
backend = "model2vec"

[embed.degradation]
degrade_after_n_failures = 1
block_writes_when_degraded = true
"#,
    )
    .expect("parse config");
    let server = MempalMcpServer::new(
        std::path::PathBuf::from("/tmp/mempal-server-info.db"),
        config,
    );
    let tmp = TempDir::new().expect("tempdir");
    let config_path = tmp.path().join("config.toml");
    write_config(
        &config_path,
        r#"
db_path = "/tmp/mempal-server-info.db"

[embed]
backend = "model2vec"

[embed.degradation]
degrade_after_n_failures = 1
block_writes_when_degraded = true
"#,
    );
    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
    let status = global_embed_status();
    status.reset_for_tests();
    status.record_failure(&"synthetic degraded");

    let info = server.get_info();
    let instructions = serde_json::to_string(&info).expect("serialize server info");
    assert!(instructions.contains("11."));
    assert!(instructions.contains("DEGRADED EMBED BACKEND"));
}

#[test]
fn test_missing_base_url_fails_fast_with_example() {
    let config = Config::parse(
        r#"
db_path = "/tmp/mempal-missing-base.db"

[embed]
backend = "openai_compat"

[embed.openai_compat]
model = "Qwen/Qwen3-Embedding-8B"
dim = 4
"#,
    )
    .expect("parse config");

    let error = OpenAiCompatibleEmbedder::from_config(&config).expect_err("missing base_url");
    let rendered = error.to_string();
    assert!(rendered.contains("example"));
    assert!(rendered.contains("http://127.0.0.1:18002/v1"));
}

#[tokio::test(flavor = "current_thread")]
async fn test_api_key_never_in_logs() {
    let _guard = test_guard().await;
    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    let captured = Arc::clone(&buffer);
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer().with_writer(move || CapturedWriter(Arc::clone(&captured))),
    );
    let _subscriber_guard = subscriber.set_default();

    // SAFETY: test serializes env mutation with a process-wide mutex.
    unsafe {
        std::env::set_var(
            "MEMPAL_LOG_TEST_KEY",
            "sk-test1234567890abcdef1234567890abcdef",
        );
    }
    let status = EmbedStatus::new();
    let attempts = Arc::new(Mutex::new(0usize));

    tokio::time::pause();
    let task = tokio::spawn(async move {
        retry_embed_operation(&status, None, || {
            let attempts = Arc::clone(&attempts);
            async move {
                let mut guard = attempts.lock().expect("attempts mutex");
                *guard += 1;
                Err(EmbedError::Runtime(format!(
                    "Bearer sk-test1234567890abcdef1234567890abcdef failure {}",
                    *guard
                )))
            }
        })
        .await
    });
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    task.abort();

    let output = String::from_utf8(buffer.lock().expect("buffer mutex").clone()).expect("utf8");
    assert!(!output.contains("sk-"));
    assert!(!output.contains("ya29"));
    assert!(!output.contains("Bearer "));

    // SAFETY: paired with serialized env mutation above.
    unsafe {
        std::env::remove_var("MEMPAL_LOG_TEST_KEY");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_status_exposes_embed_status() {
    let _guard = test_guard().await;
    let (addr, handle) = start_mock(0).await.expect("start embed mock");
    let env = TestHome::new(&embed_config(
        std::path::Path::new("/tmp/mempal-status.db"),
        &format!("http://{addr}/v1"),
        "",
    ));
    let config = Config::load_from(&env.config_path).expect("load config");
    let server = MempalMcpServer::new(env.db_path.clone(), config.clone());
    let embedder = from_config(&config).await.expect("build managed embedder");
    embedder
        .embed(&["status update"])
        .await
        .expect("embed success");

    let value = serde_json::to_value(server.mempal_status().await.expect("status").0)
        .expect("serialize status");
    let embed_status = &value["embed_status"];
    assert_eq!(embed_status["backend"], "openai_compat");
    assert_eq!(embed_status["base_url"], format!("http://{addr}/v1"));
    assert!(embed_status["last_success_at_unix_ms"].as_u64().is_some());
    assert_eq!(embed_status["failure_count"], 0);
    assert_eq!(embed_status["degraded"], false);

    handle.shutdown().await;
}

#[test]
fn test_base_url_rejects_userinfo_and_query_secret() {
    for base_url in [
        "http://user:pass@127.0.0.1:18002/v1",
        "http://127.0.0.1:18002/v1?api_key=secret",
    ] {
        let config = Config::parse(&embed_config(
            std::path::Path::new("/tmp/mempal-invalid-url.db"),
            base_url,
            "",
        ))
        .expect("parse config");
        let error = OpenAiCompatibleEmbedder::from_config(&config).expect_err("invalid base_url");
        let rendered = error.to_string();
        assert!(rendered.contains("base_url"));
        assert!(rendered.contains("api_key_env") || rendered.contains("userinfo"));
    }
}

#[test]
fn test_legacy_config_missing_embed_keeps_model2vec() {
    let config = Config::parse(
        r#"
db_path = "/tmp/mempal-legacy.db"

[search]
strict_project_isolation = false
"#,
    )
    .expect("parse legacy config");
    assert_eq!(config.embed.backend, "model2vec");
}

#[derive(Clone)]
struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("buffer mutex").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
