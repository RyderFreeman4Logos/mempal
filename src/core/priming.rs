use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::params;
use serde::Serialize;
use thiserror::Error;

use crate::core::db::Database;
use crate::core::project::ProjectSearchScope;
use crate::cowork::peek::{format_rfc3339, parse_rfc3339};
use crate::search::preview::truncate;

pub const DEFAULT_PRIME_TOKEN_BUDGET: usize = 2_048;
pub const MIN_PRIME_TOKEN_BUDGET: usize = 512;
pub const MAX_PRIME_TOKEN_BUDGET: usize = 8_192;
pub const DEFAULT_PRIME_SINCE: &str = "30d";
pub const PRIME_PREVIEW_CHARS: usize = 120;
pub const PRIME_LEGEND: &str = "Legend: more stars mean higher importance; format <added_at> <stars> <wing>/<room> <id> — <preview>";

#[derive(Debug, Clone)]
pub struct PrimingRequest {
    pub project_id: Option<String>,
    pub scope: ProjectSearchScope,
    pub since: String,
    pub token_budget: usize,
    pub include_stats: bool,
    pub embedder_degraded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrimingReport {
    pub project_id: Option<String>,
    pub generated_at: String,
    pub legend: String,
    pub drawers: Vec<PrimingDrawer>,
    pub stats: Option<PrimingStats>,
    pub budget_used_tokens: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrimingDrawer {
    pub id: String,
    pub added_at: String,
    pub importance_stars: i32,
    pub wing: String,
    pub room: String,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrimingStats {
    pub total: usize,
    pub recent_7d: usize,
    pub top_wings: Vec<PrimingWingCount>,
    pub embedder_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrimingWingCount {
    pub wing: String,
    pub count: usize,
}

#[derive(Debug, Error)]
pub enum PrimingError {
    #[error("invalid --since value `{value}`")]
    InvalidSince { value: String },
    #[error("failed to prepare priming query")]
    Prepare(#[source] rusqlite::Error),
    #[error("failed to execute priming query")]
    Query(#[source] rusqlite::Error),
}

#[derive(Debug, Clone)]
struct PrimingRow {
    id: String,
    content: String,
    wing: String,
    room: Option<String>,
    added_at: String,
    importance: i32,
}

pub fn build_priming_report(
    db: &Database,
    request: PrimingRequest,
) -> Result<PrimingReport, PrimingError> {
    let now = now_unix_secs();
    let since_cutoff = parse_since_cutoff(&request.since, now)?;
    let scoped_rows = query_scoped_rows(db, &request.scope)?;
    let stats = request
        .include_stats
        .then(|| build_stats(&scoped_rows, request.embedder_degraded, now));

    let filtered_rows = scoped_rows
        .iter()
        .filter(|row| {
            since_cutoff.is_none_or(|cutoff| {
                parse_added_at(&row.added_at).is_some_and(|added_at| added_at >= cutoff)
            })
        })
        .collect::<Vec<_>>();

    if filtered_rows.is_empty() {
        return Ok(PrimingReport {
            project_id: request.project_id,
            generated_at: format_timestamp(now),
            legend: PRIME_LEGEND.to_string(),
            drawers: Vec::new(),
            stats,
            budget_used_tokens: 0,
            truncated: false,
        });
    }

    let mut budget_used_tokens = estimate_tokens(PRIME_LEGEND);
    if let Some(stats) = stats.as_ref() {
        budget_used_tokens += estimate_stats_tokens(stats);
    }

    let mut drawers = Vec::new();
    for row in &filtered_rows {
        let candidate = build_drawer(row, PRIME_PREVIEW_CHARS);
        let candidate_tokens = estimate_drawer_tokens(&candidate);
        if budget_used_tokens + candidate_tokens <= request.token_budget {
            budget_used_tokens += candidate_tokens;
            drawers.push(candidate);
            continue;
        }

        if drawers.is_empty() {
            let fitted =
                fit_drawer_to_budget(row, request.token_budget.saturating_sub(budget_used_tokens));
            budget_used_tokens += estimate_drawer_tokens(&fitted);
            drawers.push(fitted);
        }
        break;
    }

    let truncated = drawers.len() < filtered_rows.len();

    Ok(PrimingReport {
        project_id: request.project_id,
        generated_at: format_timestamp(now),
        legend: PRIME_LEGEND.to_string(),
        drawers,
        stats,
        budget_used_tokens,
        truncated,
    })
}

pub fn format_stars(importance_stars: i32) -> String {
    if importance_stars <= 0 {
        "-".to_string()
    } else {
        "★".repeat(importance_stars.clamp(0, 5) as usize)
    }
}

pub fn format_top_wings(top_wings: &[PrimingWingCount]) -> String {
    if top_wings.is_empty() {
        return "none".to_string();
    }

    top_wings
        .iter()
        .map(|entry| format!("{}({})", entry.wing, entry.count))
        .collect::<Vec<_>>()
        .join(", ")
}

fn query_scoped_rows(
    db: &Database,
    scope: &ProjectSearchScope,
) -> Result<Vec<PrimingRow>, PrimingError> {
    let mut statement = db
        .conn()
        .prepare(
            r#"
            SELECT id, content, wing, room, added_at, COALESCE(importance, 0) AS importance
            FROM drawers
            WHERE deleted_at IS NULL
              AND (
                  ?1 = 'all'
                  OR (?1 = 'project' AND project_id = ?2)
                  OR (?1 = 'project_plus_global' AND (project_id = ?2 OR project_id IS NULL))
                  OR (?1 = 'null_only' AND project_id IS NULL)
              )
            ORDER BY importance DESC, added_at DESC, id DESC
            "#,
        )
        .map_err(PrimingError::Prepare)?;

    statement
        .query_map(
            params![scope.mode_param(), scope.project_id.as_deref()],
            |row| {
                Ok(PrimingRow {
                    id: row.get::<_, String>(0)?,
                    content: row.get::<_, String>(1)?,
                    wing: row.get::<_, String>(2)?,
                    room: row.get::<_, Option<String>>(3)?,
                    added_at: row.get::<_, String>(4)?,
                    importance: row.get::<_, i32>(5)?,
                })
            },
        )
        .map_err(PrimingError::Query)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(PrimingError::Query)
}

fn build_stats(rows: &[PrimingRow], embedder_degraded: bool, now: i64) -> PrimingStats {
    let recent_cutoff = now - 7 * 24 * 60 * 60;
    let recent_7d = rows
        .iter()
        .filter(|row| {
            parse_added_at(&row.added_at).is_some_and(|added_at| added_at >= recent_cutoff)
        })
        .count();

    let mut wing_counts = BTreeMap::<String, usize>::new();
    for row in rows {
        *wing_counts.entry(row.wing.clone()).or_default() += 1;
    }
    let mut top_wings = wing_counts
        .into_iter()
        .map(|(wing, count)| PrimingWingCount { wing, count })
        .collect::<Vec<_>>();
    top_wings.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.wing.cmp(&right.wing))
    });
    top_wings.truncate(3);

    PrimingStats {
        total: rows.len(),
        recent_7d,
        top_wings,
        embedder_status: if embedder_degraded {
            "degraded".to_string()
        } else {
            "healthy".to_string()
        },
    }
}

fn build_drawer(row: &PrimingRow, preview_chars: usize) -> PrimingDrawer {
    let preview = truncate(&row.content, preview_chars.min(PRIME_PREVIEW_CHARS));
    PrimingDrawer {
        id: row.id.clone(),
        added_at: format_display_added_at(&row.added_at),
        importance_stars: row.importance.clamp(0, 5),
        wing: row.wing.clone(),
        room: row.room.clone().unwrap_or_else(|| "default".to_string()),
        preview: preview.content,
    }
}

fn fit_drawer_to_budget(row: &PrimingRow, available_tokens: usize) -> PrimingDrawer {
    let full = build_drawer(row, PRIME_PREVIEW_CHARS);
    if estimate_drawer_tokens(&full) <= available_tokens {
        return full;
    }

    let fixed = PrimingDrawer {
        preview: String::new(),
        ..full.clone()
    };
    let fixed_tokens = estimate_drawer_tokens(&fixed);
    let preview_chars = available_tokens
        .saturating_sub(fixed_tokens)
        .saturating_mul(4)
        .clamp(1, PRIME_PREVIEW_CHARS);

    build_drawer(row, preview_chars)
}

fn estimate_drawer_tokens(drawer: &PrimingDrawer) -> usize {
    estimate_tokens(&format!(
        "{} {} {}/{} {} — {}",
        drawer.added_at,
        format_stars(drawer.importance_stars),
        drawer.wing,
        drawer.room,
        drawer.id,
        drawer.preview
    ))
}

fn estimate_stats_tokens(stats: &PrimingStats) -> usize {
    estimate_tokens(&format!(
        "total={}\nrecent_7d={}\ntop_wings={}\nembedder_status={}",
        stats.total,
        stats.recent_7d,
        format_top_wings(&stats.top_wings),
        stats.embedder_status
    ))
}

fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

fn parse_since_cutoff(raw: &str, now: i64) -> Result<Option<i64>, PrimingError> {
    if raw == "all" {
        return Ok(None);
    }

    let seconds = parse_duration_spec(raw)?;
    Ok(Some(now - seconds))
}

fn parse_duration_spec(raw: &str) -> Result<i64, PrimingError> {
    if raw.is_empty() {
        return Err(PrimingError::InvalidSince {
            value: raw.to_string(),
        });
    }

    let (digits, unit) = raw.split_at(raw.len() - 1);
    let value = digits
        .parse::<i64>()
        .map_err(|_| PrimingError::InvalidSince {
            value: raw.to_string(),
        })?;
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => {
            return Err(PrimingError::InvalidSince {
                value: raw.to_string(),
            });
        }
    };
    Ok(value * multiplier)
}

fn parse_added_at(raw: &str) -> Option<i64> {
    raw.parse::<i64>().ok().or_else(|| parse_rfc3339(raw))
}

fn format_display_added_at(raw: &str) -> String {
    parse_added_at(raw)
        .map(format_timestamp)
        .unwrap_or_else(|| raw.to_string())
}

fn format_timestamp(secs: i64) -> String {
    format_rfc3339(UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64))
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after unix epoch")
        .as_secs() as i64
}
