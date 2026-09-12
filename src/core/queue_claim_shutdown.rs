use super::{AsyncPendingMessageStore, ClaimedMessage, QueueError, Result};
#[cfg(any(test, feature = "db-test-seam"))]
use super::{consume_lock_failure, sqlite_busy_queue_error};

impl AsyncPendingMessageStore {
    pub(crate) async fn claim_next_by_kind_until_shutdown(
        &self,
        worker_id: String,
        claim_ttl_secs: i64,
        kind_filter: String,
        shutdown_rx: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<Option<ClaimedMessage>> {
        if *shutdown_rx.borrow() {
            return Ok(None);
        }
        #[cfg(any(test, feature = "db-test-seam"))]
        if consume_lock_failure(&self.claim_lock_failures) {
            return Err(sqlite_busy_queue_error());
        }
        let permit = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => return Ok(None),
            permit = self.permits.clone().acquire_owned() => permit.map_err(|_| {
                QueueError::BlockingTaskFailed("queue semaphore closed".to_string())
            })?,
        };
        #[cfg(any(test, feature = "db-test-seam"))]
        let delay = self.claim_blocking_delay.or(self.blocking_delay);
        #[cfg(not(any(test, feature = "db-test-seam")))]
        let delay = None;
        #[cfg(any(test, feature = "db-test-seam"))]
        let started = self.claim_blocking_started.clone();
        #[cfg(not(any(test, feature = "db-test-seam")))]
        let started: Option<std::sync::Arc<tokio::sync::Notify>> = None;
        let store = self.inner.clone();
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (approval_tx, approval_rx) = tokio::sync::oneshot::channel();
        let join = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            tracing::dispatcher::with_default(&dispatch, || {
                if let Some(started) = started {
                    started.notify_waiters();
                }
                if let Some(delay) = delay {
                    std::thread::sleep(delay);
                }
                let mut ready_tx = Some(ready_tx);
                let mut approval_rx = Some(approval_rx);
                store.claim_next_by_kind_with_approval(
                    &worker_id,
                    claim_ttl_secs,
                    &kind_filter,
                    || {
                        if let Some(ready_tx) = ready_tx.take() {
                            let _ = ready_tx.send(());
                        }
                        approval_rx
                            .take()
                            .and_then(|approval_rx| approval_rx.blocking_recv().ok())
                            .unwrap_or(false)
                    },
                )
            })
        });
        let approved = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => false,
            ready = ready_rx => ready.is_ok() && !*shutdown_rx.borrow(),
        };
        #[cfg(any(test, feature = "db-test-seam"))]
        if approved {
            if let Some(claim_approved) = &self.claim_approved {
                claim_approved.notify_waiters();
            }
        }
        let _ = approval_tx.send(approved);
        if !approved && *shutdown_rx.borrow() {
            // The blocking task cannot cross the approval fence, so it owns no
            // claim and may finish off-runtime after the scoped worker exits.
            return Ok(None);
        }
        match join.await {
            Ok(out) => out,
            Err(error) => Err(QueueError::BlockingTaskFailed(error.to_string())),
        }
    }
}
