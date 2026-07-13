use mempal::core::db::Database;
use tempfile::tempdir;

fn open_test_db() -> Database {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.db");
    // Leak the tempdir so it lives for the test duration
    let path = path.to_path_buf();
    std::mem::forget(dir);
    Database::open(&path).unwrap()
}

#[test]
fn lease_acquire_and_release() {
    let db = open_test_db();

    // Acquire succeeds
    assert!(db.lease_acquire("wing/room", "agent-1", 300, None).unwrap());

    // Same holder re-acquire (renew) succeeds
    assert!(db.lease_acquire("wing/room", "agent-1", 300, None).unwrap());

    // Different holder cannot acquire
    assert!(!db.lease_acquire("wing/room", "agent-2", 300, None).unwrap());

    // Release by correct holder
    assert!(db.lease_release("wing/room", "agent-1").unwrap());

    // Release again fails (already released)
    assert!(!db.lease_release("wing/room", "agent-1").unwrap());

    // Now agent-2 can acquire
    assert!(
        db.lease_acquire("wing/room", "agent-2", 300, Some("consolidating"))
            .unwrap()
    );
}

#[test]
fn lease_renew() {
    let db = open_test_db();

    assert!(db.lease_acquire("res/a", "holder-x", 60, None).unwrap());

    // Renew by correct holder
    assert!(db.lease_renew("res/a", "holder-x", 600).unwrap());

    // Renew by wrong holder fails
    assert!(!db.lease_renew("res/a", "holder-y", 600).unwrap());
}

#[test]
fn lease_status() {
    let db = open_test_db();

    assert!(
        db.lease_acquire("res/1", "h1", 300, Some("task-a"))
            .unwrap()
    );
    assert!(db.lease_acquire("res/2", "h2", 300, None).unwrap());

    // Status for specific resource
    let leases = db.lease_status(Some("res/1")).unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].holder_id, "h1");
    assert_eq!(leases[0].metadata.as_deref(), Some("task-a"));
    assert!(leases[0].remaining_secs > 0);

    // Status for all
    let all = db.lease_status(None).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn lease_ttl_expiry() {
    let db = open_test_db();

    // Acquire with 1-second TTL
    assert!(db.lease_acquire("ephemeral", "agent", 1, None).unwrap());

    // Immediately visible
    let leases = db.lease_status(Some("ephemeral")).unwrap();
    assert_eq!(leases.len(), 1);

    // Wait for expiry
    std::thread::sleep(std::time::Duration::from_secs(2));

    // Expired — status returns empty (lazy cleanup)
    let leases = db.lease_status(Some("ephemeral")).unwrap();
    assert_eq!(leases.len(), 0);

    // Another agent can now acquire
    assert!(
        db.lease_acquire("ephemeral", "other-agent", 300, None)
            .unwrap()
    );
}

#[test]
fn lease_cleanup_expired() {
    let db = open_test_db();

    assert!(db.lease_acquire("a", "h", 1, None).unwrap());
    assert!(db.lease_acquire("b", "h", 1, None).unwrap());
    assert!(db.lease_acquire("c", "h", 300, None).unwrap());

    std::thread::sleep(std::time::Duration::from_secs(2));

    let cleaned = db.lease_cleanup_expired().unwrap();
    assert_eq!(cleaned, 2);

    // "c" still active
    let leases = db.lease_status(None).unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].resource_path, "c");
}

#[test]
fn runtime_writer_lease_acquire_renew_release() {
    let db = open_test_db();

    let lease = db
        .runtime_writer_lease_acquire(
            "sqlite-writer",
            "daemon-owner",
            "daemon",
            300,
            Some(r#"{"command":"daemon"}"#),
        )
        .unwrap()
        .expect("acquire writer lease");

    assert_eq!(lease.name, "sqlite-writer");
    assert_eq!(lease.owner, "daemon-owner");
    assert_eq!(lease.generation, 1);
    assert_eq!(lease.mode, "daemon");
    assert_eq!(lease.pid, std::process::id());
    assert!(lease.remaining_secs > 0);
    assert_eq!(
        lease.metadata_json.as_deref(),
        Some(r#"{"command":"daemon"}"#)
    );

    assert!(db.runtime_writer_lease_renew(&lease, 600).unwrap());
    let renewed = db
        .runtime_writer_lease_status(Some("sqlite-writer"))
        .unwrap();
    assert_eq!(renewed.len(), 1);
    assert!(renewed[0].remaining_secs > 0);

    assert!(db.runtime_writer_lease_release(&lease).unwrap());
    assert!(
        db.runtime_writer_lease_status(Some("sqlite-writer"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn runtime_writer_lease_conflicts_until_expiry() {
    let db = open_test_db();

    let daemon = db
        .runtime_writer_lease_acquire("sqlite-writer", "daemon-owner", "daemon", 1, None)
        .unwrap()
        .expect("daemon lease");

    assert!(
        db.runtime_writer_lease_acquire(
            "sqlite-writer",
            "maintenance-owner",
            "maintenance",
            300,
            None,
        )
        .unwrap()
        .is_none(),
        "maintenance writer must not acquire while daemon writer lease is active"
    );
    assert!(
        !db.runtime_writer_lease_release_fenced(
            "sqlite-writer",
            "maintenance-owner",
            &daemon.session_id,
            daemon.generation,
        )
        .unwrap(),
        "wrong owner/session must not release another writer lease"
    );

    std::thread::sleep(std::time::Duration::from_secs(2));

    let maintenance = db
        .runtime_writer_lease_acquire(
            "sqlite-writer",
            "maintenance-owner",
            "maintenance",
            300,
            None,
        )
        .unwrap()
        .expect("expired daemon lease should be recoverable");
    assert_eq!(maintenance.mode, "maintenance");
    assert!(maintenance.generation > daemon.generation);
}

#[test]
fn runtime_writer_lease_generation_fences_stale_owner_after_takeover() {
    let db = open_test_db();
    let first = db
        .runtime_writer_lease_acquire("sqlite-writer", "owner", "maintenance", 1, None)
        .unwrap()
        .expect("first lease");

    std::thread::sleep(std::time::Duration::from_secs(2));
    let second = db
        .runtime_writer_lease_acquire("sqlite-writer", "owner", "maintenance", 300, None)
        .unwrap()
        .expect("take over expired lease");

    assert!(second.generation > first.generation);
    assert!(!db.runtime_writer_lease_renew(&first, 300).unwrap());
    assert!(!db.runtime_writer_lease_release(&first).unwrap());
    assert!(
        !db.runtime_writer_lease_restore_if_unheld(&first, 300)
            .unwrap()
    );
    assert!(db.runtime_writer_lease_is_active(&second).unwrap());
    assert_eq!(
        db.runtime_writer_lease_status(Some("sqlite-writer"))
            .unwrap(),
        vec![second]
    );
}

#[test]
fn runtime_writer_lease_daemon_start_rebinds_previous_daemon_identity() {
    let db = open_test_db();

    let previous = db
        .runtime_writer_lease_acquire_for_daemon_start("sqlite-writer", 300, None)
        .unwrap()
        .expect("previous daemon lease");
    assert!(
        db.runtime_writer_lease_acquire_for_daemon_start("sqlite-writer", 300, None)
            .unwrap()
            .is_none(),
        "daemon start must not replace a live previous daemon identity"
    );
    let reused_pid_owner = format!("mempal-daemon-{}-previous-start", std::process::id());
    db.conn()
        .execute(
            "UPDATE runtime_writer_leases SET owner = ?2 WHERE name = ?1",
            rusqlite::params![previous.name, reused_pid_owner],
        )
        .expect("simulate PID reuse by a later daemon process");

    let rebound = db
        .runtime_writer_lease_acquire_for_daemon_start("sqlite-writer", 300, None)
        .unwrap()
        .expect("new daemon must replace the previous start identity for the reused PID");

    assert_eq!(rebound.owner, previous.owner);
    assert_eq!(rebound.pid, std::process::id());
    assert_ne!(rebound.session_id, previous.session_id);
    let active = db
        .runtime_writer_lease_status(Some("sqlite-writer"))
        .unwrap();
    assert_eq!(active, vec![rebound]);
}

#[test]
fn runtime_writer_daemon_liveness_rejects_reused_pid_identity() {
    let db = open_test_db();
    let lease = db
        .runtime_writer_lease_acquire_for_daemon_start("sqlite-writer", 300, None)
        .unwrap()
        .expect("daemon lease");
    let reused_pid_owner = format!("mempal-daemon-{}-previous-start", std::process::id());
    db.conn()
        .execute(
            "UPDATE runtime_writer_leases SET owner = ?2 WHERE name = ?1",
            rusqlite::params![lease.name, reused_pid_owner],
        )
        .expect("simulate PID reuse by another mempal process");

    assert!(
        !db.runtime_writer_lease_has_live_daemon("sqlite-writer")
            .unwrap(),
        "reused PID must not make a stale daemon lease live"
    );
    assert!(
        !db.runtime_writer_lease_has_live_durable_ingest_worker("sqlite-writer")
            .unwrap(),
        "reused PID must not route durable ingest work to a stale daemon"
    );

    db.conn()
        .execute(
            "UPDATE runtime_writer_leases SET expires_at = '1970-01-01T00:00:00.000Z' WHERE name = ?1",
            [&lease.name],
        )
        .expect("expire stale daemon lease");
    assert!(
        db.runtime_writer_lease_status(Some("sqlite-writer"))
            .unwrap()
            .is_empty(),
        "expired stale daemon lease must not be preserved"
    );
}

#[test]
fn runtime_writer_lease_different_names_do_not_collide() {
    let db = open_test_db();

    let daemon = db
        .runtime_writer_lease_acquire("sqlite-writer", "daemon-owner", "daemon", 300, None)
        .unwrap()
        .expect("daemon lease");
    let independent = db
        .runtime_writer_lease_acquire("analytics-writer", "other-owner", "maintenance", 300, None)
        .unwrap()
        .expect("independent lease");

    assert_ne!(daemon.name, independent.name);
    assert_eq!(db.runtime_writer_lease_status(None).unwrap().len(), 2);
}
