use mempal::brief::{
    BriefCitation, BriefEvidence, BriefSummary, CognitiveBrief, superseded_drawer_ids,
};
use mempal::cited_recall_bench::{
    CONTINUATION_SOURCE, CORRECTION_ID, CORRECTION_SOURCE, CURRENT_ID, PINNED_SOURCE,
    SUPERSEDED_ID, cited_recall_bench_passes, has_required_citations, leaks_superseded,
    run_cited_recall_bench, run_cited_recall_bench_command, seed_resume_fixture,
};
use mempal::context::{ContextRequest, assemble_context_with_vector};
use mempal::core::db::Database;
use mempal::core::types::{AnchorKind, MemoryDomain};

#[test]
fn cited_recall_resume_compaction_gate_passes_without_leaking_fixture_text() {
    let report = run_cited_recall_bench().expect("cited recall bench should run");

    assert_eq!(report.schema_version, "mempal.cited_recall_bench.v1");
    assert_eq!(report.dataset.id, "cited_recall_resume_compaction_v1");
    assert!(cited_recall_bench_passes(&report), "{report:?}");
    assert_eq!(report.remote_calls, 0);
    assert!(!report.reproducibility.provider_calls_enabled);

    let json = serde_json::to_string(&report).expect("serialize report");
    assert!(!json.contains("MempalAlpha"));
    assert!(!json.contains("YAML files"));
    assert!(!json.contains("Redis"));
    assert!(!json.contains("ad-hoc notes"));
    assert!(!json.contains("fixture://cited-recall/"));
}

#[test]
fn cited_recall_command_exits_zero_for_passing_gate() {
    run_cited_recall_bench_command().expect("focused cited-recall command should pass");
}

#[test]
fn correction_and_stale_hits_drop_stale_without_current() {
    let scratch = tempfile::tempdir().expect("scratch");
    let db = Database::open(&scratch.path().join("hit-set.db")).expect("open db");
    seed_resume_fixture(&db).expect("seed");

    let superseded =
        superseded_drawer_ids(&db, &[CORRECTION_ID, SUPERSEDED_ID]).expect("walk successor chain");
    assert!(
        superseded.contains(SUPERSEDED_ID),
        "stale must drop when correction is live and current is absent: {superseded:?}"
    );
    assert!(
        superseded.contains(CURRENT_ID),
        "current is also superseded by correction: {superseded:?}"
    );
}

#[test]
fn flat_context_drops_stale_when_correction_is_present() {
    let scratch = tempfile::tempdir().expect("scratch");
    let db = Database::open(&scratch.path().join("context.db")).expect("open db");
    seed_resume_fixture(&db).expect("seed");

    let pack = assemble_context_with_vector(
        &db,
        ContextRequest {
            query: "MempalAlpha durable memory storage decision after resume".to_string(),
            domain: MemoryDomain::Project,
            field: "general".to_string(),
            cwd: scratch.path().to_path_buf(),
            include_evidence: true,
            include_cards: true,
            max_items: 12,
            dao_tian_limit: 4,
            project_id: Some("mempal-alpha".to_string()),
            trigger: None,
            context_cfg_override: None,
            include_distill_suggestions: false,
        },
        &[0.1, 0.2, 0.3, 0.4],
    )
    .expect("assemble context");

    let mut rendered = String::new();
    for section in &pack.sections {
        for item in &section.items {
            rendered.push_str(&item.text);
            rendered.push('\n');
            rendered.push_str(&item.drawer_id);
            rendered.push('\n');
            rendered.push_str(&item.source_file);
            rendered.push('\n');
        }
    }
    assert!(
        !leaks_superseded(&rendered),
        "context must drop stale YAML drawer: {rendered}"
    );
}

#[test]
fn pinned_only_citations_are_not_sufficient() {
    let brief = CognitiveBrief {
        query: "resume".to_string(),
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        search_mode: "bm25".to_string(),
        warnings: Vec::new(),
        summary: BriefSummary {
            narrative: "Pinned only.".to_string(),
            key_fact_count: 0,
            evidence_count: 1,
            card_count: 0,
            unresolved_count: 0,
            uncertainty_count: 0,
        },
        key_facts: Vec::new(),
        evidence: vec![BriefEvidence {
            text: "cite drawers".to_string(),
            citation: BriefCitation {
                drawer_id: "cited_recall_citation_policy".to_string(),
                source_file: PINNED_SOURCE.to_string(),
                anchor_kind: AnchorKind::Repo,
                anchor_id: "repo".to_string(),
                card_id: None,
            },
        }],
        cards: Vec::new(),
        entities: Vec::new(),
        unresolved_items: Vec::new(),
        uncertainty: Vec::new(),
        next_actions: Vec::new(),
    };
    let brief_surface = format!("drawer: cited_recall_citation_policy {PINNED_SOURCE}");
    let context_surface = format!("drawer: cited_recall_citation_policy {PINNED_SOURCE}");
    assert!(
        !has_required_citations(&brief, &brief_surface, &context_surface),
        "pinned is extra, not a live decision"
    );
    let live = format!("drawer: {CORRECTION_ID} {CORRECTION_SOURCE}");
    let live_brief = CognitiveBrief {
        evidence: vec![BriefEvidence {
            text: "keep sqlite".to_string(),
            citation: BriefCitation {
                drawer_id: CORRECTION_ID.to_string(),
                source_file: CORRECTION_SOURCE.to_string(),
                anchor_kind: AnchorKind::Repo,
                anchor_id: "repo".to_string(),
                card_id: None,
            },
        }],
        ..brief
    };
    assert!(has_required_citations(&live_brief, &live, &live));
    let continuation = format!("drawer: cited_recall_continuation {CONTINUATION_SOURCE}");
    assert!(has_required_citations(
        &live_brief,
        &continuation,
        &continuation
    ));
}

#[test]
fn cited_recall_surfaces_omit_hermes_prefetch() {
    let report = run_cited_recall_bench().expect("cited recall bench should run");
    assert!(
        !report
            .surfaces
            .iter()
            .any(|surface| *surface == "hermes_prefetch"),
        "hermes_prefetch is a local clone, not a production surface: {:?}",
        report.surfaces
    );
}
