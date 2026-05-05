#![cfg(feature = "integration")]

use mempal::core::config::LlmConfig;
use mempal::llm::client::{LlmClient, LlmError, LlmMessage, LlmRequest};
use mempal::llm::retry::retry_llm_operation;
use mempal::llm::status::LlmStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

fn test_llm_config() -> LlmConfig {
    LlmConfig {
        enabled: true,
        base_url: Some("http://127.0.0.1:19999/v1".to_string()),
        model: Some("test-model".to_string()),
        max_concurrent: 4,
        ..Default::default()
    }
}

#[test]
fn test_llm_config_default_disabled() {
    let config = LlmConfig::default();
    assert!(!config.enabled);
    assert!(config.base_url.is_none());
    assert_eq!(config.max_concurrent, 16);
}

#[test]
fn test_llm_client_from_config() {
    let config = test_llm_config();
    let client = LlmClient::from_config(&config);
    assert!(client.is_ok());
    let client = client.unwrap();
    assert_eq!(client.current_max_concurrent(), 4);
    assert_eq!(client.available_permits(), 4);
}

#[test]
fn test_llm_client_missing_base_url_fails() {
    let config = LlmConfig {
        enabled: true,
        model: Some("test".to_string()),
        ..Default::default()
    };
    let result = LlmClient::from_config(&config);
    assert!(result.is_err());
}

#[tokio::test]
async fn test_llm_retry_with_status_tracking() {
    let status = Arc::new(LlmStatus::new(3));
    let call_count = Arc::new(AtomicU32::new(0));
    let cc = call_count.clone();
    let status_clone = status.clone();

    let result = retry_llm_operation(1, None, move || {
        let cc = cc.clone();
        let status = status_clone.clone();
        async move {
            let n = cc.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                let err = LlmError::HttpStatus {
                    status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                    body: "error".to_string(),
                };
                status.record_failure(&err);
                Err(err)
            } else {
                status.record_success();
                Ok(mempal::llm::LlmResponse {
                    content: r#"{"verdict": "keep", "score": 0.9}"#.to_string(),
                    usage: None,
                    model: "test".to_string(),
                })
            }
        }
    })
    .await;

    assert!(result.is_ok());
    assert!(!status.is_degraded());
    assert_eq!(status.snapshot().fail_count, 0);
}

#[tokio::test]
async fn test_llm_concurrency_update_decrease() {
    let config = test_llm_config();
    let client = LlmClient::from_config(&config).unwrap();
    assert_eq!(client.available_permits(), 4);

    client.update_concurrency(2).await;
    assert_eq!(client.current_max_concurrent(), 2);
    assert_eq!(client.available_permits(), 2);

    client.update_concurrency(6).await;
    assert_eq!(client.current_max_concurrent(), 6);
    assert_eq!(client.available_permits(), 6);
}

#[test]
fn test_llm_request_serialization() {
    let request = LlmRequest {
        messages: vec![
            LlmMessage {
                role: "system".to_string(),
                content: "You are a judge.".to_string(),
            },
            LlmMessage {
                role: "user".to_string(),
                content: "Evaluate this content.".to_string(),
            },
        ],
        model: None,
        temperature: Some(0.0),
        max_tokens: Some(256),
    };
    let json = serde_json::to_string(&request).unwrap();
    assert!(json.contains("system"));
    assert!(json.contains("user"));
}

#[test]
fn test_llm_task_payload_roundtrip() {
    let payload = mempal::llm::LlmTaskPayload {
        task_type: "gating".to_string(),
        drawer_id: "drawer_test_123".to_string(),
        drawer_ids: vec![],
        content: "Some test content".to_string(),
        system_prompt: Some("Judge this.".to_string()),
    };
    let json = serde_json::to_string(&payload).unwrap();
    let decoded: mempal::llm::LlmTaskPayload = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.task_type, "gating");
    assert_eq!(decoded.drawer_id, "drawer_test_123");
}
