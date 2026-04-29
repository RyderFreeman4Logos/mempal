//! Integration tests for issue #82: friendly error messages when `mempal ingest`
//! receives a file path or a nonexistent path instead of a directory.

mod common;

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

fn write_embed_config(home: &Path, base_url: &str) {
    write_embed_config_with_privacy(home, base_url, false);
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
        &["--wing", "cli-wing", "--no-gate", "--json"],
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
