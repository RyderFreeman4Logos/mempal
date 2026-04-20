#![warn(clippy::all)]

use std::path::PathBuf;

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
pub mod status;

pub use factory::{ConfiguredEmbedderFactory, EmbedderFactory};
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
    #[error("embedding worker panicked")]
    WorkerPanic(#[source] tokio::task::JoinError),
    #[error("failed to call embedding endpoint {endpoint}")]
    HttpRequest {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("embedding endpoint returned error status {endpoint}")]
    HttpStatus {
        endpoint: String,
        #[source]
        source: reqwest::Error,
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
    #[error("failed to read embed API key from env var {var}")]
    ReadApiKeyEnv {
        var: String,
        #[source]
        source: std::env::VarError,
    },
}

#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn dimensions(&self) -> usize;
    fn name(&self) -> &str;
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
            Ok(Box::new(model2vec::Model2VecEmbedder::new(model_id)?))
        }
        #[cfg(feature = "onnx")]
        "onnx" => Ok(Box::new(onnx::OnnxEmbedder::new_or_download().await?)),
        "openai_compat" | "api" => Ok(Box::new(
            openai_compat::OpenAiCompatibleEmbedder::from_config(config)?,
        )),
        other => Err(EmbedError::UnsupportedBackend(other.to_string())),
    }
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
}
