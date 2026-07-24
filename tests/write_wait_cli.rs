mod common;

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use common::harness::embed_mock::start as start_embed_mock;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::core::db_admission::{DbAdmissionRequest, DbHolderClass, ProfileDbAdmission};
use mempal::core::queue::PendingMessageStore;
use mempal::core::types::{BootstrapEvidenceArgs, Drawer, SourceType, Triple};
use mempal::core::utils::build_triple_id;
use mempal::mcp::{IngestDrainWorkerHandle, IngestOperationState, MempalMcpServer};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
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
    let tmp = TempDir::new_in("/tmp").expect("short tempdir");
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

fn enable_daemon_in_config(config_path: &Path, home: &Path) {
    use std::io::Write as _;

    let mut config = fs::OpenOptions::new()
        .append(true)
        .open(config_path)
        .expect("open config for daemon append");
    writeln!(
        config,
        "\n[hooks]\nenabled = true\ndaemon_poll_interval_ms = 100\n\n[daemon]\nlog_path = \"{}\"",
        home.join(".mempal/daemon.log").display()
    )
    .expect("append daemon config");
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

fn wait_child_output_timeout(mut child: Child, timeout: Duration) -> Output {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if child.try_wait().expect("poll child").is_some() {
            return child.wait_with_output().expect("collect child output");
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("collect timed-out child output");
            panic!(
                "child did not exit within {timeout:?}; stdout={}, stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
fn daemon_runtime_dir(home: &Path) -> PathBuf {
    home.join(".mempal").join("runtime")
}

#[cfg(unix)]
fn wait_for_path(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {}", path.display());
}

#[cfg(unix)]
fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait().expect("poll daemon child").is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[cfg(unix)]
struct ForegroundDaemon {
    child: Child,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    daemon_log_path: PathBuf,
    started_at: Instant,
}

#[cfg(unix)]
impl ForegroundDaemon {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn diagnostics(&self) -> String {
        format!(
            "pid={}, elapsed={:?}\nstdout tail:\n{}\nstderr tail:\n{}\ndaemon.log tail:\n{}",
            self.pid(),
            self.started_at.elapsed(),
            tail_file_for_diagnostics(&self.stdout_path, 16 * 1024),
            tail_file_for_diagnostics(&self.stderr_path, 16 * 1024),
            tail_file_for_diagnostics(&self.daemon_log_path, 16 * 1024)
        )
    }
}

#[cfg(unix)]
fn tail_file_for_diagnostics(path: &Path, max_bytes: u64) -> String {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return format!("<{} unavailable: {error}>", path.display()),
    };
    let len = file.metadata().map_or(0, |metadata| metadata.len());
    let start = len.saturating_sub(max_bytes);
    if let Err(error) = file.seek(SeekFrom::Start(start)) {
        return format!("<{} seek failed: {error}>", path.display());
    }
    let mut bytes = Vec::new();
    if let Err(error) = file.read_to_end(&mut bytes) {
        return format!("<{} read failed: {error}>", path.display());
    }
    let text = String::from_utf8_lossy(&bytes);
    if start == 0 {
        text.into_owned()
    } else {
        format!("<truncated first {start} bytes>\n{text}")
    }
}

#[cfg(unix)]
fn spawn_foreground_daemon(home: &Path) -> ForegroundDaemon {
    let runtime_dir = daemon_runtime_dir(home);
    fs::create_dir_all(&runtime_dir).expect("create daemon runtime dir");
    let stdout_path = runtime_dir.join("foreground-daemon.stdout.log");
    let stderr_path = runtime_dir.join("foreground-daemon.stderr.log");
    let daemon_log_path = home.join(".mempal/daemon.log");
    let stdout = fs::File::create(&stdout_path).expect("create daemon stdout log");
    let stderr = fs::File::create(&stderr_path).expect("create daemon stderr log");

    let mut child = Command::new(mempal_bin())
        .args(["daemon", "--foreground"])
        .env("HOME", home)
        .env(
            mempal::daemon_singleton::MEMPAL_RUNTIME_DIR_ENV,
            &runtime_dir,
        )
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn foreground daemon");
    let started_at = Instant::now();
    wait_for_path(&home.join(".mempal/daemon.pid"), Duration::from_secs(10));
    wait_for_path(
        &home.join(".mempal/daemon-hook.sock"),
        Duration::from_secs(10),
    );
    if child.try_wait().expect("poll daemon").is_some() {
        let daemon = ForegroundDaemon {
            child,
            stdout_path,
            stderr_path,
            daemon_log_path,
            started_at,
        };
        panic!(
            "foreground daemon exited before test command\n{}",
            daemon.diagnostics()
        );
    }
    ForegroundDaemon {
        child,
        stdout_path,
        stderr_path,
        daemon_log_path,
        started_at,
    }
}

#[cfg(unix)]
fn terminate_daemon(daemon: &mut ForegroundDaemon) {
    if daemon
        .child
        .try_wait()
        .expect("poll daemon before SIGTERM")
        .is_some()
    {
        return;
    }

    let sigterm_sent_at = Instant::now();
    // SAFETY: this test owns the foreground daemon child process.
    let rc = unsafe { libc::kill(daemon.pid() as i32, libc::SIGTERM) };
    assert_eq!(
        rc,
        0,
        "failed to send SIGTERM to daemon\n{}",
        daemon.diagnostics()
    );
    if !wait_for_child_exit(&mut daemon.child, Duration::from_secs(20)) {
        let sigterm_elapsed = sigterm_sent_at.elapsed();
        let _ = daemon.child.kill();
        let kill_wait_start = Instant::now();
        match daemon.child.wait() {
            Ok(status) => panic!(
                "daemon did not exit after SIGTERM; killed and reaped with status {status} after kill wait {:?}; SIGTERM elapsed {:?}\n{}",
                kill_wait_start.elapsed(),
                sigterm_elapsed,
                daemon.diagnostics()
            ),
            Err(error) => panic!(
                "daemon did not exit after SIGTERM; kill/reap failed: {error}; SIGTERM elapsed {:?}\n{}",
                sigterm_elapsed,
                daemon.diagnostics()
            ),
        }
    }
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
        "controls": {
            "no_gate": false,
            "bypass_novelty": false
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

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn cleanup_ids_from_ingest_json(json: &Value) -> Vec<String> {
    let cleanup_key = if json.get("cleanup_drawer_ids").is_some() {
        "cleanup_drawer_ids"
    } else {
        "created_drawer_ids"
    };
    let ids = json[cleanup_key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned);
    ids.into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[test]
fn test_ingest_wait_json_admission_blocked_output_includes_capacity_and_headroom() {
    let home = setup_home();
    let db_path = home.path().join(".mempal/palace.db");
    let _holders = (0..16)
        .map(|_| {
            ProfileDbAdmission::acquire(&db_path, DbAdmissionRequest::new(DbHolderClass::Mcp, 1, 1))
                .expect("fill profile holder budget")
        })
        .collect::<Vec<_>>();

    let output = run_cli_with_stdin(
        home.path(),
        &[
            "ingest",
            "--stdin",
            "--wing",
            "smoke",
            "--source-type",
            "user_explicit",
            "--no-gate",
            "--wait",
            "--wait-timeout-secs",
            "5",
            "--json",
        ],
        br#"{"content":"admission-blocked JSON receipt must stay machine-readable"}"#,
    );

    assert!(
        !output.status.success(),
        "admission exhaustion must fail closed, stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "parse admission-blocked JSON receipt: {error}; stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(stdout["outcome"], "admission_blocked");
    assert_eq!(stdout["reason"], "holder_budget_exceeded");
    assert_eq!(stdout["capacity"]["holders"], 16);
    assert_eq!(stdout["headroom"]["holders"], 0);
    assert!(
        cleanup_ids_from_ingest_json(&stdout).is_empty(),
        "blocked create must not expose cleanup-safe IDs: {stdout}"
    );
}

fn cleanup_ids_from_operation_stdout(stdout: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in stdout.lines() {
        if let Some(raw_ids) = line
            .strip_prefix("cleanup_drawer_ids=")
            .or_else(|| line.strip_prefix("created_drawer_ids="))
        {
            ids.extend(
                raw_ids
                    .split(',')
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(ToOwned::to_owned),
            );
        }
    }
    ids.into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
fn delete_cleanup_ids(home: &Path, drawer_ids: &[String], project_id: Option<&str>) {
    assert!(!drawer_ids.is_empty(), "cleanup ids must not be empty");
    for drawer_id in drawer_ids {
        let mut delete = Command::new(mempal_bin());
        delete.args(["delete", drawer_id]).env("HOME", home);
        delete.envs(project_id.map(|project_id| ("MEMPAL_PROJECT_ID", project_id)));
        let delete_output = delete.output().expect("run mempal");
        assert!(
            delete_output.status.success(),
            "delete {drawer_id} must succeed, stdout={}, stderr={}",
            String::from_utf8_lossy(&delete_output.stdout),
            String::from_utf8_lossy(&delete_output.stderr)
        );
    }
}
fn hold_daemon_writer_lease(home: &Path) -> mempal::core::types::RuntimeWriterLease {
    let db = Database::open(&home.join(".mempal/palace.db")).expect("open db");
    db.runtime_writer_lease_acquire("sqlite-writer", "daemon-owner", "daemon", 300, None)
        .expect("acquire daemon writer lease")
        .expect("daemon writer lease")
}

fn hold_mcp_ingest_worker_writer_lease(home: &Path) -> mempal::core::types::RuntimeWriterLease {
    let db = Database::open(&home.join(".mempal/palace.db")).expect("open db");
    db.runtime_writer_lease_acquire(
        "sqlite-writer",
        "mcp-ingest-worker-test-owner",
        "mcp-ingest-worker",
        300,
        None,
    )
    .expect("acquire MCP ingest worker writer lease")
    .expect("MCP ingest worker writer lease")
}

fn insert_existing_drawer(home: &Path, drawer_id: &str) {
    insert_existing_drawer_with_content(home, drawer_id, "existing novelty target");
}

fn insert_existing_drawer_with_content(home: &Path, drawer_id: &str, content: &str) {
    insert_existing_drawer_with_content_and_project(home, drawer_id, content, None);
}

fn insert_existing_drawer_with_content_and_project(
    home: &Path,
    drawer_id: &str,
    content: &str,
    project_id: Option<&str>,
) {
    let db = Database::open(&home.join(".mempal/palace.db")).expect("open db");
    let drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: drawer_id.to_string(),
        content: content.to_string(),
        wing: "smoke".to_string(),
        room: Some("manual".to_string()),
        source_file: Some("test://existing-novelty-target".to_string()),
        source_type: SourceType::AgentInference,
        added_at: "1713000000".to_string(),
        chunk_index: Some(0),
        importance: 0,
    });
    db.insert_drawer_with_project_validity(&drawer, project_id, None, None, None)
        .expect("insert existing drawer");
}

#[test]
fn test_operation_stdout_cleanup_ids_ignore_informational_drawer_id_without_created_list() {
    let home = setup_home();
    let existing_id = "existing-novelty-target";
    insert_existing_drawer(home.path(), existing_id);
    let stdout = format!(
        "operation_id=op\nstate=completed\ntimed_out=false\ndrawer_id={existing_id}\ndrawer_ids={existing_id}\nchunk_count=1\ndropped=false\ntimings={{}}\n"
    );

    let cleanup_ids = cleanup_ids_from_operation_stdout(&stdout);
    assert!(
        cleanup_ids.is_empty(),
        "lone operation drawer_id must not be treated as cleanup-safe: {stdout}"
    );

    let db = Database::open(&home.path().join(".mempal/palace.db")).expect("open db");
    assert!(
        db.get_drawer(existing_id)
            .expect("get existing novelty target")
            .is_some(),
        "existing novelty target must remain present when no cleanup ids are exposed"
    );
}

fn novelty_audit_count(home: &Path) -> i64 {
    let db_path = home.join(".mempal/palace.db");
    Connection::open(&db_path)
        .expect("open sqlite")
        .query_row("SELECT COUNT(*) FROM novelty_audit", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count novelty audit")
}

fn first_ingest_async_operation(db_path: &Path) -> Option<(String, String)> {
    Connection::open(db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT id, op_state FROM pending_messages WHERE kind = 'ingest_async' ORDER BY created_at ASC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .expect("query ingest async operation")
}

fn assert_bootstrap_stderr(output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut lines = stderr.lines();
    let bootstrap = lines.next().expect("stderr must contain bootstrap line");
    assert!(
        bootstrap.starts_with("config hot-reload: bootstrapped version "),
        "unexpected stderr bootstrap line: {stderr}"
    );
    for line in lines {
        assert!(
            line.starts_with("fact_check."),
            "unexpected stderr noise: {stderr}"
        );
    }
}

fn last_audit_entry(home: &Path) -> Value {
    let audit_path = home.join(".mempal").join("audit.jsonl");
    let audit = fs::read_to_string(&audit_path).expect("read audit log");
    serde_json::from_str(audit.lines().last().expect("audit entry")).expect("parse audit entry")
}

fn assert_stdin_audit_entry(
    entry: &Value,
    expected_source: &str,
    expected_source_file: &str,
    expected_chunks: u64,
    expected_skipped: u64,
    expected_dropped_by_gate: u64,
) {
    assert_eq!(entry["mode"], "stdin");
    assert_eq!(entry["source"], expected_source);
    assert_eq!(entry["source_file"], expected_source_file);
    assert_eq!(entry["files"], 1);
    assert_eq!(entry["chunks"], expected_chunks);
    assert_eq!(entry["skipped"], expected_skipped);
    assert_eq!(entry["dropped_by_gate"], expected_dropped_by_gate);
}

fn insert_fact_check_contradiction(home: &Path) {
    let db = Database::open(&home.join(".mempal/palace.db")).expect("open db");
    let triple = Triple {
        id: build_triple_id("Bob", "husband_of", "Alice"),
        subject: "Bob".to_string(),
        predicate: "husband_of".to_string(),
        object: "Alice".to_string(),
        valid_from: Some("1700000000".to_string()),
        valid_to: None,
        confidence: 1.0,
        source_drawer: None,
    };
    db.insert_triple(&triple).expect("insert triple");
}

fn assert_completed_status(output: &Output) {
    assert_success(output);
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
    assert_success(output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("state=queued"), "{stdout}");
    assert!(stdout.contains("timed_out=false"), "{stdout}");
    assert!(stdout.contains("timings={"), "{stdout}");
}

fn start_worker(server: &MempalMcpServer) -> IngestDrainWorkerHandle {
    server.spawn_scoped_ingest_drain_worker()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_wait_matches_non_wait_plain_output() {
    let wait_home = setup_home();
    let direct_home = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    handle.set_embedding_fill(1.0).await;
    let _wait_config = write_config(wait_home.path(), &format!("http://{addr}/v1"));
    let _direct_config = write_config(direct_home.path(), &format!("http://{addr}/v1"));
    let wait_timeout = "30".to_string();
    let payload = br#"{"content":"cli wait content"}"#;

    let direct_output = run_cli_with_stdin(
        direct_home.path(),
        &[
            "ingest",
            "--stdin",
            "--wing",
            "cli-wing",
            "--source-type",
            "user_explicit",
            "--no-gate",
        ],
        payload,
    );
    let wait_output = run_cli_with_stdin(
        wait_home.path(),
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
            wait_timeout.as_str(),
        ],
        payload,
    );
    handle.shutdown().await;

    assert_success(&direct_output);
    assert_success(&wait_output);

    let direct_stdout = String::from_utf8_lossy(&direct_output.stdout);
    let wait_stdout = String::from_utf8_lossy(&wait_output.stdout);
    assert_eq!(
        wait_stdout, direct_stdout,
        "wait stdout must match direct stdout"
    );
    assert!(wait_stdout.contains("dry_run=false"), "{wait_stdout}");
    assert!(wait_stdout.contains("files=1"), "{wait_stdout}");
    assert!(wait_stdout.contains("chunks=1"), "{wait_stdout}");
    assert!(wait_stdout.contains("skipped=0"), "{wait_stdout}");
    assert!(wait_stdout.contains("dropped_by_gate=0"), "{wait_stdout}");
    assert!(
        wait_stdout.contains("superseded_drawer_id="),
        "{wait_stdout}"
    );
    assert!(
        !wait_stdout
            .lines()
            .any(|line| line.starts_with("drawer_id=")),
        "{wait_stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_wait_json_matches_non_wait_json_output() {
    let wait_home = setup_home();
    let direct_home = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    let _wait_config = write_config(wait_home.path(), &format!("http://{addr}/v1"));
    let _direct_config = write_config(direct_home.path(), &format!("http://{addr}/v1"));
    let wait_timeout = "30".to_string();
    let payload = br#"{"content":"cli wait json content"}"#;

    let direct_output = run_cli_with_stdin(
        direct_home.path(),
        &[
            "ingest",
            "--stdin",
            "--wing",
            "cli-wing",
            "--source-type",
            "user_explicit",
            "--no-gate",
            "--json",
        ],
        payload,
    );
    let wait_output = run_cli_with_stdin(
        wait_home.path(),
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
            wait_timeout.as_str(),
            "--json",
        ],
        payload,
    );
    handle.shutdown().await;

    assert_success(&direct_output);
    assert_success(&wait_output);

    let direct_json: Value =
        serde_json::from_slice(&direct_output.stdout).expect("parse direct ingest JSON");
    let wait_json: Value = serde_json::from_slice(&wait_output.stdout).expect("parse wait JSON");
    let direct_keys: BTreeSet<String> = direct_json
        .as_object()
        .expect("direct JSON object")
        .keys()
        .cloned()
        .collect();
    let wait_keys: BTreeSet<String> = wait_json
        .as_object()
        .expect("wait JSON object")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        wait_keys, direct_keys,
        "wait JSON keys must match direct JSON keys"
    );
    assert_eq!(
        wait_json["stats"], direct_json["stats"],
        "wait stats must match direct stats"
    );
    let drawer_ids = wait_json["drawer_ids"]
        .as_array()
        .expect("drawer_ids array");
    assert!(!drawer_ids.is_empty(), "drawer_ids must be non-empty");
    assert!(
        drawer_ids.iter().all(|value| value.as_str().is_some()),
        "drawer_ids must contain strings"
    );
    assert_eq!(
        wait_json["cleanup_drawer_ids"], wait_json["created_drawer_ids"],
        "cleanup_drawer_ids must mirror newly-created cleanup authority"
    );
    assert_eq!(
        wait_json["drawer_id"], drawer_ids[0],
        "top-level drawer_id must identify the same fresh drawer reported in created_drawer_ids"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_wait_json_cleanup_ids_delete_exact_drawers() {
    let home = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    let _config = write_config(home.path(), &format!("http://{addr}/v1"));
    let wait_timeout = "30".to_string();
    let payload = br#"{"content":"cli wait cleanup-safe drawer id content"}"#;

    let output = run_cli_with_stdin(
        home.path(),
        &[
            "ingest",
            "--stdin",
            "--wing",
            "smoke",
            "--room",
            "manual",
            "--source-type",
            "user_explicit",
            "--no-gate",
            "--wait",
            "--wait-timeout-secs",
            wait_timeout.as_str(),
            "--json",
        ],
        payload,
    );
    handle.shutdown().await;

    assert_success(&output);
    let stdout: Value = serde_json::from_slice(&output.stdout).expect("parse wait JSON");
    assert_eq!(stdout["stats"]["chunks"], 1);
    assert_eq!(
        stdout["cleanup_drawer_ids"], stdout["created_drawer_ids"],
        "cleanup_drawer_ids must match newly-created drawer ids"
    );
    let cleanup_ids = cleanup_ids_from_ingest_json(&stdout);
    assert_eq!(
        cleanup_ids.len(),
        1,
        "stdin wait JSON must expose exactly one cleanup-safe drawer id: {stdout}"
    );
    assert_eq!(
        stdout["drawer_id"].as_str(),
        cleanup_ids.first().map(String::as_str)
    );
    assert_eq!(
        stdout["drawer_ids"][0].as_str(),
        cleanup_ids.first().map(String::as_str)
    );

    let db_path = home.path().join(".mempal/palace.db");
    let db = Database::open(&db_path).expect("open db");
    for drawer_id in &cleanup_ids {
        assert!(
            db.get_drawer(drawer_id)
                .expect("get cleanup drawer")
                .is_some(),
            "reported cleanup id must identify an active drawer"
        );
    }
    drop(db);
    delete_cleanup_ids(home.path(), &cleanup_ids, None);
    let db = Database::open(&db_path).expect("reopen db");
    for drawer_id in &cleanup_ids {
        assert!(
            db.get_drawer(drawer_id)
                .expect("get deleted cleanup drawer")
                .is_none(),
            "reported cleanup id must be deletable by mempal delete"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_wait_json_cleanup_id_writes_under_daemon_lease() {
    let home = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    let _config = write_config(home.path(), &format!("http://{addr}/v1"));
    let wait_timeout = u64::MAX.to_string();
    let payload = br#"{"content":"cleanup authority smoke safe to delete unique body"}"#;

    let output = run_cli_with_stdin(
        home.path(),
        &[
            "ingest",
            "--stdin",
            "--wing",
            "smoke",
            "--room",
            "manual",
            "--source-type",
            "user_explicit",
            "--no-gate",
            "--wait",
            "--wait-timeout-secs",
            wait_timeout.as_str(),
            "--json",
        ],
        payload,
    );
    handle.shutdown().await;

    assert!(output.status.success(), "stdin wait ingest must succeed");
    let stdout: Value = serde_json::from_slice(&output.stdout).expect("parse wait JSON");
    let cleanup_ids = cleanup_ids_from_ingest_json(&stdout);
    assert_eq!(
        cleanup_ids.len(),
        1,
        "stdin wait JSON must expose exactly one cleanup-safe drawer id"
    );
    assert_eq!(
        stdout["cleanup_drawer_ids"], stdout["created_drawer_ids"],
        "cleanup authority must come from created_drawer_ids"
    );

    let cleanup_id = cleanup_ids.first().expect("cleanup id");
    let _daemon_lease = hold_daemon_writer_lease(home.path());

    let pin = run_cli(home.path(), &["pin", cleanup_id]);
    assert!(pin.status.success(), "pin by cleanup id must succeed");

    let unpin = run_cli(home.path(), &["unpin", cleanup_id]);
    assert!(unpin.status.success(), "unpin by cleanup id must succeed");

    let delete = run_cli(home.path(), &["delete", cleanup_id]);
    assert!(delete.status.success(), "delete by cleanup id must succeed");
    let delete_stdout = String::from_utf8_lossy(&delete.stdout);
    assert!(
        !delete_stdout.contains("cleanup authority smoke safe to delete unique body"),
        "delete stdout must not expose raw drawer content"
    );

    let db = Database::open(&home.path().join(".mempal/palace.db")).expect("open db");
    assert!(
        db.get_drawer(cleanup_id)
            .expect("get deleted cleanup drawer")
            .is_none(),
        "created cleanup id must be soft-deleted"
    );
}

#[test]
fn test_ingest_wait_duplicate_under_mcp_ingest_worker_lease_avoids_writer_conflict() {
    let home = setup_home();
    let _config = write_config(home.path(), "http://127.0.0.1:9/v1");
    insert_existing_drawer_with_content_and_project(
        home.path(),
        "existing-wait-duplicate-under-lease",
        "duplicate under MCP ingest worker lease",
        Some("lease-project"),
    );
    let _lease = hold_mcp_ingest_worker_writer_lease(home.path());
    let payload = br#"{"content":"duplicate under MCP ingest worker lease","wing":"smoke","room":"manual","project":"lease-project"}"#;

    let output = run_cli_with_stdin(
        home.path(),
        &[
            "ingest",
            "--stdin",
            "--source-type",
            "user_explicit",
            "--no-gate",
            "--wait",
            "--wait-timeout-secs",
            "0",
            "--json",
        ],
        payload,
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("SQLite writer lease `sqlite-writer` is already held"),
        "stdin wait must not direct-claim the CLI writer lease under MCP worker ownership: {stderr}"
    );
    let stdout: Value = serde_json::from_slice(&output.stdout).expect("parse stdin wait JSON");
    assert!(
        cleanup_ids_from_ingest_json(&stdout).is_empty(),
        "receipt for a pre-existing duplicate must not expose cleanup ids: {stdout}"
    );
    if output.status.success() {
        assert_eq!(stdout["stats"]["chunks"], 0);
        assert_eq!(stdout["stats"]["skipped"], 1);
    } else {
        assert_eq!(stdout["state"], "queued");
        assert_eq!(stdout["timed_out"], true);
    }
    let db = Database::open(&home.path().join(".mempal/palace.db")).expect("open db");
    assert_eq!(db.drawer_count().expect("drawer count"), 1);
}

#[cfg(unix)]
#[test]
fn test_ingest_wait_json_uses_daemon_ipc_receipt_when_daemon_is_running() {
    let home = setup_home();
    let config = write_config(home.path(), "http://127.0.0.1:9/v1");
    enable_daemon_in_config(&config, home.path());
    let mut daemon = spawn_foreground_daemon(home.path());
    let payload =
        br#"{"content":"stdin wait daemon ipc receipt content","wing":"smoke","room":"manual"}"#;

    let output = run_cli_with_stdin(
        home.path(),
        &[
            "ingest",
            "--stdin",
            "--source-type",
            "user_explicit",
            "--no-gate",
            "--wait",
            "--wait-timeout-secs",
            "0",
            "--json",
        ],
        payload,
    );

    terminate_daemon(&mut daemon);

    assert!(
        !output.status.success(),
        "zero-budget daemon receipt should exit non-zero, stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("SQLite writer lease `sqlite-writer` is already held"),
        "daemon-backed stdin wait must not direct-claim the CLI writer lease: {stderr}"
    );
    let stdout: Value = serde_json::from_slice(&output.stdout).expect("parse daemon receipt JSON");
    assert_eq!(stdout["state"], "queued");
    assert_eq!(stdout["timed_out"], true);
    assert!(
        stdout["operation_id"]
            .as_str()
            .is_some_and(|operation_id| !operation_id.is_empty()),
        "daemon receipt must include an operation id: {stdout}"
    );
    assert!(
        cleanup_ids_from_ingest_json(&stdout).is_empty(),
        "timed-out daemon receipt must not expose cleanup ids: {stdout}"
    );
}

#[test]
fn test_ingest_wait_new_content_under_mcp_ingest_worker_lease_times_out_as_receipt() {
    let home = setup_home();
    let _config = write_config(home.path(), "http://127.0.0.1:9/v1");
    let _lease = hold_mcp_ingest_worker_writer_lease(home.path());
    let payload = br#"{"content":"new stdin wait content queued behind MCP worker lease","wing":"smoke","room":"manual"}"#;

    let output = run_cli_with_stdin(
        home.path(),
        &[
            "ingest",
            "--stdin",
            "--source-type",
            "user_explicit",
            "--no-gate",
            "--wait",
            "--wait-timeout-secs",
            "0",
            "--json",
        ],
        payload,
    );

    assert!(
        !output.status.success(),
        "bounded receipt timeouts should exit non-zero, stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("SQLite writer lease `sqlite-writer` is already held"),
        "stdin wait must return a queue receipt, not a writer-lease conflict: {stderr}"
    );
    let stdout: Value = serde_json::from_slice(&output.stdout).expect("parse receipt JSON");
    assert_eq!(stdout["state"], "queued");
    assert_eq!(stdout["timed_out"], true);
    assert_eq!(stdout["drawer_id"], "");
    assert!(
        cleanup_ids_from_ingest_json(&stdout).is_empty(),
        "timed-out receipt must not expose cleanup ids: {stdout}"
    );
    let db = Database::open(&home.path().join(".mempal/palace.db")).expect("open db");
    assert_eq!(db.drawer_count().expect("drawer count"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_wait_json_timeout_returns_receipt_and_leaves_claim_queued() {
    let home = setup_home();
    let project_id = "mempal";
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    let _config_path = write_config(home.path(), &format!("http://{addr}/v1"));
    let db_path = home.path().join(".mempal/palace.db");
    handle.pause();

    let payload = serde_json::json!({
        "content": "cli wait json timeout content",
        "wing": "cli-wing"
    })
    .to_string();

    let mut child = Command::new(mempal_bin())
        .args([
            "ingest",
            "--stdin",
            "--project",
            project_id,
            "--source-type",
            "agent_observation",
            "--no-gate",
            "--wait",
            "--wait-timeout-secs",
            "6",
            "--json",
        ])
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mempal");
    {
        use std::io::Write as _;

        let mut stdin = child.stdin.take().expect("child stdin");
        stdin
            .write_all(payload.as_bytes())
            .expect("write stdin payload");
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let operation_id = loop {
        if let Some((operation_id, state)) = first_ingest_async_operation(&db_path) {
            assert!(
                matches!(state.as_str(), "queued" | "running"),
                "finite wait may claim local work while the caller timeout is still open, got {state}"
            );
            break operation_id;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "ingest wait worker did not enqueue operation"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let initial = PendingMessageStore::new_without_reclaim(&db_path)
        .operation_status(&operation_id)
        .expect("load initial status")
        .expect("operation record exists");
    assert!(
        matches!(initial.op_state.as_str(), "queued" | "running"),
        "finite wait may claim local work while the caller timeout is still open, got {}",
        initial.op_state
    );

    let output = wait_child_output_timeout(child, Duration::from_secs(9));
    assert!(
        !output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: Value = serde_json::from_slice(&output.stdout).expect("timed-out ingest JSON");
    assert_eq!(stdout["operation_id"], operation_id);
    assert_eq!(stdout["state"], "queued");
    assert_eq!(stdout["timed_out"], true);
    assert_eq!(stdout["drawer_id"], "");
    assert!(
        !stdout
            .as_object()
            .expect("ingest receipt object")
            .contains_key("drawer_ids"),
        "timed-out receipt must not report drawer ids: {stdout}"
    );
    assert!(
        cleanup_ids_from_ingest_json(&stdout).is_empty(),
        "timed-out receipt must not expose cleanup ids: {stdout}"
    );
    let queued = PendingMessageStore::new_without_reclaim(&db_path)
        .operation_status(&operation_id)
        .expect("load queued status")
        .expect("operation record exists");
    assert_eq!(queued.op_state, "queued");
    assert!(queued.claimed_at.is_none());

    handle.resume();
    let unbounded_timeout = u64::MAX.to_string();
    let recovery = run_cli(
        home.path(),
        &[
            "operation",
            "wait",
            &operation_id,
            "--timeout-secs",
            unbounded_timeout.as_str(),
            "--json",
        ],
    );

    assert_success(&recovery);
    let (recovery_stdout, recovery_stderr) = print_lines(&recovery);
    let recovery_json: Value =
        serde_json::from_str(&recovery_stdout).expect("parse operation wait recovery JSON");
    assert_eq!(recovery_json["operation_id"], operation_id);
    assert_eq!(recovery_json["state"], "completed");
    assert!(!recovery_json["timed_out"].as_bool().unwrap_or(false));
    assert!(
        !recovery_stdout.contains("cli wait json timeout content"),
        "operation wait JSON must not expose raw content: {recovery_stdout}"
    );
    assert!(
        recovery_stderr.contains("waiting for operation_id="),
        "{recovery_stderr}"
    );
    let recovery_ids = cleanup_ids_from_ingest_json(&recovery_json);
    assert_eq!(
        recovery_ids.len(),
        1,
        "operation wait recovery must expose exact cleanup ids: {recovery_stdout}"
    );
    assert_eq!(
        recovery_json["created_drawer_ids"], recovery_json["drawer_ids"],
        "new operation completion should report the same affected and cleanup-safe IDs"
    );
    let completed = PendingMessageStore::new_without_reclaim(&db_path)
        .operation_status(&operation_id)
        .expect("load completed status")
        .expect("operation record exists");
    assert_eq!(completed.op_state, "completed");
    let status = run_cli(
        home.path(),
        &["operation", "status", &operation_id, "--json"],
    );
    handle.shutdown().await;

    assert_success(&status);
    let status_json: Value =
        serde_json::from_slice(&status.stdout).expect("parse operation status JSON");
    assert_eq!(status_json["state"], "completed");
    assert_eq!(
        cleanup_ids_from_ingest_json(&status_json),
        recovery_ids,
        "operation status JSON must preserve cleanup-safe IDs after wait"
    );
    assert!(
        !String::from_utf8_lossy(&status.stdout).contains("cli wait json timeout content"),
        "operation status JSON must not expose raw content"
    );
    let db = Database::open(&db_path).expect("open db");
    assert_eq!(db.drawer_count().expect("drawer count"), 1);
    drop(db);

    delete_cleanup_ids(home.path(), &recovery_ids, Some(project_id));
    let db = Database::open(&db_path).expect("reopen db");
    for drawer_id in &recovery_ids {
        assert!(
            db.get_drawer(drawer_id)
                .expect("get deleted recovery drawer")
                .is_none(),
            "operation wait cleanup id must be deletable"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_wait_matches_non_wait_when_novelty_is_enabled() {
    let wait_home = setup_home();
    let direct_home = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    let _wait_config = write_config(wait_home.path(), &format!("http://{addr}/v1"));
    let _direct_config = write_config(direct_home.path(), &format!("http://{addr}/v1"));

    {
        use std::io::Write as _;

        for home in [wait_home.path(), direct_home.path()] {
            let config_path = home.join(".mempal/config.toml");
            let mut config = fs::OpenOptions::new()
                .append(true)
                .open(&config_path)
                .expect("open config");
            writeln!(config, "\n[ingest_gating.novelty]\nenabled = true")
                .expect("append novelty config");
        }
    }

    let seed_payload = br#"{"content":"novelty seed content"}"#;
    let seed_args = [
        "ingest",
        "--stdin",
        "--wing",
        "cli-wing",
        "--source-type",
        "user_explicit",
        "--no-gate",
    ];
    let seed_wait_output = run_cli_with_stdin(wait_home.path(), &seed_args, seed_payload);
    let seed_direct_output = run_cli_with_stdin(direct_home.path(), &seed_args, seed_payload);
    assert_success(&seed_wait_output);
    assert_success(&seed_direct_output);

    let wait_timeout = u64::MAX.to_string();
    let payload = br#"{"content":"novelty candidate content"}"#;

    let direct_output = run_cli_with_stdin(
        direct_home.path(),
        &[
            "ingest",
            "--stdin",
            "--wing",
            "cli-wing",
            "--source-type",
            "user_explicit",
            "--no-gate",
            "--json",
        ],
        payload,
    );
    let wait_output = run_cli_with_stdin(
        wait_home.path(),
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
            wait_timeout.as_str(),
            "--json",
        ],
        payload,
    );
    handle.shutdown().await;

    assert_success(&direct_output);
    assert_success(&wait_output);

    let direct_json: Value =
        serde_json::from_slice(&direct_output.stdout).expect("parse direct ingest JSON");
    let wait_json: Value = serde_json::from_slice(&wait_output.stdout).expect("parse wait JSON");
    assert_eq!(
        wait_json["stats"], direct_json["stats"],
        "wait stats must match direct stats when novelty is enabled"
    );
    let drawer_ids = wait_json["drawer_ids"]
        .as_array()
        .expect("drawer_ids array");
    assert!(!drawer_ids.is_empty(), "drawer_ids must be non-empty");
    assert_eq!(
        novelty_audit_count(wait_home.path()),
        0,
        "wait stdin ingest must not write novelty audits when non-wait stdin would not"
    );
    assert_eq!(
        novelty_audit_count(direct_home.path()),
        0,
        "direct stdin ingest must not write novelty audits"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_wait_preserves_stdin_semantics_and_audit() {
    let home = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    let config_path = write_config(home.path(), &format!("http://{addr}/v1"));
    {
        use std::io::Write as _;

        let mut config = fs::OpenOptions::new()
            .append(true)
            .open(&config_path)
            .expect("open config");
        writeln!(config, "\n[ingest_gating]\nenabled = true").expect("append gating config");
    }

    let payload = serde_json::json!({
        "content": "hi",
        "wing": "cli-wing",
        "room": "audit-room",
        "source": "csa-session",
        "source_file": "csa://session/99",
        "source_type": "user_explicit"
    })
    .to_string();

    let wait_timeout = u64::MAX.to_string();
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
            wait_timeout.as_str(),
        ],
        payload.as_bytes(),
    );
    handle.shutdown().await;

    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("dry_run=false"), "{stdout}");
    assert!(stdout.contains("files=1"), "{stdout}");
    assert!(stdout.contains("chunks=1"), "{stdout}");
    assert!(stdout.contains("skipped=0"), "{stdout}");
    assert!(stdout.contains("dropped_by_gate=0"), "{stdout}");
    assert!(stdout.contains("superseded_drawer_id="), "{stdout}");
    assert!(
        !stdout.lines().any(|line| line.starts_with("drawer_id=")),
        "{stdout}"
    );

    let db = Database::open(&home.path().join(".mempal/palace.db")).expect("open db");
    let expected_project = mempal::core::project::infer_project_id_from_path(
        &std::env::current_dir().expect("current dir"),
    )
    .expect("infer project id from cwd")
    .expect("expected project id from cwd");
    let drawer_id = db
        .find_active_drawers_by_content(
            "hi",
            "cli-wing",
            Some("audit-room"),
            Some(expected_project.as_str()),
        )
        .expect("find drawer by content")
        .into_iter()
        .next()
        .expect("drawer entry")
        .id;
    let drawer = db
        .get_drawer(&drawer_id)
        .expect("get drawer")
        .expect("drawer exists");
    assert_eq!(drawer.source_file.as_deref(), Some("csa://session/99"));
    assert_eq!(drawer.wing, "cli-wing");

    let audit_path = home.path().join(".mempal").join("audit.jsonl");
    let audit = fs::read_to_string(&audit_path).expect("read audit log");
    let entry: Value = serde_json::from_str(audit.lines().last().expect("audit entry"))
        .expect("parse audit entry");
    assert_eq!(entry["mode"], "stdin");
    assert_eq!(entry["source"], "csa-session");
    assert_eq!(entry["source_file"], "csa://session/99");
    assert_eq!(entry["dry_run"], false);
    assert_eq!(entry["dropped_by_gate"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_wait_exact_duplicate_matches_non_wait_plain_and_json_output() {
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");

    for json in [false, true] {
        let wait_home = setup_home();
        let direct_home = setup_home();
        let _wait_config = write_config(wait_home.path(), &format!("http://{addr}/v1"));
        let _direct_config = write_config(direct_home.path(), &format!("http://{addr}/v1"));

        let payload = serde_json::json!({
            "content": "duplicate wait content",
            "wing": "cli-wing",
            "room": "duplicate-room",
            "project": "duplicate-project",
            "source": "csa-session",
            "source_file": "csa://session/duplicate",
            "source_type": "user_explicit",
            "confidence": 0.82
        })
        .to_string();

        let seed_args = if json {
            vec!["ingest", "--stdin", "--no-gate", "--json"]
        } else {
            vec!["ingest", "--stdin", "--no-gate"]
        };
        let seed_wait = run_cli_with_stdin(wait_home.path(), &seed_args, payload.as_bytes());
        let seed_direct = run_cli_with_stdin(direct_home.path(), &seed_args, payload.as_bytes());
        assert!(
            seed_wait.status.success() && seed_direct.status.success(),
            "seeding duplicate fixture must succeed"
        );

        let wait_timeout = u64::MAX.to_string();
        let mut direct_args = vec![
            "ingest",
            "--stdin",
            "--wing",
            "cli-wing",
            "--source-type",
            "user_explicit",
        ];
        let mut wait_args = vec![
            "ingest",
            "--stdin",
            "--wing",
            "cli-wing",
            "--source-type",
            "user_explicit",
            "--wait",
            "--wait-timeout-secs",
            wait_timeout.as_str(),
        ];
        if json {
            direct_args.push("--json");
            wait_args.push("--json");
        }

        let direct_output =
            run_cli_with_stdin(direct_home.path(), &direct_args, payload.as_bytes());
        let wait_output = run_cli_with_stdin(wait_home.path(), &wait_args, payload.as_bytes());

        assert_success(&direct_output);
        assert_success(&wait_output);
        assert_bootstrap_stderr(&direct_output);
        assert_bootstrap_stderr(&wait_output);

        if json {
            let direct_json: Value =
                serde_json::from_slice(&direct_output.stdout).expect("parse direct duplicate JSON");
            let wait_json: Value =
                serde_json::from_slice(&wait_output.stdout).expect("parse wait duplicate JSON");
            assert_eq!(
                wait_json, direct_json,
                "wait JSON output must match direct duplicate output"
            );
            assert_eq!(direct_json["stats"]["files"], 1);
            assert_eq!(direct_json["stats"]["chunks"], 0);
            assert_eq!(direct_json["stats"]["skipped"], 1);
            assert_eq!(direct_json["stats"]["dropped_by_gate"], 0);
            assert_eq!(
                direct_json["drawer_ids"]
                    .as_array()
                    .expect("drawer ids")
                    .len(),
                1
            );
            assert!(
                direct_json["cleanup_drawer_ids"]
                    .as_array()
                    .expect("cleanup drawer ids")
                    .is_empty(),
                "exact duplicates must not expose cleanup ids for pre-existing drawers"
            );
            assert!(
                direct_json["created_drawer_ids"]
                    .as_array()
                    .expect("created drawer ids")
                    .is_empty(),
                "exact duplicates must not expose created ids for pre-existing drawers"
            );
            assert_eq!(direct_json["drawer_id"], "");
        } else {
            assert_eq!(
                String::from_utf8_lossy(&wait_output.stdout),
                String::from_utf8_lossy(&direct_output.stdout),
                "wait plain output must match direct duplicate output"
            );
            let stdout = String::from_utf8_lossy(&direct_output.stdout);
            assert!(stdout.contains("files=1"), "{stdout}");
            assert!(stdout.contains("chunks=0"), "{stdout}");
            assert!(stdout.contains("skipped=1"), "{stdout}");
            assert!(stdout.contains("dropped_by_gate=0"), "{stdout}");
        }

        let direct_audit = last_audit_entry(direct_home.path());
        let wait_audit = last_audit_entry(wait_home.path());
        assert_eq!(direct_audit["command"], "ingest");
        assert_eq!(wait_audit["command"], "ingest");
        assert_stdin_audit_entry(
            &direct_audit,
            "csa-session",
            "csa://session/duplicate",
            0,
            1,
            0,
        );
        assert_stdin_audit_entry(
            &wait_audit,
            "csa-session",
            "csa://session/duplicate",
            0,
            1,
            0,
        );
        assert!(direct_audit["superseded_drawer_id"].is_null());
        assert!(wait_audit["superseded_drawer_id"].is_null());
    }

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_wait_rejected_matches_non_wait_output_and_audit() {
    let wait_home = setup_home();
    let direct_home = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    let wait_config = write_config(wait_home.path(), &format!("http://{addr}/v1"));
    let direct_config = write_config(direct_home.path(), &format!("http://{addr}/v1"));
    {
        use std::io::Write as _;

        for config_path in [&wait_config, &direct_config] {
            let mut config = fs::OpenOptions::new()
                .append(true)
                .open(config_path)
                .expect("open config for append");
            writeln!(
                config,
                "\n[ingest_gating]\nenabled = true\n\n[ingest_gating.fact_check]\nenabled = true\nreject_on_contradiction = true"
            )
            .expect("append gating config");
        }
    }
    insert_fact_check_contradiction(wait_home.path());
    insert_fact_check_contradiction(direct_home.path());

    let payload = serde_json::json!({
        "content": "Bob is Alice's brother.",
        "wing": "cli-wing",
        "room": "reject-room",
        "project": "reject-project",
        "source": "csa-session",
        "source_file": "csa://session/reject",
        "source_type": "user_explicit",
        "confidence": 0.8
    })
    .to_string();

    let wait_timeout = u64::MAX.to_string();
    let direct_output = run_cli_with_stdin(
        direct_home.path(),
        &["ingest", "--stdin", "--json"],
        payload.as_bytes(),
    );
    let wait_output = run_cli_with_stdin(
        wait_home.path(),
        &[
            "ingest",
            "--stdin",
            "--wait",
            "--wait-timeout-secs",
            wait_timeout.as_str(),
            "--json",
        ],
        payload.as_bytes(),
    );

    assert_success(&direct_output);
    assert_success(&wait_output);
    assert_bootstrap_stderr(&direct_output);
    assert_bootstrap_stderr(&wait_output);

    let direct_json: Value = serde_json::from_slice(&direct_output.stdout).expect("direct json");
    let wait_json: Value = serde_json::from_slice(&wait_output.stdout).expect("wait json");
    assert_eq!(
        wait_json, direct_json,
        "wait rejection JSON must match direct rejection JSON"
    );
    assert_eq!(direct_json["stats"]["files"], 1);
    assert_eq!(direct_json["stats"]["chunks"], 0);
    assert_eq!(direct_json["stats"]["skipped"], 0);
    assert_eq!(direct_json["stats"]["dropped_by_gate"], 1);
    assert!(
        direct_json["drawer_ids"]
            .as_array()
            .expect("drawer ids")
            .is_empty(),
        "rejected ingest must not report drawer ids"
    );

    let direct_audit = last_audit_entry(direct_home.path());
    let wait_audit = last_audit_entry(wait_home.path());
    assert_stdin_audit_entry(
        &direct_audit,
        "csa-session",
        "csa://session/reject",
        0,
        0,
        1,
    );
    assert_stdin_audit_entry(&wait_audit, "csa-session", "csa://session/reject", 0, 0, 1);
    assert!(direct_audit["superseded_drawer_id"].is_null());
    assert!(wait_audit["superseded_drawer_id"].is_null());

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_operation_status_reports_queued_then_completed() {
    let home = setup_home();
    let (addr, _handle) = start_embed_mock(0).await.expect("start embed mock");
    let config_path = write_config(home.path(), &format!("http://{addr}/v1"));
    let _guard = ConfigOverrideGuard::install(&config_path);
    let config = Config::load_from(&config_path).expect("load config");
    let db_path = home.path().join(".mempal/palace.db");
    let server = MempalMcpServer::new(db_path.clone(), config).expect("create MCP server");

    let operation_id =
        enqueue_prepared_operation(&db_path, "status cli content", "mcp", Some("status"));

    let queued_output = run_cli(home.path(), &["operation", "status", &operation_id]);
    assert_queued_status(&queued_output);

    let warmup = start_worker(&server);
    let completed = server.wait_for_operation_completion(&operation_id).await;
    warmup.shutdown_and_drain().await;
    let completed = completed.expect("wait for completion");

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
    let db_path = home.path().join(".mempal/palace.db");

    let operation_id =
        enqueue_prepared_operation(&db_path, "wait cli content", "mcp", Some("wait"));
    let wait_timeout = "30".to_string();

    handle.pause();
    let child = spawn_cli(
        home.path(),
        &[
            "operation",
            "wait",
            &operation_id,
            "--timeout-secs",
            wait_timeout.as_str(),
        ],
    );
    tokio::time::sleep(Duration::from_millis(150)).await;

    tokio::time::sleep(Duration::from_millis(150)).await;
    handle.resume();

    let output = child.wait_with_output().expect("wait operation child");
    handle.shutdown().await;

    assert_success(&output);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_operation_wait_timeout_returns_receipt_and_leaves_finite_budget_queued() {
    let home = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    let config_path = write_config(home.path(), &format!("http://{addr}/v1"));
    let _guard = ConfigOverrideGuard::install(&config_path);
    let db_path = home.path().join(".mempal/palace.db");

    let operation_id = enqueue_prepared_operation(
        &db_path,
        "wait timeout drains active worker",
        "mcp",
        Some("wait-timeout"),
    );

    handle.pause();
    let child = spawn_cli(
        home.path(),
        &["operation", "wait", &operation_id, "--timeout-secs", "6"],
    );

    let output = wait_child_output_timeout(child, Duration::from_secs(9));
    assert!(
        !output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let (stdout, stderr) = print_lines(&output);
    assert!(stdout.contains("operation_id="), "{stdout}");
    assert!(stdout.contains("state=queued"), "{stdout}");
    assert!(stdout.contains("timed_out=true"), "{stdout}");
    assert!(stdout.contains("drawer_id="), "{stdout}");
    assert!(stderr.contains("waiting for operation_id="), "{stderr}");
    let queued = PendingMessageStore::new_without_reclaim(&db_path)
        .operation_status(&operation_id)
        .expect("load queued status")
        .expect("operation record exists");
    assert_eq!(queued.op_state, "queued");
    assert!(queued.claimed_at.is_none());

    handle.resume();
    let unbounded_timeout = u64::MAX.to_string();
    let recovery = run_cli(
        home.path(),
        &[
            "operation",
            "wait",
            &operation_id,
            "--timeout-secs",
            unbounded_timeout.as_str(),
        ],
    );
    handle.shutdown().await;

    assert_success(&recovery);
    let (recovery_stdout, recovery_stderr) = print_lines(&recovery);
    assert!(
        recovery_stdout.contains("state=completed"),
        "{recovery_stdout}"
    );
    assert!(
        recovery_stdout.contains("timed_out=false"),
        "{recovery_stdout}"
    );
    assert!(
        recovery_stderr.contains("waiting for operation_id="),
        "{recovery_stderr}"
    );

    let db = Database::open(&db_path).expect("open db");
    assert_eq!(db.drawer_count().expect("drawer count"), 1);
}
