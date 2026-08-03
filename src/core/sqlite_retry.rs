//! Shared bounded transient-SQLite-lock retry policy for content mutations.
//!
//! CLI mutation retries (#831) and MCP soft-delete retries (#836) use this
//! policy so their bounded lock handling cannot drift.

use std::future::Future;
use std::time::{Duration, Instant};

const CONTENT_MUTATION_SQLITE_LOCK_RETRY_DEADLINE: Duration = Duration::from_secs(10);
const CONTENT_MUTATION_SQLITE_LOCK_INITIAL_DELAY_MS: u64 = 25;
const CONTENT_MUTATION_SQLITE_LOCK_MAX_DELAY_MS: u64 = 500;

pub(crate) fn content_mutation_sqlite_lock_retry_deadline_error() -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::DatabaseBusy,
            extended_code: rusqlite::ffi::SQLITE_BUSY,
        },
        Some("database is locked: content mutation retry deadline exceeded".to_string()),
    )
}

fn content_mutation_sqlite_lock_retry_deadline_error_for<E>() -> E
where
    E: From<rusqlite::Error>,
{
    E::from(content_mutation_sqlite_lock_retry_deadline_error())
}

fn take_last_transient_error_or_deadline_error<E>(last_transient_error: &mut Option<E>) -> E
where
    E: From<rusqlite::Error>,
{
    last_transient_error
        .take()
        .unwrap_or_else(content_mutation_sqlite_lock_retry_deadline_error_for)
}

fn content_mutation_sqlite_lock_retry_delay(attempt: usize) -> Duration {
    let multiplier = 1_u64 << attempt.min(4);
    let delay_ms = CONTENT_MUTATION_SQLITE_LOCK_INITIAL_DELAY_MS
        .saturating_mul(multiplier)
        .min(CONTENT_MUTATION_SQLITE_LOCK_MAX_DELAY_MS);
    Duration::from_millis(delay_ms)
}

pub fn retry_content_mutation_sqlite_lock<T, E>(
    operation: impl FnMut() -> Result<T, E>,
    is_transient_lock: impl Fn(&E) -> bool,
) -> Result<T, E>
where
    E: From<rusqlite::Error>,
{
    let retry_deadline = Instant::now() + CONTENT_MUTATION_SQLITE_LOCK_RETRY_DEADLINE;
    retry_content_mutation_sqlite_lock_until(retry_deadline, operation, is_transient_lock)
}

fn retry_content_mutation_sqlite_lock_until<T, E>(
    retry_deadline: Instant,
    mut operation: impl FnMut() -> Result<T, E>,
    is_transient_lock: impl Fn(&E) -> bool,
) -> Result<T, E>
where
    E: From<rusqlite::Error>,
{
    let mut attempt = 0;
    let mut last_transient_error = None;
    loop {
        if Instant::now() >= retry_deadline {
            return Err(take_last_transient_error_or_deadline_error(
                &mut last_transient_error,
            ));
        }

        match operation() {
            // A returned success can represent a committed SQLite mutation;
            // elapsed retry budget cannot safely reinterpret it as a lock error.
            Ok(value) => return Ok(value),
            Err(error) if is_transient_lock(&error) => {
                last_transient_error = Some(error);
                let remaining = retry_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(take_last_transient_error_or_deadline_error(
                        &mut last_transient_error,
                    ));
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
    operation: F,
    is_transient_lock: impl Fn(&E) -> bool,
) -> Result<T, E>
where
    E: From<rusqlite::Error>,
    F: FnMut(Instant) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let retry_deadline =
        tokio::time::Instant::now().into_std() + CONTENT_MUTATION_SQLITE_LOCK_RETRY_DEADLINE;
    retry_content_mutation_sqlite_lock_async_until(retry_deadline, operation, is_transient_lock)
        .await
}

/// Retry a query-only read closure on transient SQLite BUSY/LOCKED errors under
/// a caller-supplied deadline (#840).
///
/// Unlike [`retry_content_mutation_sqlite_lock_async`], this does not impose a
/// fresh 10-second budget: the read must honor the wall-clock deadline the
/// caller already granted (for example the MCP search deadline), so the retry
/// loop reuses that deadline directly. A returned success is always passed
/// through unchanged, even if it arrives after the deadline has elapsed.
pub async fn retry_query_only_read_sqlite_lock_async<T, E, F, Fut>(
    retry_deadline: Instant,
    operation: F,
    is_transient_lock: impl Fn(&E) -> bool,
) -> Result<T, E>
where
    E: From<rusqlite::Error>,
    F: FnMut(Instant) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    retry_content_mutation_sqlite_lock_async_until(retry_deadline, operation, is_transient_lock)
        .await
}

async fn retry_content_mutation_sqlite_lock_async_until<T, E, F, Fut>(
    retry_deadline: Instant,
    mut operation: F,
    is_transient_lock: impl Fn(&E) -> bool,
) -> Result<T, E>
where
    E: From<rusqlite::Error>,
    F: FnMut(Instant) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt = 0;
    let mut last_transient_error = None;
    loop {
        if Instant::now() >= retry_deadline {
            return Err(take_last_transient_error_or_deadline_error(
                &mut last_transient_error,
            ));
        }

        match operation(retry_deadline).await {
            // See the synchronous helper: a successful mutation is definitive.
            Ok(value) => return Ok(value),
            Err(error) if is_transient_lock(&error) => {
                last_transient_error = Some(error);
                let remaining = retry_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(take_last_transient_error_or_deadline_error(
                        &mut last_transient_error,
                    ));
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn async_wrapper_passes_exact_shared_ten_second_deadline() {
        let start = tokio::time::Instant::now().into_std();
        retry_content_mutation_sqlite_lock_async(
            |deadline| async move {
                assert_eq!(deadline, start + Duration::from_secs(10));
                Ok::<_, rusqlite::Error>(())
            },
            |_| false,
        )
        .await
        .expect("immediate operation succeeds");
    }

    #[test]
    fn sync_retry_preserves_success_returned_after_deadline() {
        let deadline = Instant::now() + Duration::from_millis(20);
        let result = retry_content_mutation_sqlite_lock_until(
            deadline,
            || {
                std::thread::sleep(Duration::from_millis(50));
                Ok::<_, rusqlite::Error>(7)
            },
            |_| false,
        );

        assert_eq!(result.expect("late successful mutation"), 7);
        assert!(
            Instant::now() >= deadline,
            "fixture must return after expiry"
        );
    }

    #[tokio::test]
    async fn async_retry_preserves_success_returned_after_deadline() {
        let deadline = Instant::now() + Duration::from_millis(20);
        let result = retry_content_mutation_sqlite_lock_async_until(
            deadline,
            |_| async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok::<_, rusqlite::Error>(7)
            },
            |_| false,
        )
        .await;

        assert_eq!(result.expect("late successful mutation"), 7);
        assert!(
            Instant::now() >= deadline,
            "fixture must return after expiry"
        );
    }

    #[tokio::test]
    async fn read_retry_retries_transient_lock_then_succeeds() {
        // #840: the query-only read retry variant honors a caller-supplied
        // deadline and retries on transient lock errors until success.
        let deadline = Instant::now() + Duration::from_secs(5);
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_op = Arc::clone(&attempts);
        let result = retry_query_only_read_sqlite_lock_async(
            deadline,
            move |_| {
                let attempts = Arc::clone(&attempts_for_op);
                async move {
                    let n = attempts.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        Err(rusqlite::Error::SqliteFailure(
                            rusqlite::ffi::Error {
                                code: rusqlite::ErrorCode::DatabaseBusy,
                                extended_code: rusqlite::ffi::SQLITE_BUSY,
                            },
                            None,
                        ))
                    } else {
                        Ok(42)
                    }
                }
            },
            |error: &rusqlite::Error| matches!(error, rusqlite::Error::SqliteFailure(sqlite, _) if sqlite.code == rusqlite::ErrorCode::DatabaseBusy),
        )
        .await;

        assert_eq!(result.expect("retry must succeed"), 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }
}
