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
use crate::core::project::{ProjectFilterMode, ProjectSearchScope, resolve_project_id};
use crate::core::strata::is_excluded_raw_turn_drawer;

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
    include_raw_turns: Option<bool>,
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
    let scope = ProjectSearchScope::from_request(
        project_id,
        false,
        false,
        config.search.strict_project_isolation,
    );
    let limit = query.limit.unwrap_or(50).min(500);
    let strict_null_only = scope.mode == ProjectFilterMode::NullOnly;
    let drawers = db
        .timeline_drawers(
            query.wing.as_deref(),
            query.room.as_deref(),
            scope.project_id.as_deref(),
            limit,
            strict_null_only,
        )
        .map_err(hermes_internal)?;

    let exclude_raw_turns =
        config.search.exclude_raw_turns && !query.include_raw_turns.unwrap_or(false);
    let entries = drawers
        .into_iter()
        .filter(|drawer| !exclude_raw_turns || !is_excluded_raw_turn_drawer(drawer, &config.turns))
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
    let scope = ProjectSearchScope::from_request(
        project_id,
        false,
        false,
        config.search.strict_project_isolation,
    );
    let drawer_project = db
        .drawer_project_id(&request.drawer_id)
        .map_err(hermes_internal)?;
    match scope.mode {
        ProjectFilterMode::ProjectScoped | ProjectFilterMode::ProjectPlusGlobal => {
            if drawer_project.as_deref() != scope.project_id.as_deref() {
                return Err(HermesError {
                    status: StatusCode::FORBIDDEN,
                    message: format!("drawer '{}' belongs to another project", request.drawer_id),
                });
            }
        }
        ProjectFilterMode::NullOnly => {
            if drawer_project.is_some() {
                return Err(HermesError {
                    status: StatusCode::FORBIDDEN,
                    message: format!("drawer '{}' belongs to another project", request.drawer_id),
                });
            }
        }
        ProjectFilterMode::AllProjects => {}
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
