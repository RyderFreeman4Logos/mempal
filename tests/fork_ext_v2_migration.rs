use mempal::core::db::Database;
use tempfile::TempDir;

#[path = "../src/core/db_fork_ext.rs"]
mod db_fork_ext;

fn new_test_db() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    (tmp, db)
}

#[test]
fn test_fork_ext_v4_migration() {
    let (_tmp, db) = new_test_db();

    let version = db_fork_ext::read_fork_ext_version(db.conn()).expect("read version");
    assert_eq!(version, 5);

    let table_exists = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='reindex_progress'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query sqlite_master");
    assert_eq!(table_exists, 1);

    let gating_exists = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='gating_audit'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query sqlite_master");
    assert_eq!(gating_exists, 1);

    let novelty_exists = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='novelty_audit'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query sqlite_master");
    assert_eq!(novelty_exists, 1);
}

#[test]
fn test_fork_ext_v4_migration_is_idempotent() {
    let (_tmp, db) = new_test_db();

    db_fork_ext::apply_fork_ext_migrations(db.conn()).expect("first apply");
    db_fork_ext::apply_fork_ext_migrations(db.conn()).expect("second apply");

    let version = db_fork_ext::read_fork_ext_version(db.conn()).expect("read version");
    assert_eq!(version, 5);
}
