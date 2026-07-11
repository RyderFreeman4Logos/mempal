//! REST adapter for durable ingest admission and receipt lookup.

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CONTENT_LENGTH},
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;

use super::handlers::{
    ApiError, IngestRequest, MAX_REST_INGEST_BODY_BYTES, internal_error, validate_ingest_request,
    validate_rest_write_runtime,
};
use super::state::ApiState;
use crate::core::config::ConfigHandle;
use crate::core::db::Database;
use crate::core::project::{ProjectFilterMode, ProjectSearchScope, resolve_project_id};

#[derive(Debug, Deserialize)]
struct DurableIngestAdmissionRequest {
    idempotency_key: String,
    request: IngestRequest,
}

#[derive(Debug, Deserialize)]
struct DurableDeleteRequest {
    drawer_id: String,
    project_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DurableDeleteAdmissionRequest {
    idempotency_key: String,
    request: DurableDeleteRequest,
}

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route(
            "/api/ingest/durable",
            post(ingest_handler).layer(DefaultBodyLimit::max(MAX_REST_INGEST_BODY_BYTES)),
        )
        .route("/api/delete/durable", post(delete_handler))
        .route("/api/operations/{operation_id}", get(status_handler))
}

async fn ingest_handler(
    State(state): State<ApiState>,
    headers: HeaderMap,
    request: Result<Json<DurableIngestAdmissionRequest>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Json(admission) = match request {
        Ok(request) => request,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            let request_bytes = headers
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(MAX_REST_INGEST_BODY_BYTES as u64 + 1);
            return Err(ApiError::rest_body_too_large(request_bytes));
        }
        Err(rejection) => {
            return Err(ApiError::new(rejection.status(), rejection.body_text()));
        }
    };
    validate_ingest_request(&admission.request)?;
    validate_rest_write_runtime(&state.db_path)?;

    let request = serde_json::to_value(admission.request).map_err(internal_error)?;
    let db_path = state.db_path.clone();
    let idempotency_key = admission.idempotency_key;
    let receipt = tokio::task::spawn_blocking(move || {
        crate::durable_ingest::admit(&db_path, &idempotency_key, request)
    })
    .await
    .map_err(internal_error)?
    .map_err(admission_error)?;
    Ok((StatusCode::ACCEPTED, Json(receipt)))
}

async fn status_handler(
    State(state): State<ApiState>,
    Path(operation_id): Path<String>,
) -> Result<Json<crate::durable_ingest::DurableOperationReceipt>, ApiError> {
    let db_path = state.db_path.clone();
    let receipt =
        tokio::task::spawn_blocking(move || crate::durable_ingest::status(&db_path, &operation_id))
            .await
            .map_err(internal_error)?
            .map_err(admission_error)?;
    Ok(Json(receipt))
}

async fn delete_handler(
    State(state): State<ApiState>,
    Json(admission): Json<DurableDeleteAdmissionRequest>,
) -> Result<impl IntoResponse, ApiError> {
    validate_rest_write_runtime(&state.db_path)?;
    authorize_delete(&state, &admission.request)?;
    let db_path = state.db_path.clone();
    let idempotency_key = admission.idempotency_key;
    let drawer_id = admission.request.drawer_id;
    let receipt = tokio::task::spawn_blocking(move || {
        crate::durable_ingest::admit_delete(&db_path, &idempotency_key, drawer_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(admission_error)?;
    Ok((StatusCode::ACCEPTED, Json(receipt)))
}

fn authorize_delete(state: &ApiState, request: &DurableDeleteRequest) -> Result<(), ApiError> {
    let db = Database::open(&state.db_path).map_err(internal_error)?;
    if db
        .get_drawer(&request.drawer_id)
        .map_err(internal_error)?
        .is_none()
    {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "durable delete target was not found",
        ));
    }
    let config = ConfigHandle::current();
    let project_id = resolve_project_id(request.project_id.as_deref(), config.as_ref(), None)
        .map_err(internal_error)?;
    let scope = ProjectSearchScope::from_request(
        project_id,
        false,
        false,
        config.search.strict_project_isolation,
    );
    let drawer_project = db
        .drawer_project_id(&request.drawer_id)
        .map_err(internal_error)?;
    let authorized = match scope.mode {
        ProjectFilterMode::ProjectScoped | ProjectFilterMode::ProjectPlusGlobal => {
            drawer_project.as_deref() == scope.project_id.as_deref()
        }
        ProjectFilterMode::NullOnly => drawer_project.is_none(),
        ProjectFilterMode::AllProjects => true,
    };
    if authorized {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "drawer belongs to another project",
        ))
    }
}

fn admission_error(error: crate::durable_ingest::DurableAdmissionError) -> ApiError {
    use crate::durable_ingest::DurableAdmissionError;

    match error {
        DurableAdmissionError::InvalidIdempotencyKey => {
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        }
        DurableAdmissionError::OperationNotFound => {
            ApiError::new(StatusCode::NOT_FOUND, error.to_string())
        }
        DurableAdmissionError::Queue(queue_error) if queue_error.is_sqlite_lock() => {
            ApiError::database_busy()
        }
        DurableAdmissionError::Queue(
            crate::core::queue::QueueError::IngestByteBudgetExceeded {
                payload_bytes,
                active_bytes,
                limit_bytes,
            },
        ) => ApiError::queue_byte_budget(payload_bytes, active_bytes, limit_bytes),
        other => internal_error(other),
    }
}
