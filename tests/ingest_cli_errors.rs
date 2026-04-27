//! Integration tests for issue #82: friendly error messages when `mempal ingest`
//! receives a file path or a nonexistent path instead of a directory.

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

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

fn write_embed_config(home: &Path, base_url: &str) {
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
"#,
        db_path.display(),
        base_url,
        base_url
    );
    fs::write(home.join(".mempal").join("config.toml"), config).expect("write config");
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
