use super::*;
use std::cell::Cell;
use std::fs;

use crate::core::db::Database;
use crate::core::queue::AsyncPendingMessageStore;

fn request(key: &str, payload: &str) -> HookIpcEnqueueRequest {
    HookIpcEnqueueRequest {
        kind: "hook_user_prompt".to_string(),
        payload: payload.to_string(),
        idempotency_key: key.to_string(),
    }
}

#[test]
fn parent_sync_fault_is_scoped_to_the_armed_spool() {
    let dir_a = tempfile::tempdir().expect("tempdir a");
    let dir_b = tempfile::tempdir().expect("tempdir b");
    let spool_a = IngressSpool::new(dir_a.path());
    let spool_b = IngressSpool::new(dir_b.path());
    let _fault = fail_next_parent_namespace_sync_for(&spool_a);

    std::thread::scope(|scope| {
        let thread_a = scope.spawn(|| spool_a.append(&request("a-key", "payload-a")));
        let thread_b = scope.spawn(|| spool_b.append(&request("b-key", "payload-b")));
        let a_result = thread_a.join().expect("spool a thread");
        let b_result = thread_b.join().expect("spool b thread");
        assert!(
            b_result.is_ok(),
            "unarmed spool must not consume A's parent-sync fault: {b_result:?}"
        );
        assert!(
            matches!(a_result, Err(IngressSpoolError::Uncertain(_))),
            "armed spool must fail parent sync: {a_result:?}"
        );
    });
}

#[test]
fn claim_transition_syncs_spool_namespace() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let spool = IngressSpool::new(tempdir.path());
    let record = request("claim-sync-key", "payload");
    spool.append(&record).expect("append");
    let stored = spool
        .records()
        .expect("records")
        .into_iter()
        .next()
        .expect("stored record");
    let before = super::SYNC_DIRECTORY_CALLS.with(Cell::get);

    spool.claim(&stored.path).expect("claim");

    let calls = super::SYNC_DIRECTORY_CALLS.with(|value| value.get() - before);
    assert!(
        calls >= 2,
        "claim transition must sync ingress-spool and its parent, calls={calls}"
    );
}

#[tokio::test]
async fn claim_sync_failure_clears_active_marker_and_replays_once() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let db_path = tempdir.path().join("palace.db");
    Database::open(&db_path).expect("database");
    let spool = IngressSpool::new(tempdir.path());
    let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    let record = request("claim-fsync-failure", "payload");
    spool.append(&record).expect("append");
    let _fault = fail_next_parent_namespace_sync_for(&spool);

    let error = spool
        .drain_once(&store)
        .await
        .expect_err("claim namespace sync failure must stop before enqueue");
    assert!(matches!(error, IngressSpoolError::Uncertain(_)));
    assert!(
        spool
            .active_claims
            .lock()
            .expect("active claim lock")
            .is_empty(),
        "failed claim transition must clear its in-memory active marker"
    );
    let claim_count = fs::read_dir(&spool.dir)
        .expect("spool dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("claim"))
        .count();
    assert_eq!(
        claim_count, 1,
        "failed claim must remain recoverable on disk"
    );
    assert_eq!(
        rusqlite::Connection::open(&db_path)
            .expect("sqlite")
            .query_row("SELECT COUNT(*) FROM pending_messages", [], |row| row
                .get::<_, i64>(0))
            .expect("count pending rows"),
        0,
        "failed claim transition must not enqueue SQLite work"
    );

    assert_eq!(spool.drain_once(&store).await.expect("replay"), 1);
    assert_eq!(spool.drain_once(&store).await.expect("empty replay"), 0);
    assert_eq!(
        rusqlite::Connection::open(&db_path)
            .expect("sqlite")
            .query_row("SELECT COUNT(*) FROM pending_messages", [], |row| row
                .get::<_, i64>(0))
            .expect("count replayed rows"),
        1,
        "later drain must recover exactly one queue row"
    );
}
