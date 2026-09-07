use std::time::Duration;

use rmcp::ErrorData;
use tokio::time::Instant;

use crate::core::queue::PendingOperationRecord;

use super::{IngestOperationState, IngestResponse, MempalMcpServer, SystemWarning};

pub(super) enum OperationLookup {
    Record(Box<PendingOperationRecord>),
    SpoolPending,
    Missing,
}

impl MempalMcpServer {
    pub(super) async fn operation_lookup_within(
        &self,
        operation_id: &str,
        timeout: Duration,
    ) -> Result<Option<OperationLookup>, ErrorData> {
        let started_at = Instant::now();
        let lookup = |remaining| {
            tokio::time::timeout(
                remaining,
                self.async_queue.operation_status(operation_id.to_string()),
            )
        };
        match lookup(timeout).await {
            Ok(Ok(Some(record))) => return Ok(Some(OperationLookup::Record(Box::new(record)))),
            Ok(Ok(None)) => {}
            Ok(Err(error)) => return Err(queue_lookup_error(error)),
            Err(_) => return Ok(None),
        }

        let remaining = timeout.saturating_sub(started_at.elapsed());
        if remaining.is_zero() {
            return Ok(None);
        }
        let mempal_home = self.db_path.parent().map(std::path::Path::to_path_buf);
        let requested_id = operation_id.to_string();
        let pending = match tokio::time::timeout(
            remaining,
            tokio::task::spawn_blocking(move || {
                let Some(mempal_home) = mempal_home else {
                    return Ok(false);
                };
                crate::ingress_spool::IngressSpool::new(mempal_home)
                    .contains_operation_id(&requested_id)
            }),
        )
        .await
        {
            Ok(Ok(Ok(pending))) => pending,
            Ok(Ok(Err(error))) => {
                return Err(ErrorData::internal_error(
                    format!("ingress spool lookup failed: {error}"),
                    None,
                ));
            }
            Ok(Err(error)) => {
                return Err(ErrorData::internal_error(
                    format!("ingress spool lookup task failed: {error}"),
                    None,
                ));
            }
            Err(_) => return Ok(None),
        };

        let remaining = timeout.saturating_sub(started_at.elapsed());
        if remaining.is_zero() {
            return Ok(None);
        }
        match lookup(remaining).await {
            Ok(Ok(Some(record))) => Ok(Some(OperationLookup::Record(Box::new(record)))),
            Ok(Ok(None)) if pending => Ok(Some(OperationLookup::SpoolPending)),
            Ok(Ok(None)) => Ok(Some(OperationLookup::Missing)),
            Ok(Err(error)) => Err(queue_lookup_error(error)),
            Err(_) => Ok(None),
        }
    }
}

pub(super) fn spool_pending_operation_response(
    operation_id: &str,
    system_warnings: Vec<SystemWarning>,
) -> IngestResponse {
    IngestResponse {
        operation_id: Some(operation_id.to_string()),
        state: Some(IngestOperationState::Queued),
        system_warnings,
        ..IngestResponse::default()
    }
}

fn queue_lookup_error(error: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(format!("queue lookup failed: {error}"), None)
}
