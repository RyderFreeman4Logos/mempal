#![warn(clippy::all)]

use std::path::PathBuf;
use std::time::Duration;

use crate::core::config::Config;
use thiserror::Error;

pub mod alerting;
pub mod api;
pub mod factory;
#[cfg(feature = "model2vec")]
pub mod model2vec;
#[cfg(feature = "onnx")]
pub mod onnx;
pub mod openai_compat;
pub mod retry;
pub mod router;
pub mod status;
pub mod stub;

pub use factory::{
    ConfiguredEmbedderFactory, EmbedderFactory, SharedEmbedderRuntimeSnapshot,
    shared_embedder_runtime_snapshot,
};
pub use router::EmbeddingRouter;
pub use status::{EmbedHealthSnapshot, EmbedStatus, global_embed_status};

pub type Result<T> = std::result::Result<T, EmbedError>;

#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("failed to create model directory {path}")]
    CreateModelDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to check whether {path} exists")]
    CheckPathExists {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to download {url}")]
    Download {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("download returned error status for {url}")]
    DownloadStatus {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("failed to read download body from {url}")]
    ReadDownloadBody {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("failed to write {path}")]
    WriteFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to rename {from} to {to}")]
    RenameFile {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to initialize ONNX session builder: {0}")]
    SessionBuilder(String),
    #[error("failed to load ONNX model from {path}: {message}")]
    LoadModel { path: PathBuf, message: String },
    #[error("tokenizer error: {0}")]
    Tokenizer(String),
    #[error("embedding runtime error: {0}")]
    Runtime(String),
    #[error("{0}")]
    RemoteCallPolicy(#[from] crate::core::remote_calls::RemoteCallPolicyError),
    #[error("embedding worker panicked")]
    WorkerPanic(#[source] tokio::task::JoinError),
    #[error("failed to call embedding endpoint {endpoint}")]
    HttpRequest {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("embedding endpoint returned error status {status} from {endpoint}")]
    HttpStatus {
        endpoint: String,
        status: reqwest::StatusCode,
        retry_after: Option<Duration>,
    },
    #[error("failed to decode embedding response from {endpoint}")]
    DecodeResponse {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("invalid embedding response: {0}")]
    InvalidResponse(String),
    #[error("embedding endpoint returned no vectors")]
    EmptyVectors,
    #[error(
        "embedding endpoint returned vectors with unexpected dimensions; expected {expected}, got {actual}"
    )]
    InvalidDimensions { expected: usize, actual: usize },
    #[error("unsupported embed backend: {0}")]
    UnsupportedBackend(String),
    #[error("missing embed configuration: {0}")]
    MissingConfiguration(String),
    #[error("invalid embed configuration: {0}")]
    InvalidConfiguration(String),
    #[error("embedding endpoints are temporarily unavailable: {reason}")]
    TemporarilyUnavailable {
        retry_after: Duration,
        reason: String,
    },
    #[error("failed to read embed API key from env var {var}")]
    ReadApiKeyEnv {
        var: String,
        #[source]
        source: std::env::VarError,
    },
}

impl EmbedError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::HttpRequest { source, .. } => source.status().is_none_or(is_retryable_status),
            Self::HttpStatus { status, .. } => is_retryable_status(*status),
            Self::DownloadStatus { source, .. } => source.status().is_some_and(is_retryable_status),
            Self::Download { .. }
            | Self::ReadDownloadBody { .. }
            | Self::Runtime(_)
            | Self::WorkerPanic(_)
            | Self::TemporarilyUnavailable { .. } => true,
            Self::CreateModelDir { .. }
            | Self::CheckPathExists { .. }
            | Self::WriteFile { .. }
            | Self::RenameFile { .. }
            | Self::SessionBuilder(_)
            | Self::LoadModel { .. }
            | Self::Tokenizer(_)
            | Self::DecodeResponse { .. }
            | Self::InvalidResponse(_)
            | Self::EmptyVectors
            | Self::InvalidDimensions { .. }
            | Self::UnsupportedBackend(_)
            | Self::MissingConfiguration(_)
            | Self::InvalidConfiguration(_)
            | Self::RemoteCallPolicy(_)
            | Self::ReadApiKeyEnv { .. } => false,
        }
    }

    pub fn endpoint(&self) -> Option<&str> {
        match self {
            Self::HttpRequest { endpoint, .. }
            | Self::HttpStatus { endpoint, .. }
            | Self::DecodeResponse { endpoint, .. } => Some(endpoint.as_str()),
            _ => None,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::HttpStatus { retry_after, .. } => *retry_after,
            Self::TemporarilyUnavailable { retry_after, .. } => Some(*retry_after),
            _ => None,
        }
    }
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn dimensions(&self) -> usize;
    fn name(&self) -> &str;

    /// Backend-advertised maximum input tokens. `None` means unknown/unlimited.
    /// Used by the chunker to clamp effective max below this limit.
    fn max_input_tokens(&self) -> Option<usize> {
        None
    }

    /// Estimate token count for `text`. Backends with a local tokenizer
    /// should override for accuracy. The default uses a conservative
    /// heuristic: `ceil(chars / 2.5)` — safe upper bound for most
    /// tokenizers including CJK-heavy and base64-dense content.
    fn estimate_tokens(&self, text: &str) -> usize {
        estimate_tokens(text)
    }
}

/// Standalone token estimator: `ceil(chars / 2.5)`, CJK-safe.
/// Use this wherever an `Embedder` instance is not available.
pub fn estimate_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    (chars * 2).div_ceil(5)
}

pub async fn from_config(config: &Config) -> Result<Box<dyn Embedder>> {
    let primary_backend = build_backend_from_name(config, config.embed.backend.as_str()).await?;
    let fallback_backend = match config.embed.fallback.as_deref() {
        Some(name) if name.eq_ignore_ascii_case(config.embed.backend.as_str()) => None,
        Some(name) => Some(build_backend_from_name(config, name).await?),
        None => None,
    };

    Ok(Box::new(ManagedEmbedder::new(
        primary_backend,
        fallback_backend,
    )))
}

pub async fn build_backend_from_name(config: &Config, backend: &str) -> Result<Box<dyn Embedder>> {
    match backend {
        #[cfg(feature = "model2vec")]
        "model2vec" => {
            let model_id = config
                .embed
                .model
                .as_deref()
                .unwrap_or("minishlab/potion-multilingual-128M");
            Ok(Box::new(model2vec::Model2VecEmbedder::new(model_id).await?))
        }
        #[cfg(feature = "onnx")]
        "onnx" => Ok(Box::new(onnx::OnnxEmbedder::new_or_download().await?)),
        "openai_compat" | "api" => {
            crate::core::remote_calls::ensure_embedding_allowed(config)?;
            Ok(Box::new(router::EmbeddingRouter::from_config(
                &config.embed,
            )?))
        }
        "stub" => {
            let dim = config
                .embed
                .openai_compat
                .dim
                .unwrap_or(stub::DEFAULT_STUB_DIM);
            Ok(Box::new(stub::StubEmbedder::new(dim)))
        }
        other => Err(EmbedError::UnsupportedBackend(other.to_string())),
    }
}

#[cfg(any(feature = "model2vec", feature = "onnx", test))]
pub(crate) async fn run_blocking_embedder_initialization<T>(
    initialize: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(initialize)
        .await
        .map_err(EmbedError::WorkerPanic)?
}

struct ManagedEmbedder {
    primary: Box<dyn Embedder>,
    fallback: Option<Box<dyn Embedder>>,
}

impl ManagedEmbedder {
    fn new(primary: Box<dyn Embedder>, fallback: Option<Box<dyn Embedder>>) -> Self {
        Self { primary, fallback }
    }
}

#[async_trait::async_trait]
impl Embedder for ManagedEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let status = global_embed_status();
        if let Some(fallback) = &self.fallback {
            match self.primary.embed(texts).await {
                Ok(vectors) => {
                    status.record_primary_success();
                    Ok(vectors)
                }
                Err(primary_error) => {
                    status.record_failure(&primary_error);
                    let message = format!(
                        "embedder fallback active: {} failed, using {}",
                        self.primary.name(),
                        fallback.name()
                    );
                    let vectors = fallback.embed(texts).await?;
                    status.record_fallback_success(message);
                    Ok(vectors)
                }
            }
        } else {
            let vectors = retry::retry_embed_operation(status, None, || async {
                self.primary.embed(texts).await
            })
            .await?;
            status.record_primary_success();
            Ok(vectors)
        }
    }

    fn dimensions(&self) -> usize {
        self.primary.dimensions()
    }

    fn name(&self) -> &str {
        self.primary.name()
    }

    fn max_input_tokens(&self) -> Option<usize> {
        self.primary.max_input_tokens()
    }

    fn estimate_tokens(&self, text: &str) -> usize {
        self.primary.estimate_tokens(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::{Config, EmbedEndpointConfig};

    #[tokio::test]
    async fn blocking_embedder_initialization_allows_a_deadline_to_fire() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut initialization = tokio::spawn(async move {
            run_blocking_embedder_initialization(move || {
                started_tx
                    .send(())
                    .expect("report that blocking initialization started");
                release_rx.recv().expect("release blocking initialization");
                Ok::<(), EmbedError>(())
            })
            .await
        });

        started_rx
            .await
            .expect("blocking initialization should start before its deadline");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut initialization)
                .await
                .is_err(),
            "the deadline must fire while synchronous initialization is still blocked"
        );

        release_tx
            .send(())
            .expect("release blocking initialization after deadline");
        initialization
            .await
            .expect("initialization task should not panic")
            .expect("blocking initialization should finish after release");
    }

    #[tokio::test]
    async fn remote_fallback_backend_is_blocked_before_router_construction() {
        let mut config = Config::default();
        config.embed.backend = "model2vec".to_string();
        config.embed.fallback = Some("openai_compat".to_string());
        config.embed.endpoints.push(EmbedEndpointConfig {
            id: Some("remote-fallback".to_string()),
            backend: Some("openai_compat".to_string()),
            base_url: Some("https://api.openai.com/v1/private-fallback-path".to_string()),
            model: Some("text-embedding-3-large".to_string()),
            ..Default::default()
        });
        config.privacy.remote_calls.fail_closed = true;

        let error = match build_backend_from_name(&config, "openai_compat").await {
            Ok(_) => panic!("remote fallback backend should be blocked by fail-closed policy"),
            Err(error) => error,
        };

        match error {
            EmbedError::RemoteCallPolicy(policy) => {
                assert_eq!(policy.service, "embedding");
                assert_eq!(policy.allow_field, "allow_embedding");
                assert_eq!(
                    policy.endpoint,
                    crate::core::remote_calls::BLOCKED_REMOTE_ENDPOINT_LABEL
                );
            }
            other => panic!("expected remote call policy error, got {other}"),
        }
    }
}
