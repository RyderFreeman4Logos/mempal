mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use common::harness::embed_mock::start as start_embed_mock;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::core::queue::PendingMessageStore;
use mempal::core::types::Triple;
use mempal::core::utils::build_triple_id;
use mempal::mcp::{IngestOperationState, IngestRequest, MempalMcpServer};
use rmcp::handler::server::wrapper::Parameters;
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
async fn test_ingest_wait_matches_non_wait_plain_output() {
    let wait_home = setup_home();
    let direct_home = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    handle.set_embedding_fill(1.0).await;
    let _wait_config = write_config(wait_home.path(), &format!("http://{addr}/v1"));
    let _direct_config = write_config(direct_home.path(), &format!("http://{addr}/v1"));
    let wait_timeout = u64::MAX.to_string();
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

    assert!(
        direct_output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&direct_output.stdout),
        String::from_utf8_lossy(&direct_output.stderr)
    );
    assert!(
        wait_output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&wait_output.stdout),
        String::from_utf8_lossy(&wait_output.stderr)
    );

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
    let wait_timeout = u64::MAX.to_string();
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

    assert!(
        direct_output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&direct_output.stdout),
        String::from_utf8_lossy(&direct_output.stderr)
    );
    assert!(
        wait_output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&wait_output.stdout),
        String::from_utf8_lossy(&wait_output.stderr)
    );

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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_wait_json_timeout_drains_active_scoped_worker_before_exit() {
    let home = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    let _config = write_config(home.path(), &format!("http://{addr}/v1"));
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
            "mempal",
            "--source-type",
            "agent_observation",
            "--no-gate",
            "--wait",
            "--wait-timeout-secs",
            "1",
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
        if let Some((operation_id, state)) = first_ingest_async_operation(&db_path)
            && state == "running"
        {
            break operation_id;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "ingest wait worker did not claim operation"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let running = PendingMessageStore::new_without_reclaim(&db_path)
        .operation_status(&operation_id)
        .expect("load running status")
        .expect("operation record exists");
    assert_eq!(running.op_state, "running");

    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert!(
        child.try_wait().expect("poll ingest wait").is_none(),
        "ingest --stdin --wait must not exit while its scoped worker is mid-ingest"
    );

    handle.resume();
    let output = child.wait_with_output().expect("wait ingest child");
    handle.shutdown().await;

    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: Value = serde_json::from_slice(&output.stdout).expect("completed ingest JSON");
    assert_eq!(stdout["stats"]["files"], 1);
    assert_eq!(stdout["stats"]["chunks"], 1);
    assert_eq!(stdout["stats"]["skipped"], 0);
    let completed = PendingMessageStore::new_without_reclaim(&db_path)
        .operation_status(&operation_id)
        .expect("load completed status")
        .expect("operation record exists");
    assert_eq!(completed.op_state, "completed");
    let db = Database::open(&db_path).expect("open db");
    assert_eq!(db.drawer_count().expect("drawer count"), 1);
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
    assert!(
        seed_wait_output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&seed_wait_output.stdout),
        String::from_utf8_lossy(&seed_wait_output.stderr)
    );
    assert!(
        seed_direct_output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&seed_direct_output.stdout),
        String::from_utf8_lossy(&seed_direct_output.stderr)
    );

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

    assert!(
        direct_output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&direct_output.stdout),
        String::from_utf8_lossy(&direct_output.stderr)
    );
    assert!(
        wait_output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&wait_output.stdout),
        String::from_utf8_lossy(&wait_output.stderr)
    );

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
        payload.as_bytes(),
    );
    handle.shutdown().await;

    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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

        assert!(
            direct_output.status.success(),
            "stdout={}, stderr={}",
            String::from_utf8_lossy(&direct_output.stdout),
            String::from_utf8_lossy(&direct_output.stderr)
        );
        assert!(
            wait_output.status.success(),
            "stdout={}, stderr={}",
            String::from_utf8_lossy(&wait_output.stdout),
            String::from_utf8_lossy(&wait_output.stderr)
        );
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
            "30",
            "--json",
        ],
        payload.as_bytes(),
    );

    assert!(
        direct_output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&direct_output.stdout),
        String::from_utf8_lossy(&direct_output.stderr)
    );
    assert!(
        wait_output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&wait_output.stdout),
        String::from_utf8_lossy(&wait_output.stderr)
    );
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
    let db_path = home.path().join(".mempal/palace.db");

    let operation_id =
        enqueue_prepared_operation(&db_path, "wait cli content", "mcp", Some("wait"));
    let wait_timeout = u64::MAX.to_string();

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_operation_wait_timeout_drains_active_scoped_worker_before_exit() {
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
    let mut child = spawn_cli(
        home.path(),
        &["operation", "wait", &operation_id, "--timeout-secs", "1"],
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let record = PendingMessageStore::new_without_reclaim(&db_path)
            .operation_status(&operation_id)
            .expect("load operation status")
            .expect("operation record exists");
        if record.op_state == "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let running = PendingMessageStore::new_without_reclaim(&db_path)
        .operation_status(&operation_id)
        .expect("load running status")
        .expect("operation record exists");
    assert_eq!(running.op_state, "running");

    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert!(
        child.try_wait().expect("poll operation wait").is_none(),
        "operation wait must not exit while its scoped worker is mid-ingest"
    );

    handle.resume();
    let output = child.wait_with_output().expect("wait operation child");
    handle.shutdown().await;

    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let (stdout, stderr) = print_lines(&output);
    assert!(stdout.contains("state=completed"), "{stdout}");
    assert!(stdout.contains("timed_out=false"), "{stdout}");
    assert!(stderr.contains("waiting for operation_id="), "{stderr}");

    let db = Database::open(&db_path).expect("open db");
    assert_eq!(db.drawer_count().expect("drawer count"), 1);
}
