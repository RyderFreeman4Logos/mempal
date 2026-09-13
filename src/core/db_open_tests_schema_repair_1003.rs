use super::{Database, db_error_is_sqlite_lock, schema_repairs_required};
use rusqlite::Connection;
use std::time::Duration;

struct WriterLockHolder {
    release: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WriterLockHolder {
    fn acquire(path: std::path::PathBuf) -> Self {
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result = (|| {
                let conn = Connection::open(path)?;
                conn.execute_batch("BEGIN IMMEDIATE;")?;
                ready_tx.send(Ok::<_, rusqlite::Error>(())).ok();
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
                conn.execute_batch("ROLLBACK;")
            })();
            if let Err(error) = result {
                let _ = ready_tx.send(Err(error));
            }
        });
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writer holder must publish readiness")
            .expect("writer holder must acquire BEGIN IMMEDIATE");
        Self {
            release: Some(release_tx),
            thread: Some(thread),
        }
    }

    fn release(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().expect("join writer holder");
        }
    }
}

impl Drop for WriterLockHolder {
    fn drop(&mut self) {
        self.finish();
    }
}

#[test]
fn repair_required_open_installs_busy_timeout_before_sqlite_lock_boundary() {
    let _fixture_guard = super::db_open_busy_fixture_lock().blocking_lock();
    let tempdir = tempfile::TempDir::new().expect("short tempdir");
    let db_path = Database::open(&tempdir.path().join("palace.db"))
        .expect("initialize current database")
        .path()
        .to_path_buf();

    let fixture = Connection::open(&db_path).expect("open schema fixture");
    fixture
        .execute_batch("DROP INDEX idx_drawers_supersedes;")
        .expect("damage current schema");
    assert!(
        schema_repairs_required(&fixture).expect("inspect repair-required fixture"),
        "fixture must require structural repair"
    );
    drop(fixture);

    let holder = WriterLockHolder::acquire(db_path.clone());
    let observed_timeout = super::install_schema_repair_begin_test_hook(&db_path);
    let opened = Database::open_with_busy_timeout(&db_path, Duration::from_millis(25));
    assert_eq!(
        observed_timeout
            .recv_timeout(Duration::from_secs(5))
            .expect("observe timeout immediately before structural repair"),
        25,
        "repair must inherit the caller-selected SQLite busy timeout"
    );
    let error = match opened {
        Ok(_) => panic!("repair-required open must not bypass the live writer"),
        Err(error) => error,
    };
    assert!(
        db_error_is_sqlite_lock(&error),
        "repair-required open returned the wrong error: {error}"
    );

    holder.release();
    let repaired = Database::open(&db_path).expect("repair after writer release");
    assert!(
        !schema_repairs_required(repaired.conn()).expect("validate repair after release"),
        "released fixture was not repaired"
    );
}
