use std::time::Duration;

use reqwest::Url;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::core::config::Config;

use super::{EmbedError, Embedder, Result};

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleEmbedder {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    dimensions: usize,
}

impl OpenAiCompatibleEmbedder {
    pub fn from_config(config: &Config) -> Result<Self> {
        let base_url = config
            .embed
            .resolved_openai_base_url()
            .ok_or_else(|| {
                EmbedError::MissingConfiguration(
                    "embed.openai_compat.base_url (or legacy embed.base_url) is required for backend=openai_compat; example: http://127.0.0.1:18002/v1 or http://gb10:18002/v1".to_string(),
                )
            })?
            .trim_end_matches('/')
            .to_string();
        validate_base_url(&base_url)?;
        let model = config
            .embed
            .resolved_openai_model()
            .ok_or_else(|| {
                EmbedError::MissingConfiguration(
                    "embed.openai_compat.model (or legacy embed.api_model)".to_string(),
                )
            })?
            .to_string();
        let endpoint = format!("{base_url}/embeddings");

        let mut headers = HeaderMap::new();
        if let Some(env_var) = config.embed.resolved_api_key_env() {
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
            .timeout(Duration::from_secs(
                config.embed.openai_compat.request_timeout_secs,
            ))
            .build()
            .map_err(|error| EmbedError::Runtime(error.to_string()))?;

        Ok(Self {
            client,
            endpoint,
            model,
            dimensions: config.embed.resolved_openai_dim(),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
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
            .map_err(|source| EmbedError::HttpRequest {
                endpoint: endpoint.clone(),
                source,
            })?
            .error_for_status()
            .map_err(|source| EmbedError::HttpStatus {
                endpoint: endpoint.clone(),
                source,
            })?
            .json::<OpenAiEmbeddingsResponse>()
            .await
            .map_err(|source| EmbedError::DecodeResponse { endpoint, source })?;

        let vectors = response
            .data
            .into_iter()
            .map(|item| item.embedding)
            .collect::<Vec<_>>();
        validate_vectors(&vectors, self.dimensions())?;
        Ok(vectors)
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn name(&self) -> &str {
        "openai_compat"
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
