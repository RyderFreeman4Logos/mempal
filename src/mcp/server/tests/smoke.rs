use super::*;

#[tokio::test]
async fn test_mcp_ingest_smoke_mode_rejects_non_smoke_scope() {
    let (_tempdir, _db_path, server) = setup_server();

    let error = match server
        .mempal_ingest(Parameters(IngestRequest {
            content: "smoke validation must not include request content in errors".to_string(),
            wing: "mempal".to_string(),
            room: Some("mcp".to_string()),
            smoke: Some(true),
            ..IngestRequest::default()
        }))
        .await
    {
        Ok(_) => panic!("smoke mode must reject non-smoke wing"),
        Err(error) => error,
    };

    let error_text = error.to_string();
    assert!(error_text.contains("wing=\"smoke\""), "error={error_text}");
    assert!(
        !error_text.contains("request content"),
        "smoke validation errors must not echo content: {error_text}"
    );
}

#[tokio::test]
async fn test_mcp_ingest_smoke_mode_uses_deterministic_local_vector() {
    let _tempdir = tempfile::tempdir().expect("tempdir");
    let db_path = _tempdir.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let _config_guard = ConfigOverrideGuard::install(&format!(
        r#"
db_path = "{}"

[privacy]
enabled = false

[ingest_gating]
enabled = false
"#,
        db_path.display()
    ))
    .await;
    let gate = Arc::new(Notify::new());
    let call_count = Arc::new(AtomicUsize::new(0));
    let server = MempalMcpServer::new_with_factory(
        db_path.clone(),
        Arc::new(BlockingEmbedderFactory {
            vector: vec![0.1, 0.2, 0.3],
            call_count: Arc::clone(&call_count),
            started: Arc::new(Notify::new()),
            gate: Arc::clone(&gate),
            released: Arc::new(AtomicBool::new(false)),
        }),
    )
    .expect("create MCP server");

    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "bounded MCP smoke write should bypass gates for cleanup authority"
                .to_string(),
            wing: "smoke".to_string(),
            room: Some("mcp".to_string()),
            source_type: Some("agent_inference".to_string()),
            memory_kind: Some("evidence".to_string()),
            domain: Some("project".to_string()),
            field: Some("smoke".to_string()),
            smoke: Some(true),
            wait: Some(true),
            wait_timeout_secs: Some(5),
            ..IngestRequest::default()
        }))
        .await
        .expect("smoke ingest should complete")
        .0;

    assert_eq!(response.state, Some(IngestOperationState::Completed));
    assert!(
        !response.created_drawer_ids.is_empty(),
        "smoke wait response must expose cleanup authority"
    );
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        0,
        "smoke writes must not block on the configured embedder"
    );
    gate.notify_waiters();
}

#[tokio::test]
async fn test_mcp_smoke_ingest_wait_update_and_status_return_created_ids() {
    let (_tempdir, db_path, server) = setup_server();

    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "cleanup-authoritative MCP smoke write one".to_string(),
            wing: "smoke".to_string(),
            room: Some("mcp".to_string()),
            source_type: Some("agent_inference".to_string()),
            memory_kind: Some("evidence".to_string()),
            domain: Some("project".to_string()),
            field: Some("smoke".to_string()),
            smoke: Some(true),
            wait: Some(true),
            wait_timeout_secs: Some(5),
            ..IngestRequest::default()
        }))
        .await
        .expect("smoke ingest should complete")
        .0;

    assert_eq!(response.state, Some(IngestOperationState::Completed));
    assert!(
        !response.created_drawer_ids.is_empty(),
        "smoke wait response must expose cleanup authority"
    );
    assert_eq!(response.drawer_ids, response.created_drawer_ids);

    let operation_id = response.operation_id.as_deref().expect("operation id");
    let status = server
        .operation_status_json_for_test(operation_id)
        .await
        .expect("operation status should load");

    assert_eq!(status.state, Some(IngestOperationState::Completed));
    assert_eq!(status.created_drawer_ids, response.created_drawer_ids);
    assert_eq!(status.drawer_ids, status.created_drawer_ids);
    assert!(!status.dropped);
    assert!(status.rejected_reason.is_none());

    let old_id = response.created_drawer_ids[0].clone();
    let update = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "cleanup-authoritative MCP smoke write two".to_string(),
            wing: "smoke".to_string(),
            room: Some("mcp".to_string()),
            source_type: Some("agent_inference".to_string()),
            memory_kind: Some("evidence".to_string()),
            domain: Some("project".to_string()),
            field: Some("smoke".to_string()),
            supersedes: Some(old_id.clone()),
            smoke: Some(true),
            wait: Some(true),
            wait_timeout_secs: Some(5),
            ..IngestRequest::default()
        }))
        .await
        .expect("smoke update should complete")
        .0;

    assert_eq!(update.state, Some(IngestOperationState::Completed));
    assert_eq!(
        update.superseded_drawer_id.as_deref(),
        Some(old_id.as_str())
    );
    assert_eq!(
        update.created_drawer_ids.len(),
        1,
        "smoke update wait response must expose cleanup-safe new drawer id"
    );
    assert_eq!(update.drawer_ids, update.created_drawer_ids);
    assert_ne!(update.created_drawer_ids[0], old_id);

    let operation_id = update.operation_id.as_deref().expect("update operation id");
    let update_status = server
        .operation_status_json_for_test(operation_id)
        .await
        .expect("update operation status should load");
    assert_eq!(update_status.state, Some(IngestOperationState::Completed));
    assert_eq!(update_status.created_drawer_ids, update.created_drawer_ids);
    assert_eq!(update_status.drawer_ids, update.created_drawer_ids);

    let db = Database::open(&db_path).expect("open db");
    assert!(db.get_drawer(&old_id).expect("old lookup").is_none());
    assert!(
        db.get_drawer(&update.created_drawer_ids[0])
            .expect("new lookup")
            .is_some()
    );
}
