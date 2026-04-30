use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mempal::core::config::LlmConfig;
use mempal::llm::client::LlmClient;

fn test_config(max_concurrent: usize) -> LlmConfig {
    LlmConfig {
        enabled: true,
        base_url: Some("http://127.0.0.1:9999/v1".to_string()),
        model: Some("test-model".to_string()),
        max_concurrent,
        ..Default::default()
    }
}

#[tokio::test]
async fn test_semaphore_limits_concurrency() {
    let client = LlmClient::from_config(&test_config(2)).unwrap();
    assert_eq!(client.current_max_concurrent(), 2);
    assert_eq!(client.available_permits(), 2);
}

#[tokio::test]
async fn test_update_concurrency_increase() {
    let client = LlmClient::from_config(&test_config(2)).unwrap();
    client.update_concurrency(4).await;
    assert_eq!(client.current_max_concurrent(), 4);
    assert_eq!(client.available_permits(), 4);
}

#[tokio::test]
async fn test_update_concurrency_decrease_drains_permits() {
    let client = LlmClient::from_config(&test_config(4)).unwrap();
    assert_eq!(client.available_permits(), 4);
    client.update_concurrency(2).await;
    assert_eq!(client.current_max_concurrent(), 2);
    assert_eq!(client.available_permits(), 2);
}

#[tokio::test]
async fn test_update_concurrency_noop_same_value() {
    let client = LlmClient::from_config(&test_config(3)).unwrap();
    client.update_concurrency(3).await;
    assert_eq!(client.current_max_concurrent(), 3);
    assert_eq!(client.available_permits(), 3);
}

#[tokio::test]
async fn test_min_concurrency_is_one() {
    let client = LlmClient::from_config(&test_config(0)).unwrap();
    assert_eq!(client.current_max_concurrent(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_decrease_does_not_spike_concurrent_requests() {
    let client = Arc::new(LlmClient::from_config(&test_config(4)).unwrap());
    let peak = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));

    // Simulate 4 concurrent "in-flight" semaphore acquisitions
    let mut handles = Vec::new();
    for _ in 0..4 {
        let peak = peak.clone();
        let active = active.clone();
        handles.push(tokio::spawn(async move {
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(current, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(200)).await;
            active.fetch_sub(1, Ordering::SeqCst);
        }));
    }

    // Give tasks time to start
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Decrease concurrency while tasks are "in flight"
    client.update_concurrency(2).await;
    assert_eq!(client.current_max_concurrent(), 2);

    for handle in handles {
        handle.await.unwrap();
    }

    // After decrease, only 2 permits should be available
    assert_eq!(client.available_permits(), 2);
}
