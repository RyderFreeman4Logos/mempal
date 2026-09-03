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

pub(crate) async fn acquire_ingest_worker_lifecycle_lock() -> tokio::sync::OwnedMutexGuard<()> {
    global_ingest_worker_lifecycle_test_lock()
        .lock_owned()
        .await
}

pub(crate) fn acquire_ingest_worker_lifecycle_lock_blocking() -> tokio::sync::OwnedMutexGuard<()> {
    global_ingest_worker_lifecycle_test_lock().blocking_lock_owned()
}

pub(crate) use super::reset_io_burst_for_tests;

pub(crate) fn reset_ingest_worker_backoff_for_tests() {
    *super::global_ingest_worker_backoff()
        .lock()
        .expect("ingest worker backoff mutex poisoned") =
        super::IngestWorkerBackoffSnapshot::default();
}
