use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use tokio::sync::Mutex;

use crate::core::config::{EffectiveLlmEndpoint, LlmConfig};

use super::client::{LlmClient, LlmError, LlmRequest, LlmResponse, MAX_REMOTE_RETRY_HINT};
use super::retry::HeartbeatCallback;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedLlmResponse {
    pub endpoint_id: String,
    pub endpoint_model: String,
    pub response: LlmResponse,
}

#[derive(Debug)]
struct RoutedEndpoint {
    config: EffectiveLlmEndpoint,
    client: LlmClient,
    unavailable_until: Mutex<Option<Instant>>,
}

#[derive(Debug, Clone)]
pub struct LlmRouter {
    endpoints: Arc<Vec<Arc<RoutedEndpoint>>>,
}

impl LlmRouter {
    pub fn from_config(config: &LlmConfig) -> Result<Self, LlmError> {
        let endpoints = config
            .effective_endpoints()
            .map_err(|error| LlmError::MissingConfiguration(error.to_string()))?;
        Self::from_endpoints(endpoints)
    }

    pub fn from_endpoints(endpoints: Vec<EffectiveLlmEndpoint>) -> Result<Self, LlmError> {
        if endpoints.is_empty() {
            return Err(LlmError::MissingConfiguration(
                "llm endpoints must not be empty".to_string(),
            ));
        }
        let mut endpoints = endpoints.into_iter().enumerate().collect::<Vec<_>>();
        endpoints.sort_by_key(|(index, endpoint)| (endpoint.priority, *index));
        let routed = endpoints
            .into_iter()
            .map(|(_, config)| {
                let client = LlmClient::from_endpoint(&config)?;
                Ok(Arc::new(RoutedEndpoint {
                    config,
                    client,
                    unavailable_until: Mutex::new(None),
                }))
            })
            .collect::<Result<Vec<_>, LlmError>>()?;
        Ok(Self {
            endpoints: Arc::new(routed),
        })
    }

    pub async fn chat_completion(
        &self,
        request: &LlmRequest,
        heartbeat: Option<&HeartbeatCallback>,
    ) -> Result<RoutedLlmResponse, LlmError> {
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
            match endpoint.client.try_chat_completion(request).await {
                Ok(Some(response)) => {
                    return Ok(RoutedLlmResponse {
                        endpoint_id: endpoint.config.id.clone(),
                        endpoint_model: endpoint.config.model.clone(),
                        response,
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

        if let Some(endpoint) = first_saturated_endpoint {
            refresh_heartbeat(heartbeat)?;
            let response = endpoint.client.chat_completion(request).await?;
            return Ok(RoutedLlmResponse {
                endpoint_id: endpoint.config.id.clone(),
                endpoint_model: endpoint.config.model.clone(),
                response,
            });
        }

        match (last_retryable, earliest_retry_after) {
            (None, Some(retry_after)) => Err(LlmError::TemporarilyUnavailable {
                retry_after,
                reason: format!(
                    "{} endpoint(s) are cooling down after retryable failures",
                    self.endpoints.len()
                ),
            }),
            (Some(_error), Some(retry_after)) => Err(LlmError::TemporarilyUnavailable {
                retry_after,
                reason: format!(
                    "{} endpoint(s) are cooling down after retryable failures",
                    self.endpoints.len()
                ),
            }),
            (Some(error), _) => Err(error),
            (None, None) => Err(LlmError::MissingConfiguration(
                "no LLM endpoint is currently available".to_string(),
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

impl RoutedEndpoint {
    async fn temporary_unavailable_remaining(&self) -> Option<Duration> {
        let mut guard = self.unavailable_until.lock().await;
        match *guard {
            Some(until) if until > Instant::now() => {
                Some(until.saturating_duration_since(Instant::now()))
            }
            Some(_) => {
                *guard = None;
                None
            }
            None => None,
        }
    }

    fn retry_after_for_error(&self, error: &LlmError) -> Option<Duration> {
        match error {
            LlmError::ClientError {
                status: StatusCode::TOO_MANY_REQUESTS,
                retry_after,
                ..
            } => Some(
                retry_after.unwrap_or_else(|| Duration::from_secs(self.config.retry_interval_secs)),
            ),
            LlmError::TemporarilyUnavailable { retry_after, .. } => Some(*retry_after),
            LlmError::HttpRequest { .. } | LlmError::Timeout => {
                Some(Duration::from_secs(self.config.retry_interval_secs))
            }
            LlmError::HttpStatus { status, .. } if status.is_server_error() => {
                Some(Duration::from_secs(self.config.retry_interval_secs))
            }
            LlmError::HttpStatus { .. }
            | LlmError::ClientError { .. }
            | LlmError::DecodeResponse(_)
            | LlmError::MissingConfiguration(_) => None,
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
}

fn should_try_next_endpoint(error: &LlmError) -> bool {
    match error {
        LlmError::HttpRequest { .. }
        | LlmError::Timeout
        | LlmError::TemporarilyUnavailable { .. } => true,
        LlmError::HttpStatus { status, .. } => status.is_server_error(),
        LlmError::ClientError { status, .. } => *status == StatusCode::TOO_MANY_REQUESTS,
        LlmError::DecodeResponse(_) | LlmError::MissingConfiguration(_) => false,
    }
}

fn refresh_heartbeat(heartbeat: Option<&HeartbeatCallback>) -> Result<(), LlmError> {
    if let Some(callback) = heartbeat {
        callback()?;
    }
    Ok(())
}
