use std::collections::BTreeSet;
use std::env;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(feature = "rest")]
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use mempal::aaak::{AaakCodec, AaakMeta};
#[cfg(feature = "rest")]
use mempal::api::{ApiState, DEFAULT_REST_ADDR, serve as serve_rest_api};
use mempal::context::{ContextPack, ContextRequest, assemble_context};
use mempal::core::{
    config::{Config, ConfigHandle, default_config_path},
    db::Database,
    priming::PrimingRequest,
    project::{
        ProjectMigrationEvent, ProjectSearchScope, escape_project_id_for_display,
        migrate_null_project_ids, resolve_project_id,
    },
    protocol::{DEFAULT_IDENTITY_HINT, MEMORY_PROTOCOL},
    reindex::ReindexProgressStore,
    types::{
        AnchorKind, KnowledgeCard, KnowledgeCardEvent, KnowledgeCardFilter, KnowledgeEventType,
        KnowledgeEvidenceLink, KnowledgeEvidenceRole, KnowledgeStatus, KnowledgeTier, MemoryDomain,
        MemoryKind, TaxonomyEntry, TriggerHints, TunnelEndpoint,
    },
    utils::{
        build_triple_id, current_timestamp, format_tunnel_endpoint,
        normalize_added_at as normalize_added_at_value, normalize_rfc3339_timestamp,
    },
};
use mempal::embed::build_backend_from_name;
use mempal::embed::{ConfiguredEmbedderFactory, Embedder, global_embed_status};
use mempal::field_taxonomy::{FieldTaxonomyEntry, field_taxonomy};
use mempal::ingest::gating::compile_classifier_from_config;
use mempal::ingest::{
    IngestOptions, IngestStats, ingest_dir_with_options, ingest_file_with_options,
    reindex::{ReindexMode, ReindexOptions, ReindexReport, reindex_sources},
};
use mempal::knowledge_anchor::{PublishAnchorRequest, publish_anchor};
use mempal::knowledge_distill::{DistillPlan, DistillRequest, commit_distill, prepare_distill};
use mempal::knowledge_gate::{
    GateReport, PromotionPolicyEntry, evaluate_gate_by_id, promotion_policy,
};
use mempal::knowledge_lifecycle::{
    DemoteRequest, PromoteRequest, demote_knowledge, promote_knowledge,
};
use mempal::mcp::MempalMcpServer;
use mempal::observability;
use mempal::search::{SearchFilters, SearchOptions, search_with_all_options};
use serde::Serialize;
use sha2::{Digest, Sha256};

mod longmemeval;
#[path = "cli/prime.rs"]
mod prime_cli;

use crate::longmemeval::{BenchMode, LongMemEvalArgs, LongMemEvalGranularity, default_top_k};
use crate::prime_cli::{PrimeArgs, PrimeFormat};

#[derive(Parser)]
#[command(name = "mempal", about = "Project memory for coding agents")]
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
}

struct IngestCommandOptions<'a> {
    dir: &'a Path,
    wing: &'a str,
    room: Option<&'a str>,
    format: Option<String>,
    project: Option<&'a str>,
    no_gate: bool,
    dry_run: bool,
    json: bool,
    no_strip_noise: bool,
    diary_rollup: bool,
}

struct RollbackCommandOptions<'a> {
    since: &'a str,
    wing: Option<&'a str>,
    room: Option<&'a str>,
    project: Option<&'a str>,
    dry_run: bool,
    json: bool,
}

struct ContextCommandArgs {
    query: String,
    field: String,
    domain: String,
    cwd: Option<PathBuf>,
    format: String,
    include_evidence: bool,
    max_items: usize,
    dao_tian_limit: usize,
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
    Init {
        dir: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    Ingest {
        dir: PathBuf,
        #[arg(long)]
        wing: String,
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
        #[arg(long)]
        no_strip_noise: bool,
        #[arg(long)]
        diary_rollup: bool,
    },
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
    },
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
        #[arg(long, default_value_t = 12)]
        max_items: usize,
        #[arg(long = "dao-tian-limit", default_value_t = 1)]
        dao_tian_limit: usize,
    },
    Project {
        #[command(subcommand)]
        command: ProjectCommands,
    },
    Gating {
        #[command(subcommand)]
        command: GatingCommands,
    },
    WakeUp {
        #[arg(long, value_enum)]
        format: Option<WakeUpFormat>,
    },
    Prime(PrimeArgs),
    Compress {
        text: String,
    },
    Bench {
        #[command(subcommand)]
        command: BenchCommands,
    },
    Delete {
        drawer_id: String,
    },
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
    Purge {
        /// Only purge drawers soft-deleted before this ISO timestamp
        #[arg(long)]
        before: Option<String>,
    },
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
    Kg {
        #[command(subcommand)]
        command: KgCommands,
    },
    Knowledge {
        #[command(subcommand)]
        command: KnowledgeCommands,
    },
    KnowledgeCard {
        #[command(subcommand)]
        command: KnowledgeCardCommands,
    },
    Tunnels {
        #[command(subcommand)]
        command: Option<TunnelCommands>,
    },
    Taxonomy {
        #[command(subcommand)]
        command: TaxonomyCommands,
    },
    FieldTaxonomy {
        #[arg(long, default_value = "plain")]
        format: String,
    },
    Serve {
        #[arg(long)]
        mcp: bool,
    },
    Status,
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
    Stats {
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    View {
        drawer_id: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    Audit {
        #[arg(long)]
        kind: Option<String>,
        #[arg(
            long,
            help = "Filter to audit records created after this point. \
                    Duration: '10s', '15m', '2h', '3d'. \
                    ISO 8601: '2026-04-25T20:00:00Z' or '2026-04-25T20:00:00+08:00'."
        )]
        since: Option<String>,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
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
    Integrations {
        #[command(subcommand)]
        command: mempal::integrations::IntegrationCommands,
    },
    Hook {
        #[command(subcommand)]
        command: mempal::hook::HookCommands,
    },
    Hotpatch {
        #[command(subcommand)]
        command: mempal::hotpatch::HotpatchCommands,
    },
    Daemon {
        #[arg(long, default_value_t = false)]
        foreground: bool,
    },
}

#[derive(Subcommand)]
enum TaxonomyCommands {
    List,
    Edit {
        wing: String,
        room: String,
        #[arg(long)]
        keywords: String,
    },
}

#[derive(Subcommand)]
enum GatingCommands {
    Stats {
        #[arg(long)]
        since: Option<String>,
    },
}

#[derive(Subcommand)]
enum KgCommands {
    Add {
        subject: String,
        predicate: String,
        object: String,
        #[arg(long)]
        source_drawer: Option<String>,
    },
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
    Invalidate {
        triple_id: String,
    },
    Timeline {
        entity: String,
    },
    Stats,
    List,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum KnowledgeCommands {
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
    Policy {
        #[arg(long, default_value = "plain")]
        format: String,
    },
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
    Get {
        card_id: String,
        #[arg(long, default_value = "plain")]
        format: String,
    },
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
    Events {
        card_id: String,
        #[arg(long, default_value = "plain")]
        format: String,
    },
}

#[derive(Subcommand)]
enum TunnelCommands {
    Add {
        #[arg(long)]
        left: String,
        #[arg(long)]
        right: String,
        #[arg(long)]
        label: String,
    },
    List {
        #[arg(long)]
        wing: Option<String>,
        #[arg(long, default_value = "all")]
        kind: String,
    },
    Delete {
        tunnel_id: String,
    },
    Follow {
        #[arg(long)]
        from: String,
        #[arg(long, default_value_t = 1)]
        hops: u8,
    },
}

#[derive(Subcommand)]
enum ProjectCommands {
    Migrate {
        #[arg(long)]
        project: String,
        #[arg(long)]
        wing: Option<String>,
    },
}

#[derive(Subcommand)]
enum BenchCommands {
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
        Commands::Daemon { foreground } => {
            return mempal::daemon::run_command(default_config_path(), *foreground);
        }
        Commands::Prime(args) => {
            return prime_command(&config_path, args.clone());
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
            wing,
            room,
            format,
            project,
            no_gate,
            dry_run,
            json,
            no_strip_noise,
            diary_rollup,
        } => block_on_result(ingest_command(
            &db,
            config.as_ref(),
            IngestCommandOptions {
                dir: &dir,
                wing: &wing,
                room: room.as_deref(),
                format,
                project: project.as_deref(),
                no_gate,
                dry_run,
                json,
                no_strip_noise,
                diary_rollup,
            },
        )),
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
            },
        )),
        Commands::Context {
            query,
            field,
            domain,
            cwd,
            format,
            include_evidence,
            max_items,
            dao_tian_limit,
        } => block_on_result(context_command(
            &db,
            config.as_ref(),
            ContextCommandArgs {
                query,
                field,
                domain,
                cwd,
                format,
                include_evidence,
                max_items,
                dao_tian_limit,
            },
        )),
        Commands::Project { command } => project_command(&db, command),
        Commands::Delete { drawer_id } => delete_command(&db, &drawer_id),
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
            recompute_importance,
            only_zero,
            normalize_added_at,
        } => {
            if normalize_added_at {
                if recompute_importance || embedder.is_some() || from_config {
                    bail!(
                        "--normalize-added-at is mutually exclusive with --embedder, --from-config, and --recompute-importance"
                    );
                }
                normalize_added_at_command(&db)
            } else if recompute_importance {
                recompute_importance_command(&db, only_zero)
            } else if embedder.is_some() || from_config {
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
                    resume,
                    stale,
                ))
            } else {
                block_on_result(reindex_command_sources(
                    &db,
                    config.as_ref(),
                    stale,
                    force,
                    dry_run,
                ))
            }
        }
        Commands::Kg { command } => kg_command(&db, command),
        Commands::Knowledge { command } => {
            block_on_result(knowledge_command(&db, config.as_ref(), command))
        }
        Commands::KnowledgeCard { command } => knowledge_card_command(&db, command),
        Commands::Tunnels { command } => tunnels_command(&db, command),
        Commands::Taxonomy { command } => taxonomy_command(&db, command),
        Commands::FieldTaxonomy { format } => field_taxonomy_command(&format),
        Commands::Serve { mcp } => block_on_result(serve_command(config.as_ref(), mcp)),
        Commands::Status => status_command(&db, config.as_ref()),
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
        Commands::View { drawer_id, raw } => observability::view_command(
            &db,
            config.as_ref(),
            observability::ViewOptions {
                drawer_id: &drawer_id,
                raw,
            },
        ),
        Commands::Audit { kind, since, raw } => observability::audit_command(
            &db,
            config.as_ref(),
            observability::AuditOptions {
                kind: kind.as_deref(),
                since: since.as_deref(),
                raw,
            },
        ),
        Commands::FactCheck {
            path,
            wing,
            room,
            now,
        } => fact_check_command(&db, path.as_deref(), wing.as_deref(), room.as_deref(), now),
        Commands::CoworkDrain { .. }
        | Commands::CoworkStatus { .. }
        | Commands::CoworkInstallHooks { .. }
        | Commands::Integrations { .. }
        | Commands::Hook { .. }
        | Commands::Hotpatch { .. }
        | Commands::Daemon { .. } => unreachable!(),
    }
}

fn is_dashboard_command(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Tail { .. }
            | Commands::Timeline { .. }
            | Commands::Stats { .. }
            | Commands::View { .. }
            | Commands::Audit { .. }
    )
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
    tokio::runtime::Runtime::new()
        .context("failed to construct tokio runtime")?
        .block_on(future)
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
    let path = options.dir;
    if !path.exists() {
        bail!("path `{}` does not exist", path.display());
    }
    if path.is_file() && !options.dry_run {
        bail!(
            "`mempal ingest` expects a DIRECTORY, got file `{}`. To ingest a single file, create a temporary directory first, e.g. `mkdir -p /path/to/dir && cp {} /path/to/dir/ && mempal ingest /path/to/dir --wing {}`",
            path.display(),
            path.display(),
            options.wing
        );
    }
    if let Some(format) = options.format.as_deref()
        && format != "convos"
    {
        bail!("unsupported --format value: {format}");
    }

    let project_id = resolve_project_id(options.project, config, Some(options.dir))
        .context("failed to resolve ingest project id")?;
    let base_options = IngestOptions {
        room: options.room,
        source_root: if path.is_file() {
            path.parent()
        } else {
            Some(path)
        },
        dry_run: options.dry_run,
        project_id: project_id.as_deref(),
        gating: None,
        prototype_classifier: None,
        source_file_override: None,
        replace_existing_source: false,
        no_strip_noise: options.no_strip_noise,
        diary_rollup: options.diary_rollup,
        diary_rollup_day: None,
    };

    let stats = if options.dry_run {
        ingest_path_with_options(db, &NoopEmbedder, path, options.wing, base_options).await?
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
            source_root: if path.is_file() {
                path.parent()
            } else {
                Some(path)
            },
            dry_run: false,
            project_id: project_id.as_deref(),
            gating: (!options.no_gate).then_some(&config.ingest_gating),
            prototype_classifier: prototype_classifier.as_ref(),
            source_file_override: None,
            replace_existing_source: false,
            no_strip_noise: options.no_strip_noise,
            diary_rollup: options.diary_rollup,
            diary_rollup_day: None,
        };
        ingest_path_with_options(db, &*embedder, path, options.wing, live_options).await?
    };

    append_ingest_audit_log(
        db,
        options.dir,
        options.wing,
        options.format.as_deref(),
        options.dry_run,
        &stats,
    )
    .context("failed to append ingest audit log")?;

    if options.json {
        let output = IngestJsonOutput {
            dry_run: options.dry_run,
            files: stats.files,
            chunks: stats.chunks,
            skipped: stats.skipped,
            dropped_by_gate: stats.dropped_by_gate,
            drawer_ids: &stats.drawer_ids,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&output)
                .context("failed to serialize ingest JSON output")?
        );
        return Ok(());
    }

    println!(
        "dry_run={} files={} chunks={} skipped={} dropped_by_gate={} noise_bytes_stripped={} lock_wait_ms={}",
        options.dry_run,
        stats.files,
        stats.chunks,
        stats.skipped,
        stats.dropped_by_gate,
        stats.noise_bytes_stripped.unwrap_or(0),
        stats.lock_wait_ms.unwrap_or(0)
    );
    Ok(())
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
    let entry = serde_json::json!({ "timestamp": current_timestamp(), "command": "ingest", "wing": wing, "dir": dir.to_string_lossy(), "format": format, "dry_run": dry_run, "files": stats.files, "chunks": stats.chunks, "skipped": stats.skipped, "dropped_by_gate": stats.dropped_by_gate });
    writeln!(file, "{entry}")
        .with_context(|| format!("failed to write audit log {}", audit_path.display()))?;
    Ok(())
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
            max_items: args.max_items,
            dao_tian_limit: args.dao_tian_limit,
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
        "agent" => Ok(MemoryDomain::Agent),
        "skill" => Ok(MemoryDomain::Skill),
        "global" => Ok(MemoryDomain::Global),
        other => bail!("unsupported domain: {other}"),
    }
}

fn print_context_plain(pack: &ContextPack) {
    if pack.sections.is_empty() {
        println!("no context");
        return;
    }
    for section in &pack.sections {
        println!("## {}", section.name);
        for item in &section.items {
            println!("- {}", item.text);
            println!("  source: {}", item.source_file);
            println!("  drawer: {}", item.drawer_id);
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
        }
        println!();
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
    }
}
fn domain_slug(v: &MemoryDomain) -> &'static str {
    match v {
        MemoryDomain::Project => "project",
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
        KnowledgeStatus::Candidate => "candidate",
        KnowledgeStatus::Promoted => "promoted",
        KnowledgeStatus::Canonical => "canonical",
        KnowledgeStatus::Demoted => "demoted",
        KnowledgeStatus::Retired => "retired",
    }
}
fn parse_knowledge_status(v: &str) -> Result<KnowledgeStatus> {
    match v {
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
        MemoryKind::Evidence => drawer.content.as_str(),
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
    resume: bool,
    stale_only: bool,
) -> Result<()> {
    let embedder = build_specific_embedder(config, embedder_name).await?;
    let new_dim = embedder.dimensions();
    let current_dim = current_vector_dim(db).context("failed to read embedding dim")?;
    let progress_store = ReindexProgressStore::new(db.path());
    let target_fingerprint = reindex_embedder_fingerprint(config, embedder_name, new_dim);
    let resume_checkpoint = if resume {
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
    if resume_checkpoint.is_none() && (!stale_only || current_dim != Some(new_dim)) {
        println!("recreating drawer_vectors with {new_dim} dimensions...");
        db.recreate_vectors_table(new_dim)
            .context("failed to recreate vectors table")?;
    } else if stale_only {
        println!("stale-only reindex preserving existing drawer_vectors table");
    } else {
        println!("resume checkpoint found; preserving existing drawer_vectors table");
    }
    let mut drawers = reindex_rows(db).context("failed to load active drawers for reindex")?;
    if stale_only {
        drawers.retain(|row| reindex_row_is_stale(db, row, &target_fingerprint).unwrap_or(true));
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
            progress_store
                .mark_done(sp, Some(*ci), embedder_name)
                .context("failed to mark completed reindex source")?;
        }
        active_source = Some(row.source_path.clone());
        let single_input = [row.content.as_str()];
        let embed_future = embedder.embed(&single_input);
        let vectors = tokio::select! { _ = tokio::signal::ctrl_c() => { if let Some((sp, ci)) = last_processed.as_ref() { progress_store.mark_paused(sp, Some(*ci), embedder_name).context("failed to persist paused reindex checkpoint")?; } bail!("reindex interrupted; resume with `mempal reindex --embedder {embedder_name} --resume`"); } result = embed_future => result.context("embedding failed during reindex")? };
        let vector = vectors
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embedder returned no vector during reindex"))?;
        db.conn()
            .execute("DELETE FROM drawer_vectors WHERE id = ?1", [&row.id])
            .with_context(|| format!("failed to clear existing vector for {}", row.id))?;
        db.insert_vector(&row.id, &vector)
            .with_context(|| format!("failed to insert vector for {}", row.id))?;
        record_reindex_metadata(
            db,
            &row.id,
            CURRENT_REINDEX_NORMALIZE_VERSION,
            &target_fingerprint,
        )
        .with_context(|| format!("failed to record reindex metadata for {}", row.id))?;
        progress_store
            .upsert_running(&row.source_path, Some(row.chunk_index), embedder_name)
            .context("failed to persist reindex checkpoint")?;
        done += 1;
        last_processed = Some((row.source_path.clone(), row.chunk_index));
        println!("  {done}/{total}");
        if test_stop_after.is_some_and(|limit| done >= limit) {
            progress_store
                .mark_paused(&row.source_path, Some(row.chunk_index), embedder_name)
                .context("failed to persist paused reindex checkpoint")?;
            bail!("reindex interrupted for test after {done} drawers");
        }
    }
    if let Some((sp, ci)) = last_processed.as_ref() {
        progress_store
            .mark_done(sp, Some(*ci), embedder_name)
            .context("failed to finalize reindex checkpoint")?;
    }
    println!("reindex complete: {total} drawers, {new_dim}d vectors");
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

fn knowledge_card_command(db: &Database, command: KnowledgeCardCommands) -> Result<()> {
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
            };
            let cards = db
                .list_knowledge_cards(&filter)
                .context("failed to list knowledge cards")?;
            print_knowledge_cards(&cards, &format)?;
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
    }
    Ok(())
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
                    "{} tier={} status={} domain={} field={} anchor={} {}",
                    card.id,
                    knowledge_tier_slug(&card.tier),
                    knowledge_status_slug(&card.status),
                    domain_slug(&card.domain),
                    card.field,
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

fn status_command(db: &Database, config: &Config) -> Result<()> {
    let cfg_meta = ConfigHandle::snapshot_meta();
    let scrub_stats = ConfigHandle::scrub_stats();
    let runtime_warnings = ConfigHandle::collect_runtime_warnings();
    let embed_status = global_embed_status().snapshot();
    let queue_stats = mempal::core::queue::PendingMessageStore::new(db.path())
        .context("failed to open pending message store")?
        .stats()
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
    let project_breakdown = db
        .project_breakdown()
        .context("failed to count drawers per project")?;
    let null_project_backfill_pending = db
        .null_project_backfill_pending_count()
        .context("failed to count pending project backfill drawers")?;
    let taxonomy_count = db.taxonomy_count().context("failed to count taxonomy")?;
    let gating_drop_counts = db
        .gating_drop_counts()
        .context("failed to read gating counters")?;
    let gating_stats =
        observability::gating_stats(db, config, None).context("failed to read gating stats")?;
    let db_size_bytes = db
        .database_size_bytes()
        .context("failed to compute database size")?;
    let deleted_count = db
        .deleted_drawer_count()
        .context("failed to count deleted drawers")?;
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
    println!("drawer_count: {drawer_count}");
    println!("drawers per project:");
    if project_breakdown.is_empty() {
        println!("(none)");
    } else {
        for (pid, count) in project_breakdown {
            match pid {
                Some(pid) => println!("{}={count}", escape_project_id_for_display(&pid)),
                None => println!("NULL={count}"),
            }
        }
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
    let triple_count = db.triple_count().context("failed to count triples")?;
    println!("taxonomy_entries: {taxonomy_count}");
    if triple_count > 0 {
        println!("triples: {triple_count}");
    }
    println!("db_size_bytes: {db_size_bytes}");
    println!(
        "config: version={} loaded_unix_ms={}",
        cfg_meta.version, cfg_meta.loaded_at_unix_ms
    );
    println!("embed_fail_count: {}", embed_status.fail_count);
    println!("embed_degraded: {}", embed_status.degraded);
    if let Some(last_error) = embed_status.last_error {
        println!("embed_last_error: {last_error}");
    }
    if let Some(last_success_at) = embed_status.last_success_at_unix_ms {
        println!("embed_last_success_at_unix_ms: {last_success_at}");
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
    println!("Queue:");
    println!("  pending: {}", queue_stats.pending);
    println!("  claimed: {}", queue_stats.claimed);
    println!("  failed: {}", queue_stats.failed);
    match queue_stats.oldest_pending_age_secs {
        Some(age) => println!("  oldest_pending_age_secs: {age}"),
        None => println!("  oldest_pending_age_secs: none"),
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
    let dropped_total = gating_drop_counts
        .total
        .unwrap_or_else(|| gating_drop_counts.by_reason.values().copied().sum::<u64>());
    println!("  dropped_total: {dropped_total}");
    if nonzero.is_empty() {
        println!("  dropped_by_reason: none");
    } else {
        println!("  dropped_by_reason: {}", nonzero.join(", "));
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
    let counts = db.scope_counts().context("failed to query scope counts")?;
    println!("scopes:");
    if counts.is_empty() {
        println!("- none");
    } else {
        for (wing, room, count) in counts {
            println!("- {wing}/{}: {count}", render_room(room.as_deref()));
        }
    }
    Ok(())
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
    let server = MempalMcpServer::new(expand_home(&config.db_path), config.clone());
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
    let mut rest_task = tokio::spawn(async move {
        serve_rest_api(listener, state)
            .await
            .context("REST server failed")
    });
    let server = MempalMcpServer::new(db_path, config.clone());
    let service = server.serve_stdio().await?;
    let mut mcp_task = Box::pin(async move {
        service.waiting().await.context("MCP server failed")?;
        Ok(())
    });
    tokio::select! { mcp_result = &mut mcp_task => { rest_task.abort(); match rest_task.await { Ok(Ok(())) => {} Ok(Err(e)) => return Err(e), Err(je) if je.is_cancelled() => {} Err(je) => return Err(anyhow::Error::new(je).context("failed to join REST task")) } mcp_result } rest_result = &mut rest_task => match rest_result { Ok(Ok(())) => bail!("REST server exited unexpectedly"), Ok(Err(e)) => Err(e), Err(je) => Err(anyhow::Error::new(je).context("failed to join REST task")) } }
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
    source_path: String,
    chunk_index: i64,
}
const CURRENT_REINDEX_NORMALIZE_VERSION: &str = "v1";

fn reindex_rows(db: &Database) -> Result<Vec<ReindexRow>> {
    let mut stmt = db.conn().prepare(r#"SELECT id, content, COALESCE(source_file, id) AS source_path, COALESCE(chunk_index, 0) AS chunk_index FROM drawers WHERE deleted_at IS NULL ORDER BY source_path ASC, chunk_index ASC, id ASC"#).context("failed to prepare reindex query")?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ReindexRow {
                id: row.get(0)?,
                content: row.get(1)?,
                source_path: row.get(2)?,
                chunk_index: row.get(3)?,
            })
        })
        .context("failed to query reindex rows")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to collect reindex rows")?;
    Ok(rows)
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
fn reindex_embedder_fingerprint(config: &Config, backend: &str, dim: usize) -> String {
    let base_url = config
        .embed
        .resolved_openai_base_url()
        .unwrap_or_default()
        .trim_end_matches('/');
    let model = config.embed.resolved_openai_model().unwrap_or_default();
    format!("{backend}:{model}:{base_url}:{dim}")
}
fn reindex_metadata_key(drawer_id: &str, field: &str) -> String {
    format!("reindex:{drawer_id}:{field}")
}
fn load_reindex_metadata(db: &Database, drawer_id: &str, field: &str) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    db.conn()
        .query_row(
            "SELECT value FROM fork_ext_meta WHERE key = ?1",
            [reindex_metadata_key(drawer_id, field)],
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
    db.conn().execute(r#"INSERT INTO fork_ext_meta (key, value) VALUES (?1, ?2), (?3, ?4) ON CONFLICT(key) DO UPDATE SET value = excluded.value"#, rusqlite::params![reindex_metadata_key(drawer_id, "normalize_version"), normalize_version, reindex_metadata_key(drawer_id, "embedder_fingerprint"), embedder_fingerprint]).context("failed to write reindex metadata")?;
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
    let nv = load_reindex_metadata(db, &row.id, "normalize_version")?;
    if nv.as_deref() != Some(CURRENT_REINDEX_NORMALIZE_VERSION) {
        return Ok(true);
    }
    let fp = load_reindex_metadata(db, &row.id, "embedder_fingerprint")?;
    Ok(fp.as_deref() != Some(target_fingerprint))
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
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
