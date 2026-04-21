use std::env;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::core::project::{ProjectSearchScope, resolve_project_id};
use mempal::core::queue::PendingMessageStore;
use mempal::cowork::peek::format_rfc3339;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

const FOLLOW_POLL_SECS: u64 = 2;
const FOLLOW_DEBOUNCE_MS: u64 = 250;
const PREVIEW_CHARS: usize = 120;

pub struct TailOptions<'a> {
    pub limit: usize,
    pub follow: bool,
    pub wing: Option<&'a str>,
    pub room: Option<&'a str>,
    pub since: Option<&'a str>,
}

pub struct TimelineOptions<'a> {
    pub wing: Option<&'a str>,
    pub since: Option<&'a str>,
}

pub struct ViewOptions<'a> {
    pub drawer_id: &'a str,
    pub raw: bool,
}

pub struct AuditOptions<'a> {
    pub kind: Option<&'a str>,
    pub since: Option<&'a str>,
}

#[derive(Clone)]
struct DrawerRecord {
    rowid: i64,
    id: String,
    content: String,
    wing: String,
    room: Option<String>,
    added_at: String,
    importance: i32,
    project_id: Option<String>,
}

struct AuditRecord {
    audit_id: String,
    candidate_hash: String,
    decision: String,
    created_at: i64,
    project_id: Option<String>,
}

pub fn tail_command(db: &Database, config: &Config, options: TailOptions<'_>) -> Result<()> {
    let scope = dashboard_scope(config)?;
    let since_cutoff = parse_since_cutoff(options.since)?;
    let mut records = load_visible_drawers(db, &scope, options.wing, options.room, since_cutoff)?;
    records.sort_by(|a, b| b.added_at.cmp(&a.added_at).then_with(|| b.id.cmp(&a.id)));

    if options.follow {
        let limit = options.limit;
        if limit > 0 {
            for record in records.iter().take(limit) {
                println!("{}", format_tail_line(record));
            }
            io::stdout()
                .flush()
                .context("failed to flush tail output")?;
        }
        let last_seen_rowid = records.iter().map(|record| record.rowid).max().unwrap_or(0);
        return follow_tail(
            db,
            &scope,
            options.wing,
            options.room,
            since_cutoff,
            last_seen_rowid,
        );
    }

    for record in records.into_iter().take(options.limit) {
        println!("{}", format_tail_line(&record));
    }
    Ok(())
}

pub fn timeline_command(
    db: &Database,
    config: &Config,
    options: TimelineOptions<'_>,
) -> Result<()> {
    let scope = dashboard_scope(config)?;
    let since_cutoff = parse_since_cutoff(options.since)?;
    let mut records = load_visible_drawers(db, &scope, options.wing, None, since_cutoff)?;
    records.sort_by(|a, b| {
        a.added_at
            .cmp(&b.added_at)
            .then_with(|| b.importance.cmp(&a.importance))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut current_day = None::<String>;
    for record in records {
        let day = format_day(&record.added_at);
        if current_day.as_deref() != Some(day.as_str()) {
            if current_day.is_some() {
                println!();
            }
            println!("=== {day} ===");
            current_day = Some(day);
        }
        println!(
            "{} {}/{} {}",
            format_time(&record.added_at),
            record.wing,
            render_room(record.room.as_deref()),
            record.id
        );
        println!("  {}", preview(&record.content));
    }
    Ok(())
}

pub fn stats_command(db: &Database, config: &Config) -> Result<()> {
    let scope = dashboard_scope(config)?;
    let records = load_visible_drawers(db, &scope, None, None, None)?;
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
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let mut by_scope = std::collections::BTreeMap::<String, usize>::new();
    let mut importance_total = 0i64;
    for record in &records {
        let key = format!("{}/{}", record.wing, render_room(record.room.as_deref()));
        *by_scope.entry(key).or_default() += 1;
        importance_total += i64::from(record.importance);
    }
    let avg_importance = if records.is_empty() {
        0.0
    } else {
        importance_total as f64 / records.len() as f64
    };

    let queue_stats = PendingMessageStore::new(db.path())
        .context("failed to open pending message store")?
        .stats()
        .context("failed to query queue stats")?;
    let gating = load_audit_records(db, "gating", &scope, None)?;
    let novelty = load_audit_records(db, "novelty", &scope, None)?;
    let embed_count = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='drawer_vectors'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .context("failed to inspect vectors table")?;
    let vector_count = if embed_count == 0 {
        0i64
    } else {
        db.conn()
            .query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| {
                row.get::<_, i64>(0)
            })
            .context("failed to count vectors")?
    };
    let vector_dim = if embed_count == 0 {
        None
    } else {
        db.embedding_dim().context("failed to read vector dim")?
    };

    println!("schema_version: {schema_version}");
    println!("fork_ext_version: {fork_ext_version}");
    println!("drawers total: {}", records.len());
    println!("avg importance: {:.2}", avg_importance);
    println!("drawers by scope:");
    for (scope_key, count) in by_scope {
        println!("  {scope_key}: {count}");
    }
    println!("queue:");
    println!("  pending: {}", queue_stats.pending);
    println!("  claimed: {}", queue_stats.claimed);
    println!("  failed: {}", queue_stats.failed);
    println!("gating:");
    print_decision_counts(&gating);
    println!("novelty:");
    print_decision_counts(&novelty);
    println!("privacy scrub:");
    let scrub_stats = ConfigHandle::scrub_stats();
    println!(
        "  total_patterns_matched: {}",
        scrub_stats.total_patterns_matched
    );
    println!("  bytes_redacted: {}", scrub_stats.bytes_redacted);
    println!("vectors:");
    println!("  dim: {:?}", vector_dim);
    println!("  count: {vector_count}");
    Ok(())
}

pub fn view_command(db: &Database, config: &Config, options: ViewOptions<'_>) -> Result<()> {
    let scope = dashboard_scope(config)?;
    let details = db
        .get_drawer_details(options.drawer_id)
        .context("failed to load drawer")?
        .ok_or_else(|| anyhow::anyhow!("drawer {} not found", options.drawer_id))?;
    if !scope.allows_row(details.project_id.as_deref()) {
        bail!(
            "drawer {} is outside the current project scope",
            options.drawer_id
        );
    }

    let drawer = details.drawer;
    println!("drawer_id: {}", drawer.id);
    println!(
        "scope: {}/{}",
        drawer.wing,
        render_room(drawer.room.as_deref())
    );
    println!("created_at: {}", format_timestamp(&drawer.added_at));
    println!(
        "updated_at: {}",
        details.updated_at.as_deref().unwrap_or("none")
    );
    println!("merge_count: {}", details.merge_count);
    println!(
        "source_file: {}",
        drawer.source_file.as_deref().unwrap_or("(synthetic)")
    );
    println!();
    if options.raw {
        println!("{}", drawer.content);
    } else {
        let signals = mempal::aaak::analyze(&drawer.content);
        println!("flags: {}", signals.flags.join(", "));
        println!("entities: {}", signals.entities.join(", "));
        println!("topics: {}", signals.topics.join(", "));
        println!();
        println!("{}", drawer.content);
    }
    Ok(())
}

pub fn audit_command(db: &Database, config: &Config, options: AuditOptions<'_>) -> Result<()> {
    let scope = dashboard_scope(config)?;
    let since_cutoff = parse_since_cutoff(options.since)?;
    match options.kind.unwrap_or("all") {
        "all" => {
            print_audit_section(
                "gating",
                &load_audit_records(db, "gating", &scope, since_cutoff)?,
            );
            print_audit_section(
                "novelty",
                &load_audit_records(db, "novelty", &scope, since_cutoff)?,
            );
        }
        "gating" => {
            print_audit_section(
                "gating",
                &load_audit_records(db, "gating", &scope, since_cutoff)?,
            );
        }
        "novelty" => {
            print_audit_section(
                "novelty",
                &load_audit_records(db, "novelty", &scope, since_cutoff)?,
            );
        }
        other => bail!("unsupported audit kind: {other}"),
    }
    Ok(())
}

fn follow_tail(
    db: &Database,
    scope: &ProjectSearchScope,
    wing: Option<&str>,
    room: Option<&str>,
    since_cutoff: Option<i64>,
    mut last_seen_rowid: i64,
) -> Result<()> {
    let db_path = db.path().to_path_buf();
    let db_file_name = db_path
        .file_name()
        .map(OsStr::to_os_string)
        .unwrap_or_default();
    let watch_dir = db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| ".".into());
    let (tx, rx) = mpsc::channel::<()>();
    let mut watcher = create_db_watcher(tx.clone(), &db_file_name);
    let watcher_active = if let Some(active_watcher) = watcher.as_mut() {
        active_watcher
            .watch(&watch_dir, RecursiveMode::NonRecursive)
            .is_ok()
    } else {
        false
    };
    if !watcher_active {
        watcher = None;
    }

    loop {
        if watcher.is_some() {
            rx.recv().context("tail watcher disconnected")?;
            std::thread::sleep(Duration::from_millis(FOLLOW_DEBOUNCE_MS));
            while rx.try_recv().is_ok() {}
        } else {
            std::thread::sleep(Duration::from_secs(FOLLOW_POLL_SECS));
        }

        let rows =
            load_visible_drawers_after_rowid(db, scope, wing, room, since_cutoff, last_seen_rowid)?;
        if rows.is_empty() {
            continue;
        }
        for row in &rows {
            println!("{}", format_tail_line(row));
            last_seen_rowid = row.rowid;
        }
        io::stdout()
            .flush()
            .context("failed to flush tail follow output")?;
    }
}

fn create_db_watcher(
    tx: mpsc::Sender<()>,
    db_file_name: &std::ffi::OsStr,
) -> Option<RecommendedWatcher> {
    let base = db_file_name.to_string_lossy().to_string();
    notify::recommended_watcher(move |result: notify::Result<Event>| match result {
        Ok(event) if is_db_event(&event, &base) => {
            let _ = tx.send(());
        }
        Ok(_) => {}
        Err(_) => {
            let _ = tx.send(());
        }
    })
    .ok()
}

fn is_db_event(event: &Event, db_file_name: &str) -> bool {
    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any
    ) {
        return false;
    }
    let wal = format!("{db_file_name}-wal");
    let shm = format!("{db_file_name}-shm");
    event.paths.iter().any(|path| {
        path.file_name()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|name| name == db_file_name || name == wal || name == shm)
    })
}

fn dashboard_scope(config: &Config) -> Result<ProjectSearchScope> {
    let current_dir = env::current_dir().ok();
    let project_id = resolve_project_id(None, config, None)
        .context("failed to resolve dashboard project scope")?
        .or_else(|| {
            current_dir
                .as_deref()
                .filter(|cwd| is_git_repo(cwd))
                .and_then(|cwd| resolve_project_id(None, config, Some(cwd)).ok())
                .flatten()
        });
    Ok(ProjectSearchScope::from_request(
        project_id,
        false,
        false,
        config.search.strict_project_isolation,
    ))
}

fn is_git_repo(path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn load_visible_drawers(
    db: &Database,
    scope: &ProjectSearchScope,
    wing: Option<&str>,
    room: Option<&str>,
    since_cutoff: Option<i64>,
) -> Result<Vec<DrawerRecord>> {
    let mut stmt = db.conn().prepare(
        r#"
        SELECT rowid, id, content, wing, room, added_at,
               COALESCE(importance, 0), project_id
        FROM drawers
        WHERE deleted_at IS NULL
        ORDER BY CAST(added_at AS INTEGER) DESC, id DESC
        "#,
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(DrawerRecord {
                rowid: row.get::<_, i64>(0)?,
                id: row.get::<_, String>(1)?,
                content: row.get::<_, String>(2)?,
                wing: row.get::<_, String>(3)?,
                room: row.get::<_, Option<String>>(4)?,
                added_at: row.get::<_, String>(5)?,
                importance: row.get::<_, i32>(6)?,
                project_id: row.get::<_, Option<String>>(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|record| {
            scope.allows_row(record.project_id.as_deref())
                && wing.is_none_or(|expected| record.wing == expected)
                && room.is_none_or(|expected| record.room.as_deref() == Some(expected))
                && since_cutoff.is_none_or(|cutoff| {
                    parse_added_at(&record.added_at).is_some_and(|ts| ts >= cutoff)
                })
        })
        .collect())
}

fn load_visible_drawers_after_rowid(
    db: &Database,
    scope: &ProjectSearchScope,
    wing: Option<&str>,
    room: Option<&str>,
    since_cutoff: Option<i64>,
    last_seen_rowid: i64,
) -> Result<Vec<DrawerRecord>> {
    let mut stmt = db.conn().prepare(
        r#"
        SELECT rowid, id, content, wing, room, added_at,
               COALESCE(importance, 0), project_id
        FROM drawers
        WHERE deleted_at IS NULL AND rowid > ?1
        ORDER BY rowid ASC
        "#,
    )?;
    let rows = stmt
        .query_map([last_seen_rowid], |row| {
            Ok(DrawerRecord {
                rowid: row.get::<_, i64>(0)?,
                id: row.get::<_, String>(1)?,
                content: row.get::<_, String>(2)?,
                wing: row.get::<_, String>(3)?,
                room: row.get::<_, Option<String>>(4)?,
                added_at: row.get::<_, String>(5)?,
                importance: row.get::<_, i32>(6)?,
                project_id: row.get::<_, Option<String>>(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|record| {
            scope.allows_row(record.project_id.as_deref())
                && wing.is_none_or(|expected| record.wing == expected)
                && room.is_none_or(|expected| record.room.as_deref() == Some(expected))
                && since_cutoff.is_none_or(|cutoff| {
                    parse_added_at(&record.added_at).is_some_and(|ts| ts >= cutoff)
                })
        })
        .collect())
}

fn load_audit_records(
    db: &Database,
    kind: &str,
    scope: &ProjectSearchScope,
    since_cutoff: Option<i64>,
) -> Result<Vec<AuditRecord>> {
    let sql = match kind {
        "gating" => {
            r#"
            SELECT a.id, a.candidate_hash, a.decision, a.created_at,
                   COALESCE(a.project_id, d.project_id)
            FROM gating_audit a
            LEFT JOIN drawers d ON d.id = a.candidate_hash
            ORDER BY a.created_at DESC, a.id DESC
            "#
        }
        "novelty" => {
            r#"
            SELECT a.id, a.candidate_hash, a.decision, a.created_at,
                   COALESCE(a.project_id, d.project_id)
            FROM novelty_audit a
            LEFT JOIN drawers d ON d.id = a.candidate_hash
            ORDER BY a.created_at DESC, a.id DESC
            "#
        }
        other => bail!("unsupported audit kind: {other}"),
    };
    let mut stmt = db.conn().prepare(sql)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(AuditRecord {
                audit_id: row.get::<_, String>(0)?,
                candidate_hash: row.get::<_, String>(1)?,
                decision: row.get::<_, String>(2)?,
                created_at: row.get::<_, i64>(3)?,
                project_id: row.get::<_, Option<String>>(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|row| {
            scope.allows_row(row.project_id.as_deref())
                && since_cutoff.is_none_or(|cutoff| row.created_at >= cutoff)
        })
        .collect())
}

fn print_audit_section(kind: &str, rows: &[AuditRecord]) {
    println!("{kind}:");
    if rows.is_empty() {
        println!("  none");
        return;
    }
    for row in rows {
        println!(
            "  {} {} {} {}",
            format_created_at(row.created_at),
            row.candidate_hash,
            row.decision,
            row.audit_id
        );
    }
}

fn print_decision_counts(rows: &[AuditRecord]) {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for row in rows {
        *counts.entry(row.decision.clone()).or_default() += 1;
    }
    if counts.is_empty() {
        println!("  none");
        return;
    }
    for (decision, count) in counts {
        println!("  {decision}: {count}");
    }
}

fn format_tail_line(record: &DrawerRecord) -> String {
    format!(
        "{} {}/{} {} {}",
        format_time(&record.added_at),
        record.wing,
        render_room(record.room.as_deref()),
        record.id,
        preview(&record.content)
    )
}

fn preview(content: &str) -> String {
    mempal::search::preview::truncate(content, PREVIEW_CHARS).content
}

fn render_room(room: Option<&str>) -> &str {
    room.unwrap_or("default")
}

fn parse_since_cutoff(raw: Option<&str>) -> Result<Option<i64>> {
    raw.map(parse_duration_spec)
        .transpose()
        .map(|seconds| seconds.map(|seconds| now_unix_secs() - seconds))
}

fn parse_duration_spec(raw: &str) -> Result<i64> {
    if raw.is_empty() {
        bail!("empty duration");
    }
    let (digits, unit) = raw.split_at(raw.len() - 1);
    let value = digits
        .parse::<i64>()
        .with_context(|| format!("invalid duration: {raw}"))?;
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 60 * 60 * 24,
        other => bail!("unsupported duration unit: {other}"),
    };
    Ok(value * multiplier)
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_secs() as i64
}

fn parse_added_at(value: &str) -> Option<i64> {
    value.parse::<i64>().ok()
}

fn format_day(value: &str) -> String {
    let secs = parse_added_at(value).unwrap_or_default().max(0) as u64;
    format_rfc3339(UNIX_EPOCH + Duration::from_secs(secs))[..10].to_string()
}

fn format_time(value: &str) -> String {
    let secs = parse_added_at(value).unwrap_or_default().max(0) as u64;
    format_rfc3339(UNIX_EPOCH + Duration::from_secs(secs))[11..19].to_string()
}

fn format_timestamp(value: &str) -> String {
    let secs = parse_added_at(value).unwrap_or_default().max(0) as u64;
    format_rfc3339(UNIX_EPOCH + Duration::from_secs(secs))
}

fn format_created_at(value: i64) -> String {
    let secs = value.max(0) as u64;
    format_rfc3339(UNIX_EPOCH + Duration::from_secs(secs))
}
