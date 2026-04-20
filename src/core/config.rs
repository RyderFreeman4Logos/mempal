use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const DEFAULT_DB_PATH: &str = "~/.mempal/palace.db";
const DEFAULT_EMBED_BACKEND: &str = "model2vec";
const DEFAULT_HOT_RELOAD_DEBOUNCE_MS: u64 = 250;
const DEFAULT_HOT_RELOAD_POLL_FALLBACK_SECS: u64 = 5;

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
        if self.embed.base_url != other.embed.base_url {
            fields.push("embedder.base_url");
        }
        if self.embed.model != other.embed.model {
            fields.push("embedder.model");
        }
        if self.embed.api_model != other.embed.api_model {
            fields.push("embedder.api_model");
        }
        fields
    }

    pub fn merge_runtime_allowed(&self, candidate: &Self) -> Self {
        let mut effective = candidate.clone();
        effective.db_path = self.db_path.clone();
        effective.embed = self.embed.clone();
        effective
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct EmbedConfig {
    pub backend: String,
    /// Model identifier (e.g., "minishlab/potion-multilingual-128M" for model2vec).
    pub model: Option<String>,
    #[serde(alias = "api_endpoint")]
    pub base_url: Option<String>,
    pub api_model: Option<String>,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            backend: DEFAULT_EMBED_BACKEND.to_string(),
            model: None,
            base_url: None,
            api_model: None,
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
