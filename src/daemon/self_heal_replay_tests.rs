use std::sync::Arc;
use std::time::Duration;

use crate::core::db::Database;
use crate::core::queue::{AsyncPendingMessageStore, PendingMessageStore};
use crate::hook::HookEvent;

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
