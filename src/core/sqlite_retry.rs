//! Shared bounded transient-SQLite-lock retry policy for content mutations.
//!
//! CLI mutation retries (#831) and MCP soft-delete retries (#836) use this
//! policy so their bounded lock handling cannot drift.

use std::future::Future;
use std::time::{Duration, Instant};

const CONTENT_MUTATION_SQLITE_LOCK_RETRY_DEADLINE: Duration = Duration::from_secs(10);
const CONTENT_MUTATION_SQLITE_LOCK_INITIAL_DELAY_MS: u64 = 25;
const CONTENT_MUTATION_SQLITE_LOCK_MAX_DELAY_MS: u64 = 500;

fn content_mutation_sqlite_lock_retry_delay(attempt: usize) -> Duration {
    let multiplier = 1_u64 << attempt.min(4);
    let delay_ms = CONTENT_MUTATION_SQLITE_LOCK_INITIAL_DELAY_MS
        .saturating_mul(multiplier)
        .min(CONTENT_MUTATION_SQLITE_LOCK_MAX_DELAY_MS);
    Duration::from_millis(delay_ms)
}

pub fn retry_content_mutation_sqlite_lock<T, E>(
    mut operation: impl FnMut() -> Result<T, E>,
    is_transient_lock: impl Fn(&E) -> bool,
) -> Result<T, E> {
    let retry_deadline = Instant::now() + CONTENT_MUTATION_SQLITE_LOCK_RETRY_DEADLINE;
    let mut attempt = 0;
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if is_transient_lock(&error) => {
                let remaining = retry_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(error);
                }
                std::thread::sleep(
                    content_mutation_sqlite_lock_retry_delay(attempt).min(remaining),
                );
                attempt = attempt.saturating_add(1);
            }
            Err(error) => return Err(error),
        }
    }
}

pub async fn retry_content_mutation_sqlite_lock_async<T, E, F, Fut>(
    mut operation: F,
    is_transient_lock: impl Fn(&E) -> bool,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let retry_deadline = Instant::now() + CONTENT_MUTATION_SQLITE_LOCK_RETRY_DEADLINE;
    let mut attempt = 0;
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) if is_transient_lock(&error) => {
                let remaining = retry_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(error);
                }
                tokio::time::sleep(
                    content_mutation_sqlite_lock_retry_delay(attempt).min(remaining),
                )
                .await;
                attempt = attempt.saturating_add(1);
            }
            Err(error) => return Err(error),
        }
    }
}
