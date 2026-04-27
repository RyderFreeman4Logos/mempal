use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::{
    Database, FORK_EXT_META_DDL, FORK_EXT_V1_SCHEMA_SQL, FORK_EXT_V2_SCHEMA_SQL,
    FORK_EXT_V3_SCHEMA_SQL, apply_fork_ext_migrations_to, read_fork_ext_version,
    set_fork_ext_version,
};
use mempal::core::types::{Drawer, SourceType, Triple};
use mempal::embed::{EmbedError, Embedder, EmbedderFactory};
use mempal::mcp::{IngestRequest, IngestResponse, MempalMcpServer};
use rmcp::handler::server::wrapper::Parameters;
use rusqlite::{Connection, OptionalExtension, params};
use tempfile::TempDir;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const DEFAULT_ROOM: &str = "novelty";
const DEFAULT_WING: &str = "code-memory";

async fn test_guard() -> OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(AsyncMutex::new(())))
        .clone()
        .lock_owned()
        .await
}

#[derive(Clone)]
struct DeterministicEmbedderFactory {
    vectors: Arc<HashMap<String, Vec<f32>>>,
    default_vector: Vec<f32>,
    fail_contains: Arc<Vec<String>>,
}

struct DeterministicEmbedder {
    vectors: Arc<HashMap<String, Vec<f32>>>,
    default_vector: Vec<f32>,
    fail_contains: Arc<Vec<String>>,
}

#[async_trait]
impl EmbedderFactory for DeterministicEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(DeterministicEmbedder {
            vectors: Arc::clone(&self.vectors),
            default_vector: self.default_vector.clone(),
            fail_contains: Arc::clone(&self.fail_contains),
        }))
    }
}

#[async_trait]
impl Embedder for DeterministicEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if let Some(text) = texts.iter().find(|text| {
            self.fail_contains
                .iter()
                .any(|needle| text.contains(needle.as_str()))
        }) {
            return Err(EmbedError::Runtime(format!(
                "forced failure for label:{}",
                redact_label(text)
            )));
        }

        Ok(texts
            .iter()
            .map(|text| {
                self.vectors
                    .get(*text)
                    .cloned()
                    .unwrap_or_else(|| self.default_vector.clone())
            })
            .collect())
    }

    fn dimensions(&self) -> usize {
        self.default_vector.len()
    }

    fn name(&self) -> &str {
        "deterministic"
    }
}

struct TestEnv {
    _tmp: TempDir,
    db_path: PathBuf,
    config_path: PathBuf,
}

#[derive(Clone, Copy)]
struct DrawerSeed<'a> {
    id: &'a str,
    content: &'a str,
    wing: &'a str,
    room: &'a str,
    project_id: Option<&'a str>,
}

#[derive(Debug)]
struct NoveltyAuditRow {
    decision: String,
    near_drawer_id: Option<String>,
    project_id: Option<String>,
}

#[derive(Clone)]
struct LogCapture {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer
            .lock()
            .expect("log buffer mutex poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl TestEnv {
    fn new(novelty_enabled: bool) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db_path = mempal_home.join("palace.db");
        let config_path = mempal_home.join("config.toml");
        Database::open(&db_path).expect("open db");
        fs::write(&config_path, config_text(&db_path, novelty_enabled)).expect("write config");
        Self {
            _tmp: tmp,
            db_path,
            config_path,
        }
    }

    fn db(&self) -> Database {
        Database::open(&self.db_path).expect("open db")
    }

    fn server(&self, vectors: &[(&str, Vec<f32>)], fail_contains: &[&str]) -> MempalMcpServer {
        ConfigHandle::bootstrap(&self.config_path).expect("bootstrap config");
        let config = Config::load_from(&self.config_path).expect("load config");
        MempalMcpServer::new_with_factory_and_config(
            self.db_path.clone(),
            config,
            Arc::new(DeterministicEmbedderFactory {
                vectors: Arc::new(
                    vectors
                        .iter()
                        .map(|(text, vector)| ((*text).to_string(), vector.clone()))
                        .collect(),
                ),
                default_vector: vec![0.1, 0.2],
                fail_contains: Arc::new(
                    fail_contains
                        .iter()
                        .map(|needle| (*needle).to_string())
                        .collect(),
                ),
            }),
        )
    }
}

fn config_text(db_path: &Path, novelty_enabled: bool) -> String {
    format!(
        r#"
db_path = "{}"

[config_hot_reload]
enabled = false

[ingest_gating]
enabled = false

[ingest_gating.novelty]
enabled = {}
duplicate_threshold = 0.95
merge_threshold = 0.85
wing_scope = "same_wing"
top_k_candidates = 5
max_merges_per_drawer = 10
max_content_bytes_per_drawer = 65536
"#,
        db_path.display(),
        novelty_enabled
    )
}

fn insert_drawer(db_path: &Path, seed: DrawerSeed<'_>, vector: &[f32]) {
    let db = Database::open(db_path).expect("open db");
    db.insert_drawer_with_project(
        &Drawer {
            id: seed.id.to_string(),
            content: seed.content.to_string(),
            wing: seed.wing.to_string(),
            room: Some(seed.room.to_string()),
            source_file: Some(format!("{}.md", seed.id)),
            source_type: SourceType::Manual,
            added_at: "1713000000".to_string(),
            chunk_index: Some(0),
            importance: 0,
            ..Drawer::default()
        },
        seed.project_id,
    )
    .expect("insert drawer");
    db.insert_vector_with_project(seed.id, vector, seed.project_id)
        .expect("insert vector");
}

async fn ingest(
    server: &MempalMcpServer,
    wing: &str,
    room: &str,
    content: &str,
    project_id: Option<&str>,
) -> IngestResponse {
    server
        .mempal_ingest(Parameters(IngestRequest {
            content: content.to_string(),
            wing: wing.to_string(),
            room: Some(room.to_string()),
            project_id: project_id.map(ToOwned::to_owned),
            dry_run: Some(false),
            ..IngestRequest::default()
        }))
        .await
        .expect("ingest response")
        .0
}

fn novelty_rows(db_path: &Path) -> Vec<NoveltyAuditRow> {
    let conn = Connection::open(db_path).expect("open sqlite");
    let mut stmt = conn
        .prepare(
            r#"
            SELECT decision, near_drawer_id, project_id
            FROM novelty_audit
            ORDER BY created_at ASC, id ASC
            "#,
        )
        .expect("prepare novelty rows");
    stmt.query_map([], |row| {
        Ok(NoveltyAuditRow {
            decision: row.get::<_, String>(0)?,
            near_drawer_id: row.get::<_, Option<String>>(1)?,
            project_id: row.get::<_, Option<String>>(2)?,
        })
    })
    .expect("query novelty rows")
    .collect::<Result<Vec<_>, _>>()
    .expect("collect novelty rows")
}

fn drawer_count(db_path: &Path) -> i64 {
    Database::open(db_path)
        .expect("open db")
        .drawer_count()
        .expect("drawer count")
}

fn drawer_content(db_path: &Path, drawer_id: &str) -> String {
    Connection::open(db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT content FROM drawers WHERE id = ?1",
            [drawer_id],
            |row| row.get::<_, String>(0),
        )
        .expect("drawer content")
}

fn merge_state(db_path: &Path, drawer_id: &str) -> (i64, Option<String>) {
    Connection::open(db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT merge_count, updated_at FROM drawers WHERE id = ?1",
            [drawer_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .expect("merge state")
}

fn fts_search_ids(
    db_path: &Path,
    query: &str,
    wing: Option<&str>,
    room: Option<&str>,
) -> Vec<String> {
    let db = Database::open(db_path).expect("open db");
    db.search_fts(query, wing, room, "all", None, 10)
        .expect("search fts")
        .into_iter()
        .map(|(drawer_id, _rank)| drawer_id)
        .collect()
}

fn column_names(conn: &Connection, table: &str) -> Vec<String> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql).expect("prepare table_info");
    stmt.query_map([], |row| row.get::<_, String>(1))
        .expect("query table_info")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect column names")
}

fn sqlite_master_sql(conn: &Connection, object_type: &str, object_name: &str) -> Option<String> {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2",
        params![object_type, object_name],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .expect("sqlite master sql")
}

fn install_log_capture() -> (Arc<Mutex<Vec<u8>>>, tracing::dispatcher::DefaultGuard) {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer_logs = Arc::clone(&logs);
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(move || LogCapture {
            buffer: Arc::clone(&writer_logs),
        })
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (logs, guard)
}

fn captured_logs(logs: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(logs.lock().expect("log mutex poisoned").clone()).expect("utf8 logs")
}

fn redact_label(text: &str) -> String {
    format!("len={}", text.len())
}

fn create_v3_database(conn: &Connection) {
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
    chunk_index INTEGER,
    deleted_at TEXT,
    importance INTEGER DEFAULT 0
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
    INSERT INTO drawers_fts(drawers_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
END;
"#,
    )
    .expect("create upstream schema");
    conn.execute_batch(FORK_EXT_META_DDL)
        .expect("create fork_ext_meta");
    conn.execute_batch(FORK_EXT_V1_SCHEMA_SQL)
        .expect("apply fork ext v1");
    conn.execute_batch(FORK_EXT_V2_SCHEMA_SQL)
        .expect("apply fork ext v2");
    conn.execute_batch(FORK_EXT_V3_SCHEMA_SQL)
        .expect("apply fork ext v3");
    set_fork_ext_version(conn, 3).expect("set fork ext version 3");
}

fn triple_source_drawer(db_path: &Path, triple_id: &str) -> Option<String> {
    Connection::open(db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT source_drawer FROM triples WHERE id = ?1",
            [triple_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .expect("query triple source drawer")
        .flatten()
}

#[tokio::test(flavor = "current_thread")]
async fn test_agent_diary_bypasses_novelty() {
    let _guard = test_guard().await;
    let env = TestEnv::new(true);
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "existing-diary",
            content: "daily note old",
            wing: "agent-diary",
            room: "claude",
            project_id: None,
        },
        &[1.0, 0.0],
    );
    let server = env.server(&[("daily note new", vec![1.0, 0.0])], &[]);

    let response = ingest(&server, "agent-diary", "claude", "daily note new", None).await;

    assert_eq!(drawer_count(&env.db_path), 2);
    assert_eq!(
        response.novelty_action,
        Some(mempal::ingest::novelty::NoveltyAction::Insert)
    );
    assert!(novelty_rows(&env.db_path).is_empty());
}

#[test]
fn test_fork_ext_migration_v3_to_v4_idempotent_trigger() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let conn = Connection::open(&db_path).expect("open sqlite");
    create_v3_database(&conn);
    conn.execute_batch(
        r#"
CREATE TRIGGER drawers_au_fts
AFTER UPDATE OF content ON drawers BEGIN
    SELECT 1;
END;
"#,
    )
    .expect("create stale trigger");

    apply_fork_ext_migrations_to(&conn, 4).expect("first apply to v4");
    apply_fork_ext_migrations_to(&conn, 4).expect("second apply to v4");

    assert_eq!(read_fork_ext_version(&conn).expect("read version"), 4);
    let trigger_count = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND name = 'drawers_au_fts'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("trigger count");
    assert_eq!(trigger_count, 1);
    let trigger_sql =
        sqlite_master_sql(&conn, "trigger", "drawers_au_fts").expect("drawers_au_fts sql");
    assert!(
        trigger_sql
            .contains("INSERT INTO drawers_fts(drawers_fts, rowid, content) VALUES ('delete'")
    );
    assert!(
        trigger_sql
            .contains("INSERT INTO drawers_fts(rowid, content) VALUES (new.rowid, new.content)")
    );
}

#[test]
fn test_fork_ext_migration_v3_to_v4_schema() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let conn = Connection::open(&db_path).expect("open sqlite");
    create_v3_database(&conn);

    apply_fork_ext_migrations_to(&conn, 4).expect("apply to v4");

    assert_eq!(read_fork_ext_version(&conn).expect("read version"), 4);
    let drawer_columns = column_names(&conn, "drawers");
    assert!(drawer_columns.iter().any(|name| name == "merge_count"));
    assert!(drawer_columns.iter().any(|name| name == "updated_at"));
    assert!(
        sqlite_master_sql(&conn, "table", "novelty_audit").is_some(),
        "novelty_audit table missing"
    );
    assert!(
        sqlite_master_sql(&conn, "trigger", "drawers_au_fts").is_some(),
        "drawers_au_fts missing"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_fts_finds_merged_supplementary() {
    let _guard = test_guard().await;
    let env = TestEnv::new(true);
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "existing",
            content: "foo decision",
            wing: DEFAULT_WING,
            room: DEFAULT_ROOM,
            project_id: None,
        },
        &[1.0, 0.0],
    );
    let server = env.server(&[("bar addition", vec![0.9, 0.4358899])], &[]);

    let response = ingest(&server, DEFAULT_WING, DEFAULT_ROOM, "bar addition", None).await;

    assert_eq!(
        response.novelty_action,
        Some(mempal::ingest::novelty::NoveltyAction::Merge)
    );
    let ids = fts_search_ids(
        &env.db_path,
        "bar addition",
        Some(DEFAULT_WING),
        Some(DEFAULT_ROOM),
    );
    assert!(
        ids.iter().any(|drawer_id| drawer_id == "existing"),
        "merged supplementary text must be searchable via FTS"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_high_similarity_candidate_dropped() {
    let _guard = test_guard().await;
    let env = TestEnv::new(true);
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "existing-proj-a",
            content: "same project note",
            wing: DEFAULT_WING,
            room: DEFAULT_ROOM,
            project_id: Some("proj-A"),
        },
        &[0.97, 0.24310492],
    );
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "other-project",
            content: "other project note",
            wing: DEFAULT_WING,
            room: DEFAULT_ROOM,
            project_id: Some("proj-B"),
        },
        &[1.0, 0.0],
    );
    let server = env.server(&[("candidate drop", vec![1.0, 0.0])], &[]);

    let response = ingest(
        &server,
        DEFAULT_WING,
        DEFAULT_ROOM,
        "candidate drop",
        Some("proj-A"),
    )
    .await;

    assert_eq!(drawer_count(&env.db_path), 2);
    assert_eq!(response.drawer_id, "existing-proj-a");
    assert_eq!(
        response.novelty_action,
        Some(mempal::ingest::novelty::NoveltyAction::Drop)
    );
    assert_eq!(response.near_drawer_id.as_deref(), Some("existing-proj-a"));
    let audits = novelty_rows(&env.db_path);
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].decision, "drop");
    assert_eq!(audits[0].near_drawer_id.as_deref(), Some("existing-proj-a"));
    assert_eq!(audits[0].project_id.as_deref(), Some("proj-A"));
}

#[tokio::test(flavor = "current_thread")]
async fn test_low_similarity_candidate_inserted() {
    let _guard = test_guard().await;
    let env = TestEnv::new(true);
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "existing",
            content: "existing note",
            wing: DEFAULT_WING,
            room: DEFAULT_ROOM,
            project_id: None,
        },
        &[1.0, 0.0],
    );
    let server = env.server(&[("candidate insert", vec![0.2, 0.9797959])], &[]);

    let response = ingest(
        &server,
        DEFAULT_WING,
        DEFAULT_ROOM,
        "candidate insert",
        None,
    )
    .await;

    assert_eq!(drawer_count(&env.db_path), 2);
    assert_eq!(
        response.novelty_action,
        Some(mempal::ingest::novelty::NoveltyAction::Insert)
    );
    assert_eq!(response.near_drawer_id, None);
    let audits = novelty_rows(&env.db_path);
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].decision, "insert");
    assert_eq!(audits[0].near_drawer_id, None);
}

#[tokio::test(flavor = "current_thread")]
async fn test_medium_similarity_candidate_merged() {
    let _guard = test_guard().await;
    let env = TestEnv::new(true);
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "existing",
            content: "Decision: use Arc<Mutex<>>",
            wing: DEFAULT_WING,
            room: DEFAULT_ROOM,
            project_id: None,
        },
        &[1.0, 0.0],
    );
    let candidate = "Also: use RwLock when reads dominate";
    let server = env.server(&[(candidate, vec![0.9, 0.4358899])], &[]);

    let response = ingest(&server, DEFAULT_WING, DEFAULT_ROOM, candidate, None).await;

    assert_eq!(drawer_count(&env.db_path), 1);
    assert_eq!(response.drawer_id, "existing");
    assert_eq!(
        response.novelty_action,
        Some(mempal::ingest::novelty::NoveltyAction::Merge)
    );
    assert_eq!(response.near_drawer_id.as_deref(), Some("existing"));
    let content = drawer_content(&env.db_path, "existing");
    assert!(content.contains("Decision: use Arc<Mutex<>>"));
    assert!(content.contains("SUPPLEMENTARY ("));
    assert!(content.contains(candidate));
    let (merge_count, updated_at) = merge_state(&env.db_path, "existing");
    assert_eq!(merge_count, 1);
    assert!(updated_at.is_some(), "merge must set updated_at");
    let audits = novelty_rows(&env.db_path);
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].decision, "merge");
    assert_eq!(audits[0].near_drawer_id.as_deref(), Some("existing"));
}

#[tokio::test(flavor = "current_thread")]
async fn test_merge_preserves_drawer_id_for_kg() {
    let _guard = test_guard().await;
    let env = TestEnv::new(true);
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "existing",
            content: "Decision: use Arc<Mutex<>>",
            wing: DEFAULT_WING,
            room: DEFAULT_ROOM,
            project_id: None,
        },
        &[1.0, 0.0],
    );
    let db = env.db();
    db.insert_triple(&Triple {
        id: "triple-existing".to_string(),
        subject: "drawer:existing".to_string(),
        predicate: "implies".to_string(),
        object: "RwLock".to_string(),
        valid_from: None,
        valid_to: None,
        confidence: 1.0,
        source_drawer: Some("existing".to_string()),
    })
    .expect("insert triple");
    let candidate = "Also: use RwLock when reads dominate";
    let server = env.server(&[(candidate, vec![0.9, 0.4358899])], &[]);

    let response = ingest(&server, DEFAULT_WING, DEFAULT_ROOM, candidate, None).await;

    assert_eq!(response.drawer_id, "existing");
    assert!(
        Database::open(&env.db_path)
            .expect("open db")
            .get_drawer("existing")
            .expect("get drawer")
            .is_some()
    );
    assert_eq!(
        triple_source_drawer(&env.db_path, "triple-existing").as_deref(),
        Some("existing")
    );
    let triples = Database::open(&env.db_path)
        .expect("open db")
        .query_triples(
            Some("drawer:existing"),
            Some("implies"),
            Some("RwLock"),
            false,
        )
        .expect("query triples");
    assert_eq!(triples.len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn test_novelty_disabled_skips_filter() {
    let _guard = test_guard().await;
    let env = TestEnv::new(false);
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "existing",
            content: "existing note original",
            wing: DEFAULT_WING,
            room: DEFAULT_ROOM,
            project_id: None,
        },
        &[1.0, 0.0],
    );
    let server = env.server(&[("existing note duplicate", vec![1.0, 0.0])], &[]);

    let response = ingest(
        &server,
        DEFAULT_WING,
        DEFAULT_ROOM,
        "existing note duplicate",
        None,
    )
    .await;

    assert_eq!(drawer_count(&env.db_path), 2);
    assert_eq!(
        response.novelty_action,
        Some(mempal::ingest::novelty::NoveltyAction::Insert)
    );
    assert!(novelty_rows(&env.db_path).is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn test_novelty_embedder_error_fails_open() {
    let _guard = test_guard().await;
    let env = TestEnv::new(true);
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "existing",
            content: "stable existing note",
            wing: DEFAULT_WING,
            room: DEFAULT_ROOM,
            project_id: None,
        },
        &[1.0, 0.0],
    );
    let candidate = "secret-token-12345";
    let server = env.server(&[(candidate, vec![0.9, 0.4358899])], &["SUPPLEMENTARY ("]);
    let (logs, _log_guard) = install_log_capture();

    let response = ingest(&server, DEFAULT_WING, DEFAULT_ROOM, candidate, None).await;

    assert_eq!(drawer_count(&env.db_path), 2);
    assert_eq!(
        response.novelty_action,
        Some(mempal::ingest::novelty::NoveltyAction::Insert)
    );
    assert_eq!(response.near_drawer_id.as_deref(), Some("existing"));
    assert_eq!(
        drawer_content(&env.db_path, "existing"),
        "stable existing note",
        "fail-open insert must not mutate merge target"
    );
    let audits = novelty_rows(&env.db_path);
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].decision, "insert_due_to_embed_error");
    assert_eq!(audits[0].near_drawer_id.as_deref(), Some("existing"));
    let logs = captured_logs(&logs);
    assert!(logs.contains("novelty merge re-embed failed; fail-open insert"));
    assert!(
        !logs.contains(candidate),
        "logs must not leak raw drawer content"
    );
    assert!(
        !logs.contains("stable existing note\n---"),
        "logs must not include merged raw content"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_wing_scope_respected() {
    let _guard = test_guard().await;
    let env = TestEnv::new(true);
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "other-wing",
            content: "other wing note",
            wing: "planning",
            room: DEFAULT_ROOM,
            project_id: Some("proj-A"),
        },
        &[1.0, 0.0],
    );
    let server = env.server(&[("candidate scope", vec![1.0, 0.0])], &[]);

    let response = ingest(
        &server,
        DEFAULT_WING,
        DEFAULT_ROOM,
        "candidate scope",
        Some("proj-A"),
    )
    .await;

    assert_eq!(drawer_count(&env.db_path), 2);
    assert_eq!(
        response.novelty_action,
        Some(mempal::ingest::novelty::NoveltyAction::Insert)
    );
    assert_eq!(response.near_drawer_id, None);
    let audits = novelty_rows(&env.db_path);
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].decision, "insert");
    assert_eq!(audits[0].near_drawer_id, None);
}
