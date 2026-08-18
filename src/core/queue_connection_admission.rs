//! Admission owner for one pending-queue connection cache.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use super::db_admission::{DbAdmissionRequest, DbHolderClass, ProfileDbAdmission};
use super::queue::{
    DEFAULT_MAX_INGEST_ACTIVE_BYTES, QueueError, QueueStats, Result, compute_queue_stats,
};

pub(super) const QUEUE_CONNECTIONS_PER_CACHE: usize = 3;
pub(super) const QUEUE_SQLITE_CACHE_SIZE_KIB: i64 = -2_048;
pub(super) const QUEUE_CONNECTION_CACHE_BYTES: u64 =
    QUEUE_CONNECTIONS_PER_CACHE as u64 * 2 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct QueueConnectionAdmission {
    guard: Arc<Mutex<Option<ProfileDbAdmission>>>,
}

impl QueueConnectionAdmission {
    pub(super) fn new() -> Self {
        Self {
            guard: Arc::new(Mutex::new(None)),
        }
    }

    pub(super) fn ensure(&self, db_path: &Path) -> Result<std::path::PathBuf> {
        let mut admission = self
            .guard
            .lock()
            .map_err(|_| QueueError::ClaimConnectionMutexPoisoned)?;
        if admission.is_none() {
            *admission = Some(ProfileDbAdmission::acquire(
                db_path,
                DbAdmissionRequest::new(
                    DbHolderClass::current_process(),
                    QUEUE_CONNECTIONS_PER_CACHE,
                    QUEUE_CONNECTION_CACHE_BYTES,
                ),
            )?);
        }
        Ok(admission
            .as_ref()
            .expect("queue admission is initialized")
            .database_path()
            .to_path_buf())
    }

    pub(super) fn open_sqlite(&self, db_path: &Path, flags: OpenFlags) -> Result<Connection> {
        let admitted_path = self.ensure(db_path)?;
        Ok(Connection::open_with_flags(
            admitted_path,
            flags | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?)
    }

    pub(super) fn open_read_write(&self, db_path: &Path) -> Result<Connection> {
        self.open_sqlite(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)
    }
}

/// Verify that a fresh queue cache can be admitted and obtain a write lock
/// without changing queue state.
///
/// The zero SQLite busy timeout is intentional: callers use this before
/// dispatching work that must return a no-write receipt instead of waiting
/// behind an unknown writer.
pub fn queue_write_admission_preflight(path: &Path) -> Result<()> {
    let admission = QueueConnectionAdmission::new();
    let connection = admission.open_read_write(path)?;
    connection.busy_timeout(Duration::ZERO)?;
    connection.execute_batch("BEGIN IMMEDIATE; ROLLBACK;")?;
    Ok(())
}

/// Diagnostic readonly queue stat reads must never inherit SQLite's default
/// long (5s) busy wait. When a caller supplies no explicit timeout, use this
/// short bounded default so a held writer lock surfaces as a quick diagnostic.
pub(super) const QUEUE_STATS_READONLY_DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_millis(100);

/// Read queue statistics without startup reclamation or a writable connection.
pub fn queue_stats_readonly(path: &Path) -> Result<QueueStats> {
    queue_stats_readonly_with_optional_busy_timeout(path, None, |resolved_path| {
        Connection::open_with_flags(
            resolved_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
    })
}

/// Read queue statistics with a bounded SQLite busy wait for diagnostics.
pub fn queue_stats_readonly_with_busy_timeout(
    path: &Path,
    busy_timeout: Duration,
) -> Result<QueueStats> {
    queue_stats_readonly_with_optional_busy_timeout(path, Some(busy_timeout), |resolved_path| {
        Connection::open_with_flags(
            resolved_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
    })
}

#[cfg(test)]
fn queue_stats_readonly_with_opener(
    path: &Path,
    open: impl FnOnce(&Path) -> rusqlite::Result<Connection>,
) -> Result<QueueStats> {
    queue_stats_readonly_with_optional_busy_timeout(path, None, open)
}

fn queue_stats_readonly_with_optional_busy_timeout(
    path: &Path,
    busy_timeout: Option<Duration>,
    open: impl FnOnce(&Path) -> rusqlite::Result<Connection>,
) -> Result<QueueStats> {
    if !path.exists() {
        return Err(QueueError::DatabaseMissing(path.to_path_buf()));
    }
    let admission = ProfileDbAdmission::acquire(
        path,
        DbAdmissionRequest::new(DbHolderClass::current_process(), 1, 2 * 1024 * 1024),
    )?;
    let connection = open(admission.database_path())?;
    connection.busy_timeout(busy_timeout.unwrap_or(QUEUE_STATS_READONLY_DEFAULT_BUSY_TIMEOUT))?;
    connection.pragma_update(None, "cache_size", QUEUE_SQLITE_CACHE_SIZE_KIB)?;
    compute_queue_stats(&connection, DEFAULT_MAX_INGEST_ACTIVE_BYTES)
}

/// Compute queue diagnostics from an existing connection without opening or
/// admitting another profile holder.
pub fn queue_stats(connection: &Connection) -> Result<QueueStats> {
    compute_queue_stats(connection, DEFAULT_MAX_INGEST_ACTIVE_BYTES)
}

#[cfg(test)]
#[path = "queue_admission_tests.rs"]
mod tests;
