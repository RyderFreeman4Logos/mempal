//! Optional ADK-Rust post-retrieval evidence workflow.
//!
//! The retrieval boundary is [`CitedHit`]: it captures source identity, the exact
//! retrieved bytes, and a content hash before any quality selection occurs. The
//! workflow may remove low-quality hits, but it never rewrites evidence text or
//! synthesizes citations.

#[cfg(feature = "adk-rust")]
use std::collections::{HashMap, HashSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::core::{
    config::{Config, EvidenceWorkflowConfig},
    types::SearchResult,
};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct SourceSpan {
    pub byte_start: usize,
    pub byte_end: usize,
}

/// Retrieval stage that produced `relevance_score`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceScoreType {
    Lexical,
    Vector,
    Fused,
    Rerank,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
pub struct CitedHit {
    pub hit_id: String,
    pub source_uri: String,
    pub source_scope: String,
    pub source_kind: String,
    pub content_hash: String,
    pub exact_quote: String,
    pub source_span: SourceSpan,
    pub score_type: EvidenceScoreType,
    pub relevance_score: f32,
}

impl CitedHit {
    pub fn new(
        hit_id: impl Into<String>,
        source_uri: impl Into<String>,
        source_scope: impl Into<String>,
        source_kind: impl Into<String>,
        exact_quote: impl Into<String>,
        score_type: EvidenceScoreType,
        relevance_score: f32,
    ) -> Self {
        let exact_quote = exact_quote.into();
        Self {
            hit_id: hit_id.into(),
            source_uri: source_uri.into(),
            source_scope: source_scope.into(),
            source_kind: source_kind.into(),
            content_hash: content_hash(&exact_quote),
            source_span: SourceSpan {
                byte_start: 0,
                byte_end: exact_quote.len(),
            },
            exact_quote,
            score_type,
            relevance_score,
        }
    }

    pub fn from_search_result(result: &SearchResult, score_type: EvidenceScoreType) -> Self {
        let source_uri = if result.source_file.trim().is_empty() {
            format!("mempal://drawer/{}", result.drawer_id)
        } else {
            result.source_file.clone()
        };
        Self::new(
            result.drawer_id.clone(),
            source_uri,
            result.source.as_str(),
            result.source_type.as_str(),
            result.content.clone(),
            score_type,
            result.similarity,
        )
    }

    fn into_item(self) -> EvidenceItem {
        EvidenceItem {
            hit_id: self.hit_id,
            source_uri: self.source_uri,
            source_scope: self.source_scope,
            source_kind: self.source_kind,
            content_hash: self.content_hash,
            exact_quote: self.exact_quote,
            source_span: self.source_span,
            score_type: self.score_type,
            relevance_score: self.relevance_score,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
pub struct EvidenceItem {
    /// Stable drawer identity from the original retrieval result.
    pub hit_id: String,
    /// Original source path, or a stable `mempal://drawer/<id>` URI when absent.
    pub source_uri: String,
    /// Retrieval scope (`project`, `global`, or `tunnel_cross_project`).
    pub source_scope: String,
    /// Original drawer source type.
    pub source_kind: String,
    /// BLAKE3 hash of `exact_quote` at the retrieval boundary.
    pub content_hash: String,
    /// Verbatim retrieved evidence; the workflow never paraphrases this text.
    pub exact_quote: String,
    /// Byte offsets within the retrieved drawer content.
    pub source_span: SourceSpan,
    /// Retrieval stage that produced `relevance_score`.
    pub score_type: EvidenceScoreType,
    pub relevance_score: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceRoute {
    QualityGatedEvidence,
    RawBoundedHits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceFallbackReason {
    Disabled,
    FeatureUnavailable,
    NoCandidates,
    BelowQualityThreshold,
    CitationVerificationFailed,
    WorkflowError,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
pub struct EvidenceMetrics {
    pub retrieved_hits: usize,
    pub selected_hits: usize,
    pub raw_evidence_tokens: usize,
    pub selected_evidence_tokens: usize,
    pub compression_ratio: f32,
    pub minimum_relevance: f32,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
pub struct EvidencePack {
    pub route: EvidenceRoute,
    pub items: Vec<EvidenceItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<EvidenceFallbackReason>,
    pub metrics: EvidenceMetrics,
}

/// Additive MCP search adapter. The ordinary result vector remains untouched;
/// only an explicit request constructs a provenance-rich evidence projection.
pub async fn for_search(
    requested: Option<bool>,
    config: &Config,
    results: &[SearchResult],
    score_type: EvidenceScoreType,
) -> Option<EvidencePack> {
    if !requested.unwrap_or(false) {
        return None;
    }
    let cited_hits = results
        .iter()
        .map(|result| CitedHit::from_search_result(result, score_type))
        .collect();
    Some(run_optional_evidence_workflow(&config.evidence_workflow, cited_hits).await)
}

/// Execute the workflow when the ADK-Rust feature is present, or return the
/// original bounded cited hits with an explicit fallback reason otherwise.
pub async fn run_optional_evidence_workflow(
    config: &EvidenceWorkflowConfig,
    raw_hits: Vec<CitedHit>,
) -> EvidencePack {
    #[cfg(feature = "adk-rust")]
    {
        run_evidence_workflow(config, raw_hits).await
    }
    #[cfg(not(feature = "adk-rust"))]
    {
        let reason = if config.enabled {
            EvidenceFallbackReason::FeatureUnavailable
        } else {
            EvidenceFallbackReason::Disabled
        };
        raw_pack(config, raw_hits, reason)
    }
}

#[cfg(feature = "adk-rust")]
pub async fn run_evidence_workflow(
    config: &EvidenceWorkflowConfig,
    raw_hits: Vec<CitedHit>,
) -> EvidencePack {
    use adk_graph::{ExecutionConfig, State};

    let fallback_hits = raw_hits.clone();
    let serialized_config = match serde_json::to_value(config) {
        Ok(value) => value,
        Err(_) => {
            return raw_pack(config, fallback_hits, EvidenceFallbackReason::WorkflowError);
        }
    };
    let serialized_hits = match serde_json::to_value(raw_hits) {
        Ok(value) => value,
        Err(_) => {
            return raw_pack(config, fallback_hits, EvidenceFallbackReason::WorkflowError);
        }
    };
    let mut input = State::new();
    input.insert("config".to_string(), serialized_config);
    input.insert("raw_hits".to_string(), serialized_hits);

    let graph = match build_graph() {
        Ok(graph) => graph,
        Err(_) => {
            return raw_pack(config, fallback_hits, EvidenceFallbackReason::WorkflowError);
        }
    };
    let result = graph
        .invoke(
            input,
            ExecutionConfig::new("mempal-quality-gated-evidence").with_recursion_limit(8),
        )
        .await;
    result
        .ok()
        .and_then(|state| state.get("pack").cloned())
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_else(|| raw_pack(config, fallback_hits, EvidenceFallbackReason::WorkflowError))
}

#[cfg(feature = "adk-rust")]
fn build_graph() -> adk_graph::Result<adk_graph::CompiledGraph> {
    use adk_graph::{END, GraphError, NodeOutput, START, StateGraph};
    use serde_json::json;

    StateGraph::with_channels(&[
        "config",
        "raw_hits",
        "selected_items",
        "route",
        "fallback_reason",
        "pack",
    ])
    .add_node_fn("eligibility", |ctx| async move {
        let config: EvidenceWorkflowConfig = ctx.get_as("config").ok_or_else(|| {
            GraphError::SerializationError("missing evidence workflow config".to_string())
        })?;
        if config.enabled {
            Ok(NodeOutput::new().with_update("route", json!("select")))
        } else {
            Ok(NodeOutput::new()
                .with_update("route", json!("fallback"))
                .with_update("fallback_reason", json!(EvidenceFallbackReason::Disabled)))
        }
    })
    .add_node_fn("select", |ctx| async move {
        let config: EvidenceWorkflowConfig = ctx.get_as("config").ok_or_else(|| {
            GraphError::SerializationError("missing evidence workflow config".to_string())
        })?;
        let raw_hits: Vec<CitedHit> = ctx.get_as("raw_hits").ok_or_else(|| {
            GraphError::SerializationError("missing cited retrieval hits".to_string())
        })?;
        let selected = select_quality_evidence(&config, &raw_hits);
        Ok(NodeOutput::new().with_update("selected_items", serde_json::to_value(selected)?))
    })
    .add_node_fn("verify", |ctx| async move {
        let config: EvidenceWorkflowConfig = ctx.get_as("config").ok_or_else(|| {
            GraphError::SerializationError("missing evidence workflow config".to_string())
        })?;
        let raw_hits: Vec<CitedHit> = ctx.get_as("raw_hits").ok_or_else(|| {
            GraphError::SerializationError("missing cited retrieval hits".to_string())
        })?;
        let selected: Vec<EvidenceItem> = ctx.get_as("selected_items").unwrap_or_default();
        let (route, fallback_reason) = if raw_hits.is_empty() {
            ("fallback", Some(EvidenceFallbackReason::NoCandidates))
        } else if selected.is_empty() {
            (
                "fallback",
                Some(EvidenceFallbackReason::BelowQualityThreshold),
            )
        } else if verify_selected_evidence(&config, &raw_hits, &selected) {
            ("quality", None)
        } else {
            (
                "fallback",
                Some(EvidenceFallbackReason::CitationVerificationFailed),
            )
        };
        let mut output = NodeOutput::new().with_update("route", json!(route));
        if let Some(reason) = fallback_reason {
            output = output.with_update("fallback_reason", json!(reason));
        }
        Ok(output)
    })
    .add_node_fn("quality_output", |ctx| async move {
        let config: EvidenceWorkflowConfig = ctx.get_as("config").ok_or_else(|| {
            GraphError::SerializationError("missing evidence workflow config".to_string())
        })?;
        let raw_hits: Vec<CitedHit> = ctx.get_as("raw_hits").ok_or_else(|| {
            GraphError::SerializationError("missing cited retrieval hits".to_string())
        })?;
        let selected: Vec<EvidenceItem> = ctx.get_as("selected_items").unwrap_or_default();
        let pack = build_pack(
            &config,
            &raw_hits,
            selected,
            EvidenceRoute::QualityGatedEvidence,
            None,
        );
        Ok(NodeOutput::new().with_update("pack", serde_json::to_value(pack)?))
    })
    .add_node_fn("raw_fallback", |ctx| async move {
        let config: EvidenceWorkflowConfig = ctx.get_as("config").ok_or_else(|| {
            GraphError::SerializationError("missing evidence workflow config".to_string())
        })?;
        let raw_hits: Vec<CitedHit> = ctx.get_as("raw_hits").ok_or_else(|| {
            GraphError::SerializationError("missing cited retrieval hits".to_string())
        })?;
        let reason: EvidenceFallbackReason = ctx
            .get_as("fallback_reason")
            .unwrap_or(EvidenceFallbackReason::WorkflowError);
        let pack = raw_pack(&config, raw_hits, reason);
        Ok(NodeOutput::new().with_update("pack", serde_json::to_value(pack)?))
    })
    .add_edge(START, "eligibility")
    .add_conditional_edges(
        "eligibility",
        route_from_state,
        [("select", "select"), ("fallback", "raw_fallback")],
    )
    .add_edge("select", "verify")
    .add_conditional_edges(
        "verify",
        route_from_state,
        [("quality", "quality_output"), ("fallback", "raw_fallback")],
    )
    .add_edge("quality_output", END)
    .add_edge("raw_fallback", END)
    .compile()
}

#[cfg(feature = "adk-rust")]
fn route_from_state(state: &adk_graph::State) -> String {
    state
        .get("route")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("fallback")
        .to_string()
}

#[cfg(feature = "adk-rust")]
fn select_quality_evidence(
    config: &EvidenceWorkflowConfig,
    raw_hits: &[CitedHit],
) -> Vec<EvidenceItem> {
    let mut selected = Vec::new();
    let mut used_tokens: usize = 0;
    let mut seen_ids = HashSet::new();
    for hit in raw_hits.iter().take(config.input_top_k) {
        if selected.len() >= config.output_top_k
            || !hit.relevance_score.is_finite()
            || hit.relevance_score < config.minimum_relevance
            || !citation_is_valid(hit)
            || !seen_ids.insert(hit.hit_id.as_str())
        {
            continue;
        }
        let tokens = crate::embed::estimate_tokens(&hit.exact_quote);
        if used_tokens.saturating_add(tokens) > config.max_evidence_tokens {
            continue;
        }
        used_tokens += tokens;
        selected.push(hit.clone().into_item());
    }
    selected
}

#[cfg(feature = "adk-rust")]
fn verify_selected_evidence(
    config: &EvidenceWorkflowConfig,
    raw_hits: &[CitedHit],
    selected: &[EvidenceItem],
) -> bool {
    let mut raw_by_id = HashMap::new();
    for hit in raw_hits.iter().take(config.input_top_k) {
        if raw_by_id.insert(hit.hit_id.as_str(), hit).is_some() {
            return false;
        }
    }

    let mut seen_ids = HashSet::new();
    selected.len() <= config.output_top_k
        && selected.iter().all(|item| {
            let Some(raw) = raw_by_id.get(item.hit_id.as_str()) else {
                return false;
            };
            item.relevance_score.is_finite()
                && item.relevance_score >= config.minimum_relevance
                && item.source_span.byte_start == 0
                && item.source_span.byte_end == item.exact_quote.len()
                && !item.hit_id.trim().is_empty()
                && !item.source_uri.trim().is_empty()
                && !item.source_scope.trim().is_empty()
                && !item.source_kind.trim().is_empty()
                && item.content_hash == content_hash(&item.exact_quote)
                && item.source_uri == raw.source_uri
                && item.source_scope == raw.source_scope
                && item.source_kind == raw.source_kind
                && item.content_hash == raw.content_hash
                && item.exact_quote == raw.exact_quote
                && item.source_span == raw.source_span
                && item.score_type == raw.score_type
                && item.relevance_score.to_bits() == raw.relevance_score.to_bits()
                && seen_ids.insert(item.hit_id.as_str())
        })
        && selected
            .iter()
            .map(|item| crate::embed::estimate_tokens(&item.exact_quote))
            .sum::<usize>()
            <= config.max_evidence_tokens
}

fn citation_is_valid(hit: &CitedHit) -> bool {
    !hit.hit_id.trim().is_empty()
        && !hit.source_uri.trim().is_empty()
        && !hit.source_scope.trim().is_empty()
        && !hit.source_kind.trim().is_empty()
        && !hit.exact_quote.is_empty()
        && hit.source_span.byte_start == 0
        && hit.source_span.byte_end == hit.exact_quote.len()
        && hit.content_hash == content_hash(&hit.exact_quote)
}

fn raw_pack(
    config: &EvidenceWorkflowConfig,
    raw_hits: Vec<CitedHit>,
    reason: EvidenceFallbackReason,
) -> EvidencePack {
    let bounded_hits: Vec<CitedHit> = raw_hits.into_iter().take(config.input_top_k).collect();
    let items = bounded_hits
        .iter()
        .filter(|hit| citation_is_valid(hit))
        .cloned()
        .map(CitedHit::into_item)
        .collect();
    build_pack(
        config,
        &bounded_hits,
        items,
        EvidenceRoute::RawBoundedHits,
        Some(reason),
    )
}

fn build_pack(
    config: &EvidenceWorkflowConfig,
    raw_hits: &[CitedHit],
    items: Vec<EvidenceItem>,
    route: EvidenceRoute,
    fallback_reason: Option<EvidenceFallbackReason>,
) -> EvidencePack {
    let raw_evidence_tokens = raw_hits
        .iter()
        .map(|hit| crate::embed::estimate_tokens(&hit.exact_quote))
        .sum();
    let selected_evidence_tokens = items
        .iter()
        .map(|item| crate::embed::estimate_tokens(&item.exact_quote))
        .sum();
    let compression_ratio = if raw_evidence_tokens == 0 {
        0.0
    } else {
        selected_evidence_tokens as f32 / raw_evidence_tokens as f32
    };
    EvidencePack {
        route,
        metrics: EvidenceMetrics {
            retrieved_hits: raw_hits.len(),
            selected_hits: items.len(),
            raw_evidence_tokens,
            selected_evidence_tokens,
            compression_ratio,
            minimum_relevance: config.minimum_relevance,
        },
        items,
        fallback_reason,
    }
}

fn content_hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

#[cfg(all(test, feature = "adk-rust"))]
mod adk_tests {
    use super::*;

    #[test]
    fn selected_evidence_must_match_raw_hit() {
        let config = EvidenceWorkflowConfig {
            enabled: true,
            ..EvidenceWorkflowConfig::default()
        };
        let raw = CitedHit::new(
            "drawer-citation",
            "file:///memory/original.jsonl",
            "project",
            "agent_inference",
            "The selected citation must remain byte-for-byte authoritative.",
            EvidenceScoreType::Fused,
            0.02,
        );
        let raw_hits = vec![raw.clone()];
        let selected = raw.clone().into_item();

        assert!(verify_selected_evidence(
            &config,
            &raw_hits,
            std::slice::from_ref(&selected),
        ));

        let mut mismatches = Vec::new();

        let mut changed_uri = selected.clone();
        changed_uri.source_uri = "file:///memory/other.jsonl".to_string();
        mismatches.push(changed_uri);

        let mut changed_scope = selected.clone();
        changed_scope.source_scope = "global".to_string();
        mismatches.push(changed_scope);

        let mut changed_kind = selected.clone();
        changed_kind.source_kind = "user_explicit".to_string();
        mismatches.push(changed_kind);

        let mut changed_hash = selected.clone();
        changed_hash.content_hash = "0".repeat(64);
        mismatches.push(changed_hash);

        let mut changed_quote = selected.clone();
        changed_quote.exact_quote = "A different but internally valid quote.".to_string();
        changed_quote.content_hash = content_hash(&changed_quote.exact_quote);
        changed_quote.source_span.byte_end = changed_quote.exact_quote.len();
        mismatches.push(changed_quote);

        let mut changed_span = selected.clone();
        changed_span.source_span.byte_end -= 1;
        mismatches.push(changed_span);

        let mut changed_id = selected;
        changed_id.hit_id = "drawer-missing".to_string();
        mismatches.push(changed_id);

        for mismatch in mismatches {
            assert!(!verify_selected_evidence(
                &config,
                &raw_hits,
                std::slice::from_ref(&mismatch),
            ));
        }
    }
}

#[cfg(all(test, not(feature = "adk-rust")))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enabled_runtime_config_falls_back_when_feature_is_unavailable() {
        let config = EvidenceWorkflowConfig {
            enabled: true,
            ..EvidenceWorkflowConfig::default()
        };
        let hit = CitedHit::new(
            "drawer-feature-off",
            "file:///memory/feature-off.jsonl",
            "project",
            "agent_inference",
            "Compile-time feature gating preserves this exact cited hit.",
            EvidenceScoreType::Vector,
            0.99,
        );

        let pack = run_optional_evidence_workflow(&config, vec![hit.clone()]).await;

        assert_eq!(pack.route, EvidenceRoute::RawBoundedHits);
        assert_eq!(
            pack.fallback_reason,
            Some(EvidenceFallbackReason::FeatureUnavailable)
        );
        assert_eq!(pack.items[0].hit_id, hit.hit_id);
        assert_eq!(pack.items[0].content_hash, hit.content_hash);
    }
}
