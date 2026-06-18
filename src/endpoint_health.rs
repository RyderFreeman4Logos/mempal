use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::Value;
use tokio::time::timeout;

use crate::core::config::{
    Config, EffectiveEmbedEndpoint, EffectiveLlmEndpoint, LlmConfig, RemoteCallPolicyConfig,
};
use crate::core::remote_calls::{RemoteCallService, blocked_remote_endpoint_error};

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointHealthSnapshot {
    pub embedding: ProbeStatus,
    /// Backward-compatible alias for LLM generation health.
    pub llm: ProbeStatus,
    /// Shallow control-plane probe (`/models`) for LLM endpoints.
    pub llm_control_plane: ProbeStatus,
    /// Deep generation probe (`/chat/completions`) for LLM endpoints.
    pub llm_generation: ProbeStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeStatus {
    pub reachable: bool,
    pub latency_ms: Option<u64>,
    pub detail: String,
}

impl ProbeStatus {
    fn reachable(latency_ms: Option<u64>, detail: String) -> Self {
        Self {
            reachable: true,
            latency_ms,
            detail,
        }
    }

    fn unreachable(detail: String) -> Self {
        Self {
            reachable: false,
            latency_ms: None,
            detail,
        }
    }

    pub fn display(&self) -> String {
        if self.reachable {
            match self.latency_ms {
                Some(latency_ms) => format!("reachable ({latency_ms}ms)"),
                None => format!("reachable ({})", self.detail),
            }
        } else {
            format!("unreachable ({})", self.detail)
        }
    }
}

pub async fn probe_endpoints(config: &Config) -> EndpointHealthSnapshot {
    let (embedding, (llm_control_plane, llm_generation)) =
        tokio::join!(probe_embedding(config), probe_llm(config));
    EndpointHealthSnapshot {
        embedding,
        llm: llm_generation.clone(),
        llm_control_plane,
        llm_generation,
    }
}

pub fn probe_endpoints_blocking(config: &Config) -> Result<EndpointHealthSnapshot> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build endpoint health runtime")?;
    Ok(runtime.block_on(probe_endpoints(config)))
}

/// Probe the LLM endpoint used by the daemon LLM worker.
///
/// `probe_endpoints` may report memory-intelligence LLM health when that
/// subsystem has a distinct endpoint. Queue recovery for `llm_task` rows must
/// instead follow `config.llm`, because that is the endpoint used to process
/// daemon LLM work.
pub async fn probe_daemon_llm_generation(config: &Config) -> ProbeStatus {
    let (_, generation) = probe_llm_config(&config.llm, &config.privacy.remote_calls).await;
    generation
}

async fn probe_embedding(config: &Config) -> ProbeStatus {
    match config.embed.backend.as_str() {
        "openai_compat" | "api" => {
            let endpoints = match config.embed.effective_endpoints() {
                Ok(endpoints) if !endpoints.is_empty() => endpoints,
                Ok(_) => return ProbeStatus::unreachable("missing base_url".to_string()),
                Err(error) => return ProbeStatus::unreachable(error.to_string()),
            };
            probe_embedding_endpoints(&endpoints, &config.privacy.remote_calls).await
        }
        backend => ProbeStatus::reachable(None, format!("local backend: {backend}")),
    }
}

async fn probe_embedding_endpoints(
    endpoints: &[EffectiveEmbedEndpoint],
    policy: &RemoteCallPolicyConfig,
) -> ProbeStatus {
    let mut failures = Vec::new();
    for endpoint in endpoints {
        if let Some(status) =
            blocked_probe_status(policy, RemoteCallService::Embedding, &endpoint.base_url)
        {
            failures.push(status.detail);
            continue;
        }
        let status =
            probe_models_endpoint(&endpoint.base_url, endpoint.api_key_env.as_deref(), None).await;
        if status.reachable {
            return ProbeStatus::reachable(
                status.latency_ms,
                format!("http probe via {}", endpoint.id),
            );
        }
        failures.push(format!("{}: {}", endpoint.id, status.detail));
    }
    ProbeStatus::unreachable(failures.join("; "))
}

async fn probe_llm(config: &Config) -> (ProbeStatus, ProbeStatus) {
    let effective_llm = if config.memory_intelligence.mode.uses_llm()
        && config
            .memory_intelligence
            .has_effective_llm_endpoint(&config.llm)
    {
        config.memory_intelligence.effective_llm_config(&config.llm)
    } else {
        config.llm.clone()
    };
    probe_llm_config(&effective_llm, &config.privacy.remote_calls).await
}

async fn probe_llm_config(
    effective_llm: &LlmConfig,
    policy: &RemoteCallPolicyConfig,
) -> (ProbeStatus, ProbeStatus) {
    if !effective_llm.enabled {
        let disabled = ProbeStatus::unreachable("disabled".to_string());
        return (disabled.clone(), disabled);
    }
    let endpoints = match effective_llm.effective_endpoints() {
        Ok(endpoints) if !endpoints.is_empty() => endpoints,
        Ok(_) => {
            let missing = ProbeStatus::unreachable("missing base_url".to_string());
            return (missing.clone(), missing);
        }
        Err(error) => {
            let failure = ProbeStatus::unreachable(error.to_string());
            return (failure.clone(), failure);
        }
    };
    probe_llm_endpoints(&endpoints, policy).await
}

async fn probe_llm_endpoints(
    endpoints: &[EffectiveLlmEndpoint],
    policy: &RemoteCallPolicyConfig,
) -> (ProbeStatus, ProbeStatus) {
    let (control_plane, generation) = tokio::join!(
        probe_llm_control_plane_endpoints(endpoints, policy),
        probe_llm_generation_endpoints(endpoints, policy)
    );
    (control_plane, generation)
}

async fn probe_llm_control_plane_endpoints(
    endpoints: &[EffectiveLlmEndpoint],
    policy: &RemoteCallPolicyConfig,
) -> ProbeStatus {
    let mut failures = Vec::new();
    for endpoint in endpoints {
        if let Some(status) =
            blocked_probe_status(policy, RemoteCallService::Llm, &endpoint.base_url)
        {
            failures.push(status.detail);
            continue;
        }
        let status = probe_models_endpoint(
            &endpoint.base_url,
            endpoint.api_key_env.as_deref(),
            endpoint.api_key.as_deref(),
        )
        .await;
        if status.reachable {
            return ProbeStatus::reachable(
                status.latency_ms,
                format!("http probe via {}", endpoint.id),
            );
        }
        failures.push(format!("{}: {}", endpoint.id, status.detail));
    }
    ProbeStatus::unreachable(failures.join("; "))
}

async fn probe_llm_generation_endpoints(
    endpoints: &[EffectiveLlmEndpoint],
    policy: &RemoteCallPolicyConfig,
) -> ProbeStatus {
    let mut failures = Vec::new();
    for endpoint in endpoints {
        if let Some(status) =
            blocked_probe_status(policy, RemoteCallService::Llm, &endpoint.base_url)
        {
            failures.push(status.detail);
            continue;
        }
        let status = probe_chat_completion_endpoint(endpoint).await;
        if status.reachable {
            return ProbeStatus::reachable(
                status.latency_ms,
                format!("generation probe via {}", endpoint.id),
            );
        }
        failures.push(format!("{}: {}", endpoint.id, status.detail));
    }
    ProbeStatus::unreachable(failures.join("; "))
}

fn blocked_probe_status(
    policy: &RemoteCallPolicyConfig,
    service: RemoteCallService,
    endpoint: &str,
) -> Option<ProbeStatus> {
    blocked_remote_endpoint_error(policy, service, endpoint)
        .map(|error| ProbeStatus::unreachable(format!("skipped: {error}")))
}

async fn probe_models_endpoint(
    base_url: &str,
    api_key_env: Option<&str>,
    direct_api_key: Option<&str>,
) -> ProbeStatus {
    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));
    match timeout(PROBE_TIMEOUT, async {
        let client = build_http_client(api_key_env, direct_api_key, PROBE_TIMEOUT)?;
        let started = Instant::now();
        let response = client
            .get(&endpoint)
            .send()
            .await
            .with_context(|| format!("request failed for {endpoint}"))?;
        response
            .error_for_status_ref()
            .with_context(|| format!("endpoint returned {}", response.status()))?;
        Ok::<u64, anyhow::Error>(started.elapsed().as_millis() as u64)
    })
    .await
    {
        Ok(Ok(latency_ms)) => ProbeStatus::reachable(Some(latency_ms), "http probe".to_string()),
        Ok(Err(error)) => ProbeStatus::unreachable(format!("{error:#}")),
        Err(_) => {
            ProbeStatus::unreachable(format!("timeout after {}ms", PROBE_TIMEOUT.as_millis()))
        }
    }
}

async fn probe_chat_completion_endpoint(endpoint: &EffectiveLlmEndpoint) -> ProbeStatus {
    let probe_timeout = Duration::from_secs(endpoint.health_probe_timeout_secs.max(1));
    let request = match build_chat_probe_body(endpoint) {
        Ok(request) => request,
        Err(error) => return ProbeStatus::unreachable(format!("{error:#}")),
    };
    let url = format!(
        "{}/chat/completions",
        endpoint.base_url.trim_end_matches('/')
    );
    match timeout(probe_timeout, async {
        let client = build_http_client(
            endpoint.api_key_env.as_deref(),
            endpoint.api_key.as_deref(),
            probe_timeout,
        )?;
        let started = Instant::now();
        let response = client
            .post(&url)
            .json(&request)
            .send()
            .await
            .with_context(|| format!("request failed for {url}"))?;
        response
            .error_for_status_ref()
            .with_context(|| format!("generation probe returned {}", response.status()))?;
        Ok::<u64, anyhow::Error>(started.elapsed().as_millis() as u64)
    })
    .await
    {
        Ok(Ok(latency_ms)) => {
            ProbeStatus::reachable(Some(latency_ms), "generation probe".to_string())
        }
        Ok(Err(error)) => ProbeStatus::unreachable(format!("{error:#}")),
        Err(_) => {
            ProbeStatus::unreachable(format!("timeout after {}ms", probe_timeout.as_millis()))
        }
    }
}

fn build_chat_probe_body(endpoint: &EffectiveLlmEndpoint) -> Result<Value> {
    let mut body = serde_json::Map::new();
    body.insert("model".to_string(), Value::String(endpoint.model.clone()));
    body.insert(
        "messages".to_string(),
        serde_json::json!([{ "role": "user", "content": "ping" }]),
    );
    body.insert("temperature".to_string(), serde_json::json!(0.0));
    body.insert("max_tokens".to_string(), serde_json::json!(1));
    if let Some(extra_body) = endpoint.extra_body.as_ref() {
        let Value::Object(extra) = extra_body else {
            anyhow::bail!("llm.extra_body must be a JSON object");
        };
        for (key, value) in extra {
            if matches!(
                key.as_str(),
                "model" | "messages" | "temperature" | "max_tokens"
            ) {
                continue;
            }
            body.insert(key.clone(), value.clone());
        }
    }
    Ok(Value::Object(body))
}

fn build_http_client(
    api_key_env: Option<&str>,
    direct_api_key: Option<&str>,
    request_timeout: Duration,
) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    if let Some(api_key) = resolve_api_key(api_key_env, direct_api_key)? {
        let value = HeaderValue::from_str(&format!("Bearer {api_key}"))
            .context("invalid Authorization header")?;
        headers.insert(AUTHORIZATION, value);
    }
    reqwest::Client::builder()
        .default_headers(headers)
        .timeout(request_timeout)
        .build()
        .context("failed to build health probe client")
}

fn resolve_api_key(
    api_key_env: Option<&str>,
    direct_api_key: Option<&str>,
) -> Result<Option<String>> {
    if let Some(api_key) = direct_api_key.filter(|value| !value.trim().is_empty()) {
        return Ok(Some(api_key.to_string()));
    }
    let Some(env_name) = api_key_env.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    match std::env::var(env_name) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("api key env `{env_name}` is not valid unicode")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::core::types::IntelligenceMode;

    #[tokio::test]
    async fn daemon_llm_generation_probe_ignores_memory_intelligence_endpoint_when_worker_disabled()
    {
        let mut config = Config::default();
        config.llm.enabled = false;
        config.memory_intelligence.mode = IntelligenceMode::LocalLlm;
        config.memory_intelligence.llm.base_url = Some("http://127.0.0.1:9/v1".to_string());
        config.memory_intelligence.llm.model = Some("memory-intelligence-model".to_string());

        let status = probe_daemon_llm_generation(&config).await;

        assert!(!status.reachable);
        assert_eq!(status.detail, "disabled");
    }
}
