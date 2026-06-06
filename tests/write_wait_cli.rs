mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use common::harness::embed_mock::start as start_embed_mock;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::core::queue::PendingMessageStore;
use mempal::mcp::{IngestOperationState, IngestRequest, MempalMcpServer};
use rmcp::handler::server::wrapper::Parameters;
use serde_json::json;
use tempfile::TempDir;

static CONFIG_LOCK: Mutex<()> = Mutex::new(());

struct ConfigOverrideGuard {
    _lock: MutexGuard<'static, ()>,
}

impl ConfigOverrideGuard {
    fn install(config_path: &Path) -> Self {
        let lock = CONFIG_LOCK.lock().expect("config override lock");
        ConfigHandle::harness_reload_from_path(config_path);
        Self { _lock: lock }
    }
}

impl Drop for ConfigOverrideGuard {
    fn drop(&mut self) {
        let tempdir = TempDir::new().expect("reset tempdir");
        let path = tempdir.path().join("default.toml");
        fs::write(&path, "db_path = \"~/.mempal/palace.db\"\n").expect("write default config");
        ConfigHandle::harness_reload_from_path(&path);
    }
}

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn setup_home() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    fs::create_dir_all(tmp.path().join(".mempal")).expect("create mempal home");
    Database::open(&tmp.path().join(".mempal/palace.db")).expect("open db");
    tmp
}

fn write_config(home: &Path, base_url: &str) -> PathBuf {
    let db_path = home.join(".mempal/palace.db");
    let config_path = home.join(".mempal/config.toml");
    fs::write(
        &config_path,
        format!(
            r#"
db_path = "{}"

[embed]
backend = "openai_compat"
api_model = "test-embed"
base_url = "{}"
dim = 4

[embed.openai_compat]
base_url = "{}"
model = "test-embed"
dim = 4
request_timeout_secs = 2

[privacy]
enabled = false
"#,
            db_path.display(),
            base_url,
            base_url
        ),
    )
    .expect("write config");
    config_path
}

fn run_cli(home: &Path, args: &[&str]) -> Output {
    Command::new(mempal_bin())
        .args(args)
        .env("HOME", home)
        .output()
        .expect("run mempal")
}

fn run_cli_with_stdin(home: &Path, args: &[&str], payload: &[u8]) -> Output {
    use std::io::Write as _;

    let mut child = Command::new(mempal_bin())
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mempal");
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(payload).expect("write stdin payload");
    }
    child.wait_with_output().expect("wait mempal")
}

fn spawn_cli(home: &Path, args: &[&str]) -> Child {
    Command::new(mempal_bin())
        .args(args)
        .env("HOME", home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mempal")
}

fn prepared_ingest_payload(content: &str, wing: &str, room: Option<&str>) -> String {
    serde_json::to_string(&json!({
        "request": {
            "content": content,
            "wing": wing,
            "room": room,
            "source": "cli-test",
            "source_type": "user_explicit",
            "confidence": 0.9,
            "project_id": null,
            "supersedes": null,
            "replace_text": null,
            "valid_from": null,
            "valid_until": null,
            "dry_run": false,
            "wait": false,
            "wait_timeout_secs": null,
            "diary_rollup": false,
            "importance": 0,
            "memory_kind": "evidence",
            "domain": "project",
            "field": "general",
            "is_pinned": false,
            "provenance": null,
            "statement": null,
            "tier": null,
            "status": null,
            "supporting_refs": null,
            "counterexample_refs": null,
            "teaching_refs": null,
            "verification_refs": null,
            "scope_constraints": null,
            "trigger_hints": null,
            "anchor_kind": null,
            "anchor_id": null,
            "parent_anchor_id": null,
            "cwd": null
        },
        "project_id": null,
        "scrubbed_content": content,
        "source_type": "user_explicit",
        "confidence": 0.9,
        "metadata": {
            "memory_kind": "evidence",
            "domain": "project",
            "field": "general",
            "is_pinned": false,
            "anchor_kind": "global",
            "anchor_id": "general",
            "parent_anchor_id": null,
            "provenance": null,
            "statement": null,
            "tier": null,
            "status": null,
            "supporting_refs": [],
            "counterexample_refs": [],
            "teaching_refs": [],
            "verification_refs": [],
            "scope_constraints": null,
            "trigger_hints": null
        },
        "superseded_drawer_id": null,
        "raw_turn": false,
        "drawer_importance": 0
    }))
    .expect("serialize prepared ingest payload")
}

fn enqueue_prepared_operation(
    db_path: &Path,
    content: &str,
    wing: &str,
    room: Option<&str>,
) -> String {
    PendingMessageStore::new(db_path)
        .expect("queue store")
        .enqueue(
            "ingest_async",
            &prepared_ingest_payload(content, wing, room),
        )
        .expect("enqueue prepared ingest")
}

fn print_lines(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn assert_completed_status(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("state=completed"), "{stdout}");
    assert!(stdout.contains("timed_out=false"), "{stdout}");
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("drawer_id=") && line.len() > "drawer_id=".len()),
        "{stdout}"
    );
    assert!(stdout.contains("timings={"), "{stdout}");
}

fn assert_queued_status(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("state=queued"), "{stdout}");
    assert!(stdout.contains("timed_out=false"), "{stdout}");
    assert!(stdout.contains("timings={"), "{stdout}");
}

fn start_worker(server: MempalMcpServer) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let request = IngestRequest {
            content: "worker warmup".to_string(),
            wing: "mcp".to_string(),
            room: Some("warmup".to_string()),
            dry_run: Some(true),
            ..IngestRequest::default()
        };
        let _ = server.mempal_ingest(Parameters(request)).await;
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_wait_returns_drawer_id() {
    let home = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    let _config_path = write_config(home.path(), &format!("http://{addr}/v1"));

    let output = run_cli_with_stdin(
        home.path(),
        &[
            "ingest",
            "--stdin",
            "--wing",
            "cli-wing",
            "--source-type",
            "user_explicit",
            "--no-gate",
            "--wait",
            "--wait-timeout-secs",
            "30",
        ],
        br#"{"content":"cli wait content"}"#,
    );
    handle.shutdown().await;

    assert_completed_status(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("drawer_id="), "{stdout}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_operation_status_reports_queued_then_completed() {
    let home = setup_home();
    let (addr, _handle) = start_embed_mock(0).await.expect("start embed mock");
    let config_path = write_config(home.path(), &format!("http://{addr}/v1"));
    let _guard = ConfigOverrideGuard::install(&config_path);
    let config = Config::load_from(&config_path).expect("load config");
    let db_path = home.path().join(".mempal/palace.db");
    let server = MempalMcpServer::new(db_path.clone(), config);

    let operation_id =
        enqueue_prepared_operation(&db_path, "status cli content", "mcp", Some("status"));

    let queued_output = run_cli(home.path(), &["operation", "status", &operation_id]);
    assert_queued_status(&queued_output);

    let warmup = start_worker(server.clone());
    let completed = server
        .wait_for_operation_completion(&operation_id)
        .await
        .expect("wait for completion");
    warmup.await.expect("warmup join");

    assert_eq!(completed.state, Some(IngestOperationState::Completed));
    assert!(
        completed.timings.contains_key("embedding_ms"),
        "completed timings must include embedding_ms"
    );
    assert!(
        completed.timings.contains_key("db_write_ms"),
        "completed timings must include db_write_ms"
    );

    let completed_output = run_cli(home.path(), &["operation", "status", &operation_id]);
    assert_completed_status(&completed_output);
    let stdout = String::from_utf8_lossy(&completed_output.stdout);
    assert!(stdout.contains("embedding_ms"), "{stdout}");
    assert!(stdout.contains("db_write_ms"), "{stdout}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_operation_wait_exits_zero_and_prints_progress() {
    let home = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    let config_path = write_config(home.path(), &format!("http://{addr}/v1"));
    let _guard = ConfigOverrideGuard::install(&config_path);
    let config = Config::load_from(&config_path).expect("load config");
    let db_path = home.path().join(".mempal/palace.db");
    let server = MempalMcpServer::new(db_path.clone(), config);

    let operation_id =
        enqueue_prepared_operation(&db_path, "wait cli content", "mcp", Some("wait"));

    handle.pause();
    let child = spawn_cli(
        home.path(),
        &["operation", "wait", &operation_id, "--timeout-secs", "30"],
    );
    tokio::time::sleep(Duration::from_millis(150)).await;

    let warmup = start_worker(server.clone());
    tokio::time::sleep(Duration::from_millis(150)).await;
    handle.resume();

    let output = child.wait_with_output().expect("wait operation child");
    warmup.await.expect("warmup join");
    handle.shutdown().await;

    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let (stdout, stderr) = print_lines(&output);
    assert!(stdout.contains("state=completed"), "{stdout}");
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("drawer_id=") && line.len() > "drawer_id=".len()),
        "{stdout}"
    );
    assert!(stderr.contains("waiting for operation_id="), "{stderr}");
    assert!(
        stderr.contains("state=queued") || stderr.contains("state=running"),
        "{stderr}"
    );
}
