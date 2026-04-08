#![warn(clippy::all)]

pub mod api;
pub mod onnx;

pub const EMBEDDING_DIMENSIONS: usize = 384;

#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>>;
    fn dimensions(&self) -> usize;
    fn name(&self) -> &str;
}
