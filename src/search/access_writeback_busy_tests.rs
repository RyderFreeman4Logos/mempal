use super::dispatch_access_update;
use super::tests::{access_count, configure_record_access, make_drawer};
use tempfile::TempDir;

#[test]
fn dispatch_access_update_skips_when_sqlite_writer_is_busy() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .expect("build test runtime");
    runtime.block_on(async {
        let lock = crate::core::config::global_config_test_lock();
        let _guard = lock.lock().await;
        let tmp = TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let db = crate::core::db::Database::open(&db_path).expect("db");
        let drawer = make_drawer("access-busy", "alpha", "decision");
        db.insert_drawer(&drawer).expect("insert drawer");
        configure_record_access(tmp.path(), &db_path, true).await;

        let lock_holder = rusqlite::Connection::open(&db_path).expect("open lock holder");
        lock_holder
            .busy_timeout(std::time::Duration::ZERO)
            .expect("set fail-fast lock holder timeout");
        lock_holder
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("hold writer lock");

        let (pool_started_tx, pool_started_rx) = tokio::sync::oneshot::channel();
        let (pool_release_tx, pool_release_rx) = tokio::sync::oneshot::channel();
        let pool_guard = tokio::task::spawn_blocking(move || {
            pool_started_tx
                .send(())
                .expect("signal blocking pool guard");
            pool_release_rx
                .blocking_recv()
                .expect("release blocking pool guard");
        });
        pool_started_rx.await.expect("blocking pool guard started");

        dispatch_access_update(db_path.clone(), vec![drawer.id.clone()]);
        pool_release_tx.send(()).expect("release blocking pool");
        pool_guard.await.expect("join blocking pool guard");
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while crate::observability::resource_counters().access_writeback_failed_total == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("access writeback should reach the busy writer");
        tokio::task::spawn_blocking(|| {})
            .await
            .expect("drain access writeback task");
        lock_holder
            .execute_batch("COMMIT;")
            .expect("release writer lock");

        let counters = crate::observability::resource_counters();
        assert_eq!(counters.access_writeback_scheduled_total, 1);
        assert_eq!(counters.access_writeback_failed_total, 1);
        assert_eq!(
            access_count(&db_path, &drawer.id),
            0,
            "access writeback is best-effort and must not wait behind ingest-critical writes"
        );
    });
}
