use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use reqwest::Url;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, Semaphore};

use crate::core::config::{Config, EffectiveEmbedEndpoint};

use super::{EmbedError, Embedder, Result};

pub(crate) const MAX_REMOTE_RETRY_HINT: Duration = Duration::from_secs(60);
const MAX_REMOTE_RETRY_HINT_SECS: u64 = 60;

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleEmbedder {
    client: reqwest::Client,
    id: String,
    base_url: String,
    endpoint: String,
    model: String,
    dimensions: usize,
    max_input_tokens: Option<usize>,
    semaphore: Arc<Semaphore>,
    current_max: Arc<AtomicUsize>,
    concurrency_update_lock: Arc<Mutex<()>>,
}

impl OpenAiCompatibleEmbedder {
    pub fn from_config(config: &Config) -> Result<Self> {
        let endpoint = config
            .embed
            .effective_endpoints()
            .map_err(|error| EmbedError::InvalidConfiguration(error.to_string()))?
            .into_iter()
            .next()
            .ok_or_else(|| {
                EmbedError::MissingConfiguration(
                    "embed.openai_compat.base_url (or legacy embed.base_url) is required for backend=openai_compat; example: http://127.0.0.1:18002/v1 or http://gb10:18002/v1".to_string(),
                )
            })?;
        Self::from_endpoint(&endpoint)
    }

    pub fn from_endpoint(endpoint: &EffectiveEmbedEndpoint) -> Result<Self> {
        let base_url = endpoint.base_url.trim_end_matches('/').to_string();
        validate_base_url(&base_url)?;
        let request_endpoint = format!("{base_url}/embeddings");

        let mut headers = HeaderMap::new();
        if let Some(env_var) = endpoint.api_key_env.as_deref() {
            let api_key = std::env::var(env_var).map_err(|source| EmbedError::ReadApiKeyEnv {
                var: env_var.to_string(),
                source,
            })?;
            let header_value = HeaderValue::from_str(&format!("Bearer {api_key}"))
                .map_err(|error| EmbedError::Runtime(error.to_string()))?;
            headers.insert(AUTHORIZATION, header_value);
        }

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(endpoint.request_timeout_secs))
            .build()
            .map_err(|error| EmbedError::Runtime(error.to_string()))?;
        let max_concurrent = endpoint.max_concurrent.max(1);

        Ok(Self {
            client,
            id: endpoint.id.clone(),
            base_url,
            endpoint: request_endpoint,
            model: endpoint.model.clone(),
            dimensions: endpoint.dimensions,
            max_input_tokens: endpoint.max_input_tokens,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            current_max: Arc::new(AtomicUsize::new(max_concurrent)),
            concurrency_update_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn current_max_concurrent(&self) -> usize {
        self.current_max.load(Ordering::SeqCst)
    }

    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
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

    pub async fn embed_with_permit(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");
        self.send_embed(texts).await
    }

    pub async fn try_embed(&self, texts: &[&str]) -> Result<Option<Vec<Vec<f32>>>> {
        let permit = match self.semaphore.try_acquire() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => return Ok(None),
            Err(tokio::sync::TryAcquireError::Closed) => {
                return Err(EmbedError::Runtime(
                    "embedding endpoint semaphore closed".to_string(),
                ));
            }
        };
        let response = self.send_embed(texts).await;
        drop(permit);
        response.map(Some)
    }

    async fn send_embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let endpoint = self.endpoint().to_string();
        let response = self
            .client
            .post(self.endpoint())
            .json(&OpenAiEmbeddingsRequest {
                input: texts,
                model: &self.model,
            })
            .send()
            .await
            .map_err(map_reqwest_error(endpoint.clone()))?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(response.headers());
            let body = response
                .text()
                .await
                .map_err(map_reqwest_error(endpoint.clone()))?;
            let retry_after = retry_after.or_else(|| parse_reset_seconds(&body));
            return Err(EmbedError::HttpStatus {
                endpoint,
                status,
                retry_after,
            });
        }

        let response = response
            .json::<OpenAiEmbeddingsResponse>()
            .await
            .map_err(map_decode_error(endpoint.clone()))?;

        let vectors = response
            .data
            .into_iter()
            .map(|item| item.embedding)
            .collect::<Vec<_>>();
        validate_vectors(&vectors, self.dimensions())?;
        Ok(vectors)
    }
}

fn validate_base_url(base_url: &str) -> Result<()> {
    let parsed = Url::parse(base_url).map_err(|error| {
        EmbedError::InvalidConfiguration(format!("invalid embed base_url `{base_url}`: {error}"))
    })?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(EmbedError::InvalidConfiguration(
            "embed base_url must not include userinfo credentials; use api_key_env instead"
                .to_string(),
        ));
    }
    if parsed.query().is_some() {
        return Err(EmbedError::InvalidConfiguration(
            "embed base_url must not include query parameters; move secrets to api_key_env"
                .to_string(),
        ));
    }
    if parsed.fragment().is_some() {
        return Err(EmbedError::InvalidConfiguration(
            "embed base_url must not include URL fragments".to_string(),
        ));
    }
    Ok(())
}

#[async_trait::async_trait]
impl Embedder for OpenAiCompatibleEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed_with_permit(texts).await
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn name(&self) -> &str {
        "openai_compat"
    }

    fn max_input_tokens(&self) -> Option<usize> {
        self.max_input_tokens
    }
}

#[derive(Debug, Serialize)]
struct OpenAiEmbeddingsRequest<'a> {
    input: &'a [&'a str],
    model: &'a str,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingsResponse {
    data: Vec<OpenAiEmbeddingItem>,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingItem {
    embedding: Vec<f32>,
}

fn validate_vectors(vectors: &[Vec<f32>], expected_dimensions: usize) -> Result<()> {
    if vectors.is_empty() {
        return Err(EmbedError::EmptyVectors);
    }

    if let Some(actual) = vectors
        .iter()
        .map(Vec::len)
        .find(|length| *length != expected_dimensions)
    {
        return Err(EmbedError::InvalidDimensions {
            expected: expected_dimensions,
            actual,
        });
    }

    Ok(())
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

fn map_reqwest_error(endpoint: String) -> impl FnOnce(reqwest::Error) -> EmbedError {
    move |source| EmbedError::HttpRequest { endpoint, source }
}

fn map_decode_error(endpoint: String) -> impl FnOnce(reqwest::Error) -> EmbedError {
    move |source| EmbedError::DecodeResponse { endpoint, source }
}
