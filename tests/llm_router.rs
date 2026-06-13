use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mempal::core::config::Config;
use mempal::llm::{LlmError, LlmMessage, LlmRequest, LlmRouter};
use mockito::{Matcher, Server};
use tokio::sync::{Barrier, Notify};

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
async fn test_llm_router_clamps_absurd_retry_after_before_cooldown() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(429)
        .with_header("retry-after", "18446744073709551615")
        .with_body("rate limited")
        .expect(1)
        .create_async()
        .await;
    let config = pool_config(&[("primary", &format!("{}/v1", server.url()), "model-a", 0, 1)]);
    let router = LlmRouter::from_config(&config).expect("build router");

    let error = router
        .chat_completion(&request(), None)
        .await
        .expect_err("rate limit should cool down without panicking");

    mock.assert_async().await;
    assert!(matches!(
        error,
        LlmError::TemporarilyUnavailable {
            retry_after,
            ..
        } if retry_after == Duration::from_secs(60)
    ));
}

#[tokio::test]
async fn test_llm_router_all_5xx_reports_configured_retry_interval() {
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
        .with_status(503)
        .with_body("secondary warming")
        .expect(1)
        .create_async()
        .await;
    let config = endpoint_pool_config(&primary, &secondary);
    let router = LlmRouter::from_config(&config).expect("build router");

    let error = router
        .chat_completion(&request(), None)
        .await
        .expect_err("server errors should produce durable retry timing");

    primary_mock.assert_async().await;
    secondary_mock.assert_async().await;
    assert!(matches!(
        error,
        LlmError::TemporarilyUnavailable {
            retry_after,
            ..
        } if retry_after == Duration::from_secs(2)
    ));
    assert!(error.is_retryable());
}

async fn spawn_counting_server(
    model: &'static str,
    delay: Duration,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    use axum::{Json, Router, routing::post};

    let count = Arc::new(AtomicUsize::new(0));
    let count_for_handler = Arc::clone(&count);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let count = Arc::clone(&count_for_handler);
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(delay).await;
                Json(serde_json::json!({
                    "model": model,
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "ok"
                        }
                    }],
                    "usage": {
                        "prompt_tokens": 1,
                        "completion_tokens": 1,
                        "total_tokens": 2
                    }
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind counting LLM server");
    let addr = listener.local_addr().expect("server addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve counting LLM server");
    });
    (format!("http://{addr}/v1"), count, handle)
}

async fn spawn_first_request_gated_server(
    model: &'static str,
) -> (
    String,
    Arc<AtomicUsize>,
    Arc<Notify>,
    Arc<Notify>,
    tokio::task::JoinHandle<()>,
) {
    use axum::{Json, Router, routing::post};

    let count = Arc::new(AtomicUsize::new(0));
    let first_started = Arc::new(Notify::new());
    let release_first = Arc::new(Notify::new());
    let count_for_handler = Arc::clone(&count);
    let first_started_for_handler = Arc::clone(&first_started);
    let release_first_for_handler = Arc::clone(&release_first);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let count = Arc::clone(&count_for_handler);
            let first_started = Arc::clone(&first_started_for_handler);
            let release_first = Arc::clone(&release_first_for_handler);
            async move {
                let request_index = count.fetch_add(1, Ordering::SeqCst);
                if request_index == 0 {
                    first_started.notify_one();
                    release_first.notified().await;
                }
                Json(serde_json::json!({
                    "model": model,
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "ok"
                        }
                    }],
                    "usage": {
                        "prompt_tokens": 1,
                        "completion_tokens": 1,
                        "total_tokens": 2
                    }
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gated LLM server");
    let addr = listener.local_addr().expect("server addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve gated LLM server");
    });
    (
        format!("http://{addr}/v1"),
        count,
        first_started,
        release_first,
        handle,
    )
}

async fn spawn_llm_cooldown_server() -> (
    String,
    Arc<AtomicUsize>,
    Arc<Notify>,
    tokio::task::JoinHandle<()>,
) {
    use axum::{
        Router,
        http::{StatusCode, header},
        routing::post,
    };

    let count = Arc::new(AtomicUsize::new(0));
    let called = Arc::new(Notify::new());
    let count_for_handler = Arc::clone(&count);
    let called_for_handler = Arc::clone(&called);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let count = Arc::clone(&count_for_handler);
            let called = Arc::clone(&called_for_handler);
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                called.notify_one();
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, "60")],
                    "cooling down",
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind LLM cooldown server");
    let addr = listener.local_addr().expect("server addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve LLM cooldown server");
    });
    (format!("http://{addr}/v1"), count, called, handle)
}

fn pool_config(entries: &[(&str, &str, &str, i32, usize)]) -> mempal::core::config::LlmConfig {
    let mut toml = String::from("[llm]\nenabled = true\n");
    for (id, base_url, model, priority, max_concurrent) in entries {
        toml.push_str(&format!(
            r#"
[[llm.endpoints]]
id = "{id}"
base_url = "{base_url}"
model = "{model}"
priority = {priority}
max_concurrent = {max_concurrent}
"#
        ));
    }
    Config::parse(&toml).expect("parse pool config").llm
}

#[tokio::test]
async fn test_llm_router_same_priority_uses_each_endpoint_capacity_concurrently() {
    let (qwen_url, qwen_count, qwen_server) =
        spawn_counting_server("qwen-model", Duration::from_millis(200)).await;
    let (spark_url, spark_count, spark_server) =
        spawn_counting_server("spark-model", Duration::from_millis(200)).await;
    let config = pool_config(&[
        ("qwen", &qwen_url, "qwen-model", 10, 4),
        ("spark", &spark_url, "spark-model", 10, 1),
    ]);
    let router = Arc::new(LlmRouter::from_config(&config).expect("build router"));
    assert_eq!(router.pool_capacity(), 5);
    let barrier = Arc::new(Barrier::new(6));
    let mut tasks = Vec::new();
    for _ in 0..5 {
        let router = Arc::clone(&router);
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            router.chat_completion(&request(), None).await
        }));
    }
    barrier.wait().await;
    for task in tasks {
        task.await
            .expect("join")
            .expect("same-priority routed response");
    }
    qwen_server.abort();
    spark_server.abort();
    let _ = qwen_server.await;
    let _ = spark_server.await;

    assert_eq!(qwen_count.load(Ordering::SeqCst), 4);
    assert_eq!(spark_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_llm_router_waits_for_saturated_healthy_endpoint_before_cooldown() {
    let (primary_url, primary_count, first_started, release_first, primary_server) =
        spawn_first_request_gated_server("primary-model").await;
    let (cooldown_url, cooldown_count, cooldown_called, cooldown_server) =
        spawn_llm_cooldown_server().await;
    let config = pool_config(&[
        ("primary", &primary_url, "primary-model", 0, 1),
        ("cooldown", &cooldown_url, "cooldown-model", 10, 1),
    ]);
    let router = Arc::new(LlmRouter::from_config(&config).expect("build router"));
    let first_router = Arc::clone(&router);
    let first_task =
        tokio::spawn(async move { first_router.chat_completion(&request(), None).await });
    first_started.notified().await;

    let second_router = Arc::clone(&router);
    let second_task =
        tokio::spawn(async move { second_router.chat_completion(&request(), None).await });
    cooldown_called.notified().await;
    release_first.notify_one();

    first_task
        .await
        .expect("first join")
        .expect("first LLM response");
    let second = tokio::time::timeout(Duration::from_secs(2), second_task)
        .await
        .expect("second request should complete after primary permit frees")
        .expect("second join")
        .expect("second LLM response");
    primary_server.abort();
    cooldown_server.abort();
    let _ = primary_server.await;
    let _ = cooldown_server.await;

    assert_eq!(second.endpoint_id, "primary");
    assert_eq!(second.endpoint_model, "primary-model");
    assert_eq!(primary_count.load(Ordering::SeqCst), 2);
    assert_eq!(cooldown_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_llm_router_uses_lower_priority_only_when_higher_priority_saturated() {
    let (primary_url, primary_count, primary_server) =
        spawn_counting_server("primary-model", Duration::from_millis(200)).await;
    let (spillover_url, spillover_count, spillover_server) =
        spawn_counting_server("spillover-model", Duration::from_millis(200)).await;
    let config = pool_config(&[
        ("primary", &primary_url, "primary-model", 0, 1),
        ("spillover", &spillover_url, "spillover-model", 10, 1),
    ]);
    let router = Arc::new(LlmRouter::from_config(&config).expect("build router"));
    let barrier = Arc::new(Barrier::new(3));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let router = Arc::clone(&router);
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            router.chat_completion(&request(), None).await
        }));
    }
    barrier.wait().await;
    for task in tasks {
        task.await
            .expect("join")
            .expect("spillover routed response");
    }
    primary_server.abort();
    spillover_server.abort();
    let _ = primary_server.await;
    let _ = spillover_server.await;

    assert_eq!(primary_count.load(Ordering::SeqCst), 1);
    assert_eq!(spillover_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_llm_router_mixed_retryable_failures_report_cooldown() {
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
        .expect_err("retryable endpoint failures should report a cooldown");

    primary_mock.assert_async().await;
    secondary_mock.assert_async().await;
    assert!(matches!(
        error,
        LlmError::TemporarilyUnavailable {
            retry_after,
            ..
        } if retry_after == Duration::from_secs(2)
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
