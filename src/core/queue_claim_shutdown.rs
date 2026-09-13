use super::{AsyncPendingMessageStore, ClaimedMessage, PendingMessageStore, QueueError, Result};
#[cfg(any(test, feature = "db-test-seam"))]
use super::{consume_lock_failure, sqlite_busy_queue_error};

#[cfg(any(test, feature = "db-test-seam"))]
pub(super) type ClaimApprovalTestControl = (
    std::sync::Arc<tokio::sync::Notify>,
    Option<(
        std::sync::Arc<std::sync::Barrier>,
        std::sync::mpsc::Sender<()>,
        std::sync::Arc<std::sync::Barrier>,
    )>,
);

struct ApprovedClaimOwner {
    store: PendingMessageStore,
    claim: Option<ClaimedMessage>,
}

impl Drop for ApprovedClaimOwner {
    fn drop(&mut self) {
        if let Some(claim) = self.claim.take()
            && let Err(error) = self.store.release_owned_claim_after_cancellation(&claim)
        {
            tracing::warn!(
                ?error,
                "failed to release approved queue claim after caller cancellation"
            );
        }
    }
}

impl PendingMessageStore {
    /// Release only this physical owner's exact token after runtime teardown.
    fn release_owned_claim_after_cancellation(&self, claim: &ClaimedMessage) -> Result<()> {
        let mut cleanup = self.clone();
        cleanup.lifecycle_writer_lease = None;
        cleanup.release_claim(claim)
    }
}

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
        let store = self.inner.clone();
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (approval_tx, approval_rx) = tokio::sync::oneshot::channel();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
        tokio::task::spawn_blocking(move || {
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
                let result = store.claim_next_by_kind_with_approval(
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
                            if let Some((gate, _, _)) = gate {
                                gate.wait();
                            }
                        }
                        approved
                    },
                );
                #[cfg(any(test, feature = "db-test-seam"))]
                if matches!(&result, Ok(Some(_)))
                    && let Some((_, Some((_, committed, cleanup_gate)))) = &claim_approved
                {
                    let _ = committed.send(());
                    cleanup_gate.wait();
                }
                match result {
                    Ok(Some(claim)) => {
                        let transferred_claim = claim.clone();
                        let mut owner = ApprovedClaimOwner {
                            store,
                            claim: Some(claim),
                        };
                        if result_tx.send(Ok(Some(transferred_claim))).is_ok()
                            && accepted_rx.recv().is_ok()
                        {
                            owner.claim = None;
                        }
                    }
                    result => {
                        let _ = result_tx.send(result);
                    }
                }
            })
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
        let result = result_rx.await.map_err(|error| {
            QueueError::BlockingTaskFailed(format!("queue claim result owner failed: {error}"))
        })?;
        if matches!(&result, Ok(Some(_))) {
            let _ = accepted_tx.send(());
        }
        result
    }
}
