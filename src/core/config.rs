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
const DEFAULT_CHUNKER_MAX_TOKENS: usize = 1024;
const DEFAULT_CHUNKER_TARGET_TOKENS: usize = 512;
const DEFAULT_CHUNKER_OVERLAP_TOKENS: usize = 64;
const DEFAULT_HOT_RELOAD_DEBOUNCE_MS: u64 = 250;
const DEFAULT_HOT_RELOAD_POLL_FALLBACK_SECS: u64 = 5;
const DEFAULT_OPENAI_TIMEOUT_SECS: u64 = 30;
const DEFAULT_OPENAI_DIM: usize = 4096;
const DEFAULT_RETRY_INTERVAL_SECS: u64 = 2;
const DEFAULT_LLM_BACKEND: &str = "openai_compat";
const DEFAULT_LLM_REQUEST_TIMEOUT_SECS: u64 = 30;
const DEFAULT_LLM_RETRY_INTERVAL_SECS: u64 = 2;
const DEFAULT_LLM_MAX_CONCURRENT: usize = 16;
const DEFAULT_SEARCH_DEADLINE_SECS: u64 = 5;
const DEFAULT_SEARCH_PREVIEW_CHARS: usize = 120;
const DEFAULT_SEARCH_TUNNEL_FANOUT_CAP: usize = 5;
const DEFAULT_SEARCH_TUNNEL_HINTS_DISPLAY_CAP: usize = 8;
const DEFAULT_SEARCH_TUNNEL_PENALTY: f32 = 0.7;
const DEFAULT_ALERT_EVERY_N_FAILURES: u64 = 100;
const DEFAULT_DEGRADE_AFTER_N_FAILURES: u64 = 10;
const DEFAULT_HOOK_WING: &str = "agent-diary";
const DEFAULT_HOOK_POLL_INTERVAL_MS: u64 = 500;
const DEFAULT_HOOK_CLAIM_TTL_SECS: u64 = 120;
const DEFAULT_DAEMON_LOG_PATH: &str = "~/.mempal/daemon.log";
const DEFAULT_MCP_LOG_PATH: &str = "~/.mempal/mcp.log";
const DEFAULT_SESSION_REVIEW_WING: &str = "session-reviews";
const DEFAULT_SESSION_REVIEW_MIN_LENGTH: usize = 100;
const DEFAULT_SESSION_REVIEW_TRAILING_MESSAGES: usize = 1;
const DEFAULT_HOTPATCH_MIN_IMPORTANCE_STARS: i32 = 4;
const DEFAULT_HOTPATCH_MAX_SUGGESTION_LENGTH: usize = 80;
const DEFAULT_IMPORTANCE_DECAY_RATE: f64 = 0.01;
const DEFAULT_IMPORTANCE_FLOOR: f64 = 0.1;
const DEFAULT_IMPORTANCE_BOOST_PER_ACCESS: f64 = 0.15;
const DEFAULT_IMPORTANCE_BOOST_CAP: f64 = 2.0;
const DEFAULT_IMPORTANCE_STALE_PENALTY: f64 = 0.5;
const DEFAULT_PATTERNS_SIMILARITY_THRESHOLD: f64 = 0.82;
const DEFAULT_PATTERNS_MIN_SESSIONS: usize = 3;
const DEFAULT_PATTERNS_MIN_EXEMPLARS: usize = 3;
const DEFAULT_PATTERNS_PROMOTE_THRESHOLD: usize = 5;
const DEFAULT_PATTERNS_RETIRE_AFTER_DAYS: u64 = 90;
const DEFAULT_PATTERNS_SURFACING_THRESHOLD: f64 = 0.75;
const DEFAULT_PATTERNS_BOOST: f64 = 0.2;
const DEFAULT_CONTEXT_TOTAL_TOKENS: usize = 8_000;
const DEFAULT_CONTEXT_T1_RATIO: f64 = 0.30;
const DEFAULT_CONTEXT_T2_RATIO: f64 = 0.50;
const DEFAULT_CONTEXT_T3_RATIO: f64 = 0.20;
const DEFAULT_CONTEXT_MIN_T1_IMPORTANCE: u8 = 3;
const DEFAULT_CONTEXT_T3_RECENCY_WINDOW_DAYS: u64 = 3;
const DEFAULT_CONTEXT_T1_RECENCY_LAMBDA: f64 = 0.01;
const DEFAULT_REPAIR_WINDOW_DAYS: u64 = 7;
const DEFAULT_REPAIR_MIN_FAILURES: usize = 3;
const DEFAULT_REPAIR_ALERT_THRESHOLD: usize = 3;
const DEFAULT_SKILLS_ACTIVE_THRESHOLD: i64 = 3;
const DEFAULT_SKILLS_RETIRE_THRESHOLD: i64 = 3;
const DEFAULT_SKILLS_MIN_SESSIONS: usize = 5;
const DEFAULT_SKILLS_SURFACING_THRESHOLD: f64 = 0.70;
static DEFAULT_SENSITIVE_SCRUBBER: OnceLock<Option<CompiledPrivacyConfig>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub db_path: String,
    #[serde(alias = "embedder")]
    pub embed: EmbedConfig,
    pub llm: LlmConfig,
    pub chunker: ChunkerConfig,
    pub project: ProjectConfig,
    pub privacy: PrivacyConfig,
    pub config_hot_reload: ConfigHotReloadConfig,
    pub search: SearchConfig,
    pub hotpatch: HotpatchConfig,
    #[serde(alias = "gating")]
    pub ingest_gating: IngestGatingConfig,
    pub hooks: HooksConfig,
    pub daemon: DaemonConfig,
    pub api: ApiConfig,
    pub mcp: McpConfig,
    pub importance: ImportanceConfig,
    pub patterns: PatternsConfig,
    pub context: ContextConfig,
    pub repair: RepairConfig,
    pub skills: SkillsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db_path: DEFAULT_DB_PATH.to_string(),
            embed: EmbedConfig::default(),
            llm: LlmConfig::default(),
            chunker: ChunkerConfig::default(),
            project: ProjectConfig::default(),
            privacy: PrivacyConfig::default(),
            config_hot_reload: ConfigHotReloadConfig::default(),
            search: SearchConfig::default(),
            hotpatch: HotpatchConfig::default(),
            ingest_gating: IngestGatingConfig::default(),
            hooks: HooksConfig::default(),
            daemon: DaemonConfig::default(),
            api: ApiConfig::default(),
            mcp: McpConfig::default(),
            importance: ImportanceConfig::default(),
            patterns: PatternsConfig::default(),
            context: ContextConfig::default(),
            repair: RepairConfig::default(),
            skills: SkillsConfig::default(),
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
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(project_id) = self.project.id.as_deref() {
            super::project::validate_project_id(project_id)
                .map_err(|error| ConfigError::InvalidConfig(error.to_string()))?;
        }
        if self.chunker.max_tokens == 0 {
            return Err(ConfigError::InvalidConfig(
                "chunker.max_tokens must be greater than 0".to_string(),
            ));
        }
        if self.chunker.target_tokens == 0 || self.chunker.target_tokens > self.chunker.max_tokens {
            return Err(ConfigError::InvalidConfig(format!(
                "chunker.target_tokens must be in 1..={}",
                self.chunker.max_tokens
            )));
        }
        if self.chunker.overlap_tokens >= self.chunker.target_tokens {
            return Err(ConfigError::InvalidConfig(format!(
                "chunker.overlap_tokens ({}) must be less than target_tokens ({})",
                self.chunker.overlap_tokens, self.chunker.target_tokens
            )));
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
        if self.llm.enabled
            && self
                .llm
                .base_url
                .as_deref()
                .is_none_or(|base_url| base_url.trim().is_empty())
        {
            return Err(ConfigError::Validation(
                "llm.base_url must be set when llm.enabled is true".to_string(),
            ));
        }
        if self.search.preview_chars == 0 {
            return Err(ConfigError::InvalidConfig(
                "search.preview_chars must be greater than 0".to_string(),
            ));
        }
        if !self.search.tunnel_penalty.is_finite()
            || !(0.0..=1.0).contains(&self.search.tunnel_penalty)
        {
            return Err(ConfigError::InvalidConfig(
                "search.tunnel_penalty must be a finite value in 0.0..=1.0".to_string(),
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
        if self.llm.enabled != other.llm.enabled {
            fields.push("llm.enabled");
        }
        if self.llm.base_url != other.llm.base_url {
            fields.push("llm.base_url");
        }
        // llm.model is just a request parameter — safe to hot-reload

        if self.llm.api_key != other.llm.api_key {
            fields.push("llm.api_key");
        }
        if self.llm.api_key_env != other.llm.api_key_env {
            fields.push("llm.api_key_env");
        }
        if self.daemon.log_path != other.daemon.log_path {
            fields.push("daemon.log_path");
        }
        if self.mcp.log_path != other.mcp.log_path {
            fields.push("mcp.log_path");
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
        effective.llm.base_url = self.llm.base_url.clone();
        // llm.model is hot-reloadable — don't pin it
        effective.llm.api_key = self.llm.api_key.clone();
        effective.llm.api_key_env = self.llm.api_key_env.clone();
        effective.daemon.log_path = self.daemon.log_path.clone();
        effective.mcp.log_path = self.mcp.log_path.clone();
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
pub struct ApiConfig {
    /// Start the REST API server automatically when the daemon starts.
    /// Requires the binary to be built with `--features rest`.
    pub enabled: bool,
    /// Address to bind the REST API server on.
    pub addr: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            addr: "127.0.0.1:3080".to_string(),
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
pub struct McpConfig {
    pub log_path: String,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            log_path: DEFAULT_MCP_LOG_PATH.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ChunkerConfig {
    /// Hard upper bound on tokens per chunk. The chunker guarantees every
    /// emitted chunk is ≤ this value.
    pub max_tokens: usize,
    /// Soft target for split points — chunks aim for this size when natural
    /// break points (sentence, word) allow.
    pub target_tokens: usize,
    /// Overlap between adjacent chunks in tokens. Must be < target_tokens.
    pub overlap_tokens: usize,
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        Self {
            max_tokens: DEFAULT_CHUNKER_MAX_TOKENS,
            target_tokens: DEFAULT_CHUNKER_TARGET_TOKENS,
            overlap_tokens: DEFAULT_CHUNKER_OVERLAP_TOKENS,
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
pub struct LlmConfig {
    pub enabled: bool,
    pub backend: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub request_timeout_secs: u64,
    pub retry_interval_secs: u64,
    pub max_concurrent: usize,
    pub enabled_for: Vec<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: DEFAULT_LLM_BACKEND.to_string(),
            base_url: None,
            model: None,
            api_key: None,
            api_key_env: None,
            request_timeout_secs: DEFAULT_LLM_REQUEST_TIMEOUT_SECS,
            retry_interval_secs: DEFAULT_LLM_RETRY_INTERVAL_SECS,
            max_concurrent: DEFAULT_LLM_MAX_CONCURRENT,
            enabled_for: vec!["gating".to_string()],
        }
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
    /// Maximum input tokens the model accepts. When set, the chunker uses
    /// this as a hard ceiling. When `None`, the chunker relies on
    /// `[chunker].max_tokens` alone.
    pub max_input_tokens: Option<usize>,
}

impl Default for OpenAiCompatConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            model: None,
            api_key_env: None,
            request_timeout_secs: DEFAULT_OPENAI_TIMEOUT_SECS,
            dim: Some(DEFAULT_OPENAI_DIM),
            max_input_tokens: None,
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

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct SearchConfig {
    pub strict_project_isolation: bool,
    pub progressive_disclosure: bool,
    pub preview_chars: usize,
    pub tunnel_fanout_cap: usize,
    /// Maximum wing names shown per result in `tunnel_hints`. Excess entries are
    /// replaced by a single `"… +N more"` sentinel. Default: 8.
    pub tunnel_hints_display_cap: usize,
    /// Multiplier applied to a tunnel-resolved (`tunnel_cross_project`) result's
    /// similarity AND `effective_importance` before final ranking. Range `0.0..=1.0`;
    /// `1.0` disables the penalty. Default `0.7` deprioritizes cross-project rows
    /// when their raw embedding score clusters near direct in-project matches.
    pub tunnel_penalty: f32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            strict_project_isolation: false,
            progressive_disclosure: true,
            preview_chars: DEFAULT_SEARCH_PREVIEW_CHARS,
            tunnel_fanout_cap: DEFAULT_SEARCH_TUNNEL_FANOUT_CAP,
            tunnel_hints_display_cap: DEFAULT_SEARCH_TUNNEL_HINTS_DISPLAY_CAP,
            tunnel_penalty: DEFAULT_SEARCH_TUNNEL_PENALTY,
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

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct IngestGatingConfig {
    pub enabled: bool,
    #[serde(default = "default_tier1_skip_events")]
    pub tier1_skip_events: Vec<String>,
    pub rules: Vec<GatingRuleConfig>,
    pub embedding_classifier: EmbeddingClassifierConfig,
    pub fact_check: AutoFactCheckConfig,
    pub novelty: NoveltyConfig,
    pub llm_judge: Option<LlmJudgeConfig>,
}

impl Default for IngestGatingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tier1_skip_events: default_tier1_skip_events(),
            rules: Vec::new(),
            embedding_classifier: EmbeddingClassifierConfig::default(),
            fact_check: AutoFactCheckConfig::default(),
            novelty: NoveltyConfig::default(),
            llm_judge: None,
        }
    }
}

fn default_false() -> bool {
    false
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct AutoFactCheckConfig {
    #[serde(default = "default_false")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub reject_on_contradiction: bool,
    #[serde(default = "default_false")]
    pub reject_on_stale: bool,
    #[serde(default = "default_false")]
    pub reject_on_similar_name: bool,
}

impl Default for AutoFactCheckConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            reject_on_contradiction: true,
            reject_on_stale: false,
            reject_on_similar_name: false,
        }
    }
}

fn default_tier1_skip_events() -> Vec<String> {
    [
        "bash_tool",
        "Bash",
        "edit_tool",
        "Edit",
        "apply_patch",
        "ApplyPatch",
    ]
    .into_iter()
    .map(ToOwned::to_owned)
    .collect()
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct LlmJudgeConfig {
    pub enabled: bool,
    pub system_prompt: Option<String>,
    pub threshold: f64,
}

impl Default for LlmJudgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            system_prompt: None,
            threshold: 0.3,
        }
    }
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
    /// Skip when `content` starts with this literal prefix (case-sensitive).
    /// Useful for filtering raw JSON event lines like `{"type":"turn.completed",...}`.
    pub content_starts_with: Option<String>,
    /// Skip when `content` contains this literal substring (case-sensitive).
    /// More flexible than `content_starts_with`; use for patterns that may appear mid-content.
    pub content_contains: Option<String>,
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
            threshold: 0.55,
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
    Validation(String),
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

    /// Subscribe to LLM config generation changes.
    ///
    /// The counter increments whenever a hot-reloadable LLM field (model,
    /// retry_interval_secs, enabled_for, max_concurrent) changes via hot-reload.
    /// LLM workers subscribe here to cancel in-flight requests on config change.
    pub fn subscribe_llm_gen() -> tokio::sync::watch::Receiver<u64> {
        super::hot_reload::global_hot_reload_state().subscribe_llm_gen()
    }

    #[doc(hidden)]
    pub fn harness_reload_counter() -> Arc<AtomicUsize> {
        super::hot_reload::global_hot_reload_state().reload_counter_arc()
    }

    /// Force a config reload from the given path, bypassing the file watcher.
    /// Intended only for tests that need to trigger `reload_from_disk` directly.
    #[doc(hidden)]
    pub fn harness_reload_from_path(path: &std::path::Path) {
        super::hot_reload::global_hot_reload_state().reload_from_disk_for_test(path);
    }
}

fn global_scrub_stats() -> &'static Mutex<ScrubStats> {
    static SCRUB_STATS: OnceLock<Mutex<ScrubStats>> = OnceLock::new();
    SCRUB_STATS.get_or_init(|| Mutex::new(ScrubStats::default()))
}

/// Parameters for time-decay and retrieval-boost of `effective_importance`.
/// All fields are hot-reload whitelisted.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ImportanceConfig {
    /// Exponential decay rate per day. Half-life ≈ ln(2)/decay_rate days.
    /// Default 0.01 → half-life ~69 days.
    pub decay_rate: f64,
    /// Minimum value `effective_importance` can decay to (prevents full suppression).
    pub floor: f64,
    /// How much `accumulated_boost` increases per session-ingest boost event.
    pub boost_per_access: f64,
    /// Upper cap on `accumulated_boost` contribution (prevents runaway inflation).
    pub boost_cap: f64,
    /// Multiplicative penalty applied to `effective_importance` when a StaleFact is found.
    pub stale_penalty: f64,
}

impl Default for ImportanceConfig {
    fn default() -> Self {
        Self {
            decay_rate: DEFAULT_IMPORTANCE_DECAY_RATE,
            floor: DEFAULT_IMPORTANCE_FLOOR,
            boost_per_access: DEFAULT_IMPORTANCE_BOOST_PER_ACCESS,
            boost_cap: DEFAULT_IMPORTANCE_BOOST_CAP,
            stale_penalty: DEFAULT_IMPORTANCE_STALE_PENALTY,
        }
    }
}

/// Configuration for cross-session pattern induction (P13).
/// All fields are hot-reload whitelisted.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct PatternsConfig {
    /// Enable pattern detection during ingest.
    pub enabled: bool,
    /// Cosine similarity threshold for pattern candidate detection.
    pub similarity_threshold: f64,
    /// Minimum number of distinct sessions required to create a candidate.
    pub min_sessions: usize,
    /// Minimum number of exemplar drawers required to create a candidate.
    pub min_exemplars: usize,
    /// session_count threshold to promote a candidate to active.
    pub promote_threshold: usize,
    /// Days without new exemplars before auto-retiring an active pattern.
    pub retire_after_days: u64,
    /// Cosine similarity threshold for surfacing patterns in search.
    pub surfacing_threshold: f64,
    /// Score boost applied to exemplar drawers matching an active pattern.
    pub pattern_boost: f64,
}

impl Default for PatternsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            similarity_threshold: DEFAULT_PATTERNS_SIMILARITY_THRESHOLD,
            min_sessions: DEFAULT_PATTERNS_MIN_SESSIONS,
            min_exemplars: DEFAULT_PATTERNS_MIN_EXEMPLARS,
            promote_threshold: DEFAULT_PATTERNS_PROMOTE_THRESHOLD,
            retire_after_days: DEFAULT_PATTERNS_RETIRE_AFTER_DAYS,
            surfacing_threshold: DEFAULT_PATTERNS_SURFACING_THRESHOLD,
            pattern_boost: DEFAULT_PATTERNS_BOOST,
        }
    }
}

/// Token budget allocation for `mempal_context` tiered assembly.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ContextBudgetConfig {
    /// Total token budget for the assembled context.
    pub total_tokens: usize,
    /// Fraction of budget allocated to T1 (dao_tian decisions/rules).
    pub t1_ratio: f64,
    /// Fraction of budget allocated to T2 (shu evidence via hybrid search).
    pub t2_ratio: f64,
    /// Fraction of budget allocated to T3 (qi recent/operational).
    pub t3_ratio: f64,
    /// When true, unused budget from T1/T3 is transferred to T2.
    pub overflow_to_t2: bool,
}

impl Default for ContextBudgetConfig {
    fn default() -> Self {
        Self {
            total_tokens: DEFAULT_CONTEXT_TOTAL_TOKENS,
            t1_ratio: DEFAULT_CONTEXT_T1_RATIO,
            t2_ratio: DEFAULT_CONTEXT_T2_RATIO,
            t3_ratio: DEFAULT_CONTEXT_T3_RATIO,
            overflow_to_t2: true,
        }
    }
}

/// Configuration for `mempal_context` tiered retrieval assembly (P14).
/// All fields are hot-reload whitelisted.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ContextConfig {
    /// Enable tiered retrieval (T1/T2/T3). When false, falls back to flat assembly.
    pub tiered_retrieval_enabled: bool,
    /// Minimum importance for T1 candidate drawers.
    pub min_t1_importance: u8,
    /// Window in days for T3 recency candidates.
    pub t3_recency_window_days: u64,
    /// Decay rate λ for T1 recency scoring: score = eff_importance × exp(-λ × days).
    pub t1_recency_lambda: f64,
    /// Token budget allocation per tier.
    pub budget: ContextBudgetConfig,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            // Off by default so existing agent sessions and tests are unaffected;
            // users opt-in via `[context] tiered_retrieval_enabled = true`.
            tiered_retrieval_enabled: false,
            min_t1_importance: DEFAULT_CONTEXT_MIN_T1_IMPORTANCE,
            t3_recency_window_days: DEFAULT_CONTEXT_T3_RECENCY_WINDOW_DAYS,
            t1_recency_lambda: DEFAULT_CONTEXT_T1_RECENCY_LAMBDA,
            budget: ContextBudgetConfig::default(),
        }
    }
}

/// Configuration for P14 anti-pattern detection and repair warnings.
/// All fields are hot-reload whitelisted.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct RepairConfig {
    /// Enable failure detection and anti-pattern analysis.
    pub enabled: bool,
    /// Additional failure keywords beyond the built-in list.
    pub failure_keywords: Vec<String>,
    /// Window in days for counting failure events when detecting patterns.
    pub window_days: u64,
    /// Minimum number of failure events on the same topic_sig to create a pattern.
    pub min_failures: usize,
    /// Minimum failure count to inject repair_warnings into mempal_context.
    pub alert_threshold: usize,
}

impl Default for RepairConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            failure_keywords: vec![],
            window_days: DEFAULT_REPAIR_WINDOW_DAYS,
            min_failures: DEFAULT_REPAIR_MIN_FAILURES,
            alert_threshold: DEFAULT_REPAIR_ALERT_THRESHOLD,
        }
    }
}

/// Configuration for skill crystallization (P15).
/// All fields are hot-reload whitelisted.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Minimum adopt signals needed to promote from probationary → active.
    pub active_threshold: i64,
    /// Minimum reject signals (with zero adoptions) to auto-retire a probationary skill.
    pub retire_threshold: i64,
    /// Minimum pattern session_count required to promote a pattern to a skill.
    pub skill_min_sessions: usize,
    /// Cosine similarity threshold between query vector and pattern signature for
    /// surfacing matching active skills in T1 context.
    pub skill_surfacing_threshold: f64,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            active_threshold: DEFAULT_SKILLS_ACTIVE_THRESHOLD,
            retire_threshold: DEFAULT_SKILLS_RETIRE_THRESHOLD,
            skill_min_sessions: DEFAULT_SKILLS_MIN_SESSIONS,
            skill_surfacing_threshold: DEFAULT_SKILLS_SURFACING_THRESHOLD,
        }
    }
}
