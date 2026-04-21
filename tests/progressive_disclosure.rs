use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mempal::core::config::ConfigHandle;
use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use mempal::embed::{EmbedError, Embedder, EmbedderFactory};
use mempal::mcp::{MempalMcpServer, SearchRequest};
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
struct StaticEmbedderFactory {
    vector: Vec<f32>,
}

struct StaticEmbedder {
    vector: Vec<f32>,
}

#[async_trait]
impl EmbedderFactory for StaticEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(StaticEmbedder {
            vector: self.vector.clone(),
        }))
    }
}

#[async_trait]
impl Embedder for StaticEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| self.vector.clone()).collect())
    }

    fn dimensions(&self) -> usize {
        self.vector.len()
    }

    fn name(&self) -> &str {
        "static"
    }
}

struct TestEnv {
    _tmp: TempDir,
    config_path: PathBuf,
    db_path: PathBuf,
}

impl TestEnv {
    fn new(
        project_id: Option<&str>,
        strict_project_isolation: bool,
        progressive_disclosure: bool,
        preview_chars: usize,
    ) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let config_path = mempal_home.join("config.toml");
        let db_path = mempal_home.join("palace.db");
        write_config_atomic(
            &config_path,
            &config_text(
                &db_path,
                project_id,
                strict_project_isolation,
                progressive_disclosure,
                preview_chars,
            ),
        );
        Database::open(&db_path).expect("open db");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        Self {
            _tmp: tmp,
            config_path,
            db_path,
        }
    }

    fn server(&self) -> MempalMcpServer {
        MempalMcpServer::new_with_factory(
            self.db_path.clone(),
            Arc::new(StaticEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
            }),
        )
    }
}

fn config_text(
    db_path: &Path,
    project_id: Option<&str>,
    strict_project_isolation: bool,
    progressive_disclosure: bool,
    preview_chars: usize,
) -> String {
    let project_section = project_id
        .map(|project_id| format!("\n[project]\nid = \"{project_id}\"\n"))
        .unwrap_or_default();
    format!(
        r#"
db_path = "{}"
{}
[embedder]
backend = "api"
base_url = "http://127.0.0.1:9/v1/"
api_model = "test-model"

[config_hot_reload]
enabled = true
debounce_ms = 250
poll_fallback_secs = 1

[search]
strict_project_isolation = {}
progressive_disclosure = {}
preview_chars = {}
"#,
        db_path.display(),
        project_section,
        strict_project_isolation,
        progressive_disclosure,
        preview_chars
    )
}

fn write_config_atomic(path: &Path, contents: &str) {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents).expect("write temp config");
    fs::rename(&tmp, path).expect("rename config");
}

fn wait_for_config_version_change(previous: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let current = ConfigHandle::version();
        if current != previous {
            return current;
        }
        assert!(Instant::now() < deadline, "config version did not change");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn insert_drawer(
    db_path: &Path,
    id: &str,
    content: &str,
    wing: &str,
    room: Option<&str>,
    source_file: &str,
    project_id: Option<&str>,
) {
    let db = Database::open(db_path).expect("open db");
    db.insert_drawer(&Drawer {
        id: id.to_string(),
        content: content.to_string(),
        wing: wing.to_string(),
        room: room.map(str::to_string),
        source_file: Some(source_file.to_string()),
        source_type: SourceType::Manual,
        added_at: "1713000000".to_string(),
        chunk_index: Some(0),
        importance: 4,
    })
    .expect("insert drawer");
    db.insert_vector(id, &[0.1, 0.2, 0.3])
        .expect("insert vector");
    db.conn()
        .execute(
            "UPDATE drawers SET project_id = ?2 WHERE id = ?1",
            rusqlite::params![id, project_id],
        )
        .expect("update drawer project");
    db.conn()
        .execute(
            "UPDATE drawer_vectors SET project_id = ?2 WHERE id = ?1",
            rusqlite::params![id, project_id],
        )
        .expect("update vector project");
}

async fn search_response_json(server: &MempalMcpServer, query: &str) -> serde_json::Value {
    let response = server
        .mempal_search(Parameters(SearchRequest {
            query: query.to_string(),
            wing: None,
            room: None,
            top_k: Some(10),
            project_id: None,
            include_global: None,
            all_projects: None,
            disable_progressive: None,
        }))
        .await
        .expect("search should succeed")
        .0;
    serde_json::to_value(response).expect("serialize response")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_search_result_has_content_truncated_field() {
    let _guard = config_guard().await;
    let env = TestEnv::new(None, false, true, 32);
    insert_drawer(
        &env.db_path,
        "drawer-short",
        "short content stays verbatim",
        "code",
        Some("preview"),
        "/tmp/short.md",
        None,
    );

    let json = search_response_json(&env.server(), "short").await;
    let result = &json["results"][0];

    assert!(
        result.get("content_truncated").is_some(),
        "missing content_truncated field: {json}"
    );
    assert!(
        result.get("original_content_bytes").is_some(),
        "missing original_content_bytes field: {json}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_search_preview_length_cap_applied() {
    let _guard = config_guard().await;
    let env = TestEnv::new(None, false, true, 24);
    let content = "Decision: adopt deterministic replay for queue recovery because replay order is part of the audit contract and must stay stable across restarts.";
    insert_drawer(
        &env.db_path,
        "drawer-long",
        content,
        "code",
        Some("preview"),
        "/tmp/long.md",
        None,
    );

    let json = search_response_json(&env.server(), "deterministic").await;
    let result = &json["results"][0];
    let preview = result["content"].as_str().expect("content string");

    assert!(preview.chars().count() <= 25, "preview too long: {preview}");
    assert_eq!(result["content_truncated"], true);
    assert_eq!(
        result["original_content_bytes"].as_u64(),
        Some(content.len() as u64)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_search_short_content_untruncated() {
    let _guard = config_guard().await;
    let env = TestEnv::new(None, false, true, 120);
    let content = "short drawer stays whole";
    insert_drawer(
        &env.db_path,
        "drawer-short",
        content,
        "code",
        Some("preview"),
        "/tmp/short.md",
        None,
    );

    let json = search_response_json(&env.server(), "whole").await;
    let result = &json["results"][0];

    assert_eq!(result["content"], content);
    assert_eq!(result["content_truncated"], false);
    assert_eq!(
        result["original_content_bytes"].as_u64(),
        Some(content.len() as u64)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_mempal_read_drawer_returns_raw_full_content() {
    let _guard = config_guard().await;
    let env = TestEnv::new(None, false, true, 16);
    let content = "Decision: keep the original raw drawer content intact even when previews are truncated in search.";
    insert_drawer(
        &env.db_path,
        "drawer-read",
        content,
        "code",
        Some("preview"),
        "/tmp/read.md",
        None,
    );

    let response = env
        .server()
        .mempal_read_drawer(Parameters(mempal::mcp::ReadDrawerRequest {
            drawer_id: "drawer-read".to_string(),
        }))
        .await
        .expect("read drawer should succeed")
        .0;

    assert_eq!(response.drawer_id, "drawer-read");
    assert_eq!(response.content, content);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_mempal_read_drawer_respects_project_isolation() {
    let _guard = config_guard().await;
    let env = TestEnv::new(Some("proj-A"), true, true, 32);
    insert_drawer(
        &env.db_path,
        "drawer-b",
        "project B content should not leak",
        "code",
        Some("preview"),
        "/tmp/b.md",
        Some("proj-B"),
    );

    let result = env
        .server()
        .mempal_read_drawer(Parameters(mempal::mcp::ReadDrawerRequest {
            drawer_id: "drawer-b".to_string(),
        }))
        .await;
    let error = match result {
        Ok(_) => panic!("cross-project read should fail"),
        Err(error) => error,
    };

    assert!(
        error.to_string().contains("project"),
        "expected project isolation error, got: {error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_progressive_disclosure_hot_reload_applies_without_restart() {
    let _guard = config_guard().await;
    let env = TestEnv::new(None, false, true, 16);
    let content = "Decision: the preview cap can change at runtime and the next search must observe the updated value.";
    insert_drawer(
        &env.db_path,
        "drawer-hot",
        content,
        "code",
        Some("preview"),
        "/tmp/hot.md",
        None,
    );

    let before = search_response_json(&env.server(), "preview").await;
    let before_preview = before["results"][0]["content"]
        .as_str()
        .expect("before preview")
        .to_string();
    assert!(before_preview.chars().count() <= 17);

    let previous = ConfigHandle::version();
    write_config_atomic(
        &env.config_path,
        &config_text(&env.db_path, None, false, true, 48),
    );
    wait_for_config_version_change(&previous);

    let after = search_response_json(&env.server(), "preview").await;
    let after_preview = after["results"][0]["content"]
        .as_str()
        .expect("after preview")
        .to_string();

    assert!(
        after_preview.chars().count() > before_preview.chars().count(),
        "preview cap did not grow after hot reload: before={before_preview:?} after={after_preview:?}"
    );
}
