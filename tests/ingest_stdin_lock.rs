use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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

[embed]
backend = "stub"

[ingest_gating]
enabled = false
"#,
            db_path.display()
        ),
    )
    .expect("write config");
    (tmp, db_path)
}

fn hold_sqlite_write_lock(db_path: PathBuf, hold_for: Duration) -> thread::JoinHandle<()> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let conn = Connection::open(db_path).expect("open sqlite lock connection");
        conn.execute_batch("BEGIN IMMEDIATE;")
            .expect("hold SQLite write lock");
        ready_tx.send(()).expect("signal SQLite lock ready");
        thread::sleep(hold_for);
        conn.execute_batch("ROLLBACK;")
            .expect("release SQLite lock");
    });
    ready_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("SQLite write lock ready");
    handle
}

#[test]
fn test_stdin_ingest_waits_for_transient_sqlite_lock() {
    let (home, db_path) = setup_home();
    let lock = hold_sqlite_write_lock(db_path.clone(), Duration::from_millis(300));
    let payload = br#"{"content":"stdin transient sqlite lock fixture"}"#;

    let mut child = Command::new(mempal_bin())
        .args([
            "ingest",
            "--stdin",
            "--wing",
            "mempal",
            "--source-type",
            "user_explicit",
            "--confidence",
            "0.9",
            "--no-gate",
            "--json",
        ])
        .env("HOME", home.path())
        .env("MEMPAL_EMBED_BACKEND", "stub")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn stdin ingest");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(payload)
        .expect("write stdin payload");
    let output = child.wait_with_output().expect("wait stdin ingest");
    lock.join().expect("lock thread");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("database is locked") && !stderr.contains("write admission"),
        "successful stdin ingest should not print lock diagnostics: {stderr}"
    );
    assert!(
        !stderr.contains("stdin transient sqlite lock fixture"),
        "stderr must not include raw stdin content: {stderr}"
    );
    let stdout: Value = serde_json::from_slice(&output.stdout).expect("stdout json");
    assert_eq!(stdout["stats"]["files"], 1);
    assert_eq!(stdout["stats"]["chunks"], 1);
    assert_eq!(
        stdout["drawer_ids"].as_array().expect("drawer ids").len(),
        1
    );

    let db = Database::open(&db_path).expect("open db after ingest");
    assert_eq!(db.drawer_count().expect("drawer count"), 1);
}
