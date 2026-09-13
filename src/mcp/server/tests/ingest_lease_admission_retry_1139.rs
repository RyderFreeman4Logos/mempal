use super::*;

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
        !text.contains("/private/")
            && !text.contains("BACKEND_SECRET")
            && !text.contains("active_cache_bytes="),
        "MCP error must not expose raw paths or backend payload: {text}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_scoped_ingest_admission_busy_at_lease_open_is_retryable() {
    use std::os::fd::AsRawFd;

    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, server) = setup_server();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    let holder_thread = Arc::new(Mutex::new(None));
    let holder_thread_for_hook = Arc::clone(&holder_thread);
    let mut server = server;
    server.ingest_writer_lease_open_hook = Some(Arc::new(move |db_path| {
        let Some(release_rx) = release_rx
            .lock()
            .expect("admission lock release receiver")
            .take()
        else {
            return Ok(());
        };
        let lock_path = db_path.with_file_name(".palace.db.admission.lock");
        let (locked_tx, locked_rx) = std::sync::mpsc::sync_channel(0);
        let thread = std::thread::spawn(move || {
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(lock_path)
                .expect("open admission lock");
            assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
            locked_tx.send(()).expect("publish held admission lock");
            release_rx.recv().expect("release held admission lock");
            assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
        });
        *holder_thread_for_hook
            .lock()
            .expect("admission lock holder thread") = Some(thread);
        locked_rx.recv().expect("observe held admission lock");
        Ok(())
    }));

    let result = server
        .mempal_ingest_with_controls_scoped_worker_releasing(
            IngestRequest {
                content: "scoped wait must retry a real busy admission lock".to_string(),
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

    release_tx.send(()).expect("release admission lock");
    holder_thread
        .lock()
        .expect("admission lock holder thread")
        .take()
        .expect("admission lock holder started")
        .join()
        .expect("admission lock holder stopped");

    let response = match result {
        Ok(Json(response)) => response,
        Err(error) => panic!(
            "admission Busy reached the generic acquire error, error={error:?} class={:?}",
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
        .expect("open after admission lock released")
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
    let failures = vec![
        (
            "permission",
            "path_or_permission",
            crate::core::db_admission::DbAdmissionError::Io {
                path: PathBuf::from("/private/permission/palace.db.admission.lock"),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "BACKEND_SECRET permission detail",
                ),
            },
        ),
        (
            "unsafe-sidecar",
            "unsafe_sidecar",
            crate::core::db_admission::DbAdmissionError::UnsafeSidecar {
                path: PathBuf::from("/private/unsafe/palace.db.admission.lock"),
                reason: "BACKEND_SECRET symlink target",
            },
        ),
        (
            "schema",
            "unsupported_schema",
            crate::core::db_admission::DbAdmissionError::UnsupportedStateVersion {
                path: PathBuf::from("/private/schema/palace.db.admission.state"),
                version: 99,
            },
        ),
        (
            "corruption",
            "corrupt_or_invalid",
            crate::core::db_admission::DbAdmissionError::InvalidState {
                path: PathBuf::from("/private/corrupt/palace.db.admission.state"),
                source: serde_json::from_str::<serde_json::Value>("not-json").unwrap_err(),
            },
        ),
    ];

    for (name, expected_class, failure) in failures {
        let (_tempdir, db_path, server) = setup_server();
        let failure = Arc::new(Mutex::new(Some(failure)));
        let mut server = server;
        server.ingest_writer_lease_open_hook = Some(Arc::new(move |_| {
            Err(failure
                .lock()
                .expect("injected lease-open error")
                .take()
                .expect("lease-open error is injected once"))
        }));
        let error = match server
            .mempal_ingest_with_controls_scoped_worker(
                IngestRequest {
                    content: format!("{name} lease-open failure must not retry"),
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
            Ok(_) => panic!("{name} lease failure must fail closed"),
            Err(error) => error,
        };

        assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
        assert_eq!(
            sanitized_failure_class(&error),
            Some(expected_class),
            "{name} failure must expose one mandatory sanitized class"
        );
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
}
