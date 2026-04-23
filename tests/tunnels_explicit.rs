use mempal::core::db::Database;
use mempal::core::types::TunnelEndpoint;
use rusqlite::Connection;
use tempfile::TempDir;

fn new_db() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    (tmp, db)
}

fn endpoint(wing: &str, room: Option<&str>) -> TunnelEndpoint {
    TunnelEndpoint {
        wing: wing.to_string(),
        room: room.map(ToOwned::to_owned),
    }
}

fn create_v5_db(path: &std::path::Path) {
    let conn = Connection::open(path).expect("open v5 db");
    conn.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;

        CREATE TABLE drawers (
            id TEXT PRIMARY KEY,
            content TEXT NOT NULL,
            wing TEXT NOT NULL,
            room TEXT,
            source_file TEXT,
            source_type TEXT NOT NULL CHECK(source_type IN ('project', 'conversation', 'manual')),
            added_at TEXT NOT NULL,
            chunk_index INTEGER,
            deleted_at TEXT,
            importance INTEGER DEFAULT 0,
            memory_kind TEXT NOT NULL CHECK(memory_kind IN ('evidence', 'knowledge')) DEFAULT 'evidence',
            domain TEXT NOT NULL CHECK(domain IN ('project', 'agent', 'skill', 'global')) DEFAULT 'project',
            field TEXT NOT NULL DEFAULT 'general',
            anchor_kind TEXT NOT NULL CHECK(anchor_kind IN ('global', 'repo', 'worktree')) DEFAULT 'repo',
            anchor_id TEXT NOT NULL DEFAULT 'repo://legacy',
            parent_anchor_id TEXT,
            provenance TEXT CHECK(provenance IN ('runtime', 'research', 'human')),
            statement TEXT,
            tier TEXT CHECK(tier IN ('qi', 'shu', 'dao_ren', 'dao_tian')),
            status TEXT CHECK(status IN ('candidate', 'promoted', 'canonical', 'demoted', 'retired')),
            supporting_refs TEXT NOT NULL DEFAULT '[]',
            counterexample_refs TEXT NOT NULL DEFAULT '[]',
            teaching_refs TEXT NOT NULL DEFAULT '[]',
            verification_refs TEXT NOT NULL DEFAULT '[]',
            scope_constraints TEXT,
            trigger_hints TEXT
        );

        CREATE TABLE triples (
            id TEXT PRIMARY KEY,
            subject TEXT NOT NULL,
            predicate TEXT NOT NULL,
            object TEXT NOT NULL,
            valid_from TEXT,
            valid_to TEXT,
            confidence REAL DEFAULT 1.0,
            source_drawer TEXT REFERENCES drawers(id)
        );

        CREATE TABLE taxonomy (
            wing TEXT NOT NULL,
            room TEXT NOT NULL DEFAULT '',
            display_name TEXT,
            keywords TEXT,
            PRIMARY KEY (wing, room)
        );

        CREATE INDEX idx_drawers_wing ON drawers(wing);
        CREATE INDEX idx_drawers_wing_room ON drawers(wing, room);
        CREATE INDEX idx_drawers_deleted_at ON drawers(deleted_at);
        CREATE INDEX idx_triples_subject ON triples(subject);
        CREATE INDEX idx_triples_object ON triples(object);

        CREATE VIRTUAL TABLE drawers_fts USING fts5(
            content,
            content='drawers',
            content_rowid='rowid'
        );

        CREATE TRIGGER drawers_ai AFTER INSERT ON drawers BEGIN
            INSERT INTO drawers_fts(rowid, content) VALUES (new.rowid, new.content);
        END;

        CREATE TRIGGER drawers_au_softdelete AFTER UPDATE OF deleted_at ON drawers
            WHEN new.deleted_at IS NOT NULL AND old.deleted_at IS NULL BEGIN
            INSERT INTO drawers_fts(drawers_fts, rowid, content)
            VALUES ('delete', old.rowid, old.content);
        END;

        INSERT INTO drawers (
            id, content, wing, room, source_file, source_type, added_at, chunk_index,
            deleted_at, importance, provenance
        )
        VALUES
            ('drawer_001', 'content 1', 'mempal', 'auth', 'a.md', 'project', '1710000001', 0, NULL, 1, 'research'),
            ('drawer_002', 'content 2', 'robrix2', 'matrix', 'b.md', 'project', '1710000002', 0, NULL, 2, 'research');

        INSERT INTO triples (id, subject, predicate, object)
        VALUES ('triple_001', 'mempal', 'relates_to', 'robrix2');

        PRAGMA user_version = 5;
        "#,
    )
    .expect("apply v5 schema");
}

#[test]
fn test_schema_v5_to_v6_migration_preserves_data() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    create_v5_db(&db_path);

    let db = Database::open(&db_path).expect("migrate v5 db");

    assert_eq!(db.schema_version().expect("schema version"), 6);
    assert_eq!(db.drawer_count().expect("drawer count"), 2);
    assert_eq!(db.triple_count().expect("triple count"), 1);
    let tunnels_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM tunnels", [], |row| row.get(0))
        .expect("tunnels table should exist");
    assert_eq!(tunnels_count, 0);
}

#[test]
fn test_add_tunnel_dedup_unordered() {
    let (_tmp, db) = new_db();
    let left = endpoint("mempal", Some("auth"));
    let right = endpoint("robrix2", Some("matrix"));

    let first = db
        .create_tunnel(&left, &right, "both handle user auth", Some("codex"))
        .expect("create first tunnel");
    let second = db
        .create_tunnel(&right, &left, "duplicate label ignored", Some("claude"))
        .expect("create duplicate tunnel");

    assert_eq!(first.id, second.id);
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM tunnels", [], |row| row.get(0))
        .expect("count tunnels");
    assert_eq!(count, 1);
}

#[test]
fn test_add_self_tunnel_rejected() {
    let (_tmp, db) = new_db();
    let left = endpoint("mempal", Some("auth"));

    let error = db
        .create_tunnel(&left, &left, "self", Some("codex"))
        .expect_err("self-link should be rejected");

    assert!(error.to_string().contains("self-link"));
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM tunnels", [], |row| row.get(0))
        .expect("count tunnels");
    assert_eq!(count, 0);
}

#[test]
fn test_delete_explicit_tunnel_soft_delete() {
    let (_tmp, db) = new_db();
    let tunnel = db
        .create_tunnel(
            &endpoint("mempal", Some("auth")),
            &endpoint("robrix2", Some("matrix")),
            "both handle user auth",
            Some("codex"),
        )
        .expect("create tunnel");

    assert!(
        db.delete_explicit_tunnel(&tunnel.id)
            .expect("delete explicit tunnel")
    );
    let deleted_at: Option<String> = db
        .conn()
        .query_row(
            "SELECT deleted_at FROM tunnels WHERE id = ?1",
            [&tunnel.id],
            |row| row.get(0),
        )
        .expect("read deleted_at");
    assert!(deleted_at.is_some());
    assert!(
        db.list_explicit_tunnels(None)
            .expect("list explicit tunnels")
            .is_empty()
    );
}
