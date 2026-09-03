#[derive(Clone)]
struct FailOnceEmbedderFactory {
    vector: Vec<f32>,
    retryable: bool,
    failures: Arc<AtomicUsize>,
}

#[async_trait]
impl crate::embed::EmbedderFactory for FailOnceEmbedderFactory {
    async fn build(&self) -> crate::embed::Result<Box<dyn Embedder>> {
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_sub(1)
            })
            .is_ok()
        {
            return Err(if self.retryable {
                crate::embed::EmbedError::TemporarilyUnavailable {
                    retry_after: Duration::ZERO,
                    reason: "forced retryable embed failure".to_string(),
                }
            } else {
                crate::embed::EmbedError::InvalidConfiguration(
                    "forced permanent embed failure".to_string(),
                )
            });
        }
        Ok(Box::new(StubEmbedder {
            vector: self.vector.clone(),
        }))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_mcp_async_ingest_transient_write_lock_requeues_instead_of_failing() {
    let _worker_lifecycle_lock =
        crate::observability::test_support::acquire_ingest_worker_lifecycle_lock().await;
    let _observability_lock =
        crate::observability::test_support::global_observability_test_lock()
            .lock_owned()
            .await;
    crate::observability::test_support::reset_ingest_worker_backoff_for_tests();
    let (_tempdir, db_path, server) = setup_server();
    let server = server.with_sync_db_open_lock_failures_for_test(1);
    let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
    let request = IngestRequest {
        content: "transient write lock should retry async ingest".to_string(),
        wing: "smoke".to_string(),
        room: Some("mcp".to_string()),
        source_type: Some("agent_inference".to_string()),
        memory_kind: Some("evidence".to_string()),
        domain: Some("project".to_string()),
        field: Some("smoke".to_string()),
        smoke: Some(true),
        project_id: Some("project-transient-write-lock".to_string()),
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
            IngestControls::default(),
            config.as_ref(),
            compiled_privacy.as_ref(),
            project_id,
        )
        .await
        .expect("prepare async ingest");
    let payload = serde_json::to_string(&prepared).expect("serialize prepared ingest");
    let queue = crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path);
    let operation_id = queue
        .enqueue(INGEST_ASYNC_KIND, &payload)
        .expect("enqueue async ingest");
    let claim = queue
        .claim_next_by_kind("worker-transient-write-lock", 60, INGEST_ASYNC_KIND)
        .expect("claim queued op")
        .expect("claimed queued op");
    let async_queue = AsyncPendingMessageStore::from_store(queue.clone());

    server
        .process_ingest_claim(&async_queue, "worker-transient-write-lock", claim)
        .await
        .expect("transient write lock should requeue the ingest");

    let queued = queue
        .operation_status(&operation_id)
        .expect("load requeued operation")
        .expect("operation remains durable");
    assert_eq!(queued.op_state, IngestOperationState::Queued.as_str());
    assert!(queued.claimed_at.is_none());
    assert!(queued.completed_at.is_none());
    assert!(queued.failure_detail.is_none());

    let completion_count: i64 = rusqlite::Connection::open(&db_path)
        .expect("open db")
        .query_row(
            "SELECT COUNT(*) FROM pending_message_completions WHERE message_id = ?1",
            [operation_id.as_str()],
            |row| row.get(0),
        )
        .expect("count completions");
    assert_eq!(
        completion_count, 0,
        "transient write lock must not create a failed completion receipt"
    );

    tokio::time::sleep(ingest_worker_backoff_delay(1) + Duration::from_millis(200)).await;
    let retry_claim = queue
        .claim_next_by_kind("worker-transient-write-lock-retry", 60, INGEST_ASYNC_KIND)
        .expect("claim retry op")
        .expect("retry claim remains available");
    server
        .process_ingest_claim(
            &async_queue,
            "worker-transient-write-lock-retry",
            retry_claim,
        )
        .await
        .expect("retry should complete after lock clears");

    let completed = server
        .operation_status_json_for_test(&operation_id)
        .await
        .expect("completed status");
    assert_eq!(completed.state, Some(IngestOperationState::Completed));
    assert!(
        !completed.created_drawer_ids.is_empty(),
        "retry completion must expose cleanup-safe created drawer IDs"
    );
    assert!(completed.failure_detail.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn test_mcp_async_ingest_embed_failure_requeues_only_when_retryable() {
    let _worker_lifecycle_lock =
        crate::observability::test_support::acquire_ingest_worker_lifecycle_lock().await;
    for retryable in [true, false] {
        let tempdir = TempDir::new_in("/tmp").expect("short tempdir");
        let db_path = tempdir.path().join("palace.db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db fixture");
        let server = MempalMcpServer::new_with_factory(
            db_path.clone(),
            Arc::new(FailOnceEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
                retryable,
                failures: Arc::new(AtomicUsize::new(1)),
            }),
        )
        .expect("create MCP server")
        .with_async_db_for_test(async_db);
        let queue = PendingMessageStore::new_without_reclaim(&db_path);
        let operation_id = enqueue_prepared_test_ingest_operation(
            &server,
            &db_path,
            "admitted ingest must preserve embed failure disposition",
            "embed-failure",
        )
        .await;
        let async_queue = AsyncPendingMessageStore::from_store(queue.clone());
        let claim = queue
            .claim_next_by_kind("worker-embed-failure", 60, INGEST_ASYNC_KIND)
            .expect("claim queued op")
            .expect("claimed queued op");

        server
            .process_ingest_claim(&async_queue, "worker-embed-failure", claim)
            .await
            .expect("process forced embed failure");

        let record = queue
            .operation_status(&operation_id)
            .expect("load operation")
            .expect("operation remains durable");
        if retryable {
            assert_eq!(record.op_state, IngestOperationState::Queued.as_str());
            assert!(record.claimed_at.is_none());
            assert!(record.completed_at.is_none());
            let (retry_count, last_error): (i64, String) = Database::open(&db_path)
                .expect("open db")
                .conn()
                .query_row(
                    "SELECT retry_count, last_error FROM pending_messages WHERE id = ?1",
                    [operation_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("retry metadata");
            assert_eq!(retry_count, 1);
            assert!(last_error.contains("forced retryable embed failure"));

            tokio::time::sleep(Duration::from_secs(2)).await;
            let retry_claim = queue
                .claim_next_by_kind("worker-embed-retry", 60, INGEST_ASYNC_KIND)
                .expect("claim retry op")
                .expect("retry claim remains available");
            server
                .process_ingest_claim(&async_queue, "worker-embed-retry", retry_claim)
                .await
                .expect("retry should complete after embedder recovery");
            let completed = server
                .operation_status_json_for_test(&operation_id)
                .await
                .expect("completed status");
            assert_eq!(completed.state, Some(IngestOperationState::Completed));
            assert_eq!(
                Database::open(&db_path)
                    .expect("open db")
                    .drawer_count()
                    .expect("drawer count"),
                1
            );
        } else {
            assert_eq!(record.op_state, IngestOperationState::Failed.as_str());
            assert!(record.completed_at.is_some());
        }
    }
}
