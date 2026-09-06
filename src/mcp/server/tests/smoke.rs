use super::*;

#[tokio::test]
async fn test_mcp_ingest_smoke_mode_rejects_non_smoke_scope() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
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
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
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
            wait_timeout_secs: Some(0),
            ..IngestRequest::default()
        }))
        .await
        .expect("smoke ingest should complete")
        .0;

    let operation_id = response.operation_id.as_deref().expect("operation id");
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        server.wait_for_operation_completion(operation_id),
    )
    .await
    .expect("smoke operation should complete")
    .expect("smoke operation should return a completion receipt");

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
async fn test_mcp_smoke_wait_processes_without_background_worker() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, _db_path, server) = setup_server();
    server.ingest_worker_started.store(true, Ordering::SeqCst);

    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "smoke wait must not depend on background worker scheduling".to_string(),
            wing: "smoke".to_string(),
            room: Some("mcp".to_string()),
            smoke: Some(true),
            wait: Some(true),
            wait_timeout_secs: Some(5),
            ..IngestRequest::default()
        }))
        .await
        .expect("smoke ingest should complete without a background worker")
        .0;

    assert_eq!(response.state, Some(IngestOperationState::Completed));
    assert!(!response.timed_out);
    assert!(
        !response.created_drawer_ids.is_empty(),
        "smoke wait must return cleanup-safe created ids"
    );
}

#[tokio::test]
async fn test_mcp_smoke_zero_wait_starts_background_worker() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, _db_path, server) = setup_server();

    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "zero-budget smoke wait must retain asynchronous delivery".to_string(),
            wing: "smoke".to_string(),
            room: Some("mcp".to_string()),
            smoke: Some(true),
            wait: Some(true),
            wait_timeout_secs: Some(0),
            ..IngestRequest::default()
        }))
        .await
        .expect("zero-budget smoke wait should admit the operation")
        .0;

    assert_eq!(response.state, Some(IngestOperationState::Queued));
    let operation_id = response.operation_id.as_deref().expect("operation id");
    let completed = tokio::time::timeout(
        Duration::from_secs(2),
        server.wait_for_operation_completion(operation_id),
    )
    .await
    .expect("zero-budget smoke wait should retain background delivery")
    .expect("background smoke operation should complete");

    assert_eq!(completed.state, Some(IngestOperationState::Completed));
    assert!(!completed.created_drawer_ids.is_empty());
}

#[tokio::test]
async fn test_mcp_positive_smoke_timeout_retains_completion_owner() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, _db_path, server) = setup_server();
    let server = server.with_ingest_processing_delay_for_test(Duration::from_secs(1));

    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "timed-out smoke wait must retain a local completion consumer".to_string(),
            wing: "smoke".to_string(),
            room: Some("mcp".to_string()),
            smoke: Some(true),
            wait: Some(true),
            wait_timeout_secs: Some(1),
            ..IngestRequest::default()
        }))
        .await
        .expect("short smoke wait should return a durable receipt")
        .0;

    assert!(matches!(
        response.state,
        Some(IngestOperationState::Queued | IngestOperationState::Running)
    ));
    assert!(response.timed_out);
    let operation_id = response.operation_id.as_deref().expect("operation id");
    let completed = tokio::time::timeout(
        Duration::from_secs(4),
        server.wait_for_operation_completion(operation_id),
    )
    .await
    .expect("timed-out smoke receipt must retain a local completion consumer")
    .expect("background smoke operation should complete");

    assert_eq!(completed.state, Some(IngestOperationState::Completed));
    assert!(!completed.created_drawer_ids.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn test_mcp_smoke_wait_real_source_lock_times_out_without_blocking_runtime() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, server) = setup_server();
    let request = IngestRequest {
        content: "real source lock must not block bounded smoke wait".to_string(),
        wing: "smoke".to_string(),
        room: Some("mcp".to_string()),
        smoke: Some(true),
        wait: Some(true),
        wait_timeout_secs: Some(1),
        ..IngestRequest::default()
    };

    let preview = server
        .mempal_ingest(Parameters(IngestRequest {
            dry_run: Some(true),
            ..request.clone()
        }))
        .await
        .expect("smoke preview")
        .0;
    let mempal_home = db_path.parent().expect("database parent");
    let source_lock = crate::ingest::lock::acquire_source_lock(
        mempal_home,
        &preview.drawer_id,
        Duration::from_secs(1),
    )
    .expect("hold source lock");

    let (ticks, ticker) = spawn_runtime_ticker();
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_millis(1500),
        server.mempal_ingest(Parameters(request)),
    )
    .await;
    ticker.abort();
    let response = result
        .expect("one-second smoke wait must return before sync source-lock timeout")
        .expect("source-lock contention returns a durable receipt")
        .0;

    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "source-lock contention exceeded the bounded smoke wait"
    );
    assert!(
        response.timed_out
            || response
                .state
                .map(IngestOperationState::is_terminal)
                .unwrap_or(false),
        "bounded smoke wait must return a timeout or terminal receipt"
    );
    assert_runtime_ticked(&ticks, "bounded smoke ingest with a real source lock");

    let operation_id = response.operation_id.as_deref().expect("operation id");
    drop(source_lock);
    let completed = tokio::time::timeout(
        Duration::from_secs(4),
        server.wait_for_operation_completion(operation_id),
    )
    .await
    .expect("claimed work must retain an owner after the request deadline")
    .expect("source-lock-contended smoke operation should complete");
    assert_eq!(completed.state, Some(IngestOperationState::Completed));
}

#[tokio::test(flavor = "current_thread")]
async fn test_mcp_scoped_finite_wait_real_source_lock_runs_off_runtime_and_retains_claim_owner() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, server) = setup_server();
    let request = IngestRequest {
        content: "ordinary finite wait source-lock contention ".repeat(1_000),
        wing: "mcp".to_string(),
        room: Some("runtime".to_string()),
        wait: Some(true),
        wait_timeout_secs: Some(6),
        ..IngestRequest::default()
    };
    let controls = IngestControls {
        no_gate: true,
        bypass_novelty: true,
    };

    let preview = server
        .mempal_ingest_with_controls_scoped_worker(
            IngestRequest {
                dry_run: Some(true),
                ..request.clone()
            },
            controls,
        )
        .await
        .expect("ordinary scoped preview")
        .0;
    assert!(
        preview.chunk_count > 1,
        "fixture must exercise multi-chunk ingest"
    );
    let server = server.with_ingest_processing_delay_for_test(Duration::from_secs(1));
    let mempal_home = db_path.parent().expect("database parent");
    let source_lock = crate::ingest::lock::acquire_source_lock(
        mempal_home,
        preview.drawer_ids.get(1).expect("second chunk drawer id"),
        Duration::from_secs(1),
    )
    .expect("hold source lock");

    let (ticks, ticker) = spawn_runtime_ticker();
    let started = Instant::now();
    let response = tokio::time::timeout(
        Duration::from_secs(7),
        server.mempal_ingest_with_controls_scoped_worker(request, controls),
    )
    .await
    .expect("ordinary finite scoped wait must respect its request budget")
    .expect("source-lock contention returns a durable receipt")
    .0;
    ticker.abort();

    assert!(
        response.timed_out,
        "ordinary finite wait must time out under held lock"
    );
    assert!(
        started.elapsed() < Duration::from_secs(7),
        "ordinary finite wait exceeded its request budget"
    );
    assert_runtime_ticked(&ticks, "ordinary finite ingest with a real source lock");

    let operation_id = response.operation_id.as_deref().expect("operation id");
    let record = PendingMessageStore::new_without_reclaim(&db_path)
        .operation_status(operation_id)
        .expect("load claimed operation")
        .expect("operation remains queryable");
    assert_eq!(record.op_state, IngestOperationState::Running.as_str());
    assert!(
        record.claimed_at.is_some(),
        "timed-out work must retain its owner"
    );

    drop(source_lock);
    let completed = tokio::time::timeout(
        Duration::from_secs(4),
        server.wait_for_operation_completion(operation_id),
    )
    .await
    .expect("timed-out ordinary operation must retain its completion owner")
    .expect("source-lock-contended ordinary operation should complete");
    assert_eq!(completed.state, Some(IngestOperationState::Completed));
}

#[tokio::test(flavor = "current_thread")]
async fn test_mcp_scoped_finite_lease_timeout_releases_late_acquisition() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let _observability_lock = crate::observability::test_support::global_observability_test_lock()
        .lock_owned()
        .await;
    let (_tempdir, db_path, server) = setup_server();
    let (acquired_tx, acquired_rx) = mpsc::sync_channel(1);
    let server = server.with_ingest_writer_lease_acquired_hook_for_test(Arc::new(move |_lease| {
        acquired_tx.send(()).expect("report acquired writer lease");
        std::thread::sleep(Duration::from_millis(200));
    }));
    let operation_id = enqueue_prepared_test_ingest_operation(
        &server,
        &db_path,
        "late writer lease acquisition must not outlive its cancelled waiter",
        "lease-timeout",
    )
    .await;
    let queue = PendingMessageStore::new_without_reclaim(&db_path);
    let claim = queue
        .claim_next_by_kind("worker-late-lease", 60, INGEST_ASYNC_KIND)
        .expect("claim queued operation")
        .expect("claimed queued operation");
    let async_queue = AsyncPendingMessageStore::from_store(queue);
    let (processing_started_tx, processing_started_rx) = tokio::sync::oneshot::channel::<()>();
    let processing_server = server.clone();
    let processing = tokio::spawn(async move {
        processing_started_tx
            .send(())
            .expect("report processing task start");
        processing_server
            .process_ingest_claim_inline_with_budget(
                &async_queue,
                "worker-late-lease",
                claim,
                Duration::from_millis(25),
            )
            .await
    });
    processing_started_rx
        .await
        .expect("processing task must start before lease acquisition observer");
    tokio::task::spawn_blocking(move || {
        acquired_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer lease must be acquired before the waiter times out");
    })
    .await
    .expect("acquisition observer must not panic");
    let result = processing
        .await
        .expect("bounded processing task must not panic")
        .expect("lease acquisition timeout must return a bounded result");
    assert_eq!(result, ScopedIngestProcessResult::TimedOut);

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let leases = Database::open(&db_path)
                .expect("open database after SQLite unlock")
                .runtime_writer_lease_status(Some(SQLITE_WRITER_LEASE_NAME))
                .expect("read writer lease status");
            if leases.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a lease acquired after timeout must be released, not orphaned");

    let record = PendingMessageStore::new_without_reclaim(&db_path)
        .operation_status(&operation_id)
        .expect("load operation record")
        .expect("operation remains queryable");
    assert_eq!(record.op_state, IngestOperationState::Queued.as_str());
    assert!(
        record.claimed_at.is_none(),
        "timed-out claim must be released"
    );
}

#[tokio::test]
async fn test_mcp_scoped_smoke_wait_bounds_lease_check_to_remaining_budget() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, server) = setup_server();
    let async_db = QueryOnlyAsyncDb::open(&db_path, 4)
        .expect("open query-only async db")
        .with_read_delay(Duration::from_millis(500));
    let server = server.with_query_only_async_db_for_test(async_db);

    let started = Instant::now();
    let visible = tokio::time::timeout(
        Duration::from_millis(200),
        server.daemon_writer_lease_visible_for_ingest_wait(Duration::from_millis(25), false),
    )
    .await
    .expect("scoped smoke wait lease check must use the residual wait budget");

    assert!(!visible);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "lease check exceeded the scoped smoke wait's remaining budget"
    );
}

#[tokio::test]
async fn test_mcp_scoped_smoke_preflight_uses_remaining_request_budget() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let _observability_lock = crate::observability::test_support::global_observability_test_lock()
        .lock_owned()
        .await;
    let (_tempdir, db_path, server) = setup_server();
    let async_queue = AsyncPendingMessageStore::new_without_reclaim(&db_path)
        .with_blocking_delay(Duration::from_millis(700));
    let server = server
        .with_async_queue_for_test(async_queue)
        .with_query_only_read_delay_for_test(Duration::from_millis(500));

    let response = tokio::time::timeout(
        Duration::from_millis(1100),
        server.mempal_ingest(Parameters(IngestRequest {
            content: "smoke preflight lease checks must honor the request budget".to_string(),
            wing: "smoke".to_string(),
            room: Some("mcp".to_string()),
            smoke: Some(true),
            wait: Some(true),
            wait_timeout_secs: Some(1),
            ..IngestRequest::default()
        })),
    )
    .await
    .expect("scoped smoke preflight must not outlive the request budget")
    .expect("scoped smoke preflight should return a durable receipt")
    .0;

    assert_eq!(response.state, Some(IngestOperationState::Queued));
    assert!(response.timed_out);
}

#[tokio::test]
async fn test_mcp_smoke_wait_writer_lease_error_releases_and_starts_drain() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, server) = setup_server();
    let server = server
        .with_ingest_writer_lease_failures_for_test(1)
        .with_ingest_processing_delay_for_test(Duration::from_millis(500));

    let error = match server
        .mempal_ingest(Parameters(IngestRequest {
            content: "writer lease failures must not strand admitted smoke work".to_string(),
            wing: "smoke".to_string(),
            room: Some("mcp".to_string()),
            smoke: Some(true),
            wait: Some(true),
            wait_timeout_secs: Some(5),
            ..IngestRequest::default()
        }))
        .await
    {
        Ok(_) => panic!("the injected scoped writer lease failure must reach the caller"),
        Err(error) => error,
    };

    assert!(
        error
            .to_string()
            .contains("failed to acquire scoped MCP ingest writer lease"),
        "error={error}"
    );
    let db = Database::open(&db_path).expect("open queue database");
    let operation_id = db
        .conn()
        .query_row(
            "SELECT id FROM pending_messages WHERE kind = ?1 ORDER BY rowid DESC LIMIT 1",
            [INGEST_ASYNC_KIND],
            |row| row.get::<_, String>(0),
        )
        .expect("writer lease error must retain an admitted operation");
    let completed = tokio::time::timeout(
        Duration::from_secs(4),
        server.wait_for_operation_completion(&operation_id),
    )
    .await
    .expect("writer lease failure must start a drain worker")
    .expect("drain worker should complete the released smoke operation");

    assert_eq!(completed.state, Some(IngestOperationState::Completed));
    assert!(!completed.created_drawer_ids.is_empty());
}

#[tokio::test]
async fn test_mcp_smoke_ingest_wait_update_and_status_return_created_ids() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
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

#[tokio::test]
async fn test_mcp_smoke_update_retry_keeps_owned_created_ids() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, server) = setup_server();
    let old_id = "smoke-retry-old";
    let replacement_id = "smoke-retry-new";
    let update_content = "cleanup-authoritative MCP smoke retry write two";
    insert_drawer(
        &db_path,
        old_id,
        "cleanup-authoritative MCP smoke retry write one",
        "smoke",
        Some("mcp"),
        "/tmp/smoke-retry-old.md",
        0,
    );
    insert_drawer(
        &db_path,
        replacement_id,
        update_content,
        "smoke",
        Some("mcp"),
        "/tmp/smoke-retry-new.md",
        0,
    );
    let db = Database::open(&db_path).expect("open db");
    db.conn()
        .execute(
            "UPDATE drawers SET supersedes = ?1 WHERE id = ?2",
            [old_id, replacement_id],
        )
        .expect("persist replacement ownership");

    let retried = server
        .mempal_ingest(Parameters(IngestRequest {
            content: update_content.to_string(),
            wing: "smoke".to_string(),
            room: Some("mcp".to_string()),
            source_type: Some("agent_inference".to_string()),
            memory_kind: Some("evidence".to_string()),
            domain: Some("project".to_string()),
            field: Some("smoke".to_string()),
            supersedes: Some(old_id.to_string()),
            smoke: Some(true),
            wait: Some(true),
            wait_timeout_secs: Some(5),
            ..IngestRequest::default()
        }))
        .await
        .expect("retry smoke update should complete")
        .0;

    assert_eq!(retried.state, Some(IngestOperationState::Completed));
    assert_eq!(retried.created_drawer_ids, [replacement_id.to_string()]);
    assert_eq!(retried.drawer_ids, retried.created_drawer_ids);
    assert_eq!(retried.superseded_drawer_id.as_deref(), Some(old_id));

    let unrelated = server
        .mempal_ingest(Parameters(IngestRequest {
            content: update_content.to_string(),
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
        .expect("unrelated exact match should complete")
        .0;
    assert_eq!(unrelated.drawer_id, replacement_id);
    assert!(
        unrelated.created_drawer_ids.is_empty(),
        "cross-operation exact match must not grant cleanup IDs"
    );
}
