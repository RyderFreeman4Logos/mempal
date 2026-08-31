use super::*;
use crate::core::{db::Database, queue::PendingMessageStore};

struct ShutdownResetGuard;

impl Drop for ShutdownResetGuard {
    fn drop(&mut self) {
        reset_shutdown_request();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn write_stall_records_fault_without_requesting_shutdown() {
    let _shutdown_lock = global_shutdown_test_lock().lock_owned().await;
    let _shutdown_guard = ShutdownResetGuard;
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let sync_store = PendingMessageStore::new(&db_path).expect("open queue");
    sync_store
        .enqueue("hook:user-prompt-submit", "{}")
        .expect("enqueue pending message");
    let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
    observer.force_last_successful_write_for_test(0);
    let recovery = crate::daemon_recovery::DaemonRecovery::new(tmp.path());
    let watchdog = spawn_stall_watchdog(
        observer,
        AsyncPendingMessageStore::from_store(sync_store),
        Duration::from_millis(1),
        crate::daemon_recovery::DaemonRecoveryFaultReporter::new(recovery.clone()),
    );

    tokio::time::timeout(Duration::from_secs(1), watchdog)
        .await
        .expect("write-stall watchdog should finish")
        .expect("write-stall watchdog should not panic");
    assert!(
        !shutdown_requested(),
        "write-stall recovery must keep REST searches alive"
    );
    assert_eq!(
        recovery.snapshot().expect("read recovery state").last_fault,
        Some(crate::daemon_recovery::RecoveryFault::WriteStall)
    );
}
