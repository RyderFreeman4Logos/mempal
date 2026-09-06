use super::*;

async fn retry_partial_multi_chunk(
    persisted_operation_id: &str,
    retry_operation_id: &str,
) -> (IngestResponse, String) {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, server) = setup_server();
    let request = IngestRequest {
        content: (0..2_000)
            .map(|index| format!("partial receipt token-{index:04}"))
            .collect::<Vec<_>>()
            .join(" "),
        wing: "mcp".into(),
        room: Some("partial-receipt".into()),
        ..IngestRequest::default()
    };
    let controls = IngestControls {
        no_gate: true,
        bypass_novelty: true,
    };
    let initial = server
        .mempal_ingest_sync_with_superseded_override(
            request.clone(),
            controls,
            None,
            Some(persisted_operation_id),
            None,
        )
        .await
        .expect("seed multi-chunk operation")
        .0;
    assert!(initial.drawer_ids.len() > 1, "fixture must be multi-chunk");
    let persisted_drawer_id = initial.drawer_ids[0].clone();
    let removed_ids = initial.drawer_ids[1..].to_vec();
    let db = Database::open(&db_path).expect("open partial-operation database");
    assert_eq!(
        db.soft_delete_drawers_by_ids(&removed_ids)
            .expect("soft-delete uncommitted suffix"),
        removed_ids.len()
    );
    assert_eq!(
        db.purge_deleted(None).expect("purge uncommitted suffix"),
        removed_ids.len() as u64
    );

    let retried = server
        .mempal_ingest_sync_with_superseded_override(
            request,
            controls,
            None,
            Some(retry_operation_id),
            None,
        )
        .await
        .expect("retry partial multi-chunk operation")
        .0;
    assert_eq!(retried.drawer_ids, initial.drawer_ids);
    (retried, persisted_drawer_id)
}

#[tokio::test(flavor = "current_thread")]
async fn test_partial_multi_chunk_same_operation_recovers_all_created_ids() {
    let (retried, _) = retry_partial_multi_chunk("partial-same-op", "partial-same-op").await;
    assert_eq!(retried.created_drawer_ids, retried.drawer_ids);
}

#[tokio::test(flavor = "current_thread")]
async fn test_partial_multi_chunk_distinct_operation_excludes_prior_created_id() {
    let (retried, prior_id) = retry_partial_multi_chunk("partial-op-a", "partial-op-b").await;
    assert!(!retried.created_drawer_ids.contains(&prior_id));
    assert_eq!(retried.created_drawer_ids, retried.drawer_ids[1..]);
}

#[tokio::test(flavor = "current_thread")]
async fn test_same_operation_retry_recovers_created_ids_after_completion_failure() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, server) = setup_server();
    let old_id = "operation-receipt-old";
    insert_drawer_with_project(&db_path, old_id, "mcp", Some("receipt-retry"), None);

    let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
    let request = IngestRequest {
        content: "same operation retains creation authority after result persistence fails".into(),
        wing: "mcp".into(),
        room: Some("receipt-retry".into()),
        supersedes: Some(old_id.into()),
        dry_run: Some(false),
        ..IngestRequest::default()
    };
    let project_id = server
        .resolve_mcp_project_id(request.project_id.as_deref(), config.as_ref())
        .await
        .expect("resolve project");
    let prepared = server
        .prepare_async_ingest_operation(
            &request,
            IngestControls {
                no_gate: true,
                bypass_novelty: true,
            },
            config.as_ref(),
            compiled_privacy.as_ref(),
            project_id,
        )
        .await
        .expect("prepare queued update");
    let payload = serde_json::to_string(&prepared).expect("serialize queued update");
    let queue = PendingMessageStore::new_without_reclaim(&db_path);
    let operation_id = queue
        .enqueue(INGEST_ASYNC_KIND, &payload)
        .expect("enqueue queued update");
    let first_claim = queue
        .claim_next_by_kind("receipt-first", 60, INGEST_ASYNC_KIND)
        .expect("claim queued update")
        .expect("queued update exists");
    let claim_for_release = first_claim.clone();
    let failing_completion = AsyncPendingMessageStore::from_store(queue.clone())
        .with_complete_lock_failures_for_test(10_000);

    server
        .process_ingest_claim_inline_with_budget(
            &failing_completion,
            "receipt-first",
            first_claim,
            Duration::from_secs(1),
        )
        .await
        .expect_err("terminal result persistence must fail at the deterministic seam");
    let incomplete = queue
        .operation_status(&operation_id)
        .expect("read incomplete operation")
        .expect("incomplete operation exists");
    assert_eq!(incomplete.op_state, IngestOperationState::Running.as_str());
    assert!(incomplete.result_json.is_none());

    queue
        .release_claim(&claim_for_release)
        .expect("simulate stale-claim recovery after process crash");
    let retry_claim = queue
        .claim_next_by_kind("receipt-retry", 60, INGEST_ASYNC_KIND)
        .expect("reclaim same operation")
        .expect("same operation is retryable");
    let retry_queue = AsyncPendingMessageStore::from_store(queue.clone());
    server
        .process_ingest_claim(&retry_queue, "receipt-retry", retry_claim)
        .await
        .expect("same operation retry completes");

    let completed = server
        .operation_status_json_for_test(&operation_id)
        .await
        .expect("read completed retry receipt");
    assert_eq!(completed.state, Some(IngestOperationState::Completed));
    assert_eq!(completed.created_drawer_ids.len(), 1);
    assert_eq!(completed.drawer_ids, completed.created_drawer_ids);
}

#[test]
fn test_rolled_back_drawer_write_has_no_creation_authority() {
    let tempdir = TempDir::new_in("/tmp").expect("short tempdir");
    let db_path = tempdir.path().join("palace.db");
    let db = Database::open(&db_path).expect("open database");
    db.conn()
        .execute_batch("BEGIN IMMEDIATE")
        .expect("begin drawer write");
    db.conn()
        .execute(
            "INSERT INTO drawers (id, content, wing, source_type, confidence, added_at, creation_operation_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "rolled-back-drawer",
                "rolled back receipt fixture",
                "mcp",
                "agent_inference",
                0.8,
                iso_timestamp(),
                "rolled-back-operation",
            ],
        )
        .expect("insert drawer and creation identity");
    db.conn()
        .execute_batch("ROLLBACK")
        .expect("roll back write");

    let authority_count = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM drawers WHERE creation_operation_id = ?1",
            ["rolled-back-operation"],
            |row| row.get::<_, i64>(0),
        )
        .expect("query creation authority");
    assert_eq!(authority_count, 0);
    assert!(
        !db.drawer_exists("rolled-back-drawer")
            .expect("drawer lookup")
    );
}

#[test]
fn test_existing_drawer_creation_provenance_is_first_writer_wins() {
    let tempdir = TempDir::new_in("/tmp").expect("short tempdir");
    let db_path = tempdir.path().join("palace.db");
    let db = Database::open(&db_path).expect("open database");
    let drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: "first-writer-drawer".into(),
        content: "first writer provenance fixture".into(),
        wing: "mcp".into(),
        room: Some("receipt-retry".into()),
        source_file: None,
        source_type: SourceType::AgentInference,
        added_at: iso_timestamp(),
        chunk_index: Some(0),
        importance: 1,
    });

    db.insert_drawer_with_project_validity_and_operation(
        &drawer,
        None,
        None,
        None,
        None,
        Some("operation-a"),
    )
    .expect("first operation inserts drawer");
    db.insert_drawer_with_project_validity_and_operation(
        &drawer,
        None,
        None,
        None,
        None,
        Some("operation-b"),
    )
    .expect("later insert remains an ignored duplicate");

    let candidates = vec![drawer.id.clone()];
    assert_eq!(
        db.drawer_ids_created_by_operation("operation-a", &candidates)
            .expect("query first operation"),
        candidates
    );
    assert!(
        db.drawer_ids_created_by_operation("operation-b", &[drawer.id])
            .expect("query second operation")
            .is_empty()
    );
}
