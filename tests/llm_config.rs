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
fn test_memory_intelligence_config_parse_full() {
    let config = Config::parse(
        r#"
[memory_intelligence]
mode = "local_llm"

[memory_intelligence.llm]
base_url = "http://127.0.0.1:18009/v1"
model = "qwen3.6-27b-decensor-by-aeon"
timeout_secs = 1800
extra_body = { chat_template_kwargs = { enable_thinking = false } }
"#,
    )
    .expect("memory intelligence config should parse");

    assert_eq!(config.memory_intelligence.mode.to_string(), "local_llm");
    let effective = config.memory_intelligence.effective_llm_config(&config.llm);
    assert_eq!(
        effective.base_url.as_deref(),
        Some("http://127.0.0.1:18009/v1")
    );
    assert_eq!(
        effective.model.as_deref(),
        Some("qwen3.6-27b-decensor-by-aeon")
    );
    assert_eq!(effective.request_timeout_secs, 1800);
    assert_eq!(
        effective
            .extra_body
            .as_ref()
            .and_then(|body| body.pointer("/chat_template_kwargs/enable_thinking"))
            .and_then(|value| value.as_bool()),
        Some(false)
    );
}

#[test]
fn test_cloud_llm_memory_intelligence_mode_is_rejected() {
    let err = Config::parse(
        r#"
[memory_intelligence]
mode = "cloud_llm"
"#,
    )
    .expect_err("cloud_llm mode must not parse");

    match err {
        ConfigError::Parse(source) => {
            assert!(source.to_string().contains("cloud_llm"), "{source}");
        }
        other => panic!("expected ConfigError::Parse, got {other:?}"),
    }
}

#[test]
fn test_external_llm_base_url_warns_but_does_not_block_config() {
    let config = Config::parse(
        r#"
[memory_intelligence]
mode = "local_llm"

[memory_intelligence.llm]
base_url = "https://api.example.com/v1"
model = "operator-owned-model"
"#,
    )
    .expect("external LLM endpoint warning must not block config parsing");

    let warnings = config.collect_runtime_warnings();
    assert!(
        warnings.iter().any(|warning| {
            warning.source == "llm"
                && warning.message.contains("memory_intelligence.llm.base_url")
                && warning.message.contains("api.example.com")
        }),
        "{warnings:?}"
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
    changed.llm.enabled = true;
    changed.llm.backend = "other_backend".to_string();
    changed.llm.base_url = Some("http://localhost:8317/v1".to_string());
    changed.llm.model = Some("qwen35-35b-a3b".to_string());
    changed.llm.api_key = Some("direct-key".to_string());
    changed.llm.api_key_env = Some("LOCAL_ROUTER_API_KEY".to_string());
    changed.llm.extra_body = Some(serde_json::json!({"foo": "bar"}));

    let fields = config.restart_required_fields_changed(&changed);

    assert!(fields.contains(&"llm.enabled"));
    assert!(fields.contains(&"llm.backend"));
    // LLM endpoint/client request fields are hot-reloadable.
    assert!(!fields.contains(&"llm.base_url"));
    assert!(!fields.contains(&"llm.model"));
    assert!(!fields.contains(&"llm.api_key"));
    assert!(!fields.contains(&"llm.api_key_env"));
    assert!(!fields.contains(&"llm.extra_body"));
}

#[test]
fn test_llm_runtime_allowed_fields_hot_reload() {
    let mut current = Config::default();
    current.llm.enabled = true;
    current.llm.backend = "openai_compat".to_string();
    current.llm.base_url = Some("http://localhost:8317/v1".to_string());
    current.llm.model = Some("qwen35-35b-a3b".to_string());
    current.llm.api_key = Some("direct-key".to_string());
    current.llm.api_key_env = Some("LOCAL_ROUTER_API_KEY".to_string());

    let mut candidate = current.clone();
    candidate.llm.base_url = Some("http://localhost:9000/v1".to_string());
    candidate.llm.model = Some("other-model".to_string());
    candidate.llm.api_key = Some("other-direct-key".to_string());
    candidate.llm.api_key_env = Some("OTHER_KEY".to_string());
    candidate.llm.extra_body =
        Some(serde_json::json!({"chat_template_kwargs": {"enable_thinking": false}}));
    candidate.llm.max_concurrent = 4;
    candidate.llm.enabled_for = vec!["gating".to_string(), "distill".to_string()];

    let effective = current.merge_runtime_allowed(&candidate);

    assert_eq!(effective.llm.enabled, current.llm.enabled);
    assert_eq!(effective.llm.backend, current.llm.backend);
    assert_eq!(effective.llm.base_url, candidate.llm.base_url);
    assert_eq!(effective.llm.model, candidate.llm.model);
    assert_eq!(effective.llm.api_key, candidate.llm.api_key);
    assert_eq!(effective.llm.api_key_env, candidate.llm.api_key_env);
    assert_eq!(effective.llm.extra_body, candidate.llm.extra_body);
    assert_eq!(effective.llm.max_concurrent, 4);
    assert_eq!(
        effective.llm.enabled_for,
        vec!["gating".to_string(), "distill".to_string()]
    );
}

#[test]
fn test_llm_lifecycle_change_preserves_live_client_config() {
    let mut current = Config::default();
    current.llm.enabled = true;
    current.llm.backend = "openai_compat".to_string();
    current.llm.base_url = Some("http://localhost:8317/v1".to_string());
    current.llm.model = Some("qwen35-35b-a3b".to_string());
    current.llm.api_key = Some("direct-key".to_string());
    current.llm.api_key_env = Some("LOCAL_ROUTER_API_KEY".to_string());
    current.llm.max_concurrent = 2;

    let mut candidate = current.clone();
    candidate.llm.enabled = false;
    candidate.llm.base_url = None;
    candidate.llm.model = None;
    candidate.llm.api_key = None;
    candidate.llm.api_key_env = None;
    candidate.llm.extra_body = None;
    candidate.llm.max_concurrent = 4;
    candidate.llm.enabled_for = vec!["distill".to_string()];

    let effective = current.merge_runtime_allowed(&candidate);

    assert_eq!(effective.llm, current.llm);
}
