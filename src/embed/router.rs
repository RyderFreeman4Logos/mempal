use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use tokio::sync::Mutex;

use crate::core::config::{EffectiveEmbedEndpoint, EmbedConfig};

use super::openai_compat::{MAX_REMOTE_RETRY_HINT, OpenAiCompatibleEmbedder};
use super::retry::HeartbeatCallback;
use super::{EmbedError, Embedder, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct RoutedEmbeddingResponse {
    pub endpoint_id: String,
    pub endpoint_model: String,
    pub vectors: Vec<Vec<f32>>,
}

#[derive(Debug)]
struct RoutedEndpoint {
    config: EffectiveEmbedEndpoint,
    client: OpenAiCompatibleEmbedder,
    unavailable_until: Mutex<Option<Instant>>,
}

#[derive(Debug, Clone)]
pub struct EmbeddingRouter {
    endpoints: Arc<Vec<Arc<RoutedEndpoint>>>,
    dimensions: usize,
    name: String,
    max_input_tokens: Option<usize>,
}

impl EmbeddingRouter {
    pub fn from_config(config: &EmbedConfig) -> Result<Self> {
        let endpoints = config
            .effective_endpoints()
            .map_err(|error| EmbedError::InvalidConfiguration(error.to_string()))?;
        Self::from_endpoints(endpoints)
    }

    pub fn from_endpoints(endpoints: Vec<EffectiveEmbedEndpoint>) -> Result<Self> {
        if endpoints.is_empty() {
            return Err(EmbedError::MissingConfiguration(
                "embedding endpoints must not be empty".to_string(),
            ));
        }
        let dimensions = endpoints[0].dimensions;
        let name = endpoints[0].backend.clone();
        let max_input_tokens = endpoints
            .iter()
            .filter_map(|endpoint| endpoint.max_input_tokens)
            .min();
        let mut endpoints = endpoints.into_iter().enumerate().collect::<Vec<_>>();
        endpoints.sort_by_key(|(index, endpoint)| (endpoint.priority, *index));
        let routed = endpoints
            .into_iter()
            .map(|(_, config)| {
                let client = OpenAiCompatibleEmbedder::from_endpoint(&config)?;
                Ok(Arc::new(RoutedEndpoint {
                    config,
                    client,
                    unavailable_until: Mutex::new(None),
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            endpoints: Arc::new(routed),
            dimensions,
            name,
            max_input_tokens,
        })
    }

    pub async fn embed_routed(
        &self,
        texts: &[&str],
        heartbeat: Option<&HeartbeatCallback>,
    ) -> Result<RoutedEmbeddingResponse> {
        let mut last_retryable = None;
        let mut earliest_retry_after: Option<Duration> = None;
        let mut first_saturated_endpoint: Option<Arc<RoutedEndpoint>> = None;

        for endpoint in self.endpoints.iter() {
            if let Some(retry_after) = endpoint.temporary_unavailable_remaining().await {
                earliest_retry_after = Some(match earliest_retry_after {
                    Some(current) => current.min(retry_after),
                    None => retry_after,
                });
                continue;
            }
            refresh_heartbeat(heartbeat)?;
            match endpoint.client.try_embed(texts).await {
                Ok(Some(vectors)) => {
                    endpoint.mark_available().await;
                    crate::embed::global_embed_status()
                        .record_endpoint_success(&endpoint.config.id);
                    return Ok(RoutedEmbeddingResponse {
                        endpoint_id: endpoint.config.id.clone(),
                        endpoint_model: endpoint.config.model.clone(),
                        vectors,
                    });
                }
                Ok(None) => {
                    if first_saturated_endpoint.is_none() {
                        first_saturated_endpoint = Some(Arc::clone(endpoint));
                    }
                }
                Err(error) if should_try_next_endpoint(&error) => {
                    if let Some(retry_after) = endpoint.retry_after_for_error(&error) {
                        endpoint.mark_temporarily_unavailable(retry_after).await;
                        crate::embed::global_embed_status().record_endpoint_cooldown(
                            &endpoint.config.id,
                            retry_after,
                            &error,
                        );
                        earliest_retry_after = Some(match earliest_retry_after {
                            Some(current) => current.min(retry_after),
                            None => retry_after,
                        });
                    }
                    last_retryable = Some(error);
                }
                Err(error) => return Err(error),
            }
        }

        if let Some(endpoint) = first_saturated_endpoint
            && earliest_retry_after.is_none()
            && last_retryable.is_none()
        {
            refresh_heartbeat(heartbeat)?;
            let vectors = endpoint.client.embed_with_permit(texts).await?;
            endpoint.mark_available().await;
            crate::embed::global_embed_status().record_endpoint_success(&endpoint.config.id);
            return Ok(RoutedEmbeddingResponse {
                endpoint_id: endpoint.config.id.clone(),
                endpoint_model: endpoint.config.model.clone(),
                vectors,
            });
        }

        match (last_retryable, earliest_retry_after) {
            (None, Some(retry_after)) | (Some(_), Some(retry_after)) => {
                Err(EmbedError::TemporarilyUnavailable {
                    retry_after,
                    reason: format!(
                        "{} embedding endpoint(s) are cooling down after retryable failures",
                        self.endpoints.len()
                    ),
                })
            }
            (Some(error), _) => Err(error),
            (None, None) => Err(EmbedError::MissingConfiguration(
                "no embedding endpoint is currently available".to_string(),
            )),
        }
    }

    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    pub fn pool_capacity(&self) -> usize {
        self.endpoints
            .iter()
            .map(|endpoint| endpoint.config.max_concurrent.max(1))
            .sum()
    }
}

#[async_trait::async_trait]
impl Embedder for EmbeddingRouter {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed_routed(texts, None)
            .await
            .map(|response| response.vectors)
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn max_input_tokens(&self) -> Option<usize> {
        self.max_input_tokens
    }
}

impl RoutedEndpoint {
    async fn temporary_unavailable_remaining(&self) -> Option<Duration> {
        let mut guard = self.unavailable_until.lock().await;
        match *guard {
            Some(until) if until > Instant::now() => {
                Some(until.saturating_duration_since(Instant::now()))
            }
            Some(_) => {
                *guard = None;
                crate::embed::global_embed_status().clear_endpoint_cooldown(&self.config.id);
                None
            }
            None => None,
        }
    }

    fn retry_after_for_error(&self, error: &EmbedError) -> Option<Duration> {
        match error {
            EmbedError::HttpStatus {
                status: StatusCode::TOO_MANY_REQUESTS,
                retry_after,
                ..
            } => Some(
                retry_after.unwrap_or_else(|| Duration::from_secs(self.config.retry_interval_secs)),
            ),
            EmbedError::TemporarilyUnavailable { retry_after, .. } => Some(*retry_after),
            EmbedError::HttpRequest { .. }
            | EmbedError::Runtime(_)
            | EmbedError::WorkerPanic(_) => {
                Some(Duration::from_secs(self.config.retry_interval_secs))
            }
            EmbedError::HttpStatus { status, .. } if status.is_server_error() => {
                Some(Duration::from_secs(self.config.retry_interval_secs))
            }
            _ => None,
        }
    }

    async fn mark_temporarily_unavailable(&self, retry_after: Duration) {
        let mut guard = self.unavailable_until.lock().await;
        let now = Instant::now();
        *guard = Some(
            now.checked_add(retry_after)
                .unwrap_or_else(|| now + MAX_REMOTE_RETRY_HINT),
        );
    }

    async fn mark_available(&self) {
        let mut guard = self.unavailable_until.lock().await;
        *guard = None;
    }
}

fn should_try_next_endpoint(error: &EmbedError) -> bool {
    match error {
        EmbedError::HttpRequest { .. }
        | EmbedError::Runtime(_)
        | EmbedError::WorkerPanic(_)
        | EmbedError::TemporarilyUnavailable { .. } => true,
        EmbedError::HttpStatus { status, .. } => {
            status.is_server_error()
                || *status == StatusCode::REQUEST_TIMEOUT
                || *status == StatusCode::TOO_MANY_REQUESTS
        }
        _ => false,
    }
}

fn refresh_heartbeat(heartbeat: Option<&HeartbeatCallback>) -> Result<()> {
    if let Some(callback) = heartbeat {
        callback()?;
    }
    Ok(())
}
