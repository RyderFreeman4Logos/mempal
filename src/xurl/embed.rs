use rusqlite::params;

use crate::core::config::ChunkerConfig;
use crate::core::db::Database;
use crate::embed::Embedder;
use crate::ingest::chunk::chunk_text_token_aware;
use crate::xurl::{XurlError, XurlResult};

const EMBED_BATCH_SIZE: usize = 50;

#[derive(Debug, Default)]
pub struct EmbedStats {
    pub turns_processed: usize,
    pub embedded: usize,
    pub chunks_total: usize,
}

/// Serialize a float vector as little-endian bytes for BLOB storage.
fn serialize_vector(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Find turns that have no entries in `conversation_turn_vectors` and embed them.
///
/// Processes turns in batches of `EMBED_BATCH_SIZE`. Each batch is wrapped in a
/// transaction to avoid per-row journal overhead on large DBs. On error within a
/// batch the transaction is rolled back before propagating the error.
///
/// `progress_fn`, if provided, is called after each batch with `(done_so_far, total)`.
pub async fn embed_unindexed_turns<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    progress_fn: Option<&dyn Fn(usize, usize)>,
) -> XurlResult<EmbedStats> {
    let conn = db.conn();

    conn.execute_batch("PRAGMA busy_timeout = 5000;")
        .map_err(XurlError::Database)?;

    // Find all turns that have no vector yet.
    let unindexed: Vec<(String, String)> = {
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
        out
    };

    if unindexed.is_empty() {
        return Ok(EmbedStats::default());
    }

    let total = unindexed.len();
    let config = ChunkerConfig::default();
    let mut stats = EmbedStats::default();

    for batch in unindexed.chunks(EMBED_BATCH_SIZE) {
        // Phase 1: embed the whole batch with no write transaction open.
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

/// Embed all turns in `batch` and return serialised `(turn_id, chunk_index, blob)` rows.
///
/// No database connection is used here; the caller is responsible for writing
/// the returned rows inside a transaction.
async fn embed_batch_turns<E: Embedder + ?Sized>(
    embedder: &E,
    batch: &[(String, String)],
    config: &ChunkerConfig,
    stats: &mut EmbedStats,
) -> XurlResult<Vec<(String, usize, Vec<u8>)>> {
    let mut rows = Vec::new();
    for (turn_id, content) in batch {
        let chunks = chunk_text_token_aware(content, config, embedder, Some(turn_id));
        let chunk_refs: Vec<&str> = chunks.iter().map(String::as_str).collect();

        let vectors = embedder
            .embed(&chunk_refs)
            .await
            .map_err(|e| XurlError::Parse(format!("embedding failed: {e}")))?;

        for (chunk_index, vector) in vectors.iter().enumerate() {
            rows.push((turn_id.clone(), chunk_index, serialize_vector(vector)));
        }

        stats.embedded += vectors.len();
        stats.chunks_total += chunks.len();
        stats.turns_processed += 1;
    }
    Ok(rows)
}
