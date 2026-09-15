use std::time::{Duration, Instant};

use anyhow::Context;

use super::{
    AsyncPendingMessageStore, ClaimedMessage, MempalMcpServer, ScopedIngestProcessResult,
    anyhow_chain_contains_sqlite_lock, anyhow_chain_has_transient_admission,
    complete_failed_ingest_claim, queue_wait_ms, status_db_failure_kind,
};

impl MempalMcpServer {
    pub(super) async fn process_ingest_claim_with_owned_task_budget(
        &self,
        queue: &AsyncPendingMessageStore,
        worker_id: &str,
        claim: ClaimedMessage,
        budget: Duration,
    ) -> anyhow::Result<ScopedIngestProcessResult> {
        let deadline = Instant::now()
            .checked_add(budget)
            .unwrap_or_else(Instant::now);
        let mut scoped_worker = self.clone();
        let scoped_queue = queue.clone();
        let scoped_worker_id = worker_id.to_string();
        let processing = tokio::spawn(async move {
            let queue_wait_ms = queue_wait_ms(claim.created_at, claim.claimed_at);
            let (stop_tx, heartbeat) = Self::spawn_ingest_claim_heartbeat(
                scoped_queue.clone(),
                claim.id.clone(),
                &scoped_worker_id,
                scoped_worker.daemon_write_observer.clone(),
            );
            let writer_lease = if scoped_worker.external_ingest_writer_lease.is_some() {
                None
            } else {
                match scoped_worker
                    .acquire_ingest_writer_lease(&scoped_worker_id)
                    .await
                {
                    Ok(Some(lease)) => Some(lease),
                    Ok(None) => {
                        Self::stop_ingest_claim_heartbeat(stop_tx, heartbeat).await;
                        Self::release_claim_with_lock_retry(
                            &scoped_queue,
                            claim,
                            scoped_worker.daemon_write_observer.as_ref(),
                        )
                        .await
                        .context(
                            "failed to release scoped ingest claim after writer lease conflict",
                        )?;
                        return Ok(ScopedIngestProcessResult::ReleasedForRetry);
                    }
                    Err(error)
                        if anyhow_chain_contains_sqlite_lock(&error)
                            || anyhow_chain_has_transient_admission(&error) =>
                    {
                        Self::stop_ingest_claim_heartbeat(stop_tx, heartbeat).await;
                        Self::release_claim_with_lock_retry(
                            &scoped_queue,
                            claim,
                            scoped_worker.daemon_write_observer.as_ref(),
                        )
                        .await
                        .context(
                            "failed to release scoped ingest claim after transient writer lease lock",
                        )?;
                        return Ok(ScopedIngestProcessResult::ReleasedForRetry);
                    }
                    Err(error) => {
                        let before_deadline = Instant::now() < deadline;
                        Self::stop_ingest_claim_heartbeat(stop_tx, heartbeat).await;
                        if before_deadline {
                            Self::release_claim_with_lock_retry(
                                &scoped_queue,
                                claim,
                                scoped_worker.daemon_write_observer.as_ref(),
                            )
                            .await
                            .context(
                                "failed to release scoped ingest claim after writer lease error",
                            )?;
                            return Err(error)
                                .context("failed to acquire scoped MCP ingest writer lease");
                        }
                        let failure_kind = status_db_failure_kind(error.as_ref());
                        complete_failed_ingest_claim(
                            &scoped_queue,
                            &claim,
                            queue_wait_ms,
                            format!(
                                "failed to acquire scoped MCP ingest writer lease ({failure_kind})"
                            ),
                            scoped_worker.daemon_write_observer.as_ref(),
                        )
                        .await
                        .context("failed to persist scoped ingest writer lease failure")?;
                        return Err(error)
                            .context("failed to acquire scoped MCP ingest writer lease");
                    }
                }
            };
            Self::stop_ingest_claim_heartbeat(stop_tx, heartbeat).await;

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let released = match writer_lease {
                    Some(writer_lease) => writer_lease.release().await,
                    None => Ok(()),
                };
                let claim_released = Self::release_claim_with_lock_retry(
                    &scoped_queue,
                    claim,
                    scoped_worker.daemon_write_observer.as_ref(),
                )
                .await
                .context("failed to release scoped ingest claim after request deadline");
                claim_released?;
                released?;
                return Ok(ScopedIngestProcessResult::TimedOut);
            }

            if let Some(ref lease) = writer_lease {
                scoped_worker.external_ingest_writer_lease = Some(lease.lease().clone());
            }
            let result = scoped_worker
                .process_ingest_claim_inline(&scoped_queue, &scoped_worker_id, claim)
                .await;
            let released = match writer_lease {
                Some(writer_lease) => writer_lease.release().await,
                None => Ok(()),
            };
            result?;
            released?;
            Ok(ScopedIngestProcessResult::Processed)
        });

        match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), processing).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                Err(anyhow::Error::new(error).context("scoped ingest claim task failed"))
            }
            // Dropping a JoinHandle detaches the task. That task owns the claim,
            // heartbeat, and writer lease until it records the terminal receipt.
            Err(_) => Ok(ScopedIngestProcessResult::TimedOut),
        }
    }
}
