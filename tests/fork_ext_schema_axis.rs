use mempal::core::db::{Database, apply_fork_ext_migrations, read_fork_ext_version};
use rusqlite::Connection;
use tempfile::TempDir;

fn new_test_db() -> (TempDir, std::path::PathBuf, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    (tmp, db_path, db)
}

fn current_schema_version() -> u32 {
    let source = include_str!("../src/core/db.rs");
    let version_line = source
        .lines()
        .find(|line| line.contains("const CURRENT_SCHEMA_VERSION"))
        .expect("CURRENT_SCHEMA_VERSION line");
    let version = version_line
        .split('=')
        .nth(1)
        .expect("version assignment")
        .trim()
        .trim_end_matches(';');

    version.parse().expect("CURRENT_SCHEMA_VERSION value")
}

#[test]
fn test_fork_ext_version_is_six_after_audit_project_scope_phase() {
    let (_tmp, _db_path, db) = new_test_db();

    let version = read_fork_ext_version(db.conn()).expect("read fork-ext version");

    assert_eq!(version, 6);
}

#[test]
fn test_fork_ext_migrations_idempotent() {
    let (_tmp, _db_path, db) = new_test_db();

    apply_fork_ext_migrations(db.conn()).expect("first apply");
    apply_fork_ext_migrations(db.conn()).expect("second apply");

    let version = read_fork_ext_version(db.conn()).expect("read fork-ext version");
    assert_eq!(version, 6);
}

#[test]
fn test_upstream_user_version_preserved_after_fork_ext_init() {
    let (_tmp, db_path, _db) = new_test_db();
    let conn = Connection::open(db_path).expect("open sqlite connection");

    let user_version = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
        .expect("read user_version");

    assert_eq!(user_version, current_schema_version());
}

#[test]
fn test_fork_ext_meta_table_exists_after_init() {
    let (_tmp, _db_path, db) = new_test_db();

    let exists = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='fork_ext_meta'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query sqlite_master");

    assert_eq!(exists, 1);
}

#[test]
fn test_gating_audit_table_exists_after_ext_v3() {
    let (_tmp, _db_path, db) = new_test_db();

    let exists = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='gating_audit'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query sqlite_master");

    assert_eq!(exists, 1);
}

#[test]
fn test_novelty_audit_table_exists_after_ext_v4() {
    let (_tmp, _db_path, db) = new_test_db();

    let exists = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='novelty_audit'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query sqlite_master");

    assert_eq!(exists, 1);
}

#[test]
fn test_audit_tables_store_project_scope_after_ext_v6() {
    let (_tmp, _db_path, db) = new_test_db();

    let gating_columns = db
        .conn()
        .prepare("PRAGMA table_info(gating_audit)")
        .expect("prepare gating pragma")
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query gating pragma")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect gating columns");
    assert!(gating_columns.iter().any(|name| name == "project_id"));

    let novelty_columns = db
        .conn()
        .prepare("PRAGMA table_info(novelty_audit)")
        .expect("prepare novelty pragma")
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query novelty pragma")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect novelty columns");
    assert!(novelty_columns.iter().any(|name| name == "project_id"));
}
