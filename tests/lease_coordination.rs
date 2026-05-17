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
