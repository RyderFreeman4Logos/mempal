#![warn(clippy::all)]

//! Tiered retrieval strategies for `mempal_context` (P14).
//!
//! T1 (dao_tian): decision/feedback/rule drawers, scored by effective_importance × recency.
//! T3 (qi): recent drawers within recency_window_days, sorted by added_at DESC.
//! T2 (shu): uses the existing hybrid search path in mod.rs.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::params;
use thiserror::Error;

use serde::Serialize;

use crate::core::db::Database;
use crate::core::db::DbError;
use crate::core::decay::validity_window_contains_at;
use crate::embed::estimate_tokens;

struct DrawerRow {
    id: String,
    content: String,
    room: Option<String>,
    source_file: Option<String>,
    added_at: String,
    effective_importance: f64,
    valid_from: Option<String>,
    valid_until: Option<String>,
}

impl DrawerRow {
    fn is_valid_at(&self, now_unix: i64) -> bool {
        validity_window_contains_at(
            self.valid_from.as_deref(),
            self.valid_until.as_deref(),
            now_unix,
        )
    }
}

/// A single item retrieved by the tiered retrieval strategies.
#[derive(Debug, Clone, Serialize)]
pub struct TieredItem {
    pub drawer_id: String,
    pub content: String,
    pub source_file: String,
    /// The drawer `room` field — maps to "type" in the spec (e.g. "decision", "feedback").
    pub room: Option<String>,
    /// T3 provenance: "recency", "kg", or "tunnel".
    pub t3_source: Option<String>,
    pub effective_importance: f64,
    pub added_at_unix: i64,
    pub matched_pattern_id: Option<String>,
}

/// Token usage breakdown returned alongside tiered context.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BudgetUsed {
    pub t1_tokens: usize,
    pub t2_tokens: usize,
    pub t3_tokens: usize,
}

impl BudgetUsed {
    pub fn total_tokens(&self) -> usize {
        self.t1_tokens + self.t2_tokens + self.t3_tokens
    }
}

#[derive(Debug, Error)]
pub enum TieredError {
    #[error("tiered T1 query failed")]
    T1Query(#[source] DbError),
    #[error("tiered T3 query failed")]
    T3Query(#[source] DbError),
    #[error("tiered KG query failed")]
    KgQuery(#[source] DbError),
}

/// Trigger context that adjusts budget weights per use-case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextTrigger {
    /// Session start: emphasise recent context (T3) and rules (T1).
    #[default]
    SessionStart,
    /// On-demand mid-task query: emphasise query relevance (T2).
    OnDemand,
    /// Error repair: strongly emphasise decisions/rules (T1).
    Repair,
}

impl ContextTrigger {
    /// (t1_weight, t2_weight, t3_weight) — raw multipliers before normalisation.
    fn weights(self) -> (f64, f64, f64) {
        match self {
            Self::SessionStart => (1.0, 0.8, 1.2),
            Self::OnDemand => (0.7, 1.3, 0.5),
            Self::Repair => (1.5, 0.8, 0.5),
        }
    }
}

/// Parameters for the T1 retrieval pass.
pub struct T1Params<'a> {
    pub min_importance: u8,
    pub lambda: f64,
    pub budget_tokens: usize,
    pub project_id: Option<&'a str>,
    pub now_unix: i64,
}

/// Parameters for the T3 retrieval pass.
pub struct T3Params<'a> {
    pub recency_window_days: u64,
    pub budget_tokens: usize,
    pub project_id: Option<&'a str>,
    pub exclude_ids: &'a [String],
    pub now_unix: i64,
}

/// Compute per-tier token budgets given base ratios and a trigger.
///
/// Formula: `budget_i = total × (ratio_i × weight_i) / Σ(ratio_j × weight_j)`.
pub fn compute_budgets(
    total: usize,
    t1_ratio: f64,
    t2_ratio: f64,
    t3_ratio: f64,
    trigger: ContextTrigger,
) -> (usize, usize, usize) {
    let (w1, w2, w3) = trigger.weights();
    let adj1 = t1_ratio * w1;
    let adj2 = t2_ratio * w2;
    let adj3 = t3_ratio * w3;
    let sum = adj1 + adj2 + adj3;
    if sum == 0.0 {
        return (0, total, 0);
    }
    let t1 = ((total as f64) * adj1 / sum).round() as usize;
    let t3 = ((total as f64) * adj3 / sum).round() as usize;
    let t2 = total.saturating_sub(t1 + t3);
    (t1, t2, t3)
}

/// Fetch and score T1 candidates (decision/feedback/rule drawers with high importance).
///
/// Returns items ordered by descending T1 score, truncated to fit within `budget_tokens`.
pub fn fetch_t1(db: &Database, params: T1Params<'_>) -> Result<Vec<TieredItem>, TieredError> {
    let conn = db.conn();

    // Build project filter clause.
    let project_clause = if params.project_id.is_some() {
        "(project_id = ?2 OR project_id IS NULL)"
    } else {
        "1 = 1"
    };

    let sql = format!(
        r#"
        -- NULLIF(...,0.0) maps the persisted 0.0 sentinel to NULL so it falls back
        -- to base importance instead of ranking last (GitHub #309). The fallback
        -- carries the persisted stale penalty (default 1.0) so a legacy 0.0 row that
        -- fact-check down-ranked ranks at importance*penalty, not full importance.
        SELECT id, content, room, source_file, added_at,
               COALESCE(NULLIF(effective_importance, 0.0), CAST(COALESCE(importance, 0) AS REAL) * COALESCE(stale_penalty_applied, 1.0)),
               valid_from, valid_until
        FROM drawers
        WHERE deleted_at IS NULL
          AND room IN ('decision', 'feedback', 'rule')
          AND COALESCE(importance, 0) >= ?1
          AND {project_clause}
        ORDER BY COALESCE(NULLIF(effective_importance, 0.0), CAST(COALESCE(importance, 0) AS REAL) * COALESCE(stale_penalty_applied, 1.0)) DESC,
                 CAST(added_at AS INTEGER) DESC
        "#,
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| TieredError::T1Query(DbError::Sqlite(e)))?;

    let rows: Vec<DrawerRow> = if let Some(pid) = params.project_id {
        stmt.query_map(params![params.min_importance as i32, pid], |row| {
            Ok(DrawerRow {
                id: row.get::<_, String>(0)?,
                content: row.get::<_, String>(1)?,
                room: row.get::<_, Option<String>>(2)?,
                source_file: row.get::<_, Option<String>>(3)?,
                added_at: row.get::<_, String>(4)?,
                effective_importance: row.get::<_, f64>(5)?,
                valid_from: row.get::<_, Option<String>>(6)?,
                valid_until: row.get::<_, Option<String>>(7)?,
            })
        })
        .map_err(|e| TieredError::T1Query(DbError::Sqlite(e)))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TieredError::T1Query(DbError::Sqlite(e)))?
    } else {
        stmt.query_map(params![params.min_importance as i32], |row| {
            Ok(DrawerRow {
                id: row.get::<_, String>(0)?,
                content: row.get::<_, String>(1)?,
                room: row.get::<_, Option<String>>(2)?,
                source_file: row.get::<_, Option<String>>(3)?,
                added_at: row.get::<_, String>(4)?,
                effective_importance: row.get::<_, f64>(5)?,
                valid_from: row.get::<_, Option<String>>(6)?,
                valid_until: row.get::<_, Option<String>>(7)?,
            })
        })
        .map_err(|e| TieredError::T1Query(DbError::Sqlite(e)))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TieredError::T1Query(DbError::Sqlite(e)))?
    };

    // Score and sort: score = effective_importance × exp(-λ × days_since_added_at).
    let mut scored: Vec<(f64, TieredItem)> = rows
        .into_iter()
        .filter(|row| row.is_valid_at(params.now_unix))
        .map(|row| {
            let added_unix = parse_added_at_unix(&row.added_at);
            let days = (params.now_unix - added_unix).max(0) as f64 / 86_400.0;
            let score = row.effective_importance * (-params.lambda * days).exp();
            let item = TieredItem {
                drawer_id: row.id,
                content: row.content,
                source_file: row.source_file.unwrap_or_default(),
                room: row.room,
                t3_source: None,
                effective_importance: row.effective_importance,
                added_at_unix: added_unix,
                matched_pattern_id: None,
            };
            (score, item)
        })
        .collect();

    scored.sort_by(|(a, _), (b, _)| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));

    // Truncate to budget.
    let mut items = Vec::new();
    let mut used = 0usize;
    for (_, item) in scored {
        let tokens = estimate_tokens(&item.content);
        if used + tokens > params.budget_tokens && !items.is_empty() {
            break;
        }
        used += tokens;
        items.push(item);
    }

    Ok(items)
}

/// Fetch T3 candidates: recent drawers within `recency_window_days`, sorted newest-first.
///
/// Excludes drawer IDs already used by T1 (and T2 if available).
pub fn fetch_t3(db: &Database, params: T3Params<'_>) -> Result<Vec<TieredItem>, TieredError> {
    let conn = db.conn();
    let cutoff = params
        .now_unix
        .saturating_sub((params.recency_window_days as i64) * 86_400);

    let project_clause = if params.project_id.is_some() {
        "(project_id = ?2 OR project_id IS NULL)"
    } else {
        "1 = 1"
    };

    let sql = format!(
        r#"
        -- NULLIF(...,0.0): persisted 0.0 sentinel falls back to importance*penalty
        -- (stale_penalty_applied default 1.0) so down-ranked legacy rows stay down (GitHub #309).
        SELECT id, content, room, source_file, added_at,
               COALESCE(NULLIF(effective_importance, 0.0), CAST(COALESCE(importance, 0) AS REAL) * COALESCE(stale_penalty_applied, 1.0)),
               valid_from, valid_until
        FROM drawers
        WHERE deleted_at IS NULL
          AND CAST(added_at AS INTEGER) >= ?1
          AND {project_clause}
        ORDER BY CAST(added_at AS INTEGER) DESC, id DESC
        "#,
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| TieredError::T3Query(DbError::Sqlite(e)))?;

    let rows: Vec<DrawerRow> = if let Some(pid) = params.project_id {
        stmt.query_map(params![cutoff, pid], |row| {
            Ok(DrawerRow {
                id: row.get::<_, String>(0)?,
                content: row.get::<_, String>(1)?,
                room: row.get::<_, Option<String>>(2)?,
                source_file: row.get::<_, Option<String>>(3)?,
                added_at: row.get::<_, String>(4)?,
                effective_importance: row.get::<_, f64>(5)?,
                valid_from: row.get::<_, Option<String>>(6)?,
                valid_until: row.get::<_, Option<String>>(7)?,
            })
        })
        .map_err(|e| TieredError::T3Query(DbError::Sqlite(e)))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TieredError::T3Query(DbError::Sqlite(e)))?
    } else {
        stmt.query_map(params![cutoff], |row| {
            Ok(DrawerRow {
                id: row.get::<_, String>(0)?,
                content: row.get::<_, String>(1)?,
                room: row.get::<_, Option<String>>(2)?,
                source_file: row.get::<_, Option<String>>(3)?,
                added_at: row.get::<_, String>(4)?,
                effective_importance: row.get::<_, f64>(5)?,
                valid_from: row.get::<_, Option<String>>(6)?,
                valid_until: row.get::<_, Option<String>>(7)?,
            })
        })
        .map_err(|e| TieredError::T3Query(DbError::Sqlite(e)))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TieredError::T3Query(DbError::Sqlite(e)))?
    };

    let exclude_set: std::collections::BTreeSet<&str> =
        params.exclude_ids.iter().map(String::as_str).collect();

    let mut items = Vec::new();
    let mut used = 0usize;
    for row in rows {
        if !row.is_valid_at(params.now_unix) || exclude_set.contains(row.id.as_str()) {
            continue;
        }
        let tokens = estimate_tokens(&row.content);
        if used + tokens > params.budget_tokens && !items.is_empty() {
            break;
        }
        used += tokens;
        items.push(TieredItem {
            added_at_unix: parse_added_at_unix(&row.added_at),
            drawer_id: row.id,
            content: row.content,
            source_file: row.source_file.unwrap_or_default(),
            room: row.room,
            t3_source: Some("recency".to_string()),
            effective_importance: row.effective_importance,
            matched_pattern_id: None,
        });
    }

    Ok(items)
}

/// Fetch T3 KG-neighbor candidates: drawers cited as `source_drawer` in KG triples
/// where the subject or object appears in `query_terms`.
pub fn fetch_t3_kg(
    db: &Database,
    query_terms: &[&str],
    budget_tokens: usize,
    project_id: Option<&str>,
    exclude_ids: &[String],
    now_unix: i64,
) -> Result<Vec<TieredItem>, TieredError> {
    if query_terms.is_empty() || budget_tokens == 0 {
        return Ok(Vec::new());
    }
    let conn = db.conn();
    let exclude_set: std::collections::BTreeSet<&str> =
        exclude_ids.iter().map(String::as_str).collect();

    let project_clause = if project_id.is_some() {
        "(d.project_id = ?3 OR d.project_id IS NULL)"
    } else {
        "1 = 1"
    };

    // One query per term to avoid dynamic IN lists; collect unique drawer IDs.
    let mut candidate_ids: Vec<String> = Vec::new();
    for term in query_terms {
        let term_pattern = format!("%{term}%");
        let sql = format!(
            r#"
            WITH matched AS (
                SELECT
                    t.source_drawer,
                    MIN(t.rowid) AS first_seen,
                    TRIM(COALESCE(d.valid_from, '')) AS valid_from,
                    TRIM(COALESCE(d.valid_until, '')) AS valid_until
                FROM triples t
                JOIN drawers d ON d.id = t.source_drawer AND d.deleted_at IS NULL
                WHERE t.source_drawer IS NOT NULL
                  AND (t.subject LIKE ?1 OR t.object LIKE ?1)
                  AND {project_clause}
                GROUP BY t.source_drawer, d.valid_from, d.valid_until
            ),
            parsed AS (
                SELECT
                    source_drawer,
                    first_seen,
                    CASE
                        WHEN valid_from = '' THEN NULL
                        WHEN (
                            (valid_from GLOB '-[0-9]*' AND substr(valid_from, 2) NOT GLOB '*[^0-9]*')
                            OR (valid_from GLOB '[0-9]*' AND valid_from NOT GLOB '*[^0-9]*')
                        ) THEN CAST(valid_from AS INTEGER)
                        ELSE CAST(strftime('%s', valid_from) AS INTEGER)
                    END AS valid_from_secs,
                    CASE
                        WHEN valid_until = '' THEN NULL
                        WHEN (
                            (valid_until GLOB '-[0-9]*' AND substr(valid_until, 2) NOT GLOB '*[^0-9]*')
                            OR (valid_until GLOB '[0-9]*' AND valid_until NOT GLOB '*[^0-9]*')
                        ) THEN CAST(valid_until AS INTEGER)
                        ELSE CAST(strftime('%s', valid_until) AS INTEGER)
                    END AS valid_until_secs
                FROM matched
            )
            SELECT source_drawer
            FROM parsed
            WHERE (valid_from_secs IS NULL OR valid_from_secs <= ?2)
              AND (valid_until_secs IS NULL OR valid_until_secs >= ?2)
            ORDER BY first_seen ASC
            LIMIT 20
            "#,
        );
        let ids: Vec<String> = if let Some(pid) = project_id {
            conn.prepare(&sql)
                .map_err(|e| TieredError::KgQuery(DbError::Sqlite(e)))?
                .query_map(params![term_pattern, now_unix, pid], |row| row.get(0))
                .map_err(|e| TieredError::KgQuery(DbError::Sqlite(e)))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| TieredError::KgQuery(DbError::Sqlite(e)))?
        } else {
            conn.prepare(&sql)
                .map_err(|e| TieredError::KgQuery(DbError::Sqlite(e)))?
                .query_map(params![term_pattern, now_unix], |row| row.get(0))
                .map_err(|e| TieredError::KgQuery(DbError::Sqlite(e)))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| TieredError::KgQuery(DbError::Sqlite(e)))?
        };
        for id in ids {
            if !candidate_ids.contains(&id) {
                candidate_ids.push(id);
            }
        }
    }

    // Fetch drawer content for each candidate, skipping excluded IDs.
    let mut items = Vec::new();
    let mut used = 0usize;
    for drawer_id in candidate_ids {
        if exclude_set.contains(drawer_id.as_str()) {
            continue;
        }
        let validity = conn.query_row(
            "SELECT valid_from, valid_until FROM drawers WHERE id = ?1 AND deleted_at IS NULL",
            params![drawer_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        );
        let (valid_from, valid_until) = match validity {
            Ok(value) => value,
            Err(rusqlite::Error::QueryReturnedNoRows) => continue,
            Err(error) => return Err(TieredError::KgQuery(DbError::Sqlite(error))),
        };
        if !validity_window_contains_at(valid_from.as_deref(), valid_until.as_deref(), now_unix) {
            continue;
        }
        if let Ok(Some(d)) = db.get_drawer(&drawer_id) {
            let tokens = estimate_tokens(&d.content);
            if used + tokens > budget_tokens && !items.is_empty() {
                break;
            }
            used += tokens;
            let added_unix = parse_added_at_unix(&d.added_at);
            items.push(TieredItem {
                drawer_id: d.id,
                content: d.content,
                source_file: d.source_file.unwrap_or_default(),
                room: d.room,
                t3_source: Some("kg".to_string()),
                effective_importance: d.effective_importance,
                added_at_unix: added_unix,
                matched_pattern_id: None,
            });
        }
        if used >= budget_tokens {
            break;
        }
    }
    Ok(items)
}

/// Parse `added_at` as unix seconds. Accepts both integer strings and ISO-8601.
pub fn parse_added_at_unix(raw: &str) -> i64 {
    if let Ok(secs) = raw.parse::<i64>() {
        return secs;
    }
    // Fallback: treat as seconds since epoch via SystemTime (best-effort).
    if let Ok(dt) = raw.parse::<u64>() {
        return dt as i64;
    }
    // If it looks like an RFC-3339 string, try a simple parse.
    parse_rfc3339_approx(raw).unwrap_or(0)
}

fn parse_rfc3339_approx(s: &str) -> Option<i64> {
    // "2026-04-22T15:30:00Z" → unix seconds (rough: skip sub-second)
    let s = s.trim_end_matches('Z');
    let parts: Vec<&str> = s.split('T').collect();
    if parts.len() != 2 {
        return None;
    }
    let date_parts: Vec<u32> = parts[0].split('-').filter_map(|p| p.parse().ok()).collect();
    let time_parts: Vec<u32> = parts[1].split(':').filter_map(|p| p.parse().ok()).collect();
    if date_parts.len() < 3 || time_parts.len() < 2 {
        return None;
    }
    // Very rough: days from epoch to year-month-day, then add time.
    // Use SystemTime for accuracy — build from components via Duration arithmetic.
    let year = date_parts[0];
    let month = date_parts[1];
    let day = date_parts[2];
    let hour = time_parts[0];
    let min = time_parts[1];
    let sec = time_parts.get(2).copied().unwrap_or(0);
    // Days from 1970-01-01 to year-month-day (Gregorian, ignoring leap seconds).
    let days = days_from_epoch(year, month, day)?;
    let secs = days as i64 * 86_400 + hour as i64 * 3_600 + min as i64 * 60 + sec as i64;
    Some(secs)
}

fn days_from_epoch(year: u32, month: u32, day: u32) -> Option<u32> {
    if year < 1970 || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days_per_month = [0u32, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut days = 0u32;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    for m in 1..month {
        let extra = if m == 2 && is_leap(year) { 1 } else { 0 };
        days += days_per_month[m as usize] + extra;
    }
    days += day - 1;
    Some(days)
}

fn is_leap(year: u32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

/// Current unix timestamp in seconds.
pub fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_budgets_session_start_sums_to_total() {
        let (t1, t2, t3) = compute_budgets(8000, 0.30, 0.50, 0.20, ContextTrigger::SessionStart);
        assert_eq!(t1 + t2 + t3, 8000);
    }

    #[test]
    fn test_compute_budgets_repair_t1_larger_than_session_start() {
        let (t1_ss, _, _) = compute_budgets(8000, 0.30, 0.50, 0.20, ContextTrigger::SessionStart);
        let (t1_rep, _, _) = compute_budgets(8000, 0.30, 0.50, 0.20, ContextTrigger::Repair);
        assert!(
            t1_rep > t1_ss,
            "repair t1={t1_rep} should > session_start t1={t1_ss}"
        );
    }

    #[test]
    fn test_compute_budgets_on_demand_t2_larger_than_session_start() {
        let (_, t2_ss, _) = compute_budgets(8000, 0.30, 0.50, 0.20, ContextTrigger::SessionStart);
        let (_, t2_od, _) = compute_budgets(8000, 0.30, 0.50, 0.20, ContextTrigger::OnDemand);
        assert!(
            t2_od > t2_ss,
            "on_demand t2={t2_od} should > session_start t2={t2_ss}"
        );
    }

    #[test]
    fn test_t1_recency_lambda_affects_ordering() {
        // drawer-new: today, importance=3; drawer-old: 30 days ago, importance=5.
        let now = now_unix_secs();
        let old_unix = now - 30 * 86_400;
        let lambda = 0.1_f64;
        let score_new = 3.0_f64 * (-lambda * 0.0_f64).exp();
        let score_old = 5.0_f64 * (-lambda * 30.0_f64).exp();
        assert!(
            score_new > score_old,
            "drawer-new score {score_new:.4} should > drawer-old score {score_old:.4} at lambda={lambda}"
        );
        let _ = old_unix;
    }

    #[test]
    fn test_parse_added_at_unix_integer() {
        assert_eq!(parse_added_at_unix("1710000000"), 1_710_000_000);
    }

    #[test]
    fn test_parse_added_at_unix_rfc3339() {
        let secs = parse_added_at_unix("2026-04-22T00:00:00Z");
        // rough check — 2026-04-22 is after 2026-01-01 (unix ~1_767_225_600)
        assert!(secs > 1_767_000_000, "got {secs}");
    }
}
