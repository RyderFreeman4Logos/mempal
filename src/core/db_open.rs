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

    pub(crate) fn open_query_only_unadmitted(path: &Path) -> Result<Self, DbError> {
        Self::open_with_mode(path, OpenMode::QueryOnly, false)
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
        if mode.allows_write() {
            if let Some(parent) = path
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
            OpenMode::ReadOnly => {
                Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?
            }
            OpenMode::QueryOnly => {
                Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?
            }
            OpenMode::ReadWrite => Connection::open(path)?,
        };
        conn.busy_timeout(busy_timeout)?;
        conn.pragma_update(None, "cache_size", SQLITE_CACHE_SIZE_KIB_DEFAULT)?;
        register_math_functions(&conn)?;
        if matches!(mode, OpenMode::QueryOnly) {
            conn.pragma_update(None, "query_only", "ON")?;
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
            path: path.to_path_buf(),
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
    use crate::core::db_admission::ProfileDbAdmission;

    #[test]
    fn db_admission_lease_control_opens_do_not_acquire_profile_admission() {
        let tempdir = tempfile::tempdir().expect("tempdir");
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
}
