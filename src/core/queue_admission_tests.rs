use crate::core::db::Database;
use crate::core::db_admission::ProfileDbAdmission;
use crate::core::queue::{PendingMessageStore, QueueError};

use super::{QUEUE_CONNECTION_CACHE_BYTES, QUEUE_CONNECTIONS_PER_CACHE, queue_stats_readonly};

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
