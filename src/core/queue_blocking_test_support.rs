#[cfg(test)]
use super::AsyncPendingMessageStore;
use std::sync::Arc;
use tokio::sync::Notify;

pub(super) struct BlockingFinished(pub(super) Option<Arc<Notify>>);

impl Drop for BlockingFinished {
    fn drop(&mut self) {
        if let Some(finished) = self.0.take() {
            finished.notify_one();
        }
    }
}

#[cfg(test)]
impl AsyncPendingMessageStore {
    pub(crate) fn available_blocking_permits_for_test(&self) -> usize {
        self.permits.available_permits()
    }

    pub(crate) fn claim_connection_open_count_for_test(&self) -> usize {
        self.inner.claim_connection_open_count()
    }
}
