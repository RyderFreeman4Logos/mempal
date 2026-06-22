#![warn(clippy::all)]

use std::time::Duration;

use crate::algo::ranking::{RankedMemoryItem, ReciprocalRankFusion};
use crate::core::decay::{search_decay_factor_at, validity_window_contains_at};
use crate::core::{
    db::{Database, FtsMetadataFilters, FtsSearchScope},
    project::{ProjectSearchScope, SearchResultSource},
    types::{
        AnchorKind, Drawer, KnowledgeStatus, KnowledgeTier, MemoryDomain, MemoryKind,
        RouteDecision, SearchResult, SourceType,
    },
    utils::source_file_or_synthetic,
};
use crate::embed::{EmbedError, Embedder, global_embed_status};
use thiserror::Error;

use crate::search::filter::{
    RetrievalFilterParamIndexes, build_retrieval_filter_clause, build_vector_search_sql,
};
use rusqlite::{OptionalExtension, params_from_iter};

pub mod filter;
pub mod preview;
pub mod rerank;
pub mod route;
pub mod tiered;

const EXACT_VECTOR_CANDIDATE_LIMIT: i64 = 4_096;

pub type Result<T> = std::result::Result<T, SearchError>;

impl RankedMemoryItem for SearchResult {
    fn memory_id(&self) -> &str {
        &self.drawer_id
    }

    fn similarity_score(&self) -> f32 {
        self.similarity
    }

    fn effective_importance(&self) -> f64 {
        self.effective_importance
    }
}

// --- Upstream knowledge-filter types ---

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchFilters {
    pub memory_kind: Option<String>,
    pub domain: Option<String>,
    pub field: Option<String>,
    pub tier: Option<String>,
    pub status: Option<String>,
    pub anchor_kind: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchOptions {
    pub filters: SearchFilters,
    pub with_neighbors: bool,
    pub include_raw_turns: bool,
    pub include_expired: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Hybrid,
    Bm25Only,
}

impl SearchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hybrid => "hybrid",
            Self::Bm25Only => "bm25_only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorSearchCircuit {
    /// True when the sticky embedder circuit is open and vector search falls
    /// back to BM25 unless fallback is disabled.
    pub open: bool,
    /// Consecutive embed failures since the last success.
    pub failure_count: u64,
    /// Sticky-failure threshold that opens the circuit.
    pub failure_threshold: u64,
    /// Whether BM25 fallback is enabled at all.
    pub bm25_fallback_enabled: bool,
    /// Per-query vector-embedding deadline that can still trigger BM25
    /// fallback even when the embedding endpoint itself is reachable.
    pub search_deadline_secs: u64,
    /// Current sticky vector-search mode derived from the circuit state.
    pub vector_search_mode: SearchMode,
}

impl VectorSearchCircuit {
    /// Snapshot the current vector-search availability policy from the global
    /// embed health state and the active configuration.
    pub fn current() -> Self {
        let config = crate::core::config::ConfigHandle::current();
        let embed_snapshot = global_embed_status().snapshot();
        Self::from_config_and_snapshot(config.as_ref(), &embed_snapshot)
    }

    /// Build a vector-search policy snapshot from an explicit config/snapshot
    /// pair. This keeps callers on one shared decision path without forcing an
    /// extra status read.
    pub fn from_config_and_snapshot(
        config: &crate::core::config::Config,
        embed_snapshot: &crate::embed::EmbedHealthSnapshot,
    ) -> Self {
        Self {
            open: embed_snapshot.degraded,
            failure_count: embed_snapshot.fail_count,
            failure_threshold: config.embed.degradation.degrade_after_n_failures,
            bm25_fallback_enabled: config.search.bm25_fallback,
            search_deadline_secs: config.embed.retry.search_deadline_secs,
            vector_search_mode: if config.search.bm25_fallback && embed_snapshot.degraded {
                SearchMode::Bm25Only
            } else {
                SearchMode::Hybrid
            },
        }
    }
}

/// Wording for the sticky embedder circuit opening after repeated failures.
pub fn bm25_fallback_warning_degraded(fail_count: u64) -> String {
    format!(
        "embedding backend is degraded after {fail_count} failures; using BM25-only search until recovery (retry unlikely to help)"
    )
}

/// Wording for embedding build/runtime failures while BM25 fallback is enabled.
pub fn bm25_fallback_warning_embed_error(error: &str) -> String {
    format!("embedding unavailable; using BM25-only search: {error} (retry may help)")
}

/// Wording for query-embedding deadline expiry while BM25 fallback is enabled.
pub fn bm25_fallback_warning_timeout(deadline_secs: u64) -> String {
    format!(
        "embedding deadline exceeded after {deadline_secs}s; using BM25-only search (retry may help)"
    )
}

/// Wording for the edge case where the embedder returns no query vector.
pub fn bm25_fallback_warning_missing_query_vector() -> String {
    "embedding returned no query vector; using BM25-only search".to_string()
}

/// Wording for the edge case where the query vector dimension mismatches the
/// stored vector index.
pub fn bm25_fallback_warning_dimension_mismatch(new_dim: usize, current_dim: usize) -> String {
    format!(
        "embedding dimension mismatch ({new_dim}d query vs {current_dim}d index); using BM25-only search"
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchOutcome {
    pub results: Vec<SearchResult>,
    pub search_mode: SearchMode,
    pub warnings: Vec<String>,
}

impl SearchOutcome {
    fn hybrid(results: Vec<SearchResult>) -> Self {
        Self {
            results,
            search_mode: SearchMode::Hybrid,
            warnings: Vec::new(),
        }
    }

    fn bm25_only(results: Vec<SearchResult>, warning: String) -> Self {
        Self {
            results,
            search_mode: SearchMode::Bm25Only,
            warnings: vec![warning],
        }
    }
}

pub async fn maybe_rerank_search_results(
    query: &str,
    results: Vec<SearchResult>,
) -> rerank::RerankOutcome {
    let config = crate::core::config::ConfigHandle::current();
    rerank::maybe_rerank_with_config_and_policy(
        &config.search.reranker,
        &config.privacy.remote_calls,
        query,
        results,
    )
    .await
}

async fn apply_optional_reranker_to_outcome(
    query: &str,
    mut outcome: SearchOutcome,
) -> SearchOutcome {
    let config = crate::core::config::ConfigHandle::current();
    let rerank_outcome = rerank::maybe_rerank_with_config_and_policy(
        &config.search.reranker,
        &config.privacy.remote_calls,
        query,
        outcome.results,
    )
    .await;
    outcome.results = rerank_outcome.results;
    outcome.warnings.extend(rerank_outcome.warnings);
    outcome
}

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("failed to embed search query")]
    EmbedQuery(#[source] EmbedError),
    #[error("embedding deadline exceeded after {deadline_secs}s")]
    EmbedQueryTimeout { deadline_secs: u64 },
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
    #[error("failed to load neighbor chunks")]
    LoadNeighbors(#[source] crate::core::db::DbError),
    #[error("failed to load search result drawer metadata")]
    LoadDrawer(#[source] crate::core::db::DbError),
    #[error("failed to load temporal search metadata")]
    LoadTemporalMetadata(#[source] rusqlite::Error),
    #[error(
        "embedding dimension mismatch: drawer_vectors uses {current_dim}d but embedder returned {new_dim}d; run `mempal reindex --embedder <name>` before searching with this backend"
    )]
    VectorDimensionMismatch { current_dim: usize, new_dim: usize },
}

// ---------------------------------------------------------------------------
// Async entry points
// ---------------------------------------------------------------------------

/// Simple async search with project scope (fork API).
pub async fn search<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    query: &str,
    wing: Option<&str>,
    room: Option<&str>,
    scope: &ProjectSearchScope,
    top_k: usize,
) -> Result<Vec<SearchResult>> {
    Ok(search_with_all_options_outcome(
        db,
        embedder,
        query,
        wing,
        room,
        scope,
        SearchOptions::default(),
        top_k,
    )
    .await?
    .results)
}

/// Async search with upstream knowledge filters + options (upstream API).
///
/// Uses `ProjectSearchScope::all_projects()` — callers that need project
/// isolation should use [`search_with_all_options`] instead.
pub async fn search_with_options<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    query: &str,
    wing: Option<&str>,
    room: Option<&str>,
    options: SearchOptions,
    top_k: usize,
) -> Result<Vec<SearchResult>> {
    Ok(search_with_all_options_outcome(
        db,
        embedder,
        query,
        wing,
        room,
        &ProjectSearchScope::all_projects(),
        options,
        top_k,
    )
    .await?
    .results)
}

/// Async search with both project scope AND knowledge filter options (merged API).
#[allow(clippy::too_many_arguments)]
pub async fn search_with_all_options<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    query: &str,
    wing: Option<&str>,
    room: Option<&str>,
    scope: &ProjectSearchScope,
    options: SearchOptions,
    top_k: usize,
) -> Result<Vec<SearchResult>> {
    Ok(
        search_with_all_options_outcome(db, embedder, query, wing, room, scope, options, top_k)
            .await?
            .results,
    )
}

/// Async search that reports whether the response used hybrid or BM25-only
/// retrieval. Existing callers use [`search_with_all_options`] to preserve the
/// historical `Vec<SearchResult>` API.
#[allow(clippy::too_many_arguments)]
pub async fn search_with_all_options_outcome<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    query: &str,
    wing: Option<&str>,
    room: Option<&str>,
    scope: &ProjectSearchScope,
    options: SearchOptions,
    top_k: usize,
) -> Result<SearchOutcome> {
    if top_k == 0 {
        return Ok(SearchOutcome::hybrid(Vec::new()));
    }

    let route = resolve_route(db, query, wing, room)?;
    search_with_route_options_outcome(db, embedder, query, route, scope, options, top_k).await
}

pub async fn search_with_route_options_outcome<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    query: &str,
    route: RouteDecision,
    scope: &ProjectSearchScope,
    options: SearchOptions,
    top_k: usize,
) -> Result<SearchOutcome> {
    if top_k == 0 {
        return Ok(SearchOutcome::hybrid(Vec::new()));
    }

    let vector_search_circuit = VectorSearchCircuit::current();
    if vector_search_circuit.bm25_fallback_enabled && vector_search_circuit.open {
        let outcome = bm25_fallback_outcome(
            db,
            query,
            route,
            scope,
            options,
            top_k,
            bm25_fallback_warning_degraded(vector_search_circuit.failure_count),
        )?;
        return Ok(apply_optional_reranker_to_outcome(query, outcome).await);
    }

    let embeddings = match tokio::time::timeout(
        Duration::from_secs(vector_search_circuit.search_deadline_secs),
        embedder.embed(&[query]),
    )
    .await
    {
        Ok(Ok(embeddings)) => embeddings,
        Ok(Err(error)) if vector_search_circuit.bm25_fallback_enabled => {
            let outcome = bm25_fallback_outcome(
                db,
                query,
                route,
                scope,
                options,
                top_k,
                bm25_fallback_warning_embed_error(&crate::core::config::scrub_sensitive_text(
                    &error.to_string(),
                )),
            )?;
            return Ok(apply_optional_reranker_to_outcome(query, outcome).await);
        }
        Ok(Err(error)) => return Err(SearchError::EmbedQuery(error)),
        Err(_) if vector_search_circuit.bm25_fallback_enabled => {
            let outcome = bm25_fallback_outcome(
                db,
                query,
                route,
                scope,
                options,
                top_k,
                bm25_fallback_warning_timeout(vector_search_circuit.search_deadline_secs),
            )?;
            return Ok(apply_optional_reranker_to_outcome(query, outcome).await);
        }
        Err(_) => {
            return Err(SearchError::EmbedQueryTimeout {
                deadline_secs: vector_search_circuit.search_deadline_secs,
            });
        }
    };
    let Some(query_vector) = embeddings.into_iter().next() else {
        if vector_search_circuit.bm25_fallback_enabled {
            let outcome = bm25_fallback_outcome(
                db,
                query,
                route,
                scope,
                options,
                top_k,
                bm25_fallback_warning_missing_query_vector(),
            )?;
            return Ok(apply_optional_reranker_to_outcome(query, outcome).await);
        }
        return Err(SearchError::MissingQueryVector);
    };
    if let Some(current_dim) = current_vector_dim(db).map_err(SearchError::KeywordSearch)?
        && current_dim != query_vector.len()
    {
        if vector_search_circuit.bm25_fallback_enabled {
            let outcome = bm25_fallback_outcome(
                db,
                query,
                route,
                scope,
                options,
                top_k,
                bm25_fallback_warning_dimension_mismatch(query_vector.len(), current_dim),
            )?;
            return Ok(apply_optional_reranker_to_outcome(query, outcome).await);
        }
        return Err(SearchError::VectorDimensionMismatch {
            current_dim,
            new_dim: query_vector.len(),
        });
    }

    let outcome = SearchOutcome::hybrid(search_with_vector_and_scope_options(
        db,
        query,
        &query_vector,
        route,
        scope,
        options,
        top_k,
    )?);
    Ok(apply_optional_reranker_to_outcome(query, outcome).await)
}

fn bm25_fallback_outcome(
    db: &Database,
    query: &str,
    route: RouteDecision,
    scope: &ProjectSearchScope,
    options: SearchOptions,
    top_k: usize,
    warning: String,
) -> Result<SearchOutcome> {
    let results = search_bm25_only_with_options(db, query, route, scope, options, top_k)?;
    Ok(SearchOutcome::bm25_only(results, warning))
}

// ---------------------------------------------------------------------------
// Synchronous vector-entry points
// ---------------------------------------------------------------------------

/// Fork API: vector search with project scope, no knowledge filters.
pub fn search_with_vector(
    db: &Database,
    query: &str,
    query_vector: &[f32],
    route: RouteDecision,
    scope: &ProjectSearchScope,
    top_k: usize,
) -> Result<Vec<SearchResult>> {
    search_with_vector_and_scope_options(
        db,
        query,
        query_vector,
        route,
        scope,
        SearchOptions::default(),
        top_k,
    )
}

/// Upstream API: vector search with knowledge filter options, no project scope
/// (`all_projects` default).
///
/// Used by `context.rs` which does not carry a `ProjectSearchScope`.
pub fn search_with_vector_options(
    db: &Database,
    query: &str,
    query_vector: &[f32],
    route: RouteDecision,
    options: SearchOptions,
    top_k: usize,
) -> Result<Vec<SearchResult>> {
    search_with_vector_and_scope_options(
        db,
        query,
        query_vector,
        route,
        &ProjectSearchScope::all_projects(),
        options,
        top_k,
    )
}

/// Combined: vector search with BOTH project scope AND knowledge filters.
///
/// This is the single canonical implementation that all other entry points
/// delegate to.
pub fn search_with_vector_and_scope_options(
    db: &Database,
    query: &str,
    query_vector: &[f32],
    route: RouteDecision,
    scope: &ProjectSearchScope,
    options: SearchOptions,
    top_k: usize,
) -> Result<Vec<SearchResult>> {
    if top_k == 0 {
        return Ok(Vec::new());
    }

    let config = crate::core::config::ConfigHandle::current();
    let exclude_raw_turns = config.search.exclude_raw_turns && !options.include_raw_turns;
    let has_temporal_post_process = !options.include_expired
        || !matches!(
            config.search.decay.mode,
            crate::core::config::DecayMode::None
        );
    let has_filters = !options.filters.is_empty() || exclude_raw_turns || has_temporal_post_process;
    let candidate_top_k = if has_filters {
        top_k.saturating_mul(20).max(100)
    } else {
        top_k
    };

    // Hybrid search: vector + BM25, merged via RRF
    let vector_results = search_by_vector_with_filters(
        db,
        query_vector,
        route.clone(),
        scope,
        &options.filters,
        candidate_top_k,
    )?;

    let fts_ids = db
        .search_fts_filtered(
            query,
            FtsSearchScope {
                wing: route.wing.as_deref(),
                room: route.room.as_deref(),
                project_mode: scope.mode_param(),
                project_id: scope.project_id.as_deref(),
                filters: options.filters.as_fts_metadata_filters(),
            },
            candidate_top_k,
        )
        .map_err(SearchError::KeywordSearch)?;

    let mut results = if fts_ids.is_empty() {
        vector_results
    } else {
        rrf_merge(vector_results, &fts_ids, &route, scope, db, candidate_top_k)
    };
    retain_search_filters(&mut results, &options.filters);
    if exclude_raw_turns {
        results.retain(|result| {
            !crate::core::strata::is_excluded_raw_turn_result(result, &config.turns)
        });
    }

    // Inject tunnel hints: for each result, check if its room exists in other wings
    inject_tunnel_hints_and_results(db, &mut results, scope);
    retain_search_filters(&mut results, &options.filters);
    if exclude_raw_turns {
        results.retain(|result| {
            !crate::core::strata::is_excluded_raw_turn_result(result, &config.turns)
        });
    }

    let now_secs = current_unix_secs();
    let temporal_metadata = load_temporal_metadata(db, &results)?;
    if !options.include_expired {
        retain_currently_valid_results(&mut results, &temporal_metadata, now_secs);
    }

    // Read effective_importance from a single consistent snapshot before the
    // importance pipeline (boost -> decay -> rerank) so a concurrent
    // access-tracking update cannot tear the read across drawers and make the
    // rerank nondeterministic (GitHub #254).
    apply_consistent_effective_importance(db, &mut results);

    // Pattern boosting (P13): boost exemplar drawers of active patterns that
    // match the query vector. Fire-and-forget on error.
    apply_pattern_boost(db, query_vector, scope.project_id.as_deref(), &mut results);
    apply_temporal_decay(
        &mut results,
        &temporal_metadata,
        &config.search.decay,
        now_secs,
    );

    // Post-RRF secondary reranking by effective_importance (P13).
    // Does NOT alter the similarity field; purely reorders within the final set.
    rerank_by_effective_importance(&mut results);
    results.truncate(top_k);

    // Chunk neighbors hydration (upstream feature)
    if options.with_neighbors && top_k <= 10 {
        inject_chunk_neighbors(db, &mut results)?;
    }

    Ok(results)
}

#[derive(Debug, Clone)]
struct TemporalMetadata {
    added_at: String,
    valid_from: Option<String>,
    valid_until: Option<String>,
}

fn current_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn load_temporal_metadata(
    db: &Database,
    results: &[SearchResult],
) -> Result<std::collections::HashMap<String, TemporalMetadata>> {
    let ids = results
        .iter()
        .map(|result| result.drawer_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let placeholders = (1..=ids.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT id, added_at, valid_from, valid_until FROM drawers WHERE id IN ({placeholders})"
    );
    let mut statement = db
        .conn()
        .prepare(&sql)
        .map_err(SearchError::LoadTemporalMetadata)?;
    let rows = statement
        .query_map(params_from_iter(ids.iter().map(String::as_str)), |row| {
            Ok((
                row.get::<_, String>(0)?,
                TemporalMetadata {
                    added_at: row.get(1)?,
                    valid_from: row.get(2)?,
                    valid_until: row.get(3)?,
                },
            ))
        })
        .map_err(SearchError::LoadTemporalMetadata)?
        .collect::<std::result::Result<std::collections::HashMap<_, _>, _>>()
        .map_err(SearchError::LoadTemporalMetadata)?;

    Ok(rows)
}

fn retain_currently_valid_results(
    results: &mut Vec<SearchResult>,
    metadata: &std::collections::HashMap<String, TemporalMetadata>,
    now_secs: i64,
) {
    results.retain(|result| {
        metadata.get(&result.drawer_id).is_none_or(|temporal| {
            validity_window_contains_at(
                temporal.valid_from.as_deref(),
                temporal.valid_until.as_deref(),
                now_secs,
            )
        })
    });
}

/// Re-read `effective_importance` for the result set in a single consistent
/// SQLite snapshot and overwrite each result's value before the importance
/// rerank.
///
/// Why this exists: every search fires a fire-and-forget `dispatch_access_update`
/// that recomputes `effective_importance` for the hit drawers in one atomic
/// batch. The per-result `hydrate_result_metadata` path reads each drawer with a
/// *separate* `get_drawer` query, so a concurrent access-update commit landing
/// between two of those reads produces a TORN read: two drawers that share the
/// same base importance end up with inconsistent `effective_importance` (e.g.
/// `0.0` pre-update vs `0.2` post-update). `rerank_by_effective_importance` then
/// reorders them nondeterministically (GitHub #254). A single `IN (...)` query
/// reads all rows from one snapshot, so batched-together drawers are always
/// mutually consistent and the rerank stays a deterministic no-op for
/// equal-importance results.
///
/// Tunnel cross-project results are skipped: they carry a deliberately
/// penalty-scaled `effective_importance` that must not be overwritten by the raw
/// stored value. Fail-soft: on any read error the existing (possibly torn)
/// values are left in place rather than failing the search.
fn apply_consistent_effective_importance(db: &Database, results: &mut [SearchResult]) {
    let ids = results
        .iter()
        .filter(|result| result.source != SearchResultSource::TunnelCrossProject)
        .map(|result| result.drawer_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    if ids.is_empty() {
        return;
    }

    let placeholders = (1..=ids.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    // Mirror `get_drawer`'s NULLIF(...,0.0) guard so the snapshot value matches
    // what the per-result hydrate would have produced absent a concurrent write.
    // A persisted 0.0 is the "not computed" sentinel and must fall back to base
    // importance, else the drawer sinks in the importance rerank (GitHub #309).
    // The fallback carries the persisted stale penalty (default 1.0) so a legacy
    // 0.0 row that was fact-check down-ranked ranks at importance*penalty.
    let sql = format!(
        "SELECT id, COALESCE(NULLIF(effective_importance, 0.0), CAST(COALESCE(importance, 0) AS REAL) * COALESCE(stale_penalty_applied, 1.0)) \
         FROM drawers WHERE id IN ({placeholders})"
    );
    let snapshot: std::collections::HashMap<String, f64> = match db.conn().prepare(&sql) {
        Ok(mut statement) => {
            let rows = statement
                .query_map(params_from_iter(ids.iter().map(String::as_str)), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
                });
            match rows.and_then(|mapped| mapped.collect::<std::result::Result<_, _>>()) {
                Ok(map) => map,
                Err(err) => {
                    tracing::warn!(error = %err, "consistent effective_importance read failed");
                    return;
                }
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "consistent effective_importance read failed");
            return;
        }
    };

    for result in results.iter_mut() {
        if result.source == SearchResultSource::TunnelCrossProject {
            continue;
        }
        if let Some(&effective_importance) = snapshot.get(&result.drawer_id) {
            result.effective_importance = effective_importance;
        }
    }
}

fn apply_temporal_decay(
    results: &mut [SearchResult],
    metadata: &std::collections::HashMap<String, TemporalMetadata>,
    config: &crate::core::config::DecayConfig,
    now_secs: i64,
) {
    if matches!(config.mode, crate::core::config::DecayMode::None) {
        return;
    }

    for result in results.iter_mut() {
        let factor = metadata
            .get(&result.drawer_id)
            .map(|temporal| search_decay_factor_at(&temporal.added_at, config, now_secs))
            .unwrap_or(1.0);
        result.similarity *= factor as f32;
        result.effective_importance *= factor;
    }
    crate::algo::ranking::sort_by_similarity_desc_then_id(results);
}

/// Apply active-pattern score boost to matching exemplar drawers.
///
/// For each active pattern whose signature cosine similarity to `query_vector` exceeds
/// `surfacing_threshold`, the matching exemplar drawers in `results` receive a
/// `+pattern_boost` to their `effective_importance`. Non-matching drawers are unchanged.
/// Failures are silently logged (never propagate to the caller).
fn apply_pattern_boost(
    db: &Database,
    query_vector: &[f32],
    project_id: Option<&str>,
    results: &mut [SearchResult],
) {
    if results.is_empty() || query_vector.is_empty() {
        return;
    }
    let config = crate::core::config::ConfigHandle::current();
    if !config.patterns.enabled {
        return;
    }
    let surfacing_threshold = config.patterns.surfacing_threshold as f32;
    let pattern_boost = config.patterns.pattern_boost;

    let model_id = config.embed.model.clone().unwrap_or_else(|| {
        if config.embed.backend == "model2vec" {
            "model2vec/potion-multilingual-128M".to_string()
        } else {
            String::new()
        }
    });

    let patterns = match crate::core::patterns::load_active_patterns_for_search(
        db.conn(),
        &model_id,
        project_id,
    ) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = %err, "failed to load active patterns for boost");
            return;
        }
    };

    for pattern in &patterns {
        if pattern.signature.is_empty() {
            continue;
        }
        let sim = crate::core::patterns::cosine_similarity(query_vector, &pattern.signature);
        if sim < surfacing_threshold {
            continue;
        }
        // Apply boost to all results whose drawer_id is in this pattern's exemplar_ids.
        for result in results.iter_mut() {
            if pattern.exemplar_ids.contains(&result.drawer_id) {
                result.effective_importance += pattern_boost;
                result.matched_pattern_id = Some(pattern.pattern_id.clone());
            }
        }
    }
}

/// Post-RRF secondary sort by `effective_importance` (descending).
///
/// Uses a stable sort so ties preserve the RRF score ordering.
/// Per spec: must not modify the `similarity` field.
fn rerank_by_effective_importance(results: &mut [SearchResult]) {
    crate::algo::ranking::rerank_by_effective_importance(results);
}

/// Dispatch an async task to update access tracking fields for the given drawer IDs.
///
/// This must not block the search response path. The update runs on the current
/// tokio runtime as a detached `spawn_blocking` task.
pub fn dispatch_access_update(db_path: std::path::PathBuf, drawer_ids: Vec<String>) {
    if drawer_ids.is_empty() {
        return;
    }
    let config = crate::core::config::ConfigHandle::current();
    if !config.search.record_access {
        crate::observability::record_access_writeback_skipped();
        return;
    }
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let decay_rate = config.importance.decay_rate;
    let floor = config.importance.floor;
    let boost_cap = config.importance.boost_cap;
    crate::observability::record_access_writeback_scheduled();
    tokio::task::spawn_blocking(move || match crate::core::db::Database::open(&db_path) {
        Ok(db) => {
            if let Err(err) =
                db.update_access_fields_batch(&drawer_ids, now_ms, decay_rate, floor, boost_cap)
            {
                crate::observability::record_access_writeback_failed();
                tracing::warn!(error = %err, "access field update failed");
            }
        }
        Err(err) => {
            crate::observability::record_access_writeback_failed();
            tracing::warn!(error = %err, "failed to open db for access update");
        }
    });
}

/// BM25-only search path (fork API, used when embedder is unavailable).
pub fn search_bm25_only(
    db: &Database,
    query: &str,
    route: RouteDecision,
    scope: &ProjectSearchScope,
    top_k: usize,
) -> Result<Vec<SearchResult>> {
    search_bm25_only_with_options(db, query, route, scope, SearchOptions::default(), top_k)
}

/// BM25-only search path with raw-turn filtering options.
pub fn search_bm25_only_with_options(
    db: &Database,
    query: &str,
    route: RouteDecision,
    scope: &ProjectSearchScope,
    options: SearchOptions,
    top_k: usize,
) -> Result<Vec<SearchResult>> {
    if top_k == 0 {
        return Ok(Vec::new());
    }

    let config = crate::core::config::ConfigHandle::current();
    let exclude_raw_turns = config.search.exclude_raw_turns && !options.include_raw_turns;
    let has_temporal_post_process = !options.include_expired
        || !matches!(
            config.search.decay.mode,
            crate::core::config::DecayMode::None
        );
    let candidate_top_k = if exclude_raw_turns || has_temporal_post_process {
        top_k.saturating_mul(20).max(100)
    } else {
        top_k
    };
    let fts_ids = db
        .search_fts_filtered(
            query,
            FtsSearchScope {
                wing: route.wing.as_deref(),
                room: route.room.as_deref(),
                project_mode: scope.mode_param(),
                project_id: scope.project_id.as_deref(),
                filters: options.filters.as_fts_metadata_filters(),
            },
            candidate_top_k,
        )
        .map_err(SearchError::KeywordSearch)?;

    let mut results = rrf_merge(Vec::new(), &fts_ids, &route, scope, db, candidate_top_k);
    retain_search_filters(&mut results, &options.filters);
    if exclude_raw_turns {
        results.retain(|result| {
            !crate::core::strata::is_excluded_raw_turn_result(result, &config.turns)
        });
    }
    inject_tunnel_hints_and_results(db, &mut results, scope);
    retain_search_filters(&mut results, &options.filters);
    if exclude_raw_turns {
        results.retain(|result| {
            !crate::core::strata::is_excluded_raw_turn_result(result, &config.turns)
        });
    }
    let now_secs = current_unix_secs();
    let temporal_metadata = load_temporal_metadata(db, &results)?;
    if !options.include_expired {
        retain_currently_valid_results(&mut results, &temporal_metadata, now_secs);
    }
    // Consistent-snapshot effective_importance read before the importance
    // pipeline, mirroring the hybrid path — see GitHub #254.
    apply_consistent_effective_importance(db, &mut results);
    apply_temporal_decay(
        &mut results,
        &temporal_metadata,
        &config.search.decay,
        now_secs,
    );
    rerank_by_effective_importance(&mut results);
    results.truncate(top_k);
    if options.with_neighbors && top_k <= 10 {
        inject_chunk_neighbors(db, &mut results)?;
    }
    Ok(results)
}

impl SearchFilters {
    fn is_empty(&self) -> bool {
        self.memory_kind.is_none()
            && self.domain.is_none()
            && self.field.is_none()
            && self.tier.is_none()
            && self.status.is_none()
            && self.anchor_kind.is_none()
    }

    fn as_fts_metadata_filters(&self) -> FtsMetadataFilters<'_> {
        FtsMetadataFilters {
            memory_kind: self.memory_kind.as_deref(),
            domain: self.domain.as_deref(),
            field: self.field.as_deref(),
            tier: self.tier.as_deref(),
            status: self.status.as_deref(),
            anchor_kind: self.anchor_kind.as_deref(),
        }
    }
}

fn matches_filters(result: &SearchResult, filters: &SearchFilters) -> bool {
    filters
        .memory_kind
        .as_deref()
        .is_none_or(|value| value == memory_kind_slug(&result.memory_kind))
        && filters
            .domain
            .as_deref()
            .is_none_or(|value| value == domain_slug(&result.domain))
        && filters
            .field
            .as_deref()
            .is_none_or(|value| value == result.field)
        && filters.tier.as_deref().is_none_or(|value| {
            result
                .tier
                .as_ref()
                .is_some_and(|tier| value == tier_slug(tier))
        })
        && filters.status.as_deref().is_none_or(|value| {
            result
                .status
                .as_ref()
                .is_some_and(|status| value == status_slug(status))
        })
        && filters
            .anchor_kind
            .as_deref()
            .is_none_or(|value| value == anchor_kind_slug(&result.anchor_kind))
}

fn retain_search_filters(results: &mut Vec<SearchResult>, filters: &SearchFilters) {
    if !filters.is_empty() {
        results.retain(|result| matches_filters(result, filters));
    }
}

// ---------------------------------------------------------------------------
// Chunk neighbors (upstream)
// ---------------------------------------------------------------------------

fn inject_chunk_neighbors(db: &Database, results: &mut [SearchResult]) -> Result<()> {
    for result in results {
        let Some(chunk_index) = result.chunk_index else {
            continue;
        };
        let neighbors = db
            .neighbor_chunks(
                &result.source_file,
                &result.wing,
                result.room.as_deref(),
                chunk_index,
            )
            .map_err(SearchError::LoadNeighbors)?;
        if neighbors.prev.is_some() || neighbors.next.is_some() {
            result.neighbors = Some(neighbors);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tunnel hints (fork — project-aware fanout + display cap)
// ---------------------------------------------------------------------------

/// For each search result, check if its room appears in other wings (tunnel).
/// If so, add the other wing names as tunnel_hints and append any explicit
/// cross-project tunnel targets without applying the project filter.
///
/// Reads `[search].tunnel_fanout_cap`, `[search].tunnel_hints_display_cap`, and
/// `[search].tunnel_penalty` from the hot-reload config snapshot.
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
        search_cfg.tunnel_penalty,
    );
}

/// Tunnel-hint injection with explicit caps — factored out for unit tests so callers
/// can pin caps without touching the global hot-reload state.
///
/// `fanout_cap` bounds the number of injected cross-project rows per source result.
/// `hints_display_cap` bounds `tunnel_hints` string entries per result; excess wings
/// are replaced by a single `"… +N more"` sentinel as the last element.
/// `tunnel_penalty` (0.0..=1.0) multiplies each tunnel-resolved result's
/// `similarity` and `effective_importance` so cross-project rows rank below direct
/// in-project matches at equal raw scores. `1.0` disables the penalty.
pub(crate) fn inject_tunnel_hints_with_cap(
    db: &Database,
    results: &mut Vec<SearchResult>,
    scope: &ProjectSearchScope,
    fanout_cap: usize,
    hints_display_cap: usize,
    tunnel_penalty: f32,
) {
    let tunnels = match db.find_tunnels() {
        Ok(t) => t,
        Err(_) => return,
    };
    if tunnels.is_empty() {
        return;
    }

    // Build room -> other-wings map
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
                let mut combined_hints: Vec<String> =
                    other_wings.iter().map(|w| (*w).clone()).collect();
                if let Ok(explicit_hints) = db.explicit_tunnel_hints(&result.wing, Some(room)) {
                    for hint in explicit_hints {
                        if !combined_hints.contains(&hint) {
                            combined_hints.push(hint);
                        }
                    }
                }
                let total_hints = combined_hints.len();
                let mut hints = combined_hints
                    .into_iter()
                    .take(hints_display_cap)
                    .collect::<Vec<_>>();
                if total_hints > hints_display_cap {
                    hints.push(format!("… +{} more", total_hints - hints_display_cap));
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
                        let mut tunnel_result = result_from_drawer(
                            drawer,
                            SearchResultSource::TunnelCrossProject,
                            result.similarity,
                            result.route.clone(),
                        );
                        tunnel_result.similarity *= tunnel_penalty;
                        tunnel_result.effective_importance *= tunnel_penalty as f64;
                        tunnel_results.push(tunnel_result);
                        added_from_this_result += 1;
                    }
                }
            }
        }
    }
    results.extend(tunnel_results);
}

fn result_from_drawer(
    drawer: Drawer,
    source: SearchResultSource,
    similarity: f32,
    route: RouteDecision,
) -> SearchResult {
    SearchResult {
        drawer_id: drawer.id.clone(),
        content: drawer.content,
        wing: drawer.wing,
        room: drawer.room,
        source_file: source_file_or_synthetic(&drawer.id, drawer.source_file.as_deref()),
        source,
        source_type: drawer.source_type,
        confidence: drawer.confidence,
        memory_kind: drawer.memory_kind,
        domain: drawer.domain,
        field: drawer.field,
        statement: drawer.statement,
        tier: drawer.tier,
        status: drawer.status,
        anchor_kind: drawer.anchor_kind,
        anchor_id: drawer.anchor_id,
        parent_anchor_id: drawer.parent_anchor_id,
        is_pinned: drawer.is_pinned,
        importance: drawer.importance,
        similarity,
        route,
        chunk_index: drawer.chunk_index,
        neighbors: None,
        tunnel_hints: vec![],
        effective_importance: drawer.effective_importance,
        matched_pattern_id: None,
    }
}

fn hydrate_result_metadata(db: &Database, result: SearchResult) -> SearchResult {
    match db.get_drawer(&result.drawer_id) {
        Ok(Some(drawer)) => {
            result_from_drawer(drawer, result.source, result.similarity, result.route)
        }
        _ => result,
    }
}

// ---------------------------------------------------------------------------
// RRF merge
// ---------------------------------------------------------------------------

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

    let vector_ids = vector_results
        .iter()
        .map(|result| result.drawer_id.as_str())
        .collect::<Vec<_>>();
    let fts_ranked_ids = fts_ids
        .iter()
        .map(|(id, _bm25_score)| id.as_str())
        .collect::<Vec<_>>();
    let fused = ReciprocalRankFusion::default().fuse([vector_ids, fts_ranked_ids]);
    let mut result_map: HashMap<String, SearchResult> = HashMap::new();

    for result in vector_results {
        result_map.insert(result.drawer_id.clone(), result);
    }

    for (id, _bm25_score) in fts_ids {
        if !result_map.contains_key(id) {
            if let Ok(Some(drawer)) = db.get_drawer(id) {
                let source = scope.classify_row(db.drawer_project_id(id).ok().flatten().as_deref());
                result_map.insert(
                    id.clone(),
                    result_from_drawer(drawer, source, 0.0, route.clone()),
                );
            }
        }
    }

    let mut merged: Vec<SearchResult> = fused
        .into_iter()
        .filter_map(|rank| {
            let mut result = result_map.remove(&rank.id)?;
            result.similarity = rank.score as f32;
            Some(result)
        })
        .collect();
    merged.truncate(top_k);
    merged
}

// ---------------------------------------------------------------------------
// KNN helpers (fork)
// ---------------------------------------------------------------------------

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

/// Compute the KNN `k` parameter for sqlite-vec, clamped to its hardcoded
/// limit of 4096. Uses `top_k * 50` as the recall multiplier (allowing
/// post-filter shrinkage from wing/room/project predicates), floored at
/// 100 to avoid degenerate single-digit recall on tiny `top_k` values.
///
/// When the database grows beyond 4096 drawers the KNN result is an
/// *approximate* subset -- callers that need exact recall on a small
/// candidate set should use `search_by_vector_scoped_exact` instead.
pub fn compute_knn_k(top_k: usize) -> i64 {
    let raw = top_k.saturating_mul(50);
    let raw_i64 = i64::try_from(raw).unwrap_or(i64::MAX);
    raw_i64.clamp(100, EXACT_VECTOR_CANDIDATE_LIMIT)
}

// ---------------------------------------------------------------------------
// Vector search (fork path with project scope)
// ---------------------------------------------------------------------------

pub fn search_by_vector(
    db: &Database,
    query_vector: &[f32],
    route: RouteDecision,
    scope: &ProjectSearchScope,
    top_k: usize,
) -> Result<Vec<SearchResult>> {
    search_by_vector_with_filters(
        db,
        query_vector,
        route,
        scope,
        &SearchFilters::default(),
        top_k,
    )
}

fn search_by_vector_with_filters(
    db: &Database,
    query_vector: &[f32],
    route: RouteDecision,
    scope: &ProjectSearchScope,
    filters: &SearchFilters,
    top_k: usize,
) -> Result<Vec<SearchResult>> {
    if top_k == 0 {
        return Ok(Vec::new());
    }

    let applied_wing = route.wing.as_deref();
    let applied_room = route.room.as_deref();

    let count_sql = format!(
        "SELECT COUNT(*) FROM drawers d {}",
        build_retrieval_filter_clause(
            "d",
            RetrievalFilterParamIndexes {
                wing: 1,
                room: 2,
                project_mode: 3,
                project_id: 4,
                memory_kind: 5,
                domain: 6,
                field: 7,
                tier: 8,
                status: 9,
                anchor_kind: 10,
            },
        )
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
                filters.memory_kind.as_deref(),
                filters.domain.as_deref(),
                filters.field.as_deref(),
                filters.tier.as_deref(),
                filters.status.as_deref(),
                filters.anchor_kind.as_deref(),
            ),
            |row| row.get(0),
        )
        .map_err(SearchError::CountCandidateDrawers)?;
    if candidate_count == 0 {
        return Ok(Vec::new());
    }

    // When the *bounded* candidate set fits within the sqlite-vec KNN limit,
    // use the exact in-memory path regardless of scope mode. This is a
    // deliberate recall-preserving choice, not a perf compromise:
    //
    //   - The exact path applies the FULL filter (wing/room/project) *before*
    //     scoring, so it returns the true top-k over exactly the filtered set.
    //   - The KNN path (`build_vector_search_sql`) pushes only `project_id`
    //     into the vector CTE; `wing`/`room` are applied as a POST-filter on
    //     the `k`-nearest window (filter.rs:47). A wing/room-scoped query whose
    //     matching drawers fall outside the global k-nearest window would then
    //     silently return fewer than top-k rows -- an approximate-recall loss.
    //
    // After the decorate-sort-undecorate fix in `search_by_vector_scoped_exact`
    // the exact path costs one cosine evaluation per candidate (O(n)), so
    // scoring up to 4096 candidates is milliseconds -- there is no longer a
    // latency reason to prefer the approximate KNN path for that bounded case.
    //
    // Typed metadata filters are counted above and then enforced again after
    // RRF. They must not bypass this guard: the exact path materializes every
    // matching embedding, so a broad filter such as `memory_kind=evidence`
    // could otherwise load an unbounded number of vector blobs from an MCP call.
    if candidate_count <= EXACT_VECTOR_CANDIDATE_LIMIT {
        return search_by_vector_scoped_exact(
            db,
            ExactVectorSearchRequest {
                query_vector,
                route: route.clone(),
                applied_wing,
                applied_room,
                top_k,
                scope,
                filters,
            },
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
                    source_type: SourceType::AgentInference,
                    confidence: crate::core::types::default_confidence(SourceType::AgentInference),
                    // Knowledge fields are not available from the vector-only
                    // SQL path; use defaults.  Callers that need them should
                    // hydrate via the drawer record.
                    memory_kind: MemoryKind::Evidence,
                    domain: MemoryDomain::Project,
                    field: String::new(),
                    statement: None,
                    tier: None,
                    status: None,
                    anchor_kind: AnchorKind::Global,
                    anchor_id: String::new(),
                    parent_anchor_id: None,
                    is_pinned: false,
                    importance: 0,
                    similarity: (1.0_f64 - distance) as f32,
                    route: route.clone(),
                    chunk_index: None,
                    neighbors: None,
                    tunnel_hints: vec![],
                    // Hydrated by `hydrate_result_metadata` below.
                    effective_importance: 0.0,
                    matched_pattern_id: None,
                })
            },
        )
        .map_err(SearchError::ExecuteSearch)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(SearchError::CollectSearchRows)?;

    Ok(results
        .into_iter()
        .map(|result| hydrate_result_metadata(db, result))
        .collect())
}

struct ExactVectorSearchRequest<'a> {
    query_vector: &'a [f32],
    route: RouteDecision,
    applied_wing: Option<&'a str>,
    applied_room: Option<&'a str>,
    top_k: usize,
    scope: &'a ProjectSearchScope,
    filters: &'a SearchFilters,
}

fn search_by_vector_scoped_exact(
    db: &Database,
    request: ExactVectorSearchRequest<'_>,
) -> Result<Vec<SearchResult>> {
    let ExactVectorSearchRequest {
        query_vector,
        route,
        applied_wing,
        applied_room,
        top_k,
        scope,
        filters,
    } = request;
    let top_k = i64::try_from(top_k).map_err(|_| SearchError::InvalidTopK)?;
    // Use the full filter clause so all scope modes (all / project /
    // project_plus_global / null_only) work correctly through the exact path.
    let filter = build_retrieval_filter_clause(
        "d",
        RetrievalFilterParamIndexes {
            wing: 1,
            room: 2,
            project_mode: 3,
            project_id: 4,
            memory_kind: 5,
            domain: 6,
            field: 7,
            tier: 8,
            status: 9,
            anchor_kind: 10,
        },
    );
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
    let rows = statement
        .query_map(
            (
                applied_wing,
                applied_room,
                scope.mode_param(),
                scope.project_id.as_deref(),
                filters.memory_kind.as_deref(),
                filters.domain.as_deref(),
                filters.field.as_deref(),
                filters.tier.as_deref(),
                filters.status.as_deref(),
                filters.anchor_kind.as_deref(),
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

    // Decorate-sort-undecorate: score every candidate exactly once (O(n) cosine
    // evaluations) instead of recomputing the distance inside the O(n log n)
    // sort comparator (which previously did ~2 decodes per comparison plus a
    // third decode in the result-building map). The query L2 norm is computed a
    // single time and shared across all candidates.
    let query_norm = l2_norm(query_vector);
    let ranked = rank_exact_candidates(rows, top_k as usize, |drawer_id, embedding| {
        cosine_distance_from_blob_with_norm(drawer_id, embedding, query_vector, query_norm)
    });

    let results = ranked
        .into_iter()
        .map(
            |(
                distance,
                (drawer_id, content, wing, room, source_file, row_project_id, _embedding),
            )| {
                // Reuse the distance computed during ranking; never re-decode the
                // blob. Invalid-blob candidates carry an `Err` here and propagate
                // exactly as the legacy path did when one entered the top-k.
                let distance = distance?;
                Ok(SearchResult {
                    drawer_id,
                    content,
                    wing,
                    room,
                    source_file,
                    source: scope.classify_row(row_project_id.as_deref()),
                    source_type: SourceType::AgentInference,
                    confidence: crate::core::types::default_confidence(SourceType::AgentInference),
                    memory_kind: MemoryKind::Evidence,
                    domain: MemoryDomain::Project,
                    field: String::new(),
                    statement: None,
                    tier: None,
                    status: None,
                    anchor_kind: AnchorKind::Global,
                    anchor_id: String::new(),
                    parent_anchor_id: None,
                    is_pinned: false,
                    importance: 0,
                    similarity: (1.0_f64 - distance) as f32,
                    route: route.clone(),
                    chunk_index: None,
                    neighbors: None,
                    tunnel_hints: vec![],
                    // Hydrated by `hydrate_result_metadata` below.
                    effective_importance: 0.0,
                    matched_pattern_id: None,
                })
            },
        )
        .collect::<Result<Vec<_>>>()?;
    Ok(results
        .into_iter()
        .map(|result| hydrate_result_metadata(db, result))
        .collect())
}

/// Candidate row pulled from the exact (brute-force) vector path: the joined
/// drawer columns plus the raw little-endian `f32` embedding blob, before any
/// scoring. Carried verbatim through ranking so the result-building step never
/// needs to re-query.
type ExactCandidate = (
    String,         // drawer_id
    String,         // content
    String,         // wing
    Option<String>, // room
    String,         // source_file (already synthetic-resolved)
    Option<String>, // project_id
    Vec<u8>,        // embedding blob
);

/// Rank exact-path candidates by ascending cosine distance using
/// decorate-sort-undecorate, then truncate to `top_k`.
///
/// Each candidate is scored EXACTLY ONCE -- `distance_fn` is invoked
/// `rows.len()` times total (O(n)), and the cached distance is what the sort
/// comparator reads. This replaces the previous design that recomputed the
/// cosine distance inside the O(n log n) comparator (~2 decodes per comparison)
/// and a third time while building results.
///
/// Ordering matches the legacy comparator byte-for-byte: valid distances sort
/// ascending (NaN compared as `Equal`, so equal distances stay in stable input
/// order); candidates whose distance errors (invalid/corrupt blob) sort last;
/// and two errored candidates tie-break by `drawer_id`. The cached
/// `Result<f64>` distance is returned alongside each surviving row so the
/// caller reuses it (and propagates an `Err` if one lands inside the top-k,
/// exactly as before) without decoding the blob again.
fn rank_exact_candidates<F>(
    rows: Vec<ExactCandidate>,
    top_k: usize,
    mut distance_fn: F,
) -> Vec<(Result<f64>, ExactCandidate)>
where
    F: FnMut(&str, &[u8]) -> Result<f64>,
{
    let mut scored: Vec<(Result<f64>, ExactCandidate)> = rows
        .into_iter()
        .map(|row| {
            let distance = distance_fn(&row.0, &row.6);
            (distance, row)
        })
        .collect();

    scored.sort_by(|a, b| {
        let (a_dist, a_row) = (&a.0, &a.1);
        let (b_dist, b_row) = (&b.0, &b.1);
        match (a_dist, b_dist) {
            (Ok(left), Ok(right)) => left
                .partial_cmp(right)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Tie-break equal cosine distances by drawer_id so the exact
                // vector ranking is reproducible regardless of SQL row order.
                .then_with(|| a_row.0.cmp(&b_row.0)),
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            // Tie-break two invalid-blob candidates by drawer_id, matching the
            // legacy comparator's `a.0.cmp(&b.0)`.
            (Err(_), Err(_)) => a_row.0.cmp(&b_row.0),
        }
    });

    scored.truncate(top_k);
    scored
}

/// L2 (Euclidean) norm of a vector, computed in `f64` to match the precision
/// used by `cosine_distance_from_blob_with_norm`.
fn l2_norm(vector: &[f32]) -> f64 {
    vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt()
}

/// Cosine distance (`1 - cosine_similarity`, clamped to `[0, 2]`) between a
/// stored embedding blob and the query vector, given the query's precomputed
/// L2 norm. Decodes the little-endian `f32` blob exactly once. Hoisting the
/// query norm out of this function lets the exact path compute it a single time
/// for the whole candidate set instead of once per scored row.
fn cosine_distance_from_blob_with_norm(
    drawer_id: &str,
    embedding_blob: &[u8],
    query_vector: &[f32],
    query_norm: f64,
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
    let cosine_similarity = if left_norm == 0.0 || query_norm == 0.0 {
        0.0
    } else {
        dot / (left_norm * query_norm)
    };
    Ok((1.0 - cosine_similarity).clamp(0.0, 2.0))
}

// ---------------------------------------------------------------------------
// Route resolution
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Upstream knowledge-enum helpers
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn invalid_enum_value(kind: &'static str, value: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid {kind}: {value}"),
        )),
    )
}

#[allow(dead_code)]
fn memory_kind_from_str(value: &str) -> rusqlite::Result<MemoryKind> {
    value
        .parse()
        .map_err(|_| invalid_enum_value("memory_kind", value.to_string()))
}

fn memory_kind_slug(value: &MemoryKind) -> &'static str {
    value.as_str()
}

#[allow(dead_code)]
fn memory_domain_from_str(value: &str) -> rusqlite::Result<MemoryDomain> {
    match value {
        "project" => Ok(MemoryDomain::Project),
        "user" => Ok(MemoryDomain::User),
        "agent" => Ok(MemoryDomain::Agent),
        "skill" => Ok(MemoryDomain::Skill),
        "global" => Ok(MemoryDomain::Global),
        _ => Err(invalid_enum_value("domain", value.to_string())),
    }
}

fn domain_slug(value: &MemoryDomain) -> &'static str {
    match value {
        MemoryDomain::Project => "project",
        MemoryDomain::User => "user",
        MemoryDomain::Agent => "agent",
        MemoryDomain::Skill => "skill",
        MemoryDomain::Global => "global",
    }
}

#[allow(dead_code)]
fn knowledge_tier_from_str(value: &str) -> rusqlite::Result<KnowledgeTier> {
    match value {
        "qi" => Ok(KnowledgeTier::Qi),
        "shu" => Ok(KnowledgeTier::Shu),
        "dao_ren" => Ok(KnowledgeTier::DaoRen),
        "dao_tian" => Ok(KnowledgeTier::DaoTian),
        _ => Err(invalid_enum_value("tier", value.to_string())),
    }
}

fn tier_slug(value: &KnowledgeTier) -> &'static str {
    match value {
        KnowledgeTier::Qi => "qi",
        KnowledgeTier::Shu => "shu",
        KnowledgeTier::DaoRen => "dao_ren",
        KnowledgeTier::DaoTian => "dao_tian",
    }
}

#[allow(dead_code)]
fn knowledge_status_from_str(value: &str) -> rusqlite::Result<KnowledgeStatus> {
    match value {
        "candidate" => Ok(KnowledgeStatus::Candidate),
        "active" => Ok(KnowledgeStatus::Active),
        "superseded" => Ok(KnowledgeStatus::Superseded),
        "pending_review" => Ok(KnowledgeStatus::PendingReview),
        "promoted" => Ok(KnowledgeStatus::Promoted),
        "canonical" => Ok(KnowledgeStatus::Canonical),
        "demoted" => Ok(KnowledgeStatus::Demoted),
        "retired" => Ok(KnowledgeStatus::Retired),
        _ => Err(invalid_enum_value("status", value.to_string())),
    }
}

fn status_slug(value: &KnowledgeStatus) -> &'static str {
    match value {
        KnowledgeStatus::Candidate => "candidate",
        KnowledgeStatus::Active => "active",
        KnowledgeStatus::Superseded => "superseded",
        KnowledgeStatus::PendingReview => "pending_review",
        KnowledgeStatus::Promoted => "promoted",
        KnowledgeStatus::Canonical => "canonical",
        KnowledgeStatus::Demoted => "demoted",
        KnowledgeStatus::Retired => "retired",
    }
}

#[allow(dead_code)]
fn anchor_kind_from_str(value: &str) -> rusqlite::Result<AnchorKind> {
    match value {
        "global" => Ok(AnchorKind::Global),
        "repo" => Ok(AnchorKind::Repo),
        "worktree" => Ok(AnchorKind::Worktree),
        _ => Err(invalid_enum_value("anchor_kind", value.to_string())),
    }
}

fn anchor_kind_slug(value: &AnchorKind) -> &'static str {
    match value {
        AnchorKind::Global => "global",
        AnchorKind::Repo => "repo",
        AnchorKind::Worktree => "worktree",
    }
}

/// Build an FTS5 MATCH query from a user search string.
/// Each whitespace-separated term is quoted and joined with AND.
#[allow(dead_code)]
fn build_fts_match_query(query: &str) -> Option<String> {
    let terms = query
        .split_whitespace()
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();

    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" AND "))
    }
}

// ---------------------------------------------------------------------------
// Tests (fork)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::{Config, ConfigHandle, SearchConfig};
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
            source_type: SourceType::AgentInference,
            added_at: "1700000000".to_string(),
            chunk_index: None,
            importance: 0,
            ..Drawer::default()
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
            source_type: drawer.source_type,
            confidence: drawer.confidence,
            memory_kind: MemoryKind::Evidence,
            domain: MemoryDomain::Project,
            field: String::new(),
            statement: None,
            tier: None,
            status: None,
            anchor_kind: AnchorKind::Global,
            anchor_id: String::new(),
            parent_anchor_id: None,
            is_pinned: false,
            importance: drawer.importance,
            similarity: 0.9,
            route: RouteDecision {
                wing: None,
                room: None,
                confidence: 0.0,
                reason: "test".to_string(),
            },
            chunk_index: None,
            neighbors: None,
            tunnel_hints: vec![],
            effective_importance: 0.0,
            matched_pattern_id: None,
        }
    }

    fn route() -> RouteDecision {
        RouteDecision {
            wing: None,
            room: None,
            confidence: 0.0,
            reason: "test".to_string(),
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

    fn access_count(db_path: &std::path::Path, drawer_id: &str) -> i64 {
        let db = Database::open(db_path).expect("open db");
        db.conn()
            .query_row(
                "SELECT COALESCE(access_count, 0) FROM drawers WHERE id = ?1",
                [drawer_id],
                |row| row.get(0),
            )
            .expect("read access count")
    }

    async fn configure_record_access(
        dir: &std::path::Path,
        db_path: &std::path::Path,
        enabled: bool,
    ) {
        let config_path = dir.join("config.toml");
        let config = Config {
            db_path: db_path.to_string_lossy().into_owned(),
            search: SearchConfig {
                record_access: enabled,
                ..SearchConfig::default()
            },
            ..Config::default()
        };
        config.save_to(&config_path).expect("save config");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        ConfigHandle::harness_reload_from_path(&config_path);
        crate::observability::reset_resource_counters_for_tests();
    }

    struct SlowSearchEmbedder;

    #[async_trait::async_trait]
    impl crate::embed::Embedder for SlowSearchEmbedder {
        async fn embed(&self, texts: &[&str]) -> crate::embed::Result<Vec<Vec<f32>>> {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            Ok(texts.iter().map(|_| vec![0.25; 4]).collect())
        }

        fn dimensions(&self) -> usize {
            4
        }

        fn name(&self) -> &str {
            "slow-search-test-embedder"
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn search_query_embedding_deadline_falls_back_to_bm25() {
        let lock = crate::core::config::global_config_test_lock();
        let _guard = lock.lock().await;
        let tmp = TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let config_path = tmp.path().join("config.toml");
        let mut config = Config {
            db_path: db_path.to_string_lossy().into_owned(),
            ..Config::default()
        };
        config.search.bm25_fallback = true;
        config.embed.retry.search_deadline_secs = 5;
        config.save_to(&config_path).expect("save config");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        ConfigHandle::harness_reload_from_path(&config_path);
        crate::embed::global_embed_status().reset_for_tests();

        let db = Database::open(&db_path).expect("db");
        let mut drawer = make_drawer("bm25-timeout-hit", "alpha", "decision");
        drawer.content = "fallback keyword memory".to_string();
        db.insert_drawer_with_project(&drawer, Some("proj-a"))
            .expect("insert drawer");

        let outcome = search_with_route_options_outcome(
            &db,
            &SlowSearchEmbedder,
            "fallback keyword",
            route(),
            &ProjectSearchScope::all_projects(),
            SearchOptions::default(),
            3,
        )
        .await
        .expect("search should fall back to BM25");

        assert_eq!(outcome.search_mode, SearchMode::Bm25Only);
        assert_eq!(outcome.results[0].drawer_id, "bm25-timeout-hit");
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("deadline exceeded after 5s")),
            "{outcome:#?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_access_update_skips_db_write_by_default() {
        let lock = crate::core::config::global_config_test_lock();
        let _guard = lock.lock().await;
        let tmp = TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let db = Database::open(&db_path).expect("db");
        let drawer = make_drawer("access-default", "alpha", "decision");
        db.insert_drawer(&drawer).expect("insert drawer");
        configure_record_access(tmp.path(), &db_path, false).await;

        dispatch_access_update(db_path.clone(), vec![drawer.id.clone()]);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(access_count(&db_path, &drawer.id), 0);
        let counters = crate::observability::resource_counters();
        assert_eq!(counters.access_writeback_skipped_total, 1);
        assert_eq!(counters.access_writeback_scheduled_total, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_access_update_writes_only_when_opted_in() {
        let lock = crate::core::config::global_config_test_lock();
        let _guard = lock.lock().await;
        let tmp = TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let db = Database::open(&db_path).expect("db");
        let drawer = make_drawer("access-opt-in", "alpha", "decision");
        db.insert_drawer(&drawer).expect("insert drawer");
        configure_record_access(tmp.path(), &db_path, true).await;

        dispatch_access_update(db_path.clone(), vec![drawer.id.clone()]);

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if access_count(&db_path, &drawer.id) == 1 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "access writeback did not complete"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let counters = crate::observability::resource_counters();
        assert_eq!(counters.access_writeback_scheduled_total, 1);
        assert_eq!(counters.access_writeback_skipped_total, 0);
        assert_eq!(counters.access_writeback_failed_total, 0);
    }

    #[test]
    fn search_excludes_expired_drawers_by_default_and_include_expired_returns_them() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");
        let mut active = make_drawer("active", "alpha", "decision");
        active.content = "needle active".to_string();
        let mut expired = make_drawer("expired", "alpha", "decision");
        expired.content = "needle expired".to_string();

        db.insert_drawer_with_project_validity(&active, None, None, None, None)
            .expect("insert active");
        db.insert_drawer_with_project_validity(&expired, None, None, Some("0"), Some("1"))
            .expect("insert expired");

        let results = search_bm25_only_with_options(
            &db,
            "needle",
            route(),
            &ProjectSearchScope::all_projects(),
            SearchOptions::default(),
            10,
        )
        .expect("search");
        let ids = results
            .iter()
            .map(|result| result.drawer_id.as_str())
            .collect::<Vec<_>>();
        assert!(ids.contains(&"active"));
        assert!(!ids.contains(&"expired"));

        let results = search_bm25_only_with_options(
            &db,
            "needle",
            route(),
            &ProjectSearchScope::all_projects(),
            SearchOptions {
                include_expired: true,
                ..SearchOptions::default()
            },
            10,
        )
        .expect("search include expired");
        let ids = results
            .iter()
            .map(|result| result.drawer_id.as_str())
            .collect::<Vec<_>>();
        assert!(ids.contains(&"active"));
        assert!(ids.contains(&"expired"));
    }

    #[test]
    fn search_excludes_future_valid_from_by_default() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");
        let mut future = make_drawer("future", "alpha", "decision");
        future.content = "needle future".to_string();

        db.insert_drawer_with_project_validity(&future, None, None, Some("4102444800"), None)
            .expect("insert future");

        let results = search_bm25_only_with_options(
            &db,
            "needle",
            route(),
            &ProjectSearchScope::all_projects(),
            SearchOptions::default(),
            10,
        )
        .expect("search");
        assert!(results.is_empty(), "future-valid drawer must be hidden");
    }

    #[test]
    fn tunnel_fanout_cap_limits_cross_project_expansion() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");
        let source = make_drawer("alpha-1", "alpha", "decision");
        seed_cross_project(&db, &source, 10);

        let mut results = vec![make_result(&source)];
        inject_tunnel_hints_with_cap(&db, &mut results, &scoped_to_proj_a(), 3, usize::MAX, 1.0);

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
        inject_tunnel_hints_with_cap(&db, &mut results, &scoped_to_proj_a(), 0, usize::MAX, 1.0);

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
        inject_tunnel_hints_with_cap(&db, &mut results, &scoped_to_proj_a(), 100, usize::MAX, 1.0);

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
        inject_tunnel_hints_with_cap(&db, &mut results, &scoped_to_proj_a(), 2, usize::MAX, 1.0);

        // SQL LIMIT = fanout_cap + 1 = 3.  Alpha's query returns 3 beta rows
        // (beta-9, beta-8, beta-7 DESC order); alpha's Rust cap adds 2 (beta-9,
        // beta-8).  Gamma's query also returns the same 3 rows; beta-9 and
        // beta-8 are already in `seen_ids`, so only beta-7 is fresh -> 1 tunnel
        // row from gamma.  Total = 2 source + 2 (alpha) + 1 (gamma) = 5.
        assert_eq!(
            results.len(),
            5,
            "expected 2 source + 2 (alpha) + 1 (gamma) = 5, got {}",
            results.len()
        );
    }

    #[test]
    fn tunnel_penalty_scales_similarity_and_effective_importance() {
        // GIVEN: source result with similarity=0.9 and one cross-project tunnel target
        //        whose drawer row carries effective_importance=4.0 (set directly to bypass
        //        the migration default of 0.0; INSERT does not write this column).
        // WHEN:  inject_tunnel_hints_with_cap runs with penalty=0.5
        // THEN:  tunnel.similarity == 0.9 * 0.5 = 0.45 and
        //        tunnel.effective_importance == 4.0 * 0.5 = 2.0, so an in-project peer at
        //        the same raw similarity outranks the tunnel on the RRF similarity axis.
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");

        let source = make_drawer("alpha-1", "alpha", "decision");
        db.insert_drawer_with_project(&source, Some("proj-a"))
            .expect("insert source");
        let tunnel_target = make_drawer("beta-1", "beta", "decision");
        db.insert_drawer_with_project(&tunnel_target, Some("proj-b"))
            .expect("insert tunnel target");
        db.conn()
            .execute(
                "UPDATE drawers SET effective_importance = 4.0 WHERE id = ?1",
                rusqlite::params!["beta-1"],
            )
            .expect("set effective_importance on tunnel row");

        let mut results = vec![make_result(&source)];

        inject_tunnel_hints_with_cap(&db, &mut results, &scoped_to_proj_a(), 1, usize::MAX, 0.5);

        assert_eq!(results.len(), 2, "expected 1 source + 1 tunnel = 2");
        let tunnel = results
            .iter()
            .find(|r| r.source == SearchResultSource::TunnelCrossProject)
            .expect("tunnel result present");
        assert!(
            (tunnel.similarity - 0.45).abs() < 1e-6,
            "tunnel similarity should be 0.9 * 0.5 = 0.45, got {}",
            tunnel.similarity
        );
        assert!(
            (tunnel.effective_importance - 2.0).abs() < 1e-6,
            "tunnel effective_importance should be 4.0 * 0.5 = 2.0, got {}",
            tunnel.effective_importance
        );
        assert!(
            tunnel.similarity < results[0].similarity,
            "penalized tunnel similarity ({}) must be below source ({}) at equal raw score",
            tunnel.similarity,
            results[0].similarity
        );
    }

    #[test]
    fn tunnel_penalty_one_preserves_raw_similarity() {
        // penalty=1.0 must be a no-op: tunnel result's similarity equals the source's.
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");
        let source = make_drawer("alpha-1", "alpha", "decision");
        seed_cross_project(&db, &source, 1);

        let mut results = vec![make_result(&source)];
        inject_tunnel_hints_with_cap(&db, &mut results, &scoped_to_proj_a(), 1, usize::MAX, 1.0);

        let tunnel = results
            .iter()
            .find(|r| r.source == SearchResultSource::TunnelCrossProject)
            .expect("tunnel result present");
        assert!(
            (tunnel.similarity - 0.9).abs() < 1e-6,
            "penalty=1.0 must leave similarity=0.9 unchanged, got {}",
            tunnel.similarity
        );
    }

    #[test]
    fn tunnel_drawers_for_room_sql_limit_bounds_returned_rows() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");
        let source = make_drawer("alpha-1", "alpha", "decision");
        // Insert 20 beta drawers -- well above any reasonable fanout cap.
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
        inject_tunnel_hints_with_cap(
            &db,
            &mut results,
            &ProjectSearchScope::all_projects(),
            0,
            8,
            1.0,
        );

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
        inject_tunnel_hints_with_cap(
            &db,
            &mut results,
            &ProjectSearchScope::all_projects(),
            0,
            8,
            1.0,
        );

        assert_eq!(results[0].tunnel_hints.len(), 5, "exactly 5 sibling hints");
        assert!(
            !results[0].tunnel_hints.iter().any(|h| h.starts_with("… +")),
            "no sentinel expected when under cap"
        );
    }

    #[test]
    fn test_tunnel_hints_sentinel_count_is_correct() {
        // 49 siblings, cap=8 -> show 8, sentinel = "... +41 more"
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");
        seed_many_wings(&db, "alpha", 49, "room-shared");

        let source = make_drawer("alpha-1", "alpha", "room-shared");
        let mut results = vec![make_result(&source)];
        inject_tunnel_hints_with_cap(
            &db,
            &mut results,
            &ProjectSearchScope::all_projects(),
            0,
            8,
            1.0,
        );

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
        inject_tunnel_hints_with_cap(
            &db,
            &mut results,
            &ProjectSearchScope::all_projects(),
            0,
            8,
            1.0,
        );

        assert!(
            !results[0].tunnel_hints.iter().any(|h| h == "alpha"),
            "own wing must not appear in tunnel_hints"
        );
    }

    #[test]
    fn test_tunnel_hints_cap_config_override() {
        // display_cap=3 -> 3 real hints + 1 sentinel
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");
        seed_many_wings(&db, "alpha", 10, "room-shared");

        let source = make_drawer("alpha-1", "alpha", "room-shared");
        let mut results = vec![make_result(&source)];
        inject_tunnel_hints_with_cap(
            &db,
            &mut results,
            &ProjectSearchScope::all_projects(),
            0,
            3,
            1.0,
        );

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

    // --- exact vector path: decorate-sort-undecorate (issue #250) ---

    /// Build a synthetic `ExactCandidate` whose embedding blob is the
    /// little-endian encoding of `embedding`. Distance closures in these tests
    /// decode that blob to derive a deterministic distance.
    fn exact_candidate(id: &str, embedding: &[f32]) -> ExactCandidate {
        let blob: Vec<u8> = embedding.iter().flat_map(|v| v.to_le_bytes()).collect();
        (
            id.to_string(),
            format!("content {id}"),
            "alpha".to_string(),
            Some("decision".to_string()),
            format!("{id}.md"),
            None,
            blob,
        )
    }

    fn decode_first_f32(blob: &[u8]) -> f64 {
        let bytes: [u8; 4] = blob[..4].try_into().expect("4-byte blob prefix");
        f64::from(f32::from_le_bytes(bytes))
    }

    /// The core regression guard for issue #250: the cosine distance must be
    /// computed O(n) -- exactly once per candidate -- not O(n log n) inside the
    /// sort comparator. A call counter on the injected distance function proves
    /// the invocation count equals the candidate count and is independent of
    /// `top_k`.
    #[test]
    fn rank_exact_candidates_scores_each_candidate_exactly_once() {
        let n = 64usize;
        let rows: Vec<ExactCandidate> = (0..n)
            .map(|i| exact_candidate(&format!("d{i:02}"), &[i as f32]))
            .collect();

        let mut calls = 0usize;
        let top_k = 5usize;
        let ranked = rank_exact_candidates(rows, top_k, |_id, blob| {
            calls += 1;
            Ok(decode_first_f32(blob))
        });

        // O(n): one distance evaluation per candidate, regardless of top_k.
        // The legacy comparator-based design called it ~2*n*log2(n) times.
        assert_eq!(
            calls, n,
            "distance must be computed exactly once per candidate (O(n))"
        );
        assert_eq!(ranked.len(), top_k, "output truncated to top_k");

        // Ascending distance: smallest stored values first.
        let ids: Vec<&str> = ranked.iter().map(|(_, row)| row.0.as_str()).collect();
        assert_eq!(ids, vec!["d00", "d01", "d02", "d03", "d04"]);
        let dists: Vec<f64> = ranked
            .iter()
            .map(|(d, _)| *d.as_ref().expect("ok distance"))
            .collect();
        assert!(
            dists.windows(2).all(|w| w[0] <= w[1]),
            "cached distances must be ascending: {dists:?}"
        );
    }

    /// Ordering semantics must match the legacy comparator byte-for-byte:
    /// valid distances ascending, invalid-blob candidates last, and two
    /// invalid candidates tie-broken by `drawer_id`.
    #[test]
    fn rank_exact_candidates_orders_invalid_blobs_last_with_drawer_id_tiebreak() {
        let rows = vec![
            exact_candidate("good-2", &[2.0]),
            exact_candidate("bad-z", &[0.0]),
            exact_candidate("good-1", &[1.0]),
            exact_candidate("bad-a", &[0.0]),
        ];
        let ranked = rank_exact_candidates(rows, 10, |id, blob| {
            if id.starts_with("bad") {
                Err(SearchError::InvalidEmbeddingBlob {
                    drawer_id: id.to_string(),
                })
            } else {
                Ok(decode_first_f32(blob))
            }
        });

        let ids: Vec<&str> = ranked.iter().map(|(_, row)| row.0.as_str()).collect();
        assert_eq!(
            ids,
            vec!["good-1", "good-2", "bad-a", "bad-z"],
            "valid ascending, invalid last, invalid tie-broken by drawer_id"
        );
    }

    /// End-to-end: the exact path (taken because the corpus is far below the
    /// 4096 gate) must return drawers ordered by ascending cosine distance with
    /// strictly decreasing similarity.
    #[test]
    fn exact_vector_path_preserves_cosine_ordering() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");

        let drawers = [
            ("d-near", vec![1.0_f32, 0.0, 0.0, 0.0]),
            ("d-mid", vec![0.6_f32, 0.8, 0.0, 0.0]),
            ("d-far", vec![0.0_f32, 0.0, 1.0, 0.0]),
        ];
        for (id, embedding) in &drawers {
            let drawer = make_drawer(id, "alpha", "decision");
            db.insert_drawer_with_project(&drawer, None)
                .expect("insert drawer");
            db.insert_vector(id, embedding).expect("insert vector");
        }

        let query = vec![1.0_f32, 0.0, 0.0, 0.0];
        let results = search_by_vector(
            &db,
            &query,
            route(),
            &ProjectSearchScope::all_projects(),
            10,
        )
        .expect("vector search");

        let ids: Vec<&str> = results.iter().map(|r| r.drawer_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["d-near", "d-mid", "d-far"],
            "results ordered by ascending cosine distance"
        );

        let sims: Vec<f32> = results.iter().map(|r| r.similarity).collect();
        assert!(
            sims[0] > sims[1] && sims[1] > sims[2],
            "similarity must strictly decrease along the ranking: {sims:?}"
        );
    }

    /// Regression for GitHub #309: a freshly-ingested high-importance drawer must
    /// not be buried in the importance rerank by a persisted `effective_importance`
    /// of 0.0. Pre-fix, the INSERT omitted the column (so it took the column
    /// DEFAULT 0.0) and the read fallback used a NULL-only `COALESCE`, so
    /// `rerank_by_effective_importance` sorted the fresh drawer last and
    /// `truncate(top_k)` dropped it. This drives the real BM25 rerank pipeline
    /// (`apply_consistent_effective_importance` -> `rerank_by_effective_importance`
    /// -> `truncate`) with no embedder; under the default `DecayMode::None` it is
    /// fully deterministic.
    #[test]
    fn fresh_high_importance_drawer_not_buried_by_zero_effective_importance() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");

        // Fresh, high-importance drawer ingested through the real INSERT path with
        // the constructor's 0.0 sentinel left in place. Fix (B) must seed its
        // persisted effective_importance from `importance` (4.0); pre-fix it would
        // land at the column DEFAULT 0.0.
        let mut fresh = make_drawer("fresh-309", "alpha", "decision");
        fresh.content = "needle three zero nine".to_string();
        fresh.importance = 4;
        db.insert_drawer_with_project_validity(&fresh, None, None, None, None)
            .expect("insert fresh");

        // Older rival with a positive effective_importance (as the v10 migration
        // backfill would have produced). Pre-fix this outranks the 0.0 fresh drawer
        // and, at top_k = 1, evicts it entirely.
        let mut rival = make_drawer("rival-low", "alpha", "decision");
        rival.content = "needle rival".to_string();
        rival.importance = 1;
        db.insert_drawer_with_project_validity(&rival, None, None, None, None)
            .expect("insert rival");
        db.conn()
            .execute(
                "UPDATE drawers SET effective_importance = 2.0 WHERE id = ?1",
                ["rival-low"],
            )
            .expect("force rival effective_importance");

        let results = search_bm25_only_with_options(
            &db,
            "needle",
            route(),
            &ProjectSearchScope::all_projects(),
            SearchOptions::default(),
            1,
        )
        .expect("search");

        assert_eq!(
            results.first().map(|result| result.drawer_id.as_str()),
            Some("fresh-309"),
            "fresh importance-4 drawer must rank #1 over the importance-2 rival, not \
             be buried by a persisted effective_importance of 0.0 (GitHub #309)"
        );
    }

    /// Regression for GitHub #309 (read-time guard): a row whose
    /// `effective_importance` column holds a literal 0.0 (e.g. ingested before the
    /// seed fix) must read back as its base `importance` via
    /// `NULLIF(effective_importance, 0.0)`, not as 0.0. Exercises the shared
    /// `DRAWER_SELECT_COLUMNS` fallback through `get_drawer`.
    #[test]
    fn persisted_zero_effective_importance_reads_back_as_importance() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");

        let mut drawer = make_drawer("legacy-zero", "alpha", "decision");
        drawer.importance = 3;
        db.insert_drawer_with_project_validity(&drawer, None, None, None, None)
            .expect("insert drawer");
        // Simulate a legacy row that persisted the 0.0 sentinel directly.
        db.conn()
            .execute(
                "UPDATE drawers SET effective_importance = 0.0 WHERE id = ?1",
                ["legacy-zero"],
            )
            .expect("force legacy 0.0");

        let fetched = db
            .get_drawer("legacy-zero")
            .expect("get_drawer")
            .expect("drawer exists");
        assert_eq!(
            fetched.effective_importance, 3.0,
            "persisted 0.0 must fall back to base importance (3.0) via NULLIF, not stay 0.0"
        );
    }

    /// Regression for the #309 follow-up (stale-penalty persistence): applying a
    /// stale penalty to a legacy row stuck at the `effective_importance` 0.0
    /// sentinel must persist `importance * penalty` (non-zero), not
    /// `0.0 * penalty = 0.0`. Otherwise the row stays at 0.0 and the read fallback
    /// would surface it at full base importance, bypassing the P13 stale down-rank.
    #[test]
    fn apply_stale_penalty_to_legacy_zero_ei_persists_importance_times_penalty() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");

        let mut drawer = make_drawer("legacy-stale", "alpha", "decision");
        drawer.importance = 3;
        db.insert_drawer_with_project_validity(&drawer, None, None, None, None)
            .expect("insert drawer");
        // Force the legacy 0.0 sentinel that #309 left behind.
        db.conn()
            .execute(
                "UPDATE drawers SET effective_importance = 0.0 WHERE id = ?1",
                ["legacy-stale"],
            )
            .expect("force legacy 0.0");

        // Default stale penalty (0.5), as the fact-check path applies it.
        db.apply_stale_penalty_to_drawer("legacy-stale", 0.5)
            .expect("apply stale penalty");

        let fetched = db
            .get_drawer("legacy-stale")
            .expect("get_drawer")
            .expect("drawer exists");
        assert_eq!(
            fetched.effective_importance, 1.5,
            "stale penalty on a legacy 0.0 row must persist importance*penalty \
             (3 * 0.5 = 1.5), not stay at 0.0 * 0.5 = 0.0"
        );
    }

    /// Regression for the #309 round-3 finding (cumulative stale-penalty in the
    /// persist path): re-penalizing a legacy row that is BOTH stuck at the
    /// `effective_importance` 0.0 sentinel AND already carries a prior
    /// `stale_penalty_applied` must compound the penalties, not drop the old one.
    /// The persist-path fallback derives the pre-penalty value as
    /// `importance * COALESCE(stale_penalty_applied, 1.0)` — SQLite resolves every
    /// `SET` right-hand side against the row's pre-UPDATE values, so the penalty
    /// read here is the OLD multiplier. Applying 0.5 to a row with importance=3,
    /// stale_penalty_applied=0.5 must yield effective_importance = 3 * 0.5 * 0.5 =
    /// 0.75 and stale_penalty_applied = 0.5 * 0.5 = 0.25 — coherent with the read
    /// fallback. Pre-fix the fallback ignored the old penalty and persisted
    /// 3 * 0.5 = 1.5, which (being non-zero) also disabled the read fallback,
    /// leaving the stale row over-ranked until a later recompute.
    #[test]
    fn repenalizing_legacy_zero_ei_compounds_cumulative_stale_penalty() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");

        let mut drawer = make_drawer("legacy-recompound", "alpha", "decision");
        drawer.importance = 3;
        db.insert_drawer_with_project_validity(&drawer, None, None, None, None)
            .expect("insert drawer");
        // Already-penalized legacy row: 0.0 sentinel + a prior 0.5 stale penalty.
        db.conn()
            .execute(
                "UPDATE drawers SET effective_importance = 0.0, stale_penalty_applied = 0.5 \
                 WHERE id = ?1",
                ["legacy-recompound"],
            )
            .expect("force legacy stale 0.0");

        // Apply the default stale penalty again (fact-check path re-penalizing).
        db.apply_stale_penalty_to_drawer("legacy-recompound", 0.5)
            .expect("apply stale penalty");

        let fetched = db
            .get_drawer("legacy-recompound")
            .expect("get_drawer")
            .expect("drawer exists");
        assert_eq!(
            fetched.effective_importance, 0.75,
            "re-penalizing a legacy 0.0 row must compound the prior penalty: \
             3 * 0.5 (old) * 0.5 (new) = 0.75, not 3 * 0.5 = 1.5"
        );

        let persisted_penalty: f64 = db
            .conn()
            .query_row(
                "SELECT stale_penalty_applied FROM drawers WHERE id = ?1",
                ["legacy-recompound"],
                |row| row.get(0),
            )
            .expect("read stale_penalty_applied");
        assert_eq!(
            persisted_penalty, 0.25,
            "cumulative stale penalty must compound: 0.5 (old) * 0.5 (new) = 0.25"
        );
    }

    /// Regression for the #309 follow-up (penalty-aware read fallback): a legacy
    /// row stale-penalized *before* the persistence fix is stuck at
    /// `effective_importance = 0.0` with `stale_penalty_applied < 1.0`. The read
    /// fallback must rank it at `importance * penalty`, not full base importance, so
    /// fact-check down-ranks survive (P13). A never-penalized 0.0 row (penalty
    /// default 1.0) must still fall back to full importance — no re-burial.
    #[test]
    fn read_fallback_applies_stale_penalty_for_legacy_zero_ei() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");

        // Stale-penalized legacy row stuck at the 0.0 sentinel.
        let mut stale = make_drawer("legacy-stale-read", "alpha", "decision");
        stale.importance = 3;
        db.insert_drawer_with_project_validity(&stale, None, None, None, None)
            .expect("insert stale");
        db.conn()
            .execute(
                "UPDATE drawers SET effective_importance = 0.0, stale_penalty_applied = 0.5 \
                 WHERE id = ?1",
                ["legacy-stale-read"],
            )
            .expect("force legacy stale 0.0");

        // Never-penalized legacy row (stale_penalty_applied defaults to 1.0).
        let mut fresh = make_drawer("legacy-fresh-read", "alpha", "decision");
        fresh.importance = 4;
        db.insert_drawer_with_project_validity(&fresh, None, None, None, None)
            .expect("insert fresh");
        db.conn()
            .execute(
                "UPDATE drawers SET effective_importance = 0.0 WHERE id = ?1",
                ["legacy-fresh-read"],
            )
            .expect("force legacy 0.0");

        let stale_fetched = db
            .get_drawer("legacy-stale-read")
            .expect("get_drawer")
            .expect("stale exists");
        assert_eq!(
            stale_fetched.effective_importance, 1.5,
            "stale-penalized 0.0 row must read back as importance*penalty \
             (3 * 0.5 = 1.5) via the fallback"
        );

        let fresh_fetched = db
            .get_drawer("legacy-fresh-read")
            .expect("get_drawer")
            .expect("fresh exists");
        assert_eq!(
            fresh_fetched.effective_importance, 4.0,
            "never-penalized 0.0 row must read back at full importance (4.0); the \
             penalty multiplier defaults to 1.0 and must not collapse it (no #309 re-burial)"
        );
    }

    /// Regression for the #309 follow-up (search ranking): a stale-penalized legacy
    /// row stuck at `effective_importance = 0.0` must be ranked by
    /// `importance * penalty` in the search rerank, not full importance. The stale
    /// importance-4 row (penalty 0.5 -> effective 2.0) must lose top_k=1 to an
    /// importance-1 rival with a real effective_importance of 2.5. Pre-fix the
    /// penalty was ignored, the stale row computed 4.0 and wrongly evicted the rival.
    /// Exercises the consistent-snapshot fallback (`search/mod.rs`) ->
    /// `rerank_by_effective_importance`; deterministic under `DecayMode::None`.
    #[test]
    fn stale_penalized_zero_ei_ranks_below_real_rival_in_search() {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("db");

        let mut stale = make_drawer("stale-hi", "alpha", "decision");
        stale.content = "needle stale".to_string();
        stale.importance = 4;
        db.insert_drawer_with_project_validity(&stale, None, None, None, None)
            .expect("insert stale");
        db.conn()
            .execute(
                "UPDATE drawers SET effective_importance = 0.0, stale_penalty_applied = 0.5 \
                 WHERE id = ?1",
                ["stale-hi"],
            )
            .expect("force stale 0.0");

        let mut rival = make_drawer("rival-mid", "alpha", "decision");
        rival.content = "needle rival".to_string();
        rival.importance = 1;
        db.insert_drawer_with_project_validity(&rival, None, None, None, None)
            .expect("insert rival");
        db.conn()
            .execute(
                "UPDATE drawers SET effective_importance = 2.5 WHERE id = ?1",
                ["rival-mid"],
            )
            .expect("force rival effective_importance");

        let results = search_bm25_only_with_options(
            &db,
            "needle",
            route(),
            &ProjectSearchScope::all_projects(),
            SearchOptions::default(),
            1,
        )
        .expect("search");

        assert_eq!(
            results.first().map(|result| result.drawer_id.as_str()),
            Some("rival-mid"),
            "stale importance-4 row (effective 4 * 0.5 = 2.0) must rank below the rival's \
             real effective_importance 2.5; pre-fix the ignored penalty let it compute 4.0 and win"
        );
    }
}
