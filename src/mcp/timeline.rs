use rmcp::schemars::{self, JsonSchema};
use rmcp::{ErrorData, Json};
use serde::{Deserialize, Serialize};

use crate::core::config::ConfigHandle;
use crate::core::project::ProjectSearchScope;
use crate::core::timeline::{
    DEFAULT_TIMELINE_SINCE, DEFAULT_TIMELINE_TOP_K, MAX_TIMELINE_TOP_K, TimelineError,
    TimelineQuery, TimelineReport, build_timeline_report,
};

use super::server::{MempalMcpServer, current_system_warnings};
use super::tools::SystemWarning;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TimelineRequest {
    pub project_id: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub top_k: Option<usize>,
    pub min_importance: Option<u8>,
    pub wing: Option<String>,
    pub room: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TimelineResponse {
    pub project_id: Option<String>,
    pub generated_at: String,
    pub window: TimelineWindow,
    pub entries: Vec<TimelineEntry>,
    pub stats: TimelineStats,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TimelineWindow {
    pub since: String,
    pub until: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TimelineEntry {
    pub drawer_id: String,
    pub added_at: String,
    pub importance_stars: u8,
    pub wing: String,
    pub room: Option<String>,
    pub preview: String,
    pub preview_truncated: bool,
    pub original_content_bytes: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TimelineStats {
    pub total_in_window: u64,
    pub returned: usize,
    pub top_wings: Vec<TimelineWingCount>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TimelineWingCount {
    pub wing: String,
    pub count: u64,
}

pub(super) async fn handle(
    server: &MempalMcpServer,
    request: TimelineRequest,
) -> Result<Json<TimelineResponse>, ErrorData> {
    let top_k = request.top_k.unwrap_or(DEFAULT_TIMELINE_TOP_K);
    if top_k > MAX_TIMELINE_TOP_K {
        return Err(ErrorData::invalid_params(
            format!("top_k exceeds max {MAX_TIMELINE_TOP_K}"),
            None,
        ));
    }

    let min_importance = request.min_importance.unwrap_or(1);
    if min_importance > 5 {
        return Err(ErrorData::invalid_params(
            "min_importance must be between 0 and 5",
            None,
        ));
    }

    let config = ConfigHandle::current();
    let project_id = server
        .resolve_mcp_project_id(request.project_id.as_deref(), config.as_ref())
        .await?;
    let scope = ProjectSearchScope::from_request(project_id.clone(), false, false, true);
    let db = server.open_db()?;
    let report = build_timeline_report(
        &db,
        TimelineQuery {
            project_id,
            scope,
            since: request
                .since
                .unwrap_or_else(|| DEFAULT_TIMELINE_SINCE.to_string()),
            until: request.until,
            top_k,
            min_importance,
            wing: request.wing,
            room: request.room,
        },
    )
    .map_err(timeline_error)?;

    Ok(Json(TimelineResponse::from_report(
        report,
        current_system_warnings(),
    )))
}

impl TimelineResponse {
    fn from_report(report: TimelineReport, system_warnings: Vec<SystemWarning>) -> Self {
        Self {
            project_id: report.project_id,
            generated_at: report.generated_at,
            window: TimelineWindow {
                since: report.window.since,
                until: report.window.until,
            },
            entries: report
                .entries
                .into_iter()
                .map(|entry| TimelineEntry {
                    drawer_id: entry.drawer_id,
                    added_at: entry.added_at,
                    importance_stars: entry.importance_stars,
                    wing: entry.wing,
                    room: entry.room,
                    preview: entry.preview,
                    preview_truncated: entry.preview_truncated,
                    original_content_bytes: entry.original_content_bytes,
                })
                .collect(),
            stats: TimelineStats {
                total_in_window: report.stats.total_in_window,
                returned: report.stats.returned,
                top_wings: report
                    .stats
                    .top_wings
                    .into_iter()
                    .map(|entry| TimelineWingCount {
                        wing: entry.wing,
                        count: entry.count,
                    })
                    .collect(),
            },
            system_warnings,
        }
    }
}

fn timeline_error(error: TimelineError) -> ErrorData {
    match error {
        TimelineError::InvalidSince { .. } | TimelineError::InvalidUntil { .. } => {
            ErrorData::invalid_params(error.to_string(), None)
        }
        TimelineError::Prepare(_) | TimelineError::Query(_) => {
            ErrorData::internal_error(error.to_string(), None)
        }
    }
}
