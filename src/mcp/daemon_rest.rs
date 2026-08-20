//! Client-side fallback for MCP writes when the daemon owns SQLite.
//!
//! This module is intentionally available without the crate's `rest` feature:
//! that feature controls serving REST, while MCP only needs the always-present
//! `reqwest` client to delegate a write to a daemon that serves REST.

use std::fmt;
use std::time::Duration;

use super::tools::{IngestRequest, IngestResponse};
use serde_json::Value;

const REST_INGEST_PATH: &str = "/api/ingest";
const MAX_REST_INGEST_RESPONSE_BYTES: u64 = 1024 * 1024;

#[derive(Debug)]
pub(super) struct DaemonRestIngestError {
    endpoint: String,
    detail: String,
}

impl DaemonRestIngestError {
    fn new(endpoint: String, detail: impl Into<String>) -> Self {
        Self {
            endpoint,
            detail: detail.into(),
        }
    }

    pub(super) fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl fmt::Display for DaemonRestIngestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "daemon REST request to {} failed: {}",
            self.endpoint, self.detail
        )
    }
}

impl std::error::Error for DaemonRestIngestError {}

pub(super) fn ingest_endpoint(api_addr: &str) -> String {
    let addr = api_addr.trim().trim_end_matches('/');
    let base = if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    };
    format!("{base}{REST_INGEST_PATH}")
}

/// Fields that the current `/api/ingest` contract cannot preserve.
///
/// Request controls (`dry_run`, `wait`, `wait_timeout_secs`, and `smoke`) are
/// deliberately omitted: dry runs return before fallback, REST is synchronous,
/// and smoke only controls the local MCP admission path.
pub(super) fn unsupported_fields(request: &IngestRequest) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if request.source_file.is_some() {
        fields.push("source_file");
    }
    if request.diary_rollup.unwrap_or(false) {
        fields.push("diary_rollup");
    }
    if request.provenance.is_some() {
        fields.push("provenance");
    }
    if request.scope_constraints.is_some() {
        fields.push("scope_constraints");
    }
    if request.trigger_hints.is_some() {
        fields.push("trigger_hints");
    }
    if request.anchor_kind.is_some() {
        fields.push("anchor_kind");
    }
    if request.anchor_id.is_some() {
        fields.push("anchor_id");
    }
    if request.parent_anchor_id.is_some() {
        fields.push("parent_anchor_id");
    }
    if request.cwd.is_some() {
        fields.push("cwd");
    }
    fields
}

fn decode_ingest_response(body: &[u8]) -> Result<IngestResponse, serde_json::Error> {
    let mut value: Value = serde_json::from_slice(body)?;
    if let Some(object) = value.as_object_mut() {
        if object.contains_key("created_drawer_ids") {
            object.remove("cleanup_drawer_ids");
        } else if let Some(cleanup_ids) = object.remove("cleanup_drawer_ids") {
            object.insert("created_drawer_ids".to_string(), cleanup_ids);
        }
    }
    serde_json::from_value(value)
}

pub(super) async fn ingest(
    api_addr: &str,
    request: &IngestRequest,
    timeout: Duration,
) -> Result<IngestResponse, DaemonRestIngestError> {
    let endpoint = ingest_endpoint(api_addr);
    let unsupported = unsupported_fields(request);
    if !unsupported.is_empty() {
        return Err(DaemonRestIngestError::new(
            endpoint,
            format!(
                "the REST ingest contract cannot preserve MCP field(s): {}; restore daemon hook IPC for this request",
                unsupported.join(", ")
            ),
        ));
    }

    let timeout = timeout.max(Duration::from_millis(1));
    let client = reqwest::Client::builder()
        .connect_timeout(timeout.min(Duration::from_secs(1)))
        .timeout(timeout)
        .build()
        .map_err(|error| {
            DaemonRestIngestError::new(endpoint.clone(), format!("client setup error: {error}"))
        })?;
    let response = client
        .post(&endpoint)
        .json(request)
        .send()
        .await
        .map_err(|error| {
            DaemonRestIngestError::new(endpoint.clone(), format!("transport error: {error}"))
        })?;
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_REST_INGEST_RESPONSE_BYTES)
    {
        return Err(DaemonRestIngestError::new(
            endpoint,
            format!("response exceeded {} bytes", MAX_REST_INGEST_RESPONSE_BYTES),
        ));
    }
    let body = response.bytes().await.map_err(|error| {
        DaemonRestIngestError::new(endpoint.clone(), format!("response read error: {error}"))
    })?;
    if !status.is_success() {
        return Err(DaemonRestIngestError::new(
            endpoint,
            format!("returned HTTP {status}"),
        ));
    }

    decode_ingest_response(&body).map_err(|error| {
        DaemonRestIngestError::new(
            endpoint,
            format!("invalid successful response body: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_uses_http_for_bare_daemon_address() {
        assert_eq!(
            ingest_endpoint("127.0.0.1:3080/"),
            "http://127.0.0.1:3080/api/ingest"
        );
        assert_eq!(
            ingest_endpoint("https://localhost:3443"),
            "https://localhost:3443/api/ingest"
        );
    }

    #[test]
    fn smoke_and_wait_controls_are_safe_for_rest_fallback() {
        let request = IngestRequest {
            smoke: Some(true),
            wait: Some(true),
            wait_timeout_secs: Some(15),
            ..IngestRequest::default()
        };
        assert!(unsupported_fields(&request).is_empty());
    }

    #[test]
    fn rest_response_maps_cleanup_ids_to_created_ids() {
        let response = decode_ingest_response(
            serde_json::json!({
                "drawer_id": "created-drawer",
                "cleanup_drawer_ids": ["created-drawer"],
                "chunk_count": 1,
            })
            .to_string()
            .as_bytes(),
        )
        .expect("cleanup-only receipt should decode");

        assert_eq!(response.created_drawer_ids, ["created-drawer"]);
    }

    #[test]
    fn rest_response_prefers_created_ids_when_both_fields_exist() {
        let response = decode_ingest_response(
            serde_json::json!({
                "drawer_id": "created-drawer",
                "created_drawer_ids": ["created-drawer"],
                "cleanup_drawer_ids": ["cleanup-drawer"],
                "chunk_count": 1,
            })
            .to_string()
            .as_bytes(),
        )
        .expect("current receipt should decode");

        assert_eq!(response.created_drawer_ids, ["created-drawer"]);
    }

    #[test]
    fn rest_fallback_rejects_fields_it_cannot_preserve() {
        let request = IngestRequest {
            source_file: Some("session.jsonl".to_string()),
            anchor_id: Some("anchor-1".to_string()),
            ..IngestRequest::default()
        };
        assert_eq!(unsupported_fields(&request), ["source_file", "anchor_id"]);
    }
}
