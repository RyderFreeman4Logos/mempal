use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::embed::{EmbedError, Embedder, EmbedderFactory};
use mempal::mcp::MempalMcpServer;
use mockito::Server;
use tempfile::TempDir;

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
        Ok(texts.iter().map(|_| vec![0.25; 4]).collect())
    }

    fn dimensions(&self) -> usize {
        4
    }

    fn name(&self) -> &str {
        "static-test"
    }
}

struct TestEnv {
    _tmp: TempDir,
    db_path: PathBuf,
    config: Config,
}

impl TestEnv {
    fn new(llm_server: &Server) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db_path = mempal_home.join("palace.db");
        let config_path = mempal_home.join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"
db_path = "{}"

[config_hot_reload]
enabled = false

[embed]
backend = "stub"

[memory_intelligence]
mode = "local_llm"

[memory_intelligence.llm]
base_url = "{}/v1"
model = "local-test-model"
timeout_secs = 1
"#,
                db_path.display(),
                llm_server.url(),
            ),
        )
        .expect("write config");
        Database::open(&db_path).expect("open db");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        let config = Config::load_from(&config_path).expect("load config");
        Self {
            _tmp: tmp,
            db_path,
            config,
        }
    }

    fn server(&self) -> MempalMcpServer {
        MempalMcpServer::new_with_factory_and_config(
            self.db_path.clone(),
            self.config.clone(),
            Arc::new(StaticEmbedderFactory),
        )
        .expect("create MCP server")
    }
}

fn llm_response(content: &str) -> String {
    serde_json::json!({
        "model": "local-test-model",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": content,
            }
        }]
    })
    .to_string()
}

#[tokio::test]
async fn conclusion_ingest_extracts_and_persists_kg_triples() {
    let mut llm_server = Server::new_async().await;
    let extracted = serde_json::json!({
        "triples": [{
            "subject": "Project Mempal",
            "predicate": "uses",
            "object": "SQLite",
            "confidence": 0.95,
        }]
    })
    .to_string();
    let mock = llm_server
        .mock("POST", "/v1/chat/completions")
        .expect(1)
        .with_status(200)
        .with_body(llm_response(&extracted))
        .create_async()
        .await;
    let env = TestEnv::new(&llm_server);
    let server = env.server();

    let response = server
        .ingest_json_for_test(serde_json::json!({
            "content": "Project Mempal uses SQLite.",
            "wing": "hermes-user/test/default",
            "room": "facts",
            "memory_kind": "profile_fact",
            "source_type": "user_explicit",
            "source": "hermes-session-conclusion",
            "importance": 4,
        }))
        .await
        .expect("conclusion ingest");

    mock.assert_async().await;
    let triples = Database::open(&env.db_path)
        .expect("open db")
        .query_triples(Some("Project Mempal"), Some("uses"), Some("SQLite"), true)
        .expect("query triples");
    assert_eq!(triples.len(), 1);
    assert_eq!(
        triples[0].source_drawer.as_deref(),
        Some(response.drawer_id.as_str())
    );
}
