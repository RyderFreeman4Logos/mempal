//! Integration tests for issue #82: friendly error messages when `mempal ingest`
//! receives a file path or a nonexistent path instead of a directory.

mod common;
#[path = "ingest_cli_errors/delete.rs"]
mod delete;

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use common::harness::start as start_embed_mock;
use serde_json::Value;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn setup_home() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    mempal::core::db::Database::open(&mempal_home.join("palace.db")).expect("open db");
    tmp
}

fn run_ingest(home: &Path, target: &str, wing: &str) -> Output {
    Command::new(mempal_bin())
        .args(["ingest", target, "--wing", wing])
        .env("HOME", home)
        .output()
        .expect("run mempal ingest")
}

fn run_ingest_dry(home: &Path, target: &str, wing: &str) -> Output {
    Command::new(mempal_bin())
        .args(["ingest", target, "--wing", wing, "--dry-run"])
        .env("HOME", home)
        .output()
        .expect("run mempal ingest --dry-run")
}

fn run_ingest_json(home: &Path, target: &str, wing: &str) -> Output {
    Command::new(mempal_bin())
        .args(["ingest", target, "--wing", wing, "--no-gate", "--json"])
        .env("HOME", home)
        .output()
        .expect("run mempal ingest --json")
}

fn run_ingest_stdin_json(home: &Path, payload: &str, args: &[&str]) -> Output {
    run_ingest_stdin_bytes(home, payload.as_bytes(), args)
}

fn run_ingest_stdin_bytes(home: &Path, payload: &[u8], args: &[&str]) -> Output {
    let mut command = Command::new(mempal_bin());
    command
        .arg("ingest")
        .arg("--stdin")
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn mempal ingest --stdin");
    if let Some(stdin) = child.stdin.as_mut() {
        match stdin.write_all(payload) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::BrokenPipe => {}
            Err(error) => panic!("write stdin payload: {error}"),
        }
    }
    child
        .wait_with_output()
        .expect("wait mempal ingest --stdin")
}

fn hold_daemon_writer_lease(home: &Path) -> mempal::core::types::RuntimeWriterLease {
    let db =
        mempal::core::db::Database::open(&home.join(".mempal").join("palace.db")).expect("open db");
    db.runtime_writer_lease_acquire("sqlite-writer", "daemon-owner", "daemon", 300, None)
        .expect("acquire daemon writer lease")
        .expect("daemon writer lease")
}

fn write_embed_config(home: &Path, base_url: &str) {
    write_embed_config_with_privacy(home, base_url, false);
}

fn write_embed_config_with_hook_llm_judge(home: &Path, embed_base_url: &str, llm_base_url: &str) {
    let db_path = home.join(".mempal").join("palace.db");
    let config = format!(
        r#"
db_path = "{}"

[embed]
backend = "openai_compat"
base_url = "{}"
api_model = "test-embed"
dim = 4

[embed.openai_compat]
base_url = "{}"
model = "test-embed"
dim = 4
request_timeout_secs = 2

[llm]
enabled = true
base_url = "{}"
model = "test-llm"
request_timeout_secs = 1
retry_interval_secs = 1
enabled_for = ["gating"]

[hooks]
enabled = true

[gating]
enabled = true

[gating.llm_judge]
enabled = true
threshold = 0.5

[privacy]
enabled = false
"#,
        db_path.display(),
        embed_base_url,
        embed_base_url,
        llm_base_url
    );
    fs::write(home.join(".mempal").join("config.toml"), config).expect("write config");
}

fn write_embed_config_with_privacy(home: &Path, base_url: &str, privacy_enabled: bool) {
    let db_path = home.join(".mempal").join("palace.db");
    let config = format!(
        r#"
db_path = "{}"

[embed]
backend = "openai_compat"
base_url = "{}"
api_model = "test-embed"
dim = 4

[embed.openai_compat]
base_url = "{}"
model = "test-embed"
dim = 4
request_timeout_secs = 2

[privacy]
enabled = {}
"#,
        db_path.display(),
        base_url,
        base_url,
        privacy_enabled
    );
    fs::write(home.join(".mempal").join("config.toml"), config).expect("write config");
}

fn assert_stdin_error(output: Output, expected: &str) {
    assert!(
        !output.status.success(),
        "expected non-zero exit for stdin error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected),
        "stderr must contain {expected:?}, got: {stderr}"
    );
}

#[test]
fn test_ingest_file_path_returns_friendly_error() {
    let tmp = setup_home();
    let file = tmp.path().join("single-doc.md");
    fs::write(&file, "# Hello\nsome content").expect("write fixture");

    let output = run_ingest(tmp.path(), file.to_str().unwrap(), "test");

    assert!(
        !output.status.success(),
        "expected non-zero exit when given a file path"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("expects a DIRECTORY"),
        "error must mention 'expects a DIRECTORY', got: {stderr}"
    );
    assert!(
        stderr.contains("mkdir") && stderr.contains("mempal ingest"),
        "error must include mkdir+cp+ingest workaround suggestion, got: {stderr}"
    );
}

#[test]
fn test_ingest_nonexistent_path_returns_friendly_error() {
    let tmp = setup_home();
    let output = run_ingest(tmp.path(), "/nonexistent/path/does-not-exist", "test");

    assert!(
        !output.status.success(),
        "expected non-zero exit for nonexistent path"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not exist"),
        "error must mention 'does not exist', got: {stderr}"
    );
}

#[test]
fn test_ingest_dir_unchanged_behavior() {
    let tmp = setup_home();
    let source_dir = tmp.path().join("source");
    fs::create_dir_all(&source_dir).expect("create source dir");
    fs::write(source_dir.join("note.md"), "# Note\nsome content here").expect("write file");

    // Use --dry-run to avoid needing a live embedder in CI.
    let output = run_ingest_dry(tmp.path(), source_dir.to_str().unwrap(), "test");

    assert!(
        output.status.success(),
        "ingest of a directory must still succeed, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("files="),
        "output must contain file stats, got: {stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_json_outputs_created_drawer_ids() {
    let tmp = setup_home();
    let source_dir = tmp.path().join("source");
    fs::create_dir_all(&source_dir).expect("create source dir");
    fs::write(source_dir.join("note.md"), "# Note\njson drawer id content").expect("write file");
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    write_embed_config(tmp.path(), &format!("http://{addr}/v1"));

    let output = run_ingest_json(tmp.path(), source_dir.to_str().expect("source dir"), "test");
    handle.shutdown().await;

    assert!(
        output.status.success(),
        "ingest --json must succeed, stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("parse ingest JSON stdout");
    assert_eq!(json["dry_run"], false);
    assert_eq!(json["files"], 1);
    let chunks = json["chunks"].as_u64().expect("chunks number");
    let drawer_ids = json["drawer_ids"].as_array().expect("drawer_ids array");
    assert!(!drawer_ids.is_empty(), "drawer_ids must be non-empty");
    assert_eq!(drawer_ids.len() as u64, chunks);
    assert!(
        drawer_ids.iter().all(|value| value.as_str().is_some()),
        "drawer_ids must contain strings"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_stdin_json_creates_single_drawer() {
    let tmp = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    write_embed_config(tmp.path(), &format!("http://{addr}/v1"));

    let payload = r#"{
        "content": "stdin memory content from issue 99",
        "wing": "json-wing",
        "room": "json-room",
        "project": "json-project",
        "source": "csa-session",
        "source_file": "csa://session/99",
        "metadata": { "issue": 99 }
    }"#;
    let output = run_ingest_stdin_json(
        tmp.path(),
        payload,
        &[
            "--wing",
            "cli-wing",
            "--source-type",
            "user_explicit",
            "--confidence",
            "0.82",
            "--no-gate",
            "--json",
        ],
    );
    handle.shutdown().await;

    assert!(
        output.status.success(),
        "ingest --stdin --json must succeed, stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("parse stdin ingest JSON");
    let drawer_ids = json["drawer_ids"].as_array().expect("drawer_ids array");
    assert_eq!(drawer_ids.len(), 1);
    assert_eq!(json["stats"]["files"], 1);
    assert_eq!(json["stats"]["chunks"], 1);
    assert_eq!(json["stats"]["dropped_by_gate"], 0);

    let drawer_id = drawer_ids[0].as_str().expect("drawer id string");
    let db = mempal::core::db::Database::open(&tmp.path().join(".mempal").join("palace.db"))
        .expect("open db");
    let drawer = db
        .get_drawer(drawer_id)
        .expect("get drawer")
        .expect("drawer exists");
    assert_eq!(drawer.wing, "cli-wing");
    assert_eq!(drawer.room.as_deref(), Some("json-room"));
    assert_eq!(drawer.source_file.as_deref(), Some("csa://session/99"));
    assert_eq!(
        drawer.source_type,
        mempal::core::types::SourceType::UserExplicit
    );
    assert!((drawer.confidence - 0.82).abs() < f64::EPSILON);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_stdin_user_explicit_bypasses_hook_llm_gate() {
    let tmp = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    write_embed_config_with_hook_llm_judge(
        tmp.path(),
        &format!("http://{addr}/v1"),
        "http://127.0.0.1:9/v1",
    );

    let payload = r#"{
        "content": "explicit CLI memory should not wait for automatic hook LLM filtering",
        "wing": "explicit-wing",
        "room": "explicit-room",
        "source_file": "cli://stdin/explicit"
    }"#;
    let output = run_ingest_stdin_json(
        tmp.path(),
        payload,
        &["--source-type", "user_explicit", "--json"],
    );
    handle.shutdown().await;

    assert!(
        output.status.success(),
        "explicit stdin ingest must not call the unavailable LLM, stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("parse stdin ingest JSON");
    let drawer_id = json["drawer_ids"][0].as_str().expect("drawer id string");
    let db = mempal::core::db::Database::open(&tmp.path().join(".mempal").join("palace.db"))
        .expect("open db");
    let drawer = db
        .get_drawer(drawer_id)
        .expect("get drawer")
        .expect("drawer exists");
    assert_eq!(drawer.wing, "explicit-wing");
    assert_eq!(drawer.room.as_deref(), Some("explicit-room"));
    assert_eq!(
        drawer.source_type,
        mempal::core::types::SourceType::UserExplicit
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_stdin_applies_privacy_scrubbing() {
    let tmp = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    write_embed_config_with_privacy(tmp.path(), &format!("http://{addr}/v1"), true);
    let secret = format!("sk-{}", "0".repeat(40));
    let payload = serde_json::json!({
        "content": format!("keep {secret} and <private>hidden</private>"),
        "wing": "privacy-wing",
        "room": "privacy-room",
        "source_file": "stdin://privacy"
    })
    .to_string();

    let output = run_ingest_stdin_json(tmp.path(), &payload, &["--no-gate", "--json"]);
    handle.shutdown().await;

    assert!(
        output.status.success(),
        "ingest --stdin must succeed, stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("parse stdin ingest JSON");
    let drawer_id = json["drawer_ids"][0].as_str().expect("drawer id string");
    let db = mempal::core::db::Database::open(&tmp.path().join(".mempal").join("palace.db"))
        .expect("open db");
    let drawer = db
        .get_drawer(drawer_id)
        .expect("get drawer")
        .expect("drawer exists");
    assert!(
        drawer.content.contains("[REDACTED:openai_key]"),
        "{}",
        drawer.content
    );
    assert!(!drawer.content.contains(&secret), "{}", drawer.content);
    assert!(!drawer.content.contains("hidden"), "{}", drawer.content);
}

#[test]
fn test_ingest_stdin_scrubs_metadata_in_audit_log() {
    let tmp = setup_home();
    write_embed_config_with_privacy(tmp.path(), "http://127.0.0.1:9/v1", true);
    let secret = format!("sk-{}", "1".repeat(40));
    let payload = serde_json::json!({
        "content": "metadata audit content",
        "wing": "privacy-wing",
        "metadata": {
            "token": secret,
            secret.clone(): "secret-in-key",
            "nested": {
                "tokens": [format!("prefix {secret}")]
            }
        }
    })
    .to_string();

    let output = run_ingest_stdin_json(tmp.path(), &payload, &["--dry-run", "--json"]);

    assert!(
        output.status.success(),
        "ingest --stdin dry-run must succeed, stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let audit_path = tmp.path().join(".mempal").join("audit.jsonl");
    let audit = fs::read_to_string(&audit_path).expect("read audit log");
    assert!(!audit.contains(&secret), "{audit}");
    assert!(audit.contains("[REDACTED:openai_key]"), "{audit}");

    let entry: Value = serde_json::from_str(audit.lines().last().expect("audit entry"))
        .expect("parse audit entry");
    assert_eq!(entry["metadata"]["token"], "[REDACTED:openai_key]");
    assert_eq!(
        entry["metadata"]["nested"]["tokens"][0],
        "prefix [REDACTED:openai_key]"
    );

    // Verify source/source_file are also scrubbed in audit log
    assert!(
        !audit.contains("sk-"),
        "source/source_file secret leaked: {audit}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_stdin_scrubs_source_fields_in_audit_and_drawer() {
    let tmp = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    write_embed_config_with_privacy(tmp.path(), &format!("http://{addr}/v1"), true);
    let secret = format!("sk-{}", "2".repeat(40));
    let payload = serde_json::json!({
        "content": "source field privacy content",
        "wing": "privacy-wing",
        "source": format!("hook-{secret}"),
        "source_file": secret
    })
    .to_string();

    let output = run_ingest_stdin_json(tmp.path(), &payload, &["--json"]);
    handle.shutdown().await;
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let audit_path = tmp.path().join(".mempal").join("audit.jsonl");
    let audit = fs::read_to_string(&audit_path).expect("read audit log");
    assert!(!audit.contains(&secret), "source secret in audit: {audit}");

    let entry: Value = serde_json::from_str(audit.lines().last().expect("audit entry"))
        .expect("parse audit entry");
    assert!(
        entry["source"]
            .as_str()
            .unwrap()
            .contains("[REDACTED:openai_key]"),
        "source not scrubbed: {}",
        entry["source"]
    );
    assert_eq!(entry["source_file"], "[REDACTED:openai_key]");

    let json: Value = serde_json::from_slice(&output.stdout).expect("parse stdout JSON");
    let drawer_id = json["drawer_ids"][0].as_str().expect("drawer id");
    let db_path = tmp.path().join(".mempal").join("palace.db");
    let db = mempal::core::db::Database::open(&db_path).expect("open db");
    let drawer = db
        .get_drawer(drawer_id)
        .expect("get drawer")
        .expect("drawer exists");
    assert!(
        !drawer
            .source_file
            .as_deref()
            .unwrap_or("")
            .contains(&secret),
        "source_file secret in drawer: {:?}",
        drawer.source_file
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_stdin_replace_text_uses_scrubbed_text_for_lookup() {
    let tmp = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    write_embed_config_with_privacy(tmp.path(), &format!("http://{addr}/v1"), true);

    let raw_old = "old <private>hidden</private> fact";
    let old_payload = serde_json::json!({
        "content": raw_old,
        "wing": "privacy-wing",
        "room": "privacy-room",
        "project": "privacy-project"
    })
    .to_string();
    let old_output = run_ingest_stdin_json(tmp.path(), &old_payload, &["--no-gate", "--json"]);
    assert!(
        old_output.status.success(),
        "old ingest must succeed, stdout={}, stderr={}",
        String::from_utf8_lossy(&old_output.stdout),
        String::from_utf8_lossy(&old_output.stderr)
    );
    let old_json: Value =
        serde_json::from_slice(&old_output.stdout).expect("parse old ingest JSON");
    let old_id = old_json["drawer_ids"][0]
        .as_str()
        .expect("old drawer id")
        .to_string();

    let replacement_payload = serde_json::json!({
        "content": "new scrubbed replacement fact",
        "wing": "privacy-wing",
        "room": "privacy-room",
        "project": "privacy-project"
    })
    .to_string();
    let replacement_output = run_ingest_stdin_json(
        tmp.path(),
        &replacement_payload,
        &["--no-gate", "--json", "--replace-text", raw_old],
    );
    handle.shutdown().await;

    assert!(
        replacement_output.status.success(),
        "replacement ingest must match scrubbed replace_text, stdout={}, stderr={}",
        String::from_utf8_lossy(&replacement_output.stdout),
        String::from_utf8_lossy(&replacement_output.stderr)
    );
    let replacement_json: Value =
        serde_json::from_slice(&replacement_output.stdout).expect("parse replacement JSON");
    assert_eq!(
        replacement_json["stats"]["superseded_drawer_id"],
        Value::String(old_id.clone())
    );

    let db = mempal::core::db::Database::open(&tmp.path().join(".mempal").join("palace.db"))
        .expect("open db");
    assert!(db.get_drawer(&old_id).expect("old lookup").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_stdin_replacement_insert_failure_preserves_old_drawer() {
    let tmp = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    write_embed_config(tmp.path(), &format!("http://{addr}/v1"));

    let old_payload = serde_json::json!({
        "content": "old durable replacement fact",
        "wing": "replace-wing",
        "room": "replace-room",
        "project": "replace-project"
    })
    .to_string();
    let old_output = run_ingest_stdin_json(tmp.path(), &old_payload, &["--no-gate", "--json"]);
    assert!(
        old_output.status.success(),
        "old ingest must succeed, stdout={}, stderr={}",
        String::from_utf8_lossy(&old_output.stdout),
        String::from_utf8_lossy(&old_output.stderr)
    );
    let old_json: Value =
        serde_json::from_slice(&old_output.stdout).expect("parse old ingest JSON");
    let old_id = old_json["drawer_ids"][0]
        .as_str()
        .expect("old drawer id")
        .to_string();

    let db = mempal::core::db::Database::open(&tmp.path().join(".mempal").join("palace.db"))
        .expect("open db");
    db.conn()
        .execute_batch(
            r#"
            CREATE TRIGGER fail_replacement_drawer_insert
            BEFORE INSERT ON drawers
            BEGIN
                SELECT RAISE(FAIL, 'forced replacement drawer insert failure');
            END;
            "#,
        )
        .expect("install failure trigger");

    let replacement_payload = serde_json::json!({
        "content": "new replacement fact that cannot be stored",
        "wing": "replace-wing",
        "room": "replace-room",
        "project": "replace-project"
    })
    .to_string();
    let replacement_output = run_ingest_stdin_json(
        tmp.path(),
        &replacement_payload,
        &["--no-gate", "--json", "--supersedes", &old_id],
    );
    handle.shutdown().await;

    assert!(
        !replacement_output.status.success(),
        "replacement ingest must fail, stdout={}, stderr={}",
        String::from_utf8_lossy(&replacement_output.stdout),
        String::from_utf8_lossy(&replacement_output.stderr)
    );
    let stderr = String::from_utf8_lossy(&replacement_output.stderr);
    assert!(
        stderr.contains("forced replacement drawer insert failure"),
        "stderr must include forced failure, got: {stderr}"
    );
    assert!(
        db.get_drawer(&old_id).expect("old lookup").is_some(),
        "old drawer must remain active when replacement storage fails"
    );
}

#[test]
fn test_ingest_stdin_rejects_invalid_json() {
    let tmp = setup_home();
    let output = run_ingest_stdin_json(tmp.path(), "not json", &["--wing", "test"]);

    assert_stdin_error(output, "failed to parse stdin JSON object");
}

#[test]
fn test_ingest_stdin_rejects_missing_content() {
    let tmp = setup_home();
    let output = run_ingest_stdin_json(tmp.path(), r#"{"wing":"test"}"#, &[]);

    assert_stdin_error(
        output,
        "stdin JSON object is missing required `content` field",
    );
}

#[test]
fn test_ingest_stdin_rejects_empty_content() {
    let tmp = setup_home();
    let output = run_ingest_stdin_json(tmp.path(), r#"{"content":"   ","wing":"test"}"#, &[]);

    assert_stdin_error(output, "stdin JSON `content` field must not be empty");
}

#[test]
fn test_ingest_stdin_rejects_missing_wing() {
    let tmp = setup_home();
    let output = run_ingest_stdin_json(tmp.path(), r#"{"content":"hello"}"#, &[]);

    assert_stdin_error(output, "stdin ingest requires --wing or JSON `wing`");
}

#[test]
fn test_ingest_stdin_rejects_directory_path() {
    let tmp = setup_home();
    let source_dir = tmp.path().join("source");
    fs::create_dir_all(&source_dir).expect("create source dir");
    let source_dir = source_dir.to_str().expect("source dir path");
    let output = run_ingest_stdin_json(
        tmp.path(),
        r#"{"content":"hello","wing":"test"}"#,
        &[source_dir],
    );

    assert_stdin_error(
        output,
        "`mempal ingest --stdin` cannot be combined with directory path",
    );
}

#[test]
fn test_ingest_stdin_rejects_format() {
    let tmp = setup_home();
    let output = run_ingest_stdin_json(
        tmp.path(),
        r#"{"content":"hello","wing":"test"}"#,
        &["--format", "convos"],
    );

    assert_stdin_error(output, "--format is only supported for directory ingest");
}

#[test]
fn test_directory_ingest_respects_existing_writer_lease() {
    let tmp = setup_home();
    let _lease = hold_daemon_writer_lease(tmp.path());
    let input_dir = tmp.path().join("input");
    fs::create_dir_all(&input_dir).expect("create input dir");
    fs::write(
        input_dir.join("memory.md"),
        "directory lease conflict memory",
    )
    .expect("write input");

    let output = run_ingest(
        tmp.path(),
        input_dir.to_str().expect("input path utf8"),
        "lease-wing",
    );

    assert!(
        !output.status.success(),
        "directory ingest must fail under writer lease"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("SQLite writer lease `sqlite-writer` is already held"),
        "{stderr}"
    );
    let db = mempal::core::db::Database::open(&tmp.path().join(".mempal").join("palace.db"))
        .expect("open db");
    assert_eq!(db.drawer_count().expect("drawer count"), 0);
}

#[test]
fn test_stdin_non_wait_ingest_respects_existing_writer_lease() {
    let tmp = setup_home();
    let _lease = hold_daemon_writer_lease(tmp.path());
    let payload = r#"{
        "content": "stdin direct lease conflict memory",
        "wing": "lease-wing",
        "room": "lease-room"
    }"#;

    let output = run_ingest_stdin_json(tmp.path(), payload, &["--no-gate", "--json"]);

    assert!(
        !output.status.success(),
        "stdin ingest must fail under writer lease"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("SQLite writer lease `sqlite-writer` is already held"),
        "{stderr}"
    );
    let db = mempal::core::db::Database::open(&tmp.path().join(".mempal").join("palace.db"))
        .expect("open db");
    assert_eq!(db.drawer_count().expect("drawer count"), 0);
}

#[test]
fn test_ingest_stdin_rejects_diary_rollup() {
    let tmp = setup_home();
    let output = run_ingest_stdin_json(
        tmp.path(),
        r#"{"content":"hello","wing":"agent-diary","room":"codex"}"#,
        &["--diary-rollup"],
    );

    assert_stdin_error(
        output,
        "--diary-rollup is only supported for directory ingest",
    );
}

#[test]
fn test_ingest_stdin_rejects_payload_over_size_limit() {
    let tmp = setup_home();
    let payload = vec![b'a'; 10 * 1024 * 1024 + 1];
    let output = run_ingest_stdin_bytes(tmp.path(), &payload, &["--wing", "test"]);

    assert_stdin_error(output, "stdin payload exceeds 10485760 byte limit");
}
