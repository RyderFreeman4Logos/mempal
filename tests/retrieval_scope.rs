use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use mempal::core::config::ConfigHandle;
use mempal::core::db::Database;
use mempal::core::project::ProjectSearchScope;
use mempal::core::types::{
    AnchorKind, Drawer, KnowledgeStatus, KnowledgeTier, MemoryDomain, MemoryKind, RouteDecision,
    SourceType,
};
use mempal::embed::{Embedder, EmbedderFactory};
use mempal::mcp::{MempalMcpServer, RetrievalScopeRequest, SearchRequest};
use mempal::observability::{self, VectorScanMode};
use mempal::search::{
    SearchFilters, SearchOptions, search_bm25_only_with_options,
    search_with_vector_and_scope_options,
};
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

struct StaticEmbedderFactory;

struct StaticEmbedder;

#[async_trait::async_trait]
impl EmbedderFactory for StaticEmbedderFactory {
    async fn build(&self) -> mempal::embed::Result<Box<dyn Embedder>> {
        Ok(Box::new(StaticEmbedder))
    }
}

#[async_trait::async_trait]
impl Embedder for StaticEmbedder {
    async fn embed(&self, texts: &[&str]) -> mempal::embed::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vector()).collect())
    }

    fn dimensions(&self) -> usize {
        vector().len()
    }

    fn name(&self) -> &str {
        "static"
    }
}

struct TestEnv {
    _tmp: TempDir,
    db_path: PathBuf,
}

impl TestEnv {
    fn new(project_id: Option<&str>) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db_path = mempal_home.join("palace.db");
        let config_path = mempal_home.join("config.toml");
        fs::write(&config_path, config(&db_path, project_id)).expect("write config");
        Database::open(&db_path).expect("open db");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        Self { _tmp: tmp, db_path }
    }

    fn db(&self) -> Database {
        Database::open(&self.db_path).expect("open db")
    }

    fn server(&self) -> MempalMcpServer {
        MempalMcpServer::new_with_factory(self.db_path.clone(), Arc::new(StaticEmbedderFactory))
            .expect("create server")
    }
}

fn config(db_path: &Path, project_id: Option<&str>) -> String {
    let project = project_id
        .map(|id| format!("\n[project]\nid = \"{id}\"\n"))
        .unwrap_or_default();
    format!(
        r#"[storage]
db_path = "{}"

[embed]
backend = "stub"
{}
"#,
        db_path.display(),
        project
    )
}

fn vector() -> Vec<f32> {
    vec![0.1, 0.2, 0.3]
}

fn route() -> RouteDecision {
    RouteDecision {
        wing: Some("scope".to_string()),
        room: None,
        confidence: 1.0,
        reason: "test".to_string(),
    }
}

fn base_drawer(id: &str, content: &str, room: Option<&str>) -> Drawer {
    Drawer {
        id: id.to_string(),
        content: content.to_string(),
        wing: "scope".to_string(),
        room: room.map(str::to_string),
        source_file: Some(format!("{id}.md")),
        source_type: SourceType::AgentInference,
        confidence: 0.9,
        added_at: "2026-01-01T00:00:00Z".to_string(),
        chunk_index: None,
        normalize_version: 1,
        importance: 3,
        memory_kind: MemoryKind::Evidence,
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        anchor_kind: AnchorKind::Repo,
        anchor_id: "repo://scope".to_string(),
        parent_anchor_id: None,
        provenance: None,
        statement: None,
        tier: None,
        status: None,
        supporting_refs: Vec::new(),
        counterexample_refs: Vec::new(),
        teaching_refs: Vec::new(),
        verification_refs: Vec::new(),
        scope_constraints: None,
        trigger_hints: None,
        is_pinned: false,
        pin_order: None,
        supersedes: None,
        effective_importance: 3.0,
        compacted_into: None,
    }
}

fn knowledge_drawer(id: &str, content: &str, status: KnowledgeStatus) -> Drawer {
    let mut drawer = base_drawer(id, content, Some("knowledge-session"));
    drawer.memory_kind = MemoryKind::Knowledge;
    drawer.field = "tooling".to_string();
    drawer.statement = Some(content.to_string());
    drawer.tier = Some(KnowledgeTier::Qi);
    drawer.status = Some(status);
    drawer
}

fn insert(db: &Database, drawer: Drawer, project_id: Option<&str>) {
    insert_with_vector(db, drawer, project_id, &vector());
}

fn insert_with_vector(db: &Database, drawer: Drawer, project_id: Option<&str>, embedding: &[f32]) {
    let id = drawer.id.clone();
    db.insert_drawer_with_project(&drawer, project_id)
        .expect("insert drawer");
    db.insert_vector_with_project(&id, embedding, project_id)
        .expect("insert vector");
}

fn ids(results: Vec<mempal::core::types::SearchResult>) -> Vec<String> {
    results.into_iter().map(|result| result.drawer_id).collect()
}

fn reset_vector_scan_telemetry() {
    observability::reset_vector_scan_for_tests();
}

fn vector_scan_telemetry() -> observability::VectorScanSnapshot {
    observability::vector_scan_snapshot()
}

#[tokio::test]
async fn test_vector_and_bm25_apply_memory_kind_status_scope() {
    let _guard = config_guard().await;
    reset_vector_scan_telemetry();
    let env = TestEnv::new(Some("project-a"));
    let db = env.db();
    insert(
        &db,
        base_drawer(
            "drawer_scope_evidence",
            "scope contract needle",
            Some("sess-a"),
        ),
        Some("project-a"),
    );
    insert(
        &db,
        knowledge_drawer(
            "drawer_scope_promoted",
            "scope contract needle",
            KnowledgeStatus::Promoted,
        ),
        Some("project-a"),
    );
    insert(
        &db,
        knowledge_drawer(
            "drawer_scope_candidate",
            "scope contract needle",
            KnowledgeStatus::Candidate,
        ),
        Some("project-a"),
    );

    let scope =
        ProjectSearchScope::from_request(Some("project-a".to_string()), false, false, false);
    let options = SearchOptions {
        filters: SearchFilters {
            memory_kind: Some("knowledge".to_string()),
            status: Some("promoted".to_string()),
            ..SearchFilters::default()
        },
        ..SearchOptions::default()
    };
    let vector_ids = ids(search_with_vector_and_scope_options(
        &db,
        "scope contract needle",
        &vector(),
        route(),
        &scope,
        options.clone(),
        10,
    )
    .expect("vector search"));
    assert_eq!(vector_ids, vec!["drawer_scope_promoted"]);
    let scan = vector_scan_telemetry();
    assert_eq!(scan.mode, Some(VectorScanMode::Exact));
    assert_eq!(scan.candidate_count, 1);
    assert_eq!(scan.candidate_cap, 4_096);

    let bm25_ids = ids(search_bm25_only_with_options(
        &db,
        "scope contract needle",
        route(),
        &scope,
        options,
        10,
    )
    .expect("bm25 search"));
    assert_eq!(bm25_ids, vec!["drawer_scope_promoted"]);
}

#[tokio::test]
async fn test_tunnel_fanout_preserves_typed_scope_filters() {
    let _guard = config_guard().await;
    reset_vector_scan_telemetry();
    let env = TestEnv::new(Some("project-a"));
    let db = env.db();
    let mut direct = knowledge_drawer(
        "drawer_scope_promoted",
        "scope tunnel needle",
        KnowledgeStatus::Promoted,
    );
    direct.room = Some("shared-session".to_string());
    insert(&db, direct, Some("project-a"));

    let mut tunnel = base_drawer(
        "drawer_scope_wrong_tunnel",
        "scope tunnel needle",
        Some("shared-session"),
    );
    tunnel.wing = "other-scope".to_string();
    insert(&db, tunnel, Some("project-b"));

    let scope =
        ProjectSearchScope::from_request(Some("project-a".to_string()), false, false, false);
    let options = SearchOptions {
        filters: SearchFilters {
            memory_kind: Some("knowledge".to_string()),
            status: Some("promoted".to_string()),
            ..SearchFilters::default()
        },
        ..SearchOptions::default()
    };

    let vector_ids = ids(search_with_vector_and_scope_options(
        &db,
        "scope tunnel needle",
        &vector(),
        route(),
        &scope,
        options.clone(),
        10,
    )
    .expect("vector search"));
    assert_eq!(vector_ids, vec!["drawer_scope_promoted"]);
    let scan = vector_scan_telemetry();
    assert_eq!(scan.mode, Some(VectorScanMode::Exact));
    assert_eq!(scan.candidate_count, 1);
    assert_eq!(scan.candidate_cap, 4_096);

    let bm25_ids = ids(search_bm25_only_with_options(
        &db,
        "scope tunnel needle",
        route(),
        &scope,
        options,
        10,
    )
    .expect("bm25 search"));
    assert_eq!(bm25_ids, vec!["drawer_scope_promoted"]);
}

#[tokio::test]
async fn test_large_typed_vector_filter_uses_bounded_knn_fallback() {
    let _guard = config_guard().await;
    reset_vector_scan_telemetry();
    let env = TestEnv::new(Some("project-a"));
    let db = env.db();
    let query_vector = vec![1.0, 0.0, 0.0];
    let near_vector = vec![1.0, 0.0, 0.0];
    let far_vector = vec![-1.0, 0.0, 0.0];

    for index in 0..150 {
        insert_with_vector(
            &db,
            knowledge_drawer(
                &format!("drawer_scope_near_nonmatch_{index:03}"),
                "near distractor content",
                KnowledgeStatus::Promoted,
            ),
            Some("project-a"),
            &near_vector,
        );
    }
    for index in 0..=4_096 {
        insert_with_vector(
            &db,
            base_drawer(
                &format!("drawer_scope_far_evidence_{index:04}"),
                "far evidence content",
                Some("sess-a"),
            ),
            Some("project-a"),
            &far_vector,
        );
    }

    let scope =
        ProjectSearchScope::from_request(Some("project-a".to_string()), false, false, false);
    let options = SearchOptions {
        filters: SearchFilters {
            memory_kind: Some("evidence".to_string()),
            ..SearchFilters::default()
        },
        ..SearchOptions::default()
    };
    let result_ids = ids(search_with_vector_and_scope_options(
        &db,
        "vectorfallbackprobe",
        &query_vector,
        route(),
        &scope,
        options,
        1,
    )
    .expect("large filtered vector search"));

    assert!(
        result_ids.is_empty(),
        "large typed filters must use the bounded KNN fallback instead of exact-scanning all evidence vectors"
    );
    let scan = vector_scan_telemetry();
    assert_eq!(scan.mode, Some(VectorScanMode::Knn));
    assert!(scan.candidate_count > 4_096);
    assert_eq!(scan.candidate_cap, 4_096);
}

#[tokio::test]
async fn test_mcp_scope_session_filters_room() {
    let _guard = config_guard().await;
    let env = TestEnv::new(Some("project-a"));
    let db = env.db();
    insert(
        &db,
        base_drawer(
            "drawer_scope_session_a",
            "session scope needle",
            Some("sess-a"),
        ),
        Some("project-a"),
    );
    insert(
        &db,
        base_drawer(
            "drawer_scope_session_b",
            "session scope needle",
            Some("sess-b"),
        ),
        Some("project-a"),
    );

    let response = env
        .server()
        .mempal_search(Parameters(SearchRequest {
            query: "session scope needle".to_string(),
            top_k: Some(10),
            scope: Some(RetrievalScopeRequest {
                session: Some("sess-a".to_string()),
                ..RetrievalScopeRequest::default()
            }),
            ..SearchRequest::default()
        }))
        .await
        .expect("mcp search")
        .0;
    let result_ids = response
        .results
        .into_iter()
        .map(|result| result.drawer_id)
        .collect::<Vec<_>>();
    assert_eq!(result_ids, vec!["drawer_scope_session_a"]);
}

#[tokio::test]
async fn test_mcp_scope_project_prevents_cross_project_leakage() {
    let _guard = config_guard().await;
    let env = TestEnv::new(None);
    let db = env.db();
    insert(
        &db,
        base_drawer(
            "drawer_scope_project_a",
            "project scope needle",
            Some("sess-a"),
        ),
        Some("project-a"),
    );
    insert(
        &db,
        base_drawer(
            "drawer_scope_project_b",
            "project scope needle",
            Some("sess-a"),
        ),
        Some("project-b"),
    );

    let response = env
        .server()
        .mempal_search(Parameters(SearchRequest {
            query: "project scope needle".to_string(),
            top_k: Some(10),
            scope: Some(RetrievalScopeRequest {
                project_id: Some("project-a".to_string()),
                ..RetrievalScopeRequest::default()
            }),
            ..SearchRequest::default()
        }))
        .await
        .expect("mcp search")
        .0;
    let result_ids = response
        .results
        .into_iter()
        .map(|result| result.drawer_id)
        .collect::<Vec<_>>();
    assert_eq!(result_ids, vec!["drawer_scope_project_a"]);
}
