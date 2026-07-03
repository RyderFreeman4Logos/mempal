//! P109: cross-project resume.
//!
//! mempal keeps project memory in one global `palace.db`, keyed by `wing`, with
//! the absolute worktree path stored in `worktree://` anchors. This module
//! exposes deterministic, embedder-free read helpers so agents can list known
//! projects and resume one by fuzzy name from any directory.

use std::collections::HashMap;
use std::path::Path;

use rmcp::schemars::{self, JsonSchema};
use serde::Serialize;

use crate::core::db::{Database, DbError};

/// Maximum number of evidence/candidate rows a single resume call can return.
const MAX_RESUME_LIMIT: usize = 100;

/// Shell-quote a string for safe interpolation into a shell command.
///
/// Uses POSIX single-quote escaping: wraps in single quotes and escapes
/// any embedded single quotes. This prevents shell injection from
/// drawer-controlled data (worktree paths, wing names).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Clamp a caller-supplied limit to a safe maximum and convert to i64.
fn sanitize_limit(limit: Option<usize>) -> i64 {
    let capped = limit.unwrap_or(20).min(MAX_RESUME_LIMIT);
    i64::try_from(capped).unwrap_or(MAX_RESUME_LIMIT as i64)
}

const WORKTREE_PREFIX: &str = "worktree://";

/// One project's summary, derived from active drawers in a wing.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProjectSummary {
    pub wing: String,
    /// Absolute worktree path from the latest `worktree://` anchor, when known.
    pub path: Option<String>,
    /// Epoch-seconds string of the most recent drawer in this wing.
    pub last_activity: String,
    pub total: i64,
    pub evidence: i64,
    pub knowledge: i64,
}

/// A recent evidence drawer included in a resume pack.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ResumeEvidence {
    pub drawer_id: String,
    pub source_file: Option<String>,
    pub snippet: String,
    pub added_at: String,
    pub importance: i64,
}

/// Candidate knowledge that has not yet been promoted.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ResumeCandidate {
    pub drawer_id: String,
    pub statement: Option<String>,
    pub tier: Option<String>,
}

/// Everything needed to pick a project back up.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ResumePack {
    pub wing: String,
    pub path: Option<String>,
    pub last_activity: String,
    pub total: i64,
    pub evidence: i64,
    pub knowledge: i64,
    pub recent_evidence: Vec<ResumeEvidence>,
    pub in_flight: Vec<ResumeCandidate>,
    /// Concrete next step for the caller to execute outside mempal.
    pub next_step: String,
}

/// Outcome of resolving a fuzzy resume query.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "resolution", rename_all = "snake_case")]
pub enum ResumeResolution {
    Resolved(Box<ResumePack>),
    Ambiguous {
        query: String,
        candidates: Vec<ProjectSummary>,
    },
    NotFound {
        query: String,
        available: Vec<String>,
    },
}

/// List every active project wing mempal knows, newest activity first.
pub fn list_projects(db: &Database) -> Result<Vec<ProjectSummary>, DbError> {
    let mut statement = db.conn().prepare(
        r#"
        SELECT wing,
               COUNT(*) AS total,
               SUM(CASE WHEN memory_kind = 'evidence' THEN 1 ELSE 0 END) AS evidence,
               SUM(CASE WHEN memory_kind = 'knowledge' THEN 1 ELSE 0 END) AS knowledge,
               MAX(added_at) AS last_activity
        FROM drawers
        WHERE deleted_at IS NULL
        GROUP BY wing
        "#,
    )?;
    let mut summaries = statement
        .query_map([], |row| {
            Ok(ProjectSummary {
                wing: row.get(0)?,
                path: None,
                total: row.get(1)?,
                evidence: row.get(2)?,
                knowledge: row.get(3)?,
                last_activity: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut paths = db.conn().prepare(
        r#"
        SELECT wing, anchor_id
        FROM drawers
        WHERE deleted_at IS NULL
          AND anchor_kind = 'worktree'
          AND anchor_id LIKE 'worktree://%'
        ORDER BY added_at DESC, rowid DESC
        "#,
    )?;
    let path_map: HashMap<String, String> = paths
        .query_map([], |row| {
            let wing: String = row.get(0)?;
            let anchor: String = row.get(1)?;
            Ok((wing, anchor))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|(wing, anchor)| {
            anchor
                .strip_prefix(WORKTREE_PREFIX)
                .map(|path| (wing, path.to_string()))
        })
        .fold(HashMap::new(), |mut map, (wing, path)| {
            map.entry(wing).or_insert(path);
            map
        });

    for summary in &mut summaries {
        summary.path = path_map.get(&summary.wing).cloned();
    }

    summaries.sort_by(|a, b| {
        b.last_activity
            .parse::<i64>()
            .unwrap_or(0)
            .cmp(&a.last_activity.parse::<i64>().unwrap_or(0))
            .then_with(|| a.wing.cmp(&b.wing))
    });
    Ok(summaries)
}

/// Resolve a fuzzy project name and build a resume pack for a unique match.
pub fn resume_project(
    db: &Database,
    query: &str,
    evidence_limit: usize,
    candidate_limit: usize,
) -> Result<ResumeResolution, DbError> {
    let needle = query.trim().to_lowercase();
    let projects = list_projects(db)?;
    let available = || projects.iter().map(|p| p.wing.clone()).collect::<Vec<_>>();

    if needle.is_empty() {
        return Ok(ResumeResolution::NotFound {
            query: query.to_string(),
            available: available(),
        });
    }

    let exact = projects
        .iter()
        .filter(|p| p.wing.to_lowercase() == needle)
        .collect::<Vec<_>>();
    let matches = if exact.is_empty() {
        projects
            .iter()
            .filter(|p| {
                p.wing.to_lowercase().contains(&needle) || path_basename_matches(p, &needle)
            })
            .collect::<Vec<_>>()
    } else {
        exact
    };

    match matches.len() {
        0 => Ok(ResumeResolution::NotFound {
            query: query.to_string(),
            available: available(),
        }),
        1 => {
            let project = matches[0];
            let recent_evidence = recent_evidence(db, &project.wing, evidence_limit)?;
            let in_flight = candidate_knowledge(db, &project.wing, candidate_limit)?;
            let next_step = match project.path.as_deref() {
                // Build the shell command with each drawer-controlled value
                // safely quoted. The wing goes inside the outer single-quote
                // pair for the resume argument, not nested in double quotes.
                Some(path) => {
                    let quoted_path = shell_quote(path);
                    let quoted_wing = shell_quote(&project.wing);
                    format!("cd {quoted_path} && mempal context {quoted_wing}")
                }
                None => format!(
                    "project '{}' has no recorded worktree path; supply the path, then run mempal context",
                    project.wing
                ),
            };
            Ok(ResumeResolution::Resolved(Box::new(ResumePack {
                wing: project.wing.clone(),
                path: project.path.clone(),
                last_activity: project.last_activity.clone(),
                total: project.total,
                evidence: project.evidence,
                knowledge: project.knowledge,
                recent_evidence,
                in_flight,
                next_step,
            })))
        }
        _ => Ok(ResumeResolution::Ambiguous {
            query: query.to_string(),
            candidates: matches.into_iter().cloned().collect(),
        }),
    }
}

fn path_basename_matches(project: &ProjectSummary, needle: &str) -> bool {
    project
        .path
        .as_deref()
        .and_then(|path| Path::new(path).file_name())
        .and_then(|name| name.to_str())
        .map(|name| name.to_lowercase().contains(needle))
        .unwrap_or(false)
}

fn recent_evidence(
    db: &Database,
    wing: &str,
    limit: usize,
) -> Result<Vec<ResumeEvidence>, DbError> {
    let safe_limit = sanitize_limit(Some(limit));
    let mut statement = db.conn().prepare(
        r#"
        SELECT id, source_file, substr(content, 1, 200), added_at, importance
        FROM drawers
        WHERE deleted_at IS NULL
          AND wing = ?1
          AND memory_kind = 'evidence'
        ORDER BY added_at DESC, importance DESC, rowid DESC
        LIMIT ?2
        "#,
    )?;
    let rows = statement
        .query_map(rusqlite::params![wing, safe_limit], |row| {
            Ok(ResumeEvidence {
                drawer_id: row.get(0)?,
                source_file: row.get(1)?,
                snippet: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                added_at: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                importance: i64::from(row.get::<_, Option<i32>>(4)?.unwrap_or(0)),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn candidate_knowledge(
    db: &Database,
    wing: &str,
    limit: usize,
) -> Result<Vec<ResumeCandidate>, DbError> {
    let safe_limit = sanitize_limit(Some(limit));
    let mut statement = db.conn().prepare(
        r#"
        SELECT id, statement, tier
        FROM drawers
        WHERE deleted_at IS NULL
          AND wing = ?1
          AND memory_kind = 'knowledge'
          AND status = 'candidate'
        ORDER BY added_at DESC, rowid DESC
        LIMIT ?2
        "#,
    )?;
    let rows = statement
        .query_map(rusqlite::params![wing, safe_limit], |row| {
            Ok(ResumeCandidate {
                drawer_id: row.get(0)?,
                statement: row.get(1)?,
                tier: row.get(2)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}
