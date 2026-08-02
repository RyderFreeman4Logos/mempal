//! Daemon ownership and heartbeat for the process-wide SQLite writer lease.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;

use crate::core::{
    db::{Database, DbError},
    types::RuntimeWriterLease,
};
use crate::daemon_bootstrap::DaemonContext;

pub(super) const SQLITE_WRITER_LEASE_NAME: &str = "sqlite-writer";
const DAEMON_WRITER_LEASE_TTL_SECS: u64 = 120;
const DAEMON_WRITER_LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(30);
const DAEMON_WRITER_LEASE_RENEW_BUSY_TIMEOUT: Duration = Duration::from_millis(100);
const DAEMON_WRITER_LEASE_RENEW_RETRY_DEADLINE: Duration = Duration::from_secs(5);
const DAEMON_WRITER_LEASE_RENEW_RETRY_DELAY: Duration = Duration::from_millis(50);

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
    let metadata = json!({
        "command": "daemon",
        "db_path": db_path.to_string_lossy(),
    })
    .to_string();
    let lease_result = {
        let db = context.db.lock().await;
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
    match lease {
        Some(lease) => Ok(RuntimeWriterLeaseHandle::new(
            db_path.to_path_buf(),
            lease,
            recovery_faults,
        )),
        None => {
            let active = {
                let db = context.db.lock().await;
                db.runtime_writer_lease_status(Some(SQLITE_WRITER_LEASE_NAME))
                    .unwrap_or_default()
            };
            Err(anyhow::anyhow!(
                "SQLite writer lease `{}` is already held: {}",
                SQLITE_WRITER_LEASE_NAME,
                format_runtime_writer_leases(&active)
            ))
        }
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
                "name={} owner={} pid={} mode={} expires_at={} heartbeat_at={}",
                lease.name,
                lease.owner,
                lease.pid,
                lease.mode,
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
