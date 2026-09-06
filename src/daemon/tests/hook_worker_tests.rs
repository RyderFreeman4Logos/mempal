use super::*;

#[cfg(unix)]
#[tokio::test]
async fn test_bounded_hook_worker_continues_claiming_after_completed_batch() {
    let _shutdown_lock = super::super::global_shutdown_test_lock().lock_owned().await;
    let _shutdown_guard = ShutdownResetGuard::new();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let mempal_home = tmp.path().join(".mempal");
    std::fs::create_dir_all(&mempal_home).expect("create mempal home");
    Database::open(&db_path).expect("open db");
    let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
    let store = PendingMessageStore::new(&db_path).expect("store");
    let async_store = AsyncPendingMessageStore::from_store(store.clone());

    for label in ["first", "second"] {
        let hook_payload = serde_json::json!({
            "tool_name": "Bash",
            "input": format!("printf {label}"),
            "output": label,
            "exit_code": 0
        })
        .to_string();
        let envelope = CapturedHookEnvelope {
            event: HookEvent::PostToolUse.display_name().to_string(),
            kind: HookEvent::PostToolUse.queue_kind().to_string(),
            agent: "codex".to_string(),
            captured_at: "2026-05-01T12:34:56Z".to_string(),
            claude_cwd: tmp.path().to_string_lossy().to_string(),
            payload: Some(hook_payload.clone()),
            payload_path: None,
            payload_preview: None,
            original_size_bytes: hook_payload.len(),
            truncated: false,
        };
        let payload = serde_json::to_string(&envelope).expect("serialize envelope");
        store
            .enqueue(HookEvent::PostToolUse.queue_kind(), &payload)
            .expect("enqueue hook envelope");
    }

    let config = Config::default();
    assert!(
        !config.llm.enabled,
        "test runtime config must keep LLM disabled"
    );
    let idle_observer = Arc::new(Notify::new());
    let idle = idle_observer.notified();
    tokio::pin!(idle);
    idle.as_mut().enable();
    let worker = tokio::spawn(run_hook_worker(
        HookWorkerState {
            async_db,
            db_path: db_path.clone(),
            store: async_store,
            worker_id: "bounded-continuation-worker".to_string(),
            embedder: Arc::new(DaemonEmbedder::from_primary_for_test(Box::new(
                StaticEmbedder,
            ))),
            prototype_classifier: Arc::new(ArcSwap::from_pointee(None)),
            llm_gate: None,
            config: Arc::new(config),
            mempal_home,
            write_observer: crate::daemon_bootstrap::DaemonWriteObserver::for_test(),
            runtime_writer_lease: None,
            idle_observer: Some(Arc::clone(&idle_observer)),
        },
        60,
        Duration::from_millis(10),
    ));

    tokio::time::timeout(Duration::from_secs(5), idle)
        .await
        .expect("worker should enter idle after completing queued hooks");

    let stats = store.stats().expect("queue stats");
    assert_eq!(stats.pending, 0);
    assert_eq!(stats.claimed, 0);
    let completed: i64 = rusqlite::Connection::open(&db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT COUNT(*) FROM pending_message_completions WHERE kind = ?1",
            [HookEvent::PostToolUse.queue_kind()],
            |row| row.get(0),
        )
        .expect("count completions");
    assert_eq!(
        completed, 2,
        "worker should continue claiming after first completion"
    );

    let telemetry_db = Database::open(&db_path).expect("open telemetry db");
    let hook_row = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let telemetry = operation_telemetry_summary(
                &telemetry_db,
                OperationTelemetrySummaryOptions {
                    since_unix_ms: None,
                    limit: 10,
                },
            )
            .expect("summarize daemon hook telemetry");
            if let Some(row) = telemetry.into_iter().find(|row| {
                row.source == "daemon"
                    && row.operation == "hook hook_post_tool"
                    && row.call_site == "daemon.hook_worker.message"
                    && row.operation_count == 2
            }) {
                break row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("daemon hook operation telemetry should record both completed hooks");
    assert_eq!(hook_row.operation_count, 2);
    assert_eq!(hook_row.success_count, 2);
    assert_eq!(hook_row.error_count, 0);

    request_shutdown();
    tokio::time::timeout(Duration::from_secs(1), worker)
        .await
        .expect("worker should observe shutdown")
        .expect("worker task should not panic");
}

#[tokio::test]
async fn test_bounded_hook_worker_heartbeats_with_claim_worker_id() {
    let _shutdown_lock = super::super::global_shutdown_test_lock().lock_owned().await;
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let mempal_home = tmp.path().join(".mempal");
    std::fs::create_dir_all(&mempal_home).expect("create mempal home");
    Database::open(&db_path).expect("open db");
    let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
    let store = PendingMessageStore::new(&db_path).expect("store");
    let async_store = AsyncPendingMessageStore::from_store(store.clone());

    let hook_payload = serde_json::json!({
        "tool_name": "Bash",
        "input": "printf heartbeat",
        "output": "ok",
        "exit_code": 0
    })
    .to_string();
    let envelope = CapturedHookEnvelope {
        event: HookEvent::PostToolUse.display_name().to_string(),
        kind: HookEvent::PostToolUse.queue_kind().to_string(),
        agent: "codex".to_string(),
        captured_at: "2026-05-01T12:34:56Z".to_string(),
        claude_cwd: tmp.path().to_string_lossy().to_string(),
        payload: Some(hook_payload.clone()),
        payload_path: None,
        payload_preview: None,
        original_size_bytes: hook_payload.len(),
        truncated: false,
    };
    let payload = serde_json::to_string(&envelope).expect("serialize envelope");
    let queued_id = store
        .enqueue(HookEvent::PostToolUse.queue_kind(), &payload)
        .expect("enqueue hook envelope");
    let worker_id = "bounded-hook-worker";
    let message = store
        .claim_next(worker_id, 60)
        .expect("claim next")
        .expect("claimed message");
    assert_eq!(message.id, queued_id);

    let stale_heartbeat_at = unix_now_secs() - 30;
    rusqlite::Connection::open(&db_path)
        .expect("open sqlite")
        .execute(
            "UPDATE pending_messages SET claimed_at = ?2, heartbeat_at = ?2 WHERE id = ?1",
            rusqlite::params![queued_id, stale_heartbeat_at],
        )
        .expect("age heartbeat");

    let config = Config::default();
    assert!(
        !config.llm.enabled,
        "test runtime config must keep LLM disabled"
    );
    super::super::process_hook_worker_message(
        HookWorkerState {
            async_db,
            db_path: db_path.clone(),
            store: async_store,
            worker_id: worker_id.to_string(),
            embedder: std::sync::Arc::new(DaemonEmbedder::from_primary_for_test(Box::new(
                HeartbeatProbeEmbedder {
                    db_path,
                    message_id: message.id.clone(),
                    stale_heartbeat_at,
                    attempts: AtomicUsize::new(0),
                },
            ))),
            prototype_classifier: std::sync::Arc::new(ArcSwap::from_pointee(None)),
            llm_gate: None,
            config: std::sync::Arc::new(config),
            mempal_home,
            write_observer: crate::daemon_bootstrap::DaemonWriteObserver::for_test(),
            runtime_writer_lease: None,
            idle_observer: None,
        },
        message,
        60,
    )
    .await;
}
