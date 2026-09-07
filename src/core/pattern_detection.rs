use super::{
    NewPattern, PATTERNS_SCHEMA_MIN_FORK_EXT_VERSION, compute_centroid, extract_topic_tags,
    fetch_embeddings_for_drawers, find_pattern_for_exemplars, insert_pattern,
    update_pattern_with_exemplar,
};
use rusqlite::{Connection, OptionalExtension, params};

/// Arguments for `run_pattern_detection`.
pub struct PatternDetectionArgs<'a> {
    /// The newly inserted drawer's ID.
    pub new_drawer_id: &'a str,
    /// The source_file of the new drawer (used as session proxy).
    pub session_id: &'a str,
    /// The embedding vector of the new drawer.
    pub embedding: &'a [f32],
    /// Optional project scope.
    pub project_id: Option<&'a str>,
    /// Model identifier string for the current embedder.
    pub model_id: &'a str,
    /// Similarity threshold for pattern candidate detection.
    pub similarity_threshold: f64,
    /// Minimum distinct sessions to form a candidate.
    pub min_sessions: usize,
    /// Minimum exemplar count to form a candidate.
    pub min_exemplars: usize,
    /// Session count threshold to auto-promote to active.
    pub promote_threshold: usize,
    /// Top-N tags to extract.
    pub top_tags: usize,
}

pub(crate) enum PatternDetectionPlan {
    None,
    Update(PatternRevision),
    Insert {
        pattern: NewPattern,
        overlapping_exemplar_ids: Vec<String>,
    },
}

pub(crate) struct PatternRevision {
    pattern_id: String,
    exemplar_ids: String,
}

/// Run pattern detection for a newly ingested drawer.
///
/// This is called fire-and-forget from the ingest path — failures are logged
/// as warnings and never propagate to the caller.
pub fn run_pattern_detection(conn: &Connection, args: &PatternDetectionArgs<'_>) {
    let plan = plan_pattern_detection(conn, args);
    apply_pattern_detection(conn, args, plan);
}

pub(crate) fn plan_pattern_detection(
    conn: &Connection,
    args: &PatternDetectionArgs<'_>,
) -> PatternDetectionPlan {
    match try_plan_pattern_detection(conn, args) {
        Ok(plan) => plan,
        Err(err) => {
            warn_pattern_detection_error(&err, args);
            PatternDetectionPlan::None
        }
    }
}

pub(crate) fn apply_pattern_detection(
    conn: &Connection,
    args: &PatternDetectionArgs<'_>,
    plan: PatternDetectionPlan,
) {
    if let Err(err) = try_apply_pattern_detection(conn, args, plan) {
        warn_pattern_detection_error(&err, args);
    }
}

fn warn_pattern_detection_error(err: &rusqlite::Error, args: &PatternDetectionArgs<'_>) {
    tracing::warn!(
        error = %err,
        drawer_id = args.new_drawer_id,
        "pattern detection failed; skipping"
    );
}

fn try_plan_pattern_detection(
    conn: &Connection,
    args: &PatternDetectionArgs<'_>,
) -> rusqlite::Result<PatternDetectionPlan> {
    if args.embedding.is_empty() {
        return Ok(PatternDetectionPlan::None);
    }

    // Query similar drawers from the vector table using cosine similarity.
    // We re-use the novelty_candidates approach but with patterns config threshold.
    let threshold = args.similarity_threshold as f32;
    let top_k = 50i64;
    let embedding_json = serde_json::to_string(args.embedding)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

    let fork_ext_version = crate::core::db::read_fork_ext_version(conn)?;
    if fork_ext_version < PATTERNS_SCHEMA_MIN_FORK_EXT_VERSION {
        return Ok(PatternDetectionPlan::None);
    }

    // Fetch candidate similar drawers (excluding the newly inserted drawer itself).
    let similar_rows: Vec<(String, Option<String>, f32)> = if fork_ext_version >= 5 {
        let mut stmt = conn.prepare(
            r#"
            WITH matches AS (
                SELECT id
                FROM drawer_vectors
                WHERE embedding MATCH vec_f32(?1)
                  AND k = ?2
                  AND (?3 IS NULL OR project_id = ?3)
            )
            SELECT d.id, d.source_file,
                   CAST(1.0 - vec_distance_cosine(v.embedding, vec_f32(?1)) AS REAL) AS similarity
            FROM matches
            JOIN drawer_vectors v ON v.id = matches.id
            JOIN drawers d ON d.id = matches.id
            WHERE d.deleted_at IS NULL
              AND d.id != ?4
              AND (?3 IS NULL OR d.project_id = ?3)
            ORDER BY similarity DESC
            LIMIT ?2
            "#,
        )?;
        stmt.query_map(
            params![embedding_json, top_k, args.project_id, args.new_drawer_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, f32>(2)?,
                ))
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        let mut stmt = conn.prepare(
            r#"
            WITH matches AS (
                SELECT id
                FROM drawer_vectors
                WHERE embedding MATCH vec_f32(?1)
                  AND k = ?2
            )
            SELECT d.id, d.source_file,
                   CAST(1.0 - vec_distance_cosine(v.embedding, vec_f32(?1)) AS REAL) AS similarity
            FROM matches
            JOIN drawer_vectors v ON v.id = matches.id
            JOIN drawers d ON d.id = matches.id
            WHERE d.deleted_at IS NULL
              AND d.id != ?3
            ORDER BY similarity DESC
            LIMIT ?2
            "#,
        )?;
        stmt.query_map(params![embedding_json, top_k, args.new_drawer_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, f32>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    };

    // Filter by similarity threshold.
    let above_threshold: Vec<(String, String)> = similar_rows
        .into_iter()
        .filter(|(_, _, sim)| *sim >= threshold)
        .map(|(id, source_file, _)| (id, source_file.unwrap_or_else(|| "unknown".to_string())))
        .collect();

    if above_threshold.is_empty() {
        return Ok(PatternDetectionPlan::None);
    }

    // Count distinct sessions.
    let distinct_sessions: std::collections::HashSet<String> =
        above_threshold.iter().map(|(_, sf)| sf.clone()).collect();

    // Check if an existing pattern covers any of these drawers.
    let exemplar_ids: Vec<String> = above_threshold.iter().map(|(id, _)| id.clone()).collect();
    let existing_pattern_id = find_pattern_for_exemplars(conn, &exemplar_ids, args.project_id)?;

    if let Some(pattern_id) = existing_pattern_id {
        let Some(revision) = load_pattern_revision(conn, &pattern_id)? else {
            return Ok(PatternDetectionPlan::None);
        };
        let current_exemplars: Vec<String> =
            serde_json::from_str(&revision.exemplar_ids).unwrap_or_default();
        if !exemplar_ids.iter().any(|id| current_exemplars.contains(id)) {
            return Ok(PatternDetectionPlan::None);
        }
        return Ok(PatternDetectionPlan::Update(revision));
    }

    if distinct_sessions.len() < args.min_sessions || above_threshold.len() < args.min_exemplars {
        return Ok(PatternDetectionPlan::None);
    }

    // Create a new pattern candidate.
    // Fetch embeddings for the exemplar drawers to compute centroid.
    let emb_rows = fetch_embeddings_for_drawers(conn, &exemplar_ids)?;
    let embeddings: Vec<Vec<f32>> = emb_rows.iter().map(|(_, _, v)| v.clone()).collect();
    let centroid = if embeddings.is_empty() {
        args.embedding.to_vec()
    } else {
        let mut all_embs = embeddings;
        all_embs.push(args.embedding.to_vec());
        compute_centroid(&all_embs)
    };

    // Extract topic tags from the exemplar drawer contents.
    let contents = fetch_drawer_contents(conn, &exemplar_ids)?;
    let content_refs: Vec<&str> = contents.iter().map(|s| s.as_str()).collect();
    let topic_tags = extract_topic_tags(&content_refs, 5);

    // Collect all session IDs (deduplicated), including the new one.
    let mut all_sessions: Vec<String> = distinct_sessions.into_iter().collect();
    if !all_sessions.contains(&args.session_id.to_string()) {
        all_sessions.push(args.session_id.to_string());
    }

    let overlapping_exemplar_ids = exemplar_ids.clone();
    let mut all_exemplar_ids = exemplar_ids;
    if !all_exemplar_ids.contains(&args.new_drawer_id.to_string()) {
        all_exemplar_ids.push(args.new_drawer_id.to_string());
    }

    Ok(PatternDetectionPlan::Insert {
        pattern: NewPattern {
            pattern_id: uuid_v4(),
            signature: centroid,
            exemplar_ids: all_exemplar_ids,
            session_ids: all_sessions,
            topic_tags,
            model_id: Some(args.model_id.to_string()),
            project_id: args.project_id.map(str::to_string),
        },
        overlapping_exemplar_ids,
    })
}

fn try_apply_pattern_detection(
    conn: &Connection,
    args: &PatternDetectionArgs<'_>,
    plan: PatternDetectionPlan,
) -> rusqlite::Result<()> {
    match plan {
        PatternDetectionPlan::None => {}
        PatternDetectionPlan::Update(revision) => {
            let pattern_id = if load_pattern_revision(conn, &revision.pattern_id)?.is_some() {
                Some(revision.pattern_id)
            } else {
                let exemplar_ids =
                    serde_json::from_str::<Vec<String>>(&revision.exemplar_ids).unwrap_or_default();
                find_pattern_for_exemplars(conn, &exemplar_ids, args.project_id)?
            };
            if let Some(pattern_id) = pattern_id {
                update_pattern_with_exemplar(
                    conn,
                    &pattern_id,
                    args.new_drawer_id,
                    args.session_id,
                    args.embedding,
                    args.promote_threshold,
                )?;
            }
        }
        PatternDetectionPlan::Insert {
            pattern,
            overlapping_exemplar_ids,
        } => {
            if let Some(pattern_id) =
                find_pattern_for_exemplars(conn, &overlapping_exemplar_ids, args.project_id)?
            {
                update_pattern_with_exemplar(
                    conn,
                    &pattern_id,
                    args.new_drawer_id,
                    args.session_id,
                    args.embedding,
                    args.promote_threshold,
                )?;
            } else {
                insert_pattern(conn, &pattern)?;
            }
        }
    }
    Ok(())
}

fn load_pattern_revision(
    conn: &Connection,
    pattern_id: &str,
) -> rusqlite::Result<Option<PatternRevision>> {
    conn.query_row(
        "SELECT pattern_id, exemplar_ids \
         FROM patterns WHERE pattern_id = ?1 AND status IN ('candidate', 'active')",
        [pattern_id],
        |row| {
            Ok(PatternRevision {
                pattern_id: row.get(0)?,
                exemplar_ids: row.get(1)?,
            })
        },
    )
    .optional()
}

fn fetch_drawer_contents(
    conn: &Connection,
    drawer_ids: &[String],
) -> rusqlite::Result<Vec<String>> {
    if drawer_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = drawer_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql =
        format!("SELECT content FROM drawers WHERE id IN ({placeholders}) AND deleted_at IS NULL");
    let mut stmt = conn.prepare(&sql)?;
    let params_iter: Vec<rusqlite::types::Value> = drawer_ids
        .iter()
        .map(|id| rusqlite::types::Value::Text(id.clone()))
        .collect();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params_iter.iter()), |row| {
            row.get::<_, String>(0)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Simple UUID-like ID using timestamp + random-ish data
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (ts >> 96) as u32,
        (ts >> 80) as u16,
        (ts >> 68) as u16 & 0x0fff,
        ((ts >> 52) as u16 & 0x3fff) | 0x8000,
        ts as u64 & 0xffffffffffff,
    )
}

#[cfg(test)]
#[path = "patterns_plan_tests.rs"]
mod plan_tests;
