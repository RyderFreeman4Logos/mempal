//! Pure importance-decay and retrieval-boost functions (P13, LLM-free).
//!
//! Implements heuristic effective_importance = base * decay(days) + capped_boost.
//! All parameters come from `ImportanceConfig` which is hot-reload whitelisted.

use crate::core::config::ImportanceConfig;

/// Compute `effective_importance` from components.
///
/// Formula:
/// ```text
/// decay = max(exp(-decay_rate * days_since_last_hit), floor)
/// effective = base_importance * decay + min(accumulated_boost, boost_cap)
/// ```
///
/// `days_since_last_hit` is always `>= 0.0`; callers use `added_at` as
/// fallback when `last_accessed_at IS NULL`.
pub fn compute_effective_importance(
    base_importance: f64,
    days_since_last_hit: f64,
    accumulated_boost: f64,
    config: &ImportanceConfig,
) -> f64 {
    let decay = (-config.decay_rate * days_since_last_hit)
        .exp()
        .max(config.floor);
    base_importance * decay + accumulated_boost.min(config.boost_cap)
}

/// Apply stale penalty: multiply current effective_importance by `stale_penalty`.
///
/// Used when `mempal_fact_check` detects a `StaleFact` for a drawer's KG triple.
pub fn apply_stale_penalty(effective_importance: f64, config: &ImportanceConfig) -> f64 {
    effective_importance * config.stale_penalty
}

/// Convert unix-epoch milliseconds difference to floating-point days.
pub fn elapsed_days(now_ms: i64, reference_ms: i64) -> f64 {
    let diff_ms = (now_ms - reference_ms).max(0) as f64;
    diff_ms / 86_400_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::ImportanceConfig;

    fn default_cfg() -> ImportanceConfig {
        ImportanceConfig::default()
    }

    fn cfg_with(
        decay_rate: f64,
        floor: f64,
        boost_per_access: f64,
        boost_cap: f64,
    ) -> ImportanceConfig {
        ImportanceConfig {
            decay_rate,
            floor,
            boost_per_access,
            boost_cap,
            stale_penalty: 0.5,
        }
    }

    #[test]
    fn test_decay_reduces_importance_over_time() {
        let cfg = ImportanceConfig {
            decay_rate: 0.05,
            floor: 0.1,
            boost_per_access: 0.15,
            boost_cap: 2.0,
            stale_penalty: 0.5,
        };
        let result = compute_effective_importance(3.0, 30.0, 0.0, &cfg);
        assert!(result < 3.0, "decayed value {result} should be < 3.0");
        assert!(
            result >= 0.1,
            "decayed value {result} should be >= floor 0.1"
        );
    }

    #[test]
    fn test_decay_floor_prevents_zero() {
        let cfg = cfg_with(0.01, 0.1, 0.15, 2.0);
        let result = compute_effective_importance(1.0, 10_000.0, 0.0, &cfg);
        // exp(-0.01 * 10000) ≈ 0 → floor kicks in
        assert!(
            (result - 0.1).abs() < 1e-9,
            "result {result} should equal floor 0.1"
        );
    }

    #[test]
    fn test_access_boost_increases_effective_importance() {
        let cfg = default_cfg();
        let result = compute_effective_importance(2.0, 0.0, 0.3, &cfg);
        // days=0 → decay=1.0, boost=0.3 (< boost_cap 2.0)
        assert!(
            result > 2.0,
            "boosted value {result} should exceed base 2.0"
        );
    }

    #[test]
    fn test_access_boost_capped_at_max() {
        let cfg = cfg_with(0.01, 0.1, 0.15, 2.0);
        // accumulated_boost = 3.0 > boost_cap 2.0 → capped at 2.0
        let result = compute_effective_importance(3.0, 0.0, 3.0, &cfg);
        let expected = 3.0 * 1.0_f64 + 2.0; // decay=1.0, boost capped at 2.0
        assert!(
            (result - expected).abs() < 1e-9,
            "result {result} should equal {expected}"
        );
    }

    #[test]
    fn test_apply_stale_penalty() {
        let cfg = default_cfg(); // stale_penalty = 0.5
        let result = apply_stale_penalty(3.0, &cfg);
        assert!(
            (result - 1.5).abs() < 1e-9,
            "3.0 * 0.5 should be 1.5, got {result}"
        );
    }

    #[test]
    fn test_elapsed_days_positive() {
        let now = 86_400_000_i64; // 1 day in ms
        let reference = 0_i64;
        assert!((elapsed_days(now, reference) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_elapsed_days_clamps_negative_to_zero() {
        // reference > now → should clamp to 0 days
        assert!((elapsed_days(0, 1_000_000)).abs() < 1e-9);
    }
}
