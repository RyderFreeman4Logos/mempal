use mempal::core::db::Database;
use mempal::embed::Embedder;
use mempal::ingest::normalize::CURRENT_NORMALIZE_VERSION;
use mempal::ingest::{IngestOptions, ingest_file_with_options};
use rusqlite::Connection;
use tempfile::TempDir;

struct StubEmbedder;

#[async_trait::async_trait]
impl Embedder for StubEmbedder {
    async fn embed(&self, texts: &[&str]) -> mempal::embed::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
    }

    fn dimensions(&self) -> usize {
        3
    }

    fn name(&self) -> &str {
        "stub"
    }
}

fn create_v6_db(path: &std::path::Path, drawer_count: usize) {
    let conn = Connection::open(path).expect("open v6 db");
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

        CREATE TABLE tunnels (
            id TEXT PRIMARY KEY,
            left_wing TEXT NOT NULL,
            left_room TEXT,
            right_wing TEXT NOT NULL,
            right_room TEXT,
            label TEXT NOT NULL,
            created_at TEXT NOT NULL,
            created_by TEXT,
            deleted_at TEXT
        );

        CREATE INDEX idx_drawers_wing ON drawers(wing);
        CREATE INDEX idx_drawers_wing_room ON drawers(wing, room);
        CREATE INDEX idx_drawers_deleted_at ON drawers(deleted_at);
        CREATE INDEX idx_triples_subject ON triples(subject);
        CREATE INDEX idx_triples_object ON triples(object);
        CREATE INDEX idx_tunnels_left ON tunnels(left_wing, left_room) WHERE deleted_at IS NULL;
        CREATE INDEX idx_tunnels_right ON tunnels(right_wing, right_room) WHERE deleted_at IS NULL;

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

        PRAGMA user_version = 6;
        "#,
    )
    .expect("apply v6 schema");

    for index in 0..drawer_count {
        conn.execute(
            r#"
            INSERT INTO drawers (
                id, content, wing, room, source_file, source_type, added_at, chunk_index,
                deleted_at, importance, provenance
            )
            VALUES (?1, ?2, 'mempal', 'normalize', ?3, 'project', '1710000000', ?4, NULL, 0, 'research')
            "#,
            (
                format!("drawer_{index:03}"),
                format!("content {index}"),
                format!("doc-{index}.md"),
                index as i64,
            ),
        )
        .expect("insert v6 drawer");
    }
}

fn count_normalize_version(db: &Database, version: u32) -> i64 {
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM drawers WHERE normalize_version = ?1",
            [version],
            |row| row.get(0),
        )
        .expect("count normalize_version")
}

#[test]
fn test_migration_v6_to_v7_stamps_normalize_version_1() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    create_v6_db(&db_path, 20);

    let db = Database::open(&db_path).expect("migrate v6 db");

    assert_eq!(db.schema_version().expect("schema version"), 7);
    assert_eq!(db.drawer_count().expect("drawer count"), 20);
    assert_eq!(count_normalize_version(&db, 1), 20);
}

#[test]
fn test_drawer_count_by_normalize_version_and_stale_count() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    create_v6_db(&db_path, 20);
    let db = Database::open(&db_path).expect("migrate v6 db");

    db.conn()
        .execute(
            "UPDATE drawers SET normalize_version = 0 WHERE id IN ('drawer_000', 'drawer_001', 'drawer_002', 'drawer_003', 'drawer_004')",
            [],
        )
        .expect("mark stale drawers");

    assert_eq!(db.stale_drawer_count(1).expect("stale count"), 5);
    assert_eq!(
        db.drawer_count_by_normalize_version()
            .expect("version histogram"),
        vec![(0, 5), (1, 15)]
    );
}

#[tokio::test]
async fn test_new_ingest_writes_current_normalize_version() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    let source = tmp.path().join("doc.md");
    std::fs::write(&source, "normalize version source content").expect("write source");

    ingest_file_with_options(
        &db,
        &StubEmbedder,
        &source,
        "mempal",
        IngestOptions {
            room: Some("normalize"),
            source_root: source.parent(),
            dry_run: false,
            source_file_override: None,
            replace_existing_source: false,
        },
    )
    .await
    .expect("ingest source");

    let versions = distinct_versions_for_source(&db, "doc.md");
    assert_eq!(versions, vec![CURRENT_NORMALIZE_VERSION]);
}

fn distinct_versions_for_source(db: &Database, source_file: &str) -> Vec<u32> {
    let mut statement = db
        .conn()
        .prepare(
            r#"
            SELECT DISTINCT normalize_version
            FROM drawers
            WHERE source_file = ?1
            ORDER BY normalize_version
            "#,
        )
        .expect("prepare versions");
    statement
        .query_map([source_file], |row| row.get::<_, u32>(0))
        .expect("query versions")
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("collect versions")
}
