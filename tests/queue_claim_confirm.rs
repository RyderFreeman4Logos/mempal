use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mempal::core::db::Database;
use mempal::core::queue::{
    LAST_ERROR_MAX_BYTES, PendingMessageStore, QueueConfig, QueueFailureDisposition,
};
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

    let op_state: String = Connection::open(db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT op_state FROM pending_messages WHERE id = ?1",
            [&id],
            |row| row.get::<_, String>(0),
        )
        .expect("read claim op_state");
    assert_eq!(op_state, "running");
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

    let remaining = Connection::open(&db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT COUNT(*) FROM pending_messages WHERE id = ?1",
            [&claimed.id],
            |row| row.get::<_, i64>(0),
        )
        .expect("count confirmed row");
    assert_eq!(remaining, 0);

    let completion_op_state: String = Connection::open(&db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT op_state FROM pending_message_completions WHERE message_id = ?1",
            [&claimed.id],
            |row| row.get::<_, String>(0),
        )
        .expect("read completion op_state");
    assert_eq!(completion_op_state, "completed");
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
    let (retry_count, retry_backoff_ms, next_attempt_at, status, op_state, last_error): (
        i64,
        i64,
        i64,
        String,
        String,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT retry_count, retry_backoff_ms, next_attempt_at, status, op_state, last_error FROM pending_messages WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .expect("read row");

    assert_eq!(retry_count, 1);
    assert!(retry_backoff_ms >= 5_000);
    assert!(next_attempt_at >= before + 5);
    assert!(next_attempt_at < before + 15);
    assert_eq!(status, "pending");
    assert_eq!(op_state, "queued");
    assert_eq!(last_error.as_deref(), Some("timeout"));
}

#[test]
fn test_retryable_failure_is_retried_not_dead_lettered_first() {
    let (_tmp, db_path, store) = new_store();
    let id = store.enqueue("hook_event", r#"{"n":1}"#).expect("enqueue");
    store
        .claim_next("worker-a", 60)
        .expect("claim")
        .expect("message");

    store
        .mark_failed_with_disposition(&id, "transport timeout", QueueFailureDisposition::Retryable)
        .expect("mark retryable failure");

    let (status, retry_count): (String, i64) = Connection::open(db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT status, retry_count FROM pending_messages WHERE id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read queue row");
    assert_eq!(status, "pending");
    assert_eq!(retry_count, 1);
}

#[test]
fn test_terminal_failure_dead_letters_immediately() {
    let (_tmp, db_path, store) = new_store();
    let id = store.enqueue("hook_event", r#"{"n":1}"#).expect("enqueue");
    store
        .claim_next("worker-a", 60)
        .expect("claim")
        .expect("message");

    store
        .mark_failed_with_disposition(
            &id,
            "invalid embedding input",
            QueueFailureDisposition::Terminal,
        )
        .expect("mark terminal failure");

    let (status, retry_count, next_attempt_at): (String, i64, i64) = Connection::open(&db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT status, retry_count, next_attempt_at FROM pending_messages WHERE id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read queue row");
    assert_eq!(status, "failed");
    assert_eq!(retry_count, 1);
    assert!(next_attempt_at <= now_secs());
    let op_state: String = Connection::open(&db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT op_state FROM pending_messages WHERE id = ?1",
            [&id],
            |row| row.get::<_, String>(0),
        )
        .expect("read terminal op_state");
    assert_eq!(op_state, "failed");
    assert!(
        store
            .claim_next("worker-b", 60)
            .expect("claim after terminal")
            .is_none()
    );
}

#[test]
fn test_retryable_failure_stays_retryable_past_max_retries() {
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
    let (status, retry_count): (String, i64) = conn
        .query_row(
            "SELECT status, retry_count FROM pending_messages WHERE id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query status");
    assert_eq!(status, "pending");
    assert_eq!(retry_count, 4);
    let retry = store
        .claim_next("worker-z", 60)
        .expect("claim after retryable failures")
        .expect("retryable message");
    assert_eq!(retry.id, id);
}

#[test]
fn test_claim_next_breaks_timestamp_ties_by_id() {
    let (_tmp, db_path, store) = new_store();
    let first = store
        .enqueue("hook_event", r#"{"n":1}"#)
        .expect("enqueue first");
    let second = store
        .enqueue("hook_event", r#"{"n":2}"#)
        .expect("enqueue second");
    let tied_at = now_secs().saturating_sub(10);

    let conn = Connection::open(&db_path).expect("open sqlite");
    conn.execute(
        "UPDATE pending_messages SET created_at = ?3, next_attempt_at = ?3 WHERE id IN (?1, ?2)",
        rusqlite::params![first, second, tied_at],
    )
    .expect("force timestamp tie");
    drop(conn);

    let claimed_first = store
        .claim_next("worker-first", 60)
        .expect("claim first")
        .expect("first tied item");
    let claimed_second = store
        .claim_next("worker-second", 60)
        .expect("claim second")
        .expect("second tied item");

    assert_eq!(claimed_first.id, first);
    assert_eq!(claimed_second.id, second);
}

#[test]
fn test_retry_failed_embed_messages_preserves_fifo_after_later_retry() {
    let (_tmp, db_path, store) = new_store();
    let failed_embed = store
        .enqueue("hook_event", r#"{"n":1}"#)
        .expect("enqueue failed embed");
    let pending_embed = store
        .enqueue("hook_event", r#"{"n":2}"#)
        .expect("enqueue pending embed");
    let base = now_secs().saturating_sub(10);

    let conn = Connection::open(&db_path).expect("open sqlite");
    conn.execute(
        "UPDATE pending_messages SET status = 'failed', retry_count = 7, retry_backoff_ms = 5000, last_error = 'boom', created_at = ?2, next_attempt_at = ?2 WHERE id = ?1",
        rusqlite::params![failed_embed, base],
    )
    .expect("mark failed embed with older queue position");
    conn.execute(
        "UPDATE pending_messages SET created_at = ?2, next_attempt_at = ?2 WHERE id = ?1",
        rusqlite::params![pending_embed, base + 1],
    )
    .expect("place pending embed after failed embed but before retry time");
    drop(conn);

    let retried = store
        .retry_failed_embed_messages()
        .expect("retry failed embed messages");
    assert_eq!(retried, 1);

    let recovered = store
        .claim_next("retry-worker", 60)
        .expect("claim requeued failed embed")
        .expect("requeued failed embed item");
    assert_eq!(recovered.id, failed_embed);
}

#[test]
fn test_retry_failed_embed_messages_requeues_only_failed_embed_items() {
    let (_tmp, db_path, store) = new_store();
    let failed_embed = store
        .enqueue("hook_event", r#"{"n":1}"#)
        .expect("enqueue failed embed");
    let failed_llm = store
        .enqueue("llm_task", r#"{"n":2}"#)
        .expect("enqueue failed llm");
    let pending_embed = store
        .enqueue("hook_event", r#"{"n":3}"#)
        .expect("enqueue pending embed");
    let claimed_embed = store
        .enqueue("hook_event", r#"{"n":4}"#)
        .expect("enqueue claimed embed");

    let conn = Connection::open(&db_path).expect("open sqlite");
    conn.execute(
        "UPDATE pending_messages SET status = 'failed', retry_count = 7, retry_backoff_ms = 5000, last_error = 'boom' WHERE id IN (?1, ?2)",
        rusqlite::params![failed_embed, failed_llm],
    )
    .expect("mark failed rows");
    conn.execute(
        "UPDATE pending_messages SET status = 'claimed', claim_token = 'worker:claim', claimed_at = ?2, heartbeat_at = ?2 WHERE id = ?1",
        rusqlite::params![claimed_embed, now_secs()],
    )
    .expect("mark claimed row");
    drop(conn);

    let retried = store
        .retry_failed_embed_messages()
        .expect("retry failed embed messages");
    assert_eq!(retried, 1);

    let conn = Connection::open(db_path).expect("open sqlite");
    let rows = [
        (&failed_embed, "pending", 0_i64, None),
        (&failed_llm, "failed", 7_i64, Some("boom")),
        (&pending_embed, "pending", 0_i64, None),
        (&claimed_embed, "claimed", 0_i64, None),
    ];
    for (id, expected_status, expected_retry_count, expected_error) in rows {
        let (status, retry_count, last_error): (String, i64, Option<String>) = conn
            .query_row(
                "SELECT status, retry_count, last_error FROM pending_messages WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read row");
        assert_eq!(status, expected_status, "{id}");
        assert_eq!(retry_count, expected_retry_count, "{id}");
        assert_eq!(last_error.as_deref(), expected_error, "{id}");
    }
    let op_state: String = conn
        .query_row(
            "SELECT op_state FROM pending_messages WHERE id = ?1",
            [&failed_embed],
            |row| row.get::<_, String>(0),
        )
        .expect("read retried op_state");
    assert_eq!(op_state, "queued");
    drop(conn);

    let recovered = store
        .claim_next("retry-worker", 60)
        .expect("claim requeued failed embed")
        .expect("requeued failed embed item");
    assert_eq!(recovered.id, failed_embed);
}

/// Verifies concurrent enqueue doesn't deadlock or starve.
///
/// The outer `timeout(5s)` acts as a hang guard -- if any task is starved by
/// SQLite lock contention the timeout trips. We intentionally avoid per-task
/// wall-clock assertions because those are inherently flaky under heavy CPU
/// load (parallel cargo builds, CI, etc.).
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
        });
    }

    barrier.wait().await;
    timeout(Duration::from_secs(5), async move {
        let mut completed = 0usize;
        while let Some(result) = join_set.join_next().await {
            result.expect("task result");
            completed += 1;
        }
        assert_eq!(completed, task_count);
    })
    .await
    .expect("concurrent enqueue timed out");

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
    let reclaimed = Connection::open(&db_path)
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
    let op_state: String = Connection::open(&db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT op_state FROM pending_messages WHERE id = ?1",
            [&id],
            |row| row.get::<_, String>(0),
        )
        .expect("read reclaimed op_state");
    assert_eq!(op_state, "queued");
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
