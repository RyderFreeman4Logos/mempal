//! Followable MCP ingest receipts for the gap before durable queue admission.
//!
//! The registry never starts canonical ingest itself. It exposes the queue's
//! deterministic operation identity while a self-held SQLite lock is retried,
//! then yields to the existing durable queue and worker.

use super::{
    INGEST_ASYNC_KIND, IngestOperationState, IngestResponse, MCP_INGEST_QUEUE_LOCK_RETRY_DELAY,
    MempalMcpServer, PendingMessageStore, SystemWarning, ingest_worker_backoff_delay,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PendingIngestAdmission {
    /// Acceptance time reported consistently until the durable queue row exists.
    pub(super) accepted_at: String,
    /// Terminal background-admission failure; canonical ingest never starts in
    /// this state, so reporting failure cannot mask a late drawer write.
    pub(super) failure_detail: Option<String>,
}

impl MempalMcpServer {
    pub(super) fn pending_ingest_admission_response(
        &self,
        operation_id: &str,
        system_warnings: Vec<SystemWarning>,
    ) -> Option<IngestResponse> {
        let pending = match self.pending_ingest_admissions.lock() {
            Ok(pending) => pending,
            Err(error) => {
                tracing::error!(
                    operation_id,
                    ?error,
                    "pending ingest admission registry lock was poisoned"
                );
                return None;
            }
        };
        let admission = pending.get(operation_id)?;
        let state = if admission.failure_detail.is_some() {
            IngestOperationState::Failed
        } else {
            IngestOperationState::Queued
        };
        Some(IngestResponse {
            operation_id: Some(operation_id.to_string()),
            accepted_at: Some(admission.accepted_at.clone()),
            state: Some(state),
            failure_detail: admission.failure_detail.clone(),
            system_warnings,
            ..Default::default()
        })
    }

    pub(super) fn spawn_self_held_queue_admission_recovery(
        &self,
        payload: String,
        idempotency_key: String,
    ) -> anyhow::Result<String> {
        let operation_id =
            PendingMessageStore::idempotent_message_id(INGEST_ASYNC_KIND, &idempotency_key);
        let mut pending = self.pending_ingest_admissions.lock().map_err(|error| {
            anyhow::anyhow!("pending ingest admission registry lock was poisoned: {error}")
        })?;
        pending
            .entry(operation_id.clone())
            .or_insert_with(|| PendingIngestAdmission {
                accepted_at: crate::core::utils::iso_timestamp(),
                failure_detail: None,
            });
        drop(pending);

        let expected_operation_id = operation_id.clone();
        let queue = self.async_queue.clone();
        let worker = self.clone();
        tokio::spawn(async move {
            let mut retry_count = 0_u64;
            loop {
                match queue
                    .enqueue_idempotent_with_key_fail_fast(
                        INGEST_ASYNC_KIND.to_string(),
                        payload.clone(),
                        idempotency_key.clone(),
                    )
                    .await
                {
                    Ok(admitted_operation_id) => {
                        if admitted_operation_id != expected_operation_id {
                            if let Ok(mut pending) = worker.pending_ingest_admissions.lock()
                                && let Some(admission) = pending.get_mut(&expected_operation_id)
                            {
                                admission.failure_detail = Some(format!(
                                    "queue admission returned operation {admitted_operation_id} \
                                     instead of {expected_operation_id}"
                                ));
                            }
                            tracing::error!(
                                operation_id = %expected_operation_id,
                                admitted_operation_id,
                                "self-held queue admission returned an unexpected operation identity"
                            );
                            return;
                        }
                        // Status checks this registry before SQLite. Removing the
                        // entry only after enqueue commits makes the transition
                        // gap-free for concurrent status requests.
                        if let Ok(mut pending) = worker.pending_ingest_admissions.lock() {
                            pending.remove(&expected_operation_id);
                        }
                        tracing::info!(
                            operation_id = %expected_operation_id,
                            "self-held queue admission recovered with the original operation identity"
                        );
                        worker.spawn_ingest_drain_worker();
                        return;
                    }
                    Err(error) if error.is_sqlite_lock() => {
                        retry_count = retry_count.saturating_add(1);
                        tokio::time::sleep(
                            ingest_worker_backoff_delay(retry_count)
                                .max(MCP_INGEST_QUEUE_LOCK_RETRY_DELAY),
                        )
                        .await;
                    }
                    Err(error) => {
                        if let Ok(mut pending) = worker.pending_ingest_admissions.lock()
                            && let Some(admission) = pending.get_mut(&expected_operation_id)
                        {
                            admission.failure_detail =
                                Some(format!("queue admission recovery failed: {error}"));
                        }
                        tracing::error!(
                            operation_id = %expected_operation_id,
                            ?error,
                            "self-held queue admission recovery stopped on a terminal error"
                        );
                        return;
                    }
                }
            }
        });
        Ok(operation_id)
    }
}
