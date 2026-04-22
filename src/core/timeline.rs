use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::params;
use serde::Serialize;
use thiserror::Error;

use crate::core::db::Database;
use crate::core::project::ProjectSearchScope;
use crate::cowork::peek::{format_rfc3339, parse_rfc3339};
use crate::search::preview::truncate;

pub const DEFAULT_TIMELINE_SINCE: &str = "30d";
pub const DEFAULT_TIMELINE_TOP_K: usize = 20;
pub const MAX_TIMELINE_TOP_K: usize = 100;
pub const TIMELINE_PREVIEW_CHAR_LIMIT: usize = 200;

#[derive(Debug, Clone)]
pub struct TimelineQuery {
    pub project_id: Option<String>,
    pub scope: ProjectSearchScope,
    pub since: String,
    pub until: Option<String>,
    pub top_k: usize,
    pub min_importance: u8,
    pub wing: Option<String>,
    pub room: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimelineReport {
    pub project_id: Option<String>,
    pub generated_at: String,
    pub window: TimelineWindow,
    pub entries: Vec<TimelineEntry>,
    pub stats: TimelineStats,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimelineWindow {
    pub since: String,
    pub until: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimelineEntry {
    pub drawer_id: String,
    pub added_at: String,
    pub importance_stars: u8,
    pub wing: String,
    pub room: Option<String>,
    pub preview: String,
    pub preview_truncated: bool,
    pub original_content_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimelineStats {
    pub total_in_window: u64,
    pub returned: usize,
    pub top_wings: Vec<TimelineWingCount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimelineWingCount {
    pub wing: String,
    pub count: u64,
}

#[derive(Debug, Error)]
pub enum TimelineError {
    #[error("invalid since value `{value}`")]
    InvalidSince { value: String },
    #[error("invalid until value `{value}`")]
    InvalidUntil { value: String },
    #[error("failed to prepare timeline query")]
    Prepare(#[source] rusqlite::Error),
    #[error("failed to execute timeline query")]
    Query(#[source] rusqlite::Error),
}

#[derive(Debug, Clone)]
struct TimelineRow {
    id: String,
    content: String,
    wing: String,
    room: Option<String>,
    added_at: String,
    importance: i32,
}

pub fn build_timeline_report(
    db: &Database,
    request: TimelineQuery,
) -> Result<TimelineReport, TimelineError> {
    let now = now_unix_secs();
    let since = parse_since_cutoff(&request.since, now)?;
    let until = parse_until_cutoff(request.until.as_deref(), now)?;
    let rows = query_scoped_rows(db, &request.scope)?;
    let filtered = rows
        .into_iter()
        .filter(|row| row.importance >= i32::from(request.min_importance))
        .filter(|row| {
            request
                .wing
                .as_deref()
                .is_none_or(|wing| row.wing.as_str() == wing)
        })
        .filter(|row| {
            request
                .room
                .as_deref()
                .is_none_or(|room| row.room.as_deref() == Some(room))
        })
        .filter(|row| {
            parse_added_at(&row.added_at)
                .is_some_and(|added_at| added_at >= since && added_at < until)
        })
        .collect::<Vec<_>>();

    let entries = filtered
        .iter()
        .take(request.top_k)
        .map(build_entry)
        .collect::<Vec<_>>();

    Ok(TimelineReport {
        project_id: request.project_id,
        generated_at: format_timestamp(now),
        window: TimelineWindow {
            since: format_timestamp(since),
            until: format_timestamp(until),
        },
        stats: TimelineStats {
            total_in_window: filtered.len() as u64,
            returned: entries.len(),
            top_wings: build_top_wings(&filtered),
        },
        entries,
    })
}

fn query_scoped_rows(
    db: &Database,
    scope: &ProjectSearchScope,
) -> Result<Vec<TimelineRow>, TimelineError> {
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
        .map_err(TimelineError::Prepare)?;

    statement
        .query_map(
            params![scope.mode_param(), scope.project_id.as_deref()],
            |row| {
                Ok(TimelineRow {
                    id: row.get::<_, String>(0)?,
                    content: row.get::<_, String>(1)?,
                    wing: row.get::<_, String>(2)?,
                    room: row.get::<_, Option<String>>(3)?,
                    added_at: row.get::<_, String>(4)?,
                    importance: row.get::<_, i32>(5)?,
                })
            },
        )
        .map_err(TimelineError::Query)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(TimelineError::Query)
}

fn build_entry(row: &TimelineRow) -> TimelineEntry {
    let preview_limit = if row.content.chars().count() > TIMELINE_PREVIEW_CHAR_LIMIT {
        TIMELINE_PREVIEW_CHAR_LIMIT.saturating_sub(1)
    } else {
        TIMELINE_PREVIEW_CHAR_LIMIT
    };
    let preview = truncate(&row.content, preview_limit.max(1));
    TimelineEntry {
        drawer_id: row.id.clone(),
        added_at: format_display_added_at(&row.added_at),
        importance_stars: row.importance.clamp(0, 5) as u8,
        wing: row.wing.clone(),
        room: row.room.clone(),
        preview: preview.content,
        preview_truncated: preview.truncated,
        original_content_bytes: row.content.len() as u64,
    }
}

fn build_top_wings(rows: &[TimelineRow]) -> Vec<TimelineWingCount> {
    let mut counts = BTreeMap::<String, u64>::new();
    for row in rows {
        *counts.entry(row.wing.clone()).or_default() += 1;
    }

    let mut top_wings = counts
        .into_iter()
        .map(|(wing, count)| TimelineWingCount { wing, count })
        .collect::<Vec<_>>();
    top_wings.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.wing.cmp(&right.wing))
    });
    top_wings.truncate(3);
    top_wings
}

fn parse_since_cutoff(raw: &str, now: i64) -> Result<i64, TimelineError> {
    if let Some(seconds) = parse_relative_duration(raw) {
        return Ok(now - seconds);
    }

    parse_absolute_timestamp(raw).ok_or_else(|| TimelineError::InvalidSince {
        value: raw.to_string(),
    })
}

fn parse_until_cutoff(raw: Option<&str>, now: i64) -> Result<i64, TimelineError> {
    match raw {
        None => Ok(now),
        Some(raw) => parse_absolute_timestamp(raw).ok_or_else(|| TimelineError::InvalidUntil {
            value: raw.to_string(),
        }),
    }
}

fn parse_relative_duration(raw: &str) -> Option<i64> {
    if raw.len() < 2 {
        return None;
    }

    let (digits, unit) = raw.split_at(raw.len() - 1);
    let value = digits.parse::<i64>().ok()?;
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return None,
    };
    Some(value * multiplier)
}

fn parse_absolute_timestamp(raw: &str) -> Option<i64> {
    raw.parse::<i64>().ok().or_else(|| parse_rfc3339(raw))
}

fn parse_added_at(raw: &str) -> Option<i64> {
    parse_absolute_timestamp(raw)
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
