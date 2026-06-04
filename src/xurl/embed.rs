use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, Params, Statement, params};

use crate::core::config::ChunkerConfig;
use crate::core::db::{CURRENT_VECTOR_INDEX_VERSION, Database};
use crate::embed::Embedder;
use crate::ingest::chunk::chunk_text_token_aware;
use crate::xurl::store::UnindexedTurnSummary;
use crate::xurl::{XurlError, XurlResult};

/// Number of turns collected per outer window before issuing the embed pass.
/// Bounds the working set and the size of each write transaction.
pub const EMBED_BATCH_SIZE: usize = 50;

/// Maximum number of chunks merged into a SINGLE `embedder.embed()` call.
/// Chunks are merged ACROSS turns up to this cap; oversized windows are split
/// into sub-batches of at most this many chunks. This keeps embedding to
/// `ceil(total_chunks / EMBED_MAX_CHUNKS_PER_CALL)` HTTP round-trips per window
/// instead of one round-trip per turn.
pub const EMBED_MAX_CHUNKS_PER_CALL: usize = 128;

/// Number of turn IDs bound per IN-clause when collecting a scoped backlog,
/// kept well under SQLite's bound-parameter limit.
const SCOPE_ID_BATCH: usize = 500;

#[derive(Debug, Default)]
pub struct EmbedStats {
    pub turns_processed: usize,
    pub embedded: usize,
    pub chunks_total: usize,
    pub skipped_stale_content: usize,
}

/// Which unindexed turns an embed pass should cover.
enum EmbedScope<'a> {
    /// Every turn in the table that still lacks a vector (bulk / backlog drain).
    MissingAll { fingerprint: Option<&'a str> },
    /// Only turns whose id is in this set and that still lack a vector
    /// (e.g. the turns from a single freshly-ingested file).
    MissingTurnIds {
        ids: &'a [String],
        fingerprint: Option<&'a str>,
    },
    /// Every turn whose vector metadata does not match the current embedder.
    StaleAll { fingerprint: &'a str },
    /// Every turn, regardless of existing vector state.
    ForceAll { fingerprint: &'a str },
}

#[derive(Clone, Copy)]
enum WriteMode {
    InsertMissing,
    ReplaceExisting,
}

#[derive(Clone, Copy)]
struct VectorMetadata<'a> {
    fingerprint: Option<&'a str>,
    dim: Option<usize>,
    index_version: Option<&'a str>,
}

struct TurnCandidate {
    id: String,
    content: String,
}

#[derive(Debug, Default)]
struct VectorWriteStats {
    skipped_stale_content: usize,
}

/// Serialize a float vector as little-endian bytes for BLOB storage.
fn serialize_vector(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Embed every turn that has no vector yet (global backlog drain).
///
/// This is the entry point wired to `mempal xurl reindex` and to the
/// scan-all ingest path. Processes turns in windows of `EMBED_BATCH_SIZE`;
/// within each window all chunks are embedded with true cross-turn batching
/// (see [`embed_batch_turns`]). Each window's vectors are written under a
/// single short transaction, rolled back on error before propagating.
///
/// `progress_fn`, if provided, is called after each window with
/// `(done_so_far, total)`.
pub async fn embed_unindexed_turns<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    progress_fn: Option<&dyn Fn(usize, usize)>,
) -> XurlResult<EmbedStats> {
    embed_turns(
        db,
        embedder,
        EmbedScope::MissingAll { fingerprint: None },
        progress_fn,
    )
    .await
}

/// Embed every turn that lacks a vector and stamp current vector metadata.
pub async fn embed_unindexed_turns_with_fingerprint<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    fingerprint: &str,
    progress_fn: Option<&dyn Fn(usize, usize)>,
) -> XurlResult<EmbedStats> {
    embed_turns(
        db,
        embedder,
        EmbedScope::MissingAll {
            fingerprint: Some(fingerprint),
        },
        progress_fn,
    )
    .await
}

/// Embed only the turns in `turn_ids` that still lack a vector.
///
/// Used after a single-file ingest so the call returns once the just-ingested
/// turns are vectorized, without dragging in the entire historical backlog.
/// IDs already indexed are skipped; IDs absent from the table are ignored.
pub async fn embed_unindexed_turns_scoped<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    turn_ids: &[String],
    progress_fn: Option<&dyn Fn(usize, usize)>,
) -> XurlResult<EmbedStats> {
    embed_turns(
        db,
        embedder,
        EmbedScope::MissingTurnIds {
            ids: turn_ids,
            fingerprint: None,
        },
        progress_fn,
    )
    .await
}

/// Embed only the scoped turns lacking vectors and stamp current vector metadata.
pub async fn embed_unindexed_turns_scoped_with_fingerprint<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    turn_ids: &[String],
    fingerprint: &str,
    progress_fn: Option<&dyn Fn(usize, usize)>,
) -> XurlResult<EmbedStats> {
    embed_turns(
        db,
        embedder,
        EmbedScope::MissingTurnIds {
            ids: turn_ids,
            fingerprint: Some(fingerprint),
        },
        progress_fn,
    )
    .await
}

/// Re-embed turns whose stored vector metadata does not match `fingerprint`,
/// the embedder dimension, and the current xurl vector index version.
pub async fn embed_stale_turns<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    fingerprint: &str,
    progress_fn: Option<&dyn Fn(usize, usize)>,
) -> XurlResult<EmbedStats> {
    embed_turns(
        db,
        embedder,
        EmbedScope::StaleAll { fingerprint },
        progress_fn,
    )
    .await
}

/// Rebuild vectors for every stored turn.
pub async fn embed_all_turns<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    fingerprint: &str,
    progress_fn: Option<&dyn Fn(usize, usize)>,
) -> XurlResult<EmbedStats> {
    embed_turns(
        db,
        embedder,
        EmbedScope::ForceAll { fingerprint },
        progress_fn,
    )
    .await
}

/// Shared embed driver: collect the unindexed turns in `scope`, then embed and
/// store them window by window.
async fn embed_turns<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    scope: EmbedScope<'_>,
    progress_fn: Option<&dyn Fn(usize, usize)>,
) -> XurlResult<EmbedStats> {
    let conn = db.conn();

    conn.execute_batch("PRAGMA busy_timeout = 5000;")
        .map_err(XurlError::Database)?;

    let dim = embedder.dimensions();
    let candidates = match scope {
        EmbedScope::MissingAll { .. } => collect_unindexed_all(conn)?,
        EmbedScope::MissingTurnIds { ids, .. } => collect_unindexed_scoped(conn, ids)?,
        EmbedScope::StaleAll { fingerprint } => collect_stale_all(conn, fingerprint, dim)?,
        EmbedScope::ForceAll { .. } => collect_all_turns(conn)?,
    };

    if candidates.is_empty() {
        return Ok(EmbedStats::default());
    }

    let total = candidates.len();
    let config = ChunkerConfig::default();
    let mut stats = EmbedStats::default();
    let metadata = match scope {
        EmbedScope::MissingAll { fingerprint } | EmbedScope::MissingTurnIds { fingerprint, .. } => {
            match fingerprint {
                Some(fingerprint) => VectorMetadata {
                    fingerprint: Some(fingerprint),
                    dim: Some(dim),
                    index_version: Some(CURRENT_VECTOR_INDEX_VERSION),
                },
                None => VectorMetadata {
                    fingerprint: None,
                    dim: None,
                    index_version: None,
                },
            }
        }
        EmbedScope::StaleAll { fingerprint } | EmbedScope::ForceAll { fingerprint } => {
            VectorMetadata {
                fingerprint: Some(fingerprint),
                dim: Some(dim),
                index_version: Some(CURRENT_VECTOR_INDEX_VERSION),
            }
        }
    };
    let write_mode = match scope {
        EmbedScope::MissingAll { .. } | EmbedScope::MissingTurnIds { .. } => {
            WriteMode::InsertMissing
        }
        EmbedScope::StaleAll { .. } | EmbedScope::ForceAll { .. } => WriteMode::ReplaceExisting,
    };

    for batch in candidates.chunks(EMBED_BATCH_SIZE) {
        // Phase 1: embed the whole window with no write transaction open.
        let rows = embed_batch_turns(embedder, batch, &config, &mut stats).await?;

        // Phase 2: bulk-insert all collected vectors under a single short transaction.
        // BEGIN IMMEDIATE pairs the content recheck with vector replacement under SQLite's
        // write lock. If ingest commits first, the re-SELECT below sees new content and skips
        // the old vector; if this commit wins first, the later ingest update deletes it and
        // the missing-vector embed path rebuilds from fresh content.
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(XurlError::Database)?;
        let insert_result = (|| -> XurlResult<VectorWriteStats> {
            let write_stats = write_vector_rows(conn, batch, &rows, write_mode, metadata)?;
            conn.execute_batch("COMMIT").map_err(XurlError::Database)?;
            Ok(write_stats)
        })();
        let write_stats = match insert_result {
            Ok(write_stats) => write_stats,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
        };
        stats.skipped_stale_content += write_stats.skipped_stale_content;

        if let Some(f) = progress_fn {
            f(stats.turns_processed, total);
        }
    }

    Ok(stats)
}

pub fn summarize_stale_turns(
    conn: &Connection,
    fingerprint: &str,
    dim: usize,
) -> XurlResult<UnindexedTurnSummary> {
    let dim = dim as i64;
    summarize_candidates(
        conn,
        stale_where_clause(),
        &[&dim, &fingerprint, &CURRENT_VECTOR_INDEX_VERSION],
    )
}

pub fn summarize_all_turns(conn: &Connection) -> XurlResult<UnindexedTurnSummary> {
    summarize_candidates(conn, "1 = 1", &[])
}

fn summarize_candidates(
    conn: &Connection,
    where_clause: &str,
    params: &[&dyn rusqlite::ToSql],
) -> XurlResult<UnindexedTurnSummary> {
    let sql = format!(
        "SELECT COUNT(DISTINCT ct.session_id), COUNT(*) \
         FROM conversation_turns ct \
         WHERE {where_clause}"
    );
    conn.query_row(&sql, params, |row| {
        Ok(UnindexedTurnSummary {
            threads: row.get(0)?,
            turns: row.get(1)?,
        })
    })
    .map_err(XurlError::Database)
}

/// Collect `(id, content)` for every turn lacking a vector.
fn collect_unindexed_all(conn: &Connection) -> XurlResult<Vec<TurnCandidate>> {
    let mut stmt = conn
        .prepare(
            "SELECT ct.id, ct.content \
             FROM conversation_turns ct \
             LEFT JOIN conversation_turn_vectors ctv ON ctv.turn_id = ct.id AND ctv.chunk_index = 0 \
             WHERE ctv.turn_id IS NULL",
        )
        .map_err(XurlError::Database)?;

    let rows = stmt
        .query_map([], |row| {
            Ok(TurnCandidate {
                id: row.get(0)?,
                content: row.get(1)?,
            })
        })
        .map_err(XurlError::Database)?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(XurlError::Database)?);
    }
    Ok(out)
}

fn collect_all_turns(conn: &Connection) -> XurlResult<Vec<TurnCandidate>> {
    let mut stmt = conn
        .prepare(
            "SELECT ct.id, ct.content \
             FROM conversation_turns ct \
             ORDER BY ct.timestamp_epoch ASC, ct.id ASC",
        )
        .map_err(XurlError::Database)?;

    collect_turn_rows(&mut stmt, [])
}

fn collect_stale_all(
    conn: &Connection,
    fingerprint: &str,
    dim: usize,
) -> XurlResult<Vec<TurnCandidate>> {
    let sql = format!(
        "SELECT ct.id, ct.content \
         FROM conversation_turns ct \
         WHERE {} \
         ORDER BY ct.timestamp_epoch ASC, ct.id ASC",
        stale_where_clause()
    );
    let mut stmt = conn.prepare(&sql).map_err(XurlError::Database)?;
    collect_turn_rows(
        &mut stmt,
        params![dim as i64, fingerprint, CURRENT_VECTOR_INDEX_VERSION],
    )
}

fn stale_where_clause() -> &'static str {
    "NOT EXISTS ( \
         SELECT 1 FROM conversation_turn_vectors ctv_any \
         WHERE ctv_any.turn_id = ct.id \
     ) \
     OR EXISTS ( \
         SELECT 1 FROM conversation_turn_vectors ctv_stale \
         WHERE ctv_stale.turn_id = ct.id \
           AND ( \
               ctv_stale.dim IS NULL \
               OR ctv_stale.dim != ?1 \
               OR ctv_stale.embedder_fingerprint IS NULL \
               OR ctv_stale.embedder_fingerprint != ?2 \
               OR ctv_stale.index_version IS NULL \
               OR ctv_stale.index_version != ?3 \
           ) \
     )"
}

fn collect_turn_rows<P>(stmt: &mut Statement<'_>, params: P) -> XurlResult<Vec<TurnCandidate>>
where
    P: Params,
{
    let rows = stmt
        .query_map(params, |row| {
            Ok(TurnCandidate {
                id: row.get(0)?,
                content: row.get(1)?,
            })
        })
        .map_err(XurlError::Database)?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(XurlError::Database)?);
    }
    Ok(out)
}

fn write_vector_rows(
    conn: &Connection,
    candidates: &[TurnCandidate],
    rows: &[(String, usize, Vec<u8>)],
    mode: WriteMode,
    metadata: VectorMetadata<'_>,
) -> XurlResult<VectorWriteStats> {
    let expected_content_by_turn: HashMap<&str, &str> = candidates
        .iter()
        .map(|candidate| (candidate.id.as_str(), candidate.content.as_str()))
        .collect();
    let mut verified_turns = HashSet::new();
    let mut skipped_turns = HashSet::new();
    let mut stats = VectorWriteStats::default();
    for (turn_id, chunk_index, blob) in rows {
        if skipped_turns.contains(turn_id) {
            continue;
        }
        if verified_turns.insert(turn_id.clone()) {
            let Some(expected_content) = expected_content_by_turn.get(turn_id.as_str()) else {
                return Err(XurlError::Parse(format!(
                    "missing collected content token for turn {turn_id}"
                )));
            };
            if !turn_content_is_current(conn, turn_id, expected_content)? {
                skipped_turns.insert(turn_id.clone());
                stats.skipped_stale_content += 1;
                continue;
            }
            if matches!(mode, WriteMode::ReplaceExisting) {
                conn.execute(
                    "DELETE FROM conversation_turn_vectors WHERE turn_id = ?1",
                    params![turn_id],
                )
                .map_err(XurlError::Database)?;
            }
        }

        let dim = metadata.dim.map(|value| value as i64);
        let sql = match mode {
            WriteMode::InsertMissing => {
                "INSERT INTO conversation_turn_vectors \
                 (turn_id, chunk_index, vector, embedder_fingerprint, dim, index_version) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(turn_id, chunk_index) DO NOTHING"
            }
            WriteMode::ReplaceExisting => {
                "INSERT INTO conversation_turn_vectors \
                 (turn_id, chunk_index, vector, embedder_fingerprint, dim, index_version) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(turn_id, chunk_index) DO UPDATE SET \
                    vector = excluded.vector, \
                    embedder_fingerprint = excluded.embedder_fingerprint, \
                    dim = excluded.dim, \
                    index_version = excluded.index_version"
            }
        };
        conn.execute(
            sql,
            params![
                turn_id,
                *chunk_index as i64,
                blob,
                metadata.fingerprint,
                dim,
                metadata.index_version
            ],
        )
        .map_err(XurlError::Database)?;
    }
    Ok(stats)
}

fn turn_content_is_current(
    conn: &Connection,
    turn_id: &str,
    expected_content: &str,
) -> XurlResult<bool> {
    let current = conn
        .query_row(
            "SELECT content FROM conversation_turns WHERE id = ?1",
            params![turn_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(XurlError::Database)?;
    Ok(current.as_deref() == Some(expected_content))
}

/// Collect `(id, content)` for the turns in `turn_ids` that lack a vector.
///
/// IDs are bound in batches of `SCOPE_ID_BATCH` to stay under SQLite's
/// bound-parameter limit; the primary-key lookup keeps each batch cheap.
fn collect_unindexed_scoped(
    conn: &Connection,
    turn_ids: &[String],
) -> XurlResult<Vec<TurnCandidate>> {
    let mut out = Vec::new();
    for id_batch in turn_ids.chunks(SCOPE_ID_BATCH) {
        let placeholders = vec!["?"; id_batch.len()].join(",");
        let sql = format!(
            "SELECT ct.id, ct.content \
             FROM conversation_turns ct \
             LEFT JOIN conversation_turn_vectors ctv ON ctv.turn_id = ct.id AND ctv.chunk_index = 0 \
             WHERE ctv.turn_id IS NULL AND ct.id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql).map_err(XurlError::Database)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> =
            id_batch.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok(TurnCandidate {
                    id: row.get(0)?,
                    content: row.get(1)?,
                })
            })
            .map_err(XurlError::Database)?;
        for row in rows {
            out.push(row.map_err(XurlError::Database)?);
        }
    }
    Ok(out)
}

/// Embed all turns in `batch` with true cross-turn batching and return
/// serialised `(turn_id, chunk_index, blob)` rows.
///
/// Every turn is chunked first; all chunks across the whole window are merged
/// into one list and embedded in sub-batches of at most
/// `EMBED_MAX_CHUNKS_PER_CALL`. Returned vectors are mapped back to their
/// originating `(turn_id, chunk_index)` in order. No database connection is
/// used here; the caller writes the returned rows inside a transaction.
async fn embed_batch_turns<E: Embedder + ?Sized>(
    embedder: &E,
    batch: &[TurnCandidate],
    config: &ChunkerConfig,
    stats: &mut EmbedStats,
) -> XurlResult<Vec<(String, usize, Vec<u8>)>> {
    // Chunk every turn, building a flat list of chunk texts alongside a parallel
    // map of each chunk back to its (turn_id, per-turn chunk_index).
    let mut all_chunks: Vec<String> = Vec::new();
    let mut chunk_map: Vec<(String, usize)> = Vec::new();

    for candidate in batch {
        let chunks =
            chunk_text_token_aware(&candidate.content, config, embedder, Some(&candidate.id));
        stats.chunks_total += chunks.len();
        stats.turns_processed += 1;
        for (chunk_index, chunk) in chunks.into_iter().enumerate() {
            chunk_map.push((candidate.id.clone(), chunk_index));
            all_chunks.push(chunk);
        }
    }

    if all_chunks.is_empty() {
        return Ok(Vec::new());
    }

    // Issue embedding calls in sub-batches that merge across turns; never one
    // call per turn. Map each returned vector back via `chunk_map`, preserving
    // order, using a running offset into the flat chunk list.
    let mut rows = Vec::with_capacity(all_chunks.len());
    let mut offset = 0usize;
    for window in all_chunks.chunks(EMBED_MAX_CHUNKS_PER_CALL) {
        let chunk_refs: Vec<&str> = window.iter().map(String::as_str).collect();
        let vectors = embedder
            .embed(&chunk_refs)
            .await
            .map_err(|e| XurlError::Parse(format!("embedding failed: {e}")))?;

        if vectors.len() != window.len() {
            return Err(XurlError::Parse(format!(
                "embedder returned {} vectors for {} chunks",
                vectors.len(),
                window.len()
            )));
        }

        for (i, vector) in vectors.iter().enumerate() {
            let (turn_id, chunk_index) = &chunk_map[offset + i];
            rows.push((turn_id.clone(), *chunk_index, serialize_vector(vector)));
            stats.embedded += 1;
        }
        offset += window.len();
    }

    Ok(rows)
}
