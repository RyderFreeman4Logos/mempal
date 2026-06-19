//! Deterministic offline reflection reports.
//!
//! Reflection is a read-only audit surface for idle/offline maintenance. It
//! reports stable IDs and source metadata only; it does not rewrite drawers,
//! call embedders, or invoke LLMs.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::core::{
    db::{Database, DbError},
    decay::parse_temporal_timestamp_secs,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflectionMode {
    #[default]
    Deterministic,
}

impl ReflectionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deterministic => "deterministic",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflectionOptions {
    pub mode: ReflectionMode,
    pub project_id: Option<String>,
    pub limit_per_category: usize,
    pub now_unix_secs: u64,
}

impl ReflectionOptions {
    pub fn deterministic(now_unix_secs: u64) -> Self {
        Self {
            mode: ReflectionMode::Deterministic,
            project_id: None,
            limit_per_category: 10,
            now_unix_secs,
        }
    }
}

#[derive(Debug, Error)]
pub enum ReflectionError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("reflection limit is too large for SQLite: {0}")]
    LimitTooLarge(usize),
    #[error("database returned a negative count for {label}: {value}")]
    NegativeCount { label: &'static str, value: i64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReflectionReport {
    pub mode: ReflectionMode,
    pub dry_run: bool,
    pub project_id: Option<String>,
    pub generated_at_unix_secs: u64,
    pub limit_per_category: usize,
    pub summary: ReflectionSummary,
    pub duplicate_candidates: Vec<DuplicateCandidate>,
    pub expired_drawers: Vec<ExpiredDrawerCandidate>,
    pub stale_kg_facts: Vec<StaleKgFactCandidate>,
    pub tunnel_candidates: Vec<TunnelCandidate>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReflectionSummary {
    pub duplicate_group_count: usize,
    pub duplicate_drawer_count: usize,
    pub expired_drawer_count: usize,
    pub stale_kg_fact_count: usize,
    pub tunnel_candidate_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReflectionEvidenceRef {
    pub drawer_id: String,
    pub source_file: Option<String>,
    pub wing: String,
    pub room: Option<String>,
    pub project_id: Option<String>,
    pub added_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateCandidate {
    pub reason_code: String,
    pub content_hash_prefix: String,
    pub drawer_count: usize,
    pub samples: Vec<ReflectionEvidenceRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExpiredDrawerCandidate {
    pub reason_code: String,
    pub drawer: ReflectionEvidenceRef,
    pub valid_until: String,
    pub valid_until_unix_secs: i64,
    pub stale_penalty_applied: f64,
    pub already_downranked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleKgFactCandidate {
    pub reason_code: String,
    pub triple_id: String,
    pub source_drawer_id: Option<String>,
    pub source: Option<ReflectionEvidenceRef>,
    pub valid_to: String,
    pub valid_to_unix_secs: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelCandidate {
    pub reason_code: String,
    pub room: String,
    pub wings: Vec<String>,
    pub drawer_count: usize,
    pub samples: Vec<ReflectionEvidenceRef>,
}

#[derive(Debug)]
struct DuplicateGroupRow {
    content_hash: String,
    drawer_count: usize,
}

#[derive(Debug)]
struct ExpiredDrawerRow {
    evidence: ReflectionEvidenceRef,
    valid_until: String,
    stale_penalty_applied: f64,
}

pub fn current_unix_secs() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(_) => 0,
    }
}

pub fn run_reflection(
    db: &Database,
    options: &ReflectionOptions,
) -> Result<ReflectionReport, ReflectionError> {
    match options.mode {
        ReflectionMode::Deterministic => run_deterministic_reflection(db, options),
    }
}

fn run_deterministic_reflection(
    db: &Database,
    options: &ReflectionOptions,
) -> Result<ReflectionReport, ReflectionError> {
    let project_id = options.project_id.as_deref();
    let (duplicate_group_count, duplicate_drawer_count, duplicate_candidates) =
        find_duplicate_candidates(db, project_id, options.limit_per_category)?;
    let (expired_drawer_count, expired_drawers) = find_expired_drawers(
        db,
        project_id,
        options.now_unix_secs,
        options.limit_per_category,
    )?;
    let (stale_kg_fact_count, stale_kg_facts) = find_stale_kg_facts(
        db,
        project_id,
        options.now_unix_secs,
        options.limit_per_category,
    )?;
    let (tunnel_candidate_count, tunnel_candidates) =
        find_tunnel_candidates(db, project_id, options.limit_per_category)?;

    Ok(ReflectionReport {
        mode: ReflectionMode::Deterministic,
        dry_run: true,
        project_id: options.project_id.clone(),
        generated_at_unix_secs: options.now_unix_secs,
        limit_per_category: options.limit_per_category,
        summary: ReflectionSummary {
            duplicate_group_count,
            duplicate_drawer_count,
            expired_drawer_count,
            stale_kg_fact_count,
            tunnel_candidate_count,
        },
        duplicate_candidates,
        expired_drawers,
        stale_kg_facts,
        tunnel_candidates,
    })
}

fn find_duplicate_candidates(
    db: &Database,
    project_id: Option<&str>,
    limit: usize,
) -> Result<(usize, usize, Vec<DuplicateCandidate>), ReflectionError> {
    let mut stmt = db.conn().prepare(
        r#"
        SELECT content_hash, COUNT(*) AS drawer_count
        FROM drawers
        WHERE deleted_at IS NULL
          AND content_hash IS NOT NULL
          AND content_hash != ''
          AND (?1 IS NULL OR project_id = ?1)
        GROUP BY content_hash
        HAVING COUNT(*) > 1
        ORDER BY drawer_count DESC, content_hash ASC
        "#,
    )?;
    let rows = stmt.query_map(params![project_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;

    let mut groups = Vec::new();
    for row in rows {
        let (content_hash, drawer_count) = row?;
        groups.push(DuplicateGroupRow {
            content_hash,
            drawer_count: non_negative_usize("duplicate_drawer_count", drawer_count)?,
        });
    }

    let duplicate_group_count = groups.len();
    let duplicate_drawer_count = groups.iter().map(|group| group.drawer_count).sum::<usize>();
    let sample_limit = sql_limit(limit)?;
    let mut candidates = Vec::new();
    for group in groups.into_iter().take(limit) {
        candidates.push(DuplicateCandidate {
            reason_code: "exact_duplicate_content_hash".to_string(),
            content_hash_prefix: hash_prefix(&group.content_hash),
            drawer_count: group.drawer_count,
            samples: drawer_samples_for_hash(db, &group.content_hash, project_id, sample_limit)?,
        });
    }

    Ok((duplicate_group_count, duplicate_drawer_count, candidates))
}

fn find_expired_drawers(
    db: &Database,
    project_id: Option<&str>,
    now_unix_secs: u64,
    limit: usize,
) -> Result<(usize, Vec<ExpiredDrawerCandidate>), ReflectionError> {
    let now = unix_to_i64_saturating(now_unix_secs);
    let mut stmt = db.conn().prepare(
        r#"
        SELECT id, wing, room, source_file, project_id, added_at,
               valid_until, COALESCE(stale_penalty_applied, 1.0)
        FROM drawers
        WHERE deleted_at IS NULL
          AND valid_until IS NOT NULL
          AND TRIM(valid_until) != ''
          AND (?1 IS NULL OR project_id = ?1)
        ORDER BY valid_until ASC, id ASC
        "#,
    )?;
    let rows = stmt.query_map(params![project_id], |row| {
        Ok(ExpiredDrawerRow {
            evidence: evidence_from_row(row)?,
            valid_until: row.get(6)?,
            stale_penalty_applied: row.get(7)?,
        })
    })?;

    let mut count = 0usize;
    let mut candidates = Vec::new();
    for row in rows {
        let row = row?;
        let Some(valid_until_unix_secs) = parse_temporal_timestamp_secs(&row.valid_until) else {
            continue;
        };
        if valid_until_unix_secs >= now {
            continue;
        }
        count += 1;
        if candidates.len() < limit {
            candidates.push(ExpiredDrawerCandidate {
                reason_code: "expired_valid_until".to_string(),
                drawer: row.evidence,
                valid_until: row.valid_until,
                valid_until_unix_secs,
                stale_penalty_applied: row.stale_penalty_applied,
                already_downranked: row.stale_penalty_applied < 1.0,
            });
        }
    }

    Ok((count, candidates))
}

fn find_stale_kg_facts(
    db: &Database,
    project_id: Option<&str>,
    now_unix_secs: u64,
    limit: usize,
) -> Result<(usize, Vec<StaleKgFactCandidate>), ReflectionError> {
    let now = unix_to_i64_saturating(now_unix_secs);
    let triples = db.query_triples(None, None, None, false)?;
    let mut count = 0usize;
    let mut candidates = Vec::new();

    for triple in triples {
        let Some(valid_to) = triple.valid_to.as_deref() else {
            continue;
        };
        let Some(valid_to_unix_secs) = parse_temporal_timestamp_secs(valid_to) else {
            continue;
        };
        if valid_to_unix_secs >= now {
            continue;
        }
        let source = match triple.source_drawer.as_deref() {
            Some(drawer_id) => drawer_evidence_by_id(db, drawer_id)?,
            None => None,
        };
        if let Some(project_id) = project_id {
            match &source {
                Some(source) if source.project_id.as_deref() == Some(project_id) => {}
                _ => continue,
            }
        }

        count += 1;
        if candidates.len() < limit {
            candidates.push(StaleKgFactCandidate {
                reason_code: "kg_valid_to_expired".to_string(),
                triple_id: triple.id,
                source_drawer_id: triple.source_drawer,
                source,
                valid_to: valid_to.to_string(),
                valid_to_unix_secs,
            });
        }
    }

    Ok((count, candidates))
}

fn find_tunnel_candidates(
    db: &Database,
    project_id: Option<&str>,
    limit: usize,
) -> Result<(usize, Vec<TunnelCandidate>), ReflectionError> {
    let mut stmt = db.conn().prepare(
        r#"
        SELECT room, wing, COUNT(*) AS drawer_count
        FROM drawers
        WHERE deleted_at IS NULL
          AND room IS NOT NULL
          AND TRIM(room) != ''
          AND (?1 IS NULL OR project_id = ?1)
        GROUP BY room, wing
        ORDER BY room ASC, wing ASC
        "#,
    )?;
    let rows = stmt.query_map(params![project_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;

    let mut by_room: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for row in rows {
        let (room, wing, drawer_count) = row?;
        by_room.entry(room).or_default().insert(
            wing,
            non_negative_usize("tunnel_candidate_drawer_count", drawer_count)?,
        );
    }

    let candidate_rooms = by_room
        .into_iter()
        .filter(|(_, wings)| wings.len() > 1)
        .collect::<Vec<_>>();
    let count = candidate_rooms.len();
    let sample_limit = sql_limit(limit)?;
    let mut candidates = Vec::new();
    for (room, wings) in candidate_rooms.into_iter().take(limit) {
        let drawer_count = wings.values().sum::<usize>();
        candidates.push(TunnelCandidate {
            reason_code: "shared_room_across_wings".to_string(),
            room: room.clone(),
            wings: wings.keys().cloned().collect(),
            drawer_count,
            samples: drawer_samples_for_room(db, &room, project_id, sample_limit)?,
        });
    }

    Ok((count, candidates))
}

fn drawer_samples_for_hash(
    db: &Database,
    content_hash: &str,
    project_id: Option<&str>,
    limit: i64,
) -> Result<Vec<ReflectionEvidenceRef>, ReflectionError> {
    let mut stmt = db.conn().prepare(
        r#"
        SELECT id, wing, room, source_file, project_id, added_at
        FROM drawers
        WHERE deleted_at IS NULL
          AND content_hash = ?1
          AND (?2 IS NULL OR project_id = ?2)
        ORDER BY added_at DESC, id ASC
        LIMIT ?3
        "#,
    )?;
    let rows = stmt.query_map(params![content_hash, project_id, limit], evidence_from_row)?;
    collect_evidence(rows)
}

fn drawer_samples_for_room(
    db: &Database,
    room: &str,
    project_id: Option<&str>,
    limit: i64,
) -> Result<Vec<ReflectionEvidenceRef>, ReflectionError> {
    let mut stmt = db.conn().prepare(
        r#"
        SELECT id, wing, room, source_file, project_id, added_at
        FROM drawers
        WHERE deleted_at IS NULL
          AND room = ?1
          AND (?2 IS NULL OR project_id = ?2)
        ORDER BY wing ASC, added_at DESC, id ASC
        LIMIT ?3
        "#,
    )?;
    let rows = stmt.query_map(params![room, project_id, limit], evidence_from_row)?;
    collect_evidence(rows)
}

fn drawer_evidence_by_id(
    db: &Database,
    drawer_id: &str,
) -> Result<Option<ReflectionEvidenceRef>, ReflectionError> {
    let mut stmt = db.conn().prepare(
        r#"
        SELECT id, wing, room, source_file, project_id, added_at
        FROM drawers
        WHERE id = ?1
        LIMIT 1
        "#,
    )?;
    let mut rows = stmt.query_map(params![drawer_id], evidence_from_row)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

fn evidence_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReflectionEvidenceRef> {
    Ok(ReflectionEvidenceRef {
        drawer_id: row.get(0)?,
        wing: row.get(1)?,
        room: row.get(2)?,
        source_file: row.get(3)?,
        project_id: row.get(4)?,
        added_at: row.get(5)?,
    })
}

fn collect_evidence<F>(
    rows: rusqlite::MappedRows<'_, F>,
) -> Result<Vec<ReflectionEvidenceRef>, ReflectionError>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<ReflectionEvidenceRef>,
{
    let mut evidence = Vec::new();
    for row in rows {
        evidence.push(row?);
    }
    Ok(evidence)
}

fn hash_prefix(hash: &str) -> String {
    hash.chars().take(16).collect()
}

fn sql_limit(limit: usize) -> Result<i64, ReflectionError> {
    i64::try_from(limit).map_err(|_| ReflectionError::LimitTooLarge(limit))
}

fn non_negative_usize(label: &'static str, value: i64) -> Result<usize, ReflectionError> {
    usize::try_from(value).map_err(|_| ReflectionError::NegativeCount { label, value })
}

fn unix_to_i64_saturating(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use tempfile::TempDir;

    use crate::core::{
        db::Database,
        types::{BootstrapEvidenceArgs, Drawer, SourceType, Triple},
        utils::build_triple_id,
    };

    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn new_db() -> (TempDir, Database) {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        (tmp, db)
    }

    fn drawer(id: &str, content: &str, wing: &str, room: Option<&str>) -> Drawer {
        Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
            id: id.to_string(),
            content: content.to_string(),
            wing: wing.to_string(),
            room: room.map(str::to_string),
            source_file: Some(format!("{id}.md")),
            source_type: SourceType::AgentInference,
            added_at: NOW.to_string(),
            chunk_index: Some(0),
            importance: 3,
        })
    }

    #[test]
    fn deterministic_report_is_source_backed_for_duplicates_stale_and_tunnels() {
        let (_tmp, db) = new_db();
        db.insert_drawer(&drawer("dup-a", "same memory", "alpha", Some("shared")))
            .expect("insert dup a");
        db.insert_drawer(&drawer("dup-b", "same memory", "alpha", Some("shared")))
            .expect("insert dup b");
        db.insert_drawer(&drawer(
            "tunnel-b",
            "different memory",
            "beta",
            Some("shared"),
        ))
        .expect("insert tunnel b");
        db.insert_drawer_with_project_validity(
            &drawer("expired", "old memory", "alpha", Some("facts")),
            None,
            None,
            None,
            Some("1790000000"),
        )
        .expect("insert expired");
        db.insert_triple(&Triple {
            id: build_triple_id("Alice", "works_at", "Acme"),
            subject: "Alice".to_string(),
            predicate: "works_at".to_string(),
            object: "Acme".to_string(),
            valid_from: Some("1780000000".to_string()),
            valid_to: Some("1790000000".to_string()),
            confidence: 0.9,
            source_drawer: Some("expired".to_string()),
        })
        .expect("insert stale triple");

        let report =
            run_reflection(&db, &ReflectionOptions::deterministic(NOW)).expect("run reflection");

        assert!(report.dry_run);
        assert_eq!(report.summary.duplicate_group_count, 1);
        assert_eq!(report.summary.duplicate_drawer_count, 2);
        assert_eq!(report.summary.expired_drawer_count, 1);
        assert_eq!(report.summary.stale_kg_fact_count, 1);
        assert_eq!(report.summary.tunnel_candidate_count, 1);
        assert_eq!(
            report.duplicate_candidates[0]
                .samples
                .iter()
                .map(|sample| sample.drawer_id.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["dup-a", "dup-b"])
        );
        assert_eq!(report.expired_drawers[0].drawer.drawer_id, "expired");
        assert_eq!(
            report.stale_kg_facts[0]
                .source
                .as_ref()
                .map(|s| s.drawer_id.as_str()),
            Some("expired")
        );
        assert_eq!(report.tunnel_candidates[0].room, "shared");
    }

    #[test]
    fn deterministic_report_does_not_mutate_database() {
        let (_tmp, db) = new_db();
        db.insert_drawer(&drawer("dup-a", "same memory", "alpha", Some("shared")))
            .expect("insert dup a");
        db.insert_drawer(&drawer("dup-b", "same memory", "alpha", Some("shared")))
            .expect("insert dup b");
        db.insert_triple(&Triple {
            id: build_triple_id("Bob", "reports_to", "Alice"),
            subject: "Bob".to_string(),
            predicate: "reports_to".to_string(),
            object: "Alice".to_string(),
            valid_from: Some("1780000000".to_string()),
            valid_to: Some("1790000000".to_string()),
            confidence: 0.9,
            source_drawer: Some("dup-a".to_string()),
        })
        .expect("insert stale triple");
        let before_drawers = db.drawer_count().expect("drawer count before");
        let before_triples = db.triple_count().expect("triple count before");
        let before_tunnels = db.list_explicit_tunnels(None).expect("tunnels before");

        let _report =
            run_reflection(&db, &ReflectionOptions::deterministic(NOW)).expect("run reflection");

        assert_eq!(
            db.drawer_count().expect("drawer count after"),
            before_drawers
        );
        assert_eq!(
            db.triple_count().expect("triple count after"),
            before_triples
        );
        assert_eq!(
            db.list_explicit_tunnels(None).expect("tunnels after"),
            before_tunnels
        );
    }
}
