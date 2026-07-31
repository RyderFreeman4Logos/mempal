use super::*;

#[tokio::test]
async fn test_mcp_ingest_self_held_queue_recovery_completes_with_receipt() {
    let (_tempdir, db_path, server) = setup_server();
    let queue = AsyncPendingMessageStore::new_without_reclaim(&db_path)
        .with_enqueue_lock_failures_for_test(2);
    let server = server
        .with_async_queue_for_test(queue)
        .with_ingest_admission_current_mcp_holder_for_test();

    let response = server
        .mempal_ingest_with_controls(
            IngestRequest {
                content: "current MCP holder should recover through queue admission".to_string(),
                wing: "mcp".to_string(),
                room: Some("self-holder".to_string()),
                wait: Some(true),
                wait_timeout_secs: Some(10),
                ..IngestRequest::default()
            },
            side_effect_controls(),
        )
        .await
        .expect("current MCP holder should recover through the durable queue")
        .0;

    assert_eq!(response.state, Some(IngestOperationState::Completed));
    let operation_id = response
        .operation_id
        .as_deref()
        .expect("self-held recovery must return its durable operation id");
    assert!(!response.drawer_id.is_empty());
    assert!(!response.created_drawer_ids.is_empty());

    let store = PendingMessageStore::new_without_reclaim(&db_path);
    let record = store
        .operation_status(operation_id)
        .expect("query completed recovery")
        .expect("recovery operation remains queryable");
    assert_eq!(record.op_state, IngestOperationState::Completed.as_str());
    let stats = store.stats().expect("queue stats");
    assert_eq!(stats.pending, 0);
    assert_eq!(stats.claimed, 0);
    assert_eq!(stats.failed, 0);
    assert_eq!(
        Database::open(&db_path)
            .expect("open db")
            .drawer_count()
            .expect("drawer count"),
        1
    );
}

#[tokio::test]
async fn test_mcp_ingest_self_held_queue_lock_timeout_returns_followable_receipt() {
    let (_tempdir, db_path, server) = setup_server();
    let queue = AsyncPendingMessageStore::new_without_reclaim(&db_path)
        .with_enqueue_lock_failures_for_test(2);
    let server = server
        .with_async_queue_for_test(queue)
        .with_ingest_admission_current_mcp_holder_for_test()
        .with_ingest_processing_delay_for_test(Duration::from_secs(2));

    let response = server
        .mempal_ingest_with_controls(
            IngestRequest {
                content: "self-held queue lock timeout must remain followable".to_string(),
                wing: "mcp".to_string(),
                room: Some("self-holder-receipt".to_string()),
                wait: Some(true),
                wait_timeout_secs: Some(1),
                ..IngestRequest::default()
            },
            side_effect_controls(),
        )
        .await
        .expect("self-held recovery timeout must return a receipt")
        .0;

    let operation_id = response
        .operation_id
        .as_deref()
        .expect("timed-out recovery must expose an operation id");
    assert_eq!(response.state, Some(IngestOperationState::Queued));
    assert!(response.timed_out);
    assert!(response.created_drawer_ids.is_empty());
    assert!(
        PendingMessageStore::new_without_reclaim(&db_path)
            .operation_status(operation_id)
            .expect("probe durable queue before recovery")
            .is_none(),
        "the receipt must be followable before its durable queue row exists"
    );

    let pending = server
        .operation_status_json_for_test(operation_id)
        .await
        .expect("pending admission must remain queryable by operation id");
    assert_eq!(pending.state, Some(IngestOperationState::Queued));
    assert_eq!(pending.operation_id.as_deref(), Some(operation_id));
    assert!(pending.created_drawer_ids.is_empty());

    let completed = tokio::time::timeout(
        Duration::from_secs(10),
        server.wait_for_operation_completion(operation_id),
    )
    .await
    .expect("late recovery must reach a durable terminal status")
    .expect("late recovery status lookup must succeed");
    assert_eq!(completed.state, Some(IngestOperationState::Completed));
    assert_eq!(completed.operation_id.as_deref(), Some(operation_id));
    assert_eq!(completed.created_drawer_ids.len(), 1);

    let replay = server
        .operation_status_json_for_test(operation_id)
        .await
        .expect("repeated status lookup must remain safe");
    assert_eq!(replay.created_drawer_ids, completed.created_drawer_ids);
    assert_eq!(
        Database::open(&db_path)
            .expect("open db")
            .drawer_count()
            .expect("drawer count"),
        1,
        "retrying the same operation receipt must not duplicate persistence"
    );
}

#[tokio::test]
async fn test_self_held_queue_recovery_reuses_operation_identity_without_duplicate_write() {
    let (_tempdir, db_path, server) = setup_server();
    let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
    let request = IngestRequest {
        content: "one self-held recovery identity must produce one drawer".to_string(),
        wing: "mcp".to_string(),
        room: Some("self-holder-idempotency".to_string()),
        ..IngestRequest::default()
    };
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
        .expect("prepare async ingest");
    let payload = serde_json::to_string(&prepared).expect("serialize prepared ingest");
    let idempotency_key = "self-held-recovery-idempotency";

    let first_operation_id = server
        .spawn_self_held_queue_admission_recovery(payload.clone(), idempotency_key.to_string())
        .expect("register first recovery receipt");
    let replay_operation_id = server
        .spawn_self_held_queue_admission_recovery(payload, idempotency_key.to_string())
        .expect("register replayed recovery receipt");
    assert_eq!(first_operation_id, replay_operation_id);

    let completed = tokio::time::timeout(
        Duration::from_secs(10),
        server.wait_for_operation_status_with_lookup_policy(
            &first_operation_id,
            Duration::from_secs(9),
            Duration::from_millis(25),
            true,
        ),
    )
    .await
    .expect("idempotent recovery must reach terminal status")
    .expect("idempotent recovery status lookup must succeed")
    .expect("idempotent recovery must return its terminal receipt");
    assert_eq!(completed.state, Some(IngestOperationState::Completed));
    assert_eq!(completed.created_drawer_ids.len(), 1);

    let connection = rusqlite::Connection::open(&db_path).expect("open db");
    let completion_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pending_message_completions WHERE message_id = ?1",
            [first_operation_id.as_str()],
            |row| row.get(0),
        )
        .expect("count operation completions");
    assert_eq!(completion_count, 1);
    assert_eq!(
        Database::open(&db_path)
            .expect("open db")
            .drawer_count()
            .expect("drawer count"),
        1,
        "replaying one recovery identity must not duplicate the canonical write"
    );
}
