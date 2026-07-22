use std::time::Duration;

use reqwest::StatusCode;

use crate::core::config::{EffectiveLlmEndpoint, LlmConfig, RemoteCallPolicyConfig};
use crate::endpoint_pool::{
    EndpointPool, EndpointPoolEndpoint, EndpointPoolEntry, EndpointPoolItem, EndpointPoolStrategy,
};

use super::client::{LlmClient, LlmError, LlmRequest, LlmResponse};
use super::retry::HeartbeatCallback;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedLlmResponse {
    pub endpoint_id: String,
    pub endpoint_model: String,
    pub response: LlmResponse,
}

#[derive(Debug, Clone)]
pub struct LlmRouter {
    pool: EndpointPool<LlmClient>,
}

impl LlmRouter {
    pub fn from_config(config: &LlmConfig) -> Result<Self, LlmError> {
        let endpoints = config
            .effective_endpoints()
            .map_err(|error| LlmError::MissingConfiguration(error.to_string()))?;
        Self::from_endpoints(endpoints)
    }

    pub fn from_config_with_policy(
        config: &LlmConfig,
        policy: &RemoteCallPolicyConfig,
    ) -> Result<Self, LlmError> {
        crate::core::remote_calls::ensure_llm_allowed_for_policy(policy, config)?;
        Self::from_config(config)
    }

    pub fn from_endpoints(endpoints: Vec<EffectiveLlmEndpoint>) -> Result<Self, LlmError> {
        if endpoints.is_empty() {
            return Err(LlmError::MissingConfiguration(
                "llm endpoints must not be empty".to_string(),
            ));
        }
        let items = endpoints
            .into_iter()
            .map(|config| {
                let client = LlmClient::from_endpoint(&config)?;
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
            .collect::<Result<Vec<_>, LlmError>>()?;
        Ok(Self {
            pool: EndpointPool::new(items),
        })
    }

    pub async fn chat_completion(
        &self,
        request: &LlmRequest,
        heartbeat: Option<&HeartbeatCallback>,
    ) -> Result<RoutedLlmResponse, LlmError> {
        self.pool
            .route(&LlmRoutingStrategy { request, heartbeat })
            .await
    }

    pub fn endpoint_count(&self) -> usize {
        self.pool.endpoint_count()
    }

    pub fn pool_capacity(&self) -> usize {
        self.pool.pool_capacity()
    }
}

struct LlmRoutingStrategy<'a> {
    request: &'a LlmRequest,
    heartbeat: Option<&'a HeartbeatCallback>,
}

#[async_trait::async_trait]
impl EndpointPoolStrategy<LlmClient> for LlmRoutingStrategy<'_> {
    type Output = RoutedLlmResponse;
    type Error = LlmError;

    async fn try_endpoint(
        &self,
        endpoint: &EndpointPoolEntry<LlmClient>,
    ) -> Result<Option<Self::Output>, Self::Error> {
        refresh_heartbeat(self.heartbeat)?;
        match endpoint.client().try_chat_completion(self.request).await? {
            Some(response) => Ok(Some(RoutedLlmResponse {
                endpoint_id: endpoint.endpoint().id().to_string(),
                endpoint_model: endpoint.endpoint().model().to_string(),
                response,
            })),
            None => Ok(None),
        }
    }

    async fn wait_for_endpoint(
        &self,
        endpoint: &EndpointPoolEntry<LlmClient>,
    ) -> Result<Self::Output, Self::Error> {
        refresh_heartbeat(self.heartbeat)?;
        let response = endpoint.client().chat_completion(self.request).await?;
        Ok(RoutedLlmResponse {
            endpoint_id: endpoint.endpoint().id().to_string(),
            endpoint_model: endpoint.endpoint().model().to_string(),
            response,
        })
    }

    fn should_try_next(&self, error: &Self::Error) -> bool {
        should_try_next_endpoint(error)
    }

    fn retry_after_for_error(
        &self,
        endpoint: &EndpointPoolEntry<LlmClient>,
        error: &Self::Error,
    ) -> Option<Duration> {
        match error {
            LlmError::ClientError {
                status: StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT,
                retry_after,
                ..
            } => Some(retry_after.unwrap_or_else(|| endpoint.endpoint().retry_interval())),
            LlmError::TemporarilyUnavailable { retry_after, .. } => Some(*retry_after),
            LlmError::HttpRequest { .. } | LlmError::Timeout => {
                Some(endpoint.endpoint().retry_interval())
            }
            LlmError::HttpStatus { status, .. } if status.is_server_error() => {
                Some(endpoint.endpoint().retry_interval())
            }
            LlmError::HttpStatus { .. }
            | LlmError::ClientError { .. }
            | LlmError::DecodeResponse(_)
            | LlmError::MissingConfiguration(_)
            | LlmError::RemoteCallPolicy(_) => None,
        }
    }

    fn all_cooling_down_error(
        &self,
        endpoint_count: usize,
        retry_after: Duration,
        first_retryable_error: Option<&Self::Error>,
    ) -> Self::Error {
        LlmError::TemporarilyUnavailable {
            retry_after,
            reason: format!(
                "{endpoint_count} endpoint(s) are cooling down after retryable failures"
            ),
            http_status: first_retryable_error.and_then(LlmError::http_status),
        }
    }

    fn no_endpoint_available_error(&self) -> Self::Error {
        LlmError::MissingConfiguration("no LLM endpoint is currently available".to_string())
    }
}

fn should_try_next_endpoint(error: &LlmError) -> bool {
    match error {
        LlmError::HttpRequest { .. }
        | LlmError::Timeout
        | LlmError::TemporarilyUnavailable { .. } => true,
        LlmError::HttpStatus { status, .. } => status.is_server_error(),
        LlmError::ClientError { status, .. } => {
            matches!(
                *status,
                StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT
            )
        }
        LlmError::DecodeResponse(_)
        | LlmError::MissingConfiguration(_)
        | LlmError::RemoteCallPolicy(_) => false,
    }
}

fn refresh_heartbeat(heartbeat: Option<&HeartbeatCallback>) -> Result<(), LlmError> {
    if let Some(callback) = heartbeat {
        callback()?;
    }
    Ok(())
}
