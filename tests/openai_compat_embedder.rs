use std::sync::{Arc, OnceLock};

use mempal::core::config::Config;
use mempal::embed::{EmbedError, Embedder, openai_compat::OpenAiCompatibleEmbedder};
use mockito::{Matcher, Server};

async fn env_guard() -> tokio::sync::OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await
}

fn config_for(server: &Server, extra: &str) -> Config {
    Config::parse(&format!(
        r#"
db_path = "/tmp/mempal-test.db"

[embed]
backend = "openai_compat"

[embed.openai_compat]
base_url = "{}/v1"
model = "Qwen/Qwen3-Embedding-8B"
dim = 3
request_timeout_secs = 5
{}
"#,
        server.url(),
        extra
    ))
    .expect("parse config")
}

#[tokio::test]
async fn test_openai_compat_embed_happy_path() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/embeddings")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "model": "Qwen/Qwen3-Embedding-8B",
            "input": ["hello"]
        })))
        .with_status(200)
        .with_body(r#"{"data":[{"embedding":[0.1,0.2,0.3]}]}"#)
        .create();
    let config = config_for(&server, "");
    let embedder = OpenAiCompatibleEmbedder::from_config(&config).expect("build embedder");

    let vectors = embedder.embed(&["hello"]).await.expect("embed");

    mock.assert();
    assert_eq!(vectors, vec![vec![0.1, 0.2, 0.3]]);
}

#[tokio::test]
async fn test_openai_compat_embed_api_error() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/embeddings")
        .with_status(503)
        .with_body("unavailable")
        .create();
    let config = config_for(&server, "");
    let embedder = OpenAiCompatibleEmbedder::from_config(&config).expect("build embedder");

    let error = embedder.embed(&["hello"]).await.expect_err("api error");

    mock.assert();
    assert!(matches!(error, EmbedError::HttpStatus { .. }));
}

#[tokio::test]
async fn test_openai_compat_embed_malformed_response() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/embeddings")
        .with_status(200)
        .with_body(r#"{"data":[{"embedding":"bad"}]}"#)
        .create();
    let config = config_for(&server, "");
    let embedder = OpenAiCompatibleEmbedder::from_config(&config).expect("build embedder");

    let error = embedder
        .embed(&["hello"])
        .await
        .expect_err("malformed response");

    mock.assert();
    assert!(matches!(error, EmbedError::DecodeResponse { .. }));
}

#[tokio::test]
async fn test_api_key_from_env_var() {
    let _guard = env_guard().await;
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/embeddings")
        .match_header("authorization", "Bearer sk-test123")
        .with_status(200)
        .with_body(r#"{"data":[{"embedding":[0.1,0.2,0.3]}]}"#)
        .create();
    // SAFETY: tests serialize environment mutation with a process-wide mutex.
    unsafe {
        std::env::set_var("MEMPAL_TEST_KEY", "sk-test123");
    }
    let config = config_for(&server, r#"api_key_env = "MEMPAL_TEST_KEY""#);
    let embedder = OpenAiCompatibleEmbedder::from_config(&config).expect("build embedder");

    let vectors = embedder.embed(&["hello"]).await.expect("embed with auth");

    mock.assert();
    assert_eq!(vectors[0].len(), 3);
    // SAFETY: paired with the serialized mutation above.
    unsafe {
        std::env::remove_var("MEMPAL_TEST_KEY");
    }
}
