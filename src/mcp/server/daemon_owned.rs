use std::sync::Arc;

use crate::core::{AsyncDb, queue::QueueStats};

impl super::MempalMcpServer {
    pub fn with_daemon_owned_async_db(mut self, async_db: AsyncDb) -> Self {
        self.daemon_owned_async_db = true;
        let query_only = async_db.query_only_view();
        self.async_db = Arc::new(tokio::sync::OnceCell::new());
        if self.async_db.set(async_db).is_err() {
            unreachable!("daemon-owned async DB cell must be empty");
        }
        self.query_only_async_db = Arc::new(tokio::sync::OnceCell::new());
        if self.query_only_async_db.set(query_only).is_err() {
            unreachable!("daemon-owned query-only async DB cell must be empty");
        }
        self
    }

    pub(crate) fn with_daemon_write_observer(
        mut self,
        observer: crate::daemon_bootstrap::DaemonWriteObserver,
    ) -> Self {
        self.daemon_write_observer = Some(observer);
        self
    }

    async fn daemon_queue_stats(&self) -> anyhow::Result<QueueStats> {
        self.reader_db()
            .await?
            .run_read_anyhow(|db| {
                crate::core::queue::queue_stats(db.conn()).map_err(anyhow::Error::new)
            })
            .await
    }

    pub(super) async fn queue_stats_for_status(&self) -> anyhow::Result<QueueStats> {
        if self.daemon_owned_async_db {
            self.daemon_queue_stats().await
        } else {
            self.async_queue.stats().await.map_err(anyhow::Error::new)
        }
    }
}
