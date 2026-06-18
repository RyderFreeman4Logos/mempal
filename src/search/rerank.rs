//! Optional local reranker support for search results.
//!
//! The default configuration never calls a reranker. When enabled, the HTTP
//! reranker receives only the configured top-K candidate contents and failures
//! degrade to the original search order with a diagnostic warning.

use std::collections::HashSet;
use std::time::Duration;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::core::config::{
    RemoteCallPolicyConfig, SearchRerankerConfig, normalize_reranker_endpoint_url,
    scrub_sensitive_text,
};
use crate::core::types::SearchResult;

const MAX_RERANKER_RESPONSE_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct RerankOutcome {
    pub results: Vec<SearchResult>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRerankOutcome {
    pub order: Vec<usize>,
    pub warnings: Vec<String>,
}

impl IndexRerankOutcome {
    fn unchanged(len: usize) -> Self {
        Self {
            order: (0..len).collect(),
            warnings: Vec::new(),
        }
    }

    fn fallback(len: usize, error: RerankError) -> Self {
        Self {
            order: (0..len).collect(),
            warnings: vec![reranker_fallback_warning(&error)],
        }
    }
}

impl RerankOutcome {
    fn unchanged(results: Vec<SearchResult>) -> Self {
        Self {
            results,
            warnings: Vec::new(),
        }
    }

    fn fallback(results: Vec<SearchResult>, error: RerankError) -> Self {
        Self {
            results,
            warnings: vec![reranker_fallback_warning(&error)],
        }
    }
}

#[derive(Debug, Error)]
pub enum RerankError {
    #[error("missing reranker configuration: {0}")]
    MissingConfiguration(&'static str),
    #[error("{0}")]
    InvalidEndpoint(String),
    #[error("reranker request timed out")]
    Timeout,
    #[error("failed to call reranker endpoint: {source}")]
    HttpRequest {
        #[source]
        source: reqwest::Error,
    },
    #[error("reranker endpoint returned error status {status}")]
    HttpStatus { status: StatusCode },
    #[error(
        "reranker endpoint response body exceeded {limit} bytes (status {status}, received {received} bytes)"
    )]
    ResponseBodyTooLarge {
        status: StatusCode,
        limit: u64,
        received: u64,
    },
    #[error("failed to decode reranker response: {0}")]
    DecodeResponse(String),
    #[error("invalid reranker response: {0}")]
    InvalidResponse(String),
    #[error("{0}")]
    RemoteCallPolicy(#[from] crate::core::remote_calls::RemoteCallPolicyError),
}

/// Reranks a list of search results given the original query.
#[async_trait::async_trait]
pub trait Reranker: Send + Sync {
    async fn try_rerank(
        &self,
        query: &str,
        results: Vec<SearchResult>,
    ) -> Result<Vec<SearchResult>, RerankError>;
}

/// Default reranker that does nothing and performs no network calls.
pub struct NoopReranker;

#[async_trait::async_trait]
impl Reranker for NoopReranker {
    async fn try_rerank(
        &self,
        _query: &str,
        results: Vec<SearchResult>,
    ) -> Result<Vec<SearchResult>, RerankError> {
        Ok(results)
    }
}

#[derive(Debug, Clone)]
pub struct HttpReranker {
    client: reqwest::Client,
    endpoint: String,
    model: String,
}

impl HttpReranker {
    pub fn from_config(config: &SearchRerankerConfig) -> Result<Self, RerankError> {
        Self::from_config_inner(config)
    }

    pub fn from_config_with_policy(
        config: &SearchRerankerConfig,
        policy: &RemoteCallPolicyConfig,
    ) -> Result<Self, RerankError> {
        crate::core::remote_calls::ensure_rerank_allowed(policy, config)?;
        Self::from_config_inner(config)
    }

    fn from_config_inner(config: &SearchRerankerConfig) -> Result<Self, RerankError> {
        let endpoint = config
            .endpoint
            .as_deref()
            .ok_or(RerankError::MissingConfiguration("endpoint"))?;
        let endpoint =
            normalize_reranker_endpoint_url(endpoint).map_err(RerankError::InvalidEndpoint)?;
        let model = config
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .ok_or(RerankError::MissingConfiguration("model"))?
            .to_string();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .map_err(|source| RerankError::HttpRequest { source })?;
        Ok(Self {
            client,
            endpoint,
            model,
        })
    }

    pub async fn try_rerank_indices(
        &self,
        query: &str,
        documents: Vec<&str>,
    ) -> Result<Vec<usize>, RerankError> {
        if documents.len() <= 1 {
            return Ok((0..documents.len()).collect());
        }
        let document_count = documents.len();
        let request = RerankRequest {
            model: &self.model,
            query,
            documents,
            top_n: document_count,
        };
        let response = self
            .client
            .post(&self.endpoint)
            .json(&request)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let status = response.status();
        if !status.is_success() {
            read_limited_response_body(response).await?;
            return Err(RerankError::HttpStatus { status });
        }
        let body = read_limited_response_body(response).await?;
        let response = serde_json::from_slice::<RerankResponse>(&body)
            .map_err(|source| RerankError::DecodeResponse(source.to_string()))?;
        reorder_indices(document_count, response.items())
    }
}

#[async_trait::async_trait]
impl Reranker for HttpReranker {
    async fn try_rerank(
        &self,
        query: &str,
        results: Vec<SearchResult>,
    ) -> Result<Vec<SearchResult>, RerankError> {
        let documents = results
            .iter()
            .map(|result| result.content.as_str())
            .collect();
        let order = self.try_rerank_indices(query, documents).await?;
        Ok(order
            .into_iter()
            .map(|index| results[index].clone())
            .collect())
    }
}

async fn read_limited_response_body(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, RerankError> {
    let status = response.status();
    let limit = max_reranker_response_body_bytes_u64();
    if let Some(content_length) = response.content_length()
        && content_length > limit
    {
        return Err(RerankError::ResponseBodyTooLarge {
            status,
            limit,
            received: content_length,
        });
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(map_reqwest_error)? {
        let next_len = body.len().saturating_add(chunk.len());
        if next_len > MAX_RERANKER_RESPONSE_BODY_BYTES {
            return Err(RerankError::ResponseBodyTooLarge {
                status,
                limit,
                received: usize_to_u64(next_len),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn max_reranker_response_body_bytes_u64() -> u64 {
    usize_to_u64(MAX_RERANKER_RESPONSE_BODY_BYTES)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn map_reqwest_error(source: reqwest::Error) -> RerankError {
    if source.is_timeout() {
        RerankError::Timeout
    } else {
        RerankError::HttpRequest { source }
    }
}

pub async fn maybe_rerank_with_config(
    config: &SearchRerankerConfig,
    query: &str,
    results: Vec<SearchResult>,
) -> RerankOutcome {
    maybe_rerank_with_config_and_policy(config, &RemoteCallPolicyConfig::default(), query, results)
        .await
}

pub async fn maybe_rerank_with_config_and_policy(
    config: &SearchRerankerConfig,
    policy: &RemoteCallPolicyConfig,
    query: &str,
    mut results: Vec<SearchResult>,
) -> RerankOutcome {
    if !config.enabled || results.len() <= 1 {
        return RerankOutcome::unchanged(results);
    }

    let candidate_count = config.top_k.min(results.len());
    let tail = results.split_off(candidate_count);
    let candidates = results;
    let reranker = match HttpReranker::from_config_with_policy(config, policy) {
        Ok(reranker) => reranker,
        Err(error) => {
            let mut original = candidates;
            original.extend(tail);
            return RerankOutcome::fallback(original, error);
        }
    };
    match reranker.try_rerank(query, candidates.clone()).await {
        Ok(mut reranked) => {
            reranked.extend(tail);
            RerankOutcome::unchanged(reranked)
        }
        Err(error) => {
            let mut original = candidates;
            original.extend(tail);
            RerankOutcome::fallback(original, error)
        }
    }
}

pub async fn maybe_rerank_indices_with_config(
    config: &SearchRerankerConfig,
    query: &str,
    documents: Vec<&str>,
) -> IndexRerankOutcome {
    maybe_rerank_indices_with_config_and_policy(
        config,
        &RemoteCallPolicyConfig::default(),
        query,
        documents,
    )
    .await
}

pub async fn maybe_rerank_indices_with_config_and_policy(
    config: &SearchRerankerConfig,
    policy: &RemoteCallPolicyConfig,
    query: &str,
    documents: Vec<&str>,
) -> IndexRerankOutcome {
    if !config.enabled || documents.len() <= 1 {
        return IndexRerankOutcome::unchanged(documents.len());
    }

    let candidate_count = config.top_k.min(documents.len());
    let candidates = documents[..candidate_count].to_vec();
    let reranker = match HttpReranker::from_config_with_policy(config, policy) {
        Ok(reranker) => reranker,
        Err(error) => return IndexRerankOutcome::fallback(documents.len(), error),
    };
    match reranker.try_rerank_indices(query, candidates).await {
        Ok(mut order) => {
            order.extend(candidate_count..documents.len());
            IndexRerankOutcome {
                order,
                warnings: Vec::new(),
            }
        }
        Err(error) => IndexRerankOutcome::fallback(documents.len(), error),
    }
}

fn reranker_fallback_warning(error: &RerankError) -> String {
    format!(
        "reranker unavailable; using original search ranking: {}",
        scrub_sensitive_text(&error.to_string())
    )
}

#[derive(Debug, Serialize)]
struct RerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: Vec<&'a str>,
    top_n: usize,
}

#[derive(Debug, Deserialize)]
struct RerankResponse {
    #[serde(default)]
    results: Vec<RerankItem>,
    #[serde(default)]
    data: Vec<RerankItem>,
}

impl RerankResponse {
    fn items(self) -> Vec<RerankItem> {
        if self.results.is_empty() {
            self.data
        } else {
            self.results
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RerankItem {
    index: usize,
    #[serde(default, alias = "relevance_score")]
    score: Option<f32>,
}

#[cfg(test)]
fn reorder_results(
    results: Vec<SearchResult>,
    items: Vec<RerankItem>,
) -> Result<Vec<SearchResult>, RerankError> {
    let order = reorder_indices(results.len(), items)?;
    Ok(order
        .into_iter()
        .map(|index| results[index].clone())
        .collect())
}

fn reorder_indices(len: usize, items: Vec<RerankItem>) -> Result<Vec<usize>, RerankError> {
    if items.is_empty() {
        return Err(RerankError::InvalidResponse(
            "missing results or data array".to_string(),
        ));
    }
    let mut ranked = items
        .into_iter()
        .filter_map(|item| {
            if item.index >= len {
                return None;
            }
            let score = item.score?;
            if score.is_finite() {
                Some((item.index, score))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    if ranked.is_empty() {
        return Err(RerankError::InvalidResponse(
            "no valid scored candidate indices".to_string(),
        ));
    }
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut seen = HashSet::new();
    let mut ordered = Vec::with_capacity(len);
    for (index, _) in ranked {
        if seen.insert(index) {
            ordered.push(index);
        }
    }
    for index in 0..len {
        if seen.insert(index) {
            ordered.push(index);
        }
    }
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::project::SearchResultSource;
    use crate::core::types::{AnchorKind, MemoryDomain, MemoryKind, RouteDecision, SourceType};
    use tokio::io::AsyncReadExt;

    fn result(id: &str) -> SearchResult {
        SearchResult {
            drawer_id: id.to_string(),
            content: format!("content {id}"),
            wing: "mempal".to_string(),
            room: Some("search".to_string()),
            source_file: format!("{id}.md"),
            source: SearchResultSource::Project,
            source_type: SourceType::AgentInference,
            confidence: 0.8,
            memory_kind: MemoryKind::Evidence,
            domain: MemoryDomain::Project,
            field: String::new(),
            statement: None,
            tier: None,
            status: None,
            anchor_kind: AnchorKind::Global,
            anchor_id: String::new(),
            parent_anchor_id: None,
            is_pinned: false,
            importance: 0,
            similarity: 0.5,
            route: RouteDecision {
                wing: None,
                room: None,
                confidence: 0.0,
                reason: "test".to_string(),
            },
            chunk_index: None,
            neighbors: None,
            tunnel_hints: Vec::new(),
            effective_importance: 0.0,
            matched_pattern_id: None,
        }
    }

    fn result_with_content(id: &str, content: &str) -> SearchResult {
        let mut result = result(id);
        result.content = content.to_string();
        result
    }

    fn warning_text(outcome: &RerankOutcome) -> String {
        outcome.warnings.join("\n")
    }

    #[test]
    fn reorder_results_sorts_by_score_and_appends_unscored_candidates() {
        let results = vec![result("a"), result("b"), result("c")];
        let reranked = reorder_results(
            results,
            vec![
                RerankItem {
                    index: 1,
                    score: Some(0.9),
                },
                RerankItem {
                    index: 0,
                    score: Some(0.1),
                },
            ],
        )
        .expect("rerank");

        let ids = reranked
            .iter()
            .map(|result| result.drawer_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["b", "a", "c"]);
    }

    fn config(endpoint: Option<String>) -> SearchRerankerConfig {
        SearchRerankerConfig {
            enabled: true,
            endpoint,
            model: Some("test-reranker".to_string()),
            timeout_secs: 1,
            top_k: 2,
        }
    }

    fn ids(results: &[SearchResult]) -> Vec<&str> {
        results
            .iter()
            .map(|result| result.drawer_id.as_str())
            .collect()
    }

    #[tokio::test]
    async fn disabled_reranker_preserves_order_without_endpoint() {
        let config = SearchRerankerConfig::default();
        let outcome =
            maybe_rerank_with_config(&config, "query", vec![result("a"), result("b")]).await;

        assert!(outcome.warnings.is_empty());
        assert_eq!(ids(&outcome.results), vec!["a", "b"]);
    }

    #[tokio::test]
    async fn successful_http_reranker_reorders_configured_top_k_only() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/rerank")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "model": "test-reranker",
                "query": "query",
                "top_n": 2
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"results":[{"index":1,"relevance_score":0.98},{"index":0,"relevance_score":0.12}]}"#,
            )
            .create_async()
            .await;

        let outcome = maybe_rerank_with_config(
            &config(Some(server.url())),
            "query",
            vec![result("a"), result("b"), result("c")],
        )
        .await;

        mock.assert_async().await;
        assert!(outcome.warnings.is_empty());
        assert_eq!(ids(&outcome.results), vec!["b", "a", "c"]);
    }

    #[tokio::test]
    async fn reranker_http_error_preserves_order_and_warns() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/rerank")
            .with_status(500)
            .with_body("reranker down")
            .create_async()
            .await;

        let outcome = maybe_rerank_with_config(
            &config(Some(server.url())),
            "query",
            vec![result("a"), result("b")],
        )
        .await;

        mock.assert_async().await;
        assert_eq!(ids(&outcome.results), vec!["a", "b"]);
        let warnings = warning_text(&outcome);
        assert!(warnings.contains("reranker unavailable"));
        assert!(warnings.contains("original search ranking"));
        assert!(warnings.contains("500 Internal Server Error"));
        assert!(!warnings.contains("reranker down"));
    }

    #[tokio::test]
    async fn reranker_http_error_body_echoing_candidate_is_not_warned() {
        let raw_drawer_content = "UNIQUE_RAW_DRAWER_CONTENT_SHOULD_NOT_LEAK";
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/rerank")
            .with_status(400)
            .with_body(format!("invalid document: {raw_drawer_content}"))
            .create_async()
            .await;

        let outcome = maybe_rerank_with_config(
            &config(Some(server.url())),
            "query",
            vec![
                result_with_content("a", raw_drawer_content),
                result_with_content("b", "ordinary candidate"),
            ],
        )
        .await;

        mock.assert_async().await;
        assert_eq!(ids(&outcome.results), vec!["a", "b"]);
        let warnings = warning_text(&outcome);
        assert!(warnings.contains("400 Bad Request"));
        assert!(!warnings.contains(raw_drawer_content));
        assert!(!warnings.contains("invalid document"));
    }

    #[tokio::test]
    async fn oversized_reranker_error_body_preserves_order_without_warning_body() {
        let raw_drawer_content = "OVERSIZED_ERROR_ECHO_SHOULD_NOT_LEAK";
        let oversized_body = format!(
            "{raw_drawer_content}{}",
            "x".repeat(MAX_RERANKER_RESPONSE_BODY_BYTES + 1)
        );
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/rerank")
            .with_status(503)
            .with_body(oversized_body)
            .create_async()
            .await;

        let outcome = maybe_rerank_with_config(
            &config(Some(server.url())),
            "query",
            vec![
                result_with_content("a", raw_drawer_content),
                result_with_content("b", "ordinary candidate"),
            ],
        )
        .await;

        mock.assert_async().await;
        assert_eq!(ids(&outcome.results), vec!["a", "b"]);
        let warnings = warning_text(&outcome);
        assert!(warnings.contains("response body exceeded"));
        assert!(warnings.contains("503 Service Unavailable"));
        assert!(!warnings.contains(raw_drawer_content));
    }

    #[tokio::test]
    async fn oversized_reranker_success_body_preserves_order_without_warning_body() {
        let raw_drawer_content = "OVERSIZED_SUCCESS_ECHO_SHOULD_NOT_LEAK";
        let oversized_invalid_json = format!(
            "{{\"echo\":\"{raw_drawer_content}\",\"padding\":\"{}",
            "x".repeat(MAX_RERANKER_RESPONSE_BODY_BYTES + 1)
        );
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/rerank")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(oversized_invalid_json)
            .create_async()
            .await;

        let outcome = maybe_rerank_with_config(
            &config(Some(server.url())),
            "query",
            vec![
                result_with_content("a", raw_drawer_content),
                result_with_content("b", "ordinary candidate"),
            ],
        )
        .await;

        mock.assert_async().await;
        assert_eq!(ids(&outcome.results), vec!["a", "b"]);
        let warnings = warning_text(&outcome);
        assert!(warnings.contains("response body exceeded"));
        assert!(warnings.contains("200 OK"));
        assert!(!warnings.contains(raw_drawer_content));
    }

    #[tokio::test]
    async fn reranker_timeout_preserves_order_and_warns() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind delayed reranker");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let mut buffer = [0_u8; 1024];
            let _ = stream.read(&mut buffer).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let outcome = maybe_rerank_with_config(
            &config(Some(format!("http://{addr}/v1/rerank"))),
            "query",
            vec![result("a"), result("b")],
        )
        .await;
        server.abort();

        assert_eq!(ids(&outcome.results), vec!["a", "b"]);
        assert!(outcome.warnings.iter().any(|warning| {
            warning.contains("reranker unavailable") && warning.contains("timed out")
        }));
    }
}
