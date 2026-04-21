use std::future::Future;
use std::time::Duration;

use super::{Result, status::EmbedStatus};

pub type HeartbeatCallback = dyn Fn() -> Result<()> + Send + Sync;

pub async fn retry_embed_operation<F, Fut>(
    status: &EmbedStatus,
    heartbeat: Option<&HeartbeatCallback>,
    mut operation: F,
) -> Result<Vec<Vec<f32>>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Vec<Vec<f32>>>>,
{
    loop {
        match operation().await {
            Ok(vectors) => return Ok(vectors),
            Err(error) => {
                let config = status.retry_config_snapshot();
                status.record_failure_with_snapshot(&error, &config);
                if !error.is_retryable() {
                    return Err(error);
                }
                refresh_heartbeat(heartbeat);
                wait_for_next_retry(status, heartbeat).await;
            }
        }
    }
}

async fn wait_for_next_retry(status: &EmbedStatus, heartbeat: Option<&HeartbeatCallback>) {
    let started_at = tokio::time::Instant::now();
    let tick = Duration::from_millis(50);
    loop {
        let retry_after = Duration::from_secs(status.retry_interval_secs());
        let elapsed = started_at.elapsed();
        if elapsed >= retry_after {
            refresh_heartbeat(heartbeat);
            return;
        }

        let remaining = retry_after.saturating_sub(elapsed).min(tick);
        tokio::time::sleep(remaining).await;
    }
}

fn refresh_heartbeat(heartbeat: Option<&HeartbeatCallback>) {
    if let Some(callback) = heartbeat {
        let _ = callback();
    }
}
