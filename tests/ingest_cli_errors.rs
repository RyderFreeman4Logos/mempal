//! Integration tests for issue #82: friendly error messages when `mempal ingest`
//! receives a file path or a nonexistent path instead of a directory.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

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
