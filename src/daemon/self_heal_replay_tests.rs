use std::sync::Arc;
use std::time::Duration;

use crate::core::db::Database;
use crate::core::queue::{AsyncPendingMessageStore, PendingMessageStore, QueueError};
use crate::hook::HookEvent;
use crate::ingress_spool::IngressSpoolError;

struct ShutdownResetGuard;

impl ShutdownResetGuard {
    fn new() -> Self {
        super::super::reset_shutdown_request();
        Self
    }
}

impl Drop for ShutdownResetGuard {
    fn drop(&mut self) {
        super::super::reset_shutdown_request();
    }
}

#[tokio::test]
async fn spool_replay_preserves_sqlite_contention_for_watchdog() {
    let _shutdown_guard = super::super::global_shutdown_test_lock().lock_owned().await;
    let _reset_guard = ShutdownResetGuard::new();
    let tmp = tempfile::TempDir::new_in("/tmp").expect("short tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    PendingMessageStore::new(&db_path)
        .expect("open queue")
        .enqueue("existing-work", "{}")
        .expect("enqueue pending work");
    let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    let request = crate::hook_ipc::HookIpcEnqueueRequest::new(
        HookEvent::UserPromptSubmit.queue_kind(),
        r#"{"event":"replay contention"}"#,
    );
    let spool = Arc::new(crate::ingress_spool::IngressSpool::new(tmp.path()));
    spool.append(&request).expect("append spool record");
    let lock_conn = rusqlite::Connection::open(&db_path).expect("open lock connection");
    lock_conn
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("hold SQLite write lock");

    let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
    observer.force_last_successful_write_for_test(0);
    let listener =
        tokio::net::UnixListener::bind(tmp.path().join("hook.sock")).expect("bind hook listener");
    let listener_task = tokio::spawn(super::run_hook_ipc_listener(
        listener,
        store.clone(),
        observer.clone(),
        spool,
    ));
    let observed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(error) = observer.last_error_for_test() {
                break error;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("spool replay should observe the locked SQLite error");
    assert!(observed.1, "replay must preserve the SQLite lock class");

    super::super::request_shutdown();
    tokio::time::timeout(Duration::from_secs(5), listener_task)
        .await
        .expect("listener should stop after replay observation")
        .expect("listener task should not panic");
    lock_conn.execute_batch("ROLLBACK;").expect("release lock");
    super::super::reset_shutdown_request();

    assert!(!observer.maybe_log_stall(&store).await);
    assert!(!super::super::SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(
        observer
            .last_error_for_test()
            .map(|(_, is_sqlite_lock)| is_sqlite_lock),
        Some(true),
    );
}

#[tokio::test]
async fn post_publish_parent_sync_failure_then_fallback_replay_yields_one_queue_row() {
    let tmp = tempfile::TempDir::new_in("/tmp").expect("short tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
    let spool = Arc::new(crate::ingress_spool::IngressSpool::new(tmp.path()));
    let request = crate::hook_ipc::HookIpcEnqueueRequest::new(
        HookEvent::UserPromptSubmit.queue_kind(),
        r#"{"event":"UserPromptSubmit","payload":"post-publish sync"}"#,
    );
    let original_key = request.idempotency_key.clone();
    let _fault = crate::ingress_spool::fail_next_parent_namespace_sync_for(&spool);

    let persist_response =
        super::persist_hook_ipc_request(&store, &spool, &observer, request.clone()).await;
    let crate::hook_ipc::HookIpcEnqueueResponse::Error { message } = persist_response else {
        panic!("post-rename parent sync failure must surface as a persist error");
    };
    let fallback = crate::hook_ipc::HookIpcFallbackReason::Rejected(message);
    let fallback_store = PendingMessageStore::new(&db_path).expect("open fallback queue");
    if fallback.may_have_reached_daemon() {
        fallback_store
            .enqueue_idempotent_with_key(
                HookEvent::UserPromptSubmit.queue_kind(),
                &request.payload,
                &original_key,
            )
            .expect("idempotent fallback");
    } else {
        fallback_store
            .enqueue(HookEvent::UserPromptSubmit.queue_kind(), &request.payload)
            .expect("fresh fallback");
    }

    assert_eq!(
        spool
            .drain_once(&store)
            .await
            .expect("replay published record"),
        1
    );

    let count: i64 = rusqlite::Connection::open(&db_path)
        .expect("open sqlite")
        .query_row("SELECT COUNT(*) FROM pending_messages", [], |row| {
            row.get(0)
        })
        .expect("count pending");
    assert_eq!(
        count, 1,
        "fallback plus spool replay must collapse to one row"
    );
}

#[tokio::test]
async fn sigkill_equivalent_after_durable_ack_replays_once_after_duplicate_retry() {
    let tmp = tempfile::TempDir::new_in("/tmp").expect("short tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
    let request = crate::hook_ipc::HookIpcEnqueueRequest::new(
        HookEvent::UserPromptSubmit.queue_kind(),
        r#"{"event":"UserPromptSubmit","payload":"sigkill-after-ack"}"#,
    );

    let acknowledged = Arc::new(crate::ingress_spool::IngressSpool::new(tmp.path()));
    assert_eq!(
        super::persist_hook_ipc_request(&store, &acknowledged, &observer, request.clone()).await,
        crate::hook_ipc::HookIpcEnqueueResponse::Accepted,
        "the daemon must ACK only after the ingress record is durable"
    );
    drop(acknowledged);

    let restarted = Arc::new(crate::ingress_spool::IngressSpool::new(tmp.path()));
    assert_eq!(
        restarted
            .drain_once(&store)
            .await
            .expect("replay after abort"),
        1,
        "a fresh process must replay the acknowledged record"
    );
    assert_eq!(
        super::persist_hook_ipc_request(&store, &restarted, &observer, request.clone()).await,
        crate::hook_ipc::HookIpcEnqueueResponse::Accepted,
        "the duplicate client retry must retain its idempotency key"
    );
    assert_eq!(
        restarted
            .drain_once(&store)
            .await
            .expect("replay duplicate retry"),
        1
    );

    let count: i64 = rusqlite::Connection::open(&db_path)
        .expect("open sqlite")
        .query_row("SELECT COUNT(*) FROM pending_messages", [], |row| {
            row.get(0)
        })
        .expect("count visible queue events");
    assert_eq!(count, 1, "abort replay and retry must remain exactly once");
}

fn plant_orphan_claim(mempal_home: &std::path::Path) {
    let dir = mempal_home.join(crate::ingress_spool::INGRESS_SPOOL_DIR);
    let mut planted = false;
    for entry in std::fs::read_dir(&dir).expect("spool dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        std::fs::rename(&path, path.with_extension("claim")).expect("plant leftover claim");
        planted = true;
    }
    assert!(planted, "durable ACK must leave a json record to claim");
}

#[tokio::test]
async fn leftover_claim_after_mid_drain_abort_replays_once_after_duplicate_retry() {
    let tmp = tempfile::TempDir::new_in("/tmp").expect("short tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
    let request = crate::hook_ipc::HookIpcEnqueueRequest::new(
        HookEvent::UserPromptSubmit.queue_kind(),
        r#"{"event":"UserPromptSubmit","payload":"leftover-claim-after-ack"}"#,
    );

    let acknowledged = Arc::new(crate::ingress_spool::IngressSpool::new(tmp.path()));
    assert_eq!(
        super::persist_hook_ipc_request(&store, &acknowledged, &observer, request.clone()).await,
        crate::hook_ipc::HookIpcEnqueueResponse::Accepted,
        "the daemon must ACK only after the ingress record is durable"
    );
    plant_orphan_claim(tmp.path());
    drop(acknowledged);

    let restarted = Arc::new(crate::ingress_spool::IngressSpool::new(tmp.path()));
    assert_eq!(
        restarted
            .drain_once(&store)
            .await
            .expect("replay leftover claim"),
        1,
        "a fresh process must recover and replay the leftover claim"
    );
    assert_eq!(
        super::persist_hook_ipc_request(&store, &restarted, &observer, request.clone()).await,
        crate::hook_ipc::HookIpcEnqueueResponse::Accepted,
        "the duplicate client retry must retain its idempotency key"
    );
    assert_eq!(
        restarted
            .drain_once(&store)
            .await
            .expect("replay duplicate retry"),
        1
    );

    let count: i64 = rusqlite::Connection::open(&db_path)
        .expect("open sqlite")
        .query_row("SELECT COUNT(*) FROM pending_messages", [], |row| {
            row.get(0)
        })
        .expect("count visible queue events");
    assert_eq!(
        count, 1,
        "recovered claim replay and retry must remain exactly once"
    );
}

fn spool_json_and_claim_counts(mempal_home: &std::path::Path) -> (usize, usize) {
    let dir = mempal_home.join(crate::ingress_spool::INGRESS_SPOOL_DIR);
    let mut json = 0;
    let mut claims = 0;
    for entry in std::fs::read_dir(&dir).expect("spool dir") {
        match entry
            .expect("entry")
            .path()
            .extension()
            .and_then(|value| value.to_str())
        {
            Some("json") => json += 1,
            Some("claim") => claims += 1,
            _ => {}
        }
    }
    (json, claims)
}

fn pending_row_count(db_path: &std::path::Path) -> i64 {
    rusqlite::Connection::open(db_path)
        .expect("open sqlite")
        .query_row("SELECT COUNT(*) FROM pending_messages", [], |row| {
            row.get(0)
        })
        .expect("count pending")
}

#[tokio::test]
async fn stale_writer_lease_releases_claimed_spool_record_for_takeover_replay() {
    let tmp = tempfile::TempDir::new_in("/tmp").expect("short tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    let request = crate::hook_ipc::HookIpcEnqueueRequest::new(
        HookEvent::UserPromptSubmit.queue_kind(),
        r#"{"event":"UserPromptSubmit","payload":"stale-lease-replay"}"#,
    );
    let spool = crate::ingress_spool::IngressSpool::new(tmp.path());
    spool.append(&request).expect("plant durable spool record");

    let stale = db
        .runtime_writer_lease_acquire("sqlite-writer", "old", "daemon", 300, None)
        .expect("acquire old lease")
        .expect("old lease available");
    assert!(
        db.runtime_writer_lease_release(&stale)
            .expect("release old generation")
    );
    let current = db
        .runtime_writer_lease_acquire("sqlite-writer", "new", "daemon", 300, None)
        .expect("acquire current lease")
        .expect("current lease available");

    let error = spool
        .drain_once_fenced(&store, &stale)
        .await
        .expect_err("replaced generation must not commit spool replay");
    assert!(matches!(
        error,
        IngressSpoolError::Queue(QueueError::RuntimeWriterLeaseLost { generation, .. })
            if generation == stale.generation
    ));
    assert_eq!(
        pending_row_count(&db_path),
        0,
        "stale drain must not insert"
    );
    assert_eq!(
        spool_json_and_claim_counts(tmp.path()),
        (1, 0),
        "lease loss must release the claim back to replayable json"
    );

    assert_eq!(
        spool
            .drain_once_fenced(&store, &current)
            .await
            .expect("current generation may replay"),
        1
    );
    assert_eq!(pending_row_count(&db_path), 1);
    assert_eq!(spool_json_and_claim_counts(tmp.path()), (0, 0));

    spool
        .append(&request)
        .expect("duplicate producer retry after takeover");
    assert_eq!(
        spool
            .drain_once_fenced(&store, &current)
            .await
            .expect("duplicate retry drain"),
        1
    );
    assert_eq!(
        pending_row_count(&db_path),
        1,
        "duplicate producer retry must remain exactly once"
    );
}
