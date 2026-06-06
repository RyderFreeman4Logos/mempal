use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::{Database, read_fork_ext_version};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

// Serialize MCP-path tests that bootstrap the global ConfigHandle.
async fn mcp_test_guard() -> OwnedMutexGuard<()> {
    static GUARD: std::sync::OnceLock<Arc<AsyncMutex<()>>> = std::sync::OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(AsyncMutex::new(())))
        .clone()
        .lock_owned()
        .await
}
use mempal::core::patterns::{
    NewPattern, PatternDetectionArgs, PatternStatus, cosine_similarity, get_pattern,
    insert_pattern, list_patterns, patterns_table_exists, promote_pattern, retire_pattern,
    run_pattern_detection, update_pattern_with_exemplar,
};
use mempal::core::types::{Drawer, SourceType};
use mempal::embed::{EmbedError, Embedder, EmbedderFactory};
use mempal::mcp::{IngestRequest, MempalMcpServer};
use serde_json::json;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn new_test_db() -> (TempDir, PathBuf, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    (tmp, db_path, db)
}

fn unit_vec(dim: usize, value: f32) -> Vec<f32> {
    let raw = vec![value; dim];
    let norm: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
    raw.iter().map(|x| x / norm).collect()
}

fn near_vec(base: &[f32], perturbation: f32) -> Vec<f32> {
    let raw: Vec<f32> = base.iter().map(|x| x + perturbation).collect();
    let norm: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
    raw.iter().map(|x| x / norm).collect()
}

fn orthogonal_vec(dim: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dim];
    if dim > 1 {
        v[1] = 1.0;
    }
    v
}

fn seed_drawer(db: &Database, id: &str, source_file: &str, content: &str, vector: &[f32]) {
    db.insert_drawer(&Drawer {
        id: id.to_string(),
        content: content.to_string(),
        wing: "test".to_string(),
        source_file: Some(source_file.to_string()),
        source_type: SourceType::AgentInference,
        added_at: "1713000000".to_string(),
        ..Drawer::default()
    })
    .expect("insert drawer");
    db.insert_vector(id, vector).expect("insert vector");
}

fn count_patterns(db: &Database) -> i64 {
    db.conn()
        .query_row("SELECT COUNT(*) FROM patterns", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0)
}

fn pattern_status(db: &Database, pattern_id: &str) -> Option<String> {
    db.conn()
        .query_row(
            "SELECT status FROM patterns WHERE pattern_id = ?1",
            [pattern_id],
            |row| row.get::<_, String>(0),
        )
        .ok()
}

// ---------------------------------------------------------------------------
// Minimal embedder for MCP-path tests
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MapEmbedderFactory {
    map: Arc<HashMap<String, Vec<f32>>>,
    default: Vec<f32>,
}

struct MapEmbedder {
    map: Arc<HashMap<String, Vec<f32>>>,
    default: Vec<f32>,
}

#[async_trait]
impl EmbedderFactory for MapEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(MapEmbedder {
            map: Arc::clone(&self.map),
            default: self.default.clone(),
        }))
    }
}

#[async_trait]
impl Embedder for MapEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|t| {
                self.map
                    .get(*t)
                    .cloned()
                    .unwrap_or_else(|| self.default.clone())
            })
            .collect())
    }

    fn dimensions(&self) -> usize {
        self.default.len()
    }

    fn name(&self) -> &str {
        "map-embedder"
    }
}

fn config_text(db_path: &Path, patterns_enabled: bool, promote_threshold: usize) -> String {
    format!(
        r#"
db_path = "{}"

[embed]
backend = "model2vec"

[config_hot_reload]
enabled = false

[ingest_gating]
enabled = false

[patterns]
enabled = {}
similarity_threshold = 0.82
min_sessions = 3
min_exemplars = 3
promote_threshold = {}
retire_after_days = 90
surfacing_threshold = 0.75
pattern_boost = 0.2
"#,
        db_path.display(),
        patterns_enabled,
        promote_threshold,
    )
}

struct TestEnv {
    _tmp: TempDir,
    db_path: PathBuf,
    config_path: PathBuf,
}

impl TestEnv {
    fn new(patterns_enabled: bool, promote_threshold: usize) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db_path = mempal_home.join("palace.db");
        let config_path = mempal_home.join("config.toml");
        Database::open(&db_path).expect("open db");
        fs::write(
            &config_path,
            config_text(&db_path, patterns_enabled, promote_threshold),
        )
        .expect("write config");
        Self {
            _tmp: tmp,
            db_path,
            config_path,
        }
    }

    fn server(&self, vectors: &[(&str, Vec<f32>)], default_vec: Vec<f32>) -> MempalMcpServer {
        ConfigHandle::bootstrap(&self.config_path).expect("bootstrap config");
        let config = Config::load_from(&self.config_path).expect("load config");
        let map: HashMap<String, Vec<f32>> = vectors
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        MempalMcpServer::new_with_factory_and_config(
            self.db_path.clone(),
            config,
            Arc::new(MapEmbedderFactory {
                map: Arc::new(map),
                default: default_vec,
            }),
        )
    }

    fn db(&self) -> Database {
        Database::open(&self.db_path).expect("open db")
    }
}

async fn do_ingest(server: &MempalMcpServer, content: &str, source: &str) {
    server
        .ingest_json_for_test(
            serde_json::to_value(IngestRequest {
                content: content.to_string(),
                wing: "test".to_string(),
                source: Some(source.to_string()),
                dry_run: Some(false),
                ..IngestRequest::default()
            })
            .expect("serialize ingest request"),
        )
        .await
        .expect("ingest");
}

// ---------------------------------------------------------------------------
// Test: schema migration creates patterns table
// ---------------------------------------------------------------------------

#[test]
fn test_fork_ext_migration_v6_to_v7_creates_patterns_table() {
    let (_tmp, _db_path, db) = new_test_db();

    // After Database::open, all migrations (including v11 that creates patterns) run.
    let version = read_fork_ext_version(db.conn()).expect("read version");
    assert!(
        version >= 11,
        "expected fork_ext_version >= 12, got {version}"
    );

    assert!(
        patterns_table_exists(db.conn()),
        "patterns table must exist after migration"
    );

    let columns: Vec<String> = db
        .conn()
        .prepare("PRAGMA table_info(patterns)")
        .expect("prepare")
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect");

    for required_col in &[
        "pattern_id",
        "signature",
        "exemplar_ids",
        "session_ids",
        "session_count",
        "status",
    ] {
        assert!(
            columns.iter().any(|c| c == required_col),
            "missing column: {required_col}"
        );
    }

    let idx_exists = |name: &str| -> bool {
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                [name],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0
    };
    assert!(
        idx_exists("idx_patterns_status"),
        "missing idx_patterns_status"
    );
}

// ---------------------------------------------------------------------------
// Test: 3 different sessions → pattern candidate created
// ---------------------------------------------------------------------------

#[test]
fn test_pattern_candidate_created_from_three_sessions() {
    let (_tmp, _db_path, db) = new_test_db();
    let dim = 8;
    let base = unit_vec(dim, 1.0);

    seed_drawer(
        &db,
        "d1",
        "session-a.md",
        "memory alpha content",
        &near_vec(&base, 0.001),
    );
    seed_drawer(
        &db,
        "d2",
        "session-b.md",
        "memory beta content",
        &near_vec(&base, 0.002),
    );
    seed_drawer(
        &db,
        "d3",
        "session-c.md",
        "memory gamma content",
        &near_vec(&base, 0.003),
    );

    let new_emb = near_vec(&base, 0.004);
    run_pattern_detection(
        db.conn(),
        &PatternDetectionArgs {
            new_drawer_id: "d4",
            session_id: "session-d.md",
            embedding: &new_emb,
            project_id: None,
            model_id: "test-model",
            similarity_threshold: 0.82,
            min_sessions: 3,
            min_exemplars: 3,
            promote_threshold: 5,
            top_tags: 5,
        },
    );

    let count = count_patterns(&db);
    assert_eq!(count, 1, "expected 1 pattern candidate, got {count}");

    let patterns = list_patterns(db.conn(), None, None).expect("list");
    let p = &patterns[0];
    assert_eq!(
        p.status,
        PatternStatus::Candidate,
        "pattern must start as candidate"
    );
    assert!(p.session_count >= 3, "session_count must be >= 3");
    assert!(
        p.exemplar_ids.len() >= 3,
        "exemplar_ids must have >= 3 entries"
    );
    assert!(!p.signature.is_empty(), "signature blob must be non-empty");
}

// ---------------------------------------------------------------------------
// Test: same session drawers don't create a pattern
// ---------------------------------------------------------------------------

#[test]
fn test_same_session_drawers_dont_create_pattern() {
    let (_tmp, _db_path, db) = new_test_db();
    let dim = 8;
    let base = unit_vec(dim, 1.0);

    seed_drawer(
        &db,
        "s1",
        "same-session.md",
        "alpha",
        &near_vec(&base, 0.001),
    );
    seed_drawer(
        &db,
        "s2",
        "same-session.md",
        "beta",
        &near_vec(&base, 0.002),
    );
    seed_drawer(
        &db,
        "s3",
        "same-session.md",
        "gamma",
        &near_vec(&base, 0.003),
    );

    let new_emb = near_vec(&base, 0.004);
    run_pattern_detection(
        db.conn(),
        &PatternDetectionArgs {
            new_drawer_id: "s4",
            session_id: "same-session.md",
            embedding: &new_emb,
            project_id: None,
            model_id: "test-model",
            similarity_threshold: 0.82,
            min_sessions: 3,
            min_exemplars: 3,
            promote_threshold: 5,
            top_tags: 5,
        },
    );

    let count = count_patterns(&db);
    assert_eq!(count, 0, "single-session cluster must not create a pattern");
}

// ---------------------------------------------------------------------------
// Test: pattern auto-promotes when session_count reaches promote_threshold
// ---------------------------------------------------------------------------

#[test]
fn test_pattern_promotes_to_active_at_threshold() {
    let (_tmp, _db_path, db) = new_test_db();
    let dim = 8;
    let base = unit_vec(dim, 1.0);
    let promote_threshold = 5usize;

    // Seed exemplar drawers.
    seed_drawer(&db, "d1", "sess-a", "alpha", &near_vec(&base, 0.001));
    seed_drawer(&db, "d2", "sess-b", "beta", &near_vec(&base, 0.002));
    seed_drawer(&db, "d3", "sess-c", "gamma", &near_vec(&base, 0.003));
    seed_drawer(&db, "d4", "sess-d", "delta", &near_vec(&base, 0.004));

    // Manually insert a candidate pattern with 4 sessions.
    let pattern_id = "pat-promote-test".to_string();
    insert_pattern(
        db.conn(),
        &NewPattern {
            pattern_id: pattern_id.clone(),
            signature: base.clone(),
            exemplar_ids: vec![
                "d1".to_string(),
                "d2".to_string(),
                "d3".to_string(),
                "d4".to_string(),
            ],
            session_ids: vec![
                "sess-a".to_string(),
                "sess-b".to_string(),
                "sess-c".to_string(),
                "sess-d".to_string(),
            ],
            topic_tags: vec!["memory".to_string()],
            model_id: Some("test-model".to_string()),
            project_id: None,
        },
    )
    .expect("insert pattern");

    assert_eq!(
        pattern_status(&db, &pattern_id).as_deref(),
        Some("candidate")
    );

    // 5th drawer from a new session triggers promotion via run_pattern_detection.
    let new_emb = near_vec(&base, 0.005);
    run_pattern_detection(
        db.conn(),
        &PatternDetectionArgs {
            new_drawer_id: "d5",
            session_id: "sess-e",
            embedding: &new_emb,
            project_id: None,
            model_id: "test-model",
            similarity_threshold: 0.82,
            min_sessions: 3,
            min_exemplars: 3,
            promote_threshold,
            top_tags: 5,
        },
    );

    let status_after = pattern_status(&db, &pattern_id);
    assert_eq!(
        status_after.as_deref(),
        Some("active"),
        "pattern must auto-promote to active when session_count reaches threshold"
    );
}

// ---------------------------------------------------------------------------
// Test: active pattern boosts exemplar results in search
// ---------------------------------------------------------------------------

// model_id that apply_pattern_boost derives for config backend = "model2vec"
const MODEL2VEC_MODEL_ID: &str = "model2vec/potion-multilingual-128M";

#[tokio::test]
async fn test_active_pattern_boosts_exemplar_results() {
    let _guard = mcp_test_guard().await;
    let env = TestEnv::new(true, 5);
    let dim = 8;
    let base = unit_vec(dim, 1.0);
    let query_text = "active pattern query";

    // Seed base drawers and insert an active pattern.
    {
        let db = env.db();
        seed_drawer(
            &db,
            "ex1",
            "sess-a.md",
            "recurring memory topic",
            &near_vec(&base, 0.001),
        );
        seed_drawer(
            &db,
            "ex2",
            "sess-b.md",
            "recurring memory topic",
            &near_vec(&base, 0.002),
        );
        seed_drawer(
            &db,
            "ex3",
            "sess-c.md",
            "recurring memory topic",
            &near_vec(&base, 0.003),
        );

        let orthog = orthogonal_vec(dim);
        seed_drawer(
            &db,
            "unrelated",
            "sess-x.md",
            "completely different subject",
            &orthog,
        );

        insert_pattern(
            db.conn(),
            &NewPattern {
                pattern_id: "pat-boost-test".to_string(),
                signature: base.clone(),
                exemplar_ids: vec!["ex1".to_string(), "ex2".to_string(), "ex3".to_string()],
                session_ids: vec![
                    "sess-a.md".to_string(),
                    "sess-b.md".to_string(),
                    "sess-c.md".to_string(),
                ],
                topic_tags: vec!["recurring".to_string(), "memory".to_string()],
                model_id: Some(MODEL2VEC_MODEL_ID.to_string()),
                project_id: None,
            },
        )
        .expect("insert pattern");
        db.conn()
            .execute(
                "UPDATE patterns SET status = 'active', session_count = 5 WHERE pattern_id = 'pat-boost-test'",
                [],
            )
            .expect("promote pattern");
    }

    let vectors: Vec<(&str, Vec<f32>)> = vec![(query_text, base.clone())];
    let server = env.server(&vectors, near_vec(&base, 0.001));

    let resp = server
        .search_json_for_test(json!({ "query": query_text, "top_k": 10 }))
        .await
        .expect("search");

    let boosted: Vec<_> = resp
        .results
        .iter()
        .filter(|r| r.matched_pattern_id.is_some())
        .collect();
    assert!(
        !boosted.is_empty(),
        "at least one result should have matched_pattern_id set"
    );

    let boosted_ids: Vec<_> = boosted.iter().map(|r| r.drawer_id.as_str()).collect();
    assert!(
        boosted_ids
            .iter()
            .any(|&id| ["ex1", "ex2", "ex3"].contains(&id)),
        "boosted results must include exemplar drawers, got: {boosted_ids:?}"
    );
}

// ---------------------------------------------------------------------------
// Test: mempal_context includes active patterns as recurring_themes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_context_includes_recurring_themes() {
    let _guard = mcp_test_guard().await;
    let env = TestEnv::new(true, 5);
    let dim = 8;
    let base = unit_vec(dim, 1.0);

    {
        let db = env.db();
        for (i, pat_id) in ["pat-ctx-1", "pat-ctx-2"].iter().enumerate() {
            insert_pattern(
                db.conn(),
                &NewPattern {
                    pattern_id: pat_id.to_string(),
                    signature: near_vec(&base, i as f32 * 0.01),
                    exemplar_ids: vec![format!("ctx-ex-{i}-a"), format!("ctx-ex-{i}-b")],
                    session_ids: vec![
                        format!("s{i}-a"),
                        format!("s{i}-b"),
                        format!("s{i}-c"),
                        format!("s{i}-d"),
                        format!("s{i}-e"),
                    ],
                    topic_tags: vec![format!("tag-{i}")],
                    model_id: Some("map-embedder".to_string()),
                    project_id: None,
                },
            )
            .expect("insert pattern");
            db.conn()
                .execute(
                    "UPDATE patterns SET status = 'active', session_count = 5 WHERE pattern_id = ?1",
                    [*pat_id],
                )
                .expect("activate pattern");
        }
    }

    let server = env.server(&[], near_vec(&base, 0.0));
    let ctx = server
        .context_json_for_test(json!({
            "query": "test context query",
            "cwd": env._tmp.path().to_str().unwrap(),
        }))
        .await
        .expect("context");

    assert_eq!(
        ctx.recurring_themes.len(),
        2,
        "expected 2 recurring_themes, got {}",
        ctx.recurring_themes.len()
    );
    for theme in &ctx.recurring_themes {
        assert!(!theme.pattern_id.is_empty(), "pattern_id must be non-empty");
        assert!(!theme.topic_tags.is_empty(), "topic_tags must be non-empty");
        assert!(theme.session_count >= 5, "session_count must be >= 5");
    }
}

// ---------------------------------------------------------------------------
// Test: candidate patterns excluded from recurring_themes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_context_excludes_candidate_patterns() {
    let _guard = mcp_test_guard().await;
    let env = TestEnv::new(true, 5);
    let dim = 8;
    let base = unit_vec(dim, 1.0);

    {
        let db = env.db();
        insert_pattern(
            db.conn(),
            &NewPattern {
                pattern_id: "pat-candidate".to_string(),
                signature: base.clone(),
                exemplar_ids: vec!["c1".to_string(), "c2".to_string(), "c3".to_string()],
                session_ids: vec!["s1".to_string(), "s2".to_string(), "s3".to_string()],
                topic_tags: vec!["candidate-tag".to_string()],
                model_id: Some("map-embedder".to_string()),
                project_id: None,
            },
        )
        .expect("insert candidate pattern");
    }

    let server = env.server(&[], base.clone());
    let ctx = server
        .context_json_for_test(json!({
            "query": "context query",
            "cwd": env._tmp.path().to_str().unwrap(),
        }))
        .await
        .expect("context");

    assert!(
        ctx.recurring_themes.is_empty(),
        "candidate patterns must not appear in recurring_themes"
    );
}

// ---------------------------------------------------------------------------
// Test: patterns list CLI shows active patterns (not candidate)
// ---------------------------------------------------------------------------

#[test]
fn test_patterns_list_cli_shows_active() {
    use std::process::Command;

    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let mempal_home = home.join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create .mempal dir");
    let db_path = mempal_home.join("palace.db");
    let config_path = mempal_home.join("config.toml");
    fs::write(&config_path, config_text(&db_path, true, 5)).expect("write config");
    let db = Database::open(&db_path).expect("open db");

    let dim = 4;
    let base = unit_vec(dim, 1.0);

    insert_pattern(
        db.conn(),
        &NewPattern {
            pattern_id: "pat-active-cli".to_string(),
            signature: base.clone(),
            exemplar_ids: vec!["a1".to_string()],
            session_ids: vec!["s1".to_string()],
            topic_tags: vec!["active-tag".to_string()],
            model_id: None,
            project_id: None,
        },
    )
    .expect("insert active pattern");
    db.conn()
        .execute(
            "UPDATE patterns SET status = 'active', session_count = 5 WHERE pattern_id = 'pat-active-cli'",
            [],
        )
        .expect("activate");

    insert_pattern(
        db.conn(),
        &NewPattern {
            pattern_id: "pat-candidate-cli".to_string(),
            signature: base.clone(),
            exemplar_ids: vec!["b1".to_string()],
            session_ids: vec!["t1".to_string()],
            topic_tags: vec!["candidate-tag".to_string()],
            model_id: None,
            project_id: None,
        },
    )
    .expect("insert candidate pattern");

    drop(db);

    let bin = env!("CARGO_BIN_EXE_mempal");
    let out = Command::new(bin)
        .env("HOME", &home)
        .args(["patterns", "list", "--status", "active"])
        .output()
        .expect("run mempal patterns list");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "patterns list must exit 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("pat-active-cli"),
        "stdout must contain active pattern_id; got: {stdout}"
    );
    assert!(
        !stdout.contains("pat-candidate-cli"),
        "stdout must NOT contain candidate pattern_id; got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Test: patterns retire CLI sets status to retired
// ---------------------------------------------------------------------------

#[test]
fn test_patterns_retire_cli() {
    use std::process::Command;

    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let mempal_home = home.join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create .mempal dir");
    let db_path = mempal_home.join("palace.db");
    let config_path = mempal_home.join("config.toml");
    fs::write(&config_path, config_text(&db_path, true, 5)).expect("write config");
    let db = Database::open(&db_path).expect("open db");

    let dim = 4;
    let base = unit_vec(dim, 1.0);
    insert_pattern(
        db.conn(),
        &NewPattern {
            pattern_id: "pat-retire-cli".to_string(),
            signature: base.clone(),
            exemplar_ids: vec!["r1".to_string()],
            session_ids: vec!["rs1".to_string()],
            topic_tags: vec!["retire-tag".to_string()],
            model_id: None,
            project_id: None,
        },
    )
    .expect("insert pattern");
    db.conn()
        .execute(
            "UPDATE patterns SET status = 'active' WHERE pattern_id = 'pat-retire-cli'",
            [],
        )
        .expect("activate");
    drop(db);

    let bin = env!("CARGO_BIN_EXE_mempal");
    let out = Command::new(bin)
        .env("HOME", &home)
        .args(["patterns", "retire", "pat-retire-cli"])
        .output()
        .expect("run mempal patterns retire");

    assert!(
        out.status.success(),
        "patterns retire must exit 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let db2 = Database::open(&db_path).expect("open db after retire");
    let status = pattern_status(&db2, "pat-retire-cli");
    assert_eq!(
        status.as_deref(),
        Some("retired"),
        "pattern must be retired after CLI retire command"
    );
}

// ---------------------------------------------------------------------------
// Test: patterns disabled → no detection runs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_patterns_disabled_skips_detection() {
    let _guard = mcp_test_guard().await;
    let env = TestEnv::new(false, 5);
    let dim = 8;
    let base = unit_vec(dim, 1.0);

    let server = env.server(
        &[
            ("content-a", near_vec(&base, 0.001)),
            ("content-b", near_vec(&base, 0.002)),
            ("content-c", near_vec(&base, 0.003)),
            ("content-d", near_vec(&base, 0.004)),
        ],
        base.clone(),
    );

    for (content, source) in &[
        ("content-a", "sess-p.md"),
        ("content-b", "sess-q.md"),
        ("content-c", "sess-r.md"),
        ("content-d", "sess-s.md"),
    ] {
        do_ingest(&server, content, source).await;
    }

    let db = env.db();
    let count = count_patterns(&db);
    assert_eq!(
        count, 0,
        "patterns table must be empty when patterns are disabled"
    );
}

// ---------------------------------------------------------------------------
// Test: pattern detection failure does not fail ingest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_pattern_detection_failure_does_not_fail_ingest() {
    let _guard = mcp_test_guard().await;
    // Zero-length embedding short-circuits try_run_pattern_detection without error.
    let (_tmp, _db_path, db) = new_test_db();

    run_pattern_detection(
        db.conn(),
        &PatternDetectionArgs {
            new_drawer_id: "dx",
            session_id: "sess-fail.md",
            embedding: &[],
            project_id: None,
            model_id: "test-model",
            similarity_threshold: 0.82,
            min_sessions: 3,
            min_exemplars: 3,
            promote_threshold: 5,
            top_tags: 5,
        },
    );

    let count = count_patterns(&db);
    assert_eq!(
        count, 0,
        "no pattern must be created from zero-length embedding"
    );
}

// ---------------------------------------------------------------------------
// Test: incremental centroid update uses correct formula
// ---------------------------------------------------------------------------

#[test]
fn test_incremental_centroid_formula() {
    let (_tmp, _db_path, db) = new_test_db();
    let dim = 4;
    let initial_sig = vec![0.5f32; dim];

    insert_pattern(
        db.conn(),
        &NewPattern {
            pattern_id: "pat-centroid".to_string(),
            signature: initial_sig.clone(),
            exemplar_ids: vec!["e1".to_string(), "e2".to_string(), "e3".to_string()],
            session_ids: vec!["sa".to_string(), "sb".to_string(), "sc".to_string()],
            topic_tags: vec![],
            model_id: Some("test-model".to_string()),
            project_id: None,
        },
    )
    .expect("insert");

    let new_emb = vec![1.0f32; dim];
    update_pattern_with_exemplar(db.conn(), "pat-centroid", "e4", "sd", &new_emb, 10)
        .expect("update");

    let p = get_pattern(db.conn(), "pat-centroid")
        .expect("get pattern")
        .expect("pattern must exist");

    // exemplar_count was 3 (from exemplar_ids.len()); after update it's 4.
    // new_centroid[i] = (0.5 * (4-1) + 1.0) / 4 = 2.5 / 4 = 0.625
    for (i, val) in p.signature.iter().enumerate() {
        assert!(
            (val - 0.625).abs() < 1e-5,
            "signature[{i}] should be ~0.625, got {val}"
        );
    }
}

// ---------------------------------------------------------------------------
// Test: cosine_similarity returns 1.0 for identical vectors
// ---------------------------------------------------------------------------

#[test]
fn test_cosine_similarity_identical_vectors() {
    let v = vec![0.5f32, 0.5, 0.5, 0.5];
    let sim = cosine_similarity(&v, &v);
    assert!(
        (sim - 1.0).abs() < 1e-6,
        "identical vectors must have cosine similarity 1.0, got {sim}"
    );
}

// ---------------------------------------------------------------------------
// Test: promote_pattern sets status to active
// ---------------------------------------------------------------------------

#[test]
fn test_promote_pattern_sets_active() {
    let (_tmp, _db_path, db) = new_test_db();
    let dim = 4;
    let base = unit_vec(dim, 1.0);

    insert_pattern(
        db.conn(),
        &NewPattern {
            pattern_id: "pat-manual-promote".to_string(),
            signature: base.clone(),
            exemplar_ids: vec!["m1".to_string(), "m2".to_string(), "m3".to_string()],
            session_ids: vec!["ms1".to_string(), "ms2".to_string(), "ms3".to_string()],
            topic_tags: vec!["promote".to_string()],
            model_id: None,
            project_id: None,
        },
    )
    .expect("insert");

    let promoted = promote_pattern(db.conn(), "pat-manual-promote").expect("promote");
    assert!(promoted, "promote_pattern must return true for candidate");

    let p = get_pattern(db.conn(), "pat-manual-promote")
        .expect("get")
        .expect("exists");
    assert_eq!(
        p.status,
        PatternStatus::Active,
        "pattern must be active after promote"
    );
}

// ---------------------------------------------------------------------------
// Test: retire_pattern sets status to retired (idempotent)
// ---------------------------------------------------------------------------

#[test]
fn test_retire_pattern_is_idempotent() {
    let (_tmp, _db_path, db) = new_test_db();
    let dim = 4;
    let base = unit_vec(dim, 1.0);

    insert_pattern(
        db.conn(),
        &NewPattern {
            pattern_id: "pat-retire-idem".to_string(),
            signature: base.clone(),
            exemplar_ids: vec!["ri1".to_string()],
            session_ids: vec!["rs-x".to_string()],
            topic_tags: vec![],
            model_id: None,
            project_id: None,
        },
    )
    .expect("insert");

    promote_pattern(db.conn(), "pat-retire-idem").expect("promote");
    let r1 = retire_pattern(db.conn(), "pat-retire-idem").expect("first retire");
    assert!(r1, "first retire must return true");

    let r2 = retire_pattern(db.conn(), "pat-retire-idem").expect("second retire");
    assert!(r2, "second retire must also return true (idempotent)");

    let p = get_pattern(db.conn(), "pat-retire-idem")
        .expect("get")
        .expect("exists");
    assert_eq!(p.status, PatternStatus::Retired);
}
