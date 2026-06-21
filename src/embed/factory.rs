use std::sync::Arc;
use std::sync::OnceLock;

use crate::core::config::{Config, ConfigHandle};
use async_trait::async_trait;
use serde::Serialize;

use super::{EmbedError, Embedder, Result};

#[async_trait]
pub trait EmbedderFactory: Send + Sync {
    async fn build(&self) -> Result<Box<dyn Embedder>>;
}

#[derive(Clone)]
pub struct ConfiguredEmbedderFactory {
    config: Config,
    daemon_mode: bool,
}

impl ConfiguredEmbedderFactory {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            daemon_mode: false,
        }
    }

    pub fn new_for_daemon(config: Config) -> Self {
        Self {
            config,
            daemon_mode: true,
        }
    }

    fn active_config(&self) -> Result<Config> {
        let current = ConfigHandle::current();
        let config = if current.db_path == self.config.db_path
            && !config_is_default_snapshot(current.as_ref())
        {
            current.as_ref().clone()
        } else {
            self.config.clone()
        };
        if self.daemon_mode {
            config
                .validate_daemon_embedder_mode()
                .map_err(|error| EmbedError::InvalidConfiguration(error.to_string()))?;
            Ok(config.daemon_embedder_config())
        } else {
            Ok(config)
        }
    }
}

#[async_trait]
impl EmbedderFactory for ConfiguredEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>> {
        let config = self.active_config()?;
        let signature = embedder_runtime_signature(&config);
        let cache = shared_embedder_cache();
        let mut guard = cache.lock().await;
        if let Some(runtime) = guard.as_ref()
            && runtime.signature == signature
        {
            return Ok(Box::new(SharedEmbedder {
                inner: Arc::clone(&runtime.embedder),
            }));
        }
        let embedder: Arc<dyn Embedder> = Arc::from(super::from_config(&config).await?);
        *guard = Some(SharedEmbedderRuntime {
            signature,
            backend: config.embed.backend.clone(),
            model: config.embed.effective_model_summary(),
            dimensions: embedder.dimensions(),
            max_input_tokens: embedder.max_input_tokens(),
            embedder: Arc::clone(&embedder),
        });
        Ok(Box::new(SharedEmbedder { inner: embedder }))
    }
}

struct SharedEmbedderRuntime {
    signature: String,
    backend: String,
    model: Option<String>,
    dimensions: usize,
    max_input_tokens: Option<usize>,
    embedder: Arc<dyn Embedder>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SharedEmbedderRuntimeSnapshot {
    pub loaded: bool,
    pub busy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<usize>,
}

struct SharedEmbedder {
    inner: Arc<dyn Embedder>,
}

#[async_trait]
impl Embedder for SharedEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.inner.embed(texts).await
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn max_input_tokens(&self) -> Option<usize> {
        self.inner.max_input_tokens()
    }

    fn estimate_tokens(&self, text: &str) -> usize {
        self.inner.estimate_tokens(text)
    }
}

fn shared_embedder_cache() -> &'static tokio::sync::Mutex<Option<SharedEmbedderRuntime>> {
    static CACHE: OnceLock<tokio::sync::Mutex<Option<SharedEmbedderRuntime>>> = OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::Mutex::new(None))
}

pub fn shared_embedder_runtime_snapshot() -> SharedEmbedderRuntimeSnapshot {
    let cache = shared_embedder_cache();
    let Ok(guard) = cache.try_lock() else {
        return SharedEmbedderRuntimeSnapshot {
            busy: true,
            ..SharedEmbedderRuntimeSnapshot::default()
        };
    };
    match guard.as_ref() {
        Some(runtime) => SharedEmbedderRuntimeSnapshot {
            loaded: true,
            busy: false,
            backend: Some(runtime.backend.clone()),
            model: runtime.model.clone(),
            dimensions: Some(runtime.dimensions),
            max_input_tokens: runtime.max_input_tokens,
        },
        None => SharedEmbedderRuntimeSnapshot::default(),
    }
}

fn config_is_default_snapshot(config: &Config) -> bool {
    match (config.effective_hash(), Config::default().effective_hash()) {
        (Ok(current), Ok(default)) => current == default,
        _ => false,
    }
}

fn embedder_runtime_signature(config: &Config) -> String {
    serde_json::json!({
        "backend": config.embed.backend,
        "fallback": config.embed.fallback,
        "model": config.embed.model,
        "base_url": config.embed.base_url,
        "api_model": config.embed.api_model,
        "openai_compat": {
            "base_url": config.embed.openai_compat.base_url,
            "model": config.embed.openai_compat.model,
            "api_key_env": config.embed.openai_compat.api_key_env,
            "request_timeout_secs": config.embed.openai_compat.request_timeout_secs,
            "dim": config.embed.openai_compat.dim,
            "max_input_tokens": config.embed.openai_compat.max_input_tokens,
        },
        "endpoints": config.embed.effective_endpoint_fingerprints(),
        "max_concurrent": config.embed.max_concurrent,
        "retry_interval_secs": config.embed.retry.interval_secs,
        "privacy_remote_calls": {
            "fail_closed": config.privacy.remote_calls.fail_closed,
            "allow_embedding": config.privacy.remote_calls.allow_embedding,
            "allow_llm": config.privacy.remote_calls.allow_llm,
            "allow_rerank": config.privacy.remote_calls.allow_rerank,
        },
    })
    .to_string()
}
