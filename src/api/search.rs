//! Bounded REST search orchestration.
//!
//! A caller deadline is a single wall-clock budget shared by routing,
//! embedding, database search, fallback, and reranking. The response body keeps
//! the legacy result-array contract; redacted execution metadata travels in a
//! bounded response header for clients that need actionable timeout details.

use std::time::Duration;

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, header::USER_AGENT},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use tokio::time::Instant;

use crate::core::{
    config::ConfigHandle,
    project::{ProjectSearchScope, resolve_project_id},
    types::{RouteDecision, SearchResult},
};
use crate::search::{
    SearchMode, SearchOptions, SearchTelemetryStage, VectorSearchCircuit,
    bm25_fallback_warning_degraded, bm25_fallback_warning_dimension_mismatch,
    bm25_fallback_warning_embed_error, bm25_fallback_warning_missing_query_vector,
    maybe_rerank_search_results, resolve_route, search_bm25_only_with_options,
    search_with_vector_and_scope_options,
};

use super::{
    handlers::{ApiError, internal_error},
    state::{ApiState, SearchTelemetryOutcome},
};

mod contract;

use contract::{
    SearchBudget, SearchExecutionMetadata, SearchResponseMetadata, SearchResultDto,
    attach_search_headers, duration_ms, embedding_timeout_warning, reranker_timeout_warning,
    rest_search_timeout_warning, safe_correlation_id,
};

const MIN_CALLER_DEADLINE: Duration = Duration::from_millis(100);

#[derive(Debug, Deserialize)]
pub(super) struct SearchQuery {
    q: String,
    wing: Option<String>,
    room: Option<String>,
    top_k: Option<usize>,
    scope: Option<String>,
    project_id: Option<String>,
    include_global: Option<bool>,
    all_projects: Option<bool>,
    include_raw_turns: Option<bool>,
    include_expired: Option<bool>,
    deadline_ms: Option<u64>,
    correlation_id: Option<String>,
}

struct RestBm25SearchRequest {
    query: String,
    route: RouteDecision,
    scope: ProjectSearchScope,
    search_options: SearchOptions,
    top_k: usize,
    deadline: Duration,
    stage: SearchTelemetryStage,
    search_mode: SearchMode,
}

struct RestHybridSearchRequest {
    query: String,
    route: RouteDecision,
    scope: ProjectSearchScope,
    search_options: SearchOptions,
    top_k: usize,
    query_vector: Vec<f32>,
    deadline: Duration,
    search_mode: SearchMode,
}

struct SearchExecution {
    state: ApiState,
    telemetry: super::state::SearchTelemetryGuard,
    budget: SearchBudget,
    correlation_id: String,
    query: SearchQuery,
    route: RouteDecision,
    scope: ProjectSearchScope,
    search_options: SearchOptions,
    top_k: usize,
    search_mode: SearchMode,
    warnings: Vec<String>,
    metadata: SearchExecutionMetadata,
    route_elapsed: Duration,
    embed_elapsed: Duration,
    db_elapsed: Duration,
    query_vector: Option<Vec<f32>>,
    stop_search: bool,
    bm25_fallback_enabled: bool,
}

pub(super) async fn search_handler(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<SearchQuery>,
) -> Result<Response, ApiError> {
    let config = ConfigHandle::current();
    let total_deadline = caller_deadline(&query, config.api.search_db_deadline_secs)?;
    let budget = SearchBudget::new(total_deadline);
    let correlation_id = safe_correlation_id(query.correlation_id.as_deref());
    let (scope, scope_label) = resolve_api_search_scope(&query, config.as_ref())?;
    let search_options = SearchOptions {
        include_raw_turns: query.include_raw_turns.unwrap_or(false),
        include_expired: query.include_expired.unwrap_or(false),
        ..SearchOptions::default()
    };
    let top_k = query.top_k.unwrap_or(10);
    let db_deadline = Duration::from_secs(config.api.search_db_deadline_secs);
    let telemetry = state.search_telemetry().start(
        search_client_from_headers(&headers),
        scope_label,
        top_k,
        total_deadline,
    );
    let mut metadata = SearchExecutionMetadata::default();
    let mut search_mode = SearchMode::Hybrid;
    let mut warnings = Vec::new();
    let mut route_elapsed = Duration::ZERO;
    let mut embed_elapsed = Duration::ZERO;
    let db_elapsed = Duration::ZERO;

    telemetry.set_stage(SearchTelemetryStage::Routing.as_str());
    let route_started = Instant::now();
    let route_limit = budget.route_limit(db_deadline);
    let route = resolve_route_bounded(&state, &query, search_mode, route_limit).await?;
    route_elapsed += Instant::now().saturating_duration_since(route_started);
    let route = match route {
        Some(route) => route,
        None => {
            metadata.timeout(SearchTelemetryStage::Routing, "daemon.search_db");
            metadata.fallback("route_defaults");
            warnings.push(rest_search_timeout_warning("route resolution", route_limit));
            fallback_route(&query)
        }
    };

    let embed_snapshot = crate::embed::global_embed_status().snapshot();
    let vector_search_circuit =
        VectorSearchCircuit::from_config_and_snapshot(config.as_ref(), &embed_snapshot);
    let mut stop_search = false;
    let query_vector = if vector_search_circuit.bm25_fallback_enabled && vector_search_circuit.open
    {
        search_mode = SearchMode::Bm25Only;
        metadata.fallback("bm25");
        warnings.push(bm25_fallback_warning_degraded(
            vector_search_circuit.failure_count,
        ));
        None
    } else {
        telemetry.set_stage(SearchTelemetryStage::Embedding.as_str());
        let embed_started = Instant::now();
        let embed_limit = budget.primary_limit(Duration::from_secs(
            vector_search_circuit.search_deadline_secs,
        ));
        let embed_result = if embed_limit.is_zero() {
            None
        } else {
            Some(
                tokio::time::timeout(embed_limit, async {
                    let embedder = state.embedder_factory.build().await?;
                    embedder.embed(&[query.q.as_str()]).await
                })
                .await,
            )
        };
        embed_elapsed += Instant::now().saturating_duration_since(embed_started);
        match embed_result {
            Some(Ok(Ok(vectors))) => match vectors.into_iter().next() {
                Some(vector) => Some(vector),
                None if vector_search_circuit.bm25_fallback_enabled => {
                    search_mode = SearchMode::Bm25Only;
                    metadata.fallback("bm25");
                    warnings.push(bm25_fallback_warning_missing_query_vector());
                    None
                }
                None => return Err(internal_error("embedder returned no vector")),
            },
            Some(Ok(Err(error))) if vector_search_circuit.bm25_fallback_enabled => {
                search_mode = SearchMode::Bm25Only;
                metadata.fallback("bm25");
                warnings.push(bm25_fallback_warning_embed_error(
                    &crate::core::config::scrub_sensitive_text(&error.to_string()),
                ));
                None
            }
            Some(Ok(Err(error))) => return Err(internal_error(error)),
            Some(Err(_)) | None => {
                metadata.timeout(SearchTelemetryStage::Embedding, "daemon.embedding");
                warnings.push(embedding_timeout_warning(embed_limit));
                if vector_search_circuit.bm25_fallback_enabled {
                    search_mode = SearchMode::Bm25Only;
                    metadata.fallback("bm25");
                } else {
                    stop_search = true;
                }
                None
            }
        }
    };

    finish_search(SearchExecution {
        state,
        telemetry,
        budget,
        correlation_id,
        query,
        route,
        scope,
        search_options,
        top_k,
        search_mode,
        warnings,
        metadata,
        route_elapsed,
        embed_elapsed,
        db_elapsed,
        query_vector,
        stop_search,
        bm25_fallback_enabled: vector_search_circuit.bm25_fallback_enabled,
    })
    .await
}

async fn finish_search(execution: SearchExecution) -> Result<Response, ApiError> {
    let SearchExecution {
        state,
        telemetry,
        budget,
        correlation_id,
        query,
        route,
        scope,
        search_options,
        top_k,
        mut search_mode,
        mut warnings,
        mut metadata,
        route_elapsed,
        embed_elapsed,
        mut db_elapsed,
        query_vector,
        stop_search,
        bm25_fallback_enabled,
    } = execution;
    let config = ConfigHandle::current();
    let db_deadline = Duration::from_secs(config.api.search_db_deadline_secs);
    let mut results = Vec::new();
    if !stop_search {
        if let Some(query_vector) = query_vector {
            telemetry.set_stage(SearchTelemetryStage::HybridDb.as_str());
            let search_started = Instant::now();
            let hybrid_limit = budget.primary_limit(db_deadline);
            let hybrid = run_hybrid_search_bounded(
                &state,
                RestHybridSearchRequest {
                    query: query.q.clone(),
                    route: route.clone(),
                    scope: scope.clone(),
                    search_options: search_options.clone(),
                    top_k,
                    query_vector,
                    deadline: hybrid_limit,
                    search_mode,
                },
            )
            .await?;
            db_elapsed += Instant::now().saturating_duration_since(search_started);
            match hybrid {
                Some(Ok(found)) => results = found,
                Some(Err(crate::search::SearchError::VectorDimensionMismatch {
                    current_dim,
                    new_dim,
                })) if bm25_fallback_enabled => {
                    search_mode = SearchMode::Bm25Only;
                    metadata.fallback("bm25");
                    warnings.push(bm25_fallback_warning_dimension_mismatch(
                        new_dim,
                        current_dim,
                    ));
                    telemetry.set_stage(SearchTelemetryStage::Bm25FallbackDb.as_str());
                    results = run_bm25_fallback(
                        &state,
                        RestBm25SearchRequest {
                            query: query.q.clone(),
                            route,
                            scope,
                            search_options,
                            top_k,
                            deadline: budget.fallback_limit(db_deadline),
                            stage: SearchTelemetryStage::Bm25FallbackDb,
                            search_mode: SearchMode::Bm25Only,
                        },
                        &mut warnings,
                        &mut metadata,
                        &mut db_elapsed,
                    )
                    .await?;
                }
                Some(Err(error)) => return Err(internal_error(error)),
                None if bm25_fallback_enabled => {
                    metadata.timeout(SearchTelemetryStage::HybridDb, "daemon.search_db");
                    metadata.fallback("bm25");
                    search_mode = SearchMode::Bm25Only;
                    warnings.push(rest_search_timeout_warning("hybrid search", hybrid_limit));
                    telemetry.set_stage(SearchTelemetryStage::Bm25FallbackDb.as_str());
                    results = run_bm25_fallback(
                        &state,
                        RestBm25SearchRequest {
                            query: query.q.clone(),
                            route,
                            scope,
                            search_options,
                            top_k,
                            deadline: budget.fallback_limit(db_deadline),
                            stage: SearchTelemetryStage::Bm25FallbackDb,
                            search_mode: SearchMode::Bm25Only,
                        },
                        &mut warnings,
                        &mut metadata,
                        &mut db_elapsed,
                    )
                    .await?;
                }
                None => {
                    metadata.timeout(SearchTelemetryStage::HybridDb, "daemon.search_db");
                    warnings.push(rest_search_timeout_warning("hybrid search", hybrid_limit));
                }
            }
        } else {
            telemetry.set_stage(SearchTelemetryStage::Bm25Db.as_str());
            let bm25_started = Instant::now();
            let bm25_limit = budget.fallback_limit(db_deadline);
            let outcome = run_rest_bm25_search_bounded(
                &state,
                RestBm25SearchRequest {
                    query: query.q.clone(),
                    route,
                    scope,
                    search_options,
                    top_k,
                    deadline: bm25_limit,
                    stage: SearchTelemetryStage::Bm25Db,
                    search_mode,
                },
            )
            .await?;
            db_elapsed += Instant::now().saturating_duration_since(bm25_started);
            match outcome {
                Some(Ok(found)) => results = found,
                Some(Err(error)) => return Err(internal_error(error)),
                None => {
                    metadata.timeout(SearchTelemetryStage::Bm25Db, "daemon.search_db");
                    warnings.push(rest_search_timeout_warning("BM25 search", bm25_limit));
                }
            }
        }
    }

    telemetry.set_stage(SearchTelemetryStage::Rerank.as_str());
    let rerank_started = Instant::now();
    if !results.is_empty() {
        let rerank_limit = budget.remaining();
        let original_results = results.clone();
        let rerank = if rerank_limit.is_zero() {
            None
        } else {
            tokio::time::timeout(rerank_limit, maybe_rerank_search_results(&query.q, results))
                .await
                .ok()
        };
        match rerank {
            Some(outcome) => {
                if !outcome.warnings.is_empty() {
                    metadata.fallback("original_ranking");
                }
                if outcome
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("timed out"))
                {
                    metadata.timeout(SearchTelemetryStage::Rerank, "daemon.reranker");
                }
                warnings.extend(outcome.warnings);
                results = outcome.results;
            }
            None => {
                metadata.timeout(SearchTelemetryStage::Rerank, "daemon.reranker");
                metadata.fallback("original_ranking");
                warnings.push(reranker_timeout_warning(rerank_limit));
                results = original_results;
            }
        }
    }
    let rerank_elapsed = Instant::now().saturating_duration_since(rerank_started);

    let result_count = results.len();
    telemetry.finish(SearchTelemetryOutcome {
        search_mode: search_mode.as_str().to_string(),
        route: route_elapsed,
        embed: embed_elapsed,
        db: db_elapsed,
        rerank: rerank_elapsed,
        lock_wait: Duration::ZERO,
        result_count,
        warning_count: warnings.len(),
        partial: metadata.partial(),
        timed_out_stages: metadata.timed_out_stages(),
        fallbacks: metadata.fallbacks.clone(),
    });

    let mut response = Json(
        results
            .into_iter()
            .map(|result| SearchResultDto::from_result(result, search_mode, &warnings))
            .collect::<Vec<_>>(),
    )
    .into_response();
    attach_search_headers(
        &mut response,
        search_mode,
        &warnings,
        &SearchResponseMetadata {
            correlation_id: &correlation_id,
            elapsed_ms: duration_ms(budget.elapsed()),
            deadline_ms: duration_ms(budget.total),
            partial: metadata.partial(),
            retry_safe: true,
            fallback_used: &metadata.fallbacks,
            timeouts: &metadata.timeouts,
        },
    );
    Ok(response)
}

async fn run_bm25_fallback(
    state: &ApiState,
    request: RestBm25SearchRequest,
    warnings: &mut Vec<String>,
    metadata: &mut SearchExecutionMetadata,
    db_elapsed: &mut Duration,
) -> Result<Vec<SearchResult>, ApiError> {
    let started = Instant::now();
    let limit = request.deadline;
    let outcome = run_rest_bm25_search_bounded(state, request).await?;
    *db_elapsed += Instant::now().saturating_duration_since(started);
    match outcome {
        Some(Ok(results)) => Ok(results),
        Some(Err(error)) => Err(internal_error(error)),
        None => {
            metadata.timeout(SearchTelemetryStage::Bm25FallbackDb, "daemon.search_db");
            warnings.push(rest_search_timeout_warning("BM25 fallback", limit));
            Ok(Vec::new())
        }
    }
}

async fn resolve_route_bounded(
    state: &ApiState,
    query: &SearchQuery,
    search_mode: SearchMode,
    deadline: Duration,
) -> Result<Option<RouteDecision>, ApiError> {
    if deadline.is_zero() {
        return Ok(None);
    }
    let route_query = query.q.clone();
    let route_wing = query.wing.clone();
    let route_room = query.room.clone();
    match state
        .run_read_anyhow_bounded_with_telemetry(
            move |db| {
                Ok(resolve_route(
                    db,
                    &route_query,
                    route_wing.as_deref(),
                    route_room.as_deref(),
                ))
            },
            deadline,
            rest_search_read_telemetry(SearchTelemetryStage::Routing, search_mode),
        )
        .await
        .map_err(internal_error)?
    {
        Some(Ok(route)) => Ok(Some(route)),
        Some(Err(error)) => Err(internal_error(error)),
        None => Ok(None),
    }
}

async fn run_hybrid_search_bounded(
    state: &ApiState,
    request: RestHybridSearchRequest,
) -> Result<Option<crate::search::Result<Vec<SearchResult>>>, ApiError> {
    let RestHybridSearchRequest {
        query,
        route,
        scope,
        search_options,
        top_k,
        query_vector,
        deadline,
        search_mode,
    } = request;
    if deadline.is_zero() {
        return Ok(None);
    }
    state
        .run_read_anyhow_bounded_with_telemetry(
            move |db| {
                Ok(search_with_vector_and_scope_options(
                    db,
                    &query,
                    &query_vector,
                    route,
                    &scope,
                    search_options,
                    top_k,
                ))
            },
            deadline,
            rest_search_read_telemetry(SearchTelemetryStage::HybridDb, search_mode),
        )
        .await
        .map_err(internal_error)
}

async fn run_rest_bm25_search_bounded(
    state: &ApiState,
    request: RestBm25SearchRequest,
) -> Result<Option<crate::search::Result<Vec<SearchResult>>>, ApiError> {
    if request.deadline.is_zero() {
        return Ok(None);
    }
    state
        .run_read_anyhow_bounded_with_telemetry(
            move |db| {
                Ok(search_bm25_only_with_options(
                    db,
                    &request.query,
                    request.route,
                    &request.scope,
                    request.search_options,
                    request.top_k,
                ))
            },
            request.deadline,
            rest_search_read_telemetry(request.stage, request.search_mode),
        )
        .await
        .map_err(internal_error)
}

fn rest_search_read_telemetry(
    stage: SearchTelemetryStage,
    search_mode: SearchMode,
) -> crate::observability::OperationTelemetryRecord {
    crate::observability::OperationTelemetryRecord::new(
        crate::observability::OperationTelemetrySource::Rest,
        "GET /api/search",
        "rest.search.read",
    )
    .with_stage(stage.as_str())
    .with_search_mode(search_mode.as_str())
}

fn caller_deadline(query: &SearchQuery, configured_secs: u64) -> Result<Duration, ApiError> {
    let configured = Duration::from_secs(configured_secs);
    let Some(deadline_ms) = query.deadline_ms else {
        return Ok(configured);
    };
    let requested = Duration::from_millis(deadline_ms);
    if requested < MIN_CALLER_DEADLINE {
        return Err(ApiError::new(
            axum::http::StatusCode::BAD_REQUEST,
            format!(
                "deadline_ms must be at least {}",
                duration_ms(MIN_CALLER_DEADLINE)
            ),
        ));
    }
    Ok(requested.min(configured))
}

fn resolve_api_search_scope(
    query: &SearchQuery,
    config: &crate::core::config::Config,
) -> Result<(ProjectSearchScope, String), ApiError> {
    let mut include_global = query.include_global.unwrap_or(false);
    let mut all_projects = query.all_projects.unwrap_or(false);
    let explicit_scope = query
        .scope
        .as_deref()
        .map(str::trim)
        .filter(|scope| !scope.is_empty());
    match explicit_scope {
        Some("global" | "all_projects") => all_projects = true,
        Some("all_wings" | "project") => {
            include_global = false;
            all_projects = false;
        }
        Some("project_plus_global" | "project_global") => {
            include_global = true;
            all_projects = false;
        }
        Some(other) => {
            return Err(ApiError::new(
                axum::http::StatusCode::BAD_REQUEST,
                format!(
                    "invalid scope: {other}; expected project, all_wings, project_plus_global, global, or all_projects"
                ),
            ));
        }
        None => {}
    }
    let resolved_project =
        resolve_project_id(query.project_id.as_deref(), config, None).map_err(internal_error)?;
    let scope = ProjectSearchScope::from_request(
        resolved_project,
        include_global,
        all_projects,
        config.search.strict_project_isolation,
    );
    let wing = query.wing.as_deref().unwrap_or("*");
    let room = query.room.as_deref().unwrap_or("*");
    let label = format!(
        "scope={} project={} wing={} room={}",
        explicit_scope.unwrap_or("legacy"),
        scope.mode.as_sql_mode(),
        wing,
        room
    );
    Ok((scope, label))
}

fn fallback_route(query: &SearchQuery) -> RouteDecision {
    RouteDecision {
        wing: query.wing.clone(),
        room: query.room.clone(),
        confidence: if query.wing.is_some() || query.room.is_some() {
            1.0
        } else {
            0.0
        },
        reason: "bounded REST fallback: route resolution timed out".to_string(),
    }
}

fn search_client_from_headers(headers: &HeaderMap) -> String {
    headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("rest:{value}"))
        .unwrap_or_else(|| "rest:unknown".to_string())
}
