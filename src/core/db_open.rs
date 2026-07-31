//! Database construction and profile-admission boundary.

use super::*;

impl Database {
    pub fn open(path: &Path) -> Result<Self, DbError> {
        Self::open_with_mode(path, OpenMode::ReadWrite, true)
    }

    /// Open a read-write connection with a caller-selected SQLite busy timeout.
    pub fn open_with_busy_timeout(path: &Path, busy_timeout: Duration) -> Result<Self, DbError> {
        Self::open_with_mode_and_busy_timeout(path, OpenMode::ReadWrite, busy_timeout, true)
    }

    pub fn open_read_only(path: &Path) -> Result<Self, DbError> {
        Self::open_with_mode(path, OpenMode::ReadOnly, true)
    }

    /// Run a strictly read-only diagnostic operation without consuming a
    /// profile holder slot. The connection closes before this method returns.
    pub fn with_diagnostic_read_only<T>(
        path: &Path,
        operation: impl FnOnce(&Self) -> T,
    ) -> Result<T, DbError> {
        let path = super::super::db_admission::ProfileDbAdmission::resolve_database_path(path)?;
        let database = Self::open_with_mode(&path, OpenMode::ReadOnly, false)?;
        Ok(operation(&database))
    }

    /// Open a non-mutating connection without startup writes or migrations.
    pub fn open_query_only(path: &Path) -> Result<Self, DbError> {
        Self::open_with_mode(path, OpenMode::QueryOnly, true)
    }

    /// Open a non-mutating connection with a caller-selected busy timeout.
    pub fn open_query_only_with_busy_timeout(
        path: &Path,
        busy_timeout: Duration,
    ) -> Result<Self, DbError> {
        Self::open_with_mode_and_busy_timeout(path, OpenMode::QueryOnly, busy_timeout, true)
    }

    /// Open a database for lease control (heartbeat/release) WITHOUT acquiring
    /// profile admission. This avoids consuming an admission slot at holder cap.
    pub fn open_lease_control(path: &Path) -> Result<Self, DbError> {
        Self::open_with_mode(path, OpenMode::ReadWrite, false)
    }

    /// Open a database for lease control with a custom busy timeout,
    /// WITHOUT acquiring profile admission.
    pub(crate) fn open_lease_control_with_timeout(
        path: &Path,
        busy_timeout: Duration,
    ) -> Result<Self, DbError> {
        Self::open_with_mode_and_busy_timeout(path, OpenMode::ReadWrite, busy_timeout, false)
    }

    pub(crate) fn open_unadmitted(path: &Path) -> Result<Self, DbError> {
        Self::open_with_mode(path, OpenMode::ReadWrite, false)
    }

    /// Open an unadmitted read-write connection with a bounded SQLite busy wait.
    pub(crate) fn open_unadmitted_with_busy_timeout(
        path: &Path,
        busy_timeout: Duration,
    ) -> Result<Self, DbError> {
        Self::open_with_mode_and_busy_timeout(path, OpenMode::ReadWrite, busy_timeout, false)
    }

    pub(crate) fn open_query_only_unadmitted(path: &Path) -> Result<Self, DbError> {
        Self::open_with_mode(path, OpenMode::QueryOnly, false)
    }

    /// Open an unadmitted query-only connection with a bounded SQLite busy wait.
    pub(crate) fn open_query_only_unadmitted_with_busy_timeout(
        path: &Path,
        busy_timeout: Duration,
    ) -> Result<Self, DbError> {
        Self::open_with_mode_and_busy_timeout(path, OpenMode::QueryOnly, busy_timeout, false)
    }

    fn open_with_mode(path: &Path, mode: OpenMode, admitted: bool) -> Result<Self, DbError> {
        Self::open_with_mode_and_busy_timeout(path, mode, Duration::from_secs(5), admitted)
    }

    pub(super) fn open_with_mode_and_busy_timeout(
        path: &Path,
        mode: OpenMode,
        busy_timeout: Duration,
        admitted: bool,
    ) -> Result<Self, DbError> {
        let admission = admitted
            .then(|| {
                super::super::db_admission::ProfileDbAdmission::acquire(
                    path,
                    super::super::db_admission::DbAdmissionRequest::new(
                        super::super::db_admission::DbHolderClass::current_process(),
                        1,
                        (-SQLITE_CACHE_SIZE_KIB_DEFAULT as u64) * 1024,
                    ),
                )
            })
            .transpose()?;
        // Admitted opens use the admission-resolved path. Unadmitted opens
        // (lease-control) must open a non-symlink path: either a real file or
        // the canonical path returned by Database::path() after admitted open.
        // Reject caller-provided symlinks fail-closed before SQLite open.
        if !admitted
            && path
                .symlink_metadata()
                .map(|meta| meta.file_type().is_symlink())
                .unwrap_or(false)
        {
            return Err(DbError::SymlinkDatabasePath {
                path: path.to_path_buf(),
            });
        }
        let sqlite_path = admission.as_ref().map_or_else(
            || path.to_path_buf(),
            |guard| guard.database_path().to_path_buf(),
        );
        if mode.allows_write() {
            if let Some(parent) = sqlite_path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent).map_err(|source| DbError::CreateDir {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
        }

        register_sqlite_vec()?;
        let conn = match mode {
            OpenMode::ReadOnly => Connection::open_with_flags(
                &sqlite_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?,
            OpenMode::QueryOnly => Connection::open_with_flags(
                &sqlite_path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?,
            OpenMode::ReadWrite => Connection::open_with_flags(
                &sqlite_path,
                OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?,
        };
        conn.busy_timeout(busy_timeout)?;
        conn.pragma_update(None, "cache_size", SQLITE_CACHE_SIZE_KIB_DEFAULT)?;
        register_math_functions(&conn)?;
        if !mode.allows_write() {
            conn.pragma_update(None, "query_only", "ON")?;
            ensure_supported_schema_version(&conn)?;
        }
        if mode.allows_write() {
            ensure_wal_journal_mode(&conn)?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.execute_batch("PRAGMA foreign_keys = ON;")?;
            apply_migrations(&conn)?;
            db_fork_ext::apply_fork_ext_migrations(&conn)?;
        }
        Ok(Self {
            conn,
            path: sqlite_path,
            _admission: admission,
        })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db_admission::{DbAdmissionRequest, DbHolderClass, ProfileDbAdmission};

    fn short_tempdir() -> tempfile::TempDir {
        tempfile::TempDir::new_in("/tmp").expect("short tempdir")
    }

    #[test]
    fn db_admission_lease_control_opens_do_not_acquire_profile_admission() {
        let tempdir = short_tempdir();
        let db_path = tempdir.path().join("palace.db");
        let _admitted = Database::open(&db_path).expect("open admitted database");
        let before = ProfileDbAdmission::snapshot(&db_path).expect("snapshot before lease control");

        let _lease_control =
            Database::open_lease_control(&db_path).expect("open lease-control database");
        let _lease_control_with_timeout =
            Database::open_lease_control_with_timeout(&db_path, Duration::from_millis(25))
                .expect("open lease-control database with timeout");

        let after = ProfileDbAdmission::snapshot(&db_path).expect("snapshot after lease control");
        assert_eq!(after.active_holders, before.active_holders);
        assert_eq!(after.holders, before.holders);
    }

    #[test]
    fn diagnostic_operation_closes_without_consuming_an_admission_slot() {
        let tempdir = short_tempdir();
        let db_path = tempdir.path().join("palace.db");
        let _admitted = Database::open(&db_path).expect("open admitted database");
        let before = ProfileDbAdmission::snapshot(&db_path).expect("snapshot before diagnostic");

        let table_count = Database::with_diagnostic_read_only(&db_path, |database| {
            database
                .conn()
                .query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| {
                    row.get::<_, i64>(0)
                })
        })
        .expect("open diagnostic operation")
        .expect("run diagnostic query");

        let after = ProfileDbAdmission::snapshot(&db_path).expect("snapshot after diagnostic");
        assert!(table_count >= 0);
        assert_eq!(after.active_holders, before.active_holders);
        assert_eq!(after.holders, before.holders);
    }

    #[test]
    fn db_admission_canonical_path_survives_symbolic_link_retarget() {
        use std::os::unix::fs::symlink;

        let tempdir = short_tempdir();
        let first_target = tempdir.path().join("first.db");
        let second_target = tempdir.path().join("second.db");
        let link_path = tempdir.path().join("link.db");
        Connection::open(&first_target).expect("create first target database");
        Connection::open(&second_target).expect("create second target database");
        symlink(&first_target, &link_path).expect("create first database symlink");
        let admitted_path = first_target
            .canonicalize()
            .expect("canonical first target database path");

        let admission = ProfileDbAdmission::acquire(
            &link_path,
            DbAdmissionRequest::new(DbHolderClass::Cli, 1, 1024),
        )
        .expect("admit first symlink target");
        fs::remove_file(&link_path).expect("remove first database symlink");
        symlink(&second_target, &link_path).expect("retarget database symlink");

        assert_eq!(admission.database_path(), admitted_path);
    }

    #[test]
    fn db_open_admitted_uses_canonical_path_for_symbolic_link() {
        use std::os::unix::fs::symlink;

        let tempdir = short_tempdir();
        let target_path = tempdir.path().join("target.db");
        let link_path = tempdir.path().join("link.db");
        Connection::open(&target_path).expect("create target database");
        symlink(&target_path, &link_path).expect("create database symlink");

        let database = Database::open(&link_path).expect("open admitted canonical database path");

        assert_eq!(
            database.path(),
            target_path
                .canonicalize()
                .expect("canonical target database path")
        );
    }

    #[test]
    fn db_open_unadmitted_rejects_symbolic_link() {
        use std::os::unix::fs::symlink;

        let tempdir = short_tempdir();
        let target_path = tempdir.path().join("target.db");
        let link_path = tempdir.path().join("link.db");
        Connection::open(&target_path).expect("create target database");
        symlink(&target_path, &link_path).expect("create database symlink");

        let err = match Database::open_unadmitted(&link_path) {
            Ok(_) => panic!("unadmitted open must reject symlink"),
            Err(error) => error,
        };
        assert!(
            matches!(err, DbError::SymlinkDatabasePath { .. }),
            "expected SymlinkDatabasePath, got {err}"
        );
    }

    #[test]
    fn db_open_lease_control_rejects_symbolic_link() {
        use std::os::unix::fs::symlink;

        let tempdir = short_tempdir();
        let target_path = tempdir.path().join("target.db");
        let link_path = tempdir.path().join("link.db");
        Connection::open(&target_path).expect("create target database");
        symlink(&target_path, &link_path).expect("create database symlink");

        let err = match Database::open_lease_control(&link_path) {
            Ok(_) => panic!("lease-control open must reject symlink"),
            Err(error) => error,
        };
        assert!(
            matches!(err, DbError::SymlinkDatabasePath { .. }),
            "expected SymlinkDatabasePath, got {err}"
        );
    }

    #[test]
    fn db_open_lease_control_accepts_canonical_path_from_admitted_open() {
        use std::os::unix::fs::symlink;

        let tempdir = short_tempdir();
        let target_path = tempdir.path().join("target.db");
        let link_path = tempdir.path().join("link.db");
        Connection::open(&target_path).expect("create target database");
        symlink(&target_path, &link_path).expect("create database symlink");

        let admitted = Database::open(&link_path).expect("admitted open via symlink");
        let canonical = admitted.path().to_path_buf();
        drop(admitted);

        Database::open_lease_control(&canonical)
            .expect("lease-control must accept canonical path from Database::path()");
    }
}
