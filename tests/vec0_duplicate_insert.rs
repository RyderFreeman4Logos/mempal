use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use tempfile::TempDir;

fn make_drawer(id: &str) -> Drawer {
    Drawer {
        id: id.to_string(),
        content: "test content".to_string(),
        wing: "test".to_string(),
        room: Some("default".to_string()),
        source_file: Some("test.md".to_string()),
        source_type: SourceType::Manual,
        added_at: "2026-04-23T00:00:00Z".to_string(),
        chunk_index: Some(0),
        importance: 0,
        ..Drawer::default()
    }
}

#[test]
fn insert_vector_duplicate_pk_is_idempotent() {
    // sqlite-vec's vec0 virtual table does not honor `INSERT OR IGNORE` —
    // duplicate-key inserts raise SQLITE_CONSTRAINT_PRIMARYKEY (extended
    // code 1555) unconditionally. The Database layer must swallow that
    // specific error so that bulk ingest of content that produces identical
    // chunk hashes across multiple source files does not abort the whole
    // batch.
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    let drawer = make_drawer("drawer-dup-test");
    db.insert_drawer(&drawer).expect("first drawer insert");
    db.insert_vector(&drawer.id, &[0.1_f32, 0.2, 0.3, 0.4])
        .expect("first vector insert");

    db.insert_vector(&drawer.id, &[0.5_f32, 0.6, 0.7, 0.8])
        .expect("duplicate vector insert should be Ok (swallowed), not error");
}

#[test]
fn insert_vector_distinct_ids_coexist() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    let a = make_drawer("drawer-a");
    let b = make_drawer("drawer-b");
    db.insert_drawer(&a).expect("insert a");
    db.insert_drawer(&b).expect("insert b");
    db.insert_vector(&a.id, &[0.1_f32, 0.2, 0.3, 0.4])
        .expect("insert a vector");
    db.insert_vector(&b.id, &[0.5_f32, 0.6, 0.7, 0.8])
        .expect("insert b vector");

    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| row.get(0))
        .expect("count vectors");
    assert_eq!(count, 2, "both distinct-id vectors should persist");
}
