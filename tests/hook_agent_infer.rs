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

#[test]
fn test_infer_agent_name_from_field_not_substring() {
    let (home, db_path) = setup_home();
    let payload =
        r#"{"agent":"claude","notes":"mention gpt-5-codex here should not flip inference"}"#;

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
        .write_all(payload.as_bytes())
        .expect("write payload");
    let output = child.wait_with_output().expect("wait output");
    assert_eq!(output.status.code(), Some(0));

    let conn = Connection::open(db_path).expect("open sqlite");
    let envelope: String = conn
        .query_row(
            "SELECT payload FROM pending_messages ORDER BY created_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("query queue");
    let envelope_json: Value = serde_json::from_str(&envelope).expect("envelope json");
    assert_eq!(envelope_json["agent"], "claude");
}
