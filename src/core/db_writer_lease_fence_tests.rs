use super::*;

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
