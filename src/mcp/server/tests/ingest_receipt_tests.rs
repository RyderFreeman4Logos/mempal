use super::*;

#[tokio::test]
async fn test_self_held_queue_lock_fails_before_returning_receipts() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
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
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
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

#[tokio::test]
async fn test_cli_style_operation_wait_follows_live_daemon_receipt_to_created_ids() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, server) = setup_server();
    let daemon_lease = hold_daemon_writer_lease(&db_path);
    let waiter = server
        .clone()
        .with_daemon_writer_lease_check_error_for_test("forced daemon lease probe miss");
    let operation_id = enqueue_prepared_test_ingest_operation(
        &server,
        &db_path,
        "followable timeout receipt must yield cleanup ids from the daemon worker",
        "follow-daemon",
    )
    .await;

    let follow = {
        let waiter = waiter.clone();
        let operation_id = operation_id.clone();
        tokio::spawn(async move {
            waiter
                .wait_for_operation_status_with_scoped_worker(
                    &operation_id,
                    Duration::from_secs(8),
                    Duration::from_millis(25),
                )
                .await
        })
    };

    tokio::time::sleep(Duration::from_millis(400)).await;
    let daemon_worker = server.with_external_ingest_writer_lease(daemon_lease.clone());
    let async_queue = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    let claim = async_queue
        .claim_by_id_and_kind_with_lock_retry_deadline(
            "test-daemon-ingest-worker".to_string(),
            INGEST_CLAIM_TTL_SECS,
            operation_id.clone(),
            INGEST_ASYNC_KIND.to_string(),
            Duration::from_secs(1),
        )
        .await
        .expect("daemon worker claim lookup")
        .expect("live-daemon follow must leave the receipt claimable");
    daemon_worker
        .process_ingest_claim(&async_queue, "test-daemon-ingest-worker", claim)
        .await
        .expect("daemon worker should complete the followed receipt");

    let response = tokio::time::timeout(Duration::from_secs(5), follow)
        .await
        .expect("follow wait should finish after daemon completion")
        .expect("follow wait task should not panic")
        .expect("follow wait should not error")
        .expect("followed operation should reach a terminal response");

    assert_eq!(response.state, Some(IngestOperationState::Completed));
    assert!(
        !response.created_drawer_ids.is_empty(),
        "followable wait must expose cleanup-safe created drawer ids"
    );
    assert!(
        Database::open(&db_path)
            .expect("open db")
            .runtime_writer_lease_is_active(&daemon_lease)
            .expect("check daemon lease"),
        "follow wait must not take over the live daemon writer lease"
    );
    release_test_ingest_writer_lease(&db_path, &daemon_lease);
}

#[cfg(unix)]
#[tokio::test]
async fn test_fsynced_daemon_ack_returns_receipt_before_queue_visibility() {
    let (tempdir, db_path, server) = setup_server();
    let request = IngestRequest {
        content: "fsynced daemon receipt survives blocked queue visibility".into(),
        wing: "mcp".into(),
        room: Some("receipt".into()),
        ..IngestRequest::default()
    };
    let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
    let project_id = server
        .resolve_mcp_project_id(request.project_id.as_deref(), config.as_ref())
        .await
        .expect("resolve project");
    let prepared = server
        .prepare_async_ingest_operation(
            &request,
            side_effect_controls(),
            config.as_ref(),
            compiled_privacy.as_ref(),
            project_id,
        )
        .await
        .expect("prepare queued ingest");
    let payload = serde_json::to_string(&prepared).expect("serialize queued ingest");
    let idempotency_key = mcp_ingest_idempotency_key(&payload);

    let (listener, _socket_guard) =
        crate::hook_ipc::bind_listener(tempdir.path()).expect("bind daemon IPC");
    let spool = Arc::new(crate::ingress_spool::IngressSpool::new(tempdir.path()));
    let daemon_spool = Arc::clone(&spool);
    let daemon = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept daemon IPC");
        let request = crate::hook_ipc::read_enqueue_request(&mut stream)
            .await
            .expect("read daemon IPC request");
        daemon_spool.append(&request).expect("fsync ingress spool");
        crate::hook_ipc::write_enqueue_response(
            &mut stream,
            &crate::hook_ipc::HookIpcEnqueueResponse::Accepted,
        )
        .await
        .expect("write durable ACK");
        request
    });
    let lock = rusqlite::Connection::open(&db_path).expect("open lock connection");
    lock.execute_batch("BEGIN IMMEDIATE")
        .expect("block queue visibility");

    let outcome = server
        .try_enqueue_ingest_operation_via_daemon(
            payload,
            idempotency_key,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("durable ACK must return a receipt");
    let request = daemon.await.expect("daemon IPC task");
    let expected_operation_id =
        PendingMessageStore::idempotent_message_id(INGEST_ASYNC_KIND, &request.idempotency_key);
    assert_eq!(
        outcome,
        DaemonIngestEnqueue::Accepted {
            operation_id: expected_operation_id.clone(),
        }
    );
    assert!(
        PendingMessageStore::new_without_reclaim(&db_path)
            .operation_status(&expected_operation_id)
            .expect("query hidden operation")
            .is_none(),
        "ACK must precede SQLite visibility"
    );

    lock.execute_batch("ROLLBACK").expect("release queue lock");
    let queue = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    assert_eq!(spool.drain_once(&queue).await.expect("replay spool"), 1);
    assert_eq!(spool.drain_once(&queue).await.expect("dedupe replay"), 0);
    let claim = queue
        .claim_by_id_and_kind_with_lock_retry_deadline(
            "receipt-worker".into(),
            INGEST_CLAIM_TTL_SECS,
            expected_operation_id.clone(),
            INGEST_ASYNC_KIND.into(),
            Duration::from_secs(1),
        )
        .await
        .expect("claim replayed operation")
        .expect("replayed operation exists");
    server
        .process_ingest_claim(&queue, "receipt-worker", claim)
        .await
        .expect("complete replayed operation");

    let completed = server
        .operation_status_json_for_test(&expected_operation_id)
        .await
        .expect("load completion receipt");
    assert_eq!(completed.state, Some(IngestOperationState::Completed));
    assert_eq!(completed.created_drawer_ids.len(), 1);
    let conn = rusqlite::Connection::open(&db_path).expect("open verification connection");
    let completion_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_message_completions WHERE message_id = ?1",
            [&expected_operation_id],
            |row| row.get(0),
        )
        .expect("count completions");
    let created_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM drawers WHERE creation_operation_id = ?1",
            [&expected_operation_id],
            |row| row.get(0),
        )
        .expect("count created drawers");
    assert_eq!((completion_count, created_count), (1, 1));
}
