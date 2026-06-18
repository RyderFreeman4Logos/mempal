use std::net::{Ipv4Addr, Ipv6Addr};

use reqwest::Url;
use serde::Serialize;
use thiserror::Error;

use super::config::{
    Config, EffectiveEmbedEndpoint, EffectiveLlmEndpoint, LlmConfig, RemoteCallPolicyConfig,
    SearchRerankerConfig, endpoint_url_display_label, normalize_reranker_endpoint_url,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCallService {
    Embedding,
    Llm,
    Rerank,
}

impl RemoteCallService {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Embedding => "embedding",
            Self::Llm => "llm",
            Self::Rerank => "rerank",
        }
    }

    fn allow_field(self) -> &'static str {
        match self {
            Self::Embedding => "allow_embedding",
            Self::Llm => "allow_llm",
            Self::Rerank => "allow_rerank",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCallStatus {
    DefaultLocal,
    LocalModel,
    Disabled,
    LocalEndpoint,
    RemoteEndpoint,
    Misconfigured,
}

impl RemoteCallStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DefaultLocal => "default_local",
            Self::LocalModel => "local_model",
            Self::Disabled => "disabled",
            Self::LocalEndpoint => "local_endpoint",
            Self::RemoteEndpoint => "remote_endpoint",
            Self::Misconfigured => "misconfigured",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCallPolicyEffect {
    Allowed,
    BlockedByPolicy,
    NotApplicable,
}

impl RemoteCallPolicyEffect {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::BlockedByPolicy => "blocked_by_policy",
            Self::NotApplicable => "not_applicable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteCallReport {
    pub policy: RemoteCallPolicyReport,
    pub services: Vec<RemoteCallServiceReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteCallPolicyReport {
    pub fail_closed: bool,
    pub allow_embedding: bool,
    pub allow_llm: bool,
    pub allow_rerank: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteCallServiceReport {
    pub service: RemoteCallService,
    pub status: RemoteCallStatus,
    pub policy: RemoteCallPolicyEffect,
    pub endpoint: Option<String>,
    pub detail: String,
}

impl RemoteCallServiceReport {
    pub fn service_name(&self) -> &'static str {
        self.service.as_str()
    }

    pub fn status_name(&self) -> &'static str {
        self.status.as_str()
    }

    pub fn policy_name(&self) -> &'static str {
        self.policy.as_str()
    }

    fn blocks_remote_call(&self) -> bool {
        self.status == RemoteCallStatus::RemoteEndpoint
            && self.policy == RemoteCallPolicyEffect::BlockedByPolicy
    }
}

pub const BLOCKED_REMOTE_ENDPOINT_LABEL: &str = "<remote-endpoint>";

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "remote {service} calls are blocked by privacy.remote_calls.fail_closed; set privacy.remote_calls.{allow_field}=true to allow {endpoint}"
)]
pub struct RemoteCallPolicyError {
    pub service: &'static str,
    pub allow_field: &'static str,
    pub endpoint: String,
}

pub fn build_remote_call_report(config: &Config) -> RemoteCallReport {
    RemoteCallReport {
        policy: RemoteCallPolicyReport::from(&config.privacy.remote_calls),
        services: vec![
            embedding_status(config),
            llm_status(config),
            rerank_status(&config.privacy.remote_calls, &config.search.reranker),
        ],
    }
}

pub fn ensure_embedding_allowed(config: &Config) -> Result<(), RemoteCallPolicyError> {
    let status = embedding_status(config);
    ensure_status_allowed(&status)
}

pub fn ensure_llm_allowed(
    config: &Config,
    llm_config: &LlmConfig,
) -> Result<(), RemoteCallPolicyError> {
    ensure_llm_allowed_for_policy(&config.privacy.remote_calls, llm_config)
}

pub fn ensure_llm_allowed_for_policy(
    policy: &RemoteCallPolicyConfig,
    llm_config: &LlmConfig,
) -> Result<(), RemoteCallPolicyError> {
    let mut active_config = llm_config.clone();
    active_config.enabled = true;
    let status = llm_config_status(policy, &active_config);
    ensure_status_allowed(&status)
}

pub fn ensure_rerank_allowed(
    policy: &RemoteCallPolicyConfig,
    reranker: &SearchRerankerConfig,
) -> Result<(), RemoteCallPolicyError> {
    let status = rerank_status(policy, reranker);
    ensure_status_allowed(&status)
}

pub fn blocked_remote_endpoint_error(
    policy: &RemoteCallPolicyConfig,
    service: RemoteCallService,
    endpoint: &str,
) -> Option<RemoteCallPolicyError> {
    if !remote_endpoint_is_blocked_by_policy(policy, service, endpoint) {
        return None;
    }
    Some(RemoteCallPolicyError {
        service: service.as_str(),
        allow_field: service.allow_field(),
        endpoint: BLOCKED_REMOTE_ENDPOINT_LABEL.to_string(),
    })
}

pub fn endpoint_policy_display_label(
    policy: &RemoteCallPolicyConfig,
    service: RemoteCallService,
    endpoint: &str,
) -> String {
    if remote_endpoint_is_blocked_by_policy(policy, service, endpoint) {
        BLOCKED_REMOTE_ENDPOINT_LABEL.to_string()
    } else {
        endpoint_url_display_label(endpoint)
    }
}

pub fn endpoint_policy_diagnostic_label(
    policy: &RemoteCallPolicyConfig,
    service: RemoteCallService,
    endpoint: &str,
) -> String {
    if remote_endpoint_is_blocked_by_policy(policy, service, endpoint) {
        BLOCKED_REMOTE_ENDPOINT_LABEL.to_string()
    } else if endpoint_is_local_or_private(endpoint) {
        endpoint.to_string()
    } else {
        endpoint_url_display_label(endpoint)
    }
}

pub fn endpoint_policy_display_summary<'a>(
    policy: &RemoteCallPolicyConfig,
    service: RemoteCallService,
    endpoints: impl Iterator<Item = &'a str>,
) -> String {
    let mut labels = endpoints
        .map(|endpoint| endpoint_policy_display_label(policy, service, endpoint))
        .collect::<Vec<_>>();
    labels.sort();
    labels.dedup();
    labels.join(", ")
}

pub fn endpoint_policy_diagnostic_summary<'a>(
    policy: &RemoteCallPolicyConfig,
    service: RemoteCallService,
    endpoints: impl Iterator<Item = &'a str>,
) -> String {
    let mut labels = endpoints
        .map(|endpoint| endpoint_policy_diagnostic_label(policy, service, endpoint))
        .collect::<Vec<_>>();
    labels.sort();
    labels.dedup();
    labels.join(", ")
}

pub fn endpoint_policy_runtime_error(
    policy: &RemoteCallPolicyConfig,
    service: RemoteCallService,
    endpoint: &str,
    last_error: Option<String>,
) -> Option<String> {
    if remote_endpoint_is_blocked_by_policy(policy, service, endpoint) {
        None
    } else {
        last_error
    }
}

pub fn endpoint_policy_global_runtime_error<'a>(
    policy: &RemoteCallPolicyConfig,
    service: RemoteCallService,
    mut endpoints: impl Iterator<Item = &'a str>,
    last_error: Option<String>,
) -> Option<String> {
    let error = last_error?;
    let blocked =
        endpoints.any(|endpoint| remote_endpoint_is_blocked_by_policy(policy, service, endpoint));
    (!blocked).then_some(error)
}

fn remote_endpoint_is_blocked_by_policy(
    policy: &RemoteCallPolicyConfig,
    service: RemoteCallService,
    endpoint: &str,
) -> bool {
    let status = if endpoint_is_local_or_private(endpoint) {
        RemoteCallStatus::LocalEndpoint
    } else {
        RemoteCallStatus::RemoteEndpoint
    };
    policy_effect(policy, service, status) == RemoteCallPolicyEffect::BlockedByPolicy
}

fn ensure_status_allowed(status: &RemoteCallServiceReport) -> Result<(), RemoteCallPolicyError> {
    if !status.blocks_remote_call() {
        return Ok(());
    }
    Err(RemoteCallPolicyError {
        service: status.service.as_str(),
        allow_field: status.service.allow_field(),
        endpoint: status
            .endpoint
            .clone()
            .unwrap_or_else(|| "<remote-endpoint>".to_string()),
    })
}

fn embedding_status(config: &Config) -> RemoteCallServiceReport {
    let policy = &config.privacy.remote_calls;
    if !embedding_backend_is_http(&config.embed.backend) {
        let status = if config.embed.backend == "model2vec" && config.embed.model.is_none() {
            RemoteCallStatus::DefaultLocal
        } else {
            RemoteCallStatus::LocalModel
        };
        return service_report(
            RemoteCallService::Embedding,
            status,
            policy_effect(policy, RemoteCallService::Embedding, status),
            None,
            config.embed.backend.clone(),
        );
    }

    let endpoints = match config.embed.effective_endpoints() {
        Ok(endpoints) if !endpoints.is_empty() => endpoints,
        Ok(_) => {
            return service_report(
                RemoteCallService::Embedding,
                RemoteCallStatus::Misconfigured,
                RemoteCallPolicyEffect::NotApplicable,
                None,
                "openai_compat endpoint is missing".to_string(),
            );
        }
        Err(error) => {
            return service_report(
                RemoteCallService::Embedding,
                RemoteCallStatus::Misconfigured,
                RemoteCallPolicyEffect::NotApplicable,
                None,
                error.to_string(),
            );
        }
    };

    endpoint_service_report(
        policy,
        RemoteCallService::Embedding,
        endpoints.iter().map(|endpoint| endpoint.base_url.as_str()),
        "openai_compat",
    )
}

fn llm_status(config: &Config) -> RemoteCallServiceReport {
    let policy = &config.privacy.remote_calls;
    let mut reports = Vec::new();
    if config.llm.enabled && !config.llm.enabled_for.is_empty() {
        reports.push(llm_config_status(policy, &config.llm));
    }
    if config.memory_intelligence.mode.uses_llm()
        && config
            .memory_intelligence
            .has_effective_llm_endpoint(&config.llm)
    {
        let effective = config.memory_intelligence.effective_llm_config(&config.llm);
        reports.push(llm_config_status(policy, &effective));
    }
    combine_llm_reports(policy, reports)
}

fn llm_config_status(
    policy: &RemoteCallPolicyConfig,
    config: &LlmConfig,
) -> RemoteCallServiceReport {
    if !config.enabled {
        return service_report(
            RemoteCallService::Llm,
            RemoteCallStatus::Disabled,
            RemoteCallPolicyEffect::NotApplicable,
            None,
            "llm.enabled=false".to_string(),
        );
    }

    let endpoints = match config.effective_endpoints() {
        Ok(endpoints) if !endpoints.is_empty() => endpoints,
        Ok(_) => {
            return service_report(
                RemoteCallService::Llm,
                RemoteCallStatus::Misconfigured,
                RemoteCallPolicyEffect::NotApplicable,
                None,
                "LLM endpoint is missing".to_string(),
            );
        }
        Err(error) => {
            return service_report(
                RemoteCallService::Llm,
                RemoteCallStatus::Misconfigured,
                RemoteCallPolicyEffect::NotApplicable,
                None,
                error.to_string(),
            );
        }
    };

    endpoint_service_report(
        policy,
        RemoteCallService::Llm,
        endpoints.iter().map(|endpoint| endpoint.base_url.as_str()),
        "openai_compat",
    )
}

fn combine_llm_reports(
    policy: &RemoteCallPolicyConfig,
    reports: Vec<RemoteCallServiceReport>,
) -> RemoteCallServiceReport {
    if reports.is_empty() {
        return service_report(
            RemoteCallService::Llm,
            RemoteCallStatus::Disabled,
            RemoteCallPolicyEffect::NotApplicable,
            None,
            "no enabled LLM feature".to_string(),
        );
    }
    if let Some(report) = reports
        .iter()
        .find(|report| report.status == RemoteCallStatus::RemoteEndpoint)
    {
        return report.clone();
    }
    if let Some(report) = reports
        .iter()
        .find(|report| report.status == RemoteCallStatus::Misconfigured)
    {
        return report.clone();
    }
    if let Some(report) = reports
        .iter()
        .find(|report| report.status == RemoteCallStatus::LocalEndpoint)
    {
        return report.clone();
    }
    service_report(
        RemoteCallService::Llm,
        RemoteCallStatus::Disabled,
        policy_effect(policy, RemoteCallService::Llm, RemoteCallStatus::Disabled),
        None,
        "no enabled LLM feature".to_string(),
    )
}

fn rerank_status(
    policy: &RemoteCallPolicyConfig,
    reranker: &SearchRerankerConfig,
) -> RemoteCallServiceReport {
    if !reranker.enabled {
        return service_report(
            RemoteCallService::Rerank,
            RemoteCallStatus::Disabled,
            RemoteCallPolicyEffect::NotApplicable,
            None,
            "search.reranker.enabled=false".to_string(),
        );
    }
    let endpoint = match reranker
        .endpoint
        .as_deref()
        .ok_or_else(|| "search.reranker.endpoint is missing".to_string())
        .and_then(normalize_reranker_endpoint_url)
    {
        Ok(endpoint) => endpoint,
        Err(error) => {
            return service_report(
                RemoteCallService::Rerank,
                RemoteCallStatus::Misconfigured,
                RemoteCallPolicyEffect::NotApplicable,
                None,
                error,
            );
        }
    };
    endpoint_service_report(
        policy,
        RemoteCallService::Rerank,
        std::iter::once(endpoint.as_str()),
        "http",
    )
}

fn endpoint_service_report<'a>(
    policy: &RemoteCallPolicyConfig,
    service: RemoteCallService,
    endpoints: impl Iterator<Item = &'a str>,
    detail: &str,
) -> RemoteCallServiceReport {
    let endpoints = endpoints.collect::<Vec<_>>();
    let status = if endpoints
        .iter()
        .any(|endpoint| !endpoint_is_local_or_private(endpoint))
    {
        RemoteCallStatus::RemoteEndpoint
    } else {
        RemoteCallStatus::LocalEndpoint
    };
    service_report(
        service,
        status,
        policy_effect(policy, service, status),
        Some(endpoint_policy_display_summary(
            policy,
            service,
            endpoints.iter().copied(),
        )),
        detail.to_string(),
    )
}

fn service_report(
    service: RemoteCallService,
    status: RemoteCallStatus,
    policy: RemoteCallPolicyEffect,
    endpoint: Option<String>,
    detail: String,
) -> RemoteCallServiceReport {
    RemoteCallServiceReport {
        service,
        status,
        policy,
        endpoint,
        detail,
    }
}

fn policy_effect(
    policy: &RemoteCallPolicyConfig,
    service: RemoteCallService,
    status: RemoteCallStatus,
) -> RemoteCallPolicyEffect {
    if status != RemoteCallStatus::RemoteEndpoint {
        return RemoteCallPolicyEffect::NotApplicable;
    }
    if !policy.fail_closed || service_allowed(policy, service) {
        RemoteCallPolicyEffect::Allowed
    } else {
        RemoteCallPolicyEffect::BlockedByPolicy
    }
}

fn service_allowed(policy: &RemoteCallPolicyConfig, service: RemoteCallService) -> bool {
    match service {
        RemoteCallService::Embedding => policy.allow_embedding,
        RemoteCallService::Llm => policy.allow_llm,
        RemoteCallService::Rerank => policy.allow_rerank,
    }
}

fn embedding_backend_is_http(backend: &str) -> bool {
    matches!(backend, "openai_compat" | "api")
}

fn endpoint_is_local_or_private(endpoint: &str) -> bool {
    let Ok(parsed) = Url::parse(endpoint) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost")
        || !host.contains('.')
        || host.to_ascii_lowercase().ends_with(".local")
    {
        return true;
    }
    if let Ok(addr) = host.parse::<Ipv4Addr>() {
        let octets = addr.octets();
        return octets[0] == 10
            || octets[0] == 127
            || (octets[0] == 192 && octets[1] == 168)
            || (octets[0] == 172 && (16..=31).contains(&octets[1]));
    }
    host.parse::<Ipv6Addr>()
        .is_ok_and(|addr| addr.is_loopback() || addr.is_unique_local())
}

impl From<&RemoteCallPolicyConfig> for RemoteCallPolicyReport {
    fn from(policy: &RemoteCallPolicyConfig) -> Self {
        Self {
            fail_closed: policy.fail_closed,
            allow_embedding: policy.allow_embedding,
            allow_llm: policy.allow_llm,
            allow_rerank: policy.allow_rerank,
        }
    }
}

impl From<&EffectiveEmbedEndpoint> for RemoteCallServiceReport {
    fn from(endpoint: &EffectiveEmbedEndpoint) -> Self {
        let status = if endpoint_is_local_or_private(&endpoint.base_url) {
            RemoteCallStatus::LocalEndpoint
        } else {
            RemoteCallStatus::RemoteEndpoint
        };
        service_report(
            RemoteCallService::Embedding,
            status,
            RemoteCallPolicyEffect::NotApplicable,
            Some(endpoint_url_display_label(&endpoint.base_url)),
            endpoint.backend.clone(),
        )
    }
}

impl From<&EffectiveLlmEndpoint> for RemoteCallServiceReport {
    fn from(endpoint: &EffectiveLlmEndpoint) -> Self {
        let status = if endpoint_is_local_or_private(&endpoint.base_url) {
            RemoteCallStatus::LocalEndpoint
        } else {
            RemoteCallStatus::RemoteEndpoint
        };
        service_report(
            RemoteCallService::Llm,
            status,
            RemoteCallPolicyEffect::NotApplicable,
            Some(endpoint_url_display_label(&endpoint.base_url)),
            "openai_compat".to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_local_config() -> Config {
        let mut config = Config::default();
        config.embed.backend = "model2vec".to_string();
        config
    }

    fn remote_config() -> Config {
        let mut config = Config::default();
        config.embed.backend = "openai_compat".to_string();
        config.embed.base_url = Some("https://api.openai.com/v1/private-embed-path".to_string());
        config.embed.api_model = Some("text-embedding-3-large".to_string());
        config.embed.openai_compat.api_key_env = Some("MEMPAL_SECRET_TOKEN_ENV".to_string());
        config.llm.enabled = true;
        config.llm.base_url = Some("https://llm.example.com/v1/private-chat-path".to_string());
        config.llm.model = Some("secret-model".to_string());
        config.llm.api_key = Some("sk-secret-should-not-print".to_string());
        config.llm.enabled_for = vec!["gating".to_string()];
        config.search.reranker.enabled = true;
        config.search.reranker.endpoint =
            Some("https://rerank.example.com/private-rerank-path".to_string());
        config.search.reranker.model = Some("rerank-model".to_string());
        config
    }

    #[test]
    fn default_config_reports_local_embedding_and_disabled_callers() {
        let config = default_local_config();

        let report = build_remote_call_report(&config);

        assert_eq!(
            report.services[0].status,
            RemoteCallStatus::DefaultLocal,
            "{report:#?}"
        );
        assert_eq!(report.services[1].status, RemoteCallStatus::Disabled);
        assert_eq!(report.services[2].status, RemoteCallStatus::Disabled);
        assert!(ensure_embedding_allowed(&config).is_ok());
        assert!(
            ensure_rerank_allowed(&config.privacy.remote_calls, &config.search.reranker).is_ok()
        );
    }

    #[test]
    fn endpoint_report_redacts_paths_and_secrets() {
        let config = remote_config();

        let rendered =
            serde_json::to_string(&build_remote_call_report(&config)).expect("serialize report");

        assert!(rendered.contains("https://api.openai.com"));
        assert!(rendered.contains("https://llm.example.com"));
        assert!(rendered.contains("https://rerank.example.com"));
        assert!(!rendered.contains("private-embed-path"));
        assert!(!rendered.contains("private-chat-path"));
        assert!(!rendered.contains("private-rerank-path"));
        assert!(!rendered.contains("sk-secret-should-not-print"));
        assert!(!rendered.contains("MEMPAL_SECRET_TOKEN_ENV"));
    }

    #[test]
    fn fail_closed_policy_blocks_remote_but_allows_local_defaults() {
        let mut remote = remote_config();
        remote.privacy.remote_calls.fail_closed = true;

        assert!(ensure_embedding_allowed(&remote).is_err());
        assert!(ensure_llm_allowed(&remote, &remote.llm).is_err());
        assert!(
            ensure_rerank_allowed(&remote.privacy.remote_calls, &remote.search.reranker).is_err()
        );

        let mut local = default_local_config();
        local.privacy.remote_calls.fail_closed = true;
        local.search.reranker.enabled = true;
        local.search.reranker.endpoint = Some("127.0.0.1:18003".to_string());
        local.search.reranker.model = Some("rerank".to_string());

        assert!(ensure_embedding_allowed(&local).is_ok());
        assert!(ensure_llm_allowed(&local, &local.llm).is_ok());
        assert!(ensure_rerank_allowed(&local.privacy.remote_calls, &local.search.reranker).is_ok());
    }

    #[test]
    fn explicit_allow_unblocks_remote_services() {
        let mut config = remote_config();
        config.privacy.remote_calls.fail_closed = true;
        config.privacy.remote_calls.allow_embedding = true;
        config.privacy.remote_calls.allow_llm = true;
        config.privacy.remote_calls.allow_rerank = true;

        assert!(ensure_embedding_allowed(&config).is_ok());
        assert!(ensure_llm_allowed(&config, &config.llm).is_ok());
        assert!(
            ensure_rerank_allowed(&config.privacy.remote_calls, &config.search.reranker).is_ok()
        );
    }
}
