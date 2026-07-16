use super::resource_usage::ResourceUsageDto;
use crate::adoption_analytics::{RuntimeAdoptionAnalyticsGroup, RuntimeAdoptionAnalyticsReport};
use crate::brief::{
    BriefCard, BriefCitation, BriefEvidence, BriefEvidenceCitation, BriefFact, BriefSummary,
    BriefUncertainty, BriefUnresolvedItem, CognitiveBrief,
};
use crate::context::{ContextItem, ContextPack, ContextSection, DistillSuggestion, TieredAssembly};
use crate::core::phase3::{
    Phase3ReadinessReport, ResearchCandidateInsightPlan, ResearchEvidenceDrawerPlan,
    ResearchIngestPlanReport, RuntimeAdoptionCheckedRecordReport, RuntimeAdoptionGuidance,
    RuntimeAdoptionInstrumentationMode, RuntimeAdoptionInstrumentationPolicy,
    RuntimeAdoptionRecordPlan, RuntimeAdoptionRecordQualityReport, RuntimeAdoptionReviewFilters,
    RuntimeAdoptionReviewReport, RuntimeAdoptionSignalCounts, RuntimeAdoptionSignalGuidance,
    RuntimeAdoptionTrackGuidance,
};
use crate::core::types::{
    AnchorKind, ChunkNeighbors, KnowledgeCard, KnowledgeCardEvent, KnowledgeStatus, KnowledgeTier,
    MemoryDomain, MemoryKind, NeighborChunk, RouteDecision, RuntimeAdoptionEvent,
    RuntimeAdoptionSignal, RuntimeAdoptionTrack, SearchResult, TaxonomyEntry, TunnelEndpoint,
};
use crate::doctor::{DoctorDbReport, DoctorInstallReport, DoctorReport};
use crate::field_taxonomy::FieldTaxonomyEntry;
use crate::ingest::gating::GatingDecision;
use crate::ingest::novelty::NoveltyAction;
use crate::knowledge_anchor::PublishAnchorOutcome;
use crate::knowledge_card_lifecycle::{
    DemoteCardOutcome, KnowledgeCardGateReport, PromoteCardOutcome,
};
use crate::knowledge_card_retrieval::{RetrievedEvidenceCitation, RetrievedKnowledgeCard};
use crate::knowledge_distill::DistillOutcome;
use crate::knowledge_gate::{GateReport, PromotionPolicyEntry};
use crate::knowledge_lifecycle::{DemoteOutcome, PromoteOutcome};
use crate::process_diagnostics::DbHolderReport;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// schemars 1.x emits boolean `true` for `serde_json::Value`, which MCP clients
/// (e.g. Claude Code) reject when validating `tools/list`. Use this helper via
/// `#[schemars(schema_with = "json_object_schema")]` to advertise an object schema
/// instead, without changing the field's runtime Rust type.
fn json_object_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "object" })
}

pub const INGEST_SOURCE_TYPE_VALUES: [&str; 4] = [
    "user_explicit",
    "agent_observation",
    "agent_inference",
    "system_generated",
];
pub const INGEST_SOURCE_TYPE_VALUES_DESCRIPTION: &str =
    "user_explicit, agent_observation, agent_inference, system_generated";
pub const INGEST_SOURCE_TYPE_SCHEMA_DESCRIPTION: &str = concat!(
    "Optional source_type provenance: user_explicit, agent_observation, ",
    "agent_inference, system_generated."
);

fn ingest_source_type_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": ["string", "null"],
        "enum": [
            "user_explicit",
            "agent_observation",
            "agent_inference",
            "system_generated",
            null
        ],
        "description": INGEST_SOURCE_TYPE_SCHEMA_DESCRIPTION
    })
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct StatusRequest {
    /// Response detail level. Defaults to `compact`; `full` includes the
    /// protocol and AAAK reference text and mirrors CLI `status --full`.
    pub detail: Option<StatusDetail>,

    /// Scope breakdown to include. Defaults to the detail-level scope:
    /// `project` for compact responses and `all` for full responses.
    pub scope: Option<StatusScope>,

    /// Optional explicit project scope. When omitted, mempal resolves the
    /// current project from `[project]` config or MCP roots.
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusDetail {
    #[default]
    Compact,
    Full,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusScope {
    Project,
    All,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct RetrievalScopeRequest {
    /// Optional wing filter. This is a strict equality match.
    pub wing: Option<String>,
    /// Optional room filter. This is a strict equality match.
    pub room: Option<String>,
    /// Optional session filter for drawer-backed session captures. This maps
    /// to the existing `room` column and conflicts with `room` when both differ.
    pub session: Option<String>,
    /// Optional explicit project scope.
    pub project_id: Option<String>,
    /// Include legacy/global drawers (`project_id IS NULL`) alongside the
    /// current project. Ignored when `all_projects=true`.
    pub include_global: Option<bool>,
    /// Opt-in override to search across all projects for this request.
    pub all_projects: Option<bool>,
    /// Optional memory kind filter (`evidence`, `knowledge`, `atomic_fact`,
    /// `decision`, `case`, `skill`, `foresight`, `profile_fact`, or
    /// `profile_trait`).
    pub memory_kind: Option<String>,
    /// Optional domain filter (`project`, `user`, `agent`, `skill`, `global`).
    pub domain: Option<String>,
    /// Optional bootstrap field filter.
    pub field: Option<String>,
    /// Optional knowledge tier filter.
    pub tier: Option<String>,
    /// Optional lifecycle status filter such as `active`, `candidate`,
    /// `promoted`, `canonical`, `demoted`, or `retired`.
    pub status: Option<String>,
    /// Optional anchor kind filter (`global`, `repo`, `worktree`).
    pub anchor_kind: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct SearchRequest {
    /// Natural-language query. Use the user's actual question verbatim
    /// when possible — the embedding model handles paraphrase and translation.
    pub query: String,

    /// Optional wing filter. OMIT (leave null) unless you already know the
    /// EXACT wing name from a prior mempal_status call or the user named it
    /// explicitly. Wing filtering is a strict equality match, so guessing a
    /// wing name (e.g. "engineering", "backend") will silently return zero
    /// results. When in doubt, leave this field unset for a global search
    /// across all wings.
    pub wing: Option<String>,

    /// Optional room filter within a wing. Same rule as wing: OMIT unless you
    /// have seen the exact room name in a prior mempal_status call. Guessing
    /// returns zero results.
    pub room: Option<String>,

    /// Maximum number of results to return. Defaults to 10 when omitted.
    pub top_k: Option<usize>,

    /// Unified retrieval scope. Prefer this object for new callers. Legacy
    /// top-level scope fields remain accepted for compatibility and must not
    /// conflict with fields inside this object.
    pub scope: Option<RetrievalScopeRequest>,

    /// Legacy alias for `scope.project_id`.
    pub project_id: Option<String>,

    /// Legacy alias for `scope.include_global`.
    pub include_global: Option<bool>,

    /// Legacy alias for `scope.all_projects`.
    pub all_projects: Option<bool>,

    /// Return full verbatim content for this call even when progressive
    /// disclosure is enabled globally.
    pub disable_progressive: Option<bool>,

    /// Legacy alias for `scope.memory_kind`.
    pub memory_kind: Option<String>,

    /// Legacy alias for `scope.domain`.
    pub domain: Option<String>,

    /// Legacy alias for `scope.field`.
    pub field: Option<String>,

    /// Legacy alias for `scope.tier`.
    pub tier: Option<String>,

    /// Legacy alias for `scope.status`.
    pub status: Option<String>,

    /// Legacy alias for `scope.anchor_kind`.
    pub anchor_kind: Option<String>,

    /// If true and top_k <= 10, include previous/next chunks from the same source.
    pub with_neighbors: Option<bool>,

    /// Include low-importance raw dialogue turns that are excluded by default.
    pub include_raw_turns: Option<bool>,

    /// Include drawers outside their validity window. Defaults to false.
    pub include_expired: Option<bool>,

    /// Request a citation-preserving evidence pack alongside normal results.
    /// Quality gating requires both the `adk-rust` Cargo feature and
    /// `[evidence_workflow].enabled = true`; otherwise the pack reports the
    /// unchanged bounded cited hits and an explicit fallback reason.
    pub evidence: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchResponse {
    pub results: Vec<SearchResultDto>,
    /// Retrieval mode used for this response: "hybrid" for vector+BM25 or
    /// "bm25_only" when the embedder is degraded/unavailable and fallback is enabled.
    pub search_mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
    /// Optional ADK-Rust quality-gated evidence projection. Normal search
    /// results remain unchanged for compatibility.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<crate::evidence_workflow::EvidencePack>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ContextRequest {
    pub query: String,
    /// Unified retrieval scope. `mempal_context` supports `project_id`,
    /// `all_projects`, `domain`, and `field`; search-only fields such as
    /// `session`, `memory_kind`, and `status` are rejected instead of ignored.
    pub scope: Option<RetrievalScopeRequest>,
    pub field: Option<String>,
    pub domain: Option<String>,
    pub project_id: Option<String>,
    pub all_projects: Option<bool>,
    pub cwd: Option<String>,
    pub include_evidence: Option<bool>,
    pub include_cards: Option<bool>,
    pub max_items: Option<usize>,
    /// Maximum number of `dao_tian` items to include. Defaults to 1; 0 disables
    /// the `dao_tian` section while preserving lower-tier context.
    pub dao_tian_limit: Option<usize>,
    /// Trigger hint for tiered retrieval budget weights (P14).
    /// One of: "session_start" (default), "on_demand", "repair".
    pub trigger: Option<String>,
    /// P106: include the read-only `distill_suggestions` signal. Defaults to
    /// true; set false to omit it. Never changes the assembled sections.
    pub include_distill_suggestions: Option<bool>,
}

/// A single item from a tiered retrieval result (P14).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TieredContextItemDto {
    pub drawer_id: String,
    pub content: String,
    pub source_file: String,
    /// Drawer room — maps to "type" in the spec (e.g. "decision", "feedback", "rule").
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub drawer_type: Option<String>,
    /// T3 provenance: "recency", "kg", or "tunnel".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub effective_importance: f64,
    /// Pattern ID boosting this result (P13). None when no pattern matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_pattern_id: Option<String>,
}

/// Token budget usage breakdown (P14).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BudgetUsedDto {
    pub t1_tokens: usize,
    pub t2_tokens: usize,
    pub t3_tokens: usize,
    pub foresight_tokens: usize,
    pub total_tokens: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ContextResponse {
    pub query: String,
    pub domain: String,
    pub field: String,
    pub anchors: Vec<ContextAnchorDto>,
    pub sections: Vec<ContextSectionDto>,
    /// Active patterns surfaced as recurring themes (P13).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recurring_themes: Vec<PatternSummaryDto>,
    /// T1 tier (dao_tian): decision/feedback/rule drawers (P14).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t1_dao_tian: Option<Vec<TieredContextItemDto>>,
    /// T2 tier (shu): hybrid search results (P14).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t2_shu: Option<Vec<TieredContextItemDto>>,
    /// T3 tier (qi): recent drawers by recency + KG neighbors (P14).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t3_qi: Option<Vec<TieredContextItemDto>>,
    /// Alias for t1_dao_tian — backward compat with existing agents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dao_tian: Option<Vec<TieredContextItemDto>>,
    /// Alias for t2_shu — backward compat with existing agents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shu: Option<Vec<TieredContextItemDto>>,
    /// Alias for t3_qi — backward compat with existing agents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qi: Option<Vec<TieredContextItemDto>>,
    /// Token budget usage (P14). Present only when tiered_retrieval_enabled=true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_used: Option<BudgetUsedDto>,
    /// Active repair warnings (P14 decision-repair). Non-empty when anti-patterns detected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repair_warnings: Vec<crate::repair::RepairWarning>,
    /// Active skills injected at T1 head priority (P15). Agent should consult these
    /// trigger_descriptions before deciding which skills to invoke.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_skills: Vec<SkillSummaryDto>,
    /// P106: read-only signal flagging fields with dense evidence but no
    /// promoted knowledge yet. Empty when disabled or nothing qualifies.
    pub distill_suggestions: Vec<DistillSuggestionDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DistillSuggestionDto {
    pub field: String,
    pub evidence_count: usize,
    pub sample_evidence_drawer_ids: Vec<String>,
    pub suggested_tier: String,
}

impl From<DistillSuggestion> for DistillSuggestionDto {
    fn from(value: DistillSuggestion) -> Self {
        Self {
            field: value.field,
            evidence_count: value.evidence_count,
            sample_evidence_drawer_ids: value.sample_evidence_drawer_ids,
            suggested_tier: value.suggested_tier,
        }
    }
}

/// Lightweight skill summary for context responses and list actions (P15).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SkillSummaryDto {
    pub skill_id: String,
    pub name: String,
    pub trigger_description: String,
    /// Laplace-smoothed adoption rate (computed at query time).
    pub eta: f64,
    pub status: String,
    pub adoption_count: i64,
    pub rejection_count: i64,
}

/// Full skill detail for the `show` action.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SkillDto {
    pub skill_id: String,
    pub name: String,
    pub trigger_description: String,
    pub pattern_id: String,
    pub exemplar_ids: Vec<String>,
    pub adoption_count: i64,
    pub rejection_count: i64,
    /// Laplace-smoothed adoption rate (computed at query time).
    pub eta: f64,
    pub status: String,
    pub promoted_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

/// Request for the `mempal_skill` MCP tool (P15).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SkillRequest {
    /// Action to perform: list | show | promote | adopt | reject | retire
    pub action: String,
    /// Skill ID (required for show, adopt, reject, retire).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_id: Option<String>,
    /// Pattern ID (required for promote).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern_id: Option<String>,
    /// Human-readable name for the skill (required for promote).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// When to invoke this skill — provided by agent, NOT generated (required for promote).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_description: Option<String>,
    /// Filter by status for list: probationary | active | retired. Omit for all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Optional project scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

/// Response from the `mempal_skill` MCP tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SkillResponse {
    pub action: String,
    /// New or updated status after the action (adopt/reject/retire).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Full skill detail (show action or promote).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<SkillDto>,
    /// List of skills (list action).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub skills: Vec<SkillSummaryDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Lightweight pattern summary for context responses.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PatternSummaryDto {
    pub pattern_id: String,
    pub topic_tags: Vec<String>,
    pub session_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exemplar_preview: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct KnowledgeGateRequest {
    pub drawer_id: String,
    pub target_status: Option<String>,
    pub reviewer: Option<String>,
    pub allow_counterexamples: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct KnowledgeDistillRequest {
    pub statement: String,
    pub content: String,
    pub tier: String,
    pub supporting_refs: Vec<String>,
    pub counterexample_refs: Option<Vec<String>>,
    pub teaching_refs: Option<Vec<String>>,
    pub domain: Option<String>,
    pub field: Option<String>,
    pub wing: Option<String>,
    pub room: Option<String>,
    pub scope_constraints: Option<String>,
    pub trigger_hints: Option<TriggerHintsDto>,
    pub cwd: Option<String>,
    pub importance: Option<i32>,
    pub dry_run: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KnowledgeDistillResponse {
    pub drawer_id: String,
    pub created: bool,
    pub dry_run: bool,
}

impl From<DistillOutcome> for KnowledgeDistillResponse {
    fn from(outcome: DistillOutcome) -> Self {
        Self {
            drawer_id: outcome.drawer_id,
            created: outcome.created,
            dry_run: outcome.dry_run,
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct KnowledgePromoteRequest {
    pub drawer_id: String,
    pub status: String,
    pub verification_refs: Vec<String>,
    pub reason: String,
    pub reviewer: Option<String>,
    pub allow_counterexamples: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KnowledgePromoteResponse {
    pub drawer_id: String,
    pub old_status: String,
    pub new_status: String,
    pub verification_refs: Vec<String>,
    pub gate: Option<KnowledgeGateResponse>,
}

impl From<PromoteOutcome> for KnowledgePromoteResponse {
    fn from(outcome: PromoteOutcome) -> Self {
        Self {
            drawer_id: outcome.drawer_id,
            old_status: outcome.old_status,
            new_status: outcome.new_status,
            verification_refs: outcome.verification_refs,
            gate: outcome.gate.map(KnowledgeGateResponse::from),
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct KnowledgeDemoteRequest {
    pub drawer_id: String,
    pub status: String,
    pub evidence_refs: Vec<String>,
    pub reason: String,
    pub reason_type: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KnowledgeDemoteResponse {
    pub drawer_id: String,
    pub old_status: String,
    pub new_status: String,
    pub counterexample_refs: Vec<String>,
}

impl From<DemoteOutcome> for KnowledgeDemoteResponse {
    fn from(outcome: DemoteOutcome) -> Self {
        Self {
            drawer_id: outcome.drawer_id,
            old_status: outcome.old_status,
            new_status: outcome.new_status,
            counterexample_refs: outcome.counterexample_refs,
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct KnowledgePublishAnchorRequest {
    pub drawer_id: String,
    pub to: String,
    pub target_anchor_id: Option<String>,
    pub cwd: Option<String>,
    pub reason: String,
    pub reviewer: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KnowledgePublishAnchorResponse {
    pub drawer_id: String,
    pub old_anchor_kind: String,
    pub old_anchor_id: String,
    pub old_parent_anchor_id: Option<String>,
    pub new_anchor_kind: String,
    pub new_anchor_id: String,
    pub new_parent_anchor_id: Option<String>,
}

impl From<PublishAnchorOutcome> for KnowledgePublishAnchorResponse {
    fn from(outcome: PublishAnchorOutcome) -> Self {
        Self {
            drawer_id: outcome.drawer_id,
            old_anchor_kind: outcome.old_anchor_kind,
            old_anchor_id: outcome.old_anchor_id,
            old_parent_anchor_id: outcome.old_parent_anchor_id,
            new_anchor_kind: outcome.new_anchor_kind,
            new_anchor_id: outcome.new_anchor_id,
            new_parent_anchor_id: outcome.new_parent_anchor_id,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KnowledgeGateResponse {
    pub drawer_id: String,
    pub tier: String,
    pub status: String,
    pub target_status: String,
    pub allowed: bool,
    pub reasons: Vec<String>,
    pub requirements: KnowledgeGateRequirementsDto,
    pub evidence_counts: KnowledgeGateEvidenceCountsDto,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KnowledgeGateRequirementsDto {
    pub min_supporting_refs: usize,
    pub min_verification_refs: usize,
    pub min_teaching_refs: usize,
    pub reviewer_required: bool,
    pub counterexamples_block: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KnowledgeGateEvidenceCountsDto {
    pub supporting: usize,
    pub counterexample: usize,
    pub teaching: usize,
    pub verification: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KnowledgePolicyResponse {
    pub entries: Vec<KnowledgePolicyEntryDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KnowledgePolicyEntryDto {
    pub tier: String,
    pub target_status: String,
    pub requirements: KnowledgeGateRequirementsDto,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct KnowledgeCardsRequest {
    pub action: String,
    pub query: Option<String>,
    pub card_id: Option<String>,
    pub target_status: Option<String>,
    pub reviewer: Option<String>,
    pub allow_counterexamples: Option<bool>,
    pub verification_refs: Option<Vec<String>>,
    pub evidence_refs: Option<Vec<String>>,
    pub reason: Option<String>,
    pub reason_type: Option<String>,
    pub enforce_gate: Option<bool>,
    pub tier: Option<String>,
    pub status: Option<String>,
    pub domain: Option<String>,
    pub field: Option<String>,
    pub anchor_kind: Option<String>,
    pub anchor_id: Option<String>,
    /// Filter list results by whether the card was generated by crystallization.
    pub auto_generated: Option<bool>,
    pub pending_review: Option<bool>,
    pub cwd: Option<String>,
    pub top_k: Option<usize>,
    pub evidence_top_k: Option<usize>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KnowledgeCardsResponse {
    pub cards: Vec<KnowledgeCardDto>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub retrieved: Vec<RetrievedKnowledgeCardDto>,
    pub events: Vec<KnowledgeCardEventDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate: Option<KnowledgeCardGateDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promote: Option<KnowledgeCardPromoteDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub demote: Option<KnowledgeCardDemoteDto>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct Phase3Request {
    pub action: String,
    pub id: Option<String>,
    pub surface: Option<String>,
    pub outcome: Option<String>,
    pub subject_kind: Option<String>,
    pub subject_id: Option<String>,
    pub proposed_action: Option<String>,
    pub evidence_refs: Option<Vec<String>>,
    pub counterexample_refs: Option<Vec<String>>,
    pub risk_notes: Option<Vec<String>>,
    pub rollback_criteria: Option<Vec<String>>,
    pub track: Option<String>,
    pub signal: Option<String>,
    pub feature: Option<String>,
    pub query: Option<String>,
    pub context_hash: Option<String>,
    pub card_id: Option<String>,
    pub evaluator_id: Option<String>,
    pub research_report_id: Option<String>,
    pub note: Option<String>,
    #[schemars(schema_with = "json_object_schema")]
    pub metadata: Option<serde_json::Value>,
    pub limit: Option<usize>,
    pub candidate: Option<String>,
    #[schemars(schema_with = "json_object_schema")]
    pub report: Option<serde_json::Value>,
    pub execute: Option<bool>,
    pub allow_warnings: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Phase3Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guidance: Option<RuntimeAdoptionGuidanceDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instrumentation_policy: Option<RuntimeAdoptionInstrumentationPolicyDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_plan: Option<RuntimeAdoptionRecordPlanDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_quality: Option<RuntimeAdoptionRecordQualityDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_checked: Option<RuntimeAdoptionCheckedRecordDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_report: Option<RuntimeAdoptionReviewReportDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readiness_report: Option<Phase3ReadinessReportDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<RuntimeAdoptionEventDto>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub events: Vec<RuntimeAdoptionEventDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<RuntimeAdoptionStatsDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analytics: Option<RuntimeAdoptionAnalyticsDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate: Option<Phase3GateDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub research_plan: Option<ResearchAdapterPlanDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub research_ingest_plan: Option<ResearchIngestPlanDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evaluator_advice: Option<EvaluatorAdviceDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_proposal: Option<CardContextDefaultProposalDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback_control: Option<CardContextRollbackControlDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeAdoptionGuidanceDto {
    pub version: u32,
    pub recording_rule: String,
    pub required_fields: Vec<String>,
    pub optional_fields: Vec<String>,
    pub signals: Vec<RuntimeAdoptionSignalGuidanceDto>,
    pub tracks: Vec<RuntimeAdoptionTrackGuidanceDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeAdoptionSignalGuidanceDto {
    pub signal: String,
    pub when: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeAdoptionTrackGuidanceDto {
    pub track: String,
    pub when: String,
    pub feature_examples: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeAdoptionInstrumentationPolicyDto {
    pub version: u32,
    pub writes: bool,
    pub default_mode: String,
    pub allowed_modes: Vec<RuntimeAdoptionInstrumentationModeDto>,
    pub forbidden_modes: Vec<String>,
    pub requirements: Vec<String>,
    pub rollback_requirements: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeAdoptionInstrumentationModeDto {
    pub mode: String,
    pub description: String,
    pub requires_execute: bool,
    pub requires_checked_capture: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeAdoptionRecordPlanDto {
    pub writes: bool,
    pub record_command: Vec<String>,
    pub record_payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeAdoptionRecordQualityDto {
    pub writes: bool,
    pub valid: bool,
    pub quality: String,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeAdoptionCheckedRecordDto {
    pub writes: bool,
    pub blocked: bool,
    pub record_quality: RuntimeAdoptionRecordQualityDto,
    pub event: Option<RuntimeAdoptionEventDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeAdoptionReviewReportDto {
    pub writes: bool,
    pub filters: RuntimeAdoptionReviewFiltersDto,
    pub total: usize,
    pub stats: RuntimeAdoptionSignalCountsDto,
    pub features: Vec<RuntimeAdoptionFeatureReviewDto>,
    pub conclusion: String,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeAdoptionReviewFiltersDto {
    pub track: Option<String>,
    pub feature: Option<String>,
    pub signal: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeAdoptionSignalCountsDto {
    pub total: usize,
    pub used: usize,
    pub accepted: usize,
    pub rejected: usize,
    pub misses: usize,
    pub rollbacks: usize,
    pub contradictions: usize,
    pub neutral: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeAdoptionFeatureReviewDto {
    pub feature: String,
    pub stats: RuntimeAdoptionSignalCountsDto,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Phase3ReadinessReportDto {
    pub writes: bool,
    pub candidate: String,
    pub ready: bool,
    pub decision: String,
    pub required_track: String,
    pub required_feature: String,
    pub review: RuntimeAdoptionReviewReportDto,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EvaluatorAdviceDto {
    pub writes: bool,
    pub evaluator_id: String,
    pub subject_kind: String,
    pub subject_id: String,
    pub proposed_action: String,
    pub recommendation: String,
    pub lifecycle_authority: bool,
    pub deterministic_gate_required: bool,
    pub requires_human_review: bool,
    pub evidence_refs: Vec<String>,
    pub counterexample_refs: Vec<String>,
    pub risk_notes: Vec<String>,
    pub reasons: Vec<String>,
    pub adoption_capture: RuntimeAdoptionRecordPlanDto,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CardContextDefaultProposalDto {
    pub writes: bool,
    pub candidate: String,
    pub proposal_ready: bool,
    pub decision: String,
    pub readiness: Phase3ReadinessReportDto,
    pub rollback_criteria: Vec<String>,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CardContextRollbackControlDto {
    pub writes: bool,
    pub candidate: String,
    pub execute: bool,
    pub rollback_required: bool,
    pub applied: bool,
    pub include_cards_default_before: bool,
    pub include_cards_default_after: bool,
    pub review: RuntimeAdoptionReviewReportDto,
    pub reasons: Vec<String>,
}

impl From<crate::core::phase3::CardContextDefaultProposalReport> for CardContextDefaultProposalDto {
    fn from(report: crate::core::phase3::CardContextDefaultProposalReport) -> Self {
        Self {
            writes: report.writes,
            candidate: report.candidate,
            proposal_ready: report.proposal_ready,
            decision: report.decision,
            readiness: report.readiness.into(),
            rollback_criteria: report.rollback_criteria,
            reasons: report.reasons,
        }
    }
}

impl From<crate::core::phase3::CardContextRollbackControlReport> for CardContextRollbackControlDto {
    fn from(report: crate::core::phase3::CardContextRollbackControlReport) -> Self {
        Self {
            writes: report.writes,
            candidate: report.candidate,
            execute: report.execute,
            rollback_required: report.rollback_required,
            applied: report.applied,
            include_cards_default_before: report.include_cards_default_before,
            include_cards_default_after: report.include_cards_default_after,
            review: report.review.into(),
            reasons: report.reasons,
        }
    }
}

impl From<crate::core::phase3::EvaluatorAdviceReport> for EvaluatorAdviceDto {
    fn from(report: crate::core::phase3::EvaluatorAdviceReport) -> Self {
        Self {
            writes: report.writes,
            evaluator_id: report.evaluator_id,
            subject_kind: report.subject_kind,
            subject_id: report.subject_id,
            proposed_action: report.proposed_action,
            recommendation: report.recommendation,
            lifecycle_authority: report.lifecycle_authority,
            deterministic_gate_required: report.deterministic_gate_required,
            requires_human_review: report.requires_human_review,
            evidence_refs: report.evidence_refs,
            counterexample_refs: report.counterexample_refs,
            risk_notes: report.risk_notes,
            reasons: report.reasons,
            adoption_capture: report.adoption_capture.into(),
        }
    }
}

impl From<RuntimeAdoptionRecordPlan> for RuntimeAdoptionRecordPlanDto {
    fn from(plan: RuntimeAdoptionRecordPlan) -> Self {
        Self {
            writes: plan.writes,
            record_command: plan.record_command,
            record_payload: plan.record_payload,
        }
    }
}

impl From<RuntimeAdoptionRecordQualityReport> for RuntimeAdoptionRecordQualityDto {
    fn from(report: RuntimeAdoptionRecordQualityReport) -> Self {
        Self {
            writes: report.writes,
            valid: report.valid,
            quality: report.quality,
            errors: report.errors,
            warnings: report.warnings,
        }
    }
}

impl From<RuntimeAdoptionCheckedRecordReport> for RuntimeAdoptionCheckedRecordDto {
    fn from(report: RuntimeAdoptionCheckedRecordReport) -> Self {
        Self {
            writes: report.writes,
            blocked: report.blocked,
            record_quality: report.record_quality.into(),
            event: report.event.map(RuntimeAdoptionEventDto::from),
        }
    }
}

impl From<RuntimeAdoptionReviewReport> for RuntimeAdoptionReviewReportDto {
    fn from(report: RuntimeAdoptionReviewReport) -> Self {
        Self {
            writes: report.writes,
            filters: report.filters.into(),
            total: report.total,
            stats: report.stats.into(),
            features: report
                .features
                .into_iter()
                .map(|feature| RuntimeAdoptionFeatureReviewDto {
                    feature: feature.feature,
                    stats: feature.stats.into(),
                })
                .collect(),
            conclusion: report.conclusion,
            reasons: report.reasons,
        }
    }
}

impl From<Phase3ReadinessReport> for Phase3ReadinessReportDto {
    fn from(report: Phase3ReadinessReport) -> Self {
        Self {
            writes: report.writes,
            candidate: report.candidate,
            ready: report.ready,
            decision: report.decision,
            required_track: report.required_track,
            required_feature: report.required_feature,
            review: report.review.into(),
            reasons: report.reasons,
        }
    }
}

impl From<RuntimeAdoptionReviewFilters> for RuntimeAdoptionReviewFiltersDto {
    fn from(filters: RuntimeAdoptionReviewFilters) -> Self {
        Self {
            track: filters.track,
            feature: filters.feature,
            signal: filters.signal,
            limit: filters.limit,
        }
    }
}

impl From<RuntimeAdoptionSignalCounts> for RuntimeAdoptionSignalCountsDto {
    fn from(stats: RuntimeAdoptionSignalCounts) -> Self {
        Self {
            total: stats.total,
            used: stats.used,
            accepted: stats.accepted,
            rejected: stats.rejected,
            misses: stats.misses,
            rollbacks: stats.rollbacks,
            contradictions: stats.contradictions,
            neutral: stats.neutral,
        }
    }
}

impl From<RuntimeAdoptionGuidance> for RuntimeAdoptionGuidanceDto {
    fn from(guidance: RuntimeAdoptionGuidance) -> Self {
        Self {
            version: guidance.version,
            recording_rule: guidance.recording_rule,
            required_fields: guidance.required_fields,
            optional_fields: guidance.optional_fields,
            signals: guidance.signals.into_iter().map(Into::into).collect(),
            tracks: guidance.tracks.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<RuntimeAdoptionSignalGuidance> for RuntimeAdoptionSignalGuidanceDto {
    fn from(guidance: RuntimeAdoptionSignalGuidance) -> Self {
        Self {
            signal: guidance.signal,
            when: guidance.when,
        }
    }
}

impl From<RuntimeAdoptionTrackGuidance> for RuntimeAdoptionTrackGuidanceDto {
    fn from(guidance: RuntimeAdoptionTrackGuidance) -> Self {
        Self {
            track: guidance.track,
            when: guidance.when,
            feature_examples: guidance.feature_examples,
        }
    }
}

impl From<RuntimeAdoptionInstrumentationPolicy> for RuntimeAdoptionInstrumentationPolicyDto {
    fn from(policy: RuntimeAdoptionInstrumentationPolicy) -> Self {
        Self {
            version: policy.version,
            writes: policy.writes,
            default_mode: policy.default_mode,
            allowed_modes: policy.allowed_modes.into_iter().map(Into::into).collect(),
            forbidden_modes: policy.forbidden_modes,
            requirements: policy.requirements,
            rollback_requirements: policy.rollback_requirements,
        }
    }
}

impl From<RuntimeAdoptionInstrumentationMode> for RuntimeAdoptionInstrumentationModeDto {
    fn from(mode: RuntimeAdoptionInstrumentationMode) -> Self {
        Self {
            mode: mode.mode,
            description: mode.description,
            requires_execute: mode.requires_execute,
            requires_checked_capture: mode.requires_checked_capture,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeAdoptionEventDto {
    pub id: String,
    pub track: String,
    pub signal: String,
    pub feature: String,
    pub query: Option<String>,
    pub context_hash: Option<String>,
    pub card_id: Option<String>,
    pub evaluator_id: Option<String>,
    pub research_report_id: Option<String>,
    pub note: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeAdoptionStatsDto {
    pub total: usize,
    pub used: usize,
    pub accepted: usize,
    pub rejected: usize,
    pub misses: usize,
    pub rollbacks: usize,
    pub contradictions: usize,
    pub neutral: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeAdoptionAnalyticsDto {
    pub writes: bool,
    pub total_events: usize,
    pub groups: Vec<RuntimeAdoptionAnalyticsGroupDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeAdoptionAnalyticsGroupDto {
    pub track: String,
    pub feature: String,
    pub total: usize,
    pub used: usize,
    pub accepted: usize,
    pub rejected: usize,
    pub misses: usize,
    pub rollbacks: usize,
    pub contradictions: usize,
    pub neutral: usize,
    pub recommendation: String,
}

impl From<RuntimeAdoptionAnalyticsReport> for RuntimeAdoptionAnalyticsDto {
    fn from(report: RuntimeAdoptionAnalyticsReport) -> Self {
        Self {
            writes: report.writes,
            total_events: report.total_events,
            groups: report.groups.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<RuntimeAdoptionAnalyticsGroup> for RuntimeAdoptionAnalyticsGroupDto {
    fn from(group: RuntimeAdoptionAnalyticsGroup) -> Self {
        Self {
            track: group.track,
            feature: group.feature,
            total: group.total,
            used: group.used,
            accepted: group.accepted,
            rejected: group.rejected,
            misses: group.misses,
            rollbacks: group.rollbacks,
            contradictions: group.contradictions,
            neutral: group.neutral,
            recommendation: group.recommendation,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Phase3GateDto {
    pub candidate: String,
    pub ready: bool,
    pub required_track: String,
    pub stats: RuntimeAdoptionStatsDto,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ResearchAdapterPlanDto {
    pub valid: bool,
    pub report_id: String,
    pub title: String,
    pub source_count: usize,
    pub finding_count: usize,
    pub candidate_insight_count: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ResearchIngestPlanDto {
    pub valid: bool,
    pub writes: bool,
    pub report_id: String,
    pub title: String,
    pub source_count: usize,
    pub finding_count: usize,
    pub candidate_insight_count: usize,
    pub planned_evidence_count: usize,
    pub created_count: usize,
    pub skipped_count: usize,
    pub errors: Vec<String>,
    pub evidence_drawers: Vec<ResearchEvidenceDrawerPlanDto>,
    pub candidate_insights: Vec<ResearchCandidateInsightPlanDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ResearchEvidenceDrawerPlanDto {
    pub drawer_id: String,
    pub finding_index: usize,
    pub source_file: String,
    pub created: bool,
    pub skipped: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ResearchCandidateInsightPlanDto {
    pub statement: String,
    pub supporting_refs: Vec<String>,
    pub suggested_command: Vec<String>,
}

impl From<ResearchIngestPlanReport> for ResearchIngestPlanDto {
    fn from(report: ResearchIngestPlanReport) -> Self {
        Self {
            valid: report.valid,
            writes: report.writes,
            report_id: report.report_id,
            title: report.title,
            source_count: report.source_count,
            finding_count: report.finding_count,
            candidate_insight_count: report.candidate_insight_count,
            planned_evidence_count: report.planned_evidence_count,
            created_count: report.created_count,
            skipped_count: report.skipped_count,
            errors: report.errors,
            evidence_drawers: report
                .evidence_drawers
                .into_iter()
                .map(ResearchEvidenceDrawerPlanDto::from)
                .collect(),
            candidate_insights: report
                .candidate_insights
                .into_iter()
                .map(ResearchCandidateInsightPlanDto::from)
                .collect(),
        }
    }
}

impl From<ResearchEvidenceDrawerPlan> for ResearchEvidenceDrawerPlanDto {
    fn from(plan: ResearchEvidenceDrawerPlan) -> Self {
        Self {
            drawer_id: plan.drawer_id,
            finding_index: plan.finding_index,
            source_file: plan.source_file,
            created: plan.created,
            skipped: plan.skipped,
        }
    }
}

impl From<ResearchCandidateInsightPlan> for ResearchCandidateInsightPlanDto {
    fn from(plan: ResearchCandidateInsightPlan) -> Self {
        Self {
            statement: plan.statement,
            supporting_refs: plan.supporting_refs,
            suggested_command: plan.suggested_command,
        }
    }
}

impl From<RuntimeAdoptionEvent> for RuntimeAdoptionEventDto {
    fn from(event: RuntimeAdoptionEvent) -> Self {
        Self {
            id: event.id,
            track: runtime_adoption_track_slug(&event.track).to_string(),
            signal: runtime_adoption_signal_slug(&event.signal).to_string(),
            feature: event.feature,
            query: event.query,
            context_hash: event.context_hash,
            card_id: event.card_id,
            evaluator_id: event.evaluator_id,
            research_report_id: event.research_report_id,
            note: event.note,
            metadata: event.metadata,
            created_at: event.created_at,
        }
    }
}

fn runtime_adoption_track_slug(track: &RuntimeAdoptionTrack) -> &'static str {
    match track {
        RuntimeAdoptionTrack::RuntimeAdoption => "runtime_adoption",
        RuntimeAdoptionTrack::CardContext => "card_context",
        RuntimeAdoptionTrack::CardEmbedding => "card_embedding",
        RuntimeAdoptionTrack::Evaluator => "evaluator",
        RuntimeAdoptionTrack::ResearchAdapter => "research_adapter",
    }
}

fn runtime_adoption_signal_slug(signal: &RuntimeAdoptionSignal) -> &'static str {
    match signal {
        RuntimeAdoptionSignal::Used => "used",
        RuntimeAdoptionSignal::Accepted => "accepted",
        RuntimeAdoptionSignal::Rejected => "rejected",
        RuntimeAdoptionSignal::Miss => "miss",
        RuntimeAdoptionSignal::Rollback => "rollback",
        RuntimeAdoptionSignal::Contradiction => "contradiction",
        RuntimeAdoptionSignal::Neutral => "neutral",
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KnowledgeCardDto {
    pub id: String,
    pub statement: String,
    pub content: String,
    pub tier: String,
    pub status: String,
    pub domain: String,
    pub field: String,
    pub anchor_kind: String,
    pub anchor_id: String,
    pub parent_anchor_id: Option<String>,
    pub scope_constraints: Option<String>,
    pub trigger_hints: Option<TriggerHintsDto>,
    pub auto_generated: bool,
    pub crystallization_score: Option<f64>,
    pub source_drawer_ids: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RetrievedKnowledgeCardDto {
    pub card: KnowledgeCardDto,
    pub evidence_citations: Vec<RetrievedEvidenceCitationDto>,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RetrievedEvidenceCitationDto {
    pub evidence_drawer_id: String,
    pub role: String,
    pub source_file: String,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KnowledgeCardEventDto {
    pub id: String,
    pub card_id: String,
    pub event_type: String,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    pub reason: String,
    pub actor: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KnowledgeCardGateDto {
    pub card_id: String,
    pub tier: String,
    pub status: String,
    pub target_status: String,
    pub allowed: bool,
    pub reasons: Vec<String>,
    pub requirements: KnowledgeGateRequirementsDto,
    pub evidence_counts: KnowledgeGateEvidenceCountsDto,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KnowledgeCardPromoteDto {
    pub card_id: String,
    pub old_status: String,
    pub new_status: String,
    pub verification_refs: Vec<String>,
    pub gate: Option<KnowledgeCardGateDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KnowledgeCardDemoteDto {
    pub card_id: String,
    pub old_status: String,
    pub new_status: String,
    pub counterexample_refs: Vec<String>,
}

impl From<Vec<PromotionPolicyEntry>> for KnowledgePolicyResponse {
    fn from(entries: Vec<PromotionPolicyEntry>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|entry| KnowledgePolicyEntryDto {
                    tier: entry.tier,
                    target_status: entry.target_status,
                    requirements: KnowledgeGateRequirementsDto {
                        min_supporting_refs: entry.requirements.min_supporting_refs,
                        min_verification_refs: entry.requirements.min_verification_refs,
                        min_teaching_refs: entry.requirements.min_teaching_refs,
                        reviewer_required: entry.requirements.reviewer_required,
                        counterexamples_block: entry.requirements.counterexamples_block,
                    },
                })
                .collect(),
        }
    }
}

impl From<GateReport> for KnowledgeGateResponse {
    fn from(report: GateReport) -> Self {
        Self {
            drawer_id: report.drawer_id,
            tier: report.tier,
            status: report.status,
            target_status: report.target_status,
            allowed: report.allowed,
            reasons: report.reasons,
            requirements: KnowledgeGateRequirementsDto {
                min_supporting_refs: report.requirements.min_supporting_refs,
                min_verification_refs: report.requirements.min_verification_refs,
                min_teaching_refs: report.requirements.min_teaching_refs,
                reviewer_required: report.requirements.reviewer_required,
                counterexamples_block: report.requirements.counterexamples_block,
            },
            evidence_counts: KnowledgeGateEvidenceCountsDto {
                supporting: report.evidence_counts.supporting,
                counterexample: report.evidence_counts.counterexample,
                teaching: report.evidence_counts.teaching,
                verification: report.evidence_counts.verification,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ContextAnchorDto {
    pub anchor_kind: String,
    pub anchor_id: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ContextSectionDto {
    pub name: String,
    pub items: Vec<ContextItemDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ContextItemDto {
    pub drawer_id: String,
    pub source_file: String,
    pub memory_kind: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub anchor_kind: String,
    pub anchor_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_anchor_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_hints: Option<TriggerHintsDto>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub evidence_citations: Vec<ContextEvidenceCitationDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ContextEvidenceCitationDto {
    pub evidence_drawer_id: String,
    pub role: String,
    pub source_file: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchResultDto {
    pub drawer_id: String,
    pub content: String,
    pub content_truncated: bool,
    pub original_content_bytes: u64,
    pub wing: String,
    pub room: Option<String>,
    pub source_file: String,
    pub source: String,
    pub source_type: String,
    pub confidence: f64,
    pub similarity: f32,
    pub route: RouteDecisionDto,
    /// Other wings sharing this room (tunnel cross-references).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tunnel_hints: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub neighbors: Option<ChunkNeighborsDto>,
    /// 3-4 letter entity codes derived from AAAK analysis.
    pub entities: Vec<String>,
    /// Topic keywords derived from AAAK analysis. May be empty.
    pub topics: Vec<String>,
    /// Classification flags derived from AAAK analysis. Always non-empty.
    pub flags: Vec<String>,
    /// Emotion tags derived from AAAK analysis. Always non-empty.
    pub emotions: Vec<String>,
    /// Importance derived from AAAK flags, normalized to the existing 2-4 scale.
    pub importance_stars: u8,
    /// Dynamic importance after time-decay + retrieval boost (P13).
    pub effective_importance: f64,
    pub memory_kind: String,
    pub domain: String,
    pub field: String,
    pub is_pinned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub anchor_kind: String,
    pub anchor_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_anchor_id: Option<String>,
    /// Pattern ID of the active pattern boosting this result (P13). None when no pattern matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_pattern_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ChunkNeighborsDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev: Option<NeighborChunkDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<NeighborChunkDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NeighborChunkDto {
    pub drawer_id: String,
    pub content: String,
    pub chunk_index: u32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RouteDecisionDto {
    pub wing: Option<String>,
    pub room: Option<String>,
    pub confidence: f32,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReadDrawerRequest {
    pub drawer_id: String,

    /// Optional explicit project scope. When omitted, mempal resolves the
    /// current project from `[project]` config or MCP roots and scopes the
    /// read there by default. Set `all_projects=true` to bypass project
    /// scoping.
    pub project_id: Option<String>,

    /// Include legacy/global drawers (`project_id IS NULL`) alongside the
    /// current project. Ignored when `all_projects=true`.
    pub include_global: Option<bool>,

    /// Opt-in override to read across all projects for this request.
    pub all_projects: Option<bool>,
}

/// Hard cap for `mempal_read_drawers.drawer_ids` to prevent unbounded request allocation.
pub const MAX_READ_DRAWERS_REQUEST_IDS: usize = 10_000;

/// Hard cap for `mempal_read_drawers.max_count`; must stay <= `MAX_READ_DRAWERS_REQUEST_IDS`.
pub const MAX_READ_DRAWERS_MAX_COUNT: usize = 2_000;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReadDrawersRequest {
    pub drawer_ids: Vec<String>,

    /// Optional max number of distinct drawer ids to read after de-duplication.
    /// Defaults to 20 when omitted.
    pub max_count: Option<u32>,

    /// Optional explicit project scope. When omitted, mempal resolves the
    /// current project from `[project]` config or MCP roots and scopes the
    /// read there by default. Set `all_projects=true` to bypass project
    /// scoping.
    pub project_id: Option<String>,

    /// Include legacy/global drawers (`project_id IS NULL`) alongside the
    /// current project. Ignored when `all_projects=true`.
    pub include_global: Option<bool>,

    /// Opt-in override to read across all projects for this request.
    pub all_projects: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReadDrawerResponse {
    pub drawer_id: String,
    pub content: String,
    pub content_truncated: bool,
    pub original_content_bytes: u64,
    pub wing: String,
    pub room: Option<String>,
    pub source_file: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    pub merge_count: u32,
    pub importance_stars: u8,
    pub has_vector: bool,
    pub vector_dimension: Option<usize>,
    pub vector_embedder: Option<String>,
    pub vector_model: Option<String>,
    pub vector_embedder_fingerprint: Option<String>,
    pub vector_index_version: Option<String>,
    pub vector_current_embedder_fingerprint: Option<String>,
    pub vector_current_index_version: String,
    pub vector_distance_metric: Option<String>,
    pub vector_stale: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReadDrawersResponse {
    pub drawers: Vec<ReadDrawerResponse>,
    pub not_found: Vec<String>,
    pub warnings: Vec<String>,
}

/// Response for `mempal_projects_list`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProjectsListResponse {
    pub projects: Vec<crate::projects::ProjectSummary>,
}

/// Request for `mempal_projects_resume`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ProjectsResumeRequest {
    /// Project name or fragment; matches a wing or a worktree-path basename.
    pub query: String,
    /// Recent evidence drawers to include. Defaults to 5.
    pub evidence_limit: Option<usize>,
    /// In-flight candidate knowledge drawers to include. Defaults to 5.
    pub candidate_limit: Option<usize>,
}

/// Flat response for `mempal_projects_resume`, with an object root for MCP.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProjectsResumeResponse {
    /// One of `resolved`, `ambiguous`, or `not_found`.
    pub resolution: String,
    pub query: String,
    /// The resume pack when `resolution == "resolved"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pack: Option<crate::projects::ResumePack>,
    /// Candidate projects when `resolution == "ambiguous"`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<crate::projects::ProjectSummary>,
    /// Available wings when `resolution == "not_found"`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub available: Vec<String>,
}

impl From<crate::projects::ResumeResolution> for ProjectsResumeResponse {
    fn from(resolution: crate::projects::ResumeResolution) -> Self {
        use crate::projects::ResumeResolution;

        match resolution {
            ResumeResolution::Resolved(pack) => Self {
                resolution: "resolved".to_string(),
                query: pack.wing.clone(),
                pack: Some(*pack),
                candidates: Vec::new(),
                available: Vec::new(),
            },
            ResumeResolution::Ambiguous { query, candidates } => Self {
                resolution: "ambiguous".to_string(),
                query,
                pack: None,
                candidates,
                available: Vec::new(),
            },
            ResumeResolution::NotFound { query, available } => Self {
                resolution: "not_found".to_string(),
                query,
                pack: None,
                candidates: Vec::new(),
                available,
            },
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct PinnedFactsRequest {
    /// Optional explicit project scope. When omitted, mempal resolves the
    /// current project from MCP roots/config just like search.
    pub project_id: Option<String>,
    /// Maximum number of content characters returned across pinned facts.
    /// Defaults to 4000.
    pub budget_chars: Option<usize>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PinnedFactsResponse {
    pub project_id: Option<String>,
    pub budget_chars: usize,
    pub used_chars: usize,
    /// Prompt-ready canonical context assembled from returned pinned facts.
    /// Generated from SQL rows without building or querying an embedder.
    pub text: String,
    pub facts: Vec<PinnedFactDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PinnedFactDto {
    pub drawer_id: String,
    pub content: String,
    pub wing: String,
    pub room: Option<String>,
    pub source_file: String,
    pub memory_kind: String,
    pub domain: String,
    pub field: String,
    pub status: Option<String>,
    pub importance: i32,
    pub pin_order: Option<i64>,
    pub supersedes: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct IngestRequest {
    pub content: String,
    pub wing: String,
    pub room: Option<String>,
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,
    /// Optional source_type provenance: user_explicit, agent_observation, agent_inference, system_generated.
    #[schemars(schema_with = "ingest_source_type_schema")]
    pub source_type: Option<String>,
    pub confidence: Option<f64>,
    pub project_id: Option<String>,
    /// Drawer ID to replace. The old drawer must be active and in the
    /// same project scope as this ingest.
    pub supersedes: Option<String>,
    /// Exact active content to replace within the same wing/room/project
    /// scope. Must match exactly; ambiguous matches return candidate IDs.
    pub replace_text: Option<String>,
    /// Optional validity start timestamp for the new drawer.
    /// Accepts Unix seconds or RFC3339. Defaults to the drawer's added_at.
    pub valid_from: Option<String>,
    /// Optional validity end timestamp for the new drawer.
    /// Accepts Unix seconds or RFC3339. Null means still valid.
    pub valid_until: Option<String>,

    /// If true, return the drawer_id that WOULD be created without actually
    /// writing to the database. Use this to preview before committing.
    pub dry_run: Option<bool>,

    /// If true, wait for the durable ingest operation to reach a terminal
    /// state before returning. Defaults to false.
    pub wait: Option<bool>,

    /// Maximum number of seconds to wait for `wait=true` before returning
    /// the queued receipt with `timed_out=true`. Defaults to 30.
    pub wait_timeout_secs: Option<u64>,

    /// If true, append this entry to one agent-diary drawer for the current
    /// UTC day. Requires wing="agent-diary" and an explicit room.
    pub diary_rollup: Option<bool>,

    /// If true, enable the constrained MCP smoke-test write path. This is only
    /// accepted for small synthetic writes under wing="smoke", room="mcp";
    /// accepted smoke writes bypass gating/novelty so cleanup can rely on
    /// operation-scoped created_drawer_ids.
    pub smoke: Option<bool>,

    /// Importance ranking (0-5). Higher values appear first in wake-up context.
    /// Default 0. Use 3-5 for key decisions, architecture choices, and lessons learned.
    pub importance: Option<i32>,

    /// Optional typed memory kind. Defaults to "evidence", a raw verbatim
    /// drawer. Set "knowledge" only for a fully formed typed knowledge drawer;
    /// to turn existing evidence into a governed rule, prefer
    /// mempal_knowledge_distill over hand-built knowledge ingest.
    pub memory_kind: Option<String>,
    /// Optional typed domain: project, user, agent, skill, or global.
    pub domain: Option<String>,
    /// Optional typed field used by search/context filters.
    pub field: Option<String>,
    /// Pin this drawer for canonical recall through mempal_pinned_facts.
    pub is_pinned: Option<bool>,
    /// Optional provenance: runtime, research, or human.
    pub provenance: Option<String>,
    /// Knowledge-only on the default evidence entrypoint. A default evidence
    /// ingest rejects this field; use memory_kind="knowledge" only for a fully
    /// formed knowledge drawer, or use mempal_knowledge_distill to create
    /// governed knowledge from existing evidence.
    pub statement: Option<String>,
    /// Knowledge-only field. Rejected on an evidence drawer; use
    /// memory_kind="knowledge" or mempal_knowledge_distill.
    pub tier: Option<String>,
    /// Knowledge lifecycle status. Evidence drawers accept only active or
    /// canonical; candidate/promoted/demoted/retired/pending_review/superseded
    /// are knowledge-only lifecycle states for memory_kind="knowledge" or
    /// mempal_knowledge_distill.
    pub status: Option<String>,
    /// Knowledge-only field. Rejected on an evidence drawer; use
    /// memory_kind="knowledge" or mempal_knowledge_distill.
    pub supporting_refs: Option<Vec<String>>,
    /// Knowledge-only field. Rejected on an evidence drawer; use
    /// memory_kind="knowledge" or mempal_knowledge_distill.
    pub counterexample_refs: Option<Vec<String>>,
    /// Knowledge-only field. Rejected on an evidence drawer; use
    /// memory_kind="knowledge" or mempal_knowledge_distill.
    pub teaching_refs: Option<Vec<String>>,
    /// Knowledge-only field. Rejected on an evidence drawer; use
    /// memory_kind="knowledge" or mempal_knowledge_distill.
    pub verification_refs: Option<Vec<String>>,
    /// Knowledge-only on the default evidence entrypoint. A default evidence
    /// ingest rejects this field; use memory_kind="knowledge" or
    /// mempal_knowledge_distill for reusable rule scope constraints.
    pub scope_constraints: Option<String>,
    /// Knowledge-only on the default evidence entrypoint. A default evidence
    /// ingest rejects this field; use memory_kind="knowledge" or
    /// mempal_knowledge_distill for typed guidance hints.
    pub trigger_hints: Option<TriggerHintsDto>,
    pub anchor_kind: Option<String>,
    pub anchor_id: Option<String>,
    pub parent_anchor_id: Option<String>,
    pub cwd: Option<String>,
}

/// Internal ingest overrides used by trusted local callers.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct IngestControls {
    pub no_gate: bool,
    pub bypass_novelty: bool,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct OperationStatusRequest {
    pub operation_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TriggerHintsDto {
    pub intent_tags: Vec<String>,
    pub workflow_bias: Vec<String>,
    pub tool_needs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DeleteRequest {
    /// The drawer_id to soft-delete. The drawer is marked with a deleted_at
    /// timestamp but not physically removed. Use `mempal purge` CLI to
    /// permanently remove soft-deleted drawers.
    pub drawer_id: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeleteResponse {
    pub drawer_id: String,
    pub deleted: bool,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RollbackRequest {
    pub since: String,
    pub wing: Option<String>,
    pub room: Option<String>,
    pub project_id: Option<String>,
    pub dry_run: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RollbackResponse {
    pub since: String,
    pub deleted_count: usize,
    pub drawer_ids: Vec<String>,
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct LeaseRequest {
    /// Action to perform: "acquire", "release", "renew", or "status".
    pub action: String,
    /// Resource path to lock (e.g. "wing/room" or any string). Required for
    /// acquire/release/renew.
    pub resource_path: Option<String>,
    /// Unique identifier for the lease holder (e.g. agent session ID). Required
    /// for acquire/release/renew.
    pub holder_id: Option<String>,
    /// Time-to-live in seconds. The lease auto-expires after this duration.
    /// Default: 300 (5 minutes). Used for acquire and renew.
    pub ttl_secs: Option<u64>,
    /// Optional context about why the lease is held (for acquire only).
    pub metadata: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LeaseInfoDto {
    pub resource_path: String,
    pub holder_id: String,
    pub acquired_at: String,
    pub expires_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
    pub remaining_secs: i64,
}

impl From<crate::core::types::LeaseInfo> for LeaseInfoDto {
    fn from(info: crate::core::types::LeaseInfo) -> Self {
        Self {
            resource_path: info.resource_path,
            holder_id: info.holder_id,
            acquired_at: info.acquired_at,
            expires_at: info.expires_at,
            metadata: info.metadata,
            remaining_secs: info.remaining_secs,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LeaseResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease: Option<LeaseInfoDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leases: Option<Vec<LeaseInfoDto>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IngestOperationState {
    Queued,
    Running,
    Completed,
    Rejected,
    Failed,
}

impl IngestOperationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Rejected | Self::Failed)
    }
}

impl std::str::FromStr for IngestOperationState {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "rejected" => Ok(Self::Rejected),
            "failed" => Ok(Self::Failed),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct IngestResponse {
    /// Operation receipt/result ID. Present for queued and finalized
    /// non-dry-run ingests; omitted for dry-run previews.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// RFC3339 timestamp when the ingest was accepted into the queue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<String>,
    /// Current operation state. Present for queued and finalized non-dry-run
    /// ingests; omitted for dry-run previews.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<IngestOperationState>,
    /// True when `wait=true` timed out before the operation reached a
    /// terminal state. The queued receipt remains pollable via
    /// `mempal_operation_status`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub timed_out: bool,
    /// Primary outcome drawer for status display. This can identify an existing
    /// dedup, novelty drop, or merge target, so callers must not use it as a
    /// deletion or cleanup authority.
    pub drawer_id: String,
    /// Operation-scoped drawer IDs reported by the ingest result (one per
    /// inserted, deduplicated, dropped, or merged target chunk). These are
    /// informational affected/result IDs and are not deletion authority.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drawer_ids: Vec<String>,
    /// Drawer IDs newly created by this operation. This is the only response
    /// list safe for cleanup/delete automation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created_drawer_ids: Vec<String>,
    /// Number of chunks the content was split into. Always >= 1 for
    /// successful ingests; 0 while queued/running, during dry-run previews,
    /// and when `dropped` is true.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub chunk_count: usize,
    #[serde(default)]
    pub dropped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gating_decision: Option<GatingDecision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub novelty_action: Option<NoveltyAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub near_drawer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_warning: Option<DuplicateWarning>,
    /// Milliseconds spent waiting for the per-source ingest lock (P9-B).
    /// Omitted in dry-run and when lock was not acquired. When > 0, a
    /// concurrent ingest of the same content serialized with this call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_wait_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_drawer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_detail: Option<String>,
    /// Per-stage elapsed milliseconds captured by the async ingest pipeline.
    /// Present on completed receipt-backed writes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub timings: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fact_check_warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

fn is_zero(v: &usize) -> bool {
    *v == 0
}

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct DuplicateWarning {
    pub similar_drawer_id: String,
    pub similarity: f32,
    pub preview: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusResponse {
    pub schema_version: u32,
    pub fork_ext_version: u32,
    pub normalize_version_current: u32,
    pub stale_drawer_count: u64,
    /// True when drawer_vectors still uses the legacy l2 metric and needs reindex.
    pub vector_index_stale: bool,
    /// Number of rows in the `drawer_vectors` index (issue #302). Composes with
    /// `vector_index_stale`: the metric-only staleness check reports `false` for
    /// an empty-but-correct-metric table, so this row count exposes an index
    /// that was emptied by a failed recreate-reindex.
    pub vector_rows: i64,
    /// True when the vector index is empty (`vector_rows == 0`) while drawers
    /// exist — semantic recall is silently degraded to BM25-only until a reindex
    /// repopulates it (issue #302).
    pub vector_index_empty: bool,
    pub search_decay_mode: String,
    pub drawer_count: i64,
    pub total_compacted_drawers: u64,
    pub consolidation_runs: u64,
    pub last_consolidation_at: Option<String>,
    pub last_sleep_at: Option<String>,
    pub sleep_items_pruned: u64,
    pub sleep_items_compacted: u64,
    pub sleep_conflicts_resolved: u64,
    pub pending_card_count: i64,
    pub last_crystallization_at: Option<String>,
    pub design_insights: DesignInsightStatusDto,
    pub taxonomy_count: i64,
    pub db_size_bytes: u64,
    pub config_version: String,
    pub config_loaded_at_unix_ms: u64,
    pub diary_rollup_days: u32,
    pub scopes: Vec<ScopeCount>,
    pub source_type_distribution: Vec<SourceTypeCount>,
    /// Pinned canonical recall counts per project.
    pub pinned_fact_counts: Vec<PinnedFactProjectCount>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub aaak_spec: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub memory_protocol: String,
    pub endpoint_health: EndpointHealthDto,
    pub embed_status: EmbedStatusDto,
    /// Sticky embedder circuit plus the current vector-search fallback policy.
    pub embedder_circuit: EmbedderCircuitDto,
    /// Runtime write-admission gate state for passive ingest and explicit writes.
    pub ingest_gating_status: GatingRuntimeStatusDto,
    pub queue_stats: QueueStatsDto,
    /// Aggregate hook stdin admission counters from local diagnostics. Contains
    /// only counts, byte limits, and sanitized classes; never raw hook payloads.
    pub hook_admission: crate::hook_diagnostics::HookAdmissionStats,
    /// Live processes holding `palace.db`, `palace.db-wal`, or
    /// `palace.db-shm` open, classified as current daemon/MCP server versus
    /// stale or extra holders.
    pub db_holders: DbHolderReport,
    /// Aggregate, content-safe resource usage. Contains only process-level
    /// counters and configured SQLite cache ceilings; never drawer content,
    /// prompts, tokens, argv, or secret-bearing URLs.
    pub resource_usage: ResourceUsageDto,
    /// Aggregate per-operation-path read burst counters. Contains only numeric
    /// deltas/rates and path classes; never drawer content, prompts, raw
    /// payloads, argv, or secret-bearing URLs.
    pub io_burst: crate::observability::IoBurstSnapshot,
    pub ingest_worker_backoff: crate::observability::IngestWorkerBackoffSnapshot,
    pub vector_scan: crate::observability::VectorScanSnapshot,
    pub scrub_stats: ScrubStatsDto,
    pub chunker_stats: ChunkerStatsDto,
    pub llm_status: LlmStatusDto,
    /// Effective intelligence mode and local LLM health/degradation state.
    pub intelligence_status: IntelligenceStatusDto,
    pub turn_storage: TurnStorageStatusDto,
    /// Present when status degraded because the database could not be opened or
    /// queried. Write paths still fail closed; this is diagnostic-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database_diagnostic: Option<DatabaseDiagnosticDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct DesignInsightStatusDto {
    pub open_total: u64,
    pub high_value_open: u64,
    pub open_by_target: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SourceTypeCount {
    pub source_type: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct PinnedFactProjectCount {
    pub project_id: Option<String>,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct TurnStorageStatusDto {
    pub storage_mode: String,
    pub default_importance: i32,
    pub raw_turn_count: i64,
    pub raw_turn_wings: Vec<String>,
    pub raw_turn_rooms: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct EndpointHealthDto {
    pub embedding_reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_latency_ms: Option<u64>,
    pub embedding_detail: String,
    /// Backward-compatible alias for LLM generation health.
    pub llm_reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_latency_ms: Option<u64>,
    pub llm_control_plane_reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_control_plane_latency_ms: Option<u64>,
    pub llm_control_plane_detail: String,
    pub llm_generation_reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_generation_latency_ms: Option<u64>,
    pub llm_generation_detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct EmbedderCircuitDto {
    /// True when the sticky embedder circuit is open and vector-search falls
    /// back to BM25 unless the caller disables fallback entirely.
    pub open: bool,
    /// Consecutive embed failures since the last success, used to open the
    /// circuit.
    pub failure_count: u64,
    /// Sticky-failure threshold that opens the circuit.
    pub failure_threshold: u64,
    /// Whether BM25 fallback is enabled at all.
    pub bm25_fallback_enabled: bool,
    /// Per-query vector-embedding deadline that can still trigger BM25
    /// fallback even when the embedding endpoint itself is reachable.
    pub search_deadline_secs: u64,
    /// Current sticky vector-search mode derived from the circuit state.
    pub vector_search_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct LlmStatusDto {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<LlmEndpointStatusDto>,
    pub max_concurrent: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct LlmEndpointStatusDto {
    pub id: String,
    pub base_url: String,
    pub model: String,
    pub priority: i32,
    pub retry_interval_secs: u64,
    pub max_concurrent: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct GatingRuntimeStatusDto {
    pub enabled: bool,
    pub tier1_active: bool,
    pub tier2_active: bool,
    pub llm_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_model: Option<String>,
    pub quality_policy: String,
    pub tier2_threshold: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_threshold: Option<f64>,
    pub tier1_skip_events: Vec<String>,
    pub rules_count: usize,
    pub recent_window_secs: u64,
    pub recent_tier1_count: u64,
    pub recent_tier2_count: u64,
    pub recent_llm_pending_count: u64,
    pub recent_llm_verdict_count: u64,
    pub recent_llm_keep_count: u64,
    pub recent_llm_reject_count: u64,
    pub kept_total: usize,
    pub skipped_total: usize,
    pub dropped_total: u64,
    pub quarantined_total: u64,
    pub soft_deleted_total: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_keep_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_skip_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_llm_success_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_llm_failure_at: Option<i64>,
    pub restart_required_config_changes: Vec<String>,
}

impl From<crate::observability::GatingRuntimeStatus> for GatingRuntimeStatusDto {
    fn from(value: crate::observability::GatingRuntimeStatus) -> Self {
        Self {
            enabled: value.enabled,
            tier1_active: value.tier1_active,
            tier2_active: value.tier2_active,
            llm_active: value.llm_active,
            llm_model: value.llm_model,
            quality_policy: value.quality_policy,
            tier2_threshold: value.tier2_threshold,
            llm_threshold: value.llm_threshold,
            tier1_skip_events: value.tier1_skip_events,
            rules_count: value.rules_count,
            recent_window_secs: value.recent_window_secs,
            recent_tier1_count: value.recent_tier1_count,
            recent_tier2_count: value.recent_tier2_count,
            recent_llm_pending_count: value.recent_llm_pending_count,
            recent_llm_verdict_count: value.recent_llm_verdict_count,
            recent_llm_keep_count: value.recent_llm_keep_count,
            recent_llm_reject_count: value.recent_llm_reject_count,
            kept_total: value.kept_total,
            skipped_total: value.skipped_total,
            dropped_total: value.dropped_total,
            quarantined_total: value.quarantined_total,
            soft_deleted_total: value.soft_deleted_total,
            last_keep_at: value.last_keep_at,
            last_skip_at: value.last_skip_at,
            last_llm_success_at: value.last_llm_success_at,
            last_llm_failure_at: value.last_llm_failure_at,
            restart_required_config_changes: value.restart_required_config_changes,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct IntelligenceStatusDto {
    pub mode: String,
    pub llm_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_at_unix_ms: Option<u64>,
    pub failure_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct ScrubStatsDto {
    pub total_patterns_matched: u64,
    pub bytes_redacted: u64,
    pub redactions_per_pattern: BTreeMap<String, u64>,
}

impl From<crate::core::config::ScrubStats> for ScrubStatsDto {
    fn from(value: crate::core::config::ScrubStats) -> Self {
        Self {
            total_patterns_matched: value.total_patterns_matched,
            bytes_redacted: value.bytes_redacted,
            redactions_per_pattern: value.redactions_per_pattern,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct ChunkerStatsDto {
    pub hard_split_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_hard_split_source: Option<String>,
}

impl From<crate::ingest::chunk::ChunkerStatsSnapshot> for ChunkerStatsDto {
    fn from(value: crate::ingest::chunk::ChunkerStatsSnapshot) -> Self {
        Self {
            hard_split_count: value.hard_split_count,
            last_hard_split_source: value.last_hard_split_source,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QueueStatsDto {
    pub pending: u64,
    pub claimed: u64,
    pub active_payload_bytes: u64,
    pub active_ingest_payload_bytes: u64,
    pub ingest_payload_limit_bytes: u64,
    pub rejected_oversize: u64,
    pub failed: u64,
    pub failed_retryable: u64,
    pub failed_terminal: u64,
    pub failed_archived: u64,
    pub retrying: u64,
    pub failed_retryable_embed: u64,
    pub failed_retryable_llm: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_auto_requeue_at_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_retry_at_unix_secs: Option<u64>,
    pub rate_per_min: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_pending_age_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_processing_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_buckets: Vec<QueueFailureBucketDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retrying_buckets: Vec<QueueFailureBucketDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QueueFailureBucketDto {
    pub kind: String,
    pub retry_class: String,
    pub reason_code: String,
    pub sanitized_message: String,
    pub count: u64,
    pub min_retry_count: u32,
    pub max_retry_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_attempt_at_unix_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DatabaseDiagnosticDto {
    pub path: String,
    pub source: String,
    pub failure_kind: String,
    pub summary: String,
    pub hint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EmbedStatusDto {
    pub backend: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<EmbedEndpointStatusDto>,
    #[serde(default)]
    pub max_concurrent: usize,
    pub pending_count: u64,
    pub claimed_count: u64,
    pub failed_count: u64,
    pub degraded: bool,
    pub fail_count: u64,
    pub failure_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct EmbedEndpointStatusDto {
    pub id: String,
    pub backend: String,
    pub base_url: String,
    pub model: String,
    pub priority: i32,
    pub retry_interval_secs: u64,
    pub request_timeout_secs: u64,
    pub max_concurrent: usize,
    pub dimensions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_remaining_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_until_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure_at_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_at_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SystemWarning {
    pub level: String,
    pub message: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ScopeCount {
    pub wing: String,
    pub room: Option<String>,
    pub drawer_count: i64,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TaxonomyRequest {
    pub action: String,
    pub wing: Option<String>,
    pub room: Option<String>,
    pub keywords: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TaxonomyResponse {
    pub action: String,
    pub entries: Vec<TaxonomyEntryDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TaxonomyEntryDto {
    pub wing: String,
    pub room: String,
    pub display_name: Option<String>,
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FieldTaxonomyResponse {
    pub entries: Vec<FieldTaxonomyEntryDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FieldTaxonomyEntryDto {
    pub field: String,
    pub domains: Vec<String>,
    pub description: String,
    pub examples: Vec<String>,
}

impl From<FieldTaxonomyEntry> for FieldTaxonomyEntryDto {
    fn from(value: FieldTaxonomyEntry) -> Self {
        Self {
            field: value.field.to_string(),
            domains: value
                .domains
                .iter()
                .map(|domain| (*domain).to_string())
                .collect(),
            description: value.description.to_string(),
            examples: value
                .examples
                .iter()
                .map(|example| (*example).to_string())
                .collect(),
        }
    }
}

// --- Knowledge Graph ---

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct KgRequest {
    /// Action: "add", "query", or "invalidate".
    pub action: String,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    /// Triple ID (required for invalidate).
    pub triple_id: Option<String>,
    /// Only return currently-valid triples (default true).
    pub active_only: Option<bool>,
    /// Link to the source drawer that evidences this triple.
    pub source_drawer: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KgResponse {
    pub action: String,
    pub triples: Vec<TripleDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<KgStatsDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KgStatsDto {
    pub total: i64,
    pub active: i64,
    pub expired: i64,
    pub entities: i64,
    pub top_predicates: Vec<(String, i64)>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TripleDto {
    pub id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub confidence: f64,
    pub source_drawer: Option<String>,
}

// --- Tunnels ---

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TunnelsRequest {
    /// Action: "discover" (default), "list", "add", "delete", or "follow".
    pub action: Option<String>,
    pub left: Option<TunnelEndpointDto>,
    pub right: Option<TunnelEndpointDto>,
    pub from: Option<TunnelEndpointDto>,
    pub label: Option<String>,
    pub tunnel_id: Option<String>,
    pub wing: Option<String>,
    /// Filter for list: "passive", "explicit", or "all" (default).
    pub kind: Option<String>,
    /// Follow depth. Must be 1 or 2. Defaults to 1.
    pub max_hops: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TunnelEndpointDto {
    pub wing: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TunnelsResponse {
    pub tunnels: Vec<TunnelDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TunnelDto {
    pub tunnel_id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<TunnelEndpointDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right: Option<TunnelEndpointDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via_tunnel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hop: Option<u8>,
}

impl From<TunnelEndpointDto> for TunnelEndpoint {
    fn from(value: TunnelEndpointDto) -> Self {
        Self {
            wing: value.wing,
            room: value.room,
        }
    }
}

impl From<&TunnelEndpoint> for TunnelEndpointDto {
    fn from(value: &TunnelEndpoint) -> Self {
        Self {
            wing: value.wing.clone(),
            room: value.room.clone(),
        }
    }
}

// --- Cowork peek ---

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PeekPartnerRequest {
    /// Which agent tool's session to read. "auto" uses MCP ClientInfo.name
    /// to infer the partner (Claude ↔ Codex); "claude" or "codex" bypasses
    /// inference. If you explicitly name your own tool the call is rejected
    /// to prevent self-peek.
    pub tool: String,

    /// Maximum number of user+assistant messages to return. Default 30.
    pub limit: Option<usize>,

    /// Optional RFC3339 timestamp cutoff — only messages strictly newer than
    /// this are returned.
    pub since: Option<String>,

    /// Optional project directory whose partner session to read. Use this to
    /// peek a partner working in another project; when omitted, mempal reads the
    /// partner session for the project this MCP server runs in.
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PeekPartnerResponse {
    pub partner_tool: String,
    pub session_path: Option<String>,
    pub session_mtime: Option<String>,
    pub partner_active: bool,
    pub messages: Vec<PeekMessageDto>,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PeekMessageDto {
    pub role: String,
    pub at: String,
    pub text: String,
}

impl From<crate::cowork::PeekMessage> for PeekMessageDto {
    fn from(m: crate::cowork::PeekMessage) -> Self {
        Self {
            role: m.role,
            at: m.at,
            text: m.text,
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CoworkPushRequest {
    /// The message content to deliver. Maximum 8 KB. Short status updates,
    /// decision summaries, or drawer_id pointers. Do NOT push search results
    /// or large reasoning blocks — see Rule 10 in MEMORY_PROTOCOL.
    pub content: String,

    /// Target agent: "claude" or "codex". OMIT to infer partner from MCP
    /// client identity (Claude → Codex, Codex → Claude). Self-push is rejected.
    #[serde(default)]
    pub target_tool: Option<String>,

    /// Absolute filesystem path of the project cwd this push is scoped to.
    /// Internally normalized to git repo root via `project_identity()` so
    /// subdirectory callers land on the same inbox as repo-root callers.
    pub cwd: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkPushResponse {
    pub target_tool: String,
    pub inbox_path: String,
    pub pushed_at: String,
    pub inbox_size_after: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FactCheckRequest {
    /// Text to check for contradictions against KG triples + known entities.
    pub text: String,
    /// Optional wing filter for known-entity scope. OMIT unless you have
    /// already seen the exact wing name via mempal_status.
    pub wing: Option<String>,
    /// Optional room filter within a wing. OMIT unless explicitly named.
    pub room: Option<String>,
    /// Optional RFC3339 timestamp for the `now` cutoff used by
    /// StaleFact detection. OMIT to use current UTC time.
    pub now: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FactCheckResponse {
    pub issues: Vec<crate::factcheck::FactIssue>,
    pub checked_entities: Vec<String>,
    pub kg_triples_scanned: usize,
    /// Repeated failure patterns detected during this check (P14).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repair_packages: Vec<crate::repair::RepairPackage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DoctorRequest {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorResponse {
    pub current_version: String,
    pub supported_schema_version: u32,
    pub db: DoctorDbDto,
    pub db_holders: DbHolderReport,
    pub install: DoctorInstallDto,
    pub warnings: Vec<String>,
    pub recommendations: Vec<String>,
    pub mcp: DoctorMcpDto,
}

impl DoctorResponse {
    pub fn from_report(report: DoctorReport, mcp: DoctorMcpDto) -> Self {
        Self {
            current_version: report.current_version,
            supported_schema_version: report.supported_schema_version,
            db: report.db.into(),
            db_holders: report.db_holders,
            install: report.install.into(),
            warnings: report.warnings,
            recommendations: report.recommendations,
            mcp,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorDbDto {
    pub path: String,
    pub exists: bool,
    pub schema_version: Option<u32>,
    pub compatible: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorInstallDto {
    pub current_exe: Option<String>,
    pub path_mempal: Option<String>,
    pub path_matches_current_exe: Option<bool>,
}

impl From<DoctorDbReport> for DoctorDbDto {
    fn from(report: DoctorDbReport) -> Self {
        Self {
            path: report.path,
            exists: report.exists,
            schema_version: report.schema_version,
            compatible: report.compatible,
            error: report.error,
        }
    }
}

impl From<DoctorInstallReport> for DoctorInstallDto {
    fn from(report: DoctorInstallReport) -> Self {
        Self {
            current_exe: report.current_exe,
            path_mempal: report.path_mempal,
            path_matches_current_exe: report.path_matches_current_exe,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorMcpDto {
    pub required_tools: Vec<DoctorToolDto>,
    pub phase3_actions: Vec<String>,
    pub cowork_bus_actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorToolDto {
    pub name: String,
    pub advertised: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct BriefMcpRequest {
    pub query: String,
    pub field: Option<String>,
    pub domain: Option<String>,
    pub cwd: Option<String>,
    pub max_items: Option<usize>,
    pub dao_tian_limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BriefMcpResponse {
    pub query: String,
    pub domain: String,
    pub field: String,
    pub search_mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
    pub summary: BriefSummaryDto,
    pub key_facts: Vec<BriefFactDto>,
    pub evidence: Vec<BriefEvidenceDto>,
    pub cards: Vec<BriefCardDto>,
    pub entities: Vec<String>,
    pub unresolved_items: Vec<BriefUnresolvedItemDto>,
    pub uncertainty: Vec<BriefUncertaintyDto>,
    pub next_actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BriefSummaryDto {
    pub narrative: String,
    pub key_fact_count: usize,
    pub evidence_count: usize,
    pub card_count: usize,
    pub unresolved_count: usize,
    pub uncertainty_count: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BriefFactDto {
    pub text: String,
    pub section: String,
    pub citation: BriefCitationDto,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BriefEvidenceDto {
    pub text: String,
    pub citation: BriefCitationDto,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BriefCardDto {
    pub card_id: String,
    pub text: String,
    pub citation: BriefCitationDto,
    pub evidence_citations: Vec<BriefEvidenceCitationDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BriefUnresolvedItemDto {
    pub text: String,
    pub citation: BriefCitationDto,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BriefUncertaintyDto {
    pub kind: String,
    pub message: String,
    pub citations: Vec<BriefCitationDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BriefCitationDto {
    pub drawer_id: String,
    pub source_file: String,
    pub anchor_kind: String,
    pub anchor_id: String,
    pub card_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BriefEvidenceCitationDto {
    pub evidence_drawer_id: String,
    pub role: String,
    pub source_file: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CoworkBusRequest {
    /// Action to execute.
    pub action: String,
    /// Absolute filesystem path of the project cwd.
    pub cwd: String,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default)]
    pub tmux_target: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub now: Option<String>,
    #[serde(default)]
    pub seen_at: Option<String>,
    #[serde(default)]
    pub lines: Option<usize>,
    #[serde(default)]
    pub probe_tmux: Option<bool>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub summary_source: Option<String>,
    #[serde(default)]
    pub wing: Option<String>,
    #[serde(default)]
    pub room: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub capture: Option<bool>,
    #[serde(default)]
    pub execute: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkBusResponse {
    pub action: String,
    pub agents: Vec<CoworkBusAgentDto>,
    pub delivered: Vec<CoworkBusDeliveryDto>,
    pub messages: Vec<CoworkBusMessageDto>,
    pub events: Vec<CoworkBusEventDto>,
    pub deliveries: Vec<CoworkBusDeliveryStatusDto>,
    pub channels: Vec<CoworkBusChannelDto>,
    pub tmux_peek: Option<CoworkBusTmuxPeekDto>,
    pub doctor: Option<CoworkBusDoctorDto>,
    pub sessions: Vec<CoworkBusSessionDto>,
    pub handoff: Option<CoworkBusHandoffDto>,
    pub capture: Option<CoworkBusCaptureDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkBusAgentDto {
    pub agent_id: String,
    pub tool: String,
    pub transport: String,
    pub tmux_target: Option<String>,
    pub registered_at: String,
    pub updated_at: String,
    pub last_seen_at: Option<String>,
    pub presence: String,
    pub pending_count: usize,
    pub pending_bytes: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkBusDeliveryDto {
    pub message_id: String,
    pub target_agent_id: String,
    pub transport: String,
    pub inbox_path: Option<String>,
    pub inbox_size_after: Option<u64>,
    pub tmux_target: Option<String>,
    pub thread_id: Option<String>,
    pub channel: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkBusMessageDto {
    pub pushed_at: String,
    pub from: String,
    pub content: String,
    pub thread_id: Option<String>,
    pub channel: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkBusEventDto {
    pub event_id: String,
    pub occurred_at: String,
    pub event_type: String,
    pub status: String,
    pub actor_agent_id: Option<String>,
    pub target_agent_ids: Vec<String>,
    pub transport: Option<String>,
    pub message_preview: Option<String>,
    pub thread_id: Option<String>,
    pub channel: Option<String>,
    pub details: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkBusDeliveryStatusDto {
    pub message_id: String,
    pub event_type: String,
    pub status: String,
    pub from: String,
    pub target_agent_id: String,
    pub transport: String,
    pub message_preview: Option<String>,
    pub thread_id: Option<String>,
    pub channel: Option<String>,
    pub delivered_at: String,
    pub updated_at: String,
    pub acked_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkBusChannelDto {
    pub channel: String,
    pub agents: Vec<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkBusTmuxPeekDto {
    pub agent_id: String,
    pub tmux_target: String,
    pub lines: usize,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkBusDoctorDto {
    pub status: String,
    pub agent_count: usize,
    pub channel_count: usize,
    pub session_count: usize,
    pub stale_agents: usize,
    pub never_seen_agents: usize,
    pub pending_deliveries: usize,
    pub warnings: Vec<String>,
    pub tmux: Vec<CoworkBusTmuxProbeDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkBusTmuxProbeDto {
    pub agent_id: String,
    pub tmux_target: String,
    pub status: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkBusSessionDto {
    pub session_id: String,
    pub title: String,
    pub goal: Option<String>,
    pub agents: Vec<String>,
    pub channels: Vec<String>,
    pub thread_id: Option<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkBusHandoffDto {
    pub filters: CoworkBusHandoffFiltersDto,
    pub sessions: Vec<CoworkBusSessionDto>,
    pub agents: Vec<CoworkBusHandoffAgentDto>,
    pub pending_deliveries: Vec<CoworkBusDeliveryStatusDto>,
    pub recent_events: Vec<CoworkBusEventDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkBusHandoffFiltersDto {
    pub thread_id: Option<String>,
    pub channel: Option<String>,
    pub session_id: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkBusHandoffAgentDto {
    pub agent_id: String,
    pub tool: String,
    pub presence: String,
    pub pending_count: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkBusCaptureDto {
    pub writes: bool,
    pub drawer_id: Option<String>,
    pub wing: String,
    pub room: Option<String>,
    pub source: String,
    pub content: String,
}

impl From<CognitiveBrief> for BriefMcpResponse {
    fn from(brief: CognitiveBrief) -> Self {
        Self {
            query: brief.query,
            domain: domain_slug(&brief.domain).to_string(),
            field: brief.field,
            search_mode: brief.search_mode,
            warnings: brief.warnings,
            system_warnings: Vec::new(),
            summary: brief.summary.into(),
            key_facts: brief.key_facts.into_iter().map(Into::into).collect(),
            evidence: brief.evidence.into_iter().map(Into::into).collect(),
            cards: brief.cards.into_iter().map(Into::into).collect(),
            entities: brief.entities,
            unresolved_items: brief.unresolved_items.into_iter().map(Into::into).collect(),
            uncertainty: brief.uncertainty.into_iter().map(Into::into).collect(),
            next_actions: brief.next_actions,
        }
    }
}

impl From<BriefSummary> for BriefSummaryDto {
    fn from(summary: BriefSummary) -> Self {
        Self {
            narrative: summary.narrative,
            key_fact_count: summary.key_fact_count,
            evidence_count: summary.evidence_count,
            card_count: summary.card_count,
            unresolved_count: summary.unresolved_count,
            uncertainty_count: summary.uncertainty_count,
        }
    }
}

impl From<BriefFact> for BriefFactDto {
    fn from(fact: BriefFact) -> Self {
        Self {
            text: fact.text,
            section: fact.section,
            citation: fact.citation.into(),
        }
    }
}

impl From<BriefEvidence> for BriefEvidenceDto {
    fn from(evidence: BriefEvidence) -> Self {
        Self {
            text: evidence.text,
            citation: evidence.citation.into(),
        }
    }
}

impl From<BriefCard> for BriefCardDto {
    fn from(card: BriefCard) -> Self {
        Self {
            card_id: card.card_id,
            text: card.text,
            citation: card.citation.into(),
            evidence_citations: card
                .evidence_citations
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    }
}

impl From<BriefUnresolvedItem> for BriefUnresolvedItemDto {
    fn from(item: BriefUnresolvedItem) -> Self {
        Self {
            text: item.text,
            citation: item.citation.into(),
        }
    }
}

impl From<BriefUncertainty> for BriefUncertaintyDto {
    fn from(uncertainty: BriefUncertainty) -> Self {
        Self {
            kind: uncertainty.kind,
            message: uncertainty.message,
            citations: uncertainty.citations.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<BriefCitation> for BriefCitationDto {
    fn from(citation: BriefCitation) -> Self {
        Self {
            drawer_id: citation.drawer_id,
            source_file: citation.source_file,
            anchor_kind: anchor_kind_slug(&citation.anchor_kind).to_string(),
            anchor_id: citation.anchor_id,
            card_id: citation.card_id,
        }
    }
}

impl From<BriefEvidenceCitation> for BriefEvidenceCitationDto {
    fn from(citation: BriefEvidenceCitation) -> Self {
        Self {
            evidence_drawer_id: citation.evidence_drawer_id,
            role: knowledge_evidence_role_slug(&citation.role).to_string(),
            source_file: citation.source_file,
        }
    }
}

impl SearchResultDto {
    pub fn with_signals_from_result(
        value: SearchResult,
        progressive_disclosure: bool,
        preview_chars: usize,
    ) -> Self {
        let crate::core::types::SearchResult {
            drawer_id,
            content,
            wing,
            room,
            source_file,
            source,
            source_type,
            confidence,
            memory_kind,
            domain,
            field,
            statement,
            tier,
            status,
            anchor_kind,
            anchor_id,
            parent_anchor_id,
            is_pinned,
            importance: _,
            similarity,
            route,
            chunk_index: _,
            neighbors,
            tunnel_hints,
            effective_importance,
            matched_pattern_id,
        } = value;
        let analyzed_content = crate::session_review::analysis_content(&content);
        let signals = crate::aaak::analyze(analyzed_content);
        let original_content_bytes = content.len() as u64;
        let preview = if progressive_disclosure {
            crate::search::preview::truncate(analyzed_content, preview_chars)
        } else {
            crate::search::preview::PreviewText {
                content: content.clone(),
                truncated: false,
            }
        };

        Self {
            drawer_id,
            content: preview.content,
            content_truncated: preview.truncated,
            original_content_bytes,
            wing,
            room,
            source_file,
            source: source.as_str().to_string(),
            source_type: source_type.as_str().to_string(),
            confidence,
            similarity,
            route: route.into(),
            tunnel_hints,
            neighbors: neighbors.map(ChunkNeighborsDto::from),
            entities: signals.entities,
            topics: signals.topics,
            flags: signals.flags,
            emotions: signals.emotions,
            importance_stars: signals.importance_stars,
            effective_importance,
            memory_kind: memory_kind_slug(&memory_kind).to_string(),
            domain: domain_slug(&domain).to_string(),
            field,
            statement,
            tier: tier.as_ref().map(knowledge_tier_slug).map(str::to_string),
            status: status
                .as_ref()
                .map(knowledge_status_slug)
                .map(str::to_string),
            anchor_kind: anchor_kind_slug(&anchor_kind).to_string(),
            anchor_id,
            parent_anchor_id,
            is_pinned,
            matched_pattern_id,
        }
    }
}

impl From<crate::core::types::Drawer> for PinnedFactDto {
    fn from(value: crate::core::types::Drawer) -> Self {
        Self {
            drawer_id: value.id.clone(),
            content: value.content,
            wing: value.wing,
            room: value.room,
            source_file: value.source_file.unwrap_or(value.id),
            memory_kind: memory_kind_slug(&value.memory_kind).to_string(),
            domain: domain_slug(&value.domain).to_string(),
            field: value.field,
            status: value
                .status
                .as_ref()
                .map(knowledge_status_slug)
                .map(str::to_string),
            importance: value.importance,
            pin_order: value.pin_order,
            supersedes: value.supersedes,
        }
    }
}

impl From<&crate::search::tiered::TieredItem> for TieredContextItemDto {
    fn from(item: &crate::search::tiered::TieredItem) -> Self {
        Self {
            drawer_id: item.drawer_id.clone(),
            content: item.content.clone(),
            source_file: item.source_file.clone(),
            drawer_type: item.room.clone(),
            source: item.t3_source.clone(),
            effective_importance: item.effective_importance,
            matched_pattern_id: item.matched_pattern_id.clone(),
        }
    }
}

fn tiered_assembly_to_dto(
    tiered: TieredAssembly,
) -> (
    Vec<TieredContextItemDto>,
    Vec<TieredContextItemDto>,
    Vec<TieredContextItemDto>,
    BudgetUsedDto,
) {
    let t1: Vec<TieredContextItemDto> = tiered.t1_items.iter().map(|i| i.into()).collect();
    let t2: Vec<TieredContextItemDto> = tiered.t2_items.iter().map(|i| i.into()).collect();
    let t3: Vec<TieredContextItemDto> = tiered.t3_items.iter().map(|i| i.into()).collect();
    let budget = BudgetUsedDto {
        t1_tokens: tiered.budget_used.t1_tokens,
        t2_tokens: tiered.budget_used.t2_tokens,
        t3_tokens: tiered.budget_used.t3_tokens,
        foresight_tokens: tiered.budget_used.foresight_tokens,
        total_tokens: tiered.budget_used.total_tokens(),
    };
    (t1, t2, t3, budget)
}

impl From<ContextPack> for ContextResponse {
    fn from(value: ContextPack) -> Self {
        let (t1_dao_tian, t2_shu, t3_qi, budget_used) = match value.tiered {
            Some(tiered) => {
                let (t1, t2, t3, budget) = tiered_assembly_to_dto(tiered);
                (Some(t1), Some(t2), Some(t3), Some(budget))
            }
            None => (None, None, None, None),
        };

        Self {
            query: value.query,
            domain: domain_slug(&value.domain).to_string(),
            field: value.field,
            anchors: value
                .anchors
                .into_iter()
                .map(|anchor| ContextAnchorDto {
                    anchor_kind: anchor_kind_slug(&anchor.anchor_kind).to_string(),
                    anchor_id: anchor.anchor_id,
                })
                .collect(),
            sections: value
                .sections
                .into_iter()
                .map(ContextSectionDto::from)
                .collect(),
            recurring_themes: value
                .recurring_themes
                .into_iter()
                .map(|p| PatternSummaryDto {
                    pattern_id: p.pattern_id,
                    topic_tags: p.topic_tags,
                    session_count: p.session_count,
                    exemplar_preview: p.exemplar_preview,
                })
                .collect(),
            dao_tian: t1_dao_tian.clone(),
            shu: t2_shu.clone(),
            qi: t3_qi.clone(),
            t1_dao_tian,
            t2_shu,
            t3_qi,
            budget_used,
            repair_warnings: value.repair_warnings,
            active_skills: value
                .active_skills
                .into_iter()
                .map(|s| SkillSummaryDto {
                    skill_id: s.skill_id,
                    name: s.name,
                    trigger_description: s.trigger_description,
                    eta: s.eta,
                    status: "active".to_string(),
                    adoption_count: 0,
                    rejection_count: 0,
                })
                .collect(),
            distill_suggestions: value
                .distill_suggestions
                .into_iter()
                .map(DistillSuggestionDto::from)
                .collect(),
        }
    }
}

impl From<ContextSection> for ContextSectionDto {
    fn from(value: ContextSection) -> Self {
        Self {
            name: value.name,
            items: value.items.into_iter().map(ContextItemDto::from).collect(),
        }
    }
}

impl From<ContextItem> for ContextItemDto {
    fn from(value: ContextItem) -> Self {
        Self {
            drawer_id: value.drawer_id,
            source_file: value.source_file,
            memory_kind: memory_kind_slug(&value.memory_kind).to_string(),
            text: value.text,
            card_id: value.card_id,
            tier: value
                .tier
                .as_ref()
                .map(knowledge_tier_slug)
                .map(str::to_string),
            status: value
                .status
                .as_ref()
                .map(knowledge_status_slug)
                .map(str::to_string),
            anchor_kind: anchor_kind_slug(&value.anchor_kind).to_string(),
            anchor_id: value.anchor_id,
            parent_anchor_id: value.parent_anchor_id,
            trigger_hints: value.trigger_hints.map(TriggerHintsDto::from),
            evidence_citations: value
                .evidence_citations
                .into_iter()
                .map(|citation| ContextEvidenceCitationDto {
                    evidence_drawer_id: citation.evidence_drawer_id,
                    role: knowledge_evidence_role_slug(&citation.role).to_string(),
                    source_file: citation.source_file,
                })
                .collect(),
        }
    }
}

impl From<crate::core::types::TriggerHints> for TriggerHintsDto {
    fn from(value: crate::core::types::TriggerHints) -> Self {
        Self {
            intent_tags: value.intent_tags,
            workflow_bias: value.workflow_bias,
            tool_needs: value.tool_needs,
        }
    }
}

impl From<KnowledgeCard> for KnowledgeCardDto {
    fn from(value: KnowledgeCard) -> Self {
        Self {
            id: value.id,
            statement: value.statement,
            content: value.content,
            tier: knowledge_tier_slug(&value.tier).to_string(),
            status: knowledge_status_slug(&value.status).to_string(),
            domain: domain_slug(&value.domain).to_string(),
            field: value.field,
            anchor_kind: anchor_kind_slug(&value.anchor_kind).to_string(),
            anchor_id: value.anchor_id,
            parent_anchor_id: value.parent_anchor_id,
            scope_constraints: value.scope_constraints,
            trigger_hints: value.trigger_hints.map(TriggerHintsDto::from),
            auto_generated: value.auto_generated,
            crystallization_score: value.crystallization_score,
            source_drawer_ids: value.source_drawer_ids,
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

impl From<RetrievedKnowledgeCard> for RetrievedKnowledgeCardDto {
    fn from(value: RetrievedKnowledgeCard) -> Self {
        Self {
            card: KnowledgeCardDto::from(value.card),
            evidence_citations: value
                .evidence_citations
                .into_iter()
                .map(RetrievedEvidenceCitationDto::from)
                .collect(),
            score: value.score,
        }
    }
}

impl From<RetrievedEvidenceCitation> for RetrievedEvidenceCitationDto {
    fn from(value: RetrievedEvidenceCitation) -> Self {
        Self {
            evidence_drawer_id: value.evidence_drawer_id,
            role: knowledge_evidence_role_slug(&value.role).to_string(),
            source_file: value.source_file,
            score: value.score,
        }
    }
}

impl From<KnowledgeCardEvent> for KnowledgeCardEventDto {
    fn from(value: KnowledgeCardEvent) -> Self {
        Self {
            id: value.id,
            card_id: value.card_id,
            event_type: knowledge_event_type_slug(&value.event_type).to_string(),
            from_status: value
                .from_status
                .as_ref()
                .map(knowledge_status_slug)
                .map(str::to_string),
            to_status: value
                .to_status
                .as_ref()
                .map(knowledge_status_slug)
                .map(str::to_string),
            reason: value.reason,
            actor: value.actor,
            metadata: value.metadata,
            created_at: value.created_at,
        }
    }
}

impl From<KnowledgeCardGateReport> for KnowledgeCardGateDto {
    fn from(value: KnowledgeCardGateReport) -> Self {
        Self {
            card_id: value.card_id,
            tier: value.tier,
            status: value.status,
            target_status: value.target_status,
            allowed: value.allowed,
            reasons: value.reasons,
            requirements: KnowledgeGateRequirementsDto {
                min_supporting_refs: value.requirements.min_supporting_refs,
                min_verification_refs: value.requirements.min_verification_refs,
                min_teaching_refs: value.requirements.min_teaching_refs,
                reviewer_required: value.requirements.reviewer_required,
                counterexamples_block: value.requirements.counterexamples_block,
            },
            evidence_counts: KnowledgeGateEvidenceCountsDto {
                supporting: value.evidence_counts.supporting,
                counterexample: value.evidence_counts.counterexample,
                teaching: value.evidence_counts.teaching,
                verification: value.evidence_counts.verification,
            },
        }
    }
}

impl From<PromoteCardOutcome> for KnowledgeCardPromoteDto {
    fn from(value: PromoteCardOutcome) -> Self {
        Self {
            card_id: value.card_id,
            old_status: value.old_status,
            new_status: value.new_status,
            verification_refs: value.verification_refs,
            gate: value.gate.map(KnowledgeCardGateDto::from),
        }
    }
}

impl From<DemoteCardOutcome> for KnowledgeCardDemoteDto {
    fn from(value: DemoteCardOutcome) -> Self {
        Self {
            card_id: value.card_id,
            old_status: value.old_status,
            new_status: value.new_status,
            counterexample_refs: value.counterexample_refs,
        }
    }
}

impl From<ChunkNeighbors> for ChunkNeighborsDto {
    fn from(value: ChunkNeighbors) -> Self {
        Self {
            prev: value.prev.map(NeighborChunkDto::from),
            next: value.next.map(NeighborChunkDto::from),
        }
    }
}

impl From<NeighborChunk> for NeighborChunkDto {
    fn from(value: NeighborChunk) -> Self {
        Self {
            drawer_id: value.drawer_id,
            content: value.content,
            chunk_index: value.chunk_index,
        }
    }
}

fn memory_kind_slug(value: &MemoryKind) -> &'static str {
    value.as_str()
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

fn knowledge_tier_slug(value: &KnowledgeTier) -> &'static str {
    match value {
        KnowledgeTier::Qi => "qi",
        KnowledgeTier::Shu => "shu",
        KnowledgeTier::DaoRen => "dao_ren",
        KnowledgeTier::DaoTian => "dao_tian",
    }
}

fn knowledge_status_slug(value: &KnowledgeStatus) -> &'static str {
    match value {
        KnowledgeStatus::Active => "active",
        KnowledgeStatus::Superseded => "superseded",
        KnowledgeStatus::PendingReview => "pending_review",
        KnowledgeStatus::Candidate => "candidate",
        KnowledgeStatus::Promoted => "promoted",
        KnowledgeStatus::Canonical => "canonical",
        KnowledgeStatus::Demoted => "demoted",
        KnowledgeStatus::Retired => "retired",
    }
}

fn knowledge_evidence_role_slug(value: &crate::core::types::KnowledgeEvidenceRole) -> &'static str {
    match value {
        crate::core::types::KnowledgeEvidenceRole::Supporting => "supporting",
        crate::core::types::KnowledgeEvidenceRole::Verification => "verification",
        crate::core::types::KnowledgeEvidenceRole::Counterexample => "counterexample",
        crate::core::types::KnowledgeEvidenceRole::Teaching => "teaching",
    }
}

fn anchor_kind_slug(value: &AnchorKind) -> &'static str {
    match value {
        AnchorKind::Global => "global",
        AnchorKind::Repo => "repo",
        AnchorKind::Worktree => "worktree",
    }
}

fn knowledge_event_type_slug(value: &crate::core::types::KnowledgeEventType) -> &'static str {
    match value {
        crate::core::types::KnowledgeEventType::Created => "created",
        crate::core::types::KnowledgeEventType::Promoted => "promoted",
        crate::core::types::KnowledgeEventType::Demoted => "demoted",
        crate::core::types::KnowledgeEventType::Retired => "retired",
        crate::core::types::KnowledgeEventType::Linked => "linked",
        crate::core::types::KnowledgeEventType::Unlinked => "unlinked",
        crate::core::types::KnowledgeEventType::Updated => "updated",
        crate::core::types::KnowledgeEventType::PublishedAnchor => "published_anchor",
    }
}

impl From<RouteDecision> for RouteDecisionDto {
    fn from(value: RouteDecision) -> Self {
        Self {
            wing: value.wing,
            room: value.room,
            confidence: value.confidence,
            reason: value.reason,
        }
    }
}

impl From<TaxonomyEntry> for TaxonomyEntryDto {
    fn from(value: TaxonomyEntry) -> Self {
        Self {
            wing: value.wing,
            room: value.room,
            display_name: value.display_name,
            keywords: value.keywords,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::core::types::{
        AnchorKind, KnowledgeStatus, KnowledgeTier, MemoryDomain, MemoryKind, RouteDecision,
        SearchResult, SourceType,
    };

    use super::SearchResultDto;

    fn sample_result(content: &str) -> SearchResult {
        let source_type = SourceType::AgentInference;
        SearchResult {
            drawer_id: "drawer-1".to_string(),
            content: content.to_string(),
            wing: "mempal".to_string(),
            room: Some("signals".to_string()),
            source_file: "/tmp/signals.md".to_string(),
            source: crate::core::project::SearchResultSource::Project,
            source_type,
            confidence: crate::core::types::default_confidence(source_type),
            memory_kind: MemoryKind::Knowledge,
            domain: MemoryDomain::Project,
            field: "bootstrap".to_string(),
            statement: Some("normalized statement".to_string()),
            tier: Some(KnowledgeTier::Shu),
            status: Some(KnowledgeStatus::Promoted),
            anchor_kind: AnchorKind::Repo,
            anchor_id: "repo://signals".to_string(),
            parent_anchor_id: None,
            is_pinned: false,
            importance: 3,
            similarity: 0.91,
            route: RouteDecision {
                wing: Some("mempal".to_string()),
                room: Some("signals".to_string()),
                confidence: 0.88,
                reason: "unit test".to_string(),
            },
            chunk_index: Some(0),
            neighbors: None,
            tunnel_hints: vec!["docs".to_string()],
            effective_importance: 0.0,
            matched_pattern_id: None,
        }
    }

    #[test]
    fn test_with_signals_preserves_raw_content_and_citations() {
        let original = "We decided to use Arc<Mutex<>> for state because shared ownership mattered";
        let dto = SearchResultDto::with_signals_from_result(sample_result(original), false, 120);

        assert_eq!(dto.content, original);
        assert!(!dto.content_truncated);
        assert_eq!(dto.original_content_bytes, original.len() as u64);
        assert!(!dto.content.starts_with("V1|"));
        assert!(!dto.content.contains('★'));
        assert_eq!(dto.drawer_id, "drawer-1");
        assert_eq!(dto.source_file, "/tmp/signals.md");
        assert_eq!(dto.source_type, "agent_inference");
        assert_eq!(dto.confidence, 0.5);
        assert_eq!(dto.tunnel_hints, vec!["docs".to_string()]);
        assert_eq!(dto.memory_kind, "knowledge");
        assert_eq!(dto.tier.as_deref(), Some("shu"));
        assert!(dto.flags.contains(&"DECISION".to_string()));
        assert!(dto.importance_stars >= 2);
        assert!(!dto.entities.is_empty());
    }

    #[test]
    fn test_with_signals_applies_empty_content_sentinels() {
        let dto = SearchResultDto::with_signals_from_result(sample_result(""), false, 120);

        assert_eq!(dto.entities, vec!["UNK".to_string()]);
        assert_eq!(dto.flags, vec!["CORE".to_string()]);
        assert_eq!(dto.emotions, vec!["determ".to_string()]);
        assert!(dto.topics.is_empty());
        assert_eq!(dto.importance_stars, 2);
    }

    #[test]
    fn test_with_signals_truncates_preview_but_keeps_full_signal_analysis() {
        let original = "prefix ".repeat(40)
            + "We decided to keep signals computed from the full drawer content because the preview is only a projection.";
        let dto = SearchResultDto::with_signals_from_result(sample_result(&original), true, 32);

        assert!(dto.content_truncated);
        assert!(dto.content.ends_with('…'));
        assert_eq!(dto.original_content_bytes, original.len() as u64);
        assert!(dto.flags.contains(&"DECISION".to_string()));
    }
}
