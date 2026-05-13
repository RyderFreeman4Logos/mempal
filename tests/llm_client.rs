use std::sync::{Arc, OnceLock};
use std::time::Duration;

use mempal::core::config::{Config, LlmConfig};
use mempal::llm::{LlmClient, LlmError, LlmMessage, LlmRequest};
use mockito::{Matcher, Server};

async fn env_guard() -> tokio::sync::OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await
}

fn config_for(server: &Server, extra: &str) -> LlmConfig {
    Config::parse(&format!(
        r#"
[llm]
enabled = true
base_url = "{}/v1"
model = "local-test-model"
request_timeout_secs = 5
{extra}
"#,
        server.url()
    ))
    .expect("parse config")
    .llm
}

fn request() -> LlmRequest {
    LlmRequest {
        messages: vec![LlmMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        model: None,
        temperature: Some(0.2),
        max_tokens: Some(32),
    }
}

fn success_body(content: &str) -> String {
    serde_json::json!({
        "model": "local-test-model",
        "choices": [
            {
                "message": {
                    "role": "assistant",
                    "content": content
                }
            }
        ],
        "usage": {
            "prompt_tokens": 7,
            "completion_tokens": 3,
            "total_tokens": 10
        }
    })
    .to_string()
}

#[test]
fn test_llm_client_from_config_success() {
    let config = LlmConfig {
        base_url: Some("http://127.0.0.1:8317/v1".to_string()),
        model: Some("local-test-model".to_string()),
        ..Default::default()
    };

    let client = LlmClient::from_config(&config);

    assert!(client.is_ok());
}

#[test]
fn test_llm_client_from_config_missing_base_url() {
    let config = LlmConfig {
        model: Some("local-test-model".to_string()),
        ..Default::default()
    };

    let error = LlmClient::from_config(&config).expect_err("missing base_url");

    assert!(matches!(error, LlmError::MissingConfiguration(_)));
    assert!(error.to_string().contains("llm.base_url"));
}

#[tokio::test]
async fn test_llm_client_chat_completion_success() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "model": "local-test-model",
            "messages": [{"role": "user", "content": "hello"}],
            "temperature": 0.2,
            "max_tokens": 32
        })))
        .with_status(200)
        .with_body(success_body("world"))
        .create();
    let config = config_for(&server, "");
    let client = LlmClient::from_config(&config).expect("build client");

    let response = client.chat_completion(&request()).await.expect("chat");

    mock.assert();
    assert_eq!(response.content, "world");
    assert_eq!(response.model, "local-test-model");
    let usage = response.usage.expect("usage");
    assert_eq!(usage.prompt_tokens, 7);
    assert_eq!(usage.completion_tokens, 3);
}

#[tokio::test]
async fn test_llm_client_sends_extra_body() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "model": "local-test-model",
            "chat_template_kwargs": {
                "enable_thinking": false
            }
        })))
        .with_status(200)
        .with_body(success_body("extra"))
        .create();
    let config = config_for(
        &server,
        r#"extra_body = { chat_template_kwargs = { enable_thinking = false } }"#,
    );
    let client = LlmClient::from_config(&config).expect("build client");

    let response = client.chat_completion(&request()).await.expect("chat");

    mock.assert();
    assert_eq!(response.content, "extra");
}

#[tokio::test]
async fn test_llm_client_4xx_not_retryable() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(400)
        .with_body("bad request")
        .create();
    let config = config_for(&server, "");
    let client = LlmClient::from_config(&config).expect("build client");

    let error = client.chat_completion(&request()).await.expect_err("4xx");

    mock.assert();
    assert!(matches!(
        error,
        LlmError::ClientError {
            status: reqwest::StatusCode::BAD_REQUEST,
            ..
        }
    ));
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn test_llm_client_5xx_retryable() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(500)
        .with_body("server error")
        .create();
    let config = config_for(&server, "");
    let client = LlmClient::from_config(&config).expect("build client");

    let error = client.chat_completion(&request()).await.expect_err("5xx");

    mock.assert();
    assert!(matches!(
        error,
        LlmError::HttpStatus {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            ..
        }
    ));
    assert!(error.is_retryable());
}

#[tokio::test]
async fn test_llm_client_429_retryable() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(429)
        .with_header("retry-after", "2")
        .with_body("rate limited")
        .create();
    let config = config_for(&server, "");
    let client = LlmClient::from_config(&config).expect("build client");

    let error = client.chat_completion(&request()).await.expect_err("429");

    mock.assert();
    assert!(error.is_retryable());
    assert!(matches!(
        error,
        LlmError::ClientError {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            retry_after: Some(duration),
            ..
        } if duration == Duration::from_secs(2)
    ));
}

#[tokio::test]
async fn test_llm_client_no_api_key_no_auth_header() {
    let _guard = env_guard().await;
    // SAFETY: tests serialize environment mutation with a process-wide mutex.
    unsafe {
        std::env::remove_var("MEMPAL_LLM_TEST_KEY");
    }

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .match_header("authorization", Matcher::Missing)
        .with_status(200)
        .with_body(success_body("no auth"))
        .create();
    let config = config_for(&server, r#"api_key_env = "MEMPAL_LLM_TEST_KEY""#);
    let client = LlmClient::from_config(&config).expect("build client");

    let response = client.chat_completion(&request()).await.expect("chat");

    mock.assert();
    assert_eq!(response.content, "no auth");
}
