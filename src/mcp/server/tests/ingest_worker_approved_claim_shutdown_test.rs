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
        .with_claim_approved_for_test(Arc::clone(&claim_approved));
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
