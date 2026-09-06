use super::*;

use std::sync::{Mutex, mpsc};
use std::time::Duration;

static RENEW_BUSY_SIGNAL: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);

fn signal_renew_busy(_: i32) -> bool {
    if let Some(sender) = RENEW_BUSY_SIGNAL
        .lock()
        .expect("lock renewal busy signal")
        .take()
    {
        sender.send(()).expect("send renewal busy signal");
    }
    false
}

#[test]
fn takeover_after_preflight_rejects_stale_generation_mutation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("palace.db");
    let db = Database::open(&db_path).expect("database");
    db.conn()
        .execute_batch("CREATE TABLE fence_probe (value TEXT NOT NULL)")
        .expect("create fence probe");
    let stale = db
        .runtime_writer_lease_acquire("sqlite-writer", "old", "daemon", 300, None)
        .expect("acquire old lease")
        .expect("old lease available");
    assert!(
        db.runtime_writer_lease_is_active(&stale)
            .expect("preflight old generation")
    );

    assert!(
        db.runtime_writer_lease_release(&stale)
            .expect("release old generation")
    );
    let current = db
        .runtime_writer_lease_acquire("sqlite-writer", "new", "daemon", 300, None)
        .expect("acquire replacement lease")
        .expect("replacement lease available");
    assert!(current.generation > stale.generation);

    let error = db
        .with_runtime_writer_lease_write(Some(&stale), "insert fenced drawer", || {
            db.conn()
                .execute("INSERT INTO fence_probe (value) VALUES ('stale')", [])
                .map(|_| ())
                .map_err(DbError::from)
        })
        .expect_err("stale generation must not mutate after takeover");
    assert!(matches!(
        error,
        DbError::RuntimeWriterLeaseLost { generation, .. } if generation == stale.generation
    ));
    let count = || {
        db.conn()
            .query_row("SELECT COUNT(*) FROM fence_probe", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count fence probe")
    };
    assert_eq!(count(), 0);

    db.with_runtime_writer_lease_write(Some(&current), "insert fenced drawer", || {
        db.conn()
            .execute("INSERT INTO fence_probe (value) VALUES ('current')", [])
            .map(|_| ())
            .map_err(DbError::from)
    })
    .expect("current generation may mutate");
    assert_eq!(count(), 1);
}

#[test]
fn writer_lease_renew_retries_sqlite_busy_until_live_holder_releases() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("palace.db");
    let db = Database::open(&db_path).expect("database");
    // Lease-control opens use SQLITE_OPEN_NOFOLLOW. tempfile under a symlink
    // TMPDIR is lexical; admitted Database::path() is the canonical identity.
    let db_path = db.path().to_path_buf();
    let lease = db
        .runtime_writer_lease_acquire("sqlite-writer", "daemon", "daemon", 120, None)
        .expect("acquire daemon lease")
        .expect("daemon lease available");

    let renew_db = Database::open_lease_control_with_timeout(&db_path, Duration::ZERO)
        .expect("open renewal database");
    let (busy_tx, busy_rx) = mpsc::channel();
    *RENEW_BUSY_SIGNAL.lock().expect("lock renewal busy signal") = Some(busy_tx);
    renew_db
        .conn()
        .busy_handler(Some(signal_renew_busy))
        .expect("install renewal busy witness");

    let (holder_ready_tx, holder_ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let holder_path = db_path.clone();
    let holder = std::thread::spawn(move || {
        let holder = rusqlite::Connection::open(holder_path).expect("open SQLite lock holder");
        holder
            .busy_timeout(Duration::ZERO)
            .expect("make lock holder fail fast");
        holder
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("hold SQLite writer lock");
        holder_ready_tx
            .send(())
            .expect("signal SQLite writer lock ready");
        release_rx
            .recv()
            .expect("wait for SQLite writer lock release");
        holder
            .execute_batch("ROLLBACK;")
            .expect("release SQLite writer lock");
    });
    holder_ready_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("SQLite writer lock holder must be ready");

    let renewal = std::thread::spawn(move || renew_db.runtime_writer_lease_renew(&lease, 120));
    let renewal_observed_busy = busy_rx.recv_timeout(Duration::from_secs(1)).is_ok();
    let renewal_still_running = !renewal.is_finished();

    let witness = rusqlite::Connection::open(&db_path).expect("open SQLite busy witness");
    witness
        .busy_timeout(Duration::ZERO)
        .expect("make SQLite busy witness fail fast");
    let busy_result = witness.execute_batch("BEGIN IMMEDIATE;");

    release_tx
        .send(())
        .expect("signal SQLite writer lock release");
    holder.join().expect("join SQLite lock holder");
    let renewal_result = renewal.join().expect("join renewal");

    assert!(
        renewal_observed_busy,
        "renewal must observe SQLite busy while the holder is still held"
    );
    assert!(
        renewal_still_running,
        "renewal must not return before the competing writer lock is released"
    );
    let busy_error = busy_result.expect_err("the competing writer lock must still be held");
    let rusqlite::Error::SqliteFailure(error, message) = busy_error else {
        panic!("expected SQLite DatabaseBusy, got {busy_error:?}");
    };
    assert_eq!(error.code, rusqlite::ErrorCode::DatabaseBusy);
    assert_eq!(error.extended_code, 5);
    assert_eq!(message.as_deref(), Some("database is locked"));
    let renewed = renewal_result.unwrap_or_else(|error| {
        panic!("live daemon lease must remain renewable after contention: {error:?}")
    });
    assert!(
        renewed,
        "live daemon lease must remain renewable after contention"
    );
}
