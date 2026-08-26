use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_worker_successful_claim_clears_observed_contention() {
    let _guard = crate::core::config::global_config_test_lock()
        .lock_owned()
        .await;
    let _shutdown_guard = crate::daemon::global_shutdown_test_lock()
        .lock_owned()
        .await;
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let _config_reset = ConfigHarnessResetGuard;
    let config_path = tmp.path().join("config.toml");
    let db_path = tmp.path().join("palace.db");
    let request_count = Arc::new(AtomicUsize::new(0));
    let request_notify = Arc::new(Notify::new());
    let response_release = Arc::new(Notify::new());
    let (base_url, server) = spawn_counting_llm_server_with_response_gate(
        Arc::clone(&request_count),
        Arc::clone(&request_notify),
        Some(Arc::clone(&response_release)),
    )
    .await;
    std::fs::write(&config_path, worker_test_config(&base_url)).expect("write worker config");
    ConfigHandle::bootstrap_quiet(&config_path).expect("bootstrap worker config");

    let db = Database::open(&db_path).expect("open db");
    let store = PendingMessageStore::new(db.path()).expect("open queue");
    insert_drawer(&db, "claim-contention-cleared");
    record_pending_llm_audit(&db, "claim-contention-cleared");
    store
        .enqueue(
            LLM_TASK_KIND,
            &serde_json::to_string(&gating_task("claim-contention-cleared"))
                .expect("serialize task"),
        )
        .expect("enqueue LLM task");
    let async_store = AsyncPendingMessageStore::from_store(store.clone())
        .with_claim_lock_failures_for_test(1);
    let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
    let client_runtime = Arc::new(Mutex::new(LlmClientRuntime::new(
        &ConfigHandle::current().llm,
    )));
    let (runtime_locked_tx, runtime_locked_rx) = std::sync::mpsc::sync_channel(1);
    let (runtime_release_tx, runtime_release_rx) = std::sync::mpsc::sync_channel(1);
    let locked_runtime = Arc::clone(&client_runtime);
    let runtime_holder = std::thread::spawn(move || {
        let _guard = locked_runtime.lock().expect("lock LLM client runtime");
        runtime_locked_tx
            .send(())
            .expect("report locked LLM client runtime");
        let _ = runtime_release_rx.recv();
    });
    runtime_locked_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("LLM client runtime should be locked before worker spawn");
    let test_lease = db
        .runtime_writer_lease_acquire("sqlite-writer", "test", "llm-worker-test", 300, None)
        .expect("acquire test lease")
        .expect("test lease available");
    let observer = DaemonWriteObserver::for_test();
    let (worker_completed, completion) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(run_llm_worker_inner(
        Arc::new(async_store),
        client_runtime,
        Arc::new(LlmStatus::new(5)),
        async_db,
        observer.clone(),
        test_lease,
        Some(worker_completed),
    ));

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if observer.last_error_for_test().is_some_and(|(_, lock)| lock) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first claim lock should reach the observer");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if observer.last_error_for_test().is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("successful claim should clear observed contention");
    assert_eq!(
        observer.last_error_for_test(),
        None,
        "successful LLM claim must clear contention before semantic completion"
    );

    runtime_release_tx
        .send(())
        .expect("release LLM client runtime");
    runtime_holder
        .join()
        .expect("LLM client runtime holder should exit");
    tokio::time::timeout(Duration::from_secs(5), request_notify.notified())
        .await
        .expect("worker should start the claimed LLM request");
    assert_eq!(
        request_count.load(Ordering::SeqCst),
        1,
        "the first request must be observed before its response is released"
    );
    response_release.notify_one();
    tokio::time::timeout(Duration::from_secs(20), completion)
        .await
        .expect("worker should complete the claimed task")
        .expect("worker completion observer should remain connected");
    worker.abort();
    let _ = worker.await;
    server.abort();
    let _ = server.await;
    assert_eq!(request_count.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_worker_survives_mark_failed_lock_contention() {
    let _guard = crate::core::config::global_config_test_lock()
        .lock_owned()
        .await;
    let _shutdown_guard = crate::daemon::global_shutdown_test_lock()
        .lock_owned()
        .await;
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let _config_reset = ConfigHarnessResetGuard;
    let config_path = tmp.path().join("config.toml");
    let db_path = tmp.path().join("palace.db");
    let request_count = Arc::new(AtomicUsize::new(0));
    let request_notify = Arc::new(Notify::new());
    let (base_url, server) =
        spawn_counting_llm_server(Arc::clone(&request_count), request_notify).await;
    std::fs::write(&config_path, worker_test_config(&base_url)).expect("write worker config");
    ConfigHandle::bootstrap_quiet(&config_path).expect("bootstrap worker config");

    let db = Database::open(&db_path).expect("open db");
    let store = PendingMessageStore::new(db.path()).expect("open queue");
    store
        .enqueue(LLM_TASK_KIND, "{}")
        .expect("enqueue malformed first task");
    insert_drawer(&db, "mark-failed-lock-second");
    record_pending_llm_audit(&db, "mark-failed-lock-second");
    store
        .enqueue(
            LLM_TASK_KIND,
            &serde_json::to_string(&gating_task("mark-failed-lock-second"))
                .expect("serialize task"),
        )
        .expect("enqueue second LLM task");
    let async_store = AsyncPendingMessageStore::from_store(store.clone())
        .with_complete_lock_failures_for_test(1);
    let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
    let client_runtime = Arc::new(Mutex::new(LlmClientRuntime::new(
        &ConfigHandle::current().llm,
    )));
    let test_lease = db
        .runtime_writer_lease_acquire("sqlite-writer", "test", "llm-worker-test", 300, None)
        .expect("acquire test lease")
        .expect("test lease available");
    let (worker_completed, completion) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(run_llm_worker_inner(
        Arc::new(async_store),
        client_runtime,
        Arc::new(LlmStatus::new(5)),
        async_db,
        DaemonWriteObserver::for_test(),
        test_lease,
        Some(worker_completed),
    ));

    tokio::time::timeout(Duration::from_secs(20), completion)
        .await
        .expect("worker should retain capacity after mark_failed lock")
        .expect("worker completion observer should remain connected");
    worker.abort();
    let _ = worker.await;
    server.abort();
    let _ = server.await;

    assert_eq!(request_count.load(Ordering::SeqCst), 1);
    let stats = store.stats().expect("queue stats");
    assert_eq!((stats.pending, stats.claimed), (0, 1));
}
