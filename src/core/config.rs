use std::env;
use std::fs;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, OnceLock};

use regex::Regex;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::types::IntelligenceMode;

const DEFAULT_DB_PATH: &str = "~/.mempal/palace.db";
const DEFAULT_EMBED_BACKEND: &str = "openai_compat";
pub const DEFAULT_MODEL2VEC_FINGERPRINT_MODEL: &str = "model2vec/potion-multilingual-128M";
const DEFAULT_CHUNKER_MAX_TOKENS: usize = 1024;
const DEFAULT_CHUNKER_TARGET_TOKENS: usize = 512;
const DEFAULT_CHUNKER_OVERLAP_TOKENS: usize = 64;
const DEFAULT_HOT_RELOAD_DEBOUNCE_MS: u64 = 250;
const DEFAULT_HOT_RELOAD_POLL_FALLBACK_SECS: u64 = 5;
const DEFAULT_OPENAI_TIMEOUT_SECS: u64 = 30;
const DEFAULT_OPENAI_DIM: usize = 4096;
const DEFAULT_RETRY_INTERVAL_SECS: u64 = 2;
const DEFAULT_EMBED_MAX_CONCURRENT: usize = 16;
const DEFAULT_EMBED_ENDPOINT_PRIORITY: i32 = 0;
const DEFAULT_LLM_BACKEND: &str = "openai_compat";
const DEFAULT_LLM_REQUEST_TIMEOUT_SECS: u64 = 30;
const DEFAULT_LLM_HEALTH_PROBE_TIMEOUT_SECS: u64 = 3;
const DEFAULT_LLM_RETRY_INTERVAL_SECS: u64 = 2;
const DEFAULT_LLM_MAX_CONCURRENT: usize = 16;
const DEFAULT_LLM_ENDPOINT_PRIORITY: i32 = 0;
const DEFAULT_MEMORY_INTELLIGENCE_TIMEOUT_SECS: u64 = 1800;
const DEFAULT_SEARCH_DEADLINE_SECS: u64 = 5;
const DEFAULT_SEARCH_PREVIEW_CHARS: usize = 120;
const DEFAULT_SEARCH_TUNNEL_FANOUT_CAP: usize = 5;
const DEFAULT_SEARCH_TUNNEL_HINTS_DISPLAY_CAP: usize = 8;
const DEFAULT_SEARCH_TUNNEL_PENALTY: f32 = 0.7;
const DEFAULT_SEARCH_DECAY_HALF_LIFE_DAYS: u64 = 90;
const DEFAULT_SEARCH_DECAY_STEP_FULL_DAYS: u64 = 30;
const DEFAULT_SEARCH_DECAY_STEP_REDUCED_WEIGHT: f64 = 0.5;
const DEFAULT_SEARCH_BM25_FALLBACK: bool = true;
const DEFAULT_SEARCH_RERANKER_TIMEOUT_SECS: u64 = 2;
const DEFAULT_SEARCH_RERANKER_TOP_K: usize = 20;
const DEFAULT_TURN_STORAGE_MODE: TurnStorageMode = TurnStorageMode::RawEvidence;
const DEFAULT_TURN_IMPORTANCE: i32 = 0;
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
const DEFAULT_API_WRITE_QUEUE_CAPACITY: usize = 1_000;
const DEFAULT_API_WRITE_DRAIN_TIMEOUT_SECS: u64 = 30;
const DEFAULT_API_SEARCH_DB_DEADLINE_SECS: u64 = 30;
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
const DEFAULT_CONSOLIDATION_SIMILARITY_THRESHOLD: f64 = 0.85;
const DEFAULT_CONSOLIDATION_MIN_CLUSTER_SIZE: usize = 3;
const DEFAULT_CONSOLIDATION_MAX_CLUSTERS_PER_RUN: usize = 100;
const DEFAULT_CONSOLIDATION_STRATEGY: &str = "richest_content";
const DEFAULT_CRYSTALLIZE_MIN_CLUSTER_SIZE: usize = 5;
const DEFAULT_CRYSTALLIZE_READINESS_THRESHOLD: f64 = 5.0;
const DEFAULT_CRYSTALLIZE_MAX_CANDIDATES_PER_RUN: usize = 20;
const DEFAULT_SLEEP_NREM_PRUNE_MIN_AGE_DAYS: u64 = 30;
const DEFAULT_SLEEP_NREM_PRUNE_MAX_IMPORTANCE: i32 = 1;
const DEFAULT_SLEEP_NREM_COMPACTION_THRESHOLD: f64 = 0.85;
const DEFAULT_SLEEP_SALIENCE_IDLE_MINUTES: u64 = 30;
const DEFAULT_SLEEP_SCHEDULE: &str = "0 3 * * *";
static DEFAULT_SENSITIVE_SCRUBBER: OnceLock<Option<CompiledPrivacyConfig>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub db_path: String,
    #[serde(alias = "embedder")]
    pub embed: EmbedConfig,
    pub llm: LlmConfig,
    pub memory_intelligence: MemoryIntelligenceConfig,
    pub chunker: ChunkerConfig,
    pub project: ProjectConfig,
    pub privacy: PrivacyConfig,
    pub config_hot_reload: ConfigHotReloadConfig,
    pub search: SearchConfig,
    pub turns: TurnsConfig,
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
    pub consolidation: ConsolidationConfig,
    pub crystallize: CrystallizeConfig,
    pub sleep: SleepConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db_path: DEFAULT_DB_PATH.to_string(),
            embed: EmbedConfig::default(),
            llm: LlmConfig::default(),
            memory_intelligence: MemoryIntelligenceConfig::default(),
            chunker: ChunkerConfig::default(),
            project: ProjectConfig::default(),
            privacy: PrivacyConfig::default(),
            config_hot_reload: ConfigHotReloadConfig::default(),
            search: SearchConfig::default(),
            turns: TurnsConfig::default(),
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
            consolidation: ConsolidationConfig::default(),
            crystallize: CrystallizeConfig::default(),
            sleep: SleepConfig::default(),
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
                let mut config = Self::default();
                config.apply_env_overrides();
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
        validate_llm_endpoint_pool_toml(&root)?;
        let mut config: Self = toml::from_str(contents)?;
        if root.get("embed").is_none() && root.get("embedder").is_none() {
            config.embed.backend = "model2vec".to_string();
        }
        config.apply_env_overrides();
        config.validate()?;
        Ok(config)
    }

    /// Override embed config from environment variables.
    ///
    /// Applied after TOML parsing so env vars take priority over config file.
    /// Only applies when the variable is present and non-empty.
    ///
    /// Variables:
    /// - `MEMPAL_EMBED_BACKEND`  — sets `embed.backend`
    /// - `MEMPAL_EMBED_BASE_URL` — sets `embed.openai_compat.base_url`
    /// - `MEMPAL_EMBED_MODEL`    — sets `embed.openai_compat.model`
    /// - `MEMPAL_EMBED_DIM`      — sets `embed.openai_compat.dim` (parsed as usize; invalid values are ignored with a warning)
    fn apply_env_overrides(&mut self) {
        if let Ok(val) = env::var("MEMPAL_EMBED_BACKEND") {
            if !val.is_empty() {
                self.embed.backend = val;
            }
        }
        if let Ok(val) = env::var("MEMPAL_EMBED_BASE_URL") {
            if !val.is_empty() {
                self.embed.openai_compat.base_url = Some(val);
            }
        }
        if let Ok(val) = env::var("MEMPAL_EMBED_MODEL") {
            if !val.is_empty() {
                self.embed.openai_compat.model = Some(val);
            }
        }
        if let Ok(val) = env::var("MEMPAL_EMBED_DIM") {
            if !val.is_empty() {
                match val.parse::<usize>() {
                    Ok(dim) => self.embed.openai_compat.dim = Some(dim),
                    Err(_) => eprintln!(
                        "warning: MEMPAL_EMBED_DIM={val:?} is not a valid usize, ignoring"
                    ),
                }
            }
        }
    }

    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        use std::io::Write as _;

        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
        let contents = toml::to_string_pretty(self)?;
        // Stage in same directory so persist() is an atomic rename, not a
        // cross-filesystem copy. NamedTempFile is auto-removed on Drop if
        // persist is not called (e.g. on error path).
        let mut tmp =
            tempfile::NamedTempFile::new_in(parent).map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        tmp.write_all(contents.as_bytes())
            .map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        tmp.as_file()
            .sync_all()
            .map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        tmp.persist(path)
            .map_err(|err| ConfigError::Write {
                path: path.to_path_buf(),
                source: err.error,
            })
            .map(|_| ())
    }

    pub fn save_default(&self) -> Result<(), ConfigError> {
        self.save_to(&default_config_path())
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
        self.embed.validate_endpoint_pool()?;
        self.llm.validate_endpoint_pool()?;
        self.validate_llm_judge_guardrails()?;
        if self.llm.enabled && self.llm.effective_endpoints()?.is_empty() {
            return Err(ConfigError::Validation(
                "llm.base_url must be set when llm.enabled is true".to_string(),
            ));
        }
        if self.memory_intelligence.llm.timeout_secs == 0 {
            return Err(ConfigError::InvalidConfig(
                "memory_intelligence.llm.timeout_secs must be greater than 0".to_string(),
            ));
        }
        for warning in self.llm_base_url_locality_warnings() {
            tracing::warn!(source = warning.source, "{}", warning.message);
        }
        if self.search.preview_chars == 0 {
            return Err(ConfigError::InvalidConfig(
                "search.preview_chars must be greater than 0".to_string(),
            ));
        }
        if self.api.write_queue_capacity == 0 {
            return Err(ConfigError::InvalidConfig(
                "api.write_queue_capacity must be greater than 0".to_string(),
            ));
        }
        if self.api.write_drain_timeout_secs == 0 {
            return Err(ConfigError::InvalidConfig(
                "api.write_drain_timeout_secs must be greater than 0".to_string(),
            ));
        }
        if self.api.search_db_deadline_secs == 0 {
            return Err(ConfigError::InvalidConfig(
                "api.search_db_deadline_secs must be greater than 0".to_string(),
            ));
        }
        if !(0..=5).contains(&self.turns.default_importance) {
            return Err(ConfigError::InvalidConfig(
                "turns.default_importance must be between 0 and 5".to_string(),
            ));
        }
        if !self.search.tunnel_penalty.is_finite()
            || !(0.0..=1.0).contains(&self.search.tunnel_penalty)
        {
            return Err(ConfigError::InvalidConfig(
                "search.tunnel_penalty must be a finite value in 0.0..=1.0".to_string(),
            ));
        }
        if self.search.reranker.timeout_secs == 0 {
            return Err(ConfigError::InvalidConfig(
                "search.reranker.timeout_secs must be greater than 0".to_string(),
            ));
        }
        if self.search.reranker.top_k == 0 {
            return Err(ConfigError::InvalidConfig(
                "search.reranker.top_k must be greater than 0".to_string(),
            ));
        }
        if self.search.reranker.enabled {
            normalize_reranker_endpoint_url(
                self.search.reranker.endpoint.as_deref().unwrap_or_default(),
            )
            .map_err(ConfigError::Validation)?;
            if self
                .search
                .reranker
                .model
                .as_deref()
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .is_none()
            {
                return Err(ConfigError::Validation(
                    "search.reranker.model must not be empty when search.reranker.enabled is true"
                        .to_string(),
                ));
            }
        }
        if self.search.decay.half_life_days == 0 {
            return Err(ConfigError::InvalidConfig(
                "search.decay.half_life_days must be greater than 0".to_string(),
            ));
        }
        if !self.search.decay.step_reduced_weight.is_finite()
            || !(0.0..=1.0).contains(&self.search.decay.step_reduced_weight)
        {
            return Err(ConfigError::InvalidConfig(
                "search.decay.step_reduced_weight must be a finite value in 0.0..=1.0".to_string(),
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
        if !self.consolidation.similarity_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.consolidation.similarity_threshold)
        {
            return Err(ConfigError::InvalidConfig(
                "consolidation.similarity_threshold must be a finite value in 0.0..=1.0"
                    .to_string(),
            ));
        }
        if self.consolidation.min_cluster_size < 2 {
            return Err(ConfigError::InvalidConfig(
                "consolidation.min_cluster_size must be at least 2".to_string(),
            ));
        }
        if self.consolidation.max_clusters_per_run == 0 {
            return Err(ConfigError::InvalidConfig(
                "consolidation.max_clusters_per_run must be greater than 0".to_string(),
            ));
        }
        match self.consolidation.strategy.as_str() {
            "richest_content" | "llm_summary" => {}
            other => {
                return Err(ConfigError::InvalidConfig(format!(
                    "unsupported consolidation.strategy: {other}"
                )));
            }
        }
        if self.crystallize.min_cluster_size < 2 {
            return Err(ConfigError::InvalidConfig(
                "crystallize.min_cluster_size must be at least 2".to_string(),
            ));
        }
        if !self.crystallize.readiness_threshold.is_finite()
            || self.crystallize.readiness_threshold < 0.0
        {
            return Err(ConfigError::InvalidConfig(
                "crystallize.readiness_threshold must be a finite value >= 0.0".to_string(),
            ));
        }
        if self.crystallize.max_candidates_per_run == 0 {
            return Err(ConfigError::InvalidConfig(
                "crystallize.max_candidates_per_run must be greater than 0".to_string(),
            ));
        }
        if self.sleep.nrem_prune_min_age_days == 0 {
            return Err(ConfigError::InvalidConfig(
                "sleep.nrem_prune_min_age_days must be greater than 0".to_string(),
            ));
        }
        if !(0..=5).contains(&self.sleep.nrem_prune_max_importance) {
            return Err(ConfigError::InvalidConfig(
                "sleep.nrem_prune_max_importance must be between 0 and 5".to_string(),
            ));
        }
        if !self.sleep.nrem_compaction_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.sleep.nrem_compaction_threshold)
        {
            return Err(ConfigError::InvalidConfig(
                "sleep.nrem_compaction_threshold must be a finite value in 0.0..=1.0".to_string(),
            ));
        }
        if self.sleep.salience_idle_minutes == 0 {
            return Err(ConfigError::InvalidConfig(
                "sleep.salience_idle_minutes must be greater than 0".to_string(),
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
        if self.embed.endpoint_vector_identity() != other.embed.endpoint_vector_identity() {
            fields.push("embedder.endpoints.vector_identity");
        }
        if self.llm.enabled != other.llm.enabled {
            fields.push("llm.enabled");
        }
        if self.llm.backend != other.llm.backend {
            fields.push("llm.backend");
        }
        // LLM endpoint/client request fields are safe to hot-reload: workers
        // cancel in-flight tasks on generation bump and rebuild the client for
        // the next claim cycle.
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
        if self.embed.endpoint_vector_identity() == candidate.embed.endpoint_vector_identity() {
            effective.embed.endpoints = candidate.embed.endpoints.clone();
            effective.embed.max_concurrent = candidate.embed.max_concurrent;
            effective.embed.retry = candidate.embed.retry.clone();
        } else {
            effective.embed.endpoints = self.embed.endpoints.clone();
            effective.embed.max_concurrent = self.embed.max_concurrent;
            effective.embed.retry = self.embed.retry.clone();
        }
        if self.llm.enabled != candidate.llm.enabled || self.llm.backend != candidate.llm.backend {
            effective.llm = self.llm.clone();
        } else {
            effective.llm.enabled = self.llm.enabled;
            effective.llm.backend = self.llm.backend.clone();
            // LLM endpoint/client request fields are hot-reloadable; keep the
            // candidate values so new workers/requests use the latest endpoint.
        }
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
        warnings.extend(self.llm_base_url_locality_warnings());
        if let Some(judge) = self.ingest_gating.llm_judge.as_ref()
            && !judge.enabled
            && judge.allow_fallback_worse_memory
        {
            warnings.push(RuntimeWarning {
                level: "warn",
                source: "llm",
                message: "LLM memory judging is disabled by explicit allow_fallback_worse_memory=true; memory quality may degrade.".to_string(),
            });
        }
        warnings
    }

    fn validate_llm_judge_guardrails(&self) -> Result<(), ConfigError> {
        let Some(judge) = self.ingest_gating.llm_judge.as_ref() else {
            return Ok(());
        };
        if !judge.enabled {
            if judge.allow_fallback_worse_memory {
                return Ok(());
            }
            return Err(ConfigError::Validation(
                "ingest_gating.llm_judge.enabled=false requires allow_fallback_worse_memory=true to acknowledge lower memory quality".to_string(),
            ));
        }
        if !self.llm.enabled {
            return Err(ConfigError::Validation(
                "ingest_gating.llm_judge.enabled=true requires [llm].enabled=true".to_string(),
            ));
        }
        if !self.llm.enabled_for.iter().any(|item| item == "gating") {
            return Err(ConfigError::Validation(
                "ingest_gating.llm_judge.enabled=true requires llm.enabled_for to include \"gating\"".to_string(),
            ));
        }
        if self.llm.effective_endpoints()?.is_empty() {
            return Err(ConfigError::Validation(
                "ingest_gating.llm_judge.enabled=true requires at least one configured LLM endpoint".to_string(),
            ));
        }
        Ok(())
    }

    fn llm_base_url_locality_warnings(&self) -> Vec<RuntimeWarning> {
        let mut warnings = Vec::new();
        warnings.extend(llm_endpoint_locality_warnings(
            "llm",
            self.llm
                .effective_endpoints()
                .ok()
                .as_deref()
                .unwrap_or(&[]),
        ));
        if self.memory_intelligence.llm.defines_endpoint() {
            let effective = self.memory_intelligence.effective_llm_config(&self.llm);
            warnings.extend(llm_endpoint_locality_warnings(
                "memory_intelligence.llm",
                effective
                    .effective_endpoints()
                    .ok()
                    .as_deref()
                    .unwrap_or(&[]),
            ));
        }
        if self.search.reranker.enabled
            && let Ok(endpoint) = normalize_reranker_endpoint_url(
                self.search.reranker.endpoint.as_deref().unwrap_or_default(),
            )
            && !llm_base_url_is_local_or_lan(&endpoint)
        {
            let host = llm_base_url_host(&endpoint)
                .map(|host| format!(" host `{host}`"))
                .unwrap_or_default();
            warnings.push(RuntimeWarning {
                level: "warn",
                source: "reranker",
                message: format!(
                    "search.reranker.endpoint{host} appears outside localhost/LAN; mempal assumes user-configured reranker endpoints are local or private network and does not block operation"
                ),
            });
        }
        warnings
    }
}

fn llm_endpoint_locality_warnings(
    setting: &str,
    endpoints: &[EffectiveLlmEndpoint],
) -> Vec<RuntimeWarning> {
    endpoints
        .iter()
        .filter_map(|endpoint| {
            let base_url = endpoint.base_url.trim();
            if base_url.is_empty() || llm_base_url_is_local_or_lan(base_url) {
                return None;
            }
            let host = llm_base_url_host(base_url)
                .map(|host| format!(" host `{host}`"))
                .unwrap_or_default();
            let field = if endpoint.id == "legacy" {
                format!("{setting}.base_url")
            } else {
                format!("{setting}.endpoints[{}].base_url", endpoint.id)
            };
            Some(RuntimeWarning {
                level: "warn",
                source: "llm",
                message: format!(
                    "{field}{host} appears outside localhost/LAN; mempal assumes user-configured LLM endpoints are local or private network and does not block operation"
                ),
            })
        })
        .collect()
}

fn llm_base_url_is_local_or_lan(base_url: &str) -> bool {
    let Some(host) = llm_base_url_host(base_url) else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") || !host.contains('.') {
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

fn llm_base_url_host(base_url: &str) -> Option<String> {
    Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
}

fn validate_llm_endpoint_pool_toml(root: &toml::Value) -> Result<(), ConfigError> {
    let Some(llm) = root.get("llm").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    if !llm.contains_key("endpoints") {
        return Ok(());
    }
    let scalar_keys = ["base_url", "model", "api_key", "api_key_env", "extra_body"];
    let conflicts = scalar_keys
        .into_iter()
        .filter(|key| llm.contains_key(*key))
        .collect::<Vec<_>>();
    if conflicts.is_empty() {
        return Ok(());
    }
    Err(ConfigError::Validation(format!(
        "llm endpoint list cannot be combined with legacy scalar endpoint fields: {}",
        conflicts.join(", ")
    )))
}

fn normalize_embed_endpoint_id(
    configured: Option<&str>,
    index: usize,
) -> Result<String, ConfigError> {
    normalize_endpoint_id(configured, index, "embed endpoints")
}

fn normalize_llm_endpoint_id(
    configured: Option<&str>,
    index: usize,
) -> Result<String, ConfigError> {
    normalize_endpoint_id(configured, index, "llm endpoint")
}

fn normalize_endpoint_id(
    configured: Option<&str>,
    index: usize,
    label: &str,
) -> Result<String, ConfigError> {
    let id = configured
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("endpoint-{}", index + 1));
    if id.len() > 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        return Err(ConfigError::Validation(format!(
            "{label} id `{id}` must match [A-Za-z0-9_.-]{{1,64}}"
        )));
    }
    Ok(id)
}

fn normalize_embed_endpoint_backend(backend: &str, field: &str) -> Result<String, ConfigError> {
    let backend = backend.trim();
    if backend.is_empty() {
        return Err(ConfigError::Validation(format!(
            "{field} must not be empty"
        )));
    }
    match backend {
        "openai_compat" | "api" => Ok("openai_compat".to_string()),
        other => Err(ConfigError::Validation(format!(
            "{field}={other:?} is not supported in embed endpoint pools; use openai_compat-compatible HTTP endpoints"
        ))),
    }
}

fn embed_backend_is_http(backend: &str) -> bool {
    matches!(backend, "openai_compat" | "api")
}

fn normalize_embed_endpoint_base_url(
    base_url: Option<&str>,
    field: &str,
) -> Result<String, ConfigError> {
    let base_url = base_url
        .map(str::trim)
        .filter(|base_url| !base_url.is_empty())
        .ok_or_else(|| ConfigError::Validation(format!("{field} must not be empty")))?
        .trim_end_matches('/')
        .to_string();
    validate_embed_base_url(&base_url, field).map_err(ConfigError::Validation)?;
    Ok(base_url)
}

fn normalize_llm_endpoint_base_url(
    base_url: Option<&str>,
    field: &str,
) -> Result<String, ConfigError> {
    let base_url = base_url
        .map(str::trim)
        .filter(|base_url| !base_url.is_empty())
        .ok_or_else(|| ConfigError::Validation(format!("{field} must not be empty")))?
        .trim_end_matches('/')
        .to_string();
    validate_llm_base_url(&base_url).map_err(ConfigError::Validation)?;
    Ok(base_url)
}

fn normalize_embed_endpoint_model(model: Option<&str>, field: &str) -> Result<String, ConfigError> {
    normalize_required_model(model, field)
}

fn normalize_llm_endpoint_model(model: Option<&str>, field: &str) -> Result<String, ConfigError> {
    normalize_required_model(model, field)
}

fn normalize_required_model(model: Option<&str>, field: &str) -> Result<String, ConfigError> {
    model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ConfigError::Validation(format!("{field} must not be empty")))
}

fn validate_embed_base_url(base_url: &str, field: &str) -> Result<(), String> {
    validate_base_url(base_url, field, "api_key_env")
}

pub fn validate_llm_base_url(base_url: &str) -> Result<(), String> {
    validate_base_url(base_url, "llm.base_url", "api_key_env")
}

fn validate_base_url(base_url: &str, field: &str, credential_hint: &str) -> Result<(), String> {
    let parsed =
        Url::parse(base_url).map_err(|error| format!("invalid {field} `{base_url}`: {error}"))?;
    if parsed.scheme().is_empty() {
        return Err(format!("{field} must include a URL scheme"));
    }
    if parsed.host_str().is_none() {
        return Err(format!("{field} must include a host"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!(
            "{field} must not include userinfo credentials; use {credential_hint} instead"
        ));
    }
    if parsed.query().is_some() {
        return Err(format!(
            "{field} must not include query parameters; move secrets to {credential_hint}"
        ));
    }
    if parsed.fragment().is_some() {
        return Err(format!("{field} must not include URL fragments"));
    }
    Ok(())
}

fn validate_embed_endpoint_vector_identity(
    endpoints: &[EffectiveEmbedEndpoint],
) -> Result<(), ConfigError> {
    let Some(first) = endpoints.first().map(EmbedVectorIdentity::from) else {
        return Ok(());
    };
    for endpoint in endpoints.iter().skip(1) {
        let identity = EmbedVectorIdentity::from(endpoint);
        if identity != first {
            return Err(ConfigError::Validation(
                "embed endpoints must share one vector identity (backend, model, dim); configure separate reindex targets for different embedding models".to_string(),
            ));
        }
    }
    Ok(())
}

pub fn normalize_reranker_endpoint_url(endpoint: &str) -> Result<String, String> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err(
            "search.reranker.endpoint must not be empty when reranker is enabled".to_string(),
        );
    }
    let raw = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint.to_string()
    } else {
        format!("http://{endpoint}")
    };
    let mut parsed = Url::parse(&raw)
        .map_err(|error| format!("invalid search.reranker.endpoint `{endpoint}`: {error}"))?;
    if parsed.host_str().is_none() {
        return Err("search.reranker.endpoint must include a host".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(
            "search.reranker.endpoint must not include userinfo credentials; run rerankers on trusted local/LAN endpoints"
                .to_string(),
        );
    }
    if parsed.query().is_some() {
        return Err(
            "search.reranker.endpoint must not include query parameters; configure a plain local/LAN endpoint URL"
                .to_string(),
        );
    }
    if parsed.path().is_empty() || parsed.path() == "/" {
        parsed.set_path("/v1/rerank");
    }
    Ok(parsed.to_string().trim_end_matches('/').to_string())
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
    /// Deprecated (P16): SessionEnd auto-ingest was removed. Field kept for config
    /// compat; set to false in your config to suppress the deprecation warning.
    pub auto_ingest_conversation: bool,
}

impl Default for HooksSessionEndConfig {
    fn default() -> Self {
        Self {
            extract_self_review: false,
            trailing_messages: DEFAULT_SESSION_REVIEW_TRAILING_MESSAGES,
            min_length: DEFAULT_SESSION_REVIEW_MIN_LENGTH,
            wing: DEFAULT_SESSION_REVIEW_WING.to_string(),
            auto_ingest_conversation: false,
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
    /// Maximum number of pending REST write requests before new writes get 503.
    pub write_queue_capacity: usize,
    /// Maximum time to wait for queued REST writes during graceful shutdown.
    pub write_drain_timeout_secs: u64,
    /// Maximum time a REST search may spend in synchronous database work before
    /// returning a partial/fallback response.
    pub search_db_deadline_secs: u64,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            addr: "127.0.0.1:3080".to_string(),
            write_queue_capacity: DEFAULT_API_WRITE_QUEUE_CAPACITY,
            write_drain_timeout_secs: DEFAULT_API_WRITE_DRAIN_TIMEOUT_SECS,
            search_db_deadline_secs: DEFAULT_API_SEARCH_DB_DEADLINE_SECS,
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
    pub endpoints: Vec<EmbedEndpointConfig>,
    pub max_concurrent: usize,
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
            endpoints: Vec::new(),
            max_concurrent: DEFAULT_EMBED_MAX_CONCURRENT,
            retry: RetryConfig::default(),
            alert: AlertConfig::default(),
            degradation: DegradationConfig::default(),
        }
    }
}

impl EmbedConfig {
    pub fn vector_embedder_fingerprint(&self, backend: &str, dim: usize) -> String {
        if backend == self.backend
            && let Some(identity) = self.endpoint_vector_identity()
        {
            return identity.fingerprint();
        }
        let base_url = self
            .resolved_openai_base_url()
            .unwrap_or_default()
            .trim_end_matches('/');
        let model = self.vector_fingerprint_model(backend);
        format!("{backend}:{model}:{base_url}:{dim}")
    }

    pub fn current_vector_embedder_fingerprint(&self, dim: usize) -> String {
        self.vector_embedder_fingerprint(self.backend.as_str(), dim)
    }

    fn vector_fingerprint_model(&self, backend: &str) -> String {
        if backend == "model2vec" {
            return self
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL2VEC_FINGERPRINT_MODEL.to_string());
        }
        self.resolved_openai_model()
            .or(self.model.as_deref())
            .unwrap_or_default()
            .to_string()
    }

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

    pub fn effective_endpoints(&self) -> Result<Vec<EffectiveEmbedEndpoint>, ConfigError> {
        let endpoints = self.effective_endpoints_without_validation()?;
        validate_embed_endpoint_vector_identity(&endpoints)?;
        Ok(endpoints)
    }

    pub fn validate_endpoint_pool(&self) -> Result<(), ConfigError> {
        if self.endpoints.is_empty() {
            return Ok(());
        }
        validate_embed_endpoint_vector_identity(&self.effective_endpoints_without_validation()?)
    }

    pub fn pool_capacity(&self) -> usize {
        self.effective_endpoints()
            .ok()
            .filter(|endpoints| !endpoints.is_empty())
            .map(|endpoints| {
                endpoints
                    .into_iter()
                    .map(|endpoint| endpoint.max_concurrent.max(1))
                    .sum::<usize>()
            })
            .unwrap_or_else(|| self.max_concurrent.max(1))
    }

    pub fn effective_model_summary(&self) -> Option<String> {
        summarize_effective_embed_endpoints(
            self,
            |endpoint| endpoint.model.clone(),
            EffectiveEmbedEndpoint::model_label,
        )
    }

    pub fn effective_base_url_summary(&self) -> Option<String> {
        summarize_effective_embed_endpoints(
            self,
            |endpoint| endpoint.base_url.clone(),
            EffectiveEmbedEndpoint::base_url_label,
        )
    }

    pub fn effective_endpoint_fingerprints(&self) -> Vec<serde_json::Value> {
        self.effective_endpoints()
            .unwrap_or_default()
            .into_iter()
            .map(|endpoint| {
                serde_json::json!({
                    "id": endpoint.id,
                    "backend": endpoint.backend,
                    "base_url": endpoint.base_url,
                    "model": endpoint.model,
                    "api_key_env": endpoint.api_key_env,
                    "priority": endpoint.priority,
                    "request_timeout_secs": endpoint.request_timeout_secs,
                    "retry_interval_secs": endpoint.retry_interval_secs,
                    "max_concurrent": endpoint.max_concurrent,
                    "dimensions": endpoint.dimensions,
                    "max_input_tokens": endpoint.max_input_tokens,
                })
            })
            .collect()
    }

    fn legacy_effective_endpoint(&self) -> Result<Option<EffectiveEmbedEndpoint>, ConfigError> {
        let backend = normalize_embed_endpoint_backend(&self.backend, "embed.backend")?;
        if !embed_backend_is_http(&backend) {
            return Ok(None);
        }
        let has_base_url = self
            .resolved_openai_base_url()
            .is_some_and(|base_url| !base_url.trim().is_empty());
        let has_model = self
            .resolved_openai_model()
            .is_some_and(|model| !model.trim().is_empty());
        if !has_base_url && !has_model {
            return Ok(None);
        }
        let base_url =
            normalize_embed_endpoint_base_url(self.resolved_openai_base_url(), "embed.base_url")?;
        let model = normalize_embed_endpoint_model(self.resolved_openai_model(), "embed.model")?;
        Ok(Some(EffectiveEmbedEndpoint {
            id: "legacy".to_string(),
            backend,
            base_url,
            model,
            api_key_env: self.openai_compat.api_key_env.clone(),
            priority: DEFAULT_EMBED_ENDPOINT_PRIORITY,
            request_timeout_secs: self.openai_compat.request_timeout_secs.max(1),
            retry_interval_secs: self.retry.interval_secs.max(1),
            max_concurrent: self.max_concurrent.max(1),
            dimensions: self.resolved_openai_dim(),
            max_input_tokens: self.openai_compat.max_input_tokens,
        }))
    }

    fn endpoint_vector_identity(&self) -> Option<EmbedVectorIdentity> {
        self.effective_endpoints()
            .ok()
            .and_then(|endpoints| endpoints.first().map(EmbedVectorIdentity::from))
    }

    fn effective_endpoints_without_validation(
        &self,
    ) -> Result<Vec<EffectiveEmbedEndpoint>, ConfigError> {
        if self.endpoints.is_empty() {
            return self.legacy_effective_endpoint().map(|endpoint| {
                endpoint
                    .into_iter()
                    .collect::<Vec<EffectiveEmbedEndpoint>>()
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        self.endpoints
            .iter()
            .enumerate()
            .map(|(index, endpoint)| {
                let id = normalize_embed_endpoint_id(endpoint.id.as_deref(), index)?;
                if !seen.insert(id.clone()) {
                    return Err(ConfigError::Validation(format!(
                        "embed endpoints id `{id}` must be unique"
                    )));
                }
                Ok(EffectiveEmbedEndpoint {
                    id,
                    backend: normalize_embed_endpoint_backend(
                        endpoint.backend.as_deref().unwrap_or(self.backend.as_str()),
                        format!("embed.endpoints[{index}].backend").as_str(),
                    )?,
                    base_url: normalize_embed_endpoint_base_url(
                        endpoint
                            .base_url
                            .as_deref()
                            .or_else(|| self.resolved_openai_base_url()),
                        format!("embed.endpoints[{index}].base_url").as_str(),
                    )?,
                    model: normalize_embed_endpoint_model(
                        endpoint
                            .model
                            .as_deref()
                            .or_else(|| self.resolved_openai_model()),
                        format!("embed.endpoints[{index}].model").as_str(),
                    )?,
                    api_key_env: endpoint
                        .api_key_env
                        .clone()
                        .or_else(|| self.openai_compat.api_key_env.clone()),
                    priority: endpoint.priority.unwrap_or(DEFAULT_EMBED_ENDPOINT_PRIORITY),
                    request_timeout_secs: endpoint
                        .request_timeout_secs
                        .unwrap_or(self.openai_compat.request_timeout_secs)
                        .max(1),
                    retry_interval_secs: endpoint
                        .retry_interval_secs
                        .unwrap_or(self.retry.interval_secs)
                        .max(1),
                    max_concurrent: endpoint
                        .max_concurrent
                        .unwrap_or(self.max_concurrent)
                        .max(1),
                    dimensions: endpoint
                        .dim
                        .or(self.openai_compat.dim)
                        .unwrap_or(DEFAULT_OPENAI_DIM),
                    max_input_tokens: endpoint
                        .max_input_tokens
                        .or(self.openai_compat.max_input_tokens),
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct EmbedEndpointConfig {
    pub id: Option<String>,
    pub backend: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
    /// Lower numbers are tried first. Equal priority endpoints share active load
    /// according to each endpoint's `max_concurrent` capacity.
    pub priority: Option<i32>,
    #[serde(alias = "timeout_secs")]
    pub request_timeout_secs: Option<u64>,
    pub retry_interval_secs: Option<u64>,
    pub max_concurrent: Option<usize>,
    pub dim: Option<usize>,
    pub max_input_tokens: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveEmbedEndpoint {
    pub id: String,
    pub backend: String,
    pub base_url: String,
    pub model: String,
    pub api_key_env: Option<String>,
    pub priority: i32,
    pub request_timeout_secs: u64,
    pub retry_interval_secs: u64,
    pub max_concurrent: usize,
    pub dimensions: usize,
    pub max_input_tokens: Option<usize>,
}

impl EffectiveEmbedEndpoint {
    pub fn model_label(&self) -> String {
        format!("{}={}", self.id, self.model)
    }

    pub fn base_url_label(&self) -> String {
        format!("{}={}", self.id, self.base_url)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EmbedVectorIdentity {
    backend: String,
    model: String,
    dimensions: usize,
}

impl EmbedVectorIdentity {
    fn fingerprint(&self) -> String {
        format!("{}:{}:pool:{}", self.backend, self.model, self.dimensions)
    }
}

impl From<&EffectiveEmbedEndpoint> for EmbedVectorIdentity {
    fn from(endpoint: &EffectiveEmbedEndpoint) -> Self {
        Self {
            backend: endpoint.backend.clone(),
            model: endpoint.model.clone(),
            dimensions: endpoint.dimensions,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct LlmConfig {
    pub enabled: bool,
    pub backend: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub extra_body: Option<serde_json::Value>,
    pub endpoints: Vec<LlmEndpointConfig>,
    #[serde(alias = "timeout_secs")]
    pub request_timeout_secs: u64,
    pub health_probe_timeout_secs: u64,
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
            extra_body: None,
            endpoints: Vec::new(),
            request_timeout_secs: DEFAULT_LLM_REQUEST_TIMEOUT_SECS,
            health_probe_timeout_secs: DEFAULT_LLM_HEALTH_PROBE_TIMEOUT_SECS,
            retry_interval_secs: DEFAULT_LLM_RETRY_INTERVAL_SECS,
            max_concurrent: DEFAULT_LLM_MAX_CONCURRENT,
            enabled_for: vec!["gating".to_string()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct LlmEndpointConfig {
    pub id: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub extra_body: Option<serde_json::Value>,
    /// Lower numbers are tried first. Equal priority endpoints share active load
    /// according to each endpoint's `max_concurrent` capacity.
    pub priority: Option<i32>,
    #[serde(alias = "timeout_secs")]
    pub request_timeout_secs: Option<u64>,
    pub health_probe_timeout_secs: Option<u64>,
    pub retry_interval_secs: Option<u64>,
    pub max_concurrent: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveLlmEndpoint {
    pub id: String,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub extra_body: Option<serde_json::Value>,
    pub priority: i32,
    pub request_timeout_secs: u64,
    pub health_probe_timeout_secs: u64,
    pub retry_interval_secs: u64,
    pub max_concurrent: usize,
}

impl EffectiveLlmEndpoint {
    pub fn model_label(&self) -> String {
        format!("{}={}", self.id, self.model)
    }

    pub fn base_url_label(&self) -> String {
        format!("{}={}", self.id, self.base_url)
    }
}

impl LlmConfig {
    pub fn effective_endpoints(&self) -> Result<Vec<EffectiveLlmEndpoint>, ConfigError> {
        self.validate_endpoint_pool()?;
        if self.endpoints.is_empty() {
            return self
                .legacy_effective_endpoint()
                .map(|endpoint| endpoint.into_iter().collect::<Vec<EffectiveLlmEndpoint>>());
        }

        let mut seen = std::collections::BTreeSet::new();
        self.endpoints
            .iter()
            .enumerate()
            .map(|(index, endpoint)| {
                let id = normalize_llm_endpoint_id(endpoint.id.as_deref(), index)?;
                if !seen.insert(id.clone()) {
                    return Err(ConfigError::Validation(format!(
                        "llm.endpoints id `{id}` must be unique"
                    )));
                }
                let base_url = normalize_llm_endpoint_base_url(
                    endpoint.base_url.as_deref(),
                    format!("llm.endpoints[{index}].base_url").as_str(),
                )?;
                let model = normalize_llm_endpoint_model(
                    endpoint.model.as_deref(),
                    format!("llm.endpoints[{index}].model").as_str(),
                )?;
                Ok(EffectiveLlmEndpoint {
                    id,
                    base_url,
                    model,
                    api_key: endpoint.api_key.clone(),
                    api_key_env: endpoint.api_key_env.clone(),
                    extra_body: endpoint.extra_body.clone(),
                    priority: endpoint.priority.unwrap_or(DEFAULT_LLM_ENDPOINT_PRIORITY),
                    request_timeout_secs: endpoint
                        .request_timeout_secs
                        .unwrap_or(self.request_timeout_secs),
                    health_probe_timeout_secs: endpoint
                        .health_probe_timeout_secs
                        .unwrap_or(self.health_probe_timeout_secs)
                        .max(1),
                    retry_interval_secs: endpoint
                        .retry_interval_secs
                        .unwrap_or(self.retry_interval_secs)
                        .max(1),
                    max_concurrent: endpoint
                        .max_concurrent
                        .unwrap_or(self.max_concurrent)
                        .max(1),
                })
            })
            .collect()
    }

    pub fn validate_endpoint_pool(&self) -> Result<(), ConfigError> {
        if self.endpoints.is_empty() {
            return Ok(());
        }
        if self.has_legacy_endpoint_definition() {
            return Err(ConfigError::Validation(
                "llm endpoint list cannot be combined with legacy scalar endpoint fields"
                    .to_string(),
            ));
        }
        Ok(())
    }

    pub fn pool_capacity(&self) -> usize {
        self.effective_endpoints()
            .ok()
            .filter(|endpoints| !endpoints.is_empty())
            .map(|endpoints| {
                endpoints
                    .into_iter()
                    .map(|endpoint| endpoint.max_concurrent.max(1))
                    .sum::<usize>()
            })
            .unwrap_or_else(|| self.max_concurrent.max(1))
    }

    pub fn effective_model_summary(&self) -> Option<String> {
        summarize_effective_endpoints(
            self,
            |endpoint| endpoint.model.clone(),
            EffectiveLlmEndpoint::model_label,
        )
    }

    pub fn effective_base_url_summary(&self) -> Option<String> {
        summarize_effective_endpoints(
            self,
            |endpoint| endpoint.base_url.clone(),
            EffectiveLlmEndpoint::base_url_label,
        )
    }

    pub fn effective_endpoint_fingerprints(&self) -> Vec<serde_json::Value> {
        self.effective_endpoints()
            .unwrap_or_default()
            .into_iter()
            .map(|endpoint| {
                serde_json::json!({
                    "id": endpoint.id,
                    "base_url": endpoint.base_url,
                    "model": endpoint.model,
                    "api_key_env": endpoint.api_key_env,
                    "extra_body": endpoint.extra_body,
                    "priority": endpoint.priority,
                    "request_timeout_secs": endpoint.request_timeout_secs,
                    "retry_interval_secs": endpoint.retry_interval_secs,
                    "max_concurrent": endpoint.max_concurrent,
                })
            })
            .collect()
    }

    fn legacy_effective_endpoint(&self) -> Result<Option<EffectiveLlmEndpoint>, ConfigError> {
        let has_base_url = self
            .base_url
            .as_deref()
            .is_some_and(|base_url| !base_url.trim().is_empty());
        let has_model = self
            .model
            .as_deref()
            .is_some_and(|model| !model.trim().is_empty());
        if !has_base_url && !has_model {
            return Ok(None);
        }
        let base_url = normalize_llm_endpoint_base_url(self.base_url.as_deref(), "llm.base_url")?;
        let model = normalize_llm_endpoint_model(self.model.as_deref(), "llm.model")?;
        Ok(Some(EffectiveLlmEndpoint {
            id: "legacy".to_string(),
            base_url,
            model,
            api_key: self.api_key.clone(),
            api_key_env: self.api_key_env.clone(),
            extra_body: self.extra_body.clone(),
            priority: DEFAULT_LLM_ENDPOINT_PRIORITY,
            request_timeout_secs: self.request_timeout_secs,
            health_probe_timeout_secs: self.health_probe_timeout_secs.max(1),
            retry_interval_secs: self.retry_interval_secs.max(1),
            max_concurrent: self.max_concurrent.max(1),
        }))
    }

    fn has_legacy_endpoint_definition(&self) -> bool {
        self.base_url
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            || self
                .model
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            || self
                .api_key
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            || self
                .api_key_env
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            || self.extra_body.is_some()
    }
}

fn summarize_effective_endpoints(
    config: &LlmConfig,
    single_label: fn(&EffectiveLlmEndpoint) -> String,
    multi_label: fn(&EffectiveLlmEndpoint) -> String,
) -> Option<String> {
    let endpoints = config.effective_endpoints().ok()?;
    if endpoints.is_empty() {
        return None;
    }
    if endpoints.len() == 1 {
        return endpoints.first().map(single_label);
    }
    Some(
        endpoints
            .iter()
            .map(multi_label)
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn summarize_effective_embed_endpoints(
    config: &EmbedConfig,
    single_label: fn(&EffectiveEmbedEndpoint) -> String,
    multi_label: fn(&EffectiveEmbedEndpoint) -> String,
) -> Option<String> {
    let endpoints = config.effective_endpoints().ok()?;
    if endpoints.is_empty() {
        return None;
    }
    if endpoints.len() == 1 {
        return endpoints.first().map(single_label);
    }
    Some(
        endpoints
            .iter()
            .map(multi_label)
            .collect::<Vec<_>>()
            .join(", "),
    )
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct MemoryIntelligenceConfig {
    pub mode: IntelligenceMode,
    pub llm: MemoryIntelligenceLlmConfig,
}

impl Default for MemoryIntelligenceConfig {
    fn default() -> Self {
        Self {
            mode: IntelligenceMode::Deterministic,
            llm: MemoryIntelligenceLlmConfig::default(),
        }
    }
}

impl MemoryIntelligenceConfig {
    pub fn effective_llm_config(&self, base: &LlmConfig) -> LlmConfig {
        let mut config = base.clone();
        config.enabled = self.mode.uses_llm();
        let overlay_defines_endpoint_identity = self.llm.defines_endpoint_identity();
        if overlay_defines_endpoint_identity {
            config.endpoints.clear();
        }
        if self.llm.base_url.is_some() {
            config.base_url = self.llm.base_url.clone();
        }
        if self.llm.model.is_some() {
            config.model = self.llm.model.clone();
        }
        config.request_timeout_secs = self.llm.timeout_secs;
        if !overlay_defines_endpoint_identity && !config.endpoints.is_empty() {
            for endpoint in &mut config.endpoints {
                if self.llm.api_key.is_some() {
                    endpoint.api_key = self.llm.api_key.clone();
                }
                if self.llm.api_key_env.is_some() {
                    if self.llm.api_key.is_none() {
                        endpoint.api_key = None;
                    }
                    endpoint.api_key_env = self.llm.api_key_env.clone();
                }
                if self.llm.extra_body.is_some() {
                    endpoint.extra_body = self.llm.extra_body.clone();
                }
            }
        } else {
            if self.llm.api_key.is_some() {
                config.api_key = self.llm.api_key.clone();
            }
            if self.llm.api_key_env.is_some() {
                if self.llm.api_key.is_none() {
                    config.api_key = None;
                }
                config.api_key_env = self.llm.api_key_env.clone();
            }
            if self.llm.extra_body.is_some() {
                config.extra_body = self.llm.extra_body.clone();
            }
        }
        config
    }

    pub fn has_effective_llm_endpoint(&self, base: &LlmConfig) -> bool {
        let effective = self.effective_llm_config(base);
        self.mode.uses_llm()
            && effective
                .effective_endpoints()
                .is_ok_and(|endpoints| !endpoints.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct MemoryIntelligenceLlmConfig {
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub timeout_secs: u64,
    pub extra_body: Option<serde_json::Value>,
}

impl MemoryIntelligenceLlmConfig {
    fn defines_endpoint_identity(&self) -> bool {
        self.base_url
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            || self
                .model
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
    }

    pub fn defines_endpoint(&self) -> bool {
        self.base_url
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            || self
                .model
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            || self
                .api_key
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            || self
                .api_key_env
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            || self.extra_body.is_some()
    }
}

impl Default for MemoryIntelligenceLlmConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            model: None,
            api_key: None,
            api_key_env: None,
            timeout_secs: DEFAULT_MEMORY_INTELLIGENCE_TIMEOUT_SECS,
            extra_body: None,
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
    pub bm25_fallback: bool,
    pub progressive_disclosure: bool,
    pub exclude_raw_turns: bool,
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
    pub reranker: SearchRerankerConfig,
    pub decay: DecayConfig,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            strict_project_isolation: false,
            bm25_fallback: DEFAULT_SEARCH_BM25_FALLBACK,
            progressive_disclosure: true,
            exclude_raw_turns: true,
            preview_chars: DEFAULT_SEARCH_PREVIEW_CHARS,
            tunnel_fanout_cap: DEFAULT_SEARCH_TUNNEL_FANOUT_CAP,
            tunnel_hints_display_cap: DEFAULT_SEARCH_TUNNEL_HINTS_DISPLAY_CAP,
            tunnel_penalty: DEFAULT_SEARCH_TUNNEL_PENALTY,
            reranker: SearchRerankerConfig::default(),
            decay: DecayConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct SearchRerankerConfig {
    pub enabled: bool,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub timeout_secs: u64,
    pub top_k: usize,
}

impl Default for SearchRerankerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            model: None,
            timeout_secs: DEFAULT_SEARCH_RERANKER_TIMEOUT_SECS,
            top_k: DEFAULT_SEARCH_RERANKER_TOP_K,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct DecayConfig {
    pub mode: DecayMode,
    pub half_life_days: u64,
    pub step_full_days: u64,
    pub step_reduced_weight: f64,
}

impl Default for DecayConfig {
    fn default() -> Self {
        Self {
            mode: DecayMode::None,
            half_life_days: DEFAULT_SEARCH_DECAY_HALF_LIFE_DAYS,
            step_full_days: DEFAULT_SEARCH_DECAY_STEP_FULL_DAYS,
            step_reduced_weight: DEFAULT_SEARCH_DECAY_STEP_REDUCED_WEIGHT,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecayMode {
    #[default]
    None,
    Exponential,
    Linear,
    Step,
}

impl std::fmt::Display for DecayMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::None => "none",
            Self::Exponential => "exponential",
            Self::Linear => "linear",
            Self::Step => "step",
        };
        f.write_str(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStorageMode {
    Off,
    RawEvidence,
    Summarized,
}

impl Default for TurnStorageMode {
    fn default() -> Self {
        DEFAULT_TURN_STORAGE_MODE
    }
}

impl std::fmt::Display for TurnStorageMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Off => "off",
            Self::RawEvidence => "raw_evidence",
            Self::Summarized => "summarized",
        };
        f.write_str(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct TurnsConfig {
    pub storage_mode: TurnStorageMode,
    pub default_importance: i32,
    pub raw_turn_wings: Vec<String>,
    pub raw_turn_rooms: Vec<String>,
}

impl Default for TurnsConfig {
    fn default() -> Self {
        Self {
            storage_mode: TurnStorageMode::default(),
            default_importance: DEFAULT_TURN_IMPORTANCE,
            raw_turn_wings: vec!["hooks-raw".to_string(), "hermes-user".to_string()],
            raw_turn_rooms: vec!["turns".to_string(), "turns/raw".to_string()],
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
    pub allow_fallback_worse_memory: bool,
    pub quality_policy: GatingQualityPolicy,
    pub system_prompt: Option<String>,
    pub threshold: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatingQualityPolicy {
    #[default]
    Tiered,
    LlmFirst,
    LlmRequiredForKeep,
}

impl std::fmt::Display for GatingQualityPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Tiered => "tiered",
            Self::LlmFirst => "llm_first",
            Self::LlmRequiredForKeep => "llm_required_for_keep",
        };
        f.write_str(value)
    }
}

impl Default for LlmJudgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_fallback_worse_memory: false,
            quality_policy: GatingQualityPolicy::default(),
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
    #[error("failed to write config to {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize config TOML")]
    Serialize(#[from] toml::ser::Error),
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

#[derive(Debug, Clone)]
pub struct LlmRuntimeSnapshot {
    pub config: Arc<Config>,
    pub generation: u64,
}

pub struct ConfigHandle;

impl ConfigHandle {
    pub fn bootstrap(path: impl AsRef<Path>) -> Result<(), ConfigError> {
        super::hot_reload::global_hot_reload_state().bootstrap(path.as_ref())
    }

    pub fn bootstrap_quiet(path: impl AsRef<Path>) -> Result<(), ConfigError> {
        super::hot_reload::global_hot_reload_state().bootstrap_quiet(path.as_ref())
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

    pub fn current_llm_runtime_snapshot() -> LlmRuntimeSnapshot {
        let state = super::hot_reload::global_hot_reload_state();
        let generation = state.current_llm_generation();
        let config = state.current();
        LlmRuntimeSnapshot { config, generation }
    }

    pub fn current_embed_generation() -> u64 {
        super::hot_reload::global_hot_reload_state().current_embed_generation()
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
        for event in Self::restart_required_pending() {
            warnings.push(RuntimeWarning {
                level: "warn",
                source: "config",
                message: event,
            });
        }
        warnings
    }

    pub fn restart_required_pending() -> Vec<String> {
        let mut events =
            super::hot_reload::global_hot_reload_state().restart_required_pending_events();
        events.extend(
            super::hot_reload::restart_required_pending_events_from_config_path(
                &default_config_path(),
            ),
        );
        let mut seen = std::collections::BTreeSet::new();
        events
            .into_iter()
            .filter(|event| seen.insert(event.clone()))
            .collect()
    }

    pub fn runtime_prototypes() -> Vec<String> {
        super::hot_reload::global_hot_reload_state().runtime_prototypes()
    }

    pub fn simulate_notify_failure() {
        super::hot_reload::global_hot_reload_state().simulate_notify_failure();
    }

    /// Subscribe to LLM config generation changes.
    ///
    /// The counter increments whenever a hot-reloadable LLM field (endpoint,
    /// credentials, model, retry_interval_secs, enabled_for, max_concurrent)
    /// changes via hot-reload.
    /// LLM workers subscribe here to cancel in-flight requests on config change.
    pub fn subscribe_llm_gen() -> tokio::sync::watch::Receiver<u64> {
        super::hot_reload::global_hot_reload_state().subscribe_llm_gen()
    }

    /// Subscribe to embedding endpoint generation changes.
    ///
    /// The counter increments when hot-reload applies endpoint pool runtime
    /// fields while preserving embedding vector identity.
    pub fn subscribe_embed_gen() -> tokio::sync::watch::Receiver<u64> {
        super::hot_reload::global_hot_reload_state().subscribe_embed_gen()
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

#[cfg(test)]
pub(crate) fn global_config_test_lock() -> Arc<tokio::sync::Mutex<()>> {
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    Arc::clone(LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))))
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
    /// Include Phase-2 knowledge cards by default in context assembly.
    pub include_cards_default: bool,
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
            include_cards_default: false,
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

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ConsolidationConfig {
    pub similarity_threshold: f64,
    pub min_cluster_size: usize,
    pub max_clusters_per_run: usize,
    pub strategy: String,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            similarity_threshold: DEFAULT_CONSOLIDATION_SIMILARITY_THRESHOLD,
            min_cluster_size: DEFAULT_CONSOLIDATION_MIN_CLUSTER_SIZE,
            max_clusters_per_run: DEFAULT_CONSOLIDATION_MAX_CLUSTERS_PER_RUN,
            strategy: DEFAULT_CONSOLIDATION_STRATEGY.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct CrystallizeConfig {
    pub enabled: bool,
    pub min_cluster_size: usize,
    pub readiness_threshold: f64,
    pub auto_approve: bool,
    pub max_candidates_per_run: usize,
}

impl Default for CrystallizeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_cluster_size: DEFAULT_CRYSTALLIZE_MIN_CLUSTER_SIZE,
            readiness_threshold: DEFAULT_CRYSTALLIZE_READINESS_THRESHOLD,
            auto_approve: false,
            max_candidates_per_run: DEFAULT_CRYSTALLIZE_MAX_CANDIDATES_PER_RUN,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct SleepConfig {
    pub enabled: bool,
    pub nrem_prune_min_age_days: u64,
    pub nrem_prune_max_importance: i32,
    pub nrem_compaction_threshold: f64,
    pub rem_auto_resolve: bool,
    pub salience_idle_minutes: u64,
    pub schedule: String,
}

impl Default for SleepConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            nrem_prune_min_age_days: DEFAULT_SLEEP_NREM_PRUNE_MIN_AGE_DAYS,
            nrem_prune_max_importance: DEFAULT_SLEEP_NREM_PRUNE_MAX_IMPORTANCE,
            nrem_compaction_threshold: DEFAULT_SLEEP_NREM_COMPACTION_THRESHOLD,
            rem_auto_resolve: true,
            salience_idle_minutes: DEFAULT_SLEEP_SALIENCE_IDLE_MINUTES,
            schedule: DEFAULT_SLEEP_SCHEDULE.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use super::{Config, DecayMode};

    // Serialize env-mutating tests to prevent flaky parallel interference.
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.get_or_init(Mutex::default).lock().unwrap()
    }

    #[test]
    fn search_decay_defaults_to_none() {
        let config = Config::parse("").expect("empty config should parse");
        assert_eq!(config.search.decay.mode, DecayMode::None);
        assert_eq!(config.search.decay.half_life_days, 90);
        assert_eq!(config.search.decay.step_full_days, 30);
        assert_eq!(config.search.decay.step_reduced_weight, 0.5);
        assert!(!config.search.reranker.enabled);
        assert!(config.search.reranker.endpoint.is_none());
        assert!(config.search.reranker.model.is_none());
    }

    #[test]
    fn search_reranker_config_parses_nested_section() {
        let config = Config::parse(
            r#"
            [search.reranker]
            enabled = true
            endpoint = "gb10:18003"
            model = "qwen3-reranker"
            timeout_secs = 3
            top_k = 12
            "#,
        )
        .expect("reranker config should parse");

        assert!(config.search.reranker.enabled);
        assert_eq!(
            super::normalize_reranker_endpoint_url(
                config
                    .search
                    .reranker
                    .endpoint
                    .as_deref()
                    .expect("endpoint")
            )
            .expect("normalize endpoint"),
            "http://gb10:18003/v1/rerank"
        );
        assert_eq!(
            config.search.reranker.model.as_deref(),
            Some("qwen3-reranker")
        );
        assert_eq!(config.search.reranker.timeout_secs, 3);
        assert_eq!(config.search.reranker.top_k, 12);
    }

    #[test]
    fn search_reranker_config_rejects_invalid_enabled_config() {
        let err = Config::parse(
            r#"
            [search.reranker]
            enabled = true
            model = "qwen3-reranker"
            "#,
        )
        .expect_err("enabled reranker without endpoint must be rejected");
        assert!(
            err.to_string().contains("search.reranker.endpoint"),
            "unexpected error: {err}"
        );

        let err = Config::parse(
            r#"
            [search.reranker]
            enabled = true
            endpoint = "gb10:18003"
            "#,
        )
        .expect_err("enabled reranker without model must be rejected");
        assert!(
            err.to_string().contains("search.reranker.model"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn search_reranker_config_rejects_invalid_timeout_and_top_k() {
        let err = Config::parse(
            r#"
            [search.reranker]
            timeout_secs = 0
            "#,
        )
        .expect_err("zero timeout must be rejected");
        assert!(
            err.to_string().contains("search.reranker.timeout_secs"),
            "unexpected error: {err}"
        );

        let err = Config::parse(
            r#"
            [search.reranker]
            top_k = 0
            "#,
        )
        .expect_err("zero top_k must be rejected");
        assert!(
            err.to_string().contains("search.reranker.top_k"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn search_decay_config_parses_nested_section() {
        let config = Config::parse(
            r#"
            [search.decay]
            mode = "exponential"
            half_life_days = 45
            step_full_days = 10
            step_reduced_weight = 0.25
            "#,
        )
        .expect("decay config should parse");

        assert_eq!(config.search.decay.mode, DecayMode::Exponential);
        assert_eq!(config.search.decay.half_life_days, 45);
        assert_eq!(config.search.decay.step_full_days, 10);
        assert_eq!(config.search.decay.step_reduced_weight, 0.25);
    }

    #[test]
    fn search_decay_config_rejects_invalid_values() {
        let err = Config::parse(
            r#"
            [search.decay]
            half_life_days = 0
            "#,
        )
        .expect_err("zero half-life must be rejected");
        assert!(
            err.to_string().contains("search.decay.half_life_days"),
            "unexpected error: {err}"
        );

        let err = Config::parse(
            r#"
            [search.decay]
            step_reduced_weight = 1.5
            "#,
        )
        .expect_err("step weight above one must be rejected");
        assert!(
            err.to_string().contains("search.decay.step_reduced_weight"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn apply_env_overrides_present_applies() {
        let _guard = env_lock();
        // SAFETY: single-threaded test context; ENV_LOCK mutex serializes all env mutations
        // in this module, so no concurrent env access is possible.
        unsafe {
            std::env::set_var("MEMPAL_EMBED_BACKEND", "stub");
            std::env::set_var("MEMPAL_EMBED_BASE_URL", "http://127.0.0.1:9999/v1");
            std::env::set_var("MEMPAL_EMBED_MODEL", "my-model");
            std::env::set_var("MEMPAL_EMBED_DIM", "512");
        }
        let config = Config::parse("").expect("parse with env overrides");
        // SAFETY: single-threaded test context; ENV_LOCK mutex serializes all env mutations
        // in this module, so no concurrent env access is possible.
        unsafe {
            std::env::remove_var("MEMPAL_EMBED_BACKEND");
            std::env::remove_var("MEMPAL_EMBED_BASE_URL");
            std::env::remove_var("MEMPAL_EMBED_MODEL");
            std::env::remove_var("MEMPAL_EMBED_DIM");
        }
        assert_eq!(config.embed.backend, "stub");
        assert_eq!(
            config.embed.openai_compat.base_url.as_deref(),
            Some("http://127.0.0.1:9999/v1")
        );
        assert_eq!(
            config.embed.openai_compat.model.as_deref(),
            Some("my-model")
        );
        assert_eq!(config.embed.openai_compat.dim, Some(512));
    }

    #[test]
    fn apply_env_overrides_absent_leaves_default() {
        let _guard = env_lock();
        let backend_was = std::env::var("MEMPAL_EMBED_BACKEND").ok();
        let url_was = std::env::var("MEMPAL_EMBED_BASE_URL").ok();
        let model_was = std::env::var("MEMPAL_EMBED_MODEL").ok();
        let dim_was = std::env::var("MEMPAL_EMBED_DIM").ok();
        // SAFETY: single-threaded test context; ENV_LOCK mutex serializes all env mutations
        // in this module, so no concurrent env access is possible.
        unsafe {
            std::env::remove_var("MEMPAL_EMBED_BACKEND");
            std::env::remove_var("MEMPAL_EMBED_BASE_URL");
            std::env::remove_var("MEMPAL_EMBED_MODEL");
            std::env::remove_var("MEMPAL_EMBED_DIM");
        }
        let config = Config::parse("").expect("parse without env overrides");
        // SAFETY: single-threaded test context; ENV_LOCK mutex serializes all env mutations
        // in this module, so no concurrent env access is possible.
        unsafe {
            if let Some(v) = backend_was {
                std::env::set_var("MEMPAL_EMBED_BACKEND", v);
            }
            if let Some(v) = url_was {
                std::env::set_var("MEMPAL_EMBED_BASE_URL", v);
            }
            if let Some(v) = model_was {
                std::env::set_var("MEMPAL_EMBED_MODEL", v);
            }
            if let Some(v) = dim_was {
                std::env::set_var("MEMPAL_EMBED_DIM", v);
            }
        }
        // Empty config.toml with no env overrides → model2vec fallback; dim keeps struct default
        assert_eq!(config.embed.backend, "model2vec");
        assert!(config.embed.openai_compat.base_url.is_none());
        assert!(config.embed.openai_compat.model.is_none());
        assert_eq!(
            config.embed.openai_compat.dim,
            Some(super::DEFAULT_OPENAI_DIM)
        );
    }
}
