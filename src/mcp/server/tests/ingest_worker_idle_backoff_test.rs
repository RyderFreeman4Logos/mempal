#[cfg(target_os = "linux")]
async fn wait_for_io_burst_path_sample_count(
    path: crate::observability::IoOperationPath,
    minimum_sample_count: u64,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = io_burst_path(&crate::observability::io_burst_snapshot(), path);
        if snapshot.sample_count >= minimum_sample_count {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {path:?} IO burst sample count to reach {minimum_sample_count}; observed {}",
            snapshot.sample_count
        );
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn test_mcp_async_ingest_worker_backs_off_when_idle() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let _observability_lock = crate::observability::test_support::global_observability_test_lock()
        .lock_owned()
        .await;
    crate::observability::test_support::reset_ingest_worker_backoff_for_tests();
    crate::observability::test_support::reset_io_burst_for_tests();
    let (_tempdir, db_path, server) = setup_server();
    let queue = AsyncPendingMessageStore::from_store(
        crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path),
    );
    let server = server.with_async_queue_for_test(queue);
    let before_queue = io_burst_path(
        &crate::observability::io_burst_snapshot(),
        crate::observability::IoOperationPath::Queue,
    );
    let handle = server.spawn_scoped_ingest_drain_worker();

    wait_for_ingest_worker_backoff_snapshot(1, 2_000, None).await;
    #[cfg(target_os = "linux")]
    {
        wait_for_io_burst_path_sample_count(
            crate::observability::IoOperationPath::Queue,
            before_queue.sample_count.saturating_add(1),
        )
        .await;
        let after_queue = io_burst_path(
            &crate::observability::io_burst_snapshot(),
            crate::observability::IoOperationPath::Queue,
        );
        assert!(
            after_queue.sample_count > before_queue.sample_count,
            "idle queue claims must be visible in IO burst telemetry"
        );
    }

    tokio::time::advance(Duration::from_secs(2)).await;
    wait_for_ingest_worker_backoff_snapshot(2, 4_000, None).await;

    handle.shutdown_and_drain().await;
    assert_ingest_worker_backoff_snapshot(0, 0, None);
}
