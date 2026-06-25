use crate::core::config::NoveltyConfig;
use crate::core::db::{Database, read_fork_ext_version};
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
    pub project_id: Option<String>,
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
    if !config.enabled || config.top_k_candidates == 0 || candidate.wing == "agent-diary" {
        return NoveltyDecision {
            should_audit: false,
            ..NoveltyDecision::insert()
        };
    }

    let fork_ext_version = match read_fork_ext_version(db.conn()) {
        Ok(version) => version,
        Err(error) => {
            tracing::warn!(
                ?error,
                "failed to read fork_ext_version for novelty; fail-open insert"
            );
            return NoveltyDecision::insert();
        }
    };
    if !novelty_vector_search_has_pushed_project_scope(candidate, config, fork_ext_version) {
        if candidate.project_id.is_some() {
            tracing::warn!(
                fork_ext_version,
                wing = %candidate.wing,
                room = ?candidate.room,
                "shared palace detected, project isolation unavailable; novelty disabled"
            );
            return NoveltyDecision {
                should_audit: false,
                ..NoveltyDecision::insert()
            };
        }

        return evaluate_no_project_novelty(db, candidate, vector, config);
    }

    let (wing, room) = novelty_scope(candidate, config);
    let results = match db.novelty_candidates(
        vector,
        wing.as_deref(),
        room.as_deref(),
        candidate.project_id.as_deref(),
        config.top_k_candidates,
    ) {
        Ok(results) => results,
        Err(error) => {
            tracing::warn!(?error, "novelty search failed; fail-open insert");
            return NoveltyDecision::insert();
        }
    };

    novelty_decision_from_results(results, config)
}

pub fn novelty_vector_search_has_pushed_project_scope(
    candidate: &NoveltyCandidate,
    config: &NoveltyConfig,
    fork_ext_version: u32,
) -> bool {
    config.enabled
        && config.top_k_candidates > 0
        && candidate.wing != "agent-diary"
        && candidate.project_id.is_some()
        && fork_ext_version >= 5
}

fn evaluate_no_project_novelty(
    db: &Database,
    candidate: &NoveltyCandidate,
    vector: &[f32],
    config: &NoveltyConfig,
) -> NoveltyDecision {
    let (wing, room) = novelty_scope(candidate, config);
    let candidate_count =
        match db.count_novelty_candidate_drawers(wing.as_deref(), room.as_deref(), None) {
            Ok(count) => count,
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "no-project novelty candidate count failed; fail-open insert"
                );
                return NoveltyDecision::insert();
            }
        };

    if candidate_count > crate::search::EXACT_VECTOR_CANDIDATE_LIMIT {
        tracing::warn!(
            wing = %candidate.wing,
            room = ?candidate.room,
            candidate_count,
            limit = crate::search::EXACT_VECTOR_CANDIDATE_LIMIT,
            "no-project novelty disabled because scoped candidate count exceeds exact vector limit"
        );
        return NoveltyDecision {
            should_audit: false,
            ..NoveltyDecision::insert()
        };
    }

    tracing::warn!(
        wing = %candidate.wing,
        room = ?candidate.room,
        candidate_count,
        "no-project novelty using bounded exact vector scan because sqlite-vec KNN has no pushed-down project scope"
    );
    let results = match db.novelty_candidates_exact(
        vector,
        wing.as_deref(),
        room.as_deref(),
        None,
        config.top_k_candidates,
    ) {
        Ok(results) => results,
        Err(error) => {
            tracing::warn!(
                ?error,
                "bounded no-project novelty search failed; fail-open insert"
            );
            return NoveltyDecision::insert();
        }
    };

    novelty_decision_from_results(results, config)
}

fn novelty_decision_from_results(
    results: Vec<(String, f32)>,
    config: &NoveltyConfig,
) -> NoveltyDecision {
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
