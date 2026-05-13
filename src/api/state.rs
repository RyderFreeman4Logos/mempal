use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::core::config::ConfigHandle;
use crate::embed::EmbedderFactory;

#[derive(Clone)]
pub struct ApiState {
    pub db_path: PathBuf,
    pub embedder_factory: Arc<dyn EmbedderFactory>,
    write_queue: Arc<super::handlers::WriteQueue>,
}

impl ApiState {
    pub fn new(db_path: PathBuf, embedder_factory: Arc<dyn EmbedderFactory>) -> Self {
        let config = ConfigHandle::current();
        Self::with_write_queue_config(
            db_path,
            embedder_factory,
            config.api.write_queue_capacity,
            Duration::from_secs(config.api.write_drain_timeout_secs),
        )
    }

    pub fn with_write_queue_config(
        db_path: PathBuf,
        embedder_factory: Arc<dyn EmbedderFactory>,
        queue_capacity: usize,
        drain_timeout: Duration,
    ) -> Self {
        let write_queue = Arc::new(super::handlers::WriteQueue::spawn(
            db_path.clone(),
            Arc::clone(&embedder_factory),
            queue_capacity,
            drain_timeout,
        ));
        Self {
            db_path,
            embedder_factory,
            write_queue,
        }
    }

    pub(crate) fn write_queue(&self) -> &super::handlers::WriteQueue {
        &self.write_queue
    }

    pub async fn drain_write_queue(&self) -> bool {
        self.write_queue.drain().await
    }
}
