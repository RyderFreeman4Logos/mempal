//! Daemon ownership and heartbeat for the process-wide SQLite writer lease.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;

use crate::core::{
    db::{Database, DbError},
    types::RuntimeWriterLease,
};
use crate::daemon_bootstrap::{DaemonContext, DaemonWriterLeaseHeld, SharedDatabase};

pub(super) const SQLITE_WRITER_LEASE_NAME: &str = "sqlite-writer";
const DAEMON_WRITER_LEASE_TTL_SECS: u64 = 120;
const DAEMON_WRITER_LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(30);
const DAEMON_WRITER_LEASE_RENEW_BUSY_TIMEOUT: Duration = Duration::from_millis(100);
const DAEMON_WRITER_LEASE_RENEW_RETRY_DEADLINE: Duration = Duration::from_secs(5);
const DAEMON_WRITER_LEASE_RENEW_RETRY_DELAY: Duration = Duration::from_millis(50);
const DAEMON_WRITER_LEASE_ADMISSION_WAIT_MAX: Duration = Duration::from_secs(5);
const DAEMON_WRITER_LEASE_ADMISSION_RETRY_DELAY: Duration = Duration::from_millis(100);
/// Ceiling on the in-lease admission wait extension. A live heartbeating holder
/// renews against a bounded TTL (maintenance/daemon leases are 120s), so its
/// observed remaining_secs is already capped to that horizon; this ceiling only
/// guards a pathological/stale holder reporting a huge remaining_secs and keeps
/// the extended in-process wait bounded.
const DAEMON_WRITER_LEASE_ADMISSION_WAIT_EXTENDED_MAX: Duration = Duration::from_secs(180);

pub(super) struct RuntimeWriterLeaseHandle {
    db_path: PathBuf,
    lease: RuntimeWriterLease,
    heartbeat: tokio::task::JoinHandle<()>,
}

impl RuntimeWriterLeaseHandle {
    fn new(
        db_path: PathBuf,
        lease: RuntimeWriterLease,
        recovery_faults: crate::daemon_recovery::DaemonRecoveryFaultReporter,
    ) -> Self {
        let heartbeat =
            spawn_runtime_writer_lease_heartbeat(db_path.clone(), lease.clone(), recovery_faults);
        Self {
            db_path,
            lease,
            heartbeat,
        }
    }

    pub(super) fn lease(&self) -> &RuntimeWriterLease {
        &self.lease
    }
}

impl Drop for RuntimeWriterLeaseHandle {
    fn drop(&mut self) {
        self.heartbeat.abort();
        if let Ok(db) = Database::open(&self.db_path) {
            let _ = db.runtime_writer_lease_release(&self.lease);
        }
    }
}

pub(super) async fn acquire_daemon_writer_lease(
    context: &DaemonContext,
    db_path: &Path,
    recovery_faults: crate::daemon_recovery::DaemonRecoveryFaultReporter,
) -> Result<RuntimeWriterLeaseHandle> {
    acquire_daemon_writer_lease_with_bounded_wait(
        &context.db,
        db_path,
        recovery_faults,
        DAEMON_WRITER_LEASE_ADMISSION_WAIT_MAX,
        DAEMON_WRITER_LEASE_ADMISSION_RETRY_DELAY,
    )
    .await
}

async fn acquire_daemon_writer_lease_with_bounded_wait(
    db: &SharedDatabase,
    db_path: &Path,
    recovery_faults: crate::daemon_recovery::DaemonRecoveryFaultReporter,
    wait_max: Duration,
    retry_delay: Duration,
) -> Result<RuntimeWriterLeaseHandle> {
    acquire_daemon_writer_lease_with_holder_observed(
        db,
        db_path,
        recovery_faults,
        wait_max,
        retry_delay,
        #[cfg(test)]
        None,
    )
    .await
}

async fn acquire_daemon_writer_lease_with_holder_observed(
    db: &SharedDatabase,
    db_path: &Path,
    recovery_faults: crate::daemon_recovery::DaemonRecoveryFaultReporter,
    wait_max: Duration,
    retry_delay: Duration,
    #[cfg(test)] holder_observed: Option<&std::sync::mpsc::SyncSender<()>>,
) -> Result<RuntimeWriterLeaseHandle> {
    let metadata = json!({
        "command": "daemon",
        "db_path": db_path.to_string_lossy(),
    })
    .to_string();
    let mut deadline = None;
    loop {
        let lease_result = {
            let db = db.lock().await;
            db.runtime_writer_lease_acquire_for_daemon_start(
                SQLITE_WRITER_LEASE_NAME,
                DAEMON_WRITER_LEASE_TTL_SECS,
                Some(&metadata),
            )
        };
        let lease = match lease_result {
            Ok(lease) => lease,
            Err(error) => {
                if crate::core::db::db_error_is_sqlite_lock(&error) {
                    recovery_faults
                        .record_fault_once(crate::daemon_recovery::RecoveryFault::DatabaseLocked);
                }
                return Err(error).context("failed to acquire daemon writer lease");
            }
        };
        if let Some(lease) = lease {
            return Ok(RuntimeWriterLeaseHandle::new(
                db_path.to_path_buf(),
                lease,
                recovery_faults,
            ));
        }

        let (holders, disjoint_holder, live_remaining_secs) = {
            let db = db.lock().await;
            match db.runtime_writer_lease_status(Some(SQLITE_WRITER_LEASE_NAME)) {
                Ok(active) => (
                    format_runtime_writer_leases(&active),
                    active
                        .iter()
                        .any(|lease| writer_lease_holder_refuses_immediately(&lease.mode)),
                    active.iter().map(|lease| lease.remaining_secs).collect(),
                ),
                Err(_) => (
                    "holder status unavailable".to_string(),
                    false,
                    Vec::<i64>::new(),
                ),
            }
        };
        #[cfg(test)]
        if let Some(holder_observed) = holder_observed {
            let _ = holder_observed.try_send(());
        }
        if disjoint_holder {
            return Err(anyhow::Error::new(DaemonWriterLeaseHeld::new(holders)));
        }
        let now = Instant::now();
        let target = now + effective_admission_wait(live_remaining_secs, wait_max);
        if deadline.is_none() {
            eprintln!(
                "daemon SQLite writer lease is held by {holders}; waiting up to {}ms before temporary refusal",
                target.saturating_duration_since(now).as_millis()
            );
        }
        // Re-arm the deadline each iteration while a live holder is present: the
        // in-process wait tracks the holder's observed remaining_secs horizon so the
        // daemon stays in a non-failed waiting state and acquires the instant the
        // holder releases, instead of exiting 75 and stranding the user unit under
        // RestartPreventExitStatus=75 (#916).
        let dl = deadline.get_or_insert(target);
        if target > *dl {
            *dl = target;
        }
        if now >= *dl {
            return Err(anyhow::Error::new(DaemonWriterLeaseHeld::new(holders)));
        }
        tokio::time::sleep(dl.saturating_duration_since(now).min(retry_delay)).await;
    }
}

/// Admission wait budget: at least `wait_max` and, when a live holder is
/// present, up to its observed `remaining_secs` (capped) so bootstrap does not
/// exit 75 while a legitimate maintenance holder is still working (#916).
fn effective_admission_wait(
    live_remaining_secs: impl IntoIterator<Item = i64>,
    wait_max: Duration,
) -> Duration {
    let extend = live_remaining_secs
        .into_iter()
        .map(|secs| Duration::from_secs(secs.max(0) as u64))
        .max()
        .unwrap_or(Duration::ZERO)
        .min(DAEMON_WRITER_LEASE_ADMISSION_WAIT_EXTENDED_MAX);
    wait_max.max(extend)
}

/// Live writer modes the daemon must not wait out or displace during admission:
/// a live `mcp-ingest-worker` holds the writer lease for independent durable
/// ingestion, so daemon start refuses immediately (temporary refusal, no
/// takeover) instead of blocking, and must not let the admission wait extension
/// keep it alive past the holder's horizon (#849, #916-successor).
fn writer_lease_holder_refuses_immediately(mode: &str) -> bool {
    matches!(mode, "mcp-ingest-worker")
}

fn spawn_runtime_writer_lease_heartbeat(
    db_path: PathBuf,
    lease: RuntimeWriterLease,
    recovery_faults: crate::daemon_recovery::DaemonRecoveryFaultReporter,
) -> tokio::task::JoinHandle<()> {
    spawn_runtime_writer_lease_heartbeat_with_renewal_finished_signal(
        db_path,
        lease,
        recovery_faults,
        #[cfg(test)]
        None,
    )
}

fn spawn_runtime_writer_lease_heartbeat_with_renewal_finished_signal(
    db_path: PathBuf,
    lease: RuntimeWriterLease,
    recovery_faults: crate::daemon_recovery::DaemonRecoveryFaultReporter,
    #[cfg(test)] renewal_finished: Option<std::sync::mpsc::SyncSender<()>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(DAEMON_WRITER_LEASE_RENEW_INTERVAL);
        loop {
            interval.tick().await;
            let db_path = db_path.clone();
            let lease_for_renew = lease.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<bool> {
                renew_daemon_writer_lease_with_retry(&db_path, &lease_for_renew)
            })
            .await;
            #[cfg(test)]
            if let Some(renewal_finished) = &renewal_finished {
                let _ = renewal_finished.try_send(());
            }
            match result {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => {
                    tracing::error!(
                        lease = %lease.name,
                        owner = %lease.owner,
                        "daemon writer lease was lost; requesting shutdown"
                    );
                    recovery_faults
                        .record_fault_once(crate::daemon_recovery::RecoveryFault::WriterLeaseLost);
                    #[cfg(unix)]
                    super::request_shutdown_and_notify();
                    break;
                }
                Ok(Err(error)) => {
                    tracing::warn!(error = %error, "failed to renew daemon writer lease");
                }
                Err(error) => {
                    tracing::warn!(error = %error, "writer lease heartbeat task failed");
                }
            }
        }
    })
}

fn renew_daemon_writer_lease_with_retry(
    db_path: &Path,
    lease: &RuntimeWriterLease,
) -> Result<bool> {
    let started = Instant::now();
    loop {
        match renew_daemon_writer_lease_once(db_path, lease) {
            Ok(renewed) => return Ok(renewed),
            Err(error)
                if anyhow_error_is_sqlite_lock(&error)
                    && started.elapsed() < DAEMON_WRITER_LEASE_RENEW_RETRY_DEADLINE =>
            {
                std::thread::sleep(DAEMON_WRITER_LEASE_RENEW_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
}

fn renew_daemon_writer_lease_once(db_path: &Path, lease: &RuntimeWriterLease) -> Result<bool> {
    let db = Database::open_with_busy_timeout(db_path, DAEMON_WRITER_LEASE_RENEW_BUSY_TIMEOUT)
        .context("failed to open DB for writer lease renew")?;
    db.runtime_writer_lease_renew(lease, DAEMON_WRITER_LEASE_TTL_SECS)
        .context("failed to renew daemon writer lease")
}

fn anyhow_error_is_sqlite_lock(error: &anyhow::Error) -> bool {
    error.chain().any(|error| {
        error
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(crate::core::db::rusqlite_error_is_lock)
            || error
                .downcast_ref::<DbError>()
                .is_some_and(crate::core::db::db_error_is_sqlite_lock)
            || matches!(
                error.downcast_ref::<crate::core::db_admission::DbAdmissionError>(),
                Some(crate::core::db_admission::DbAdmissionError::Busy { .. })
            )
    })
}

fn format_runtime_writer_leases(leases: &[RuntimeWriterLease]) -> String {
    if leases.is_empty() {
        return "none visible".to_string();
    }
    leases
        .iter()
        .map(|lease| {
            format!(
                "mode={} pid={} owner={} name={} remaining_secs={} expires_at={} heartbeat_at={}",
                lease.mode,
                lease.pid,
                lease.owner,
                lease.name,
                lease.remaining_secs,
                lease.expires_at,
                lease.heartbeat_at
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_lease_retry_classifier_treats_admission_busy_as_lock() {
        let busy = crate::core::db_admission::DbAdmissionError::Busy {
            path: PathBuf::from("/tmp/profile.db.admission.lock"),
            timeout_ms: 250,
        };
        let wrapped = anyhow::Error::new(busy).context("failed to open DB for writer lease renew");
        assert!(
            anyhow_error_is_sqlite_lock(&wrapped),
            "admission Busy must remain retryable through the renew classifier"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn daemon_writer_lease_waits_for_holder_release_before_admission() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let db_path = tempdir.path().join("palace.db");
        let holder_db = Database::open(&db_path).expect("open holder database");
        let held = holder_db
            .runtime_writer_lease_acquire(
                SQLITE_WRITER_LEASE_NAME,
                "mcp-holder",
                "mcp",
                DAEMON_WRITER_LEASE_TTL_SECS,
                None,
            )
            .expect("acquire holder lease")
            .expect("holder lease available");
        let daemon_db = std::sync::Arc::new(tokio::sync::Mutex::new(
            Database::open(&db_path).expect("open daemon database"),
        ));
        let (holder_observed_tx, holder_observed_rx) = std::sync::mpsc::sync_channel(1);
        let release_path = db_path.clone();
        let release_lease = held.clone();
        let releaser = tokio::task::spawn_blocking(move || {
            holder_observed_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("daemon admission must observe the live holder before release");
            Database::open(&release_path)
                .expect("open releaser database")
                .runtime_writer_lease_release(&release_lease)
                .expect("release holder lease")
        });

        let recovery = crate::daemon_recovery::DaemonRecovery::new(tempdir.path());
        let acquired = acquire_daemon_writer_lease_with_holder_observed(
            &daemon_db,
            &db_path,
            crate::daemon_recovery::DaemonRecoveryFaultReporter::new(recovery),
            Duration::from_secs(1),
            Duration::from_millis(5),
            Some(&holder_observed_tx),
        )
        .await
        .expect("daemon admission continues after holder releases");
        assert_eq!(acquired.lease().mode, "daemon");
        assert!(releaser.await.expect("join lease releaser"));
    }

    #[test]
    fn effective_admission_wait_floors_at_base_and_caps_extension() {
        let base = Duration::from_millis(5);
        // No live holder -> the fixed base bounded wait (legacy #847 behavior).
        assert_eq!(effective_admission_wait(Vec::<i64>::new(), base), base);
        // 0 remaining -> base wait.
        assert_eq!(effective_admission_wait([0i64], base), base);
        // Live holder below the cap -> the observed remaining_secs extends the wait
        // so bootstrap does not exit 75 and strand the unit while the holder releases.
        assert_eq!(
            effective_admission_wait([109i64], base),
            Duration::from_secs(109)
        );
        // The extension is capped so a pathological/stale holder never wedges forever.
        assert_eq!(
            effective_admission_wait([240_000i64], base),
            DAEMON_WRITER_LEASE_ADMISSION_WAIT_EXTENDED_MAX
        );
        // Only the longest live holder drives the wait.
        assert_eq!(
            effective_admission_wait([10i64, 40i64], base),
            Duration::from_secs(40)
        );
    }

    #[test]
    fn writer_lease_holder_refuses_immediately_discriminates_wait_modes() {
        // Compatibility boundaries, never rewritten to wait 180s for an
        // incompatible live durable ingest worker (#916 successor).
        assert!(writer_lease_holder_refuses_immediately("mcp-ingest-worker"));
        // Maintenance/rejudge/daemon-family holders are waited out (#916).
        for compatible in ["maintenance", "rejudge", "daemon", "mcp"] {
            assert!(!writer_lease_holder_refuses_immediately(compatible));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn daemon_writer_lease_waits_out_a_live_holder_before_admission_not_75() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let db_path = tempdir.path().join("palace.db");
        let holder_db = Database::open(&db_path).expect("open holder database");
        let held = holder_db
            .runtime_writer_lease_acquire(
                SQLITE_WRITER_LEASE_NAME,
                "maintenance-owner",
                "maintenance",
                DAEMON_WRITER_LEASE_TTL_SECS,
                None,
            )
            .expect("acquire holder lease")
            .expect("holder lease available");

        let daemon_db = std::sync::Arc::new(tokio::sync::Mutex::new(
            Database::open(&db_path).expect("open daemon database"),
        ));
        let (holder_observed_tx, holder_observed_rx) = std::sync::mpsc::sync_channel(1);
        let release_path = db_path.clone();
        let release_lease = held.clone();
        let releaser = tokio::task::spawn_blocking(move || {
            // The daemon must observe the live holder, then stay alive long past the
            // nominal 300ms base wait before the holder releases.
            holder_observed_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("daemon must observe the live holder");
            std::thread::sleep(Duration::from_millis(700));
            Database::open(&release_path)
                .expect("open releaser database")
                .runtime_writer_lease_release(&release_lease)
                .expect("release holder lease")
        });

        let recovery = crate::daemon_recovery::DaemonRecovery::new(tempdir.path());
        let acquired = acquire_daemon_writer_lease_with_holder_observed(
            &daemon_db,
            &db_path,
            crate::daemon_recovery::DaemonRecoveryFaultReporter::new(recovery),
            Duration::from_millis(300),
            Duration::from_millis(20),
            Some(&holder_observed_tx),
        )
        .await
        .expect(
            "live upstream holder must extend bootstrap admission past the base wait \
             instead of exiting 75 and stranding the user unit (RestartPreventExitStatus=75)",
        );
        assert_eq!(acquired.lease().mode, "daemon");
        assert!(releaser.await.expect("join lease releaser"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn daemon_writer_lease_waits_for_expired_live_holder_release_before_admission() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let db_path = tempdir.path().join("palace.db");
        let holder_db = Database::open(&db_path).expect("open holder database");
        let held = holder_db
            .runtime_writer_lease_acquire_for_daemon_start(
                SQLITE_WRITER_LEASE_NAME,
                DAEMON_WRITER_LEASE_TTL_SECS,
                None,
            )
            .expect("acquire live holder lease")
            .expect("holder lease available");
        holder_db
            .conn()
            .execute(
                "UPDATE runtime_writer_leases \
                 SET expires_at = '1970-01-01T00:00:00Z' \
                 WHERE name = ?1 AND owner = ?2 AND session_id = ?3",
                rusqlite::params![&held.name, &held.owner, &held.session_id],
            )
            .expect("force live holder expiry");
        let status = holder_db
            .runtime_writer_lease_status(Some(SQLITE_WRITER_LEASE_NAME))
            .expect("load live expired holder status");
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].remaining_secs, 0);

        let daemon_db = std::sync::Arc::new(tokio::sync::Mutex::new(
            Database::open(&db_path).expect("open daemon database"),
        ));
        let (holder_observed_tx, holder_observed_rx) = std::sync::mpsc::sync_channel(1);
        let release_path = db_path.clone();
        let release_lease = held.clone();
        let releaser = tokio::task::spawn_blocking(move || {
            holder_observed_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("daemon admission must observe the live expired holder before release");
            Database::open(&release_path)
                .expect("open releaser database")
                .runtime_writer_lease_release(&release_lease)
                .expect("release live expired holder lease")
        });

        let acquired = acquire_daemon_writer_lease_with_holder_observed(
            &daemon_db,
            &db_path,
            crate::daemon_recovery::DaemonRecoveryFaultReporter::new(
                crate::daemon_recovery::DaemonRecovery::new(tempdir.path()),
            ),
            Duration::from_secs(1),
            Duration::from_millis(5),
            Some(&holder_observed_tx),
        )
        .await;
        assert!(releaser.await.expect("join lease releaser"));

        let acquired =
            acquired.expect("daemon admission continues after live expired holder releases");
        assert_eq!(acquired.lease().mode, "daemon");
    }

    struct ShutdownResetGuard;

    impl Drop for ShutdownResetGuard {
        fn drop(&mut self) {
            super::super::reset_shutdown_request();
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn daemon_writer_lease_heartbeat_survives_crud_lock_and_recovers() {
        let _shutdown_lock = super::super::global_shutdown_test_lock().lock_owned().await;
        super::super::reset_shutdown_request();
        let _shutdown_reset = ShutdownResetGuard;
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open isolated database");
        let lease = db
            .runtime_writer_lease_acquire_for_daemon_start(
                SQLITE_WRITER_LEASE_NAME,
                DAEMON_WRITER_LEASE_TTL_SECS,
                None,
            )
            .expect("acquire daemon writer lease")
            .expect("daemon writer lease must be available");
        let recovery = crate::daemon_recovery::DaemonRecovery::new(tempdir.path());
        let recovery_faults =
            crate::daemon_recovery::DaemonRecoveryFaultReporter::new(recovery.clone());

        db.conn()
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("hold CRUD writer lock");
        let (renewal_finished_tx, renewal_finished_rx) = std::sync::mpsc::sync_channel(1);
        let heartbeat = spawn_runtime_writer_lease_heartbeat_with_renewal_finished_signal(
            db_path.clone(),
            lease.clone(),
            recovery_faults,
            Some(renewal_finished_tx),
        );
        tokio::task::spawn_blocking(move || {
            renewal_finished_rx.recv_timeout(
                DAEMON_WRITER_LEASE_RENEW_RETRY_DEADLINE
                    + DAEMON_WRITER_LEASE_RENEW_BUSY_TIMEOUT
                    + Duration::from_secs(1),
            )
        })
        .await
        .expect("join renewal completion checkpoint")
        .expect("writer lease renewal must finish its bounded retry while CRUD is locked");

        assert!(
            !super::super::shutdown_requested(),
            "transient CRUD contention must not request daemon shutdown"
        );
        assert!(
            !heartbeat.is_finished(),
            "transient CRUD contention must not stop the daemon lease heartbeat"
        );
        let snapshot = recovery.snapshot().expect("read isolated recovery state");
        assert_eq!(snapshot.recent_fault_count, 0);
        assert_eq!(snapshot.last_fault, None);

        db.conn()
            .execute_batch("ROLLBACK;")
            .expect("release CRUD writer lock");
        let renewed = tokio::task::spawn_blocking(move || {
            renew_daemon_writer_lease_with_retry(&db_path, &lease)
        })
        .await
        .expect("join recovery renewal")
        .expect("renew after CRUD pressure releases");
        assert!(renewed, "the same live lease must recover after contention");

        heartbeat.abort();
        let _ = heartbeat.await;
    }
}
