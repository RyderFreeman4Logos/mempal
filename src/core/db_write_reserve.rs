use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{
    Database, DbError, RUNTIME_WRITER_LEASE_TRANSACTION_RETRY_DEADLINE, db_error_is_sqlite_lock,
};

pub(crate) const WRITE_RESERVE_BYTES: u64 = 300 * 1024 * 1024;
static WRITE_RESERVE_WARNED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

#[cfg(test)]
struct ReserveConsumedTestHook {
    path: PathBuf,
    consumed: std::sync::mpsc::SyncSender<PathBuf>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
struct ReserveEnsureAttemptedTestHook {
    path: PathBuf,
    attempted: std::sync::mpsc::SyncSender<()>,
}

#[cfg(test)]
static RESERVE_CONSUMED_TEST_HOOK: OnceLock<Mutex<Option<ReserveConsumedTestHook>>> =
    OnceLock::new();

#[cfg(test)]
static RESERVE_ENSURE_ATTEMPTED_TEST_HOOK: OnceLock<Mutex<Option<ReserveEnsureAttemptedTestHook>>> =
    OnceLock::new();

pub(crate) fn write_reserve_path(database_path: &Path) -> PathBuf {
    let name = database_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("palace.db");
    database_path.with_file_name(format!(".{name}.write-reserve"))
}

pub(crate) fn write_reserve_lock_path(database_path: &Path) -> PathBuf {
    let name = database_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("palace.db");
    database_path.with_file_name(format!(".{name}.write-reserve.lock"))
}

#[cfg(unix)]
struct WriteReserveLock {
    file: std::fs::File,
}

#[cfg(not(unix))]
struct WriteReserveLock;

impl WriteReserveLock {
    fn acquire(database_path: &Path) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::{os::fd::AsRawFd, os::unix::fs::OpenOptionsExt};

            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .mode(0o600)
                .open(write_reserve_lock_path(database_path))?;
            let deadline = Instant::now() + RUNTIME_WRITER_LEASE_TRANSACTION_RETRY_DEADLINE;
            loop {
                // SAFETY: `file` keeps this valid descriptor open for the call,
                // and `flock` does not retain the descriptor or access Rust memory.
                let result =
                    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if result == 0 {
                    return Ok(Self { file });
                }
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                    return Err(error);
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "write reserve lock wait timed out",
                    ));
                }
                std::thread::sleep(Duration::from_millis(25).min(remaining));
            }
        }
        #[cfg(not(unix))]
        {
            let _ = database_path;
            Ok(Self)
        }
    }
}

#[cfg(unix)]
impl Drop for WriteReserveLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;

        // SAFETY: `file` owns this descriptor, and `flock` does not retain it.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
fn notify_reserve_ensure_attempted_for_test(database_path: &Path) {
    let Ok(hook) = RESERVE_ENSURE_ATTEMPTED_TEST_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
    else {
        return;
    };
    if let Some(hook) = hook.as_ref().filter(|hook| hook.path == database_path) {
        let _ = hook.attempted.send(());
    }
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
        .open(&path)?;
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
        sync_reserve_parent_directory(&path)?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn ensure_write_reserve(_database_path: &Path) -> std::io::Result<()> {
    Ok(())
}

pub(crate) fn ensure_write_reserve_logged(database_path: &Path) {
    #[cfg(test)]
    notify_reserve_ensure_attempted_for_test(database_path);
    tracing::info!(
        path = %database_path.display(),
        "waiting for write reserve lock"
    );
    let Ok(_lock) = WriteReserveLock::acquire(database_path) else {
        tracing::debug!(
            path = %database_path.display(),
            "write reserve lock unavailable"
        );
        return;
    };
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
    if sync_reserve_parent_directory(&path).is_err() {
        return false;
    }
    let warned = WRITE_RESERVE_WARNED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map(|mut paths| paths.insert(path.clone()))
        .unwrap_or(false);
    if warned {
        tracing::warn!(
            operation_key = operation,
            "disk reserve consumed; stop and have the user release disk space"
        );
    }
    #[cfg(test)]
    pause_after_reserve_consumed_for_test(&path);
    true
}

fn sync_reserve_parent_directory(path: &Path) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    crate::ingress_spool::sync_directory(parent)
}

#[cfg(test)]
fn pause_after_reserve_consumed_for_test(path: &Path) {
    let hook = RESERVE_CONSUMED_TEST_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|mut hooks| {
            hooks
                .as_ref()
                .filter(|hook| hook.path == path)
                .is_some()
                .then(|| hooks.take())
                .flatten()
        });
    if let Some(hook) = hook {
        hook.consumed
            .send(path.to_path_buf())
            .expect("reserve test hook receiver");
        hook.resume.recv().expect("reserve test hook resume");
    }
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

#[cfg(test)]
static WRITE_RESERVE_RETRY_SQLITE_FULL_TEST_PATH: OnceLock<Mutex<Option<PathBuf>>> =
    OnceLock::new();

#[cfg(test)]
pub(crate) fn fail_next_write_reserve_retry_with_sqlite_full(path: &Path) {
    WRITE_RESERVE_RETRY_SQLITE_FULL_TEST_PATH
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("write-reserve SQLite_FULL test hook mutex")
        .replace(path.to_path_buf());
}

#[cfg(test)]
pub(crate) fn fail_write_reserve_retry_with_sqlite_full_for_test(
    path: &Path,
) -> Result<(), DbError> {
    let should_fail = WRITE_RESERVE_RETRY_SQLITE_FULL_TEST_PATH
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|mut path_to_fail| {
            path_to_fail
                .as_ref()
                .filter(|configured_path| configured_path.as_path() == path)
                .is_some()
                .then(|| path_to_fail.take())
                .flatten()
        })
        .is_some();
    if should_fail {
        return Err(DbError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DiskFull,
                extended_code: rusqlite::ffi::SQLITE_FULL,
            },
            Some("database or disk is full".to_string()),
        )));
    }
    Ok(())
}

pub(crate) fn with_write_reserve_retry<T, E>(
    database_path: &Path,
    operation: &'static str,
    mut write: impl FnMut() -> Result<T, E>,
) -> Result<T, E>
where
    E: From<DbError> + std::error::Error + 'static,
{
    let _reserve_lock = WriteReserveLock::acquire(database_path).map_err(|source| {
        E::from(DbError::WriteReserveLock {
            path: write_reserve_lock_path(database_path),
            source,
        })
    })?;
    let mut reserve_consumed = false;
    loop {
        match write() {
            Ok(value) => return Ok(value),
            Err(error)
                if !reserve_consumed
                    && write_error_is_sqlite_full(&error)
                    && consume_write_reserve(database_path, operation) =>
            {
                reserve_consumed = true;
            }
            Err(error) => return Err(error),
        }
    }
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
        if !self.conn.is_autocommit() {
            return write();
        }
        with_write_reserve_retry(&self.path, operation, || {
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
                return Err(E::from(error));
            }

            match write() {
                Ok(value) => match self.conn.execute_batch("COMMIT") {
                    Ok(()) => Ok(value),
                    Err(error) => {
                        let _ = self.conn.execute_batch("ROLLBACK");
                        Err(E::from(DbError::from(error)))
                    }
                },
                Err(error) => {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    Err(error)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc::sync_channel;
    use std::thread;

    use super::*;

    fn sqlite_full_error() -> DbError {
        DbError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DiskFull,
                extended_code: rusqlite::ffi::SQLITE_FULL,
            },
            Some("database or disk is full".to_string()),
        ))
    }

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
                return Err(sqlite_full_error());
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

    #[test]
    fn concurrent_open_does_not_refill_reserve_before_retry() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let database_path = db.path().to_path_buf();
        let reserve_path = write_reserve_path(db.path());
        let (consumed_tx, consumed_rx) = sync_channel(1);
        let (resume_tx, resume_rx) = sync_channel(0);
        RESERVE_CONSUMED_TEST_HOOK
            .get_or_init(|| Mutex::new(None))
            .lock()
            .expect("reserve test hook mutex")
            .replace(ReserveConsumedTestHook {
                path: reserve_path.clone(),
                consumed: consumed_tx,
                resume: resume_rx,
            });
        let (ensure_attempted_tx, ensure_attempted_rx) = sync_channel(1);
        RESERVE_ENSURE_ATTEMPTED_TEST_HOOK
            .get_or_init(|| Mutex::new(None))
            .lock()
            .expect("reserve ensure test hook mutex")
            .replace(ReserveEnsureAttemptedTestHook {
                path: database_path,
                attempted: ensure_attempted_tx,
            });

        let writer = thread::spawn(move || {
            let mut attempts = 0;
            db.with_write_reserve_retry("competing open", || {
                attempts += 1;
                if attempts == 1 {
                    return Err(sqlite_full_error());
                }
                if reserve_path.exists() {
                    return Err(sqlite_full_error());
                }
                db.conn().execute(
                    "CREATE TABLE reserve_competing_open(value TEXT NOT NULL)",
                    [],
                )?;
                Ok(())
            })
        });

        consumed_rx
            .recv()
            .expect("writer must pause after consuming reserve");
        let competing_path = db_path.clone();
        let competing = thread::spawn(move || Database::open(&competing_path));
        ensure_attempted_rx
            .recv()
            .expect("competing open must enter reserve ensure path");
        resume_tx.send(()).expect("resume writer");

        writer
            .join()
            .expect("writer thread")
            .expect("retry must not be defeated by competing reserve refill");
        let competing = competing
            .join()
            .expect("competing opener thread")
            .expect("competing open");
        assert!(
            write_reserve_path(competing.path()).exists(),
            "reserve refill is allowed after recovery completes"
        );
        assert_eq!(
            competing
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = 'reserve_competing_open'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("read retried write"),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_reserve_lock_contention_is_bounded() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        use std::time::Duration;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("uncontended open must refill write reserve");
        let lock_path = write_reserve_lock_path(&db_path);
        let holder = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .mode(0o600)
            .open(lock_path)
            .expect("open write-reserve lock");
        // SAFETY: `holder` keeps this valid descriptor open for the call.
        assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX) }, 0);

        let (result_tx, result_rx) = sync_channel(1);
        let started = Instant::now();
        let contender = thread::spawn(move || {
            let result =
                db.with_write_reserve_retry("test lock contention", || Ok::<_, DbError>(()));
            result_tx
                .send((started.elapsed(), result))
                .expect("send lock result");
        });
        let received = result_rx.recv_timeout(Duration::from_secs(6));
        drop(holder);
        contender.join().expect("contender thread");

        let (elapsed, result) = received.expect("bounded write-reserve lock acquisition");
        assert!(
            elapsed < Duration::from_secs(6),
            "lock wait took {elapsed:?}"
        );
        let error = result.expect_err("contended write-reserve lock must time out");
        let DbError::WriteReserveLock { source, .. } = error else {
            panic!("expected typed write-reserve lock error");
        };
        assert_eq!(source.kind(), std::io::ErrorKind::TimedOut);
        Database::open(&db_path).expect("uncontended open after release");
    }

    #[cfg(unix)]
    #[test]
    fn reserve_namespace_transitions_sync_parent_after_publication() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let reserve_path = write_reserve_path(&db_path);
        let take_sync_count =
            || crate::ingress_spool::SYNC_DIRECTORY_CALLS.with(|calls| calls.get());
        let reset_sync_count =
            || crate::ingress_spool::SYNC_DIRECTORY_CALLS.with(|calls| calls.set(0));

        reset_sync_count();
        ensure_write_reserve(&db_path).expect("create reserve");
        let created = take_sync_count();
        reset_sync_count();
        assert!(consume_write_reserve(&db_path, "test namespace sync"));
        let removed = take_sync_count();
        reset_sync_count();
        ensure_write_reserve(&db_path).expect("refill reserve");
        let refilled = take_sync_count();

        assert_eq!(created, 1);
        assert_eq!(removed, 1);
        assert_eq!(refilled, 1);
        assert!(reserve_path.exists());
    }
}
