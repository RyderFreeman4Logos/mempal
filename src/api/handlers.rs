use std::{
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use crate::core::{
    anchor,
    config::{ConfigHandle, scrub_runtime_diagnostic_text},
    db::{Database, DbError, db_error_is_sqlite_lock},
    db_admission::DbAdmissionError,
    project::resolve_project_id,
    queue::{QueueStats, failure_headline_count, queue_stats_readonly},
    remote_calls::{
        RemoteCallService, endpoint_policy_display_label, endpoint_policy_global_runtime_error,
        endpoint_policy_runtime_error,
    },
    strata::{count_raw_turn_drawers, is_raw_turn, raw_turn_importance, should_store_raw_turns},
    types::{
        BootstrapEvidenceArgs, Drawer, KnowledgeStatus, KnowledgeTier, MemoryDomain, MemoryKind,
        SourceType, TaxonomyEntry, default_confidence,
    },
    utils::{
        build_bootstrap_evidence_drawer_id, iso_timestamp, link_superseded_drawer,
        source_file_or_synthetic,
    },
};
use crate::embed::global_embed_status;
use crate::ingest::gating::evaluate_fact_check_gate;
use crate::ingest::normalize::CURRENT_NORMALIZE_VERSION;
use crate::observability::{
    OperationTelemetryRecord, OperationTelemetrySource, OperationTelemetrySpan,
};
use crate::search::VectorSearchCircuit;
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Query, State, rejection::JsonRejection},
    http::{
        HeaderMap, HeaderValue, Method, Request, StatusCode, header::CONTENT_LENGTH,
        header::CONTENT_TYPE,
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Notify, mpsc, oneshot};
use tower_http::cors::{AllowOrigin, CorsLayer};

use super::state::{ApiState, SearchTelemetrySnapshot};

pub const DEFAULT_REST_ADDR: &str = "127.0.0.1:3080";
const HERMES_COMPAT_VERSION: &str = "mempal-hermes-compat/1";
const STATUS_DB_SNAPSHOT_DEADLINE: Duration = Duration::from_secs(1);
const REST_WRITE_RESTART_HINT: &str = "Restart the mempal daemon after upgrading so REST writes use a binary that supports this palace.db schema.";
const REST_STALE_DAEMON_RESTART_HINT: &str =
    "Run `mempal daemon restart`, then retry the write once.";
const REST_WRITE_DATABASE_BUSY_HINT: &str = "SQLite is temporarily locked by another writer; retry after the current write or maintenance job releases palace.db.";
pub const MAX_REST_INGEST_BODY_BYTES: usize =
    crate::ingest::admission::MAX_INGEST_REQUEST_BYTES + (2 * 1024 * 1024);
static REST_WRITE_REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    serve_with_optional_mcp(listener, state, None, shutdown).await
}

pub async fn serve_with_mcp<F>(
    listener: tokio::net::TcpListener,
    state: ApiState,
    server: crate::mcp::MempalMcpServer,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    serve_with_optional_mcp(listener, state, Some(server), shutdown).await
}

pub async fn serve_with_optional_mcp<F>(
    listener: tokio::net::TcpListener,
    state: ApiState,
    server: Option<crate::mcp::MempalMcpServer>,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let drain_state = state.clone();
    let bound_addr = listener.local_addr()?;
    let app = match server {
        Some(server) => router_with_mcp_at(state, server, bound_addr),
        None => router(state),
    };
    axum::serve(listener, app)
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
    let telemetry_state = state.clone();
    Router::new()
        .route("/api/search", get(super::search::search_handler))
        .route(
            "/api/ingest",
            post(ingest_handler).layer(DefaultBodyLimit::max(MAX_REST_INGEST_BODY_BYTES)),
        )
        .route("/api/taxonomy", get(taxonomy_handler))
        .route("/api/status", get(status_handler))
        .route("/api/pinned_facts", get(pinned_facts_handler))
        .merge(super::durable::routes())
        .merge(super::hermes_compat::routes())
        .route_layer(middleware::from_fn_with_state(
            telemetry_state,
            rest_operation_telemetry,
        ))
        .with_state(state)
        .layer(cors_layer())
}

pub fn router_with_mcp_at(
    state: ApiState,
    server: crate::mcp::MempalMcpServer,
    bound_addr: SocketAddr,
) -> Router {
    router(state).nest_service("/mcp", super::mcp::service(server, Some(bound_addr)))
}

async fn rest_operation_telemetry(
    State(state): State<ApiState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let operation = format!("{} {}", request.method().as_str(), request.uri().path());
    let span = OperationTelemetrySpan::start(
        state.db_path.clone(),
        OperationTelemetryRecord::new(OperationTelemetrySource::Rest, operation, "rest.request"),
    );
    let response = next.run(request).await;
    let status = response.status();
    if status.is_client_error() {
        span.finish_error_class("http_4xx");
    } else if status.is_server_error() {
        span.finish_error_class("http_5xx");
    } else {
        span.finish_success();
    }
    response
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

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct IngestRequest {
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
    opposes_refs: Option<Vec<String>>,
    supersedes_refs: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
struct IngestResponse {
    drawer_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    drawer_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    created_drawer_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    cleanup_drawer_ids: Vec<String>,
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
    value.trim().parse().map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "invalid memory_kind: {}; expected {}",
                value.trim(),
                MemoryKind::supported_slugs()
            ),
        )
    })
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

pub(super) fn memory_kind_slug(kind: MemoryKind) -> &'static str {
    kind.as_str()
}

pub(super) fn domain_slug(domain: MemoryDomain) -> &'static str {
    match domain {
        MemoryDomain::Project => "project",
        MemoryDomain::User => "user",
        MemoryDomain::Agent => "agent",
        MemoryDomain::Skill => "skill",
        MemoryDomain::Global => "global",
    }
}

pub(super) fn knowledge_status_slug(status: &KnowledgeStatus) -> &'static str {
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

pub(super) fn knowledge_tier_slug(tier: &KnowledgeTier) -> &'static str {
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

fn refs_are_empty(groups: &[&[String]]) -> bool {
    groups.iter().all(|refs| refs.is_empty())
}

fn validate_active_or_canonical_status(
    label: &str,
    status: Option<&KnowledgeStatus>,
) -> Result<(), ApiError> {
    if status
        .is_some_and(|value| !matches!(value, KnowledgeStatus::Active | KnowledgeStatus::Canonical))
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("{label} status must be active or canonical"),
        ));
    }
    Ok(())
}

fn validate_non_knowledge_metadata(
    label: &str,
    status: Option<&KnowledgeStatus>,
    tier: &Option<KnowledgeTier>,
    ref_groups: &[&[String]],
) -> Result<(), ApiError> {
    if tier.is_some() || !refs_are_empty(ref_groups) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("{label} drawer does not allow knowledge-only tier/ref fields"),
        ));
    }
    validate_active_or_canonical_status(label, status)
}

fn validate_typed_metadata_contract(
    memory_kind: MemoryKind,
    status: Option<&KnowledgeStatus>,
    tier: &Option<KnowledgeTier>,
    ref_groups: &[&[String]],
) -> Result<(), ApiError> {
    if memory_kind.is_knowledge() {
        return Ok(());
    }
    if memory_kind.is_raw_evidence() {
        return validate_non_knowledge_metadata("evidence", status, tier, ref_groups);
    }
    validate_non_knowledge_metadata("typed record", status, tier, ref_groups)
}

pub(super) struct ValidatedIngestRequest {
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

pub(super) fn validate_ingest_request(
    request: &IngestRequest,
) -> Result<ValidatedIngestRequest, ApiError> {
    crate::ingest::admission::validate_ingest_request_bytes(&request.content)
        .map_err(ApiError::payload_too_large)?;
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
    let typed_supporting_refs = normalize_refs(request.supporting_refs.as_deref());
    let typed_counterexample_refs = normalize_refs(request.counterexample_refs.as_deref());
    let typed_teaching_refs = normalize_refs(request.teaching_refs.as_deref());
    let typed_verification_refs = normalize_refs(request.verification_refs.as_deref());
    let typed_opposes_refs = normalize_refs(request.opposes_refs.as_deref());
    let typed_supersedes_refs = normalize_refs(request.supersedes_refs.as_deref());
    validate_typed_metadata_contract(
        typed_memory_kind.unwrap_or(MemoryKind::Evidence),
        typed_status.as_ref(),
        &typed_tier,
        &[
            &typed_supporting_refs,
            &typed_counterexample_refs,
            &typed_teaching_refs,
            &typed_verification_refs,
            &typed_opposes_refs,
            &typed_supersedes_refs,
        ],
    )?;

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
        typed_supporting_refs,
        typed_counterexample_refs,
        typed_teaching_refs,
        typed_verification_refs,
    })
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    diagnostic: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    drawer_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    taxonomy_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    db_size_bytes: Option<i64>,
    embedding_status: String,
    embedding_endpoints: Vec<ApiEmbeddingEndpointStatus>,
    embed_status: ApiEmbedStatus,
    embedder_cache: crate::embed::SharedEmbedderRuntimeSnapshot,
    search_mode: String,
    /// Hot-reloaded end-to-end and stage search deadline policy (seconds).
    search_policy: SearchPolicyStatus,
    embedder_circuit: EmbedderCircuitStatus,
    queue_stats: ApiQueueStats,
    hook_admission: crate::hook_diagnostics::HookAdmissionStats,
    resource_usage: super::resource_status::ResourceUsageStatus,
    io_burst: crate::observability::IoBurstSnapshot,
    write_queue: WriteQueueStats,
    feature_flags: FeatureFlags,
    hermes_compat_version: String,
    search_decay_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    wings: Option<Vec<ScopeCount>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_type_distribution: Option<Vec<SourceTypeCount>>,
    turn_storage: TurnStorageStatus,
    ingest_worker_backoff: crate::observability::IngestWorkerBackoffSnapshot,
    vector_scan: crate::observability::VectorScanSnapshot,
    search_telemetry: SearchTelemetrySnapshot,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    status_warnings: Vec<String>,
}

/// Operator-visible search deadline policy from the active hot-reloaded config.
#[derive(Debug, Serialize)]
struct SearchPolicyStatus {
    /// End-to-end monotonic query deadline shared across all search stages.
    query_deadline_secs: u64,
    /// Stage cap for DB work (still limited by remaining E2E budget).
    db_deadline_secs: u64,
    /// Stage cap for query embedding (still limited by remaining E2E budget).
    embed_deadline_secs: u64,
    /// Stage cap for reranker HTTP (still limited by remaining E2E budget).
    reranker_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
struct ApiEmbeddingEndpointStatus {
    id: String,
    backend: String,
    base_url: String,
    model: String,
    priority: i32,
    retry_interval_secs: u64,
    request_timeout_secs: u64,
    max_concurrent: usize,
    dimensions: usize,
    cooldown_remaining_secs: Option<u64>,
    cooldown_until_unix_ms: Option<u64>,
    last_failure_at_unix_ms: Option<u64>,
    last_success_at_unix_ms: Option<u64>,
    last_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ApiEmbedStatus {
    backend: String,
    base_url: Option<String>,
    model: Option<String>,
    endpoints: Vec<ApiEmbeddingEndpointStatus>,
    max_concurrent: usize,
    pending_count: u64,
    claimed_count: u64,
    failed_count: u64,
    degraded: bool,
    block_writes_when_degraded: bool,
    write_refused: bool,
    fail_count: u64,
    failure_count: u64,
    last_error: Option<String>,
    last_success_at_unix_ms: Option<u64>,
}

#[derive(Debug, Default, Serialize)]
struct ApiQueueStats {
    pending: u64,
    claimed: u64,
    active_payload_bytes: u64,
    active_ingest_payload_bytes: u64,
    ingest_payload_limit_bytes: u64,
    rejected_oversize: u64,
    failed: u64,
    failed_retryable: u64,
    failed_terminal: u64,
    failed_retryable_embed: u64,
    failed_retryable_llm: u64,
}

impl From<&QueueStats> for ApiQueueStats {
    fn from(value: &QueueStats) -> Self {
        Self {
            pending: value.pending,
            claimed: value.claimed,
            active_payload_bytes: value.active_payload_bytes,
            active_ingest_payload_bytes: value.active_ingest_payload_bytes,
            ingest_payload_limit_bytes: value.ingest_payload_limit_bytes,
            rejected_oversize: value.rejected_oversize,
            failed: value.failed,
            failed_retryable: value.failed_retryable,
            failed_terminal: value.failed_terminal,
            failed_retryable_embed: value.failed_retryable_embed,
            failed_retryable_llm: value.failed_retryable_llm,
        }
    }
}

fn sanitize_api_runtime_error(error: Option<String>) -> Option<String> {
    error.map(|message| scrub_runtime_diagnostic_text(&message))
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
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_turn_count: Option<i64>,
    raw_turn_wings: Vec<String>,
    raw_turn_rooms: Vec<String>,
}

#[derive(Default)]
struct StatusDbSnapshot {
    drawer_count: i64,
    taxonomy_count: i64,
    db_size_bytes: i64,
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

fn rest_search_timeout_warning(stage: &str, deadline: Duration) -> String {
    format!(
        "{stage} deadline exceeded after {}s; returning partial/fallback search results",
        deadline.as_secs()
    )
}

async fn ingest_handler(
    State(state): State<ApiState>,
    headers: HeaderMap,
    request: Result<Json<IngestRequest>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Json(request) = match request {
        Ok(request) => request,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            let request_bytes = headers
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(MAX_REST_INGEST_BODY_BYTES as u64 + 1);
            state
                .write_queue()
                .record_oversize_rejection(
                    "rest_request_body",
                    request_bytes,
                    MAX_REST_INGEST_BODY_BYTES as u64,
                )
                .await;
            return Err(ApiError::rest_body_too_large(request_bytes));
        }
        Err(rejection) => {
            return Err(ApiError::new(rejection.status(), rejection.body_text()));
        }
    };
    if let Err(error) = validate_ingest_request(&request) {
        if error.kind == "payload_too_large" {
            state
                .write_queue()
                .record_oversize_rejection(
                    "rest_request",
                    u64::try_from(request.content.len()).unwrap_or(u64::MAX),
                    crate::ingest::admission::MAX_INGEST_REQUEST_BYTES as u64,
                )
                .await;
        }
        return Err(error);
    }
    validate_rest_write_runtime(&state.db_path)?;
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
    let db = open_rest_write_database(&db_path)?;
    let embedder: Box<dyn crate::embed::Embedder> =
        embedder_factory.build().await.map_err(internal_error)?;
    let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
    let validated = validate_ingest_request(&request)?;
    let project_id = resolve_project_id(request.project_id.as_deref(), config.as_ref(), None)
        .map_err(internal_error)?;
    let raw_turn = is_raw_turn(
        &request.wing,
        request.room.as_deref(),
        validated.typed_memory_kind.as_ref(),
        &config.turns,
    );
    if raw_turn && !should_store_raw_turns(&config.turns.storage_mode) {
        return Ok(IngestResponse {
            drawer_id: String::new(),
            drawer_ids: Vec::new(),
            created_drawer_ids: Vec::new(),
            cleanup_drawer_ids: Vec::new(),
            chunk_count: 0,
            dropped: false,
            superseded_drawer_id: None,
            fact_check_warnings: Vec::new(),
        });
    }
    let drawer_importance = raw_turn_importance(
        &request.wing,
        request.room.as_deref(),
        validated.typed_memory_kind.as_ref(),
        &config.turns,
    )
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
            .map_err(db_error_to_write_api_error)?;
        if !raw_turn
            && let Some(outcome) = evaluate_fact_check_gate(
                &drawer_id,
                chunk,
                &db,
                None,
                project_id.as_deref(),
                &config.ingest_gating.fact_check,
                validated.confidence,
            )
            .map_err(db_error_to_write_api_error)?
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
            created_drawer_ids: Vec::new(),
            cleanup_drawer_ids: Vec::new(),
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
            cleanup_drawer_ids: Vec::new(),
            created_drawer_ids: Vec::new(),
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
    let mut newly_created_drawer_ids: Vec<String> = Vec::new();
    for ((chunk_idx, chunk, drawer_id, exact_duplicate), vector) in
        accepted_chunks.iter().zip(vectors.iter())
    {
        let drawer_exists = *exact_duplicate
            || db
                .drawer_exists(drawer_id.as_str())
                .map_err(db_error_to_write_api_error)?;

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
            .map_err(db_error_to_write_api_error)?;
            db.insert_vector_with_project(drawer_id, vector, project_id.as_deref())
                .map_err(db_error_to_write_api_error)?;
            newly_created_drawer_ids.push(drawer_id.clone());
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
        cleanup_drawer_ids: newly_created_drawer_ids.clone(),
        created_drawer_ids: newly_created_drawer_ids,
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
    let db = Database::open(&state.db_path).map_err(db_error_to_api_error)?;
    let drawers = db
        .get_pinned_facts(project_id.as_deref(), budget_chars)
        .map_err(db_error_to_api_error)?;
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
    let db = Database::open(&state.db_path).map_err(db_error_to_api_error)?;
    let entries = db
        .taxonomy_entries()
        .map_err(db_error_to_api_error)?
        .into_iter()
        .map(TaxonomyEntryDto::from)
        .collect();
    Ok(Json(entries))
}

#[derive(Debug, Default, Deserialize)]
struct StatusQuery {
    #[serde(default)]
    diagnostic: bool,
}

/// Return a cheap bounded health/config snapshot by default.
///
/// `diagnostic=true` opts into the DB-wide snapshot fields that can scan large
/// palace tables or vector metadata on production-sized databases.
async fn status_handler(
    State(state): State<ApiState>,
    Query(query): Query<StatusQuery>,
) -> Result<Json<StatusResponse>, ApiError> {
    let config = ConfigHandle::current();
    let daemon_embed_config = config.daemon_embedder_config();
    let embed_snapshot = crate::embed::global_embed_status().snapshot();
    let endpoint_runtime = crate::embed::global_embed_status()
        .endpoint_runtime_snapshots()
        .into_iter()
        .map(|snapshot| (snapshot.id.clone(), snapshot))
        .collect::<std::collections::BTreeMap<_, _>>();
    let vector_search_circuit =
        VectorSearchCircuit::from_config_and_snapshot(config.as_ref(), &embed_snapshot);
    let db_deadline = STATUS_DB_SNAPSHOT_DEADLINE;
    let search_telemetry = state.search_telemetry().snapshot();
    let (db_snapshot, status_warnings) = if query.diagnostic {
        let turns_config = config.turns.clone();
        match state
            .run_read_anyhow_bounded(
                move |db| {
                    let drawer_count = db.drawer_count()?;
                    let raw_turn_count = count_raw_turn_drawers(db, &turns_config)?;
                    let taxonomy_count = db.taxonomy_count()?;
                    let db_size_bytes =
                        i64::try_from(db.database_size_bytes()?).unwrap_or(i64::MAX);
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
        {
            Ok(Some(snapshot)) => (snapshot, Vec::new()),
            Ok(None) => (
                StatusDbSnapshot::default(),
                vec![rest_search_timeout_warning(
                    "status database snapshot",
                    db_deadline,
                )],
            ),
            Err(error) => (
                StatusDbSnapshot::default(),
                vec![status_database_snapshot_error_warning(&error)],
            ),
        }
    } else {
        (StatusDbSnapshot::default(), Vec::new())
    };
    let mut status_warnings = status_warnings;
    if current_executable_deleted() {
        status_warnings.push(
            "daemon binary has been deleted or replaced; run `mempal daemon restart` after upgrade"
                .to_string(),
        );
    }
    let resource_usage = tokio::task::spawn_blocking({
        let db_path = state.db_path.clone();
        let snapshot = state.async_db_resource_snapshot();
        move || super::resource_status::build_resource_usage(&db_path, snapshot)
    })
    .await
    .unwrap_or_else(|err| {
        tracing::warn!(?err, "resource status spawn_blocking failed");
        super::resource_status::build_resource_usage_degraded()
    });
    let embed_endpoints = daemon_embed_config
        .embed
        .effective_endpoints()
        .unwrap_or_default();
    let queue_stats = queue_stats_readonly(&state.db_path).ok();
    let queue_stats_report = queue_stats
        .as_ref()
        .map(ApiQueueStats::from)
        .unwrap_or_default();
    let embed_failure_headline = queue_stats
        .as_ref()
        .map(|stats| failure_headline_count(embed_snapshot.fail_count, stats))
        .unwrap_or(embed_snapshot.fail_count);
    let block_writes_when_degraded = daemon_embed_config
        .embed
        .degradation
        .block_writes_when_degraded;
    let write_refused = embed_snapshot.degraded && block_writes_when_degraded;
    let embedding_endpoints = embed_endpoints
        .iter()
        .map(|endpoint| {
            let runtime = endpoint_runtime.get(&endpoint.id);
            let last_error = sanitize_api_runtime_error(endpoint_policy_runtime_error(
                &daemon_embed_config.privacy.remote_calls,
                RemoteCallService::Embedding,
                &endpoint.base_url,
                runtime.and_then(|state| state.last_error.clone()),
            ));
            ApiEmbeddingEndpointStatus {
                id: endpoint.id.clone(),
                backend: endpoint.backend.clone(),
                base_url: endpoint_policy_display_label(
                    &daemon_embed_config.privacy.remote_calls,
                    RemoteCallService::Embedding,
                    &endpoint.base_url,
                ),
                model: endpoint.model.clone(),
                priority: endpoint.priority,
                retry_interval_secs: endpoint.retry_interval_secs,
                request_timeout_secs: endpoint.request_timeout_secs,
                max_concurrent: endpoint.max_concurrent,
                dimensions: endpoint.dimensions,
                cooldown_remaining_secs: runtime.and_then(|state| state.cooldown_remaining_secs),
                cooldown_until_unix_ms: runtime.and_then(|state| state.cooldown_until_unix_ms),
                last_failure_at_unix_ms: runtime.and_then(|state| state.last_failure_at_unix_ms),
                last_success_at_unix_ms: runtime.and_then(|state| state.last_success_at_unix_ms),
                last_error,
            }
        })
        .collect::<Vec<_>>();
    let embed_status = ApiEmbedStatus {
        backend: daemon_embed_config.embed.backend.clone(),
        base_url: daemon_embed_config
            .embed
            .resolved_openai_base_url()
            .map(|base_url| {
                endpoint_policy_display_label(
                    &daemon_embed_config.privacy.remote_calls,
                    RemoteCallService::Embedding,
                    base_url,
                )
            }),
        model: daemon_embed_config.embed.effective_model_summary(),
        endpoints: embedding_endpoints.clone(),
        max_concurrent: daemon_embed_config.embed.pool_capacity(),
        pending_count: queue_stats_report.pending,
        claimed_count: queue_stats_report.claimed,
        failed_count: queue_stats_report.failed,
        degraded: embed_snapshot.degraded,
        block_writes_when_degraded,
        write_refused,
        fail_count: embed_failure_headline,
        failure_count: embed_failure_headline,
        last_error: sanitize_api_runtime_error(endpoint_policy_global_runtime_error(
            &daemon_embed_config.privacy.remote_calls,
            RemoteCallService::Embedding,
            embed_endpoints
                .iter()
                .map(|endpoint| endpoint.base_url.as_str()),
            embed_snapshot.last_error.clone(),
        )),
        last_success_at_unix_ms: embed_snapshot.last_success_at_unix_ms,
    };

    let (
        drawer_count,
        taxonomy_count,
        db_size_bytes,
        wings,
        source_type_distribution,
        raw_turn_count,
    ) = if query.diagnostic {
        (
            Some(db_snapshot.drawer_count),
            Some(db_snapshot.taxonomy_count),
            Some(db_snapshot.db_size_bytes),
            Some(db_snapshot.wings),
            Some(db_snapshot.source_type_distribution),
            Some(db_snapshot.raw_turn_count),
        )
    } else {
        (None, None, None, None, None, None)
    };

    Ok(Json(StatusResponse {
        diagnostic: query.diagnostic,
        drawer_count,
        taxonomy_count,
        db_size_bytes,
        embedding_status: current_embedding_status(&embed_snapshot).to_string(),
        embedding_endpoints,
        embed_status,
        embedder_cache: crate::embed::shared_embedder_runtime_snapshot(),
        search_mode: vector_search_circuit
            .vector_search_mode
            .as_str()
            .to_string(),
        search_policy: SearchPolicyStatus {
            query_deadline_secs: config.api.search_query_deadline_secs,
            db_deadline_secs: config.api.search_db_deadline_secs,
            embed_deadline_secs: config.embed.retry.search_deadline_secs,
            reranker_timeout_secs: config.search.reranker.timeout_secs,
        },
        embedder_circuit: vector_search_circuit.into(),
        queue_stats: queue_stats_report,
        hook_admission: crate::hook_diagnostics::hook_admission_stats(
            state.db_path.parent().unwrap_or_else(|| Path::new(".")),
            crate::hook::MAX_INLINE_PAYLOAD_BYTES as u64,
        ),
        resource_usage,
        io_burst: crate::observability::io_burst_snapshot(),
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
        wings,
        source_type_distribution,
        turn_storage: TurnStorageStatus {
            storage_mode: config.turns.storage_mode.to_string(),
            default_importance: config.turns.default_importance,
            raw_turn_count,
            raw_turn_wings: config.turns.raw_turn_wings.clone(),
            raw_turn_rooms: config.turns.raw_turn_rooms.clone(),
        },
        ingest_worker_backoff: crate::observability::ingest_worker_backoff_snapshot(),
        vector_scan: crate::observability::vector_scan_snapshot(),
        search_telemetry,
        status_warnings,
    }))
}

fn status_database_snapshot_error_warning(error: &anyhow::Error) -> String {
    if let Some(DbError::UnsupportedSchemaVersion { current, supported }) =
        error.downcast_ref::<DbError>()
    {
        return format!(
            "status database snapshot unavailable because palace.db schema {current} is newer than this daemon supports ({supported}); run `mempal daemon restart` after upgrading or install a mempal binary that supports this schema"
        );
    }
    let detail = crate::core::config::scrub_sensitive_text(&error.to_string());
    format!("status database snapshot unavailable; returning partial status: {detail}")
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
pub(super) struct ApiError {
    status: StatusCode,
    message: String,
    kind: &'static str,
    schema_skew: Option<SchemaSkew>,
    recovery_hint: Option<&'static str>,
    retryable: Option<bool>,
    stale_daemon: Option<crate::stale_daemon::StaleDaemonDiagnostic>,
    admission_receipt: Option<serde_json::Value>,
}

impl ApiError {
    pub(super) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            kind: "http_error",
            schema_skew: None,
            recovery_hint: None,
            retryable: None,
            stale_daemon: None,
            admission_receipt: None,
        }
    }

    pub(super) fn search_admission(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            kind: "search_admission_error",
            schema_skew: None,
            recovery_hint: None,
            retryable: Some(false),
            stale_daemon: None,
            admission_receipt: None,
        }
    }

    fn schema_skew(current: u32, supported: u32) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: format!(
                "database schema version {current} is newer than this mempal daemon supports ({supported}); restart or upgrade the daemon before retrying REST writes"
            ),
            kind: "schema_skew",
            schema_skew: Some(SchemaSkew { current, supported }),
            recovery_hint: Some(REST_WRITE_RESTART_HINT),
            retryable: Some(false),
            stale_daemon: None,
            admission_receipt: None,
        }
    }

    fn stale_daemon(diagnostic: crate::stale_daemon::StaleDaemonDiagnostic) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "mempal daemon binary has been deleted or replaced; restart the daemon before retrying REST writes".to_string(),
            kind: "stale_daemon",
            schema_skew: None,
            recovery_hint: Some(REST_STALE_DAEMON_RESTART_HINT),
            retryable: Some(false),
            stale_daemon: Some(diagnostic),
            admission_receipt: None,
        }
    }

    pub(super) fn database_busy() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "SQLite database is temporarily busy; retry the mempal write after the active writer completes".to_string(),
            kind: "database_busy",
            schema_skew: None,
            recovery_hint: Some(REST_WRITE_DATABASE_BUSY_HINT),
            retryable: Some(true),
            stale_daemon: None,
            admission_receipt: None,
        }
    }

    fn holder_budget_unavailable() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message:
                "mempal profile holder budget is exhausted; retry after capacity becomes available"
                    .to_string(),
            kind: "admission_unavailable",
            schema_skew: None,
            recovery_hint: None,
            retryable: Some(true),
            stale_daemon: None,
            admission_receipt: None,
        }
    }

    fn holder_budget_admission(error: DbAdmissionError) -> Self {
        let DbAdmissionError::BudgetExceeded {
            active_holders,
            max_holders,
            active_cache_bytes,
            max_cache_bytes,
            requested_cache_bytes,
            reaped_stale_holders,
            reserved_service_holders,
            service_holders,
            reason,
        } = error
        else {
            unreachable!("holder-budget receipt requires a budget admission error");
        };
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "mempal profile holder budget is exhausted; write was refused before queueing"
                .to_string(),
            kind: "admission_blocked",
            schema_skew: None,
            recovery_hint: None,
            retryable: Some(false),
            stale_daemon: None,
            admission_receipt: Some(json!({
                "outcome": "admission_blocked",
                "reason": "holder_budget_exceeded",
                "action": "write_refused",
                "created_drawer_ids": [],
                "cleanup_drawer_ids": [],
                "capacity": {"holders": max_holders, "cache_bytes": max_cache_bytes},
                "headroom": {
                    "holders": max_holders.saturating_sub(active_holders),
                    "cache_bytes": max_cache_bytes.saturating_sub(active_cache_bytes),
                },
                "profile_admission": {
                    "active_holders": active_holders,
                    "configured_holder_limit": max_holders,
                    "active_cache_bytes": active_cache_bytes,
                    "configured_cache_bytes": max_cache_bytes,
                    "reaped_stale_holders_this_snapshot": reaped_stale_holders,
                    "reserved_service_holders": reserved_service_holders,
                    "service_holders": service_holders,
                    "requested_cache_bytes": requested_cache_bytes,
                    "budget_reason": reason.to_string(),
                },
            })),
        }
    }

    fn payload_too_large(error: crate::ingest::admission::IngestRequestTooLarge) -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: error.to_string(),
            kind: "payload_too_large",
            schema_skew: None,
            recovery_hint: None,
            retryable: Some(false),
            stale_daemon: None,
            admission_receipt: None,
        }
    }

    pub(super) fn rest_body_too_large(request_bytes: u64) -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: format!(
                "REST ingest request body is too large: {request_bytes} bytes exceeds {} byte limit",
                MAX_REST_INGEST_BODY_BYTES
            ),
            kind: "payload_too_large",
            schema_skew: None,
            recovery_hint: None,
            retryable: Some(false),
            stale_daemon: None,
            admission_receipt: None,
        }
    }

    pub(super) fn queue_byte_budget(
        request_bytes: u64,
        pending_bytes: u64,
        limit_bytes: u64,
    ) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: format!(
                "REST write queue byte budget exceeded: request_bytes={request_bytes} pending_bytes={pending_bytes} limit_bytes={limit_bytes}"
            ),
            kind: "queue_byte_budget",
            schema_skew: None,
            recovery_hint: None,
            retryable: Some(true),
            stale_daemon: None,
            admission_receipt: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SchemaSkew {
    current: u32,
    supported: u32,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut error = json!({
            "message": self.message,
            "status": self.status.as_u16(),
            "kind": self.kind,
        });
        if let Some(schema_skew) = self.schema_skew {
            error["schema_version"] = json!(schema_skew.current);
            error["supported_schema_version"] = json!(schema_skew.supported);
        }
        if let Some(recovery_hint) = self.recovery_hint {
            error["recovery_hint"] = json!(recovery_hint);
        }
        if let Some(retryable) = self.retryable {
            error["retryable"] = json!(retryable);
        }
        if let Some(admission_receipt) = self.admission_receipt
            && let (Some(error_fields), Some(receipt_fields)) =
                (error.as_object_mut(), admission_receipt.as_object())
        {
            error_fields.extend(receipt_fields.clone());
        }
        if let Some(diagnostic) = self.stale_daemon {
            error["stale_daemon"] = json!(diagnostic.stale_daemon);
            error["daemon_pid"] = json!(diagnostic.daemon_pid);
            error["exe_deleted"] = json!(diagnostic.exe_deleted);
            error["retry_safe_after_restart"] = json!(diagnostic.retry_safe_after_restart);
        }
        (
            self.status,
            Json(json!({
                "error": error,
            })),
        )
            .into_response()
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WriteQueueStats {
    pub queued: u64,
    pub pending: u64,
    pub pending_bytes: u64,
    pub byte_capacity: u64,
    pub rejected_oversize: u64,
    pub completed: u64,
    pub failed: u64,
}

pub(crate) struct WriteQueue {
    sender: mpsc::Sender<WriteJob>,
    stats: Arc<WriteQueueCounters>,
    db_path: PathBuf,
    byte_capacity: u64,
    accepting: Arc<AtomicBool>,
    drain_timeout: Duration,
    drained: Arc<Notify>,
}

struct WriteQueueCounters {
    queued: AtomicU64,
    pending: AtomicU64,
    pending_bytes: AtomicU64,
    rejected_oversize: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
}

struct WriteJob {
    request_id: u64,
    request: IngestRequest,
    content_bytes: u64,
    respond_to: oneshot::Sender<Result<IngestResponse, ApiError>>,
}

impl WriteQueue {
    pub(super) fn spawn(
        db_path: PathBuf,
        embedder_factory: Arc<dyn crate::embed::EmbedderFactory>,
        capacity: usize,
        byte_capacity: u64,
        drain_timeout: Duration,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(capacity);
        let stats = Arc::new(WriteQueueCounters {
            queued: AtomicU64::new(0),
            pending: AtomicU64::new(0),
            pending_bytes: AtomicU64::new(0),
            rejected_oversize: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        });
        let accepting = Arc::new(AtomicBool::new(true));
        let drained = Arc::new(Notify::new());
        tokio::spawn(write_worker(
            db_path.clone(),
            embedder_factory,
            receiver,
            Arc::clone(&stats),
            Arc::clone(&drained),
        ));
        Self {
            sender,
            stats,
            db_path,
            byte_capacity,
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

        let content_bytes = u64::try_from(request.content.len()).unwrap_or(u64::MAX);
        let reservation = self.stats.pending_bytes.fetch_update(
            Ordering::SeqCst,
            Ordering::SeqCst,
            |pending_bytes| {
                pending_bytes
                    .checked_add(content_bytes)
                    .filter(|next| *next <= self.byte_capacity)
            },
        );
        if let Err(pending_bytes) = reservation {
            self.record_oversize_rejection("rest_write_queue", content_bytes, self.byte_capacity)
                .await;
            return Err(ApiError::queue_byte_budget(
                content_bytes,
                pending_bytes,
                self.byte_capacity,
            ));
        }

        let (respond_to, response_rx) = oneshot::channel();
        let job = WriteJob {
            request_id: next_rest_write_request_id(),
            request,
            content_bytes,
            respond_to,
        };
        match self.sender.try_send(job) {
            Ok(()) => {
                self.stats.queued.fetch_add(1, Ordering::SeqCst);
                self.stats.pending.fetch_add(1, Ordering::SeqCst);
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.stats
                    .pending_bytes
                    .fetch_sub(content_bytes, Ordering::SeqCst);
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "REST write queue is full",
                ));
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.stats
                    .pending_bytes
                    .fetch_sub(content_bytes, Ordering::SeqCst);
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
            pending_bytes: self.stats.pending_bytes.load(Ordering::SeqCst),
            byte_capacity: self.byte_capacity,
            rejected_oversize: self.stats.rejected_oversize.load(Ordering::SeqCst),
            completed: self.stats.completed.load(Ordering::SeqCst),
            failed: self.stats.failed.load(Ordering::SeqCst),
        }
    }

    async fn record_oversize_rejection(
        &self,
        surface: &'static str,
        request_bytes: u64,
        limit_bytes: u64,
    ) {
        self.stats.rejected_oversize.fetch_add(1, Ordering::SeqCst);
        tracing::warn!(
            surface,
            request_bytes,
            limit_bytes,
            "rejecting oversized ingest admission"
        );
        let db_path = self.db_path.clone();
        let recorded = tokio::task::spawn_blocking(move || {
            crate::core::queue::PendingMessageStore::new_without_reclaim(db_path)
                .record_oversize_rejection_fail_fast()
        })
        .await;
        match recorded {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(
                    ?error,
                    surface,
                    "failed to persist oversize rejection counter"
                );
            }
            Err(error) => {
                tracing::warn!(?error, surface, "oversize counter task failed");
            }
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
        let log_metadata = RestWriteLogMetadata::from_request(job.request_id, &job.request);
        let result =
            process_ingest_request(db_path.clone(), Arc::clone(&embedder_factory), job.request)
                .await;
        match &result {
            Ok(_) => {
                stats.completed.fetch_add(1, Ordering::SeqCst);
            }
            Err(error) => {
                stats.failed.fetch_add(1, Ordering::SeqCst);
                log_rest_write_failure(&log_metadata, error);
            }
        }
        stats.pending.fetch_sub(1, Ordering::SeqCst);
        stats
            .pending_bytes
            .fetch_sub(job.content_bytes, Ordering::SeqCst);
        drained.notify_waiters();
        let _ = job.respond_to.send(result);
    }
}

pub(super) fn validate_rest_write_runtime(db_path: &std::path::Path) -> Result<(), ApiError> {
    open_rest_write_database(db_path).map(|_| ())
}

fn open_rest_write_database(db_path: &std::path::Path) -> Result<Database, ApiError> {
    if let Some(diagnostic) = current_stale_daemon_diagnostic() {
        return Err(ApiError::stale_daemon(diagnostic));
    }
    Database::open(db_path).map_err(db_error_to_write_api_error)
}

fn current_executable_deleted() -> bool {
    current_stale_daemon_diagnostic().is_some()
}

fn current_stale_daemon_diagnostic() -> Option<crate::stale_daemon::StaleDaemonDiagnostic> {
    crate::stale_daemon::inspect_current()
}

fn next_rest_write_request_id() -> u64 {
    REST_WRITE_REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed) + 1
}

#[derive(Debug)]
struct RestWriteLogMetadata {
    request_id: u64,
    content_len: usize,
    content_hash_prefix: String,
    source_type: &'static str,
}

impl RestWriteLogMetadata {
    fn from_request(request_id: u64, request: &IngestRequest) -> Self {
        Self {
            request_id,
            content_len: request.content.len(),
            content_hash_prefix: blake3::hash(request.content.as_bytes()).to_hex()[..12]
                .to_string(),
            source_type: request.source_type_label(),
        }
    }
}

impl IngestRequest {
    fn source_type_label(&self) -> &'static str {
        match self.source_type.as_deref() {
            Some("user_explicit") => "user_explicit",
            Some("agent_observation") => "agent_observation",
            Some("agent_inference") => "agent_inference",
            Some("system_generated") => "system_generated",
            Some(_) => "invalid",
            None => "unspecified",
        }
    }
}

fn log_rest_write_failure(metadata: &RestWriteLogMetadata, error: &ApiError) {
    let (schema_current, schema_supported) = error
        .schema_skew
        .map(|schema| (Some(schema.current), Some(schema.supported)))
        .unwrap_or((None, None));
    tracing::error!(
        request_id = metadata.request_id,
        route = "/api/ingest",
        source_type = metadata.source_type,
        content_len = metadata.content_len,
        content_hash_prefix = %metadata.content_hash_prefix,
        http_status = error.status.as_u16(),
        error_kind = error.kind,
        retryable = error.retryable.unwrap_or(false),
        schema_version = schema_current,
        supported_schema_version = schema_supported,
        stale_binary = current_executable_deleted(),
        recovery_hint = error.recovery_hint.unwrap_or("inspect REST daemon logs and retry after fixing the write path"),
        "REST write failed"
    );
}

fn db_error_to_api_error(error: DbError) -> ApiError {
    match error {
        DbError::Admission(DbAdmissionError::BudgetExceeded { .. }) => {
            ApiError::holder_budget_unavailable()
        }
        DbError::UnsupportedSchemaVersion { current, supported } => {
            ApiError::schema_skew(current, supported)
        }
        other if db_error_is_sqlite_lock(&other) => ApiError::database_busy(),
        other => internal_error(other),
    }
}

fn db_error_to_write_api_error(error: DbError) -> ApiError {
    match error {
        DbError::Admission(admission @ DbAdmissionError::BudgetExceeded { .. }) => {
            ApiError::holder_budget_admission(admission)
        }
        other => db_error_to_api_error(other),
    }
}

pub(super) fn internal_error(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        crate::core::config::scrub_sensitive_text(&error.to_string()),
    )
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
        _ => db_error_to_write_api_error(error),
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
        .map_err(db_error_to_write_api_error)?
        .into_iter()
        .find(|summary| Some(summary.id.as_str()) != excluded_drawer_id)
        .map(|summary| summary.id))
}

fn supersede_drawer_for_ingest(db: &Database, old_id: &str, new_id: &str) -> Result<(), ApiError> {
    db.supersede_drawer(old_id, &format!("replaced by {new_id}"))
        .map_err(db_error_to_write_api_error)?;
    Ok(())
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

#[cfg(test)]
mod tests;
