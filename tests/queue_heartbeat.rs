use std::path::PathBuf;
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
    let store = PendingMessageStore::with_config(
        &db_path,
        QueueConfig {
            base_delay_ms: 5_000,
            max_delay_ms: 60_000,
            max_retries: 3,
        },
    )
    .expect("create store");
    (tmp, db_path, store)
}

#[test]
fn test_refresh_heartbeat_updates_claimed_row() {
    let (_tmp, db_path, store) = new_store();
    let id = store
        .enqueue("hook_event", r#"{"tool":"Bash"}"#)
        .expect("enqueue");
    store
        .claim_next("worker-a", 60)
        .expect("claim")
        .expect("message");

    store
        .refresh_heartbeat(&id, "worker-a")
        .expect("refresh heartbeat");

    let conn = Connection::open(db_path).expect("open sqlite");
    let heartbeat_at = conn
        .query_row(
            "SELECT heartbeat_at FROM pending_messages WHERE id = ?1",
            [&id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .expect("query heartbeat");

    assert!(heartbeat_at.unwrap_or_default() >= now_secs() - 5);
}

#[test]
fn test_reclaim_stale_rolls_back_on_heartbeat_silence() {
    let (_tmp, db_path, store) = new_store();
    let id = store
        .enqueue("hook_event", r#"{"tool":"Bash"}"#)
        .expect("enqueue");
    store
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

    let conn = Connection::open(db_path).expect("reopen sqlite");
    let (status, claimed_at, heartbeat_at, retry_count): (String, Option<i64>, Option<i64>, i64) =
        conn.query_row(
            "SELECT status, claimed_at, heartbeat_at, retry_count FROM pending_messages WHERE id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("query reclaimed row");

    assert_eq!(status, "pending");
    assert!(claimed_at.is_none());
    assert!(heartbeat_at.is_none());
    assert_eq!(retry_count, 0);
}

#[test]
fn test_reclaim_stale_preserves_heartbeating_claim() {
    let (_tmp, db_path, store) = new_store();
    let id = store
        .enqueue("hook_event", r#"{"tool":"Bash"}"#)
        .expect("enqueue");
    store
        .claim_next("worker-a", 60)
        .expect("claim")
        .expect("message");

    let conn = Connection::open(&db_path).expect("open sqlite");
    conn.execute(
        "UPDATE pending_messages SET claimed_at = ?2, heartbeat_at = ?3 WHERE id = ?1",
        rusqlite::params![id, now_secs() - 300, now_secs() - 3],
    )
    .expect("set heartbeat");
    drop(conn);

    let reclaimed = store.reclaim_stale(60).expect("reclaim");
    assert_eq!(reclaimed, 0);

    let conn = Connection::open(db_path).expect("reopen sqlite");
    let status = conn
        .query_row(
            "SELECT status FROM pending_messages WHERE id = ?1",
            [&id],
            |row| row.get::<_, String>(0),
        )
        .expect("query status");
    assert_eq!(status, "claimed");
}
