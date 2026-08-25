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
