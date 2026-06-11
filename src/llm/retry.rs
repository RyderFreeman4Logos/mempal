use std::time::Duration;

use super::client::{LlmError, LlmResponse};

pub type HeartbeatCallback = dyn Fn() -> Result<(), LlmError> + Send + Sync;

const MAX_HEARTBEAT_REFRESH_SECS: u64 = 5;
const MAX_RETRY_AFTER_SECS: u64 = 60;

pub async fn retry_llm_operation<F, Fut>(
    retry_interval_secs: u64,
    heartbeat: Option<&HeartbeatCallback>,
    mut operation: F,
) -> Result<LlmResponse, LlmError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<LlmResponse, LlmError>>,
{
    let heartbeat_interval_secs = retry_interval_secs.clamp(1, MAX_HEARTBEAT_REFRESH_SECS);
    loop {
        match await_with_heartbeat(operation(), heartbeat, heartbeat_interval_secs).await {
            Ok(response) => return Ok(response),
            Err(error) => {
                if !error.is_retryable() {
                    return Err(error);
                }
                let wait = retry_after_from_error(&error, retry_interval_secs);
                refresh_heartbeat(heartbeat);
                wait_with_heartbeat(wait, heartbeat).await;
            }
        }
    }
}

async fn await_with_heartbeat<Fut, T>(
    future: Fut,
    heartbeat: Option<&HeartbeatCallback>,
    heartbeat_interval_secs: u64,
) -> T
where
    Fut: std::future::Future<Output = T>,
{
    match heartbeat {
        None => future.await,
        Some(callback) => {
            let mut future = Box::pin(future);
            let mut ticker = tokio::time::interval(Duration::from_secs(heartbeat_interval_secs));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await;
            loop {
                tokio::select! {
                    result = &mut future => return result,
                    _ = ticker.tick() => refresh_heartbeat(Some(callback)),
                }
            }
        }
    }
}

fn retry_after_from_error(error: &LlmError, default_secs: u64) -> Duration {
    if let LlmError::TemporarilyUnavailable { retry_after, .. } = error {
        let capped = retry_after.as_secs().min(MAX_RETRY_AFTER_SECS);
        return Duration::from_secs(capped);
    }
    if let LlmError::ClientError {
        retry_after: Some(header_duration),
        ..
    } = error
    {
        let capped = header_duration.as_secs().min(MAX_RETRY_AFTER_SECS);
        return Duration::from_secs(capped);
    }
    Duration::from_secs(default_secs)
}

async fn wait_with_heartbeat(total: Duration, heartbeat: Option<&HeartbeatCallback>) {
    let started_at = tokio::time::Instant::now();
    let tick = Duration::from_millis(50);
    loop {
        let elapsed = started_at.elapsed();
        if elapsed >= total {
            refresh_heartbeat(heartbeat);
            return;
        }
        let remaining = total.saturating_sub(elapsed).min(tick);
        tokio::time::sleep(remaining).await;
    }
}

fn refresh_heartbeat(heartbeat: Option<&HeartbeatCallback>) {
    if let Some(callback) = heartbeat {
        let _ = callback();
    }
}
