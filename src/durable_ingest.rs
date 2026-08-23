//! Durable REST admission backed by the canonical pending-message queue.
//!
//! This module owns the transport-neutral contract shared by the REST adapter
//! and the existing ingest worker. Admission commits an opaque, validated REST
//! request to `PendingMessageStore`; it does not create another worker or
//! completion ledger.

use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::core::queue::{PendingMessageStore, PendingOperationRecord, QueueError};

pub(crate) const INGEST_ASYNC_KIND: &str = "ingest_async";
const CONTRACT_VERSION: &str = "mempal.rest.ingest.v1";
const DELETE_CONTRACT_VERSION: &str = "mempal.rest.delete.v1";
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;

/// Queue payload used by durable REST ingest admissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DurableIngestEnvelope {
    contract: String,
    pub(crate) request: Value,
}

impl DurableIngestEnvelope {
    pub(crate) fn new(request: Value) -> Self {
        Self {
            contract: CONTRACT_VERSION.to_string(),
            request,
        }
    }

    pub(crate) fn decode(payload: &str) -> Option<Self> {
        let envelope = serde_json::from_str::<Self>(payload).ok()?;
        (envelope.contract == CONTRACT_VERSION).then_some(envelope)
    }
}

/// Queue payload used by durable REST delete admissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DurableDeleteEnvelope {
    contract: String,
    pub(crate) drawer_id: String,
}

impl DurableDeleteEnvelope {
    fn new(drawer_id: String) -> Self {
        Self {
            contract: DELETE_CONTRACT_VERSION.to_string(),
            drawer_id,
        }
    }

    pub(crate) fn decode(payload: &str) -> Option<Self> {
        let envelope = serde_json::from_str::<Self>(payload).ok()?;
        (envelope.contract == DELETE_CONTRACT_VERSION).then_some(envelope)
    }
}

/// Public receipt returned after authoritative SQLite queue admission.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DurableOperationReceipt {
    pub operation_id: String,
    pub accepted_at: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drawer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_detail: Option<String>,
}

#[derive(Debug, Error)]
pub enum DurableAdmissionError {
    #[error("idempotency_key must be 1..={MAX_IDEMPOTENCY_KEY_BYTES} printable ASCII bytes")]
    InvalidIdempotencyKey,
    #[error("failed to encode durable ingest envelope")]
    Encode(#[source] serde_json::Error),
    #[error("durable operation key conflicts with existing work")]
    OperationKeyConflict,
    #[error("durable ingest queue operation failed")]
    Queue(#[from] QueueError),
    #[error("durable operation not found")]
    OperationNotFound,
}

/// Commit a durable ingest request using the producer-owned stable key.
pub fn admit(
    db_path: &Path,
    idempotency_key: &str,
    request: Value,
) -> Result<DurableOperationReceipt, DurableAdmissionError> {
    validate_idempotency_key(idempotency_key)?;
    let payload = serde_json::to_string(&DurableIngestEnvelope::new(request))
        .map_err(DurableAdmissionError::Encode)?;
    let store = PendingMessageStore::new_without_reclaim(db_path);
    let operation_id = store
        .enqueue_idempotent_with_key(INGEST_ASYNC_KIND, &payload, idempotency_key)
        .map_err(map_queue_error)?;
    status_with_store(&store, &operation_id)
}

/// Commit a durable delete request to the canonical ingest worker queue.
pub fn admit_delete(
    db_path: &Path,
    idempotency_key: &str,
    drawer_id: String,
) -> Result<DurableOperationReceipt, DurableAdmissionError> {
    validate_idempotency_key(idempotency_key)?;
    let payload = serde_json::to_string(&DurableDeleteEnvelope::new(drawer_id))
        .map_err(DurableAdmissionError::Encode)?;
    let store = PendingMessageStore::new_without_reclaim(db_path);
    let operation_id = store
        .enqueue_idempotent_with_key(INGEST_ASYNC_KIND, &payload, idempotency_key)
        .map_err(map_queue_error)?;
    status_with_store(&store, &operation_id)
}

/// Read the authoritative queue/completion-ledger state for an operation.
pub fn status(
    db_path: &Path,
    operation_id: &str,
) -> Result<DurableOperationReceipt, DurableAdmissionError> {
    let store = PendingMessageStore::new_without_reclaim(db_path);
    status_with_store(&store, operation_id)
}

fn status_with_store(
    store: &PendingMessageStore,
    operation_id: &str,
) -> Result<DurableOperationReceipt, DurableAdmissionError> {
    let record = store
        .operation_status(operation_id)?
        .ok_or(DurableAdmissionError::OperationNotFound)?;
    Ok(receipt_from_record(record))
}

fn validate_idempotency_key(key: &str) -> Result<(), DurableAdmissionError> {
    let valid = !key.is_empty()
        && key.len() <= MAX_IDEMPOTENCY_KEY_BYTES
        && key.bytes().all(|byte| byte.is_ascii_graphic());
    if valid {
        Ok(())
    } else {
        Err(DurableAdmissionError::InvalidIdempotencyKey)
    }
}

fn map_queue_error(error: QueueError) -> DurableAdmissionError {
    match error {
        QueueError::IdempotencyConflict => DurableAdmissionError::OperationKeyConflict,
        other => DurableAdmissionError::Queue(other),
    }
}

fn receipt_from_record(record: PendingOperationRecord) -> DurableOperationReceipt {
    let accepted_at_secs = if record.completed_at.is_some() {
        record.created_at.div_euclid(1_000)
    } else {
        record.created_at
    };
    DurableOperationReceipt {
        operation_id: record.id,
        accepted_at: crate::cowork::peek::format_rfc3339(
            UNIX_EPOCH + Duration::from_secs(accepted_at_secs.max(0) as u64),
        ),
        state: record.op_state,
        drawer_id: record.result_drawer_id,
        rejected_reason: record.rejected_reason,
        failure_detail: record.failure_detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_key_conflict_is_rejected_before_aliasing_pending_work() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        crate::core::db::Database::open(&db_path).expect("initialize database");

        let first = admit(
            &db_path,
            "shared-operation-key",
            serde_json::json!({
                "content": "private first payload",
                "wing": "project-a",
                "room": "facts",
            }),
        )
        .expect("first admission");
        let identical = admit(
            &db_path,
            "shared-operation-key",
            serde_json::json!({
                "content": "private first payload",
                "wing": "project-a",
                "room": "facts",
            }),
        )
        .expect("identical pending retry");
        let conflict = admit(
            &db_path,
            "shared-operation-key",
            serde_json::json!({
                "content": "private conflicting payload",
                "wing": "project-b",
                "room": "facts",
            }),
        );

        assert_eq!(identical.operation_id, first.operation_id);
        assert!(matches!(
            &conflict,
            Err(DurableAdmissionError::OperationKeyConflict)
        ));
        let rendered = conflict
            .err()
            .expect("conflict receipt must be terminal")
            .to_string();
        assert!(!rendered.contains("private first payload"));
        assert!(!rendered.contains("private conflicting payload"));
        assert_eq!(first.state, "queued");
    }

    #[test]
    fn completed_explicit_key_retries_require_durable_identity_after_reopen() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        crate::core::db::Database::open(&db_path).expect("initialize database");
        let request = serde_json::json!({
            "content": "private completed payload",
            "wing": "project-a",
            "room": "facts",
        });
        let first =
            admit(&db_path, "completed-operation-key", request.clone()).expect("first admission");
        let store = PendingMessageStore::new_without_reclaim(&db_path);
        let claim = store
            .claim_next_by_kind("test-worker", 60, INGEST_ASYNC_KIND)
            .expect("claim completed operation")
            .expect("pending operation");
        store.confirm(&claim).expect("complete operation");
        drop(store);

        let identical = admit(&db_path, "completed-operation-key", request)
            .expect("identical completed retry after reopen");
        assert_eq!(identical.operation_id, first.operation_id);
        assert_eq!(identical.state, "completed");
        assert!(matches!(
            admit(
                &db_path,
                "completed-operation-key",
                serde_json::json!({
                    "content": "private completed conflict",
                    "wing": "project-b",
                    "room": "facts",
                }),
            ),
            Err(DurableAdmissionError::OperationKeyConflict)
        ));

        let connection = rusqlite::Connection::open(&db_path).expect("open legacy fixture");
        connection
            .execute(
                "UPDATE pending_message_completions SET source_hash = NULL WHERE message_id = ?1",
                [&first.operation_id],
            )
            .expect("remove legacy fingerprint");
        drop(connection);
        let legacy = admit(
            &db_path,
            "completed-operation-key",
            serde_json::json!({"content": "fixture-private-legacy-marker"}),
        );
        assert!(matches!(
            &legacy,
            Err(DurableAdmissionError::OperationKeyConflict)
        ));
        assert!(
            !legacy
                .expect_err("legacy identity must fail closed")
                .to_string()
                .contains("fixture-private-legacy-marker")
        );
    }

    #[test]
    fn idempotency_key_validation_is_bounded_and_content_free() {
        assert!(validate_idempotency_key("provider-event_01").is_ok());
        assert!(validate_idempotency_key("").is_err());
        assert!(validate_idempotency_key("contains whitespace").is_err());
        assert!(validate_idempotency_key(&"x".repeat(129)).is_err());
    }
}
