use crate::core::config::NoveltyConfig;
use crate::core::db::Database;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum NoveltyAction {
    Insert,
    Merge,
    Drop,
}

impl NoveltyAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Merge => "merge",
            Self::Drop => "drop",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NoveltyCandidate {
    pub wing: String,
    pub room: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NoveltyDecision {
    pub action: NoveltyAction,
    pub near_drawer_id: Option<String>,
    pub cosine: Option<f32>,
    pub should_audit: bool,
    pub audit_decision: Option<&'static str>,
}

impl NoveltyDecision {
    pub fn insert() -> Self {
        Self {
            action: NoveltyAction::Insert,
            near_drawer_id: None,
            cosine: None,
            should_audit: true,
            audit_decision: None,
        }
    }
}

pub fn evaluate(
    db: &Database,
    candidate: &NoveltyCandidate,
    vector: &[f32],
    config: &NoveltyConfig,
) -> NoveltyDecision {
    if !config.enabled || candidate.wing == "agent-diary" {
        return NoveltyDecision {
            should_audit: false,
            ..NoveltyDecision::insert()
        };
    }

    let (wing, room) = novelty_scope(candidate, config);
    let results = match db.novelty_candidates(
        vector,
        wing.as_deref(),
        room.as_deref(),
        config.top_k_candidates,
    ) {
        Ok(results) => results,
        Err(error) => {
            tracing::warn!(?error, "novelty search failed; fail-open insert");
            return NoveltyDecision::insert();
        }
    };

    let Some(top) = results.first() else {
        return NoveltyDecision::insert();
    };

    if top.1 >= config.duplicate_threshold {
        return NoveltyDecision {
            action: NoveltyAction::Drop,
            near_drawer_id: Some(top.0.clone()),
            cosine: Some(top.1),
            should_audit: true,
            audit_decision: None,
        };
    }
    if top.1 >= config.merge_threshold {
        return NoveltyDecision {
            action: NoveltyAction::Merge,
            near_drawer_id: Some(top.0.clone()),
            cosine: Some(top.1),
            should_audit: true,
            audit_decision: None,
        };
    }

    NoveltyDecision::insert()
}

fn novelty_scope(
    candidate: &NoveltyCandidate,
    config: &NoveltyConfig,
) -> (Option<String>, Option<String>) {
    match config.wing_scope.as_str() {
        "same_room" => (Some(candidate.wing.clone()), candidate.room.clone()),
        "global" => (None, None),
        _ => (Some(candidate.wing.clone()), None),
    }
}
