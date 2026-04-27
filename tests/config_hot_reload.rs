use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use mempal::core::config::ConfigHandle;
use mempal::core::db::Database;
use mempal::embed::{EmbedError, Embedder, EmbedderFactory};
use mempal::mcp::{IngestRequest, MempalMcpServer};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolRequestParams, serve_client};
use tempfile::TempDir;
use tokio::process::Command as TokioCommand;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

async fn test_guard() -> OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(AsyncMutex::new(())))
        .clone()
        .lock_owned()
        .await
}

#[derive(Clone)]
struct RecordingEmbedderFactory {
    vector: Vec<f32>,
    delay: Duration,
    seen_inputs: Arc<Mutex<Vec<String>>>,
    entered_tx: Arc<Mutex<Option<mpsc::Sender<()>>>>,
}

struct RecordingEmbedder {
    vector: Vec<f32>,
    delay: Duration,
    seen_inputs: Arc<Mutex<Vec<String>>>,
    entered_tx: Arc<Mutex<Option<mpsc::Sender<()>>>>,
}

#[async_trait]
impl EmbedderFactory for RecordingEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(RecordingEmbedder {
            vector: self.vector.clone(),
            delay: self.delay,
            seen_inputs: Arc::clone(&self.seen_inputs),
            entered_tx: Arc::clone(&self.entered_tx),
        }))
    }
}

#[async_trait]
impl Embedder for RecordingEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.seen_inputs
            .lock()
            .expect("seen_inputs mutex poisoned")
            .extend(texts.iter().map(|text| (*text).to_string()));
        if let Some(tx) = self
            .entered_tx
            .lock()
            .expect("entered_tx mutex poisoned")
            .take()
        {
            let _ = tx.send(());
        }
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        Ok(texts.iter().map(|_| self.vector.clone()).collect())
    }

    fn dimensions(&self) -> usize {
        self.vector.len()
    }

    fn name(&self) -> &str {
        "recording"
    }
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
        write_config_atomic(&config_path, config_text);
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open db");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        Self {
            _tmp: tmp,
            config_path,
            db_path,
        }
    }

    fn server(&self, factory: Arc<dyn EmbedderFactory>) -> MempalMcpServer {
        MempalMcpServer::new_with_factory(self.db_path.clone(), factory)
    }

    fn db(&self) -> Database {
        Database::open(&self.db_path).expect("open db")
    }
}

fn write_config_atomic(path: &Path, content: &str) {
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, content).expect("write temp config");
    fs::rename(&tmp_path, path).expect("rename config atomically");
}

fn wait_until(timeout: Duration, step: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(step);
    }
    predicate()
}

fn wait_for_version_change(previous: &str) -> String {
    let mut current = ConfigHandle::version();
    let changed = wait_until(Duration::from_secs(3), Duration::from_millis(50), || {
        current = ConfigHandle::version();
        current != previous
    });
    assert!(changed, "config version did not change from {previous}");
    current
}

fn recent_events() -> Vec<String> {
    ConfigHandle::recent_events()
}

fn latest_event_contains(needle: &str) -> bool {
    recent_events().iter().any(|event| event.contains(needle))
}

fn base_config(db_path: &Path) -> String {
    format!(
        r#"
db_path = "{}"

[embedder]
backend = "api"
base_url = "http://gb10:18002/v1/"
api_model = "test-model"

[privacy]
enabled = true

[[privacy.scrub_patterns]]
name = "openai_key"
pattern = "sk-[A-Za-z0-9]{{36}}"

[config_hot_reload]
enabled = true
debounce_ms = 250
poll_fallback_secs = 1

[search]
strict_project_isolation = false

[ingest_gating.embedding_classifier]
prototypes = ["A", "B", "C"]
"#,
        db_path.display()
    )
}

fn config_with_custom_token(db_path: &Path) -> String {
    format!(
        r#"
db_path = "{}"

[embedder]
backend = "api"
base_url = "http://gb10:18002/v1/"
api_model = "test-model"

[privacy]
enabled = true

[[privacy.scrub_patterns]]
name = "openai_key"
pattern = "sk-[A-Za-z0-9]{{36}}"

[[privacy.scrub_patterns]]
name = "custom_token"
pattern = "CT-[a-f0-9]{{16}}"

[config_hot_reload]
enabled = true
debounce_ms = 250
poll_fallback_secs = 1

[search]
strict_project_isolation = false

[ingest_gating.embedding_classifier]
prototypes = ["A", "B", "C"]
"#,
        db_path.display()
    )
}

fn config_with_tier1_reject_short(db_path: &Path) -> String {
    base_config(db_path).replace(
        "[ingest_gating.embedding_classifier]\nprototypes = [\"A\", \"B\", \"C\"]",
        r#"[ingest_gating]
enabled = true

[[ingest_gating.rules]]
action = "reject"
content_bytes_lt = 12

[ingest_gating.embedding_classifier]
enabled = false"#,
    )
}

async fn ingest(server: &MempalMcpServer, content: &str) -> String {
    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content: content.to_string(),
            wing: "mempal".to_string(),
            room: Some("config-hot-reload".to_string()),
            ..IngestRequest::default()
        }))
        .await
        .expect("ingest should succeed")
        .0;
    response.drawer_id
}

fn count_inotify_fds() -> usize {
    let entries = fs::read_dir("/proc/self/fd").expect("read /proc/self/fd");
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| fs::read_link(entry.path()).ok())
        .filter(|target| target.to_string_lossy().contains("anon_inode:inotify"))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_privacy_pattern_hot_reload_applies_on_next_ingest() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&base_config(&PathBuf::from("/tmp/placeholder")));
    let config = base_config(&env.db_path);
    write_config_atomic(&env.config_path, &config);
    ConfigHandle::bootstrap(&env.config_path).expect("rebootstrap config");

    let factory = Arc::new(RecordingEmbedderFactory {
        vector: vec![0.1, 0.2, 0.3],
        delay: Duration::ZERO,
        seen_inputs: Arc::new(Mutex::new(Vec::new())),
        entered_tx: Arc::new(Mutex::new(None)),
    });
    let server = env.server(factory);

    let first_id = ingest(&server, "token CT-1234567890abcdef should stay").await;
    let first = env
        .db()
        .get_drawer(&first_id)
        .expect("get first drawer")
        .expect("first drawer exists");
    assert!(first.content.contains("CT-1234567890abcdef"));

    let previous = ConfigHandle::version();
    write_config_atomic(&env.config_path, &config_with_custom_token(&env.db_path));
    wait_for_version_change(&previous);

    let second_id = ingest(&server, "token CT-1234567890abcdef should redact").await;
    let second = env
        .db()
        .get_drawer(&second_id)
        .expect("get second drawer")
        .expect("second drawer exists");
    assert!(second.content.contains("[REDACTED:custom_token]"));
    assert!(!second.content.contains("CT-1234567890abcdef"));
    assert!(latest_event_contains(
        "config hot-reload: version changed from"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_gating_hot_reload_applies_without_server_restart() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&base_config(&PathBuf::from("/tmp/placeholder")));
    let config = base_config(&env.db_path);
    write_config_atomic(&env.config_path, &config);
    ConfigHandle::bootstrap(&env.config_path).expect("rebootstrap config");

    let factory = Arc::new(RecordingEmbedderFactory {
        vector: vec![0.1, 0.2, 0.3],
        delay: Duration::ZERO,
        seen_inputs: Arc::new(Mutex::new(Vec::new())),
        entered_tx: Arc::new(Mutex::new(None)),
    });
    let server = env.server(factory);

    let first_id = ingest(&server, "this stays accepted before reload").await;
    assert!(
        env.db()
            .get_drawer(&first_id)
            .expect("get first drawer")
            .is_some()
    );

    let previous = ConfigHandle::version();
    write_config_atomic(
        &env.config_path,
        &config_with_tier1_reject_short(&env.db_path),
    );
    wait_for_version_change(&previous);

    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "tiny".to_string(),
            wing: "mempal".to_string(),
            room: Some("config-hot-reload".to_string()),
            ..IngestRequest::default()
        }))
        .await
        .expect("ingest should return structured reject")
        .0;

    let decision = response.gating_decision.expect("tier-1 decision");
    assert_eq!(decision.decision, "rejected");
    assert_eq!(decision.tier, 1);
    assert_eq!(env.db().drawer_count().expect("drawer count"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_parse_failure_preserves_previous_config() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&config_with_custom_token(&PathBuf::from(
        "/tmp/placeholder",
    )));
    let config = config_with_custom_token(&env.db_path);
    write_config_atomic(&env.config_path, &config);
    ConfigHandle::bootstrap(&env.config_path).expect("rebootstrap config");

    let factory = Arc::new(RecordingEmbedderFactory {
        vector: vec![0.1, 0.2, 0.3],
        delay: Duration::ZERO,
        seen_inputs: Arc::new(Mutex::new(Vec::new())),
        entered_tx: Arc::new(Mutex::new(None)),
    });
    let server = env.server(factory);

    let stable_version = ConfigHandle::version();
    write_config_atomic(
        &env.config_path,
        "privacy.enabled = ***\n[config_hot_reload]\nenabled = true\n",
    );
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(ConfigHandle::version(), stable_version);
    assert!(latest_event_contains(
        "parse failed, keeping previous version"
    ));

    let drawer_id = ingest(&server, "bad CT-1234567890abcdef still scrubs").await;
    let drawer = env
        .db()
        .get_drawer(&drawer_id)
        .expect("get drawer")
        .expect("drawer exists");
    assert!(drawer.content.contains("[REDACTED:custom_token]"));

    let restored = base_config(&env.db_path);
    write_config_atomic(&env.config_path, &restored);
    wait_for_version_change(&stable_version);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_request_scoped_snapshot_prevents_mid_flight_mutation() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&base_config(&PathBuf::from("/tmp/placeholder")));
    let config = base_config(&env.db_path);
    write_config_atomic(&env.config_path, &config);
    ConfigHandle::bootstrap(&env.config_path).expect("rebootstrap config");

    let seen_inputs = Arc::new(Mutex::new(Vec::new()));
    let (entered_tx, entered_rx) = mpsc::channel();
    let factory = Arc::new(RecordingEmbedderFactory {
        vector: vec![0.1, 0.2, 0.3],
        delay: Duration::from_millis(600),
        seen_inputs: Arc::clone(&seen_inputs),
        entered_tx: Arc::new(Mutex::new(Some(entered_tx))),
    });
    let server = env.server(factory);

    let content = "sk-abcdef1234567890abcdef1234567890abcd should scrub";
    let server_for_task = server.clone();
    let first_task = tokio::spawn(async move { ingest(&server_for_task, content).await });
    entered_rx.recv().expect("embedder entered");

    let previous = ConfigHandle::version();
    let disabled = base_config(&env.db_path).replace("enabled = true", "enabled = false");
    write_config_atomic(&env.config_path, &disabled);
    wait_for_version_change(&previous);

    let first_id = first_task.await.expect("join first ingest");
    let first = env
        .db()
        .get_drawer(&first_id)
        .expect("get first drawer")
        .expect("first drawer exists");
    assert!(first.content.contains("[REDACTED:openai_key]"));

    let second_id = ingest(&server, content).await;
    let second = env
        .db()
        .get_drawer(&second_id)
        .expect("get second drawer")
        .expect("second drawer exists");
    assert!(
        second
            .content
            .contains("sk-abcdef1234567890abcdef1234567890abcd")
    );

    let seen = seen_inputs.lock().expect("seen mutex poisoned").clone();
    assert!(seen[0].contains("[REDACTED:openai_key]"));
    assert!(seen[1].contains("sk-abcdef1234567890abcdef1234567890abcd"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_embedder_backend_change_warns_and_ignores() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&base_config(&PathBuf::from("/tmp/placeholder")));
    let config = base_config(&env.db_path);
    write_config_atomic(&env.config_path, &config);
    ConfigHandle::bootstrap(&env.config_path).expect("rebootstrap config");

    let stable_version = ConfigHandle::version();
    let changed = config.replace("http://gb10:18002/v1/", "http://localhost:9000/v1/");
    write_config_atomic(&env.config_path, &changed);
    std::thread::sleep(Duration::from_millis(500));

    assert_eq!(ConfigHandle::version(), stable_version);
    assert_eq!(
        ConfigHandle::current().embed.base_url.as_deref(),
        Some("http://gb10:18002/v1/")
    );
    assert!(latest_event_contains(
        "embedder.base_url requires restart, change ignored"
    ));

    let allowed = changed.replace(
        "strict_project_isolation = false",
        "strict_project_isolation = true",
    );
    write_config_atomic(&env.config_path, &allowed);
    wait_for_version_change(&stable_version);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_notify_watcher_crash_falls_back_to_poll() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&base_config(&PathBuf::from("/tmp/placeholder")));
    let config = base_config(&env.db_path);
    write_config_atomic(&env.config_path, &config);
    ConfigHandle::bootstrap(&env.config_path).expect("rebootstrap config");

    ConfigHandle::simulate_notify_failure();
    assert!(
        wait_until(Duration::from_secs(1), Duration::from_millis(50), || {
            latest_event_contains("notify watcher crashed, falling back to poll")
        }),
        "fallback poll log not observed"
    );

    let previous = ConfigHandle::version();
    let disabled = config.replace("enabled = true", "enabled = false");
    write_config_atomic(&env.config_path, &disabled);
    wait_for_version_change(&previous);

    let factory = Arc::new(RecordingEmbedderFactory {
        vector: vec![0.1, 0.2, 0.3],
        delay: Duration::ZERO,
        seen_inputs: Arc::new(Mutex::new(Vec::new())),
        entered_tx: Arc::new(Mutex::new(None)),
    });
    let server = env.server(factory);
    let drawer_id = ingest(&server, "sk-abcdef1234567890abcdef1234567890abcd remains").await;
    let drawer = env
        .db()
        .get_drawer(&drawer_id)
        .expect("get drawer")
        .expect("drawer exists");
    assert!(
        drawer
            .content
            .contains("sk-abcdef1234567890abcdef1234567890abcd")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_status_prints_config_version_and_loaded_at() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");
    let config = base_config(&db_path);
    write_config_atomic(&mempal_home.join("config.toml"), &config);

    let output = Command::new(mempal_bin())
        .arg("status")
        .env("HOME", tmp.path())
        .output()
        .expect("run mempal status");
    assert!(output.status.success(), "status failed: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("status stdout utf8");
    assert!(
        stdout
            .lines()
            .any(|line| line.trim() == "fork_ext_version: 7"),
        "fork_ext_version line missing from status: {stdout}"
    );
    let line = stdout
        .lines()
        .find(|line| line.starts_with("config: version="))
        .expect("config line present");
    let version = line
        .split("version=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("version token");
    assert_eq!(version.len(), 12);

    let loaded_ms = line
        .split("loaded_unix_ms=")
        .nth(1)
        .and_then(|ms| ms.parse::<u64>().ok())
        .expect("loaded_unix_ms token");
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_millis() as u64;
    assert!(now_ms.saturating_sub(loaded_ms) < 600_000);
    assert!(stdout.contains("Scrub:\n"));
    assert!(stdout.contains("  total_patterns_matched: 0"));
    assert!(stdout.contains("  bytes_redacted: 0"));
    assert!(stdout.contains("  redactions_per_pattern: none"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_mcp_status_returns_config_version() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&base_config(&PathBuf::from("/tmp/placeholder")));
    let config = base_config(&env.db_path);
    write_config_atomic(&env.config_path, &config);
    ConfigHandle::bootstrap(&env.config_path).expect("rebootstrap config");

    let factory = Arc::new(RecordingEmbedderFactory {
        vector: vec![0.1, 0.2, 0.3],
        delay: Duration::ZERO,
        seen_inputs: Arc::new(Mutex::new(Vec::new())),
        entered_tx: Arc::new(Mutex::new(None)),
    });
    let server = env.server(factory);

    let first = server.mempal_status().await.expect("status").0;
    assert_eq!(first.config_version.len(), 12);

    let changed = config.replace(
        "strict_project_isolation = false",
        "strict_project_isolation = true",
    );
    write_config_atomic(&env.config_path, &changed);
    wait_for_version_change(&first.config_version);

    let second = server.mempal_status().await.expect("status").0;
    assert_ne!(first.config_version, second.config_version);
    assert!(second.config_loaded_at_unix_ms >= first.config_loaded_at_unix_ms);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_mcp_stdio_child_hot_reloads() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");
    let config_path = mempal_home.join("config.toml");
    let initial_config = base_config(&db_path);
    write_config_atomic(&config_path, &initial_config);

    let mut child = TokioCommand::new(mempal_bin())
        .arg("serve")
        .arg("--mcp")
        .env("HOME", tmp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mempal serve --mcp");

    let stdout = child.stdout.take().expect("child stdout");
    let stdin = child.stdin.take().expect("child stdin");
    let client = serve_client((), (stdout, stdin))
        .await
        .expect("initialize mcp stdio client");

    let first: serde_json::Value = client
        .call_tool(CallToolRequestParams::new("mempal_status"))
        .await
        .expect("first mempal_status call")
        .into_typed()
        .expect("decode first status");
    let first_version = first
        .get("config_version")
        .and_then(serde_json::Value::as_str)
        .expect("first config_version");
    assert_eq!(first_version.len(), 12);

    let changed = initial_config.replace(
        "strict_project_isolation = false",
        "strict_project_isolation = true",
    );
    write_config_atomic(&config_path, &changed);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let second: serde_json::Value = client
        .call_tool(CallToolRequestParams::new("mempal_status"))
        .await
        .expect("second mempal_status call")
        .into_typed()
        .expect("decode second status");
    let second_version = second
        .get("config_version")
        .and_then(serde_json::Value::as_str)
        .expect("second config_version");
    assert_ne!(first_version, second_version);

    client.cancel().await.expect("cancel mcp client");
    match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
        Ok(Ok(status)) => assert!(status.success(), "child exited with {status}"),
        Ok(Err(err)) => panic!("failed waiting for child: {err}"),
        Err(_) => {
            child.kill().await.expect("kill stuck child");
            panic!("mcp stdio child did not exit after client shutdown");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_rapid_edits_coalesced_by_debounce() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&base_config(&PathBuf::from("/tmp/placeholder")));
    let config = base_config(&env.db_path);
    write_config_atomic(&env.config_path, &config);
    ConfigHandle::bootstrap(&env.config_path).expect("rebootstrap config");

    let attempts_before = ConfigHandle::parse_attempts();
    let previous = ConfigHandle::version();
    for i in 0..5 {
        let toggled = if i == 4 {
            config.replace(
                "strict_project_isolation = false",
                "strict_project_isolation = true",
            )
        } else {
            config.replace(
                "api_model = \"test-model\"",
                &format!("api_model = \"test-model-{i}\""),
            )
        };
        write_config_atomic(&env.config_path, &toggled);
        std::thread::sleep(Duration::from_millis(50));
    }
    wait_for_version_change(&previous);

    let attempts_after = ConfigHandle::parse_attempts();
    assert!(
        attempts_after.saturating_sub(attempts_before) <= 2,
        "parse attempts delta too high: {}",
        attempts_after.saturating_sub(attempts_before)
    );
    assert!(ConfigHandle::current().search.strict_project_isolation);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hot_reload_disabled_no_watcher() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");

    let disabled = format!(
        r#"
db_path = "{}"

[embedder]
backend = "api"
base_url = "http://gb10:18002/v1/"
api_model = "test-model"

[privacy]
enabled = true

[config_hot_reload]
enabled = false
debounce_ms = 250
poll_fallback_secs = 1

[search]
strict_project_isolation = false
"#,
        db_path.display()
    );
    let config_path = mempal_home.join("config.toml");
    write_config_atomic(&config_path, &disabled);

    let _before = count_inotify_fds();
    ConfigHandle::bootstrap(&config_path).expect("bootstrap disabled hot reload");
    let after = count_inotify_fds();
    assert_eq!(after, 0);

    let version = ConfigHandle::version();
    let loaded_at = ConfigHandle::loaded_at_unix_ms();
    let changed = disabled.replace(
        "strict_project_isolation = false",
        "strict_project_isolation = true",
    );
    write_config_atomic(&config_path, &changed);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(ConfigHandle::version(), version);
    assert_eq!(ConfigHandle::loaded_at_unix_ms(), loaded_at);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_prototype_hot_reload_deferred_to_restart() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&base_config(&PathBuf::from("/tmp/placeholder")));
    let config = base_config(&env.db_path);
    write_config_atomic(&env.config_path, &config);
    ConfigHandle::bootstrap(&env.config_path).expect("rebootstrap config");

    assert_eq!(ConfigHandle::runtime_prototypes(), vec!["A", "B", "C"]);
    let previous = ConfigHandle::version();
    let changed = config.replace("[\"A\", \"B\", \"C\"]", "[\"A\", \"B\", \"C\", \"D\"]");
    write_config_atomic(&env.config_path, &changed);
    wait_for_version_change(&previous);

    assert_eq!(
        ConfigHandle::current()
            .ingest_gating
            .embedding_classifier
            .prototypes,
        vec!["A", "B", "C", "D"]
    );
    assert_eq!(ConfigHandle::runtime_prototypes(), vec!["A", "B", "C"]);
    assert!(latest_event_contains(
        "prototype change detected, effective after daemon restart"
    ));

    ConfigHandle::bootstrap(&env.config_path).expect("restart bootstrap");
    assert_eq!(ConfigHandle::runtime_prototypes(), vec!["A", "B", "C", "D"]);
}
