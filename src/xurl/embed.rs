use rusqlite::params;

use crate::core::config::ChunkerConfig;
use crate::core::db::Database;
use crate::embed::Embedder;
use crate::ingest::chunk::chunk_text_token_aware;
use crate::xurl::{XurlError, XurlResult};

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
/// Long turns (>512 tokens by default) are split into overlapping chunks using
/// the same `chunk_text_token_aware` function used by the drawer ingest pipeline.
/// Each chunk produces a separate row in `conversation_turn_vectors` with an
/// incrementing `chunk_index`.
pub async fn embed_unindexed_turns<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
) -> XurlResult<EmbedStats> {
    let conn = db.conn();

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

    let config = ChunkerConfig::default();
    let mut stats = EmbedStats::default();

    for (turn_id, content) in &unindexed {
        let chunks = chunk_text_token_aware(content, &config, embedder, Some(turn_id));
        let chunk_refs: Vec<&str> = chunks.iter().map(String::as_str).collect();

        let vectors = embedder
            .embed(&chunk_refs)
            .await
            .map_err(|e| XurlError::Parse(format!("embedding failed: {e}")))?;

        for (chunk_index, vector) in vectors.iter().enumerate() {
            let blob = serialize_vector(vector);
            conn.execute(
                "INSERT INTO conversation_turn_vectors (turn_id, chunk_index, vector) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT(turn_id, chunk_index) DO NOTHING",
                params![turn_id, chunk_index as i64, blob],
            )
            .map_err(XurlError::Database)?;
        }

        stats.embedded += vectors.len();
        stats.chunks_total += chunks.len();
        stats.turns_processed += 1;
    }

    Ok(stats)
}
