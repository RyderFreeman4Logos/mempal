use anyhow::{Result, bail};

use crate::Embedder;

#[derive(Debug, Clone)]
pub struct ApiEmbedder {
    endpoint: String,
    model: Option<String>,
    dimensions: usize,
}

impl ApiEmbedder {
    pub fn new(endpoint: String, model: Option<String>, dimensions: usize) -> Self {
        Self {
            endpoint,
            model,
            dimensions,
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

#[async_trait::async_trait]
impl Embedder for ApiEmbedder {
    async fn embed(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        bail!(
            "api embedder is not implemented yet for endpoint {}",
            self.endpoint()
        )
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn name(&self) -> &str {
        "api"
    }
}
