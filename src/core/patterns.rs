use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub const PATTERNS_SCHEMA_MIN_FORK_EXT_VERSION: u32 = 11;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternStatus {
    Candidate,
    Active,
    Retired,
}

impl PatternStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Active => "active",
            Self::Retired => "retired",
        }
    }
}

impl std::str::FromStr for PatternStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "candidate" => Ok(Self::Candidate),
            "active" => Ok(Self::Active),
            "retired" => Ok(Self::Retired),
            other => Err(format!("unknown pattern status: {other}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pattern {
    pub pattern_id: String,
    pub signature: Vec<f32>,
    pub exemplar_ids: Vec<String>,
    pub exemplar_count: usize,
    pub session_ids: Vec<String>,
    pub session_count: usize,
    pub topic_tags: Vec<String>,
    pub model_id: Option<String>,
    pub status: PatternStatus,
    pub first_seen_at: i64,
    pub updated_at: i64,
    pub project_id: Option<String>,
}

/// Lightweight summary for inclusion in MCP context responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternSummary {
    pub pattern_id: String,
    pub topic_tags: Vec<String>,
    pub session_count: usize,
    pub exemplar_preview: Option<String>,
}

/// Arguments for creating a new pattern candidate.
pub struct NewPattern {
    pub pattern_id: String,
    pub signature: Vec<f32>,
    pub exemplar_ids: Vec<String>,
    pub session_ids: Vec<String>,
    pub topic_tags: Vec<String>,
    pub model_id: Option<String>,
    pub project_id: Option<String>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn blob_to_vec(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// Returns true if the `patterns` table exists (fork_ext_version >= 11).
pub fn patterns_table_exists(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='patterns'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// Insert a new pattern candidate into the `patterns` table.
pub fn insert_pattern(conn: &Connection, p: &NewPattern) -> rusqlite::Result<()> {
    let now = now_ms();
    let exemplar_json = serde_json::to_string(&p.exemplar_ids).unwrap_or_else(|_| "[]".to_string());
    let session_json = serde_json::to_string(&p.session_ids).unwrap_or_else(|_| "[]".to_string());
    let tags_json = serde_json::to_string(&p.topic_tags).unwrap_or_else(|_| "[]".to_string());
    let sig_blob = vec_to_blob(&p.signature);
    let exemplar_count = p.exemplar_ids.len() as i64;
    let session_count = p.session_ids.len() as i64;

    conn.execute(
        r#"
        INSERT INTO patterns (
            pattern_id, signature, exemplar_ids, exemplar_count,
            session_ids, session_count, topic_tags, model_id,
            status, first_seen_at, updated_at, project_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'candidate', ?9, ?9, ?10)
        ON CONFLICT(pattern_id) DO NOTHING
        "#,
        params![
            p.pattern_id,
            sig_blob,
            exemplar_json,
            exemplar_count,
            session_json,
            session_count,
            tags_json,
            p.model_id,
            now,
            p.project_id,
        ],
    )?;
    Ok(())
}

/// Update an existing pattern with a new exemplar, using incremental mean for the centroid.
/// Returns the new session_count after update.
pub fn update_pattern_with_exemplar(
    conn: &Connection,
    pattern_id: &str,
    new_drawer_id: &str,
    new_session_id: &str,
    new_embedding: &[f32],
    promote_threshold: usize,
) -> rusqlite::Result<usize> {
    let row = conn.query_row(
        "SELECT signature, exemplar_ids, exemplar_count, session_ids, session_count, status
         FROM patterns WHERE pattern_id = ?1",
        [pattern_id],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
            ))
        },
    );

    let (sig_blob, exemplar_json, old_count, session_json, session_count, status) = match row {
        Ok(r) => r,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(0),
        Err(e) => return Err(e),
    };

    let mut old_sig = blob_to_vec(&sig_blob);
    let new_count = old_count + 1;
    // Incremental mean: new_centroid = (old_centroid * (count-1) + new_embedding) / count
    for (old, new) in old_sig.iter_mut().zip(new_embedding.iter()) {
        *old = ((*old * (new_count - 1) as f32) + new) / new_count as f32;
    }

    let mut exemplar_ids: Vec<String> = serde_json::from_str(&exemplar_json).unwrap_or_default();
    if !exemplar_ids.contains(&new_drawer_id.to_string()) {
        exemplar_ids.push(new_drawer_id.to_string());
    }

    let mut session_ids: Vec<String> = serde_json::from_str(&session_json).unwrap_or_default();
    let new_session_count = if session_ids.contains(&new_session_id.to_string()) {
        session_count
    } else {
        session_ids.push(new_session_id.to_string());
        session_count + 1
    };

    let new_status = if status == "candidate" && new_session_count >= promote_threshold as i64 {
        "active"
    } else {
        &status
    };

    conn.execute(
        r#"
        UPDATE patterns
        SET signature = ?2,
            exemplar_ids = ?3,
            exemplar_count = ?4,
            session_ids = ?5,
            session_count = ?6,
            status = ?7,
            updated_at = ?8
        WHERE pattern_id = ?1
        "#,
        params![
            pattern_id,
            vec_to_blob(&old_sig),
            serde_json::to_string(&exemplar_ids).unwrap_or_else(|_| "[]".to_string()),
            new_count,
            serde_json::to_string(&session_ids).unwrap_or_else(|_| "[]".to_string()),
            new_session_count,
            new_status,
            now_ms(),
        ],
    )?;

    Ok(new_session_count as usize)
}

/// Set a pattern's status to 'retired'.
pub fn retire_pattern(conn: &Connection, pattern_id: &str) -> rusqlite::Result<bool> {
    let count = conn.execute(
        "UPDATE patterns SET status = 'retired', updated_at = ?2 WHERE pattern_id = ?1",
        params![pattern_id, now_ms()],
    )?;
    Ok(count > 0)
}

/// Set a pattern's status to 'active' (manual promote).
pub fn promote_pattern(conn: &Connection, pattern_id: &str) -> rusqlite::Result<bool> {
    let count = conn.execute(
        "UPDATE patterns SET status = 'active', updated_at = ?2 WHERE pattern_id = ?1 AND status = 'candidate'",
        params![pattern_id, now_ms()],
    )?;
    Ok(count > 0)
}

/// Fetch all patterns matching the given status filter and optional project_id.
pub fn list_patterns(
    conn: &Connection,
    status: Option<&str>,
    project_id: Option<&str>,
) -> rusqlite::Result<Vec<Pattern>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT pattern_id, signature, exemplar_ids, exemplar_count,
               session_ids, session_count, topic_tags, model_id,
               status, first_seen_at, updated_at, project_id
        FROM patterns
        WHERE (?1 IS NULL OR status = ?1)
          AND (?2 IS NULL OR project_id = ?2 OR project_id IS NULL)
        ORDER BY session_count DESC, updated_at DESC
        "#,
    )?;
    let rows = stmt
        .query_map(params![status, project_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, Option<String>>(11)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    rows.into_iter()
        .map(
            |(
                pattern_id,
                sig_blob,
                exemplar_json,
                exemplar_count,
                session_json,
                session_count,
                tags_json,
                model_id,
                status_str,
                first_seen_at,
                updated_at,
                project_id,
            )| {
                let status = status_str
                    .parse::<PatternStatus>()
                    .map_err(|_e| rusqlite::Error::InvalidQuery)?;
                Ok(Pattern {
                    pattern_id,
                    signature: blob_to_vec(&sig_blob),
                    exemplar_ids: serde_json::from_str(&exemplar_json).unwrap_or_default(),
                    exemplar_count: exemplar_count as usize,
                    session_ids: serde_json::from_str(&session_json).unwrap_or_default(),
                    session_count: session_count as usize,
                    topic_tags: tags_json
                        .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
                        .unwrap_or_default(),
                    model_id,
                    status,
                    first_seen_at,
                    updated_at,
                    project_id,
                })
            },
        )
        .collect()
}

/// Fetch a single pattern by ID.
pub fn get_pattern(conn: &Connection, pattern_id: &str) -> rusqlite::Result<Option<Pattern>> {
    let result = conn
        .query_row(
            r#"
            SELECT pattern_id, signature, exemplar_ids, exemplar_count,
                   session_ids, session_count, topic_tags, model_id,
                   status, first_seen_at, updated_at, project_id
            FROM patterns WHERE pattern_id = ?1
            "#,
            [pattern_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, Option<String>>(11)?,
                ))
            },
        )
        .optional()?;

    let Some((
        pattern_id,
        sig_blob,
        exemplar_json,
        exemplar_count,
        session_json,
        session_count,
        tags_json,
        model_id,
        status_str,
        first_seen_at,
        updated_at,
        project_id,
    )) = result
    else {
        return Ok(None);
    };

    let status = status_str
        .parse::<PatternStatus>()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;

    Ok(Some(Pattern {
        pattern_id,
        signature: blob_to_vec(&sig_blob),
        exemplar_ids: serde_json::from_str(&exemplar_json).unwrap_or_default(),
        exemplar_count: exemplar_count as usize,
        session_ids: serde_json::from_str(&session_json).unwrap_or_default(),
        session_count: session_count as usize,
        topic_tags: tags_json
            .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
            .unwrap_or_default(),
        model_id,
        status,
        first_seen_at,
        updated_at,
        project_id,
    }))
}

/// Find an existing active or candidate pattern whose exemplar set overlaps with a given drawer set.
/// Returns the pattern_id of the first matching pattern, if any.
pub fn find_pattern_for_exemplars(
    conn: &Connection,
    drawer_ids: &[String],
    project_id: Option<&str>,
) -> rusqlite::Result<Option<String>> {
    if drawer_ids.is_empty() {
        return Ok(None);
    }
    // Walk candidate/active patterns and check if ANY drawer in drawer_ids is in exemplar_ids.
    let mut stmt = conn.prepare(
        r#"
        SELECT pattern_id, exemplar_ids
        FROM patterns
        WHERE status IN ('candidate', 'active')
          AND (?1 IS NULL OR project_id = ?1 OR project_id IS NULL)
        "#,
    )?;
    let rows: Vec<(String, String)> = stmt
        .query_map([project_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    for (pattern_id, exemplar_json) in rows {
        let exemplars: Vec<String> = serde_json::from_str(&exemplar_json).unwrap_or_default();
        if drawer_ids.iter().any(|id| exemplars.contains(id)) {
            return Ok(Some(pattern_id));
        }
    }
    Ok(None)
}

/// Compute cosine similarity between two equal-length vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)).clamp(-1.0, 1.0)
}

/// Compute the centroid (element-wise mean) of a collection of embeddings.
pub fn compute_centroid(embeddings: &[Vec<f32>]) -> Vec<f32> {
    if embeddings.is_empty() {
        return Vec::new();
    }
    let dim = embeddings[0].len();
    let n = embeddings.len() as f32;
    let mut centroid = vec![0.0f32; dim];
    for emb in embeddings {
        for (c, v) in centroid.iter_mut().zip(emb.iter()) {
            *c += v;
        }
    }
    centroid.iter_mut().for_each(|c| *c /= n);
    centroid
}

/// Load active patterns filtered by model_id and optional project_id for search boosting.
/// Patterns with a mismatched model_id are excluded.
pub fn load_active_patterns_for_search(
    conn: &Connection,
    current_model_id: &str,
    project_id: Option<&str>,
) -> rusqlite::Result<Vec<Pattern>> {
    let all_active = list_patterns(conn, Some("active"), project_id)?;
    Ok(all_active
        .into_iter()
        .filter(|p| {
            p.model_id
                .as_deref()
                .map(|m| m == current_model_id)
                .unwrap_or(false)
        })
        .collect())
}

/// Load active patterns for context (recurring_themes), matching project scope.
pub fn load_active_patterns_for_context(
    conn: &Connection,
    project_id: Option<&str>,
) -> rusqlite::Result<Vec<Pattern>> {
    list_patterns(conn, Some("active"), project_id)
}

/// Extract topic tags from drawer content using simple term frequency.
/// Returns up to `top_n` words ranked by length × uniqueness heuristic.
pub fn extract_topic_tags(contents: &[&str], top_n: usize) -> Vec<String> {
    use std::collections::HashMap;

    let mut freq: HashMap<String, usize> = HashMap::new();
    for content in contents {
        let words = tokenize(content);
        for word in words {
            *freq.entry(word).or_insert(0) += 1;
        }
    }

    let mut scored: Vec<(String, usize)> = freq.into_iter().collect();
    // Sort by frequency descending, then alphabetically for stability
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored
        .into_iter()
        .take(top_n)
        .map(|(word, _)| word)
        .collect()
}

fn tokenize(text: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "the", "a", "an", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
        "do", "does", "did", "will", "would", "should", "can", "could", "may", "might", "shall",
        "must", "to", "of", "in", "on", "at", "by", "for", "with", "from", "as", "that", "this",
        "it", "its", "or", "and", "but", "not", "no", "so", "if", "when", "then", "than", "more",
        "into", "out", "up", "down", "any", "all", "also", "i", "we", "you", "he", "she", "they",
        "me", "us", "him", "her", "them", "my", "our", "your", "his", "their",
    ];
    text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|w| w.len() >= 4)
        .map(|w| w.to_lowercase())
        .filter(|w| !STOP_WORDS.contains(&w.as_str()))
        .collect()
}

/// Fetch embeddings for a list of drawer_ids from the `drawer_vectors` table.
type EmbeddingRow = (String, Option<String>, Vec<f32>);

/// Returns a Vec of (drawer_id, source_file, embedding) tuples.
pub fn fetch_embeddings_for_drawers(
    conn: &Connection,
    drawer_ids: &[String],
) -> rusqlite::Result<Vec<EmbeddingRow>> {
    if drawer_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = drawer_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        r#"
        SELECT v.id, d.source_file, vec_to_json(v.embedding)
        FROM drawer_vectors v
        JOIN drawers d ON d.id = v.id
        WHERE v.id IN ({placeholders})
          AND d.deleted_at IS NULL
        "#
    );
    let mut stmt = conn.prepare(&sql)?;
    let params_iter: Vec<rusqlite::types::Value> = drawer_ids
        .iter()
        .map(|id| rusqlite::types::Value::Text(id.clone()))
        .collect();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params_iter.iter()), |row| {
            let id: String = row.get(0)?;
            let source_file: Option<String> = row.get(1)?;
            let embedding_json: String = row.get(2)?;
            Ok((id, source_file, embedding_json))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut result = Vec::with_capacity(rows.len());
    for (id, source_file, json) in rows {
        let values: Vec<f32> = serde_json::from_str(&json).unwrap_or_default();
        result.push((id, source_file, values));
    }
    Ok(result)
}

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

/// Run pattern detection for a newly ingested drawer.
///
/// This is called fire-and-forget from the ingest path — failures are logged
/// as warnings and never propagate to the caller.
pub fn run_pattern_detection(conn: &Connection, args: &PatternDetectionArgs<'_>) {
    if let Err(err) = try_run_pattern_detection(conn, args) {
        tracing::warn!(
            error = %err,
            drawer_id = args.new_drawer_id,
            "pattern detection failed; skipping"
        );
    }
}

fn try_run_pattern_detection(
    conn: &Connection,
    args: &PatternDetectionArgs<'_>,
) -> rusqlite::Result<()> {
    if args.embedding.is_empty() {
        return Ok(());
    }

    // Query similar drawers from the vector table using cosine similarity.
    // We re-use the novelty_candidates approach but with patterns config threshold.
    let threshold = args.similarity_threshold as f32;
    let top_k = 50i64;
    let embedding_json = serde_json::to_string(args.embedding)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

    let fork_ext_version = super::db::read_fork_ext_version(conn)?;
    if fork_ext_version < PATTERNS_SCHEMA_MIN_FORK_EXT_VERSION {
        return Ok(());
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
        return Ok(());
    }

    // Count distinct sessions.
    let distinct_sessions: std::collections::HashSet<String> =
        above_threshold.iter().map(|(_, sf)| sf.clone()).collect();

    // Check if an existing pattern covers any of these drawers.
    let exemplar_ids: Vec<String> = above_threshold.iter().map(|(id, _)| id.clone()).collect();
    let existing_pattern_id = find_pattern_for_exemplars(conn, &exemplar_ids, args.project_id)?;

    if let Some(pattern_id) = existing_pattern_id {
        // Update existing pattern with the new exemplar.
        update_pattern_with_exemplar(
            conn,
            &pattern_id,
            args.new_drawer_id,
            args.session_id,
            args.embedding,
            args.promote_threshold,
        )?;
    } else if distinct_sessions.len() >= args.min_sessions
        && above_threshold.len() >= args.min_exemplars
    {
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

        let mut all_exemplar_ids = exemplar_ids;
        if !all_exemplar_ids.contains(&args.new_drawer_id.to_string()) {
            all_exemplar_ids.push(args.new_drawer_id.to_string());
        }

        let pattern_id = uuid_v4();
        insert_pattern(
            conn,
            &NewPattern {
                pattern_id,
                signature: centroid,
                exemplar_ids: all_exemplar_ids,
                session_ids: all_sessions,
                topic_tags,
                model_id: Some(args.model_id.to_string()),
                project_id: args.project_id.map(str::to_string),
            },
        )?;
    }

    Ok(())
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
