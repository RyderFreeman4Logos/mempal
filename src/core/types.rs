use std::fmt;
use std::str::FromStr;

use super::anchor;
use serde::{Deserialize, Serialize};

use crate::core::project::SearchResultSource;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntelligenceMode {
    #[default]
    Deterministic,
    LocalLlm,
    Auto,
}

impl IntelligenceMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deterministic => "deterministic",
            Self::LocalLlm => "local_llm",
            Self::Auto => "auto",
        }
    }

    pub fn uses_llm(self) -> bool {
        !matches!(self, Self::Deterministic)
    }
}

impl fmt::Display for IntelligenceMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    #[default]
    AgentInference,
    UserExplicit,
    AgentObservation,
    SystemGenerated,
    Manual,
}

impl SourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserExplicit => "user_explicit",
            Self::AgentObservation => "agent_observation",
            Self::AgentInference => "agent_inference",
            Self::SystemGenerated => "system_generated",
            Self::Manual => "manual",
        }
    }
}

impl fmt::Display for SourceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseSourceTypeError {
    value: String,
}

impl fmt::Display for ParseSourceTypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid source_type: {}", self.value)
    }
}

impl std::error::Error for ParseSourceTypeError {}

impl FromStr for SourceType {
    type Err = ParseSourceTypeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "user_explicit" => Ok(Self::UserExplicit),
            "agent_observation" => Ok(Self::AgentObservation),
            "agent_inference" => Ok(Self::AgentInference),
            "system_generated" => Ok(Self::SystemGenerated),
            // Legacy source_type values from schema <= v10. They are accepted
            // at read boundaries so older DB snapshots can still be inspected.
            "manual" | "project" => Ok(Self::AgentInference),
            "conversation" => Ok(Self::AgentObservation),
            other => Err(ParseSourceTypeError {
                value: other.to_string(),
            }),
        }
    }
}

pub fn default_confidence(source_type: SourceType) -> f64 {
    match source_type {
        SourceType::UserExplicit => 0.9,
        SourceType::AgentObservation => 0.7,
        SourceType::AgentInference | SourceType::Manual => 0.5,
        SourceType::SystemGenerated => 0.3,
    }
}

/// First-class memory taxonomy stored on every drawer.
///
/// Lifecycle is shared across kinds through the existing drawer columns:
/// `status` marks active/canonical/superseded/etc., `supersedes` records a
/// replacement link, and `valid_from`/`valid_until` bound temporal validity in
/// SQLite. `Evidence` is the backwards-compatible raw evidence drawer kind.
/// `Knowledge` is the only kind governed by tier/supporting-ref promotion gates;
/// the other structured kinds are typed records that keep provenance on the
/// original drawer row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// Raw source evidence; persisted verbatim and used as the migration target
    /// for legacy generic drawers.
    #[default]
    Evidence,
    /// Governed knowledge with tier/status/supporting-ref lifecycle gates.
    Knowledge,
    /// Minimal claim that should be independently superseded or expired.
    AtomicFact,
    /// Durable choice with rationale and replacement history.
    Decision,
    /// Worked example, incident, or task trajectory.
    Case,
    /// Reusable procedure or operating capability.
    Skill,
    /// Forward-looking hypothesis, prediction, or watch item.
    Foresight,
    /// Legacy user/profile fact slug retained for existing rows and clients.
    ProfileFact,
    /// Durable user preference or behavioral trait.
    ProfileTrait,
}

impl MemoryKind {
    pub const SUPPORTED: &'static [Self] = &[
        Self::Evidence,
        Self::Knowledge,
        Self::AtomicFact,
        Self::Decision,
        Self::Case,
        Self::Skill,
        Self::Foresight,
        Self::ProfileFact,
        Self::ProfileTrait,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Evidence => "evidence",
            Self::Knowledge => "knowledge",
            Self::AtomicFact => "atomic_fact",
            Self::Decision => "decision",
            Self::Case => "case",
            Self::Skill => "skill",
            Self::Foresight => "foresight",
            Self::ProfileFact => "profile_fact",
            Self::ProfileTrait => "profile_trait",
        }
    }

    pub fn supported_slugs() -> &'static str {
        "evidence, knowledge, atomic_fact, decision, case, skill, foresight, profile_fact, profile_trait"
    }

    pub fn is_knowledge(self) -> bool {
        matches!(self, Self::Knowledge)
    }

    pub fn is_raw_evidence(self) -> bool {
        matches!(self, Self::Evidence)
    }

    pub fn is_typed_record(self) -> bool {
        matches!(
            self,
            Self::AtomicFact
                | Self::Decision
                | Self::Case
                | Self::Skill
                | Self::Foresight
                | Self::ProfileFact
                | Self::ProfileTrait
        )
    }

    pub fn prefers_statement_text(self) -> bool {
        self.is_knowledge() || self.is_typed_record()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseMemoryKindError {
    value: String,
}

impl fmt::Display for ParseMemoryKindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid memory_kind: {}", self.value)
    }
}

impl std::error::Error for ParseMemoryKindError {}

impl FromStr for MemoryKind {
    type Err = ParseMemoryKindError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "evidence" => Ok(Self::Evidence),
            "knowledge" => Ok(Self::Knowledge),
            "atomic_fact" => Ok(Self::AtomicFact),
            "decision" => Ok(Self::Decision),
            "case" => Ok(Self::Case),
            "skill" => Ok(Self::Skill),
            "foresight" => Ok(Self::Foresight),
            "profile_fact" => Ok(Self::ProfileFact),
            "profile_trait" => Ok(Self::ProfileTrait),
            other => Err(ParseMemoryKindError {
                value: other.to_string(),
            }),
        }
    }
}

impl fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryDomain {
    #[default]
    Project,
    User,
    Agent,
    Skill,
    Global,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorKind {
    #[default]
    Global,
    Repo,
    Worktree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Runtime,
    Research,
    Human,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeTier {
    Qi,
    Shu,
    DaoRen,
    DaoTian,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeStatus {
    Active,
    Superseded,
    PendingReview,
    Candidate,
    Promoted,
    Canonical,
    Demoted,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerHints {
    pub intent_tags: Vec<String>,
    pub workflow_bias: Vec<String>,
    pub tool_needs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeEvidenceRole {
    Supporting,
    Verification,
    Counterexample,
    Teaching,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeEventType {
    Created,
    Promoted,
    Demoted,
    Retired,
    Linked,
    Unlinked,
    Updated,
    PublishedAnchor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeAdoptionTrack {
    RuntimeAdoption,
    CardContext,
    CardEmbedding,
    Evaluator,
    ResearchAdapter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeAdoptionSignal {
    Used,
    Accepted,
    Rejected,
    Miss,
    Rollback,
    Contradiction,
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStrategy {
    RichestContent,
    LlmSummary,
}

impl CompactionStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RichestContent => "richest_content",
            Self::LlmSummary => "llm_summary",
        }
    }
}

impl fmt::Display for CompactionStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseCompactionStrategyError {
    value: String,
}

impl fmt::Display for ParseCompactionStrategyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid compaction strategy: {}", self.value)
    }
}

impl std::error::Error for ParseCompactionStrategyError {}

impl FromStr for CompactionStrategy {
    type Err = ParseCompactionStrategyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "richest_content" => Ok(Self::RichestContent),
            "llm_summary" => Ok(Self::LlmSummary),
            other => Err(ParseCompactionStrategyError {
                value: other.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionResult {
    pub target_id: String,
    pub source_ids: Vec<String>,
    pub strategy: CompactionStrategy,
    pub cluster_size: usize,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ConsolidationStats {
    pub total_compacted_drawers: u64,
    pub consolidation_runs: u64,
    pub last_consolidation_at: Option<String>,
    pub last_sleep_at: Option<String>,
    pub sleep_items_pruned: u64,
    pub sleep_items_compacted: u64,
    pub sleep_conflicts_resolved: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SleepStats {
    pub last_sleep_at: Option<String>,
    pub items_pruned: u64,
    pub items_compacted: u64,
    pub conflicts_resolved: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeAdoptionEvent {
    pub id: String,
    pub track: RuntimeAdoptionTrack,
    pub signal: RuntimeAdoptionSignal,
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeAdoptionFilter {
    pub track: Option<RuntimeAdoptionTrack>,
    pub feature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeCard {
    pub id: String,
    pub statement: String,
    pub content: String,
    pub tier: KnowledgeTier,
    pub status: KnowledgeStatus,
    pub domain: MemoryDomain,
    pub field: String,
    pub anchor_kind: AnchorKind,
    pub anchor_id: String,
    pub parent_anchor_id: Option<String>,
    pub scope_constraints: Option<String>,
    pub trigger_hints: Option<TriggerHints>,
    pub auto_generated: bool,
    pub crystallization_score: Option<f64>,
    pub source_drawer_ids: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KnowledgeCardFilter {
    pub tier: Option<KnowledgeTier>,
    pub status: Option<KnowledgeStatus>,
    pub domain: Option<MemoryDomain>,
    pub field: Option<String>,
    pub anchor_kind: Option<AnchorKind>,
    pub anchor_id: Option<String>,
    pub auto_generated: Option<bool>,
    pub pending_review: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeEvidenceLink {
    pub id: String,
    pub card_id: String,
    pub evidence_drawer_id: String,
    pub role: KnowledgeEvidenceRole,
    pub note: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeCardEvent {
    pub id: String,
    pub card_id: String,
    pub event_type: KnowledgeEventType,
    pub from_status: Option<KnowledgeStatus>,
    pub to_status: Option<KnowledgeStatus>,
    pub reason: String,
    pub actor: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy)]
pub struct BootstrapIdentityParts<'a> {
    pub memory_kind: &'a MemoryKind,
    pub domain: &'a MemoryDomain,
    pub field: &'a str,
    pub anchor_kind: &'a AnchorKind,
    pub anchor_id: &'a str,
    pub parent_anchor_id: Option<&'a str>,
    pub provenance: Option<&'a Provenance>,
    pub statement: Option<&'a str>,
    pub tier: Option<&'a KnowledgeTier>,
    pub status: Option<&'a KnowledgeStatus>,
    pub supporting_refs: &'a [String],
    pub counterexample_refs: &'a [String],
    pub teaching_refs: &'a [String],
    pub verification_refs: &'a [String],
    pub scope_constraints: Option<&'a str>,
    pub trigger_hints: Option<&'a TriggerHints>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TunnelEndpoint {
    pub wing: String,
    pub room: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplicitTunnel {
    pub id: String,
    pub left: TunnelEndpoint,
    pub right: TunnelEndpoint,
    pub label: String,
    pub created_at: String,
    pub created_by: Option<String>,
    pub deleted_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelFollowResult {
    pub endpoint: TunnelEndpoint,
    pub via_tunnel_id: String,
    pub hop: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReindexSource {
    pub source_root: Option<String>,
    pub source_file: Option<String>,
    pub project_id: Option<String>,
    pub wing: String,
    pub room: Option<String>,
    pub drawer_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Drawer {
    pub id: String,
    pub content: String,
    pub wing: String,
    pub room: Option<String>,
    pub source_file: Option<String>,
    pub source_type: SourceType,
    pub confidence: f64,
    pub added_at: String,
    pub chunk_index: Option<i64>,
    #[serde(default = "default_normalize_version")]
    pub normalize_version: u32,
    /// Importance ranking (0-5). Higher = more important for wake-up context.
    #[serde(default)]
    pub importance: i32,
    pub memory_kind: MemoryKind,
    pub domain: MemoryDomain,
    pub field: String,
    pub anchor_kind: AnchorKind,
    pub anchor_id: String,
    pub parent_anchor_id: Option<String>,
    pub provenance: Option<Provenance>,
    pub statement: Option<String>,
    pub tier: Option<KnowledgeTier>,
    pub status: Option<KnowledgeStatus>,
    #[serde(default)]
    pub supporting_refs: Vec<String>,
    #[serde(default)]
    pub counterexample_refs: Vec<String>,
    #[serde(default)]
    pub teaching_refs: Vec<String>,
    #[serde(default)]
    pub verification_refs: Vec<String>,
    pub scope_constraints: Option<String>,
    pub trigger_hints: Option<TriggerHints>,
    #[serde(default)]
    pub is_pinned: bool,
    pub pin_order: Option<i64>,
    pub supersedes: Option<String>,
    /// Dynamic importance after time-decay + retrieval boost (P13).
    /// Defaults to `importance as f64` when the column doesn't exist (pre-v10 DBs).
    #[serde(default)]
    pub effective_importance: f64,
    #[serde(default)]
    pub compacted_into: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapEvidenceArgs {
    pub id: String,
    pub content: String,
    pub wing: String,
    pub room: Option<String>,
    pub source_file: Option<String>,
    pub source_type: SourceType,
    pub added_at: String,
    pub chunk_index: Option<i64>,
    pub importance: i32,
}

impl Drawer {
    pub fn new_bootstrap_evidence(args: BootstrapEvidenceArgs) -> Self {
        let defaults = anchor::bootstrap_defaults(&args.source_type);
        let confidence = default_confidence(args.source_type);
        Self {
            id: args.id,
            content: args.content,
            wing: args.wing,
            room: args.room,
            source_file: args.source_file,
            source_type: args.source_type,
            confidence,
            added_at: args.added_at,
            chunk_index: args.chunk_index,
            normalize_version: default_normalize_version(),
            importance: args.importance,
            memory_kind: MemoryKind::Evidence,
            domain: MemoryDomain::Project,
            field: defaults.field,
            anchor_kind: defaults.anchor_kind,
            anchor_id: defaults.anchor_id,
            parent_anchor_id: defaults.parent_anchor_id,
            provenance: Some(defaults.provenance),
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
            effective_importance: args.importance as f64,
            compacted_into: None,
        }
    }
}

impl Default for Drawer {
    fn default() -> Self {
        let source_type = SourceType::default();
        let defaults = anchor::bootstrap_defaults(&source_type);
        Self {
            id: String::new(),
            content: String::new(),
            wing: String::new(),
            room: None,
            source_file: None,
            source_type,
            confidence: default_confidence(source_type),
            added_at: String::new(),
            chunk_index: None,
            normalize_version: default_normalize_version(),
            importance: 0,
            memory_kind: MemoryKind::default(),
            domain: MemoryDomain::default(),
            field: defaults.field,
            anchor_kind: defaults.anchor_kind,
            anchor_id: defaults.anchor_id,
            parent_anchor_id: defaults.parent_anchor_id,
            provenance: Some(defaults.provenance),
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
            effective_importance: 0.0,
            compacted_into: None,
        }
    }
}

fn default_normalize_version() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrawerDetails {
    pub drawer: Drawer,
    pub updated_at: Option<String>,
    pub merge_count: u32,
    pub project_id: Option<String>,
    pub vector: DrawerVectorDetails,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrawerVectorDetails {
    pub has_vector: bool,
    pub dimension: Option<usize>,
    pub embedder: Option<String>,
    pub model: Option<String>,
    pub embedder_fingerprint: Option<String>,
    pub index_version: Option<String>,
    pub current_embedder_fingerprint: Option<String>,
    pub current_index_version: String,
    pub distance_metric: Option<String>,
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrawerSummary {
    pub id: String,
    pub wing: String,
    pub room: Option<String>,
    pub source_file: Option<String>,
    pub project_id: Option<String>,
    pub added_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TunnelDrawer {
    pub drawer: Drawer,
    pub target_project_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Triple {
    pub id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub confidence: f64,
    pub source_drawer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxonomyEntry {
    pub wing: String,
    pub room: String,
    pub display_name: Option<String>,
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TripleStats {
    pub total: i64,
    pub active: i64,
    pub expired: i64,
    pub entities: i64,
    pub top_predicates: Vec<(String, i64)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteDecision {
    pub wing: Option<String>,
    pub room: Option<String>,
    pub confidence: f32,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeighborChunk {
    pub drawer_id: String,
    pub content: String,
    pub chunk_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkNeighbors {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev: Option<NeighborChunk>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<NeighborChunk>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    pub drawer_id: String,
    pub content: String,
    pub wing: String,
    pub room: Option<String>,
    pub source_file: String,
    pub source: SearchResultSource,
    pub source_type: SourceType,
    pub confidence: f64,
    pub memory_kind: MemoryKind,
    pub domain: MemoryDomain,
    pub field: String,
    pub statement: Option<String>,
    pub tier: Option<KnowledgeTier>,
    pub status: Option<KnowledgeStatus>,
    pub anchor_kind: AnchorKind,
    pub anchor_id: String,
    pub parent_anchor_id: Option<String>,
    pub is_pinned: bool,
    /// Static importance ranking (0-5). Raw-turn exclusion uses this field so
    /// access boosts cannot accidentally promote transcript storage into
    /// durable recall.
    pub importance: i32,
    pub similarity: f32,
    pub route: RouteDecision,
    #[serde(skip)]
    pub chunk_index: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub neighbors: Option<ChunkNeighbors>,
    /// Other wings that share this result's room (tunnel hints).
    /// Capped to `[search].tunnel_hints_display_cap` (default 8); excess entries
    /// are replaced by a single `"… +N more"` sentinel as the last element.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tunnel_hints: Vec<String>,
    /// Dynamic importance after time-decay + retrieval boost (P13).
    /// Loaded from the `effective_importance` column; defaults to
    /// `importance as f64` when the column doesn't exist (pre-v10 DBs).
    pub effective_importance: f64,
    /// Pattern ID of the active pattern this result's drawer belongs to (P13).
    /// Non-NULL when the result received a pattern boost during search.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_pattern_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseInfo {
    pub resource_path: String,
    pub holder_id: String,
    pub acquired_at: String,
    pub expires_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
    pub remaining_secs: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_type_round_trips_as_snake_case() {
        let encoded = serde_json::to_string(&SourceType::UserExplicit).expect("serialize");
        assert_eq!(encoded, "\"user_explicit\"");
        let decoded: SourceType = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, SourceType::UserExplicit);
        assert_eq!(SourceType::SystemGenerated.to_string(), "system_generated");
    }

    #[test]
    fn source_type_from_str_accepts_current_and_legacy_values() {
        assert_eq!(
            "agent_observation".parse::<SourceType>().expect("current"),
            SourceType::AgentObservation
        );
        assert_eq!(
            "conversation".parse::<SourceType>().expect("legacy"),
            SourceType::AgentObservation
        );
        assert_eq!(
            "manual".parse::<SourceType>().expect("legacy"),
            SourceType::AgentInference
        );
        assert!("unknown".parse::<SourceType>().is_err());
    }

    #[test]
    fn default_confidence_matches_trust_hierarchy() {
        assert_eq!(default_confidence(SourceType::UserExplicit), 0.9);
        assert_eq!(default_confidence(SourceType::AgentObservation), 0.7);
        assert_eq!(default_confidence(SourceType::AgentInference), 0.5);
        assert_eq!(default_confidence(SourceType::SystemGenerated), 0.3);
    }

    #[test]
    fn memory_kind_taxonomy_round_trips_as_stable_slugs() {
        let expected = [
            (MemoryKind::Evidence, "evidence"),
            (MemoryKind::Knowledge, "knowledge"),
            (MemoryKind::AtomicFact, "atomic_fact"),
            (MemoryKind::Decision, "decision"),
            (MemoryKind::Case, "case"),
            (MemoryKind::Skill, "skill"),
            (MemoryKind::Foresight, "foresight"),
            (MemoryKind::ProfileFact, "profile_fact"),
            (MemoryKind::ProfileTrait, "profile_trait"),
        ];

        assert_eq!(MemoryKind::SUPPORTED.len(), expected.len());
        for (kind, slug) in expected {
            assert_eq!(kind.as_str(), slug);
            assert_eq!(kind.to_string(), slug);
            assert_eq!(slug.parse::<MemoryKind>().expect("parse slug"), kind);
            let encoded = serde_json::to_string(&kind).expect("serialize kind");
            assert_eq!(encoded, format!("\"{slug}\""));
            let decoded: MemoryKind = serde_json::from_str(&encoded).expect("deserialize kind");
            assert_eq!(decoded, kind);
        }

        assert!(MemoryKind::Knowledge.is_knowledge());
        assert!(MemoryKind::Decision.is_typed_record());
        assert!(MemoryKind::ProfileTrait.prefers_statement_text());
        assert!(MemoryKind::Evidence.is_raw_evidence());
    }
}
