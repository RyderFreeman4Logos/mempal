#![warn(clippy::all)]

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use mempal::context::{ContextRequest, assemble_context_with_vector};
use mempal::core::config::{ContextBudgetConfig, ContextConfig};
use mempal::core::db::{CURRENT_SCHEMA_VERSION, Database};
use mempal::core::foresight::{
    ForesightCreateRequest, ForesightError, ForesightListRequest, ForesightResolveRequest,
    ForesightStatus, create_foresight, list_foresights, resolve_foresight,
};
use mempal::core::project::ProjectSearchScope;
use mempal::core::types::{
    AnchorKind, BootstrapEvidenceArgs, Drawer, KnowledgeEvidenceRole, KnowledgeStatus,
    KnowledgeTier, MemoryDomain, MemoryKind, Provenance, RouteDecision, SearchResult, SourceType,
};
use mempal::embed::estimate_tokens;
use mempal::search::tiered::ContextTrigger;
use mempal::search::{SearchOptions, search_bm25_only_with_options};
use serde_json::Value;
use tempfile::TempDir;

const NOW: i64 = 1_800_000_000;
const DUE: &str = "1";
const FUTURE: &str = "1800086400";
const EXPIRED_UNTIL: &str = "2";
const LEGACY_REPO: &str = "repo://legacy";

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
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
        wing: Some("mempal".to_string()),
        room: Some("foresight".to_string()),
        confidence: 1.0,
        reason: "foresight test".to_string(),
    }
}

fn insert_evidence(db: &Database, id: &str) {
    let drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: id.to_string(),
        content: format!("supporting evidence for {id}"),
        wing: "mempal".to_string(),
        room: Some("evidence".to_string()),
        source_file: Some(format!("tests://{id}.md")),
        source_type: SourceType::AgentObservation,
        added_at: "1799999000".to_string(),
        chunk_index: Some(0),
        importance: 2,
    });
    db.insert_drawer_with_project_validity(&drawer, None, None, None, None)
        .expect("insert evidence");
    db.insert_vector(&drawer.id, &dummy_vector())
        .expect("insert evidence vector");
}

fn insert_context_knowledge(db: &Database, id: &str) {
    let drawer = Drawer {
        id: id.to_string(),
        content: format!("ordinary context knowledge for {id}"),
        wing: "mempal".to_string(),
        room: Some("knowledge".to_string()),
        source_file: Some(format!("tests://{id}.md")),
        source_type: SourceType::AgentInference,
        confidence: 1.0,
        added_at: "1799999000".to_string(),
        chunk_index: Some(0),
        importance: 3,
        memory_kind: MemoryKind::Knowledge,
        domain: MemoryDomain::Project,
        field: "foresight".to_string(),
        anchor_kind: AnchorKind::Repo,
        anchor_id: LEGACY_REPO.to_string(),
        provenance: Some(Provenance::Runtime),
        statement: Some(format!("ordinary context knowledge for {id}")),
        tier: Some(KnowledgeTier::DaoRen),
        status: Some(KnowledgeStatus::Promoted),
        effective_importance: 3.0,
        ..Drawer::default()
    };
    db.insert_drawer_with_project_validity(&drawer, None, None, None, None)
        .expect("insert context knowledge");
    db.insert_vector(&drawer.id, &dummy_vector())
        .expect("insert context vector");
}

fn create_request(statement: &str, due_at: &str) -> ForesightCreateRequest {
    ForesightCreateRequest {
        statement: statement.to_string(),
        reason: Some("follow up before stale assumptions become facts".to_string()),
        trigger_condition: "when context is assembled for foresight field".to_string(),
        due_at: due_at.to_string(),
        valid_from: None,
        valid_until: None,
        supporting_refs: Vec::new(),
        counterexample_refs: Vec::new(),
        verification_refs: Vec::new(),
        wing: "mempal".to_string(),
        room: Some("foresight".to_string()),
        project_id: None,
        domain: MemoryDomain::Project,
        field: "foresight".to_string(),
        anchor_kind: AnchorKind::Repo,
        anchor_id: LEGACY_REPO.to_string(),
        parent_anchor_id: None,
        source_file: Some(format!("tests://{statement}.md")),
        importance: 3,
        dry_run: false,
    }
}

fn create_test_foresight(db: &Database, request: ForesightCreateRequest) -> String {
    let outcome = create_foresight(db, request).expect("create foresight");
    assert!(outcome.created);
    outcome.drawer_id
}

fn list_default(db: &Database, now_unix: i64) -> Vec<mempal::core::foresight::Foresight> {
    list_foresights(
        db,
        ForesightListRequest {
            scope: ProjectSearchScope::all_projects(),
            domain: None,
            field: None,
            anchor_kind: None,
            anchor_id: None,
            include_pending: false,
            include_resolved: false,
            include_expired: false,
            now_unix,
            limit: None,
        },
    )
    .expect("list foresights")
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

fn result_ids(results: &[SearchResult]) -> BTreeSet<&str> {
    results
        .iter()
        .map(|result| result.drawer_id.as_str())
        .collect()
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
        query: "foresight475 future check".to_string(),
        domain: MemoryDomain::Project,
        field: "foresight".to_string(),
        cwd: cwd.to_path_buf(),
        include_evidence: false,
        include_cards: false,
        max_items: 10,
        dao_tian_limit: 1,
        project_id: None,
        trigger: Some(ContextTrigger::SessionStart),
        context_cfg_override: Some(context_config()),
        include_distill_suggestions: false,
    }
}

#[test]
fn schema_v19_adds_foresights_table() {
    let (_tmp, db) = new_db();
    let version: i64 = db
        .conn()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version");
    assert_eq!(version as u32, CURRENT_SCHEMA_VERSION);

    let exists: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'foresights'",
            [],
            |row| row.get(0),
        )
        .expect("foresights table");
    assert_eq!(exists, 1);
}

#[test]
fn future_foresight_is_explicit_not_normal_fact_retrieval() {
    let (_tmp, db) = new_db();
    let drawer_id = create_test_foresight(
        &db,
        create_request("foresight475 future not yet due", FUTURE),
    );

    let default_results = search(
        &db,
        "foresight475 future not yet due",
        SearchOptions::default(),
    );
    assert!(!result_ids(&default_results).contains(drawer_id.as_str()));

    let explicit = list_foresights(
        &db,
        ForesightListRequest {
            scope: ProjectSearchScope::all_projects(),
            include_pending: true,
            include_resolved: false,
            include_expired: false,
            now_unix: NOW,
            limit: None,
            domain: None,
            field: None,
            anchor_kind: None,
            anchor_id: None,
        },
    )
    .expect("list pending");
    assert_eq!(explicit.len(), 1);
    assert_eq!(explicit[0].drawer_id, drawer_id);
    assert_eq!(explicit[0].status, ForesightStatus::Pending);

    let valid_from: String = db
        .conn()
        .query_row(
            "SELECT valid_from FROM drawers WHERE id = ?1",
            [drawer_id.as_str()],
            |row| row.get(0),
        )
        .expect("valid_from");
    assert_eq!(valid_from, FUTURE);
}

#[test]
fn valid_from_after_due_defers_due_status() {
    let (_tmp, db) = new_db();
    let mut request = create_request("foresight475 valid_from gate", DUE);
    request.valid_from = Some(FUTURE.to_string());
    let drawer_id = create_test_foresight(&db, request);

    assert!(list_default(&db, NOW).is_empty());
    let default_results = search(
        &db,
        "foresight475 valid_from gate",
        SearchOptions::default(),
    );
    assert!(!result_ids(&default_results).contains(drawer_id.as_str()));

    let explicit = list_foresights(
        &db,
        ForesightListRequest {
            scope: ProjectSearchScope::all_projects(),
            include_pending: true,
            include_resolved: false,
            include_expired: false,
            now_unix: NOW,
            limit: None,
            domain: None,
            field: None,
            anchor_kind: None,
            anchor_id: None,
        },
    )
    .expect("list valid_from-gated pending");
    assert_eq!(explicit.len(), 1);
    assert_eq!(explicit[0].drawer_id, drawer_id);
    assert_eq!(explicit[0].status, ForesightStatus::Pending);
}

#[test]
fn due_foresight_surfaces_in_context_with_supporting_refs() {
    let (tmp, db) = new_db();
    insert_evidence(&db, "drawer_supporting_foresight475");
    let mut request = create_request("foresight475 due context signal", DUE);
    request.supporting_refs = vec!["drawer_supporting_foresight475".to_string()];
    let drawer_id = create_test_foresight(&db, request);

    let due = list_default(&db, NOW);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].status, ForesightStatus::Due);
    assert_eq!(
        due[0].supporting_refs,
        vec!["drawer_supporting_foresight475".to_string()]
    );

    let pack = assemble_context_with_vector(&db, context_request(tmp.path()), &dummy_vector())
        .expect("assemble context");
    let foresight_section = pack
        .sections
        .iter()
        .find(|section| section.name == "foresight")
        .expect("foresight section");
    let item = foresight_section
        .items
        .iter()
        .find(|item| item.drawer_id == drawer_id)
        .expect("foresight context item");
    assert_eq!(item.memory_kind, MemoryKind::Foresight);
    assert_eq!(item.status, Some(KnowledgeStatus::Active));
    assert!(item.text.contains("foresight475 due context signal"));
    assert_eq!(item.evidence_citations.len(), 1);
    assert_eq!(
        item.evidence_citations[0].role,
        KnowledgeEvidenceRole::Supporting
    );
    assert_eq!(
        item.evidence_citations[0].evidence_drawer_id,
        "drawer_supporting_foresight475"
    );
}

#[test]
fn flat_context_keeps_due_foresight_within_max_items() {
    let (tmp, db) = new_db();
    insert_context_knowledge(&db, "drawer_normal_foresight475");
    let drawer_id = create_test_foresight(
        &db,
        create_request("foresight475 due replaces lower priority context", DUE),
    );

    let mut cfg = context_config();
    cfg.tiered_retrieval_enabled = false;
    let mut request = context_request(tmp.path());
    request.max_items = 1;
    request.context_cfg_override = Some(cfg);

    let pack =
        assemble_context_with_vector(&db, request, &dummy_vector()).expect("assemble context");
    let total_items: usize = pack
        .sections
        .iter()
        .map(|section| section.items.len())
        .sum();
    assert_eq!(total_items, 1, "max_items must cap total context items");
    let foresight_section = pack
        .sections
        .iter()
        .find(|section| section.name == "foresight")
        .expect("foresight section");
    assert_eq!(foresight_section.items.len(), 1);
    assert_eq!(foresight_section.items[0].drawer_id, drawer_id);
}

#[test]
fn tiered_context_budget_counts_due_foresight_tokens() {
    let (tmp, db) = new_db();
    insert_evidence(&db, "drawer_vector_bootstrap_foresight475");
    let drawer_id = create_test_foresight(
        &db,
        create_request("foresight475 due token accounting", DUE),
    );

    let pack = assemble_context_with_vector(&db, context_request(tmp.path()), &dummy_vector())
        .expect("assemble context");
    let foresight_item = pack
        .sections
        .iter()
        .find(|section| section.name == "foresight")
        .and_then(|section| {
            section
                .items
                .iter()
                .find(|item| item.drawer_id == drawer_id)
        })
        .expect("foresight context item");
    let expected_tokens = estimate_tokens(&foresight_item.text);
    let tiered = pack.tiered.expect("tiered assembly");

    assert_eq!(tiered.budget_used.foresight_tokens, expected_tokens);
    assert_eq!(
        tiered.budget_used.total_tokens(),
        tiered.budget_used.t1_tokens
            + tiered.budget_used.t2_tokens
            + tiered.budget_used.t3_tokens
            + tiered.budget_used.foresight_tokens
    );
    assert!(tiered.budget_used.total_tokens() <= context_config().budget.total_tokens);
}

#[test]
fn expired_and_resolved_foresights_are_hidden_by_default() {
    let (_tmp, db) = new_db();
    let mut expired_request = create_request("foresight475 expired risk", DUE);
    expired_request.valid_until = Some(EXPIRED_UNTIL.to_string());
    let expired_id = create_test_foresight(&db, expired_request);

    let resolved_id =
        create_test_foresight(&db, create_request("foresight475 resolved follow up", DUE));
    let outcome = resolve_foresight(
        &db,
        ForesightResolveRequest {
            drawer_id: resolved_id.clone(),
            resolution_note: Some("checked and retired".to_string()),
        },
    )
    .expect("resolve foresight");
    assert!(outcome.resolved);

    let visible = list_default(&db, NOW);
    assert!(visible.is_empty());

    let search_results = search(
        &db,
        "foresight475 expired risk resolved follow up",
        SearchOptions::default(),
    );
    let ids = result_ids(&search_results);
    assert!(!ids.contains(expired_id.as_str()));
    assert!(!ids.contains(resolved_id.as_str()));

    let explicit = list_foresights(
        &db,
        ForesightListRequest {
            scope: ProjectSearchScope::all_projects(),
            domain: None,
            field: None,
            anchor_kind: None,
            anchor_id: None,
            include_pending: false,
            include_resolved: true,
            include_expired: true,
            now_unix: NOW,
            limit: None,
        },
    )
    .expect("list explicit hidden states");
    assert!(
        explicit
            .iter()
            .any(|foresight| foresight.drawer_id == expired_id
                && foresight.status == ForesightStatus::Expired)
    );
    assert!(
        explicit
            .iter()
            .any(|foresight| foresight.drawer_id == resolved_id
                && foresight.status == ForesightStatus::Resolved)
    );
}

#[test]
fn supporting_refs_and_provenance_are_preserved() {
    let (_tmp, db) = new_db();
    insert_evidence(&db, "drawer_supporting_refs475");
    insert_evidence(&db, "drawer_counter_refs475");
    insert_evidence(&db, "drawer_verification_refs475");
    let mut request = create_request("foresight475 refs preserved", DUE);
    request.supporting_refs = vec!["drawer_supporting_refs475".to_string()];
    request.counterexample_refs = vec!["drawer_counter_refs475".to_string()];
    request.verification_refs = vec!["drawer_verification_refs475".to_string()];
    let drawer_id = create_test_foresight(&db, request);

    let drawer = db
        .get_drawer(&drawer_id)
        .expect("load foresight")
        .expect("foresight exists");
    assert_eq!(drawer.memory_kind, MemoryKind::Foresight);
    assert_eq!(drawer.provenance, Some(Provenance::Runtime));
    assert_eq!(drawer.status, Some(KnowledgeStatus::Active));
    assert_eq!(drawer.supporting_refs, vec!["drawer_supporting_refs475"]);
    assert_eq!(drawer.counterexample_refs, vec!["drawer_counter_refs475"]);
    assert_eq!(
        drawer.verification_refs,
        vec!["drawer_verification_refs475"]
    );
}

#[test]
fn evidence_refs_accept_existing_drawer_ids_with_hyphens_for_all_roles() {
    let (_tmp, db) = new_db();
    let diary_id = "drawer_agent-diary_claude_day_2026-06-19";
    insert_evidence(&db, diary_id);
    let mut request = create_request("foresight475 hyphen evidence refs", DUE);
    request.supporting_refs = vec![format!("  {diary_id}  ")];
    request.counterexample_refs = vec![diary_id.to_string()];
    request.verification_refs = vec![diary_id.to_string()];

    let drawer_id = create_test_foresight(&db, request);
    let drawer = db
        .get_drawer(&drawer_id)
        .expect("load foresight")
        .expect("foresight exists");

    assert_eq!(drawer.supporting_refs, vec![diary_id]);
    assert_eq!(drawer.counterexample_refs, vec![diary_id]);
    assert_eq!(drawer.verification_refs, vec![diary_id]);
}

#[test]
fn evidence_refs_reject_empty_missing_and_non_evidence_inputs() {
    let (_tmp, db) = new_db();

    let mut empty_ref = create_request("foresight475 empty evidence ref", DUE);
    empty_ref.supporting_refs = vec!["  ".to_string()];
    let error = create_foresight(&db, empty_ref).expect_err("empty ref rejected");
    assert!(matches!(
        error,
        ForesightError::MalformedRef {
            field: "supporting_refs"
        }
    ));

    let mut missing_ref = create_request("foresight475 missing evidence ref", DUE);
    missing_ref.counterexample_refs = vec!["drawer_missing-ref_2026-06-19".to_string()];
    let error = create_foresight(&db, missing_ref).expect_err("missing ref rejected");
    assert!(matches!(
        error,
        ForesightError::RefDrawerNotFound(ref drawer_id)
            if drawer_id == "drawer_missing-ref_2026-06-19"
    ));

    insert_context_knowledge(&db, "drawer_non_evidence_ref475");
    let mut non_evidence_ref = create_request("foresight475 non evidence ref", DUE);
    non_evidence_ref.verification_refs = vec!["drawer_non_evidence_ref475".to_string()];
    let error = create_foresight(&db, non_evidence_ref).expect_err("non-evidence ref rejected");
    assert!(matches!(
        error,
        ForesightError::RefNotEvidence {
            field: "verification_refs",
            ref drawer_id,
        } if drawer_id == "drawer_non_evidence_ref475"
    ));
}

#[test]
fn cli_add_list_resolve_round_trip() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let mempal_home = home.join(".mempal");
    std::fs::create_dir_all(&mempal_home).expect("create mempal home");
    Database::open(&mempal_home.join("palace.db")).expect("open db");

    let add_output = Command::new(mempal_bin())
        .args([
            "foresight",
            "add",
            "--statement",
            "foresight475 cli signal",
            "--trigger",
            "when cli lists due foresights",
            "--due-at",
            "1",
            "--anchor-kind",
            "repo",
            "--anchor-id",
            LEGACY_REPO,
            "--json",
        ])
        .env("HOME", &home)
        .output()
        .expect("run foresight add");
    assert!(
        add_output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&add_output.stderr)
    );
    let add_json: Value = serde_json::from_slice(&add_output.stdout).expect("add json");
    let drawer_id = add_json["drawer_id"].as_str().expect("drawer_id");

    let list_output = Command::new(mempal_bin())
        .args([
            "foresight",
            "list",
            "--all-projects",
            "--now",
            "2",
            "--format",
            "json",
        ])
        .env("HOME", &home)
        .output()
        .expect("run foresight list");
    assert!(
        list_output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&list_output.stderr)
    );
    let list_json: Value = serde_json::from_slice(&list_output.stdout).expect("list json");
    assert_eq!(
        list_json.as_array().expect("array")[0]["drawer_id"],
        drawer_id
    );
    assert_eq!(list_json.as_array().expect("array")[0]["status"], "due");

    let resolve_output = Command::new(mempal_bin())
        .args(["foresight", "resolve", drawer_id, "--json"])
        .env("HOME", &home)
        .output()
        .expect("run foresight resolve");
    assert!(
        resolve_output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&resolve_output.stderr)
    );
    let resolve_json: Value = serde_json::from_slice(&resolve_output.stdout).expect("resolve json");
    assert_eq!(resolve_json["drawer_id"], drawer_id);
    assert_eq!(resolve_json["resolved"], true);
}
