use std::sync::Arc;
use std::sync::OnceLock;

use crate::core::config::{Config, ConfigHandle};
use async_trait::async_trait;

use super::{Embedder, Result};

#[async_trait]
pub trait EmbedderFactory: Send + Sync {
    async fn build(&self) -> Result<Box<dyn Embedder>>;
}

#[derive(Clone)]
pub struct ConfiguredEmbedderFactory {
    config: Config,
}

impl ConfiguredEmbedderFactory {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    fn active_config(&self) -> Config {
        let current = ConfigHandle::current();
        if current.db_path == self.config.db_path && !config_is_default_snapshot(current.as_ref()) {
            current.as_ref().clone()
        } else {
            self.config.clone()
        }
    }
}

#[async_trait]
impl EmbedderFactory for ConfiguredEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>> {
        let config = self.active_config();
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
            embedder: Arc::clone(&embedder),
        });
        Ok(Box::new(SharedEmbedder { inner: embedder }))
    }
}

struct SharedEmbedderRuntime {
    signature: String,
    embedder: Arc<dyn Embedder>,
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
    })
    .to_string()
}
