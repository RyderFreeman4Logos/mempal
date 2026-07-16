//! P14 anti-pattern detection and repair warning injection.
//!
//! Heuristic pipeline (no LLM):
//!   1. Ingest fires `spawn_failure_detection` after each drawer insert.
//!   2. A keyword regex scan flags content as a failure event.
//!   3. topic_sig = sha256(top-5 TF-IDF words sorted, joined by "|")[:32]
//!   4. When ≥ min_failures share the same topic_sig within window_days,
//!      a RepairPackage is assembled with failure drawers + success drawers.
//!   5. mempal_fact_check returns RepairPackage(s) as RepeatedFailurePattern.
//!   6. mempal_context injects repair_warnings for active patterns.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use regex::Regex;
use rmcp::schemars::{self, JsonSchema};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::core::config::RepairConfig;

/// Built-in failure keywords (case-insensitive word-boundary match).
pub const BUILTIN_FAILURE_KEYWORDS: &[&str] = &[
    "error",
    "failed",
    "failure",
    "reverted",
    "rolled back",
    "exception",
    "panic",
    "aborted",
    "wrong",
    "mistake",
    "incorrect",
];

/// Lightweight preview of a drawer included in a RepairPackage.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DrawerPreview {
    pub drawer_id: String,
    pub preview: String,
}

/// Structured evidence package for a repeated failure pattern.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RepairPackage {
    pub topic_sig: String,
    pub failure_count: usize,
    pub window_days: u64,
    pub failure_drawers: Vec<DrawerPreview>,
    pub success_drawers: Vec<DrawerPreview>,
}

/// Warning injected into mempal_context when active anti-patterns exist.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RepairWarning {
    pub severity: String,
    pub message: String,
    pub topic_sig: String,
}

// ---------------------------------------------------------------------------
// Topic signature
// ---------------------------------------------------------------------------

/// Compute topic_sig from drawer content.
///
/// Algorithm: compute TF scores for each word (lowercased, alpha-only), take
/// top-5 by TF (alphabetically as tiebreak), sort them, join with `|`, hash
/// with SHA-256, return the first 32 hex characters.
pub fn compute_topic_sig(content: &str) -> String {
    let word_re = Regex::new(r"[a-zA-Z]{3,}").expect("static regex");
    let stopwords: std::collections::HashSet<&str> = [
        "the", "and", "for", "are", "was", "with", "this", "that", "from", "have", "had", "been",
        "has", "its", "not", "but", "all", "can", "will", "when", "they", "their", "our", "into",
        "also", "more", "which", "there", "any", "one", "each",
    ]
    .iter()
    .copied()
    .collect();

    let mut tf: HashMap<String, usize> = HashMap::new();
    for word in word_re.find_iter(content) {
        let w = word.as_str().to_lowercase();
        if !stopwords.contains(w.as_str()) {
            *tf.entry(w).or_insert(0) += 1;
        }
    }

    let mut pairs: Vec<(usize, String)> =
        tf.into_iter().map(|(word, count)| (count, word)).collect();
    // Sort descending by count, then ascending by word for stable tiebreak.
    pairs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let top5: Vec<String> = pairs.into_iter().take(5).map(|(_, w)| w).collect();
    let mut sorted = top5.clone();
    sorted.sort();
    let joined = sorted.join("|");

    let mut hasher = Sha256::new();
    hasher.update(joined.as_bytes());
    let hash = hasher.finalize();
    hash[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

// ---------------------------------------------------------------------------
// Keyword detection
// ---------------------------------------------------------------------------

/// Check content for built-in + configured failure keywords.
/// Returns the matched keyword as the failure_type string, or None.
pub fn detect_failure_keyword(content: &str, extra_keywords: &[String]) -> Option<String> {
    let combined: Vec<&str> = BUILTIN_FAILURE_KEYWORDS
        .iter()
        .copied()
        .chain(extra_keywords.iter().map(String::as_str))
        .collect();

    for kw in &combined {
        let pattern = format!(r"(?i)\b{}\b", regex::escape(kw));
        if let Ok(re) = Regex::new(&pattern) {
            if re.is_match(content) {
                return Some(kw.to_lowercase().replace(' ', "_"));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// DB writes
// ---------------------------------------------------------------------------

/// Arguments for recording a single failure event.
pub struct FailureEventArgs<'a> {
    pub event_id: &'a str,
    pub drawer_id: &'a str,
    pub wing: &'a str,
    pub room: Option<&'a str>,
    pub topic_sig: &'a str,
    pub failure_type: &'a str,
    pub project_id: Option<&'a str>,
    pub detected_at_ms: i64,
}

struct DetectedFailure {
    event_id: String,
    topic_sig: String,
    failure_type: String,
    detected_at_ms: i64,
}

/// Insert a failure_event row. Uses unix epoch milliseconds for detected_at.
pub fn record_failure_event(
    conn: &Connection,
    args: &FailureEventArgs<'_>,
) -> rusqlite::Result<()> {
    conn.execute(
        r#"
        INSERT OR IGNORE INTO failure_events
            (event_id, drawer_id, wing, room, topic_sig, failure_type, detected_at, project_id)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        "#,
        params![
            args.event_id,
            args.drawer_id,
            args.wing,
            args.room,
            args.topic_sig,
            args.failure_type,
            args.detected_at_ms,
            args.project_id,
        ],
    )?;
    Ok(())
}

/// Synchronous failure detection: check content for failure keywords, record
/// event if found.  Opens a fresh DB connection so it can be called from any
/// context (sync or async, any thread).
pub fn try_record_failure(
    db_path: &Path,
    drawer_id: &str,
    content: &str,
    wing: &str,
    room: Option<&str>,
    project_id: Option<&str>,
    config: &RepairConfig,
) {
    let Some(detected) = detect_failure(content, config) else {
        return;
    };
    let args = failure_event_args(&detected, drawer_id, wing, room, project_id);
    if let Err(e) = write_failure_event_to_path(db_path, &args) {
        tracing::warn!(error = %e, drawer_id, "failure event write failed");
    }
}

/// Detect and record a failure using the caller's SQLite connection.
///
/// Runtime writers use this entry point inside their generation-fenced write
/// transaction so lease validation and the optional signal insert share one
/// SQLite write lock.
pub fn try_record_failure_on_connection(
    conn: &Connection,
    drawer_id: &str,
    content: &str,
    wing: &str,
    room: Option<&str>,
    project_id: Option<&str>,
    config: &RepairConfig,
) -> rusqlite::Result<()> {
    let Some(detected) = detect_failure(content, config) else {
        return Ok(());
    };
    let args = failure_event_args(&detected, drawer_id, wing, room, project_id);
    record_failure_event(conn, &args)
}

fn detect_failure(content: &str, config: &RepairConfig) -> Option<DetectedFailure> {
    let failure_type = detect_failure_keyword(content, &config.failure_keywords)?;
    let detected_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    Some(DetectedFailure {
        event_id: new_event_id(),
        topic_sig: compute_topic_sig(content),
        failure_type,
        detected_at_ms,
    })
}

fn failure_event_args<'a>(
    detected: &'a DetectedFailure,
    drawer_id: &'a str,
    wing: &'a str,
    room: Option<&'a str>,
    project_id: Option<&'a str>,
) -> FailureEventArgs<'a> {
    FailureEventArgs {
        event_id: &detected.event_id,
        drawer_id,
        wing,
        room,
        topic_sig: &detected.topic_sig,
        failure_type: &detected.failure_type,
        project_id,
        detected_at_ms: detected.detected_at_ms,
    }
}

/// Fire-and-forget async failure detection after an ingest (MCP / long-lived
/// runtime).  Spawns a tokio task that calls [`try_record_failure`].
pub fn spawn_failure_detection(
    db_path: PathBuf,
    drawer_id: String,
    content: String,
    wing: String,
    room: Option<String>,
    project_id: Option<String>,
    config: RepairConfig,
) {
    tokio::spawn(async move {
        try_record_failure(
            &db_path,
            &drawer_id,
            &content,
            &wing,
            room.as_deref(),
            project_id.as_deref(),
            &config,
        );
    });
}

/// Open a fresh connection and write a failure event.
fn write_failure_event_to_path(db_path: &Path, args: &FailureEventArgs<'_>) -> anyhow::Result<()> {
    let admitted = crate::core::db_connection::AdmittedSqliteConnection::open_default(db_path)?;
    let conn = admitted.connection();
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    record_failure_event(conn, args)?;
    Ok(())
}

fn new_event_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Simple deterministic UUID-like ID using time + thread-local counter.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{ts:032x}{seq:016x}")
}

// ---------------------------------------------------------------------------
// Pattern detection
// ---------------------------------------------------------------------------

/// Detect repeated failure patterns within the given window.
///
/// Returns one RepairPackage per topic_sig that has ≥ min_failures events
/// within window_days ending at now_ms.
pub fn detect_repeated_failures(
    conn: &Connection,
    config: &RepairConfig,
    project_id: Option<&str>,
    now_ms: i64,
) -> Vec<RepairPackage> {
    if !config.enabled {
        return vec![];
    }

    let window_start_ms = now_ms - (config.window_days as i64) * 86_400_000;
    let min_failures = config.min_failures as i64;

    // Query for topic_sigs with enough failure events.
    let sigs = match query_pattern_sigs(conn, window_start_ms, min_failures, project_id) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "failed to query failure pattern sigs");
            return vec![];
        }
    };

    let mut packages = Vec::new();
    for (topic_sig, count) in sigs {
        let pkg = assemble_repair_package(
            conn,
            &topic_sig,
            count as usize,
            config.window_days,
            window_start_ms,
        );
        packages.push(pkg);
    }
    packages
}

fn query_pattern_sigs(
    conn: &Connection,
    window_start_ms: i64,
    min_failures: i64,
    project_id: Option<&str>,
) -> rusqlite::Result<Vec<(String, i64)>> {
    // Check if the table exists first.
    let table_exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='failure_events'",
        [],
        |row| row.get(0),
    )?;
    if table_exists == 0 {
        return Ok(vec![]);
    }

    let mut stmt = if project_id.is_some() {
        conn.prepare(
            r#"
            SELECT topic_sig, COUNT(*) AS cnt
            FROM failure_events
            WHERE detected_at >= ?1
              AND (project_id = ?3 OR project_id IS NULL)
            GROUP BY topic_sig
            HAVING cnt >= ?2
            ORDER BY cnt DESC
            "#,
        )?
    } else {
        conn.prepare(
            r#"
            SELECT topic_sig, COUNT(*) AS cnt
            FROM failure_events
            WHERE detected_at >= ?1
            GROUP BY topic_sig
            HAVING cnt >= ?2
            ORDER BY cnt DESC
            "#,
        )?
    };

    let rows = if let Some(pid) = project_id {
        stmt.query_map(params![window_start_ms, min_failures, pid], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        stmt.query_map(params![window_start_ms, min_failures], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    Ok(rows)
}

/// Build a RepairPackage for a given topic_sig.
pub fn assemble_repair_package(
    conn: &Connection,
    topic_sig: &str,
    failure_count: usize,
    window_days: u64,
    window_start_ms: i64,
) -> RepairPackage {
    let failure_drawers =
        fetch_failure_drawers(conn, topic_sig, window_start_ms, 5).unwrap_or_default();
    let success_drawers =
        fetch_success_drawers(conn, topic_sig, window_start_ms, 5).unwrap_or_default();
    RepairPackage {
        topic_sig: topic_sig.to_string(),
        failure_count,
        window_days,
        failure_drawers,
        success_drawers,
    }
}

fn fetch_failure_drawers(
    conn: &Connection,
    topic_sig: &str,
    window_start_ms: i64,
    limit: i64,
) -> rusqlite::Result<Vec<DrawerPreview>> {
    let table_exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='failure_events'",
        [],
        |row| row.get(0),
    )?;
    if table_exists == 0 {
        return Ok(vec![]);
    }

    let mut stmt = conn.prepare(
        r#"
        SELECT d.id, d.content
        FROM drawers d
        INNER JOIN failure_events fe ON fe.drawer_id = d.id
        WHERE fe.topic_sig = ?1
          AND fe.detected_at >= ?2
          AND (d.deleted_at IS NULL OR d.deleted_at = '')
        GROUP BY d.id
        ORDER BY fe.detected_at DESC
        LIMIT ?3
        "#,
    )?;

    let rows = stmt
        .query_map(params![topic_sig, window_start_ms, limit], |row| {
            let id: String = row.get(0)?;
            let content: String = row.get(1)?;
            let preview: String = content.chars().take(200).collect();
            Ok(DrawerPreview {
                drawer_id: id,
                preview,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Find drawers in the same wing/room that are semantically related but
/// contain no failure keywords — these act as positive exemplars.
fn fetch_success_drawers(
    conn: &Connection,
    topic_sig: &str,
    window_start_ms: i64,
    limit: i64,
) -> rusqlite::Result<Vec<DrawerPreview>> {
    // Get wing/room from the failure events.
    let table_exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='failure_events'",
        [],
        |row| row.get(0),
    )?;
    if table_exists == 0 {
        return Ok(vec![]);
    }

    let wing_room: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT wing, room FROM failure_events WHERE topic_sig = ?1 LIMIT 1",
            params![topic_sig],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;

    let Some((wing, room)) = wing_room else {
        return Ok(vec![]);
    };

    // Get failure drawer IDs to exclude.
    let mut excl_stmt = conn.prepare(
        "SELECT DISTINCT drawer_id FROM failure_events WHERE topic_sig = ?1 AND detected_at >= ?2",
    )?;
    let excluded_ids: Vec<String> = excl_stmt
        .query_map(params![topic_sig, window_start_ms], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // Build NOT IN clause.
    if excluded_ids.is_empty() {
        return Ok(vec![]);
    }
    let placeholders: Vec<String> = (1..=excluded_ids.len())
        .map(|i| format!("?{}", i + 3))
        .collect();
    let not_in = placeholders.join(", ");

    let sql = format!(
        r#"
        SELECT id, content FROM drawers
        WHERE wing = ?1
          AND (room = ?2 OR room IS NULL)
          AND (deleted_at IS NULL OR deleted_at = '')
          AND id NOT IN ({not_in})
        ORDER BY ROWID DESC
        LIMIT ?3
        "#
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut param_values: Vec<rusqlite::types::Value> = vec![
        rusqlite::types::Value::Text(wing),
        match room {
            Some(r) => rusqlite::types::Value::Text(r),
            None => rusqlite::types::Value::Null,
        },
        rusqlite::types::Value::Integer(limit),
    ];
    for id in &excluded_ids {
        param_values.push(rusqlite::types::Value::Text(id.clone()));
    }

    let rows = stmt
        .query_map(rusqlite::params_from_iter(param_values), |row| {
            let id: String = row.get(0)?;
            let content: String = row.get(1)?;
            Ok((id, content))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // Filter out rows that contain failure keywords.
    let extra: &[String] = &[];
    let success: Vec<DrawerPreview> = rows
        .into_iter()
        .filter(|(_, content)| detect_failure_keyword(content, extra).is_none())
        .map(|(id, content)| DrawerPreview {
            drawer_id: id,
            preview: content.chars().take(200).collect(),
        })
        .take(limit as usize)
        .collect();
    Ok(success)
}

// ---------------------------------------------------------------------------
// Repair warnings for context injection
// ---------------------------------------------------------------------------

/// Load active repair warnings to inject into mempal_context.
pub fn load_repair_warnings(
    conn: &Connection,
    config: &RepairConfig,
    project_id: Option<&str>,
    now_ms: i64,
) -> Vec<RepairWarning> {
    if !config.enabled {
        return vec![];
    }

    let window_start_ms = now_ms - (config.window_days as i64) * 86_400_000;
    let threshold = config.alert_threshold as i64;

    let table_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='failure_events'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if table_exists == 0 {
        return vec![];
    }

    let sigs = if let Some(pid) = project_id {
        conn.prepare(
            r#"
            SELECT fe.topic_sig, COUNT(*) AS cnt, MIN(fe.wing) as wing, MIN(fe.room) as room
            FROM failure_events fe
            WHERE fe.detected_at >= ?1
              AND (fe.project_id = ?3 OR fe.project_id IS NULL)
            GROUP BY fe.topic_sig
            HAVING cnt >= ?2
            ORDER BY cnt DESC
            "#,
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![window_start_ms, threshold, pid], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
        })
        .unwrap_or_default()
    } else {
        conn.prepare(
            r#"
            SELECT fe.topic_sig, COUNT(*) AS cnt, MIN(fe.wing) as wing, MIN(fe.room) as room
            FROM failure_events fe
            WHERE fe.detected_at >= ?1
            GROUP BY fe.topic_sig
            HAVING cnt >= ?2
            ORDER BY cnt DESC
            "#,
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![window_start_ms, threshold], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
        })
        .unwrap_or_default()
    };

    sigs.into_iter()
        .map(|(topic_sig, cnt, wing, room)| {
            let location = match room {
                Some(r) => format!("wing={wing} room={r}"),
                None => format!("wing={wing}"),
            };
            RepairWarning {
                severity: "warn".to_string(),
                message: format!(
                    "Repeated failure pattern detected in {location}: topic_sig={topic_sig} count={cnt}"
                ),
                topic_sig,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Unit tests (sync, no async runtime needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_sig_is_32_hex_chars() {
        let sig = compute_topic_sig("the deployment failed with a database error");
        assert_eq!(sig.len(), 32, "topic_sig must be 32 hex chars");
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn topic_sig_deterministic() {
        let a = compute_topic_sig("migration failed with SQLITE_ERROR");
        let b = compute_topic_sig("migration failed with SQLITE_ERROR");
        assert_eq!(a, b);
    }

    #[test]
    fn detect_failure_keyword_builtin() {
        assert!(detect_failure_keyword("the migration failed with an error", &[]).is_some());
        assert!(detect_failure_keyword("process was aborted unexpectedly", &[]).is_some());
        assert!(detect_failure_keyword("everything worked fine", &[]).is_none());
    }

    #[test]
    fn detect_failure_keyword_case_insensitive() {
        assert!(detect_failure_keyword("Deploy FAILED due to timeout", &[]).is_some());
        assert!(detect_failure_keyword("System ERROR encountered", &[]).is_some());
    }

    #[test]
    fn detect_failure_keyword_word_boundary() {
        // "failed" should not match "unfailing"
        assert!(detect_failure_keyword("unfailing service works fine", &[]).is_none());
    }

    #[test]
    fn detect_failure_keyword_custom() {
        let extra = vec!["crash".to_string()];
        // "crash" with word boundary matches standalone "crash" not "crashed".
        assert!(detect_failure_keyword("the system had a crash today", &extra).is_some());
        assert!(detect_failure_keyword("no issues here", &extra).is_none());
    }
}
