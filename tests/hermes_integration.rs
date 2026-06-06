#![warn(clippy::all)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use mempal::core::compaction::merge_cluster;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::{Database, find_similar_clusters};
use mempal::core::types::{
    BootstrapEvidenceArgs, CompactionStrategy, Drawer, KnowledgeStatus, MemoryDomain, MemoryKind,
    SourceType, Triple, default_confidence,
};
use mempal::core::utils::build_triple_id;
use mempal::crystallize::{CrystallizeOptions, run_crystallization_deterministic};
use mempal::embed::{EmbedError, Embedder, EmbedderFactory, global_embed_status};
use mempal::factcheck::{self, FactIssue};
use mempal::intelligence::IntelligenceRouter;
use mempal::mcp::{
    IngestRequest, MempalMcpServer, PinnedFactsRequest, SearchRequest, StatusDetail, StatusRequest,
};
use mempal::sleep::{SleepPhaseSelection, SleepRunOptions, run_sleep_cycle};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;
use tokio::sync::{Mutex, MutexGuard};

const NOW_SECS: u64 = 1_800_000_000;
const NOW_RFC3339: &str = "2027-01-15T08:00:00Z";

fn serial_mutex() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}

async fn serial_guard() -> MutexGuard<'static, ()> {
    serial_mutex().lock().await
}

fn blocking_serial_guard() -> MutexGuard<'static, ()> {
    serial_mutex().blocking_lock()
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
        "hermes-static"
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
        Err(EmbedError::Runtime("embedder unavailable".to_string()))
    }

    fn dimensions(&self) -> usize {
        4
    }

    fn name(&self) -> &str {
        "hermes-failing"
    }
}

struct TestEnv {
    _tmp: TempDir,
    db_path: PathBuf,
    config_path: PathBuf,
}

impl TestEnv {
    fn new(project_id: Option<&str>) -> Self {
        Self::with_extra(project_id, "")
    }

    fn with_extra(project_id: Option<&str>, extra_config: &str) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db_path = mempal_home.join("palace.db");
        let config_path = mempal_home.join("config.toml");
        fs::write(
            &config_path,
            config_text(&db_path, project_id, extra_config),
        )
        .expect("write config");
        Database::open(&db_path).expect("open db");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        global_embed_status().reset_for_tests();
        Self {
            _tmp: tmp,
            db_path,
            config_path,
        }
    }

    fn config(&self) -> Config {
        Config::load_from(&self.config_path).expect("load config")
    }

    fn db(&self) -> Database {
        Database::open(&self.db_path).expect("open db")
    }

    fn server(&self) -> MempalMcpServer {
        MempalMcpServer::new_with_factory_and_config(
            self.db_path.clone(),
            self.config(),
            Arc::new(StaticEmbedderFactory {
                vector: vec![0.2, 0.4, 0.6, 0.8],
            }),
        )
    }

    fn server_with_factory(&self, factory: Arc<dyn EmbedderFactory>) -> MempalMcpServer {
        MempalMcpServer::new_with_factory_and_config(self.db_path.clone(), self.config(), factory)
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        global_embed_status().reset_for_tests();
    }
}

fn config_text(db_path: &Path, project_id: Option<&str>, extra_config: &str) -> String {
    let project = project_id
        .map(|id| format!("\n[project]\nid = \"{id}\"\n"))
        .unwrap_or_default();
    format!(
        r#"
db_path = "{}"
{}
[config_hot_reload]
enabled = false

[embed]
backend = "model2vec"

[llm]
enabled = false

[search]
bm25_fallback = true
strict_project_isolation = false
progressive_disclosure = false
exclude_raw_turns = true

[ingest_gating]
enabled = false

[ingest_gating.novelty]
enabled = false

[memory_intelligence]
mode = "deterministic"

[sleep]
nrem_prune_min_age_days = 1
nrem_prune_max_importance = 1
nrem_compaction_threshold = 0.95
rem_auto_resolve = true

[consolidation]
similarity_threshold = 0.95
min_cluster_size = 3
max_clusters_per_run = 100
strategy = "richest_content"

[crystallize]
enabled = true
min_cluster_size = 3
readiness_threshold = 0.01
auto_approve = false
max_candidates_per_run = 10
{extra_config}
"#,
        db_path.display(),
        project
    )
}

fn drawer(id: &str, content: &str, importance: i32, added_at: &str) -> Drawer {
    Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: id.to_string(),
        content: content.to_string(),
        wing: "hermes".to_string(),
        room: Some("facts".to_string()),
        source_file: Some(format!("tests://hermes/{id}")),
        source_type: SourceType::AgentInference,
        added_at: added_at.to_string(),
        chunk_index: Some(0),
        importance,
    })
}

fn insert_drawer(db: &Database, id: &str, content: &str, project_id: Option<&str>, vector: &[f32]) {
    let drawer = drawer(id, content, 3, "2026-01-15T00:00:00Z");
    db.insert_drawer_with_project(&drawer, project_id)
        .expect("insert drawer");
    db.insert_vector_with_project(id, vector, project_id)
        .expect("insert vector");
}

fn insert_pinned(
    db: &Database,
    id: &str,
    content: &str,
    pin_order: Option<i64>,
    project_id: Option<&str>,
) {
    let mut drawer = drawer(id, content, 5, "2027-01-15T00:00:00Z");
    drawer.memory_kind = MemoryKind::ProfileFact;
    drawer.domain = MemoryDomain::User;
    drawer.field = "preferences".to_string();
    drawer.status = Some(KnowledgeStatus::Active);
    drawer.is_pinned = true;
    drawer.pin_order = pin_order;
    db.insert_drawer_with_project(&drawer, project_id)
        .expect("insert pinned drawer");
}

fn insert_triple(
    db: &Database,
    subject: &str,
    predicate: &str,
    object: &str,
    confidence: f64,
    source_drawer: Option<&str>,
) {
    db.insert_triple(&Triple {
        id: build_triple_id(subject, predicate, object),
        subject: subject.to_string(),
        predicate: predicate.to_string(),
        object: object.to_string(),
        valid_from: Some((NOW_SECS - 86_400).to_string()),
        valid_to: None,
        confidence,
        source_drawer: source_drawer.map(str::to_string),
    })
    .expect("insert triple");
}

fn active_drawer_count(db: &Database) -> i64 {
    db.drawer_count().expect("drawer count")
}

fn raw_drawer_status(db: &Database, drawer_id: &str) -> (Option<String>, Option<String>) {
    db.conn()
        .query_row(
            "SELECT status, deleted_at FROM drawers WHERE id = ?1",
            [drawer_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read drawer status")
}

fn now_unix_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| NOW_SECS.to_string())
}

async fn ingest(
    server: &MempalMcpServer,
    content: &str,
    project_id: Option<&str>,
) -> mempal::mcp::IngestResponse {
    server
        .ingest_json_for_test(
            serde_json::to_value(IngestRequest {
                content: content.to_string(),
                wing: "hermes".to_string(),
                room: Some("facts".to_string()),
                source: Some("tests://hermes/ingest".to_string()),
                project_id: project_id.map(str::to_string),
                importance: Some(3),
                ..IngestRequest::default()
            })
            .expect("serialize ingest request"),
        )
        .await
        .expect("ingest")
}

async fn search_ids(server: &MempalMcpServer, query: &str) -> Vec<String> {
    server
        .mempal_search(Parameters(SearchRequest {
            query: query.to_string(),
            top_k: Some(10),
            ..SearchRequest::default()
        }))
        .await
        .expect("search")
        .0
        .results
        .into_iter()
        .map(|result| result.drawer_id)
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn test_add_duplicate_idempotent() {
    let _guard = serial_guard().await;
    let env = TestEnv::new(Some("project-a"));
    let server = env.server();

    let first = ingest(&server, "Hermes duplicate idempotent fact", None).await;
    let second = ingest(&server, "Hermes duplicate idempotent fact", None).await;

    assert_eq!(first.drawer_id, second.drawer_id);
    assert_eq!(first.drawer_ids, second.drawer_ids);
    assert_eq!(active_drawer_count(&env.db()), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn test_replace_supersedes_old() {
    let _guard = serial_guard().await;
    let env = TestEnv::new(Some("project-a"));
    let server = env.server();
    let old = ingest(&server, "Hermes old provider fact", None).await;

    let new = server
        .ingest_json_for_test(
            serde_json::to_value(IngestRequest {
                content: "Hermes new provider fact".to_string(),
                wing: "hermes".to_string(),
                room: Some("facts".to_string()),
                supersedes: Some(old.drawer_id.clone()),
                importance: Some(4),
                ..IngestRequest::default()
            })
            .expect("serialize ingest request"),
        )
        .await
        .expect("superseding ingest");

    assert_eq!(
        new.superseded_drawer_id.as_deref(),
        Some(old.drawer_id.as_str())
    );
    let new_drawer = env
        .db()
        .get_drawer(&new.drawer_id)
        .expect("load new")
        .expect("new drawer");
    assert_eq!(
        new_drawer.supersedes.as_deref(),
        Some(old.drawer_id.as_str())
    );
    let (status, deleted_at) = raw_drawer_status(&env.db(), &old.drawer_id);
    assert_eq!(status.as_deref(), Some("superseded"));
    assert!(deleted_at.is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn test_remove_excludes_from_search() {
    let _guard = serial_guard().await;
    let env = TestEnv::new(Some("project-a"));
    let server = env.server();
    let inserted = ingest(&server, "Hermes removable search needle", None).await;
    assert!(
        search_ids(&server, "removable needle")
            .await
            .contains(&inserted.drawer_id)
    );

    let deleted = env
        .db()
        .soft_delete_drawer(&inserted.drawer_id)
        .expect("soft delete");

    assert!(deleted);
    assert!(
        !search_ids(&server, "removable needle")
            .await
            .contains(&inserted.drawer_id)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_project_isolation_in_search() {
    let _guard = serial_guard().await;
    let env = TestEnv::new(Some("project-a"));
    let db = env.db();
    insert_drawer(
        &db,
        "project-a-search",
        "Hermes isolation search needle alpha",
        Some("project-a"),
        &[1.0, 0.0, 0.0, 0.0],
    );
    insert_drawer(
        &db,
        "project-b-search",
        "Hermes isolation search needle beta",
        Some("project-b"),
        &[1.0, 0.0, 0.0, 0.0],
    );

    let ids = search_ids(&env.server(), "isolation search needle").await;

    assert!(ids.contains(&"project-a-search".to_string()));
    assert!(!ids.contains(&"project-b-search".to_string()));
}

#[tokio::test(flavor = "current_thread")]
async fn test_project_isolation_in_compaction() {
    let _guard = serial_guard().await;
    let env = TestEnv::new(Some("project-a"));
    let db = env.db();
    for (project_id, prefix) in [("project-a", "a"), ("project-b", "b")] {
        for (idx, vector) in [
            vec![1.0_f32, 0.0, 0.0, 0.0],
            vec![0.99, 0.01, 0.0, 0.0],
            vec![0.98, 0.02, 0.0, 0.0],
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("{prefix}-compact-{idx}");
            insert_drawer(
                &db,
                &id,
                &format!("Hermes compaction cluster {project_id}"),
                Some(project_id),
                &vector,
            );
        }
    }

    let clusters = find_similar_clusters(
        db.conn(),
        Some("hermes"),
        Some("facts"),
        Some("project-a"),
        0.95,
        3,
    )
    .expect("find project-a clusters");
    assert_eq!(clusters.len(), 1);
    let ids = clusters[0]
        .iter()
        .map(|(drawer_id, _)| drawer_id.clone())
        .collect::<Vec<_>>();
    assert!(ids.iter().all(|id| id.starts_with("a-compact-")));
    let result = merge_cluster(&db, &ids, CompactionStrategy::RichestContent, false)
        .expect("merge project-a cluster");

    assert_eq!(result.cluster_size, 3);
    let project_b_compacted: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM drawers WHERE project_id = 'project-b' AND compacted_into IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("project-b compacted");
    assert_eq!(project_b_compacted, 0);
}

#[test]
fn test_project_isolation_in_sleep() {
    let _guard = blocking_serial_guard();
    let env = TestEnv::new(Some("project-a"));
    let config = env.config();
    let db = env.db();
    let old = "2020-01-01T00:00:00Z";
    db.insert_drawer_with_project(&drawer("sleep-a", "old low a", 1, old), Some("project-a"))
        .expect("insert project-a");
    db.insert_drawer_with_project(&drawer("sleep-b", "old low b", 1, old), Some("project-b"))
        .expect("insert project-b");

    let summary = run_sleep_cycle(
        &db,
        &config,
        SleepRunOptions {
            phases: SleepPhaseSelection {
                nrem: true,
                rem: false,
                salience: false,
            },
            dry_run: false,
            project_id: Some("project-a".to_string()),
        },
    )
    .expect("sleep");

    assert_eq!(summary.pruned_count(), 1);
    assert!(db.get_drawer("sleep-a").expect("load a").is_none());
    assert!(db.get_drawer("sleep-b").expect("load b").is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn test_pinned_facts_in_session_context() {
    let _guard = serial_guard().await;
    let env = TestEnv::new(Some("project-a"));
    let db = env.db();
    insert_pinned(
        &db,
        "pinned-session",
        "Pinned Hermes facts enter session context.",
        Some(0),
        Some("project-a"),
    );
    db.insert_drawer_with_project(
        &drawer(
            "unpinned-session",
            "Unpinned Hermes fact stays out.",
            5,
            "2027-01-15T00:00:00Z",
        ),
        Some("project-a"),
    )
    .expect("insert unpinned");

    let response = env
        .server()
        .mempal_pinned_facts(Parameters(PinnedFactsRequest {
            project_id: None,
            budget_chars: Some(4_000),
        }))
        .await
        .expect("pinned facts")
        .0;

    assert_eq!(response.facts.len(), 1);
    assert_eq!(response.facts[0].drawer_id, "pinned-session");
    assert!(response.text.contains("Pinned Hermes facts"));
}

#[tokio::test(flavor = "current_thread")]
async fn test_canonical_budget_ordering() {
    let _guard = serial_guard().await;
    let env = TestEnv::new(Some("project-a"));
    let db = env.db();
    insert_pinned(&db, "pin-0", "abcdef", Some(0), Some("project-a"));
    insert_pinned(&db, "pin-1", "ghijkl", Some(1), Some("project-a"));
    insert_pinned(&db, "pin-2", "mnopqr", Some(2), Some("project-a"));

    let response = env
        .server()
        .mempal_pinned_facts(Parameters(PinnedFactsRequest {
            project_id: None,
            budget_chars: Some(10),
        }))
        .await
        .expect("pinned facts")
        .0;

    let ids = response
        .facts
        .iter()
        .map(|fact| fact.drawer_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["pin-0", "pin-1"]);
    assert_eq!(response.facts[0].content, "abcdef");
    assert_eq!(response.facts[1].content, "ghij");
    assert_eq!(response.used_chars, 10);
}

#[tokio::test(flavor = "current_thread")]
async fn test_canonical_works_without_embedder() {
    let _guard = serial_guard().await;
    let env = TestEnv::new(Some("project-a"));
    insert_pinned(
        &env.db(),
        "pinned-no-embed",
        "Pinned recall does not need embedding.",
        Some(0),
        Some("project-a"),
    );

    let response = env
        .server_with_factory(Arc::new(PanicEmbedderFactory))
        .mempal_pinned_facts(Parameters(PinnedFactsRequest {
            project_id: None,
            budget_chars: Some(4_000),
        }))
        .await
        .expect("pinned facts")
        .0;

    assert_eq!(response.facts[0].drawer_id, "pinned-no-embed");
}

#[tokio::test(flavor = "current_thread")]
async fn test_supersession_chain_end_to_end() {
    let _guard = serial_guard().await;
    let env = TestEnv::new(Some("project-a"));
    let server = env.server();
    let old = ingest(&server, "Hermes supersession chain old", None).await;
    let new = server
        .ingest_json_for_test(
            serde_json::to_value(IngestRequest {
                content: "Hermes supersession chain new".to_string(),
                wing: "hermes".to_string(),
                room: Some("facts".to_string()),
                supersedes: Some(old.drawer_id.clone()),
                ..IngestRequest::default()
            })
            .expect("serialize ingest request"),
        )
        .await
        .expect("superseding ingest");

    let ids = search_ids(&server, "Hermes supersession chain").await;

    assert!(ids.contains(&new.drawer_id));
    assert!(!ids.contains(&old.drawer_id));
    let (old_status, old_deleted_at) = raw_drawer_status(&env.db(), &old.drawer_id);
    assert_eq!(old_status.as_deref(), Some("superseded"));
    assert!(old_deleted_at.is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn test_fact_check_detects_contradiction() {
    let _guard = serial_guard().await;
    let env = TestEnv::new(Some("project-a"));
    let db = env.db();
    insert_triple(&db, "Bob", "husband_of", "Alice", 0.9, None);

    let report =
        factcheck::check("Bob is Alice's brother", &db, NOW_SECS, None).expect("fact check");

    assert!(report.issues.iter().any(|issue| {
        matches!(
            issue,
            FactIssue::RelationContradiction {
                subject,
                text_claim,
                kg_fact,
                ..
            } if subject == "Bob" && text_claim == "brother_of" && kg_fact == "husband_of"
        )
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn test_confidence_based_resolution() {
    let _guard = serial_guard().await;
    let env = TestEnv::new(Some("project-a"));
    let config = env.config();
    let db = env.db();
    let mut old = drawer(
        "old-marriage-source",
        "Bob is Alice's husband.",
        3,
        "2027-01-01T00:00:00Z",
    );
    old.confidence = 0.4;
    db.insert_drawer_with_project(&old, Some("project-a"))
        .expect("insert old");
    insert_triple(
        &db,
        "Bob",
        "husband_of",
        "Alice",
        0.4,
        Some("old-marriage-source"),
    );
    let mut new = drawer(
        "new-family-source",
        "Bob is Alice's brother.",
        4,
        "2027-01-10T00:00:00Z",
    );
    new.confidence = 0.9;
    db.insert_drawer_with_project(&new, Some("project-a"))
        .expect("insert new");

    let summary = run_sleep_cycle(
        &db,
        &config,
        SleepRunOptions {
            phases: SleepPhaseSelection {
                nrem: false,
                rem: true,
                salience: false,
            },
            dry_run: false,
            project_id: Some("project-a".to_string()),
        },
    )
    .expect("sleep rem");

    assert_eq!(summary.conflicts_resolved_count(), 1);
    let valid_to: Option<String> = db
        .conn()
        .query_row(
            "SELECT valid_to FROM triples WHERE id = ?1",
            [build_triple_id("Bob", "husband_of", "Alice")],
            |row| row.get(0),
        )
        .expect("read triple valid_to");
    assert!(valid_to.is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn test_recent_facts_score_higher() {
    let _guard = serial_guard().await;
    let env = TestEnv::with_extra(
        Some("project-a"),
        r#"
[search.decay]
mode = "linear"
half_life_days = 7
"#,
    );
    let db = env.db();
    insert_drawer(
        &db,
        "recent-temporal",
        "Hermes temporal ranking needle",
        Some("project-a"),
        &[1.0, 0.0, 0.0, 0.0],
    );
    db.conn()
        .execute(
            "UPDATE drawers SET added_at = ?2, effective_importance = 5.0, importance = 5 WHERE id = ?1",
            ("recent-temporal", now_unix_string()),
        )
        .expect("set recent timestamp");
    insert_drawer(
        &db,
        "old-temporal",
        "Hermes temporal ranking needle",
        Some("project-a"),
        &[1.0, 0.0, 0.0, 0.0],
    );
    db.conn()
        .execute(
            "UPDATE drawers SET added_at = '2020-01-01T00:00:00Z', effective_importance = 5.0, importance = 5 WHERE id = 'old-temporal'",
            [],
        )
        .expect("set old timestamp");

    let response = env
        .server()
        .mempal_search(Parameters(SearchRequest {
            query: "Hermes temporal ranking needle".to_string(),
            top_k: Some(2),
            ..SearchRequest::default()
        }))
        .await
        .expect("search")
        .0;

    assert_eq!(response.results[0].drawer_id, "recent-temporal");
    assert!(response.results[0].effective_importance > response.results[1].effective_importance);
}

#[test]
fn test_old_low_importance_pruned_by_sleep() {
    let _guard = blocking_serial_guard();
    let env = TestEnv::new(Some("project-a"));
    let config = env.config();
    let db = env.db();
    db.insert_drawer_with_project(
        &drawer("old-low", "stale low importance", 1, "2020-01-01T00:00:00Z"),
        Some("project-a"),
    )
    .expect("insert low");
    db.insert_drawer_with_project(
        &drawer(
            "old-high",
            "stale high importance",
            5,
            "2020-01-01T00:00:00Z",
        ),
        Some("project-a"),
    )
    .expect("insert high");

    let summary = run_sleep_cycle(
        &db,
        &config,
        SleepRunOptions {
            phases: SleepPhaseSelection {
                nrem: true,
                rem: false,
                salience: false,
            },
            dry_run: false,
            project_id: Some("project-a".to_string()),
        },
    )
    .expect("sleep nrem");

    assert_eq!(summary.pruned_count(), 1);
    assert!(db.get_drawer("old-low").expect("load low").is_none());
    assert!(db.get_drawer("old-high").expect("load high").is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn test_deterministic_mode_full_pipeline() {
    let _guard = serial_guard().await;
    let env = TestEnv::new(Some("project-a"));
    let config = env.config();
    let router = IntelligenceRouter::from_config(&config);
    let enhanced = router
        .enhance_ingest("Alice works at Acme #employment")
        .await;
    assert!(!enhanced.used_llm);

    let server = env.server();
    let inserted = ingest(&server, "Alice works at Acme #employment", None).await;
    assert!(
        search_ids(&server, "Alice Acme employment")
            .await
            .contains(&inserted.drawer_id)
    );

    let report =
        factcheck::check("Alice works at Acme", &env.db(), NOW_SECS, None).expect("fact check");
    assert!(report.issues.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn test_auto_mode_graceful_degradation() {
    let _guard = serial_guard().await;
    let config = Config::parse(
        r#"
[memory_intelligence]
mode = "auto"

[memory_intelligence.llm]
base_url = "http://127.0.0.1:9/v1"
model = "unavailable-local-test"
timeout_secs = 1
"#,
    )
    .expect("parse config");
    let router = IntelligenceRouter::from_config(&config);

    let enhanced = router.enhance_ingest("Alice works at Acme").await;

    assert!(!enhanced.used_llm);
    assert_eq!(enhanced.raw_content, "Alice works at Acme");
    assert!(enhanced.fallback_reason.is_some());
}

#[test]
fn test_consolidate_then_crystallize() {
    let _guard = blocking_serial_guard();
    let env = TestEnv::new(Some("project-a"));
    let config = env.config();
    let db = env.db();
    for id in ["compact-a", "compact-b", "compact-c"] {
        db.insert_drawer_with_project(
            &drawer(id, "Hermes compaction source", 2, "2027-01-01T00:00:00Z"),
            Some("project-a"),
        )
        .expect("insert compact drawer");
    }
    let compact_ids = ["compact-a", "compact-b", "compact-c"]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let compaction = merge_cluster(&db, &compact_ids, CompactionStrategy::RichestContent, false)
        .expect("merge cluster");
    assert_eq!(compaction.cluster_size, 3);

    for (index, added_at) in [
        "2026-01-01T00:00:00Z",
        "2026-01-10T00:00:00Z",
        "2026-01-20T00:00:00Z",
        "2026-01-30T00:00:00Z",
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("crystal-{index}");
        db.insert_drawer_with_project(
            &drawer(
                &id,
                "Decision: Hermes auto crystallization preserves source citations for review.",
                5,
                added_at,
            ),
            Some("project-a"),
        )
        .expect("insert crystallize drawer");
    }

    let summary = run_crystallization_deterministic(
        &db,
        &config,
        CrystallizeOptions {
            dry_run: false,
            project_id: Some("project-a".to_string()),
            use_llm: false,
        },
    )
    .expect("crystallize");

    assert!(summary.cards_created >= 1);
    assert!(
        db.pending_auto_generated_knowledge_card_count()
            .expect("pending cards")
            >= 1
    );
}

#[test]
fn test_crystallize_respects_pinned() {
    let _guard = blocking_serial_guard();
    let env = TestEnv::new(Some("project-a"));
    let config = env.config();
    let db = env.db();
    let mut pinned = drawer(
        "pinned-crystal",
        "Decision: Hermes auto crystallization preserves source citations for review.",
        5,
        "2026-01-15T00:00:00Z",
    );
    pinned.is_pinned = true;
    pinned.status = Some(KnowledgeStatus::Active);
    db.insert_drawer_with_project(&pinned, Some("project-a"))
        .expect("insert pinned evidence");
    for (index, added_at) in [
        "2026-01-01T00:00:00Z",
        "2026-01-10T00:00:00Z",
        "2026-01-20T00:00:00Z",
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("unpinned-crystal-{index}");
        db.insert_drawer_with_project(
            &drawer(
                &id,
                "Decision: Hermes auto crystallization preserves source citations for review.",
                5,
                added_at,
            ),
            Some("project-a"),
        )
        .expect("insert crystallize drawer");
    }

    let summary = run_crystallization_deterministic(
        &db,
        &config,
        CrystallizeOptions {
            dry_run: false,
            project_id: Some("project-a".to_string()),
            use_llm: false,
        },
    )
    .expect("crystallize");

    assert_eq!(summary.cards_created, 1);
    assert!(summary.candidates.iter().all(|candidate| {
        !candidate
            .source_drawer_ids
            .contains(&"pinned-crystal".to_string())
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn test_search_bm25_fallback_integration() {
    let _guard = serial_guard().await;
    let env = TestEnv::new(Some("project-a"));
    let db = env.db();
    db.insert_drawer_with_project(
        &drawer(
            "bm25-fallback",
            "Hermes degraded embedder fallback needle",
            4,
            "2026-01-15T00:00:00Z",
        ),
        Some("project-a"),
    )
    .expect("insert fallback drawer");

    let response = env
        .server_with_factory(Arc::new(FailingEmbedderFactory))
        .mempal_search(Parameters(SearchRequest {
            query: "degraded fallback needle".to_string(),
            top_k: Some(5),
            ..SearchRequest::default()
        }))
        .await
        .expect("search fallback")
        .0;

    assert_eq!(response.search_mode, "bm25_only");
    assert!(!response.warnings.is_empty());
    assert_eq!(response.results[0].drawer_id, "bm25-fallback");
}

#[tokio::test(flavor = "current_thread")]
async fn test_status_reflects_all_features() {
    let _guard = serial_guard().await;
    let env = TestEnv::new(Some("project-a"));
    insert_pinned(
        &env.db(),
        "status-pinned",
        "Pinned status fact.",
        Some(0),
        Some("project-a"),
    );

    let status = env
        .server()
        .mempal_status_with_options(StatusRequest {
            detail: Some(StatusDetail::Full),
            scope: None,
            project_id: None,
        })
        .await
        .expect("status")
        .0;

    assert_eq!(status.intelligence_status.mode, "deterministic");
    assert_eq!(status.queue_stats.pending, 0);
    assert!(
        status
            .pinned_fact_counts
            .iter()
            .any(|count| { count.project_id.as_deref() == Some("project-a") && count.count == 1 })
    );
    assert!(status.memory_protocol.contains("pinned"));
    assert_eq!(status.search_decay_mode, "none");
    assert!(status.schema_version >= 15);
}

#[tokio::test(flavor = "current_thread")]
async fn test_typed_metadata_round_trips_with_search_results() {
    let _guard = serial_guard().await;
    let env = TestEnv::new(Some("project-a"));
    let db = env.db();
    let mut drawer = drawer(
        "typed-search",
        "Hermes typed metadata search needle",
        4,
        "2026-01-15T00:00:00Z",
    );
    drawer.memory_kind = MemoryKind::Knowledge;
    drawer.domain = MemoryDomain::Skill;
    drawer.field = "tooling".to_string();
    drawer.status = Some(KnowledgeStatus::Promoted);
    db.insert_drawer_with_project(&drawer, Some("project-a"))
        .expect("insert typed drawer");
    db.insert_vector_with_project(&drawer.id, &[1.0, 0.0, 0.0, 0.0], Some("project-a"))
        .expect("insert vector");

    let response = env
        .server()
        .mempal_search(Parameters(SearchRequest {
            query: "typed metadata search needle".to_string(),
            memory_kind: Some("knowledge".to_string()),
            domain: Some("skill".to_string()),
            field: Some("tooling".to_string()),
            top_k: Some(5),
            ..SearchRequest::default()
        }))
        .await
        .expect("search")
        .0;

    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].drawer_id, "typed-search");
    assert_eq!(response.results[0].memory_kind, "knowledge");
    assert_eq!(response.results[0].domain, "skill");
    assert_eq!(response.results[0].field, "tooling");
    assert_eq!(response.results[0].status.as_deref(), Some("promoted"));
}

#[test]
fn test_source_confidence_defaults_are_preserved() {
    let source_type = SourceType::UserExplicit;
    let mut drawer = drawer(
        "confidence-default",
        "Hermes confidence default fact",
        3,
        NOW_RFC3339,
    );
    drawer.source_type = source_type;
    drawer.confidence = default_confidence(source_type);

    assert_eq!(drawer.confidence, 0.9);
}
