use super::{Database, db_error_is_sqlite_lock, schema_repairs_required};
use rusqlite::Connection;
use std::time::Duration;

#[test]
fn repair_required_current_schema_open_fails_bounded_then_repairs_after_writer_release() {
    let _fixture_guard = super::db_open_busy_fixture_lock().blocking_lock();
    let tempdir = tempfile::TempDir::new().expect("short tempdir");
    let db_path = tempdir.path().join("palace.db");
    drop(Database::open(&db_path).expect("initialize current database"));

    let blocker = Connection::open(&db_path).expect("open migration blocker");
    blocker
        .execute_batch("DROP INDEX idx_drawers_supersedes;")
        .expect("damage current schema");
    assert!(
        schema_repairs_required(&blocker).expect("inspect repair-required fixture"),
        "fixture must require structural repair"
    );

    let (opened_tx, opened_rx) = std::sync::mpsc::channel();
    let (repair_begin_rx, repair_resume_tx) =
        super::install_schema_repair_begin_test_hook(&db_path);
    let open_path = db_path.clone();
    let opener = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        let _ = opened_tx.send(Database::open_with_busy_timeout(
            &open_path,
            Duration::from_millis(25),
        ));
    });
    repair_begin_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("repair opener must reach the busy-window boundary");
    blocker
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("hold writer lock at the repair boundary");
    repair_resume_tx
        .send(())
        .expect("release repair opener into the busy window");
    let opened = opened_rx.recv_timeout(Duration::from_millis(250));
    blocker
        .execute_batch("ROLLBACK;")
        .expect("release migration blocker");
    opener.join().expect("join blocked database opener");
    let error = match opened.expect("busy timeout must bound repair-required open") {
        Ok(_) => panic!("repair-required open must not bypass the live writer"),
        Err(error) => error,
    };
    assert!(
        db_error_is_sqlite_lock(&error),
        "repair-required open returned the wrong error: {error}"
    );
    let repaired = Database::open(&db_path).expect("repair after writer release");
    assert!(
        !schema_repairs_required(repaired.conn()).expect("validate repair after release"),
        "released fixture was not repaired"
    );
}
