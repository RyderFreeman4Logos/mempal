//! Profile holder budget checks and service-seat reservation (#809).

use std::fmt;

use serde::{Deserialize, Serialize};

use super::db_admission::{DbAdmissionConfig, DbAdmissionError, DbAdmissionRequest};

/// Why profile admission refused a holder after stale reaping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetExceededReason {
    HolderLimit,
    CacheBudget,
    ReservedServiceSlots,
}

impl fmt::Display for BudgetExceededReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::HolderLimit => "holder_limit",
            Self::CacheBudget => "cache_budget",
            Self::ReservedServiceSlots => "reserved_service_slots",
        };
        formatter.write_str(value)
    }
}

pub(super) fn validate_request(
    request: DbAdmissionRequest,
    config: DbAdmissionConfig,
) -> Result<(), DbAdmissionError> {
    if request.connection_count == 0 {
        return Err(DbAdmissionError::InvalidRequest(
            "connection_count must be positive",
        ));
    }
    if request.configured_cache_bytes == 0 {
        return Err(DbAdmissionError::InvalidRequest(
            "configured_cache_bytes must be positive",
        ));
    }
    if config.max_holders == 0 || config.max_cache_bytes == 0 {
        return Err(DbAdmissionError::InvalidRequest(
            "profile limits must be positive",
        ));
    }
    if config.reserved_service_holders > config.max_holders {
        return Err(DbAdmissionError::InvalidRequest(
            "reserved_service_holders must not exceed max_holders",
        ));
    }
    Ok(())
}

pub(super) fn budget_exceeded_reason(
    active_holders: usize,
    request: DbAdmissionRequest,
    config: DbAdmissionConfig,
    active_cache_bytes: u64,
) -> Option<BudgetExceededReason> {
    if active_holders >= config.max_holders {
        return Some(BudgetExceededReason::HolderLimit);
    }
    if active_cache_bytes.saturating_add(request.configured_cache_bytes) > config.max_cache_bytes {
        return Some(BudgetExceededReason::CacheBudget);
    }
    if !request.holder_class.is_service_holder()
        && config.reserved_service_holders > 0
        && active_holders
            .saturating_add(1)
            .saturating_add(config.reserved_service_holders)
            > config.max_holders
    {
        // Keep the last reserved seats free for daemon/MCP service holders.
        return Some(BudgetExceededReason::ReservedServiceSlots);
    }
    None
}
