use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use mempal::core::db::Database;
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn setup_home() -> (TempDir, PathBuf) {
    setup_home_with_extra_config("")
}

fn setup_home_with_extra_config(extra_config: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");
    fs::write(
        mempal_home.join("config.toml"),
        format!(
            r#"
db_path = "{}"

[hooks]
enabled = true
{extra_config}
"#,
            db_path.display()
        ),
    )
    .expect("write config");
    (tmp, db_path)
}

fn run_hook(home: &TempDir, command: &str, payload: &[u8]) -> std::process::Output {
    let mut child = Command::new(mempal_bin())
        .args(["hook", command])
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook command");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(payload)
        .expect("write payload");
    child.wait_with_output().expect("wait output")
}

fn pending_message_count(db_path: &PathBuf) -> i64 {
    let conn = Connection::open(db_path).expect("open sqlite");
    conn.query_row("SELECT COUNT(*) FROM pending_messages", [], |row| {
        row.get(0)
    })
    .expect("query pending count")
}

#[test]
fn test_hook_post_tool_enqueues_to_queue() {
    let (home, db_path) = setup_home();
    let payload = r#"{"tool_name":"Bash","input":"ls","exit_code":0,"output":"ok"}"#;

    let mut child = Command::new(mempal_bin())
        .args(["hook", "hook_post_tool"])
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook command");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(payload.as_bytes())
        .expect("write payload");
    let output = child.wait_with_output().expect("wait output");

    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );

    let conn = Connection::open(db_path).expect("open sqlite");
    let (kind, envelope): (String, String) = conn
        .query_row(
            "SELECT kind, payload FROM pending_messages ORDER BY created_at DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query queue");
    assert_eq!(kind, "hook_post_tool");
    let envelope_json: Value = serde_json::from_str(&envelope).expect("envelope json");
    assert_eq!(envelope_json["event"], "PostToolUse");
    assert_eq!(envelope_json["payload"], payload);
}

#[test]
fn test_hook_writes_nothing_to_stdout() {
    let (home, _db_path) = setup_home();
    let payload = r#"{"tool_name":"Bash","input":"ls","exit_code":0,"output":"ok"}"#;

    let mut child = Command::new(mempal_bin())
        .args(["hook", "hook_post_tool"])
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook command");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(payload.as_bytes())
        .expect("write payload");
    let output = child.wait_with_output().expect("wait output");

    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn test_hook_envelopes_oversized_payload() {
    let (home, db_path) = setup_home();
    let mut oversized = String::from("{\"payload\":\"");
    oversized.push_str(&"a".repeat((10 * 1024 * 1024) - 64));
    oversized.push('你');
    oversized.push_str(&"b".repeat(1024 * 1024));
    oversized.push_str("\"}");

    let mut child = Command::new(mempal_bin())
        .args(["hook", "hook_post_tool"])
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook command");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(oversized.as_bytes())
        .expect("write payload");
    let output = child.wait_with_output().expect("wait output");

    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("payload envelope-wrapped"),
        "stderr should mention envelope-wrapped, got: {stderr}"
    );

    let conn = Connection::open(db_path).expect("open sqlite");
    let envelope: String = conn
        .query_row(
            "SELECT payload FROM pending_messages ORDER BY created_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("query queue");
    let envelope_json: Value = serde_json::from_str(&envelope).expect("envelope json");

    assert_eq!(envelope_json["truncated"], true);
    assert!(
        envelope_json["original_size_bytes"]
            .as_u64()
            .expect("original size")
            > 10_000_000
    );
    let preview = envelope_json["payload_preview"]
        .as_str()
        .expect("preview string");
    assert!(preview.len() <= 4096);
    let payload_path = PathBuf::from(
        envelope_json["payload_path"]
            .as_str()
            .expect("payload path string"),
    );
    assert!(payload_path.exists(), "oversized payload file must exist");
}

#[test]
fn test_hook_storage_mode_off_skips_raw_turn_enqueue() {
    let (home, db_path) = setup_home_with_extra_config(
        r#"
[turns]
storage_mode = "off"
raw_turn_wings = ["hooks-raw"]
raw_turn_rooms = []
"#,
    );
    let payload = r#"{"prompt":"do not persist raw turn"}"#;

    let output = run_hook(&home, "hook_user_prompt", payload.as_bytes());

    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(pending_message_count(&db_path), 0);
}

#[test]
fn test_hook_storage_mode_off_skips_oversize_payload_file() {
    let (home, db_path) = setup_home_with_extra_config(
        r#"
[turns]
storage_mode = "off"
raw_turn_wings = ["hooks-raw"]
raw_turn_rooms = []
"#,
    );
    let oversized = vec![b'a'; 11 * 1024 * 1024];

    let output = run_hook(&home, "hook_user_prompt", &oversized);

    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(pending_message_count(&db_path), 0);
    assert!(
        !home.path().join(".mempal").join("hook-oversize").exists(),
        "off-mode raw turn capture must not write oversize files"
    );
}
