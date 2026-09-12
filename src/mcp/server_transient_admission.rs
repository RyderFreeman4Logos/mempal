use anyhow::Error as AnyhowError;

pub(super) fn anyhow_chain_has_transient_admission(error: &AnyhowError) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<crate::core::db::DbError>(),
            Some(crate::core::db::DbError::Admission(
                crate::core::db_admission::DbAdmissionError::Busy { .. }
                    | crate::core::db_admission::DbAdmissionError::BudgetExceeded { .. }
            ))
        ) || matches!(
            cause.downcast_ref::<crate::core::db_admission::DbAdmissionError>(),
            Some(
                crate::core::db_admission::DbAdmissionError::Busy { .. }
                    | crate::core::db_admission::DbAdmissionError::BudgetExceeded { .. }
            )
        )
    })
}
