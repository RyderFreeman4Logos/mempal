#![warn(clippy::all)]

use anyhow::{Context, Result, bail};
use clap::Subcommand;

use mempal::core::{
    case_skill::{SkillProposalOptions, propose_skills_from_cases},
    config::Config,
    db::Database,
    project::resolve_project_id,
    skills::{
        PromoteArgs, PromotionError, adopt_skill, get_skill, list_skills, promote_pattern_to_skill,
        reject_skill, retire_skill, skills_table_exists,
    },
};

#[derive(Debug, Clone, Subcommand)]
pub enum SkillsCommands {
    /// List skills (all or filtered by status).
    List {
        /// Filter by status: probationary, active, retired. Omit for all.
        #[arg(long)]
        status: Option<String>,
        /// Filter by project ID.
        #[arg(long)]
        project: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show details for a single skill.
    Show {
        /// Skill ID.
        skill_id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Promote an active pattern to a probationary skill.
    Promote {
        /// Pattern ID to promote.
        pattern_id: String,
        /// Human-readable name for the skill.
        #[arg(long)]
        name: String,
        /// Trigger description used to decide when to apply the skill.
        #[arg(long)]
        trigger: String,
        /// Optional project scope.
        #[arg(long)]
        project: Option<String>,
    },
    /// Propose probationary skills from repeated verified cases.
    Propose {
        /// Build proposals from memory_kind=case drawers.
        #[arg(long = "from-cases")]
        from_cases: bool,
        /// Minimum successful verified cases required per procedure key.
        #[arg(long = "min-support", required = true)]
        min_support: usize,
        /// Minimum verification evidence refs required per successful case.
        #[arg(long = "min-verification-refs", default_value_t = 1)]
        min_verification_refs: usize,
        /// Optional case wing filter.
        #[arg(long)]
        wing: Option<String>,
        /// Optional project scope.
        #[arg(long)]
        project: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
        /// Show proposals without writing skill drawers.
        #[arg(long)]
        dry_run: bool,
    },
    /// Record adoption of a skill (positive signal).
    Adopt {
        /// Skill ID.
        skill_id: String,
    },
    /// Record rejection of a skill (negative signal).
    Reject {
        /// Skill ID.
        skill_id: String,
    },
    /// Manually retire a skill.
    Retire {
        /// Skill ID.
        skill_id: String,
    },
}

pub fn run_command(config: &Config, command: SkillsCommands) -> Result<()> {
    let db_path = mempal::core::utils::expand_home(&config.db_path);
    let db = Database::open(std::path::Path::new(&db_path))
        .with_context(|| format!("failed to open database at {}", db_path.display()))?;

    if !skills_table_exists(db.conn()) {
        bail!("skills table not yet created — run `mempal init` to apply migrations");
    }

    match command {
        SkillsCommands::List {
            status,
            project,
            json,
        } => cmd_list(&db, status.as_deref(), project.as_deref(), json),
        SkillsCommands::Show { skill_id, json } => cmd_show(&db, &skill_id, json),
        SkillsCommands::Promote {
            pattern_id,
            name,
            trigger,
            project,
        } => cmd_promote(
            &db,
            config,
            &pattern_id,
            &name,
            &trigger,
            project.as_deref(),
        ),
        SkillsCommands::Propose {
            from_cases,
            min_support,
            min_verification_refs,
            wing,
            project,
            json,
            dry_run,
        } => cmd_propose_from_cases(
            &db,
            config,
            ProposeFromCasesCliArgs {
                from_cases,
                min_support,
                min_verification_refs,
                wing,
                project,
                json,
                dry_run,
            },
        ),
        SkillsCommands::Adopt { skill_id } => cmd_adopt(&db, config, &skill_id),
        SkillsCommands::Reject { skill_id } => cmd_reject(&db, config, &skill_id),
        SkillsCommands::Retire { skill_id } => cmd_retire(&db, &skill_id),
    }
}

fn cmd_list(db: &Database, status: Option<&str>, project: Option<&str>, json: bool) -> Result<()> {
    let skills = list_skills(db.conn(), status, project).context("failed to list skills")?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&skills).context("failed to serialize skills")?
        );
        return Ok(());
    }

    if skills.is_empty() {
        println!("no skills found");
        return Ok(());
    }

    println!(
        "{:<38} {:<14} {:>6} {:>7}  {:>6}  NAME",
        "SKILL_ID", "STATUS", "ADOPT", "REJECT", "ETA"
    );
    println!("{}", "-".repeat(100));
    for s in &skills {
        println!(
            "{:<38} {:<14} {:>6} {:>7}  {:>5.2}  {}",
            s.skill_id,
            s.status.as_str(),
            s.adoption_count,
            s.rejection_count,
            s.eta(),
            s.name,
        );
    }
    Ok(())
}

fn cmd_show(db: &Database, skill_id: &str, json: bool) -> Result<()> {
    let skill = get_skill(db.conn(), skill_id)
        .context("failed to fetch skill")?
        .with_context(|| format!("skill not found: {skill_id}"))?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&skill).context("failed to serialize skill")?
        );
        return Ok(());
    }

    println!("Skill ID:         {}", skill.skill_id);
    println!("Name:             {}", skill.name);
    println!("Status:           {}", skill.status.as_str());
    println!("Pattern ID:       {}", skill.pattern_id);
    println!("Adoption count:   {}", skill.adoption_count);
    println!("Rejection count:  {}", skill.rejection_count);
    println!("ETA:              {:.4}", skill.eta());
    println!(
        "Project ID:       {}",
        skill.project_id.as_deref().unwrap_or("(all)")
    );
    println!("Promoted at:      {}", format_epoch_ms(skill.promoted_at));
    println!("Updated at:       {}", format_epoch_ms(skill.updated_at));
    println!("Trigger:          {}", skill.trigger_description);
    println!("\nExemplar drawer IDs:");
    for id in &skill.exemplar_ids {
        println!("  {}", id);
    }
    Ok(())
}

fn cmd_promote(
    db: &Database,
    config: &Config,
    pattern_id: &str,
    name: &str,
    trigger: &str,
    project: Option<&str>,
) -> Result<()> {
    let current_dir = std::env::current_dir().ok();
    let project_id = resolve_project_id(project, config, current_dir.as_deref())
        .context("failed to resolve skill project id")?;
    let args = PromoteArgs {
        pattern_id,
        name,
        trigger_description: trigger,
        project_id: project_id.as_deref(),
        skill_min_sessions: config.skills.skill_min_sessions,
    };
    let _writer_lease = super::acquire_cli_content_writer_lease(db, "skills-promote")?;
    match promote_pattern_to_skill(db.conn(), &args) {
        Ok(skill) => {
            println!(
                "skill {} created (probationary) — adopt {} more time(s) to activate",
                skill.skill_id,
                config.skills.active_threshold - skill.adoption_count
            );
        }
        Err(PromotionError::PatternNotFound(_)) => {
            bail!("pattern not found: {pattern_id}");
        }
        Err(PromotionError::PatternNotActive(_)) => {
            bail!("pattern {pattern_id} is not active — only active patterns can be promoted");
        }
        Err(PromotionError::InsufficientSessions(have, need)) => {
            bail!("pattern {pattern_id} has only {have} sessions; need at least {need} to promote");
        }
        Err(PromotionError::SkillAlreadyExists) => {
            bail!("a probationary or active skill already exists for this pattern");
        }
        Err(PromotionError::Db(e)) => {
            bail!("database error during promotion: {e}");
        }
    }
    Ok(())
}

struct ProposeFromCasesCliArgs {
    from_cases: bool,
    min_support: usize,
    min_verification_refs: usize,
    wing: Option<String>,
    project: Option<String>,
    json: bool,
    dry_run: bool,
}

fn cmd_propose_from_cases(
    db: &Database,
    config: &Config,
    args: ProposeFromCasesCliArgs,
) -> Result<()> {
    if !args.from_cases {
        bail!("skills propose currently requires --from-cases");
    }
    let current_dir = std::env::current_dir().ok();
    let project_id = resolve_project_id(args.project.as_deref(), config, current_dir.as_deref())
        .context("failed to resolve skill proposal project id")?;
    let _writer_lease = if args.dry_run {
        None
    } else {
        Some(super::acquire_cli_content_writer_lease(
            db,
            "skills-propose",
        )?)
    };
    let outcome = propose_skills_from_cases(
        db,
        SkillProposalOptions {
            from_cases: args.from_cases,
            min_support: args.min_support,
            min_verification_refs: args.min_verification_refs,
            wing: args.wing,
            project_id,
            dry_run: args.dry_run,
        },
    )
    .context("failed to propose skills from cases")?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&outcome)
                .context("failed to serialize skill proposals")?
        );
        return Ok(());
    }

    if outcome.proposals.is_empty() {
        println!("no case-backed skill proposals met the support threshold");
        return Ok(());
    }

    println!(
        "{:<38} {:>7} {:>7} {:>7}  PROCEDURE",
        "SKILL_ID", "SUPPORT", "VERIFY", "COUNTER"
    );
    println!("{}", "-".repeat(100));
    for proposal in &outcome.proposals {
        println!(
            "{:<38} {:>7} {:>7} {:>7}  {}",
            proposal.skill_id.as_deref().unwrap_or("(dry-run)"),
            proposal.support_count,
            proposal.verification_ref_count,
            proposal.counterexample_count,
            proposal.procedure_key
        );
    }
    Ok(())
}

fn cmd_adopt(db: &Database, config: &Config, skill_id: &str) -> Result<()> {
    let _writer_lease = super::acquire_cli_content_writer_lease(db, "skills-adopt")?;
    let new_status = adopt_skill(db.conn(), skill_id, config.skills.active_threshold)
        .with_context(|| format!("failed to adopt skill {skill_id}"))?;
    match new_status {
        None => bail!("skill not found: {skill_id}"),
        Some(s) => println!(
            "skill {skill_id} adoption recorded — status: {}",
            s.as_str()
        ),
    }
    Ok(())
}

fn cmd_reject(db: &Database, config: &Config, skill_id: &str) -> Result<()> {
    let _writer_lease = super::acquire_cli_content_writer_lease(db, "skills-reject")?;
    let new_status = reject_skill(db.conn(), skill_id, config.skills.retire_threshold)
        .with_context(|| format!("failed to reject skill {skill_id}"))?;
    match new_status {
        None => bail!("skill not found: {skill_id}"),
        Some(s) => println!(
            "skill {skill_id} rejection recorded — status: {}",
            s.as_str()
        ),
    }
    Ok(())
}

fn cmd_retire(db: &Database, skill_id: &str) -> Result<()> {
    let _writer_lease = super::acquire_cli_content_writer_lease(db, "skills-retire")?;
    let found = retire_skill(db.conn(), skill_id)
        .with_context(|| format!("failed to retire skill {skill_id}"))?;
    if found {
        println!("skill {skill_id} retired");
    } else {
        println!("skill {skill_id} not found");
    }
    Ok(())
}

fn format_epoch_ms(ms: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    UNIX_EPOCH
        .checked_add(Duration::from_millis(ms as u64))
        .map(|t| {
            let secs = t
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let (y, mo, d, h, min, s) = epoch_to_ymd_hms(secs);
            format!("{y:04}-{mo:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
        })
        .unwrap_or_else(|| ms.to_string())
}

fn epoch_to_ymd_hms(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    let y400 = days / 146097;
    let d1 = days % 146097;
    let y100 = d1 / 36524;
    let d2 = d1 % 36524;
    let y4 = d2 / 1461;
    let d3 = d2 % 1461;
    let y1 = d3 / 365;
    let yd = d3 % 365;
    let year = y400 * 400 + y100 * 100 + y4 * 4 + y1 + 1970;
    let (mo, d) = doy_to_md(yd + 1, is_leap(year));
    (year, mo as u64, d as u64, h, m, s)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn doy_to_md(doy: u64, leap: bool) -> (u32, u32) {
    let months = if leap {
        [31u32, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut rem = doy as u32;
    for (i, &days) in months.iter().enumerate() {
        if rem <= days {
            return (i as u32 + 1, rem);
        }
        rem -= days;
    }
    (12, 31)
}
