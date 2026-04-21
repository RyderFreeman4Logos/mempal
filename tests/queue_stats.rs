use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use mempal::core::db::{Database, apply_fork_ext_migrations_to};
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
fn test_fork_ext_migration_v0_to_v1_creates_pending_messages_table() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let conn = Connection::open(&db_path).expect("open sqlite");

    let upstream_user_version_before = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .expect("read initial user_version");
    assert_eq!(upstream_user_version_before, 0);

    apply_fork_ext_migrations_to(&conn, 1).expect("apply ext v1 migration");

    let fork_ext_version = conn
        .query_row(
            "SELECT value FROM fork_ext_meta WHERE key = 'fork_ext_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("read fork_ext_version");
    assert_eq!(fork_ext_version, "1");

    let upstream_user_version_after = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .expect("read final user_version");
    assert_eq!(upstream_user_version_after, 0);

    let table_exists = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='pending_messages'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query pending_messages table");
    assert_eq!(table_exists, 1);

    let index_exists = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_pending_next_attempt'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query pending_messages index");
    assert_eq!(index_exists, 1);
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
    assert_eq!(stats.failed, 0);
    assert_eq!(stats.oldest_pending_age_secs, None);
}

#[test]
fn test_status_command_shows_queue_stats() {
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
    assert!(stdout.contains("failed: 1"), "{stdout}");
    assert!(stdout.contains("oldest_pending_age_secs:"), "{stdout}");
}

#[test]
fn test_queue_module_no_unwrap() {
    let queue_source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/core/queue.rs");
    let content = fs::read_to_string(&queue_source).expect("read queue source");
    let offenders = content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.contains("// SAFETY:") && line.contains(".unwrap()"))
        .map(|(index, line)| format!("{}:{}", index + 1, line.trim()))
        .collect::<Vec<_>>();

    assert!(
        offenders.is_empty(),
        "queue module contains .unwrap():\n{}",
        offenders.join("\n")
    );
}
