use super::{AsyncPendingMessageStore, ClaimedMessage, QueueError, Result};
#[cfg(any(test, feature = "db-test-seam"))]
use super::{consume_lock_failure, sqlite_busy_queue_error};

#[cfg(any(test, feature = "db-test-seam"))]
pub(super) type ClaimApprovalTestControl = (
    std::sync::Arc<tokio::sync::Notify>,
    Option<std::sync::Arc<[tokio::sync::Notify; 3]>>,
);

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
        let started = self.blocking_started.clone();
        #[cfg(not(any(test, feature = "db-test-seam")))]
        let started: Option<std::sync::Arc<tokio::sync::Notify>> = None;
        #[cfg(any(test, feature = "db-test-seam"))]
        let claim_approved = self.claim_approved.clone();
        #[cfg(any(test, feature = "db-test-seam"))]
        let claim_committed = self.claim_approved.clone();
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
                        let approved = approval_rx
                            .take()
                            .and_then(|approval_rx| approval_rx.blocking_recv().ok())
                            .unwrap_or(false);
                        #[cfg(any(test, feature = "db-test-seam"))]
                        if approved && let Some((approved, gate)) = &claim_approved {
                            approved.notify_waiters();
                            if let Some(gate) = gate {
                                tokio::runtime::Handle::current().block_on(async {
                                    tokio::time::timeout(
                                        std::time::Duration::from_secs(1),
                                        gate[0].notified(),
                                    )
                                    .await
                                    .expect("test approval gate timed out");
                                });
                            }
                        }
                        approved
                    },
                )
            })
        });
        let owner_queue = self.clone();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let _result_owner = tokio::spawn(async move {
            let result = match join.await {
                Ok(out) => out,
                Err(error) => Err(QueueError::BlockingTaskFailed(error.to_string())),
            };
            #[cfg(any(test, feature = "db-test-seam"))]
            if matches!(&result, Ok(Some(_)))
                && let Some((_, Some(gate))) = &claim_committed
            {
                gate[1].notify_one();
                tokio::time::timeout(std::time::Duration::from_secs(1), gate[2].notified())
                    .await
                    .expect("test settlement gate timed out");
            }
            if let Err(Ok(Some(claim))) = result_tx.send(result)
                && let Err(error) = owner_queue.release_claim(claim).await
            {
                tracing::warn!(
                    ?error,
                    "failed to release approved queue claim after caller cancellation"
                );
            }
        });
        let approved = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => false,
            ready = ready_rx => ready.is_ok() && !*shutdown_rx.borrow(),
        };
        let _ = approval_tx.send(approved);
        if !approved && *shutdown_rx.borrow() {
            // The blocking task cannot cross the approval fence, so it owns no
            // claim and may finish off-runtime after the scoped worker exits.
            return Ok(None);
        }
        result_rx.await.map_err(|error| {
            QueueError::BlockingTaskFailed(format!("queue claim result owner failed: {error}"))
        })?
    }
}
