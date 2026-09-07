use super::*;
use crate::core::{db::Database, queue::PendingMessageStore};
use std::io::{self, Write};
use std::sync::{Arc, Mutex, PoisonError};

struct ClaimLogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for ClaimLogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn claim_log_text(logs: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(logs.lock().unwrap_or_else(PoisonError::into_inner).clone())
        .expect("tracing output must be UTF-8")
}

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

#[tokio::test(flavor = "current_thread")]
async fn claim_contention_event_identifies_victim_and_known_local_holder_once() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("initialize database");
    let sync_store = PendingMessageStore::new(&db_path).expect("open queue");
    sync_store
        .enqueue("hook:user-prompt-submit", "{}")
        .expect("enqueue pending message");
    let store = AsyncPendingMessageStore::from_store(sync_store);
    let holder = rusqlite::Connection::open(&db_path).expect("open holder connection");
    let owner =
        crate::core::writer_owner_diagnostics::begin_immediate(&holder, "blocking test writer")
            .expect("hold writer transaction");
    let evidence = store.writer_lock_evidence();
    assert_eq!(
        evidence.class,
        crate::core::writer_owner_diagnostics::WriterLockClass::HeldWriter
    );
    assert!(evidence.elapsed.is_some_and(|elapsed| !elapsed.is_zero()));

    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer_logs = Arc::clone(&logs);
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || ClaimLogWriter(Arc::clone(&writer_logs)))
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);
    let _guard = tracing::dispatcher::set_default(&dispatch);
    let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
    let mut backoff = ClaimBackoffState {
        write_observer: Some(observer.clone()),
        ..Default::default()
    };

    for _ in 0..2 {
        assert!(matches!(
            poll_claim_next(&store, "victim-worker", 60, &mut backoff, |_| Box::pin(
                std::future::ready(())
            ))
            .await,
            ClaimPollResult::RetryAfterError
        ));
        observer.record_queue_maintenance_success();
    }

    let log = claim_log_text(&logs);
    assert_eq!(
        log.matches("writer_evidence=Some(WriterLockEvidence")
            .count(),
        1,
        "owner evidence must be bounded by the claim backoff throttle: {log}"
    );
    assert!(log.contains("worker_id=\"victim-worker\""), "{log}");
    assert!(log.contains("class: HeldWriter"), "{log}");
    assert!(
        log.contains("operation: Some(\"blocking test writer\")"),
        "{log}"
    );
    assert!(log.contains("elapsed: Some("), "{log}");
    assert!(!log.contains(db_path.to_string_lossy().as_ref()), "{log}");
    assert!(!log.contains("BEGIN IMMEDIATE"), "{log}");

    holder.execute_batch("ROLLBACK").expect("release holder");
    owner.release();
    assert!(matches!(
        poll_claim_next(&store, "victim-worker", 60, &mut backoff, |_| Box::pin(
            std::future::ready(())
        ))
        .await,
        ClaimPollResult::Claimed(_)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn claim_contention_event_does_not_invent_an_untracked_holder() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("initialize database");
    let sync_store = PendingMessageStore::new(&db_path).expect("open queue");
    sync_store
        .enqueue("hook:user-prompt-submit", "{}")
        .expect("enqueue pending message");
    let store = AsyncPendingMessageStore::from_store(sync_store);
    let holder = rusqlite::Connection::open(&db_path).expect("open raw holder connection");
    holder
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold untracked writer transaction");
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer_logs = Arc::clone(&logs);
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || ClaimLogWriter(Arc::clone(&writer_logs)))
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);
    let _guard = tracing::dispatcher::set_default(&dispatch);
    let mut backoff = ClaimBackoffState {
        write_observer: Some(crate::daemon_bootstrap::DaemonWriteObserver::for_test()),
        ..Default::default()
    };

    assert!(matches!(
        poll_claim_next(&store, "unknown-holder-victim", 60, &mut backoff, |_| {
            Box::pin(std::future::ready(()))
        })
        .await,
        ClaimPollResult::RetryAfterError
    ));

    let log = claim_log_text(&logs);
    assert!(
        log.contains("writer_evidence=Some(WriterLockEvidence"),
        "{log}"
    );
    assert!(log.contains("class: NoTrackedLocalOwner"), "{log}");
    assert!(log.contains("owner_pid: None"), "{log}");
    assert!(log.contains("operation: None"), "{log}");
    holder
        .execute_batch("ROLLBACK")
        .expect("release raw holder");
}
