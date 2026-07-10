//! Redacted health diagnostics for replaced daemon executables.

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

use crate::process_diagnostics::{ProcessMemoryReport, inspect_process_memory};

/// Safe diagnostics for a live daemon whose executable was replaced.
///
/// The contract intentionally omits executable paths and process arguments.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub(crate) struct StaleDaemonDiagnostic {
    pub stale_daemon: bool,
    pub daemon_pid: i32,
    pub exe_deleted: bool,
    pub retry_safe_after_restart: bool,
}

impl StaleDaemonDiagnostic {
    fn from_process_report(report: &ProcessMemoryReport) -> Option<Self> {
        report.exe_deleted.then_some(Self {
            stale_daemon: true,
            daemon_pid: report.pid,
            exe_deleted: true,
            retry_safe_after_restart: true,
        })
    }
}

/// Inspect `pid` and return a diagnostic only for a deleted executable.
pub(crate) fn inspect(pid: i32) -> Option<StaleDaemonDiagnostic> {
    StaleDaemonDiagnostic::from_process_report(&inspect_process_memory(pid))
}

/// Inspect the calling daemon process without assuming `u32` fits in `i32`.
#[cfg(feature = "rest")]
pub(crate) fn inspect_current() -> Option<StaleDaemonDiagnostic> {
    i32::try_from(std::process::id()).ok().and_then(inspect)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_is_redacted_and_restart_retry_safe() {
        let report = ProcessMemoryReport {
            pid: 706_141,
            exe_path: Some("/usr/local/bin/mempal (deleted)".to_string()),
            exe_deleted: true,
            ..ProcessMemoryReport::default()
        };

        let diagnostic = StaleDaemonDiagnostic::from_process_report(&report)
            .expect("deleted executable should produce a stale-daemon diagnostic");

        assert!(diagnostic.stale_daemon);
        assert_eq!(diagnostic.daemon_pid, 706_141);
        assert!(diagnostic.exe_deleted);
        assert!(diagnostic.retry_safe_after_restart);
        assert!(
            StaleDaemonDiagnostic::from_process_report(&ProcessMemoryReport {
                pid: 42,
                exe_deleted: false,
                ..ProcessMemoryReport::default()
            })
            .is_none()
        );
    }
}
