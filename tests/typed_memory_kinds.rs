#![warn(clippy::all)]

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use mempal::context::{ContextRequest, assemble_context_with_vector};
use mempal::core::config::{ContextBudgetConfig, ContextConfig};
use mempal::core::db::Database;
use mempal::core::project::ProjectSearchScope;
use mempal::core::types::{
    AnchorKind, Drawer, KnowledgeStatus, MemoryDomain, MemoryKind, Provenance, RouteDecision,
    SearchResult, SourceType,
};
use mempal::search::tiered::ContextTrigger;
use mempal::search::{SearchOptions, search_bm25_only_with_options};
use tempfile::TempDir;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(1_746_000_000)
}

fn new_db() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    (tmp, db)
}

fn dummy_vector() -> Vec<f32> {
    vec![0.25; 384]
}

fn route() -> RouteDecision {
    RouteDecision {
        wing: Some("typed472".to_string()),
        room: None,
        confidence: 1.0,
        reason: "typed memory kind test".to_string(),
    }
}

fn typed_drawer(
    id: &str,
    kind: MemoryKind,
    room: &str,
    status: KnowledgeStatus,
    content: &str,
    importance: i32,
) -> Drawer {
    Drawer {
        id: id.to_string(),
        content: content.to_string(),
        wing: "typed472".to_string(),
        room: Some(room.to_string()),
        source_file: Some(format!("tests://{id}.md")),
        source_type: SourceType::AgentInference,
        confidence: 0.9,
        added_at: now_secs().to_string(),
        chunk_index: Some(0),
        normalize_version: 1,
        importance,
        memory_kind: kind,
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        anchor_kind: AnchorKind::Repo,
        anchor_id: "repo://typed-memory-kinds".to_string(),
        parent_anchor_id: None,
        provenance: Some(Provenance::Human),
        statement: Some(format!("statement for {id}")),
        tier: None,
        status: Some(status),
        supporting_refs: Vec::new(),
        counterexample_refs: Vec::new(),
        teaching_refs: Vec::new(),
        verification_refs: Vec::new(),
        scope_constraints: None,
        trigger_hints: None,
        is_pinned: false,
        pin_order: None,
        supersedes: None,
        effective_importance: importance as f64,
        compacted_into: None,
    }
}

fn insert_typed(
    db: &Database,
    drawer: &Drawer,
    valid_from: Option<&str>,
    valid_until: Option<&str>,
) {
    db.insert_drawer_with_project_validity(drawer, None, None, valid_from, valid_until)
        .expect("insert drawer");
    db.insert_vector(&drawer.id, &dummy_vector())
        .expect("insert vector");
}

fn result_ids(results: &[SearchResult]) -> BTreeSet<&str> {
    results
        .iter()
        .map(|result| result.drawer_id.as_str())
        .collect()
}

fn search(db: &Database, query: &str, options: SearchOptions) -> Vec<SearchResult> {
    search_bm25_only_with_options(
        db,
        query,
        route(),
        &ProjectSearchScope::all_projects(),
        options,
        20,
    )
    .expect("search")
}

fn context_config() -> ContextConfig {
    ContextConfig {
        tiered_retrieval_enabled: true,
        min_t1_importance: 3,
        t3_recency_window_days: 30,
        t1_recency_lambda: 0.01,
        budget: ContextBudgetConfig {
            total_tokens: 8000,
            t1_ratio: 0.30,
            t2_ratio: 0.50,
            t3_ratio: 0.20,
            overflow_to_t2: true,
        },
        include_cards_default: false,
    }
}

fn context_request(cwd: &Path) -> ContextRequest {
    ContextRequest {
        query: "typed472 decision".to_string(),
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        cwd: cwd.to_path_buf(),
        include_evidence: false,
        include_cards: false,
        max_items: 10,
        dao_tian_limit: 5,
        project_id: None,
        trigger: Some(ContextTrigger::SessionStart),
        context_cfg_override: Some(context_config()),
        include_distill_suggestions: false,
    }
}

#[test]
fn schema_accepts_supported_memory_kinds_without_rewriting_provenance() {
    let (_tmp, db) = new_db();
    let kinds = [
        MemoryKind::AtomicFact,
        MemoryKind::Decision,
        MemoryKind::Case,
        MemoryKind::Skill,
        MemoryKind::Foresight,
        MemoryKind::ProfileFact,
        MemoryKind::ProfileTrait,
    ];

    for kind in kinds {
        let id = format!("kind_{}", kind.as_str());
        let drawer = typed_drawer(
            &id,
            kind,
            "general",
            KnowledgeStatus::Active,
            &format!("typed472 taxonomy {}", kind.as_str()),
            2,
        );
        insert_typed(&db, &drawer, None, None);

        let stored = db
            .get_drawer(&id)
            .expect("load drawer")
            .expect("drawer exists");
        assert_eq!(stored.memory_kind, kind);
        assert_eq!(stored.source_file, Some(format!("tests://{id}.md")));
        assert_eq!(stored.provenance, Some(Provenance::Human));
    }

    let table_sql: String = db
        .conn()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'drawers'",
            [],
            |row| row.get(0),
        )
        .expect("drawers schema");
    for kind in MemoryKind::SUPPORTED {
        assert!(
            table_sql.contains(kind.as_str()),
            "schema should allow memory_kind={}",
            kind.as_str()
        );
    }
}

#[test]
fn atomic_fact_search_honors_active_superseded_and_expired_lifecycle() {
    let (_tmp, db) = new_db();
    let active = typed_drawer(
        "fact_active",
        MemoryKind::AtomicFact,
        "fact",
        KnowledgeStatus::Active,
        "typed472 atomic fact active",
        4,
    );
    let superseded = typed_drawer(
        "fact_old",
        MemoryKind::AtomicFact,
        "fact",
        KnowledgeStatus::Active,
        "typed472 atomic fact old",
        4,
    );
    let expired = typed_drawer(
        "fact_expired",
        MemoryKind::AtomicFact,
        "fact",
        KnowledgeStatus::Active,
        "typed472 atomic fact expired",
        4,
    );

    insert_typed(&db, &active, None, None);
    insert_typed(&db, &superseded, None, None);
    insert_typed(&db, &expired, Some("0"), Some("1"));
    assert!(
        db.supersede_drawer("fact_old", "replaced by fact_active")
            .expect("supersede drawer")
    );

    let results = search(&db, "typed472 atomic fact", SearchOptions::default());
    let ids = result_ids(&results);
    assert!(ids.contains("fact_active"));
    assert!(!ids.contains("fact_old"));
    assert!(!ids.contains("fact_expired"));

    let active_result = results
        .iter()
        .find(|result| result.drawer_id == "fact_active")
        .expect("active fact result");
    assert_eq!(active_result.memory_kind, MemoryKind::AtomicFact);
    assert_eq!(active_result.status, Some(KnowledgeStatus::Active));
    assert_eq!(active_result.source_file, "tests://fact_active.md");

    let include_expired = search(
        &db,
        "typed472 atomic fact",
        SearchOptions {
            include_expired: true,
            ..SearchOptions::default()
        },
    );
    let ids = result_ids(&include_expired);
    assert!(ids.contains("fact_active"));
    assert!(ids.contains("fact_expired"));
    assert!(!ids.contains("fact_old"));
}

#[test]
fn decision_search_and_context_expose_kind_with_lifecycle_filtering() {
    let (tmp, db) = new_db();
    let active = typed_drawer(
        "decision_active",
        MemoryKind::Decision,
        "decision",
        KnowledgeStatus::Active,
        "typed472 decision active",
        5,
    );
    let superseded = typed_drawer(
        "decision_old",
        MemoryKind::Decision,
        "decision",
        KnowledgeStatus::Active,
        "typed472 decision old",
        5,
    );
    let expired = typed_drawer(
        "decision_expired",
        MemoryKind::Decision,
        "decision",
        KnowledgeStatus::Active,
        "typed472 decision expired",
        5,
    );

    insert_typed(&db, &active, None, None);
    insert_typed(&db, &superseded, None, None);
    insert_typed(&db, &expired, Some("0"), Some("1"));
    assert!(
        db.supersede_drawer("decision_old", "replaced by decision_active")
            .expect("supersede drawer")
    );

    let results = search(&db, "typed472 decision", SearchOptions::default());
    let ids = result_ids(&results);
    assert!(ids.contains("decision_active"));
    assert!(!ids.contains("decision_old"));
    assert!(!ids.contains("decision_expired"));
    assert_eq!(
        results
            .iter()
            .find(|result| result.drawer_id == "decision_active")
            .map(|result| result.memory_kind),
        Some(MemoryKind::Decision)
    );

    let pack = assemble_context_with_vector(&db, context_request(tmp.path()), &dummy_vector())
        .expect("assemble context");
    let all_items = pack
        .sections
        .iter()
        .flat_map(|section| section.items.iter())
        .collect::<Vec<_>>();
    let context_ids = all_items
        .iter()
        .map(|item| item.drawer_id.as_str())
        .collect::<BTreeSet<_>>();
    assert!(context_ids.contains("decision_active"));
    assert!(!context_ids.contains("decision_old"));
    assert!(!context_ids.contains("decision_expired"));

    let active_item = all_items
        .into_iter()
        .find(|item| item.drawer_id == "decision_active")
        .expect("active decision context item");
    assert_eq!(active_item.memory_kind, MemoryKind::Decision);
    assert_eq!(active_item.status, Some(KnowledgeStatus::Active));
    assert_eq!(
        serde_json::to_value(active_item).expect("serialize context item")["memory_kind"],
        "decision"
    );
}

#[test]
fn tiered_context_prefers_statement_text_for_typed_records() {
    let (tmp, db) = new_db();
    let mut drawer = typed_drawer(
        "decision_statement_text",
        MemoryKind::Decision,
        "decision",
        KnowledgeStatus::Active,
        "typed472 decision verbose raw content that should stay out of context text",
        5,
    );
    drawer.statement = Some("Use the concise validated decision statement.".to_string());
    insert_typed(&db, &drawer, None, None);

    let pack = assemble_context_with_vector(&db, context_request(tmp.path()), &dummy_vector())
        .expect("assemble context");
    let item = pack
        .sections
        .iter()
        .flat_map(|section| section.items.iter())
        .find(|item| item.drawer_id == "decision_statement_text")
        .expect("typed decision context item");

    assert_eq!(item.text, "Use the concise validated decision statement.");
    assert!(
        !item.text.contains("verbose raw content"),
        "tiered context item should not expose verbose raw content when a typed statement exists"
    );
}
