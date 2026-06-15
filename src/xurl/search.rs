use std::collections::{HashMap, HashSet};

use serde::Serialize;
use sha2::{Digest, Sha256};

struct TurnBestEntry {
    score: f32,
    session_id: String,
    tool: String,
    role: String,
    content: String,
    timestamp_epoch: f64,
    source_path: Option<String>,
    hermes_profile: Option<String>,
    session_title: Option<String>,
    session_source: Option<String>,
    message_id: Option<String>,
    tool_name: Option<String>,
    tool_call_id: Option<String>,
    previous_message_id: Option<String>,
    next_message_id: Option<String>,
}

use crate::core::config::SearchRerankerConfig;
use crate::core::db::Database;
use crate::embed::Embedder;
use crate::xurl::store::{TurnFilter, push_filter_conditions};
use crate::xurl::{XurlError, XurlResult};

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub turn_id: String,
    pub session_id: String,
    pub tool: String,
    pub role: String,
    pub content: String,
    pub timestamp_epoch: f64,
    pub score: f32,
    pub source_path: Option<String>,
    pub hermes_profile: Option<String>,
    pub session_title: Option<String>,
    pub session_source: Option<String>,
    pub message_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    pub previous_message_id: Option<String>,
    pub next_message_id: Option<String>,
}

/// Aggregated result returned by [`search`].
#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    /// Number of hits that passed `min_score_floor` before pagination.
    pub passing_total: usize,
    /// Highest score among hits that were filtered out by `min_score_floor`.
    pub best_score_below_floor: Option<f32>,
    /// Total unique candidates (after per-turn and per-content dedup, before floor and limit).
    pub total_candidates: usize,
    /// The min-score threshold that was applied, if any.
    pub min_score_floor: Option<f32>,
    /// Non-sensitive diagnostics for optional retrieval stages such as reranking.
    pub warnings: Vec<String>,
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
    /// Optional top-K reranker. `None` keeps xurl search fully local and preserves vector order.
    pub reranker: Option<SearchRerankerConfig>,
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
        reranker,
    } = opts;
    if limit == 0 {
        return Ok(SearchResult {
            hits: Vec::new(),
            passing_total: 0,
            best_score_below_floor: None,
            total_candidates: 0,
            min_score_floor: min_score,
            warnings: Vec::new(),
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
    push_filter_conditions(
        &filter,
        Some("ct"),
        &mut conditions,
        &mut param_values,
        &mut pidx,
    );

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let sql = format!(
        "SELECT ctv.turn_id, ctv.vector, \
         ct.session_id, ct.tool, ct.role, ct.content, ct.timestamp_epoch, ct.project_path, \
         ct.hermes_profile, ct.session_title, ct.session_source, ct.message_id, \
         ct.tool_name, ct.tool_call_id, ct.previous_message_id, ct.next_message_id \
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
        hermes_profile: Option<String>,
        session_title: Option<String>,
        session_source: Option<String>,
        message_id: Option<String>,
        tool_name: Option<String>,
        tool_call_id: Option<String>,
        previous_message_id: Option<String>,
        next_message_id: Option<String>,
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
                hermes_profile: row.get(8)?,
                session_title: row.get(9)?,
                session_source: row.get(10)?,
                message_id: row.get(11)?,
                tool_name: row.get(12)?,
                tool_call_id: row.get(13)?,
                previous_message_id: row.get(14)?,
                next_message_id: row.get(15)?,
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
        let entry = best_by_turn
            .entry(row.turn_id.clone())
            .or_insert(TurnBestEntry {
                score: -1.0,
                session_id: row.session_id,
                tool: row.tool,
                role: row.role,
                content: row.content,
                timestamp_epoch: row.timestamp_epoch,
                source_path: row.project_path,
                hermes_profile: row.hermes_profile,
                session_title: row.session_title,
                session_source: row.session_source,
                message_id: row.message_id,
                tool_name: row.tool_name,
                tool_call_id: row.tool_call_id,
                previous_message_id: row.previous_message_id,
                next_message_id: row.next_message_id,
            });
        if score > entry.score {
            entry.score = score;
        }
    }

    // Convert to SearchHit and sort by descending score.
    let mut all_hits: Vec<SearchHit> = best_by_turn
        .into_iter()
        .map(|(turn_id, entry)| SearchHit {
            turn_id,
            session_id: entry.session_id,
            tool: entry.tool,
            role: entry.role,
            content: entry.content,
            timestamp_epoch: entry.timestamp_epoch,
            score: entry.score,
            source_path: entry.source_path,
            hermes_profile: entry.hermes_profile,
            session_title: entry.session_title,
            session_source: entry.session_source,
            message_id: entry.message_id,
            tool_name: entry.tool_name,
            tool_call_id: entry.tool_call_id,
            previous_message_id: entry.previous_message_id,
            next_message_id: entry.next_message_id,
        })
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

    let passing_total = passing.len();
    let mut passing = passing;
    let mut warnings = Vec::new();
    if let Some(reranker_config) = reranker {
        let documents = passing
            .iter()
            .map(|hit| hit.content.as_str())
            .collect::<Vec<_>>();
        let outcome = crate::search::rerank::maybe_rerank_indices_with_config(
            &reranker_config,
            query,
            documents,
        )
        .await;
        warnings.extend(outcome.warnings);
        passing = outcome
            .order
            .into_iter()
            .filter_map(|index| passing.get(index).cloned())
            .collect();
    }
    let hits = passing.into_iter().skip(offset).take(limit).collect();

    Ok(SearchResult {
        hits,
        passing_total,
        best_score_below_floor,
        total_candidates,
        min_score_floor: min_score,
        warnings,
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

/// Render search hits in markdown format.
pub fn format_hits_markdown(result: &SearchResult) -> String {
    let mut output = String::new();
    if result.hits.is_empty() {
        if result.passing_total == 0 {
            if let Some(best) = result.best_score_below_floor {
                let floor = result.min_score_floor.unwrap_or(0.0);
                let count = result.total_candidates;
                let noun = if count == 1 { "result" } else { "results" };
                let target = if count == 1 { "it" } else { "them" };
                let suggested = (best * 10.0).floor() / 10.0;
                output.push_str(&format!(
                    "No confident match (best score {best:.3} < floor {floor:.2}; \
                     {count} {noun} below the {floor:.2} floor - rerun with --min-score {suggested:.1} or lower \
                     to view {target})\n"
                ));
            } else {
                output.push_str("No results found.\n");
            }
        } else {
            output.push_str("No more results on this page.\n");
        }
        return output;
    }
    for hit in &result.hits {
        let ts = format_timestamp(hit.timestamp_epoch);
        output.push_str("---\n");
        output.push_str(&format!(
            "**[{}]** `{}` · {} · {} (score: {:.3})",
            hit.tool, hit.session_id, ts, hit.role, hit.score
        ));
        output.push('\n');
        if let Some(ref path) = hit.source_path {
            output.push_str(&format!("  source: {path}\n"));
        }
        if hit.tool == "hermes" {
            let mut parts = Vec::new();
            if let Some(ref profile) = hit.hermes_profile {
                parts.push(format!("profile={profile}"));
            }
            parts.push(format!("session={}", hit.session_id));
            if let Some(ref message_id) = hit.message_id {
                parts.push(format!("message={message_id}"));
            }
            if let Some(ref source) = hit.session_source {
                parts.push(format!("source={source}"));
            }
            if let Some(ref title) = hit.session_title {
                parts.push(format!("title={title}"));
            }
            output.push_str(&format!("  citation: hermes {}\n", parts.join(" ")));
            if hit.previous_message_id.is_some() || hit.next_message_id.is_some() {
                output.push_str(&format!(
                    "  neighbors: prev={} next={}\n",
                    hit.previous_message_id.as_deref().unwrap_or("-"),
                    hit.next_message_id.as_deref().unwrap_or("-")
                ));
            }
            if let Some(ref tool_name) = hit.tool_name {
                output.push_str(&format!("  tool: {tool_name}\n"));
            }
        }
        output.push('\n');
        output.push_str(hit.content.trim());
        output.push_str("\n\n");
    }
    output.push_str("---\n");
    output.push_str(&format!(
        "_{} candidates considered ({} shown after dedup + min-score floor)_",
        result.total_candidates,
        result.hits.len()
    ));
    output.push('\n');
    output
}

/// Print search hits in markdown format to stdout.
pub fn print_hits_markdown(result: &SearchResult) {
    print!("{}", format_hits_markdown(result));
}
