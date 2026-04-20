use mempal::core::db::Database;
use rusqlite::Connection;
use tempfile::TempDir;

#[path = "../src/core/db_fork_ext.rs"]
mod db_fork_ext;

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
fn test_fork_ext_version_is_zero_on_fresh_db() {
    let (_tmp, _db_path, db) = new_test_db();

    let version = db_fork_ext::read_fork_ext_version(db.conn()).expect("read fork-ext version");

    assert_eq!(version, 0);
}

#[test]
fn test_fork_ext_migrations_idempotent() {
    let (_tmp, _db_path, db) = new_test_db();

    db_fork_ext::apply_fork_ext_migrations(db.conn()).expect("first apply");
    db_fork_ext::apply_fork_ext_migrations(db.conn()).expect("second apply");

    let version = db_fork_ext::read_fork_ext_version(db.conn()).expect("read fork-ext version");
    assert_eq!(version, 0);
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
