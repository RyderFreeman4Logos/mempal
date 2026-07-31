use super::*;

#[tokio::test]
async fn test_self_held_queue_lock_fails_before_returning_receipts() {
    let (_tempdir, db_path, server) = setup_server();
    let queue = AsyncPendingMessageStore::new_without_reclaim(&db_path)
        .with_enqueue_lock_failures_for_test(100);
    let server = server
        .with_async_queue_for_test(queue)
        .with_ingest_admission_current_mcp_holder_for_test();

    for index in 0..4 {
        let result = server
            .mempal_ingest_with_controls(
                IngestRequest {
                    content: format!("locked pre-admission request {index}"),
                    wing: "mcp".to_string(),
                    room: Some("self-holder".to_string()),
                    wait: Some(false),
                    ..IngestRequest::default()
                },
                side_effect_controls(),
            )
            .await;

        assert!(
            result.is_err(),
            "an operation receipt requires a durable queue row"
        );
    }

    assert!(
        !server.ingest_worker_started.load(Ordering::Acquire),
        "failed admission must not start a local worker"
    );
    let stats = PendingMessageStore::new_without_reclaim(&db_path)
        .stats()
        .expect("queue stats");
    assert_eq!(stats.pending, 0);
    assert_eq!(stats.claimed, 0);
    assert_eq!(stats.failed, 0);
    assert_eq!(
        Database::open(&db_path)
            .expect("open db")
            .drawer_count()
            .expect("drawer count"),
        0
    );
}

#[tokio::test]
async fn test_self_held_queue_lock_under_daemon_lease_does_not_start_local_worker() {
    let (_tempdir, db_path, server) = setup_server();
    let queue = AsyncPendingMessageStore::new_without_reclaim(&db_path)
        .with_enqueue_lock_failures_for_test(1);
    let server = server
        .with_async_queue_for_test(queue)
        .with_ingest_admission_current_mcp_holder_for_test();
    let daemon_lease = hold_daemon_writer_lease(&db_path);

    let response = server
        .mempal_ingest_with_controls(
            IngestRequest {
                content: "locked admission must defer to the daemon".to_string(),
                wing: "mcp".to_string(),
                room: Some("daemon-lease".to_string()),
                wait: Some(true),
                wait_timeout_secs: Some(0),
                ..IngestRequest::default()
            },
            side_effect_controls(),
        )
        .await
        .expect("bounded retry should durably admit the operation")
        .0;

    let deadline = Instant::now() + Duration::from_secs(2);
    while !server.ingest_worker_started.load(Ordering::Acquire) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(
        !server.ingest_worker_started.load(Ordering::Acquire),
        "pre-admission recovery must not bypass daemon worker deferral"
    );
    let operation_id = response.operation_id.expect("durable operation receipt");
    let record = PendingMessageStore::new_without_reclaim(&db_path)
        .operation_status(&operation_id)
        .expect("query durable operation")
        .expect("returned receipt must have a durable queue row");
    assert_eq!(record.op_state, IngestOperationState::Queued.as_str());
    assert!(
        Database::open(&db_path)
            .expect("open db")
            .runtime_writer_lease_is_active(&daemon_lease)
            .expect("check daemon lease"),
        "MCP receipt handling must preserve the daemon lease"
    );
    release_test_ingest_writer_lease(&db_path, &daemon_lease);
}
