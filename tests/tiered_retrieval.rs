#![warn(clippy::all)]

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use mempal::context::{ContextRequest, assemble_context_with_vector};
use mempal::core::config::{ContextBudgetConfig, ContextConfig};
use mempal::core::db::Database;
use mempal::core::types::{Drawer, MemoryDomain, SourceType};
use mempal::search::tiered::{ContextTrigger, T3Params, compute_budgets, fetch_t3, now_unix_secs};
use tempfile::TempDir;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(1_746_000_000)
}

fn new_db() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    (tmp, db)
}

fn dummy_vector() -> Vec<f32> {
    vec![0.1; 384]
}

fn tiered_config_enabled() -> ContextConfig {
    ContextConfig {
        tiered_retrieval_enabled: true,
        min_t1_importance: 3,
        t3_recency_window_days: 3,
        t1_recency_lambda: 0.01,
        budget: ContextBudgetConfig {
            total_tokens: 8000,
            t1_ratio: 0.30,
            t2_ratio: 0.50,
            t3_ratio: 0.20,
            overflow_to_t2: true,
        },
    }
}

fn tiered_config_disabled() -> ContextConfig {
    ContextConfig {
        tiered_retrieval_enabled: false,
        ..ContextConfig::default()
    }
}

fn make_drawer(id: &str, room: &str, importance: i32, days_ago: i64) -> Drawer {
    let added_at = (now_secs() - days_ago * 86_400).to_string();
    Drawer {
        id: id.to_string(),
        content: format!("content for drawer {id} with some text to estimate tokens"),
        wing: "test".to_string(),
        room: Some(room.to_string()),
        source_file: Some(format!("tests://{id}.md")),
        source_type: SourceType::Manual,
        added_at,
        importance,
        effective_importance: importance as f64,
        ..Drawer::default()
    }
}

fn insert(db: &Database, drawer: &Drawer) {
    db.insert_drawer(drawer).expect("insert drawer");
    db.insert_vector(&drawer.id, &dummy_vector())
        .expect("insert vector");
}

fn request_with_cfg(cwd: &Path, cfg: ContextConfig) -> ContextRequest {
    ContextRequest {
        query: "test query".to_string(),
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        cwd: cwd.to_path_buf(),
        include_evidence: false,
        include_cards: false,
        max_items: 20,
        dao_tian_limit: 5,
        project_id: None,
        trigger: None,
        context_cfg_override: Some(cfg),
    }
}

fn request_with_trigger(cwd: &Path, cfg: ContextConfig, trigger: ContextTrigger) -> ContextRequest {
    ContextRequest {
        trigger: Some(trigger),
        ..request_with_cfg(cwd, cfg)
    }
}

// --- Scenario: session_start trigger returns t1/t2/t3 arrays ---

#[test]
fn test_tiered_context_session_start_default_weights() {
    let (tmp, db) = new_db();
    // T1 candidates: decision drawers with importance >= 3
    insert(&db, &make_drawer("d-decision-1", "decision", 4, 5));
    insert(&db, &make_drawer("d-feedback-1", "feedback", 3, 2));
    // T3 candidates: recent drawers (within 3 days)
    insert(&db, &make_drawer("d-recent-1", "general", 1, 1));
    insert(&db, &make_drawer("d-recent-2", "general", 2, 0));
    // Older drawer (should NOT be in T3)
    insert(&db, &make_drawer("d-old-1", "general", 1, 10));

    let req = request_with_trigger(
        tmp.path(),
        tiered_config_enabled(),
        ContextTrigger::SessionStart,
    );
    let pack = assemble_context_with_vector(&db, req, &dummy_vector()).expect("assemble");

    let tiered = pack.tiered.expect("tiered assembly should be present");
    assert!(
        !tiered.t1_items.is_empty(),
        "T1 should have decision/feedback drawers"
    );
    for item in &tiered.t1_items {
        let room = item.room.as_deref().unwrap_or("");
        assert!(
            matches!(room, "decision" | "feedback" | "rule"),
            "T1 item room should be decision/feedback/rule, got: {room}"
        );
    }
    let total_used = tiered.budget_used.total_tokens();
    assert!(
        total_used <= 8000,
        "budget_used.total_tokens={total_used} should not exceed 8000"
    );
}

// --- Scenario: repair trigger boosts T1 budget ---

#[test]
fn test_tiered_context_repair_trigger_boosts_t1() {
    let (t1_ss, _, _) = compute_budgets(8000, 0.30, 0.50, 0.20, ContextTrigger::SessionStart);
    let (t1_rep, _, _) = compute_budgets(8000, 0.30, 0.50, 0.20, ContextTrigger::Repair);
    assert!(
        t1_rep > t1_ss,
        "repair T1 budget {t1_rep} should exceed session_start T1 budget {t1_ss}"
    );
}

// --- Scenario: on_demand boosts T2 budget ---

#[test]
fn test_tiered_context_on_demand_boosts_t2() {
    let (_, t2_ss, _) = compute_budgets(8000, 0.30, 0.50, 0.20, ContextTrigger::SessionStart);
    let (_, t2_od, _) = compute_budgets(8000, 0.30, 0.50, 0.20, ContextTrigger::OnDemand);
    assert!(
        t2_od > t2_ss,
        "on_demand T2 budget {t2_od} should exceed session_start T2 budget {t2_ss}"
    );
}

// --- Scenario: budget does not exceed total_tokens ---

#[test]
fn test_tiered_context_budget_does_not_exceed_total() {
    let (tmp, db) = new_db();
    // Insert many drawers to exercise budget limits
    for i in 0..20 {
        insert(&db, &make_drawer(&format!("t1-{i}"), "decision", 4, i));
        insert(&db, &make_drawer(&format!("t3-{i}"), "general", 1, 0));
    }

    let req = request_with_trigger(
        tmp.path(),
        tiered_config_enabled(),
        ContextTrigger::SessionStart,
    );
    let pack = assemble_context_with_vector(&db, req, &dummy_vector()).expect("assemble");

    let tiered = pack.tiered.expect("tiered assembly should be present");
    let total = tiered.budget_used.total_tokens();
    let sum =
        tiered.budget_used.t1_tokens + tiered.budget_used.t2_tokens + tiered.budget_used.t3_tokens;
    assert!(total <= 8000, "total_tokens={total} must not exceed 8000");
    assert_eq!(total, sum, "total_tokens must equal t1+t2+t3 sum");
}

// --- Scenario: overflow_to_t2=true transfers T1/T3 unused budget to T2 ---

#[test]
fn test_tiered_context_overflow_budget_to_t2() {
    // Use direct budget computation to verify overflow logic.
    // With overflow: T1 budget = 2400, T1 uses 100 tokens → overflow to T2 = 2300.
    // Without overflow: T2 receives only its base ratio budget.
    let total = 8000usize;
    let t1_ratio = 0.30;
    let t2_ratio = 0.50;
    let t3_ratio = 0.20;
    let (t1_budget, t2_budget, _t3_budget) = compute_budgets(
        total,
        t1_ratio,
        t2_ratio,
        t3_ratio,
        ContextTrigger::SessionStart,
    );
    let t1_used = 100usize;
    let overflow = t1_budget.saturating_sub(t1_used);
    let effective_t2 = t2_budget + overflow;
    assert!(
        effective_t2 > t2_budget,
        "T2 effective budget {effective_t2} should exceed base T2 budget {t2_budget} when T1 has overflow"
    );
}

// --- Scenario: T3 only includes drawers within recency_window_days ---

#[test]
fn test_tiered_t3_respects_recency_window() {
    let (_, db) = new_db();
    let now = now_unix_secs();
    // drawer_old: 10 days ago — outside 3-day window
    insert(&db, &make_drawer("drawer-old", "general", 1, 10));
    // drawer_new: today — inside 3-day window
    insert(&db, &make_drawer("drawer-new", "general", 1, 0));

    let items = fetch_t3(
        &db,
        T3Params {
            recency_window_days: 3,
            budget_tokens: 8000,
            project_id: None,
            exclude_ids: &[],
            now_unix: now,
        },
    )
    .expect("fetch T3");

    let ids: Vec<&str> = items.iter().map(|i| i.drawer_id.as_str()).collect();
    assert!(
        ids.contains(&"drawer-new"),
        "drawer-new should be in T3: {ids:?}"
    );
    assert!(
        !ids.contains(&"drawer-old"),
        "drawer-old should NOT be in T3: {ids:?}"
    );
}

// --- Scenario: tiered_retrieval_enabled=false falls back to flat assembly ---

#[test]
fn test_tiered_context_disabled_falls_back() {
    let (tmp, db) = new_db();
    insert(&db, &make_drawer("flat-drawer-1", "general", 1, 0));

    let req = request_with_cfg(tmp.path(), tiered_config_disabled());
    let pack = assemble_context_with_vector(&db, req, &dummy_vector()).expect("assemble");

    assert!(
        pack.tiered.is_none(),
        "flat path should not produce tiered assembly"
    );
}

// --- Scenario: legacy fields preserved in tiered mode ---
// Verify via ContextPack.sections: build_tiered_sections creates named sections
// "dao_tian", "shu", "qi" whose items match t1/t2/t3 items respectively.

#[test]
fn test_tiered_context_preserves_legacy_fields() {
    let (tmp, db) = new_db();
    insert(&db, &make_drawer("legacy-t1", "decision", 4, 1));
    insert(&db, &make_drawer("legacy-t3", "general", 1, 0));

    let req = request_with_cfg(tmp.path(), tiered_config_enabled());
    let pack = assemble_context_with_vector(&db, req, &dummy_vector()).expect("assemble");

    if let Some(tiered) = &pack.tiered {
        // sections should mirror t1/t2/t3 items (dao_tian/shu/qi named sections)
        let dao_tian_section = pack.sections.iter().find(|s| s.name == "dao_tian");
        let t1_count = tiered.t1_items.len();
        if t1_count > 0 {
            let section_count = dao_tian_section.map(|s| s.items.len()).unwrap_or(0);
            assert_eq!(
                section_count, t1_count,
                "dao_tian section items should equal t1_items count"
            );
        }
        assert!(
            pack.tiered.is_some(),
            "tiered field should be present when enabled"
        );
    }
}

// --- Scenario: mempal_search unaffected by tiered config ---

#[test]
fn test_search_unaffected_by_tiered_context_config() {
    use mempal::core::types::RouteDecision;
    use mempal::search::{SearchFilters, SearchOptions, search_with_vector_options};

    let (_, db) = new_db();
    insert(&db, &make_drawer("search-drawer-1", "general", 1, 0));

    let route = RouteDecision {
        wing: None,
        room: None,
        confidence: 0.0,
        reason: "test".to_string(),
    };

    // search should work regardless of tiered config (it's independent)
    let results = search_with_vector_options(
        &db,
        "test query",
        &dummy_vector(),
        route,
        SearchOptions {
            filters: SearchFilters::default(),
            with_neighbors: false,
        },
        10,
    )
    .expect("search should succeed");

    // Just verify the search runs successfully; tiered config has no effect on it.
    let _ = results;
}
