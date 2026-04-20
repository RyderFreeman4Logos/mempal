use crate::core::config::Config;
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
}

#[async_trait]
impl EmbedderFactory for ConfiguredEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>> {
        super::from_config(&self.config).await
    }
}
