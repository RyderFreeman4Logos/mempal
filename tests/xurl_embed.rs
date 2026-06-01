use mempal::core::db::Database;
use mempal::embed::Embedder;
use mempal::xurl::embed;
use rusqlite::params;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};
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
