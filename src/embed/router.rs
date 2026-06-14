use std::time::Duration;

use reqwest::StatusCode;

use crate::core::config::{EffectiveEmbedEndpoint, EmbedConfig};
use crate::endpoint_pool::{
    EndpointPool, EndpointPoolEndpoint, EndpointPoolEntry, EndpointPoolItem, EndpointPoolStrategy,
};

use super::openai_compat::OpenAiCompatibleEmbedder;
use super::retry::HeartbeatCallback;
use super::{EmbedError, Embedder, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct RoutedEmbeddingResponse {
    pub endpoint_id: String,
    pub endpoint_model: String,
    pub vectors: Vec<Vec<f32>>,
}

#[derive(Debug, Clone)]
pub struct EmbeddingRouter {
    pool: EndpointPool<OpenAiCompatibleEmbedder>,
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
        let items = endpoints
            .into_iter()
            .map(|config| {
                let client = OpenAiCompatibleEmbedder::from_endpoint(&config)?;
                Ok(EndpointPoolItem::new(
                    EndpointPoolEndpoint::new(
                        config.id,
                        config.model,
                        config.priority,
                        config.max_concurrent,
                        Duration::from_secs(config.retry_interval_secs),
                    ),
                    client,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            pool: EndpointPool::new(items),
            dimensions,
            name,
            max_input_tokens,
        })
    }

    pub async fn embed_routed<'a>(
        &self,
        texts: &'a [&'a str],
        heartbeat: Option<&'a HeartbeatCallback>,
    ) -> Result<RoutedEmbeddingResponse> {
        self.pool
            .route(&EmbeddingRoutingStrategy { texts, heartbeat })
            .await
    }

    pub fn endpoint_count(&self) -> usize {
        self.pool.endpoint_count()
    }

    pub fn pool_capacity(&self) -> usize {
        self.pool.pool_capacity()
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

struct EmbeddingRoutingStrategy<'a> {
    texts: &'a [&'a str],
    heartbeat: Option<&'a HeartbeatCallback>,
}

#[async_trait::async_trait]
impl EndpointPoolStrategy<OpenAiCompatibleEmbedder> for EmbeddingRoutingStrategy<'_> {
    type Output = RoutedEmbeddingResponse;
    type Error = EmbedError;

    async fn try_endpoint(
        &self,
        endpoint: &EndpointPoolEntry<OpenAiCompatibleEmbedder>,
    ) -> std::result::Result<Option<Self::Output>, Self::Error> {
        refresh_heartbeat(self.heartbeat)?;
        match endpoint.client().try_embed(self.texts).await? {
            Some(vectors) => {
                crate::embed::global_embed_status()
                    .record_endpoint_success(endpoint.endpoint().id());
                Ok(Some(RoutedEmbeddingResponse {
                    endpoint_id: endpoint.endpoint().id().to_string(),
                    endpoint_model: endpoint.endpoint().model().to_string(),
                    vectors,
                }))
            }
            None => Ok(None),
        }
    }

    async fn wait_for_endpoint(
        &self,
        endpoint: &EndpointPoolEntry<OpenAiCompatibleEmbedder>,
    ) -> std::result::Result<Self::Output, Self::Error> {
        refresh_heartbeat(self.heartbeat)?;
        let vectors = endpoint.client().embed_with_permit(self.texts).await?;
        crate::embed::global_embed_status().record_endpoint_success(endpoint.endpoint().id());
        Ok(RoutedEmbeddingResponse {
            endpoint_id: endpoint.endpoint().id().to_string(),
            endpoint_model: endpoint.endpoint().model().to_string(),
            vectors,
        })
    }

    fn should_try_next(&self, error: &Self::Error) -> bool {
        should_try_next_endpoint(error)
    }

    fn retry_after_for_error(
        &self,
        endpoint: &EndpointPoolEntry<OpenAiCompatibleEmbedder>,
        error: &Self::Error,
    ) -> Option<Duration> {
        match error {
            EmbedError::HttpStatus {
                status: StatusCode::TOO_MANY_REQUESTS,
                retry_after,
                ..
            } => Some(retry_after.unwrap_or_else(|| endpoint.endpoint().retry_interval())),
            EmbedError::TemporarilyUnavailable { retry_after, .. } => Some(*retry_after),
            EmbedError::HttpRequest { .. }
            | EmbedError::Runtime(_)
            | EmbedError::WorkerPanic(_) => Some(endpoint.endpoint().retry_interval()),
            EmbedError::HttpStatus { status, .. } if status.is_server_error() => {
                Some(endpoint.endpoint().retry_interval())
            }
            _ => None,
        }
    }

    fn all_cooling_down_error(&self, endpoint_count: usize, retry_after: Duration) -> Self::Error {
        EmbedError::TemporarilyUnavailable {
            retry_after,
            reason: format!(
                "{endpoint_count} embedding endpoint(s) are cooling down after retryable failures"
            ),
        }
    }

    fn no_endpoint_available_error(&self) -> Self::Error {
        EmbedError::MissingConfiguration("no embedding endpoint is currently available".to_string())
    }

    fn clear_cooldown_on_success(&self) -> bool {
        true
    }

    fn on_cooldown_marked(
        &self,
        endpoint: &EndpointPoolEntry<OpenAiCompatibleEmbedder>,
        retry_after: Duration,
        error: &Self::Error,
    ) {
        crate::embed::global_embed_status().record_endpoint_cooldown(
            endpoint.endpoint().id(),
            retry_after,
            error,
        );
    }

    fn on_cooldown_cleared(&self, endpoint: &EndpointPoolEntry<OpenAiCompatibleEmbedder>) {
        crate::embed::global_embed_status().clear_endpoint_cooldown(endpoint.endpoint().id());
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
