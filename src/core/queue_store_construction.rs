use std::path::Path;

use super::{ConnectionCache, PendingMessageStore, QueueConfig};

impl PendingMessageStore {
    pub fn new_without_reclaim(path: impl AsRef<Path>) -> Self {
        Self::new_without_reclaim_with_config(path, QueueConfig::default())
    }

    /// Create a queue store without startup reclamation using an explicit configuration.
    pub fn new_without_reclaim_with_config(path: impl AsRef<Path>, config: QueueConfig) -> Self {
        Self {
            db_path: path.as_ref().to_path_buf(),
            config,
            connection_cache: ConnectionCache::new(),
        }
    }
}
