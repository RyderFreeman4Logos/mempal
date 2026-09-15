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

async fn claimed_test_ingest(
    server: &MempalMcpServer,
    db_path: &Path,
    worker_id: &str,
    content: &str,
) -> (
    PendingMessageStore,
    AsyncPendingMessageStore,
    String,
    ClaimedMessage,
) {
    let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
    let request = IngestRequest {
        content: content.to_string(),
        wing: "mcp".to_string(),
        room: Some("lease-lifecycle".to_string()),
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
            Instant::now() + MCP_INGEST_ADMISSION_DEADLINE,
        )
        .await
        .expect("prepare async ingest");
    let payload = serde_json::to_string(&prepared).expect("serialize prepared ingest");
    let queue = PendingMessageStore::new_without_reclaim(db_path);
    let operation_id = queue
        .enqueue(INGEST_ASYNC_KIND, &payload)
        .expect("enqueue async ingest");
    let claim = queue
        .claim_next_by_kind(worker_id, 60, INGEST_ASYNC_KIND)
        .expect("claim queued op")
        .expect("claimed queued op");
    let async_queue = AsyncPendingMessageStore::from_store(queue.clone());
    (queue, async_queue, operation_id, claim)
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
    assert_eq!(
        status_db_failure_kind(permission.as_ref()),
        "path_or_permission"
    );
    assert_eq!(
        status_db_failure_kind(unsafe_sidecar.as_ref()),
        "unsafe_sidecar"
    );
    assert_eq!(
        status_db_failure_kind(schema.as_ref()),
        "unsupported_schema"
    );
    assert_eq!(
        status_db_failure_kind(corrupt.as_ref()),
        "corrupt_or_invalid"
    );

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

#[tokio::test(flavor = "current_thread")]
async fn test_scoped_ingest_timely_lease_failure_returns_error_and_releases_claim() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, server) = setup_server();
    let server = server.with_ingest_writer_lease_failures_for_test(1);

    let error = match tokio::time::timeout(
        Duration::from_secs(3),
        server.mempal_ingest_with_controls_scoped_worker(
            IngestRequest {
                content: "timely lease failure must fail closed before the request deadline"
                    .to_string(),
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
        ),
    )
    .await
    .expect("pre-spawn lease failure must arrive before the request deadline")
    {
        Ok(_) => panic!("timely lease failure must return a synchronous MCP error"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
    assert_sanitized_error_surface(&error);

    let queue = PendingMessageStore::new_without_reclaim(&db_path);
    let claim = queue
        .claim_next_by_kind("timely-failure-verifier", 60, INGEST_ASYNC_KIND)
        .expect("claim retryable operation")
        .expect("timely failure must release its exact queue row");
    let operation_id = claim.id.clone();
    queue
        .release_claim(&claim)
        .expect("release verifier claim after proving retryability");
    let record = queue
        .operation_status(&operation_id)
        .expect("query timely failure operation")
        .expect("timely failure operation must remain queryable");
    assert_eq!(record.op_state, IngestOperationState::Queued.as_str());
    assert!(record.claimed_at.is_none());
    assert_eq!(
        Database::open(&db_path)
            .expect("open after timely lease failure")
            .drawer_count()
            .expect("drawer count"),
        0
    );
    assert!(
        Database::open(&db_path)
            .expect("open lease status after timely failure")
            .runtime_writer_lease_status(Some(SQLITE_WRITER_LEASE_NAME))
            .expect("read writer leases")
            .is_empty(),
        "timely lease failure must not strand a writer lease"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_owned_scoped_ingest_lease_acquire_uses_original_deadline() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, mut server) = setup_server();
    let (queue, async_queue, operation_id, claim) = claimed_test_ingest(
        &server,
        &db_path,
        "worker-owned-deadline",
        "late successful lease acquisition must not restart the full budget",
    )
    .await;
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    server.ingest_writer_lease_open_hook = Some(Arc::new(move |_| {
        entered_tx.send(()).expect("report lease-open entry");
        release_rx
            .lock()
            .expect("lease-open release receiver")
            .take()
            .expect("lease-open release receiver used once")
            .recv()
            .expect("release lease-open");
        Ok(())
    }));
    let processing = tokio::spawn(async move {
        server
            .process_ingest_claim_with_owned_task_budget(
                &async_queue,
                "worker-owned-deadline",
                claim,
                Duration::from_millis(20),
            )
            .await
    });
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(1)))
        .await
        .expect("lease-open observer must not panic")
        .expect("lease-open must start");
    let mut processing = processing;
    let bounded = tokio::time::timeout(Duration::from_millis(100), &mut processing).await;
    let returned_before_release = bounded.is_ok();
    let drawers_before_release = Database::open(&db_path)
        .expect("open before releasing lease-open")
        .drawer_count()
        .expect("drawer count before release");
    release_tx.send(()).expect("release lease-open");
    let result = match bounded {
        Ok(joined) => joined,
        Err(_) => processing.await,
    }
    .expect("owned processing task must not panic")
    .expect("late successful acquire must return a typed timeout");

    assert!(
        returned_before_release,
        "lease acquisition exceeded its supplied budget"
    );
    assert_eq!(result, ScopedIngestProcessResult::TimedOut);
    assert_eq!(
        drawers_before_release, 0,
        "timed-out acquire wrote before return"
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let record = queue
                .operation_status(&operation_id)
                .expect("load operation")
                .expect("operation exists");
            if record.op_state == IngestOperationState::Queued.as_str()
                && record.claimed_at.is_none()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("late acquire must release its claim without durable work");
    assert_eq!(
        Database::open(&db_path)
            .expect("open after late acquire")
            .drawer_count()
            .expect("drawer count after late acquire"),
        0,
        "a queued timeout must not conceal durable completion"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_scoped_ingest_late_non_transient_lease_open_records_failure() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, mut server) = setup_server();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    server.ingest_writer_lease_open_hook = Some(Arc::new(move |_| {
        entered_tx.send(()).expect("report lease-open entry");
        release_rx
            .lock()
            .expect("lease-open release receiver")
            .take()
            .expect("lease-open release receiver used once")
            .recv()
            .expect("release lease-open");
        Err(crate::core::db_admission::DbAdmissionError::InvalidState {
            path: PathBuf::from("/private/corrupt/palace.db.admission.state"),
            source: serde_json::from_str::<serde_json::Value>("not-json").unwrap_err(),
        })
    }));
    let request_server = server.clone();
    let mut request = tokio::spawn(async move {
        request_server
            .mempal_ingest_with_controls_scoped_worker(
                IngestRequest {
                    content: "late corruption must return a non-success receipt".to_string(),
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
    });
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(2)))
        .await
        .expect("lease-open observer must not panic")
        .expect("lease-open must start");
    let response_before_release = tokio::time::timeout(Duration::from_secs(8), &mut request).await;
    let returned_before_release = response_before_release.is_ok();
    release_tx
        .send(())
        .expect("release late lease-open failure");
    let response = match response_before_release {
        Ok(joined) => joined,
        Err(_) => tokio::time::timeout(Duration::from_secs(5), &mut request)
            .await
            .expect("request must settle during cleanup"),
    }
    .expect("request task must not panic")
    .unwrap_or_else(|error| {
        panic!(
            "late failure must not cross the elapsed caller deadline as a synchronous error: {error:?}"
        )
    })
    .0;

    assert!(
        returned_before_release,
        "caller must return before the late lease-open failure is released"
    );
    assert!(
        response.timed_out || response.state == Some(IngestOperationState::Failed),
        "late corruption must return a non-success receipt: {response:?}"
    );
    assert_ne!(response.state, Some(IngestOperationState::Completed));
    assert!(response.created_drawer_ids.is_empty());
    assert!(response.drawer_ids.is_empty());
    assert!(response.drawer_id.is_empty());
    let operation_id = response
        .operation_id
        .as_deref()
        .expect("late failure receipt must remain queryable");

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let record = PendingMessageStore::new_without_reclaim(&db_path)
                .operation_status(operation_id)
                .expect("load late failure operation")
                .expect("late failure operation exists");
            if record.op_state == IngestOperationState::Failed.as_str() {
                let detail = record
                    .failure_detail
                    .as_deref()
                    .expect("late failure must persist a sanitized detail");
                assert!(detail.contains("corrupt_or_invalid"), "{record:?}");
                assert!(!detail.contains("/private/") && !detail.contains("BACKEND_SECRET"));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("late non-transient acquire must reach durable failure");
    let stats = PendingMessageStore::new_without_reclaim(&db_path)
        .stats()
        .expect("queue stats after late failure");
    assert_eq!(stats.claimed, 0);
    assert_eq!(
        Database::open(&db_path)
            .expect("open after late failure")
            .drawer_count()
            .expect("drawer count"),
        0
    );
    assert!(
        Database::open(&db_path)
            .expect("open lease status after late failure")
            .runtime_writer_lease_status(Some(SQLITE_WRITER_LEASE_NAME))
            .expect("read writer leases")
            .is_empty(),
        "late lease-open failure must not strand a writer lease"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_owned_scoped_ingest_caller_abort_retains_claim_owner_during_acquire() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, mut server) = setup_server();
    let (queue, async_queue, operation_id, claim) = claimed_test_ingest(
        &server,
        &db_path,
        "worker-owned-cancel",
        "caller cancellation must not strand a claimed row",
    )
    .await;
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    server.ingest_writer_lease_open_hook = Some(Arc::new(move |_| {
        entered_tx.send(()).expect("report lease-open entry");
        release_rx
            .lock()
            .expect("lease-open release receiver")
            .take()
            .expect("lease-open release receiver used once")
            .recv()
            .expect("release lease-open");
        Ok(())
    }));
    let caller = tokio::spawn(async move {
        server
            .process_ingest_claim_with_owned_task_budget(
                &async_queue,
                "worker-owned-cancel",
                claim,
                Duration::from_secs(5),
            )
            .await
    });
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(1)))
        .await
        .expect("lease-open observer must not panic")
        .expect("lease-open must start");
    caller.abort();
    assert!(
        caller
            .await
            .expect_err("caller task must be cancelled")
            .is_cancelled()
    );
    release_tx.send(()).expect("release lease-open");

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let record = queue
                .operation_status(&operation_id)
                .expect("load operation")
                .expect("operation exists");
            if record.op_state == IngestOperationState::Completed.as_str() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the owned task must complete without waiting for stale-claim TTL");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if Database::open(&db_path)
                .expect("open after cancelled caller")
                .runtime_writer_lease_status(Some(SQLITE_WRITER_LEASE_NAME))
                .expect("read writer leases")
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned task must release the writer lease");
}

#[tokio::test(flavor = "current_thread")]
async fn test_owned_scoped_ingest_reports_explicit_writer_lease_release_failure() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, server) = setup_server();
    let (_queue, async_queue, _operation_id, claim) = claimed_test_ingest(
        &server,
        &db_path,
        "worker-owned-release",
        "normal completion must report writer lease release failure",
    )
    .await;
    INGEST_WRITER_LEASE_RELEASE_LOCK_FAILURES.store(1, Ordering::SeqCst);
    let result = server
        .process_ingest_claim_with_owned_task_budget(
            &async_queue,
            "worker-owned-release",
            claim,
            Duration::from_secs(5),
        )
        .await;
    let leases = Database::open(&db_path)
        .expect("open after forced release lock")
        .runtime_writer_lease_status(Some(SQLITE_WRITER_LEASE_NAME))
        .expect("read writer leases");
    let leaked_lease = !leases.is_empty();
    for lease in leases {
        Database::open(&db_path)
            .expect("open RED cleanup database")
            .runtime_writer_lease_release(&lease)
            .expect("clean exact leaked test lease");
    }

    assert!(
        result.is_err(),
        "release lock must not report clean processing success"
    );
    assert!(
        !leaked_lease,
        "RAII fallback must clear the exact lease after explicit release fails"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_ingest_writer_lease_acquire_bypasses_profile_admission_lock() {
    use std::os::fd::AsRawFd;

    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, server) = setup_server();
    let lock_path = db_path.with_file_name(".palace.db.admission.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .expect("open admission lock");
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);

    let acquired = tokio::time::timeout(
        Duration::from_secs(2),
        server.acquire_ingest_writer_lease("mcp-ingest-worker-admission-independent"),
    )
    .await;
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);

    let lease = acquired
        .expect("writer lease acquisition deadlocked behind profile admission")
        .expect("writer lease acquisition must not consume profile admission")
        .expect("writer lease must be available");
    lease.release().await.expect("release writer lease");
}
