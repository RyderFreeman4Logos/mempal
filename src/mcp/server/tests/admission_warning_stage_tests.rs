use super::*;
use std::time::Instant;

#[tokio::test(flavor = "current_thread")]
async fn test_mcp_ingest_admission_db_work_runs_off_runtime() {
    let (_tempdir, _db_path, server) = setup_server();
    let server = server.with_ingest_warning_snapshot_delay_for_test(Duration::from_millis(300));
    let (ticks, ticker) = spawn_runtime_ticker();

    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "offruntime ingest admission".to_string(),
            wing: "mcp".to_string(),
            room: Some("runtime".to_string()),
            dry_run: Some(false),
            wait: Some(false),
            ..IngestRequest::default()
        }))
        .await
        .expect("ingest")
        .0;
    ticker.abort();

    assert_eq!(response.state, Some(IngestOperationState::Queued));
    assert!(response.operation_id.is_some());
    assert_runtime_ticked(&ticks, "mempal_ingest admission");
}

#[tokio::test(flavor = "current_thread")]
async fn test_mcp_ingest_admission_warning_uses_request_budget_and_returns_receipt() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, _db_path, server) = setup_server();
    let (stage_tx, stage_rx) = mpsc::channel();
    let server = server
        .with_ingest_warning_snapshot_delay_for_test(Duration::from_millis(150))
        .with_mcp_deadline_for_test(Duration::from_millis(500))
        .with_daemon_writer_lease_check_error_for_test("skip unrelated lease probe")
        .with_ingest_admission_stage_observer_for_test(stage_tx);

    let client_started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        server.mempal_ingest(Parameters(IngestRequest {
            content: "bounded ingest admission".to_string(),
            wing: "mcp".to_string(),
            room: Some("deadline".to_string()),
            dry_run: Some(false),
            wait: Some(false),
            ..IngestRequest::default()
        })),
    )
    .await
    .expect("MCP ingest should return before client timeout")
    .expect("stale-index warning snapshot must preserve a durable queue receipt")
    .0;
    let client_elapsed = client_started.elapsed();
    let stages: Vec<_> = stage_rx.try_iter().collect();
    let stage_names: Vec<_> = stages.iter().map(|(stage, _)| *stage).collect();
    let relative_ms: Vec<_> = stages
        .iter()
        .map(|(stage, observed)| (*stage, observed.duration_since(client_started).as_millis()))
        .collect();
    eprintln!(
        "admission_stage_measurement client_elapsed_ms={} stages={relative_ms:?}",
        client_elapsed.as_millis()
    );

    assert_eq!(result.state, Some(IngestOperationState::Queued));
    assert!(result.operation_id.is_some());
    assert!(!result.system_warnings.iter().any(|warning| {
        warning.source == "mcp_timeout"
            && warning
                .message
                .contains("stale vector index check exceeded")
    }));
    assert!(stages.windows(2).all(|pair| pair[0].1 <= pair[1].1));
    for required in [
        IngestAdmissionStage::PoolOpenEnter,
        IngestAdmissionStage::PoolOpenReturn,
        IngestAdmissionStage::WarningBlockingEnter,
        IngestAdmissionStage::WarningBlockingReturn,
        IngestAdmissionStage::PreparationReturn,
        IngestAdmissionStage::DurableQueueEnter,
        IngestAdmissionStage::DurableQueueReturn,
        IngestAdmissionStage::LeaseCheckEnter,
    ] {
        assert!(
            stage_names.contains(&required),
            "missing stage {required:?}"
        );
    }
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
async fn fork_fence_contention_consumes_actual_ingest_client_budget() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, _db_path, server) = setup_server();
    let (stage_tx, stage_rx) = mpsc::channel();
    let server = server
        .with_ingest_warning_snapshot_delay_for_test(Duration::from_millis(150))
        .with_mcp_deadline_for_test(Duration::from_millis(500))
        .with_daemon_writer_lease_check_error_for_test("skip unrelated lease probe")
        .with_ingest_admission_stage_observer_for_test(stage_tx);
    let (fence_acquired_tx, fence_acquired_rx) = mpsc::sync_channel(0);
    let (release_fence_tx, release_fence_rx) = mpsc::sync_channel(0);

    let fence = thread::spawn(move || {
        let guard = crate::core::db_admission::test_process_fork_write_guard_for_test();
        fence_acquired_tx
            .send(())
            .expect("report fork fence acquisition");
        release_fence_rx.recv().expect("release fork fence");
        drop(guard);
    });
    let client_started = Instant::now();
    let request = tokio::time::timeout(
        Duration::from_secs(1),
        server.mempal_ingest(Parameters(IngestRequest {
            content: "fork fence causal control".to_string(),
            wing: "mcp".to_string(),
            room: Some("deadline".to_string()),
            dry_run: Some(false),
            wait: Some(false),
            ..IngestRequest::default()
        })),
    );
    tokio::pin!(request);
    fence_acquired_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("fork fence must be held within the existing client budget");

    let request_result = request.await;
    let elapsed = client_started.elapsed();
    let blocked_stages: Vec<_> = stage_rx.try_iter().map(|(stage, _)| stage).collect();
    release_fence_tx.send(()).expect("release fork fence");
    fence.join().expect("fork fence holder");
    drop(crate::core::db_admission::test_process_fork_write_guard_for_test());

    eprintln!(
        "fork_fence_causal_measurement client_elapsed_ms={} stages={blocked_stages:?}",
        elapsed.as_millis()
    );
    assert!(
        request_result.is_err(),
        "the branch-added fork fence can consume the unchanged 1s client budget"
    );
    assert_eq!(blocked_stages, vec![IngestAdmissionStage::PoolOpenEnter]);
    assert!(elapsed >= Duration::from_secs(1));
}
