use std::env;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::core::config::{Config, ConfigHandle};
use crate::core::db::{Database, read_fork_ext_version};
use crate::core::project::{ProjectSearchScope, resolve_project_id};
use crate::cowork::peek::format_rfc3339;
use anyhow::{Context, Result, bail};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const FOLLOW_POLL_SECS: u64 = 2;
const FOLLOW_DEBOUNCE_MS: u64 = 250;
const FOLLOW_SILENCE_TIMEOUT_MS: u64 = 3_000;
const PREVIEW_CHARS: usize = 120;

pub struct TailOptions<'a> {
    pub limit: usize,
    pub follow: bool,
    pub wing: Option<&'a str>,
    pub room: Option<&'a str>,
    pub since: Option<&'a str>,
    pub raw: bool,
}

pub struct TimelineOptions<'a> {
    pub wing: Option<&'a str>,
    pub since: Option<&'a str>,
    pub format: &'a str,
    pub raw: bool,
}

pub struct StatsOptions {
    pub raw: bool,
}

pub struct ViewOptions<'a> {
    pub drawer_id: &'a str,
    pub raw: bool,
}

pub struct AuditOptions<'a> {
    pub kind: Option<&'a str>,
    pub since: Option<&'a str>,
    pub raw: bool,
}

pub struct GatingStatsOptions<'a> {
    pub since: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailFollowEvent {
    Notify,
    Tick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailFollowWake {
    Notify,
    Tick,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailFollowBatch {
    pub wake: TailFollowWake,
    pub lines: Vec<String>,
    pub last_seen_rowid: i64,
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
    candidate_drawer_id: String,
    decision: String,
    created_at: i64,
    similarity_score: Option<f32>,
    near_drawer_id: Option<String>,
    project_id: Option<String>,
}

struct GatingAuditDetail {
    audit_id: String,
    candidate_hash: String,
    decision: String,
    tier: u8,
    label: Option<String>,
    reason: Option<String>,
    created_at: i64,
    project_id: Option<String>,
}

struct GatingExplainFields {
    tier: Option<u8>,
    label: Option<String>,
    reason: Option<String>,
}

#[derive(Default)]
pub struct GatingStatsSummary {
    pub total: usize,
    pub kept: usize,
    pub skipped: usize,
    pub tier1_kept: usize,
    pub tier1_skipped: usize,
    pub tier2_kept: usize,
    pub tier2_skipped: usize,
    pub unclassified: usize,
    pub kept_by_label: std::collections::BTreeMap<String, usize>,
    pub skipped_by_reason: std::collections::BTreeMap<String, usize>,
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
                println!("{}", format_tail_line(record, options.raw));
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
            options.raw,
        );
    }

    for record in records.into_iter().take(options.limit) {
        println!("{}", format_tail_line(&record, options.raw));
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
        format_day(&a.added_at)
            .cmp(&format_day(&b.added_at))
            .then_with(|| b.importance.cmp(&a.importance))
            .then_with(|| a.added_at.cmp(&b.added_at))
            .then_with(|| a.id.cmp(&b.id))
    });

    match options.format {
        "json" | "ndjson" => {
            for record in records {
                let signals = crate::aaak::analyze(visible_content(&record.wing, &record.content));
                let line = TimelineJsonLine {
                    timestamp: format_timestamp(&record.added_at),
                    drawer_id: record.id,
                    wing: maybe_escape(&record.wing, options.raw),
                    room: maybe_escape(render_room(record.room.as_deref()), options.raw),
                    importance_stars: record.importance,
                    flags: signals
                        .flags
                        .into_iter()
                        .map(|value| maybe_escape(&value, options.raw))
                        .collect(),
                    preview: maybe_escape(
                        &preview(&record.wing, &record.content, options.raw),
                        options.raw,
                    ),
                };
                println!(
                    "{}",
                    serde_json::to_string(&line).context("failed to serialize timeline row")?
                );
            }
            return Ok(());
        }
        "text" => {}
        other => bail!("unsupported timeline format: {other}"),
    }

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
            maybe_escape(&record.wing, options.raw),
            maybe_escape(render_room(record.room.as_deref()), options.raw),
            maybe_escape(&record.id, options.raw)
        );
        println!(
            "  {}",
            maybe_escape(
                &preview(&record.wing, &record.content, options.raw),
                options.raw,
            )
        );
    }
    Ok(())
}

pub fn stats_command(db: &Database, config: &Config, options: StatsOptions) -> Result<()> {
    let scope = dashboard_scope(config)?;
    let records = load_visible_drawers(db, &scope, None, None, None)?;
    let schema_version = db
        .schema_version()
        .context("failed to read schema version")?;
    let fork_ext_version =
        read_fork_ext_version(db.conn()).context("failed to read fork_ext_version")?;
    let mut by_scope = std::collections::BTreeMap::<String, usize>::new();
    let mut by_project = std::collections::BTreeMap::<Option<String>, usize>::new();
    let mut importance_total = 0i64;
    for record in &records {
        let key = format!("{}/{}", record.wing, render_room(record.room.as_deref()));
        *by_scope.entry(key).or_default() += 1;
        *by_project.entry(record.project_id.clone()).or_default() += 1;
        importance_total += i64::from(record.importance);
    }
    let avg_importance = if records.is_empty() {
        0.0
    } else {
        importance_total as f64 / records.len() as f64
    };

    let queue_stats = query_queue_stats(db).context("failed to query queue stats")?;
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
        println!("  {}: {count}", maybe_escape(&scope_key, options.raw));
    }
    println!("drawers by project:");
    for (project_id, count) in by_project {
        match project_id {
            Some(project_id) => println!("  {}: {count}", maybe_escape(&project_id, options.raw)),
            None => println!("  NULL: {count}"),
        }
    }
    println!("queue:");
    println!("  pending: {}", queue_stats.pending);
    println!("  claimed: {}", queue_stats.claimed);
    println!("  failed: {}", queue_stats.failed);
    println!("  heartbeat: {}", queue_stats.heartbeat_state);
    println!(
        "  heartbeat_age_secs: {}",
        queue_stats
            .heartbeat_age_secs
            .map_or_else(|| "none".to_string(), |value| value.to_string())
    );
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
    println!("drawer_id: {}", maybe_escape(&drawer.id, options.raw));
    println!(
        "scope: {}/{}",
        maybe_escape(&drawer.wing, options.raw),
        maybe_escape(render_room(drawer.room.as_deref()), options.raw)
    );
    println!("created_at: {}", format_timestamp(&drawer.added_at));
    println!(
        "updated_at: {}",
        details.updated_at.as_deref().unwrap_or("none")
    );
    println!("merge_count: {}", details.merge_count);
    println!(
        "source_file: {}",
        maybe_escape(
            drawer.source_file.as_deref().unwrap_or("(synthetic)"),
            options.raw
        )
    );
    println!("content_truncated: false");
    println!("original_content_bytes: {}", drawer.content.len());
    println!();
    if options.raw {
        println!("{}", drawer.content);
    } else {
        let display_content = visible_content(&drawer.wing, &drawer.content);
        let signals = crate::aaak::analyze(display_content);
        println!(
            "flags: {}",
            signals
                .flags
                .iter()
                .map(|value| maybe_escape(value, false))
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!(
            "entities: {}",
            signals
                .entities
                .iter()
                .map(|value| maybe_escape(value, false))
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!(
            "topics: {}",
            signals
                .topics
                .iter()
                .map(|value| maybe_escape(value, false))
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!();
        println!("{}", maybe_escape(display_content, false));
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
                options.raw,
            );
            print_audit_section(
                "novelty",
                &load_audit_records(db, "novelty", &scope, since_cutoff)?,
                options.raw,
            );
        }
        "gating" => {
            print_audit_section(
                "gating",
                &load_audit_records(db, "gating", &scope, since_cutoff)?,
                options.raw,
            );
        }
        "novelty" => {
            print_audit_section(
                "novelty",
                &load_audit_records(db, "novelty", &scope, since_cutoff)?,
                options.raw,
            );
        }
        other => bail!("unsupported audit kind: {other}"),
    }
    Ok(())
}

pub fn gating_stats(
    db: &Database,
    config: &Config,
    since: Option<&str>,
) -> Result<GatingStatsSummary> {
    let scope = dashboard_scope(config)?;
    let since_cutoff = parse_since_cutoff(since)?;
    let rows = load_gating_audit_details(db, &scope, since_cutoff)?;
    Ok(summarize_gating_rows(&rows))
}

pub fn gating_stats_command(
    db: &Database,
    config: &Config,
    options: GatingStatsOptions<'_>,
) -> Result<()> {
    let summary = gating_stats(db, config, options.since)?;
    print_gating_stats(&summary, options.since);
    Ok(())
}

pub fn print_empty_gating_stats(since: Option<&str>) {
    print_gating_stats(&GatingStatsSummary::default(), since);
}

fn follow_tail(
    db: &Database,
    scope: &ProjectSearchScope,
    wing: Option<&str>,
    room: Option<&str>,
    since_cutoff: Option<i64>,
    mut last_seen_rowid: i64,
    raw: bool,
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
    let (tx, mut rx) = unbounded_channel::<TailFollowEvent>();
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
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .context("failed to build tail follow runtime")?;

    loop {
        if watcher.is_some() {
            let batch = runtime.block_on(collect_tail_follow_batch(
                db,
                scope,
                TailFollowFilters {
                    wing,
                    room,
                    since_cutoff,
                    raw,
                },
                last_seen_rowid,
                &mut rx,
            ))?;
            if batch.lines.is_empty() {
                continue;
            }
            for line in &batch.lines {
                println!("{line}");
            }
            last_seen_rowid = batch.last_seen_rowid;
        } else {
            std::thread::sleep(Duration::from_secs(FOLLOW_POLL_SECS));
            let rows = load_visible_drawers_after_rowid(
                db,
                scope,
                wing,
                room,
                since_cutoff,
                last_seen_rowid,
            )?;
            if rows.is_empty() {
                continue;
            }
            for row in &rows {
                println!("{}", format_tail_line(row, raw));
                last_seen_rowid = row.rowid;
            }
        }
        io::stdout()
            .flush()
            .context("failed to flush tail follow output")?;
    }
}

fn create_db_watcher(
    tx: UnboundedSender<TailFollowEvent>,
    db_file_name: &std::ffi::OsStr,
) -> Option<RecommendedWatcher> {
    let base = db_file_name.to_string_lossy().to_string();
    notify::recommended_watcher(move |result: notify::Result<Event>| match result {
        Ok(event) if is_db_event(&event, &base) => {
            let _ = tx.send(TailFollowEvent::Notify);
        }
        Ok(_) => {}
        Err(_) => {
            let _ = tx.send(TailFollowEvent::Notify);
        }
    })
    .ok()
}

#[derive(Clone, Copy)]
pub struct TailFollowFilters<'a> {
    pub wing: Option<&'a str>,
    pub room: Option<&'a str>,
    pub since_cutoff: Option<i64>,
    pub raw: bool,
}

pub async fn next_tail_follow_event(
    rx: &mut UnboundedReceiver<TailFollowEvent>,
) -> TailFollowEvent {
    match tokio::time::timeout(Duration::from_millis(FOLLOW_SILENCE_TIMEOUT_MS), rx.recv()).await {
        Ok(Some(TailFollowEvent::Notify)) => {
            tokio::time::sleep(Duration::from_millis(FOLLOW_DEBOUNCE_MS)).await;
            while rx.try_recv().is_ok() {}
            TailFollowEvent::Notify
        }
        Ok(Some(TailFollowEvent::Tick)) => TailFollowEvent::Tick,
        Ok(None) | Err(_) => TailFollowEvent::Tick,
    }
}

pub async fn collect_tail_follow_batch(
    db: &Database,
    scope: &ProjectSearchScope,
    filters: TailFollowFilters<'_>,
    last_seen_rowid: i64,
    rx: &mut UnboundedReceiver<TailFollowEvent>,
) -> Result<TailFollowBatch> {
    let wake = match next_tail_follow_event(rx).await {
        TailFollowEvent::Notify => TailFollowWake::Notify,
        TailFollowEvent::Tick => TailFollowWake::Tick,
    };
    let rows = load_visible_drawers_after_rowid(
        db,
        scope,
        filters.wing,
        filters.room,
        filters.since_cutoff,
        last_seen_rowid,
    )?;
    let next_rowid = rows.last().map(|row| row.rowid).unwrap_or(last_seen_rowid);
    let lines = rows
        .iter()
        .map(|row| format_tail_line(row, filters.raw))
        .collect();
    Ok(TailFollowBatch {
        wake,
        lines,
        last_seen_rowid: next_rowid,
    })
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
    if kind == "gating" {
        return Ok(load_gating_audit_details(db, scope, since_cutoff)?
            .into_iter()
            .map(|row| AuditRecord {
                audit_id: row.audit_id,
                candidate_drawer_id: row.candidate_hash,
                decision: row.decision,
                created_at: row.created_at,
                similarity_score: None,
                near_drawer_id: None,
                project_id: row.project_id,
            })
            .collect());
    }

    let sql = match kind {
        "novelty" => {
            r#"
            SELECT a.id, a.candidate_hash, a.decision, a.created_at,
                   a.cosine, a.near_drawer_id,
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
                candidate_drawer_id: row.get::<_, String>(1)?,
                decision: row.get::<_, String>(2)?,
                created_at: row.get::<_, i64>(3)?,
                similarity_score: row.get::<_, Option<f32>>(4)?,
                near_drawer_id: row.get::<_, Option<String>>(5)?,
                project_id: row.get::<_, Option<String>>(6)?,
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

fn load_gating_audit_details(
    db: &Database,
    scope: &ProjectSearchScope,
    since_cutoff: Option<i64>,
) -> Result<Vec<GatingAuditDetail>> {
    if db.conn().query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='gating_audit'",
        [],
        |row| row.get::<_, i64>(0),
    )? == 0
    {
        return Ok(Vec::new());
    }

    let columns = table_columns(db, "gating_audit")?;
    let project_expr = if columns.iter().any(|name| name == "project_id") {
        "COALESCE(a.project_id, d.project_id)"
    } else {
        "d.project_id"
    };
    let modern = ["tier", "label", "reason", "drawer_id"]
        .into_iter()
        .all(|column| columns.iter().any(|name| name == column));

    let rows = if modern {
        let sql = format!(
            r#"
            SELECT a.id, a.candidate_hash, a.decision, a.tier, a.label, a.reason,
                   a.explain_json, a.created_at, {project_expr}
            FROM gating_audit a
            LEFT JOIN drawers d ON d.id = COALESCE(a.drawer_id, a.candidate_hash)
            ORDER BY a.created_at DESC, a.id DESC
            "#
        );
        let mut stmt = db.conn().prepare(&sql)?;
        stmt.query_map([], |row| {
            let mut tier = row.get::<_, i64>(3)?.clamp(0, i64::from(u8::MAX)) as u8;
            let mut label = row.get::<_, Option<String>>(4)?;
            let mut reason = row.get::<_, Option<String>>(5)?;
            let explain_json = row.get::<_, Option<String>>(6)?;
            if tier == 0 && label.is_none() && reason.is_none() {
                if let Some(parsed) = explain_json
                    .as_deref()
                    .and_then(parse_gating_explain_fields)
                {
                    tier = parsed.tier.unwrap_or(tier);
                    label = parsed.label.or(label);
                    reason = parsed.reason.or(reason);
                }
            }
            Ok(GatingAuditDetail {
                audit_id: row.get::<_, String>(0)?,
                candidate_hash: row.get::<_, String>(1)?,
                decision: canonical_gating_decision(row.get::<_, String>(2)?),
                tier,
                label,
                reason,
                created_at: row.get::<_, i64>(7)?,
                project_id: row.get::<_, Option<String>>(8)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        let sql = format!(
            r#"
            SELECT a.id, a.candidate_hash, a.decision, a.explain_json, a.created_at, {project_expr}
            FROM gating_audit a
            LEFT JOIN drawers d ON d.id = a.candidate_hash
            ORDER BY a.created_at DESC, a.id DESC
            "#
        );
        let mut stmt = db.conn().prepare(&sql)?;
        stmt.query_map([], |row| {
            let explain_json = row.get::<_, String>(3)?;
            let parsed = parse_gating_explain_fields(&explain_json);
            Ok(GatingAuditDetail {
                audit_id: row.get::<_, String>(0)?,
                candidate_hash: row.get::<_, String>(1)?,
                decision: canonical_gating_decision(row.get::<_, String>(2)?),
                tier: parsed
                    .as_ref()
                    .and_then(|value| value.tier)
                    .unwrap_or_default(),
                label: parsed.as_ref().and_then(|value| value.label.clone()),
                reason: parsed.and_then(|value| value.reason),
                created_at: row.get::<_, i64>(4)?,
                project_id: row.get::<_, Option<String>>(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    };

    Ok(rows
        .into_iter()
        .filter(|row| {
            scope.allows_row(row.project_id.as_deref())
                && since_cutoff.is_none_or(|cutoff| row.created_at >= cutoff)
        })
        .collect())
}

fn parse_gating_explain_fields(raw: &str) -> Option<GatingExplainFields> {
    let parsed = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    Some(GatingExplainFields {
        tier: parsed
            .get("tier")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value.min(u64::from(u8::MAX)) as u8),
        label: parsed
            .get("label")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        reason: parsed
            .get("reason")
            .or_else(|| parsed.get("gating_reason"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
    })
}

fn summarize_gating_rows(rows: &[GatingAuditDetail]) -> GatingStatsSummary {
    let mut summary = GatingStatsSummary {
        total: rows.len(),
        ..GatingStatsSummary::default()
    };
    for row in rows {
        match (row.decision.as_str(), row.tier) {
            ("keep", 1) => {
                summary.kept += 1;
                summary.tier1_kept += 1;
            }
            ("skip", 1) => {
                summary.skipped += 1;
                summary.tier1_skipped += 1;
            }
            ("keep", 2) => {
                summary.kept += 1;
                summary.tier2_kept += 1;
            }
            ("skip", 2) => {
                summary.skipped += 1;
                summary.tier2_skipped += 1;
            }
            ("keep", _) => {
                summary.kept += 1;
                summary.unclassified += 1;
            }
            ("skip", _) => {
                summary.skipped += 1;
                summary.unclassified += 1;
            }
            _ => {
                summary.unclassified += 1;
            }
        }
        if row.decision == "keep" {
            let key = row.label.clone().unwrap_or_else(|| "unlabeled".to_string());
            *summary.kept_by_label.entry(key).or_default() += 1;
        } else if row.decision == "skip" {
            let key = row.reason.clone().unwrap_or_else(|| "unknown".to_string());
            *summary.skipped_by_reason.entry(key).or_default() += 1;
        }
    }
    summary
}

fn render_breakdown(values: &std::collections::BTreeMap<String, usize>) -> String {
    if values.is_empty() {
        return "none".to_string();
    }
    values
        .iter()
        .map(|(key, count)| format!("{key}={count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn print_gating_stats(summary: &GatingStatsSummary, since: Option<&str>) {
    println!("Gating stats:");
    if let Some(since) = since {
        println!("  window: last {since}");
    } else {
        println!("  window: all-time");
        println!("  all_time_total: {}", summary.total);
    }
    println!("  kept: {}", summary.kept);
    println!("  skipped: {}", summary.skipped);
    println!("  tier1_kept: {}", summary.tier1_kept);
    println!("  tier1_skipped: {}", summary.tier1_skipped);
    println!("  tier2_kept: {}", summary.tier2_kept);
    println!("  tier2_skipped: {}", summary.tier2_skipped);
    println!("  unclassified: {}", summary.unclassified);
    println!(
        "  kept_by_label: {}",
        render_breakdown(&summary.kept_by_label)
    );
    println!(
        "  skipped_by_reason: {}",
        render_breakdown(&summary.skipped_by_reason)
    );
}

fn canonical_gating_decision(raw: String) -> String {
    match raw.as_str() {
        "accepted" => "keep".to_string(),
        "rejected" => "skip".to_string(),
        other => other.to_string(),
    }
}

fn table_columns(db: &Database, table: &str) -> Result<Vec<String>> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = db.conn().prepare(&pragma)?;
    Ok(stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?)
}

fn print_audit_section(kind: &str, rows: &[AuditRecord], raw: bool) {
    println!("{kind}:");
    if rows.is_empty() {
        println!("  none");
        return;
    }
    for row in rows {
        if kind == "novelty" {
            let similarity = row
                .similarity_score
                .map(|value| format!("{value:.3}"))
                .unwrap_or_else(|| "none".to_string());
            let near_drawer_id = row
                .near_drawer_id
                .as_deref()
                .map(|value| maybe_escape(value, raw))
                .unwrap_or_else(|| "none".to_string());
            println!(
                "  {} decision={} candidate_drawer_id={} similarity_score={} near_drawer_id={} audit_id={}",
                format_created_at(row.created_at),
                row.decision,
                maybe_escape(&row.candidate_drawer_id, raw),
                similarity,
                near_drawer_id,
                maybe_escape(&row.audit_id, raw)
            );
        } else {
            println!(
                "  {} decision={} candidate_drawer_id={} audit_id={}",
                format_created_at(row.created_at),
                row.decision,
                maybe_escape(&row.candidate_drawer_id, raw),
                maybe_escape(&row.audit_id, raw)
            );
        }
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

fn format_tail_line(record: &DrawerRecord, raw: bool) -> String {
    format!(
        "{} {}/{} {} {}",
        format_time(&record.added_at),
        maybe_escape(&record.wing, raw),
        maybe_escape(render_room(record.room.as_deref()), raw),
        maybe_escape(&record.id, raw),
        maybe_escape(&preview(&record.wing, &record.content, raw), raw)
    )
}

fn preview(wing: &str, content: &str, raw: bool) -> String {
    let content = if raw {
        content
    } else {
        visible_content(wing, content)
    };
    crate::search::preview::truncate(content, PREVIEW_CHARS).content
}

fn visible_content<'a>(wing: &str, content: &'a str) -> &'a str {
    let content = crate::session_review::analysis_content(content);
    if wing == "hooks-raw" {
        crate::session_review::split_hooks_raw_metadata(content).0
    } else {
        content
    }
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

#[derive(Debug, Clone, Serialize)]
struct TimelineJsonLine {
    timestamp: String,
    drawer_id: String,
    wing: String,
    room: String,
    importance_stars: i32,
    flags: Vec<String>,
    preview: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DashboardQueueStats {
    pending: i64,
    claimed: i64,
    failed: i64,
    heartbeat_age_secs: Option<i64>,
    heartbeat_state: &'static str,
}

fn query_queue_stats(db: &Database) -> Result<DashboardQueueStats> {
    let has_pending_messages = db.conn().query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='pending_messages'",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if has_pending_messages == 0 {
        return Ok(DashboardQueueStats {
            pending: 0,
            claimed: 0,
            failed: 0,
            heartbeat_age_secs: None,
            heartbeat_state: "none",
        });
    }

    let pending = db.conn().query_row(
        "SELECT COUNT(*) FROM pending_messages WHERE status = 'pending'",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    let claimed = db.conn().query_row(
        "SELECT COUNT(*) FROM pending_messages WHERE status = 'claimed'",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    let failed = db.conn().query_row(
        "SELECT COUNT(*) FROM pending_messages WHERE status = 'failed'",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    let last_heartbeat = db.conn().query_row(
        "SELECT MAX(heartbeat_at) FROM pending_messages WHERE heartbeat_at IS NOT NULL",
        [],
        |row| row.get::<_, Option<i64>>(0),
    )?;
    let heartbeat_age_secs =
        last_heartbeat.map(|heartbeat| now_unix_secs().saturating_sub(heartbeat));
    let heartbeat_state = match heartbeat_age_secs {
        Some(age) if age <= 120 => "healthy",
        Some(_) => "stale",
        None => "none",
    };
    Ok(DashboardQueueStats {
        pending,
        claimed,
        failed,
        heartbeat_age_secs,
        heartbeat_state,
    })
}

fn maybe_escape(value: &str, raw: bool) -> String {
    if raw {
        value.to_string()
    } else {
        escape_terminal_text(value)
    }
}

pub fn escape_terminal_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_control() {
            escaped.extend(ch.escape_default());
        } else {
            escaped.push(ch);
        }
    }
    escaped
}
