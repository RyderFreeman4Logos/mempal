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
"#,
            db_path.display()
        ),
    )
    .expect("write config");
    (tmp, db_path)
}

fn run_hook(home: &TempDir, bytes: &[u8]) -> std::process::Output {
    let mut child = Command::new(mempal_bin())
        .args(["hook", "hook_user_prompt"])
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
        .write_all(bytes)
        .expect("write payload");
    child.wait_with_output().expect("wait output")
}

fn last_envelope_payload(db_path: &PathBuf) -> String {
    let conn = Connection::open(db_path).expect("open sqlite");
    let envelope: String = conn
        .query_row(
            "SELECT payload FROM pending_messages ORDER BY created_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("query queue");
    let envelope_json: Value = serde_json::from_str(&envelope).expect("envelope json");
    envelope_json["payload"]
        .as_str()
        .expect("payload string")
        .to_string()
}

#[test]
fn test_hook_reads_valid_utf8_without_replacement() {
    let (home, db_path) = setup_home();
    let payload = "继续处理 UTF-8 payload";

    let output = run_hook(&home, payload.as_bytes());
    assert_eq!(output.status.code(), Some(0));

    let stored = last_envelope_payload(&db_path);
    assert_eq!(stored, payload);
    assert!(
        !stored.contains('\u{fffd}'),
        "valid UTF-8 must not be lossy-decoded: {stored}"
    );
}

#[test]
fn test_hook_reads_invalid_utf8_with_replacement_char() {
    let (home, db_path) = setup_home();
    let payload = b"prefix\xffsuffix";

    let output = run_hook(&home, payload);
    assert_eq!(output.status.code(), Some(0));

    let stored = last_envelope_payload(&db_path);
    assert_eq!(stored, "prefix\u{fffd}suffix");
}
