use mempal::core::db::{CURRENT_VECTOR_INDEX_VERSION, Database};
use mempal::embed::Embedder;
use mempal::xurl::embed;
use mempal::xurl::model::{Provenance, RawTurn, Role, Tool, TurnMetadata};
use mempal::xurl::store;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

struct TestDb {
    _dir: TempDir,
    path: PathBuf,
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
        path,
        inner: db,
    }
}

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn open_home_db(home: &Path) -> Database {
    let mempal_home = home.join(".mempal");
    std::fs::create_dir_all(&mempal_home).expect("create mempal home");
    Database::open(&mempal_home.join("palace.db")).expect("open home db")
}

fn run_xurl_reindex(home: &Path, args: &[&str]) -> Output {
    Command::new(mempal_bin())
        .args(["xurl", "reindex"])
        .args(args)
        .env("HOME", home)
        .env("MEMPAL_EMBED_BACKEND", "unsupported-dry-run-backend")
        .output()
        .expect("run mempal xurl reindex")
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

/// An embedder that records how many times `embed()` is called, the total
/// number of chunks embedded, and the largest single call — used to prove that
/// chunks are merged ACROSS turns into a few sub-batch calls rather than one
/// HTTP round-trip per turn.
struct CountingEmbedder {
    dim: usize,
    calls: std::sync::atomic::AtomicUsize,
    total_inputs: std::sync::atomic::AtomicUsize,
    max_call_len: std::sync::atomic::AtomicUsize,
}

impl CountingEmbedder {
    fn new(dim: usize) -> Self {
        Self {
            dim,
            calls: std::sync::atomic::AtomicUsize::new(0),
            total_inputs: std::sync::atomic::AtomicUsize::new(0),
            max_call_len: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn total_inputs(&self) -> usize {
        self.total_inputs.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn max_call_len(&self) -> usize {
        self.max_call_len.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl Embedder for CountingEmbedder {
    async fn embed(&self, texts: &[&str]) -> mempal::embed::Result<Vec<Vec<f32>>> {
        use std::sync::atomic::Ordering::Relaxed;
        self.calls.fetch_add(1, Relaxed);
        self.total_inputs.fetch_add(texts.len(), Relaxed);
        self.max_call_len.fetch_max(texts.len(), Relaxed);
        Ok(texts.iter().map(|_| vec![0.1f32; self.dim]).collect())
    }

    fn dimensions(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &str {
        "counting"
    }
}

struct ValueEmbedder {
    dim: usize,
    value: f32,
    total_inputs: std::sync::atomic::AtomicUsize,
}

struct UpdatingEmbedder {
    dim: usize,
    old_value: f32,
    unchanged_value: f32,
    db_path: PathBuf,
    updated_turn: RawTurn,
    updated: std::sync::atomic::AtomicBool,
}

impl UpdatingEmbedder {
    fn new(
        dim: usize,
        old_value: f32,
        unchanged_value: f32,
        db_path: PathBuf,
        updated_turn: RawTurn,
    ) -> Self {
        Self {
            dim,
            old_value,
            unchanged_value,
            db_path,
            updated_turn,
            updated: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl Embedder for UpdatingEmbedder {
    async fn embed(&self, texts: &[&str]) -> mempal::embed::Result<Vec<Vec<f32>>> {
        if !self.updated.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let db = Database::open(&self.db_path).expect("open db during embed");
            store::insert_turns(db.conn(), std::slice::from_ref(&self.updated_turn))
                .expect("simulate concurrent content update");
        }

        Ok(texts
            .iter()
            .map(|text| {
                let value = if *text == "old content" {
                    self.old_value
                } else {
                    self.unchanged_value
                };
                vec![value; self.dim]
            })
            .collect())
    }

    fn dimensions(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &str {
        "updating"
    }
}

impl ValueEmbedder {
    fn new(dim: usize, value: f32) -> Self {
        Self {
            dim,
            value,
            total_inputs: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn total_inputs(&self) -> usize {
        self.total_inputs.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl Embedder for ValueEmbedder {
    async fn embed(&self, texts: &[&str]) -> mempal::embed::Result<Vec<Vec<f32>>> {
        self.total_inputs
            .fetch_add(texts.len(), std::sync::atomic::Ordering::Relaxed);
        Ok(texts.iter().map(|_| vec![self.value; self.dim]).collect())
    }

    fn dimensions(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &str {
        "value"
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

fn vector_count(conn: &rusqlite::Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM conversation_turn_vectors",
        [],
        |row| row.get(0),
    )
    .expect("count vectors")
}

fn vector_blob(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn seed_vector(
    conn: &rusqlite::Connection,
    turn_id: &str,
    values: &[f32],
    fingerprint: &str,
    index_version: &str,
) {
    conn.execute(
        "INSERT INTO conversation_turn_vectors \
         (turn_id, chunk_index, vector, embedder_fingerprint, dim, index_version) \
         VALUES (?1, 0, ?2, ?3, ?4, ?5)",
        params![
            turn_id,
            vector_blob(values),
            fingerprint,
            values.len() as i64,
            index_version
        ],
    )
    .expect("seed vector");
}

fn make_raw_turn(session_id: &str, turn_index: u32, content: &str) -> RawTurn {
    RawTurn {
        session_id: session_id.to_string(),
        tool: Tool::Cc,
        role: Role::User,
        content: content.to_string(),
        timestamp_epoch: 1_000.0 + f64::from(turn_index),
        project_path: None,
        git_branch: None,
        is_csa_delegated: false,
        provenance: Provenance::Human,
        turn_index,
        metadata: TurnMetadata::default(),
    }
}

fn vector_metadata(conn: &rusqlite::Connection) -> HashMap<String, (String, i64, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT turn_id, embedder_fingerprint, dim, index_version \
             FROM conversation_turn_vectors \
             ORDER BY turn_id",
        )
        .expect("prepare metadata query");
    stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            (
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ),
        ))
    })
    .expect("query metadata")
    .collect::<Result<HashMap<_, _>, _>>()
    .expect("collect metadata")
}

fn vector_bytes(conn: &rusqlite::Connection, turn_id: &str) -> Vec<u8> {
    conn.query_row(
        "SELECT vector FROM conversation_turn_vectors WHERE turn_id = ?1 AND chunk_index = 0",
        params![turn_id],
        |row| row.get(0),
    )
    .expect("read vector bytes")
}

fn maybe_vector_bytes(conn: &rusqlite::Connection, turn_id: &str) -> Option<Vec<u8>> {
    conn.query_row(
        "SELECT vector FROM conversation_turn_vectors WHERE turn_id = ?1 AND chunk_index = 0",
        params![turn_id],
        |row| row.get(0),
    )
    .optional()
    .expect("read optional vector bytes")
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
    embed::embed_unindexed_turns(&db.inner, &embedder, None)
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
    embed::embed_unindexed_turns(&db.inner, &embedder, None)
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
    let stats1 = embed::embed_unindexed_turns(&db.inner, &embedder, None)
        .await
        .unwrap();
    assert_eq!(stats1.turns_processed, 1);

    // Second run: already indexed, should be a noop
    let stats2 = embed::embed_unindexed_turns(&db.inner, &embedder, None)
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
    let stats = embed::embed_unindexed_turns(&db.inner, &embedder, None)
        .await
        .unwrap();
    assert_eq!(stats.turns_processed, 2);
    assert_eq!(stats.chunks_total, 2);
    assert_eq!(stats.embedded, 2);
}

#[tokio::test]
async fn test_embed_batch_progress() {
    let db = open_temp_db_at_fork_ext(16);

    // 120 turns → 3 batches of 50 (EMBED_BATCH_SIZE=50): progress_fn fires 3 times.
    for i in 0..120usize {
        insert_raw_turn_row(
            db.conn(),
            TurnRow {
                id: &format!("turn-bp-{i:03}"),
                session: "sess-bp",
                tool: "cc",
                turn_index: i as i64,
                role: "user",
                content: &format!("batch progress test turn {i}"),
                timestamp: i as f64,
                token_count: None,
            },
        );
    }

    let call_count = std::sync::atomic::AtomicUsize::new(0);
    let embedder = MockEmbedder::new_fixed_dim(256);

    let stats = embed::embed_unindexed_turns(
        &db.inner,
        &embedder,
        Some(&|_done, _total| {
            call_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }),
    )
    .await
    .expect("embed_unindexed_turns should succeed");

    let calls = call_count.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        calls > 1,
        "expected multiple progress callbacks for 120 turns, got {calls}"
    );
    assert_eq!(
        stats.turns_processed, 120,
        "all 120 turns should be processed"
    );

    let indexed_count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(DISTINCT turn_id) FROM conversation_turn_vectors",
            [],
            |r| r.get(0),
        )
        .expect("count indexed turns");
    assert_eq!(indexed_count, 120, "all 120 turns should have vectors");
}

/// Batching proof (issue #258): chunks from many turns are merged into a few
/// sub-batch `embed()` calls — `ceil(total_chunks / EMBED_MAX_CHUNKS_PER_CALL)` —
/// NOT one call per turn. Uses several long turns whose combined chunk count
/// exceeds the per-call cap so the sub-batch split is exercised.
#[tokio::test]
async fn test_embed_batches_chunks_across_turns() {
    let db = open_temp_db_at_fork_ext(16);

    // 10 long turns sit in a single EMBED_BATCH_SIZE (50) window. Each ~30K
    // chars → ~25 chunks, so the window holds well over the 128 per-call cap.
    let n_turns = 10usize;
    let long_content = "word ".repeat(6000);
    for i in 0..n_turns {
        insert_raw_turn_row(
            db.conn(),
            TurnRow {
                id: &format!("turn-cb-{i:02}"),
                session: "sess-cb",
                tool: "cc",
                turn_index: i as i64,
                role: "assistant",
                content: &long_content,
                timestamp: i as f64,
                token_count: None,
            },
        );
    }

    let embedder = CountingEmbedder::new(64);
    let stats = embed::embed_unindexed_turns(&db.inner, &embedder, None)
        .await
        .expect("embed should succeed");

    let cap = embed::EMBED_MAX_CHUNKS_PER_CALL;
    let total_chunks = embedder.total_inputs();

    assert_eq!(stats.turns_processed, n_turns, "all turns processed");
    assert_eq!(
        total_chunks, stats.chunks_total,
        "embedder should see every produced chunk exactly once"
    );
    assert!(
        total_chunks > cap,
        "test must produce more than one sub-batch worth of chunks; got {total_chunks} (cap {cap})"
    );
    assert_eq!(
        embedder.calls(),
        total_chunks.div_ceil(cap),
        "embed() calls must equal ceil(total_chunks / cap), proving cross-turn merge"
    );
    assert!(
        embedder.calls() < n_turns,
        "must NOT be one embed() call per turn ({} calls for {n_turns} turns)",
        embedder.calls()
    );
    assert!(
        embedder.max_call_len() <= cap,
        "no single embed() call may exceed the per-call cap"
    );
}

/// Scope proof (issue #258): a scoped embed touches only the requested turns.
/// A pre-seeded unindexed turn from session A stays unindexed while a scoped
/// embed for session B's turn indexes only B.
#[tokio::test]
async fn test_embed_scope_isolates_other_sessions() {
    let db = open_temp_db_at_fork_ext(16);

    insert_raw_turn_row(
        db.conn(),
        TurnRow {
            id: "turn-A",
            session: "sessA",
            tool: "cc",
            turn_index: 0,
            role: "user",
            content: "alpha content from session A",
            timestamp: 1.0,
            token_count: None,
        },
    );
    insert_raw_turn_row(
        db.conn(),
        TurnRow {
            id: "turn-B",
            session: "sessB",
            tool: "cc",
            turn_index: 0,
            role: "user",
            content: "bravo content from session B",
            timestamp: 2.0,
            token_count: None,
        },
    );

    let embedder = MockEmbedder::new_fixed_dim(64);
    let scope = vec!["turn-B".to_string()];
    let stats = embed::embed_unindexed_turns_scoped(&db.inner, &embedder, &scope, None)
        .await
        .expect("scoped embed should succeed");
    assert_eq!(
        stats.turns_processed, 1,
        "only the scoped turn should be processed"
    );

    let a_vectors: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM conversation_turn_vectors WHERE turn_id=?",
            params!["turn-A"],
            |r| r.get(0),
        )
        .unwrap();
    let b_vectors: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM conversation_turn_vectors WHERE turn_id=?",
            params!["turn-B"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(a_vectors, 0, "session A turn must remain unindexed");
    assert!(b_vectors >= 1, "session B turn must be indexed");
}

/// Backlog drain proof (issue #258): the unscoped path that `xurl reindex`
/// wires to embeds every remaining unindexed turn.
#[tokio::test]
async fn test_reindex_path_drains_all_unindexed() {
    let db = open_temp_db_at_fork_ext(16);

    for i in 0..30usize {
        insert_raw_turn_row(
            db.conn(),
            TurnRow {
                id: &format!("turn-bl-{i:02}"),
                session: "sess-bl",
                tool: "cc",
                turn_index: i as i64,
                role: "user",
                content: &format!("backlog turn {i}"),
                timestamp: i as f64,
                token_count: None,
            },
        );
    }

    let before = mempal::xurl::store::count_unindexed_turns(db.conn()).unwrap();
    assert_eq!(before, 30, "all 30 turns start unindexed");

    let embedder = MockEmbedder::new_fixed_dim(32);
    let stats = embed::embed_unindexed_turns(&db.inner, &embedder, None)
        .await
        .expect("reindex drain should succeed");
    assert_eq!(stats.turns_processed, 30, "drain must process every turn");

    let after = mempal::xurl::store::count_unindexed_turns(db.conn()).unwrap();
    assert_eq!(after, 0, "reindex must drain the entire backlog");
}

#[tokio::test]
async fn test_xurl_reindex_stale_reembeds_only_stale_fingerprint_turns() {
    let db = open_temp_db_at_fork_ext(18);
    for (idx, turn_id) in ["turn-a1", "turn-a2", "turn-b1"].iter().enumerate() {
        insert_raw_turn_row(
            db.conn(),
            TurnRow {
                id: turn_id,
                session: "sess-stale",
                tool: "cc",
                turn_index: idx as i64,
                role: "user",
                content: &format!("stale switch turn {idx}"),
                timestamp: idx as f64,
                token_count: None,
            },
        );
    }
    seed_vector(
        db.conn(),
        "turn-a1",
        &[1.0, 1.0],
        "fp-a",
        CURRENT_VECTOR_INDEX_VERSION,
    );
    seed_vector(
        db.conn(),
        "turn-a2",
        &[1.0, 1.0],
        "fp-a",
        CURRENT_VECTOR_INDEX_VERSION,
    );
    seed_vector(
        db.conn(),
        "turn-b1",
        &[9.0, 9.0, 9.0],
        "fp-b",
        CURRENT_VECTOR_INDEX_VERSION,
    );
    let before_b = vector_bytes(db.conn(), "turn-b1");

    let embedder = ValueEmbedder::new(3, 2.0);
    let stats = embed::embed_stale_turns(&db.inner, &embedder, "fp-b", None)
        .await
        .expect("stale reindex should succeed");

    assert_eq!(stats.turns_processed, 2);
    assert_eq!(stats.embedded, 2);
    assert_eq!(
        embedder.total_inputs(),
        2,
        "only the stale A-fingerprint turns should be embedded"
    );
    let metadata = vector_metadata(db.conn());
    assert_eq!(
        metadata.get("turn-a1"),
        Some(&(
            "fp-b".to_string(),
            3,
            CURRENT_VECTOR_INDEX_VERSION.to_string()
        ))
    );
    assert_eq!(
        metadata.get("turn-a2"),
        Some(&(
            "fp-b".to_string(),
            3,
            CURRENT_VECTOR_INDEX_VERSION.to_string()
        ))
    );
    assert_eq!(
        metadata.get("turn-b1"),
        Some(&(
            "fp-b".to_string(),
            3,
            CURRENT_VECTOR_INDEX_VERSION.to_string()
        ))
    );
    assert_eq!(
        vector_bytes(db.conn(), "turn-b1"),
        before_b,
        "fresh B-fingerprint vector must not be rewritten by --stale"
    );
}

#[tokio::test]
async fn test_xurl_reindex_force_rebuilds_all_turn_vectors() {
    let db = open_temp_db_at_fork_ext(18);
    for (idx, turn_id) in ["turn-force-a", "turn-force-b", "turn-force-c"]
        .iter()
        .enumerate()
    {
        insert_raw_turn_row(
            db.conn(),
            TurnRow {
                id: turn_id,
                session: "sess-force",
                tool: "cc",
                turn_index: idx as i64,
                role: "user",
                content: &format!("force turn {idx}"),
                timestamp: idx as f64,
                token_count: None,
            },
        );
    }
    seed_vector(
        db.conn(),
        "turn-force-a",
        &[1.0, 1.0],
        "fp-a",
        CURRENT_VECTOR_INDEX_VERSION,
    );
    seed_vector(
        db.conn(),
        "turn-force-b",
        &[9.0, 9.0, 9.0],
        "fp-b",
        CURRENT_VECTOR_INDEX_VERSION,
    );

    let embedder = ValueEmbedder::new(3, 4.0);
    let stats = embed::embed_all_turns(&db.inner, &embedder, "fp-b", None)
        .await
        .expect("force reindex should succeed");

    assert_eq!(stats.turns_processed, 3);
    assert_eq!(stats.embedded, 3);
    assert_eq!(stats.skipped_stale_content, 0);
    assert_eq!(embedder.total_inputs(), 3);
    let metadata = vector_metadata(db.conn());
    for turn_id in ["turn-force-a", "turn-force-b", "turn-force-c"] {
        assert_eq!(
            metadata.get(turn_id),
            Some(&(
                "fp-b".to_string(),
                3,
                CURRENT_VECTOR_INDEX_VERSION.to_string()
            ))
        );
        assert_eq!(
            vector_bytes(db.conn(), turn_id),
            vector_blob(&[4.0, 4.0, 4.0])
        );
    }
}

#[tokio::test]
async fn force_reindex_skips_vector_write_when_content_changes_after_collection() {
    let db = open_temp_db_at_fork_ext(18);
    let changed = make_raw_turn("sess-race", 0, "old content");
    let changed_id = store::turn_id_for(&changed);
    let unchanged = make_raw_turn("sess-race", 1, "unchanged content");
    let unchanged_id = store::turn_id_for(&unchanged);
    store::insert_turns(db.conn(), &[changed, unchanged]).expect("seed turns");
    seed_vector(
        db.conn(),
        &changed_id,
        &[0.5, 0.5, 0.5],
        "fp-current",
        CURRENT_VECTOR_INDEX_VERSION,
    );
    seed_vector(
        db.conn(),
        &unchanged_id,
        &[0.5, 0.5, 0.5],
        "fp-current",
        CURRENT_VECTOR_INDEX_VERSION,
    );

    let updated = make_raw_turn("sess-race", 0, "new content");
    let embedder = UpdatingEmbedder::new(3, 1.0, 2.0, db.path.clone(), updated);
    let stats = embed::embed_all_turns(&db.inner, &embedder, "fp-current", None)
        .await
        .expect("force reindex should succeed");

    assert_eq!(stats.turns_processed, 2);
    assert_eq!(stats.skipped_stale_content, 1);
    assert_eq!(
        maybe_vector_bytes(db.conn(), &changed_id),
        None,
        "old-content vector write must be skipped after the turn content changes"
    );
    assert_eq!(
        vector_bytes(db.conn(), &unchanged_id),
        vector_blob(&[2.0, 2.0, 2.0]),
        "unchanged peer turn must still be rewritten normally"
    );
}

#[tokio::test]
async fn test_xurl_reindex_stale_noops_for_current_embedder_metadata() {
    let db = open_temp_db_at_fork_ext(18);
    insert_raw_turn_row(
        db.conn(),
        TurnRow {
            id: "turn-current",
            session: "sess-current",
            tool: "cc",
            turn_index: 0,
            role: "user",
            content: "already current",
            timestamp: 1.0,
            token_count: None,
        },
    );
    seed_vector(
        db.conn(),
        "turn-current",
        &[7.0, 7.0, 7.0],
        "fp-current",
        CURRENT_VECTOR_INDEX_VERSION,
    );

    let embedder = ValueEmbedder::new(3, 8.0);
    let stats = embed::embed_stale_turns(&db.inner, &embedder, "fp-current", None)
        .await
        .expect("stale reindex should succeed");

    assert_eq!(stats.turns_processed, 0);
    assert_eq!(stats.embedded, 0);
    assert_eq!(embedder.total_inputs(), 0);
    assert_eq!(
        vector_bytes(db.conn(), "turn-current"),
        vector_blob(&[7.0, 7.0, 7.0])
    );
}

#[tokio::test]
async fn content_update_invalidates_vectors_for_scoped_missing_embed() {
    let db = open_temp_db_at_fork_ext(18);
    let original = make_raw_turn("sess-update", 0, "old content");
    let turn_id = store::turn_id_for(&original);
    let insert_stats = store::insert_turns(db.conn(), std::slice::from_ref(&original))
        .expect("insert original turn");
    assert_eq!(insert_stats.inserted, 1);

    seed_vector(
        db.conn(),
        &turn_id,
        &[1.0, 1.0, 1.0],
        "fp-current",
        CURRENT_VECTOR_INDEX_VERSION,
    );
    let old_vector = vector_bytes(db.conn(), &turn_id);

    let updated = make_raw_turn("sess-update", 0, "new content");
    let update_stats =
        store::insert_turns(db.conn(), &[updated]).expect("update changed turn content");
    assert_eq!(update_stats.updated, 1);

    let embedder = ValueEmbedder::new(3, 2.0);
    let embed_stats = embed::embed_unindexed_turns_scoped_with_fingerprint(
        &db.inner,
        &embedder,
        std::slice::from_ref(&turn_id),
        "fp-current",
        None,
    )
    .await
    .expect("scoped missing embed should rebuild invalidated vector");

    assert_eq!(embed_stats.turns_processed, 1);
    assert_eq!(embed_stats.embedded, 1);
    assert_eq!(embedder.total_inputs(), 1);
    assert_ne!(
        vector_bytes(db.conn(), &turn_id),
        old_vector,
        "changed content must not retain the old-content vector"
    );
    assert_eq!(
        vector_bytes(db.conn(), &turn_id),
        vector_blob(&[2.0, 2.0, 2.0])
    );

    let stale_stats = embed::embed_stale_turns(&db.inner, &embedder, "fp-current", None)
        .await
        .expect("stale reindex should see the rebuilt vector as current");
    assert_eq!(stale_stats.turns_processed, 0);
}

#[tokio::test]
async fn identical_reingest_keeps_existing_vector_without_churn() {
    let db = open_temp_db_at_fork_ext(18);
    let turn = make_raw_turn("sess-no-churn", 0, "same content");
    let turn_id = store::turn_id_for(&turn);
    store::insert_turns(db.conn(), std::slice::from_ref(&turn)).expect("insert original turn");
    seed_vector(
        db.conn(),
        &turn_id,
        &[3.0, 3.0, 3.0],
        "fp-current",
        CURRENT_VECTOR_INDEX_VERSION,
    );
    let before = vector_bytes(db.conn(), &turn_id);

    let reingest_stats =
        store::insert_turns(db.conn(), std::slice::from_ref(&turn)).expect("reingest same turn");
    assert_eq!(reingest_stats.skipped, 1);

    let embedder = ValueEmbedder::new(3, 4.0);
    let embed_stats = embed::embed_unindexed_turns_scoped_with_fingerprint(
        &db.inner,
        &embedder,
        std::slice::from_ref(&turn_id),
        "fp-current",
        None,
    )
    .await
    .expect("scoped missing embed should skip unchanged vector");

    assert_eq!(embed_stats.turns_processed, 0);
    assert_eq!(embed_stats.embedded, 0);
    assert_eq!(embedder.total_inputs(), 0);
    assert_eq!(
        vector_bytes(db.conn(), &turn_id),
        before,
        "identical reingest must not delete or rebuild vectors"
    );
}

#[test]
fn test_xurl_reindex_dry_run_reports_without_embed_or_writes() {
    let home = TempDir::new().expect("home");
    let db = open_home_db(home.path());

    insert_raw_turn_row(
        db.conn(),
        TurnRow {
            id: "turn-dry-a",
            session: "thread-a",
            tool: "cc",
            turn_index: 0,
            role: "user",
            content: "dry run pending a",
            timestamp: 1.0,
            token_count: None,
        },
    );
    insert_raw_turn_row(
        db.conn(),
        TurnRow {
            id: "turn-dry-b",
            session: "thread-a",
            tool: "cc",
            turn_index: 1,
            role: "assistant",
            content: "dry run pending b",
            timestamp: 2.0,
            token_count: None,
        },
    );
    insert_raw_turn_row(
        db.conn(),
        TurnRow {
            id: "turn-dry-c",
            session: "thread-b",
            tool: "codex",
            turn_index: 0,
            role: "user",
            content: "dry run pending c",
            timestamp: 3.0,
            token_count: None,
        },
    );
    insert_raw_turn_row(
        db.conn(),
        TurnRow {
            id: "turn-indexed",
            session: "thread-indexed",
            tool: "cc",
            turn_index: 0,
            role: "user",
            content: "already indexed",
            timestamp: 4.0,
            token_count: None,
        },
    );
    db.conn()
        .execute(
            "INSERT INTO conversation_turn_vectors (turn_id, chunk_index, vector) VALUES (?1, 0, ?2)",
            params!["turn-indexed", vec![0_u8; 8]],
        )
        .expect("insert existing vector");

    let before_vectors = vector_count(db.conn());
    let output = run_xurl_reindex(home.path(), &["--dry-run", "--json"]);
    assert!(
        output.status.success(),
        "dry-run reindex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value =
        serde_json::from_slice(&output.stdout).expect("dry-run reindex json report");

    assert_eq!(report["dry_run"], Value::from(true));
    assert_eq!(report["threads_would_process"], Value::from(2));
    assert_eq!(report["turns_would_process"], Value::from(3));
    assert_eq!(
        vector_count(db.conn()),
        before_vectors,
        "dry-run must not write vectors"
    );
    assert_eq!(
        mempal::xurl::store::count_unindexed_turns(db.conn()).unwrap(),
        3,
        "dry-run must leave candidate turns unindexed"
    );
}
