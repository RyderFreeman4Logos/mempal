use super::*;
use crate::core::db::{Database, SQLITE_CACHE_SIZE_KIB_DEFAULT};

#[cfg(unix)]
use rusqlite::OpenFlags;

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

#[cfg(unix)]
#[test]
fn raw_connection_opens_the_admitted_target_after_symlink_retarget() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let first_target = temp.path().join("first.db");
    let second_target = temp.path().join("second.db");
    let link_path = temp.path().join("palace.db");
    Connection::open(&first_target)
        .expect("create first database")
        .pragma_update(None, "application_id", 101_i64)
        .expect("mark first database");
    Connection::open(&second_target)
        .expect("create second database")
        .pragma_update(None, "application_id", 202_i64)
        .expect("mark second database");
    symlink(&first_target, &link_path).expect("link first database");
    let expected = first_target.canonicalize().expect("canonical first target");

    let admitted = AdmittedSqliteConnection::open_with_after_admission(
        &link_path,
        DbHolderClass::Cli,
        SQLITE_CACHE_SIZE_KIB_DEFAULT,
        |resolved_path| {
            assert_eq!(
                resolved_path, expected,
                "open must receive the admitted target"
            );
            std::fs::remove_file(&link_path).expect("remove first symlink");
            symlink(&second_target, &link_path).expect("retarget database symlink");
            Connection::open_with_flags(
                resolved_path,
                OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )
        },
    )
    .expect("admitted connection must retain the first target");

    assert_eq!(
        admitted
            .connection()
            .pragma_query_value(None, "application_id", |row| row.get::<_, i64>(0))
            .expect("read opened database marker"),
        101
    );
}
