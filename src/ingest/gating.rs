use std::sync::Arc;

use crate::core::config::{
    Config, EmbeddingClassifierConfig, GatingRuleConfig, IngestGatingConfig,
};
use crate::embed::{EmbedError, Embedder, EmbedderFactory};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::OnceCell;

#[derive(Debug, Clone)]
pub struct IngestCandidate {
    pub content: String,
    pub tool_name: Option<String>,
    pub exit_code: Option<i32>,
}

impl IngestCandidate {
    pub fn content_bytes(&self) -> usize {
        self.content.len()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GatingDecision {
    pub decision: String,
    pub tier: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_pattern: Option<String>,
}

impl GatingDecision {
    pub fn accepted(tier: u8, label: impl Into<Option<String>>, score: Option<f32>) -> Self {
        Self {
            decision: "accepted".to_string(),
            tier,
            label: label.into(),
            score,
            matched_pattern: None,
        }
    }

    pub fn rejected(tier: u8, matched_pattern: Option<String>, score: Option<f32>) -> Self {
        Self {
            decision: "rejected".to_string(),
            tier,
            label: None,
            score,
            matched_pattern,
        }
    }

    pub fn is_rejected(&self) -> bool {
        self.decision == "rejected"
    }
}

#[derive(Debug, Clone)]
pub struct Tier2Outcome {
    pub decision: GatingDecision,
    pub vector: Option<Vec<f32>>,
}

#[derive(Debug, Clone)]
struct EmbeddedPrototype {
    label: String,
    vector: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct PrototypeClassifier {
    prototypes: Vec<EmbeddedPrototype>,
}

impl PrototypeClassifier {
    pub fn decide(&self, vector: &[f32], threshold: f32) -> GatingDecision {
        let (label, score) = self.classify(vector);
        if score >= threshold {
            GatingDecision::accepted(2, label.map(ToOwned::to_owned), Some(score))
        } else {
            GatingDecision::rejected(2, None, Some(score))
        }
    }

    fn classify(&self, vector: &[f32]) -> (Option<&str>, f32) {
        let mut best_label = None;
        let mut best_score = f32::NEG_INFINITY;
        for prototype in &self.prototypes {
            let score = cosine_similarity(vector, &prototype.vector);
            if score > best_score {
                best_score = score;
                best_label = Some(prototype.label.as_str());
            }
        }
        if best_score.is_finite() {
            (best_label, best_score)
        } else {
            (best_label, 0.0)
        }
    }
}

#[derive(Debug, Error)]
pub enum GatingInitError {
    #[error("failed to build gating embedder")]
    BuildEmbedder(#[source] EmbedError),
    #[error("gating prototype init failed: label='{label}', reason='{source}'")]
    PrototypeEmbed {
        label: String,
        #[source]
        source: EmbedError,
    },
    #[error(
        "gating prototype init failed: label='{label}', dimension mismatch expected {expected} got {actual}"
    )]
    PrototypeDimMismatch {
        label: String,
        expected: usize,
        actual: usize,
    },
}

pub struct GatingRuntime {
    config: Config,
    factory: Arc<dyn EmbedderFactory>,
    classifier: OnceCell<Option<PrototypeClassifier>>,
}

impl GatingRuntime {
    pub fn new(config: Config, factory: Arc<dyn EmbedderFactory>) -> Self {
        Self {
            config,
            factory,
            classifier: OnceCell::new(),
        }
    }

    pub async fn initialize(&self) -> Result<(), GatingInitError> {
        let _ = self.classifier().await?;
        Ok(())
    }

    pub async fn classifier(&self) -> Result<Option<PrototypeClassifier>, GatingInitError> {
        let classifier = self
            .classifier
            .get_or_try_init(|| async {
                let embedder = self
                    .factory
                    .build()
                    .await
                    .map_err(GatingInitError::BuildEmbedder)?;
                compile_classifier(
                    &self.config.ingest_gating.embedding_classifier,
                    embedder.as_ref(),
                )
                .await
            })
            .await?;
        Ok(classifier.clone())
    }
}

pub fn evaluate_tier1(
    candidate: &IngestCandidate,
    gating: &IngestGatingConfig,
) -> Option<GatingDecision> {
    if !gating.enabled {
        return Some(GatingDecision::accepted(
            0,
            Some("gating_disabled".to_string()),
            None,
        ));
    }

    for rule in &gating.rules {
        let matched_pattern = match_rule(candidate, rule)?;
        match normalize_rule_action(rule.action.as_str()) {
            RuleAction::Reject => {
                return Some(GatingDecision::rejected(
                    1,
                    Some(matched_pattern.to_string()),
                    None,
                ));
            }
            RuleAction::Accept => {
                let mut decision =
                    GatingDecision::accepted(1, Some("rule_accept".to_string()), None);
                decision.matched_pattern = Some(matched_pattern.to_string());
                return Some(decision);
            }
            RuleAction::Continue => continue,
        }
    }

    None
}

pub fn tier2_enabled(gating: &IngestGatingConfig) -> bool {
    gating.enabled
        && gating.embedding_classifier.enabled
        && !gating.embedding_classifier.prototypes.is_empty()
}

pub async fn compile_classifier_from_embedder<E: Embedder + ?Sized>(
    embedder: &E,
    gating: &IngestGatingConfig,
) -> Result<Option<PrototypeClassifier>, GatingInitError> {
    compile_classifier(&gating.embedding_classifier, embedder).await
}

pub async fn evaluate_tier2<E: Embedder + ?Sized>(
    candidate: &IngestCandidate,
    classifier: &PrototypeClassifier,
    embedder: &E,
    threshold: f32,
) -> Tier2Outcome {
    match embedder.embed(&[candidate.content.as_str()]).await {
        Ok(vectors) => {
            let vector = vectors.into_iter().next();
            match vector {
                Some(vector) => {
                    let decision = classifier.decide(&vector, threshold);
                    Tier2Outcome {
                        decision,
                        vector: Some(vector),
                    }
                }
                None => Tier2Outcome {
                    decision: GatingDecision::accepted(0, Some("embedder_error".to_string()), None),
                    vector: None,
                },
            }
        }
        Err(error) => {
            tracing::warn!(
                ?error,
                "gating tier-2 candidate embed failed; fail-open keep"
            );
            Tier2Outcome {
                decision: GatingDecision::accepted(0, Some("embedder_error".to_string()), None),
                vector: None,
            }
        }
    }
}

async fn compile_classifier<E: Embedder + ?Sized>(
    config: &EmbeddingClassifierConfig,
    embedder: &E,
) -> Result<Option<PrototypeClassifier>, GatingInitError> {
    if !config.enabled || config.prototypes.is_empty() {
        return Ok(None);
    }

    let mut prototypes = Vec::with_capacity(config.prototypes.len());
    for label in &config.prototypes {
        let vectors = embedder.embed(&[label.as_str()]).await.map_err(|source| {
            GatingInitError::PrototypeEmbed {
                label: label.clone(),
                source,
            }
        })?;
        if let Some(vector) = vectors.into_iter().next() {
            let actual = vector.len();
            let expected = embedder.dimensions();
            if actual != expected {
                return Err(GatingInitError::PrototypeDimMismatch {
                    label: label.clone(),
                    expected,
                    actual,
                });
            }
            prototypes.push(EmbeddedPrototype {
                label: label.clone(),
                vector,
            });
        }
    }

    Ok(Some(PrototypeClassifier { prototypes }))
}

fn match_rule<'a>(candidate: &IngestCandidate, rule: &'a GatingRuleConfig) -> Option<&'a str> {
    let mut matched = None;

    if let Some(tool) = &rule.tool {
        if candidate.tool_name.as_deref() != Some(tool.as_str()) {
            return None;
        }
        matched = Some("tool");
    }

    if let Some(tool_in) = &rule.tool_in {
        let tool_name = candidate.tool_name.as_deref()?;
        if !tool_in.iter().any(|item| item == tool_name) {
            return None;
        }
        matched = Some("tool_in");
    }

    if let Some(content_bytes_lt) = rule.content_bytes_lt {
        if candidate.content_bytes() >= content_bytes_lt {
            return None;
        }
        matched = Some("content_bytes_lt");
    }

    if let Some(content_bytes_gt) = rule.content_bytes_gt {
        if candidate.content_bytes() <= content_bytes_gt {
            return None;
        }
        matched = Some("content_bytes_gt");
    }

    if let Some(exit_code_eq) = rule.exit_code_eq {
        if candidate.exit_code != Some(exit_code_eq) {
            return None;
        }
        matched = Some("exit_code_eq");
    }

    matched
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }

    let mut dot = 0.0_f32;
    let mut left_norm = 0.0_f32;
    let mut right_norm = 0.0_f32;
    for (lhs, rhs) in left.iter().zip(right.iter()) {
        dot += lhs * rhs;
        left_norm += lhs * lhs;
        right_norm += rhs * rhs;
    }

    if left_norm == 0.0 || right_norm == 0.0 {
        return 0.0;
    }

    dot / (left_norm.sqrt() * right_norm.sqrt())
}

enum RuleAction {
    Reject,
    Accept,
    Continue,
}

fn normalize_rule_action(action: &str) -> RuleAction {
    match action {
        "reject" | "skip" => RuleAction::Reject,
        "accept" | "keep" => RuleAction::Accept,
        _ => RuleAction::Continue,
    }
}
