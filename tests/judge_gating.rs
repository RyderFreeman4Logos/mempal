use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::embed::{EmbedError, Embedder, EmbedderFactory};
use mempal::mcp::{IngestRequest, MempalMcpServer};
use mockito::Server;
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

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
    embed_calls: Arc<Mutex<Vec<String>>>,
}

struct DeterministicEmbedder {
    vectors: Arc<HashMap<String, Vec<f32>>>,
    default_vector: Vec<f32>,
    embed_calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl EmbedderFactory for DeterministicEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(DeterministicEmbedder {
            vectors: Arc::clone(&self.vectors),
            default_vector: self.default_vector.clone(),
            embed_calls: Arc::clone(&self.embed_calls),
        }))
    }
}

#[async_trait]
impl Embedder for DeterministicEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut calls = self.embed_calls.lock().expect("embed_calls mutex");
        for text in texts {
            calls.push((*text).to_string());
        }
        drop(calls);

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

fn write_config(path: &Path, db_path: &Path, body: &str) {
    fs::write(
        path,
        format!(
            r#"
db_path = "{}"

[config_hot_reload]
enabled = false

{}
"#,
            db_path.display(),
            body
        ),
    )
    .expect("write config");
}

fn drawer_count(db_path: &Path) -> i64 {
    Database::open(db_path)
        .expect("open db")
        .drawer_count()
        .expect("drawer count")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tier1_rule_reject_short_circuits_tier2() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");

    let config_path = mempal_home.join("config.toml");
    write_config(
        &config_path,
        &db_path,
        r#"
[ingest_gating]
enabled = true

[[ingest_gating.rules]]
action = "reject"
content_bytes_lt = 12

[ingest_gating.embedding_classifier]
enabled = true
threshold = 0.8
prototypes = ["accept"]
"#,
    );

    let embed_calls = Arc::new(Mutex::new(Vec::new()));
    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
    let config = Config::load_from(&config_path).expect("load config");
    let factory = Arc::new(DeterministicEmbedderFactory {
        vectors: Arc::new(HashMap::from([("accept".to_string(), vec![1.0, 0.0])])),
        default_vector: vec![0.0, 1.0],
        embed_calls: Arc::clone(&embed_calls),
    });
    let server =
        MempalMcpServer::new_with_factory_and_config(db_path.clone(), config.clone(), factory);

    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "tiny".to_string(),
            wing: "code-memory".to_string(),
            room: Some("gating".to_string()),
            source: None,
            dry_run: Some(false),
            importance: None,
        }))
        .await
        .expect("gating request should succeed")
        .0;

    assert!(config.ingest_gating.enabled);
    assert_eq!(drawer_count(&db_path), 0);
    assert!(
        embed_calls.lock().expect("embed_calls mutex").is_empty(),
        "tier-1 reject must not touch prototype or candidate embedding"
    );
    let decision = response.gating_decision.expect("gating decision");
    assert_eq!(decision.decision, "rejected");
    assert_eq!(decision.tier, 1);
    assert_eq!(
        decision.matched_pattern.as_deref(),
        Some("content_bytes_lt")
    );
    assert_eq!(decision.score, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tier2_prototype_cosine_classifier() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");

    let config_path = mempal_home.join("config.toml");
    write_config(
        &config_path,
        &db_path,
        r#"
[ingest_gating]
enabled = true

[ingest_gating.embedding_classifier]
enabled = true
threshold = 0.8
prototypes = ["accept-prototype"]
"#,
    );

    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
    let config = Config::load_from(&config_path).expect("load config");
    let factory = Arc::new(DeterministicEmbedderFactory {
        vectors: Arc::new(HashMap::from([
            ("accept-prototype".to_string(), vec![1.0, 0.0]),
            ("candidate-keep".to_string(), vec![1.0, 0.0]),
            ("candidate-drop".to_string(), vec![0.0, 1.0]),
        ])),
        default_vector: vec![0.0, 1.0],
        embed_calls: Arc::new(Mutex::new(Vec::new())),
    });
    let server =
        MempalMcpServer::new_with_factory_and_config(db_path.clone(), config.clone(), factory);

    let accepted = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "candidate-keep".to_string(),
            wing: "code-memory".to_string(),
            room: Some("gating".to_string()),
            source: None,
            dry_run: Some(false),
            importance: None,
        }))
        .await
        .expect("accepted request")
        .0;
    let rejected = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "candidate-drop".to_string(),
            wing: "code-memory".to_string(),
            room: Some("gating".to_string()),
            source: None,
            dry_run: Some(false),
            importance: None,
        }))
        .await
        .expect("rejected request")
        .0;

    assert_eq!(config.ingest_gating.embedding_classifier.threshold, 0.8);
    assert_eq!(
        accepted
            .gating_decision
            .expect("accepted gating decision")
            .decision,
        "accepted"
    );
    let rejected_decision = rejected.gating_decision.expect("rejected gating decision");
    assert_eq!(rejected_decision.decision, "rejected");
    assert_eq!(rejected_decision.tier, 2);
    assert!(rejected_decision.score.expect("tier-2 score") < 0.8);
}

#[test]
fn test_prototype_init_failure_fails_fast() {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");

    let mut server = Server::new();
    let _mock = server
        .mock("POST", "/v1/embeddings")
        .with_status(500)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error":"prototype boom"}"#)
        .create();

    let config_path = mempal_home.join("config.toml");
    write_config(
        &config_path,
        &db_path,
        &format!(
            r#"
[embed]
backend = "openai_compat"

[embed.openai_compat]
base_url = "{}/v1"
model = "test-embed"
dim = 3
request_timeout_secs = 2

[hooks]
enabled = true
daemon_poll_interval_ms = 100

[daemon]
log_path = "{}"

[ingest_gating]
enabled = true

[ingest_gating.embedding_classifier]
enabled = true
threshold = 0.5
prototypes = ["accept-prototype"]
"#,
            server.url(),
            mempal_home.join("daemon.log").display()
        ),
    );

    let output = Command::new(mempal_bin())
        .args(["daemon", "--foreground"])
        .env("HOME", tmp.path())
        .stdin(Stdio::null())
        .output()
        .expect("run daemon");

    assert!(
        !output.status.success(),
        "daemon startup must fail when prototype init fails"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("gating prototype init failed"), "{stderr}");
    assert!(stderr.contains("accept-prototype"), "{stderr}");
    assert!(
        !mempal_home.join("daemon.pid").exists(),
        "pid file must not survive failed daemon startup"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_gating_decision_explain_field_structured() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");

    let config_path = mempal_home.join("config.toml");
    write_config(
        &config_path,
        &db_path,
        r#"
[ingest_gating]
enabled = true

[[ingest_gating.rules]]
action = "reject"
content_bytes_lt = 32

[ingest_gating.embedding_classifier]
enabled = true
threshold = 0.5
prototypes = ["accept-prototype"]
"#,
    );

    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
    let config = Config::load_from(&config_path).expect("load config");
    let factory = Arc::new(DeterministicEmbedderFactory {
        vectors: Arc::new(HashMap::from([(
            "accept-prototype".to_string(),
            vec![1.0, 0.0],
        )])),
        default_vector: vec![0.0, 1.0],
        embed_calls: Arc::new(Mutex::new(Vec::new())),
    });
    let server = MempalMcpServer::new_with_factory_and_config(db_path, config, factory);

    let response = server
        .mempal_ingest(Parameters(IngestRequest {
            content: "reject me".to_string(),
            wing: "code-memory".to_string(),
            room: Some("gating".to_string()),
            source: None,
            dry_run: Some(false),
            importance: None,
        }))
        .await
        .expect("request")
        .0;

    let decision = response.gating_decision.expect("gating decision");
    let json = serde_json::to_value(&decision).expect("serialize gating decision");
    assert_eq!(json.get("tier").and_then(|value| value.as_u64()), Some(1));
    assert_eq!(
        json.get("matched_pattern").and_then(|value| value.as_str()),
        Some("content_bytes_lt")
    );
    assert!(
        json.get("score").is_none_or(|value| value.is_null()),
        "tier-1 rule reject should not fabricate a cosine score: {json}"
    );
}
