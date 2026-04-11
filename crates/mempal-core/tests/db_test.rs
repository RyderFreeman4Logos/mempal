use mempal_core::db::Database;
use mempal_core::types::{Drawer, SourceType};
use rusqlite::Connection;
use rusqlite::Row;
use tempfile::tempdir;

#[test]
fn test_db_init() {
    let dir = tempdir().expect("temp dir should be created");
    let path = dir.path().join("test.db");
    let db = Database::open(&path).expect("database should open");

    assert!(path.exists());

    let tables: Vec<String> = db
        .conn()
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .expect("table query should prepare")
        .query_map([], |row: &Row<'_>| row.get::<_, String>(0))
        .expect("table query should run")
        .collect::<Result<Vec<_>, _>>()
        .expect("table rows should collect");

    assert!(tables.contains(&"drawers".to_string()));
    // drawer_vectors is lazy-created on first insert_vector() call,
    // so it does not exist on a fresh database.
    // assert!(tables.contains(&"drawer_vectors".to_string()));
    assert!(tables.contains(&"triples".to_string()));
    assert!(tables.contains(&"taxonomy".to_string()));

    let schema_version: u32 = db.schema_version().expect("schema version should load");
    assert_eq!(schema_version, 4);

    let indexes: Vec<String> = db
        .conn()
        .prepare("SELECT name FROM sqlite_master WHERE type='index' ORDER BY name")
        .expect("index query should prepare")
        .query_map([], |row: &Row<'_>| row.get::<_, String>(0))
        .expect("index query should run")
        .collect::<Result<Vec<_>, _>>()
        .expect("index rows should collect");

    assert!(indexes.contains(&"idx_drawers_wing".to_string()));
    assert!(indexes.contains(&"idx_drawers_wing_room".to_string()));
}

#[test]
fn test_db_idempotent() {
    let dir = tempdir().expect("temp dir should be created");
    let path = dir.path().join("test.db");
    let db = Database::open(&path).expect("database should open");

    db.insert_drawer(&Drawer {
        id: "test1".into(),
        content: "hello".into(),
        wing: "w".into(),
        room: None,
        source_file: None,
        source_type: SourceType::Manual,
        added_at: "2026-04-08".into(),
        chunk_index: None,
        importance: 0,
    })
    .expect("drawer insert should succeed");

    drop(db);

    let reopened = Database::open(&path).expect("database should reopen");
    let count = reopened.drawer_count().expect("count query should succeed");

    assert_eq!(count, 1);
    assert_eq!(
        reopened
            .schema_version()
            .expect("schema version should load after reopen"),
        4
    );
}

#[test]
fn test_db_migrates_legacy_schema_without_user_version() {
    let dir = tempdir().expect("temp dir should be created");
    let path = dir.path().join("legacy.db");
    let conn = Connection::open(&path).expect("legacy db should open");
    conn.execute_batch(
        r#"
        CREATE TABLE drawers (
            id TEXT PRIMARY KEY,
            content TEXT NOT NULL,
            wing TEXT NOT NULL,
            room TEXT,
            source_file TEXT,
            source_type TEXT NOT NULL,
            added_at TEXT NOT NULL,
            chunk_index INTEGER
        );
        INSERT INTO drawers (id, content, wing, room, source_file, source_type, added_at, chunk_index)
        VALUES ('legacy', 'hello', 'myapp', NULL, 'README.md', 'project', '2026-04-10', 0);
        "#,
    )
    .expect("legacy schema should initialize");
    drop(conn);

    let db = Database::open(&path).expect("database should migrate legacy schema");

    assert_eq!(
        db.schema_version()
            .expect("schema version should be upgraded"),
        4
    );

    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drawers", [], |row: &Row<'_>| {
            row.get::<_, i64>(0)
        })
        .expect("drawer count query should succeed");
    assert_eq!(count, 1);
}

fn make_drawer(id: &str, wing: &str) -> Drawer {
    Drawer {
        id: id.into(),
        content: format!("content of {id}"),
        wing: wing.into(),
        room: None,
        source_file: Some("test.md".into()),
        source_type: SourceType::Manual,
        added_at: "2026-04-10".into(),
        chunk_index: None,
        importance: 0,
    }
}

#[test]
fn test_soft_delete_drawer() {
    let dir = tempdir().expect("temp dir");
    let db = Database::open(&dir.path().join("test.db")).expect("db open");

    db.insert_drawer(&make_drawer("d1", "w")).expect("insert");
    db.insert_drawer(&make_drawer("d2", "w")).expect("insert");

    assert_eq!(db.drawer_count().expect("count"), 2);

    // Soft-delete d1
    let deleted = db.soft_delete_drawer("d1").expect("soft delete");
    assert!(deleted);

    // d1 no longer visible in count or exists
    assert_eq!(db.drawer_count().expect("count"), 1);
    assert!(!db.drawer_exists("d1").expect("exists"));
    assert!(db.drawer_exists("d2").expect("exists"));

    // get_drawer returns None for deleted
    assert!(db.get_drawer("d1").expect("get").is_none());
    assert!(db.get_drawer("d2").expect("get").is_some());

    // Double delete returns false
    let deleted_again = db.soft_delete_drawer("d1").expect("soft delete again");
    assert!(!deleted_again);

    // deleted_drawer_count
    assert_eq!(db.deleted_drawer_count().expect("deleted count"), 1);
}

#[test]
fn test_purge_deleted() {
    let dir = tempdir().expect("temp dir");
    let db = Database::open(&dir.path().join("test.db")).expect("db open");

    db.insert_drawer(&make_drawer("d1", "w")).expect("insert");
    db.insert_drawer(&make_drawer("d2", "w")).expect("insert");
    db.insert_drawer(&make_drawer("d3", "w")).expect("insert");

    db.soft_delete_drawer("d1").expect("delete d1");
    db.soft_delete_drawer("d2").expect("delete d2");

    // Purge all deleted
    let purged = db.purge_deleted(None).expect("purge");
    assert_eq!(purged, 2);
    assert_eq!(db.deleted_drawer_count().expect("deleted count"), 0);

    // d3 still exists
    assert_eq!(db.drawer_count().expect("count"), 1);
    assert!(db.get_drawer("d3").expect("get").is_some());
}

#[test]
fn test_top_drawers_excludes_deleted() {
    let dir = tempdir().expect("temp dir");
    let db = Database::open(&dir.path().join("test.db")).expect("db open");

    db.insert_drawer(&make_drawer("d1", "w")).expect("insert");
    db.insert_drawer(&make_drawer("d2", "w")).expect("insert");

    db.soft_delete_drawer("d1").expect("delete d1");

    let recent = db.top_drawers(10).expect("recent");
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].id, "d2");
}

#[test]
fn test_scope_counts_excludes_deleted() {
    let dir = tempdir().expect("temp dir");
    let db = Database::open(&dir.path().join("test.db")).expect("db open");

    db.insert_drawer(&make_drawer("d1", "w")).expect("insert");
    db.insert_drawer(&make_drawer("d2", "w")).expect("insert");

    db.soft_delete_drawer("d1").expect("delete d1");

    let scopes = db.scope_counts().expect("scopes");
    assert_eq!(scopes.len(), 1);
    assert_eq!(scopes[0].2, 1); // only 1 active drawer
}

// --- FTS5 tests ---

#[test]
fn test_fts5_search_basic() {
    let dir = tempdir().expect("temp dir");
    let db = Database::open(&dir.path().join("test.db")).expect("db open");

    let mut d1 = make_drawer("d1", "w");
    d1.content = "Clerk authentication migration decision".into();
    db.insert_drawer(&d1).expect("insert");

    let mut d2 = make_drawer("d2", "w");
    d2.content = "SQLite database performance tuning".into();
    db.insert_drawer(&d2).expect("insert");

    // BM25 search for "Clerk"
    let results = db.search_fts("Clerk", None, None, 10).expect("fts search");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "d1");

    // BM25 search for "SQLite"
    let results = db.search_fts("SQLite", None, None, 10).expect("fts search");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "d2");
}

#[test]
fn test_fts5_excludes_soft_deleted() {
    let dir = tempdir().expect("temp dir");
    let db = Database::open(&dir.path().join("test.db")).expect("db open");

    let mut d1 = make_drawer("d1", "w");
    d1.content = "important Clerk decision".into();
    db.insert_drawer(&d1).expect("insert");

    db.soft_delete_drawer("d1").expect("soft delete");

    let results = db.search_fts("Clerk", None, None, 10).expect("fts search");
    assert!(results.is_empty());
}

// --- Triples tests ---

#[test]
fn test_triple_crud() {
    use mempal_core::types::Triple;

    let dir = tempdir().expect("temp dir");
    let db = Database::open(&dir.path().join("test.db")).expect("db open");

    let triple = Triple {
        id: "t1".into(),
        subject: "Kai".into(),
        predicate: "recommends".into(),
        object: "Clerk".into(),
        valid_from: Some("2026-04-10".into()),
        valid_to: None,
        confidence: 1.0,
        source_drawer: None,
    };
    db.insert_triple(&triple).expect("insert triple");

    // Query by subject
    let results = db
        .query_triples(Some("Kai"), None, None, true)
        .expect("query");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].object, "Clerk");

    // Query by object
    let results = db
        .query_triples(None, None, Some("Clerk"), true)
        .expect("query");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].subject, "Kai");

    // Invalidate
    let ok = db.invalidate_triple("t1").expect("invalidate");
    assert!(ok);

    // Active-only query should return empty
    let results = db
        .query_triples(Some("Kai"), None, None, true)
        .expect("query");
    assert!(results.is_empty());

    // All query should still find it
    let results = db
        .query_triples(Some("Kai"), None, None, false)
        .expect("query");
    assert_eq!(results.len(), 1);

    assert_eq!(db.triple_count().expect("count"), 1);
}

// --- Tunnels tests ---

#[test]
fn test_tunnels_single_wing() {
    let dir = tempdir().expect("temp dir");
    let db = Database::open(&dir.path().join("test.db")).expect("db open");

    let mut d = make_drawer("d1", "wing_a");
    d.room = Some("auth".into());
    db.insert_drawer(&d).expect("insert");

    let tunnels = db.find_tunnels().expect("tunnels");
    assert!(tunnels.is_empty()); // need 2+ wings for a tunnel
}

#[test]
fn test_tunnels_cross_wing() {
    let dir = tempdir().expect("temp dir");
    let db = Database::open(&dir.path().join("test.db")).expect("db open");

    let mut d1 = make_drawer("d1", "wing_a");
    d1.room = Some("auth".into());
    db.insert_drawer(&d1).expect("insert");

    let mut d2 = make_drawer("d2", "wing_b");
    d2.room = Some("auth".into());
    db.insert_drawer(&d2).expect("insert");

    let mut d3 = make_drawer("d3", "wing_a");
    d3.room = Some("deploy".into());
    db.insert_drawer(&d3).expect("insert");

    let tunnels = db.find_tunnels().expect("tunnels");
    assert_eq!(tunnels.len(), 1);
    assert_eq!(tunnels[0].0, "auth");
    assert!(tunnels[0].1.contains(&"wing_a".to_string()));
    assert!(tunnels[0].1.contains(&"wing_b".to_string()));
}

// --- Schema version ---

#[test]
fn test_schema_version_is_4() {
    let dir = tempdir().expect("temp dir");
    let db = Database::open(&dir.path().join("test.db")).expect("db open");
    assert_eq!(db.schema_version().expect("version"), 4);
}
