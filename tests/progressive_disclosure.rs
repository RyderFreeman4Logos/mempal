use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mempal::core::config::ConfigHandle;
use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use mempal::embed::{EmbedError, Embedder, EmbedderFactory};
use mempal::mcp::{
    MAX_READ_DRAWERS_MAX_COUNT, MAX_READ_DRAWERS_REQUEST_IDS, MempalMcpServer, ReadDrawerRequest,
    ReadDrawersRequest, SearchRequest, SearchResponse,
};
use rmcp::ServerHandler;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ErrorCode;
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
    fn new(progressive_disclosure: bool, preview_chars: usize) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let config_path = mempal_home.join("config.toml");
        let db_path = mempal_home.join("palace.db");
        write_config(
            &config_path,
            &config_text(&db_path, progressive_disclosure, preview_chars),
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

fn config_text(db_path: &Path, progressive_disclosure: bool, preview_chars: usize) -> String {
    format!(
        r#"
db_path = "{}"

[embedder]
backend = "api"
base_url = "http://127.0.0.1:9/v1/"
api_model = "test-model"

[config_hot_reload]
enabled = true
debounce_ms = 250
poll_fallback_secs = 1

[search]
strict_project_isolation = false
progressive_disclosure = {}
preview_chars = {}
"#,
        db_path.display(),
        progressive_disclosure,
        preview_chars
    )
}

fn write_config(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write config");
}

fn write_config_atomic(path: &Path, contents: &str) {
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, contents).expect("write temp config");
    fs::rename(&tmp_path, path).expect("rename config atomically");
}

fn wait_until(
    timeout: std::time::Duration,
    step: std::time::Duration,
    mut predicate: impl FnMut() -> bool,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(step);
    }
    predicate()
}

fn wait_for_version_change(previous: &str) -> String {
    let mut current = ConfigHandle::version();
    let changed = wait_until(
        std::time::Duration::from_secs(3),
        std::time::Duration::from_millis(50),
        || {
            current = ConfigHandle::version();
            current != previous
        },
    );
    assert!(changed, "config version did not change from {previous}");
    current
}

fn insert_drawer(db_path: &Path, id: &str, content: &str, source_file: &str, insert_vector: bool) {
    let db = Database::open(db_path).expect("open db");
    db.insert_drawer(&Drawer {
        id: id.to_string(),
        content: content.to_string(),
        wing: "code".to_string(),
        room: Some("preview".to_string()),
        source_file: Some(source_file.to_string()),
        source_type: SourceType::Manual,
        added_at: "1713000000".to_string(),
        chunk_index: Some(0),
        importance: 4,
    })
    .expect("insert drawer");
    if insert_vector {
        db.insert_vector(id, &[0.1, 0.2, 0.3])
            .expect("insert vector");
    }
}

async fn search(
    server: &MempalMcpServer,
    query: &str,
    disable_progressive: Option<bool>,
) -> SearchResponse {
    server
        .mempal_search(Parameters(SearchRequest {
            query: query.to_string(),
            wing: None,
            room: None,
            top_k: Some(10),
            project_id: None,
            include_global: None,
            all_projects: None,
            disable_progressive,
        }))
        .await
        .expect("search should succeed")
        .0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cjk_truncation_utf8_safe() {
    let preview = mempal::search::preview::truncate(
        "系统决策：采用共享内存同步机制解决状态漂移问题的根本原因是并发安全",
        10,
    );

    assert!(preview.truncated);
    assert!(preview.content.ends_with('…'));
    assert!(preview.content.chars().count() <= 11);
    assert!(std::str::from_utf8(preview.content.as_bytes()).is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_disabled_returns_verbatim_content() {
    let _guard = config_guard().await;
    let env = TestEnv::new(false, 32);
    let content = "Decision: keep full verbatim content when progressive disclosure is disabled.";
    insert_drawer(
        &env.db_path,
        "drawer-disabled",
        content,
        "/tmp/disabled.md",
        true,
    );

    let response = search(&env.server(), "verbatim", None).await;
    let result = &response.results[0];

    assert_eq!(result.content, content);
    assert!(!result.content_truncated);
    assert_eq!(result.original_content_bytes, content.len() as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_enabled_truncates_content() {
    let _guard = config_guard().await;
    let env = TestEnv::new(true, 32);
    let content = "Decision: the disclosure preview should stop at a clean word boundary while still reporting the original byte count for the full drawer body.";
    insert_drawer(
        &env.db_path,
        "drawer-enabled",
        content,
        "/tmp/enabled.md",
        true,
    );

    let response = search(&env.server(), "disclosure", None).await;
    let result = &response.results[0];

    assert!(result.content_truncated);
    assert!(result.content.ends_with('…'));
    assert!(result.content.chars().count() <= 33);
    assert_eq!(result.original_content_bytes, content.len() as u64);
    assert_ne!(result.content, content);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_per_call_disable_progressive() {
    let _guard = config_guard().await;
    let env = TestEnv::new(true, 24);
    let content = "Decision: callers may opt out per request when they expect a tiny result set.";
    insert_drawer(
        &env.db_path,
        "drawer-override",
        content,
        "/tmp/override.md",
        true,
    );

    let response = search(&env.server(), "result", Some(true)).await;
    let result = &response.results[0];

    assert_eq!(result.content, content);
    assert!(!result.content_truncated);
    assert_eq!(result.original_content_bytes, content.len() as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_search_non_progressive_returns_raw_bytes_for_session_review_drawer() {
    let _guard = config_guard().await;
    let env = TestEnv::new(true, 24);
    let content = concat!(
        "Decision: keep the full session review drawer verbatim when callers disable preview mode.",
        "\n\n<!-- mempal:session-review -->\n",
        "linked_drawer_ids: [\"drawer-a\",\"drawer-b\"]\n",
        "session_id: \"sess-raw\""
    );
    insert_drawer(
        &env.db_path,
        "drawer-session-review-raw",
        content,
        "sess-raw",
        true,
    );

    let response = search(&env.server(), "verbatim", Some(true)).await;
    let result = response
        .results
        .iter()
        .find(|result| result.drawer_id == "drawer-session-review-raw")
        .expect("session review result");

    assert_eq!(result.content, content);
    assert!(result.content.contains("<!-- mempal:session-review -->"));
    assert_eq!(result.original_content_bytes, content.len() as u64);
    assert!(!result.content_truncated);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_read_drawer_not_found() {
    let _guard = config_guard().await;
    let env = TestEnv::new(true, 24);
    let error = env
        .server()
        .mempal_read_drawer(Parameters(ReadDrawerRequest {
            drawer_id: "missing-drawer".to_string(),
            project_id: None,
            include_global: None,
            all_projects: None,
        }))
        .await;
    let error = match error {
        Ok(_) => panic!("missing drawer should return error"),
        Err(error) => error,
    };

    assert_eq!(error.code, ErrorCode::RESOURCE_NOT_FOUND);
    assert_eq!(error.message, "drawer not found");
    assert_eq!(
        error.data,
        Some(serde_json::json!({
            "error": "not_found",
            "drawer_id": "missing-drawer",
        }))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_read_drawer_returns_full_verbatim() {
    let _guard = config_guard().await;
    let env = TestEnv::new(true, 16);
    let content = "Decision: mempal_read_drawer is the escape hatch and therefore always returns raw content.";
    insert_drawer(&env.db_path, "drawer-read", content, "/tmp/read.md", true);

    let response = env
        .server()
        .mempal_read_drawer(Parameters(ReadDrawerRequest {
            drawer_id: "drawer-read".to_string(),
            project_id: None,
            include_global: None,
            all_projects: None,
        }))
        .await
        .expect("read drawer should succeed")
        .0;

    assert_eq!(response.drawer_id, "drawer-read");
    assert_eq!(response.content, content);
    assert!(!response.content_truncated);
    assert_eq!(response.original_content_bytes, content.len() as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_read_drawers_batch_with_not_found() {
    let _guard = config_guard().await;
    let env = TestEnv::new(true, 16);
    insert_drawer(&env.db_path, "drawer-a", "alpha", "/tmp/a.md", false);
    insert_drawer(&env.db_path, "drawer-b", "beta", "/tmp/b.md", false);

    let small = env
        .server()
        .mempal_read_drawers(Parameters(ReadDrawersRequest {
            drawer_ids: vec![
                "drawer-a".to_string(),
                "missing-small".to_string(),
                "drawer-b".to_string(),
            ],
            max_count: Some(10),
            project_id: None,
            include_global: None,
            all_projects: None,
        }))
        .await
        .expect("small batch read should succeed")
        .0;

    assert_eq!(small.drawers.len(), 2);
    assert_eq!(
        small
            .drawers
            .iter()
            .map(|drawer| drawer.drawer_id.as_str())
            .collect::<Vec<_>>(),
        vec!["drawer-a", "drawer-b"]
    );
    assert_eq!(small.not_found, vec!["missing-small".to_string()]);
    assert!(small.warnings.is_empty());

    let truncated = env
        .server()
        .mempal_read_drawers(Parameters(ReadDrawersRequest {
            drawer_ids: vec![
                "drawer-a".to_string(),
                "missing-truncated".to_string(),
                "drawer-b".to_string(),
            ],
            max_count: Some(2),
            project_id: None,
            include_global: None,
            all_projects: None,
        }))
        .await
        .expect("truncated batch read should succeed")
        .0;

    assert_eq!(truncated.drawers.len(), 1);
    assert_eq!(truncated.drawers[0].drawer_id, "drawer-a");
    assert_eq!(truncated.not_found, vec!["missing-truncated".to_string()]);
    assert_eq!(
        truncated.warnings,
        vec![
            "truncated_to_max_count: requested 3 unique drawer_ids, processed first 2 due to max_count=2"
                .to_string()
        ]
    );

    let mut large_ids = Vec::with_capacity(1500);
    for index in 0..1500usize {
        let drawer_id = format!("drawer-large-{index:04}");
        insert_drawer(
            &env.db_path,
            &drawer_id,
            &format!("large batch content {index}"),
            &format!("/tmp/{drawer_id}.md"),
            false,
        );
        large_ids.push(drawer_id);
    }

    let large = env
        .server()
        .mempal_read_drawers(Parameters(ReadDrawersRequest {
            drawer_ids: large_ids.clone(),
            max_count: Some(2000),
            project_id: None,
            include_global: None,
            all_projects: None,
        }))
        .await
        .expect("large batch read should succeed")
        .0;

    assert_eq!(large.drawers.len(), 1500);
    assert!(large.not_found.is_empty());
    assert!(large.warnings.is_empty());
    assert_eq!(large.drawers[0].drawer_id, large_ids[0]);
    assert_eq!(large.drawers[1499].drawer_id, large_ids[1499]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_read_drawers_rejects_oversized_input() {
    let _guard = config_guard().await;
    let env = TestEnv::new(true, 16);
    let drawer_ids = (0..=MAX_READ_DRAWERS_REQUEST_IDS)
        .map(|index| format!("drawer-{index:05}"))
        .collect::<Vec<_>>();

    let error = env
        .server()
        .mempal_read_drawers(Parameters(ReadDrawersRequest {
            drawer_ids,
            max_count: Some(20),
            project_id: None,
            include_global: None,
            all_projects: None,
        }))
        .await;
    let error = match error {
        Ok(_) => panic!("oversized drawer_ids request must be rejected"),
        Err(error) => error,
    };

    assert_eq!(error.code, ErrorCode::INVALID_REQUEST);
    assert!(
        error
            .message
            .contains(&MAX_READ_DRAWERS_REQUEST_IDS.to_string()),
        "expected limit in error message, got: {}",
        error.message
    );
    assert_eq!(
        error.data,
        Some(serde_json::json!({
            "error": "invalid_request",
            "field": "drawer_ids",
            "requested": MAX_READ_DRAWERS_REQUEST_IDS + 1,
            "max_allowed": MAX_READ_DRAWERS_REQUEST_IDS,
        }))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_read_drawers_rejects_oversized_max_count() {
    let _guard = config_guard().await;
    let env = TestEnv::new(true, 16);

    let error = env
        .server()
        .mempal_read_drawers(Parameters(ReadDrawersRequest {
            drawer_ids: vec!["drawer-a".to_string()],
            max_count: Some((MAX_READ_DRAWERS_MAX_COUNT + 1) as u32),
            project_id: None,
            include_global: None,
            all_projects: None,
        }))
        .await;
    let error = match error {
        Ok(_) => panic!("oversized max_count must be rejected"),
        Err(error) => error,
    };

    assert_eq!(error.code, ErrorCode::INVALID_REQUEST);
    assert!(
        error
            .message
            .contains(&MAX_READ_DRAWERS_MAX_COUNT.to_string()),
        "expected limit in error message, got: {}",
        error.message
    );
    assert_eq!(
        error.data,
        Some(serde_json::json!({
            "error": "invalid_request",
            "field": "max_count",
            "requested": MAX_READ_DRAWERS_MAX_COUNT + 1,
            "max_allowed": MAX_READ_DRAWERS_MAX_COUNT,
        }))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_server_info_injects_rule_10_when_active() {
    let _guard = config_guard().await;
    let env = TestEnv::new(true, 16);
    let info = <MempalMcpServer as ServerHandler>::get_info(&env.server());
    let encoded = serde_json::to_value(&info).expect("serialize server info");

    assert!(
        encoded["instructions"]
            .as_str()
            .expect("instructions string")
            .contains("RULE 10 (progressive disclosure)")
    );
    assert!(
        encoded["instructions"]
            .as_str()
            .expect("instructions string")
            .contains("mempal_read_drawer")
    );
    assert!(
        encoded["instructions"]
            .as_str()
            .expect("instructions string")
            .contains("content_truncated")
    );
    assert_eq!(
        encoded["capabilities"]["experimental"]["mempal"]["progressive_disclosure_active"],
        serde_json::Value::Bool(true)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_server_info_omits_rule_10_when_inactive() {
    let _guard = config_guard().await;
    let env = TestEnv::new(false, 16);
    let info = <MempalMcpServer as ServerHandler>::get_info(&env.server());
    let encoded = serde_json::to_value(&info).expect("serialize server info");

    assert!(
        !encoded["instructions"]
            .as_str()
            .expect("instructions string")
            .contains("RULE 10 (progressive disclosure)")
    );
    assert_eq!(
        encoded["capabilities"]["experimental"]["mempal"]["progressive_disclosure_active"],
        serde_json::Value::Bool(false)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_server_info_reflects_hot_reloaded_progressive_disclosure_state() {
    let _guard = config_guard().await;
    let env = TestEnv::new(false, 16);
    let server = env.server();

    let initial = serde_json::to_value(<MempalMcpServer as ServerHandler>::get_info(&server))
        .expect("serialize initial server info");
    assert_eq!(
        initial["capabilities"]["experimental"]["mempal"]["progressive_disclosure_active"],
        serde_json::Value::Bool(false)
    );
    assert!(
        !initial["instructions"]
            .as_str()
            .expect("initial instructions string")
            .contains("RULE 10 (progressive disclosure)")
    );

    let initial_version = ConfigHandle::version();
    write_config_atomic(&env.config_path, &config_text(&env.db_path, true, 16));
    let enabled_version = wait_for_version_change(&initial_version);

    let enabled = serde_json::to_value(<MempalMcpServer as ServerHandler>::get_info(&server))
        .expect("serialize enabled server info");
    assert_eq!(
        enabled["capabilities"]["experimental"]["mempal"]["progressive_disclosure_active"],
        serde_json::Value::Bool(true)
    );
    assert!(
        enabled["instructions"]
            .as_str()
            .expect("enabled instructions string")
            .contains("RULE 10 (progressive disclosure)")
    );

    write_config_atomic(&env.config_path, &config_text(&env.db_path, false, 16));
    wait_for_version_change(&enabled_version);

    let disabled_again =
        serde_json::to_value(<MempalMcpServer as ServerHandler>::get_info(&server))
            .expect("serialize disabled-again server info");
    assert_eq!(
        disabled_again["capabilities"]["experimental"]["mempal"]["progressive_disclosure_active"],
        serde_json::Value::Bool(false)
    );
    assert!(
        !disabled_again["instructions"]
            .as_str()
            .expect("disabled-again instructions string")
            .contains("RULE 10 (progressive disclosure)")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_short_content_not_truncated() {
    let _guard = config_guard().await;
    let env = TestEnv::new(true, 120);
    let content = "short drawer stays whole";
    insert_drawer(&env.db_path, "drawer-short", content, "/tmp/short.md", true);

    let response = search(&env.server(), "whole", None).await;
    let result = &response.results[0];

    assert_eq!(result.content, content);
    assert!(!result.content_truncated);
    assert_eq!(result.original_content_bytes, content.len() as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_signals_computed_from_full_content() {
    let _guard = config_guard().await;
    let env = TestEnv::new(true, 32);
    let content = "prefix ".repeat(40)
        + "We decided to keep signals computed from the full drawer content because the preview is only a projection.";
    insert_drawer(
        &env.db_path,
        "drawer-signals",
        &content,
        "/tmp/signals.md",
        true,
    );

    let response = search(&env.server(), "projection", None).await;
    let result = &response.results[0];

    assert!(result.content_truncated);
    assert!(!result.content.contains("decided"));
    assert!(result.flags.iter().any(|flag| flag == "DECISION"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_truncation_aligns_to_word_boundary() {
    let preview =
        mempal::search::preview::truncate("The quick brown fox jumps over the lazy dog", 20);

    assert!(preview.truncated);
    assert_eq!(preview.content, "The quick brown fox…");
}
