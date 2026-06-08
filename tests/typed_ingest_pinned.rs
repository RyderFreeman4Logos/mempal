use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use async_trait::async_trait;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::core::types::{
    Drawer, KnowledgeStatus, MemoryDomain, MemoryKind, SourceType, default_confidence,
};
use mempal::core::utils::current_timestamp;
use mempal::embed::{EmbedError, Embedder, EmbedderFactory};
use mempal::mcp::{IngestRequest, MempalMcpServer, PinnedFactsRequest};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

#[derive(Clone)]
struct StaticEmbedderFactory {
    dim: usize,
}

struct StaticEmbedder {
    dim: usize,
}

#[async_trait]
impl EmbedderFactory for StaticEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(StaticEmbedder { dim: self.dim }))
    }
}

#[async_trait]
impl Embedder for StaticEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.25; self.dim]).collect())
    }

    fn dimensions(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &str {
        "static-test"
    }
}

#[derive(Clone)]
struct PanicEmbedderFactory;

#[async_trait]
impl EmbedderFactory for PanicEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        panic!("pinned facts must not build an embedder")
    }
}

struct TestEnv {
    _tmp: TempDir,
    home: PathBuf,
    db_path: PathBuf,
    config_path: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let home = tmp.path().join("home");
        let mempal_home = home.join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open db");
        let config_path = mempal_home.join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"
db_path = "{}"

[config_hot_reload]
enabled = false

[embed]
backend = "model2vec"
"#,
                db_path.display()
            ),
        )
        .expect("write config");
        Self {
            _tmp: tmp,
            home,
            db_path,
            config_path,
        }
    }

    fn config(&self) -> Config {
        ConfigHandle::bootstrap(&self.config_path).expect("bootstrap config");
        Config::load_from(&self.config_path).expect("load config")
    }

    fn db(&self) -> Database {
        Database::open(&self.db_path).expect("open db")
    }

    fn server(&self) -> MempalMcpServer {
        MempalMcpServer::new_with_factory_and_config(
            self.db_path.clone(),
            self.config(),
            Arc::new(StaticEmbedderFactory { dim: 4 }),
        )
        .expect("create MCP server")
    }
}

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn insert_drawer(
    db: &Database,
    id: &str,
    content: &str,
    is_pinned: bool,
    pin_order: Option<i64>,
    importance: i32,
    status: Option<KnowledgeStatus>,
) {
    let source_type = SourceType::AgentObservation;
    let drawer = Drawer {
        id: id.to_string(),
        content: content.to_string(),
        wing: "typed-tests".to_string(),
        room: Some("facts".to_string()),
        source_file: Some(format!("tests://{id}")),
        source_type,
        confidence: default_confidence(source_type),
        added_at: current_timestamp(),
        importance,
        memory_kind: MemoryKind::ProfileFact,
        domain: MemoryDomain::User,
        field: "preferences".to_string(),
        status,
        is_pinned,
        pin_order,
        ..Drawer::default()
    };
    db.insert_drawer(&drawer).expect("insert drawer");
}

#[tokio::test]
async fn test_typed_ingest_preserves_kind() {
    let env = TestEnv::new();
    let server = env.server();

    let response = server
        .ingest_json_for_test(
            serde_json::to_value(IngestRequest {
                content: "Prefer CLI dashboards over web dashboards.".to_string(),
                wing: "profile".to_string(),
                room: Some("facts".to_string()),
                source: Some("typed-test".to_string()),
                memory_kind: Some("profile_fact".to_string()),
                domain: Some("user".to_string()),
                field: Some("preferences".to_string()),
                is_pinned: Some(true),
                ..IngestRequest::default()
            })
            .expect("serialize ingest request"),
        )
        .await
        .expect("typed ingest");

    let db = env.db();
    let drawer = db
        .get_drawer(&response.drawer_id)
        .expect("load drawer")
        .expect("drawer exists");
    assert_eq!(drawer.memory_kind, MemoryKind::ProfileFact);
    assert_eq!(drawer.domain, MemoryDomain::User);
    assert_eq!(drawer.field, "preferences");
    assert!(drawer.is_pinned);
}

#[tokio::test]
async fn test_pinned_facts_no_embedding() {
    let env = TestEnv::new();
    let db = env.db();
    insert_drawer(
        &db,
        "drawer_pinned_no_embed",
        "Pinned facts load through SQL only.",
        true,
        Some(0),
        5,
        Some(KnowledgeStatus::Active),
    );
    let server = MempalMcpServer::new_with_factory_and_config(
        env.db_path.clone(),
        env.config(),
        Arc::new(PanicEmbedderFactory),
    )
    .expect("create MCP server");

    let response = server
        .mempal_pinned_facts(Parameters(PinnedFactsRequest {
            project_id: None,
            budget_chars: Some(4_000),
        }))
        .await
        .expect("pinned facts")
        .0;

    assert_eq!(response.facts.len(), 1);
    assert_eq!(response.facts[0].drawer_id, "drawer_pinned_no_embed");
    assert!(
        response
            .text
            .contains("Pinned facts load through SQL only.")
    );
}

#[test]
fn test_pinned_facts_budget() {
    let env = TestEnv::new();
    let db = env.db();
    insert_drawer(
        &db,
        "drawer_budget",
        "abcdefghijklmnopqrstuvwxyz",
        true,
        Some(0),
        5,
        Some(KnowledgeStatus::Active),
    );

    let facts = db.get_pinned_facts(None, 10).expect("load pinned facts");

    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].content, "abcdefghij");
    assert_eq!(facts[0].content.chars().count(), 10);
}

#[tokio::test]
async fn test_supersedes_chain() {
    let env = TestEnv::new();
    let server = env.server();
    let old = server
        .ingest_json_for_test(
            serde_json::to_value(IngestRequest {
                content: "Old canonical fact.".to_string(),
                wing: "profile".to_string(),
                room: Some("facts".to_string()),
                source: Some("typed-test-old".to_string()),
                ..IngestRequest::default()
            })
            .expect("serialize ingest request"),
        )
        .await
        .expect("old ingest");

    let new = server
        .ingest_json_for_test(
            serde_json::to_value(IngestRequest {
                content: "New canonical fact.".to_string(),
                wing: "profile".to_string(),
                room: Some("facts".to_string()),
                source: Some("typed-test-new".to_string()),
                supersedes: Some(old.drawer_id.clone()),
                ..IngestRequest::default()
            })
            .expect("serialize ingest request"),
        )
        .await
        .expect("new ingest");

    let db = env.db();
    let new_drawer = db
        .get_drawer(&new.drawer_id)
        .expect("load new drawer")
        .expect("new drawer exists");
    assert_eq!(
        new_drawer.supersedes.as_deref(),
        Some(old.drawer_id.as_str())
    );
    let (status, deleted_at): (Option<String>, Option<String>) = db
        .conn()
        .query_row(
            "SELECT status, deleted_at FROM drawers WHERE id = ?1",
            [&old.drawer_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("load old drawer status");
    assert_eq!(status.as_deref(), Some("superseded"));
    assert!(deleted_at.is_some());
}

#[tokio::test]
async fn test_default_ingest_unchanged() {
    let env = TestEnv::new();
    let server = env.server();

    let response = server
        .ingest_json_for_test(
            serde_json::to_value(IngestRequest {
                content: "Plain evidence still uses old defaults.".to_string(),
                wing: "project".to_string(),
                room: Some("notes".to_string()),
                source: Some("typed-test-default".to_string()),
                ..IngestRequest::default()
            })
            .expect("serialize ingest request"),
        )
        .await
        .expect("default ingest");

    let db = env.db();
    let drawer = db
        .get_drawer(&response.drawer_id)
        .expect("load drawer")
        .expect("drawer exists");
    assert_eq!(drawer.memory_kind, MemoryKind::Evidence);
    assert_eq!(drawer.domain, MemoryDomain::Project);
    assert_eq!(drawer.field, "general");
    assert!(!drawer.is_pinned);
    assert_eq!(drawer.pin_order, None);
    assert_eq!(drawer.supersedes, None);
    assert_eq!(drawer.status, None);
}

#[test]
fn test_cli_pin_unpin_pinned_reorder() {
    let env = TestEnv::new();
    let db = env.db();
    insert_drawer(
        &db,
        "drawer_cli_pin",
        "CLI pin command fact.",
        false,
        None,
        4,
        Some(KnowledgeStatus::Active),
    );

    let pin = Command::new(mempal_bin())
        .env("HOME", &env.home)
        .args(["pin", "drawer_cli_pin"])
        .output()
        .expect("run pin");
    assert!(
        pin.status.success(),
        "pin stderr: {}",
        String::from_utf8_lossy(&pin.stderr)
    );

    let pinned = Command::new(mempal_bin())
        .env("HOME", &env.home)
        .arg("pinned")
        .output()
        .expect("run pinned");
    assert!(
        pinned.status.success(),
        "pinned stderr: {}",
        String::from_utf8_lossy(&pinned.stderr)
    );
    assert!(
        String::from_utf8_lossy(&pinned.stdout).contains("drawer_cli_pin"),
        "pinned stdout should list drawer_cli_pin"
    );

    let reorder = Command::new(mempal_bin())
        .env("HOME", &env.home)
        .args(["pinned", "--reorder", "drawer_cli_pin"])
        .output()
        .expect("run reorder");
    assert!(
        reorder.status.success(),
        "reorder stderr: {}",
        String::from_utf8_lossy(&reorder.stderr)
    );

    let unpin = Command::new(mempal_bin())
        .env("HOME", &env.home)
        .args(["unpin", "drawer_cli_pin"])
        .output()
        .expect("run unpin");
    assert!(
        unpin.status.success(),
        "unpin stderr: {}",
        String::from_utf8_lossy(&unpin.stderr)
    );

    let is_pinned: i64 = db
        .conn()
        .query_row(
            "SELECT is_pinned FROM drawers WHERE id = 'drawer_cli_pin'",
            [],
            |row| row.get(0),
        )
        .expect("read is_pinned");
    assert_eq!(is_pinned, 0);
}
