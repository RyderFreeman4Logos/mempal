use async_trait::async_trait;

use super::{Embedder, Result};

/// Fallback dimension used ONLY when the stub backend is selected with no explicit dim
/// configured. Production paths supply dim explicitly (e.g., `openai_compat.dim` defaults
/// to `Some(4096)`), so this constant is never reached in normal operation.
pub const DEFAULT_STUB_DIM: usize = 384;

/// Hermetic, zero-network embedder for tests and CI.
///
/// Returns deterministic fixed-dimension vectors without loading any model or
/// making any network request. Every call succeeds instantly.
///
/// Selected via `MEMPAL_EMBED_BACKEND=stub` (injected by the subprocess test
/// harnesses when no real embedder is configured in the environment).
pub struct StubEmbedder {
    dim: usize,
}

impl StubEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

#[async_trait]
impl Embedder for StubEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let vectors = texts
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let seed = (i as u64).wrapping_mul(0x9e3779b97f4a7c15)
                    ^ (text.len() as u64).wrapping_mul(0x6c62272e07bb0142);
                (0..self.dim)
                    .map(|j| {
                        let x = seed.wrapping_add((j as u64).wrapping_mul(0x517cc1b727220a95));
                        // Map u64 to [-1.0, 1.0)
                        (x as i64 as f32) / (i64::MAX as f32)
                    })
                    .collect()
            })
            .collect();
        Ok(vectors)
    }

    fn dimensions(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &str {
        "stub"
    }
}
