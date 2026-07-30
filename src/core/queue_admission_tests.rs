use crate::core::db::Database;
use crate::core::db_admission::ProfileDbAdmission;
use crate::core::queue::{PendingMessageStore, QueueError};
use std::time::{Duration, Instant};

#[cfg(unix)]
use rusqlite::{Connection, OpenFlags};

use super::{
    QUEUE_CONNECTION_CACHE_BYTES, QUEUE_CONNECTIONS_PER_CACHE, queue_stats_readonly,
    queue_stats_readonly_with_busy_timeout, queue_write_admission_preflight,
};

#[test]
fn readonly_stats_missing_database_has_no_filesystem_side_effects() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_dir = temp.path().join("missing-home").join(".mempal");
    let db_path = db_dir.join("palace.db");

    assert!(matches!(
        queue_stats_readonly(&db_path),
        Err(QueueError::DatabaseMissing(path)) if path == db_path
    ));
    assert!(!db_dir.exists());
}

#[test]
fn queue_cache_clones_share_admission_and_forks_register_separately() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("palace.db");
    drop(Database::open(&db_path).expect("initialize database"));

    let store = PendingMessageStore::new_without_reclaim(&db_path);
    let clone = store.clone();
    store.stats().expect("open root cache");
    clone.stats().expect("reuse root cache");

    let root_snapshot = ProfileDbAdmission::snapshot(&db_path).expect("root snapshot");
    assert_eq!(root_snapshot.active_holders, 1);
    assert_eq!(
        root_snapshot.holders[0].connection_count,
        QUEUE_CONNECTIONS_PER_CACHE
    );
    assert_eq!(
        root_snapshot.active_cache_bytes,
        QUEUE_CONNECTION_CACHE_BYTES
    );

    let fork = store.fork_connection_cache();
    fork.stats().expect("open fork cache");
    assert_eq!(
        ProfileDbAdmission::snapshot(&db_path)
            .expect("fork snapshot")
            .active_holders,
        2
    );

    drop(fork);
    assert_eq!(
        ProfileDbAdmission::snapshot(&db_path)
            .expect("fork released snapshot")
            .active_holders,
        1
    );
    drop(clone);
    drop(store);
    assert_eq!(
        ProfileDbAdmission::snapshot(&db_path)
            .expect("all released snapshot")
            .active_holders,
        0
    );
}

#[cfg(unix)]
#[test]
fn readonly_stats_open_the_admitted_target_after_symlink_retarget() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let first_target = temp.path().join("first.db");
    let second_target = temp.path().join("second.db");
    let link_path = temp.path().join("palace.db");
    drop(Database::open(&first_target).expect("initialize first database"));
    drop(Database::open(&second_target).expect("initialize second database"));
    symlink(&first_target, &link_path).expect("link first database");
    let expected = first_target.canonicalize().expect("canonical first target");

    let stats = super::queue_stats_readonly_with_opener(&link_path, |resolved_path| {
        assert_eq!(
            resolved_path, expected,
            "stats must open the admitted target"
        );
        std::fs::remove_file(&link_path).expect("remove first symlink");
        symlink(&second_target, &link_path).expect("retarget database symlink");
        Connection::open_with_flags(
            resolved_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
    })
    .expect("readonly stats must retain the first target");

    assert_eq!(stats.pending, 0);
}

#[test]
fn readonly_stats_with_diagnostic_busy_timeout_returns_without_default_busy_wait() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("palace.db");
    drop(Database::open(&db_path).expect("initialize database"));

    let lock = Connection::open(&db_path).expect("open SQLite lock connection");
    lock.pragma_update(None, "journal_mode", "DELETE")
        .expect("disable WAL for exclusive diagnostic lock");
    lock.execute_batch("BEGIN EXCLUSIVE;")
        .expect("hold SQLite exclusive lock");

    let started = Instant::now();
    let error = queue_stats_readonly_with_busy_timeout(&db_path, Duration::from_millis(25))
        .expect_err("diagnostic queue stats must report the held writer lock");
    let elapsed = started.elapsed();

    lock.execute_batch("ROLLBACK;")
        .expect("release SQLite write lock");
    assert!(
        error.is_sqlite_lock(),
        "expected SQLite lock diagnostic, got {error}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "diagnostic stats inherited a long SQLite busy wait: {elapsed:?}"
    );
}

#[cfg(unix)]
#[test]
fn write_admission_preflight_returns_without_default_busy_wait() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("palace.db");
    drop(Database::open(&db_path).expect("initialize database"));

    let lock = Connection::open(&db_path).expect("open SQLite lock connection");
    lock.execute_batch("BEGIN IMMEDIATE;")
        .expect("hold SQLite write lock");

    let started = Instant::now();
    let error = queue_write_admission_preflight(&db_path)
        .expect_err("queue preflight must report the held writer lock");
    let elapsed = started.elapsed();

    lock.execute_batch("ROLLBACK;")
        .expect("release SQLite write lock");
    assert!(
        error.is_sqlite_lock(),
        "expected SQLite lock preflight error, got {error}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "queue preflight inherited a long SQLite busy wait: {elapsed:?}"
    );
}
