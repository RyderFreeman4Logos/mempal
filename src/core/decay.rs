//! Pure importance-decay, retrieval-boost, and search temporal scoring functions.
//!
//! Implements heuristic effective_importance = base * decay(days) + capped_boost.
//! All parameters come from `ImportanceConfig` which is hot-reload whitelisted.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::config::{DecayConfig, DecayMode, ImportanceConfig};
use crate::cowork::peek::parse_rfc3339;

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

/// Parse mempal temporal fields as Unix seconds or RFC3339 timestamps.
pub fn parse_temporal_timestamp_secs(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    value.parse::<i64>().ok().or_else(|| parse_rfc3339(value))
}

/// Compute the search relevance decay factor for a drawer timestamp.
///
/// Invalid timestamps fail open with factor `1.0` so search recall is not lost
/// because of legacy or externally supplied metadata.
pub fn search_decay_factor(added_at: &str, config: &DecayConfig) -> f64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    search_decay_factor_at(added_at, config, now)
}

/// Deterministic variant of [`search_decay_factor`] for tests and callers that
/// already captured a request-level clock.
pub fn search_decay_factor_at(added_at: &str, config: &DecayConfig, now_secs: i64) -> f64 {
    let Some(added_at_secs) = parse_temporal_timestamp_secs(added_at) else {
        return 1.0;
    };
    let age_days = ((now_secs - added_at_secs).max(0) as f64) / 86_400.0;
    match config.mode {
        DecayMode::None => 1.0,
        DecayMode::Exponential => (-0.693 * age_days / config.half_life_days as f64).exp(),
        DecayMode::Linear => (1.0 - age_days / config.half_life_days as f64).max(0.0),
        DecayMode::Step => {
            if age_days <= config.step_full_days as f64 {
                1.0
            } else {
                config.step_reduced_weight
            }
        }
    }
}

/// Return whether a validity window contains `now_secs`.
///
/// Unset or unparseable bounds are treated as open so malformed legacy
/// metadata does not silently hide otherwise searchable memories.
pub fn validity_window_contains_at(
    valid_from: Option<&str>,
    valid_until: Option<&str>,
    now_secs: i64,
) -> bool {
    if valid_from
        .and_then(parse_temporal_timestamp_secs)
        .is_some_and(|from| from > now_secs)
    {
        return false;
    }
    if valid_until
        .and_then(parse_temporal_timestamp_secs)
        .is_some_and(|until| until < now_secs)
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::{DecayConfig, DecayMode, ImportanceConfig};

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

    #[test]
    fn test_search_decay_none_is_noop() {
        let cfg = DecayConfig::default();
        let factor = search_decay_factor_at("1710000000", &cfg, 1710000000 + 365 * 86_400);
        assert_eq!(factor, 1.0);
    }

    #[test]
    fn test_search_decay_exponential_halves_at_half_life() {
        let cfg = DecayConfig {
            mode: DecayMode::Exponential,
            half_life_days: 90,
            ..DecayConfig::default()
        };
        let factor = search_decay_factor_at("1710000000", &cfg, 1710000000 + 90 * 86_400);
        assert!((factor - 0.500_073_6).abs() < 1e-6);
    }

    #[test]
    fn test_search_decay_linear_reaches_zero_at_half_life() {
        let cfg = DecayConfig {
            mode: DecayMode::Linear,
            half_life_days: 90,
            ..DecayConfig::default()
        };
        let factor = search_decay_factor_at("1710000000", &cfg, 1710000000 + 45 * 86_400);
        assert!((factor - 0.5).abs() < 1e-9);

        let zero = search_decay_factor_at("1710000000", &cfg, 1710000000 + 91 * 86_400);
        assert_eq!(zero, 0.0);
    }

    #[test]
    fn test_search_decay_step_uses_reduced_weight_after_full_window() {
        let cfg = DecayConfig {
            mode: DecayMode::Step,
            step_full_days: 30,
            step_reduced_weight: 0.25,
            ..DecayConfig::default()
        };
        assert_eq!(
            search_decay_factor_at("1710000000", &cfg, 1710000000 + 30 * 86_400),
            1.0
        );
        assert_eq!(
            search_decay_factor_at("1710000000", &cfg, 1710000000 + 31 * 86_400),
            0.25
        );
    }

    #[test]
    fn test_search_decay_accepts_rfc3339_and_fails_open() {
        let cfg = DecayConfig {
            mode: DecayMode::Linear,
            half_life_days: 10,
            ..DecayConfig::default()
        };
        let factor = search_decay_factor_at("2026-05-01T00:00:00Z", &cfg, 1_777_852_800);
        assert!((factor - 0.7).abs() < 1e-9);
        assert_eq!(
            search_decay_factor_at("not-a-time", &cfg, 1_777_930_400),
            1.0
        );
    }

    #[test]
    fn test_validity_window_excludes_future_and_expired() {
        let now = 1_710_000_000;
        assert!(validity_window_contains_at(None, None, now));
        assert!(validity_window_contains_at(
            Some("1709999900"),
            Some("1710000100"),
            now
        ));
        assert!(!validity_window_contains_at(Some("1710000100"), None, now));
        assert!(!validity_window_contains_at(None, Some("1709999900"), now));
        assert!(validity_window_contains_at(
            Some("not-a-time"),
            Some("also-not-a-time"),
            now
        ));
    }
}
