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

        let active = {
            let db = db.lock().await;
            db.runtime_writer_lease_status(Some(SQLITE_WRITER_LEASE_NAME))
                .unwrap_or_default()
        };
        let holders = format_runtime_writer_leases(&active);
        let now = Instant::now();
        let deadline = *deadline.get_or_insert_with(|| {
            eprintln!(
                "daemon SQLite writer lease is held by {holders}; waiting up to {}ms before temporary refusal",
                wait_max.as_millis()
            );
            now + wait_max
        });
        if now >= deadline {
            return Err(anyhow::Error::new(DaemonWriterLeaseHeld::new(holders)));
        }
        tokio::time::sleep(deadline.saturating_duration_since(now).min(retry_delay)).await;
    }
}

fn spawn_runtime_writer_lease_heartbeat(
    db_path: PathBuf,
    lease: RuntimeWriterLease,
    recovery_faults: crate::daemon_recovery::DaemonRecoveryFaultReporter,
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
        let release_path = db_path.clone();
        let release_lease = held.clone();
        let releaser = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(20));
            Database::open(&release_path)
                .expect("open releaser database")
                .runtime_writer_lease_release(&release_lease)
                .expect("release holder lease")
        });

        let recovery = crate::daemon_recovery::DaemonRecovery::new(tempdir.path());
        let acquired = acquire_daemon_writer_lease_with_bounded_wait(
            &daemon_db,
            &db_path,
            crate::daemon_recovery::DaemonRecoveryFaultReporter::new(recovery),
            Duration::from_secs(1),
            Duration::from_millis(5),
        )
        .await
        .expect("daemon admission continues after holder releases");
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
        let release_path = db_path.clone();
        let release_lease = held.clone();
        let releaser = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(20));
            Database::open(&release_path)
                .expect("open releaser database")
                .runtime_writer_lease_release(&release_lease)
                .expect("release live expired holder lease")
        });

        let acquired = acquire_daemon_writer_lease_with_bounded_wait(
            &daemon_db,
            &db_path,
            crate::daemon_recovery::DaemonRecoveryFaultReporter::new(
                crate::daemon_recovery::DaemonRecovery::new(tempdir.path()),
            ),
            Duration::from_secs(1),
            Duration::from_millis(5),
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
        let heartbeat =
            spawn_runtime_writer_lease_heartbeat(db_path.clone(), lease.clone(), recovery_faults);
        tokio::time::sleep(DAEMON_WRITER_LEASE_RENEW_RETRY_DEADLINE + Duration::from_secs(1)).await;

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
