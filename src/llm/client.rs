use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex, Semaphore};

use crate::core::config::{EffectiveLlmEndpoint, LlmConfig, validate_llm_base_url};

const MAX_REMOTE_RETRY_HINT_SECS: u64 = 60;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmRequest {
    pub messages: Vec<LlmMessage>,
    pub model: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmResponse {
    pub content: String,
    pub usage: Option<Usage>,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("failed to call LLM endpoint")]
    HttpRequest {
        #[source]
        source: reqwest::Error,
    },
    #[error("LLM endpoint returned error status {status}")]
    HttpStatus { status: StatusCode, body: String },
    #[error("failed to decode LLM response: {0}")]
    DecodeResponse(String),
    #[error("LLM endpoint returned client error status {status}")]
    ClientError {
        status: StatusCode,
        body: String,
        retry_after: Option<Duration>,
    },
    #[error("LLM request timed out")]
    Timeout,
    #[error("all LLM endpoints are temporarily unavailable: {reason}")]
    TemporarilyUnavailable {
        retry_after: Duration,
        reason: String,
    },
    #[error("missing LLM configuration: {0}")]
    MissingConfiguration(String),
    #[error("{0}")]
    RemoteCallPolicy(#[from] crate::core::remote_calls::RemoteCallPolicyError),
}

impl LlmError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::HttpRequest { .. } | Self::Timeout | Self::TemporarilyUnavailable { .. } => true,
            Self::HttpStatus { status, .. } => status.is_server_error(),
            Self::ClientError { status, .. } => {
                matches!(
                    *status,
                    StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT
                )
            }
            Self::DecodeResponse(_) | Self::MissingConfiguration(_) | Self::RemoteCallPolicy(_) => {
                false
            }
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::ClientError { retry_after, .. } => *retry_after,
            Self::TemporarilyUnavailable { retry_after, .. } => Some(*retry_after),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    extra_body: Option<Value>,
    semaphore: Arc<Semaphore>,
    current_max: Arc<AtomicUsize>,
    concurrency_update_lock: Mutex<()>,
}

impl LlmClient {
    pub fn from_config(config: &LlmConfig) -> Result<Self, LlmError> {
        let mut endpoints = config
            .effective_endpoints()
            .map_err(|error| LlmError::MissingConfiguration(error.to_string()))?;
        let endpoint = if endpoints.is_empty() {
            return Err(LlmError::MissingConfiguration(
                "llm.base_url is required for backend=openai_compat; example: http://127.0.0.1:8317/v1"
                    .to_string(),
            ));
        } else {
            endpoints.remove(0)
        };
        Self::from_endpoint(&endpoint)
    }

    pub fn from_endpoint(endpoint: &EffectiveLlmEndpoint) -> Result<Self, LlmError> {
        validate_base_url(&endpoint.base_url)?;

        let mut headers = HeaderMap::new();
        let resolved_key =
            resolve_api_key(endpoint.api_key.as_deref(), endpoint.api_key_env.as_deref())?;
        if let Some(api_key) = resolved_key {
            let header_value =
                HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|error| {
                    LlmError::MissingConfiguration(format!(
                        "llm api_key produced an invalid Authorization header: {error}"
                    ))
                })?;
            headers.insert(AUTHORIZATION, header_value);
        }

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(endpoint.request_timeout_secs))
            .build()
            .map_err(|source| LlmError::HttpRequest { source })?;

        let max_concurrent = endpoint.max_concurrent.max(1);
        Ok(Self {
            http,
            base_url: endpoint.base_url.clone(),
            model: endpoint.model.clone(),
            extra_body: endpoint.extra_body.clone(),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            current_max: Arc::new(AtomicUsize::new(max_concurrent)),
            concurrency_update_lock: Mutex::new(()),
        })
    }

    pub async fn update_concurrency(&self, new_max: usize) {
        let _guard = self.concurrency_update_lock.lock().await;
        let new_max = new_max.max(1);
        let old_max = self.current_max.load(Ordering::SeqCst);
        if new_max == old_max {
            return;
        }
        if new_max > old_max {
            self.semaphore.add_permits(new_max - old_max);
        } else {
            let diff = (old_max - new_max) as u32;
            let permit = self
                .semaphore
                .acquire_many(diff)
                .await
                .expect("semaphore closed");
            permit.forget();
        }
        self.current_max.store(new_max, Ordering::SeqCst);
    }

    pub fn current_max_concurrent(&self) -> usize {
        self.current_max.load(Ordering::SeqCst)
    }

    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    pub async fn chat_completion(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");
        self.send_chat_completion(request).await
    }

    pub async fn try_chat_completion(
        &self,
        request: &LlmRequest,
    ) -> Result<Option<LlmResponse>, LlmError> {
        let permit = match self.semaphore.try_acquire() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => return Ok(None),
            Err(tokio::sync::TryAcquireError::Closed) => {
                return Err(LlmError::MissingConfiguration(
                    "llm endpoint semaphore closed".to_string(),
                ));
            }
        };
        let response = self.send_chat_completion(request).await;
        drop(permit);
        response.map(Some)
    }

    async fn send_chat_completion(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let endpoint = format!("{}/chat/completions", self.base_url);
        let model = request.model.as_deref().unwrap_or(&self.model);
        let mut body = serde_json::Map::new();
        body.insert("model".to_string(), Value::String(model.to_string()));
        body.insert(
            "messages".to_string(),
            serde_json::to_value(&request.messages)
                .map_err(|error| LlmError::DecodeResponse(error.to_string()))?,
        );
        if let Some(temperature) = request.temperature {
            let temperature = serde_json::Number::from_f64(temperature).ok_or_else(|| {
                LlmError::DecodeResponse("temperature must be finite".to_string())
            })?;
            body.insert("temperature".to_string(), Value::Number(temperature));
        }
        if let Some(max_tokens) = request.max_tokens {
            body.insert(
                "max_tokens".to_string(),
                Value::Number(serde_json::Number::from(max_tokens)),
            );
        }
        merge_extra_body(&mut body, self.extra_body.as_ref())?;
        let response = self
            .http
            .post(&endpoint)
            .json(&Value::Object(body))
            .send()
            .await
            .map_err(map_reqwest_error)?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(response.headers());
            let body = response.text().await.map_err(map_reqwest_error)?;
            let retry_after = retry_after.or_else(|| parse_reset_seconds(&body));
            if status.is_client_error() {
                return Err(LlmError::ClientError {
                    status,
                    body,
                    retry_after,
                });
            }
            return Err(LlmError::HttpStatus { status, body });
        }

        let response = response
            .json::<OpenAiChatResponse>()
            .await
            .map_err(map_decode_error)?;
        let message = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| {
                LlmError::DecodeResponse(
                    "chat completion response did not include any choices".to_string(),
                )
            })?
            .message;
        let content = message.content.or(message.reasoning).unwrap_or_default();

        Ok(LlmResponse {
            content,
            usage: response.usage,
            model: response.model,
        })
    }
}

fn merge_extra_body(
    body: &mut serde_json::Map<String, Value>,
    extra_body: Option<&Value>,
) -> Result<(), LlmError> {
    let Some(extra_body) = extra_body else {
        return Ok(());
    };
    let Value::Object(extra) = extra_body else {
        return Err(LlmError::MissingConfiguration(
            "llm.extra_body must be a JSON object".to_string(),
        ));
    };
    for (key, value) in extra {
        if matches!(
            key.as_str(),
            "model" | "messages" | "temperature" | "max_tokens"
        ) {
            continue;
        }
        body.insert(key.clone(), value.clone());
    }
    Ok(())
}

fn resolve_api_key(
    direct: Option<&str>,
    env_name: Option<&str>,
) -> Result<Option<String>, LlmError> {
    if let Some(key) = direct.filter(|k| !k.trim().is_empty()) {
        return Ok(Some(key.to_string()));
    }
    read_api_key(env_name)
}

fn read_api_key(api_key_env: Option<&str>) -> Result<Option<String>, LlmError> {
    let Some(env_var) = api_key_env.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    match std::env::var(env_var) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(LlmError::MissingConfiguration(format!(
            "llm.api_key_env `{env_var}` is not valid unicode"
        ))),
    }
}

fn validate_base_url(base_url: &str) -> Result<(), LlmError> {
    validate_llm_base_url(base_url).map_err(LlmError::MissingConfiguration)
}

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(clamp_remote_retry_hint_secs)
}

fn parse_reset_seconds(body: &str) -> Option<Duration> {
    let value = serde_json::from_str::<Value>(body).ok()?;
    find_reset_seconds(&value).map(clamp_remote_retry_hint_secs)
}

fn clamp_remote_retry_hint_secs(secs: u64) -> Duration {
    Duration::from_secs(secs.min(MAX_REMOTE_RETRY_HINT_SECS))
}

fn find_reset_seconds(value: &Value) -> Option<u64> {
    match value {
        Value::Object(map) => {
            if let Some(reset) = map.get("reset_seconds").and_then(value_as_u64) {
                return Some(reset);
            }
            map.values().find_map(find_reset_seconds)
        }
        Value::Array(values) => values.iter().find_map(find_reset_seconds),
        _ => None,
    }
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse::<u64>().ok()))
}

fn map_reqwest_error(source: reqwest::Error) -> LlmError {
    if source.is_timeout() {
        LlmError::Timeout
    } else {
        LlmError::HttpRequest { source }
    }
}

fn map_decode_error(source: reqwest::Error) -> LlmError {
    if source.is_timeout() {
        LlmError::Timeout
    } else {
        LlmError::DecodeResponse(source.to_string())
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiChatResponse {
    choices: Vec<OpenAiChoice>,
    usage: Option<Usage>,
    model: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAiMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
}
