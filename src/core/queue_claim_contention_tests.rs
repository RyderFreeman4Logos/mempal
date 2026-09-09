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
