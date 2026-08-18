use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tempfile::tempdir;

use crate::brief::{BriefRequest, CognitiveBrief, assemble_brief_from_bm25_for_project};
use crate::context::{ContextPack, ContextRequest, assemble_context_with_vector};
use crate::core::{
    compaction::merge_cluster,
    db::Database,
    types::{BootstrapEvidenceArgs, CompactionStrategy, Drawer, MemoryDomain, SourceType},
};
use crate::hook::render_hook_brief_context;
use crate::ingest::normalize::CURRENT_NORMALIZE_VERSION;
use crate::search::bm25_fallback_warning_embed_error;

const DATASET_ID: &str = "cited_recall_resume_compaction_v1";
const DATASET_VERSION: &str = "2026-08-18";
const SCHEMA_VERSION: &str = "mempal.cited_recall_bench.v1";
const PROJECT_ID: &str = "mempal-alpha";
const FOREIGN_PROJECT_ID: &str = "mempal-alpha-notes";
const QUERY: &str = "MempalAlpha durable memory storage decision after resume";
const SUPERSEDED_MARKER: &str = "YAML files";
const FOREIGN_MARKER: &str = "Redis";
const CURRENT_DECISION: &str =
    "Current decision: MempalAlpha uses SQLite for durable memory storage.";
const CORRECTION: &str =
    "Correction: keep SQLite; do not switch MempalAlpha durable storage to ad-hoc notes.";
const CONTINUATION: &str =
    "Continuation after compaction: resume MempalAlpha with the SQLite storage decision.";
const SUPERSEDED: &str = "Old decision: MempalAlpha uses YAML files for durable memory storage.";
const FOREIGN: &str = "MempalAlpha-notes uses Redis for memory storage in a different project.";
const PINNED: &str = "Pinned: MempalAlpha answers must cite drawer_id and source_file.";
const CURRENT_SOURCE: &str = "fixture://cited-recall/mempal-alpha/sqlite-current.md";
const CORRECTION_SOURCE: &str = "fixture://cited-recall/mempal-alpha/sqlite-correction.md";
const CONTINUATION_SOURCE: &str = "fixture://cited-recall/mempal-alpha/continuation.md";
const SUPERSEDED_SOURCE: &str = "fixture://cited-recall/mempal-alpha/yaml-stale.md";
const FOREIGN_SOURCE: &str = "fixture://cited-recall/mempal-alpha-notes/redis.md";
const PINNED_SOURCE: &str = "fixture://cited-recall/mempal-alpha/citation-policy.md";
const CURRENT_ID: &str = "cited_recall_sqlite_current";
const CORRECTION_ID: &str = "cited_recall_sqlite_correction";
const CONTINUATION_ID: &str = "cited_recall_continuation";
const SUPERSEDED_ID: &str = "cited_recall_yaml_stale";
const FOREIGN_ID: &str = "cited_recall_foreign_redis";
const PINNED_ID: &str = "cited_recall_citation_policy";
const BUDGET_TOKENS: usize = 8_000;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CitedRecallBenchReport {
    pub schema_version: &'static str,
    pub dataset: CitedRecallDatasetSummary,
    pub reproducibility: CitedRecallReproducibility,
    pub scenario_count: usize,
    pub failed_scenarios: usize,
    pub latest_decision: ScenarioCheck,
    pub citations: ScenarioCheck,
    pub foreign_project: ScenarioCheck,
    pub deterministic_order: ScenarioCheck,
    pub context_budget: ScenarioCheck,
    pub no_evidence_fallback: ScenarioCheck,
    pub remote_calls: u64,
    pub surfaces: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CitedRecallDatasetSummary {
    pub id: &'static str,
    pub version: &'static str,
    pub source: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CitedRecallReproducibility {
    pub deterministic: bool,
    pub fixture_seed: &'static str,
    pub provider_calls_enabled: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ScenarioCheck {
    pub name: &'static str,
    pub passed: bool,
    pub failures: usize,
}

pub fn cited_recall_bench_passes(report: &CitedRecallBenchReport) -> bool {
    report.failed_scenarios == 0
        && report.latest_decision.passed
        && report.citations.passed
        && report.foreign_project.passed
        && report.deterministic_order.passed
        && report.context_budget.passed
        && report.no_evidence_fallback.passed
        && report.remote_calls == 0
}

pub fn run_cited_recall_bench_command() -> Result<()> {
    let report = run_cited_recall_bench()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).context("serialize cited recall bench report")?
    );
    if !cited_recall_bench_passes(&report) {
        bail!(
            "cited recall bench failed: {} scenario(s)",
            report.failed_scenarios
        );
    }
    Ok(())
}

pub fn run_cited_recall_bench() -> Result<CitedRecallBenchReport> {
    let scratch = tempdir().context("create cited recall scratch dir")?;
    let cwd = scratch.path().to_path_buf();
    let db = Database::open(&scratch.path().join("cited-recall.db"))
        .context("open cited recall database")?;
    seed_resume_fixture(&db)?;

    let warning = bm25_fallback_warning_embed_error("cited recall bench uses BM25");
    let brief = assemble_brief_from_bm25_for_project(
        &db,
        brief_request(QUERY, cwd.clone()),
        warning,
        Some(PROJECT_ID.to_string()),
    )
    .context("assemble cited recall brief")?;
    let pinned = db
        .get_pinned_facts(Some(PROJECT_ID), BUDGET_TOKENS.saturating_mul(4))
        .context("load pinned facts")?;
    let context = assemble_context_with_vector(
        &db,
        context_request(QUERY, cwd.clone()),
        &[0.1, 0.2, 0.3, 0.4],
    )
    .context("assemble cited recall context")?;
    let empty_db = Database::open(&scratch.path().join("empty.db")).context("open empty db")?;
    let empty_brief = assemble_brief_from_bm25_for_project(
        &empty_db,
        brief_request(QUERY, cwd),
        bm25_fallback_warning_embed_error("cited recall empty-evidence uses BM25"),
        Some(PROJECT_ID.to_string()),
    )
    .context("assemble empty-evidence brief")?;

    let brief_text = render_hook_brief_context(&pinned, &brief);
    let hermes_text = render_hermes_prefetch_context(&pinned, &brief);
    let context_text = render_context_surface(&context);
    let surfaces = [
        brief_text.as_str(),
        hermes_text.as_str(),
        context_text.as_str(),
    ];

    let latest_decision = check(
        "latest_decision",
        [&brief_text, &hermes_text]
            .iter()
            .filter(|text| leaks_superseded(text))
            .count(),
    );
    let citations = check(
        "citations",
        usize::from(!has_required_citations(&brief, &surfaces)),
    );
    let foreign_project = check(
        "foreign_project",
        surfaces.iter().filter(|text| leaks_foreign(text)).count(),
    );
    let deterministic_order = check(
        "deterministic_order",
        usize::from(!order_is_deterministic(&brief_text, &hermes_text)),
    );
    let context_budget = check(
        "context_budget",
        usize::from(!budget_holds(&brief_text, &hermes_text, &context_text)),
    );
    let no_evidence_fallback = check(
        "no_evidence_fallback",
        usize::from(!empty_evidence_is_bounded(&empty_brief)),
    );
    let checks = [
        &latest_decision,
        &citations,
        &foreign_project,
        &deterministic_order,
        &context_budget,
        &no_evidence_fallback,
    ];
    let failed_scenarios = checks.iter().filter(|check| !check.passed).count();

    Ok(CitedRecallBenchReport {
        schema_version: SCHEMA_VERSION,
        dataset: CitedRecallDatasetSummary {
            id: DATASET_ID,
            version: DATASET_VERSION,
            source: "built_in",
        },
        reproducibility: CitedRecallReproducibility {
            deterministic: true,
            fixture_seed: "cited-recall-fixed",
            provider_calls_enabled: false,
        },
        scenario_count: checks.len(),
        failed_scenarios,
        latest_decision,
        citations,
        foreign_project,
        deterministic_order,
        context_budget,
        no_evidence_fallback,
        remote_calls: 0,
        surfaces: vec![
            "mempal_brief",
            "codex_hook",
            "hermes_prefetch",
            "mempal_context",
        ],
    })
}

fn check(name: &'static str, failures: usize) -> ScenarioCheck {
    ScenarioCheck {
        name,
        passed: failures == 0,
        failures,
    }
}

fn brief_request(query: &str, cwd: PathBuf) -> BriefRequest {
    BriefRequest {
        query: query.to_string(),
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        cwd,
        max_items: 12,
        dao_tian_limit: 4,
    }
}

fn context_request(query: &str, cwd: PathBuf) -> ContextRequest {
    ContextRequest {
        query: query.to_string(),
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        cwd,
        include_evidence: true,
        include_cards: true,
        max_items: 12,
        dao_tian_limit: 4,
        project_id: Some(PROJECT_ID.to_string()),
        trigger: None,
        context_cfg_override: None,
        include_distill_suggestions: false,
    }
}

fn seed_resume_fixture(db: &Database) -> Result<()> {
    FixtureEvidence {
        id: SUPERSEDED_ID,
        content: SUPERSEDED,
        project_id: PROJECT_ID,
        source_file: SUPERSEDED_SOURCE,
        added_at: "1710000000",
        pinned: false,
        supersedes: None,
        importance: 2,
    }
    .insert(db)?;
    FixtureEvidence {
        id: CURRENT_ID,
        content: CURRENT_DECISION,
        project_id: PROJECT_ID,
        source_file: CURRENT_SOURCE,
        added_at: "1710000100",
        pinned: false,
        supersedes: Some(SUPERSEDED_ID),
        importance: 4,
    }
    .insert(db)?;
    FixtureEvidence {
        id: CORRECTION_ID,
        content: CORRECTION,
        project_id: PROJECT_ID,
        source_file: CORRECTION_SOURCE,
        added_at: "1710000200",
        pinned: false,
        supersedes: Some(CURRENT_ID),
        importance: 5,
    }
    .insert(db)?;
    FixtureEvidence {
        id: CONTINUATION_ID,
        content: CONTINUATION,
        project_id: PROJECT_ID,
        source_file: CONTINUATION_SOURCE,
        added_at: "1710000300",
        pinned: false,
        supersedes: None,
        importance: 3,
    }
    .insert(db)?;
    FixtureEvidence {
        id: FOREIGN_ID,
        content: FOREIGN,
        project_id: FOREIGN_PROJECT_ID,
        source_file: FOREIGN_SOURCE,
        added_at: "1710000400",
        pinned: false,
        supersedes: None,
        importance: 4,
    }
    .insert(db)?;
    FixtureEvidence {
        id: PINNED_ID,
        content: PINNED,
        project_id: PROJECT_ID,
        source_file: PINNED_SOURCE,
        added_at: "1710000500",
        pinned: true,
        supersedes: None,
        importance: 5,
    }
    .insert(db)?;
    merge_cluster(
        db,
        &[CORRECTION_ID.to_string(), CONTINUATION_ID.to_string()],
        CompactionStrategy::RichestContent,
        false,
    )
    .context("compact correction/continuation")?;
    Ok(())
}

struct FixtureEvidence {
    id: &'static str,
    content: &'static str,
    project_id: &'static str,
    source_file: &'static str,
    added_at: &'static str,
    pinned: bool,
    supersedes: Option<&'static str>,
    importance: i32,
}

impl FixtureEvidence {
    fn insert(self, db: &Database) -> Result<()> {
        let drawer = Drawer {
            normalize_version: CURRENT_NORMALIZE_VERSION,
            is_pinned: self.pinned,
            pin_order: self.pinned.then_some(1),
            supersedes: self.supersedes.map(ToOwned::to_owned),
            ..Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
                id: self.id.to_string(),
                content: self.content.to_string(),
                wing: "cited_recall".to_string(),
                room: Some("decision".to_string()),
                source_file: Some(self.source_file.to_string()),
                source_type: SourceType::AgentObservation,
                added_at: self.added_at.to_string(),
                chunk_index: Some(0),
                importance: self.importance,
            })
        };
        db.insert_drawer_with_project(&drawer, Some(self.project_id))
            .with_context(|| format!("seed cited recall drawer {}", self.id))?;
        db.insert_vector_with_project(self.id, &[0.1, 0.2, 0.3, 0.4], Some(self.project_id))
            .with_context(|| format!("seed cited recall vector {}", self.id))?;
        Ok(())
    }
}

fn leaks_superseded(text: &str) -> bool {
    text.contains(SUPERSEDED_MARKER)
        || text.contains(SUPERSEDED_ID)
        || text.contains(SUPERSEDED_SOURCE)
}

fn leaks_foreign(text: &str) -> bool {
    text.contains(FOREIGN_MARKER) || text.contains(FOREIGN_ID) || text.contains(FOREIGN_SOURCE)
}

fn has_required_citations(brief: &CognitiveBrief, surfaces: &[&str]) -> bool {
    let cited_sources = brief
        .evidence
        .iter()
        .map(|item| item.citation.source_file.as_str())
        .chain(
            brief
                .key_facts
                .iter()
                .map(|item| item.citation.source_file.as_str()),
        )
        .collect::<Vec<_>>();
    let has_live_source = [
        CURRENT_SOURCE,
        CORRECTION_SOURCE,
        CONTINUATION_SOURCE,
        PINNED_SOURCE,
    ]
    .iter()
    .any(|source| {
        cited_sources.contains(source) || surfaces.iter().any(|text| text.contains(source))
    });
    has_live_source
        && surfaces
            .iter()
            .all(|text| text.contains("drawer:") || text.contains("drawer_id"))
}

fn order_is_deterministic(brief_text: &str, hermes_text: &str) -> bool {
    let pinned_at = brief_text.find("## Pinned facts");
    let ranked_at = brief_text
        .find("## Evidence")
        .or_else(|| brief_text.find("## Key Facts"))
        .or_else(|| brief_text.find("## Summary"));
    let hermes_ok = hermes_text.contains("## Pinned Facts");
    pinned_at
        .zip(ranked_at)
        .is_some_and(|(pinned, ranked)| pinned < ranked)
        && hermes_ok
}

fn budget_holds(brief_text: &str, hermes_text: &str, context_text: &str) -> bool {
    [brief_text, hermes_text, context_text]
        .into_iter()
        .all(|text| crate::embed::estimate_tokens(text) <= BUDGET_TOKENS)
}

fn empty_evidence_is_bounded(brief: &CognitiveBrief) -> bool {
    let narrative = brief.summary.narrative.to_lowercase();
    brief.key_facts.is_empty()
        && brief.evidence.is_empty()
        && brief.cards.is_empty()
        && (narrative.contains("no cited") || narrative.contains("unsupported"))
        && brief.uncertainty.iter().any(|item| {
            item.kind == "no_evidence" || item.message.to_lowercase().contains("no cited")
        })
        && !narrative.contains("i remember")
}

fn render_context_surface(pack: &ContextPack) -> String {
    let mut out = String::from("## Context\n");
    for section in &pack.sections {
        out.push_str(&format!("## {}\n", section.name));
        for item in &section.items {
            out.push_str(&format!(
                "- {}\n  drawer: {}\n  source: {}\n",
                item.text, item.drawer_id, item.source_file
            ));
        }
    }
    out
}

fn render_hermes_prefetch_context(
    pinned: &[crate::core::types::Drawer],
    brief: &CognitiveBrief,
) -> String {
    // Matches contrib/hermes-agent-plugin prefetch/pinned citation labels.
    let mut lines = vec!["## Mempal Memory".to_string()];
    if !pinned.is_empty() {
        lines.push("## Pinned Facts (always active)".to_string());
        for drawer in pinned {
            let source = drawer.source_file.as_deref().unwrap_or("unknown");
            lines.push(format!(
                "- [authoritative/pinned][{}] {} (drawer_id: {}, source: {}, importance: {})",
                drawer.memory_kind.as_str(),
                drawer.content.replace('\n', " "),
                drawer.id,
                source,
                drawer.importance
            ));
        }
    }
    for ev in &brief.evidence {
        lines.push(format!(
            "- [evidence] {} (drawer_id: {}, source: {}, importance: 0)",
            ev.text.replace('\n', " "),
            ev.citation.drawer_id,
            ev.citation.source_file
        ));
    }
    for fact in &brief.key_facts {
        lines.push(format!(
            "- [evidence] {} (drawer_id: {}, source: {}, importance: 0)",
            fact.text.replace('\n', " "),
            fact.citation.drawer_id,
            fact.citation.source_file
        ));
    }
    lines.join("\n")
}
