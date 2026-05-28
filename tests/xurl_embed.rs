use mempal::core::db::Database;
use mempal::embed::Embedder;
use mempal::xurl::embed;
use rusqlite::params;
use tempfile::TempDir;

struct TestDb {
    _dir: TempDir,
    inner: Database,
}

impl TestDb {
    fn conn(&self) -> &rusqlite::Connection {
        self.inner.conn()
    }
}

fn open_temp_db_at_fork_ext(_version: u32) -> TestDb {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("palace.db");
    let db = Database::open(&path).expect("open db");
    TestDb {
        _dir: dir,
        inner: db,
    }
}

/// A mock embedder that always returns fixed-value vectors of the given dimension.
struct MockEmbedder {
    dim: usize,
}

impl MockEmbedder {
    fn new_fixed_dim(dim: usize) -> Self {
        Self { dim }
    }
}

#[async_trait::async_trait]
impl Embedder for MockEmbedder {
    async fn embed(&self, texts: &[&str]) -> mempal::embed::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.1f32; self.dim]).collect())
    }

    fn dimensions(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &str {
        "mock"
    }
}

/// Minimal description of a turn row for test fixtures.
struct TurnRow<'a> {
    id: &'a str,
    session: &'a str,
    tool: &'a str,
    turn_index: i64,
    role: &'a str,
    content: &'a str,
    timestamp: f64,
    token_count: Option<i64>,
}

/// Insert a minimal conversation_turns row directly for testing.
fn insert_raw_turn_row(conn: &rusqlite::Connection, row: TurnRow<'_>) {
    conn.execute(
        "INSERT INTO conversation_turns \
         (id, session_id, tool, turn_index, role, content, timestamp_epoch, \
          token_count, project_path, git_branch, is_csa_delegated, provenance) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,NULL,NULL,0,'human')",
        params![
            row.id,
            row.session,
            row.tool,
            row.turn_index,
            row.role,
            row.content,
            row.timestamp,
            row.token_count
        ],
    )
    .expect("insert test row");
}

#[tokio::test]
async fn long_assistant_turn_produces_multiple_vectors() {
    let db = open_temp_db_at_fork_ext(16);
    let turn_id = "turn-001";
    // ~1500 words → well over 512 tokens (≈600 tokens by heuristic)
    let long_content = "word ".repeat(1500);
    insert_raw_turn_row(
        db.conn(),
        TurnRow {
            id: turn_id,
            session: "sess1",
            tool: "cc",
            turn_index: 0,
            role: "assistant",
            content: &long_content,
            timestamp: 1.0,
            token_count: Some(1500),
        },
    );

    let embedder = MockEmbedder::new_fixed_dim(256);
    embed::embed_unindexed_turns(&db.inner, &embedder)
        .await
        .unwrap();

    let vector_count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM conversation_turn_vectors WHERE turn_id=?",
            params![turn_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        vector_count > 1,
        "expected multiple chunks for long turn, got {vector_count}"
    );
}

#[tokio::test]
async fn short_turn_produces_exactly_one_vector() {
    let db = open_temp_db_at_fork_ext(16);
    let turn_id = "turn-002";
    insert_raw_turn_row(
        db.conn(),
        TurnRow {
            id: turn_id,
            session: "sess1",
            tool: "cc",
            turn_index: 1,
            role: "user",
            content: "Hi there",
            timestamp: 1.1,
            token_count: Some(3),
        },
    );

    let embedder = MockEmbedder::new_fixed_dim(256);
    embed::embed_unindexed_turns(&db.inner, &embedder)
        .await
        .unwrap();

    let count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM conversation_turn_vectors WHERE turn_id=?",
            params![turn_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn re_embed_already_indexed_turns_is_noop() {
    let db = open_temp_db_at_fork_ext(16);
    let turn_id = "turn-003";
    insert_raw_turn_row(
        db.conn(),
        TurnRow {
            id: turn_id,
            session: "sess1",
            tool: "cc",
            turn_index: 0,
            role: "user",
            content: "Hello world",
            timestamp: 1.0,
            token_count: None,
        },
    );

    let embedder = MockEmbedder::new_fixed_dim(256);

    // First run: should embed
    let stats1 = embed::embed_unindexed_turns(&db.inner, &embedder)
        .await
        .unwrap();
    assert_eq!(stats1.turns_processed, 1);

    // Second run: already indexed, should be a noop
    let stats2 = embed::embed_unindexed_turns(&db.inner, &embedder)
        .await
        .unwrap();
    assert_eq!(stats2.embedded, 0, "re-embedding should be a noop");
    assert_eq!(stats2.turns_processed, 0);
}

#[tokio::test]
async fn embed_stats_counts_chunks() {
    let db = open_temp_db_at_fork_ext(16);
    // Two short turns → 2 turns × 1 chunk each
    insert_raw_turn_row(
        db.conn(),
        TurnRow {
            id: "t-a",
            session: "s",
            tool: "cc",
            turn_index: 0,
            role: "user",
            content: "Hello",
            timestamp: 1.0,
            token_count: None,
        },
    );
    insert_raw_turn_row(
        db.conn(),
        TurnRow {
            id: "t-b",
            session: "s",
            tool: "cc",
            turn_index: 1,
            role: "assistant",
            content: "World",
            timestamp: 2.0,
            token_count: None,
        },
    );

    let embedder = MockEmbedder::new_fixed_dim(64);
    let stats = embed::embed_unindexed_turns(&db.inner, &embedder)
        .await
        .unwrap();
    assert_eq!(stats.turns_processed, 2);
    assert_eq!(stats.chunks_total, 2);
    assert_eq!(stats.embedded, 2);
}
