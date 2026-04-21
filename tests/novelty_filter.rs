use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use mempal::embed::{EmbedError, Embedder, EmbedderFactory};
use mempal::mcp::{IngestRequest, MempalMcpServer};
use rmcp::handler::server::wrapper::Parameters;
use rusqlite::Connection;
use tempfile::TempDir;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[path = "../src/core/db_fork_ext.rs"]
mod db_fork_ext;

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
}

struct DeterministicEmbedder {
    vectors: Arc<HashMap<String, Vec<f32>>>,
    default_vector: Vec<f32>,
}

#[async_trait]
impl EmbedderFactory for DeterministicEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(DeterministicEmbedder {
            vectors: Arc::clone(&self.vectors),
            default_vector: self.default_vector.clone(),
        }))
    }
}

#[async_trait]
impl Embedder for DeterministicEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
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

fn write_config(path: &Path, db_path: &Path) {
    fs::write(
        path,
        format!(
            r#"
db_path = "{}"

[config_hot_reload]
enabled = false

[ingest_gating]
enabled = false

[ingest_gating.novelty]
enabled = true
duplicate_threshold = 0.95
merge_threshold = 0.80
wing_scope = "same_wing"
top_k_candidates = 1
max_merges_per_drawer = 10
max_content_bytes_per_drawer = 65536
"#,
            db_path.display()
        ),
    )
    .expect("write config");
}

fn insert_existing_drawer(db_path: &Path, content: &str, vector: &[f32]) {
    let db = Database::open(db_path).expect("open db");
    db.insert_drawer(&Drawer {
        id: "existing".to_string(),
        content: content.to_string(),
        wing: "code-memory".to_string(),
        room: Some("novelty".to_string()),
        source_file: Some("existing.md".to_string()),
        source_type: SourceType::Manual,
        added_at: "1713000000".to_string(),
        chunk_index: Some(0),
        importance: 0,
    })
    .expect("insert drawer");
    db.insert_vector("existing", vector).expect("insert vector");
}

fn merged_content(db_path: &Path) -> String {
    let conn = Connection::open(db_path).expect("open sqlite");
    conn.query_row(
        "SELECT content FROM drawers WHERE id = 'existing'",
        [],
        |row| row.get::<_, String>(0),
    )
    .expect("query merged content")
}

fn merge_count_and_updated_at(db_path: &Path) -> (i64, Option<String>) {
    let conn = Connection::open(db_path).expect("open sqlite");
    conn.query_row(
        "SELECT merge_count, updated_at FROM drawers WHERE id = 'existing'",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
    )
    .expect("query merge metadata")
}

fn build_server(
    db_path: &Path,
    config_path: &Path,
    vectors: HashMap<String, Vec<f32>>,
) -> MempalMcpServer {
    ConfigHandle::bootstrap(config_path).expect("bootstrap config");
    let config = Config::load_from(config_path).expect("load config");
    MempalMcpServer::new_with_factory_and_config(
        db_path.to_path_buf(),
        config,
        Arc::new(DeterministicEmbedderFactory {
            vectors: Arc::new(vectors),
            default_vector: vec![0.6, 0.8],
        }),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_novelty_drop_near_duplicate() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    let config_path = mempal_home.join("config.toml");
    Database::open(&db_path).expect("open db");
    write_config(&config_path, &db_path);
    insert_existing_drawer(&db_path, "existing note", &[1.0, 0.0]);

    let server = build_server(
        &db_path,
        &config_path,
        HashMap::from([("candidate-drop".to_string(), vec![1.0, 0.0])]),
    );

    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "candidate-drop".to_string(),
            wing: "code-memory".to_string(),
            room: Some("novelty".to_string()),
            source: None,
            dry_run: Some(false),
            importance: None,
        }))
        .await
        .expect("drop response")
        .0;

    assert_eq!(drawer_count(&db_path), 1);
    assert_eq!(
        response.novelty_action,
        Some(mempal::ingest::novelty::NoveltyAction::Drop)
    );
    assert_eq!(response.near_drawer_id.as_deref(), Some("existing"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_novelty_merge_similar_but_extended() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    let config_path = mempal_home.join("config.toml");
    Database::open(&db_path).expect("open db");
    write_config(&config_path, &db_path);
    insert_existing_drawer(&db_path, "Decision: use Arc<Mutex<>>", &[1.0, 0.0]);

    let server = build_server(
        &db_path,
        &config_path,
        HashMap::from([(
            "Also: use RwLock when reads dominate".to_string(),
            vec![0.85, 0.5267827],
        )]),
    );

    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "Also: use RwLock when reads dominate".to_string(),
            wing: "code-memory".to_string(),
            room: Some("novelty".to_string()),
            source: None,
            dry_run: Some(false),
            importance: None,
        }))
        .await
        .expect("merge response")
        .0;

    assert_eq!(drawer_count(&db_path), 1);
    assert_eq!(
        response.novelty_action,
        Some(mempal::ingest::novelty::NoveltyAction::Merge)
    );
    assert_eq!(response.near_drawer_id.as_deref(), Some("existing"));
    let (merge_count, updated_at) = merge_count_and_updated_at(&db_path);
    assert_eq!(merge_count, 1);
    assert!(updated_at.is_some(), "updated_at must be set after merge");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_novelty_insert_distinct() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    let config_path = mempal_home.join("config.toml");
    Database::open(&db_path).expect("open db");
    write_config(&config_path, &db_path);
    insert_existing_drawer(&db_path, "existing note", &[1.0, 0.0]);

    let server = build_server(
        &db_path,
        &config_path,
        HashMap::from([("candidate-insert".to_string(), vec![0.2, 0.9797959])]),
    );

    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "candidate-insert".to_string(),
            wing: "code-memory".to_string(),
            room: Some("novelty".to_string()),
            source: None,
            dry_run: Some(false),
            importance: None,
        }))
        .await
        .expect("insert response")
        .0;

    assert_eq!(drawer_count(&db_path), 2);
    assert_eq!(
        response.novelty_action,
        Some(mempal::ingest::novelty::NoveltyAction::Insert)
    );
    assert_eq!(response.near_drawer_id, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_merge_preserves_raw_verbatim() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    let config_path = mempal_home.join("config.toml");
    Database::open(&db_path).expect("open db");
    write_config(&config_path, &db_path);
    insert_existing_drawer(&db_path, "old raw content", &[1.0, 0.0]);

    let new_content = "new raw continuation";
    let server = build_server(
        &db_path,
        &config_path,
        HashMap::from([(new_content.to_string(), vec![0.85, 0.5267827])]),
    );

    server
        .mempal_ingest(Parameters(IngestRequest {
            content: new_content.to_string(),
            wing: "code-memory".to_string(),
            room: Some("novelty".to_string()),
            source: None,
            dry_run: Some(false),
            importance: None,
        }))
        .await
        .expect("merge response");

    let content = merged_content(&db_path);
    assert!(content.starts_with("old raw content\n---\nSUPPLEMENTARY ("));
    assert!(content.ends_with(&format!("):\n{new_content}")));
    assert!(
        !content.contains("summary"),
        "merge must preserve raw verbatim"
    );
}

#[test]
fn test_migration_v3_to_v4_idempotent_trigger() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let conn = Connection::open(&db_path).expect("open sqlite");
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
    conn.execute_batch(db_fork_ext::FORK_EXT_META_DDL)
        .expect("fork ext meta");
    conn.execute_batch(db_fork_ext::FORK_EXT_V1_SCHEMA_SQL)
        .expect("ext v1");
    conn.execute_batch(db_fork_ext::FORK_EXT_V2_SCHEMA_SQL)
        .expect("ext v2");
    conn.execute_batch(db_fork_ext::FORK_EXT_V3_SCHEMA_SQL)
        .expect("ext v3");
    db_fork_ext::set_fork_ext_version(&conn, 3).expect("set version 3");
    conn.execute_batch(
        r#"
CREATE TRIGGER drawers_fts_after_update
AFTER UPDATE ON drawers BEGIN
    SELECT 1;
END;
"#,
    )
    .expect("create stale trigger");

    db_fork_ext::apply_fork_ext_migrations(&conn).expect("first apply");
    db_fork_ext::apply_fork_ext_migrations(&conn).expect("second apply");

    let trigger_count = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name='drawers_fts_after_update'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("trigger count");
    assert_eq!(trigger_count, 1);
}

fn drawer_count(db_path: &Path) -> i64 {
    Database::open(db_path)
        .expect("open db")
        .drawer_count()
        .expect("drawer count")
}
