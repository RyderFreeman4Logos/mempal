#![cfg(feature = "integration")]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mempal::core::config::ConfigHandle;
use mempal::core::db::Database;
use mempal::embed::{EmbedError, Embedder, EmbedderFactory};
use mempal::mcp::{IngestRequest, MempalMcpServer, SearchRequest};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

async fn config_guard() -> OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(AsyncMutex::new(())))
        .clone()
        .lock_owned()
        .await
}

#[derive(Clone)]
struct StaticEmbedderFactory;

struct StaticEmbedder;

#[async_trait]
impl EmbedderFactory for StaticEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(StaticEmbedder))
    }
}

#[async_trait]
impl Embedder for StaticEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
    }

    fn dimensions(&self) -> usize {
        3
    }

    fn name(&self) -> &str {
        "static"
    }
}

struct TestEnv {
    _tmp: TempDir,
    db_path: PathBuf,
    server: MempalMcpServer,
}

impl TestEnv {
    fn new(turns_section: &str) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let config_path = mempal_home.join("config.toml");
        let db_path = mempal_home.join("palace.db");
        fs::write(&config_path, config_text(&db_path, turns_section)).expect("write config");
        Database::open(&db_path).expect("open db");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        let server =
            MempalMcpServer::new_with_factory(db_path.clone(), Arc::new(StaticEmbedderFactory));
        Self {
            _tmp: tmp,
            db_path,
            server,
        }
    }

    fn db(&self) -> Database {
        Database::open(&self.db_path).expect("open db")
    }
}

fn config_text(db_path: &Path, turns_section: &str) -> String {
    format!(
        r#"
db_path = "{}"

[embedder]
backend = "api"
base_url = "http://127.0.0.1:9/v1/"
api_model = "test-model"

[config_hot_reload]
enabled = false

[search]
strict_project_isolation = false
progressive_disclosure = false
preview_chars = 200
exclude_raw_turns = true

{turns_section}
"#,
        db_path.display()
    )
}

async fn ingest(
    server: &MempalMcpServer,
    content: &str,
    wing: &str,
    room: &str,
    importance: i32,
) -> mempal::mcp::IngestResponse {
    server
        .mempal_ingest(Parameters(IngestRequest {
            content: content.to_string(),
            wing: wing.to_string(),
            room: Some(room.to_string()),
            importance: Some(importance),
            ..IngestRequest::default()
        }))
        .await
        .expect("ingest")
        .0
}

#[tokio::test]
async fn storage_mode_off_skips_raw_turn_ingest() {
    let _guard = config_guard().await;
    let env = TestEnv::new(
        r#"
[turns]
storage_mode = "off"
raw_turn_wings = ["hooks-raw"]
raw_turn_rooms = ["turns"]
"#,
    );

    let response = ingest(
        &env.server,
        "needle raw turn should not persist",
        "hooks-raw",
        "user-prompt",
        5,
    )
    .await;

    assert!(!response.dropped);
    assert!(response.drawer_ids.is_empty());
    assert_eq!(response.chunk_count, 0);
    assert_eq!(env.db().drawer_count().expect("drawer count"), 0);
}

#[tokio::test]
async fn raw_evidence_forces_low_importance() {
    let _guard = config_guard().await;
    let env = TestEnv::new(
        r#"
[turns]
storage_mode = "raw_evidence"
default_importance = 0
raw_turn_wings = ["hooks-raw"]
raw_turn_rooms = ["turns"]
"#,
    );

    let response = ingest(
        &env.server,
        "needle raw turn persists as evidence",
        "hooks-raw",
        "user-prompt",
        5,
    )
    .await;
    let drawer_id = response.drawer_ids.first().expect("drawer id");
    let drawer = env
        .db()
        .get_drawer(drawer_id)
        .expect("get drawer")
        .expect("stored drawer");

    assert_eq!(drawer.importance, 0);
}

#[tokio::test]
async fn default_search_excludes_raw_turns_until_requested() {
    let _guard = config_guard().await;
    let env = TestEnv::new(
        r#"
[turns]
storage_mode = "raw_evidence"
default_importance = 0
raw_turn_wings = ["hooks-raw"]
raw_turn_rooms = ["turns"]
"#,
    );
    ingest(
        &env.server,
        "needle durable project fact",
        "mempal",
        "facts",
        4,
    )
    .await;
    ingest(
        &env.server,
        "needle raw transcript turn",
        "hooks-raw",
        "user-prompt",
        5,
    )
    .await;

    let default_search = env
        .server
        .mempal_search(Parameters(SearchRequest {
            query: "needle".to_string(),
            top_k: Some(10),
            ..SearchRequest::default()
        }))
        .await
        .expect("default search")
        .0;
    assert!(
        default_search
            .results
            .iter()
            .all(|result| result.wing != "hooks-raw")
    );

    let include_raw = env
        .server
        .mempal_search(Parameters(SearchRequest {
            query: "needle".to_string(),
            top_k: Some(10),
            include_raw_turns: Some(true),
            ..SearchRequest::default()
        }))
        .await
        .expect("include raw search")
        .0;
    assert!(
        include_raw
            .results
            .iter()
            .any(|result| result.wing == "hooks-raw")
    );
}

#[tokio::test]
async fn non_raw_ingest_ignores_turn_storage_off() {
    let _guard = config_guard().await;
    let env = TestEnv::new(
        r#"
[turns]
storage_mode = "off"
raw_turn_wings = ["hooks-raw"]
raw_turn_rooms = ["turns"]
"#,
    );

    let response = ingest(
        &env.server,
        "durable fact still persists",
        "mempal",
        "facts",
        4,
    )
    .await;
    let drawer_id = response.drawer_ids.first().expect("drawer id");
    let drawer = env
        .db()
        .get_drawer(drawer_id)
        .expect("get drawer")
        .expect("stored drawer");

    assert_eq!(drawer.importance, 4);
    assert_eq!(env.db().drawer_count().expect("drawer count"), 1);
}

#[tokio::test]
async fn custom_raw_turn_room_prefixes_are_honored() {
    let _guard = config_guard().await;
    let env = TestEnv::new(
        r#"
[turns]
storage_mode = "off"
raw_turn_wings = ["custom-raw"]
raw_turn_rooms = ["conversation"]
"#,
    );

    ingest(
        &env.server,
        "custom room raw turn should skip",
        "ordinary-wing",
        "conversation",
        5,
    )
    .await;
    ingest(
        &env.server,
        "custom wing raw turn should skip",
        "custom-raw-agent",
        "events",
        5,
    )
    .await;

    assert_eq!(env.db().drawer_count().expect("drawer count"), 0);
}
