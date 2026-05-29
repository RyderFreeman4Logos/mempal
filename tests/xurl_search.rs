use mempal::core::db::Database;
use mempal::embed::Embedder;
use mempal::xurl::model::{Provenance, RawTurn, Role, Tool};
use mempal::xurl::search::{self, SearchOptions};
use mempal::xurl::store::{self, TurnFilter};
use tempfile::TempDir;

// ── test DB helper ────────────────────────────────────────────────────────────

struct TestDb {
    _dir: TempDir,
    inner: Database,
}

impl TestDb {
    fn conn(&self) -> &rusqlite::Connection {
        self.inner.conn()
    }
}

fn open_temp_db() -> TestDb {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("palace.db");
    let db = Database::open(&path).expect("open db");
    TestDb {
        _dir: dir,
        inner: db,
    }
}

// ── mock embedders ────────────────────────────────────────────────────────────

/// Fixed-value mock: every text returns the same vector. Useful for
/// testing structural correctness (dedup, filter) but not semantic ranking.
struct FixedEmbedder {
    dim: usize,
}

#[async_trait::async_trait]
impl Embedder for FixedEmbedder {
    async fn embed(&self, texts: &[&str]) -> mempal::embed::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.5f32; self.dim]).collect())
    }
    fn dimensions(&self) -> usize {
        self.dim
    }
    fn name(&self) -> &str {
        "fixed"
    }
}

/// Semantic mock: returns different vectors based on keyword presence.
/// dim must be ≥ 4.
/// Dim 0: "database" / "migration" / "flyway"
/// Dim 1: "bread" / "bake" / "flour"
/// Dim 2: "rust" / "ownership" / "borrow"
/// Dim 3: fallback (when no keyword matches)
struct SemanticEmbedder {
    dim: usize,
}

impl SemanticEmbedder {
    fn new(dim: usize) -> Self {
        assert!(dim >= 4, "dim must be ≥ 4 for SemanticEmbedder");
        Self { dim }
    }

    fn score_text(&self, text: &str) -> Vec<f32> {
        let lower = text.to_lowercase();
        let mut v = vec![0.0f32; self.dim];
        if lower.contains("database") || lower.contains("migration") || lower.contains("flyway") {
            v[0] = 1.0;
        }
        if lower.contains("bread") || lower.contains("bake") || lower.contains("flour") {
            v[1] = 1.0;
        }
        if lower.contains("rust") || lower.contains("ownership") || lower.contains("borrow") {
            v[2] = 1.0;
        }
        // Normalize
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        } else {
            v[3] = 1.0; // fallback: unique per-text content
        }
        v
    }
}

#[async_trait::async_trait]
impl Embedder for SemanticEmbedder {
    async fn embed(&self, texts: &[&str]) -> mempal::embed::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.score_text(t)).collect())
    }
    fn dimensions(&self) -> usize {
        self.dim
    }
    fn name(&self) -> &str {
        "semantic-mock"
    }
}

// ── fixture helpers ───────────────────────────────────────────────────────────

fn make_turn(session_id: &str, tool: Tool, turn_index: u32, role: Role, content: &str) -> RawTurn {
    RawTurn {
        session_id: session_id.to_string(),
        tool,
        role,
        content: content.to_string(),
        timestamp_epoch: 1_748_000_000.0 + f64::from(turn_index),
        project_path: None,
        git_branch: None,
        is_csa_delegated: false,
        provenance: Provenance::Human,
        turn_index,
    }
}

/// Insert a turn and immediately embed it so it appears in search results.
async fn seed_turn<E: Embedder + ?Sized>(
    db: &TestDb,
    embedder: &E,
    session_id: &str,
    tool: Tool,
    turn_index: u32,
    role: Role,
    content: &str,
) {
    let turn = make_turn(session_id, tool, turn_index, role, content);
    store::insert_turns(db.conn(), &[turn]).unwrap();
    mempal::xurl::embed::embed_unindexed_turns(&db.inner, embedder, None)
        .await
        .unwrap();
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn search_returns_semantically_relevant_turn() {
    let db = open_temp_db();
    let embedder = SemanticEmbedder::new(16);

    seed_turn(
        &db,
        &embedder,
        "sess1",
        Tool::Cc,
        0,
        Role::User,
        "database migration strategy",
    )
    .await;
    seed_turn(
        &db,
        &embedder,
        "sess1",
        Tool::Cc,
        1,
        Role::Assistant,
        "Use flyway for schema changes",
    )
    .await;
    seed_turn(
        &db,
        &embedder,
        "sess2",
        Tool::Codex,
        0,
        Role::User,
        "how to bake bread",
    )
    .await;

    let results = search::search(
        &db.inner,
        &embedder,
        "database migration",
        SearchOptions {
            limit: 5,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert!(!results.hits.is_empty(), "expected at least one result");
    // Top result should be about database/migration, not bread
    let top = &results.hits[0];
    let is_database = top.content.contains("database")
        || top.content.contains("migration")
        || top.content.contains("flyway");
    assert!(
        is_database,
        "top result should be database-related, got: {}",
        top.content
    );
}

#[tokio::test]
async fn search_with_tool_filter() {
    let db = open_temp_db();
    let embedder = SemanticEmbedder::new(16);

    // Same content in both cc and codex; filter to cc only
    seed_turn(
        &db,
        &embedder,
        "sess1",
        Tool::Cc,
        0,
        Role::User,
        "rust ownership rules",
    )
    .await;
    seed_turn(
        &db,
        &embedder,
        "sess2",
        Tool::Codex,
        0,
        Role::User,
        "rust ownership rules",
    )
    .await;

    let results = search::search(
        &db.inner,
        &embedder,
        "rust ownership",
        SearchOptions {
            limit: 10,
            filter: Some(TurnFilter {
                tool: Some(Tool::Cc),
                limit: 10,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert!(!results.hits.is_empty());
    assert!(
        results.hits.iter().all(|r| r.tool == "cc"),
        "all results should be cc, got: {:?}",
        results
            .hits
            .iter()
            .map(|r| r.tool.as_str())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn search_deduplicates_multi_chunk_turns() {
    let db = open_temp_db();
    let embedder = FixedEmbedder { dim: 32 };

    // Insert one turn then manually insert two vector chunks for it
    let turn = make_turn(
        "sess1",
        Tool::Cc,
        0,
        Role::Assistant,
        "long assistant answer",
    );
    store::insert_turns(db.conn(), &[turn]).unwrap();

    // Determine the generated turn ID
    let turn_id: String = db
        .conn()
        .query_row(
            "SELECT id FROM conversation_turns WHERE session_id='sess1' AND turn_index=0",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // Insert two chunks manually
    let blob: Vec<u8> = vec![0u8; 32 * 4]; // 32-dim zero vector
    db.conn().execute(
        "INSERT INTO conversation_turn_vectors (turn_id, chunk_index, vector) VALUES (?1, 0, ?2)",
        rusqlite::params![&turn_id, &blob],
    ).unwrap();
    db.conn().execute(
        "INSERT INTO conversation_turn_vectors (turn_id, chunk_index, vector) VALUES (?1, 1, ?2)",
        rusqlite::params![&turn_id, &blob],
    ).unwrap();

    let results = search::search(
        &db.inner,
        &embedder,
        "anything",
        SearchOptions {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Even though there are two chunks, the turn should appear exactly once
    let ids: Vec<&str> = results.hits.iter().map(|r| r.turn_id.as_str()).collect();
    let unique_ids: std::collections::HashSet<&&str> = ids.iter().collect();
    assert_eq!(ids.len(), unique_ids.len(), "duplicate turn_ids in results");
}

#[tokio::test]
async fn search_excludes_csa_by_default() {
    let db = open_temp_db();
    let embedder = FixedEmbedder { dim: 16 };

    // Insert a normal turn and a CSA-delegated turn
    let mut normal = make_turn("sess1", Tool::Cc, 0, Role::User, "normal turn");
    normal.is_csa_delegated = false;
    let mut csa = make_turn("sess2", Tool::Cc, 0, Role::User, "csa delegated turn");
    csa.is_csa_delegated = true;

    store::insert_turns(db.conn(), &[normal, csa]).unwrap();
    mempal::xurl::embed::embed_unindexed_turns(&db.inner, &embedder, None)
        .await
        .unwrap();

    // Default search (exclude CSA)
    let results = search::search(
        &db.inner,
        &embedder,
        "turn",
        SearchOptions {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(results.hits.len(), 1, "should exclude CSA turn by default");
    assert_eq!(results.hits[0].content, "normal turn");

    // With include_csa=true
    let results_all = search::search(
        &db.inner,
        &embedder,
        "turn",
        SearchOptions {
            limit: 10,
            include_csa: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        results_all.hits.len(),
        2,
        "should include CSA turn when flag is set"
    );
}

#[tokio::test]
async fn search_empty_db_returns_empty() {
    let db = open_temp_db();
    let embedder = FixedEmbedder { dim: 16 };
    let results = search::search(
        &db.inner,
        &embedder,
        "anything",
        SearchOptions {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(results.hits.is_empty());
}

#[tokio::test]
async fn test_search_min_score_filters_low_hits() {
    let db = open_temp_db();
    // Use a SemanticEmbedder so the query "database migration" gets score ~1.0 for
    // database-matching content and score 0.0 for unrelated content.
    let embedder = SemanticEmbedder::new(16);

    seed_turn(
        &db,
        &embedder,
        "sess1",
        Tool::Cc,
        0,
        Role::User,
        "database migration strategy",
    )
    .await;
    seed_turn(
        &db,
        &embedder,
        "sess2",
        Tool::Cc,
        0,
        Role::User,
        "how to bake bread",
    )
    .await;

    // With a high floor, only the relevant result should pass
    let result = search::search(
        &db.inner,
        &embedder,
        "database migration",
        SearchOptions {
            limit: 10,
            min_score: Some(0.9),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(
        result.hits.len(),
        1,
        "only the high-score hit should pass the floor"
    );
    assert!(
        result.hits[0].content.contains("database") || result.hits[0].content.contains("migration"),
        "passing hit should be database-related"
    );
    // bread hit should be captured as best_score_below_floor
    assert!(
        result.best_score_below_floor.is_some(),
        "low-score hit should be captured in best_score_below_floor"
    );
    assert_eq!(result.min_score_floor, Some(0.9));
}

#[tokio::test]
async fn test_search_dedup_identical_content() {
    let db = open_temp_db();
    let embedder = FixedEmbedder { dim: 16 };

    // Two different turns with identical content (simulates overlapping session ingestion)
    let turn_a = make_turn("sess-a", Tool::Cc, 0, Role::User, "identical content here");
    let turn_b = make_turn("sess-b", Tool::Cc, 0, Role::User, "identical content here");
    store::insert_turns(db.conn(), &[turn_a, turn_b]).unwrap();
    mempal::xurl::embed::embed_unindexed_turns(&db.inner, &embedder, None)
        .await
        .unwrap();

    let result = search::search(
        &db.inner,
        &embedder,
        "identical",
        SearchOptions {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Despite two stored turns, identical content should yield only one hit
    assert_eq!(
        result.hits.len(),
        1,
        "identical-content turns should be deduped to one hit, got: {}",
        result.hits.len()
    );
    assert_eq!(result.hits[0].content, "identical content here");
}
