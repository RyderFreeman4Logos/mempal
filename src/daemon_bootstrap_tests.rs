use super::*;
use crate::core::queue::{PendingMessageStore, QueueError};

#[test]
fn cooldown_refusal_keeps_the_temporary_exit_status() {
    let error = anyhow::Error::new(DaemonCooldownRequired::new(1));
    assert_eq!(
        temporary_refusal_exit_status(&error),
        Some(DAEMON_TEMPORARY_ADMISSION_REFUSAL_EXIT_STATUS)
    );
}

#[test]
fn writer_lease_refusal_keeps_the_temporary_exit_status() {
    let error = anyhow::Error::new(DaemonWriterLeaseHeld::new("mode=mcp pid=42 owner=test"));
    assert_eq!(
        temporary_refusal_exit_status(&error),
        Some(DAEMON_TEMPORARY_ADMISSION_REFUSAL_EXIT_STATUS)
    );
}

fn sqlite_protocol_queue_error() -> QueueError {
    QueueError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::FileLockingProtocolFailed,
            extended_code: rusqlite::ffi::SQLITE_PROTOCOL,
        },
        Some("locking protocol conflict".to_string()),
    ))
}

#[tokio::test]
async fn write_observer_reports_stall_when_queue_has_work_and_no_recent_writes() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let sync_store = PendingMessageStore::new(&db_path).expect("open queue");
    sync_store
        .enqueue("hook:user-prompt-submit", "{}")
        .expect("enqueue pending message");
    let store = AsyncPendingMessageStore::from_store(sync_store);

    let observer = DaemonWriteObserver::new();
    let now = unix_secs();
    observer.force_last_successful_write_for_test(now.saturating_sub(DAEMON_STALL_SECONDS));
    observer.record_error("failed to merge drawer");

    let diagnostic = observer
        .stall_diagnostic(&store, now)
        .await
        .expect("stall diagnostic");
    assert_eq!(diagnostic.queued_count, 1);
    assert!(diagnostic.seconds_since_successful_write >= DAEMON_STALL_SECONDS);
    assert_eq!(diagnostic.last_error, "failed to merge drawer");
    assert!(observer.maybe_log_stall(&store).await);
    assert!(
        !observer.maybe_log_stall(&store).await,
        "one stalled generation must emit only one recovery signal per throttle window"
    );
}

#[tokio::test]
async fn write_observer_requests_recovery_after_success_invalidates_lock_error() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let sync_store = PendingMessageStore::new(&db_path).expect("open queue");
    sync_store
        .enqueue("hook:user-prompt-submit", "{}")
        .expect("enqueue pending message");
    let store = AsyncPendingMessageStore::from_store(sync_store);

    let observer = DaemonWriteObserver::new();
    observer.record_claim_error(&sqlite_protocol_queue_error());
    observer.record_successful_write();
    observer.force_last_successful_write_for_test(unix_secs().saturating_sub(DAEMON_STALL_SECONDS));

    assert!(
        observer.maybe_log_stall(&store).await,
        "a successful write must invalidate historical SQLite contention"
    );
}

#[tokio::test]
async fn write_observer_does_not_restart_for_typed_sqlite_protocol_stall() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let sync_store = PendingMessageStore::new(&db_path).expect("open queue");
    sync_store
        .enqueue("hook:user-prompt-submit", "{}")
        .expect("enqueue pending message");
    let store = AsyncPendingMessageStore::from_store(sync_store);

    let observer = DaemonWriteObserver::new();
    observer.force_last_successful_write_for_test(unix_secs().saturating_sub(DAEMON_STALL_SECONDS));
    observer.record_claim_error(&sqlite_protocol_queue_error());

    assert!(
        !observer.maybe_log_stall(&store).await,
        "transient SQLite pressure must keep the daemon alive so workers can recover"
    );
}

#[tokio::test]
async fn hook_heartbeat_result_observation_preserves_semantic_stall_clock() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let sync_store = PendingMessageStore::new(&db_path).expect("open queue");
    sync_store
        .enqueue("hook:user-prompt-submit", "{}")
        .expect("enqueue pending message");
    let store =
        AsyncPendingMessageStore::from_store(sync_store).with_heartbeat_lock_failures_for_test(1);

    let claimed = store
        .claim_next("hook-worker".to_string(), 30)
        .await
        .expect("claim")
        .expect("claimed message");

    let observer = DaemonWriteObserver::new();
    observer.force_last_successful_write_for_test(unix_secs().saturating_sub(DAEMON_STALL_SECONDS));

    crate::daemon::refresh_hook_message_heartbeat(&store, &claimed.id, "hook-worker", &observer)
        .await;
    assert!(
        !observer.maybe_log_stall(&store).await,
        "fresh heartbeat FileLockingProtocolFailed contention must suppress false stall recovery without a subsequent claim error"
    );

    crate::daemon::refresh_hook_message_heartbeat(&store, &claimed.id, "hook-worker", &observer)
        .await;
    let diagnostic = observer
        .stall_diagnostic(&store, unix_secs())
        .await
        .expect("maintenance success must not advance the semantic stall clock");
    assert_eq!(diagnostic.last_error, "none recorded");
    assert!(diagnostic.seconds_since_successful_write >= DAEMON_STALL_SECONDS);

    observer.record_successful_write();
    assert_eq!(observer.stall_diagnostic(&store, unix_secs()).await, None);
}

#[tokio::test]
async fn write_observer_record_queue_error_non_lock_still_allows_recovery() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let sync_store = PendingMessageStore::new(&db_path).expect("open queue");
    sync_store
        .enqueue("hook:user-prompt-submit", "{}")
        .expect("enqueue pending message");
    let store = AsyncPendingMessageStore::from_store(sync_store);

    let observer = DaemonWriteObserver::new();
    observer.force_last_successful_write_for_test(unix_secs().saturating_sub(DAEMON_STALL_SECONDS));
    observer.record_queue_error(
        "failed to confirm msg-1",
        &QueueError::MessageNotFound("msg-1".to_string()),
    );

    assert!(
        observer.maybe_log_stall(&store).await,
        "typed non-lock queue mutation errors must remain fail-closed for stall recovery"
    );
}

#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn daemon_ingest_claim_contention_suppresses_stall_restart() {
    crate::observability::reset_ingest_worker_backoff_for_tests();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("initialize database");
    let sync_store = PendingMessageStore::new(&db_path).expect("open queue");
    sync_store
        .enqueue("ingest_async", "{}")
        .expect("enqueue async ingest");
    let store =
        AsyncPendingMessageStore::from_store(sync_store).with_claim_lock_failures_for_test(1);
    let observer = DaemonWriteObserver::new();
    observer.force_last_successful_write_for_test(unix_secs().saturating_sub(DAEMON_STALL_SECONDS));
    let server = crate::mcp::MempalMcpServer::new(db_path, crate::core::config::Config::default())
        .expect("create daemon-scoped ingest server")
        .with_async_queue_for_test(store.clone())
        .with_daemon_write_observer(observer.clone());
    let worker = server.spawn_scoped_ingest_drain_worker();

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let snapshot = crate::observability::ingest_worker_backoff_snapshot();
        if snapshot.retry_count == 1
            && snapshot.last_error_class.as_deref() == Some("sqlite_locked")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for daemon ingest claim contention"
        );
        tokio::task::yield_now().await;
    }

    assert!(
        !observer.maybe_log_stall(&store).await,
        "current daemon ingest claim contention must keep retrying instead of restarting"
    );
    worker.shutdown_and_drain().await;
}

#[tokio::test]
async fn write_observer_requests_recovery_after_sqlite_contention_expires() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let sync_store = PendingMessageStore::new(&db_path).expect("open queue");
    sync_store
        .enqueue("hook:user-prompt-submit", "{}")
        .expect("enqueue pending message");
    let store = AsyncPendingMessageStore::from_store(sync_store);

    let observer = DaemonWriteObserver::new();
    let now = unix_secs();
    observer.force_last_successful_write_for_test(now.saturating_sub(DAEMON_STALL_SECONDS));
    observer.record_claim_error(&sqlite_protocol_queue_error());
    observer.force_last_error_observed_at_for_test(
        now.saturating_sub(DAEMON_SQLITE_CONTENTION_FRESHNESS_SECONDS),
    );

    assert!(
        observer.maybe_log_stall(&store).await,
        "expired SQLite contention must not suppress recovery"
    );
}

#[tokio::test]
async fn write_observer_ignores_empty_queue() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let sync_store = PendingMessageStore::new(&db_path).expect("open queue");
    let store = AsyncPendingMessageStore::from_store(sync_store);

    let observer = DaemonWriteObserver::new();
    let now = unix_secs();
    observer.force_last_successful_write_for_test(now.saturating_sub(DAEMON_STALL_SECONDS));

    assert_eq!(observer.stall_diagnostic(&store, now).await, None);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn write_observer_stall_checks_record_queue_io_burst() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let store = AsyncPendingMessageStore::from_store(
        PendingMessageStore::new(&db_path).expect("open queue"),
    );
    let queue_sample_count = || {
        crate::observability::io_burst_snapshot()
            .paths
            .into_iter()
            .find(|path| path.path == crate::observability::IoOperationPath::Queue)
            .map_or(0, |path| path.sample_count)
    };
    let before = queue_sample_count();

    DaemonWriteObserver::new().maybe_log_stall(&store).await;

    assert!(queue_sample_count() > before);
}

#[test]
fn daemon_storage_open_skips_queue_reclaim_while_writer_is_busy() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    drop(Database::open(&db_path).expect("initialize database"));

    let holder = rusqlite::Connection::open(&db_path).expect("open SQLite lock holder");
    holder
        .busy_timeout(Duration::ZERO)
        .expect("make lock holder fail fast");
    holder
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("hold SQLite write lock");

    let (sender, receiver) = std::sync::mpsc::channel();
    // Schedule the opener before starting the busy-window guard.
    let opener_started = std::sync::Arc::new(std::sync::Barrier::new(2));
    let opener_start_gate = std::sync::Arc::clone(&opener_started);
    let open_path = db_path.clone();
    let opener = std::thread::spawn(move || {
        opener_start_gate.wait();
        let outcome = open_daemon_storage_once(&open_path)
            .map(drop)
            .map_err(|error| format!("{error:#}"));
        sender.send(outcome).expect("send daemon storage result");
    });
    opener_started.wait();
    let outcome = receiver.recv_timeout(Duration::from_secs(2)).ok();
    let opened_while_busy = outcome.is_some();

    holder
        .execute_batch("ROLLBACK;")
        .expect("release SQLite write lock");
    let outcome = outcome.unwrap_or_else(|| {
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("daemon storage open must finish after releasing lock")
    });
    opener.join().expect("join daemon storage opener");

    outcome.expect("open daemon storage");
    assert!(
        opened_while_busy,
        "daemon restart must not perform queue reclamation while opening storage"
    );
}

#[test]
fn test_sqlite_lock_detection_checks_context_chain() {
    let error = anyhow::anyhow!("database is locked: Error code 5")
        .context("failed to open daemon database");

    assert!(is_sqlite_lock_error(&error));
}

#[test]
fn test_daemon_db_env_path_resolves_missing_relative_path_against_current_dir() -> Result<()> {
    let cwd = std::env::current_dir().context("read current working directory")?;
    let relative = PathBuf::from("tmp-mempal-daemon-db-path.db");
    let resolved = daemon_db_env_path(&relative)?;

    assert!(resolved.is_absolute());
    assert_eq!(resolved, cwd.join(&relative));
    Ok(())
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct FakeDbHolderOps {
    reports: Vec<DbHolderReport>,
    running: std::collections::BTreeMap<i32, bool>,
    signals: Vec<(i32, i32)>,
    term_ignored: std::collections::BTreeSet<i32>,
}

#[cfg(target_os = "linux")]
impl DbHolderProcessOps for FakeDbHolderOps {
    fn inspect_holders(&mut self) -> DbHolderReport {
        if self.reports.len() > 1 {
            self.reports.remove(0)
        } else {
            self.reports
                .first()
                .cloned()
                .unwrap_or_else(|| db_holder_report(Vec::new()))
        }
    }

    fn signal(&mut self, pid: i32, signal: i32) -> Result<()> {
        self.signals.push((pid, signal));
        if signal == libc::SIGKILL || (signal == libc::SIGTERM && !self.term_ignored.contains(&pid))
        {
            self.running.insert(pid, false);
        }
        Ok(())
    }

    fn is_running(&mut self, pid: i32) -> Result<bool> {
        Ok(self.running.get(&pid).copied().unwrap_or(false))
    }

    fn sleep(&mut self, _duration: Duration) {}
}

#[cfg(target_os = "linux")]
fn db_holder_report(holders: Vec<crate::process_diagnostics::DbHolderProcess>) -> DbHolderReport {
    DbHolderReport {
        db_path: "/tmp/palace.db".to_string(),
        holder_count: holders.len(),
        extra_holder_count: holders
            .iter()
            .filter(|holder| holder.classification == "extra_holder")
            .count(),
        stale_mcp_server_count: holders
            .iter()
            .filter(|holder| holder.classification == "stale_mcp_server")
            .count(),
        orphan_daemon_count: holders
            .iter()
            .filter(|holder| holder.classification == "orphan_daemon")
            .count(),
        error: None,
        holders,
    }
}

#[cfg(target_os = "linux")]
fn db_holder(
    pid: i32,
    role: &str,
    classification: &str,
    started_at_unix_secs: u64,
) -> crate::process_diagnostics::DbHolderProcess {
    crate::process_diagnostics::DbHolderProcess {
        pid,
        role: role.to_string(),
        classification: classification.to_string(),
        command: match role {
            "mempal_mcp_server" => "mempal serve".to_string(),
            "mempal_daemon" => "mempal daemon".to_string(),
            _ => "other process".to_string(),
        },
        opened_files: vec!["db".to_string()],
        started_at_unix_secs: Some(started_at_unix_secs),
        age_secs: None,
        current_process: classification == "current_process",
        current_daemon: classification == "current_daemon",
        current_mcp_server: classification == "current_mcp_server",
    }
}

#[cfg(target_os = "linux")]
#[test]
fn test_db_holder_termination_skips_pid_reuse_before_sigterm() {
    let original = db_holder(77, "mempal_mcp_server", "stale_mcp_server", 1_000);
    let target = DbHolderRemediationTarget::from_holder(&original);
    let reused = db_holder(77, "other", "extra_holder", 2_000);
    let mut ops = FakeDbHolderOps {
        reports: vec![db_holder_report(vec![reused])],
        running: [(77, true)].into_iter().collect(),
        ..Default::default()
    };

    let outcome = terminate_db_holder_targets_with_ops(
        &[target],
        &mut ops,
        Duration::ZERO,
        Duration::from_millis(1),
    );

    assert!(ops.signals.is_empty());
    assert!(outcome.signaled.is_empty());
    assert_eq!(outcome.errors.len(), 1);
    assert!(outcome.errors[0].contains("SIGTERM pid 77 skipped"));
    assert!(outcome.errors[0].contains("classification=stale_mcp_server"));
    assert!(outcome.errors[0].contains("classification=extra_holder"));
}

#[cfg(target_os = "linux")]
#[test]
fn test_db_holder_termination_skips_pid_reuse_before_sigkill() {
    let original = db_holder(88, "mempal_daemon", "orphan_daemon", 1_000);
    let target = DbHolderRemediationTarget::from_holder(&original);
    let reused = db_holder(88, "mempal_mcp_server", "current_mcp_server", 2_000);
    let mut ops = FakeDbHolderOps {
        reports: vec![
            db_holder_report(vec![original]),
            db_holder_report(vec![reused]),
        ],
        running: [(88, true)].into_iter().collect(),
        term_ignored: [88].into_iter().collect(),
        ..Default::default()
    };

    let outcome = terminate_db_holder_targets_with_ops(
        &[target],
        &mut ops,
        Duration::ZERO,
        Duration::from_millis(1),
    );

    assert_eq!(ops.signals, vec![(88, libc::SIGTERM)]);
    assert_eq!(outcome.signaled, vec![88]);
    assert!(outcome.killed.is_empty());
    assert_eq!(outcome.errors.len(), 1);
    assert!(outcome.errors[0].contains("SIGKILL pid 88 skipped"));
    assert!(outcome.errors[0].contains("classification=orphan_daemon"));
    assert!(outcome.errors[0].contains("classification=current_mcp_server"));
}

#[cfg(target_os = "linux")]
#[test]
fn test_db_holder_termination_preserves_matching_stale_holder_reap() {
    let original = db_holder(99, "mempal_mcp_server", "stale_mcp_server", 1_000);
    let target = DbHolderRemediationTarget::from_holder(&original);
    let mut ops = FakeDbHolderOps {
        reports: vec![db_holder_report(vec![original])],
        running: [(99, true)].into_iter().collect(),
        term_ignored: [99].into_iter().collect(),
        ..Default::default()
    };

    let outcome = terminate_db_holder_targets_with_ops(
        &[target],
        &mut ops,
        Duration::ZERO,
        Duration::from_millis(1),
    );

    assert_eq!(ops.signals, vec![(99, libc::SIGTERM), (99, libc::SIGKILL)]);
    assert_eq!(outcome.signaled, vec![99]);
    assert_eq!(outcome.killed, vec![99]);
    assert!(outcome.errors.is_empty());
}
