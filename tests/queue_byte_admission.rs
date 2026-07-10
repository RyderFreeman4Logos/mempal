use std::sync::{Arc, Barrier};

use mempal::core::db::Database;
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
    let barrier = Arc::new(Barrier::new(3));
    let payload = "m".repeat(600);

    let workers = (0..2)
        .map(|_| {
            let db_path = db_path.clone();
            let barrier = Arc::clone(&barrier);
            let payload = payload.clone();
            std::thread::spawn(move || {
                let store =
                    PendingMessageStore::with_config(db_path, config).expect("open queue store");
                barrier.wait();
                store.enqueue("ingest_async", &payload)
            })
        })
        .collect::<Vec<_>>();

    barrier.wait();
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
