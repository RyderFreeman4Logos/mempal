use super::*;
use std::path::Path;
use std::time::Instant;

#[cfg(target_os = "linux")]
use crate::core::db_admission_test_process::{self, DeadlineChild, SpawnSpec, SupervisionError};
#[cfg(target_os = "linux")]
const _: fn() = db_admission_test_process::reference_shared_test_api;

#[cfg(target_os = "linux")]
const FORK_FENCE_FIXTURE_ROOT_ENV: &str = "MEMPAL_FORK_FENCE_FIXTURE_ROOT";
#[cfg(target_os = "linux")]
const FORK_FENCE_TEST: &str = "mcp::server::tests::admission_warning_stage_tests::fork_fence_contention_refuses_ingest_within_admission_budget";

#[tokio::test(flavor = "current_thread")]
async fn test_mcp_ingest_admission_db_work_runs_off_runtime() {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
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
    .await;
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
    let result = result
        .expect("MCP ingest should return before client timeout")
        .expect("stale-index warning snapshot must preserve a durable queue receipt")
        .0;

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
async fn fork_fence_contention_refuses_ingest_within_admission_budget() {
    if let Some(root) = std::env::var_os(FORK_FENCE_FIXTURE_ROOT_ENV) {
        run_fork_fence_contention_fixture(Path::new(&root)).await;
        return;
    }

    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let fixture = tempfile::tempdir().expect("fixture marker directory");
    let ready = fixture.path().join("ready");
    let busy = fixture.path().join("busy");
    let release = fixture.path().join("release");
    let executable = std::env::current_exe().expect("current unit-test executable");
    let mut spec = SpawnSpec::new(executable).expect("absolute unit-test executable");
    spec.args([
        "--exact",
        FORK_FENCE_TEST,
        "--nocapture",
        "--test-threads=1",
    ])
    .env(FORK_FENCE_FIXTURE_ROOT_ENV, fixture.path().as_os_str());

    let child = thread::spawn(move || DeadlineChild::output(spec, Duration::from_secs(30)));
    let ready_seen = wait_for_marker(&ready, Duration::from_secs(10));
    let neighbor = ready_seen.then(|| {
        let temp = tempfile::tempdir().expect("concurrent neighbor temp dir");
        let result = Database::open(&temp.path().join("palace.db"));
        (temp, result)
    });
    let busy_seen = ready_seen && wait_for_marker(&busy, Duration::from_secs(10));
    fs::write(&release, b"").expect("release fork-fence fixture");
    let output = match child.join().expect("fork-fence fixture supervisor") {
        Ok(output) => output,
        Err(SupervisionError::CleanupIncomplete(incomplete)) => incomplete
            .finish_output(Duration::from_secs(5))
            .expect("finish fork-fence fixture cleanup"),
        Err(error) => panic!("run fork-fence fixture: {error:?}"),
    };

    assert!(ready_seen, "child did not acquire its fork fence");
    assert!(busy_seen, "child did not prove bounded AdmissionBusy");
    let (_temp, neighbor) = neighbor.expect("ready child requires a concurrent neighbor");
    drop(neighbor.expect("child-local fork fence must not block parent admission"));
    assert!(
        output.success(),
        "fork-fence fixture failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.timed_out);
    assert!(output.cleanup.kill_fence_sent);
    assert!(output.cleanup.errors.is_empty(), "{:#?}", output.cleanup);
}

#[cfg(target_os = "linux")]
async fn run_fork_fence_contention_fixture(fixture: &Path) {
    let _worker_lifecycle_lock = acquire_ingest_worker_lifecycle_lock().await;
    let (_tempdir, db_path, server) = setup_server();
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
    fence_acquired_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("fork fence must be held within the existing client budget");
    fs::write(fixture.join("ready"), b"").expect("publish held fork fence");

    let client_started = Instant::now();
    let request_result = tokio::time::timeout(
        Duration::from_secs(1),
        server.mempal_ingest(Parameters(IngestRequest {
            content: "fork fence bounded refusal".to_string(),
            wing: "mcp".to_string(),
            room: Some("deadline".to_string()),
            dry_run: Some(false),
            wait: Some(false),
            ..IngestRequest::default()
        })),
    )
    .await;
    let elapsed = client_started.elapsed();
    let blocked_stages: Vec<_> = stage_rx.try_iter().map(|(stage, _)| stage).collect();

    let error = match request_result
        .expect("fork-fence admission must finish before the 1s client watchdog")
    {
        Ok(_) => panic!("held fork fence admitted ingest before SQLite opened"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
    assert!(
        error
            .message
            .contains("write admission was not confirmed (locked_or_busy)"),
        "unexpected admission error: {error:?}"
    );
    let data = error.data.expect("typed admission refusal data");
    assert_eq!(data["reason"], "database_locked");
    assert_eq!(data["action"], "retry_after_transient_lock");
    assert_eq!(blocked_stages, vec![IngestAdmissionStage::PoolOpenEnter]);
    assert!(elapsed < Duration::from_millis(500), "elapsed: {elapsed:?}");
    fs::write(fixture.join("busy"), b"").expect("publish bounded refusal proof");
    assert!(
        wait_for_marker(&fixture.join("release"), Duration::from_secs(10)),
        "parent did not release fork-fence fixture"
    );
    release_fence_tx.send(()).expect("release fork fence");
    fence.join().expect("fork fence holder");
    drop(crate::core::db_admission::test_process_fork_write_guard_for_test());
    // The sibling request-budget test owns full-ingest coverage; this control only proves
    // that releasing the injected fence restores the real admission path.
    drop(Database::open(&db_path).expect("released fork fence must admit a real database open"));
}

#[cfg(target_os = "linux")]
fn wait_for_marker(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.is_file() {
            return true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    path.is_file()
}
