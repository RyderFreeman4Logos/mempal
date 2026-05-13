use crate::core::{
    config::ConfigHandle,
    db::Database,
    project::{ProjectSearchScope, resolve_project_id},
    types::{
        BootstrapEvidenceArgs, Drawer, RouteDecision, SearchResult, SourceType, TaxonomyEntry,
    },
    utils::{build_bootstrap_evidence_drawer_id, iso_timestamp, source_file_or_synthetic},
};
use crate::ingest::gating::evaluate_fact_check_gate;
use crate::ingest::normalize::CURRENT_NORMALIZE_VERSION;
use crate::search::{resolve_route, search_with_vector};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderValue, Method, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_http::cors::{AllowOrigin, CorsLayer};

use super::state::ApiState;

pub const DEFAULT_REST_ADDR: &str = "127.0.0.1:3080";

pub async fn serve(listener: tokio::net::TcpListener, state: ApiState) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/api/search", get(search_handler))
        .route("/api/ingest", post(ingest_handler))
        .route("/api/taxonomy", get(taxonomy_handler))
        .route("/api/status", get(status_handler))
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
    project_id: Option<String>,
    include_global: Option<bool>,
    all_projects: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct IngestRequest {
    content: String,
    wing: String,
    room: Option<String>,
    source: Option<String>,
    project_id: Option<String>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fact_check_warnings: Vec<String>,
}

fn is_zero(v: &usize) -> bool {
    *v == 0
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    drawer_count: i64,
    taxonomy_count: i64,
    db_size_bytes: u64,
    wings: Vec<ScopeCount>,
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
    similarity: f32,
    route: RouteDecisionDto,
}

#[derive(Debug, Serialize)]
struct RouteDecisionDto {
    wing: Option<String>,
    room: Option<String>,
    confidence: f32,
    reason: String,
}

#[derive(Debug, Serialize)]
struct TaxonomyEntryDto {
    wing: String,
    room: String,
    display_name: Option<String>,
    keywords: Vec<String>,
}

async fn search_handler(
    State(state): State<ApiState>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<SearchResultDto>>, ApiError> {
    let embedder: Box<dyn crate::embed::Embedder> = state
        .embedder_factory
        .build()
        .await
        .map_err(internal_error)?;
    let query_vector: Vec<f32> = embedder
        .embed(&[query.q.as_str()])
        .await
        .map_err(internal_error)?
        .into_iter()
        .next()
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "embedder returned no vector",
            )
        })?;
    let db = Database::open(&state.db_path).map_err(internal_error)?;
    let route = resolve_route(&db, &query.q, query.wing.as_deref(), query.room.as_deref())
        .map_err(internal_error)?;
    let config = ConfigHandle::current();
    let scope = ProjectSearchScope::from_request(
        resolve_project_id(query.project_id.as_deref(), config.as_ref(), None)
            .map_err(internal_error)?,
        query.include_global.unwrap_or(false),
        query.all_projects.unwrap_or(false),
        config.search.strict_project_isolation,
    );
    let results = search_with_vector(
        &db,
        &query.q,
        &query_vector,
        route,
        &scope,
        query.top_k.unwrap_or(10),
    )
    .map_err(internal_error)?;

    Ok(Json(
        results.into_iter().map(SearchResultDto::from).collect(),
    ))
}

async fn ingest_handler(
    State(state): State<ApiState>,
    Json(request): Json<IngestRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let embedder: Box<dyn crate::embed::Embedder> = state
        .embedder_factory
        .build()
        .await
        .map_err(internal_error)?;
    let db = Database::open(&state.db_path).map_err(internal_error)?;
    let config = ConfigHandle::current();
    let project_id = resolve_project_id(request.project_id.as_deref(), config.as_ref(), None)
        .map_err(internal_error)?;

    // Chunk the content using the token-aware chunker (issue #57).
    let chunks =
        crate::ingest::prepare_chunks(&request.content, &config.chunker, embedder.as_ref());
    if chunks.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "content produced no chunks",
        ));
    }

    let mut accepted_chunks: Vec<(usize, &String, String)> = Vec::with_capacity(chunks.len());
    let mut fact_check_warnings = Vec::new();
    for (chunk_idx, chunk) in chunks.iter().enumerate() {
        let drawer_id = build_bootstrap_evidence_drawer_id(
            &request.wing,
            request.room.as_deref(),
            chunk,
            &SourceType::Manual,
        );
        if request.wing != "hooks-raw"
            && let Some(outcome) = evaluate_fact_check_gate(
                &drawer_id,
                chunk,
                &db,
                project_id.as_deref(),
                &config.ingest_gating.fact_check,
            )
            .map_err(internal_error)?
        {
            fact_check_warnings.extend(outcome.warnings);
            if outcome.decision.is_rejected() {
                continue;
            }
        }
        accepted_chunks.push((chunk_idx, chunk, drawer_id));
    }

    if accepted_chunks.is_empty() {
        return Ok((
            StatusCode::CREATED,
            Json(IngestResponse {
                drawer_id: String::new(),
                drawer_ids: Vec::new(),
                chunk_count: 0,
                dropped: true,
                fact_check_warnings,
            }),
        ));
    }

    // Embed all accepted chunks in one batch call.
    let chunk_refs: Vec<&str> = accepted_chunks
        .iter()
        .map(|(_, chunk, _)| chunk.as_str())
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
    for ((chunk_idx, chunk, drawer_id), vector) in accepted_chunks.iter().zip(vectors.iter()) {
        let drawer_exists = db
            .drawer_exists(drawer_id.as_str())
            .map_err(internal_error)?;

        if !drawer_exists {
            let source_file = source_file_or_synthetic(drawer_id, request.source.as_deref());
            let drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
                id: drawer_id.clone(),
                content: chunk.to_string(),
                wing: request.wing.clone(),
                room: request.room.clone(),
                source_file: Some(source_file),
                source_type: SourceType::Manual,
                added_at: iso_timestamp(),
                chunk_index: Some(*chunk_idx as i64),
                importance: 0,
            });
            let drawer = Drawer {
                normalize_version: CURRENT_NORMALIZE_VERSION,
                ..drawer
            };
            db.insert_drawer_with_project(&drawer, project_id.as_deref())
                .map_err(internal_error)?;
            db.insert_vector_with_project(drawer_id, vector, project_id.as_deref())
                .map_err(internal_error)?;
        }
        drawer_ids.push(drawer_id.clone());
    }

    let primary_drawer_id = drawer_ids.first().cloned().unwrap_or_default();
    let chunk_count = drawer_ids.len();
    Ok((
        StatusCode::CREATED,
        Json(IngestResponse {
            drawer_id: primary_drawer_id,
            drawer_ids,
            chunk_count,
            dropped: false,
            fact_check_warnings,
        }),
    ))
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
    let db = Database::open(&state.db_path).map_err(internal_error)?;
    let drawer_count = db.drawer_count().map_err(internal_error)?;
    let taxonomy_count = db.taxonomy_count().map_err(internal_error)?;
    let db_size_bytes = db.database_size_bytes().map_err(internal_error)?;
    let wings = db
        .scope_counts()
        .map_err(internal_error)?
        .into_iter()
        .map(|(wing, room, drawer_count)| ScopeCount {
            wing,
            room,
            drawer_count,
        })
        .collect();

    Ok(Json(StatusResponse {
        drawer_count,
        taxonomy_count,
        db_size_bytes,
        wings,
    }))
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
                "error": self.message,
            })),
        )
            .into_response()
    }
}

fn internal_error(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

impl From<SearchResult> for SearchResultDto {
    fn from(value: SearchResult) -> Self {
        Self {
            drawer_id: value.drawer_id,
            content: value.content,
            wing: value.wing,
            room: value.room,
            source_file: value.source_file,
            source: value.source.as_str().to_string(),
            similarity: value.similarity,
            route: value.route.into(),
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
