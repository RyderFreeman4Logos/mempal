use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, OnceLock};

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
const DEFAULT_SEARCH_PREVIEW_CHARS: usize = 120;
const DEFAULT_ALERT_EVERY_N_FAILURES: u64 = 100;
const DEFAULT_DEGRADE_AFTER_N_FAILURES: u64 = 10;
const DEFAULT_HOOK_WING: &str = "agent-diary";
const DEFAULT_HOOK_POLL_INTERVAL_MS: u64 = 500;
const DEFAULT_HOOK_CLAIM_TTL_SECS: u64 = 120;
const DEFAULT_DAEMON_LOG_PATH: &str = "~/.mempal/daemon.log";
const DEFAULT_SESSION_REVIEW_WING: &str = "session-reviews";
const DEFAULT_SESSION_REVIEW_MIN_LENGTH: usize = 100;
const DEFAULT_SESSION_REVIEW_TRAILING_MESSAGES: usize = 1;
const DEFAULT_HOTPATCH_MIN_IMPORTANCE_STARS: i32 = 4;
const DEFAULT_HOTPATCH_MAX_SUGGESTION_LENGTH: usize = 80;
static DEFAULT_SENSITIVE_SCRUBBER: OnceLock<Option<CompiledPrivacyConfig>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub db_path: String,
    #[serde(alias = "embedder")]
    pub embed: EmbedConfig,
    pub project: ProjectConfig,
    pub privacy: PrivacyConfig,
    pub config_hot_reload: ConfigHotReloadConfig,
    pub search: SearchConfig,
    pub hotpatch: HotpatchConfig,
    #[serde(alias = "gating")]
    pub ingest_gating: IngestGatingConfig,
    pub hooks: HooksConfig,
    pub daemon: DaemonConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db_path: DEFAULT_DB_PATH.to_string(),
            embed: EmbedConfig::default(),
            project: ProjectConfig::default(),
            privacy: PrivacyConfig::default(),
            config_hot_reload: ConfigHotReloadConfig::default(),
            search: SearchConfig::default(),
            hotpatch: HotpatchConfig::default(),
            ingest_gating: IngestGatingConfig::default(),
            hooks: HooksConfig::default(),
            daemon: DaemonConfig::default(),
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
        let root: toml::Value = toml::from_str(contents)?;
        let mut config: Self = toml::from_str(contents)?;
        if root.get("embed").is_none() && root.get("embedder").is_none() {
            config.embed.backend = "model2vec".to_string();
        }
        if has_llm_judge_section(&root) {
            eprintln!("warning: llm_judge tier ignored: external LLM API disabled by design");
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(project_id) = self.project.id.as_deref() {
            super::project::validate_project_id(project_id)
                .map_err(|error| ConfigError::InvalidConfig(error.to_string()))?;
        }
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
        if self.search.preview_chars == 0 {
            return Err(ConfigError::InvalidConfig(
                "search.preview_chars must be greater than 0".to_string(),
            ));
        }
        if !(0..=5).contains(&self.hotpatch.min_importance_stars) {
            return Err(ConfigError::InvalidConfig(
                "hotpatch.min_importance_stars must be between 0 and 5".to_string(),
            ));
        }
        if self.hotpatch.max_suggestion_length == 0 {
            return Err(ConfigError::InvalidConfig(
                "hotpatch.max_suggestion_length must be greater than 0".to_string(),
            ));
        }
        if self.hotpatch.watch_files.is_empty() {
            return Err(ConfigError::InvalidConfig(
                "hotpatch.watch_files must not be empty".to_string(),
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
        if self.hooks.daemon_poll_interval_ms == 0 {
            return Err(ConfigError::InvalidConfig(
                "hooks.daemon_poll_interval_ms must be greater than 0".to_string(),
            ));
        }
        if self.hooks.daemon_claim_ttl_secs == 0 {
            return Err(ConfigError::InvalidConfig(
                "hooks.daemon_claim_ttl_secs must be greater than 0".to_string(),
            ));
        }
        if self.hooks.session_end.trailing_messages == 0 {
            return Err(ConfigError::InvalidConfig(
                "hooks.session_end.trailing_messages must be greater than 0".to_string(),
            ));
        }
        if let Some(path) = self
            .embed
            .alert
            .script_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            && !Path::new(path).is_absolute()
        {
            eprintln!(
                "warning: alerting script_path is not absolute: {}; CWD at invocation may differ from expectation",
                path
            );
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
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "scrub_content regex compile failed, falling back to no-op"
                );
                input.to_string()
            }
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

        let mut content = input.to_string();
        let mut stats = ScrubStats::default();

        for (name, regex) in &compiled.patterns {
            let matches = regex.find_iter(&content).collect::<Vec<_>>();
            if matches.is_empty() {
                continue;
            }

            let matched_count = matches.len() as u64;
            let bytes_redacted = matches
                .iter()
                .map(|matched| matched.as_str().len() as u64)
                .sum::<u64>();
            stats.record_match(name, matched_count, bytes_redacted);
            let replacement = if name == "private_tag" {
                String::new()
            } else {
                format!("[REDACTED:{name}]")
            };
            content = regex
                .replace_all(&content, regex::NoExpand(replacement.as_str()))
                .into_owned();
        }

        if stats.total_patterns_matched > 0 {
            global_scrub_stats()
                .lock()
                .expect("scrub stats mutex poisoned")
                .merge(&stats);
        }

        content
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
        if self.daemon.log_path != other.daemon.log_path {
            fields.push("daemon.log_path");
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

    pub fn collect_runtime_warnings(&self) -> Vec<RuntimeWarning> {
        let mut warnings = Vec::new();
        if self.hooks.enabled && !self.privacy.enabled {
            warnings.push(RuntimeWarning {
                level: "warn",
                source: "privacy",
                message: "hooks capture is enabled while privacy scrubbing is disabled; captured content may persist secrets. Set [privacy].enabled = true or disable [hooks].enabled.".to_string(),
            });
        }
        if self.hooks.enabled && !self.ingest_gating.enabled {
            warnings.push(RuntimeWarning {
                level: "warn",
                source: "gating",
                message: "hooks capture is enabled while local gating is disabled; passive captures will bypass memory filtering.".to_string(),
            });
        }
        if self.hooks.enabled && self.ingest_gating.fail_open_active() {
            warnings.push(RuntimeWarning {
                level: "warn",
                source: "gating",
                message: "hooks capture is enabled while tier-2 gating is fail-open on embedder errors; review warnings before trusting passive captures.".to_string(),
            });
        }
        warnings
    }
}

pub(crate) fn scrub_sensitive_text(input: &str) -> String {
    let compiled = DEFAULT_SENSITIVE_SCRUBBER.get_or_init(|| {
        let mut config = Config::default();
        config.privacy.enabled = true;
        config.compile_privacy().ok()
    });
    let Some(compiled) = compiled.as_ref() else {
        return input.to_string();
    };

    let mut content = input.to_string();
    for (name, regex) in &compiled.patterns {
        let replacement = format!("[REDACTED:{name}]");
        content = regex
            .replace_all(&content, regex::NoExpand(replacement.as_str()))
            .into_owned();
    }
    content
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct HooksConfig {
    pub enabled: bool,
    pub capture: Vec<String>,
    pub wing: String,
    pub daemon_poll_interval_ms: u64,
    pub daemon_claim_ttl_secs: u64,
    pub session_end: HooksSessionEndConfig,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            capture: vec![
                "PostToolUse".to_string(),
                "UserPromptSubmit".to_string(),
                "SessionStart".to_string(),
                "SessionEnd".to_string(),
            ],
            wing: DEFAULT_HOOK_WING.to_string(),
            daemon_poll_interval_ms: DEFAULT_HOOK_POLL_INTERVAL_MS,
            daemon_claim_ttl_secs: DEFAULT_HOOK_CLAIM_TTL_SECS,
            session_end: HooksSessionEndConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct HooksSessionEndConfig {
    #[serde(alias = "enabled")]
    pub extract_self_review: bool,
    pub trailing_messages: usize,
    pub min_length: usize,
    pub wing: String,
}

impl Default for HooksSessionEndConfig {
    fn default() -> Self {
        Self {
            extract_self_review: false,
            trailing_messages: DEFAULT_SESSION_REVIEW_TRAILING_MESSAGES,
            min_length: DEFAULT_SESSION_REVIEW_MIN_LENGTH,
            wing: DEFAULT_SESSION_REVIEW_WING.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub log_path: String,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            log_path: DEFAULT_DAEMON_LOG_PATH.to_string(),
        }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeWarning {
    pub level: &'static str,
    pub message: String,
    pub source: &'static str,
}

#[derive(Debug, Clone)]
pub struct CompiledPrivacyConfig {
    enabled: bool,
    patterns: Vec<(String, Regex)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ScrubStats {
    pub total_patterns_matched: u64,
    pub bytes_redacted: u64,
    pub redactions_per_pattern: std::collections::BTreeMap<String, u64>,
}

impl ScrubStats {
    fn record_match(&mut self, pattern_name: &str, matched_count: u64, bytes_redacted: u64) {
        self.total_patterns_matched += matched_count;
        self.bytes_redacted += bytes_redacted;
        *self
            .redactions_per_pattern
            .entry(pattern_name.to_string())
            .or_default() += matched_count;
    }

    fn merge(&mut self, other: &Self) {
        self.total_patterns_matched += other.total_patterns_matched;
        self.bytes_redacted += other.bytes_redacted;
        for (pattern_name, count) in &other.redactions_per_pattern {
            *self
                .redactions_per_pattern
                .entry(pattern_name.clone())
                .or_default() += count;
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct SearchConfig {
    pub strict_project_isolation: bool,
    pub progressive_disclosure: bool,
    pub preview_chars: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            strict_project_isolation: false,
            progressive_disclosure: false,
            preview_chars: DEFAULT_SEARCH_PREVIEW_CHARS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct HotpatchConfig {
    pub enabled: bool,
    pub min_importance_stars: i32,
    pub watch_files: Vec<String>,
    pub max_suggestion_length: usize,
    pub allowed_target_prefixes: Vec<String>,
}

impl Default for HotpatchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_importance_stars: DEFAULT_HOTPATCH_MIN_IMPORTANCE_STARS,
            watch_files: vec![
                "CLAUDE.md".to_string(),
                "AGENTS.md".to_string(),
                "GEMINI.md".to_string(),
            ],
            max_suggestion_length: DEFAULT_HOTPATCH_MAX_SUGGESTION_LENGTH,
            allowed_target_prefixes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct ProjectConfig {
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct IngestGatingConfig {
    pub enabled: bool,
    pub rules: Vec<GatingRuleConfig>,
    pub embedding_classifier: EmbeddingClassifierConfig,
    pub novelty: NoveltyConfig,
}

impl IngestGatingConfig {
    pub fn fail_open_active(&self) -> bool {
        self.enabled
            && self.embedding_classifier.enabled
            && !self.embedding_classifier.prototypes.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct GatingRuleConfig {
    pub action: String,
    pub tool: Option<String>,
    pub tool_in: Option<Vec<String>>,
    pub content_bytes_lt: Option<usize>,
    pub content_bytes_gt: Option<usize>,
    pub exit_code_eq: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct EmbeddingClassifierConfig {
    pub enabled: bool,
    pub threshold: f32,
    pub prototypes: Vec<String>,
}

impl Default for EmbeddingClassifierConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: 0.35,
            prototypes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct NoveltyConfig {
    pub enabled: bool,
    pub duplicate_threshold: f32,
    pub merge_threshold: f32,
    pub wing_scope: String,
    pub top_k_candidates: usize,
    pub max_merges_per_drawer: u32,
    pub max_content_bytes_per_drawer: usize,
}

impl Default for NoveltyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            duplicate_threshold: 0.95,
            merge_threshold: 0.80,
            wing_scope: "same_wing".to_string(),
            top_k_candidates: 5,
            max_merges_per_drawer: 10,
            max_content_bytes_per_drawer: 65_536,
        }
    }
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

    pub fn scrub_stats() -> ScrubStats {
        global_scrub_stats()
            .lock()
            .expect("scrub stats mutex poisoned")
            .clone()
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

    pub fn collect_runtime_warnings() -> Vec<RuntimeWarning> {
        let mut warnings = Self::current().collect_runtime_warnings();
        let mut seen = std::collections::BTreeSet::new();
        for event in Self::recent_events() {
            if event.contains("requires restart, change ignored") && seen.insert(event.clone()) {
                warnings.push(RuntimeWarning {
                    level: "warn",
                    source: "config",
                    message: event,
                });
            }
        }
        warnings
    }

    pub fn runtime_prototypes() -> Vec<String> {
        super::hot_reload::global_hot_reload_state().runtime_prototypes()
    }

    pub fn simulate_notify_failure() {
        super::hot_reload::global_hot_reload_state().simulate_notify_failure();
    }

    #[doc(hidden)]
    pub fn harness_reload_counter() -> Arc<AtomicUsize> {
        super::hot_reload::global_hot_reload_state().reload_counter_arc()
    }
}

fn global_scrub_stats() -> &'static Mutex<ScrubStats> {
    static SCRUB_STATS: OnceLock<Mutex<ScrubStats>> = OnceLock::new();
    SCRUB_STATS.get_or_init(|| Mutex::new(ScrubStats::default()))
}

fn has_llm_judge_section(root: &toml::Value) -> bool {
    ["ingest_gating", "gating"]
        .into_iter()
        .filter_map(|key| root.get(key))
        .any(|section| section.get("llm_judge").is_some())
}
