use std::collections::BTreeSet;

use mempal::core::db::Database;
use mempal::embed::Embedder;
use mempal::xurl::ingest::{self, HermesIngestOptions, IngestCallbacks};
use mempal::xurl::model::Tool;
use mempal::xurl::search::{self, SearchOptions};
use mempal::xurl::store::TurnFilter;
use rusqlite::Connection;
use tempfile::TempDir;

struct FixedEmbedder;

#[async_trait::async_trait]
impl Embedder for FixedEmbedder {
    async fn embed(&self, texts: &[&str]) -> mempal::embed::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.1; 16]).collect())
    }

    fn dimensions(&self) -> usize {
        16
    }

    fn name(&self) -> &str {
        "fixed"
    }
}

fn create_hermes_snapshot(path: &std::path::Path) {
    let conn = Connection::open(path).expect("open Hermes fixture");
    conn.execute_batch(
        "CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            title TEXT,
            source TEXT,
            cwd TEXT
        );
        CREATE TABLE messages (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            timestamp REAL NOT NULL,
            active INTEGER NOT NULL,
            compacted INTEGER NOT NULL
        );
        INSERT INTO sessions (id, title, source, cwd)
        VALUES ('session-1', 'Reconcile fixture', 'cli', '/repo/project');
        INSERT INTO messages
            (id, session_id, role, content, timestamp, active, compacted) VALUES
            ('keep', 'session-1', 'user', 'canonical keep marker', 1.0, 1, 0),
            ('rewound', 'session-1', 'assistant', 'ghost rewind marker', 2.0, 1, 0),
            ('deleted', 'session-1', 'user', 'ghost delete marker', 3.0, 1, 0),
            ('summary-old', 'session-1', 'assistant', 'ghost replaced summary', 4.0, 0, 1);",
    )
    .expect("create Hermes fixture schema");
}

fn mutate_hermes_snapshot(path: &std::path::Path) {
    let conn = Connection::open(path).expect("reopen Hermes fixture");
    conn.execute_batch(
        "UPDATE messages SET active = 0, compacted = 0 WHERE id IN ('rewound', 'summary-old');
         DELETE FROM messages WHERE id = 'deleted';
         INSERT INTO messages
             (id, session_id, role, content, timestamp, active, compacted)
         VALUES
             ('summary-new', 'session-1', 'assistant', 'canonical replacement summary', 5.0, 0, 1);",
    )
    .expect("mutate Hermes fixture");
}

fn create_scoped_hermes_snapshot(path: &std::path::Path) {
    let conn = Connection::open(path).expect("open scoped Hermes fixture");
    conn.execute_batch(
        "CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            title TEXT,
            source TEXT,
            cwd TEXT
        );
        CREATE TABLE messages (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            timestamp REAL NOT NULL,
            active INTEGER NOT NULL,
            compacted INTEGER NOT NULL
        );
        INSERT INTO sessions (id, title, source, cwd) VALUES
            ('target-session', 'Target', 'cli', '/repo/target'),
            ('other-session', 'Other', 'cli', '/repo/other');
        INSERT INTO messages
            (id, session_id, role, content, timestamp, active, compacted) VALUES
            ('target-message', 'target-session', 'user', 'target marker', 1.0, 1, 0),
            ('other-message', 'other-session', 'user', 'other marker', 2.0, 1, 0);",
    )
    .expect("create scoped Hermes fixture schema");
}

async fn ingest_snapshot(db: &Database, source: &std::path::Path) -> ingest::IngestStats {
    ingest_snapshot_with_cwd(db, source, None).await
}

async fn ingest_snapshot_with_cwd(
    db: &Database,
    source: &std::path::Path,
    cwd: Option<&str>,
) -> ingest::IngestStats {
    ingest::ingest_hermes_with_vector_fingerprint(
        db,
        &FixedEmbedder,
        &HermesIngestOptions {
            profile: "default".to_string(),
            db_path: Some(source.to_path_buf()),
            cwd: cwd.map(str::to_string),
            ..Default::default()
        },
        "fixed:16",
        IngestCallbacks {
            on_file_parsed: None,
            on_embed_progress: None,
        },
    )
    .await
    .expect("ingest Hermes snapshot")
}

#[tokio::test]
async fn reingest_removes_rewound_deleted_and_replaced_hermes_turns() {
    let dir = TempDir::new().expect("tempdir");
    let source = dir.path().join("state.db");
    let mempal_path = dir.path().join("mempal.db");
    create_hermes_snapshot(&source);
    let db = Database::open(&mempal_path).expect("open Mempal fixture");

    let first = ingest_snapshot(&db, &source).await;
    assert_eq!(first.turns_inserted, 4);
    assert_eq!(first.vectors_created, 4);

    mutate_hermes_snapshot(&source);
    let reconciled = ingest_snapshot(&db, &source).await;
    assert_eq!(reconciled.turns_removed, 3);

    let stored_ids = db
        .conn()
        .prepare(
            "SELECT message_id FROM conversation_turns
             WHERE tool = 'hermes' AND hermes_profile = 'default'
             ORDER BY message_id",
        )
        .expect("prepare stored identity query")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query stored identities")
        .collect::<Result<BTreeSet<_>, _>>()
        .expect("collect stored identities");
    assert_eq!(
        stored_ids,
        BTreeSet::from(["keep".to_string(), "summary-new".to_string()])
    );

    let vector_count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM conversation_turn_vectors",
            [],
            |row| row.get(0),
        )
        .expect("count reconciled vectors");
    assert_eq!(vector_count, 2);

    let search = search::search(
        &db,
        &FixedEmbedder,
        "ghost marker",
        SearchOptions {
            limit: 10,
            min_score: Some(0.0),
            filter: Some(TurnFilter {
                tool: Some(Tool::Hermes),
                hermes_profile: Some("default".to_string()),
                limit: 10,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .expect("search reconciled Hermes turns");
    let cited_ids = search
        .hits
        .iter()
        .filter_map(|hit| hit.message_id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(cited_ids, stored_ids);

    let unchanged = ingest_snapshot(&db, &source).await;
    assert_eq!(unchanged.turns_inserted, 0);
    assert_eq!(unchanged.turns_updated, 0);
    assert_eq!(unchanged.turns_removed, 0);
    assert_eq!(unchanged.turns_skipped, 2);
    assert_eq!(unchanged.vectors_created, 0);
}

#[tokio::test]
async fn cwd_scoped_reingest_preserves_stale_rows_outside_scope() {
    let dir = TempDir::new().expect("tempdir");
    let source = dir.path().join("state.db");
    let mempal_path = dir.path().join("mempal.db");
    create_scoped_hermes_snapshot(&source);
    let db = Database::open(&mempal_path).expect("open Mempal fixture");

    let first = ingest_snapshot(&db, &source).await;
    assert_eq!(first.turns_inserted, 2);

    Connection::open(&source)
        .expect("reopen scoped Hermes fixture")
        .execute("DELETE FROM messages", [])
        .expect("delete source messages");

    let scoped = ingest_snapshot_with_cwd(&db, &source, Some("/repo/target/")).await;
    assert_eq!(scoped.turns_removed, 1);

    let remaining_ids = db
        .conn()
        .prepare(
            "SELECT message_id FROM conversation_turns
             WHERE tool = 'hermes' ORDER BY message_id",
        )
        .expect("prepare remaining identity query")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query remaining identities")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect remaining identities");
    assert_eq!(remaining_ids, vec!["other-message"]);
}
