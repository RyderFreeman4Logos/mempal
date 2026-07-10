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
    validate_delete_write_runtime()?;
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
                return Err(HermesError::new(
                    StatusCode::FORBIDDEN,
                    format!("drawer '{}' belongs to another project", request.drawer_id),
                ));
            }
        }
        ProjectFilterMode::NullOnly => {
            if drawer_project.is_some() {
                return Err(HermesError::new(
                    StatusCode::FORBIDDEN,
                    format!("drawer '{}' belongs to another project", request.drawer_id),
                ));
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
        Err(HermesError::new(
            StatusCode::NOT_FOUND,
            format!("drawer '{}' not found", request.drawer_id),
        ))
    }
}

#[derive(Debug)]
struct HermesError {
    status: StatusCode,
    message: String,
    stale_daemon: Option<crate::stale_daemon::StaleDaemonDiagnostic>,
}

impl HermesError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            stale_daemon: None,
        }
    }

    fn stale_daemon(diagnostic: crate::stale_daemon::StaleDaemonDiagnostic) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "mempal daemon binary has been deleted or replaced; restart the daemon before retrying REST writes".to_string(),
            stale_daemon: Some(diagnostic),
        }
    }
}

impl IntoResponse for HermesError {
    fn into_response(self) -> axum::response::Response {
        let mut error = json!({
            "message": self.message,
            "status": self.status.as_u16(),
        });
        if let Some(diagnostic) = self.stale_daemon {
            error["kind"] = json!("stale_daemon");
            error["stale_daemon"] = json!(diagnostic.stale_daemon);
            error["daemon_pid"] = json!(diagnostic.daemon_pid);
            error["exe_deleted"] = json!(diagnostic.exe_deleted);
            error["retryable"] = json!(false);
            error["retry_safe_after_restart"] = json!(diagnostic.retry_safe_after_restart);
            error["recovery_hint"] =
                json!("Run `mempal daemon restart`, then retry the write once.");
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

fn hermes_internal(error: impl std::fmt::Display) -> HermesError {
    HermesError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn validate_delete_write_runtime() -> Result<(), HermesError> {
    validate_delete_write_runtime_with(crate::stale_daemon::inspect_current)
}

fn validate_delete_write_runtime_with(
    inspect: impl FnOnce() -> Option<crate::stale_daemon::StaleDaemonDiagnostic>,
) -> Result<(), HermesError> {
    match inspect() {
        Some(diagnostic) => Err(HermesError::stale_daemon(diagnostic)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delete_write_runtime_returns_structured_stale_daemon_error() {
        let result = validate_delete_write_runtime_with(|| {
            Some(crate::stale_daemon::StaleDaemonDiagnostic {
                stale_daemon: true,
                daemon_pid: 706_141,
                exe_deleted: true,
                retry_safe_after_restart: true,
            })
        });
        let error = result.expect_err("stale daemon must reject /api/delete before DB open");
        let response = error.into_response();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read delete stale-daemon response");
        let body: serde_json::Value =
            serde_json::from_slice(&body).expect("parse delete stale-daemon response");
        let error = &body["error"];
        assert_eq!(error["kind"], "stale_daemon");
        assert_eq!(error["daemon_pid"], 706_141);
        assert_eq!(error["exe_deleted"], true);
        assert_eq!(error["retryable"], false);
        assert_eq!(error["retry_safe_after_restart"], true);
        assert!(error.get("exe_path").is_none());
    }
}
