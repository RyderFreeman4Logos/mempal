#![cfg(feature = "rest")]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header::CONTENT_TYPE};
use mempal::api::ApiState;
use mempal::core::config::ConfigHandle;
use mempal::core::db::Database;
use mempal::core::types::{
    Drawer, KnowledgeStatus, MemoryDomain, MemoryKind, SourceType, default_confidence,
};
use mempal::core::utils::current_timestamp;
use mempal::embed::{EmbedError, Embedder, EmbedderFactory, global_embed_status};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct TestEnv {
    _tmp: TempDir,
    db_path: PathBuf,
    config_path: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
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

[search]
bm25_fallback = true

[embed.degradation]
degrade_after_n_failures = 1
block_writes_when_degraded = true

[api]
write_queue_capacity = 10
write_drain_timeout_secs = 2
"#,
                db_path.display()
            ),
        )
        .expect("write config");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        global_embed_status().reset_for_tests();
        Self {
            _tmp: tmp,
            db_path,
            config_path,
        }
    }

    fn db(&self) -> Database {
        Database::open(&self.db_path).expect("open db")
    }

    fn state(&self, factory: Arc<dyn EmbedderFactory>) -> ApiState {
        ApiState::with_write_queue_config(self.db_path.clone(), factory, 10, Duration::from_secs(2))
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        global_embed_status().reset_for_tests();
        let _ = ConfigHandle::bootstrap(&self.config_path);
    }
}

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
        Ok(texts.iter().map(|_| vec![0.1; self.dim]).collect())
    }

    fn dimensions(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &str {
        "static-typed-rest-test"
    }
}

#[derive(Clone)]
struct FailingEmbedderFactory;

struct FailingEmbedder;

#[async_trait]
impl EmbedderFactory for FailingEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(FailingEmbedder))
    }
}

#[async_trait]
impl Embedder for FailingEmbedder {
    async fn embed(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Err(EmbedError::Runtime("embedder down".to_string()))
    }

    fn dimensions(&self) -> usize {
        4
    }

    fn name(&self) -> &str {
        "failing-typed-rest-test"
    }
}

async fn post_json(state: ApiState, uri: &str, body: Value) -> (StatusCode, Value) {
    let response = mempal::api::router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&body).expect("serialize body"),
                ))
                .expect("build request"),
        )
        .await
        .expect("REST request");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body = serde_json::from_slice(&bytes).expect("parse json");
    (status, body)
}

async fn get_json(state: ApiState, uri: &str) -> (StatusCode, Value) {
    let response = mempal::api::router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("REST request");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body = serde_json::from_slice(&bytes).expect("parse json");
    (status, body)
}

struct PinnedDrawerArgs<'a> {
    id: &'a str,
    content: &'a str,
    pin_order: i64,
    importance: i32,
    wing: &'a str,
    room: Option<&'a str>,
    domain: MemoryDomain,
    status: KnowledgeStatus,
}

fn insert_pinned_drawer(db: &Database, args: PinnedDrawerArgs<'_>) {
    let PinnedDrawerArgs {
        id,
        content,
        pin_order,
        importance,
        wing,
        room,
        domain,
        status,
    } = args;
    let source_type = SourceType::AgentObservation;
    let drawer = Drawer {
        id: id.to_string(),
        content: content.to_string(),
        wing: wing.to_string(),
        room: room.map(str::to_string),
        source_file: Some(format!("tests://{id}")),
        source_type,
        confidence: default_confidence(source_type),
        added_at: current_timestamp(),
        importance,
        memory_kind: MemoryKind::ProfileFact,
        domain,
        field: "test-field".to_string(),
        status: Some(status),
        is_pinned: true,
        pin_order: Some(pin_order),
        ..Drawer::default()
    };
    db.insert_drawer(&drawer).expect("insert drawer");
}

#[tokio::test]
async fn test_typed_ingest_persists_memory_kind() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));

    let (status, body) = post_json(
        state,
        "/api/ingest",
        json!({
            "content": "Prefer CLI dashboards over web UIs.",
            "wing": "profile",
            "room": "facts",
            "memory_kind": "profile_fact",
            "domain": "user",
            "field": "preferences",
            "importance": 4,
            "is_pinned": true,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "body={body}");
    let drawer_id = body["drawer_id"].as_str().expect("drawer_id in response");
    let db = env.db();
    let drawer = db
        .get_drawer(drawer_id)
        .expect("load drawer")
        .expect("drawer exists");
    assert_eq!(drawer.memory_kind, MemoryKind::ProfileFact);
    assert_eq!(drawer.domain, MemoryDomain::User);
    assert_eq!(drawer.field, "preferences");
    assert_eq!(drawer.importance, 4);
    assert!(drawer.is_pinned);
}

#[tokio::test]
async fn test_typed_ingest_status_and_tier() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));

    let (status, body) = post_json(
        state,
        "/api/ingest",
        json!({
            "content": "All writes require a review gate.",
            "wing": "policy",
            "room": "rules",
            "memory_kind": "knowledge",
            "domain": "project",
            "field": "engineering",
            "status": "canonical",
            "tier": "dao_tian",
            "statement": "All writes require a review gate.",
            "supporting_refs": [],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "body={body}");
    let drawer_id = body["drawer_id"].as_str().expect("drawer_id");
    let db = env.db();
    let drawer = db
        .get_drawer(drawer_id)
        .expect("load drawer")
        .expect("drawer exists");
    assert_eq!(drawer.memory_kind, MemoryKind::Knowledge);
    assert_eq!(drawer.domain, MemoryDomain::Project);
    assert!(drawer.statement.is_some());
}

#[tokio::test]
async fn test_typed_ingest_defaults_unchanged() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));

    let (status, body) = post_json(
        state,
        "/api/ingest",
        json!({
            "content": "Plain evidence drawer with no typed fields.",
            "wing": "project",
            "room": "notes",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "body={body}");
    let drawer_id = body["drawer_id"].as_str().expect("drawer_id");
    let db = env.db();
    let drawer = db
        .get_drawer(drawer_id)
        .expect("load drawer")
        .expect("drawer exists");
    assert_eq!(drawer.memory_kind, MemoryKind::Evidence);
    assert_eq!(drawer.domain, MemoryDomain::Project);
    assert_eq!(drawer.field, "general");
    assert!(!drawer.is_pinned);
    assert_eq!(drawer.importance, 0);
}

#[tokio::test]
async fn test_typed_ingest_invalid_memory_kind_returns_400() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));

    let (status, _body) = post_json(
        state,
        "/api/ingest",
        json!({
            "content": "Some content.",
            "wing": "test",
            "memory_kind": "invalid_kind",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_typed_ingest_invalid_memory_kind_returns_400_when_writes_degraded() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));
    global_embed_status().record_failure(&"synthetic degraded state");
    assert!(global_embed_status().should_block_writes());

    let (status, body) = post_json(
        state,
        "/api/ingest",
        json!({
            "content": "Some content.",
            "wing": "test",
            "memory_kind": "invalid_kind",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
}

#[tokio::test]
async fn test_pinned_facts_endpoint_returns_pinned_drawers() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let db = env.db();
    insert_pinned_drawer(
        &db,
        PinnedDrawerArgs {
            id: "drawer_rest_pinned_a",
            content: "First pinned fact for REST test.",
            pin_order: 0,
            importance: 5,
            wing: "profile",
            room: Some("facts"),
            domain: MemoryDomain::User,
            status: KnowledgeStatus::Active,
        },
    );
    insert_pinned_drawer(
        &db,
        PinnedDrawerArgs {
            id: "drawer_rest_pinned_b",
            content: "Second pinned fact for REST test.",
            pin_order: 1,
            importance: 3,
            wing: "project",
            room: Some("decisions"),
            domain: MemoryDomain::Project,
            status: KnowledgeStatus::Canonical,
        },
    );
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));

    let (status, body) = get_json(state, "/api/pinned_facts").await;

    assert_eq!(status, StatusCode::OK, "body={body}");
    let facts = body.as_array().expect("array response");
    assert_eq!(facts.len(), 2);
    let ids: Vec<&str> = facts
        .iter()
        .filter_map(|f| f["drawer_id"].as_str())
        .collect();
    assert!(ids.contains(&"drawer_rest_pinned_a"), "missing a");
    assert!(ids.contains(&"drawer_rest_pinned_b"), "missing b");
}

#[tokio::test]
async fn test_pinned_facts_wing_filter() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let db = env.db();
    insert_pinned_drawer(
        &db,
        PinnedDrawerArgs {
            id: "drawer_rest_pinned_wing_a",
            content: "Pinned in profile wing.",
            pin_order: 0,
            importance: 4,
            wing: "profile",
            room: None,
            domain: MemoryDomain::User,
            status: KnowledgeStatus::Active,
        },
    );
    insert_pinned_drawer(
        &db,
        PinnedDrawerArgs {
            id: "drawer_rest_pinned_wing_b",
            content: "Pinned in project wing.",
            pin_order: 1,
            importance: 4,
            wing: "project",
            room: None,
            domain: MemoryDomain::Project,
            status: KnowledgeStatus::Active,
        },
    );
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));

    let (status, body) = get_json(state, "/api/pinned_facts?wing=profile").await;

    assert_eq!(status, StatusCode::OK, "body={body}");
    let facts = body.as_array().expect("array");
    assert_eq!(facts.len(), 1);
    assert_eq!(
        facts[0]["drawer_id"].as_str(),
        Some("drawer_rest_pinned_wing_a")
    );
}

#[tokio::test]
async fn test_pinned_facts_domain_filter() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let db = env.db();
    insert_pinned_drawer(
        &db,
        PinnedDrawerArgs {
            id: "drawer_dom_user",
            content: "User domain pinned.",
            pin_order: 0,
            importance: 4,
            wing: "profile",
            room: None,
            domain: MemoryDomain::User,
            status: KnowledgeStatus::Active,
        },
    );
    insert_pinned_drawer(
        &db,
        PinnedDrawerArgs {
            id: "drawer_dom_project",
            content: "Project domain pinned.",
            pin_order: 1,
            importance: 4,
            wing: "project",
            room: None,
            domain: MemoryDomain::Project,
            status: KnowledgeStatus::Active,
        },
    );
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));

    let (status, body) = get_json(state, "/api/pinned_facts?domain=user").await;

    assert_eq!(status, StatusCode::OK, "body={body}");
    let facts = body.as_array().expect("array");
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0]["drawer_id"].as_str(), Some("drawer_dom_user"));
    assert_eq!(facts[0]["domain"].as_str(), Some("user"));
}

#[tokio::test]
async fn test_pinned_facts_response_includes_metadata() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let db = env.db();
    insert_pinned_drawer(
        &db,
        PinnedDrawerArgs {
            id: "drawer_meta_check",
            content: "Check that metadata is in response.",
            pin_order: 0,
            importance: 5,
            wing: "profile",
            room: Some("facts"),
            domain: MemoryDomain::User,
            status: KnowledgeStatus::Canonical,
        },
    );
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));

    let (status, body) = get_json(state, "/api/pinned_facts").await;

    assert_eq!(status, StatusCode::OK);
    let facts = body.as_array().expect("array");
    assert_eq!(facts.len(), 1);
    let f = &facts[0];
    assert_eq!(f["drawer_id"].as_str(), Some("drawer_meta_check"));
    assert_eq!(f["memory_kind"].as_str(), Some("profile_fact"));
    assert_eq!(f["domain"].as_str(), Some("user"));
    assert_eq!(f["field"].as_str(), Some("test-field"));
    assert_eq!(f["importance"].as_i64(), Some(5));
    assert_eq!(f["pin_order"].as_i64(), Some(0));
    assert_eq!(f["status"].as_str(), Some("canonical"));
    assert!(f["added_at"].as_str().is_some());
}

#[tokio::test]
async fn test_search_response_includes_typed_fields() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let db = env.db();
    let source_type = SourceType::UserExplicit;
    let drawer = Drawer {
        id: "drawer_search_typed".to_string(),
        content: "typed search test content unique phrase".to_string(),
        wing: "test".to_string(),
        room: Some("typed".to_string()),
        source_file: Some("tests://typed-search".to_string()),
        source_type,
        confidence: default_confidence(source_type),
        added_at: current_timestamp(),
        importance: 3,
        memory_kind: MemoryKind::ProfileFact,
        domain: MemoryDomain::User,
        field: "search-field".to_string(),
        is_pinned: true,
        ..Drawer::default()
    };
    db.insert_drawer(&drawer).expect("insert drawer");
    // Use FailingEmbedderFactory to force BM25 fallback; sqlite-vec isn't
    // available in integration test context for vector search.
    for _ in 0..10 {
        global_embed_status().record_failure(&EmbedError::Runtime("down".to_string()));
    }
    let state = env.state(Arc::new(FailingEmbedderFactory));

    let (status, body) = get_json(state, "/api/search?q=typed+search+test+content&top_k=5").await;

    assert_eq!(status, StatusCode::OK, "search 500 body={body}");
    let results = body.as_array().expect("results array");
    let result = results
        .iter()
        .find(|r| r["drawer_id"].as_str() == Some("drawer_search_typed"))
        .expect("typed drawer in results");
    assert_eq!(result["memory_kind"].as_str(), Some("profile_fact"));
    assert_eq!(result["domain"].as_str(), Some("user"));
    assert_eq!(result["field"].as_str(), Some("search-field"));
    assert_eq!(result["importance"].as_i64(), Some(3));
    assert_eq!(result["is_pinned"].as_bool(), Some(true));
}

#[tokio::test]
async fn test_delete_soft_deletes_drawer() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));

    let (ingest_status, ingest_body) = post_json(
        state.clone(),
        "/api/ingest",
        json!({
            "content": "Drawer to be deleted via REST.",
            "wing": "test",
            "room": "delete-test",
        }),
    )
    .await;
    assert_eq!(
        ingest_status,
        StatusCode::CREATED,
        "ingest body={ingest_body}"
    );
    let drawer_id = ingest_body["drawer_id"]
        .as_str()
        .expect("drawer_id in ingest response")
        .to_string();

    let (del_status, del_body) =
        post_json(state, "/api/delete", json!({ "drawer_id": drawer_id })).await;
    assert_eq!(del_status, StatusCode::OK, "delete body={del_body}");
    assert_eq!(del_body["deleted"].as_bool(), Some(true));

    let db = env.db();
    let deleted_at: Option<String> = db
        .conn()
        .query_row(
            "SELECT deleted_at FROM drawers WHERE id = ?1",
            [&drawer_id],
            |row| row.get(0),
        )
        .expect("query drawer");
    assert!(
        deleted_at.is_some(),
        "drawer should have deleted_at set after soft-delete"
    );
}

#[tokio::test]
async fn test_delete_not_found_returns_404() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::new();
    let state = env.state(Arc::new(StaticEmbedderFactory { dim: 4 }));

    let (status, body) = post_json(
        state,
        "/api/delete",
        json!({ "drawer_id": "drawer_does_not_exist_xyz" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body={body}");
}
