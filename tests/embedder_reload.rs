mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use common::harness::{ReloadCounter, start as start_mock};
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use mempal::embed::openai_compat::OpenAiCompatibleEmbedder;
use mempal::embed::retry::retry_embed_operation;
use mempal::embed::{EmbedError, Embedder, global_embed_status};
use mempal::mcp::{IngestRequest, MempalMcpServer};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;
use tracing_subscriber::layer::SubscriberExt;

async fn test_guard() -> tokio::sync::OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await
}

fn write_config(path: &Path, content: &str) {
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, content).expect("write temp config");
    fs::rename(&tmp_path, path).expect("rename config");
}

fn wait_until(timeout: Duration, step: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if predicate() {
            return;
        }
        std::thread::sleep(step);
    }
    assert!(predicate(), "condition not met before timeout");
}

fn config_text(db_path: &Path, base_url: &str, extra: &str) -> String {
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

[embed.retry]
interval_secs = 1
search_deadline_secs = 5

[embed.alert]
enabled = true
script_path = "{}"
alert_every_n_failures = 1

[embed.degradation]
degrade_after_n_failures = 2
block_writes_when_degraded = true

[config_hot_reload]
enabled = true
debounce_ms = 50
poll_fallback_secs = 1

{}
"#,
        db_path.display(),
        base_url,
        db_path
            .parent()
            .expect("db parent")
            .join("alert.sh")
            .display(),
        extra
    )
}

struct TestEnv {
    _tmp: TempDir,
    config_path: PathBuf,
    db_path: PathBuf,
}

impl TestEnv {
    fn new(config_text: &str) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let config_path = mempal_home.join("config.toml");
        let db_path = mempal_home.join("palace.db");
        write_config(&config_path, config_text);
        Database::open(&db_path).expect("open db");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        Self {
            _tmp: tmp,
            config_path,
            db_path,
        }
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_missing_alert_script_warns_only() {
    let _guard = test_guard().await;
    let (addr, _handle) = start_mock(0).await.expect("start mock");
    let env = TestEnv::new(&config_text(
        Path::new("/tmp/mempal-missing-alert.db"),
        &format!("http://{addr}/v1"),
        "",
    ));
    let missing_script = env
        .db_path
        .parent()
        .expect("db parent")
        .join("missing-alert.sh");
    let config = Config::load_from(&env.config_path).expect("load config");
    let mut updated = config.clone();
    updated.embed.alert.script_path = Some(missing_script.display().to_string());
    write_config(
        &env.config_path,
        &toml::to_string(&updated).expect("serialize config"),
    );
    ConfigHandle::bootstrap(&env.config_path).expect("rebootstrap config");

    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    let captured = Arc::clone(&buffer);
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer().with_writer(move || CapturedWriter(Arc::clone(&captured))),
    );
    let _guard = tracing::subscriber::set_default(subscriber);
    let status = global_embed_status();
    status.reset_for_tests();
    status.record_failure(&"alert me");
    wait_until(Duration::from_secs(2), Duration::from_millis(25), || {
        String::from_utf8_lossy(&buffer.lock().expect("buffer"))
            .contains("failed to spawn embed alert script")
    });

    let embedder = OpenAiCompatibleEmbedder::from_config(&updated).expect("build embedder");
    let vectors = embedder
        .embed(&["still works"])
        .await
        .expect("embed success");
    assert_eq!(vectors[0].len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_alert_script_args() {
    let _guard = test_guard().await;
    let (addr, _handle) = start_mock(0).await.expect("start mock");
    let env = TestEnv::new(&config_text(
        Path::new("/tmp/mempal-alert-args.db"),
        &format!("http://{addr}/v1"),
        "",
    ));
    let script_path = env.db_path.parent().expect("db parent").join("alert.sh");
    let args_log = env
        .db_path
        .parent()
        .expect("db parent")
        .join("alert-args.log");
    fs::write(
        &script_path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n",
            args_log.display()
        ),
    )
    .expect("write script");
    let mut perms = fs::metadata(&script_path).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).expect("chmod script");

    let mut config = Config::load_from(&env.config_path).expect("load config");
    config.embed.alert.script_path = Some(script_path.display().to_string());
    write_config(
        &env.config_path,
        &toml::to_string(&config).expect("serialize config"),
    );
    ConfigHandle::bootstrap(&env.config_path).expect("rebootstrap config");

    let status = global_embed_status();
    status.reset_for_tests();
    status.record_failure(&"backend down");

    wait_until(Duration::from_secs(2), Duration::from_millis(25), || {
        args_log.exists()
    });
    let args = fs::read_to_string(&args_log).expect("read args log");
    assert!(args.contains("--failure-count"));
    assert!(args.contains("1"));
    assert!(args.contains("--error-text"));
    assert!(args.contains("backend down"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_retry_interval_hot_reload() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&config_text(
        Path::new("/tmp/mempal-retry-hot-reload.db"),
        "http://127.0.0.1:18002/v1",
        "",
    ));
    let status = global_embed_status();
    status.reset_for_tests();
    let times = Arc::new(Mutex::new(Vec::<i64>::new()));
    let attempts = Arc::new(Mutex::new(0usize));
    let start = std::time::Instant::now();

    let task: tokio::task::JoinHandle<mempal::embed::Result<Vec<Vec<f32>>>> = tokio::spawn({
        let times = Arc::clone(&times);
        let attempts = Arc::clone(&attempts);
        async move {
            retry_embed_operation(status, None, || {
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
        }
    });

    wait_until(Duration::from_secs(3), Duration::from_millis(25), || {
        times.lock().expect("times mutex").len() >= 2
    });

    let changed = fs::read_to_string(&env.config_path)
        .expect("read config")
        .replace("interval_secs = 1", "interval_secs = 3");
    write_config(&env.config_path, &changed);
    ConfigHandle::bootstrap(&env.config_path).expect("manual hot reload");

    let _ = task.await.expect("join retry task").expect("retry success");
    let millis = times.lock().expect("times mutex").clone();
    assert_eq!(millis.len(), 4);
    assert!(
        (millis[1] - 1_000).abs() <= 250,
        "second attempt: {:?}",
        millis
    );
    assert!(
        (millis[2] - 4_000).abs() <= 350,
        "third attempt: {:?}",
        millis
    );
    assert!(
        (millis[3] - 7_000).abs() <= 450,
        "fourth attempt: {:?}",
        millis
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_openai_compat_section_requires_restart() {
    let _guard = test_guard().await;
    let (addr, _handle) = start_mock(0).await.expect("start mock");
    let env = TestEnv::new(&config_text(
        Path::new("/tmp/mempal-restart-warning.db"),
        &format!("http://{addr}/v1"),
        "",
    ));
    let config = fs::read_to_string(&env.config_path).expect("read config");
    let changed = config.replace(&format!("http://{addr}/v1"), "http://127.0.0.1:18002/v1");
    let old_version = ConfigHandle::version();
    write_config(&env.config_path, &changed);
    wait_until(Duration::from_secs(2), Duration::from_millis(25), || {
        ConfigHandle::recent_events()
            .iter()
            .any(|event| event.contains("embedder.openai_compat.base_url requires restart"))
    });
    assert_eq!(ConfigHandle::version(), old_version);

    let config = Config::load_from(&env.config_path).expect("load changed config");
    let server = MempalMcpServer::new(env.db_path.clone(), config);
    let status = server.mempal_status().await.expect("mcp status").0;
    assert!(status.system_warnings.iter().any(|warning| {
        warning
            .message
            .contains("embedder.openai_compat.base_url requires restart")
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_sighup_triggers_reload() {
    let _guard = test_guard().await;
    let (addr, _handle) = start_mock(0).await.expect("start mock");
    let env = TestEnv::new(&config_text(
        Path::new("/tmp/mempal-sighup.db"),
        &format!("http://{addr}/v1"),
        "",
    ));
    let counter = ReloadCounter::from_hot_reload_state();
    counter.reset();
    let changed = fs::read_to_string(&env.config_path)
        .expect("read config")
        .replace("search_deadline_secs = 5", "search_deadline_secs = 7");

    // SAFETY: raises SIGHUP in the current test process on Unix.
    unsafe {
        libc::raise(libc::SIGHUP);
    }
    write_config(&env.config_path, &changed);

    wait_until(Duration::from_secs(2), Duration::from_millis(25), || {
        counter.count() == 1
    });
    assert_eq!(counter.count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_dim_mismatch_fail_fast() {
    let _guard = test_guard().await;
    global_embed_status().reset_for_tests();
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    db.insert_drawer(&Drawer {
        id: "existing".to_string(),
        content: "existing drawer".to_string(),
        wing: "test".to_string(),
        room: Some("room".to_string()),
        source_file: Some("existing.txt".to_string()),
        source_type: SourceType::Project,
        added_at: "1713000000".to_string(),
        chunk_index: Some(0),
        importance: 0,
    })
    .expect("insert drawer");
    db.insert_vector("existing", &[0.1, 0.2])
        .expect("insert vector");

    let (addr, _handle) = start_mock(0).await.expect("start mock");
    let config = Config::parse(&config_text(&db_path, &format!("http://{addr}/v1"), ""))
        .expect("parse config");
    let server = MempalMcpServer::new(db_path, config);
    let error = match server
        .mempal_ingest(Parameters(IngestRequest {
            content: "new drawer".to_string(),
            wing: "test".to_string(),
            room: Some("room".to_string()),
            source: None,
            project_id: None,
            dry_run: Some(false),
            importance: None,
        }))
        .await
    {
        Ok(_) => panic!("dim mismatch should fail"),
        Err(error) => error,
    };

    let rendered = error.message.to_string();
    assert!(rendered.contains("dimension mismatch"));
    assert!(rendered.contains("mempal reindex"));
}
