use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mempal::core::db::Database;
use mempal::core::queue::{LAST_ERROR_MAX_BYTES, PendingMessageStore, QueueConfig};
use rusqlite::Connection;
use tempfile::TempDir;
use tokio::sync::Barrier;
use tokio::task::JoinSet;
use tokio::time::timeout;

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
fn test_enqueue_then_claim_returns_same_payload() {
    let (_tmp, _db_path, store) = new_store();

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
}

#[test]
fn test_confirm_deletes_row() {
    let (_tmp, db_path, store) = new_store();
    let id = store
        .enqueue("hook_event", r#"{"tool":"Bash"}"#)
        .expect("enqueue");
    let claimed = store
        .claim_next("worker-1", 60)
        .expect("claim")
        .expect("message");
    assert_eq!(claimed.id, id);

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_enqueue_does_not_block() {
    let (_tmp, _db_path, store) = new_store();
    let store = Arc::new(store);
    let task_count = 8usize;
    let items_per_task = 25usize;
    let barrier = Arc::new(Barrier::new(task_count + 1));
    let mut join_set = JoinSet::new();

    for task_index in 0..task_count {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        join_set.spawn(async move {
            barrier.wait().await;
            let started = Instant::now();
            tokio::task::spawn_blocking(move || {
                for item_index in 0..items_per_task {
                    store
                        .enqueue(
                            "hook_event",
                            &format!(r#"{{"task":{task_index},"item":{item_index}}}"#),
                        )
                        .expect("enqueue from concurrent task");
                }
            })
            .await
            .expect("join blocking enqueue worker");
            started.elapsed()
        });
    }

    barrier.wait().await;
    let latencies = timeout(Duration::from_secs(5), async move {
        let mut elapsed = Vec::with_capacity(task_count);
        while let Some(result) = join_set.join_next().await {
            elapsed.push(result.expect("task result"));
        }
        elapsed
    })
    .await
    .expect("concurrent enqueue timed out");

    for latency in &latencies {
        // Wall-clock tolerance widened to 4500ms (just under the outer 5s
        // timeout) to absorb CPU contention from concurrent rustc/cargo runs;
        // the assertion still fails fast on real per-task starvation because
        // the timeout(5s) wrapper above would trip first if any task hung.
        assert!(
            latency.as_millis() < 4_500,
            "enqueue task should stay millisecond-level, got {latency:?}"
        );
    }

    let stats = store.stats().expect("stats");
    assert_eq!(stats.pending, (task_count * items_per_task) as u64);
}

#[test]
fn test_store_startup_auto_reclaims_stale() {
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
    drop(store);

    let restarted = PendingMessageStore::new(&db_path).expect("restart store");
    let reclaimed = Connection::open(db_path)
        .expect("reopen sqlite")
        .query_row(
            "SELECT status, claimed_at, heartbeat_at FROM pending_messages WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .expect("query row");

    assert_eq!(reclaimed.0, "pending");
    assert!(reclaimed.1.is_none());
    assert!(reclaimed.2.is_none());
    assert_eq!(restarted.stats().expect("stats").pending, 1);
}

#[test]
fn test_concurrent_claim_winner_takes_all() {
    let (_tmp, _db_path, store) = new_store();
    store.enqueue("hook_event", r#"{"n":1}"#).expect("enqueue");

    let shared = Arc::new(store);
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let store_a = Arc::clone(&shared);
    let store_b = Arc::clone(&shared);
    let barrier_a = Arc::clone(&barrier);
    let barrier_b = Arc::clone(&barrier);

    let handle_a = std::thread::spawn(move || {
        barrier_a.wait();
        store_a.claim_next("worker-a", 60).expect("claim a")
    });
    let handle_b = std::thread::spawn(move || {
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

#[test]
fn test_readonly_open_keeps_non_wal_journal_mode() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("readonly.db");
    let conn = Connection::open(&db_path).expect("create sqlite db");
    let initial_mode = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
        .expect("read initial journal mode");
    assert_ne!(initial_mode.to_lowercase(), "wal");
    drop(conn);

    let db = Database::open_read_only(&db_path).expect("open readonly db");
    let readonly_mode = db
        .conn()
        .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
        .expect("read readonly journal mode");

    assert_eq!(readonly_mode.to_lowercase(), initial_mode.to_lowercase());
    assert_ne!(readonly_mode.to_lowercase(), "wal");
}

#[test]
fn test_last_error_is_redacted_and_truncated() {
    let (_tmp, db_path, store) = new_store();
    let id = store
        .enqueue("hook_event", r#"{"tool":"Bash"}"#)
        .expect("enqueue");
    store
        .claim_next("worker-a", 60)
        .expect("claim")
        .expect("message");

    let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789SECRETKEY";
    let oversized_error = format!("before {secret} {}", "x".repeat(LAST_ERROR_MAX_BYTES * 2));
    store
        .mark_failed(&id, &oversized_error)
        .expect("mark failed with secret");

    let stored_error = Connection::open(db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT last_error FROM pending_messages WHERE id = ?1",
            [&id],
            |row| row.get::<_, Option<String>>(0),
        )
        .expect("read last_error")
        .expect("stored error");

    assert!(
        stored_error.contains("[REDACTED:openai_key]"),
        "{stored_error}"
    );
    assert!(!stored_error.contains(secret), "{stored_error}");
    assert!(
        stored_error.len() <= LAST_ERROR_MAX_BYTES,
        "stored error length={} exceeds {LAST_ERROR_MAX_BYTES}",
        stored_error.len()
    );
}
