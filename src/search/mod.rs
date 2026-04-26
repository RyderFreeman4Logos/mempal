#![warn(clippy::all)]

use crate::core::{
    db::Database,
    project::{ProjectSearchScope, SearchResultSource},
    types::{RouteDecision, SearchResult},
    utils::source_file_or_synthetic,
};
use crate::embed::{EmbedError, Embedder};
use thiserror::Error;

use crate::search::filter::{build_filter_clause, build_vector_search_sql};
use rusqlite::OptionalExtension;

pub mod filter;
pub mod preview;
pub mod rerank;
pub mod route;

pub type Result<T> = std::result::Result<T, SearchError>;

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("failed to embed search query")]
    EmbedQuery(#[source] EmbedError),
    #[error("embedder returned no query vector")]
    MissingQueryVector,
    #[error("failed to count candidate drawers")]
    CountCandidateDrawers(#[source] rusqlite::Error),
    #[error("failed to serialize query vector")]
    SerializeQueryVector(#[source] serde_json::Error),
    #[error("top_k does not fit into i64")]
    InvalidTopK,
    #[error("failed to prepare search statement")]
    PrepareSearch(#[source] rusqlite::Error),
    #[error("failed to execute search query")]
    ExecuteSearch(#[source] rusqlite::Error),
    #[error("failed to collect search rows")]
    CollectSearchRows(#[source] rusqlite::Error),
    #[error("invalid embedding blob for drawer {drawer_id}")]
    InvalidEmbeddingBlob { drawer_id: String },
    #[error("failed to load taxonomy entries")]
    LoadTaxonomy(#[source] crate::core::db::DbError),
    #[error("failed to run keyword search")]
    KeywordSearch(#[source] crate::core::db::DbError),
    #[error(
        "embedding dimension mismatch: drawer_vectors uses {current_dim}d but embedder returned {new_dim}d; run `mempal reindex --embedder <name>` before searching with this backend"
    )]
    VectorDimensionMismatch { current_dim: usize, new_dim: usize },
}

pub async fn search<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    query: &str,
    wing: Option<&str>,
    room: Option<&str>,
    scope: &ProjectSearchScope,
    top_k: usize,
) -> Result<Vec<SearchResult>> {
    if top_k == 0 {
        return Ok(Vec::new());
    }

    let route = resolve_route(db, query, wing, room)?;

    let embeddings = embedder
        .embed(&[query])
        .await
        .map_err(SearchError::EmbedQuery)?;
    let query_vector = embeddings
        .into_iter()
        .next()
        .ok_or(SearchError::MissingQueryVector)?;
    if let Some(current_dim) = current_vector_dim(db).map_err(SearchError::KeywordSearch)?
        && current_dim != query_vector.len()
    {
        return Err(SearchError::VectorDimensionMismatch {
            current_dim,
            new_dim: query_vector.len(),
        });
    }

    search_with_vector(db, query, &query_vector, route, scope, top_k)
}

fn current_vector_dim(
    db: &Database,
) -> std::result::Result<Option<usize>, crate::core::db::DbError> {
    let exists: bool = db.conn().query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='drawer_vectors')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }

    let dimension = db
        .conn()
        .query_row(
            "SELECT vec_length(embedding) FROM drawer_vectors LIMIT 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .map(|value| value as usize);
    Ok(dimension)
}

pub fn search_with_vector(
    db: &Database,
    query: &str,
    query_vector: &[f32],
    route: RouteDecision,
    scope: &ProjectSearchScope,
    top_k: usize,
) -> Result<Vec<SearchResult>> {
    if top_k == 0 {
        return Ok(Vec::new());
    }

    // Hybrid search: vector + BM25, merged via RRF
    let vector_results = search_by_vector(db, query_vector, route.clone(), scope, top_k)?;

    let fts_ids = db
        .search_fts(
            query,
            route.wing.as_deref(),
            route.room.as_deref(),
            scope.mode_param(),
            scope.project_id.as_deref(),
            top_k,
        )
        .map_err(SearchError::KeywordSearch)?;

    let mut results = if fts_ids.is_empty() {
        vector_results
    } else {
        rrf_merge(vector_results, &fts_ids, &route, scope, db, top_k)
    };

    // Inject tunnel hints: for each result, check if its room exists in other wings
    inject_tunnel_hints_and_results(db, &mut results, scope);

    Ok(results)
}

pub fn search_bm25_only(
    db: &Database,
    query: &str,
    route: RouteDecision,
    scope: &ProjectSearchScope,
    top_k: usize,
) -> Result<Vec<SearchResult>> {
    if top_k == 0 {
        return Ok(Vec::new());
    }

    let fts_ids = db
        .search_fts(
            query,
            route.wing.as_deref(),
            route.room.as_deref(),
            scope.mode_param(),
            scope.project_id.as_deref(),
            top_k,
        )
        .map_err(SearchError::KeywordSearch)?;

    let mut results = rrf_merge(Vec::new(), &fts_ids, &route, scope, db, top_k);
    inject_tunnel_hints_and_results(db, &mut results, scope);
    Ok(results)
}

/// For each search result, check if its room appears in other wings (tunnel).
/// If so, add the other wing names as tunnel_hints and append any explicit
/// cross-project tunnel targets without applying the project filter.
///
/// Reads `[search].tunnel_fanout_cap` and `[search].tunnel_hints_display_cap`
/// from the hot-reload config snapshot.
fn inject_tunnel_hints_and_results(
    db: &Database,
    results: &mut Vec<SearchResult>,
    scope: &ProjectSearchScope,
) {
    let search_cfg = crate::core::hot_reload::global_hot_reload_state()
        .current()
        .search
        .clone();
    inject_tunnel_hints_with_cap(
        db,
        results,
        scope,
        search_cfg.tunnel_fanout_cap,
        search_cfg.tunnel_hints_display_cap,
    );
}

/// Tunnel-hint injection with explicit caps — factored out for unit tests so callers
/// can pin caps without touching the global hot-reload state.
///
/// `fanout_cap` bounds the number of injected cross-project rows per source result.
/// `hints_display_cap` bounds `tunnel_hints` string entries per result; excess wings
/// are replaced by a single `"… +N more"` sentinel as the last element.
pub(crate) fn inject_tunnel_hints_with_cap(
    db: &Database,
    results: &mut Vec<SearchResult>,
    scope: &ProjectSearchScope,
    fanout_cap: usize,
    hints_display_cap: usize,
) {
    let tunnels = match db.find_tunnels() {
        Ok(t) => t,
        Err(_) => return,
    };
    if tunnels.is_empty() {
        return;
    }

    // Build room → other-wings map
    let tunnel_map: std::collections::HashMap<&str, &[String]> = tunnels
        .iter()
        .map(|(room, wings)| (room.as_str(), wings.as_slice()))
        .collect();

    let mut tunnel_results = Vec::new();
    let mut seen_ids = results
        .iter()
        .map(|result| result.drawer_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    for result in results.iter_mut() {
        if let Some(room) = result.room.as_deref() {
            if let Some(wings) = tunnel_map.get(room) {
                let other_wings: Vec<&String> =
                    wings.iter().filter(|w| *w != &result.wing).collect();
                let total_other = other_wings.len();
                let mut hints: Vec<String> = other_wings
                    .iter()
                    .take(hints_display_cap)
                    .map(|w| (*w).clone())
                    .collect();
                if total_other > hints_display_cap {
                    hints.push(format!("… +{} more", total_other - hints_display_cap));
                }
                result.tunnel_hints = hints;
            }
            if fanout_cap == 0 {
                continue;
            }
            if let Ok(drawers) = db.tunnel_drawers_for_room(
                room,
                &result.drawer_id,
                scope.project_id.as_deref(),
                fanout_cap.saturating_add(1),
            ) {
                let mut added_from_this_result = 0usize;
                for tunnel in drawers {
                    if added_from_this_result >= fanout_cap {
                        break;
                    }
                    let drawer = tunnel.drawer;
                    if seen_ids.insert(drawer.id.clone()) {
                        tunnel_results.push(SearchResult {
                            drawer_id: drawer.id.clone(),
                            content: drawer.content,
                            wing: drawer.wing,
                            room: drawer.room,
                            source_file: source_file_or_synthetic(
                                &drawer.id,
                                drawer.source_file.as_deref(),
                            ),
                            source: SearchResultSource::TunnelCrossProject,
                            similarity: result.similarity,
                            route: result.route.clone(),
                            tunnel_hints: vec![],
                        });
                        added_from_this_result += 1;
                    }
                }
            }
        }
    }
    results.extend(tunnel_results);
}

/// Reciprocal Rank Fusion: merge vector and BM25 ranked lists.
/// RRF score = sum(1 / (k + rank)) across both lists, with k=60.
fn rrf_merge(
    vector_results: Vec<SearchResult>,
    fts_ids: &[(String, f64)],
    route: &RouteDecision,
    scope: &ProjectSearchScope,
    db: &Database,
    top_k: usize,
) -> Vec<SearchResult> {
    use std::collections::HashMap;

    const RRF_K: f64 = 60.0;

    let mut scores: HashMap<String, f64> = HashMap::new();
    let mut result_map: HashMap<String, SearchResult> = HashMap::new();

    // Score vector results
    for (rank, result) in vector_results.into_iter().enumerate() {
        let score = 1.0 / (RRF_K + rank as f64 + 1.0);
        scores.insert(result.drawer_id.clone(), score);
        result_map.insert(result.drawer_id.clone(), result);
    }

    // Score FTS results and merge
    for (rank, (id, _bm25_score)) in fts_ids.iter().enumerate() {
        let score = 1.0 / (RRF_K + rank as f64 + 1.0);
        *scores.entry(id.clone()).or_default() += score;

        // If this ID wasn't in vector results, load the drawer
        if !result_map.contains_key(id) {
            if let Ok(Some(drawer)) = db.get_drawer(id) {
                result_map.insert(
                    id.clone(),
                    SearchResult {
                        drawer_id: drawer.id,
                        content: drawer.content,
                        wing: drawer.wing,
                        room: drawer.room,
                        source_file: source_file_or_synthetic(id, drawer.source_file.as_deref()),
                        source: scope
                            .classify_row(db.drawer_project_id(id).ok().flatten().as_deref()),
                        similarity: 0.0, // will be overwritten below
                        route: route.clone(),
                        tunnel_hints: vec![],
                    },
                );
            }
        }
    }

    // Sort by RRF score descending, fill in similarity field
    let mut merged: Vec<SearchResult> = scores
        .into_iter()
        .filter_map(|(id, rrf_score)| {
            let mut result = result_map.remove(&id)?;
            result.similarity = rrf_score as f32;
            Some(result)
        })
        .collect();
    merged.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    merged.truncate(top_k);
    merged
}

/// Compute the KNN `k` parameter for sqlite-vec, clamped to its hardcoded
/// limit of 4096. Uses `top_k * 50` as the recall multiplier (allowing
/// post-filter shrinkage from wing/room/project predicates), floored at
/// 100 to avoid degenerate single-digit recall on tiny `top_k` values.
///
/// When the database grows beyond 4096 drawers the KNN result is an
/// *approximate* subset — callers that need exact recall on a small
/// candidate set should use `search_by_vector_scoped_exact` instead.
pub fn compute_knn_k(top_k: usize) -> i64 {
    let raw = top_k.saturating_mul(50);
    let raw_i64 = i64::try_from(raw).unwrap_or(i64::MAX);
    raw_i64.clamp(100, 4_096)
}

pub fn search_by_vector(
    db: &Database,
    query_vector: &[f32],
    route: RouteDecision,
    scope: &ProjectSearchScope,
    top_k: usize,
) -> Result<Vec<SearchResult>> {
    if top_k == 0 {
        return Ok(Vec::new());
    }

    let applied_wing = route.wing.as_deref();
    let applied_room = route.room.as_deref();

    let count_sql = format!(
        "SELECT COUNT(*) FROM drawers d {}",
        build_filter_clause("d", 1, 2, 3, 4)
    );
    let candidate_count: i64 = db
        .conn()
        .query_row(
            &count_sql,
            (
                applied_wing,
                applied_room,
                scope.mode_param(),
                scope.project_id.as_deref(),
            ),
            |row| row.get(0),
        )
        .map_err(SearchError::CountCandidateDrawers)?;
    if candidate_count == 0 {
        return Ok(Vec::new());
    }

    // When the candidate set fits within the sqlite-vec KNN limit, use
    // the exact in-memory path regardless of scope mode — this avoids
    // approximate recall loss and sidesteps the 4096 KNN cap entirely.
    if candidate_count <= 4_096 {
        return search_by_vector_scoped_exact(
            db,
            query_vector,
            route.clone(),
            applied_wing,
            applied_room,
            top_k,
            scope,
        );
    }

    let query_json =
        serde_json::to_string(query_vector).map_err(SearchError::SerializeQueryVector)?;
    let top_k_i64 = i64::try_from(top_k).map_err(|_| SearchError::InvalidTopK)?;
    let knn_k = compute_knn_k(top_k);

    let search_sql = build_vector_search_sql(scope.mode);

    let mut statement = db
        .conn()
        .prepare(&search_sql)
        .map_err(SearchError::PrepareSearch)?;
    let results = statement
        .query_map(
            (
                query_json.as_str(),
                knn_k,
                scope.mode_param(),
                scope.project_id.as_deref(),
                applied_wing,
                applied_room,
                top_k_i64,
            ),
            |row| {
                let distance: f64 = row.get(6)?;
                let drawer_id: String = row.get(0)?;
                let source_file = row.get::<_, Option<String>>(4)?;
                let row_project_id = row.get::<_, Option<String>>(5)?;
                Ok(SearchResult {
                    drawer_id: drawer_id.clone(),
                    content: row.get(1)?,
                    wing: row.get(2)?,
                    room: row.get(3)?,
                    source_file: source_file_or_synthetic(&drawer_id, source_file.as_deref()),
                    source: scope.classify_row(row_project_id.as_deref()),
                    similarity: (1.0_f64 - distance) as f32,
                    route: route.clone(),
                    tunnel_hints: vec![],
                })
            },
        )
        .map_err(SearchError::ExecuteSearch)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(SearchError::CollectSearchRows)?;

    Ok(results)
}

fn search_by_vector_scoped_exact(
    db: &Database,
    query_vector: &[f32],
    route: RouteDecision,
    applied_wing: Option<&str>,
    applied_room: Option<&str>,
    top_k: usize,
    scope: &ProjectSearchScope,
) -> Result<Vec<SearchResult>> {
    let top_k = i64::try_from(top_k).map_err(|_| SearchError::InvalidTopK)?;
    // Use the full filter clause so all scope modes (all / project /
    // project_plus_global / null_only) work correctly through the exact path.
    let filter = build_filter_clause("d", 1, 2, 3, 4);
    let search_sql = format!(
        r#"
        SELECT d.id, d.content, d.wing, d.room, d.source_file, d.project_id, v.embedding
        FROM drawer_vectors v
        JOIN drawers d ON d.id = v.id
        {filter}
        "#
    );
    let mut statement = db
        .conn()
        .prepare(&search_sql)
        .map_err(SearchError::PrepareSearch)?;
    let mut rows = statement
        .query_map(
            (
                applied_wing,
                applied_room,
                scope.mode_param(),
                scope.project_id.as_deref(),
            ),
            |row| {
                let drawer_id: String = row.get(0)?;
                let source_file = row.get::<_, Option<String>>(4)?;
                let row_project_id = row.get::<_, Option<String>>(5)?;
                let embedding = row.get::<_, Vec<u8>>(6)?;
                Ok((
                    drawer_id.clone(),
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    source_file_or_synthetic(&drawer_id, source_file.as_deref()),
                    row_project_id,
                    embedding,
                ))
            },
        )
        .map_err(SearchError::ExecuteSearch)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(SearchError::CollectSearchRows)?;

    rows.sort_by(|a, b| {
        let a_distance = cosine_distance_from_blob(&a.0, &a.6, query_vector);
        let b_distance = cosine_distance_from_blob(&b.0, &b.6, query_vector);
        match (a_distance, b_distance) {
            (Ok(left), Ok(right)) => left
                .partial_cmp(&right)
                .unwrap_or(std::cmp::Ordering::Equal),
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => a.0.cmp(&b.0),
        }
    });

    let results = rows
        .into_iter()
        .take(top_k as usize)
        .map(
            |(drawer_id, content, wing, room, source_file, row_project_id, embedding)| {
                let distance = cosine_distance_from_blob(&drawer_id, &embedding, query_vector)?;
                Ok(SearchResult {
                    drawer_id: drawer_id.clone(),
                    content,
                    wing,
                    room,
                    source_file,
                    source: scope.classify_row(row_project_id.as_deref()),
                    similarity: (1.0_f64 - distance) as f32,
                    route: route.clone(),
                    tunnel_hints: vec![],
                })
            },
        )
        .collect::<Result<Vec<_>>>()?;
    Ok(results)
}

fn cosine_distance_from_blob(
    drawer_id: &str,
    embedding_blob: &[u8],
    query_vector: &[f32],
) -> Result<f64> {
    if embedding_blob.len() % std::mem::size_of::<f32>() != 0 {
        return Err(SearchError::InvalidEmbeddingBlob {
            drawer_id: drawer_id.to_string(),
        });
    }
    let embedding = embedding_blob
        .chunks_exact(std::mem::size_of::<f32>())
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect::<Vec<_>>();
    if embedding.len() != query_vector.len() {
        return Err(SearchError::InvalidEmbeddingBlob {
            drawer_id: drawer_id.to_string(),
        });
    }

    let dot = embedding
        .iter()
        .zip(query_vector.iter())
        .map(|(left, right)| f64::from(*left) * f64::from(*right))
        .sum::<f64>();
    let left_norm = embedding
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    let right_norm = query_vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    let cosine_similarity = if left_norm == 0.0 || right_norm == 0.0 {
        0.0
    } else {
        dot / (left_norm * right_norm)
    };
    Ok((1.0 - cosine_similarity).clamp(0.0, 2.0))
}

pub fn resolve_route(
    db: &Database,
    query: &str,
    wing: Option<&str>,
    room: Option<&str>,
) -> Result<RouteDecision> {
    if wing.is_some() || room.is_some() {
        let scope = match (wing, room) {
            (Some(wing), Some(room)) => format!("{wing}/{room}"),
            (Some(wing), None) => wing.to_string(),
            (None, Some(room)) => format!("room={room}"),
            (None, None) => "global".to_string(),
        };
        return Ok(RouteDecision {
            wing: wing.map(ToOwned::to_owned),
            room: room.map(ToOwned::to_owned),
            confidence: 1.0,
            reason: format!("explicit filters provided: {scope}"),
        });
    }

    let taxonomy = db.taxonomy_entries().map_err(SearchError::LoadTaxonomy)?;
    let route = route::route_query(query, &taxonomy);
    if route.confidence >= 0.5 {
        return Ok(route);
    }

    Ok(RouteDecision {
        wing: None,
        room: None,
        confidence: route.confidence,
        reason: route.reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::project::{ProjectSearchScope, SearchResultSource};
    use crate::core::types::{Drawer, RouteDecision, SearchResult, SourceType};
    use tempfile::TempDir;

    fn make_drawer(id: &str, wing: &str, room: &str) -> Drawer {
        Drawer {
            id: id.to_string(),
            content: format!("content for {id}"),
            wing: wing.to_string(),
            room: Some(room.to_string()),
            source_file: Some(format!("{id}.md")),
            source_type: SourceType::Manual,
            added_at: "1700000000".to_string(),
            chunk_index: None,
            importance: 0,
        }
    }

    fn make_result(drawer: &Drawer) -> SearchResult {
        SearchResult {
            drawer_id: drawer.id.clone(),
            content: drawer.content.clone(),
            wing: drawer.wing.clone(),
            room: drawer.room.clone(),
            source_file: drawer.source_file.clone().unwrap_or_default(),
            source: SearchResultSource::Project,
            similarity: 0.9,
            route: RouteDecision {
                wing: None,
                room: None,
                confidence: 0.0,
                reason: "test".to_string(),
            },
            tunnel_hints: vec![],
        }
    }

    fn seed_cross_project(db: &Database, source: &Drawer, beta_count: usize) {
        db.insert_drawer_with_project(source, Some("proj-a"))
            .expect("insert source");
        for i in 0..beta_count {
            let id = format!("beta-{i}");
            let drawer = make_drawer(&id, "beta", "decision");
            db.insert_drawer_with_project(&drawer, Some("proj-b"))
                .expect("insert beta");
        }
    }

    fn scoped_to_proj_a() -> ProjectSearchScope {
        ProjectSearchScope::from_request(Some("proj-a".to_string()), false, false, false)
    }

    #[test]
    fn tunnel_fanout_cap_limits_cross_project_expansion() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");
        let source = make_drawer("alpha-1", "alpha", "decision");
        seed_cross_project(&db, &source, 10);

        let mut results = vec![make_result(&source)];
        inject_tunnel_hints_with_cap(&db, &mut results, &scoped_to_proj_a(), 3, usize::MAX);

        assert_eq!(
            results.len(),
            4,
            "expected 1 source + 3 tunnel = 4, got {}",
            results.len()
        );
        assert_eq!(results[0].drawer_id, "alpha-1");
        for result in &results[1..] {
            assert_eq!(result.source, SearchResultSource::TunnelCrossProject);
            assert_eq!(result.wing, "beta");
        }
    }

    #[test]
    fn tunnel_fanout_cap_zero_disables_cross_project_rows() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");
        let source = make_drawer("alpha-1", "alpha", "decision");
        seed_cross_project(&db, &source, 5);

        let mut results = vec![make_result(&source)];
        inject_tunnel_hints_with_cap(&db, &mut results, &scoped_to_proj_a(), 0, usize::MAX);

        assert_eq!(results.len(), 1, "cap=0 must not add tunnel drawers");
        assert_eq!(
            results[0].tunnel_hints,
            vec!["beta".to_string()],
            "wing hints should still populate with cap=0"
        );
    }

    #[test]
    fn tunnel_fanout_cap_large_returns_all_available() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");
        let source = make_drawer("alpha-1", "alpha", "decision");
        seed_cross_project(&db, &source, 2);

        let mut results = vec![make_result(&source)];
        inject_tunnel_hints_with_cap(&db, &mut results, &scoped_to_proj_a(), 100, usize::MAX);

        assert_eq!(
            results.len(),
            3,
            "cap>available must return all {} available",
            2
        );
    }

    #[test]
    fn tunnel_fanout_cap_applies_per_source_result() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");

        let alpha = make_drawer("alpha-1", "alpha", "decision");
        let gamma = make_drawer("gamma-1", "gamma", "decision");
        db.insert_drawer_with_project(&alpha, Some("proj-a"))
            .expect("insert alpha");
        db.insert_drawer_with_project(&gamma, Some("proj-a"))
            .expect("insert gamma");
        for i in 0..10 {
            let id = format!("beta-{i}");
            let drawer = make_drawer(&id, "beta", "decision");
            db.insert_drawer_with_project(&drawer, Some("proj-b"))
                .expect("insert beta");
        }

        let mut results = vec![make_result(&alpha), make_result(&gamma)];
        inject_tunnel_hints_with_cap(&db, &mut results, &scoped_to_proj_a(), 2, usize::MAX);

        // SQL LIMIT = fanout_cap + 1 = 3.  Alpha's query returns 3 beta rows
        // (beta-9, beta-8, beta-7 DESC order); alpha's Rust cap adds 2 (beta-9,
        // beta-8).  Gamma's query also returns the same 3 rows; beta-9 and
        // beta-8 are already in `seen_ids`, so only beta-7 is fresh → 1 tunnel
        // row from gamma.  Total = 2 source + 2 (alpha) + 1 (gamma) = 5.
        assert_eq!(
            results.len(),
            5,
            "expected 2 source + 2 (alpha) + 1 (gamma) = 5, got {}",
            results.len()
        );
    }

    #[test]
    fn tunnel_drawers_for_room_sql_limit_bounds_returned_rows() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");
        let source = make_drawer("alpha-1", "alpha", "decision");
        // Insert 20 beta drawers — well above any reasonable fanout cap.
        seed_cross_project(&db, &source, 20);

        let limit: usize = 5;
        let drawers = db
            .tunnel_drawers_for_room("decision", "alpha-1", Some("proj-a"), limit)
            .expect("query");

        assert_eq!(
            drawers.len(),
            limit,
            "SQL LIMIT should bound returned rows to {limit}, got {}",
            drawers.len()
        );
    }

    // --- tunnel_hints display cap tests ---

    fn seed_many_wings(db: &Database, source_wing: &str, sibling_count: usize, room: &str) {
        let source = make_drawer(&format!("{source_wing}-1"), source_wing, room);
        db.insert_drawer_with_project(&source, None)
            .expect("insert source");
        for i in 0..sibling_count {
            let id = format!("sibling-{i}");
            let wing = format!("wing-{i:02}");
            let d = make_drawer(&id, &wing, room);
            db.insert_drawer_with_project(&d, None)
                .expect("insert sibling");
        }
    }

    #[test]
    fn test_tunnel_hints_capped_at_default_when_many_wings() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");
        seed_many_wings(&db, "alpha", 49, "room-shared");

        let source = make_drawer("alpha-1", "alpha", "room-shared");
        let mut results = vec![make_result(&source)];
        inject_tunnel_hints_with_cap(&db, &mut results, &ProjectSearchScope::all_projects(), 0, 8);

        // 8 real hints + 1 sentinel = 9 = display_cap + 1
        assert!(
            results[0].tunnel_hints.len() <= 9,
            "expected <= 9 hints, got {}",
            results[0].tunnel_hints.len()
        );
        let last = results[0].tunnel_hints.last().expect("has entries");
        assert!(last.starts_with("… +"), "expected sentinel, got {:?}", last);
    }

    #[test]
    fn test_tunnel_hints_no_sentinel_when_under_cap() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");
        seed_many_wings(&db, "alpha", 5, "room-shared");

        let source = make_drawer("alpha-1", "alpha", "room-shared");
        let mut results = vec![make_result(&source)];
        inject_tunnel_hints_with_cap(&db, &mut results, &ProjectSearchScope::all_projects(), 0, 8);

        assert_eq!(results[0].tunnel_hints.len(), 5, "exactly 5 sibling hints");
        assert!(
            !results[0].tunnel_hints.iter().any(|h| h.starts_with("… +")),
            "no sentinel expected when under cap"
        );
    }

    #[test]
    fn test_tunnel_hints_sentinel_count_is_correct() {
        // 49 siblings, cap=8 → show 8, sentinel = "… +41 more"
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");
        seed_many_wings(&db, "alpha", 49, "room-shared");

        let source = make_drawer("alpha-1", "alpha", "room-shared");
        let mut results = vec![make_result(&source)];
        inject_tunnel_hints_with_cap(&db, &mut results, &ProjectSearchScope::all_projects(), 0, 8);

        let sentinel = results[0].tunnel_hints.last().expect("has sentinel");
        assert_eq!(sentinel, "… +41 more");
    }

    #[test]
    fn test_tunnel_hints_excludes_self_wing() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");
        seed_many_wings(&db, "alpha", 10, "room-shared");

        let source = make_drawer("alpha-1", "alpha", "room-shared");
        let mut results = vec![make_result(&source)];
        inject_tunnel_hints_with_cap(&db, &mut results, &ProjectSearchScope::all_projects(), 0, 8);

        assert!(
            !results[0].tunnel_hints.iter().any(|h| h == "alpha"),
            "own wing must not appear in tunnel_hints"
        );
    }

    #[test]
    fn test_tunnel_hints_cap_config_override() {
        // display_cap=3 → 3 real hints + 1 sentinel
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");
        seed_many_wings(&db, "alpha", 10, "room-shared");

        let source = make_drawer("alpha-1", "alpha", "room-shared");
        let mut results = vec![make_result(&source)];
        inject_tunnel_hints_with_cap(&db, &mut results, &ProjectSearchScope::all_projects(), 0, 3);

        assert_eq!(
            results[0].tunnel_hints.len(),
            4,
            "expected 3 real + 1 sentinel = 4"
        );
        let sentinel = results[0].tunnel_hints.last().expect("has sentinel");
        assert!(sentinel.starts_with("… +"), "last entry should be sentinel");
    }

    // --- compute_knn_k unit tests ---

    #[test]
    fn compute_knn_k_zero_top_k_returns_floor() {
        assert_eq!(compute_knn_k(0), 100);
    }

    #[test]
    fn compute_knn_k_one_returns_floor() {
        // 1 * 50 = 50, clamped up to 100
        assert_eq!(compute_knn_k(1), 100);
    }

    #[test]
    fn compute_knn_k_small_top_k() {
        // 10 * 50 = 500
        assert_eq!(compute_knn_k(10), 500);
    }

    #[test]
    fn compute_knn_k_at_ceiling_boundary() {
        // 81 * 50 = 4050, still under 4096
        assert_eq!(compute_knn_k(81), 4050);
        // 82 * 50 = 4100, clamped to 4096
        assert_eq!(compute_knn_k(82), 4096);
    }

    #[test]
    fn compute_knn_k_large_top_k_clamped() {
        assert_eq!(compute_knn_k(100), 4096);
        assert_eq!(compute_knn_k(1_000), 4096);
        assert_eq!(compute_knn_k(10_000), 4096);
    }

    #[test]
    fn compute_knn_k_always_in_bounds() {
        for top_k in [0, 1, 2, 10, 50, 81, 82, 100, 1_000, 10_000, usize::MAX] {
            let k = compute_knn_k(top_k);
            assert!(k >= 100, "k={k} below floor for top_k={top_k}");
            assert!(k <= 4_096, "k={k} above ceiling for top_k={top_k}");
        }
    }
}
