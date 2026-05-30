use rusqlite::{Connection, params};

use crate::core::config::ChunkerConfig;
use crate::core::db::Database;
use crate::embed::Embedder;
use crate::ingest::chunk::chunk_text_token_aware;
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
}

/// Which unindexed turns an embed pass should cover.
enum EmbedScope<'a> {
    /// Every turn in the table that still lacks a vector (bulk / backlog drain).
    All,
    /// Only turns whose id is in this set and that still lack a vector
    /// (e.g. the turns from a single freshly-ingested file).
    TurnIds(&'a [String]),
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
    embed_turns(db, embedder, EmbedScope::All, progress_fn).await
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
    embed_turns(db, embedder, EmbedScope::TurnIds(turn_ids), progress_fn).await
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

    let unindexed = match scope {
        EmbedScope::All => collect_unindexed_all(conn)?,
        EmbedScope::TurnIds(ids) => collect_unindexed_scoped(conn, ids)?,
    };

    if unindexed.is_empty() {
        return Ok(EmbedStats::default());
    }

    let total = unindexed.len();
    let config = ChunkerConfig::default();
    let mut stats = EmbedStats::default();

    for batch in unindexed.chunks(EMBED_BATCH_SIZE) {
        // Phase 1: embed the whole window with no write transaction open.
        let rows = embed_batch_turns(embedder, batch, &config, &mut stats).await?;

        // Phase 2: bulk-insert all collected vectors under a single short transaction.
        conn.execute_batch("BEGIN").map_err(XurlError::Database)?;
        let insert_result = (|| -> XurlResult<()> {
            for (turn_id, chunk_index, blob) in &rows {
                conn.execute(
                    "INSERT INTO conversation_turn_vectors (turn_id, chunk_index, vector) \
                     VALUES (?1, ?2, ?3) \
                     ON CONFLICT(turn_id, chunk_index) DO NOTHING",
                    params![turn_id, *chunk_index as i64, blob],
                )
                .map_err(XurlError::Database)?;
            }
            conn.execute_batch("COMMIT").map_err(XurlError::Database)?;
            Ok(())
        })();
        if let Err(e) = insert_result {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }

        if let Some(f) = progress_fn {
            f(stats.turns_processed, total);
        }
    }

    Ok(stats)
}

/// Collect `(id, content)` for every turn lacking a vector.
fn collect_unindexed_all(conn: &Connection) -> XurlResult<Vec<(String, String)>> {
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
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(XurlError::Database)?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(XurlError::Database)?);
    }
    Ok(out)
}

/// Collect `(id, content)` for the turns in `turn_ids` that lack a vector.
///
/// IDs are bound in batches of `SCOPE_ID_BATCH` to stay under SQLite's
/// bound-parameter limit; the primary-key lookup keeps each batch cheap.
fn collect_unindexed_scoped(
    conn: &Connection,
    turn_ids: &[String],
) -> XurlResult<Vec<(String, String)>> {
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
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
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
    batch: &[(String, String)],
    config: &ChunkerConfig,
    stats: &mut EmbedStats,
) -> XurlResult<Vec<(String, usize, Vec<u8>)>> {
    // Chunk every turn, building a flat list of chunk texts alongside a parallel
    // map of each chunk back to its (turn_id, per-turn chunk_index).
    let mut all_chunks: Vec<String> = Vec::new();
    let mut chunk_map: Vec<(String, usize)> = Vec::new();

    for (turn_id, content) in batch {
        let chunks = chunk_text_token_aware(content, config, embedder, Some(turn_id));
        stats.chunks_total += chunks.len();
        stats.turns_processed += 1;
        for (chunk_index, chunk) in chunks.into_iter().enumerate() {
            chunk_map.push((turn_id.clone(), chunk_index));
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
