use serde::{Deserialize, Serialize};

use super::config::ConfigError;

/// Optional post-retrieval workflow that admits only quality-gated, exactly cited evidence.
///
/// The compile-time `adk-rust` Cargo feature and this runtime switch must both be enabled
/// before ADK-Rust transforms search results. Defaults keep the legacy search path unchanged.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct EvidenceWorkflowConfig {
    pub enabled: bool,
    pub engine: String,
    pub mode: String,
    pub input_top_k: usize,
    pub output_top_k: usize,
    pub max_evidence_tokens: usize,
    pub minimum_relevance: f32,
}

impl Default for EvidenceWorkflowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            engine: "adk-rust".to_string(),
            mode: "quality-gated".to_string(),
            input_top_k: 30,
            output_top_k: 8,
            max_evidence_tokens: 6_000,
            // Hybrid and BM25 retrieval expose reciprocal-rank-fusion scores.
            // With k=60, a rank-30 hit from one list still scores 1/90 ≈ 0.011.
            minimum_relevance: 0.01,
        }
    }
}

impl EvidenceWorkflowConfig {
    pub(super) fn validate(&self) -> Result<(), ConfigError> {
        if self.input_top_k == 0 {
            return Err(ConfigError::InvalidConfig(
                "evidence_workflow.input_top_k must be greater than 0".to_string(),
            ));
        }
        if self.output_top_k == 0 || self.output_top_k > self.input_top_k {
            return Err(ConfigError::InvalidConfig(
                "evidence_workflow.output_top_k must be in 1..=input_top_k".to_string(),
            ));
        }
        if self.max_evidence_tokens == 0 {
            return Err(ConfigError::InvalidConfig(
                "evidence_workflow.max_evidence_tokens must be greater than 0".to_string(),
            ));
        }
        if !self.minimum_relevance.is_finite() || !(0.0..=1.0).contains(&self.minimum_relevance) {
            return Err(ConfigError::InvalidConfig(
                "evidence_workflow.minimum_relevance must be a finite value in 0.0..=1.0"
                    .to_string(),
            ));
        }
        if self.enabled && (self.engine != "adk-rust" || self.mode != "quality-gated") {
            return Err(ConfigError::InvalidConfig(
                "enabled evidence_workflow requires engine = \"adk-rust\" and mode = \"quality-gated\""
                    .to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_workflow_defaults_off() {
        let config = EvidenceWorkflowConfig::default();

        assert!(!config.enabled);
        assert_eq!(config.engine, "adk-rust");
        assert_eq!(config.mode, "quality-gated");
        assert_eq!(config.minimum_relevance, 0.01);
    }

    #[test]
    fn evidence_workflow_rejects_invalid_quality_floor() {
        let config = EvidenceWorkflowConfig {
            minimum_relevance: f32::NAN,
            ..EvidenceWorkflowConfig::default()
        };

        assert!(config.validate().is_err());
    }
}
