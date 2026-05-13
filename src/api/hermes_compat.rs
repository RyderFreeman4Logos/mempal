use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::state::ApiState;
use crate::core::config::ConfigHandle;
use crate::core::db::Database;
use crate::core::project::resolve_project_id;

pub fn routes() -> Router<ApiState> {
    Router::new()
        .route("/api/timeline", get(timeline_handler))
        .route("/api/delete", post(delete_handler))
}

#[derive(Debug, Deserialize)]
struct TimelineQuery {
    wing: Option<String>,
    room: Option<String>,
    limit: Option<usize>,
    project_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct TimelineEntry {
    drawer_id: String,
    content: String,
    wing: String,
    room: Option<String>,
    source_file: Option<String>,
    added_at: String,
    importance: i32,
}

#[derive(Debug, Deserialize)]
struct DeleteRequest {
    drawer_id: String,
    project_id: Option<String>,
}

async fn timeline_handler(
    State(state): State<ApiState>,
    Query(query): Query<TimelineQuery>,
) -> Result<Json<Vec<TimelineEntry>>, HermesError> {
    let db = Database::open(&state.db_path).map_err(hermes_internal)?;
    let config = ConfigHandle::current();
    let project_id = resolve_project_id(query.project_id.as_deref(), config.as_ref(), None)
        .map_err(hermes_internal)?;
    let limit = query.limit.unwrap_or(50).min(500);
    let drawers = db
        .timeline_drawers(
            query.wing.as_deref(),
            query.room.as_deref(),
            project_id.as_deref(),
            limit,
        )
        .map_err(hermes_internal)?;

    let entries = drawers
        .into_iter()
        .map(|d| TimelineEntry {
            drawer_id: d.id,
            content: d.content,
            wing: d.wing,
            room: d.room,
            source_file: d.source_file,
            added_at: d.added_at,
            importance: d.importance,
        })
        .collect();
    Ok(Json(entries))
}

async fn delete_handler(
    State(state): State<ApiState>,
    Json(request): Json<DeleteRequest>,
) -> Result<impl IntoResponse, HermesError> {
    let db = Database::open(&state.db_path).map_err(hermes_internal)?;
    let config = ConfigHandle::current();
    let project_id = resolve_project_id(request.project_id.as_deref(), config.as_ref(), None)
        .map_err(hermes_internal)?;
    if let Some(ref pid) = project_id {
        let drawer_project = db
            .drawer_project_id(&request.drawer_id)
            .map_err(hermes_internal)?;
        if drawer_project.as_deref() != Some(pid.as_str()) {
            return Err(HermesError {
                status: StatusCode::FORBIDDEN,
                message: format!("drawer '{}' belongs to another project", request.drawer_id),
            });
        }
    }
    let deleted = db
        .soft_delete_drawer(&request.drawer_id)
        .map_err(hermes_internal)?;
    if deleted {
        Ok((StatusCode::OK, Json(json!({"deleted": true}))))
    } else {
        Err(HermesError {
            status: StatusCode::NOT_FOUND,
            message: format!("drawer '{}' not found", request.drawer_id),
        })
    }
}

#[derive(Debug)]
struct HermesError {
    status: StatusCode,
    message: String,
}

impl IntoResponse for HermesError {
    fn into_response(self) -> axum::response::Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

fn hermes_internal(error: impl std::fmt::Display) -> HermesError {
    HermesError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: error.to_string(),
    }
}
