//! Database construction and profile-admission boundary.

use super::*;

fn ensure_supported_fork_ext_version(conn: &Connection) -> Result<u32, DbError> {
    let current_version = read_fork_ext_version(conn)?;
    if current_version > CURRENT_FORK_EXT_VERSION {
        return Err(DbError::UnsupportedForkExtVersion {
            current: current_version,
            supported: CURRENT_FORK_EXT_VERSION,
        });
    }
    Ok(current_version)
}

const V5_REPAIR_SCHEMA_OBJECTS: &[&str] = &["idx_drawers_content_hash"];
const V11_DRAWER_COLUMNS: &[&str] = &["source_type", "confidence"];
const V12_DRAWER_COLUMNS: &[&str] = &["compacted_into"];
const V12_REPAIR_SCHEMA_OBJECTS: &[&str] = &[
    "idx_drawers_compacted_into",
    "consolidation_log",
    "idx_consolidation_log_created_at",
    "idx_consolidation_log_scope",
];
const V13_DRAWER_COLUMNS: &[&str] = &["is_pinned", "pin_order", "supersedes"];
const V13_REPAIR_SCHEMA_OBJECTS: &[&str] = &["idx_drawers_pinned", "idx_drawers_supersedes"];
const V14_DRAWER_COLUMNS: &[&str] = &["consolidation_priority", "last_sleep_at"];
const V14_REPAIR_SCHEMA_OBJECTS: &[&str] = &[
    "idx_drawers_consolidation_priority",
    "idx_drawers_last_sleep_at",
    "sleep_log",
    "idx_sleep_log_created_at",
    "sleep_resolution_log",
    "idx_sleep_resolution_log_created_at",
];
const V15_KNOWLEDGE_CARD_COLUMNS: &[&str] = &[
    "auto_generated",
    "crystallization_score",
    "source_drawer_ids",
];
const V15_REPAIR_SCHEMA_OBJECTS: &[&str] = &["idx_knowledge_cards_auto_pending"];

fn schema_repairs_required(conn: &Connection) -> Result<bool, DbError> {
    let drawer_columns = drawers_column_names(conn)?;
    let schema_objects = schema_object_names(conn)?;
    let drawers_sql = table_sql(conn, "drawers")?;

    if V5_DRAWER_COLUMN_MIGRATIONS
        .iter()
        .any(|column| !drawer_columns.contains(column.name))
        || required_names_missing(&schema_objects, V5_REPAIR_SCHEMA_OBJECTS)
        || required_names_missing(&drawer_columns, V11_DRAWER_COLUMNS)
        || !drawers_source_type_check_is_current(&drawers_sql)
        || required_names_missing(&drawer_columns, V12_DRAWER_COLUMNS)
        || required_names_missing(&schema_objects, V12_REPAIR_SCHEMA_OBJECTS)
        || required_names_missing(&drawer_columns, V13_DRAWER_COLUMNS)
        || schema_sql_requires_rewrite(&drawers_sql, V13_TYPED_INGEST_CHECK_REPLACEMENTS)
        || required_names_missing(&schema_objects, V13_REPAIR_SCHEMA_OBJECTS)
        || required_names_missing(&drawer_columns, V14_DRAWER_COLUMNS)
        || required_names_missing(&schema_objects, V14_REPAIR_SCHEMA_OBJECTS)
    {
        return Ok(true);
    }

    // V15 only repairs the V8 table when that table already exists.
    if !schema_objects.contains("knowledge_cards") {
        return Ok(false);
    }
    let knowledge_card_columns = table_column_names(conn, "knowledge_cards")?;
    let knowledge_cards_sql = table_sql(conn, "knowledge_cards")?;
    Ok(
        required_names_missing(&knowledge_card_columns, V15_KNOWLEDGE_CARD_COLUMNS)
            || required_names_missing(&schema_objects, V15_REPAIR_SCHEMA_OBJECTS)
            || schema_sql_requires_rewrite(&knowledge_cards_sql, V15_STATUS_CHECK_REPLACEMENTS),
    )
}

fn required_names_missing(existing: &HashSet<String>, required: &[&str]) -> bool {
    required.iter().any(|name| !existing.contains(*name))
}

fn schema_sql_requires_rewrite(table_sql: &str, replacements: &[(&str, &str)]) -> bool {
    replacements
        .iter()
        .any(|(legacy, _)| table_sql.contains(legacy))
}

pub(super) fn table_sql(conn: &Connection, table_name: &str) -> Result<String, DbError> {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table_name],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

pub(super) fn drawers_source_type_check_is_current(table_sql: &str) -> bool {
    [
        "user_explicit",
        "agent_observation",
        "agent_inference",
        "system_generated",
    ]
    .iter()
    .all(|source_type| table_sql.contains(source_type))
}

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
        let schema_version = ensure_supported_schema_version(&conn)?;
        let fork_ext_version = ensure_supported_fork_ext_version(&conn)?;
        conn.pragma_update(None, "cache_size", SQLITE_CACHE_SIZE_KIB_DEFAULT)?;
        register_math_functions(&conn)?;
        if !mode.allows_write() {
            conn.pragma_update(None, "query_only", "ON")?;
        }
        if mode.allows_write() {
            ensure_wal_journal_mode(&conn)?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.execute_batch("PRAGMA foreign_keys = ON;")?;
            if schema_version < CURRENT_SCHEMA_VERSION || schema_repairs_required(&conn)? {
                apply_migrations(&conn)?;
            }
            if fork_ext_version < CURRENT_FORK_EXT_VERSION {
                db_fork_ext::apply_fork_ext_migrations(&conn)?;
            }
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

    #[test]
    fn db_open_applies_busy_timeout_before_schema_queries() {
        let tempdir = short_tempdir();
        let db_path = tempdir.path().join("palace.db");
        drop(Database::open(&db_path).expect("initialize database"));

        let blocker = Connection::open(&db_path).expect("open lock holder");
        blocker
            .pragma_update(None, "journal_mode", "DELETE")
            .expect("select delete journal mode");
        blocker
            .execute_batch("BEGIN EXCLUSIVE;")
            .expect("hold exclusive schema lock");

        let (opened_tx, opened_rx) = std::sync::mpsc::channel();
        let open_path = db_path.clone();
        let opener = std::thread::spawn(move || {
            let _ = opened_tx.send(Database::open_with_busy_timeout(
                &open_path,
                Duration::from_millis(25),
            ));
        });

        let opened = opened_rx.recv_timeout(Duration::from_millis(250));
        blocker
            .execute_batch("COMMIT;")
            .expect("release schema lock");
        opener.join().expect("join database opener");
        assert!(
            opened
                .expect("caller-selected timeout must bound schema query")
                .is_err(),
            "exclusive schema lock must outlast the caller-selected busy timeout"
        );
    }

    #[test]
    fn current_schema_db_open_does_not_reapply_migrations_under_live_writer() {
        let tempdir = short_tempdir();
        let db_path = tempdir.path().join("palace.db");
        drop(Database::open(&db_path).expect("initialize current database"));

        let blocker = Connection::open(&db_path).expect("open same-version writer");
        blocker
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("hold same-version write transaction");
        let opened = Database::open_with_busy_timeout(&db_path, Duration::from_millis(25));
        blocker
            .execute_batch("ROLLBACK;")
            .expect("release same-version writer");

        opened.expect("current schema open must not request a SQLite write lock");
    }

    #[test]
    fn current_schema_db_open_repairs_all_legacy_structural_invariants() {
        for (repair, damage_sql, invariant_sql) in [
            (
                "V5 drawer metadata",
                "DROP INDEX idx_drawers_content_hash; ALTER TABLE drawers DROP COLUMN content_hash;",
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('drawers') WHERE name = 'content_hash')",
            ),
            (
                "V11 source confidence",
                "ALTER TABLE drawers DROP COLUMN confidence;",
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('drawers') WHERE name = 'confidence')",
            ),
            (
                "V12 compaction",
                "DROP TABLE consolidation_log;",
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'consolidation_log')",
            ),
            (
                "V13 typed pinned",
                "DROP INDEX idx_drawers_pinned;",
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'idx_drawers_pinned')",
            ),
            (
                "V14 sleep",
                "DROP TABLE sleep_log;",
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'sleep_log')",
            ),
            (
                "V15 crystallization",
                "ALTER TABLE knowledge_cards DROP COLUMN crystallization_score;",
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('knowledge_cards') WHERE name = 'crystallization_score')",
            ),
        ] {
            let tempdir = short_tempdir();
            let db_path = tempdir.path().join("palace.db");
            drop(Database::open(&db_path).expect("initialize current database"));
            let conn = Connection::open(&db_path).expect("open fixture database");
            conn.execute_batch(damage_sql)
                .unwrap_or_else(|error| panic!("damage {repair} fixture: {error}"));
            drop(conn);

            let repaired = Database::open(&db_path)
                .unwrap_or_else(|error| panic!("repair {repair} fixture: {error}"));
            let satisfied = repaired
                .conn()
                .query_row(invariant_sql, [], |row| row.get::<_, bool>(0))
                .unwrap_or_else(|error| panic!("validate {repair} fixture: {error}"));
            assert!(satisfied, "{repair} invariant was not repaired");
        }
    }

    #[test]
    fn future_fork_ext_version_is_rejected_before_writable_pragmas() {
        let tempdir = short_tempdir();
        let db_path = tempdir.path().join("palace.db");
        drop(Database::open(&db_path).expect("initialize database"));

        let conn = Connection::open(&db_path).expect("open fixture database");
        conn.pragma_update(None, "journal_mode", "DELETE")
            .expect("select delete journal mode");
        let future_version = CURRENT_FORK_EXT_VERSION + 1;
        set_fork_ext_version(&conn, future_version).expect("set future fork extension version");
        drop(conn);
        let before = fs::read(&db_path).expect("snapshot database before rejected opens");

        for (label, open) in [
            (
                "read-write",
                Database::open as fn(&Path) -> Result<Database, DbError>,
            ),
            ("query-only", Database::open_query_only),
        ] {
            let error = match open(&db_path) {
                Ok(_) => panic!("{label} open must reject a future fork extension schema"),
                Err(error) => error,
            };
            assert!(
                matches!(
                    &error,
                    DbError::UnsupportedForkExtVersion { current, supported }
                        if *current == future_version
                            && *supported == CURRENT_FORK_EXT_VERSION
                ),
                "{label} open returned the wrong error: {error}"
            );
            assert_eq!(
                fs::read(&db_path).expect("snapshot database after rejected open"),
                before,
                "{label} rejection must not change the database"
            );
        }

        let conn = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .expect("open rejected database read-only");
        let journal_mode = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .expect("read journal mode");
        assert_eq!(journal_mode.to_ascii_lowercase(), "delete");
        assert_eq!(
            read_fork_ext_version(&conn).expect("read preserved fork extension version"),
            future_version
        );
        assert!(!db_path.with_extension("db-wal").exists());
        assert!(!db_path.with_extension("db-shm").exists());
    }
}
