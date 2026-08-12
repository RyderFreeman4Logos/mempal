use std::sync::{Arc, Barrier, mpsc};
use std::time::Duration;

use mempal::core::db::Database;
use mempal::core::db_admission::{DbAdmissionRequest, DbHolderClass, ProfileDbAdmission};
use mempal::core::queue::{PendingMessageStore, QueueConfig, QueueError};

#[test]
fn concurrent_ingest_requests_cannot_exceed_active_byte_budget() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let config = QueueConfig {
        max_ingest_active_bytes: 1_000,
        ..QueueConfig::default()
    };
    let snapshot = ProfileDbAdmission::snapshot(&db_path).expect("empty admission snapshot");
    let usable_holders = if DbHolderClass::current_process().is_service_holder() {
        snapshot.configured_holder_limit
    } else {
        snapshot
            .configured_holder_limit
            .saturating_sub(snapshot.reserved_service_holders)
    };
    assert!(
        usable_holders >= 2,
        "fixture needs two usable queue-holder slots, got {usable_holders}"
    );
    let saturated_holders = (0..usable_holders - 1)
        .map(|_| {
            ProfileDbAdmission::acquire(
                &db_path,
                DbAdmissionRequest::new(DbHolderClass::current_process(), 1, 1),
            )
            .expect("reserve all but one usable queue-holder slot")
        })
        .collect::<Vec<_>>();

    let enqueue_barrier = Arc::new(Barrier::new(3));
    let (ready_tx, ready_rx) = mpsc::channel();
    let payload = "m".repeat(600);

    let workers = (0..2)
        .map(|_| {
            let db_path = db_path.clone();
            let enqueue_barrier = Arc::clone(&enqueue_barrier);
            let ready_tx = ready_tx.clone();
            let payload = payload.clone();
            std::thread::spawn(move || {
                let store = PendingMessageStore::new_without_reclaim_with_config(db_path, config);
                ready_tx.send(()).expect("report enqueue readiness");
                enqueue_barrier.wait();
                store.enqueue("ingest_async", &payload)
            })
        })
        .collect::<Vec<_>>();

    drop(ready_tx);
    for _ in 0..2 {
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("queue setup must reach the enqueue boundary under constrained admission");
    }
    drop(saturated_holders);
    enqueue_barrier.wait();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("join enqueue worker"))
        .collect::<Vec<_>>();

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let budget_error = results
        .iter()
        .find_map(|result| result.as_ref().err())
        .expect("one request must exceed the aggregate byte budget");
    assert!(matches!(
        budget_error,
        QueueError::IngestByteBudgetExceeded {
            payload_bytes: 600,
            active_bytes: 600,
            limit_bytes: 1_000,
        }
    ));

    let stats = PendingMessageStore::with_config(&db_path, config)
        .expect("reopen queue store")
        .stats()
        .expect("queue stats");
    assert_eq!(stats.active_ingest_payload_bytes, 600);
    assert_eq!(stats.active_payload_bytes, 600);
    assert_eq!(stats.ingest_payload_limit_bytes, 1_000);
    assert_eq!(stats.rejected_oversize, 1);
}
