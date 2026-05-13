//! Deterministic offline memory consolidation.
//!
//! Sleep runs in three phases:
//! - NREM prunes stale low-importance drawers and compacts similar clusters.
//! - REM checks active drawers against the KG and invalidates lower-confidence
//!   contradicted triples when the new fact is also newer.
//! - Salience scores active drawers so the next NREM pass can prioritize the
//!   most useful clusters first.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};
use thiserror::Error;

use crate::core::{
    compaction::merge_cluster,
    config::Config,
    db::{Database, DbError, find_similar_clusters},
    decay::parse_temporal_timestamp_secs,
    types::CompactionStrategy,
    utils::{current_timestamp, iso_timestamp},
};
use crate::factcheck::{self, FactCheckError, FactIssue};

#[derive(Debug, Error)]
pub enum SleepError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    FactCheck(#[from] FactCheckError),
    #[error("invalid sleep configuration: {0}")]
    InvalidConfig(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepPhase {
    Nrem,
    Rem,
    Salience,
}

impl SleepPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nrem => "nrem",
            Self::Rem => "rem",
            Self::Salience => "salience",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SleepPhaseSelection {
    pub nrem: bool,
    pub rem: bool,
    pub salience: bool,
}

impl SleepPhaseSelection {
    pub fn all() -> Self {
        Self {
            nrem: true,
            rem: true,
            salience: true,
        }
    }

    pub fn selected_or_all(self) -> Vec<SleepPhase> {
        let selected = if self.any() { self } else { Self::all() };
        let mut phases = Vec::new();
        if selected.nrem {
            phases.push(SleepPhase::Nrem);
        }
        if selected.rem {
            phases.push(SleepPhase::Rem);
        }
        if selected.salience {
            phases.push(SleepPhase::Salience);
        }
        phases
    }

    fn any(self) -> bool {
        self.nrem || self.rem || self.salience
    }

    fn label(self) -> &'static str {
        match (self.any(), self.nrem, self.rem, self.salience) {
            (false, _, _, _) | (true, true, true, true) => "full",
            (true, true, false, false) => "nrem",
            (true, false, true, false) => "rem",
            (true, false, false, true) => "salience",
            _ => "selected",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SleepRunOptions {
    pub phases: SleepPhaseSelection,
    pub dry_run: bool,
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NremSummary {
    pub processed_drawers: usize,
    pub pruned_drawers: usize,
    pub clusters_found: usize,
    pub clusters_compacted: usize,
    pub compacted_drawers: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemSummary {
    pub processed_drawers: usize,
    pub conflicts_detected: usize,
    pub conflicts_resolved: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SalienceSummary {
    pub processed_drawers: usize,
    pub scored_drawers: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SleepCycleSummary {
    pub dry_run: bool,
    pub phases: Vec<SleepPhase>,
    pub nrem: Option<NremSummary>,
    pub rem: Option<RemSummary>,
    pub salience: Option<SalienceSummary>,
}

impl SleepCycleSummary {
    pub fn processed_count(&self) -> usize {
        self.nrem
            .as_ref()
            .map_or(0, |summary| summary.processed_drawers)
            + self
                .rem
                .as_ref()
                .map_or(0, |summary| summary.processed_drawers)
            + self
                .salience
                .as_ref()
                .map_or(0, |summary| summary.processed_drawers)
    }

    pub fn pruned_count(&self) -> usize {
        self.nrem
            .as_ref()
            .map_or(0, |summary| summary.pruned_drawers)
    }

    pub fn compacted_count(&self) -> usize {
        self.nrem
            .as_ref()
            .map_or(0, |summary| summary.compacted_drawers)
    }

    pub fn conflicts_resolved_count(&self) -> usize {
        self.rem
            .as_ref()
            .map_or(0, |summary| summary.conflicts_resolved)
    }

    pub fn salience_scored_count(&self) -> usize {
        self.salience
            .as_ref()
            .map_or(0, |summary| summary.scored_drawers)
    }
}

#[derive(Debug, Clone)]
struct DrawerForSleep {
    id: String,
    content: String,
    wing: String,
    confidence: f64,
    added_at: String,
}

#[derive(Debug, Clone)]
struct SalienceCandidate {
    id: String,
    added_at: String,
    importance: i32,
    access_count: i64,
    has_citations: bool,
}

pub fn run_sleep_cycle(
    db: &Database,
    config: &Config,
    options: SleepRunOptions,
) -> Result<SleepCycleSummary, SleepError> {
    let phases = options.phases.selected_or_all();
    let project_ids = sleep_project_ids(db, options.project_id.as_deref())?;
    let mut summary = SleepCycleSummary {
        dry_run: options.dry_run,
        phases: phases.clone(),
        ..SleepCycleSummary::default()
    };

    for phase in phases {
        match phase {
            SleepPhase::Nrem => {
                summary.nrem = Some(run_nrem(db, config, &project_ids, options.dry_run)?);
            }
            SleepPhase::Rem => {
                summary.rem = Some(run_rem(db, config, &project_ids, options.dry_run)?);
            }
            SleepPhase::Salience => {
                summary.salience = Some(run_salience(db, &project_ids, options.dry_run)?);
            }
        }
    }

    if !options.dry_run {
        insert_sleep_log(db, options.phases.label(), &summary)?;
    }

    Ok(summary)
}

fn run_nrem(
    db: &Database,
    config: &Config,
    project_ids: &[Option<String>],
    dry_run: bool,
) -> Result<NremSummary, SleepError> {
    let now_secs = unix_now_secs();
    let sleep_at = iso_timestamp();
    let mut summary = NremSummary::default();

    for project_id in project_ids {
        let candidates = prune_candidates(
            db,
            project_id.as_deref(),
            config.sleep.nrem_prune_max_importance,
        )?;
        summary.processed_drawers += candidates.len();

        let old_ids = candidates
            .into_iter()
            .filter_map(|(id, added_at)| {
                let added_at_secs = parse_temporal_timestamp_secs(&added_at)?;
                let age_days = (now_secs - added_at_secs).max(0) as u64 / 86_400;
                (age_days >= config.sleep.nrem_prune_min_age_days).then_some(id)
            })
            .collect::<Vec<_>>();

        summary.pruned_drawers += old_ids.len();
        if !dry_run && !old_ids.is_empty() {
            soft_delete_drawers_for_sleep(db, &old_ids, &sleep_at)?;
        }
        let pruned_ids = old_ids.into_iter().collect::<HashSet<_>>();

        for wing in active_wings(db, project_id.as_deref())? {
            let mut clusters = find_similar_clusters(
                db.conn(),
                Some(wing.as_str()),
                None,
                project_id.as_deref(),
                config.sleep.nrem_compaction_threshold,
                config.consolidation.min_cluster_size,
            )?;
            if !pruned_ids.is_empty() {
                clusters = clusters
                    .into_iter()
                    .filter_map(|cluster| {
                        let filtered = cluster
                            .into_iter()
                            .filter(|(drawer_id, _)| !pruned_ids.contains(drawer_id))
                            .collect::<Vec<_>>();
                        (filtered.len() >= config.consolidation.min_cluster_size)
                            .then_some(filtered)
                    })
                    .collect();
            }
            summary.clusters_found += clusters.len();
            sort_clusters_by_priority(db, &mut clusters)?;

            for cluster in clusters
                .iter()
                .take(config.consolidation.max_clusters_per_run)
            {
                let drawer_ids = cluster
                    .iter()
                    .map(|(drawer_id, _)| drawer_id.clone())
                    .collect::<Vec<_>>();
                if drawer_ids.is_empty() {
                    continue;
                }
                let result =
                    merge_cluster(db, &drawer_ids, CompactionStrategy::RichestContent, dry_run)?;
                summary.clusters_compacted += 1;
                summary.compacted_drawers += result.source_ids.len().saturating_sub(1);
                if !dry_run {
                    mark_last_sleep_at(db, &result.source_ids, &sleep_at)?;
                }
            }
        }
    }

    Ok(summary)
}

fn run_rem(
    db: &Database,
    config: &Config,
    project_ids: &[Option<String>],
    dry_run: bool,
) -> Result<RemSummary, SleepError> {
    let now_secs = unix_now_secs() as u64;
    let sleep_at = iso_timestamp();
    let mut summary = RemSummary::default();

    for project_id in project_ids {
        for drawer in active_drawers_for_rem(db, project_id.as_deref())? {
            summary.processed_drawers += 1;
            let report = factcheck::check_with_confidence(
                &drawer.content,
                db,
                now_secs,
                Some((drawer.wing.as_str(), None)),
                drawer.confidence,
            )?;
            for issue in report.issues {
                let FactIssue::RelationContradiction {
                    triple_id,
                    source_drawer,
                    existing_confidence,
                    ..
                } = issue
                else {
                    continue;
                };

                summary.conflicts_detected += 1;
                if !config.sleep.rem_auto_resolve
                    || drawer.confidence <= existing_confidence
                    || !is_newer_than_source(db, &drawer.added_at, source_drawer.as_deref())?
                {
                    continue;
                }

                summary.conflicts_resolved += 1;
                if !dry_run {
                    db.invalidate_triple(&triple_id)?;
                    insert_resolution_log(
                        db,
                        &sleep_at,
                        &drawer,
                        &triple_id,
                        source_drawer.as_deref(),
                        existing_confidence,
                    )?;
                    mark_last_sleep_at(db, std::slice::from_ref(&drawer.id), &sleep_at)?;
                }
            }
        }
    }

    Ok(summary)
}

fn run_salience(
    db: &Database,
    project_ids: &[Option<String>],
    dry_run: bool,
) -> Result<SalienceSummary, SleepError> {
    let now_secs = unix_now_secs();
    let sleep_at = iso_timestamp();
    let mut summary = SalienceSummary::default();

    for project_id in project_ids {
        let candidates = salience_candidates(db, project_id.as_deref())?;
        summary.processed_drawers += candidates.len();

        let updates = candidates
            .into_iter()
            .filter_map(|candidate| {
                let age_days = parse_temporal_timestamp_secs(&candidate.added_at)
                    .map(|added_at| (now_secs - added_at).max(0) as f64 / 86_400.0)?;
                let score = salience_priority(
                    candidate.access_count,
                    candidate.importance,
                    age_days,
                    candidate.has_citations,
                );
                Some((candidate.id, score))
            })
            .collect::<Vec<_>>();

        summary.scored_drawers += updates.len();
        if !dry_run && !updates.is_empty() {
            update_salience_scores(db, &updates, &sleep_at)?;
        }
    }

    Ok(summary)
}

pub fn salience_priority(
    access_count: i64,
    importance: i32,
    age_days: f64,
    has_citations: bool,
) -> f64 {
    let access_component = ((access_count.max(0) as f64) + 1.0).ln() * 4.0;
    let importance_component = f64::from(importance.clamp(0, 5)) * 10.0;
    let recency_component = 5.0 / (1.0 + age_days.max(0.0) / 30.0);
    let citation_component = if has_citations { 2.0 } else { 0.0 };
    importance_component + access_component + recency_component + citation_component
}

fn sleep_project_ids(
    db: &Database,
    explicit_project_id: Option<&str>,
) -> Result<Vec<Option<String>>, DbError> {
    if let Some(project_id) = explicit_project_id {
        return Ok(vec![Some(project_id.to_string())]);
    }

    let mut stmt = db.conn().prepare(
        r#"
        SELECT DISTINCT project_id
        FROM drawers
        WHERE deleted_at IS NULL AND compacted_into IS NULL
        ORDER BY project_id
        "#,
    )?;
    let mut values = stmt
        .query_map([], |row| row.get::<_, Option<String>>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if values.is_empty() {
        values.push(None);
    }
    Ok(values)
}

fn active_wings(db: &Database, project_id: Option<&str>) -> Result<Vec<String>, DbError> {
    let mut stmt = db.conn().prepare(
        r#"
        SELECT DISTINCT wing
        FROM drawers
        WHERE deleted_at IS NULL
          AND compacted_into IS NULL
          AND ((project_id IS NULL AND ?1 IS NULL) OR project_id = ?1)
        ORDER BY wing
        "#,
    )?;
    let values = stmt
        .query_map(params![project_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(values)
}

fn prune_candidates(
    db: &Database,
    project_id: Option<&str>,
    max_importance: i32,
) -> Result<Vec<(String, String)>, DbError> {
    let mut stmt = db.conn().prepare(
        r#"
        SELECT id, added_at
        FROM drawers
        WHERE deleted_at IS NULL
          AND compacted_into IS NULL
          AND COALESCE(is_pinned, 0) = 0
          AND COALESCE(status, '') != 'canonical'
          AND COALESCE(importance, 0) <= ?1
          AND ((project_id IS NULL AND ?2 IS NULL) OR project_id = ?2)
        ORDER BY consolidation_priority DESC NULLS LAST, added_at ASC, id ASC
        "#,
    )?;
    let values = stmt
        .query_map(params![max_importance, project_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(values)
}

fn active_drawers_for_rem(
    db: &Database,
    project_id: Option<&str>,
) -> Result<Vec<DrawerForSleep>, DbError> {
    let mut stmt = db.conn().prepare(
        r#"
        SELECT id, content, wing, confidence, added_at
        FROM drawers
        WHERE deleted_at IS NULL
          AND compacted_into IS NULL
          AND ((project_id IS NULL AND ?1 IS NULL) OR project_id = ?1)
        ORDER BY wing, added_at ASC, id ASC
        "#,
    )?;
    let values = stmt
        .query_map(params![project_id], |row| {
            Ok(DrawerForSleep {
                id: row.get(0)?,
                content: row.get(1)?,
                wing: row.get(2)?,
                confidence: row.get(3)?,
                added_at: row.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(values)
}

fn salience_candidates(
    db: &Database,
    project_id: Option<&str>,
) -> Result<Vec<SalienceCandidate>, DbError> {
    let mut stmt = db.conn().prepare(
        r#"
        SELECT id,
               added_at,
               COALESCE(importance, 0),
               COALESCE(access_count, 0),
               CASE
                   WHEN COALESCE(source_file, '') != '' THEN 1
                   WHEN COALESCE(supporting_refs, '[]') != '[]' THEN 1
                   WHEN COALESCE(verification_refs, '[]') != '[]' THEN 1
                   ELSE 0
               END
        FROM drawers
        WHERE deleted_at IS NULL
          AND compacted_into IS NULL
          AND ((project_id IS NULL AND ?1 IS NULL) OR project_id = ?1)
        ORDER BY added_at DESC, id ASC
        "#,
    )?;
    let values = stmt
        .query_map(params![project_id], |row| {
            Ok(SalienceCandidate {
                id: row.get(0)?,
                added_at: row.get(1)?,
                importance: row.get(2)?,
                access_count: row.get(3)?,
                has_citations: row.get::<_, i64>(4)? == 1,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(values)
}

fn sort_clusters_by_priority(
    db: &Database,
    clusters: &mut [Vec<(String, f64)>],
) -> Result<(), DbError> {
    let ids = clusters
        .iter()
        .flat_map(|cluster| cluster.iter().map(|(drawer_id, _)| drawer_id.clone()))
        .collect::<HashSet<_>>();
    let priorities = load_priorities(db, &ids)?;
    clusters.sort_by(|left, right| {
        let left_priority = cluster_priority(left, &priorities);
        let right_priority = cluster_priority(right, &priorities);
        right_priority
            .partial_cmp(&left_priority)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                average_similarity(right)
                    .partial_cmp(&average_similarity(left))
                    .unwrap_or(Ordering::Equal)
            })
    });
    Ok(())
}

fn load_priorities(db: &Database, ids: &HashSet<String>) -> Result<HashMap<String, f64>, DbError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut priorities = HashMap::new();
    let mut stmt = db
        .conn()
        .prepare("SELECT consolidation_priority FROM drawers WHERE id = ?1")?;
    for id in ids {
        let value = stmt
            .query_row([id.as_str()], |row| row.get::<_, Option<f64>>(0))
            .optional()?
            .flatten()
            .unwrap_or(0.0);
        priorities.insert(id.clone(), value);
    }
    Ok(priorities)
}

fn cluster_priority(cluster: &[(String, f64)], priorities: &HashMap<String, f64>) -> f64 {
    cluster
        .iter()
        .filter_map(|(drawer_id, _)| priorities.get(drawer_id).copied())
        .fold(0.0_f64, f64::max)
}

fn average_similarity(cluster: &[(String, f64)]) -> f64 {
    if cluster.is_empty() {
        return 0.0;
    }
    cluster
        .iter()
        .map(|(_, similarity)| similarity)
        .sum::<f64>()
        / cluster.len() as f64
}

fn soft_delete_drawers_for_sleep(
    db: &Database,
    drawer_ids: &[String],
    sleep_at: &str,
) -> Result<(), DbError> {
    let deleted_at = current_timestamp();
    db.conn().execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<(), DbError> {
        for drawer_id in drawer_ids {
            db.conn().execute(
                r#"
                UPDATE drawers
                SET deleted_at = ?1,
                    valid_until = ?1,
                    last_sleep_at = ?2
                WHERE id = ?3 AND deleted_at IS NULL
                "#,
                params![deleted_at, sleep_at, drawer_id],
            )?;
        }
        Ok(())
    })();
    commit_or_rollback(db, result)
}

fn mark_last_sleep_at(db: &Database, drawer_ids: &[String], sleep_at: &str) -> Result<(), DbError> {
    db.conn().execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<(), DbError> {
        for drawer_id in drawer_ids {
            db.conn().execute(
                "UPDATE drawers SET last_sleep_at = ?1 WHERE id = ?2",
                params![sleep_at, drawer_id],
            )?;
        }
        Ok(())
    })();
    commit_or_rollback(db, result)
}

fn update_salience_scores(
    db: &Database,
    updates: &[(String, f64)],
    sleep_at: &str,
) -> Result<(), DbError> {
    db.conn().execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<(), DbError> {
        for (drawer_id, score) in updates {
            db.conn().execute(
                r#"
                UPDATE drawers
                SET consolidation_priority = ?1,
                    last_sleep_at = ?2
                WHERE id = ?3 AND deleted_at IS NULL
                "#,
                params![score, sleep_at, drawer_id],
            )?;
        }
        Ok(())
    })();
    commit_or_rollback(db, result)
}

fn commit_or_rollback(db: &Database, result: Result<(), DbError>) -> Result<(), DbError> {
    match result {
        Ok(()) => {
            db.conn().execute_batch("COMMIT")?;
            Ok(())
        }
        Err(error) => {
            let _ = db.conn().execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn is_newer_than_source(
    db: &Database,
    new_added_at: &str,
    source_drawer: Option<&str>,
) -> Result<bool, DbError> {
    let Some(source_drawer) = source_drawer else {
        return Ok(false);
    };
    let Some(new_secs) = parse_temporal_timestamp_secs(new_added_at) else {
        return Ok(false);
    };
    let old_added_at = db
        .conn()
        .query_row(
            "SELECT added_at FROM drawers WHERE id = ?1",
            [source_drawer],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(old_added_at) = old_added_at else {
        return Ok(false);
    };
    let Some(old_secs) = parse_temporal_timestamp_secs(&old_added_at) else {
        return Ok(false);
    };
    Ok(new_secs > old_secs)
}

fn insert_resolution_log(
    db: &Database,
    created_at: &str,
    drawer: &DrawerForSleep,
    triple_id: &str,
    source_drawer: Option<&str>,
    existing_confidence: f64,
) -> Result<(), DbError> {
    let id = stable_log_id("sleep_resolution", &[created_at, &drawer.id, triple_id]);
    db.conn().execute(
        r#"
        INSERT OR IGNORE INTO sleep_resolution_log (
            id,
            created_at,
            wing,
            drawer_id,
            contradicted_triple_id,
            contradicted_source_drawer,
            new_confidence,
            existing_confidence,
            action,
            dry_run
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'invalidated', 0)
        "#,
        params![
            id,
            created_at,
            drawer.wing,
            drawer.id,
            triple_id,
            source_drawer,
            drawer.confidence,
            existing_confidence,
        ],
    )?;
    Ok(())
}

fn insert_sleep_log(
    db: &Database,
    phase_label: &str,
    summary: &SleepCycleSummary,
) -> Result<(), DbError> {
    let created_at = iso_timestamp();
    let unique_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string());
    let id = stable_log_id("sleep", &[&created_at, phase_label, &unique_nanos]);
    db.conn().execute(
        r#"
        INSERT INTO sleep_log (
            id,
            created_at,
            phase,
            processed_count,
            pruned_count,
            compacted_count,
            conflicts_resolved_count,
            salience_scored_count,
            dry_run
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)
        "#,
        params![
            id,
            created_at,
            phase_label,
            i64::try_from(summary.processed_count()).unwrap_or(i64::MAX),
            i64::try_from(summary.pruned_count()).unwrap_or(i64::MAX),
            i64::try_from(summary.compacted_count()).unwrap_or(i64::MAX),
            i64::try_from(summary.conflicts_resolved_count()).unwrap_or(i64::MAX),
            i64::try_from(summary.salience_scored_count()).unwrap_or(i64::MAX),
        ],
    )?;
    Ok(())
}

fn stable_log_id(prefix: &str, parts: &[&str]) -> String {
    let mut seed = String::new();
    for part in parts {
        seed.push_str(part);
        seed.push('\0');
    }
    let digest = blake3::hash(seed.as_bytes()).to_hex().to_string();
    format!("{prefix}_{}", &digest[..16])
}

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::Database;
    use crate::core::types::{
        BootstrapEvidenceArgs, Drawer, KnowledgeStatus, SourceType, Triple, default_confidence,
    };

    fn drawer(id: &str, content: &str, importance: i32, added_at: &str) -> Drawer {
        Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
            id: id.to_string(),
            content: content.to_string(),
            wing: "memory".to_string(),
            room: Some("facts".to_string()),
            source_file: Some(format!("{id}.md")),
            source_type: SourceType::AgentInference,
            added_at: added_at.to_string(),
            chunk_index: Some(0),
            importance,
        })
    }

    fn db() -> (tempfile::TempDir, Database) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        (tempdir, db)
    }

    fn nrem_options() -> SleepRunOptions {
        SleepRunOptions {
            phases: SleepPhaseSelection {
                nrem: true,
                rem: false,
                salience: false,
            },
            dry_run: false,
            project_id: None,
        }
    }

    #[test]
    fn test_nrem_prunes_low_importance() {
        let (_tmp, db) = db();
        let config = Config::default();
        db.insert_drawer(&drawer("old-low", "old low", 1, "2020-01-01T00:00:00Z"))
            .expect("insert old");
        db.insert_drawer(&drawer(
            "old-important",
            "old important",
            4,
            "2020-01-01T00:00:00Z",
        ))
        .expect("insert important");

        let summary = run_sleep_cycle(&db, &config, nrem_options()).expect("sleep");

        assert_eq!(summary.nrem.expect("nrem").pruned_drawers, 1);
        assert!(db.get_drawer("old-low").expect("load old").is_none());
        assert!(
            db.get_drawer("old-important")
                .expect("load important")
                .is_some()
        );
    }

    #[test]
    fn test_nrem_skips_pinned() {
        let (_tmp, db) = db();
        let config = Config::default();
        let mut pinned = drawer("pinned", "old pinned", 1, "2020-01-01T00:00:00Z");
        pinned.is_pinned = true;
        pinned.status = Some(KnowledgeStatus::Canonical);
        db.insert_drawer(&pinned).expect("insert pinned");

        let summary = run_sleep_cycle(&db, &config, nrem_options()).expect("sleep");

        assert_eq!(summary.nrem.expect("nrem").pruned_drawers, 0);
        assert!(db.get_drawer("pinned").expect("load pinned").is_some());
    }

    #[test]
    fn test_rem_resolves_contradiction_by_confidence() {
        let (_tmp, db) = db();
        let config = Config::default();
        let old = drawer(
            "old-fact",
            "Bob is Alice's brother",
            3,
            "2020-01-01T00:00:00Z",
        );
        db.insert_drawer(&old).expect("insert old");
        db.insert_triple(&Triple {
            id: "triple-bob-alice".to_string(),
            subject: "Bob".to_string(),
            predicate: "brother_of".to_string(),
            object: "Alice".to_string(),
            valid_from: Some("2020-01-01T00:00:00Z".to_string()),
            valid_to: None,
            confidence: default_confidence(SourceType::AgentInference),
            source_drawer: Some("old-fact".to_string()),
        })
        .expect("insert triple");
        let mut new = drawer(
            "new-fact",
            "Bob is Alice's husband",
            4,
            "2026-01-01T00:00:00Z",
        );
        new.confidence = 0.9;
        db.insert_drawer(&new).expect("insert new");

        let summary = run_sleep_cycle(
            &db,
            &config,
            SleepRunOptions {
                phases: SleepPhaseSelection {
                    nrem: false,
                    rem: true,
                    salience: false,
                },
                dry_run: false,
                project_id: None,
            },
        )
        .expect("sleep");

        assert_eq!(summary.rem.expect("rem").conflicts_resolved, 1);
        let valid_to: Option<String> = db
            .conn()
            .query_row(
                "SELECT valid_to FROM triples WHERE id = 'triple-bob-alice'",
                [],
                |row| row.get(0),
            )
            .expect("valid_to");
        assert!(valid_to.is_some());
    }

    #[test]
    fn test_salience_scoring_formula() {
        let score = salience_priority(9, 3, 15.0, true);
        let expected = 30.0 + 10.0_f64.ln() * 4.0 + 5.0 / 1.5 + 2.0;
        assert!((score - expected).abs() < 0.000_001);
    }

    #[test]
    fn test_full_sleep_cycle() {
        let (_tmp, db) = db();
        let mut config = Config::default();
        config.sleep.nrem_compaction_threshold = 0.95;
        config.consolidation.min_cluster_size = 3;

        db.insert_drawer(&drawer("old-low", "old low", 1, "2020-01-01T00:00:00Z"))
            .expect("insert old");
        for (id, vector) in [
            ("cluster-a", vec![1.0_f32, 0.0, 0.0]),
            ("cluster-b", vec![0.99_f32, 0.01, 0.0]),
            ("cluster-c", vec![0.98_f32, 0.02, 0.0]),
        ] {
            let d = drawer(id, "cluster content", 3, "2026-01-01T00:00:00Z");
            db.insert_drawer(&d).expect("insert cluster drawer");
            db.insert_vector(&d.id, &vector).expect("insert vector");
        }
        let old = drawer(
            "old-fact",
            "Bob is Alice's brother",
            3,
            "2020-01-01T00:00:00Z",
        );
        db.insert_drawer(&old).expect("insert old fact");
        db.insert_triple(&Triple {
            id: "triple-full".to_string(),
            subject: "Bob".to_string(),
            predicate: "brother_of".to_string(),
            object: "Alice".to_string(),
            valid_from: Some("2020-01-01T00:00:00Z".to_string()),
            valid_to: None,
            confidence: 0.5,
            source_drawer: Some("old-fact".to_string()),
        })
        .expect("insert triple");
        let mut new = drawer(
            "new-fact",
            "Bob is Alice's husband",
            4,
            "2026-01-01T00:00:00Z",
        );
        new.confidence = 0.9;
        db.insert_drawer(&new).expect("insert new fact");

        let summary = run_sleep_cycle(&db, &config, SleepRunOptions::default()).expect("sleep");

        assert_eq!(summary.phases, SleepPhaseSelection::all().selected_or_all());
        assert_eq!(summary.nrem.expect("nrem").pruned_drawers, 1);
        assert_eq!(summary.rem.expect("rem").conflicts_resolved, 1);
        assert!(
            summary
                .salience
                .expect("salience")
                .scored_drawers
                .saturating_sub(1)
                > 0
        );
    }

    #[test]
    fn test_dry_run_no_mutations() {
        let (_tmp, db) = db();
        let config = Config::default();
        db.insert_drawer(&drawer("old-low", "old low", 1, "2020-01-01T00:00:00Z"))
            .expect("insert old");

        let summary = run_sleep_cycle(
            &db,
            &config,
            SleepRunOptions {
                dry_run: true,
                ..SleepRunOptions::default()
            },
        )
        .expect("sleep");

        assert!(summary.dry_run);
        assert!(db.get_drawer("old-low").expect("load old").is_some());
        let priority: Option<f64> = db
            .conn()
            .query_row(
                "SELECT consolidation_priority FROM drawers WHERE id = 'old-low'",
                [],
                |row| row.get(0),
            )
            .expect("priority");
        assert!(priority.is_none());
        assert_eq!(db.sleep_stats().expect("sleep stats").last_sleep_at, None);
    }
}
