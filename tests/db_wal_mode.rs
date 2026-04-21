use mempal::core::db::Database;
use tempfile::TempDir;

#[test]
fn test_db_opens_with_wal_journal_mode() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");

    let journal_mode = db
        .conn()
        .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
        .expect("read journal_mode");

    assert_eq!(journal_mode.to_lowercase(), "wal");
}

#[test]
fn test_db_opens_with_normal_synchronous() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");

    let synchronous = db
        .conn()
        .query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))
        .expect("read synchronous");

    assert_eq!(synchronous, 1);
}
