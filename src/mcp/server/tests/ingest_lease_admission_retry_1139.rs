use super::*;

fn saturate_mcp_holders(db_path: &Path, count: usize) -> Vec<ProfileDbAdmission> {
    (0..count)
        .map(|_| {
            ProfileDbAdmission::acquire(db_path, DbAdmissionRequest::new(DbHolderClass::Mcp, 1, 1))
                .expect("fill MCP service holders")
        })
        .collect()
}

fn sanitized_failure_class(error: &ErrorData) -> Option<&str> {
    error.data.as_ref().and_then(|data| {
        data.get("reason")
            .and_then(Value::as_str)
            .or_else(|| data.get("failure_kind").and_then(Value::as_str))
            .or_else(|| {
                data.get("database_diagnostic")
                    .and_then(|diag| diag.get("failure_kind"))
                    .and_then(Value::as_str)
            })
    })
}

fn assert_sanitized_error_surface(error: &ErrorData) {
    let text = format!("{error:?}");
    assert!(
        !text.contains("/palace.db") && !text.contains("active_cache_bytes="),
        "MCP error must not expose raw paths or backend payload: {text}"
    );
}

#[tokio::test]
async fn test_scoped_ingest_holder_budget_exhaustion_after_queue_admission_is_retryable() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let tempdir = TempDir::new().expect("short tempdir");
    let db_path = tempdir.path().join("palace.db");
    Database::open(&db_path).expect("initialize isolated database");
    let queue = AsyncPendingMessageStore::new_without_reclaim(&db_path)
        .with_admission_holder_class_for_test(DbHolderClass::Mcp);
    let server = MempalMcpServer::new_with_factory(
        db_path.clone(),
        Arc::new(StubEmbedderFactory {
            vector: vec![0.1, 0.2, 0.3],
        }),
    )
    .expect("create MCP server")
    .with_async_queue_for_test(queue)
    .with_query_only_async_db_open_error_for_test("query-only warning fixture")
    .with_daemon_writer_lease_check_error_for_test("skip query-only lease probe");
    let _holders = saturate_mcp_holders(&db_path, 15);
    let snapshot = ProfileDbAdmission::snapshot(&db_path).expect("baseline holder snapshot");
    assert_eq!(snapshot.active_holders, 15);
    assert_eq!(snapshot.configured_holder_limit, 16);

    let result = server
        .mempal_ingest_with_controls_scoped_worker_releasing(
            IngestRequest {
                content: "scoped wait must retry after admitted lease open exhausts holders"
                    .to_string(),
                wing: "mcp".to_string(),
                room: Some("lease-admission".to_string()),
                wait: Some(true),
                wait_timeout_secs: Some(8),
                ..IngestRequest::default()
            },
            IngestControls {
                no_gate: true,
                bypass_novelty: true,
            },
        )
        .await;

    let peak = ProfileDbAdmission::snapshot(&db_path).expect("peak holder snapshot");
    assert!(
        peak.active_holders <= 16,
        "admission must never exceed 16 holders, got {}",
        peak.active_holders
    );

    let response = match result {
        Ok(Json(response)) => response,
        Err(error) => panic!(
            "generic acquire error/missing class, error={error:?} class={:?}",
            sanitized_failure_class(&error)
        ),
    };
    assert_eq!(response.state, Some(IngestOperationState::Queued));
    assert!(response.timed_out);
    let operation_id = response
        .operation_id
        .as_deref()
        .expect("followable queued timeout must include operation id");
    assert!(!operation_id.is_empty());
    assert!(response.created_drawer_ids.is_empty());
    assert!(response.drawer_ids.is_empty());
    assert!(response.drawer_id.is_empty());

    drop(_holders);
    let record = PendingMessageStore::new_without_reclaim(&db_path)
        .operation_status(operation_id)
        .expect("query admitted operation")
        .expect("admitted operation remains followable");
    assert_eq!(record.op_state, "queued");
    let stats = PendingMessageStore::new_without_reclaim(&db_path)
        .stats()
        .expect("queue stats");
    assert_eq!(stats.pending, 1);
    assert_eq!(stats.claimed, 0);
    let drawers = Database::open(&db_path)
        .expect("open after holders released")
        .drawer_count()
        .expect("drawer count");
    assert_eq!(drawers, 0, "retryable admission must not write drawers");
}

#[test]
fn test_writer_lease_retry_classifier_rejects_non_transient_admission() {
    let permission = anyhow::Error::new(crate::core::db_admission::DbAdmissionError::Io {
        path: PathBuf::from("palace.db.admission.lock"),
        source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    })
    .context("failed to open database for MCP ingest writer lease");
    assert!(!anyhow_chain_contains_sqlite_lock(&permission));

    let unsafe_sidecar =
        anyhow::Error::new(crate::core::db_admission::DbAdmissionError::UnsafeSidecar {
            path: PathBuf::from("palace.db.admission.lock"),
            reason: "symlink",
        })
        .context("failed to open database for MCP ingest writer lease");
    assert!(!anyhow_chain_contains_sqlite_lock(&unsafe_sidecar));

    let schema = anyhow::Error::new(
        crate::core::db_admission::DbAdmissionError::UnsupportedStateVersion {
            path: PathBuf::from("palace.db.admission.state"),
            version: 99,
        },
    )
    .context("failed to open database for MCP ingest writer lease");
    assert!(!anyhow_chain_contains_sqlite_lock(&schema));

    let corrupt = anyhow::Error::new(crate::core::db_admission::DbAdmissionError::InvalidState {
        path: PathBuf::from("palace.db.admission.state"),
        source: serde_json::from_str::<serde_json::Value>("not-json").unwrap_err(),
    })
    .context("failed to open database for MCP ingest writer lease");
    assert!(!anyhow_chain_contains_sqlite_lock(&corrupt));

    let busy = anyhow::Error::new(crate::core::db_admission::DbAdmissionError::Busy {
        path: PathBuf::from("palace.db.admission.lock"),
        timeout_ms: 250,
    })
    .context("failed to open database for MCP ingest writer lease");
    let budget = anyhow::Error::new(
        crate::core::db_admission::DbAdmissionError::BudgetExceeded {
            active_holders: 16,
            max_holders: 16,
            active_cache_bytes: 1,
            max_cache_bytes: 256 * 1024 * 1024,
            requested_cache_bytes: 16 * 1024 * 1024,
            reaped_stale_holders: 0,
            reserved_service_holders: 2,
            service_holders: 16,
            reason: crate::core::db_admission::BudgetExceededReason::HolderLimit,
        },
    )
    .context("failed to open database for MCP ingest writer lease");
    assert!(
        anyhow_chain_has_transient_admission(&busy),
        "admission Busy must share the acquire retry classifier"
    );
    assert!(
        anyhow_chain_has_transient_admission(&budget),
        "admission BudgetExceeded must share the acquire retry classifier"
    );
    assert!(!anyhow_chain_has_transient_admission(&permission));
    assert!(!anyhow_chain_has_transient_admission(&unsafe_sidecar));
    assert!(!anyhow_chain_has_transient_admission(&schema));
    assert!(!anyhow_chain_has_transient_admission(&corrupt));
}

#[tokio::test]
async fn test_scoped_ingest_non_transient_lease_open_remains_fail_closed() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, server) = setup_server();
    let server = server.with_ingest_writer_lease_failures_for_test(1);

    let error = match server
        .mempal_ingest_with_controls_scoped_worker(
            IngestRequest {
                content: "permission and schema failures must not retry".to_string(),
                wing: "mcp".to_string(),
                room: Some("lease-fail-closed".to_string()),
                wait: Some(true),
                wait_timeout_secs: Some(6),
                ..IngestRequest::default()
            },
            IngestControls {
                no_gate: true,
                bypass_novelty: true,
            },
        )
        .await
    {
        Ok(_) => panic!("non-transient lease failure must fail closed"),
        Err(error) => error,
    };

    assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
    assert!(
        error
            .to_string()
            .contains("failed to acquire scoped MCP ingest writer lease")
    );
    if let Some(class) = sanitized_failure_class(&error) {
        assert!(
            matches!(
                class,
                "path_or_permission" | "corrupt_or_invalid" | "database_degraded" | "unknown"
            ),
            "fail-closed class must stay sanitized, got {class}"
        );
    }
    assert_sanitized_error_surface(&error);

    let stats = PendingMessageStore::new_without_reclaim(&db_path)
        .stats()
        .expect("queue stats");
    assert_eq!(stats.claimed, 0);
    let drawers = Database::open(&db_path)
        .expect("open after fail-closed lease")
        .drawer_count()
        .expect("drawer count");
    assert_eq!(drawers, 0);
}
