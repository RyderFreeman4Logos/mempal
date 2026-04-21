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
    let config = status.retry_config_snapshot();
    loop {
        match operation().await {
            Ok(vectors) => return Ok(vectors),
            Err(error) => {
                status.record_failure_with_snapshot(&error, &config);
                refresh_heartbeat(heartbeat);
                tokio::time::sleep(Duration::from_secs(config.retry_interval_secs)).await;
                refresh_heartbeat(heartbeat);
            }
        }
    }
}

fn refresh_heartbeat(heartbeat: Option<&HeartbeatCallback>) {
    if let Some(callback) = heartbeat {
        let _ = callback();
    }
}
