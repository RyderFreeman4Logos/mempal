use std::sync::Arc;

use mempal_embed::{Embedder, api::ApiEmbedder, onnx::OnnxEmbedder};
use tokio::sync::OnceCell;

async fn shared_onnx_embedder() -> Arc<OnnxEmbedder> {
    static EMBEDDER: OnceCell<Arc<OnnxEmbedder>> = OnceCell::const_new();

    EMBEDDER
        .get_or_init(|| async {
            Arc::new(
                OnnxEmbedder::new_or_download()
                    .await
                    .expect("onnx embedder should initialize"),
            )
        })
        .await
        .clone()
}

#[tokio::test]
async fn test_embed_empty() {
    let embedder = shared_onnx_embedder().await;
    let result = embedder
        .embed(&[])
        .await
        .expect("empty embedding batch should succeed");

    assert!(result.is_empty());
}

#[tokio::test]
async fn test_onnx_dimensions() {
    let embedder = shared_onnx_embedder().await;

    assert_eq!(embedder.dimensions(), 384);
}

#[tokio::test]
async fn test_onnx_embed_single() {
    let embedder = shared_onnx_embedder().await;
    let vectors = embedder
        .embed(&["hello world"])
        .await
        .expect("single embedding should succeed");

    assert_eq!(vectors.len(), 1);
    assert_eq!(vectors[0].len(), 384);
    assert!(
        vectors[0]
            .iter()
            .all(|value| *value >= -1.0 && *value <= 1.0)
    );
}

#[tokio::test]
async fn test_onnx_batch() {
    let embedder = shared_onnx_embedder().await;
    let vectors = embedder
        .embed(&["text a", "text b", "text c"])
        .await
        .expect("batch embedding should succeed");

    assert_eq!(vectors.len(), 3);
    assert!(vectors.iter().all(|vector| vector.len() == 384));
}

#[tokio::test]
async fn test_api_embedder_config() {
    let embedder = ApiEmbedder::new(
        "http://localhost:11434/api/embeddings".into(),
        Some("nomic-embed-text".into()),
        384,
    );

    assert_eq!(embedder.dimensions(), 384);
    assert_eq!(embedder.name(), "api");
    assert!(embedder.embed(&["hello"]).await.is_err());
}
