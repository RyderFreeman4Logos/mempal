#[tokio::test(flavor = "current_thread")]
async fn test_mcp_async_ingest_malformed_payload_records_failed_receipt() {
    let (_tempdir, db_path, server) = setup_server();
    let queue = crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path);
    let operation_id = queue
        .enqueue(INGEST_ASYNC_KIND, "{not json")
        .expect("enqueue malformed async ingest");
    let claim = queue
        .claim_next_by_kind("worker-malformed", 60, INGEST_ASYNC_KIND)
        .expect("claim malformed op")
        .expect("claimed malformed op");
    let async_queue = AsyncPendingMessageStore::from_store(queue.clone());

    server
        .process_ingest_claim(&async_queue, "worker-malformed", claim)
        .await
        .expect("process malformed payload");

    let failed = server
        .operation_status_json_for_test(&operation_id)
        .await
        .expect("failed status");
    assert_eq!(failed.state, Some(IngestOperationState::Failed));
    assert!(failed.drawer_id.is_empty());
    assert!(
        failed
            .failure_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("failed to decode ingest operation")),
        "{failed:?}"
    );

    let record = crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path)
        .operation_status(&operation_id)
        .expect("load operation record")
        .expect("operation record exists");
    assert_eq!(record.op_state, IngestOperationState::Failed.as_str());
    assert!(record.completed_at.is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn test_mcp_async_ingest_failure_receipt_retries_transient_sqlite_lock() {
    let (_tempdir, db_path, server) = setup_server();
    let queue = crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path);
    let operation_id = queue
        .enqueue(INGEST_ASYNC_KIND, "{not json")
        .expect("enqueue malformed async ingest");
    let claim = queue
        .claim_next_by_kind("worker-failure-lock", 60, INGEST_ASYNC_KIND)
        .expect("claim malformed op")
        .expect("claimed malformed op");
    let async_queue = AsyncPendingMessageStore::from_store(queue.clone())
        .with_complete_lock_failures_for_test(2);

    server
        .process_ingest_claim(&async_queue, "worker-failure-lock", claim)
        .await
        .expect("transient failure receipt persistence locks should be retried");

    let failed = server
        .operation_status_json_for_test(&operation_id)
        .await
        .expect("failed status");
    assert_eq!(failed.state, Some(IngestOperationState::Failed));
    assert!(failed.drawer_id.is_empty());
    assert!(
        failed
            .failure_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("failed to decode ingest operation")),
        "{failed:?}"
    );
    let record = queue
        .operation_status(&operation_id)
        .expect("load operation record")
        .expect("operation record exists");
    assert_eq!(record.op_state, IngestOperationState::Failed.as_str());
    assert!(record.completed_at.is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn test_mcp_scoped_finite_malformed_payload_completion_lock_releases_claim() {
    let (_tempdir, db_path, server) = setup_server();
    let queue = crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path);
    let operation_id = queue
        .enqueue(INGEST_ASYNC_KIND, "{not json")
        .expect("enqueue malformed async ingest");
    let claim = queue
        .claim_next_by_kind("worker-finite-failure-lock", 60, INGEST_ASYNC_KIND)
        .expect("claim malformed op")
        .expect("claimed malformed op");
    let async_queue = AsyncPendingMessageStore::from_store(queue.clone())
        .with_complete_lock_failures_for_test(10_000);

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        server.process_ingest_claim_inline_with_budget(
            &async_queue,
            "worker-finite-failure-lock",
            claim,
            Duration::from_secs(1),
        ),
    )
    .await
    .expect("finite scoped failure receipt retry must not use the 300s background window")
    .expect("malformed payload should release for retry when failure receipt is locked");

    assert_eq!(result, ScopedIngestProcessResult::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "finite scoped failure receipt retry exceeded the caller budget"
    );
    let record = queue
        .operation_status(&operation_id)
        .expect("load operation record")
        .expect("operation record exists");
    assert_eq!(record.op_state, IngestOperationState::Queued.as_str());
    assert!(
        record.claimed_at.is_none(),
        "malformed payload must release the claim when failure receipt persistence times out"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_mcp_scoped_expired_malformed_payload_retries_claim_release() {
    let (_tempdir, db_path, server) = setup_server();
    let queue = crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path);
    let operation_id = queue
        .enqueue(INGEST_ASYNC_KIND, "{not json")
        .expect("enqueue malformed async ingest");
    let claim = queue
        .claim_next_by_kind("worker-expired-failure-lock", 60, INGEST_ASYNC_KIND)
        .expect("claim malformed op")
        .expect("claimed malformed op");
    let async_queue = AsyncPendingMessageStore::from_store(queue.clone())
        .with_complete_lock_failures_for_test(1)
        .with_release_lock_failures_for_test(1);

    let result = server
        .process_ingest_claim_inline_with_budget(
            &async_queue,
            "worker-expired-failure-lock",
            claim,
            Duration::ZERO,
        )
        .await
        .expect("expired scoped cleanup must retry a transient release lock");

    assert_eq!(result, ScopedIngestProcessResult::TimedOut);
    let record = queue
        .operation_status(&operation_id)
        .expect("load operation record")
        .expect("operation record exists");
    assert_eq!(record.op_state, IngestOperationState::Queued.as_str());
    assert!(record.claimed_at.is_none(), "expired claim must be released");
}

#[tokio::test(flavor = "current_thread")]
async fn test_mcp_scoped_post_claim_expiry_retries_release_lock() {
    let (_tempdir, db_path, server) = setup_server();
    let async_queue = AsyncPendingMessageStore::new_without_reclaim(&db_path)
        .with_claim_blocking_delay(Duration::from_millis(1500))
        .with_release_lock_failures_for_test(1);
    let server = server.with_async_queue_for_test(async_queue);
    let operation_id = enqueue_prepared_test_ingest_operation(
        &server,
        &db_path,
        "post-claim expiry must release after a transient lock",
        "post-claim-expiry-release-lock",
    )
    .await;

    let result = tokio::time::timeout(
        Duration::from_secs(3),
        server.wait_for_operation_status_with_scoped_worker_until_terminal(
            &operation_id,
            Duration::from_secs(1),
            Duration::from_millis(1),
        ),
    )
    .await
    .expect("post-claim cleanup must not outlive the caller budget")
    .expect("expired post-claim cleanup must retry a transient release lock");

    assert!(result.is_none());
    let record = PendingMessageStore::new_without_reclaim(&db_path)
        .operation_status(&operation_id)
        .expect("load operation status")
        .expect("operation must stay queryable");
    assert_eq!(record.op_state, IngestOperationState::Queued.as_str());
    assert!(record.claimed_at.is_none(), "expired post-claim must be released");
}

#[tokio::test(flavor = "current_thread")]
async fn test_mcp_scoped_finite_malformed_payload_real_sqlite_lock_respects_budget() {
    let (_tempdir, db_path, server) = setup_server();
    let queue = crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path);
    let operation_id = queue
        .enqueue(INGEST_ASYNC_KIND, "{not json")
        .expect("enqueue malformed async ingest");
    let claim = queue
        .claim_next_by_kind("worker-real-failure-lock", 60, INGEST_ASYNC_KIND)
        .expect("claim malformed op")
        .expect("claimed malformed op");
    let async_queue = AsyncPendingMessageStore::from_store(queue.clone());
    let lock = hold_sqlite_write_lock(db_path.clone(), Duration::from_secs(2));

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        server.process_ingest_claim_inline_with_budget(
            &async_queue,
            "worker-real-failure-lock",
            claim,
            Duration::from_secs(1),
        ),
    )
    .await
    .expect("finite scoped failure receipt must not wait for SQLite's default busy timeout");
    let error = result.expect_err("real locked failure receipt should return a bounded error");
    assert!(
        error
            .to_string()
            .contains("failed to release timed-out scoped ingest claim"),
        "{error:#}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "finite scoped malformed-payload completion waited for the default SQLite busy timeout"
    );
    lock.join().expect("lock thread");
    let record = queue
        .operation_status(&operation_id)
        .expect("load operation record")
        .expect("operation record exists");
    assert_eq!(record.op_state, IngestOperationState::Running.as_str());
    assert!(record.completed_at.is_none());
}
