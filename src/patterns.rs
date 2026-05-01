#![warn(clippy::all)]

use anyhow::{Context, Result, bail};
use clap::Subcommand;

use mempal::core::{
    config::Config,
    db::Database,
    patterns::{
        get_pattern, list_patterns, patterns_table_exists, promote_pattern, retire_pattern,
    },
};

#[derive(Debug, Clone, Subcommand)]
pub enum PatternsCommands {
    /// List patterns (all or filtered by status).
    List {
        /// Filter by status: candidate, active, retired. Omit to list all.
        #[arg(long)]
        status: Option<String>,
        /// Filter by project ID.
        #[arg(long)]
        project: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show details for a single pattern.
    Show {
        /// Pattern ID.
        pattern_id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Retire (deactivate) a pattern.
    Retire {
        /// Pattern ID.
        pattern_id: String,
    },
    /// Promote a candidate pattern to active.
    Promote {
        /// Pattern ID.
        pattern_id: String,
    },
}

pub fn run_command(config: &Config, command: PatternsCommands) -> Result<()> {
    let db_path = mempal::core::utils::expand_home(&config.db_path);
    let db = Database::open(std::path::Path::new(&db_path))
        .with_context(|| format!("failed to open database at {}", db_path.display()))?;

    if !patterns_table_exists(db.conn()) {
        bail!("patterns table not yet created — run `mempal init` to apply migrations");
    }

    match command {
        PatternsCommands::List {
            status,
            project,
            json,
        } => cmd_list(&db, status.as_deref(), project.as_deref(), json),
        PatternsCommands::Show { pattern_id, json } => cmd_show(&db, &pattern_id, json),
        PatternsCommands::Retire { pattern_id } => cmd_retire(&db, &pattern_id),
        PatternsCommands::Promote { pattern_id } => cmd_promote(&db, &pattern_id),
    }
}

fn cmd_list(db: &Database, status: Option<&str>, project: Option<&str>, json: bool) -> Result<()> {
    let patterns = list_patterns(db.conn(), status, project).context("failed to list patterns")?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&patterns).context("failed to serialize patterns")?
        );
        return Ok(());
    }

    if patterns.is_empty() {
        println!("no patterns found");
        return Ok(());
    }

    println!(
        "{:<38} {:<12} {:>8} {:>10}  TOPIC_TAGS",
        "PATTERN_ID", "STATUS", "SESSIONS", "EXEMPLARS"
    );
    println!("{}", "-".repeat(90));
    for p in &patterns {
        let tags = p.topic_tags.join(", ");
        println!(
            "{:<38} {:<12} {:>8} {:>10}  {}",
            p.pattern_id,
            p.status.as_str(),
            p.session_count,
            p.exemplar_count,
            if tags.is_empty() {
                "(none)".to_string()
            } else {
                tags
            },
        );
    }
    Ok(())
}

fn cmd_show(db: &Database, pattern_id: &str, json: bool) -> Result<()> {
    let pattern = get_pattern(db.conn(), pattern_id)
        .context("failed to fetch pattern")?
        .with_context(|| format!("pattern not found: {pattern_id}"))?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&pattern).context("failed to serialize pattern")?
        );
        return Ok(());
    }

    println!("Pattern ID:     {}", pattern.pattern_id);
    println!("Status:         {}", pattern.status.as_str());
    println!("Session count:  {}", pattern.session_count);
    println!("Exemplar count: {}", pattern.exemplar_count);
    println!(
        "Model ID:       {}",
        pattern.model_id.as_deref().unwrap_or("(unknown)")
    );
    println!(
        "Project ID:     {}",
        pattern.project_id.as_deref().unwrap_or("(all)")
    );
    println!("First seen:     {}", format_epoch_ms(pattern.first_seen_at));
    println!("Updated:        {}", format_epoch_ms(pattern.updated_at));
    println!("Topic tags:     {}", pattern.topic_tags.join(", "));
    println!("Signature dim:  {}", pattern.signature.len());
    println!("\nExemplar drawer IDs:");
    for id in &pattern.exemplar_ids {
        println!("  {}", id);
    }
    println!("\nSession IDs:");
    for id in &pattern.session_ids {
        println!("  {}", id);
    }
    Ok(())
}

fn cmd_retire(db: &Database, pattern_id: &str) -> Result<()> {
    let found = retire_pattern(db.conn(), pattern_id)
        .with_context(|| format!("failed to retire pattern {pattern_id}"))?;
    if found {
        println!("pattern {pattern_id} retired");
    } else {
        println!("pattern {pattern_id} not found");
    }
    Ok(())
}

fn cmd_promote(db: &Database, pattern_id: &str) -> Result<()> {
    let promoted = promote_pattern(db.conn(), pattern_id)
        .with_context(|| format!("failed to promote pattern {pattern_id}"))?;
    if promoted {
        println!("pattern {pattern_id} promoted to active");
    } else {
        println!("pattern {pattern_id} not found or not in candidate status");
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
            // Simple ISO-like format without external deps
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
    // Approximate: days since 1970-01-01
    let y400 = days / 146097;
    let d1 = days % 146097;
    let y100 = d1 / 36524;
    let d2 = d1 % 36524;
    let y4 = d2 / 1461;
    let d3 = d2 % 1461;
    let y1 = d3 / 365;
    let yd = d3 % 365;
    let year = y400 * 400 + y100 * 100 + y4 * 4 + y1 + 1970;
    // Rough month/day from day-of-year
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
