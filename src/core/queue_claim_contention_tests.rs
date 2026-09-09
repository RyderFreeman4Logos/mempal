use crate::core::writer_owner_diagnostics::WriterLockClass;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

#[test]
fn idle_claim_does_not_block_runtime_writer_lease_renewal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open database");
    let lease = db
        .runtime_writer_lease_acquire("sqlite-writer", "daemon", "daemon", 300, None)
        .expect("acquire runtime writer lease")
        .expect("runtime writer lease available");
    db.conn()
        .busy_timeout(Duration::ZERO)
        .expect("make lease renewal fail fast per attempt");

    let store = PendingMessageStore::new(&db_path).expect("open queue store");
    let update_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let paused_once = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let evidence = Arc::new(Mutex::new(None));
    let (paused_tx, paused_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);

    let authorizer_update = Arc::clone(&update_seen);
    let progress_update = Arc::clone(&update_seen);
    let progress_once = Arc::clone(&paused_once);
    let progress_evidence = Arc::clone(&evidence);
    let evidence_path = db_path.clone();
    store
        .with_claim_connection(|conn| {
            conn.authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Update {
                        table_name: "pending_messages",
                        ..
                    }
                ) {
                    authorizer_update.store(true, Ordering::Release);
                }
                Authorization::Allow
            }));
            conn.progress_handler(
                1,
                Some(move || {
                    if progress_update.load(Ordering::Acquire)
                        && !progress_once.swap(true, Ordering::AcqRel)
                    {
                        *progress_evidence.lock().expect("lock evidence") =
                            Some(writer_lock_evidence_for_path(&evidence_path));
                        paused_tx.send(()).expect("announce paused idle claim");
                        release_rx
                            .recv_timeout(Duration::from_secs(7))
                            .expect("release paused idle claim");
                    }
                    false
                }),
            );
            Ok(())
        })
        .expect("install deterministic SQLite hooks");

    let claim_store = store.clone();
    let claim = std::thread::spawn(move || claim_store.claim_next("hook-worker", 120));
    let paused = paused_rx.recv_timeout(Duration::from_secs(1)).is_ok();
    let renewal = db.runtime_writer_lease_renew(&lease, 300);
    if paused {
        release_tx.send(()).expect("release idle claim");
    }
    let claimed = claim.join().expect("join idle claim").expect("idle claim");
    assert!(claimed.is_none(), "isolated queue must remain idle");

    if paused {
        let captured = evidence
            .lock()
            .expect("lock captured evidence")
            .clone()
            .expect("idle claim owner evidence");
        assert_eq!(captured.class, WriterLockClass::HeldWriter);
        assert_eq!(captured.owner_pid, Some(std::process::id()));
        assert_eq!(captured.operation, Some("claim queued message"));
    }
    assert!(
        renewal.expect("runtime writer lease renewal must not hit idle claim contention"),
        "runtime writer lease must remain active"
    );
}

#[test]
fn by_id_idle_preflight_uses_primary_key_and_bounded_work() {
    const BACKLOG_ROWS: usize = 4_096;
    const MAX_VM_STEPS: u64 = 500;

    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open database");
    db.conn()
        .execute_batch(
            r#"
            WITH RECURSIVE backlog(i) AS (
                VALUES(1)
                UNION ALL
                SELECT i + 1 FROM backlog WHERE i < 4096
            )
            INSERT INTO pending_messages (
                id, kind, source_hash, status, payload, created_at, next_attempt_at
            )
            SELECT printf('backlog-%05d', i), 'hook_event', printf('hash-%05d', i),
                   'pending', '{}', 1, 1
            FROM backlog;
            "#,
        )
        .expect("seed eligible backlog");

    let mut statement = db
        .conn()
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {CLAIM_BY_ID_WORK_AVAILABLE_SQL}"
        ))
        .expect("prepare query plan");
    let plan = statement
        .query_map(
            params![0_i64, i64::MAX, "missing-target", "ingest_async"],
            |row| row.get::<_, String>(3),
        )
        .expect("query plan")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect query plan");
    drop(statement);

    let store = PendingMessageStore::new_without_reclaim(&db_path);
    let vm_steps = Arc::new(AtomicU64::new(0));
    let update_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let counted_steps = Arc::clone(&vm_steps);
    let counted_update = Arc::clone(&update_seen);
    store
        .with_claim_connection(|conn| {
            conn.progress_handler(
                1,
                Some(move || {
                    counted_steps.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            );
            conn.authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Update {
                        table_name: "pending_messages",
                        ..
                    }
                ) {
                    counted_update.store(true, Ordering::Release);
                }
                Authorization::Allow
            }));
            Ok(())
        })
        .expect("install SQLite work counters");

    let claimed = store
        .claim_by_id_and_kind("scoped-worker", 120, "missing-target", "ingest_async")
        .expect("targeted idle claim");
    let vm_steps = vm_steps.load(Ordering::Relaxed);
    eprintln!("by-ID eligibility: backlog={BACKLOG_ROWS}, vm_steps={vm_steps}, plan={plan:?}");
    assert!(claimed.is_none(), "missing target must remain unclaimed");
    assert!(
        !update_seen.load(Ordering::Acquire),
        "ineligible targeted claim must remain read-only"
    );
    assert!(
        plan.iter().any(|detail| {
            detail.contains("sqlite_autoindex_pending_messages_1") && detail.contains("id=?")
        }) && vm_steps < MAX_VM_STEPS,
        "by-ID eligibility must use the primary key and bounded work; backlog={BACKLOG_ROWS}, \
         vm_steps={vm_steps}, plan={plan:?}"
    );

    db.conn()
        .execute(
            r#"
            INSERT INTO pending_messages (
                id, kind, source_hash, status, payload, created_at, next_attempt_at, heartbeat_at
            ) VALUES ('stale-other-kind', 'hook_event', 'stale-hash', 'claimed', '{}', 1, 1, 0)
            "#,
            [],
        )
        .expect("seed stale global claim");
    assert!(
        store
            .claim_by_id_and_kind("scoped-worker", 120, "missing-target", "ingest_async")
            .expect("targeted claim with global stale work")
            .is_none(),
        "missing target must remain unclaimed after global maintenance"
    );
    let stale_status: String = db
        .conn()
        .query_row(
            "SELECT status FROM pending_messages WHERE id = 'stale-other-kind'",
            [],
            |row| row.get(0),
        )
        .expect("read globally reclaimed status");
    assert_eq!(
        stale_status, "pending",
        "targeted preflight must retain global stale reclaim"
    );
}
