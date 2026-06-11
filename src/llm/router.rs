use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use tokio::sync::Mutex;

use crate::core::config::{EffectiveLlmEndpoint, LlmConfig};

use super::client::{LlmClient, LlmError, LlmRequest, LlmResponse};
use super::retry::HeartbeatCallback;

const MAX_RETRY_AFTER_SECS: u64 = 60;

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
        let routed = endpoints
            .into_iter()
            .map(|config| {
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
        let mut last_non_cooldown_retryable = None;
        let mut earliest_retry_after: Option<Duration> = None;
        for endpoint in self.endpoints.iter() {
            if let Some(retry_after) = endpoint.temporary_unavailable_remaining().await {
                earliest_retry_after = Some(match earliest_retry_after {
                    Some(current) => current.min(retry_after),
                    None => retry_after,
                });
                continue;
            }
            refresh_heartbeat(heartbeat)?;
            match endpoint.client.chat_completion(request).await {
                Ok(response) => {
                    return Ok(RoutedLlmResponse {
                        endpoint_id: endpoint.config.id.clone(),
                        endpoint_model: endpoint.config.model.clone(),
                        response,
                    });
                }
                Err(error) if should_try_next_endpoint(&error) => {
                    if let LlmError::ClientError {
                        status: StatusCode::TOO_MANY_REQUESTS,
                        retry_after,
                        ..
                    } = &error
                    {
                        let retry_after = endpoint.mark_temporarily_unavailable(*retry_after).await;
                        earliest_retry_after = Some(match earliest_retry_after {
                            Some(current) => current.min(retry_after),
                            None => retry_after,
                        });
                    } else {
                        last_non_cooldown_retryable = Some(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
        match (last_non_cooldown_retryable, earliest_retry_after) {
            (Some(error), _) => Err(error),
            (None, Some(retry_after)) => Err(LlmError::TemporarilyUnavailable {
                retry_after,
                reason: format!(
                    "{} endpoint(s) are cooling down after retryable failures",
                    self.endpoints.len()
                ),
            }),
            (None, None) => Err(LlmError::MissingConfiguration(
                "no LLM endpoint is currently available".to_string(),
            )),
        }
    }

    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
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

    async fn mark_temporarily_unavailable(&self, retry_after: Option<Duration>) -> Duration {
        let retry_after = retry_after
            .unwrap_or_else(|| Duration::from_secs(MAX_RETRY_AFTER_SECS))
            .min(Duration::from_secs(MAX_RETRY_AFTER_SECS));
        let mut guard = self.unavailable_until.lock().await;
        *guard = Some(Instant::now() + retry_after);
        retry_after
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
