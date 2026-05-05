#![cfg(feature = "integration")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use mempal::llm::client::{LlmError, LlmResponse, Usage};
use mempal::llm::retry::retry_llm_operation;
use reqwest::StatusCode;

fn ok_response() -> LlmResponse {
    LlmResponse {
        content: "ok".to_string(),
        usage: Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
        }),
        model: "test".to_string(),
    }
}

#[tokio::test]
async fn test_retry_success_after_transient_failures() {
    let call_count = Arc::new(AtomicU32::new(0));
    let cc = call_count.clone();
    let result = retry_llm_operation(1, None, move || {
        let cc = cc.clone();
        async move {
            let n = cc.fetch_add(1, Ordering::SeqCst);
            if n < 3 {
                Err(LlmError::HttpStatus {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    body: "server error".to_string(),
                })
            } else {
                Ok(ok_response())
            }
        }
    })
    .await;
    assert!(result.is_ok());
    assert_eq!(call_count.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn test_retry_non_retryable_returns_immediately() {
    let call_count = Arc::new(AtomicU32::new(0));
    let cc = call_count.clone();
    let result = retry_llm_operation(1, None, move || {
        let cc = cc.clone();
        async move {
            cc.fetch_add(1, Ordering::SeqCst);
            Err::<LlmResponse, _>(LlmError::ClientError {
                status: StatusCode::BAD_REQUEST,
                body: "bad request".to_string(),
                retry_after: None,
            })
        }
    })
    .await;
    assert!(result.is_err());
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_retry_429_respects_retry_after_header() {
    let call_count = Arc::new(AtomicU32::new(0));
    let cc = call_count.clone();
    let start = Instant::now();
    let result = retry_llm_operation(10, None, move || {
        let cc = cc.clone();
        async move {
            let n = cc.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Err(LlmError::ClientError {
                    status: StatusCode::TOO_MANY_REQUESTS,
                    body: "rate limited".to_string(),
                    retry_after: Some(Duration::from_secs(1)),
                })
            } else {
                Ok(ok_response())
            }
        }
    })
    .await;
    let elapsed = start.elapsed();
    assert!(result.is_ok());
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
    // Should have waited ~1s (Retry-After), not 10s (default)
    assert!(elapsed >= Duration::from_millis(900));
    assert!(elapsed < Duration::from_secs(3));
}

#[tokio::test]
async fn test_retry_429_without_header_uses_default_interval() {
    let call_count = Arc::new(AtomicU32::new(0));
    let cc = call_count.clone();
    let start = Instant::now();
    let result = retry_llm_operation(1, None, move || {
        let cc = cc.clone();
        async move {
            let n = cc.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Err(LlmError::ClientError {
                    status: StatusCode::TOO_MANY_REQUESTS,
                    body: "rate limited".to_string(),
                    retry_after: None,
                })
            } else {
                Ok(ok_response())
            }
        }
    })
    .await;
    let elapsed = start.elapsed();
    assert!(result.is_ok());
    assert!(elapsed >= Duration::from_millis(900));
}

#[tokio::test]
async fn test_retry_heartbeat_called_during_wait() {
    let heartbeat_count = Arc::new(AtomicU32::new(0));
    let hc = heartbeat_count.clone();
    let heartbeat = move || -> Result<(), LlmError> {
        hc.fetch_add(1, Ordering::SeqCst);
        Ok(())
    };

    let call_count = Arc::new(AtomicU32::new(0));
    let cc = call_count.clone();
    let _ = retry_llm_operation(1, Some(&heartbeat), move || {
        let cc = cc.clone();
        async move {
            let n = cc.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Err(LlmError::HttpStatus {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    body: "error".to_string(),
                })
            } else {
                Ok(ok_response())
            }
        }
    })
    .await;

    assert!(heartbeat_count.load(Ordering::SeqCst) >= 2);
}

#[tokio::test]
async fn test_retry_immediate_success_no_wait() {
    let start = Instant::now();
    let result = retry_llm_operation(10, None, || async { Ok(ok_response()) }).await;
    let elapsed = start.elapsed();
    assert!(result.is_ok());
    assert!(elapsed < Duration::from_millis(100));
}
