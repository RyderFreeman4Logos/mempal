use std::collections::{HashMap, HashSet};

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Internal per-turn accumulator: (score, session_id, tool, role, content, timestamp, source_path).
type TurnBestEntry = (f32, String, String, String, String, f64, Option<String>);

use crate::core::db::Database;
use crate::embed::Embedder;
use crate::xurl::store::TurnFilter;
use crate::xurl::{XurlError, XurlResult};

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub turn_id: String,
    pub session_id: String,
    pub tool: String,
    pub role: String,
    pub content: String,
    pub timestamp_epoch: f64,
    pub score: f32,
    pub source_path: Option<String>,
}

/// Aggregated result returned by [`search`].
#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    /// Highest score among hits that were filtered out by `min_score_floor`.
    pub best_score_below_floor: Option<f32>,
    /// Total unique candidates (after per-turn and per-content dedup, before floor and limit).
    pub total_candidates: usize,
    /// The min-score threshold that was applied, if any.
    pub min_score_floor: Option<f32>,
}

/// Deserialize a raw f32 little-endian BLOB into a vector.
fn deserialize_vector(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("4-byte aligned blob")))
        .collect()
}

/// Cosine similarity between two equal-length vectors. Returns 0.0 on zero norm.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// Options controlling semantic search behaviour.
#[derive(Debug, Default)]
pub struct SearchOptions {
    /// Maximum number of hits to return (0 ⇒ empty result immediately).
    pub limit: usize,
    /// Column-level pre-filter applied before scoring.
    pub filter: Option<TurnFilter>,
    /// Include CSA-delegated turns (default: false).
    pub include_csa: bool,
    /// Include non-human-provenance turns (default: false).
    pub include_agent_prompts: bool,
    /// Exclude hits scoring below this threshold.
    pub min_score: Option<f32>,
}

/// Semantic search over `conversation_turn_vectors` using brute-force cosine similarity.
///
/// The query is embedded via `embedder`, then every stored vector is scored.
/// Multi-chunk turns are deduplicated by keeping the best-scoring chunk.
/// Identical-content turns are deduplicated, keeping the highest-scored representative.
/// Default filtering excludes CSA-delegated turns and non-human-provenance turns.
///
/// If `opts.min_score` is set, hits below the floor are excluded;
/// [`SearchResult::best_score_below_floor`] reflects the highest score that did not make the cut.
pub async fn search<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    query: &str,
    opts: SearchOptions,
) -> XurlResult<SearchResult> {
    let SearchOptions {
        limit,
        filter,
        include_csa,
        include_agent_prompts,
        min_score,
    } = opts;
    if limit == 0 {
        return Ok(SearchResult {
            hits: Vec::new(),
            best_score_below_floor: None,
            total_candidates: 0,
            min_score_floor: min_score,
        });
    }

    // Embed the query string.
    let query_vecs = embedder
        .embed(&[query])
        .await
        .map_err(|e| XurlError::Parse(format!("embedding query failed: {e}")))?;
    let query_vec = query_vecs
        .into_iter()
        .next()
        .ok_or_else(|| XurlError::Parse("embedder returned empty result for query".into()))?;

    let conn = db.conn();
    let filter = filter.unwrap_or_default();
    let offset = filter.offset;

    // Build WHERE clause and parameter list dynamically.
    let mut conditions: Vec<String> = Vec::new();
    let mut param_values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut pidx = 1usize;

    if !include_csa {
        conditions.push("ct.is_csa_delegated = 0".into());
    }
    if !include_agent_prompts {
        conditions.push("ct.provenance = 'human'".into());
    }
    if let Some(ref tool) = filter.tool {
        conditions.push(format!("ct.tool = ?{pidx}"));
        param_values.push(Box::new(tool.as_str().to_string()));
        pidx += 1;
    }
    if let Some(ref sid) = filter.session_id {
        conditions.push(format!("ct.session_id = ?{pidx}"));
        param_values.push(Box::new(sid.clone()));
        pidx += 1;
    }
    if let Some(since) = filter.since_epoch {
        conditions.push(format!("ct.timestamp_epoch >= ?{pidx}"));
        param_values.push(Box::new(since));
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let sql = format!(
        "SELECT ctv.turn_id, ctv.vector, \
         ct.session_id, ct.tool, ct.role, ct.content, ct.timestamp_epoch, ct.project_path \
         FROM conversation_turn_vectors ctv \
         JOIN conversation_turns ct ON ct.id = ctv.turn_id \
         {where_clause}"
    );

    struct VectorRow {
        turn_id: String,
        vector: Vec<u8>,
        session_id: String,
        tool: String,
        role: String,
        content: String,
        timestamp_epoch: f64,
        project_path: Option<String>,
    }

    let refs: Vec<&dyn rusqlite::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql).map_err(XurlError::Database)?;
    let rows: Vec<VectorRow> = stmt
        .query_map(refs.as_slice(), |row| {
            Ok(VectorRow {
                turn_id: row.get(0)?,
                vector: row.get(1)?,
                session_id: row.get(2)?,
                tool: row.get(3)?,
                role: row.get(4)?,
                content: row.get(5)?,
                timestamp_epoch: row.get(6)?,
                project_path: row.get(7)?,
            })
        })
        .map_err(XurlError::Database)?
        .collect::<Result<_, _>>()
        .map_err(XurlError::Database)?;

    // Score each chunk, deduplicate by turn_id keeping best score.
    let mut best_by_turn: HashMap<String, TurnBestEntry> = HashMap::new();

    for row in rows {
        let vec = deserialize_vector(&row.vector);
        let score = cosine_similarity(&query_vec, &vec);
        let entry = best_by_turn.entry(row.turn_id.clone()).or_insert((
            -1.0,
            row.session_id,
            row.tool,
            row.role,
            row.content,
            row.timestamp_epoch,
            row.project_path,
        ));
        if score > entry.0 {
            entry.0 = score;
        }
    }

    // Convert to SearchHit and sort by descending score.
    let mut all_hits: Vec<SearchHit> = best_by_turn
        .into_iter()
        .map(
            |(turn_id, (score, session_id, tool, role, content, timestamp_epoch, source_path))| {
                SearchHit {
                    turn_id,
                    session_id,
                    tool,
                    role,
                    content,
                    timestamp_epoch,
                    score,
                    source_path,
                }
            },
        )
        .collect();

    all_hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Dedup by content hash: hits are sorted desc by score, so the first occurrence per
    // content hash is always the highest-scored representative.
    let mut seen_content_hashes: HashSet<String> = HashSet::new();
    let deduped: Vec<SearchHit> = all_hits
        .into_iter()
        .filter(|hit| {
            let hash = format!("{:x}", Sha256::digest(hit.content.as_bytes()));
            seen_content_hashes.insert(hash)
        })
        .collect();

    let total_candidates = deduped.len();

    // Apply min_score floor, then truncate.
    let (passing, below_floor): (Vec<SearchHit>, Vec<SearchHit>) = if let Some(floor) = min_score {
        deduped.into_iter().partition(|h| h.score >= floor)
    } else {
        (deduped, Vec::new())
    };

    // below_floor is sorted desc by score (partitioned from a sorted vec), so first = highest.
    let best_score_below_floor = below_floor.into_iter().next().map(|h| h.score);

    let hits = passing.into_iter().skip(offset).take(limit).collect();

    Ok(SearchResult {
        hits,
        best_score_below_floor,
        total_candidates,
        min_score_floor: min_score,
    })
}

/// Format a Unix timestamp as a compact ISO 8601 date-time string.
pub fn format_timestamp(epoch: f64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let d = UNIX_EPOCH + Duration::from_secs_f64(epoch);
    let secs = d.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    // Simple ISO 8601 formatting without chrono dependency in this module.
    let (y, mo, day, h, mi, s) = epoch_to_datetime(secs);
    format!("{y:04}-{mo:02}-{day:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn epoch_to_datetime(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let s = secs % 60;
    let total_min = secs / 60;
    let mi = total_min % 60;
    let total_h = total_min / 60;
    let h = total_h % 24;
    let total_days = total_h / 24;

    // Days since 1970-01-01 → Gregorian calendar
    let mut year = 1970u64;
    let mut days = total_days;
    loop {
        let dy = if is_leap(year) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        year += 1;
    }
    let months = [
        31u64,
        if is_leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1u64;
    for &dm in &months {
        if days < dm {
            break;
        }
        days -= dm;
        month += 1;
    }
    (year, month, days + 1, h, mi, s)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Print search hits in markdown format to stdout.
pub fn print_hits_markdown(result: &SearchResult) {
    if result.hits.is_empty() {
        if let Some(best) = result.best_score_below_floor {
            let floor = result.min_score_floor.unwrap_or(0.0);
            println!("No confident match (best score {best:.3} < floor {floor:.2})");
        } else {
            println!("No results found.");
        }
        return;
    }
    for hit in &result.hits {
        let ts = format_timestamp(hit.timestamp_epoch);
        println!("---");
        println!(
            "**[{}]** `{}` · {} · {} (score: {:.3})",
            hit.tool, hit.session_id, ts, hit.role, hit.score
        );
        if let Some(ref path) = hit.source_path {
            println!("  source: {path}");
        }
        println!();
        println!("{}", hit.content.trim());
        println!();
    }
    println!("---");
    println!(
        "_{} candidates considered ({} shown after dedup + min-score floor)_",
        result.total_candidates,
        result.hits.len()
    );
}
