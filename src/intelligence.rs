use std::collections::HashSet;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;

use crate::core::config::Config;
use crate::core::types::{IntelligenceMode, SearchResult, Triple};
use crate::core::utils::{build_triple_id, current_timestamp};
use crate::llm::{LlmMessage, LlmRequest, LlmRouter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnhancedContent {
    pub raw_content: String,
    pub candidate_facts: Vec<String>,
    pub tags: Vec<String>,
    pub used_llm: bool,
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnhancedSearchResult {
    pub drawer_id: String,
    pub summary: String,
    pub relevance_boost: f32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnhancedResults {
    pub items: Vec<EnhancedSearchResult>,
    pub used_llm: bool,
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntelligenceHealthSnapshot {
    pub failure_count: u64,
    pub last_error: Option<String>,
    pub last_success_at_unix_ms: Option<u64>,
}

pub struct IntelligenceStatus {
    failure_count: AtomicU64,
    last_success_at_unix_ms: AtomicU64,
    last_error: std::sync::Mutex<Option<String>>,
}

impl IntelligenceStatus {
    fn new() -> Self {
        Self {
            failure_count: AtomicU64::new(0),
            last_success_at_unix_ms: AtomicU64::new(0),
            last_error: std::sync::Mutex::new(None),
        }
    }

    pub fn record_success(&self) {
        self.failure_count.store(0, Ordering::SeqCst);
        self.last_success_at_unix_ms
            .store(current_unix_ms(), Ordering::SeqCst);
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = None;
        }
    }

    pub fn record_failure(&self, error: &impl std::fmt::Display) {
        self.failure_count.fetch_add(1, Ordering::SeqCst);
        let message = crate::core::config::scrub_sensitive_text(&error.to_string());
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = Some(message);
        }
    }

    pub fn snapshot(&self) -> IntelligenceHealthSnapshot {
        let success = self.last_success_at_unix_ms.load(Ordering::SeqCst);
        IntelligenceHealthSnapshot {
            failure_count: self.failure_count.load(Ordering::SeqCst),
            last_error: self.last_error.lock().ok().and_then(|guard| guard.clone()),
            last_success_at_unix_ms: (success > 0).then_some(success),
        }
    }
}

pub fn global_intelligence_status() -> &'static IntelligenceStatus {
    static STATUS: OnceLock<IntelligenceStatus> = OnceLock::new();
    STATUS.get_or_init(IntelligenceStatus::new)
}

#[derive(Debug, Error)]
pub enum IntelligenceError {
    #[error("LLM call failed: {0}")]
    Llm(#[from] crate::llm::LlmError),
    #[error("LLM output was not valid JSON: {0}")]
    InvalidSchema(String),
    #[error("LLM output included unsupported fact: {0}")]
    UnsupportedFact(String),
    #[error("LLM output did not preserve source reference: {0}")]
    MissingSourceRef(String),
    #[error("LLM output marked a contradiction without marking it as a correction")]
    UnmarkedContradiction,
}

pub type Result<T> = std::result::Result<T, IntelligenceError>;

#[async_trait]
pub trait IntelligenceEnhancer: Send + Sync {
    async fn enhance_ingest(&self, raw_content: &str) -> Result<EnhancedContent>;
    async fn enhance_search(&self, results: &[SearchResult]) -> Result<EnhancedResults>;
    async fn extract_kg_triples(&self, content: &str) -> Result<Vec<Triple>>;
    async fn explain_contradiction(&self, a: &str, b: &str) -> Result<String>;
}

pub struct IntelligenceRouter {
    mode: IntelligenceMode,
    llm: Option<LlmIntelligenceEnhancer>,
}

impl IntelligenceRouter {
    pub fn from_config(config: &Config) -> Self {
        let mode = config.memory_intelligence.mode;
        let llm = if config
            .memory_intelligence
            .has_effective_llm_endpoint(&config.llm)
        {
            let llm_config = config.memory_intelligence.effective_llm_config(&config.llm);
            LlmRouter::from_config(&llm_config)
                .ok()
                .map(LlmIntelligenceEnhancer::new)
        } else {
            None
        };
        Self { mode, llm }
    }

    pub fn mode(&self) -> IntelligenceMode {
        self.mode
    }

    pub fn llm_configured(&self) -> bool {
        self.llm.is_some()
    }

    pub async fn enhance_ingest(&self, raw_content: &str) -> EnhancedContent {
        if !self.mode.uses_llm() {
            return deterministic_enhance_ingest(raw_content);
        }
        let Some(llm) = self.llm.as_ref() else {
            return deterministic_enhance_ingest_with_reason(
                raw_content,
                "memory intelligence LLM is not configured",
            );
        };
        match llm.enhance_ingest(raw_content).await {
            Ok(mut enhanced) => {
                enhanced.used_llm = true;
                global_intelligence_status().record_success();
                enhanced
            }
            Err(error) => {
                global_intelligence_status().record_failure(&error);
                deterministic_enhance_ingest_with_reason(raw_content, &error.to_string())
            }
        }
    }

    pub async fn enhance_search(&self, results: &[SearchResult]) -> EnhancedResults {
        if results.is_empty() || !self.mode.uses_llm() {
            return EnhancedResults::default();
        }
        let Some(llm) = self.llm.as_ref() else {
            return EnhancedResults {
                fallback_reason: Some("memory intelligence LLM is not configured".to_string()),
                ..EnhancedResults::default()
            };
        };
        match llm.enhance_search(results).await {
            Ok(mut enhanced) => {
                enhanced.used_llm = true;
                global_intelligence_status().record_success();
                enhanced
            }
            Err(error) => {
                global_intelligence_status().record_failure(&error);
                EnhancedResults {
                    fallback_reason: Some(error.to_string()),
                    ..EnhancedResults::default()
                }
            }
        }
    }

    pub async fn extract_kg_triples(&self, content: &str) -> Vec<Triple> {
        if !self.mode.uses_llm() {
            return Vec::new();
        }
        let Some(llm) = self.llm.as_ref() else {
            return Vec::new();
        };
        match llm.extract_kg_triples(content).await {
            Ok(triples) => {
                global_intelligence_status().record_success();
                triples
            }
            Err(error) => {
                global_intelligence_status().record_failure(&error);
                Vec::new()
            }
        }
    }

    pub async fn explain_contradiction(&self, a: &str, b: &str) -> String {
        if !self.mode.uses_llm() {
            return deterministic_explanation(a, b);
        }
        let Some(llm) = self.llm.as_ref() else {
            return deterministic_explanation(a, b);
        };
        match llm.explain_contradiction(a, b).await {
            Ok(explanation) => {
                global_intelligence_status().record_success();
                explanation
            }
            Err(error) => {
                global_intelligence_status().record_failure(&error);
                deterministic_explanation(a, b)
            }
        }
    }
}

pub struct DeterministicIntelligenceEnhancer;

#[async_trait]
impl IntelligenceEnhancer for DeterministicIntelligenceEnhancer {
    async fn enhance_ingest(&self, raw_content: &str) -> Result<EnhancedContent> {
        Ok(deterministic_enhance_ingest(raw_content))
    }

    async fn enhance_search(&self, _results: &[SearchResult]) -> Result<EnhancedResults> {
        Ok(EnhancedResults::default())
    }

    async fn extract_kg_triples(&self, _content: &str) -> Result<Vec<Triple>> {
        Ok(Vec::new())
    }

    async fn explain_contradiction(&self, a: &str, b: &str) -> Result<String> {
        Ok(deterministic_explanation(a, b))
    }
}

pub struct LlmIntelligenceEnhancer {
    router: LlmRouter,
}

impl LlmIntelligenceEnhancer {
    pub fn new(router: LlmRouter) -> Self {
        Self { router }
    }
}

#[async_trait]
impl IntelligenceEnhancer for LlmIntelligenceEnhancer {
    async fn enhance_ingest(&self, raw_content: &str) -> Result<EnhancedContent> {
        let response = self
            .router
            .chat_completion(&LlmRequest {
                messages: vec![
                    LlmMessage {
                        role: "system".to_string(),
                        content: "Return strict JSON with keys candidate_facts, tags, contradiction, correction. Every candidate_fact must be directly supported by the input text.".to_string(),
                    },
                    LlmMessage {
                        role: "user".to_string(),
                        content: raw_content.to_string(),
                    },
                ],
                model: None,
                temperature: Some(0.0),
                max_tokens: Some(512),
            }, None)
            .await?;
        let response = response.response;
        let decoded: LlmEnhancedContent = parse_llm_json(&response.content)?;
        validate_enhanced_content(raw_content, decoded)
    }

    async fn enhance_search(&self, results: &[SearchResult]) -> Result<EnhancedResults> {
        let payload = results
            .iter()
            .map(|result| {
                format!(
                    "drawer_id: {}\nsource_file: {}\ncontent: {}",
                    result.drawer_id, result.source_file, result.content
                )
            })
            .collect::<Vec<_>>()
            .join("\n---\n");
        let response = self
            .router
            .chat_completion(&LlmRequest {
                messages: vec![
                    LlmMessage {
                        role: "system".to_string(),
                        content: "Return strict JSON: {\"results\":[{\"drawer_id\":\"...\",\"summary\":\"...\",\"relevance_boost\":0.0,\"source_refs\":[\"drawer_id\"]}]}. Preserve source_refs.".to_string(),
                    },
                    LlmMessage {
                        role: "user".to_string(),
                        content: payload,
                    },
                ],
                model: None,
                temperature: Some(0.0),
                max_tokens: Some(1024),
            }, None)
            .await?;
        let response = response.response;
        let decoded: LlmEnhancedResults = parse_llm_json(&response.content)?;
        validate_search_results(results, decoded)
    }

    async fn extract_kg_triples(&self, content: &str) -> Result<Vec<Triple>> {
        let response = self
            .router
            .chat_completion(&LlmRequest {
                messages: vec![
                    LlmMessage {
                        role: "system".to_string(),
                        content: "Return strict JSON: {\"triples\":[{\"subject\":\"...\",\"predicate\":\"...\",\"object\":\"...\",\"confidence\":0.0}]}. Only extract triples directly supported by the input text.".to_string(),
                    },
                    LlmMessage {
                        role: "user".to_string(),
                        content: content.to_string(),
                    },
                ],
                model: None,
                temperature: Some(0.0),
                max_tokens: Some(512),
            }, None)
            .await?;
        let response = response.response;
        let decoded: LlmTriples = parse_llm_json(&response.content)?;
        validate_triples(content, decoded)
    }

    async fn explain_contradiction(&self, a: &str, b: &str) -> Result<String> {
        let response = self
            .router
            .chat_completion(&LlmRequest {
                messages: vec![
                    LlmMessage {
                        role: "system".to_string(),
                        content: "Return strict JSON: {\"explanation\":\"...\"}. Do not invent facts outside the two inputs.".to_string(),
                    },
                    LlmMessage {
                        role: "user".to_string(),
                        content: format!("A:\n{a}\n\nB:\n{b}"),
                    },
                ],
                model: None,
                temperature: Some(0.0),
                max_tokens: Some(256),
            }, None)
            .await?;
        let response = response.response;
        let decoded: LlmExplanation = parse_llm_json(&response.content)?;
        let explanation = decoded.explanation.trim();
        if explanation.is_empty() {
            return Err(IntelligenceError::InvalidSchema(
                "explanation must not be empty".to_string(),
            ));
        }
        Ok(explanation.to_string())
    }
}

#[derive(Debug, Deserialize)]
struct LlmEnhancedContent {
    #[serde(default)]
    candidate_facts: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    contradiction: bool,
    #[serde(default)]
    correction: bool,
}

#[derive(Debug, Deserialize)]
struct LlmEnhancedResults {
    #[serde(default)]
    results: Vec<LlmSearchItem>,
}

#[derive(Debug, Deserialize)]
struct LlmSearchItem {
    drawer_id: String,
    summary: String,
    #[serde(default)]
    relevance_boost: f32,
    #[serde(default)]
    source_refs: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct LlmTriples {
    #[serde(default)]
    triples: Vec<LlmTriple>,
}

#[derive(Debug, Deserialize)]
struct LlmTriple {
    subject: String,
    predicate: String,
    object: String,
    #[serde(default = "default_confidence")]
    confidence: f64,
}

#[derive(Debug, Deserialize)]
struct LlmExplanation {
    explanation: String,
}

fn default_confidence() -> f64 {
    0.5
}

fn deterministic_enhance_ingest(raw_content: &str) -> EnhancedContent {
    EnhancedContent {
        raw_content: raw_content.to_string(),
        candidate_facts: Vec::new(),
        tags: deterministic_tags(raw_content),
        used_llm: false,
        fallback_reason: None,
    }
}

fn deterministic_enhance_ingest_with_reason(raw_content: &str, reason: &str) -> EnhancedContent {
    let mut enhanced = deterministic_enhance_ingest(raw_content);
    enhanced.fallback_reason = Some(reason.to_string());
    enhanced
}

fn deterministic_tags(raw_content: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    raw_content
        .split_whitespace()
        .filter_map(|token| {
            let tag = token
                .trim_end_matches(|c: char| c.is_ascii_punctuation())
                .strip_prefix('#')?;
            normalize_tag(tag)
        })
        .filter(|tag| seen.insert(tag.clone()))
        .collect()
}

fn normalize_tag(tag: &str) -> Option<String> {
    let normalized = tag
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .flat_map(char::to_lowercase)
        .collect::<String>();
    (!normalized.is_empty()).then_some(normalized)
}

fn deterministic_explanation(a: &str, b: &str) -> String {
    format!(
        "Deterministic contradiction context: `{}` conflicts with `{}`.",
        one_line(a),
        one_line(b)
    )
}

fn one_line(value: &str) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_LEN: usize = 160;
    let mut chars = compact.chars();
    let truncated = chars.by_ref().take(MAX_LEN).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn parse_llm_json<T: for<'de> Deserialize<'de>>(content: &str) -> Result<T> {
    serde_json::from_str(json_payload(content))
        .map_err(|error| IntelligenceError::InvalidSchema(error.to_string()))
}

fn json_payload(content: &str) -> &str {
    let trimmed = content.trim();
    if !trimmed.starts_with("```") {
        return trimmed;
    }
    let Some((_, rest)) = trimmed.split_once('\n') else {
        return trimmed;
    };
    rest.trim()
        .strip_suffix("```")
        .map(str::trim)
        .unwrap_or_else(|| rest.trim())
}

fn validate_enhanced_content(
    raw_content: &str,
    decoded: LlmEnhancedContent,
) -> Result<EnhancedContent> {
    if decoded.contradiction && !decoded.correction {
        return Err(IntelligenceError::UnmarkedContradiction);
    }
    let mut candidate_facts = Vec::new();
    for fact in decoded.candidate_facts {
        let fact = fact.trim();
        if fact.is_empty() {
            continue;
        }
        if !raw_content.contains(fact) {
            return Err(IntelligenceError::UnsupportedFact(fact.to_string()));
        }
        candidate_facts.push(fact.to_string());
    }
    let tags = decoded
        .tags
        .iter()
        .filter_map(|tag| normalize_tag(tag.trim_start_matches('#')))
        .collect::<Vec<_>>();
    Ok(EnhancedContent {
        raw_content: raw_content.to_string(),
        candidate_facts,
        tags,
        used_llm: true,
        fallback_reason: None,
    })
}

fn validate_search_results(
    source_results: &[SearchResult],
    decoded: LlmEnhancedResults,
) -> Result<EnhancedResults> {
    let known_ids = source_results
        .iter()
        .map(|result| result.drawer_id.as_str())
        .collect::<HashSet<_>>();
    let mut items = Vec::new();
    for item in decoded.results {
        if !known_ids.contains(item.drawer_id.as_str()) {
            return Err(IntelligenceError::MissingSourceRef(item.drawer_id));
        }
        if !item.source_refs.iter().any(|id| id == &item.drawer_id) {
            return Err(IntelligenceError::MissingSourceRef(item.drawer_id));
        }
        let summary = item.summary.trim();
        if summary.is_empty() {
            return Err(IntelligenceError::InvalidSchema(
                "summary must not be empty".to_string(),
            ));
        }
        if !item.relevance_boost.is_finite() || !(-1.0..=1.0).contains(&item.relevance_boost) {
            return Err(IntelligenceError::InvalidSchema(
                "relevance_boost must be finite and in -1.0..=1.0".to_string(),
            ));
        }
        items.push(EnhancedSearchResult {
            drawer_id: item.drawer_id,
            summary: summary.to_string(),
            relevance_boost: item.relevance_boost,
        });
    }
    Ok(EnhancedResults {
        items,
        used_llm: true,
        fallback_reason: None,
    })
}

fn validate_triples(content: &str, decoded: LlmTriples) -> Result<Vec<Triple>> {
    let mut triples = Vec::new();
    for triple in decoded.triples {
        let subject = triple.subject.trim();
        let predicate = triple.predicate.trim();
        let object = triple.object.trim();
        if subject.is_empty() || predicate.is_empty() || object.is_empty() {
            return Err(IntelligenceError::InvalidSchema(
                "triple subject, predicate, and object must not be empty".to_string(),
            ));
        }
        if !content.contains(subject) || !content.contains(object) {
            return Err(IntelligenceError::UnsupportedFact(format!(
                "{subject} {predicate} {object}"
            )));
        }
        if !triple.confidence.is_finite() || !(0.0..=1.0).contains(&triple.confidence) {
            return Err(IntelligenceError::InvalidSchema(
                "triple confidence must be finite and in 0.0..=1.0".to_string(),
            ));
        }
        triples.push(Triple {
            id: build_triple_id(subject, predicate, object),
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
            valid_from: Some(current_timestamp()),
            valid_to: None,
            confidence: triple.confidence,
            source_drawer: None,
        });
    }
    Ok(triples)
}

fn current_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
