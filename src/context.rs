#![warn(clippy::all)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::Serialize;
use thiserror::Error;

use crate::core::{
    anchor,
    config::ContextConfig,
    db::Database,
    db::DbError,
    patterns::PatternSummary,
    skills::SkillForContext,
    types::{
        AnchorKind, KnowledgeCard, KnowledgeCardFilter, KnowledgeEvidenceLink,
        KnowledgeEvidenceRole, KnowledgeStatus, KnowledgeTier, MemoryDomain, MemoryKind,
        RouteDecision, SearchResult, TriggerHints,
    },
};
use crate::embed::{EmbedError, Embedder};
use crate::search::tiered::{
    BudgetUsed, ContextTrigger, T1Params, T3Params, TieredItem, compute_budgets, fetch_t1,
    fetch_t3, fetch_t3_kg, now_unix_secs,
};
use crate::search::{SearchError, SearchFilters, SearchOptions, search_with_vector_options};

pub type Result<T> = std::result::Result<T, ContextError>;

#[derive(Debug, Error)]
pub enum ContextError {
    #[error("failed to derive context anchors")]
    DeriveAnchor(#[from] anchor::AnchorError),
    #[error("failed to embed context query")]
    EmbedQuery(#[source] EmbedError),
    #[error("embedder returned no context query vector")]
    MissingQueryVector,
    #[error("failed to search context candidates")]
    Search(#[source] SearchError),
    #[error("failed to load context drawer metadata")]
    LoadDrawer(#[source] DbError),
    #[error("failed to load context card metadata")]
    LoadCard(#[source] DbError),
    #[error("tiered retrieval failed")]
    Tiered(#[source] crate::search::tiered::TieredError),
}

/// Re-export for convenience so callers don't need to import search::tiered.
pub use crate::search::tiered::ContextTrigger as Trigger;

#[derive(Debug, Clone)]
pub struct ContextRequest {
    pub query: String,
    pub domain: MemoryDomain,
    pub field: String,
    pub cwd: PathBuf,
    pub include_evidence: bool,
    pub include_cards: bool,
    pub max_items: usize,
    pub dao_tian_limit: usize,
    /// Optional project scope for pattern filtering (P13).
    pub project_id: Option<String>,
    /// Trigger hint for tiered retrieval budget weights (P14).
    pub trigger: Option<ContextTrigger>,
    /// Override for context assembly config; None → use global ConfigHandle.
    pub context_cfg_override: Option<ContextConfig>,
    /// P106: include the read-only `distill_suggestions` signal. Defaults to
    /// true at the CLI/MCP surfaces; never changes the assembled sections.
    pub include_distill_suggestions: bool,
}

/// T1/T2/T3 tiered assembly result (P14).
#[derive(Debug, Clone, Serialize)]
pub struct TieredAssembly {
    pub t1_items: Vec<TieredItem>,
    pub t2_items: Vec<TieredItem>,
    pub t3_items: Vec<TieredItem>,
    pub budget_used: BudgetUsed,
    /// Active skills injected at the head of T1 (P15).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_skills: Vec<SkillForContext>,
}

/// P106 distill-signal thresholds. Fixed constants in v1 (not config-tunable).
pub const DISTILL_SIGNAL_MIN_EVIDENCE: i64 = 5;
pub const DISTILL_SIGNAL_MAX_SUGGESTIONS: usize = 3;
pub const DISTILL_SIGNAL_SAMPLE_LIMIT: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextAnchor {
    pub anchor_kind: AnchorKind,
    pub anchor_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextPack {
    pub query: String,
    pub domain: MemoryDomain,
    pub field: String,
    pub anchors: Vec<ContextAnchor>,
    pub sections: Vec<ContextSection>,
    /// Active patterns surfaced as recurring themes (P13).
    pub recurring_themes: Vec<PatternSummary>,
    /// Tiered assembly result (P14). Present only when tiered_retrieval_enabled=true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiered: Option<TieredAssembly>,
    /// Active repair warnings injected at T1 priority (P14 decision-repair).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repair_warnings: Vec<crate::repair::RepairWarning>,
    /// Active skills injected at T1 head priority (P15 skill crystallization).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_skills: Vec<SkillForContext>,
    /// P106: read-only signal flagging fields where evidence is dense but no
    /// promoted knowledge exists yet. Empty when disabled or nothing qualifies.
    /// Never alters `sections`.
    pub distill_suggestions: Vec<DistillSuggestion>,
}

/// P106: a read-only suggestion that a `field` is worth distilling. The agent
/// MAY act on it via the explicit distill -> gate lifecycle; mempal never acts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DistillSuggestion {
    pub field: String,
    pub evidence_count: usize,
    pub sample_evidence_drawer_ids: Vec<String>,
    pub suggested_tier: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextSection {
    pub name: String,
    pub items: Vec<ContextItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextItem {
    pub drawer_id: String,
    pub source_file: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<KnowledgeTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<KnowledgeStatus>,
    pub anchor_kind: AnchorKind,
    pub anchor_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_anchor_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_hints: Option<TriggerHints>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub evidence_citations: Vec<ContextEvidenceCitation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextEvidenceCitation {
    pub evidence_drawer_id: String,
    pub role: KnowledgeEvidenceRole,
    pub source_file: String,
}

#[derive(Debug, Clone)]
struct AnchorCandidate {
    anchor_kind: AnchorKind,
    anchor_id: String,
    domain: MemoryDomain,
}

#[derive(Debug, Clone)]
struct CandidateQuery<'a> {
    request: &'a ContextRequest,
    query_vector: &'a [f32],
    route: &'a RouteDecision,
    anchor: &'a AnchorCandidate,
    memory_kind: MemoryKind,
    tier: Option<KnowledgeTier>,
    status: Option<KnowledgeStatus>,
    top_k: usize,
}

struct CardAppendState<'a> {
    seen: &'a mut BTreeSet<String>,
    items: &'a mut Vec<ContextItem>,
    remaining: &'a mut usize,
    tier_remaining: &'a mut usize,
}

pub async fn assemble_context<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    request: ContextRequest,
) -> Result<ContextPack> {
    let query_vector = embedder
        .embed(&[request.query.as_str()])
        .await
        .map_err(ContextError::EmbedQuery)?
        .into_iter()
        .next()
        .ok_or(ContextError::MissingQueryVector)?;

    assemble_context_with_vector(db, request, &query_vector)
}

pub fn assemble_context_with_vector(
    db: &Database,
    request: ContextRequest,
    query_vector: &[f32],
) -> Result<ContextPack> {
    // Determine effective context config (override → global → default).
    let cfg = request
        .context_cfg_override
        .clone()
        .unwrap_or_else(|| crate::core::config::ConfigHandle::current().context.clone());

    if cfg.tiered_retrieval_enabled {
        assemble_tiered(db, request, query_vector, &cfg)
    } else {
        assemble_flat(db, request, query_vector)
    }
}

// --- Tiered path (tiered_retrieval_enabled = true) ---

fn assemble_tiered(
    db: &Database,
    request: ContextRequest,
    query_vector: &[f32],
    cfg: &ContextConfig,
) -> Result<ContextPack> {
    let trigger = request.trigger.unwrap_or_default();
    let now = now_unix_secs();
    let project_id = request.project_id.as_deref();

    let (t1_budget, t2_budget, t3_budget) = compute_budgets(
        cfg.budget.total_tokens,
        cfg.budget.t1_ratio,
        cfg.budget.t2_ratio,
        cfg.budget.t3_ratio,
        trigger,
    );

    // T1: decision/feedback/rule drawers scored by importance × recency.
    let t1_items = fetch_t1(
        db,
        T1Params {
            min_importance: cfg.min_t1_importance,
            lambda: cfg.t1_recency_lambda,
            budget_tokens: t1_budget,
            project_id,
            now_unix: now,
        },
    )
    .map_err(ContextError::Tiered)?;

    let t1_ids: Vec<String> = t1_items.iter().map(|i| i.drawer_id.clone()).collect();
    let t1_tokens_used: usize = t1_items
        .iter()
        .map(|i| crate::embed::estimate_tokens(&i.content))
        .sum();

    // T3: recent drawers.
    let t3_items = fetch_t3(
        db,
        T3Params {
            recency_window_days: cfg.t3_recency_window_days,
            budget_tokens: t3_budget,
            project_id,
            exclude_ids: &t1_ids,
            now_unix: now,
        },
    )
    .map_err(ContextError::Tiered)?;

    let t3_ids: Vec<String> = t3_items.iter().map(|i| i.drawer_id.clone()).collect();
    let t3_tokens_used: usize = t3_items
        .iter()
        .map(|i| crate::embed::estimate_tokens(&i.content))
        .sum();

    // T3 KG supplement: drawers related by KG triples.
    let query_terms: Vec<&str> = request.query.split_whitespace().collect();
    let kg_budget = t3_budget.saturating_sub(t3_tokens_used);
    let mut exclude_ids = t1_ids.clone();
    exclude_ids.extend(t3_ids);
    let t3_kg_items = if kg_budget > 0 {
        exclude_ids.sort();
        fetch_t3_kg(db, &query_terms, kg_budget, project_id, &exclude_ids, now)
            .map_err(ContextError::Tiered)?
    } else {
        vec![]
    };
    let t3_kg_tokens: usize = t3_kg_items
        .iter()
        .map(|i| crate::embed::estimate_tokens(&i.content))
        .sum();

    // Merge T3 items (recency + KG); combined exclusion set for T2.
    let t3_all: Vec<TieredItem> = t3_items.into_iter().chain(t3_kg_items).collect();
    let t3_tokens_used = t3_tokens_used + t3_kg_tokens;

    // T2: hybrid search (BM25 + vector + RRF).
    let t1_t3_ids: Vec<String> = t1_items
        .iter()
        .chain(t3_all.iter())
        .map(|i| i.drawer_id.clone())
        .collect();
    let t2_overflow = if cfg.budget.overflow_to_t2 {
        t1_budget.saturating_sub(t1_tokens_used) + t3_budget.saturating_sub(t3_tokens_used)
    } else {
        0
    };
    let effective_t2_budget = t2_budget + t2_overflow;
    let t2_results = search_t2_hybrid(db, &request, query_vector, &t1_t3_ids, effective_t2_budget)?;

    let t2_tokens_used: usize = t2_results
        .iter()
        .map(|i| crate::embed::estimate_tokens(&i.content))
        .sum();

    let budget_used = BudgetUsed {
        t1_tokens: t1_tokens_used,
        t2_tokens: t2_tokens_used,
        t3_tokens: t3_tokens_used,
    };

    // Build legacy sections for backward compat.
    let sections = build_tiered_sections(db, &t1_items, &t2_results, &t3_all, &request)?;

    let active_skills = load_active_skills(
        db,
        project_id,
        query_vector,
        &crate::core::config::ConfigHandle::current().skills,
    );

    let tiered = TieredAssembly {
        t1_items,
        t2_items: t2_results,
        t3_items: t3_all,
        budget_used,
        active_skills: active_skills.clone(),
    };

    let anchors = context_anchors(&request)?;
    let recurring_themes = load_recurring_themes(db, project_id);
    let repair_warnings = load_repair_warnings(db, project_id);
    let distill_suggestions = if request.include_distill_suggestions {
        detect_distill_suggestions(db)?
    } else {
        Vec::new()
    };

    Ok(ContextPack {
        query: request.query,
        domain: request.domain,
        field: request.field,
        anchors: anchors
            .into_iter()
            .map(|a| ContextAnchor {
                anchor_kind: a.anchor_kind,
                anchor_id: a.anchor_id,
            })
            .collect(),
        sections,
        recurring_themes,
        tiered: Some(tiered),
        repair_warnings,
        active_skills,
        distill_suggestions,
    })
}

/// Run T2 hybrid search, excluding already-used drawer IDs, budget-capped.
fn search_t2_hybrid(
    db: &Database,
    request: &ContextRequest,
    query_vector: &[f32],
    exclude_ids: &[String],
    budget_tokens: usize,
) -> Result<Vec<TieredItem>> {
    let route = RouteDecision {
        wing: None,
        room: None,
        confidence: 0.0,
        reason: "tiered T2 search".to_string(),
    };

    let anchors = context_anchors(request)?;
    let exclude_set: BTreeSet<&str> = exclude_ids.iter().map(String::as_str).collect();
    let mut items = Vec::new();
    let mut used = 0usize;
    let mut seen = BTreeSet::new();

    for anchor in &anchors {
        if used >= budget_tokens {
            break;
        }
        let filters = SearchFilters {
            memory_kind: None,
            domain: Some(domain_slug(&anchor.domain).to_string()),
            field: Some(request.field.clone()),
            tier: None,
            status: None,
            anchor_kind: Some(anchor_kind_slug(&anchor.anchor_kind).to_string()),
        };
        let results = search_with_vector_options(
            db,
            &request.query,
            query_vector,
            route.clone(),
            SearchOptions {
                filters,
                with_neighbors: false,
                ..SearchOptions::default()
            },
            50,
        )
        .map_err(ContextError::Search)?;

        for result in results {
            if result.anchor_id != anchor.anchor_id {
                continue;
            }
            if !seen.insert(result.drawer_id.clone()) {
                continue;
            }
            if exclude_set.contains(result.drawer_id.as_str()) {
                continue;
            }
            let tokens = crate::embed::estimate_tokens(&result.content);
            if used + tokens > budget_tokens && !items.is_empty() {
                break;
            }
            used += tokens;
            items.push(TieredItem {
                drawer_id: result.drawer_id,
                content: result.content,
                source_file: result.source_file,
                room: result.room,
                t3_source: None,
                effective_importance: result.effective_importance,
                added_at_unix: 0,
                matched_pattern_id: result.matched_pattern_id,
            });
        }
    }

    Ok(items)
}

/// Build backward-compat `sections` array from tiered items.
fn build_tiered_sections(
    db: &Database,
    t1: &[TieredItem],
    t2: &[TieredItem],
    t3: &[TieredItem],
    request: &ContextRequest,
) -> Result<Vec<ContextSection>> {
    let mut sections = Vec::new();

    if !t1.is_empty() {
        sections.push(ContextSection {
            name: "dao_tian".to_string(),
            items: t1
                .iter()
                .map(|item| tiered_to_context_item(db, item, request))
                .collect::<Result<Vec<_>>>()?,
        });
    }

    if !t2.is_empty() {
        sections.push(ContextSection {
            name: "shu".to_string(),
            items: t2
                .iter()
                .map(|item| tiered_to_context_item(db, item, request))
                .collect::<Result<Vec<_>>>()?,
        });
    }

    if !t3.is_empty() {
        sections.push(ContextSection {
            name: "qi".to_string(),
            items: t3
                .iter()
                .map(|item| tiered_to_context_item(db, item, request))
                .collect::<Result<Vec<_>>>()?,
        });
    }

    Ok(sections)
}

fn tiered_to_context_item(
    db: &Database,
    item: &TieredItem,
    request: &ContextRequest,
) -> Result<ContextItem> {
    let trigger_hints = db
        .get_drawer(&item.drawer_id)
        .map_err(ContextError::LoadDrawer)?
        .and_then(|d| d.trigger_hints);
    // Derive anchor context using anchors built from request.
    let anchors = context_anchors(request)?;
    let first = anchors.into_iter().next();
    let (anchor_kind, anchor_id) = first
        .map(|a| (a.anchor_kind, a.anchor_id))
        .unwrap_or((AnchorKind::Global, "global://default".to_string()));
    Ok(ContextItem {
        drawer_id: item.drawer_id.clone(),
        source_file: item.source_file.clone(),
        text: item.content.clone(),
        card_id: None,
        tier: None,
        status: None,
        anchor_kind,
        anchor_id,
        parent_anchor_id: None,
        trigger_hints,
        evidence_citations: Vec::new(),
    })
}

// --- Flat path (tiered_retrieval_enabled = false, legacy behavior) ---

fn assemble_flat(
    db: &Database,
    request: ContextRequest,
    query_vector: &[f32],
) -> Result<ContextPack> {
    let anchors = context_anchors(&request)?;
    let route = RouteDecision {
        wing: None,
        room: None,
        confidence: 0.0,
        reason: "mind-model context assembly".to_string(),
    };

    let mut sections = Vec::new();
    let mut remaining = request.max_items;
    let mut seen = BTreeSet::new();

    for tier in tier_order() {
        if remaining == 0 {
            break;
        }
        let mut tier_remaining = if matches!(tier, KnowledgeTier::DaoTian) {
            request.dao_tian_limit.min(remaining)
        } else {
            remaining
        };
        if tier_remaining == 0 {
            continue;
        }
        let mut items = Vec::new();
        for anchor in &anchors {
            if remaining == 0 || tier_remaining == 0 {
                break;
            }
            for status in active_statuses() {
                if remaining == 0 || tier_remaining == 0 {
                    break;
                }
                let mut results = search_context_candidates(
                    db,
                    CandidateQuery {
                        request: &request,
                        query_vector,
                        route: &route,
                        anchor,
                        memory_kind: MemoryKind::Knowledge,
                        tier: Some(tier.clone()),
                        status: Some(status.clone()),
                        top_k: tier_remaining,
                    },
                )?;
                results.retain(|result| result.anchor_id == anchor.anchor_id);
                for result in results {
                    if remaining == 0 || tier_remaining == 0 {
                        break;
                    }
                    if !seen.insert(result.drawer_id.clone()) {
                        continue;
                    }
                    items.push(context_item_from_result(db, result)?);
                    remaining -= 1;
                    tier_remaining -= 1;
                }
            }
        }
        if !items.is_empty() {
            if request.include_cards && remaining > 0 {
                append_card_context_items(
                    db,
                    &request,
                    &anchors,
                    tier,
                    CardAppendState {
                        seen: &mut seen,
                        items: &mut items,
                        remaining: &mut remaining,
                        tier_remaining: &mut tier_remaining,
                    },
                )?;
            }
            sections.push(ContextSection {
                name: tier_slug(tier).to_string(),
                items,
            });
        } else if request.include_cards && remaining > 0 {
            append_card_context_items(
                db,
                &request,
                &anchors,
                tier,
                CardAppendState {
                    seen: &mut seen,
                    items: &mut items,
                    remaining: &mut remaining,
                    tier_remaining: &mut tier_remaining,
                },
            )?;
            if !items.is_empty() {
                sections.push(ContextSection {
                    name: tier_slug(tier).to_string(),
                    items,
                });
            }
        }
    }

    if request.include_evidence && remaining > 0 {
        let mut items = Vec::new();
        for anchor in &anchors {
            if remaining == 0 {
                break;
            }
            let mut results = search_context_candidates(
                db,
                CandidateQuery {
                    request: &request,
                    query_vector,
                    route: &route,
                    anchor,
                    memory_kind: MemoryKind::Evidence,
                    tier: None,
                    status: None,
                    top_k: remaining,
                },
            )?;
            results.retain(|result| result.anchor_id == anchor.anchor_id);
            for result in results {
                if remaining == 0 {
                    break;
                }
                if !seen.insert(result.drawer_id.clone()) {
                    continue;
                }
                items.push(context_item_from_result(db, result)?);
                remaining -= 1;
            }
        }
        if !items.is_empty() {
            sections.push(ContextSection {
                name: "evidence".to_string(),
                items,
            });
        }
    }

    let recurring_themes = load_recurring_themes(db, request.project_id.as_deref());
    let repair_warnings = load_repair_warnings(db, request.project_id.as_deref());
    let active_skills = load_active_skills(
        db,
        request.project_id.as_deref(),
        query_vector,
        &crate::core::config::ConfigHandle::current().skills,
    );
    let distill_suggestions = if request.include_distill_suggestions {
        detect_distill_suggestions(db)?
    } else {
        Vec::new()
    };

    Ok(ContextPack {
        query: request.query,
        domain: request.domain,
        field: request.field,
        anchors: anchors
            .into_iter()
            .map(|anchor| ContextAnchor {
                anchor_kind: anchor.anchor_kind,
                anchor_id: anchor.anchor_id,
            })
            .collect(),
        sections,
        recurring_themes,
        tiered: None,
        repair_warnings,
        active_skills,
        distill_suggestions,
    })
}

fn load_active_skills(
    db: &Database,
    project_id: Option<&str>,
    query_vector: &[f32],
    skills_cfg: &crate::core::config::SkillsConfig,
) -> Vec<SkillForContext> {
    if !crate::core::skills::skills_table_exists(db.conn()) {
        return vec![];
    }
    match crate::core::skills::load_active_skills_for_context(
        db.conn(),
        project_id,
        query_vector,
        skills_cfg.skill_surfacing_threshold as f32,
    ) {
        Ok(skills) => skills,
        Err(err) => {
            tracing::warn!(error = %err, "failed to load active skills for context");
            vec![]
        }
    }
}

fn load_repair_warnings(
    db: &Database,
    project_id: Option<&str>,
) -> Vec<crate::repair::RepairWarning> {
    let config = crate::core::config::ConfigHandle::current();
    if !config.repair.enabled {
        return vec![];
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    crate::repair::load_repair_warnings(db.conn(), &config.repair, project_id, now_ms)
}

fn load_recurring_themes(db: &Database, project_id: Option<&str>) -> Vec<PatternSummary> {
    if !crate::core::patterns::patterns_table_exists(db.conn()) {
        return vec![];
    }
    match crate::core::patterns::load_active_patterns_for_context(db.conn(), project_id) {
        Ok(patterns) => patterns
            .into_iter()
            .map(|p| {
                let exemplar_preview = p.exemplar_ids.first().and_then(|id| {
                    db.conn()
                        .query_row(
                            "SELECT SUBSTR(content, 1, 120) FROM drawers WHERE id = ?1",
                            [id],
                            |row| row.get::<_, String>(0),
                        )
                        .ok()
                });
                PatternSummary {
                    pattern_id: p.pattern_id,
                    topic_tags: p.topic_tags,
                    session_count: p.session_count,
                    exemplar_preview,
                }
            })
            .collect(),
        Err(err) => {
            tracing::warn!(error = %err, "failed to load active patterns for context");
            vec![]
        }
    }
}

/// P106 detector: deterministic, read-only. Flags each `field` whose active
/// evidence count is at least `DISTILL_SIGNAL_MIN_EVIDENCE` AND which has zero
/// active promoted-or-canonical knowledge. Returns at most
/// `DISTILL_SIGNAL_MAX_SUGGESTIONS`, ordered by descending evidence count then
/// ascending field. Performs no database writes and no LLM call.
fn detect_distill_suggestions(db: &Database) -> Result<Vec<DistillSuggestion>> {
    let mut qualifying: Vec<(String, i64)> = db
        .distill_field_counts()
        .map_err(ContextError::LoadDrawer)?
        .into_iter()
        .filter(|(_, evidence_count, promoted_count)| {
            *evidence_count >= DISTILL_SIGNAL_MIN_EVIDENCE && *promoted_count == 0
        })
        .map(|(field, evidence_count, _)| (field, evidence_count))
        .collect();

    qualifying.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    qualifying.truncate(DISTILL_SIGNAL_MAX_SUGGESTIONS);

    let mut suggestions = Vec::with_capacity(qualifying.len());
    for (field, evidence_count) in qualifying {
        let sample_evidence_drawer_ids = db
            .sample_evidence_drawer_ids(&field, DISTILL_SIGNAL_SAMPLE_LIMIT)
            .map_err(ContextError::LoadDrawer)?;
        suggestions.push(DistillSuggestion {
            field,
            evidence_count: evidence_count as usize,
            sample_evidence_drawer_ids,
            suggested_tier: "dao_ren".to_string(),
        });
    }
    Ok(suggestions)
}

fn context_anchors(request: &ContextRequest) -> Result<Vec<AnchorCandidate>> {
    let derived = anchor::derive_anchor_from_cwd(Some(&request.cwd))?;
    let mut anchors = Vec::new();
    anchors.push(AnchorCandidate {
        anchor_kind: AnchorKind::Worktree,
        anchor_id: derived.anchor_id,
        domain: request.domain,
    });

    let repo_anchor_id = derived
        .parent_anchor_id
        .unwrap_or_else(|| anchor::LEGACY_REPO_ANCHOR_ID.to_string());
    anchors.push(AnchorCandidate {
        anchor_kind: AnchorKind::Repo,
        anchor_id: repo_anchor_id,
        domain: request.domain,
    });

    // P12 backfilled existing drawers to repo://legacy. Keep it as a fallback
    // so the first runtime assembler remains useful on pre-anchor databases.
    anchors.push(AnchorCandidate {
        anchor_kind: AnchorKind::Repo,
        anchor_id: anchor::LEGACY_REPO_ANCHOR_ID.to_string(),
        domain: request.domain,
    });

    anchors.push(AnchorCandidate {
        anchor_kind: AnchorKind::Global,
        anchor_id: "global://default".to_string(),
        domain: MemoryDomain::Global,
    });

    Ok(dedup_anchors(anchors))
}

fn dedup_anchors(anchors: Vec<AnchorCandidate>) -> Vec<AnchorCandidate> {
    let mut seen = BTreeSet::new();
    anchors
        .into_iter()
        .filter(|anchor| {
            seen.insert((
                anchor_kind_slug(&anchor.anchor_kind).to_string(),
                anchor.anchor_id.clone(),
            ))
        })
        .collect()
}

fn search_context_candidates(
    db: &Database,
    query: CandidateQuery<'_>,
) -> Result<Vec<SearchResult>> {
    let filters = SearchFilters {
        memory_kind: Some(memory_kind_slug(&query.memory_kind).to_string()),
        domain: Some(domain_slug(&query.anchor.domain).to_string()),
        field: Some(query.request.field.clone()),
        tier: query.tier.as_ref().map(tier_slug).map(str::to_string),
        status: query.status.as_ref().map(status_slug).map(str::to_string),
        anchor_kind: Some(anchor_kind_slug(&query.anchor.anchor_kind).to_string()),
    };

    search_with_vector_options(
        db,
        &query.request.query,
        query.query_vector,
        query.route.clone(),
        SearchOptions {
            filters,
            with_neighbors: false,
            ..SearchOptions::default()
        },
        query.top_k,
    )
    .map_err(ContextError::Search)
}

fn context_item_from_result(db: &Database, result: SearchResult) -> Result<ContextItem> {
    let trigger_hints = db
        .get_drawer(&result.drawer_id)
        .map_err(ContextError::LoadDrawer)?
        .and_then(|drawer| drawer.trigger_hints);
    let text = match result.memory_kind {
        MemoryKind::Knowledge => result
            .statement
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(result.content.as_str())
            .to_string(),
        MemoryKind::Evidence | MemoryKind::ProfileFact => result.content,
    };
    Ok(ContextItem {
        drawer_id: result.drawer_id,
        source_file: result.source_file,
        text,
        tier: result.tier,
        status: result.status,
        anchor_kind: result.anchor_kind,
        anchor_id: result.anchor_id,
        parent_anchor_id: result.parent_anchor_id,
        trigger_hints,
        card_id: None,
        evidence_citations: Vec::new(),
    })
}

fn append_card_context_items(
    db: &Database,
    request: &ContextRequest,
    anchors: &[AnchorCandidate],
    tier: &KnowledgeTier,
    state: CardAppendState<'_>,
) -> Result<()> {
    for anchor in anchors {
        if *state.remaining == 0 || *state.tier_remaining == 0 {
            break;
        }
        for status in active_statuses() {
            if *state.remaining == 0 || *state.tier_remaining == 0 {
                break;
            }
            let cards = db
                .list_knowledge_cards(&KnowledgeCardFilter {
                    tier: Some(tier.clone()),
                    status: Some(status.clone()),
                    domain: Some(anchor.domain),
                    field: Some(request.field.clone()),
                    anchor_kind: Some(anchor.anchor_kind.clone()),
                    anchor_id: Some(anchor.anchor_id.clone()),
                    ..KnowledgeCardFilter::default()
                })
                .map_err(ContextError::LoadCard)?;
            for card in cards {
                if *state.remaining == 0 || *state.tier_remaining == 0 {
                    break;
                }
                let seen_key = format!("card:{}", card.id);
                if !state.seen.insert(seen_key) {
                    continue;
                }
                state.items.push(context_item_from_card(db, card)?);
                *state.remaining -= 1;
                *state.tier_remaining -= 1;
            }
        }
    }
    Ok(())
}

fn context_item_from_card(db: &Database, card: KnowledgeCard) -> Result<ContextItem> {
    let evidence_citations = db
        .knowledge_evidence_links(&card.id)
        .map_err(ContextError::LoadCard)?
        .into_iter()
        .map(|link| evidence_citation_from_link(db, link))
        .collect::<Result<Vec<_>>>()?;
    Ok(ContextItem {
        drawer_id: card.id.clone(),
        source_file: format!("knowledge-card://{}", card.id),
        text: card.statement.clone(),
        card_id: Some(card.id),
        tier: Some(card.tier),
        status: Some(card.status),
        anchor_kind: card.anchor_kind,
        anchor_id: card.anchor_id,
        parent_anchor_id: card.parent_anchor_id,
        trigger_hints: card.trigger_hints,
        evidence_citations,
    })
}

fn evidence_citation_from_link(
    db: &Database,
    link: KnowledgeEvidenceLink,
) -> Result<ContextEvidenceCitation> {
    let source_file = db
        .get_drawer(&link.evidence_drawer_id)
        .map_err(ContextError::LoadDrawer)?
        .and_then(|drawer| drawer.source_file)
        .unwrap_or_else(|| format!("drawer://{}", link.evidence_drawer_id));
    Ok(ContextEvidenceCitation {
        evidence_drawer_id: link.evidence_drawer_id,
        role: link.role,
        source_file,
    })
}

fn tier_order() -> &'static [KnowledgeTier] {
    &[
        KnowledgeTier::DaoTian,
        KnowledgeTier::DaoRen,
        KnowledgeTier::Shu,
        KnowledgeTier::Qi,
    ]
}

fn active_statuses() -> &'static [KnowledgeStatus] {
    &[KnowledgeStatus::Canonical, KnowledgeStatus::Promoted]
}

fn memory_kind_slug(value: &MemoryKind) -> &'static str {
    match value {
        MemoryKind::Evidence => "evidence",
        MemoryKind::Knowledge => "knowledge",
        MemoryKind::ProfileFact => "profile_fact",
    }
}

fn domain_slug(value: &MemoryDomain) -> &'static str {
    match value {
        MemoryDomain::Project => "project",
        MemoryDomain::User => "user",
        MemoryDomain::Agent => "agent",
        MemoryDomain::Skill => "skill",
        MemoryDomain::Global => "global",
    }
}

fn tier_slug(value: &KnowledgeTier) -> &'static str {
    match value {
        KnowledgeTier::Qi => "qi",
        KnowledgeTier::Shu => "shu",
        KnowledgeTier::DaoRen => "dao_ren",
        KnowledgeTier::DaoTian => "dao_tian",
    }
}

fn status_slug(value: &KnowledgeStatus) -> &'static str {
    match value {
        KnowledgeStatus::Candidate => "candidate",
        KnowledgeStatus::Active => "active",
        KnowledgeStatus::Superseded => "superseded",
        KnowledgeStatus::PendingReview => "pending_review",
        KnowledgeStatus::Promoted => "promoted",
        KnowledgeStatus::Canonical => "canonical",
        KnowledgeStatus::Demoted => "demoted",
        KnowledgeStatus::Retired => "retired",
    }
}

fn anchor_kind_slug(value: &AnchorKind) -> &'static str {
    match value {
        AnchorKind::Global => "global",
        AnchorKind::Repo => "repo",
        AnchorKind::Worktree => "worktree",
    }
}
