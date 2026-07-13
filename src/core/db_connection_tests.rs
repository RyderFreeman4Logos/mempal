use super::*;
use crate::core::db::{Database, SQLITE_CACHE_SIZE_KIB_DEFAULT};

#[test]
fn raw_connection_admission_matches_connection_lifetime_and_cache() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("palace.db");
    drop(Database::open(&db_path).expect("initialize database"));

    let connection =
        AdmittedSqliteConnection::open(&db_path, DbHolderClass::Cli, SQLITE_CACHE_SIZE_KIB_DEFAULT)
            .expect("open admitted connection");
    let snapshot = ProfileDbAdmission::snapshot(&db_path).expect("active snapshot");
    assert_eq!(snapshot.active_holders, 1);
    assert_eq!(snapshot.holders[0].connection_count, 1);
    assert_eq!(
        connection
            .connection()
            .pragma_query_value(None, "cache_size", |row| row.get::<_, i64>(0))
            .expect("configured SQLite cache"),
        SQLITE_CACHE_SIZE_KIB_DEFAULT
    );
    assert_eq!(
        snapshot.active_cache_bytes,
        SQLITE_CACHE_SIZE_KIB_DEFAULT.unsigned_abs() * 1024
    );

    drop(connection);
    assert_eq!(
        ProfileDbAdmission::snapshot(&db_path)
            .expect("released snapshot")
            .active_holders,
        0
    );
}
