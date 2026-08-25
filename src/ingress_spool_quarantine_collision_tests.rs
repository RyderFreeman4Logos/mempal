use super::*;
use crate::core::db::Database;

fn request(key: &str, payload: &str) -> HookIpcEnqueueRequest {
    HookIpcEnqueueRequest {
        kind: "hook_user_prompt".to_string(),
        payload: payload.to_string(),
        idempotency_key: key.to_string(),
    }
}

fn spool_contains_payload(spool: &IngressSpool, payload: &str) -> bool {
    fs::read_dir(&spool.dir).expect("spool dir").any(|entry| {
        let path = entry.expect("entry").path();
        fs::read_to_string(path).is_ok_and(|body| body.contains(payload))
    })
}

#[tokio::test]
async fn same_key_quarantine_keeps_first_conflicting_payload() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let db_path = tempdir.path().join("palace.db");
    Database::open(&db_path).expect("database");
    let spool = IngressSpool::new(tempdir.path());
    let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    let first = request("shared-key", "payload-b");
    let second = request("shared-key", "payload-c");

    store
        .enqueue_idempotent_with_key(
            first.kind.clone(),
            "seeded-original".to_string(),
            first.idempotency_key.clone(),
        )
        .await
        .expect("seed original key");

    spool.append(&first).expect("append first conflict");
    assert_eq!(spool.drain_once(&store).await.expect("quarantine first"), 0);
    assert!(
        spool_contains_payload(&spool, "payload-b"),
        "first conflict must be parked before the second arrives"
    );

    spool.append(&second).expect("append second conflict");
    assert_eq!(
        spool.drain_once(&store).await.expect("quarantine second"),
        0
    );

    assert!(
        spool_contains_payload(&spool, "payload-b"),
        "first quarantined payload must survive a later same-key conflict"
    );
}
