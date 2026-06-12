use std::{
    future::Future,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::core::{
    anchor,
    config::ConfigHandle,
    db::Database,
    project::{ProjectSearchScope, resolve_project_id},
    strata::{count_raw_turn_drawers, is_raw_turn, raw_turn_importance, should_store_raw_turns},
    types::{
        BootstrapEvidenceArgs, Drawer, KnowledgeStatus, KnowledgeTier, MemoryDomain, MemoryKind,
        RouteDecision, SearchResult, SourceType, TaxonomyEntry, default_confidence,
    },
    utils::{
        build_bootstrap_evidence_drawer_id, iso_timestamp, link_superseded_drawer,
        source_file_or_synthetic,
    },
};
use crate::embed::global_embed_status;
use crate::ingest::gating::evaluate_fact_check_gate;
use crate::ingest::normalize::CURRENT_NORMALIZE_VERSION;
use crate::search::{
    SearchMode, SearchOptions, VectorSearchCircuit, bm25_fallback_warning_degraded,
    bm25_fallback_warning_dimension_mismatch, bm25_fallback_warning_embed_error,
    bm25_fallback_warning_missing_query_vector, bm25_fallback_warning_timeout,
    maybe_rerank_search_results, resolve_route, search_bm25_only_with_options,
    search_with_vector_and_scope_options,
};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header::CONTENT_TYPE, header::USER_AGENT},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Notify, mpsc, oneshot};
use tower_http::cors::{AllowOrigin, CorsLayer};

use super::state::{ApiState, SearchTelemetryOutcome, SearchTelemetrySnapshot};

pub const DEFAULT_REST_ADDR: &str = "127.0.0.1:3080";
const HERMES_COMPAT_VERSION: &str = "mempal-hermes-compat/1";
const REST_SEARCH_WARNING_HEADER: &str = "mempal-warnings";

pub async fn serve(listener: tokio::net::TcpListener, state: ApiState) -> std::io::Result<()> {
    serve_with_shutdown(listener, state, shutdown_signal()).await
}

pub async fn serve_with_shutdown<F>(
    listener: tokio::net::TcpListener,
    state: ApiState,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let drain_state = state.clone();
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            shutdown.await;
            if !drain_state.drain_write_queue().await {
                tracing::warn!("REST write queue drain timed out during graceful shutdown");
            }
        })
        .await
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                if let Some(signal) = sigterm.as_mut() {
                    let _ = signal.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/api/search", get(search_handler))
        .route("/api/ingest", post(ingest_handler))
        .route("/api/taxonomy", get(taxonomy_handler))
        .route("/api/status", get(status_handler))
        .route("/api/pinned_facts", get(pinned_facts_handler))
        .merge(super::hermes_compat::routes())
        .with_state(state)
        .layer(cors_layer())
}

fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _| {
            is_local_origin(origin)
        }))
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([CONTENT_TYPE])
}

fn is_local_origin(origin: &HeaderValue) -> bool {
    origin
        .to_str()
        .map(|value| {
            value.starts_with("http://localhost")
                || value.starts_with("https://localhost")
                || value.starts_with("http://127.0.0.1")
                || value.starts_with("https://127.0.0.1")
        })
        .unwrap_or(false)
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
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
}

#[derive(Debug, Deserialize)]
struct IngestRequest {
    content: String,
    wing: String,
    room: Option<String>,
    source: Option<String>,
    source_type: Option<String>,
    confidence: Option<f64>,
    project_id: Option<String>,
    supersedes: Option<String>,
    replace_text: Option<String>,
    valid_from: Option<String>,
    valid_until: Option<String>,
    // Typed memory fields (parity with MCP mempal_ingest)
    memory_kind: Option<String>,
    domain: Option<String>,
    field: Option<String>,
    importance: Option<i32>,
    status: Option<String>,
    tier: Option<String>,
    is_pinned: Option<bool>,
    statement: Option<String>,
    supporting_refs: Option<Vec<String>>,
    counterexample_refs: Option<Vec<String>>,
    teaching_refs: Option<Vec<String>>,
    verification_refs: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
struct IngestResponse {
    drawer_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    drawer_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    chunk_count: usize,
    #[serde(default)]
    dropped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_drawer_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fact_check_warnings: Vec<String>,
}

fn is_zero(v: &usize) -> bool {
    *v == 0
}

fn validate_temporal_param(name: &str, value: Option<&str>) -> Result<(), ApiError> {
    if let Some(raw) = value
        && crate::core::decay::parse_temporal_timestamp_secs(raw).is_none()
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("{name} must be a Unix timestamp or RFC3339 timestamp"),
        ));
    }
    Ok(())
}

fn parse_source_type_param(value: Option<&str>) -> Result<SourceType, ApiError> {
    match value {
        Some(raw) => raw.parse::<SourceType>().map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "source_type must be one of user_explicit, agent_observation, agent_inference, system_generated",
            )
        }),
        None => Ok(SourceType::AgentInference),
    }
}

fn resolve_confidence_param(source_type: SourceType, value: Option<f64>) -> Result<f64, ApiError> {
    match value {
        Some(confidence) if confidence.is_finite() && (0.0..=1.0).contains(&confidence) => {
            Ok(confidence)
        }
        Some(_) => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "confidence must be a finite float between 0.0 and 1.0",
        )),
        None => Ok(default_confidence(source_type)),
    }
}

fn parse_memory_kind(value: &str) -> Result<MemoryKind, ApiError> {
    match value.trim() {
        "evidence" => Ok(MemoryKind::Evidence),
        "knowledge" => Ok(MemoryKind::Knowledge),
        "profile_fact" => Ok(MemoryKind::ProfileFact),
        other => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("invalid memory_kind: {other}; expected evidence, knowledge, or profile_fact"),
        )),
    }
}

fn parse_domain(value: &str) -> Result<MemoryDomain, ApiError> {
    match value.trim() {
        "project" => Ok(MemoryDomain::Project),
        "user" => Ok(MemoryDomain::User),
        "agent" => Ok(MemoryDomain::Agent),
        "skill" => Ok(MemoryDomain::Skill),
        "global" => Ok(MemoryDomain::Global),
        other => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("invalid domain: {other}; expected project, user, agent, skill, or global"),
        )),
    }
}

fn parse_status_opt(value: &str) -> Result<KnowledgeStatus, ApiError> {
    match value.trim() {
        "active" => Ok(KnowledgeStatus::Active),
        "superseded" => Ok(KnowledgeStatus::Superseded),
        "pending_review" => Ok(KnowledgeStatus::PendingReview),
        "candidate" => Ok(KnowledgeStatus::Candidate),
        "promoted" => Ok(KnowledgeStatus::Promoted),
        "canonical" => Ok(KnowledgeStatus::Canonical),
        "demoted" => Ok(KnowledgeStatus::Demoted),
        "retired" => Ok(KnowledgeStatus::Retired),
        other => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("invalid status: {other}"),
        )),
    }
}

fn parse_tier_opt(value: &str) -> Result<KnowledgeTier, ApiError> {
    match value.trim() {
        "qi" => Ok(KnowledgeTier::Qi),
        "shu" => Ok(KnowledgeTier::Shu),
        "dao_ren" => Ok(KnowledgeTier::DaoRen),
        "dao_tian" => Ok(KnowledgeTier::DaoTian),
        other => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("invalid tier: {other}; expected qi, shu, dao_ren, or dao_tian"),
        )),
    }
}

fn memory_kind_slug(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Evidence => "evidence",
        MemoryKind::Knowledge => "knowledge",
        MemoryKind::ProfileFact => "profile_fact",
    }
}

fn domain_slug(domain: MemoryDomain) -> &'static str {
    match domain {
        MemoryDomain::Project => "project",
        MemoryDomain::User => "user",
        MemoryDomain::Agent => "agent",
        MemoryDomain::Skill => "skill",
        MemoryDomain::Global => "global",
    }
}

fn knowledge_status_slug(status: &KnowledgeStatus) -> &'static str {
    match status {
        KnowledgeStatus::Active => "active",
        KnowledgeStatus::Superseded => "superseded",
        KnowledgeStatus::PendingReview => "pending_review",
        KnowledgeStatus::Candidate => "candidate",
        KnowledgeStatus::Promoted => "promoted",
        KnowledgeStatus::Canonical => "canonical",
        KnowledgeStatus::Demoted => "demoted",
        KnowledgeStatus::Retired => "retired",
    }
}

fn knowledge_tier_slug(tier: &KnowledgeTier) -> &'static str {
    match tier {
        KnowledgeTier::Qi => "qi",
        KnowledgeTier::Shu => "shu",
        KnowledgeTier::DaoRen => "dao_ren",
        KnowledgeTier::DaoTian => "dao_tian",
    }
}

fn normalize_refs(values: Option<&[String]>) -> Vec<String> {
    values
        .unwrap_or(&[])
        .iter()
        .filter_map(|v| {
            let t = v.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        })
        .collect()
}

struct ValidatedIngestRequest {
    source_type: SourceType,
    confidence: f64,
    typed_memory_kind: Option<MemoryKind>,
    typed_domain: Option<MemoryDomain>,
    typed_field: Option<String>,
    typed_status: Option<KnowledgeStatus>,
    typed_tier: Option<KnowledgeTier>,
    typed_is_pinned: bool,
    typed_statement: Option<String>,
    typed_supporting_refs: Vec<String>,
    typed_counterexample_refs: Vec<String>,
    typed_teaching_refs: Vec<String>,
    typed_verification_refs: Vec<String>,
}

fn validate_ingest_request(request: &IngestRequest) -> Result<ValidatedIngestRequest, ApiError> {
    validate_temporal_param("valid_from", request.valid_from.as_deref())?;
    validate_temporal_param("valid_until", request.valid_until.as_deref())?;
    let source_type = parse_source_type_param(request.source_type.as_deref())?;
    let confidence = resolve_confidence_param(source_type, request.confidence)?;
    let typed_memory_kind = request
        .memory_kind
        .as_deref()
        .map(parse_memory_kind)
        .transpose()?;
    let typed_domain = request.domain.as_deref().map(parse_domain).transpose()?;
    let typed_field = request
        .field
        .as_deref()
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty());
    let typed_status = request
        .status
        .as_deref()
        .map(parse_status_opt)
        .transpose()?;
    let typed_tier = request.tier.as_deref().map(parse_tier_opt).transpose()?;
    let typed_statement = request
        .statement
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    Ok(ValidatedIngestRequest {
        source_type,
        confidence,
        typed_memory_kind,
        typed_domain,
        typed_field,
        typed_status,
        typed_tier,
        typed_is_pinned: request.is_pinned.unwrap_or(false),
        typed_statement,
        typed_supporting_refs: normalize_refs(request.supporting_refs.as_deref()),
        typed_counterexample_refs: normalize_refs(request.counterexample_refs.as_deref()),
        typed_teaching_refs: normalize_refs(request.teaching_refs.as_deref()),
        typed_verification_refs: normalize_refs(request.verification_refs.as_deref()),
    })
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    drawer_count: i64,
    taxonomy_count: i64,
    db_size_bytes: u64,
    embedding_status: String,
    search_mode: String,
    embedder_circuit: EmbedderCircuitStatus,
    write_queue: WriteQueueStats,
    feature_flags: FeatureFlags,
    hermes_compat_version: String,
    search_decay_mode: String,
    wings: Vec<ScopeCount>,
    source_type_distribution: Vec<SourceTypeCount>,
    turn_storage: TurnStorageStatus,
    search_telemetry: SearchTelemetrySnapshot,
}

#[derive(Debug, Serialize)]
struct EmbedderCircuitStatus {
    open: bool,
    failure_count: u64,
    failure_threshold: u64,
    bm25_fallback_enabled: bool,
    search_deadline_secs: u64,
    vector_search_mode: String,
}

impl From<VectorSearchCircuit> for EmbedderCircuitStatus {
    fn from(value: VectorSearchCircuit) -> Self {
        Self {
            open: value.open,
            failure_count: value.failure_count,
            failure_threshold: value.failure_threshold,
            bm25_fallback_enabled: value.bm25_fallback_enabled,
            search_deadline_secs: value.search_deadline_secs,
            vector_search_mode: value.vector_search_mode.as_str().to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
struct FeatureFlags {
    typed_ingest: bool,
    pinned_facts: bool,
    compaction: bool,
    sleep_cycle: bool,
    crystallize: bool,
    intelligence_modes: bool,
}

#[derive(Debug, Serialize)]
struct SourceTypeCount {
    source_type: String,
    count: i64,
}

#[derive(Debug, Serialize)]
struct TurnStorageStatus {
    storage_mode: String,
    default_importance: i32,
    raw_turn_count: i64,
    raw_turn_wings: Vec<String>,
    raw_turn_rooms: Vec<String>,
}

struct StatusDbSnapshot {
    drawer_count: i64,
    taxonomy_count: i64,
    db_size_bytes: u64,
    wings: Vec<ScopeCount>,
    source_type_distribution: Vec<SourceTypeCount>,
    raw_turn_count: i64,
}

#[derive(Debug, Serialize)]
struct ScopeCount {
    wing: String,
    room: Option<String>,
    drawer_count: i64,
}

#[derive(Debug, Serialize)]
struct SearchResultDto {
    drawer_id: String,
    content: String,
    wing: String,
    room: Option<String>,
    source_file: String,
    source: String,
    source_type: String,
    confidence: f64,
    similarity: f32,
    route: RouteDecisionDto,
    search_mode: String,
    // Typed metadata fields
    memory_kind: String,
    domain: String,
    field: String,
    importance: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tier: Option<String>,
    is_pinned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    statement: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RouteDecisionDto {
    wing: Option<String>,
    room: Option<String>,
    confidence: f32,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct PinnedFactsQuery {
    wing: Option<String>,
    room: Option<String>,
    domain: Option<String>,
    project_id: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct PinnedFactDto {
    drawer_id: String,
    content: String,
    wing: String,
    room: Option<String>,
    source_file: String,
    memory_kind: String,
    domain: String,
    field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    importance: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pin_order: Option<i64>,
    added_at: String,
}

#[derive(Debug, Serialize)]
struct TaxonomyEntryDto {
    wing: String,
    room: String,
    display_name: Option<String>,
    keywords: Vec<String>,
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
        Some("global" | "all_projects") => {
            all_projects = true;
        }
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
                StatusCode::BAD_REQUEST,
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

fn rest_search_timeout_warning(stage: &str, deadline: Duration) -> String {
    format!(
        "{stage} deadline exceeded after {}s; returning partial/fallback search results",
        deadline.as_secs()
    )
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

fn attach_search_headers(response: &mut Response, search_mode: SearchMode, warnings: &[String]) {
    response.headers_mut().insert(
        "search-mode",
        HeaderValue::from_static(search_mode.as_str()),
    );
    if search_mode == SearchMode::Bm25Only {
        response
            .headers_mut()
            .insert("degraded", HeaderValue::from_static("true"));
    }
    if !warnings.is_empty()
        && let Ok(value) = HeaderValue::from_str(&warnings.join(" | "))
    {
        response
            .headers_mut()
            .insert(REST_SEARCH_WARNING_HEADER, value);
    }
}

async fn run_rest_bm25_search_bounded(
    state: &ApiState,
    query: String,
    route: RouteDecision,
    scope: ProjectSearchScope,
    search_options: SearchOptions,
    top_k: usize,
    deadline: Duration,
) -> anyhow::Result<Option<crate::search::Result<Vec<SearchResult>>>> {
    state
        .run_read_anyhow_bounded(
            move |db| {
                Ok(search_bm25_only_with_options(
                    db,
                    &query,
                    route,
                    &scope,
                    search_options,
                    top_k,
                ))
            },
            deadline,
        )
        .await
}

async fn search_handler(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<SearchQuery>,
) -> Result<Response, ApiError> {
    let config = ConfigHandle::current();
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
        db_deadline,
    );
    let embed_snapshot = crate::embed::global_embed_status().snapshot();
    let vector_search_circuit =
        VectorSearchCircuit::from_config_and_snapshot(config.as_ref(), &embed_snapshot);
    let mut search_mode = SearchMode::Hybrid;
    let mut warnings = Vec::new();
    let mut partial = false;
    let route_elapsed;
    let mut embed_elapsed = Duration::ZERO;
    let mut db_elapsed = Duration::ZERO;
    let query_vector = if vector_search_circuit.bm25_fallback_enabled && vector_search_circuit.open
    {
        search_mode = SearchMode::Bm25Only;
        warnings.push(bm25_fallback_warning_degraded(
            vector_search_circuit.failure_count,
        ));
        None
    } else {
        telemetry.set_stage("embedding");
        let embed_started = Instant::now();
        match state.embedder_factory.build().await {
            Ok(embedder) => match tokio::time::timeout(
                Duration::from_secs(vector_search_circuit.search_deadline_secs),
                embedder.embed(&[query.q.as_str()]),
            )
            .await
            {
                Ok(Ok(vectors)) => {
                    embed_elapsed = embed_started.elapsed();
                    match vectors.into_iter().next() {
                        Some(vector) => Some(vector),
                        None if vector_search_circuit.bm25_fallback_enabled => {
                            search_mode = SearchMode::Bm25Only;
                            warnings.push(bm25_fallback_warning_missing_query_vector());
                            None
                        }
                        None => {
                            return Err(ApiError::new(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "embedder returned no vector",
                            ));
                        }
                    }
                }
                Ok(Err(error)) if vector_search_circuit.bm25_fallback_enabled => {
                    embed_elapsed = embed_started.elapsed();
                    search_mode = SearchMode::Bm25Only;
                    warnings.push(bm25_fallback_warning_embed_error(
                        &crate::core::config::scrub_sensitive_text(&error.to_string()),
                    ));
                    None
                }
                Ok(Err(error)) => {
                    return Err(internal_error(error));
                }
                Err(_) if vector_search_circuit.bm25_fallback_enabled => {
                    embed_elapsed = embed_started.elapsed();
                    search_mode = SearchMode::Bm25Only;
                    partial = true;
                    warnings.push(bm25_fallback_warning_timeout(
                        vector_search_circuit.search_deadline_secs,
                    ));
                    None
                }
                Err(_) => {
                    return Err(ApiError::new(
                        StatusCode::GATEWAY_TIMEOUT,
                        "embedding deadline exceeded",
                    ));
                }
            },
            Err(error) if vector_search_circuit.bm25_fallback_enabled => {
                embed_elapsed = embed_started.elapsed();
                search_mode = SearchMode::Bm25Only;
                warnings.push(bm25_fallback_warning_embed_error(
                    &crate::core::config::scrub_sensitive_text(&error.to_string()),
                ));
                None
            }
            Err(error) => {
                return Err(internal_error(error));
            }
        }
    };

    telemetry.set_stage("routing");
    let route_started = Instant::now();
    let fallback_route = RouteDecision {
        wing: query.wing.clone(),
        room: query.room.clone(),
        confidence: if query.wing.is_some() || query.room.is_some() {
            1.0
        } else {
            0.0
        },
        reason: "bounded REST fallback: route resolution timed out".to_string(),
    };
    let route_query = query.q.clone();
    let route_wing = query.wing.clone();
    let route_room = query.room.clone();
    let route = match state
        .run_read_anyhow_bounded(
            move |db| {
                Ok(resolve_route(
                    db,
                    &route_query,
                    route_wing.as_deref(),
                    route_room.as_deref(),
                ))
            },
            db_deadline,
        )
        .await
    {
        Ok(Some(Ok(route))) => {
            route_elapsed = route_started.elapsed();
            route
        }
        Ok(Some(Err(error))) => return Err(internal_error(error)),
        Ok(None) => {
            route_elapsed = route_started.elapsed();
            partial = true;
            warnings.push(rest_search_timeout_warning("route resolution", db_deadline));
            fallback_route
        }
        Err(error) => return Err(internal_error(error)),
    };

    let results = if let Some(query_vector) = query_vector {
        telemetry.set_stage("hybrid_db");
        let search_started = Instant::now();
        let hybrid_query = query.q.clone();
        let hybrid_route = route.clone();
        let hybrid_scope = scope.clone();
        let hybrid_options = search_options.clone();
        match state
            .run_read_anyhow_bounded(
                move |db| {
                    Ok(search_with_vector_and_scope_options(
                        db,
                        &hybrid_query,
                        &query_vector,
                        hybrid_route,
                        &hybrid_scope,
                        hybrid_options,
                        top_k,
                    ))
                },
                db_deadline,
            )
            .await
        {
            Ok(Some(Ok(results))) => {
                db_elapsed += search_started.elapsed();
                results
            }
            Ok(Some(Err(crate::search::SearchError::VectorDimensionMismatch {
                current_dim,
                new_dim,
            }))) if vector_search_circuit.bm25_fallback_enabled => {
                db_elapsed += search_started.elapsed();
                search_mode = SearchMode::Bm25Only;
                warnings.push(bm25_fallback_warning_dimension_mismatch(
                    new_dim,
                    current_dim,
                ));
                telemetry.set_stage("bm25_fallback_db");
                let bm25_started = Instant::now();
                match run_rest_bm25_search_bounded(
                    &state,
                    query.q.clone(),
                    route,
                    scope,
                    search_options,
                    top_k,
                    db_deadline,
                )
                .await
                {
                    Ok(Some(Ok(results))) => {
                        db_elapsed += bm25_started.elapsed();
                        results
                    }
                    Ok(Some(Err(error))) => return Err(internal_error(error)),
                    Ok(None) => {
                        db_elapsed += bm25_started.elapsed();
                        partial = true;
                        warnings.push(rest_search_timeout_warning("BM25 fallback", db_deadline));
                        Vec::new()
                    }
                    Err(error) => return Err(internal_error(error)),
                }
            }
            Ok(Some(Err(error))) => return Err(internal_error(error)),
            Ok(None) if vector_search_circuit.bm25_fallback_enabled => {
                db_elapsed += search_started.elapsed();
                search_mode = SearchMode::Bm25Only;
                partial = true;
                warnings.push(rest_search_timeout_warning("hybrid search", db_deadline));
                telemetry.set_stage("bm25_fallback_db");
                let bm25_started = Instant::now();
                match run_rest_bm25_search_bounded(
                    &state,
                    query.q.clone(),
                    route,
                    scope,
                    search_options,
                    top_k,
                    db_deadline,
                )
                .await
                {
                    Ok(Some(Ok(results))) => {
                        db_elapsed += bm25_started.elapsed();
                        results
                    }
                    Ok(Some(Err(error))) => return Err(internal_error(error)),
                    Ok(None) => {
                        db_elapsed += bm25_started.elapsed();
                        warnings.push(rest_search_timeout_warning("BM25 fallback", db_deadline));
                        Vec::new()
                    }
                    Err(error) => return Err(internal_error(error)),
                }
            }
            Ok(None) => {
                db_elapsed += search_started.elapsed();
                return Err(ApiError::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    rest_search_timeout_warning("hybrid search", db_deadline),
                ));
            }
            Err(error) => return Err(internal_error(error)),
        }
    } else {
        telemetry.set_stage("bm25_db");
        let bm25_started = Instant::now();
        match run_rest_bm25_search_bounded(
            &state,
            query.q.clone(),
            route,
            scope,
            search_options,
            top_k,
            db_deadline,
        )
        .await
        {
            Ok(Some(Ok(results))) => {
                db_elapsed += bm25_started.elapsed();
                results
            }
            Ok(Some(Err(error))) => return Err(internal_error(error)),
            Ok(None) => {
                db_elapsed += bm25_started.elapsed();
                partial = true;
                warnings.push(rest_search_timeout_warning("BM25 search", db_deadline));
                Vec::new()
            }
            Err(error) => return Err(internal_error(error)),
        }
    };
    telemetry.set_stage("rerank");
    let rerank_started = Instant::now();
    let rerank_outcome = maybe_rerank_search_results(&query.q, results).await;
    let rerank_elapsed = rerank_started.elapsed();
    warnings.extend(rerank_outcome.warnings);
    let results = rerank_outcome.results;

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
        partial,
    });

    let mut response = Json(
        results
            .into_iter()
            .map(|result| SearchResultDto::from_result(result, search_mode, &warnings))
            .collect::<Vec<_>>(),
    )
    .into_response();
    attach_search_headers(&mut response, search_mode, &warnings);
    Ok(response)
}

async fn ingest_handler(
    State(state): State<ApiState>,
    Json(request): Json<IngestRequest>,
) -> Result<impl IntoResponse, ApiError> {
    validate_ingest_request(&request)?;
    if global_embed_status().should_block_writes() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "mempal embed backend degraded; writes are paused until recovery. Read operations remain available.",
        ));
    }
    let response = state.write_queue().enqueue(request).await?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn process_ingest_request(
    db_path: PathBuf,
    embedder_factory: Arc<dyn crate::embed::EmbedderFactory>,
    request: IngestRequest,
) -> Result<IngestResponse, ApiError> {
    if global_embed_status().should_block_writes() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "mempal embed backend degraded; writes are paused until recovery. Read operations remain available.",
        ));
    }
    let embedder: Box<dyn crate::embed::Embedder> =
        embedder_factory.build().await.map_err(internal_error)?;
    let db = Database::open(&db_path).map_err(internal_error)?;
    let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
    let validated = validate_ingest_request(&request)?;
    let project_id = resolve_project_id(request.project_id.as_deref(), config.as_ref(), None)
        .map_err(internal_error)?;
    let raw_turn = is_raw_turn(&request.wing, request.room.as_deref(), &config.turns);
    if raw_turn && !should_store_raw_turns(&config.turns.storage_mode) {
        return Ok(IngestResponse {
            drawer_id: String::new(),
            drawer_ids: Vec::new(),
            chunk_count: 0,
            dropped: false,
            superseded_drawer_id: None,
            fact_check_warnings: Vec::new(),
        });
    }
    let drawer_importance =
        raw_turn_importance(&request.wing, request.room.as_deref(), &config.turns)
            .unwrap_or_else(|| request.importance.unwrap_or(0));

    // Chunk the content using the token-aware chunker (issue #57).
    let chunks =
        crate::ingest::prepare_chunks(&request.content, &config.chunker, embedder.as_ref());
    if chunks.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "content produced no chunks",
        ));
    }

    let scrubbed_replace_text = request
        .replace_text
        .as_deref()
        .map(|text| config.scrub_content_with_compiled(text, compiled_privacy.as_ref()));
    let replacement_target = db
        .resolve_replacement_target(
            request.supersedes.as_deref(),
            scrubbed_replace_text.as_deref(),
            &request.wing,
            request.room.as_deref(),
            project_id.as_deref(),
        )
        .map_err(replacement_error)?;
    let superseded_drawer_id = replacement_target
        .as_ref()
        .map(|summary| summary.id.clone());
    let superseded_drawer_id_ref = superseded_drawer_id.as_deref();
    let mut superseded_response_id: Option<String> = None;

    let mut accepted_chunks: Vec<(usize, &String, String, bool)> = Vec::with_capacity(chunks.len());
    let mut fact_check_warnings = Vec::new();
    for (chunk_idx, chunk) in chunks.iter().enumerate() {
        if let Some(existing_id) = exact_duplicate_drawer_id(
            &db,
            chunk,
            &request.wing,
            request.room.as_deref(),
            project_id.as_deref(),
            superseded_drawer_id_ref,
        )? {
            accepted_chunks.push((chunk_idx, chunk, existing_id, true));
            continue;
        }

        let preferred_drawer_id = build_bootstrap_evidence_drawer_id(
            &request.wing,
            request.room.as_deref(),
            chunk,
            &validated.source_type,
        );
        let drawer_id = db
            .resolve_available_drawer_id(&preferred_drawer_id)
            .map_err(internal_error)?;
        if !raw_turn
            && let Some(outcome) = evaluate_fact_check_gate(
                &drawer_id,
                chunk,
                &db,
                project_id.as_deref(),
                &config.ingest_gating.fact_check,
                validated.confidence,
            )
            .map_err(internal_error)?
        {
            fact_check_warnings.extend(outcome.warnings);
            if outcome.decision.is_rejected() {
                continue;
            }
        }
        accepted_chunks.push((chunk_idx, chunk, drawer_id, false));
    }

    if accepted_chunks.is_empty() {
        return Ok(IngestResponse {
            drawer_id: String::new(),
            drawer_ids: Vec::new(),
            chunk_count: 0,
            dropped: true,
            superseded_drawer_id: None,
            fact_check_warnings,
        });
    }

    if accepted_chunks.iter().all(|(_, _, _, exists)| *exists) {
        let drawer_ids = accepted_chunks
            .iter()
            .map(|(_, _, drawer_id, _)| drawer_id.clone())
            .collect::<Vec<_>>();
        if let Some(old_id) = superseded_drawer_id.as_deref() {
            let replacement_id = drawer_ids.first().map(String::as_str).unwrap_or("existing");
            supersede_drawer_for_ingest(&db, old_id, replacement_id)?;
            superseded_response_id = Some(old_id.to_string());
        }
        let primary_drawer_id = drawer_ids.first().cloned().unwrap_or_default();
        return Ok(IngestResponse {
            drawer_id: primary_drawer_id,
            drawer_ids,
            chunk_count: accepted_chunks.len(),
            dropped: false,
            superseded_drawer_id: superseded_response_id,
            fact_check_warnings,
        });
    }

    // Embed all accepted chunks in one batch call.
    let chunk_refs: Vec<&str> = accepted_chunks
        .iter()
        .map(|(_, chunk, _, _)| chunk.as_str())
        .collect();
    let vectors = embedder.embed(&chunk_refs).await.map_err(internal_error)?;
    if vectors.len() != accepted_chunks.len() {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "embedder returned wrong number of vectors",
        ));
    }

    // Insert each chunk as a separate drawer.
    let mut drawer_ids: Vec<String> = Vec::with_capacity(accepted_chunks.len());
    for ((chunk_idx, chunk, drawer_id, exact_duplicate), vector) in
        accepted_chunks.iter().zip(vectors.iter())
    {
        let drawer_exists = *exact_duplicate
            || db
                .drawer_exists(drawer_id.as_str())
                .map_err(internal_error)?;

        if !drawer_exists {
            let source_file = source_file_or_synthetic(drawer_id, request.source.as_deref());
            let base = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
                id: drawer_id.clone(),
                content: chunk.to_string(),
                wing: request.wing.clone(),
                room: request.room.clone(),
                source_file: Some(source_file),
                source_type: validated.source_type,
                added_at: iso_timestamp(),
                chunk_index: Some(*chunk_idx as i64),
                importance: drawer_importance,
            });
            let drawer = Drawer {
                confidence: validated.confidence,
                normalize_version: CURRENT_NORMALIZE_VERSION,
                memory_kind: validated.typed_memory_kind.unwrap_or(base.memory_kind),
                domain: validated.typed_domain.unwrap_or(base.domain),
                field: validated
                    .typed_field
                    .clone()
                    .unwrap_or_else(|| anchor::DEFAULT_FIELD.to_string()),
                status: validated.typed_status.clone().or(base.status),
                tier: validated.typed_tier.clone().or(base.tier),
                is_pinned: validated.typed_is_pinned,
                statement: validated.typed_statement.clone().or(base.statement),
                supporting_refs: if validated.typed_supporting_refs.is_empty() {
                    base.supporting_refs
                } else {
                    validated.typed_supporting_refs.clone()
                },
                counterexample_refs: if validated.typed_counterexample_refs.is_empty() {
                    base.counterexample_refs
                } else {
                    validated.typed_counterexample_refs.clone()
                },
                teaching_refs: if validated.typed_teaching_refs.is_empty() {
                    base.teaching_refs
                } else {
                    validated.typed_teaching_refs.clone()
                },
                verification_refs: if validated.typed_verification_refs.is_empty() {
                    base.verification_refs
                } else {
                    validated.typed_verification_refs.clone()
                },
                ..base
            };
            let mut drawer = drawer;
            if let Some(old_id) = superseded_drawer_id.as_deref() {
                link_superseded_drawer(&mut drawer, old_id);
            }
            db.insert_drawer_with_project_validity(
                &drawer,
                project_id.as_deref(),
                None,
                request.valid_from.as_deref(),
                request.valid_until.as_deref(),
            )
            .map_err(internal_error)?;
            db.insert_vector_with_project(drawer_id, vector, project_id.as_deref())
                .map_err(internal_error)?;
        }
        drawer_ids.push(drawer_id.clone());
    }

    if let Some(old_id) = superseded_drawer_id.as_deref()
        && let Some(replacement_id) = drawer_ids.first()
    {
        supersede_drawer_for_ingest(&db, old_id, replacement_id)?;
        superseded_response_id = Some(old_id.to_string());
    }

    let primary_drawer_id = drawer_ids.first().cloned().unwrap_or_default();
    let chunk_count = drawer_ids.len();
    Ok(IngestResponse {
        drawer_id: primary_drawer_id,
        drawer_ids,
        chunk_count,
        dropped: false,
        superseded_drawer_id: superseded_response_id,
        fact_check_warnings,
    })
}

async fn pinned_facts_handler(
    State(state): State<ApiState>,
    Query(query): Query<PinnedFactsQuery>,
) -> Result<Json<Vec<PinnedFactDto>>, ApiError> {
    let config = ConfigHandle::current();
    let project_id = resolve_project_id(query.project_id.as_deref(), config.as_ref(), None)
        .map_err(internal_error)?;
    let domain_filter = if let Some(d) = query.domain.as_deref() {
        Some(parse_domain(d)?)
    } else {
        None
    };
    let limit = query.limit.unwrap_or(50).min(500);
    let budget_chars = limit * 2000;
    let db = Database::open(&state.db_path).map_err(internal_error)?;
    let drawers = db
        .get_pinned_facts(project_id.as_deref(), budget_chars)
        .map_err(internal_error)?;
    let facts = drawers
        .into_iter()
        .filter(|d| {
            let wing_ok = query.wing.as_deref().is_none_or(|w| d.wing == w);
            let room_ok = query
                .room
                .as_deref()
                .is_none_or(|r| d.room.as_deref() == Some(r));
            let domain_ok = domain_filter.is_none_or(|dom| d.domain == dom);
            wing_ok && room_ok && domain_ok
        })
        .take(limit)
        .map(|d| PinnedFactDto {
            drawer_id: d.id.clone(),
            content: d.content,
            wing: d.wing,
            room: d.room,
            source_file: d.source_file.unwrap_or(d.id),
            memory_kind: memory_kind_slug(d.memory_kind).to_string(),
            domain: domain_slug(d.domain).to_string(),
            field: d.field,
            status: d
                .status
                .as_ref()
                .map(knowledge_status_slug)
                .map(str::to_string),
            importance: d.importance,
            pin_order: d.pin_order,
            added_at: d.added_at,
        })
        .collect();
    Ok(Json(facts))
}

async fn taxonomy_handler(
    State(state): State<ApiState>,
) -> Result<Json<Vec<TaxonomyEntryDto>>, ApiError> {
    let db = Database::open(&state.db_path).map_err(internal_error)?;
    let entries = db
        .taxonomy_entries()
        .map_err(internal_error)?
        .into_iter()
        .map(TaxonomyEntryDto::from)
        .collect();
    Ok(Json(entries))
}

async fn status_handler(State(state): State<ApiState>) -> Result<Json<StatusResponse>, ApiError> {
    let config = ConfigHandle::current();
    let embed_snapshot = crate::embed::global_embed_status().snapshot();
    let vector_search_circuit =
        VectorSearchCircuit::from_config_and_snapshot(config.as_ref(), &embed_snapshot);
    let turns_config = config.turns.clone();
    let db_deadline = Duration::from_secs(config.api.search_db_deadline_secs);
    let db_snapshot = state
        .run_read_anyhow_bounded(
            move |db| {
                let drawer_count = db.drawer_count()?;
                let raw_turn_count = count_raw_turn_drawers(db, &turns_config)?;
                let taxonomy_count = db.taxonomy_count()?;
                let db_size_bytes = db.database_size_bytes()?;
                let wings = db
                    .scope_counts()?
                    .into_iter()
                    .map(|(wing, room, drawer_count)| ScopeCount {
                        wing,
                        room,
                        drawer_count,
                    })
                    .collect();
                let source_type_distribution = db
                    .source_type_counts()?
                    .into_iter()
                    .map(|(source_type, count)| SourceTypeCount {
                        source_type: source_type.to_string(),
                        count,
                    })
                    .collect();
                Ok(StatusDbSnapshot {
                    drawer_count,
                    taxonomy_count,
                    db_size_bytes,
                    wings,
                    source_type_distribution,
                    raw_turn_count,
                })
            },
            db_deadline,
        )
        .await
        .map_err(internal_error)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                rest_search_timeout_warning("status database snapshot", db_deadline),
            )
        })?;

    Ok(Json(StatusResponse {
        drawer_count: db_snapshot.drawer_count,
        taxonomy_count: db_snapshot.taxonomy_count,
        db_size_bytes: db_snapshot.db_size_bytes,
        embedding_status: current_embedding_status(&embed_snapshot).to_string(),
        search_mode: vector_search_circuit
            .vector_search_mode
            .as_str()
            .to_string(),
        embedder_circuit: vector_search_circuit.into(),
        write_queue: state.write_queue().stats(),
        feature_flags: FeatureFlags {
            typed_ingest: true,
            pinned_facts: true,
            compaction: true,
            sleep_cycle: true,
            crystallize: true,
            intelligence_modes: true,
        },
        hermes_compat_version: HERMES_COMPAT_VERSION.to_string(),
        search_decay_mode: config.search.decay.mode.to_string(),
        wings: db_snapshot.wings,
        source_type_distribution: db_snapshot.source_type_distribution,
        turn_storage: TurnStorageStatus {
            storage_mode: config.turns.storage_mode.to_string(),
            default_importance: config.turns.default_importance,
            raw_turn_count: db_snapshot.raw_turn_count,
            raw_turn_wings: config.turns.raw_turn_wings.clone(),
            raw_turn_rooms: config.turns.raw_turn_rooms.clone(),
        },
        search_telemetry: state.search_telemetry().snapshot(),
    }))
}

fn current_embedding_status(snapshot: &crate::embed::EmbedHealthSnapshot) -> &'static str {
    if snapshot.degraded {
        "degraded"
    } else if snapshot.fail_count > 0 && snapshot.last_success_at_unix_ms.is_none() {
        "unavailable"
    } else {
        "healthy"
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": {
                    "message": self.message,
                    "status": self.status.as_u16(),
                },
            })),
        )
            .into_response()
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WriteQueueStats {
    pub queued: u64,
    pub pending: u64,
    pub completed: u64,
    pub failed: u64,
}

pub(crate) struct WriteQueue {
    sender: mpsc::Sender<WriteJob>,
    stats: Arc<WriteQueueCounters>,
    accepting: Arc<AtomicBool>,
    drain_timeout: Duration,
    drained: Arc<Notify>,
}

struct WriteQueueCounters {
    queued: AtomicU64,
    pending: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
}

struct WriteJob {
    request: IngestRequest,
    respond_to: oneshot::Sender<Result<IngestResponse, ApiError>>,
}

impl WriteQueue {
    pub(super) fn spawn(
        db_path: PathBuf,
        embedder_factory: Arc<dyn crate::embed::EmbedderFactory>,
        capacity: usize,
        drain_timeout: Duration,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(capacity);
        let stats = Arc::new(WriteQueueCounters {
            queued: AtomicU64::new(0),
            pending: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        });
        let accepting = Arc::new(AtomicBool::new(true));
        let drained = Arc::new(Notify::new());
        tokio::spawn(write_worker(
            db_path,
            embedder_factory,
            receiver,
            Arc::clone(&stats),
            Arc::clone(&drained),
        ));
        Self {
            sender,
            stats,
            accepting,
            drain_timeout,
            drained,
        }
    }

    async fn enqueue(&self, request: IngestRequest) -> Result<IngestResponse, ApiError> {
        if !self.accepting.load(Ordering::SeqCst) {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "REST write queue is draining",
            ));
        }

        let (respond_to, response_rx) = oneshot::channel();
        let job = WriteJob {
            request,
            respond_to,
        };
        match self.sender.try_send(job) {
            Ok(()) => {
                self.stats.queued.fetch_add(1, Ordering::SeqCst);
                self.stats.pending.fetch_add(1, Ordering::SeqCst);
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "REST write queue is full",
                ));
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "REST write queue is closed",
                ));
            }
        }

        response_rx.await.map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "REST write queue worker stopped before completing the request",
            )
        })?
    }

    pub(super) async fn drain(&self) -> bool {
        self.accepting.store(false, Ordering::SeqCst);
        tokio::time::timeout(self.drain_timeout, async {
            loop {
                if self.stats.pending.load(Ordering::SeqCst) == 0 {
                    return;
                }
                self.drained.notified().await;
            }
        })
        .await
        .is_ok()
    }

    pub(crate) fn stats(&self) -> WriteQueueStats {
        WriteQueueStats {
            queued: self.stats.queued.load(Ordering::SeqCst),
            pending: self.stats.pending.load(Ordering::SeqCst),
            completed: self.stats.completed.load(Ordering::SeqCst),
            failed: self.stats.failed.load(Ordering::SeqCst),
        }
    }
}

async fn write_worker(
    db_path: PathBuf,
    embedder_factory: Arc<dyn crate::embed::EmbedderFactory>,
    mut receiver: mpsc::Receiver<WriteJob>,
    stats: Arc<WriteQueueCounters>,
    drained: Arc<Notify>,
) {
    while let Some(job) = receiver.recv().await {
        let recovery_content = job.request.content.clone();
        let result =
            process_ingest_request(db_path.clone(), Arc::clone(&embedder_factory), job.request)
                .await;
        match &result {
            Ok(_) => {
                stats.completed.fetch_add(1, Ordering::SeqCst);
            }
            Err(error) => {
                stats.failed.fetch_add(1, Ordering::SeqCst);
                tracing::error!(
                    error = %error.message,
                    drawer_content = %recovery_content,
                    "REST write failed; drawer content logged for manual recovery"
                );
            }
        }
        stats.pending.fetch_sub(1, Ordering::SeqCst);
        drained.notify_waiters();
        let _ = job.respond_to.send(result);
    }
}

fn internal_error(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn replacement_error(error: crate::core::db::DbError) -> ApiError {
    match error {
        crate::core::db::DbError::ReplacementTargetConflict
        | crate::core::db::DbError::SupersededDrawerNotFound { .. }
        | crate::core::db::DbError::SupersededDrawerProjectMismatch { .. }
        | crate::core::db::DbError::ReplacementTextNotFound
        | crate::core::db::DbError::ReplacementTextAmbiguous { .. } => {
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        }
        _ => internal_error(error),
    }
}

fn exact_duplicate_drawer_id(
    db: &Database,
    content: &str,
    wing: &str,
    room: Option<&str>,
    project_id: Option<&str>,
    excluded_drawer_id: Option<&str>,
) -> Result<Option<String>, ApiError> {
    Ok(db
        .find_active_drawers_by_content(content, wing, room, project_id)
        .map_err(internal_error)?
        .into_iter()
        .find(|summary| Some(summary.id.as_str()) != excluded_drawer_id)
        .map(|summary| summary.id))
}

fn supersede_drawer_for_ingest(db: &Database, old_id: &str, new_id: &str) -> Result<(), ApiError> {
    db.supersede_drawer(old_id, &format!("replaced by {new_id}"))
        .map_err(internal_error)?;
    Ok(())
}

impl SearchResultDto {
    fn from_result(value: SearchResult, search_mode: SearchMode, warnings: &[String]) -> Self {
        Self {
            drawer_id: value.drawer_id,
            content: value.content,
            wing: value.wing,
            room: value.room,
            source_file: value.source_file,
            source: value.source.as_str().to_string(),
            source_type: value.source_type.as_str().to_string(),
            confidence: value.confidence,
            similarity: value.similarity,
            route: value.route.into(),
            search_mode: search_mode.as_str().to_string(),
            memory_kind: memory_kind_slug(value.memory_kind).to_string(),
            domain: domain_slug(value.domain).to_string(),
            field: value.field,
            importance: value.importance,
            status: value
                .status
                .as_ref()
                .map(knowledge_status_slug)
                .map(str::to_string),
            tier: value
                .tier
                .as_ref()
                .map(knowledge_tier_slug)
                .map(str::to_string),
            is_pinned: value.is_pinned,
            statement: value.statement,
            warnings: warnings.to_vec(),
        }
    }
}

impl From<RouteDecision> for RouteDecisionDto {
    fn from(value: RouteDecision) -> Self {
        Self {
            wing: value.wing,
            room: value.room,
            confidence: value.confidence,
            reason: value.reason,
        }
    }
}

impl From<TaxonomyEntry> for TaxonomyEntryDto {
    fn from(value: TaxonomyEntry) -> Self {
        Self {
            wing: value.wing,
            room: value.room,
            display_name: value.display_name,
            keywords: value.keywords,
        }
    }
}
