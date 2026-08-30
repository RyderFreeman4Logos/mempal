use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use super::{
    Database, DbError, RUNTIME_WRITER_LEASE_TRANSACTION_RETRY_DEADLINE, db_error_is_sqlite_lock,
};

pub(crate) const WRITE_RESERVE_BYTES: u64 = 300 * 1024 * 1024;
static WRITE_RESERVE_WARNED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

pub(crate) fn write_reserve_path(database_path: &Path) -> PathBuf {
    let name = database_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("palace.db");
    database_path.with_file_name(format!(".{name}.write-reserve"))
}

#[cfg(unix)]
pub(crate) fn ensure_write_reserve(database_path: &Path) -> std::io::Result<()> {
    use std::{os::fd::AsRawFd, os::unix::fs::OpenOptionsExt};

    let path = write_reserve_path(database_path);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .mode(0o600)
        .open(path)?;
    let current_len = file.metadata()?.len();
    if current_len < WRITE_RESERVE_BYTES {
        // SAFETY: `file` keeps this valid descriptor open for the call, and the
        // nonnegative offset and length are each bounded by WRITE_RESERVE_BYTES.
        let result = unsafe {
            libc::posix_fallocate(
                file.as_raw_fd(),
                current_len as libc::off_t,
                (WRITE_RESERVE_BYTES - current_len) as libc::off_t,
            )
        };
        if result != 0 {
            return Err(std::io::Error::from_raw_os_error(result));
        }
        file.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn ensure_write_reserve(_database_path: &Path) -> std::io::Result<()> {
    Ok(())
}

pub(crate) fn ensure_write_reserve_logged(database_path: &Path) {
    if let Err(error) = ensure_write_reserve(database_path) {
        tracing::debug!(
            path = %database_path.display(),
            %error,
            "write reserve unavailable"
        );
    }
}

fn consume_write_reserve(database_path: &Path, operation: &'static str) -> bool {
    let path = write_reserve_path(database_path);
    if fs::remove_file(&path).is_err() {
        return false;
    }
    let warned = WRITE_RESERVE_WARNED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map(|mut paths| paths.insert(path))
        .unwrap_or(false);
    if warned {
        tracing::warn!(
            operation_key = operation,
            "disk reserve consumed; stop and have the user release disk space"
        );
    }
    true
}

fn write_error_is_sqlite_full<E: std::error::Error + 'static>(error: &E) -> bool {
    let mut current = Some(error as &(dyn std::error::Error + 'static));
    while let Some(error) = current {
        if let Some(sqlite_error) = error.downcast_ref::<rusqlite::Error>() {
            if matches!(
                sqlite_error,
                rusqlite::Error::SqliteFailure(code, _)
                    if code.code == rusqlite::ErrorCode::DiskFull
                        || code.extended_code == rusqlite::ffi::SQLITE_FULL
            ) {
                return true;
            }
        }
        let text = error.to_string().to_ascii_lowercase();
        if text.contains("database or disk is full")
            || text.contains("disk full")
            || text.contains("enospc")
        {
            return true;
        }
        current = error.source();
    }
    false
}

impl Database {
    pub(crate) fn with_write_reserve_retry<T, E>(
        &self,
        operation: &'static str,
        mut write: impl FnMut() -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<DbError> + std::error::Error + 'static,
    {
        let mut reserve_consumed = false;
        loop {
            if !self.conn.is_autocommit() {
                return write();
            }

            let begin = crate::core::sqlite_retry::retry_content_mutation_sqlite_lock_until(
                Instant::now() + RUNTIME_WRITER_LEASE_TRANSACTION_RETRY_DEADLINE,
                || {
                    self.conn
                        .execute_batch("BEGIN IMMEDIATE")
                        .map_err(DbError::from)
                },
                db_error_is_sqlite_lock,
            );
            if let Err(error) = begin {
                if !reserve_consumed
                    && write_error_is_sqlite_full(&error)
                    && consume_write_reserve(&self.path, operation)
                {
                    reserve_consumed = true;
                    continue;
                }
                return Err(E::from(error));
            }

            match write() {
                Ok(value) => match self.conn.execute_batch("COMMIT") {
                    Ok(()) => return Ok(value),
                    Err(commit_error) => {
                        let _ = self.conn.execute_batch("ROLLBACK");
                        if !reserve_consumed
                            && write_error_is_sqlite_full(&commit_error)
                            && consume_write_reserve(&self.path, operation)
                        {
                            reserve_consumed = true;
                            continue;
                        }
                        return Err(E::from(DbError::from(commit_error)));
                    }
                },
                Err(error) => {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    if !reserve_consumed
                        && write_error_is_sqlite_full(&error)
                        && consume_write_reserve(&self.path, operation)
                    {
                        reserve_consumed = true;
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn write_reserve_is_consumed_and_db_write_retries_after_sqlite_full() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let reserve_path = write_reserve_path(db.path());
        assert_eq!(
            fs::metadata(&reserve_path)
                .expect("write reserve sidecar")
                .len(),
            WRITE_RESERVE_BYTES
        );

        let mut attempts = 0;
        db.with_write_reserve_retry("test write", || {
            attempts += 1;
            if attempts == 1 {
                return Err(DbError::Sqlite(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code: rusqlite::ErrorCode::DiskFull,
                        extended_code: rusqlite::ffi::SQLITE_FULL,
                    },
                    Some("database or disk is full".to_string()),
                )));
            }
            db.conn()
                .execute("CREATE TABLE reserve_retry(value TEXT NOT NULL)", [])?;
            db.conn()
                .execute("INSERT INTO reserve_retry(value) VALUES ('durable')", [])?;
            Ok(())
        })
        .expect("write must succeed after consuming reserve");

        assert_eq!(attempts, 2);
        assert!(!reserve_path.exists(), "reserve must be consumed once");
        assert_eq!(
            db.conn()
                .query_row("SELECT value FROM reserve_retry", [], |row| {
                    row.get::<_, String>(0)
                })
                .expect("read durable retry"),
            "durable"
        );
    }
}
