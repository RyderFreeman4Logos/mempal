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

#[cfg(test)]
fn db_open_busy_fixture_lock() -> &'static std::sync::Mutex<()> {
    // ponytail: one process-global fixture lock; split by contention domain if throughput matters.
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(std::sync::Mutex::default)
}

#[cfg(test)]
#[path = "db_open_tests_schema_repair_1003.rs"]
mod schema_repair_busy_1003_tests;

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
        let mut admission = admitted
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
            if admitted {
                ensure_write_reserve_logged(&sqlite_path);
            }
        }
        register_sqlite_vec()?;
        let mut open = || {
            #[cfg(test)]
            super::db_write_reserve::fail_write_reserve_retry_with_sqlite_full_for_test(
                &sqlite_path,
            )?;
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
                path: sqlite_path.clone(),
                _admission: admission.take(),
            })
        };
        if admitted && mode.allows_write() {
            super::db_write_reserve::with_write_reserve_retry(
                &sqlite_path,
                "database bootstrap",
                open,
            )
        } else {
            open()
        }
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
    use super::super::db_write_reserve::{WRITE_RESERVE_BYTES, write_reserve_path};
    use super::*;
    use crate::core::db_admission::{DbAdmissionRequest, DbHolderClass, ProfileDbAdmission};
    use crate::ingress_spool::AppendOutcome;

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

    include!("db_open_write_reserve_tests.rs");

    #[test]
    fn current_schema_db_open_repairs_all_legacy_structural_invariants() {
        let _fixture_guard = super::db_open_busy_fixture_lock()
            .lock()
            .expect("serialize database busy fixtures");
        let mut columns = V5_DRAWER_COLUMN_MIGRATIONS
            .iter()
            .map(|column| ("V5", "drawers", column.name))
            .collect::<Vec<_>>();
        columns.extend(
            [
                ("V11", "drawers", V11_DRAWER_COLUMNS),
                ("V12", "drawers", V12_DRAWER_COLUMNS),
                ("V13", "drawers", V13_DRAWER_COLUMNS),
                ("V14", "drawers", V14_DRAWER_COLUMNS),
                ("V15", "knowledge_cards", V15_KNOWLEDGE_CARD_COLUMNS),
            ]
            .into_iter()
            .flat_map(|(version, table, columns)| {
                columns.iter().map(move |&column| (version, table, column))
            }),
        );
        let mut repairs = columns
            .into_iter()
            .map(|(version, table, column)| {
                let dependent_indexes = match column {
                    "content_hash" => "DROP INDEX idx_drawers_content_hash; ",
                    "compacted_into" => {
                        "DROP INDEX idx_drawers_compacted_into; DROP INDEX idx_drawers_consolidation_priority; "
                    }
                    "is_pinned" | "pin_order" => "DROP INDEX idx_drawers_pinned; ",
                    "supersedes" => "DROP INDEX idx_drawers_supersedes; ",
                    "consolidation_priority" => {
                        "DROP INDEX idx_drawers_consolidation_priority; "
                    }
                    "last_sleep_at" => "DROP INDEX idx_drawers_last_sleep_at; ",
                    "auto_generated" => "DROP INDEX idx_knowledge_cards_auto_pending; ",
                    _ => "",
                };
                (
                    format!("{version} {table}.{column}"),
                    format!("{dependent_indexes}ALTER TABLE {table} DROP COLUMN {column};"),
                )
            })
            .collect::<Vec<_>>();
        repairs.extend(
            [
                ("V5", V5_REPAIR_SCHEMA_OBJECTS),
                ("V12", V12_REPAIR_SCHEMA_OBJECTS),
                ("V13", V13_REPAIR_SCHEMA_OBJECTS),
                ("V14", V14_REPAIR_SCHEMA_OBJECTS),
                ("V15", V15_REPAIR_SCHEMA_OBJECTS),
            ]
            .into_iter()
            .flat_map(|(version, objects)| {
                objects.iter().map(move |&object| {
                    let kind = if matches!(
                        object,
                        "consolidation_log" | "sleep_log" | "sleep_resolution_log"
                    ) {
                        "TABLE"
                    } else {
                        "INDEX"
                    };
                    (
                        format!("{version} schema object {object}"),
                        format!("DROP {kind} {object};"),
                    )
                })
            }),
        );
        assert_eq!(repairs.len(), 42);
        for (repair, damage_sql) in repairs {
            let tempdir = short_tempdir();
            let db_path = tempdir.path().join("palace.db");
            drop(Database::open(&db_path).expect("initialize current database"));
            let conn = Connection::open(&db_path).expect("open fixture database");
            conn.execute_batch(&damage_sql)
                .expect("damage structural repair fixture");
            assert!(
                schema_repairs_required(&conn).expect("inspect damaged structural fixture"),
                "{repair} fixture must require repair"
            );
            drop(conn);
            let repaired = Database::open(&db_path)
                .unwrap_or_else(|error| panic!("repair {repair} fixture: {error}"));
            assert!(
                !schema_repairs_required(repaired.conn()).expect("validate structural repair"),
                "{repair} invariant was not repaired"
            );
        }

        let mut check_repairs = vec![(
            "V11 drawers source_type CHECK".to_owned(),
            "drawers",
            "source_type TEXT NOT NULL CHECK(source_type IN ('project', 'conversation', 'manual'))",
            "source_type TEXT NOT NULL DEFAULT 'system_generated' CHECK(source_type IN ('user_explicit', 'agent_observation', 'agent_inference', 'system_generated'))",
        )];
        check_repairs.extend(V13_TYPED_INGEST_CHECK_REPLACEMENTS.iter().enumerate().map(
            |(index, &(legacy, current))| {
                let current = if legacy.starts_with("memory_kind ") {
                    "memory_kind TEXT NOT NULL CHECK(memory_kind IN ('evidence', 'knowledge', 'atomic_fact', 'decision', 'case', 'skill', 'foresight', 'profile_fact', 'profile_trait')) DEFAULT 'evidence'"
                } else {
                    current
                };
                (
                    format!("V13 drawers CHECK replacement {index}: {legacy}"),
                    "drawers",
                    legacy,
                    current,
                )
            },
        ));
        check_repairs.extend(V15_STATUS_CHECK_REPLACEMENTS.iter().enumerate().map(
            |(index, &(legacy, current))| {
                (
                    format!("V15 knowledge_cards CHECK replacement {index}: {legacy}"),
                    "knowledge_cards",
                    legacy,
                    current,
                )
            },
        ));
        assert_eq!(check_repairs.len(), 13);
        for (repair, table, legacy, current) in check_repairs {
            let tempdir = short_tempdir();
            let db_path = tempdir.path().join("palace.db");
            drop(Database::open(&db_path).expect("initialize current database"));
            let conn = Connection::open(&db_path).expect("open fixture database");
            conn.pragma_update(None, "writable_schema", "ON")
                .expect("enable writable schema");
            conn.execute(
                "UPDATE sqlite_master SET sql = replace(sql, ?1, ?2) WHERE type = 'table' AND name = ?3",
                rusqlite::params![current, legacy, table],
            )
            .expect("damage table SQL fixture");
            conn.pragma_update(None, "writable_schema", "OFF")
                .expect("disable writable schema");
            let damaged_table_sql = table_sql(&conn, table).expect("inspect damaged table SQL");
            assert!(
                damaged_table_sql.contains(legacy),
                "{repair} fixture did not install the legacy SQL: {damaged_table_sql}"
            );
            assert!(
                schema_repairs_required(&conn).expect("inspect damaged table SQL fixture"),
                "{repair} fixture must require repair"
            );
            drop(conn);
            let repaired = Database::open(&db_path)
                .unwrap_or_else(|error| panic!("repair {repair} fixture: {error}"));
            assert!(
                !schema_repairs_required(repaired.conn()).expect("validate table SQL repair"),
                "{repair} invariant was not repaired"
            );
        }
    }

    #[test]
    fn future_user_version_open_modes_fail_before_write_in_delete_mode() {
        let tempdir = short_tempdir();
        let db_path = tempdir.path().join("palace.db");
        let wal_path = db_path.with_extension("db-wal");
        drop(Database::open(&db_path).expect("initialize database"));
        let conn = Connection::open(&db_path).expect("open fixture database");
        conn.pragma_update(None, "journal_mode", "DELETE")
            .expect("select delete journal mode");
        let future_version = CURRENT_SCHEMA_VERSION + 1;
        conn.pragma_update(None, "user_version", future_version)
            .expect("set future user version");
        drop(conn);
        let before_db = fs::read(&db_path).expect("snapshot database before rejected opens");
        let before_wal = fs::read(&wal_path).ok();
        assert!(before_wal.is_none(), "DELETE mode must not retain a WAL");

        for (label, open) in [
            (
                "read-write",
                Database::open as fn(&Path) -> Result<Database, DbError>,
            ),
            ("query-only", Database::open_query_only),
        ] {
            let error = match open(&db_path) {
                Ok(_) => panic!("{label} open must reject a future user version"),
                Err(error) => error,
            };
            assert!(
                matches!(
                    &error,
                    DbError::UnsupportedSchemaVersion { current, supported }
                        if *current == future_version && *supported == CURRENT_SCHEMA_VERSION
                ),
                "{label} open returned the wrong error: {error}"
            );
            assert_eq!(
                fs::read(&db_path).expect("snapshot database after rejected open"),
                before_db,
                "{label} rejection must not change the database"
            );
            assert_eq!(
                fs::read(&wal_path).ok(),
                before_wal,
                "{label} rejection must not change WAL bytes"
            );
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
