//! Rule-based importance scorer for mempal drawers.
//!
//! This is intentionally LLM-free: the heuristic runs purely on content and wing
//! metadata, keeping the scorer fast, offline, and deterministic.
//!
//! Used by `mempal reindex --recompute-importance` to back-fill legacy drawers that
//! were ingested before agents started setting importance explicitly. The scorer is
//! NOT called at ingest time — per spec p5 line 37, importance comes from the caller.

use crate::core::types::Drawer;

/// True when `content` has a Markdown H1 heading that starts with `heading_prefix`.
///
/// Line-aware: `# Decision` is not triggered by `## Decision` (which contains the
/// substring `# Decision` at offset 1, but only at the start of a line do they differ).
fn has_h1_heading(content: &str, heading_prefix: &str) -> bool {
    content.starts_with(heading_prefix) || content.contains(&format!("\n{heading_prefix}"))
}

/// Score drawer importance with a rule-based heuristic. Returns a value in `[0, 5]`.
///
/// Rules (score accumulates, result clamped to 5):
///
/// - `wing == "decision"` or H1 heading `# Decision` → +2
/// - `wing == "discovery"` or `wing == "feature"` or H1 heading `# Discovery` → +1
/// - Each of `## Why`, `## Decision`, `## Concepts`, `## Facts` present → +1 each, capped at +2
/// - Content length > 1000 bytes → +1
pub fn score_importance(drawer: &Drawer) -> i32 {
    let mut score: i32 = 0;
    let wing = drawer.wing.as_str();
    let content = drawer.content.as_str();

    let wing_bonus = if wing == "decision" || has_h1_heading(content, "# Decision") {
        2
    } else if wing == "discovery" || wing == "feature" || has_h1_heading(content, "# Discovery") {
        1
    } else {
        0
    };
    score += wing_bonus;

    // Structural sections: each present counts once, total contribution capped at 2.
    let section_bonus = ["## Why", "## Decision", "## Concepts", "## Facts"]
        .iter()
        .filter(|&&s| content.contains(s))
        .count();
    score += i32::min(section_bonus as i32, 2);

    if content.len() > 1000 {
        score += 1;
    }

    score.clamp(0, 5)
}

#[cfg(test)]
mod tests {
    use super::score_importance;
    use crate::core::types::{Drawer, SourceType};

    fn drawer(wing: &str, content: &str) -> Drawer {
        Drawer {
            id: "test-id".to_string(),
            content: content.to_string(),
            wing: wing.to_string(),
            room: None,
            source_file: None,
            source_type: SourceType::Manual,
            added_at: "1713000000".to_string(),
            chunk_index: None,
            importance: 0,
        }
    }

    #[test]
    fn test_score_importance_decision_with_full_sections_at_least_3() {
        let content = "# Decision: use SQLite\n\n\
            ## Why\nSQLite is embedded and has no external dependencies.\n\n\
            ## Decision\nWe chose SQLite over Postgres for simplicity.\n\n\
            ## Concepts\n- ACID, WAL mode\n\n\
            ## Facts\n- SQLite is used by billions of apps\n\n\
            This decision was reached after benchmarking several alternatives.";
        let d = drawer("decision", content);
        let score = score_importance(&d);
        assert!(
            score >= 3,
            "decision drawer with full sections should score >= 3, got {score}"
        );
        assert!(score <= 5, "score must not exceed 5, got {score}");
    }

    #[test]
    fn test_score_importance_short_chatter_returns_zero_or_one() {
        let content = "ok sounds good";
        let d = drawer("default", content);
        let score = score_importance(&d);
        assert!(score <= 1, "short chatter should score <= 1, got {score}");
    }

    #[test]
    fn test_score_importance_clamps_to_five() {
        // Max without clamping: decision → +2, two sections → +2, length → +1 = 5.
        // Extra sections beyond 2 cannot push it above 5.
        let long_content = format!(
            "# Decision: max score\n## Why\n{}\n## Decision\nyes\n## Concepts\nc\n## Facts\nf\n",
            "x".repeat(1100)
        );
        let d = drawer("decision", &long_content);
        let score = score_importance(&d);
        assert_eq!(score, 5, "max-score input should clamp to 5");
    }

    #[test]
    fn test_score_importance_discovery_wing() {
        let d = drawer("discovery", "short discovery note");
        let score = score_importance(&d);
        assert_eq!(score, 1, "discovery wing without extras should score 1");
    }

    #[test]
    fn test_score_importance_feature_wing() {
        let d = drawer("feature", "brief feature note");
        let score = score_importance(&d);
        assert_eq!(score, 1, "feature wing without extras should score 1");
    }

    #[test]
    fn test_score_importance_section_cap_at_two() {
        // All 4 sections present on default wing, no decision/discovery markers.
        // Use "### Decision" (H3) to avoid triggering "# Decision" H1 detection.
        let content =
            "plain text\n## Why\nreason\n## Concepts\nconcepts\n## Facts\nfacts\n### Decision\nd";
        let d = drawer("default", content);
        let score = score_importance(&d);
        // sections: ## Why, ## Concepts, ## Facts = 3 present, capped at 2 → +2
        // wing: default → 0; length: short → 0
        assert_eq!(score, 2, "three sections on default wing should score 2");
    }

    #[test]
    fn test_h1_heading_does_not_match_h2() {
        // "## Decision" must NOT trigger the # Decision bonus (+2)
        let content = "## Decision: this is H2, not H1\n## Why\nreason";
        let d = drawer("default", content);
        let score = score_importance(&d);
        // sections: ## Decision, ## Why → 2 sections capped at 2 → +2; no H1 → 0
        assert_eq!(
            score, 2,
            "## Decision should not trigger H1 decision bonus; got {score}"
        );
    }
}
