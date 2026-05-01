#![warn(clippy::all)]

use anyhow::{Context, Result};
use clap::Subcommand;

use mempal::core::{config::Config, db::Database};
use mempal::repair::{RepairPackage, assemble_repair_package, detect_repeated_failures};

#[derive(Debug, Clone, Subcommand)]
pub enum RepairCommands {
    /// List detected anti-patterns.
    List {
        /// Filter to a specific wing.
        #[arg(long)]
        wing: Option<String>,
        /// Look back this many days (default: from config).
        #[arg(long)]
        since: Option<u64>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show full detail for a single anti-pattern by topic_sig.
    Show {
        /// topic_sig (32 hex chars).
        topic_sig: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
}

pub fn run_command(config: &Config, command: RepairCommands) -> Result<()> {
    let db_path = mempal::core::utils::expand_home(&config.db_path);
    let db = Database::open(std::path::Path::new(&db_path))
        .with_context(|| format!("failed to open database at {}", db_path.display()))?;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    match command {
        RepairCommands::List { wing, since, json } => {
            cmd_list(&db, config, wing.as_deref(), since, json, now_ms)
        }
        RepairCommands::Show { topic_sig, json } => cmd_show(&db, config, &topic_sig, json, now_ms),
    }
}

fn cmd_list(
    db: &Database,
    config: &Config,
    wing: Option<&str>,
    since_override: Option<u64>,
    json: bool,
    now_ms: i64,
) -> Result<()> {
    let mut repair_cfg = config.repair.clone();
    if let Some(days) = since_override {
        repair_cfg.window_days = days;
    }

    let mut packages = detect_repeated_failures(db.conn(), &repair_cfg, None, now_ms);

    if let Some(w) = wing {
        // Filter by wing: check if any failure drawer comes from that wing.
        // We filter by fetching wing metadata from failure_events.
        packages = filter_packages_by_wing(db, packages, w, now_ms, repair_cfg.window_days);
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&packages).context("serialize repair packages")?
        );
        return Ok(());
    }

    if packages.is_empty() {
        println!("no anti-patterns detected");
        return Ok(());
    }

    for pkg in &packages {
        println!(
            "topic_sig={} failure_count={} window_days={}",
            pkg.topic_sig, pkg.failure_count, pkg.window_days
        );
        if !pkg.failure_drawers.is_empty() {
            println!("  failure drawers:");
            for d in pkg.failure_drawers.iter().take(3) {
                let preview: String = d.preview.chars().take(80).collect();
                println!("    [{}] {}", d.drawer_id, preview);
            }
        }
        if !pkg.success_drawers.is_empty() {
            println!("  success drawers (contrast):");
            for d in pkg.success_drawers.iter().take(3) {
                let preview: String = d.preview.chars().take(80).collect();
                println!("    [{}] {}", d.drawer_id, preview);
            }
        }
        println!();
    }
    Ok(())
}

fn cmd_show(
    db: &Database,
    config: &Config,
    topic_sig: &str,
    json: bool,
    now_ms: i64,
) -> Result<()> {
    let repair_cfg = &config.repair;
    let window_start_ms = now_ms - (repair_cfg.window_days as i64) * 86_400_000;

    // Count failures for this sig.
    let failure_count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM failure_events WHERE topic_sig = ?1 AND detected_at >= ?2",
            rusqlite::params![topic_sig, window_start_ms],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let pkg = assemble_repair_package(
        db.conn(),
        topic_sig,
        failure_count as usize,
        repair_cfg.window_days,
        window_start_ms,
    );

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&pkg).context("serialize repair package")?
        );
        return Ok(());
    }

    println!("topic_sig:     {}", pkg.topic_sig);
    println!("failure_count: {}", pkg.failure_count);
    println!("window_days:   {}", pkg.window_days);
    println!();

    if pkg.failure_drawers.is_empty() {
        println!("failure drawers: (none)");
    } else {
        println!("failure drawers:");
        for d in &pkg.failure_drawers {
            println!("  [{}]", d.drawer_id);
            println!("    {}", d.preview);
            println!();
        }
    }

    if pkg.success_drawers.is_empty() {
        println!("success drawers (contrast): (none found)");
    } else {
        println!("success drawers (contrast):");
        for d in &pkg.success_drawers {
            println!("  [{}]", d.drawer_id);
            println!("    {}", d.preview);
            println!();
        }
    }
    Ok(())
}

/// Filter packages to only include those with at least one failure drawer
/// from the specified wing (via a direct DB query).
fn filter_packages_by_wing(
    db: &Database,
    packages: Vec<RepairPackage>,
    wing: &str,
    now_ms: i64,
    window_days: u64,
) -> Vec<RepairPackage> {
    let window_start_ms = now_ms - (window_days as i64) * 86_400_000;
    packages
        .into_iter()
        .filter(|pkg| {
            db.conn()
                .query_row(
                    "SELECT COUNT(*) FROM failure_events WHERE topic_sig = ?1 AND wing = ?2 AND detected_at >= ?3",
                    rusqlite::params![pkg.topic_sig, wing, window_start_ms],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0
        })
        .collect()
}
