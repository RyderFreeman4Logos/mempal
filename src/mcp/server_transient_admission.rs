use anyhow::Error as AnyhowError;

use crate::core::{db::DbError, db_admission::DbAdmissionError};

pub(super) fn anyhow_chain_has_transient_admission(error: &AnyhowError) -> bool {
    error.chain().any(|cause| {
        let admission = cause.downcast_ref::<DbAdmissionError>().or_else(|| {
            match cause.downcast_ref::<DbError>() {
                Some(DbError::Admission(error)) => Some(error),
                _ => None,
            }
        });
        matches!(
            admission,
            Some(DbAdmissionError::Busy { .. } | DbAdmissionError::BudgetExceeded { .. })
        )
    })
}

pub(super) fn db_admission_failure_kind(error: &DbAdmissionError) -> &'static str {
    match error {
        DbAdmissionError::Busy { .. } => "locked_or_busy",
        DbAdmissionError::BudgetExceeded { .. } => "holder_budget_exceeded",
        DbAdmissionError::Io { source, .. }
            if matches!(
                source.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            ) =>
        {
            "path_or_permission"
        }
        DbAdmissionError::Io { .. } => "unknown",
        DbAdmissionError::UnsafeSidecarDirectory { .. }
        | DbAdmissionError::UnsafeSidecar { .. } => "unsafe_sidecar",
        DbAdmissionError::UnsupportedStateVersion { .. } => "unsupported_schema",
        DbAdmissionError::StateTooLarge { .. } | DbAdmissionError::InvalidState { .. } => {
            "corrupt_or_invalid"
        }
        DbAdmissionError::InvalidRequest(_) => "invalid_request",
    }
}

pub(super) fn scoped_ingest_worker_error(error: AnyhowError) -> rmcp::ErrorData {
    let failure_kind = super::status_db_failure_kind(error.as_ref());
    rmcp::ErrorData::internal_error(
        format!("scoped async ingest worker failed ({failure_kind})"),
        Some(serde_json::json!({ "failure_kind": failure_kind })),
    )
}
