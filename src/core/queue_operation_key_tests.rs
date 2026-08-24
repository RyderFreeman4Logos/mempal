    #[test]
    fn pending_message_ids_include_process_component() {
        let id = next_id("msg");
        let pid_component = format!("{:08x}", std::process::id());

        assert!(
            id.contains(&pid_component),
            "pending message id {id} should include process component {pid_component}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_claim_confirm_run_off_runtime() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");
        let sync_store = PendingMessageStore::new(&db_path).expect("open queue");
        sync_store
            .enqueue("hook_user_prompt", "{\"event\":\"UserPromptSubmit\"}")
            .expect("enqueue");
        let store = AsyncPendingMessageStore::from_store(sync_store)
            .with_blocking_delay(Duration::from_millis(300));

        let ticks = Arc::new(AtomicU64::new(0));
        let ticks_bg = Arc::clone(&ticks);
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                ticks_bg.fetch_add(1, Ordering::SeqCst);
            }
        });

        let claimed = store
            .claim_next("worker-a".to_string(), 60)
            .await
            .expect("claim")
            .expect("claimed message");
        store.confirm(claimed).await.expect("confirm off runtime");
        ticker.abort();

        let observed = ticks.load(Ordering::SeqCst);
        assert!(
            observed >= 5,
            "ticker advanced {observed} times while delayed claim/confirm ran; \
             queue SQLite must run off the Tokio worker"
        );
    }

    #[test]
    fn claim_next_skips_ingest_async_rows_for_dedicated_workers() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");
        let store = PendingMessageStore::new(&db_path).expect("open queue");
        let ingest_id = store
            .enqueue("ingest_async", r#"{"request":{}}"#)
            .expect("enqueue async ingest");
        let hook_id = store
            .enqueue("hook_user_prompt", r#"{"event":"UserPromptSubmit"}"#)
            .expect("enqueue hook");

        let claimed = store
            .claim_next("hook-worker", 60)
            .expect("claim next")
            .expect("hook row should be claimed");

        assert_eq!(claimed.id, hook_id);
        assert_eq!(claimed.kind, "hook_user_prompt");
        let ingest_status = store
            .operation_status(&ingest_id)
            .expect("load async ingest status")
            .expect("async ingest row remains visible");
        assert_eq!(ingest_status.op_state, "queued");
        assert!(ingest_status.claimed_at.is_none());
    }

    #[test]
    fn claim_next_reuses_persistent_claim_connection_across_polls() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");
        let store = PendingMessageStore::new(&db_path).expect("open queue");
        let cloned_store = store.clone();

        assert_eq!(store.claim_connection_open_count(), 0);
        assert!(
            cloned_store
                .claim_next("hook-worker", 60)
                .expect("first idle claim")
                .is_none()
        );
        assert_eq!(store.claim_connection_open_count(), 1);

        let hook_id = store
            .enqueue("hook_user_prompt", r#"{"event":"UserPromptSubmit"}"#)
            .expect("enqueue hook");
        let claimed = cloned_store
            .claim_next("hook-worker", 60)
            .expect("claim hook")
            .expect("hook row should be claimed");

        assert_eq!(claimed.id, hook_id);
        assert_eq!(store.claim_connection_open_count(), 1);
        assert!(
            cloned_store
                .claim_next("hook-worker", 60)
                .expect("second idle claim")
                .is_none()
        );
        assert_eq!(store.claim_connection_open_count(), 1);
    }

    #[test]
    fn claim_next_by_kind_reuses_persistent_claim_connection_across_idle_polls() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");
        let store = PendingMessageStore::new(&db_path).expect("open queue");
        let cloned_store = store.clone();

        assert_eq!(store.claim_connection_open_count(), 0);
        for _ in 0..3 {
            assert!(
                cloned_store
                    .claim_next_by_kind("ingest-worker", 60, "ingest_async")
                    .expect("idle ingest claim")
                    .is_none()
            );
            assert_eq!(store.claim_connection_open_count(), 1);
        }

        let ingest_id = store
            .enqueue("ingest_async", r#"{"request":{}}"#)
            .expect("enqueue async ingest");
        let claimed = cloned_store
            .claim_next_by_kind("ingest-worker", 60, "ingest_async")
            .expect("claim ingest")
            .expect("ingest row should be claimed");

        assert_eq!(claimed.id, ingest_id);
        assert_eq!(store.claim_connection_open_count(), 1);
    }

    #[test]
    fn confirm_and_enqueue_reuse_cached_writer_connection() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");
        let store = PendingMessageStore::new_without_reclaim(&db_path);

        assert_eq!(store.writer_open_count(), 0);
        let id = store
            .enqueue("hook_user_prompt", r#"{"event":"UserPromptSubmit"}"#)
            .expect("enqueue hook");
        assert_eq!(store.writer_open_count(), 1);

        let claim = store
            .claim_next("hook-worker", 60)
            .expect("claim hook")
            .expect("hook row should be claimed");
        assert_eq!(claim.id, id);
        store.confirm(&claim).expect("confirm hook");
        assert_eq!(store.writer_open_count(), 1);
    }

    #[test]
    fn status_and_stats_read_through_writer_lock() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");
        let store = PendingMessageStore::new(&db_path).expect("open queue");
        let ingest_id = store
            .enqueue("ingest_async", r#"{"request":{}}"#)
            .expect("enqueue async ingest");

        let lock_holder = Connection::open(&db_path).expect("open lock holder");
        lock_holder
            .busy_timeout(Duration::from_millis(25))
            .expect("set lock holder busy timeout");
        lock_holder
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("hold write lock");

        let status = store
            .operation_status(&ingest_id)
            .expect("operation status should not need startup writes")
            .expect("operation remains visible");
        let stats = store.stats().expect("stats should not need startup writes");

        assert_eq!(status.op_state, "queued");
        assert_eq!(stats.pending, 1);
    }

    #[test]
    fn queue_busy_timeout_covers_smoke_read_write_contention() {
        assert!(
            DEFAULT_BUSY_TIMEOUT >= Duration::from_secs(30),
            "async ingest queue writes must outwait transient full-smoke read/write contention"
        );
    }

    #[test]
    fn claim_next_uses_bounded_sqlite_lock_budget() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");
        let store = PendingMessageStore::new(&db_path).expect("open queue");
        store
            .enqueue("hook", r#"{"request":{}}"#)
            .expect("enqueue async ingest");

        let lock_holder = Connection::open(&db_path).expect("open lock holder");
        lock_holder
            .busy_timeout(Duration::ZERO)
            .expect("set fail-fast lock holder timeout");
        lock_holder
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("hold write lock");

        let started = std::time::Instant::now();
        let error = store
            .claim_next("worker-a", 60)
            .expect_err("claim should exhaust the bounded lock budget");
        let elapsed = started.elapsed();

        lock_holder
            .execute_batch("ROLLBACK;")
            .expect("release write lock");

        assert!(error.is_sqlite_lock());
        assert!(
            elapsed < Duration::from_secs(10),
            "claim lock budget must not monopolize queue blocking workers for the 30s default busy timeout"
        );
    }
