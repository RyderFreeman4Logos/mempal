use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use rusqlite::params;
use tempfile::TempDir;

fn make_drawer(id: &str, content: &str, wing: &str, room: Option<&str>) -> Drawer {
    Drawer {
        id: id.to_string(),
        content: content.to_string(),
        wing: wing.to_string(),
        room: room.map(|s| s.to_string()),
        source_file: Some("test.md".to_string()),
        source_type: SourceType::Manual,
        added_at: "2026-04-25T00:00:00Z".to_string(),
        chunk_index: Some(0),
        importance: 0,
        ..Drawer::default()
    }
}

fn fresh_db() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    (tmp, db)
}

fn user_version(db: &Database) -> u32 {
    db.conn()
        .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
        .expect("read user_version")
}

#[test]
fn v5_schema_adds_content_hash_column_and_index() {
    let (_tmp, db) = fresh_db();
    assert!(user_version(&db) >= 5);

    let has_column: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('drawers') WHERE name = 'content_hash'",
            [],
            |row| row.get(0),
        )
        .expect("pragma table_info");
    assert_eq!(has_column, 1, "content_hash column must exist after v5");

    let has_index: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM pragma_index_list('drawers') WHERE name = 'idx_drawers_content_hash'",
            [],
            |row| row.get(0),
        )
        .expect("pragma index_list");
    assert_eq!(has_index, 1, "idx_drawers_content_hash must exist after v5");
}

#[test]
fn insert_drawer_populates_content_hash() {
    let (_tmp, db) = fresh_db();
    let drawer = make_drawer("d1", "hello world", "test", Some("default"));
    db.insert_drawer(&drawer).expect("insert drawer");

    let hash: String = db
        .conn()
        .query_row(
            "SELECT content_hash FROM drawers WHERE id = ?1",
            params!["d1"],
            |row| row.get(0),
        )
        .expect("read content_hash");

    let expected = blake3::hash(b"hello world").to_hex().to_string();
    assert_eq!(hash, expected);
}

#[test]
fn dedup_resolves_existing_drawer_via_content_hash() {
    let (_tmp, db) = fresh_db();
    let drawer = make_drawer("d1", "exact same body", "test", Some("default"));
    db.insert_drawer(&drawer).expect("insert");

    let (resolved, exists) = db
        .resolve_ingest_drawer_id("test", Some("default"), "exact same body", None)
        .expect("resolve");
    assert!(exists);
    assert_eq!(resolved, "d1");
}

#[test]
fn dedup_isolates_per_project() {
    let (_tmp, db) = fresh_db();
    let drawer = make_drawer("d_p1", "shared body", "test", Some("default"));
    db.insert_drawer_with_project(&drawer, Some("project-1"))
        .expect("insert in project-1");

    let (_, exists_other_project) = db
        .resolve_ingest_drawer_id("test", Some("default"), "shared body", Some("project-2"))
        .expect("resolve in project-2");
    assert!(
        !exists_other_project,
        "same content in a different project must not match"
    );

    let (resolved_same, exists_same) = db
        .resolve_ingest_drawer_id("test", Some("default"), "shared body", Some("project-1"))
        .expect("resolve in project-1");
    assert!(exists_same);
    assert_eq!(resolved_same, "d_p1");
}

#[test]
fn dedup_query_uses_content_hash_index() {
    let (_tmp, db) = fresh_db();
    let drawer = make_drawer("d1", "indexed body", "test", Some("default"));
    db.insert_drawer(&drawer).expect("insert");

    let blake = blake3::hash(b"indexed body").to_hex().to_string();
    let plan: Vec<String> = db
        .conn()
        .prepare(
            "EXPLAIN QUERY PLAN SELECT id FROM drawers \
             WHERE deleted_at IS NULL AND wing = ?1 AND content_hash = ?2 \
             AND ((room IS NULL AND ?3 IS NULL) OR room = ?3) \
             AND ((project_id IS NULL AND ?4 IS NULL) OR project_id = ?4) \
             ORDER BY id LIMIT 1",
        )
        .expect("prepare explain")
        .query_map(
            params!["test", blake, Some("default"), None::<&str>],
            |row| row.get::<_, String>(3),
        )
        .expect("query explain")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect explain");

    let plan_text = plan.join("\n");
    assert!(
        plan_text.contains("idx_drawers_content_hash"),
        "dedup query must use the content_hash index, plan was:\n{plan_text}"
    );
}

#[test]
fn v5_migration_backfills_existing_drawers_on_reopen() {
    let tmp = TempDir::new().expect("tempdir");
    let path = tmp.path().join("palace.db");

    {
        let db = Database::open(&path).expect("open db v5");
        db.insert_drawer(&make_drawer("a", "alpha", "test", None))
            .expect("insert a");
        db.insert_drawer(&make_drawer("b", "beta", "test", None))
            .expect("insert b");
        // Simulate an install that was created at v4: drop the content_hash
        // values and rewind user_version. The next Database::open() should
        // re-run the v5 migration (idempotent column add) and the backfill.
        db.conn()
            .execute("UPDATE drawers SET content_hash = NULL", [])
            .expect("null out hashes");
        db.conn()
            .execute_batch("PRAGMA user_version = 4")
            .expect("rewind user_version");
    }

    let db2 = Database::open(&path).expect("reopen db");
    assert!(user_version(&db2) >= 5);

    let rows: Vec<(String, Option<String>)> = db2
        .conn()
        .prepare("SELECT id, content_hash FROM drawers ORDER BY id")
        .expect("prepare")
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect");

    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].1.as_deref(),
        Some(blake3::hash(b"alpha").to_hex().to_string().as_str())
    );
    assert_eq!(
        rows[1].1.as_deref(),
        Some(blake3::hash(b"beta").to_hex().to_string().as_str())
    );
}
