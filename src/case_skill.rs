#![warn(clippy::all)]

use anyhow::{Context, Result};
use clap::Subcommand;

use mempal::core::{
    case_skill::{CaseCloseRequest, CaseOpenRequest, CaseVerdict, close_case, open_case},
    config::Config,
    db::Database,
    project::resolve_project_id,
};

#[derive(Debug, Clone, Subcommand)]
pub enum CaseCommands {
    /// Open a procedural case with task trajectory metadata.
    Open {
        /// Task or problem statement this case captures.
        #[arg(long)]
        task: String,
        /// Explicit deterministic grouping key for later skill proposals.
        #[arg(long = "procedure-key")]
        procedure_key: String,
        /// Human-readable procedure summary.
        #[arg(long = "procedure")]
        procedure: String,
        /// Procedure step. Repeat for ordered steps.
        #[arg(long = "step")]
        steps: Vec<String>,
        /// Task trajectory note. Repeat for ordered observations.
        #[arg(long = "trajectory")]
        trajectory: Vec<String>,
        /// Anti-pattern seen during the case.
        #[arg(long = "anti-pattern")]
        anti_patterns: Vec<String>,
        /// Failed approach seen during the case.
        #[arg(long = "failed-approach")]
        failed_approaches: Vec<String>,
        /// Wing for the case drawer.
        #[arg(long, default_value = "mempal")]
        wing: String,
        /// Room for the case drawer.
        #[arg(long, default_value = "cases")]
        room: String,
        /// Optional project scope.
        #[arg(long)]
        project: Option<String>,
        /// Importance ranking (0-5).
        #[arg(long, default_value_t = 3)]
        importance: i32,
        /// Print JSON.
        #[arg(long)]
        json: bool,
        /// Show the candidate drawer id without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Close an open case with verdict, tests, and verification refs.
    Close {
        /// Case drawer id.
        case_id: String,
        /// Verdict: success, failure, or inconclusive.
        #[arg(long)]
        verdict: String,
        /// Test or local verification command/result.
        #[arg(long = "test")]
        tests: Vec<String>,
        /// Evidence drawer proving the verdict. Required for success.
        #[arg(long = "verification-ref")]
        verification_refs: Vec<String>,
        /// Anti-pattern seen during the case.
        #[arg(long = "anti-pattern")]
        anti_patterns: Vec<String>,
        /// Failed approach seen during the case.
        #[arg(long = "failed-approach")]
        failed_approaches: Vec<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

pub fn run_command(config: &Config, command: CaseCommands) -> Result<()> {
    let db_path = mempal::core::utils::expand_home(&config.db_path);
    let db = Database::open(std::path::Path::new(&db_path))
        .with_context(|| format!("failed to open database at {}", db_path.display()))?;

    match command {
        CaseCommands::Open {
            task,
            procedure_key,
            procedure,
            steps,
            trajectory,
            anti_patterns,
            failed_approaches,
            wing,
            room,
            project,
            importance,
            json,
            dry_run,
        } => {
            let current_dir = std::env::current_dir().ok();
            let project_id = resolve_project_id(project.as_deref(), config, current_dir.as_deref())
                .context("failed to resolve case project id")?;
            let outcome = open_case(
                &db,
                CaseOpenRequest {
                    task,
                    procedure_key,
                    procedure_summary: procedure,
                    procedure_steps: steps,
                    trajectory,
                    anti_patterns,
                    failed_approaches,
                    wing,
                    room,
                    project_id,
                    importance,
                    dry_run,
                },
            )
            .context("failed to open case")?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&outcome)
                        .context("failed to serialize case open outcome")?
                );
            } else {
                println!(
                    "case_id={} created={} dry_run={}",
                    outcome.case_id, outcome.created, outcome.dry_run
                );
            }
        }
        CaseCommands::Close {
            case_id,
            verdict,
            tests,
            verification_refs,
            anti_patterns,
            failed_approaches,
            json,
        } => {
            let verdict = verdict.parse::<CaseVerdict>()?;
            let outcome = close_case(
                &db,
                CaseCloseRequest {
                    case_id,
                    verdict,
                    tests,
                    verification_refs,
                    anti_patterns,
                    failed_approaches,
                },
            )
            .context("failed to close case")?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&outcome)
                        .context("failed to serialize case close outcome")?
                );
            } else {
                println!(
                    "case_id={} verdict={} verification_refs={}",
                    outcome.case_id,
                    outcome.verdict.as_str(),
                    outcome.verification_refs.join(",")
                );
            }
        }
    }
    Ok(())
}
