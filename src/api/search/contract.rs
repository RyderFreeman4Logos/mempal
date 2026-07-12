use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::{http::HeaderValue, response::Response as AxumResponse};
use serde::Serialize;
use tokio::time::Instant;

use crate::core::types::{RouteDecision, SearchResult};
use crate::search::{SearchMode, SearchTelemetryStage};

use super::super::handlers::{
    domain_slug, knowledge_status_slug, knowledge_tier_slug, memory_kind_slug,
};

const REST_SEARCH_METADATA_HEADER: &str = "mempal-search-metadata";
const REST_SEARCH_WARNING_HEADER: &str = "mempal-warnings";
const MAX_BM25_FALLBACK_RESERVE: Duration = Duration::from_millis(1_500);
const MAX_ROUTE_BUDGET: Duration = Duration::from_millis(250);
static SEARCH_CORRELATION_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize)]
pub(super) struct SearchResultDto {
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

#[derive(Debug, Clone, Serialize)]
pub(super) struct SearchTimeoutMetadata {
    stage: String,
    boundary: String,
}

#[derive(Debug, Serialize)]
pub(super) struct SearchResponseMetadata<'a> {
    pub correlation_id: &'a str,
    pub elapsed_ms: u64,
    pub deadline_ms: u64,
    pub partial: bool,
    pub retry_safe: bool,
    pub fallback_used: &'a [String],
    pub timeouts: &'a [SearchTimeoutMetadata],
}

pub(super) struct SearchBudget {
    started: Instant,
    deadline: Instant,
    pub total: Duration,
    fallback_reserve: Duration,
}

impl SearchBudget {
    pub fn new(total: Duration) -> Self {
        let started = Instant::now();
        Self {
            started,
            deadline: started + total,
            total,
            fallback_reserve: MAX_BM25_FALLBACK_RESERVE.min(total / 4),
        }
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub fn route_limit(&self, configured: Duration) -> Duration {
        let fraction = Duration::from_millis((duration_ms(self.total) / 10).max(1));
        configured
            .min(MAX_ROUTE_BUDGET)
            .min(fraction)
            .min(self.remaining().saturating_sub(self.fallback_reserve))
    }

    pub fn primary_limit(&self, configured: Duration) -> Duration {
        configured.min(self.remaining().saturating_sub(self.fallback_reserve))
    }

    pub fn fallback_limit(&self, configured: Duration) -> Duration {
        configured.min(self.remaining())
    }

    pub fn elapsed(&self) -> Duration {
        Instant::now().saturating_duration_since(self.started)
    }
}

#[derive(Default)]
pub(super) struct SearchExecutionMetadata {
    pub timeouts: Vec<SearchTimeoutMetadata>,
    pub fallbacks: Vec<String>,
}

impl SearchExecutionMetadata {
    pub fn timeout(&mut self, stage: SearchTelemetryStage, boundary: &str) {
        let stage = stage.as_str().to_string();
        if self.timeouts.iter().any(|item| item.stage == stage) {
            return;
        }
        self.timeouts.push(SearchTimeoutMetadata {
            stage,
            boundary: boundary.to_string(),
        });
    }

    pub fn fallback(&mut self, fallback: &str) {
        if !self.fallbacks.iter().any(|value| value == fallback) {
            self.fallbacks.push(fallback.to_string());
        }
    }

    pub fn partial(&self) -> bool {
        !self.timeouts.is_empty()
    }

    pub fn timed_out_stages(&self) -> Vec<String> {
        self.timeouts
            .iter()
            .map(|item| item.stage.clone())
            .collect()
    }
}

pub(super) fn safe_correlation_id(candidate: Option<&str>) -> String {
    candidate
        .map(str::trim)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
        .map(str::to_string)
        .unwrap_or_else(|| {
            let id = SEARCH_CORRELATION_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
            format!("server-search-{id}")
        })
}

pub(super) fn attach_search_headers(
    response: &mut AxumResponse,
    search_mode: SearchMode,
    warnings: &[String],
    metadata: &SearchResponseMetadata<'_>,
) {
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
    if let Ok(serialized) = serde_json::to_string(metadata)
        && let Ok(value) = HeaderValue::from_str(&serialized)
    {
        response
            .headers_mut()
            .insert(REST_SEARCH_METADATA_HEADER, value);
    }
}

pub(super) fn rest_search_timeout_warning(stage: &str, deadline: Duration) -> String {
    format!(
        "{stage} deadline exceeded after {}; returning partial/fallback search results",
        display_duration(deadline)
    )
}

pub(super) fn embedding_timeout_warning(deadline: Duration) -> String {
    format!(
        "embedding deadline exceeded after {}; using BM25-only search (retry may help)",
        display_duration(deadline)
    )
}

pub(super) fn reranker_timeout_warning(deadline: Duration) -> String {
    format!(
        "reranker deadline exceeded after {}; using original search ranking",
        display_duration(deadline)
    )
}

fn display_duration(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration_ms(duration))
    }
}

pub(super) fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl SearchResultDto {
    pub fn from_result(value: SearchResult, search_mode: SearchMode, warnings: &[String]) -> Self {
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
