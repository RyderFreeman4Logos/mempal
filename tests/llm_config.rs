use mempal::core::config::{Config, ConfigError, LlmConfig};

#[test]
fn test_llm_config_default_disabled() {
    let config = LlmConfig::default();

    assert!(!config.enabled);
    assert_eq!(config.backend, "openai_compat");
    assert_eq!(config.request_timeout_secs, 30);
    assert_eq!(config.retry_interval_secs, 2);
    assert_eq!(config.max_concurrent, 16);
    assert_eq!(config.enabled_for, vec!["gating".to_string()]);
}

#[test]
fn test_config_without_llm_section_parses_ok() {
    let config = Config::parse("").expect("config without llm section should parse");

    assert!(!config.llm.enabled);
}

#[test]
fn test_llm_config_parse_full() {
    let config = Config::parse(
        r#"
[llm]
enabled = true
backend = "openai_compat"
base_url = "http://localhost:8317/v1"
model = "qwen35-35b-a3b"
api_key_env = "LOCAL_ROUTER_API_KEY"
request_timeout_secs = 45
retry_interval_secs = 3
max_concurrent = 8
enabled_for = ["gating", "distill", "compress"]
"#,
    )
    .expect("full llm config should parse");

    assert!(config.llm.enabled);
    assert_eq!(config.llm.backend, "openai_compat");
    assert_eq!(
        config.llm.base_url.as_deref(),
        Some("http://localhost:8317/v1")
    );
    assert_eq!(config.llm.model.as_deref(), Some("qwen35-35b-a3b"));
    assert_eq!(
        config.llm.api_key_env.as_deref(),
        Some("LOCAL_ROUTER_API_KEY")
    );
    assert_eq!(config.llm.request_timeout_secs, 45);
    assert_eq!(config.llm.retry_interval_secs, 3);
    assert_eq!(config.llm.max_concurrent, 8);
    assert_eq!(
        config.llm.enabled_for,
        vec![
            "gating".to_string(),
            "distill".to_string(),
            "compress".to_string()
        ]
    );
}

#[test]
fn test_llm_config_missing_base_url_when_enabled() {
    let err = Config::parse(
        r#"
[llm]
enabled = true
"#,
    )
    .expect_err("enabled llm without base_url should fail validation");

    match err {
        ConfigError::Validation(message) => {
            assert!(message.contains("llm.base_url"), "{message}");
        }
        other => panic!("expected ConfigError::Validation, got {other:?}"),
    }
}

#[test]
fn test_llm_restart_required_fields() {
    let config = Config::default();
    let mut changed = config.clone();
    changed.llm.base_url = Some("http://localhost:8317/v1".to_string());
    changed.llm.model = Some("qwen35-35b-a3b".to_string());
    changed.llm.api_key_env = Some("LOCAL_ROUTER_API_KEY".to_string());

    let fields = config.restart_required_fields_changed(&changed);

    assert!(fields.contains(&"llm.base_url"));
    assert!(fields.contains(&"llm.model"));
    assert!(fields.contains(&"llm.api_key_env"));
}

#[test]
fn test_llm_runtime_allowed_fields_hot_reload() {
    let mut current = Config::default();
    current.llm.base_url = Some("http://localhost:8317/v1".to_string());
    current.llm.model = Some("qwen35-35b-a3b".to_string());
    current.llm.api_key_env = Some("LOCAL_ROUTER_API_KEY".to_string());

    let mut candidate = current.clone();
    candidate.llm.base_url = Some("http://localhost:9000/v1".to_string());
    candidate.llm.model = Some("other-model".to_string());
    candidate.llm.api_key_env = Some("OTHER_KEY".to_string());
    candidate.llm.max_concurrent = 4;
    candidate.llm.enabled_for = vec!["gating".to_string(), "distill".to_string()];

    let effective = current.merge_runtime_allowed(&candidate);

    assert_eq!(effective.llm.base_url, current.llm.base_url);
    assert_eq!(effective.llm.model, current.llm.model);
    assert_eq!(effective.llm.api_key_env, current.llm.api_key_env);
    assert_eq!(effective.llm.max_concurrent, 4);
    assert_eq!(
        effective.llm.enabled_for,
        vec!["gating".to_string(), "distill".to_string()]
    );
}
