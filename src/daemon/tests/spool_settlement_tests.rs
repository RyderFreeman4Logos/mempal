use super::*;

#[tokio::test]
async fn test_hook_spool_cleanup_waits_for_successful_confirm() {
    for (confirm_failures, should_exist) in [(1, true), (0, false)] {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let mempal_home = tmp.path().join(".mempal");
        let spool = mempal_home
            .join(crate::hook::HOOK_SPOOL_DIR)
            .join("settlement.json");
        std::fs::create_dir_all(spool.parent().expect("spool parent"))
            .expect("create spool parent");
        std::fs::write(
            &spool,
            r#"{"tool_name":"Bash","input":"printf settlement","output":"settled"}"#,
        )
        .expect("write spool payload");
        let db = Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(db.path()).expect("store");
        let async_store = AsyncPendingMessageStore::from_store(store.clone())
            .with_complete_lock_failures_for_test(confirm_failures);
        let envelope = test_envelope_with_payload_path(spool.display().to_string());
        store
            .enqueue(
                HookEvent::PostToolUse.queue_kind(),
                &serde_json::to_string(&envelope).expect("serialize envelope"),
            )
            .expect("enqueue hook envelope");
        let message = store
            .claim_next("spool-settlement-worker", 60)
            .expect("claim next")
            .expect("claimed message");

        super::super::process_hook_worker_message(
            HookWorkerState {
                async_db,
                db_path,
                store: async_store,
                worker_id: "spool-settlement-worker".to_string(),
                embedder: Arc::new(DaemonEmbedder::from_primary_for_test(Box::new(
                    StaticEmbedder,
                ))),
                prototype_classifier: Arc::new(ArcSwap::from_pointee(None)),
                llm_gate: None,
                config: Arc::new(Config::default()),
                mempal_home,
                write_observer: crate::daemon_bootstrap::DaemonWriteObserver::for_test(),
                runtime_writer_lease: None,
                idle_observer: None,
            },
            message,
            60,
        )
        .await;

        assert_eq!(
            spool.exists(),
            should_exist,
            "spool deletion must follow successful queue confirmation"
        );
    }
}
