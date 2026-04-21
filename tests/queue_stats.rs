use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use mempal::core::db::Database;
use mempal::core::queue::{PendingMessageStore, QueueConfig};
use rusqlite::{Connection, params};
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_secs() as i64
}

fn new_store(config: QueueConfig) -> (TempDir, PathBuf, PendingMessageStore) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let store = PendingMessageStore::with_config(&db_path, config).expect("create store");
    (tmp, db_path, store)
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
"#,
            db_path.display()
        ),
    )
    .expect("write config");
    (tmp, db_path)
}

#[test]
fn test_queue_stats_reflects_current_state() {
    let (_tmp, db_path, store) = new_store(QueueConfig {
        base_delay_ms: 0,
        max_delay_ms: 0,
        max_retries: 0,
    });

    let pending_id = store.enqueue("hook_event", r#"{"n":1}"#).expect("enqueue");
    let claimed_id = store.enqueue("hook_event", r#"{"n":2}"#).expect("enqueue");
    let done_id = store.enqueue("hook_event", r#"{"n":3}"#).expect("enqueue");
    let failed_id = store.enqueue("hook_event", r#"{"n":4}"#).expect("enqueue");

    let conn = Connection::open(&db_path).expect("open sqlite");
    conn.execute(
        "UPDATE pending_messages SET created_at = ?2, next_attempt_at = ?3 WHERE id = ?1",
        params![pending_id, now_secs() - 120, now_secs() + 3_600],
    )
    .expect("age pending row");

    let claimed = store
        .claim_next("worker-claimed", 60)
        .expect("claim")
        .expect("claimed row");
    assert_eq!(claimed.id, claimed_id);

    let done = store
        .claim_next("worker-done", 60)
        .expect("claim")
        .expect("done row");
    assert_eq!(done.id, done_id);
    store.confirm(&done.id).expect("confirm");

    let failed = store
        .claim_next("worker-failed", 60)
        .expect("claim")
        .expect("failed row");
    assert_eq!(failed.id, failed_id);
    store.mark_failed(&failed.id, "boom").expect("mark failed");

    let stats = store.stats().expect("stats");
    assert_eq!(stats.pending, 1);
    assert_eq!(stats.claimed, 1);
    assert_eq!(stats.done, 1);
    assert_eq!(stats.failed, 1);
    assert!(
        stats.oldest_pending_age_secs.is_some_and(|age| age >= 100),
        "{stats:?}"
    );

    let remaining_claimed = Connection::open(&db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT id FROM pending_messages WHERE status = 'claimed'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("claimed row id");
    assert_eq!(remaining_claimed, claimed_id);
}

#[test]
fn test_oldest_pending_age_none_when_empty() {
    let (_tmp, _db_path, store) = new_store(QueueConfig::default());

    let stats = store.stats().expect("stats");
    assert_eq!(stats.pending, 0);
    assert_eq!(stats.claimed, 0);
    assert_eq!(stats.done, 0);
    assert_eq!(stats.failed, 0);
    assert_eq!(stats.oldest_pending_age_secs, None);
}

#[test]
fn test_cli_status_prints_queue_section() {
    let (home, db_path) = setup_home();
    let store = PendingMessageStore::with_config(
        &db_path,
        QueueConfig {
            base_delay_ms: 0,
            max_delay_ms: 0,
            max_retries: 0,
        },
    )
    .expect("create store");

    let pending_id = store.enqueue("hook_event", r#"{"n":1}"#).expect("enqueue");
    let claimed_id = store.enqueue("hook_event", r#"{"n":2}"#).expect("enqueue");
    let done_id = store.enqueue("hook_event", r#"{"n":3}"#).expect("enqueue");
    let failed_id = store.enqueue("hook_event", r#"{"n":4}"#).expect("enqueue");

    Connection::open(&db_path)
        .expect("open sqlite")
        .execute(
            "UPDATE pending_messages SET created_at = ?2, next_attempt_at = ?3 WHERE id = ?1",
            params![pending_id, now_secs() - 90, now_secs() + 3_600],
        )
        .expect("age pending row");

    let claimed = store
        .claim_next("worker-claimed", 60)
        .expect("claim")
        .expect("claimed");
    assert_eq!(claimed.id, claimed_id);
    let done = store
        .claim_next("worker-done", 60)
        .expect("claim")
        .expect("done");
    assert_eq!(done.id, done_id);
    store.confirm(&done.id).expect("confirm");
    let failed = store
        .claim_next("worker-failed", 60)
        .expect("claim")
        .expect("failed");
    assert_eq!(failed.id, failed_id);
    store.mark_failed(&failed.id, "boom").expect("mark failed");

    let output = Command::new(mempal_bin())
        .arg("status")
        .env("HOME", home.path())
        .output()
        .expect("run mempal status");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("status stdout utf8");
    assert!(stdout.contains("Queue:"), "{stdout}");
    assert!(stdout.contains("pending: 1"), "{stdout}");
    assert!(stdout.contains("claimed: 1"), "{stdout}");
    assert!(stdout.contains("done: 1"), "{stdout}");
    assert!(stdout.contains("failed: 1"), "{stdout}");
    assert!(stdout.contains("oldest_pending_age_secs:"), "{stdout}");
}
