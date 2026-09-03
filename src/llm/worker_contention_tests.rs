use super::*;

#[test]
fn test_worker_successful_claim_clears_observed_contention() {
    super::with_isolated_llm_worker_runtime(async {
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
        let (base_url, server) = spawn_counting_llm_server_with_expected_content(
            Arc::clone(&request_count),
            Arc::clone(&request_notify),
            Some("claim-contention-cleared".to_string()),
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
                &serde_json::to_string(&LlmTaskPayload {
                    content: "claim-contention-cleared".to_string(),
                    ..gating_task("claim-contention-cleared")
                })
                .expect("serialize task"),
            )
            .expect("enqueue LLM task");
        let async_store = AsyncPendingMessageStore::from_store(store.clone())
            .with_claim_lock_failures_for_test(1);
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let client_runtime = SharedLlmClientRuntime::new(&ConfigHandle::current().llm);
        let (runtime_locked_tx, runtime_locked_rx) = std::sync::mpsc::sync_channel(1);
        let (runtime_release_tx, runtime_release_rx) = std::sync::mpsc::sync_channel(1);
        let locked_runtime = client_runtime.clone();
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
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
        tokio::time::timeout(Duration::from_secs(20), completion)
            .await
            .expect("worker should complete the claimed task")
            .expect("worker completion observer should remain connected");
        worker.abort();
        let _ = worker.await;
        server.abort();
        let _ = server.await;
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn test_worker_survives_mark_failed_lock_contention() {
    super::with_isolated_llm_worker_runtime(async {
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
        let (base_url, server) = spawn_counting_llm_server_with_expected_content(
            Arc::clone(&request_count),
            request_notify,
            Some("mark-failed-lock-second".to_string()),
        )
        .await;
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
                &serde_json::to_string(&LlmTaskPayload {
                    content: "mark-failed-lock-second".to_string(),
                    ..gating_task("mark-failed-lock-second")
                })
                .expect("serialize task"),
            )
            .expect("enqueue second LLM task");
        let async_store = AsyncPendingMessageStore::from_store(store.clone())
            .with_complete_lock_failures_for_test(1);
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let client_runtime = SharedLlmClientRuntime::new(&ConfigHandle::current().llm);
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
    });
}

#[test]
fn test_worker_gating_uses_endpoint_pool_fallback() {
    super::with_isolated_llm_worker_runtime(async {
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

        let primary_count = Arc::new(AtomicUsize::new(0));
        let secondary_count = Arc::new(AtomicUsize::new(0));
        let primary_notify = Arc::new(Notify::new());
        let secondary_notify = Arc::new(Notify::new());
        let (primary_base_url, primary_server) =
            spawn_failing_llm_server(Arc::clone(&primary_count), Arc::clone(&primary_notify)).await;
        let (secondary_base_url, secondary_server) =
            spawn_counting_llm_server(Arc::clone(&secondary_count), Arc::clone(&secondary_notify))
                .await;

        std::fs::write(
            &config_path,
            worker_endpoint_pool_config(&primary_base_url, &secondary_base_url),
        )
        .expect("write endpoint pool config");
        ConfigHandle::bootstrap_quiet(&config_path).expect("bootstrap endpoint pool config");

        let db = Database::open(&db_path).expect("open db");
        let drawer_id = "endpoint-pool-fallback-drawer";
        insert_drawer(&db, drawer_id);
        record_pending_llm_audit(&db, drawer_id);
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        let task = gating_task(drawer_id);
        store
            .enqueue(
                LLM_TASK_KIND,
                &serde_json::to_string(&task).expect("serialize task"),
            )
            .expect("enqueue LLM task");

        let async_store = AsyncPendingMessageStore::from_store(store.clone());
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let client_runtime = SharedLlmClientRuntime::new(&ConfigHandle::current().llm);
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

        tokio::time::timeout(Duration::from_secs(180), completion)
            .await
            .expect("fallback worker should complete")
            .expect("worker completion observer should remain connected");

        worker.abort();
        let _ = worker.await;
        primary_server.abort();
        secondary_server.abort();
        let _ = primary_server.await;
        let _ = secondary_server.await;

        assert_eq!(
            primary_count.load(Ordering::SeqCst),
            1,
            "production worker should try the primary endpoint first"
        );
        assert_eq!(
            secondary_count.load(Ordering::SeqCst),
            1,
            "production worker should fall back to the secondary endpoint"
        );
        assert!(
            !drawer_is_deleted(&db, drawer_id),
            "keep verdict from fallback endpoint must retain the drawer"
        );
        assert!(
            store
                .claim_next_by_kind("after-fallback-worker", 1, LLM_TASK_KIND)
                .expect("claim after worker fallback")
                .is_none(),
            "completed fallback task must be confirmed"
        );
    });
}
