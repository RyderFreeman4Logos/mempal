//! Terminal timeout response finalization for bounded REST search.

use std::time::Duration;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::search::{SearchMode, SearchTelemetryStage};

use super::contract::{
    SearchBudget, SearchExecutionMetadata, SearchResponseMetadata, attach_search_headers,
    duration_ms,
};
use crate::api::state::{SearchTelemetryGuard, SearchTelemetryOutcome};

pub(super) struct TerminalSearchTimeout {
    pub telemetry: SearchTelemetryGuard,
    pub budget: SearchBudget,
    pub correlation_id: String,
    pub search_mode: SearchMode,
    pub metadata: SearchExecutionMetadata,
    pub route_elapsed: Duration,
    pub embed_elapsed: Duration,
    pub db_elapsed: Duration,
    pub message: String,
    pub stage: SearchTelemetryStage,
    pub boundary: &'static str,
}

pub(super) fn finalize_terminal_timeout(mut timeout: TerminalSearchTimeout) -> Response {
    timeout.metadata.timeout(timeout.stage, timeout.boundary);
    timeout.telemetry.finish(SearchTelemetryOutcome {
        search_mode: timeout.search_mode.as_str().to_string(),
        route: timeout.route_elapsed,
        embed: timeout.embed_elapsed,
        db: timeout.db_elapsed,
        rerank: Duration::ZERO,
        lock_wait: Duration::ZERO,
        result_count: 0,
        warning_count: 1,
        partial: true,
        timed_out_stages: timeout.metadata.timed_out_stages(),
        fallbacks: timeout.metadata.fallbacks.clone(),
    });

    let metadata = SearchResponseMetadata {
        correlation_id: &timeout.correlation_id,
        elapsed_ms: duration_ms(timeout.budget.elapsed()),
        deadline_ms: duration_ms(timeout.budget.total),
        partial: true,
        retry_safe: true,
        fallback_used: &timeout.metadata.fallbacks,
        timeouts: &timeout.metadata.timeouts,
    };
    let mut response = (
        StatusCode::GATEWAY_TIMEOUT,
        Json(serde_json::json!({
            "error": {
                "message": &timeout.message,
                "status": StatusCode::GATEWAY_TIMEOUT.as_u16(),
                "kind": "search_timeout",
                "retryable": true,
                "search_metadata": &metadata,
            }
        })),
    )
        .into_response();
    attach_search_headers(
        &mut response,
        timeout.search_mode,
        std::slice::from_ref(&timeout.message),
        &metadata,
    );
    response
}
