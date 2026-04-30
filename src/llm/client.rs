use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::Url;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Semaphore;

use crate::core::config::LlmConfig;

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
    #[error("LLM endpoint returned error status {status}: {body}")]
    HttpStatus { status: StatusCode, body: String },
    #[error("failed to decode LLM response: {0}")]
    DecodeResponse(String),
    #[error("LLM endpoint returned client error status {status}: {body}")]
    ClientError {
        status: StatusCode,
        body: String,
        retry_after: Option<Duration>,
    },
    #[error("LLM request timed out")]
    Timeout,
    #[error("missing LLM configuration: {0}")]
    MissingConfiguration(String),
}

impl LlmError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::HttpRequest { .. } | Self::Timeout => true,
            Self::HttpStatus { status, .. } => status.is_server_error(),
            Self::ClientError { status, .. } => *status == StatusCode::TOO_MANY_REQUESTS,
            Self::DecodeResponse(_) | Self::MissingConfiguration(_) => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    semaphore: Arc<Semaphore>,
    current_max: Arc<AtomicUsize>,
}

impl LlmClient {
    pub fn from_config(config: &LlmConfig) -> Result<Self, LlmError> {
        let base_url = config
            .base_url
            .as_deref()
            .filter(|base_url| !base_url.trim().is_empty())
            .ok_or_else(|| {
                LlmError::MissingConfiguration(
                    "llm.base_url is required for backend=openai_compat; example: http://127.0.0.1:8317/v1"
                        .to_string(),
                )
            })?
            .trim_end_matches('/')
            .to_string();
        validate_base_url(&base_url)?;

        let model = config
            .model
            .as_deref()
            .filter(|model| !model.trim().is_empty())
            .ok_or_else(|| LlmError::MissingConfiguration("llm.model".to_string()))?
            .to_string();

        let mut headers = HeaderMap::new();
        let resolved_key =
            resolve_api_key(config.api_key.as_deref(), config.api_key_env.as_deref())?;
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
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .map_err(|source| LlmError::HttpRequest { source })?;

        let max_concurrent = config.max_concurrent.max(1);
        Ok(Self {
            http,
            base_url,
            model,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            current_max: Arc::new(AtomicUsize::new(max_concurrent)),
        })
    }

    pub async fn update_concurrency(&self, new_max: usize) {
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
        let endpoint = format!("{}/chat/completions", self.base_url);
        let model = request.model.as_deref().unwrap_or(&self.model);
        let response = self
            .http
            .post(&endpoint)
            .json(&OpenAiChatRequest {
                model,
                messages: &request.messages,
                temperature: request.temperature,
                max_tokens: request.max_tokens,
            })
            .send()
            .await
            .map_err(map_reqwest_error)?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(response.headers());
            let body = response.text().await.map_err(map_reqwest_error)?;
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
        let content = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| {
                LlmError::DecodeResponse(
                    "chat completion response did not include any choices".to_string(),
                )
            })?
            .message
            .content;

        Ok(LlmResponse {
            content,
            usage: response.usage,
            model: response.model,
        })
    }
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
    let parsed = Url::parse(base_url).map_err(|error| {
        LlmError::MissingConfiguration(format!("invalid llm.base_url `{base_url}`: {error}"))
    })?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(LlmError::MissingConfiguration(
            "llm.base_url must not include userinfo credentials; use api_key_env instead"
                .to_string(),
        ));
    }
    if parsed.query().is_some() {
        return Err(LlmError::MissingConfiguration(
            "llm.base_url must not include query parameters; move secrets to api_key_env"
                .to_string(),
        ));
    }
    if parsed.fragment().is_some() {
        return Err(LlmError::MissingConfiguration(
            "llm.base_url must not include URL fragments".to_string(),
        ));
    }
    Ok(())
}

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
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

#[derive(Debug, Serialize)]
struct OpenAiChatRequest<'a> {
    model: &'a str,
    messages: &'a [LlmMessage],
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
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
    content: String,
}
