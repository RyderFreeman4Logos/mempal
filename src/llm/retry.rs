use std::time::Duration;

use super::client::{LlmError, LlmResponse};

pub type HeartbeatCallback = dyn Fn() -> Result<(), LlmError> + Send + Sync;

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
    loop {
        match operation().await {
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

fn retry_after_from_error(error: &LlmError, default_secs: u64) -> Duration {
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
