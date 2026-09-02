use super::{Database, db_error_is_sqlite_lock, schema_repairs_required};
use rusqlite::Connection;
use std::time::Duration;

#[test]
fn repair_required_current_schema_open_fails_bounded_then_repairs_after_writer_release() {
    let _fixture_guard = super::db_open_busy_fixture_lock()
        .lock()
        .expect("serialize database busy fixtures");
    let tempdir = tempfile::TempDir::new_in("/tmp").expect("short tempdir");
    let db_path = tempdir.path().join("palace.db");
    drop(Database::open(&db_path).expect("initialize current database"));

    let blocker = Connection::open(&db_path).expect("open migration blocker");
    blocker
        .execute_batch("DROP INDEX idx_drawers_supersedes; BEGIN IMMEDIATE;")
        .expect("damage current schema and hold writer lock");
    assert!(
        schema_repairs_required(&blocker).expect("inspect repair-required fixture"),
        "fixture must require structural repair"
    );

    let (opened_tx, opened_rx) = std::sync::mpsc::channel();
    let (opener_started_tx, opener_started_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let open_path = db_path.clone();
    let opener = std::thread::spawn(move || {
        opener_started_tx
            .send(())
            .expect("report repair opener readiness");
        let _ = opened_tx.send(Database::open_with_busy_timeout(
            &open_path,
            Duration::from_millis(25),
        ));
    });
    opener_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("repair opener must reach the busy-window boundary");
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
