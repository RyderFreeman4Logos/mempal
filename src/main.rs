use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(feature = "rest")]
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use mempal::aaak::{AaakCodec, AaakMeta};
use mempal::adoption_analytics::build_runtime_adoption_analytics;
#[cfg(feature = "rest")]
use mempal::api::{ApiState, DEFAULT_REST_ADDR, serve as serve_rest_api};
use mempal::brief::{BriefRequest, assemble_brief};
use mempal::context::{ContextPack, ContextRequest, assemble_context};
use mempal::core::{
    anchor,
    compaction::merge_cluster,
    config::{CompiledPrivacyConfig, Config, ConfigHandle, default_config_path},
    db::{
        CURRENT_VECTOR_INDEX_VERSION, Database, HistoricalRejudgeAudit, ReindexVectorStash,
        VECTOR_DISTANCE_METRIC, find_similar_clusters, vector_metadata_key,
    },
    phase3::{
        CardContextDefaultProposalReport, CardContextRollbackControlReport, EvaluatorAdviceInput,
        EvaluatorAdviceReport, Phase3ReadinessReport, ResearchIngestPlanReport,
        RuntimeAdoptionCaptureInput, RuntimeAdoptionCaptureReport,
        RuntimeAdoptionCheckedRecordReport, RuntimeAdoptionGuidance, RuntimeAdoptionRecordPlan,
        RuntimeAdoptionRecordPlanInput, RuntimeAdoptionRecordQualityReport,
        RuntimeAdoptionReviewFilters, RuntimeAdoptionReviewReport,
        build_research_ingest_plan_from_value, capture_runtime_adoption_record_input,
        card_context_default_proposal, card_context_default_readiness,
        card_context_rollback_control, check_runtime_adoption_record, evaluator_advice,
        prepare_runtime_adoption_capture, prepare_runtime_adoption_record,
        review_runtime_adoption_events, runtime_adoption_guidance,
        runtime_adoption_instrumentation_policy, should_write_checked_record,
    },
    priming::PrimingRequest,
    project::{
        ProjectMigrationEvent, ProjectSearchScope, escape_project_id_for_display,
        migrate_null_project_ids, resolve_project_id,
    },
    protocol::{DEFAULT_IDENTITY_HINT, MEMORY_PROTOCOL},
    reindex::ReindexProgressStore,
    strata::{count_raw_turn_drawers, is_raw_turn, raw_turn_importance, should_store_raw_turns},
    types::{
        AnchorKind, BootstrapEvidenceArgs, CompactionStrategy, Drawer, KnowledgeCard,
        KnowledgeCardEvent, KnowledgeCardFilter, KnowledgeEventType, KnowledgeEvidenceLink,
        KnowledgeEvidenceRole, KnowledgeStatus, KnowledgeTier, MemoryDomain, MemoryKind,
        Provenance, RuntimeAdoptionEvent, RuntimeAdoptionFilter, RuntimeAdoptionSignal,
        RuntimeAdoptionTrack, SourceType, TaxonomyEntry, TriggerHints, TunnelEndpoint,
        default_confidence,
    },
    utils::{
        build_bootstrap_evidence_drawer_id, build_triple_id, current_timestamp,
        format_tunnel_endpoint, iso_timestamp, link_superseded_drawer,
        normalize_added_at as normalize_added_at_value, normalize_rfc3339_timestamp,
        source_file_or_synthetic,
    },
};
use mempal::cowork::{
    CoworkCaptureRequest, CreateSessionRequest, HandoffFilters, RegisterAgentRequest,
    SendOperation, SendRequest,
};
use mempal::crystallize::{CrystallizeOptions, CrystallizeSummary, run_crystallization};
use mempal::doctor::build_doctor_report;
use mempal::embed::build_backend_from_name;
use mempal::embed::{ConfiguredEmbedderFactory, Embedder, global_embed_status};
use mempal::field_taxonomy::{FieldTaxonomyEntry, field_taxonomy};
use mempal::ingest::gating::compile_classifier_from_config;
use mempal::ingest::{
    IngestOptions, IngestStats,
    detect::detect_format,
    gating::{IngestCandidate, evaluate_fact_check_gate, evaluate_tier1, evaluate_tier2},
    ingest_dir_with_options, ingest_file_with_options,
    normalize::{CURRENT_NORMALIZE_VERSION, NormalizeOptions, normalize_content_with_options},
    reindex::{ReindexMode, ReindexOptions, ReindexReport, reindex_sources},
};
use mempal::knowledge_anchor::{PublishAnchorRequest, publish_anchor};
use mempal::knowledge_card_backfill::{
    KnowledgeCardBackfillApplyOptions, KnowledgeCardBackfillApplyResult,
    KnowledgeCardBackfillReport, apply_backfill, build_backfill_report,
};
use mempal::knowledge_card_lifecycle::{
    DemoteCardOutcome, DemoteCardRequest, KnowledgeCardGateReport, PromoteCardOutcome,
    PromoteCardRequest, demote_card, evaluate_card_gate_by_id, promote_card,
};
use mempal::knowledge_card_retrieval::{
    KnowledgeCardRetrievalRequest, RetrievedKnowledgeCard, retrieve_knowledge_cards,
};
use mempal::knowledge_distill::{DistillPlan, DistillRequest, commit_distill, prepare_distill};
use mempal::knowledge_gate::{
    GateReport, PromotionPolicyEntry, evaluate_gate_by_id, promotion_policy,
};
use mempal::knowledge_lifecycle::{
    DemoteRequest, PromoteRequest, demote_knowledge, promote_knowledge,
};
use mempal::llm::{DEFAULT_GATING_JUDGE_PROMPT, LlmClient, LlmMessage, LlmRequest};
use mempal::mcp::{
    IngestControls, IngestOperationState, IngestRequest, IngestResponse, MempalMcpServer,
    OperationStatusRequest,
};
use mempal::observability;
use mempal::search::{SearchFilters, SearchOptions, search_with_all_options};
use mempal::sleep::{
    NremSummary, RemSummary, SalienceSummary, SleepCycleSummary, SleepPhaseSelection,
    SleepRunOptions, run_sleep_cycle,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

mod longmemeval;
mod patterns;
#[path = "cli/prime.rs"]
mod prime_cli;
mod repair_cli;
mod skills;

use crate::longmemeval::{BenchMode, LongMemEvalArgs, LongMemEvalGranularity, default_top_k};
use crate::prime_cli::{PrimeArgs, PrimeFormat};

#[derive(Parser)]
#[command(name = "mempal", about = "Project memory for coding agents", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

struct SearchCommandOptions<'a> {
    query: &'a str,
    wing: Option<&'a str>,
    room: Option<&'a str>,
    filters: SearchFilters,
    top_k: usize,
    project: Option<&'a str>,
    include_global: bool,
    all_projects: bool,
    json: bool,
    with_neighbors: bool,
    include_raw_turns: bool,
    include_expired: bool,
}

struct IngestCommandOptions<'a> {
    dir: Option<&'a Path>,
    stdin: bool,
    wing: Option<&'a str>,
    room: Option<&'a str>,
    format: Option<String>,
    project: Option<&'a str>,
    no_gate: bool,
    dry_run: bool,
    json: bool,
    no_strip_noise: bool,
    diary_rollup: bool,
    wait: bool,
    wait_timeout_secs: Option<u64>,
    source_type: Option<&'a str>,
    memory_kind: Option<&'a str>,
    domain: Option<&'a str>,
    field: Option<&'a str>,
    is_pinned: bool,
    confidence: Option<f64>,
    supersedes: Option<&'a str>,
    replace_text: Option<&'a str>,
    valid_from: Option<&'a str>,
    valid_until: Option<&'a str>,
}

#[derive(Debug, Clone, Default)]
struct ReindexVectorTarget {
    drawer_ids: Vec<String>,
    project_id: Option<String>,
    wing: Option<String>,
    room: Option<String>,
    limit: Option<usize>,
}

impl ReindexVectorTarget {
    fn is_empty(&self) -> bool {
        self.drawer_ids.is_empty()
            && self.project_id.is_none()
            && self.wing.is_none()
            && self.room.is_none()
            && self.limit.is_none()
    }

    fn has_scope_filter(&self) -> bool {
        !self.drawer_ids.is_empty()
            || self.project_id.is_some()
            || self.wing.is_some()
            || self.room.is_some()
    }
}

struct RollbackCommandOptions<'a> {
    since: &'a str,
    wing: Option<&'a str>,
    room: Option<&'a str>,
    project: Option<&'a str>,
    dry_run: bool,
    json: bool,
}

struct ConsolidateCommandOptions<'a> {
    wing: Option<&'a str>,
    room: Option<&'a str>,
    threshold: Option<f64>,
    min_cluster: Option<usize>,
    dry_run: bool,
    strategy: Option<&'a str>,
    limit: Option<usize>,
}

struct SleepCommandOptions {
    nrem: bool,
    rem: bool,
    salience: bool,
    dry_run: bool,
}

struct CrystallizeCliOptions {
    dry_run: bool,
    project: Option<String>,
    json: bool,
}

struct CardsCommandOptions {
    pending: bool,
    approve: Option<String>,
    reject: Option<String>,
    format: String,
}

struct ContextCommandArgs {
    query: String,
    field: String,
    domain: String,
    cwd: Option<PathBuf>,
    format: String,
    include_evidence: bool,
    include_cards: bool,
    max_items: usize,
    dao_tian_limit: usize,
    no_distill_suggestions: bool,
    trigger: Option<String>,
}

#[derive(Serialize)]
struct RollbackOutput {
    since: String,
    deleted_count: usize,
    drawer_ids: Vec<String>,
    dry_run: bool,
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum WakeUpFormat {
    Aaak,
    Protocol,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize taxonomy rooms from a directory.
    Init {
        dir: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    /// Import memories from a directory or stdin.
    Ingest {
        dir: Option<PathBuf>,
        #[arg(long)]
        stdin: bool,
        #[arg(long)]
        wing: Option<String>,
        #[arg(long)]
        room: Option<String>,
        #[arg(long)]
        format: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = false)]
        no_gate: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = false)]
        wait: bool,
        #[arg(long = "wait-timeout-secs")]
        wait_timeout_secs: Option<u64>,
        #[arg(long)]
        no_strip_noise: bool,
        #[arg(long)]
        diary_rollup: bool,
        #[arg(long = "source-type")]
        source_type: Option<String>,
        #[arg(long = "memory-kind")]
        memory_kind: Option<String>,
        #[arg(long)]
        domain: Option<String>,
        #[arg(long)]
        field: Option<String>,
        #[arg(long = "is-pinned", alias = "pinned", default_value_t = false)]
        is_pinned: bool,
        #[arg(long)]
        confidence: Option<f64>,
        #[arg(long)]
        supersedes: Option<String>,
        #[arg(long)]
        replace_text: Option<String>,
        #[arg(long = "valid-from")]
        valid_from: Option<String>,
        #[arg(long = "valid-until")]
        valid_until: Option<String>,
    },
    /// Inspect or wait for a receipt-backed ingest operation.
    Operation {
        #[command(subcommand)]
        command: OperationCommands,
    },
    /// Index a Claude Code session JSONL transcript as searchable conversation memory.
    /// Wing is always "conversation"; room defaults to the sessionId field or filename stem.
    IngestConversation {
        /// Path to the Claude Code session JSONL file.
        path: PathBuf,
        /// Override the room (session ID). Defaults to the `sessionId` field in the
        /// file, falling back to the filename stem.
        #[arg(long)]
        session_id: Option<String>,
        /// Project ID for project-scoped storage.
        #[arg(long)]
        project: Option<String>,
        /// Count chunks without storing any drawers.
        #[arg(long)]
        dry_run: bool,
        /// Output result as JSON.
        #[arg(long)]
        json: bool,
        /// Skip ingest gating filters.
        #[arg(long, default_value_t = false)]
        no_gate: bool,
    },
    /// Search indexed memories with optional filters.
    Search {
        query: String,
        #[arg(long)]
        wing: Option<String>,
        #[arg(long)]
        room: Option<String>,
        #[arg(long)]
        memory_kind: Option<String>,
        #[arg(long)]
        domain: Option<String>,
        #[arg(long)]
        field: Option<String>,
        #[arg(long)]
        tier: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        anchor_kind: Option<String>,
        #[arg(long, default_value_t = 10)]
        top_k: usize,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = false)]
        include_global: bool,
        #[arg(long, default_value_t = false)]
        all_projects: bool,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        with_neighbors: bool,
        #[arg(long)]
        include_raw_turns: bool,
        #[arg(long = "include-expired")]
        include_expired: bool,
    },
    /// Assemble tiered context for a query.
    Context {
        query: String,
        #[arg(long, default_value = "general")]
        field: String,
        #[arg(long, default_value = "project")]
        domain: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long, default_value = "plain")]
        format: String,
        #[arg(long)]
        include_evidence: bool,
        #[arg(long)]
        include_cards: bool,
        #[arg(long = "no-include-cards")]
        no_include_cards: bool,
        #[arg(long, default_value_t = 12)]
        max_items: usize,
        #[arg(long = "dao-tian-limit", default_value_t = 1)]
        dao_tian_limit: usize,
        #[arg(long = "no-distill-suggestions")]
        no_distill_suggestions: bool,
        /// Tiered retrieval trigger: session_start (default), on_demand, repair.
        #[arg(long)]
        trigger: Option<String>,
    },
    /// Inspect configuration and intelligence settings.
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Manage project-scoped drawer metadata.
    Project {
        #[command(subcommand)]
        command: ProjectCommands,
    },
    /// Inspect ingest gating telemetry.
    Gating {
        #[command(subcommand)]
        command: GatingCommands,
    },
    /// Print wake-up context and memory protocol.
    WakeUp {
        #[arg(long, value_enum)]
        format: Option<WakeUpFormat>,
    },
    /// Prime the current session with recent project memory.
    Prime(PrimeArgs),
    /// Encode text as an AAAK document.
    Compress { text: String },
    /// Run evaluation benchmarks.
    Bench {
        #[command(subcommand)]
        command: BenchCommands,
    },
    /// Soft-delete a drawer by ID.
    Delete { drawer_id: String },
    /// Pin a drawer by ID.
    Pin { drawer_id: String },
    /// Unpin a drawer by ID.
    Unpin { drawer_id: String },
    /// List or reorder pinned drawers.
    Pinned {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, num_args = 1..)]
        reorder: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Soft-delete recent drawers matching rollback filters.
    Rollback {
        #[arg(long)]
        since: String,
        #[arg(long)]
        wing: Option<String>,
        #[arg(long)]
        room: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Permanently purge soft-deleted drawers.
    Purge {
        /// Only purge drawers soft-deleted before this ISO timestamp
        #[arg(long)]
        before: Option<String>,
    },
    /// Rebuild indexes and derived metadata.
    Reindex {
        #[arg(long)]
        embedder: Option<String>,
        #[arg(long, default_value_t = false)]
        from_config: bool,
        #[arg(long, default_value_t = false)]
        resume: bool,
        #[arg(long, default_value_t = false)]
        stale: bool,
        #[arg(long, default_value_t = false)]
        force: bool,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// With --embedder/--from-config --stale: number of stale/new drawers to embed per write batch.
        #[arg(long)]
        batch_size: Option<usize>,
        /// With --embedder/--from-config --stale: re-embed only this drawer ID.
        #[arg(long = "drawer-id")]
        drawer_ids: Vec<String>,
        /// With --embedder/--from-config --stale: re-embed only this project.
        #[arg(long)]
        project: Option<String>,
        /// With --embedder/--from-config --stale: re-embed only this wing.
        #[arg(long)]
        wing: Option<String>,
        /// With --embedder/--from-config --stale: re-embed only this room.
        #[arg(long)]
        room: Option<String>,
        /// With --embedder/--from-config --stale: stop after this many candidate drawers.
        #[arg(long)]
        limit: Option<usize>,
        /// With --embedder/--from-config --stale: max retries for a single batch's
        /// embed call before the batch is skipped (it stays stale for a later
        /// `--stale` re-run) and the run continues. Default: 15.
        #[arg(long)]
        max_batch_retries: Option<usize>,
        /// Return failed embed-queue messages to pending for daemon retry.
        #[arg(long, default_value_t = false)]
        failed: bool,
        /// Recompute importance scores for existing drawers using rule-based heuristics.
        /// Mutually exclusive with embedder-based reindex; does not re-embed.
        #[arg(long, default_value_t = false)]
        recompute_importance: bool,
        /// With --recompute-importance: only process drawers where importance is 0.
        #[arg(long, default_value_t = false)]
        only_zero: bool,
        /// Normalise legacy Unix-epoch `added_at` values to ISO 8601 (RFC 3339 UTC).
        /// Idempotent: already-ISO rows are skipped.  Mutually exclusive with
        /// embedder-based reindex and --recompute-importance.
        #[arg(long, default_value_t = false)]
        normalize_added_at: bool,
    },
    /// Merge near-duplicate drawers using configured consolidation.
    Consolidate {
        #[arg(long)]
        wing: Option<String>,
        #[arg(long)]
        room: Option<String>,
        #[arg(long)]
        threshold: Option<f64>,
        #[arg(long = "min-cluster")]
        min_cluster: Option<usize>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        strategy: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Create candidate knowledge cards from evidence clusters.
    Crystallize {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Run offline sleep-cycle maintenance.
    Sleep {
        #[arg(long)]
        nrem: bool,
        #[arg(long)]
        rem: bool,
        #[arg(long)]
        salience: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Manage knowledge graph triples.
    Kg {
        #[command(subcommand)]
        command: KgCommands,
    },
    /// Manage distilled knowledge drawers.
    Knowledge {
        #[command(subcommand)]
        command: KnowledgeCommands,
    },
    /// Manage governed knowledge cards.
    KnowledgeCard {
        #[command(subcommand)]
        command: KnowledgeCardCommands,
    },
    /// Review or moderate auto-generated cards.
    Cards {
        #[arg(long)]
        pending: bool,
        #[arg(long)]
        approve: Option<String>,
        #[arg(long)]
        reject: Option<String>,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Record and inspect runtime adoption evidence.
    Phase3 {
        #[command(subcommand)]
        command: Phase3Commands,
    },
    /// Manage explicit tunnels between memory scopes.
    Tunnels {
        #[command(subcommand)]
        command: Option<TunnelCommands>,
    },
    /// Manage wing and room routing taxonomy.
    Taxonomy {
        #[command(subcommand)]
        command: TaxonomyCommands,
    },
    /// List recommended mind-model field taxonomy.
    FieldTaxonomy {
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Serve the MCP or REST interface.
    Serve {
        #[arg(long)]
        mcp: bool,
    },
    /// Show current status. Fast by default. Use --full for per-project and scope breakdown.
    Status {
        /// Include per-project drawer counts and scope breakdown (expensive on large databases).
        #[arg(long, default_value_t = false)]
        full: bool,
    },
    /// List recent drawers from the CLI dashboard.
    Tail {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = false)]
        follow: bool,
        #[arg(long)]
        wing: Option<String>,
        #[arg(long)]
        room: Option<String>,
        #[arg(
            long,
            help = "Filter to drawers added after this point. \
                    Duration: '10s', '15m', '2h', '3d'. \
                    ISO 8601: '2026-04-25T20:00:00Z' or '2026-04-25T20:00:00+08:00'."
        )]
        since: Option<String>,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Show a memory timeline from the CLI dashboard.
    Timeline {
        #[arg(long)]
        wing: Option<String>,
        #[arg(
            long,
            help = "Filter to drawers added after this point. \
                    Duration: '10s', '15m', '2h', '3d'. \
                    ISO 8601: '2026-04-25T20:00:00Z' or '2026-04-25T20:00:00+08:00'."
        )]
        since: Option<String>,
        #[arg(long, default_value = "text")]
        format: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Show dashboard statistics.
    Stats {
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// View a drawer by ID.
    View {
        drawer_id: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
        /// Inspect the drawer in an explicit project scope.
        #[arg(long)]
        project: Option<String>,
        /// With --project, also allow legacy global drawers.
        #[arg(long, default_value_t = false)]
        include_global: bool,
        /// Inspect drawers across all project scopes.
        #[arg(long, default_value_t = false)]
        all_projects: bool,
    },
    /// Audit gating, embedding, novelty, and stale-memory signals.
    Audit {
        #[command(subcommand)]
        command: Option<AuditCommands>,
        /// List drawers whose effective_importance is below this threshold (P13).
        #[arg(long, default_value_t = false)]
        stale: bool,
        /// Threshold for --stale (default: 0.5).
        #[arg(long, default_value_t = 0.5)]
        threshold: f64,
    },
    /// Recompute effective_importance for all active drawers using current decay params (P13).
    RecomputeImportance,
    /// Run offline contradiction check on text against KG triples +
    /// known-entity registry. Pure read, no LLM, no network.
    FactCheck {
        /// File path or `-` for stdin. Omit for stdin.
        path: Option<PathBuf>,
        /// Optional wing filter for known-entity scope.
        #[arg(long)]
        wing: Option<String>,
        /// Optional room filter within the wing.
        #[arg(long)]
        room: Option<String>,
        /// RFC3339 timestamp for the `now` cutoff (stale-fact detection).
        /// Defaults to the current UTC time.
        #[arg(long)]
        now: Option<String>,
    },
    /// Drain cowork inbox messages for the given target.
    CoworkDrain {
        #[arg(long)]
        target: String,
        #[arg(long, conflicts_with = "cwd_source")]
        cwd: Option<PathBuf>,
        #[arg(long, conflicts_with = "cwd")]
        cwd_source: Option<String>,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Show current cowork inbox state for both targets at the given cwd.
    CoworkStatus {
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Install cowork hooks.
    CoworkInstallHooks {
        #[arg(long, default_value_t = false)]
        global_codex: bool,
    },
    /// Manage external tool integrations.
    Integrations {
        #[command(subcommand)]
        command: mempal::integrations::IntegrationCommands,
    },
    /// Run passive capture hooks and hook installer.
    Hook {
        #[command(subcommand)]
        command: mempal::hook::HookCommands,
    },
    /// Review and apply CLAUDE.md hotpatch suggestions.
    Hotpatch {
        #[command(subcommand)]
        command: mempal::hotpatch::HotpatchCommands,
    },
    /// Manage the background daemon.
    Daemon {
        #[command(subcommand)]
        command: Option<DaemonSubcommand>,
        /// Run in foreground without daemonizing (legacy; applies when no subcommand is given).
        #[arg(long, default_value_t = false)]
        foreground: bool,
    },
    /// Session checkpoint management (save/restore/cleanup).
    Checkpoint {
        #[command(subcommand)]
        command: CheckpointCommands,
    },
    /// Pattern management (list, show, retire, promote).
    Patterns {
        #[command(subcommand)]
        command: patterns::PatternsCommands,
    },
    /// Skill management (list, show, promote, adopt, reject, retire).
    Skills {
        #[command(subcommand)]
        command: skills::SkillsCommands,
    },
    /// Anti-pattern detection and repair (list, show).
    Repair {
        #[command(subcommand)]
        command: repair_cli::RepairCommands,
    },
    /// Index and query conversation turns from CC, Codex, and Hermes sessions.
    Xurl {
        #[command(subcommand)]
        command: XurlCommands,
    },
    /// Register a new agent in the multi-agent cowork bus.
    #[command(name = "cowork-register")]
    CoworkRegister {
        #[arg(long = "agent-id")]
        agent_id: String,
        #[arg(long)]
        tool: String,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long, default_value = "inbox")]
        transport: String,
        #[arg(long = "tmux-target")]
        tmux_target: Option<String>,
    },
    /// Update the last-seen timestamp for an agent.
    #[command(name = "cowork-heartbeat")]
    CoworkHeartbeat {
        #[arg(long = "agent-id")]
        agent_id: String,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long = "seen-at")]
        seen_at: Option<String>,
    },
    /// List registered agents and their presence status.
    #[command(name = "cowork-agents")]
    CoworkAgents {
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long)]
        now: Option<String>,
    },
    /// Send a message from one agent to another.
    #[command(name = "cowork-send")]
    CoworkSend {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long)]
        message: String,
        #[arg(long = "thread-id")]
        thread_id: Option<String>,
    },
    /// Drain inbox messages for an agent.
    #[command(name = "cowork-agent-drain")]
    CoworkAgentDrain {
        #[arg(long = "agent-id")]
        agent_id: String,
        #[arg(long)]
        cwd: PathBuf,
    },
    /// List delivery statuses for messages in the bus.
    #[command(name = "cowork-deliveries")]
    CoworkDeliveries {
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long = "agent-id")]
        agent_id: Option<String>,
    },
    /// Acknowledge receipt of a delivered message.
    #[command(name = "cowork-ack")]
    CoworkAck {
        #[arg(long = "agent-id")]
        agent_id: String,
        #[arg(long = "message-id")]
        message_id: String,
        #[arg(long)]
        cwd: PathBuf,
    },
    /// List bus events (register, send, drain, etc.).
    #[command(name = "cowork-events")]
    CoworkEvents {
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long, default_value = "plain")]
        format: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Set channel membership (replaces existing members).
    #[command(name = "cowork-channel-set")]
    CoworkChannelSet {
        #[arg(long)]
        channel: String,
        #[arg(long = "agent")]
        agents: Vec<String>,
        #[arg(long)]
        cwd: PathBuf,
    },
    /// Send a message to all agents in a channel.
    #[command(name = "cowork-channel-send")]
    CoworkChannelSend {
        #[arg(long)]
        from: String,
        #[arg(long)]
        channel: String,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long)]
        message: String,
        #[arg(long = "thread-id")]
        thread_id: Option<String>,
    },
    /// Broadcast a message to multiple named agents.
    #[command(name = "cowork-broadcast")]
    CoworkBroadcast {
        #[arg(long)]
        from: String,
        #[arg(long = "to")]
        targets: Vec<String>,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long)]
        message: String,
        #[arg(long = "thread-id")]
        thread_id: Option<String>,
    },
    /// Print the multi-agent cowork runbook.
    #[command(name = "cowork-runbook")]
    CoworkRunbook {
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Health check for the cowork bus registry.
    #[command(name = "cowork-doctor")]
    CoworkDoctor {
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long)]
        now: Option<String>,
        #[arg(long = "probe-tmux")]
        probe_tmux: bool,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Capture recent output from a tmux-transport agent pane.
    #[command(name = "cowork-tmux-peek")]
    CoworkTmuxPeek {
        #[arg(long = "agent-id")]
        agent_id: String,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long, default_value_t = 50)]
        lines: usize,
    },
    /// Create a named team session.
    #[command(name = "cowork-session-create")]
    CoworkSessionCreate {
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long = "session-id")]
        session_id: String,
        #[arg(long)]
        title: String,
        #[arg(long = "agent")]
        agents: Vec<String>,
    },
    /// List team sessions.
    #[command(name = "cowork-sessions")]
    CoworkSessions {
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Update the status of a team session.
    #[command(name = "cowork-session-status")]
    CoworkSessionStatus {
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long = "session-id")]
        session_id: String,
        #[arg(long)]
        status: String,
    },
    /// Close a team session, optionally capturing to memory.
    #[command(name = "cowork-session-close")]
    CoworkSessionClose {
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long = "session-id")]
        session_id: String,
        #[arg(long)]
        capture: bool,
        #[arg(long)]
        execute: bool,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Show a handoff summary for the current bus state.
    #[command(name = "cowork-handoff")]
    CoworkHandoff {
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long = "thread-id")]
        thread_id: Option<String>,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Capture cowork handoff summary to memory.
    #[command(name = "cowork-capture")]
    CoworkCapture {
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long = "summary-source")]
        summary_source: String,
        #[arg(long)]
        execute: bool,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Print the mempal maintenance runbook.
    #[command(name = "maintenance-runbook")]
    MaintenanceRunbook {
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Maintenance workflows (guided-run, etc.).
    Maintenance {
        #[command(subcommand)]
        command: MaintenanceCommands,
    },
    /// Release readiness checklist.
    #[command(name = "release-readiness")]
    ReleaseReadiness {
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// System health check (schema version, install path, warnings).
    Doctor {
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Assemble a cognitive brief for a query from project memory.
    Brief {
        query: String,
        #[arg(long, default_value = "plain")]
        format: String,
    },
}

#[derive(Subcommand)]
enum OperationCommands {
    /// Print the current status of a receipt-backed ingest operation.
    Status { operation_id: String },
    /// Block until a receipt-backed ingest operation reaches a terminal state.
    Wait {
        operation_id: String,
        #[arg(long = "timeout-secs", default_value_t = 30)]
        timeout_secs: u64,
    },
}

#[derive(Subcommand)]
enum CheckpointCommands {
    /// Save a session checkpoint drawer.
    Save {
        /// Explicit content. Reads from stdin if omitted.
        #[arg(long)]
        content: Option<String>,
        /// Project ID override.
        #[arg(long)]
        project: Option<String>,
    },
    /// Retrieve the latest checkpoint content.
    Latest {
        /// Project ID filter.
        #[arg(long)]
        project: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Soft-delete checkpoints older than a given age.
    Cleanup {
        /// Max age to keep (e.g. '24h', '7d'). Default: 24h.
        #[arg(long, default_value = "24h")]
        max_age: String,
        /// Show what would be deleted without doing it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Extract session context from a Claude Code session JSONL file.
    Extract {
        /// Path to the session JSONL file.
        path: PathBuf,
        /// Only show the last N assistant messages. Default: 1.
        #[arg(long, default_value_t = 1)]
        last: usize,
    },
    /// Enable automatic checkpoint on Stop hook.
    Enable,
    /// Disable automatic checkpoint on Stop hook.
    Disable,
    /// Show whether automatic checkpoint is enabled or disabled.
    Status,
}

#[derive(Subcommand, Clone, Debug)]
enum DaemonSubcommand {
    /// Start the daemon. Fails if already running.
    Start {
        /// Run in foreground without daemonizing.
        #[arg(long, default_value_t = false)]
        foreground: bool,
    },
    /// Gracefully stop the running daemon (waits up to 30s).
    Stop,
    /// Stop and restart the daemon.
    Restart,
    /// Reap duplicate daemon processes while keeping one singleton alive.
    Reap,
    /// Show daemon status, PID, and queue stats.
    Status,
}

#[derive(Subcommand)]
enum XurlCommands {
    /// Ingest turns from a single file or scan all default tool directories.
    Ingest {
        /// Tool source: cc, codex, or hermes. Required when --path is given.
        #[arg(long, value_enum)]
        tool: Option<XurlTool>,
        /// Path to a specific file to ingest. Omit to scan default directories.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Override session ID (only used with --path).
        #[arg(long)]
        session_id: Option<String>,
        /// Print result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Semantic search over indexed conversation turns.
    Search {
        /// Search query string.
        query: String,
        /// Filter by tool: cc, codex, or hermes.
        #[arg(long, value_enum)]
        tool: Option<XurlTool>,
        /// Filter to a specific session ID.
        #[arg(long)]
        session: Option<String>,
        /// Only return turns newer than this duration (e.g. 7d, 24h, 10m).
        #[arg(long)]
        since: Option<String>,
        /// Maximum number of results to return.
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Page number (0-based).
        #[arg(long, default_value_t = 0)]
        page: usize,
        /// Also include CSA-delegated turns (excluded by default).
        #[arg(long)]
        include_csa: bool,
        /// Also include agent-generated user prompts (excluded by default).
        #[arg(long)]
        include_agent_prompts: bool,
        /// Minimum cosine similarity score (0.0–1.0). Hits below this floor are suppressed.
        #[arg(long, default_value_t = 0.70)]
        min_score: f32,
        /// Output format: markdown (default) or json.
        #[arg(long, value_enum, default_value_t = XurlFormat::Markdown)]
        format: XurlFormat,
    },
    /// Show conversation turns in reverse-chronological order.
    Timeline {
        /// Filter by tool: cc, codex, or hermes.
        #[arg(long, value_enum)]
        tool: Option<XurlTool>,
        /// Filter to a specific session ID.
        #[arg(long)]
        session: Option<String>,
        /// Only return turns newer than this duration (e.g. 7d, 24h, 10m).
        #[arg(long)]
        since: Option<String>,
        /// Number of turns per page.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Page number (0-based).
        #[arg(long, default_value_t = 0)]
        page: usize,
        /// Also include CSA-delegated turns (excluded by default).
        #[arg(long)]
        include_csa: bool,
        /// Also include agent-generated user prompts (excluded by default).
        #[arg(long)]
        include_agent_prompts: bool,
        /// Output format: markdown (default) or json.
        #[arg(long, value_enum, default_value_t = XurlFormat::Markdown)]
        format: XurlFormat,
    },
    /// Show per-tool turn counts and date ranges.
    Stats {
        /// Filter by tool: cc, codex, or hermes.
        #[arg(long, value_enum)]
        tool: Option<XurlTool>,
        /// Filter to a specific session ID.
        #[arg(long)]
        session: Option<String>,
        /// Only include turns newer than this duration (e.g. 7d, 24h, 10m).
        #[arg(long)]
        since: Option<String>,
        /// Print result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Embed all turns that still lack a vector (drains the historical backlog).
    Reindex {
        /// Re-embed only turns whose vector metadata is missing or stale.
        #[arg(long)]
        stale: bool,
        /// Rebuild vectors for all turns.
        #[arg(long)]
        force: bool,
        /// Show how many threads/turns would be embedded without writing vectors.
        #[arg(long)]
        dry_run: bool,
        /// Print progress as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Backfill project_path for historical turns without re-ingesting content.
    Backfill {
        /// Show what would be updated without writing.
        #[arg(long)]
        dry_run: bool,
        /// Print result as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum XurlTool {
    Cc,
    Codex,
    Hermes,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum XurlFormat {
    Markdown,
    Json,
}

#[derive(Serialize)]
struct XurlStatsJson {
    tools: Vec<XurlStatsToolJson>,
    unindexed_remaining: i64,
}

#[derive(Serialize)]
struct XurlStatsToolJson {
    tool: String,
    count: i64,
    first: String,
    last: String,
    min_timestamp: f64,
    max_timestamp: f64,
}

impl From<XurlTool> for mempal::xurl::model::Tool {
    fn from(t: XurlTool) -> Self {
        match t {
            XurlTool::Cc => mempal::xurl::model::Tool::Cc,
            XurlTool::Codex => mempal::xurl::model::Tool::Codex,
            XurlTool::Hermes => mempal::xurl::model::Tool::Hermes,
        }
    }
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Show current memory intelligence mode and effective LLM settings.
    Intelligence,
}

#[derive(Subcommand)]
enum AuditCommands {
    /// Show gating decisions (keep/skip) with optional filters.
    Gating {
        /// Filter by decision: keep or skip.
        #[arg(long)]
        decision: Option<String>,
        /// Filter by LLM verdict: keep or reject.
        #[arg(long)]
        llm_verdict: Option<String>,
        /// Filter by time: '10m', '2h', '24h', '3d'.
        #[arg(long)]
        since: Option<String>,
        /// Filter by project ID.
        #[arg(long)]
        project: Option<String>,
        /// Output format: text (default) or json.
        #[arg(long, default_value = "text")]
        format: String,
        /// Show raw (unescaped) text in text format.
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Show embedding failure logs.
    Embed {
        /// Filter by time: '10m', '2h', '24h', '3d'.
        #[arg(long)]
        since: Option<String>,
        /// Output format: text (default) or json.
        #[arg(long, default_value = "text")]
        format: String,
        /// Show raw (unescaped) text in text format.
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Show novelty (de-duplication) decisions.
    Novelty {
        /// Filter by time: '10m', '2h', '24h', '3d'.
        #[arg(long)]
        since: Option<String>,
        /// Output format: text (default) or json.
        #[arg(long, default_value = "text")]
        format: String,
        /// Show raw (unescaped) text in text format.
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Soft-delete low-confidence kept drawers.
    Cleanup {
        /// Show what would be deleted without actually deleting.
        #[arg(long)]
        dry_run: bool,
        /// Score threshold below which to delete (default 0.55).
        #[arg(long)]
        score_threshold: Option<f32>,
        /// Filter by wing (default 'hooks-raw').
        #[arg(long)]
        wing: Option<String>,
    },
}

#[derive(Subcommand)]
enum TaxonomyCommands {
    /// List taxonomy routing entries.
    List,
    /// Update keywords for a wing and room.
    Edit {
        wing: String,
        room: String,
        #[arg(long)]
        keywords: String,
    },
}

#[derive(Subcommand)]
enum GatingCommands {
    /// Show ingest gating decision statistics.
    Stats {
        #[arg(long)]
        since: Option<String>,
    },
}

#[derive(Subcommand)]
enum KgCommands {
    /// Add a knowledge graph triple.
    Add {
        subject: String,
        predicate: String,
        object: String,
        #[arg(long)]
        source_drawer: Option<String>,
    },
    /// Query knowledge graph triples.
    Query {
        #[arg(long)]
        subject: Option<String>,
        #[arg(long)]
        predicate: Option<String>,
        #[arg(long)]
        object: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Invalidate a knowledge graph triple.
    Invalidate { triple_id: String },
    /// Show the timeline for an entity.
    Timeline { entity: String },
    /// Show knowledge graph statistics.
    Stats,
    /// List knowledge graph triples.
    List,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum KnowledgeCommands {
    /// Distill reusable knowledge from evidence drawers.
    Distill {
        #[arg(long)]
        statement: String,
        #[arg(long)]
        content: String,
        #[arg(long)]
        tier: String,
        #[arg(long = "supporting-ref", required = true)]
        supporting_refs: Vec<String>,
        #[arg(long, default_value = "mempal")]
        wing: String,
        #[arg(long, default_value = "knowledge")]
        room: String,
        #[arg(long, default_value = "project")]
        domain: String,
        #[arg(long, default_value = "general")]
        field: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long = "scope-constraints")]
        scope_constraints: Option<String>,
        #[arg(long = "counterexample-ref")]
        counterexample_refs: Vec<String>,
        #[arg(long = "teaching-ref")]
        teaching_refs: Vec<String>,
        #[arg(long = "intent-tag")]
        intent_tags: Vec<String>,
        #[arg(long = "workflow-bias")]
        workflow_bias: Vec<String>,
        #[arg(long = "tool-need")]
        tool_needs: Vec<String>,
        #[arg(long, default_value_t = 2)]
        importance: i32,
        #[arg(long)]
        dry_run: bool,
    },
    /// Promote a knowledge drawer to a new lifecycle status.
    Promote {
        drawer_id: String,
        #[arg(long)]
        status: String,
        #[arg(long = "verification-ref", required = true)]
        verification_refs: Vec<String>,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        reviewer: Option<String>,
    },
    /// Demote a knowledge drawer with counter-evidence.
    Demote {
        drawer_id: String,
        #[arg(long)]
        status: String,
        #[arg(long = "evidence-ref", required = true)]
        evidence_refs: Vec<String>,
        #[arg(long)]
        reason: String,
        #[arg(long = "reason-type")]
        reason_type: String,
    },
    /// Evaluate promotion readiness for a knowledge drawer.
    Gate {
        drawer_id: String,
        #[arg(long = "target-status")]
        target_status: Option<String>,
        #[arg(long)]
        reviewer: Option<String>,
        #[arg(long = "allow-counterexamples")]
        allow_counterexamples: bool,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Show the knowledge promotion policy.
    Policy {
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Publish knowledge to a broader anchor scope.
    PublishAnchor {
        drawer_id: String,
        #[arg(long)]
        to: String,
        #[arg(long = "target-anchor-id")]
        target_anchor_id: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        reviewer: Option<String>,
    },
}

#[derive(Subcommand)]
enum KnowledgeCardCommands {
    /// Create a knowledge card.
    Create {
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        statement: String,
        #[arg(long)]
        content: String,
        #[arg(long)]
        tier: String,
        #[arg(long)]
        status: String,
        #[arg(long, default_value = "project")]
        domain: String,
        #[arg(long, default_value = "general")]
        field: String,
        #[arg(long = "anchor-kind", default_value = "repo")]
        anchor_kind: String,
        #[arg(long = "anchor-id")]
        anchor_id: String,
        #[arg(long = "parent-anchor-id")]
        parent_anchor_id: Option<String>,
        #[arg(long = "scope-constraints")]
        scope_constraints: Option<String>,
        #[arg(long = "intent-tag")]
        intent_tags: Vec<String>,
        #[arg(long = "workflow-bias")]
        workflow_bias: Vec<String>,
        #[arg(long = "tool-need")]
        tool_needs: Vec<String>,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Show a knowledge card by ID.
    Get {
        card_id: String,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// List knowledge cards with optional filters.
    List {
        #[arg(long)]
        tier: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        domain: Option<String>,
        #[arg(long)]
        field: Option<String>,
        #[arg(long = "anchor-kind")]
        anchor_kind: Option<String>,
        #[arg(long = "anchor-id")]
        anchor_id: Option<String>,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Retrieve knowledge cards for a query.
    Retrieve {
        query: String,
        #[arg(long, default_value = "project")]
        domain: String,
        #[arg(long, default_value = "general")]
        field: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long = "top-k", default_value_t = 5)]
        top_k: usize,
        #[arg(long = "evidence-top-k", default_value_t = 20)]
        evidence_top_k: usize,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Link evidence to a knowledge card.
    Link {
        card_id: String,
        evidence_drawer_id: String,
        #[arg(long)]
        role: String,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        id: Option<String>,
    },
    /// Append a lifecycle event to a knowledge card.
    Event {
        card_id: String,
        #[arg(long = "type")]
        event_type: String,
        #[arg(long)]
        reason: String,
        #[arg(long = "from-status")]
        from_status: Option<String>,
        #[arg(long = "to-status")]
        to_status: Option<String>,
        #[arg(long)]
        actor: Option<String>,
        #[arg(long = "metadata-json")]
        metadata_json: Option<String>,
        #[arg(long)]
        id: Option<String>,
    },
    /// List lifecycle events for a knowledge card.
    Events {
        card_id: String,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Evaluate lifecycle readiness for a knowledge card.
    Gate {
        card_id: String,
        #[arg(long = "target-status")]
        target_status: Option<String>,
        #[arg(long)]
        reviewer: Option<String>,
        #[arg(long, default_value_t = false)]
        allow_counterexamples: bool,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Promote a knowledge card to a new lifecycle status.
    Promote {
        card_id: String,
        #[arg(long)]
        status: String,
        #[arg(long = "verification-ref")]
        verification_refs: Vec<String>,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        reviewer: Option<String>,
        #[arg(long, default_value_t = false)]
        allow_counterexamples: bool,
        #[arg(long, default_value_t = true)]
        enforce_gate: bool,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Demote a knowledge card with counter-evidence.
    Demote {
        card_id: String,
        #[arg(long)]
        status: String,
        #[arg(long = "evidence-ref")]
        evidence_refs: Vec<String>,
        #[arg(long)]
        reason: String,
        #[arg(long = "reason-type")]
        reason_type: String,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Plan evidence backfill for knowledge cards.
    BackfillPlan {
        #[arg(long)]
        tier: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        domain: Option<String>,
        #[arg(long)]
        field: Option<String>,
        #[arg(long = "anchor-kind")]
        anchor_kind: Option<String>,
        #[arg(long = "anchor-id")]
        anchor_id: Option<String>,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Apply evidence backfill for knowledge cards.
    BackfillApply {
        #[arg(long)]
        tier: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        domain: Option<String>,
        #[arg(long)]
        field: Option<String>,
        #[arg(long = "anchor-kind")]
        anchor_kind: Option<String>,
        #[arg(long = "anchor-id")]
        anchor_id: Option<String>,
        #[arg(long)]
        execute: bool,
        #[arg(long, default_value = "plain")]
        format: String,
    },
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // clap command enums favor direct argument fields over boxing.
enum Phase3Commands {
    /// Manage runtime adoption records.
    Adoption {
        #[command(subcommand)]
        command: Phase3AdoptionCommands,
    },
    /// Request deterministic evaluator advice.
    Evaluator {
        #[command(subcommand)]
        command: Phase3EvaluatorCommands,
    },
    /// Prepare a default-on proposal for a candidate feature.
    DefaultProposal {
        candidate: String,
        #[arg(long = "rollback-criterion")]
        rollback_criteria: Vec<String>,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Enable or disable a governed default feature.
    DefaultControl {
        candidate: String,
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
        #[arg(long = "rollback-criterion")]
        rollback_criteria: Vec<String>,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Evaluate rollback evidence for a default feature.
    RollbackControl {
        candidate: String,
        #[arg(long)]
        execute: bool,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Evaluate Phase 3 gate readiness for a candidate.
    Gate {
        candidate: String,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Show readiness for a Phase 3 candidate.
    Readiness {
        candidate: String,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Validate a research adapter plan.
    ResearchValidatePlan {
        path: PathBuf,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Ingest a validated research adapter plan.
    ResearchIngestPlan {
        path: PathBuf,
        #[arg(long)]
        execute: bool,
        #[arg(long, default_value = "plain")]
        format: String,
    },
}

#[derive(Subcommand)]
enum Phase3EvaluatorCommands {
    /// Produce deterministic evaluator advice.
    Advise {
        #[arg(long = "evaluator-id")]
        evaluator_id: Option<String>,
        #[arg(long = "subject-kind")]
        subject_kind: String,
        #[arg(long = "subject-id")]
        subject_id: String,
        #[arg(long = "proposed-action")]
        proposed_action: String,
        #[arg(long = "evidence-ref")]
        evidence_refs: Vec<String>,
        #[arg(long = "counterexample-ref")]
        counterexample_refs: Vec<String>,
        #[arg(long = "risk-note")]
        risk_notes: Vec<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long, default_value = "plain")]
        format: String,
    },
}

#[derive(Subcommand)]
enum Phase3AdoptionCommands {
    /// Show Phase 3 adoption guidance.
    Guidance {
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Show runtime adoption instrumentation policy.
    InstrumentationPolicy {
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Prepare a runtime adoption record without writing it.
    PrepareRecord {
        #[arg(long)]
        track: String,
        #[arg(long)]
        signal: String,
        #[arg(long)]
        feature: String,
        #[arg(long)]
        query: Option<String>,
        #[arg(long = "context-hash")]
        context_hash: Option<String>,
        #[arg(long = "card-id")]
        card_id: Option<String>,
        #[arg(long = "evaluator-id")]
        evaluator_id: Option<String>,
        #[arg(long = "research-report-id")]
        research_report_id: Option<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long = "metadata-json")]
        metadata_json: Option<String>,
        #[arg(long)]
        id: Option<String>,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Capture a runtime adoption outcome.
    Capture {
        #[arg(long)]
        surface: String,
        #[arg(long)]
        outcome: String,
        #[arg(long)]
        query: Option<String>,
        #[arg(long = "context-hash")]
        context_hash: Option<String>,
        #[arg(long = "card-id")]
        card_id: Option<String>,
        #[arg(long = "evaluator-id")]
        evaluator_id: Option<String>,
        #[arg(long = "research-report-id")]
        research_report_id: Option<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long = "metadata-json")]
        metadata_json: Option<String>,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        execute: bool,
        #[arg(long = "allow-warnings")]
        allow_warnings: bool,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Check runtime adoption record quality.
    CheckRecord {
        #[arg(long)]
        track: String,
        #[arg(long)]
        signal: String,
        #[arg(long)]
        feature: String,
        #[arg(long)]
        query: Option<String>,
        #[arg(long = "context-hash")]
        context_hash: Option<String>,
        #[arg(long = "card-id")]
        card_id: Option<String>,
        #[arg(long = "evaluator-id")]
        evaluator_id: Option<String>,
        #[arg(long = "research-report-id")]
        research_report_id: Option<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long = "metadata-json")]
        metadata_json: Option<String>,
        #[arg(long)]
        id: Option<String>,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Write a quality-checked runtime adoption record.
    RecordChecked {
        #[arg(long)]
        track: String,
        #[arg(long)]
        signal: String,
        #[arg(long)]
        feature: String,
        #[arg(long)]
        query: Option<String>,
        #[arg(long = "context-hash")]
        context_hash: Option<String>,
        #[arg(long = "card-id")]
        card_id: Option<String>,
        #[arg(long = "evaluator-id")]
        evaluator_id: Option<String>,
        #[arg(long = "research-report-id")]
        research_report_id: Option<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long = "metadata-json")]
        metadata_json: Option<String>,
        #[arg(long)]
        id: Option<String>,
        #[arg(long = "allow-warnings")]
        allow_warnings: bool,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Review runtime adoption evidence.
    Review {
        #[arg(long)]
        track: Option<String>,
        #[arg(long)]
        feature: Option<String>,
        #[arg(long)]
        signal: Option<String>,
        #[arg(long, default_value_t = 10_000)]
        limit: usize,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Write a runtime adoption record.
    Record {
        #[arg(long)]
        track: String,
        #[arg(long)]
        signal: String,
        #[arg(long)]
        feature: String,
        #[arg(long)]
        query: Option<String>,
        #[arg(long = "context-hash")]
        context_hash: Option<String>,
        #[arg(long = "card-id")]
        card_id: Option<String>,
        #[arg(long = "evaluator-id")]
        evaluator_id: Option<String>,
        #[arg(long = "research-report-id")]
        research_report_id: Option<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long = "metadata-json")]
        metadata_json: Option<String>,
        #[arg(long)]
        id: Option<String>,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// List runtime adoption records.
    List {
        #[arg(long)]
        track: Option<String>,
        #[arg(long)]
        feature: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Show runtime adoption statistics.
    Stats {
        #[arg(long)]
        track: Option<String>,
        #[arg(long)]
        feature: Option<String>,
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Run a child command and capture adoption outcome.
    Wrap {
        #[arg(long)]
        surface: String,
        #[arg(long)]
        query: Option<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        outcome: Option<String>,
        #[arg(long)]
        execute: bool,
        #[arg(long = "allow-warnings")]
        allow_warnings: bool,
        #[arg(long, default_value = "json")]
        format: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        child_cmd: Vec<String>,
    },
    /// Show runtime adoption analytics.
    Analytics {
        #[arg(long, default_value = "plain")]
        format: String,
    },
}

#[derive(Subcommand)]
enum MaintenanceCommands {
    /// Print a guided maintenance run report.
    #[command(name = "guided-run")]
    GuidedRun {
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// Re-run current retention policy over historical drawers.
    Rejudge {
        /// Mutate storage. Omit for dry-run.
        #[arg(long, default_value_t = false)]
        execute: bool,
        /// Irreversibly delete candidates instead of auditable soft-delete.
        #[arg(long = "hard-delete", default_value_t = false)]
        hard_delete: bool,
        /// Limit number of active drawers scanned.
        #[arg(long, default_value_t = 1000)]
        limit: usize,
        #[arg(long)]
        wing: Option<String>,
        #[arg(long)]
        room: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value = "plain")]
        format: String,
    },
}

#[derive(Subcommand)]
enum TunnelCommands {
    /// Add an explicit tunnel between two scopes.
    Add {
        #[arg(long)]
        left: String,
        #[arg(long)]
        right: String,
        #[arg(long)]
        label: String,
    },
    /// List explicit tunnels.
    List {
        #[arg(long)]
        wing: Option<String>,
        #[arg(long, default_value = "all")]
        kind: String,
    },
    /// Delete an explicit tunnel by ID.
    Delete { tunnel_id: String },
    /// Follow tunnels from a starting scope.
    Follow {
        #[arg(long)]
        from: String,
        #[arg(long, default_value_t = 1)]
        hops: u8,
    },
}

#[derive(Subcommand)]
enum ProjectCommands {
    /// Migrate drawers into a project scope.
    Migrate {
        #[arg(long)]
        project: String,
        #[arg(long)]
        wing: Option<String>,
    },
}

#[derive(Subcommand)]
enum BenchCommands {
    /// Run the LongMemEval benchmark.
    #[command(name = "longmemeval")]
    LongMemEval {
        data_file: PathBuf,
        #[arg(long, value_enum, default_value_t = BenchMode::Raw)]
        mode: BenchMode,
        #[arg(long, value_enum, default_value_t = LongMemEvalGranularity::Session)]
        granularity: LongMemEvalGranularity,
        #[arg(long, default_value_t = 0)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        skip: usize,
        #[arg(long, default_value_t = default_top_k())]
        top_k: usize,
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

struct WrapCommandOpts {
    surface: String,
    query: Option<String>,
    note: Option<String>,
    outcome: Option<String>,
    execute: bool,
    allow_warnings: bool,
    format: String,
    child_cmd: Vec<String>,
}

#[derive(Serialize)]
struct WrapReport {
    writes: bool,
    execute: bool,
    child_exit_code: i32,
    child_stdout: String,
    outcome: String,
    capture: RuntimeAdoptionCaptureReport,
}

#[derive(Serialize)]
struct ReleaseReadinessReport {
    writes: bool,
    checks: Vec<ReleaseReadinessCheck>,
    recommended_commands: Vec<String>,
}

#[derive(Serialize)]
struct ReleaseReadinessCheck {
    name: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Serialize)]
struct MaintenanceGuidedRunReport {
    writes: bool,
    steps: Vec<MaintenanceStep>,
}

#[derive(Serialize)]
struct MaintenanceStep {
    command: String,
    description: String,
}

struct HistoricalRejudgeOptions<'a> {
    execute: bool,
    hard_delete: bool,
    limit: usize,
    wing: Option<&'a str>,
    room: Option<&'a str>,
    project: Option<&'a str>,
    format: &'a str,
}

#[derive(Debug, Clone, Serialize)]
struct HistoricalRejudgeReport {
    dry_run: bool,
    mutation: String,
    scanned_count: usize,
    candidate_count: usize,
    protected_count: usize,
    mutated_count: usize,
    estimated_bytes_reclaimed: usize,
    judge_model: Option<String>,
    config_version: String,
    wings_rooms: Vec<HistoricalRejudgeScopeCount>,
    examples: Vec<HistoricalRejudgeExample>,
}

#[derive(Debug, Clone, Serialize)]
struct HistoricalRejudgeScopeCount {
    wing: String,
    room: Option<String>,
    count: usize,
}

#[derive(Debug, Clone, Serialize)]
struct HistoricalRejudgeExample {
    drawer_id: String,
    wing: String,
    room: Option<String>,
    reason: String,
    score: Option<f64>,
    judge: String,
    bytes: usize,
    preview: String,
}

#[derive(Debug, Clone)]
struct HistoricalRejudgeDecision {
    delete_candidate: bool,
    protected: bool,
    reason: String,
    label: Option<String>,
    score: Option<f64>,
    tier: u8,
    judge: String,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        for cause in error.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let config_path = default_config_path();

    match &cli.command {
        Commands::CoworkDrain {
            target,
            cwd,
            cwd_source,
            format,
        } => {
            return cowork_drain_command(
                target.clone(),
                cwd.clone(),
                cwd_source.clone(),
                format.clone(),
            );
        }
        Commands::CoworkStatus { cwd } => {
            let resolved = match cwd {
                Some(p) => p.clone(),
                None => std::env::current_dir()
                    .context("cowork-status: failed to determine current directory")?,
            };
            return cowork_status_command(resolved);
        }
        Commands::CoworkInstallHooks { global_codex } => {
            return cowork_install_hooks_command(*global_codex);
        }
        Commands::Integrations { command } => {
            return mempal::integrations::run_command(command.clone());
        }
        Commands::Hook { command } => {
            return mempal::hook::run_command(command.clone());
        }
        Commands::Hotpatch { command } => {
            let config = Config::load_from(&config_path)
                .with_context(|| format!("failed to load config {}", config_path.display()))?;
            let db_path = expand_home(&config.db_path);
            let mempal_home = db_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            return mempal::hotpatch::run_command(&config, &mempal_home, command.clone());
        }
        Commands::Config { command } => {
            let config = Config::load_from(&config_path)
                .with_context(|| format!("failed to load config {}", config_path.display()))?;
            return config_command(&config, command);
        }
        Commands::Daemon {
            command,
            foreground,
        } => {
            let cfg_path = default_config_path();
            return match command.as_ref() {
                None => run_daemon_start(cfg_path, *foreground),
                Some(DaemonSubcommand::Start { foreground: fg }) => run_daemon_start(cfg_path, *fg),
                Some(DaemonSubcommand::Stop) => {
                    let db_path = daemon_config_db_path(&cfg_path)?;
                    run_daemon_stop(&db_path)
                }
                Some(DaemonSubcommand::Restart) => run_daemon_restart(cfg_path),
                Some(DaemonSubcommand::Reap) => {
                    let db_path = daemon_config_db_path(&cfg_path)?;
                    run_daemon_reap(&db_path)
                }
                Some(DaemonSubcommand::Status) => {
                    let db_path = daemon_config_db_path(&cfg_path)?;
                    run_daemon_status(&db_path)
                }
            };
        }
        Commands::Prime(args) => {
            return prime_command(&config_path, args.clone());
        }
        Commands::CoworkRegister {
            agent_id,
            tool,
            cwd,
            transport,
            tmux_target,
        } => {
            return cowork_register_command(
                agent_id.clone(),
                tool.clone(),
                cwd.clone(),
                transport.clone(),
                tmux_target.clone(),
            );
        }
        Commands::CoworkHeartbeat {
            agent_id,
            cwd,
            seen_at,
        } => {
            return cowork_heartbeat_command(agent_id.clone(), cwd.clone(), seen_at.clone());
        }
        Commands::CoworkAgents { cwd, now } => {
            return cowork_agents_command(cwd.clone(), now.clone());
        }
        Commands::CoworkSend {
            from,
            to,
            cwd,
            message,
            thread_id,
        } => {
            return cowork_send_command(
                from.clone(),
                to.clone(),
                cwd.clone(),
                message.clone(),
                thread_id.clone(),
            );
        }
        Commands::CoworkAgentDrain { agent_id, cwd } => {
            return cowork_agent_drain_command(agent_id.clone(), cwd.clone());
        }
        Commands::CoworkDeliveries { cwd, agent_id } => {
            return cowork_deliveries_command(cwd.clone(), agent_id.clone());
        }
        Commands::CoworkAck {
            agent_id,
            message_id,
            cwd,
        } => {
            return cowork_ack_command(agent_id.clone(), message_id.clone(), cwd.clone());
        }
        Commands::CoworkEvents { cwd, format, limit } => {
            return cowork_events_command(cwd.clone(), format.clone(), *limit);
        }
        Commands::CoworkChannelSet {
            channel,
            agents,
            cwd,
        } => {
            return cowork_channel_set_command(channel.clone(), agents.clone(), cwd.clone());
        }
        Commands::CoworkChannelSend {
            from,
            channel,
            cwd,
            message,
            thread_id,
        } => {
            return cowork_channel_send_command(
                from.clone(),
                channel.clone(),
                cwd.clone(),
                message.clone(),
                thread_id.clone(),
            );
        }
        Commands::CoworkBroadcast {
            from,
            targets,
            cwd,
            message,
            thread_id,
        } => {
            return cowork_broadcast_command(
                from.clone(),
                targets.clone(),
                cwd.clone(),
                message.clone(),
                thread_id.clone(),
            );
        }
        Commands::CoworkRunbook { format } => {
            return cowork_runbook_command(format.clone());
        }
        Commands::CoworkDoctor {
            cwd,
            now,
            probe_tmux,
            format,
        } => {
            return cowork_doctor_command(cwd.clone(), now.clone(), *probe_tmux, format.clone());
        }
        Commands::CoworkTmuxPeek {
            agent_id,
            cwd,
            lines,
        } => {
            return cowork_tmux_peek_command(agent_id.clone(), cwd.clone(), *lines);
        }
        Commands::CoworkSessionCreate {
            cwd,
            session_id,
            title,
            agents,
        } => {
            return cowork_session_create_command(
                cwd.clone(),
                session_id.clone(),
                title.clone(),
                agents.clone(),
            );
        }
        Commands::CoworkSessions { cwd, format } => {
            return cowork_sessions_command(cwd.clone(), format.clone());
        }
        Commands::CoworkSessionStatus {
            cwd,
            session_id,
            status,
        } => {
            return cowork_session_status_command(cwd.clone(), session_id.clone(), status.clone());
        }
        Commands::CoworkSessionClose {
            cwd,
            session_id,
            capture,
            execute,
            format,
        } => {
            let config = Config::load_from(&config_path)
                .with_context(|| format!("failed to load config {}", config_path.display()))?;
            let db_path = expand_home(&config.db_path);
            return cowork_session_close_command(
                cwd.clone(),
                session_id.clone(),
                *capture,
                *execute,
                format.clone(),
                db_path,
            );
        }
        Commands::CoworkHandoff {
            cwd,
            thread_id,
            format,
        } => {
            return cowork_handoff_command(cwd.clone(), thread_id.clone(), format.clone());
        }
        Commands::CoworkCapture {
            cwd,
            summary_source,
            execute,
            format,
        } => {
            let config = Config::load_from(&config_path)
                .with_context(|| format!("failed to load config {}", config_path.display()))?;
            let db_path = expand_home(&config.db_path);
            return cowork_capture_command(
                cwd.clone(),
                summary_source.clone(),
                *execute,
                format.clone(),
                db_path,
            );
        }
        Commands::MaintenanceRunbook { format } => {
            return maintenance_runbook_command(format.clone());
        }
        Commands::Maintenance {
            command: MaintenanceCommands::GuidedRun { format },
        } => {
            return maintenance_guided_run_command(format.clone());
        }
        Commands::ReleaseReadiness { format } => {
            return release_readiness_command(format.clone());
        }
        Commands::Doctor { format } => {
            return doctor_command(format.clone());
        }
        _ => {}
    }

    ConfigHandle::bootstrap(&config_path).context("failed to bootstrap config hot reload")?;
    let config = ConfigHandle::current();
    let db_path = expand_home(&config.db_path);
    let dashboard_mode = is_dashboard_command(&cli.command);
    if dashboard_mode && !db_path.exists() {
        bail!(
            "no palace.db found at {}; run `mempal init` first",
            display_path_for_user(&db_path)
        );
    }

    let db = match if dashboard_mode {
        open_dashboard_database(&db_path).context("failed to open dashboard database")
    } else {
        Database::open(&db_path).context("failed to open database")
    } {
        Ok(db) => db,
        Err(_error)
            if matches!(
                &cli.command,
                Commands::Gating {
                    command: GatingCommands::Stats { .. }
                }
            ) && !config_path.exists() =>
        {
            eprintln!(
                "warning: failed to open database {}; reporting empty gating stats",
                db_path.display()
            );
            let since = match &cli.command {
                Commands::Gating {
                    command: GatingCommands::Stats { since },
                } => since.as_deref(),
                _ => None,
            };
            observability::print_empty_gating_stats(since);
            return Ok(());
        }
        Err(error) => return Err(error),
    };

    match cli.command {
        Commands::Init { dir, dry_run } => init_command(&db, &dir, dry_run),
        Commands::Ingest {
            dir,
            stdin,
            wing,
            room,
            format,
            project,
            no_gate,
            dry_run,
            json,
            wait,
            wait_timeout_secs,
            no_strip_noise,
            diary_rollup,
            source_type,
            memory_kind,
            domain,
            field,
            is_pinned,
            confidence,
            supersedes,
            replace_text,
            valid_from,
            valid_until,
        } => block_on_result(ingest_command(
            &db,
            config.as_ref(),
            IngestCommandOptions {
                dir: dir.as_deref(),
                stdin,
                wing: wing.as_deref(),
                room: room.as_deref(),
                format,
                project: project.as_deref(),
                no_gate,
                dry_run,
                json,
                wait,
                wait_timeout_secs,
                no_strip_noise,
                diary_rollup,
                source_type: source_type.as_deref(),
                memory_kind: memory_kind.as_deref(),
                domain: domain.as_deref(),
                field: field.as_deref(),
                is_pinned,
                confidence,
                supersedes: supersedes.as_deref(),
                replace_text: replace_text.as_deref(),
                valid_from: valid_from.as_deref(),
                valid_until: valid_until.as_deref(),
            },
        )),
        Commands::Operation { command } => {
            block_on_result(operation_command(&db, config.as_ref(), command))
        }
        Commands::IngestConversation { .. } => {
            eprintln!(
                "error: `mempal ingest-conversation` was removed in P16.\n\
                 Use `mempal xurl ingest --tool cc --path <path>` instead.\n\
                 To scan all default directories: `mempal xurl ingest`"
            );
            std::process::exit(1);
        }
        Commands::Search {
            query,
            wing,
            room,
            memory_kind,
            domain,
            field,
            tier,
            status,
            anchor_kind,
            top_k,
            project,
            include_global,
            all_projects,
            json,
            with_neighbors,
            include_raw_turns,
            include_expired,
        } => block_on_result(search_command(
            &db,
            config.as_ref(),
            SearchCommandOptions {
                query: &query,
                wing: wing.as_deref(),
                room: room.as_deref(),
                filters: SearchFilters {
                    memory_kind,
                    domain,
                    field,
                    tier,
                    status,
                    anchor_kind,
                },
                top_k,
                project: project.as_deref(),
                include_global,
                all_projects,
                json,
                with_neighbors,
                include_raw_turns,
                include_expired,
            },
        )),
        Commands::Context {
            query,
            field,
            domain,
            cwd,
            format,
            include_evidence,
            include_cards,
            no_include_cards,
            max_items,
            dao_tian_limit,
            trigger,
            no_distill_suggestions,
        } => {
            let effective_include_cards = if no_include_cards {
                false
            } else if include_cards {
                true
            } else {
                config.context.include_cards_default
            };
            block_on_result(context_command(
                &db,
                config.as_ref(),
                ContextCommandArgs {
                    query,
                    field,
                    domain,
                    cwd,
                    format,
                    include_evidence,
                    include_cards: effective_include_cards,
                    max_items,
                    dao_tian_limit,
                    trigger,
                    no_distill_suggestions,
                },
            ))
        }
        Commands::Project { command } => project_command(&db, command),
        Commands::Config { .. } => unreachable!(),
        Commands::Delete { drawer_id } => delete_command(&db, &drawer_id),
        Commands::Pin { drawer_id } => pin_command(&db, &drawer_id),
        Commands::Unpin { drawer_id } => unpin_command(&db, &drawer_id),
        Commands::Pinned {
            project,
            reorder,
            json,
        } => pinned_command(&db, config.as_ref(), project.as_deref(), &reorder, json),
        Commands::Rollback {
            since,
            wing,
            room,
            project,
            dry_run,
            json,
        } => rollback_command(
            &db,
            config.as_ref(),
            RollbackCommandOptions {
                since: &since,
                wing: wing.as_deref(),
                room: room.as_deref(),
                project: project.as_deref(),
                dry_run,
                json,
            },
        ),
        Commands::Purge { before } => purge_command(&db, before.as_deref()),
        Commands::WakeUp { format } => wake_up_command(&db, format),
        Commands::Prime(_) => unreachable!(),
        Commands::Compress { text } => compress_command(&text),
        Commands::Bench { command } => block_on_result(bench_command(config.as_ref(), command)),
        Commands::Reindex {
            embedder,
            from_config,
            resume,
            stale,
            force,
            dry_run,
            batch_size,
            drawer_ids,
            project,
            wing,
            room,
            limit,
            max_batch_retries,
            failed,
            recompute_importance,
            only_zero,
            normalize_added_at,
        } => {
            let current_dir = env::current_dir().ok();
            let target_project_id = project
                .as_deref()
                .map(|project| {
                    resolve_project_id(Some(project), config.as_ref(), current_dir.as_deref())
                })
                .transpose()
                .context("failed to resolve reindex project filter")?
                .flatten();
            let vector_target = ReindexVectorTarget {
                drawer_ids,
                project_id: target_project_id,
                wing,
                room,
                limit,
            };
            if failed {
                if embedder.is_some()
                    || from_config
                    || resume
                    || stale
                    || force
                    || dry_run
                    || batch_size.is_some()
                    || max_batch_retries.is_some()
                    || !vector_target.is_empty()
                    || recompute_importance
                    || only_zero
                    || normalize_added_at
                {
                    bail!("--failed is mutually exclusive with other reindex modes and modifiers");
                }
                reindex_failed_queue_command(&db)
            } else if normalize_added_at {
                if recompute_importance
                    || embedder.is_some()
                    || from_config
                    || !vector_target.is_empty()
                {
                    bail!(
                        "--normalize-added-at is mutually exclusive with --embedder, --from-config, and --recompute-importance"
                    );
                }
                normalize_added_at_command(&db)
            } else if recompute_importance {
                if !vector_target.is_empty() {
                    bail!(
                        "--drawer-id/--project/--wing/--room/--limit require --embedder or --from-config with --stale"
                    );
                }
                recompute_importance_command(&db, only_zero)
            } else if embedder.is_some() || from_config {
                if dry_run && !stale {
                    bail!(
                        "--dry-run on embedder/vector reindex requires --stale; add --stale or use `mempal reindex --stale --dry-run`"
                    );
                }
                if !vector_target.is_empty() && !stale {
                    bail!(
                        "--drawer-id/--project/--wing/--room/--limit require --embedder or --from-config with --stale"
                    );
                }
                if max_batch_retries.is_some() && !stale {
                    bail!("--max-batch-retries requires --stale");
                }
                if !vector_target.is_empty() && resume {
                    bail!("--resume cannot be combined with targeted stale reindex filters");
                }
                let backend = match (embedder.as_deref(), from_config) {
                    (Some(name), false) => name.to_string(),
                    (None, true) => config.embed.backend.clone(),
                    (Some(_), true) => {
                        bail!("use either --embedder <name> or --from-config, not both")
                    }
                    (None, false) => unreachable!(),
                };
                block_on_result(reindex_command_by_embedder(
                    &db,
                    config.as_ref(),
                    &backend,
                    ReindexEmbedderMode {
                        resume,
                        stale_only: stale,
                        dry_run,
                    },
                    StaleBatchTuning {
                        batch_size: batch_size.unwrap_or(1000),
                        max_batch_retries: max_batch_retries.unwrap_or(DEFAULT_MAX_BATCH_RETRIES),
                    },
                    vector_target,
                ))
            } else {
                if batch_size.is_some() || max_batch_retries.is_some() || !vector_target.is_empty()
                {
                    bail!(
                        "--batch-size/--max-batch-retries/--drawer-id/--project/--wing/--room/--limit require --embedder or --from-config with --stale"
                    );
                }
                block_on_result(reindex_command_sources(
                    &db,
                    config.as_ref(),
                    stale,
                    force,
                    dry_run,
                ))
            }
        }
        Commands::Consolidate {
            wing,
            room,
            threshold,
            min_cluster,
            dry_run,
            strategy,
            limit,
        } => consolidate_command(
            &db,
            config.as_ref(),
            ConsolidateCommandOptions {
                wing: wing.as_deref(),
                room: room.as_deref(),
                threshold,
                min_cluster,
                dry_run,
                strategy: strategy.as_deref(),
                limit,
            },
        ),
        Commands::Crystallize {
            dry_run,
            project,
            json,
        } => block_on_result(crystallize_command(
            &db,
            config.as_ref(),
            CrystallizeCliOptions {
                dry_run,
                project,
                json,
            },
        )),
        Commands::Sleep {
            nrem,
            rem,
            salience,
            dry_run,
        } => sleep_command(
            &db,
            config.as_ref(),
            SleepCommandOptions {
                nrem,
                rem,
                salience,
                dry_run,
            },
        ),
        Commands::Maintenance {
            command:
                MaintenanceCommands::Rejudge {
                    execute,
                    hard_delete,
                    limit,
                    wing,
                    room,
                    project,
                    format,
                },
        } => block_on_result(maintenance_rejudge_command(
            &db,
            config.as_ref(),
            HistoricalRejudgeOptions {
                execute,
                hard_delete,
                limit,
                wing: wing.as_deref(),
                room: room.as_deref(),
                project: project.as_deref(),
                format: format.as_str(),
            },
        )),
        Commands::Kg { command } => kg_command(&db, command),
        Commands::Knowledge { command } => {
            block_on_result(knowledge_command(&db, config.as_ref(), command))
        }
        Commands::KnowledgeCard { command } => {
            block_on_result(knowledge_card_command(&db, config.as_ref(), command))
        }
        Commands::Cards {
            pending,
            approve,
            reject,
            format,
        } => cards_command(
            &db,
            CardsCommandOptions {
                pending,
                approve,
                reject,
                format,
            },
        ),
        Commands::Phase3 { command } => {
            block_on_result(phase3_command(&db, config.as_ref(), command))
        }
        Commands::Tunnels { command } => tunnels_command(&db, command),
        Commands::Taxonomy { command } => taxonomy_command(&db, command),
        Commands::FieldTaxonomy { format } => field_taxonomy_command(&format),
        Commands::Serve { mcp } => block_on_result(serve_command(config.as_ref(), mcp)),
        Commands::Status { full } => status_command(&db, config.as_ref(), full),
        Commands::Gating { command } => gating_command(&db, config.as_ref(), command),
        Commands::Tail {
            limit,
            follow,
            wing,
            room,
            since,
            raw,
        } => observability::tail_command(
            &db,
            config.as_ref(),
            observability::TailOptions {
                limit,
                follow,
                wing: wing.as_deref(),
                room: room.as_deref(),
                since: since.as_deref(),
                raw,
            },
        ),
        Commands::Timeline {
            wing,
            since,
            format,
            raw,
        } => observability::timeline_command(
            &db,
            config.as_ref(),
            observability::TimelineOptions {
                wing: wing.as_deref(),
                since: since.as_deref(),
                format: &format,
                raw,
            },
        ),
        Commands::Stats { raw } => {
            observability::stats_command(&db, config.as_ref(), observability::StatsOptions { raw })
        }
        Commands::View {
            drawer_id,
            raw,
            project,
            include_global,
            all_projects,
        } => observability::view_command(
            &db,
            config.as_ref(),
            observability::ViewOptions {
                drawer_id: &drawer_id,
                raw,
                project: project.as_deref(),
                include_global,
                all_projects,
            },
        ),
        Commands::Audit {
            command,
            stale,
            threshold,
        } => {
            if stale {
                audit_stale_command(&db, threshold)
            } else if let Some(cmd) = command {
                match cmd {
                    AuditCommands::Gating {
                        decision,
                        llm_verdict,
                        since,
                        project,
                        format,
                        raw,
                    } => observability::audit_gating_command(
                        &db,
                        config.as_ref(),
                        observability::GatingAuditOptions {
                            decision_filter: decision,
                            llm_verdict_filter: llm_verdict,
                            since: since.as_deref(),
                            project_filter: project.as_deref(),
                            format: &format,
                            raw,
                        },
                    ),
                    AuditCommands::Embed { since, format, raw } => {
                        observability::audit_embed_command(
                            &db,
                            config.as_ref(),
                            since.as_deref(),
                            &format,
                            raw,
                        )
                    }
                    AuditCommands::Novelty { since, format, raw } => {
                        observability::audit_novelty_command(
                            &db,
                            config.as_ref(),
                            since.as_deref(),
                            &format,
                            raw,
                        )
                    }
                    AuditCommands::Cleanup {
                        dry_run,
                        score_threshold,
                        wing,
                    } => observability::audit_cleanup_command(
                        &db,
                        config.as_ref(),
                        observability::AuditCleanupOptions {
                            dry_run,
                            score_threshold: score_threshold.unwrap_or(0.55),
                            wing_filter: wing.as_deref().unwrap_or("hooks-raw"),
                        },
                    ),
                }
            } else {
                bail!("`mempal audit` requires a subcommand (gating, embed, novelty) or --stale");
            }
        }
        Commands::RecomputeImportance => recompute_effective_importance_command(&db),
        Commands::FactCheck {
            path,
            wing,
            room,
            now,
        } => fact_check_command(&db, path.as_deref(), wing.as_deref(), room.as_deref(), now),
        Commands::Checkpoint { command } => {
            block_on_result(checkpoint_command(&db, config.as_ref(), command))
        }
        Commands::Patterns { command } => patterns::run_command(config.as_ref(), command),
        Commands::Skills { command } => skills::run_command(config.as_ref(), command),
        Commands::Repair { command } => repair_cli::run_command(config.as_ref(), command),
        Commands::Xurl { command } => {
            block_on_result(xurl_ingest_command(&db, config.as_ref(), command))
        }
        Commands::Brief { query, format } => {
            block_on_result(brief_command(&db, config.as_ref(), query, format))
        }
        Commands::CoworkDrain { .. }
        | Commands::CoworkStatus { .. }
        | Commands::CoworkInstallHooks { .. }
        | Commands::Integrations { .. }
        | Commands::Hook { .. }
        | Commands::Hotpatch { .. }
        | Commands::Daemon { .. }
        | Commands::CoworkRegister { .. }
        | Commands::CoworkHeartbeat { .. }
        | Commands::CoworkAgents { .. }
        | Commands::CoworkSend { .. }
        | Commands::CoworkAgentDrain { .. }
        | Commands::CoworkDeliveries { .. }
        | Commands::CoworkAck { .. }
        | Commands::CoworkEvents { .. }
        | Commands::CoworkChannelSet { .. }
        | Commands::CoworkChannelSend { .. }
        | Commands::CoworkBroadcast { .. }
        | Commands::CoworkRunbook { .. }
        | Commands::CoworkDoctor { .. }
        | Commands::CoworkTmuxPeek { .. }
        | Commands::CoworkSessionCreate { .. }
        | Commands::CoworkSessions { .. }
        | Commands::CoworkSessionStatus { .. }
        | Commands::CoworkSessionClose { .. }
        | Commands::CoworkHandoff { .. }
        | Commands::CoworkCapture { .. }
        | Commands::MaintenanceRunbook { .. }
        | Commands::Maintenance { .. }
        | Commands::ReleaseReadiness { .. }
        | Commands::Doctor { .. } => unreachable!(),
    }
}

fn is_dashboard_command(command: &Commands) -> bool {
    match command {
        Commands::Status { .. }
        | Commands::Tail { .. }
        | Commands::Timeline { .. }
        | Commands::Stats { .. }
        | Commands::View { .. } => true,
        Commands::Audit { command, .. } => {
            !matches!(command, Some(AuditCommands::Cleanup { dry_run: false, .. }))
        }
        _ => false,
    }
}

fn open_dashboard_database(path: &Path) -> Result<Database> {
    let db = Database::open_read_only(path)?;
    db.conn()
        .execute_batch("PRAGMA query_only = ON;")
        .context("failed to enable query_only for dashboard connection")?;
    Ok(db)
}

fn display_path_for_user(path: &Path) -> String {
    if let Some(home) = env::var_os("HOME").map(PathBuf::from)
        && let Ok(stripped) = path.strip_prefix(&home)
    {
        if stripped.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", stripped.display());
    }
    path.display().to_string()
}

fn block_on_result<F, T>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let runtime = tokio::runtime::Runtime::new().context("failed to construct tokio runtime")?;
    let result = runtime.block_on(future);
    if result.as_ref().err().is_some_and(is_ingest_wait_timed_out) {
        runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    }
    result
}

async fn bench_command(config: &Config, command: BenchCommands) -> Result<()> {
    match command {
        BenchCommands::LongMemEval {
            data_file,
            mode,
            granularity,
            limit,
            skip,
            top_k,
            out,
        } => {
            longmemeval::run_longmemeval_command(
                config,
                LongMemEvalArgs {
                    data_file,
                    mode,
                    granularity,
                    limit,
                    skip,
                    top_k,
                    out,
                },
            )
            .await
        }
    }
}

fn prime_command(config_path: &Path, args: PrimeArgs) -> Result<()> {
    let config = Config::load_from(config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    let db_path = expand_home(&config.db_path);
    if !db_path.exists() {
        eprintln!("mempal: palace.db not found; skipping priming");
        return Ok(());
    }
    let db = open_dashboard_database(&db_path).context("failed to open priming database")?;
    let current_dir = env::current_dir().ok();
    let project_id =
        resolve_project_id(args.project_id.as_deref(), &config, current_dir.as_deref())
            .context("failed to resolve prime project id")?;
    let scope = ProjectSearchScope::from_request(
        project_id.clone(),
        false,
        false,
        config.search.strict_project_isolation,
    );
    let include_stats = args.want_stats();
    let report = mempal::core::priming::build_priming_report(
        &db,
        PrimingRequest {
            project_id,
            scope,
            since: args.since,
            token_budget: args.token_budget,
            include_stats,
            embedder_degraded: prime_embedder_degraded(),
        },
    )
    .context("failed to build priming output")?;
    if report.drawers.is_empty() {
        return Ok(());
    }
    match args.format {
        PrimeFormat::Text => println!("{}", prime_cli::render_text(&report)),
        PrimeFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&report).context("failed to serialize prime JSON")?
        ),
    }
    Ok(())
}

fn init_command(db: &Database, dir: &Path, dry_run: bool) -> Result<()> {
    let wing = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("default")
        .to_string();
    let rooms = detect_rooms(dir)?;
    if !dry_run {
        for room in &rooms {
            let keywords = serde_json::to_string(&vec![room.clone()])
                .context("failed to serialize taxonomy keywords")?;
            db.conn().execute("INSERT OR IGNORE INTO taxonomy (wing, room, display_name, keywords) VALUES (?1, ?2, ?3, ?4)", (&wing, room, room, keywords.as_str())).with_context(|| format!("failed to insert taxonomy room {room}"))?;
        }
    }
    println!("dry_run={dry_run}");
    println!("wing: {wing}");
    if rooms.is_empty() {
        println!("rooms: none detected");
    } else {
        println!("rooms:");
        for room in rooms {
            println!("- {room}");
        }
    }
    Ok(())
}

async fn ingest_command(
    db: &Database,
    config: &Config,
    options: IngestCommandOptions<'_>,
) -> Result<()> {
    match (options.stdin, options.dir) {
        (true, Some(path)) => {
            bail!(
                "`mempal ingest --stdin` cannot be combined with directory path `{}`",
                path.display()
            );
        }
        (true, None) => {
            return ingest_stdin_command(db, config, options).await;
        }
        (false, None) => {
            bail!("`mempal ingest` requires a directory path unless --stdin is used");
        }
        (false, Some(_)) => {}
    }
    if options.supersedes.is_some() || options.replace_text.is_some() {
        bail!("--supersedes and --replace-text are only supported with --stdin ingest");
    }

    let path = options
        .dir
        .expect("validated that non-stdin ingest has a directory path");
    let wing = options
        .wing
        .context("`mempal ingest` requires --wing for directory ingest")?;
    if !path.exists() {
        bail!("path `{}` does not exist", path.display());
    }
    if path.is_file() && !options.dry_run {
        bail!(
            "`mempal ingest` expects a DIRECTORY, got file `{}`. To ingest a single file, create a temporary directory first, e.g. `mkdir -p /path/to/dir && cp {} /path/to/dir/ && mempal ingest /path/to/dir --wing {}`",
            path.display(),
            path.display(),
            wing
        );
    }
    if let Some(format) = options.format.as_deref()
        && format != "convos"
    {
        bail!("unsupported --format value: {format}");
    }

    let project_id = resolve_project_id(options.project, config, Some(path))
        .context("failed to resolve ingest project id")?;
    let valid_from = validate_temporal_bound("--valid-from", options.valid_from)?;
    let valid_until = validate_temporal_bound("--valid-until", options.valid_until)?;
    let source_type = parse_source_type_bound("--source-type", options.source_type)?
        .unwrap_or(SourceType::AgentInference);
    let memory_kind = parse_memory_kind_bound("--memory-kind", options.memory_kind)?;
    let domain = options
        .domain
        .map(parse_domain)
        .transpose()
        .context("failed to parse --domain")?;
    let confidence = resolve_confidence_bound("--confidence", source_type, options.confidence)?;
    let base_options = IngestOptions {
        room: options.room,
        source_root: if path.is_file() { None } else { Some(path) },
        dry_run: options.dry_run,
        project_id: project_id.as_deref(),
        gating: None,
        prototype_classifier: None,
        source_file_override: None,
        source_type: Some(source_type),
        memory_kind,
        domain,
        field: options.field,
        is_pinned: options.is_pinned,
        confidence: Some(confidence),
        replace_existing_source: false,
        replace_across_rooms: false,
        no_strip_noise: options.no_strip_noise,
        diary_rollup: options.diary_rollup,
        diary_rollup_day: None,
        supersedes: options.supersedes,
        replace_text: options.replace_text,
        valid_from,
        valid_until,
    };

    let stats = if options.dry_run {
        ingest_path_with_options(db, &NoopEmbedder, path, wing, base_options).await?
    } else {
        let prototype_classifier = if config.ingest_gating.enabled && !options.no_gate {
            compile_classifier_from_config(config)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))
                .context("gating prototypes unavailable")?
        } else {
            None
        };
        let embedder = build_embedder(config).await?;
        let live_options = IngestOptions {
            room: options.room,
            source_root: if path.is_file() { None } else { Some(path) },
            dry_run: false,
            project_id: project_id.as_deref(),
            gating: (!options.no_gate).then_some(&config.ingest_gating),
            prototype_classifier: prototype_classifier.as_ref(),
            source_file_override: None,
            source_type: Some(source_type),
            memory_kind,
            domain,
            field: options.field,
            is_pinned: options.is_pinned,
            confidence: Some(confidence),
            replace_existing_source: false,
            replace_across_rooms: false,
            no_strip_noise: options.no_strip_noise,
            diary_rollup: options.diary_rollup,
            diary_rollup_day: None,
            supersedes: options.supersedes,
            replace_text: options.replace_text,
            valid_from,
            valid_until,
        };
        ingest_path_with_options(db, &*embedder, path, wing, live_options).await?
    };

    append_ingest_audit_log(
        db,
        path,
        wing,
        options.format.as_deref(),
        options.dry_run,
        &stats,
    )
    .context("failed to append ingest audit log")?;

    print_fact_check_warnings(&stats.fact_check_warnings);
    if options.json {
        let output = IngestJsonOutput {
            dry_run: options.dry_run,
            files: stats.files,
            chunks: stats.chunks,
            skipped: stats.skipped,
            dropped_by_gate: stats.dropped_by_gate,
            drawer_ids: &stats.drawer_ids,
            superseded_drawer_id: stats.superseded_drawer_id.clone(),
            fact_check_warnings: stats.fact_check_warnings.clone(),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&output)
                .context("failed to serialize ingest JSON output")?
        );
        return Ok(());
    }

    println!(
        "dry_run={} files={} chunks={} skipped={} dropped_by_gate={} noise_bytes_stripped={} lock_wait_ms={} superseded_drawer_id={}",
        options.dry_run,
        stats.files,
        stats.chunks,
        stats.skipped,
        stats.dropped_by_gate,
        stats.noise_bytes_stripped.unwrap_or(0),
        stats.lock_wait_ms.unwrap_or(0),
        stats.superseded_drawer_id.as_deref().unwrap_or("")
    );
    Ok(())
}

#[derive(Debug, Deserialize)]
struct StdinIngestRecord {
    content: Option<String>,
    wing: Option<String>,
    room: Option<String>,
    project: Option<String>,
    source: Option<String>,
    source_file: Option<String>,
    source_type: Option<String>,
    memory_kind: Option<String>,
    domain: Option<String>,
    field: Option<String>,
    is_pinned: Option<bool>,
    confidence: Option<f64>,
    supersedes: Option<String>,
    replace_text: Option<String>,
    valid_from: Option<String>,
    valid_until: Option<String>,
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

const MAX_STDIN_INGEST_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug)]
struct ResolvedStdinIngest {
    content: String,
    wing: String,
    room: Option<String>,
    source: Option<String>,
    source_file: Option<String>,
    project_id: Option<String>,
    source_type: SourceType,
    confidence: f64,
    supersedes: Option<String>,
    replace_text: Option<String>,
    valid_from: Option<String>,
    valid_until: Option<String>,
    memory_kind: MemoryKind,
    domain: MemoryDomain,
    field: String,
    is_pinned: bool,
    no_gate: bool,
    raw_turn: bool,
    drawer_importance: i32,
    cwd: Option<PathBuf>,
    bypass_novelty: bool,
}

impl ResolvedStdinIngest {
    fn source_hint(&self) -> Option<&str> {
        self.source_file.as_deref().or(self.source.as_deref())
    }

    fn source_file_for_drawer(drawer_id: &str, source_hint: Option<&str>) -> String {
        source_file_or_synthetic(drawer_id, source_hint)
    }

    fn wait_request(&self, wait_timeout_secs: u64) -> IngestRequest {
        IngestRequest {
            content: self.content.clone(),
            wing: self.wing.clone(),
            room: self.room.clone(),
            source: self.source.clone(),
            source_file: self.source_file.clone(),
            source_type: Some(self.source_type.to_string()),
            confidence: Some(self.confidence),
            project_id: self.project_id.clone(),
            supersedes: self.supersedes.clone(),
            replace_text: self.replace_text.clone(),
            valid_from: self.valid_from.clone(),
            valid_until: self.valid_until.clone(),
            dry_run: Some(false),
            wait: Some(true),
            wait_timeout_secs: Some(wait_timeout_secs),
            diary_rollup: Some(false),
            importance: Some(self.drawer_importance),
            memory_kind: Some(memory_kind_slug(&self.memory_kind).to_string()),
            domain: Some(domain_slug(&self.domain).to_string()),
            field: Some(self.field.clone()),
            is_pinned: Some(self.is_pinned),
            provenance: None,
            statement: None,
            tier: None,
            status: None,
            supporting_refs: None,
            counterexample_refs: None,
            teaching_refs: None,
            verification_refs: None,
            scope_constraints: None,
            trigger_hints: None,
            anchor_kind: None,
            anchor_id: None,
            parent_anchor_id: None,
            cwd: self.cwd.as_ref().map(|path| path.display().to_string()),
        }
    }
}

#[derive(Serialize)]
struct StdinIngestJsonOutput<'a> {
    drawer_ids: &'a [String],
    stats: StdinIngestStatsJson,
}

#[derive(Serialize)]
struct StdinIngestStatsJson {
    dry_run: bool,
    files: usize,
    chunks: usize,
    skipped: usize,
    dropped_by_gate: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fact_check_warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_drawer_id: Option<String>,
}

fn exact_duplicate_drawer_id(
    db: &Database,
    content: &str,
    wing: &str,
    room: Option<&str>,
    project_id: Option<&str>,
    excluded_drawer_id: Option<&str>,
) -> Result<Option<String>> {
    Ok(db
        .find_active_drawers_by_content(content, wing, room, project_id)?
        .into_iter()
        .find(|summary| Some(summary.id.as_str()) != excluded_drawer_id)
        .map(|summary| summary.id))
}

struct ExactDuplicateStdinIngest<'a> {
    db: &'a Database,
    wing: &'a str,
    json: bool,
    record: &'a StdinIngestRecord,
    stats: &'a mut IngestStats,
    drawer_id: &'a str,
    is_pinned: bool,
    superseded_drawer_id: Option<&'a str>,
}

fn finalize_exact_duplicate_stdin_ingest(ctx: ExactDuplicateStdinIngest<'_>) -> Result<()> {
    let ExactDuplicateStdinIngest {
        db,
        wing,
        json,
        record,
        stats,
        drawer_id,
        is_pinned,
        superseded_drawer_id,
    } = ctx;
    if is_pinned {
        db.pin_drawer(drawer_id, None)
            .with_context(|| format!("failed to pin duplicate drawer {drawer_id}"))?;
    }
    stats.skipped = 1;
    stats.drawer_ids = vec![drawer_id.to_string()];
    if let Some(old_id) = superseded_drawer_id {
        db.supersede_drawer(old_id, &format!("replaced by {drawer_id}"))
            .with_context(|| format!("failed to supersede drawer {old_id}"))?;
        stats.superseded_drawer_id = Some(old_id.to_string());
    }
    append_ingest_stdin_audit_log(db, wing, false, record, stats)
        .context("failed to append ingest audit log")?;
    print_stdin_ingest_output(json, false, stats)?;
    Ok(())
}

fn resolve_stdin_ingest(
    config: &Config,
    options: &IngestCommandOptions<'_>,
    record: &StdinIngestRecord,
    content: String,
    wing: String,
    room: Option<String>,
) -> Result<ResolvedStdinIngest> {
    let source = record.source.clone();
    let source_file = record.source_file.clone();
    let project = options.project.or(record.project.as_deref());
    let supersedes = options
        .supersedes
        .or(record.supersedes.as_deref())
        .map(ToOwned::to_owned);
    let replace_text = options
        .replace_text
        .or(record.replace_text.as_deref())
        .map(ToOwned::to_owned);
    let source_type = parse_source_type_bound(
        "source_type",
        options.source_type.or(record.source_type.as_deref()),
    )?
    .unwrap_or(SourceType::AgentInference);
    let memory_kind = parse_memory_kind_bound(
        "memory_kind",
        options.memory_kind.or(record.memory_kind.as_deref()),
    )?
    .unwrap_or(MemoryKind::Evidence);
    let domain = options
        .domain
        .or(record.domain.as_deref())
        .map(parse_domain)
        .transpose()
        .context("failed to parse stdin ingest domain")?
        .unwrap_or(MemoryDomain::Project);
    let field = options
        .field
        .or(record.field.as_deref())
        .unwrap_or("general")
        .to_string();
    let is_pinned = options.is_pinned || record.is_pinned.unwrap_or(false);
    let confidence = resolve_confidence_bound(
        "confidence",
        source_type,
        options.confidence.or(record.confidence),
    )?;
    let valid_from = validate_temporal_bound(
        "valid_from",
        options.valid_from.or(record.valid_from.as_deref()),
    )?
    .map(ToOwned::to_owned);
    let valid_until = validate_temporal_bound(
        "valid_until",
        options.valid_until.or(record.valid_until.as_deref()),
    )?
    .map(ToOwned::to_owned);
    let cwd = env::current_dir().ok();
    let project_id = resolve_project_id(project, config, cwd.as_deref())
        .context("failed to resolve stdin ingest project id")?;
    let raw_turn = is_raw_turn(&wing, room.as_deref(), &config.turns);
    let drawer_importance = raw_turn_importance(&wing, room.as_deref(), &config.turns).unwrap_or(0);

    Ok(ResolvedStdinIngest {
        content,
        wing,
        room,
        source,
        source_file,
        project_id,
        source_type,
        confidence,
        supersedes,
        replace_text,
        valid_from,
        valid_until,
        memory_kind,
        domain,
        field,
        is_pinned,
        no_gate: options.no_gate,
        raw_turn,
        drawer_importance,
        cwd,
        bypass_novelty: true,
    })
}

fn validate_temporal_bound<'a>(field: &str, value: Option<&'a str>) -> Result<Option<&'a str>> {
    if let Some(raw) = value {
        if mempal::core::decay::parse_temporal_timestamp_secs(raw).is_none() {
            bail!("{field} must be a Unix timestamp or RFC3339 timestamp");
        }
    }
    Ok(value)
}

fn parse_source_type_bound(field: &str, value: Option<&str>) -> Result<Option<SourceType>> {
    value
        .map(|raw| {
            raw.parse::<SourceType>()
                .with_context(|| format!("{field} must be one of user_explicit, agent_observation, agent_inference, system_generated"))
        })
        .transpose()
}

fn parse_memory_kind_bound(field: &str, value: Option<&str>) -> Result<Option<MemoryKind>> {
    value
        .map(|raw| match raw {
            "evidence" => Ok(MemoryKind::Evidence),
            "knowledge" => Ok(MemoryKind::Knowledge),
            "profile_fact" => Ok(MemoryKind::ProfileFact),
            other => bail!("{field} must be one of evidence, knowledge, profile_fact; got {other}"),
        })
        .transpose()
}

fn resolve_confidence_bound(
    field: &str,
    source_type: SourceType,
    value: Option<f64>,
) -> Result<f64> {
    match value {
        Some(confidence) if confidence.is_finite() && (0.0..=1.0).contains(&confidence) => {
            Ok(confidence)
        }
        Some(confidence) => {
            bail!("{field} must be a finite float between 0.0 and 1.0, got {confidence}")
        }
        None => Ok(default_confidence(source_type)),
    }
}

async fn ingest_stdin_command(
    db: &Database,
    config: &Config,
    options: IngestCommandOptions<'_>,
) -> Result<()> {
    if options.format.is_some() {
        bail!("--format is only supported for directory ingest");
    }
    if options.diary_rollup {
        bail!("--diary-rollup is only supported for directory ingest");
    }

    let mut input = Vec::new();
    std::io::stdin()
        .take(MAX_STDIN_INGEST_BYTES as u64 + 1)
        .read_to_end(&mut input)
        .context("failed to read stdin")?;
    if input.len() > MAX_STDIN_INGEST_BYTES {
        bail!(
            "stdin payload exceeds {} byte limit",
            MAX_STDIN_INGEST_BYTES
        );
    }
    let input = String::from_utf8(input).context("stdin payload is not valid UTF-8")?;
    let record: StdinIngestRecord =
        serde_json::from_str(&input).context("failed to parse stdin JSON object")?;

    let raw_content = record
        .content
        .as_deref()
        .context("stdin JSON object is missing required `content` field")?
        .to_string();
    if raw_content.trim().is_empty() {
        bail!("stdin JSON `content` field must not be empty");
    }
    let content = normalize_stdin_content(&raw_content, options.no_strip_noise)?;

    let wing = options
        .wing
        .or(record.wing.as_deref())
        .context("stdin ingest requires --wing or JSON `wing`")?
        .to_string();
    let room = options
        .room
        .or(record.room.as_deref())
        .map(ToOwned::to_owned);
    let resolved = resolve_stdin_ingest(config, &options, &record, content, wing, room)?;
    let mut stats = IngestStats {
        files: 1,
        ..IngestStats::default()
    };
    if resolved.raw_turn && !should_store_raw_turns(&config.turns.storage_mode) {
        stats.skipped = 1;
        print_stdin_ingest_output(options.json, options.dry_run, &stats)?;
        return Ok(());
    }
    let (privacy_config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
    let scrubbed_replace_text = resolved
        .replace_text
        .as_deref()
        .map(|text| privacy_config.scrub_content_with_compiled(text, compiled_privacy.as_ref()));

    let replacement_target = db
        .resolve_replacement_target(
            resolved.supersedes.as_deref(),
            scrubbed_replace_text.as_deref(),
            &resolved.wing,
            resolved.room.as_deref(),
            resolved.project_id.as_deref(),
        )
        .context("failed to resolve replacement target")?;
    let superseded_drawer_id = replacement_target
        .as_ref()
        .map(|summary| summary.id.clone());
    let superseded_drawer_id_ref = superseded_drawer_id.as_deref();
    let exact_duplicate = exact_duplicate_drawer_id(
        db,
        &resolved.content,
        &resolved.wing,
        resolved.room.as_deref(),
        resolved.project_id.as_deref(),
        superseded_drawer_id_ref,
    )
    .context("failed to check exact duplicate")?;
    let drawer_id = if let Some(existing_id) = exact_duplicate.as_ref() {
        existing_id.clone()
    } else {
        let preferred_id = build_bootstrap_evidence_drawer_id(
            &resolved.wing,
            resolved.room.as_deref(),
            &resolved.content,
            &resolved.source_type,
        );
        db.resolve_available_drawer_id(&preferred_id)
            .with_context(|| format!("failed to resolve drawer id for {preferred_id}"))?
    };
    if options.dry_run {
        stats.chunks = 1;
        stats.drawer_ids.push(drawer_id.clone());
        append_ingest_stdin_audit_log(db, &resolved.wing, options.dry_run, &record, &stats)
            .context("failed to append ingest audit log")?;
        print_stdin_ingest_output(options.json, options.dry_run, &stats)?;
        return Ok(());
    }

    // Exact duplicates are a local no-op. Keep `--wait` on the same
    // terminal stats/audit path as the direct stdin ingest instead of
    // queueing a receipt that would report different skip semantics.
    if options.wait && exact_duplicate.is_some() {
        finalize_exact_duplicate_stdin_ingest(ExactDuplicateStdinIngest {
            db,
            wing: &resolved.wing,
            json: options.json,
            record: &record,
            stats: &mut stats,
            drawer_id: &drawer_id,
            is_pinned: resolved.is_pinned,
            superseded_drawer_id: superseded_drawer_id.as_deref(),
        })?;
        return Ok(());
    }

    if options.wait {
        let wait_request = resolved.wait_request(options.wait_timeout_secs.unwrap_or(30));
        let controls = IngestControls {
            no_gate: resolved.no_gate,
            bypass_novelty: resolved.bypass_novelty,
        };
        let server = MempalMcpServer::new(db.path().to_path_buf(), config.clone())?;
        let response = server
            .mempal_ingest_with_controls(wait_request, controls)
            .await
            .map(|response| response.0)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        if response.timed_out {
            print_ingest_wait_receipt_response(options.json, &response)?;
            std::io::stdout()
                .flush()
                .context("failed to flush timed-out ingest receipt")?;
            return Err(IngestWaitTimedOut::new(response.operation_id.as_deref()).into());
        }

        let mut wait_stats = ingest_stdin_wait_stats_from_response(&response);
        match response.state {
            Some(IngestOperationState::Completed) => {
                append_ingest_stdin_audit_log(db, &resolved.wing, false, &record, &wait_stats)
                    .context("failed to append ingest audit log")?;
                print_stdin_ingest_output(options.json, false, &wait_stats)?;
                return Ok(());
            }
            Some(IngestOperationState::Rejected) => {
                wait_stats.drawer_ids.clear();
                append_ingest_stdin_audit_log(db, &resolved.wing, false, &record, &wait_stats)
                    .context("failed to append ingest audit log")?;
                print_stdin_ingest_output(options.json, false, &wait_stats)?;
                return Ok(());
            }
            Some(IngestOperationState::Failed) => {
                print_stdin_ingest_output(options.json, false, &wait_stats)?;
                bail!(
                    "ingest operation {} failed: {}",
                    response.operation_id.as_deref().unwrap_or(""),
                    response.failure_detail.as_deref().unwrap_or("failed")
                );
            }
            _ => {
                print_ingest_wait_receipt_response(options.json, &response)?;
                return Ok(());
            }
        }
    }

    if exact_duplicate.is_some() {
        finalize_exact_duplicate_stdin_ingest(ExactDuplicateStdinIngest {
            db,
            wing: &resolved.wing,
            json: options.json,
            record: &record,
            stats: &mut stats,
            drawer_id: &drawer_id,
            is_pinned: resolved.is_pinned,
            superseded_drawer_id: superseded_drawer_id.as_deref(),
        })?;
        return Ok(());
    }

    let prototype_classifier = if config.ingest_gating.enabled && !resolved.no_gate {
        compile_classifier_from_config(config)
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))
            .context("gating prototypes unavailable")?
    } else {
        None
    };
    let embedder = build_embedder(config).await?;

    if !resolved.no_gate {
        let candidate = IngestCandidate {
            content: resolved.content.clone(),
            event: None,
            tool_name: None,
            exit_code: None,
        };
        let mut gating_decision = evaluate_tier1(&candidate, &config.ingest_gating);
        if gating_decision.is_none()
            && let Some(classifier) = prototype_classifier.as_ref()
        {
            let tier2 = evaluate_tier2(
                &candidate,
                classifier,
                embedder.as_ref(),
                config.ingest_gating.embedding_classifier.threshold,
            )
            .await;
            gating_decision = Some(tier2.decision);
        }
        if let Some(decision) = gating_decision.as_ref() {
            db.record_gating_audit(
                &drawer_id,
                decision,
                resolved.project_id.as_deref(),
                Some(&resolved.content),
            )
            .with_context(|| format!("failed to record gating audit for {drawer_id}"))?;
            if decision.is_rejected() {
                stats.dropped_by_gate = 1;
                stats.drawer_ids.clear();
                append_ingest_stdin_audit_log(db, &resolved.wing, options.dry_run, &record, &stats)
                    .context("failed to append ingest audit log")?;
                print_stdin_ingest_output(options.json, options.dry_run, &stats)?;
                return Ok(());
            }
        }

        if !resolved.raw_turn
            && let Some(outcome) = evaluate_fact_check_gate(
                &drawer_id,
                &resolved.content,
                db,
                resolved.project_id.as_deref(),
                &config.ingest_gating.fact_check,
                resolved.confidence,
            )
            .with_context(|| format!("failed to record fact-check gate audit for {drawer_id}"))?
        {
            stats.fact_check_warnings.extend(outcome.warnings);
            if outcome.decision.is_rejected() {
                stats.dropped_by_gate = 1;
                stats.drawer_ids.clear();
                append_ingest_stdin_audit_log(db, &resolved.wing, options.dry_run, &record, &stats)
                    .context("failed to append ingest audit log")?;
                print_stdin_ingest_output(options.json, options.dry_run, &stats)?;
                return Ok(());
            }
        }
    }

    let texts = [resolved.content.as_str()];
    let vectors = embedder
        .embed(&texts)
        .await
        .context("failed to embed stdin content")?;
    let vector = vectors
        .into_iter()
        .next()
        .context("embedder returned no vector for stdin content")?;
    let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
    let scrub = |s: &str| config.scrub_content_with_compiled(s, compiled_privacy.as_ref());
    let source_hint = resolved.source_hint().map(&scrub);
    let source_file =
        ResolvedStdinIngest::source_file_for_drawer(&drawer_id, source_hint.as_deref());

    let drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: drawer_id.clone(),
        content: resolved.content,
        wing: resolved.wing.clone(),
        room: resolved.room.clone(),
        source_file: Some(source_file),
        source_type: resolved.source_type,
        added_at: iso_timestamp(),
        chunk_index: Some(0),
        importance: resolved.drawer_importance,
    });
    let drawer = Drawer {
        confidence: resolved.confidence,
        normalize_version: CURRENT_NORMALIZE_VERSION,
        ..drawer
    };
    let mut drawer = drawer;
    drawer.memory_kind = resolved.memory_kind;
    drawer.domain = resolved.domain;
    drawer.field = resolved.field;
    drawer.is_pinned = resolved.is_pinned;
    if let Some(old_id) = superseded_drawer_id.as_deref() {
        link_superseded_drawer(&mut drawer, old_id);
    }

    db.insert_drawer_with_project_validity(
        &drawer,
        resolved.project_id.as_deref(),
        None,
        resolved.valid_from.as_deref(),
        resolved.valid_until.as_deref(),
    )
    .with_context(|| format!("failed to insert drawer {}", drawer.id))?;
    db.insert_vector_with_project(&drawer_id, &vector, resolved.project_id.as_deref())
        .with_context(|| format!("failed to insert vector for drawer {drawer_id}"))?;

    if let Some(old_id) = superseded_drawer_id.as_deref() {
        db.supersede_drawer(old_id, &format!("replaced by {drawer_id}"))
            .with_context(|| format!("failed to supersede drawer {old_id}"))?;
        stats.superseded_drawer_id = Some(old_id.to_string());
    }

    // Failure detection (P14) — synchronous, lightweight DB write.
    {
        let config_snap = ConfigHandle::current();
        if config_snap.repair.enabled {
            mempal::repair::try_record_failure(
                db.path(),
                &drawer_id,
                &drawer.content,
                &resolved.wing,
                resolved.room.as_deref(),
                resolved.project_id.as_deref(),
                &config_snap.repair,
            );
        }
    }

    stats.chunks = 1;
    stats.drawer_ids.push(drawer_id);
    append_ingest_stdin_audit_log(db, &resolved.wing, options.dry_run, &record, &stats)
        .context("failed to append ingest audit log")?;
    print_stdin_ingest_output(options.json, options.dry_run, &stats)?;
    Ok(())
}

fn normalize_stdin_content(content: &str, no_strip_noise: bool) -> Result<String> {
    let format = detect_format(content);
    let normalize_output = normalize_content_with_options(
        content,
        format,
        NormalizeOptions {
            strip_noise: !no_strip_noise,
        },
    )
    .context("failed to normalize stdin content")?;
    let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
    let scrubbed =
        config.scrub_content_with_compiled(&normalize_output.content, compiled_privacy.as_ref());
    if scrubbed.trim().is_empty() {
        bail!("stdin JSON `content` field must not be empty after normalization");
    }
    Ok(scrubbed)
}

fn print_stdin_ingest_output(json: bool, dry_run: bool, stats: &IngestStats) -> Result<()> {
    print_fact_check_warnings(&stats.fact_check_warnings);
    if json {
        let output = StdinIngestJsonOutput {
            drawer_ids: &stats.drawer_ids,
            stats: StdinIngestStatsJson {
                dry_run,
                files: stats.files,
                chunks: stats.chunks,
                skipped: stats.skipped,
                dropped_by_gate: stats.dropped_by_gate,
                fact_check_warnings: stats.fact_check_warnings.clone(),
                superseded_drawer_id: stats.superseded_drawer_id.clone(),
            },
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&output)
                .context("failed to serialize stdin ingest JSON output")?
        );
        return Ok(());
    }

    println!(
        "dry_run={} files={} chunks={} skipped={} dropped_by_gate={} superseded_drawer_id={}",
        dry_run,
        stats.files,
        stats.chunks,
        stats.skipped,
        stats.dropped_by_gate,
        stats.superseded_drawer_id.as_deref().unwrap_or("")
    );
    Ok(())
}

fn print_fact_check_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("{warning}");
    }
}

fn print_operation_response(response: &IngestResponse) -> Result<()> {
    println!(
        "operation_id={}",
        response.operation_id.as_deref().unwrap_or("")
    );
    println!(
        "state={}",
        response.state.map(|state| state.as_str()).unwrap_or("")
    );
    println!("timed_out={}", response.timed_out);
    println!("drawer_id={}", response.drawer_id);
    if !response.drawer_ids.is_empty() {
        println!("drawer_ids={}", response.drawer_ids.join(","));
    }
    println!("chunk_count={}", response.chunk_count);
    println!("dropped={}", response.dropped);
    if let Some(accepted_at) = response.accepted_at.as_deref() {
        println!("accepted_at={accepted_at}");
    }
    if let Some(rejected_reason) = response.rejected_reason.as_deref() {
        println!("rejected_reason={rejected_reason}");
    }
    if let Some(failure_detail) = response.failure_detail.as_deref() {
        println!("failure_detail={failure_detail}");
    }
    println!(
        "timings={}",
        serde_json::to_string(&response.timings).context("failed to serialize timings")?
    );
    Ok(())
}

fn ingest_stdin_wait_stats_from_response(response: &IngestResponse) -> IngestStats {
    let mut stats = IngestStats {
        files: 1,
        chunks: if response.dropped {
            0
        } else {
            response.chunk_count
        },
        dropped_by_gate: if response.dropped { 1 } else { 0 },
        ..IngestStats::default()
    };
    stats.drawer_ids = if response.drawer_ids.is_empty() && !response.drawer_id.is_empty() {
        vec![response.drawer_id.clone()]
    } else {
        response.drawer_ids.clone()
    };
    stats.fact_check_warnings = response.fact_check_warnings.clone();
    stats.superseded_drawer_id = response.superseded_drawer_id.clone();
    stats
}

async fn wait_for_operation_status_with_progress(
    server: &MempalMcpServer,
    operation_id: &str,
    timeout: std::time::Duration,
    poll_interval: std::time::Duration,
) -> Result<Option<IngestResponse>> {
    let deadline = std::time::Instant::now() + clamp_wait_timeout(timeout);
    loop {
        let response = server
            .mempal_operation_status(rmcp::handler::server::wrapper::Parameters(
                OperationStatusRequest {
                    operation_id: operation_id.to_string(),
                },
            ))
            .await
            .context("failed to load operation status")?
            .0;
        if response
            .state
            .map(IngestOperationState::is_terminal)
            .unwrap_or(false)
        {
            return Ok(Some(response));
        }
        if timeout.is_zero() || std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        eprintln!(
            "waiting operation_id={} state={}",
            operation_id,
            response
                .state
                .map(|state| state.as_str())
                .unwrap_or("unknown")
        );
        tokio::time::sleep(poll_interval).await;
    }
}

const MAX_WAIT_TIMEOUT_SECS: u64 = 86_400;

fn clamp_wait_timeout(timeout: std::time::Duration) -> std::time::Duration {
    timeout.min(std::time::Duration::from_secs(MAX_WAIT_TIMEOUT_SECS))
}

async fn operation_command(
    db: &Database,
    config: &Config,
    command: OperationCommands,
) -> Result<()> {
    let server = MempalMcpServer::new(db.path().to_path_buf(), config.clone())?;
    match command {
        OperationCommands::Status { operation_id } => {
            let response = server
                .mempal_operation_status(rmcp::handler::server::wrapper::Parameters(
                    OperationStatusRequest { operation_id },
                ))
                .await
                .context("failed to load operation status")?
                .0;
            print_operation_response(&response)?;
            Ok(())
        }
        OperationCommands::Wait {
            operation_id,
            timeout_secs,
        } => {
            eprintln!(
                "waiting for operation_id={} timeout_secs={timeout_secs}",
                operation_id
            );
            match wait_for_operation_status_with_progress(
                &server,
                &operation_id,
                std::time::Duration::from_secs(timeout_secs),
                std::time::Duration::from_millis(150),
            )
            .await?
            {
                Some(response) => {
                    print_operation_response(&response)?;
                    match response.state {
                        Some(IngestOperationState::Completed) => Ok(()),
                        Some(IngestOperationState::Rejected) => {
                            bail!("operation {operation_id} was rejected")
                        }
                        Some(IngestOperationState::Failed) => {
                            bail!("operation {operation_id} failed")
                        }
                        _ => Ok(()),
                    }
                }
                None => {
                    let mut response = server
                        .mempal_operation_status(rmcp::handler::server::wrapper::Parameters(
                            OperationStatusRequest {
                                operation_id: operation_id.clone(),
                            },
                        ))
                        .await
                        .context("failed to load timed-out operation status")?
                        .0;
                    response.timed_out = true;
                    print_operation_response(&response)?;
                    bail!("timed out waiting for operation {operation_id}")
                }
            }
        }
    }
}

async fn checkpoint_command(
    db: &Database,
    config: &Config,
    command: CheckpointCommands,
) -> Result<()> {
    use rusqlite::OptionalExtension;

    match command {
        CheckpointCommands::Save { content, project } => {
            let raw_content = match content {
                Some(c) => c,
                None => {
                    let mut input = String::new();
                    std::io::stdin()
                        .read_to_string(&mut input)
                        .context("failed to read checkpoint content from stdin")?;
                    input
                }
            };
            if raw_content.trim().is_empty() {
                bail!("checkpoint content must not be empty");
            }
            let content = normalize_stdin_content(&raw_content, false)?;

            let wing = "session-checkpoint";
            let room: Option<&str> = Some("claude");
            let source_type = SourceType::AgentInference;
            let drawer_id = build_checkpoint_drawer_id(wing, room, &content, &source_type)
                .context("failed to build checkpoint drawer id")?;

            let cwd = env::current_dir().ok();
            let project_id = resolve_project_id(project.as_deref(), config, cwd.as_deref())
                .context("failed to resolve checkpoint project id")?;

            let embedder = build_embedder(config).await?;
            let vectors = embedder
                .embed(&[content.as_str()])
                .await
                .context("failed to embed checkpoint content")?;
            let vector = vectors
                .into_iter()
                .next()
                .context("embedder returned no vector for checkpoint")?;

            let drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
                id: drawer_id.clone(),
                content,
                wing: wing.to_string(),
                room: room.map(ToOwned::to_owned),
                source_file: Some("session-checkpoint".to_string()),
                source_type,
                added_at: iso_timestamp(),
                chunk_index: Some(0),
                importance: 4,
            });
            let drawer = Drawer {
                normalize_version: CURRENT_NORMALIZE_VERSION,
                ..drawer
            };

            db.insert_drawer_with_project(&drawer, project_id.as_deref())
                .with_context(|| format!("failed to insert checkpoint drawer {}", drawer.id))?;
            db.insert_vector_with_project(&drawer_id, &vector, project_id.as_deref())
                .with_context(|| format!("failed to insert vector for checkpoint {drawer_id}"))?;

            println!("checkpoint saved: {drawer_id}");
        }
        CheckpointCommands::Latest { project, json } => {
            let cwd = env::current_dir().ok();
            let project_id = resolve_project_id(project.as_deref(), config, cwd.as_deref())
                .context("failed to resolve project id")?;

            let row = db
                .conn()
                .query_row(
                    "SELECT id, content, added_at FROM drawers \
                     WHERE wing = 'session-checkpoint' AND deleted_at IS NULL \
                     AND (?1 IS NULL OR project_id = ?1) \
                     ORDER BY added_at DESC LIMIT 1",
                    [&project_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()?;

            if let Some((id, content, added_at)) = row {
                if json {
                    let output = serde_json::json!({
                        "id": id,
                        "content": content,
                        "added_at": added_at,
                    });
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&output)
                            .context("failed to serialize checkpoint JSON")?
                    );
                } else {
                    println!("{content}");
                }
            } else if json {
                println!("{{}}");
            } else {
                eprintln!("no checkpoints found");
            }
        }
        CheckpointCommands::Cleanup { max_age, dry_run } => {
            let duration_secs =
                parse_checkpoint_duration(&max_age).context("invalid max-age duration")?;
            let now_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before epoch")
                .as_secs() as i64;
            let cutoff_secs = (now_unix - duration_secs).max(0);
            let cutoff_ts = mempal::cowork::peek::format_rfc3339(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(cutoff_secs as u64),
            );

            if dry_run {
                let count: i64 = db.conn().query_row(
                    "SELECT COUNT(*) FROM drawers \
                     WHERE wing = 'session-checkpoint' AND deleted_at IS NULL \
                     AND added_at < ?1",
                    [&cutoff_ts],
                    |row| row.get(0),
                )?;
                println!("dry-run: would delete {count} checkpoints older than {cutoff_ts}");
            } else {
                let now_ts = iso_timestamp();
                let affected = db.conn().execute(
                    "UPDATE drawers SET deleted_at = ?1 \
                     WHERE wing = 'session-checkpoint' AND deleted_at IS NULL \
                     AND added_at < ?2",
                    [&now_ts, &cutoff_ts],
                )?;
                println!("deleted {affected} checkpoints older than {cutoff_ts}");
            }
        }
        CheckpointCommands::Extract { path, last } => {
            let file = std::fs::File::open(&path)
                .with_context(|| format!("failed to open {}", path.display()))?;
            let reader = std::io::BufReader::new(file);
            use std::io::BufRead;
            let mut assistant_texts: Vec<String> = Vec::new();
            for line in reader.lines() {
                let line = line.context("failed to read JSONL line")?;
                if line.trim().is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v.get("type").and_then(Value::as_str) != Some("assistant") {
                    continue;
                }
                let msg = v.get("message").unwrap_or(&v);
                if let Some(content) = msg.get("content") {
                    let text = extract_text_from_content(content);
                    if !text.is_empty() {
                        assistant_texts.push(text);
                    }
                }
            }
            let start = assistant_texts.len().saturating_sub(last);
            for text in &assistant_texts[start..] {
                println!("{text}");
                println!("---");
            }
            if assistant_texts.is_empty() {
                eprintln!("no assistant messages found in {}", path.display());
            }
        }
        CheckpointCommands::Enable | CheckpointCommands::Disable | CheckpointCommands::Status => {
            let flag = std::env::var_os("HOME")
                .map(PathBuf::from)
                .expect("HOME not set")
                .join(".mempal")
                .join("checkpoint-disabled");
            match command {
                CheckpointCommands::Enable => {
                    if flag.exists() {
                        std::fs::remove_file(&flag)
                            .with_context(|| format!("failed to remove {}", flag.display()))?;
                        println!("checkpoint enabled");
                    } else {
                        println!("checkpoint already enabled");
                    }
                }
                CheckpointCommands::Disable => {
                    if !flag.exists() {
                        std::fs::write(&flag, "disabled by mempal checkpoint disable\n")
                            .with_context(|| format!("failed to create {}", flag.display()))?;
                        println!("checkpoint disabled");
                    } else {
                        println!("checkpoint already disabled");
                    }
                }
                CheckpointCommands::Status => {
                    if flag.exists() {
                        println!("checkpoint: disabled");
                    } else {
                        println!("checkpoint: enabled");
                    }
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(())
}

fn extract_text_from_content(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(arr) => {
            let mut parts = Vec::new();
            for item in arr {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = item.get("text").and_then(Value::as_str) {
                        parts.push(t.to_string());
                    }
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

fn build_checkpoint_drawer_id(
    wing: &str,
    room: Option<&str>,
    content: &str,
    source_type: &SourceType,
) -> Result<String> {
    let base_id = build_bootstrap_evidence_drawer_id(wing, room, content, source_type);
    let timestamp_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock before epoch")?
        .as_secs();
    Ok(format!("{base_id}_{timestamp_secs:x}"))
}

fn parse_checkpoint_duration(raw: &str) -> Result<i64> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty duration");
    }
    let (idx, unit_char) = raw
        .char_indices()
        .last()
        .context("empty duration after trim")?;
    let digits = &raw[..idx];
    let value = digits.parse::<i64>().context("invalid duration digits")?;
    if value <= 0 {
        bail!("duration must be positive");
    }
    let multiplier: i64 = match unit_char {
        'h' => 3600,
        'd' => 86400,
        _ => bail!("unsupported duration unit: {unit_char} (use 'h' or 'd')"),
    };
    value.checked_mul(multiplier).context("duration overflow")
}

async fn ingest_path_with_options<'a, E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    path: &'a Path,
    wing: &'a str,
    options: IngestOptions<'a>,
) -> mempal::ingest::Result<IngestStats> {
    if path.is_file() {
        ingest_file_with_options(db, embedder, path, wing, options).await
    } else {
        ingest_dir_with_options(db, embedder, path, wing, options).await
    }
}

#[derive(Serialize)]
struct IngestJsonOutput<'a> {
    dry_run: bool,
    files: usize,
    chunks: usize,
    skipped: usize,
    dropped_by_gate: usize,
    drawer_ids: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_drawer_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fact_check_warnings: Vec<String>,
}

#[derive(Default)]
struct NoopEmbedder;

#[async_trait::async_trait]
impl Embedder for NoopEmbedder {
    async fn embed(
        &self,
        _texts: &[&str],
    ) -> std::result::Result<Vec<Vec<f32>>, mempal::embed::EmbedError> {
        Ok(Vec::new())
    }
    fn dimensions(&self) -> usize {
        384
    }
    fn name(&self) -> &str {
        "noop"
    }
}

fn append_ingest_audit_log(
    db: &Database,
    dir: &Path,
    wing: &str,
    format: Option<&str>,
    dry_run: bool,
    stats: &IngestStats,
) -> Result<()> {
    let audit_path = db
        .path()
        .parent()
        .map(|p| p.join("audit.jsonl"))
        .unwrap_or_else(|| PathBuf::from("audit.jsonl"));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&audit_path)
        .with_context(|| format!("failed to open audit log {}", audit_path.display()))?;
    let entry = serde_json::json!({ "timestamp": current_timestamp(), "command": "ingest", "wing": wing, "dir": dir.to_string_lossy(), "format": format, "dry_run": dry_run, "files": stats.files, "chunks": stats.chunks, "skipped": stats.skipped, "dropped_by_gate": stats.dropped_by_gate, "superseded_drawer_id": stats.superseded_drawer_id.as_deref() });
    writeln!(file, "{entry}")
        .with_context(|| format!("failed to write audit log {}", audit_path.display()))?;
    Ok(())
}

fn append_ingest_stdin_audit_log(
    db: &Database,
    wing: &str,
    dry_run: bool,
    record: &StdinIngestRecord,
    stats: &IngestStats,
) -> Result<()> {
    let audit_path = db
        .path()
        .parent()
        .map(|p| p.join("audit.jsonl"))
        .unwrap_or_else(|| PathBuf::from("audit.jsonl"));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&audit_path)
        .with_context(|| format!("failed to open audit log {}", audit_path.display()))?;
    let metadata = record.metadata.as_ref().map(scrub_metadata_for_audit_log);
    let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
    let scrub = |s: &str| config.scrub_content_with_compiled(s, compiled_privacy.as_ref());
    let source = record.source.as_deref().map(&scrub);
    let source_file = record.source_file.as_deref().map(&scrub);
    let entry = serde_json::json!({
        "timestamp": current_timestamp(),
        "command": "ingest",
        "mode": "stdin",
        "wing": wing,
        "source": source,
        "source_file": source_file,
        "supersedes": record.supersedes.as_deref().map(&scrub),
        "replace_text": record.replace_text.as_deref().map(&scrub),
        "metadata": metadata.as_ref(),
        "dry_run": dry_run,
        "files": stats.files,
        "chunks": stats.chunks,
        "skipped": stats.skipped,
        "dropped_by_gate": stats.dropped_by_gate,
        "superseded_drawer_id": stats.superseded_drawer_id.as_deref(),
    });
    writeln!(file, "{entry}")
        .with_context(|| format!("failed to write audit log {}", audit_path.display()))?;
    Ok(())
}

fn scrub_metadata_for_audit_log(
    metadata: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
    metadata
        .iter()
        .map(|(key, value)| {
            (
                config.scrub_content_with_compiled(key, compiled_privacy.as_ref()),
                scrub_metadata_value_for_audit_log(value, &config, compiled_privacy.as_ref()),
            )
        })
        .collect()
}

fn scrub_metadata_value_for_audit_log(
    value: &Value,
    config: &Config,
    compiled_privacy: &CompiledPrivacyConfig,
) -> Value {
    match value {
        Value::String(text) => {
            Value::String(config.scrub_content_with_compiled(text, compiled_privacy))
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| scrub_metadata_value_for_audit_log(item, config, compiled_privacy))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    (
                        config.scrub_content_with_compiled(key, compiled_privacy),
                        scrub_metadata_value_for_audit_log(value, config, compiled_privacy),
                    )
                })
                .collect(),
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
    }
}

async fn context_command(db: &Database, config: &Config, args: ContextCommandArgs) -> Result<()> {
    if args.max_items == 0 {
        bail!("--max-items must be greater than 0");
    }
    let domain = parse_domain(&args.domain)?;
    let cwd = match args.cwd {
        Some(cwd) => cwd,
        None => env::current_dir().context("failed to read current directory")?,
    };
    let trigger = args.trigger.as_deref().map(|s| match s {
        "on_demand" => mempal::search::tiered::ContextTrigger::OnDemand,
        "repair" => mempal::search::tiered::ContextTrigger::Repair,
        _ => mempal::search::tiered::ContextTrigger::SessionStart,
    });
    let embedder = build_embedder(config).await?;
    let pack = assemble_context(
        db,
        &*embedder,
        ContextRequest {
            query: args.query,
            domain,
            field: args.field,
            cwd,
            include_evidence: args.include_evidence,
            include_cards: args.include_cards,
            max_items: args.max_items,
            dao_tian_limit: args.dao_tian_limit,
            project_id: config.project.id.clone(),
            trigger,
            context_cfg_override: None,
            include_distill_suggestions: !args.no_distill_suggestions,
        },
    )
    .await?;
    match args.format.as_str() {
        "plain" => print_context_plain(&pack),
        "json" => println!(
            "{}",
            serde_json::to_string_pretty(&pack).context("failed to serialize context pack")?
        ),
        other => bail!("unsupported context format: {other}"),
    }
    Ok(())
}

fn parse_domain(value: &str) -> Result<MemoryDomain> {
    match value {
        "project" => Ok(MemoryDomain::Project),
        "user" => Ok(MemoryDomain::User),
        "agent" => Ok(MemoryDomain::Agent),
        "skill" => Ok(MemoryDomain::Skill),
        "global" => Ok(MemoryDomain::Global),
        other => bail!("unsupported domain: {other}"),
    }
}

fn print_context_plain(pack: &ContextPack) {
    if pack.sections.is_empty() {
        println!("no context");
    }
    for section in &pack.sections {
        println!("## {}", section.name);
        for item in &section.items {
            println!("- {}", item.text);
            println!("  source: {}", item.source_file);
            println!("  drawer: {}", item.drawer_id);
            if let Some(card_id) = item.card_id.as_deref() {
                println!("  card: {card_id}");
            }
            println!(
                "  anchor: {} {}",
                anchor_kind_slug(&item.anchor_kind),
                item.anchor_id
            );
            if let (Some(tier), Some(status)) = (&item.tier, &item.status) {
                println!(
                    "  knowledge: tier={} status={}",
                    knowledge_tier_slug(tier),
                    knowledge_status_slug(status)
                );
            }
            if let Some(trigger_hints) = item.trigger_hints.as_ref() {
                println!(
                    "  trigger_hints: intent_tags={} workflow_bias={} tool_needs={}",
                    trigger_hints.intent_tags.join(","),
                    trigger_hints.workflow_bias.join(","),
                    trigger_hints.tool_needs.join(",")
                );
            }
            for citation in &item.evidence_citations {
                println!(
                    "  evidence: {} role={} source={}",
                    citation.evidence_drawer_id,
                    knowledge_evidence_role_slug(&citation.role),
                    citation.source_file
                );
            }
        }
        println!();
    }

    if !pack.distill_suggestions.is_empty() {
        println!("## distill_suggestions");
        for suggestion in &pack.distill_suggestions {
            println!(
                "- field={} evidence_count={} suggested_tier={}",
                suggestion.field, suggestion.evidence_count, suggestion.suggested_tier
            );
            if !suggestion.sample_evidence_drawer_ids.is_empty() {
                println!(
                    "  samples: {}",
                    suggestion.sample_evidence_drawer_ids.join(",")
                );
            }
        }
    }
}

async fn search_command(
    db: &Database,
    config: &Config,
    options: SearchCommandOptions<'_>,
) -> Result<()> {
    let current_dir = env::current_dir().ok();
    let resolved_project = resolve_project_id(options.project, config, current_dir.as_deref())
        .context("failed to resolve search project id")?;
    let scope = ProjectSearchScope::from_request(
        resolved_project,
        options.include_global,
        options.all_projects,
        config.search.strict_project_isolation,
    );
    let embedder = build_embedder(config).await?;
    let results = search_with_all_options(
        db,
        &*embedder,
        options.query,
        options.wing,
        options.room,
        &scope,
        SearchOptions {
            filters: options.filters,
            with_neighbors: options.with_neighbors,
            include_raw_turns: options.include_raw_turns,
            include_expired: options.include_expired,
        },
        options.top_k,
    )
    .await?;
    let results = results
        .into_iter()
        .map(build_cli_search_result)
        .collect::<Vec<_>>();

    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&results).context("failed to serialize search results")?
        );
        return Ok(());
    }
    if results.is_empty() {
        println!("no results");
        return Ok(());
    }

    for result in &results {
        let room = result.room.clone().unwrap_or_else(|| "default".to_string());
        println!(
            "[{:.3}] {}/{} {}",
            result.similarity, result.wing, room, result.drawer_id
        );
        println!("source: {}", result.source_file);
        println!(
            "kind: {} domain: {} field: {} anchor: {} {}",
            result.memory_kind, result.domain, result.field, result.anchor_kind, result.anchor_id
        );
        if let Some(parent_anchor_id) = result.parent_anchor_id.as_deref() {
            println!("parent_anchor: {parent_anchor_id}");
        }
        if let Some(tier) = result.tier.as_deref() {
            println!(
                "knowledge: tier={tier} status={}",
                result.status.as_deref().unwrap_or("unknown")
            );
        }
        if let Some(statement) = result.statement.as_deref() {
            println!("statement: {statement}");
        }
        if !result.tunnel_hints.is_empty() {
            println!("tunnel: also in {}", result.tunnel_hints.join(", "));
        }
        if let Some(neighbors) = result.neighbors.as_ref() {
            if let Some(prev) = neighbors.prev.as_ref() {
                println!("prev[{}]: {}", prev.chunk_index, prev.content);
            }
            if let Some(next) = neighbors.next.as_ref() {
                println!("next[{}]: {}", next.chunk_index, next.content);
            }
        }
        println!("{}", result.content);
        println!();
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct CliSearchResult {
    drawer_id: String,
    content: String,
    wing: String,
    room: Option<String>,
    source_file: String,
    similarity: f32,
    route: mempal::core::types::RouteDecision,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tunnel_hints: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    neighbors: Option<mempal::core::types::ChunkNeighbors>,
    memory_kind: String,
    domain: String,
    field: String,
    is_pinned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    statement: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    anchor_kind: String,
    anchor_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_anchor_id: Option<String>,
}

fn build_cli_search_result(result: mempal::core::types::SearchResult) -> CliSearchResult {
    CliSearchResult {
        drawer_id: result.drawer_id,
        content: result.content,
        wing: result.wing,
        room: result.room,
        source_file: result.source_file,
        similarity: result.similarity,
        route: result.route,
        tunnel_hints: result.tunnel_hints,
        neighbors: result.neighbors,
        memory_kind: memory_kind_slug(&result.memory_kind).to_string(),
        domain: domain_slug(&result.domain).to_string(),
        field: result.field,
        is_pinned: result.is_pinned,
        statement: result.statement,
        tier: result
            .tier
            .as_ref()
            .map(knowledge_tier_slug)
            .map(str::to_string),
        status: result
            .status
            .as_ref()
            .map(knowledge_status_slug)
            .map(str::to_string),
        anchor_kind: anchor_kind_slug(&result.anchor_kind).to_string(),
        anchor_id: result.anchor_id,
        parent_anchor_id: result.parent_anchor_id,
    }
}

fn memory_kind_slug(v: &MemoryKind) -> &'static str {
    match v {
        MemoryKind::Evidence => "evidence",
        MemoryKind::Knowledge => "knowledge",
        MemoryKind::ProfileFact => "profile_fact",
    }
}
fn domain_slug(v: &MemoryDomain) -> &'static str {
    match v {
        MemoryDomain::Project => "project",
        MemoryDomain::User => "user",
        MemoryDomain::Agent => "agent",
        MemoryDomain::Skill => "skill",
        MemoryDomain::Global => "global",
    }
}
fn knowledge_tier_slug(v: &KnowledgeTier) -> &'static str {
    match v {
        KnowledgeTier::Qi => "qi",
        KnowledgeTier::Shu => "shu",
        KnowledgeTier::DaoRen => "dao_ren",
        KnowledgeTier::DaoTian => "dao_tian",
    }
}
fn parse_knowledge_tier(v: &str) -> Result<KnowledgeTier> {
    match v {
        "qi" => Ok(KnowledgeTier::Qi),
        "shu" => Ok(KnowledgeTier::Shu),
        "dao_ren" => Ok(KnowledgeTier::DaoRen),
        "dao_tian" => Ok(KnowledgeTier::DaoTian),
        o => bail!("unsupported knowledge tier: {o}"),
    }
}
fn knowledge_status_slug(v: &KnowledgeStatus) -> &'static str {
    match v {
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
fn parse_knowledge_status(v: &str) -> Result<KnowledgeStatus> {
    match v {
        "active" => Ok(KnowledgeStatus::Active),
        "superseded" => Ok(KnowledgeStatus::Superseded),
        "pending_review" => Ok(KnowledgeStatus::PendingReview),
        "candidate" => Ok(KnowledgeStatus::Candidate),
        "promoted" => Ok(KnowledgeStatus::Promoted),
        "canonical" => Ok(KnowledgeStatus::Canonical),
        "demoted" => Ok(KnowledgeStatus::Demoted),
        "retired" => Ok(KnowledgeStatus::Retired),
        o => bail!("unsupported knowledge status: {o}"),
    }
}
fn anchor_kind_slug(v: &AnchorKind) -> &'static str {
    match v {
        AnchorKind::Global => "global",
        AnchorKind::Repo => "repo",
        AnchorKind::Worktree => "worktree",
    }
}
fn parse_anchor_kind(v: &str) -> Result<AnchorKind> {
    match v {
        "global" => Ok(AnchorKind::Global),
        "repo" => Ok(AnchorKind::Repo),
        "worktree" => Ok(AnchorKind::Worktree),
        o => bail!("unsupported anchor kind: {o}"),
    }
}
fn knowledge_evidence_role_slug(v: &KnowledgeEvidenceRole) -> &'static str {
    match v {
        KnowledgeEvidenceRole::Supporting => "supporting",
        KnowledgeEvidenceRole::Verification => "verification",
        KnowledgeEvidenceRole::Counterexample => "counterexample",
        KnowledgeEvidenceRole::Teaching => "teaching",
    }
}
fn parse_knowledge_evidence_role(v: &str) -> Result<KnowledgeEvidenceRole> {
    match v {
        "supporting" => Ok(KnowledgeEvidenceRole::Supporting),
        "verification" => Ok(KnowledgeEvidenceRole::Verification),
        "counterexample" => Ok(KnowledgeEvidenceRole::Counterexample),
        "teaching" => Ok(KnowledgeEvidenceRole::Teaching),
        o => bail!("unsupported knowledge evidence role: {o}"),
    }
}
fn knowledge_event_type_slug(v: &KnowledgeEventType) -> &'static str {
    match v {
        KnowledgeEventType::Created => "created",
        KnowledgeEventType::Promoted => "promoted",
        KnowledgeEventType::Demoted => "demoted",
        KnowledgeEventType::Retired => "retired",
        KnowledgeEventType::Linked => "linked",
        KnowledgeEventType::Unlinked => "unlinked",
        KnowledgeEventType::Updated => "updated",
        KnowledgeEventType::PublishedAnchor => "published_anchor",
    }
}
fn parse_knowledge_event_type(v: &str) -> Result<KnowledgeEventType> {
    match v {
        "created" => Ok(KnowledgeEventType::Created),
        "promoted" => Ok(KnowledgeEventType::Promoted),
        "demoted" => Ok(KnowledgeEventType::Demoted),
        "retired" => Ok(KnowledgeEventType::Retired),
        "linked" => Ok(KnowledgeEventType::Linked),
        "unlinked" => Ok(KnowledgeEventType::Unlinked),
        "updated" => Ok(KnowledgeEventType::Updated),
        "published_anchor" => Ok(KnowledgeEventType::PublishedAnchor),
        o => bail!("unsupported knowledge event type: {o}"),
    }
}

fn runtime_adoption_track_slug(value: &RuntimeAdoptionTrack) -> &'static str {
    match value {
        RuntimeAdoptionTrack::RuntimeAdoption => "runtime_adoption",
        RuntimeAdoptionTrack::CardContext => "card_context",
        RuntimeAdoptionTrack::CardEmbedding => "card_embedding",
        RuntimeAdoptionTrack::Evaluator => "evaluator",
        RuntimeAdoptionTrack::ResearchAdapter => "research_adapter",
    }
}

fn parse_runtime_adoption_track(value: &str) -> Result<RuntimeAdoptionTrack> {
    match value {
        "runtime_adoption" => Ok(RuntimeAdoptionTrack::RuntimeAdoption),
        "card_context" => Ok(RuntimeAdoptionTrack::CardContext),
        "card_embedding" => Ok(RuntimeAdoptionTrack::CardEmbedding),
        "evaluator" => Ok(RuntimeAdoptionTrack::Evaluator),
        "research_adapter" => Ok(RuntimeAdoptionTrack::ResearchAdapter),
        other => bail!("unsupported runtime adoption track: {other}"),
    }
}

fn runtime_adoption_signal_slug(value: &RuntimeAdoptionSignal) -> &'static str {
    match value {
        RuntimeAdoptionSignal::Used => "used",
        RuntimeAdoptionSignal::Accepted => "accepted",
        RuntimeAdoptionSignal::Rejected => "rejected",
        RuntimeAdoptionSignal::Miss => "miss",
        RuntimeAdoptionSignal::Rollback => "rollback",
        RuntimeAdoptionSignal::Contradiction => "contradiction",
        RuntimeAdoptionSignal::Neutral => "neutral",
    }
}

fn parse_runtime_adoption_signal(value: &str) -> Result<RuntimeAdoptionSignal> {
    match value {
        "used" => Ok(RuntimeAdoptionSignal::Used),
        "accepted" => Ok(RuntimeAdoptionSignal::Accepted),
        "rejected" => Ok(RuntimeAdoptionSignal::Rejected),
        "miss" => Ok(RuntimeAdoptionSignal::Miss),
        "rollback" => Ok(RuntimeAdoptionSignal::Rollback),
        "contradiction" => Ok(RuntimeAdoptionSignal::Contradiction),
        "neutral" => Ok(RuntimeAdoptionSignal::Neutral),
        other => bail!("unsupported runtime adoption signal: {other}"),
    }
}

fn stable_cli_id(prefix: &str, parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update([0]);
        hasher.update(part.trim().as_bytes());
    }
    let digest = format!("{:x}", hasher.finalize());
    format!("{prefix}_{}", &digest[..16])
}

fn effective_wake_up_text(drawer: &mempal::core::types::Drawer) -> &str {
    match drawer.memory_kind {
        MemoryKind::Knowledge => drawer
            .statement
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(drawer.content.as_str()),
        MemoryKind::Evidence | MemoryKind::ProfileFact => drawer.content.as_str(),
    }
}

fn project_command(db: &Database, command: ProjectCommands) -> Result<()> {
    match command {
        ProjectCommands::Migrate { project, wing } => {
            migrate_null_project_ids(db.path(), &project, wing.as_deref(), |event| match event {
                ProjectMigrationEvent::Busy { delay_ms } => {
                    println!("batch busy, retrying in {delay_ms}ms");
                    let _ = std::io::stdout().flush();
                }
                ProjectMigrationEvent::Progress(progress) => {
                    println!(
                        "batch {}: {} drawers updated, {} remaining",
                        progress.batch_index, progress.updated, progress.remaining
                    );
                    let _ = std::io::stdout().flush();
                }
            })
            .context("failed to migrate project ids")
        }
    }
}

fn wake_up_command(db: &Database, format: Option<WakeUpFormat>) -> Result<()> {
    match format {
        Some(WakeUpFormat::Aaak) => return wake_up_aaak_command(db),
        Some(WakeUpFormat::Protocol) => {
            println!("{MEMORY_PROTOCOL}");
            return Ok(());
        }
        None => {}
    }
    let drawer_count = db.drawer_count().context("failed to count drawers")?;
    let taxonomy_count = db.taxonomy_count().context("failed to count taxonomy")?;
    let top_drawers = db
        .top_drawers(5)
        .context("failed to load recent drawers for wake-up")?;
    let token_estimate = estimate_wake_up_tokens(&top_drawers);
    println!("## L0 — Identity");
    let identity = read_identity_file();
    if identity.is_empty() {
        println!("{DEFAULT_IDENTITY_HINT}");
    } else {
        for line in identity.lines() {
            println!("{line}");
        }
    }
    println!();
    println!("drawer_count: {drawer_count}");
    println!("taxonomy_entries: {taxonomy_count}");
    println!();
    println!("## L1 — Recent Context");
    if top_drawers.is_empty() {
        println!("no recent drawers");
    } else {
        for drawer in &top_drawers {
            println!(
                "- {}/{} {}",
                drawer.wing,
                render_room(drawer.room.as_deref()),
                drawer.id
            );
            if let Some(source_file) = drawer.source_file.as_deref() {
                println!("  source: {source_file}");
            }
            println!(
                "  {}",
                truncate_for_summary(effective_wake_up_text(drawer), 120)
            );
        }
    }
    println!();
    println!("estimated_tokens: {token_estimate}");
    println!();
    println!("## Memory Protocol");
    println!("{MEMORY_PROTOCOL}");
    Ok(())
}

fn read_identity_file() -> String {
    let Some(home) = env::var_os("HOME") else {
        return String::new();
    };
    std::fs::read_to_string(PathBuf::from(home).join(".mempal").join("identity.txt"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn wake_up_aaak_command(db: &Database) -> Result<()> {
    let top_drawers = db
        .top_drawers(5)
        .context("failed to load recent drawers for AAAK wake-up")?;
    let text = if top_drawers.is_empty() {
        "mempal wake-up: no recent drawers".to_string()
    } else {
        top_drawers
            .iter()
            .map(effective_wake_up_text)
            .collect::<Vec<_>>()
            .join(" ")
    };
    let wing = top_drawers
        .first()
        .map(|d| d.wing.as_str())
        .unwrap_or("mempal");
    let room = top_drawers
        .first()
        .and_then(|d| d.room.as_deref())
        .unwrap_or("default");
    let output = AaakCodec::default().encode(
        &text,
        &AaakMeta {
            wing: wing.to_string(),
            room: room.to_string(),
            date: current_timestamp(),
            source: "wake-up".to_string(),
        },
    );
    println!("{}", output.document);
    Ok(())
}

fn compress_command(text: &str) -> Result<()> {
    let output = AaakCodec::default().encode(
        text,
        &AaakMeta {
            wing: "manual".to_string(),
            room: "compress".to_string(),
            date: current_timestamp(),
            source: "cli".to_string(),
        },
    );
    println!("{}", output.document);
    Ok(())
}

fn consolidate_command(
    db: &Database,
    config: &Config,
    options: ConsolidateCommandOptions<'_>,
) -> Result<()> {
    let threshold = options
        .threshold
        .unwrap_or(config.consolidation.similarity_threshold);
    let min_cluster = options
        .min_cluster
        .unwrap_or(config.consolidation.min_cluster_size);
    let limit = options
        .limit
        .unwrap_or(config.consolidation.max_clusters_per_run);
    let strategy_value = options.strategy.unwrap_or(&config.consolidation.strategy);
    let strategy = match strategy_value.parse::<CompactionStrategy>() {
        Ok(strategy) => strategy,
        Err(_) => bail!("invalid compaction strategy: {strategy_value}"),
    };
    if strategy == CompactionStrategy::LlmSummary {
        bail!("LLM compaction not yet implemented");
    }

    let current_dir = env::current_dir().ok();
    let project_id = resolve_project_id(None, config, current_dir.as_deref())
        .context("failed to resolve project id for consolidation")?;
    if project_id.is_none() && options.wing.is_none() {
        bail!("consolidation requires --wing when no project id can be resolved");
    }

    let clusters = find_similar_clusters(
        db.conn(),
        options.wing,
        options.room,
        project_id.as_deref(),
        threshold,
        min_cluster,
    )
    .context("failed to find similar drawer clusters")?;

    println!("clusters_found: {}", clusters.len());
    let mut processed = 0usize;
    let mut drawers_merged = 0usize;
    for (index, cluster) in clusters.iter().take(limit).enumerate() {
        let drawer_ids = cluster
            .iter()
            .map(|(drawer_id, _)| drawer_id.clone())
            .collect::<Vec<_>>();
        let result = merge_cluster(db, &drawer_ids, strategy, options.dry_run)
            .context("failed to merge drawer cluster")?;
        processed += 1;
        if options.dry_run {
            println!(
                "cluster {}: size={} target={} strategy={} dry_run=true avg_similarity={:.4}",
                index + 1,
                result.cluster_size,
                result.target_id,
                result.strategy,
                average_cluster_similarity(cluster)
            );
            for (drawer_id, similarity) in cluster {
                let preview = db
                    .get_drawer(drawer_id)
                    .context("failed to load drawer preview")?
                    .map(|drawer| preview_one_line(&drawer.content, 96))
                    .unwrap_or_else(|| "<inactive>".to_string());
                println!("  - {drawer_id} similarity={similarity:.4} preview=\"{preview}\"");
            }
        } else {
            let source_count = result.source_ids.len().saturating_sub(1);
            drawers_merged += source_count;
            println!(
                "merged cluster {}: target={} sources={} cluster_size={}",
                index + 1,
                result.target_id,
                source_count,
                result.cluster_size
            );
        }
    }
    println!(
        "summary: clusters_found={} processed={} drawers_merged={}",
        clusters.len(),
        processed,
        drawers_merged
    );
    Ok(())
}

fn sleep_command(db: &Database, config: &Config, options: SleepCommandOptions) -> Result<()> {
    let current_dir = env::current_dir().ok();
    let project_id = resolve_project_id(None, config, current_dir.as_deref())
        .context("failed to resolve project id for sleep cycle")?;
    let summary = run_sleep_cycle(
        db,
        config,
        SleepRunOptions {
            phases: SleepPhaseSelection {
                nrem: options.nrem,
                rem: options.rem,
                salience: options.salience,
            },
            dry_run: options.dry_run,
            project_id,
        },
    )
    .context("sleep cycle failed")?;
    print_sleep_summary(&summary);
    Ok(())
}

async fn crystallize_command(
    db: &Database,
    config: &Config,
    options: CrystallizeCliOptions,
) -> Result<()> {
    let current_dir = env::current_dir().ok();
    let project_id = match options.project {
        Some(project) => Some(project),
        None => resolve_project_id(None, config, current_dir.as_deref())
            .context("failed to resolve project id for crystallization")?,
    };
    let summary = run_crystallization(
        db,
        config,
        CrystallizeOptions {
            dry_run: options.dry_run,
            project_id,
            use_llm: true,
        },
    )
    .await
    .context("auto-crystallization failed")?;
    print_crystallize_summary(&summary, options.json)
}

fn print_crystallize_summary(summary: &CrystallizeSummary, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "candidates_found": summary.candidates_found,
                "cards_created": summary.cards_created,
                "dry_run": summary.dry_run,
                "used_llm": summary.used_llm,
                "fallback_count": summary.fallback_count,
                "candidates": summary.candidates.iter().map(|candidate| {
                    serde_json::json!({
                        "card_id": &candidate.card.id,
                        "status": knowledge_status_slug(&candidate.card.status),
                        "cluster_key": &candidate.cluster_key,
                        "drawer_count": candidate.drawer_count,
                        "crystallization_score": candidate.crystallization_score,
                        "source_drawer_ids": &candidate.source_drawer_ids,
                        "source_files": &candidate.source_files,
                        "used_llm": candidate.used_llm,
                        "fallback_reason": &candidate.fallback_reason,
                    })
                }).collect::<Vec<_>>()
            }))
            .context("failed to serialize crystallize summary")?
        );
        return Ok(());
    }

    println!(
        "crystallize: candidates_found={} cards_created={} dry_run={}",
        summary.candidates_found, summary.cards_created, summary.dry_run
    );
    for candidate in &summary.candidates {
        println!(
            "candidate card_id={} status={} score={:.3} drawers={} key={}",
            candidate.card.id,
            knowledge_status_slug(&candidate.card.status),
            candidate.crystallization_score,
            candidate.drawer_count,
            candidate.cluster_key
        );
        println!("statement: {}", candidate.card.statement);
        if !candidate.source_files.is_empty() {
            println!("sources: {}", candidate.source_files.join(", "));
        }
    }
    Ok(())
}

fn cards_command(db: &Database, options: CardsCommandOptions) -> Result<()> {
    let selected = usize::from(options.pending)
        + usize::from(options.approve.is_some())
        + usize::from(options.reject.is_some());
    if selected != 1 {
        bail!("choose exactly one of --pending, --approve <card_id>, or --reject <card_id>");
    }
    if options.pending {
        let cards = db
            .list_knowledge_cards(&KnowledgeCardFilter {
                status: Some(KnowledgeStatus::PendingReview),
                auto_generated: Some(true),
                pending_review: Some(true),
                ..KnowledgeCardFilter::default()
            })
            .context("failed to list pending auto-generated cards")?;
        return print_knowledge_cards(&cards, &options.format);
    }
    if let Some(card_id) = options.approve {
        let card = set_pending_auto_card_status(
            db,
            &card_id,
            KnowledgeStatus::Promoted,
            "approved auto-generated card",
        )?;
        return print_knowledge_card(&card, &options.format);
    }
    if let Some(card_id) = options.reject {
        let card = set_pending_auto_card_status(
            db,
            &card_id,
            KnowledgeStatus::Retired,
            "rejected auto-generated card",
        )?;
        return print_knowledge_card(&card, &options.format);
    }
    unreachable!()
}

fn set_pending_auto_card_status(
    db: &Database,
    card_id: &str,
    status: KnowledgeStatus,
    reason: &str,
) -> Result<KnowledgeCard> {
    let mut card = db
        .get_knowledge_card(card_id)
        .context("failed to get knowledge card")?
        .with_context(|| format!("knowledge card not found: {card_id}"))?;
    if !card.auto_generated || card.status != KnowledgeStatus::PendingReview {
        bail!("card {card_id} is not a pending auto-generated card");
    }
    let old_status = card.status.clone();
    card.status = status.clone();
    card.updated_at = current_timestamp();
    db.update_knowledge_card(&card)
        .context("failed to update knowledge card")?;
    db.append_knowledge_event(&KnowledgeCardEvent {
        id: stable_cli_id(
            "event",
            &[
                card.id.as_str(),
                knowledge_status_slug(&status),
                card.updated_at.as_str(),
            ],
        ),
        card_id: card.id.clone(),
        event_type: if status == KnowledgeStatus::Retired {
            KnowledgeEventType::Retired
        } else {
            KnowledgeEventType::Promoted
        },
        from_status: Some(old_status),
        to_status: Some(status),
        reason: reason.to_string(),
        actor: Some("mempal.cards".to_string()),
        metadata: Some(serde_json::json!({
            "auto_generated": true,
            "crystallization_score": card.crystallization_score,
            "source_drawer_ids": card.source_drawer_ids,
        })),
        created_at: card.updated_at.clone(),
    })
    .context("failed to append knowledge card event")?;
    Ok(card)
}

fn print_sleep_summary(summary: &SleepCycleSummary) {
    println!(
        "sleep: phases={} dry_run={}",
        summary
            .phases
            .iter()
            .map(|phase| phase.as_str())
            .collect::<Vec<_>>()
            .join(","),
        summary.dry_run
    );
    if let Some(nrem) = &summary.nrem {
        print_nrem_summary(nrem);
    }
    if summary.crystallize_candidates > 0 || summary.crystallized_cards > 0 {
        println!(
            "crystallize: candidates={} cards_created={}",
            summary.crystallize_candidates, summary.crystallized_cards
        );
    }
    if let Some(rem) = &summary.rem {
        print_rem_summary(rem);
    }
    if let Some(salience) = &summary.salience {
        print_salience_summary(salience);
    }
    println!(
        "summary: processed={} pruned={} compacted={} conflicts_resolved={} salience_scored={}",
        summary.processed_count(),
        summary.pruned_count(),
        summary.compacted_count(),
        summary.conflicts_resolved_count(),
        summary.salience_scored_count()
    );
}

fn print_nrem_summary(summary: &NremSummary) {
    println!(
        "nrem: processed={} pruned={} clusters_found={} clusters_compacted={} compacted_drawers={}",
        summary.processed_drawers,
        summary.pruned_drawers,
        summary.clusters_found,
        summary.clusters_compacted,
        summary.compacted_drawers
    );
}

fn print_rem_summary(summary: &RemSummary) {
    println!(
        "rem: processed={} conflicts_detected={} conflicts_resolved={}",
        summary.processed_drawers, summary.conflicts_detected, summary.conflicts_resolved
    );
}

fn print_salience_summary(summary: &SalienceSummary) {
    println!(
        "salience: processed={} scored={}",
        summary.processed_drawers, summary.scored_drawers
    );
}

fn average_cluster_similarity(cluster: &[(String, f64)]) -> f64 {
    if cluster.is_empty() {
        return 0.0;
    }
    cluster
        .iter()
        .map(|(_, similarity)| similarity)
        .sum::<f64>()
        / cluster.len() as f64
}

fn preview_one_line(content: &str, max_chars: usize) -> String {
    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let mut preview = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview.replace('"', "\\\"")
}

fn recompute_importance_command(db: &Database, only_zero: bool) -> Result<()> {
    use mempal::importance::score_importance;
    let drawers = db
        .drawers_for_rescore(only_zero)
        .context("failed to load drawers for importance rescoring")?;
    let total = drawers.len();
    if total == 0 {
        println!("no drawers to rescore");
        return Ok(());
    }
    println!("scoring {total} drawers...");
    let updates: Vec<(String, i32)> = drawers
        .into_iter()
        .map(|d| {
            let s = score_importance(&d);
            (d.id, s)
        })
        .collect();
    let updated = db
        .bulk_update_importance(&updates)
        .context("failed to apply importance scores")?;
    println!("updated {updated} drawers with recomputed importance scores");
    Ok(())
}

fn reindex_failed_queue_command(db: &Database) -> Result<()> {
    let store = mempal::core::queue::PendingMessageStore::new(db.path())
        .context("failed to open pending message queue")?;
    let retried = store
        .retry_failed_embed_messages()
        .context("failed to requeue failed embed messages")?;
    println!("requeued failed embed queue items: {retried}");
    Ok(())
}

fn audit_stale_command(db: &Database, threshold: f64) -> Result<()> {
    let rows = db
        .drawers_below_importance_threshold(threshold, 200)
        .context("failed to query stale drawers")?;
    if rows.is_empty() {
        println!("no drawers below effective_importance threshold {threshold:.3}");
        return Ok(());
    }
    println!(
        "{:<44}  {:<20}  {:<16}  {:>8}  {:>10}  {:>16}",
        "drawer_id", "wing", "room", "eff_imp", "accesses", "last_accessed_at"
    );
    println!("{}", "-".repeat(120));
    for (id, wing, room, eff_imp, access_count, last_accessed_ms) in &rows {
        let room_str = room.as_deref().unwrap_or("-");
        let last_str = last_accessed_ms
            .map(|ms| {
                use std::time::{Duration, UNIX_EPOCH};
                let secs = (ms / 1000) as u64;
                let t = UNIX_EPOCH + Duration::from_secs(secs);
                mempal::cowork::peek::format_rfc3339(t)
            })
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{id:<44}  {wing:<20}  {room_str:<16}  {eff_imp:>8.3}  {access_count:>10}  {last_str:>16}"
        );
    }
    println!("\n{} drawer(s) below threshold {threshold:.3}", rows.len());
    Ok(())
}

fn recompute_effective_importance_command(db: &Database) -> Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let config = mempal::core::config::ConfigHandle::current();
    let imp = &config.importance;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    println!(
        "recomputing effective_importance (decay_rate={}, floor={}, boost_cap={})...",
        imp.decay_rate, imp.floor, imp.boost_cap
    );
    let updated = db
        .recompute_all_effective_importance(now_ms, imp.decay_rate, imp.floor, imp.boost_cap)
        .context("failed to recompute effective_importance")?;
    println!("updated {updated} drawers");
    Ok(())
}

fn load_added_at_rows(db: &Database) -> Result<Vec<(String, String)>> {
    let mut stmt = db
        .conn()
        .prepare("SELECT id, added_at FROM drawers WHERE deleted_at IS NULL ORDER BY rowid ASC")
        .context("failed to prepare added_at query")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("failed to execute added_at query")?
        .collect::<Result<Vec<_>, _>>()
        .context("failed to collect added_at rows")?;
    Ok(rows)
}

fn normalize_added_at_command(db: &Database) -> Result<()> {
    let rows = load_added_at_rows(db).context("failed to load drawers for normalization")?;
    let total = rows.len();
    if total == 0 {
        println!("no drawers found");
        return Ok(());
    }
    println!("scanning {total} drawers for Unix-epoch added_at values...");
    let updates: Vec<(String, String)> = rows
        .into_iter()
        .filter_map(|(id, added_at)| normalize_added_at_value(&added_at).map(|iso| (id, iso)))
        .collect();
    let to_update = updates.len();
    if to_update == 0 {
        println!("nothing to do: 0 drawers need added_at normalisation");
        println!("all {total} drawers already have ISO 8601 added_at");
        return Ok(());
    }
    println!("normalising {to_update} rows (batches of 1000)...");
    let updated = db
        .bulk_update_added_at(&updates)
        .context("failed to apply added_at normalisation")?;
    println!("done: {updated} drawers normalised to ISO 8601 added_at");
    Ok(())
}

async fn reindex_command_by_embedder(
    db: &Database,
    config: &Config,
    embedder_name: &str,
    mode: ReindexEmbedderMode,
    batch: StaleBatchTuning,
    target: ReindexVectorTarget,
) -> Result<()> {
    if mode.dry_run && !mode.stale_only {
        bail!(
            "--dry-run on embedder/vector reindex requires --stale; add --stale or use `mempal reindex --stale --dry-run`"
        );
    }
    if mode.dry_run {
        let stale_count = preview_reindex_stale_vector_work(db, config, embedder_name, target)
            .await
            .context("failed to plan stale vector reindex")?;
        println!("would re-embed {stale_count} stale drawers");
        println!("dry-run: no vectors were written");
        return Ok(());
    }
    let embedder = build_specific_embedder(config, embedder_name).await?;
    let new_dim = embedder.dimensions();
    let current_dim = current_vector_dim(db).context("failed to read embedding dim")?;
    let current_metric = db
        .vector_table_distance_metric()
        .context("failed to read vector distance metric")?;
    let progress_store = ReindexProgressStore::new(db.path());
    let target_fingerprint = config
        .embed
        .vector_embedder_fingerprint(embedder_name, new_dim);
    let mut resume_checkpoint = if mode.resume {
        progress_store
            .latest_resumable(Some(embedder_name))
            .context("failed to load reindex checkpoint")?
    } else {
        None
    };
    println!("embedder: {} ({}d)", embedder_name, new_dim);
    if let Some(dim) = current_dim {
        println!("current vector dim: {dim}");
    } else {
        println!("current vector dim: (empty table)");
    }
    println!(
        "current vector metric: {}",
        current_metric.as_deref().unwrap_or("(empty table)")
    );
    let metric_is_current = current_metric.as_deref() == Some(VECTOR_DISTANCE_METRIC);
    let table_layout_is_current = current_dim == Some(new_dim) && metric_is_current;
    let should_recreate_table = if mode.stale_only {
        !table_layout_is_current
    } else {
        resume_checkpoint.is_none() || !table_layout_is_current
    };
    if mode.stale_only
        && should_recreate_table
        && reindex_target_narrows_store(db, &target)
            .context("failed to evaluate targeted stale reindex safety")?
    {
        bail!(
            "vector table layout is stale (metric/dim mismatch) and requires a full rebuild; \
             a targeted reindex (--drawer-id/--project/--wing/--room/--limit) cannot rebuild \
             the table without dropping non-target vectors. Re-run a full \
             `mempal reindex --from-config --stale` (no target filters) to rebuild and \
             re-embed the whole store."
        );
    }
    // #302 atomicity: a metric/dim-change reindex DROPs the old drawer_vectors
    // up front, then embeds. If every batch then fails (embedder fully down),
    // the table is left empty and search silently degrades to BM25-only. Stash
    // the old rows aside BEFORE the recreate so a reindex that embeds ZERO
    // vectors can roll them back (see finalize_reindex_atomicity below). vec0
    // has no ALTER TABLE RENAME (sqlite-vec 0.1.9 xRename=0,
    // asg017/sqlite-vec#43), so this strategy-A "stage the old data aside" uses
    // a temp-table copy rather than embed-into-staging + rename.
    let reindex_stash = if should_recreate_table {
        if resume_checkpoint.is_some() {
            println!(
                "resume checkpoint ignored because drawer_vectors metric or dimension is stale"
            );
            resume_checkpoint = None;
        }
        let stash = db
            .stash_vectors_before_recreate()
            .context("failed to stash existing vectors before recreate")?;
        println!("recreating drawer_vectors with {new_dim} dimensions...");
        db.recreate_vectors_table(new_dim)
            .context("failed to recreate vectors table")?;
        stash
    } else if mode.stale_only {
        println!("stale-only reindex preserving existing drawer_vectors table");
        None
    } else {
        println!("resume checkpoint found; preserving existing drawer_vectors table");
        None
    };
    let progress_store_for_reindex = progress_store.clone();
    let target_fingerprint_for_reindex = target_fingerprint.clone();
    let embed_result: Result<()> = async move {
    if mode.stale_only && resume_checkpoint.is_none() {
        let policy = BatchRetryPolicy {
            interval: std::time::Duration::from_secs(config.embed.retry.interval_secs),
            max_retries: batch.max_batch_retries,
        };
        return reindex_stale_batches(
            db,
            embedder.as_ref(),
            embedder_name,
            &target_fingerprint_for_reindex,
            batch.batch_size,
            target,
            policy,
        )
        .await;
    }
    let mut drawers = reindex_rows(db).context("failed to load active drawers for reindex")?;
    if mode.stale_only {
        drawers.retain(|row| {
            reindex_row_is_stale(db, row, &target_fingerprint_for_reindex).unwrap_or(true)
        });
    }
    let total = drawers.len();
    println!("re-embedding {total} drawers...");
    let mut done = 0;
    let mut last_processed: Option<(String, i64)> = None;
    let mut active_source: Option<String> = None;
    let test_stop_after = std::env::var("MEMPAL_TEST_REINDEX_STOP_AFTER")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());
    for row in drawers {
        if should_skip_reindex_row(
            resume_checkpoint.as_ref(),
            &row.source_path,
            row.chunk_index,
        ) {
            done += 1;
            last_processed = Some((row.source_path.clone(), row.chunk_index));
            active_source = Some(row.source_path.clone());
            continue;
        }
        if let Some(prev_src) = active_source.as_ref()
            && prev_src != &row.source_path
            && let Some((sp, ci)) = last_processed.as_ref()
            && sp == prev_src
        {
            progress_store_for_reindex
                .mark_done(sp, Some(*ci), embedder_name)
                .context("failed to mark completed reindex source")?;
        }
        active_source = Some(row.source_path.clone());
        let single_input = [row.content.as_str()];
        let embed_future = embedder.embed(&single_input);
        let vectors = tokio::select! { _ = tokio::signal::ctrl_c() => { if let Some((sp, ci)) = last_processed.as_ref() { progress_store_for_reindex.mark_paused(sp, Some(*ci), embedder_name).context("failed to persist paused reindex checkpoint")?; } bail!("reindex interrupted; resume with `mempal reindex --embedder {embedder_name} --resume`"); } result = embed_future => result.context("embedding failed during reindex")? };
        let vector = vectors
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embedder returned no vector during reindex"))?;
        db.conn()
            .execute("DELETE FROM drawer_vectors WHERE id = ?1", [&row.id])
            .with_context(|| format!("failed to clear existing vector for {}", row.id))?;
        db.insert_vector_with_project(&row.id, &vector, row.project_id.as_deref())
            .with_context(|| format!("failed to insert vector for {}", row.id))?;
        record_reindex_metadata(
            db,
            &row.id,
            CURRENT_VECTOR_INDEX_VERSION,
            &target_fingerprint_for_reindex,
        )
        .with_context(|| format!("failed to record reindex metadata for {}", row.id))?;
        progress_store_for_reindex
            .upsert_running(&row.source_path, Some(row.chunk_index), embedder_name)
            .context("failed to persist reindex checkpoint")?;
        done += 1;
        last_processed = Some((row.source_path.clone(), row.chunk_index));
        println!("  {done}/{total}");
        if test_stop_after.is_some_and(|limit| done >= limit) {
            progress_store_for_reindex
                .mark_paused(&row.source_path, Some(row.chunk_index), embedder_name)
                .context("failed to persist paused reindex checkpoint")?;
            bail!("reindex interrupted for test after {done} drawers");
        }
    }
    if let Some((sp, ci)) = last_processed.as_ref() {
        progress_store_for_reindex
            .mark_done(sp, Some(*ci), embedder_name)
            .context("failed to finalize reindex checkpoint")?;
    }
    println!("reindex complete: {total} drawers, {new_dim}d vectors");
    Ok(())
    }
    .await;

    match finalize_reindex_atomicity(db, reindex_stash, embed_result) {
        Ok(()) => {
            // Safety net: successful embedder/vector reindex runs should settle
            // any still-running checkpoints to `done`, but only for rows that
            // are already current. This preserves the #325 invariant: promote
            // `running` -> `done` only, and never resurrect `failed` rows.
            progress_store
                .finalize_completed_running_rows(CURRENT_VECTOR_INDEX_VERSION, &target_fingerprint)
                .context("failed to finalize completed reindex checkpoints")?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

async fn preview_reindex_stale_vector_work(
    db: &Database,
    config: &Config,
    embedder_name: &str,
    target: ReindexVectorTarget,
) -> Result<usize> {
    let embedder = build_specific_embedder(config, embedder_name).await?;
    let target_fingerprint = config
        .embed
        .vector_embedder_fingerprint(embedder_name, embedder.dimensions());
    build_reindex_stale_pending_rows(db, &target_fingerprint, &target)
        .context("failed to snapshot stale reindex work list")
}

/// Decide the fate of a recreate-reindex's staged old vectors (#302 atomicity).
///
/// `stash` is `Some` only when the reindex recreated `drawer_vectors` (a
/// metric/dim change). The post-#301 skip-continue loop can finish "successfully"
/// having embedded anywhere from zero to all batches, so the swap decision is by
/// row count, not exit status:
/// - at least one vector embedded: keep the freshly populated table, drop the
///   stash, and pass the embed result through (partial progress from #301 skips
///   is retained).
/// - zero vectors embedded (total embedder outage): restore the old rows so the
///   store is left exactly as it started, and report failure so an empty index
///   is never silently installed.
///
/// When `stash` is `None` (no recreate happened) the embed result passes through
/// unchanged.
fn finalize_reindex_atomicity(
    db: &Database,
    stash: Option<ReindexVectorStash>,
    embed_result: Result<()>,
) -> Result<()> {
    let Some(stash) = stash else {
        return embed_result;
    };
    let embedded = db
        .vector_row_count()
        .context("failed to count embedded vectors after reindex")?;
    if embedded == 0 {
        let preserved = stash.row_count();
        db.restore_vectors_from_stash(&stash)
            .context("failed to restore previous vectors after empty reindex")?;
        let message = format!(
            "reindex embedded 0 vectors (embedder unavailable); restored the previous \
             {preserved}-row drawer_vectors table unchanged. Re-run `mempal reindex` once the \
             embedder is reachable."
        );
        return Err(match embed_result {
            Err(error) => error.context(message),
            Ok(()) => anyhow::anyhow!("{message}"),
        });
    }
    db.discard_reindex_stash()
        .context("failed to discard reindex stash after successful reindex")?;
    embed_result
}

fn reindex_target_narrows_store(db: &Database, target: &ReindexVectorTarget) -> Result<bool> {
    if target.has_scope_filter() {
        return Ok(true);
    }
    let Some(limit) = target.limit else {
        return Ok(false);
    };
    let active_drawer_count = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM drawers WHERE deleted_at IS NULL",
            [],
            |row| row.get::<_, i64>(0),
        )
        .context("failed to count active drawers")?;
    let active_drawer_count =
        usize::try_from(active_drawer_count).context("active drawer count is invalid")?;
    Ok(limit < active_drawer_count)
}

/// Default number of retries for a single stale-reindex batch's embed call
/// before the batch is skipped. With the default `[embed.retry].interval_secs`
/// of 2s this absorbs blips up to ~30s while still bounding the run.
const DEFAULT_MAX_BATCH_RETRIES: usize = 15;

/// Bounded retry policy for stale-batch reindex embed calls.
///
/// The ingest path retries embeds *indefinitely* so NEW data is never lost.
/// Reindex is different: it rebuilds an index over data already stored raw, so
/// it MUST terminate. A batch that keeps failing is therefore retried a bounded
/// number of times, then skipped (its drawers stay stale and are picked up by a
/// later `mempal reindex --stale` re-run) rather than blocking the whole run.
#[derive(Debug, Clone, Copy)]
struct BatchRetryPolicy {
    /// Fixed delay between attempts; mirrors `[embed.retry].interval_secs`
    /// (no backoff), matching the ingest path's retry cadence.
    interval: std::time::Duration,
    /// Retries after the first attempt before a batch is skipped.
    max_retries: usize,
}

/// CLI-supplied tuning for the stale-batch reindex loop (`reindex --stale`).
/// Bundled so the embedder reindex entry point stays within a sane arity.
#[derive(Debug, Clone, Copy)]
struct StaleBatchTuning {
    /// Drawers re-embedded per batch (`--batch-size`, default 1000).
    batch_size: usize,
    /// Max retries for a batch's embed call before it is skipped
    /// (`--max-batch-retries`, default `DEFAULT_MAX_BATCH_RETRIES`).
    max_batch_retries: usize,
}

#[derive(Debug, Clone, Copy)]
struct ReindexEmbedderMode {
    resume: bool,
    stale_only: bool,
    dry_run: bool,
}

/// Embed one stale batch, retrying at the policy's fixed interval (no backoff,
/// mirroring the ingest path) up to `policy.max_retries` before giving up.
///
/// Retries on ANY error — including ones `EmbedError::is_retryable` classifies
/// as terminal (e.g. a read timeout surfacing as `DecodeResponse`, the #301
/// failure) — because a reindex batch must survive blips regardless of error
/// class; a genuinely persistent failure is bounded by the cap and skipped by
/// the caller. A vector-count mismatch is treated the same way (retry, then
/// skip). Returns the embedded vectors, or the last error once the cap is
/// exhausted so the caller can skip-and-continue.
async fn embed_stale_batch_with_retry(
    embedder: &dyn Embedder,
    texts: &[&str],
    expected_len: usize,
    policy: BatchRetryPolicy,
) -> Result<Vec<Vec<f32>>> {
    let mut attempt = 0usize;
    loop {
        let retry_error = match embedder.embed(texts).await {
            Ok(vectors) if vectors.len() == expected_len => return Ok(vectors),
            Ok(vectors) => anyhow::anyhow!(
                "embedder returned {} vectors for {expected_len} stale drawers",
                vectors.len()
            ),
            Err(error) => {
                anyhow::Error::new(error).context("embedding failed during stale batch reindex")
            }
        };
        if attempt >= policy.max_retries {
            return Err(retry_error);
        }
        attempt += 1;
        tokio::time::sleep(policy.interval).await;
    }
}

async fn reindex_stale_batches(
    db: &Database,
    embedder: &dyn Embedder,
    embedder_name: &str,
    target_fingerprint: &str,
    batch_size: usize,
    target: ReindexVectorTarget,
    policy: BatchRetryPolicy,
) -> Result<()> {
    if batch_size == 0 {
        bail!("--batch-size must be greater than 0");
    }
    if target.limit == Some(0) {
        bail!("--limit must be greater than 0");
    }
    println!("stale-only reindex batch size: {batch_size}");
    if !target.drawer_ids.is_empty() {
        println!("target drawer ids: {}", target.drawer_ids.join(", "));
    }
    if let Some(project_id) = target.project_id.as_deref() {
        println!("target project: {project_id}");
    }
    if let Some(wing) = target.wing.as_deref() {
        println!("target wing: {wing}");
    }
    if let Some(room) = target.room.as_deref() {
        println!("target room: {room}");
    }
    if let Some(limit) = target.limit {
        println!("target limit: {limit}");
    }
    let mut processed = 0usize;
    let mut skipped_concurrent_update = 0usize;
    let mut skipped_failed_batches = 0usize;
    let mut skipped_failed_drawers = 0usize;
    let progress_store = ReindexProgressStore::new(db.path());
    let mut failed_sources = std::collections::HashSet::new();
    // Drawers from batches that failed past the retry cap. They keep no fresh
    // vector (stay stale), so they MUST be excluded from later batch queries:
    // the stale selection re-runs every iteration, and without exclusion it
    // would keep re-selecting them and the loop would never terminate. They are
    // staged in a connection-local TEMP table (excluded via a subquery) instead
    // of an in-memory list expanded into `NOT IN (?, ?, ...)` placeholders,
    // whose bind-variable count would overflow SQLITE_MAX_VARIABLE_NUMBER
    // (32766) on a pathological run and crash the skip-continue (issue #304).
    ensure_reindex_skipped_table(db).context("failed to create reindex skip table")?;
    db.conn()
        .execute_batch(&format!("DELETE FROM {REINDEX_SKIPPED_TABLE};"))
        .context("failed to reset reindex skip table")?;
    build_reindex_stale_pending_rows(db, target_fingerprint, &target)
        .context("failed to snapshot stale reindex work list")?;
    let mut batch_index = 0usize;
    loop {
        let rows = pull_reindex_pending_batch(db, batch_size)
            .context("failed to load stale reindex batch")?;
        if rows.is_empty() {
            break;
        }
        batch_index += 1;
        let texts = rows
            .iter()
            .map(|row| row.content.as_str())
            .collect::<Vec<_>>();
        let vectors = match embed_stale_batch_with_retry(embedder, &texts, rows.len(), policy).await
        {
            Ok(vectors) => vectors,
            Err(error) => {
                // Skip-and-continue: a batch still failing after the bounded
                // retry is logged and skipped so the run can finish every other
                // batch. Unlike ingest (which blocks forever to never lose NEW
                // data), reindex rebuilds an index over data already stored raw,
                // so it must terminate; the skipped drawers stay stale and a
                // later `mempal reindex --stale` re-run retries them.
                eprintln!(
                    "warning: batch {batch_index}: embed failed after {} attempt(s); \
                     skipping {} drawer(s) (they stay stale — re-run \
                     `mempal reindex --stale` to retry): {error:#}",
                    policy.max_retries + 1,
                    rows.len()
                );
                skipped_failed_batches += 1;
                skipped_failed_drawers += rows.len();
                let failed_ids: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
                stage_reindex_skipped_ids(db, &failed_ids)
                    .context("failed to record skipped drawer ids")?;
                delete_reindex_pending_ids(db, &failed_ids)
                    .context("failed to delete skipped stale reindex work")?;
                for (source_path, chunk_index) in reindex_source_checkpoints(&rows) {
                    progress_store
                        .mark_failed(&source_path, Some(chunk_index), embedder_name)
                        .context("failed to record failed reindex checkpoint")?;
                    failed_sources.insert(source_path);
                }
                continue;
            }
        };
        let stats = write_reindex_vector_batch(db, &rows, &vectors, target_fingerprint)
            .context("failed to write stale reindex batch")?;
        let pulled_ids = rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
        delete_reindex_pending_ids(db, &pulled_ids)
            .context("failed to delete completed stale reindex work")?;
        processed += stats.reindexed;
        skipped_concurrent_update += stats.skipped_concurrent_update;
        if stats.skipped_concurrent_update == 0 {
            println!(
                "batch {batch_index}: re-embedded {} stale/new drawers (total {processed})",
                stats.reindexed
            );
        } else {
            println!(
                "batch {batch_index}: re-embedded {} stale/new drawers, skipped {} concurrent updates (total {processed})",
                stats.reindexed, stats.skipped_concurrent_update
            );
        }
        for (source_path, chunk_index) in reindex_source_checkpoints(&rows) {
            let checkpoint = if failed_sources.contains(source_path.as_str()) {
                progress_store.mark_failed(&source_path, Some(chunk_index), embedder_name)
            } else {
                progress_store.upsert_running(&source_path, Some(chunk_index), embedder_name)
            };
            checkpoint.context("failed to persist reindex checkpoint")?;
        }
    }
    progress_store
        .finalize_completed_running_rows(CURRENT_VECTOR_INDEX_VERSION, target_fingerprint)
        .context("failed to finalize completed reindex sources")?;
    if skipped_concurrent_update == 0 {
        println!("stale reindex complete: {processed} drawers");
    } else {
        println!(
            "stale reindex complete: {processed} drawers ({skipped_concurrent_update} skipped due to concurrent updates)"
        );
    }
    // Surface failed-batch skips (mirrors the #297 `skipped_stale_content`
    // counter): always reported so an operator can see them. Skips are NOT
    // fatal — the run still exits 0 on completion because the skipped drawers
    // remain stale and are picked up by a later `mempal reindex --stale`.
    println!("skipped failed batches: {skipped_failed_batches}");
    println!("skipped failed drawers: {skipped_failed_drawers}");
    Ok(())
}

async fn reindex_command_sources(
    db: &Database,
    config: &Config,
    stale: bool,
    force: bool,
    dry_run: bool,
) -> Result<()> {
    if stale && force {
        bail!("--stale and --force are mutually exclusive");
    }
    let mode = if force {
        ReindexMode::Force
    } else {
        ReindexMode::Stale
    };
    let options = ReindexOptions { mode, dry_run };
    let report = if dry_run {
        reindex_sources(db, &NoopEmbedder, options)
            .await
            .context("failed to plan reindex")?
    } else {
        let embedder = build_embedder(config).await?;
        println!("embedder: {} ({}d)", embedder.name(), embedder.dimensions());
        reindex_sources(db, &*embedder, options)
            .await
            .context("failed to reindex sources")?
    };
    print_reindex_report(report, dry_run);
    Ok(())
}

fn print_reindex_report(report: ReindexReport, dry_run: bool) {
    if dry_run {
        println!(
            "would reprocess {} drawers from {} sources",
            report.candidate_drawers, report.candidate_sources
        );
        if report.skipped_missing_drawers > 0 {
            println!(
                "would skip {} drawers from {} missing sources",
                report.skipped_missing_drawers, report.skipped_missing_sources
            );
        }
        return;
    }
    println!(
        "reindex complete: processed {} sources, {} drawers selected, {} chunks written, skipped {} existing chunks, skipped {} missing-source drawers",
        report.processed_sources,
        report.candidate_drawers,
        report.reingested_chunks,
        report.skipped_existing_chunks,
        report.skipped_missing_drawers
    );
}

async fn knowledge_command(
    db: &Database,
    config: &Config,
    command: KnowledgeCommands,
) -> Result<()> {
    match command {
        KnowledgeCommands::Distill {
            statement,
            content,
            tier,
            supporting_refs,
            wing,
            room,
            domain,
            field,
            cwd,
            scope_constraints,
            counterexample_refs,
            teaching_refs,
            intent_tags,
            workflow_bias,
            tool_needs,
            importance,
            dry_run,
        } => {
            let trigger_hints = build_trigger_hints(intent_tags, workflow_bias, tool_needs);
            let request = DistillRequest {
                statement,
                content,
                tier,
                supporting_refs,
                wing,
                room,
                domain,
                field,
                cwd,
                scope_constraints,
                counterexample_refs,
                teaching_refs,
                trigger_hints,
                importance,
                dry_run,
            };
            let outcome = match prepare_distill(db, request)? {
                DistillPlan::Done(outcome) => outcome,
                DistillPlan::Create(prepared) => {
                    let embedder = build_embedder(config).await?;
                    let vector = embedder
                        .embed(&[prepared.content.as_str()])
                        .await
                        .context("failed to embed distilled knowledge")?
                        .into_iter()
                        .next()
                        .context("embedder returned no vector")?;
                    commit_distill(db, *prepared, &vector)?
                }
            };
            if outcome.dry_run {
                println!("dry_run=true drawer_id={}", outcome.drawer_id);
                return Ok(());
            }
            println!(
                "drawer_id={} created={}",
                outcome.drawer_id, outcome.created
            );
        }
        KnowledgeCommands::Promote {
            drawer_id,
            status,
            verification_refs,
            reason,
            reviewer,
        } => {
            let outcome = promote_knowledge(
                db,
                PromoteRequest {
                    drawer_id: drawer_id.clone(),
                    status,
                    verification_refs,
                    reason,
                    reviewer,
                    allow_counterexamples: false,
                    enforce_gate: false,
                },
            )?;
            println!(
                "promoted {}: {} -> {}",
                drawer_id, outcome.old_status, outcome.new_status
            );
        }
        KnowledgeCommands::Demote {
            drawer_id,
            status,
            evidence_refs,
            reason,
            reason_type,
        } => {
            let outcome = demote_knowledge(
                db,
                DemoteRequest {
                    drawer_id: drawer_id.clone(),
                    status,
                    evidence_refs,
                    reason,
                    reason_type,
                },
            )?;
            println!(
                "demoted {}: {} -> {}",
                drawer_id, outcome.old_status, outcome.new_status
            );
        }
        KnowledgeCommands::Gate {
            drawer_id,
            target_status,
            reviewer,
            allow_counterexamples,
            format,
        } => {
            let report = evaluate_gate_by_id(
                db,
                &drawer_id,
                target_status.as_deref(),
                reviewer.as_deref(),
                allow_counterexamples,
            )?;
            match format.as_str() {
                "plain" => print_gate_report(&report),
                "json" => println!(
                    "{}",
                    serde_json::to_string_pretty(&report)
                        .context("failed to serialize gate report")?
                ),
                other => bail!("unsupported gate format: {other}"),
            }
        }
        KnowledgeCommands::Policy { format } => {
            let policy = promotion_policy();
            match format.as_str() {
                "plain" => print_promotion_policy(&policy),
                "json" => println!(
                    "{}",
                    serde_json::to_string_pretty(&policy)
                        .context("failed to serialize knowledge policy")?
                ),
                other => bail!("unsupported policy format: {other}"),
            }
        }
        KnowledgeCommands::PublishAnchor {
            drawer_id,
            to,
            target_anchor_id,
            cwd,
            reason,
            reviewer,
        } => {
            let outcome = publish_anchor(
                db,
                PublishAnchorRequest {
                    drawer_id: drawer_id.clone(),
                    to,
                    target_anchor_id,
                    cwd,
                    reason,
                    reviewer,
                },
            )?;
            println!(
                "published {}: {}:{} -> {}:{}",
                drawer_id,
                outcome.old_anchor_kind,
                outcome.old_anchor_id,
                outcome.new_anchor_kind,
                outcome.new_anchor_id
            );
        }
    }
    Ok(())
}

async fn knowledge_card_command(
    db: &Database,
    config: &Config,
    command: KnowledgeCardCommands,
) -> Result<()> {
    match command {
        KnowledgeCardCommands::Create {
            id,
            statement,
            content,
            tier,
            status,
            domain,
            field,
            anchor_kind,
            anchor_id,
            parent_anchor_id,
            scope_constraints,
            intent_tags,
            workflow_bias,
            tool_needs,
            format,
        } => {
            let tier = parse_knowledge_tier(&tier)?;
            let status = parse_knowledge_status(&status)?;
            let domain = parse_domain(&domain)?;
            let anchor_kind = parse_anchor_kind(&anchor_kind)?;
            let trigger_hints = build_trigger_hints(intent_tags, workflow_bias, tool_needs);
            let id = id.unwrap_or_else(|| {
                stable_cli_id(
                    "card",
                    &[
                        statement.as_str(),
                        content.as_str(),
                        knowledge_tier_slug(&tier),
                        knowledge_status_slug(&status),
                        domain_slug(&domain),
                        field.as_str(),
                        anchor_kind_slug(&anchor_kind),
                        anchor_id.as_str(),
                    ],
                )
            });
            let now = current_timestamp();
            let card = KnowledgeCard {
                id: id.clone(),
                statement,
                content,
                tier,
                status,
                domain,
                field,
                anchor_kind,
                anchor_id,
                parent_anchor_id,
                scope_constraints,
                trigger_hints,
                auto_generated: false,
                crystallization_score: None,
                source_drawer_ids: Vec::new(),
                created_at: now.clone(),
                updated_at: now,
            };
            db.insert_knowledge_card(&card)
                .context("failed to insert knowledge card")?;
            match format.as_str() {
                "plain" => println!("card_id={id} created=true"),
                "json" => println!(
                    "{}",
                    serde_json::to_string_pretty(&card)
                        .context("failed to serialize knowledge card")?
                ),
                other => bail!("unsupported knowledge-card format: {other}"),
            }
        }
        KnowledgeCardCommands::Get { card_id, format } => {
            let card = db
                .get_knowledge_card(&card_id)
                .context("failed to get knowledge card")?
                .with_context(|| format!("knowledge card not found: {card_id}"))?;
            print_knowledge_card(&card, &format)?;
        }
        KnowledgeCardCommands::List {
            tier,
            status,
            domain,
            field,
            anchor_kind,
            anchor_id,
            format,
        } => {
            let filter = KnowledgeCardFilter {
                tier: tier.as_deref().map(parse_knowledge_tier).transpose()?,
                status: status.as_deref().map(parse_knowledge_status).transpose()?,
                domain: domain.as_deref().map(parse_domain).transpose()?,
                field,
                anchor_kind: anchor_kind.as_deref().map(parse_anchor_kind).transpose()?,
                anchor_id,
                ..KnowledgeCardFilter::default()
            };
            let cards = db
                .list_knowledge_cards(&filter)
                .context("failed to list knowledge cards")?;
            print_knowledge_cards(&cards, &format)?;
        }
        KnowledgeCardCommands::Retrieve {
            query,
            domain,
            field,
            cwd,
            top_k,
            evidence_top_k,
            format,
        } => {
            if top_k == 0 {
                bail!("--top-k must be greater than 0");
            }
            let domain = parse_domain(&domain)?;
            let cwd = cwd.unwrap_or(env::current_dir().context("failed to read current dir")?);
            let embedder = build_embedder(config).await?;
            let results = retrieve_knowledge_cards(
                db,
                &*embedder,
                KnowledgeCardRetrievalRequest {
                    query,
                    domain,
                    field,
                    cwd,
                    top_k,
                    evidence_top_k,
                },
            )
            .await
            .context("failed to retrieve knowledge cards")?;
            print_retrieved_knowledge_cards(&results, &format)?;
        }
        KnowledgeCardCommands::Link {
            card_id,
            evidence_drawer_id,
            role,
            note,
            id,
        } => {
            let role = parse_knowledge_evidence_role(&role)?;
            let id = id.unwrap_or_else(|| {
                stable_cli_id(
                    "link",
                    &[
                        card_id.as_str(),
                        evidence_drawer_id.as_str(),
                        knowledge_evidence_role_slug(&role),
                        note.as_deref().unwrap_or(""),
                    ],
                )
            });
            let link = KnowledgeEvidenceLink {
                id: id.clone(),
                card_id,
                evidence_drawer_id,
                role,
                note,
                created_at: current_timestamp(),
            };
            db.insert_knowledge_evidence_link(&link)
                .context("failed to insert knowledge evidence link")?;
            println!("link_id={id} created=true");
        }
        KnowledgeCardCommands::Event {
            card_id,
            event_type,
            reason,
            from_status,
            to_status,
            actor,
            metadata_json,
            id,
        } => {
            let event_type = parse_knowledge_event_type(&event_type)?;
            let from_status = from_status
                .as_deref()
                .map(parse_knowledge_status)
                .transpose()?;
            let to_status = to_status
                .as_deref()
                .map(parse_knowledge_status)
                .transpose()?;
            let metadata = metadata_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .context("failed to parse --metadata-json")?;
            let created_at = current_timestamp();
            let id = id.unwrap_or_else(|| {
                stable_cli_id(
                    "event",
                    &[
                        card_id.as_str(),
                        knowledge_event_type_slug(&event_type),
                        reason.as_str(),
                        created_at.as_str(),
                    ],
                )
            });
            let event = KnowledgeCardEvent {
                id: id.clone(),
                card_id,
                event_type,
                from_status,
                to_status,
                reason,
                actor,
                metadata,
                created_at,
            };
            db.append_knowledge_event(&event)
                .context("failed to append knowledge card event")?;
            println!("event_id={id} created=true");
        }
        KnowledgeCardCommands::Events { card_id, format } => {
            let events = db
                .knowledge_events(&card_id)
                .context("failed to list knowledge card events")?;
            print_knowledge_card_events(&events, &format)?;
        }
        KnowledgeCardCommands::Gate {
            card_id,
            target_status,
            reviewer,
            allow_counterexamples,
            format,
        } => {
            let report = evaluate_card_gate_by_id(
                db,
                &card_id,
                target_status.as_deref(),
                reviewer.as_deref(),
                allow_counterexamples,
            )
            .context("failed to evaluate knowledge card gate")?;
            print_knowledge_card_gate_report(&report, &format)?;
        }
        KnowledgeCardCommands::Promote {
            card_id,
            status,
            verification_refs,
            reason,
            reviewer,
            allow_counterexamples,
            enforce_gate,
            format,
        } => {
            let outcome = promote_card(
                db,
                PromoteCardRequest {
                    card_id,
                    status,
                    verification_refs,
                    reason,
                    reviewer,
                    allow_counterexamples,
                    enforce_gate,
                },
            )
            .context("failed to promote knowledge card")?;
            print_knowledge_card_promote_outcome(&outcome, &format)?;
        }
        KnowledgeCardCommands::Demote {
            card_id,
            status,
            evidence_refs,
            reason,
            reason_type,
            format,
        } => {
            let outcome = demote_card(
                db,
                DemoteCardRequest {
                    card_id,
                    status,
                    evidence_refs,
                    reason,
                    reason_type,
                },
            )
            .context("failed to demote knowledge card")?;
            print_knowledge_card_demote_outcome(&outcome, &format)?;
        }
        KnowledgeCardCommands::BackfillPlan {
            tier,
            status,
            domain,
            field,
            anchor_kind,
            anchor_id,
            format,
        } => {
            let filter = KnowledgeCardFilter {
                tier: tier.as_deref().map(parse_knowledge_tier).transpose()?,
                status: status.as_deref().map(parse_knowledge_status).transpose()?,
                domain: domain.as_deref().map(parse_domain).transpose()?,
                field,
                anchor_kind: anchor_kind.as_deref().map(parse_anchor_kind).transpose()?,
                anchor_id,
                ..KnowledgeCardFilter::default()
            };
            let report = build_backfill_report(db, &filter)
                .context("failed to build knowledge card backfill plan")?;
            print_knowledge_card_backfill_report(&report, &format)?;
        }
        KnowledgeCardCommands::BackfillApply {
            tier,
            status,
            domain,
            field,
            anchor_kind,
            anchor_id,
            execute,
            format,
        } => {
            let filter = KnowledgeCardFilter {
                tier: tier.as_deref().map(parse_knowledge_tier).transpose()?,
                status: status.as_deref().map(parse_knowledge_status).transpose()?,
                domain: domain.as_deref().map(parse_domain).transpose()?,
                field,
                anchor_kind: anchor_kind.as_deref().map(parse_anchor_kind).transpose()?,
                anchor_id,
                ..KnowledgeCardFilter::default()
            };
            let result = apply_backfill(db, &filter, KnowledgeCardBackfillApplyOptions { execute })
                .context("failed to apply knowledge card backfill")?;
            print_knowledge_card_backfill_apply_result(&result, &format)?;
        }
    }
    Ok(())
}

async fn phase3_command(db: &Database, config: &Config, command: Phase3Commands) -> Result<()> {
    match command {
        Phase3Commands::Adoption { command } => phase3_adoption_command(db, command),
        Phase3Commands::Evaluator { command } => phase3_evaluator_command(command),
        Phase3Commands::DefaultProposal {
            candidate,
            rollback_criteria,
            format,
        } => {
            let report = phase3_default_proposal(db, &candidate, rollback_criteria)?;
            print_card_context_default_proposal(&report, &format)
        }
        Phase3Commands::DefaultControl {
            candidate,
            enable,
            disable,
            rollback_criteria,
            format,
        } => {
            let report =
                phase3_default_control(db, &candidate, enable, disable, rollback_criteria)?;
            print_phase3_default_control(&report, &format)
        }
        Phase3Commands::RollbackControl {
            candidate,
            execute,
            format,
        } => {
            let report = phase3_rollback_control(db, &candidate, execute)?;
            print_phase3_rollback_control(&report, &format)
        }
        Phase3Commands::Gate { candidate, format } => {
            let report = phase3_gate_report(db, &candidate)?;
            print_phase3_gate_report(&report, &format)
        }
        Phase3Commands::Readiness { candidate, format } => {
            let report = phase3_readiness_report(db, &candidate)?;
            print_phase3_readiness_report(&report, &format)
        }
        Phase3Commands::ResearchValidatePlan { path, format } => {
            let report = validate_research_adapter_plan(&path)?;
            print_research_adapter_plan(&report, &format)
        }
        Phase3Commands::ResearchIngestPlan {
            path,
            execute,
            format,
        } => research_ingest_plan_command(db, config, &path, execute, &format).await,
    }
}

fn phase3_default_proposal(
    db: &Database,
    candidate: &str,
    rollback_criteria: Vec<String>,
) -> Result<CardContextDefaultProposalReport> {
    match candidate {
        "card-context" => {
            let events = db
                .list_runtime_adoption_events(
                    &RuntimeAdoptionFilter {
                        track: Some(RuntimeAdoptionTrack::CardContext),
                        feature: Some("include_cards".to_string()),
                    },
                    10_000,
                )
                .context("failed to list runtime adoption events")?;
            Ok(card_context_default_proposal(&events, rollback_criteria))
        }
        other => bail!("unsupported phase3 default proposal candidate: {other}"),
    }
}

#[derive(Debug, Serialize)]
struct Phase3DefaultControlReport {
    writes: bool,
    candidate: String,
    requested: String,
    applied: bool,
    include_cards_default: bool,
    proposal: Option<CardContextDefaultProposalReport>,
    reasons: Vec<String>,
}

fn phase3_default_control(
    db: &Database,
    candidate: &str,
    enable: bool,
    disable: bool,
    rollback_criteria: Vec<String>,
) -> Result<Phase3DefaultControlReport> {
    if enable == disable {
        bail!("exactly one of --enable or --disable is required");
    }
    match candidate {
        "card-context" => {
            let mut config = Config::load().context("failed to load config")?;
            if disable {
                config.context.include_cards_default = false;
                config.save_default().context("failed to save config")?;
                return Ok(Phase3DefaultControlReport {
                    writes: true,
                    candidate: candidate.to_string(),
                    requested: "disable".to_string(),
                    applied: true,
                    include_cards_default: false,
                    proposal: None,
                    reasons: vec!["card context default disabled".to_string()],
                });
            }

            let proposal = phase3_default_proposal(db, candidate, rollback_criteria)?;
            if !proposal.proposal_ready {
                return Ok(Phase3DefaultControlReport {
                    writes: false,
                    candidate: candidate.to_string(),
                    requested: "enable".to_string(),
                    applied: false,
                    include_cards_default: config.context.include_cards_default,
                    proposal: Some(proposal),
                    reasons: vec!["default-on proposal is not ready".to_string()],
                });
            }

            config.context.include_cards_default = true;
            config.save_default().context("failed to save config")?;
            Ok(Phase3DefaultControlReport {
                writes: true,
                candidate: candidate.to_string(),
                requested: "enable".to_string(),
                applied: true,
                include_cards_default: true,
                proposal: Some(proposal),
                reasons: vec!["card context default enabled".to_string()],
            })
        }
        other => bail!("unsupported phase3 default-control candidate: {other}"),
    }
}

fn phase3_rollback_control(
    db: &Database,
    candidate: &str,
    execute: bool,
) -> Result<CardContextRollbackControlReport> {
    match candidate {
        "card-context" => {
            let mut config = Config::load().context("failed to load config")?;
            let events = db
                .list_runtime_adoption_events(
                    &RuntimeAdoptionFilter {
                        track: Some(RuntimeAdoptionTrack::CardContext),
                        feature: Some("include_cards".to_string()),
                    },
                    10_000,
                )
                .context("failed to list runtime adoption events")?;
            let report = card_context_rollback_control(
                &events,
                config.context.include_cards_default,
                execute,
            );
            if report.applied {
                config.context.include_cards_default = false;
                config.save_default().context("failed to save config")?;
            }
            Ok(report)
        }
        other => bail!("unsupported phase3 rollback-control candidate: {other}"),
    }
}

fn phase3_evaluator_command(command: Phase3EvaluatorCommands) -> Result<()> {
    match command {
        Phase3EvaluatorCommands::Advise {
            evaluator_id,
            subject_kind,
            subject_id,
            proposed_action,
            evidence_refs,
            counterexample_refs,
            risk_notes,
            note,
            format,
        } => {
            let report = evaluator_advice(EvaluatorAdviceInput {
                evaluator_id: evaluator_id.unwrap_or_default(),
                subject_kind,
                subject_id,
                proposed_action,
                evidence_refs,
                counterexample_refs,
                risk_notes,
                note,
            })
            .map_err(anyhow::Error::msg)?;
            print_evaluator_advice(&report, &format)
        }
    }
}

fn phase3_readiness_report(db: &Database, candidate: &str) -> Result<Phase3ReadinessReport> {
    match candidate {
        "card-context-default" => {
            let events = db
                .list_runtime_adoption_events(
                    &RuntimeAdoptionFilter {
                        track: Some(RuntimeAdoptionTrack::CardContext),
                        feature: Some("include_cards".to_string()),
                    },
                    10_000,
                )
                .context("failed to list runtime adoption events")?;
            Ok(card_context_default_readiness(&events))
        }
        other => bail!("unsupported phase3 readiness candidate: {other}"),
    }
}

fn print_card_context_default_proposal(
    report: &CardContextDefaultProposalReport,
    format: &str,
) -> Result<()> {
    match format {
        "plain" => {
            println!("writes={}", report.writes);
            println!("candidate={}", report.candidate);
            println!("proposal_ready={}", report.proposal_ready);
            println!("decision={}", report.decision);
            println!("readiness_ready={}", report.readiness.ready);
            for criterion in &report.rollback_criteria {
                println!("rollback_criterion={criterion}");
            }
            for reason in &report.reasons {
                println!("reason={reason}");
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(report)
                    .context("failed to serialize card context default proposal")?
            );
            Ok(())
        }
        other => bail!("unsupported phase3 default proposal format: {other}"),
    }
}

fn print_phase3_default_control(report: &Phase3DefaultControlReport, format: &str) -> Result<()> {
    match format {
        "plain" => {
            println!("writes={}", report.writes);
            println!("candidate={}", report.candidate);
            println!("requested={}", report.requested);
            println!("applied={}", report.applied);
            println!("include_cards_default={}", report.include_cards_default);
            for reason in &report.reasons {
                println!("reason={reason}");
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(report)
                    .context("failed to serialize phase3 default control report")?
            );
            Ok(())
        }
        other => bail!("unsupported phase3 default control format: {other}"),
    }
}

fn print_phase3_rollback_control(
    report: &CardContextRollbackControlReport,
    format: &str,
) -> Result<()> {
    match format {
        "plain" => {
            println!("writes={}", report.writes);
            println!("candidate={}", report.candidate);
            println!("execute={}", report.execute);
            println!("rollback_required={}", report.rollback_required);
            println!("applied={}", report.applied);
            println!(
                "include_cards_default_before={}",
                report.include_cards_default_before
            );
            println!(
                "include_cards_default_after={}",
                report.include_cards_default_after
            );
            println!("rollback_events={}", report.review.stats.rollbacks);
            for reason in &report.reasons {
                println!("reason={reason}");
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(report)
                    .context("failed to serialize phase3 rollback control report")?
            );
            Ok(())
        }
        other => bail!("unsupported phase3 rollback-control format: {other}"),
    }
}

fn print_phase3_readiness_report(report: &Phase3ReadinessReport, format: &str) -> Result<()> {
    match format {
        "plain" => {
            println!("writes={}", report.writes);
            println!("candidate={}", report.candidate);
            println!("ready={}", report.ready);
            println!("decision={}", report.decision);
            println!("required_track={}", report.required_track);
            println!("required_feature={}", report.required_feature);
            println!("accepted={}", report.review.stats.accepted);
            println!("rejected={}", report.review.stats.rejected);
            println!("misses={}", report.review.stats.misses);
            println!("rollbacks={}", report.review.stats.rollbacks);
            println!("contradictions={}", report.review.stats.contradictions);
            for reason in &report.reasons {
                println!("reason={reason}");
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(report)
                    .context("failed to serialize phase3 readiness report")?
            );
            Ok(())
        }
        other => bail!("unsupported phase3 readiness format: {other}"),
    }
}

fn print_evaluator_advice(report: &EvaluatorAdviceReport, format: &str) -> Result<()> {
    match format {
        "plain" => {
            println!("writes={}", report.writes);
            println!("evaluator_id={}", report.evaluator_id);
            println!("subject_kind={}", report.subject_kind);
            println!("subject_id={}", report.subject_id);
            println!("proposed_action={}", report.proposed_action);
            println!("recommendation={}", report.recommendation);
            println!("lifecycle_authority={}", report.lifecycle_authority);
            println!(
                "deterministic_gate_required={}",
                report.deterministic_gate_required
            );
            println!("requires_human_review={}", report.requires_human_review);
            for reason in &report.reasons {
                println!("reason={reason}");
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(report)
                    .context("failed to serialize evaluator advice")?
            );
            Ok(())
        }
        other => bail!("unsupported phase3 evaluator format: {other}"),
    }
}

fn build_research_ingest_plan(path: &Path) -> Result<ResearchIngestPlanReport> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read research report {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse research report {}", path.display()))?;
    Ok(build_research_ingest_plan_from_value(&value))
}

fn research_evidence_drawer(
    drawer_id: String,
    content: String,
    source_file: String,
    report_id: &str,
    finding_index: usize,
) -> Drawer {
    Drawer {
        id: drawer_id,
        content,
        wing: "mempal".to_string(),
        room: Some("research".to_string()),
        source_file: Some(source_file),
        source_type: SourceType::SystemGenerated,
        confidence: default_confidence(SourceType::SystemGenerated),
        added_at: current_timestamp(),
        chunk_index: Some(finding_index as i64),
        normalize_version: CURRENT_NORMALIZE_VERSION,
        importance: 3,
        memory_kind: MemoryKind::Evidence,
        domain: MemoryDomain::Project,
        field: "research".to_string(),
        anchor_kind: AnchorKind::Repo,
        anchor_id: anchor::LEGACY_REPO_ANCHOR_ID.to_string(),
        parent_anchor_id: None,
        provenance: Some(Provenance::Research),
        statement: Some(format!(
            "Research finding from {report_id} #{finding_index}"
        )),
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

fn print_research_ingest_plan(report: &ResearchIngestPlanReport, format: &str) -> Result<()> {
    match format {
        "plain" => {
            println!("valid={}", report.valid);
            println!("writes={}", report.writes);
            println!("report_id={}", report.report_id);
            println!("title={}", report.title);
            println!("planned_evidence_count={}", report.planned_evidence_count);
            println!("created_count={}", report.created_count);
            println!("skipped_count={}", report.skipped_count);
            println!("candidate_insight_count={}", report.candidate_insight_count);
            for error in &report.errors {
                println!("error={error}");
            }
            for drawer in &report.evidence_drawers {
                println!(
                    "drawer={} finding_index={} created={} skipped={}",
                    drawer.drawer_id, drawer.finding_index, drawer.created, drawer.skipped
                );
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(report)
                    .context("failed to serialize research ingest plan")?
            );
            Ok(())
        }
        other => bail!("unsupported phase3 research ingest format: {other}"),
    }
}

async fn research_ingest_plan_command(
    db: &Database,
    config: &Config,
    path: &Path,
    execute: bool,
    format: &str,
) -> Result<()> {
    let mut report = build_research_ingest_plan(path)?;
    if report.valid && execute {
        let existing = report
            .evidence_drawers
            .iter()
            .filter_map(|plan| {
                db.get_drawer(&plan.drawer_id)
                    .ok()
                    .flatten()
                    .map(|_| plan.drawer_id.clone())
            })
            .collect::<BTreeSet<_>>();
        let pending = report
            .evidence_drawers
            .iter()
            .enumerate()
            .filter(|(_, plan)| !existing.contains(&plan.drawer_id))
            .map(|(index, plan)| (index, plan.drawer_id.clone(), plan.content.clone()))
            .collect::<Vec<_>>();

        let mut created_indices = BTreeSet::new();
        if !pending.is_empty() {
            let embedder = build_embedder(config).await?;
            let content_refs = pending
                .iter()
                .map(|(_, _, content)| content.as_str())
                .collect::<Vec<_>>();
            let vectors = embedder
                .embed(&content_refs)
                .await
                .context("failed to embed research evidence drawers")?;

            for ((index, drawer_id, content), vector) in pending.into_iter().zip(vectors) {
                let source_file = report.evidence_drawers[index].source_file.clone();
                let drawer = research_evidence_drawer(
                    drawer_id.clone(),
                    content,
                    source_file,
                    report.report_id.as_str(),
                    index,
                );
                db.insert_drawer(&drawer).with_context(|| {
                    format!("failed to insert research evidence drawer {drawer_id}")
                })?;
                db.insert_vector(&drawer_id, &vector)
                    .with_context(|| format!("failed to insert vector for {drawer_id}"))?;
                created_indices.insert(index);
            }
        }

        for (index, plan) in report.evidence_drawers.iter_mut().enumerate() {
            plan.created = created_indices.contains(&index);
            plan.skipped = existing.contains(&plan.drawer_id);
        }
        report.created_count = report
            .evidence_drawers
            .iter()
            .filter(|plan| plan.created)
            .count();
        report.skipped_count = report
            .evidence_drawers
            .iter()
            .filter(|plan| plan.skipped)
            .count();
        report.writes = report.created_count > 0;
    }
    print_research_ingest_plan(&report, format)
}

fn phase3_adoption_command(db: &Database, command: Phase3AdoptionCommands) -> Result<()> {
    match command {
        Phase3AdoptionCommands::Guidance { format } => {
            print_runtime_adoption_guidance(&runtime_adoption_guidance(), &format)
        }
        Phase3AdoptionCommands::InstrumentationPolicy { format } => {
            print_runtime_adoption_instrumentation_policy(
                &runtime_adoption_instrumentation_policy(),
                &format,
            )
        }
        Phase3AdoptionCommands::PrepareRecord {
            track,
            signal,
            feature,
            query,
            context_hash,
            card_id,
            evaluator_id,
            research_report_id,
            note,
            metadata_json,
            id,
            format,
        } => {
            let track = parse_runtime_adoption_track(&track)?;
            let signal = parse_runtime_adoption_signal(&signal)?;
            let metadata = metadata_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .context("failed to parse --metadata-json")?;
            let plan = prepare_runtime_adoption_record(RuntimeAdoptionRecordPlanInput {
                id,
                track: runtime_adoption_track_slug(&track).to_string(),
                signal: runtime_adoption_signal_slug(&signal).to_string(),
                feature,
                query,
                context_hash,
                card_id,
                evaluator_id,
                research_report_id,
                note,
                metadata,
            });
            print_runtime_adoption_record_plan(&plan, &format)
        }
        Phase3AdoptionCommands::Capture {
            surface,
            outcome,
            query,
            context_hash,
            card_id,
            evaluator_id,
            research_report_id,
            note,
            metadata_json,
            id,
            execute,
            allow_warnings,
            format,
        } => {
            let metadata = metadata_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .context("failed to parse --metadata-json")?;
            let record_input = capture_runtime_adoption_record_input(RuntimeAdoptionCaptureInput {
                id,
                surface: surface.clone(),
                outcome: outcome.clone(),
                query,
                context_hash,
                card_id,
                evaluator_id,
                research_report_id,
                note,
                metadata,
            })
            .map_err(anyhow::Error::msg)?;
            let mut report =
                prepare_runtime_adoption_capture(surface, outcome, execute, record_input.clone());
            if execute {
                let track = parse_runtime_adoption_track(&record_input.track)?;
                let signal = parse_runtime_adoption_signal(&record_input.signal)?;
                let should_write =
                    should_write_checked_record(&report.record_quality, allow_warnings);
                let event = if should_write {
                    let event = runtime_adoption_event_from_input(record_input, track, signal);
                    db.insert_runtime_adoption_event(&event)
                        .context("failed to insert runtime adoption event")?;
                    Some(event)
                } else {
                    None
                };
                report.writes = event.is_some();
                report.record_checked = Some(RuntimeAdoptionCheckedRecordReport {
                    writes: event.is_some(),
                    blocked: event.is_none(),
                    record_quality: report.record_quality.clone(),
                    event,
                });
            }
            print_runtime_adoption_capture(&report, &format)
        }
        Phase3AdoptionCommands::CheckRecord {
            track,
            signal,
            feature,
            query,
            context_hash,
            card_id,
            evaluator_id,
            research_report_id,
            note,
            metadata_json,
            id,
            format,
        } => {
            let track = parse_runtime_adoption_track(&track)?;
            let signal = parse_runtime_adoption_signal(&signal)?;
            let metadata = metadata_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .context("failed to parse --metadata-json")?;
            let input = RuntimeAdoptionRecordPlanInput {
                id,
                track: runtime_adoption_track_slug(&track).to_string(),
                signal: runtime_adoption_signal_slug(&signal).to_string(),
                feature,
                query,
                context_hash,
                card_id,
                evaluator_id,
                research_report_id,
                note,
                metadata,
            };
            let report = check_runtime_adoption_record(&input);
            print_runtime_adoption_record_quality(&report, &format)
        }
        Phase3AdoptionCommands::RecordChecked {
            track,
            signal,
            feature,
            query,
            context_hash,
            card_id,
            evaluator_id,
            research_report_id,
            note,
            metadata_json,
            id,
            allow_warnings,
            format,
        } => {
            let track = parse_runtime_adoption_track(&track)?;
            let signal = parse_runtime_adoption_signal(&signal)?;
            let metadata = metadata_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .context("failed to parse --metadata-json")?;
            let input = RuntimeAdoptionRecordPlanInput {
                id,
                track: runtime_adoption_track_slug(&track).to_string(),
                signal: runtime_adoption_signal_slug(&signal).to_string(),
                feature,
                query,
                context_hash,
                card_id,
                evaluator_id,
                research_report_id,
                note,
                metadata,
            };
            let quality = check_runtime_adoption_record(&input);
            let should_write = should_write_checked_record(&quality, allow_warnings);
            let event = if should_write {
                let event = runtime_adoption_event_from_input(input, track, signal);
                db.insert_runtime_adoption_event(&event)
                    .context("failed to insert runtime adoption event")?;
                Some(event)
            } else {
                None
            };
            let report = RuntimeAdoptionCheckedRecordReport {
                writes: event.is_some(),
                blocked: event.is_none(),
                record_quality: quality,
                event,
            };
            print_runtime_adoption_checked_record(&report, &format)
        }
        Phase3AdoptionCommands::Review {
            track,
            feature,
            signal,
            limit,
            format,
        } => {
            let track = track
                .as_deref()
                .map(parse_runtime_adoption_track)
                .transpose()?;
            let signal = signal
                .as_deref()
                .map(parse_runtime_adoption_signal)
                .transpose()?;
            let events = db
                .list_runtime_adoption_events(
                    &RuntimeAdoptionFilter {
                        track: track.clone(),
                        feature: feature.clone(),
                    },
                    limit,
                )
                .context("failed to list runtime adoption events")?;
            let report = review_runtime_adoption_events(
                &events,
                RuntimeAdoptionReviewFilters {
                    track: track
                        .as_ref()
                        .map(runtime_adoption_track_slug)
                        .map(str::to_string),
                    feature,
                    signal: signal
                        .as_ref()
                        .map(runtime_adoption_signal_slug)
                        .map(str::to_string),
                    limit,
                },
            );
            print_runtime_adoption_review(&report, &format)
        }
        Phase3AdoptionCommands::Record {
            track,
            signal,
            feature,
            query,
            context_hash,
            card_id,
            evaluator_id,
            research_report_id,
            note,
            metadata_json,
            id,
            format,
        } => {
            let track = parse_runtime_adoption_track(&track)?;
            let signal = parse_runtime_adoption_signal(&signal)?;
            let metadata = metadata_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .context("failed to parse --metadata-json")?;
            let event = runtime_adoption_event_from_input(
                RuntimeAdoptionRecordPlanInput {
                    id,
                    track: runtime_adoption_track_slug(&track).to_string(),
                    signal: runtime_adoption_signal_slug(&signal).to_string(),
                    feature,
                    query,
                    context_hash,
                    card_id,
                    evaluator_id,
                    research_report_id,
                    note,
                    metadata,
                },
                track,
                signal,
            );
            let id = event.id.clone();
            db.insert_runtime_adoption_event(&event)
                .context("failed to insert runtime adoption event")?;
            match format.as_str() {
                "plain" => println!("event_id={id} created=true"),
                "json" => println!(
                    "{}",
                    serde_json::to_string_pretty(&event)
                        .context("failed to serialize adoption event")?
                ),
                other => bail!("unsupported phase3 adoption format: {other}"),
            }
            Ok(())
        }
        Phase3AdoptionCommands::List {
            track,
            feature,
            limit,
            format,
        } => {
            let events = db
                .list_runtime_adoption_events(
                    &RuntimeAdoptionFilter {
                        track: track
                            .as_deref()
                            .map(parse_runtime_adoption_track)
                            .transpose()?,
                        feature,
                    },
                    limit,
                )
                .context("failed to list runtime adoption events")?;
            print_runtime_adoption_events(&events, &format)
        }
        Phase3AdoptionCommands::Stats {
            track,
            feature,
            format,
        } => {
            let events = db
                .list_runtime_adoption_events(
                    &RuntimeAdoptionFilter {
                        track: track
                            .as_deref()
                            .map(parse_runtime_adoption_track)
                            .transpose()?,
                        feature,
                    },
                    10_000,
                )
                .context("failed to list runtime adoption events")?;
            let stats = RuntimeAdoptionStats::from_events(&events);
            print_runtime_adoption_stats(&stats, &format)
        }
        Phase3AdoptionCommands::Wrap {
            surface,
            query,
            note,
            outcome,
            execute,
            allow_warnings,
            format,
            child_cmd,
        } => phase3_adoption_wrap_command(
            db,
            WrapCommandOpts {
                surface,
                query,
                note,
                outcome,
                execute,
                allow_warnings,
                format,
                child_cmd,
            },
        ),
        Phase3AdoptionCommands::Analytics { format } => {
            let events = db
                .list_runtime_adoption_events(&RuntimeAdoptionFilter::default(), 10_000)
                .context("failed to list runtime adoption events")?;
            let report = build_runtime_adoption_analytics(&events);
            match format.as_str() {
                "json" => {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                    Ok(())
                }
                "plain" => {
                    println!("adoption analytics total_events={}", report.total_events);
                    for group in &report.groups {
                        println!(
                            "  feature={} track={} accepted={} rejected={} recommendation={}",
                            group.feature,
                            group.track,
                            group.accepted,
                            group.rejected,
                            group.recommendation
                        );
                    }
                    Ok(())
                }
                other => bail!("unsupported phase3 adoption analytics format: {other}"),
            }
        }
    }
}

fn runtime_adoption_event_from_input(
    input: RuntimeAdoptionRecordPlanInput,
    track: RuntimeAdoptionTrack,
    signal: RuntimeAdoptionSignal,
) -> RuntimeAdoptionEvent {
    let created_at = current_timestamp();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string());
    let id = input.id.unwrap_or_else(|| {
        stable_cli_id(
            "adoption",
            &[
                runtime_adoption_track_slug(&track),
                runtime_adoption_signal_slug(&signal),
                input.feature.as_str(),
                input.query.as_deref().unwrap_or(""),
                input.context_hash.as_deref().unwrap_or(""),
                input.card_id.as_deref().unwrap_or(""),
                input.evaluator_id.as_deref().unwrap_or(""),
                input.research_report_id.as_deref().unwrap_or(""),
                created_at.as_str(),
                nonce.as_str(),
            ],
        )
    });
    RuntimeAdoptionEvent {
        id,
        track,
        signal,
        feature: input.feature,
        query: input.query,
        context_hash: input.context_hash,
        card_id: input.card_id,
        evaluator_id: input.evaluator_id,
        research_report_id: input.research_report_id,
        note: input.note,
        metadata: input.metadata,
        created_at,
    }
}

fn print_runtime_adoption_checked_record(
    report: &RuntimeAdoptionCheckedRecordReport,
    format: &str,
) -> Result<()> {
    match format {
        "plain" => {
            println!("writes={}", report.writes);
            println!("blocked={}", report.blocked);
            println!("quality={}", report.record_quality.quality);
            if let Some(event) = report.event.as_ref() {
                println!("event_id={}", event.id);
            }
            for error in &report.record_quality.errors {
                println!("error={error}");
            }
            for warning in &report.record_quality.warnings {
                println!("warning={warning}");
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(report)
                    .context("failed to serialize runtime adoption checked record report")?
            );
            Ok(())
        }
        other => bail!("unsupported phase3 adoption format: {other}"),
    }
}

fn print_runtime_adoption_capture(
    report: &RuntimeAdoptionCaptureReport,
    format: &str,
) -> Result<()> {
    match format {
        "plain" => {
            println!("writes={}", report.writes);
            println!("execute={}", report.execute);
            println!("surface={}", report.surface);
            println!("outcome={}", report.outcome);
            println!("quality={}", report.record_quality.quality);
            if let Some(checked) = report.record_checked.as_ref() {
                println!("blocked={}", checked.blocked);
                if let Some(event) = checked.event.as_ref() {
                    println!("event_id={}", event.id);
                }
            }
            for error in &report.record_quality.errors {
                println!("error={error}");
            }
            for warning in &report.record_quality.warnings {
                println!("warning={warning}");
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(report)
                    .context("failed to serialize runtime adoption capture report")?
            );
            Ok(())
        }
        other => bail!("unsupported phase3 adoption format: {other}"),
    }
}

fn print_runtime_adoption_review(report: &RuntimeAdoptionReviewReport, format: &str) -> Result<()> {
    match format {
        "plain" => {
            println!("writes={}", report.writes);
            println!("total={}", report.total);
            println!("conclusion={}", report.conclusion);
            for reason in &report.reasons {
                println!("reason={reason}");
            }
            for feature in &report.features {
                println!(
                    "feature={} total={} accepted={} rejected={} misses={} rollbacks={}",
                    feature.feature,
                    feature.stats.total,
                    feature.stats.accepted,
                    feature.stats.rejected,
                    feature.stats.misses,
                    feature.stats.rollbacks
                );
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(report)
                    .context("failed to serialize runtime adoption review report")?
            );
            Ok(())
        }
        other => bail!("unsupported phase3 adoption format: {other}"),
    }
}

fn print_runtime_adoption_record_quality(
    report: &RuntimeAdoptionRecordQualityReport,
    format: &str,
) -> Result<()> {
    match format {
        "plain" => {
            println!("writes={}", report.writes);
            println!("valid={}", report.valid);
            println!("quality={}", report.quality);
            for error in &report.errors {
                println!("error={error}");
            }
            for warning in &report.warnings {
                println!("warning={warning}");
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(report)
                    .context("failed to serialize runtime adoption record quality report")?
            );
            Ok(())
        }
        other => bail!("unsupported phase3 adoption format: {other}"),
    }
}

fn print_runtime_adoption_record_plan(
    plan: &RuntimeAdoptionRecordPlan,
    format: &str,
) -> Result<()> {
    match format {
        "plain" => {
            println!("writes={}", plan.writes);
            println!("record_command={}", plan.record_command.join(" "));
            if let Some(action) = plan.record_payload.get("action").and_then(Value::as_str) {
                println!("action={action}");
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(plan)
                    .context("failed to serialize runtime adoption record plan")?
            );
            Ok(())
        }
        other => bail!("unsupported phase3 adoption format: {other}"),
    }
}

fn print_runtime_adoption_guidance(guidance: &RuntimeAdoptionGuidance, format: &str) -> Result<()> {
    match format {
        "plain" => {
            println!("version={}", guidance.version);
            println!("recording_rule={}", guidance.recording_rule);
            println!("required_fields={}", guidance.required_fields.join(","));
            println!("optional_fields={}", guidance.optional_fields.join(","));
            for signal in &guidance.signals {
                println!("signal={} when={}", signal.signal, signal.when);
            }
            for track in &guidance.tracks {
                println!(
                    "track={} when={} feature_examples={}",
                    track.track,
                    track.when,
                    track.feature_examples.join(",")
                );
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(guidance)
                    .context("failed to serialize runtime adoption guidance")?
            );
            Ok(())
        }
        other => bail!("unsupported phase3 adoption format: {other}"),
    }
}

fn print_runtime_adoption_instrumentation_policy(
    policy: &mempal::core::phase3::RuntimeAdoptionInstrumentationPolicy,
    format: &str,
) -> Result<()> {
    match format {
        "plain" => {
            println!("version={}", policy.version);
            println!("writes={}", policy.writes);
            println!("default_mode={}", policy.default_mode);
            for mode in &policy.allowed_modes {
                println!(
                    "allowed_mode={} requires_execute={} requires_checked_capture={}",
                    mode.mode, mode.requires_execute, mode.requires_checked_capture
                );
            }
            for mode in &policy.forbidden_modes {
                println!("forbidden_mode={mode}");
            }
            for requirement in &policy.requirements {
                println!("requirement={requirement}");
            }
            for requirement in &policy.rollback_requirements {
                println!("rollback_requirement={requirement}");
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(policy)
                    .context("failed to serialize runtime adoption instrumentation policy")?
            );
            Ok(())
        }
        other => bail!("unsupported phase3 adoption format: {other}"),
    }
}

#[derive(Debug, Serialize)]
struct RuntimeAdoptionStats {
    total: usize,
    used: usize,
    accepted: usize,
    rejected: usize,
    misses: usize,
    rollbacks: usize,
    contradictions: usize,
    neutral: usize,
}

impl RuntimeAdoptionStats {
    fn from_events(events: &[RuntimeAdoptionEvent]) -> Self {
        let mut stats = Self {
            total: events.len(),
            used: 0,
            accepted: 0,
            rejected: 0,
            misses: 0,
            rollbacks: 0,
            contradictions: 0,
            neutral: 0,
        };
        for event in events {
            match event.signal {
                RuntimeAdoptionSignal::Used => stats.used += 1,
                RuntimeAdoptionSignal::Accepted => stats.accepted += 1,
                RuntimeAdoptionSignal::Rejected => stats.rejected += 1,
                RuntimeAdoptionSignal::Miss => stats.misses += 1,
                RuntimeAdoptionSignal::Rollback => stats.rollbacks += 1,
                RuntimeAdoptionSignal::Contradiction => stats.contradictions += 1,
                RuntimeAdoptionSignal::Neutral => stats.neutral += 1,
            }
        }
        stats
    }
}

#[derive(Debug, Serialize)]
struct Phase3GateReport {
    candidate: String,
    ready: bool,
    required_track: &'static str,
    stats: RuntimeAdoptionStats,
    reasons: Vec<String>,
}

fn phase3_gate_report(db: &Database, candidate: &str) -> Result<Phase3GateReport> {
    let (track, ready_fn): (RuntimeAdoptionTrack, fn(&RuntimeAdoptionStats) -> bool) =
        match candidate {
            "card-context-default" => (RuntimeAdoptionTrack::CardContext, |stats| {
                stats.accepted >= 3 && stats.rollbacks == 0 && stats.rejected <= stats.accepted
            }),
            "card-embeddings" => (RuntimeAdoptionTrack::CardEmbedding, |stats| {
                stats.misses >= 3 && stats.rollbacks == 0
            }),
            "evaluator-api" => (RuntimeAdoptionTrack::Evaluator, |stats| {
                stats.accepted >= 3 && stats.rollbacks == 0 && stats.contradictions == 0
            }),
            "research-adapter" => (RuntimeAdoptionTrack::ResearchAdapter, |stats| {
                stats.accepted >= 1 && stats.contradictions == 0 && stats.rollbacks == 0
            }),
            other => bail!("unsupported phase3 candidate: {other}"),
        };
    let events = db
        .list_runtime_adoption_events(
            &RuntimeAdoptionFilter {
                track: Some(track.clone()),
                feature: None,
            },
            10_000,
        )
        .context("failed to list runtime adoption events")?;
    let stats = RuntimeAdoptionStats::from_events(&events);
    let ready = ready_fn(&stats);
    let mut reasons = Vec::new();
    if ready {
        reasons.push("minimum evidence threshold satisfied".to_string());
    } else {
        reasons.push("minimum evidence threshold not satisfied".to_string());
    }
    if stats.rollbacks > 0 {
        reasons.push("rollback signals block default or authority changes".to_string());
    }
    if stats.contradictions > 0 {
        reasons.push("contradiction signals require review before implementation".to_string());
    }
    Ok(Phase3GateReport {
        candidate: candidate.to_string(),
        ready,
        required_track: runtime_adoption_track_slug(&track),
        stats,
        reasons,
    })
}

#[derive(Debug, Serialize)]
struct ResearchAdapterPlanReport {
    valid: bool,
    report_id: String,
    title: String,
    source_count: usize,
    finding_count: usize,
    candidate_insight_count: usize,
    errors: Vec<String>,
}

fn validate_research_adapter_plan(path: &Path) -> Result<ResearchAdapterPlanReport> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read research report {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse research report {}", path.display()))?;
    let mut errors = Vec::new();
    let report_id = required_string(&value, "report_id", &mut errors);
    let title = required_string(&value, "title", &mut errors);
    let sources = value
        .get("sources")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    if sources == 0 {
        errors.push("sources must contain at least one item".to_string());
    }
    let findings = value
        .get("findings")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    if findings == 0 {
        errors.push("findings must contain at least one item".to_string());
    }
    let candidate_insights = value
        .get("candidate_insights")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    Ok(ResearchAdapterPlanReport {
        valid: errors.is_empty(),
        report_id,
        title,
        source_count: sources,
        finding_count: findings,
        candidate_insight_count: candidate_insights,
        errors,
    })
}

fn required_string(
    value: &serde_json::Value,
    field: &'static str,
    errors: &mut Vec<String>,
) -> String {
    match value.get(field).and_then(serde_json::Value::as_str) {
        Some(raw) if !raw.trim().is_empty() => raw.trim().to_string(),
        _ => {
            errors.push(format!("{field} is required"));
            String::new()
        }
    }
}

fn print_runtime_adoption_events(events: &[RuntimeAdoptionEvent], format: &str) -> Result<()> {
    match format {
        "plain" => {
            if events.is_empty() {
                println!("no runtime adoption events");
                return Ok(());
            }
            for event in events {
                println!(
                    "{} track={} signal={} feature={} at={}",
                    event.id,
                    runtime_adoption_track_slug(&event.track),
                    runtime_adoption_signal_slug(&event.signal),
                    event.feature,
                    event.created_at
                );
                if let Some(note) = event.note.as_deref() {
                    println!("  note: {note}");
                }
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(events)
                    .context("failed to serialize runtime adoption events")?
            );
            Ok(())
        }
        other => bail!("unsupported phase3 adoption format: {other}"),
    }
}

fn print_runtime_adoption_stats(stats: &RuntimeAdoptionStats, format: &str) -> Result<()> {
    match format {
        "plain" => {
            println!("total={}", stats.total);
            println!("used={}", stats.used);
            println!("accepted={}", stats.accepted);
            println!("rejected={}", stats.rejected);
            println!("misses={}", stats.misses);
            println!("rollbacks={}", stats.rollbacks);
            println!("contradictions={}", stats.contradictions);
            println!("neutral={}", stats.neutral);
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(stats)
                    .context("failed to serialize runtime adoption stats")?
            );
            Ok(())
        }
        other => bail!("unsupported phase3 adoption format: {other}"),
    }
}

fn print_phase3_gate_report(report: &Phase3GateReport, format: &str) -> Result<()> {
    match format {
        "plain" => {
            println!("candidate={}", report.candidate);
            println!("ready={}", report.ready);
            println!("required_track={}", report.required_track);
            println!("accepted={}", report.stats.accepted);
            println!("misses={}", report.stats.misses);
            println!("rollbacks={}", report.stats.rollbacks);
            for reason in &report.reasons {
                println!("reason={reason}");
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(report)
                    .context("failed to serialize phase3 gate report")?
            );
            Ok(())
        }
        other => bail!("unsupported phase3 gate format: {other}"),
    }
}

fn print_research_adapter_plan(report: &ResearchAdapterPlanReport, format: &str) -> Result<()> {
    match format {
        "plain" => {
            println!("valid={}", report.valid);
            println!("report_id={}", report.report_id);
            println!("title={}", report.title);
            println!("source_count={}", report.source_count);
            println!("finding_count={}", report.finding_count);
            println!("candidate_insight_count={}", report.candidate_insight_count);
            for error in &report.errors {
                println!("error={error}");
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(report)
                    .context("failed to serialize research adapter plan")?
            );
            Ok(())
        }
        other => bail!("unsupported research adapter plan format: {other}"),
    }
}

fn normalized_nonempty_strings(values: &[String]) -> Vec<String> {
    values
        .iter()
        .filter_map(|v| {
            let t = v.trim();
            (!t.is_empty()).then(|| t.to_string())
        })
        .collect()
}
fn build_trigger_hints(
    intent_tags: Vec<String>,
    workflow_bias: Vec<String>,
    tool_needs: Vec<String>,
) -> Option<TriggerHints> {
    let intent_tags = normalized_nonempty_strings(&intent_tags);
    let workflow_bias = normalized_nonempty_strings(&workflow_bias);
    let tool_needs = normalized_nonempty_strings(&tool_needs);
    if intent_tags.is_empty() && workflow_bias.is_empty() && tool_needs.is_empty() {
        return None;
    }
    Some(TriggerHints {
        intent_tags,
        workflow_bias,
        tool_needs,
    })
}

fn print_knowledge_cards(cards: &[KnowledgeCard], format: &str) -> Result<()> {
    match format {
        "plain" => {
            if cards.is_empty() {
                println!("no knowledge cards");
                return Ok(());
            }
            for card in cards {
                println!(
                    "{} tier={} status={} domain={} field={} auto_generated={} score={} anchor={} {}",
                    card.id,
                    knowledge_tier_slug(&card.tier),
                    knowledge_status_slug(&card.status),
                    domain_slug(&card.domain),
                    card.field,
                    card.auto_generated,
                    card.crystallization_score
                        .map(|score| format!("{score:.3}"))
                        .unwrap_or_else(|| "none".to_string()),
                    anchor_kind_slug(&card.anchor_kind),
                    card.anchor_id
                );
                println!("statement: {}", card.statement);
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(cards)
                    .context("failed to serialize knowledge cards")?
            );
            Ok(())
        }
        other => bail!("unsupported knowledge-card format: {other}"),
    }
}
fn print_knowledge_card(card: &KnowledgeCard, format: &str) -> Result<()> {
    match format {
        "plain" => print_knowledge_cards(std::slice::from_ref(card), format),
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(card).context("failed to serialize knowledge card")?
            );
            Ok(())
        }
        other => bail!("unsupported knowledge-card format: {other}"),
    }
}

fn print_retrieved_knowledge_cards(results: &[RetrievedKnowledgeCard], format: &str) -> Result<()> {
    match format {
        "plain" => {
            if results.is_empty() {
                println!("no retrieved knowledge cards");
                return Ok(());
            }
            for result in results {
                let card = &result.card;
                println!(
                    "{} score={:.6} tier={} status={} domain={} field={}",
                    card.id,
                    result.score,
                    knowledge_tier_slug(&card.tier),
                    knowledge_status_slug(&card.status),
                    domain_slug(&card.domain),
                    card.field
                );
                println!("statement: {}", card.statement);
                for citation in &result.evidence_citations {
                    println!(
                        "evidence: {} role={} source={} score={:.6}",
                        citation.evidence_drawer_id,
                        knowledge_evidence_role_slug(&citation.role),
                        citation.source_file,
                        citation.score
                    );
                }
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(results)
                    .context("failed to serialize retrieved knowledge cards")?
            );
            Ok(())
        }
        other => bail!("unsupported knowledge-card retrieve format: {other}"),
    }
}
fn print_knowledge_card_events(events: &[KnowledgeCardEvent], format: &str) -> Result<()> {
    match format {
        "plain" => {
            if events.is_empty() {
                println!("no knowledge card events");
                return Ok(());
            }
            for event in events {
                println!(
                    "{} card_id={} type={} reason={}",
                    event.id,
                    event.card_id,
                    knowledge_event_type_slug(&event.event_type),
                    event.reason
                );
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(events)
                    .context("failed to serialize knowledge card events")?
            );
            Ok(())
        }
        other => bail!("unsupported knowledge-card format: {other}"),
    }
}

fn print_knowledge_card_gate_report(report: &KnowledgeCardGateReport, format: &str) -> Result<()> {
    match format {
        "plain" => {
            println!("card_id={}", report.card_id);
            println!("tier={}", report.tier);
            println!("status={}", report.status);
            println!("target_status={}", report.target_status);
            println!("allowed={}", report.allowed);
            println!(
                "evidence_counts supporting={} verification={} teaching={} counterexample={}",
                report.evidence_counts.supporting,
                report.evidence_counts.verification,
                report.evidence_counts.teaching,
                report.evidence_counts.counterexample
            );
            println!(
                "requirements supporting>={} verification>={} teaching>={} reviewer_required={} counterexamples_block={}",
                report.requirements.min_supporting_refs,
                report.requirements.min_verification_refs,
                report.requirements.min_teaching_refs,
                report.requirements.reviewer_required,
                report.requirements.counterexamples_block
            );
            if !report.reasons.is_empty() {
                println!("reasons={}", report.reasons.join("; "));
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(report)
                    .context("failed to serialize knowledge card gate report")?
            );
            Ok(())
        }
        other => bail!("unsupported knowledge-card gate format: {other}"),
    }
}

fn print_knowledge_card_promote_outcome(outcome: &PromoteCardOutcome, format: &str) -> Result<()> {
    match format {
        "plain" => {
            println!(
                "card_id={} old_status={} new_status={} verification_refs={}",
                outcome.card_id,
                outcome.old_status,
                outcome.new_status,
                outcome.verification_refs.join(",")
            );
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(outcome)
                    .context("failed to serialize knowledge card promote outcome")?
            );
            Ok(())
        }
        other => bail!("unsupported knowledge-card promote format: {other}"),
    }
}

fn print_knowledge_card_demote_outcome(outcome: &DemoteCardOutcome, format: &str) -> Result<()> {
    match format {
        "plain" => {
            println!(
                "card_id={} old_status={} new_status={} counterexample_refs={}",
                outcome.card_id,
                outcome.old_status,
                outcome.new_status,
                outcome.counterexample_refs.join(",")
            );
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(outcome)
                    .context("failed to serialize knowledge card demote outcome")?
            );
            Ok(())
        }
        other => bail!("unsupported knowledge-card demote format: {other}"),
    }
}

fn print_knowledge_card_backfill_report(
    report: &KnowledgeCardBackfillReport,
    format: &str,
) -> Result<()> {
    match format {
        "plain" => {
            println!(
                "ready={} skipped={} already_exists={}",
                report.ready_count, report.skipped_count, report.already_exists_count
            );
            if report.candidates.is_empty() {
                println!("no knowledge drawers");
                return Ok(());
            }
            for candidate in &report.candidates {
                println!(
                    "{} -> {} status={:?}",
                    candidate.source_drawer_id, candidate.prospective_card_id, candidate.status
                );
                if !candidate.reasons.is_empty() {
                    println!("  reasons: {}", candidate.reasons.join("; "));
                }
                if let Some(statement) = candidate.statement.as_deref() {
                    println!("  statement: {statement}");
                }
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(report)
                    .context("failed to serialize knowledge card backfill report")?
            );
            Ok(())
        }
        other => bail!("unsupported knowledge-card backfill-plan format: {other}"),
    }
}

fn print_knowledge_card_backfill_apply_result(
    result: &KnowledgeCardBackfillApplyResult,
    format: &str,
) -> Result<()> {
    match format {
        "plain" => {
            println!(
                "dry_run={} ready={} skipped={} already_exists={} created_count={} linked_count={} event_count={} link_errors={}",
                result.dry_run,
                result.ready_count,
                result.skipped_count,
                result.already_exists_count,
                result.created_count,
                result.linked_count,
                result.event_count,
                result.link_errors.len()
            );
            if result.candidates.is_empty() {
                println!("no knowledge drawers");
            } else {
                for candidate in &result.candidates {
                    println!(
                        "{} -> {} status={:?}",
                        candidate.source_drawer_id, candidate.prospective_card_id, candidate.status
                    );
                    if !candidate.reasons.is_empty() {
                        println!("  reasons: {}", candidate.reasons.join("; "));
                    }
                    if let Some(statement) = candidate.statement.as_deref() {
                        println!("  statement: {statement}");
                    }
                }
            }
            for error in &result.link_errors {
                println!(
                    "link_error card_id={} evidence_drawer_id={} role={} error={}",
                    error.card_id, error.evidence_drawer_id, error.role, error.error
                );
            }
            Ok(())
        }
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(result)
                    .context("failed to serialize knowledge card backfill apply result")?
            );
            Ok(())
        }
        other => bail!("unsupported knowledge-card backfill-apply format: {other}"),
    }
}
fn print_gate_report(report: &GateReport) {
    println!("drawer_id={}", report.drawer_id);
    println!("tier={}", report.tier);
    println!("status={}", report.status);
    println!("target_status={}", report.target_status);
    println!("allowed={}", report.allowed);
    println!(
        "evidence_counts supporting={} verification={} teaching={} counterexample={}",
        report.evidence_counts.supporting,
        report.evidence_counts.verification,
        report.evidence_counts.teaching,
        report.evidence_counts.counterexample
    );
    println!(
        "requirements supporting>={} verification>={} teaching>={} reviewer_required={} counterexamples_block={}",
        report.requirements.min_supporting_refs,
        report.requirements.min_verification_refs,
        report.requirements.min_teaching_refs,
        report.requirements.reviewer_required,
        report.requirements.counterexamples_block
    );
    for reason in &report.reasons {
        println!("reason={reason}");
    }
}
fn print_promotion_policy(policy: &[PromotionPolicyEntry]) {
    for entry in policy {
        println!(
            "{} -> {} supporting>={} verification>={} teaching>={} reviewer_required={} counterexamples_block={}",
            entry.tier,
            entry.target_status,
            entry.requirements.min_supporting_refs,
            entry.requirements.min_verification_refs,
            entry.requirements.min_teaching_refs,
            entry.requirements.reviewer_required,
            entry.requirements.counterexamples_block
        );
    }
}

fn delete_command(db: &Database, drawer_id: &str) -> Result<()> {
    let drawer = db
        .get_drawer(drawer_id)
        .context("failed to look up drawer")?;
    match drawer {
        Some(drawer) => {
            db.soft_delete_drawer(drawer_id)
                .context("failed to soft-delete drawer")?;
            append_audit_entry(db, "delete", &serde_json::json!({ "drawer_id": drawer_id }))
                .context("failed to append audit log")?;
            println!("soft-deleted {}", drawer_id);
            println!(
                "  wing={} room={} source={}",
                drawer.wing,
                drawer.room.as_deref().unwrap_or("default"),
                drawer.source_file.as_deref().unwrap_or("(none)")
            );
            println!("  content: {}", truncate_for_summary(&drawer.content, 100));
            println!("  (use `mempal purge` to permanently remove)");
        }
        None => bail!("drawer not found: {drawer_id}"),
    }
    Ok(())
}

fn pin_command(db: &Database, drawer_id: &str) -> Result<()> {
    if !db
        .pin_drawer(drawer_id, None)
        .context("failed to pin drawer")?
    {
        bail!("drawer not found: {drawer_id}");
    }
    append_audit_entry(db, "pin", &serde_json::json!({ "drawer_id": drawer_id }))
        .context("failed to append audit log")?;
    println!("pinned {drawer_id}");
    Ok(())
}

fn unpin_command(db: &Database, drawer_id: &str) -> Result<()> {
    if !db
        .unpin_drawer(drawer_id)
        .context("failed to unpin drawer")?
    {
        bail!("drawer not found: {drawer_id}");
    }
    append_audit_entry(db, "unpin", &serde_json::json!({ "drawer_id": drawer_id }))
        .context("failed to append audit log")?;
    println!("unpinned {drawer_id}");
    Ok(())
}

fn pinned_command(
    db: &Database,
    config: &Config,
    project: Option<&str>,
    reorder: &[String],
    json: bool,
) -> Result<()> {
    if !reorder.is_empty() {
        for drawer_id in reorder {
            if db
                .get_drawer(drawer_id)
                .context("failed to look up pinned reorder drawer")?
                .is_none()
            {
                bail!("drawer not found: {drawer_id}");
            }
        }
        db.reorder_pinned_facts(reorder)
            .context("failed to reorder pinned facts")?;
        append_audit_entry(
            db,
            "pinned_reorder",
            &serde_json::json!({ "drawer_ids": reorder }),
        )
        .context("failed to append audit log")?;
    }

    let cwd = env::current_dir().ok();
    let project_id = match project {
        Some(project) => resolve_project_id(Some(project), config, cwd.as_deref())
            .context("failed to resolve pinned project id")?,
        None => None,
    };
    let facts = db
        .get_pinned_facts(project_id.as_deref(), 1_000_000)
        .context("failed to load pinned facts")?;

    if json {
        let output = facts
            .iter()
            .map(|drawer| {
                serde_json::json!({
                    "drawer_id": &drawer.id,
                    "pin_order": drawer.pin_order,
                    "memory_kind": memory_kind_slug(&drawer.memory_kind),
                    "domain": domain_slug(&drawer.domain),
                    "field": &drawer.field,
                    "status": drawer.status.as_ref().map(knowledge_status_slug),
                    "importance": drawer.importance,
                    "source_file": &drawer.source_file,
                    "content": &drawer.content,
                })
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "facts": output }))
                .context("failed to serialize pinned facts JSON")?
        );
        return Ok(());
    }

    if facts.is_empty() {
        println!("no pinned facts");
        return Ok(());
    }
    for drawer in facts {
        let order = drawer
            .pin_order
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{order}\t{}\t{}/{}\t{}",
            drawer.id,
            domain_slug(&drawer.domain),
            drawer.field,
            truncate_for_summary(&drawer.content, 120)
        );
    }
    Ok(())
}

fn rollback_command(
    db: &Database,
    config: &Config,
    options: RollbackCommandOptions<'_>,
) -> Result<()> {
    let since = normalize_rfc3339_timestamp(options.since)
        .with_context(|| format!("invalid --since ISO 8601 timestamp: {}", options.since))?;
    let current_dir = env::current_dir().ok();
    let project_id = resolve_project_id(options.project, config, current_dir.as_deref())
        .context("failed to resolve rollback project id")?;
    let output = if options.dry_run {
        let count = db
            .count_drawers_since(&since, options.wing, options.room, project_id.as_deref())
            .context("failed to count rollback drawers")?;
        RollbackOutput {
            since,
            deleted_count: count.max(0) as usize,
            drawer_ids: Vec::new(),
            dry_run: true,
        }
    } else {
        let drawer_ids = db
            .soft_delete_drawers_since(&since, options.wing, options.room, project_id.as_deref())
            .context("failed to rollback drawers")?;
        RollbackOutput {
            since,
            deleted_count: drawer_ids.len(),
            drawer_ids,
            dry_run: false,
        }
    };
    if options.json {
        println!(
            "{}",
            serde_json::to_string(&output).context("failed to serialize rollback output")?
        );
    } else if output.dry_run {
        println!(
            "would delete {} drawers since {}",
            output.deleted_count, output.since
        );
    } else {
        println!(
            "deleted {} drawers since {}",
            output.deleted_count, output.since
        );
        for did in &output.drawer_ids {
            println!("  {did}");
        }
    }
    Ok(())
}

fn purge_command(db: &Database, before: Option<&str>) -> Result<()> {
    let deleted_count = db
        .deleted_drawer_count()
        .context("failed to count deleted drawers")?;
    if deleted_count == 0 {
        println!("no soft-deleted drawers to purge");
        return Ok(());
    }
    let purged = db
        .purge_deleted(before)
        .context("failed to purge deleted drawers")?;
    append_audit_entry(
        db,
        "purge",
        &serde_json::json!({ "before": before, "purged": purged }),
    )
    .context("failed to append audit log")?;
    println!("permanently removed {purged} drawer(s)");
    Ok(())
}

fn append_audit_entry(db: &Database, command: &str, details: &serde_json::Value) -> Result<()> {
    let audit_path = db
        .path()
        .parent()
        .map(|p| p.join("audit.jsonl"))
        .unwrap_or_else(|| PathBuf::from("audit.jsonl"));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&audit_path)
        .with_context(|| format!("failed to open audit log {}", audit_path.display()))?;
    let entry = serde_json::json!({ "timestamp": current_timestamp(), "command": command, "details": details });
    writeln!(file, "{entry}")
        .with_context(|| format!("failed to write audit log {}", audit_path.display()))?;
    Ok(())
}

fn kg_command(db: &Database, command: KgCommands) -> Result<()> {
    use mempal::core::types::Triple;
    match command {
        KgCommands::Add {
            subject,
            predicate,
            object,
            source_drawer,
        } => {
            let id = build_triple_id(&subject, &predicate, &object);
            let triple = Triple {
                id: id.clone(),
                subject: subject.clone(),
                predicate: predicate.clone(),
                object: object.clone(),
                valid_from: Some(current_timestamp()),
                valid_to: None,
                confidence: 1.0,
                source_drawer,
            };
            db.insert_triple(&triple)
                .context("failed to insert triple")?;
            println!("added: ({subject}) --[{predicate}]--> ({object})");
            println!("  id: {id}");
        }
        KgCommands::Query {
            subject,
            predicate,
            object,
            all,
        } => {
            let triples = db
                .query_triples(
                    subject.as_deref(),
                    predicate.as_deref(),
                    object.as_deref(),
                    !all,
                )
                .context("failed to query triples")?;
            if triples.is_empty() {
                println!("no triples found");
            } else {
                for t in &triples {
                    let valid = match (&t.valid_from, &t.valid_to) {
                        (Some(from), Some(to)) => format!("{from}..{to}"),
                        (Some(from), None) => format!("{from}..now"),
                        _ => "always".to_string(),
                    };
                    println!(
                        "({}) --[{}]--> ({})  [{valid}]  id={}",
                        t.subject, t.predicate, t.object, t.id
                    );
                }
                println!("\n{} triple(s)", triples.len());
            }
        }
        KgCommands::Invalidate { triple_id } => {
            if !db
                .triple_exists(&triple_id)
                .context("failed to check triple existence")?
            {
                bail!("triple not found: {triple_id}");
            }
            let invalidated = db
                .invalidate_triple(&triple_id)
                .context("failed to invalidate triple")?;
            if invalidated {
                append_audit_entry(
                    db,
                    "kg-invalidate",
                    &serde_json::json!({ "triple_id": triple_id }),
                )
                .context("failed to append audit log")?;
                println!("invalidated triple {triple_id}");
            } else {
                println!("triple {triple_id} already invalidated");
            }
        }
        KgCommands::Timeline { entity } => {
            let triples = db
                .timeline_for_entity(&entity)
                .context("failed to get timeline")?;
            if triples.is_empty() {
                println!("no triples for '{entity}'");
            } else {
                for t in &triples {
                    let valid = match (&t.valid_from, &t.valid_to) {
                        (Some(from), Some(to)) => format!("{from}..{to}"),
                        (Some(from), None) => format!("{from}..now"),
                        _ => "always".to_string(),
                    };
                    let dir = if t.subject == entity {
                        format!("({}) --[{}]--> ({})", t.subject, t.predicate, t.object)
                    } else {
                        format!("({}) <--[{}]-- ({})", entity, t.predicate, t.subject)
                    };
                    println!("{dir}  [{valid}]");
                }
                println!("\n{} event(s) for '{entity}'", triples.len());
            }
        }
        KgCommands::Stats => {
            let stats = db.triple_stats().context("failed to get KG stats")?;
            println!("total: {}", stats.total);
            println!("active: {}", stats.active);
            println!("expired: {}", stats.expired);
            println!("entities: {}", stats.entities);
            if !stats.top_predicates.is_empty() {
                println!("top predicates:");
                for (pred, count) in &stats.top_predicates {
                    println!("  {pred}: {count}");
                }
            }
        }
        KgCommands::List => {
            let count = db.triple_count().context("failed to count triples")?;
            println!("triple_count: {count}");
        }
    }
    Ok(())
}

fn tunnels_command(db: &Database, command: Option<TunnelCommands>) -> Result<()> {
    match command {
        None => tunnels_discover_command(db),
        Some(TunnelCommands::Add { left, right, label }) => {
            let tunnel = db
                .create_tunnel(
                    &parse_tunnel_endpoint(&left)?,
                    &parse_tunnel_endpoint(&right)?,
                    &label,
                    Some("mempal-cli"),
                )
                .context("failed to add tunnel")?;
            println!(
                "created tunnel {}\n{} <-> {} | {}",
                tunnel.id,
                format_tunnel_endpoint(&tunnel.left),
                format_tunnel_endpoint(&tunnel.right),
                tunnel.label
            );
            Ok(())
        }
        Some(TunnelCommands::List { wing, kind }) => {
            tunnels_list_command(db, wing.as_deref(), &kind)
        }
        Some(TunnelCommands::Delete { tunnel_id }) => {
            if tunnel_id.starts_with("passive_") {
                bail!("cannot delete passive tunnel");
            }
            if db
                .delete_explicit_tunnel(&tunnel_id)
                .context("failed to delete tunnel")?
            {
                println!("deleted tunnel {tunnel_id}");
                Ok(())
            } else {
                bail!("tunnel not found: {tunnel_id}");
            }
        }
        Some(TunnelCommands::Follow { from, hops }) => {
            let endpoint = parse_tunnel_endpoint(&from)?;
            let results = db
                .follow_explicit_tunnels(&endpoint, hops)
                .context("failed to follow tunnels")?;
            if results.is_empty() {
                println!("no explicit tunnel neighbors");
            } else {
                for r in &results {
                    println!(
                        "hop {} via {} -> {}",
                        r.hop,
                        r.via_tunnel_id,
                        format_tunnel_endpoint(&r.endpoint)
                    );
                }
                println!("\n{} tunnel neighbor(s)", results.len());
            }
            Ok(())
        }
    }
}
fn tunnels_discover_command(db: &Database) -> Result<()> {
    let tunnels = db.find_tunnels().context("failed to find tunnels")?;
    if tunnels.is_empty() {
        println!("no tunnels (need rooms shared across multiple wings)");
    } else {
        for (room, wings) in &tunnels {
            println!("room '{}' <-> wings: {}", room, wings.join(", "));
        }
        println!("\n{} tunnel(s)", tunnels.len());
    }
    Ok(())
}
fn tunnels_list_command(db: &Database, wing: Option<&str>, kind: &str) -> Result<()> {
    let mut count = 0_usize;
    if matches!(kind, "all" | "passive") {
        for (room, wings) in db
            .find_tunnels()
            .context("failed to find passive tunnels")?
        {
            if wing.is_none_or(|f| wings.iter().any(|i| i == f)) {
                println!(
                    "passive passive_{room}: room '{room}' <-> wings: {}",
                    wings.join(", ")
                );
                count += 1;
            }
        }
    }
    if matches!(kind, "all" | "explicit") {
        for tunnel in db
            .list_explicit_tunnels(wing)
            .context("failed to list explicit tunnels")?
        {
            println!(
                "explicit {}: {} <-> {} | {}",
                tunnel.id,
                format_tunnel_endpoint(&tunnel.left),
                format_tunnel_endpoint(&tunnel.right),
                tunnel.label
            );
            count += 1;
        }
    }
    if !matches!(kind, "all" | "passive" | "explicit") {
        bail!("unsupported tunnel kind: {kind}");
    }
    if count == 0 {
        println!("no tunnels");
    } else {
        println!("\n{count} tunnel(s)");
    }
    Ok(())
}
fn parse_tunnel_endpoint(value: &str) -> Result<TunnelEndpoint> {
    let trimmed = value.trim();
    let (wing, room) = match trimmed.split_once(':') {
        Some((w, r)) => (w.trim(), Some(r.trim())),
        None => (trimmed, None),
    };
    if wing.is_empty() {
        bail!("tunnel endpoint wing is required");
    }
    Ok(TunnelEndpoint {
        wing: wing.to_string(),
        room: room.filter(|r| !r.is_empty()).map(ToOwned::to_owned),
    })
}

fn taxonomy_command(db: &Database, command: TaxonomyCommands) -> Result<()> {
    match command {
        TaxonomyCommands::List => taxonomy_list_command(db),
        TaxonomyCommands::Edit {
            wing,
            room,
            keywords,
        } => taxonomy_edit_command(db, &wing, &room, &keywords),
    }
}
fn taxonomy_list_command(db: &Database) -> Result<()> {
    let entries = db
        .taxonomy_entries()
        .context("failed to load taxonomy entries")?;
    if entries.is_empty() {
        println!("no taxonomy entries");
        return Ok(());
    }
    for entry in entries {
        let keywords = if entry.keywords.is_empty() {
            "<none>".to_string()
        } else {
            entry.keywords.join(", ")
        };
        println!(
            "- {}/{} [{}]",
            entry.wing,
            render_room(Some(entry.room.as_str())),
            keywords
        );
    }
    Ok(())
}
fn taxonomy_edit_command(db: &Database, wing: &str, room: &str, keywords: &str) -> Result<()> {
    let entry = TaxonomyEntry {
        wing: wing.to_string(),
        room: room.to_string(),
        display_name: Some(room.to_string()),
        keywords: parse_keywords_arg(keywords),
    };
    db.upsert_taxonomy_entry(&entry)
        .context("failed to update taxonomy entry")?;
    println!(
        "updated {}/{} [{}]",
        wing,
        render_room(Some(room)),
        entry.keywords.join(", ")
    );
    Ok(())
}

fn field_taxonomy_command(format: &str) -> Result<()> {
    let entries = field_taxonomy();
    match format {
        "plain" => print_field_taxonomy(&entries),
        "json" => println!(
            "{}",
            serde_json::to_string_pretty(&entries).context("failed to serialize field taxonomy")?
        ),
        other => bail!("unsupported field taxonomy format: {other}"),
    }
    Ok(())
}
fn print_field_taxonomy(entries: &[FieldTaxonomyEntry]) {
    for entry in entries {
        println!(
            "- {} domains={} examples={} :: {}",
            entry.field,
            entry.domains.join(","),
            entry.examples.join("; "),
            entry.description
        );
    }
}

fn fact_check_command(
    db: &Database,
    path: Option<&Path>,
    wing: Option<&str>,
    room: Option<&str>,
    now: Option<String>,
) -> Result<()> {
    use std::io::Read;
    let text = match path {
        Some(p) if p.as_os_str() == "-" => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("failed to read stdin")?;
            buf
        }
        Some(p) => {
            std::fs::read_to_string(p).with_context(|| format!("failed to read {}", p.display()))?
        }
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("failed to read stdin")?;
            buf
        }
    };
    let now_secs = mempal::factcheck::resolve_now(now.as_deref())?;
    let scope = mempal::factcheck::validate_scope(wing, room)?;
    let report =
        mempal::factcheck::check(&text, db, now_secs, scope).context("fact check failed")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).context("failed to serialize fact-check report")?
    );
    Ok(())
}

fn config_command(config: &Config, command: &ConfigCommands) -> Result<()> {
    match command {
        ConfigCommands::Intelligence => config_intelligence_command(config),
    }
}

fn config_intelligence_command(config: &Config) -> Result<()> {
    let effective_llm = config.memory_intelligence.effective_llm_config(&config.llm);
    let llm_state = if !config.memory_intelligence.mode.uses_llm() {
        "disabled"
    } else if config
        .memory_intelligence
        .has_effective_llm_endpoint(&config.llm)
    {
        "healthy"
    } else {
        "disabled"
    };
    println!("memory_intelligence:");
    println!("  mode: {}", config.memory_intelligence.mode);
    println!("  llm_state: {llm_state}");
    println!("  llm_backend: {}", effective_llm.backend);
    match effective_llm.model.as_deref() {
        Some(model) => println!("  llm_model: {model}"),
        None => println!("  llm_model: none"),
    }
    match effective_llm.base_url.as_deref() {
        Some(base_url) => println!("  llm_base_url: {base_url}"),
        None => println!("  llm_base_url: none"),
    }
    println!("  timeout_secs: {}", effective_llm.request_timeout_secs);
    println!(
        "  extra_body: {}",
        if effective_llm.extra_body.is_some() {
            "configured"
        } else {
            "none"
        }
    );
    Ok(())
}

fn status_command(db: &Database, config: &Config, full: bool) -> Result<()> {
    let cfg_meta = ConfigHandle::snapshot_meta();
    let scrub_stats = ConfigHandle::scrub_stats();
    let runtime_warnings = ConfigHandle::collect_runtime_warnings();
    let endpoint_health = if full {
        Some(
            mempal::endpoint_health::probe_endpoints_blocking(config)
                .context("failed to probe endpoint health")?,
        )
    } else {
        None
    };
    let embed_status = global_embed_status().snapshot();
    let intelligence_status = mempal::intelligence::global_intelligence_status().snapshot();
    let queue_stats = mempal::core::queue::queue_stats_readonly(db.path())
        .context("failed to query pending message stats")?;
    let schema_version = db
        .schema_version()
        .context("failed to read schema version")?;
    let fork_ext_version = db
        .conn()
        .query_row(
            "SELECT value FROM fork_ext_meta WHERE key = 'fork_ext_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    let drawer_count = db.drawer_count().context("failed to count drawers")?;
    let source_type_counts = db
        .source_type_counts()
        .context("failed to count drawers per source type")?;
    let pinned_fact_counts = db
        .pinned_fact_counts_by_project()
        .context("failed to count pinned facts per project")?;
    let raw_turn_count =
        count_raw_turn_drawers(db, &config.turns).context("failed to count raw turn drawers")?;
    let project_breakdown: Option<Vec<(Option<String>, i64)>> = if full {
        Some(
            db.project_breakdown()
                .context("failed to count drawers per project")?,
        )
    } else {
        None
    };
    let null_project_backfill_pending = db
        .null_project_backfill_pending_count()
        .context("failed to count pending project backfill drawers")?;
    let taxonomy_count = db.taxonomy_count().context("failed to count taxonomy")?;
    let gating_drop_counts = db
        .gating_drop_counts()
        .context("failed to read gating counters")?;
    let gating_stats =
        observability::gating_stats(db, config, None).context("failed to read gating stats")?;
    let dropped_total = gating_drop_counts
        .total
        .unwrap_or_else(|| gating_drop_counts.by_reason.values().copied().sum::<u64>());
    let ingest_gating_status = observability::gating_runtime_status(
        db,
        config,
        dropped_total,
        ConfigHandle::restart_required_pending(),
    )
    .context("failed to build ingest gating status")?;
    let db_size_bytes = db
        .database_size_bytes()
        .context("failed to compute database size")?;
    let deleted_count = db
        .deleted_drawer_count()
        .context("failed to count deleted drawers")?;
    let consolidation_stats = db
        .consolidation_stats()
        .context("failed to read consolidation stats")?;
    let pending_card_count = db
        .pending_auto_generated_knowledge_card_count()
        .context("failed to count pending auto-generated cards")?;
    let last_crystallization_at = db
        .last_crystallization_at()
        .context("failed to read last crystallization timestamp")?;
    let daemon_pid = read_daemon_pid(db.path())?;
    let daemon_running = daemon_pid
        .map(process_is_running)
        .transpose()
        .context("failed to probe daemon pid liveness")?
        .unwrap_or(false);
    let last_heartbeat = db
        .conn()
        .query_row(
            "SELECT MAX(heartbeat_at) FROM pending_messages WHERE heartbeat_at IS NOT NULL",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )
        .context("failed to query daemon heartbeat")?;
    println!("schema_version: {schema_version}");
    println!("fork_ext_version: {fork_ext_version}");
    println!(
        "vector_index_stale: {}",
        db.vector_index_is_stale()
            .context("failed to check vector index metric")?
    );
    // #302: the metric-only staleness check above reports false for an
    // empty-but-correct-metric table, so surface the raw vector row count and an
    // explicit empty flag (rows == 0 while drawers exist) next to it.
    let vector_rows = db
        .vector_row_count()
        .context("failed to count vector index rows")?;
    println!("vector_rows: {vector_rows}");
    println!(
        "vector_index_empty: {}",
        vector_rows == 0 && drawer_count > 0
    );
    println!("search_decay_mode: {}", config.search.decay.mode);
    println!("drawer_count: {drawer_count}");
    match project_breakdown {
        Some(breakdown) => {
            println!("drawers per project:");
            if breakdown.is_empty() {
                println!("(none)");
            } else {
                for (pid, count) in breakdown {
                    match pid {
                        Some(pid) => println!("{}={count}", escape_project_id_for_display(&pid)),
                        None => println!("NULL={count}"),
                    }
                }
            }
        }
        None => println!("drawers per project: (use --full for breakdown)"),
    }
    println!(
        "null_project_backfill_pending: {}",
        null_project_backfill_pending > 0
    );
    if null_project_backfill_pending > 0 {
        println!("null_project_backfill_count: {null_project_backfill_pending}");
    }
    if deleted_count > 0 {
        println!("deleted_drawers: {deleted_count} (use `mempal purge` to remove)");
    }
    println!("Consolidation:");
    println!(
        "  total_compacted_drawers: {}",
        consolidation_stats.total_compacted_drawers
    );
    println!(
        "  consolidation_runs: {}",
        consolidation_stats.consolidation_runs
    );
    match consolidation_stats.last_consolidation_at.as_deref() {
        Some(last) => println!("  last_consolidation_at: {last}"),
        None => println!("  last_consolidation_at: none"),
    }
    println!("Sleep:");
    match consolidation_stats.last_sleep_at.as_deref() {
        Some(last) => println!("  last_sleep_at: {last}"),
        None => println!("  last_sleep_at: none"),
    }
    println!("  items_pruned: {}", consolidation_stats.sleep_items_pruned);
    println!(
        "  items_compacted: {}",
        consolidation_stats.sleep_items_compacted
    );
    println!(
        "  conflicts_resolved: {}",
        consolidation_stats.sleep_conflicts_resolved
    );
    println!("Crystallize:");
    println!("  pending_card_count: {pending_card_count}");
    match last_crystallization_at.as_deref() {
        Some(last) => println!("  last_crystallization_at: {last}"),
        None => println!("  last_crystallization_at: none"),
    }
    let triple_count = db.triple_count().context("failed to count triples")?;
    println!("taxonomy_entries: {taxonomy_count}");
    if triple_count > 0 {
        println!("triples: {triple_count}");
    }
    println!("db_size_bytes: {db_size_bytes}");
    println!("Source Types:");
    if source_type_counts.is_empty() {
        println!("  none");
    } else {
        for (source_type, count) in source_type_counts {
            println!("  {source_type}: {count}");
        }
    }
    println!("Pinned Facts:");
    if pinned_fact_counts.is_empty() {
        println!("  none");
    } else {
        for (project_id, count) in pinned_fact_counts {
            match project_id {
                Some(project_id) => {
                    println!("  {}: {count}", escape_project_id_for_display(&project_id))
                }
                None => println!("  NULL: {count}"),
            }
        }
    }
    println!("Turns:");
    println!("  storage_mode: {}", config.turns.storage_mode);
    println!("  default_importance: {}", config.turns.default_importance);
    println!("  raw_turn_count: {raw_turn_count}");
    println!(
        "  raw_turn_wings: {}",
        display_list_or_none(&config.turns.raw_turn_wings)
    );
    println!(
        "  raw_turn_rooms: {}",
        display_list_or_none(&config.turns.raw_turn_rooms)
    );
    println!(
        "config: version={} loaded_unix_ms={}",
        cfg_meta.version, cfg_meta.loaded_at_unix_ms
    );
    let embed_failure_headline =
        mempal::core::queue::failure_headline_count(embed_status.fail_count, &queue_stats);
    println!("embed_fail_count: {embed_failure_headline}");
    println!("embed_degraded: {}", embed_status.degraded);
    if let Some(last_error) = embed_status.last_error {
        println!("embed_last_error: {last_error}");
    }
    if let Some(last_success_at) = embed_status.last_success_at_unix_ms {
        println!("embed_last_success_at_unix_ms: {last_success_at}");
    }
    match &endpoint_health {
        Some(health) => {
            println!("Endpoints:");
            println!("  embedding: {}", health.embedding.display());
            println!("  llm: {}", health.llm.display());
        }
        None => println!("Endpoints: (use --full to probe)"),
    }
    println!("Daemon:");
    println!("  running: {daemon_running}");
    match daemon_pid {
        Some(pid) => println!("  pid: {pid}"),
        None => println!("  pid: none"),
    }
    match last_heartbeat {
        Some(hb) => println!("  last_heartbeat_unix_secs: {hb}"),
        None => println!("  last_heartbeat_unix_secs: none"),
    }
    let db_holders = mempal::process_diagnostics::inspect_db_holders(db.path());
    print_db_holder_report("DB Holders", &db_holders, "  ");
    println!("Queue:");
    println!("  pending: {}", queue_stats.pending);
    println!("  claimed: {}", queue_stats.claimed);
    println!("  failed: {}", queue_stats.failed);
    println!("  rate_per_min: {:.1}", queue_stats.rate_per_min);
    match queue_stats.oldest_pending_age_secs {
        Some(age) => println!("  oldest_pending_age_secs: {age}"),
        None => println!("  oldest_pending_age_secs: none"),
    }
    match queue_stats.avg_processing_ms {
        Some(avg) => println!("  avg_processing_ms: {avg}"),
        None => println!("  avg_processing_ms: n/a"),
    }
    match queue_stats.eta_secs {
        Some(eta) => println!("  eta_secs: {eta}"),
        None => println!("  eta_secs: n/a"),
    }
    println!("Scrub:");
    println!(
        "  total_patterns_matched: {}",
        scrub_stats.total_patterns_matched
    );
    println!("  bytes_redacted: {}", scrub_stats.bytes_redacted);
    if scrub_stats.redactions_per_pattern.is_empty() {
        println!("  redactions_per_pattern: none");
    } else {
        let per = scrub_stats
            .redactions_per_pattern
            .iter()
            .map(|(p, c)| format!("{p}={c}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("  redactions_per_pattern: {per}");
    }
    println!("Gating:");
    println!("  enabled: {}", ingest_gating_status.enabled);
    println!("  tier1_active: {}", ingest_gating_status.tier1_active);
    println!("  tier2_active: {}", ingest_gating_status.tier2_active);
    println!("  llm_active: {}", ingest_gating_status.llm_active);
    match ingest_gating_status.llm_model.as_deref() {
        Some(model) => println!("  llm_model: {model}"),
        None => println!("  llm_model: none"),
    }
    println!(
        "  tier2_threshold: {}",
        ingest_gating_status.tier2_threshold
    );
    match ingest_gating_status.llm_threshold {
        Some(threshold) => println!("  llm_threshold: {threshold}"),
        None => println!("  llm_threshold: none"),
    }
    println!("  rules_count: {}", ingest_gating_status.rules_count);
    println!(
        "  tier1_skip_events: {}",
        display_list_or_none(&ingest_gating_status.tier1_skip_events)
    );
    println!("  kept: {}", gating_stats.kept);
    println!("  skipped: {}", gating_stats.skipped);
    println!("  tier1_kept: {}", gating_stats.tier1_kept);
    println!("  tier1_skipped: {}", gating_stats.tier1_skipped);
    println!("  tier2_kept: {}", gating_stats.tier2_kept);
    println!("  tier2_skipped: {}", gating_stats.tier2_skipped);
    println!("  unclassified: {}", gating_stats.unclassified);
    let nonzero = gating_drop_counts
        .by_reason
        .iter()
        .filter_map(|(r, c)| (*c > 0).then_some(format!("{r}={c}")))
        .collect::<Vec<_>>();
    println!("  dropped_total: {dropped_total}");
    println!(
        "  quarantined_total: {}",
        ingest_gating_status.quarantined_total
    );
    println!(
        "  soft_deleted_total: {}",
        ingest_gating_status.soft_deleted_total
    );
    match ingest_gating_status.last_keep_at {
        Some(timestamp) => println!("  last_keep_at: {timestamp}"),
        None => println!("  last_keep_at: none"),
    }
    match ingest_gating_status.last_skip_at {
        Some(timestamp) => println!("  last_skip_at: {timestamp}"),
        None => println!("  last_skip_at: none"),
    }
    match ingest_gating_status.last_llm_success_at {
        Some(timestamp) => println!("  last_llm_success_at: {timestamp}"),
        None => println!("  last_llm_success_at: none"),
    }
    match ingest_gating_status.last_llm_failure_at {
        Some(timestamp) => println!("  last_llm_failure_at: {timestamp}"),
        None => println!("  last_llm_failure_at: none"),
    }
    if ingest_gating_status
        .restart_required_config_changes
        .is_empty()
    {
        println!("  restart_required_config_changes: none");
    } else {
        println!(
            "  restart_required_config_changes: {}",
            ingest_gating_status
                .restart_required_config_changes
                .join(" | ")
        );
    }
    if nonzero.is_empty() {
        println!("  dropped_by_reason: none");
    } else {
        println!("  dropped_by_reason: {}", nonzero.join(", "));
    }
    println!("LLM:");
    println!("  enabled: {}", config.llm.enabled);
    if config.llm.enabled {
        println!("  backend: {}", config.llm.backend);
        if let Some(model) = config.llm.model.as_deref() {
            println!("  model: {model}");
        }
        println!("  max_concurrent: {}", config.llm.max_concurrent);
        let llm_pending = queue_stats.pending;
        println!("  queue_pending: {llm_pending}");
    }
    println!("Intelligence:");
    println!("  mode: {}", config.memory_intelligence.mode);
    let intelligence_llm_state = if !config.memory_intelligence.mode.uses_llm() {
        "disabled"
    } else if config
        .memory_intelligence
        .has_effective_llm_endpoint(&config.llm)
    {
        match &endpoint_health {
            Some(health) if !health.llm.reachable => "degraded",
            _ => "healthy",
        }
    } else {
        "disabled"
    };
    println!("  llm_state: {intelligence_llm_state}");
    match intelligence_status.last_success_at_unix_ms {
        Some(last_success) => println!("  last_success_at_unix_ms: {last_success}"),
        None => println!("  last_success_at_unix_ms: none"),
    }
    println!("  failure_count: {}", intelligence_status.failure_count);
    if let Some(last_error) = intelligence_status.last_error.as_deref() {
        println!("  last_error: {last_error}");
    }
    if !runtime_warnings.is_empty() {
        println!("Warnings:");
        for w in runtime_warnings {
            println!(
                "  [{}] {} ({})",
                w.level.to_ascii_uppercase(),
                w.message,
                w.source
            );
        }
    }
    if full {
        let counts = db.scope_counts().context("failed to query scope counts")?;
        println!("scopes:");
        if counts.is_empty() {
            println!("- none");
        } else {
            for (wing, room, count) in counts {
                println!("- {wing}/{}: {count}", render_room(room.as_deref()));
            }
        }
    } else {
        println!("scopes: (use --full for breakdown)");
    }
    Ok(())
}

fn display_list_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_string()
    } else {
        values.join(", ")
    }
}

fn gating_command(db: &Database, config: &Config, command: GatingCommands) -> Result<()> {
    match command {
        GatingCommands::Stats { since } => observability::gating_stats_command(
            db,
            config,
            observability::GatingStatsOptions {
                since: since.as_deref(),
            },
        ),
    }
}

fn read_daemon_pid(db_path: &Path) -> Result<Option<i32>> {
    let Some(mempal_home) = db_path.parent() else {
        return Ok(None);
    };
    let pid_path = mempal_home.join("daemon.pid");
    let content = match std::fs::read_to_string(&pid_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to read daemon pid file {}", pid_path.display()));
        }
    };
    let pid = content
        .trim()
        .parse::<i32>()
        .with_context(|| format!("invalid daemon pid in {}", pid_path.display()))?;
    Ok(Some(pid))
}

#[cfg(unix)]
fn process_is_running(pid: i32) -> Result<bool> {
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).with_context(|| format!("failed to probe process {pid}")),
    }
}
#[cfg(not(unix))]
fn process_is_running(_pid: i32) -> Result<bool> {
    Ok(false)
}

#[derive(Debug)]
struct DaemonNotRunning;

impl std::fmt::Display for DaemonNotRunning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("daemon is not running")
    }
}

impl std::error::Error for DaemonNotRunning {}

async fn serve_command(config: &Config, mcp: bool) -> Result<()> {
    if mcp {
        return serve_mcp_command(config).await;
    }
    #[cfg(feature = "rest")]
    {
        return serve_mcp_and_rest_command(config).await;
    }
    #[cfg(not(feature = "rest"))]
    {
        serve_mcp_command(config).await
    }
}

async fn serve_mcp_command(config: &Config) -> Result<()> {
    let server = MempalMcpServer::new(expand_home(&config.db_path), config.clone())?;
    let service = server.serve_stdio().await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(feature = "rest")]
async fn serve_mcp_and_rest_command(config: &Config) -> Result<()> {
    let db_path = expand_home(&config.db_path);
    let listener = tokio::net::TcpListener::bind(DEFAULT_REST_ADDR)
        .await
        .with_context(|| format!("failed to bind REST server to {DEFAULT_REST_ADDR}"))?;
    let local_addr = listener
        .local_addr()
        .context("failed to resolve REST server address")?;
    eprintln!("REST listening on http://{local_addr}");
    let state = ApiState::new(
        db_path.clone(),
        Arc::new(ConfiguredEmbedderFactory::new(config.clone())),
    );
    let rest_state = state.clone();
    let mut rest_task = tokio::spawn(async move {
        serve_rest_api(listener, state)
            .await
            .context("REST server failed")
    });
    let server = MempalMcpServer::new(db_path, config.clone())?;
    let service = server.serve_stdio().await?;
    let mut mcp_task = Box::pin(async move {
        service.waiting().await.context("MCP server failed")?;
        Ok(())
    });
    tokio::select! { mcp_result = &mut mcp_task => { let _ = rest_state.drain_write_queue().await; rest_task.abort(); match rest_task.await { Ok(Ok(())) => {} Ok(Err(e)) => return Err(e), Err(je) if je.is_cancelled() => {} Err(je) => return Err(anyhow::Error::new(je).context("failed to join REST task")) } mcp_result } rest_result = &mut rest_task => match rest_result { Ok(Ok(())) => bail!("REST server exited unexpectedly"), Ok(Err(e)) => Err(e), Err(je) => Err(anyhow::Error::new(je).context("failed to join REST task")) } }
}

/// Parse a duration string like "7d", "24h", "10m" into a Unix epoch threshold.
fn parse_since_to_epoch(since: &str) -> Result<f64> {
    const VALID_FORMS: &str = r#"valid forms are "7d", "24h", or "30m""#;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock error: {e}"))?
        .as_secs_f64();
    let (value, unit_secs) = if let Some(value) = since.strip_suffix('d') {
        (value, 86400.0)
    } else if let Some(value) = since.strip_suffix('h') {
        (value, 3600.0)
    } else if let Some(value) = since.strip_suffix('m') {
        (value, 60.0)
    } else {
        anyhow::bail!("invalid --since duration '{since}'; {VALID_FORMS}");
    };
    if value.is_empty() {
        anyhow::bail!("invalid --since duration '{since}'; {VALID_FORMS}");
    }
    let n: f64 = value
        .parse()
        .with_context(|| format!("invalid --since duration '{since}'; {VALID_FORMS}"))?;
    let secs = n * unit_secs;
    Ok(now - secs)
}

async fn xurl_ingest_command(db: &Database, config: &Config, command: XurlCommands) -> Result<()> {
    match command {
        XurlCommands::Ingest {
            tool,
            path,
            session_id,
            json,
        } => {
            let embedder = build_embedder(config).await?;
            let vector_fingerprint = config
                .embed
                .current_vector_embedder_fingerprint(embedder.dimensions());
            let parse_cb = |name: &str, turns: usize| {
                if json {
                    eprintln!(
                        "{}",
                        serde_json::json!({"phase":"parse","file":name,"turns":turns})
                    );
                } else {
                    eprintln!("[parse] file: {name} ({turns} turns)");
                }
            };
            let embed_cb = |done: usize, total: usize| {
                if json {
                    eprintln!(
                        "{}",
                        serde_json::json!({"phase":"embed","done":done,"total":total})
                    );
                } else {
                    eprintln!("[embed] {done}/{total} turns vectorized");
                }
            };
            let stats = if let Some(p) = path {
                let t =
                    tool.ok_or_else(|| anyhow::anyhow!("--tool is required when --path is given"))?;
                mempal::xurl::ingest::ingest_file_with_vector_fingerprint(
                    db,
                    embedder.as_ref(),
                    &p,
                    t.into(),
                    session_id.as_deref(),
                    &vector_fingerprint,
                    mempal::xurl::ingest::IngestCallbacks {
                        on_file_parsed: Some(&parse_cb),
                        on_embed_progress: Some(&embed_cb),
                    },
                )
                .await
                .context("xurl ingest failed")?
            } else {
                let cfg = mempal::xurl::ingest::AutoScanConfig::default();
                mempal::xurl::ingest::ingest_all_with_vector_fingerprint(
                    db,
                    embedder.as_ref(),
                    &cfg,
                    &vector_fingerprint,
                    Some(&parse_cb),
                    Some(&embed_cb),
                )
                .await
                .context("xurl ingest-all failed")?
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&stats).context("json serialize")?
                );
            } else {
                println!("turns parsed:   {}", stats.turns_parsed);
                println!("turns inserted: {}", stats.turns_inserted);
                println!("turns skipped:  {}", stats.turns_skipped);
                println!("turns updated:  {}", stats.turns_updated);
                println!("vectors created:{}", stats.vectors_created);
            }
            Ok(())
        }

        XurlCommands::Search {
            query,
            tool,
            session,
            since,
            limit,
            page,
            include_csa,
            include_agent_prompts,
            min_score,
            format,
        } => {
            let since_epoch = since.as_deref().map(parse_since_to_epoch).transpose()?;
            let filter = mempal::xurl::store::TurnFilter {
                tool: tool.map(Into::into),
                session_id: session,
                since_epoch,
                limit,
                offset: page * limit,
            };
            let embedder = build_embedder(config).await?;
            let result = mempal::xurl::search::search(
                db,
                embedder.as_ref(),
                &query,
                mempal::xurl::search::SearchOptions {
                    limit,
                    filter: Some(filter),
                    include_csa,
                    include_agent_prompts,
                    min_score: Some(min_score),
                },
            )
            .await
            .context("xurl search failed")?;

            match format {
                XurlFormat::Json => {
                    println!(
                        "{}",
                        serde_json::to_string(&result).context("json serialize")?
                    );
                }
                XurlFormat::Markdown => {
                    mempal::xurl::search::print_hits_markdown(&result);
                }
            }
            Ok(())
        }

        XurlCommands::Timeline {
            tool,
            session,
            since,
            limit,
            page,
            include_csa,
            include_agent_prompts,
            format,
        } => {
            let since_epoch = since.as_deref().map(parse_since_to_epoch).transpose()?;
            let filter = mempal::xurl::store::TurnFilter {
                tool: tool.map(Into::into),
                session_id: session,
                since_epoch,
                limit,
                offset: page * limit,
            };
            let turns = mempal::xurl::store::get_turns_filtered(
                db.conn(),
                filter,
                include_csa,
                include_agent_prompts,
            )
            .context("xurl timeline query failed")?;

            match format {
                XurlFormat::Json => {
                    let json_turns = mempal::xurl::store::timeline_json_turns(&turns);
                    println!(
                        "{}",
                        serde_json::to_string(&json_turns).context("json serialize")?
                    );
                }
                XurlFormat::Markdown => {
                    if turns.is_empty() {
                        println!("No turns found.");
                    } else {
                        for t in &turns {
                            println!("---");
                            println!("{}", mempal::xurl::store::format_timeline_header(t));
                            println!();
                            let preview = char_safe_preview(&t.content, 300);
                            println!("{}", preview.trim());
                            println!();
                        }
                        println!("---");
                    }
                }
            }
            Ok(())
        }

        XurlCommands::Stats {
            tool,
            session,
            since,
            json,
        } => {
            let since_epoch = since.as_deref().map(parse_since_to_epoch).transpose()?;
            let filter = mempal::xurl::store::TurnFilter {
                tool: tool.map(Into::into),
                session_id: session,
                since_epoch,
                ..Default::default()
            };
            let stats = mempal::xurl::store::get_stats_filtered(db.conn(), &filter)
                .context("xurl stats query failed")?;
            let unindexed_remaining =
                mempal::xurl::store::count_unindexed_turns_filtered(db.conn(), &filter)
                    .context("xurl unindexed count failed")?;
            if json {
                let tools: Vec<XurlStatsToolJson> = stats
                    .iter()
                    .map(|s| XurlStatsToolJson {
                        tool: s.tool.clone(),
                        count: s.count,
                        first: mempal::xurl::search::format_timestamp(s.min_timestamp),
                        last: mempal::xurl::search::format_timestamp(s.max_timestamp),
                        min_timestamp: s.min_timestamp,
                        max_timestamp: s.max_timestamp,
                    })
                    .collect();
                let report = XurlStatsJson {
                    tools,
                    unindexed_remaining,
                };
                println!(
                    "{}",
                    serde_json::to_string(&report).context("json serialize")?
                );
                return Ok(());
            }
            if stats.is_empty() {
                println!("No conversation turns indexed yet. Run `mempal xurl ingest` first.");
                return Ok(());
            }
            println!("| tool   | turns | first                | last                 |");
            println!("|--------|------:|----------------------|----------------------|");
            for s in &stats {
                let first = mempal::xurl::search::format_timestamp(s.min_timestamp);
                let last = mempal::xurl::search::format_timestamp(s.max_timestamp);
                println!("| {:<6} | {:>5} | {} | {} |", s.tool, s.count, first, last);
            }
            println!();
            println!("unindexed_remaining: {unindexed_remaining}");
            if unindexed_remaining > 0 {
                println!("(run `mempal xurl reindex` to embed the backlog)");
            }
            Ok(())
        }

        XurlCommands::Reindex {
            stale,
            force,
            dry_run,
            json,
        } => {
            if stale && force {
                bail!("--stale and --force are mutually exclusive");
            }
            if dry_run {
                let (mode, summary) = if force {
                    (
                        "force",
                        mempal::xurl::embed::summarize_all_turns(db.conn())
                            .context("xurl force reindex dry-run query failed")?,
                    )
                } else if stale {
                    let embedder = build_embedder(config).await?;
                    let vector_fingerprint = config
                        .embed
                        .current_vector_embedder_fingerprint(embedder.dimensions());
                    (
                        "stale",
                        mempal::xurl::embed::summarize_stale_turns(
                            db.conn(),
                            &vector_fingerprint,
                            embedder.dimensions(),
                        )
                        .context("xurl stale reindex dry-run query failed")?,
                    )
                } else {
                    (
                        "missing",
                        mempal::xurl::store::summarize_unindexed_turns(db.conn())
                            .context("xurl reindex dry-run query failed")?,
                    )
                };
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "dry_run": true,
                            "mode": mode,
                            "threads_would_process": summary.threads,
                            "turns_would_process": summary.turns,
                        })
                    );
                } else {
                    println!("dry-run: true");
                    println!("mode: {mode}");
                    println!("threads would process: {}", summary.threads);
                    println!("turns would process:   {}", summary.turns);
                    println!("vectors written:       0");
                }
                return Ok(());
            }
            let embedder = build_embedder(config).await?;
            let vector_fingerprint = config
                .embed
                .current_vector_embedder_fingerprint(embedder.dimensions());
            let embed_cb = |done: usize, total: usize| {
                if json {
                    eprintln!(
                        "{}",
                        serde_json::json!({"phase":"embed","done":done,"total":total})
                    );
                } else {
                    eprintln!("[embed] {done}/{total} turns vectorized");
                }
            };
            let (mode, stats) = if force {
                (
                    "force",
                    mempal::xurl::embed::embed_all_turns(
                        db,
                        embedder.as_ref(),
                        &vector_fingerprint,
                        Some(&embed_cb),
                    )
                    .await
                    .context("xurl force reindex failed")?,
                )
            } else if stale {
                (
                    "stale",
                    mempal::xurl::embed::embed_stale_turns(
                        db,
                        embedder.as_ref(),
                        &vector_fingerprint,
                        Some(&embed_cb),
                    )
                    .await
                    .context("xurl stale reindex failed")?,
                )
            } else {
                (
                    "missing",
                    mempal::xurl::embed::embed_unindexed_turns_with_fingerprint(
                        db,
                        embedder.as_ref(),
                        &vector_fingerprint,
                        Some(&embed_cb),
                    )
                    .await
                    .context("xurl reindex failed")?,
                )
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "mode": mode,
                        "turns_processed": stats.turns_processed,
                        "chunks_total": stats.chunks_total,
                        "vectors_created": stats.embedded,
                        "skipped_stale_content": stats.skipped_stale_content,
                    })
                );
            } else {
                println!("mode: {mode}");
                println!("turns processed: {}", stats.turns_processed);
                println!("chunks total:    {}", stats.chunks_total);
                println!("vectors created: {}", stats.embedded);
                println!("skipped stale content: {}", stats.skipped_stale_content);
            }
            Ok(())
        }

        XurlCommands::Backfill { dry_run, json } => {
            let stats = mempal::xurl::backfill::backfill_project_paths(
                db.conn(),
                &mempal::xurl::backfill::BackfillSourceConfig::default(),
                mempal::xurl::backfill::BackfillOptions {
                    dry_run,
                    batch_size: 1_000,
                },
            )
            .context("xurl backfill failed")?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&stats).context("json serialize")?
                );
            } else {
                println!("sessions scanned:       {}", stats.sessions_scanned);
                if dry_run {
                    println!("turns would fill:       {}", stats.turns_filled);
                } else {
                    println!("turns filled:           {}", stats.turns_filled);
                }
                println!("turns skipped no source: {}", stats.turns_skipped_no_source);
                println!("turns already set:      {}", stats.turns_already_set);
                println!("batches:                {}", stats.batches);
                if !stats.by_project_path.is_empty() {
                    println!();
                    println!("| project_path | sessions | turns |");
                    println!("|--------------|---------:|------:|");
                    for (project_path, group) in &stats.by_project_path {
                        println!(
                            "| `{}` | {} | {} |",
                            project_path, group.sessions, group.turns
                        );
                    }
                }
            }
            Ok(())
        }
    }
}

async fn build_embedder(config: &Config) -> Result<Box<dyn Embedder>> {
    use mempal::embed::EmbedderFactory;
    ConfiguredEmbedderFactory::new(config.clone())
        .build()
        .await
        .context("failed to initialize embedder")
}
async fn build_specific_embedder(config: &Config, backend: &str) -> Result<Box<dyn Embedder>> {
    let mut selected = config.clone();
    selected.embed.backend = backend.to_string();
    selected.embed.fallback = None;
    build_backend_from_name(&selected, backend)
        .await
        .context("failed to initialize requested embedder")
}

#[derive(Debug, Clone)]
struct ReindexRow {
    id: String,
    content: String,
    content_hash: Option<String>,
    source_path: String,
    chunk_index: i64,
    project_id: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ReindexBatchWriteStats {
    reindexed: usize,
    skipped_concurrent_update: usize,
}

fn reindex_rows(db: &Database) -> Result<Vec<ReindexRow>> {
    let mut stmt = db.conn().prepare(r#"SELECT id, content, content_hash, COALESCE(source_file, id) AS source_path, COALESCE(chunk_index, 0) AS chunk_index, project_id FROM drawers WHERE deleted_at IS NULL ORDER BY source_path ASC, chunk_index ASC, id ASC"#).context("failed to prepare reindex query")?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ReindexRow {
                id: row.get(0)?,
                content: row.get(1)?,
                content_hash: row.get(2)?,
                source_path: row.get(3)?,
                chunk_index: row.get(4)?,
                project_id: row.get(5)?,
            })
        })
        .context("failed to query reindex rows")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to collect reindex rows")?;
    Ok(rows)
}

fn append_reindex_target_filters(
    sql: &mut String,
    values: &mut Vec<rusqlite::types::Value>,
    target: &ReindexVectorTarget,
    alias: &str,
) {
    let prefix = if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    };
    if !target.drawer_ids.is_empty() {
        let placeholders = std::iter::repeat_n("?", target.drawer_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        sql.push_str(&format!(" AND {prefix}id IN ({placeholders})"));
        values.extend(
            target
                .drawer_ids
                .iter()
                .cloned()
                .map(rusqlite::types::Value::Text),
        );
    }
    if let Some(project_id) = target.project_id.as_ref() {
        sql.push_str(&format!(" AND {prefix}project_id = ?"));
        values.push(rusqlite::types::Value::Text(project_id.clone()));
    }
    if let Some(wing) = target.wing.as_ref() {
        sql.push_str(&format!(" AND {prefix}wing = ?"));
        values.push(rusqlite::types::Value::Text(wing.clone()));
    }
    if let Some(room) = target.room.as_ref() {
        sql.push_str(&format!(" AND {prefix}room = ?"));
        values.push(rusqlite::types::Value::Text(room.clone()));
    }
}

/// Connection-local TEMP table staging the IDs of drawers whose batch failed
/// past the retry cap during a `reindex --stale` run. They stay stale and MUST
/// be excluded from every later batch query, or the stale selection (which
/// re-runs each iteration) would re-select them and loop forever.
///
/// Excluding them via a subquery against this TEMP table keeps the stale
/// query's bind-variable count constant regardless of how many drawers are
/// skipped. The previous `NOT IN (?, ?, ...)` placeholder expansion grew one
/// bind variable per skipped drawer, so a pathological run (more than
/// `SQLITE_MAX_VARIABLE_NUMBER` = 32766 skipped IDs) overflowed the limit and
/// crashed the graceful skip-continue with "too many SQL variables" — the exact
/// failure the skip-continue was meant to prevent (issue #304). The table lives
/// on the reindex loop's connection, so it persists across iterations and is
/// auto-dropped when the connection closes.
const REINDEX_SKIPPED_TABLE: &str = "tmp_reindex_skipped";
const REINDEX_PENDING_TABLE: &str = "tmp_reindex_pending";

/// Stale-detection snapshot query for the "vectors already exist" path.
///
/// Issue #341: vector presence MUST be checked with a correlated `EXISTS`
/// point-lookup, never a set-based `LEFT JOIN drawer_vectors` against the
/// sqlite-vec `vec0` virtual table. `vec0` reliably serves a point lookup by
/// `id` but not a plain set equi-join inside a larger plan; at production scale
/// the planner can resolve such a set-join to NULL for every row, which would
/// mis-flag every live drawer as stale and trigger a full-DB re-embed (a strictly
/// worse failure than the #340 finalize 0-promotion). The point lookup mirrors
/// the proven `drawer_vector_exists` (`SELECT EXISTS(SELECT 1 FROM
/// drawer_vectors WHERE id = ?)`) and the #340 finalize-sweep fix. `{pending_table}`
/// is substituted with `REINDEX_PENDING_TABLE` at call time; the two `?`
/// placeholders bind the current index version and target fingerprint.
const REINDEX_STALE_PENDING_VECTORED_SQL: &str = r#"
                INSERT INTO {pending_table} (id, source_path, chunk_index)
                SELECT d.id,
                       COALESCE(d.source_file, d.id) AS source_path,
                       COALESCE(d.chunk_index, 0) AS chunk_index
                FROM drawers d
                LEFT JOIN fork_ext_meta idx
                  ON idx.key = 'reindex:' || d.id || ':index_version'
                LEFT JOIN fork_ext_meta legacy_idx
                  ON legacy_idx.key = 'reindex:' || d.id || ':normalize_version'
                LEFT JOIN fork_ext_meta fp
                  ON fp.key = 'reindex:' || d.id || ':embedder_fingerprint'
                WHERE d.deleted_at IS NULL
                  AND (
                      NOT EXISTS (SELECT 1 FROM drawer_vectors dv WHERE dv.id = d.id)
                      OR COALESCE(idx.value, legacy_idx.value, '') != ?
                      OR COALESCE(fp.value, '') != ?
                  )
                "#;

/// Create the reindex skip TEMP table if absent. Idempotent, so any staging or
/// query call may invoke it without coordinating with the loop.
fn ensure_reindex_skipped_table(db: &Database) -> Result<()> {
    db.conn()
        .execute_batch(&format!(
            "CREATE TEMP TABLE IF NOT EXISTS {REINDEX_SKIPPED_TABLE} (id TEXT PRIMARY KEY);"
        ))
        .context("failed to create reindex skip table")?;
    Ok(())
}

/// Stage drawer IDs into the skip TEMP table (idempotent upsert). Inserts
/// row-by-row inside a single transaction so the statement's bind-variable
/// count stays constant no matter how many IDs are staged — a multi-row
/// `VALUES (?), (?), ...` insert would re-introduce the variable-limit failure
/// this mechanism exists to avoid (issue #304).
fn stage_reindex_skipped_ids(db: &Database, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    ensure_reindex_skipped_table(db)?;
    let conn = db.conn();
    conn.execute_batch("BEGIN;")
        .context("failed to begin reindex skip staging")?;
    let result = (|| -> Result<()> {
        let mut stmt = conn
            .prepare(&format!(
                "INSERT OR IGNORE INTO {REINDEX_SKIPPED_TABLE} (id) VALUES (?1)"
            ))
            .context("failed to prepare reindex skip insert")?;
        for id in ids {
            stmt.execute([id])
                .context("failed to stage skipped drawer id")?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT;")
                .context("failed to commit reindex skip staging")?;
            Ok(())
        }
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(error)
        }
    }
}

fn ensure_reindex_pending_table(db: &Database) -> Result<()> {
    db.conn()
        .execute_batch(&format!(
            "CREATE TEMP TABLE IF NOT EXISTS {REINDEX_PENDING_TABLE} (
                id TEXT PRIMARY KEY,
                source_path TEXT,
                chunk_index INTEGER
            );"
        ))
        .context("failed to create reindex pending table")?;
    Ok(())
}

fn reindex_vectors_exist(db: &Database) -> Result<bool> {
    db.conn()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='drawer_vectors')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .context("failed to query vector table presence")
}

fn build_reindex_stale_pending_rows(
    db: &Database,
    target_fingerprint: &str,
    target: &ReindexVectorTarget,
) -> Result<usize> {
    // The skip-exclusion subquery below references this connection-local TEMP
    // table; create it lazily so the query never fails with "no such table"
    // even when no batch has been skipped yet (issue #304).
    ensure_reindex_skipped_table(db)?;
    ensure_reindex_pending_table(db)?;
    db.conn()
        .execute_batch(&format!("DELETE FROM {REINDEX_PENDING_TABLE};"))
        .context("failed to reset reindex pending table")?;

    let vectors_exist = reindex_vectors_exist(db)?;
    let mut values = Vec::new();
    let mut sql = if vectors_exist {
        values.push(rusqlite::types::Value::Text(
            CURRENT_VECTOR_INDEX_VERSION.to_string(),
        ));
        values.push(rusqlite::types::Value::Text(target_fingerprint.to_string()));
        REINDEX_STALE_PENDING_VECTORED_SQL.replace("{pending_table}", REINDEX_PENDING_TABLE)
    } else {
        format!(
            r#"
                INSERT INTO {REINDEX_PENDING_TABLE} (id, source_path, chunk_index)
                SELECT d.id,
                       COALESCE(d.source_file, d.id) AS source_path,
                       COALESCE(d.chunk_index, 0) AS chunk_index
                FROM drawers d
                WHERE d.deleted_at IS NULL
                "#
        )
    };
    append_reindex_target_filters(&mut sql, &mut values, target, "d");
    sql.push_str(&format!(
        " AND d.id NOT IN (SELECT id FROM {REINDEX_SKIPPED_TABLE})"
    ));
    sql.push_str(" ORDER BY 2 ASC, 3 ASC, 1 ASC");
    if let Some(limit) = target.limit {
        let limit = i64::try_from(limit).context("target limit is too large")?;
        sql.push_str(" LIMIT ?");
        values.push(rusqlite::types::Value::Integer(limit));
    }
    db.conn()
        .execute(&sql, rusqlite::params_from_iter(values.iter()))
        .context("failed to build stale reindex pending rows")
}

fn pull_reindex_pending_batch(db: &Database, batch_size: usize) -> Result<Vec<ReindexRow>> {
    let limit = i64::try_from(batch_size).context("batch size is too large")?;
    ensure_reindex_pending_table(db)?;
    let mut stmt = db
        .conn()
        .prepare(&format!(
            r#"
                SELECT p.id,
                       d.content,
                       d.content_hash,
                       p.source_path,
                       p.chunk_index,
                       d.project_id
                FROM {REINDEX_PENDING_TABLE} p
                JOIN drawers d ON d.id = p.id
                ORDER BY p.rowid ASC
                LIMIT ?
                "#
        ))
        .context("failed to prepare stale reindex pending batch query")?;
    stmt.query_map([limit], |row| {
        Ok(ReindexRow {
            id: row.get(0)?,
            content: row.get(1)?,
            content_hash: row.get(2)?,
            source_path: row.get(3)?,
            chunk_index: row.get(4)?,
            project_id: row.get(5)?,
        })
    })
    .context("failed to query stale reindex pending batch")?
    .collect::<std::result::Result<Vec<_>, _>>()
    .context("failed to collect stale reindex pending batch")
}

fn delete_reindex_pending_ids(db: &Database, ids: &[String]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    ensure_reindex_pending_table(db)?;
    let mut deleted = 0usize;
    for chunk in ids.chunks(999) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("DELETE FROM {REINDEX_PENDING_TABLE} WHERE id IN ({placeholders})");
        let values = chunk
            .iter()
            .cloned()
            .map(rusqlite::types::Value::Text)
            .collect::<Vec<_>>();
        deleted += db
            .conn()
            .execute(&sql, rusqlite::params_from_iter(values.iter()))
            .context("failed to delete stale reindex pending ids")?;
    }
    Ok(deleted)
}

fn reindex_source_checkpoints(rows: &[ReindexRow]) -> Vec<(String, i64)> {
    let mut checkpoints = Vec::new();
    let mut row_index = 0usize;
    while row_index < rows.len() {
        let source_path = rows[row_index].source_path.clone();
        let mut last_chunk = rows[row_index].chunk_index;
        row_index += 1;
        while row_index < rows.len() && rows[row_index].source_path == source_path {
            last_chunk = rows[row_index].chunk_index;
            row_index += 1;
        }
        checkpoints.push((source_path, last_chunk));
    }
    checkpoints
}

#[cfg(test)]
fn reindex_pending_count(db: &Database) -> Result<i64> {
    ensure_reindex_pending_table(db)?;
    db.conn()
        .query_row(
            &format!("SELECT COUNT(*) FROM {REINDEX_PENDING_TABLE}"),
            [],
            |row| row.get::<_, i64>(0),
        )
        .context("failed to count stale reindex pending rows")
}

#[cfg(test)]
fn reindex_skipped_count(db: &Database) -> Result<i64> {
    ensure_reindex_skipped_table(db)?;
    db.conn()
        .query_row(
            &format!("SELECT COUNT(*) FROM {REINDEX_SKIPPED_TABLE}"),
            [],
            |row| row.get::<_, i64>(0),
        )
        .context("failed to count skipped stale reindex rows")
}

#[cfg(test)]
fn reindex_pending_ids(db: &Database) -> Result<Vec<String>> {
    ensure_reindex_pending_table(db)?;
    let mut stmt = db
        .conn()
        .prepare(&format!(
            "SELECT id FROM {REINDEX_PENDING_TABLE} ORDER BY rowid ASC"
        ))
        .context("failed to prepare stale reindex pending ids query")?;
    stmt.query_map([], |row| row.get::<_, String>(0))
        .context("failed to query stale reindex pending ids")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to collect stale reindex pending ids")
}

#[cfg(test)]
fn reindex_skipped_ids(db: &Database) -> Result<Vec<String>> {
    ensure_reindex_skipped_table(db)?;
    let mut stmt = db
        .conn()
        .prepare(&format!(
            "SELECT id FROM {REINDEX_SKIPPED_TABLE} ORDER BY id ASC"
        ))
        .context("failed to prepare stale reindex skipped ids query")?;
    stmt.query_map([], |row| row.get::<_, String>(0))
        .context("failed to query stale reindex skipped ids")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to collect stale reindex skipped ids")
}

#[cfg(test)]
fn reindex_vector_ids(db: &Database) -> Result<Vec<String>> {
    if !reindex_vectors_exist(db)? {
        return Ok(Vec::new());
    }
    let mut stmt = db
        .conn()
        .prepare("SELECT id FROM drawer_vectors ORDER BY id ASC")
        .context("failed to prepare vector ids query")?;
    stmt.query_map([], |row| row.get::<_, String>(0))
        .context("failed to query vector ids")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to collect vector ids")
}

fn write_reindex_vector_batch(
    db: &Database,
    rows: &[ReindexRow],
    vectors: &[Vec<f32>],
    target_fingerprint: &str,
) -> Result<ReindexBatchWriteStats> {
    use rusqlite::OptionalExtension;

    db.conn()
        .busy_timeout(std::time::Duration::from_millis(0))
        .context("failed to set fail-fast busy timeout")?;
    let begin = db.conn().execute_batch("BEGIN IMMEDIATE;");
    db.conn()
        .busy_timeout(std::time::Duration::from_secs(5))
        .context("failed to restore busy timeout")?;
    begin.context("failed to begin stale reindex batch transaction")?;

    let result = (|| -> Result<ReindexBatchWriteStats> {
        let mut stats = ReindexBatchWriteStats::default();
        for (row, vector) in rows.iter().zip(vectors) {
            let current_token = db
                .conn()
                .query_row(
                    "SELECT content_hash FROM drawers WHERE id = ?1 AND deleted_at IS NULL",
                    [&row.id],
                    |db_row| db_row.get::<_, Option<String>>(0),
                )
                .optional()
                .with_context(|| format!("failed to verify current drawer token for {}", row.id))?;
            if current_token != Some(row.content_hash.clone()) {
                stats.skipped_concurrent_update += 1;
                continue;
            }
            db.conn()
                .execute("DELETE FROM drawer_vectors WHERE id = ?1", [&row.id])
                .with_context(|| format!("failed to clear existing vector for {}", row.id))?;
            db.insert_vector_with_project(&row.id, vector, row.project_id.as_deref())
                .with_context(|| format!("failed to insert vector for {}", row.id))?;
            record_reindex_metadata(
                db,
                &row.id,
                CURRENT_VECTOR_INDEX_VERSION,
                target_fingerprint,
            )
            .with_context(|| format!("failed to record reindex metadata for {}", row.id))?;
            stats.reindexed += 1;
        }
        Ok(stats)
    })();

    match result {
        Ok(stats) => {
            db.conn()
                .execute_batch("COMMIT;")
                .context("failed to commit stale reindex batch")?;
            Ok(stats)
        }
        Err(error) => {
            let _ = db.conn().execute_batch("ROLLBACK;");
            Err(error)
        }
    }
}

fn should_skip_reindex_row(
    checkpoint: Option<&mempal::core::reindex::ReindexProgressRow>,
    source_path: &str,
    chunk_index: i64,
) -> bool {
    let Some(cp) = checkpoint else {
        return false;
    };
    if source_path < cp.source_path.as_str() {
        return true;
    }
    if source_path > cp.source_path.as_str() {
        return false;
    }
    cp.last_processed_chunk_id
        .is_some_and(|last| chunk_index <= last)
}
fn current_vector_dim(db: &Database) -> Result<Option<usize>> {
    use rusqlite::OptionalExtension;
    let exists: bool = db.conn().query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='drawer_vectors')", [], |row| row.get(0)).context("failed to query vector table presence")?;
    if !exists {
        return Ok(None);
    }
    let dim = db
        .conn()
        .query_row(
            "SELECT vec_length(embedding) FROM drawer_vectors LIMIT 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .context("failed to read vector dimension")?
        .map(|v| v as usize);
    Ok(dim)
}
fn load_reindex_metadata(db: &Database, drawer_id: &str, field: &str) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    db.conn()
        .query_row(
            "SELECT value FROM fork_ext_meta WHERE key = ?1",
            [vector_metadata_key(drawer_id, field)],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .context("failed to load reindex metadata")
}
fn record_reindex_metadata(
    db: &Database,
    drawer_id: &str,
    normalize_version: &str,
    embedder_fingerprint: &str,
) -> Result<()> {
    db.conn().execute(r#"INSERT INTO fork_ext_meta (key, value) VALUES (?1, ?2), (?3, ?4), (?5, ?6) ON CONFLICT(key) DO UPDATE SET value = excluded.value"#, rusqlite::params![vector_metadata_key(drawer_id, "index_version"), normalize_version, vector_metadata_key(drawer_id, "normalize_version"), normalize_version, vector_metadata_key(drawer_id, "embedder_fingerprint"), embedder_fingerprint]).context("failed to write reindex metadata")?;
    Ok(())
}
fn drawer_vector_exists(db: &Database, drawer_id: &str) -> Result<bool> {
    let Some(_dim) = current_vector_dim(db)? else {
        return Ok(false);
    };
    db.conn()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM drawer_vectors WHERE id = ?1)",
            [drawer_id],
            |row| row.get::<_, bool>(0),
        )
        .context("failed to query vector existence")
}
fn reindex_row_is_stale(db: &Database, row: &ReindexRow, target_fingerprint: &str) -> Result<bool> {
    if !drawer_vector_exists(db, &row.id)? {
        return Ok(true);
    }
    if db.vector_table_distance_metric()?.as_deref() != Some(VECTOR_DISTANCE_METRIC) {
        return Ok(true);
    }
    let nv = load_reindex_metadata(db, &row.id, "index_version")?.or(load_reindex_metadata(
        db,
        &row.id,
        "normalize_version",
    )?);
    if nv.as_deref() != Some(CURRENT_VECTOR_INDEX_VERSION) {
        return Ok(true);
    }
    let fp = load_reindex_metadata(db, &row.id, "embedder_fingerprint")?;
    Ok(fp.as_deref() != Some(target_fingerprint))
}

fn expand_home(path: &str) -> PathBuf {
    mempal::core::utils::expand_home(path)
}

fn daemon_config_db_path(config_path: &Path) -> Result<PathBuf> {
    let config = Config::load_from(config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    Ok(expand_home(&config.db_path))
}

fn daemon_home_from_db_path(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(unix)]
const DAEMON_TERM_CLEANUP_BUFFER: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(unix)]
const DAEMON_TERM_GRACE: std::time::Duration =
    mempal::daemon::DAEMON_DRAIN_BUDGET.saturating_add(DAEMON_TERM_CLEANUP_BUFFER);
#[cfg(unix)]
const DAEMON_TERM_POLL: std::time::Duration = std::time::Duration::from_millis(100);

#[cfg(unix)]
#[derive(Debug, Default, PartialEq, Eq)]
struct DaemonTerminateOutcome {
    signaled: Vec<i32>,
    killed: Vec<i32>,
}

#[cfg(unix)]
trait DaemonProcessOps {
    fn signal(&mut self, pid: i32, signal: i32) -> Result<()>;
    fn is_running(&mut self, pid: i32) -> Result<bool>;
    fn sleep(&mut self, duration: std::time::Duration);
}

#[cfg(unix)]
struct RealDaemonProcessOps;

#[cfg(unix)]
impl DaemonProcessOps for RealDaemonProcessOps {
    fn signal(&mut self, pid: i32, signal: i32) -> Result<()> {
        // SAFETY: kill(2) sends a signal to a specific PID selected by exact
        // daemon argv matching. ESRCH means the process exited after planning.
        let rc = unsafe { libc::kill(pid, signal) };
        if rc == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        Err(error).with_context(|| format!("failed to signal daemon pid {pid}"))
    }

    fn is_running(&mut self, pid: i32) -> Result<bool> {
        process_is_running(pid)
    }

    fn sleep(&mut self, duration: std::time::Duration) {
        std::thread::sleep(duration);
    }
}

#[cfg(unix)]
fn collect_daemon_candidates(db_path: &Path) -> Result<(Vec<i32>, Option<i32>)> {
    collect_daemon_candidates_with_scan(db_path, mempal::daemon_singleton::enumerate_daemon_pids)
}

#[cfg(unix)]
fn collect_daemon_candidates_with_scan(
    db_path: &Path,
    enumerate_daemon_pids: impl Fn(&str, &Path) -> Vec<i32>,
) -> Result<(Vec<i32>, Option<i32>)> {
    let binary =
        mempal::daemon_singleton::current_binary_name().unwrap_or_else(|| "mempal".to_string());
    let mut candidates = enumerate_daemon_pids(&binary, db_path);

    let pidfile_pid = match read_daemon_pid(db_path)? {
        Some(pid) if process_is_running(pid)? => {
            if !candidates.contains(&pid) {
                candidates.push(pid);
            }
            Some(pid)
        }
        _ => None,
    };

    candidates.sort_unstable();
    candidates.dedup();

    Ok((candidates, pidfile_pid))
}

#[cfg(unix)]
fn terminate_daemon_targets_with_ops(
    targets: &[i32],
    ops: &mut impl DaemonProcessOps,
    grace: std::time::Duration,
    poll: std::time::Duration,
) -> Result<DaemonTerminateOutcome> {
    let mut outcome = DaemonTerminateOutcome::default();
    let mut remaining = Vec::new();
    let mut errors = Vec::new();

    for pid in targets {
        if let Err(error) = ops.signal(*pid, libc::SIGTERM) {
            errors.push(format!("SIGTERM pid {pid} failed: {error}"));
        } else {
            outcome.signaled.push(*pid);
        }
        remaining.push(*pid);
    }

    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        let mut live = Vec::new();
        for pid in remaining {
            match ops.is_running(pid) {
                Ok(true) => live.push(pid),
                Ok(false) => {}
                Err(error) => {
                    errors.push(format!("SIGTERM status pid {pid} failed: {error}"));
                }
            }
        }
        if live.is_empty() {
            if errors.is_empty() {
                return Ok(outcome);
            }
            return Err(anyhow::anyhow!(
                "daemon termination encountered errors: {}",
                errors.join("; "),
            ));
        }
        remaining = live;
        ops.sleep(poll);
    }

    for pid in remaining {
        match ops.is_running(pid) {
            Ok(true) => {
                if let Err(error) = ops.signal(pid, libc::SIGKILL) {
                    errors.push(format!("SIGKILL pid {pid} failed: {error}"));
                } else {
                    outcome.killed.push(pid);
                }
            }
            Ok(false) => {}
            Err(error) => {
                errors.push(format!("SIGKILL status pid {pid} failed: {error}"));
            }
        }
    }

    if errors.is_empty() {
        Ok(outcome)
    } else {
        Err(anyhow::anyhow!(
            "daemon termination encountered errors: {}",
            errors.join("; "),
        ))
    }
}

#[cfg(unix)]
fn terminate_daemon_targets(targets: &[i32]) -> Result<DaemonTerminateOutcome> {
    let mut ops = RealDaemonProcessOps;
    terminate_daemon_targets_with_ops(targets, &mut ops, DAEMON_TERM_GRACE, DAEMON_TERM_POLL)
}

#[cfg(unix)]
fn reap_daemon_orphans(
    db_path: &Path,
    verbose: bool,
) -> Result<mempal::daemon_singleton::DaemonReapPlan> {
    let (candidates, pidfile_pid) = collect_daemon_candidates(db_path)?;
    let plan = mempal::daemon_singleton::plan_daemon_reap(&candidates, pidfile_pid);

    if plan.targets.is_empty() {
        if verbose {
            match plan.keeper {
                Some(pid) => println!("daemon reap: keeping pid {pid}; no duplicate daemons found"),
                None => println!("daemon reap: no live daemon processes found"),
            }
        }
        return Ok(plan);
    }

    let outcome = terminate_daemon_targets(&plan.targets)?;
    if verbose {
        match plan.keeper {
            Some(pid) => println!("daemon reap: keeping pid {pid}"),
            None => println!("daemon reap: no keeper elected"),
        }
        println!(
            "daemon reap: SIGTERM sent to {}",
            format_pid_list(&outcome.signaled)
        );
        if !outcome.killed.is_empty() {
            println!(
                "daemon reap: SIGKILL sent to {}",
                format_pid_list(&outcome.killed)
            );
        }
    }

    Ok(plan)
}

#[cfg(unix)]
fn format_pid_list(pids: &[i32]) -> String {
    pids.iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn run_daemon_start(config_path: PathBuf, foreground: bool) -> Result<()> {
    let db_path = daemon_config_db_path(&config_path)?;
    #[cfg(unix)]
    {
        let plan = reap_daemon_orphans(&db_path, false)?;
        if let Some(pid) = plan.keeper {
            bail!("daemon already running (pid {pid}); stop with `mempal daemon stop`");
        }
    }
    if let Some(pid) = read_daemon_pid(&db_path)? {
        if process_is_running(pid)? {
            bail!("daemon already running (pid {pid}); stop with `mempal daemon stop`");
        }
        // Stale pidfile: remove so bootstrap can write a fresh one.
        if let Some(mempal_home) = db_path.parent() {
            let _ = std::fs::remove_file(mempal_home.join("daemon.pid"));
        }
    }
    mempal::daemon::run_command(config_path, foreground)
}

#[cfg(unix)]
fn run_daemon_stop(db_path: &Path) -> Result<()> {
    let mempal_home = daemon_home_from_db_path(db_path);
    let (targets, _pidfile_pid) = collect_daemon_candidates(db_path)?;

    if targets.is_empty() {
        let _ = std::fs::remove_file(mempal_home.join("daemon.pid"));
        return Err(DaemonNotRunning.into());
    }

    let reaped = targets.len();
    let outcome = terminate_daemon_targets(&targets)?;

    let _ = std::fs::remove_file(mempal_home.join("daemon.pid"));
    if reaped > 1 {
        println!("daemon stopped ({reaped} processes reaped)");
    } else {
        println!("daemon stopped");
    }
    if !outcome.killed.is_empty() {
        println!(
            "daemon SIGKILL escalated for {}",
            format_pid_list(&outcome.killed)
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn run_daemon_stop(_db_path: &Path) -> Result<()> {
    bail!("daemon stop is only supported on Unix")
}

fn run_daemon_restart(config_path: PathBuf) -> Result<()> {
    let db_path = daemon_config_db_path(&config_path)?;
    if let Err(error) = run_daemon_stop(&db_path)
        && !error.is::<DaemonNotRunning>()
    {
        return Err(error);
    }
    mempal::daemon::run_command(config_path, false)
}

#[cfg(unix)]
fn run_daemon_reap(db_path: &Path) -> Result<()> {
    let mempal_home = daemon_home_from_db_path(db_path);
    let plan = reap_daemon_orphans(db_path, true)?;
    if plan.keeper.is_none() {
        let _ = std::fs::remove_file(mempal_home.join("daemon.pid"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn run_daemon_reap(_db_path: &Path) -> Result<()> {
    bail!("daemon reap is only supported on Unix")
}

#[cfg(all(test, unix))]
mod daemon_reap_tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::Write;
    use std::path::Path;

    #[derive(Default)]
    struct FakeProcessOps {
        running: BTreeMap<i32, bool>,
        signals: Vec<(i32, i32)>,
        failed_signals: BTreeSet<(i32, i32)>,
        term_ignored: BTreeSet<i32>,
    }

    impl DaemonProcessOps for FakeProcessOps {
        fn signal(&mut self, pid: i32, signal: i32) -> Result<()> {
            self.signals.push((pid, signal));
            if self.failed_signals.contains(&(pid, signal)) {
                return Err(anyhow::anyhow!("simulated signal failure for pid {pid}"));
            }
            if signal == libc::SIGKILL
                || (signal == libc::SIGTERM && !self.term_ignored.contains(&pid))
            {
                self.running.insert(pid, false);
            }
            Ok(())
        }

        fn is_running(&mut self, pid: i32) -> Result<bool> {
            Ok(self.running.get(&pid).copied().unwrap_or(false))
        }

        fn sleep(&mut self, _duration: std::time::Duration) {}
    }

    #[cfg(test)]
    fn write_pid(path: &Path, pid: i32) -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .context("failed to write pidfile")?;
        writeln!(file, "{pid}").context("failed to write pid")
    }

    #[test]
    fn test_collect_daemon_candidates_includes_running_pidfile_when_scan_is_empty() -> Result<()> {
        let tmp = tempfile::tempdir().context("tempdir")?;
        let db_path = tmp.path().join("palace.db");
        let pid_path = tmp.path().join("daemon.pid");
        let mut child = std::process::Command::new("sleep").args(["120"]).spawn()?;
        write_pid(&pid_path, child.id() as i32)?;

        let (candidates, pidfile_pid) =
            collect_daemon_candidates_with_scan(&db_path, |_binary, _db_path| Vec::new())?;

        assert_eq!(candidates, vec![child.id() as i32]);
        assert_eq!(pidfile_pid, Some(child.id() as i32));

        child.kill()?;
        child.wait()?;
        Ok(())
    }

    #[test]
    fn test_terminate_daemon_targets_aggregates_errors_and_continues() {
        let mut ops = FakeProcessOps::default();
        ops.running.insert(11, true);
        ops.running.insert(22, true);
        ops.term_ignored.insert(22);
        ops.failed_signals.insert((11, libc::SIGTERM));
        ops.failed_signals.insert((22, libc::SIGKILL));

        let error = terminate_daemon_targets_with_ops(
            &[11, 22],
            &mut ops,
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(1),
        )
        .expect_err("termination should aggregate signal failures");

        assert_eq!(
            ops.signals,
            vec![
                (11, libc::SIGTERM),
                (22, libc::SIGTERM),
                (11, libc::SIGKILL),
                (22, libc::SIGKILL),
            ]
        );
        assert!(ops.signals.contains(&(22, libc::SIGKILL)));
        let error = error.to_string();
        assert!(error.contains("SIGTERM pid 11 failed"));
        assert!(error.contains("SIGKILL pid 22 failed"));
    }

    #[test]
    fn test_terminate_daemon_targets_escalates_sigkill_after_grace() {
        let mut ops = FakeProcessOps::default();
        ops.running.insert(42, true);
        ops.term_ignored.insert(42);

        let outcome = terminate_daemon_targets_with_ops(
            &[42],
            &mut ops,
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(1),
        )
        .expect("terminate targets");

        assert_eq!(outcome.signaled, vec![42]);
        assert_eq!(outcome.killed, vec![42]);
        assert_eq!(ops.signals, vec![(42, libc::SIGTERM), (42, libc::SIGKILL)]);
        assert!(!ops.is_running(42).expect("running state"));
    }

    #[test]
    fn test_terminate_daemon_targets_does_not_kill_after_sigterm_exit() {
        let mut ops = FakeProcessOps::default();
        ops.running.insert(7, true);

        let outcome = terminate_daemon_targets_with_ops(
            &[7],
            &mut ops,
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(1),
        )
        .expect("terminate targets");

        assert_eq!(outcome.signaled, vec![7]);
        assert!(outcome.killed.is_empty());
        assert_eq!(ops.signals, vec![(7, libc::SIGTERM)]);
    }
}

fn run_daemon_status(db_path: &Path) -> Result<()> {
    // Enumerate ALL live `mempal daemon` siblings so status reports the true
    // process count and can warn on duplicates the single pidfile PID hides
    // (#257). The scan excludes this `daemon status` process itself.
    let binary =
        mempal::daemon_singleton::current_binary_name().unwrap_or_else(|| "mempal".to_string());
    let mempal_home = daemon_home_from_db_path(db_path);
    let siblings = mempal::daemon_singleton::enumerate_daemon_pids(&binary, db_path);

    match read_daemon_pid(db_path)? {
        None => {
            if siblings.is_empty() {
                println!("status: stopped");
            } else {
                println!("status: running (no pid file; orphaned daemon)");
            }
        }
        Some(pid) => {
            if process_is_running(pid)? {
                println!("status: running");
                println!("pid: {pid}");
                let pid_path = mempal_home.join("daemon.pid");
                if let Ok(meta) = std::fs::metadata(&pid_path) {
                    if let Ok(modified) = meta.modified() {
                        if let Ok(age) = std::time::SystemTime::now().duration_since(modified) {
                            println!("uptime_secs: {}", age.as_secs());
                        }
                    }
                }
                if let Ok(store) = mempal::core::queue::PendingMessageStore::new(db_path) {
                    if let Ok(stats) = store.stats() {
                        println!("queue.pending: {}", stats.pending);
                        println!("queue.claimed: {}", stats.claimed);
                        println!("queue.failed: {}", stats.failed);
                    }
                }
            } else if siblings.is_empty() {
                println!("status: stopped (stale pid file, pid {pid} not running)");
            } else {
                println!(
                    "status: running (stale pid file, pid {pid} not running; orphaned daemon)"
                );
            }
        }
    }

    if !siblings.is_empty() {
        let pids = siblings
            .iter()
            .map(i32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        println!("live_daemons: {}", siblings.len());
        println!("daemon_pids: {pids}");
    }
    if siblings.len() > 1 {
        println!(
            "warning: {} duplicate daemon processes detected (expected 1); run `mempal daemon stop` to reap orphans",
            siblings.len()
        );
    }
    let db_holders = mempal::process_diagnostics::inspect_db_holders(db_path);
    print_db_holder_report("db_holders", &db_holders, "");
    Ok(())
}
fn prime_embedder_degraded() -> bool {
    if std::env::var_os("MEMPAL_TEST_EMBED_DEGRADED").is_some() {
        return true;
    }
    global_embed_status().is_degraded()
}

fn cowork_drain_command(
    target: String,
    cwd: Option<PathBuf>,
    cwd_source: Option<String>,
    format: String,
) -> Result<()> {
    use mempal::cowork::Tool;
    use mempal::cowork::inbox;
    let inner: Result<(), Box<dyn std::error::Error>> = (|| {
        let target_tool = Tool::from_target_str(&target)
            .ok_or_else(|| format!("invalid target `{target}`: expected claude|codex"))?;
        let mempal_home = inbox::mempal_home();
        let resolved_cwd: PathBuf = match (cwd, cwd_source.as_deref()) {
            (Some(path), None) => path,
            (None, Some("stdin-json")) => {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                let payload: serde_json::Value = serde_json::from_str(&buf)?;
                PathBuf::from(
                    payload
                        .get("cwd")
                        .and_then(|v| v.as_str())
                        .ok_or("stdin JSON payload missing `cwd` string field")?,
                )
            }
            (None, Some(other)) => return Err(format!("unsupported --cwd-source: {other}").into()),
            (None, None) => std::env::current_dir()?,
            (Some(_), Some(_)) => unreachable!("clap conflicts_with prevents this"),
        };
        let messages = inbox::drain(&mempal_home, target_tool, &resolved_cwd)?;
        if messages.is_empty() {
            return Ok(());
        }
        let partner = target_tool
            .partner()
            .ok_or("target has no partner (auto)")?;
        let out = match format.as_str() {
            "plain" => inbox::format_plain(partner, &messages),
            "codex-hook-json" => inbox::format_codex_hook_json(partner, &messages)?,
            _ => return Err(format!("unknown format: {format}").into()),
        };
        print!("{out}");
        Ok(())
    })();
    if let Err(e) = inner {
        eprintln!("mempal cowork-drain: {e}");
    }
    Ok(())
}

fn cowork_status_command(cwd: PathBuf) -> Result<()> {
    use mempal::cowork::Tool;
    use mempal::cowork::inbox;
    let mempal_home = inbox::mempal_home();
    println!("Project: {}", cwd.display());
    println!();
    for target in [Tool::Claude, Tool::Codex] {
        let path = match inbox::inbox_path(&mempal_home, target, &cwd) {
            Ok(p) => p,
            Err(_) => {
                println!("{} inbox:  <invalid cwd>", target.dir_name());
                continue;
            }
        };
        if !path.exists() {
            println!("{} inbox:  0 messages", target.dir_name());
            continue;
        }
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let count = content.lines().filter(|l| !l.trim().is_empty()).count();
        let bytes = content.len();
        println!(
            "{} inbox:  {} message{}, {} B",
            target.dir_name(),
            count,
            if count == 1 { "" } else { "s" },
            bytes
        );
        for line in content.lines().take(3) {
            if let Ok(msg) = serde_json::from_str::<inbox::InboxMessage>(line) {
                println!("  from {} @ {}: {}", msg.from, msg.pushed_at, msg.content);
            }
        }
    }
    Ok(())
}

fn cowork_install_hooks_command(global_codex: bool) -> Result<()> {
    let inner: Result<(), Box<dyn std::error::Error>> = (|| {
        let cwd = std::env::current_dir()?;
        let claude_dir = cwd.join(".claude/hooks");
        std::fs::create_dir_all(&claude_dir)?;
        let claude_script = claude_dir.join("user-prompt-submit.sh");
        let claude_content = "#!/bin/bash\n# mempal cowork inbox drain\nmempal cowork-drain --target claude --cwd \"${CLAUDE_PROJECT_CWD:-$PWD}\" 2>/dev/null || true\n";
        std::fs::write(&claude_script, claude_content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&claude_script)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&claude_script, perms)?;
        }
        println!("installed Claude Code hook at {}", claude_script.display());
        const CANONICAL_CLAUDE_CMD: &str = "bash .claude/hooks/user-prompt-submit.sh";
        let settings_path = cwd.join(".claude/settings.json");
        let mut settings: serde_json::Value = if settings_path.exists() {
            let s = std::fs::read_to_string(&settings_path)?;
            serde_json::from_str(&s)
                .map_err(|e| format!("refusing to overwrite .claude/settings.json: {e}"))?
        } else {
            serde_json::json!({ "hooks": {} })
        };
        if !settings.is_object() {
            return Err("refusing to overwrite .claude/settings.json: not an object".into());
        }
        let hooks_field = settings
            .as_object_mut()
            .ok_or("settings root not object")?
            .entry("hooks")
            .or_insert_with(|| serde_json::json!({}));
        if !hooks_field.is_object() {
            return Err("`hooks` field is not an object".into());
        }
        let event_arr = hooks_field
            .as_object_mut()
            .ok_or("hooks not object")?
            .entry("UserPromptSubmit")
            .or_insert_with(|| serde_json::json!([]));
        let event_arr = event_arr
            .as_array_mut()
            .ok_or("UserPromptSubmit not array")?;
        let entry_has_drain = |entry: &serde_json::Value| -> Option<bool> {
            let hooks = entry.get("hooks")?.as_array()?;
            for h in hooks {
                let cmd = h.get("command")?.as_str()?;
                if cmd == CANONICAL_CLAUDE_CMD {
                    return Some(true);
                }
                if cmd.contains("user-prompt-submit.sh") || cmd.contains("mempal cowork-drain") {
                    return Some(false);
                }
            }
            None
        };
        let mut canonical_count = 0usize;
        let mut has_stale = false;
        for entry in event_arr.iter() {
            match entry_has_drain(entry) {
                Some(true) => canonical_count += 1,
                Some(false) => has_stale = true,
                None => {}
            }
        }
        if has_stale || canonical_count != 1 {
            event_arr.retain(|e| entry_has_drain(e).is_none());
            event_arr.push(serde_json::json!({ "hooks": [{ "type": "command", "command": CANONICAL_CLAUDE_CMD }] }));
            std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
            if has_stale {
                println!("healed stale Claude Code drain hook");
            }
            println!("registered Claude Code hook in {}", settings_path.display());
        } else {
            println!("= Claude Code hook already registered (no-op)");
        }
        if global_codex {
            let home = match std::env::var_os("HOME") {
                Some(h) => PathBuf::from(h),
                None => return Err("cannot resolve $HOME".into()),
            };
            let codex_dir = home.join(".codex");
            std::fs::create_dir_all(&codex_dir)?;
            let hooks_path = codex_dir.join("hooks.json");
            let mut root: serde_json::Value = if hooks_path.exists() {
                serde_json::from_str(&std::fs::read_to_string(&hooks_path)?)?
            } else {
                serde_json::json!({ "hooks": {} })
            };
            if !root.is_object() {
                root = serde_json::json!({ "hooks": {} });
            }
            let hooks_field = root
                .as_object_mut()
                .ok_or("hooks.json root not object")?
                .entry("hooks")
                .or_insert_with(|| serde_json::json!({}));
            let event_arr = hooks_field
                .as_object_mut()
                .ok_or("hooks not object")?
                .entry("UserPromptSubmit")
                .or_insert_with(|| serde_json::json!([]));
            let event_arr = event_arr
                .as_array_mut()
                .ok_or("UserPromptSubmit not array")?;
            const CANONICAL_CODEX_CMD: &str = "mempal cowork-drain --target codex --format codex-hook-json --cwd-source stdin-json";
            let entry_has_drain = |entry: &serde_json::Value| -> Option<bool> {
                let hooks = entry.get("hooks")?.as_array()?;
                for h in hooks {
                    let cmd = h.get("command")?.as_str()?;
                    if cmd == CANONICAL_CODEX_CMD {
                        return Some(true);
                    }
                    if cmd.contains("mempal cowork-drain") {
                        return Some(false);
                    }
                }
                None
            };
            let mut canonical_count = 0usize;
            let mut has_stale = false;
            for entry in event_arr.iter() {
                match entry_has_drain(entry) {
                    Some(true) => canonical_count += 1,
                    Some(false) => has_stale = true,
                    None => {}
                }
            }
            if has_stale || canonical_count != 1 {
                event_arr.retain(|e| entry_has_drain(e).is_none());
                event_arr.push(serde_json::json!({ "hooks": [{ "type": "command", "command": CANONICAL_CODEX_CMD, "statusMessage": "mempal cowork drain" }] }));
                std::fs::write(&hooks_path, serde_json::to_string_pretty(&root)?)?;
                println!("merged Codex hook into {}", hooks_path.display());
            } else {
                println!("= Codex hook already installed (no-op)");
            }
            if !codex_hooks_feature_enabled(&codex_dir) {
                println!();
                println!("WARNING: Codex `codex_hooks` feature is currently disabled.");
                println!("   To activate: codex features enable codex_hooks");
            }
        }
        println!();
        println!("Next steps:");
        println!("  1. Claude Code picks up settings.json changes on the next prompt");
        println!("  2. Restart Codex TUI so it re-reads ~/.codex/hooks.json");
        println!("  3. Test: ask Claude to push a test message to codex");
        Ok(())
    })();
    if let Err(e) = inner {
        eprintln!("mempal cowork-install-hooks: {e}");
        return Err(anyhow::anyhow!("cowork-install-hooks failed"));
    }
    Ok(())
}

fn codex_hooks_feature_enabled(codex_dir: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(codex_dir.join("config.toml")) else {
        return false;
    };
    for line in content.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let bare = key.trim().strip_prefix("features.").unwrap_or(key.trim());
        if bare == "codex_hooks" && val.trim() == "true" {
            return true;
        }
    }
    false
}

fn parse_keywords_arg(keywords: &str) -> Vec<String> {
    keywords
        .split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}
fn render_room(room: Option<&str>) -> &str {
    match room {
        Some(r) if !r.is_empty() => r,
        _ => "default",
    }
}
fn truncate_for_summary(content: &str, limit: usize) -> String {
    let compact = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= limit {
        return compact;
    }
    compact.chars().take(limit).collect::<String>() + "..."
}
/// Truncate `s` to at most `max_chars` characters, appending an ellipsis only
/// when truncation actually occurs. Operates on `char` boundaries so it never
/// panics on multi-byte UTF-8 input (issue #249: a byte-index slice such as
/// `&s[..300]` aborts when byte 300 lands inside a multi-byte CJK char).
fn char_safe_preview(s: &str, max_chars: usize) -> String {
    if s.chars().count() > max_chars {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}…")
    } else {
        s.to_string()
    }
}
fn estimate_wake_up_tokens(drawers: &[mempal::core::types::Drawer]) -> usize {
    drawers
        .iter()
        .map(|d| effective_wake_up_text(d).split_whitespace().count())
        .sum()
}

fn detect_rooms(dir: &Path) -> Result<Vec<String>> {
    let mut rooms = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)
            .with_context(|| format!("failed to read directory {}", current.display()))?
        {
            let entry =
                entry.with_context(|| format!("failed to read entry in {}", current.display()))?;
            let path = entry.path();
            if !path.is_dir() || should_skip_dir(&path) {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && !matches!(name, "src" | "tests")
            {
                rooms.insert(name.to_string());
            }
            stack.push(path);
        }
    }
    Ok(rooms.into_iter().collect())
}
fn should_skip_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| matches!(n, ".git" | "target" | "node_modules"))
        .unwrap_or(false)
}

fn mempal_home() -> PathBuf {
    env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".mempal"))
        .unwrap_or_else(|| PathBuf::from(".mempal"))
}

fn cowork_register_command(
    agent_id: String,
    tool: String,
    cwd: PathBuf,
    transport: String,
    tmux_target: Option<String>,
) -> Result<()> {
    use mempal::cowork::bus::register_agent;
    let home = mempal_home();
    let record = register_agent(
        &home,
        &cwd,
        RegisterAgentRequest {
            agent_id,
            tool,
            transport,
            tmux_target,
        },
    )?;
    println!("registered agent_id={}", record.agent_id);
    Ok(())
}

fn cowork_heartbeat_command(agent_id: String, cwd: PathBuf, seen_at: Option<String>) -> Result<()> {
    use mempal::cowork::bus::heartbeat_agent;
    let home = mempal_home();
    let record = heartbeat_agent(&home, &cwd, &agent_id, seen_at.as_deref())?;
    let last_seen = record.last_seen_at.as_deref().unwrap_or("-");
    println!("last_seen_at={last_seen}");
    Ok(())
}

fn cowork_agents_command(cwd: PathBuf, now: Option<String>) -> Result<()> {
    use mempal::cowork::bus::list_agent_status_at;
    let home = mempal_home();
    let statuses = list_agent_status_at(&home, &cwd, now.as_deref())?;
    for status in &statuses {
        let rec = &status.record;
        let last_seen = rec.last_seen_at.as_deref().unwrap_or("-");
        println!(
            "{} tool={} transport={} presence={} last_seen_at={}",
            rec.agent_id, rec.tool, rec.transport, status.presence, last_seen
        );
    }
    Ok(())
}

fn cowork_send_command(
    from: String,
    to: String,
    cwd: PathBuf,
    message: String,
    thread_id: Option<String>,
) -> Result<()> {
    use mempal::cowork::bus::send;
    let home = mempal_home();
    let report = send(
        &home,
        &cwd,
        SendRequest {
            from,
            targets: vec![to],
            message,
            operation: SendOperation::Send,
            thread_id,
            channel: None,
        },
    )?;
    for delivery in &report.delivered {
        println!("message_id={}", delivery.message_id);
    }
    Ok(())
}

fn cowork_agent_drain_command(agent_id: String, cwd: PathBuf) -> Result<()> {
    use mempal::cowork::bus::{drain_agent, format_agent_plain};
    let home = mempal_home();
    let messages = drain_agent(&home, &cwd, &agent_id)?;
    print!("{}", format_agent_plain(&agent_id, &messages));
    Ok(())
}

fn cowork_deliveries_command(cwd: PathBuf, agent_id: Option<String>) -> Result<()> {
    use mempal::cowork::bus::{format_delivery_statuses_plain, list_delivery_statuses};
    let home = mempal_home();
    let deliveries = list_delivery_statuses(&home, &cwd, agent_id.as_deref())?;
    print!("{}", format_delivery_statuses_plain(&deliveries));
    Ok(())
}

fn cowork_ack_command(agent_id: String, message_id: String, cwd: PathBuf) -> Result<()> {
    use mempal::cowork::bus::ack_delivery;
    let home = mempal_home();
    let status = ack_delivery(&home, &cwd, &agent_id, &message_id)?;
    println!("status={}", status.status);
    Ok(())
}

fn cowork_events_command(cwd: PathBuf, format: String, limit: usize) -> Result<()> {
    use mempal::cowork::bus::{format_events_plain, list_events};
    let home = mempal_home();
    let events = list_events(&home, &cwd, Some(limit))?;
    match format.as_str() {
        "json" => println!("{}", serde_json::to_string_pretty(&events)?),
        "plain" => print!("{}", format_events_plain(&events)),
        other => bail!("unsupported cowork-events format: {other}"),
    }
    Ok(())
}

fn cowork_channel_set_command(channel: String, agents: Vec<String>, cwd: PathBuf) -> Result<()> {
    use mempal::cowork::bus::set_channel;
    let home = mempal_home();
    set_channel(&home, &cwd, &channel, agents.clone())?;
    println!("channel={channel} members={}", agents.join(","));
    Ok(())
}

fn cowork_channel_send_command(
    from: String,
    channel: String,
    cwd: PathBuf,
    message: String,
    thread_id: Option<String>,
) -> Result<()> {
    use mempal::cowork::bus::send_channel;
    let home = mempal_home();
    let report = send_channel(&home, &cwd, from, channel, message, thread_id)?;
    for delivery in &report.delivered {
        println!("message_id={}", delivery.message_id);
    }
    Ok(())
}

fn cowork_broadcast_command(
    from: String,
    targets: Vec<String>,
    cwd: PathBuf,
    message: String,
    thread_id: Option<String>,
) -> Result<()> {
    use mempal::cowork::bus::send;
    let home = mempal_home();
    let report = send(
        &home,
        &cwd,
        SendRequest {
            from,
            targets,
            message,
            operation: SendOperation::Broadcast,
            thread_id,
            channel: None,
        },
    )?;
    for delivery in &report.delivered {
        println!("message_id={}", delivery.message_id);
    }
    Ok(())
}

fn cowork_runbook_command(format: String) -> Result<()> {
    const RUNBOOK_CONTENT: &str = "\
Multi-Agent Cowork Runbook
==========================

1. Register agents: cowork-register --agent-id <id> --tool <tool> --cwd <path>
2. Send messages: cowork-send --from <id> --to <id> --cwd <path> --message <msg>
3. Set channels: cowork-channel-set --channel <name> --agent <id> --cwd <path>
4. Channel broadcast: cowork-channel-send --from <id> --channel <name> --cwd <path> --message <msg>
5. Drain inbox: cowork-agent-drain --agent-id <id> --cwd <path>
6. Check deliveries: cowork-deliveries --cwd <path>
7. Ack message: cowork-ack --agent-id <id> --message-id <mid> --cwd <path>
8. Peek tmux pane: cowork-tmux-peek --agent-id <id> --cwd <path>
9. Doctor check: cowork-doctor --cwd <path>
10. Capture handoff: cowork-capture --cwd <path> --summary-source handoff --execute";
    match format.as_str() {
        "plain" => {
            println!("{RUNBOOK_CONTENT}");
            Ok(())
        }
        "json" => {
            let value = serde_json::json!({
                "title": "Multi-Agent Cowork Runbook",
                "content": RUNBOOK_CONTENT
            });
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        other => bail!("unknown format: {other}"),
    }
}

fn cowork_doctor_command(
    cwd: PathBuf,
    now: Option<String>,
    probe_tmux: bool,
    format: String,
) -> Result<()> {
    use mempal::cowork::bus::{doctor, format_doctor_plain};
    let home = mempal_home();
    let report = doctor(&home, &cwd, now.as_deref(), probe_tmux)?;
    match format.as_str() {
        "json" => println!("{}", serde_json::to_string_pretty(&report)?),
        "plain" => print!("{}", format_doctor_plain(&report)),
        other => bail!("unsupported cowork-doctor format: {other}"),
    }
    Ok(())
}

fn cowork_tmux_peek_command(agent_id: String, cwd: PathBuf, lines: usize) -> Result<()> {
    use mempal::cowork::bus::tmux_peek_agent;
    let home = mempal_home();
    let peek = tmux_peek_agent(&home, &cwd, &agent_id, lines)?;
    print!("{}", peek.content);
    Ok(())
}

fn cowork_session_create_command(
    cwd: PathBuf,
    session_id: String,
    title: String,
    agents: Vec<String>,
) -> Result<()> {
    use mempal::cowork::bus::create_session;
    let home = mempal_home();
    create_session(
        &home,
        &cwd,
        CreateSessionRequest {
            session_id: session_id.clone(),
            title,
            goal: None,
            agents,
            channels: vec![],
            thread_id: None,
        },
    )?;
    println!("created session_id={session_id}");
    Ok(())
}

fn cowork_sessions_command(cwd: PathBuf, format: String) -> Result<()> {
    use mempal::cowork::bus::{format_sessions_plain, list_sessions};
    let home = mempal_home();
    let sessions = list_sessions(&home, &cwd)?;
    match format.as_str() {
        "json" => println!("{}", serde_json::to_string_pretty(&sessions)?),
        "plain" => print!("{}", format_sessions_plain(&sessions)),
        other => bail!("unsupported cowork-sessions format: {other}"),
    }
    Ok(())
}

fn cowork_session_status_command(cwd: PathBuf, session_id: String, status: String) -> Result<()> {
    use mempal::cowork::bus::update_session_status;
    let home = mempal_home();
    update_session_status(&home, &cwd, &session_id, &status)?;
    println!("session_id={session_id} status={status}");
    Ok(())
}

fn cowork_session_close_command(
    cwd: PathBuf,
    session_id: String,
    capture: bool,
    execute: bool,
    format: String,
    db_path: PathBuf,
) -> Result<()> {
    use mempal::cowork::bus::{capture_handoff_to_memory, update_session_status};
    let home = mempal_home();
    let session = update_session_status(&home, &cwd, &session_id, "closed")?;
    if capture {
        let db_opt = if execute {
            Some(
                Database::open(&db_path)
                    .context("failed to open database for cowork-session-close --capture")?,
            )
        } else {
            None
        };
        let capture_report = capture_handoff_to_memory(
            db_opt.as_ref(),
            &home,
            &cwd,
            CoworkCaptureRequest {
                summary_source: "handoff".to_string(),
                wing: "cowork-capture".to_string(),
                room: None,
                thread_id: None,
                channel: None,
                session_id: Some(session_id.clone()),
                note: None,
                execute,
            },
        )?;
        match format.as_str() {
            "json" => {
                let value = serde_json::json!({
                    "session": session,
                    "capture": capture_report,
                });
                println!("{}", serde_json::to_string_pretty(&value)?);
            }
            "plain" => {
                println!("session_id={} status=closed", session.session_id);
                if let Some(drawer_id) = &capture_report.drawer_id {
                    println!("captured drawer_id={drawer_id}");
                }
            }
            other => bail!("unsupported cowork-session-close format: {other}"),
        }
    } else {
        match format.as_str() {
            "json" => println!("{}", serde_json::to_string_pretty(&session)?),
            "plain" => println!("session_id={} status=closed", session.session_id),
            other => bail!("unsupported cowork-session-close format: {other}"),
        }
    }
    Ok(())
}

fn cowork_handoff_command(cwd: PathBuf, thread_id: Option<String>, format: String) -> Result<()> {
    use mempal::cowork::bus::{build_handoff_summary, format_handoff_plain};
    let home = mempal_home();
    if !matches!(format.as_str(), "plain" | "json") {
        bail!("unsupported cowork-handoff format: {format}");
    }
    let summary = build_handoff_summary(
        &home,
        &cwd,
        HandoffFilters {
            thread_id,
            channel: None,
            session_id: None,
            limit: None,
        },
    )?;
    match format.as_str() {
        "json" => println!("{}", serde_json::to_string_pretty(&summary)?),
        _ => print!("{}", format_handoff_plain(&summary)),
    }
    Ok(())
}

fn cowork_capture_command(
    cwd: PathBuf,
    summary_source: String,
    execute: bool,
    format: String,
    db_path: PathBuf,
) -> Result<()> {
    use mempal::cowork::bus::capture_handoff_to_memory;
    if !matches!(summary_source.as_str(), "handoff") {
        bail!("unsupported cowork capture summary source: {summary_source}");
    }
    let home = mempal_home();
    let db_opt = if execute {
        Some(Database::open(&db_path).context("failed to open database for cowork-capture")?)
    } else {
        None
    };
    let report = capture_handoff_to_memory(
        db_opt.as_ref(),
        &home,
        &cwd,
        CoworkCaptureRequest {
            summary_source,
            wing: "cowork-capture".to_string(),
            room: None,
            thread_id: None,
            channel: None,
            session_id: None,
            note: None,
            execute,
        },
    )?;
    match format.as_str() {
        "json" => println!("{}", serde_json::to_string_pretty(&report)?),
        "plain" => {
            println!("writes={}", report.writes);
            if let Some(drawer_id) = &report.drawer_id {
                println!("drawer_id={drawer_id}");
            }
        }
        other => bail!("unsupported cowork-capture format: {other}"),
    }
    Ok(())
}

fn maintenance_runbook_command(format: String) -> Result<()> {
    const RUNBOOK_CONTENT: &str = "\
Mempal Maintenance Runbook
==========================

1. Validate research plan: mempal phase3 research-validate-plan
2. Ingest research plan: mempal phase3 research-ingest-plan <report.json>
3. Knowledge distill: mempal knowledge distill
4. Check adoption events: mempal phase3 adoption review
5. Capture handoff: mempal cowork-capture --cwd <path> --summary-source handoff
6. Run runtime adoption analytics: mempal phase3 adoption analytics
7. Doctor check: mempal doctor";
    match format.as_str() {
        "plain" => {
            println!("{RUNBOOK_CONTENT}");
            Ok(())
        }
        "json" => {
            let value = serde_json::json!({
                "title": "Mempal Maintenance Runbook",
                "content": RUNBOOK_CONTENT
            });
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        other => bail!("unknown format: {other}"),
    }
}

fn maintenance_guided_run_command(format: String) -> Result<()> {
    let steps = vec![
        MaintenanceStep {
            command: "mempal phase3 research-validate-plan".to_string(),
            description: "Validate current research plan against project memory".to_string(),
        },
        MaintenanceStep {
            command: "mempal phase3 adoption review".to_string(),
            description: "Review runtime adoption events for the current session".to_string(),
        },
        MaintenanceStep {
            command: "mempal cowork-doctor --cwd .".to_string(),
            description: "Health check the multi-agent cowork bus registry".to_string(),
        },
        MaintenanceStep {
            command: "mempal cowork-capture --cwd . --summary-source handoff".to_string(),
            description: "Capture cowork handoff summary to project memory".to_string(),
        },
    ];
    let report = MaintenanceGuidedRunReport {
        writes: false,
        steps,
    };
    match format.as_str() {
        "json" => {
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        "plain" => {
            println!("Guided Maintenance Run");
            println!();
            for (i, step) in report.steps.iter().enumerate() {
                println!("Step {}: {}", i + 1, step.description);
                println!("  {}", step.command);
            }
            Ok(())
        }
        other => bail!("unsupported maintenance guided-run format: {other}"),
    }
}

async fn maintenance_rejudge_command(
    db: &Database,
    config: &Config,
    options: HistoricalRejudgeOptions<'_>,
) -> Result<()> {
    if options.limit == 0 {
        bail!("--limit must be greater than zero");
    }
    if options.hard_delete && !options.execute {
        bail!("--hard-delete requires --execute");
    }
    let current_dir = env::current_dir().ok();
    let project_id = if options.project.is_some() {
        resolve_project_id(options.project, config, current_dir.as_deref())
            .context("failed to resolve historical rejudge project id")?
    } else {
        None
    };
    let rows = db
        .historical_rejudge_candidates(
            options.wing,
            options.room,
            project_id.as_deref(),
            options.limit,
        )
        .context("failed to load historical rejudge candidates")?;
    let config_version = historical_rejudge_config_version(config);
    let llm_client = historical_rejudge_llm_client(config);
    let judge_model = llm_client
        .as_ref()
        .and(config.llm.model.clone())
        .or_else(|| Some("deterministic".to_string()).filter(|_| llm_client.is_none()));

    let mut decisions = Vec::with_capacity(rows.len());
    for row in &rows {
        let decision = evaluate_historical_drawer(row, config, llm_client.as_ref()).await;
        decisions.push((row, decision));
    }

    let candidates = decisions
        .iter()
        .filter(|(_, decision)| decision.delete_candidate)
        .collect::<Vec<_>>();
    let candidate_ids = candidates
        .iter()
        .map(|(row, _)| row.drawer.id.clone())
        .collect::<Vec<_>>();

    let mutated_count = if options.execute && !candidate_ids.is_empty() {
        if options.hard_delete {
            db.hard_delete_drawers_by_ids(&candidate_ids)
                .context("failed to hard-delete historical rejudge candidates")?
        } else {
            db.soft_delete_drawers_by_ids(&candidate_ids)
                .context("failed to soft-delete historical rejudge candidates")?
        }
    } else {
        0
    };

    for (row, decision) in &decisions {
        let audit_decision = if decision.delete_candidate {
            "skip"
        } else {
            "keep"
        };
        db.record_historical_rejudge_audit(HistoricalRejudgeAudit {
            drawer_id: &row.drawer.id,
            decision: audit_decision,
            tier: decision.tier,
            reason: Some(&decision.reason),
            label: decision.label.as_deref(),
            score: decision.score,
            project_id: row.project_id.as_deref(),
            content_preview: Some(&preview_one_line(&row.drawer.content, 500)),
            judge_model: judge_model.as_deref(),
            config_version: &config_version,
            mutation: if !options.execute {
                "dry_run"
            } else if options.hard_delete {
                "hard_delete"
            } else {
                "soft_delete"
            },
        })
        .context("failed to record historical rejudge audit")?;
    }

    if options.execute {
        append_audit_entry(
            db,
            "maintenance-rejudge",
            &serde_json::json!({
                "hard_delete": options.hard_delete,
                "scanned_count": rows.len(),
                "candidate_count": candidate_ids.len(),
                "mutated_count": mutated_count,
                "drawer_ids": candidate_ids,
                "judge_model": judge_model,
                "config_version": config_version,
            }),
        )
        .context("failed to append historical rejudge audit log")?;
    }

    let report = build_historical_rejudge_report(
        !options.execute,
        if options.hard_delete {
            "hard_delete"
        } else {
            "soft_delete"
        },
        rows.len(),
        mutated_count,
        judge_model,
        config_version,
        &decisions,
    );
    print_historical_rejudge_report(&report, options.format)
}

async fn evaluate_historical_drawer(
    row: &mempal::core::db::HistoricalRejudgeCandidate,
    config: &Config,
    llm_client: Option<&LlmClient>,
) -> HistoricalRejudgeDecision {
    let drawer = &row.drawer;
    if let Some(reason) = historical_protection_reason(drawer) {
        return protected_historical_decision(reason);
    }

    let candidate = historical_ingest_candidate(drawer);
    if let Some(decision) = evaluate_tier1(&candidate, &config.ingest_gating)
        && decision.is_rejected()
    {
        return HistoricalRejudgeDecision {
            delete_candidate: true,
            protected: false,
            reason: decision
                .gating_reason
                .clone()
                .unwrap_or_else(|| "tier1_rejected".to_string()),
            label: decision.label.clone(),
            score: decision.score.map(f64::from),
            tier: decision.tier,
            judge: "deterministic".to_string(),
        };
    }

    if let Some(client) = llm_client {
        match request_historical_llm_score(client, drawer, config).await {
            Ok((score, reason)) => {
                let threshold = config
                    .ingest_gating
                    .llm_judge
                    .as_ref()
                    .map(|judge| judge.threshold)
                    .unwrap_or(0.3);
                return HistoricalRejudgeDecision {
                    delete_candidate: score < threshold,
                    protected: false,
                    reason: if score < threshold {
                        format!("llm_below_threshold:{reason}")
                    } else {
                        format!("llm_keep:{reason}")
                    },
                    label: Some("llm_judge".to_string()),
                    score: Some(score),
                    tier: 3,
                    judge: "llm".to_string(),
                };
            }
            Err(error) => {
                let deterministic = deterministic_historical_decision(drawer);
                return HistoricalRejudgeDecision {
                    reason: format!("llm_unavailable_fallback:{}:{error}", deterministic.reason),
                    ..deterministic
                };
            }
        }
    }

    deterministic_historical_decision(drawer)
}

const HIGH_HISTORICAL_IMPORTANCE_THRESHOLD: i32 = 3;

fn historical_protection_reason(drawer: &Drawer) -> Option<String> {
    if drawer.is_pinned {
        return Some("pinned".to_string());
    }
    if drawer.importance >= HIGH_HISTORICAL_IMPORTANCE_THRESHOLD {
        return Some(format!("explicit_importance_{}", drawer.importance));
    }
    if drawer.effective_importance >= f64::from(HIGH_HISTORICAL_IMPORTANCE_THRESHOLD) {
        return Some(format!(
            "effective_importance_{:.3}",
            drawer.effective_importance
        ));
    }
    if matches!(
        drawer.status,
        Some(KnowledgeStatus::Canonical | KnowledgeStatus::Promoted)
    ) {
        return Some("canonical_or_promoted".to_string());
    }
    if matches!(
        drawer.memory_kind,
        MemoryKind::Knowledge | MemoryKind::ProfileFact
    ) {
        return Some("typed_high_signal_memory".to_string());
    }
    let score = mempal::importance::score_importance(drawer);
    if score >= 3 {
        return Some(format!("importance_score_{score}"));
    }
    let content = drawer.content.to_ascii_lowercase();
    for marker in [
        "decision",
        "we decided",
        "architecture",
        "root cause",
        "bug root cause",
        "review finding",
        "user prefers",
        "explicit user",
        "canonical fact",
    ] {
        if content.contains(marker) {
            return Some(format!("high_signal_marker:{marker}"));
        }
    }
    None
}

fn historical_ingest_candidate(drawer: &Drawer) -> IngestCandidate {
    let content = drawer.content.clone();
    let lower_source = drawer
        .source_file
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let event = if drawer.wing == "hooks-raw" || lower_source.contains("hook") {
        if drawer.room.as_deref() == Some("user-prompt") {
            Some("UserPromptSubmit".to_string())
        } else {
            Some("PostToolUse".to_string())
        }
    } else {
        None
    };
    let tool_name = drawer
        .room
        .as_deref()
        .filter(|room| drawer.wing == "hooks-raw" && *room != "user-prompt")
        .map(ToOwned::to_owned)
        .or_else(|| extract_json_string_field(&content, "tool_name"));
    let exit_code = extract_json_i32_field(&content, "exit_code");
    IngestCandidate {
        content,
        event,
        tool_name,
        exit_code,
    }
}

fn deterministic_historical_decision(drawer: &Drawer) -> HistoricalRejudgeDecision {
    if let Some(reason) = historical_protection_reason(drawer) {
        return protected_historical_decision(reason);
    }

    let score = mempal::importance::score_importance(drawer);
    HistoricalRejudgeDecision {
        delete_candidate: score <= 1,
        protected: false,
        reason: if score <= 1 {
            format!("importance_score_{score}")
        } else {
            format!("importance_keep_{score}")
        },
        label: Some("importance".to_string()),
        score: Some(f64::from(score) / 5.0),
        tier: 0,
        judge: "deterministic".to_string(),
    }
}

fn protected_historical_decision(reason: String) -> HistoricalRejudgeDecision {
    HistoricalRejudgeDecision {
        delete_candidate: false,
        protected: true,
        reason,
        label: Some("protected".to_string()),
        score: Some(1.0),
        tier: 0,
        judge: "deterministic".to_string(),
    }
}

fn historical_rejudge_llm_client(config: &Config) -> Option<LlmClient> {
    if !config.llm.enabled || !config.llm.enabled_for.iter().any(|item| item == "gating") {
        return None;
    }
    let judge = config.ingest_gating.llm_judge.as_ref()?;
    if !judge.enabled {
        return None;
    }
    LlmClient::from_config(&config.llm).ok()
}

async fn request_historical_llm_score(
    client: &LlmClient,
    drawer: &Drawer,
    config: &Config,
) -> Result<(f64, String)> {
    let system_prompt = config
        .ingest_gating
        .llm_judge
        .as_ref()
        .and_then(|judge| judge.system_prompt.as_deref())
        .unwrap_or(DEFAULT_GATING_JUDGE_PROMPT);
    let request = LlmRequest {
        messages: vec![
            LlmMessage {
                role: "system".to_string(),
                content: system_prompt.to_string(),
            },
            LlmMessage {
                role: "user".to_string(),
                content: drawer.content.clone(),
            },
        ],
        model: None,
        temperature: Some(0.0),
        max_tokens: Some(512),
    };
    let response = client
        .chat_completion(&request)
        .await
        .context("historical rejudge LLM request failed")?;
    Ok(parse_historical_llm_score(&response.content))
}

fn parse_historical_llm_score(content: &str) -> (f64, String) {
    let parsed = serde_json::from_str::<serde_json::Value>(content).unwrap_or_default();
    let score = parsed
        .get("score")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| {
            let verdict = parsed
                .get("verdict")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("keep")
                .to_ascii_lowercase();
            if matches!(
                verdict.as_str(),
                "reject" | "delete" | "drop" | "quarantine"
            ) {
                0.0
            } else {
                1.0
            }
        })
        .clamp(0.0, 1.0);
    let reason = parsed
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("no_reason")
        .to_string();
    (score, reason)
}

fn extract_json_string_field(content: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .and_then(|value| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
}

fn extract_json_i32_field(content: &str, key: &str) -> Option<i32> {
    serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .and_then(|value| value.get(key).and_then(serde_json::Value::as_i64))
        .and_then(|value| i32::try_from(value).ok())
}

fn historical_rejudge_config_version(config: &Config) -> String {
    let relevant = serde_json::json!({
        "gating": config.ingest_gating,
        "llm": {
            "enabled": config.llm.enabled,
            "model": config.llm.model,
            "enabled_for": config.llm.enabled_for,
            "max_concurrent": config.llm.max_concurrent,
        }
    });
    let bytes = serde_json::to_vec(&relevant).unwrap_or_default();
    blake3::hash(&bytes).to_hex()[..12].to_string()
}

fn build_historical_rejudge_report(
    dry_run: bool,
    mutation: &str,
    scanned_count: usize,
    mutated_count: usize,
    judge_model: Option<String>,
    config_version: String,
    decisions: &[(
        &mempal::core::db::HistoricalRejudgeCandidate,
        HistoricalRejudgeDecision,
    )],
) -> HistoricalRejudgeReport {
    let mut scope_counts: BTreeMap<(String, Option<String>), usize> = BTreeMap::new();
    let mut examples = Vec::new();
    let mut estimated_bytes_reclaimed = 0usize;
    let mut candidate_count = 0usize;
    let mut protected_count = 0usize;
    for (row, decision) in decisions {
        if decision.protected {
            protected_count += 1;
        }
        if !decision.delete_candidate {
            continue;
        }
        candidate_count += 1;
        estimated_bytes_reclaimed += row.drawer.content.len();
        *scope_counts
            .entry((row.drawer.wing.clone(), row.drawer.room.clone()))
            .or_default() += 1;
        if examples.len() < 5 {
            examples.push(HistoricalRejudgeExample {
                drawer_id: row.drawer.id.clone(),
                wing: row.drawer.wing.clone(),
                room: row.drawer.room.clone(),
                reason: decision.reason.clone(),
                score: decision.score,
                judge: decision.judge.clone(),
                bytes: row.drawer.content.len(),
                preview: preview_one_line(&row.drawer.content, 120),
            });
        }
    }
    let wings_rooms = scope_counts
        .into_iter()
        .map(|((wing, room), count)| HistoricalRejudgeScopeCount { wing, room, count })
        .collect();
    HistoricalRejudgeReport {
        dry_run,
        mutation: mutation.to_string(),
        scanned_count,
        candidate_count,
        protected_count,
        mutated_count,
        estimated_bytes_reclaimed,
        judge_model,
        config_version,
        wings_rooms,
        examples,
    }
}

fn print_historical_rejudge_report(report: &HistoricalRejudgeReport, format: &str) -> Result<()> {
    match format {
        "json" => {
            println!("{}", serde_json::to_string_pretty(report)?);
            Ok(())
        }
        "plain" => {
            println!("Historical Memory Rejudge");
            println!("dry_run={}", report.dry_run);
            println!("mutation={}", report.mutation);
            println!("scanned={}", report.scanned_count);
            println!("candidates={}", report.candidate_count);
            println!("protected={}", report.protected_count);
            println!("mutated={}", report.mutated_count);
            println!(
                "estimated_bytes_reclaimed={}",
                report.estimated_bytes_reclaimed
            );
            println!(
                "judge_model={}",
                report.judge_model.as_deref().unwrap_or("none")
            );
            println!("config_version={}", report.config_version);
            if !report.wings_rooms.is_empty() {
                println!("affected_wings_rooms:");
                for scope in &report.wings_rooms {
                    println!(
                        "  {}/{} count={}",
                        scope.wing,
                        scope.room.as_deref().unwrap_or("default"),
                        scope.count
                    );
                }
            }
            if !report.examples.is_empty() {
                println!("examples:");
                for example in &report.examples {
                    println!(
                        "  {} {}/{} reason={} score={} bytes={} judge={} preview=\"{}\"",
                        example.drawer_id,
                        example.wing,
                        example.room.as_deref().unwrap_or("default"),
                        example.reason,
                        example
                            .score
                            .map(|score| format!("{score:.3}"))
                            .unwrap_or_else(|| "none".to_string()),
                        example.bytes,
                        example.judge,
                        example.preview
                    );
                }
            }
            if report.dry_run && report.candidate_count > 0 {
                println!("rerun with --execute to apply auditable soft-delete");
                println!("add --hard-delete --execute only for irreversible deletion");
            }
            Ok(())
        }
        other => bail!("unsupported maintenance rejudge format: {other}"),
    }
}

fn release_readiness_command(format: String) -> Result<()> {
    let checks = vec![
        ReleaseReadinessCheck {
            name: "cargo-metadata".to_string(),
            status: "ok".to_string(),
            detail: None,
        },
        ReleaseReadinessCheck {
            name: "spec-plan-inventory".to_string(),
            status: "ok".to_string(),
            detail: None,
        },
        ReleaseReadinessCheck {
            name: "doctor".to_string(),
            status: "ok".to_string(),
            detail: None,
        },
    ];
    let recommended_commands = vec![
        "cargo package --list".to_string(),
        "cargo test".to_string(),
        "mempal doctor".to_string(),
        "just pre-commit".to_string(),
    ];
    let report = ReleaseReadinessReport {
        writes: false,
        checks,
        recommended_commands,
    };
    match format.as_str() {
        "json" => {
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        "plain" => {
            println!("Release Readiness");
            println!();
            for check in &report.checks {
                println!("  [{}] {}", check.status, check.name);
            }
            println!();
            println!("Recommended commands:");
            for cmd in &report.recommended_commands {
                println!("  {cmd}");
            }
            Ok(())
        }
        other => bail!("unsupported release-readiness format: {other}"),
    }
}

fn doctor_command(format: String) -> Result<()> {
    if !matches!(format.as_str(), "plain" | "json") {
        bail!("unsupported doctor format: {format}");
    }
    // Best-effort: honor custom db_path from config when available.
    // Fall back to the default path so doctor remains functional when config is
    // absent or unparseable (doctor is the tool you run when the env is broken).
    let db_path = Config::load()
        .ok()
        .map(|c| expand_home(&c.db_path))
        .unwrap_or_else(|| mempal_home().join("palace.db"));
    let report = build_doctor_report(&db_path);
    match format.as_str() {
        "json" => println!("{}", serde_json::to_string_pretty(&report)?),
        _ => {
            println!("db_path={}", report.db.path);
            println!(
                "db_exists={} db_schema_version={:?}",
                report.db.exists, report.db.schema_version
            );
            println!(
                "supported_schema_version={}",
                report.supported_schema_version
            );
            println!(
                "path_matches_current_exe={:?}",
                report.install.path_matches_current_exe
            );
            if report.restart_required_config_changes.is_empty() {
                println!("restart_required_config_changes=none");
            } else {
                println!(
                    "restart_required_config_changes={}",
                    report.restart_required_config_changes.join(" | ")
                );
            }
            for warning in &report.warnings {
                println!("warning: {warning}");
            }
            print_db_holder_report("db_holders", &report.db_holders, "");
        }
    }
    Ok(())
}

fn print_db_holder_report(
    heading: &str,
    report: &mempal::process_diagnostics::DbHolderReport,
    indent: &str,
) {
    println!("{heading}:");
    println!("{indent}path: {}", report.db_path);
    println!("{indent}total: {}", report.holder_count);
    println!("{indent}extra_holders: {}", report.extra_holder_count);
    println!(
        "{indent}stale_mcp_servers: {}",
        report.stale_mcp_server_count
    );
    println!("{indent}orphan_daemons: {}", report.orphan_daemon_count);
    if let Some(error) = report.error.as_deref() {
        println!("{indent}error: {error}");
    }
    if report.holders.is_empty() {
        println!("{indent}holders: none");
        return;
    }
    println!("{indent}holders:");
    for holder in &report.holders {
        let age = holder
            .age_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let started = holder
            .started_at_unix_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let files = if holder.opened_files.is_empty() {
            "none".to_string()
        } else {
            holder.opened_files.join(",")
        };
        println!(
            "{indent}- pid={} role={} classification={} current_process={} current_daemon={} current_mcp_server={} age_secs={} started_at_unix_secs={} files={} command={}",
            holder.pid,
            holder.role,
            holder.classification,
            holder.current_process,
            holder.current_daemon,
            holder.current_mcp_server,
            age,
            started,
            files,
            holder.command
        );
    }
}

async fn brief_command(
    db: &Database,
    config: &Config,
    query: String,
    format: String,
) -> Result<()> {
    if !matches!(format.as_str(), "plain" | "json") {
        bail!("unsupported brief format: {format}");
    }
    let embedder = build_embedder(config).await?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let brief = assemble_brief(
        db,
        embedder.as_ref(),
        BriefRequest {
            query: query.clone(),
            domain: mempal::core::types::MemoryDomain::Project,
            field: "general".to_string(),
            cwd,
            max_items: 20,
            dao_tian_limit: 5,
        },
    )
    .await
    .context("failed to assemble cognitive brief")?;
    match format.as_str() {
        "json" => println!("{}", serde_json::to_string_pretty(&brief)?),
        _ => print_brief_plain(&brief),
    }
    Ok(())
}

fn print_brief_plain(brief: &mempal::brief::CognitiveBrief) {
    println!("## Summary\n{}", brief.summary.narrative);
    println!("\n## Key Facts");
    for fact in &brief.key_facts {
        println!(
            "- {}\n  drawer: {}\n  source: {}",
            fact.text, fact.citation.drawer_id, fact.citation.source_file
        );
    }
    println!("\n## Evidence");
    for ev in &brief.evidence {
        println!(
            "- {}\n  drawer: {}\n  source: {}",
            ev.text, ev.citation.drawer_id, ev.citation.source_file
        );
    }
    println!("\n## Uncertainty");
    for u in &brief.uncertainty {
        println!("- [{}] {}", u.kind, u.message);
    }
    println!("\n## Next Actions");
    for action in &brief.next_actions {
        println!("- {action}");
    }
}

fn phase3_adoption_wrap_command(db: &Database, opts: WrapCommandOpts) -> Result<()> {
    let WrapCommandOpts {
        surface,
        query,
        note,
        outcome: outcome_override,
        execute,
        allow_warnings,
        format,
        child_cmd,
    } = opts;

    // Validate --outcome and --surface BEFORE spawning the child.  An invalid
    // invocation must never run an arbitrary child command.
    const VALID_OUTCOMES: &[&str] = &[
        "accepted",
        "rejected",
        "used",
        "miss",
        "rollback",
        "contradiction",
        "neutral",
    ];
    if let Some(ref ov) = outcome_override {
        if !VALID_OUTCOMES.contains(&ov.as_str()) {
            bail!(
                "invalid --outcome value {ov:?}; valid values: {}",
                VALID_OUTCOMES.join(", ")
            );
        }
    }
    // Surface validation uses the same logic as capture_runtime_adoption_record_input.
    // Pass a known-valid placeholder outcome; only the surface path is checked here.
    capture_runtime_adoption_record_input(RuntimeAdoptionCaptureInput {
        id: None,
        surface: surface.clone(),
        outcome: outcome_override
            .as_deref()
            .unwrap_or("accepted")
            .to_string(),
        query: query.clone(),
        context_hash: None,
        card_id: None,
        evaluator_id: None,
        research_report_id: None,
        note: note.clone(),
        metadata: None,
    })
    .map_err(anyhow::Error::msg)?;

    let (program, args) = child_cmd
        .split_first()
        .expect("child_cmd non-empty by clap");
    let child_output = std::process::Command::new(program)
        .args(args)
        .stderr(std::process::Stdio::inherit())
        .output()
        .context("failed to run child command")?;
    let child_exit_code = child_output.status.code().unwrap_or(1);
    let child_stdout = String::from_utf8_lossy(&child_output.stdout).into_owned();
    let auto_outcome = if child_exit_code == 0 {
        "accepted".to_string()
    } else {
        "rejected".to_string()
    };
    // Both outcome_override (if Some) and surface are already validated above.
    let outcome = outcome_override.unwrap_or(auto_outcome);

    let record_input = capture_runtime_adoption_record_input(RuntimeAdoptionCaptureInput {
        id: None,
        surface: surface.clone(),
        outcome: outcome.clone(),
        query,
        context_hash: None,
        card_id: None,
        evaluator_id: None,
        research_report_id: None,
        note,
        metadata: None,
    })
    .map_err(anyhow::Error::msg)?;

    let mut capture =
        prepare_runtime_adoption_capture(surface, outcome.clone(), execute, record_input.clone());

    if execute {
        let track = parse_runtime_adoption_track(&record_input.track)?;
        let signal = parse_runtime_adoption_signal(&record_input.signal)?;
        let should_write = should_write_checked_record(&capture.record_quality, allow_warnings);
        let event = if should_write {
            let ev = runtime_adoption_event_from_input(record_input, track, signal);
            db.insert_runtime_adoption_event(&ev)
                .context("failed to insert wrap adoption event")?;
            Some(ev)
        } else {
            None
        };
        capture.writes = event.is_some();
        capture.record_checked = Some(RuntimeAdoptionCheckedRecordReport {
            writes: event.is_some(),
            blocked: event.is_none(),
            record_quality: capture.record_quality.clone(),
            event,
        });
    }

    let report = WrapReport {
        writes: capture.writes,
        execute,
        child_exit_code,
        child_stdout: child_stdout.clone(),
        outcome: outcome.clone(),
        capture,
    };
    match format.as_str() {
        "plain" => {
            println!(
                "outcome={} exit_code={} writes={}\n{child_stdout}",
                report.outcome, report.child_exit_code, report.writes
            );
        }
        _ => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }

    if child_exit_code != 0 {
        std::process::exit(child_exit_code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "model2vec")]
    use safetensors::tensor::TensorView;
    #[cfg(feature = "model2vec")]
    use safetensors::{Dtype, serialize_to_file};
    use std::collections::BTreeSet;
    #[cfg(feature = "model2vec")]
    use std::path::Path;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    const TEST_TARGET_FINGERPRINT: &str = "test-embedder:8:target";

    #[test]
    #[cfg(unix)]
    fn test_daemon_reaper_grace_covers_drain_budget() {
        // Finding 1 fix: verify that the reaper grace is >= the daemon's drain budget
        // plus a small cleanup buffer, ensuring we don't SIGKILL a healthy draining daemon.
        assert_eq!(
            DAEMON_TERM_GRACE,
            mempal::daemon::DAEMON_DRAIN_BUDGET.saturating_add(DAEMON_TERM_CLEANUP_BUFFER),
            "reaper grace must stay coupled to the daemon drain budget plus cleanup buffer"
        );
        assert!(
            DAEMON_TERM_GRACE >= mempal::daemon::DAEMON_DRAIN_BUDGET,
            "reaper grace ({:?}) must be >= daemon drain budget ({:?})",
            DAEMON_TERM_GRACE,
            mempal::daemon::DAEMON_DRAIN_BUDGET
        );
        assert!(
            DAEMON_TERM_GRACE >= mempal::daemon::DAEMON_DRAIN_BUDGET + DAEMON_TERM_CLEANUP_BUFFER,
            "reaper grace should include a safety buffer over the drain budget"
        );
    }

    #[derive(Default)]
    struct RecordingEmbedder {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingEmbedder {
        fn seen_texts(&self) -> Vec<String> {
            self.seen
                .lock()
                .expect("recording embedder mutex poisoned")
                .clone()
        }
    }

    #[async_trait::async_trait]
    impl Embedder for RecordingEmbedder {
        async fn embed(
            &self,
            texts: &[&str],
        ) -> std::result::Result<Vec<Vec<f32>>, mempal::embed::EmbedError> {
            self.seen
                .lock()
                .expect("recording embedder mutex poisoned")
                .extend(texts.iter().map(|text| (*text).to_string()));
            Ok(texts.iter().map(|text| test_vector(text)).collect())
        }

        fn dimensions(&self) -> usize {
            8
        }

        fn name(&self) -> &str {
            "recording-test"
        }
    }

    #[derive(Default)]
    struct FailFirstBatchEmbedder {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Embedder for FailFirstBatchEmbedder {
        async fn embed(
            &self,
            texts: &[&str],
        ) -> std::result::Result<Vec<Vec<f32>>, mempal::embed::EmbedError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                return Err(mempal::embed::EmbedError::Runtime(
                    "forced test batch failure".to_string(),
                ));
            }
            Ok(texts.iter().map(|text| test_vector(text)).collect())
        }

        fn dimensions(&self) -> usize {
            8
        }

        fn name(&self) -> &str {
            "fail-first-test"
        }
    }

    fn test_vector(text: &str) -> Vec<f32> {
        vec![text.len() as f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]
    }

    fn empty_reindex_target(limit: Option<usize>) -> ReindexVectorTarget {
        ReindexVectorTarget {
            drawer_ids: Vec::new(),
            project_id: None,
            wing: None,
            room: None,
            limit,
        }
    }

    #[cfg(feature = "model2vec")]
    fn model2vec_dry_run_config() -> Config {
        let mut config = Config::default();
        config.embed.backend = "model2vec".to_string();
        config
    }

    #[cfg(feature = "model2vec")]
    fn write_model2vec_fixture(model_dir: &Path, dimensions: usize) {
        std::fs::create_dir_all(model_dir).expect("create model2vec fixture dir");

        let tokenizer = serde_json::json!({
            "version": "1.0",
            "truncation": null,
            "padding": null,
            "added_tokens": [],
            "normalizer": null,
            "pre_tokenizer": null,
            "post_processor": null,
            "decoder": null,
            "model": {
                "type": "WordLevel",
                "vocab": {
                    "[UNK]": 0
                },
                "unk_token": "[UNK]"
            }
        });
        std::fs::write(
            model_dir.join("tokenizer.json"),
            serde_json::to_vec(&tokenizer).expect("serialize tokenizer"),
        )
        .expect("write tokenizer.json");

        let embedding_bytes = vec![1.0_f32; dimensions]
            .into_iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<u8>>();
        let tensor = TensorView::new(Dtype::F32, vec![1, dimensions], &embedding_bytes)
            .expect("create embeddings tensor");
        serialize_to_file(
            [(String::from("embeddings"), tensor)],
            &None,
            &model_dir.join("model.safetensors"),
        )
        .expect("write model.safetensors");

        std::fs::write(model_dir.join("config.json"), r#"{"normalize":true}"#)
            .expect("write config.json");
    }

    fn insert_reindex_test_drawer(
        db: &Database,
        id: &str,
        content: &str,
        source_file: &str,
        chunk_index: i64,
    ) {
        db.insert_drawer_with_project(
            &Drawer {
                id: id.to_string(),
                content: content.to_string(),
                wing: "test".to_string(),
                room: Some("reindex".to_string()),
                source_file: Some(source_file.to_string()),
                source_type: SourceType::AgentInference,
                added_at: format!("171300000{chunk_index}"),
                chunk_index: Some(chunk_index),
                ..Drawer::default()
            },
            Some("default"),
        )
        .expect("insert reindex test drawer");
    }

    /// Regression for issue #249: `mempal xurl timeline` aborted with
    /// "byte index 300 is not a char boundary" when a turn's content had a
    /// multi-byte UTF-8 char straddling byte 300 (common with CJK text).
    #[test]
    fn char_safe_preview_does_not_panic_on_multibyte_boundary() {
        // 298 ASCII bytes (indices 0..=297), then '前' (3 bytes) occupies
        // bytes 298, 299, 300 — so byte index 300 is mid-char and the old
        // `&s[..300]` byte slice would have panicked.
        let mut content = "a".repeat(298);
        content.push_str(&"前".repeat(50));
        assert!(
            !content.is_char_boundary(300),
            "test precondition: byte 300 must fall inside a multi-byte char"
        );
        assert!(content.chars().count() > 300, "must exceed the char limit");

        let preview = char_safe_preview(&content, 300);

        // 300 retained chars plus the single ellipsis char.
        assert_eq!(preview.chars().count(), 301);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn char_safe_preview_returns_input_unchanged_when_short() {
        let content = "短文本"; // 3 chars, well under the limit
        assert_eq!(char_safe_preview(content, 300), content);
    }

    #[tokio::test]
    async fn reindex_stale_batches_processes_snapshot_once_and_drains_pending() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        db.recreate_vectors_table(8).expect("create vector table");
        for i in 0i64..5 {
            insert_reindex_test_drawer(
                &db,
                &format!("d-{i}"),
                &format!("content {i}"),
                "fixtures/source.txt",
                i,
            );
        }

        let embedder = RecordingEmbedder::default();
        reindex_stale_batches(
            &db,
            &embedder,
            "recording-test",
            TEST_TARGET_FINGERPRINT,
            2,
            empty_reindex_target(None),
            BatchRetryPolicy {
                interval: std::time::Duration::from_millis(0),
                max_retries: 0,
            },
        )
        .await
        .expect("stale reindex batches");

        let seen = embedder.seen_texts();
        let unique_seen = seen.iter().cloned().collect::<BTreeSet<_>>();
        assert_eq!(seen.len(), 5, "all stale drawers are embedded once");
        assert_eq!(unique_seen.len(), 5, "no stale drawer is embedded twice");
        assert_eq!(
            reindex_pending_count(&db).expect("pending count"),
            0,
            "pending work list is drained"
        );
        assert_eq!(
            db.vector_row_count().expect("vector count"),
            5,
            "all stale drawers receive vectors"
        );
        for i in 0..5 {
            assert_eq!(
                load_reindex_metadata(&db, &format!("d-{i}"), "embedder_fingerprint")
                    .expect("load fingerprint")
                    .as_deref(),
                Some(TEST_TARGET_FINGERPRINT)
            );
        }
    }

    #[tokio::test]
    async fn reindex_stale_batches_skips_failed_batch_and_finishes_later_batches() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        db.recreate_vectors_table(8).expect("create vector table");
        for i in 0i64..4 {
            insert_reindex_test_drawer(
                &db,
                &format!("d-{i}"),
                &format!("content {i}"),
                "fixtures/source.txt",
                i,
            );
        }

        let embedder = FailFirstBatchEmbedder::default();
        reindex_stale_batches(
            &db,
            &embedder,
            "fail-first-test",
            TEST_TARGET_FINGERPRINT,
            2,
            empty_reindex_target(None),
            BatchRetryPolicy {
                interval: std::time::Duration::from_millis(0),
                max_retries: 0,
            },
        )
        .await
        .expect("stale reindex with skipped first batch");

        assert_eq!(
            embedder.calls.load(Ordering::SeqCst),
            2,
            "failed batch is not retried when max_retries is zero"
        );
        assert_eq!(
            reindex_pending_count(&db).expect("pending count"),
            0,
            "failed and successful batches are both removed from pending"
        );
        assert_eq!(
            reindex_skipped_count(&db).expect("skipped count"),
            2,
            "failed batch ids are reported through the skip table"
        );
        assert_eq!(
            reindex_skipped_ids(&db).expect("skipped ids"),
            vec!["d-0".to_string(), "d-1".to_string()]
        );
        assert_eq!(
            reindex_vector_ids(&db).expect("vector ids"),
            vec!["d-2".to_string(), "d-3".to_string()],
            "later batches still complete"
        );
    }

    #[test]
    fn reindex_stale_pending_snapshot_excludes_current_fingerprint() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        insert_reindex_test_drawer(&db, "fresh", "fresh content", "fixtures/source.txt", 0);
        insert_reindex_test_drawer(&db, "stale", "stale content", "fixtures/source.txt", 1);
        db.insert_vector_with_project("fresh", &[0.1_f32; 8], Some("default"))
            .expect("insert fresh vector");
        record_reindex_metadata(
            &db,
            "fresh",
            CURRENT_VECTOR_INDEX_VERSION,
            TEST_TARGET_FINGERPRINT,
        )
        .expect("record fresh metadata");

        let inserted = build_reindex_stale_pending_rows(
            &db,
            TEST_TARGET_FINGERPRINT,
            &empty_reindex_target(None),
        )
        .expect("build stale pending rows");

        assert_eq!(inserted, 1);
        assert_eq!(
            reindex_pending_ids(&db).expect("pending ids"),
            vec!["stale".to_string()],
            "drawer with current fingerprint is excluded"
        );
    }

    #[cfg(feature = "model2vec")]
    #[tokio::test]
    async fn reindex_embedder_dry_run_snapshots_stale_rows_without_writes() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        let model_dir = tmp.path().join("model2vec-fixture");
        write_model2vec_fixture(&model_dir, 1024);
        let mut config = model2vec_dry_run_config();
        config.embed.model = Some(model_dir.display().to_string());
        let embedder = build_specific_embedder(&config, "model2vec")
            .await
            .expect("build local model2vec embedder");
        let dim = embedder.dimensions();
        db.recreate_vectors_table(dim).expect("create vector table");

        insert_reindex_test_drawer(&db, "fresh", "fresh content", "fixtures/source.txt", 0);
        insert_reindex_test_drawer(&db, "stale", "stale content", "fixtures/source.txt", 1);

        let target_fingerprint = config.embed.vector_embedder_fingerprint("model2vec", dim);
        let fresh_vector = vec![0.1_f32; dim];
        let stale_vector = vec![0.2_f32; dim];
        db.insert_vector_with_project("fresh", &fresh_vector, Some("default"))
            .expect("insert fresh vector");
        db.insert_vector_with_project("stale", &stale_vector, Some("default"))
            .expect("insert stale vector");
        record_reindex_metadata(
            &db,
            "fresh",
            CURRENT_VECTOR_INDEX_VERSION,
            &target_fingerprint,
        )
        .expect("record fresh fingerprint");

        let before_vectors = db.vector_row_count().expect("vector count before");
        let inserted = preview_reindex_stale_vector_work(
            &db,
            &config,
            "model2vec",
            empty_reindex_target(None),
        )
        .await
        .expect("dry-run stale snapshot");

        assert_eq!(inserted, 1);
        assert_eq!(
            reindex_pending_ids(&db).expect("pending ids"),
            vec!["stale".to_string()]
        );
        assert_eq!(
            db.vector_row_count().expect("vector count after"),
            before_vectors,
            "dry-run must not mutate drawer_vectors"
        );
    }

    #[cfg(feature = "model2vec")]
    #[tokio::test]
    async fn reindex_embedder_dry_run_without_stale_is_rejected() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        let config = model2vec_dry_run_config();

        let error = reindex_command_by_embedder(
            &db,
            &config,
            "model2vec",
            ReindexEmbedderMode {
                resume: false,
                stale_only: false,
                dry_run: true,
            },
            StaleBatchTuning {
                batch_size: 2,
                max_batch_retries: 0,
            },
            empty_reindex_target(None),
        )
        .await
        .expect_err("dry-run without stale must fail");

        let message = format!("{error:#}");
        assert!(
            message.contains("--stale"),
            "error should tell the user to add --stale, got: {message}"
        );
    }

    #[tokio::test]
    async fn reindex_stale_batches_limit_caps_snapshot() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        db.recreate_vectors_table(8).expect("create vector table");
        for i in 0i64..5 {
            insert_reindex_test_drawer(
                &db,
                &format!("d-{i}"),
                &format!("content {i}"),
                "fixtures/source.txt",
                i,
            );
        }

        let embedder = RecordingEmbedder::default();
        reindex_stale_batches(
            &db,
            &embedder,
            "recording-test",
            TEST_TARGET_FINGERPRINT,
            2,
            empty_reindex_target(Some(3)),
            BatchRetryPolicy {
                interval: std::time::Duration::from_millis(0),
                max_retries: 0,
            },
        )
        .await
        .expect("limited stale reindex");

        assert_eq!(
            embedder.seen_texts().len(),
            3,
            "--limit caps the snapshot size"
        );
        assert_eq!(
            db.vector_row_count().expect("vector count"),
            3,
            "only the limited snapshot is re-embedded"
        );
        assert_eq!(
            reindex_pending_count(&db).expect("pending count"),
            0,
            "limited pending work list is drained"
        );
    }

    /// Issue #304: failed-batch drawer IDs were expanded into
    /// `NOT IN (?, ?, ...)` placeholders, so a `reindex --stale` run that
    /// skipped more than SQLite's `SQLITE_MAX_VARIABLE_NUMBER` (32766) drawers'
    /// worth of IDs blew the bind-variable limit and crashed the graceful
    /// skip-continue with "too many SQL variables". The TEMP-table exclusion
    /// uses a subquery (zero bind variables for the skip set), so an arbitrarily
    /// large skip set must now query cleanly. This drives the with-vectors
    /// (`d.` alias) branch of the stale selection.
    #[test]
    fn reindex_stale_skip_exclusion_survives_more_than_sqlite_var_limit() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");

        // A live drawer that already has a vector but no reindex metadata, so it
        // is stale for any target fingerprint and must be re-selected after the
        // skipped IDs are excluded.
        insert_reindex_test_drawer(&db, "live-1", "keep me", "fixtures/source.txt", 0);
        db.insert_vector_with_project("live-1", &[0.1_f32; 8], Some("default"))
            .expect("insert live vector");

        // Stage comfortably more skipped IDs than SQLite's default
        // SQLITE_MAX_VARIABLE_NUMBER (32766). The old `NOT IN (?, ?, ...)`
        // expansion emitted one bind variable per skipped ID, so this many IDs
        // overflowed the limit and aborted the query with "too many SQL
        // variables"; the TEMP-table subquery binds nothing for the skip set.
        let over_limit: usize = 32_766 + 8_000;
        let skipped: Vec<String> = (0..over_limit).map(|i| format!("skip-{i}")).collect();
        stage_reindex_skipped_ids(&db, &skipped).expect("stage skipped ids");

        // Must NOT error with "too many SQL variables".
        let inserted = build_reindex_stale_pending_rows(
            &db,
            "target-fingerprint",
            &empty_reindex_target(None),
        )
        .expect("stale snapshot must not hit the SQLite variable limit");
        let rows = pull_reindex_pending_batch(&db, 512).expect("pull stale pending batch");

        assert_eq!(inserted, 1, "only the live stale drawer is snapshotted");
        assert!(
            rows.iter().any(|row| row.id == "live-1"),
            "the live stale drawer must still be selected"
        );
        assert!(
            rows.iter().all(|row| !row.id.starts_with("skip-")),
            "skipped drawers must remain excluded for the rest of the run"
        );
    }

    #[test]
    fn reindex_pending_delete_chunks_more_than_999_ids() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        for i in 0i64..1_500 {
            insert_reindex_test_drawer(
                &db,
                &format!("d-{i:04}"),
                &format!("content {i}"),
                "fixtures/source.txt",
                i,
            );
        }

        let inserted = build_reindex_stale_pending_rows(
            &db,
            "target-fingerprint",
            &empty_reindex_target(None),
        )
        .expect("build stale pending rows");
        let ids = reindex_pending_ids(&db).expect("pending ids");
        let deleted = delete_reindex_pending_ids(&db, &ids)
            .expect("chunked delete must not hit SQLite variable limit");

        assert_eq!(inserted, 1_500);
        assert_eq!(ids.len(), 1_500);
        assert_eq!(deleted, 1_500);
        assert_eq!(
            reindex_pending_count(&db).expect("pending count"),
            0,
            "chunked delete removes every pending row"
        );
    }

    #[test]
    fn reindex_pending_pull_preserves_snapshot_order() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        insert_reindex_test_drawer(&db, "b-0", "b content", "fixtures/b.txt", 0);
        insert_reindex_test_drawer(&db, "a-2", "a2 content", "fixtures/a.txt", 2);
        insert_reindex_test_drawer(&db, "a-1b", "a1b content", "fixtures/a.txt", 1);
        insert_reindex_test_drawer(&db, "a-1a", "a1a content", "fixtures/a.txt", 1);

        let inserted = build_reindex_stale_pending_rows(
            &db,
            "target-fingerprint",
            &empty_reindex_target(None),
        )
        .expect("build stale pending rows");
        let rows = pull_reindex_pending_batch(&db, 10).expect("pull pending rows");
        let ids = rows.into_iter().map(|row| row.id).collect::<Vec<_>>();

        assert_eq!(inserted, 4);
        assert_eq!(
            ids,
            vec![
                "a-1a".to_string(),
                "a-1b".to_string(),
                "a-2".to_string(),
                "b-0".to_string()
            ],
            "pull order follows the source_path, chunk_index, id snapshot order"
        );
    }

    /// A clean `reindex --stale` (zero skipped drawers) must select every stale
    /// drawer — the #304 TEMP-table exclusion must not change clean-run output.
    /// This drives the no-vectors branch of the stale selection.
    #[test]
    fn reindex_stale_pending_snapshot_returns_all_when_nothing_skipped() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        for i in 0i64..3 {
            insert_reindex_test_drawer(
                &db,
                &format!("d-{i}"),
                &format!("content {i}"),
                "fixtures/source.txt",
                i,
            );
        }

        let inserted = build_reindex_stale_pending_rows(
            &db,
            "target-fingerprint",
            &empty_reindex_target(None),
        )
        .expect("clean stale snapshot");
        let rows = pull_reindex_pending_batch(&db, 512).expect("pull clean stale pending rows");

        assert_eq!(inserted, 3);
        assert_eq!(
            rows.len(),
            3,
            "all stale drawers must be selected when nothing is skipped"
        );
    }

    /// Issue #341 regression guard. The stale-detection snapshot must check
    /// vector presence with a correlated `EXISTS` point-lookup that the `vec0`
    /// virtual table reliably serves — never a set-based `LEFT JOIN
    /// drawer_vectors`, which the planner can resolve to NULL for every row at
    /// production scale, mis-flagging every live drawer as stale and triggering
    /// a full-DB re-embed. Mirrors the #340 finalize-sweep fix. A behavioural
    /// test cannot catch the regression because the set-join still works at the
    /// small scale of unit tests, so this asserts the SQL form directly.
    #[test]
    fn reindex_stale_pending_vectored_sql_uses_correlated_exists_point_lookup() {
        let sql = REINDEX_STALE_PENDING_VECTORED_SQL;
        assert!(
            sql.contains("NOT EXISTS (SELECT 1 FROM drawer_vectors dv WHERE dv.id = d.id)"),
            "stale-detection must check vector presence with a correlated EXISTS point-lookup"
        );
        assert!(
            !sql.contains("LEFT JOIN drawer_vectors"),
            "stale-detection must NOT set-join the drawer_vectors vec0 virtual table"
        );
    }
}

fn print_ingest_wait_receipt_response(json: bool, response: &IngestResponse) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(response)
                .context("failed to serialize ingest wait receipt JSON")?
        );
        return Ok(());
    }
    print_operation_response(response)
}

#[derive(Debug)]
struct IngestWaitTimedOut {
    operation_id: String,
}

impl IngestWaitTimedOut {
    fn new(operation_id: Option<&str>) -> Self {
        Self {
            operation_id: operation_id.unwrap_or_default().to_string(),
        }
    }
}

impl std::fmt::Display for IngestWaitTimedOut {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.operation_id.is_empty() {
            formatter.write_str("timed out waiting for ingest operation")
        } else {
            write!(
                formatter,
                "timed out waiting for ingest operation {}",
                self.operation_id
            )
        }
    }
}

impl std::error::Error for IngestWaitTimedOut {}

fn is_ingest_wait_timed_out(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|source| source.downcast_ref::<IngestWaitTimedOut>().is_some())
}

#[cfg(test)]
mod ingest_wait_timeout_error_tests {
    use super::*;

    #[test]
    fn detects_context_wrapped_ingest_wait_timeout() {
        let wrapped = Err::<(), IngestWaitTimedOut>(IngestWaitTimedOut::new(Some("op-1")))
            .context("wait failed")
            .expect_err("context-wrapped timeout should remain an error");

        assert!(is_ingest_wait_timed_out(&wrapped));
    }

    #[test]
    fn ignores_other_errors_for_ingest_wait_timeout() {
        let error = anyhow::anyhow!("other error");

        assert!(!is_ingest_wait_timed_out(&error));
    }
}

#[cfg(test)]
mod historical_rejudge_tests {
    use super::*;

    fn test_config() -> Config {
        Config::parse("[gating]\nenabled = true\n").expect("parse config")
    }

    fn insert_drawer(db: &Database, id: &str, content: &str, wing: &str, room: Option<&str>) {
        db.insert_drawer(&Drawer {
            id: id.to_string(),
            content: content.to_string(),
            wing: wing.to_string(),
            room: room.map(ToOwned::to_owned),
            source_file: Some(format!("{id}.json")),
            source_type: SourceType::AgentInference,
            added_at: "2026-01-01T00:00:00Z".to_string(),
            ..Drawer::default()
        })
        .expect("insert drawer");
    }

    fn insert_custom_drawer(db: &Database, drawer: Drawer) {
        db.insert_drawer(&drawer).expect("insert drawer");
    }

    fn insert_high_importance_drawer(db: &Database, id: &str) {
        insert_custom_drawer(
            db,
            Drawer {
                id: id.to_string(),
                content: "ok".to_string(),
                wing: "notes".to_string(),
                source_file: Some(format!("{id}.json")),
                source_type: SourceType::AgentInference,
                added_at: "2026-01-01T00:00:00Z".to_string(),
                importance: 5,
                ..Drawer::default()
            },
        );
    }

    fn assert_protected_importance_audit(db: &Database, drawer_id: &str, mutation: &str) {
        let (decision, reason, explain_json): (String, String, String) = db
            .conn()
            .query_row(
                "SELECT decision, reason, explain_json FROM gating_audit WHERE drawer_id = ?1",
                [drawer_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("load protected importance audit");
        let explain =
            serde_json::from_str::<serde_json::Value>(&explain_json).expect("parse explain json");

        assert_eq!(decision, "keep");
        assert_eq!(reason, "explicit_importance_5");
        assert_eq!(
            explain.get("mutation").and_then(serde_json::Value::as_str),
            Some(mutation)
        );
    }

    fn audit_count(db: &Database) -> i64 {
        db.conn()
            .query_row("SELECT COUNT(*) FROM gating_audit", [], |row| row.get(0))
            .expect("count gating audit")
    }

    #[test]
    fn deterministic_historical_decision_preserves_explicit_high_importance() {
        let drawer = Drawer {
            id: "important".to_string(),
            content: "ok".to_string(),
            wing: "notes".to_string(),
            importance: 5,
            ..Drawer::default()
        };

        let decision = deterministic_historical_decision(&drawer);

        assert!(!decision.delete_candidate);
        assert!(decision.protected);
        assert_eq!(decision.reason, "explicit_importance_5");
    }

    #[tokio::test]
    async fn historical_rejudge_dry_run_reports_without_deleting() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        insert_drawer(&db, "low", "ok", "notes", None);

        maintenance_rejudge_command(
            &db,
            &test_config(),
            HistoricalRejudgeOptions {
                execute: false,
                hard_delete: false,
                limit: 100,
                wing: None,
                room: None,
                project: None,
                format: "json",
            },
        )
        .await
        .expect("dry-run rejudge");

        assert!(db.get_drawer("low").expect("load drawer").is_some());
        assert_eq!(db.deleted_drawer_count().expect("deleted count"), 0);
        assert_eq!(audit_count(&db), 1, "dry-run still records auditability");
    }

    #[tokio::test]
    async fn historical_rejudge_dry_run_preserves_explicit_high_importance() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        insert_high_importance_drawer(&db, "important");

        maintenance_rejudge_command(
            &db,
            &test_config(),
            HistoricalRejudgeOptions {
                execute: false,
                hard_delete: false,
                limit: 100,
                wing: None,
                room: None,
                project: None,
                format: "json",
            },
        )
        .await
        .expect("dry-run rejudge");

        assert!(db.get_drawer("important").expect("load drawer").is_some());
        assert_eq!(db.deleted_drawer_count().expect("deleted count"), 0);
        assert_protected_importance_audit(&db, "important", "dry_run");
    }

    #[tokio::test]
    async fn historical_rejudge_execute_soft_deletes_candidates() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        insert_drawer(&db, "low", "ok", "notes", None);

        maintenance_rejudge_command(
            &db,
            &test_config(),
            HistoricalRejudgeOptions {
                execute: true,
                hard_delete: false,
                limit: 100,
                wing: None,
                room: None,
                project: None,
                format: "plain",
            },
        )
        .await
        .expect("execute rejudge");

        assert!(db.get_drawer("low").expect("load drawer").is_none());
        assert_eq!(db.deleted_drawer_count().expect("deleted count"), 1);
        let audit_log =
            std::fs::read_to_string(tmp.path().join("audit.jsonl")).expect("read audit log");
        assert!(audit_log.contains("\"command\":\"maintenance-rejudge\""));
        assert!(audit_log.contains("\"drawer_ids\":[\"low\"]"));
    }

    #[tokio::test]
    async fn historical_rejudge_execute_preserves_explicit_high_importance() {
        for (hard_delete, mutation) in [(false, "soft_delete"), (true, "hard_delete")] {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
            insert_drawer(&db, "low", "ok", "notes", None);
            insert_high_importance_drawer(&db, "important");

            maintenance_rejudge_command(
                &db,
                &test_config(),
                HistoricalRejudgeOptions {
                    execute: true,
                    hard_delete,
                    limit: 100,
                    wing: None,
                    room: None,
                    project: None,
                    format: "plain",
                },
            )
            .await
            .expect("execute rejudge");

            assert!(db.get_drawer("low").expect("load low drawer").is_none());
            assert!(
                db.get_drawer("important")
                    .expect("load important drawer")
                    .is_some()
            );
            assert_protected_importance_audit(&db, "important", mutation);
        }
    }

    #[tokio::test]
    async fn historical_rejudge_preserves_pinned_canonical_and_high_signal() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        insert_custom_drawer(
            &db,
            Drawer {
                id: "pinned".to_string(),
                content: "ok".to_string(),
                wing: "notes".to_string(),
                is_pinned: true,
                added_at: "2026-01-01T00:00:00Z".to_string(),
                ..Drawer::default()
            },
        );
        insert_custom_drawer(
            &db,
            Drawer {
                id: "canonical".to_string(),
                content: "ok".to_string(),
                wing: "notes".to_string(),
                status: Some(KnowledgeStatus::Canonical),
                added_at: "2026-01-01T00:00:00Z".to_string(),
                ..Drawer::default()
            },
        );
        insert_drawer(
            &db,
            "high-signal",
            "We decided the historical rejudge must preserve architecture decisions and bug root cause findings.",
            "notes",
            None,
        );

        maintenance_rejudge_command(
            &db,
            &test_config(),
            HistoricalRejudgeOptions {
                execute: true,
                hard_delete: false,
                limit: 100,
                wing: None,
                room: None,
                project: None,
                format: "json",
            },
        )
        .await
        .expect("execute rejudge");

        for id in ["pinned", "canonical", "high-signal"] {
            assert!(db.get_drawer(id).expect("load drawer").is_some(), "{id}");
        }
        assert_eq!(db.deleted_drawer_count().expect("deleted count"), 0);
    }

    #[tokio::test]
    async fn historical_rejudge_cleans_low_value_hook_events() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        insert_drawer(
            &db,
            "read-hook",
            r#"{"tool_name":"Read","input":"README.md","output":"file contents","exit_code":0}"#,
            "hooks-raw",
            Some("Read"),
        );
        insert_drawer(
            &db,
            "decision-prompt",
            "We decided to keep salience gating for hooks because it preserves long-term memory quality.",
            "hooks-raw",
            Some("user-prompt"),
        );

        maintenance_rejudge_command(
            &db,
            &test_config(),
            HistoricalRejudgeOptions {
                execute: true,
                hard_delete: false,
                limit: 100,
                wing: Some("hooks-raw"),
                room: None,
                project: None,
                format: "plain",
            },
        )
        .await
        .expect("execute hook rejudge");

        assert!(
            db.get_drawer("read-hook")
                .expect("load read hook")
                .is_none()
        );
        assert!(
            db.get_drawer("decision-prompt")
                .expect("load decision prompt")
                .is_some()
        );
        let reason: String = db
            .conn()
            .query_row(
                "SELECT reason FROM gating_audit WHERE drawer_id = 'read-hook'",
                [],
                |row| row.get(0),
            )
            .expect("load read-hook audit reason");
        assert_eq!(reason, "read_tool");
    }
}
