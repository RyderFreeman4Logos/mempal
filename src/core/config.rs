use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const DEFAULT_DB_PATH: &str = "~/.mempal/palace.db";
const DEFAULT_EMBED_BACKEND: &str = "openai_compat";
const DEFAULT_HOT_RELOAD_DEBOUNCE_MS: u64 = 250;
const DEFAULT_HOT_RELOAD_POLL_FALLBACK_SECS: u64 = 5;
const DEFAULT_OPENAI_TIMEOUT_SECS: u64 = 30;
const DEFAULT_OPENAI_DIM: usize = 4096;
const DEFAULT_RETRY_INTERVAL_SECS: u64 = 2;
const DEFAULT_SEARCH_DEADLINE_SECS: u64 = 5;
const DEFAULT_ALERT_EVERY_N_FAILURES: u64 = 100;
const DEFAULT_DEGRADE_AFTER_N_FAILURES: u64 = 10;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub db_path: String,
    #[serde(alias = "embedder")]
    pub embed: EmbedConfig,
    pub privacy: PrivacyConfig,
    pub config_hot_reload: ConfigHotReloadConfig,
    pub search: SearchConfig,
    pub ingest_gating: IngestGatingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db_path: DEFAULT_DB_PATH.to_string(),
            embed: EmbedConfig::default(),
            privacy: PrivacyConfig::default(),
            config_hot_reload: ConfigHotReloadConfig::default(),
            search: SearchConfig::default(),
            ingest_gating: IngestGatingConfig::default(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(&default_config_path())
    }

    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(contents) => Self::parse(&contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let config = Self::default();
                config.validate()?;
                Ok(config)
            }
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    pub fn parse(contents: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(contents)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.embed.retry.interval_secs == 0 {
            return Err(ConfigError::InvalidConfig(
                "embed.retry.interval_secs must be greater than 0".to_string(),
            ));
        }
        if self.embed.retry.search_deadline_secs == 0 {
            return Err(ConfigError::InvalidConfig(
                "embed.retry.search_deadline_secs must be greater than 0".to_string(),
            ));
        }
        if self.embed.alert.alert_every_n_failures == 0 {
            return Err(ConfigError::InvalidConfig(
                "embed.alert.alert_every_n_failures must be greater than 0".to_string(),
            ));
        }
        if self.embed.degradation.degrade_after_n_failures == 0 {
            return Err(ConfigError::InvalidConfig(
                "embed.degradation.degrade_after_n_failures must be greater than 0".to_string(),
            ));
        }
        let _ = self.compile_privacy()?;
        Ok(())
    }

    pub fn compile_privacy(&self) -> Result<CompiledPrivacyConfig, ConfigError> {
        let patterns = self
            .privacy
            .scrub_patterns
            .iter()
            .map(|pattern| {
                Regex::new(&pattern.pattern)
                    .map(|regex| (pattern.name.clone(), regex))
                    .map_err(|source| ConfigError::InvalidRegex {
                        name: pattern.name.clone(),
                        source,
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(CompiledPrivacyConfig {
            enabled: self.privacy.enabled,
            patterns,
        })
    }

    pub fn scrub_content(&self, input: &str) -> String {
        match self.compile_privacy() {
            Ok(compiled) => self.scrub_content_with_compiled(input, &compiled),
            Err(_) => input.to_string(),
        }
    }

    pub fn scrub_content_with_compiled(
        &self,
        input: &str,
        compiled: &CompiledPrivacyConfig,
    ) -> String {
        if !self.privacy.enabled || !compiled.enabled || compiled.patterns.is_empty() {
            return input.to_string();
        }

        compiled
            .patterns
            .iter()
            .fold(input.to_string(), |content, (name, regex)| {
                regex
                    .replace_all(&content, format!("[REDACTED:{name}]"))
                    .into_owned()
            })
    }

    pub fn effective_hash(&self) -> Result<String, ConfigError> {
        let bytes = toml::to_string(self)
            .map_err(|source| ConfigError::SerializeEffectiveConfig { source })?;
        Ok(blake3::hash(bytes.as_bytes()).to_hex()[..12].to_string())
    }

    pub fn restart_required_fields_changed(&self, other: &Self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if self.db_path != other.db_path {
            fields.push("database.path");
        }
        if self.embed.backend != other.embed.backend {
            fields.push("embedder.backend");
        }
        if self.embed.fallback != other.embed.fallback {
            fields.push("embedder.fallback");
        }
        if self.embed.base_url != other.embed.base_url {
            fields.push("embedder.base_url");
        }
        if self.embed.model != other.embed.model {
            fields.push("embedder.model");
        }
        if self.embed.api_model != other.embed.api_model {
            fields.push("embedder.api_model");
        }
        if self.embed.openai_compat.base_url != other.embed.openai_compat.base_url {
            fields.push("embedder.openai_compat.base_url");
        }
        if self.embed.openai_compat.model != other.embed.openai_compat.model {
            fields.push("embedder.openai_compat.model");
        }
        if self.embed.openai_compat.api_key_env != other.embed.openai_compat.api_key_env {
            fields.push("embedder.openai_compat.api_key_env");
        }
        if self.embed.openai_compat.request_timeout_secs
            != other.embed.openai_compat.request_timeout_secs
        {
            fields.push("embedder.openai_compat.request_timeout_secs");
        }
        if self.embed.openai_compat.dim != other.embed.openai_compat.dim {
            fields.push("embedder.openai_compat.dim");
        }
        fields
    }

    pub fn merge_runtime_allowed(&self, candidate: &Self) -> Self {
        let mut effective = candidate.clone();
        effective.db_path = self.db_path.clone();
        effective.embed.backend = self.embed.backend.clone();
        effective.embed.fallback = self.embed.fallback.clone();
        effective.embed.model = self.embed.model.clone();
        effective.embed.base_url = self.embed.base_url.clone();
        effective.embed.api_model = self.embed.api_model.clone();
        effective.embed.openai_compat = self.embed.openai_compat.clone();
        effective
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct EmbedConfig {
    pub backend: String,
    pub fallback: Option<String>,
    /// Model identifier (e.g., "minishlab/potion-multilingual-128M" for model2vec).
    pub model: Option<String>,
    #[serde(alias = "api_endpoint")]
    pub base_url: Option<String>,
    pub api_model: Option<String>,
    pub openai_compat: OpenAiCompatConfig,
    pub retry: RetryConfig,
    pub alert: AlertConfig,
    pub degradation: DegradationConfig,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            backend: DEFAULT_EMBED_BACKEND.to_string(),
            fallback: None,
            model: None,
            base_url: None,
            api_model: None,
            openai_compat: OpenAiCompatConfig::default(),
            retry: RetryConfig::default(),
            alert: AlertConfig::default(),
            degradation: DegradationConfig::default(),
        }
    }
}

impl EmbedConfig {
    pub fn resolved_openai_base_url(&self) -> Option<&str> {
        self.openai_compat
            .base_url
            .as_deref()
            .or(self.base_url.as_deref())
    }

    pub fn resolved_openai_model(&self) -> Option<&str> {
        self.openai_compat
            .model
            .as_deref()
            .or(self.api_model.as_deref())
    }

    pub fn resolved_api_key_env(&self) -> Option<&str> {
        self.openai_compat
            .api_key_env
            .as_deref()
            .filter(|value| !value.is_empty())
    }

    pub fn resolved_openai_dim(&self) -> usize {
        self.openai_compat.dim.unwrap_or(DEFAULT_OPENAI_DIM)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct OpenAiCompatConfig {
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
    pub request_timeout_secs: u64,
    pub dim: Option<usize>,
}

impl Default for OpenAiCompatConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            model: None,
            api_key_env: None,
            request_timeout_secs: DEFAULT_OPENAI_TIMEOUT_SECS,
            dim: Some(DEFAULT_OPENAI_DIM),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct RetryConfig {
    pub interval_secs: u64,
    pub search_deadline_secs: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            interval_secs: DEFAULT_RETRY_INTERVAL_SECS,
            search_deadline_secs: DEFAULT_SEARCH_DEADLINE_SECS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct AlertConfig {
    pub enabled: bool,
    pub script_path: Option<String>,
    pub alert_every_n_failures: u64,
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            script_path: None,
            alert_every_n_failures: DEFAULT_ALERT_EVERY_N_FAILURES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct DegradationConfig {
    pub degrade_after_n_failures: u64,
    pub block_writes_when_degraded: bool,
}

impl Default for DegradationConfig {
    fn default() -> Self {
        Self {
            degrade_after_n_failures: DEFAULT_DEGRADE_AFTER_N_FAILURES,
            block_writes_when_degraded: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct PrivacyConfig {
    pub enabled: bool,
    pub scrub_patterns: Vec<ScrubPattern>,
}

impl PrivacyConfig {
    fn default_scrub_patterns() -> Vec<ScrubPattern> {
        vec![
            ScrubPattern {
                name: "private_tag".to_string(),
                pattern: r"(?is)<private>.*?</private>".to_string(),
            },
            ScrubPattern {
                name: "openai_key".to_string(),
                pattern: r"sk-[A-Za-z0-9]{32,}".to_string(),
            },
            ScrubPattern {
                name: "anthropic_key".to_string(),
                pattern: r"sk-ant-[A-Za-z0-9_-]{64,}".to_string(),
            },
            ScrubPattern {
                name: "aws_access".to_string(),
                pattern: r"AKIA[0-9A-Z]{16}".to_string(),
            },
            ScrubPattern {
                name: "bearer_token".to_string(),
                pattern: r"Bearer\s+[A-Za-z0-9._-]{20,}".to_string(),
            },
            ScrubPattern {
                name: "hex_token".to_string(),
                pattern: r"\b[a-f0-9]{32,}\b".to_string(),
            },
        ]
    }
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            scrub_patterns: Self::default_scrub_patterns(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ScrubPattern {
    pub name: String,
    #[serde(alias = "regex")]
    pub pattern: String,
}

#[derive(Debug, Clone)]
pub struct CompiledPrivacyConfig {
    enabled: bool,
    patterns: Vec<(String, Regex)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ConfigHotReloadConfig {
    pub enabled: bool,
    pub debounce_ms: u64,
    pub poll_fallback_secs: u64,
}

impl Default for ConfigHotReloadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            debounce_ms: DEFAULT_HOT_RELOAD_DEBOUNCE_MS,
            poll_fallback_secs: DEFAULT_HOT_RELOAD_POLL_FALLBACK_SECS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct SearchConfig {
    pub strict_project_isolation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct IngestGatingConfig {
    pub embedding_classifier: EmbeddingClassifierConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct EmbeddingClassifierConfig {
    pub prototypes: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config from {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config TOML")]
    Parse(#[from] toml::de::Error),
    #[error("invalid privacy regex for pattern {name}")]
    InvalidRegex {
        name: String,
        #[source]
        source: regex::Error,
    },
    #[error("failed to serialize effective config")]
    SerializeEffectiveConfig {
        #[source]
        source: toml::ser::Error,
    },
    #[error("invalid config: {0}")]
    InvalidConfig(String),
}

pub fn default_config_path() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".mempal").join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("~/.mempal/config.toml"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSnapshotMeta {
    pub version: String,
    pub loaded_at_unix_ms: u64,
}

pub struct ConfigHandle;

impl ConfigHandle {
    pub fn bootstrap(path: impl AsRef<Path>) -> Result<(), ConfigError> {
        super::hot_reload::global_hot_reload_state().bootstrap(path.as_ref())
    }

    pub fn current() -> Arc<Config> {
        super::hot_reload::global_hot_reload_state().current()
    }

    pub fn current_compiled_privacy() -> Arc<CompiledPrivacyConfig> {
        super::hot_reload::global_hot_reload_state().current_compiled_privacy()
    }

    pub fn current_privacy_snapshot() -> (Arc<Config>, Arc<CompiledPrivacyConfig>) {
        super::hot_reload::global_hot_reload_state().current_privacy_snapshot()
    }

    pub fn scrub_content(input: &str) -> String {
        let (config, compiled) = Self::current_privacy_snapshot();
        config.scrub_content_with_compiled(input, compiled.as_ref())
    }

    pub fn snapshot_meta() -> ConfigSnapshotMeta {
        super::hot_reload::global_hot_reload_state().snapshot_meta()
    }

    pub fn version() -> String {
        Self::snapshot_meta().version
    }

    pub fn loaded_at_unix_ms() -> u64 {
        Self::snapshot_meta().loaded_at_unix_ms
    }

    pub fn parse_attempts() -> usize {
        super::hot_reload::global_hot_reload_state().parse_attempts()
    }

    pub fn recent_events() -> Vec<String> {
        super::hot_reload::global_hot_reload_state().recent_events()
    }

    pub fn runtime_prototypes() -> Vec<String> {
        super::hot_reload::global_hot_reload_state().runtime_prototypes()
    }

    pub fn simulate_notify_failure() {
        super::hot_reload::global_hot_reload_state().simulate_notify_failure();
    }
}
