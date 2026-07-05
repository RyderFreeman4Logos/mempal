use mempal::core::config::{Config, ConfigError, LlmConfig};

#[test]
fn test_llm_config_default_disabled() {
    let config = LlmConfig::default();

    assert!(!config.enabled);
    assert_eq!(config.backend, "openai_compat");
    assert_eq!(config.request_timeout_secs, 240);
    assert_eq!(config.health_probe_timeout_secs, 10);
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
health_probe_timeout_secs = 5
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
    assert_eq!(config.llm.health_probe_timeout_secs, 5);
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
fn test_llm_config_legacy_scalar_config_yields_one_effective_endpoint() {
    let config = Config::parse(
        r#"
[llm]
enabled = true
base_url = "http://localhost:8317/v1"
model = "qwen35-35b-a3b"
api_key_env = "LOCAL_ROUTER_API_KEY"
request_timeout_secs = 45
health_probe_timeout_secs = 6
max_concurrent = 8
"#,
    )
    .expect("legacy llm config should parse");

    let endpoints = config
        .llm
        .effective_endpoints()
        .expect("legacy endpoint should normalize");

    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].id, "legacy");
    assert_eq!(endpoints[0].base_url, "http://localhost:8317/v1");
    assert_eq!(endpoints[0].model, "qwen35-35b-a3b");
    assert_eq!(
        endpoints[0].api_key_env.as_deref(),
        Some("LOCAL_ROUTER_API_KEY")
    );
    assert_eq!(endpoints[0].request_timeout_secs, 45);
    assert_eq!(endpoints[0].health_probe_timeout_secs, 6);
    assert_eq!(endpoints[0].max_concurrent, 8);
}

#[test]
fn test_llm_config_endpoint_list_yields_stable_effective_endpoint_ids() {
    let config = Config::parse(
        r#"
[llm]
enabled = true

[[llm.endpoints]]
base_url = "http://primary.local:8317/v1/"
model = "primary-model"
priority = 10

[[llm.endpoints]]
id = "lan.backup-1"
base_url = "http://backup.local:8317/v1"
model = "backup-model"
priority = 20
request_timeout_secs = 12
health_probe_timeout_secs = 4
retry_interval_secs = 4
max_concurrent = 3
"#,
    )
    .expect("endpoint list should parse");

    let endpoints = config
        .llm
        .effective_endpoints()
        .expect("endpoint list should normalize");

    assert_eq!(endpoints.len(), 2);
    assert_eq!(endpoints[0].id, "endpoint-1");
    assert_eq!(endpoints[0].base_url, "http://primary.local:8317/v1");
    assert_eq!(endpoints[0].model, "primary-model");
    assert_eq!(endpoints[0].priority, 10);
    assert_eq!(endpoints[0].request_timeout_secs, 240);
    assert_eq!(endpoints[0].health_probe_timeout_secs, 10);
    assert_eq!(endpoints[0].retry_interval_secs, 2);
    assert_eq!(endpoints[0].max_concurrent, 16);
    assert_eq!(endpoints[1].id, "lan.backup-1");
    assert_eq!(endpoints[1].priority, 20);
    assert_eq!(endpoints[1].request_timeout_secs, 12);
    assert_eq!(endpoints[1].health_probe_timeout_secs, 4);
    assert_eq!(endpoints[1].retry_interval_secs, 4);
    assert_eq!(endpoints[1].max_concurrent, 3);
    assert_eq!(config.llm.pool_capacity(), 19);
}

#[test]
fn test_llm_config_endpoint_list_reports_effective_status_summaries() {
    let config = Config::parse(
        r#"
[llm]
enabled = true

[[llm.endpoints]]
id = "primary"
base_url = "http://primary.local:8317/v1"
model = "primary-model"

[[llm.endpoints]]
id = "secondary"
base_url = "http://secondary.local:8317/v1"
model = "secondary-model"
"#,
    )
    .expect("endpoint list should parse");

    assert_eq!(
        config.llm.effective_model_summary().as_deref(),
        Some("primary=primary-model, secondary=secondary-model")
    );
    assert_eq!(
        config.llm.effective_base_url_summary().as_deref(),
        Some("primary=http://primary.local:8317/v1, secondary=http://secondary.local:8317/v1")
    );
}

#[test]
fn test_embed_config_endpoint_pool_yields_effective_endpoints() {
    let config = Config::parse(
        r#"
[embed]
backend = "openai_compat"

[[embed.endpoints]]
id = "gb10"
base_url = "http://gb10.local:18002/v1/"
model = "Qwen/Qwen3-Embedding-8B"
priority = 0
max_concurrent = 4

[[embed.endpoints]]
id = "spark"
base_url = "http://spark.local:18002/v1"
model = "Qwen/Qwen3-Embedding-8B"
priority = 0
request_timeout_secs = 7
retry_interval_secs = 3
max_concurrent = 2
"#,
    )
    .expect("embedding endpoint list should parse");

    let endpoints = config
        .embed
        .effective_endpoints()
        .expect("embedding endpoint list should normalize");

    assert_eq!(endpoints.len(), 2);
    assert_eq!(endpoints[0].id, "gb10");
    assert_eq!(endpoints[0].base_url, "http://gb10.local:18002/v1");
    assert_eq!(endpoints[0].model, "Qwen/Qwen3-Embedding-8B");
    assert_eq!(endpoints[0].dimensions, 4096);
    assert_eq!(endpoints[0].priority, 0);
    assert_eq!(endpoints[0].request_timeout_secs, 240);
    assert_eq!(endpoints[0].retry_interval_secs, 2);
    assert_eq!(endpoints[0].max_concurrent, 4);
    assert_eq!(endpoints[1].id, "spark");
    assert_eq!(endpoints[1].request_timeout_secs, 7);
    assert_eq!(endpoints[1].retry_interval_secs, 3);
    assert_eq!(endpoints[1].max_concurrent, 2);
    assert_eq!(config.embed.pool_capacity(), 6);
    assert_eq!(
        config.embed.effective_model_summary().as_deref(),
        Some("gb10=Qwen/Qwen3-Embedding-8B, spark=Qwen/Qwen3-Embedding-8B")
    );
}

#[test]
fn test_embed_endpoint_pool_rejects_mixed_vector_identity() {
    let error = Config::parse(
        r#"
[embed]
backend = "openai_compat"

[[embed.endpoints]]
id = "gb10"
base_url = "http://gb10.local:18002/v1"
model = "Qwen/Qwen3-Embedding-8B"
dim = 4096

[[embed.endpoints]]
id = "spark"
base_url = "http://spark.local:18002/v1"
model = "other-embedding-model"
dim = 4096
"#,
    )
    .expect_err("mixed embedding models must not parse");

    assert!(
        error
            .to_string()
            .contains("embed endpoints must share one vector identity"),
        "{error}"
    );
}

#[test]
fn test_embed_endpoint_pool_runtime_fields_hot_reload_when_identity_matches() {
    let current = Config::parse(
        r#"
[embed]
backend = "openai_compat"

[[embed.endpoints]]
id = "gb10"
base_url = "http://gb10.local:18002/v1"
model = "Qwen/Qwen3-Embedding-8B"
dim = 4096
max_concurrent = 1
"#,
    )
    .expect("current config");
    let candidate = Config::parse(
        r#"
[embed]
backend = "openai_compat"

[[embed.endpoints]]
id = "gb10"
base_url = "http://spark.local:18002/v1"
model = "Qwen/Qwen3-Embedding-8B"
dim = 4096
max_concurrent = 3
retry_interval_secs = 5
"#,
    )
    .expect("candidate config");

    assert!(
        !current
            .restart_required_fields_changed(&candidate)
            .contains(&"embedder.endpoints.vector_identity")
    );
    let effective = current.merge_runtime_allowed(&candidate);
    let endpoints = effective
        .embed
        .effective_endpoints()
        .expect("effective endpoints");

    assert_eq!(endpoints[0].base_url, "http://spark.local:18002/v1");
    assert_eq!(endpoints[0].max_concurrent, 3);
    assert_eq!(endpoints[0].retry_interval_secs, 5);
}

#[test]
fn test_embed_endpoint_pool_vector_identity_change_requires_restart() {
    let current = Config::parse(
        r#"
[embed]
backend = "openai_compat"

[[embed.endpoints]]
id = "gb10"
base_url = "http://gb10.local:18002/v1"
model = "Qwen/Qwen3-Embedding-8B"
dim = 4096
"#,
    )
    .expect("current config");
    let candidate = Config::parse(
        r#"
[embed]
backend = "openai_compat"

[[embed.endpoints]]
id = "gb10"
base_url = "http://gb10.local:18002/v1"
model = "Qwen/Qwen3-Embedding-8B"
dim = 1024
"#,
    )
    .expect("candidate config");

    assert!(
        current
            .restart_required_fields_changed(&candidate)
            .contains(&"embedder.endpoints.vector_identity")
    );
    let effective = current.merge_runtime_allowed(&candidate);
    let endpoints = effective
        .embed
        .effective_endpoints()
        .expect("effective endpoints");

    assert_eq!(endpoints[0].dimensions, 4096);
}

#[test]
fn test_endpoint_pool_external_llm_base_url_warns_but_does_not_block_config() {
    let config = Config::parse(
        r#"
[llm]
enabled = true

[[llm.endpoints]]
id = "local"
base_url = "http://localhost:8317/v1"
model = "local-model"

[[llm.endpoints]]
id = "cloud"
base_url = "https://api.example.com/v1"
model = "operator-owned-model"
"#,
    )
    .expect("external endpoint pool warning must not block config parsing");

    let warnings = config.collect_runtime_warnings();
    assert!(
        warnings.iter().any(|warning| {
            warning.source == "llm"
                && warning.message.contains("llm.endpoints[cloud].base_url")
                && warning.message.contains("api.example.com")
        }),
        "{warnings:?}"
    );
}

#[test]
fn test_llm_config_scalar_and_endpoint_list_conflict_fails_validation() {
    let err = Config::parse(
        r#"
[llm]
enabled = true
base_url = "http://localhost:8317/v1"
model = "legacy-model"

[[llm.endpoints]]
base_url = "http://backup.local:8317/v1"
model = "backup-model"
"#,
    )
    .expect_err("scalar endpoint and endpoint list must conflict");

    match err {
        ConfigError::Validation(message) => {
            assert!(
                message.contains("legacy scalar endpoint fields"),
                "{message}"
            );
            assert!(message.contains("base_url"), "{message}");
            assert!(message.contains("model"), "{message}");
        }
        other => panic!("expected ConfigError::Validation, got {other:?}"),
    }
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
fn test_llm_config_memory_intelligence_inherits_endpoint_pool_with_request_overrides() {
    let config = Config::parse(
        r#"
[llm]
enabled = true
request_timeout_secs = 30

[[llm.endpoints]]
id = "primary"
base_url = "http://primary.local:8317/v1"
model = "primary-model"

[[llm.endpoints]]
id = "secondary"
base_url = "http://secondary.local:8317/v1"
model = "secondary-model"

[memory_intelligence]
mode = "local_llm"

[memory_intelligence.llm]
api_key = "mi-direct-key"
api_key_env = "MI_LLM_API_KEY"
timeout_secs = 7
extra_body = { chat_template_kwargs = { enable_thinking = false } }
"#,
    )
    .expect("memory intelligence endpoint-pool overlay should parse");

    assert!(
        config
            .memory_intelligence
            .has_effective_llm_endpoint(&config.llm)
    );
    let effective = config.memory_intelligence.effective_llm_config(&config.llm);
    let endpoints = effective
        .effective_endpoints()
        .expect("memory intelligence endpoint pool should normalize");

    assert_eq!(endpoints.len(), 2);
    assert_eq!(endpoints[0].id, "primary");
    assert_eq!(endpoints[0].base_url, "http://primary.local:8317/v1");
    assert_eq!(endpoints[0].model, "primary-model");
    assert_eq!(endpoints[1].id, "secondary");
    assert_eq!(endpoints[1].base_url, "http://secondary.local:8317/v1");
    assert_eq!(endpoints[1].model, "secondary-model");
    for endpoint in endpoints {
        assert_eq!(endpoint.api_key.as_deref(), Some("mi-direct-key"));
        assert_eq!(endpoint.api_key_env.as_deref(), Some("MI_LLM_API_KEY"));
        assert_eq!(endpoint.request_timeout_secs, 7);
        assert_eq!(
            endpoint
                .extra_body
                .as_ref()
                .and_then(|body| body.pointer("/chat_template_kwargs/enable_thinking"))
                .and_then(|value| value.as_bool()),
            Some(false)
        );
    }
}

#[test]
fn test_llm_config_memory_intelligence_api_key_env_override_replaces_inherited_direct_key() {
    let config = Config::parse(
        r#"
[llm]
enabled = true

[[llm.endpoints]]
id = "primary"
base_url = "http://primary.local:8317/v1"
model = "primary-model"
api_key = "base-direct-key"

[memory_intelligence]
mode = "local_llm"

[memory_intelligence.llm]
api_key_env = "MI_LLM_API_KEY"
"#,
    )
    .expect("memory intelligence api_key_env overlay should parse");

    let effective = config.memory_intelligence.effective_llm_config(&config.llm);
    let endpoints = effective
        .effective_endpoints()
        .expect("memory intelligence endpoint pool should normalize");

    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].api_key, None);
    assert_eq!(endpoints[0].api_key_env.as_deref(), Some("MI_LLM_API_KEY"));
}

#[test]
fn test_llm_config_memory_intelligence_endpoint_identity_override_replaces_base_endpoint_pool() {
    let config = Config::parse(
        r#"
[llm]
enabled = true

[[llm.endpoints]]
id = "base"
base_url = "http://base.local:8317/v1"
model = "base-model"

[memory_intelligence]
mode = "local_llm"

[memory_intelligence.llm]
base_url = "http://mi.local:18009/v1"
model = "mi-model"
"#,
    )
    .expect("memory intelligence endpoint identity override should parse");

    let effective = config.memory_intelligence.effective_llm_config(&config.llm);
    let endpoints = effective
        .effective_endpoints()
        .expect("memory intelligence identity endpoint should normalize");

    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].id, "legacy");
    assert_eq!(endpoints[0].base_url, "http://mi.local:18009/v1");
    assert_eq!(endpoints[0].model, "mi-model");
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
fn test_llm_judge_disabled_requires_explicit_quality_degradation_opt_in() {
    let err = Config::parse(
        r#"
[gating.llm_judge]
enabled = false
"#,
    )
    .expect_err("disabled LLM judge must require explicit unsafe opt-in");

    match err {
        ConfigError::Validation(message) => {
            assert!(
                message.contains("allow_fallback_worse_memory=true"),
                "{message}"
            );
        }
        other => panic!("expected ConfigError::Validation, got {other:?}"),
    }
}

#[test]
fn test_llm_judge_disabled_with_explicit_opt_in_warns() {
    let config = Config::parse(
        r#"
[gating.llm_judge]
enabled = false
allow_fallback_worse_memory = true
"#,
    )
    .expect("explicit unsafe opt-in should parse");

    let warnings = config.collect_runtime_warnings();
    assert!(
        warnings.iter().any(|warning| {
            warning.source == "llm" && warning.message.contains("allow_fallback_worse_memory=true")
        }),
        "{warnings:?}"
    );
}

#[test]
fn test_llm_judge_enabled_requires_llm_gating_endpoint() {
    let err = Config::parse(
        r#"
[gating.llm_judge]
enabled = true
"#,
    )
    .expect_err("enabled LLM judge without LLM endpoint must fail");

    match err {
        ConfigError::Validation(message) => {
            assert!(message.contains("[llm].enabled=true"), "{message}");
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
