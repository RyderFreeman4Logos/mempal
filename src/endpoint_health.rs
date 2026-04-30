use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use tokio::time::timeout;

use crate::core::config::Config;

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointHealthSnapshot {
    pub embedding: ProbeStatus,
    pub llm: ProbeStatus,
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
    let (embedding, llm) = tokio::join!(probe_embedding(config), probe_llm(config));
    EndpointHealthSnapshot { embedding, llm }
}

pub fn probe_endpoints_blocking(config: &Config) -> Result<EndpointHealthSnapshot> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build endpoint health runtime")?;
    Ok(runtime.block_on(probe_endpoints(config)))
}

async fn probe_embedding(config: &Config) -> ProbeStatus {
    match config.embed.backend.as_str() {
        "openai_compat" | "api" => {
            let Some(base_url) = config.embed.resolved_openai_base_url() else {
                return ProbeStatus::unreachable("missing base_url".to_string());
            };
            probe_models_endpoint(base_url, config.embed.resolved_api_key_env(), None).await
        }
        backend => ProbeStatus::reachable(None, format!("local backend: {backend}")),
    }
}

async fn probe_llm(config: &Config) -> ProbeStatus {
    if !config.llm.enabled {
        return ProbeStatus::unreachable("disabled".to_string());
    }
    let Some(base_url) = config
        .llm
        .base_url
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return ProbeStatus::unreachable("missing base_url".to_string());
    };
    probe_models_endpoint(
        base_url,
        config.llm.api_key_env.as_deref(),
        config.llm.api_key.as_deref(),
    )
    .await
}

async fn probe_models_endpoint(
    base_url: &str,
    api_key_env: Option<&str>,
    direct_api_key: Option<&str>,
) -> ProbeStatus {
    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));
    match timeout(PROBE_TIMEOUT, async {
        let client = build_http_client(api_key_env, direct_api_key)?;
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

fn build_http_client(
    api_key_env: Option<&str>,
    direct_api_key: Option<&str>,
) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    if let Some(api_key) = resolve_api_key(api_key_env, direct_api_key)? {
        let value = HeaderValue::from_str(&format!("Bearer {api_key}"))
            .context("invalid Authorization header")?;
        headers.insert(AUTHORIZATION, value);
    }
    reqwest::Client::builder()
        .default_headers(headers)
        .timeout(PROBE_TIMEOUT)
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
