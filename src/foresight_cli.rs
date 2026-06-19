#![warn(clippy::all)]

use std::env;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use mempal::core::{
    anchor,
    config::Config,
    db::Database,
    decay::parse_temporal_timestamp_secs,
    foresight::{
        ForesightCreateRequest, ForesightListRequest, ForesightResolveRequest, create_foresight,
        current_unix_secs, list_foresights, resolve_foresight,
    },
    project::{ProjectSearchScope, resolve_project_id},
    types::{AnchorKind, MemoryDomain},
};

#[derive(Debug, Clone, Subcommand)]
pub enum ForesightCommands {
    /// Create a future-bound foresight memory signal.
    Add(Box<ForesightAddArgs>),
    /// List due foresights, with opt-in future/resolved/expired visibility.
    List(Box<ForesightListArgs>),
    /// Mark a foresight resolved and hide it from normal retrieval.
    Resolve {
        /// Foresight drawer id.
        drawer_id: String,
        /// Optional resolution note.
        #[arg(long)]
        note: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Args)]
pub struct ForesightAddArgs {
    /// Future check or risk statement.
    #[arg(long)]
    statement: String,
    /// Deterministic trigger condition that should make this relevant.
    #[arg(long)]
    trigger: String,
    /// Due timestamp. Accepts Unix seconds or RFC3339.
    #[arg(long = "due-at")]
    due_at: String,
    /// Optional reason for the future check.
    #[arg(long)]
    reason: Option<String>,
    /// Optional timestamp before which normal retrieval should ignore it.
    #[arg(long = "valid-from")]
    valid_from: Option<String>,
    /// Optional expiration timestamp after which it is no longer relevant.
    #[arg(long = "valid-until")]
    valid_until: Option<String>,
    /// Supporting evidence drawer id. Repeatable.
    #[arg(long = "supporting-ref")]
    supporting_refs: Vec<String>,
    /// Counterexample evidence drawer id. Repeatable.
    #[arg(long = "counterexample-ref")]
    counterexample_refs: Vec<String>,
    /// Verification evidence drawer id. Repeatable.
    #[arg(long = "verification-ref")]
    verification_refs: Vec<String>,
    /// Wing for the foresight drawer.
    #[arg(long, default_value = "mempal")]
    wing: String,
    /// Room for the foresight drawer.
    #[arg(long, default_value = "foresight")]
    room: String,
    /// Optional project scope.
    #[arg(long)]
    project: Option<String>,
    /// Memory domain.
    #[arg(long, default_value = "project")]
    domain: String,
    /// Mind-model field.
    #[arg(long, default_value = "foresight")]
    field: String,
    /// Anchor kind for explicit --anchor-id. Defaults to derived worktree anchor.
    #[arg(long = "anchor-kind", default_value = "worktree")]
    anchor_kind: String,
    /// Explicit anchor id. If omitted, derive the current worktree anchor.
    #[arg(long = "anchor-id")]
    anchor_id: Option<String>,
    /// Optional source/provenance display path.
    #[arg(long = "source-file")]
    source_file: Option<String>,
    /// Importance ranking (0-5).
    #[arg(long, default_value_t = 3)]
    importance: i32,
    /// Print JSON.
    #[arg(long)]
    json: bool,
    /// Show the candidate drawer id without writing.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ForesightListArgs {
    /// Optional project scope.
    #[arg(long)]
    project: Option<String>,
    /// Include legacy global rows in project scope.
    #[arg(long, default_value_t = false)]
    include_global: bool,
    /// Search all project scopes.
    #[arg(long, default_value_t = false)]
    all_projects: bool,
    /// Optional memory domain filter.
    #[arg(long)]
    domain: Option<String>,
    /// Optional mind-model field filter.
    #[arg(long)]
    field: Option<String>,
    /// Optional anchor kind filter.
    #[arg(long = "anchor-kind")]
    anchor_kind: Option<String>,
    /// Optional anchor id filter.
    #[arg(long = "anchor-id")]
    anchor_id: Option<String>,
    /// Include future, not-yet-due foresights.
    #[arg(long = "include-future", default_value_t = false)]
    include_future: bool,
    /// Include resolved foresights.
    #[arg(long = "include-resolved", default_value_t = false)]
    include_resolved: bool,
    /// Include expired foresights.
    #[arg(long = "include-expired", default_value_t = false)]
    include_expired: bool,
    /// Include future, resolved, and expired rows.
    #[arg(long, default_value_t = false)]
    all: bool,
    /// Evaluation timestamp for deterministic due filtering.
    #[arg(long)]
    now: Option<String>,
    /// Maximum rows to print.
    #[arg(long, default_value_t = 20)]
    limit: usize,
    /// Output format: plain or json.
    #[arg(long, default_value = "plain")]
    format: String,
}

pub fn run_command(db: &Database, config: &Config, command: ForesightCommands) -> Result<()> {
    match command {
        ForesightCommands::Add(args) => {
            let ForesightAddArgs {
                statement,
                trigger,
                due_at,
                reason,
                valid_from,
                valid_until,
                supporting_refs,
                counterexample_refs,
                verification_refs,
                wing,
                room,
                project,
                domain,
                field,
                anchor_kind,
                anchor_id,
                source_file,
                importance,
                json,
                dry_run,
            } = *args;
            let current_dir = env::current_dir().ok();
            let project_id = resolve_project_id(project.as_deref(), config, current_dir.as_deref())
                .context("failed to resolve foresight project id")?;
            let domain = parse_domain(&domain)?;
            let (anchor_kind, anchor_id, parent_anchor_id) =
                resolve_anchor(anchor_kind.as_str(), anchor_id.as_deref())?;
            let outcome = create_foresight(
                db,
                ForesightCreateRequest {
                    statement,
                    reason,
                    trigger_condition: trigger,
                    due_at,
                    valid_from,
                    valid_until,
                    supporting_refs,
                    counterexample_refs,
                    verification_refs,
                    wing,
                    room: Some(room),
                    project_id,
                    domain,
                    field,
                    anchor_kind,
                    anchor_id,
                    parent_anchor_id,
                    source_file,
                    importance,
                    dry_run,
                },
            )
            .context("failed to create foresight")?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&outcome)
                        .context("failed to serialize foresight create outcome")?
                );
            } else {
                println!(
                    "foresight_id={} created={} dry_run={}",
                    outcome.drawer_id, outcome.created, outcome.dry_run
                );
            }
        }
        ForesightCommands::List(args) => {
            let ForesightListArgs {
                project,
                include_global,
                all_projects,
                domain,
                field,
                anchor_kind,
                anchor_id,
                include_future,
                include_resolved,
                include_expired,
                all,
                now,
                limit,
                format,
            } = *args;
            let current_dir = env::current_dir().ok();
            let project_id = resolve_project_id(project.as_deref(), config, current_dir.as_deref())
                .context("failed to resolve foresight project id")?;
            let scope = ProjectSearchScope::from_request(
                project_id,
                include_global,
                all_projects,
                config.search.strict_project_isolation,
            );
            let now_unix = match now.as_deref() {
                Some(value) => parse_temporal_timestamp_secs(value)
                    .with_context(|| format!("invalid --now timestamp: {value}"))?,
                None => current_unix_secs(),
            };
            let rows = list_foresights(
                db,
                ForesightListRequest {
                    scope,
                    domain: domain.as_deref().map(parse_domain).transpose()?,
                    field,
                    anchor_kind: anchor_kind.as_deref().map(parse_anchor_kind).transpose()?,
                    anchor_id,
                    include_pending: include_future || all,
                    include_resolved: include_resolved || all,
                    include_expired: include_expired || all,
                    now_unix,
                    limit: Some(limit),
                },
            )
            .context("failed to list foresights")?;
            match format.as_str() {
                "plain" => print_foresights_plain(&rows),
                "json" => println!(
                    "{}",
                    serde_json::to_string_pretty(&rows)
                        .context("failed to serialize foresights")?
                ),
                other => bail!("unsupported foresight list format: {other}"),
            }
        }
        ForesightCommands::Resolve {
            drawer_id,
            note,
            json,
        } => {
            let outcome = resolve_foresight(
                db,
                ForesightResolveRequest {
                    drawer_id,
                    resolution_note: note,
                },
            )
            .context("failed to resolve foresight")?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&outcome)
                        .context("failed to serialize foresight resolve outcome")?
                );
            } else {
                println!(
                    "foresight_id={} resolved={} resolved_at={}",
                    outcome.drawer_id, outcome.resolved, outcome.resolved_at
                );
            }
        }
    }
    Ok(())
}

fn resolve_anchor(
    anchor_kind: &str,
    anchor_id: Option<&str>,
) -> Result<(AnchorKind, String, Option<String>)> {
    let parsed_kind = parse_anchor_kind(anchor_kind)?;
    if let Some(explicit_id) = anchor_id {
        anchor::validate_explicit_anchor(&parsed_kind, explicit_id)
            .context("invalid explicit foresight anchor")?;
        return Ok((parsed_kind, explicit_id.to_string(), None));
    }

    if parsed_kind == AnchorKind::Worktree {
        let cwd = env::current_dir().context("failed to read current directory")?;
        let derived = anchor::derive_anchor_from_cwd(Some(cwd.as_path()))
            .context("failed to derive anchor")?;
        return Ok((
            derived.anchor_kind,
            derived.anchor_id,
            derived.parent_anchor_id,
        ));
    }

    if parsed_kind == AnchorKind::Repo {
        return Ok(anchor::bootstrap_anchor());
    }

    bail!("--anchor-id is required for anchor-kind {anchor_kind}");
}

fn parse_domain(value: &str) -> Result<MemoryDomain> {
    match value {
        "project" => Ok(MemoryDomain::Project),
        "user" => Ok(MemoryDomain::User),
        "agent" => Ok(MemoryDomain::Agent),
        "skill" => Ok(MemoryDomain::Skill),
        "global" => Ok(MemoryDomain::Global),
        other => bail!("unsupported foresight domain: {other}"),
    }
}

fn parse_anchor_kind(value: &str) -> Result<AnchorKind> {
    match value {
        "global" => Ok(AnchorKind::Global),
        "repo" => Ok(AnchorKind::Repo),
        "worktree" => Ok(AnchorKind::Worktree),
        other => bail!("unsupported foresight anchor kind: {other}"),
    }
}

fn print_foresights_plain(rows: &[mempal::core::foresight::Foresight]) {
    if rows.is_empty() {
        println!("no foresights");
        return;
    }

    for row in rows {
        println!(
            "{} status={} due_at={} anchor={} field={}",
            row.drawer_id,
            row.status.as_str(),
            row.due_at,
            row.anchor_id,
            row.field
        );
        println!("  trigger: {}", row.trigger_condition);
        println!("  statement: {}", row.statement);
        if let Some(reason) = row.reason.as_deref() {
            println!("  reason: {reason}");
        }
        if !row.supporting_refs.is_empty() {
            println!("  supporting_refs: {}", row.supporting_refs.join(","));
        }
        if !row.counterexample_refs.is_empty() {
            println!(
                "  counterexample_refs: {}",
                row.counterexample_refs.join(",")
            );
        }
        if !row.verification_refs.is_empty() {
            println!("  verification_refs: {}", row.verification_refs.join(","));
        }
    }
}
