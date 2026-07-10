//! Stale daemon write-routing guard.

use std::path::Path;

use rmcp::model::ErrorData;

use crate::stale_daemon::StaleDaemonDiagnostic;

/// Reject writes before routing when a live daemon executable was replaced.
pub(super) fn guard_write(db_path: &Path) -> std::result::Result<(), ErrorData> {
    #[cfg(target_os = "linux")]
    {
        let binary =
            crate::daemon_singleton::current_binary_name().unwrap_or_else(|| "mempal".to_string());
        let daemon_pids = crate::daemon_singleton::enumerate_daemon_pids(&binary, db_path);
        ensure_write_runtime_current_with(daemon_pids, crate::stale_daemon::inspect)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = db_path;
        Ok(())
    }
}

pub(super) fn should_use_ingest_worker(socket_exists: bool, daemon_pids: &[i32]) -> bool {
    socket_exists && !daemon_pids.is_empty()
}

fn ensure_write_runtime_current_with(
    daemon_pids: impl IntoIterator<Item = i32>,
    inspect: impl Fn(i32) -> Option<StaleDaemonDiagnostic>,
) -> std::result::Result<(), ErrorData> {
    match daemon_pids.into_iter().find_map(inspect) {
        Some(diagnostic) => Err(stale_daemon_write_error(diagnostic)),
        None => Ok(()),
    }
}

fn stale_daemon_write_error(diagnostic: StaleDaemonDiagnostic) -> ErrorData {
    ErrorData::internal_error(
        format!(
            "mempal daemon pid {} is running a deleted or replaced executable; run `mempal daemon restart` before retrying the write",
            diagnostic.daemon_pid
        ),
        Some(serde_json::json!({
            "reason": "stale_daemon",
            "boundary": "daemon_executable",
            "action": "restart_daemon_then_retry",
            "stale_daemon": diagnostic.stale_daemon,
            "daemon_pid": diagnostic.daemon_pid,
            "exe_deleted": diagnostic.exe_deleted,
            "retryable": false,
            "retry_safe_after_restart": diagnostic.retry_safe_after_restart,
            "recovery_hint": "Run `mempal daemon restart`, then retry the write once.",
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_stale_daemon_with_structured_redacted_diagnostics() {
        let result = ensure_write_runtime_current_with([42, 706_141], |pid| {
            (pid == 706_141).then_some(StaleDaemonDiagnostic {
                stale_daemon: true,
                daemon_pid: pid,
                exe_deleted: true,
                retry_safe_after_restart: true,
            })
        });
        let error = result.expect_err("stale daemon must reject writes before routing");

        assert!(error.message.contains("mempal daemon restart"));
        let data = error.data.expect("structured stale-daemon error data");
        assert_eq!(data["reason"], "stale_daemon");
        assert_eq!(data["boundary"], "daemon_executable");
        assert_eq!(data["action"], "restart_daemon_then_retry");
        assert_eq!(data["stale_daemon"], true);
        assert_eq!(data["daemon_pid"], 706_141);
        assert_eq!(data["exe_deleted"], true);
        assert_eq!(data["retryable"], false);
        assert_eq!(data["retry_safe_after_restart"], true);
        assert!(data.get("exe_path").is_none());
    }

    #[test]
    fn permits_write_routing_when_daemon_executables_are_current() {
        let result = ensure_write_runtime_current_with([42], |_| None);

        assert!(result.is_ok());
    }

    #[test]
    fn daemon_worker_requires_socket_and_live_pid() {
        assert!(should_use_ingest_worker(true, &[1234]));
        assert!(!should_use_ingest_worker(false, &[1234]));
        assert!(!should_use_ingest_worker(true, &[]));
    }
}
