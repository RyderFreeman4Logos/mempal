//! Monotonic-clock representability checks for configured search deadlines.

use std::time::{Duration, Instant};

use super::config::{Config, ConfigError};

pub(super) fn validate_search_deadlines(config: &Config) -> Result<(), ConfigError> {
    let deadlines = [
        (
            "embed.retry.search_deadline_secs",
            config.embed.retry.search_deadline_secs,
        ),
        (
            "api.search_query_deadline_secs",
            config.api.search_query_deadline_secs,
        ),
        (
            "api.search_db_deadline_secs",
            config.api.search_db_deadline_secs,
        ),
        (
            "search.reranker.timeout_secs",
            config.search.reranker.timeout_secs,
        ),
    ];
    for (field, seconds) in deadlines {
        if Instant::now()
            .checked_add(Duration::from_secs(seconds))
            .is_none()
        {
            return Err(ConfigError::InvalidConfig(format!(
                "{field} must be representable by the platform monotonic clock"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_u64_max_without_panicking() {
        let mut config = Config::default();
        config.api.search_query_deadline_secs = u64::MAX;

        let error = validate_search_deadlines(&config)
            .expect_err("u64::MAX seconds must exceed monotonic clock representation");

        assert!(error.to_string().contains("search_query_deadline_secs"));
        assert!(error.to_string().contains("representable"));
    }

    #[test]
    fn accepts_operator_deadline_above_four_minutes() {
        let mut config = Config::default();
        config.api.search_query_deadline_secs = 600;

        validate_search_deadlines(&config).expect("600 seconds should be representable");
    }
}
