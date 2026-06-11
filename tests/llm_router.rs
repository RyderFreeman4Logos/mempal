use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mempal::core::config::Config;
use mempal::llm::{LlmError, LlmMessage, LlmRequest, LlmRouter};
use mockito::{Matcher, Server};

fn request() -> LlmRequest {
    LlmRequest {
        messages: vec![LlmMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        model: None,
        temperature: Some(0.0),
        max_tokens: Some(16),
    }
}

fn success_body(model: &str, content: &str) -> String {
    serde_json::json!({
        "model": model,
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

fn endpoint_pool_config(primary: &Server, secondary: &Server) -> mempal::core::config::LlmConfig {
    Config::parse(&format!(
        r#"
[llm]
enabled = true

[[llm.endpoints]]
id = "primary"
base_url = "{}/v1"
model = "primary-model"

[[llm.endpoints]]
id = "secondary"
base_url = "{}/v1"
model = "secondary-model"
"#,
        primary.url(),
        secondary.url()
    ))
    .expect("parse endpoint pool")
    .llm
}

#[tokio::test]
async fn test_llm_router_primary_5xx_falls_back_to_secondary_success() {
    let mut primary = Server::new_async().await;
    let mut secondary = Server::new_async().await;
    let primary_mock = primary
        .mock("POST", "/v1/chat/completions")
        .with_status(500)
        .with_body("server error")
        .create_async()
        .await;
    let secondary_mock = secondary
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "model": "secondary-model"
        })))
        .with_status(200)
        .with_body(success_body("secondary-model", "ok"))
        .create_async()
        .await;
    let config = endpoint_pool_config(&primary, &secondary);
    let router = LlmRouter::from_config(&config).expect("build router");

    let response = router
        .chat_completion(&request(), None)
        .await
        .expect("fallback response");

    primary_mock.assert_async().await;
    secondary_mock.assert_async().await;
    assert_eq!(response.endpoint_id, "secondary");
    assert_eq!(response.endpoint_model, "secondary-model");
    assert_eq!(response.response.content, "ok");
}

#[tokio::test]
async fn test_llm_router_429_marks_endpoint_and_falls_back() {
    let mut primary = Server::new_async().await;
    let mut secondary = Server::new_async().await;
    let primary_mock = primary
        .mock("POST", "/v1/chat/completions")
        .with_status(429)
        .with_header("retry-after", "2")
        .with_body("rate limited")
        .create_async()
        .await;
    let secondary_mock = secondary
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(success_body("secondary-model", "fallback"))
        .expect(2)
        .create_async()
        .await;
    let config = endpoint_pool_config(&primary, &secondary);
    let router = LlmRouter::from_config(&config).expect("build router");

    let first = router
        .chat_completion(&request(), None)
        .await
        .expect("first fallback");
    let second = router
        .chat_completion(&request(), None)
        .await
        .expect("second fallback");

    primary_mock.assert_async().await;
    secondary_mock.assert_async().await;
    assert_eq!(first.endpoint_id, "secondary");
    assert_eq!(second.endpoint_id, "secondary");
}

#[tokio::test]
async fn test_llm_router_all_temporarily_unavailable_remains_retryable() {
    let mut primary = Server::new_async().await;
    let mut secondary = Server::new_async().await;
    let primary_mock = primary
        .mock("POST", "/v1/chat/completions")
        .with_status(429)
        .with_header("retry-after", "5")
        .with_body("primary rate limited")
        .expect(1)
        .create_async()
        .await;
    let secondary_mock = secondary
        .mock("POST", "/v1/chat/completions")
        .with_status(429)
        .with_header("retry-after", "60")
        .with_body("secondary rate limited")
        .expect(1)
        .create_async()
        .await;
    let config = endpoint_pool_config(&primary, &secondary);
    let router = LlmRouter::from_config(&config).expect("build router");

    let first = router
        .chat_completion(&request(), None)
        .await
        .expect_err("all endpoints rate limited");
    let second = router
        .chat_completion(&request(), None)
        .await
        .expect_err("cooling down endpoints should remain retryable");

    primary_mock.assert_async().await;
    secondary_mock.assert_async().await;
    assert!(matches!(
        first,
        LlmError::TemporarilyUnavailable {
            retry_after,
            ..
        } if retry_after == std::time::Duration::from_secs(5)
    ));
    assert!(first.is_retryable());
    assert!(matches!(
        second,
        LlmError::TemporarilyUnavailable {
            retry_after,
            ..
        } if retry_after <= std::time::Duration::from_secs(5)
    ));
    assert!(second.is_retryable());
}

#[tokio::test]
async fn test_llm_router_mixed_5xx_then_rate_limit_uses_non_cooldown_retry_policy() {
    let mut primary = Server::new_async().await;
    let mut secondary = Server::new_async().await;
    let primary_mock = primary
        .mock("POST", "/v1/chat/completions")
        .with_status(500)
        .with_body("primary server error")
        .expect(1)
        .create_async()
        .await;
    let secondary_mock = secondary
        .mock("POST", "/v1/chat/completions")
        .with_status(429)
        .with_header("retry-after", "60")
        .with_body("secondary rate limited")
        .expect(1)
        .create_async()
        .await;
    let config = endpoint_pool_config(&primary, &secondary);
    let router = LlmRouter::from_config(&config).expect("build router");

    let error = router
        .chat_completion(&request(), None)
        .await
        .expect_err("ordinary retryable failure should win over endpoint cooldown");

    primary_mock.assert_async().await;
    secondary_mock.assert_async().await;
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
async fn test_llm_router_nonretryable_4xx_stops_without_secondary() {
    let mut primary = Server::new_async().await;
    let mut secondary = Server::new_async().await;
    let primary_mock = primary
        .mock("POST", "/v1/chat/completions")
        .with_status(400)
        .with_body("bad request")
        .create_async()
        .await;
    let secondary_mock = secondary
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(success_body("secondary-model", "should not be used"))
        .expect(0)
        .create_async()
        .await;
    let config = endpoint_pool_config(&primary, &secondary);
    let router = LlmRouter::from_config(&config).expect("build router");

    let error = router
        .chat_completion(&request(), None)
        .await
        .expect_err("4xx must stop");

    primary_mock.assert_async().await;
    secondary_mock.assert_async().await;
    assert!(matches!(
        error,
        LlmError::ClientError {
            status: reqwest::StatusCode::BAD_REQUEST,
            ..
        }
    ));
}

#[tokio::test]
async fn test_llm_router_heartbeat_fires_during_routed_attempts() {
    let mut primary = Server::new_async().await;
    let mut secondary = Server::new_async().await;
    let primary_mock = primary
        .mock("POST", "/v1/chat/completions")
        .with_status(500)
        .with_body("server error")
        .create_async()
        .await;
    let secondary_mock = secondary
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(success_body("secondary-model", "ok"))
        .create_async()
        .await;
    let config = endpoint_pool_config(&primary, &secondary);
    let router = LlmRouter::from_config(&config).expect("build router");
    let heartbeat_count = Arc::new(AtomicUsize::new(0));
    let heartbeat_count_for_callback = Arc::clone(&heartbeat_count);
    let heartbeat: Box<mempal::llm::retry::HeartbeatCallback> = Box::new(move || {
        heartbeat_count_for_callback.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });

    let response = router
        .chat_completion(&request(), Some(heartbeat.as_ref()))
        .await
        .expect("fallback response");

    primary_mock.assert_async().await;
    secondary_mock.assert_async().await;
    assert_eq!(response.endpoint_id, "secondary");
    assert!(
        heartbeat_count.load(Ordering::SeqCst) >= 2,
        "heartbeat should fire before each routed endpoint attempt"
    );
}
