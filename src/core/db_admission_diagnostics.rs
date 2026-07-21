//! Presentation helpers for admission liveness diagnostics.

use std::fmt;

use super::db_admission::UnknownHolderReason;

impl fmt::Display for UnknownHolderReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::UnknownLeaseVersion => "unknown_lease_version",
            Self::LeaseOpenUnavailable => "lease_open_unavailable",
            Self::LeaseLockUnavailable => "lease_lock_unavailable",
            Self::LegacyProcessIdentityUnverifiable => "legacy_process_identity_unverifiable",
        };
        formatter.write_str(value)
    }
}
