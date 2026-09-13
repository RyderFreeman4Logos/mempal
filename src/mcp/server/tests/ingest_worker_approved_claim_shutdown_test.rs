#[tokio::test]
async fn test_scoped_ingest_shutdown_keeps_approved_claim_owner() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, server) = setup_server();
    let queue = crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path);
    let operation_id = queue
        .enqueue(INGEST_ASYNC_KIND, "{}")
        .expect("enqueue operation");
    let lock = rusqlite::Connection::open(&db_path).expect("open lock holder");
    lock.execute_batch("BEGIN IMMEDIATE")
        .expect("hold SQLite writer lock");
    let claim_approved = Arc::new(tokio::sync::Notify::new());
    let approved = claim_approved.notified();
    let async_queue = AsyncPendingMessageStore::from_store(queue.clone())
        .with_claim_approved_for_test((Arc::clone(&claim_approved), None));
    let verification_queue = async_queue.clone();
    let handle = server
        .with_async_queue_for_test(async_queue)
        .spawn_scoped_ingest_drain_worker();

    tokio::time::timeout(Duration::from_secs(1), approved)
        .await
        .expect("ready_rx/approval handshake did not succeed");
    handle.request_shutdown();
    assert!(
        !handle.handle.is_finished(),
        "original worker must retain join while the approved claim is still in flight"
    );

    lock.execute_batch("ROLLBACK")
        .expect("release SQLite writer lock");
    tokio::time::timeout(Duration::from_secs(1), handle.shutdown_and_drain())
        .await
        .expect("approved claim owner delayed scoped shutdown");

    let record = queue
        .operation_status(&operation_id)
        .expect("load operation")
        .expect("operation remains durable");
    assert_eq!(record.op_state, IngestOperationState::Failed.as_str());
    assert!(record.claimed_at.is_some());
    assert!(record.completed_at.is_some());

    let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let leftover = tokio::time::timeout(
        Duration::from_secs(1),
        verification_queue.claim_next_by_kind_until_shutdown(
            "verification-worker".to_string(),
            INGEST_CLAIM_TTL_SECS,
            INGEST_ASYNC_KIND.to_string(),
            &mut shutdown_rx,
        ),
    )
    .await
    .expect("verification claim timed out")
    .expect("verification claim failed");
    assert!(
        leftover.is_none(),
        "original worker must complete/release the approved claim; leftover={leftover:?}"
    );
}

#[test]
fn test_actual_runtime_shutdown_releases_late_approved_claim() {
    let _worker_lifecycle_lock =
        crate::observability::test_support::acquire_ingest_worker_lifecycle_lock_blocking();
    let (_tempdir, db_path, server) = setup_server();
    let queue = crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path);
    let operation_id = queue
        .enqueue(INGEST_ASYNC_KIND, "{}")
        .expect("enqueue operation");
    let db = crate::core::db::Database::open(&db_path).expect("open lease owner");
    let lease = db
        .runtime_writer_lease_acquire(
            "sqlite-writer",
            "runtime-shutdown-test",
            "daemon",
            120,
            None,
        )
        .expect("acquire runtime writer lease")
        .expect("runtime writer lease available");
    let claim_approved = Arc::new(tokio::sync::Notify::new());
    let claim_gate = Arc::new(std::sync::Barrier::new(2));
    let cleanup_gate = Arc::new(std::sync::Barrier::new(2));
    let (committed_tx, committed_rx) = std::sync::mpsc::channel();
    let async_queue = AsyncPendingMessageStore::from_store(queue.clone())
        .with_lifecycle_writer_lease(lease.clone())
        .with_claim_approved_for_test((
            Arc::clone(&claim_approved),
            Some((
                Arc::clone(&claim_gate),
                committed_tx,
                Arc::clone(&cleanup_gate),
            )),
        ));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build private daemon-style runtime");
    runtime.block_on(async {
        let approved = claim_approved.notified();
        let handle = server
            .with_async_queue_for_test(async_queue)
            .spawn_scoped_ingest_drain_worker();
        tokio::time::timeout(Duration::from_secs(1), approved)
            .await
            .expect("claim approval did not reach the blocking owner");
        handle
            .shutdown_and_drain_with_budget(Some(Duration::ZERO))
            .await;
        assert_eq!(queue.reclaim_stale(0).expect("one-shot shutdown reclaim"), 0);
    });
    runtime.shutdown_timeout(Duration::ZERO);

    claim_gate.wait();
    committed_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("blocking claim did not commit after runtime shutdown");
    assert!(
        db.runtime_writer_lease_release(&lease)
            .expect("release runtime writer lease before owner cleanup"),
        "runtime writer lease must remain owned until explicit teardown"
    );
    cleanup_gate.wait();
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        let record = queue
            .operation_status(&operation_id)
            .expect("load late-claimed operation")
            .expect("operation remains durable");
        if record.op_state == IngestOperationState::Queued.as_str()
            && record.claimed_at.is_none()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "late claim remained owned after runtime shutdown: {record:?}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
