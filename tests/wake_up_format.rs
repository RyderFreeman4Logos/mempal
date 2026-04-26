//! Integration tests for issue #79: `mempal wake-up --format` clap value_enum.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn setup() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    std::fs::create_dir_all(&mempal_home).expect("create mempal home");
    mempal::core::db::Database::open(&mempal_home.join("palace.db")).expect("open db");
    tmp
}

fn run_wake_up(home: &Path, args: &[&str]) -> Output {
    Command::new(mempal_bin())
        .arg("wake-up")
        .args(args)
        .env("HOME", home)
        .output()
        .expect("run mempal wake-up")
}

#[test]
fn test_wake_up_format_help_lists_possible_values() {
    let output = Command::new(mempal_bin())
        .args(["wake-up", "--help"])
        .output()
        .expect("run mempal wake-up --help");

    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("aaak"),
        "help should mention 'aaak', got: {help}"
    );
    assert!(
        help.contains("protocol"),
        "help should mention 'protocol', got: {help}"
    );
    assert!(
        help.contains("possible values") || help.contains("aaak") && help.contains("protocol"),
        "help should list possible values, got: {help}"
    );
}

#[test]
fn test_wake_up_format_rejects_text_at_parse_time() {
    let tmp = setup();
    let output = run_wake_up(tmp.path(), &["--format", "text"]);

    assert!(
        !output.status.success(),
        "expected non-zero exit for invalid format 'text'"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("text") || stderr.contains("invalid"),
        "expected parse error mentioning 'text', got: {stderr}"
    );
    // Must NOT be a runtime bail — clap must reject it before any DB access
    // (no "unsupported wake-up format" runtime message)
    assert!(
        !stderr.contains("unsupported wake-up format"),
        "should be clap parse error, not runtime bail: {stderr}"
    );
}

#[test]
fn test_wake_up_format_aaak_unchanged_behavior() {
    let tmp = setup();
    let output = run_wake_up(tmp.path(), &["--format", "aaak"]);

    assert!(
        output.status.success(),
        "expected success for --format aaak, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // AAAK output contains the AAAK codec header or "no recent drawers" message
    assert!(
        !stdout.is_empty(),
        "expected non-empty output for --format aaak"
    );
}

#[test]
fn test_wake_up_format_protocol_unchanged_behavior() {
    let tmp = setup();
    let output = run_wake_up(tmp.path(), &["--format", "protocol"]);

    assert!(
        output.status.success(),
        "expected success for --format protocol, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // MEMORY_PROTOCOL begins with "MEMPAL MEMORY PROTOCOL"
    assert!(
        stdout.contains("MEMPAL MEMORY PROTOCOL"),
        "expected protocol content in output, got: {stdout}"
    );
}

#[test]
fn test_wake_up_no_format_unchanged_behavior() {
    let tmp = setup();
    let output = run_wake_up(tmp.path(), &[]);

    assert!(
        output.status.success(),
        "expected success when --format omitted, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Default output includes "L0" section header
    assert!(
        stdout.contains("L0") || stdout.contains("drawer_count"),
        "expected default wake-up output, got: {stdout}"
    );
}
