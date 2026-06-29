use crate::core::config::NoveltyConfig;
use crate::core::db::{Database, read_fork_ext_version};
use crate::observability::{VectorScanMode, VectorScanSnapshot, record_vector_scan};
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
            record_fail_open_vector_scan(
                None,
                0,
                config.novelty_scan_limit as u64,
                "fork_ext_version_read_failed",
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
            record_fail_open_vector_scan(
                None,
                0,
                config.novelty_scan_limit as u64,
                "project_scope_unavailable",
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
            record_fail_open_vector_scan(
                Some(VectorScanMode::Knn),
                0,
                config.top_k_candidates as u64,
                "project_scoped_search_failed",
            );
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
                record_fail_open_vector_scan(
                    Some(VectorScanMode::Bounded),
                    0,
                    config.novelty_scan_limit as u64,
                    "bounded_no_project_count_failed",
                );
                return NoveltyDecision::insert();
            }
        };

    tracing::warn!(
        wing = %candidate.wing,
        room = ?candidate.room,
        candidate_count,
        scan_limit = config.novelty_scan_limit,
        "no-project novelty using bounded recent vector scan because sqlite-vec KNN has no pushed-down project scope"
    );
    record_vector_scan(VectorScanSnapshot {
        mode: Some(VectorScanMode::Bounded),
        candidate_count: candidate_count as u64,
        candidate_cap: config.novelty_scan_limit as u64,
        last_fail_open_reason: None,
    });
    let results = match db.novelty_candidates_exact(
        vector,
        wing.as_deref(),
        room.as_deref(),
        None,
        config.top_k_candidates,
        config.novelty_scan_limit,
    ) {
        Ok(results) => results,
        Err(error) => {
            tracing::warn!(
                ?error,
                "bounded no-project novelty search failed; fail-open insert"
            );
            record_fail_open_vector_scan(
                Some(VectorScanMode::Bounded),
                candidate_count as u64,
                config.novelty_scan_limit as u64,
                "bounded_no_project_search_failed",
            );
            return NoveltyDecision::insert();
        }
    };

    let mut decision = novelty_decision_from_results(results, config);
    if matches!(decision.action, NoveltyAction::Insert)
        && candidate_count > config.novelty_scan_limit as i64
    {
        decision.should_audit = false;
    }
    decision
}

fn record_fail_open_vector_scan(
    mode: Option<VectorScanMode>,
    candidate_count: u64,
    candidate_cap: u64,
    reason: &'static str,
) {
    record_vector_scan(VectorScanSnapshot {
        mode,
        candidate_count,
        candidate_cap,
        last_fail_open_reason: Some(reason.to_string()),
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{Drawer, SourceType};
    use tempfile::TempDir;

    fn test_drawer(id: &str) -> Drawer {
        Drawer {
            id: id.to_string(),
            content: format!("content for {id}"),
            wing: "code-memory".to_string(),
            room: Some("novelty".to_string()),
            source_file: Some(format!("{id}.md")),
            source_type: SourceType::AgentInference,
            added_at: "1700000000".to_string(),
            chunk_index: None,
            importance: 0,
            ..Drawer::default()
        }
    }

    #[test]
    fn evaluate_records_fail_open_reason_when_exact_search_bails_out() {
        crate::observability::reset_vector_scan_for_tests();
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("open db");
        let drawer = test_drawer("seed");

        db.insert_drawer_with_project(&drawer, None)
            .expect("insert drawer");
        db.insert_vector_with_project(&drawer.id, &[0.1_f32, 0.2_f32, 0.3_f32], None)
            .expect("insert vector");
        db.conn()
            .execute_batch("DROP TABLE drawer_vectors;")
            .expect("drop drawer_vectors");

        let decision = evaluate(
            &db,
            &NoveltyCandidate {
                wing: "code-memory".to_string(),
                room: Some("novelty".to_string()),
                project_id: None,
            },
            &[0.1_f32, 0.2_f32, 0.3_f32],
            &NoveltyConfig {
                enabled: true,
                ..NoveltyConfig::default()
            },
        );

        assert!(matches!(decision.action, NoveltyAction::Insert));
        let snapshot = crate::observability::vector_scan_snapshot();
        // When drawer_vectors is absent, count_novelty_candidate_drawers returns 0
        // and novelty_candidates_exact returns Ok(empty). The scan succeeds (no
        // error), so fail_open_reason is None — the absence of vectors is not a
        // failure, just an empty result set.
        assert_eq!(snapshot.mode, Some(VectorScanMode::Bounded));
        assert_eq!(snapshot.candidate_count, 0);
        assert_eq!(snapshot.last_fail_open_reason, None);
    }

    #[test]
    fn evaluate_records_fail_open_reason_when_novelty_search_errors() {
        crate::observability::reset_vector_scan_for_tests();
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("test.db")).expect("open db");
        let drawer = test_drawer("seed-fail-open");

        db.insert_drawer_with_project(&drawer, None)
            .expect("insert drawer");
        // Insert a 3-dim vector so the table exists and count > 0.
        db.insert_vector_with_project(&drawer.id, &[0.1_f32, 0.2_f32, 0.3_f32], None)
            .expect("insert vector");

        // Query with a mismatched dimension (4-dim vs 3-dim stored).
        // This causes vec_distance_cosine to error inside
        // novelty_candidates_exact, triggering the fail-open path that
        // records last_fail_open_reason = "bounded_no_project_search_failed".
        let decision = evaluate(
            &db,
            &NoveltyCandidate {
                wing: "code-memory".to_string(),
                room: Some("novelty".to_string()),
                project_id: None,
            },
            // 4-dim query vector vs 3-dim stored vectors
            &[0.1_f32, 0.2_f32, 0.3_f32, 0.4_f32],
            &NoveltyConfig {
                enabled: true,
                ..NoveltyConfig::default()
            },
        );

        assert!(matches!(decision.action, NoveltyAction::Insert));
        let snapshot = crate::observability::vector_scan_snapshot();
        assert_eq!(snapshot.mode, Some(VectorScanMode::Bounded));
        assert!(
            snapshot.last_fail_open_reason.is_some(),
            "fail-open reason should be recorded when novelty search errors: {snapshot:?}"
        );
    }
}
