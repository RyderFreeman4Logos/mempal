use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use mempal::core::db::Database;
use mempal::core::queue::{PendingMessageStore, QueueConfig};
use rusqlite::Connection;
use tempfile::TempDir;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_secs() as i64
}

fn new_store() -> (TempDir, PathBuf, PendingMessageStore) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let store = PendingMessageStore::new(&db_path).expect("create store");
    (tmp, db_path, store)
}

#[test]
fn test_fork_ext_migration_v0_to_v5_preserves_pending_messages_table() {
    let (_tmp, db_path, _store) = new_store();
    let conn = Connection::open(db_path).expect("open sqlite");

    let version = conn
        .query_row(
            "SELECT value FROM fork_ext_meta WHERE key = 'fork_ext_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("read fork_ext_version");
    assert_eq!(version, "5");

    let exists = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='pending_messages'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query sqlite_master");
    assert_eq!(exists, 1);

    let index_exists = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_pending_next_attempt'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query sqlite_master");
    assert_eq!(index_exists, 1);
}

#[test]
fn test_enqueue_claim_confirm_basic() {
    let (_tmp, db_path, store) = new_store();

    let id = store
        .enqueue("hook_event", r#"{"tool":"Bash"}"#)
        .expect("enqueue");
    let claimed = store
        .claim_next("worker-1", 60)
        .expect("claim")
        .expect("message");

    assert_eq!(claimed.id, id);
    assert_eq!(claimed.kind, "hook_event");
    assert_eq!(claimed.payload, r#"{"tool":"Bash"}"#);
    assert_eq!(claimed.retry_count, 0);

    store.confirm(&claimed.id).expect("confirm");
    let stats = store.stats().expect("stats");
    assert_eq!(stats.pending, 0);
    assert_eq!(stats.claimed, 0);
    assert_eq!(stats.failed, 0);

    let remaining = Connection::open(db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT COUNT(*) FROM pending_messages WHERE id = ?1",
            [&claimed.id],
            |row| row.get::<_, i64>(0),
        )
        .expect("count confirmed row");
    assert_eq!(remaining, 0);
}

#[test]
fn test_claim_is_exclusive() {
    let (_tmp, _db_path, store) = new_store();
    store.enqueue("hook_event", r#"{"n":1}"#).expect("enqueue");

    let first = store.claim_next("worker-a", 60).expect("first claim");
    let second = store.claim_next("worker-b", 60).expect("second claim");

    assert!(first.is_some());
    assert!(second.is_none());
}

#[test]
fn test_mark_failed_sets_backoff_next_attempt() {
    let (_tmp, db_path, store) = new_store();
    let id = store.enqueue("hook_event", r#"{"n":1}"#).expect("enqueue");
    let claimed = store
        .claim_next("worker-a", 60)
        .expect("claim")
        .expect("message");
    assert_eq!(claimed.id, id);

    let before = now_secs();
    store.mark_failed(&id, "timeout").expect("mark failed");

    let conn = Connection::open(db_path).expect("open sqlite");
    let (retry_count, retry_backoff_ms, next_attempt_at, status, last_error): (
        i64,
        i64,
        i64,
        String,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT retry_count, retry_backoff_ms, next_attempt_at, status, last_error FROM pending_messages WHERE id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .expect("read row");

    assert_eq!(retry_count, 1);
    assert!(retry_backoff_ms >= 5_000);
    assert!(next_attempt_at >= before + 5);
    assert!(next_attempt_at < before + 15);
    assert_eq!(status, "pending");
    assert_eq!(last_error.as_deref(), Some("timeout"));
}

#[test]
fn test_max_retries_marks_failed_permanently() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let store = PendingMessageStore::with_config(
        &db_path,
        QueueConfig {
            base_delay_ms: 0,
            max_delay_ms: 0,
            max_retries: 3,
        },
    )
    .expect("store");

    let id = store.enqueue("hook_event", r#"{"n":1}"#).expect("enqueue");
    for worker in ["worker-a", "worker-b", "worker-c", "worker-d"] {
        let claimed = store
            .claim_next(worker, 60)
            .expect("claim")
            .expect("message");
        assert_eq!(claimed.id, id);
        store.mark_failed(&id, "timeout").expect("mark failed");
    }

    let conn = Connection::open(&db_path).expect("open sqlite");
    let status = conn
        .query_row(
            "SELECT status FROM pending_messages WHERE id = ?1",
            [&id],
            |row| row.get::<_, String>(0),
        )
        .expect("query status");
    assert_eq!(status, "failed");
    assert!(
        store
            .claim_next("worker-z", 60)
            .expect("claim after failed")
            .is_none()
    );
}

#[test]
fn test_concurrent_claim_winner_takes_all() {
    let (_tmp, _db_path, store) = new_store();
    store.enqueue("hook_event", r#"{"n":1}"#).expect("enqueue");

    let shared = Arc::new(store);
    let barrier = Arc::new(Barrier::new(3));
    let store_a = Arc::clone(&shared);
    let store_b = Arc::clone(&shared);
    let barrier_a = Arc::clone(&barrier);
    let barrier_b = Arc::clone(&barrier);

    let handle_a = thread::spawn(move || {
        barrier_a.wait();
        store_a.claim_next("worker-a", 60).expect("claim a")
    });
    let handle_b = thread::spawn(move || {
        barrier_b.wait();
        store_b.claim_next("worker-b", 60).expect("claim b")
    });
    barrier.wait();

    let a = handle_a.join().expect("join a");
    let b = handle_b.join().expect("join b");
    let winners = [a.is_some(), b.is_some()]
        .into_iter()
        .filter(|won| *won)
        .count();
    assert_eq!(winners, 1);
}

#[test]
fn test_crash_recovery_reclaims_and_reissues_claim() {
    let (_tmp, db_path, store) = new_store();
    let id = store.enqueue("hook_event", r#"{"n":1}"#).expect("enqueue");
    let _claimed = store
        .claim_next("worker-a", 60)
        .expect("claim")
        .expect("message");

    let conn = Connection::open(&db_path).expect("open sqlite");
    conn.execute(
        "UPDATE pending_messages SET heartbeat_at = ?2, claimed_at = ?2 WHERE id = ?1",
        rusqlite::params![id, now_secs() - 120],
    )
    .expect("age heartbeat");

    drop(conn);
    let reclaimed = store.reclaim_stale(60).expect("reclaim");
    assert_eq!(reclaimed, 1);

    let reclaimed_msg = store
        .claim_next("worker-b", 60)
        .expect("claim again")
        .expect("message");
    assert_eq!(reclaimed_msg.id, id);
}
