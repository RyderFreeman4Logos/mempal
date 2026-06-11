use mempal::core::config::Config;
use mempal::core::types::IntelligenceMode;
use mempal::intelligence::IntelligenceRouter;
use mockito::Server;

fn llm_response(content: &str) -> String {
    serde_json::json!({
        "model": "local-test-model",
        "choices": [
            {
                "message": {
                    "role": "assistant",
                    "content": content
                }
            }
        ]
    })
    .to_string()
}

fn config_for(server: &Server, mode: &str) -> Config {
    Config::parse(&format!(
        r#"
[memory_intelligence]
mode = "{mode}"

[memory_intelligence.llm]
base_url = "{}/v1"
model = "local-test-model"
timeout_secs = 1
extra_body = {{ chat_template_kwargs = {{ enable_thinking = false }} }}
"#,
        server.url()
    ))
    .expect("parse config")
}

fn endpoint_pool_config_for(primary: &Server, secondary: &Server, mode: &str) -> Config {
    Config::parse(&format!(
        r#"
[memory_intelligence]
mode = "{mode}"

[memory_intelligence.llm]
timeout_secs = 1

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
    .expect("parse endpoint pool config")
}

#[test]
fn test_intelligence_mode_config_parsing() {
    for (raw, expected) in [
        ("deterministic", IntelligenceMode::Deterministic),
        ("local_llm", IntelligenceMode::LocalLlm),
        ("auto", IntelligenceMode::Auto),
    ] {
        let config = Config::parse(&format!(
            r#"
[memory_intelligence]
mode = "{raw}"
"#
        ))
        .expect("parse mode");

        assert_eq!(config.memory_intelligence.mode, expected);
    }
}

#[tokio::test]
async fn test_deterministic_mode_no_llm_calls() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .expect(0)
        .with_status(500)
        .create();
    let config = config_for(&server, "deterministic");
    let router = IntelligenceRouter::from_config(&config);

    let enhanced = router
        .enhance_ingest("Remember #architecture decision")
        .await;

    mock.assert();
    assert!(!enhanced.used_llm);
    assert_eq!(enhanced.tags, vec!["architecture".to_string()]);
    assert!(enhanced.candidate_facts.is_empty());
    assert!(enhanced.fallback_reason.is_none());
}

#[tokio::test]
async fn test_auto_mode_fallback() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(500)
        .with_body("unavailable")
        .create();
    let config = config_for(&server, "auto");
    let router = IntelligenceRouter::from_config(&config);

    let enhanced = router.enhance_ingest("Alice works at Acme").await;

    mock.assert();
    assert!(!enhanced.used_llm);
    assert_eq!(enhanced.raw_content, "Alice works at Acme");
    assert!(enhanced.candidate_facts.is_empty());
    assert!(enhanced.fallback_reason.is_some());
}

#[tokio::test]
async fn test_memory_intelligence_uses_endpoint_pool_fallback() {
    let mut primary = Server::new_async().await;
    let mut secondary = Server::new_async().await;
    let primary_mock = primary
        .mock("POST", "/v1/chat/completions")
        .with_status(500)
        .with_body("primary unavailable")
        .create_async()
        .await;
    let supported = serde_json::json!({
        "candidate_facts": ["Alice works at Acme"],
        "tags": ["employment"],
        "contradiction": false,
        "correction": false
    })
    .to_string();
    let secondary_mock = secondary
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(llm_response(&supported))
        .create_async()
        .await;
    let config = endpoint_pool_config_for(&primary, &secondary, "local_llm");
    let router = IntelligenceRouter::from_config(&config);

    let enhanced = router.enhance_ingest("Alice works at Acme").await;

    primary_mock.assert_async().await;
    secondary_mock.assert_async().await;
    assert!(enhanced.used_llm);
    assert_eq!(enhanced.candidate_facts, vec!["Alice works at Acme"]);
}

#[tokio::test]
async fn test_llm_output_gate_rejects_hallucination() {
    let mut server = Server::new_async().await;
    let hallucinated = serde_json::json!({
        "candidate_facts": ["Bob works at Acme"],
        "tags": ["employment"],
        "contradiction": false,
        "correction": false
    })
    .to_string();
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(llm_response(&hallucinated))
        .create();
    let config = config_for(&server, "local_llm");
    let router = IntelligenceRouter::from_config(&config);

    let enhanced = router.enhance_ingest("Alice works at Acme").await;

    mock.assert();
    assert!(!enhanced.used_llm);
    assert!(enhanced.candidate_facts.is_empty());
    assert!(
        enhanced
            .fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("unsupported fact"))
    );
}
