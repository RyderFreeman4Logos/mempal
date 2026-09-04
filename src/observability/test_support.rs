use std::sync::{Arc, OnceLock};

pub(crate) fn global_observability_test_lock() -> Arc<tokio::sync::Mutex<()>> {
    // ponytail: global test lock, split by independent telemetry store if test runtime matters.
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    Arc::clone(LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))))
}

pub(crate) fn global_ingest_worker_lifecycle_test_lock() -> Arc<tokio::sync::Mutex<()>> {
    // ponytail: one process-wide lifecycle lock; split when backoff becomes worker-scoped.
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    Arc::clone(LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))))
}

pub(crate) async fn acquire_ingest_worker_lifecycle_lock() -> (
    std::sync::MutexGuard<'static, ()>,
    tokio::sync::OwnedMutexGuard<()>,
) {
    let db_admission_class_lock = crate::core::db::db_open_busy_fixture_lock()
        .lock()
        .expect("serialize database admission fixtures");
    let worker_lifecycle_lock = global_ingest_worker_lifecycle_test_lock()
        .lock_owned()
        .await;
    (db_admission_class_lock, worker_lifecycle_lock)
}

pub(crate) fn acquire_ingest_worker_lifecycle_lock_blocking() -> (
    std::sync::MutexGuard<'static, ()>,
    tokio::sync::OwnedMutexGuard<()>,
) {
    let db_admission_class_lock = crate::core::db::db_open_busy_fixture_lock()
        .lock()
        .expect("serialize database admission fixtures");
    let worker_lifecycle_lock = global_ingest_worker_lifecycle_test_lock().blocking_lock_owned();
    (db_admission_class_lock, worker_lifecycle_lock)
}

pub(crate) use super::reset_io_burst_for_tests;

pub(crate) fn reset_ingest_worker_backoff_for_tests() {
    *super::global_ingest_worker_backoff()
        .lock()
        .expect("ingest worker backoff mutex poisoned") =
        super::IngestWorkerBackoffSnapshot::default();
}
